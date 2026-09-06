//! 检查更新(UPGRADE-PLAN 阶段四「桌面体验」):请求 GitHub Releases 最新版,
//! 与当前版本按分段数值比较,返回新版信息供前端展示「前往下载」。
//!
//! 决策(见 UPGRADE-PLAN 4.3):自实现检查而非 tauri-plugin-updater ——
//! 后者需生成/保管签名密钥并托管 latest.json,对单发行渠道收益低。
//!
//! 取版本为两段式(真实环境实测:api.github.com 未认证请求 60 次/小时按
//! 出口 IP 计,经共享代理出口 IP 极易 403 限流):
//! 1. 主路径(重定向,免 API 限流):GET releases/latest 页面 URL
//!    ([`GITHUB_LATEST_RELEASE_PAGE`]),`redirect(Policy::none())` 禁自动跟随,
//!    GitHub 返回 302,从 `Location` 头(形如
//!    `https://github.com/yjkz/Docker-Deploy-SSH/releases/tag/v5.1.0`)
//!    的路径解析出 tag 即得最新版本 —— 全程不碰 api.github.com,
//!    页面重定向本身无配额限制;
//! 2. 回退路径(API):主路径任何失败(网络结构变化/响应异常/解析失败)
//!    → 回退到原 api.github.com 调用([`update_check_via_api`]),
//!    并在 notes 前缀注明「经 API 回退」。
//!
//! 两个路径共用代理构建([`build_http_client`])、错误分类
//! ([`classify_http_error`])与版本比较([`compare_versions`]/[`parse_version`])。

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::time::Duration;

/// 主路径:Release 页面 URL(非 API,免限流探测入口)。
///
/// 原理(免 API 限流):请求该 html 页面 URL,GitHub 固定返回 302,
/// `Location` 头为 `https://github.com/yjkz/Docker-Deploy-SSH/releases/tag/<tag>`
/// 形式,从路径解析 tag 即得最新版本 —— 不经过 api.github.com,
/// 不受其未认证配额(60 次/小时按出口 IP 计,共享代理出口极易 403)限制,
/// 页面重定向本身无配额。与 [`GITHUB_RELEASES_PAGE`] 同值但职责不同:
/// 此处是「探测请求目标」,彼处是「展示用兜底链接」。
const GITHUB_LATEST_RELEASE_PAGE: &str =
    "https://github.com/yjkz/Docker-Deploy-SSH/releases/latest";

/// 回退路径:GitHub API「最新 Release」端点(仅主路径失败时调用)。
///
/// 来源:本仓库 remote = `https://github.com/yjkz/Docker-Deploy-SSH.git`,
/// 按 GitHub REST 约定 `/repos/{owner}/{repo}/releases/latest` 拼接;
/// 发布 tag 形如 `v4.6.0`(带 v 前缀,解析时容错剥离)。
/// 注意:未认证请求 60 次/小时按出口 IP 计,共享代理出口易 403 限流。
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
    /// Release 页面链接(主路径 = 302 Location 本身;API 路径 = html_url,
    /// 前端「前往下载」用)
    pub url: String,
    /// 版本说明(主路径重定向探测为空串,前端对空 notes 已容错不渲染;
    /// API 路径 = body,最长 [`NOTES_MAX_CHARS`] 字符,回退时带「经 API 回退」前缀)
    pub notes: String,
}

