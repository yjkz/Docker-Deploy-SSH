// // 项目源更新 + compose 解析(阶段三/第四批)

use super::*;

/// 导入 compose 文件:校验文件存在且可解析后,把 compose(连同同目录 `.env`
/// 与 override 文件,若存在)复制到 `config/stacks/<uuid>/docker-compose.yml`
/// 持久化,同时写入 `origin.json` 记录导入时的原始父目录名(供副本路径解析
/// 推导 compose 默认镜像名兜底候选,写入失败仅告警不阻断),以解析出的默认
/// 传输分类创建新项目(名称为用户自命名,compose_file 指向副本),写回配置
/// 并返回完整 ProjectConfig。
/// (解析时同目录 override 文件已按 [`crate::stack::find_override_files`] 顺序
/// 做服务级浅合并,分类与镜像基于合并结果。)
#[tauri::command]
pub fn import_compose(source_path: String, name: String) -> Result<ProjectConfig, String> {
    let source = PathBuf::from(&source_path);
    if !source.is_file() {
        return Err(format!("compose 文件不存在:{}", source_path));
    }
    // 先解析校验并取默认分类(解析失败不落盘、不改配置;导入阶段不做本地匹配)
    let stack = parse_compose_file(&source, &[])?;
    let name = if name.trim().is_empty() {
        // 未命名时回退为 compose 的项目名(顶层 name 或文件名去扩展名)
        stack.project_name.clone()
    } else {
        name.trim().to_string()
    };

    let id = uuid::Uuid::new_v4().to_string();
    let dest_dir = crate::config::config_dir().join("stacks").join(&id);
    // 复制口径与「从源更新」共用(compose + .env + override 同名副本)
    let dest = copy_compose_bundle(&source, &dest_dir)?;

    // 记录导入来源的原始 compose 父目录名(origin.json):副本父目录是 uuid,
    // 后续解析推导 compose 默认镜像名兜底候选(<原目录名>-<服务名>)需要它。
    // 失败仅告警不阻断导入(缺失时候选退化为 uuid 目录名,兜底扫描不可用)。
    if let Some(dir_name) = source.parent().and_then(Path::file_name) {
        if let Err(e) = crate::stack::save_origin_file(&dest_dir, &dir_name.to_string_lossy()) {
            log::warn!("导入栈「{}」记录来源目录名失败: {}", name, e);
        }
    }

    let project = ProjectConfig {
        id,
        name,
        image_filter: String::new(),
        compose_file: dest.to_string_lossy().to_string(),
        file_mappings: Vec::new(),
        service_overrides: stack
            .services
            .iter()
            .map(|s| ServiceOverride {
                service: s.service.clone(),
                mode: s.mode.clone(),
            })
            .collect(),
        health_wait_secs: 0,
        pre_deploy_cmd: None,
        post_deploy_cmd: None,
        notify_webhook: None,
        source_compose_path: Some(source.to_string_lossy().to_string()),
        source_hash: source_content_hash(&source).ok(),
        // 项目级部署目录留空 = 沿用服务器目录(保持导入后即可部署的旧行为);
        // 需要多项目分目录时在项目表单里填一次
        remote_dir: None,
        default_server_id: None,
        // 归档保留数留空 = 默认 5 个(与历史行为一致)
        release_keep: None,
    };
    let mut cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    cfg.projects.push(project.clone());
    save_config(&cfg).map_err(|e| format!("保存配置失败: {}", e))?;
    Ok(project)
}

// ===== 项目「从源更新」(第三批:源 compose 变更检测与副本同步)=====

