// // 单镜像 + 整栈部署管线(含 compose 打包/上传/钩子/健康检查)

use super::*;

/// 发起部署:立即返回 `Ok(())`,管线在后台任务执行并通过事件推送进度。
#[tauri::command]
pub fn deploy(req: DeployRequest, app: AppHandle) -> Result<(), String> {
    spawn_deploy_task(app.clone(), async move {
        // 单发语义:全量事件(deploy-progress + deploy-done)+ 断点续传开启
        run_one_deploy(&app, req, None, DeployEmitOpts::single()).await
    });
    Ok(())
}

/// 发起整栈部署(compose 多服务):立即返回 `Ok(())`,六步管线在后台任务
/// 执行并通过事件推送进度(1 分类确认 2 打包 3 上传 4 装载 5 拉取 6 启动)。
#[tauri::command]
pub fn deploy_stack(req: StackDeployRequest, app: AppHandle) -> Result<(), String> {
    spawn_deploy_task(app.clone(), async move {
        run_one_deploy_stack(&app, req, None, DeployEmitOpts::single()).await
    });
    Ok(())
}

/// 单次部署执行的内层(单发/续传/批量共用):执行「[`run_deploy`] 管线 +
/// history + webhook + 通知中心」的统一收尾([`finish_deploy_run`]),并按
/// `opts` 表达事件:
///
/// - `emit_progress = false`(批量):任务级抑制管线内的 `deploy-progress`
///   (批量进度由 `deploy-batch` 事件表达),单发 `true` 照常;
/// - `emit_done = false`(批量):收尾不 emit `deploy-done`(结果由
///   `deploy-batch` 表达),单发 `true` 恰好 emit 一次;
/// - `log_prefix`(批量 = `[服务器名] `):任务级 `deploy-log` 前缀;
/// - `checkpoint = false`(批量):断点落盘整体关闭(resume 一律 `None`)。
///
/// 返回 `Ok(部署记录)`(成功)/ `Err(错误文案)`(失败/取消/panic)。
pub(crate) async fn run_one_deploy(
    app: &AppHandle,
    req: DeployRequest,
    resume: Option<ResumeContext>,
    opts: DeployEmitOpts,
) -> Result<DeployRecord, String> {
    DEPLOY_EVENT_CTX
        .scope(
            DeployEventCtx::of(&opts),
            finish_deploy_run(
                app.clone(),
                run_deploy(app, req, resume, opts.checkpoint),
                opts.emit_done,
            ),
        )
        .await
}

/// 整栈部署执行的内层(单发/续传/批量共用),语义同 [`run_one_deploy`]。
pub(crate) async fn run_one_deploy_stack(
    app: &AppHandle,
    req: StackDeployRequest,
    resume: Option<ResumeContext>,
    opts: DeployEmitOpts,
) -> Result<DeployRecord, String> {
    DEPLOY_EVENT_CTX
        .scope(
            DeployEventCtx::of(&opts),
            finish_deploy_run(
                app.clone(),
                run_deploy_stack(app, req, resume, opts.checkpoint),
                opts.emit_done,
            ),
        )
        .await
}

/// 部署管线入口:组装部署历史记录骨架(含开始计时),执行管线主体,
/// 出口填充 success/message/duration 后连同结果与 webhook 通知地址一起返回
/// (由 [`finish_deploy_run`] 落历史、发通知)。
///
/// `resume` 为 `Some`(断点续传入口 [`deploy_resume_start`])时,管线从断点的
/// `step_next` 起跳步执行并对已完成产物幂等化复用;正常部署传 `None`(行为不变)。
/// `checkpoint = false`(批量部署)时断点落盘整体关闭(见
/// [`run_deploy_steps`] 的 `checkpoint` 参数)。
async fn run_deploy(
    app: &AppHandle,
    req: DeployRequest,
    resume: Option<ResumeContext>,
    checkpoint: bool,
) -> (Result<(), String>, DeployRecord, Option<String>) {
    let started = std::time::Instant::now();
    // webhook 通知地址:项目配置了 notify_webhook 才发(前置失败的路径取不到,为 None)
    let webhook_url = project_webhook_url(&req.project_id);
    // 骨架:server/project 名称由前置解析回填(前置失败时以 ID 兜底)
    let mut record = DeployRecord::new_skeleton(
        MODE_SINGLE,
        &req.server_id,
        &req.project_id,
        vec![req.image.clone()],
    );
    let result = run_deploy_steps(app, req, &mut record, resume, checkpoint).await;
    record.success = result.is_ok();
    record.message = match &result {
        Ok(()) => "部署完成".to_string(),
        Err(e) => e.clone(),
    };
    record.duration_secs = started.elapsed().as_secs();
    (result, record, webhook_url)
}

