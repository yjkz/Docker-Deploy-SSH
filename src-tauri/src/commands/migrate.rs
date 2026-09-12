// // 跨服务器镜像迁移

use super::*;

/// 写一条 `mode = "migrate"` 的部署历史(项目迁移审计用)。
///
/// 迁移既不是部署也不是回滚,单独一种 mode:历史表据此显示徽标,且不提供
/// 「回滚」按钮(回滚语义对该记录无意义 —— 源服务器上的资产并未删除)。
pub fn append_migration_history(
    project_name: &str,
    source_name: &str,
    target_name: &str,
    images: &[String],
    warnings: &[String],
) {
    let mut record = DeployRecord::new_skeleton(
        MODE_MIGRATE,
        &format!("{} → {}", source_name, target_name),
        project_name,
        images.to_vec(),
    );
    record.success = true;
    record.message = if warnings.is_empty() {
        format!("已迁移到 {}", target_name)
    } else {
        format!("已迁移到 {}({} 条警告)", target_name, warnings.len())
    };
    crate::history::append_record(record);
}

/// `migrate-log` 事件载荷(camelCase,与批次二新增契约统一)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateLogEvent {
    /// 迁移代号:每次 start 递增;前端据此丢弃迟到旧迁移的事件。
    pub migrate_id: u64,
    /// 一行迁移日志(与 deploy-log 相同的 `[HH:MM:SS]` 时间戳前缀)。
    pub line: String,
}

/// `migrate-done` 事件载荷(camelCase)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateDoneEvent {
    pub migrate_id: u64,
    pub success: bool,
    /// 结束说明(取消 = 「迁移已取消」;成功 = 汇总;失败 = 中文错误)。
    pub message: String,
    /// 逐镜像结果(顺序与请求 images 对齐):`true` = 已在目标服务器
    /// (含同 ID 自动跳过与本次成功装载两种情况)。
    pub images_done: Vec<bool>,
}

/// 迁移全局单会话状态(与 manage_stats/manage_logs 的 generation 模式一致)。
/// 内部 Arc 供 tokio::spawn 的迁移任务共享(避免 tauri::State 的局部生命周期)。
/// 由 tauri Builder `.manage(MigrateState::default())` 注册。
#[derive(Default)]
pub struct MigrateState {
    inner: Arc<MigrateStateInner>,
}

#[derive(Default)]
pub(crate) struct MigrateStateInner {
    cancelled: std::sync::atomic::AtomicBool,
    /// 会话代号:每次 start 递增;旧任务收尾前发现代号过期则不再 emit。
    generation: std::sync::atomic::AtomicU64,
}

