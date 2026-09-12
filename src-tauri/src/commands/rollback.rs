// // 一键回滚 + 独立回滚中心 + 归档版本说明 + manifest

use super::*;

/// manifest.json 中单个服务的镜像条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestImage {
    /// compose 服务名
    pub service: String,
    /// 镜像引用(`repo:tag`;`docker load` 会恢复包内镜像的原引用)
    pub tag: String,
    /// 发布目录内的镜像包文件名;未打包(跳过传输)为 `null`
    pub file: Option<String>,
}

/// 整栈部署成功时写入发布目录的 `manifest.json` 结构(回滚列表页据此展示
/// 各 release 包含的服务;`docker-compose.yml` 副本随清单一并归档)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub project: String,
    /// 发布时间戳(即 release 目录名,形如 20260905-101010)
    pub ts: String,
    pub compose_copy: String,
    pub images: Vec<ManifestImage>,
}

impl ReleaseManifest {
    /// `compose_copy` 固定为发布目录内的归档文件名。
    pub fn new(project: String, ts: String, images: Vec<ManifestImage>) -> Self {
        Self {
            project,
            ts,
            compose_copy: "docker-compose.yml".to_string(),
            images,
        }
    }
}

/// 组装 manifest 的镜像条目(纯函数,便于单测):逐个 Local 服务一条;
/// `skip[i] == true` 表示该服务未打包(skip_unchanged 剔除且未留档),
/// `file` 记 `None`,其余按顺序消费 `packed_files` 中的镜像包文件名。
/// `tag` 存完整镜像引用(无标签时按 Docker 约定补 latest,见 [`split_image_ref`])。
pub(crate) fn build_manifest_images(
    local: &[&StackServiceChoice],
    skip: &[bool],
    packed_files: &[String],
) -> Vec<ManifestImage> {
    let mut files = packed_files.iter();
    local
        .iter()
        .enumerate()
        .map(|(i, svc)| {
            let (repo, tag) = split_image_ref(&svc.image);
            let file = if skip.get(i).copied().unwrap_or(false) {
                None
            } else {
                files.next().cloned()
            };
            ManifestImage {
                service: svc.service.clone(),
                tag: format!("{}:{}", repo, tag),
                file,
            }
        })
        .collect()
}

/// 整栈部署收尾子步:向发布目录写入回滚资料 —— compose 副本(sftp 上传,
/// 归档名 `docker-compose.yml`,内容即本次部署使用的本地副本)与
/// `manifest.json`(base64 经远端 exec 解码写入,避免 JSON 引号/换行的
/// shell 转义问题)。
///
/// 尽力而为:任一失败仅告警 —— 发布目录缺 manifest/副本时,回滚命令会优雅
/// 降级(服务列表为空、沿用服务器现有 compose 文件),不推翻已成功的部署。
pub(crate) async fn write_release_artifacts(
    app: &AppHandle,
    client: &mut SshClient,
    project: &ProjectConfig,
    release_dir: &str,
    manifest: &ReleaseManifest,
) {
    // 1) compose 副本(部署前置已校验本地副本存在,直接复用)
    let compose_local = PathBuf::from(&project.compose_file);
    let archived = client
        .sftp_upload(&compose_local, release_dir, "docker-compose.yml", false, &|_, _| {})
        .await;
    match archived {
        Ok(()) => emit_log(app, "已存档 compose 副本到发布目录"),
        Err(e) => emit_log(
            app,
            &format!(
                "警告:存档 compose 副本失败(回滚时将沿用服务器现有 compose 文件): {}",
                e
            ),
        ),
    }

    // 2) manifest.json:echo <b64> | base64 -d > '<release_dir>/manifest.json'
    let json = match serde_json::to_string(manifest) {
        Ok(j) => j,
        Err(e) => {
            emit_log(app, &format!("警告:序列化 manifest.json 失败: {}", e));
            return;
        }
    };
    let manifest_path = remote_join(release_dir, "manifest.json");
    let cmd = format!(
        "echo {} | base64 -d > {}",
        BASE64_STANDARD.encode(json.as_bytes()),
        shell_single_quote(&manifest_path)
    );
    let code = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "写入发布清单超时",
        "请检查服务器网络后重试",
        async {
            client
                .exec(&cmd, &mut |_| {})
                .await
                .map_err(|e| format!("远端写入 manifest.json 失败: {}", e))
        },
    )
    .await;
    match code {
        Ok(0) => emit_log(app, "已写入发布清单 manifest.json"),
        Ok(c) => emit_log(
            app,
            &format!("警告:写入 manifest.json 失败(退出码 {}): {}", c, manifest_path),
        ),
        Err(e) => emit_log(app, &format!("警告:{}", e)),
    }
}

// ===== 一键回滚(整栈回滚到历史 release / 单镜像回滚到历史标签)=====

/// `rollback_list_releases` 返回的单个历史发布条目。
/// (Tauri 序列化为 camelCase,与前端 deploy.js 读取的 `hasManifest` /
/// `hasComposeCopy` 字段名一致。)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseBrief {
    /// 发布时间戳(releases 目录名,形如 20260905-101010)
    pub ts: String,
    /// 发布目录内的文件名列表(镜像包 + manifest.json / docker-compose.yml)
    pub files: Vec<String>,
    /// manifest.json 里记录的服务名列表;无清单(旧版本发布)为空
    pub services: Vec<String>,
    pub has_manifest: bool,
    pub has_compose_copy: bool,
}

/// `rollback_list_tags` 返回的单个本地镜像标签条目(按创建时间倒序)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagBrief {
    pub tag: String,
    /// 镜像 ID(`docker images` 的 ID 字段,含 `sha256:` 前缀原样返回)
    pub id: String,
    /// docker 的 CreatedAt 文本(如 `2026-08-01 10:00:00 +0800 CST`)
    pub created: String,
}