/// 部署管线主体(严格顺序,任一步失败即中止)。`record` 为组装中的部署历史
/// 记录,随步骤推进回填服务器/项目名称与实际部署的镜像引用。
///
/// 智能传输(`skip_unchanged`,仅 `use_date_tag = false` 生效):对比本地与
/// 远端同标签镜像 ID,一致时跳过步骤 2/3 的导出上传与步骤 5 的装载,
/// compose up 照常执行。
///
/// 断点续传(`resume`,UPGRADE-PLAN 阶段六):`resume.step_next` 之后的步骤
/// 才执行,已完成步骤 emit「断点续传:跳过步骤 N」;`checkpoint = true` 时
/// 每个步骤完成的收尾处落盘断点(成功后清除,失败/取消保留给续传)。
/// `resume` 为 `None` 且 `checkpoint = true` 为正常部署:行为与旧版一致,
/// 仅新增步骤边界落盘与成功清理。
async fn run_deploy_steps(
    app: &AppHandle,
    req: DeployRequest,
    record: &mut DeployRecord,
    resume: Option<ResumeContext>,
    checkpoint: bool,
) -> Result<(), String> {
    // ---- 步骤 0:前置 ----
    // 每次 deploy 开始时重置取消标志;结束时保持不变(取消后为 true,下次部署重置)
    reset_cancelled(app);

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &req.server_id)?.clone();
    let project = find_project(&cfg, &req.project_id)?.clone();
    record.server_name = server.name.clone();
    record.project_name = project.name.clone();
    record.server_id = Some(server.id.clone());
    record.project_id = Some(project.id.clone());
    let password = resolve_password(
        &server.auth.auth_type,
        req.password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    // 托盘 tooltip(第十五批):置「部署中」运行态。目标串给窗口隐藏时的
    // 用户看;批量前缀取自当前任务的事件上下文(有 `[服务器名] ` 前缀即批量,
    // 死代码 deploy_batch 专用;线上批量每台是独立单发,自然逐台刷新)
    {
        // 有前缀 = 死代码批量路径专用;线上批量每台是独立单发任务,前缀为空
        let batch_hint = DeployEventCtx::log_prefix().trim().to_string();
        crate::tray_status::set_deploy_running(
            app,
            false,
            0,
            0,
            &format!("{} / {}", server.name, project.name),
            &batch_hint,
        );
    }
    emit_log(
        app,
        &format!(
            "开始部署:服务器「{}」/ 项目「{}」",
            server.name, project.name
        ),
    );

    // 断点续传:落盘键与产物上下文(续传以断点产物为准,正常部署从请求构造)
    // 键优先取断点上下文(清理与加载指向同一键);正常部署按请求现算
    let key = match &resume {
        Some(r) => r.key.clone(),
        None => checkpoint_key(&req.server_id, &req.project_id, MODE_SINGLE),
    };
    let resume_step = resume.as_ref().map(|r| r.step_next).unwrap_or(1);
    let mut art = match &resume {
        Some(r) => r.single.clone(),
        None => SingleResumeArtifacts {
            origin_ref: req.image.clone(),
            repository: req.repository.clone(),
            use_date_tag: req.use_date_tag,
            skip_unchanged: req.skip_unchanged.unwrap_or(false),
            ..Default::default()
        },
    };
    if let Some(r) = &resume {
        emit_log(
            app,
            &format!(
                "断点续传:从步骤 {}({})继续部署",
                r.step_next,
                resume_step_label(MODE_SINGLE, r.step_next)
            ),
        );
    }
    // 初始断点(步骤 1 前落盘,同键新部署覆盖旧断点;续传不重置起点)
    if checkpoint && resume.is_none() {
        checkpoint_save(&key, MODE_SINGLE, 1, &server, &project, artifacts_value(&art));
    }

    // ---- 步骤 1:打标签 ----
    let image_ref = if resume_step > 1 {
        emit_log(app, "断点续传:跳过步骤 1(打标签)");
        match art.image_ref.clone() {
            Some(r) => r,
            None => {
                return Err(
                    "断点数据损坏:缺少已打标签的镜像引用,请放弃该断点后重新部署".to_string(),
                )
            }
        }
    } else {
        emit_progress(app, 1, 5, "打标签");
        ensure_not_cancelled(app)?;
        let image_ref = if req.use_date_tag {
            let new_tag = unique_deploy_tag(app, &req.repository).await?;
            emit_log(app, &format!("打标签: {} -> {}", req.image, new_tag));
            tag_image(&req.image, &new_tag)?;
            emit_log(app, "标签已创建");
            new_tag
        } else {
            emit_log(app, "使用原始镜像标签,跳过打标签");
            req.image.clone()
        };
        art.image_ref = Some(image_ref.clone());
        if checkpoint {
            checkpoint_save(&key, MODE_SINGLE, 2, &server, &project, artifacts_value(&art));
        }
        image_ref
    };
    // 历史记录登记实际部署的镜像引用(勾选日期标签时为生成的部署标签)
    record.images = vec![image_ref.clone()];

    // ---- 智能传输判定(仅 use_date_tag = false 生效)----
    // 日期标签每次部署都生成全新 tag,远端必然没有同 tag 镜像,检测无意义;
    // 对比口径与整栈一致:远端同标签镜像 ID == 本地镜像 ID(见 [`same_image_id`])。
    let mut skip_transfer = false;
    // 判定"未变化"时保住对比阶段的连接,步骤 3 直接复用(不再重连);
    // 判定"有变化"则丢弃,维持原有「先导出、后建连上传」的时序
    let mut probe: Option<SshClient> = None;
    if req.skip_unchanged.unwrap_or(false) {
        if req.use_date_tag {
            emit_log(app, "使用日期标签部署,每次均为全新 tag,跳过未变化检测");
        } else {
            ensure_not_cancelled(app)?;
            emit_log(app, "智能传输:正在对比本地与远端镜像 ID…");
            let mut probe_client =
                SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default())
                    .await?;
            let remote_ids = query_remote_image_id_map(&mut probe_client).await?;
            let (repo, tag) = split_image_ref(&image_ref);
            let full_ref = format!("{}:{}", repo, tag);
            if let (Some(remote_id), Ok(Some(local_id))) = (
                remote_ids.get(&full_ref),
                image_id_by_ref(&image_ref).await,
            ) {
                if same_image_id(remote_id, &local_id) {
                    skip_transfer = true;
                    emit_log(app, &format!("未变化,跳过导出与上传: {}", image_ref));
                }
            }
            if skip_transfer {
                probe = Some(probe_client);
            }
        }
    }

    // ---- 步骤 2:导出压缩(镜像未变化时整步跳过)----
    // `packed` 为 `None` 表示跳过传输:不产生本地 tar,也没有装载步骤
    let packed: Option<(String, PathBuf, Option<TempFileGuard>, Option<u64>)> = if skip_transfer {
        emit_progress(app, 2, 5, "跳过导出(镜像未变化)");
        emit_log(app, "镜像与远端一致,跳过导出压缩");
        None
    } else {
        // 断点续传:上次已完成导出(步骤 2 边界之后失败)且本地 tar 完整
        // (存在且 >0 字节)→ 复用跳过导出;丢失/半成品 → 重新导出。
        // 步骤 3 已完成(> 3)时远端 tar 已就绪,本地文件是否存在无关紧要
        // (装载用远端路径),直接复用记录的文件名、不再重新导出。
        let reusable = resume.as_ref().filter(|r| r.step_next > 2).and_then(|r| {
            let local = r.single.tar_local.as_ref()?;
            let name = r.single.tar_name.as_ref()?;
            let complete = r.step_next > 3
                || std::fs::metadata(local)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false);
            complete.then(|| (name.clone(), PathBuf::from(local)))
        });
        match reusable {
            Some((tar_name, out_path)) => {
                emit_log(
                    app,
                    &format!(
                        "断点续传:跳过导出压缩,复用已导出的镜像包 {}",
                        out_path.display()
                    ),
                );
                Some((tar_name, out_path, None, image_size(&image_ref)))
            }
            None => {
                if resume_step > 2 {
                    emit_log(app, "警告:断点记录的本地镜像包已丢失,重新导出");
                }
                emit_progress(app, 2, 5, "导出压缩镜像");
                ensure_not_cancelled(app)?;
                let tar_name = format!("{}.tar.gz", uuid::Uuid::new_v4());
                let out_path = std::env::temp_dir().join(&tar_name);
                // 断点续传开启时保留本地 tar 供失败后复用(成功/放弃时显式清理);
                // 关闭时用完即删:Drop guard 覆盖成功/失败全部路径(旧行为)
                let guard = if checkpoint {
                    TempFileGuard::keep(out_path.clone())
                } else {
                    TempFileGuard::new(out_path.clone())
                };

                // 空间预检:导出目标盘(临时目录所在盘)剩余空间 ≥ 镜像大小 × 1.5
                // (镜像大小暂存,供步骤 3 的远端磁盘预检复用,避免二次查询)
                let image_bytes = image_size(&image_ref);
                match image_bytes {
                    Some(size) => check_export_disk_space(size)?,
                    None => emit_log(app, "警告:无法获取镜像大小,跳过磁盘剩余空间检查"),
                }

                let total_bytes = export_image(app, &image_ref, &out_path).await?;
                emit_log(app, &format!("导出完成,共 {} MB", total_bytes / 1024 / 1024));
                art.tar_local = Some(out_path.to_string_lossy().to_string());
                art.tar_name = Some(tar_name.clone());
                if checkpoint {
                    checkpoint_save(&key, MODE_SINGLE, 3, &server, &project, artifacts_value(&art));
                }
                Some((tar_name, out_path, Some(guard), image_bytes))
            }
        }
    };

    // ---- 步骤 3:上传镜像(镜像未变化时跳过)----
    let mut client = if skip_transfer {
        emit_progress(app, 3, 5, "跳过上传(镜像未变化)");
        emit_log(app, "镜像与远端一致,跳过上传");
        probe.take().expect("镜像未变化路径必然持有对比阶段连接")
    } else {
        // 断点续传:步骤 3 已完成时不再推送本步进度(事件从 step_next 起)
        if resume_step <= 3 {
            emit_progress(app, 3, 5, "上传镜像到服务器");
        }
        ensure_not_cancelled(app)?;
        let (tar_name, out_path, _, image_bytes) = packed
            .as_ref()
            .expect("未跳过传输时导出产物必然存在");
        let mut client =
            SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default())
                .await?;
        if resume_step > 3 {
            // 断点续传:上次已完成上传 → 仅建连(后续步骤复用连接),不重复上传
            // (远端 tar 仍由步骤 5.6 在装载后清理,行为不变)
            emit_log(app, "断点续传:跳过步骤 3(上传镜像)");
        } else {
            // 远端磁盘预检:上传前确认 Docker 根目录所在盘剩余空间 ≥ 镜像大小 × 1.5
            // (镜像大小未知 → 告警跳过;不足 → 中文报错中止)
            let need_bytes = image_bytes.map(|size| (size as f64 * 1.5) as u64);
            remote_disk_precheck(app, &mut client, need_bytes).await?;
            // 镜像包同名即同内容(uuid 命名),启用断点续传
            upload_tar(app, &mut client, out_path, tar_name).await?;
            emit_log(app, "镜像上传完成");
            if checkpoint {
                checkpoint_save(&key, MODE_SINGLE, 4, &server, &project, artifacts_value(&art));
            }
        }
        client
    };

    // ---- 步骤 4:同步文件(含部署前钩子;断点续传已完成则整步跳过)----
    // 跳过时仅按项目配置推演远端 compose 路径(不再重复上传 compose 副本)
    let single_compose = if resume_step > 4 {
        emit_log(app, "断点续传:跳过步骤 4(同步文件)");
        single_compose_target(&server, &project)?
    } else {
        emit_progress(app, 4, 5, "同步项目文件");
        ensure_not_cancelled(app)?;
        let target = prepare_single_compose(app, &mut client, &server, &project).await?;
        sync_files(app, &mut client, &server, &project).await?;
        emit_log(app, "项目文件同步完成");
        // 部署前钩子(归入步骤 4:装载前执行,旧容器仍在运行;失败即中止部署)
        run_hook(app, &mut client, &project, HookKind::Pre, &effective_remote_dir(&server, &project)).await?;
        if checkpoint {
            checkpoint_save(&key, MODE_SINGLE, 5, &server, &project, artifacts_value(&art));
        }
        target
    };

    // ---- 步骤 5:服务器部署 ----
    emit_progress(app, 5, 5, "服务器部署");
    ensure_not_cancelled(app)?;
    let retag = if req.use_date_tag {
        // 勾选日期标签时,save/load 的是日期 tag;必须把原引用(如 myapp:latest)
        // 也指到新镜像上,compose 引用原 tag 才能感知变化并重建容器。
        Some((image_ref.clone(), req.image.clone()))
    } else {
        None
    };
    server_deploy(
        app,
        &mut client,
        &server,
        &project,
        &single_compose.remote_file,
        &single_compose.override_names,
        // 智能传输判定未变化时无本地包 → 跳过 docker load(远端已是该镜像)
        packed.as_ref().map(|(tar_name, _, _, _)| tar_name.as_str()),
        retag,
        // 断点续传:装载前先 inspect 远端镜像,已存在(上次装载已成功)则跳过
        resume.as_ref().map(|_| image_ref.as_str()),
    )
    .await?;

    // ---- 成功收尾:清除断点 + 删除断点期保留的本地临时 tar ----
    // (失败/取消不走这里:断点与临时 tar 都保留,供续传复用)
    if checkpoint {
        let local_tars: Vec<PathBuf> = art
            .tar_local
            .iter()
            .map(PathBuf::from)
            .collect();
        checkpoint_cleanup_on_success(&key, &local_tars);
    }

    emit_log(app, "部署完成");
    Ok(())
}

/// 步骤 1:生成不与本地已有标签冲突的部署标签。
///
/// `make_deploy_tag` 时间戳精确到秒,同秒重复部署会撞名;
/// 检测到本地已有同名标签则 sleep 1 秒后重新生成,最多重试 5 次。
async fn unique_deploy_tag(app: &AppHandle, repository: &str) -> Result<String, String> {
    let mut tag = make_deploy_tag(repository, "");
    for _ in 0..5 {
        if !image_exists(&tag) {
            return Ok(tag);
        }
        emit_log(app, &format!("标签 {} 已存在,1 秒后重新生成", tag));
        tokio::time::sleep(Duration::from_secs(1)).await;
        tag = make_deploy_tag(repository, "");
    }
    if image_exists(&tag) {
        return Err(
            "无法生成唯一的部署标签(重试 5 次均与本地已有标签冲突),请稍后重试".to_string(),
        );
    }
    Ok(tag)
}

/// 步骤 2 前置:检查临时目录所在盘剩余空间 ≥ 镜像大小 × 1.5,不足报错。
/// (整栈部署传入全部 Local 镜像大小之和,按同一口径预检。)
fn check_export_disk_space(image_bytes: u64) -> Result<(), String> {
    let need = (image_bytes as f64 * 1.5) as u64;
    let dir = std::env::temp_dir();
    let free = fs4::free_space(&dir)
        .map_err(|e| format!("检查磁盘剩余空间失败 ({}): {}", dir.display(), e))?;
    if free < need {
        return Err(format!(
            "磁盘剩余空间不足:导出镜像约需 {:.1} GB,临时目录 {} 所在盘仅剩 {:.1} GB",
            need as f64 / 1024.0 / 1024.0 / 1024.0,
            dir.display(),
            free as f64 / 1024.0 / 1024.0 / 1024.0,
        ));
    }
    Ok(())
}

/// 把阻塞型 `save_gzip`(`docker save` → gzip 流式压缩)放入 blocking 线程池
/// 执行,返回压缩后的总字节数;进度回调由调用方提供(并行打包传空回调)。
/// 内层 blocking 任务 panic 也被转换为 `Err`,不会向上传播 panic。
async fn run_save_gzip<F>(image_ref: &str, out_path: &Path, progress_cb: F) -> Result<u64, String>
where
    F: Fn(u64) + Send + 'static,
{
    let image = image_ref.to_string();
    let path = out_path.to_path_buf();
    let handle = tauri::async_runtime::spawn_blocking(move || save_gzip(&image, &path, progress_cb));
    match handle.await {
        Ok(Ok(total)) => Ok(total),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("导出任务异常终止: {}", e)),
    }
}

/// 步骤 2(单镜像管线):`docker save` → gzip 流式压缩导出到 `out_path`。
///
/// progress_cb 用 `AtomicU64` 累计压缩后字节数,每 ≥5MB 变化 emit 一次
/// `deploy-log`(“已导出 X MB”)。
async fn export_image(app: &AppHandle, image_ref: &str, out_path: &Path) -> Result<u64, String> {
    // 导出进度回调在 blocking 线程池执行,读不到任务级日志前缀
    // ([`DEPLOY_EVENT_CTX`]),这里在任务内先取好、捕获进闭包
    // (单发路径为空串,行为不变;批量路径由回调自带 `[服务器名] ` 前缀)
    let log_prefix = DeployEventCtx::log_prefix();
    let last_reported = Arc::new(AtomicU64::new(0));
    let app_for_cb = app.clone();
    let last = Arc::clone(&last_reported);

    run_save_gzip(image_ref, out_path, move |n| {
        let prev = last.load(Ordering::Relaxed);
        if n >= prev.saturating_add(LOG_PROGRESS_STEP) {
            last.store(n, Ordering::Relaxed);
            emit_log(&app_for_cb, &format!("{}已导出 {} MB", log_prefix, n / 1024 / 1024));
        }
    })
    .await
}

