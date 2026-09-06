//! 检查更新(UPGRADE-PLAN 阶段四「桌面体验」):请求 GitHub Releases 最新版,
//! 与当前版本按分段数值比较,返回新版信息供前端展示「前往下载」。
//!
//! 决策(见 UPGRADE-PLAN 4.3):自实现检查而非 tauri-plugin-updater ——
//! 后者需生成/保管签名密钥并托管 latest.json,对单发行渠道收益低。

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::time::Duration;

/// GitHub API「最新 Release」端点。
///
/// 来源:本仓库 remote = `https://github.com/yjkz/Docker-Deploy-SSH.git`,
/// 按 GitHub REST 约定 `/repos/{owner}/{repo}/releases/latest` 拼接;
/// 发布 tag 形如 `v4.6.0`(带 v 前缀,解析时容错剥离)。
const GITHUB_LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/yjkz/Docker-Deploy-SSH/releases/latest";

/// Release 页面链接兜底值(API 未返回 html_url 时使用,与上面的仓库一致)。
const GITHUB_RELEASES_PAGE: &str = "https://github.com/yjkz/Docker-Deploy-SSH/releases/latest";

/// 客户端整体超时:15 秒(自发起请求至响应体接收完毕)。
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// 版本说明(body)最长保留字符数:超出截断,避免超长 changelog 撑爆前端模态。
const NOTES_MAX_CHARS: usize = 2000;

/// 检查更新结果(serde camelCase 对齐前端 JS 字段)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// 当前版本(Cargo.toml 的 version,经 `env!("CARGO_PKG_VERSION")` 编译期注入)
    pub current: String,
    /// 最新版本(tag_name 剥 v 前缀;响应无法解析时 = 当前版本)
    pub latest: String,
    /// 是否有更新(latest > current)
    pub has_update: bool,
    /// Release 页面链接(html_url,前端「前往下载」用)
    pub url: String,
    /// 版本说明(body,最长 [`NOTES_MAX_CHARS`] 字符;解析失败时附带原因说明)
    pub notes: String,
}

/// GitHub `releases/latest` 响应中本实现关心的字段。
#[derive(Debug, Deserialize)]
struct LatestRelease {
    /// 发布 tag(如 "v4.7.0")
    #[serde(default)]
    tag_name: String,
    /// Release 页面链接
    #[serde(default)]
    html_url: String,
    /// 版本说明正文(可能为空)
    #[serde(default)]
    body: String,
}

/// 检查更新(设置中心「检查更新」/「测试连接」共用)。
///
/// - `proxy`:`Some(非空)` → 经该代理请求,支持 `http://` 与 `socks5://`
///   (及 socks5h/socks4)前缀,依赖 reqwest 的 `socks` feature;地址无效
///   返回中文错误「代理地址无效」;`None`/空串 → 直连。
/// - GitHub API 要求 User-Agent 非空,固定带上当前版本号。
/// - 传输层失败(超时/DNS/代理不可达/非 200)按类别返回中文错误并附原文;
///   响应体不是合法 JSON 或 tag 无法解析版本号时,按 `latest = current`、
///   `has_update = false` 处理并在 notes 说明原因(不打断前端展示)。
// ===== 外部链接打开 =====

/// 用系统默认浏览器打开外部链接(Tauri WebView 内 window.open 外链会被吞)。
/// 仅允许 https:// 链接(当前唯一来源是 GitHub Release 页,防御性校验)。
#[tauri::command]
pub fn open_external(url: String) -> std::result::Result<(), String> {
    if !url.starts_with("https://") {
        return Err(format!("不允许打开非 https 链接: {}", url));
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", "", &url])
            .spawn()
            .map_err(|e| format!("打开浏览器失败: {}", e))?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = &url;
        Err("当前平台未支持外部链接打开".to_string())
    }
}

