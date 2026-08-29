//! Tauri 命令层与部署管线(Task 5)。
//!
//! 所有命令统一返回 `Result<T, String>`,错误信息面向用户(中文)。
//!
//! 事件(Tauri 2 `Emitter::emit`):
//! - `deploy-progress`:`DeployProgress { step, total, message }`,step 1..5
//!   (1=打标签 2=导出压缩 3=上传镜像 4=同步文件 5=服务器部署)
//! - `deploy-log`:一行日志字符串,带 `[HH:MM:SS]` 前缀
//! - `deploy-done`:`DeployDone { success, message }`;emit 后按结果落一条
//!   部署历史(`history::append_record`,成功/失败/取消统一记录)
//! - `server-log`:`install_server_docker` 安装脚本与 `prune_server` 清理命令的逐行输出
//!
//! 部署管线(`deploy` 命令同步返回 `Ok(())`,后台任务执行,严格顺序,
//! 任一步失败即中止并 emit `deploy-done` failure):
//! 前置(找 server/project、解析密码)→ 打标签 → 导出压缩 → 上传镜像
//! → 同步文件 → 部署前钩子 → 服务器部署(docker load → compose up -d →
//! 健康检查 → 部署后钩子 → 清理远端 tar)。
//!
//! 整栈部署管线(`deploy_stack`,六步,progress step 1..6):
//! 前置(找 server/project、解析密码)→ 分类确认 → 打包(本地镜像并发 save_gzip)
//! → 上传(compose 副本与 override 文件 + releases/<时间戳>/ 镜像包 + 文件映射;
//! 镜像包失败后同路径重试一次,激活断点续传)→ 部署前钩子 → 装载(逐包 docker load)
//! → 拉取(compose pull;私有仓库认证失败时追加 docker login 提示)
//! → 启动(compose up -d)→ 健康检查 → 部署后钩子
//! → 清理旧 releases(仅留最新 5 个)。

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::config::{
    load_config, save_config, AppConfig, AuthType, ProjectConfig, ServerConfig, ServiceOverride,
    TransferMode,
};
use crate::crypto::dpapi_unprotect;
use crate::history::{append_record, load_history, DeployRecord, MODE_SINGLE, MODE_STACK};
use crate::docker::{
    check_host, image_exists, image_size, make_deploy_tag, save_gzip, start_daemon, tag_image,
    HostCheckReport, ImageInfo,
};
use crate::ssh::{
    check_server_env, exec_collect, mkdir_p_cmd, ServerCheckReport, SshClient, INSTALL_DOCKER_CMD,
};
use crate::stack::{apply_overrides, find_override_files, parse_compose_file, ComposeStack};

/// 取消提示文案(取消导致的失败统一用它,便于前端识别)。
const CANCELLED_MSG: &str = "部署已取消";
/// SSH 建连超时(秒):russh 对不可达地址可能长时间挂起且自身不带超时,统一兜底。
const SSH_CONNECT_TIMEOUT_SECS: u64 = 15;
/// SSH 检测/建目录类命令的执行超时(秒)(安装 Docker 固定 1800 秒,另行指定)。
const SSH_EXEC_TIMEOUT_SECS: u64 = 60;
/// 导出进度日志的汇报粒度:每 ≥5MB 变化汇报一次。
const LOG_PROGRESS_STEP: u64 = 5 * 1024 * 1024;
/// 整栈部署:并行打包的并发度上限(实际取 `min(本值, 可用并行度)`)。
const PACK_CONCURRENCY_CAP: usize = 3;
/// 镜像包上传失败后的重试等待(秒):给网络/服务端一点恢复时间,再同路径续传重试。
const UPLOAD_RETRY_DELAY_SECS: u64 = 2;
/// 整栈部署:单包 `docker load` 的执行超时(秒)。
const STACK_LOAD_TIMEOUT_SECS: u64 = 600;
/// 整栈部署:`docker compose pull` / `up -d` 的执行超时(秒)。
const STACK_COMPOSE_TIMEOUT_SECS: u64 = 900;
/// 服务器清理(`prune_server`)的执行超时(秒)。
const PRUNE_TIMEOUT_SECS: u64 = 300;
/// 部署前/后钩子命令的执行超时(秒)。
const HOOK_TIMEOUT_SECS: u64 = 600;
/// 健康检查:轮询间隔(秒)。
const HEALTH_POLL_INTERVAL_SECS: u64 = 5;
/// 健康检查:单轮 `compose ps` 状态查询的执行超时(秒)。
const HEALTH_PS_TIMEOUT_SECS: u64 = 60;
/// 整栈拉取失败时并入错误信息的远端输出末尾行数
/// (供 [`augment_pull_error`] 依据输出识别私有仓库认证问题)。
const PULL_OUTPUT_TAIL_LINES: usize = 10;

/// 部署运行状态:`cancel_deploy` 置位 `cancelled`,
/// 部署管线在各步骤之间以及 exec 输出行回调中检查后中止。
#[derive(Default)]
pub struct DeployState {
    pub cancelled: AtomicBool,
}

/// `deploy-progress` 事件负载。
#[derive(Debug, Clone, Serialize)]
pub struct DeployProgress {
    /// 当前步骤(单镜像 1..5;整栈 1..6)
    pub step: u8,
    /// 总步骤数(单镜像 5;整栈 6)
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

/// `deploy_stack` 命令的请求参数(整栈部署)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackDeployRequest {
    pub project_id: String,
    pub server_id: String,
    /// 前端从 parse_compose 结果逐服务确认后回传的传输分类列表
    pub services: Vec<StackServiceChoice>,
    /// 前端临时输入的 SSH 密码(密码认证时优先于已保存的密文)
    pub password_plain: Option<String>,
}

/// 整栈部署中单个 compose 服务的传输分类。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackServiceChoice {
    /// compose 服务名(compose pull / up 按此名定位)
    pub service: String,
    /// 传输方式为本地传输(Local)时必须非空的镜像引用;服务器拉取(Pull)时可为空
    pub image: String,
    pub mode: TransferMode,
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

/// 导入 compose 文件:校验文件存在且可解析后,把 compose(连同同目录 `.env`
/// 与 override 文件,若存在)复制到 `config/stacks/<uuid>/docker-compose.yml`
/// 持久化,以解析出的默认传输分类创建新项目(名称为用户自命名,compose_file
/// 指向副本),写回配置并返回完整 ProjectConfig。
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
    // compose 同目录的 override 文件一并复制(同名 basename):
    // 与解析合并保持一致,远端 pull/up 的 -f 文件链才能指向同名文件
    if let Some(parent) = source.parent() {
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

// ===== 服务器清理 + 远端磁盘预检(Task 2)=====

/// 清理服务器:删除悬空镜像与已停止容器(见 [`prune_cmd`]),输出逐行 emit
/// `server-log`(与 [`install_server_docker`] 同通道,前端服务器卡片可回显),
/// 超时 [`PRUNE_TIMEOUT_SECS`] 秒。
#[tauri::command]
pub async fn prune_server(
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
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref()),
    )
    .await?;

    let mut on_output = |line: &str| {
        let _ = app.emit("server-log", line.trim_end().to_string());
    };
    let cmd = prune_cmd();
    let fut = client.exec(&cmd, &mut on_output);
    let code = with_timeout(
        PRUNE_TIMEOUT_SECS,
        "服务器清理超时",
        "请检查服务器网络后重试",
        async { fut.await.map_err(|e| format!("执行服务器清理命令失败: {}", e)) },
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "服务器清理命令退出码 {},请根据输出排查(常见原因:无 docker 权限)",
            code
        ));
    }
    Ok(())
}

