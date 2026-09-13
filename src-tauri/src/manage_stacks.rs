//! 远程 Compose 栈管理模块(C 阶段)。
//!
//! 通过现有 SSH 通道在远程服务器执行 `docker compose` 命令,实现栈(Compose 项目)的
//! 扫描、启停、查看服务状态与日志。
//! 复用 `crate::manage` 中已 pub(crate) 的连接与执行助手,保持低耦合:
//! `connect_server` → `with_timeout` / `shell_quote` / `exec_json_list` / `exec_action`。
//!
//! 输出约定:
//! - `docker compose ps --format json` 输出为 NDJSON(每行一个 JSON 对象),
//!   字段为 PascalCase;栈未启动时输出为空(返回空 Vec 而非错误)。
//! - `docker compose logs` 输出为合并的文本,原样返回。

use serde::{Deserialize, Serialize};

use crate::manage::{
    connect_server, exec_action, exec_json_list, is_docker_perm_denied, shell_quote,
    with_timeout, ActionResult, EXEC_TIMEOUT_SECS,
};
use crate::ssh::exec_collect;

/// up / down 操作可能涉及镜像拉取与容器重建,超时放宽到 120 秒
const STACK_ACTION_TIMEOUT_SECS: u64 = 120;

// ===== 返回前端结构(snake_case,Serialize) =====

/// 扫描到的栈:dir 为 compose 文件所在目录,compose_file 为完整路径
#[derive(Debug, Serialize)]
pub struct StackRow {
    dir: String,
    compose_file: String,
}

/// `docker compose ps --format json` 解析结果(核心四字段)
#[derive(Debug, Serialize)]
pub struct StackPsRow {
    name: String,
    image: String,
    state: String,
    status: String,
}

// ===== Docker NDJSON 输出结构(PascalCase,仅 Deserialize) =====

/// `docker compose ps --format json` 每行一个对象;部分版本可能带 ID / Publishers / Ports 字段
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawStackPs {
    /// 容忍输出中的 ID 字段(解析时忽略,避免未知字段报错场景;本身不使用)
    #[serde(rename = "ID", default)]
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    status: String,
}

// ===== 内部助手 =====

/// 取 compose 文件的父目录(远端为 Unix 路径,手动按 '/' 分割;无 '/' 时视为当前目录)
fn parent_dir_of(compose_file: &str) -> String {
    match compose_file.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => compose_file[..i].to_string(),
        None => ".".to_string(),
    }
}

/// 构造 `docker compose -f <file> --project-directory <dir>` 前缀
fn compose_prefix(compose_file: &str) -> String {
    let dir = parent_dir_of(compose_file);
    format!(
        "docker compose -f {} --project-directory {}",
        shell_quote(compose_file),
        shell_quote(&dir)
    )
}

// ===== Tauri 命令 =====

/// 扫描 remote_dir 下(深度 ≤4,与回滚中心/清理分析的项目扫描口径一致)的
/// compose 文件,返回栈列表。
///
/// `include_archived`(camelCase;缺省 false):默认**排除**本应用部署归档内的
/// compose 副本(`*/releases/*` 下的文件 —— 深度放开到 4 后会被扫进来,但那是
/// 回滚留档不是可操作栈);勾选「显示归档副本」时不过滤。
#[tauri::command]
pub async fn manage_list_stacks(
    server_id: String,
    password_plain: Option<String>,
    include_archived: Option<bool>,
) -> Result<Vec<StackRow>, String> {
    let include_archived = include_archived.unwrap_or(false);
    let (server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;

    // find 多 -name 需 \( \) 与 -o 组合;深度 ≤4(第八批:与回滚中心/清理分析的
    // 项目扫描口径统一,原为 ≤2 —— 部署目录下超过一层子目录的栈会扫不到)。
    // 默认追加 ! -path '*/releases/*' 排除归档副本(与项目扫描同一排除口径)。
    let archived_filter = if include_archived {
        String::new()
    } else {
        String::from(" ! -path '*/releases/*'")
    };
    let cmd = format!(
        "find {} -maxdepth 4 -type f \\( -name 'docker-compose.yml' -o -name 'docker-compose.yaml' -o -name 'compose.yml' -o -name 'compose.yaml' \\){} ",
        shell_quote(&server.remote_dir),
        archived_filter
    );
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "扫描 Compose 栈超时",
        "请检查服务器网络后重试",
        async { exec_collect(&mut client, &cmd).await },
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "扫描目录「{}」失败(目录可能不存在或无权限): {}",
            server.remote_dir,
            out.trim()
        ));
    }

    // 每行为一个 compose 文件完整路径,按最后一个 '/' 分离目录与文件名
    let mut stacks: Vec<StackRow> = Vec::new();
    for line in out.lines() {
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        let dir = parent_dir_of(path);
        stacks.push(StackRow {
            dir,
            compose_file: path.to_string(),
        });
    }
    stacks.sort_by(|a, b| a.compose_file.cmp(&b.compose_file));
    Ok(stacks)
}

