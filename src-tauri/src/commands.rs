//! Tauri 命令层与部署管线(Task 5)。
//!
//! 所有命令统一返回 `Result<T, String>`,错误信息面向用户(中文)。
//!
//! 事件(Tauri 2 `Emitter::emit`):
//! - `deploy-progress`:`DeployProgress { step, total, message }`,step 1..5
//!   (1=打标签 2=导出压缩 3=上传镜像 4=同步文件 5=服务器部署)
//! - `deploy-log`:一行日志字符串,带 `[HH:MM:SS]` 前缀
//! - `deploy-done`:`DeployDone { success, message }`;emit 后按结果落一条
//!   部署历史(`history::append_record`,成功/失败/取消统一记录),并按项目
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
    append_record, load_history, DeployRecord, MODE_ROLLBACK, MODE_SINGLE, MODE_STACK,
};
use crate::ssh::{
    check_server_env, exec_collect, mkdir_p_cmd, ServerCheckReport, SshClient, INSTALL_DOCKER_CMD,
};
use crate::stack::{
    apply_overrides, find_override_files, parse_compose_file, split_image_ref, ComposeStack,
};

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
/// 分项目扫描的目录深度上限(起点之下);目录更深时把扫描起点指到项目父目录。
const CLEANUP_SCAN_MAX_DEPTH: usize = 4;
/// 部署前/后钩子命令的执行超时(秒)。
const HOOK_TIMEOUT_SECS: u64 = 600;
/// 健康检查:轮询间隔(秒)。
const HEALTH_POLL_INTERVAL_SECS: u64 = 5;
/// 健康检查:单轮 `compose ps` 状态查询的执行超时(秒)。
const HEALTH_PS_TIMEOUT_SECS: u64 = 60;
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

/// `deploy_batch` 命令的请求参数(serde camelCase)。
///
/// 字段按 `mode` 取用:模式无关字段(`mode`/`project_id`/`server_ids`/
/// `password_plain`)必填,single 专用(`image`/`repository`/`use_date_tag`)与
/// stack 专用(`services`/`force_archive`)为可选(仅对应模式校验其存在性)。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchDeployRequest {
    /// 部署模式:`"single"`(单镜像)/ `"stack"`(整栈)
    pub mode: String,
    pub project_id: String,
    /// 目标服务器 ID 列表(批量入口去重;逐台串行执行)
    pub server_ids: Vec<String>,
    /// single 模式:本地完整镜像引用(如 `myapp:latest`)
    pub image: Option<String>,
    /// single 模式:部署仓库名(生成日期标签时的前缀)
    pub repository: Option<String>,
    /// single 模式:生成 `repository:YYYYmmdd-HHMMSS` 日期标签(缺省 false)
    pub use_date_tag: Option<bool>,
    /// stack 模式:前端确认后的服务传输分类列表(所有目标服务器共用)
    pub services: Option<Vec<StackServiceChoice>>,
    /// 智能传输:本地与远端同标签镜像 ID 一致时跳过传输(single/stack 通用,
    /// 缺省 false)
    pub skip_unchanged: Option<bool>,
    /// stack 模式:智能传输强制留档(缺省 false)
    pub force_archive: Option<bool>,
    /// 前端临时输入的 SSH 密码(密码认证时优先于已保存的密文;所有目标共用)
    pub password_plain: Option<String>,
}

/// `deploy-batch` 事件负载(camelCase):批量部署逐台推送单台状态;全部结束后
/// 追加一条 `state = "batch-done"` 的汇总条目(`server_id`/`server_name` 为
/// 空串,message 含「N 成功 / M 失败 / K 跳过」)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchDeployEvent {
    /// 本条事件对应的服务器 ID(汇总条目为空串)
    pub server_id: String,
    /// 本条事件对应的服务器名(找不到配置时以 ID 兜底;汇总条目为空串)
    pub server_name: String,
    /// 单台状态:`running`(开始)/`success`/`failed`/`skipped`(已取消);
    /// 汇总条目为 `batch-done`
    pub state: String,
    /// 说明文案(开始提示/失败错误/跳过原因/成功提示/汇总)
    pub message: String,
}

/// `deploy-batch` 单台状态:开始执行。
const BATCH_STATE_RUNNING: &str = "running";
/// `deploy-batch` 单台状态:部署成功。
const BATCH_STATE_SUCCESS: &str = "success";
/// `deploy-batch` 单台状态:部署失败(含被取消的当前台,message 为「部署已取消」)。
const BATCH_STATE_FAILED: &str = "failed";
/// `deploy-batch` 单台状态:用户取消后余台跳过(不执行管线)。
const BATCH_STATE_SKIPPED: &str = "skipped";
/// `deploy-batch` 汇总条目状态:全部台已结束(或编排层 panic 兜底中止)。
const BATCH_STATE_BATCH_DONE: &str = "batch-done";

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
    /// 源文件路径(手工项目为空)
    pub source_path: String,
    /// `unchanged` / `changed` / `missing` / `unknown`
    /// (unknown = 旧配置无哈希或源不可读,不参与自动更新)
    pub state: String,
    /// 面向用户的说明
    pub detail: String,
}

/// 检查所有项目的源 compose 是否已变更(只读,不改配置)。
///
/// 手工项目(无 `source_compose_path`)与旧配置(无 `source_hash`)一律
/// 报 `unknown`,避免"凭空认为要更新"。
#[tauri::command]
pub fn check_project_sources() -> Result<Vec<ProjectSourceStatus>, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    Ok(cfg.projects.iter().map(project_source_status).collect())
}

/// 计算单个项目的源状态(纯读)。
fn project_source_status(p: &ProjectConfig) -> ProjectSourceStatus {
    let base = |state: &str, detail: String| ProjectSourceStatus {
        project_id: p.id.clone(),
        project_name: p.name.clone(),
        source_path: p.source_compose_path.clone().unwrap_or_default(),
        state: state.to_string(),
        detail,
    };
    let Some(path_str) = p.source_compose_path.as_deref().filter(|s| !s.trim().is_empty()) else {
        return base("unknown", "手工项目(无导入源),不参与源更新".to_string());
    };
    let path = PathBuf::from(path_str);
    if !path.is_file() {
        return base("missing", format!("源文件已不存在:{}", path_str));
    }
    let Some(saved) = p.source_hash.as_deref().filter(|s| !s.trim().is_empty()) else {
        return base(
            "unknown",
            "旧配置未记录源哈希,重新导入或手动更新一次即可启用比对".to_string(),
        );
    };
    match source_content_hash(&path) {
        Ok(now) if now == saved => base("unchanged", "源未变更".to_string()),
        Ok(_) => base("changed", "源 compose 已变更,可更新".to_string()),
        Err(e) => base("unknown", format!("源不可读:{}", e)),
    }
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

    let new_hash = source_content_hash(&source).ok();
    let p = &mut cfg.projects[idx];
    p.compose_file = new_dest.to_string_lossy().to_string();
    p.service_overrides = merged;
    p.source_hash = new_hash;
    let updated = p.clone();
    save_config(&cfg).map_err(|e| format!("保存配置失败: {}", e))?;
    Ok(project_source_status(&updated))
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
///
/// `key_passphrase` 为本次测试一次性输入的私钥口令(优先于已存储的
/// `key_pass_enc`);`remember_key_pass` 为 true 且口令非空时,连接成功后
/// DPAPI 加密存入 `AuthConfig.key_pass_enc`(下次免输入)。
#[tauri::command]
pub async fn test_server(
    server_id: String,
    password_plain: Option<String>,
    key_passphrase: Option<String>,
    remember_key_pass: Option<bool>,
) -> Result<ServerCheckReport, String> {
    let report = connect_and_check(&server_id, password_plain, key_passphrase.clone()).await?;
    // 连接成功后才保存口令:口令错误时建连必失败,保存坏口令没有意义
    if remember_key_pass.unwrap_or(false) {
        remember_key_passphrase(&server_id, key_passphrase)?;
    }
    Ok(report)
}

/// 与 `test_server` 等价的环境检查(独立命令名,语义 = 部署前环境自检)。
#[tauri::command]
pub async fn server_env_check(
    server_id: String,
    password_plain: Option<String>,
    key_passphrase: Option<String>,
) -> Result<ServerCheckReport, String> {
    connect_and_check(&server_id, password_plain, key_passphrase).await
}

/// 连接 + 远端环境检查的公共实现。
async fn connect_and_check(
    server_id: &str,
    password_plain: Option<String>,
    key_passphrase: Option<String>,
) -> Result<ServerCheckReport, String> {
    let (server, mut client) = connect_server(
        server_id,
        password_plain.as_deref(),
        key_passphrase.as_deref(),
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

/// 按 server_id 解析服务器配置、密码与私钥口令并建立 SSH 连接
/// (带连接超时兜底)。`connect_and_check` 与 [`preview_stack_changes`] 共用的
/// 建连路径,返回 `(服务器配置, 已连接的客户端)`。
///
/// - `password_plain`:前端临时输入的 SSH 密码(密码认证时优先于已保存密文);
/// - `key_passphrase_plain`:一次性输入的私钥口令(优先于已存储的
///   `key_pass_enc`;无输入时用存储值,均为 `None` 则按无口令私钥加载)。
/// - 主机密钥 TOFU:连接成功后首次观察到指纹时落盘到 `ServerConfig`
///   (见 [`persist_host_key_if_needed`])。
async fn connect_server(
    server_id: &str,
    password_plain: Option<&str>,
    key_passphrase_plain: Option<&str>,
) -> Result<(ServerConfig, SshClient), String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain,
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = match key_passphrase_plain.filter(|p| !p.is_empty()) {
        // 一次性输入的口令优先于已存储密文(空串视为未输入)
        Some(p) => Some(p.to_string()),
        None => resolve_key_passphrase(&server)?,
    };
    // observed:记录本次连接观察到的主机指纹,首次连接后 TOFU 落盘
    let observed = Arc::new(OnceLock::new());
    let client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::clone(&observed)),
    )
    .await?;
    persist_host_key_if_needed(&server, &observed.get().cloned());
    Ok((server, client))
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
    let key_pass = resolve_key_passphrase(&server)?;
    let mut client = SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default())
        .await?;

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
    let key_pass = resolve_key_passphrase(&server)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
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
    let key_pass = resolve_key_passphrase(&server)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
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

// ===== 部署断点续传(UPGRADE-PLAN 阶段六)=====

/// 单镜像部署断点产物(serde camelCase,存入 [`ResumeCheckpoint`] 的
/// `artifacts` 字段)。记录跨 attempt 复用的步骤产物:
///
/// - `origin_ref` / `repository` / `use_date_tag` / `skip_unchanged`:部署请求
///   回传字段(续传不经过前端,由断点重建部署请求);
/// - `image_ref`:步骤 1 产物 —— 实际部署引用(日期标签或原始引用);
/// - `tar_local` / `tar_name`:步骤 2 产物 —— 本地 tar 绝对路径(断点期保留,
///   成功/放弃时显式删除)与远端上传文件名(`/tmp/<tar_name>`)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SingleResumeArtifacts {
    /// 原始镜像引用(部署请求的 `image`;步骤 1 重跑的打标签源、retag 目标)
    origin_ref: String,
    /// 打标签前缀(部署请求的 `repository`;步骤 1 重跑生成新标签时用)
    repository: String,
    /// 是否日期标签部署(部署请求的 `use_date_tag`)
    use_date_tag: bool,
    /// 步骤 1 产物:实际部署引用(`None` = 步骤 1 尚未完成)
    image_ref: Option<String>,
    /// 步骤 2 产物:本地 tar 绝对路径
    tar_local: Option<String>,
    /// 步骤 2 决定的远端 tar 文件名(步骤 3 上传到 `/tmp/<tar_name>`)
    tar_name: Option<String>,
    /// 回传字段:智能传输开关(部署请求的 `skip_unchanged`)
    skip_unchanged: bool,
}

/// 整栈部署断点产物(serde camelCase,存入 [`ResumeCheckpoint`] 的
/// `artifacts` 字段)。
///
/// - `services` / `skip_unchanged` / `force_archive`:部署请求回传字段
///   (服务分类列表由前端确认,续传时以断点为准重建请求);
/// - `unchanged`:步骤 1 智能传输判定结果(与 Local 服务顺序对齐;
///   续传**不重跑**判定 —— 远端状态已被上次部署部分改变,重放才确定);
/// - `files` / `locals` / `images`:步骤 2 打包产物,三个列表按打包顺序对齐
///   (远端镜像包文件名 / 本地 tar 绝对路径 / 镜像引用);
/// - `release_ts`:步骤 3 创建的发布时间戳(续传复用同一发布目录)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct StackResumeArtifacts {
    /// 部署请求的完整服务分类列表
    services: Vec<StackServiceChoice>,
    /// 智能传输判定结果(与 Local 服务顺序对齐;未启用时为空)
    unchanged: Vec<bool>,
    /// 回传字段:智能传输开关
    skip_unchanged: bool,
    /// 回传字段:强制留档
    force_archive: bool,
    /// 步骤 3 中创建的发布时间戳(`None` = 尚未创建发布目录)
    release_ts: Option<String>,
    /// 步骤 2 产物:镜像包远端文件名(按打包顺序)
    files: Vec<String>,
    /// 与 [`StackResumeArtifacts::files`] 对齐的本地 tar 绝对路径
    locals: Vec<String>,
    /// 与 [`StackResumeArtifacts::files`] 对齐的镜像引用(装载幂等检查用)
    images: Vec<String>,
}

/// 断点续传上下文:由 checkpoint 反序列化而来,驱动部署管线的跳步与幂等化。
#[derive(Debug, Clone)]
struct ResumeContext {
    /// 断点键(成功收尾按它清除断点)
    key: String,
    /// 续传起点:下一个待执行的步骤号(该步骤及之后都要执行)
    step_next: u32,
    /// 单镜像模式产物(mode = "single" 时有效)
    single: SingleResumeArtifacts,
    /// 整栈模式产物(mode = "stack" 时有效)
    stack: StackResumeArtifacts,
}

/// 单镜像部署的总步骤数(与 emit_progress 的 total 一致)。
const SINGLE_TOTAL_STEPS: u32 = 5;
/// 整栈部署的总步骤数。
const STACK_TOTAL_STEPS: u32 = 6;

/// 步骤号 → 中文标签(续传状态/日志展示用,自建映射):
/// 单镜像 1..5 = 打标签/导出压缩/上传镜像/同步文件/服务器部署;
/// 整栈 1..6 = 分类确认/打包/上传/装载/拉取/启动;
/// 超出范围(成功后清除断点,理论不可达)按"部署收尾"兜底。
fn resume_step_label(mode: &str, step_next: u32) -> String {
    let labels: &[&str] = match mode {
        MODE_SINGLE => &["打标签", "导出压缩", "上传镜像", "同步文件", "服务器部署"],
        MODE_STACK => &["分类确认", "打包", "上传", "装载", "拉取", "启动"],
        _ => &[],
    };
    if step_next < 1 {
        return "部署收尾".to_string();
    }
    labels
        .get(step_next as usize - 1)
        .map(|s| (*s).to_string())
        .unwrap_or_else(|| "部署收尾".to_string())
}

/// 解析单镜像断点产物(损坏 → `None`,调用方按断点数据损坏报错)。
fn parse_single_artifacts(v: &serde_json::Value) -> Option<SingleResumeArtifacts> {
    serde_json::from_value(v.clone()).ok()
}

