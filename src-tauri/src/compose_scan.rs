//! Compose 扫描识别放宽(第二十六批):自定义 compose 文件名与扫描深度。
//!
//! 背景:此前四个标准名(`docker-compose.yml/.yaml`、`compose.yml/.yaml`)与
//! 深度 4 是硬编码的 —— 用户用 `docker-compose.prod.yml`、`app.yml` 等变体命名,
//! 或在更深目录里放栈时扫不到。
//!
//! ## 安全边界(本模块的存在理由)
//!
//! 文件名与深度**会拼进远端 shell 命令**的 `find ... -name ...` 里,故必须校验:
//! - 文件名:拒绝含 `/`、`\`、`..`、空串、控制字符、超长 —— 它们能改变 find 的
//!   路径语义(如 `../etc/passwd`),或绕过 `-name` 的"单文件"假设;
//!   允许字符集保守限定为 `[A-Za-z0-9._-]`(覆盖所有常见变体命名)。
//! - 深度:夹取 1..=8(过深会拖慢扫描;过浅扫不到既有部署)。
//!
//! 校验失败的处理:**整项回退到内置默认**(而不是报错阻断扫描)—— 设置是
//! 可选增强,配错了不该让整个栈列表不可用;回退时 log::warn 留痕。

/// 内置默认 compose 文件名(四标准名,与历史硬编码口径一致)。
pub(crate) const DEFAULT_COMPOSE_NAMES: [&str; 4] = [
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yml",
    "compose.yaml",
];

/// 内置默认扫描深度(与历史硬编码口径一致)。
pub(crate) const DEFAULT_SCAN_DEPTH: u32 = 4;

/// 深度下限/上限(夹取范围)。
pub(crate) const MIN_SCAN_DEPTH: u32 = 1;
pub(crate) const MAX_SCAN_DEPTH: u32 = 8;

/// 文件名最大长度(保守;真实 compose 名不会超过这个量级)。
const MAX_NAME_LEN: usize = 64;

