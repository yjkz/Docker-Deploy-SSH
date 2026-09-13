//! 远程 Docker 管理模块(A 阶段 MVP)。
//!
//! 通过现有 SSH 通道在远程服务器执行 docker 命令,实现容器 / 镜像的查看与操作。
//! 低耦合:自实现连接辅助,仅通过公开 API 组合:
//! `config::load_config` → `commands::resolve_password` / `resolve_key_passphrase` /
//! `persist_host_key_if_needed`(pub(crate)) →
//! `ssh::SshClient::connect` → `ssh::exec_collect`(pub(crate))。
//!
//! Docker 输出约定:
//! - 列表类命令(`docker ps -a` / `docker images` / `docker system df`)带
//!   `--format json`,输出为 NDJSON(每行一个 JSON 对象),字段为 PascalCase。
//! - `docker info --format json` 输出单个 JSON 对象。
//! - `docker inspect <id>` 输出 JSON 数组(即使只查一个,前端取 `[0]`)。

use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::commands::{
    persist_host_key_if_needed, resolve_key_passphrase, resolve_password,
};
use crate::config::{load_config, AppConfig, ServerConfig};
use crate::ssh::{exec_collect, SshClient};

// ===== 超时常量 =====
pub(crate) const CONNECT_TIMEOUT_SECS: u64 = 15;
pub(crate) const EXEC_TIMEOUT_SECS: u64 = 60;
pub(crate) const PULL_TIMEOUT_SECS: u64 = 300;

/// 非 root 用户无 docker.sock 权限时的明确中文提示。
/// 第十六批:判定与展示文案分离 —— 生产一律用 [`crate::errors::perm_denied`]
/// (带 perm_denied 码);本常量已无引用,文案本体收编进 errors::perm_denied。
#[allow(dead_code)]
pub(crate) const PERM_DENIED_MSG: &str =
    "当前 SSH 用户无 Docker 权限(无法访问 /var/run/docker.sock),请将该用户加入 docker 组或使用 root 用户连接";

// ===== 连接辅助(自实现,不调用 commands.rs 私有函数) =====

pub(crate) fn find_server<'a>(cfg: &'a AppConfig, server_id: &str) -> Result<&'a ServerConfig, String> {
    cfg.servers
        .iter()
        .find(|s| s.id == server_id)
        .ok_or_else(|| format!("未找到 ID 为「{}」的服务器配置", server_id))
}

pub(crate) async fn with_timeout<T>(
    secs: u64,
    desc: &str,
    hint: &str,
    fut: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(res) => res,
        Err(_) => Err(crate::errors::tagged(
            crate::errors::ErrCode::Timeout,
            format!("{}({} 秒):{}", desc, secs, hint),
        )),
    }
}

pub(crate) async fn connect_server(
    server_id: &str,
    password_plain: Option<&str>,
) -> Result<(ServerConfig, SshClient), String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain,
        server.auth.password_enc.as_deref(),
    )?;
    // 阶段三:私钥口令(DPAPI 解密 key_pass_enc)+ 主机密钥 TOFU(观察值落盘)
    let key_pass = resolve_key_passphrase(&server)?;
    let observed = Arc::new(OnceLock::new());
    let client = with_timeout(
        CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::clone(&observed)),
    )
    .await?;
    persist_host_key_if_needed(&server, &observed.get().cloned());
    Ok((server, client))
}

pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub(crate) fn parse_ndjson<T: serde::de::DeserializeOwned>(text: &str) -> Result<Vec<T>, String> {
    let mut items = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let item: T = serde_json::from_str(trimmed)
            .map_err(|e| format!("解析第 {} 行 JSON 失败: {}", i + 1, e))?;
        items.push(item);
    }
    Ok(items)
}

pub(crate) fn is_docker_perm_denied(out: &str) -> bool {
    let lower = out.to_lowercase();
    lower.contains("permission denied") && lower.contains("docker.sock")
}

/// 执行列表类命令,返回解析后的 Vec;非 0 退出码 → Err(含权限兜底,挂码)。
pub(crate) async fn exec_json_list<T: serde::de::DeserializeOwned>(
    client: &mut SshClient,
    cmd: &str,
) -> Result<Vec<T>, String> {
    let (code, out) = exec_collect(client, cmd).await?;
    if code != 0 {
        if is_docker_perm_denied(&out) {
            return Err(crate::errors::perm_denied());
        }
        // 退出码 -1 是 exec 未收到 ExitStatus 的默认值(连接已断):Transport
        let cls = if code == -1 {
            crate::errors::ErrCode::Transport
        } else {
            crate::errors::ErrCode::Protocol
        };
        return Err(crate::errors::tagged(
            cls,
            format!("命令执行失败(退出码 {}): {}", code, out.trim()),
        ));
    }
    parse_ndjson(&out)
}

/// 执行操作类命令,返回 ActionResult(非 0 退出码视为操作失败,带回输出原文)。
pub(crate) async fn exec_action(client: &mut SshClient, cmd: &str) -> Result<ActionResult, String> {
    let (code, out) = exec_collect(client, cmd).await?;
    if code != 0 {
        return Ok(ActionResult {
            success: false,
            message: out.trim().to_string(),
        });
    }
    Ok(ActionResult {
        success: true,
        message: "操作成功".to_string(),
    })
}

