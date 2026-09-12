//! Tauri 命令层与部署管线(Task 5)。
//!
//! 所有命令统一返回 `Result<T, String>`,错误信息面向用户(中文)。
//!
//! 事件(Tauri 2 `Emitter::emit`):
//! - `deploy-progress`:`DeployProgress { step, total, message }`,step 1..5
//!   (1=打标签 2=导出压缩 3=上传镜像 4=同步文件 5=服务器部署)
//! - `deploy-log`:一行日志字符串,带 `[HH:MM:SS]` 前缀
//! - `deploy-done`:`DeployDone { success, message }`;先落地一条部署历史
//!   (`history::append_record`,成功/失败/取消统一记录;第十一批起改为先于
//!   emit,保证前端 done 后立即 get_history 可读到本次记录)再 emit,并按项目
//!   配置的 `notify_webhook` 异步发送 webhook 通知(尽力而为,失败仅告警)
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
//! → 启动(compose up -d)→ 健康检查(compose ps/logs 与 pull/up 同序
//! -f override,override-only 服务不逃逸判定)→ 部署后钩子
//! → 清理旧 releases(仅留最新 5 个)。
//!
//! 整栈部署预览(`preview_stack_changes`,Task 6 独立 dry-run 功能,不接入
//! 部署流程):建连后对比本地 compose 解析结果与远端实际状态(远端镜像 ID +
//! compose 项目现存容器),逐服务分类为 重建/新建/不变/拉取/缺失;纯只读,
//! 不落盘、不改远端状态。
//!
//! 智能传输(`skip_unchanged` / `force_archive`):打包前建连对比本地与远端
//! 同标签镜像 ID(`same_image_id` 口径),未变化的服务跳过传输或仅打包留档;
//! 整栈成功时向 release 目录写入 `manifest.json` 与 compose 副本存档。
//!
//! 部署断点续传(UPGRADE-PLAN 阶段六):两条部署管线在每个步骤完成的收尾处
//! 把进度与产物落盘到 `config/resume-deploy.json`(键 `server_id|project_id|mode`,
//! 见 [`crate::config::ResumeCheckpoint`]);部署失败或被取消时断点保留,可经
//! `deploy_resume_status` / `deploy_resume_start` / `deploy_resume_discard`
//! 查询、续传或放弃。续传按 `step_next` 跳过已完成的步骤,并对跨 attempt 的
//! 产物做幂等化复用(本地 tar 复用、SFTP 断点续传、远端镜像 inspect 跳过装载、
//! 发布目录复用);部署成功后清除断点并清理断点期保留的本地临时 tar。
//! 断点不修改正常部署的事件/历史/通知语义(`deploy-done` 恰好一次等不变)。
//!
//! 多服务器批量部署(UPGRADE-PLAN 阶段七,`deploy_batch` 命令):一次请求向
//! 多台服务器部署同一项目。编排在后台任务逐台串行执行,每台复用单发管线
//! ([`run_one_deploy`] / [`run_one_deploy_stack`],部署历史与成功/失败通知按
//! 单发语义每台照发),但批量路径不 emit `deploy-progress` / `deploy-done`
//! ——单台开始/成功/失败/跳过与最终汇总改由 `deploy-batch` 事件表达,
//! `deploy-log` 每行加 `[服务器名] ` 前缀(任务级上下文,见
//! [`DEPLOY_EVENT_CTX`]);批量与断点续传互斥(不落断点、不支持续传,
//! 单台失败整台重跑);取消后当前台由管线内取消检查中止,余台逐台跳过。
//!
//! 一键回滚(`rollback_*` 命令):整栈回滚 = 逐包 `docker load` 历史 release,
//! 恢复 compose 副本后 `compose up -d`;单镜像回滚 = `docker tag` 把目标
//! 引用指回历史标签后 `compose up -d`。复用 deploy-log / deploy-done 事件体系,
//! 成功后落一条 `mode = "rollback"` 的部署历史。

use std::any::Any;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::config::{
    checkpoint_key, load_config, load_resume_map, remove_checkpoint, save_config, save_checkpoint,
    AppConfig, AuthType, ProjectConfig, ResumeCheckpoint, ServerConfig, ServiceOverride,
    TransferMode,
};
use crate::crypto::{dpapi_protect, dpapi_unprotect};
use crate::docker::{
    check_host, image_exists, image_id_by_ref, image_size, make_deploy_tag, save_gzip,
    start_daemon, tag_image, HostCheckReport, ImageInfo,
};
use crate::history::{
    append_record, load_history, DeployRecord, MODE_MIGRATE, MODE_ROLLBACK, MODE_SINGLE,
    MODE_STACK,
};
use crate::ssh::{
    check_server_env, exec_collect, mkdir_p_cmd, ServerCheckReport, SshClient, INSTALL_DOCKER_CMD,
};
use crate::stack::{
    apply_overrides, find_override_files, parse_compose_file, split_image_ref, ComposeStack,
};

