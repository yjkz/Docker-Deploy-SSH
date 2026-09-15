//! 定时/延迟部署(第二十二批):项目级「每天 HH:MM」或「一次性延迟」日程,
//! 到点由后端 tick 自主发起部署(单镜像 / 整栈复用既有部署管线)。
//!
//! ## 设计要点
//!
//! - **存储**:独立文件 `config/deploy-schedules.json`(不含密文,不进
//!   CONFIG_LOCK;同 profiles.rs 的低耦合先例);模块内 `SCHED_LOCK` 保护
//!   读改写,cap [`MAX_SCHEDULES`] 条裁最旧。
//! - **tick 循环**:`Mutex<Option<JoinHandle>>` 单任务常驻(应用启动时启动,
//!   应用退出随运行时结束);30s 一轮,与到点时刻比对 —— 触发窗口 =
//!   `[时刻, 时刻 + 90s)`(容忍 tick 抖动与系统休眠唤醒后的迟到 tick)。
//! - **错过不补跑**(用户定案):应用启动时对「今天已过点且今天未跑过」的
//!   daily 条目写一条 `last_result = "已错过(应用未运行),未补跑"`(仅记录
//!   一次,不重复刷);之后正常到点照跑。
//! - **触发前取全局远程操作互斥位**:与手动部署/回滚/迁移互斥;被拒时写
//!   `last_result` 跳过本轮(不打扰),下一周期自然重试不适用(一次性直接
//!   记失败,daily 等明天)。
//! - **触发后走既有管线**:整栈 = [`parse_project_stack`] 现解析(与部署页
//!   同口径)+ 过滤无 image 服务 → [`run_one_deploy_stack`];单镜像 =
//!   存储的 `image_ref` → [`run_one_deploy`]。托盘/历史/通知/断点全部由
//!   既有收尾链路承担,本模块零重复实现。
//! - **deploy-progress 提示**:触发瞬间 emit 一行 `deploy-log`「定时部署
//!   触发」,同时前端部署页在无本地部署进行中时会「采纳」该部署(第二
//!   十二批前端配合):按钮态/取消钮可用,事件照常驱动进度。

use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::commands::{
    self, parse_project_stack, run_one_deploy, run_one_deploy_stack, DeployEmitOpts,
    DeployRequest, StackDeployRequest, StackServiceChoice,
};
use crate::config::{self, ProjectConfig};

/// 存储文件内的条目数上限(超限裁最旧,与 MAX_PROFILES 同风格)。
pub const MAX_SCHEDULES: usize = 30;

/// tick 粒度(秒):到点判定精度;触发窗口 = [时刻, 时刻 + 90s)。
const TICK_SECS: u64 = 30;

/// 触发窗口(秒):时刻过后多久内仍视为「到点」(容忍 tick 抖动/休眠唤醒)。
const FIRE_WINDOW_SECS: i64 = 90;

/// 日程条目(存储于 `config/deploy-schedules.json`;camelCase 与前端直通)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploySchedule {
    /// 前端生成的 uuid
    pub id: String,
    pub project_id: String,
    pub server_id: String,
    /// "stack" | "single"
    pub mode: String,
    /// 单镜像模式的镜像引用(`repository:tag`);stack 模式为空串
    #[serde(default)]
    pub image_ref: String,
    /// "daily"(每天 HH:MM)| "once"(一次性:today HH:MM,过点即停用)
    pub kind: String,
    /// 触发时刻 "HH:MM"(本地时区)
    pub time: String,
    /// 选项:整栈跳过未变化镜像 / 强制留档
    #[serde(default)]
    pub skip_unchanged: bool,
    #[serde(default)]
    pub force_archive: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub created_at: String,
    /// 最近执行日期 "YYYY-MM-DD"(daily 防重跑;once 执行后即停用)
    #[serde(default)]
    pub last_run_date: String,
    /// 最近一次结果描述(展示用)
    #[serde(default)]
    pub last_result: String,
}

fn default_true() -> bool {
    true
}

/// 模块级锁:保护存储文件读改写(独立文件,不进 CONFIG_LOCK)。
static SCHED_LOCK: Mutex<()> = Mutex::new(());

/// tick 循环句柄(应用启动即常驻)。
static SCHED_TASK: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> = Mutex::new(None);

fn schedules_path() -> std::path::PathBuf {
    config::config_dir().join("deploy-schedules.json")
}

