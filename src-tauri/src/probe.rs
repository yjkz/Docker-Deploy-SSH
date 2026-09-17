//! 服务器定时探活(第十七批):按设置间隔 TCP 探活全部已配置服务器,
//! 状态**翻转**时(在线→离线 / 离线→恢复)经通知中心分发桌面/邮件通知。
//!
//! ## 设计要点
//!
//! - **TCP 连接探活**:只连 `host:port`(SSH 端口),不做 SSH 认证 —— 轻量、
//!   不碰密钥/密码、不触发 TOFU;端口可连 = 服务器在线的最强信号。
//! - **状态翻转才通知**:维护每台服务器的最近状态(HashMap),仅当与上轮
//!   不同时发通知(`probe` 事件类型,订阅开关 `notify.events.on_probe`,
//!   默认关)—— 探活每 N 分钟一轮,不做每轮轰炸。
//! - **进程内单任务**:`Mutex<Option<JoinHandle>>` + generation 守卫,设置变更
//!   时停旧起新(同 manage_stats 的会话替换模式);应用退出随运行时一起结束。
//! - **无新命令**:设置保存(`app_settings_set`)与启动(setup)时后端自行
//!   调 [`sync_from_settings`],前端零新增 invoke;前端设置中心只多一个
//!   「探活间隔(分钟,0=关)」字段。
//! - **探测结果不落盘**:探活状态是易变的运行时观测,重启后首轮即重建
//!   (首轮建立基线,不通知 —— 避免每次启动都弹一轮通知)。
//!
//! ## 资源阈值告警(第二十四批,同模块第二任务)
//!
//! - **采样**:按 `AppSettings.alert_interval_mins`(0=关,默认关)独立任务,
//!   逐台 SSH `host_metrics_cmd`(复用 manage.rs 的采样命令与解析,
//!   `host_percent_of` 折算磁盘/内存/CPU 百分比);采样走完整 SSH 连接
//!   (经 `commands::connect_server`,凭据解析不变量与全站一致)。
//! - **防抖**:连续 **2 轮**超阈才告警(瞬态尖峰不误报);恢复后**单轮**
//!   回落即发「已恢复」。状态机为纯函数(`evaluate_alert_flips`),便于单测。
//! - **互斥**:每台采样前取 `acquire_remote_op`(RAII,逐台短持有)——与
//!   部署/回滚/迁移互斥(硬约束 9);被拒跳过该台本轮(不打扰,下轮重试)。
//! - **通知**:事件类型 `alert`(`notify.events.on_alert`,默认关);
//!   阈值与间隔在设置中心配(`alert_*` 系列字段)。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tauri::AppHandle;

/// 探活状态存储:server_id → 最近一轮在线(true=可连)。
/// 首轮(不在表里)只建立基线,不通知。
static LAST_STATE: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);

/// 探活循环句柄(设置变更时停旧起新;tauri 异步运行时的 JoinHandle,含 abort)。
static PROBE_TASK: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> = Mutex::new(None);
/// TCP 连接超时(秒):端口探活不需要长等待。
const PROBE_TIMEOUT_SECS: u64 = 5;

/// 按当前设置同步探活任务:`probe_interval_mins = 0` → 停止;>0 → (重)启动。
/// 在 setup(应用启动)与 `app_settings_set`(设置保存)时调用。
pub fn sync_from_settings(app: &AppHandle) {
    let interval_mins = crate::config::load_app_settings().probe_interval_mins;
    let mut guard = match PROBE_TASK.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    if let Some(handle) = guard.take() {
        handle.abort(); // 旧任务立即停止(下一轮 tick 也不会跑)
    }
    if interval_mins == 0 {
        // 关闭探活:清状态基线,下次再开时按首轮重新建线
        if let Ok(mut st) = LAST_STATE.lock() {
            *st = None;
        }
        log::info!("定时探活已关闭");
        return;
    }
    let app = app.clone();
    let interval = Duration::from_secs(u64::from(interval_mins) * 60);
    let handle = tauri::async_runtime::spawn(async move {
        log::info!("定时探活已启动: 间隔 {} 分钟", interval_mins);
        // 启动即先探一轮(建立基线),之后按间隔轮询
        probe_once(&app).await;
        let mut ticker = tokio::time::interval(interval);
        // 首个 tick 立即触发,与上面手动首轮重复 → 跳过首个
        ticker.tick().await;
        loop {
            ticker.tick().await;
            probe_once(&app).await;
        }
    });
    *guard = Some(handle);
}