/// 解析整栈断点产物(损坏 → `None`)。
fn parse_stack_artifacts(v: &serde_json::Value) -> Option<StackResumeArtifacts> {
    serde_json::from_value(v.clone()).ok()
}

/// 收集断点记录的本地临时 tar 路径(`deploy_resume_discard` 清理用;
/// 产物解析失败按空处理 —— 清理是尽力而为,不让放弃操作失败)。
fn resume_local_tars(cp: &ResumeCheckpoint) -> Vec<PathBuf> {
    match cp.mode.as_str() {
        MODE_SINGLE => parse_single_artifacts(&cp.artifacts)
            .into_iter()
            .filter_map(|a| a.tar_local)
            .map(PathBuf::from)
            .collect(),
        MODE_STACK => parse_stack_artifacts(&cp.artifacts)
            .into_iter()
            .flat_map(|a| a.locals.into_iter())
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// 断点产物序列化(纯结构体,序列化不会失败;兜底为 `Null`)。
fn artifacts_value<T: Serialize>(art: &T) -> serde_json::Value {
    serde_json::to_value(art).unwrap_or_default()
}

/// 落盘部署断点(尽力而为:失败仅告警,不影响部署管线本身)。
///
/// `step_next` = 下一个待执行的步骤号(刚完成步骤号 + 1;失败/取消发生
/// 在该步骤,续传时从它开始重跑)。每次保存都会覆盖同键旧条目并刷新 `ts`。
fn checkpoint_save(
    key: &str,
    mode: &str,
    step_next: u32,
    server: &ServerConfig,
    project: &ProjectConfig,
    artifacts: serde_json::Value,
) {
    let cp = ResumeCheckpoint {
        key: key.to_string(),
        mode: mode.to_string(),
        step_next,
        ts: chrono::Local::now().format("%F %T").to_string(),
        server_id: server.id.clone(),
        project_id: project.id.clone(),
        server_name: server.name.clone(),
        project_name: project.name.clone(),
        artifacts,
    };
    if let Err(e) = save_checkpoint(&cp) {
        log::warn!("保存部署断点失败(不影响本次部署): {}", e);
    }
}

/// 成功收尾:清除断点 + 删除断点期保留的本地临时 tar(尽力而为)。
/// 取消/失败不清 —— 断点与临时 tar 都要留给续传复用。
fn checkpoint_cleanup_on_success(key: &str, local_tars: &[PathBuf]) {
    match remove_checkpoint(key) {
        Ok(_) => log::info!("部署成功,已清除断点 {}", key),
        Err(e) => log::warn!("部署成功后清除断点失败: {}", e),
    }
    for path in local_tars {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("清理断点期保留的本地临时文件失败 ({}): {}", path.display(), e);
            }
        }
    }
}

/// 校验断点并构造续传上下文(模式未知/步骤号越界/产物损坏/缺关键产物 → 报错)。
fn resume_context_of(cp: &ResumeCheckpoint) -> Result<ResumeContext, String> {
    let corrupt = |why: &str| {
        format!(
            "断点数据损坏({}),请放弃该断点后重新部署",
            why
        )
    };
    match cp.mode.as_str() {
        MODE_SINGLE => {
            if cp.step_next < 1 || cp.step_next > SINGLE_TOTAL_STEPS {
                return Err(corrupt("步骤号越界"));
            }
            let art = parse_single_artifacts(&cp.artifacts)
                .ok_or_else(|| corrupt("单镜像产物解析失败"))?;
            if cp.step_next > 1 && art.image_ref.is_none() {
                return Err(corrupt("缺少已打标签的镜像引用"));
            }
            if cp.step_next > 2 && (art.tar_local.is_none() || art.tar_name.is_none()) {
                return Err(corrupt("缺少已导出的镜像包信息"));
            }
            Ok(ResumeContext {
                key: cp.key.clone(),
                step_next: cp.step_next,
                single: art,
                stack: StackResumeArtifacts::default(),
            })
        }
        MODE_STACK => {
            if cp.step_next < 1 || cp.step_next > STACK_TOTAL_STEPS {
                return Err(corrupt("步骤号越界"));
            }
            let art = parse_stack_artifacts(&cp.artifacts)
                .ok_or_else(|| corrupt("整栈产物解析失败"))?;
            if cp.step_next > 2 && (art.files.len() != art.locals.len() || art.files.len() != art.images.len()) {
                return Err(corrupt("镜像包产物列表不一致"));
            }
            Ok(ResumeContext {
                key: cp.key.clone(),
                step_next: cp.step_next,
                single: SingleResumeArtifacts::default(),
                stack: art,
            })
        }
        other => Err(format!("断点模式未知:{},请放弃该断点后重新部署", other)),
    }
}

// ===== 部署命令 =====

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

/// 发起批量部署(多服务器批量,UPGRADE-PLAN 阶段七):立即返回 `Ok(())`,
/// 编排在后台任务逐台串行执行,单台状态经 `deploy-batch` 事件推送。
///
/// - 请求校验(mode 合法 / server_ids 去重非空 / 项目存在 / 模式所需字段齐备)
///   同步完成,非法请求立即返回 `Err`,不进入后台任务;
/// - 逐台构造与单发一致的 `DeployRequest` / `StackDeployRequest`(仅替换
///   server_id)串行执行;每台的部署历史与成功/失败/取消通知按单发语义
///   落盘与发送;某台的服务器配置缺失只影响该台(emit failed),不中断批量;
/// - 批量路径不 emit `deploy-progress` / `deploy-done`(单台进度与结果由
///   `deploy-batch` 表达),`deploy-log` 每行加 `[服务器名] ` 前缀;
/// - 与断点续传互斥:批量不落断点、不支持续传(resume = None 且 checkpoint
///   关闭,见 [`DeployEmitOpts::batch`]),单台失败整台重跑;
/// - 取消:`cancel_deploy` 置位后,当前台由管线内取消检查中止(报
///   「部署已取消」),余台在循环顶部检查后逐台 emit skipped。
#[tauri::command]
pub fn deploy_batch(batch: BatchDeployRequest, app: AppHandle) -> Result<(), String> {
    // 同步校验:请求非法立即报错(不产生任何后台副作用)
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server_ids = validate_batch_request(&cfg, &batch)?;
    tauri::async_runtime::spawn(async move {
        // 编排层 panic 兜底:尽力 emit 一条 batch-done,避免前端停在单台 running
        let app_for_panic = app.clone();
        match CatchPanic::new(run_batch_deploy(app.clone(), batch, server_ids)).await {
            Ok(()) => {}
            Err(panic_info) => {
                log::error!("批量部署编排任务发生 panic: {}", panic_info);
                emit_batch_event(
                    &app_for_panic,
                    "",
                    "",
                    BATCH_STATE_BATCH_DONE,
                    "批量部署因内部错误中止,详情见日志",
                );
            }
        }
    });
    Ok(())
}

/// 批量部署请求校验(纯函数,便于单测):mode 合法、server_ids 去重后非空、
/// 项目存在、模式所需字段齐备(single:镜像引用非空、启用日期标签时部署
/// 仓库名非空;stack:服务分类列表非空)。返回去重后的目标服务器 ID 列表。
/// 服务器是否存在于配置**不在此校验** —— 逐台执行时缺失只标记该台 failed,
/// 不中断批量(见 [`run_batch_deploy`])。
fn validate_batch_request(cfg: &AppConfig, batch: &BatchDeployRequest) -> Result<Vec<String>, String> {
    match batch.mode.as_str() {
        MODE_SINGLE | MODE_STACK => {}
        other => {
            return Err(format!(
                "批量部署模式无效:{},仅支持 single(单镜像)或 stack(整栈)",
                other
            ))
        }
    }
    // server_ids 去重(保留首次出现顺序);空串/空白 ID 不在此剔除,
    // 交给逐台执行时按「未找到服务器配置」标记 failed,行为对前端可见
    let mut seen = std::collections::HashSet::new();
    let server_ids: Vec<String> = batch
        .server_ids
        .iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect();
    if server_ids.is_empty() {
        return Err("批量部署至少需要选择一台服务器".to_string());
    }
    // 项目必须存在(批量级校验:项目缺失时逐台必然全部失败,直接拒绝)
    let project = find_project(cfg, &batch.project_id)?;
    match batch.mode.as_str() {
        MODE_SINGLE => {
            if batch.image.as_deref().map(str::trim).unwrap_or("").is_empty() {
                return Err("批量部署(single)缺少镜像引用".to_string());
            }
            if batch.use_date_tag.unwrap_or(false)
                && batch
                    .repository
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
            {
                return Err("批量部署(single)启用日期标签时缺少部署仓库名".to_string());
            }
        }
        _ => {
            match &batch.services {
                Some(services) if !services.is_empty() => {}
                _ => {
                    return Err(format!(
                        "项目「{}」批量整栈部署缺少服务传输分类列表",
                        project.name
                    ))
                }
            }
        }
    }
    Ok(server_ids)
}

/// 批量部署编排(后台任务):逐台串行执行。每台开始 emit `deploy-batch`
/// running,结束 emit success/failed;用户取消后余台 emit skipped(不执行);
/// 全部结束 emit batch-done 汇总。单台的部署历史与通知由
/// [`run_one_deploy`] / [`run_one_deploy_stack`] 按单发语义处理。
///
/// 与断点续传的互斥通过 [`DeployEmitOpts::batch`] 表达:resume 一律 `None`、
/// `checkpoint = false`(管线内所有 `checkpoint_save` 与成功清理整体跳过,
/// 本地临时 tar 恢复用完即删的 Drop 语义)。
async fn run_batch_deploy(app: AppHandle, batch: BatchDeployRequest, server_ids: Vec<String>) {
    // 批量开始时统一重置取消标志(与单发部署的管线入口语义一致:
    // 清掉上一次部署/批量遗留的取消位,之后由用户取消置位)
    reset_cancelled(&app);
    let mode = batch.mode.clone();
    let mut ok_count = 0u32;
    let mut fail_count = 0u32;
    let mut skip_count = 0u32;

    for server_id in &server_ids {
        // 服务器配置逐台现查(缺失只影响该台,不中断批量)
        let server = load_config()
            .ok()
            .and_then(|cfg| find_server(&cfg, server_id).ok().cloned());
        let Some(server) = server else {
            fail_count += 1;
            let msg = format!("未找到 ID 为「{}」的服务器配置", server_id);
            log::warn!("批量部署:{}", msg);
            emit_batch_event(&app, server_id, server_id, BATCH_STATE_FAILED, &msg);
            continue;
        };
        let server_name = server.name.clone();

        // 余台取消检查:已取消则不再执行,逐台标记 skipped
        if is_cancelled(&app) {
            skip_count += 1;
            emit_batch_event(&app, server_id, &server_name, BATCH_STATE_SKIPPED, "已取消");
            continue;
        }

        emit_batch_event(
            &app,
            server_id,
            &server_name,
            BATCH_STATE_RUNNING,
            "开始部署",
        );
        // 批量事件语义:不 emit deploy-progress / deploy-done,日志加服务器前缀
        let opts = DeployEmitOpts::batch(&server_name);
        let result = match mode.as_str() {
            MODE_SINGLE => {
                let req = DeployRequest {
                    image: batch.image.clone().unwrap_or_default(),
                    repository: batch.repository.clone().unwrap_or_default(),
                    server_id: server_id.clone(),
                    project_id: batch.project_id.clone(),
                    use_date_tag: batch.use_date_tag.unwrap_or(false),
                    password_plain: batch.password_plain.clone(),
                    skip_unchanged: batch.skip_unchanged,
                };
                run_one_deploy(&app, req, None, opts).await
            }
            MODE_STACK => {
                let req = StackDeployRequest {
                    project_id: batch.project_id.clone(),
                    server_id: server_id.clone(),
                    services: batch.services.clone().unwrap_or_default(),
                    password_plain: batch.password_plain.clone(),
                    skip_unchanged: batch.skip_unchanged,
                    force_archive: batch.force_archive,
                };
                run_one_deploy_stack(&app, req, None, opts).await
            }
            // deploy_batch 入口已校验模式,防御性兜底
            other => Err(format!("批量部署模式无效:{}", other)),
        };
        match result {
            Ok(_) => {
                ok_count += 1;
                emit_batch_event(
                    &app,
                    server_id,
                    &server_name,
                    BATCH_STATE_SUCCESS,
                    "部署完成",
                );
            }
            Err(e) => {
                fail_count += 1;
                // 当前台因取消失败时 message = 「部署已取消」,
                // 余台由循环顶部的检查逐台置为 skipped
                emit_batch_event(&app, server_id, &server_name, BATCH_STATE_FAILED, &e);
            }
        }
    }

    emit_batch_event(
        &app,
        "",
        "",
        BATCH_STATE_BATCH_DONE,
        &format!(
            "批量部署结束:{} 成功 / {} 失败 / {} 跳过",
            ok_count, fail_count, skip_count
        ),
    );
}

/// emit `deploy-batch` 事件(批量部署逐台状态/汇总,payload camelCase)。
fn emit_batch_event(
    app: &AppHandle,
    server_id: &str,
    server_name: &str,
    state: &str,
    message: &str,
) {
    let _ = app.emit(
        "deploy-batch",
        BatchDeployEvent {
            server_id: server_id.to_string(),
            server_name: server_name.to_string(),
            state: state.to_string(),
            message: message.to_string(),
        },
    );
}

/// 查询某个服务器/项目是否存在可续传的部署断点。
///
/// 同一服务器 + 项目可能同时存在单镜像与整栈两条断点(互为不同键),
/// 取最近落盘(`ts` 最新)的一条;无断点返回 `None`。
#[tauri::command]
pub fn deploy_resume_status(
    server_id: String,
    project_id: String,
) -> Result<Option<ResumeView>, String> {
    Ok(load_resume_map()
        .into_values()
        .filter(|c| c.server_id == server_id && c.project_id == project_id)
        .max_by(|a, b| a.ts.cmp(&b.ts))
        .map(|cp| resume_view_of(&cp)))
}

/// `deploy_resume_status` 返回的断点视图(camelCase,前端续传入口展示用)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeView {
    /// 断点键(传给 `deploy_resume_start` / `deploy_resume_discard`)
    pub key: String,
    /// 部署模式:`single` / `stack`
    pub mode: String,
    /// 下一个待执行的步骤号(续传起点)
    pub step_next: u32,
    /// 步骤中文名(如「打标签」「上传」)
    pub step_label: String,
    /// 断点最近落盘时间
    pub ts: String,
    pub server_name: String,
    pub project_name: String,
}

/// [`ResumeCheckpoint`] → 前端视图(纯函数,便于单测)。
fn resume_view_of(cp: &ResumeCheckpoint) -> ResumeView {
    ResumeView {
        key: cp.key.clone(),
        mode: cp.mode.clone(),
        step_next: cp.step_next,
        step_label: resume_step_label(&cp.mode, cp.step_next),
        ts: cp.ts.clone(),
        server_name: cp.server_name.clone(),
        project_name: cp.project_name.clone(),
    }
}

