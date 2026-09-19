//! 部署日报/汇总(第二十八批 B2)。
//!
//! **做什么**:把当天的 `DeployRecord` 聚合成**一条**摘要(成功/失败/取消计数 +
//! 涉及的项目与服务器),经通知管道(`kind = "digest"`)发一次 —— 替代逐条即时
//! 通知的"轰炸",适合「下班后看一眼今天部署了什么」。
//!
//! **触发源**:独立常驻任务(仿 `probe.rs` 的 probe/alert 先例,不复用部署日程
//! 的 tick —— 那是「到点执行部署」的语义,与「到点发一条汇总」不同)。发射时刻由
//! `AppSettings.digest_hour` 指定(缺省 `None` = 关闭);到点后若当天尚未发送则发,
//! 每次发送记 `last_sent_date` 防重。
//!
//! **为什么用独立状态文件**:发送标记若放进 `AppSettings`,settings 表单保存会
//! 整量覆盖该字段(前端只发自己认识的字段)→ 标记被静默清空、日报当天重发。
//! 独立文件(`config/digest-state.json`)与 `deploy-schedules.json` 同款,不受
//! 设置保存影响。
//!
//! **订阅开关**:`NotifyConfig.events.on_digest`(默认关 —— 与 probe/alert 同纪律:
//! 新增通知类型不改变既有用户的通知量)。

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

use tauri::AppHandle;

use crate::config;
use crate::history::{self, DeployRecord};

/// tick 粒度(秒):到点精度;日报只需分钟级,60s 足够且比部署日程(30s)更省。
const TICK_SECS: u64 = 60;

/// 发送状态(独立文件,防设置保存误清)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DigestState {
    /// 最近一次发送日期 "YYYY-MM-DD"(防当天重发)
    #[serde(default)]
    pub last_sent_date: String,
}

/// 常驻任务句柄(设置变更时停旧起新,与 probe/alert 同款)。
static DIGEST_TASK: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> = Mutex::new(None);

fn state_path() -> std::path::PathBuf {
    config::config_dir().join("digest-state.json")
}

/// 读发送状态(缺失/损坏 → 默认值;损坏仅告警,不阻断)。
pub fn load_state() -> DigestState {
    let path = state_path();
    match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_else(|e| {
            log::warn!("日报状态文件损坏,按默认处理 ({}): {}", path.display(), e);
            DigestState::default()
        }),
        Err(_) => DigestState::default(),
    }
}

/// 写发送状态(原子写;失败仅告警 —— 最坏是当天重发一次)。
fn save_state(state: &DigestState) {
    if let Some(dir) = state_path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = config::write_json_atomic(&state_path(), state) {
        log::warn!("写入日报状态失败: {}", e);
    }
}

/// 当天部署计数(纯函数产出,便于单测与正文拼装)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DigestCounts {
    pub total: usize,
    pub success: usize,
    pub failed: usize,
    /// 取消(mode 与 success 之外的第三条线:取消不算失败)
    pub cancelled: usize,
    /// 涉及的项目名(去重,保序 = 首次出现顺序)
    pub projects: Vec<String>,
    /// 涉及的服务器名(去重保序)
    pub servers: Vec<String>,
}

/// 按日期聚合部署记录(纯函数):只统计 `ts` 以 `date` 开头的条目。
///
/// 返回 `None` = 当天无记录(**不发空日报**)。
/// 「取消」判定:记录 `message` 含取消语义 —— 收尾路径对取消写的是
/// `errors::cancelled()` 的文案,历史里 `success=false`。为避免把取消算成
/// 「失败」而误导,这里按 `success=false` 且消息含取消标记分流。
pub fn build_digest(records: &[DeployRecord], date: &str) -> Option<DigestCounts> {
    let mut counts = DigestCounts::default();
    for r in records {
        if !r.ts.starts_with(date) {
            continue;
        }
        counts.total += 1;
        if r.success {
            counts.success += 1;
        } else if is_cancel_message(&r.message) {
            counts.cancelled += 1;
        } else {
            counts.failed += 1;
        }
        if !r.project_name.trim().is_empty() && !counts.projects.iter().any(|p| p == &r.project_name) {
            counts.projects.push(r.project_name.clone());
        }
        if !r.server_name.trim().is_empty() && !counts.servers.iter().any(|s| s == &r.server_name) {
            counts.servers.push(r.server_name.clone());
        }
    }
    if counts.total == 0 {
        None
    } else {
        Some(counts)
    }
}

