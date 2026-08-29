//! SSH/SFTP 模块(任务 3)。
//!
//! 基于 `russh` 0.46 + `russh-sftp` 2.x 提供:
//! - [`SshClient::connect`]:密钥(PEM,未加密)/ 密码两种认证
//! - [`SshClient::exec`]:开通道执行命令,stdout+stderr 合并按行实时回调,返回退出码
//! - [`SshClient::sftp_upload`]:单文件上传(可选断点续传),带字节进度回调
//! - [`SshClient::sftp_upload_dir`]:整目录上传,带字节进度回调
//! - [`SshClient::sftp_stat_size`]:查询远端文件大小(断点续传的决策依据)
//! - [`check_server_env`]:远端 docker / compose / gzip / 目录 / 磁盘环境探测
//!
//! 安全取舍(有意为之):本工具面向个人部署场景,首次连接直接接受服务器主机密钥
//! (不校验 known_hosts),避免交互式确认;如需严格校验,可在
//! [`ClientHandler::check_server_key`] 中接入指纹比对。

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::ChannelMsg;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{OpenFlags, StatusCode};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::config::{AuthType, ServerConfig};

/// 远端一键安装 Docker 的官方脚本命令。
pub const INSTALL_DOCKER_CMD: &str = "curl -fsSL https://get.docker.com | sh";

/// SFTP 单次读写块大小:64KB。
const CHUNK_SIZE: usize = 64 * 1024;

/// 主机密钥处理器:无条件接受(见模块注释的安全取舍)。
struct ClientHandler;

#[async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // 首次连接直接接受主机密钥 —— 工具类应用的有意取舍(见模块注释)。
        Ok(true)
    }
}

/// 一条已完成认证的 SSH 连接(exec / SFTP 每次各自开通道,可复用连接)。
pub struct SshClient {
    handle: Handle<ClientHandler>,
}

impl SshClient {
    /// 建立连接并完成认证。
    ///
    /// - `AuthType::Key`:读取 `cfg.auth.key_path` 指向的 PEM 私钥文件(支持未加密密钥;
    ///   加密私钥的口令输入属后续任务扩展点,此处以无口令方式加载)。
    /// - `AuthType::Password`:使用 `password_plain`;若为 `None` 返回 `Err("需要密码")`。
    ///   (DPAPI 解密在配置/命令层完成,不在本模块内。)
    pub async fn connect(cfg: &ServerConfig, password_plain: Option<&str>) -> Result<Self, String> {
        let config = Arc::new(client::Config::default());
        let mut handle = client::connect(config, (cfg.host.as_str(), cfg.port), ClientHandler)
            .await
            .map_err(|e| format!("SSH 连接失败 ({}:{}): {}", cfg.host, cfg.port, e))?;

        match cfg.auth.auth_type {
            AuthType::Key => {
                let key_path = cfg.auth.key_path.as_deref().ok_or_else(|| {
                    "SSH 密钥认证失败: 未配置私钥路径(key_path 为空)".to_string()
                })?;
                let key = russh::keys::load_secret_key(key_path, None)
                    .map_err(|e| format!("加载私钥失败 ({}): {}", key_path, e))?;
                let ok = handle
                    .authenticate_publickey(&cfg.username, Arc::new(key))
                    .await
                    .map_err(|e| format!("SSH 公钥认证失败 (用户 {}): {}", cfg.username, e))?;
                if !ok {
                    return Err(format!(
                        "SSH 公钥认证失败: 服务器拒绝了密钥 (用户 {}, 私钥 {})",
                        cfg.username, key_path
                    ));
                }
            }
            AuthType::Password => {
                let password = password_plain.ok_or_else(|| "需要密码".to_string())?;
                let ok = handle
                    .authenticate_password(&cfg.username, password)
                    .await
                    .map_err(|e| format!("SSH 密码认证失败 (用户 {}): {}", cfg.username, e))?;
                if !ok {
                    return Err(format!(
                        "SSH 密码认证失败: 密码错误或被拒绝 (用户 {})",
                        cfg.username
                    ));
                }
            }
        }

        Ok(SshClient { handle })
    }

