// // 整栈部署预览(dry-run 独立功能)

use super::*;

/// 部署预览的单服务条目;`action` 为分类结果字符串:
/// `"Recreate"`(重建)/`"Create"`(新建)/`"Unchanged"`(不变)/
/// `"Pull"`(服务器拉取)/`"Absent"`(缺失)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackPreviewEntry {
    pub service: String,
    /// compose 里的镜像引用(未设置 image 的服务不产生条目)
    pub image: String,
    /// 传输方式(与部署分类一致,已应用 service_overrides)
    pub mode: TransferMode,
    pub action: String,
}

/// 整栈部署预览结果(纯只读,不落盘、不改远端状态);`errors` 为非阻断问题
/// (compose 副本缺失/解析失败、服务未设 image 等)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackPreview {
    pub entries: Vec<StackPreviewEntry>,
    pub errors: Vec<String>,
}

/// 整栈部署 dry-run 预览:建连后对比「本地 compose 解析结果」与「远端实际状态」,
/// 逐服务分类为 重建/新建/不变/拉取/缺失(见 [`classify_change`])。
///
/// - 本地 `docker images`(含 ID)一次取回,既供 compose 三级匹配,也供镜像
///   ID 与远端对比;compose 副本按 [`parse_compose_file`] 解析并应用
///   service_overrides;
/// - 远端 `docker images` 收集 (repo:tag → 镜像 ID);远端 `docker ps -a`
///   (按 compose project 标签 = remote_dir 基名过滤)收集各服务现存容器镜像;
/// - compose 副本缺失/解析失败、服务未设 image 等 → 记入 `errors`(空 entries
///   照常返回);连接/远端查询失败 → `Err`。
#[tauri::command]
pub async fn preview_stack_changes(
    server_id: String,
    project_id: String,
    password_plain: Option<String>,
) -> Result<StackPreview, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let project = find_project(&cfg, &project_id)?.clone();

    // compose 副本检查前置:副本缺失直接以 errors 返回,避免无谓建连
    let mut errors: Vec<String> = Vec::new();
    if project.compose_file.trim().is_empty() {
        errors.push(format!(
            "项目「{}」未配置 compose 文件,无法预览",
            project.name
        ));
        return Ok(StackPreview {
            entries: Vec::new(),
            errors,
        });
    }
    let compose_path = PathBuf::from(&project.compose_file);
    if !compose_path.is_file() {
        errors.push(format!("compose 文件不存在:{}", project.compose_file));
        return Ok(StackPreview {
            entries: Vec::new(),
            errors,
        });
    }

    let (server, mut client) = connect_server(&server_id, password_plain.as_deref(), None).await?;

    // 本地镜像列表(docker images,含 ID):一次取回,既供 compose 三级匹配,
    // 也供镜像 ID 与远端对比
    let local_images = tauri::async_runtime::spawn_blocking(crate::docker::list_images)
        .await
        .map_err(|e| format!("获取镜像列表任务失败: {}", e))??;
    let pairs: Vec<(String, String)> = local_images
        .iter()
        .map(|i| (i.repository.clone(), i.tag.clone()))
        .collect();
    let mut stack = match parse_compose_file(&compose_path, &pairs) {
        Ok(s) => s,
        Err(e) => return Ok(StackPreview { entries: Vec::new(), errors: vec![e] }),
    };
    apply_overrides(&mut stack.services, &project.service_overrides);
    errors.extend(stack.errors);

    // 远端镜像列表(repo:tag → 镜像 ID)
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询远端镜像超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, REMOTE_IMAGES_CMD),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "查询远端镜像列表失败(退出码 {}),请确认服务器 Docker 可用",
            code
        ));
    }
    let remote_images = parse_image_lines(&out);

    // 远端 compose 项目现存容器(按 project 标签过滤,项目名 = remote_dir 基名)
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询远端容器超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &compose_containers_cmd(&effective_remote_dir(&server, &project))),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "查询远端容器列表失败(退出码 {}),请确认服务器 Docker 可用",
            code
        ));
    }
    let containers = parse_container_lines(&out);

    // 逐服务分类
    let mut entries = Vec::with_capacity(stack.services.len());
    for svc in &stack.services {
        let Some(image) = svc.image.as_deref() else {
            errors.push(format!(
                "服务「{}」未设置 image 字段,无法预览变更",
                svc.service
            ));
            continue;
        };
        let (repo, tag) = split_image_ref(image);
        let local_info = local_images
            .iter()
            .find(|i| i.repository == repo && i.tag == tag);
        let remote_id = remote_images
            .iter()
            .find(|i| i.repository == repo && i.tag == tag)
            .map(|i| i.id.as_str());
        let action = classify_change(
            &svc.mode,
            local_info.is_some(),
            remote_id,
            local_info.map(|i| i.id.as_str()),
            containers.get(&svc.service).map(String::as_str),
        );
        entries.push(StackPreviewEntry {
            service: svc.service.clone(),
            image: image.to_string(),
            mode: svc.mode.clone(),
            action: action.to_string(),
        });
    }
    Ok(StackPreview { entries, errors })
}