/// 对指定栈执行 up -d / down 操作
#[tauri::command]
pub async fn manage_stack_action(
    server_id: String,
    password_plain: Option<String>,
    compose_file: String,
    action: String,
) -> Result<ActionResult, String> {
    let sub = match action.as_str() {
        "up" => "up -d",
        "down" => "down",
        other => return Err(format!("不支持的栈操作: {}(仅支持 up / down)", other)),
    };
    let cmd = format!("{} {}", compose_prefix(&compose_file), sub);
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        STACK_ACTION_TIMEOUT_SECS,
        "栈操作超时",
        "栈较大或涉及镜像拉取,请稍后重试",
        exec_action(&mut client, &cmd),
    )
    .await
}

/// 查看栈内服务状态;栈未启动时输出为空,返回空 Vec
#[tauri::command]
pub async fn manage_stack_ps(
    server_id: String,
    password_plain: Option<String>,
    compose_file: String,
) -> Result<Vec<StackPsRow>, String> {
    let cmd = format!("{} ps --format json", compose_prefix(&compose_file));
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    // compose ps 非 0 退出码(如 "no configuration file")→ Err 原文(含权限兜底);
    // 输出为空(栈未启动)时 exec_json_list 正常返回空 Vec
    let raw: Vec<RawStackPs> = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取栈服务状态超时",
        "请检查服务器网络后重试",
        exec_json_list(&mut client, &cmd),
    )
    .await?;
    Ok(raw
        .into_iter()
        .map(|p| StackPsRow {
            name: p.name,
            image: p.image,
            state: p.state,
            status: p.status,
        })
        .collect())
}

/// 查看栈日志;tail=0 表示取全部(不带 --tail 参数)
#[tauri::command]
pub async fn manage_stack_logs(
    server_id: String,
    password_plain: Option<String>,
    compose_file: String,
    tail: u32,
) -> Result<String, String> {
    let tail_arg = if tail == 0 {
        String::new()
    } else {
        format!(" --tail {}", tail)
    };
    let cmd = format!("{} logs{} 2>&1", compose_prefix(&compose_file), tail_arg);
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取栈日志超时",
        "请检查服务器网络后重试",
        async { exec_collect(&mut client, &cmd).await },
    )
    .await?;
    if code != 0 {
        if is_docker_perm_denied(&out) {
            return Err(crate::errors::perm_denied());
        }
        return Err(format!("docker compose logs 失败(退出码 {}): {}", code, out.trim()));
    }
    Ok(out)
}

// ===== compose .env 查看/编辑(UPGRADE-PLAN 阶段五) =====

// base64 编解码(远端传输用;与 commands.rs/config_io.rs 同一惯用法)。
// 说明:按约定新代码在文件末尾追加,故 use 置于此处(Rust 模块级 items 顺序无关)。
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;

/// .env 内容大小上限:256KB(按字节计;超出直接拒绝,避免远端命令行过长)
const STACK_ENV_MAX_BYTES: usize = 256 * 1024;

/// compose 栈同目录 .env 文件的读取结果。
/// 注意:本结构按契约使用 camelCase(前端直接消费),与文件内其他 snake_case 结构不同。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StackEnv {
    /// 远端是否存在 .env 文件
    exists: bool,
    /// .env 文件内容(UTF-8;不存在时为空串)
    content: String,
    /// 文件含非 UTF-8 字节(如 GBK)时为 true:content 是 lossy 展示值(U+FFFD
    /// 替换符),**不能直接保存回写**(替换符一旦落盘即永久损坏原始字节)
    #[serde(default)]
    not_utf8: bool,
    /// 原始字节 base64(仅 not_utf8=true 时非空):前端在「未改动」时把它
    /// 原样传回 → 原始字节无损回写(第 N 批,消除 lossy 回写乱码风险)
    #[serde(default)]
    raw_b64: String,
}

