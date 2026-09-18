//! SSH 配置导入(第二十五批):把 `~/.ssh/config` 与 `known_hosts` 里的主机
//! 读成候选清单,供 03 页「从 SSH 配置导入」勾选后批量建服务器。
//!
//! **只读扫描,零新写路径**:本模块唯一的命令返回候选清单,前端勾选后逐条走
//! 既有 `save_server_entry`(密文 merge / 哨兵 / `update_config` 锁语义完整
//! 保留)。不读 `~/.ssh/known_hosts` 的密钥做指纹预填 —— 见
//! [`SshHostCandidate::known_in_hosts`] 的说明。

use std::collections::HashSet;
use std::path::PathBuf;

use serde::Serialize;

/// 一个可导入的 SSH 主机候选(前端契约,camelCase)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshHostCandidate {
    /// `Host` 行首个非通配 token(OpenSSH 里它就是用户敲的别名)
    pub alias: String,
    /// 实际地址:`HostName`(缺省按 OpenSSH 语义回退到 `alias`)
    pub host: String,
    /// 端口(`Port` 缺省/不可解析 → 22)
    pub port: u16,
    /// `User`(该配置块未写 → None,由用户在表单里补)
    pub user: Option<String>,
    /// `IdentityFile` 展开 `~` / `%d` 后的路径(未写 → None)
    pub identity_file: Option<String>,
    /// 该主机是否已在 `known_hosts` 出现 —— **仅作提示,不预填指纹**。
    ///
    /// 不预填的原因:一个主机在 known_hosts 里常有多条不同算法的密钥
    /// (ed25519/rsa/ecdsa),而服务端实际协商出哪一条由握手决定;预填错一条
    /// 会让连接硬失败(报「主机密钥已变更」)。TOFU 首次连接自然记录才准确。
    pub known_in_hosts: bool,
}

/// `ssh_config_scan` 的返回(前端契约,camelCase)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConfigScan {
    pub hosts: Vec<SshHostCandidate>,
    /// 跳过的通配配置块数(`Host *` / `Host *.example.com` 等全局默认,
    /// 不是可导入的具体主机;回显给用户避免「我配置里有 10 个怎么只出现 6 个」)
    pub skipped_wildcards: usize,
    /// 实际读取的 config 路径(回显,便于用户确认读的是哪个文件)
    pub config_path: String,
    pub config_exists: bool,
    pub known_hosts_exists: bool,
}

/// 解析出的单个配置块(内部形态;identity 尚未展开 `~`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedHost {
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    /// 原始 `IdentityFile` 值(展开在 [`expand_home_tokens`])
    pub identity_raw: Option<String>,
}

impl ParsedHost {
    /// 转前端候选:展开路径记号 + 标注 known_hosts 命中。
    /// 命中判定同时看 `host` 与 `alias`(OpenSSH 记 known_hosts 用的名字
    /// 视连接方式而定,两者都算命中更符合用户预期)。
    pub(crate) fn into_candidate(
        self,
        home: Option<&str>,
        known: &HashSet<String>,
    ) -> SshHostCandidate {
        let known_in_hosts = known.contains(&self.host.to_ascii_lowercase())
            || known.contains(&self.alias.to_ascii_lowercase());
        SshHostCandidate {
            known_in_hosts,
            identity_file: self
                .identity_raw
                .as_deref()
                .map(|v| expand_home_tokens(v, home)),
            alias: self.alias,
            host: self.host,
            port: self.port,
            user: self.user,
        }
    }
}