    /// 在远端执行 `cmd`。
    ///
    /// stdout 与 stderr 合并、按行实时回调 `on_output`(完整行带换行符;
    /// 最后不完整的行在通道关闭时输出),返回退出码(未收到 ExitStatus 时为 -1)。
    pub async fn exec(
        &mut self,
        cmd: &str,
        on_output: &mut impl FnMut(&str),
    ) -> Result<i32, String> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| format!("SSH 打开会话通道失败: {}", e))?;
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| format!("SSH 执行命令失败 ({}): {}", cmd, e))?;

        let mut exit_code: i32 = -1;
        // 字节级缓冲,避免多字节字符被 TCP 分块截断时输出乱码
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { ref data })
                | Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                    buf.extend_from_slice(data);
                    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=pos).collect();
                        on_output(&String::from_utf8_lossy(&line));
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = exit_status as i32;
                }
                Some(ChannelMsg::Eof) => {
                    // 服务端输出结束,继续等待 Close
                }
                Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
        // 不完整的最后一行也要输出
        if !buf.is_empty() {
            on_output(&String::from_utf8_lossy(&buf));
        }
        Ok(exit_code)
    }

    /// 上传单个文件到 `remote_dir/remote_name`。
    ///
    /// 回调 `(已传字节, 总字节)`;远端目录须已存在
    /// (调用方可用 [`mkdir_p_cmd`] + [`SshClient::exec`] 先建目录)。
    ///
    /// `resume = true` 时启用断点续传:先查询远端同名文件大小 ——
    /// 远端更小 → 以 CREATE|WRITE(不截断)打开并 seek 到远端大小处续写,
    /// 进度口径为 `(远端已有 + 本次已传, 本地总长)`;
    /// 远端不小于本地 → 视为已完成(回调 `(total, total)` 后直接成功);
    /// 远端不存在 → 全新上传。`resume = false` → 恒为全新上传(打开即截断)。
    ///
    /// 续传假设远端已有部分与本地文件的前缀一致(本工具仅对"同名即同内容"的
    /// 镜像包启用续传;内容可变的小文件应传 `false`)。
    pub async fn sftp_upload(
        &mut self,
        local: &Path,
        remote_dir: &str,
        remote_name: &str,
        resume: bool,
        on_progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<(), String> {
        let total = tokio::fs::metadata(local)
            .await
            .map_err(|e| format!("读取本地文件元数据失败 ({}): {}", local.display(), e))?
            .len();
        let remote_path = join_remote(remote_dir, remote_name);

        let sftp = self.open_sftp().await?;
        // 仅在启用续传时查询远端大小(全新上传无需一次额外往返)
        let remote_size = if resume {
            stat_remote_size(&sftp, &remote_path).await?
        } else {
            None
        };

        let mut sent: u64 = 0;
        match resume_plan(remote_size, total, resume) {
            ResumePlan::AlreadyDone => {
                // 远端已有同名文件且不小于本地:视为传输已完成
                on_progress(total, total);
                Ok(())
            }
            ResumePlan::Resume(offset) => {
                // 进度基数 = 远端已有字节
                sent = offset;
                on_progress(sent, total);
                copy_file_to_remote(&sftp, local, &remote_path, offset, &mut sent, total, on_progress)
                    .await
            }
            ResumePlan::Fresh => {
                on_progress(0, total);
                copy_file_to_remote(&sftp, local, &remote_path, 0, &mut sent, total, on_progress).await
            }
        }
    }

    /// 递归上传 `local_dir` 下全部内容(文件 + 子目录)到 `remote_dir`,
    /// 远端先 `mkdir -p`(顶层与各子目录,均幂等)。
    ///
    /// 进度回调为整个目录累计的 `(已传字节, 总字节)`。
    pub async fn sftp_upload_dir(
        &mut self,
        local_dir: &Path,
        remote_dir: &str,
        on_progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<(), String> {
        // 1. 预扫描本地目录:收集文件(local 路径、远端路径、大小)与全部子目录
        let top_remote = normalize_remote(remote_dir);
        let mut files: Vec<(PathBuf, String, u64)> = Vec::new();
        let mut subdirs: Vec<String> = Vec::new();
        walk_local_files(local_dir, &top_remote, &mut files, &mut subdirs)
            .map_err(|e| format!("遍历本地目录失败 ({}): {}", local_dir.display(), e))?;
        let total: u64 = files.iter().map(|f| f.2).sum();

        // 2. 远端建目录:顶层 mkdir -p;子目录合并成一条 mkdir -p
        //    (mkdir -p 会连带创建中间父目录,且重复执行无害)
        let mkdir_top = mkdir_p_cmd(&top_remote);
        let code = self.exec(&mkdir_top, &mut |_| {}).await?;
        if code != 0 {
            return Err(format!(
                "远端创建目录失败 ({}),退出码 {}",
                top_remote, code
            ));
        }
        if !subdirs.is_empty() {
            let mut cmd = String::from("mkdir -p");
            for d in &subdirs {
                cmd.push(' ');
                cmd.push_str(&shell_single_quote(d));
            }
            let code = self.exec(&cmd, &mut |_| {}).await?;
            if code != 0 {
                return Err(format!(
                    "远端创建子目录失败 ({} 个目录),退出码 {}",
                    subdirs.len(),
                    code
                ));
            }
        }

        // 3. 逐个文件上传,累计进度(目录映射为内容可变文件,不做续传)
        let sftp = self.open_sftp().await?;
        let mut sent: u64 = 0;
        on_progress(0, total);
        for (local_path, remote_path, _len) in &files {
            copy_file_to_remote(
                &sftp,
                local_path,
                remote_path,
                0,
                &mut sent,
                total,
                on_progress,
            )
            .await?;
        }
        Ok(())
    }

    /// 查询远端文件大小(字节)。
    ///
    /// 文件不存在 → `Ok(None)`;其余 SFTP 错误(权限、传输层失败等)
    /// 以中文 `Err` 传播 —— 调用方无法区分"查不到"与"查失败"以外的场景时,
    /// 不应把传输层故障误判为"文件不存在"。
    pub async fn sftp_stat_size(&mut self, remote_path: &str) -> Result<Option<u64>, String> {
        let sftp = self.open_sftp().await?;
        stat_remote_size(&sftp, remote_path).await
    }

    /// 打开一个 SFTP 会话(独立通道 + sftp 子系统)。
    async fn open_sftp(&self) -> Result<SftpSession, String> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| format!("SSH 打开 SFTP 通道失败: {}", e))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| format!("SSH 请求 sftp 子系统失败: {}", e))?;
        SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| format!("SFTP 会话初始化失败: {}", e))
    }
}