// ===== Docker NDJSON 输出结构(PascalCase,仅 Deserialize) =====

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawContainer {
    #[serde(rename = "ID")]
    id: String,
    #[serde(default)]
    names: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    ports: String,
    #[serde(default)]
    created_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawImage {
    #[serde(default)]
    repository: String,
    #[serde(default)]
    tag: String,
    #[serde(rename = "ID")]
    id: String,
    #[serde(default)]
    size: String,
    #[serde(default)]
    created_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawDockerInfo {
    #[serde(default)]
    server_version: String,
    #[serde(default)]
    operating_system: String,
    #[serde(default)]
    kernel_version: String,
    #[serde(default)]
    architecture: String,
    #[serde(default)]
    containers: i64,
    #[serde(default)]
    containers_running: i64,
    #[serde(default)]
    containers_paused: i64,
    #[serde(default)]
    containers_stopped: i64,
    #[serde(default)]
    images: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawSystemDf {
    #[serde(rename = "Type")]
    entry_type: String,
    #[serde(default)]
    size: String,
}

// ===== 返回前端结构(snake_case,Serialize) =====

#[derive(Debug, Serialize)]
pub struct ServerBrief {
    id: String,
    name: String,
    host: String,
}

#[derive(Debug, Serialize)]
pub struct ManageOverview {
    docker_version: String,
    os: String,
    kernel: String,
    arch: String,
    containers_running: String,
    containers_paused: String,
    containers_stopped: String,
    containers_total: String,
    images_total: String,
    disk_used: String,
    /// 宿主机 CPU 占用百分比(两次 /proc/stat 采样差分;取不到为空串 → 前端「—」)
    cpu_percent: String,
    /// 宿主机逻辑核心数(nproc)
    cpu_cores: String,
    /// 宿主机内存已用(人类可读)
    mem_used: String,
    /// 宿主机内存总量(人类可读)
    mem_total: String,
    /// 宿主机内存占用百分比(四舍五入整数)
    mem_percent: String,
    /// 宿主机根文件系统磁盘已用(人类可读;采样不可用为空串 → 前端不显示该行)
    root_disk_used: String,
    /// 宿主机根文件系统磁盘总量(人类可读)
    root_disk_total: String,
    /// 宿主机根文件系统占用百分比(四舍五入整数)
    root_disk_percent: String,
    /// Docker 数据目录(/var/lib/docker)所在文件系统已用 —— 仅当它是独立挂载点
    /// (与根分区不同文件系统)时非空,否则前端不追加第二行
    docker_disk_used: String,
    /// Docker 数据目录所在文件系统总量(人类可读)
    docker_disk_total: String,
    /// Docker 数据目录所在文件系统占用百分比(四舍五入整数)
    docker_disk_percent: String,
    /// Docker 数据目录的挂载点路径(展示用,如 /mnt/data)
    docker_disk_mount: String,
}

#[derive(Debug, Serialize)]
pub struct ActionResult {
    pub(crate) success: bool,
    pub(crate) message: String,
}

#[derive(Debug, Serialize)]
pub struct ContainerRow {
    id: String,
    names: String,
    image: String,
    state: String,
    status: String,
    ports: String,
    created_at: String,
}

#[derive(Debug, Serialize)]
pub struct ImageRow {
    repository: String,
    tag: String,
    id: String,
    size: String,
    created_at: String,
}

// ===== 10 个 Tauri 命令 =====

#[tauri::command]
pub async fn manage_list_servers() -> Result<Vec<ServerBrief>, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    Ok(cfg
        .servers
        .iter()
        .map(|s| ServerBrief {
            id: s.id.clone(),
            name: s.name.clone(),
            host: s.host.clone(),
        })
        .collect())
}

#[tauri::command]
pub async fn manage_overview(
    server_id: String,
    password_plain: Option<String>,
) -> Result<ManageOverview, String> {
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;

    // 1. docker info --format json(单对象,一次连接内顺序执行)
    let info_out = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取 Docker 信息超时",
        "请检查服务器 Docker 状态后重试",
        async {
            let (code, out) = exec_collect(&mut client, "docker info --format json").await?;
            if code != 0 {
                if is_docker_perm_denied(&out) {
                    return Err(crate::errors::perm_denied());
                }
                return Err(format!("docker info 失败(退出码 {}): {}", code, out.trim()));
            }
            Ok(out)
        },
    )
    .await?;

    let info: RawDockerInfo = serde_json::from_str(info_out.trim())
        .map_err(|e| format!("解析 docker info JSON 失败: {}", e))?;

    // 2. docker system df --format json(NDJSON)
    let df_entries: Vec<RawSystemDf> = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取磁盘占用超时",
        "请检查服务器 Docker 状态后重试",
        exec_json_list(&mut client, "docker system df --format json"),
    )
    .await?;

    let disk_parts: Vec<String> = df_entries
        .iter()
        .filter(|e| !e.size.is_empty() && e.size != "0B")
        .map(|e| format!("{}: {}", e.entry_type, e.size))
        .collect();
    let disk_used = if disk_parts.is_empty() {
        "0B".to_string()
    } else {
        disk_parts.join(" | ")
    };

    // 3. 宿主机性能采样(概览指标):单次 exec —— /proc/stat 两次采样(间隔 1s)
    //    差分算 CPU 占用,/proc/meminfo 取内存,nproc 取核心数;/proc 不存在的
    //    系统(macOS 等)解析为空,前端按「—」降级。内部含 1s 采样间隔,超时放宽
    let (_, host_out) = with_timeout(
        EXEC_TIMEOUT_SECS + 5,
        "获取宿主机性能超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &host_metrics_cmd()),
    )
    .await
    .unwrap_or((1, String::new()));
    let host = parse_host_metrics(&host_out);
    let mem_percent = match (host.mem_used, host.mem_total) {
        (Some(used), Some(total)) if total > 0 => {
            Some(((used as f64) / (total as f64) * 100.0).round() as u64)
        }
        _ => None,
    };
    let root_disk = host.root_disk.as_ref();
    let docker_disk = host.docker_disk.as_ref();

    Ok(ManageOverview {
        docker_version: info.server_version,
        os: info.operating_system,
        kernel: info.kernel_version,
        arch: info.architecture,
        containers_running: info.containers_running.to_string(),
        containers_paused: info.containers_paused.to_string(),
        containers_stopped: info.containers_stopped.to_string(),
        containers_total: info.containers.to_string(),
        images_total: info.images.to_string(),
        disk_used,
        cpu_percent: host
            .cpu_percent
            .map(|p| format!("{p:.1}%"))
            .unwrap_or_default(),
        cpu_cores: host.cores.map(|c| c.to_string()).unwrap_or_default(),
        mem_used: host.mem_used.map(format_bytes_metric).unwrap_or_default(),
        mem_total: host.mem_total.map(format_bytes_metric).unwrap_or_default(),
        mem_percent: mem_percent.map(|p| format!("{p}%")).unwrap_or_default(),
        root_disk_used: root_disk
            .map(|d| format_bytes_metric(d.used_kb * 1024))
            .unwrap_or_default(),
        root_disk_total: root_disk
            .map(|d| format_bytes_metric(d.total_kb * 1024))
            .unwrap_or_default(),
        root_disk_percent: root_disk
            .and_then(|d| disk_percent(d.used_kb, d.total_kb))
            .map(|p| format!("{p}%"))
            .unwrap_or_default(),
        docker_disk_used: docker_disk
            .map(|d| format_bytes_metric(d.used_kb * 1024))
            .unwrap_or_default(),
        docker_disk_total: docker_disk
            .map(|d| format_bytes_metric(d.total_kb * 1024))
            .unwrap_or_default(),
        docker_disk_percent: docker_disk
            .and_then(|d| disk_percent(d.used_kb, d.total_kb))
            .map(|p| format!("{p}%"))
            .unwrap_or_default(),
        docker_disk_mount: docker_disk
            .map(|d| d.mount.clone())
            .unwrap_or_default(),
    })
}