/// 记录是否属于「用户取消」(纯函数;与前端 `errCodeOf` / 后端错误码同口径的
/// 文本回退 —— 历史记录只留 message,故按标记词判定)。
fn is_cancel_message(msg: &str) -> bool {
    msg.contains("已取消") || msg.contains("取消")
}

/// 拼日报标题与正文(纯函数,便于单测)。
pub fn digest_text(counts: &DigestCounts, date: &str) -> (String, String) {
    let title = format!("{} 部署日报", &date[5..]);
    let mut body = format!(
        "共 {} 次:成功 {} / 失败 {}",
        counts.total, counts.success, counts.failed
    );
    if counts.cancelled > 0 {
        body.push_str(&format!(" / 取消 {}", counts.cancelled));
    }
    if !counts.projects.is_empty() {
        body.push_str(&format!("\n项目:{}", counts.projects.join("、")));
    }
    if !counts.servers.is_empty() {
        body.push_str(&format!("\n服务器:{}", counts.servers.join("、")));
    }
    (title, body)
}

/// 是否应发送日报(纯函数):配置了小时、当天未发、且当前已到/过该小时。
///
/// `configured_hour = None` → 关闭。`now_secs` = 当日 0 点起秒数。
/// 「已到/过」而非「恰好在整点」:任务按 [`TICK_SECS`] 轮询,应用在该小时
/// 内启动(或休眠唤醒)也算到点 —— 日刊错过不补跑(隔天不再补),与部署日程
/// 的错过语义一致。
pub fn should_fire(configured_hour: Option<u32>, now_secs: i64, last_sent_date: &str, today: &str) -> bool {
    let Some(hour) = configured_hour else {
        return false;
    };
    if hour > 23 {
        return false; // 损坏值不触发(保存侧已夹取)
    }
    if last_sent_date == today {
        return false;
    }
    now_secs >= i64::from(hour) * 3600
}

/// 当前本地时间的 (日期, 当日秒数)。
fn now_parts() -> (String, i64) {
    let now = chrono::Local::now();
    let secs = now
        .time()
        .signed_duration_since(chrono::NaiveTime::MIN)
        .num_seconds();
    (now.format("%Y-%m-%d").to_string(), secs)
}

/// 一轮检查:到点则聚合当天记录并发送(发送后记状态)。
pub async fn tick_once(app: &AppHandle) {
    let settings = config::load_app_settings();
    let (today, now_secs) = now_parts();
    let state = load_state();
    if !should_fire(settings.digest_hour, now_secs, &state.last_sent_date, &today) {
        return;
    }
    let records = history::load_history();
    let Some(counts) = build_digest(&records, &today) else {
        // 当天无部署:不发空日报,但仍记状态(否则每分钟重查一次)
        save_state(&DigestState {
            last_sent_date: today,
        });
        return;
    };
    let (title, body) = digest_text(&counts, &today);
    crate::notify::fire(app.clone(), "digest", title, body).await;
    save_state(&DigestState {
        last_sent_date: today,
    });
}

/// 按设置启停日报任务(setup 与 `app_settings_set` 各调一次,与 probe 同款)。
///
/// `digest_hour = None` → 停旧任务且不启动。
pub fn sync_from_settings(app: &AppHandle) {
    let settings = config::load_app_settings();
    let mut guard = DIGEST_TASK.lock().unwrap_or_else(|p| p.into_inner());
    // 先停旧(设置变更后按新配置重启;与 probe::sync_from_settings 同款)
    if let Some(handle) = guard.take() {
        handle.abort();
    }
    if settings.digest_hour.is_none() {
        return;
    }
    let app = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        log::info!("部署日报任务已启动: 每天 {:?} 时", app_digest_hour());
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(TICK_SECS));
        ticker.tick().await; // 跳过首个立即 tick
        loop {
            ticker.tick().await;
            tick_once(&app).await;
        }
    });
    *guard = Some(handle);
}