/// 执行命令并收集全部输出,返回 `(退出码, 合并后的 stdout+stderr)`。
/// 传输层失败(无法开通道等)直接以 `Err` 传播。
pub(crate) async fn exec_collect(client: &mut SshClient, cmd: &str) -> Result<(i32, String), String> {
    let mut out = String::new();
    let code = client.exec(cmd, &mut |line| out.push_str(line)).await?;
    Ok((code, out))
}

/// 远端环境检查报告。
#[derive(Debug, Default, Serialize)]
pub struct ServerCheckReport {
    pub docker: bool,
    pub compose: bool,
    pub gzip: bool,
    pub remote_dir_exists: bool,
    pub disk_free_gb: f64,
    pub errors: Vec<String>,
}

/// 检查远端环境:docker / docker compose / gzip 是否可用、远端目录是否存在、
/// 磁盘剩余空间(GB)。单项失败不影响其余检查,细节记录在 `errors` 里。
pub async fn check_server_env(
    client: &mut SshClient,
    remote_dir: &str,
) -> Result<ServerCheckReport, String> {
    let mut report = ServerCheckReport::default();

    // docker 是否可用:docker --version 退出码
    let (code, out) = exec_collect(client, "docker --version").await?;
    if code == 0 {
        report.docker = true;
    } else {
        report
            .errors
            .push(format!("docker --version 退出码 {}(未安装?)输出: {}", code, out.trim()));
    }

    // compose 插件是否可用:docker compose version 退出码
    let (code, out) = exec_collect(client, "docker compose version").await?;
    if code == 0 {
        report.compose = true;
    } else {
        report.errors.push(format!(
            "docker compose version 退出码 {}(compose 插件未安装?)输出: {}",
            code,
            out.trim()
        ));
    }

    // gzip 是否可用:gzip --version 退出码
    let (code, out) = exec_collect(client, "gzip --version").await?;
    if code == 0 {
        report.gzip = true;
    } else {
        report
            .errors
            .push(format!("gzip --version 退出码 {}(未安装?)输出: {}", code, out.trim()));
    }

    // 远端目录存在性:test -d '<dir>' && echo ok
    let dir_cmd = format!("test -d {} && echo ok", shell_single_quote(remote_dir));
    let (code, out) = exec_collect(client, &dir_cmd).await?;
    if code == 0 {
        report.remote_dir_exists = out.contains("ok");
    } else {
        report.remote_dir_exists = false;
    }

    // 磁盘可用空间:df -PBG <dir> | tail -1 | awk '{print $4}'(去掉 G 解析为 f64)
    let df_cmd = format!(
        "df -PBG {} | tail -1 | awk '{{print $4}}'",
        shell_single_quote(remote_dir)
    );
    let (code, out) = exec_collect(client, &df_cmd).await?;
    if code == 0 {
        let raw = out.trim().trim_end_matches('G').trim();
        match raw.parse::<f64>() {
            Ok(v) => report.disk_free_gb = v,
            Err(_) => {
                report.disk_free_gb = 0.0;
                report.errors.push(format!(
                    "磁盘可用空间解析失败(df 输出: {:?})",
                    out.trim()
                ));
            }
        }
    } else {
        report.disk_free_gb = 0.0;
        report.errors.push(format!(
            "df 查询磁盘空间退出码 {}(目录不存在?)输出: {}",
            code,
            out.trim()
        ));
    }

    Ok(report)
}