/// 从断点续传部署:校验断点与服务器/项目仍存在后,走与正常部署完全相同的
/// 后台任务路径(`spawn_deploy_task`,事件/历史/通知语义一致,前端零特殊
/// 处理)。管线从断点的 `step_next` 起、对已完成产物幂等化复用。
#[tauri::command]
pub fn deploy_resume_start(
    key: String,
    password_plain: Option<String>,
    app: AppHandle,
) -> Result<(), String> {
    let cp = load_resume_map()
        .remove(&key)
        .ok_or_else(|| format!("断点不存在或已被清理: {}", key))?;
    // 服务器/项目仍存在(被删除的配置无法续传)
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    find_server(&cfg, &cp.server_id)?;
    find_project(&cfg, &cp.project_id)?;
    let ctx = resume_context_of(&cp)?;
    match cp.mode.as_str() {
        MODE_SINGLE => {
            let req = DeployRequest {
                image: ctx.single.origin_ref.clone(),
                repository: ctx.single.repository.clone(),
                server_id: cp.server_id.clone(),
                project_id: cp.project_id.clone(),
                use_date_tag: ctx.single.use_date_tag,
                password_plain,
                skip_unchanged: Some(ctx.single.skip_unchanged),
            };
            spawn_deploy_task(app.clone(), async move {
                // 续传与单发同语义:全量事件 + 断点续传开启(checkpoint = true)
                run_one_deploy(&app, req, Some(ctx), DeployEmitOpts::single()).await
            });
            Ok(())
        }
        MODE_STACK => {
            let req = StackDeployRequest {
                project_id: cp.project_id.clone(),
                server_id: cp.server_id.clone(),
                services: ctx.stack.services.clone(),
                password_plain,
                skip_unchanged: Some(ctx.stack.skip_unchanged),
                force_archive: Some(ctx.stack.force_archive),
            };
            spawn_deploy_task(app.clone(), async move {
                run_one_deploy_stack(&app, req, Some(ctx), DeployEmitOpts::single()).await
            });
            Ok(())
        }
        // resume_context_of 已校验模式,防御性兜底
        other => Err(format!("断点模式未知:{}", other)),
    }
}

/// 放弃断点续传:删除断点 + 删除断点期保留的本地临时 tar + 尽力删除远端
/// 临时产物(单镜像 `/tmp` 下的 tar;整栈本次的部分发布目录;连接失败等
/// 一律忽略,不报错)。
#[tauri::command]
pub async fn deploy_resume_discard(key: String) -> Result<(), String> {
    let cp = remove_checkpoint(&key)
        .map_err(|e| format!("删除部署断点失败: {}", e))?
        .ok_or_else(|| format!("断点不存在或已被清理: {}", key))?;

    // 1) 本地临时 tar(尽力而为,失败仅告警)
    for path in resume_local_tars(&cp) {
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("清理本地临时文件失败 ({}): {}", path.display(), e);
            }
        }
    }

    // 2) 远端临时产物(尽力而为:任何失败仅记日志,不让放弃操作失败)
    discard_remote_artifacts(&cp).await;
    Ok(())
}

/// 远端临时产物清理(`deploy_resume_discard` 的尽力而为子步)。
async fn discard_remote_artifacts(cp: &ResumeCheckpoint) {
    // 组装待删除路径:单镜像 = /tmp/<tar_name>;整栈 = releases/<ts> 整目录
    // (目录里只有本次 attempt 的半成品镜像包与 compose 副本)
    let targets: Vec<String> = match cp.mode.as_str() {
        MODE_SINGLE => parse_single_artifacts(&cp.artifacts)
            .and_then(|a| a.tar_name)
            .map(|n| vec![remote_join("/tmp", &n)])
            .unwrap_or_default(),
        MODE_STACK => parse_stack_artifacts(&cp.artifacts)
            .and_then(|a| a.release_ts)
            .map(|ts| vec![releases_dir_of(cp, &ts)])
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    if targets.is_empty() {
        return;
    }
    // 建连凭据:只用已保存的密文/密钥;密码认证且未存密码 → 无法建连,跳过
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("放弃断点:读取配置失败,跳过远端临时产物清理: {}", e);
            return;
        }
    };
    let server = match find_server(&cfg, &cp.server_id) {
        Ok(s) => s.clone(),
        Err(e) => {
            log::warn!("放弃断点:{}跳过远端临时产物清理", e);
            return;
        }
    };
    let (password, key_pass) = match (
        resolve_password(&server.auth.auth_type, None, server.auth.password_enc.as_deref()),
        resolve_key_passphrase(&server),
    ) {
        (Ok(p), Ok(k)) => (p, k),
        _ => {
            log::warn!("放弃断点:无法解析服务器「{}」的登录凭据,跳过远端临时产物清理", server.name);
            return;
        }
    };
    let connect = SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default());
    let mut client = match with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        connect,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("放弃断点:连接服务器「{}」失败,跳过远端临时产物清理: {}", server.name, e);
            return;
        }
    };
    let cmd = format!(
        "rm -rf {}",
        targets.iter().map(|t| shell_single_quote(t)).collect::<Vec<_>>().join(" ")
    );
    match with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "清理远端临时产物超时",
        "请检查服务器网络后重试",
        client.exec(&cmd, &mut |_| {}),
    )
    .await
    {
        Ok(_) => log::info!("放弃断点:已清理远端临时产物 {:?}", targets),
        Err(e) => log::warn!("放弃断点:清理远端临时产物失败(忽略): {}", e),
    }
}