/// 读取全部日程(缺失/损坏 → 空表 + 告警;与 load_profiles 同风格)。
pub fn load_schedules() -> Vec<DeploySchedule> {
    let path = schedules_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("读取定时日程失败 ({}): {}", path.display(), e);
            }
            return Vec::new();
        }
    };
    match serde_json::from_str::<Vec<DeploySchedule>>(&text) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("定时日程文件损坏,按空表处理 ({}): {}", path.display(), e);
            Vec::new()
        }
    }
}

/// 保存全部日程(原子写;调用方持 [`SCHED_LOCK`])。
fn save_schedules(list: &[DeploySchedule]) -> Result<(), String> {
    if let Some(dir) = schedules_path().parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败: {}", e))?;
    }
    config::write_json_atomic(&schedules_path(), &list.to_vec())
        .map_err(|e| format!("写入定时日程失败: {}", e))
}

/// 校验 "HH:MM"(00-23:00-59)。纯函数,便于单测。
pub fn validate_time(t: &str) -> Result<(u32, u32), String> {
    let s = t.trim();
    let (h, m) = s
        .split_once(':')
        .ok_or_else(|| format!("时间格式应为 HH:MM:{}", t))?;
    let hh: u32 = h
        .parse()
        .map_err(|_| format!("时间格式应为 HH:MM:{}", t))?;
    let mm: u32 = m
        .parse()
        .map_err(|_| format!("时间格式应为 HH:MM:{}", t))?;
    if hh > 23 || mm > 59 || h.len() != 2 || m.len() != 2 {
        return Err(format!("时间超出范围(00:00-23:59):{}", t));
    }
    Ok((hh, mm))
}

/// 判断某条目在 `now`(本地时间)时点是否应当触发。纯函数,便于单测。
///
/// 触发条件(`now_secs` = 当日 0 点起的秒数):
/// - enabled;`last_run_date != today`(daily 防重 / once 同字段);
/// - `now_secs >= fire_secs && now_secs < fire_secs + FIRE_WINDOW_SECS`;
/// - **once 额外要求 `today == 创建日`**(目标日 = 保存当天;跨天不触发,
///   防「昨天的一次性在今天迟到执行」——错过不补跑的语义延伸)。
pub fn is_due(s: &DeploySchedule, today: &str, now_secs: i64) -> bool {
    if !s.enabled {
        return false;
    }
    if s.last_run_date == today {
        return false; // 今天已跑(daily 防重;once 同字段)
    }
    if s.kind == "once" && s.created_at.get(0..10) != Some(today) {
        return false;
    }
    let Ok((hh, mm)) = validate_time(&s.time) else {
        return false; // 非法时间不触发(保存时已校验,防御损坏文件)
    };
    let fire_secs = i64::from(hh) * 3600 + i64::from(mm) * 60;
    now_secs >= fire_secs && now_secs < fire_secs + FIRE_WINDOW_SECS
}