/// 单引号 shell 包裹;内部单引号按 `'\''` 转义。
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 构造 `mkdir -p '<path>'` 命令(单引号包裹;路径内单引号用 `'\''` 转义)。
pub fn mkdir_p_cmd(path: &str) -> String {
    format!("mkdir -p {}", shell_single_quote(path))
}

/// 拼接远端路径:去掉 `dir` 尾部 '/' 后接 '/' + `name`。
fn join_remote(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

/// 规范化远端目录:去掉尾部 '/',根目录 "/" 保持不变。
fn normalize_remote(dir: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 递归遍历本地目录,收集普通文件 `(本地路径, 远端路径, 大小)` 与全部子目录
/// (远端路径)。符号链接等非普通文件跳过。
fn walk_local_files(
    dir: &Path,
    remote_prefix: &str,
    files: &mut Vec<(PathBuf, String, u64)>,
    subdirs: &mut Vec<String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let remote_sub = join_remote(remote_prefix, &name);
        if ft.is_dir() {
            subdirs.push(remote_sub.clone());
            walk_local_files(&entry.path(), &remote_sub, files, subdirs)?;
        } else if ft.is_file() {
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files.push((entry.path(), remote_sub, len));
        }
    }
    Ok(())
}

/// 断点续传决策(纯函数,便于单测)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumePlan {
    /// 全新上传:远端不存在同名文件,或调用方未启用续传
    Fresh,
    /// 续传:从远端已有 `offset` 字节处继续写(不截断)
    Resume(u64),
    /// 远端文件已不小于本地,视为传输已完成,无需再传
    AlreadyDone,
}