/// 一轮探活:逐台 TCP 连 `host:port`(5s 超时),与上轮状态比对,
/// 翻转时发 `probe` 通知。逐台串行(台数少,且避免瞬时并发连接风暴)。
/// 锁纪律:状态更新在独立同步块内完成(**锁内不 await**),翻转清单
/// 收集后在锁外发通知(notify::fire 是异步)。
async fn probe_once(app: &AppHandle) {
    let servers = match crate::config::load_config() {
        Ok(cfg) => cfg.servers,
        Err(e) => {
            log::warn!("探活跳过本轮:读取配置失败: {}", e);
            return;
        }
    };
    // 先探全部(锁外),再一次锁内更新基线并收集翻转
    let mut results: Vec<(String, String, bool)> = Vec::with_capacity(servers.len());
    for s in &servers {
        let online = tcp_probe(&s.host, s.port).await;
        results.push((s.id.clone(), s.name.clone(), online));
    }
    let flips: Vec<(String, bool)> = {
        let mut last = match LAST_STATE.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let map = last.get_or_insert_with(HashMap::new);
        results
            .into_iter()
            .filter_map(|(id, name, online)| {
                let flipped = match map.get(&id) {
                    Some(prev) => *prev != online, // 翻转
                    None => false,                 // 首轮基线,不通知
                };
                map.insert(id, online);
                flipped.then_some((name, online))
            })
            .collect()
        // 锁守卫在此块尾 drop,不跨 await
    };
    for (name, online) in flips {
        let (title, body) = if online {
            ("服务器恢复在线".to_string(), format!("「{}」恢复可连接", name))
        } else {
            (
                "服务器离线".to_string(),
                format!("「{}」连续探活失败(TCP {} 秒超时),请检查服务器或网络", name, PROBE_TIMEOUT_SECS),
            )
        };
        log::warn!("探活状态翻转: {} → {}", name, if online { "在线" } else { "离线" });
        crate::notify::fire(app.clone(), "probe", title, body).await;
    }
}

/// TCP 连接探活(纯连接,不发数据;成功 = 端口可连)。
async fn tcp_probe(host: &str, port: u16) -> bool {
    let addr = format!("{}:{}", host, port);
    match tokio::time::timeout(
        Duration::from_secs(PROBE_TIMEOUT_SECS),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            log::info!("探活失败 {} : {}", addr, e);
            false
        }
        Err(_) => false, // 超时
    }
}

// ===== 资源阈值告警(第二十四批)=====

/// 告警状态存储:server_id → 各维度最近轮次状态(miss=连续超阈轮数 /
/// 是否已告警)。
static ALERT_STATE: Mutex<Option<HashMap<String, AlertServerState>>> = Mutex::new(None);

/// 告警任务句柄(设置变更时停旧起新,与探活任务同模式、独立实例)。
static ALERT_TASK: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> = Mutex::new(None);

/// 连续超阈多少轮才告警(防瞬态尖峰误报;恢复判定不设防抖,单轮回落即恢复)。
const ALERT_STREAK: u32 = 2;

/// 单台服务器的告警跟踪状态(按维度独立)。
#[derive(Debug, Clone, Default, PartialEq)]
struct AlertServerState {
    /// 各维度:连续 miss 轮数(>= ALERT_STREAK 已含「已告警」语义)
    cpu_streak: u32,
    mem_streak: u32,
    disk_streak: u32,
    /// 各维度:是否处于已告警态(供恢复通知判定)
    cpu_alerted: bool,
    mem_alerted: bool,
    disk_alerted: bool,
}