/// 任务内读一次配置(仅用于启动日志;失败给 None)。
fn app_digest_hour() -> Option<u32> {
    config::load_app_settings().digest_hour
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ts: &str, project: &str, server: &str, success: bool, msg: &str) -> DeployRecord {
        DeployRecord {
            ts: ts.into(),
            mode: "stack".into(),
            server_name: server.into(),
            project_name: project.into(),
            images: Vec::new(),
            success,
            message: msg.into(),
            duration_secs: 10,
            release_dir: None,
            server_id: None,
            project_id: None,
        }
    }

    #[test]
    fn test_build_digest_counts_and_dedup() {
        let records = vec![
            rec("2026-09-19 09:00:00", "webapp", "web-01", true, "部署成功"),
            rec("2026-09-19 10:00:00", "webapp", "web-01", false, "健康检查失败"),
            rec("2026-09-19 11:00:00", "api", "api-01", true, "部署成功"),
            rec("2026-09-19 12:00:00", "api", "api-01", false, "部署已取消"),
            // 别的日期:不计
            rec("2026-09-18 23:59:59", "webapp", "web-01", true, "部署成功"),
        ];
        let c = build_digest(&records, "2026-09-19").unwrap();
        assert_eq!(c.total, 4);
        assert_eq!(c.success, 2);
        assert_eq!(c.failed, 1, "取消不算失败");
        assert_eq!(c.cancelled, 1);
        // 去重保序(首次出现顺序)
        assert_eq!(c.projects, vec!["webapp".to_string(), "api".to_string()]);
        assert_eq!(c.servers, vec!["web-01".to_string(), "api-01".to_string()]);
    }

    #[test]
    fn test_build_digest_empty_day_is_none() {
        let records = vec![rec("2026-09-18 09:00:00", "webapp", "web-01", true, "ok")];
        assert!(build_digest(&records, "2026-09-19").is_none(), "当天无记录不发空日报");
        assert!(build_digest(&[], "2026-09-19").is_none());
    }

    #[test]
    fn test_build_digest_partial_ts_prefix() {
        // 前缀匹配而非日期相等:同一 yyyy-MM-dd 前缀都算当天
        let records = vec![
            rec("2026-09-19 00:00:00", "a", "s1", true, "ok"),
            rec("2026-09-19 23:59:59", "a", "s1", true, "ok"),
            // 前缀相同但并非当天(防 "2026-09-190" 之类的越界形态)
            rec("2026-09-190 10:00:00", "a", "s1", true, "ok"),
        ];
        let c = build_digest(&records, "2026-09-19").unwrap();
        assert_eq!(c.total, 3);
    }

    #[test]
    fn test_build_digest_skips_blank_names() {
        let records = vec![rec("2026-09-19 09:00:00", "  ", "", true, "ok")];
        let c = build_digest(&records, "2026-09-19").unwrap();
        assert_eq!(c.total, 1);
        assert!(c.projects.is_empty(), "空项目名不入列");
        assert!(c.servers.is_empty(), "空服务器名不入列");
    }

    #[test]
    fn test_digest_text_shape() {
        let c = DigestCounts {
            total: 5,
            success: 3,
            failed: 1,
            cancelled: 1,
            projects: vec!["webapp".into(), "api".into()],
            servers: vec!["web-01".into()],
        };
        let (title, body) = digest_text(&c, "2026-09-19");
        assert_eq!(title, "09-19 部署日报");
        assert!(body.contains("共 5 次:成功 3 / 失败 1 / 取消 1"), "{}", body);
        assert!(body.contains("项目:webapp、api"), "{}", body);
        assert!(body.contains("服务器:web-01"), "{}", body);
        // 无取消时不出现「取消」段
        let c2 = DigestCounts { total: 2, success: 2, ..Default::default() };
        let (_, body2) = digest_text(&c2, "2026-09-19");
        assert!(!body2.contains("取消"), "{}", body2);
        assert!(!body2.contains("项目:"), "无项目段: {}", body2);
    }

    #[test]
    fn test_should_fire_rules() {
        let noon = 12 * 3600;
        // 未配置 → 永不发
        assert!(!should_fire(None, noon, "", "2026-09-19"));
        // 到点(整点及之后)→ 发
        assert!(should_fire(Some(12), noon, "", "2026-09-19"));
        assert!(should_fire(Some(12), noon + 3600, "", "2026-09-19"), "过点后补发(应用晚启动)");
        // 未到点 → 不发
        assert!(!should_fire(Some(13), noon, "", "2026-09-19"));
        // 今天已发 → 不发
        assert!(!should_fire(Some(12), noon + 60, "2026-09-19", "2026-09-19"));
        // 昨天发的 → 今天照发
        assert!(should_fire(Some(12), noon, "2026-09-18", "2026-09-19"));
        // 损坏小时值 → 不发
        assert!(!should_fire(Some(24), 23 * 3600, "", "2026-09-19"));
    }

    #[test]
    fn test_digest_state_serde_camel_case() {
        let st = DigestState { last_sent_date: "2026-09-19".into() };
        let v = serde_json::to_value(&st).unwrap();
        assert!(v.get("lastSentDate").is_some(), "{:?}", v);
        let back: DigestState = serde_json::from_value(v).unwrap();
        assert_eq!(back, st);
        // 缺字段 → 默认空串(旧文件/首运行)
        let empty: DigestState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.last_sent_date, "");
    }
}