// ===== 宿主机性能采样(概览指标;纯函数,便于单测)=====

/// 拼宿主机性能采样命令:单次 exec 完成 —— /proc/stat 两次采样(间隔 1s)
/// 差分算 CPU 占用,/proc/meminfo 取内存总量与可用量,nproc 取核心数,
/// `df -kP` 取根分区与 /var/lib/docker 所在文件系统用量(-P POSIX 格式,
/// 长设备名不折行;目录不存在时该行缺失,stdout 仍有根分区行)。
/// 标记行 `==CPU1/==MEM/==NPROC/==DISK/==CPU2` 供解析切分;grep/df 全部
/// `2>/dev/null`,/proc 缺失的系统输出为空段(解析为空 → 前端「—」)。
pub(crate) fn host_metrics_cmd() -> String {
    "echo '==CPU1'; grep '^cpu ' /proc/stat 2>/dev/null; \
     echo '==MEM'; grep -E '^(MemTotal|MemAvailable|MemFree|Buffers|Cached):' /proc/meminfo 2>/dev/null; \
     echo '==NPROC'; nproc 2>/dev/null; \
     echo '==DISK'; df -kP / /var/lib/docker 2>/dev/null; \
     sleep 1; echo '==CPU2'; grep '^cpu ' /proc/stat 2>/dev/null"
        .to_string()
}

/// [`host_metrics_cmd`] 的解析产出。
#[derive(Debug, Default, PartialEq)]
struct HostMetrics {
    /// 两次 /proc/stat 采样差分的 CPU 占用(0-100,一位小数)
    cpu_percent: Option<f64>,
    /// 逻辑核心数(nproc)
    cores: Option<u32>,
    /// 内存总量(字节)
    mem_total: Option<u64>,
    /// 内存已用(字节;= total - available,无 available 时 = total - free - buffers - cached)
    mem_used: Option<u64>,
    /// 根分区(/)所在文件系统样本(KB 口径,来自 df -kP)
    root_disk: Option<DfSample>,
    /// Docker 数据目录所在文件系统样本 —— 仅当 /var/lib/docker 本身是挂载点
    /// (独立文件系统)时为 Some;与根分区同盘时 df 报告的挂载点是 /,归入 root
    docker_disk: Option<DfSample>,
}