/// 列出项目服务器上的历史发布(新 → 旧),供前端渲染一键回滚列表。
///
/// `releases` 目录不存在(从未整栈部署)返回空列表,不是错误;单个发布目录
/// 缺 manifest.json / 读取失败时相应字段优雅降级(`services` 空、
/// `has_manifest` false),不让整个列表功能失败。
#[tauri::command]
pub async fn rollback_list_releases(
    server_id: String,
    password_plain: Option<String>,
    project_id: String,
) -> Result<Vec<ReleaseBrief>, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    // 项目存在性校验 + 取其项目级部署目录(第四批;未配置则回落服务器目录)
    let project = find_project(&cfg, &project_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    let project_dir = effective_remote_dir(&server, &project);
    // 第七批二次优化:单次往返 —— find 循环在远端枚举归档并批量读取
    // (releases 目录不存在时 find 静默无输出 → 空列表,与既有口径一致);
    // 列表不需要镜像清单与 compose 文本,命令不带这两段
    let scan_cmd = releases_scan_cmd(&project_dir, 100, &[], false);
    let (_, dump_out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "读取发布列表超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &scan_cmd),
    )
    .await
    .unwrap_or((1, String::new()));
    let dump = parse_releases_dump(&dump_out, &[]);

    let mut briefs = Vec::new();
    for entry in &dump.entries {
        if entry.files.is_empty() {
            // 目录可能刚被清理或不可读,跳过该条目,不拖垮整个列表
            log::warn!("跳过无法读取的发布目录 {}(批量读取为空)", entry.ts);
            continue;
        }
        let files = entry.files.clone();
        let has_manifest = files.iter().any(|f| f == "manifest.json");
        let has_compose_copy = files.iter().any(|f| f == "docker-compose.yml");
        let services = parse_release_manifest(&entry.manifest)
            .map(|m| m.images.into_iter().map(|img| img.service).collect())
            .unwrap_or_default();
        briefs.push(ReleaseBrief {
            ts: entry.ts.clone(),
            files,
            services,
            has_manifest,
            has_compose_copy,
        });
    }
    // find + sort -r 已按新 → 旧返回(时间戳形如 20260905-101010)
    Ok(briefs)
}

/// 列出项目服务器上指定仓库的全部镜像标签(创建时间倒序),
/// 供单镜像回滚选择"回到哪个历史标签"。
#[tauri::command]
pub async fn rollback_list_tags(
    server_id: String,
    password_plain: Option<String>,
    project_id: String,
    repository: String,
) -> Result<Vec<TagBrief>, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    find_project(&cfg, &project_id)?;
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref(), None).await?;
    let cmd = format!(
        "docker images {} --format '{{{{json .}}}}'",
        shell_single_quote(&repository)
    );
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询远端标签超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &cmd),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "查询远端镜像标签失败(退出码 {}),请确认服务器 Docker 可用",
            code
        ));
    }
    let mut tags: Vec<TagBrief> = parse_image_lines(&out)
        .into_iter()
        .map(|i| TagBrief {
            tag: i.tag,
            id: i.id,
            created: i.created,
        })
        .collect();
    // 创建时间倒序(docker 的 CreatedAt 文本按字典序即时间序)
    tags.sort_by(|a, b| b.created.cmp(&a.created));
    Ok(tags)
}

/// 整栈一键回滚:把指定历史 release 的镜像包重新 `docker load`(自动恢复
/// 镜像原标签),恢复 compose 副本并 `compose up -d`。复用 deploy-log /
/// deploy-done 事件体系,`deploy-done` 恰好 emit 一次;成功后落一条
/// `mode = "rollback"` 的部署历史。
#[tauri::command]
pub async fn rollback_execute_stack(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    project_id: String,
    release_ts: String,
) -> Result<(), String> {
    finish_rollback(
        &app,
        rollback_execute_stack_inner(
            &app,
            &server_id,
            password_plain.as_deref(),
            &project_id,
            &release_ts,
        ),
    )
    .await
}