/// 取消提示文案(取消导致的失败统一用它,便于前端识别)。
pub(crate) const CANCELLED_MSG: &str = "部署已取消";
// ===== 子模块(第十二批结构治理:按文件头分节拆分;本文件保留跨域共享设施)=====
// 各子模块经 pub use 再导出,lib.rs 的 commands::xxx 路径不变。
#[path = "compose_sources.rs"]
mod compose_sources;
pub use compose_sources::*;
#[path = "host_server.rs"]
mod host_server;
pub use host_server::*;
#[path = "resume.rs"]
mod resume;
pub use resume::*;
#[path = "deploy.rs"]
mod deploy;
pub use deploy::*;
#[path = "batch.rs"]
mod batch;
pub use batch::*;
#[path = "preview.rs"]
mod preview;
pub use preview::*;
#[path = "rollback.rs"]
mod rollback;
pub use rollback::*;
#[path = "cleanup.rs"]
mod cleanup;
pub use cleanup::*;
#[path = "migrate.rs"]
mod migrate;
pub use migrate::*;
#[cfg(test)]
mod tests;

/// SSH 建连超时(秒):russh 对不可达地址可能长时间挂起且自身不带超时,统一兜底。
const SSH_CONNECT_TIMEOUT_SECS: u64 = 15;
/// SSH 检测/建目录类命令的执行超时(秒)(安装 Docker 固定 1800 秒,另行指定)。
pub(crate) const SSH_EXEC_TIMEOUT_SECS: u64 = 60;
/// 导出进度日志的汇报粒度:每 ≥5MB 变化汇报一次。
pub(crate) const LOG_PROGRESS_STEP: u64 = 5 * 1024 * 1024;

/// 整栈部署:并行打包的并发度上限(实际取 `min(本值, 可用并行度)`)。
const PACK_CONCURRENCY_CAP: usize = 3;

/// 镜像包上传失败后的重试等待(秒):给网络/服务端一点恢复时间,再同路径续传重试。
const UPLOAD_RETRY_DELAY_SECS: u64 = 2;
/// 整栈部署:单包 `docker load` 的执行超时(秒)。
pub(crate) const STACK_LOAD_TIMEOUT_SECS: u64 = 600;
/// 整栈部署:`docker compose pull` / `up -d` 的执行超时(秒)。
pub(crate) const STACK_COMPOSE_TIMEOUT_SECS: u64 = 900;

/// 服务器清理(`prune_server`)的执行超时(秒)。
const PRUNE_TIMEOUT_SECS: u64 = 300;

/// 分项目扫描的目录深度上限(起点之下);目录更深时把扫描起点指到项目父目录。
const CLEANUP_SCAN_MAX_DEPTH: usize = 4;

/// 部署前/后钩子命令的执行超时(秒)。
const HOOK_TIMEOUT_SECS: u64 = 600;
/// 健康检查:轮询间隔(秒)。
pub(crate) const HEALTH_POLL_INTERVAL_SECS: u64 = 5;
/// 健康检查:单轮 `compose ps` 状态查询的执行超时(秒)。
pub(crate) const HEALTH_PS_TIMEOUT_SECS: u64 = 60;

/// 整栈拉取失败时并入错误信息的远端输出末尾行数
/// (供 [`augment_pull_error`] 依据输出识别私有仓库认证问题)。
const PULL_OUTPUT_TAIL_LINES: usize = 10;

/// 部署完成 webhook 通知的 HTTP 超时(秒)。
const WEBHOOK_TIMEOUT_SECS: u64 = 10;

/// 整栈部署预览:远端查询镜像列表的命令(JSON 输出,每行一条;ID 为 12 位
/// 截断口径,与预览本地侧 `list_images` 的 ID 口径一致,**不要单独加
/// `--no-trunc`**,否则破坏预览两侧同口径对比)。
const REMOTE_IMAGES_CMD: &str = "docker images --format '{{json .}}'";