/// GitHub `releases/latest` 响应中本实现关心的字段(仅回退路径使用)。
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
///   返回中文错误「代理地址无效」;`None`/空串 → 直连。两个路径共用。
/// - 主路径为重定向探测(notes 为空,前端对空 notes 已容错);回退路径为
///   GitHub API,成功时 notes 带前缀「经 API 回退(重定向探测失败: …)」。
/// - 传输层失败(超时/DNS/代理不可达/非预期状态)按类别返回中文错误并附原文;
///   回退路径中响应体不是合法 JSON 或 tag 无法解析版本号时,按
///   `latest = current`、`has_update = false` 处理并在 notes 说明原因
///   (不打断前端展示)。
#[tauri::command]
pub async fn update_check(proxy: Option<String>) -> Result<UpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();

    // 主路径:releases/latest 页面 302 重定向探测(无 API 配额限制)
    match update_check_via_redirect(proxy.as_deref(), &current).await {
        Ok(info) => Ok(info),
        Err(redirect_err) => {
            // 回退路径:GitHub REST API(未认证 60 次/小时按出口 IP 计)
            match update_check_via_api(proxy.as_deref(), &current).await {
                Ok(mut info) => {
                    // notes 前缀注明走了 API 回退,附主路径失败原因便于现场排查
                    let fallback_note =
                        format!("经 API 回退(重定向探测失败: {redirect_err})");
                    info.notes = if info.notes.is_empty() {
                        fallback_note
                    } else {
                        format!("{fallback_note};{}", info.notes)
                    };
                    Ok(info)
                }
                // 回退也失败:以 API 错误为主(分类提示更准),附加重定向失败原因
                Err(api_err) => Err(format!("{api_err}(重定向探测亦失败: {redirect_err})")),
            }
        }
    }
}

// ===== 主路径:重定向探测 =====

/// 主路径:请求 releases/latest 页面 URL,从 302 的 `Location` 头解析最新 tag。
///
/// GitHub 对该 html 页面 URL 固定返回 302,`Location` 为
/// `https://github.com/yjkz/Docker-Deploy-SSH/releases/tag/<tag>` 形式,
/// 不经过 api.github.com 的未认证配额(60 次/小时按出口 IP 计)。
/// 任一环节失败(网络/非 3xx/缺 Location/解析不出 tag/tag 非版本号)
/// 返回 Err,由 [`update_check`] 统一回退到 API 路径。
async fn update_check_via_redirect(
    proxy: Option<&str>,
    current: &str,
) -> Result<UpdateInfo, String> {
    // 禁自动跟随:要自己读 302 的 Location 头,不能让 reqwest 直接追到 tag 页
    let client = build_http_client(proxy, true)?;
    let response = client
        .get(GITHUB_LATEST_RELEASE_PAGE)
        .send()
        .await
        .map_err(|e| classify_http_error(&e))?;

    let status = response.status();
    if !status.is_redirection() {
        // 附响应原文(截断),便于现场排查(如代理劫持/网关认证页返回 200/403)
        let body = response.text().await.unwrap_or_default();
        let excerpt: String = body.chars().take(300).collect();
        return Err(format!(
            "重定向探测未返回 3xx(HTTP {}): {}",
            status.as_u16(),
            excerpt
        ));
    }

    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "重定向响应缺少 Location 头".to_string())?;

    let tag = parse_tag_from_location(&location)
        .ok_or_else(|| format!("无法从重定向地址解析版本 tag: {location}"))?;

    // 剥 v 前缀 + 版本比较与 API 路径共用;tag 非版本号 → Err 触发 API 回退
    let latest_display = tag.trim_start_matches(['v', 'V']).to_string();
    let ordering = compare_versions(current, &latest_display)
        .ok_or_else(|| format!("重定向 tag 非版本号(\"{tag}\")"))?;

    Ok(UpdateInfo {
        current: current.to_string(),
        latest: latest_display,
        has_update: ordering == Ordering::Greater,
        // 「前往下载」直达该版本 Release 页(Location 本身,https 链接)
        url: location,
        // 空 notes:tag 页无 body 摘要;前端 set-notes 对空串已容错(不渲染)
        notes: String::new(),
    })
}