/// 依据远端同名文件大小与本地大小决定上传策略:
/// `None`(远端不存在)或 `resume = false` → 全新上传;
/// `remote < local` → 从 `remote` 偏移续传;`remote >= local` → 已完成。
pub fn resume_plan(remote: Option<u64>, local: u64, resume: bool) -> ResumePlan {
    if !resume {
        return ResumePlan::Fresh;
    }
    match remote {
        None => ResumePlan::Fresh,
        Some(remote_size) if remote_size < local => ResumePlan::Resume(remote_size),
        Some(_) => ResumePlan::AlreadyDone,
    }
}

/// 用给定 SFTP 会话查询远端文件大小;文件不存在(SSH_FX_NO_SUCH_FILE)→ `None`,
/// 其余错误以中文 `Err` 传播。
async fn stat_remote_size(sftp: &SftpSession, remote_path: &str) -> Result<Option<u64>, String> {
    match sftp.metadata(remote_path.to_string()).await {
        Ok(meta) => Ok(Some(meta.len())),
        Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => Ok(None),
        Err(e) => Err(format!("SFTP 查询远端文件大小失败 ({}): {}", remote_path, e)),
    }
}

/// 把本地文件从 `start_offset` 字节处起按 64KB 分块写入远端 SFTP 文件,
/// 累计 `sent`(调用方进入时已含偏移基数)并按块回调进度。
///
/// `start_offset = 0` → CREATE|WRITE|TRUNCATE 全新写;
/// `start_offset > 0` → CREATE|WRITE(不截断)打开,本地读指针与远端写指针
/// 均 seek 到偏移处续写(断点续传;russh-sftp 的写句柄按内部偏移发包)。
async fn copy_file_to_remote(
    sftp: &SftpSession,
    local: &Path,
    remote_path: &str,
    start_offset: u64,
    sent: &mut u64,
    total: u64,
    on_progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), String> {
    let mut local_file = tokio::fs::File::open(local)
        .await
        .map_err(|e| format!("打开本地文件失败 ({}): {}", local.display(), e))?;
    let flags = if start_offset > 0 {
        OpenFlags::CREATE | OpenFlags::WRITE
    } else {
        OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE
    };
    let mut remote_file = sftp
        .open_with_flags(remote_path.to_string(), flags)
        .await
        .map_err(|e| format!("SFTP 打开远端文件失败 ({}): {}", remote_path, e))?;

    if start_offset > 0 {
        // 断点续传:本地读指针与远端写指针都跳过已传前缀
        local_file
            .seek(SeekFrom::Start(start_offset))
            .await
            .map_err(|e| format!("本地文件 seek 失败 ({}): {}", local.display(), e))?;
        remote_file
            .seek(SeekFrom::Start(start_offset))
            .await
            .map_err(|e| format!("SFTP 远端文件 seek 失败 ({}): {}", remote_path, e))?;
    }

    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = local_file
            .read(&mut buf)
            .await
            .map_err(|e| format!("读取本地文件失败 ({}): {}", local.display(), e))?;
        if n == 0 {
            break;
        }
        remote_file
            .write_all(&buf[..n])
            .await
            .map_err(|e| format!("SFTP 写入远端文件失败 ({}): {}", remote_path, e))?;
        *sent += n as u64;
        on_progress(*sent, total);
    }
    // 显式 shutdown(等价 File::close)以等待远端确认,避免 Drop 静默丢弃写入错误
    remote_file
        .shutdown()
        .await
        .map_err(|e| format!("SFTP 关闭远端文件失败 ({}): {}", remote_path, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, ServerConfig};

    // ===== 纯函数单测(无网络依赖)=====

    #[test]
    fn test_mkdir_p_cmd_basic() {
        assert_eq!(mkdir_p_cmd("/opt/app"), "mkdir -p '/opt/app'");
    }

    #[test]
    fn test_mkdir_p_cmd_escapes_single_quote() {
        assert_eq!(mkdir_p_cmd("/opt/a'b"), "mkdir -p '/opt/a'\\''b'");
    }

    #[test]
    fn test_mkdir_p_cmd_nested_path() {
        assert_eq!(
            mkdir_p_cmd("/data/app/releases/2026"),
            "mkdir -p '/data/app/releases/2026'"
        );
    }

    // ===== Task 4:resume_plan 续传决策 =====

    #[test]
    fn test_resume_plan_remote_missing_is_fresh() {
        // 远端不存在 → 全新上传(无论 resume 开关)
        assert_eq!(resume_plan(None, 100, true), ResumePlan::Fresh);
        assert_eq!(resume_plan(None, 100, false), ResumePlan::Fresh);
    }

    #[test]
    fn test_resume_plan_disabled_is_fresh() {
        // resume=false 恒为全新上传(即使远端有更小的同名文件)
        assert_eq!(resume_plan(Some(40), 100, false), ResumePlan::Fresh);
        assert_eq!(resume_plan(Some(100), 100, false), ResumePlan::Fresh);
        assert_eq!(resume_plan(Some(200), 100, false), ResumePlan::Fresh);
    }

    #[test]
    fn test_resume_plan_remote_smaller_resumes_from_remote() {
        // 远端更小 → 从远端大小处续传
        assert_eq!(resume_plan(Some(0), 100, true), ResumePlan::Resume(0));
        assert_eq!(resume_plan(Some(1), 100, true), ResumePlan::Resume(1));
        assert_eq!(
            resume_plan(Some(99), 100, true),
            ResumePlan::Resume(99)
        );
    }

    #[test]
    fn test_resume_plan_remote_equal_is_already_done() {
        // 远端等于本地 → 视为已完成
        assert_eq!(resume_plan(Some(100), 100, true), ResumePlan::AlreadyDone);
        // 边界:双方均为 0 字节
        assert_eq!(resume_plan(Some(0), 0, true), ResumePlan::AlreadyDone);
    }

    #[test]
    fn test_resume_plan_remote_larger_is_already_done() {
        // 远端更大(如上次是别的更大的文件)→ 不续写、不截断,视为已完成
        assert_eq!(resume_plan(Some(200), 100, true), ResumePlan::AlreadyDone);
    }

    // ===== 可选真机测试:需要可连通的 SSH 服务器,默认 #[ignore] =====
    //
    // 运行示例(密码认证):
    //   DD_SSH_TEST_HOST=1.2.3.4 DD_SSH_TEST_USER=root DD_SSH_TEST_PASSWORD=xxx \
    //     cargo test ssh:: -- --ignored
    // 密钥认证:把 DD_SSH_TEST_PASSWORD 换成 DD_SSH_TEST_KEY=/path/to/id_rsa

    #[allow(dead_code)]
    fn test_cfg_from_env() -> Option<ServerConfig> {
        let host = std::env::var("DD_SSH_TEST_HOST").ok()?;
        let username = std::env::var("DD_SSH_TEST_USER").ok()?;
        let auth = match std::env::var("DD_SSH_TEST_KEY") {
            Ok(key_path) => AuthConfig {
                auth_type: AuthType::Key,
                key_path: Some(key_path),
                password_enc: None,
            },
            Err(_) => AuthConfig {
                auth_type: AuthType::Password,
                key_path: None,
                password_enc: None,
            },
        };
        Some(ServerConfig {
            id: "test".into(),
            name: "test".into(),
            host,
            port: 22,
            username,
            auth,
            remote_dir: "/tmp/dd-ssh-test".into(),
        })
    }

    #[tokio::test]
    #[ignore = "需要真实 SSH 服务器(见函数注释的运行方式)"]
    async fn test_connect_and_exec_real() {
        let cfg = test_cfg_from_env()
            .expect("请设置 DD_SSH_TEST_HOST / DD_SSH_TEST_USER / (DD_SSH_TEST_PASSWORD | DD_SSH_TEST_KEY)");
        let pw = std::env::var("DD_SSH_TEST_PASSWORD").ok();
        let mut client = SshClient::connect(&cfg, pw.as_deref())
            .await
            .expect("connect 失败");

        let mut out = String::new();
        let code = client
            .exec("echo hello && whoami", &mut |line| out.push_str(line))
            .await
            .expect("exec 失败");
        assert_eq!(code, 0);
        assert!(out.contains("hello"), "输出应包含 hello,实际: {out}");
    }

    #[tokio::test]
    #[ignore = "需要真实 SSH 服务器(见函数注释的运行方式)"]
    async fn test_sftp_upload_real() {
        let cfg = test_cfg_from_env()
            .expect("请设置 DD_SSH_TEST_HOST / DD_SSH_TEST_USER / (DD_SSH_TEST_PASSWORD | DD_SSH_TEST_KEY)");
        let pw = std::env::var("DD_SSH_TEST_PASSWORD").ok();
        let mut client = SshClient::connect(&cfg, pw.as_deref())
            .await
            .expect("connect 失败");

        // 准备本地目录:a.txt + nested/b.txt
        let tmp = std::env::temp_dir().join(format!("dd-ssh-sftp-test-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("nested")).unwrap();
        std::fs::write(tmp.join("a.txt"), b"hello sftp").unwrap();
        std::fs::write(tmp.join("nested/b.txt"), b"nested file").unwrap();

        let remote = "/tmp/dd-ssh-sftp-test-upload";
        let last = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let last2 = Arc::clone(&last);
        client
            .sftp_upload_dir(&tmp, remote, &move |sent, total| {
                last2.store(sent, std::sync::atomic::Ordering::Relaxed);
                assert!(sent <= total, "进度回调: 已传 {} 不应大于总 {}", sent, total);
            })
            .await
            .expect("sftp_upload_dir 失败");
        assert!(last.load(std::sync::atomic::Ordering::Relaxed) > 0, "应有进度回调");

        // 单文件上传到同一目录(全新写)
        client
            .sftp_upload(&tmp.join("a.txt"), remote, "single.txt", false, &|_, _| {})
            .await
            .expect("sftp_upload 失败");

        // 断点续传:远端同名文件比本地大 → 视为已完成,不应改写远端内容
        client
            .sftp_upload(&tmp.join("a.txt"), remote, "single.txt", true, &|sent, total| {
                assert_eq!(sent, total, "远端更大时回调应为 (total, total)");
            })
            .await
            .expect("sftp_upload(resume) 失败");
        let (code, out) = exec_collect(&mut client, "cat /tmp/dd-ssh-sftp-test-upload/single.txt")
            .await
            .unwrap();
        assert_eq!(code, 0);
        assert!(out.starts_with("hello sftp"), "远端内容不符,实际: {out}");

        // 验证远端内容
        let (code, out) = exec_collect(&mut client, "cat /tmp/dd-ssh-sftp-test-upload/a.txt")
            .await
            .unwrap();
        assert_eq!(code, 0);
        assert!(out.starts_with("hello sftp"), "远端内容不符,实际: {out}");
        let (code, out) = exec_collect(&mut client, "cat /tmp/dd-ssh-sftp-test-upload/nested/b.txt")
            .await
            .unwrap();
        assert_eq!(code, 0);
        assert!(out.starts_with("nested file"), "远端子目录内容不符,实际: {out}");

        // 环境检查顺手覆盖
        let report = check_server_env(&mut client, remote).await.expect("check_server_env 失败");
        assert!(report.remote_dir_exists, "远端目录应存在");
        assert!(report.disk_free_gb >= 0.0);

        // 清理
        client
            .exec("rm -rf /tmp/dd-ssh-sftp-test-upload", &mut |_| {})
            .await
            .ok();
        std::fs::remove_dir_all(&tmp).ok();
    }
}
