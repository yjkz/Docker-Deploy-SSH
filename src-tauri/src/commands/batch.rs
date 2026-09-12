// // 多服务器批量部署(后端编排为死代码,保留见 wiki/04)

use super::*;

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
/// - 与断点续传互斥:本实现不落断点、不支持续传(resume = None 且 checkpoint
///   关闭,见 [`DeployEmitOpts::batch`]),单台失败整台重跑;
///   **注意:该「批量不落断点」只描述本死代码实现** —— 线上批量由前端队列
///   逐台调用单发 `deploy`/`deploy_stack`(恒 `checkpoint = true`),因此
///   线上批量的失败台**有**断点且可续传(第十四批);
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