/// 解析 `ssh_config` 文本 → (候选主机, 跳过通配块数)。纯函数。
///
/// 依据 OpenSSH 语义:
/// - 关键字**大小写不敏感**;`Key Value` 与 `Key=Value` 两种写法都合法
/// - **首个值生效**:同一块内重复关键字,后来的忽略
/// - `Host` 开启新块且可带多个模式;含 `*`/`?` 的模式是通配(全局默认),
///   整块不计入候选,只累加 `skipped_wildcards`
/// - `#` 起注释;空行忽略;未知关键字忽略(含 `Include` —— 不做递归展开)
/// - `Port` 不可解析或越界(1–65535)→ 回退 22
/// - `HostName` 缺省 → 回退 `alias`
pub(crate) fn parse_ssh_config(text: &str) -> (Vec<ParsedHost>, usize) {
    /// 解析中的块状态:alias 决定它是否可导入(wildcard 块不入候选)
    struct Block {
        alias: String,
        wildcard: bool,
        host: Option<String>,
        port: Option<u16>,
        user: Option<String>,
        identity: Option<String>,
    }

    /// 一行拆成 (关键字小写, 值)。兼容 `Key Value` / `Key=Value`;
    /// 空行/注释/纯键行返回 None。
    fn split_kv(line: &str) -> Option<(String, String)> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (key, value) = match line.find(|c: char| c.is_whitespace() || c == '=') {
            Some(i) => {
                let rest = &line[i..];
                // `=` 本身也当分隔符吃掉(可能两侧带空白)
                let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
                (&line[..i], rest)
            }
            None => (line, ""),
        };
        Some((key.to_ascii_lowercase(), value.trim().to_string()))
    }

    let mut out: Vec<ParsedHost> = Vec::new();
    let mut skipped = 0usize;
    let mut cur: Option<Block> = None;

    /// 收尾一个块:非通配的落进结果
    fn flush(cur: Option<Block>, out: &mut Vec<ParsedHost>, skipped: &mut usize) {
        let Some(b) = cur else { return };
        if b.wildcard {
            *skipped += 1;
            return;
        }
        out.push(ParsedHost {
            host: b.host.unwrap_or_else(|| b.alias.clone()),
            alias: b.alias,
            port: b.port.unwrap_or(22),
            user: b.user,
            identity_raw: b.identity,
        });
    }

    for raw in text.lines() {
        let Some((key, value)) = split_kv(raw) else { continue };
        match key.as_str() {
            "host" => {
                flush(cur.take(), &mut out, &mut skipped);
                // 模式列表可含多个 token;取首个非通配 token 作别名
                let patterns: Vec<&str> = value.split_whitespace().collect();
                if patterns.is_empty() {
                    continue; // 空 Host 行:不开块
                }
                let wildcard = patterns.iter().any(|p| p.contains('*') || p.contains('?'));
                let alias = patterns
                    .iter()
                    .find(|p| !p.contains('*') && !p.contains('?'))
                    .map(|p| p.to_string())
                    // 全是通配模式时仍要有个名字用于计数(不入候选)
                    .unwrap_or_else(|| patterns[0].to_string());
                cur = Some(Block {
                    alias,
                    wildcard,
                    host: None,
                    port: None,
                    user: None,
                    identity: None,
                });
            }
            _ => {
                let Some(b) = cur.as_mut() else { continue }; // Host 之前的关键字:全局段,忽略
                match key.as_str() {
                    // 首个值生效:已有值就忽略后来的
                    "hostname" if b.host.is_none() && !value.is_empty() => {
                        b.host = Some(value);
                    }
                    "port" if b.port.is_none() => {
                        // 解析失败或越界 → 留 None,flush 时回退 22
                        if let Ok(p) = value.parse::<u32>() {
                            if (1..=65535).contains(&p) {
                                b.port = Some(p as u16);
                            }
                        }
                    }
                    "user" if b.user.is_none() && !value.is_empty() => {
                        b.user = Some(value);
                    }
                    "identityfile" if b.identity.is_none() && !value.is_empty() => {
                        b.identity = Some(value);
                    }
                    _ => {} // 未知关键字/重复关键字:忽略
                }
            }
        }
    }
    flush(cur.take(), &mut out, &mut skipped);
    (out, skipped)
}

