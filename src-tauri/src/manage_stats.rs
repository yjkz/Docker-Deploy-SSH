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

use crate::manage::{connect_server, is_docker_perm_denied, parse_ndjson, with_timeout, EXEC_TIMEOUT_SECS};
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

/// 容器级聚合指标(第 N 批):由单轮 stats 全表计算,随事件推给前端在
/// 监控页顶部展示「最吃资源的容器」;纯函数聚合,单测覆盖。
/// 注意:本结构按契约使用 camelCase(前端直接消费),与文件内其他 snake_case
/// 结构不同(同 StackEnv 先例)。
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerTop {
    /// 容器名(展示口径)
    name: String,
    /// CPU%(docker stats 原样字符串,如 "12.34%")
    cpu_percent: String,
    /// 内存占用(docker stats 原样,如 "256MiB / 3.84GiB")
    mem_usage: String,
}

/// 单轮容器聚合(第 N 批,camelCase)。
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatsAggregate {
    /// 本轮样本容器总数
    count: usize,
    /// CPU% 最高的前 3 个容器(降序;样本 <3 时全列)
    top_cpu: Vec<ContainerTop>,
    /// 内存% 最高的前 3 个容器(降序)
    top_mem: Vec<ContainerTop>,
}

/// 把 docker stats 的 "12.34%" 解析为 f64(原样/空串/非数字 → None)
fn parse_percent(s: &str) -> Option<f64> {
    s.trim().trim_end_matches('%').trim().parse().ok()
}

