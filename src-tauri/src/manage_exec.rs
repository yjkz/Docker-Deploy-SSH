//! 容器交互式 Exec 终端模块(C 阶段追加)。
//!
//! 通过 [`SshClient::exec_interactive`] 打开 PTY 通道执行
//! `docker exec -it <container> <shell>`,并把通道交给后台任务:
//! - 读任务:持续 `wait()` 通道输出,经事件 `manage-exec-output` 推送给前端,
//!   payload 为 `{ session_id, data, eof }`;通道关闭(eof=true)时自清理会话,
//!   并组装结束原因(退出码 + 输出尾段原文 + shell 切换建议)随 eof 透出。
//! - 写任务:持有 `channel.make_writer()`(russh 0.46 中不借用通道、可移交),
//!   从 mpsc 队列取字节写入远端 stdin。命令层只与 mpsc Sender 打交道,
//!   避免在持 std::sync::Mutex 锁期间 await。
//! - resize 经 mpsc 转发给读任务,由其调用 `channel.window_change`(真实实现)。
//!
//! shell 选择:启动前先在同一条连接上经非交互 `docker exec` 探测容器内可用
//! shell([`probe_container_shell`],bash 优先、退回 sh,含可执行性验证);
//! 用户显式选择时尊重其值,未指定时用探测结果,distroless(无 sh)直接报错。
//!
//! 低耦合:复用 `crate::manage` 已 pub(crate) 的 `connect_server` / `shell_quote`
//! 与 `crate::ssh` 已 pub(crate) 的 `exec_collect`。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use tauri::{Emitter, Manager};

use crate::manage::{connect_server, shell_quote};
use crate::ssh::{exec_collect, SshClient};

/// 事件名:`manage-exec-output`。
const EXEC_EVENT: &str = "manage-exec-output";

/// 读任务内部事件队列容量(写/resize 指令)。
const CTRL_BUFFER: usize = 64;

/// shell 探测脚本:在容器内经 `sh -c` 执行,bash 优先,逐级退回 sh。
/// 关键点:`command -v` 只验证 PATH 存在性,发现不了「文件在但不可执行」——
/// 真机取证到的 docker exec 退出码 126 正是此类(127 才是「不存在」),
/// 故对 bash 追加 `bash -c true` 做可执行性验证,sh 同理。
/// 脚本本身不含单引号,整体经 shell_quote 单引号包裹后随 SSH exec 下发。
const SHELL_PROBE_SCRIPT: &str = "command -v bash >/dev/null 2>&1 && bash -c true >/dev/null 2>&1 && echo bash \
|| { command -v sh >/dev/null 2>&1 && sh -c true >/dev/null 2>&1 && echo sh; }";

/// 输出尾段缓冲上限(字节):超出后只保留尾部 `TAIL_KEEP` 字节。
const TAIL_MAX: usize = 4096;
const TAIL_KEEP: usize = 1024;

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

/// `manage_exec_start` 返回:会话 ID + 实际使用的 shell。
/// shell 可能与前端传入不同(前端选「自动」时为探测结果),前端以此回显。
#[derive(Clone, Serialize)]
pub struct ExecStartInfo {
    pub session_id: String,
    /// 实际使用的 shell(用户显式指定,或自动探测到的 bash/sh)
    pub shell: String,
}

/// 会话前探测容器内可用 shell(同一条 SSH 连接上一次非交互 `docker exec`,
/// 不带 `-it`,快速返回)。
///
/// - `Ok(Some("bash"|"sh"))`:探测到可用 shell;
/// - `Ok(None)`:脚本跑通但 bash/sh 均不可用(distroless 类镜像);
/// - `Err`:探测命令本身失败(容器未运行、docker 异常、sh 缺失),
///   把 docker 原始错误上抛给前端 toast。
async fn probe_container_shell(
    client: &mut SshClient,
    container_id: &str,
) -> Result<Option<String>, String> {
    let cmd = format!(
        "docker exec {} sh -c {}",
        shell_quote(container_id),
        shell_quote(SHELL_PROBE_SCRIPT)
    );
    let (code, out) = exec_collect(client, &cmd).await?;
    let found = out.trim();
    if found == "bash" || found == "sh" {
        return Ok(Some(found.to_string()));
    }
    if code == 127 {
        // docker exec 退出码 127 ⇒ exec 的 sh 本身不存在(distroless 类镜像);
        // 探测脚本自身只会以 0/1 结束,127 只能来自最外层 exec 找不到 sh
        return Err("容器内未找到可用的 shell(bash/sh)".to_string());
    }
    if code == 0 {
        return Ok(None);
    }
    Err(format!(
        "容器 shell 探测失败 (退出码 {}): {}",
        code,
        out.trim()
    ))
}