/// 展开 OpenSSH 路径记号:`~` 与 `%d` = 用户主目录。
/// `home` 为 None 时**原样返回**(不猜路径);未命中记号也原样返回。
pub(crate) fn expand_home_tokens(value: &str, home: Option<&str>) -> String {
    let v = value.trim();
    let Some(home) = home else {
        return v.to_string();
    };
    let home = home.trim_end_matches(['/', '\\']);
    if v == "~" || v == "%d" {
        return home.to_string();
    }
    if let Some(rest) = v.strip_prefix("~/") {
        return format!("{}/{}", home, rest);
    }
    if let Some(rest) = v.strip_prefix("%d/") {
        return format!("{}/{}", home, rest);
    }
    v.to_string()
}

/// 解析 `known_hosts` 文本 → 出现过的**明文**主机名集合(全部小写)。纯函数。
/// - 一行格式:`host1,host2 keytype base64 [comment]`
/// - `[host]:port` 形态取出 `host`
/// - 哈希主机名(`|1|...`,OpenSSH HashKnownHosts)不可反查 → 跳过
/// - 注释行与空行跳过
pub(crate) fn parse_known_hosts_hosts(text: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(hosts_field) = line.split_whitespace().next() else {
            continue;
        };
        for host in hosts_field.split(',') {
            let host = host.trim();
            if host.is_empty() {
                continue;
            }
            // 哈希主机名(以 | 开头)无法反查成明文,跳过
            if host.starts_with('|') {
                continue;
            }
            // `[host]:port` → 取 host 本体;IPv6 裸写(含多个冒号)不剥
            let plain = if let Some(rest) = host.strip_prefix('[') {
                match rest.find(']') {
                    Some(i) => &rest[..i],
                    None => rest,
                }
            } else {
                host
            };
            if !plain.is_empty() {
                set.insert(plain.to_ascii_lowercase());
            }
        }
    }
    set
}