/// [`rollback_execute_stack`] 的管线主体:成功返回组装好的部署历史记录
/// (由 [`finish_rollback`] 落历史),失败返回中文错误。
async fn rollback_execute_stack_inner(
    app: &AppHandle,
    server_id: &str,
    password_plain: Option<&str>,
    project_id: &str,
    release_ts: &str,
) -> Result<DeployRecord, String> {
    let started = std::time::Instant::now();
    // 每次回滚开始时重置取消标志(与部署管线一致)
    reset_cancelled(app);

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, server_id)?.clone();
    let project = find_project(&cfg, project_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain,
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut record = DeployRecord::new_skeleton(MODE_ROLLBACK, &server.name, &project.name, Vec::new());
    record.server_id = Some(server.id.clone());
    record.project_id = Some(project.id.clone());

    emit_log(
        app,
        &format!(
            "开始整栈回滚:服务器「{}」/ 项目「{}」,目标发布 {}",
            server.name, project.name, release_ts
        ),
    );

    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    let release_dir = releases_dir(&effective_remote_dir(&server, &project), release_ts);

    // ---- 校验发布目录存在 ----
    ensure_not_cancelled(app)?;
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验发布目录超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &test_dir_cmd(&release_dir)),
    )
    .await?;
    if code != 0 {
        return Err(format!("回滚目标发布目录不存在: {}", release_dir));
    }

    // ---- 列出发布目录内容(镜像包 + manifest.json + compose 副本)----
    ensure_not_cancelled(app)?;
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询发布目录超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &ls_dir_cmd(&release_dir)),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "查询发布目录内容失败(退出码 {}): {}",
            code, release_dir
        ));
    }
    let files = parse_ls_lines(&out);
    let packages: Vec<String> = files
        .iter()
        .filter(|f| f.ends_with(".tar.gz"))
        .cloned()
        .collect();
    let has_compose_copy = files.iter().any(|f| f == "docker-compose.yml");

    // ---- 读 manifest(存在才解析):校验留档归属并登记镜像引用,供回滚历史展示 ----
    if files.iter().any(|f| f == "manifest.json") {
        let manifest_path = remote_join(&release_dir, "manifest.json");
        let (code, out) = with_timeout(
            SSH_EXEC_TIMEOUT_SECS,
            "读取发布清单超时",
            "请检查服务器网络后重试",
            exec_collect(&mut client, &cat_file_cmd(&manifest_path)),
        )
        .await?;
        if code == 0 {
            if let Some(m) = parse_release_manifest(&out) {
                // 留档归属校验:同服务器多项目共用 remote_dir 时,防止误回滚
                // 对方项目的留档(manifest 在任何 docker load 之前读取,此处
                // 中止时远端状态未被改动)
                if m.project != project.name {
                    return Err(format!(
                        "留档 {} 属于项目「{}」,与当前项目「{}」不符,已中止",
                        release_ts, m.project, project.name
                    ));
                }
                record.images = m.images.into_iter().map(|i| i.tag).collect();
            }
        } else {
            emit_log(app, "警告:读取发布清单失败,按无清单处理");
        }
    }

    // ---- 逐包 docker load(load 自动恢复镜像原标签)----
    let n = packages.len();
    if n == 0 {
        emit_log(app, "发布目录内无镜像包,跳过 docker load");
    }
    for (i, name) in packages.iter().enumerate() {
        ensure_not_cancelled(app)?;
        let remote_tar = remote_join(&release_dir, name);
        emit_log(
            app,
            &format!(
                "回滚装载镜像包 ({}/{}): docker load -i {}",
                i + 1,
                n,
                remote_tar
            ),
        );
        let load_cmd = format!("docker load -i {}", shell_single_quote(&remote_tar));
        if let Err(e) = exec_forwarded(app, &mut client, &load_cmd, STACK_LOAD_TIMEOUT_SECS).await {
            // 部分失败提示:中止时点之前的包已装载成功(镜像标签已恢复),
            // 但容器尚未重建 —— 明确当前状态,避免误以为回滚未产生任何效果
            return Err(format!(
                "装载镜像包 {}/{} 失败:{};已装载 {}/{} 个镜像包,这些包的镜像标签已恢复,容器未重建(可排除问题后重新发起回滚)",
                i + 1,
                n,
                e,
                i,
                n
            ));
        }
    }

    // ---- 恢复 compose 副本(发布目录归档了本次部署使用的 compose 文件)----
    ensure_not_cancelled(app)?;
    let remote_compose = remote_compose_path(&effective_remote_dir(&server, &project));
    if has_compose_copy {
        let cp_cmd = format!(
            "cp {} {}",
            shell_single_quote(&remote_join(&release_dir, "docker-compose.yml")),
            shell_single_quote(&remote_compose)
        );
        emit_log(app, &format!("恢复 compose 文件: {}", cp_cmd));
        if let Err(e) = exec_forwarded(app, &mut client, &cp_cmd, SSH_EXEC_TIMEOUT_SECS).await {
            // 降级继续(与"无副本沿用现有 compose"同口径):base compose 仍在
            // 远端根目录(部署时已上传),副本恢复失败不阻断回滚语义
            emit_log(
                app,
                &format!(
                    "警告:恢复 compose 副本失败({}),沿用服务器现有 compose 文件继续回滚",
                    e
                ),
            );
        }
    } else {
        emit_log(app, "发布目录无 compose 副本,沿用服务器现有 compose 文件");
    }

    // ---- compose up -d(镜像标签已恢复,up 按引用重建容器)----
    ensure_not_cancelled(app)?;
    // override 文件名与单镜像回滚同口径:部署时 upload_compose_files 已把
    // override 上传到远端根目录,回滚按文件名直接引用,保证 -f 文件链与
    // 部署时 pull/up 一致(override-only 服务不逃逸)
    let override_names = compose_override_names(&project.compose_file);
    let up_cmd = compose_up_cmd(&effective_remote_dir(&server, &project), &remote_compose, &override_names);
    emit_log(app, &format!("启动服务: {}", up_cmd));
    exec_forwarded(app, &mut client, &up_cmd, STACK_COMPOSE_TIMEOUT_SECS).await?;

    emit_log(app, "整栈回滚完成");
    record.success = true;
    record.message = format!("回滚到 {}", release_ts);
    record.duration_secs = started.elapsed().as_secs();
    Ok(record)
}

/// 单镜像一键回滚:把服务器的 `repository:date_tag`(历史标签)重新指到
/// `target_ref`(compose 引用的标签,如 myapp:latest),再 `compose up -d`。
/// 复用 deploy-log / deploy-done 事件体系,`deploy-done` 恰好 emit 一次;
/// 成功后落一条 `mode = "rollback"` 的部署历史。
#[tauri::command]
pub async fn rollback_execute_single(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    project_id: String,
    repository: String,
    date_tag: String,
    target_ref: String,
) -> Result<(), String> {
    finish_rollback(
        &app,
        rollback_execute_single_inner(
            &app,
            &server_id,
            password_plain.as_deref(),
            &project_id,
            &repository,
            &date_tag,
            &target_ref,
        ),
    )
    .await
}

