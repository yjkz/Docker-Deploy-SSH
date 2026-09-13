//! 容器/Compose 栈日志实时跟随(UPGRADE-PLAN 第二批·阶段九)。
//!
//! 通过事件流式推送:前端监听 Tauri 事件 `manage-logs` 接收日志行,
//! payload 为 `{ streamId, data, eof }`(camelCase,与批次二新增契约统一):
//! `data` 为一行日志(不含换行;尾行可能不带换行);`eof=true` 表示本次流已
//! 结束(被停止/自然结束/出错,原因附在 data),前端停用「实时跟随」开关。
//!
//! 流模型:一个流 = 一条 SSH 连接 + 一个 `docker logs -f` / `compose logs -f`
//! 通道。命令层把取消 mpsc 的 Receiver 移交后台任务,输出逐行经
//! [`SshClient::exec_streaming`] 回调 emit;stop 命令发 `()` → select 立即
//! 取消并主动 close 通道(不等远端);通道自然关闭(容器退出/远端断开)也以
//! eof 收尾。
//!
//! 会话模型:全局单流。`LogsState`(generation 代号)语义与
//! [`crate::manage_stats::StatsState`] 一致 —— 再次 start 自动替换旧流:
//! start 先给**旧代号**的取消句柄发信号(旧任务立即走取消路径关通道返回,
//! 不再持有 SSH 连接与远端 `docker logs -f` 等到 3600s 兜底),输出回调
//! 里代号过期的旧流也已静默。
//!
//! 低耦合:复用 `crate::manage` 的 `connect_server` / `shell_quote`;
//! 零新增依赖(取消通道用 tokio mpsc)。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tokio::sync::mpsc;

use crate::manage::{connect_server, shell_quote};

/// 事件名:`AppBus.on('manage-logs', handler)`。
const LOGS_EVENT: &str = "manage-logs";

/// 单行长度上限(字节):超过截断(防个别超长行撑爆事件载荷)。
const MAX_LINE_BYTES: usize = 16 * 1024;

/// 单流最长执行时长(秒):兜底防远端 hang 死导致流永不结束
/// (`docker logs -f` 正常跟随到 stop 或通道关闭)。
const STREAM_TIMEOUT_SECS: u64 = 3600;

// ===== 全局会话状态 =====

/// 由 tauri Builder `.manage(LogsState::default())` 注册的全局状态。
#[derive(Default)]
pub struct LogsState {
    inner: Arc<Mutex<LogsStateInner>>,
    /// 各代的取消句柄(generation → Sender):stop 按代号精准停当前流;
    /// 旧流退出时按代号移除自己的句柄,防误删新流句柄。
    streams: Arc<Mutex<HashMap<u64, mpsc::Sender<()>>>>,
}

#[derive(Default)]
struct LogsStateInner {
    /// 会话代号:每次 start 递增;旧流任务发现代号过期即自清理。
    generation: u64,
    /// 当前是否有流在运行。
    running: bool,
}

impl LogsStateInner {
    /// 检查代号 `gen` 是否仍为当前流(且在运行)。
    fn is_current(&self, gen: u64) -> bool {
        self.running && self.generation == gen
    }

    /// 流任务退出时清理:仅当当前代号仍是自己时才清除 running(防误清新流)。
    fn finish(&mut self, gen: u64) {
        if self.generation == gen {
            self.running = false;
        }
    }
}

impl LogsState {
    /// 开始新流:递增代号并标记运行中,返回分配到的代号。
    fn begin(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.generation += 1;
        inner.running = true;
        inner.generation
    }

    // 注:原 `end()` 已并入 manage_log_stream_stop 的锁段(第二十批 P2 修复,
    // 推进代号 + 清 running 需与取句柄在同一临界区完成),独立方法随之移除。
}

// ===== 数据结构 =====

/// `manage-logs` 事件 payload(camelCase)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogsPayload {
    /// 流代号:每次 start 递增;前端据此丢弃迟到旧流的事件。
    stream_id: u64,
    /// 一行日志(不含换行);eof 时携带结束原因(用户停止为空串)。
    data: String,
    /// true = 本次流已结束,前端停用实时跟随。
    eof: bool,
}

// ===== Tauri 命令 =====