/// 整栈断点的发布目录路径(放弃清理用;服务器配置缺失时退化为 ts 路径)。
fn releases_dir_of(cp: &ResumeCheckpoint, ts: &str) -> String {
    let remote_dir = load_config()
        .ok()
        .and_then(|cfg| find_server(&cfg, &cp.server_id).ok().map(|s| s.remote_dir.clone()))
        .unwrap_or_default();
    releases_dir(&remote_dir, ts)
}

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
struct DeployEmitOpts {
    /// 管线内是否 emit `deploy-progress`(批量关闭:进度由 `deploy-batch` 表达)
    emit_progress: bool,
    /// 收尾是否 emit `deploy-done`(批量关闭:结果由 `deploy-batch` 表达)
    emit_done: bool,
    /// `deploy-log` 每行追加的前缀(单发 = 空串;批量 = `[服务器名] `)
    log_prefix: String,
    /// 是否落盘部署断点(批量关闭:批量与断点续传互斥,失败整台重跑)
    checkpoint: bool,
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
async fn run_one_deploy(
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
async fn run_one_deploy_stack(
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

/// 管线执行 + 统一收尾([`spawn_deploy_task`] 与批量单台路径共用):
/// panic 兜底([`CatchPanic`])+ 收尾事件 + 部署历史 + webhook 通知。
///
/// 正常结束路径(成功/失败/取消)在通知分发之前按需 emit `deploy-done`
/// (`emit_done = false` 的批量路径不 emit,结果由 `deploy-batch` 表达),
/// 之后落地部署历史记录(由管线组装的 [`DeployRecord`],append 失败仅告警,
/// 不影响收尾),并按项目配置的 `notify_webhook` 异步发送 webhook 通知
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
    // deploy-done 之后落地部署历史(成功/失败/取消统一记录)
    if let Some(record) = &record {
        // webhook 通知:项目配置了 notify_webhook 才发;阻塞 HTTP 放 blocking
        // 线程池 fire-and-forget,失败仅告警,不影响部署收尾
        if let Some(url) = webhook_url.filter(|u| !u.trim().is_empty()) {
            let payload = webhook_payload(record);
            tauri::async_runtime::spawn_blocking(move || send_webhook(&url, &payload));
        }
        append_record(record.clone());
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
    let password = resolve_password(
        &server.auth.auth_type,
        req.password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
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
        run_hook(app, &mut client, &project, HookKind::Pre, &server.remote_dir).await?;
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
        // 服务器相对路径未填时回退为本地路径的末段名(与编辑表单的默认值同一口径):
        // 目录映射传目录名本身、文件映射传文件名,避免把内容铺进部署根目录
        let remote_rel = if mapping.remote.trim().is_empty() {
            local_basename(&mapping.local).unwrap_or_default()
        } else {
            mapping.remote.clone()
        };
        let full_remote = remote_join(&server.remote_dir, &remote_rel);
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
            remote_file: remote_compose_path(&server.remote_dir),
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
fn is_windows_absolute_path(path: &str) -> bool {
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
        shell_single_quote(&server.remote_dir),
        shell_single_quote(remote_compose),
    );
    emit_log(
        app,
        &format!(
            "启动服务: cd {} && docker compose -f {} up -d",
            server.remote_dir, remote_compose
        ),
    );
    exec_forwarded(app, client, &up_cmd, 600).await?;

    // 5.4 健康检查(up 后按预算轮询服务状态;health_wait_secs=0 时跳过)
    health_check(
        app,
        client,
        project,
        &server.remote_dir,
        remote_compose,
        override_names,
    )
    .await?;

    // 5.5 部署后钩子(健康检查通过后执行;失败仅告警,不影响部署结果)
    run_hook(app, client, project, HookKind::Post, &server.remote_dir).await?;

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

/// 钩子执行失败的结果映射(纯函数,便于单测)。
///
/// - 取消(错误文案为 [`CANCELLED_MSG`])一律原样透传,Pre/Post 同口径
///   (与 pull 步骤一致):`spawn_deploy_task` 按该文案判定 cancel 事件,
///   包装成其他文案会判定失配,把用户取消误报为部署失败(通知走 failure);
/// - `Pre` 其余失败:包装为「<钩子名>执行失败,部署中止:<原因>」;
/// - `Post` 其余失败:返回 `None`,由调用方仅告警、不影响部署结果。
fn hook_failure_result(which: HookKind, e: &str) -> Option<String> {
    if e == CANCELLED_MSG {
        return Some(e.to_string());
    }
    match which {
        HookKind::Pre => Some(format!("{}执行失败,部署中止: {}", which.label(), e)),
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

// ===== 智能传输(跳过未变化镜像)=====

/// 智能传输:查询远端镜像列表并构建 `repo:tag` → 镜像 ID 映射。
///
/// 使用 [`REMOTE_IMAGES_CMD_FULL`](完整 64 位 ID 口径,与本地
/// [`crate::docker::image_id_by_ref`] 同口径,跳过判定 [`same_image_id`] 才能
/// 相等;预览走 [`REMOTE_IMAGES_CMD`] 的 12 位截断口径,两侧独立);
/// 行解析复用 [`parse_image_lines`](单行解析失败仅告警跳过);退出码非 0
/// (远端 Docker 不可用等)以中文错误返回,由调用方决定中止或降级。
/// 单镜像 / 整栈两条部署管线共用的对比数据源。
async fn query_remote_image_id_map(
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
    let password = resolve_password(
        &server.auth.auth_type,
        req.password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;

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
    let release_dir = releases_dir(&server.remote_dir, &ts);

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
        run_hook(app, &mut client, &project, HookKind::Pre, &server.remote_dir).await?;
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
        if checkpoint {
            checkpoint_save(&key, MODE_STACK, 6, &server, &project, artifacts_value(&art));
        }
    }

    // ---- 步骤 6:启动 ----
    emit_progress(app, 6, 6, "启动");
    ensure_not_cancelled(app)?;
    let remote_compose = remote_compose_path(&server.remote_dir);
    let up_cmd = compose_up_cmd(&server.remote_dir, &remote_compose, &override_names);
    emit_log(app, &format!("启动服务: {}", up_cmd));
    exec_forwarded(app, &mut client, &up_cmd, STACK_COMPOSE_TIMEOUT_SECS).await?;

    // 健康检查(up 后按预算轮询服务状态;health_wait_secs=0 时跳过)
    // overrides 与 pull/up 同源,override-only 服务同样进入健康判定
    health_check(
        app,
        &mut client,
        &project,
        &server.remote_dir,
        &remote_compose,
        &override_names,
    )
    .await?;

    // 部署后钩子(健康检查通过后执行;失败仅告警,不影响部署结果)
    run_hook(app, &mut client, &project, HookKind::Post, &server.remote_dir).await?;

    // ---- 收尾:向发布目录写入回滚资料(manifest.json + compose 副本)----
    // 尽力而为:失败仅告警(缺 manifest/副本时回滚命令会优雅降级),不推翻
    // 已成功的部署结果;须在清理旧 releases 之前写入,避免目录先被清掉
    ensure_not_cancelled(app)?;
    let manifest = ReleaseManifest::new(project.name.clone(), ts.clone(), manifest_images);
    write_release_artifacts(app, &mut client, &project, &release_dir, &manifest).await;

    // ---- 清理旧 releases(仅留最新 5 个,尽力而为,失败仅告警)----
    ensure_not_cancelled(app)?;
    let cleanup_cmd = cleanup_releases_cmd(&server.remote_dir);
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

// ===== 整栈部署预览(dry-run,Task 6,独立功能不接入部署流程)=====

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
        exec_collect(&mut client, &compose_containers_cmd(&server.remote_dir)),
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
fn same_image_id(a: &str, b: &str) -> bool {
    fn norm(id: &str) -> &str {
        let id = id.trim();
        id.strip_prefix("sha256:").unwrap_or(id)
    }
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && !b.is_empty() && a.eq_ignore_ascii_case(b)
}

/// 取远端目录的基名(去尾部 `/` 后取最后一个 `/` 之后的部分;
/// 根目录/空串 → 空串)。远端 compose 部署的项目名 = 该基名。
fn remote_dir_basename(remote_dir: &str) -> &str {
    let trimmed = remote_dir.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// 拼装查询远端 compose 项目现存容器的命令:`docker ps -a` 按
/// `com.docker.compose.project` 标签过滤(项目名 = remote_dir 基名,含已退出
/// 容器),JSON 输出每容器一行;`--filter` 参数整体单引号包裹防注入。
fn compose_containers_cmd(remote_dir: &str) -> String {
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
fn parse_image_lines(out: &str) -> Vec<ImageInfo> {
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
fn parse_container_lines(out: &str) -> HashMap<String, String> {
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
fn parse_container_line(line: &str) -> Option<(String, String)> {
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
fn build_manifest_images(
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
async fn write_release_artifacts(
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
    // 校验项目存在(前端按项目发起回滚;remote_dir 取自服务器配置)
    find_project(&cfg, &project_id)?;
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

    let releases_root = remote_join(&server.remote_dir, "releases");
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询发布列表超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &ls_dir_cmd(&releases_root)),
    )
    .await?;
    if code != 0 {
        // releases 目录不存在(从未整栈部署)→ 空列表
        return Ok(Vec::new());
    }
    let mut briefs = Vec::new();
    for ts in parse_ls_lines(&out) {
        let dir = remote_join(&releases_root, &ts);
        let (code, out) = with_timeout(
            SSH_EXEC_TIMEOUT_SECS,
            "查询发布目录超时",
            "请检查服务器网络后重试",
            exec_collect(&mut client, &ls_dir_cmd(&dir)),
        )
        .await?;
        if code != 0 {
            // 目录可能刚被清理或不可读,跳过该条目,不拖垮整个列表
            log::warn!("跳过无法读取的发布目录 {} (退出码 {})", dir, code);
            continue;
        }
        let files = parse_ls_lines(&out);
        let has_manifest = files.iter().any(|f| f == "manifest.json");
        let has_compose_copy = files.iter().any(|f| f == "docker-compose.yml");
        let mut services = Vec::new();
        if has_manifest {
            let manifest_path = remote_join(&dir, "manifest.json");
            let (code, out) = with_timeout(
                SSH_EXEC_TIMEOUT_SECS,
                "读取发布清单超时",
                "请检查服务器网络后重试",
                exec_collect(&mut client, &cat_file_cmd(&manifest_path)),
            )
            .await?;
            if code == 0 {
                if let Some(m) = parse_release_manifest(&out) {
                    services = m.images.into_iter().map(|i| i.service).collect();
                }
            } else {
                log::warn!("读取发布清单失败 ({}): 退出码 {}", manifest_path, code);
            }
        }
        briefs.push(ReleaseBrief {
            ts,
            files,
            services,
            has_manifest,
            has_compose_copy,
        });
    }
    // 新 → 旧:时间戳形如 20260905-101010,字符串倒序即时间倒序
    briefs.sort_by(|a, b| b.ts.cmp(&a.ts));
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

    let release_dir = releases_dir(&server.remote_dir, release_ts);

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
    let remote_compose = remote_compose_path(&server.remote_dir);
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
    let up_cmd = compose_up_cmd(&server.remote_dir, &remote_compose, &override_names);
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
            remote_compose_path(&server.remote_dir),
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
    let up_cmd = compose_up_cmd(&server.remote_dir, &compose_path, &overrides);
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
    pub has_manifest: bool,
    pub has_compose_copy: bool,
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

    // 1. 扫 compose 文件 → 项目目录
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "扫描项目超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &cleanup_scan_compose_cmd(&root)),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "扫描目录「{}」失败(目录可能不存在或无权限)",
            root
        ));
    }
    let compose_paths = abs_path_lines(&out);
    let mut dirs: Vec<String> = Vec::new();
    for p in &compose_paths {
        if let Some((parent, _)) = p.rsplit_once('/') {
            if !parent.is_empty() && !dirs.iter().any(|d| d == parent) {
                dirs.push(parent.to_string());
            }
        }
    }

    // 2. 合并 docker 已知的 compose 项目工作目录(label 是权威来源,可补
    //    上"目录里 compose 文件被删但容器仍在"的情况)
    let label_cmd = "docker ps --format '{{json .}}'";
    if let Ok((0, ps_out)) =
        exec_collect(&mut client, label_cmd).await
    {
        let (items, _) = parse_cleanup_ndjson(&ps_out);
        let cids: Vec<String> = items
            .iter()
            .map(|v| jstr(v, "ID"))
            .filter(|s| !s.is_empty())
            .collect();
        if !cids.is_empty() {
            let quoted: Vec<String> = cids.iter().map(|c| shell_single_quote(c)).collect();
            let wd_cmd = format!(
                "docker inspect --format '{{{{index .Config.Labels \"com.docker.compose.project.working_dir\"}}}}' {} 2>/dev/null",
                quoted.join(" ")
            );
            if let Ok((_, wd_out)) = exec_collect(&mut client, &wd_cmd).await {
                for line in abs_path_lines(&wd_out) {
                    if !dirs.iter().any(|d| d == &line) {
                        dirs.push(line);
                    }
                }
            }
        }
    }
    dirs.sort();

    // 3. 归档 + 运行容器数
    let (_, rel_out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "扫描归档超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &cleanup_scan_releases_cmd(&root)),
    )
    .await?;
    let release_paths = abs_path_lines(&rel_out);

    let (_, ps_all_out) =
        exec_collect(&mut client, "docker ps -a --format '{{json .}}'").await.unwrap_or((0, String::new()));
    let (containers, _) = parse_cleanup_ndjson(&ps_all_out);

    let mut projects = Vec::new();
    for dir in dirs {
        let compose_file = compose_paths
            .iter()
            .find(|p| p.rsplit_once('/').map(|(d, _)| d) == Some(dir.as_str()))
            .cloned()
            .unwrap_or_default();
        let mut releases: Vec<String> = release_paths
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
        let running = containers
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
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询归档超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &ls_dir_cmd(&releases_root)),
    )
    .await?;
    let mut ts_list = if code == 0 { parse_ls_lines(&out) } else { Vec::new() };
    ts_list.sort_by(|a, b| b.cmp(a));

    // 逐归档读文件清单与 manifest(与既有 rollback_list_releases 同口径)
    let mut releases: Vec<RollbackReleaseDetail> = Vec::new();
    for ts in ts_list.iter().take(50) {
        let rd = remote_join(&releases_root, ts);
        let Ok((code, files_out)) = exec_collect(&mut client, &ls_dir_cmd(&rd)).await else {
            continue;
        };
        if code != 0 {
            continue;
        }
        let files = parse_ls_lines(&files_out);
        let packages: Vec<String> = files.iter().filter(|f| f.ends_with(".tar.gz")).cloned().collect();
        let has_manifest = files.iter().any(|f| f == "manifest.json");
        let has_compose_copy = files.iter().any(|f| f == "docker-compose.yml");
        let mut services: Vec<String> = Vec::new();
        if has_manifest {
            let mp = remote_join(&rd, "manifest.json");
            if let Ok((0, m_out)) = exec_collect(&mut client, &cat_file_cmd(&mp)).await {
                if let Some(m) = parse_release_manifest(&m_out) {
                    services = m.images.into_iter().map(|i| i.service).collect();
                }
            }
        }
        releases.push(RollbackReleaseDetail {
            ts: ts.clone(),
            dir: rd,
            packages,
            services,
            has_manifest,
            has_compose_copy,
        });
    }

    // 日期标签:读该目录 compose 的镜像仓库名,再逐个列标签
    let mut repositories: Vec<RollbackTagDetail> = Vec::new();
    let compose_candidates = [
        remote_join(&dir, "docker-compose.yml"),
        remote_join(&dir, "compose.yml"),
    ];
    for cand in compose_candidates.iter() {
        let Ok((0, text)) = exec_collect(&mut client, &cat_file_cmd(cand)).await else {
            continue;
        };
        for repo in compose_image_repos(&text) {
            if repositories.iter().any(|r| r.repository == repo) {
                continue;
            }
            let repo = repo.trim().to_string();
            if repo.is_empty() {
                continue;
            }
            let cmd = format!("docker images {} --format '{{{{json .}}}}'", shell_single_quote(&repo));
            let Ok((0, out)) = exec_collect(&mut client, &cmd).await else {
                continue;
            };
            let (items, _) = parse_cleanup_ndjson(&out);
            let mut tags: Vec<TagBrief> = items
                .iter()
                .filter(|v| is_date_tag(&jstr(v, "Tag")))
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
        if !repositories.is_empty() {
            break;
        }
    }

    Ok(RollbackProjectDetail {
        dir,
        compose_file: compose_candidates
            .iter()
            .find(|c| **c != String::new())
            .cloned()
            .unwrap_or_default(),
        releases,
        repositories,
    })
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
fn rollback_notify_text(
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
    let result = match CatchPanic::new(fut).await {
        Ok(res) => res,
        Err(panic_info) => {
            log::error!("回滚管线发生 panic: {}", panic_info);
            Err("回滚过程发生内部错误,详情见日志".to_string())
        }
    };
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
pub fn docker_inspect_cmd(image: &str) -> String {
    format!("docker image inspect {}", shell_single_quote(image))
}

/// 逐行解析 `ls -1` 输出为条目列表(trim + 去空行;纯函数,便于单测)。
pub fn parse_ls_lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// 解析发布目录的 manifest.json 内容(纯函数):损坏 / 缺字段 → `None`
/// (调用方按"无清单"降级,不让列表与回滚功能失败)。
pub fn parse_release_manifest(json: &str) -> Option<ReleaseManifest> {
    serde_json::from_str(json.trim()).ok()
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
fn format_log_line(prefix: &str, msg: &str) -> String {
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
struct TempFileGuard {
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

// ===== 清理分析(阶段八:prune 预览 + 定向执行;与既有 prune_server 同通道)=====

/// 清理分析单条目:未使用镜像(无标签/悬空)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupImage {
    pub id: String,
    pub repository: String,
    pub tag: String,
    pub size: String,
    /// 是否被任一容器(含停止/创建态)引用:true 时前端禁选(删不掉,rmi 会失败)
    pub in_use: bool,
}

/// 清理分析单条目:停止容器。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupContainer {
    pub id: String,
    pub names: String,
    pub image: String,
    pub status: String,
}

/// 清理分析单条目:未使用卷。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupVolume {
    pub name: String,
}

/// 分项目视图单条目:日期标签镜像(`repo:YYYYmmdd-HHMMSS`)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupTagImage {
    pub reference: String,
    pub id: String,
    pub size: String,
    pub created: String,
    /// 被任一容器引用时为 true,前端禁选并跳过删除
    pub in_use: bool,
}

/// 分项目视图:服务器上扫描到的项目目录及其可清理项。
///
/// 列表以**服务器真实目录**为准(扫描起点可配),应用内项目仅作标注;
/// 归档与标签的归属靠该目录下 compose 文件里的 `image:` 仓库名匹配。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupProject {
    /// 项目目录(绝对路径)
    pub dir: String,
    /// 该项目使用的 compose 文件(未扫到时为空串)
    pub compose_file: String,
    /// `du -sh` 输出(如 "1.2G";取不到为 "?")
    pub size: String,
    /// 发布归档目录(完整远端路径,新→旧)
    pub releases: Vec<String>,
    /// 该项目的日期标签镜像(新→旧)
    pub tag_images: Vec<CleanupTagImage>,
    /// 匹配到的应用内项目名(仅标注;空串=服务器上存在但软件内未配置)
    pub app_project: String,
}

/// 单条扫描命令的诊断信息(命令原文、退出码、输出摘要)。
///
/// 用于「清理识别不到」类问题的自证:即使某节为空,也能看到命令实际
/// 退出码与服务器返回,而不必猜。
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CleanupDiag {
    pub label: String,
    pub cmd: String,
    /// 传输层失败时为 None
    pub exit_code: Option<i32>,
    /// 输出摘要(最多 [`CLEANUP_DIAG_OUTPUT_LINES`] 行)
    pub output: String,
}

/// 清理分析报告(各节互不影响,单项查询失败记入 errors/warnings 不阻断其余)。
#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CleanupReport {
    pub dangling_images: Vec<CleanupImage>,
    pub stopped_containers: Vec<CleanupContainer>,
    pub unused_volumes: Vec<CleanupVolume>,
    pub build_cache_size: String,
    /// 分项目可清理项(第三批新增)
    pub projects: Vec<CleanupProject>,
    /// 本次实际使用的扫描起点
    pub scan_root: String,
    /// 非致命提示(命令 stderr 混入、解析跳过的行等)
    pub warnings: Vec<String>,
    /// 逐条命令诊断
    pub diagnostics: Vec<CleanupDiag>,
    /// 致命错误(该节整体不可用)
    pub errors: Vec<String>,
}

/// 清理执行结果(逐节)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupSectionResult {
    pub label: String,
    pub ok: bool,
    pub output: String,
}

/// 分项目清理目标(前端勾选后原样回传:只删用户看到并勾选的条目)。
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CleanupProjectTarget {
    /// 项目目录(仅用于结果标签)
    pub dir: String,
    /// 待删归档目录(完整远端路径)
    #[serde(default)]
    pub release_dirs: Vec<String>,
    /// 待删镜像引用(`repo:tag`)
    #[serde(default)]
    pub image_refs: Vec<String>,
}

/// 清理执行勾选项(分节布尔 + 各节显式目标列表)。
///
/// 为什么带目标列表:清理只应删除用户在预览里看到并勾选的条目。
/// 逐条显式传入既避免 `prune -f` 的"清理范围外扩"(例如 volume prune
/// 会连未列出的未使用卷一起删),也让执行结果与预览一一对应。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupSections {
    pub images: bool,
    pub containers: bool,
    pub volumes: bool,
    pub builder: bool,
    /// 待删无标签镜像 ID 列表
    #[serde(default)]
    pub image_ids: Vec<String>,
    /// 待删停止容器 ID 列表
    #[serde(default)]
    pub container_ids: Vec<String>,
    /// 待删未使用卷名列表
    #[serde(default)]
    pub volume_names: Vec<String>,
    /// 分项目清理目标
    #[serde(default)]
    pub projects: Vec<CleanupProjectTarget>,
}

impl CleanupSections {
    /// 是否至少勾选了一项可执行内容(供「至少勾选一项」校验)。
    fn has_any(&self) -> bool {
        (self.images && !self.image_ids.is_empty())
            || (self.containers && !self.container_ids.is_empty())
            || (self.volumes && !self.volume_names.is_empty())
            || self.builder
            || !self.projects.is_empty()
    }
}

/// 宽松解析 NDJSON 行:跳过非 JSON 行(收进 warnings),不再让整节失败。
///
/// 服务器上 `docker` 可能把 `WARNING: No swap limit support` 之类的提示
/// 写进 stderr,而 [`exec_collect`] 把 stdout+stderr 合并返回 —— 旧实现
/// 只要有一行不是 JSON 就整节解析失败、列表恒为空,表现为"扫描不到"。
fn parse_cleanup_ndjson(text: &str) -> (Vec<serde_json::Value>, Vec<String>) {
    let mut items = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // 只对 `{` 开头的行尝试 JSON;其余按提示行收集(限 3 条防刷屏)
        if t.starts_with('{') {
            match serde_json::from_str::<serde_json::Value>(t) {
                Ok(v) => items.push(v),
                Err(e) => {
                    if warnings.len() < 3 {
                        warnings.push(format!("第 {} 行解析失败: {}", i + 1, e));
                    }
                }
            }
        } else if warnings.len() < 3 {
            warnings.push(t.to_string());
        }
    }
    (items, warnings)
}

/// 输出摘要保留行数(诊断区展示)。
const CLEANUP_DIAG_OUTPUT_LINES: usize = 8;

/// 取文本前 n 行(诊断输出摘要;超长时追加省略标记)。
fn head_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines[..lines.len().min(n)].join("\n");
    if lines.len() > n {
        out.push_str(&format!("\n… (共 {} 行)", lines.len()));
    }
    out
}

fn jstr(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// 单条扫描命令的结果:解析后的条目 + 非致命提示 + 诊断信息。
struct CleanupQuery {
    /// JSON 模式下的解析条目;文本模式下为空
    items: Vec<serde_json::Value>,
    warnings: Vec<String>,
    diag: CleanupDiag,
    /// 完整输出(文本模式解析用;诊断里只有截断摘要,不进前端)
    full_output: String,
}

/// 执行一条清理扫描命令(`json_mode` 决定是否按 NDJSON 解析)。
///
/// 传输层错误与退出码非 0 都只记 warnings(不向上传播),使单节失败
/// 不影响其余节的扫描结果 —— 这正是"为什么某一节是 0"可排查的前提。
async fn run_cleanup_query(
    client: &mut SshClient,
    label: &str,
    cmd: &str,
    json_mode: bool,
) -> CleanupQuery {
    let mut diag = CleanupDiag {
        label: label.to_string(),
        cmd: cmd.to_string(),
        exit_code: None,
        output: String::new(),
    };
    match exec_collect(client, cmd).await {
        Ok((code, out)) => {
            diag.exit_code = Some(code);
            diag.output = head_lines(&out, CLEANUP_DIAG_OUTPUT_LINES);
            if code != 0 {
                return CleanupQuery {
                    items: Vec::new(),
                    warnings: vec![format!(
                        "{}查询失败(退出码 {}): {}",
                        label,
                        code,
                        head_lines(&out, 1)
                    )],
                    diag,
                    full_output: out,
                };
            }
            let (items, warnings) = if json_mode {
                parse_cleanup_ndjson(&out)
            } else {
                (Vec::new(), Vec::new())
            };
            CleanupQuery {
                items,
                warnings,
                diag,
                full_output: out,
            }
        }
        Err(e) => {
            diag.output = format!("(传输层错误) {}", e);
            CleanupQuery {
                items: Vec::new(),
                warnings: vec![format!("{}查询失败: {}", label, e)],
                diag,
                full_output: String::new(),
            }
        }
    }
}

/// 从命令输出的纯文本行里取绝对路径(过滤空行与摘要省略标记;纯函数,便于单测)。
fn abs_path_lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('/') && !l.starts_with("…"))
        .map(String::from)
        .collect()
}

/// 把镜像引用裁成仓库名(去 `:tag` / `@digest`;纯函数,便于单测)。
/// 例:`myapp:latest` → `myapp`;`registry:5000/app` → `registry:5000/app`。
fn image_repo_of(reference: &str) -> String {
    let r = reference.trim();
    if r.is_empty() {
        return String::new();
    }
    // 有 @digest 时以 digest 之前为准
    let base = r.split('@').next().unwrap_or(r);
    // 最后一个 ':' 若在最后一个 '/' 之后才是 tag 分隔符(避免误切 registry:port)
    match (base.rfind(':'), base.rfind('/')) {
        (Some(c), Some(s)) if c < s => base.to_string(),
        (Some(c), None) => base[..c].to_string(),
        (Some(c), Some(_)) => base[..c].to_string(),
        (None, _) => base.to_string(),
    }
}

/// 判断字符串是否为日期标签 `YYYYmmdd-HHMMSS`(纯函数,便于单测)。
fn is_date_tag(tag: &str) -> bool {
    let t = tag.trim();
    if t.len() != 15 {
        return false;
    }
    let b = t.as_bytes();
    for (i, ch) in b.iter().enumerate() {
        if i == 8 {
            if *ch != b'-' {
                return false;
            }
        } else if !ch.is_ascii_digit() {
            return false;
        }
    }
    true
}

/// 从 compose 文本提取全部 `services.*.image` 的仓库名(纯函数,便于单测)。
/// 解析失败或无 image 字段 → 空 Vec(调用方据此跳过该项目的标签清理,不误删)。
fn compose_image_repos(yaml_text: &str) -> Vec<String> {
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(yaml_text) else {
        return Vec::new();
    };
    let Some(services) = doc.get("services").and_then(|s| s.as_mapping()) else {
        return Vec::new();
    };
    let mut repos: Vec<String> = Vec::new();
    for (_name, svc) in services {
        if let Some(image) = svc.get("image").and_then(|i| i.as_str()) {
            let repo = image_repo_of(image);
            if !repo.is_empty() && !repos.contains(&repo) {
                repos.push(repo);
            }
        }
    }
    repos
}

// ===== 分项目扫描拼命令(纯函数,便于单测)=====

/// 扫描含 compose 文件的目录:深度 ≤ [`CLEANUP_SCAN_MAX_DEPTH`],排除
/// `releases/` 归档与 `.git`(归档里的 compose 副本不是项目)。
pub fn cleanup_scan_compose_cmd(root: &str) -> String {
    format!(
        "find {} -maxdepth {} -type f \\( -name 'docker-compose.yml' -o -name 'docker-compose.yaml' -o -name 'compose.yml' -o -name 'compose.yaml' \\) ! -path '*/releases/*' ! -path '*/.git/*' 2>/dev/null",
        shell_single_quote(root),
        CLEANUP_SCAN_MAX_DEPTH
    )
}

/// 扫描发布归档目录(`<项目>/releases/<YYYYmmdd-HHMMSS>`)的完整路径。
pub fn cleanup_scan_releases_cmd(root: &str) -> String {
    format!(
        "find {} -maxdepth {} -type d -path '*/releases/*' -name '20*-*' 2>/dev/null",
        shell_single_quote(root),
        CLEANUP_SCAN_MAX_DEPTH + 2
    )
}

/// 拼 `du -sh <dir>...`(一次调用取多个目录占用;取不到的目录由 du 自行跳过)。
pub fn cleanup_du_cmd(dirs: &[String]) -> String {
    let quoted: Vec<String> = dirs.iter().map(|d| shell_single_quote(d)).collect();
    format!("du -sh {} 2>/dev/null", quoted.join(" "))
}

/// 拼「逐个 cat compose(带路径标记行)」命令:一次往返取回多份 compose 内容,
/// 标记行形如 `==COMPOSE:<path>`,便于按项目切分。
pub fn cleanup_cat_composes_cmd(files: &[String]) -> String {
    let mut out = String::new();
    for f in files {
        // 标记行与内容都经 printf/cat 输出;路径单引号包裹防注入
        out.push_str(&format!(
            "printf '==COMPOSE:%s\\n' {}; cat {} 2>/dev/null; printf '\\n'; ",
            shell_single_quote(f),
            shell_single_quote(f)
        ));
    }
    out
}

/// 解析 `==COMPOSE:<path>` 标记切分的 compose 内容(纯函数,便于单测)。
/// 返回 `(path, content)` 列表。
fn split_compose_dump(out: &str) -> Vec<(String, String)> {
    let mut result: Vec<(String, String)> = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut buf = String::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("==COMPOSE:") {
            if let Some(p) = cur_path.take() {
                result.push((p, buf.clone()));
            }
            buf.clear();
            cur_path = Some(rest.trim().to_string());
        } else if cur_path.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(p) = cur_path {
        result.push((p, buf));
    }
    result
}

/// 项目目录 = 发布归档路径去末尾两级(`<dir>/releases/<ts>` → `<dir>`)。
/// 纯函数,便于单测。
fn project_dir_of_release(release_path: &str) -> String {
    let p = release_path.trim_end_matches('/');
    let without_ts = match p.rfind('/') {
        Some(i) => &p[..i],
        None => return String::new(),
    };
    match without_ts.rfind('/') {
        Some(i) => without_ts[..i].to_string(),
        None => String::new(),
    }
}

/// 解析 `du -sh` 输出为 `路径 → 占用`(纯函数,便于单测)。
/// GNU du 输出形如 `1.2G\t/home/x/proj`(大小以制表符或空格分隔路径)。
fn parse_du_output(out: &str) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // 以最后一个制表符切分;无制表符时退化为按首个空白切分
        if let Some((size, path)) = t.rsplit_once('\t') {
            rows.push((path.trim().to_string(), size.trim().to_string()));
        } else if let Some((size, path)) = t.split_once(char::is_whitespace) {
            rows.push((path.trim().to_string(), size.trim().to_string()));
        }
    }
    rows
}


/// 清理分析:无标签镜像 / 停止容器 / 未使用卷 / build cache 占用 / 分项目可清理项。
/// 只读查询,不做任何清理;单项失败不影响其余(记入 warnings,诊断逐条可查)。
///
/// 无标签镜像为什么不用 `docker images -f dangling=true`:该过滤在新版
/// Docker(BuildKit / containerd image store)下常返回空,而 `docker images`
/// 明明列得出 `<none>:<none>` 条目 —— 表现为"有悬空镜像但识别不到"。这里改为
/// 全量拉取后在客户端过滤 `Repository == "<none>"`,并用容器引用集合标记在用项。
#[tauri::command]
pub async fn cleanup_preview(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    scan_root: Option<String>,
) -> Result<CleanupReport, String> {
    let _ = app; // 与 prune_server 等命令签名风格一致(结果经返回值而非事件)
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

    let mut report = CleanupReport::default();
    // 扫描起点:显式传入优先,否则用服务器配置的部署目录
    let scan_root = scan_root
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| server.remote_dir.clone());
    report.scan_root = scan_root.clone();

