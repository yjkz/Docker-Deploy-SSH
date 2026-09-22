//! 文件管理(第三十三批):容器 rootfs / 数据卷 / 部署目录三源 —— 浏览、下载、
//! 上传(含覆盖前本机备份)、文本查看与编辑、新建目录/改名/删除。
//!
//! **通道选择**(本机 Docker 实测):
//! - `docker cp` 是 daemon 侧文件操作 —— 容器里**没有任何二进制**(distroless / scratch)
//!   也能双向传文件;容器**已停止**同样可用(`docker exec` 则不行)。
//! - 列举/改名/删除需要容器内有 shell(`sh` → `busybox sh` 候选链);没有时列举返回
//!   `unsupported`,前端降级为「按完整路径下载/上传」(不静默报错)。
//! - 数据卷经临时容器挂载(`docker run --rm -v <vol>:/v ...`;tar 镜像来自
//!   [`crate::migrate_project::pick_tar_image`]:设置项 `tarImage` 优先),卷内操作
//!   **总有工具可用**。
//! - 部署目录走宿主机命令与 SFTP(不经 docker)。**写权限**(第三十四批(五),用户裁决):
//!   仅当目标路径落在设置项「宿主机可写目录」白名单内(空列表 = 只读,维持第三十三批
//!   口径)且远端 `readlink -f` canonicalize 后仍在白名单内时放行 —— 见
//!   [`ensure_host_write_allowed`];容器/卷源不受此限。
//!
//! 传输统一经服务器 `/tmp/dd-fm-<uuid>/` 中转(容器/卷 ↔ 宿主 cp + SFTP),结束时守卫清理;
//! 进度经 `files-transfer-progress` 事件(完成/失败由命令返回值表达,避免双信号),
//! 取消经 [`manage_files_cancel`] 置位(块级生效)。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::commands::{remote_join, tail_lines};
use crate::manage::{connect_server, shell_quote, with_timeout, EXEC_TIMEOUT_SECS};
use crate::ssh::exec_collect;
use crate::ssh::SshClient;

/// 传输中转根目录(服务器侧)。
pub(crate) const FM_TMP_ROOT: &str = "/tmp/dd-fm";
/// 文本查看/编辑的体积上限(超过只给下载)。
pub(crate) const FM_TEXT_MAX_BYTES: u64 = 512 * 1024;
/// 覆盖前本机备份同文件保留份数。
pub(crate) const FM_BACKUP_KEEP: usize = 3;
/// 列举/单条 fs 操作超时。
const FM_LIST_TIMEOUT_SECS: u64 = 60;
/// 大件传输超时(GB 级容器文件/目录包)。
const FM_TRANSFER_TIMEOUT_SECS: u64 = 1800;

/// 传输取消位(模块级单例;`manage_files_cancel` 置位,传输前后检查)。
static FM_CANCELLED: AtomicBool = AtomicBool::new(false);

fn reset_cancelled() {
    FM_CANCELLED.store(false, Ordering::SeqCst);
}

fn ensure_not_cancelled() -> Result<(), String> {
    if FM_CANCELLED.load(Ordering::SeqCst) {
        return Err(crate::errors::cancelled());
    }
    Ok(())
}

// ===== 数据源 =====

/// 文件源类型(前端传 `container` / `volume` / `deployDir`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Container,
    Volume,
    DeployDir,
}

impl SourceKind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "container" => Ok(Self::Container),
            "volume" => Ok(Self::Volume),
            "deployDir" => Ok(Self::DeployDir),
            other => Err(format!(
                "不支持的文件源: {}(仅支持 container / volume / deployDir)",
                other
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Volume => "volume",
            Self::DeployDir => "deployDir",
        }
    }

    /// 部署目录写权限(第三十四批(五)):由设置项「宿主机可写目录」白名单决定
    /// —— 见 [`ensure_host_write_allowed`];容器 / 卷源恒可写。
    pub fn is_host_source(self) -> bool {
        matches!(self, Self::DeployDir)
    }
}

// ===== 宿主机写权限闸门(第三十四批(五))=====
// 第三十三批「部署目录只读」为用户裁决;本批按用户裁决放开为**白名单可写**:
// 设置项 `hostWritePaths`(config::AppSettings)列出允许写入的绝对路径前缀,
// 空列表 = 维持只读。写操作的目标路径必须在服务器上 `readlink -f`
// canonicalize 后落在某条白名单之内(目录边界匹配)—— 防符号链接逃逸;
// 白名单条目本身也取同一次 canonicalize 结果(允许白名单自身是软链)。
// 容器 / 卷源不经过本闸门。

/// 拼装 canonicalize 命令:逐路径 `readlink -f`,失败(不存在等)回落原字面
/// 路径;输出与输入**同序、一行一个**(纯函数,便于单测)。
///
/// 不用 `--`(BusyBox readlink 不支持);条目由 [`shell_quote`] 单引号包裹,
/// 且调用方保证全部是绝对路径(不以 `-` 开头)。
pub fn canonicalize_paths_cmd(paths: &[String]) -> String {
    let mut quoted: Vec<String> = Vec::with_capacity(paths.len());
    for p in paths {
        quoted.push(shell_quote(p));
    }
    format!(
        "for p in {}; do (readlink -f \"$p\" 2>/dev/null || printf '%s\\n' \"$p\"); done",
        quoted.join(" ")
    )
}

/// 宿主机写权限闸门:目标绝对路径经远端 canonicalize 后必须落在设置项
/// 「宿主机可写目录」白名单内,否则拒绝并给出可操作文案。
pub async fn ensure_host_write_allowed(
    client: &mut SshClient,
    abs_path: &str,
) -> Result<(), String> {
    let allowed = crate::config::normalize_host_write_paths(
        &crate::config::load_app_settings().host_write_paths,
    );
    if allowed.is_empty() {
        return Err(
            "部署目录为只读源:未配置「宿主机可写目录」白名单,不支持上传/新建/改名/删除\
             (可下载或查看;可在设置中心「通用」区添加允许写入的目录)"
                .to_string(),
        );
    }
    let mut paths: Vec<String> = vec![abs_path.to_string()];
    paths.extend(allowed.iter().cloned());
    let cmd = canonicalize_paths_cmd(&paths);
    let (code, out) = exec_collect(client, &cmd).await?;
    if code != 0 {
        return Err(format!(
            "校验可写目录失败(readlink 退出码 {}): {}",
            code,
            out.trim()
        ));
    }
    let lines: Vec<String> = out.lines().map(|l| l.trim().to_string()).collect();
    if lines.len() < paths.len() || lines[0].is_empty() {
        return Err("校验可写目录失败(远端输出不完整)".to_string());
    }
    let mut prefixes: Vec<String> = Vec::new();
    for (i, lit) in allowed.iter().enumerate() {
        prefixes.push(lit.clone());
        if let Some(canon) = lines.get(i + 1) {
            if !canon.is_empty() && canon != lit {
                prefixes.push(canon.clone());
            }
        }
    }
    let target = &lines[0];
    if crate::config::path_within_any(target, &prefixes) {
        return Ok(());
    }
    Err(format!(
        "部署目录为只读源:目标 {} 不在「宿主机可写目录」白名单内(远端实际解析为 {};\
         可在设置中心「通用」区把所需目录加入白名单)",
        abs_path, target
    ))
}

// ===== 纯函数:路径 / 校验 =====