/// 启动扫描用:判断条目是否**已错过**(错过不补跑,写结果)。
///
/// 返回 `Some(disable)`:disable=true 表示还应同时停用该条目。
/// - daily:超过触发窗口且今天未跑 → `Some(false)`(保持启用,明天照常);
/// - once:创建日已过仍未跑(或创建日当天已过窗口)→ `Some(true)`(停用,
///   一次性日程不会跨天迟到执行)。
pub fn missed_disposition(s: &DeploySchedule, today: &str, now_secs: i64) -> Option<bool> {
    if !s.enabled || s.last_run_date == today {
        return None;
    }
    let Ok((hh, mm)) = validate_time(&s.time) else {
        return None;
    };
    let fire_secs = i64::from(hh) * 3600 + i64::from(mm) * 60;
    match s.kind.as_str() {
        "daily" => {
            if now_secs >= fire_secs + FIRE_WINDOW_SECS {
                Some(false)
            } else {
                None
            }
        }
        "once" => {
            let created_date = s.created_at.get(0..10).unwrap_or("");
            if today > created_date || now_secs >= fire_secs + FIRE_WINDOW_SECS {
                Some(true)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 当前本地时间的 (日期 "YYYY-MM-DD", 当日秒数)。
fn now_parts() -> (String, i64) {
    let now = chrono::Local::now();
    let secs = now
        .time()
        .signed_duration_since(chrono::NaiveTime::MIN)
        .num_seconds();
    (now.format("%Y-%m-%d").to_string(), secs)
}

// ===== 命令 =====

/// 列出全部定时日程。
#[tauri::command]
pub fn deploy_schedules_list() -> Vec<DeploySchedule> {
    load_schedules()
}

/// 保存(新建/编辑)一条定时日程:校验字段、upsert、超上限裁最旧。
#[tauri::command]
pub fn deploy_schedules_save(schedule: DeploySchedule) -> Result<(), String> {
    if schedule.id.trim().is_empty() {
        return Err("日程 id 不能为空".to_string());
    }
    if schedule.project_id.trim().is_empty() || schedule.server_id.trim().is_empty() {
        return Err("请先选择项目与服务器".to_string());
    }
    if schedule.mode != "stack" && schedule.mode != "single" {
        return Err(format!("未知的部署模式:{}", schedule.mode));
    }
    if schedule.mode == "single" && schedule.image_ref.trim().is_empty() {
        return Err("单镜像模式需要选择镜像".to_string());
    }
    if schedule.kind != "daily" && schedule.kind != "once" {
        return Err(format!("未知的日程类型:{}", schedule.kind));
    }
    validate_time(&schedule.time)?;
    let _guard = SCHED_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut list = load_schedules();
    if let Some(existing) = list.iter_mut().find(|s| s.id == schedule.id) {
        // 编辑保留最近执行记录与创建时刻(created_at 是 once 的目标日锚点,
        // 也参与 cap 排序 —— 编辑不得重置)
        let mut next = schedule.clone();
        next.last_run_date = existing.last_run_date.clone();
        next.last_result = existing.last_result.clone();
        next.created_at = existing.created_at.clone();
        *existing = next;
    } else {
        // 新条目:创建时刻由后端归一为当前时间(once 的「目标日 = 创建日」
        // 语义依赖它;前端传值不参与判定)
        let mut fresh = schedule;
        fresh.created_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        list.push(fresh);
    }
    if list.len() > MAX_SCHEDULES {
        // 裁最旧(created_at 字符串比较即时间序,同断点表口径)
        list.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        let cut = list.len() - MAX_SCHEDULES;
        list.drain(0..cut);
    }
    save_schedules(&list)
}

/// 删除一条定时日程(幂等)。
#[tauri::command]
pub fn deploy_schedules_delete(id: String) -> Result<(), String> {
    let _guard = SCHED_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut list = load_schedules();
    let before = list.len();
    list.retain(|s| s.id != id);
    if list.len() == before {
        return Ok(()); // 幂等
    }
    save_schedules(&list)
}

// ===== tick 循环与触发 =====

/// 启动常驻 tick 循环(setup 调用一次;重复调用为无操作)。
pub fn start_scheduler(app: &AppHandle) {
    let mut guard = SCHED_TASK.lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_some() {
        return;
    }
    let app = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        log::info!("定时部署调度器已启动: tick {} 秒", TICK_SECS);
        // 启动扫描:记录「错过未补跑」(仅一次),防用户开应用看不到错过提示
        startup_missed_scan();
        let mut ticker = tokio::time::interval(Duration::from_secs(TICK_SECS));
        ticker.tick().await; // 跳过首个立即 tick
        loop {
            ticker.tick().await;
            tick_once(&app).await;
        }
    });
    *guard = Some(handle);
}

/// 启动扫描:对已错过的条目写一次「已错过」结果(once 同时停用)。
fn startup_missed_scan() {
    let (today, now_secs) = now_parts();
    let _guard = SCHED_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut list = load_schedules();
    let mut changed = false;
    for s in list.iter_mut() {
        if let Some(disable) = missed_disposition(s, &today, now_secs) {
            if s.last_result != "已错过(应用未运行),未补跑" {
                s.last_result = "已错过(应用未运行),未补跑".to_string();
                changed = true;
            }
            if disable && s.enabled {
                s.enabled = false;
                changed = true;
            }
        }
    }
    if changed {
        if let Err(e) = save_schedules(&list) {
            log::warn!("记录错过日程失败: {}", e);
        }
    }
}

/// 一轮 tick:找出应触发条目,逐个尝试发起(串行,符合互斥语义)。
async fn tick_once(app: &AppHandle) {
    let (today, now_secs) = now_parts();
    // 先在锁内挑出候选(不 await);串行逐个触发(每个触发内部独占互斥位)
    let due: Vec<DeploySchedule> = {
        let _guard = SCHED_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let list = load_schedules();
        list.into_iter()
            .filter(|s| is_due(s, &today, now_secs))
            .collect()
    };
    for sched in due {
        fire_schedule(app, sched, &today).await;
    }
}

/// 触发单条日程:校验项目/服务器仍在 → 构造请求 → 走既有部署管线 →
/// 回写 `last_run_date` / `last_result`。串行执行(await 到部署收尾)。
async fn fire_schedule(app: &AppHandle, sched: DeploySchedule, today: &str) {
    let cfg = config::load_config().ok();
    let project = cfg
        .as_ref()
        .and_then(|cfg| cfg.projects.iter().find(|p| p.id == sched.project_id).cloned());
    let server_exists = cfg
        .as_ref()
        .map(|cfg| cfg.servers.iter().any(|s| s.id == sched.server_id))
        .unwrap_or(false);

    let (ok, msg) = match (project, server_exists) {
        (Some(project), true) => run_schedule_deploy(app, &sched, &project).await,
        (None, _) => (false, "项目已不存在,日程跳过".to_string()),
        (_, false) => (false, "服务器已不存在,日程跳过".to_string()),
    };

    // 回写执行记录(锁内读改写;失败仅告警)
    {
        let _guard = SCHED_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut list = load_schedules();
        if let Some(s) = list.iter_mut().find(|s| s.id == sched.id) {
            s.last_run_date = today.to_string();
            s.last_result = if ok {
                format!("{} 部署成功", &today[5..])
            } else {
                format!("{} 失败:{}", &today[5..], msg)
            };
            if sched.kind == "once" {
                s.enabled = false; // 一次性:执行后即停用
                s.last_result = format!("{} · 已停用(一次性)", s.last_result);
            }
            if let Err(e) = save_schedules(&list) {
                log::warn!("回写日程执行记录失败: {}", e);
            }
        }
    }
}

/// 执行一次日程部署(返回 (是否成功, 结果描述))。
async fn run_schedule_deploy(
    app: &AppHandle,
    sched: &DeploySchedule,
    project: &ProjectConfig,
) -> (bool, String) {
    // 触发提示行(前端部署页日志可见;无部署页也能从日志文件排障)
    let _ = app.emit(
        "deploy-log",
        commands::format_log_line(
            "",
            &format!(
                "定时部署触发:{} @ {}",
                project.name,
                sched.time
            ),
        ),
    );

    if sched.mode == "single" {
        let (repository, _tag) = crate::stack::split_image_ref(&sched.image_ref);
        let req = DeployRequest {
            image: sched.image_ref.clone(),
            repository,
            server_id: sched.server_id.clone(),
            project_id: sched.project_id.clone(),
            use_date_tag: false,
            password_plain: None,
            skip_unchanged: Some(sched.skip_unchanged),
        };
        return match run_one_deploy(app, req, None, DeployEmitOpts::scheduled()).await {
            Ok(_) => (true, "成功".to_string()),
            Err(e) => (false, crate::errors::strip(&e).to_string()),
        };
    }

    // 整栈:现解析(与部署页同口径,含 service_overrides 应用)→ 过滤无 image 服务
    let stack = match parse_project_stack(project).await {
        Ok(s) => s,
        Err(e) => return (false, format!("解析 compose 失败:{}", e)),
    };
    if !stack.errors.is_empty() {
        return (
            false,
            format!("compose 存在阻断问题:{}", stack.errors.join("; ")),
        );
    }
    let services: Vec<StackServiceChoice> = stack
        .services
        .iter()
        .filter_map(|svc| {
            svc.image.as_ref().map(|img| StackServiceChoice {
                service: svc.service.clone(),
                image: img.clone(),
                mode: svc.mode.clone(),
            })
        })
        .collect();
    // Local 类必须有 image(与 validate_stack_choices 同约束;此处过滤后校验)
    if services.is_empty() {
        return (
            false,
            "compose 未解析出可部署服务(服务均无 image 字段?)".to_string(),
        );
    }
    let req = StackDeployRequest {
        project_id: sched.project_id.clone(),
        server_id: sched.server_id.clone(),
        services,
        password_plain: None,
        skip_unchanged: Some(sched.skip_unchanged),
        force_archive: Some(sched.force_archive),
    };
    match run_one_deploy_stack(app, req, None, DeployEmitOpts::scheduled()).await {
        Ok(_) => (true, "成功".to_string()),
        Err(e) => (false, crate::errors::strip(&e).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: &str, kind: &str, time: &str, enabled: bool) -> DeploySchedule {
        DeploySchedule {
            id: id.into(),
            project_id: "p".into(),
            server_id: "s".into(),
            mode: "stack".into(),
            image_ref: String::new(),
            kind: kind.into(),
            time: time.into(),
            skip_unchanged: true,
            force_archive: false,
            enabled,
            created_at: "2026-09-15 10:00:00".into(),
            last_run_date: String::new(),
            last_result: String::new(),
        }
    }

    /// 10:00 对应秒数
    fn secs(h: i64, m: i64) -> i64 {
        h * 3600 + m * 60
    }

    #[test]
    fn test_validate_time() {
        assert_eq!(validate_time("08:30").unwrap(), (8, 30));
        assert_eq!(validate_time("00:00").unwrap(), (0, 0));
        assert_eq!(validate_time("23:59").unwrap(), (23, 59));
        assert!(validate_time("24:00").is_err());
        assert!(validate_time("8:30").is_err(), "必须两位小时");
        assert!(validate_time("08:5").is_err(), "必须两位分钟");
        assert!(validate_time("aa:bb").is_err());
        assert!(validate_time("08-30").is_err());
    }

    #[test]
    fn test_is_due_window() {
        let s = mk("1", "daily", "10:00", true);
        // 窗口前 1 秒:不触发
        assert!(!is_due(&s, "2026-09-15", secs(9, 59) + 59));
        // 窗口起点:触发
        assert!(is_due(&s, "2026-09-15", secs(10, 0)));
        // 窗口内(89 秒处):触发
        assert!(is_due(&s, "2026-09-15", secs(10, 0) + 89));
        // 窗口末端(90 秒处):不触发(半开区间)
        assert!(!is_due(&s, "2026-09-15", secs(10, 0) + 90));
    }

    #[test]
    fn test_is_due_guards() {
        // 停用不触发
        let disabled = mk("1", "daily", "10:00", false);
        assert!(!is_due(&disabled, "2026-09-15", secs(10, 0)));
        // 今天已跑(daily 防重)不触发
        let mut ran = mk("1", "daily", "10:00", true);
        ran.last_run_date = "2026-09-15".into();
        assert!(!is_due(&ran, "2026-09-15", secs(10, 0)));
        // 昨天跑过,今天照触发
        let mut yesterday = mk("1", "daily", "10:00", true);
        yesterday.last_run_date = "2026-09-14".into();
        assert!(is_due(&yesterday, "2026-09-15", secs(10, 0)));
        // 非法时间不触发(防御损坏文件)
        let bad = mk("1", "daily", "99:99", true);
        assert!(!is_due(&bad, "2026-09-15", secs(10, 0)));
    }

    #[test]
    fn test_is_due_once_only_on_created_day() {
        // once:创建当天窗口内触发
        let mut s = mk("1", "once", "10:00", true);
        s.created_at = "2026-09-15 08:00:00".into();
        assert!(is_due(&s, "2026-09-15", secs(10, 0)));
        // 次日窗口内不触发(错过不补跑的延伸:一次性不得跨天迟到执行)
        assert!(!is_due(&s, "2026-09-16", secs(10, 0)));
    }

    #[test]
    fn test_missed_disposition() {
        // daily:窗口内不算错过
        let s = mk("1", "daily", "10:00", true);
        assert_eq!(missed_disposition(&s, "2026-09-15", secs(10, 0) + 89), None);
        // daily:超过窗口 → 记录,保持启用
        assert_eq!(
            missed_disposition(&s, "2026-09-15", secs(10, 0) + 90),
            Some(false)
        );
        // daily:已跑过不算错过
        let mut ran = mk("1", "daily", "10:00", true);
        ran.last_run_date = "2026-09-15".into();
        assert_eq!(missed_disposition(&ran, "2026-09-15", secs(12, 0)), None);
        // once:创建日当天过窗口 → 停用
        let mut once = mk("1", "once", "10:00", true);
        once.created_at = "2026-09-15 08:00:00".into();
        assert_eq!(
            missed_disposition(&once, "2026-09-15", secs(10, 0) + 90),
            Some(true)
        );
        // once:跨天(未执行残留)→ 停用
        assert_eq!(missed_disposition(&once, "2026-09-16", secs(8, 0)), Some(true));
        // 停用不扫
        let disabled = mk("1", "daily", "10:00", false);
        assert_eq!(missed_disposition(&disabled, "2026-09-15", secs(12, 0)), None);
    }

    #[test]
    fn test_schedule_serde_camel_case() {
        let s = mk("id-1", "daily", "08:00", true);
        let v = serde_json::to_value(&s).unwrap();
        for key in [
            "projectId",
            "serverId",
            "imageRef",
            "skipUnchanged",
            "forceArchive",
            "createdAt",
            "lastRunDate",
            "lastResult",
        ] {
            assert!(v.get(key).is_some(), "缺字段 {}: {:?}", key, v);
        }
        // 往返
        let back: DeploySchedule = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
    }
}