/// [`rollback_execute_single`] 的管线主体:成功返回组装好的部署历史记录,
/// 失败返回中文错误。
async fn rollback_execute_single_inner(
    app: &AppHandle,
    server_id: &str,
    password_plain: Option<&str>,
    project_id: &str,
    repository: &str,
    date_tag: &str,
    target_ref: &str,
) -> Result<DeployRecord, String> {
    let started = std::time::Instant::now();
    reset_cancelled(app);

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, server_id)?.clone();
    let project = find_project(&cfg, project_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain,
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut record = DeployRecord::new_skeleton(
        MODE_ROLLBACK,
        &server.name,
        &project.name,
        vec![target_ref.to_string()],
    );
    record.server_id = Some(server.id.clone());
    record.project_id = Some(project.id.clone());

    let source = format!("{}:{}", repository, date_tag);
    emit_log(
        app,
        &format!(
            "开始镜像回滚:服务器「{}」/ 项目「{}」:{} -> {}",
            server.name, project.name, source, target_ref
        ),
    );

    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    // ---- 校验历史标签存在(不存在 → 明确报错,不盲目 docker tag)----
    ensure_not_cancelled(app)?;
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验镜像超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &docker_inspect_cmd(&source)),
    )
    .await?;
    if code != 0 {
        return Err(format!("服务器上不存在镜像 {},无法回滚到该标签", source));
    }

    // ---- 远端 compose 文件检查(与单镜像部署 prepare_single_compose 同口径)----
    // 导入项目:部署时上传到远端根目录的副本;旧版手工项目:compose_file 即远端路径
    let (compose_path, overrides) = if Path::new(&project.compose_file).is_file() {
        (
            remote_compose_path(&effective_remote_dir(&server, &project)),
            compose_override_names(&project.compose_file),
        )
    } else if is_windows_absolute_path(&project.compose_file) {
        return Err(format!(
            "本地 compose 文件不存在:{};请确认路径或重新导入 compose 文件",
            project.compose_file
        ));
    } else {
        (project.compose_file.clone(), Vec::new())
    };
    ensure_not_cancelled(app)?;
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验 compose 文件超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &test_file_cmd(&compose_path)),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "远端 compose 文件不存在:{},请先完成一次部署",
            compose_path
        ));
    }

    // ---- docker tag:把 compose 引用的标签重新指回历史镜像(零拷贝)----
    ensure_not_cancelled(app)?;
    let tag_cmd = docker_tag_cmd(&source, target_ref);
    emit_log(app, &format!("回滚标签: {}", tag_cmd));
    exec_forwarded(app, &mut client, &tag_cmd, SSH_EXEC_TIMEOUT_SECS).await?;

    // ---- compose up -d ----
    ensure_not_cancelled(app)?;
    let up_cmd = compose_up_cmd(&effective_remote_dir(&server, &project), &compose_path, &overrides);
    emit_log(app, &format!("启动服务: {}", up_cmd));
    exec_forwarded(app, &mut client, &up_cmd, STACK_COMPOSE_TIMEOUT_SECS).await?;

    emit_log(app, "镜像回滚完成");
    record.success = true;
    record.message = format!("回滚到 {}", source);
    record.duration_secs = started.elapsed().as_secs();
    Ok(record)
}

// ===== 独立回滚模块(第三批:按服务器真实项目分列)=====
//
// 现状 `rollback_*` 全部以应用内 `project_id` 为入口,释出目录固定取
// `server.remote_dir/releases`;服务器上真实存在的项目(尤其不是本应用部署的)
// 无法被看到或回滚。本组命令按**目录**工作:先扫描服务器真实项目,
// 再对指定目录列出发布归档/日期标签并执行回滚。

/// 服务器上的一个真实项目(回滚模块列表项)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackProject {
    /// 项目目录(绝对路径)
    pub dir: String,
    /// compose 文件路径(未扫到为空)
    pub compose_file: String,
    /// 该项目的发布归档目录(完整路径,新→旧)
    pub releases: Vec<String>,
    /// 归档数量
    pub release_count: usize,
    /// 最近的归档时间戳(无归档为空)
    pub latest_release: String,
    /// 运行中的容器数(按 compose project label 归属;取不到为 0)
    pub running_containers: usize,
    /// 匹配到的应用内项目名(仅标注;空串 = 服务器上存在但软件内未配置)
    pub app_project: String,
}

/// 发布归档明细(回滚选择项)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackReleaseDetail {
    pub ts: String,
    pub dir: String,
    /// 归档内的镜像包文件名
    pub packages: Vec<String>,
    /// manifest 记录的服务名(无清单为空)
    pub services: Vec<String>,
    /// manifest 记录的逐服务镜像条目(第七批:详情模态展示;无清单为空)
    pub manifest_images: Vec<ManifestImage>,
    pub has_manifest: bool,
    pub has_compose_copy: bool,
    /// 版本说明标题(归档内 release-notes.json;未设置/解析失败为 None)
    pub note_title: Option<String>,
    /// 版本说明正文(同上)
    pub note_body: Option<String>,
    /// 版本说明最近保存时间(RFC3339;同上)
    pub note_updated_at: Option<String>,
}

/// 日期标签镜像明细(单镜像回滚选择项;复用 [`TagBrief`] 字段口径)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackTagDetail {
    pub repository: String,
    pub tags: Vec<TagBrief>,
}

/// 项目明细:发布归档 + 各仓库的日期标签(供回滚面板两级选择)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackProjectDetail {
    pub dir: String,
    pub compose_file: String,
    pub releases: Vec<RollbackReleaseDetail>,
    pub repositories: Vec<RollbackTagDetail>,
}

/// 扫描服务器上的真实项目(回滚模块入口)。
///
/// 项目来源 = 文件系统扫描(含 compose 文件的目录)+ `docker ps` 的
/// compose labels 合并去重;以服务器真实目录为准,应用内项目仅作标注。
/// `scan_root` 缺省用服务器配置的 `remote_dir`。
#[tauri::command]
pub async fn rollback_scan_projects(
    server_id: String,
    password_plain: Option<String>,
    scan_root: Option<String>,
) -> Result<Vec<RollbackProject>, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    let root = scan_root
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| server.remote_dir.clone());

    // 第七批二次优化:单次往返 —— 组合命令一次带回 compose 文件清单、
    // releases 归档清单与 `docker ps -a`(Labels 直读 compose working_dir,
    // 替代此前「docker ps 取 ID → 逐容器 inspect」的两次额外往返)
    let (_, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "扫描项目超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &rollback_scan_cmd(&root)),
    )
    .await?;
    let scan = parse_rollback_scan(&out);
    if !scan.root_exists {
        return Err(format!(
            "扫描目录「{}」失败(目录可能不存在或无权限)",
            root
        ));
    }

    // 项目目录 = compose 文件父目录 + docker Labels 的 working_dir 合并去重
    let mut dirs: Vec<String> = Vec::new();
    for p in &scan.compose_paths {
        if let Some((parent, _)) = p.rsplit_once('/') {
            if !parent.is_empty() && !dirs.iter().any(|d| d == parent) {
                dirs.push(parent.to_string());
            }
        }
    }
    for wd in compose_working_dirs(&scan.ps_items) {
        if !dirs.iter().any(|d| d == &wd) {
            dirs.push(wd);
        }
    }
    dirs.sort();

    let mut projects = Vec::new();
    for dir in dirs {
        let compose_file = scan
            .compose_paths
            .iter()
            .find(|p| p.rsplit_once('/').map(|(d, _)| d) == Some(dir.as_str()))
            .cloned()
            .unwrap_or_default();
        let mut releases: Vec<String> = scan
            .release_paths
            .iter()
            .filter(|r| project_dir_of_release(r) == dir)
            .cloned()
            .collect();
        releases.sort_by(|a, b| b.cmp(a));
        let latest = releases
            .first()
            .and_then(|r| r.rsplit('/').next().map(String::from))
            .unwrap_or_default();
        let dir_name = dir.rsplit('/').next().unwrap_or("");
        let app_project = cfg
            .projects
            .iter()
            .find(|p| {
                p.name == dir_name
                    || p.compose_file
                        .rsplit_once('/')
                        .map(|(d, _)| d == dir)
                        .unwrap_or(false)
            })
            .map(|p| p.name.clone())
            .unwrap_or_default();
        // 运行容器数:以 Names 前缀/目录名近似归属(无 labels 时的兜底)
        let running = scan
            .ps_items
            .iter()
            .filter(|c| {
                let status = jstr(c, "Status").to_lowercase();
                status.starts_with("up") && jstr(c, "Names").contains(dir_name)
            })
            .count();
        projects.push(RollbackProject {
            dir,
            compose_file,
            release_count: releases.len(),
            releases,
            latest_release: latest,
            running_containers: running,
            app_project,
        });
    }
    Ok(projects)
}

