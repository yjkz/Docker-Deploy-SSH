//! docker stats 实时监控模块(C 阶段追加)。
//!
//! 通过事件流式推送:前端监听 Tauri 事件 `manage-stats` 接收轮询数据,
//! payload 为 `{ server_id, stats: Vec<StatsRow>, error: Option<String>, stopped: bool }`。
//!
//! 连接模型:一个监控会话只建立一条 SSH 连接并跨轮复用(`SshClient::exec`
//! 每次各自开新通道,复用同一条连接是安全的),每轮只剩服务端采样耗时,
//! 真实帧间隔得以贴近用户设定值。单轮失败分类处理:命令失败照旧上报并
//! 继续(不清连接);传输层失败上报后立即重连一次,重连失败计入连续失败
//! 计数(沿用 MAX_CONNECT_FAILURES 语义),成功则重置计数;权限拒绝立即
//! 停止。会话退出时连接随轮询任务 drop 而关闭,无泄漏。
//!
//! 会话模型:全局同时只允许一个 stats 会话。`StatsState` 内部用
//! `generation`(会话代号)+ `running` 保护;再次 `manage_stats_start` 时
//! 递增 generation,旧轮询循环每轮检查自己的代号是否仍为最新,若不是则
//! 自动退出,从而实现旧会话自清理、新会话无缝接管。
//!
//! 低耦合:复用 `crate::manage` 已 pub(crate) 的助手
//! (`connect_server` / `parse_ndjson` / `is_docker_perm_denied` /
//! `PERM_DENIED_MSG`)与 `crate::ssh::exec_collect`。

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::Emitter;

use crate::manage::{
    connect_server, is_docker_perm_denied, parse_ndjson, with_timeout, EXEC_TIMEOUT_SECS,
    PERM_DENIED_MSG,
};
use crate::ssh::{exec_collect, SshClient};

/// 轮询间隔上下限(秒)。
const INTERVAL_MIN: u32 = 1;
const INTERVAL_MAX: u32 = 60;
/// 连接连续失败达到该轮数后自动停止监控。
const MAX_CONNECT_FAILURES: u32 = 3;

/// 事件名:`AppBus.on('manage-stats', handler)`。
const STATS_EVENT: &str = "manage-stats";

// ===== 全局会话状态 =====

/// 由 tauri Builder `.manage(StatsState::default())` 注册的全局状态。
#[derive(Default)]
pub struct StatsState {
    /// Arc 共享给 tokio::spawn 的监控循环,避免借用 tauri::State 的局部生命周期
    inner: std::sync::Arc<Mutex<StatsStateInner>>,
}

#[derive(Default)]
struct StatsStateInner {
    /// 会话代号:每次 start 递增;循环每轮检查自己持有的代号是否仍为最新。
    generation: u64,
    /// 当前是否有会话在运行。
    running: bool,
}

impl StatsStateInner {
    /// 检查代号 `gen` 是否仍为最新会话。
    fn is_current(&self, gen: u64) -> bool {
        self.running && self.generation == gen
    }

    /// 循环退出时清理:仅当当前代号仍是自己时才清除 running(避免误清新会话)。
    fn finish(&mut self, gen: u64) {
        if self.generation == gen {
            self.running = false;
        }
    }
}

impl StatsState {
    /// 开始新会话:递增代号并标记运行中,返回分配到的代号。
    fn begin(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.generation += 1;
        inner.running = true;
        inner.generation
    }

    /// 结束当前会话(递增代号使旧循环退出,清除 running 标记)。
    fn end(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.generation += 1;
        inner.running = false;
    }
}

// ===== 数据结构 =====

/// `docker stats --no-stream --format json` 单行输出(PascalCase)。
/// 注意:容器 ID 字段实际名为 `Container`;所有字段 #[serde(default)] 防缺失。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawStats {
    #[serde(rename = "Container", default)]
    container: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    cpu_perc: String,
    #[serde(default)]
    mem_usage: String,
    #[serde(default)]
    mem_perc: String,
    #[serde(default)]
    net_io: String,
    #[serde(default)]
    block_io: String,
    #[serde(default)]
    pids: String,
}

/// 推送给前端的单条容器统计(snake_case)。
#[derive(Debug, Clone, Serialize)]
pub struct StatsRow {
    container_id: String,
    name: String,
    cpu_percent: String,
    mem_usage: String,
    mem_percent: String,
    net_io: String,
    block_io: String,
    /// PIDs 为字符串数字(docker stats 原样输出)。
    pids: String,
}

/// `manage-stats` 事件 payload。
#[derive(Debug, Clone, Serialize)]
struct StatsPayload {
    server_id: String,
    stats: Vec<StatsRow>,
    /// 单轮失败时的中文错误提示;成功轮为 None。
    error: Option<String>,
    /// true 表示后端监控循环已自行终止(权限拒绝 / 连续连接失败),
    /// 前端据此把 UI 置回「已停止」;普通单轮失败为 false(循环继续)。
    stopped: bool,
}

