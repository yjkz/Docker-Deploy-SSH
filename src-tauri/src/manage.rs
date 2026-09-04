//! 远程 Docker 管理模块(A 阶段 MVP)。
//!
//! 通过现有 SSH 通道在远程服务器执行 docker 命令,实现容器 / 镜像的查看与操作。
//! 低耦合:自实现连接辅助,仅通过公开 API 组合:
//! `config::load_config` → `commands::resolve_password`(pub) →
//! `ssh::SshClient::connect` → `ssh::exec_collect`(pub(crate))。
//!
//! Docker 输出约定:
//! - 列表类命令(`docker ps -a` / `docker images` / `docker system df`)带
//!   `--format json`,输出为 NDJSON(每行一个 JSON 对象),字段为 PascalCase。
//! - `docker info --format json` 输出单个 JSON 对象。
//! - `docker inspect <id>` 输出 JSON 数组(即使只查一个,前端取 `[0]`)。

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::commands::resolve_password;
use crate::config::{load_config, AppConfig, ServerConfig};
use crate::ssh::{exec_collect, SshClient};

// ===== 超时常量 =====
const CONNECT_TIMEOUT_SECS: u64 = 15;
const EXEC_TIMEOUT_SECS: u64 = 60;
const PULL_TIMEOUT_SECS: u64 = 300;

/// 非 root 用户无 docker.sock 权限时的明确中文提示。
const PERM_DENIED_MSG: &str =
    "当前 SSH 用户无 Docker 权限(无法访问 /var/run/docker.sock),请将该用户加入 docker 组或使用 root 用户连接";

// ===== 连接辅助(自实现,不调用 commands.rs 私有函数) =====

fn find_server<'a>(cfg: &'a AppConfig, server_id: &str) -> Result<&'a ServerConfig, String> {
    cfg.servers
        .iter()
        .find(|s| s.id == server_id)
        .ok_or_else(|| format!("未找到 ID 为「{}」的服务器配置", server_id))
}

async fn with_timeout<T>(
    secs: u64,
    desc: &str,
    hint: &str,
    fut: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(res) => res,
        Err(_) => Err(format!("{}({} 秒):{}", desc, secs, hint)),
    }
}

async fn connect_server(
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
    let client = with_timeout(
        CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref()),
    )
    .await?;
    Ok((server, client))
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn parse_ndjson<T: serde::de::DeserializeOwned>(text: &str) -> Result<Vec<T>, String> {
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

fn is_docker_perm_denied(out: &str) -> bool {
    let lower = out.to_lowercase();
    lower.contains("permission denied") && lower.contains("docker.sock")
}

/// 执行列表类命令,返回解析后的 Vec;非 0 退出码 → Err(含权限兜底)。
async fn exec_json_list<T: serde::de::DeserializeOwned>(
    client: &mut SshClient,
    cmd: &str,
) -> Result<Vec<T>, String> {
    let (code, out) = exec_collect(client, cmd).await?;
    if code != 0 {
        if is_docker_perm_denied(&out) {
            return Err(PERM_DENIED_MSG.to_string());
        }
        return Err(format!("命令执行失败(退出码 {}): {}", code, out.trim()));
    }
    parse_ndjson(&out)
}

/// 执行操作类命令,返回 ActionResult(非 0 退出码视为操作失败,带回输出原文)。
async fn exec_action(client: &mut SshClient, cmd: &str) -> Result<ActionResult, String> {
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
}

#[derive(Debug, Serialize)]
pub struct ActionResult {
    success: bool,
    message: String,
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
                    return Err(PERM_DENIED_MSG.to_string());
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
    })
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
            return Err(PERM_DENIED_MSG.to_string());
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
            return Err(PERM_DENIED_MSG.to_string());
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