/// 开始实时日志流的参数(命令 8 参超 clippy 默认阈值,按参数组拆分):
/// 服务器与认证一组,流目标一组。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogStreamTarget {
    /// 流类型:`"container"`(需 `container_id`)/ `"stack"`(需 `compose_file`)
    target: String,
    /// container 流:容器 ID/名称
    container_id: Option<String>,
    /// stack 流:compose 文件完整远端路径(项目目录自动取父目录)
    compose_file: Option<String>,
    /// 起始行数:0 = 全部,>0 = 最近 N 行(与一次性日志同语义)
    tail: Option<u64>,
}

/// 开始实时日志流(全局单流,重复调用自动替换旧流)。
///
/// 数据经 `manage-logs` 事件流式推送,命令本身立即返回(成功与否以事件为准)。
#[tauri::command]
pub async fn manage_log_stream_start(
    app: tauri::AppHandle,
    logs_state: tauri::State<'_, LogsState>,
    server_id: String,
    password_plain: Option<String>,
    tgt: LogStreamTarget,
) -> Result<(), String> {
    if server_id.trim().is_empty() {
        return Err("服务器 ID 不能为空".to_string());
    }
    let tail = tgt.tail.unwrap_or(0);
    // 组装远端跟随命令(路径/容器名经 shell_quote;输出合并 stderr 与既有
    // manage_container_logs / manage_stack_logs 的 `2>&1` 口径一致)
    let cmd = match tgt.target.as_str() {
        "container" => {
            let cid = tgt.container_id.as_deref().unwrap_or("").trim();
            if cid.is_empty() {
                return Err("容器 ID 不能为空".to_string());
            }
            if tail > 0 {
                format!("docker logs -f --tail {} {} 2>&1", tail, shell_quote(cid))
            } else {
                format!("docker logs -f {} 2>&1", shell_quote(cid))
            }
        }
        "stack" => {
            let file = tgt.compose_file.as_deref().unwrap_or("").trim();
            if file.is_empty() {
                return Err("compose 文件路径不能为空".to_string());
            }
            let dir = std::path::Path::new(file)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut c = format!(
                "docker compose -f {} --project-directory {} logs -f",
                shell_quote(file),
                shell_quote(&dir)
            );
            if tail > 0 {
                c.push_str(&format!(" --tail {}", tail));
            }
            c.push_str(" 2>&1");
            c
        }
        other => return Err(format!("未知的日志流类型: {}", other)),
    };

    // 递增代号:旧流任务发现代号过期即自清理;注册本代取消句柄。
    // 第二十批 P2 修复:先给旧代号的取消句柄发信号再注册新句柄 ——
    // 被顶替的旧流立即走 exec_streaming 的取消路径(关通道返回),不再持有
    // SSH 连接与远端 docker logs -f 等到 3600s 兜底;此前旧流只能靠输出行
    // 回调里的 is_current 静默,主体永不退出。try_send 不阻塞(容量 1,旧流
    // 已在等/已退出的两种情况都安全)。锁内完成「信号旧流 + begin + 注册」,
    // 与 stop 的锁段互斥,见 stop 处注释。
    let (generation, cancel_tx, mut cancel_rx) = {
        let mut sm = logs_state.streams.lock().unwrap();
        for old_tx in sm.values() {
            let _ = old_tx.try_send(());
        }
        let generation = logs_state.begin();
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>(1);
        // clone 入表(sender 双持有:表内供 stop/顶替取用,本地随本函数结束 drop)
        sm.insert(generation, cancel_tx.clone());
        (generation, cancel_tx, cancel_rx)
    };
    log::info!("日志流启动: server={} generation={}", server_id, generation);
    drop(cancel_tx); // 本地 sender 立即 drop:取消只经表内句柄发生,语义单一

    let state = Arc::clone(&logs_state.inner);
    let streams = Arc::clone(&logs_state.streams);
    let password = password_plain;

    tokio::spawn(async move {
        let result = run_log_stream(
            &app,
            &state,
            &server_id,
            password.as_deref(),
            &cmd,
            generation,
            &mut cancel_rx,
        )
        .await;
        // 自清理:仅当仍为当前流时清 running;移除本代取消句柄
        state.lock().unwrap().finish(generation);
        // 仅当表中该代号仍是自己时移除(get 后 remove 双查防误删)
        {
            let mut sm = streams.lock().unwrap();
            if sm.get(&generation).is_some() {
                sm.remove(&generation);
            }
        }
        let (data, warn) = match result {
            Ok(reason) => (reason, false),
            Err(e) => (format!("日志流已中断: {}", e), true),
        };
        if warn {
            log::warn!("日志流异常结束: generation={} 错误: {}", generation, data);
        } else {
            log::info!(
                "日志流结束: generation={} 原因: {}",
                generation,
                if data.is_empty() { "用户停止" } else { &data }
            );
        }
        let _ = app.emit(
            LOGS_EVENT,
            LogsPayload {
                stream_id: generation,
                data,
                eof: true,
            },
        );
    });
    Ok(())
}

