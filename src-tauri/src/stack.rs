//! compose 文件解析 + 本地镜像三级匹配(纯逻辑,便于单测)。
//!
//! 职责:
//! - [`parse_compose_file`]:读取 compose YAML → 顶层 `services` 逐服务解析
//!   (image / build)→ 用 compose 同目录 `.env` 只对 image 字段做变量插值
//!   → 按默认规则分类(has_build→Local;仅 image→Pull)→ 与本地镜像
//!   (`docker images` 的 repo/tag 对)做 Exact / RepoOnly / Missing 三级匹配;
//! - [`find_override_files`] + 服务级浅合并:检测 compose 同目录的 override
//!   文件并按序合并进 base(同名服务的 image/build 等键存在即以 override 为准,
//!   新服务追加;`services` 之外的顶层键不参与合并),合并结果同时决定本地
//!   解析分类与远端 `compose pull/up` 的 `-f` 文件链;
//! - [`apply_overrides`]:用项目保存的 service_overrides 覆盖默认分类。
//!
//! 解析约定:
//! - 顶层 `x-` 开头的扩展键自动忽略(只读取 `name` 与 `services`);
//! - YAML 合并键(`<<: *anchor`)显式应用(serde_yaml 不自动合并),
//!   compose 常用锚点注入公共 image/build;
//! - `build` 字段存在即可(字符串或映射都算 has_build);
//! - image 与 build 都缺失的服务仍保留在 services 中,同时记入 `errors`
//!   (部署前由前端红框阻断);
//! - 服务只有 `build:` 未写 `image:` 字段时,按 compose v2 默认镜像命名
//!   (`<项目名>-<服务名>` / `_` 变体等候选)扫描本地镜像兜底:命中自动填入
//!   该镜像并记 Exact,未命中保持 Local + Missing 并提示先构建。项目名候选
//!   来源按序:顶层 `name:` → 同目录 `origin.json` 记录的导入原始目录名 →
//!   compose 父目录名(副本目录名为 uuid 时候选仅参与扫描,提示中剔除);
//! - 镜像引用无标签时按 Docker 约定补 `latest` 参与匹配;
//! - Pull 类服务的 image 首段为私有仓库主机名([`registry_of`],Docker Hub
//!   官方别名 docker.io / index.docker.io / registry-1.docker.io 除外)时,
//!   追加 “请确认服务器已 docker login” 警告。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// 插值后的镜像引用;compose 未设 image 字段且默认命名兜底未命中时为 None
    pub image: Option<String>,
    /// compose 是否有 build 字段(字符串或映射都算)
    pub has_build: bool,
    /// 传输方式(默认分类;可被 service_overrides 覆盖)
    pub mode: TransferMode,
    pub match_state: MatchState,
    /// 本地实际存在的标签(Exact 时与 compose 相同;RepoOnly 时为本地任一标签)
    pub local_tag: Option<String>,
    /// 非阻断警告(插值未定义变量 / 标签不一致 / 本地不存在 / 未设 image 的
    /// 默认命名兜底结果 / 私有仓库需登录)
    pub warning: Option<String>,
    /// 私有仓库主机名(image 首段,见 [`registry_of`]);docker.io 官方仓库为 None
    #[serde(default)]
    pub registry: Option<String>,
}

/// compose 解析结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeStack {
    /// compose 顶层 `name`;未设置时取文件名去扩展名
    pub project_name: String,
    pub services: Vec<StackService>,
    /// 阻断性错误(如服务既无 image 也无 build);非空时前端红框禁止部署
    pub errors: Vec<String>,
    /// 参与合并的 override 文件名列表(按 [`find_override_files`] 检测顺序;
    /// 无 override 时为空;远端 pull/up 按同序追加 `-f`)
    #[serde(default)]
    pub overrides: Vec<String>,
}