/// 列出某项目目录下的发布归档与日期标签(回滚面板明细)。
#[tauri::command]
pub async fn rollback_project_detail(
    server_id: String,
    password_plain: Option<String>,
    dir: String,
) -> Result<RollbackProjectDetail, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    let dir = dir.trim().trim_end_matches('/').to_string();
    let releases_root = remote_join(&dir, "releases");
    // ① 单次往返批量读取(第七批二次优化):归档枚举由远端 find 循环完成 ——
    //    命令长度恒定,连"先 ls 再拼装"的往返都省掉;manifest / 版本说明 /
    //    compose 候选 / 全量镜像列表一条命令全部带回,整次明细仅 1 次 SSH 往返
    let compose_candidates = [
        remote_join(&dir, "docker-compose.yml"),
        remote_join(&dir, "compose.yml"),
    ];
    let scan_cmd = releases_scan_cmd(&dir, 50, &compose_candidates, true);
    let (_, dump_out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "读取归档详情超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &scan_cmd),
    )
    .await
    .unwrap_or((1, String::new()));
    let dump = parse_releases_dump(&dump_out, &compose_candidates);

    let mut releases: Vec<RollbackReleaseDetail> = Vec::new();
    for entry in &dump.entries {
        if entry.files.is_empty() {
            // 文件清单为空 = 归档目录刚被清理或不可读:跳过,不拖垮明细(与既有口径一致)
            continue;
        }
        let packages: Vec<String> = entry
            .files
            .iter()
            .filter(|f| f.ends_with(".tar.gz"))
            .cloned()
            .collect();
        let has_manifest = entry.files.iter().any(|f| f == "manifest.json");
        let has_compose_copy = entry.files.iter().any(|f| f == "docker-compose.yml");
        let (services, manifest_images) = match parse_release_manifest(&entry.manifest) {
            Some(m) => (
                m.images.iter().map(|img| img.service.clone()).collect(),
                m.images,
            ),
            None => (Vec::new(), Vec::new()),
        };
        let (note_title, note_body, note_updated_at) = match parse_release_notes(&entry.notes) {
            Some(n) => (Some(n.title), Some(n.body), Some(n.updated_at)),
            None => (None, None, None),
        };
        releases.push(RollbackReleaseDetail {
            ts: entry.ts.clone(),
            dir: remote_join(&releases_root, &entry.ts),
            packages,
            services,
            manifest_images,
            has_manifest,
            has_compose_copy,
            note_title,
            note_body,
            note_updated_at,
        });
    }

    // ② 日期标签:compose 候选解析仓库名;镜像列表已在 ① 带回,本地按仓库过滤(零往返)
    let mut repositories: Vec<RollbackTagDetail> = Vec::new();
    let mut compose_file = String::new();
    let image_items = parse_cleanup_ndjson(&dump.images).0;
    for (i, cand) in compose_candidates.iter().enumerate() {
        let repos = compose_image_repos(&dump.compose_texts[i]);
        if repos.is_empty() {
            continue;
        }
        if compose_file.is_empty() {
            // 修复:此前恒返回第一个候选名;现记录实际产出仓库的候选
            compose_file = cand.clone();
        }
        for repo in repos {
            if repo.trim().is_empty() || repositories.iter().any(|r| r.repository == repo) {
                continue;
            }
            let mut tags: Vec<TagBrief> = image_items
                .iter()
                .filter(|v| jstr(v, "Repository") == repo && is_date_tag(&jstr(v, "Tag")))
                .map(|v| TagBrief {
                    tag: jstr(v, "Tag"),
                    id: jstr(v, "ID"),
                    created: jstr(v, "CreatedAt"),
                })
                .collect();
            tags.sort_by(|a, b| b.tag.cmp(&a.tag));
            if !tags.is_empty() {
                repositories.push(RollbackTagDetail { repository: repo, tags });
            }
        }
        break; // 与既有口径一致:第一个产出仓库的候选生效
    }
    if compose_file.is_empty() {
        // compose 可读但未声明任何 image(或两个候选都读不到):回退到可读到
        // 文本的候选,保持"能识别到 compose 文件"这一信息不丢失
        if let Some(i) = dump
            .compose_texts
            .iter()
            .position(|t| !t.trim().is_empty())
        {
            compose_file = compose_candidates[i].clone();
        }
    }

    Ok(RollbackProjectDetail {
        dir,
        compose_file,
        releases,
        repositories,
    })
}