/// 日志流主体:建连 → `exec_streaming`(select 取消)→ 返回结束原因
/// (`Ok(空串)` = 用户主动停止;`Ok(非空)` = 自然结束的说明;`Err` = 中断)。
async fn run_log_stream(
    app: &tauri::AppHandle,
    state: &Arc<Mutex<LogsStateInner>>,
    server_id: &str,
    password_plain: Option<&str>,
    cmd: &str,
    generation: u64,
    cancel_rx: &mut mpsc::Receiver<()>,
) -> Result<String, String> {
    // 建连(15s 超时;含 TOFU/私钥口令解析,与 manage 系列同口径)
    let (_server, mut client) = connect_server(server_id, password_plain).await?;

    // 输出回调:逐行 emit;会话已被替换/停止后静默(任务随后退出)
    let app_for_cb = app.clone();
    let state_for_cb = Arc::clone(state);
    let gen = generation;
    let mut on_line = move |line: &str| {
        if !state_for_cb.lock().unwrap().is_current(gen) {
            return;
        }
        // 超长行截断(按字节切,lossy 转换容错 UTF-8 边界)
        let truncated = line.len() > MAX_LINE_BYTES;
        let mut data = String::from_utf8_lossy(&line.as_bytes()[..line.len().min(MAX_LINE_BYTES)])
            .into_owned();
        if truncated {
            data.push_str("…(超长行截断)");
        }
        let _ = app_for_cb.emit(
            LOGS_EVENT,
            LogsPayload {
                stream_id: gen,
                data,
                eof: false,
            },
        );
    };

    // 整体兜底超时:远端 hang 死时最多 1 小时强制结束(stop 场景不受影响)
    let mut exit_code: i32 = -1;
    let stream_fut = client.exec_streaming(cmd, cancel_rx, &mut exit_code, &mut on_line);
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_secs(STREAM_TIMEOUT_SECS),
        stream_fut,
    )
    .await
    .map_err(|_| "日志流超时(3600 秒)自动结束".to_string())??;

    if cancelled {
        return Ok(String::new()); // 用户停止:eof data 为空
    }
    // 自然结束:容器退出/通道关闭;退出码非 0 时给针对性提示
    if exit_code != 0 {
        return Ok(match exit_code {
            127 => "远端命令未找到(退出码 127),日志流结束".to_string(),
            126 => "远端命令不可执行(退出码 126),日志流结束".to_string(),
            code => format!("远端日志流结束(退出码 {})", code),
        });
    }
    Ok("日志流已结束".to_string())
}