/// 校验并归一化**相对路径**(相对源根):去首尾 `/`,拒绝 `..`、空段与 NUL。
/// 空串 = 源根(合法)。
pub fn normalize_rel_path(input: &str) -> Result<String, String> {
    let trimmed = input.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    for seg in trimmed.split('/') {
        if seg == ".." {
            return Err("路径不合法(不允许 ..)".to_string());
        }
        if seg.is_empty() {
            return Err("路径不合法(出现了空的路径段)".to_string());
        }
        if seg.contains('\0') {
            return Err("路径不合法(含非法字符)".to_string());
        }
    }
    Ok(trimmed.to_string())
}

/// 目标容器/卷/目录的形态校验(拼远端命令前的必做检查)。
pub fn validate_target(kind: SourceKind, target: &str) -> Result<String, String> {
    let t = target.trim();
    if t.is_empty() {
        return Err("目标不能为空".to_string());
    }
    match kind {
        SourceKind::Container => {
            let is_hex =
                t.len() >= 12 && t.len() <= 64 && t.chars().all(|c| c.is_ascii_hexdigit());
            let is_name = t.len() <= 255
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
                && !t.starts_with('-');
            if is_hex || is_name {
                Ok(t.to_string())
            } else {
                Err(format!("容器标识不合法: {}", t))
            }
        }
        SourceKind::Volume => {
            let ok = t.len() <= 255
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
                && !t.starts_with('-')
                && !t.contains("..");
            if ok {
                Ok(t.to_string())
            } else {
                Err(format!("卷名不合法: {}", t))
            }
        }
        SourceKind::DeployDir => {
            if !t.starts_with('/') {
                return Err(format!("部署目录必须是绝对路径: {}", t));
            }
            if t.split('/').any(|seg| seg == "..") {
                return Err("部署目录不合法(不允许 ..)".to_string());
            }
            Ok(t.trim_end_matches('/').to_string())
        }
    }
}

/// 源内相对路径 → **远端绝对路径**(容器 = 容器 FS;卷 = 临时容器 `/v`;目录 = 宿主路径)。
pub fn source_abs_path(kind: SourceKind, target: &str, rel: &str) -> String {
    match kind {
        SourceKind::Container => {
            if rel.is_empty() {
                "/".to_string()
            } else {
                format!("/{}", rel)
            }
        }
        SourceKind::Volume => {
            if rel.is_empty() {
                "/v".to_string()
            } else {
                format!("/v/{}", rel)
            }
        }
        SourceKind::DeployDir => {
            let base = target.trim_end_matches('/');
            if rel.is_empty() {
                base.to_string()
            } else {
                format!("{}/{}", base, rel)
            }
        }
    }
}

/// 相对路径的最后一段(中转与本地落盘命名);空 → `root` 占位。
pub fn rel_file_name(rel: &str) -> String {
    let name = rel.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if name.is_empty() {
        "root".to_string()
    } else {
        name.to_string()
    }
}

// ===== 纯函数:ls 解析 =====

/// 单条 `ls -la` 记录(前端 camelCase)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    /// 权限位原样(如 `drwxr-xr-x`)
    pub mode: String,
    /// 日期原样(展示用,不解析)
    pub mtime: String,
}

/// 解析 `LC_ALL=C ls -la` 输出(纯函数)。
///
/// 形态(GNU 与 BusyBox 一致):`-rw-r--r-- 1 root root 12 Sep 22 16:52 file.txt`
/// 即 5 字段(权限/链接/属主/属组/大小)+ 3 个日期字段 + 文件名(可含空格);
/// `l` 开头是符号链接(名后带 ` -> 目标`,只保留名);`.` / `..` / `total` 行跳过。
pub fn parse_ls_long(out: &str) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    for line in out.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() || line.starts_with("total ") {
            continue;
        }
        let mut it = line.split_whitespace();
        let mode = match it.next() {
            Some(m) if m.len() >= 10 => m.to_string(),
            _ => continue,
        };
        let _links = it.next();
        let _owner = it.next();
        let _group = it.next();
        let size = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let rest: Vec<&str> = it.collect();
        // 日期:整 3 段(月 日 时/年);不足 4 段时把剩余整体当文件名(容错,不丢条目)
        let (mtime, name_tokens) = if rest.len() >= 4 {
            (rest[..3].join(" "), &rest[3..])
        } else {
            (String::new(), &rest[..])
        };
        let raw_name = name_tokens.join(" ");
        if raw_name.is_empty() || raw_name == "." || raw_name == ".." {
            continue;
        }
        let is_link = mode.starts_with('l');
        let name = if is_link {
            raw_name
                .split_once(" -> ")
                .map(|(n, _)| n.to_string())
                .unwrap_or(raw_name)
        } else {
            raw_name
        };
        entries.push(FileEntry {
            name,
            is_dir: mode.starts_with('d'),
            is_link,
            size,
            mode,
            mtime,
        });
    }
    entries
}

// ===== 纯函数:命令拼装 =====

/// 容器内执行脚本的候选命令(`sh` → `busybox sh`;后者仅在前者「没有 shell」时使用)。
pub fn container_exec_cmds(cid: &str, script: &str) -> Vec<String> {
    vec![
        format!(
            "docker exec {} sh -c {}",
            shell_quote(cid),
            shell_quote(script)
        ),
        format!(
            "docker exec {} busybox sh -c {}",
            shell_quote(cid),
            shell_quote(script)
        ),
    ]
}

/// 输出是否表示「容器里没有这个可执行文件」(决定是否退化到候选链下一项)。
/// docker/OCI 专有文案:`exec: "sh": executable file not found in $PATH`。
pub fn looks_like_missing_shell(out: &str) -> bool {
    let lower = out.to_ascii_lowercase();
    lower.contains("executable file not found") || lower.contains("not found in $path")
}

/// 数据卷操作命令:`docker run --rm -v <vol>:/v [-v <staging>:/out] <image> <script>`。
pub fn volume_run_cmd(image: &str, vol: &str, script: &str, staging: Option<&str>) -> String {
    let mut mounts = format!("-v {}", shell_quote(&format!("{}:/v", vol)));
    if let Some(dir) = staging {
        mounts.push_str(&format!(
            " -v {}",
            shell_quote(&format!("{}:/out", dir))
        ));
    }
    format!(
        "docker run --rm {} {} sh -c {}",
        mounts,
        shell_quote(image),
        shell_quote(script)
    )
}

/// 中转目录(服务器侧,按传输 id 隔离)。
pub fn staging_dir(id: &str) -> String {
    format!("{}-{}", FM_TMP_ROOT, id)
}

/// 列举脚本(`LC_ALL=C` 固定英文月份,解析稳定)。
pub fn ls_script(abs_path: &str) -> String {
    format!("LC_ALL=C ls -la {}", shell_quote(abs_path))
}

/// 目录打包命令(服务器侧)。
pub fn tar_dir_cmd(staging: &str, name: &str) -> String {
    format!(
        "tar czf {} -C {} {}",
        shell_quote(&remote_join(staging, &format!("{}.tar.gz", name))),
        shell_quote(staging),
        shell_quote(name)
    )
}

