//! 容器交互式 Exec 终端模块(C 阶段追加)。
//!
//! 通过 [`SshClient::exec_interactive`] 打开 PTY 通道执行
//! `docker exec -it <container> <shell>`,并把通道交给后台任务:
//! - 读任务:持续 `wait()` 通道输出,经事件 `manage-exec-output` 推送给前端,
//!   payload 为 `{ session_id, data, eof }`;通道关闭(eof=true)时自清理会话。
//! - 写任务:持有 `channel.make_writer()`(russh 0.46 中不借用通道、可移交),
//!   从 mpsc 队列取字节写入远端 stdin。命令层只与 mpsc Sender 打交道,
//!   避免在持 std::sync::Mutex 锁期间 await。
//! - resize 经 mpsc 转发给读任务,由其调用 `channel.window_change`(真实实现)。
//!
//! 低耦合:复用 `crate::manage` 已 pub(crate) 的 `connect_server` / `shell_quote`。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use tauri::{Emitter, Manager};

use crate::manage::{connect_server, shell_quote};
use crate::ssh::SshClient;

/// 事件名:`manage-exec-output`。
const EXEC_EVENT: &str = "manage-exec-output";

/// 读任务内部事件队列容量(写/resize 指令)。
const CTRL_BUFFER: usize = 64;

// ===== 会话状态 =====

/// 由 tauri Builder `.manage(ExecState::default())` 注册的全局状态。
#[derive(Default)]
pub struct ExecState {
    sessions: Mutex<HashMap<String, ExecSession>>,
}

/// 一个存活的交互式终端会话。
///
/// `client` 持有已认证的 SSH 连接(保活,通道依赖它);其余均为与后台
/// 任务通信的 mpsc 句柄(全部 Send,std::sync::Mutex 满足 tauri State
/// 的 Send + Sync 要求;持锁时只 clone 句柄,绝不在持锁状态下 await)。
struct ExecSession {
    /// 保活的 SSH 连接(关闭连接即断开终端)。
    _client: SshClient,
    /// 向写任务投递 stdin 字节。
    write_tx: mpsc::Sender<Vec<u8>>,
    /// 向读任务投递终端 resize 请求 (cols, rows)。
    resize_tx: mpsc::Sender<(u32, u32)>,
    /// 通知读任务主动关闭通道(用户点"停止")。
    stop_tx: mpsc::Sender<()>,
}

/// 会话 ID 生成:时间戳 + 进程内计数器,保证唯一(不引入 uuid 依赖)。
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);
fn next_session_id() -> String {
    let n = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("exec-{}-{}", ts, n)
}

/// `manage-exec-output` 事件 payload(前端按此结构监听)。
#[derive(Clone, Serialize)]
pub struct ExecOutputPayload {
    pub session_id: String,
    /// 本次推送的输出文本(from_utf8_lossy)。
    pub data: String,
    /// true 表示通道已关闭(会话结束),前端据此提示并停用输入。
    pub eof: bool,
    /// 会话结束原因(eof=true 时携带,数据帧为 None):写失败 / 远端退出码 /
    /// 通道异常关闭,前端显示给用户,不再只给一个无声的「会话已结束」。
    pub error: Option<String>,
}

// ===== Tauri 命令 =====