/// 静默导出(无逐块进度日志):并行打包时多个镜像交叉输出逐块日志没有意义,
/// 进度改为按「完成镜像数」汇报(见 [`pack_local_images`])。
async fn export_image_silent(image_ref: &str, out_path: &Path) -> Result<u64, String> {
    run_save_gzip(image_ref, out_path, |_| {}).await
}

/// 拼装「镜像包上传重试仍失败」的中文错误(纯函数,便于单测):
/// 保留两次失败信息,便于对照首次中断点与重试失败原因。
pub fn upload_retry_failure_msg(retry_err: &str, first_err: &str) -> String {
    format!("镜像上传重试仍失败:{}(首次失败:{})", retry_err, first_err)
}

/// 镜像包上传重试前的等待:提示 + 固定间隔(轮间检查取消)。
/// 取消 → 返回错误码 canceled 的错误中止,不再重试;否则由调用方对**同一远端路径**
/// 执行重试 —— `sftp_upload(resume=true)` 的 stat 命中远端半成品 → 断点续传生效。
async fn upload_retry_wait(app: &AppHandle) -> Result<(), String> {
    emit_log(app, "上传中断,2 秒后从断点续传重试");
    tokio::time::sleep(Duration::from_secs(UPLOAD_RETRY_DELAY_SECS)).await;
    ensure_not_cancelled(app)
}

/// 步骤 3:上传镜像 tar 到远端固定 `/tmp` 目录,按每 10% 进度 emit `deploy-log`。
///
/// 断点续传在「失败后同路径重试一次」时生效:每次部署 attempt 的 tar 名均为新
/// uuid,attempt 之间无同名文件;同 attempt 内重试时 `sftp_upload(resume=true)`
/// 经 stat 命中远端半成品 → Resume 分支,不必重传已传部分。
async fn upload_tar(
    app: &AppHandle,
    client: &mut SshClient,
    tar_path: &Path,
    tar_name: &str,
) -> Result<(), String> {
    let last_pct = Arc::new(AtomicU64::new(0));
    let app_for_cb = app.clone();
    let last = Arc::clone(&last_pct);
    let on_progress = move |sent, total| {
        if total == 0 {
            return;
        }
        let step10 = (sent * 100 / total) / 10 * 10;
        let prev = last.load(Ordering::Relaxed);
        if step10 > prev {
            last.store(step10, Ordering::Relaxed);
            emit_log(&app_for_cb, &format!("镜像上传进度 {}%", step10));
        }
    };

    // 失败后同路径重试一次:重试时 stat 命中断点 → 续传生效;
    // 重试返回 AlreadyDone(远端已传 ≥ 本地)同样视为该包成功。
    let first_err = match client
        .sftp_upload(tar_path, "/tmp", tar_name, true, &on_progress)
        .await
    {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    upload_retry_wait(app).await?;
    match client
        .sftp_upload(tar_path, "/tmp", tar_name, true, &on_progress)
        .await
    {
        Ok(()) => {
            emit_log(app, "断点续传重试成功");
            Ok(())
        }
        Err(e) => Err(upload_retry_failure_msg(&e, &first_err)),
    }
}

/// 步骤 4:同步项目 `file_mappings` 到远端。
///
/// - 目录映射:`sftp_upload_dir(local, remote_join(remote_dir, mapping.remote))`
/// - 文件映射:`sftp_upload(local, 父目录, 文件名)`,远端目标 =
///   `remote_join(remote_dir, mapping.remote)`,再拆成父目录 + 文件名
///
/// 本地路径不存在 → 报错中止(错误信息含本地路径)。
async fn sync_files(
    app: &AppHandle,
    client: &mut SshClient,
    server: &ServerConfig,
    project: &ProjectConfig,
) -> Result<(), String> {
    if project.file_mappings.is_empty() {
        emit_log(app, "项目未配置文件映射,跳过文件同步");
        return Ok(());
    }
    for mapping in &project.file_mappings {
        ensure_not_cancelled(app)?;
        let local = PathBuf::from(&mapping.local);
        if !local.exists() {
            return Err(format!("同步文件失败:本地路径不存在:{}", mapping.local));
        }
        // 服务器相对路径未填时回退为本地路径的末段名(与编辑表单的默认值同一口径):
        // 目录映射传目录名本身、文件映射传文件名,避免把内容铺进部署根目录
        let remote_rel = if mapping.remote.trim().is_empty() {
            local_basename(&mapping.local).unwrap_or_default()
        } else {
            mapping.remote.clone()
        };
        let full_remote = remote_join(&effective_remote_dir(server, project), &remote_rel);
        if mapping.is_dir {
            emit_log(app, &format!("同步目录: {} -> {}", mapping.local, full_remote));
            client.sftp_upload_dir(&local, &full_remote, &|_, _| {}).await?;
        } else {
            let (dir, name) = split_remote_file(&full_remote);
            emit_log(app, &format!("同步文件: {} -> {}/{}", mapping.local, dir, name));
            // 文件映射内容可变、远端同名未必同内容,不做断点续传(全新写)
            client
                .sftp_upload(&local, &dir, &name, false, &|_, _| {})
                .await?;
        }
    }
    Ok(())
}

/// 取本地路径的末段名(兼容 Windows `\` 与 POSIX `/`;去尾部分隔符)。
/// 纯函数,便于单测。例:`E:\apps\web` → `web`;`/opt/data/` → `data`。
pub fn local_basename(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return None;
    }
    let last = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .trim()
        .to_string();
    if last.is_empty() || last == "." || last == ".." {
        None
    } else {
        Some(last)
    }
}

/// 单镜像部署使用的远端 compose 文件及 override 文件名。
struct SingleComposeTarget {
    remote_file: String,
    override_names: Vec<String>,
}

/// 准备单镜像部署所需的 compose 文件。
///
/// 先按 [`single_compose_target`] 推演远端 compose 路径,导入项目(本地副本
/// 存在)再上传副本;旧版手工项目把它作为远端路径保存,无需上传。
/// 断点续传跳过步骤 4 时只调 [`single_compose_target`](不再重复上传)。
async fn prepare_single_compose(
    app: &AppHandle,
    client: &mut SshClient,
    server: &ServerConfig,
    project: &ProjectConfig,
) -> Result<SingleComposeTarget, String> {
    let target = single_compose_target(server, project)?;
    let local = PathBuf::from(&project.compose_file);
    if local.is_file() {
        upload_compose_files(app, client, server, project).await?;
    }
    Ok(target)
}

/// 推演单镜像部署使用的远端 compose 文件及 override 文件名(纯路径推演,
/// 不上传、不建连):导入项目的 `compose_file` 是本地副本路径 → 远端使用
/// 根目录副本;旧版手工项目则把它作为远端路径保存,继续直接使用。
/// 不存在的 Windows 盘符路径明确报错,避免把本机路径拼进 SSH 命令。
fn single_compose_target(
    server: &ServerConfig,
    project: &ProjectConfig,
) -> Result<SingleComposeTarget, String> {
    if project.compose_file.trim().is_empty() {
        return Err(format!("项目「{}」未配置 compose 文件", project.name));
    }

    let local = PathBuf::from(&project.compose_file);
    if local.is_file() {
        return Ok(SingleComposeTarget {
            remote_file: remote_compose_path(&effective_remote_dir(server, project)),
            override_names: compose_override_names(&project.compose_file),
        });
    }

    if is_windows_absolute_path(&project.compose_file) {
        return Err(format!(
            "本地 compose 文件不存在:{};请确认路径或重新导入 compose 文件",
            project.compose_file
        ));
    }

    Ok(SingleComposeTarget {
        remote_file: project.compose_file.clone(),
        override_names: Vec::new(),
    })
}

/// 判断 Windows 盘符绝对路径或 UNC 路径,不把它误当作远端路径。
pub(crate) fn is_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/'))
        || path.starts_with("\\\\")
}

/// 步骤 5:服务器部署 —— `docker load` → 同步原标签 → `compose up -d` → 健康检查 → 部署后钩子 → 删除远端 tar。
///
/// 每条命令超时 600 秒(清理 60 秒),输出实时转发到 `deploy-log`,收到输出行时检查取消标志。
/// `tar_name` 为 `None`(智能传输判定镜像未变化,无本地包)时跳过装载与 tar
/// 清理 —— 远端已是同 tag 同 ID 的镜像,compose up 即可完成回退。
/// `retag` 为 `Some((日期tag, 原引用))` 时,装载后把原引用(如 myapp:latest)也指向
/// 新镜像,否则 compose 引用原 tag 时感知不到变化、不会重建容器。
/// `idempotent_load_ref` 为 `Some(镜像引用)`(断点续传)时,装载前先
/// `docker image inspect` 远端 —— 已存在(上次装载成功后失败)则跳过 load。
/// up 之后先做健康检查(未启用则跳过),再执行部署后钩子(失败仅告警)。
// 参数本就偏多(阶段六新增 idempotent_load_ref 后 9 个),保持平铺签名、
// 显式关闭 clippy 提示(避免为消警重构签名扩大 diff)。
#[allow(clippy::too_many_arguments)]
async fn server_deploy(
    app: &AppHandle,
    client: &mut SshClient,
    server: &ServerConfig,
    project: &ProjectConfig,
    remote_compose: &str,
    override_names: &[String],
    tar_name: Option<&str>,
    retag: Option<(String, String)>,
    idempotent_load_ref: Option<&str>,
) -> Result<(), String> {
    // 5.1 加载镜像(镜像未变化时跳过:ID 已在远端,无需 load)
    match tar_name {
        Some(tar_name) => {
            // 断点续传:远端已有该镜像(上次装载成功后才失败)→ 跳过,逐次幂等
            let mut loaded = false;
            if let Some(image_ref) = idempotent_load_ref {
                if remote_has_image(client, image_ref).await? {
                    emit_log(
                        app,
                        &format!(
                            "断点续传:远端已存在镜像 {},跳过 docker load",
                            image_ref
                        ),
                    );
                    loaded = true;
                }
            }
            if !loaded {
                let remote_tar = remote_join("/tmp", tar_name);
                emit_log(app, &format!("加载镜像到服务器: docker load -i {}", remote_tar));
                let load_cmd = format!("docker load -i {}", shell_single_quote(&remote_tar));
                exec_forwarded(app, client, &load_cmd, 600).await?;
            }
        }
        None => emit_log(app, "镜像与远端一致,跳过 docker load(远端已是该镜像)"),
    }

    // 5.2 同步原标签(仅勾选日期标签时):零拷贝的指针移动,让 compose 的变更检测生效
    if let Some((date_tag, original)) = &retag {
        let tag_cmd = docker_tag_cmd(date_tag, original);
        emit_log(app, &format!("同步原标签: {}", tag_cmd));
        exec_forwarded(app, client, &tag_cmd, 60).await?;
    }

    // 5.3 启动服务:这里只使用已解析的远端 compose 路径
    let up_cmd = format!(
        "cd {} && docker compose -f {} up -d",
        shell_single_quote(&effective_remote_dir(server, project)),
        shell_single_quote(remote_compose),
    );
    emit_log(
        app,
        &format!(
            "启动服务: cd {} && docker compose -f {} up -d",
            effective_remote_dir(server, project), remote_compose
        ),
    );
    exec_forwarded(app, client, &up_cmd, 600).await?;

    // 5.4 健康检查(up 后按预算轮询服务状态;health_wait_secs=0 时跳过)
    health_check(
        app,
        client,
        project,
        &effective_remote_dir(server, project),
        remote_compose,
        override_names,
    )
    .await?;

    // 5.5 部署后钩子(健康检查通过后执行;失败仅告警,不影响部署结果)
    run_hook(app, client, project, HookKind::Post, &effective_remote_dir(server, project)).await?;

    // 5.6 清理远端 tar(尽力而为,失败不影响部署结果;未装载时无 tar 可清理)
    if let Some(tar_name) = tar_name {
        let remote_tar = remote_join("/tmp", tar_name);
        let rm_cmd = format!("rm -f {}", shell_single_quote(&remote_tar));
        if let Err(e) = exec_forwarded(app, client, &rm_cmd, 60).await {
            emit_log(app, &format!("警告:清理远端临时文件失败: {}", e));
        }
    }
    Ok(())
}