// ===== Tauri 命令 =====

/// 开始 docker stats 实时监控(全局单会话,重复调用自动替换旧会话)。
///
/// 数据通过 `manage-stats` 事件流式推送,命令本身立即返回。
#[tauri::command]
pub async fn manage_stats_start(
    app: tauri::AppHandle,
    stats_state: tauri::State<'_, StatsState>,
    server_id: String,
    password_plain: Option<String>,
    interval_secs: Option<u32>,
) -> Result<(), String> {
    if server_id.trim().is_empty() {
        return Err("服务器 ID 不能为空".to_string());
    }
    let interval = interval_secs
        .unwrap_or(2)
        .clamp(INTERVAL_MIN, INTERVAL_MAX);
    // 递增 generation:旧循环在下轮检查时发现代号过期而自动退出
    let generation = stats_state.begin();
    log::info!(
        "监控启动: server={} interval={}s generation={}",
        server_id,
        interval,
        generation
    );
    let state = std::sync::Arc::clone(&stats_state.inner);

    tokio::spawn(async move {
        // 会话级 SSH 连接:整个监控会话只在建连/重连时握手,跨轮复用同一条
        // 连接(SshClient::exec 每次各自开新通道,复用安全),消除每轮重新
        // SSH 握手的开销,让真实帧间隔贴近用户设定值。None表示当前无可用
        // 连接(尚未建连,或传输层失败后重连未成功)。会话退出时 client 随
        // 任务结束 drop,russh 驱动循环随之关闭底层连接,无泄漏。
        let mut client: Option<SshClient> = None;
        let mut connect_failures: u32 = 0;
        let mut round: u64 = 0;
        loop {
            // 会话已被替换或停止 → 退出
            if !state.lock().unwrap().is_current(generation) {
                log::info!(
                    "监控退出: generation={} 会话已被替换或停止",
                    generation
                );
                break;
            }

            round += 1;
            let started = std::time::Instant::now();

            // 建连/补连:首轮建连,或上轮传输失败且重连未成功时再次尝试。
            // 沿用既有连接失败计数语义:连续 MAX_CONNECT_FAILURES 轮失败则停止。
            if client.is_none() {
                match connect_server(&server_id, password_plain.as_deref()).await {
                    Ok((_server, c)) => {
                        client = Some(c);
                        connect_failures = 0;
                        log::info!(
                            "监控连接成功: generation={} round={} 后续轮次复用该连接",
                            generation,
                            round
                        );
                    }
                    Err(err) => {
                        connect_failures += 1;
                        log::warn!(
                            "监控第 {} 轮失败: generation={} 耗时 {:.1}s 原因: {}",
                            round,
                            generation,
                            started.elapsed().as_secs_f64(),
                            err
                        );
                        if connect_failure_limit_reached(
                            &app,
                            &state,
                            &server_id,
                            generation,
                            connect_failures,
                            &err,
                        ) {
                            break;
                        }
                        let payload = StatsPayload {
                            server_id: server_id.clone(),
                            stats: Vec::new(),
                            error: Some(err),
                            stopped: false,
                        };
                        let _ = app.emit(STATS_EVENT, payload);
                        // 本轮无连接可执行,等待后进入下一轮重试
                        if !wait_next_round(&state, generation, interval).await {
                            return;
                        }
                        continue;
                    }
                }
            }

            // 复用已有连接执行一轮采集(上方建连失败路径已 continue,此处必有连接)
            let outcome = if let Some(c) = client.as_mut() {
                run_stats_round(c).await
            } else {
                // 理论不可达;保守起见等待后进入下一轮重试
                if !wait_next_round(&state, generation, interval).await {
                    return;
                }
                continue;
            };
            match outcome {
                RoundOutcome::Success(rows) => {
                    connect_failures = 0;
                    log::info!(
                        "监控第 {} 轮成功: generation={} {} 个容器, 耗时 {:.1}s",
                        round,
                        generation,
                        rows.len(),
                        started.elapsed().as_secs_f64()
                    );
                    let payload = StatsPayload {
                        server_id: server_id.clone(),
                        stats: rows,
                        error: None,
                        stopped: false,
                    };
                    let _ = app.emit(STATS_EVENT, payload);
                }
                RoundOutcome::PermDenied => {
                    // 权限拒绝:立即停止
                    log::warn!("监控退出: generation={} docker 权限被拒绝", generation);
                    let payload = StatsPayload {
                        server_id: server_id.clone(),
                        stats: Vec::new(),
                        error: Some(PERM_DENIED_MSG.to_string()),
                        stopped: true,
                    };
                    let _ = app.emit(STATS_EVENT, payload);
                    state.lock().unwrap().finish(generation);
                    break;
                }
                RoundOutcome::CommandFailed(err) => {
                    // 命令失败(退出码非 0/解析失败):连接仍可用,不清连接,继续下一轮
                    connect_failures = 0;
                    log::warn!(
                        "监控第 {} 轮失败: generation={} 耗时 {:.1}s 原因: {}",
                        round,
                        generation,
                        started.elapsed().as_secs_f64(),
                        err
                    );
                    let payload = StatsPayload {
                        server_id: server_id.clone(),
                        stats: Vec::new(),
                        error: Some(err),
                        stopped: false,
                    };
                    let _ = app.emit(STATS_EVENT, payload);
                }
                RoundOutcome::TransportFailed(err) => {
                    // 传输层失败(连接已断/执行超时):发 error payload 后立即重连一次
                    log::warn!(
                        "监控第 {} 轮失败: generation={} 耗时 {:.1}s 原因: {}",
                        round,
                        generation,
                        started.elapsed().as_secs_f64(),
                        err
                    );
                    let payload = StatsPayload {
                        server_id: server_id.clone(),
                        stats: Vec::new(),
                        error: Some(err),
                        stopped: false,
                    };
                    let _ = app.emit(STATS_EVENT, payload);
                    // 旧连接已不可信:先丢弃再重连;重连失败计入连续失败计数
                    client = None;
                    match connect_server(&server_id, password_plain.as_deref()).await {
                        Ok((_server, c)) => {
                            client = Some(c);
                            connect_failures = 0;
                            log::info!(
                                "监控重连成功: generation={} round={}",
                                generation,
                                round
                            );
                        }
                        Err(re) => {
                            connect_failures += 1;
                            log::warn!(
                                "监控重连失败: generation={} round={} 原因: {}",
                                generation,
                                round,
                                re
                            );
                            if connect_failure_limit_reached(
                                &app,
                                &state,
                                &server_id,
                                generation,
                                connect_failures,
                                &re,
                            ) {
                                break;
                            }
                        }
                    }
                }
            }

            // 等待下一轮;期间若会话被替换/停止则提前退出
            if !wait_next_round(&state, generation, interval).await {
                return;
            }
        }
        // 循环退出时清理 running 状态
        state.lock().unwrap().finish(generation);
    });

    Ok(())
}