/// 智能传输跳过判定:远端查询镜像列表的命令(`--no-trunc` 输出完整 64 位 ID,
/// 含 `sha256:` 前缀)。
///
/// 仅用于部署跳过判定的数据源 [`query_remote_image_id_map`]:本地侧
/// [`crate::docker::image_id_by_ref`] 返回完整 64 位 ID,远端必须同为完整口径,
/// [`same_image_id`] 才能正确判定相等(12 位截断 ID 与完整 ID 永不相等,
/// 会导致跳过逻辑永不触发)。
const REMOTE_IMAGES_CMD_FULL: &str = "docker images --no-trunc --format '{{json .}}'";

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
    /// 智能传输:本地与远端同标签镜像 ID 一致时跳过导出/上传/装载
    /// (仅 `use_date_tag = false` 时生效,日期标签是全新 tag 必然有变化;
    /// 缺省 = false,向后兼容)
    pub skip_unchanged: Option<bool>,
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
    /// 智能传输:未变化(远端同标签且镜像 ID 一致)的服务跳过打包/上传/装载
    /// (缺省 = false,向后兼容)
    pub skip_unchanged: Option<bool>,
    /// 智能传输的强制留档:未变化的服务仍打包上传进 release 目录(供回滚
    /// `docker load`),仅跳过装载步骤(缺省 = false,向后兼容)
    pub force_archive: Option<bool>,
}

/// 整栈部署中单个 compose 服务的传输分类。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackServiceChoice {
    /// compose 服务名(compose pull / up 按此名定位)
    pub service: String,
    /// 传输方式为本地传输(Local)时必须非空的镜像引用;服务器拉取(Pull)时可为空
    pub image: String,
    pub mode: TransferMode,
}

// ===== 多服务器批量部署(UPGRADE-PLAN 阶段七)=====

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

/// 后台部署任务的统一启动器(单发/续传路径):在后台任务里执行
/// [`run_one_deploy`] / [`run_one_deploy_stack`] 的 future。收尾语义
/// (panic 兜底、`deploy-done` 恰好 emit 一次、部署历史、webhook/通知中心)
/// 由内层经 [`finish_deploy_run`] 保证,这里只负责 spawn 与丢弃返回值。
fn spawn_deploy_task<F>(_app: AppHandle, fut: F)
where
    F: std::future::Future<Output = Result<DeployRecord, String>> + Send + 'static,
{
    // future 已持有自己的 AppHandle 克隆(见 run_one_* 的 finish_deploy_run 入参)
    tauri::async_runtime::spawn(async move {
        let _ = fut.await;
    });
}

/// 单次部署的事件表达选项(单发全量 / 批量收敛),由 [`run_one_deploy`] /
/// [`run_one_deploy_stack`] 施加到任务级上下文([`DEPLOY_EVENT_CTX`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeployEmitOpts {
    /// 管线内是否 emit `deploy-progress`(批量关闭:进度由 `deploy-batch` 表达)
    pub(crate) emit_progress: bool,
    /// 收尾是否 emit `deploy-done`(批量关闭:结果由 `deploy-batch` 表达)
    pub(crate) emit_done: bool,
    /// `deploy-log` 每行追加的前缀(单发 = 空串;批量 = `[服务器名] `)
    pub(crate) log_prefix: String,
    /// 是否落盘部署断点(批量关闭:批量与断点续传互斥,失败整台重跑)
    pub(crate) checkpoint: bool,
}

impl DeployEmitOpts {
    /// 单发/续传路径:事件与断点行为与历史版本完全一致。
    fn single() -> Self {
        Self {
            emit_progress: true,
            emit_done: true,
            log_prefix: String::new(),
            checkpoint: true,
        }
    }

    /// 批量路径:收敛事件 + 关断点;日志加 `[服务器名] ` 前缀。
    fn batch(server_name: &str) -> Self {
        Self {
            emit_progress: false,
            emit_done: false,
            log_prefix: format!("[{}] ", server_name),
            checkpoint: false,
        }
    }
}

// 部署事件的任务级上下文(见 [`DeployEventCtx`]):批量部署在单台管线外包裹
// scope,使管线内所有 emit_log / emit_progress 调用自动获得前缀/抑制语义,
// 无需改动管线内部的逐处 emit 调用;单发/续传路径无 scope,取缺省值
// = 历史行为不变。(task_local! 宏调用本身不支持外挂 rustdoc,故用普通注释。)
tokio::task_local! {
    static DEPLOY_EVENT_CTX: DeployEventCtx;
}