/// 把源 compose 及其同目录 `.env` / override 文件复制到项目的副本目录。
/// 返回副本 compose 的路径(调用方据此更新 `compose_file`)。
///
/// 为什么把复制独立成函数:导入([`import_compose`])与「从源更新」
/// ([`update_project_from_source`])必须用完全相同的复制口径,否则更新后
/// 解析结果会与导入时不一致。
fn copy_compose_bundle(source: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("创建栈目录失败 ({}): {}", dest_dir.display(), e))?;
    let dest = dest_dir.join("docker-compose.yml");
    std::fs::copy(source, &dest).map_err(|e| {
        format!(
            "复制 compose 文件失败 ({} -> {}): {}",
            source.display(),
            dest.display(),
            e
        )
    })?;
    if let Some(parent) = source.parent() {
        let source_env = parent.join(".env");
        if source_env.is_file() {
            let dest_env = dest_dir.join(".env");
            std::fs::copy(&source_env, &dest_env)
                .map_err(|e| format!("复制 .env 文件失败 ({}): {}", source_env.display(), e))?;
        }
        // override 文件按同名 basename 复制,保持远端 -f 文件链与解析合并一致
        for ov_path in find_override_files(parent) {
            let Some(name) = ov_path.file_name() else {
                continue;
            };
            let dest_ov = dest_dir.join(name);
            std::fs::copy(&ov_path, &dest_ov).map_err(|e| {
                format!(
                    "复制 override 文件失败 ({} -> {}): {}",
                    ov_path.display(),
                    dest_ov.display(),
                    e
                )
            })?;
        }
    }
    Ok(dest)
}

/// 源 compose 内容哈希:对 compose 本体 + 同目录 `.env` + 各 override 文件
/// 的**内容**（按固定顺序拼接)取 sha256 十六进制。
///
/// 只要其中任一份内容变化,哈希即变化 —— 用于判断「源是否已更新」。
/// 文件不可读时返回 Err(调用方按"无法比对"降级,不误判为已变更)。
pub fn source_content_hash(source: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut push_file = |path: &Path| -> Result<(), String> {
        let bytes = std::fs::read(path)
            .map_err(|e| format!("读取源文件失败 ({}): {}", path.display(), e))?;
        // 文件名 + 长度 + 内容一起入哈希,避免"交换两份文件内容"这类碰撞
        hasher.update(path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default().as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
        Ok(())
    };
    push_file(source)?;
    if let Some(parent) = source.parent() {
        let env = parent.join(".env");
        if env.is_file() {
            push_file(&env)?;
        }
        for ov in find_override_files(parent) {
            push_file(&ov)?;
        }
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        hex.push_str(&format!("{:02x}", b));
    }
    Ok(hex)
}

/// 单个项目的源更新比对结果(前端据此渲染徽章/提示)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSourceStatus {
    pub project_id: String,
    pub project_name: String,
    /// 源文件路径(未绑定为空)
    pub source_path: String,
    /// `unchanged` / `changed` / `missing` / `unbound` / `unreadable`
    pub state: String,
    /// 面向用户的说明
    pub detail: String,
    /// 该项目是否为「导入项目」(compose 副本在 `config/stacks/` 下)。
    /// 导入项目应绑定源文件;手工项目(远端相对路径)天然无源。
    pub imported: bool,
    /// 是否已绑定源文件(等价于 `source_path` 非空),前端据此决定按钮文案
    pub bound: bool,
}

/// 判断项目是否为导入项目:compose 副本位于 `config/stacks/<uuid>/` 下。
///
/// 导入项目(`import_compose` 产物)的 `compose_file` 指向应用配置目录内的
/// 副本;手工项目的 `compose_file` 是远端相对路径(如 `docker-compose.yml`)。
/// 第三批之前导入的项目没有 `source_compose_path`,需要据此提示用户补绑源。
fn is_imported_project(p: &ProjectConfig) -> bool {
    let path = Path::new(&p.compose_file);
    if !path.is_absolute() {
        return false;
    }
    let stacks = crate::config::config_dir().join("stacks");
    // 规范化比较失败时退化为字符串前缀匹配(Windows 大小写不敏感)
    match (path.canonicalize(), stacks.canonicalize()) {
        (Ok(p), Ok(s)) => p.starts_with(&s),
        _ => {
            let p = p.compose_file.replace('\\', "/").to_lowercase();
            let s = stacks.to_string_lossy().replace('\\', "/").to_lowercase();
            p.starts_with(&s)
        }
    }
}