/// 新建目录 / 改名 / 删除的脚本(纯函数;`new_name` 仅 rename 用)。
pub fn fs_op_script(op: &str, abs_path: &str, new_name: Option<&str>) -> Result<String, String> {
    match op {
        "mkdir" => Ok(format!("mkdir -p {}", shell_quote(abs_path))),
        "rename" => {
            let new_name = new_name.unwrap_or("").trim();
            if new_name.is_empty()
                || new_name.contains('/')
                || new_name == "."
                || new_name == ".."
                || new_name.contains('\0')
            {
                return Err("新名称不合法".to_string());
            }
            let parent = abs_path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            let dest = if parent.is_empty() || parent == "/" {
                format!("/{}", new_name)
            } else {
                format!("{}/{}", parent, new_name)
            };
            Ok(format!(
                "mv {} {}",
                shell_quote(abs_path),
                shell_quote(&dest)
            ))
        }
        "delete" => Ok(format!("rm -rf {}", shell_quote(abs_path))),
        other => Err(format!("不支持的文件操作: {}", other)),
    }
}

/// 时间戳目录名(与部署归档同形态)。
pub fn now_ts() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// 本机备份目录:`<config>/fm-backups/<server>/<kind>-<target>/`(同一文件的不同时间戳子目录)。
pub fn fm_backup_dir(config_dir: &Path, server_id: &str, kind: SourceKind, target: &str) -> PathBuf {
    let safe_target: String = target
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    config_dir
        .join("fm-backups")
        .join(server_id)
        .join(format!("{}-{}", kind.as_str(), safe_target))
}

/// 时间戳目录名列表 → 应清理的(保留最新 `keep` 份;名字是 `yyyyMMdd-HHmmss`,字典序=时间序)。
pub fn select_fm_backups_to_prune(names: &[String], keep: usize) -> Vec<String> {
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort();
    sorted.reverse();
    sorted.into_iter().skip(keep).cloned().collect()
}

// ===== 内部助手 =====

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TransferProgress {
    op_id: String,
    phase: String,
    done: u64,
    total: u64,
}

fn emit_progress(app: &AppHandle, op_id: &str, phase: &str, done: u64, total: u64) {
    let _ = app.emit(
        "files-transfer-progress",
        TransferProgress {
            op_id: op_id.to_string(),
            phase: phase.to_string(),
            done,
            total,
        },
    );
}

fn noop_emit() -> Arc<dyn Fn(&str) + Send + Sync> {
    Arc::new(|_| {})
}

/// 执行一条命令并要求退出码 0(失败附输出尾部)。
async fn run_cmd_ok(
    client: &mut SshClient,
    cmd: &str,
    timeout_secs: u64,
    desc: &str,
) -> Result<String, String> {
    ensure_not_cancelled()?;
    let (code, out) = with_timeout(
        timeout_secs,
        desc,
        "请检查服务器网络后重试",
        exec_collect(client, cmd),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "{}(退出码 {}): {}",
            desc,
            code,
            tail_lines(&out, 3)
        ));
    }
    Ok(out)
}

/// 候选链执行:首条成功即返回;仅当「容器没有 shell」时尝试下一条;其余失败原样返回。
async fn run_shell_candidates(
    client: &mut SshClient,
    cmds: &[String],
    desc: &str,
) -> Result<String, String> {
    let mut last: Option<(i32, String)> = None;
    for cmd in cmds {
        ensure_not_cancelled()?;
        let (code, out) = with_timeout(
            FM_LIST_TIMEOUT_SECS,
            desc,
            "请检查服务器网络后重试",
            exec_collect(client, cmd),
        )
        .await?;
        if code == 0 {
            return Ok(out);
        }
        if !looks_like_missing_shell(&out) {
            return Err(format!("{}(退出码 {}): {}", desc, code, tail_lines(&out, 3)));
        }
        last = Some((code, out));
    }
    let (code, out) = last.unwrap_or((-1, String::new()));
    Err(format!(
        "{}(退出码 {},容器内没有可用 shell): {}",
        desc,
        code,
        tail_lines(&out, 2)
    ))
}

/// 中转目录清理守卫(错误/取消路径也执行;失败仅告警)。
struct RemoteDirGuard(String);

impl RemoteDirGuard {
    async fn run(self, client: &mut SshClient) {
        let (code, out) = exec_collect(client, &self.0).await.unwrap_or((1, String::new()));
        if code != 0 {
            log::warn!("文件传输中转目录清理失败: {}", tail_lines(&out, 2));
        }
    }
}

fn cleanup_cmd(staging: &str) -> String {
    format!("rm -rf {}", shell_quote(staging))
}

/// 源内路径是否为目录:
/// - 容器/卷:先物化到中转目录,再用**宿主侧** `test -d` 判断(不依赖容器工具)。
/// - 部署目录:宿主侧直接 `test -d`。
async fn probe_dir(client: &mut SshClient, start: &str, name: &str) -> Result<bool, String> {
    let target = remote_join(start, name);
    let (code, _) = exec_collect(
        client,
        &format!("test -d {}", shell_quote(&target)),
    )
    .await?;
    Ok(code == 0)
}

// ===== 命令:列举 =====

/// 列举结果(前端 camelCase)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileListPayload {
    /// 归一化后的相对路径(空 = 源根)
    pub path: String,
    pub entries: Vec<FileEntry>,
    /// 容器无 shell 时 true:前端降级为「按完整路径下载/上传」
    pub unsupported: bool,
    /// 降级/警告原因
    pub note: String,
}

/// 列举目录(三源统一;容器无 shell 时返回 `unsupported` 而非报错)。
#[tauri::command]
pub async fn manage_files_list(
    server_id: String,
    password_plain: Option<String>,
    kind: String,
    target: String,
    path: Option<String>,
) -> Result<FileListPayload, String> {
    let kind = SourceKind::parse(&kind)?;
    let target = validate_target(kind, &target)?;
    let rel = normalize_rel_path(path.as_deref().unwrap_or(""))?;
    let abs = source_abs_path(kind, &target, &rel);
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;

    let out = match kind {
        SourceKind::Container => {
            let cmds = container_exec_cmds(&target, &ls_script(&abs));
            match run_shell_candidates(&mut client, &cmds, "列举容器目录失败").await {
                Ok(out) => out,
                Err(e) if e.contains("容器内没有可用 shell") => {
                    return Ok(FileListPayload {
                        path: rel,
                        entries: Vec::new(),
                        unsupported: true,
                        note: format!(
                            "该容器内没有可用 shell(如 distroless / scratch):无法列出目录;仍可按完整路径下载与上传。详情:{}",
                            e
                        ),
                    });
                }
                Err(e) if e.contains("is not running") => {
                    return Ok(FileListPayload {
                        path: rel,
                        entries: Vec::new(),
                        unsupported: true,
                        note: "该容器未运行:无法列举目录(下载/上传仍可用 docker cp 通道,但对已停止容器需要完整路径)。启动容器后重试即可浏览。".to_string(),
                    });
                }
                Err(e) => return Err(e),
            }
        }
        SourceKind::Volume => {
            let image = crate::migrate_project::pick_tar_image(&mut client, &noop_emit()).await?;
            let cmd = volume_run_cmd(&image, &target, &ls_script(&abs), None);
            run_cmd_ok(&mut client, &cmd, FM_LIST_TIMEOUT_SECS, "列举卷目录失败").await?
        }
        SourceKind::DeployDir => {
            run_cmd_ok(&mut client, &ls_script(&abs), FM_LIST_TIMEOUT_SECS, "列举部署目录失败")
                .await?
        }
    };

    Ok(FileListPayload {
        path: rel,
        entries: parse_ls_long(&out),
        unsupported: false,
        note: String::new(),
    })
}

// ===== 命令:下载 =====