/// 单个维度的判定结果:本轮应发出的告警/恢复动作。
#[derive(Debug, Clone, PartialEq)]
enum AlertAction {
    /// 新进入超阈(连续 N 轮)——发告警
    Fire(String),
    /// 从已告警态恢复——发恢复通知
    Recover(String),
}

/// 纯函数:对单维度(当前值、阈值、连续轮数状态)推演新状态与动作。
///
/// 规则:
/// - 当前值缺失(None)→ 状态保持不变(采样不可用不等于恢复);
/// - 阈值 == 0 → 该维度关闭,状态清零且无动作;
/// - 超阈 → streak+1;从「未告警」跨过 `ALERT_STREAK` 的那一轮发出告警;
/// - 回落 → streak 清零;若此前已告警则发恢复。
fn eval_dimension(
    current: Option<f64>,
    threshold: u32,
    streak: &mut u32,
    alerted: &mut bool,
    label: &str,
) -> Option<AlertAction> {
    if threshold == 0 {
        *streak = 0;
        *alerted = false;
        return None;
    }
    let cur = current?; // 采样缺失:维持既有状态
    if cur >= f64::from(threshold) {
        *streak = streak.saturating_add(1);
        if *streak >= ALERT_STREAK && !*alerted {
            *alerted = true;
            return Some(AlertAction::Fire(label.to_string()));
        }
        None
    } else {
        *streak = 0;
        if *alerted {
            *alerted = false;
            return Some(AlertAction::Recover(label.to_string()));
        }
        None
    }
}

/// 纯函数:单台一轮采样的全部维度判定(供单测与告警任务共用)。
/// 返回本轮应发出的动作列表(label 已含可读维度名)。
fn evaluate_alert_flips(
    state: &mut AlertServerState,
    percent: &crate::manage::HostPercent,
    thresholds: (u32, u32, u32),
) -> Vec<AlertAction> {
    let mut actions = Vec::new();
    let (cpu_th, mem_th, disk_th) = thresholds;
    if let Some(a) = eval_dimension(percent.cpu, cpu_th, &mut state.cpu_streak, &mut state.cpu_alerted, "CPU") {
        actions.push(a);
    }
    if let Some(a) = eval_dimension(percent.mem.map(|v| v as f64), mem_th, &mut state.mem_streak, &mut state.mem_alerted, "内存") {
        actions.push(a);
    }
    if let Some(a) = eval_dimension(percent.disk.map(|v| v as f64), disk_th, &mut state.disk_streak, &mut state.disk_alerted, "磁盘") {
        actions.push(a);
    }
    actions
}

/// 按当前设置同步告警任务:`alert_interval_mins = 0` → 停止;>0 → (重)启动。
/// 在 setup 与 `app_settings_set` 时调用(与探活任务并列、独立)。
pub fn sync_alert_from_settings(app: &AppHandle) {
    let interval_mins = crate::config::load_app_settings().alert_interval_mins;
    let mut guard = match ALERT_TASK.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    if let Some(handle) = guard.take() {
        handle.abort();
    }
    if interval_mins == 0 {
        if let Ok(mut st) = ALERT_STATE.lock() {
            *st = None;
        }
        log::info!("资源告警已关闭");
        return;
    }
    let app = app.clone();
    let interval = Duration::from_secs(u64::from(interval_mins) * 60);
    let handle = tauri::async_runtime::spawn(async move {
        log::info!("资源告警已启动: 间隔 {} 分钟", interval_mins);
        // 启动即先采一轮(建立基线),之后按间隔轮询(与探活同节奏)
        alert_once(&app).await;
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // 跳过首个立即 tick
        loop {
            ticker.tick().await;
            alert_once(&app).await;
        }
    });
    *guard = Some(handle);
}