/// 用户主目录:Windows 取 `USERPROFILE`,类 Unix 取 `HOME`;
/// 两者皆空 → None(不猜路径,由调用方报错)。
pub(crate) fn home_dir() -> Option<PathBuf> {
    for key in ["USERPROFILE", "HOME"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    None
}

/// 扫描 `~/.ssh`(只读):返回可导入主机候选。不写任何配置。
///
/// 失败语义:主目录缺失 → Err(config 缺失不算错误,返回 `config_exists:false`
/// 让前端给「没有配置文件」的空态);config 存在但读不了 → Err 原文。
#[tauri::command]
pub fn ssh_config_scan() -> Result<SshConfigScan, String> {
    let Some(home) = home_dir() else {
        return Err("未找到用户主目录(USERPROFILE / HOME 均为空),无法定位 ~/.ssh".to_string());
    };
    let ssh_dir = home.join(".ssh");
    let config_path = ssh_dir.join("config");
    let known_path = ssh_dir.join("known_hosts");

    let config_exists = config_path.is_file();
    let known_hosts_exists = known_path.is_file();

    let config_text = if config_exists {
        std::fs::read_to_string(&config_path)
            .map_err(|e| format!("读取 {} 失败: {}", config_path.display(), e))?
    } else {
        String::new()
    };
    // known_hosts 读失败不算致命(它只提供提示标记):按空集继续
    let known_text = if known_hosts_exists {
        std::fs::read_to_string(&known_path).unwrap_or_default()
    } else {
        String::new()
    };

    let known = parse_known_hosts_hosts(&known_text);
    let (parsed, skipped_wildcards) = parse_ssh_config(&config_text);
    let home_str = home.to_string_lossy().to_string();
    let hosts = parsed
        .into_iter()
        .map(|h| h.into_candidate(Some(&home_str), &known))
        .collect();

    Ok(SshConfigScan {
        hosts,
        skipped_wildcards,
        config_path: config_path.display().to_string(),
        config_exists,
        known_hosts_exists,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== parse_ssh_config:基本块与两种写法 =====

    #[test]
    fn test_parse_basic_host_block() {
        let text = "\
Host tencent1
    HostName 111.229.102.148
    User root
    Port 2222
    IdentityFile ~/.ssh/tencent.pem
";
        let (hosts, skipped) = parse_ssh_config(text);
        assert_eq!(skipped, 0);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "tencent1");
        assert_eq!(hosts[0].host, "111.229.102.148");
        assert_eq!(hosts[0].port, 2222);
        assert_eq!(hosts[0].user.as_deref(), Some("root"));
        assert_eq!(hosts[0].identity_raw.as_deref(), Some("~/.ssh/tencent.pem"));
    }

    #[test]
    fn test_parse_multiple_patterns_takes_first_token() {
        // 真机 config 实态:`Host alias ip` 双 token,别名取首个非通配 token
        let text = "\
Host tencent1deploy 111.229.102.148
    HostName 111.229.102.148
    User deploy
";
        let (hosts, _) = parse_ssh_config(text);
        assert_eq!(hosts[0].alias, "tencent1deploy");
        assert_eq!(hosts[0].host, "111.229.102.148");
        assert_eq!(hosts[0].user.as_deref(), Some("deploy"));
    }

    #[test]
    fn test_parse_case_insensitive_and_equals_form() {
        // 关键字大小写不敏感 + Key=Value 写法;混合出现也要能解析
        let text = "\
host mixed
  hostname=example.com
  PORT=2200
  user=ubuntu
";
        let (hosts, _) = parse_ssh_config(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "mixed");
        assert_eq!(hosts[0].host, "example.com");
        assert_eq!(hosts[0].port, 2200);
        assert_eq!(hosts[0].user.as_deref(), Some("ubuntu"));
    }

    #[test]
    fn test_parse_defaults_when_optional_keys_missing() {
        // HostName 缺省 → 回退 alias;Port/User/IdentityFile 缺省分别为 22/None/None
        let text = "Host lonely\n";
        let (hosts, _) = parse_ssh_config(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host, "lonely");
        assert_eq!(hosts[0].port, 22);
        assert_eq!(hosts[0].user, None);
        assert_eq!(hosts[0].identity_raw, None);
    }

    #[test]
    fn test_parse_first_value_wins_within_block() {
        // OpenSSH:首个值生效,同块内重复关键字后来的忽略
        let text = "\
Host dup
  User first
  User second
  Port 2200
  Port 2299
";
        let (hosts, _) = parse_ssh_config(text);
        assert_eq!(hosts[0].user.as_deref(), Some("first"));
        assert_eq!(hosts[0].port, 2200);
    }

    #[test]
    fn test_parse_ignores_comments_blank_lines_and_unknown_keys() {
        let text = "\
# 顶部注释
Include ~/.ssh/conf.d/*

Host real
  # 块内注释
  HostName real.example.com
  Compression yes
  ServerAliveInterval 60
";
        let (hosts, _) = parse_ssh_config(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "real");
        assert_eq!(hosts[0].host, "real.example.com");
    }

    #[test]
    fn test_parse_skips_wildcard_blocks() {
        // 通配块是全局默认,不是可导入主机;计数回显
        let text = "\
Host *
  User defaultuser
  Port 22

Host *.internal.example.com
  ProxyJump bastion

Host concrete
  HostName concrete.example.com
";
        let (hosts, skipped) = parse_ssh_config(text);
        assert_eq!(skipped, 2);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "concrete");
        // 通配块里的 User 不应泄漏到具体主机(它们不是同一个块)
        assert_eq!(hosts[0].user, None);
    }

    #[test]
    fn test_parse_invalid_port_falls_back_to_22() {
        // 端口不可解析/越界 → 22(不静默丢弃整条主机)
        let text = "\
Host badport
  Port 99999
Host notanumber
  Port abc
Host zero
  Port 0
";
        let (hosts, _) = parse_ssh_config(text);
        assert_eq!(hosts.len(), 3);
        assert!(hosts.iter().all(|h| h.port == 22));
    }

    #[test]
    fn test_parse_key_before_any_host_block_is_ignored() {
        // 首个 Host 之前的散落键值不产生主机(OpenSSH 里它们是全局段)
        let text = "\
User globaluser
Port 2222

Host after
  HostName after.example.com
";
        let (hosts, _) = parse_ssh_config(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "after");
        assert_eq!(hosts[0].port, 22); // 全局段不参与
    }

    #[test]
    fn test_parse_empty_text() {
        let (hosts, skipped) = parse_ssh_config("");
        assert!(hosts.is_empty());
        assert_eq!(skipped, 0);
    }

    // ===== expand_home_tokens =====

    #[test]
    fn test_expand_home_tokens() {
        let home = Some("C:\\Users\\you");
        // 正斜杠 `/` 是分隔符(Windows 文件 API 同样接受)
        assert_eq!(expand_home_tokens("~/.ssh/id_rsa", home), "C:\\Users\\you/.ssh/id_rsa");
        assert_eq!(expand_home_tokens("%d/.ssh/id_rsa", home), "C:\\Users\\you/.ssh/id_rsa");
        assert_eq!(expand_home_tokens("~", home), "C:\\Users\\you");
        assert_eq!(expand_home_tokens("%d", home), "C:\\Users\\you");
        // 主目录末尾自带分隔符时不双写(Windows 的 USERPROFILE 常无尾斜杠,
        // 但用户 HOME 变量可能带 —— 统一先剥尾再拼,避免 `\//`)
        assert_eq!(expand_home_tokens("~/x", Some("C:\\Users\\you\\")), "C:\\Users\\you/x");
        assert_eq!(expand_home_tokens("~/x", Some("/home/you/")), "/home/you/x");
        // 无记号 / 无主目录:原样
        assert_eq!(expand_home_tokens("/abs/path", home), "/abs/path");
        assert_eq!(expand_home_tokens("~/.ssh/id_rsa", None), "~/.ssh/id_rsa");
        // 前后空白剔除
        assert_eq!(expand_home_tokens("  /abs/path  ", home), "/abs/path");
    }

    // ===== parse_known_hosts_hosts =====

    #[test]
    fn test_parse_known_hosts_hosts() {
        let text = "\
# 注释行
121.41.65.161 ssh-ed25519 AAAAKEY1
example.com,www.example.com ssh-rsa AAAAKEY2
[10.0.0.1]:2222 ssh-ed25519 AAAAKEY3

|1|hashedsalty=|hashedvalue= ssh-ed25519 AAAAKEY4
";
        let set = parse_known_hosts_hosts(text);
        assert!(set.contains("121.41.65.161"));
        assert!(set.contains("example.com"));
        assert!(set.contains("www.example.com"));
        assert!(set.contains("10.0.0.1")); // [host]:port 取 host
        assert!(!set.contains("|1|hashedsalty=|hashedvalue=")); // 哈希名不可反查
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn test_parse_known_hosts_hosts_is_lowercased() {
        // 主机名大小写不敏感:统一小写存储,便于比对
        let set = parse_known_hosts_hosts("Example.COM ssh-rsa AAAA");
        assert!(set.contains("example.com"));
    }

    // ===== into_candidate:合成 =====

    #[test]
    fn test_into_candidate_marks_known_and_expands() {
        let known: HashSet<String> = ["111.229.102.148".to_string()].into_iter().collect();
        let parsed = ParsedHost {
            alias: "tencent1".into(),
            host: "111.229.102.148".into(),
            port: 22,
            user: Some("root".into()),
            identity_raw: Some("~/.ssh/tencent.pem".into()),
        };
        let cand = parsed.clone().into_candidate(Some("/home/u"), &known);
        assert!(cand.known_in_hosts);
        assert_eq!(cand.identity_file.as_deref(), Some("/home/u/.ssh/tencent.pem"));

        // 未命中 → false
        let unknown: HashSet<String> = HashSet::new();
        assert!(!parsed.clone().into_candidate(Some("/home/u"), &unknown).known_in_hosts);

        // alias 命中同样算已知(OpenSSH 记的名字视连接方式而定)
        let by_alias: HashSet<String> = ["tencent1".to_string()].into_iter().collect();
        assert!(parsed.into_candidate(Some("/home/u"), &by_alias).known_in_hosts);
    }
}