/// 下载到本机目录;目录类型自动打包为 `<name>.tar.gz`。返回本机落盘路径。
#[tauri::command]
pub async fn manage_files_download(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    kind: String,
    target: String,
    path: String,
    local_dir: String,
) -> Result<String, String> {
    let kind = SourceKind::parse(&kind)?;
    let target = validate_target(kind, &target)?;
    let rel = normalize_rel_path(&path)?;
    if rel.is_empty() {
        return Err("请进入具体目录后选择要下载的条目(不支持整源打包)".to_string());
    }
    let abs = source_abs_path(kind, &target, &rel);
    let name = rel_file_name(&rel);
    let op_id = uuid::Uuid::new_v4().to_string();
    let local_dir = PathBuf::from(local_dir.trim());
    if !local_dir.is_dir() {
        return Err(format!("本机目录不存在: {}", local_dir.display()));
    }
    reset_cancelled();

    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let staging = staging_dir(&op_id);
    let cleanup = RemoteDirGuard(cleanup_cmd(&staging));

    let result: Result<String, String> = async {
        // 1) 判定目录并物化到「远端可直接下载的路径」
        let (remote_file, local_name, is_dir) = match kind {
            SourceKind::Container => {
                run_cmd_ok(
                    &mut client,
                    &format!("mkdir -p {}", shell_quote(&staging)),
                    EXEC_TIMEOUT_SECS,
                    "创建中转目录失败",
                )
                .await?;
                run_cmd_ok(
                    &mut client,
                    &format!(
                        "docker cp {} {}",
                        shell_quote(&format!("{}:{}", target, abs)),
                        shell_quote(&format!("{}/", staging))
                    ),
                    FM_TRANSFER_TIMEOUT_SECS,
                    "从容器复制到中转目录失败",
                )
                .await?;
                let is_dir = probe_dir(&mut client, &staging, &name).await?;
                (remote_join(&staging, &name), name.clone(), is_dir)
            }
            SourceKind::Volume => {
                run_cmd_ok(
                    &mut client,
                    &format!("mkdir -p {}", shell_quote(&staging)),
                    EXEC_TIMEOUT_SECS,
                    "创建中转目录失败",
                )
                .await?;
                let image = crate::migrate_project::pick_tar_image(&mut client, &noop_emit()).await?;
                let script = format!("cp -r {} /out/", shell_quote(&abs));
                let cmd = volume_run_cmd(&image, &target, &script, Some(&staging));
                run_cmd_ok(
                    &mut client,
                    &cmd,
                    FM_TRANSFER_TIMEOUT_SECS,
                    "从卷复制到中转目录失败",
                )
                .await?;
                let is_dir = probe_dir(&mut client, &staging, &name).await?;
                (remote_join(&staging, &name), name.clone(), is_dir)
            }
            SourceKind::DeployDir => {
                let (code, _) = exec_collect(
                    &mut client,
                    &format!("test -d {}", shell_quote(&abs)),
                )
                .await?;
                let is_dir = code == 0;
                if is_dir {
                    run_cmd_ok(
                        &mut client,
                        &format!("mkdir -p {}", shell_quote(&staging)),
                        EXEC_TIMEOUT_SECS,
                        "创建中转目录失败",
                    )
                    .await?;
                }
                (abs.clone(), name.clone(), is_dir)
            }
        };

        // 2) 目录 → 服务器侧打包;文件 → 原样
        let (remote_file, local_name) = if is_dir {
            let parent = remote_file
                .rsplit_once('/')
                .map(|(p, _)| p)
                .unwrap_or("/tmp")
                .to_string();
            run_cmd_ok(
                &mut client,
                &tar_dir_cmd(&parent, &name),
                FM_TRANSFER_TIMEOUT_SECS,
                "打包目录失败",
            )
            .await?;
            (
                remote_join(&parent, &format!("{}.tar.gz", name)),
                format!("{}.tar.gz", name),
            )
        } else {
            (remote_file, local_name)
        };
        ensure_not_cancelled()?;

        // 3) SFTP 下载(带进度)
        let local_path = local_dir.join(&local_name);
        let progress: Arc<dyn Fn(u64, u64) + Send + Sync> = {
            let app = app.clone();
            let op_id = op_id.clone();
            Arc::new(move |done: u64, total: u64| emit_progress(&app, &op_id, "download", done, total))
        };
        client
            .sftp_download(&remote_file, &local_path, progress.as_ref())
            .await?;
        ensure_not_cancelled()?;
        Ok(local_path.to_string_lossy().to_string())
    }
    .await;

    cleanup.run(&mut client).await;
    result
}

// ===== 命令:上传 / 文本写入 =====

/// 上传本机文件到 `dest_dir`(源内相对目录);覆盖前默认备份到本机 `config/fm-backups/`。
// 参数即 Tauri 契约(前端一次给全),保持平铺签名、显式关闭 clippy 提示
// (与部署管线 `server_deploy` 同口径:不为消警重构签名。)
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn manage_files_upload(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    kind: String,
    target: String,
    dest_dir: String,
    local_path: String,
    backup: Option<bool>,
) -> Result<serde_json::Value, String> {
    let src = PathBuf::from(local_path.trim());
    if !src.is_file() {
        return Err(format!("本机文件不存在: {}", src.display()));
    }
    let name = src
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return Err("本机文件名为空".to_string());
    }
    let dest_dir = normalize_rel_path(&dest_dir)?;
    let dest_rel = if dest_dir.is_empty() {
        name.clone()
    } else {
        format!("{}/{}", dest_dir, name)
    };
    upload_local_file(
        &app,
        &server_id,
        password_plain.as_deref(),
        &kind,
        &target,
        &dest_rel,
        &src,
        backup.unwrap_or(true),
    )
    .await
}