/// 一轮资源采样:逐台 SSH 采 `host_metrics_cmd`,按维度推演并分发 alert 通知。
async fn alert_once(app: &AppHandle) {
    let settings = crate::config::load_app_settings();
    let thresholds = (
        settings.alert_cpu_percent,
        settings.alert_mem_percent,
        settings.alert_disk_percent,
    );
    let servers = match crate::config::load_config() {
        Ok(cfg) => cfg.servers,
        Err(e) => {
            log::warn!("资源告警跳过本轮:读取配置失败: {}", e);
            return;
        }
    };
    for s in &servers {
        // 互斥(硬约束 9):逐台短持有;部署/回滚/迁移进行中被拒 → 跳过该台本轮
        let _guard = match crate::commands::acquire_remote_op() {
            Ok(g) => g,
            Err(_) => {
                log::info!("资源告警跳过「{}」本轮:已有远程操作进行中", s.name);
                continue;
            }
        };
        let percent = match sample_server_percent(s).await {
            Ok(p) => p,
            Err(e) => {
                log::warn!("资源采样失败「{}」: {}", s.name, e);
                continue;
            }
        };
        // 锁内推演(不跨 await),锁外发通知
        let actions: Vec<AlertAction> = {
            let mut st = match ALERT_STATE.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            let map = st.get_or_insert_with(HashMap::new);
            let entry = map.entry(s.id.clone()).or_default();
            evaluate_alert_flips(entry, &percent, thresholds)
        };
        for action in actions {
            let (title, body) = match action {
                AlertAction::Fire(label) => (
                    format!("{} 资源告警:{}", s.name, label),
                    format!(
                        "「{}」{} 使用率连续 {} 轮超过阈值(CPU {}/内存 {}/磁盘 {} 阈值可在设置中心调整)",
                        s.name, label, ALERT_STREAK,
                        thresholds.0, thresholds.1, thresholds.2
                    ),
                ),
                AlertAction::Recover(label) => (
                    format!("{} 资源恢复正常:{}", s.name, label),
                    format!("「{}」{} 使用率已回落至阈值以下", s.name, label),
                ),
            };
            log::warn!("资源告警: {}", title);
            crate::notify::fire(app.clone(), "alert", title, body).await;
        }
    }
}

