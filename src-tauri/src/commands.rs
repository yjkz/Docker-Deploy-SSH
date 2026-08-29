//! Tauri 命令层与部署管线(Task 5)。
//!
//! 所有命令统一返回 `Result<T, String>`,错误信息面向用户(中文)。
//!
//! 事件(Tauri 2 `Emitter::emit`):
//! - `deploy-progress`:`DeployProgress { step, total, message }`,step 1..5
//!   (1=打标签 2=导出压缩 3=上传镜像 4=同步文件 5=服务器部署)
//! - `deploy-log`:一行日志字符串,带 `[HH:MM:SS]` 前缀
//! - `deploy-done`:`DeployDone { success, message }`
//! - `server-log`:`install_server_docker` 安装脚本的逐行输出
//!
//! 部署管线(`deploy` 命令同步返回 `Ok(())`,后台任务执行,严格顺序,
//! 任一步失败即中止并 emit `deploy-done` failure):
//! 前置(找 server/project、解析密码)→ 打标签 → 导出压缩 → 上传镜像
//! → 同步文件 → 服务器部署(docker load → compose up -d → 清理远端 tar)。

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::config::{
    load_config, save_config, AppConfig, AuthType, ProjectConfig, ServerConfig, ServiceOverride,
};
use crate::crypto::dpapi_unprotect;
use crate::docker::{
    check_host, image_exists, image_size, make_deploy_tag, save_gzip, start_daemon, tag_image,
    HostCheckReport, ImageInfo,
};
use crate::ssh::{
    check_server_env, mkdir_p_cmd, ServerCheckReport, SshClient, INSTALL_DOCKER_CMD,
};
use crate::stack::{apply_overrides, parse_compose_file, ComposeStack};

/// 取消提示文案(取消导致的失败统一用它,便于前端识别)。
const CANCELLED_MSG: &str = "部署已取消";
/// SSH 建连超时(秒):russh 对不可达地址可能长时间挂起且自身不带超时,统一兜底。
const SSH_CONNECT_TIMEOUT_SECS: u64 = 15;
/// SSH 检测/建目录类命令的执行超时(秒)(安装 Docker 固定 1800 秒,另行指定)。
const SSH_EXEC_TIMEOUT_SECS: u64 = 60;
/// 导出进度日志的汇报粒度:每 ≥5MB 变化汇报一次。
const LOG_PROGRESS_STEP: u64 = 5 * 1024 * 1024;

/// 部署运行状态:`cancel_deploy` 置位 `cancelled`,
/// 部署管线在各步骤之间以及 exec 输出行回调中检查后中止。
#[derive(Default)]
pub struct DeployState {
    pub cancelled: AtomicBool,
}

/// `deploy-progress` 事件负载。
#[derive(Debug, Clone, Serialize)]
pub struct DeployProgress {
    /// 当前步骤(1..5)
    pub step: u8,
    /// 总步骤数(5)
    pub total: u8,
    pub message: String,
}

/// `deploy-done` 事件负载。
#[derive(Debug, Clone, Serialize)]
pub struct DeployDone {
    pub success: bool,
    pub message: String,
}

/// `deploy` 命令的请求参数。
#[derive(Debug, Clone, Deserialize)]
pub struct DeployRequest {
    /// 本地完整镜像引用(如 `myapp:latest`)
    pub image: String,
    /// 部署仓库名(生成日期标签时的前缀)
    pub repository: String,
    pub server_id: String,
    pub project_id: String,
    /// true 时生成 `repository:YYYYmmdd-HHMMSS` 日期标签
    pub use_date_tag: bool,
    /// 前端临时输入的 SSH 密码(密码认证时优先于已保存的密文)
    pub password_plain: Option<String>,
}

// ===== 配置命令 =====

/// 读取全部配置(服务器 + 项目)。
#[tauri::command]
pub fn get_config() -> Result<AppConfig, String> {
    load_config().map_err(|e| format!("读取配置失败: {}", e))
}