/// 任务级部署事件上下文(见 [`DEPLOY_EVENT_CTX`];由 [`DeployEmitOpts`] 投影)。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeployEventCtx {
    log_prefix: String,
    emit_progress: bool,
}

impl DeployEventCtx {
    fn of(opts: &DeployEmitOpts) -> Self {
        Self {
            log_prefix: opts.log_prefix.clone(),
            emit_progress: opts.emit_progress,
        }
    }

    /// 当前任务的 `deploy-log` 前缀(无 scope = 单发路径,空串)。
    fn log_prefix() -> String {
        DEPLOY_EVENT_CTX
            .try_with(|c| c.log_prefix.clone())
            .unwrap_or_default()
    }

    /// 当前任务是否允许 emit `deploy-progress`(无 scope = 单发路径,允许)。
    fn progress_enabled() -> bool {
        DEPLOY_EVENT_CTX
            .try_with(|c| c.emit_progress)
            .unwrap_or(true)
    }
}

/// 管线执行 + 统一收尾([`spawn_deploy_task`] 与批量单台路径共用):
/// panic 兜底([`CatchPanic`])+ 收尾事件 + 部署历史 + webhook 通知。
///
/// 正常结束路径(成功/失败/取消)在通知分发之前按需 emit `deploy-done`
/// (`emit_done = false` 的批量路径不 emit,结果由 `deploy-batch` 表达),
/// emit 前先落地部署历史记录(由管线组装的 [`DeployRecord`],append 失败仅告警,
/// 不影响收尾;先落盘保证前端 done 后立即查历史可见,第十一批调整),并按项目
/// 配置的 `notify_webhook` 异步发送 webhook 通知
/// (尽力而为,失败仅告警);同样调用 [`crate::notify::fire`] 分发通知中心
/// 通知(桌面/邮件,按 AppConfig.notify 的事件订阅与渠道开关,失败仅告警)。
///
/// 返回 `Ok(记录)` = 成功;`Err(错误文案)` = 失败/取消/panic
/// (panic 路径不写历史、不发 webhook,记录随管线丢失,与旧版一致)。
async fn finish_deploy_run<F>(app: AppHandle, fut: F, emit_done: bool) -> Result<DeployRecord, String>
where
    F: std::future::Future<Output = (Result<(), String>, DeployRecord, Option<String>)>,
{
    let (result, record, webhook_url) = match CatchPanic::new(fut).await {
        Ok(triple) => (triple.0, Some(triple.1), triple.2),
        Err(panic_info) => {
            log::error!("部署管线发生 panic: {}", panic_info);
            // 管线内组装的部署记录随 panic 丢失:此路径不写历史、不发 webhook,
            // 统一错误文案走下方失败分支发送 failure 通知(正文为 record
            // 缺失时的兜底文案)
            (
                Err("部署过程发生内部错误,详情见日志".to_string()),
                None,
                None,
            )
        }
    };
    // 先落地部署历史再 emit deploy-done(第十一批):前端在 done 后立即查历史
    // (部署时预填的版本说明要读最新成功记录的 release_dir),先落盘消除竞态;
    // webhook/通知仍随后分发,收尾语义不变
    if let Some(record) = &record {
        // webhook 通知:项目配置了 notify_webhook 才发;阻塞 HTTP 放 blocking
        // 线程池 fire-and-forget,失败仅告警,不影响部署收尾
        if let Some(url) = webhook_url.filter(|u| !u.trim().is_empty()) {
            let payload = webhook_payload(record);
            tauri::async_runtime::spawn_blocking(move || send_webhook(&url, &payload));
        }
        append_record(record.clone());
    }
    match &result {
        Ok(()) => {
            if emit_done {
                let _ = app.emit(
                    "deploy-done",
                    DeployDone {
                        success: true,
                        message: "部署完成".to_string(),
                    },
                );
            }
            // 通知中心:部署成功(按 notify 配置的事件订阅与渠道开关异步分发)
            let (title, body) = deploy_notify_text(true, "部署完成", &record);
            crate::notify::fire(app.clone(), "success", title, body).await;
        }
        Err(e) => {
            emit_log(&app, &format!("部署失败: {}", e));
            // 取消导致的失败(固定文案 CANCELLED_MSG)按 cancel 事件分发
            let kind = if e.as_str() == CANCELLED_MSG { "cancel" } else { "failure" };
            let (title, body) = deploy_notify_text(false, e, &record);
            if emit_done {
                let _ = app.emit("deploy-done", DeployDone { success: false, message: e.clone() });
            }
            // 通知中心:部署失败/取消(emit deploy-done 之后异步分发,不阻塞收尾)
            crate::notify::fire(app.clone(), kind, title, body).await;
        }
    }
    match (result, record) {
        (Ok(()), Some(record)) => Ok(record),
        (Err(e), _) => Err(e),
        // panic 路径 record 必为 None 且 result 必为 Err,此分支不可达(防御性兜底)
        (Ok(()), None) => Err("部署过程发生内部错误,详情见日志".to_string()),
    }
}