#[tauri::command]
pub async fn update_check(proxy: Option<String>) -> Result<UpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();

    // HTTP 客户端:整体超时 15s;GitHub API 要求非空 User-Agent
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("DockerDeploy-SSH/", env!("CARGO_PKG_VERSION")))
        .timeout(HTTP_TIMEOUT);

    // 代理:trim 后非空才启用(空串等价直连);无效地址直接报错不发起请求
    let proxy_input = proxy
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    if let Some(proxy_url) = proxy_input {
        let proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|e| format!("代理地址无效({}): {}", proxy_url, e))?;
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|e| format!("HTTP 客户端构建失败: {}", e))?;

    let response = client
        .get(GITHUB_LATEST_RELEASE_API)
        // GitHub REST 推荐 Accept 头(缺省也能用,显式声明更稳)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| classify_http_error(&e))?;

    let status = response.status();
    if !status.is_success() {
        // 附响应原文(截断),便于现场排查(限流时 GitHub 会给出说明 JSON)
        let body = response.text().await.unwrap_or_default();
        let excerpt: String = body.chars().take(300).collect();
        if status.as_u16() == 403 {
            return Err(format!(
                "GitHub API 请求被限流(HTTP 403,未认证请求每小时限额,请稍后重试): {}",
                excerpt
            ));
        }
        return Err(format!(
            "GitHub API 返回异常状态(HTTP {}): {}",
            status.as_u16(),
            excerpt
        ));
    }

    let body = response
        .text()
        .await
        .map_err(|e| format!("读取 GitHub 响应失败: {}", e))?;

    let release: LatestRelease = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            // 响应不是合法 JSON:按「无法判断版本」兜底处理,不作为错误抛出
            let excerpt: String = body.chars().take(300).collect();
            return Ok(UpdateInfo {
                latest: current.clone(),
                has_update: false,
                notes: format!("解析 GitHub 响应失败({}),原文: {}", e, excerpt),
                current,
                url: GITHUB_RELEASES_PAGE.to_string(),
            });
        }
    };

    // 版本比较:latest 无法解析(tag 缺失/非版本号)时同样按当前版本兜底
    let latest_tag = release.tag_name.trim().to_string();
    let latest_display = latest_tag.trim_start_matches(['v', 'V']).to_string();
    match compare_versions(&current, &latest_display) {
        Some(ordering) => Ok(UpdateInfo {
            current,
            latest: latest_display,
            has_update: ordering == Ordering::Greater,
            url: if release.html_url.is_empty() {
                GITHUB_RELEASES_PAGE.to_string()
            } else {
                release.html_url
            },
            notes: truncate_notes(&release.body),
        }),
        None => Ok(UpdateInfo {
            latest: current.clone(),
            has_update: false,
            notes: format!(
                "无法解析最新版本号(\"{}\"),请前往 Release 页确认",
                latest_tag
            ),
            current,
            url: if release.html_url.is_empty() {
                GITHUB_RELEASES_PAGE.to_string()
            } else {
                release.html_url
            },
        }),
    }
}

/// 把 reqwest 传输层错误归类为中文提示(附原文,便于排查)。
fn classify_http_error(err: &reqwest::Error) -> String {
    let raw = err.to_string();
    let lower = raw.to_ascii_lowercase();
    if err.is_timeout() {
        format!("请求超时(15 秒),请检查网络或代理: {}", raw)
    } else if err.is_connect() {
        // 连接类失败再细分:DNS 解析失败 / 代理不可达 / 一般网络不可达
        if lower.contains("dns error")
            || lower.contains("failed to lookup")
            || lower.contains("getaddrinfo")
            || lower.contains("name or service not known")
            || lower.contains("no record")
        {
            format!("DNS 解析失败,请检查网络连接或代理地址: {}", raw)
        } else if lower.contains("proxy") || lower.contains("socks") || lower.contains("tunnel") {
            format!("代理不可达,请检查代理地址与端口: {}", raw)
        } else {
            format!("连接失败(服务器不可达或网络被拦截): {}", raw)
        }
    } else {
        format!("网络请求失败: {}", raw)
    }
}

/// 截断版本说明至 [`NOTES_MAX_CHARS`] 字符(按 Unicode 字符而非字节,中文安全)。
fn truncate_notes(notes: &str) -> String {
    let trimmed = notes.trim();
    if trimmed.chars().count() <= NOTES_MAX_CHARS {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(NOTES_MAX_CHARS).collect();
        format!("{}…(已截断)", head)
    }
}

/// 版本号解析(剥 `v`/`V` 前缀后按 `.` 分段):每段取开头连续数字转 u64
/// (如 "0-beta" → 0),整串不含数字视为无法解析,返回 None。
fn parse_version(version: &str) -> Option<Vec<u64>> {
    let stripped = version.trim().trim_start_matches(['v', 'V']);
    if stripped.is_empty() || !stripped.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(
        stripped
            .split('.')
            .map(|seg| {
                let digits: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse::<u64>().unwrap_or(0)
            })
            .collect(),
    )
}