    // 0. 先取容器列表与它们引用的镜像 ID(用于标记在用的无标签/旧标签镜像)
    let containers_q = run_cleanup_query(
        &mut client,
        "容器列表",
        "docker ps -a --no-trunc --format '{{json .}}'",
        true,
    )
    .await;
    report.diagnostics.push(containers_q.diag.clone());
    report.warnings.extend(containers_q.warnings.clone());
    let container_rows = containers_q.items;

    // 容器条目只有镜像名,需 inspect 才拿到镜像 ID(sha256:...)
    let cids: Vec<String> = container_rows
        .iter()
        .map(|v| jstr(v, "ID"))
        .filter(|s| !s.is_empty())
        .collect();
    let mut in_use_image_ids: Vec<String> = Vec::new();
    if !cids.is_empty() {
        let quoted: Vec<String> = cids.iter().map(|c| shell_single_quote(c)).collect();
        let inspect_cmd = format!(
            "docker inspect --format '{{{{.Image}}}}' {} 2>/dev/null",
            quoted.join(" ")
        );
        let inspect_q = run_cleanup_query(&mut client, "容器镜像引用", &inspect_cmd, false).await;
        // 纯文本输出:逐行取 sha256:...(不要求 JSON)
        for line in inspect_q.full_output.lines() {
            let t = line.trim();
            if t.starts_with("sha256:") {
                in_use_image_ids.push(t.to_string());
            }
        }
        report.diagnostics.push(inspect_q.diag);
    }

    // 1. 无标签镜像(<none>:<none> 及 <none> 仓库)—— 全量拉取后客户端过滤
    let images_q = run_cleanup_query(
        &mut client,
        "镜像列表",
        "docker images --no-trunc --format '{{json .}}'",
        true,
    )
    .await;
    report.diagnostics.push(images_q.diag.clone());
    report.warnings.extend(images_q.warnings.clone());
    for v in &images_q.items {
        let repo = jstr(v, "Repository");
        let tag = jstr(v, "Tag");
        if repo != "<none>" && tag != "<none>" {
            continue;
        }
        let id = jstr(v, "ID");
        let in_use = in_use_image_ids.iter().any(|x| x == &id);
        report.dangling_images.push(CleanupImage {
            repository: repo,
            tag,
            size: jstr(v, "Size"),
            in_use,
            id,
        });
    }

    // 2. 停止容器(exited 与 created;已在第 0 步取回,直接复用)
    report.stopped_containers = container_rows
        .iter()
        .filter(|v| {
            let state = jstr(v, "State").to_lowercase();
            let status = jstr(v, "Status").to_lowercase();
            state == "exited"
                || state == "created"
                || status.starts_with("exited")
                || status.starts_with("created")
        })
        .map(|v| CleanupContainer {
            id: jstr(v, "ID"),
            names: jstr(v, "Names"),
            image: jstr(v, "Image"),
            status: jstr(v, "Status"),
        })
        .collect();

    // 3. 未使用卷
    let volumes_q = run_cleanup_query(
        &mut client,
        "卷列表",
        "docker volume ls -f dangling=true --format '{{json .}}'",
        true,
    )
    .await;
    report.diagnostics.push(volumes_q.diag.clone());
    report.warnings.extend(volumes_q.warnings.clone());
    report.unused_volumes = volumes_q
        .items
        .iter()
        .map(|v| CleanupVolume {
            name: jstr(v, "Name"),
        })
        .collect();

    // 4. build cache 占用(docker system df 的 Build Cache 行)
    let df_q = run_cleanup_query(
        &mut client,
        "磁盘占用",
        "docker system df --format '{{json .}}'",
        true,
    )
    .await;
    report.diagnostics.push(df_q.diag.clone());
    report.warnings.extend(df_q.warnings.clone());
    report.build_cache_size = df_q
        .items
        .iter()
        .find(|v| jstr(v, "Type") == "Build Cache")
        .map(|v| jstr(v, "Size"))
        .unwrap_or_else(|| "0B".to_string());

    // 5. 分项目扫描:compose 文件 → 目录 → du 占用 → releases 归档 → 日期标签镜像
    match scan_cleanup_projects(&mut client, &scan_root, &cfg, &in_use_image_ids).await {
        Ok((projects, mut diags, mut warnings)) => {
            report.projects = projects;
            report.diagnostics.append(&mut diags);
            report.warnings.append(&mut warnings);
        }
        Err(e) => report.errors.push(format!("分项目扫描失败: {}", e)),
    }

    Ok(report)
}

/// 扫描服务器上的项目目录,汇总每个项目的占用/归档/旧标签镜像(只读)。
///
/// 步骤:find compose(排除 releases)→ 目录去重 → du -sh 取占用 →
/// 逐目录 cat compose 提取镜像仓库名 → 匹配 releases 归档与日期标签镜像。
/// 注意:纯文本命令的解析必须用 `full_output`(诊断里只有截断摘要)。
async fn scan_cleanup_projects(
    client: &mut SshClient,
    scan_root: &str,
    cfg: &crate::config::AppConfig,
    in_use_image_ids: &[String],
) -> Result<(Vec<CleanupProject>, Vec<CleanupDiag>, Vec<String>), String> {
    let mut diags: Vec<CleanupDiag> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // 5.1 扫 compose 文件
    let compose_q = run_cleanup_query(
        client,
        "项目扫描",
        &cleanup_scan_compose_cmd(scan_root),
        false,
    )
    .await;
    let compose_paths = abs_path_lines(&compose_q.full_output);
    diags.push(compose_q.diag);
    if compose_paths.is_empty() {
        return Ok((Vec::new(), diags, warnings));
    }

    // 项目目录去重(compose 文件所在目录)
    let mut dirs: Vec<String> = Vec::new();
    for p in &compose_paths {
        if let Some((parent, _)) = p.rsplit_once('/') {
            if !parent.is_empty() && !dirs.iter().any(|d| d == parent) {
                dirs.push(parent.to_string());
            }
        }
    }
    dirs.sort();

    // 5.2 du -sh 取占用
    let du_q = run_cleanup_query(client, "项目占用", &cleanup_du_cmd(&dirs), false).await;
    let du_rows = parse_du_output(&du_q.full_output);
    diags.push(du_q.diag);

    // 5.3 逐目录 cat compose(带标记行),提取镜像仓库名
    let cat_q = run_cleanup_query(
        client,
        "compose 内容",
        &cleanup_cat_composes_cmd(&compose_paths),
        false,
    )
    .await;
    let dumped = split_compose_dump(&cat_q.full_output);
    diags.push(cat_q.diag);
    let mut dir_repos: Vec<(String, Vec<String>)> = Vec::new();
    for (path, content) in &dumped {
        let Some((dir, _)) = path.rsplit_once('/') else {
            continue;
        };
        let repos = compose_image_repos(content);
        if let Some(entry) = dir_repos.iter_mut().find(|(d, _)| d == dir) {
            for r in repos {
                if !entry.1.contains(&r) {
                    entry.1.push(r);
                }
            }
        } else {
            dir_repos.push((dir.to_string(), repos));
        }
    }

    // 5.4 扫 releases 归档(一次 find 取全量,再按项目目录归组)
    let rel_q = run_cleanup_query(
        client,
        "发布归档",
        &cleanup_scan_releases_cmd(scan_root),
        false,
    )
    .await;
    let release_paths = abs_path_lines(&rel_q.full_output);
    diags.push(rel_q.diag);

    // 5.5 全量镜像列表(取日期标签镜像;含 ID/大小/创建时间)
    let tag_q = run_cleanup_query(
        client,
        "镜像标签",
        "docker images --format '{{json .}}'",
        true,
    )
    .await;
    let all_images: Vec<serde_json::Value> = tag_q.items.clone();
    warnings.extend(tag_q.warnings.clone());
    diags.push(tag_q.diag);

    // 5.6 组项目视图
    let mut projects: Vec<CleanupProject> = Vec::new();
    for dir in &dirs {
        let compose_file = compose_paths
            .iter()
            .find(|p| p.rsplit_once('/').map(|(d, _)| d) == Some(dir.as_str()))
            .cloned()
            .unwrap_or_default();
        let repos = dir_repos
            .iter()
            .find(|(d, _)| d == dir)
            .map(|(_, r)| r.clone())
            .unwrap_or_default();
        let size = du_rows
            .iter()
            .find(|(p, _)| p == dir)
            .map(|(_, s)| s.clone())
            .unwrap_or_else(|| "?".to_string());

        // 该项目下的归档目录(新→旧,按目录名时间戳倒序)
        let mut releases: Vec<String> = release_paths
            .iter()
            .filter(|r| project_dir_of_release(r) == *dir)
            .cloned()
            .collect();
        releases.sort_by(|a, b| b.cmp(a));

        // 该项目可清理的日期标签镜像(仓库名匹配 + 未被容器引用)
        let tag_images: Vec<CleanupTagImage> = all_images
            .iter()
            .filter_map(|v| {
                let repo = jstr(v, "Repository");
                let tag = jstr(v, "Tag");
                if repo == "<none>" || !is_date_tag(&tag) {
                    return None;
                }
                if !repos.iter().any(|r| r == &repo) {
                    return None;
                }
                let id = jstr(v, "ID");
                Some(CleanupTagImage {
                    reference: format!("{}:{}", repo, tag),
                    in_use: in_use_image_ids.iter().any(|x| x == &id),
                    id,
                    size: jstr(v, "Size"),
                    created: jstr(v, "CreatedAt"),
                })
            })
            .collect();

        // 应用内项目标注:按「项目名 == 目录名」或 compose 文件名匹配
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

        projects.push(CleanupProject {
            dir: dir.clone(),
            compose_file,
            size,
            releases,
            tag_images,
            app_project,
        });
    }

    Ok((projects, diags, warnings))
}