/// 上传主体(上传与文本写入共用):`dest_rel` 为**含文件名的目标相对路径**。
#[allow(clippy::too_many_arguments)]
async fn upload_local_file(
    app: &AppHandle,
    server_id: &str,
    password_plain: Option<&str>,
    kind_str: &str,
    target: &str,
    dest_rel: &str,
    local_file: &Path,
    backup: bool,
) -> Result<serde_json::Value, String> {
    let kind = SourceKind::parse(kind_str)?;
    let target = validate_target(kind, target)?;
    let dest_rel = normalize_rel_path(dest_rel)?;
    if dest_rel.is_empty() {
        return Err("目标路径不合法".to_string());
    }
    let name = rel_file_name(&dest_rel);
    let op_id = uuid::Uuid::new_v4().to_string();
    reset_cancelled();

    let (_server, mut client) = connect_server(server_id, password_plain).await?;
    // 宿主机源(部署目录)写闸门:白名单 + 远端 canonicalize(第三十四批(五))
    if kind.is_host_source() {
        ensure_host_write_allowed(&mut client, &source_abs_path(kind, &target, &dest_rel)).await?;
    }
    let staging = staging_dir(&op_id);
    let cleanup = RemoteDirGuard(cleanup_cmd(&staging));

    let result: Result<serde_json::Value, String> = async {
        run_cmd_ok(
            &mut client,
            &format!("mkdir -p {}", shell_quote(&staging)),
            EXEC_TIMEOUT_SECS,
            "创建中转目录失败",
        )
        .await?;

        // 0) 覆盖前备份(尽力而为:失败不阻断上传,返回值里说明)
        let mut backup_path = String::new();
        if backup {
            if let Some(p) = try_backup_existing(
                &mut client, app, server_id, kind, &target, &dest_rel, &op_id,
            )
            .await
            .ok()
            .flatten()
            {
                backup_path = p;
            }
        }

        // 1) 本机 → 中转目录
        let progress: Arc<dyn Fn(u64, u64) + Send + Sync> = {
            let app = app.clone();
            let op_id = op_id.clone();
            Arc::new(move |done: u64, total: u64| emit_progress(&app, &op_id, "upload", done, total))
        };
        client
            .sftp_upload(local_file, &staging, &name, false, progress.as_ref())
            .await?;
        ensure_not_cancelled()?;

        // 2) 中转目录 → 目标(容器/卷)
        let staged = remote_join(&staging, &name);
        match kind {
            SourceKind::Container => {
                run_cmd_ok(
                    &mut client,
                    &format!(
                        "docker cp {} {}",
                        shell_quote(&staged),
                        shell_quote(&format!("{}:{}", target, source_abs_path(kind, &target, &dest_rel)))
                    ),
                    FM_TRANSFER_TIMEOUT_SECS,
                    "写入容器失败(目标目录须已存在)",
                )
                .await?;
            }
            SourceKind::Volume => {
                let image = crate::migrate_project::pick_tar_image(&mut client, &noop_emit()).await?;
                let script = format!(
                    "cp {} {}",
                    shell_quote(&format!("/out/{}", name)),
                    shell_quote(&source_abs_path(kind, &target, &dest_rel))
                );
                let cmd = volume_run_cmd(&image, &target, &script, Some(&staging));
                run_cmd_ok(
                    &mut client,
                    &cmd,
                    FM_TRANSFER_TIMEOUT_SECS,
                    "写入卷失败(目标目录须已存在)",
                )
                .await?;
            }
            SourceKind::DeployDir => {
                // 宿主机目标(第三十四批(五)):直接 cp(白名单闸门已在入口按
                // canonicalize 结果放行;目标目录须已存在 —— 与卷分支同口径)
                run_cmd_ok(
                    &mut client,
                    &format!(
                        "cp {} {}",
                        shell_quote(&staged),
                        shell_quote(&source_abs_path(kind, &target, &dest_rel))
                    ),
                    FM_TRANSFER_TIMEOUT_SECS,
                    "写入目标文件失败(目标目录须已存在)",
                )
                .await?;
            }
        }

        Ok(serde_json::json!({
            "opId": op_id,
            "backedUp": if backup_path.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(backup_path) },
            "targetPath": dest_rel,
        }))
    }
    .await;

    cleanup.run(&mut client).await;
    result
}

/// 覆盖前把远端已有文件备份到本机(不存在/失败 → `Ok(None)`,绝不阻断上传)。
async fn try_backup_existing(
    client: &mut SshClient,
    app: &AppHandle,
    server_id: &str,
    kind: SourceKind,
    target: &str,
    dest_rel: &str,
    op_id: &str,
) -> Result<Option<String>, String> {
    let name = rel_file_name(dest_rel);
    let abs = source_abs_path(kind, target, dest_rel);
    let staging = staging_dir(op_id);

    // ① 存在性检查(不依赖容器工具:卷用临时容器,容器用 docker cp 探测不可行 → 用 inspect 之外的方式)
    let exists = match kind {
        SourceKind::Container => {
            // 容器内 test -f 需要 shell;无 shell 时退化为「先试 cp,失败即视为新文件」
            let cmds = container_exec_cmds(target, &format!("test -f {}", shell_quote(&abs)));
            match run_shell_candidates(client, &cmds, "检查目标文件失败").await {
                Ok(_) => true,
                Err(e) if e.contains("容器内没有可用 shell") => {
                    // 无 shell:直接把目标当不存在处理(新文件覆盖场景下无需备份)
                    false
                }
                Err(_) => false, // 文件不存在(或不可读)→ 新文件
            }
        }
        SourceKind::Volume => {
            let image = crate::migrate_project::pick_tar_image(client, &noop_emit()).await?;
            let cmd = volume_run_cmd(
                &image,
                target,
                &format!("test -f {}", shell_quote(&abs)),
                None,
            );
            run_cmd_ok(client, &cmd, FM_LIST_TIMEOUT_SECS, "检查目标文件失败")
                .await
                .is_ok()
        }
        SourceKind::DeployDir => run_cmd_ok(
            client,
            &format!("test -f {}", shell_quote(&abs)),
            FM_LIST_TIMEOUT_SECS,
            "检查目标文件失败",
        )
        .await
        .is_ok(),
    };
    if !exists {
        return Ok(None);
    }

    // ② 取回到本机备份目录
    let staged_name = format!("bak-{}", name);
    let fetch_cmd = match kind {
        SourceKind::Container => format!(
            "docker cp {} {}",
            shell_quote(&format!("{}:{}", target, abs)),
            shell_quote(&remote_join(&staging, &staged_name))
        ),
        SourceKind::Volume => {
            let image = crate::migrate_project::pick_tar_image(client, &noop_emit()).await?;
            volume_run_cmd(
                &image,
                target,
                &format!("cp {} /out/{}", shell_quote(&abs), shell_quote(&staged_name)),
                Some(&staging),
            )
        }
        SourceKind::DeployDir => format!(
            "cp {} {}",
            shell_quote(&abs),
            shell_quote(&remote_join(&staging, &staged_name))
        ),
    };
    if run_cmd_ok(client, &fetch_cmd, FM_TRANSFER_TIMEOUT_SECS, "备份原文件失败")
        .await
        .is_err()
    {
        return Ok(None);
    }

    let dir = fm_backup_dir(&crate::config::config_dir(), server_id, kind, target).join(now_ts());
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建备份目录失败: {}", e))?;
    let local = dir.join(name);
    let progress: Arc<dyn Fn(u64, u64) + Send + Sync> = {
        let app = app.clone();
        let op_id = op_id.to_string();
        Arc::new(move |done: u64, total: u64| emit_progress(&app, &op_id, "backup", done, total))
    };
    if client
        .sftp_download(&remote_join(&staging, &staged_name), &local, progress.as_ref())
        .await
        .is_err()
    {
        return Ok(None);
    }
    // 同文件备份保留 N 份
    if let Some(parent) = local.parent().and_then(|p| p.parent()) {
        let names: Vec<String> = std::fs::read_dir(parent)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        for old in select_fm_backups_to_prune(&names, FM_BACKUP_KEEP) {
            let _ = std::fs::remove_dir_all(parent.join(old));
        }
    }
    Ok(Some(local.to_string_lossy().to_string()))
}

/// 文本查看(≤512KB 且 UTF-8 才返回内容;二进制/超大只回报标志)。
#[tauri::command]
pub async fn manage_files_read_text(
    server_id: String,
    password_plain: Option<String>,
    kind: String,
    target: String,
    path: String,
) -> Result<FileTextPayload, String> {
    let kind = SourceKind::parse(&kind)?;
    let target = validate_target(kind, &target)?;
    let rel = normalize_rel_path(&path)?;
    if rel.is_empty() {
        return Err("请选择具体文件".to_string());
    }
    let abs = source_abs_path(kind, &target, &rel);
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    let b64 = read_file_base64(&mut client, kind, &target, &abs).await?;
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = BASE64_STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| format!("文件内容解码失败: {}", e))?;
    let size = bytes.len() as u64;
    if size > FM_TEXT_MAX_BYTES {
        return Ok(FileTextPayload {
            text: String::new(),
            bytes: size,
            binary: false,
            truncated: true,
        });
    }
    let binary = bytes.contains(&0) || std::str::from_utf8(&bytes).is_err();
    Ok(FileTextPayload {
        text: if binary {
            String::new()
        } else {
            String::from_utf8_lossy(&bytes).to_string()
        },
        bytes: size,
        binary,
        truncated: false,
    })
}