/// 组装部署收尾通知的标题与正文(纯函数,便于单测)。
///
/// 标题:成功=「部署成功」;取消(错误文案为 CANCELLED_MSG)=「部署已取消」;
/// 其余失败=「部署失败」。正文含项目名 + 服务器名 + 结果消息 + 耗时;
/// `record` 为 `None`(管线 panic,记录丢失)时用兜底文案。
fn deploy_notify_text(
    success: bool,
    message: &str,
    record: &Option<DeployRecord>,
) -> (String, String) {
    let title = if success {
        "部署成功".to_string()
    } else if message == CANCELLED_MSG {
        "部署已取消".to_string()
    } else {
        "部署失败".to_string()
    };
    let body = match record {
        Some(r) => format!(
            "项目「{}」@ 服务器「{}」:{}(耗时 {} 秒)",
            r.project_name, r.server_name, message, r.duration_secs
        ),
        None => format!("{}(部署详情缺失,详见应用日志)", message),
    };
    (title, body)
}

/// Future 的 panic 兜底包装:被包裹 future 在 poll 中 panic 时返回 `Err(panic 信息)`,
/// 而不是让整个后台任务静默消失(配合 [`deploy`] 保证 `deploy-done` 恰好 emit 一次)。
pub(crate) struct CatchPanic<F: std::future::Future>(Pin<Box<F>>);