/// 停止当前实时日志流(幂等;后端已自停时仍返回 Ok)。
///
/// 向当前流的取消通道发送 `()` → select 立即取消并主动 close 通道;
/// eof 事件由流任务统一 emit。
#[tauri::command]
pub async fn manage_log_stream_stop(logs_state: tauri::State<'_, LogsState>) -> Result<(), String> {
    // 第二十批 P2 修复:stop 与 start 是两个独立 invoke,原实现「读代号 → 取
    // 句柄 → end」三步间无锁,快速「停旧开新」时可能读到 begin() 之后的新
    // 代号,把取消信号发给**新流**(新流刚启动就被停,旧流反而漏网)。
    // 现改为:锁内一次性完成「快照代号 + 取句柄 + 推进状态」,锁外发信号 ——
    // 与 start 的「信号旧流 + begin + 注册」锁段互斥后,两命令交错只有两种
    // 全序(stop 全先于 start / start 全先于 stop),各自语义都正确。
    let tx = {
        let mut inner = logs_state.inner.lock().unwrap();
        if !inner.running {
            return Ok(()); // 幂等:无运行中流
        }
        let gen = inner.generation;
        // end 语义:递增代号使旧任务退出判定生效,清 running
        inner.generation += 1;
        inner.running = false;
        let tx = logs_state.streams.lock().unwrap().get(&gen).cloned();
        log::info!("日志流停止: generation={}", gen);
        tx
    };
    if let Some(tx) = tx {
        // 锁外发信号(mpsc 容量 1;接收端已退出时 send 失败,安全忽略)
        let _ = tx.send(()).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generation_guard() {
        let mut inner = LogsStateInner::default();
        // 初始无流:任何代号都非当前
        assert!(!inner.is_current(1));
        // 运行中仅当前代号有效
        inner.generation = 1;
        inner.running = true;
        assert!(inner.is_current(1));
        assert!(!inner.is_current(2));
        // 旧流退出:代号过期,finish 不得清掉新流的 running(防误清新流)
        inner.generation = 2;
        inner.running = true;
        inner.finish(1);
        assert!(inner.running, "旧代号 finish 不应清 running");
        inner.finish(2);
        assert!(!inner.running);
    }

    #[test]
    fn test_begin_end_monotonic() {
        let st = LogsState::default();
        let g1 = st.begin();
        // 重复 start 自动替换旧流:代号严格递增
        let g2 = st.begin();
        assert!(g2 > g1);
        // stop 并入锁段后(第二十批 P2),begin 之间无需显式 end —— 代号
        // 严格递增由 begin 自身保证
        let g3 = st.begin();
        assert!(g3 > g2);
    }

    #[test]
    fn test_start_signals_old_stream_cancel() {
        // 第二十批 P2 修复回归:新流 start 时旧代号的取消句柄必须收到信号 ——
        // 被顶替的旧流立即走取消路径退出,不再持有连接等到 3600s 兜底;
        // 且新流的句柄不得被误伤。receiver 全程保活(模拟流任务持有)。
        use tokio::sync::mpsc::error::TryRecvError;
        let st = LogsState::default();
        // 第一代流:begin + 注册句柄(receiver 模拟流任务,跨块保活)
        let (mut rx1, g1) = {
            let mut sm = st.streams.lock().unwrap();
            let g = st.begin();
            let (tx, rx) = mpsc::channel::<()>(1);
            sm.insert(g, tx);
            drop(sm);
            (rx, g)
        };
        // 第二次 start(与命令层同款锁段):信号旧流 → begin → 注册新句柄
        let (mut rx2, g2) = {
            let mut sm = st.streams.lock().unwrap();
            for old_tx in sm.values() {
                let _ = old_tx.try_send(());
            }
            let g = st.begin();
            let (tx, rx) = mpsc::channel::<()>(1);
            sm.insert(g, tx);
            drop(sm);
            (rx, g)
        };
        assert!(g2 > g1, "代号严格递增");
        // 旧流收到了恰好一次取消信号(唤醒退出)
        assert_eq!(rx1.try_recv(), Ok(()), "被顶替的旧流应收到取消信号");
        assert_eq!(rx1.try_recv(), Err(TryRecvError::Empty), "旧流只收到一次信号");
        // 新流未收到任何信号,且句柄仍可用
        assert_eq!(rx2.try_recv(), Err(TryRecvError::Empty), "新流不得被旧流的取消信号误伤");
        let new_tx = st.streams.lock().unwrap().get(&g2).cloned();
        assert!(new_tx.is_some(), "新代号句柄已注册");
        assert!(new_tx.unwrap().try_send(()).is_ok(), "新流句柄仍可发送(stop 路径可用)");
    }

    #[test]
    fn test_stream_handle_registry_cleanup() {
        // 流任务退出路径:仅当表中该代号仍是自己时移除(防误删新流句柄)
        let st = LogsState::default();
        let mut sm = st.streams.lock().unwrap();
        let (tx1, _rx1) = mpsc::channel::<()>(1);
        let (tx2, _rx2) = mpsc::channel::<()>(1);
        sm.insert(1, tx1);
        sm.insert(2, tx2);
        // 旧流(代号 1)退出:remove 自己
        if sm.get(&1).is_some() {
            sm.remove(&1);
        }
        assert!(sm.get(&1).is_none(), "旧代号句柄已移除");
        assert!(sm.get(&2).is_some(), "新代号句柄不受影响");
    }
}