/// 保存全部配置(原子写入)。
#[tauri::command]
pub fn save_config_cmd(cfg: AppConfig) -> Result<(), String> {
    save_config(&cfg).map_err(|e| format!("保存配置失败: {}", e))
}

/// 用 DPAPI 加密明文密码,返回 base64 密文(前端保存服务器配置时存回 password_enc)。
#[tauri::command]
pub fn encrypt_password(plain: String) -> Result<String, String> {
    crate::crypto::dpapi_protect(&plain)
}

// ===== compose 整栈命令 =====

/// 导入 compose 文件:校验文件存在且可解析后,把 compose(连同同目录 `.env`,
/// 若存在)复制到 `config/stacks/<uuid>/docker-compose.yml` 持久化,以解析出的
/// 默认传输分类创建新项目(名称为用户自命名,compose_file 指向副本),写回配置
/// 并返回完整 ProjectConfig。
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
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("创建栈目录失败 ({}): {}", dest_dir.display(), e))?;
    let dest = dest_dir.join("docker-compose.yml");
    std::fs::copy(&source, &dest).map_err(|e| {
        format!(
            "复制 compose 文件失败 ({} -> {}): {}",
            source.display(),
            dest.display(),
            e
        )
    })?;
    // compose 同目录的 .env 一并复制(不存在则跳过),保证后续解析/部署插值一致
    if let Some(parent) = source.parent() {
        let source_env = parent.join(".env");
        if source_env.is_file() {
            let dest_env = dest_dir.join(".env");
            std::fs::copy(&source_env, &dest_env).map_err(|e| {
                format!("复制 .env 文件失败 ({}): {}", source_env.display(), e)
            })?;
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
    };
    let mut cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    cfg.projects.push(project.clone());
    save_config(&cfg).map_err(|e| format!("保存配置失败: {}", e))?;
    Ok(project)
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

/// 检测宿主机 Docker 环境(内部多次调用 docker CLI,放 blocking 线程池避免卡 UI)。
#[tauri::command]
pub async fn host_check() -> Result<HostCheckReport, String> {
    Ok(tauri::async_runtime::spawn_blocking(check_host)
        .await
        .map_err(|e| format!("宿主机检测任务失败: {}", e))?)
}

/// 拉起 Docker 守护进程(可能阻塞最长 60 秒,放 blocking 线程池)。
#[tauri::command]
pub async fn start_docker() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(start_daemon)
        .await
        .map_err(|e| format!("启动 Docker 任务失败: {}", e))?
}

/// 列出本地全部镜像。
#[tauri::command]
pub async fn list_images() -> Result<Vec<ImageInfo>, String> {
    tauri::async_runtime::spawn_blocking(crate::docker::list_images)
        .await
        .map_err(|e| format!("获取镜像列表任务失败: {}", e))?
}

// ===== 服务器命令 =====

/// 连接服务器并检查远端环境(docker/compose/gzip/远端目录/磁盘空间)。
#[tauri::command]
pub async fn test_server(
    server_id: String,
    password_plain: Option<String>,
) -> Result<ServerCheckReport, String> {
    connect_and_check(&server_id, password_plain).await
}

/// 与 `test_server` 等价的环境检查(独立命令名,语义 = 部署前环境自检)。
#[tauri::command]
pub async fn server_env_check(
    server_id: String,
    password_plain: Option<String>,
) -> Result<ServerCheckReport, String> {
    connect_and_check(&server_id, password_plain).await
}

/// 连接 + 远端环境检查的公共实现。
async fn connect_and_check(
    server_id: &str,
    password_plain: Option<String>,
) -> Result<ServerCheckReport, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref()),
    )
    .await?;
    with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "环境检查超时",
        "请检查服务器网络后重试",
        check_server_env(&mut client, &server.remote_dir),
    )
    .await
}

