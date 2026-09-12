// // 部署断点续传

use super::*;

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
pub(crate) struct SingleResumeArtifacts {
    /// 原始镜像引用(部署请求的 `image`;步骤 1 重跑的打标签源、retag 目标)
    pub(crate) origin_ref: String,
    /// 打标签前缀(部署请求的 `repository`;步骤 1 重跑生成新标签时用)
    pub(crate) repository: String,
    /// 是否日期标签部署(部署请求的 `use_date_tag`)
    pub(crate) use_date_tag: bool,
    /// 步骤 1 产物:实际部署引用(`None` = 步骤 1 尚未完成)
    pub(crate) image_ref: Option<String>,
    /// 步骤 2 产物:本地 tar 绝对路径
    pub(crate) tar_local: Option<String>,
    /// 步骤 2 决定的远端 tar 文件名(步骤 3 上传到 `/tmp/<tar_name>`)
    pub(crate) tar_name: Option<String>,
    /// 回传字段:智能传输开关(部署请求的 `skip_unchanged`)
    pub(crate) skip_unchanged: bool,
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
pub(crate) struct StackResumeArtifacts {
    /// 部署请求的完整服务分类列表
    pub(crate) services: Vec<StackServiceChoice>,
    /// 智能传输判定结果(与 Local 服务顺序对齐;未启用时为空)
    pub(crate) unchanged: Vec<bool>,
    /// 回传字段:智能传输开关
    pub(crate) skip_unchanged: bool,
    /// 回传字段:强制留档
    pub(crate) force_archive: bool,
    /// 步骤 3 中创建的发布时间戳(`None` = 尚未创建发布目录)
    pub(crate) release_ts: Option<String>,
    /// 步骤 2 产物:镜像包远端文件名(按打包顺序)
    pub(crate) files: Vec<String>,
    /// 与 [`StackResumeArtifacts::files`] 对齐的本地 tar 绝对路径
    pub(crate) locals: Vec<String>,
    /// 与 [`StackResumeArtifacts::files`] 对齐的镜像引用(装载幂等检查用)
    pub(crate) images: Vec<String>,
}

/// 断点续传上下文:由 checkpoint 反序列化而来,驱动部署管线的跳步与幂等化。
#[derive(Debug, Clone)]
pub(crate) struct ResumeContext {
    /// 断点键(成功收尾按它清除断点)
    pub(crate) key: String,
    /// 续传起点:下一个待执行的步骤号(该步骤及之后都要执行)
    pub(crate) step_next: u32,
    /// 单镜像模式产物(mode = "single" 时有效)
    pub(crate) single: SingleResumeArtifacts,
    /// 整栈模式产物(mode = "stack" 时有效)
    pub(crate) stack: StackResumeArtifacts,
}

/// 单镜像部署的总步骤数(与 emit_progress 的 total 一致)。
const SINGLE_TOTAL_STEPS: u32 = 5;

/// 整栈部署的总步骤数。
const STACK_TOTAL_STEPS: u32 = 6;

/// 步骤号 → 中文标签(续传状态/日志展示用,自建映射):
/// 单镜像 1..5 = 打标签/导出压缩/上传镜像/同步文件/服务器部署;
/// 整栈 1..6 = 分类确认/打包/上传/装载/拉取/启动;
/// 超出范围(成功后清除断点,理论不可达)按"部署收尾"兜底。
pub(crate) fn resume_step_label(mode: &str, step_next: u32) -> String {
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
pub(crate) fn parse_single_artifacts(v: &serde_json::Value) -> Option<SingleResumeArtifacts> {
    serde_json::from_value(v.clone()).ok()
}

/// 解析整栈断点产物(损坏 → `None`)。
pub(crate) fn parse_stack_artifacts(v: &serde_json::Value) -> Option<StackResumeArtifacts> {
    serde_json::from_value(v.clone()).ok()
}

/// 收集断点记录的本地临时 tar 路径(`deploy_resume_discard` 清理用;
/// 产物解析失败按空处理 —— 清理是尽力而为,不让放弃操作失败)。
pub(crate) fn resume_local_tars(cp: &ResumeCheckpoint) -> Vec<PathBuf> {
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
pub(crate) fn artifacts_value<T: Serialize>(art: &T) -> serde_json::Value {
    serde_json::to_value(art).unwrap_or_default()
}

/// 落盘部署断点(尽力而为:失败仅告警,不影响部署管线本身)。
///
/// `step_next` = 下一个待执行的步骤号(刚完成步骤号 + 1;失败/取消发生
/// 在该步骤,续传时从它开始重跑)。每次保存都会覆盖同键旧条目并刷新 `ts`。
pub(crate) fn checkpoint_save(
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
pub(crate) fn checkpoint_cleanup_on_success(key: &str, local_tars: &[PathBuf]) {
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
pub(crate) fn resume_context_of(cp: &ResumeCheckpoint) -> Result<ResumeContext, String> {
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
pub(crate) fn resume_view_of(cp: &ResumeCheckpoint) -> ResumeView {
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

