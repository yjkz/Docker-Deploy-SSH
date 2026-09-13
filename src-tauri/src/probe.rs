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
}
