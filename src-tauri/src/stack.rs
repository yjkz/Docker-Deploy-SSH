//! compose 文件解析 + 本地镜像三级匹配(纯逻辑,便于单测)。
//!
//! 职责:
//! - [`parse_compose_file`]:读取 compose YAML → 顶层 `services` 逐服务解析
//!   (image / build)→ 用 compose 同目录 `.env` 只对 image 字段做变量插值
//!   → 按默认规则分类(has_build→Local;仅 image→Pull)→ 与本地镜像
//!   (`docker images` 的 repo/tag 对)做 Exact / RepoOnly / Missing 三级匹配;
//! - [`apply_overrides`]:用项目保存的 service_overrides 覆盖默认分类。
//!
//! 解析约定:
//! - 顶层 `x-` 开头的扩展键自动忽略(只读取 `name` 与 `services`);
//! - YAML 合并键(`<<: *anchor`)显式应用(serde_yaml 不自动合并),
//!   compose 常用锚点注入公共 image/build;
//! - `build` 字段存在即可(字符串或映射都算 has_build);
//! - image 与 build 都缺失的服务仍保留在 services 中,同时记入 `errors`
//!   (部署前由前端红框阻断);
//! - 镜像引用无标签时按 Docker 约定补 `latest` 参与匹配。

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{ServiceOverride, TransferMode};

/// 本地镜像匹配状态(前端匹配徽章的三态)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchState {
    /// 本地存在完全一致的 repo:tag
    Exact,
    /// 本地存在同名仓库但标签不一致
    RepoOnly,
    /// 本地不存在该仓库
    Missing,
}

/// 解析出的单个 compose 服务及其默认分类/匹配结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackService {
    pub service: String,
    /// 插值后的镜像引用;compose 未设 image 字段时为 None
    pub image: Option<String>,
    /// compose 是否有 build 字段(字符串或映射都算)
    pub has_build: bool,
    /// 传输方式(默认分类;可被 service_overrides 覆盖)
    pub mode: TransferMode,
    pub match_state: MatchState,
    /// 本地实际存在的标签(Exact 时与 compose 相同;RepoOnly 时为本地任一标签)
    pub local_tag: Option<String>,
    /// 非阻断警告(插值未定义变量 / 标签不一致 / 本地不存在 / 未设 image)
    pub warning: Option<String>,
}

/// compose 解析结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeStack {
    /// compose 顶层 `name`;未设置时取文件名去扩展名
    pub project_name: String,
    pub services: Vec<StackService>,
    /// 阻断性错误(如服务既无 image 也无 build);非空时前端红框禁止部署
    pub errors: Vec<String>,
}