/// 给可能长时间无响应的 SSH future 整体套一层超时(兜底 russh 自身不带连接超时)。
///
/// 超时错误格式 `{desc}({secs} 秒):{hint}`,如
/// “连接超时(15 秒):请检查服务器地址与网络”。
async fn with_timeout<T>(
    secs: u64,
    desc: &str,
    hint: &str,
    fut: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(res) => res,
        Err(_) => Err(format!("{}({} 秒):{}", desc, secs, hint)),
    }
}

/// 在远端执行官方脚本安装 Docker(最长 1800 秒),输出逐行 emit `server-log`。
#[tauri::command]
pub async fn install_server_docker(
    server_id: String,
    password_plain: Option<String>,
    app: AppHandle,
) -> Result<(), String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let mut client = SshClient::connect(&server, password.as_deref()).await?;

    let mut on_output = |line: &str| {
        let _ = app.emit("server-log", line.trim_end().to_string());
    };
    let fut = client.exec(INSTALL_DOCKER_CMD, &mut on_output);
    let code = match tokio::time::timeout(Duration::from_secs(1800), fut).await {
        Ok(res) => res.map_err(|e| format!("执行 Docker 安装命令失败: {}", e))?,
        Err(_) => return Err("安装 Docker 超时(1800 秒),请检查服务器网络后重试".to_string()),
    };
    if code != 0 {
        return Err(format!(
            "Docker 安装脚本退出码 {},请根据安装日志排查(常见原因:网络不通、需要 root)",
            code
        ));
    }
    Ok(())
}

/// 在远端创建服务器配置的部署根目录(`mkdir -p <remote_dir>`)。
#[tauri::command]
pub async fn create_remote_dir(
    server_id: String,
    password_plain: Option<String>,
) -> Result<(), String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref()),
    )
    .await?;
    let cmd = mkdir_p_cmd(&server.remote_dir);
    let code = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "创建目录超时",
        "请检查服务器网络后重试",
        async {
            client
                .exec(&cmd, &mut |_| {})
                .await
                .map_err(|e| format!("远端创建目录失败: {}", e))
        },
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "远端创建目录 {} 失败(退出码 {},常见原因:无写入权限)",
            server.remote_dir, code
        ));
    }
    Ok(())
}

// ===== 部署命令 =====

/// 发起部署:立即返回 `Ok(())`,管线在后台任务执行并通过事件推送进度。
#[tauri::command]
pub fn deploy(req: DeployRequest, app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn(async move {
        // panic 兜底:run_deploy 内部任何 panic 都转成失败结果,
        // 保证任何路径下 deploy-done 恰好 emit 一次(不会静默丢失)。
        let result = match CatchPanic::new(run_deploy(&app, req)).await {
            Ok(result) => result,
            Err(panic_info) => {
                log::error!("部署管线发生 panic: {}", panic_info);
                Err("部署过程发生内部错误,详情见日志".to_string())
            }
        };
        match result {
            Ok(()) => {
                let _ = app.emit(
                    "deploy-done",
                    DeployDone {
                        success: true,
                        message: "部署完成".to_string(),
                    },
                );
            }
            Err(e) => {
                emit_log(&app, &format!("部署失败: {}", e));
                let _ = app.emit("deploy-done", DeployDone { success: false, message: e });
            }
        }
    });
    Ok(())
}

/// Future 的 panic 兜底包装:被包裹 future 在 poll 中 panic 时返回 `Err(panic 信息)`,
/// 而不是让整个后台任务静默消失(配合 [`deploy`] 保证 `deploy-done` 恰好 emit 一次)。
struct CatchPanic<F: std::future::Future>(Pin<Box<F>>);

impl<F: std::future::Future> CatchPanic<F> {
    fn new(fut: F) -> Self {
        Self(Box::pin(fut))
    }
}

impl<F: std::future::Future> std::future::Future for CatchPanic<F> {
    type Output = Result<F::Output, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.get_mut().0.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Ok(Poll::Pending) => Poll::Pending,
            // panic 后 inner 已不可恢复,直接以错误收尾,不再 poll
            Err(payload) => Poll::Ready(Err(panic_message(&payload))),
        }
    }
}