/// 从 302 的 `Location` 头解析最新 Release tag(纯函数,便于单测)。
///
/// 期望形态:`https://github.com/{owner}/{repo}/releases/tag/<tag>`。
/// 容错:末尾斜杠、query、fragment、无 scheme 的相对路径形态
/// (如 `/yjkz/Docker-Deploy-SSH/releases/tag/v5.1.0`)。
/// 非 tag 路径、tag 段为空或含 `/`(GitHub 支持带斜杠 tag,此处保守判
/// 解析失败 → 触发 API 回退)返回 None。
fn parse_tag_from_location(location: &str) -> Option<String> {
    let trimmed = location.trim();
    // 绝对 URL:用 Url::parse 取 path(自动剥 query/fragment);
    // 解析失败(相对路径等):整串按 path 处理,交给后面的 marker 匹配
    let path = match reqwest::Url::parse(trimmed) {
        Ok(url) => url.path().to_string(),
        Err(_) => trimmed.to_string(),
    };
    let path = path.trim_end_matches('/');
    const TAG_MARKER: &str = "/releases/tag/";
    let start = path.find(TAG_MARKER)? + TAG_MARKER.len();
    let tag = &path[start..];
    if tag.is_empty() || tag.contains('/') {
        return None;
    }
    Some(tag.to_string())
}

// ===== 回退路径:GitHub REST API =====

/// 回退路径:GitHub API「最新 Release」(仅主路径失败时由 [`update_check`] 调用)。
///
/// 逻辑与两段式改造前的原实现一致:GitHub API 要求 User-Agent 非空(已在
/// [`build_http_client`] 固定带上),Accept 按 GitHub REST 推荐声明;传输层
/// 失败按类别返回中文错误并附原文;响应体不是合法 JSON 或 tag 无法解析
/// 版本号时,按 `latest = current`、`has_update = false` 兜底(不作为错误
/// 抛出,由 notes 说明原因)。
async fn update_check_via_api(proxy: Option<&str>, current: &str) -> Result<UpdateInfo, String> {
    // 保持默认重定向策略(与改造前一致,无需读中间跳转)
    let client = build_http_client(proxy, false)?;

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
        .map_err(|e| format!("读取 GitHub 响应失败: {e}"))?;

    let release: LatestRelease = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            // 响应不是合法 JSON:按「无法判断版本」兜底处理,不作为错误抛出
            let excerpt: String = body.chars().take(300).collect();
            return Ok(UpdateInfo {
                latest: current.to_string(),
                has_update: false,
                notes: format!("解析 GitHub 响应失败({e}),原文: {excerpt}"),
                current: current.to_string(),
                url: GITHUB_RELEASES_PAGE.to_string(),
            });
        }
    };

    // 版本比较:latest 无法解析(tag 缺失/非版本号)时同样按当前版本兜底
    let latest_tag = release.tag_name.trim().to_string();
    let latest_display = latest_tag.trim_start_matches(['v', 'V']).to_string();
    match compare_versions(current, &latest_display) {
        Some(ordering) => Ok(UpdateInfo {
            current: current.to_string(),
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
            latest: current.to_string(),
            has_update: false,
            notes: format!(
                "无法解析最新版本号(\"{latest_tag}\"),请前往 Release 页确认"
            ),
            current: current.to_string(),
            url: if release.html_url.is_empty() {
                GITHUB_RELEASES_PAGE.to_string()
            } else {
                release.html_url
            },
        }),
    }
}

// ===== 共用:HTTP 客户端构建 =====