/// 文本内容(前端 camelCase)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTextPayload {
    pub text: String,
    pub bytes: u64,
    pub binary: bool,
    pub truncated: bool,
}

/// 取文件字节:统一走 `base64`(避免二进制经文本通道变形)。
async fn read_file_base64(
    client: &mut SshClient,
    kind: SourceKind,
    target: &str,
    abs: &str,
) -> Result<String, String> {
    match kind {
        SourceKind::Container => {
            let cmds = container_exec_cmds(target, &format!("cat {} | base64", shell_quote(abs)));
            run_shell_candidates(client, &cmds, "读取容器文件失败").await
        }
        SourceKind::Volume => {
            let image = crate::migrate_project::pick_tar_image(client, &noop_emit()).await?;
            let cmd = volume_run_cmd(
                &image,
                target,
                &format!("cat {} | base64", shell_quote(abs)),
                None,
            );
            run_cmd_ok(client, &cmd, FM_LIST_TIMEOUT_SECS, "读取卷文件失败").await
        }
        SourceKind::DeployDir => {
            run_cmd_ok(
                client,
                &format!("base64 {}", shell_quote(abs)),
                FM_LIST_TIMEOUT_SECS,
                "读取文件失败",
            )
            .await
        }
    }
}

/// 写入文本(≤512KB;经本机临时文件复用上传通道,`backup` 默认 true)。
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn manage_files_write_text(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    kind: String,
    target: String,
    path: String,
    content_b64: String,
    backup: Option<bool>,
) -> Result<serde_json::Value, String> {
    let kind = SourceKind::parse(&kind)?;
    let target = validate_target(kind, &target)?;
    let rel = normalize_rel_path(&path)?;
    if rel.is_empty() {
        return Err("请选择具体文件".to_string());
    }
    let cleaned: String = content_b64.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = BASE64_STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| format!("内容编码不合法: {}", e))?;
    if bytes.len() as u64 > FM_TEXT_MAX_BYTES {
        return Err(format!(
            "文本超过 {} KB 上限:请改用 下载 → 本地编辑 → 上传",
            FM_TEXT_MAX_BYTES / 1024
        ));
    }
    // 落到本机临时目录(**文件名与目标一致**,上传通道按原名写回)
    let tmp_dir = std::env::temp_dir().join(format!("dd-fm-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("创建临时目录失败: {}", e))?;
    let tmp_file = tmp_dir.join(rel_file_name(&rel));
    std::fs::write(&tmp_file, &bytes).map_err(|e| format!("写入临时文件失败: {}", e))?;
    let guard = crate::commands::TempFileGuard::new_pub(tmp_file.clone());

    let res = upload_local_file(
        &app,
        &server_id,
        password_plain.as_deref(),
        kind.as_str(),
        &target,
        &rel,
        &tmp_file,
        backup.unwrap_or(true),
    )
    .await;
    drop(guard);
    let _ = std::fs::remove_dir_all(&tmp_dir);
    res
}

// ===== 命令:新建目录 / 改名 / 删除 / 取消 =====

/// 单条文件操作(`mkdir` / `rename` / `delete`;部署目录经白名单闸门)。
#[tauri::command]
pub async fn manage_files_fs_op(
    server_id: String,
    password_plain: Option<String>,
    kind: String,
    target: String,
    path: String,
    op: String,
    new_name: Option<String>,
) -> Result<(), String> {
    let kind = SourceKind::parse(&kind)?;
    let target = validate_target(kind, &target)?;
    let rel = normalize_rel_path(&path)?;
    if rel.is_empty() {
        return Err("请选择具体条目".to_string());
    }
    let abs = source_abs_path(kind, &target, &rel);
    let script = fs_op_script(&op, &abs, new_name.as_deref())?;
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;
    match kind {
        SourceKind::Container => {
            let cmds = container_exec_cmds(&target, &script);
            run_shell_candidates(&mut client, &cmds, "文件操作失败").await?;
        }
        SourceKind::Volume => {
            let image = crate::migrate_project::pick_tar_image(&mut client, &noop_emit()).await?;
            let cmd = volume_run_cmd(&image, &target, &script, None);
            run_cmd_ok(&mut client, &cmd, FM_LIST_TIMEOUT_SECS, "文件操作失败").await?;
        }
        SourceKind::DeployDir => {
            // 白名单闸门(第三十四批(五)):目标路径 canonicalize 后须在白名单内
            ensure_host_write_allowed(&mut client, &abs).await?;
            run_cmd_ok(&mut client, &script, FM_LIST_TIMEOUT_SECS, "文件操作失败").await?;
        }
    }
    Ok(())
}

/// 取消进行中的传输(块级生效;无进行中操作时为空操作)。
#[tauri::command]
pub async fn manage_files_cancel() -> Result<(), String> {
    FM_CANCELLED.store(true, Ordering::SeqCst);
    Ok(())
}

// ===== 命令:C 容器快照 =====

/// 容器快照(前端 camelCase)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSnapshot {
    pub container_id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub restart_count: i64,
    pub created: String,
    /// 实际环境变量(`docker inspect .Config.Env`)
    pub env: Vec<(String, String)>,
    /// 端口映射(`docker port` 输出行)
    pub ports: Vec<String>,
    /// 进程表(`docker top` 原文行,含表头;上限 64 行)
    pub processes: Vec<String>,
    /// 挂载(`TYPE SOURCE -> DESTINATION` 行)
    pub mounts: Vec<String>,
    /// compose 声明但容器里没有的环境变量名(需 `project_dir`)
    pub missing_env_keys: Vec<String>,
    /// 容器里有但 compose 未声明的环境变量名
    pub extra_env_keys: Vec<String>,
    /// 对比口径 / 降级说明
    pub note: String,
}