/// 解析 compose 文件并做本地镜像三级匹配。
///
/// * `compose_path`:compose YAML 路径(插值用的 `.env` 取其同目录);
/// * `local_images`:`docker images` 的 (repository, tag) 对。
///
/// 非 YAML / 缺少 services 等结构性失败返回 `Err`(中文);
/// 单个服务的非阻断问题记入 [`ComposeStack::errors`] / `warning`。
pub fn parse_compose_file(
    compose_path: &Path,
    local_images: &[(String, String)],
) -> Result<ComposeStack, String> {
    let text = std::fs::read_to_string(compose_path).map_err(|e| {
        format!(
            "读取 compose 文件失败 ({}): {}",
            compose_path.display(),
            e
        )
    })?;
    let mut value: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| format!("解析 compose 文件失败,不是有效的 YAML: {}", e))?;
    // serde_yaml 不自动应用合并键(<<):compose 惯用锚点 + `<<: *common` 注入
    // image/build 等公共字段,必须显式合并,否则相关服务会被误判为未设置 image/build
    value.apply_merge().map_err(|e| {
        format!("解析 compose 文件失败,处理 YAML 合并键(<<)失败: {}", e)
    })?;
    if !value.is_mapping() {
        return Err("compose 文件顶层结构不正确,应为键值映射".to_string());
    }

    // 项目名:顶层 name(字符串且非空)优先,否则取文件名去扩展名
    let project_name = value
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            compose_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "compose".to_string())
        });

    let services_value = value
        .get("services")
        .ok_or_else(|| "compose 文件缺少 services 配置,无法解析".to_string())?;
    let services_map = services_value
        .as_mapping()
        .ok_or_else(|| "compose 文件 services 配置格式不正确,应为键值映射".to_string())?;
    if services_map.is_empty() {
        return Err("compose 文件 services 为空,无法解析".to_string());
    }

    let env_map = load_env_file(compose_path.parent().unwrap_or_else(|| Path::new("")));

    let mut services = Vec::new();
    let mut errors = Vec::new();
    for (key, entry) in services_map {
        let Some(service_name) = key.as_str() else {
            errors.push("compose 文件中存在非字符串的服务名,已跳过".to_string());
            continue;
        };
        let Some(entry_map) = entry.as_mapping() else {
            errors.push(format!(
                "服务「{}」配置格式不正确,应为键值映射",
                service_name
            ));
            continue;
        };

        // image 字段:字符串才有效;缺失或 null 视为未设置
        let image_raw = match entry_map.get("image") {
            None | Some(serde_yaml::Value::Null) => None,
            Some(v) => match v.as_str() {
                Some(s) => Some(s.to_string()),
                None => {
                    errors.push(format!(
                        "服务「{}」的 image 字段应为字符串,已按未设置处理",
                        service_name
                    ));
                    None
                }
            },
        };
        // build 字段存在即可(字符串或映射都算 has_build),null 视为未设置
        let has_build = matches!(entry_map.get("build"), Some(v) if !v.is_null());

        // env 插值只作用于 image 字段
        let mut warnings: Vec<String> = Vec::new();
        let image = image_raw
            .map(|raw| {
                let interpolated = interpolate_env_inner(&raw, &env_map, &mut warnings);
                let trimmed = interpolated.trim().to_string();
                if trimmed.is_empty() {
                    None // 插值后为空(如未定义变量)按未设置处理
                } else {
                    Some(trimmed)
                }
            })
            .flatten();

        let (mode, match_state, local_tag, match_warning) =
            classify(image.as_deref(), has_build, local_images);
        if let Some(w) = match_warning {
            warnings.push(w);
        }
        if image.is_none() {
            if has_build {
                warnings.push(
                    "未设 image 字段,无法核验/传输,请在 compose 补 image:".to_string(),
                );
            } else {
                errors.push(format!(
                    "服务「{}」未设置 image 且没有 build 配置,无法部署",
                    service_name
                ));
            }
        }
        let warning = if warnings.is_empty() {
            None
        } else {
            Some(warnings.join("; "))
        };

        services.push(StackService {
            service: service_name.to_string(),
            image,
            has_build,
            mode,
            match_state,
            local_tag,
            warning,
        });
    }

    Ok(ComposeStack {
        project_name,
        services,
        errors,
    })
}

/// 用项目保存的 service_overrides 覆盖解析出的默认分类
/// (仅按服务名精确匹配,未覆盖的服务保持默认)。
pub fn apply_overrides(services: &mut [StackService], overrides: &[ServiceOverride]) {
    for svc in services.iter_mut() {
        if let Some(o) = overrides.iter().find(|o| o.service == svc.service) {
            svc.mode = o.mode.clone();
        }
    }
}