/// 部署变更分类(纯函数,预览核心逻辑)。
///
/// 口径:`remote_image_id` / `local_image_id` 均取自 `docker images
/// --format '{{json .}}'` 的 ID 字段(本地/远端同一命令,口径一致),
/// 比较前剥除 `sha256:` 前缀并忽略大小写(见 [`same_image_id`])。
///
/// - Pull → `"Pull"`(服务器自拉,不对比本地);
/// - Local 且本地不存在该 repo:tag → `"Absent"`;
/// - 远端无该 repo:tag 的镜像 → 远端已有容器(旧版在跑)`"Recreate"`,
///   否则 `"Create"`;
/// - 远端镜像 ID 与本地一致 → `"Unchanged"`;
/// - ID 不同 → `"Recreate"`(镜像已更新,up 时会重建容器)。
pub fn classify_change(
    mode: &TransferMode,
    local_exists: bool,
    remote_image_id: Option<&str>,
    local_image_id: Option<&str>,
    remote_container_image: Option<&str>,
) -> &'static str {
    if matches!(mode, TransferMode::Pull) {
        return "Pull";
    }
    if !local_exists {
        return "Absent";
    }
    match (remote_image_id, local_image_id) {
        (Some(r), Some(l)) if same_image_id(r, l) => "Unchanged",
        (Some(_), Some(_)) => "Recreate",
        // 远端无该镜像:已有容器(旧版在跑)→ 重建,否则全新创建
        _ => {
            if remote_container_image.is_some() {
                "Recreate"
            } else {
                "Create"
            }
        }
    }
}

/// 镜像 ID 等价判定(纯函数):剥除 `sha256:` 前缀、忽略大小写后比较,
/// 容忍不同 docker 版本的输出差异;任一为空视为不等。
pub(crate) fn same_image_id(a: &str, b: &str) -> bool {
    fn norm(id: &str) -> &str {
        let id = id.trim();
        id.strip_prefix("sha256:").unwrap_or(id)
    }
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && !b.is_empty() && a.eq_ignore_ascii_case(b)
}

/// 取远端目录的基名(去尾部 `/` 后取最后一个 `/` 之后的部分;
/// 根目录/空串 → 空串)。远端 compose 部署的项目名 = 该基名。
pub(crate) fn remote_dir_basename(remote_dir: &str) -> &str {
    let trimmed = remote_dir.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// 拼装查询远端 compose 项目现存容器的命令:`docker ps -a` 按
/// `com.docker.compose.project` 标签过滤(项目名 = remote_dir 基名,含已退出
/// 容器),JSON 输出每容器一行;`--filter` 参数整体单引号包裹防注入。
pub(crate) fn compose_containers_cmd(remote_dir: &str) -> String {
    format!(
        "docker ps -a --filter {} --format '{{{{json .}}}}'",
        shell_single_quote(&format!(
            "label=com.docker.compose.project={}",
            remote_dir_basename(remote_dir)
        ))
    )
}

/// 逐行解析 `docker images --format {{json .}}` 输出为镜像信息
/// (解析失败的行告警跳过,不让整条查询失败)。
pub(crate) fn parse_image_lines(out: &str) -> Vec<ImageInfo> {
    let mut images = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match crate::docker::parse_image_line(line) {
            Ok(info) => images.push(info),
            Err(e) => log::warn!("跳过无法解析的远端镜像行: {}", e),
        }
    }
    images
}

/// 解析 `docker ps --format {{json .}}` 输出为 (compose 服务名 → 容器镜像引用)
/// 映射(同一服务多容器时后者覆盖;无 compose 服务标签的容器跳过)。
pub(crate) fn parse_container_lines(out: &str) -> HashMap<String, String> {
    let mut containers = HashMap::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_container_line(line) {
            Some((service, image)) => {
                containers.insert(service, image);
            }
            None => log::warn!("跳过无法解析的远端容器行(缺 compose 服务标签或字段异常)"),
        }
    }
    containers
}

/// 从 `docker ps --format {{json .}}` 的一行 JSON 提取
/// `(compose 服务名, 容器镜像引用)`(纯函数,便于单测)。
///
/// 服务名取 Labels(`docker ps` 输出为逗号分隔的 `key=value` 字符串)里的
/// `com.docker.compose.service`;手动 `docker run` 的容器没有该标签 → `None`。
pub(crate) fn parse_container_line(line: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let labels = v.get("Labels")?.as_str()?;
    let service = labels
        .split(',')
        .find_map(|kv| kv.trim().strip_prefix("com.docker.compose.service="))?
        .trim()
        .to_string();
    if service.is_empty() {
        return None;
    }
    let image = v.get("Image")?.as_str()?.to_string();
    Some((service, image))
}

// ===== 发布清单(manifest.json,智能传输收尾写入,供一键回滚)=====