/// `df -kP` 单行解析样本(1024-blocks 口径)。
#[derive(Debug, Default, PartialEq)]
struct DfSample {
    total_kb: u64,
    used_kb: u64,
    mount: String,
}

/// 解析宿主机性能采样输出(纯函数,便于单测)。
fn parse_host_metrics(out: &str) -> HostMetrics {
    let mut m = HostMetrics::default();
    const S_NONE: u8 = 0;
    const S_CPU1: u8 = 1;
    const S_MEM: u8 = 2;
    const S_NPROC: u8 = 3;
    const S_DISK: u8 = 4;
    const S_CPU2: u8 = 5;
    let mut kind = S_NONE;
    let mut cpu1 = String::new();
    let mut cpu2 = String::new();
    let mut mem_total_kb = None::<u64>;
    let mut mem_avail_kb = None::<u64>;
    let mut mem_free_kb = None::<u64>;
    let mut buffers_kb = None::<u64>;
    let mut cached_kb = None::<u64>;
    for line in out.lines() {
        match line.trim() {
            "==CPU1" => {
                kind = S_CPU1;
                continue;
            }
            "==MEM" => {
                kind = S_MEM;
                continue;
            }
            "==NPROC" => {
                kind = S_NPROC;
                continue;
            }
            "==DISK" => {
                kind = S_DISK;
                continue;
            }
            "==CPU2" => {
                kind = S_CPU2;
                continue;
            }
            _ => {}
        }
        match kind {
            S_CPU1 => cpu1 = line.trim().to_string(),
            S_CPU2 => cpu2 = line.trim().to_string(),
            S_NPROC => m.cores = line.trim().parse().ok(),
            S_DISK => {
                if let Some(s) = split_df_line(line) {
                    if s.mount == "/var/lib/docker" {
                        // /var/lib/docker 自身是挂载点 = Docker 数据在独立文件系统
                        m.docker_disk = Some(s);
                    } else if m.root_disk.is_none() {
                        // df 对每个参数各输出一行;同盘时两行挂载点都是 /,首行即根分区
                        m.root_disk = Some(s);
                    }
                }
            }
            S_MEM => {
                if let Some((key, value)) = split_meminfo_line(line) {
                    match key.as_str() {
                        "MemTotal" => mem_total_kb = Some(value),
                        "MemAvailable" => mem_avail_kb = Some(value),
                        "MemFree" => mem_free_kb = Some(value),
                        "Buffers" => buffers_kb = Some(value),
                        "Cached" => cached_kb = Some(value),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    m.cpu_percent = cpu_percent_between(&cpu1, &cpu2);
    if let Some(total) = mem_total_kb {
        m.mem_total = Some(total * 1024);
        // MemAvailable(3.14+ 内核)优先;老内核回退 free + buffers + cached
        let avail = mem_avail_kb.or_else(|| {
            Some(mem_free_kb.unwrap_or(0) + buffers_kb.unwrap_or(0) + cached_kb.unwrap_or(0))
        });
        if let Some(avail) = avail {
            m.mem_used = Some(total.saturating_sub(avail) * 1024);
        }
    }
    m
}

/// 切 meminfo 行的「键 值 单位」(纯函数):`MemTotal:  16332000 kB` → ("MemTotal", 16332000)。
fn split_meminfo_line(line: &str) -> Option<(String, u64)> {
    let (key, rest) = line.split_once(':')?;
    let value = rest.trim().split_whitespace().next()?.parse().ok()?;
    Some((key.trim().to_string(), value))
}

/// 切 `df -kP` 行(纯函数):`/dev/sda1 40203644 8372440 29787180 21% /`
/// → DfSample { total_kb: 40203644, used_kb: 8372440, mount: "/" }。
/// 首列设备名按惯例不含空白;挂载点理论上可含空格,故自第 6 列起重新拼接。
/// 表头(`Filesystem 1024-blocks …`,第 2 列非数字)与残行 → None。
fn split_df_line(line: &str) -> Option<DfSample> {
    let t: Vec<&str> = line.split_whitespace().collect();
    if t.len() < 6 {
        return None;
    }
    let total_kb = t[1].parse().ok()?;
    let used_kb = t[2].parse().ok()?;
    Some(DfSample {
        total_kb,
        used_kb,
        mount: t[5..].join(" "),
    })
}

/// 磁盘占用百分比(四舍五入整数;total 为 0 → None)。纯函数便于单测。
fn disk_percent(used_kb: u64, total_kb: u64) -> Option<u64> {
    if total_kb == 0 {
        return None;
    }
    Some(((used_kb as f64) / (total_kb as f64) * 100.0).round() as u64)
}

/// 用两次 /proc/stat `cpu ` 汇总行差分计算 CPU 占用百分比(纯函数,便于单测)。
/// busy = Δtotal - Δidle - Δiowait,占用 = busy / Δtotal;Δtotal 为 0 或解析
/// 失败(空段/字段不足)→ None。
fn cpu_percent_between(prev: &str, next: &str) -> Option<f64> {
    fn jiffies(line: &str) -> Option<(u64, u64)> {
        // 严格匹配 "cpu "(带空格)的汇总行;"cpu0" 等每核心行不算
        let v = line.strip_prefix("cpu ")?.trim();
        let nums: Vec<u64> = v.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if nums.is_empty() {
            return None;
        }
        let total: u64 = nums.iter().sum();
        let idle = nums.get(3).copied().unwrap_or(0) + nums.get(4).copied().unwrap_or(0);
        Some((total, idle))
    }
    let (p_total, p_idle) = jiffies(prev)?;
    let (n_total, n_idle) = jiffies(next)?;
    let d_total = n_total.checked_sub(p_total)?;
    let d_idle = n_idle.checked_sub(p_idle)?;
    if d_total == 0 {
        return None;
    }
    let busy = d_total.saturating_sub(d_idle);
    Some((busy as f64 / d_total as f64 * 1000.0).round() / 10.0)
}

/// 字节 → 人类可读(≥1GB 显示 GB 一位小数,否则 MB 取整)。
fn format_bytes_metric(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[tauri::command]
pub async fn manage_list_containers(
    server_id: String,
    password_plain: Option<String>,
) -> Result<Vec<ContainerRow>, String> {
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let raw: Vec<RawContainer> = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取容器列表超时",
        "请检查服务器网络后重试",
        exec_json_list(&mut client, "docker ps -a --format json"),
    )
    .await?;
    Ok(raw
        .into_iter()
        .map(|c| ContainerRow {
            id: c.id,
            names: c.names,
            image: c.image,
            state: c.state,
            status: c.status,
            ports: c.ports,
            created_at: c.created_at,
        })
        .collect())
}

#[tauri::command]
pub async fn manage_container_inspect(
    server_id: String,
    password_plain: Option<String>,
    container_id: String,
) -> Result<serde_json::Value, String> {
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let cmd = format!("docker inspect {}", shell_quote(&container_id));
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取容器详情超时",
        "请检查服务器网络后重试",
        async { exec_collect(&mut client, &cmd).await },
    )
    .await?;
    if code != 0 {
        if is_docker_perm_denied(&out) {
            return Err(crate::errors::perm_denied());
        }
        return Err(format!("docker inspect 失败(退出码 {}): {}", code, out.trim()));
    }
    let value: serde_json::Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("解析 inspect JSON 失败: {}", e))?;
    Ok(value)
}

#[tauri::command]
pub async fn manage_container_action(
    server_id: String,
    password_plain: Option<String>,
    container_id: String,
    action: String,
) -> Result<ActionResult, String> {
    let docker_cmd = match action.as_str() {
        "start" => format!("docker start {}", shell_quote(&container_id)),
        "stop" => format!("docker stop {}", shell_quote(&container_id)),
        "restart" => format!("docker restart {}", shell_quote(&container_id)),
        // 删除运行中容器用 docker rm -f(对已停止容器同样有效)
        "rm" => format!("docker rm -f {}", shell_quote(&container_id)),
        other => return Err(format!("不支持的容器操作: {}", other)),
    };
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        EXEC_TIMEOUT_SECS,
        "容器操作超时",
        "请检查服务器网络后重试",
        exec_action(&mut client, &docker_cmd),
    )
    .await
}

#[tauri::command]
pub async fn manage_container_logs(
    server_id: String,
    password_plain: Option<String>,
    container_id: String,
    tail: u32,
) -> Result<String, String> {
    let tail_arg = if tail == 0 {
        "all".to_string()
    } else {
        tail.to_string()
    };
    let cmd = format!(
        "docker logs --tail {} {}",
        tail_arg,
        shell_quote(&container_id)
    );
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取容器日志超时",
        "请检查服务器网络后重试",
        async { exec_collect(&mut client, &cmd).await },
    )
    .await?;
    if code != 0 {
        if is_docker_perm_denied(&out) {
            return Err(crate::errors::perm_denied());
        }
        return Err(format!("docker logs 失败(退出码 {}): {}", code, out.trim()));
    }
    Ok(out)
}

#[tauri::command]
pub async fn manage_list_images(
    server_id: String,
    password_plain: Option<String>,
) -> Result<Vec<ImageRow>, String> {
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let raw: Vec<RawImage> = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取镜像列表超时",
        "请检查服务器网络后重试",
        exec_json_list(&mut client, "docker images --format json"),
    )
    .await?;
    Ok(raw
        .into_iter()
        .map(|i| ImageRow {
            repository: i.repository,
            tag: i.tag,
            id: i.id,
            size: i.size,
            created_at: i.created_at,
        })
        .collect())
}

