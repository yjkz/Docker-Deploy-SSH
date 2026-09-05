//! docker stats 实时监控模块(C 阶段追加)。
//!
//! 通过事件流式推送:前端监听 Tauri 事件 `manage-stats` 接收轮询数据,
//! payload 为 `{ server_id, stats: Vec<StatsRow>, error: Option<String> }`。
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
    let state = std::sync::Arc::clone(&stats_state.inner);

    tokio::spawn(async move {
        let mut connect_failures: u32 = 0;
        loop {
            // 会话已被替换或停止 → 退出
            if !state.lock().unwrap().is_current(generation) {
                break;
            }

            match run_stats_once(&server_id, &password_plain).await {
                Ok(rows) => {
                    connect_failures = 0;
                    let payload = StatsPayload {
                        server_id: server_id.clone(),
                        stats: rows,
                        error: None,
                        stopped: false,
                    };
                    let _ = app.emit(STATS_EVENT, payload);
                }
                Err(err) => {
                    // 权限拒绝:立即停止
                    if is_docker_perm_denied(&err) {
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
                    // 区分连接失败与其他失败:连接连续失败达到阈值则自动停止。
                    // 连接失败来源:connect_server 内的「SSH 连接失败/认证失败/连接超时/
                    // 读取配置失败/未找到 ID 为…/需要密码」等错误文案。
                    let is_connect = err.contains("SSH 连接失败")
                        || err.contains("SSH 公钥认证失败")
                        || err.contains("SSH 密码认证失败")
                        || err.contains("SSH 密钥认证失败")
                        || err.contains("连接超时")
                        || err.contains("加载私钥失败")
                        || err.contains("读取配置失败")
                        || err.contains("未找到 ID 为")
                        || err.contains("需要密码");
                    if is_connect {
                        connect_failures += 1;
                        if connect_failures >= MAX_CONNECT_FAILURES {
                            let payload = StatsPayload {
                                server_id: server_id.clone(),
                                stats: Vec::new(),
                                error: Some(format!(
                                    "连续 {} 轮连接失败,监控已停止:{}",
                                    MAX_CONNECT_FAILURES, err
                                )),
                                stopped: true,
                            };
                            let _ = app.emit(STATS_EVENT, payload);
                            state.lock().unwrap().finish(generation);
                            break;
                        }
                    } else {
                        connect_failures = 0;
                    }
                    let payload = StatsPayload {
                        server_id: server_id.clone(),
                        stats: Vec::new(),
                        error: Some(err),
                        stopped: false,
                    };
                    let _ = app.emit(STATS_EVENT, payload);
                }
            }

            // 等待下一轮;期间若会话被替换/停止则提前退出
            let mut elapsed = Duration::ZERO;
            let step = Duration::from_millis(200);
            while elapsed < Duration::from_secs(interval as u64) {
                tokio::time::sleep(step).await;
                elapsed += step;
                if !state.lock().unwrap().is_current(generation) {
                    state.lock().unwrap().finish(generation);
                    return;
                }
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
    stats_state.end();
    Ok(())
}

// ===== 单轮采集 =====

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

/// 执行一轮 docker stats:新建 SSH 连接 → 执行命令 → 解析 NDJSON。
async fn run_stats_once(
    server_id: &str,
    password_plain: &Option<String>,
) -> Result<Vec<StatsRow>, String> {
    let (_server, mut client) = connect_server(server_id, password_plain.as_deref()).await?;
    // 单轮套超时:远端 hang 住时不能让轮询循环永久阻塞(否则旧会话无法退出)
    let out = exec_stats_collect(&mut client).await?;
    let raw: Vec<RawStats> = parse_ndjson(&out)?;
    Ok(raw
        .into_iter()
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
        .collect())
}