/// 拼装服务器清理命令:删除悬空镜像与已停止容器(`-f` 免交互;用 `;` 串联,
/// 第二项不受第一项退出码影响,两项均尽力执行)。
pub fn prune_cmd() -> String {
    "docker image prune -f; docker container prune -f".to_string()
}

/// 拼装查询 Docker 数据根目录的命令(`docker info` 的 Go 模板,单行输出)。
pub fn docker_root_cmd() -> String {
    "docker info -f '{{.DockerRootDir}}'".to_string()
}

/// 拼装查询 `path` 所在文件系统剩余空间(GB)的命令。
/// 与 [`check_server_env`] 的 df 口径一致:`-P` POSIX 单行格式 + `-BG` 以 GB 为块
/// 单位,`tail -1` 取数据行,`awk` 取第 4 列(Available);路径单引号包裹防注入。
pub fn df_free_gb_cmd(path: &str) -> String {
    format!(
        "df -PBG {} | tail -1 | awk '{{print $4}}'",
        shell_single_quote(path)
    )
}

/// 解析 `df -PBG` 第 4 列的 Available 值(如 `30G` / `30` / `0.5`)为 GB 数;
/// 空输出或非数字(BusyBox 等口径不一致的环境)返回 `None`。
/// 解析口径与 [`check_server_env`] 一致(trim 后去掉尾部 `G` 再按 f64 解析)。
pub fn parse_df_gb(raw: &str) -> Option<f64> {
    let trimmed = raw.trim().trim_end_matches('G').trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

/// 远端磁盘预检判定(纯函数):剩余空间(GB)小于 `need_bytes`(已含余量)换算的
/// GB 数 → 返回中文错误(含所需/实际 GB);`free_gb` 为 `None` 表示无法获取剩余
/// 空间,跳过预检返回 `Ok(())`(告警由调用方负责)。
pub fn precheck_remote_disk(free_gb: Option<f64>, need_bytes: u64) -> Result<(), String> {
    let free = match free_gb {
        Some(v) => v,
        None => return Ok(()),
    };
    let need_gb = need_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    if free < need_gb {
        return Err(format!(
            "服务器磁盘剩余空间不足:本次部署约需 {:.1} GB,Docker 根目录所在盘仅剩 {:.1} GB,请先清理服务器磁盘后重试",
            need_gb, free
        ));
    }
    Ok(())
}

// ===== 部署命令 =====

/// 发起部署:立即返回 `Ok(())`,管线在后台任务执行并通过事件推送进度。
#[tauri::command]
pub fn deploy(req: DeployRequest, app: AppHandle) -> Result<(), String> {
    spawn_deploy_task(app.clone(), async move { run_deploy(&app, req).await });
    Ok(())
}

/// 发起整栈部署(compose 多服务):立即返回 `Ok(())`,六步管线在后台任务
/// 执行并通过事件推送进度(1 分类确认 2 打包 3 上传 4 装载 5 拉取 6 启动)。
#[tauri::command]
pub fn deploy_stack(req: StackDeployRequest, app: AppHandle) -> Result<(), String> {
    spawn_deploy_task(app.clone(), async move { run_deploy_stack(&app, req).await });
    Ok(())
}

/// 后台部署任务的统一启动器:panic 兜底([`CatchPanic`])+ 收尾事件 + 部署历史,
/// 保证任何路径(成功/失败/panic)下 `deploy-done` 恰好 emit 一次;
/// 正常结束路径(成功/失败/取消)在 emit `deploy-done` 之后落一条部署历史记录
/// (由管线组装的 [`DeployRecord`],append 失败仅告警,不影响收尾)。
fn spawn_deploy_task<F>(app: AppHandle, fut: F)
where
    F: std::future::Future<Output = (Result<(), String>, DeployRecord)> + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        let (result, record) = match CatchPanic::new(fut).await {
            Ok(pair) => (pair.0, Some(pair.1)),
            Err(panic_info) => {
                log::error!("部署管线发生 panic: {}", panic_info);
                // 管线内组装的部署记录随 panic 丢失,此路径不写历史
                (
                    Err("部署过程发生内部错误,详情见日志".to_string()),
                    None,
                )
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
        // deploy-done 之后落地部署历史(成功/失败/取消统一记录)
        if let Some(record) = record {
            append_record(record);
        }
    });
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

/// 读取部署历史(倒序 = 最新在前;文件缺失/损坏时为空,由 history 层容错)。
#[tauri::command]
pub fn get_history() -> Result<Vec<DeployRecord>, String> {
    Ok(load_history().into_iter().rev().collect())
}

// ===== 部署管线(严格顺序,任一步失败即中止)=====

/// 部署管线入口:组装部署历史记录骨架(含开始计时),执行管线主体,
/// 出口填充 success/message/duration 后连同结果一起返回(由 spawn 层落历史)。
async fn run_deploy(app: &AppHandle, req: DeployRequest) -> (Result<(), String>, DeployRecord) {
    let started = std::time::Instant::now();
    // 骨架:server/project 名称由前置解析回填(前置失败时以 ID 兜底)
    let mut record = DeployRecord::new_skeleton(
        MODE_SINGLE,
        &req.server_id,
        &req.project_id,
        vec![req.image.clone()],
    );
    let result = run_deploy_steps(app, req, &mut record).await;
    record.success = result.is_ok();
    record.message = match &result {
        Ok(()) => "部署完成".to_string(),
        Err(e) => e.clone(),
    };
    record.duration_secs = started.elapsed().as_secs();
    (result, record)
}

/// 部署管线主体(严格顺序,任一步失败即中止)。`record` 为组装中的部署历史
/// 记录,随步骤推进回填服务器/项目名称与实际部署的镜像引用。
async fn run_deploy_steps(
    app: &AppHandle,
    req: DeployRequest,
    record: &mut DeployRecord,
) -> Result<(), String> {
    // ---- 步骤 0:前置 ----
    // 每次 deploy 开始时重置取消标志;结束时保持不变(取消后为 true,下次部署重置)
    reset_cancelled(app);

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &req.server_id)?.clone();
    let project = find_project(&cfg, &req.project_id)?.clone();
    record.server_name = server.name.clone();
    record.project_name = project.name.clone();
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
    // 历史记录登记实际部署的镜像引用(勾选日期标签时为生成的部署标签)
    record.images = vec![image_ref.clone()];

    // ---- 步骤 2:导出压缩 ----
    emit_progress(app, 2, 5, "导出压缩镜像");
    ensure_not_cancelled(app)?;
    let tar_name = format!("{}.tar.gz", uuid::Uuid::new_v4());
    let out_path = std::env::temp_dir().join(&tar_name);
    // 本地 tar 用完即删:Drop guard 覆盖成功/失败全部路径
    let _tar_guard = TempFileGuard(out_path.clone());

    // 空间预检:导出目标盘(临时目录所在盘)剩余空间 ≥ 镜像大小 × 1.5
    // (镜像大小暂存,供步骤 3 的远端磁盘预检复用,避免二次查询)
    let image_bytes = image_size(&image_ref);
    match image_bytes {
        Some(size) => check_export_disk_space(size)?,
        None => emit_log(app, "警告:无法获取镜像大小,跳过磁盘剩余空间检查"),
    }

    let total_bytes = export_image(app, &image_ref, &out_path).await?;
    emit_log(app, &format!("导出完成,共 {} MB", total_bytes / 1024 / 1024));

    // ---- 步骤 3:上传镜像 ----
    emit_progress(app, 3, 5, "上传镜像到服务器");
    ensure_not_cancelled(app)?;
    let mut client = SshClient::connect(&server, password.as_deref()).await?;
    // 远端磁盘预检:上传前确认 Docker 根目录所在盘剩余空间 ≥ 镜像大小 × 1.5
    // (镜像大小未知 → 告警跳过;不足 → 中文报错中止)
    let need_bytes = image_bytes.map(|size| (size as f64 * 1.5) as u64);
    remote_disk_precheck(app, &mut client, need_bytes).await?;
    // 镜像包同名即同内容(uuid 命名),启用断点续传
    upload_tar(app, &mut client, &out_path, &tar_name).await?;
    emit_log(app, "镜像上传完成");

    // ---- 步骤 4:同步文件 ----
    emit_progress(app, 4, 5, "同步项目文件");
    ensure_not_cancelled(app)?;
    sync_files(app, &mut client, &server, &project).await?;
    emit_log(app, "项目文件同步完成");
    // 部署前钩子(归入步骤 4:装载前执行,旧容器仍在运行;失败即中止部署)
    run_hook(app, &mut client, &project, HookKind::Pre, &server.remote_dir).await?;

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
    server_deploy(app, &mut client, &server, &project, &tar_name, retag).await?;

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
    let last_reported = Arc::new(AtomicU64::new(0));
    let app_for_cb = app.clone();
    let last = Arc::clone(&last_reported);

    run_save_gzip(image_ref, out_path, move |n| {
        let prev = last.load(Ordering::Relaxed);
        if n >= prev.saturating_add(LOG_PROGRESS_STEP) {
            last.store(n, Ordering::Relaxed);
            emit_log(&app_for_cb, &format!("已导出 {} MB", n / 1024 / 1024));
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
/// 取消 → 返回 [`CANCELLED_MSG`] 中止,不再重试;否则由调用方对**同一远端路径**
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
        let full_remote = remote_join(&server.remote_dir, &mapping.remote);
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

/// 步骤 5:服务器部署 —— `docker load` → 同步原标签 → `compose up -d` → 健康检查 → 部署后钩子 → 删除远端 tar。
///
/// 每条命令超时 600 秒(清理 60 秒),输出实时转发到 `deploy-log`,收到输出行时检查取消标志。
/// `retag` 为 `Some((日期tag, 原引用))` 时,装载后把原引用(如 myapp:latest)也指向
/// 新镜像,否则 compose 引用原 tag 时感知不到变化、不会重建容器。
/// up 之后先做健康检查(未启用则跳过),再执行部署后钩子(失败仅告警),
/// 最后清理远端 tar。
async fn server_deploy(
    app: &AppHandle,
    client: &mut SshClient,
    server: &ServerConfig,
    project: &ProjectConfig,
    tar_name: &str,
    retag: Option<(String, String)>,
) -> Result<(), String> {
    let remote_tar = remote_join("/tmp", tar_name);

    // 5.1 加载镜像
    emit_log(app, &format!("加载镜像到服务器: docker load -i {}", remote_tar));
    let load_cmd = format!("docker load -i {}", shell_single_quote(&remote_tar));
    exec_forwarded(app, client, &load_cmd, 600).await?;

    // 5.2 同步原标签(仅勾选日期标签时):零拷贝的指针移动,让 compose 的变更检测生效
    if let Some((date_tag, original)) = &retag {
        let tag_cmd = docker_tag_cmd(date_tag, original);
        emit_log(app, &format!("同步原标签: {}", tag_cmd));
        exec_forwarded(app, client, &tag_cmd, 60).await?;
    }

    // 5.3 启动服务(cd 到远端目录后按相对 compose 文件启动)
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

    // 5.4 健康检查(up 后按预算轮询服务状态;health_wait_secs=0 时跳过)
    health_check(app, client, project, &server.remote_dir, &project.compose_file).await?;

    // 5.5 部署后钩子(健康检查通过后执行;失败仅告警,不影响部署结果)
    run_hook(app, client, project, HookKind::Post, &server.remote_dir).await?;

    // 5.6 清理远端 tar(尽力而为,失败不影响部署结果)
    let rm_cmd = format!("rm -f {}", shell_single_quote(&remote_tar));
    if let Err(e) = exec_forwarded(app, client, &rm_cmd, 60).await {
        emit_log(app, &format!("警告:清理远端临时文件失败: {}", e));
    }
    Ok(())
}

// ===== 部署钩子 + 健康检查(Task 3)=====

/// 部署钩子类型:`Pre` = 部署前(装载前,旧容器仍在运行,失败中止部署),
/// `Post` = 部署后(健康检查通过后,失败仅告警)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookKind {
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
        Err(e) => match which {
            HookKind::Pre => Err(format!("{}执行失败,部署中止: {}", which.label(), e)),
            HookKind::Post => {
                if e == CANCELLED_MSG {
                    Err(e)
                } else {
                    emit_log(
                        app,
                        &format!("警告:{}执行失败(不影响部署结果): {}", which.label(), e),
                    );
                    Ok(())
                }
            }
        },
    }
}

/// up 之后的健康检查:`health_wait_secs > 0` 时,每 [`HEALTH_POLL_INTERVAL_SECS`]
/// 秒轮询一次 [`compose_ps_json_cmd`](`docker compose ps --all --format json`),
/// 预算为 `health_wait_secs` 秒;判定见 [`health_verdict`]。
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
) -> Result<(), String> {
    if project.health_wait_secs == 0 {
        emit_log(app, "健康检查未启用,跳过");
        return Ok(());
    }
    let budget = Duration::from_secs(project.health_wait_secs as u64);
    let started = std::time::Instant::now();
    let ps_cmd = compose_ps_json_cmd(remote_dir, compose_file);
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
                dump_compose_logs(app, client, remote_dir, compose_file).await;
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
            dump_compose_logs(app, client, remote_dir, compose_file).await;
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
async fn dump_compose_logs(
    app: &AppHandle,
    client: &mut SshClient,
    remote_dir: &str,
    compose_file: &str,
) {
    emit_log(app, "正在获取服务最近日志(最后 50 行):");
    let cmd = compose_logs_cmd(remote_dir, compose_file);
    if let Err(e) = exec_forwarded(app, client, &cmd, SSH_EXEC_TIMEOUT_SECS).await {
        emit_log(app, &format!("警告:获取服务日志失败: {}", e));
    }
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
    exec_forwarded_inner(app, client, cmd, timeout_secs, 0).await
}

/// [`exec_forwarded`] 的实现:`tail_lines > 0` 时,非零退出的错误信息额外并入
/// 远端输出末尾 `tail_lines` 行(供调用方依据输出内容做判定/提示,如
/// [`augment_pull_error`]);其余行为(日志逐行转发、超时、取消)不变。
async fn exec_forwarded_inner(
    app: &AppHandle,
    client: &mut SshClient,
    cmd: &str,
    timeout_secs: u64,
    tail_lines: usize,
) -> Result<(), String> {
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let app_for_cb = app.clone();
    let cancel_flag = Arc::clone(&saw_cancel);
    let tail = Arc::new(Mutex::new(Vec::new()));
    let tail_for_cb = Arc::clone(&tail);
    let mut on_output = move |line: &str| {
        emit_log(&app_for_cb, line);
        if tail_lines > 0 {
            let mut buf = tail_for_cb.lock().unwrap_or_else(|e| e.into_inner());
            buf.push(line.trim_end().to_string());
            if buf.len() > tail_lines {
                buf.remove(0);
            }
        }
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
        let mut msg = format!("远端命令执行失败(退出码 {}): {}", code, cmd);
        if tail_lines > 0 {
            let buf = tail.lock().unwrap_or_else(|e| e.into_inner());
            if !buf.is_empty() {
                msg.push_str("\n远端输出(末尾):\n");
                msg.push_str(&buf.join("\n"));
            }
        }
        return Err(msg);
    }
    // 末尾取消复查:命令可能全程无输出、回调一次都未触发,
    // 结束后再查一次取消标志,保证取消后不会把该步误报为成功。
    if is_cancelled(app) {
        return Err(CANCELLED_MSG.to_string());
    }
    Ok(())
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

// ===== 整栈部署管线(六步,任一步失败即中止)=====

/// 整栈部署管线入口:组装部署历史记录骨架(含开始计时),执行管线主体,
/// 出口填充 success/message/duration 后连同结果一起返回(由 spawn 层落历史)。
async fn run_deploy_stack(
    app: &AppHandle,
    req: StackDeployRequest,
) -> (Result<(), String>, DeployRecord) {
    let started = std::time::Instant::now();
    // 骨架:镜像列表取本地传输的服务镜像;server/project 名称由前置解析回填
    let mut record = DeployRecord::new_skeleton(
        MODE_STACK,
        &req.server_id,
        &req.project_id,
        stack_record_images(&req.services),
    );
    let result = run_deploy_stack_steps(app, req, &mut record).await;
    record.success = result.is_ok();
    record.message = match &result {
        Ok(()) => "部署完成".to_string(),
        Err(e) => e.clone(),
    };
    record.duration_secs = started.elapsed().as_secs();
    (result, record)
}

/// 整栈部署管线主体(六步,任一步失败即中止)。`record` 为组装中的部署历史
/// 记录,前置解析后回填服务器/项目名称。
async fn run_deploy_stack_steps(
    app: &AppHandle,
    req: StackDeployRequest,
    record: &mut DeployRecord,
) -> Result<(), String> {
    // ---- 前置:找 server/project、解析密码 ----
    // 每次部署开始时重置取消标志(与单镜像 run_deploy 一致)
    reset_cancelled(app);

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &req.server_id)?.clone();
    let project = find_project(&cfg, &req.project_id)?.clone();
    record.server_name = server.name.clone();
    record.project_name = project.name.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        req.password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;

    // ---- 步骤 1:分类确认 ----
    emit_progress(app, 1, 6, "分类确认");
    ensure_not_cancelled(app)?;
    validate_stack_choices(&req.services)?;
    let (local_choices, pull_choices) = group_by_mode(&req.services);
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
            req.services.len(),
            local_choices.len(),
            pull_choices.len()
        ),
    );

    // ---- 步骤 2:打包 ----
    emit_progress(app, 2, 6, "打包");
    ensure_not_cancelled(app)?;
    let tars = pack_local_images(app, &local_choices).await?;

    // ---- 步骤 3:上传 ----
    emit_progress(app, 3, 6, "上传");
    ensure_not_cancelled(app)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref()),
    )
    .await?;

    // 远端磁盘预检:上传前确认 Docker 根目录所在盘剩余空间 ≥ Local 镜像字节总和 × 1.5
    // (与本地导出预检同一 sum 口径;大小未知 → 告警跳过;不足 → 中文报错中止)
    let local_sizes: Vec<Option<u64>> = local_choices
        .iter()
        .map(|s| image_size(&s.image))
        .collect();
    let need_bytes = sum_sizes(&local_sizes).map(|total| (total as f64 * 1.5) as u64);
    remote_disk_precheck(app, &mut client, need_bytes).await?;

    // 远端建本次发布目录 <remote_dir>/releases/<时间戳>/(mkdir -p 连带创建 remote_dir)
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let release_dir = releases_dir(&server.remote_dir, &ts);
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

    upload_compose_files(app, &mut client, &server, &project).await?;
    upload_local_tars(app, &mut client, &tars, &release_dir).await?;
    sync_files(app, &mut client, &server, &project).await?;
    emit_log(app, "上传完成");
    // 部署前钩子(归入步骤 3:装载/拉取前执行,旧容器仍在运行;失败即中止部署)
    run_hook(app, &mut client, &project, HookKind::Pre, &server.remote_dir).await?;

    // ---- 步骤 4:装载 ----
    emit_progress(app, 4, 6, "装载");
    ensure_not_cancelled(app)?;
    let tar_count = tars.files.len();
    if tar_count == 0 {
        emit_log(app, "无本地镜像包,跳过装载");
    }
    for (i, (_, name)) in tars.files.iter().enumerate() {
        ensure_not_cancelled(app)?;
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

    // ---- 步骤 5:拉取 ----
    emit_progress(app, 5, 6, "拉取");
    ensure_not_cancelled(app)?;
    // override 文件名:按 compose 副本目录检测(与 upload_compose_files 上传的
    // 一致),pull / up 均按同序 -f 传入,保证远端合并结果与本地解析一致
    let override_names = compose_override_names(&project.compose_file);
    let pull_names: Vec<String> = pull_choices.iter().map(|s| s.service.clone()).collect();
    if pull_names.is_empty() {
        emit_log(app, "无需要服务器拉取的服务,跳过拉取");
    } else {
        let remote_compose = remote_compose_path(&server.remote_dir);
        let pull_cmd =
            compose_pull_cmd(&server.remote_dir, &remote_compose, &override_names, &pull_names);
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
            if e == CANCELLED_MSG {
                e
            } else {
                augment_pull_error(&format!(
                    "{}(请检查服务器能否出网访问镜像仓库,或在服务分类中把这些服务改为本地传输)",
                    e
                ))
            }
        })?;
    }

    // ---- 步骤 6:启动 ----
    emit_progress(app, 6, 6, "启动");
    ensure_not_cancelled(app)?;
    let remote_compose = remote_compose_path(&server.remote_dir);
    let up_cmd = compose_up_cmd(&server.remote_dir, &remote_compose, &override_names);
    emit_log(app, &format!("启动服务: {}", up_cmd));
    exec_forwarded(app, &mut client, &up_cmd, STACK_COMPOSE_TIMEOUT_SECS).await?;

    // 健康检查(up 后按预算轮询服务状态;health_wait_secs=0 时跳过)
    health_check(app, &mut client, &project, &server.remote_dir, &remote_compose).await?;

    // 部署后钩子(健康检查通过后执行;失败仅告警,不影响部署结果)
    run_hook(app, &mut client, &project, HookKind::Post, &server.remote_dir).await?;

    // ---- 收尾:清理旧 releases(仅留最新 5 个,尽力而为,失败仅告警)----
    ensure_not_cancelled(app)?;
    let cleanup_cmd = cleanup_releases_cmd(&server.remote_dir);
    if let Err(e) = exec_forwarded(app, &mut client, &cleanup_cmd, SSH_EXEC_TIMEOUT_SECS).await {
        emit_log(app, &format!("警告:清理旧 releases 目录失败: {}", e));
    }

    // 本地 tar 由 tars 的 TempFileGuard 在本函数返回(成功/失败)时统一删除
    emit_log(app, "整栈部署完成");
    Ok(())
}