/// 解析 compose 文件并做本地镜像三级匹配。
///
/// * `compose_path`:compose YAML 路径(插值用的 `.env` 与 override 文件取其同目录);
/// * `local_images`:`docker images` 的 (repository, tag) 对。
///
/// 非 YAML / 缺少 services 等结构性失败返回 `Err`(中文);
/// 单个服务的非阻断问题记入 [`ComposeStack::errors`] / `warning`。
///
/// base 解析成功后,按 [`find_override_files`] 顺序读入同目录 override 文件
/// 做服务级浅合并(同名服务 image/build 等键存在即以 override 为准,新服务
/// 追加),参与合并的文件名记入 [`ComposeStack::overrides`]。
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

    // ---- override 检测 + 服务级浅合并(多个 override 按检测顺序,后覆盖前)----
    let mut overrides: Vec<String> = Vec::new();
    if let Some(dir) = compose_path.parent() {
        for override_path in find_override_files(dir) {
            let file_name = override_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let ov_text = std::fs::read_to_string(&override_path).map_err(|e| {
                format!(
                    "读取 override 文件失败 ({}): {}",
                    override_path.display(),
                    e
                )
            })?;
            let mut ov_value: serde_yaml::Value = serde_yaml::from_str(&ov_text).map_err(|e| {
                format!(
                    "解析 override 文件失败 ({}),不是有效的 YAML: {}",
                    override_path.display(),
                    e
                )
            })?;
            ov_value.apply_merge().map_err(|e| {
                format!(
                    "解析 override 文件失败 ({}),处理 YAML 合并键(<<)失败: {}",
                    override_path.display(),
                    e
                )
            })?;
            if let Some(ov_services) = ov_value.get("services") {
                merge_override_services(&mut value, ov_services);
            }
            overrides.push(file_name);
        }
    }

    // 项目名:顶层 name(字符串且非空)优先,否则取文件名去扩展名。
    // 显式声明的 name 同时是默认镜像名兜底的最高优先级候选来源。
    let declared_name = value
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let project_name = declared_name
        .clone()
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
        let mut image = image_raw
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

        let (mode, mut match_state, mut local_tag, match_warning) =
            classify(image.as_deref(), has_build, local_images);
        if let Some(w) = match_warning {
            warnings.push(w);
        }
        // has_build 且未设 image 字段:按 compose v2 默认命名
        // (<项目名>-<服务名> / _ 变体等候选,项目名候选来源见
        // [`compose_project_name_candidates`])扫描本地镜像兜底;命中视同
        // compose 显式写了该镜像(Exact),未命中保持 Missing 并提示先构建/
        // 补 image。image 字段已存在(即使本地 Missing)不参与兜底:此时构建
        // 产物的命名就是该字段,默认命名不适用。
        if image.is_none() {
            if has_build {
                let projects =
                    compose_project_name_candidates(compose_path, declared_name.as_deref());
                let candidates = default_image_candidates(&projects, service_name);
                match scan_default_image(&candidates, local_images) {
                    Some((repo, tag)) => {
                        warnings.push(format!(
                            "未设 image 字段,已按 compose 默认命名匹配到本地镜像 {}:{};建议在 compose 中显式写 image: 固化命名",
                            repo, tag
                        ));
                        image = Some(format!("{}:{}", repo, tag));
                        match_state = MatchState::Exact;
                        local_tag = Some(tag);
                    }
                    None => {
                        // 展示用候选剔除 uuid 副本目录名派生的候选,避免误导
                        let visible = visible_candidates(&candidates, compose_path);
                        let shown = if visible.is_empty() {
                            "未能识别原始项目目录名".to_string()
                        } else {
                            visible
                                .iter()
                                .take(2)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(" / ")
                        };
                        warnings.push(format!(
                            "未设 image 字段,且未在本地找到默认命名镜像({});请先构建或在 compose 中补 image: 字段",
                            shown
                        ));
                    }
                }
            } else {
                errors.push(format!(
                    "服务「{}」未设置 image 且没有 build 配置,无法部署",
                    service_name
                ));
            }
        }

        // 私有仓库判定:image(可能被兜底填充)首段为 registry 主机名时记录
        let registry = image.as_deref().and_then(registry_of);
        // Pull 类且来自私有仓库:提醒服务器需先 docker login
        if matches!(mode, TransferMode::Pull) && registry.is_some() {
            warnings.push("私有仓库,请确认服务器已 docker login".to_string());
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
            registry,
        });
    }

    Ok(ComposeStack {
        project_name,
        services,
        errors,
        overrides,
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

/// 按 compose 默认加载顺序检测 `compose_dir` 下的 override 文件:
/// `compose.override.yaml` → `compose.override.yml` →
/// `docker-compose.override.yaml` → `docker-compose.override.yml`,
/// 存在的全部按此序返回(合并时后者覆盖前者;无则返回空)。
pub fn find_override_files(compose_dir: &Path) -> Vec<PathBuf> {
    [
        "compose.override.yaml",
        "compose.override.yml",
        "docker-compose.override.yaml",
        "docker-compose.override.yml",
    ]
    .iter()
    .map(|name| compose_dir.join(name))
    .filter(|path| path.is_file())
    .collect()
}

/// 推导 compose v2 的项目名候选(纯函数,读 `compose_path` 同目录文件)。
///
/// compose 未用 `-p` 指定项目名时,默认项目名优先级为 顶层 `name:` →
/// compose 所在目录名。本应用把 compose 复制到 `config/stacks/<uuid>/` 持久化,
/// 副本路径的父目录名是 uuid 而非用户原始目录,故候选来源按序:
/// ① 顶层 `name:`(`declared_name`,解析自 compose 文档);
/// ② 同目录 `origin.json` 记录的导入时原始父目录名(存在才取,损坏/缺失忽略);
/// ③ compose 文件父目录名(原始路径直接解析时的真实目录名,最后手段)。
/// 每个来源再派生:原样 → 全小写 → 合规化小写(小写化后剔除 [^a-z0-9_-]
/// 字符;首字符是否字母数字不再额外修补,不合规时小写原样已作为候选覆盖),
/// 全体按序去重。无任何来源时返回空列表。
pub fn compose_project_name_candidates(
    compose_path: &Path,
    declared_name: Option<&str>,
) -> Vec<String> {
    let mut bases: Vec<String> = Vec::new();
    if let Some(name) = declared_name.map(str::trim).filter(|s| !s.is_empty()) {
        bases.push(name.to_string());
    }
    if let Some(dir) = compose_path.parent() {
        if let Some(origin_dir) = load_origin_dir_name(dir) {
            bases.push(origin_dir);
        }
        if let Some(dir_name) = dir.file_name() {
            bases.push(dir_name.to_string_lossy().to_string());
        }
    }
    let mut candidates: Vec<String> = Vec::new();
    for base in bases {
        let lower = base.to_lowercase();
        let normalized: String = lower
            .chars()
            .filter(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
            .collect();
        for candidate in [base, lower, normalized] {
            if !candidate.is_empty() && !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

/// uuid 常规连字符形态判定(8-4-4-4-12 十六进制段,大小写均可)。
/// 导入副本位于 `config/stacks/<uuid>/`,uuid 目录名派生的默认镜像候选
/// 不可能对应用户的构建产物,warning 展示时需剔除以免误导。
fn is_uuid_like(s: &str) -> bool {
    let group_lens = [8, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip(group_lens)
            .all(|(p, len)| p.len() == len && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// 过滤 warning 展示用的镜像候选:剔除由 uuid 形态副本目录名派生的候选
/// (`<uuid>-<服务>` / `<uuid>_<服务>` 及其小写变体),其余原样保留。
fn visible_candidates(candidates: &[String], compose_path: &Path) -> Vec<String> {
    let uuid_base = compose_path
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| is_uuid_like(n));
    let Some(uuid_base) = uuid_base else {
        return candidates.to_vec();
    };
    let prefixes: Vec<String> = [uuid_base.clone(), uuid_base.to_lowercase()]
        .iter()
        .flat_map(|b| [format!("{}-", b), format!("{}_", b)])
        .collect();
    candidates
        .iter()
        .filter(|c| !prefixes.iter().any(|p| c.starts_with(p)))
        .cloned()
        .collect()
}

/// 由项目名候选 × compose 默认镜像命名生成默认镜像名候选(纯函数)。
///
/// 只写 `build:` 的服务,compose v2 构建产物的默认镜像名为
/// `<项目名>-<服务名>`(早期版本为 `<项目名>_<服务名>`),每个项目名候选
/// 依次拼两种分隔符,按序去重。服务名保持原样(不做大小写变换)。
pub fn default_image_candidates(project_names: &[String], service: &str) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    for project in project_names {
        for sep in ["-", "_"] {
            let candidate = format!("{}{}{}", project, sep, service);
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

/// 在本地镜像列表中按候选顺序查找默认命名的镜像(纯函数)。
///
/// 逐个候选收集 repo 完全相等的所有 (repo, tag):标签优先取 `latest`
/// (compose 构建未写标签时的默认标签),否则取字典序第一个;
/// 第一个有命中的候选即返回 `Some((repo, tag))`,全部未命中返回 `None`。
pub fn scan_default_image(
    candidates: &[String],
    local_images: &[(String, String)],
) -> Option<(String, String)> {
    for candidate in candidates {
        let mut tags: Vec<&str> = local_images
            .iter()
            .filter(|(repo, _)| repo == candidate)
            .map(|(_, tag)| tag.as_str())
            .collect();
        if tags.is_empty() {
            continue;
        }
        let tag = if tags.contains(&"latest") {
            "latest".to_string()
        } else {
            tags.sort();
            tags[0].to_string()
        };
        return Some((candidate.clone(), tag));
    }
    None
}

/// 导入来源信息(compose 同目录 `origin.json` 的内容)。
///
/// 本应用把 compose 复制到 `config/stacks/<uuid>/` 持久化,副本父目录名是
/// uuid;记录导入时的原始父目录名,供副本路径解析时推导 compose 默认
/// 项目名 / 默认镜像名兜底候选([`compose_project_name_candidates`])。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackOrigin {
    pub dir_name: String,
}

/// 读取 compose 同目录 `origin.json` 的原始目录名。
/// 文件不存在 / JSON 损坏 / 字段缺失或为空 → `None`(兜底推导静默降级)。
fn load_origin_dir_name(compose_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(compose_dir.join("origin.json")).ok()?;
    let origin: StackOrigin = serde_json::from_str(&text).ok()?;
    let name = origin.dir_name.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// 把导入来源信息写入 compose 同目录的 `origin.json`(导入流程调用)。
/// 失败由调用方 `log::warn` 告警,不阻断导入主流程(缺失时候选退化为
/// uuid 目录名,默认镜像兜底对旧导入不可用,仅影响提示文案)。
pub fn save_origin_file(compose_dir: &Path, dir_name: &str) -> Result<(), String> {
    let origin = StackOrigin {
        dir_name: dir_name.trim().to_string(),
    };
    let path = compose_dir.join("origin.json");
    let text =
        serde_json::to_string_pretty(&origin).map_err(|e| format!("序列化 origin 失败: {}", e))?;
    std::fs::write(&path, text)
        .map_err(|e| format!("写入 origin.json 失败 ({}): {}", path.display(), e))
}

/// 把 override 文件的 `services` 映射按服务级浅合并进 base 顶层文档:
///
/// - 同名服务:条目级浅合并 —— 从 base 条目映射出发,override 条目的键覆盖
///   (image/build 等存在即覆盖;条目内部结构不做深合并);
/// - override 的新服务:追加到 base `services` 末尾;
/// - `services` 之外的顶层键不参与合并(base 的 `name` 等保持不变);
/// - base 缺 `services` 键(或为 null)且 override 提供了非空 services →
///   整体插入/替换,使合并后的文档可正常解析;
/// - 结构不符(override 的 services 非映射、base 的 services 非映射)时跳过
///   合并保持 base 原样(后续解析按 base 原样处理)。
fn merge_override_services(base: &mut serde_yaml::Value, override_services: &serde_yaml::Value) {
    let Some(ov_map) = override_services.as_mapping() else {
        return; // override 的 services 不是映射,跳过合并
    };
    if ov_map.is_empty() {
        return;
    }
    let Some(base_map) = base.as_mapping_mut() else {
        return; // base 顶层不是映射(解析前置检查已排除),防御性跳过
    };
    match base_map.get_mut("services") {
        None => {
            // base 无 services 键:override 的 services 整体插入
            base_map.insert(
                serde_yaml::Value::String("services".to_string()),
                override_services.clone(),
            );
        }
        Some(base_services) => {
            if base_services.is_null() {
                *base_services = override_services.clone();
                return;
            }
            let Some(base_services_map) = base_services.as_mapping_mut() else {
                return; // base 的 services 不是映射,无法合并(后续解析照常报错)
            };
            for (key, ov_entry) in ov_map {
                match base_services_map.get_mut(key) {
                    Some(base_entry) => {
                        // 同名服务:条目级浅合并(override 键覆盖 base 条目)
                        if let (Some(entry_map), Some(ov_entry_map)) =
                            (base_entry.as_mapping_mut(), ov_entry.as_mapping())
                        {
                            for (k, v) in ov_entry_map {
                                entry_map.insert(k.clone(), v.clone());
                            }
                        } else if !ov_entry.is_null() {
                            // base 条目非映射(异常写法)而 override 条目可读 → 整体替换
                            *base_entry = ov_entry.clone();
                        }
                    }
                    None => {
                        // override 的新服务:追加到 services 末尾
                        base_services_map.insert(key.clone(), ov_entry.clone());
                    }
                }
            }
        }
    }
}

/// 分类默认规则 + 本地三级匹配(纯函数)。
///
/// 规则:has_build→Local;仅 image→Pull;Missing 且无 build→Pull+警告
/// “本地不存在,将由服务器拉取”;RepoOnly→标签不一致警告;
/// image 缺失时匹配状态记 Missing(警告与默认命名兜底匹配由调用方处理)。
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

/// 判定镜像引用的私有仓库主机名(纯函数)。
///
/// 规则与 Docker 一致:存在 `/` 时取第一个 `/` 之前的首段,该段含 `.` 或 `:`
/// 或等于 `localhost` 时视为 registry 主机名(如 `ghcr.io/x/y` → `ghcr.io`、
/// `reg:5000/a` → `reg:5000`);无 `/`(`myapp:v1` 的 `:` 是标签分隔符)或首段
/// 为普通仓库名段 → `None`(按 docker.io 处理)。
/// Docker Hub 官方别名 `docker.io` / `index.docker.io` / `registry-1.docker.io`
/// 同样返回 `None` —— Pull 类官方 Hub 镜像匿名可拉,不算私有仓库、免登录提示。
pub fn registry_of(image: &str) -> Option<String> {
    let s = image.trim();
    let (first, _) = s.split_once('/')?;
    // Docker Hub 官方仓库的完整引用形态(三种别名):不算私有仓库
    if matches!(first, "docker.io" | "index.docker.io" | "registry-1.docker.io") {
        return None;
    }
    if first.contains('.') || first.contains(':') || first == "localhost" {
        Some(first.to_string())
    } else {
        None
    }
}

/// 把镜像引用拆成 `(仓库, 标签)`;无标签时按 Docker 约定补 `latest`。
/// 标签分隔取最后一个 `/` 之后的第一个 `:`
/// (因此仓库内含端口的 `reg:5000/app:v1` 也能正确拆分)。
/// (供部署预览按 repo:tag 在本地/远端镜像列表中定位镜像 ID。)
pub fn split_image_ref(image: &str) -> (String, String) {
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
        // Pull 类 + 首段含 `.`(私有仓库主机名)→ 追加登录提示
        assert_eq!(
            svc1.registry.as_deref(),
            Some("registry.example"),
            "首段含 . 应判定为 registry"
        );
        assert_eq!(
            svc1.warning.as_deref(),
            Some("私有仓库,请确认服务器已 docker login")
        );
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

    // ===== 5. build 无 image 字段 → 默认命名兜底;无 image 无 build → errors =====

    #[test]
    fn test_build_without_image_and_no_image_no_build() {
        let dir = temp_fixture_dir();
        let path = dir.join("stack.yml");
        std::fs::write(
            &path,
            "services:\n  worker:\n    build: .\n  broken:\n    ports:\n      - \"80:80\"\n",
        )
        .unwrap();
        // 本地为空:默认命名兜底必然未命中 → 保持 Missing + 提示构建/补 image
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        // has_build 且 image 缺失、默认命名未命中 → Local + Missing + 指定警告
        let worker = find_svc(&stack, "worker");
        assert_eq!(worker.mode, TransferMode::Local);
        assert_eq!(worker.image, None);
        assert_eq!(worker.match_state, MatchState::Missing);
        let warning = worker.warning.as_deref().unwrap_or_default();
        assert!(warning.contains("未设 image 字段"), "实际: {}", warning);
        assert!(
            warning.contains("默认命名镜像"),
            "未命中应提示默认命名: {}", warning
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

    // ===== compose_project_name_candidates:常规 / 大写 / 含空格连字符 =====

    #[test]
    fn test_compose_project_name_candidates() {
        // 常规小写目录名:原样与小写重合,单候选
        assert_eq!(
            compose_project_name_candidates(Path::new("/tmp/myproj/compose.yaml"), None),
            vec!["myproj".to_string()]
        );
        // 大写目录名:原样 + 全小写
        assert_eq!(
            compose_project_name_candidates(
                Path::new("C:/work/MyApp/docker-compose.yml"),
                None
            ),
            vec!["MyApp".to_string(), "myapp".to_string()]
        );
        // 含空格与连字符的目录名:原样 + 小写 + 合规化(剔除空格等非法字符)
        assert_eq!(
            compose_project_name_candidates(Path::new("/srv/My App-1/compose.yaml"), None),
            vec![
                "My App-1".to_string(),
                "my app-1".to_string(),
                "myapp-1".to_string()
            ]
        );
        // 无父目录 → 无来源 → 空候选;有显式 name 时仍可从 name 推导
        assert!(compose_project_name_candidates(Path::new("compose.yaml"), None).is_empty());
        assert_eq!(
            compose_project_name_candidates(Path::new("compose.yaml"), Some("MyApp")),
            vec!["MyApp".to_string(), "myapp".to_string()]
        );
        // 顶层 name:优先于父目录名(去重后排在前面)
        assert_eq!(
            compose_project_name_candidates(Path::new("/tmp/myproj/compose.yaml"), Some("Alpha")),
            vec!["Alpha".to_string(), "alpha".to_string(), "myproj".to_string()]
        );
        // 同目录 origin.json 的 dir_name 优先于父目录名(副本路径场景)
        let dir = temp_fixture_dir();
        std::fs::write(
            dir.join("origin.json"),
            r#"{"dir_name": "original-proj"}"#,
        )
        .unwrap();
        let path = dir.join("docker-compose.yml");
        let parent_name = dir.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            compose_project_name_candidates(&path, None),
            vec!["original-proj".to_string(), parent_name.clone()],
            "origin.json 的 dir_name 应优先于父目录名"
        );
        // origin.json 损坏 → 静默忽略,退回父目录名
        std::fs::write(dir.join("origin.json"), "not json").unwrap();
        assert_eq!(
            compose_project_name_candidates(&path, None),
            vec![parent_name]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== is_uuid_like / visible_candidates:uuid 副本目录名候选过滤 =====

    #[test]
    fn test_is_uuid_like_and_visible_candidates() {
        // 标准 uuid(v4 连字符形态,大小写均可)判定
        assert!(is_uuid_like("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_uuid_like("550E8400-E29B-41D4-A716-446655440000"));
        assert!(!is_uuid_like("myproj"));
        assert!(!is_uuid_like("550e8400-e29b-41d4-a716-4466554400")); // 末段 11 位
        assert!(!is_uuid_like("550e8400e29b41d4a716446655440000")); // 无连字符

        // uuid 形态父目录:派生候选(-/_ 及小写变体)全部被剔除
        let uuid_dir = uuid::Uuid::new_v4().to_string();
        let path = Path::new("/tmp").join(&uuid_dir).join("docker-compose.yml");
        let candidates = vec![
            format!("{}-web", uuid_dir),
            format!("{}_web", uuid_dir),
            format!("{}-web", uuid_dir.to_lowercase()),
            "myproj-web".to_string(),
            "myproj_web".to_string(),
        ];
        let visible = visible_candidates(&candidates, &path);
        assert_eq!(
            visible,
            vec!["myproj-web".to_string(), "myproj_web".to_string()],
            "uuid 派生候选应被剔除: {:?}",
            visible
        );
        // 非 uuid 父目录:候选原样保留
        assert_eq!(
            visible_candidates(&candidates, Path::new("/tmp/myproj/docker-compose.yml")),
            candidates
        );
    }

    // ===== default_image_candidates:序与去重 =====

    #[test]
    fn test_default_image_candidates() {
        // 每个项目名 × [-, _],按序去重
        let projects = vec!["MyApp".to_string(), "myapp".to_string()];
        assert_eq!(
            default_image_candidates(&projects, "web"),
            vec![
                "MyApp-web".to_string(),
                "MyApp_web".to_string(),
                "myapp-web".to_string(),
                "myapp_web".to_string(),
            ]
        );
        // 重复项目名去重
        let dup = vec!["myproj".to_string(), "myproj".to_string()];
        assert_eq!(
            default_image_candidates(&dup, "api"),
            vec!["myproj-api".to_string(), "myproj_api".to_string()]
        );
        // 空项目名列表 → 空
        assert!(default_image_candidates(&[], "web").is_empty());
    }

    // ===== scan_default_image:latest 优先 / 多 tag 排序取首 / 未命中 =====

    #[test]
    fn test_scan_default_image() {
        let candidates = vec!["myproj-web".to_string(), "myproj_web".to_string()];
        let mk = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(r, t)| (r.to_string(), t.to_string()))
                .collect()
        };
        // 多 tag:latest 优先
        assert_eq!(
            scan_default_image(
                &candidates,
                &mk(&[("myproj-web", "v1"), ("myproj-web", "latest")])
            ),
            Some(("myproj-web".to_string(), "latest".to_string()))
        );
        // 无 latest:多 tag 取字典序第一个("v1" < "v10" < "v2")
        assert_eq!(
            scan_default_image(
                &candidates,
                &mk(&[("myproj-web", "v2"), ("myproj-web", "v10"), ("myproj-web", "v1")])
            ),
            Some(("myproj-web".to_string(), "v1".to_string()))
        );
        // 首候选未命中,次候选(下划线变体)命中
        assert_eq!(
            scan_default_image(&candidates, &mk(&[("myproj_web", "v2")])),
            Some(("myproj_web".to_string(), "v2".to_string()))
        );
        // 全部未命中 / 本地为空 → None
        assert_eq!(scan_default_image(&candidates, &mk(&[("other", "v1")])), None);
        assert_eq!(scan_default_image(&candidates, &[]), None);
    }

    /// 模拟导入副本结构:临时 stacks 根下 `<uuid>/docker-compose.yml` +
    /// 可选 `origin.json`(dir_name)。返回 (compose 副本路径, 临时根目录)。
    fn copy_fixture(origin_dir_name: Option<&str>, compose_yaml: &str) -> (PathBuf, PathBuf) {
        let root = temp_fixture_dir();
        let copy_dir = root.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&copy_dir).unwrap();
        if let Some(origin) = origin_dir_name {
            std::fs::write(
                copy_dir.join("origin.json"),
                format!(r#"{{"dir_name": "{origin}"}}"#),
            )
            .unwrap();
        }
        let path = copy_dir.join("docker-compose.yml");
        std::fs::write(&path, compose_yaml).unwrap();
        (path, root)
    }

    // ===== 兜底集成(副本结构):origin.json 记录原目录名 → 默认命名命中 =====

    #[test]
    fn test_fallback_matches_dash_variant() {
        // 副本父目录名是 uuid,真实目录名 myproj 来自 origin.json
        let (path, root) = copy_fixture(
            Some("myproj"),
            "services:\n  web:\n    build: ./web\n",
        );
        let local = vec![("myproj-web".to_string(), "latest".to_string())];
        let stack = parse_compose_file(&path, &local).unwrap();
        std::fs::remove_dir_all(&root).ok();

        let web = find_svc(&stack, "web");
        assert_eq!(web.mode, TransferMode::Local);
        assert_eq!(
            web.image.as_deref(),
            Some("myproj-web:latest"),
            "应按 origin 记录的原目录名自动填入命中的镜像"
        );
        assert_eq!(web.match_state, MatchState::Exact);
        assert_eq!(web.local_tag.as_deref(), Some("latest"));
        let warning = web.warning.as_deref().unwrap_or_default();
        assert!(warning.contains("默认命名"), "应含默认命名提示: {}", warning);
        assert!(
            warning.contains("myproj-web:latest"),
            "应含命中镜像引用: {}", warning
        );
        assert!(warning.contains("image:"), "应建议显式固化命名: {}", warning);
    }

    // ===== 兜底集成(副本结构):下划线变体命中 =====

    #[test]
    fn test_fallback_matches_underscore_variant() {
        let (path, root) = copy_fixture(
            Some("myproj"),
            "services:\n  web:\n    build: ./web\n",
        );
        let local = vec![("myproj_web".to_string(), "v2".to_string())];
        let stack = parse_compose_file(&path, &local).unwrap();
        std::fs::remove_dir_all(&root).ok();

        let web = find_svc(&stack, "web");
        assert_eq!(web.image.as_deref(), Some("myproj_web:v2"));
        assert_eq!(web.match_state, MatchState::Exact);
        assert_eq!(web.local_tag.as_deref(), Some("v2"));
    }

    // ===== 兜底集成(副本结构):全部未命中 → Local+Missing,候选不含 uuid =====

    #[test]
    fn test_fallback_miss_keeps_missing_with_hint() {
        let (path, root) = copy_fixture(
            Some("myproj"),
            "services:\n  web:\n    build: ./web\n",
        );
        let uuid_dir = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&root).ok();

        let web = find_svc(&stack, "web");
        assert_eq!(web.mode, TransferMode::Local);
        assert_eq!(web.image, None);
        assert_eq!(web.match_state, MatchState::Missing);
        let warning = web.warning.as_deref().unwrap_or_default();
        assert!(
            warning.contains("默认命名镜像"),
            "未命中应提示默认命名: {}", warning
        );
        // 候选展示(- 与 _ 变体),且不出现 uuid 目录名派生的候选
        assert!(warning.contains("myproj-web"), "实际: {}", warning);
        assert!(warning.contains("myproj_web"), "实际: {}", warning);
        assert!(
            !warning.contains(&uuid_dir),
            "warning 不应展示 uuid 副本目录名: {}", warning
        );
    }

    // ===== 兜底集成:uuid 副本目录名且无 origin.json → 展示剔除 uuid =====

    #[test]
    fn test_fallback_miss_filters_uuid_dir_name() {
        // 旧版本导入的栈没有 origin.json:uuid 候选只参与扫描,不进 warning
        let (path, root) = copy_fixture(None, "services:\n  web:\n    build: ./web\n");
        let uuid_dir = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&root).ok();

        let warning = find_svc(&stack, "web").warning.as_deref().unwrap_or_default();
        assert!(
            warning.contains("默认命名镜像"),
            "未命中应提示默认命名: {}", warning
        );
        assert!(
            !warning.contains(&uuid_dir),
            "warning 不应展示 uuid 副本目录名: {}", warning
        );
        assert!(
            warning.contains("未能识别原始项目目录名"),
            "候选全被剔除时应如实说明: {}", warning
        );
    }

    // ===== 兜底集成:顶层 name: 参与候选(无 origin.json)=====

    #[test]
    fn test_fallback_uses_top_level_name() {
        let (path, root) = copy_fixture(
            None,
            "name: myproj\nservices:\n  web:\n    build: ./web\n",
        );
        let local = vec![("myproj-web".to_string(), "latest".to_string())];
        let stack = parse_compose_file(&path, &local).unwrap();
        std::fs::remove_dir_all(&root).ok();

        let web = find_svc(&stack, "web");
        assert_eq!(
            web.image.as_deref(),
            Some("myproj-web:latest"),
            "顶层 name: 应作为候选来源(优先于 uuid 目录名)"
        );
        assert_eq!(web.match_state, MatchState::Exact);
    }

    // ===== 兜底集成:顶层 name: 优先于 origin.json 目录名 =====

    #[test]
    fn test_fallback_declared_name_wins_over_origin() {
        let (path, root) = copy_fixture(
            Some("origproj"),
            "name: myproj\nservices:\n  web:\n    build: ./web\n",
        );
        let local = vec![
            ("myproj-web".to_string(), "v1".to_string()),
            ("origproj-web".to_string(), "v9".to_string()),
        ];
        let stack = parse_compose_file(&path, &local).unwrap();
        std::fs::remove_dir_all(&root).ok();

        // compose v2 项目名优先级:name: > 目录名,两者都在本地时 name: 命中在前
        let web = find_svc(&stack, "web");
        assert_eq!(web.image.as_deref(), Some("myproj-web:v1"));
        assert_eq!(web.match_state, MatchState::Exact);
    }

    // ===== 兜底集成:原始路径直接解析(无 origin.json)回退父目录名 =====

    #[test]
    fn test_fallback_no_origin_falls_back_to_parent_dir() {
        // 非副本路径(preview_compose / 导入前预览场景):父目录名即真实目录名
        let root = temp_fixture_dir();
        let proj = root.join("myproj");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join("compose.yaml");
        std::fs::write(&path, "services:\n  web:\n    build: ./web\n").unwrap();
        let local = vec![("myproj-web".to_string(), "latest".to_string())];
        let stack = parse_compose_file(&path, &local).unwrap();
        std::fs::remove_dir_all(&root).ok();

        let web = find_svc(&stack, "web");
        assert_eq!(web.image.as_deref(), Some("myproj-web:latest"));
        assert_eq!(web.match_state, MatchState::Exact);
    }

    // ===== image 字段存在但本地 Missing → 不参与兜底 =====

    #[test]
    fn test_image_field_present_missing_not_fallback_scanned() {
        // image 字段已存在:构建命名就是该字段,默认命名兜底不适用
        // (即便 origin.json 提供的候选能命中也不得覆盖)
        let (path, root) = copy_fixture(
            Some("myproj"),
            "services:\n  web:\n    build: ./web\n    image: custom:1\n",
        );
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&root).ok();

        let web = find_svc(&stack, "web");
        assert_eq!(
            web.image.as_deref(),
            Some("custom:1"),
            "image 不应被默认命名覆盖"
        );
        assert_eq!(web.match_state, MatchState::Missing);
        assert_eq!(
            web.warning, None,
            "Local 类 image 缺失不告警,更不应出现默认命名兜底提示"
        );
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

    // ===== find_override_files:按序检测、缺失跳过 =====

    #[test]
    fn test_find_override_files_order_and_missing() {
        // 四个变体全部存在 → 按固定顺序返回(合并时后者覆盖前者)
        let dir = temp_fixture_dir();
        for name in [
            "docker-compose.override.yml",
            "compose.override.yaml",
            "compose.override.yml",
            "docker-compose.override.yaml",
        ] {
            std::fs::write(dir.join(name), "services: {}").unwrap();
        }
        let found = find_override_files(&dir);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "compose.override.yaml".to_string(),
                "compose.override.yml".to_string(),
                "docker-compose.override.yaml".to_string(),
                "docker-compose.override.yml".to_string(),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();

        // 只有部分存在 → 仅返回存在的,顺序保持
        let dir = temp_fixture_dir();
        std::fs::write(dir.join("docker-compose.override.yaml"), "services: {}").unwrap();
        std::fs::write(dir.join("compose.override.yml"), "services: {}").unwrap();
        let found = find_override_files(&dir);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "compose.override.yml".to_string(),
                "docker-compose.override.yaml".to_string(),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();

        // 目录不存在 / 无 override → 空
        assert!(find_override_files(Path::new("Z:/definitely/not/exist")).is_empty());
        let dir = temp_fixture_dir();
        std::fs::write(dir.join("other.yaml"), "services: {}").unwrap();
        assert!(find_override_files(&dir).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== override 合并:image/build 覆盖、新服务追加、非服务键不深合并 =====

    #[test]
    fn test_parse_merges_override_image_and_build() {
        let dir = temp_fixture_dir();
        let path = dir.join("docker-compose.yml");
        std::fs::write(
            &path,
            "services:\n  web:\n    build: ./web\n    image: myapp:1\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("compose.override.yaml"),
            "services:\n  web:\n    image: myapp:2\n  cache:\n    image: redis:7\n",
        )
        .unwrap();
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        // 参与合并的 override 文件名被记录
        assert_eq!(stack.overrides, vec!["compose.override.yaml".to_string()]);
        // 同名服务:image 以 override 为准;base 的 build 保留(has_build → Local)
        let web = find_svc(&stack, "web");
        assert_eq!(web.image.as_deref(), Some("myapp:2"));
        assert!(web.has_build, "base 的 build 应保留");
        assert_eq!(web.mode, TransferMode::Local);
        // override 里的新服务照常追加
        let cache = find_svc(&stack, "cache");
        assert_eq!(cache.image.as_deref(), Some("redis:7"));
        assert_eq!(cache.mode, TransferMode::Pull);
        // 未在 override 出现的服务保持 base 原样
        assert_eq!(find_svc(&stack, "db").image.as_deref(), Some("postgres:16"));
    }

    #[test]
    fn test_parse_override_build_only_makes_service_local() {
        // override 只补 build:base 仅 image(Pull)→ 合并后 has_build(Local),
        // base 的 image 保留
        let dir = temp_fixture_dir();
        let path = dir.join("docker-compose.yml");
        std::fs::write(&path, "services:\n  app:\n    image: myapp:1\n").unwrap();
        std::fs::write(
            dir.join("docker-compose.override.yml"),
            "services:\n  app:\n    build: ./app\n",
        )
        .unwrap();
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        let app = find_svc(&stack, "app");
        assert!(app.has_build);
        assert_eq!(app.image.as_deref(), Some("myapp:1"), "base 的 image 应保留");
        assert_eq!(app.mode, TransferMode::Local);
        assert_eq!(
            stack.overrides,
            vec!["docker-compose.override.yml".to_string()]
        );
    }

    #[test]
    fn test_parse_override_non_service_keys_not_merged() {
        // 顶层 name 等非 services 键不参与合并:base 的 name 保持不变
        let dir = temp_fixture_dir();
        let path = dir.join("docker-compose.yml");
        std::fs::write(&path, "name: base-proj\nservices:\n  web:\n    image: a:1\n").unwrap();
        std::fs::write(
            dir.join("compose.override.yaml"),
            "name: override-proj\nvolumes:\n  data: {}\nservices:\n  web:\n    image: a:2\n",
        )
        .unwrap();
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(stack.project_name, "base-proj", "服务级之外的键不做合并");
        assert_eq!(find_svc(&stack, "web").image.as_deref(), Some("a:2"));
    }

    #[test]
    fn test_parse_multiple_overrides_later_wins() {
        // 多个 override 按检测顺序合并:后合并者覆盖先合并者
        let dir = temp_fixture_dir();
        let path = dir.join("docker-compose.yml");
        std::fs::write(&path, "services:\n  web:\n    image: a:1\n").unwrap();
        std::fs::write(dir.join("compose.override.yaml"), "services:\n  web:\n    image: a:2\n").unwrap();
        std::fs::write(dir.join("compose.override.yml"), "services:\n  web:\n    image: a:3\n").unwrap();
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            stack.overrides,
            vec![
                "compose.override.yaml".to_string(),
                "compose.override.yml".to_string(),
            ]
        );
        assert_eq!(find_svc(&stack, "web").image.as_deref(), Some("a:3"));
    }

    #[test]
    fn test_parse_without_override_records_empty_and_invalid_override_errors() {
        // 无 override → overrides 为空,解析不受影响
        let dir = temp_fixture_dir();
        let path = dir.join("docker-compose.yml");
        std::fs::write(&path, "services:\n  web:\n    image: a:1\n").unwrap();
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(stack.overrides.is_empty());

        // override 不是有效 YAML → 结构性报错(远端 compose -f 同样无法加载)
        let dir = temp_fixture_dir();
        let path = dir.join("docker-compose.yml");
        std::fs::write(&path, "services:\n  web:\n    image: a:1\n").unwrap();
        std::fs::write(dir.join("compose.override.yaml"), "services: [unclosed\n").unwrap();
        let err = parse_compose_file(&path, &[]).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        assert!(err.contains("override"), "错误应指向 override 文件: {}", err);
        assert!(err.contains("YAML"), "实际: {}", err);
    }

    // ===== registry_of 私有仓库判定 =====

    #[test]
    fn test_registry_of() {
        // docker.io(无斜杠时 `:` 是标签分隔;普通仓库名段)→ None
        assert_eq!(registry_of("myapp"), None);
        assert_eq!(registry_of("myapp:v1"), None);
        assert_eq!(registry_of("library/nginx:1.25"), None);
        assert_eq!(registry_of("user/app"), None);
        // Docker Hub 官方别名的完整引用形态 → None(官方 Hub 镜像免登录提示)
        assert_eq!(registry_of("docker.io/library/nginx:1.25"), None);
        assert_eq!(registry_of("index.docker.io/user/app:1"), None);
        assert_eq!(registry_of("registry-1.docker.io/user/app:1"), None);
        // 首段含 . / : / localhost → registry 主机名
        assert_eq!(registry_of("ghcr.io/x/y:v1"), Some("ghcr.io".to_string()));
        assert_eq!(registry_of("reg:5000/a"), Some("reg:5000".to_string()));
        assert_eq!(
            registry_of("reg.example.com:5000/app:v1"),
            Some("reg.example.com:5000".to_string())
        );
        assert_eq!(registry_of("localhost/app"), Some("localhost".to_string()));
    }

    // ===== Pull 类官方 Hub 镜像(docker.io 别名)不弹私有仓库登录提示 =====

    #[test]
    fn test_docker_hub_official_pull_no_private_warning() {
        let dir = temp_fixture_dir();
        let path = dir.join("stack.yml");
        std::fs::write(
            &path,
            "services:\n  db:\n    image: docker.io/library/postgres:16\n  cache:\n    image: index.docker.io/redis:7\n",
        )
        .unwrap();
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        for name in ["db", "cache"] {
            let svc = stack
                .services
                .iter()
                .find(|s| s.service == name)
                .unwrap_or_else(|| panic!("服务 {} 不在解析结果中", name));
            assert_eq!(svc.mode, TransferMode::Pull);
            assert_eq!(
                svc.registry, None,
                "官方 Hub 镜像不应判定为私有仓库: {}",
                name
            );
            assert_eq!(
                svc.warning.as_deref(),
                Some("本地不存在,将由服务器拉取"),
                "不应追加私有仓库登录提示: {}",
                name
            );
        }
    }

    // ===== 私有仓库 Pull 类服务的 warning 与 registry 字段 =====

    #[test]
    fn test_private_registry_pull_warning() {
        let dir = temp_fixture_dir();
        let path = dir.join("stack.yml");
        std::fs::write(
            &path,
            "services:\n  priv:\n    image: ghcr.io/x/y:1\n  pub:\n    image: nginx:1.25\n  local:\n    build: .\n    image: ghcr.io/x/z:1\n",
        )
        .unwrap();
        let stack = parse_compose_file(&path, &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        // Pull 类 + 私有仓库 → 追加登录提示(与既有警告分号拼接)
        let priv_svc = find_svc(&stack, "priv");
        assert_eq!(priv_svc.registry.as_deref(), Some("ghcr.io"));
        let warning = priv_svc.warning.as_deref().unwrap_or_default();
        assert!(
            warning.contains("私有仓库,请确认服务器已 docker login"),
            "应含登录提示: {}", warning
        );
        assert!(warning.contains("本地不存在,将由服务器拉取"), "既有警告应保留: {}", warning);
        assert!(warning.contains("; "), "警告应以分号拼接: {}", warning);
        // docker.io 公共镜像:registry 为 None,不追加登录提示
        let pub_svc = find_svc(&stack, "pub");
        assert_eq!(pub_svc.registry, None);
        assert_eq!(pub_svc.warning.as_deref(), Some("本地不存在,将由服务器拉取"));
        // 本地传输(Local)不告警,但 registry 字段仍记录
        let local_svc = find_svc(&stack, "local");
        assert_eq!(local_svc.registry.as_deref(), Some("ghcr.io"));
        assert!(local_svc.warning.is_none());
    }

    // ===== 新增字段 serde 默认兼容(旧 JSON 缺字段)=====

    #[test]
    fn test_new_fields_serde_default() {
        // 旧 JSON 无 registry / overrides 字段 → 反序列化为 None / 空 Vec(兼容)
        let svc_json = r#"{"service":"web","image":"a:1","has_build":false,"mode":"Pull","match_state":"Missing","local_tag":null,"warning":null}"#;
        let svc: StackService = serde_json::from_str(svc_json).unwrap();
        assert_eq!(svc.registry, None);
        let stack_json = r#"{"project_name":"p","services":[],"errors":[]}"#;
        let stack: ComposeStack = serde_json::from_str(stack_json).unwrap();
        assert!(stack.overrides.is_empty());
        // 新字段正常反序列化
        let stack_json2 = r#"{"project_name":"p","services":[],"errors":[],"overrides":["compose.override.yaml"]}"#;
        let stack2: ComposeStack = serde_json::from_str(stack_json2).unwrap();
        assert_eq!(stack2.overrides, vec!["compose.override.yaml".to_string()]);
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