/// 分类默认规则 + 本地三级匹配(纯函数)。
///
/// 规则:has_build→Local;仅 image→Pull;Missing 且无 build→Pull+警告
/// “本地不存在,将由服务器拉取”;RepoOnly→标签不一致警告;
/// image 缺失时匹配状态记 Missing(image 缺失的警告/错误由调用方处理)。
/// 返回 `(传输方式, 匹配状态, 本地实际标签, 匹配类警告)`。
fn classify(
    image: Option<&str>,
    has_build: bool,
    local_images: &[(String, String)],
) -> (TransferMode, MatchState, Option<String>, Option<String>) {
    let Some(image) = image else {
        let mode = if has_build {
            TransferMode::Local
        } else {
            TransferMode::Pull
        };
        return (mode, MatchState::Missing, None, None);
    };
    let (repo, tag) = split_image_ref(image);
    let mode = if has_build {
        TransferMode::Local
    } else {
        TransferMode::Pull
    };
    if local_images.iter().any(|(r, t)| r == &repo && t == &tag) {
        return (mode, MatchState::Exact, Some(tag), None);
    }
    // 同名仓库但标签不一致:取本地第一个同名仓库的标签(多为最近构建)
    if let Some((r, t)) = local_images.iter().find(|(r, _)| r == &repo) {
        let warning = format!("本地标签不一致:compose 要 {},本地有 {}:{}", image, r, t);
        return (mode, MatchState::RepoOnly, Some(t.clone()), Some(warning));
    }
    let warning = if has_build {
        None
    } else {
        Some("本地不存在,将由服务器拉取".to_string())
    };
    (mode, MatchState::Missing, None, warning)
}

/// 把镜像引用拆成 `(仓库, 标签)`;无标签时按 Docker 约定补 `latest`。
/// 标签分隔取最后一个 `/` 之后的第一个 `:`
/// (因此仓库内含端口的 `reg:5000/app:v1` 也能正确拆分)。
fn split_image_ref(image: &str) -> (String, String) {
    let s = image.trim();
    let after_last_slash = match s.rfind('/') {
        Some(i) => &s[i + 1..],
        None => s,
    };
    match after_last_slash.find(':') {
        Some(j) => {
            let split = s.len() - after_last_slash.len() + j;
            (s[..split].to_string(), s[split + 1..].to_string())
        }
        None => (s.to_string(), "latest".to_string()),
    }
}

/// 对 image 字段做变量插值(丢弃未定义变量警告的便捷入口)。
///
/// 支持 `${VAR}`、`${VAR:-default}`、`$VAR` 与 `$$`(字面 `$`);
/// 未定义变量替换为空串。需要收集警告时用 [`interpolate_env_inner`]。
pub fn interpolate_env(raw: &str, env: &HashMap<String, String>) -> String {
    let mut warnings = Vec::new();
    interpolate_env_inner(raw, env, &mut warnings)
}

/// [`interpolate_env`] 的完整实现:未定义变量替换为空串并收集警告。
fn interpolate_env_inner(
    raw: &str,
    env: &HashMap<String, String>,
    warnings: &mut Vec<String>,
) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '$' {
            out.push(c);
            i += 1;
            continue;
        }
        if i + 1 < chars.len() && chars[i + 1] == '$' {
            out.push('$'); // $$ → 字面 $
            i += 2;
            continue;
        }
        if i + 1 < chars.len() && chars[i + 1] == '{' {
            match chars[i + 2..].iter().position(|&ch| ch == '}') {
                Some(close) => {
                    let end = i + 2 + close;
                    let content: String = chars[i + 2..end].iter().collect();
                    push_var_expr(&content, env, &mut out, warnings);
                    i = end + 1;
                }
                None => {
                    out.push('$'); // 无闭合大括号,按字面输出
                    i += 1;
                }
            }
            continue;
        }
        // 裸 $VAR:取连续的字母/数字/下划线
        let mut j = i + 1;
        while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        if j == i + 1 {
            out.push('$'); // '$' 后不是变量字符,按字面输出
            i += 1;
            continue;
        }
        let name: String = chars[i + 1..j].iter().collect();
        push_var_value(&name, env, &mut out, warnings);
        i = j;
    }
    out
}

/// 处理 `${NAME}` / `${NAME:-default}` 大括号表达式的内容。
fn push_var_expr(
    content: &str,
    env: &HashMap<String, String>,
    out: &mut String,
    warnings: &mut Vec<String>,
) {
    if let Some((name, default)) = content.split_once(":-") {
        let name = name.trim();
        if name.is_empty() {
            out.push_str(content); // "${:-x}" 等非法写法,原样保留
            return;
        }
        // `:-` 语义:变量未定义或为空串时用默认值
        let value = env.get(name).map(String::as_str).unwrap_or("");
        if value.is_empty() {
            out.push_str(default);
        } else {
            out.push_str(value);
        }
        return;
    }
    let name = content.trim();
    if name.is_empty() {
        out.push_str(content); // "${}" 原样保留
        return;
    }
    push_var_value(name, env, out, warnings);
}