/// 拼装删除无标签镜像的命令(逐 ID `docker rmi`,不使用 prune)。
///
/// 为什么不用 `docker image prune -f`:prune 删除的范围由 docker 自行判定,
/// 与用户在预览里勾选的条目未必一致;逐 ID 删除让执行结果与勾选一一对应。
fn rmi_ids_cmd(ids: &[String]) -> String {
    let quoted: Vec<String> = ids.iter().map(|i| shell_single_quote(i)).collect();
    format!("docker rmi {}", quoted.join(" "))
}

/// 拼装删除停止容器的命令(逐 ID `docker rm`)。
fn rm_container_ids_cmd(ids: &[String]) -> String {
    let quoted: Vec<String> = ids.iter().map(|i| shell_single_quote(i)).collect();
    format!("docker rm {}", quoted.join(" "))
}

/// 拼装删除未使用卷的命令(逐名 `docker volume rm`;卷被占用时该条失败不影响其余)。
fn rm_volume_names_cmd(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| shell_single_quote(n)).collect();
    format!("docker volume rm {}", quoted.join(" "))
}

/// 拼装删除发布归档目录的命令(逐目录 `rm -rf`;路径由后端拼自服务器扫描结果)。
fn rm_release_dirs_cmd(dirs: &[String]) -> String {
    let quoted: Vec<String> = dirs.iter().map(|d| shell_single_quote(d)).collect();
    format!("rm -rf {}", quoted.join(" "))
}