/// 版本比较:剥 v 前缀后按 `.` 分段数值比较,短版本缺的段按 0 补齐
/// (如 "4.6" 与 "4.6.0" 相等、"4.6" < "4.6.1")。
///
/// 返回 latest 相对 current 的 [`Ordering`](`Greater` = 有更新);
/// 任一版本无法解析(空/不含数字,见 [`parse_version`])返回 None,
/// 调用方按「无法判断」兜底(has_update = false)。
fn compare_versions(current: &str, latest: &str) -> Option<Ordering> {
    let current_segments = parse_version(current)?;
    let latest_segments = parse_version(latest)?;
    let len = current_segments.len().max(latest_segments.len());
    for i in 0..len {
        let cur = current_segments.get(i).copied().unwrap_or(0);
        let lat = latest_segments.get(i).copied().unwrap_or(0);
        if lat != cur {
            return Some(lat.cmp(&cur));
        }
    }
    Some(Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compare_versions_equal() {
        // 完全相等 / v 前缀剥离 / 缺段补零均视为相等
        assert_eq!(compare_versions("4.6.0", "4.6.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("4.6.0", "v4.6.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("4.6.0", "V4.6.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("4.6", "4.6.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("4.6.0", "4.6"), Some(Ordering::Equal));
    }

    #[test]
    fn test_compare_versions_newer_and_older() {
        // 有更新:latest > current(逐段比较,不按字符串序,"10" > "9")
        assert_eq!(compare_versions("4.6.0", "4.7.0"), Some(Ordering::Greater));
        assert_eq!(
            compare_versions("4.6.0", "5.0.0"),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_versions("4.9.9", "4.10.0"),
            Some(Ordering::Greater)
        );
        // 无更新:latest <= current
        assert_eq!(compare_versions("4.7.0", "4.6.0"), Some(Ordering::Less));
        assert_eq!(compare_versions("5.0.0", "5.0.0"), Some(Ordering::Equal));
    }

    #[test]
    fn test_compare_versions_segment_edge_cases() {
        // 末段带预发布后缀:取段内开头数字("0-beta" → 0)
        assert_eq!(
            compare_versions("4.6.0", "4.7.0-beta"),
            Some(Ordering::Greater)
        );
        // 段内前导数字解析("2rc" → 2)
        assert_eq!(compare_versions("4.6.1", "4.6.2rc1"), Some(Ordering::Greater));
        // 空段按 0
        assert_eq!(compare_versions("4.6.0", "4.6."), Some(Ordering::Equal));
    }

    #[test]
    fn test_compare_versions_unparseable() {
        // 任一版本不含数字 → 无法判断(None)
        assert_eq!(compare_versions("4.6.0", "abc"), None);
        assert_eq!(compare_versions("", "4.6.0"), None);
        assert_eq!(compare_versions("4.6.0", ""), None);
        assert_eq!(compare_versions("4.6.0", "latest"), None);
    }

    #[test]
    fn test_truncate_notes() {
        // 短文本原样返回(去首尾空白)
        assert_eq!(truncate_notes("  更新说明  "), "更新说明");
        assert_eq!(truncate_notes(""), "");
        // 超长按 2000 字符截断并附标记(按字符数而非字节数,中文安全)
        let long = "更".repeat(2500);
        let cut = truncate_notes(&long);
        assert_eq!(cut.chars().count(), NOTES_MAX_CHARS + "…(已截断)".chars().count());
        assert!(cut.starts_with(&"更".repeat(NOTES_MAX_CHARS)));
        assert!(cut.ends_with("…(已截断)"));
        // 恰好 2000 字符不截断
        let exact = "a".repeat(NOTES_MAX_CHARS);
        assert_eq!(truncate_notes(&exact), exact);
    }

    #[test]
    fn test_update_info_camel_case_serde() {
        // UpdateInfo 序列化为 camelCase,对齐前端字段
        let info = UpdateInfo {
            current: "4.6.0".into(),
            latest: "4.7.0".into(),
            has_update: true,
            url: "https://github.com/yjkz/Docker-Deploy-SSH/releases/tag/v4.7.0".into(),
            notes: "说明".into(),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"current\""));
        assert!(json.contains("\"latest\""));
        assert!(json.contains("\"hasUpdate\":true"));
        assert!(json.contains("\"url\""));
        assert!(json.contains("\"notes\""));
        let back: UpdateInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, info);
    }
}