/// 断点续传的装载幂等检查:远端是否已存在该镜像引用
/// (`docker image inspect <ref>` 退出码 0 = 存在,即上次装载已成功)。
/// 传输层失败照常以 `Err` 传播(连接已坏,后续步骤必然失败)。
async fn remote_has_image(client: &mut SshClient, image_ref: &str) -> Result<bool, String> {
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "检查远端镜像超时",
        "请检查服务器网络后重试",
        exec_collect(client, &docker_inspect_cmd(image_ref)),
    )
    .await?;
    Ok(code == 0)
}

// ===== 部署钩子 + 健康检查(Task 3)=====

/// 部署钩子类型:`Pre` = 部署前(装载前,旧容器仍在运行,失败中止部署),
/// `Post` = 部署后(健康检查通过后,失败仅告警)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookKind {
    Pre,
    Post,
}

impl HookKind {
    /// 日志中的中文名称。
    fn label(self) -> &'static str {
        match self {
            HookKind::Pre => "部署前钩子",
            HookKind::Post => "部署后钩子",
        }
    }

    /// 项目配置里对应的钩子命令(未配置为 `None`)。
    fn cmd_of<'a>(self, project: &'a ProjectConfig) -> Option<&'a str> {
        match self {
            HookKind::Pre => project.pre_deploy_cmd.as_deref(),
            HookKind::Post => project.post_deploy_cmd.as_deref(),
        }
    }
}

/// 钩子执行失败的结果映射(纯函数,便于单测)。
///
/// - 取消(错误码 canceled,第十六批)一律原样透传(标记随串保留),Pre/Post
///   同口径(与 pull 步骤一致):收尾按 `code_of` 判定 cancel 事件,包装成
///   其他文案会判定失配,把用户取消误报为部署失败(通知走 failure);
/// - `Pre` 其余失败:包装为「<钩子名>执行失败,部署中止:<原因>」(剥标记展示);
/// - `Post` 其余失败:返回 `None`,由调用方仅告警、不影响部署结果。
pub(crate) fn hook_failure_result(which: HookKind, e: &str) -> Option<String> {
    if crate::errors::code_of(e) == Some(crate::errors::ErrCode::Cancelled) {
        return Some(e.to_string());
    }
    match which {
        HookKind::Pre => Some(format!(
            "{}执行失败,部署中止: {}",
            which.label(),
            crate::errors::strip(e)
        )),
        HookKind::Post => None,
    }
}

/// 执行项目的部署前/后钩子命令(远端执行,可选)。
///
/// - 未配置或空白 → 直接返回 `Ok(())`;
/// - 命令以 [`hook_cmd`](`cd '<remote_dir>' && ( <cmd> )`)执行,超时
///   [`HOOK_TIMEOUT_SECS`] 秒,输出实时转发到 `deploy-log`;
/// - `Pre` 失败 → `Err` 中止部署(此时旧容器仍在运行,尚未 load/up);
/// - `Post` 失败 → 仅告警并返回 `Ok(())`(不影响部署结果);取消除外。
async fn run_hook(
    app: &AppHandle,
    client: &mut SshClient,
    project: &ProjectConfig,
    which: HookKind,
    remote_dir: &str,
) -> Result<(), String> {
    let cmd = match which.cmd_of(project).map(str::trim) {
        Some(c) if !c.is_empty() => c,
        _ => return Ok(()),
    };
    let full_cmd = hook_cmd(remote_dir, cmd);
    emit_log(app, &format!("执行{}: {}", which.label(), cmd));
    match exec_forwarded(app, client, &full_cmd, HOOK_TIMEOUT_SECS).await {
        Ok(()) => {
            emit_log(app, &format!("{}执行完成", which.label()));
            Ok(())
        }
        Err(e) => match hook_failure_result(which, &e) {
            Some(err) => Err(err),
            None => {
                emit_log(
                    app,
                    &format!("警告:{}执行失败(不影响部署结果): {}", which.label(), e),
                );
                Ok(())
            }
        },
    }
}

/// up 之后的健康检查:`health_wait_secs > 0` 时,每 [`HEALTH_POLL_INTERVAL_SECS`]
/// 秒轮询一次 [`compose_ps_json_cmd`](`docker compose ps --all --format json`),
/// 预算为 `health_wait_secs` 秒;判定见 [`health_verdict`]。
///
/// `overrides` 为与 pull/up 同源检测的 override 文件名列表([`compose_override_names`]
/// → [`upload_compose_files`] 上传的同一批文件):逐个追加 `-f`,保证 override-only
/// 的服务同样出现在 ps 输出中、不逃逸健康判定。
///
/// - 全部服务 running 且(无 healthcheck 或 healthy)→ 通过;
/// - 任一服务 Restarting/Dead(或 Exited 且退出码非零/字段缺失)→ 立即失败;
///   Exited 且退出码 0(一次性服务正常退出)→ 不算失败,继续轮询,预算耗尽
///   报错并注明"若为一次性初始化服务请关闭健康检查";
/// - 解析不出状态(旧版 compose 输出、查询暂时失败等)→ 继续轮询至预算耗尽;
/// - 失败时先经 [`dump_compose_logs`] 拉取各服务最近日志进部署日志,再以中文
///   错误中止(`健康检查未通过:<服务> <状态>`)。
/// - `health_wait_secs == 0` → 未启用,直接跳过。
async fn health_check(
    app: &AppHandle,
    client: &mut SshClient,
    project: &ProjectConfig,
    remote_dir: &str,
    compose_file: &str,
    overrides: &[String],
) -> Result<(), String> {
    if project.health_wait_secs == 0 {
        emit_log(app, "健康检查未启用,跳过");
        return Ok(());
    }
    let budget = Duration::from_secs(project.health_wait_secs as u64);
    let started = std::time::Instant::now();
    let ps_cmd = compose_ps_json_cmd(remote_dir, compose_file, overrides);
    emit_log(
        app,
        &format!(
            "健康检查:开始轮询服务状态(每 {} 秒一轮,预算 {} 秒)",
            HEALTH_POLL_INTERVAL_SECS, project.health_wait_secs
        ),
    );
    // 最近一轮"尚未就绪"的服务与状态(预算耗尽时报错展示);
    // last_exited_zero:该服务是否"已退出(退出码 0)"(一次性服务提示)
    let mut last_pending: Option<(String, String)> = None;
    let mut last_exited_zero = false;
    loop {
        ensure_not_cancelled(app)?;
        // 单轮查询:60 秒超时;查询超时或 SSH 传输失败都按"无法判定"
        // 继续轮询(不直接失败,与解析不出的处理一致)
        let out = match with_timeout(
            HEALTH_PS_TIMEOUT_SECS,
            "健康检查状态查询超时",
            "请检查服务器网络后重试",
            exec_collect(client, &ps_cmd),
        )
        .await
        {
            Ok((_, out)) => out,
            Err(e) => {
                emit_log(app, &format!("警告:{}(继续等待)", e));
                String::new()
            }
        };
        let lines: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        match health_verdict(&lines) {
            HealthVerdict::Pass => {
                emit_log(
                    app,
                    &format!("健康检查通过(耗时 {} 秒)", started.elapsed().as_secs()),
                );
                return Ok(());
            }
            HealthVerdict::Unhealthy { service, state } => {
                dump_compose_logs(app, client, remote_dir, compose_file, overrides).await;
                return Err(format!("健康检查未通过:{} {}", service, state));
            }
            HealthVerdict::Indeterminate { pending, exited_zero } => {
                if let Some(p) = pending {
                    if last_pending.as_ref() != Some(&p) {
                        emit_log(app, &format!("健康检查:{} 尚未就绪({})", p.0, p.1));
                    }
                    last_pending = Some(p);
                    last_exited_zero = exited_zero;
                }
            }
        }
        if started.elapsed() >= budget {
            dump_compose_logs(app, client, remote_dir, compose_file, overrides).await;
            return Err(match (&last_pending, last_exited_zero) {
                // 一次性服务已正常退出:报错注明,提示关闭健康检查
                (Some((service, state)), true) => format!(
                    "健康检查未通过:{} {},若为一次性初始化服务请关闭健康检查(等待 {} 秒超时)",
                    service, state, project.health_wait_secs
                ),
                (Some((service, state)), false) => format!(
                    "健康检查未通过:{} {}(等待 {} 秒超时)",
                    service, state, project.health_wait_secs
                ),
                (None, _) => format!(
                    "健康检查未通过:无法获取服务状态(等待 {} 秒超时)",
                    project.health_wait_secs
                ),
            });
        }
        // 轮间取消检查后按固定间隔进入下一轮
        ensure_not_cancelled(app)?;
        tokio::time::sleep(Duration::from_secs(HEALTH_POLL_INTERVAL_SECS)).await;
    }
}