/// 按变量名取值拼入输出;未定义 → 空串并收集警告。
fn push_var_value(
    name: &str,
    env: &HashMap<String, String>,
    out: &mut String,
    warnings: &mut Vec<String>,
) {
    match env.get(name) {
        Some(v) => out.push_str(v),
        None => warnings.push(format!("环境变量 {} 未定义,已替换为空串", name)),
    }
}

/// 读取 compose 同目录的 `.env`(若存在)为 KEY→VALUE 表。
///
/// 逐行 `KEY=VALUE`;忽略 `#` 注释行、空行与无 `=` 的行;
/// 键值两端空白剔除,成对包裹的单/双引号剥除。
fn load_env_file(dir: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let path = dir.join(".env");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return map,
        Err(e) => {
            log::warn!("读取 .env 失败 ({}): {}", path.display(), e);
            return map;
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        map.insert(key.to_string(), unquote(value.trim()));
    }
    map
}

/// 剥除成对包裹的单/双引号(compose 对 .env 值的处理方式)。
fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个临时 fixture 目录(测试结束自行清理)。
    fn temp_fixture_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ddtest-compose-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 按服务名取解析结果中的服务。
    fn find_svc<'a>(stack: &'a ComposeStack, name: &str) -> &'a StackService {
        stack
            .services
            .iter()
            .find(|s| s.service == name)
            .unwrap_or_else(|| panic!("服务 {} 不在解析结果中", name))
    }

    // ===== 1. 基本解析:2 服务(build+image)分类正确;x- 键忽略 =====

    #[test]
    fn test_parse_basic_two_services() {
        let dir = temp_fixture_dir();
        let path = dir.join("stack.yml");
        std::fs::write(
            &path,
            "x-common: &common\n  restart: unless-stopped\n\nservices:\n  web:\n    build: ./web\n    image: myapp:latest\n  api:\n    build:\n      context: ./api\n      dockerfile: Dockerfile\n    image: api:dev\n",
        )
        .unwrap();
        let local = vec![
            ("myapp".to_string(), "latest".to_string()),
            ("api".to_string(), "dev".to_string()),
        ];
        let stack = parse_compose_file(&path, &local).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert!(stack.errors.is_empty());
        assert_eq!(stack.project_name, "stack"); // 文件名去扩展名
        assert_eq!(stack.services.len(), 2);
        // build(字符串)+ image,本地精确匹配 → Local + Exact
        let web = find_svc(&stack, "web");
        assert_eq!(web.mode, TransferMode::Local);
        assert_eq!(web.match_state, MatchState::Exact);
        assert_eq!(web.local_tag.as_deref(), Some("latest"));
        assert!(web.warning.is_none());
        // build(映射)同样算 has_build → Local
        let api = find_svc(&stack, "api");
        assert!(api.has_build);
        assert_eq!(api.mode, TransferMode::Local);
    }

    // ===== 锚点 + 合并键(<<):x-common 注入公共 image/build =====

    #[test]
    fn test_parse_yaml_merge_keys() {
        let dir = temp_fixture_dir();
        let path = dir.join("stack.yml");
        // serde_yaml 不自动应用合并键;x-common 锚点注入 image 和 build,
        // api 服务在合并后再覆盖 image(显式键优先于合并键)
        std::fs::write(
            &path,
            "x-common: &common\n  build: ./svc\n  image: shared:v1\n\nservices:\n  web:\n    <<: *common\n  api:\n    <<: *common\n    image: api:override\n",
        )
        .unwrap();
        let local = vec![
            ("shared".to_string(), "v1".to_string()),
            ("api".to_string(), "override".to_string()),
        ];
        let stack = parse_compose_file(&path, &local).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert!(stack.errors.is_empty(), "errors: {:?}", stack.errors);
        // web:靠 << 注入 image/build,不得误判"未设置 image 且没有 build 配置"
        let web = find_svc(&stack, "web");
        assert!(web.has_build, "<< 注入的 build 应生效");
        assert_eq!(web.image.as_deref(), Some("shared:v1"), "<< 注入的 image 应生效");
        assert_eq!(web.match_state, MatchState::Exact);
        assert!(web.warning.is_none());
        // api:显式 image 覆盖合并键的 image;build 仍由 << 注入(不被误分类为 Pull)
        let api = find_svc(&stack, "api");
        assert!(api.has_build);
        assert_eq!(api.image.as_deref(), Some("api:override"));
        assert_eq!(api.mode, TransferMode::Local);
    }

    // ===== 2. ${IMAGE}:${TAG} + .env 插值 =====

    #[test]
    fn test_parse_env_interpolation() {
        let dir = temp_fixture_dir();
        let path = dir.join("stack.yml");
        std::fs::write(&path, "services:\n  app:\n    image: ${IMAGE}:${TAG}\n").unwrap();
        std::fs::write(dir.join(".env"), "IMAGE=myapp\nTAG=v1\n\n# 注释行\n").unwrap();
        let local = vec![("myapp".to_string(), "v1".to_string())];
        let stack = parse_compose_file(&path, &local).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let app = find_svc(&stack, "app");
        assert_eq!(app.image.as_deref(), Some("myapp:v1"));
        assert_eq!(app.match_state, MatchState::Exact);
        assert!(app.warning.is_none());
    }

    // ===== 3. ${VAR:-fallback} 默认值;未定义变量警告 =====

    #[test]
    fn test_interpolate_default_and_undefined_warning() {
        // 纯插值行为
        let empty = HashMap::new();
        assert_eq!(interpolate_env("${A:-fallback}", &empty), "fallback");
        let mut env = HashMap::new();
        env.insert("A".to_string(), "x".to_string());
        assert_eq!(interpolate_env("${A:-fallback}", &env), "x");
        // 变量已定义但为空串 → 也用默认值(`:-` 语义)
        env.insert("A".to_string(), String::new());
        assert_eq!(interpolate_env("${A:-fallback}", &env), "fallback");
        // 裸 $VAR 与 $$
        let mut env2 = HashMap::new();
        env2.insert("H".to_string(), "hello".to_string());
        assert_eq!(interpolate_env("$H/world", &env2), "hello/world");
        assert_eq!(interpolate_env("$$notvar", &env2), "$notvar");

        // 未定义变量 → 空串 + 警告(经 inner 收集)
        let mut warnings = Vec::new();
        let out = interpolate_env_inner("app:${MISS}", &empty, &mut warnings);
        assert_eq!(out, "app:");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("MISS"), "警告应含变量名: {}", warnings[0]);

        // 端到端:默认值生效且不告警;未定义变量产生服务级 warning
        let dir = temp_fixture_dir();
        let path = dir.join("stack.yml");
        std::fs::write(
            &path,
            "services:\n  svc1:\n    image: ${REG:-registry.example}/app:1.0\n  svc2:\n    build: .\n    image: app2:${VER}\n",
        )
        .unwrap();
        let local = vec![("registry.example/app".to_string(), "1.0".to_string())];
        let stack = parse_compose_file(&path, &local).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let svc1 = find_svc(&stack, "svc1");
        assert_eq!(svc1.image.as_deref(), Some("registry.example/app:1.0"));
        assert_eq!(svc1.match_state, MatchState::Exact);
        assert!(svc1.warning.is_none());
        let svc2 = find_svc(&stack, "svc2");
        assert_eq!(svc2.image.as_deref(), Some("app2:"));
        assert_eq!(svc2.mode, TransferMode::Local);
        let warning = svc2.warning.as_deref().unwrap_or_default();
        assert!(warning.contains("VER 未定义"), "应含未定义变量警告: {}", warning);
    }

    // ===== 4. 三级匹配:Exact / RepoOnly / Missing =====

    #[test]
    fn test_match_three_states() {
        let dir = temp_fixture_dir();
        let path = dir.join("stack.yml");
        std::fs::write(
            &path,
            "services:\n  a:\n    image: alpha:1\n  b:\n    image: beta:2\n  c:\n    image: gamma:3\n",
        )
        .unwrap();
        let local = vec![
            ("alpha".to_string(), "1".to_string()),
            ("beta".to_string(), "9".to_string()),
        ];
        let stack = parse_compose_file(&path, &local).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        // Exact:本地有完全一致的 repo:tag
        let a = find_svc(&stack, "a");
        assert_eq!(a.match_state, MatchState::Exact);
        assert_eq!(a.local_tag.as_deref(), Some("1"));
        assert!(a.warning.is_none());
        // RepoOnly:同名仓库但标签不一致,local_tag 记本地实际标签
        let b = find_svc(&stack, "b");
        assert_eq!(b.match_state, MatchState::RepoOnly);
        assert_eq!(b.local_tag.as_deref(), Some("9"));
        let warning = b.warning.as_deref().unwrap_or_default();
        assert!(
            warning.contains("本地标签不一致") && warning.contains("beta:2") && warning.contains("beta:9"),
            "标签不一致警告应含双方引用: {}", warning
        );
        // Missing:本地无该仓库;无 build → Pull + “本地不存在”警告
        let c = find_svc(&stack, "c");
        assert_eq!(c.match_state, MatchState::Missing);
        assert_eq!(c.local_tag, None);
        assert_eq!(c.mode, TransferMode::Pull);
        assert_eq!(c.warning.as_deref(), Some("本地不存在,将由服务器拉取"));
    }

    // ===== 5. build 无 image 字段 → warning;无 image 无 build → errors =====

    #[test]
    fn test_build_without_image_and_no_image_no_build() {
        let dir = temp_fixture_dir();
        let path = dir.join("stack.yml");
        std::fs::write(
            &path,
            "services:\n  worker:\n    build: .\n  broken:\n    ports:\n      - \"80:80\"\n",
        )
        .unwrap();
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        // has_build 且 image 缺失 → Local + 指定警告文案
        let worker = find_svc(&stack, "worker");
        assert_eq!(worker.mode, TransferMode::Local);
        assert_eq!(worker.image, None);
        assert_eq!(
            worker.warning.as_deref(),
            Some("未设 image 字段,无法核验/传输,请在 compose 补 image:")
        );
        // 无 image 且无 build → 记入 errors(阻断部署),服务仍保留在列表中
        assert!(stack
            .errors
            .iter()
            .any(|e| e.contains("broken") && e.contains("image")), "errors 应含 broken: {:?}", stack.errors);
        let broken = find_svc(&stack, "broken");
        assert_eq!(broken.image, None);
        assert!(!broken.has_build);
        assert_eq!(broken.match_state, MatchState::Missing);
    }

    // ===== 6. service_overrides 覆盖默认分类 =====

    #[test]
    fn test_apply_overrides() {
        let dir = temp_fixture_dir();
        let path = dir.join("stack.yml");
        std::fs::write(
            &path,
            "services:\n  web:\n    build: ./web\n    image: myapp:latest\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        let mut stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        // 默认:web=Local(build),db=Pull(仅 image)
        assert_eq!(find_svc(&stack, "web").mode, TransferMode::Local);
        assert_eq!(find_svc(&stack, "db").mode, TransferMode::Pull);
        // 覆盖:web→Pull;未覆盖的 db 保持默认
        apply_overrides(
            &mut stack.services,
            &[ServiceOverride { service: "web".into(), mode: TransferMode::Pull }],
        );
        assert_eq!(find_svc(&stack, "web").mode, TransferMode::Pull);
        assert_eq!(find_svc(&stack, "db").mode, TransferMode::Pull);
        // 不存在的服务名不影响任何服务
        apply_overrides(
            &mut stack.services,
            &[ServiceOverride { service: "ghost".into(), mode: TransferMode::Local }],
        );
        assert_eq!(find_svc(&stack, "web").mode, TransferMode::Pull);
        assert_eq!(find_svc(&stack, "db").mode, TransferMode::Pull);
    }

    // ===== 7. 顶层 name / 解析失败(非 YAML / 无 services)=====

    #[test]
    fn test_project_name_from_top_level_name() {
        let dir = temp_fixture_dir();
        let path = dir.join("app.yml");
        std::fs::write(&path, "name: myproj\nservices:\n  a:\n    image: alpha\n").unwrap();
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(stack.project_name, "myproj");
        // 无标签镜像按 Docker 约定补 latest 参与匹配
        let local = vec![("alpha".to_string(), "latest".to_string())];
        let dir = temp_fixture_dir();
        let path = dir.join("app.yml");
        std::fs::write(&path, "services:\n  a:\n    image: alpha\n").unwrap();
        let stack = parse_compose_file(&path, &local).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(find_svc(&stack, "a").match_state, MatchState::Exact);
    }

    #[test]
    fn test_parse_rejects_invalid_yaml() {
        let dir = temp_fixture_dir();
        let path = dir.join("broken.yml");
        std::fs::write(&path, "services: [unclosed\n").unwrap();
        let err = parse_compose_file(&path, &[]).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        assert!(err.contains("YAML"), "错误应说明不是有效 YAML: {}", err);
    }

    #[test]
    fn test_parse_rejects_missing_services() {
        let dir = temp_fixture_dir();
        let path = dir.join("nosvc.yml");
        std::fs::write(&path, "foo: bar\n").unwrap();
        let err = parse_compose_file(&path, &[]).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        assert!(err.contains("services"), "错误应指出缺少 services: {}", err);
    }

    #[test]
    fn test_parse_missing_file_errors() {
        let err = parse_compose_file(Path::new("Z:/definitely/not/exist.yml"), &[]).unwrap_err();
        assert!(err.contains("读取 compose 文件失败"), "实际: {}", err);
    }

    // ===== split_image_ref =====

    #[test]
    fn test_split_image_ref() {
        assert_eq!(split_image_ref("myapp"), ("myapp".into(), "latest".into()));
        assert_eq!(split_image_ref("myapp:v1"), ("myapp".into(), "v1".into()));
        assert_eq!(
            split_image_ref("reg.example.com:5000/app:v1"),
            ("reg.example.com:5000/app".into(), "v1".into())
        );
        assert_eq!(
            split_image_ref("reg.example.com:5000/app"),
            ("reg.example.com:5000/app".into(), "latest".into())
        );
    }

    // ===== load_env_file =====

    #[test]
    fn test_load_env_file() {
        let dir = temp_fixture_dir();
        std::fs::write(
            dir.join(".env"),
            "# 注释\n\nKEY=value\n QUOTED=\"hello world\" \nEMPTY=\nNOEQ_LINE\nexport X=1\n",
        )
        .unwrap();
        let map = load_env_file(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(map.get("KEY").map(String::as_str), Some("value"));
        assert_eq!(map.get("QUOTED").map(String::as_str), Some("hello world"));
        assert_eq!(map.get("EMPTY").map(String::as_str), Some(""));
        // 无 = 的行跳过;`export X=1` 整行按首个 = 拆分,键为 "export X"
        //(${export X} 无法引用,无副作用)
        assert!(!map.contains_key("NOEQ_LINE"));
        assert_eq!(map.len(), 4, "实际: {:?}", map);
    }

    // ===== TransferMode serde 序列化为 "Local"/"Pull" =====

    #[test]
    fn test_transfer_mode_serde() {
        assert_eq!(serde_yaml::to_string(&TransferMode::Local).unwrap().trim(), "Local");
        assert_eq!(
            serde_json::to_value(TransferMode::Pull).unwrap(),
            serde_json::Value::String("Pull".into())
        );
        let json = r#"{"service":"web","mode":"Local"}"#;
        let o: ServiceOverride = serde_json::from_str(json).unwrap();
        assert_eq!(o.mode, TransferMode::Local);
    }
}