/// 从 panic payload 提取可读信息(&str / String / 其他)。
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "未知 panic".to_string()
    }
}

/// 取消当前部署(置位 AtomicBool;管线在各步骤间检查后中止)。
#[tauri::command]
pub fn cancel_deploy(state: tauri::State<'_, DeployState>) -> Result<(), String> {
    state.cancelled.store(true, Ordering::SeqCst);
    Ok(())
}

// ===== 部署管线(严格顺序,任一步失败即中止)=====

/// 部署管线主体。失败返回中文错误,由 [`deploy`] 统一 emit `deploy-done` failure。
async fn run_deploy(app: &AppHandle, req: DeployRequest) -> Result<(), String> {
    // ---- 步骤 0:前置 ----
    // 每次 deploy 开始时重置取消标志;结束时保持不变(取消后为 true,下次部署重置)
    reset_cancelled(app);

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &req.server_id)?.clone();
    let project = find_project(&cfg, &req.project_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        req.password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    emit_log(
        app,
        &format!(
            "开始部署:服务器「{}」/ 项目「{}」",
            server.name, project.name
        ),
    );

    // ---- 步骤 1:打标签 ----
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

    // ---- 步骤 2:导出压缩 ----
    emit_progress(app, 2, 5, "导出压缩镜像");
    ensure_not_cancelled(app)?;
    let tar_name = format!("{}.tar.gz", uuid::Uuid::new_v4());
    let out_path = std::env::temp_dir().join(&tar_name);
    // 本地 tar 用完即删:Drop guard 覆盖成功/失败全部路径
    let _tar_guard = TempFileGuard(out_path.clone());

    // 空间预检:导出目标盘(临时目录所在盘)剩余空间 ≥ 镜像大小 × 1.5
    match image_size(&image_ref) {
        Some(size) => check_export_disk_space(size)?,
        None => emit_log(app, "警告:无法获取镜像大小,跳过磁盘剩余空间检查"),
    }

    let total_bytes = export_image(app, &image_ref, &out_path).await?;
    emit_log(app, &format!("导出完成,共 {} MB", total_bytes / 1024 / 1024));

    // ---- 步骤 3:上传镜像 ----
    emit_progress(app, 3, 5, "上传镜像到服务器");
    ensure_not_cancelled(app)?;
    let mut client = SshClient::connect(&server, password.as_deref()).await?;
    upload_tar(app, &mut client, &out_path, &tar_name).await?;
    emit_log(app, "镜像上传完成");

    // ---- 步骤 4:同步文件 ----
    emit_progress(app, 4, 5, "同步项目文件");
    ensure_not_cancelled(app)?;
    sync_files(app, &mut client, &server, &project).await?;
    emit_log(app, "项目文件同步完成");

    // ---- 步骤 5:服务器部署 ----
    emit_progress(app, 5, 5, "服务器部署");
    ensure_not_cancelled(app)?;
    server_deploy(app, &mut client, &server, &project, &tar_name).await?;

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

/// 步骤 2:`docker save` → gzip 流式压缩导出到 `out_path`。
///
/// 阻塞型 `save_gzip` 放入 blocking 线程池;progress_cb 用 `AtomicU64`
/// 累计压缩后字节数,每 ≥5MB 变化 emit 一次 `deploy-log`(“已导出 X MB”)。
/// 返回压缩后的总字节数。
async fn export_image(app: &AppHandle, image_ref: &str, out_path: &Path) -> Result<u64, String> {
    let last_reported = Arc::new(AtomicU64::new(0));
    let app_for_cb = app.clone();
    let image = image_ref.to_string();
    let path = out_path.to_path_buf();
    let last = Arc::clone(&last_reported);

    let handle = tauri::async_runtime::spawn_blocking(move || {
        save_gzip(&image, &path, move |n| {
            let prev = last.load(Ordering::Relaxed);
            if n >= prev.saturating_add(LOG_PROGRESS_STEP) {
                last.store(n, Ordering::Relaxed);
                emit_log(&app_for_cb, &format!("已导出 {} MB", n / 1024 / 1024));
            }
        })
    });
    match handle.await {
        Ok(Ok(total)) => Ok(total),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("导出任务异常终止: {}", e)),
    }
}