/// 步骤 2 打包出的本地镜像包集合。
struct LocalTars {
    /// `(本地路径, 远端文件名)`,按服务顺序排列(与串行打包时的顺序一致)
    files: Vec<(PathBuf, String)>,
    /// Drop 守卫:管线函数返回(成功或失败)时删除全部本地 tar
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
/// 半成品 tar 都随管线返回统一删除。
async fn pack_local_images(
    app: &AppHandle,
    local: &[&StackServiceChoice],
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
    let mut outputs: Vec<(PathBuf, String)> = Vec::with_capacity(n);
    for _ in 0..n {
        let tar_name = format!("{}.tar.gz", uuid::Uuid::new_v4());
        let out_path = std::env::temp_dir().join(&tar_name);
        outputs.push((out_path, tar_name));
    }
    let guards: Vec<TempFileGuard> = outputs.iter().map(|(p, _)| TempFileGuard(p.clone())).collect();
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
        return Err(CANCELLED_MSG.to_string());
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
            remote_join(&server.remote_dir, compose_name)
        ),
    );
    // compose 副本 / .env / override 内容可变且远端同名,不做续传(全新写覆盖)
    client
        .sftp_upload(&compose_local, &server.remote_dir, compose_name, false, &|_, _| {})
        .await?;
    if let Some(env_path) = compose_local.parent().map(|p| p.join(".env")) {
        if env_path.is_file() {
            emit_log(app, "上传 compose 同目录 .env 文件");
            client
                .sftp_upload(&env_path, &server.remote_dir, ".env", false, &|_, _| {})
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
                remote_join(&server.remote_dir, &name)
            ),
        );
        client
            .sftp_upload(&ov_path, &server.remote_dir, &name, false, &|_, _| {})
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
async fn upload_local_tars(
    app: &AppHandle,
    client: &mut SshClient,
    tars: &LocalTars,
    release_dir: &str,
) -> Result<(), String> {
    let n = tars.files.len();
    if n == 0 {
        emit_log(app, "无本地镜像包需要上传");
        return Ok(());
    }
    for (i, (path, name)) in tars.files.iter().enumerate() {
        ensure_not_cancelled(app)?;
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
fn compose_override_names(compose_file: &str) -> Vec<String> {
    let dir = Path::new(compose_file).parent().unwrap_or_else(|| Path::new(""));
    find_override_files(dir)
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .collect()
}

/// 拼装 releases 清理命令:按修改时间保留最新 5 个版本目录,其余删除
/// (`tail -n +6` 从第 6 行起取;`xargs -r` 无输入时不执行 rm)。
pub fn cleanup_releases_cmd(remote_dir: &str) -> String {
    format!(
        "ls -1dt {}/*/ | tail -n +6 | xargs -r rm -rf",
        shell_single_quote(&remote_join(remote_dir, "releases"))
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
fn compose_file_flags(compose_file: &str, overrides: &[String]) -> String {
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

// ===== 钩子/健康检查纯逻辑(便于单测,Task 3)=====

/// 拼装钩子命令:`cd '<remote_dir>' && ( <cmd> )`。
///
/// `cmd` 是用户在项目配置里自己填写的**可信复合命令**(可含 `&&`/`;`/重定向/
/// 管道等),原样拼入、不加引号 —— 相当于用户在本机执行自定义脚本,`cmd`
/// 内容不是防注入边界;`remote_dir` 是程序拼装的路径,单引号转义防注入。
pub fn hook_cmd(remote_dir: &str, cmd: &str) -> String {
    format!("cd {} && ( {} )", shell_single_quote(remote_dir), cmd)
}

/// 拼装查询 compose 服务状态的命令(`--all` 含已退出/未启动容器,每容器一行
/// JSON,含 Service/State/Health/ExitCode)。
///
/// 必须带 `--all`:默认的 `docker compose ps` 只列 running 容器,"启动即退出
/// 且其余服务健康"的多服务栈会因故障服务缺席而被误判通过。
pub fn compose_ps_json_cmd(remote_dir: &str, compose_file: &str) -> String {
    format!(
        "cd {} && docker compose -f {} ps --all --format json",
        shell_single_quote(remote_dir),
        shell_single_quote(compose_file)
    )
}

/// 拼装查看 compose 各服务最近日志的命令(最后 50 行)。
pub fn compose_logs_cmd(remote_dir: &str, compose_file: &str) -> String {
    format!(
        "cd {} && docker compose -f {} logs --tail 50",
        shell_single_quote(remote_dir),
        shell_single_quote(compose_file)
    )
}

/// 单轮健康检查判定结果(见 [`health_verdict`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthVerdict {
    /// 全部服务 running 且健康检查通过(或无 healthcheck)
    Pass,
    /// 任一服务进入失败终态(Restarting/Dead,或 Exited 且退出码非零/缺失),
    /// 立即中止
    Unhealthy { service: String, state: String },
    /// 尚无法判定(解析失败、服务仍在启动/健康检查进行中、一次性服务已正常
    /// 退出等),继续轮询;`pending` 携带最近观察到的未就绪服务与展示状态,
    /// 供预算耗尽时报错展示;`exited_zero` 表示该服务"已退出(退出码 0)"
    /// (典型为一次性初始化服务,预算耗尽的报错需据此提示关闭健康检查)。
    Indeterminate {
        pending: Option<(String, String)>,
        exited_zero: bool,
    },
}

/// 对 `docker compose ps --all --format json` 的输出行做单轮健康判定(纯函数)。
///
/// - 逐行解析 JSON(容忍旧版 compose 一次性输出 JSON 数组;非 JSON 行忽略);
///   解析不出任何服务记录 → `Indeterminate`;
/// - restarting/dead → 立即 `Unhealthy{ service, state }`;
/// - exited:按 `ExitCode` 区分(存在版本差异)——非零 → 立即 `Unhealthy`
///   (state 展示 "exited(非零退出)");`0` → 一次性服务正常退出,不算失败,
///   归入 `Indeterminate`(`pending` 展示 "已退出(退出码 0)" 且 `exited_zero`
///   为 true,预算耗尽时由调用方附加一次性服务提示);`ExitCode` 字段缺失 →
///   保守按 `Unhealthy`(宁误报不漏报);
/// - 全部服务 state=="running" 且(无 Health 字段/为空 或 "healthy")→ `Pass`;
/// - 其余(服务仍在启动、health 为 starting/unhealthy 等)→ `Indeterminate`,
///   `pending` 取第一个未就绪服务(有 Health 且非 healthy 时展示 Health,
///   否则展示容器 state)。
pub fn health_verdict(lines: &[&str]) -> HealthVerdict {
    // 收集解析出的记录
    let mut entries: Vec<PsEntry> = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            // 旧版 compose 一次性输出 JSON 数组
            Ok(serde_json::Value::Array(items)) => {
                for item in &items {
                    if let Some(e) = parse_ps_entry(item) {
                        entries.push(e);
                    }
                }
            }
            Ok(v) => {
                if let Some(e) = parse_ps_entry(&v) {
                    entries.push(e);
                }
            }
            // 非 JSON 行(警告、日志前缀等)忽略
            Err(_) => {}
        }
    }
    if entries.is_empty() {
        return HealthVerdict::Indeterminate {
            pending: None,
            exited_zero: false,
        };
    }
    // 失败终态:立即失败(取先出现者);exited 需结合 ExitCode 区分一次性服务
    for e in &entries {
        match e.state.to_ascii_lowercase().as_str() {
            "restarting" | "dead" => {
                return HealthVerdict::Unhealthy {
                    service: e.service.clone(),
                    state: e.state.clone(),
                };
            }
            "exited" => match e.exit_code {
                // 一次性服务正常退出:不算失败,进入下方 pending 逻辑
                Some(0) => {}
                Some(_) => {
                    return HealthVerdict::Unhealthy {
                        service: e.service.clone(),
                        state: "exited(非零退出)".to_string(),
                    };
                }
                // ExitCode 字段缺失(版本差异)→ 保守按失败(宁误报不漏报)
                None => {
                    return HealthVerdict::Unhealthy {
                        service: e.service.clone(),
                        state: e.state.clone(),
                    };
                }
            },
            _ => {}
        }
    }
    // 逐服务判定 running + 健康
    let mut pending: Option<(String, String)> = None;
    let mut exited_zero = false;
    for e in &entries {
        let state_ok = e.state.eq_ignore_ascii_case("running");
        let health_ok = match e.health.as_deref() {
            None => true, // 无 healthcheck(或输出为空)
            Some(h) => h.eq_ignore_ascii_case("healthy"),
        };
        if state_ok && health_ok {
            continue;
        }
        if pending.is_none() {
            // 已退出(退出码 0)的服务永不满足"全部 running",展示专用状态,
            // 预算耗尽时调用方据此附加一次性服务提示
            let is_exited_zero =
                e.state.eq_ignore_ascii_case("exited") && e.exit_code == Some(0);
            let shown = if is_exited_zero {
                "已退出(退出码 0)".to_string()
            } else {
                // 展示口径:健康检查未通过时优先展示 Health(如 starting/unhealthy),
                // 否则展示容器状态(如 created/paused)
                e.health.clone().unwrap_or_else(|| e.state.clone())
            };
            pending = Some((e.service.clone(), shown));
            exited_zero = is_exited_zero;
        }
    }
    match pending {
        None => HealthVerdict::Pass,
        Some(p) => HealthVerdict::Indeterminate {
            pending: Some(p),
            exited_zero,
        },
    }
}

/// 单条 `compose ps --format json` 记录的解析结果。
struct PsEntry {
    service: String,
    state: String,
    health: Option<String>,
    exit_code: Option<i64>,
}

/// 从单条 `compose ps --format json` 记录提取服务信息。
///
/// 服务名优先 `Service` 字段,缺失时回退容器 `Name`;State 缺失视为无效记录;
/// Health 兼容三种形态:缺失/`null`/空串 → `None`(视为无 healthcheck)、
/// 字符串原样、嵌套对象取其 `Status` 字段;ExitCode 非整数/缺失 → `None`
/// (调用方对 exited 保守判失败)。
fn parse_ps_entry(v: &serde_json::Value) -> Option<PsEntry> {
    let state = v.get("State")?.as_str()?.to_string();
    let service = v
        .get("Service")
        .and_then(|s| s.as_str())
        .or_else(|| v.get("Name").and_then(|s| s.as_str()))
        .unwrap_or("<unknown>")
        .to_string();
    let health = match v.get("Health") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) if s.is_empty() => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(other) => other.get("Status").and_then(|s| s.as_str()).map(String::from),
    };
    let exit_code = v.get("ExitCode").and_then(|c| c.as_i64());
    Some(PsEntry {
        service,
        state,
        health,
        exit_code,
    })
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

    #[test]
    fn test_import_compose_copies_override_files() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 源 compose + 同目录 override(合并后 web 的 image 以 override 为准)
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("docker-compose.yml");
        std::fs::write(
            &source,
            "name: demo\nservices:\n  web:\n    build: ./web\n    image: myapp:1\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        std::fs::write(
            src_dir.join("compose.override.yaml"),
            "services:\n  web:\n    image: myapp:2\n",
        )
        .unwrap();
        // 非 override 命名的文件不应被复制
        std::fs::write(src_dir.join("other.yaml"), "services: {}\n").unwrap();

        let project =
            import_compose(source.to_string_lossy().to_string(), "override 栈".into()).unwrap();
        let copy_dir = PathBuf::from(&project.compose_file)
            .parent()
            .unwrap()
            .to_path_buf();

        // override 副本同名落在 stacks/<uuid>/ 下
        assert!(
            copy_dir.join("compose.override.yaml").is_file(),
            "override 副本应存在: {}",
            copy_dir.display()
        );
        assert!(
            !copy_dir.join("other.yaml").exists(),
            "非 override 文件不应被复制"
        );
        // 解析时已合并 override:web 的 image 以 override 为准(build 保留 → Local)
        let web = project.service_overrides.iter().find(|o| o.service == "web").unwrap();
        assert_eq!(web.mode, TransferMode::Local);
        let db = project.service_overrides.iter().find(|o| o.service == "db").unwrap();
        assert_eq!(db.mode, TransferMode::Pull);

        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== 整栈部署:请求反序列化(前端契约,snake_case)=====

    /// 构造服务分类项的便捷函数。
    fn choice(service: &str, image: &str, mode: TransferMode) -> StackServiceChoice {
        StackServiceChoice {
            service: service.into(),
            image: image.into(),
            mode,
        }
    }

    #[test]
    fn test_stack_deploy_request_deserialize() {
        let json = r#"{
            "project_id": "p1",
            "server_id": "s1",
            "services": [
                {"service": "web", "image": "myapp:1", "mode": "Local"},
                {"service": "db", "image": "", "mode": "Pull"}
            ],
            "password_plain": null
        }"#;
        let req: StackDeployRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.project_id, "p1");
        assert_eq!(req.server_id, "s1");
        assert_eq!(req.services.len(), 2);
        assert_eq!(req.services[0].mode, TransferMode::Local);
        assert_eq!(req.services[1].mode, TransferMode::Pull);
        assert_eq!(req.services[1].image, "");
        assert_eq!(req.password_plain, None);
    }

    // ===== 步骤 1:validate_stack_choices =====

    #[test]
    fn test_validate_stack_choices_rejects_empty() {
        let err = validate_stack_choices(&[]).unwrap_err();
        assert!(err.contains("为空"), "实际: {}", err);
    }

    #[test]
    fn test_validate_stack_choices_local_image_required() {
        let services = vec![
            choice("web", "myapp:1", TransferMode::Local),
            choice("db", "   ", TransferMode::Local),
        ];
        let err = validate_stack_choices(&services).unwrap_err();
        assert!(err.contains("db"), "错误应含服务名: {}", err);
        assert!(err.contains("本地传输"), "实际: {}", err);
    }

    #[test]
    fn test_validate_stack_choices_ok_allows_empty_pull_image() {
        // Pull 类镜像为空合法(服务器自行拉取)
        let services = vec![
            choice("web", "myapp:1", TransferMode::Local),
            choice("db", "", TransferMode::Pull),
        ];
        validate_stack_choices(&services).unwrap();
    }

    // ===== 步骤 2:group_by_mode / sum_sizes =====

    #[test]
    fn test_group_by_mode() {
        let services = vec![
            choice("a", "a:1", TransferMode::Local),
            choice("b", "b:1", TransferMode::Pull),
            choice("c", "c:1", TransferMode::Local),
        ];
        let (local, pull) = group_by_mode(&services);
        let local_names: Vec<&str> = local.iter().map(|s| s.service.as_str()).collect();
        let pull_names: Vec<&str> = pull.iter().map(|s| s.service.as_str()).collect();
        assert_eq!(local_names, vec!["a", "c"]);
        assert_eq!(pull_names, vec!["b"]);
    }

    #[test]
    fn test_group_by_mode_all_pull() {
        // 全 Pull:local 为空,打包/装载/上传镜像包均跳过
        let services = vec![choice("db", "", TransferMode::Pull)];
        let (local, pull) = group_by_mode(&services);
        assert!(local.is_empty());
        assert_eq!(pull.len(), 1);
    }

    #[test]
    fn test_stack_record_images() {
        // 仅登记本地传输且镜像非空的服务,按服务顺序
        let services = vec![
            choice("web", "myapp:1", TransferMode::Local),
            choice("db", "", TransferMode::Pull),
            choice("cache", "redis:7", TransferMode::Local),
            choice("worker", "   ", TransferMode::Local),
        ];
        assert_eq!(
            stack_record_images(&services),
            vec!["myapp:1".to_string(), "redis:7".to_string()]
        );
        // 全 Pull → 空列表
        assert!(stack_record_images(&[choice("db", "", TransferMode::Pull)]).is_empty());
    }

    #[test]
    fn test_sum_sizes() {
        assert_eq!(sum_sizes(&[]), Some(0));
        assert_eq!(sum_sizes(&[Some(1), Some(2)]), Some(3));
        // 任一项未知 → 整体未知(跳过磁盘预检)
        assert_eq!(sum_sizes(&[Some(1), None, Some(2)]), None);
        // 溢出保护
        assert_eq!(sum_sizes(&[Some(u64::MAX), Some(1)]), None);
    }

    // ===== 步骤 3/4:releases 路径拼装 =====

    #[test]
    fn test_releases_dir() {
        assert_eq!(
            releases_dir("/opt/app", "20260829-101010"),
            "/opt/app/releases/20260829-101010"
        );
        // remote_dir 尾部斜杠被吸收
        assert_eq!(
            releases_dir("/opt/app/", "20260829-101010"),
            "/opt/app/releases/20260829-101010"
        );
    }

    #[test]
    fn test_remote_compose_path() {
        assert_eq!(
            remote_compose_path("/opt/app"),
            "/opt/app/docker-compose.yml"
        );
    }

    // ===== 步骤 5:compose_pull_cmd =====

    #[test]
    fn test_compose_pull_cmd() {
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &[],
                &["web".to_string(), "db".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' pull 'web' 'db'"
        );
        // 有 override:按检测顺序追加 -f(compose 后者覆盖前者)
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["compose.override.yaml".to_string(), "docker-compose.override.yml".to_string()],
                &["web".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'compose.override.yaml' -f 'docker-compose.override.yml' pull 'web'"
        );
    }

    #[test]
    fn test_compose_pull_cmd_quotes_block_injection() {
        // 服务名内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &[],
                &["a'; rm -rf /".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' pull 'a'\\''; rm -rf /'"
        );
        // override 文件名同样转义
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["o'.yaml".to_string()],
                &["web".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'o'\\''.yaml' pull 'web'"
        );
    }

    // ===== 步骤 6:compose_up_cmd =====

    #[test]
    fn test_compose_up_cmd() {
        assert_eq!(
            compose_up_cmd("/opt/app", "/opt/app/docker-compose.yml", &[]),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' up -d"
        );
        assert_eq!(
            compose_up_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["compose.override.yaml".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'compose.override.yaml' up -d"
        );
    }

    // ===== Task 5:augment_pull_error 私有仓库认证提示 =====

    #[test]
    fn test_augment_pull_error_auth_variants() {
        for fragment in [
            "unauthorized: authentication required",
            "HTTP 401 Unauthorized",
            "denied: requested access to the resource is denied",
            "_ERROR: PERMISSION DENIED_",
        ] {
            let err = format!("远端命令执行失败(退出码 1): pull ({})", fragment);
            let msg = augment_pull_error(&err);
            assert!(
                msg.contains("检测到私有仓库认证问题,请先在服务器上 docker login 对应 registry"),
                "应追加登录提示: {}", msg
            );
            assert!(msg.starts_with(&err), "原错误应保留在前: {}", msg);
        }
    }

    #[test]
    fn test_augment_pull_error_other_failure_unchanged() {
        let err = "远端命令执行失败(退出码 1): pull (no such host)";
        assert_eq!(augment_pull_error(err), err);
        assert_eq!(augment_pull_error(""), "");
    }

    // ===== 收尾:cleanup_releases_cmd =====

    #[test]
    fn test_cleanup_releases_cmd() {
        assert_eq!(
            cleanup_releases_cmd("/opt/app"),
            "ls -1dt '/opt/app/releases'/*/ | tail -n +6 | xargs -r rm -rf"
        );
    }

    #[test]
    fn test_cleanup_releases_cmd_escapes_quote() {
        assert_eq!(
            cleanup_releases_cmd("/op't"),
            "ls -1dt '/op'\\''t/releases'/*/ | tail -n +6 | xargs -r rm -rf"
        );
    }

    // ===== 单镜像步骤 5.2:docker_tag_cmd =====

    #[test]
    fn test_docker_tag_cmd() {
        assert_eq!(
            docker_tag_cmd("myapp:20260829-143000", "myapp:latest"),
            "docker tag 'myapp:20260829-143000' 'myapp:latest'"
        );
    }

    #[test]
    fn test_docker_tag_cmd_escapes_quote() {
        assert_eq!(
            docker_tag_cmd("my'app:20260829", "my'app:latest"),
            "docker tag 'my'\\''app:20260829' 'my'\\''app:latest'"
        );
    }

    // ===== Task 2:远端磁盘预检命令拼装 =====

    #[test]
    fn test_docker_root_cmd() {
        assert_eq!(docker_root_cmd(), "docker info -f '{{.DockerRootDir}}'");
    }

    #[test]
    fn test_df_free_gb_cmd() {
        assert_eq!(
            df_free_gb_cmd("/var/lib/docker"),
            "df -PBG '/var/lib/docker' | tail -1 | awk '{print $4}'"
        );
    }

    #[test]
    fn test_df_free_gb_cmd_escapes_quote() {
        // 路径内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            df_free_gb_cmd("/var/li'b"),
            "df -PBG '/var/li'\\''b' | tail -1 | awk '{print $4}'"
        );
    }

    #[test]
    fn test_parse_df_gb() {
        assert_eq!(parse_df_gb("30G\n"), Some(30.0));
        assert_eq!(parse_df_gb("  12 "), Some(12.0));
        assert_eq!(parse_df_gb("0.5"), Some(0.5));
        // 空输出 / 非数字(BusyBox 等口径不一致)→ None,调用方跳过预检
        assert_eq!(parse_df_gb(""), None);
        assert_eq!(parse_df_gb("   \n"), None);
        assert_eq!(parse_df_gb("N/A"), None);
    }

    // ===== Task 2:precheck_remote_disk 判定(Ok / None 跳过 / 不足)=====

    #[test]
    fn test_precheck_remote_disk_ok() {
        assert!(precheck_remote_disk(Some(20.0), 15 * 1024 * 1024 * 1024).is_ok());
        // 恰好等于需求(边界)也通过
        assert!(precheck_remote_disk(Some(15.0), 15 * 1024 * 1024 * 1024).is_ok());
        // 需求为 0(如全 Pull)恒通过
        assert!(precheck_remote_disk(Some(0.0), 0).is_ok());
    }

    #[test]
    fn test_precheck_remote_disk_none_skips() {
        // 无法获取剩余空间 → 跳过预检(告警由调用方负责)
        assert!(precheck_remote_disk(None, u64::MAX).is_ok());
    }

    #[test]
    fn test_precheck_remote_disk_insufficient() {
        let err = precheck_remote_disk(Some(10.0), 15 * 1024 * 1024 * 1024).unwrap_err();
        assert!(err.contains("磁盘剩余空间不足"), "实际: {}", err);
        assert!(err.contains("15.0"), "错误应含所需 GB: {}", err);
        assert!(err.contains("10.0"), "错误应含实际 GB: {}", err);
        assert!(err.contains("清理服务器磁盘"), "实际: {}", err);
    }

    // ===== Task 2:prune_cmd =====

    #[test]
    fn test_prune_cmd() {
        assert_eq!(
            prune_cmd(),
            "docker image prune -f; docker container prune -f"
        );
    }

    // ===== Task 4 修复轮:镜像包上传失败后同路径重试一次 =====

    #[test]
    fn test_upload_retry_failure_msg_keeps_both_errors() {
        // 重试失败时报错应同时携带两次失败信息,便于对照断点与失败原因
        let msg = upload_retry_failure_msg("SFTP 写入远端文件失败 (…)", "SSH 连接失败");
        assert!(msg.contains("重试仍失败"), "实际: {}", msg);
        assert!(msg.contains("SFTP 写入远端文件失败"), "应含重试错误: {}", msg);
        assert!(msg.contains("首次失败:SSH 连接失败"), "应含首次错误: {}", msg);
    }

    // ===== Task 3:钩子命令拼装 =====

    #[test]
    fn test_hook_cmd() {
        assert_eq!(
            hook_cmd("/opt/app", "docker image prune -f"),
            "cd '/opt/app' && ( docker image prune -f )"
        );
    }

    #[test]
    fn test_hook_cmd_escapes_remote_dir_quote() {
        // remote_dir 单引号转义;钩子命令是用户配置的可信复合命令,原样拼入
        // (支持 && / ; / 重定向,不整体加引号 —— 非防注入边界,见 hook_cmd 文档)
        assert_eq!(
            hook_cmd("/op't", "a && b; c > /tmp/log"),
            "cd '/op'\\''t' && ( a && b; c > /tmp/log )"
        );
    }

    // ===== Task 3:compose ps / logs 命令拼装 =====

    #[test]
    fn test_compose_ps_json_cmd() {
        assert_eq!(
            compose_ps_json_cmd("/opt/app", "/opt/app/docker-compose.yml"),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' ps --all --format json"
        );
    }

    #[test]
    fn test_compose_ps_json_cmd_escapes_quote() {
        // 路径内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            compose_ps_json_cmd("/opt/app", "/op't.yml"),
            "cd '/opt/app' && docker compose -f '/op'\\''t.yml' ps --all --format json"
        );
    }

    #[test]
    fn test_compose_logs_cmd() {
        assert_eq!(
            compose_logs_cmd("/opt/app", "./docker-compose.yml"),
            "cd '/opt/app' && docker compose -f './docker-compose.yml' logs --tail 50"
        );
    }

    // ===== Task 3:health_verdict 判定 =====

    /// 构造一行 `compose ps --format json` 输出
    /// (health/exit_code 传 None 表示不带该字段)。
    fn ps_line(service: &str, state: &str, health: Option<&str>, exit_code: Option<i64>) -> String {
        let mut json = format!(r#"{{"Service":"{}","State":"{}""#, service, state);
        if let Some(h) = health {
            json.push_str(&format!(r#","Health":"{}""#, h));
        }
        if let Some(c) = exit_code {
            json.push_str(&format!(r#","ExitCode":{}"#, c));
        }
        json.push('}');
        json
    }

    /// String 行列表转 `&str` 切片(临时 String 需先绑定再借用)。
    fn as_lines(raw: &[String]) -> Vec<&str> {
        raw.iter().map(String::as_str).collect()
    }

    #[test]
    fn test_health_verdict_all_running_pass() {
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("db", "running", Some(""), None),
            ps_line("cache", "running", Some("healthy"), Some(0)),
        ];
        assert_eq!(health_verdict(&as_lines(&raw)), HealthVerdict::Pass);
    }

    #[test]
    fn test_health_verdict_exited_fails_fast() {
        // exited 且无 ExitCode 字段(版本差异)→ 保守按失败(宁误报不漏报)
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("db", "exited", None, None),
        ];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Unhealthy {
                service: "db".to_string(),
                state: "exited".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_exited_nonzero_exit_code_unhealthy() {
        // exited 且 ExitCode≠0 → 立即失败,状态注明"非零退出"
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("db", "exited", None, Some(1)),
        ];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Unhealthy {
                service: "db".to_string(),
                state: "exited(非零退出)".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_exited_zero_exit_code_pending() {
        // exited 且 ExitCode==0(一次性服务正常退出)→ 不算失败,继续轮询,
        // pending 展示"已退出(退出码 0)"并置 exited_zero(预算耗尽时报错
        // 据此提示关闭健康检查)
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("job", "exited", None, Some(0)),
        ];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("job".to_string(), "已退出(退出码 0)".to_string())),
                exited_zero: true
            }
        );
    }

    #[test]
    fn test_health_verdict_exited_missing_exit_code_unhealthy() {
        // 仅 exited 容器且无 ExitCode 字段 → 保守按失败
        let raw = vec![ps_line("db", "exited", Some(""), None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Unhealthy {
                service: "db".to_string(),
                state: "exited".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_restarting_and_dead_fail_fast() {
        for state in ["restarting", "dead"] {
            let raw = vec![ps_line("db", state, None, None)];
            assert_eq!(
                health_verdict(&as_lines(&raw)),
                HealthVerdict::Unhealthy {
                    service: "db".to_string(),
                    state: state.to_string()
                }
            );
        }
    }

    #[test]
    fn test_health_verdict_blank_or_garbage_indeterminate() {
        // 空行 / 全空白 / 非 JSON 输出(旧版 compose、警告行等)→ 无法判定,继续轮询
        assert_eq!(
            health_verdict(&[""]),
            HealthVerdict::Indeterminate { pending: None, exited_zero: false }
        );
        assert_eq!(
            health_verdict(&["   "]),
            HealthVerdict::Indeterminate { pending: None, exited_zero: false }
        );
        assert_eq!(
            health_verdict(&[r#"time="2026-08-30" level=warning msg="x""#]),
            HealthVerdict::Indeterminate { pending: None, exited_zero: false }
        );
    }

    #[test]
    fn test_health_verdict_health_three_states() {
        // 无 Health 字段(无 healthcheck)→ Pass
        let raw = vec![ps_line("web", "running", None, None)];
        assert_eq!(health_verdict(&as_lines(&raw)), HealthVerdict::Pass);
        // Health="healthy" → Pass
        let raw = vec![ps_line("web", "running", Some("healthy"), None)];
        assert_eq!(health_verdict(&as_lines(&raw)), HealthVerdict::Pass);
        // Health="starting" → 尚未就绪,继续轮询(pending 展示 Health)
        let raw = vec![ps_line("web", "running", Some("starting"), None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("web".to_string(), "starting".to_string())),
                exited_zero: false
            }
        );
        // Health="unhealthy" → 未通过,继续轮询(预算耗尽时报错展示该状态)
        let raw = vec![ps_line("web", "running", Some("unhealthy"), None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("web".to_string(), "unhealthy".to_string())),
                exited_zero: false
            }
        );
    }

    #[test]
    fn test_health_verdict_not_running_pending() {
        // 非终态且非 running(created/paused)→ 继续轮询,带出服务与容器状态
        let raw = vec![ps_line("web", "created", None, None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("web".to_string(), "created".to_string())),
                exited_zero: false
            }
        );
    }

    #[test]
    fn test_health_verdict_json_array_format() {
        // 旧版 compose 一次性输出 JSON 数组 → 同样可解析
        let lines = vec![
            r#"[{"Service":"web","State":"running"},{"Service":"db","State":"running","Health":"healthy"}]"#,
        ];
        assert_eq!(health_verdict(&lines), HealthVerdict::Pass);
    }

    #[test]
    fn test_health_verdict_falls_back_to_name_field() {
        // 缺 Service 字段时回退容器 Name
        let lines = vec![r#"{"Name":"app-db-1","State":"exited"}"#];
        assert_eq!(
            health_verdict(&lines),
            HealthVerdict::Unhealthy {
                service: "app-db-1".to_string(),
                state: "exited".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_health_object_status() {
        // Health 为嵌套对象时取其 Status 字段
        let lines = vec![r#"{"Service":"web","State":"running","Health":{"Status":"healthy"}}"#];
        assert_eq!(health_verdict(&lines), HealthVerdict::Pass);
    }
}