/// 由单轮全表计算聚合(纯函数,便于单测):Top CPU 按解析出的数值降序,
/// 解析失败的行排在末尾(稳定排序保持 docker 输出序);Top 内存同口径。
pub(crate) fn aggregate_stats(rows: &[StatsRow]) -> StatsAggregate {
    let mut by_cpu: Vec<&StatsRow> = rows.iter().collect();
    by_cpu.sort_by(|a, b| {
        let av = parse_percent(&a.cpu_percent).unwrap_or(f64::MIN);
        let bv = parse_percent(&b.cpu_percent).unwrap_or(f64::MIN);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut by_mem: Vec<&StatsRow> = rows.iter().collect();
    by_mem.sort_by(|a, b| {
        let av = parse_percent(&a.mem_percent).unwrap_or(f64::MIN);
        let bv = parse_percent(&b.mem_percent).unwrap_or(f64::MIN);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });
    let to_top = |r: &StatsRow| ContainerTop {
        name: r.name.clone(),
        cpu_percent: r.cpu_percent.clone(),
        mem_usage: r.mem_usage.clone(),
    };
    StatsAggregate {
        count: rows.len(),
        top_cpu: by_cpu.iter().take(3).map(|r| to_top(r)).collect(),
        top_mem: by_mem.iter().take(3).map(|r| to_top(r)).collect(),
    }
}

/// `manage-stats` 事件 payload。
#[derive(Debug, Clone, Serialize)]
struct StatsPayload {
    server_id: String,
    stats: Vec<StatsRow>,
    /// 容器级聚合(第 N 批,camelCase;失败轮为默认值 count=0)
    #[serde(rename = "aggregate")]
    aggregate: StatsAggregate,
    /// 单轮失败时的中文错误提示(第十六批起可带 `[dderr:*]` 码标记);成功轮为 None。
    error: Option<String>,
    /// true 表示后端监控循环已自行终止(权限拒绝 / 连续连接失败),
    /// 前端据此把 UI 置回「已停止」;普通单轮失败为 false(循环继续)。
    stopped: bool,
    /// 错误类别码(第十六批,camelCase;`error` 为 None 时也是 None):
    /// "canceled"/"transport"/"auth"/"perm_denied"/"timeout"/… 见 wiki/04 码表。
    /// 前端优先按码分类,无码回退旧行为(版本错配时安全)。
    #[serde(rename = "errorCode", skip_serializing_if = "Option::is_none")]
    error_code: Option<&'static str>,
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
    // 托盘 tooltip(第十五批):监控是长驻后台动作,窗口隐藏时用户从托盘
    // 就能看到「正在监控哪台」。名称查不到(配置刚删)时回退用 id。
    {
        let name = crate::config::load_config()
            .ok()
            .and_then(|cfg| cfg.servers.iter().find(|s| s.id == server_id).map(|s| s.name.clone()))
            .unwrap_or_else(|| server_id.clone());
        crate::tray_status::set_monitor(&app, true, &name);
    }
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
                            aggregate: StatsAggregate::default(),
                            error: Some(err.clone()),
                            stopped: false,
                            error_code: Some(crate::errors::ErrCode::Transport.as_str()),
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
                        aggregate: aggregate_stats(&rows),
                        stats: rows,
                        error: None,
                        stopped: false,
                        error_code: None,
                    };
                    let _ = app.emit(STATS_EVENT, payload);
                }
                RoundOutcome::PermDenied => {
                    // 权限拒绝:立即停止
                    log::warn!("监控退出: generation={} docker 权限被拒绝", generation);
                    let payload = StatsPayload {
                        server_id: server_id.clone(),
                        stats: Vec::new(),
                        aggregate: StatsAggregate::default(),
                        error: Some(crate::errors::perm_denied()),
                        stopped: true,
                        error_code: Some(crate::errors::ErrCode::PermDenied.as_str()),
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
                        crate::errors::strip(&err)
                    );
                    let payload = StatsPayload {
                        server_id: server_id.clone(),
                        stats: Vec::new(),
                        aggregate: StatsAggregate::default(),
                        error: Some(err.clone()),
                        stopped: false,
                        // 命令层错误码:退出码非 0 → protocol,解析失败 → parse
                        error_code: Some(
                            if err.starts_with("docker stats 失败") {
                                crate::errors::ErrCode::Protocol
                            } else {
                                crate::errors::ErrCode::Parse
                            }
                            .as_str(),
                        ),
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
                        crate::errors::strip(&err)
                    );
                    let payload = StatsPayload {
                        server_id: server_id.clone(),
                        stats: Vec::new(),
                        aggregate: StatsAggregate::default(),
                        error: Some(err.clone()),
                        stopped: false,
                        error_code: Some(
                            if err.contains("超时") {
                                crate::errors::ErrCode::Timeout
                            } else {
                                crate::errors::ErrCode::Transport
                            }
                            .as_str(),
                        ),
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
pub async fn manage_stats_stop(app: tauri::AppHandle, stats_state: tauri::State<'_, StatsState>) -> Result<(), String> {
    log::info!("收到停止监控请求");
    stats_state.end();
    // 托盘 tooltip(第十五批):清监控态(部署运行态不受影响,优先级更高)
    crate::tray_status::set_monitor(&app, false, "");
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
        aggregate: StatsAggregate::default(),
        // 熔断停止:连接类失败,挂 transport 码(第十六批);文案剥内层标记
        error: Some(format!(
            "连续 {} 轮连接失败,监控已停止:{}",
            MAX_CONNECT_FAILURES,
            crate::errors::strip(err)
        )),
        stopped: true,
        error_code: Some(crate::errors::ErrCode::Transport.as_str()),
    };
    let _ = app.emit(STATS_EVENT, payload);
    state.lock().unwrap().finish(generation);
    // 托盘 tooltip(第十五批):监控熔断自动退出也要清态 —— 不清的话托盘会
    // 永远挂着「监控中」,与实际不符(前端可能不在线,收不到 stopped 事件)
    crate::tray_status::set_monitor(app, false, "");
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
/// 错误串挂码(第十六批):超时 → Timeout;权限拒绝 → PermDenied;
/// 命令非零退出 → Protocol(退出码 -1 表示连接已断,归 Transport);
/// 传输层(SSH 通道/执行失败)由 ssh.rs 生产点挂 Transport。
async fn exec_stats_collect(client: &mut SshClient) -> Result<String, String> {
    use crate::errors::ErrCode;
    if JSON_SHORTHAND_OK.load(Ordering::Relaxed) {
        let (code, out) = with_timeout(
            EXEC_TIMEOUT_SECS,
            "获取容器统计超时",
            "请检查服务器网络后重试",
            exec_collect(client, CMD_STATS_JSON),
        )
        .await
        .map_err(|e| {
            if crate::errors::code_of(&e) == Some(ErrCode::Transport) {
                // 连接已断(ssh.rs 已挂 Transport)保留原码;超时由下方 Timeout 分支处理
                e
            } else if crate::errors::code_of(&e) == Some(ErrCode::Timeout) {
                e
            } else {
                crate::errors::tagged(ErrCode::Timeout, e)
            }
        })?;
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
            return Err(crate::errors::perm_denied());
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
            return Err(crate::errors::perm_denied());
        }
        // 退出码 -1 是 exec 未收到 ExitStatus 的默认值,通常意味着连接已断
        let cls = if code == -1 { ErrCode::Transport } else { ErrCode::Protocol };
        return Err(crate::errors::tagged(
            cls,
            format!("docker stats 失败(退出码 {}): {}", code, out.trim()),
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
/// 第十六批:改按错误码判定(transport/timeout 均视为连接不可信);
/// 无码错误(旧式文案)保守按命令层处理 —— 与旧行为一致。
fn is_transport_error(err: &str) -> bool {
    matches!(
        crate::errors::code_of(err),
        Some(crate::errors::ErrCode::Transport) | Some(crate::errors::ErrCode::Timeout)
    )
}

/// 用会话内复用的连接执行一轮 docker stats:
/// 执行命令(经 exec_stats_collect 及其旧版 Docker 降级逻辑)→ 解析 NDJSON。
async fn run_stats_round(client: &mut SshClient) -> RoundOutcome {
    let out = match exec_stats_collect(client).await {
        Ok(out) => out,
        Err(err) => {
            // 权限拒绝按码判定(perm_denied);远端 docker 原样输出仍走
            // is_docker_perm_denied 文案兜底(匹配的是 docker 的英文输出,
            // 无法挂码 —— 外部工具输出是文案匹配的唯一合法存留区,见 wiki/07)
            if crate::errors::code_of(&err) == Some(crate::errors::ErrCode::PermDenied)
                || is_docker_perm_denied(&err)
            {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generation_guard() {
        let mut inner = StatsStateInner::default();
        assert!(!inner.is_current(1));
        inner.generation = 1;
        inner.running = true;
        assert!(inner.is_current(1));
        assert!(!inner.is_current(2));
        // 旧会话自清理:代号过期时 finish 不得清新会话的 running
        inner.generation = 2;
        inner.running = true;
        inner.finish(1);
        assert!(inner.running, "旧代号 finish 不应清 running");
        inner.finish(2);
        assert!(!inner.running);
    }

    #[test]
    fn test_is_transport_error() {
        use crate::errors::{tagged, ErrCode};
        // 传输层失败(连接不可信,需重连)—— 第十六批起按码判定
        assert!(is_transport_error(&tagged(
            ErrCode::Transport,
            "SSH 打开会话通道失败: x"
        )));
        assert!(is_transport_error(&tagged(ErrCode::Timeout, "获取容器统计超时")));
        assert!(!is_transport_error(&tagged(
            ErrCode::Protocol,
            "docker stats 失败(退出码 1): x"
        )));
        // 无码旧式错误保守按命令层(与旧行为一致:不重连)
        assert!(!is_transport_error("SSH 打开会话通道失败: x"));
        assert!(!is_transport_error("解析失败"));
    }

    fn row(name: &str, cpu: &str, mem_pct: &str, mem_usage: &str) -> StatsRow {
        StatsRow {
            container_id: format!("id-{name}"),
            name: name.into(),
            cpu_percent: cpu.into(),
            mem_usage: mem_usage.into(),
            mem_percent: mem_pct.into(),
            net_io: String::new(),
            block_io: String::new(),
            pids: String::new(),
        }
    }

    #[test]
    fn test_aggregate_stats_top_cpu_and_mem() {
        // 5 容器:CPU 降序 Top3 = c9/c5/c1;内存降序 Top3 = m7/m2/m5
        let rows = vec![
            row("c1", "1.50%", "30.00%", "100MiB / 4GiB"),
            row("c9", "99.00%", "10.00%", "50MiB / 4GiB"),
            row("m7", "5.00%", "80.00%", "900MiB / 4GiB"),
            row("m2", "3.00%", "60.00%", "600MiB / 4GiB"),
            row("c5", "50.00%", "40.00%", "400MiB / 4GiB"),
        ];
        let agg = aggregate_stats(&rows);
        assert_eq!(agg.count, 5);
        let cpu_names: Vec<&str> = agg.top_cpu.iter().map(|t| t.name.as_str()).collect();
        // CPU Top3 = 99 > 50 > 5(c9/c5/m7;c1 的 1.5% 落榜)
        assert_eq!(cpu_names, vec!["c9", "c5", "m7"]);
        assert_eq!(agg.top_cpu[0].cpu_percent, "99.00%");
        let mem_names: Vec<&str> = agg.top_mem.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(mem_names, vec!["m7", "m2", "c5"]);
        assert_eq!(agg.top_mem[0].mem_usage, "900MiB / 4GiB");
    }

    #[test]
    fn test_aggregate_stats_empty_and_unparseable() {
        // 空表:count=0,Top 均空
        let agg = aggregate_stats(&[]);
        assert_eq!(agg, StatsAggregate::default());
        // 样本 < 3:全列(不截断补空)
        let rows = vec![row("a", "0.10%", "1.00%", "x"), row("b", "0.20%", "2.00%", "y")];
        let agg = aggregate_stats(&rows);
        assert_eq!(agg.count, 2);
        assert_eq!(agg.top_cpu.len(), 2);
        assert_eq!(agg.top_cpu[0].name, "b");
        // 解析失败("–" 空串等)排在可解析行之后,不 panic
        let rows = vec![
            row("bad", "", "n/a", "z"),
            row("ok", "1.00%", "2.00%", "w"),
        ];
        let agg = aggregate_stats(&rows);
        assert_eq!(agg.top_cpu[0].name, "ok");
        assert_eq!(agg.top_cpu[1].name, "bad");
    }

    #[test]
    fn test_begin_end_monotonic() {
        let st = StatsState::default();
        let g1 = st.begin();
        let g2 = st.begin(); // 重复 start 无缝接管:代号严格递增
        assert!(g2 > g1);
        st.end();
        let g3 = st.begin();
        assert!(g3 > g2);
    }
}