/// 定向执行勾选的清理项(逐节流式输出 server-log,与既有清理同通道)。
/// 至少勾选一项;各节独立执行,单节失败不影响其余。
///
/// 每节都按前端回传的**显式目标列表**逐条删除(见 [`CleanupSections`] 注释);
/// 分项目清理复用同一命令,避免两套实现。
#[tauri::command]
pub async fn cleanup_execute(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    sections: CleanupSections,
) -> Result<Vec<CleanupSectionResult>, String> {
    if !sections.has_any() {
        return Err("请至少勾选一项要清理的内容".to_string());
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

    let mut plan: Vec<(String, String)> = Vec::new();
    if sections.images && !sections.image_ids.is_empty() {
        plan.push((
            format!("无标签镜像({} 项)", sections.image_ids.len()),
            rmi_ids_cmd(&sections.image_ids),
        ));
    }
    if sections.containers && !sections.container_ids.is_empty() {
        plan.push((
            format!("停止容器({} 个)", sections.container_ids.len()),
            rm_container_ids_cmd(&sections.container_ids),
        ));
    }
    if sections.volumes && !sections.volume_names.is_empty() {
        plan.push((
            format!("未使用卷({} 个)", sections.volume_names.len()),
            rm_volume_names_cmd(&sections.volume_names),
        ));
    }
    if sections.builder {
        plan.push(("构建缓存".to_string(), "docker builder prune -f".to_string()));
    }
    // 分项目:归档删除与标签删除各聚合成一条命令(减少往返),逐项结果由输出体现
    let all_release_dirs: Vec<String> = sections
        .projects
        .iter()
        .flat_map(|p| p.release_dirs.iter().cloned())
        .collect();
    if !all_release_dirs.is_empty() {
        plan.push((
            format!("旧发布归档({} 个)", all_release_dirs.len()),
            rm_release_dirs_cmd(&all_release_dirs),
        ));
    }
    let all_image_refs: Vec<String> = sections
        .projects
        .iter()
        .flat_map(|p| p.image_refs.iter().cloned())
        .collect();
    if !all_image_refs.is_empty() {
        plan.push((
            format!("旧版本镜像({} 个)", all_image_refs.len()),
            rmi_ids_cmd(&all_image_refs),
        ));
    }

    let mut results = Vec::new();
    for (label, cmd) in plan {
        let mut lines: Vec<String> = Vec::new();
        // 借用作用域:on_output 持有 &mut client,出块后释放,便于循环下一节继续用
        let outcome: Result<i32, String> = {
            let mut on_output = |line: &str| {
                let t = line.trim_end();
                let _ = app.emit("server-log", t.to_string());
                lines.push(t.to_string());
            };
            let fut = client.exec(&cmd, &mut on_output);
            with_timeout(
                PRUNE_TIMEOUT_SECS,
                "服务器清理超时",
                "请检查服务器网络后重试",
                async { fut.await.map_err(|e| format!("执行清理命令失败: {}", e)) },
            )
            .await
        };
        // 单节传输层失败只记为该节失败,不中断后续节(与"各节独立"语义一致)
        match outcome {
            Ok(code) => {
                if code != 0 {
                    let _ = app.emit(
                        "server-log",
                        format!("[{}] 清理失败(退出码 {})", label, code),
                    );
                }
                results.push(CleanupSectionResult {
                    label,
                    ok: code == 0,
                    output: lines.join("\n"),
                });
            }
            Err(e) => {
                let _ = app.emit("server-log", format!("[{}] {}", label, e));
                results.push(CleanupSectionResult {
                    label,
                    ok: false,
                    output: format!("{}\n{}", lines.join("\n"), e),
                });
            }
        }
    }
    Ok(results)
}

// ===== 跨服务器镜像迁移(阶段十:源 save → 下载本地 → 上传目标 → load)=====

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
struct MigrateStateInner {
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
}

impl MigrateStateInner {
    fn is_cancelled(&self) -> bool {
        self.cancelled
            .load(std::sync::atomic::Ordering::SeqCst)
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
        let result =
            run_migrate(&app, &state, migrate_id, &req, images).await;
        let (success, message, images_done) = match result {
            Ok((msg, done)) => (true, msg, done),
            Err((msg, done)) => (false, msg, done),
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
        let (code, out) = exec_collect(&mut src, &docker_inspect_cmd(image))
            .await
            .map_err(|e| (format!("源服务器查询镜像 {} 失败: {}", image, e), done.clone()))?;
        let src_id = if code == 0 {
            out.trim().strip_prefix("sha256:").map(str::to_string)
        } else {
            None
        };
        if src_id.is_none() {
            let msg = format!(
                "源服务器上不存在镜像 {}(或无权访问),跳过",
                image
            );
            emit_line(&format!("警告:{}", msg));
            continue;
        }

        // 目标同 ID 自动跳过(镜像已在目标侧,零传输)
        let (code, out) = exec_collect(&mut dst, &docker_inspect_cmd(image))
            .await
            .map_err(|e| (format!("目标服务器查询镜像 {} 失败: {}", image, e), done.clone()))?;
        if code == 0 {
            if let Some(dst_id) = out.trim().strip_prefix("sha256:") {
                if same_image_id(&src_id.clone().unwrap_or_default(), dst_id) {
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
async fn save_gzip_remote(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, TransferMode};

    // ===== 清理分析纯函数(第三批)=====

    #[test]
    fn test_parse_cleanup_ndjson_skips_non_json() {
        // stderr 混入(如 "WARNING: No swap limit support")不应让整节失败;
        // 以 `{` 开头但结构损坏的行单独报"解析失败"
        let out = "WARNING: No swap limit support\n{\"ID\":\"abc\",\"Repository\":\"x\"}\n\n{ broken json\n";
        let (items, warnings) = parse_cleanup_ndjson(out);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["ID"], "abc");
        // 提示行进 warnings;损坏的 JSON 行报解析失败
        assert!(warnings.iter().any(|w| w.contains("No swap limit")));
        assert!(warnings.iter().any(|w| w.contains("解析失败")));
    }

    #[test]
    fn test_parse_cleanup_ndjson_warning_cap() {
        // 提示行最多收集 3 条,防刷屏
        let out = "a\nb\nc\nd\ne\n";
        let (items, warnings) = parse_cleanup_ndjson(out);
        assert!(items.is_empty());
        assert_eq!(warnings.len(), 3);
    }

    #[test]
    fn test_image_repo_of() {
        assert_eq!(image_repo_of("myapp:latest"), "myapp");
        assert_eq!(image_repo_of("myapp"), "myapp");
        // registry:port/name 里的冒号不是 tag 分隔符
        assert_eq!(image_repo_of("registry:5000/app"), "registry:5000/app");
        assert_eq!(image_repo_of("registry:5000/app:v1"), "registry:5000/app");
        assert_eq!(image_repo_of("app@sha256:deadbeef"), "app");
        assert_eq!(image_repo_of(""), "");
    }

    #[test]
    fn test_is_date_tag() {
        assert!(is_date_tag("20260905-101010"));
        assert!(!is_date_tag("latest"));
        assert!(!is_date_tag("20260905"));
        assert!(!is_date_tag("2026090-1010100"));
        assert!(!is_date_tag("2026090x-101010"));
    }

    #[test]
    fn test_compose_image_repos() {
        let yaml = "\
services:
  web:
    image: myapp:latest
  db:
    image: registry:5000/postgres:16
  worker:
    image: myapp:latest
";
        let repos = compose_image_repos(yaml);
        assert!(repos.contains(&"myapp".to_string()));
        assert!(repos.contains(&"registry:5000/postgres".to_string()));
        // 去重:myapp 只出现一次
        assert_eq!(repos.iter().filter(|r| *r == "myapp").count(), 1);
        // 解析失败 → 空(调用方据此跳过标签清理,不误删)
        assert!(compose_image_repos("}{ not yaml").is_empty());
        assert!(compose_image_repos("services: {}").is_empty());
    }

    #[test]
    fn test_split_compose_dump() {
        let out = "==COMPOSE:/home/a/docker-compose.yml\nservices:\n  web:\n    image: x\n\n==COMPOSE:/home/b/docker-compose.yml\nservices:\n  db:\n    image: y\n\n";
        let parts = split_compose_dump(out);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, "/home/a/docker-compose.yml");
        assert!(parts[0].1.contains("image: x"));
        assert_eq!(parts[1].0, "/home/b/docker-compose.yml");
        assert!(parts[1].1.contains("image: y"));
    }

    #[test]
    fn test_project_dir_of_release() {
        assert_eq!(
            project_dir_of_release("/home/henghao/zetok/releases/20260905-101010"),
            "/home/henghao/zetok"
        );
        assert_eq!(
            project_dir_of_release("/home/henghao/zetok/releases/20260905-101010/"),
            "/home/henghao/zetok"
        );
        assert_eq!(project_dir_of_release("/tmp/x"), "");
    }

    #[test]
    fn test_parse_du_output() {
        // GNU du:大小与路径以制表符分隔
        let rows = parse_du_output("1.2G\t/home/a/proj\n340M\t/home/a/other\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("/home/a/proj".to_string(), "1.2G".to_string()));
        assert_eq!(rows[1], ("/home/a/other".to_string(), "340M".to_string()));
        // 退化为空格分隔
        let rows2 = parse_du_output("12K /home/a/b");
        assert_eq!(rows2[0], ("/home/a/b".to_string(), "12K".to_string()));
        assert!(parse_du_output("").is_empty());
    }

    #[test]
    fn test_abs_path_lines() {
        let out = "/home/a/docker-compose.yml\nrunning\n… (共 12 行)\n/home/b/compose.yml\n";
        let lines = abs_path_lines(out);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "/home/a/docker-compose.yml");
    }

    #[test]
    fn test_cleanup_cmd_builders_quote() {
        // 含空格与单引号的路径必须被安全包裹
        let dirs = vec!["/home/a b/releases/20260905-101010".to_string()];
        assert_eq!(
            rm_release_dirs_cmd(&dirs),
            "rm -rf '/home/a b/releases/20260905-101010'"
        );
        let ids = vec!["sha256:abc".to_string()];
        assert_eq!(rmi_ids_cmd(&ids), "docker rmi 'sha256:abc'");
        assert_eq!(
            rm_volume_names_cmd(&["vol1".to_string()]),
            "docker volume rm 'vol1'"
        );
    }

    #[test]
    fn test_cleanup_scan_compose_cmd_excludes_archives() {
        let cmd = cleanup_scan_compose_cmd("/home/henghao");
        assert!(cmd.contains("-maxdepth 4"));
        assert!(cmd.contains("'*/releases/*'"));
        assert!(cmd.contains("'*/.git/*'"));
        assert!(cmd.contains("/home/henghao"));
    }

    #[test]
    fn test_cleanup_sections_has_any() {
        let empty = CleanupSections {
            images: true,
            containers: false,
            volumes: false,
            builder: false,
            image_ids: vec![],
            container_ids: vec![],
            volume_names: vec![],
            projects: vec![],
        };
        // 勾了节但没有任何目标 → 不可执行(防"勾了却没东西可删"的误报成功)
        assert!(!empty.has_any());
        let with_target = CleanupSections {
            images: true,
            containers: false,
            volumes: false,
            builder: false,
            image_ids: vec!["sha256:x".into()],
            container_ids: vec![],
            volume_names: vec![],
            projects: vec![],
        };
        assert!(with_target.has_any());
        let builder_only = CleanupSections {
            images: false,
            containers: false,
            volumes: false,
            builder: true,
            image_ids: vec![],
            container_ids: vec![],
            volume_names: vec![],
            projects: vec![],
        };
        assert!(builder_only.has_any());
    }

    #[test]
    fn test_local_basename() {
        // Windows 与 POSIX 分隔符都支持,尾部斜杠去掉
        assert_eq!(local_basename("E:\\apps\\web"), Some("web".to_string()));
        assert_eq!(local_basename("/opt/data/"), Some("data".to_string()));
        assert_eq!(local_basename("C:/x/y/z.txt"), Some("z.txt".to_string()));
        assert_eq!(local_basename("web"), Some("web".to_string()));
        // 无法取名的输入 → None(调用方据此保持原行为)
        assert_eq!(local_basename(""), None);
        assert_eq!(local_basename("   "), None);
        assert_eq!(local_basename("/"), None);
    }

    #[test]
    fn test_source_content_hash_detects_change() {
        let dir = std::env::temp_dir().join(format!("dd-hash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let compose = dir.join("docker-compose.yml");
        std::fs::write(&compose, "services:\n  web:\n    image: a:1\n").unwrap();
        let h1 = source_content_hash(&compose).unwrap();
        // 同内容 → 同哈希(可重复比对)
        assert_eq!(h1, source_content_hash(&compose).unwrap());

        // 改 compose → 哈希变化
        std::fs::write(&compose, "services:\n  web:\n    image: a:2\n").unwrap();
        let h2 = source_content_hash(&compose).unwrap();
        assert_ne!(h1, h2);

        // 改 .env(compose 未动)→ 也要变化(插值结果会变)
        std::fs::write(&compose, "services:\n  web:\n    image: a:1\n").unwrap();
        std::fs::write(dir.join(".env"), "TAG=2\n").unwrap();
        let h3 = source_content_hash(&compose).unwrap();
        assert_ne!(h1, h3);

        // 源不存在 → Err(调用方按"无法比对"降级,不误判为已变更)
        assert!(source_content_hash(&dir.join("nope.yml")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_project_source_status_states() {
        let mk = |path: Option<&str>, hash: Option<&str>| ProjectConfig {
            id: "p".into(),
            name: "n".into(),
            image_filter: String::new(),
            compose_file: "c".into(),
            file_mappings: Vec::new(),
            service_overrides: Vec::new(),
            health_wait_secs: 0,
            pre_deploy_cmd: None,
            post_deploy_cmd: None,
            notify_webhook: None,
            source_compose_path: path.map(String::from),
            source_hash: hash.map(String::from),
        };
        // 手工项目(无源)→ unknown,不参与自动更新
        assert_eq!(project_source_status(&mk(None, None)).state, "unknown");
        // 源不存在 → missing
        let missing = mk(Some("E:/definitely/not/here/docker-compose.yml"), Some("x"));
        assert_eq!(project_source_status(&missing).state, "missing");
        // 旧配置无哈希 → unknown(要求先手动更新一次)
        let dir = std::env::temp_dir().join(format!("dd-src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let compose = dir.join("docker-compose.yml");
        std::fs::write(&compose, "services: {}\n").unwrap();
        let no_hash = mk(Some(&compose.to_string_lossy()), None);
        assert_eq!(project_source_status(&no_hash).state, "unknown");
        // 哈希一致 → unchanged;不一致 → changed
        let h = source_content_hash(&compose).unwrap();
        let same = mk(Some(&compose.to_string_lossy()), Some(&h));
        assert_eq!(project_source_status(&same).state, "unchanged");
        let diff = mk(Some(&compose.to_string_lossy()), Some("deadbeef"));
        assert_eq!(project_source_status(&diff).state, "changed");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== deploy_notify_text(通知中心挂点文案)=====

    #[test]
    fn test_deploy_notify_text_success() {
        let record = DeployRecord {
            ts: "2026-09-05 12:00:00".into(),
            mode: MODE_SINGLE.into(),
            server_name: "生产".into(),
            project_name: "博客".into(),
            images: vec![],
            success: true,
            message: "部署完成".into(),
            duration_secs: 42,
            release_dir: None,
        };
        let (title, body) = deploy_notify_text(true, "部署完成", &Some(record));
        assert_eq!(title, "部署成功");
        assert!(body.contains("博客"));
        assert!(body.contains("生产"));
        assert!(body.contains("部署完成"));
        assert!(body.contains("42"));
    }

    #[test]
    fn test_deploy_notify_text_failure_and_cancel() {
        let record = DeployRecord {
            ts: String::new(),
            mode: MODE_STACK.into(),
            server_name: "s".into(),
            project_name: "p".into(),
            images: vec![],
            success: false,
            message: CANCELLED_MSG.into(),
            duration_secs: 3,
            release_dir: None,
        };
        // 取消(固定文案)→ cancel 标题
        let (title, body) = deploy_notify_text(false, CANCELLED_MSG, &Some(record));
        assert_eq!(title, "部署已取消");
        assert!(body.contains(CANCELLED_MSG));
        // 普通失败 → failure 标题
        let (title, _) = deploy_notify_text(false, "健康检查未通过", &None);
        assert_eq!(title, "部署失败");
        // panic 路径(无记录)→ 兜底正文
        let (_, body) = deploy_notify_text(false, "部署过程发生内部错误", &None);
        assert!(body.contains("部署详情缺失"));
    }

    // ===== hook_failure_result(钩子失败映射)=====

    #[test]
    fn test_hook_failure_result_cancel_passthrough() {
        // 取消错误原样透传(Pre/Post 同口径):包装成其他文案会让
        // spawn_deploy_task 的 cancel 判定失配,把取消误报为部署失败
        assert_eq!(
            hook_failure_result(HookKind::Pre, CANCELLED_MSG),
            Some(CANCELLED_MSG.to_string())
        );
        assert_eq!(
            hook_failure_result(HookKind::Post, CANCELLED_MSG),
            Some(CANCELLED_MSG.to_string())
        );
    }

    #[test]
    fn test_hook_failure_result_pre_wraps_post_swallows() {
        // Pre 普通失败 → 「执行失败,部署中止」;Post 普通失败 → None(仅告警)
        assert_eq!(
            hook_failure_result(HookKind::Pre, "exit code 1"),
            Some("部署前钩子执行失败,部署中止: exit code 1".to_string())
        );
        assert_eq!(hook_failure_result(HookKind::Post, "exit code 1"), None);
    }

    // ===== rollback_notify_text(回滚通知文案)=====

    #[test]
    fn test_rollback_notify_text_success() {
        let record = DeployRecord {
            ts: "2026-09-05 12:00:00".into(),
            mode: MODE_ROLLBACK.into(),
            server_name: "生产".into(),
            project_name: "博客".into(),
            images: vec![],
            success: true,
            message: "回滚到 20260905120000".into(),
            duration_secs: 7,
            release_dir: None,
        };
        let message = record.message.clone();
        let (title, body) = rollback_notify_text(true, &message, &Some(record));
        assert_eq!(title, "回滚成功");
        assert!(body.contains("博客"));
        // 目标 release ts 随「回滚到 …」消息进入正文
        assert!(body.contains("20260905120000"));
        assert!(body.contains("7"));
    }

    #[test]
    fn test_rollback_notify_text_failure_and_cancel() {
        // 取消(固定文案)→ cancel 标题;普通失败 → failure 标题
        let (title, _) = rollback_notify_text(false, CANCELLED_MSG, &None);
        assert_eq!(title, "回滚已取消");
        let (title, body) = rollback_notify_text(false, "回滚目标发布目录不存在", &None);
        assert_eq!(title, "回滚失败");
        // 无记录(panic/早期失败)→ 兜底正文
        assert!(body.contains("回滚详情缺失"));
    }

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

    // ===== 单镜像 compose 路径 =====

    #[test]
    fn test_is_windows_absolute_path() {
        assert!(is_windows_absolute_path(
            r"E:\github\Docker-Deploy-SSH\docker-compose.yml"
        ));
        assert!(is_windows_absolute_path(
            "E:/github/Docker-Deploy-SSH/docker-compose.yml"
        ));
        assert!(is_windows_absolute_path(r"\\server\share\docker-compose.yml"));
        assert!(!is_windows_absolute_path("docker-compose.yml"));
        assert!(!is_windows_absolute_path("/opt/app/docker-compose.yml"));
    }

    #[test]
    fn test_single_image_command_uses_remote_compose_path() {
        let local = r"E:\github\Docker-Deploy-SSH\config\stacks\id\docker-compose.yml";
        let remote = remote_compose_path("/home/henghao");
        let cmd = compose_up_cmd("/home/henghao", &remote, &[]);
        assert!(!cmd.contains(local));
        assert_eq!(
            cmd,
            "cd '/home/henghao' && docker compose -f '/home/henghao/docker-compose.yml' up -d"
        );
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

    // ===== resolve_key_passphrase(阶段三:加密私钥口令)=====

    /// 构造仅含 auth 的最小 ServerConfig(供 resolve_key_passphrase 测试)。
    #[allow(dead_code)]
    fn key_pass_cfg(key_pass_enc: Option<String>) -> ServerConfig {
        ServerConfig {
            id: "s1".into(),
            name: "n".into(),
            host: "1.2.3.4".into(),
            port: 22,
            username: "root".into(),
            auth: AuthConfig {
                auth_type: AuthType::Key,
                key_path: Some("C:/k".into()),
                password_enc: None,
                key_pass_enc,
            },
            remote_dir: "/opt/app".into(),
            host_key_sha256: None,
        }
    }

    #[test]
    fn test_resolve_key_passphrase_none_when_absent() {
        // 未配置 key_pass_enc(旧版配置/未加密私钥)→ None,不报错
        assert_eq!(resolve_key_passphrase(&key_pass_cfg(None)).unwrap(), None);
        // 空串按未配置处理
        assert_eq!(resolve_key_passphrase(&key_pass_cfg(Some(String::new()))).unwrap(), None);
    }

    #[cfg(windows)]
    #[test]
    fn test_resolve_key_passphrase_dpapi_roundtrip() {
        let enc = dpapi_protect("key-pass-123").unwrap();
        assert_eq!(
            resolve_key_passphrase(&key_pass_cfg(Some(enc))).unwrap(),
            Some("key-pass-123".to_string())
        );
    }

    #[test]
    fn test_resolve_key_passphrase_bad_enc() {
        // 密文无效(base64 非法)→ 报错
        assert!(resolve_key_passphrase(&key_pass_cfg(Some("不是base64!!".into()))).is_err());
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
        // origin.json 记录导入来源的原始父目录名(供副本路径解析默认镜像名兜底)
        let origin: crate::stack::StackOrigin = serde_json::from_str(
            &std::fs::read_to_string(copy.parent().unwrap().join("origin.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(origin.dir_name, "src", "origin.json 应记录源的父目录名");

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
            compose_ps_json_cmd("/opt/app", "/opt/app/docker-compose.yml", &[]),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' ps --all --format json"
        );
        // override 与 pull/up 同序追加 -f:override-only 服务也进入健康判定
        assert_eq!(
            compose_ps_json_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["compose.override.yaml".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'compose.override.yaml' ps --all --format json"
        );
    }

    #[test]
    fn test_compose_ps_json_cmd_escapes_quote() {
        // 路径内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            compose_ps_json_cmd("/opt/app", "/op't.yml", &[]),
            "cd '/opt/app' && docker compose -f '/op'\\''t.yml' ps --all --format json"
        );
    }

    #[test]
    fn test_compose_logs_cmd() {
        assert_eq!(
            compose_logs_cmd("/opt/app", "./docker-compose.yml", &[]),
            "cd '/opt/app' && docker compose -f './docker-compose.yml' logs --tail 50"
        );
        assert_eq!(
            compose_logs_cmd(
                "/opt/app",
                "./docker-compose.yml",
                &["compose.override.yml".to_string()]
            ),
            "cd '/opt/app' && docker compose -f './docker-compose.yml' -f 'compose.override.yml' logs --tail 50"
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

    // ===== Task 6:classify_change 部署变更分类(六例)=====

    #[test]
    fn test_classify_change_pull_mode_always_pull() {
        // 例 1:Pull 类一律 "Pull",不看本地/远端状态
        assert_eq!(
            classify_change(
                &TransferMode::Pull,
                true,
                Some("sha256:a"),
                Some("sha256:a"),
                Some("old:1")
            ),
            "Pull"
        );
        assert_eq!(
            classify_change(&TransferMode::Pull, false, None, None, None),
            "Pull"
        );
    }

    #[test]
    fn test_classify_change_local_image_missing_absent() {
        // 例 2:Local 且本地不存在该 repo:tag → "Absent"
        assert_eq!(
            classify_change(&TransferMode::Local, false, Some("sha256:a"), None, None),
            "Absent"
        );
        assert_eq!(
            classify_change(&TransferMode::Local, false, None, None, Some("old:1")),
            "Absent"
        );
    }

    #[test]
    fn test_classify_change_remote_missing_create() {
        // 例 3:远端无该镜像、也无现存容器 → 全新创建
        assert_eq!(
            classify_change(&TransferMode::Local, true, None, Some("sha256:abc"), None),
            "Create"
        );
    }

    #[test]
    fn test_classify_change_remote_missing_with_container_recreate() {
        // 例 4:远端无该镜像但有现存容器(旧版在跑)→ 重建
        assert_eq!(
            classify_change(
                &TransferMode::Local,
                true,
                None,
                Some("sha256:abc"),
                Some("myapp:old")
            ),
            "Recreate"
        );
    }

    #[test]
    fn test_classify_change_same_id_unchanged() {
        // 例 5:远端镜像 ID 与本地一致(容忍 sha256: 前缀与大小写差异)→ 不变
        assert_eq!(
            classify_change(
                &TransferMode::Local,
                true,
                Some("sha256:ABC123"),
                Some("abc123"),
                Some("myapp:1")
            ),
            "Unchanged"
        );
    }

    #[test]
    fn test_classify_change_different_id_recreate() {
        // 例 6:ID 不同 → 镜像已更新,重建
        assert_eq!(
            classify_change(
                &TransferMode::Local,
                true,
                Some("sha256:aaa"),
                Some("sha256:bbb"),
                Some("myapp:1")
            ),
            "Recreate"
        );
    }

    // ===== 智能传输:same_image_id 完整 ID 口径 =====

    #[test]
    fn test_same_image_id_full_64_hex_equal() {
        // 远端 --no-trunc 输出(sha256: 前缀 + 完整 64 位)vs 本地
        // image_id_by_ref 输出(剥前缀后的完整 64 位)→ 相等(跳过判定的主口径)
        let full = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(same_image_id(
            &format!("sha256:{}", full),
            full
        ));
        assert!(same_image_id(
            &format!("sha256:{}", full),
            &format!("sha256:{}", full)
        ));
    }

    #[test]
    fn test_same_image_id_case_and_prefix_mixed() {
        // 大小写差异、sha256: 前缀有无混合,均视为相等
        assert!(same_image_id("sha256:ABCDEF123456", "abcdef123456"));
        assert!(same_image_id("ABCDEF123456", "sha256:abcdef123456"));
        assert!(same_image_id("  sha256:abc123  ", "ABC123"));
    }

    #[test]
    fn test_same_image_id_truncated_vs_full_not_equal() {
        // 回归:12 位截断 ID(旧 REMOTE_IMAGES_CMD 口径)与完整 64 位 ID
        // 不相等 —— 跳过判定数据源必须用 REMOTE_IMAGES_CMD_FULL
        let full = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(!same_image_id("0123456789abcdef", full));
    }

    #[test]
    fn test_same_image_id_empty_not_equal() {
        // 任一为空视为不等(避免空串误判"未变化")
        assert!(!same_image_id("", "abc"));
        assert!(!same_image_id("sha256:", "abc"));
        assert!(!same_image_id("", ""));
    }

    // ===== Task 6:webhook 载荷序列化 =====

    #[test]
    fn test_webhook_payload_fields() {
        let record = DeployRecord {
            ts: "2026-08-29 10:00:00".into(),
            mode: MODE_STACK.into(),
            server_name: "生产服务器".into(),
            project_name: "博客".into(),
            images: vec!["web:1".into()],
            success: true,
            message: "部署完成".into(),
            duration_secs: 42,
            release_dir: None,
        };
        let v: serde_json::Value = serde_json::from_str(&webhook_payload(&record)).unwrap();
        assert_eq!(v["event"], "deploy");
        assert_eq!(v["success"], true);
        assert_eq!(v["message"], "部署完成");
        assert_eq!(v["server"], "生产服务器");
        assert_eq!(v["project"], "博客");
        assert_eq!(v["duration_secs"], 42);
        assert_eq!(v["ts"], "2026-08-29 10:00:00");

        // 失败记录同样携带完整字段(success=false)
        let mut failed = record.clone();
        failed.success = false;
        failed.message = "部署失败:连接超时".into();
        let v: serde_json::Value = serde_json::from_str(&webhook_payload(&failed)).unwrap();
        assert_eq!(v["success"], false);
        assert_eq!(v["message"], "部署失败:连接超时");
        assert_eq!(v["event"], "deploy");
    }

    // ===== Task 6:远端容器查询命令与解析 =====

    #[test]
    fn test_remote_dir_basename() {
        assert_eq!(remote_dir_basename("/opt/app"), "app");
        assert_eq!(remote_dir_basename("/opt/app/"), "app");
        assert_eq!(remote_dir_basename("app"), "app");
        assert_eq!(remote_dir_basename("/"), "");
        assert_eq!(remote_dir_basename(""), "");
    }

    #[test]
    fn test_compose_containers_cmd() {
        assert_eq!(
            compose_containers_cmd("/opt/app"),
            "docker ps -a --filter 'label=com.docker.compose.project=app' --format '{{json .}}'"
        );
        // 项目名(基名)内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            compose_containers_cmd("/op't"),
            "docker ps -a --filter 'label=com.docker.compose.project=op'\\''t' --format '{{json .}}'"
        );
    }

    #[test]
    fn test_parse_container_line() {
        let line = r#"{"Command":"nginx","CreatedAt":"2026-08-30 10:00:00 +0800 CST","ID":"abc123","Image":"myapp:1","Labels":"com.docker.compose.project=demo,com.docker.compose.service=web","Names":"demo-web-1","State":"running"}"#;
        assert_eq!(
            parse_container_line(line),
            Some(("web".to_string(), "myapp:1".to_string()))
        );
        // 无 compose 服务标签(手动 docker run 的容器)→ None
        assert_eq!(parse_container_line(r#"{"Image":"x","Labels":"foo=bar"}"#), None);
        // 非 JSON 行 → None
        assert_eq!(parse_container_line("oops"), None);
    }

    // ===== 修复轮:webhook URL 日志脱敏 =====

    #[test]
    fn test_url_host_for_log_sanitizes_token() {
        // query 内嵌 token(飞书/钉钉机器人地址形态):只留 host
        assert_eq!(
            url_host_for_log("https://open.feishu.cn/open-apis/bot/v2/hook?token=secret123"),
            "open.feishu.cn"
        );
        // userinfo 内嵌凭据:取 @ 之后的 host[:port]
        assert_eq!(
            url_host_for_log("https://user:pass@hook.example.com:8443/x"),
            "hook.example.com:8443"
        );
        // 无 path/query
        assert_eq!(url_host_for_log("http://example.com"), "example.com");
        // 解析不出 host(非 URL / 空 authority)→ 占位符
        assert_eq!(url_host_for_log("not-a-url"), "<unparseable-url>");
        assert_eq!(url_host_for_log("https://"), "<unparseable-url>");
        assert_eq!(url_host_for_log(""), "<unparseable-url>");
    }

    #[test]
    fn test_webhook_error_detail_no_url() {
        // Status 错误:只落状态码,不内嵌 URL(ureq 的 Display 会带响应 URL)
        let resp = ureq::Response::new(404, "Not Found", "body").unwrap();
        let detail = webhook_error_detail(&ureq::Error::Status(404, resp));
        assert_eq!(detail, "HTTP 状态码 404");
        assert!(!detail.contains("http"), "不应包含 URL: {}", detail);
    }

    // ===== 阶段六:部署断点续传(纯函数单测)=====

    /// 构造单镜像断点产物(测试便捷函数)。
    fn single_art() -> SingleResumeArtifacts {
        SingleResumeArtifacts {
            origin_ref: "myapp:latest".into(),
            repository: "myapp".into(),
            use_date_tag: true,
            image_ref: Some("myapp:20260906-101010".into()),
            tar_local: Some("C:\\Temp\\abc.tar.gz".into()),
            tar_name: Some("abc.tar.gz".into()),
            skip_unchanged: false,
        }
    }

    /// 构造整栈断点产物(测试便捷函数)。
    fn stack_art() -> StackResumeArtifacts {
        StackResumeArtifacts {
            services: vec![
                choice("web", "myapp:1", TransferMode::Local),
                choice("db", "", TransferMode::Pull),
            ],
            unchanged: vec![false],
            skip_unchanged: true,
            force_archive: false,
            release_ts: Some("20260906-101010".into()),
            files: vec!["a.tar.gz".into()],
            locals: vec!["C:\\Temp\\a.tar.gz".into()],
            images: vec!["myapp:1".into()],
        }
    }

    #[test]
    fn test_resume_step_label_single_and_stack() {
        // 单镜像 1..5:打标签/导出压缩/上传镜像/同步文件/服务器部署
        assert_eq!(resume_step_label(MODE_SINGLE, 1), "打标签");
        assert_eq!(resume_step_label(MODE_SINGLE, 2), "导出压缩");
        assert_eq!(resume_step_label(MODE_SINGLE, 3), "上传镜像");
        assert_eq!(resume_step_label(MODE_SINGLE, 4), "同步文件");
        assert_eq!(resume_step_label(MODE_SINGLE, 5), "服务器部署");
        // 整栈 1..6:分类确认/打包/上传/装载/拉取/启动
        assert_eq!(resume_step_label(MODE_STACK, 1), "分类确认");
        assert_eq!(resume_step_label(MODE_STACK, 2), "打包");
        assert_eq!(resume_step_label(MODE_STACK, 3), "上传");
        assert_eq!(resume_step_label(MODE_STACK, 4), "装载");
        assert_eq!(resume_step_label(MODE_STACK, 5), "拉取");
        assert_eq!(resume_step_label(MODE_STACK, 6), "启动");
        // 越界(成功后即清除断点,理论不可达)与未知模式兜底
        assert_eq!(resume_step_label(MODE_SINGLE, 6), "部署收尾");
        assert_eq!(resume_step_label(MODE_SINGLE, 0), "部署收尾");
        assert_eq!(resume_step_label("rollback", 1), "部署收尾");
    }

    #[test]
    fn test_single_resume_artifacts_serde_roundtrip() {
        // camelCase 序列化(存入 resume-deploy.json 的 artifacts 字段)→ 读回逐字段相等
        let art = single_art();
        let v = serde_json::to_value(&art).unwrap();
        assert_eq!(v["originRef"], "myapp:latest");
        assert_eq!(v["useDateTag"], true);
        assert_eq!(v["tarLocal"], "C:\\Temp\\abc.tar.gz");
        assert_eq!(v["tarName"], "abc.tar.gz");
        let back: SingleResumeArtifacts = serde_json::from_value(v).unwrap();
        assert_eq!(back, art);
    }

    #[test]
    fn test_stack_resume_artifacts_serde_roundtrip() {
        let art = stack_art();
        let v = serde_json::to_value(&art).unwrap();
        assert_eq!(v["releaseTs"], "20260906-101010");
        assert_eq!(v["skipUnchanged"], true);
        assert_eq!(v["forceArchive"], false);
        assert_eq!(v["files"][0], "a.tar.gz");
        assert_eq!(v["images"][0], "myapp:1");
        // services 内嵌 StackServiceChoice(snake_case 契约不变)
        assert_eq!(v["services"][0]["service"], "web");
        assert_eq!(v["services"][0]["mode"], "Local");
        let back: StackResumeArtifacts = serde_json::from_value(v).unwrap();
        assert_eq!(back, art);
    }

    #[test]
    fn test_parse_resume_artifacts_corrupt_is_none() {
        // 非 JSON 对象(数组/字符串)→ None,调用方按断点数据损坏报错
        assert!(parse_single_artifacts(&serde_json::json!([1, 2, 3])).is_none());
        assert!(parse_stack_artifacts(&serde_json::json!("垃圾")).is_none());
        // JSON 对象但字段全缺 → serde default 宽松解析为默认值;关键产物缺失
        // 由 resume_context_of 的字段级校验拦截(步骤号 > 产物完成度 → 报错)
        let lenient = parse_single_artifacts(&serde_json::json!({"nope": 1}));
        assert!(lenient.is_some());
        let cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_SINGLE),
            mode: MODE_SINGLE.into(),
            step_next: 2,
            ts: String::new(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "s".into(),
            project_name: "p".into(),
            artifacts: serde_json::json!({"nope": 1}),
        };
        assert!(resume_context_of(&cp).is_err(), "步骤 2 但缺镜像引用应判损坏");
        // 合法数据 → Some
        assert!(parse_single_artifacts(&serde_json::to_value(single_art()).unwrap()).is_some());
        assert!(parse_stack_artifacts(&serde_json::to_value(stack_art()).unwrap()).is_some());
    }

    #[test]
    fn test_resume_local_tars_by_mode() {
        // 单镜像:tar_local;整栈:locals 全部;未知模式:空
        let single_cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_SINGLE),
            mode: MODE_SINGLE.into(),
            step_next: 3,
            ts: "2026-09-06 10:00:00".into(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "服务器".into(),
            project_name: "项目".into(),
            artifacts: serde_json::to_value(single_art()).unwrap(),
        };
        let tars = resume_local_tars(&single_cp);
        assert_eq!(tars, vec![PathBuf::from("C:\\Temp\\abc.tar.gz")]);

        let stack_cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_STACK),
            mode: MODE_STACK.into(),
            step_next: 4,
            artifacts: serde_json::to_value(stack_art()).unwrap(),
            ..single_cp.clone()
        };
        let tars = resume_local_tars(&stack_cp);
        assert_eq!(tars, vec![PathBuf::from("C:\\Temp\\a.tar.gz")]);

        let unknown = ResumeCheckpoint { mode: "rollback".into(), ..stack_cp.clone() };
        assert!(resume_local_tars(&unknown).is_empty());
        // artifacts 损坏 → 空(清理尽力而为,不让放弃操作失败)
        let corrupt = ResumeCheckpoint {
            artifacts: serde_json::json!("垃圾"),
            ..stack_cp
        };
        assert!(resume_local_tars(&corrupt).is_empty());
    }

    #[test]
    fn test_resume_context_of_validates() {
        let base = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_SINGLE),
            mode: MODE_SINGLE.into(),
            step_next: 3,
            ts: String::new(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "s".into(),
            project_name: "p".into(),
            artifacts: serde_json::to_value(single_art()).unwrap(),
        };
        // 合法单镜像断点
        let ctx = resume_context_of(&base).unwrap();
        assert_eq!(ctx.step_next, 3);
        assert_eq!(ctx.key, base.key);
        assert_eq!(ctx.single.tar_name.as_deref(), Some("abc.tar.gz"));

        // 未知模式 → 报错
        let bad_mode = ResumeCheckpoint { mode: "x".into(), ..base.clone() };
        assert!(resume_context_of(&bad_mode).is_err());

        // 步骤号越界 → 报错
        let bad_step = ResumeCheckpoint { step_next: 6, ..base.clone() };
        assert!(resume_context_of(&bad_step).is_err());

        // 步骤 1 未完成(image_ref 缺失)合法;步骤 3 但缺镜像引用 → 报错
        let mut art = single_art();
        art.image_ref = None;
        let step1 = ResumeCheckpoint { step_next: 1, artifacts: serde_json::to_value(&art).unwrap(), ..base.clone() };
        assert!(resume_context_of(&step1).is_ok());
        let step3 = ResumeCheckpoint { step_next: 3, artifacts: serde_json::to_value(&art).unwrap(), ..base.clone() };
        assert!(resume_context_of(&step3).is_err());

        // 整栈:合法 + 产物列表不一致 → 报错
        let stack_base = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_STACK),
            mode: MODE_STACK.into(),
            step_next: 4,
            artifacts: serde_json::to_value(stack_art()).unwrap(),
            ..base
        };
        assert!(resume_context_of(&stack_base).is_ok());
        let mut art = stack_art();
        art.locals.clear();
        let mismatch = ResumeCheckpoint { artifacts: serde_json::to_value(&art).unwrap(), ..stack_base };
        assert!(resume_context_of(&mismatch).is_err());
    }

    #[test]
    fn test_resume_view_of_mapping() {
        let cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_STACK),
            mode: MODE_STACK.into(),
            step_next: 3,
            ts: "2026-09-06 10:00:00".into(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "生产".into(),
            project_name: "博客".into(),
            artifacts: serde_json::json!({}),
        };
        let view = resume_view_of(&cp);
        assert_eq!(view.key, "s1|p1|stack");
        assert_eq!(view.mode, "stack");
        assert_eq!(view.step_next, 3);
        assert_eq!(view.step_label, "上传");
        assert_eq!(view.ts, "2026-09-06 10:00:00");
        assert_eq!(view.server_name, "生产");
        assert_eq!(view.project_name, "博客");
        // camelCase 序列化(前端契约)
        let v = serde_json::to_value(&view).unwrap();
        assert!(v.get("stepNext").is_some());
        assert!(v.get("stepLabel").is_some());
        assert!(v.get("serverName").is_some());
    }

    #[test]
    fn test_temp_file_guard_drop_semantics() {
        // new:Drop 删除;keep(断点活跃):Drop 保留(由成功收尾/放弃显式清理)
        let p1 = std::env::temp_dir().join(format!("dd-guard-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&p1, b"x").unwrap();
        {
            let _g = TempFileGuard::new(p1.clone());
        }
        assert!(!p1.exists(), "TempFileGuard::new 应在 Drop 时删除文件");

        let p2 = std::env::temp_dir().join(format!("dd-guard-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&p2, b"x").unwrap();
        {
            let _g = TempFileGuard::keep(p2.clone());
        }
        assert!(p2.exists(), "TempFileGuard::keep 不应在 Drop 时删除文件");
        std::fs::remove_file(&p2).ok();
    }

    #[test]
    fn test_checkpoint_cleanup_on_success_removes_tar_and_checkpoint() {
        // 成功收尾:断点删除 + 保留的本地 tar 删除;文件已不存在时静默
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        let key = checkpoint_key("s9", "p9", MODE_SINGLE);
        let cp = ResumeCheckpoint {
            key: key.clone(),
            mode: MODE_SINGLE.into(),
            step_next: 5,
            ts: "2026-09-06 10:00:00".into(),
            server_id: "s9".into(),
            project_id: "p9".into(),
            server_name: "s".into(),
            project_name: "p".into(),
            artifacts: serde_json::json!({}),
        };
        crate::config::save_checkpoint(&cp).unwrap();
        let tar = dir.join("left.tar.gz");
        std::fs::write(&tar, b"x").unwrap();

        checkpoint_cleanup_on_success(&key, &[tar.clone(), dir.join("already-gone.tar.gz")]);
        assert!(crate::config::load_resume_map().get(&key).is_none(), "断点应被清除");
        assert!(!tar.exists(), "保留的本地 tar 应被删除");

        std::fs::remove_dir_all(&dir).ok();
    }
}