/// 步骤 3:上传镜像 tar 到远端固定 `/tmp` 目录,按每 10% 进度 emit `deploy-log`。
async fn upload_tar(
    app: &AppHandle,
    client: &mut SshClient,
    tar_path: &Path,
    tar_name: &str,
) -> Result<(), String> {
    let last_pct = Arc::new(AtomicU64::new(0));
    let app_for_cb = app.clone();
    let last = Arc::clone(&last_pct);

    client
        .sftp_upload(tar_path, "/tmp", tar_name, &move |sent, total| {
            if total == 0 {
                return;
            }
            let step10 = (sent * 100 / total) / 10 * 10;
            let prev = last.load(Ordering::Relaxed);
            if step10 > prev {
                last.store(step10, Ordering::Relaxed);
                emit_log(&app_for_cb, &format!("镜像上传进度 {}%", step10));
            }
        })
        .await
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
        let full_remote = remote_join(&server.remote_dir, &mapping.remote);
        if mapping.is_dir {
            emit_log(app, &format!("同步目录: {} -> {}", mapping.local, full_remote));
            client.sftp_upload_dir(&local, &full_remote, &|_, _| {}).await?;
        } else {
            let (dir, name) = split_remote_file(&full_remote);
            emit_log(app, &format!("同步文件: {} -> {}/{}", mapping.local, dir, name));
            client.sftp_upload(&local, &dir, &name, &|_, _| {}).await?;
        }
    }
    Ok(())
}

/// 步骤 5:服务器部署 —— `docker load` → `docker compose up -d` → 删除远端 tar。
///
/// 每条命令超时 600 秒,输出实时转发到 `deploy-log`,收到输出行时检查取消标志。
async fn server_deploy(
    app: &AppHandle,
    client: &mut SshClient,
    server: &ServerConfig,
    project: &ProjectConfig,
    tar_name: &str,
) -> Result<(), String> {
    let remote_tar = remote_join("/tmp", tar_name);

    // 5.1 加载镜像
    emit_log(app, &format!("加载镜像到服务器: docker load -i {}", remote_tar));
    let load_cmd = format!("docker load -i {}", shell_single_quote(&remote_tar));
    exec_forwarded(app, client, &load_cmd, 600).await?;

    // 5.2 启动服务(cd 到远端目录后按相对 compose 文件启动)
    let up_cmd = format!(
        "cd {} && docker compose -f {} up -d",
        shell_single_quote(&server.remote_dir),
        shell_single_quote(&project.compose_file),
    );
    emit_log(
        app,
        &format!(
            "启动服务: cd {} && docker compose -f {} up -d",
            server.remote_dir, project.compose_file
        ),
    );
    exec_forwarded(app, client, &up_cmd, 600).await?;

    // 5.3 清理远端 tar(尽力而为,失败不影响部署结果)
    let rm_cmd = format!("rm -f {}", shell_single_quote(&remote_tar));
    if let Err(e) = exec_forwarded(app, client, &rm_cmd, 60).await {
        emit_log(app, &format!("警告:清理远端临时文件失败: {}", e));
    }
    Ok(())
}