/// 内容是否超出 .env 大小上限(按字节,与字符数无关)
fn env_content_too_large(content: &str) -> bool {
    content.len() > STACK_ENV_MAX_BYTES
}

/// 拼接 dir 下的 .env 完整路径;dir 为 "/"(根目录)时避免拼出 "//.env"
fn env_path_of(dir: &str) -> String {
    if dir.ends_with('/') {
        format!("{}.env", dir)
    } else {
        format!("{}/.env", dir)
    }
}

/// 构造原子写命令:先把 base64 解码重定向到临时文件 `.env.ddtmp.$$`(`$$` 由 shell
/// 展开为当前 shell 的 PID,并发保存各自独占临时文件,消除交错损坏窗口),
/// 成功后 `mv` 覆盖目标 .env(mv 同文件系统内为 rename,写入中断不损坏原文件)。
/// b64 字符集(A-Za-z0-9+/=)无 shell 元字符,按规格直接拼接;路径统一经 shell_quote。
/// 注意 `$$` 必须拼在引号外(单引号内不展开):仅对前缀路径加引号,`$$` 后缀裸拼
/// (展开结果为纯数字,无注入面)。
fn env_write_cmd(env_path: &str, b64: &str) -> String {
    let tmp_prefix = format!("{}.ddtmp.", env_path);
    format!(
        "echo {} | base64 -d > {}$$ && mv {}$$ {}",
        b64,
        shell_quote(&tmp_prefix),
        shell_quote(&tmp_prefix),
        shell_quote(env_path)
    )
}

/// 将远端 `base64` 命令输出解码为**原始字节**:剔除 ASCII 空白(GNU/BusyBox
/// base64 默认 76 列换行)后标准解码。UTF-8 与否的分流在 read 命令内完成
/// (纯 UTF-8 → content;含非 UTF-8 → lossy 展示 + raw_b64 原始字节,见
/// [`manage_stack_env_read`])。
fn decode_remote_b64_bytes(out: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = out.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    BASE64_STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| format!(".env base64 解码失败: {}", e))
}

/// 解析远端 `wc -c <文件` 输出为字节数(输出可能带换行/前后空白)
fn parse_remote_size(out: &str) -> Result<usize, String> {
    out.trim()
        .parse::<usize>()
        .map_err(|_| format!("解析 .env 大小失败: {}", out.trim()))
}

/// 读取 compose 文件同目录的 .env(远端 base64 编码传输,本地解码,防编码/二进制损坏)。
/// 文件不存在返回 { exists: false, content: "" } 而非错误。
#[tauri::command]
pub async fn manage_stack_env_read(
    server_id: String,
    password_plain: Option<String>,
    compose_file: String,
) -> Result<StackEnv, String> {
    let quoted = shell_quote(&env_path_of(&parent_dir_of(&compose_file)));
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;

    // 1) 判存在:test -f 退出码 0=存在,1=不存在,>1=检查出错(权限等)。
    //    本命令非 docker 系,不走 is_docker_perm_denied 兜底,失败给普通中文错误。
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "检查 .env 是否存在超时",
        "请检查服务器网络后重试",
        async { exec_collect(&mut client, &format!("test -f {}", quoted)).await },
    )
    .await?;
    if code > 1 {
        return Err(format!("检查 .env 失败(退出码 {}): {}", code, out.trim()));
    }
    if code != 0 {
        return Ok(StackEnv {
            exists: false,
            content: String::new(),
            not_utf8: false,
            raw_b64: String::new(),
        });
    }

    // 2) 大小检查:超上限直接拒绝,避免超大文件撑爆前端编辑器/传输通道。
    //    选 `wc -c <文件` 而非 `stat -c %s`:wc 为 POSIX 工具,GNU/BSD/BusyBox
    //    输出一致(stat 格式参数在部分 BusyBox 构建上不可用);重定向 `<` 使输出
    //    只有数字,便于可靠解析
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "检查 .env 大小超时",
        "请检查服务器网络后重试",
        async { exec_collect(&mut client, &format!("wc -c <{}", quoted)).await },
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "检查 .env 大小失败(退出码 {}): {}",
            code,
            out.trim()
        ));
    }
    let size = parse_remote_size(&out)?;
    if size > STACK_ENV_MAX_BYTES {
        return Err(".env 文件过大(上限 256KB),请在服务器上直接编辑".to_string());
    }

    // 3) 读回:`base64 <文件` 输出可能带 76 列换行,本地去空白后解码
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "读取 .env 超时",
        "请检查服务器网络后重试",
        async { exec_collect(&mut client, &format!("base64 <{}", quoted)).await },
    )
    .await?;
    if code != 0 {
        return Err(format!("读取 .env 失败(退出码 {}): {}", code, out.trim()));
    }
    // 第 N 批(非 UTF-8 无损往返):解码原始字节 → 纯 UTF-8 直接返回;
    // 含非 UTF-8 字节(如 GBK)时 content 为 lossy 展示值,同时带回原始字节
    // base64,前端「未改动」保存时原样回传 → 原始字节无损落盘(见 save)
    let bytes = decode_remote_b64_bytes(&out)?;
    match String::from_utf8(bytes) {
        Ok(content) => Ok(StackEnv {
            exists: true,
            content,
            not_utf8: false,
            raw_b64: String::new(),
        }),
        Err(e) => {
            let bytes = e.into_bytes();
            Ok(StackEnv {
                exists: true,
                content: String::from_utf8_lossy(&bytes).into_owned(),
                not_utf8: true,
                raw_b64: BASE64_STANDARD.encode(&bytes),
            })
        }
    }
}