impl MigrateState {
    fn reset(&self) {
        self.inner
            .cancelled
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
    fn cancel(&self) {
        self.inner
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn next_generation(&self) -> u64 {
        self.inner
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }
    fn current_generation(&self) -> u64 {
        self.inner
            .generation
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 开始一次迁移:清取消位 + 递增代号(供 [`crate::migrate_project`] 复用,
    /// 两种迁移共用同一个全局单会话,天然互斥)。
    pub(crate) fn begin_migration(&self) -> u64 {
        self.reset();
        self.next_generation()
    }

    /// 取出内部 Arc(后台任务持有,避免 `tauri::State` 的生命周期限制)。
    pub(crate) fn inner_arc(&self) -> Arc<MigrateStateInner> {
        Arc::clone(&self.inner)
    }
}

impl MigrateStateInner {
    fn is_cancelled(&self) -> bool {
        self.cancelled
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 取消标志查询(公开包装,供迁移模块复用)。
    pub(crate) fn is_cancelled_pub(&self) -> bool {
        self.is_cancelled()
    }
}

/// `migrate_images` 的请求参数(camelCase)。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateRequest {
    /// 源服务器 ID(镜像所在)
    pub source_id: String,
    /// 目标服务器 ID(镜像去向)
    pub target_id: String,
    /// 要迁移的完整镜像引用列表(如 `myapp:latest`;逐个串行迁移)
    pub images: Vec<String>,
    /// 源服务器密码(Password 认证且本次输入新密码时传,一般省略)
    pub source_password_plain: Option<String>,
    /// 目标服务器密码(同上)
    pub target_password_plain: Option<String>,
}

/// 发起跨服务器镜像迁移(阶段十):立即返回 `Ok(())`,迁移在后台任务执行,
/// 进度经 `migrate-log`、结果经 `migrate-done` 事件推送。
///
/// 逐镜像串行:①源服务器 `docker save`+gzip(流式导出到本地临时目录)
/// → ②`sftp_download` 拉回本地 → ③`sftp_upload` 推到目标服务器 `/tmp/`
/// → ④目标 `docker load` → ⑤两端清理临时产物。**目标同 ID 自动跳过**
/// (迁移前 inspect 目标侧镜像 ID,与源侧一致即跳过全流程,报「目标已有」)。
/// 取消:`migrate_status(true)` 置位,镜像边界生效(协作式)。
#[tauri::command]
pub fn migrate_images(
    app: AppHandle,
    migrate_state: tauri::State<'_, MigrateState>,
    req: MigrateRequest,
) -> Result<(), String> {
    if req.source_id.trim().is_empty() || req.target_id.trim().is_empty() {
        return Err("源/目标服务器 ID 不能为空".to_string());
    }
    if req.source_id == req.target_id {
        return Err("源服务器与目标服务器不能相同".to_string());
    }
    let images: Vec<String> = req
        .images
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if images.is_empty() {
        return Err("请至少选择一个要迁移的镜像".to_string());
    }

    migrate_state.reset();
    let migrate_id = migrate_state.next_generation();
    log::info!("镜像迁移启动: {} -> {} 共 {} 个", req.source_id, req.target_id, images.len());
    let state = Arc::clone(&migrate_state.inner);

    tauri::async_runtime::spawn(async move {
        // panic 兜底:迁移任务 panic 时前端会永久停在「迁移中…」,故以错误收尾
        // (与 deploy_batch 的编排层兜底同一做法)
        let (success, message, images_done) = match CatchPanic::new(run_migrate(
            &app,
            &state,
            migrate_id,
            &req,
            images.clone(),
        ))
        .await
        {
            Ok(Ok((msg, done))) => (true, msg, done),
            Ok(Err((msg, done))) => (false, msg, done),
            Err(panic_info) => {
                log::error!("镜像迁移任务发生 panic: {}", panic_info);
                (
                    false,
                    "镜像迁移因内部错误中止,详情见日志".to_string(),
                    vec![false; images.len()],
                )
            }
        };
        let _ = app.emit(
            "migrate-done",
            MigrateDoneEvent {
                migrate_id,
                success,
                message,
                images_done,
            },
        );
    });
    Ok(())
}

/// 取消当前迁移 / 查询迁移状态(幂等)。
///
/// `cancel = true` → 置取消标志(镜像边界生效,协作式);
/// `cancel = false` → 仅查询,返回 `{ running: true(有代号即视为可能运行中) }`。
/// 返回值为当前迁移代号(0 = 尚未发起过),前端用于事件归属判别。
#[tauri::command]
pub async fn migrate_status(
    migrate_state: tauri::State<'_, MigrateState>,
    cancel: bool,
) -> Result<u64, String> {
    if cancel {
        migrate_state.cancel();
    }
    Ok(migrate_state.current_generation())
}

/// 迁移主体:逐镜像串行执行四步,返回 `(结束说明, 逐镜像完成标记)`。
/// 失败时返回 `Err((错误, 已完成部分))`;取消返回
/// `Err(("迁移已取消", 已完成部分))`(与部署取消文案同风格)。
async fn run_migrate(
    app: &AppHandle,
    state: &MigrateStateInner,
    migrate_id: u64,
    req: &MigrateRequest,
    images: Vec<String>,
) -> Result<(String, Vec<bool>), (String, Vec<bool>)> {
    let mut done = vec![false; images.len()];
    // 事件回调:Arc 包装使其 Send + Sync(可移入 spawn 与各 async fn)
    let emit_line: Arc<dyn Fn(&str) + Send + Sync> = {
        let app = app.clone();
        Arc::new(move |msg: &str| {
            let _ = app.emit(
                "migrate-log",
                MigrateLogEvent {
                    migrate_id,
                    line: format_log_line("", msg),
                },
            );
        })
    };

    // 连接源与目标(各自独立建连;TOFU/口令解析与 manage 系列同口径)
    emit_line(&format!(
        "连接源服务器…(共 {} 个镜像待迁移)",
        images.len()
    ));
    let (src_server, mut src) = connect_server(&req.source_id, req.source_password_plain.as_deref(), None)
        .await
        .map_err(|e| (e, done.clone()))?;
    emit_line(&format!("已连接源服务器「{}」", src_server.name));
    let (dst_server, mut dst) = connect_server(&req.target_id, req.target_password_plain.as_deref(), None)
        .await
        .map_err(|e| (e, done.clone()))?;
    emit_line(&format!("已连接目标服务器「{}」", dst_server.name));

    let total = images.len();
    let mut migrated: usize = 0;
    let mut skipped: usize = 0;
    for (i, image) in images.iter().enumerate() {
        if state.is_cancelled() {
            return Err((CANCELLED_MSG.to_string(), done));
        }
        emit_line(&format!(
            "({}/{}) 开始迁移 {}",
            i + 1,
            total,
            image
        ));

        // 源侧镜像 ID(供目标同 ID 跳过判定;取不到 → 后续 save 阶段报错)
        let (code, out) = exec_collect(&mut src, &docker_inspect_id_cmd(image))
            .await
            .map_err(|e| (format!("源服务器查询镜像 {} 失败: {}", image, e), done.clone()))?;
        let src_id = if code == 0 { parse_inspect_id(&out) } else { None };
        if src_id.is_none() {
            let msg = format!(
                "源服务器上不存在镜像 {}(或无权访问),跳过",
                image
            );
            emit_line(&format!("警告:{}", msg));
            continue;
        }

        // 目标同 ID 自动跳过(镜像已在目标侧,零传输)
        let (code, out) = exec_collect(&mut dst, &docker_inspect_id_cmd(image))
            .await
            .map_err(|e| (format!("目标服务器查询镜像 {} 失败: {}", image, e), done.clone()))?;
        if code == 0 {
            if let Some(dst_id) = parse_inspect_id(&out) {
                if same_image_id(&src_id.clone().unwrap_or_default(), &dst_id) {
                    emit_line(&format!("目标服务器已有同 ID 镜像,跳过传输: {}", image));
                    done[i] = true;
                    skipped += 1;
                    continue;
                }
            }
            emit_line(&format!(
                "警告:目标服务器已有同名镜像但 ID 不同,将覆盖(旧镜像变悬空): {}",
                image
            ));
        }

        // ① 源服务器 save+gzip → 直接落本地临时目录(sftp 下载目标即本地文件,
        //    免去「远端 tar 再下载」一步;源侧不落盘)
        let tar_name = format!("migrate-{}.tar.gz", uuid::Uuid::new_v4());
        let local_path = std::env::temp_dir().join(&tar_name);
        let guard = TempFileGuard::new(local_path.clone());
        emit_line(&format!("({}/{}) 源服务器导出压缩…", i + 1, total));
        save_gzip_remote(&mut src, image, &local_path, &emit_line)
            .await
            .map_err(|e| (e, done.clone()))?;

        // ② 上传到目标 /tmp(进度经 migrate-log;断点续传不必需,全新上传)
        if state.is_cancelled() {
            drop(guard);
            return Err((CANCELLED_MSG.to_string(), done));
        }
        emit_line(&format!("({}/{}) 上传到目标服务器…", i + 1, total));
        let last = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let app_cb = app.clone();
        let mid = migrate_id;
        let last_cb = Arc::clone(&last);
        dst.sftp_upload(&local_path, "/tmp", &tar_name, false, &(move |sent, total_bytes| {
            if total_bytes > 0 && sent >= last_cb.load(std::sync::atomic::Ordering::Relaxed) + LOG_PROGRESS_STEP {
                last_cb.store(sent, std::sync::atomic::Ordering::Relaxed);
                let _ = app_cb.emit(
                    "migrate-log",
                    MigrateLogEvent {
                        migrate_id: mid,
                        line: format_log_line("", &format!("已上传 {} MB / {} MB", sent / 1024 / 1024, total_bytes / 1024 / 1024)),
                    },
                );
            }
        }))
        .await
        .map_err(|e| (format!("上传镜像包到目标服务器失败: {}", e), done.clone()))?;

        // ③ 目标 docker load(输出转发 migrate-log)
        if state.is_cancelled() {
            drop(guard);
            return Err((CANCELLED_MSG.to_string(), done));
        }
        let remote_tar = format!("/tmp/{}", tar_name);
        emit_line(&format!("({}/{}) 目标服务器装载: docker load -i {}", i + 1, total, remote_tar));
        let load_cmd = format!("docker load -i {}", shell_single_quote(&remote_tar));
        exec_forwarded_migrate(&mut dst, &load_cmd, &emit_line)
            .await
            .map_err(|e| (e, done.clone()))?;

        // ④ 清理远端 /tmp tar(尽力而为)
        let rm_cmd = format!("rm -f {}", shell_single_quote(&remote_tar));
        let _ = exec_collect(&mut dst, &rm_cmd).await;
        drop(guard); // 本地临时 tar 随守卫删除

        done[i] = true;
        migrated += 1;
        emit_line(&format!("({}/{}) 完成: {}", i + 1, total, image));
    }

    Ok((
        format!(
            "迁移完成:{} 成功 / {} 跳过(目标已有)/ {} 失败",
            migrated,
            skipped,
            total - migrated - skipped
        ),
        done,
    ))
}

/// 源服务器 `docker save <image>` → 流式写本地 `out_path`:
/// 源侧开 `docker save <image> | gzip` 原始通道,Data 块直写本地文件
/// (源端零落盘;**不经 exec 的按行拆分**,二进制安全)。
pub(crate) async fn save_gzip_remote(
    src: &mut SshClient,
    image: &str,
    out_path: &std::path::Path,
    emit_line: &Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<(), String> {
    use russh::ChannelMsg;
    use tokio::io::AsyncWriteExt;
    let cmd = format!("docker save {} | gzip", shell_single_quote(image));
    // 经 pub(crate) 助手打开原始通道(ssh.rs 的 exec 会做行拆分,不适合二进制流)
    let mut channel = src.raw_exec_channel(&cmd).await?;
    let mut file = tokio::fs::File::create(out_path)
        .await
        .map_err(|e| format!("无法创建本地临时文件 {}: {}", out_path.display(), e))?;
    let mut written: u64 = 0;
    let mut last_reported: u64 = 0;
    let mut stderr_tail: Vec<String> = Vec::new();
    let mut exit_code: i32 = -1;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { ref data }) => {
                file.write_all(data)
                    .await
                    .map_err(|e| format!("写入本地临时文件失败: {}", e))?;
                written += data.len() as u64;
                // 进度按 5MB 节流(与部署导出同口径)
                if written >= last_reported + LOG_PROGRESS_STEP {
                    last_reported = written;
                    emit_line(&format!("已迁移 {} MB", written / 1024 / 1024));
                }
            }
            Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                // stderr(gzip/docker 错误):收尾几行供报错
                let line = String::from_utf8_lossy(data).trim().to_string();
                if !line.is_empty() && stderr_tail.len() < PULL_OUTPUT_TAIL_LINES {
                    stderr_tail.push(line);
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                exit_code = exit_status as i32;
            }
            Some(ChannelMsg::Eof) => {}
            Some(ChannelMsg::Close) | None => break,
            _ => {}
        }
    }
    file.flush()
        .await
        .map_err(|e| format!("刷新本地临时文件失败: {}", e))?;
    if exit_code != 0 {
        return Err(format!(
            "源服务器导出镜像失败(退出码 {}){}",
            exit_code,
            if stderr_tail.is_empty() {
                String::new()
            } else {
                format!(": {}", stderr_tail.join(" | "))
            }
        ));
    }
    if written == 0 {
        return Err("源服务器导出镜像失败: 无输出(镜像不存在?)".to_string());
    }
    Ok(())
}

/// 迁移路径的 exec 输出转发:每行 emit `migrate-log`(与 exec_forwarded 同构,
/// 但走 migrate 事件;末尾复查取消由镜像边界负责)。
async fn exec_forwarded_migrate(
    client: &mut SshClient,
    cmd: &str,
    emit_line: &Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<(), String> {
    let mut buf = String::new();
    let mut on_line = |line: &str| {
        emit_line(line.trim_end());
        buf.push_str(line);
    };
    let code = with_timeout(
        STACK_LOAD_TIMEOUT_SECS,
        "目标服务器装载超时",
        "请检查服务器网络后重试",
        async {
            client
                .exec(cmd, &mut on_line)
                .await
                .map_err(|e| format!("执行 docker load 失败: {}", e))
        },
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "目标服务器装载失败(退出码 {}): {}",
            code,
            buf.trim().lines().last().unwrap_or("无输出")
        ));
    }
    Ok(())
}
