//! 远程 Compose 栈管理模块(C 阶段)。
//!
//! 通过现有 SSH 通道在远程服务器执行 `docker compose` 命令,实现栈(Compose 项目)的
//! 扫描、启停、查看服务状态与日志。
//! 复用 `crate::manage` 中已 pub(crate) 的连接与执行助手,保持低耦合:
//! `connect_server` → `with_timeout` / `shell_quote` / `exec_json_list` / `exec_action`。
//!
//! 输出约定:
//! - `docker compose ps --format json` 输出为 NDJSON(每行一个 JSON 对象),
//!   字段为 PascalCase;栈未启动时输出为空(返回空 Vec 而非错误)。
//! - `docker compose logs` 输出为合并的文本,原样返回。

use serde::{Deserialize, Serialize};

use crate::manage::{
    connect_server, exec_action, exec_json_list, is_docker_perm_denied, shell_quote,
    with_timeout, ActionResult, EXEC_TIMEOUT_SECS, PERM_DENIED_MSG,
};
use crate::ssh::exec_collect;

/// up / down 操作可能涉及镜像拉取与容器重建,超时放宽到 120 秒
const STACK_ACTION_TIMEOUT_SECS: u64 = 120;

// ===== 返回前端结构(snake_case,Serialize) =====

/// 扫描到的栈:dir 为 compose 文件所在目录,compose_file 为完整路径
#[derive(Debug, Serialize)]
pub struct StackRow {
    dir: String,
    compose_file: String,
}

/// `docker compose ps --format json` 解析结果(核心四字段)
#[derive(Debug, Serialize)]
pub struct StackPsRow {
    name: String,
    image: String,
    state: String,
    status: String,
}

// ===== Docker NDJSON 输出结构(PascalCase,仅 Deserialize) =====

/// `docker compose ps --format json` 每行一个对象;部分版本可能带 ID / Publishers / Ports 字段
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawStackPs {
    /// 容忍输出中的 ID 字段(解析时忽略,避免未知字段报错场景;本身不使用)
    #[serde(rename = "ID", default)]
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    status: String,
}

// ===== 内部助手 =====

/// 取 compose 文件的父目录(远端为 Unix 路径,手动按 '/' 分割;无 '/' 时视为当前目录)
fn parent_dir_of(compose_file: &str) -> String {
    match compose_file.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => compose_file[..i].to_string(),
        None => ".".to_string(),
    }
}

/// 构造 `docker compose -f <file> --project-directory <dir>` 前缀
fn compose_prefix(compose_file: &str) -> String {
    let dir = parent_dir_of(compose_file);
    format!(
        "docker compose -f {} --project-directory {}",
        shell_quote(compose_file),
        shell_quote(&dir)
    )
}

// ===== Tauri 命令 =====

/// 扫描 remote_dir 下(深度 ≤2)的 compose 文件,返回栈列表
#[tauri::command]
pub async fn manage_list_stacks(
    server_id: String,
    password_plain: Option<String>,
) -> Result<Vec<StackRow>, String> {
    let (server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;

    // find 多 -name 需 \( \) 与 -o 组合;深度 ≤2:remote_dir 本层的文件 + 一层子目录
    let cmd = format!(
        "find {} -maxdepth 2 -type f \\( -name 'docker-compose.yml' -o -name 'docker-compose.yaml' -o -name 'compose.yml' -o -name 'compose.yaml' \\)",
        shell_quote(&server.remote_dir)
    );
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "扫描 Compose 栈超时",
        "请检查服务器网络后重试",
        async { exec_collect(&mut client, &cmd).await },
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "扫描目录「{}」失败(目录可能不存在或无权限): {}",
            server.remote_dir,
            out.trim()
        ));
    }

    // 每行为一个 compose 文件完整路径,按最后一个 '/' 分离目录与文件名
    let mut stacks: Vec<StackRow> = Vec::new();
    for line in out.lines() {
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        let dir = parent_dir_of(path);
        stacks.push(StackRow {
            dir,
            compose_file: path.to_string(),
        });
    }
    stacks.sort_by(|a, b| a.compose_file.cmp(&b.compose_file));
    Ok(stacks)
}

/// 对指定栈执行 up -d / down 操作
#[tauri::command]
pub async fn manage_stack_action(
    server_id: String,
    password_plain: Option<String>,
    compose_file: String,
    action: String,
) -> Result<ActionResult, String> {
    let sub = match action.as_str() {
        "up" => "up -d",
        "down" => "down",
        other => return Err(format!("不支持的栈操作: {}(仅支持 up / down)", other)),
    };
    let cmd = format!("{} {}", compose_prefix(&compose_file), sub);
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    with_timeout(
        STACK_ACTION_TIMEOUT_SECS,
        "栈操作超时",
        "栈较大或涉及镜像拉取,请稍后重试",
        exec_action(&mut client, &cmd),
    )
    .await
}

/// 查看栈内服务状态;栈未启动时输出为空,返回空 Vec
#[tauri::command]
pub async fn manage_stack_ps(
    server_id: String,
    password_plain: Option<String>,
    compose_file: String,
) -> Result<Vec<StackPsRow>, String> {
    let cmd = format!("{} ps --format json", compose_prefix(&compose_file));
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    // compose ps 非 0 退出码(如 "no configuration file")→ Err 原文(含权限兜底);
    // 输出为空(栈未启动)时 exec_json_list 正常返回空 Vec
    let raw: Vec<RawStackPs> = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取栈服务状态超时",
        "请检查服务器网络后重试",
        exec_json_list(&mut client, &cmd),
    )
    .await?;
    Ok(raw
        .into_iter()
        .map(|p| StackPsRow {
            name: p.name,
            image: p.image,
            state: p.state,
            status: p.status,
        })
        .collect())
}

/// 查看栈日志;tail=0 表示取全部(不带 --tail 参数)
#[tauri::command]
pub async fn manage_stack_logs(
    server_id: String,
    password_plain: Option<String>,
    compose_file: String,
    tail: u32,
) -> Result<String, String> {
    let tail_arg = if tail == 0 {
        String::new()
    } else {
        format!(" --tail {}", tail)
    };
    let cmd = format!("{} logs{} 2>&1", compose_prefix(&compose_file), tail_arg);
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let (code, out) = with_timeout(
        EXEC_TIMEOUT_SECS,
        "获取栈日志超时",
        "请检查服务器网络后重试",
        async { exec_collect(&mut client, &cmd).await },
    )
    .await?;
    if code != 0 {
        if is_docker_perm_denied(&out) {
            return Err(PERM_DENIED_MSG.to_string());
        }
        return Err(format!("docker compose logs 失败(退出码 {}): {}", code, out.trim()));
    }
    Ok(out)
}