#[tauri::command]
pub async fn manage_image_pull(
    server_id: String,
    password_plain: Option<String>,
    image: String,
) -> Result<ActionResult, String> {
    if image.trim().is_empty() {
        return Err("镜像名不能为空".to_string());
    }
    let cmd = format!("docker pull {}", shell_quote(&image));
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        PULL_TIMEOUT_SECS,
        "拉取镜像超时",
        "镜像较大或网络较慢,请稍后重试",
        exec_action(&mut client, &cmd),
    )
    .await
}

#[tauri::command]
pub async fn manage_image_remove(
    server_id: String,
    password_plain: Option<String>,
    image_id: String,
    force: bool,
) -> Result<ActionResult, String> {
    let flag = if force { " -f" } else { "" };
    let cmd = format!("docker rmi{} {}", flag, shell_quote(&image_id));
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        EXEC_TIMEOUT_SECS,
        "删除镜像超时",
        "请检查服务器网络后重试",
        exec_action(&mut client, &cmd),
    )
    .await
}

#[tauri::command]
pub async fn manage_image_tag(
    server_id: String,
    password_plain: Option<String>,
    image: String,
    new_tag: String,
) -> Result<ActionResult, String> {
    if new_tag.trim().is_empty() {
        return Err("新标签不能为空".to_string());
    }
    let cmd = format!(
        "docker tag {} {}",
        shell_quote(&image),
        shell_quote(&new_tag)
    );
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        EXEC_TIMEOUT_SECS,
        "打标签超时",
        "请检查服务器网络后重试",
        exec_action(&mut client, &cmd),
    )
    .await
}