/// 保存 compose 文件同目录的 .env(原子写:先写 `.env.ddtmp.$$` 再 mv 覆盖,可新建文件)。
/// `raw_b64`(可选,第 N 批):非 UTF-8 文件「未改动」的原样回写 —— 前端在
/// 编辑器内容与读取时的 lossy 展示完全一致时传回 `rawB64`(读取命令带回的
/// 原始字节),后端跳过 content 直接落盘原始字节,消除 U+FFFD 回写乱码;
/// 用户改过内容(或未传 raw_b64)时按 content 正常 UTF-8 写入。
#[tauri::command]
pub async fn manage_stack_env_save(
    server_id: String,
    password_plain: Option<String>,
    compose_file: String,
    content: String,
    raw_b64: Option<String>,
) -> Result<ActionResult, String> {
    // 写入字节:优先 raw_b64 原样回写(校验合法 base64 且不超上限);
    // 否则按 content(UTF-8 文本)编码
    let bytes: Vec<u8> = match raw_b64.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(b64) => {
            let decoded = BASE64_STANDARD
                .decode(b64.as_bytes())
                .map_err(|e| format!("rawB64 解码失败: {}", e))?;
            if decoded.len() > STACK_ENV_MAX_BYTES {
                return Err(".env 内容过大(上限 256KB)".to_string());
            }
            decoded
        }
        None => {
            // 大小上限校验(按字节):超出直接拒绝,不发往远端
            if env_content_too_large(&content) {
                return Err(".env 内容过大(上限 256KB)".to_string());
            }
            content.into_bytes()
        }
    };
    let env_path = env_path_of(&parent_dir_of(&compose_file));
    let cmd = env_write_cmd(&env_path, &BASE64_STANDARD.encode(&bytes));
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let result = with_timeout(
        EXEC_TIMEOUT_SECS,
        "保存 .env 超时",
        "请检查服务器网络后重试",
        exec_action(&mut client, &cmd),
    )
    .await?;
    // exec_action 对非 0 退出码返回 success=false(带输出原文);按契约转为 Err,
    // 附「写入失败」上下文(目录不存在/权限不足等直接体现在原文中)
    if !result.success {
        return Err(format!("写入失败: {}", result.message));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parent_dir_of_variants() {
        // 锁定既有路径推导行为(env 两条新命令依赖同一逻辑)
        assert_eq!(parent_dir_of("/opt/app/docker-compose.yml"), "/opt/app");
        assert_eq!(parent_dir_of("/docker-compose.yml"), "/"); // Some(0) → 根目录
        assert_eq!(parent_dir_of("docker-compose.yml"), "."); // 无 '/' → 当前目录
    }

    #[test]
    fn test_env_path_of_variants() {
        assert_eq!(env_path_of("/opt/app"), "/opt/app/.env");
        assert_eq!(env_path_of("/"), "/.env"); // 根目录不拼出 "//.env"
        assert_eq!(env_path_of("."), "./.env");
        assert_eq!(env_path_of("/opt/app/"), "/opt/app/.env"); // 容错尾部斜杠
    }

    #[test]
    fn test_env_write_cmd_quotes_and_chains() {
        // 临时文件带 `$$`(shell 展开 PID),且拼在引号外保证可展开;mv 同步引用
        assert_eq!(
            env_write_cmd("/opt/app/.env", "QUJD"),
            "echo QUJD | base64 -d > '/opt/app/.env.ddtmp.'$$ && mv '/opt/app/.env.ddtmp.'$$ '/opt/app/.env'"
        );
        // 路径含单引号时经 shell_quote 转义,不破坏命令结构
        let cmd = env_write_cmd("/opt/a'pp/.env", "QUJD");
        assert!(cmd.contains("'\\''pp/.env.ddtmp.'$$"));
        assert!(cmd.ends_with("mv '/opt/a'\\''pp/.env.ddtmp.'$$ '/opt/a'\\''pp/.env'"));
    }

    #[test]
    fn test_decode_remote_b64_roundtrip_with_wrap() {
        // 远端 base64 默认按列换行;解码前去空白,换行不影响结果
        let content = "PORT=8080\nDB_PASS=p@ss'word\"!\n";
        let encoded = BASE64_STANDARD.encode(content.as_bytes());
        let wrapped = encoded
            .as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).unwrap().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(decode_remote_b64_bytes(&wrapped).unwrap(), content.as_bytes());
        // 末尾换行(命令输出常见)同样不影响
        assert_eq!(
            decode_remote_b64_bytes(&format!("{}\n", encoded)).unwrap(),
            content.as_bytes()
        );
    }

    #[test]
    fn test_decode_remote_b64_empty_and_non_utf8() {
        // 空文件 → 空字节
        assert_eq!(decode_remote_b64_bytes("\n").unwrap(), Vec::<u8>::new());
        // 非 UTF-8 字节(如 GBK 内容)→ 原样保留原始字节(read 命令据此分流
        // lossy 展示与 raw_b64 回写,第 N 批无损往返的底座)
        assert_eq!(
            decode_remote_b64_bytes(&BASE64_STANDARD.encode([0xFF, 0xFE, b'a'])).unwrap(),
            vec![0xFF, 0xFE, b'a']
        );
        // 非 base64 字符 → 明确报错而非 panic
        assert!(decode_remote_b64_bytes("!!!!").is_err());
    }

    #[test]
    fn test_parse_remote_size() {
        // 常见输出:数字 + 结尾换行(`wc -c <文件` 不带文件名前缀)
        assert_eq!(parse_remote_size("4096\n").unwrap(), 4096);
        // 空文件 / 带前后空白
        assert_eq!(parse_remote_size("0").unwrap(), 0);
        assert_eq!(parse_remote_size(" 128 \n").unwrap(), 128);
        // 非数字(权限报错等异常输出)→ 明确报错而非静默按 0 处理
        assert!(parse_remote_size("wc: /x/.env: Permission denied").is_err());
    }

    #[test]
    fn test_env_b64_has_no_shell_metacharacters() {
        // 拼入命令行前的不变量:b64 字符集仅 A-Za-z0-9+/=
        let content = "A=1\nB='two words' $X `cmd` ; rm -rf /\n".repeat(10);
        let b64 = BASE64_STANDARD.encode(content.as_bytes());
        assert!(b64
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }

    #[test]
    fn test_env_content_size_limit_boundary() {
        // 恰等于上限放行(仅严格大于拒绝)
        assert_eq!(STACK_ENV_MAX_BYTES, 256 * 1024);
        assert!(!env_content_too_large(&"a".repeat(STACK_ENV_MAX_BYTES)));
        assert!(env_content_too_large(&"a".repeat(STACK_ENV_MAX_BYTES + 1)));
        // 多字节字符按字节计(中文 3 字节/字)
        assert!(env_content_too_large(&"中".repeat((STACK_ENV_MAX_BYTES + 3) / 3)));
    }
}