/// 删除指定的发布归档目录(回滚中心「删除」按钮;二次确认由前端负责)。
///
/// 安全约束(防误删/防注入):
/// - `dir` 必须是绝对路径;
/// - `ts` 必须是纯目录名(不含 `/` 与 `..`),与 `dir` 拼成
///   `<dir>/releases/<ts>` 后**校验路径前缀**,越出该项目 releases 目录即拒;
/// - 只执行 `rm -rf` 这一个由后端拼装、单引号包裹的路径。
#[tauri::command]
pub async fn rollback_delete_release(
    server_id: String,
    password_plain: Option<String>,
    dir: String,
    release_ts: String,
) -> Result<(), String> {
    let dir = dir.trim().trim_end_matches('/').to_string();
    if !dir.starts_with('/') {
        return Err(format!("项目目录必须是绝对路径:{}", dir));
    }
    let ts = release_ts.trim().to_string();
    if ts.is_empty() || ts.contains('/') || ts.contains("..") || ts.contains('\\') {
        return Err(format!("发布标识不合法:{}", release_ts));
    }
    let release_dir = remote_join(&dir, &format!("releases/{}", ts));
    // 前缀校验:必须严格位于 <dir>/releases/ 之下(防 ts 注入逃逸)
    if !release_dir.starts_with(&format!("{}/releases/", dir)) {
        return Err(format!("目标越出项目 releases 目录:{}", release_dir));
    }

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    // 存在性校验:不存在则明确报错(而非静默成功)
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验归档超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &test_dir_cmd(&release_dir)),
    )
    .await?;
    if code != 0 {
        return Err(format!("归档目录不存在:{}", release_dir));
    }

    let cmd = format!("rm -rf {}", shell_single_quote(&release_dir));
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "删除归档超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &cmd),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "删除归档失败(退出码 {}): {}",
            code,
            out.trim()
        ));
    }
    Ok(())
}

/// 删除指定的日期标签镜像(回滚中心「删除」按钮;二次确认由前端负责)。
///
/// 安全约束:`reference` 形如 `repo:YYYYmmdd-HHMMSS`,仅校验非空且不含
/// shell 元字符风险字符(实际经 [`shell_single_quote`] 包裹);删除前先
/// `docker image inspect` 校验存在,避免 `docker rmi` 的模糊匹配误删。
#[tauri::command]
pub async fn rollback_delete_tag(
    server_id: String,
    password_plain: Option<String>,
    reference: String,
) -> Result<(), String> {
    let reference = reference.trim().to_string();
    if reference.is_empty() || reference.contains(char::is_whitespace) {
        return Err(format!("镜像引用不合法:{}", reference));
    }
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验镜像超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &docker_inspect_cmd(&reference)),
    )
    .await?;
    if code != 0 {
        return Err(format!("服务器上不存在镜像 {}", reference));
    }

    // 不使用 -f:若仍被容器引用则命令失败并回传原因(前端二次确认已提示),
    // 比强制删除更安全 —— 与「清理分析」跳过在用镜像的口径一致。
    let cmd = format!("docker rmi {}", shell_single_quote(&reference));
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "删除镜像超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &cmd),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "删除镜像失败(退出码 {}): {}",
            code,
            out.trim()
        ));
    }
    Ok(())
}

// ===== 归档版本说明(第七批;类 GitHub Release 的标题 + 更新描述)=====
//
// 说明持久化在归档目录内的 `release-notes.json`(与归档同生共死、跨机器可用),
// 读取并入 [`releases_dump_cmd`] 批量 cat(零额外往返);写入复用 manage_stacks
// .env 的「base64 → tmp($$ PID 后缀)→ mv」原子写先例。

/// 单条版本说明的内容上限(UTF-8 字节数)。与 .env 编辑的量级对齐——远小于
/// exec 命令行安全长度,base64 膨胀 4/3 后仍充裕。
const RELEASE_NOTES_MAX_BYTES: usize = 64 * 1024;

/// 归档的版本说明(归档目录内 `release-notes.json`;camelCase 契约)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseNotes {
    /// 版本标题(类似 GitHub Release 的 tag 名,如「v1.2.0 修复登录问题」)
    pub title: String,
    /// 更新说明正文(多行)
    pub body: String,
    /// 最近一次保存时间(RFC3339 UTC)
    pub updated_at: String,
}

/// 解析 release-notes.json 文本(纯函数,便于单测):损坏/缺字段 → `None`
/// (调用方按"未设置说明"降级,不影响详情展示)。
pub(crate) fn parse_release_notes(json: &str) -> Option<ReleaseNotes> {
    serde_json::from_str(json.trim()).ok()
}

/// 拼 release-notes.json 的原子写入命令(纯函数,便于单测)。
///
/// 形态与 manage_stacks 的 `env_write_cmd` 同源:tmp 名 = 引号包裹的路径 +
/// `.ddtmp.` + 引号外的 `$$`(PID 展开为纯数字,防并发交错;引号内 `$$`
/// 不展开,故必须留在外侧),`mv` 同文件系统 rename 原子,中断不损坏旧文件。
pub(crate) fn release_notes_write_cmd(notes_path: &str, b64: &str) -> String {
    let tmp = format!("{}'.ddtmp.'", shell_single_quote(notes_path));
    format!(
        "echo {} | base64 -d > {}$$ && mv {}$$ {}",
        b64,
        tmp,
        tmp,
        shell_single_quote(notes_path)
    )
}

/// [`rollback_set_release_notes`] 入参(camelCase 契约)。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseNotesRequest {
    pub server_id: String,
    pub password_plain: Option<String>,
    /// 项目目录(绝对路径)
    pub dir: String,
    /// 归档时间戳(纯目录名)
    pub ts: String,
    pub title: String,
    pub body: String,
}

/// [`rollback_set_release_notes`] 返回:保存/清除后的最近更新时间。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseNotesSaved {
    pub updated_at: String,
}