// ============================================================
// B 阶段:卷(Volume)与网络(Network)管理 —— 纯追加
// ============================================================

// ===== Docker NDJSON 输出结构(B 阶段追加) =====

/// `docker volume ls --format json` 每行一个对象;CreatedAt 仅 Docker 25+ 提供,缺失时为空
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawVolume {
    #[serde(rename = "Name")]
    name: String,
    #[serde(default)]
    driver: String,
    #[serde(default)]
    mountpoint: String,
    #[serde(default)]
    created_at: String,
}

/// `docker network ls --format json` 每行一个对象
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawNetwork {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(default)]
    driver: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    created_at: String,
}

/// `docker network inspect <names...>` 数组元素:Containers 是 {容器ID: {...}} 映射,用于统计已连接容器数
#[derive(Debug, Deserialize)]
struct RawNetworkInspect {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Containers", default)]
    containers: std::collections::HashMap<String, serde_json::Value>,
}

// ===== 返回前端结构(B 阶段追加,snake_case) =====

#[derive(Debug, Serialize)]
pub struct VolumeRow {
    name: String,
    driver: String,
    mountpoint: String,
    created_at: String,
}

#[derive(Debug, Serialize)]
pub struct NetworkRow {
    id: String,
    name: String,
    driver: String,
    scope: String,
    created_at: String,
    containers: u32,
}

// ===== 卷命令 =====

#[tauri::command]
pub async fn manage_list_volumes(
    server_id: String,
    password_plain: Option<String>,
) -> Result<Vec<VolumeRow>, String> {
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let raw: Vec<RawVolume> = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取卷列表超时",
        "请检查服务器网络后重试",
        exec_json_list(&mut client, "docker volume ls --format json"),
    )
    .await?;
    Ok(raw
        .into_iter()
        .map(|v| VolumeRow {
            name: v.name,
            driver: v.driver,
            mountpoint: v.mountpoint,
            created_at: v.created_at,
        })
        .collect())
}

#[tauri::command]
pub async fn manage_volume_inspect(
    server_id: String,
    password_plain: Option<String>,
    volume_name: String,
) -> Result<serde_json::Value, String> {
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let cmd = format!("docker volume inspect {}", shell_quote(&volume_name));
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取卷详情超时",
        "请检查服务器网络后重试",
        async { exec_collect(&mut client, &cmd).await },
    )
    .await?;
    if code != 0 {
        if is_docker_perm_denied(&out) {
            return Err(crate::errors::perm_denied());
        }
        return Err(format!("docker volume inspect 失败(退出码 {}): {}", code, out.trim()));
    }
    let value: serde_json::Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("解析卷 inspect JSON 失败: {}", e))?;
    Ok(value)
}

#[tauri::command]
pub async fn manage_volume_create(
    server_id: String,
    password_plain: Option<String>,
    volume_name: String,
    driver: Option<String>,
) -> Result<ActionResult, String> {
    let name = volume_name.trim().to_string();
    if name.is_empty() {
        return Err("卷名称不能为空".to_string());
    }
    let mut cmd = "docker volume create".to_string();
    if let Some(d) = driver.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            cmd.push_str(&format!(" --driver {}", shell_quote(d)));
        }
    }
    cmd.push(' ');
    cmd.push_str(&shell_quote(&name));
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        EXEC_TIMEOUT_SECS,
        "创建卷超时",
        "请检查服务器网络后重试",
        exec_action(&mut client, &cmd),
    )
    .await
}

#[tauri::command]
pub async fn manage_volume_remove(
    server_id: String,
    password_plain: Option<String>,
    volume_name: String,
) -> Result<ActionResult, String> {
    let cmd = format!("docker volume rm {}", shell_quote(&volume_name));
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        EXEC_TIMEOUT_SECS,
        "删除卷超时",
        "请检查服务器网络后重试",
        exec_action(&mut client, &cmd),
    )
    .await
}