/// 启动容器交互式终端:建连 → shell 探测 → PTY → exec,返回 session_id 与
/// 实际使用的 shell。输出经事件 `manage-exec-output` 异步推送。
#[tauri::command]
pub async fn manage_exec_start(
    app: tauri::AppHandle,
    exec_state: tauri::State<'_, ExecState>,
    server_id: String,
    password_plain: Option<String>,
    container_id: String,
    shell: Option<String>,
) -> Result<ExecStartInfo, String> {
    // 建连(每次 start 新建连接,随会话保活,随会话结束丢弃)
    let (_, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;

    // shell 自动探测:失败(容器未运行等)在打开终端前就把 docker 原始错误
    // 返回给前端 toast,而不是开一个注定立即结束的会话
    let probed = probe_container_shell(&mut client, &container_id).await?;
    // 用户显式选择(下拉选了 bash/sh)→ 尊重其值,探测结果仅用于失败提示;
    // 未指定/选「自动」(None 或空串)→ 采用探测结果
    let shell_used = match shell.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => match probed.as_ref() {
            Some(s) => s.clone(),
            None => return Err("容器内未找到可用的 shell(bash/sh)".to_string()),
        },
    };
    let cmd = format!(
        "docker exec -it {} {}",
        shell_quote(&container_id),
        shell_quote(&shell_used)
    );

    let channel = client.exec_interactive(&cmd, 120, 32).await?;

    // 先取写句柄(不借用通道,可移交),再把通道移交给读任务
    let writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send> = Box::new(channel.make_writer());
    let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(CTRL_BUFFER);
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u32, u32)>(CTRL_BUFFER);
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);

    let session_id = next_session_id();
    log::info!(
        "交互式终端启动: session={} container={} shell={} 探测结果={}",
        session_id,
        container_id,
        shell_used,
        probed.as_deref().unwrap_or("无可用 shell")
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
    let shell_used_read = shell_used.clone();
    tokio::spawn(async move {
        let mut channel = channel;
        let mut exit_status: Option<u32> = None; // 远端 ExitStatus(正常 exit / exec 失败均有)
        let mut user_stop = false; // 用户点「停止」触发的退出(前端已自清,无需再提示)
        let mut output_tail = String::new(); // 输出尾段(退出码非 0 时并入结束原因)
        loop {
            tokio::select! {
                msg = channel.wait() => match msg {
                    Some(russh::ChannelMsg::Data { ref data })
                    | Some(russh::ChannelMsg::ExtendedData { ref data, .. }) => {
                        let text = String::from_utf8_lossy(data).into_owned();
                        output_tail.push_str(&text);
                        if output_tail.len() > TAIL_MAX {
                            // 只保留尾部,丢弃头部(按字符边界,避免截断多字节字符)
                            let mut cut = output_tail.len() - TAIL_KEEP;
                            while !output_tail.is_char_boundary(cut) {
                                cut += 1;
                            }
                            output_tail.drain(..cut);
                        }
                        let payload = ExecOutputPayload {
                            session_id: sid.clone(),
                            data: text,
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
        // 会话结束原因:写任务失败 > 远端退出码(带输出尾段)> 通道异常关闭;
        // 用户主动停止不提示。会话级 127/126 必然来自 shell 本身(交互 shell 存活时
        // 用户命令的退出码不会结束会话),故可据此给出针对性提示。
        let reason = if user_stop {
            None
        } else if let Some(err) = write_err.lock().unwrap().clone() {
            Some(format!("终端输入通道异常({})", err))
        } else if let Some(code) = exit_status {
            let mut msg = match code {
                127 => format!(
                    "远端命令未找到(退出码 127,容器内可能没有 {})",
                    shell_used_read
                ),
                126 => format!(
                    "容器内 {} 无法执行(退出码 126,常见于权限拒绝或二进制架构不匹配)",
                    shell_used_read
                ),
                _ => format!("远端进程已退出 (退出码 {})", code),
            };
            // 退出码非 0:把输出最后一段(去 ANSI 后截断,通常含 docker 的 OCI
            // 错误原文,如 "OCI runtime exec failed: ...")并入原因
            if code != 0 {
                if let Some(tail) = last_output_line(&output_tail, 200) {
                    msg.push_str(": ");
                    msg.push_str(&tail);
                }
                // 探测发现与实际使用的 shell 不同 → 给出切换建议
                if let Some(p) = probed.as_deref() {
                    if p != shell_used_read {
                        msg.push_str(&format!(
                            "(探测到容器内可用 shell: {},可在 Shell 下拉框切换后重试)",
                            p
                        ));
                    }
                }
            }
            Some(msg)
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

    Ok(ExecStartInfo {
        session_id,
        shell: shell_used,
    })
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

// ===== 输出尾段处理(纯函数,便于单测)=====

/// 去除 ANSI 转义序列:CSI(`ESC [ ... 字母`)、OSC(`ESC ] ... BEL/ST`)、
/// 其余 `ESC x` 单字符转义;与前端 termWrite 的简易处理口径对齐。
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI:吃到结束字母为止
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC:吃到 BEL 或 ST(ESC \)为止
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {} // 单字符转义(如 ESC M)直接丢弃
            }
        } else if c.is_control() && c != '\n' && c != '\r' && c != '\t' {
            // 其余 C0 控制字符(BEL 等)丢弃,与前端 termWrite 剥 \x07 的口径一致
        } else {
            out.push(c);
        }
    }
    out
}

/// 取输出尾段中最后一个非空行(去 ANSI、去首尾空白),超长只保留末尾 `cap`
/// 个字符(错误原文,如 OCI runtime 报错,通常位于输出末尾)。无可用内容 → None。
fn last_output_line(tail: &str, cap: usize) -> Option<String> {
    let stripped = strip_ansi(tail);
    let line = stripped
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())?;
    let chars: Vec<char> = line.chars().collect();
    let start = chars.len().saturating_sub(cap);
    Some(chars[start..].iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_probe_script_has_no_single_quote() {
        // 脚本不含单引号 ⇒ shell_quote 单引号包裹后语义无损
        assert!(!SHELL_PROBE_SCRIPT.contains('\''));
        // 且在 bash/dash 下均为合法 POSIX 片段的基本形态(含 bash 可执行性验证)
        assert!(SHELL_PROBE_SCRIPT.contains("bash -c true"));
        assert!(SHELL_PROBE_SCRIPT.contains("sh -c true"));
    }

    #[test]
    fn test_strip_ansi_csi_and_osc() {
        assert_eq!(strip_ansi("\x1b[?2004hroot@x:~# "), "root@x:~# ");
        assert_eq!(strip_ansi("\x1b]0;title\x07abc"), "abc");
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\abc"), "abc");
        assert_eq!(strip_ansi("\x1b[31merr\x1b[0m"), "err");
        // 不完整序列(尾部悬挂)也不应 panic
        assert_eq!(strip_ansi("ok\x1b[3"), "ok");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn test_last_output_line_picks_last_non_empty() {
        let tail = "bash: prompt\x1b[K\r\n\r\nOCI runtime exec failed: exec: \"bash\": permission denied\r\n";
        assert_eq!(
            last_output_line(tail, 200).unwrap(),
            "OCI runtime exec failed: exec: \"bash\": permission denied"
        );
    }

    #[test]
    fn test_last_output_line_truncates_to_tail() {
        let long = "x".repeat(300);
        let got = last_output_line(&long, 200).unwrap();
        assert_eq!(got.chars().count(), 200);
        // 保留的是末尾 200 个 x
        assert!(got.chars().all(|c| c == 'x'));
    }

    #[test]
    fn test_last_output_line_keeps_multibyte_boundary() {
        // 中文多字节 + 截断:按字符数截,不产生乱码半字符
        let s = "失败原因:".to_string() + &"错".repeat(150);
        let got = last_output_line(&s, 20).unwrap();
        assert_eq!(got.chars().count(), 20);
        assert!(got.starts_with("错") && got.ends_with("错"));
    }

    #[test]
    fn test_last_output_line_empty_returns_none() {
        assert_eq!(last_output_line("", 200), None);
        assert_eq!(last_output_line("\r\n\x1b[?1h\x07", 200), None);
    }
}