/// 单台服务器采样:完整 SSH 连接(凭据经 connect_server 内部解析,不变量
/// 与全站一致)+ `host_metrics_cmd` 一次往返 → 百分比三元组。
async fn sample_server_percent(
    server: &crate::config::ServerConfig,
) -> Result<crate::manage::HostPercent, String> {
    let (_, mut client) = crate::manage::connect_server(&server.id, None).await?;
    let (code, out) = crate::manage::with_timeout(
        crate::manage::EXEC_TIMEOUT_SECS + 5,
        "资源采样超时",
        "请检查服务器网络后重试",
        async { crate::ssh::exec_collect(&mut client, &crate::manage::host_metrics_cmd()).await },
    )
    .await?;
    if code != 0 && out.trim().is_empty() {
        return Err(format!("采样命令退出码 {} 且输出为空", code));
    }
    Ok(crate::manage::host_percent_of(&crate::manage::parse_host_metrics(&out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机回环端口探活:监听中的端口 = true;无监听的端口大概率 false
    /// (端口可能被其他进程占用,只断言「已监听端口可探通」这个方向)。
    #[tokio::test]
    async fn test_tcp_probe_listening_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(tcp_probe("127.0.0.1", port).await);
    }

    /// 不可解析主机在超时内返回 false(不 panic、不挂起)。
    #[tokio::test]
    async fn test_tcp_probe_bad_host_returns_false() {
        // .invalid 是保留 TLD,永不解析
        assert!(!tcp_probe("nonexistent-host.invalid", 22).await);
    }

    // ===== 资源阈值告警纯函数(第二十四批)=====

    fn pct(cpu: Option<f64>, mem: Option<u64>, disk: Option<u64>) -> crate::manage::HostPercent {
        crate::manage::HostPercent { cpu, mem, disk }
    }

    #[test]
    fn test_eval_dimension_debounce_fire_on_second_streak() {
        let (mut streak, mut alerted) = (0u32, false);
        // 第 1 轮超阈:不告警(防抖)
        assert_eq!(eval_dimension(Some(95.0), 90, &mut streak, &mut alerted, "CPU"), None);
        assert_eq!(streak, 1);
        // 第 2 轮超阈:告警
        let act = eval_dimension(Some(96.0), 90, &mut streak, &mut alerted, "CPU");
        assert_eq!(act, Some(AlertAction::Fire("CPU".into())));
        assert!(alerted);
        // 第 3 轮仍超阈:不重复告警
        assert_eq!(eval_dimension(Some(97.0), 90, &mut streak, &mut alerted, "CPU"), None);
    }

    #[test]
    fn test_eval_dimension_recover_on_single_dip() {
        // 预置已告警态
        let (mut streak, mut alerted) = (5u32, true);
        // 单轮回落即恢复(不防抖)
        let act = eval_dimension(Some(50.0), 90, &mut streak, &mut alerted, "内存");
        assert_eq!(act, Some(AlertAction::Recover("内存".into())));
        assert!(!alerted);
        assert_eq!(streak, 0);
        // 再次回落后不重复发
        assert_eq!(eval_dimension(Some(40.0), 90, &mut streak, &mut alerted, "内存"), None);
    }

    #[test]
    fn test_eval_dimension_annulled_above_threshold() {
        // 超阈后回落到阈值下、但未达 2 连续轮 → 状态清零,无告警(防抖也防抖掉)
        let (mut streak, mut alerted) = (1u32, false);
        assert_eq!(eval_dimension(Some(50.0), 90, &mut streak, &mut alerted, "磁盘"), None);
        assert_eq!(streak, 0);
        // 再超阈重新从 1 计(不因上一轮的 1 而立即告警)
        assert_eq!(eval_dimension(Some(95.0), 90, &mut streak, &mut alerted, "磁盘"), None);
        assert_eq!(streak, 1);
    }

    #[test]
    fn test_eval_dimension_missing_sample_keeps_state() {
        // 采样缺失(None)→ 状态保持,不视作恢复
        let (mut streak, mut alerted) = (2u32, true);
        assert_eq!(eval_dimension(None, 90, &mut streak, &mut alerted, "CPU"), None);
        assert!(alerted);
        assert_eq!(streak, 2);
    }

    #[test]
    fn test_eval_dimension_threshold_zero_disables() {
        // 阈值 0 = 该项关闭:状态清零,无动作(即便值很高)
        let (mut streak, mut alerted) = (3u32, true);
        assert_eq!(eval_dimension(Some(99.0), 0, &mut streak, &mut alerted, "CPU"), None);
        assert_eq!(streak, 0);
        assert!(!alerted);
    }

    #[test]
    fn test_evaluate_alert_flips_multi_dimension() {
        // 首次两个维度同时超阈(第 1 轮)→ 无告警;第 2 轮 → 两条 Fire
        let mut st = AlertServerState::default();
        let th = (90, 90, 0); // 磁盘关闭
        let a1 = evaluate_alert_flips(&mut st, &pct(Some(95.0), Some(92), Some(99)), th);
        assert!(a1.is_empty());
        let a2 = evaluate_alert_flips(&mut st, &pct(Some(95.0), Some(92), Some(99)), th);
        assert_eq!(a2.len(), 2);
        assert!(a2.contains(&AlertAction::Fire("CPU".into())));
        assert!(a2.contains(&AlertAction::Fire("内存".into())));
        // 磁盘阈值 0:超阈也不参与
        assert!(!a2.iter().any(|a| matches!(a, AlertAction::Fire(l) if l == "磁盘")));
    }
}