/// 停止 docker stats 实时监控(递增 generation 使循环退出)。
#[tauri::command]
pub async fn manage_stats_stop(stats_state: tauri::State<'_, StatsState>) -> Result<(), String> {
    log::info!("收到停止监控请求");
    stats_state.end();
    Ok(())
}

// ===== 轮询循环辅助 =====

/// 等待下一轮轮询间隔;期间每 200ms 检查一次会话是否已被替换/停止,
/// 若是则清理 running 并返回 false(调用方应立即退出任务)。
async fn wait_next_round(
    state: &Mutex<StatsStateInner>,
    generation: u64,
    interval: u32,
) -> bool {
    let mut elapsed = Duration::ZERO;
    let step = Duration::from_millis(200);
    while elapsed < Duration::from_secs(interval as u64) {
        tokio::time::sleep(step).await;
        elapsed += step;
        if !state.lock().unwrap().is_current(generation) {
            log::info!(
                "监控退出: generation={} 会话已被替换或停止",
                generation
            );
            state.lock().unwrap().finish(generation);
            return false;
        }
    }
    true
}

/// 连接失败达到 MAX_CONNECT_FAILURES 阈值时的统一收口(沿用既有 stopped 语义):
/// 发出 stopped=true 的 payload 并清理 running,返回 true 表示循环应退出;
/// 未达阈值返回 false,由调用方按普通单轮失败继续。
fn connect_failure_limit_reached(
    app: &tauri::AppHandle,
    state: &Mutex<StatsStateInner>,
    server_id: &str,
    generation: u64,
    connect_failures: u32,
    err: &str,
) -> bool {
    if connect_failures < MAX_CONNECT_FAILURES {
        return false;
    }
    log::warn!(
        "监控退出: generation={} 连续 {} 轮连接失败",
        generation,
        MAX_CONNECT_FAILURES
    );
    let payload = StatsPayload {
        server_id: server_id.to_string(),
        stats: Vec::new(),
        error: Some(format!(
            "连续 {} 轮连接失败,监控已停止:{}",
            MAX_CONNECT_FAILURES, err
        )),
        stopped: true,
    };
    let _ = app.emit(STATS_EVENT, payload);
    state.lock().unwrap().finish(generation);
    true
}

// ===== 单轮采集与失败分类 =====