/// 执行远端命令并把输出逐行转发到 `deploy-log`,带超时与取消检查。
///
/// 取消无法中断正在阻塞读取的 exec,因此输出行回调中记录取消状态,
/// 命令结束后立即以“部署已取消”失败返回。
async fn exec_forwarded(
    app: &AppHandle,
    client: &mut SshClient,
    cmd: &str,
    timeout_secs: u64,
) -> Result<(), String> {
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let app_for_cb = app.clone();
    let cancel_flag = Arc::clone(&saw_cancel);
    let mut on_output = move |line: &str| {
        emit_log(&app_for_cb, line);
        if is_cancelled(&app_for_cb) {
            cancel_flag.store(true, Ordering::SeqCst);
        }
    };

    let fut = client.exec(cmd, &mut on_output);
    let code = match tokio::time::timeout(Duration::from_secs(timeout_secs), fut).await {
        Ok(res) => res.map_err(|e| format!("SSH 执行命令失败: {}", e))?,
        Err(_) => {
            return Err(format!("远端命令执行超时({} 秒): {}", timeout_secs, cmd));
        }
    };
    if saw_cancel.load(Ordering::SeqCst) {
        return Err(CANCELLED_MSG.to_string());
    }
    if code != 0 {
        return Err(format!("远端命令执行失败(退出码 {}): {}", code, cmd));
    }
    // 末尾取消复查:命令可能全程无输出、回调一次都未触发,
    // 结束后再查一次取消标志,保证取消后不会把该步误报为成功。
    if is_cancelled(app) {
        return Err(CANCELLED_MSG.to_string());
    }
    Ok(())
}

// ===== 管线辅助 =====

/// 取消标志是否置位。
fn is_cancelled(app: &AppHandle) -> bool {
    app.state::<DeployState>()
        .cancelled
        .load(Ordering::SeqCst)
}

/// 部署开始时重置取消标志。
fn reset_cancelled(app: &AppHandle) {
    app.state::<DeployState>()
        .cancelled
        .store(false, Ordering::SeqCst);
}

/// 各步骤之间的取消检查:已取消则返回错误中止管线。
fn ensure_not_cancelled(app: &AppHandle) -> Result<(), String> {
    if is_cancelled(app) {
        Err(CANCELLED_MSG.to_string())
    } else {
        Ok(())
    }
}

/// emit `deploy-progress` 事件。
fn emit_progress(app: &AppHandle, step: u8, total: u8, message: &str) {
    let _ = app.emit(
        "deploy-progress",
        DeployProgress {
            step,
            total,
            message: message.to_string(),
        },
    );
}

/// emit `deploy-log` 事件:一行日志,带 `[HH:MM:SS]` 前缀(尾随换行剔除)。
fn emit_log(app: &AppHandle, msg: &str) {
    let line = format!(
        "[{}] {}",
        chrono::Local::now().format("%H:%M:%S"),
        msg.trim_end()
    );
    let _ = app.emit("deploy-log", line);
}

/// 本地临时 tar 的 Drop 守卫:作用域结束(成功或失败)时删除文件。
struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("删除本地临时文件失败 ({}): {}", self.0.display(), e);
            }
        }
    }
}

/// 按 ID 查找服务器配置。
fn find_server<'a>(cfg: &'a AppConfig, server_id: &str) -> Result<&'a ServerConfig, String> {
    cfg.servers
        .iter()
        .find(|s| s.id == server_id)
        .ok_or_else(|| format!("未找到 ID 为「{}」的服务器配置", server_id))
}

/// 按 ID 查找项目配置。
fn find_project<'a>(cfg: &'a AppConfig, project_id: &str) -> Result<&'a ProjectConfig, String> {
    cfg.projects
        .iter()
        .find(|p| p.id == project_id)
        .ok_or_else(|| format!("未找到 ID 为「{}」的项目配置", project_id))
}

/// 解析 SSH 认证所需的明文密码(纯函数,便于测试)。
///
/// - `AuthType::Key`:返回 `None`(SshClient 走私钥文件认证);
/// - `AuthType::Password`:优先使用前端传入的 `password_plain`
///   (空字符串视为未输入),否则用 DPAPI 解密配置中的 `password_enc`;
///   两者都没有 → 报错。
pub fn resolve_password(
    auth_type: &AuthType,
    password_plain: Option<&str>,
    password_enc: Option<&str>,
) -> Result<Option<String>, String> {
    match auth_type {
        AuthType::Key => Ok(None),
        AuthType::Password => match password_plain.filter(|p| !p.is_empty()) {
            Some(p) => Ok(Some(p.to_string())),
            None => match password_enc.filter(|e| !e.is_empty()) {
                Some(enc) => Ok(Some(dpapi_unprotect(enc)?)),
                None => Err(
                    "密码认证需要输入密码,或先在服务器设置中保存密码".to_string(),
                ),
            },
        },
    }
}