/// 容器快照:实际环境变量 / 端口 / 进程 / 挂载;给出 `project_dir` 时与 compose 副本
/// 的 `environment:` **键名**做双向对比(只比键名,口径见 `note`)。
#[tauri::command]
pub async fn manage_container_snapshot(
    server_id: String,
    password_plain: Option<String>,
    container_id: String,
    project_dir: Option<String>,
) -> Result<ContainerSnapshot, String> {
    let cid = validate_target(SourceKind::Container, &container_id)?;
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref()).await?;

    let fmt = "{{.Name}}|{{.Config.Image}}|{{.State.Status}}|{{.RestartCount}}|{{.Created}}";
    let out = run_cmd_ok(
        &mut client,
        &format!(
            "docker inspect --format {} {}",
            shell_quote(fmt),
            shell_quote(&cid)
        ),
        EXEC_TIMEOUT_SECS,
        "读取容器信息失败",
    )
    .await?;
    let line = out.lines().next().unwrap_or("").trim();
    let parts: Vec<&str> = line.split('|').collect();
    let name = parts
        .first()
        .map(|s| s.trim_start_matches('/').to_string())
        .unwrap_or_default();
    let image = parts.get(1).unwrap_or(&"").to_string();
    let status = parts.get(2).unwrap_or(&"").to_string();
    let restart_count = parts.get(3).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let created = parts.get(4).unwrap_or(&"").to_string();

    // 环境变量(JSON 数组)
    let env_out = exec_collect(
        &mut client,
        &format!(
            "docker inspect --format {} {}",
            shell_quote("{{json .Config.Env}}"),
            shell_quote(&cid)
        ),
    )
    .await
    .map(|(_, o)| o)
    .unwrap_or_default();
    let env: Vec<(String, String)> =
        serde_json::from_str::<Vec<String>>(env_out.lines().next().unwrap_or("[]").trim())
            .unwrap_or_default()
            .into_iter()
            .map(|kv| match kv.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (kv, String::new()),
            })
            .collect();

    let (_, port_out) = exec_collect(&mut client, &format!("docker port {}", shell_quote(&cid)))
        .await
        .unwrap_or((1, String::new()));
    let ports: Vec<String> = port_out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();

    let (_, mount_out) = exec_collect(
        &mut client,
        &format!(
            "docker inspect --format {} {}",
            shell_quote("{{range .Mounts}}{{.Type}} {{.Source}} -> {{.Destination}}{{\"\\n\"}}{{end}}"),
            shell_quote(&cid)
        ),
    )
    .await
    .unwrap_or((1, String::new()));
    let mounts: Vec<String> = mount_out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();

    let (code, top_out) = exec_collect(&mut client, &format!("docker top {}", shell_quote(&cid)))
        .await
        .unwrap_or((1, String::new()));
    let processes: Vec<String> = if code == 0 {
        top_out
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.trim().is_empty())
            .take(64)
            .map(String::from)
            .collect()
    } else {
        Vec::new()
    };

    let mut missing_env_keys: Vec<String> = Vec::new();
    let mut extra_env_keys: Vec<String> = Vec::new();
    let mut note =
        "环境变量为容器实际值(docker inspect);未提供项目目录,不做 compose 声明对比".to_string();
    if let Some(dir) = project_dir
        .map(|d| d.trim().trim_end_matches('/').to_string())
        .filter(|d| d.starts_with('/') && !d.contains(".."))
    {
        match declared_env_keys(&mut client, &cid, &dir).await {
            Ok((declared, why)) => {
                let actual: std::collections::HashSet<&str> =
                    env.iter().map(|(k, _)| k.as_str()).collect();
                let declared_set: std::collections::HashSet<&str> =
                    declared.iter().map(|s| s.as_str()).collect();
                missing_env_keys = declared
                    .iter()
                    .filter(|k| !actual.contains(k.as_str()))
                    .cloned()
                    .collect();
                extra_env_keys = env
                    .iter()
                    .map(|(k, _)| k.clone())
                    .filter(|k| !declared_set.contains(k.as_str()))
                    .collect();
                note = why;
            }
            Err(e) => note = format!("未能读取 compose 声明({}),仅显示实际值", e),
        }
    }

    Ok(ContainerSnapshot {
        container_id: cid,
        name,
        image,
        status,
        restart_count,
        created,
        env,
        ports,
        processes,
        mounts,
        missing_env_keys,
        extra_env_keys,
        note,
    })
}

/// 从部署目录的 compose 副本取「该容器对应服务」声明的环境变量**键名**。
async fn declared_env_keys(
    client: &mut SshClient,
    cid: &str,
    dir: &str,
) -> Result<(Vec<String>, String), String> {
    let out = run_cmd_ok(
        client,
        &format!(
            "docker inspect --format {} {}",
            shell_quote("{{index .Config.Labels \"com.docker.compose.service\"}}"),
            shell_quote(cid)
        ),
        EXEC_TIMEOUT_SECS,
        "读取容器 compose 服务标签失败",
    )
    .await?;
    let service = out.trim().to_string();
    if service.is_empty() {
        return Err("该容器不带 compose 服务标签".to_string());
    }
    let compose_path = remote_join(dir, "docker-compose.yml");
    let text = run_cmd_ok(
        client,
        &format!("cat {}", shell_quote(&compose_path)),
        EXEC_TIMEOUT_SECS,
        "读取 compose 副本失败",
    )
    .await?;
    let keys = crate::stack::declared_env_keys_of_service(&text, &service)?;
    Ok((
        keys,
        format!(
            "声明值来自 compose 副本 {} 的服务「{}」;仅比较键名(插值 / env_file 注入的差异不计)",
            compose_path, service
        ),
    ))
}