/// 保存归档的版本说明(回滚中心「版本详情」表单)。
///
/// 写入 `<dir>/releases/<ts>/release-notes.json`;**title 与 body 均为空时删除
/// 该文件**(保持归档目录干净,详情回退为「未设置」)。
/// 安全约束与 [`rollback_delete_release`] 同款:dir 绝对路径、ts 纯目录名、
/// 拼接后前缀校验防逃逸;内容经 base64 编码(字符集无 shell 元字符)。
#[tauri::command]
pub async fn rollback_set_release_notes(req: ReleaseNotesRequest) -> Result<ReleaseNotesSaved, String> {
    let dir = req.dir.trim().trim_end_matches('/').to_string();
    if !dir.starts_with('/') {
        return Err(format!("项目目录必须是绝对路径:{}", req.dir));
    }
    let ts = req.ts.trim().to_string();
    if ts.is_empty() || ts.contains('/') || ts.contains("..") || ts.contains('\\') {
        return Err(format!("发布标识不合法:{}", req.ts));
    }
    let release_dir = remote_join(&dir, &format!("releases/{}", ts));
    if !release_dir.starts_with(&format!("{}/releases/", dir)) {
        return Err(format!("目标越出项目 releases 目录:{}", release_dir));
    }
    let title = req.title.trim().to_string();
    let body = req.body.trim().to_string();
    if title.len() + body.len() > RELEASE_NOTES_MAX_BYTES {
        return Err(format!(
            "版本说明过长(上限 {} KB)",
            RELEASE_NOTES_MAX_BYTES / 1024
        ));
    }

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &req.server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        req.password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    let notes_path = remote_join(&release_dir, "release-notes.json");
    let updated_at = chrono::Utc::now().to_rfc3339();
    let cmd = if title.is_empty() && body.is_empty() {
        // 两字段均空 = 清除说明:删除文件,详情回退为「未设置」
        format!("rm -f {}", shell_single_quote(&notes_path))
    } else {
        let notes = ReleaseNotes {
            title,
            body,
            updated_at: updated_at.clone(),
        };
        let json = serde_json::to_string(&notes).map_err(|e| format!("序列化版本说明失败: {}", e))?;
        release_notes_write_cmd(&notes_path, &BASE64_STANDARD.encode(json.as_bytes()))
    };
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "保存版本说明超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &cmd),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "保存版本说明失败(退出码 {}): {}",
            code,
            out.trim()
        ));
    }
    Ok(ReleaseNotesSaved { updated_at })
}

/// 按**服务器项目目录**执行整栈回滚(独立回滚模块入口)。
///
/// 与 [`rollback_execute_stack`] 的差异:不依赖应用内项目配置 —— 释出目录
/// 直接取自传入的项目目录,compose 恢复目标为 `<dir>/docker-compose.yml`。
/// 归属校验改为"归档目录必须位于该项目目录下"(路径前缀比对),避免跨项目误回滚。
/// 复用 deploy-log / deploy-done 事件体系与部署历史记录。
#[tauri::command]
pub async fn rollback_execute_stack_at(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    dir: String,
    release_ts: String,
) -> Result<(), String> {
    finish_rollback(
        &app,
        rollback_execute_stack_at_inner(
            &app,
            &server_id,
            password_plain.as_deref(),
            &dir,
            &release_ts,
        ),
    )
    .await
}

/// [`rollback_execute_stack_at`] 的管线主体。
async fn rollback_execute_stack_at_inner(
    app: &AppHandle,
    server_id: &str,
    password_plain: Option<&str>,
    dir: &str,
    release_ts: &str,
) -> Result<DeployRecord, String> {
    let started = std::time::Instant::now();
    reset_cancelled(app);

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain,
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let dir = dir.trim().trim_end_matches('/').to_string();
    if !dir.starts_with('/') {
        return Err(format!("项目目录必须是绝对路径:{}", dir));
    }
    let project_name = dir.rsplit('/').next().unwrap_or(&dir).to_string();
    let mut record =
        DeployRecord::new_skeleton(MODE_ROLLBACK, &server.name, &project_name, Vec::new());

    emit_log(
        app,
        &format!(
            "开始整栈回滚:服务器「{}」/ 项目目录 {} ,目标发布 {}",
            server.name, dir, release_ts
        ),
    );

    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    // 归档目录 = <dir>/releases/<ts>;归属校验靠"必须位于该项目目录下"
    let release_dir = remote_join(&dir, &format!("releases/{}", release_ts));
    if !release_dir.starts_with(&format!("{}/releases/", dir)) {
        return Err(format!("回滚目标越出项目目录:{}", release_dir));
    }

    ensure_not_cancelled(app)?;
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验发布目录超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &test_dir_cmd(&release_dir)),
    )
    .await?;
    if code != 0 {
        return Err(format!("回滚目标发布目录不存在: {}", release_dir));
    }

    ensure_not_cancelled(app)?;
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询发布目录超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &ls_dir_cmd(&release_dir)),
    )
    .await?;
    if code != 0 {
        return Err(format!("查询发布目录内容失败(退出码 {}): {}", code, release_dir));
    }
    let files = parse_ls_lines(&out);
    let packages: Vec<String> = files.iter().filter(|f| f.ends_with(".tar.gz")).cloned().collect();
    let has_compose_copy = files.iter().any(|f| f == "docker-compose.yml");

    // manifest 只用于历史展示(归属由目录前缀保证,不以项目名卡)
    if files.iter().any(|f| f == "manifest.json") {
        let mp = remote_join(&release_dir, "manifest.json");
        if let Ok((0, m_out)) = exec_collect(&mut client, &cat_file_cmd(&mp)).await {
            if let Some(m) = parse_release_manifest(&m_out) {
                record.images = m.images.into_iter().map(|i| i.tag).collect();
            }
        }
    }

    let n = packages.len();
    if n == 0 {
        emit_log(app, "发布目录内无镜像包,跳过 docker load");
    }
    for (i, name) in packages.iter().enumerate() {
        ensure_not_cancelled(app)?;
        let remote_tar = remote_join(&release_dir, name);
        emit_log(
            app,
            &format!("回滚装载镜像包 ({}/{}): docker load -i {}", i + 1, n, remote_tar),
        );
        let load_cmd = format!("docker load -i {}", shell_single_quote(&remote_tar));
        if let Err(e) = exec_forwarded(app, &mut client, &load_cmd, STACK_LOAD_TIMEOUT_SECS).await {
            return Err(format!(
                "装载镜像包 {}/{} 失败:{};已装载 {}/{} 个镜像包,这些包的镜像标签已恢复,容器未重建(可排除问题后重新发起回滚)",
                i + 1, n, e, i, n
            ));
        }
    }

    // 恢复 compose 副本到该项目目录(而非 server.remote_dir)
    ensure_not_cancelled(app)?;
    let target_compose = remote_join(&dir, "docker-compose.yml");
    if has_compose_copy {
        let cp_cmd = format!(
            "cp {} {}",
            shell_single_quote(&remote_join(&release_dir, "docker-compose.yml")),
            shell_single_quote(&target_compose)
        );
        emit_log(app, &format!("恢复 compose 文件: {}", cp_cmd));
        if let Err(e) = exec_forwarded(app, &mut client, &cp_cmd, SSH_EXEC_TIMEOUT_SECS).await {
            emit_log(
                app,
                &format!("警告:恢复 compose 副本失败({}),沿用现有 compose 文件继续回滚", e),
            );
        }
    } else {
        emit_log(app, "发布目录无 compose 副本,沿用现有 compose 文件");
    }

    // compose up -d:cd 到项目目录,按目录内 compose 文件启动
    // (override 文件按远端同名约定自动生效,无需显式 -f 链)
    ensure_not_cancelled(app)?;
    let up_cmd = format!(
        "cd {} && docker compose up -d",
        shell_single_quote(&dir)
    );
    emit_log(app, &format!("启动服务: {}", up_cmd));
    exec_forwarded(app, &mut client, &up_cmd, STACK_COMPOSE_TIMEOUT_SECS).await?;

    emit_log(app, "整栈回滚完成");
    record.success = true;
    record.message = format!("回滚到 {}", release_ts);
    record.release_dir = Some(release_dir);
    record.duration_secs = started.elapsed().as_secs();
    Ok(record)
}