/// 健康检查失败时,拉取各服务最近 50 行日志并逐行转发到 `deploy-log`
/// (尽力而为:获取失败仅告警,不掩盖原始的健康检查错误)。
/// `overrides` 与健康检查的 ps 查询同源,保证 override-only 服务日志可查。
async fn dump_compose_logs(
    app: &AppHandle,
    client: &mut SshClient,
    remote_dir: &str,
    compose_file: &str,
    overrides: &[String],
) {
    emit_log(app, "正在获取服务最近日志(最后 50 行):");
    let cmd = compose_logs_cmd(remote_dir, compose_file, overrides);
    if let Err(e) = exec_forwarded(app, client, &cmd, SSH_EXEC_TIMEOUT_SECS).await {
        emit_log(app, &format!("警告:获取服务日志失败: {}", e));
    }
}

/// 远端磁盘预检(两条部署管线共用):查询 Docker 数据根目录所在盘剩余空间,
/// 与 `need_bytes`(已含 ×1.5 余量)比对,不足则以中文错误中止管线。
///
/// 容错(仅 emit 告警后跳过,不硬性拦截):`need_bytes` 为 `None`(本地镜像大小
/// 未知)、Docker 根目录查询失败/为空、df 输出解析失败(BusyBox 等口径不一致)。
/// SSH 传输层错误(通道打不开等)照常以 `Err` 传播 —— 连接已坏,上传必然失败。
async fn remote_disk_precheck(
    app: &AppHandle,
    client: &mut SshClient,
    need_bytes: Option<u64>,
) -> Result<(), String> {
    let need = match need_bytes {
        Some(v) => v,
        None => {
            emit_log(app, "警告:无法获取本地镜像大小,跳过服务器磁盘剩余空间检查");
            return Ok(());
        }
    };

    // 1. Docker 数据根目录(单行 trim)
    let (code, out) = exec_collect(client, &docker_root_cmd()).await?;
    let root = out.trim().to_string();
    if code != 0 || root.is_empty() {
        emit_log(
            app,
            &format!(
                "警告:无法获取 Docker 根目录(退出码 {}),跳过服务器磁盘剩余空间检查",
                code
            ),
        );
        return Ok(());
    }

    // 2. 根目录所在盘剩余空间(GB)
    let (code, out) = exec_collect(client, &df_free_gb_cmd(&root)).await?;
    let free_gb = if code == 0 { parse_df_gb(&out) } else { None };
    let free = match free_gb {
        Some(v) => v,
        None => {
            emit_log(
                app,
                &format!(
                    "警告:服务器磁盘剩余空间查询失败(退出码 {},df 输出: {:?}),跳过磁盘剩余空间检查",
                    code,
                    out.trim()
                ),
            );
            return Ok(());
        }
    };

    // 3. 判定(不足 → 中文报错中止)
    precheck_remote_disk(Some(free), need)?;
    emit_log(
        app,
        &format!(
            "服务器磁盘剩余空间检查通过:Docker 根目录 {} 所在盘剩余 {:.1} GB,本次部署约需 {:.1} GB",
            root,
            free,
            need as f64 / 1024.0 / 1024.0 / 1024.0
        ),
    );
    Ok(())
}

// ===== 智能传输(跳过未变化镜像)=====

/// 智能传输:查询远端镜像列表并构建 `repo:tag` → 镜像 ID 映射。
///
/// 使用 [`REMOTE_IMAGES_CMD_FULL`](完整 64 位 ID 口径,与本地
/// [`crate::docker::image_id_by_ref`] 同口径,跳过判定 [`same_image_id`] 才能
/// 相等;预览走 [`REMOTE_IMAGES_CMD`] 的 12 位截断口径,两侧独立);
/// 行解析复用 [`parse_image_lines`](单行解析失败仅告警跳过);退出码非 0
/// (远端 Docker 不可用等)以中文错误返回,由调用方决定中止或降级。
/// 单镜像 / 整栈两条部署管线共用的对比数据源。
pub(crate) async fn query_remote_image_id_map(
    client: &mut SshClient,
) -> Result<HashMap<String, String>, String> {
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询远端镜像超时",
        "请检查服务器网络后重试",
        exec_collect(client, REMOTE_IMAGES_CMD_FULL),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "查询远端镜像列表失败(退出码 {}),请确认服务器 Docker 可用",
            code
        ));
    }
    Ok(parse_image_lines(&out)
        .into_iter()
        .map(|i| (format!("{}:{}", i.repository, i.tag), i.id))
        .collect())
}

// ===== 整栈部署管线(六步,任一步失败即中止)=====

/// 整栈部署管线入口:组装部署历史记录骨架(含开始计时),执行管线主体,
/// 出口填充 success/message/duration 后连同结果与 webhook 通知地址一起返回
/// (由 [`finish_deploy_run`] 落历史、发通知)。
///
/// `resume` 为 `Some`(断点续传入口 [`deploy_resume_start`])时,管线从断点的
/// `step_next` 起跳步执行并对已完成产物幂等化复用;正常部署传 `None`(行为不变)。
/// `checkpoint = false`(批量部署)时断点落盘整体关闭(见
/// [`run_deploy_stack_steps`] 的 `checkpoint` 参数)。
async fn run_deploy_stack(
    app: &AppHandle,
    req: StackDeployRequest,
    resume: Option<ResumeContext>,
    checkpoint: bool,
) -> (Result<(), String>, DeployRecord, Option<String>) {
    let started = std::time::Instant::now();
    // webhook 通知地址:项目配置了 notify_webhook 才发(前置失败的路径取不到,为 None)
    let webhook_url = project_webhook_url(&req.project_id);
    // 骨架:镜像列表取本地传输的服务镜像;server/project 名称由前置解析回填
    let mut record = DeployRecord::new_skeleton(
        MODE_STACK,
        &req.server_id,
        &req.project_id,
        stack_record_images(&req.services),
    );
    let result = run_deploy_stack_steps(app, req, &mut record, resume, checkpoint).await;
    record.success = result.is_ok();
    record.message = match &result {
        Ok(()) => "部署完成".to_string(),
        Err(e) => e.clone(),
    };
    record.duration_secs = started.elapsed().as_secs();
    (result, record, webhook_url)
}