/// 检查所有项目的源 compose 是否已变更(只读,不改配置)。
///
/// 导入项目若尚未绑定源(`source_compose_path` 为空)报 `unbound`,
/// 前端提示「选择源文件」补绑;手工项目本就无源,同样报 `unbound`
/// 但 `imported=false`,前端不提示(避免噪音)。
#[tauri::command]
pub fn check_project_sources() -> Result<Vec<ProjectSourceStatus>, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    Ok(cfg.projects.iter().map(project_source_status).collect())
}

/// 计算单个项目的源状态(纯读)。
pub(crate) fn project_source_status(p: &ProjectConfig) -> ProjectSourceStatus {
    let imported = is_imported_project(p);
    let bound = p
        .source_compose_path
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let base = |state: &str, detail: String| ProjectSourceStatus {
        project_id: p.id.clone(),
        project_name: p.name.clone(),
        source_path: p.source_compose_path.clone().unwrap_or_default(),
        state: state.to_string(),
        detail,
        imported,
        bound,
    };
    if !bound {
        return base(
            "unbound",
            if imported {
                "导入项目尚未绑定源文件,点击「绑定源」选择原 compose 即可启用变更比对".to_string()
            } else {
                "手工项目(compose 为远端路径),无源文件可比对".to_string()
            },
        );
    }
    let path_str = p.source_compose_path.as_deref().unwrap_or_default();
    let path = PathBuf::from(path_str);
    if !path.is_file() {
        return base("missing", format!("源文件已不存在:{}", path_str));
    }
    // 已绑定但无哈希(手工改过配置):用当前内容补算基准,按"未变更"处理并
    // 由调用方落盘 —— 用户已明确绑定,不该再报 unknown 让他无从下手。
    let saved = match p.source_hash.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(h) => h.to_string(),
        None => match source_content_hash(&path) {
            Ok(h) => h,
            Err(e) => return base("unreadable", format!("源不可读:{}", e)),
        },
    };
    match source_content_hash(&path) {
        Ok(now) if now == saved => base("unchanged", "源未变更".to_string()),
        Ok(_) => base("changed", "源 compose 已变更,可更新".to_string()),
        Err(e) => base("unreadable", format!("源不可读:{}", e)),
    }
}