/// 启动容器交互式终端:建连 → PTY → exec,返回 session_id。
/// 输出经事件 `manage-exec-output` 异步推送。
#[tauri::command]
pub async fn manage_exec_start(
    app: tauri::AppHandle,
    exec_state: tauri::State<'_, ExecState>,
    server_id: String,
    password_plain: Option<String>,
    container_id: String,
    shell: Option<String>,
) -> Result<String, String> {
    let shell = shell.unwrap_or_else(|| "bash".to_string());
    let cmd = format!(
        "docker exec -it {} {}",
        shell_quote(&container_id),
        shell_quote(&shell)
    );

    // 建连(每次 start 新建连接,随会话保活,随会话结束丢弃)
    let (_, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let channel = client.exec_interactive(&cmd, 120, 32).await?;

    // 先取写句柄(不借用通道,可移交),再把通道移交给读任务
    let writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send> = Box::new(channel.make_writer());
    let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(CTRL_BUFFER);
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u32, u32)>(CTRL_BUFFER);
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);

    let session_id = next_session_id();
    log::info!(
        "交互式终端启动: session={} container={} shell={}",
        session_id,
        container_id,
        shell
    );

    // 写任务错误槽位:write_all 失败时记录原因,读任务退出时并入 eof 事件
    // (写失败意味着会话已不可用,连接层随即将关闭,wait() 返回 None)
    let write_err: std::sync::Arc<Mutex<Option<String>>> =
        std::sync::Arc::new(Mutex::new(None));

    // 写任务:队列字节 → 通道 stdin;写失败记录原因;队列关闭时发 EOF
    let sid_w = session_id.clone();
    let write_err_w = std::sync::Arc::clone(&write_err);
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(bytes) = write_rx.recv().await {
            if let Err(e) = writer.write_all(&bytes).await {
                let reason = format!("写入远端失败: {}", e);
                log::warn!("交互式终端写入失败: session={} {}", sid_w, reason);
                *write_err_w.lock().unwrap() = Some(reason);
                break;
            }
            let _ = writer.flush().await;
        }
        let _ = writer.shutdown().await; // 向远端发送 EOF
    });

    // 先注册会话,再启动读任务:若通道立即关闭(如容器无该 shell、docker exec
    // 快速失败),读任务的 remove 才能命中,避免插入前清理落空留下僵尸会话
    exec_state
        .sessions
        .lock()
        .unwrap()
        .insert(session_id.clone(), ExecSession {
            _client: client,
            write_tx,
            resize_tx,
            stop_tx,
        });

    // 读任务:wait() 输出 → 事件推送;resize/stop 经 select 并发处理;
    // 结束时组装原因(用户停止 / 远端退出码 / 写失败 / 通道关闭)随 eof 事件
    // 推给前端,并移除会话。注意 russh 0.46 客户端在收到远端 CHANNEL_CLOSE
    // 或连接断开时并不投递 ChannelMsg::Close,而是移除内部 ChannelRef 使
    // wait() 返回 None(client/encrypted.rs 的 CHANNEL_CLOSE 分支),故正常
    // 的「远端进程退出」走的是 None 分支。
    let sid = session_id.clone();
    tokio::spawn(async move {
        let mut channel = channel;
        let mut exit_status: Option<u32> = None; // 远端 ExitStatus(正常 exit / exec 失败均有)
        let mut user_stop = false; // 用户点「停止」触发的退出(前端已自清,无需再提示)
        loop {
            tokio::select! {
                msg = channel.wait() => match msg {
                    Some(russh::ChannelMsg::Data { ref data })
                    | Some(russh::ChannelMsg::ExtendedData { ref data, .. }) => {
                        let payload = ExecOutputPayload {
                            session_id: sid.clone(),
                            data: String::from_utf8_lossy(data).into_owned(),
                            eof: false,
                            error: None,
                        };
                        let _ = app.emit(EXEC_EVENT, payload);
                    }
                    Some(russh::ChannelMsg::ExitStatus { exit_status: code }) => {
                        exit_status = Some(code);
                    }
                    Some(russh::ChannelMsg::Eof)
                    | Some(russh::ChannelMsg::Success) => {
                        // Eof:远端不再输出但通道可能仍收尾;Success:忽略
                    }
                    Some(russh::ChannelMsg::Close) => break,
                    None => break,
                    _ => {}
                },
                Some((cols, rows)) = resize_rx.recv() => {
                    let _ = channel.window_change(cols, rows, 0, 0).await;
                }
                _ = stop_rx.recv() => {
                    user_stop = true;
                    let _ = channel.close().await;
                    break;
                }
            }
        }
        // 会话结束原因:写任务失败 > 远端退出码 > 通道异常关闭;用户主动停止不提示
        let reason = if user_stop {
            None
        } else if let Some(err) = write_err.lock().unwrap().clone() {
            Some(format!("终端输入通道异常({})", err))
        } else if let Some(code) = exit_status {
            if code == 127 {
                Some(format!(
                    "远端命令未找到(容器内可能没有 {},可在 Shell 下拉框切换为 sh 后重试)",
                    shell
                ))
            } else {
                Some(format!("远端进程已退出 (退出码 {})", code))
            }
        } else {
            Some("SSH 通道已关闭(网络中断或远端关闭了会话)".to_string())
        };
        log::info!(
            "交互式终端结束: session={} 原因={}",
            sid,
            reason.as_deref().unwrap_or("用户主动关闭")
        );
        // 通道关闭:通知前端会话结束(带原因)并移除会话
        let _ = app.emit(
            EXEC_EVENT,
            ExecOutputPayload { session_id: sid.clone(), data: String::new(), eof: true, error: reason },
        );
        app.state::<ExecState>().sessions.lock().unwrap().remove(&sid);
    });

    Ok(session_id)
}

/// 向会话 stdin 写入数据(含回车 `\r`)。
#[tauri::command]
pub async fn manage_exec_write(
    exec_state: tauri::State<'_, ExecState>,
    session_id: String,
    data: String,
) -> Result<(), String> {
    let tx = {
        let map = exec_state.sessions.lock().unwrap();
        match map.get(&session_id) {
            Some(s) => s.write_tx.clone(),
            None => return Err(format!("交互式终端会话不存在或已结束: {}", session_id)),
        }
    };
    tx.send(data.into_bytes())
        .await
        .map_err(|_| format!("交互式终端会话不存在或已结束: {}", session_id))
}

/// 调整终端大小(转发给读任务调用 russh `window_change`,真实实现)。
#[tauri::command]
pub async fn manage_exec_resize(
    exec_state: tauri::State<'_, ExecState>,
    session_id: String,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    let tx = {
        let map = exec_state.sessions.lock().unwrap();
        match map.get(&session_id) {
            Some(s) => s.resize_tx.clone(),
            None => return Err(format!("交互式终端会话不存在或已结束: {}", session_id)),
        }
    };
    tx.send((cols, rows))
        .await
        .map_err(|_| format!("交互式终端会话不存在或已结束: {}", session_id))
}

/// 停止会话:关闭通道、中止读循环、清理会话与写任务。
#[tauri::command]
pub async fn manage_exec_stop(
    exec_state: tauri::State<'_, ExecState>,
    session_id: String,
) -> Result<(), String> {
    // 从 map 取出会话:句柄随会话一起 drop(写队列关闭 → 写任务发 EOF 收尾)
    let session = {
        let mut map = exec_state.sessions.lock().unwrap();
        map.remove(&session_id)
    };
    match session {
        Some(s) => {
            // 通知读任务主动 close 通道;读循环已退出时此发送失败,可忽略
            let _ = s.stop_tx.send(()).await;
            Ok(())
        }
        None => Err(format!("交互式终端会话不存在或已结束: {}", session_id)),
    }
}