/// 整栈部署管线主体(六步,任一步失败即中止)。`record` 为组装中的部署历史
/// 记录,前置解析后回填服务器/项目名称。
///
/// 断点续传(`resume`,UPGRADE-PLAN 阶段六):`resume.step_next` 之后的步骤
/// 才执行;`checkpoint = true` 时每个步骤完成的收尾处落盘断点(成功后清除,
/// 失败/取消保留)。续传时服务分类与智能传输判定结果均以断点为准(**不重跑**
/// 判定 —— 远端状态已被上次部署部分改变,重放才确定),已上传的镜像包按
/// 远端文件大小校验跳过、已装载的镜像按 `docker image inspect` 跳过,
/// 发布目录复用断点记录的时间戳。
async fn run_deploy_stack_steps(
    app: &AppHandle,
    req: StackDeployRequest,
    record: &mut DeployRecord,
    resume: Option<ResumeContext>,
    checkpoint: bool,
) -> Result<(), String> {
    // ---- 前置:找 server/project、解析密码 ----
    // 每次部署开始时重置取消标志(与单镜像 run_deploy 一致)
    reset_cancelled(app);

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &req.server_id)?.clone();
    let project = find_project(&cfg, &req.project_id)?.clone();
    record.server_name = server.name.clone();
    record.project_name = project.name.clone();
    record.server_id = Some(server.id.clone());
    record.project_id = Some(project.id.clone());
    let password = resolve_password(
        &server.auth.auth_type,
        req.password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    // 托盘 tooltip(第十五批):整栈「部署中」运行态(同单镜像口径)
    {
        // 有前缀 = 死代码批量路径专用;线上批量每台是独立单发任务,前缀为空
        let batch_hint = DeployEventCtx::log_prefix().trim().to_string();
        crate::tray_status::set_deploy_running(
            app,
            true,
            0,
            0,
            &format!("{} / {}", server.name, project.name),
            &batch_hint,
        );
    }

    // 断点续传:落盘键与产物上下文(续传以断点产物为准,正常部署从请求构造)
    // 键优先取断点上下文(清理与加载指向同一键);正常部署按请求现算
    let key = match &resume {
        Some(r) => r.key.clone(),
        None => checkpoint_key(&req.server_id, &req.project_id, MODE_STACK),
    };
    let resume_step = resume.as_ref().map(|r| r.step_next).unwrap_or(1);
    let mut art = match &resume {
        Some(r) => r.stack.clone(),
        None => StackResumeArtifacts {
            services: req.services.clone(),
            skip_unchanged: req.skip_unchanged.unwrap_or(false),
            force_archive: req.force_archive.unwrap_or(false),
            ..Default::default()
        },
    };
    // 服务分类列表:续传时以断点为准(前端未参与续传,重放上次确认的分类)
    let services = match &resume {
        Some(r) => &r.stack.services,
        None => &req.services,
    };
    let (local_choices, pull_choices) = group_by_mode(services);
    if let Some(r) = &resume {
        emit_log(
            app,
            &format!(
                "断点续传:从步骤 {}({})继续整栈部署",
                r.step_next,
                resume_step_label(MODE_STACK, r.step_next)
            ),
        );
    }
    // 初始断点(步骤 1 前落盘,同键新部署覆盖旧断点;续传不重置起点)
    if checkpoint && resume.is_none() {
        checkpoint_save(&key, MODE_STACK, 1, &server, &project, artifacts_value(&art));
    }

    // ---- 步骤 1:分类确认 ----
    let skip_unchanged = req.skip_unchanged.unwrap_or(false);
    let force_archive = req.force_archive.unwrap_or(false);
    let mut unchanged: Vec<bool> = vec![false; local_choices.len()];
    if resume_step > 1 {
        emit_log(app, "断点续传:跳过步骤 1(分类确认)");
        // 恢复智能传输判定结果(与 Local 服务顺序对齐;不重跑判定,见函数文档)
        unchanged = art.unchanged.clone();
        if unchanged.len() != local_choices.len() {
            return Err(
                "断点数据损坏:智能传输判定结果与当前服务分类不一致,请放弃该断点后重新部署"
                    .to_string(),
            );
        }
    } else {
        emit_progress(app, 1, 6, "分类确认");
        ensure_not_cancelled(app)?;
        validate_stack_choices(services)?;
        // compose 本地副本必须存在:step 3 要上传到服务器,远端 `docker compose -f` 指向它
        if project.compose_file.trim().is_empty() {
            return Err(format!("项目「{}」未配置 compose 文件", project.name));
        }
        if !Path::new(&project.compose_file).is_file() {
            return Err(format!("compose 文件不存在:{}", project.compose_file));
        }
        emit_log(
            app,
            &format!(
                "开始整栈部署:服务器「{}」/ 项目「{}」,共 {} 个服务(本地传输 {} 个,服务器拉取 {} 个)",
                server.name,
                project.name,
                services.len(),
                local_choices.len(),
                pull_choices.len()
            ),
        );

        // ---- 智能传输:对比本地/远端同标签镜像 ID,标记未变化的 Local 服务 ----
        // 行为矩阵(skip_unchanged × force_archive,均缺省 false):
        // - 未启用:全部 Local 服务正常打包/上传/装载(与旧版本一致);
        // - skip=true, force=false:未变化服务从打包/上传/装载全链路剔除;
        // - skip=true, force=true:未变化服务仍打包上传留档(供回滚 load),
        //   仅跳过装载(其镜像 ID 已在远端)。
        let smart_transfer = skip_unchanged || force_archive;
        if smart_transfer && !local_choices.is_empty() {
            // 专用建连完成对比(用后即断,不占用打包阶段;与 deploy 的建连口径一致)
            emit_log(app, "智能传输:正在对比本地与远端镜像 ID…");
            let (_server, mut probe) =
                connect_server(&req.server_id, req.password_plain.as_deref(), None).await?;
            let remote_ids = query_remote_image_id_map(&mut probe).await?;
            for (i, svc) in local_choices.iter().enumerate() {
                let (repo, tag) = split_image_ref(&svc.image);
                let full_ref = format!("{}:{}", repo, tag);
                let (Some(remote_id), Ok(Some(local_id))) = (
                    remote_ids.get(&full_ref),
                    image_id_by_ref(&svc.image).await,
                ) else {
                    continue;
                };
                if same_image_id(remote_id, &local_id) {
                    unchanged[i] = true;
                    if force_archive {
                        emit_log(app, &format!("未变化,打包留档(跳过装载): {}", svc.image));
                    } else {
                        emit_log(app, &format!("未变化,跳过传输: {}", svc.image));
                    }
                }
            }
        }
        art.unchanged = unchanged.clone();
        if checkpoint {
            checkpoint_save(&key, MODE_STACK, 2, &server, &project, artifacts_value(&art));
        }
    }

    // 打包列表:skip 且非 force 时剔除未变化服务;其余情况保持全部 Local。
    // `pack_unchanged` 与打包列表按下标对齐,供装载步骤跳过留档的未变化镜像。
    // (断点续传时 unchanged 取自断点,过滤结果与上次打包顺序一致)
    let pack_list: Vec<&StackServiceChoice> = local_choices
        .iter()
        .enumerate()
        .filter(|(i, _)| force_archive || !unchanged[*i])
        .map(|(_, s)| *s)
        .collect();
    let pack_unchanged: Vec<bool> = pack_list
        .iter()
        .map(|s| {
            local_choices
                .iter()
                .position(|c| c.service == s.service)
                .map(|i| unchanged[i])
                .unwrap_or(false)
        })
        .collect();

    // ---- 步骤 2:打包 ----
    let tars = if resume_step > 2 {
        emit_log(app, "断点续传:跳过步骤 2(打包),复用已打包的镜像包");
        // 断点打包产物与过滤结果一致性校验(损坏 → 明确报错,避免错位装载)
        if art.files.len() != pack_list.len()
            || art.locals.len() != art.files.len()
            || art.images.len() != art.files.len()
        {
            return Err(
                "断点数据损坏:镜像包产物与当前服务分类不一致,请放弃该断点后重新部署".to_string(),
            );
        }
        // 复用的 tar 不挂 Drop 守卫:断点期保留,成功/放弃时显式清理
        LocalTars {
            files: art
                .files
                .iter()
                .cloned()
                .zip(art.locals.iter().map(PathBuf::from))
                .map(|(name, path)| (path, name))
                .collect(),
            _guards: Vec::new(),
        }
    } else {
        emit_progress(app, 2, 6, "打包");
        ensure_not_cancelled(app)?;
        let tars = if pack_list.is_empty() {
            if local_choices.is_empty() {
                emit_log(app, "所有服务均由服务器拉取镜像,跳过本地打包");
            } else {
                emit_log(app, "全部本地镜像均未变化,跳过打包与传输");
            }
            LocalTars {
                files: Vec::new(),
                _guards: Vec::new(),
            }
        } else {
            // 断点续传开启时保留本地 tar 供失败后复用(成功/放弃时显式清理)
            pack_local_images(app, &pack_list, checkpoint).await?
        };
        art.files = tars.files.iter().map(|(_, n)| n.clone()).collect();
        art.locals = tars
            .files
            .iter()
            .map(|(p, _)| p.to_string_lossy().to_string())
            .collect();
        art.images = pack_list.iter().map(|s| s.image.clone()).collect();
        if checkpoint {
            checkpoint_save(&key, MODE_STACK, 3, &server, &project, artifacts_value(&art));
        }
        tars
    };

    // manifest 镜像条目(整栈成功收尾写入 manifest.json):逐个 Local 服务一条,
    // 被跳过传输(未留档)的服务 file = null,其余按打包顺序携带镜像包文件名
    // (断点续传时 unchanged 来自断点,结果与首次部署一致)
    let skip_flags: Vec<bool> = unchanged.iter().map(|u| *u && !force_archive).collect();
    let packed_files: Vec<String> = tars.files.iter().map(|(_, n)| n.clone()).collect();
    let manifest_images = build_manifest_images(&local_choices, &skip_flags, &packed_files);

    // ---- 步骤 3:上传 ----
    // 断点续传:步骤 3 已完成时不再推送本步进度(事件从 step_next 起)
    if resume_step <= 3 {
        emit_progress(app, 3, 6, "上传");
    }
    ensure_not_cancelled(app)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    // 发布时间戳:断点续传复用断点记录的 ts(同一发布目录,已上传镜像包才能
    // 按名续传/按镜像幂等装载);正常部署每次新生成(旧行为)。续传且步骤 3
    // 已完成时断点必有 ts,缺失视为断点损坏。
    let ts = match art.release_ts.clone() {
        Some(ts) => ts,
        None if resume.is_some() && resume_step > 3 => {
            return Err(
                "断点数据损坏:缺少发布目录信息,请放弃该断点后重新部署".to_string(),
            )
        }
        None => chrono::Local::now().format("%Y%m%d-%H%M%S").to_string(),
    };
    let release_dir = releases_dir(&effective_remote_dir(&server, &project), &ts);

    if resume_step > 3 {
        // 断点续传:上次已完成上传 → 仅建连(后续步骤复用连接)
        emit_log(app, &format!("断点续传:跳过步骤 3(上传),复用发布目录 {}", release_dir));
    } else {
        // 远端磁盘预检:上传前确认 Docker 根目录所在盘剩余空间 ≥ Local 镜像字节总和 × 1.5
        // (与本地导出预检同一 sum 口径;大小未知 → 告警跳过;不足 → 中文报错中止)
        let local_sizes: Vec<Option<u64>> = local_choices
            .iter()
            .map(|s| image_size(&s.image))
            .collect();
        let need_bytes = sum_sizes(&local_sizes).map(|total| (total as f64 * 1.5) as u64);
        remote_disk_precheck(app, &mut client, need_bytes).await?;

        // 远端建本次发布目录 <remote_dir>/releases/<时间戳>/(mkdir -p 连带创建
        // remote_dir;断点续传复用同目录,mkdir -p 幂等)
        let mkdir_cmd = mkdir_p_cmd(&release_dir);
        let code = with_timeout(
            SSH_EXEC_TIMEOUT_SECS,
            "创建远端目录超时",
            "请检查服务器网络后重试",
            async {
                client
                    .exec(&mkdir_cmd, &mut |_| {})
                    .await
                    .map_err(|e| format!("远端创建目录失败: {}", e))
            },
        )
        .await?;
        if code != 0 {
            return Err(format!(
                "远端创建目录 {} 失败(退出码 {},常见原因:无写入权限)",
                release_dir, code
            ));
        }
        emit_log(app, &format!("本次发布目录: {}", release_dir));
        if art.release_ts.as_deref() != Some(ts.as_str()) {
            // 步骤 3 内部落盘:目录创建成功即记录 ts,上传中断后可复用同一发布目录
            art.release_ts = Some(ts.clone());
            if checkpoint {
                checkpoint_save(&key, MODE_STACK, 3, &server, &project, artifacts_value(&art));
            }
        }

        upload_compose_files(app, &mut client, &server, &project).await?;
        // 断点续传:逐包校验远端大小,已上传完成的包跳过(半成品包由
        // sftp_upload(resume=true) 续传)
        upload_local_tars(app, &mut client, &tars, &release_dir, resume.is_some()).await?;
        sync_files(app, &mut client, &server, &project).await?;
        emit_log(app, "上传完成");
        // 部署前钩子(归入步骤 3:装载/拉取前执行,旧容器仍在运行;失败即中止部署)
        run_hook(app, &mut client, &project, HookKind::Pre, &effective_remote_dir(&server, &project)).await?;
        if checkpoint {
            checkpoint_save(&key, MODE_STACK, 4, &server, &project, artifacts_value(&art));
        }
    }

    // ---- 步骤 4:装载 ----
    if resume_step > 4 {
        emit_log(app, "断点续传:跳过步骤 4(装载)");
    } else {
        emit_progress(app, 4, 6, "装载");
        ensure_not_cancelled(app)?;
        let tar_count = tars.files.len();
        if tar_count == 0 {
            emit_log(app, "无本地镜像包,跳过装载");
        }
        for (i, (_, name)) in tars.files.iter().enumerate() {
            ensure_not_cancelled(app)?;
            // force_archive 留档的未变化镜像:ID 已在远端,仅归档进 release 目录,不装载
            if pack_unchanged[i] {
                emit_log(app, &format!("镜像未变化,跳过装载(仅留档): {}", name));
                continue;
            }
            // 断点续传:该包上次装载已成功(远端已有该镜像)→ 跳过,逐包幂等
            if resume.is_some() && remote_has_image(&mut client, &art.images[i]).await? {
                emit_log(
                    app,
                    &format!(
                        "断点续传:远端已存在镜像 {},跳过装载: {}",
                        art.images[i], name
                    ),
                );
                continue;
            }
            let remote_tar = remote_join(&release_dir, name);
            emit_log(
                app,
                &format!(
                    "装载镜像包 ({}/{}): docker load -i {}",
                    i + 1,
                    tar_count,
                    remote_tar
                ),
            );
            let load_cmd = format!("docker load -i {}", shell_single_quote(&remote_tar));
            exec_forwarded(app, &mut client, &load_cmd, STACK_LOAD_TIMEOUT_SECS).await?;
        }
        if checkpoint {
            checkpoint_save(&key, MODE_STACK, 5, &server, &project, artifacts_value(&art));
        }
    }

    // ---- 步骤 5:拉取 ----
    // override 文件名:按 compose 副本目录检测(与 upload_compose_files 上传的
    // 一致),pull / up 均按同序 -f 传入,保证远端合并结果与本地解析一致
    // (跳过本步时 up 仍需要,故先于分支计算)
    let override_names = compose_override_names(&project.compose_file);
    if resume_step > 5 {
        emit_log(app, "断点续传:跳过步骤 5(拉取)");
    } else {
        emit_progress(app, 5, 6, "拉取");
        ensure_not_cancelled(app)?;
        let pull_names: Vec<String> = pull_choices.iter().map(|s| s.service.clone()).collect();
        if pull_names.is_empty() {
            emit_log(app, "无需要服务器拉取的服务,跳过拉取");
        } else {
            let remote_compose = remote_compose_path(&effective_remote_dir(&server, &project));
            let pull_cmd =
                compose_pull_cmd(&effective_remote_dir(&server, &project), &remote_compose, &override_names, &pull_names);
            emit_log(app, &format!("拉取远端镜像: {}", pull_cmd));
            // 远端输出末尾并入错误信息:私有仓库认证失败(401/Unauthorized/denied)
            // 时由 augment_pull_error 追加 docker login 提示
            exec_forwarded_inner(
                app,
                &mut client,
                &pull_cmd,
                STACK_COMPOSE_TIMEOUT_SECS,
                PULL_OUTPUT_TAIL_LINES,
            )
            .await
            .map_err(|e| {
                if crate::errors::code_of(&e) == Some(crate::errors::ErrCode::Cancelled) {
                    e
                } else {
                    augment_pull_error(&format!(
                        "{}(请检查服务器能否出网访问镜像仓库,或在服务分类中把这些服务改为本地传输)",
                        e
                    ))
                }
            })?;
        }
        if checkpoint {
            checkpoint_save(&key, MODE_STACK, 6, &server, &project, artifacts_value(&art));
        }
    }

    // ---- 步骤 6:启动 ----
    emit_progress(app, 6, 6, "启动");
    ensure_not_cancelled(app)?;
    let remote_compose = remote_compose_path(&effective_remote_dir(&server, &project));
    let up_cmd = compose_up_cmd(&effective_remote_dir(&server, &project), &remote_compose, &override_names);
    emit_log(app, &format!("启动服务: {}", up_cmd));
    exec_forwarded(app, &mut client, &up_cmd, STACK_COMPOSE_TIMEOUT_SECS).await?;

    // 健康检查(up 后按预算轮询服务状态;health_wait_secs=0 时跳过)
    // overrides 与 pull/up 同源,override-only 服务同样进入健康判定
    health_check(
        app,
        &mut client,
        &project,
        &effective_remote_dir(&server, &project),
        &remote_compose,
        &override_names,
    )
    .await?;

    // 部署后钩子(健康检查通过后执行;失败仅告警,不影响部署结果)
    run_hook(
        app,
        &mut client,
        &project,
        HookKind::Post,
        &effective_remote_dir(&server, &project),
    )
    .await?;

    // ---- 收尾:向发布目录写入回滚资料(manifest.json + compose 副本)----
    // 尽力而为:失败仅告警(缺 manifest/副本时回滚命令会优雅降级),不推翻
    // 已成功的部署结果;须在清理旧 releases 之前写入,避免目录先被清掉
    ensure_not_cancelled(app)?;
    let manifest = ReleaseManifest::new(project.name.clone(), ts.clone(), manifest_images);
    write_release_artifacts(app, &mut client, &project, &release_dir, &manifest).await;

    // ---- 清理旧 releases(保留该项目配置的数量,尽力而为,失败仅告警)----
    ensure_not_cancelled(app)?;
    let keep = crate::config::release_keep_of(&project);
    let cleanup_cmd = cleanup_releases_cmd(&effective_remote_dir(&server, &project), keep);
    emit_log(
        app,
        &format!("清理旧发布归档(保留最新 {} 个)", keep),
    );
    if let Err(e) = exec_forwarded(app, &mut client, &cleanup_cmd, SSH_EXEC_TIMEOUT_SECS).await {
        emit_log(app, &format!("警告:清理旧 releases 目录失败: {}", e));
    }

    // ---- 成功收尾:清除断点 + 删除断点期保留的本地临时 tar ----
    // (失败/取消不走这里:断点与临时 tar 都保留,供续传复用)
    if checkpoint {
        let local_tars: Vec<PathBuf> = tars.files.iter().map(|(p, _)| p.clone()).collect();
        checkpoint_cleanup_on_success(&key, &local_tars);
    }

    // 整栈成功:登记本次发布目录,供前端一键回滚定位
    record.release_dir = Some(release_dir.clone());
    emit_log(app, "整栈部署完成");
    Ok(())
}