/// 组装回滚收尾通知的标题与正文(纯函数,便于单测)。///
/// 标题:成功=「回滚成功」;取消(错误文案为 CANCELLED_MSG)=「回滚已取消」;
/// 其余失败=「回滚失败」。正文含项目名 + 服务器名 + 结果消息 + 耗时
/// (成功时消息为「回滚到 <目标 release ts / 镜像标签>」,回滚目标随之入文);
/// `record` 为 `None`(panic 或配置读取等早期失败,记录未组装/随错误丢失)
/// 时用兜底文案。
pub(crate) fn rollback_notify_text(
    success: bool,
    message: &str,
    record: &Option<DeployRecord>,
) -> (String, String) {
    let title = if success {
        "回滚成功".to_string()
    } else if message == CANCELLED_MSG {
        "回滚已取消".to_string()
    } else {
        "回滚失败".to_string()
    };
    let body = match record {
        Some(r) => format!(
            "项目「{}」@ 服务器「{}」:{}(耗时 {} 秒)",
            r.project_name, r.server_name, message, r.duration_secs
        ),
        None => format!("{}(回滚详情缺失,详见应用日志)", message),
    };
    (title, body)
}

/// 回滚命令的统一收尾:任何路径(成功/失败/panic)下 `deploy-done` 恰好
/// emit 一次;成功后落地 `mode = "rollback"` 的部署历史记录(append 失败
/// 仅告警,不影响回滚结果);emit 之后调 [`crate::notify::fire`] 分发通知
/// 中心通知(成功/失败/取消,事件订阅复用部署的 AppConfig.notify.events,
/// 失败仅告警,不影响回滚结果)。
async fn finish_rollback<F>(app: &AppHandle, fut: F) -> Result<(), String>
where
    F: std::future::Future<Output = Result<DeployRecord, String>> + Send,
{
    // 托盘 tooltip(第十五批):回滚执行中。回滚期间服务会重启,窗口隐藏时
    // 用户应当能从托盘看出「现在不是空闲,别动手动脚」;收尾(成功/失败)在
    // 下方 match 内清态
    crate::tray_status::set_rolling_back(app, true);
    let result = match CatchPanic::new(fut).await {
        Ok(res) => res,
        Err(panic_info) => {
            log::error!("回滚管线发生 panic: {}", panic_info);
            Err("回滚过程发生内部错误,详情见日志".to_string())
        }
    };
    // 无论成败都清回滚态;成败后的空闲文案分别带「上次部署成功/失败」语义
    // (回滚的成败也记进同一终态:用户视角「回滚完成」即部署态的一种恢复)
    crate::tray_status::set_deploy_finished(app, result.is_ok(), false);
    match result {
        Ok(record) => {
            // 通知文案在 record 被消费前组装(正文含项目名与回滚目标)
            let (title, body) = rollback_notify_text(true, &record.message, &Some(record.clone()));
            let _ = app.emit(
                "deploy-done",
                DeployDone {
                    success: true,
                    message: "回滚完成".to_string(),
                },
            );
            // 通知中心:回滚成功(emit deploy-done 之后异步分发,不阻塞收尾)
            crate::notify::fire(app.clone(), "success", title, body).await;
            append_record(record);
            Ok(())
        }
        Err(e) => {
            emit_log(app, &format!("回滚失败: {}", e));
            // 取消导致的失败(固定文案 CANCELLED_MSG)按 cancel 事件分发
            let kind = if e == CANCELLED_MSG { "cancel" } else { "failure" };
            let (title, body) = rollback_notify_text(false, &e, &None);
            let _ = app.emit("deploy-done", DeployDone { success: false, message: e.clone() });
            // 通知中心:回滚失败/取消(emit deploy-done 之后异步分发)
            crate::notify::fire(app.clone(), kind, title, body).await;
            Err(e)
        }
    }
}

/// 解析发布目录的 manifest.json 内容(纯函数):损坏 / 缺字段 → `None`
/// (调用方按"无清单"降级,不让列表与回滚功能失败)。
pub fn parse_release_manifest(json: &str) -> Option<ReleaseManifest> {
    serde_json::from_str(json.trim()).ok()
}

// ===== 钩子/健康检查纯逻辑(便于单测,Task 3)=====