/// 拼接远端路径:`base` 去尾部 `/` 后接 `/` + `rel`。
///
/// `rel` 以 `/` 开头时去掉开头 `/` 仍视为相对 `base` 拼接
/// (工具不提供绝对路径逃逸);`base` 为空时结果为 `/rel`。
pub fn remote_join(base: &str, rel: &str) -> String {
    let base = base.trim_end_matches('/');
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        if base.is_empty() {
            "/".to_string()
        } else {
            base.to_string()
        }
    } else if base.is_empty() {
        format!("/{}", rel)
    } else {
        format!("{}/{}", base, rel)
    }
}

/// 把远端完整文件路径拆成 `(父目录, 文件名)`;无 `/` 时父目录为空串。
fn split_remote_file(path: &str) -> (String, String) {
    match path.rfind('/') {
        Some(i) => (path[..i].to_string(), path[i + 1..].to_string()),
        None => (String::new(), path.to_string()),
    }
}

/// 单引号 shell 包裹;内部单引号按 `'\''` 转义。
/// (与 ssh.rs 内部实现一致;ssh::shell_single_quote 未导出,故本地实现。)
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TransferMode;

    // ===== remote_join =====

    #[test]
    fn test_remote_join_simple() {
        assert_eq!(
            remote_join("/opt/app", "docker-compose.yml"),
            "/opt/app/docker-compose.yml"
        );
    }

    #[test]
    fn test_remote_join_nested_rel() {
        assert_eq!(
            remote_join("/opt/app", "sql/init.sql"),
            "/opt/app/sql/init.sql"
        );
    }

    #[test]
    fn test_remote_join_leading_slash_no_escape() {
        // rel 以 '/' 开头时视为相对 remote_dir 仍拼接,不提供绝对路径逃逸
        assert_eq!(remote_join("/opt/app", "/abs/x"), "/opt/app/abs/x");
    }

    #[test]
    fn test_remote_join_base_trailing_slash() {
        assert_eq!(remote_join("/opt/app/", "y"), "/opt/app/y");
        assert_eq!(remote_join("/opt/app//", "a/b"), "/opt/app/a/b");
    }

    #[test]
    fn test_remote_join_empty_rel() {
        assert_eq!(remote_join("/opt/app", ""), "/opt/app");
    }

    #[test]
    fn test_remote_join_empty_base() {
        assert_eq!(remote_join("", "a.txt"), "/a.txt");
        assert_eq!(remote_join("/", "a.txt"), "/a.txt");
    }

    // ===== split_remote_file =====

    #[test]
    fn test_split_remote_file() {
        assert_eq!(
            split_remote_file("/opt/app/docker-compose.yml"),
            ("/opt/app".to_string(), "docker-compose.yml".to_string())
        );
        assert_eq!(
            split_remote_file("a.txt"),
            (String::new(), "a.txt".to_string())
        );
    }

    // ===== shell_single_quote =====

    #[test]
    fn test_shell_single_quote() {
        assert_eq!(shell_single_quote("/opt/app"), "'/opt/app'");
        assert_eq!(shell_single_quote("/opt/a'b"), "'/opt/a'\\''b'");
    }

    // ===== resolve_password =====

    #[test]
    fn test_resolve_password_key_auth() {
        // Key 认证:一律 None,不使用密码
        assert_eq!(resolve_password(&AuthType::Key, None, None).unwrap(), None);
        assert_eq!(
            resolve_password(&AuthType::Key, Some("ignored"), Some("enc")).unwrap(),
            None
        );
    }

    #[test]
    fn test_resolve_password_plain_takes_priority() {
        assert_eq!(
            resolve_password(&AuthType::Password, Some("pw"), None).unwrap(),
            Some("pw".to_string())
        );
        // 前端输入的明文优先于已保存密文
        assert_eq!(
            resolve_password(&AuthType::Password, Some("fresh"), Some("enc")).unwrap(),
            Some("fresh".to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_resolve_password_fallback_to_enc() {
        let enc = crate::crypto::dpapi_protect("saved-pw").unwrap();
        assert_eq!(
            resolve_password(&AuthType::Password, None, Some(&enc)).unwrap(),
            Some("saved-pw".to_string())
        );
        // 空明文视为未输入,回退到已保存密文
        assert_eq!(
            resolve_password(&AuthType::Password, Some(""), Some(&enc)).unwrap(),
            Some("saved-pw".to_string())
        );
    }

    #[test]
    fn test_resolve_password_missing() {
        // 密码认证但既无明文也无密文 → 报错
        assert!(resolve_password(&AuthType::Password, None, None).is_err());
    }

    #[test]
    fn test_resolve_password_bad_enc() {
        // 密文无效(base64 非法)→ 报错
        assert!(resolve_password(&AuthType::Password, None, Some("不是base64!!")).is_err());
    }

    // ===== import_compose =====

    #[test]
    fn test_import_compose_copies_parse_and_saves() {
        // DD_CONFIG_DIR 是进程级环境变量,与 config 层测试共用锁串行执行
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 源 compose(文件名故意不是 docker-compose.yml)+ 同目录 .env
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("my-stack.yaml");
        std::fs::write(
            &source,
            "name: demo\nservices:\n  web:\n    build: ./web\n    image: ${IMAGE}:v1\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        std::fs::write(src_dir.join(".env"), "IMAGE=myapp\n").unwrap();

        let project =
            import_compose(source.to_string_lossy().to_string(), "测试栈".into()).unwrap();

        // 副本位于 config/stacks/<uuid>/docker-compose.yml,内容与源一致
        let copy = PathBuf::from(&project.compose_file);
        assert!(copy.is_file(), "compose 副本应存在: {}", project.compose_file);
        assert_eq!(copy.file_name().unwrap().to_string_lossy(), "docker-compose.yml");
        let stacks_dir = dir.join("config").join("stacks");
        assert_eq!(
            copy.parent().unwrap().parent().unwrap(),
            stacks_dir.as_path(),
            "副本应在 config/stacks/<uuid>/ 下"
        );
        assert_eq!(
            std::fs::read_to_string(&copy).unwrap(),
            std::fs::read_to_string(&source).unwrap(),
            "副本内容应与源一致(原样复制,不做插值)"
        );
        // 同目录 .env 一并复制
        assert!(copy.parent().unwrap().join(".env").is_file(), ".env 副本应存在");

        // service_overrides 取解析默认:web=Local(build),db=Pull(仅 image)
        assert_eq!(project.service_overrides.len(), 2);
        let web = project.service_overrides.iter().find(|o| o.service == "web").unwrap();
        assert_eq!(web.mode, TransferMode::Local);
        let db = project.service_overrides.iter().find(|o| o.service == "db").unwrap();
        assert_eq!(db.mode, TransferMode::Pull);

        // 返回的 compose_file 指向副本而非源路径;配置已保存
        assert_ne!(project.compose_file, source.to_string_lossy().to_string());
        let cfg = load_config().unwrap();
        assert!(cfg.projects.iter().any(|p| p.id == project.id && p.name == "测试栈"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_import_compose_missing_file() {
        let err = import_compose("Z:/definitely/not/compose.yml".into(), "x".into()).unwrap_err();
        assert!(err.contains("不存在"), "实际: {}", err);
    }

    #[test]
    fn test_import_compose_invalid_yaml() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        let source = dir.join("bad.yml");
        std::fs::write(&source, "services: [unclosed\n").unwrap();
        let err = import_compose(source.to_string_lossy().to_string(), "x".into()).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        // 解析失败不落盘:不应产生 stacks 目录
        assert!(err.contains("YAML"), "实际: {}", err);
        assert!(!dir.join("config").join("stacks").exists(), "解析失败不应创建栈目录");
    }
}