impl<F: std::future::Future> CatchPanic<F> {
    pub(crate) fn new(fut: F) -> Self {
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

// ===== 部署 webhook 通知(Task 6)=====

/// 部署完成 webhook 通知的 JSON 载荷(纯函数,便于单测)。
/// 字段:`event`/`success`/`message`/`server`/`project`/`duration_secs`/`ts`。
fn webhook_payload(record: &DeployRecord) -> String {
    serde_json::json!({
        "event": "deploy",
        "success": record.success,
        "message": record.message,
        "server": record.server_name,
        "project": record.project_name,
        "duration_secs": record.duration_secs,
        "ts": record.ts,
    })
    .to_string()
}

/// 发送 webhook 通知(阻塞 HTTP,调用方须放 blocking 线程池)。
/// 尽力而为:成功 `log::info`,失败(网络/超时/非 2xx)仅 `log::warn`,
/// 不向调用方传播错误、不影响部署结果。
/// 日志脱敏:webhook URL 常内嵌 token(query/userinfo,如飞书/钉钉机器人
/// 地址),完整落 `logs/app.log` 会泄漏凭据 —— 成功/失败日志均只记主机名
/// (见 [`url_host_for_log`]);ureq 的错误文本同样内嵌完整 URL,不直接落其
/// Display,只取状态码/错误类别与描述(见 [`webhook_error_detail`])。
fn send_webhook(url: &str, payload: &str) {
    let host = url_host_for_log(url);
    let result = ureq::post(url)
        .timeout(Duration::from_secs(WEBHOOK_TIMEOUT_SECS))
        .set("Content-Type", "application/json")
        .send_string(payload);
    match result {
        Ok(_) => log::info!("部署 webhook 通知已发送:{}", host),
        Err(e) => log::warn!(
            "部署 webhook 通知发送失败 ({}): {}",
            host,
            webhook_error_detail(&e)
        ),
    }
}

/// URL 日志脱敏(纯函数):只取 `scheme://` 之后的 `host[:port]` 部分
/// (`user:pass@host` 的 userinfo 凭据段一并剥除),不落 path/query;
/// 解析不出 host(非 URL / 空 authority)返回 `"<unparseable-url>"`。
fn url_host_for_log(url: &str) -> String {
    let Some((_, authority_full)) = url.split_once("://") else {
        return "<unparseable-url>".to_string();
    };
    let authority = authority_full.split(['/', '?', '#']).next().unwrap_or("");
    // user:pass@host 形态取 @ 之后的 host;无 @ 即整段
    let host = match authority.rsplit_once('@') {
        Some((_, h)) => h,
        None => authority,
    };
    if host.is_empty() {
        "<unparseable-url>".to_string()
    } else {
        host.to_string()
    }
}

/// ureq 错误的脱敏文本(纯函数):ureq 的错误 Display 会内嵌完整 URL
/// (`Status` 带响应 URL、`Transport` 带请求 URL),不能直接落日志;
/// 这里只取状态码 / 错误类别与描述,不含 URL。
fn webhook_error_detail(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("HTTP 状态码 {}", code),
        ureq::Error::Transport(t) => match t.message() {
            Some(m) => format!("{}: {}", t.kind(), m),
            None => format!("{}", t.kind()),
        },
    }
}

/// 读取项目配置的 webhook 通知地址(`notify_webhook`;读取配置失败或项目
/// 不存在 → `None`,此时不发通知)。
fn project_webhook_url(project_id: &str) -> Option<String> {
    let cfg = load_config().ok()?;
    find_project(&cfg, project_id)
        .ok()
        .and_then(|p| p.notify_webhook.clone())
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

/// 拼装 `test -d '<path>'`(远端目录存在性检查,退出码 0 = 存在)。
pub fn test_dir_cmd(path: &str) -> String {
    format!("test -d {}", shell_single_quote(path))
}

/// 拼装 `test -f '<path>'`(远端文件存在性检查,退出码 0 = 存在)。
pub fn test_file_cmd(path: &str) -> String {
    format!("test -f {}", shell_single_quote(path))
}

/// 拼装 `ls -1 '<path>'`(逐行列出远端目录内容)。
pub fn ls_dir_cmd(path: &str) -> String {
    format!("ls -1 {}", shell_single_quote(path))
}

/// 拼装 `cat '<path>'`。
pub fn cat_file_cmd(path: &str) -> String {
    format!("cat {}", shell_single_quote(path))
}

/// 拼装 `docker image inspect '<ref>'`(退出码 0 = 服务器上存在该镜像引用)。
///
/// **只用于存在性判定**。要取镜像 ID 请用 [`docker_inspect_id_cmd`]:本命令不带
/// `--format`,输出是 pretty-print 的 JSON 数组(首字符 `[`),按 ID 解析必然失败。
pub fn docker_inspect_cmd(image: &str) -> String {
    format!("docker image inspect {}", shell_single_quote(image))
}

/// 拼装 `docker image inspect --format '{{.Id}}' '<ref>'`(输出为完整 64 位
/// `sha256:...` ID 单行;引用不存在时退出码非 0)。
///
/// 与 `docker::image_id_by_ref_blocking`(本地口径)、`REMOTE_IMAGES_CMD_FULL`
/// (`--no-trunc` 完整 ID 口径)同为准,供 `same_image_id` 跨端比较。
pub fn docker_inspect_id_cmd(image: &str) -> String {
    format!(
        "docker image inspect --format '{{{{.Id}}}}' {}",
        shell_single_quote(image)
    )
}

/// 从 [`docker_inspect_id_cmd`] 的输出解析镜像 ID(纯函数,便于单测):
/// trim 后剥 `sha256:` 前缀;空输出或落到 JSON 形态(误用无 `--format` 的命令)
/// → `None`,避免把垃圾当 ID 参与跨端比较。
pub fn parse_inspect_id(out: &str) -> Option<String> {
    let v = out.trim();
    if v.is_empty() || v.starts_with('[') || v.starts_with('{') {
        return None;
    }
    Some(v.strip_prefix("sha256:").unwrap_or(v).to_string())
}

/// 逐行解析 `ls -1` 输出为条目列表(trim + 去空行;纯函数,便于单测)。
pub fn parse_ls_lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

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
/// `overrides` 按检测顺序逐个追加 `-f`(与 pull/up 同序合并),否则只存在于
/// override 中的服务不会出现在 ps 输出、完全逃逸健康判定。
pub fn compose_ps_json_cmd(remote_dir: &str, compose_file: &str, overrides: &[String]) -> String {
    format!(
        "cd {} && docker compose {} ps --all --format json",
        shell_single_quote(remote_dir),
        compose_file_flags(compose_file, overrides)
    )
}

/// 拼装查看 compose 各服务最近日志的命令(最后 50 行;overrides 同 ps 口径)。
pub fn compose_logs_cmd(remote_dir: &str, compose_file: &str, overrides: &[String]) -> String {
    format!(
        "cd {} && docker compose {} logs --tail 50",
        shell_single_quote(remote_dir),
        compose_file_flags(compose_file, overrides)
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
///
/// 批量部署路径(任务级上下文关闭,见 [`DEPLOY_EVENT_CTX`])不 emit ——
/// 批量的单台进度由 `deploy-batch` 事件表达,避免多台交叉刷屏;
/// 单发/续传路径无上下文,行为不变。
fn emit_progress(app: &AppHandle, step: u8, total: u8, message: &str) {
    if !DeployEventCtx::progress_enabled() {
        return;
    }
    let _ = app.emit(
        "deploy-progress",
        DeployProgress {
            step,
            total,
            message: message.to_string(),
        },
    );
}

/// 组装 `deploy-log` 的整行文本(纯函数,便于单测):
/// `[HH:MM:SS] <前缀><消息>`(尾随换行剔除;前缀紧贴消息,如
/// `[12:00:00] [生产] 加载镜像到服务器: …`)。
pub(crate) fn format_log_line(prefix: &str, msg: &str) -> String {
    format!(
        "[{}] {}{}",
        chrono::Local::now().format("%H:%M:%S"),
        prefix,
        msg.trim_end()
    )
}

/// emit `deploy-log` 事件:一行日志,带 `[HH:MM:SS]` 前缀(尾随换行剔除)。
///
/// 批量部署路径(任务级上下文,见 [`DEPLOY_EVENT_CTX`])额外追加
/// `[服务器名] ` 前缀区分来源服务器;单发/续传路径无上下文(前缀为空),行为不变。
fn emit_log(app: &AppHandle, msg: &str) {
    let _ = app.emit("deploy-log", format_log_line(&DeployEventCtx::log_prefix(), msg));
}

/// 本地临时 tar 的 Drop 守卫:作用域结束(成功或失败)时删除文件。
///
/// [`TempFileGuard::keep`](断点续传活跃时使用)不删除 —— 临时 tar 供失败后
/// 续传复用,由成功收尾([`checkpoint_cleanup_on_success`])或
/// `deploy_resume_discard` 显式清理。
pub(crate) struct TempFileGuard {
    path: PathBuf,
    /// true = 断点活跃,Drop 不删除(保留给续传)
    keep: bool,
}

impl TempFileGuard {
    /// 用完即删的守卫(默认;旧行为)。
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    /// 断点活跃时使用的守卫:Drop 不删除。
    fn keep(path: PathBuf) -> Self {
        Self { path, keep: true }
    }

    /// 公开构造(供 [`crate::migrate_project`] 使用;语义同 [`Self::new`])。
    pub(crate) fn new_pub(path: PathBuf) -> Self {
        Self::new(path)
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("删除本地临时文件失败 ({}): {}", self.path.display(), e);
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
pub(crate) fn find_project<'a>(cfg: &'a AppConfig, project_id: &str) -> Result<&'a ProjectConfig, String> {
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

/// 解析 SSH 认证所需的明文私钥口令(纯函数,便于测试;阶段三「加密私钥口令」)。
///
/// `AuthConfig.key_pass_enc` 有值 → DPAPI 解密返回 `Some(明文)`;
/// 未配置(旧版配置 / 未加密私钥)→ `Ok(None)`,由 ssh 层按无口令私钥加载
/// (加载加密私钥时会得到「私钥已加密,请输入私钥口令」的明确报错)。
pub(crate) fn resolve_key_passphrase(cfg: &ServerConfig) -> Result<Option<String>, String> {
    match cfg.auth.key_pass_enc.as_deref().filter(|e| !e.is_empty()) {
        Some(enc) => Ok(Some(dpapi_unprotect(enc)?)),
        None => Ok(None),
    }
}

/// 保存「本次测试连接」输入的私钥口令(test_server 的 remember_key_pass)。
///
/// 口令为空/空白 → 直接返回(等价于不保存);否则重新 load → 改
/// `AuthConfig.key_pass_enc`(DPAPI 加密)→ save,避免覆盖内存之外的并发修改。
fn remember_key_passphrase(server_id: &str, key_passphrase: Option<String>) -> Result<(), String> {
    let Some(pass) = key_passphrase.filter(|p| !p.trim().is_empty()) else {
        return Ok(());
    };
    let mut cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = cfg
        .servers
        .iter_mut()
        .find(|s| s.id == server_id)
        .ok_or_else(|| format!("未找到 ID 为「{}」的服务器配置", server_id))?;
    server.auth.key_pass_enc = Some(dpapi_protect(&pass)?);
    save_config(&cfg).map_err(|e| format!("保存私钥口令失败: {}", e))
}

/// 主机密钥 TOFU 首次落盘(commands.rs 与 manage.rs 的 connect_server 共用)。
///
/// 连接成功且本次观察到服务器指纹、而该服务器配置**尚未**记录指纹
/// (`host_key_sha256` 为 None)时,按 load → 改 → save 把指纹写入
/// `ServerConfig.host_key_sha256`。已有记录(或观察不到指纹)→ 不动配置。
/// 落盘失败仅告警不报错:连接已成功,持久化失败不应让本次操作整体失败。
pub(crate) fn persist_host_key_if_needed(server: &ServerConfig, observed: &Option<String>) {
    let Some(fingerprint) = observed.as_deref() else {
        return;
    };
    if server.host_key_sha256.is_some() {
        return; // 已有信任记录(TOFU 已完成;变更拒绝由 ssh 层负责)
    }
    let mut cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("保存主机密钥指纹前读取配置失败: {}", e);
            return;
        }
    };
    match cfg.servers.iter_mut().find(|s| s.id == server.id) {
        Some(s) if s.host_key_sha256.is_none() => {
            s.host_key_sha256 = Some(fingerprint.to_string());
            if let Err(e) = save_config(&cfg) {
                log::warn!("保存主机密钥指纹失败 (服务器 {}): {}", server.name, e);
            } else {
                log::info!("已记录服务器「{}」的主机密钥指纹 (TOFU)", server.name);
            }
        }
        _ => {}
    }
}

/// 重新信任服务器主机密钥(TOFU 重置):清空 `host_key_sha256`,
/// 下次连接将重新接受并记录当前指纹(服务器重装/换 IP 后由用户显式调用)。
#[tauri::command]
pub fn retrust_host_key(server_id: String) -> Result<(), String> {
    let mut cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = cfg
        .servers
        .iter_mut()
        .find(|s| s.id == server_id)
        .ok_or_else(|| format!("未找到 ID 为「{}」的服务器配置", server_id))?;
    server.host_key_sha256 = None;
    save_config(&cfg).map_err(|e| format!("保存配置失败: {}", e))
}

/// 该项目实际使用的远程部署目录(第四批,纯函数便于单测)。
///
/// 优先级:项目级 `ProjectConfig.remote_dir`(非空)→ 服务器 `ServerConfig.remote_dir`。
/// 项目未配置时完全沿用旧行为(服务器级目录),因此旧配置的部署路径不变。
///
/// 背景:`ServerConfig.remote_dir` 是服务器级单一目录,同服务器多项目会共用
/// 同一部署目录与 `docker-compose.yml` —— 项目级目录让各项目互不干扰。
pub fn effective_remote_dir(server: &ServerConfig, project: &ProjectConfig) -> String {
    project
        .remote_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| server.remote_dir.clone())
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
pub(crate) fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ===== 清理分析(阶段八:prune 预览 + 定向执行;与既有 prune_server 同通道)=====

/// 取字符串末尾 `n` 行(用于错误信息里附远端输出尾部)。
pub fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.trim().lines().collect();
    if lines.is_empty() {
        return "无输出".to_string();
    }
    let start = lines.len().saturating_sub(n);
    lines[start..].join(" / ")
}

/// 逐行转发远端输出到指定事件(供迁移等非 deploy-log 场景复用)。
pub(crate) async fn exec_forwarded_via_event(
    client: &mut SshClient,
    cmd: &str,
    emit_line: &Arc<dyn Fn(&str) + Send + Sync>,
    what: &str,
) -> Result<(), String> {
    let mut buf = String::new();
    let mut on_line = |line: &str| {
        emit_line(line.trim_end());
        buf.push_str(line);
    };
    let code = with_timeout(
        STACK_LOAD_TIMEOUT_SECS,
        &format!("{}超时", what),
        "请检查服务器网络后重试",
        async {
            client
                .exec(cmd, &mut on_line)
                .await
                .map_err(|e| format!("执行 {} 失败: {}", what, e))
        },
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "{}失败(退出码 {}): {}",
            what,
            code,
            buf.trim().lines().last().unwrap_or("无输出")
        ));
    }
    Ok(())
}

/// 按 ID 查找服务器配置(公开包装,供 [`crate::migrate_project`] 复用)。
pub(crate) fn find_server_pub<'a>(
    cfg: &'a AppConfig,
    server_id: &str,
) -> Result<&'a ServerConfig, String> {
    find_server(cfg, server_id)
}