/// 单个文件名是否合法(纯函数)。
///
/// 规则:非空、≤ [`MAX_NAME_LEN`]、仅 `[A-Za-z0-9._-]`、不以 `.` 或 `-` 开头
/// (避免 `..`/隐藏文件/误当选项),且不含 `..` 子串。
pub(crate) fn is_valid_compose_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return false;
    }
    if name.starts_with('.') || name.starts_with('-') {
        return false;
    }
    if name.contains("..") {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// 归一化用户配置的文件名列表 → 实际用于扫描的列表(纯函数)。
///
/// - 空列表 / 全部非法 → 回退 [`DEFAULT_COMPOSE_NAMES`];
/// - 逐项校验,非法项丢弃;至少保留一个合法项(否则回退默认);
/// - 去重(保持输入顺序,便于界面回显稳定);
/// - 数量上限 16(防超长 find 命令)。
pub(crate) fn normalize_compose_names(configured: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in configured {
        let name = raw.trim();
        if !is_valid_compose_name(name) {
            continue;
        }
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
        if out.len() >= 16 {
            break;
        }
    }
    if out.is_empty() {
        DEFAULT_COMPOSE_NAMES.iter().map(|s| s.to_string()).collect()
    } else {
        out
    }
}

/// 夹取扫描深度到 [MIN, MAX];0(未配置/关闭态)按默认处理。
pub(crate) fn clamp_scan_depth(configured: u32) -> u32 {
    if configured == 0 {
        return DEFAULT_SCAN_DEPTH;
    }
    configured.clamp(MIN_SCAN_DEPTH, MAX_SCAN_DEPTH)
}

/// 拼 `find` 的 `-name` 部分:`\\( -name 'a' -o -name 'b' \\)`(纯函数)。
///
/// 每个名字都经 [`is_valid_compose_name`] 校验过的调用方才应进入(此处再兜底
/// 丢弃非法项 —— 命令拼装是最后一道防线);非法项全部剔除后若为空,回退默认。
pub(crate) fn find_name_clause(names: &[String]) -> String {
    let valid: Vec<String> = names
        .iter()
        .map(|n| n.trim().to_string())
        .filter(|n| is_valid_compose_name(n))
        .collect();
    let list = if valid.is_empty() {
        DEFAULT_COMPOSE_NAMES.iter().map(|s| s.to_string()).collect()
    } else {
        valid
    };
    let parts: Vec<String> = list.iter().map(|n| format!("-name '{}'", n)).collect();
    format!("\\( {} \\)", parts.join(" -o "))
}

/// 从设置读「扫描名字 + 深度」的便捷入口(两个扫描点共用同一口径)。
pub(crate) fn scan_config_from_settings() -> (Vec<String>, u32) {
    let settings = crate::config::load_app_settings();
    let names = normalize_compose_names(&settings.compose_file_names);
    let depth = clamp_scan_depth(settings.compose_scan_max_depth);
    (names, depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== is_valid_compose_name:安全边界 =====

    #[test]
    fn test_valid_names_accepted() {
        for ok in [
            "docker-compose.yml",
            "docker-compose.yaml",
            "compose.yml",
            "compose.yaml",
            "docker-compose.prod.yml",
            "app.yml",
            "my_stack-compose.yml",
            "DockerCompose.YML",
        ] {
            assert!(is_valid_compose_name(ok), "应合法: {}", ok);
        }
    }

    #[test]
    fn test_path_traversal_and_separators_rejected() {
        // 安全核心:任何能改变 find 路径语义的输入必须拒绝
        for bad in [
            "../etc/passwd",
            "sub/dir.yml",
            "..",
            "a..b.yml",          // 含 .. 但无斜杠 —— 只有 .. 检查能挡
            "a/../b.yml",
            "dir\\file.yml",     // 反斜杠
            ".hidden.yml",       // 隐藏文件
            "-o -name x",        // 伪造 find 选项
            "a;rm -rf /",        // 命令注入尝试
            "a`whoami`.yml",     // 反引号
            "a$HOME.yml",        // 变量展开
            "a'b.yml",           // 单引号(即便 shell_quote 会转义,也不该放进来)
            "space name.yml",    // 空格
            "tab\tname.yml",     // 控制字符
            "",                  // 空
            &"x".repeat(65),     // 超长
        ] {
            assert!(!is_valid_compose_name(bad), "应拒绝: {:?}", bad);
        }
    }

    // ===== normalize_compose_names =====

    #[test]
    fn test_normalize_falls_back_to_defaults_on_empty_or_all_invalid() {
        let d = || DEFAULT_COMPOSE_NAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(normalize_compose_names(&[]), d());
        assert_eq!(normalize_compose_names(&["".into(), "../x".into()]), d());
    }

    #[test]
    fn test_normalize_keeps_valid_drops_invalid_dedupes() {
        let got = normalize_compose_names(&[
            "custom.yml".into(),
            "../bad.yml".into(),      // 丢弃
            "custom.yml".into(),      // 去重
            "second.yaml".into(),
            "  third.yml  ".into(),   // trim
        ]);
        assert_eq!(got, vec!["custom.yml", "second.yaml", "third.yml"]);
    }

    #[test]
    fn test_normalize_caps_at_16() {
        let many: Vec<String> = (0..30).map(|i| format!("c{}.yml", i)).collect();
        assert_eq!(normalize_compose_names(&many).len(), 16);
    }

    // ===== clamp_scan_depth =====

    #[test]
    fn test_clamp_scan_depth() {
        assert_eq!(clamp_scan_depth(0), DEFAULT_SCAN_DEPTH); // 未配置 → 默认
        assert_eq!(clamp_scan_depth(1), 1);
        assert_eq!(clamp_scan_depth(4), 4);
        assert_eq!(clamp_scan_depth(8), 8);
        assert_eq!(clamp_scan_depth(99), MAX_SCAN_DEPTH); // 上夹
        // 注意 u32 无负数;下夹由 MIN 保证
        assert_eq!(clamp_scan_depth(MIN_SCAN_DEPTH), MIN_SCAN_DEPTH);
    }

    // ===== find_name_clause =====

    #[test]
    fn test_find_name_clause_shape() {
        let clause = find_name_clause(&["a.yml".into(), "b.yaml".into()]);
        assert_eq!(clause, "\\( -name 'a.yml' -o -name 'b.yaml' \\)");
    }

    #[test]
    fn test_find_name_clause_falls_back_when_all_invalid() {
        // 命令拼装是最后一道防线:调用方传进非法项也不得进入命令
        let clause = find_name_clause(&["../x".into(), "a;b".into()]);
        assert!(clause.contains("docker-compose.yml"), "应回退默认: {}", clause);
        assert!(!clause.contains(".."), "非法项泄漏进命令: {}", clause);
        assert!(!clause.contains(';'), "非法项泄漏进命令: {}", clause);
    }

    #[test]
    fn test_find_name_clause_drops_invalid_keeps_valid() {
        let clause = find_name_clause(&["ok.yml".into(), "bad/name.yml".into()]);
        assert!(clause.contains("'ok.yml'"), "{}", clause);
        assert!(!clause.contains("bad"), "非法项泄漏: {}", clause);
    }
}