/// docker stats 命令(`--format json` 简写,较新版本 Docker 支持)。
const CMD_STATS_JSON: &str = "docker stats --no-stream --format json";
/// 降级命令(显式 `{{json .}}` Go 模板,旧版 Docker 亦支持)。
const CMD_STATS_TEMPLATE: &str = "docker stats --no-stream --format '{{json .}}'";

/// 进程级缓存:`--format json` 简写是否可用。旧版 Docker 不认该简写时
/// (退出码非 0 报错,或把 "json" 当模板文本原样输出、退出码 0),降级为
/// 模板命令。模板命令在新旧版本均可工作,故全局降级一次即够,避免每轮双重执行。
static JSON_SHORTHAND_OK: AtomicBool = AtomicBool::new(true);

/// 单轮套超时执行 docker stats 并返回输出;旧版 Docker 自动降级模板重试。
async fn exec_stats_collect(client: &mut SshClient) -> Result<String, String> {
    if JSON_SHORTHAND_OK.load(Ordering::Relaxed) {
        let (code, out) = with_timeout(
            EXEC_TIMEOUT_SECS,
            "获取容器统计超时",
            "请检查服务器网络后重试",
            exec_collect(client, CMD_STATS_JSON),
        )
        .await?;
        // 简写可用:退出码 0 且输出为 NDJSON(每行 '{' 开头;空输出视为无容器)。
        // 旧版 Docker 会把 "json" 当模板文本,每行原样输出 "json" 且退出码 0。
        if code == 0
            && out
                .lines()
                .all(|l| l.trim().is_empty() || l.trim().starts_with('{'))
        {
            return Ok(out);
        }
        if code != 0 && is_docker_perm_denied(&out) {
            return Err(PERM_DENIED_MSG.to_string());
        }
        JSON_SHORTHAND_OK.store(false, Ordering::Relaxed);
    }
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取容器统计超时",
        "请检查服务器网络后重试",
        exec_collect(client, CMD_STATS_TEMPLATE),
    )
    .await?;
    if code != 0 {
        if is_docker_perm_denied(&out) {
            return Err(PERM_DENIED_MSG.to_string());
        }
        return Err(format!(
            "docker stats 失败(退出码 {}): {}",
            code,
            out.trim()
        ));
    }
    Ok(out)
}

/// 单轮采集结果分类:按错误来源决定轮询循环的后续处理(继续 / 重连 / 停止)。
enum RoundOutcome {
    /// 采集成功。
    Success(Vec<StatsRow>),
    /// 权限拒绝:立即停止监控(stopped=true)。
    PermDenied,
    /// 命令失败(退出码非 0 / 解析失败):连接仍可用,发 error payload 后继续下一轮。
    CommandFailed(String),
    /// 传输层失败(通道打不开 / 命令发不出去 / 执行超时 / 连接已断):
    /// 连接不可信,发 error payload 后立即重连一次。
    TransportFailed(String),
}

/// 判断单轮错误是否属于 SSH 传输层失败(连接不可信,需要重连)。
/// `exec_stats_collect` 的错误来源:传输层(`SSH 打开会话通道失败` /
/// `SSH 执行命令失败`,以及套超时后的 `获取容器统计超时`)与命令层
/// (`docker stats 失败(退出码 N)`)。退出码 -1 是 exec 未收到
/// ExitStatus 的默认值,通常意味着连接已断,同样按传输层失败处理。
fn is_transport_error(err: &str) -> bool {
    err.contains("SSH 打开会话通道失败")
        || err.contains("SSH 执行命令失败")
        || err.contains("获取容器统计超时")
        || err.contains("退出码 -1")
}

/// 用会话内复用的连接执行一轮 docker stats:
/// 执行命令(经 exec_stats_collect 及其旧版 Docker 降级逻辑)→ 解析 NDJSON。
async fn run_stats_round(client: &mut SshClient) -> RoundOutcome {
    let out = match exec_stats_collect(client).await {
        Ok(out) => out,
        Err(err) => {
            if err == PERM_DENIED_MSG || is_docker_perm_denied(&err) {
                return RoundOutcome::PermDenied;
            }
            if is_transport_error(&err) {
                return RoundOutcome::TransportFailed(err);
            }
            return RoundOutcome::CommandFailed(err);
        }
    };
    match parse_ndjson::<RawStats>(&out) {
        Ok(raw) => RoundOutcome::Success(
            raw.into_iter()
                .map(|r| StatsRow {
                    container_id: r.container,
                    name: r.name,
                    cpu_percent: r.cpu_perc,
                    mem_usage: r.mem_usage,
                    mem_percent: r.mem_perc,
                    net_io: r.net_io,
                    block_io: r.block_io,
                    pids: r.pids,
                })
                .collect(),
        ),
        // 解析失败属命令层问题(输出不是预期 NDJSON),连接仍可复用
        Err(e) => RoundOutcome::CommandFailed(e),
    }
}