/// 步骤 2 打包出的本地镜像包集合。
struct LocalTars {
    /// `(本地路径, 远端文件名)`,按服务顺序排列(与串行打包时的顺序一致)
    files: Vec<(PathBuf, String)>,
    /// Drop 守卫:管线函数返回(成功或失败)时删除全部本地 tar
    /// (断点续传开启时以 keep 模式构造 —— 不删除,成功/放弃时显式清理;
    /// 断点续传复用的 tar 不挂守卫)
    _guards: Vec<TempFileGuard>,
}

/// 步骤 2:把 Local 类镜像并发导出为 gzip 压缩包(`temp_dir/<uuid>.tar.gz`)。
///
/// 磁盘预检保持打包前一次性(全部 Local 镜像大小求和 ×1.5,复用
/// [`check_export_disk_space`];有镜像大小未知则跳过预检并告警)。
/// 列表为空(全 Pull)时返回空集合。
///
/// 并发打包:并发度 = [`PACK_CONCURRENCY_CAP`] 与可用并行度的较小值,
/// 用 `tokio::task::JoinSet` 保活至多 N 个任务、完成一个补位一个
/// (阻塞型 `save_gzip` 经 [`export_image_silent`] 在 blocking 线程池执行);
/// 结果按服务顺序回填,`files` 顺序与串行版一致。
///
/// 进度与取消:每完成一个镜像 emit 一次 `deploy-log`(“打包完成 (i/n)”)并
/// 检查一次取消;取消或出错后不再启动新任务,但**已启动的阻塞 `docker save`
/// 无法中断**,只能等其在途任务自然结束后以“部署已取消”/首个错误中止。
/// 输出路径的 [`TempFileGuard`] 预先建立,任何返回路径(成功/失败/取消)下
/// 半成品 tar 都随管线返回统一删除;`keep_guards = true`(断点续传开启)时
/// 守卫以 keep 模式构造 —— tar 保留给失败后续传复用,成功/放弃时显式清理。
async fn pack_local_images(
    app: &AppHandle,
    local: &[&StackServiceChoice],
    keep_guards: bool,
) -> Result<LocalTars, String> {
    if local.is_empty() {
        emit_log(app, "所有服务均由服务器拉取镜像,跳过本地打包");
        return Ok(LocalTars {
            files: Vec::new(),
            _guards: Vec::new(),
        });
    }

    let sizes: Vec<Option<u64>> = local.iter().map(|s| image_size(&s.image)).collect();
    match sum_sizes(&sizes) {
        Some(total) => check_export_disk_space(total)?,
        None => emit_log(app, "警告:无法获取部分镜像大小,跳过磁盘剩余空间检查"),
    }

    let n = local.len();
    // guard 先建:导出失败的半成品文件同样会在管线返回时删除
    // (断点续传开启时保留,供失败后复用)
    let mut outputs: Vec<(PathBuf, String)> = Vec::with_capacity(n);
    for _ in 0..n {
        let tar_name = format!("{}.tar.gz", uuid::Uuid::new_v4());
        let out_path = std::env::temp_dir().join(&tar_name);
        outputs.push((out_path, tar_name));
    }
    let guards: Vec<TempFileGuard> = outputs
        .iter()
        .map(|(p, _)| {
            if keep_guards {
                TempFileGuard::keep(p.clone())
            } else {
                TempFileGuard::new(p.clone())
            }
        })
        .collect();
    let images: Vec<String> = local.iter().map(|s| s.image.clone()).collect();

    let available = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    let concurrency = PACK_CONCURRENCY_CAP.min(available).max(1);
    emit_log(
        app,
        &format!("并行打包 {} 个本地镜像包(并发 {})", n, concurrency),
    );

    let mut set: tokio::task::JoinSet<(usize, Result<u64, String>)> = tokio::task::JoinSet::new();
    // 启动第一批(至多 concurrency 个);此后每完成一个补位一个
    let mut next = 0usize;
    while next < n && set.len() < concurrency {
        set.spawn(spawn_pack_job(next, images[next].clone(), outputs[next].0.clone()));
        next += 1;
    }

    let mut first_error: Option<String> = None;
    let mut cancelled = false;
    let mut done = 0usize;
    while let Some(joined) = set.join_next().await {
        let (idx, res) = match joined {
            Ok(pair) => pair,
            // 外层包装任务自身 panic(理论上不可能,防御性兜底)
            Err(e) => (usize::MAX, Err(format!("打包任务异常终止: {}", e))),
        };
        match res {
            Ok(bytes) => {
                done += 1;
                if let Some(name) = images.get(idx) {
                    emit_log(
                        app,
                        &format!(
                            "打包完成 ({}/{}): {} (共 {} MB)",
                            done,
                            n,
                            name,
                            bytes / 1024 / 1024
                        ),
                    );
                }
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
        // 每完成一个检查一次取消;取消/出错后不再启动新任务
        if is_cancelled(app) {
            cancelled = true;
        }
        if cancelled || first_error.is_some() {
            continue;
        }
        if next < n {
            set.spawn(spawn_pack_job(next, images[next].clone(), outputs[next].0.clone()));
            next += 1;
        }
    }

    if cancelled {
        return Err(crate::errors::cancelled());
    }
    if let Some(e) = first_error {
        return Err(e);
    }
    Ok(LocalTars {
        files: outputs,
        _guards: guards,
    })
}

/// 启动一个静默打包任务:并发导出 `image` 到 `out_path`,返回 `(任务序号, 结果)`。
/// (外层 async 任务包装保证内层 blocking 任务 panic 也带序号转为 `Err`,
/// 便于定位失败的是哪个镜像。)
fn spawn_pack_job(
    idx: usize,
    image: String,
    out_path: PathBuf,
) -> impl std::future::Future<Output = (usize, Result<u64, String>)> + Send + 'static {
    async move {
        let res = export_image_silent(&image, &out_path).await;
        (idx, res)
    }
}

/// 步骤 3 子步:上传 compose 副本(及同目录 `.env`、override 文件,若存在)
/// 到远端根目录。
///
/// 远端 `docker compose -f` 指向这份副本,服务器上没有它无法启动,
/// 故先于镜像包上传,失败尽早暴露。`.env` 供服务器端 compose 变量插值;
/// override 文件按副本目录 [`find_override_files`] 检测、同名 basename 上传,
/// 供 pull / up 按同序追加 `-f`(与本地解析合并一致)。
async fn upload_compose_files(
    app: &AppHandle,
    client: &mut SshClient,
    server: &ServerConfig,
    project: &ProjectConfig,
) -> Result<(), String> {
    let compose_local = PathBuf::from(&project.compose_file);
    let compose_name = "docker-compose.yml";
    emit_log(
        app,
        &format!(
            "上传 compose 文件: {} -> {}",
            project.compose_file,
            remote_join(&effective_remote_dir(server, project), compose_name)
        ),
    );
    // compose 副本 / .env / override 内容可变且远端同名,不做续传(全新写覆盖)
    client
        .sftp_upload(&compose_local, &effective_remote_dir(server, project), compose_name, false, &|_, _| {})
        .await?;
    if let Some(env_path) = compose_local.parent().map(|p| p.join(".env")) {
        if env_path.is_file() {
            emit_log(app, "上传 compose 同目录 .env 文件");
            client
                .sftp_upload(&env_path, &effective_remote_dir(server, project), ".env", false, &|_, _| {})
                .await?;
        }
    }
    for ov_path in find_override_files(compose_local.parent().unwrap_or_else(|| Path::new(""))) {
        let Some(name) = ov_path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        emit_log(
            app,
            &format!(
                "上传 override 文件: {} -> {}",
                ov_path.display(),
                remote_join(&effective_remote_dir(server, project), &name)
            ),
        );
        client
            .sftp_upload(&ov_path, &effective_remote_dir(server, project), &name, false, &|_, _| {})
            .await?;
    }
    Ok(())
}

/// 步骤 3 子步:逐包上传本地镜像包到远端 releases 目录(进度:包序号 + 字节,
/// 每 ≥5MB 变化汇报一次)。
///
/// 断点续传在「失败后同路径重试一次」时生效:每次部署 attempt 的 releases 目录
/// 均为新时间戳,attempt 之间无同名文件;同 attempt 内重试时 `sftp_upload(resume=true)`
/// 经 stat 命中远端半成品 → Resume 分支(重试返回 AlreadyDone = 远端已传 ≥ 本地,
/// 同样视为该包成功)。
///
/// `verify_remote = true`(断点续传,UPGRADE-PLAN 阶段六):逐包先经
/// [`SshClient::sftp_stat_size`] 校验 —— 远端同名文件不小于本地(该包上次已
/// 上传完成)→ 跳过;查询失败不跳过(交给下方上传,由其内部 stat 兜底判定)。
async fn upload_local_tars(
    app: &AppHandle,
    client: &mut SshClient,
    tars: &LocalTars,
    release_dir: &str,
    verify_remote: bool,
) -> Result<(), String> {
    let n = tars.files.len();
    if n == 0 {
        emit_log(app, "无本地镜像包需要上传");
        return Ok(());
    }
    for (i, (path, name)) in tars.files.iter().enumerate() {
        ensure_not_cancelled(app)?;
        // 断点续传:远端同名文件与本地大小一致 → 该包上次已上传完成,跳过
        if verify_remote {
            let local_size = tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0);
            if local_size > 0 {
                let remote_path = remote_join(release_dir, name);
                let uploaded = match client.sftp_stat_size(&remote_path).await {
                    Ok(Some(remote_size)) => remote_size >= local_size,
                    Ok(None) => false,
                    // 查询失败不跳过:交给下方上传兜底
                    Err(_) => false,
                };
                if uploaded {
                    emit_log(app, &format!("断点续传:镜像包 {} 已上传完成,跳过", name));
                    continue;
                }
            }
        }
        emit_log(app, &format!("上传镜像包 ({}/{}): {}", i + 1, n, name));
        let app_for_cb = app.clone();
        let idx = i + 1;
        let last = Arc::new(AtomicU64::new(0));
        let last_cb = Arc::clone(&last);
        let on_progress = move |sent, total| {
            if total == 0 {
                return;
            }
            if sent >= last_cb.load(Ordering::Relaxed) + LOG_PROGRESS_STEP {
                last_cb.store(sent, Ordering::Relaxed);
                emit_log(
                    &app_for_cb,
                    &format!(
                        "上传镜像包 ({}/{}): {} MB / {} MB",
                        idx,
                        n,
                        sent / 1024 / 1024,
                        total / 1024 / 1024
                    ),
                );
            }
        };
        // 失败后同路径重试一次(见 [`upload_retry_wait`];AlreadyDone 亦视为成功)
        let first_err = match client
            .sftp_upload(path, release_dir, name, true, &on_progress)
            .await
        {
            Ok(()) => continue,
            Err(e) => e,
        };
        upload_retry_wait(app).await?;
        match client
            .sftp_upload(path, release_dir, name, true, &on_progress)
            .await
        {
            Ok(()) => emit_log(app, &format!("镜像包 {} 断点续传重试成功", name)),
            Err(e) => return Err(upload_retry_failure_msg(&e, &first_err)),
        }
    }
    emit_log(app, &format!("镜像包上传完成,共 {} 个", n));
    Ok(())
}