/// 比对「源 compose 目录」与「配置内副本目录」的**内容**是否不同(纯内容,
/// 忽略 compose 文件名):用于判断本次更新是否真的带来改动。
///
/// 为什么不能直接用 [`source_content_hash`]:该哈希把文件名也纳入计算,而
/// 副本文件名恒为 `docker-compose.yml` —— 源文件若叫 `compose.yml` 或
/// `docker-compose.yaml`,内容完全相同也会被判为"已变更"(假阳性)。
///
/// 规则:compose 本体只比字节;`.env` 与各 override 文件按同名比较
/// (override 的增删/改名属于真实变更,需计入)。副本缺失 → `true`(需同步)。
pub(crate) fn bundle_content_changed(source: &Path, dest: &Path) -> Result<bool, String> {
    let read = |p: &Path| -> Result<Option<Vec<u8>>, String> {
        match std::fs::read(p) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("读取文件失败 ({}): {}", p.display(), e)),
        }
    };
    // compose 本体(源必存在,调用方已校验;副本可能缺失 → None)
    let src_bytes = read(source)?.unwrap_or_default();
    let dst_bytes = read(dest)?;
    if dst_bytes.as_deref() != Some(src_bytes.as_slice()) {
        return Ok(true);
    }
    let src_dir = source.parent();
    let dst_dir = dest.parent();
    let (Some(src_dir), Some(dst_dir)) = (src_dir, dst_dir) else {
        return Ok(false);
    };
    // .env 同名比较(一边有一边无 = 变更)
    let env_src = read(&src_dir.join(".env"))?;
    let env_dst = read(&dst_dir.join(".env"))?;
    if env_src != env_dst {
        return Ok(true);
    }
    // override 文件:源侧检测到的名单与内容逐个比对
    let src_overrides = find_override_files(src_dir);
    for ov in &src_overrides {
        let Some(name) = ov.file_name() else { continue };
        let a = read(ov)?;
        let b = read(&dst_dir.join(name))?;
        if a != b {
            return Ok(true);
        }
    }
    // 副本侧存在而源侧已无的 override(被删除)→ 也算变更
    for ov in find_override_files(dst_dir) {
        let Some(name) = ov.file_name() else { continue };
        if !src_overrides.iter().any(|s| s.file_name() == Some(name)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 为项目绑定(或改绑)源 compose 文件,并以当前内容建立比对基准。
///
/// 用于第三批之前导入的项目(它们的 `source_compose_path` 为空,无法参与
/// 变更比对)。绑定后不立刻重拷副本 —— 只记录基准,后续由
/// [`update_project_from_source`] 同步内容。
#[tauri::command]
pub fn bind_project_source(
    project_id: String,
    source_path: String,
) -> Result<ProjectSourceStatus, String> {
    let path_str = source_path.trim().to_string();
    let path = PathBuf::from(&path_str);
    if !path.is_file() {
        return Err(format!("源 compose 文件不存在:{}", path_str));
    }
    // 先解析校验:不是有效 compose 就拒绝绑定(避免绑错文件后无法更新)
    parse_compose_file(&path, &[])?;
    let hash = source_content_hash(&path)?;

    let mut cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let idx = cfg
        .projects
        .iter()
        .position(|p| p.id == project_id)
        .ok_or_else(|| format!("项目不存在:{}", project_id))?;
    {
        let p = &mut cfg.projects[idx];
        p.source_compose_path = Some(path_str);
        p.source_hash = Some(hash);
    }
    let updated = cfg.projects[idx].clone();
    save_config(&cfg).map_err(|e| format!("保存配置失败: {}", e))?;
    Ok(project_source_status(&updated))
}

/// 从源更新项目:重拷 compose/.env/override → 重解析 → 合并保留 service_overrides。
///
/// 保留策略:
/// - 仍在的服务沿用用户已保存的分类(`service_overrides`);
/// - 新增服务取解析出的默认分类;
/// - 源里已消失的服务丢弃其分类。
/// 其余字段(名称/过滤词/映射/钩子/健康检查/通知)一律不动 —— 更新只同步
/// compose 本体,不覆盖用户在应用内的配置。
/// 更新前把旧副本另存为 `docker-compose.yml.bak`(出错可人工回退)。
///
/// **返回值语义**:`changed` = 本次确实同步了新内容(源与更新前的副本不同);
/// `unchanged` = 源与更新前的副本一致(仅刷新基准,无实际改动)。
/// 判定必须用**更新前**的副本与源比较 —— 若更新后再比配置里的哈希,基准
/// 已被本次写入覆盖,结果恒为 unchanged(旧实现即此 bug:文件同步了却报
/// 「无改动」,用户无法判断是否生效)。
#[tauri::command]
pub fn update_project_from_source(project_id: String) -> Result<ProjectSourceStatus, String> {
    let mut cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let idx = cfg
        .projects
        .iter()
        .position(|p| p.id == project_id)
        .ok_or_else(|| format!("项目不存在:{}", project_id))?;
    let project = cfg.projects[idx].clone();
    let source_str = project
        .source_compose_path
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("项目「{}」是手工项目,没有可更新的导入源", project.name))?;
    let source = PathBuf::from(&source_str);
    if !source.is_file() {
        return Err(format!("源 compose 不存在:{}", source_str));
    }

    // 先解析校验:源有问题时不落盘、不改配置(与导入一致)
    let stack = parse_compose_file(&source, &[])?;

    let dest = PathBuf::from(&project.compose_file);
    let dest_dir = dest
        .parent()
        .ok_or_else(|| format!("项目副本路径异常:{}", project.compose_file))?
        .to_path_buf();
    if !dest_dir.is_dir() {
        return Err(format!("项目副本目录不存在:{}", dest_dir.display()));
    }

    // ---- 更新前判定:源内容 vs 旧副本内容(副本缺失 = 视为有改动需同步)----
    // 用内容比较(忽略 compose 文件名差异):源叫 compose.yml 而副本恒为
    // docker-compose.yml 时不应误报"已变更"。
    let dest_same = !bundle_content_changed(&source, &dest)?;

    // 旧副本另存(.bak):更新出错或结果不符时可人工回退
    if dest.is_file() {
        let bak = dest_dir.join("docker-compose.yml.bak");
        if let Err(e) = std::fs::copy(&dest, &bak) {
            log::warn!("备份旧 compose 副本失败 ({}): {}", bak.display(), e);
        }
    }
    let new_dest = copy_compose_bundle(&source, &dest_dir)?;

    // 合并 service_overrides:仅在源中仍存在的服务沿用旧分类
    let new_names: Vec<String> = stack.services.iter().map(|s| s.service.clone()).collect();
    let mut merged: Vec<ServiceOverride> = project
        .service_overrides
        .iter()
        .filter(|o| new_names.iter().any(|n| n == &o.service))
        .cloned()
        .collect();
    for svc in &stack.services {
        if !merged.iter().any(|o| o.service == svc.service) {
            merged.push(ServiceOverride {
                service: svc.service.clone(),
                mode: svc.mode.clone(),
            });
        }
    }

    // origin.json 同步刷新(目录名可能已变)
    if let Some(dir_name) = source.parent().and_then(Path::file_name) {
        if let Err(e) = crate::stack::save_origin_file(&dest_dir, &dir_name.to_string_lossy()) {
            log::warn!("更新栈「{}」来源目录名失败: {}", project.name, e);
        }
    }

    let p = &mut cfg.projects[idx];
    p.compose_file = new_dest.to_string_lossy().to_string();
    p.service_overrides = merged;
    p.source_hash = source_content_hash(&source).ok();
    let updated = p.clone();
    save_config(&cfg).map_err(|e| format!("保存配置失败: {}", e))?;

    // 状态按**更新前**的比对结果给出;`source_hash` 现已是最新基准,故直接
    // 组装状态而不复用 project_source_status(那会拿新基准复核,恒 unchanged)。
    let status = project_source_status(&updated);
    Ok(if dest_same {
        status
    } else {
        ProjectSourceStatus {
            state: "changed".to_string(),
            detail: format!(
                "已从源同步:{} 处服务(含 .env 与 override)",
                stack.services.len()
            ),
            ..status
        }
    })
}

/// 解析项目持久化的 compose:`docker images` 一次 → parse_compose_file
/// → 应用该项目的 service_overrides 覆盖默认分类。供部署页渲染服务分类表。
#[tauri::command]
pub async fn parse_compose(project_id: String) -> Result<ComposeStack, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let project = find_project(&cfg, &project_id)?.clone();
    if project.compose_file.trim().is_empty() {
        return Err(format!("项目「{}」未配置 compose 文件", project.name));
    }
    let compose_path = PathBuf::from(&project.compose_file);
    if !compose_path.is_file() {
        return Err(format!("compose 文件不存在:{}", project.compose_file));
    }
    let mut stack = parse_with_local_images(&compose_path).await?;
    apply_overrides(&mut stack.services, &project.service_overrides);
    Ok(stack)
}

/// 对任意路径的 compose 做静态只读解析(不落盘、不改配置),供导入前预览。
#[tauri::command]
pub async fn preview_compose(source_path: String) -> Result<ComposeStack, String> {
    let path = PathBuf::from(&source_path);
    if !path.is_file() {
        return Err(format!("compose 文件不存在:{}", source_path));
    }
    parse_with_local_images(&path).await
}

/// `docker images` 一次 → repo/tag 对 → `parse_compose_file`。
async fn parse_with_local_images(compose_path: &Path) -> Result<ComposeStack, String> {
    let images = tauri::async_runtime::spawn_blocking(crate::docker::list_images)
        .await
        .map_err(|e| format!("获取镜像列表任务失败: {}", e))??;
    let pairs: Vec<(String, String)> = images
        .into_iter()
        .map(|i| (i.repository, i.tag))
        .collect();
    parse_compose_file(compose_path, &pairs)
}

// ===== 宿主机 Docker 命令 =====