// ===== 网络命令 =====

#[tauri::command]
pub async fn manage_list_networks(
    server_id: String,
    password_plain: Option<String>,
) -> Result<Vec<NetworkRow>, String> {
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let raw: Vec<RawNetwork> = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取网络列表超时",
        "请检查服务器网络后重试",
        exec_json_list(&mut client, "docker network ls --format json"),
    )
    .await?;

    // 一次连接内追加 inspect 全部网络,统计各网络已连接容器数;失败时按 0 计(不阻塞列表展示)
    let names: Vec<String> = raw.iter().map(|n| n.name.clone()).collect();
    let mut container_counts: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    if !names.is_empty() {
        let inspect_cmd = format!(
            "docker network inspect {}",
            names
                .iter()
                .map(|n| shell_quote(n))
                .collect::<Vec<_>>()
                .join(" ")
        );
        if let Ok((0, out)) = exec_collect(&mut client, &inspect_cmd).await {
            if let Ok(list) = serde_json::from_str::<Vec<RawNetworkInspect>>(out.trim()) {
                for net in list {
                    container_counts.insert(net.name, net.containers.len() as u32);
                }
            }
        }
    }

    Ok(raw
        .into_iter()
        .map(|n| {
            let containers = container_counts.get(&n.name).copied().unwrap_or(0);
            NetworkRow {
                id: n.id,
                name: n.name,
                driver: n.driver,
                scope: n.scope,
                created_at: n.created_at,
                containers,
            }
        })
        .collect())
}

#[tauri::command]
pub async fn manage_network_inspect(
    server_id: String,
    password_plain: Option<String>,
    network_id: String,
) -> Result<serde_json::Value, String> {
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let cmd = format!("docker network inspect {}", shell_quote(&network_id));
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取网络详情超时",
        "请检查服务器网络后重试",
        async { exec_collect(&mut client, &cmd).await },
    )
    .await?;
    if code != 0 {
        if is_docker_perm_denied(&out) {
            return Err(crate::errors::perm_denied());
        }
        return Err(format!("docker network inspect 失败(退出码 {}): {}", code, out.trim()));
    }
    let value: serde_json::Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("解析网络 inspect JSON 失败: {}", e))?;
    Ok(value)
}

#[tauri::command]
pub async fn manage_network_create(
    server_id: String,
    password_plain: Option<String>,
    network_name: String,
    driver: Option<String>,
) -> Result<ActionResult, String> {
    let name = network_name.trim().to_string();
    if name.is_empty() {
        return Err("网络名称不能为空".to_string());
    }
    let mut cmd = "docker network create".to_string();
    if let Some(d) = driver.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            cmd.push_str(&format!(" --driver {}", shell_quote(d)));
        }
    }
    cmd.push(' ');
    cmd.push_str(&shell_quote(&name));
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        EXEC_TIMEOUT_SECS,
        "创建网络超时",
        "请检查服务器网络后重试",
        exec_action(&mut client, &cmd),
    )
    .await
}

#[tauri::command]
pub async fn manage_network_remove(
    server_id: String,
    password_plain: Option<String>,
    network_id: String,
) -> Result<ActionResult, String> {
    let cmd = format!("docker network rm {}", shell_quote(&network_id));
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        EXEC_TIMEOUT_SECS,
        "删除网络超时",
        "请检查服务器网络后重试",
        exec_action(&mut client, &cmd),
    )
    .await
}

#[tauri::command]
pub async fn manage_network_connect(
    server_id: String,
    password_plain: Option<String>,
    network_id: String,
    container_id: String,
) -> Result<ActionResult, String> {
    if container_id.trim().is_empty() {
        return Err("容器名/ID 不能为空".to_string());
    }
    let cmd = format!(
        "docker network connect {} {}",
        shell_quote(&network_id),
        shell_quote(container_id.trim())
    );
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        EXEC_TIMEOUT_SECS,
        "连接容器超时",
        "请检查服务器网络后重试",
        exec_action(&mut client, &cmd),
    )
    .await
}