// ===== 整栈部署纯逻辑(便于单测)=====

/// 步骤 1 的纯校验:服务分类列表非空;Local 类服务的镜像引用必须非空
/// (本地传输需要打包上传,没有镜像引用无法进行)。
pub fn validate_stack_choices(services: &[StackServiceChoice]) -> Result<(), String> {
    if services.is_empty() {
        return Err("服务分类列表为空,请先解析 compose 并确认各服务的传输分类".to_string());
    }
    for svc in services {
        if matches!(svc.mode, TransferMode::Local) && svc.image.trim().is_empty() {
            return Err(format!(
                "服务「{}」分类为本地传输但镜像为空,请在 compose 补 image: 字段,或将其改为服务器拉取",
                svc.service
            ));
        }
    }
    Ok(())
}

/// 按传输方式把服务分成 `(本地传输, 服务器拉取)` 两组,各自保持原顺序。
pub fn group_by_mode(
    services: &[StackServiceChoice],
) -> (Vec<&StackServiceChoice>, Vec<&StackServiceChoice>) {
    let mut local = Vec::new();
    let mut pull = Vec::new();
    for svc in services {
        match svc.mode {
            TransferMode::Local => local.push(svc),
            TransferMode::Pull => pull.push(svc),
        }
    }
    (local, pull)
}

/// 整栈部署历史记录的镜像列表:本地传输且镜像引用非空的服务镜像
/// (按服务顺序;Pull 类由服务器自拉,引用常为空,不计入)。
pub fn stack_record_images(services: &[StackServiceChoice]) -> Vec<String> {
    services
        .iter()
        .filter(|s| matches!(s.mode, TransferMode::Local) && !s.image.trim().is_empty())
        .map(|s| s.image.clone())
        .collect()
}

/// 求和一组镜像大小;任一项未知(`None`)或求和溢出则整体返回 `None`
/// (调用方跳过磁盘预检并告警)。
pub fn sum_sizes(sizes: &[Option<u64>]) -> Option<u64> {
    let mut total: u64 = 0;
    for size in sizes {
        total = total.checked_add((*size)?)?;
    }
    Some(total)
}

/// 拼装本次发布目录:`<remote_dir>/releases/<ts>`(ts 形如 20260829-101010)。
pub fn releases_dir(remote_dir: &str, ts: &str) -> String {
    remote_join(remote_dir, &format!("releases/{}", ts))
}

/// 远端 compose 文件路径(step 3 上传到远端根目录的副本)。
pub fn remote_compose_path(remote_dir: &str) -> String {
    remote_join(remote_dir, "docker-compose.yml")
}

/// 检测项目 compose 文件同目录的 override 文件,返回文件名(basename)列表
/// (按 compose 默认合并顺序;供远端 pull / up 的 `-f` 文件链使用,
/// 与 [`upload_compose_files`] 上传的 override 文件一致)。
pub(crate) fn compose_override_names(compose_file: &str) -> Vec<String> {
    let dir = Path::new(compose_file).parent().unwrap_or_else(|| Path::new(""));
    find_override_files(dir)
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .collect()
}

/// 拼装 releases 清理命令:按修改时间保留最新 `keep` 个版本目录,其余删除
/// (`tail -n +<keep+1>` 从第 keep+1 行起取;`xargs -r` 无输入时不执行 rm)。
///
/// `keep = 0` 时删除全部历史归档(`tail -n +1`,即整表)。
/// `keep` 由项目配置经 [`crate::config::release_keep_of`] 解析(默认 5 个)。
pub fn cleanup_releases_cmd(remote_dir: &str, keep: u32) -> String {
    format!(
        "ls -1dt {}/*/ | tail -n +{} | xargs -r rm -rf",
        shell_single_quote(&remote_join(remote_dir, "releases")),
        keep.saturating_add(1)
    )
}

/// 拼装 compose pull 命令;compose 文件与每个 override 文件(按检测顺序,
/// 后者覆盖前者)逐个 `-f` 传入,远端路径与服务名逐个单引号包裹防注入。
pub fn compose_pull_cmd(
    remote_dir: &str,
    compose_file: &str,
    overrides: &[String],
    services: &[String],
) -> String {
    let quoted: Vec<String> = services.iter().map(|s| shell_single_quote(s)).collect();
    format!(
        "cd {} && docker compose {} pull {}",
        shell_single_quote(remote_dir),
        compose_file_flags(compose_file, overrides),
        quoted.join(" ")
    )
}

/// 拼装 compose up 命令(后台启动全部服务;override 文件按序 `-f` 追加)。
pub fn compose_up_cmd(remote_dir: &str, compose_file: &str, overrides: &[String]) -> String {
    format!(
        "cd {} && docker compose {} up -d",
        shell_single_quote(remote_dir),
        compose_file_flags(compose_file, overrides)
    )
}

/// 拼装 `-f <base> -f <override>...` 片段(compose 按顺序合并,后者覆盖前者);
/// 文件路径逐个单引号包裹防注入。
pub(crate) fn compose_file_flags(compose_file: &str, overrides: &[String]) -> String {
    let mut flags = vec![format!("-f {}", shell_single_quote(compose_file))];
    flags.extend(
        overrides
            .iter()
            .map(|o| format!("-f {}", shell_single_quote(o))),
    );
    flags.join(" ")
}

/// pull 失败错误增强(纯函数):错误信息(含并入的远端输出末尾,见
/// [`PULL_OUTPUT_TAIL_LINES`])含 `401` / `Unauthorized` / `denied`
/// (不区分大小写)时,判定为私有仓库认证问题,在错误后追加服务器
/// docker login 提示;其余错误原样返回。
pub fn augment_pull_error(err: &str) -> String {
    let lower = err.to_ascii_lowercase();
    if lower.contains("401") || lower.contains("unauthorized") || lower.contains("denied") {
        format!(
            "{};检测到私有仓库认证问题,请先在服务器上 docker login 对应 registry",
            err
        )
    } else {
        err.to_string()
    }
}

/// 组装 `docker tag` 指针移动命令:让 target 引用与 source 引用指向同一镜像。
/// (零拷贝;target 已存在时覆盖其指向,旧镜像失去全部标签后成为悬空镜像。)
pub fn docker_tag_cmd(source: &str, target: &str) -> String {
    format!(
        "docker tag {} {}",
        shell_single_quote(source),
        shell_single_quote(target)
    )
}

// ===== 整栈部署预览(dry-run,Task 6,独立功能不接入部署流程)=====