// ===== 纯函数单测 =====

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_rel_path() {
        assert_eq!(normalize_rel_path("").unwrap(), "");
        assert_eq!(normalize_rel_path("/etc/nginx/").unwrap(), "etc/nginx");
        assert_eq!(normalize_rel_path(" a/b ").unwrap(), "a/b");
        assert!(normalize_rel_path("..").is_err());
        assert!(normalize_rel_path("a/../b").is_err());
        assert!(normalize_rel_path("a//b").is_err());
    }

    #[test]
    fn test_validate_target() {
        assert!(validate_target(SourceKind::Container, "ddfm-test").is_ok());
        assert!(validate_target(
            SourceKind::Container,
            "5c1b2f0a9e7d3c4b8a6f1e2d3c4b5a6978877665544332211ffeeddccbbaa99"
        )
        .is_ok());
        assert!(validate_target(SourceKind::Container, "bad name").is_err());
        assert!(validate_target(SourceKind::Container, "-x").is_err());
        assert!(validate_target(SourceKind::Volume, "demo_pgdata").is_ok());
        assert!(validate_target(SourceKind::Volume, "a..b").is_err());
        assert!(validate_target(SourceKind::DeployDir, "/opt/app").is_ok());
        assert!(validate_target(SourceKind::DeployDir, "opt/app").is_err());
        assert!(validate_target(SourceKind::DeployDir, "/opt/../etc").is_err());
    }

    #[test]
    fn test_source_abs_path() {
        assert_eq!(source_abs_path(SourceKind::Container, "cid", ""), "/");
        assert_eq!(source_abs_path(SourceKind::Container, "cid", "etc/x"), "/etc/x");
        assert_eq!(source_abs_path(SourceKind::Volume, "vol", ""), "/v");
        assert_eq!(source_abs_path(SourceKind::Volume, "vol", "a/b"), "/v/a/b");
        assert_eq!(source_abs_path(SourceKind::DeployDir, "/opt/app/", ""), "/opt/app");
        assert_eq!(source_abs_path(SourceKind::DeployDir, "/opt/app/", "releases"), "/opt/app/releases");
    }

    #[test]
    fn test_rel_file_name() {
        assert_eq!(rel_file_name("etc/nginx/nginx.conf"), "nginx.conf");
        assert_eq!(rel_file_name("a/b/"), "b");
        assert_eq!(rel_file_name(""), "root");
    }

    #[test]
    fn test_parse_ls_long_gnu_and_busybox_shapes() {
        // GNU / BusyBox 同形;含目录、普通文件、符号链接、带空格文件名、total 行
        let out = "total 20\n\
drwxr-xr-x    2 root     root          4096 Sep 22 16:52 conf.d\n\
-rw-r--r--    1 root     root           123 Sep 22 16:52 nginx.conf\n\
-rw-r--r--    1 root     root             7 Sep 22  2025 old file.txt\n\
lrwxrwxrwx    1 root     root            11 Sep 22 16:52 access.log -> /dev/stdout\n\
warning: something\n";
        let entries = parse_ls_long(out);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].name, "conf.d");
        assert!(entries[0].is_dir && !entries[0].is_link);
        assert_eq!(entries[0].size, 4096);
        assert_eq!(entries[1].name, "nginx.conf");
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].mode, "-rw-r--r--");
        assert_eq!(entries[2].name, "old file.txt", "带空格文件名不得被截断");
        assert_eq!(entries[2].mtime, "Sep 22 2025");
        assert_eq!(entries[3].name, "access.log", "符号链接只保留名字");
        assert!(entries[3].is_link);
    }

    #[test]
    fn test_fs_op_script() {
        assert_eq!(
            fs_op_script("mkdir", "/v/a/b", None).unwrap(),
            "mkdir -p '/v/a/b'"
        );
        assert_eq!(
            fs_op_script("rename", "/v/a/b.txt", Some("c.txt")).unwrap(),
            "mv '/v/a/b.txt' '/v/a/c.txt'"
        );
        assert_eq!(
            fs_op_script("rename", "/top", Some("new")).unwrap(),
            "mv '/top' '/new'"
        );
        assert!(fs_op_script("rename", "/v/a", Some("../x")).is_err());
        assert!(fs_op_script("rename", "/v/a", Some("a/b")).is_err());
        assert_eq!(fs_op_script("delete", "/v/a", None).unwrap(), "rm -rf '/v/a'");
        assert!(fs_op_script("chmod", "/v/a", None).is_err());
    }

    #[test]
    fn test_backup_prune_and_dir() {
        let names: Vec<String> = ["20260920-101010", "20260922-090000", "20260921-120000"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            select_fm_backups_to_prune(&names, 2),
            vec!["20260920-101010".to_string()]
        );
        assert!(select_fm_backups_to_prune(&names, 5).is_empty());
        assert_eq!(select_fm_backups_to_prune(&names, 0).len(), 3);
        let dir = fm_backup_dir(
            Path::new("/cfg"),
            "s1",
            SourceKind::Container,
            "ddfm-test",
        );
        assert!(dir.to_string_lossy().contains("fm-backups"));
        assert!(dir.to_string_lossy().ends_with("container-ddfm-test"));
    }

    /// 真机测试(需本机 Docker;`cargo test manage_files -- --ignored`):
    /// 容器「列举 → 解析」与「cp 出 → cp 回」全链,验证 `docker cp` 通道与
    /// `LC_ALL=C ls -la` 解析在真实输出上成立。
    /// 镜像取 `DD_FM_TEST_IMAGE`(缺省 `busybox:latest`),本地不存在则跳过
    /// (不在测试里联网拉镜像 —— 环境各异)。
    #[test]
    #[ignore]
    fn test_real_docker_list_and_cp_roundtrip() {
        use std::process::Command;
        let run = |args: &[&str]| -> (i32, String) {
            match Command::new("docker").args(args).output() {
                Ok(o) => (
                    o.status.code().unwrap_or(-1),
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    ),
                ),
                Err(_) => (-99, "docker 不可用".to_string()),
            }
        };
        if run(&["version", "--format", "{{.Server.Version}}"]).0 != 0 {
            eprintln!("跳过:本机无可用 Docker");
            return;
        }
        let image = std::env::var("DD_FM_TEST_IMAGE").unwrap_or_else(|_| "busybox:latest".to_string());
        if run(&["image", "inspect", &image]).0 != 0 {
            eprintln!("跳过:本地没有镜像 {},请先 pull 或 tag 后再跑", image);
            return;
        }
        let name = format!("dd-fm-test-{}", std::process::id());
        let staging = std::env::temp_dir().join(format!("dd-fm-test-{}", std::process::id()));
        let _ = run(&["rm", "-f", &name]);
        let _ = std::fs::create_dir_all(&staging);
        let (c, out) = run(&["run", "-d", "--name", &name, &image, "sleep", "300"]);
        assert_eq!(c, 0, "docker run 失败: {}", out);
        let (c, out) = run(&[
            "exec", &name, "sh", "-c",
            "mkdir -p /srv/app && printf hello > /srv/app/app.conf",
        ]);
        assert_eq!(c, 0, "写入测试文件失败: {}", out);

        // ① 列举 → 解析
        let (c, out) = run(&["exec", &name, "sh", "-c", "LC_ALL=C ls -la /srv/app"]);
        assert_eq!(c, 0, "列举失败: {}", out);
        let entries = parse_ls_long(&out);
        let conf = entries.iter().find(|e| e.name == "app.conf").expect("应解析出 app.conf");
        assert!(!conf.is_dir);
        assert_eq!(conf.size, 5);

        // ② cp 出 → 内容一致
        let (c, out) = run(&[
            "cp",
            &format!("{}:/srv/app/app.conf", name),
            &format!("{}/", staging.display()),
        ]);
        assert_eq!(c, 0, "cp 出失败: {}", out);
        let local = staging.join("app.conf");
        assert_eq!(std::fs::read_to_string(&local).unwrap(), "hello");

        // ③ cp 回(覆盖式)
        std::fs::write(&local, "world").unwrap();
        let (c, out) = run(&[
            "cp",
            &local.display().to_string(),
            &format!("{}:/srv/app/app.conf", name),
        ]);
        assert_eq!(c, 0, "cp 回失败: {}", out);
        let (c, out) = run(&["exec", &name, "cat", "/srv/app/app.conf"]);
        assert_eq!(c, 0, "读回失败: {}", out);
        assert_eq!(out.trim(), "world");

        // ④ 目录:cp 出后宿主侧 test -d 判目录 → 打包命令形态可用
        let (c, out) = run(&["cp", &format!("{}:/srv", name), &format!("{}/", staging.display())]);
        assert_eq!(c, 0, "目录 cp 失败: {}", out);
        assert!(staging.join("srv").is_dir());
        // 打包在**服务器侧**执行(应用里走 SSH);测试用本机 tar 验证同一形态
        let tar_out = Command::new("tar")
            .args([
                "czf",
                &staging.join("srv.tar.gz").display().to_string(),
                "-C",
                &staging.display().to_string(),
                "srv",
            ])
            .output();
        match tar_out {
            Ok(o) if o.status.success() => {}
            Ok(o) => panic!("打包失败: {}", String::from_utf8_lossy(&o.stderr)),
            Err(e) => panic!("本机 tar 不可用: {}", e),
        }
        assert!(staging.join("srv.tar.gz").is_file());

        // 清理
        let _ = run(&["rm", "-f", &name]);
        let _ = std::fs::remove_dir_all(&staging);
    }

    #[test]
    fn test_cmd_builders() {
        assert_eq!(
            container_exec_cmds("c1", "ls -la /x")[0],
            "docker exec 'c1' sh -c 'ls -la /x'"
        );
        assert_eq!(
            volume_run_cmd("busybox:latest", "vol", "ls /v", Some("/tmp/dd-fm-1")),
            "docker run --rm -v 'vol:/v' -v '/tmp/dd-fm-1:/out' 'busybox:latest' sh -c 'ls /v'"
        );
        assert_eq!(staging_dir("abc"), "/tmp/dd-fm-abc");
        assert_eq!(tar_dir_cmd("/tmp/dd-fm-1", "conf.d"), "tar czf '/tmp/dd-fm-1/conf.d.tar.gz' -C '/tmp/dd-fm-1' 'conf.d'");
        assert!(looks_like_missing_shell("exec: \"sh\": executable file not found in $PATH"));
        assert!(!looks_like_missing_shell("cat: /x: No such file or directory"));
    }
}