#[tauri::command]
pub async fn manage_network_disconnect(
    server_id: String,
    password_plain: Option<String>,
    network_id: String,
    container_id: String,
) -> Result<ActionResult, String> {
    if container_id.trim().is_empty() {
        return Err("容器名/ID 不能为空".to_string());
    }
    let cmd = format!(
        "docker network disconnect {} {}",
        shell_quote(&network_id),
        shell_quote(container_id.trim())
    );
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        EXEC_TIMEOUT_SECS,
        "断开容器超时",
        "请检查服务器网络后重试",
        exec_action(&mut client, &cmd),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_percent_between() {
        // 常规差分:Δtotal=300, Δidle=100, busy=200 → 66.7%
        let p = cpu_percent_between(
            "cpu  100 0 100 700 0 0 0 0 0 0",
            "cpu  200 0 200 800 0 0 0 0 0 0",
        )
        .unwrap();
        assert!((p - 66.7).abs() < 0.1, "实际 {p}");
        // 计数器无增长(采样过快)→ None
        assert_eq!(
            cpu_percent_between(
                "cpu  100 0 100 700 0 0 0 0 0 0",
                "cpu  100 0 100 700 0 0 0 0 0 0",
            ),
            None
        );
        // 空段 / 非 cpu 行 → None
        assert_eq!(cpu_percent_between("", ""), None);
        assert_eq!(cpu_percent_between("cpu0 1 2 3", "cpu0 4 5 6"), None);
    }

    #[test]
    fn test_parse_host_metrics() {
        let out = "==CPU1\n\
                   cpu  100 0 100 700 0 0 0 0 0 0\n\
                   ==MEM\n\
                   MemTotal:       16088764 kB\n\
                   MemAvailable:    9905408 kB\n\
                   MemFree:         5204300 kB\n\
                   Buffers:          322148 kB\n\
                   Cached:          2210844 kB\n\
                   ==NPROC\n\
                   8\n\
                   ==DISK\n\
                   Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                   /dev/sda1  40203644 8372440 29787180 21% /\n\
                   /dev/sda1  40203644 8372440 29787180 21% /\n\
                   ==CPU2\n\
                   cpu  150 0 150 750 0 0 0 0 0 0\n";
        let m = parse_host_metrics(out);
        assert!((m.cpu_percent.unwrap() - 66.7).abs() < 0.1);
        assert_eq!(m.cores, Some(8));
        assert_eq!(m.mem_total, Some(16088764 * 1024));
        // used = total - available
        assert_eq!(m.mem_used, Some((16088764 - 9905408) * 1024));
        // df 表头被跳过;/ 与 /var/lib/docker 同盘时两行挂载点都是 /,
        // 首行即根分区,Docker 不单列
        let root = m.root_disk.unwrap();
        assert_eq!(root.total_kb, 40203644);
        assert_eq!(root.used_kb, 8372440);
        assert_eq!(root.mount, "/");
        assert_eq!(m.docker_disk, None);
    }

    #[test]
    fn test_parse_host_metrics_docker_separate_mount() {
        // /var/lib/docker 是独立挂载点:df 第二行挂载点为自身路径 → 单列 Docker 盘
        let out = "==DISK\n\
                   /dev/sda1 40203644 8372440 29787180 21% /\n\
                   /dev/vdb1 82067888 12400000 65483544 16% /var/lib/docker\n";
        let m = parse_host_metrics(out);
        let root = m.root_disk.unwrap();
        assert_eq!(root.mount, "/");
        assert_eq!(root.total_kb, 40203644);
        let docker = m.docker_disk.unwrap();
        assert_eq!(docker.mount, "/var/lib/docker");
        assert_eq!(docker.total_kb, 82067888);
        assert_eq!(docker.used_kb, 12400000);
        // 无 DISK 段(/proc 缺失等)→ 磁盘字段为空
        let m = parse_host_metrics("==NPROC\n2\n");
        assert_eq!(m.root_disk, None);
        assert_eq!(m.docker_disk, None);
    }

    #[test]
    fn test_split_df_line_and_disk_percent() {
        // 挂载点含空格:自第 6 列起重新拼接
        let s = split_df_line("/dev/sda1 1000 250 750 25% /mnt/my data").unwrap();
        assert_eq!((s.total_kb, s.used_kb, s.mount.as_str()), (1000, 250, "/mnt/my data"));
        // 表头(第 2 列非数字)与残行 → None
        assert_eq!(split_df_line("Filesystem 1024-blocks Used Available Capacity Mounted on"), None);
        assert_eq!(split_df_line("/dev/sda1 1000 250"), None);
        assert_eq!(split_df_line(""), None);
        // 百分比:常规 / 满 / 空盘
        assert_eq!(disk_percent(250, 1000), Some(25));
        assert_eq!(disk_percent(1000, 1000), Some(100));
        assert_eq!(disk_percent(0, 0), None);
    }

    #[test]
    fn test_parse_host_metrics_fallback_and_missing() {
        // 老内核无 MemAvailable:回退 free + buffers + cached
        let out = "==MEM\n\
                   MemTotal:       1000000 kB\n\
                   MemFree:         400000 kB\n\
                   Buffers:         100000 kB\n\
                   Cached:          200000 kB\n";
        let m = parse_host_metrics(out);
        assert_eq!(m.mem_total, Some(1000000 * 1024));
        assert_eq!(m.mem_used, Some((1000000 - 700000) * 1024));
        assert_eq!(m.cpu_percent, None);
        assert_eq!(m.cores, None);
        // /proc 不存在的系统:全空输出 → 全空
        let m = parse_host_metrics("");
        assert_eq!(m, HostMetrics::default());
    }

    #[test]
    fn test_format_bytes_metric() {
        assert_eq!(format_bytes_metric(512 * 1024 * 1024), "512 MB");
        assert_eq!(format_bytes_metric(2 * 1024 * 1024 * 1024), "2.0 GB");
        assert_eq!(format_bytes_metric(15 * 1024 * 1024 * 1024 + 5), "15.0 GB");
    }
}