/// 构建检查更新用的 HTTP 客户端(主/回退两路径共用)。
///
/// - 整体超时 15 秒;GitHub 要求 User-Agent 非空,固定带上当前版本号;
/// - `proxy`:trim 后非空才启用(空串等价直连);无效地址直接报错不发起请求;
/// - `no_redirect`:true 时 `redirect(Policy::none())` 禁自动跟随(主路径需
///   自己读 302 的 Location 头);false 保持默认(回退路径维持原行为)。
fn build_http_client(proxy: Option<&str>, no_redirect: bool) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("DockerDeploy-SSH/", env!("CARGO_PKG_VERSION")))
        .timeout(HTTP_TIMEOUT);
    if no_redirect {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    if let Some(proxy_url) = proxy
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
    {
        let proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|e| format!("代理地址无效({proxy_url}): {e}"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| format!("HTTP 客户端构建失败: {e}"))
}

// ===== 外部链接打开 =====

/// 用系统默认浏览器打开外部链接(Tauri WebView 内 window.open 外链会被吞)。
/// 仅允许 https:// 链接(当前唯一来源是 GitHub Release 页,防御性校验)。
#[tauri::command]
pub fn open_external(url: String) -> std::result::Result<(), String> {
    if !url.starts_with("https://") {
        return Err(format!("不允许打开非 https 链接: {url}"));
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", "", &url])
            .spawn()
            .map_err(|e| format!("打开浏览器失败: {e}"))?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = &url;
        Err("当前平台未支持外部链接打开".to_string())
    }
}

/// 把 reqwest 传输层错误归类为中文提示(附原文,便于排查)。
fn classify_http_error(err: &reqwest::Error) -> String {
    let raw = err.to_string();
    let lower = raw.to_ascii_lowercase();
    if err.is_timeout() {
        format!("请求超时(15 秒),请检查网络或代理: {raw}")
    } else if err.is_connect() {
        // 连接类失败再细分:DNS 解析失败 / 代理不可达 / 一般网络不可达
        if lower.contains("dns error")
            || lower.contains("failed to lookup")
            || lower.contains("getaddrinfo")
            || lower.contains("name or service not known")
            || lower.contains("no record")
        {
            format!("DNS 解析失败,请检查网络连接或代理地址: {raw}")
        } else if lower.contains("proxy") || lower.contains("socks") || lower.contains("tunnel") {
            format!("代理不可达,请检查代理地址与端口: {raw}")
        } else {
            format!("连接失败(服务器不可达或网络被拦截): {raw}")
        }
    } else {
        format!("网络请求失败: {raw}")
    }
}

/// 截断版本说明至 [`NOTES_MAX_CHARS`] 字符(按 Unicode 字符而非字节,中文安全)。
fn truncate_notes(notes: &str) -> String {
    let trimmed = notes.trim();
    if trimmed.chars().count() <= NOTES_MAX_CHARS {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(NOTES_MAX_CHARS).collect();
        format!("{head}…(已截断)")
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

    // ===== parse_tag_from_location(主路径 Location 头解析) =====

    #[test]
    fn test_parse_tag_from_location_standard() {
        // 标准形态(真实环境实测 GitHub 返回的 Location 即此形态)
        assert_eq!(
            parse_tag_from_location(
                "https://github.com/yjkz/Docker-Deploy-SSH/releases/tag/v5.1.0"
            ),
            Some("v5.1.0".to_string())
        );
        // 无 v 前缀 tag 原样返回(剥前缀交给版本比较阶段)
        assert_eq!(
            parse_tag_from_location(
                "https://github.com/yjkz/Docker-Deploy-SSH/releases/tag/5.1.0"
            ),
            Some("5.1.0".to_string())
        );
    }

    #[test]
    fn test_parse_tag_from_location_trailing_slash_query_fragment() {
        // 末尾斜杠
        assert_eq!(
            parse_tag_from_location(
                "https://github.com/yjkz/Docker-Deploy-SSH/releases/tag/v5.1.0/"
            ),
            Some("v5.1.0".to_string())
        );
        // 带 query
        assert_eq!(
            parse_tag_from_location(
                "https://github.com/yjkz/Docker-Deploy-SSH/releases/tag/v5.1.0?utm_source=feed"
            ),
            Some("v5.1.0".to_string())
        );
        // query + 末尾斜杠 + fragment 组合
        assert_eq!(
            parse_tag_from_location(
                "https://github.com/yjkz/Docker-Deploy-SSH/releases/tag/v5.1.0/?x=1#top"
            ),
            Some("v5.1.0".to_string())
        );
        // 相对路径形态(无 scheme/host,防御性容错)
        assert_eq!(
            parse_tag_from_location("/yjkz/Docker-Deploy-SSH/releases/tag/v5.1.0"),
            Some("v5.1.0".to_string())
        );
    }

    #[test]
    fn test_parse_tag_from_location_non_tag_paths() {
        // 非 tag 路径(releases/latest 页自身)→ None
        assert_eq!(
            parse_tag_from_location(
                "https://github.com/yjkz/Docker-Deploy-SSH/releases/latest"
            ),
            None
        );
        // 仓库根路径 → None
        assert_eq!(
            parse_tag_from_location("https://github.com/yjkz/Docker-Deploy-SSH"),
            None
        );
        // tag 段为空 → None
        assert_eq!(
            parse_tag_from_location("https://github.com/yjkz/Docker-Deploy-SSH/releases/tag/"),
            None
        );
        // 空串/非 URL 文本 → None
        assert_eq!(parse_tag_from_location(""), None);
        assert_eq!(parse_tag_from_location("v5.1.0"), None);
        // tag 段含斜杠(GitHub 支持带斜杠 tag,本实现保守判解析失败 → 触发 API 回退)
        assert_eq!(
            parse_tag_from_location(
                "https://github.com/yjkz/Docker-Deploy-SSH/releases/tag/release/v1.0"
            ),
            None
        );
    }

    // ===== 真机集成测试(默认忽略,需本机代理与 gh CLI) =====

    /// 真机验证(默认忽略;需本机代理 http://127.0.0.1:12450 与 gh CLI):
    /// `cargo test --lib update::tests::test_real_redirect_latest_tag -- --ignored --nocapture`
    ///
    /// 断言:主路径(重定向)解析出的 tag 与权威口径
    /// `HTTPS_PROXY=http://127.0.0.1:12450 gh api repos/yjkz/Docker-Deploy-SSH/releases/latest --jq .tag_name`
    /// 一致;url 为该 tag 的 Release 页;全链路 update_check 未走 API 回退。
    #[tokio::test]
    #[ignore]
    async fn test_real_redirect_latest_tag() {
        const PROXY: &str = "http://127.0.0.1:12450";
        let current = env!("CARGO_PKG_VERSION").to_string();

        // 主路径:重定向探测应成功,url 为该 tag 的 Release 页
        let info = update_check_via_redirect(Some(PROXY), &current)
            .await
            .expect("重定向主路径应成功(需本机代理可用)");
        assert!(
            info.url.contains("/releases/tag/"),
            "url 应为 tag Release 页,实际: {}",
            info.url
        );
        assert!(info.notes.is_empty(), "主路径 notes 应为空");

        // 权威口径:gh api(带 HTTPS_PROXY 环境变量)
        let output = std::process::Command::new("gh")
            .args([
                "api",
                "repos/yjkz/Docker-Deploy-SSH/releases/latest",
                "--jq",
                ".tag_name",
            ])
            .env("HTTPS_PROXY", PROXY)
            .output()
            .expect("无法启动 gh CLI(需安装并加入 PATH)");
        assert!(
            output.status.success(),
            "gh api 执行失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let gh_tag = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert!(!gh_tag.is_empty(), "gh api 输出为空");
        // gh 输出为 tag 原文(带 v 前缀),与 latest(已剥前缀)应一致
        assert_eq!(
            info.latest,
            gh_tag.trim_start_matches(['v', 'V']),
            "重定向解析 tag({})应与 gh api({})一致",
            info.latest,
            gh_tag
        );

        // 全链路:主路径成功 → 不应出现「经 API 回退」前缀
        let full = update_check(Some(PROXY.to_string()))
            .await
            .expect("update_check 全链路应成功");
        assert!(
            !full.notes.contains("经 API 回退"),
            "主路径成功时不应回退,notes: {}",
            full.notes
        );
    }
}
