//! SSH/SFTP 模块(任务 3)。
//!
//! 基于 `russh` 0.46 + `russh-sftp` 2.x 提供:
//! - [`SshClient::connect`]:密钥(PEM,支持加密私钥 + 口令)/ 密码两种认证,
//!   并内置主机密钥 TOFU 校验(见下)
//! - [`SshClient::exec`]:开通道执行命令,stdout+stderr 合并按行实时回调,返回退出码
//! - [`SshClient::sftp_upload`]:单文件上传(可选断点续传),带字节进度回调
//! - [`SshClient::sftp_upload_dir`]:整目录上传,带字节进度回调
//! - [`SshClient::sftp_stat_size`]:查询远端文件大小(断点续传的决策依据)
//! - [`check_server_env`]:远端 docker / compose / gzip / 目录 / 磁盘环境探测
//!
//! 主机密钥安全(TOFU,阶段三):首次连接接受服务器主机密钥并把观察到的
//! OpenSSH 风格指纹(`SHA256:` + base64(nopad))经 `observed_host_key` 交由
//! 调用方落盘(`ServerConfig.host_key_sha256`);此后每次连接在
//! [`ClientHandler::check_server_key`] 中与配置的期望指纹比对,不一致即拒绝
//! 连接(防中间人;服务器重装/换 IP 后由用户显式重新信任)。
//!
//! 阶段九/十追加(独立 impl 块,纯追加):[`SshClient::exec_streaming`]
//! (可取消的流式日志,live-follow 日志数据源)与 [`SshClient::sftp_download`]
//! (SFTP 下载,跨服务器镜像迁移数据源)。

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

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

/// 主机密钥处理器(携带状态):按 TOFU 策略校验服务器主机密钥。
///
/// - `expected`:`ServerConfig.host_key_sha256`(期望指纹);`None` = 首次连接,
///   接受并记录(TOFU 信任-on-first-use);
/// - `observed`:调用方传入的共享槽位,记录本次连接观察到的指纹,
///   供调用方在连接成功后做首次落盘。
struct ClientHandler {
    expected: Option<String>,
    observed: Arc<OnceLock<String>>,
}

/// 计算 OpenSSH 风格的服务器主机密钥指纹:
/// `SHA256:` + base64(nopad)(SHA-256(公钥 SSH wire blob))。
///
/// russh 0.46 的 [`russh::keys::key::PublicKey::fingerprint`] 已实现
/// blob(`public_key_bytes()`)→ SHA-256 → base64(nopad) 全流程,
/// 仅缺 `SHA256:` 前缀,这里补齐为与 `ssh-keygen -lf` 一致的形态。
fn host_fingerprint(key: &russh::keys::key::PublicKey) -> String {
    format!("SHA256:{}", key.fingerprint())
}

/// 私钥加载错误的中文映射(纯函数,便于单测)。
///
/// - 私钥已加密但未提供口令(OpenSSH 格式)→ 提示输入口令;
/// - 提供了口令仍解不开(PEM 解密失败:PKCS#8 → `Pkcs8`,OpenSSH →
///   `KeyIsCorrupt`)→ 「私钥口令错误或私钥已损坏」;
/// - 无口令且解析失败(PKCS#8 加密格式无口令时解析即失败,`Pkcs8`/`Der`)
///   → 提示「可能已加密需口令」或文件损坏;
/// - 其余(路径不存在、格式不支持等)→ 通用「加载私钥失败」并附原始错误。
fn map_key_load_error(key_path: &str, had_passphrase: bool, e: russh::keys::Error) -> String {
    use crate::errors::{tagged, ErrCode};
    match e {
        russh::keys::Error::KeyIsEncrypted => tagged(
            ErrCode::Auth,
            format!(
                "私钥已加密,请输入私钥口令或先在服务器设置中保存口令 ({})",
                key_path
            ),
        ),
        russh::keys::Error::KeyIsCorrupt | russh::keys::Error::CouldNotReadKey
        | russh::keys::Error::Pkcs8(_)
            if had_passphrase =>
        {
            tagged(
                ErrCode::Auth,
                format!("私钥口令错误或私钥已损坏 ({})", key_path),
            )
        }
        russh::keys::Error::Pkcs8(_) | russh::keys::Error::Der(_) => tagged(
            ErrCode::Auth,
            format!(
                "加载私钥失败 ({}): 私钥可能已加密(需提供口令)或文件已损坏",
                key_path
            ),
        ),
        // 文件读不到/权限不足等本地文件系统问题
        e => tagged(
            ErrCode::Fs,
            format!("加载私钥失败 ({}): {}", key_path, e),
        ),
    }
}

#[async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let fingerprint = host_fingerprint(server_public_key);
        // 无论接受与否都记录观察值(拒绝路径的错误信息也可引用,调用方可读)
        let _ = self.observed.set(fingerprint.clone());
        match &self.expected {
            // 首次连接:接受并记录(TOFU;落盘由调用方完成)
            None => Ok(true),
            // 已有期望指纹:一致才接受,否则拒绝(russh 以 UnknownKey 错误中止建连)
            Some(expected) => Ok(expected == &fingerprint),
        }
    }
}

/// 一条已完成认证的 SSH 连接(exec / SFTP 每次各自开通道,可复用连接)。
pub struct SshClient {
    handle: Handle<ClientHandler>,
}

impl SshClient {
    /// 建立连接并完成认证(含主机密钥 TOFU 校验)。
    ///
    /// - `AuthType::Key`:读取 `cfg.auth.key_path` 指向的 PEM 私钥文件;
    ///   加密私钥经 `key_passphrase` 解密(口令由调用方经
    ///   `commands::resolve_key_passphrase` 从 DPAPI 密文解析),口令错误映射为
    ///   「私钥口令错误或私钥已损坏」;未加密私钥传 `None` 即可。
    /// - `AuthType::Password`:使用 `password_plain`;若为 `None` 返回 `Err("需要密码")`。
    ///   (DPAPI 解密在配置/命令层完成,不在本模块内。)
    /// - 主机密钥 TOFU:`cfg.host_key_sha256` 为期望指纹(缺省 = 首次连接接受);
    ///   服务器指纹与之不一致 → 拒绝建连并返回固定中文错误(前端引导重新信任)。
    ///   本次观察到的指纹写入 `observed_host_key`,首次连接后由调用方落盘
    ///   (见 `commands::persist_host_key_if_needed`);调用方不关心时可传
    ///   `Arc::default()`。
    pub async fn connect(
        cfg: &ServerConfig,
        password_plain: Option<&str>,
        key_passphrase: Option<&str>,
        observed_host_key: Arc<OnceLock<String>>,
    ) -> Result<Self, String> {
        let config = Arc::new(client::Config::default());
        let handler = ClientHandler {
            expected: cfg.host_key_sha256.clone(),
            observed: observed_host_key,
        };
        let mut handle = client::connect(config, (cfg.host.as_str(), cfg.port), handler)
            .await
            .map_err(|e| match e {
                // check_server_key 返回 false 时 russh 以 UnknownKey 中止建连
                russh::Error::UnknownKey => crate::errors::tagged(
                    crate::errors::ErrCode::Auth,
                    "服务器主机密钥已变更!可能为服务器重装/换 IP,也可能存在中间人风险。如确认无误,请在服务器管理中重新信任该主机。",
                ),
                // 其余建连失败:对端不可达/网络断,挂 Transport(连接不可信)
                e => crate::errors::tagged(
                    crate::errors::ErrCode::Transport,
                    format!("SSH 连接失败 ({}:{}): {}", cfg.host, cfg.port, e),
                ),
            })?;

        match cfg.auth.auth_type {
            AuthType::Key => {
                let key_path = cfg.auth.key_path.as_deref().ok_or_else(|| {
                    crate::errors::tagged(
                        crate::errors::ErrCode::Config,
                        "SSH 密钥认证失败: 未配置私钥路径(key_path 为空)",
                    )
                })?;
                let key = russh::keys::load_secret_key(key_path, key_passphrase)
                    .map_err(|e| map_key_load_error(key_path, key_passphrase.is_some(), e))?;
                let ok = handle
                    .authenticate_publickey(&cfg.username, Arc::new(key))
                    .await
                    .map_err(|e| {
                        crate::errors::tagged(
                            crate::errors::ErrCode::Auth,
                            format!("SSH 公钥认证失败 (用户 {}): {}", cfg.username, e),
                        )
                    })?;
                if !ok {
                    return Err(crate::errors::tagged(
                        crate::errors::ErrCode::Auth,
                        format!(
                            "SSH 公钥认证失败: 服务器拒绝了密钥 (用户 {}, 私钥 {})",
                            cfg.username, key_path
                        ),
                    ));
                }
            }
            AuthType::Password => {
                let password = password_plain
                    .ok_or_else(|| crate::errors::tagged(crate::errors::ErrCode::Input, "需要密码"))?;
                let ok = handle
                    .authenticate_password(&cfg.username, password)
                    .await
                    .map_err(|e| {
                        crate::errors::tagged(
                            crate::errors::ErrCode::Auth,
                            format!("SSH 密码认证失败 (用户 {}): {}", cfg.username, e),
                        )
                    })?;
                if !ok {
                    return Err(crate::errors::tagged(
                        crate::errors::ErrCode::Auth,
                        format!("SSH 密码认证失败: 密码错误或被拒绝 (用户 {})", cfg.username),
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
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SSH 打开会话通道失败: {}", e)))?;
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SSH 执行命令失败 ({}): {}", cmd, e)))?;

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
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("读取本地文件元数据失败 ({}): {}", local.display(), e)))?
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
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("遍历本地目录失败 ({}): {}", local_dir.display(), e)))?;
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
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SSH 打开 SFTP 通道失败: {}", e)))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SSH 请求 sftp 子系统失败: {}", e)))?;
        SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SFTP 会话初始化失败: {}", e)))
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
        Err(e) => Err(crate::errors::tagged(crate::errors::ErrCode::Fs, format!("SFTP 查询远端文件大小失败 ({}): {}", remote_path, e))),
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
        .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("打开本地文件失败 ({}): {}", local.display(), e)))?;
    let flags = if start_offset > 0 {
        OpenFlags::CREATE | OpenFlags::WRITE
    } else {
        OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE
    };
    let mut remote_file = sftp
        .open_with_flags(remote_path.to_string(), flags)
        .await
        .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("SFTP 打开远端文件失败 ({}): {}", remote_path, e)))?;

    if start_offset > 0 {
        // 断点续传:本地读指针与远端写指针都跳过已传前缀
        local_file
            .seek(SeekFrom::Start(start_offset))
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("本地文件 seek 失败 ({}): {}", local.display(), e)))?;
        remote_file
            .seek(SeekFrom::Start(start_offset))
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("SFTP 远端文件 seek 失败 ({}): {}", remote_path, e)))?;
    }

    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = local_file
            .read(&mut buf)
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("读取本地文件失败 ({}): {}", local.display(), e)))?;
        if n == 0 {
            break;
        }
        remote_file
            .write_all(&buf[..n])
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("SFTP 写入远端文件失败 ({}): {}", remote_path, e)))?;
        *sent += n as u64;
        on_progress(*sent, total);
    }
    // 显式 shutdown(等价 File::close)以等待远端确认,避免 Drop 静默丢弃写入错误
    remote_file
        .shutdown()
        .await
        .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("SFTP 关闭远端文件失败 ({}): {}", remote_path, e)))?;
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

    // ===== 阶段三:主机密钥指纹(TOFU)与私钥口令错误映射(离线单测)=====

    /// OpenSSH 风格指纹格式:`SHA256:` + 43 字符 base64(nopad,URL 安全字母表),
    /// 同一密钥稳定、不同密钥互异。
    #[test]
    fn test_host_fingerprint_format() {
        use base64::engine::general_purpose::STANDARD_NO_PAD as B64_NOPAD;
        use base64::Engine as _;

        let key = russh::keys::key::KeyPair::generate_ed25519();
        let pk = key.clone_public_key().expect("clone_public_key 失败");
        let fp = host_fingerprint(&pk);

        assert!(fp.starts_with("SHA256:"), "指纹应有 SHA256: 前缀: {fp}");
        let body = &fp["SHA256:".len()..];
        assert_eq!(body.len(), 43, "SHA-256 base64(nopad) 应为 43 字符: {body}");
        // russh 内部用 data_encoding::BASE64_NOPAD(标准字母表 + /,无填充)
        assert!(
            body.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/'),
            "应为标准字母表 base64(nopad): {body}"
        );
        // base64 解码后恰为 32 字节(SHA-256 摘要)
        assert_eq!(B64_NOPAD.decode(body).unwrap().len(), 32);
        // 同一密钥两次计算结果一致
        assert_eq!(host_fingerprint(&pk), fp);

        // 不同密钥指纹互异
        let pk2 = russh::keys::key::KeyPair::generate_ed25519()
            .clone_public_key()
            .unwrap();
        assert_ne!(host_fingerprint(&pk2), fp);
    }

    /// TOFU 判定:期望为 None → 接受并记录观察值;一致 → 接受;不一致 → 拒绝。
    #[tokio::test]
    async fn test_check_server_key_tofu() {
        use russh::client::Handler as _;

        let pk = russh::keys::key::KeyPair::generate_ed25519()
            .clone_public_key()
            .expect("clone_public_key 失败");
        let fp = host_fingerprint(&pk);

        // 首次连接(expected=None):接受,且 observed 记录指纹
        let observed = Arc::new(OnceLock::new());
        let mut handler = ClientHandler {
            expected: None,
            observed: Arc::clone(&observed),
        };
        assert!(handler.check_server_key(&pk).await.unwrap());
        assert_eq!(observed.get().map(String::as_str), Some(fp.as_str()));

        // 期望指纹一致 → 接受
        let observed2 = Arc::new(OnceLock::new());
        let mut handler = ClientHandler {
            expected: Some(fp.clone()),
            observed: Arc::clone(&observed2),
        };
        assert!(handler.check_server_key(&pk).await.unwrap());
        assert_eq!(observed2.get().map(String::as_str), Some(fp.as_str()));

        // 期望指纹不一致(服务器换 key / 中间人)→ 拒绝
        let mut handler = ClientHandler {
            expected: Some("SHA256:mismatched-fingerprint-value-000000000000000".into()),
            observed: Arc::new(OnceLock::new()),
        };
        assert!(!handler.check_server_key(&pk).await.unwrap());
    }

    /// 私钥口令错误映射:加密私钥无口令 → KeyIsEncrypted 提示;
    /// 有口令但解不开 → 「私钥口令错误或私钥已损坏」;无口令路径 → 通用失败。
    #[test]
    fn test_map_key_load_error() {
        use crate::errors::{code_of, strip, ErrCode};
        // Error 未实现 Copy,各断言分别构造
        let enc = map_key_load_error("/k", false, russh::keys::Error::KeyIsEncrypted);
        assert!(
            strip(&enc).contains("私钥已加密"),
            "加密私钥未提供口令应提示输入口令"
        );
        assert_eq!(code_of(&enc), Some(ErrCode::Auth));
        let wrong_pass = map_key_load_error("/k", true, russh::keys::Error::KeyIsCorrupt);
        assert_eq!(strip(&wrong_pass), "私钥口令错误或私钥已损坏 (/k)");
        assert_eq!(code_of(&wrong_pass), Some(ErrCode::Auth));
        let corrupt = map_key_load_error("/k", true, russh::keys::Error::CouldNotReadKey);
        assert_eq!(strip(&corrupt), "私钥口令错误或私钥已损坏 (/k)");
        // 未提供口令时的损坏/读失败:走通用「加载私钥失败」并附原始错误
        let generic = map_key_load_error("/k", false, russh::keys::Error::KeyIsCorrupt);
        assert_eq!(strip(&generic), "加载私钥失败 (/k): The key is corrupt");
        // 本地文件系统类(读不到/权限不足)挂 fs 码
        assert_eq!(code_of(&generic), Some(ErrCode::Fs));
    }

    /// 离线加密私钥 roundtrip:russh 生成的 ed25519 密钥加密为 PKCS#8 PEM 后,
    /// 正确口令可解开,错误口令按 russh 实际错误被映射为「口令错误或已损坏」。
    #[test]
    fn test_load_encrypted_pem_key() {
        let key = russh::keys::key::KeyPair::generate_ed25519();
        let path = std::env::temp_dir().join(format!("dd-enc-key-{}.pem", uuid::Uuid::new_v4()));
        let f = std::fs::File::create(&path).unwrap();
        russh::keys::encode_pkcs8_pem_encrypted(&key, b"correct-pass", 3, f).unwrap();

        // 正确口令 → 解开
        assert!(russh::keys::load_secret_key(&path, Some("correct-pass")).is_ok());
        // 错误口令 → 解密失败(PKCS#8 路径为 Pkcs8 错误)→ 映射为口令错误提示
        let err = russh::keys::load_secret_key(&path, Some("wrong-pass")).unwrap_err();
        assert!(
            matches!(err, russh::keys::Error::Pkcs8(_)),
            "错误口令应为 Pkcs8 解密失败,实际: {:?}",
            err
        );
        assert!(map_key_load_error("x", true, err).contains("私钥口令错误或私钥已损坏"));
        // 无口令 → 解析失败(Der 错误)→ 提示可能已加密需口令
        let err = russh::keys::load_secret_key(&path, None).unwrap_err();
        assert!(matches!(err, russh::keys::Error::Der(_)));
        assert!(
            map_key_load_error("x", false, err).contains("私钥可能已加密"),
            "无口令加载加密私钥应提示需要口令"
        );

        std::fs::remove_file(&path).ok();
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
                key_pass_enc: None,
            },
            Err(_) => AuthConfig {
                auth_type: AuthType::Password,
                key_path: None,
                password_enc: None,
                key_pass_enc: None,
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
            host_key_sha256: None,
        })
    }

    #[tokio::test]
    #[ignore = "需要真实 SSH 服务器(见函数注释的运行方式)"]
    async fn test_connect_and_exec_real() {
        let cfg = test_cfg_from_env()
            .expect("请设置 DD_SSH_TEST_HOST / DD_SSH_TEST_USER / (DD_SSH_TEST_PASSWORD | DD_SSH_TEST_KEY)");
        let pw = std::env::var("DD_SSH_TEST_PASSWORD").ok();
        let mut client = SshClient::connect(&cfg, pw.as_deref(), None, Arc::default())
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

    // ===== 阶段九:exec_streaming(流式 + 取消)真机测试 =====

    #[tokio::test]
    #[ignore = "需要真实 SSH 服务器(见函数注释的运行方式)"]
    async fn test_exec_streaming_and_cancel_real() {
        let cfg = test_cfg_from_env()
            .expect("请设置 DD_SSH_TEST_HOST / DD_SSH_TEST_USER / (DD_SSH_TEST_PASSWORD | DD_SSH_TEST_KEY)");
        let pw = std::env::var("DD_SSH_TEST_PASSWORD").ok();
        let mut client = SshClient::connect(&cfg, pw.as_deref(), None, Arc::default())
            .await
            .expect("connect 失败");

        // 1) 自然结束:echo 三行,输出逐行回调,退出码 0
        let (mut cancel_tx, mut cancel_rx) = tokio::sync::mpsc::channel::<()>(1);
        let mut lines: Vec<String> = Vec::new();
        let mut exit_code: i32 = -1;
        let finished = client
            .exec_streaming(
                "echo a; echo b; echo c",
                &mut cancel_rx,
                &mut exit_code,
                &mut |line: &str| lines.push(line.trim_end().to_string()),
            )
            .await
            .expect("exec_streaming 失败");
        assert!(finished, "echo 命令应自然结束");
        assert_eq!(exit_code, 0);
        assert!(lines.iter().any(|l| l.contains("a")));
        assert!(lines.iter().any(|l| l.contains("c")));
        // cancel_tx 保持存活直到此刻(证明「Sender drop = 取消」的约定)
        cancel_tx.send(()).await.ok();

        // 2) 取消:yes 持续输出 → 收到取消信号后应尽快返回 Ok(false)
        let (mut cancel_tx2, mut cancel_rx2) = tokio::sync::mpsc::channel::<()>(1);
        let mut count: usize = 0;
        let mut exit2: i32 = -1;
        let finished2 = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            client.exec_streaming(
                "yes",
                &mut cancel_rx2,
                &mut exit2,
                &mut |_line: &str| count += 1,
            ),
        )
        .await
        .expect("取消测试整体超时")
        .expect("exec_streaming(yes) 失败");
        assert!(!finished2, "被取消后应返回 false");
        cancel_tx2.send(()).await.ok();
        let _ = count; // 收到若干行后即被取消,行数不定
    }

    // ===== 阶段十:sftp_download(下载 roundtrip)真机测试 =====

    #[tokio::test]
    #[ignore = "需要真实 SSH 服务器(见函数注释的运行方式)"]
    async fn test_sftp_download_real() {
        let cfg = test_cfg_from_env()
            .expect("请设置 DD_SSH_TEST_HOST / DD_SSH_TEST_USER / (DD_SSH_TEST_PASSWORD | DD_SSH_TEST_KEY)");
        let pw = std::env::var("DD_SSH_TEST_PASSWORD").ok();
        let mut client = SshClient::connect(&cfg, pw.as_deref(), None, Arc::default())
            .await
            .expect("connect 失败");

        // 远端造一个含二进制字节的文件(覆盖 0x00-0xFF 全字节值)
        let remote = "/tmp/dd-ssh-sftp-test-download.bin";
        let (code, _) = crate::ssh::exec_collect(
            &mut client,
            "od -An -v -tu1 /dev/zero | head -c 0; printf '' > /tmp/dd-ssh-sftp-test-download.bin; for i in $(seq 0 255); do printf \"\\\\$(printf '%03o' $i)\" >> /tmp/dd-ssh-sftp-test-download.bin; done",
        )
        .await
        .expect("造远端文件失败");
        assert_eq!(code, 0);

        // 下载并核对内容逐字节一致
        let local = std::env::temp_dir().join(format!("dd-dl-{}.bin", std::process::id()));
        let last = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let last2 = Arc::clone(&last);
        client
            .sftp_download(remote, &local, &move |sent, total| {
                last2.store(sent, std::sync::atomic::Ordering::Relaxed);
                assert!(sent <= total);
            })
            .await
            .expect("sftp_download 失败");
        let bytes = std::fs::read(&local).expect("读本地下载文件失败");
        assert_eq!(bytes.len(), 256);
        for (i, b) in bytes.iter().enumerate() {
            assert_eq!(*b, i as u8, "第 {} 字节不符", i);
        }
        assert!(last.load(std::sync::atomic::Ordering::Relaxed) > 0, "应有进度回调");

        // 清理
        client.exec(&format!("rm -f {}", remote), &mut |_| {}).await.ok();
        std::fs::remove_file(&local).ok();
    }

    #[tokio::test]
    #[ignore = "需要真实 SSH 服务器(见函数注释的运行方式)"]
    async fn test_sftp_upload_real() {
        let cfg = test_cfg_from_env()
            .expect("请设置 DD_SSH_TEST_HOST / DD_SSH_TEST_USER / (DD_SSH_TEST_PASSWORD | DD_SSH_TEST_KEY)");
        let pw = std::env::var("DD_SSH_TEST_PASSWORD").ok();
        let mut client = SshClient::connect(&cfg, pw.as_deref(), None, Arc::default())
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

    // ===== 第六批:项目迁移的数据卷搬运(导出 → 导入 roundtrip)=====

    /// 验证「命名卷经临时容器 tar 导出 → 导入到另一个卷」的完整链路:
    /// 写入已知内容 → 导出 → 导入到新卷 → 比对新卷内容与原始内容逐字节一致。
    ///
    /// 需要服务器可执行 docker 且能拉取 busybox(或已存在 busybox/alpine)。
    /// 运行:`DD_SSH_TEST_HOST=... cargo test migrate_project -- --ignored`
    #[tokio::test]
    #[ignore = "需要真实 SSH 服务器 + docker(见函数注释的运行方式)"]
    async fn test_volume_export_import_roundtrip_real() {
        use crate::migrate_project::{volume_export_cmd, volume_import_cmd};

        let cfg = test_cfg_from_env()
            .expect("请设置 DD_SSH_TEST_HOST / DD_SSH_TEST_USER / (DD_SSH_TEST_PASSWORD | DD_SSH_TEST_KEY)");
        let pw = std::env::var("DD_SSH_TEST_PASSWORD").ok();

        // 用 remote_dir 作为临时工作目录,避免污染真实数据
        let tmp = "/tmp/dd-migrate-test";
        let src_vol = "dd_migrate_src";
        let dst_vol = "dd_migrate_dst";

        // 直连(不写配置),复用 connect_server 之外的裸连接:这里用 SshClient::connect
        let mut client = SshClient::connect(&cfg, pw.as_deref(), None, Arc::default())
            .await
            .expect("connect 失败");

        // 选 tar 镜像:busybox 优先
        let tar_image = {
            let (code, _) = exec_collect(&mut client, "docker image inspect busybox:latest")
                .await
                .expect("inspect 失败");
            if code == 0 {
                "busybox:latest"
            } else {
                let (code, out) = exec_collect(&mut client, "docker pull busybox:latest")
                    .await
                    .expect("pull 失败");
                assert_eq!(code, 0, "无法获取 busybox(需服务器出网或预置): {out}");
                "busybox:latest"
            }
        };

        // 准备:清理旧卷与目录
        let _ = exec_collect(
            &mut client,
            &format!("rm -rf {tmp} && mkdir -p {tmp} && docker volume rm -f {src_vol} {dst_vol} 2>/dev/null; true"),
        )
        .await
        .expect("准备环境失败");

        // 1) 建源卷并写入已知内容(含二进制字节,验证二进制安全)
        let (code, out) = exec_collect(
            &mut client,
            &format!(
                "docker volume create {src_vol} >/dev/null && \
                 docker run --rm --entrypoint sh -v {src_vol}:/data {tar_image} \
                 -c 'printf \"hello-volume\\n\" > /data/a.txt; head -c 256 /dev/urandom > /data/b.bin; mkdir -p /data/sub; echo nested > /data/sub/c.txt'"
            ),
        )
        .await
        .expect("初始化源卷失败");
        assert_eq!(code, 0, "初始化源卷失败: {out}");

        // 记录源内容摘要(逐文件 sha256)
        let (code, src_digest) = exec_collect(
            &mut client,
            &format!(
                "docker run --rm --entrypoint sh -v {src_vol}:/data {tar_image} \
                 -c 'cd /data && find . -type f | sort | xargs sha256sum'"
            ),
        )
        .await
        .expect("读取源摘要失败");
        assert_eq!(code, 0, "读取源摘要失败: {src_digest}");
        assert!(src_digest.contains("a.txt"), "源卷应有 a.txt: {src_digest}");

        // 2) 导出源卷
        let remote_pkg = format!("{tmp}/vol.tar.gz");
        let (code, out) = exec_collect(
            &mut client,
            &volume_export_cmd(tar_image, src_vol, &remote_pkg),
        )
        .await
        .expect("导出失败");
        assert_eq!(code, 0, "导出卷失败: {out}");

        let (code, size_out) = exec_collect(&mut client, &format!("stat -c %s {remote_pkg}"))
            .await
            .expect("取包大小失败");
        assert_eq!(code, 0);
        assert!(
            size_out.trim().parse::<u64>().unwrap_or(0) > 0,
            "导出的包不应为空"
        );

        // 3) 导入到新卷
        let (code, out) = exec_collect(
            &mut client,
            &volume_import_cmd(tar_image, dst_vol, &remote_pkg),
        )
        .await
        .expect("导入失败");
        assert_eq!(code, 0, "导入卷失败: {out}");

        // 4) 比对目标卷内容与源卷一致
        let (code, dst_digest) = exec_collect(
            &mut client,
            &format!(
                "docker run --rm --entrypoint sh -v {dst_vol}:/data {tar_image} \
                 -c 'cd /data && find . -type f | sort | xargs sha256sum'"
            ),
        )
        .await
        .expect("读取目标摘要失败");
        assert_eq!(code, 0, "读取目标摘要失败: {dst_digest}");
        assert_eq!(
            src_digest.trim(),
            dst_digest.trim(),
            "目标卷内容应与源卷逐字节一致"
        );

        // 5) 清理
        let _ = exec_collect(
            &mut client,
            &format!("rm -rf {tmp}; docker volume rm -f {src_vol} {dst_vol} 2>/dev/null; true"),
        )
        .await
        .ok();
    }
}

/// C 阶段追加:交互式 exec 相关方法(独立 impl 块,纯追加,不改任何既有代码)。
impl SshClient {
    /// C 阶段追加:打开交互式 PTY 会话执行命令,返回可读写的通道流。
    /// 与一次性 exec 不同,通道在调用方 Drop/关闭前保持打开,支持双向传输。
    ///
    /// 流程:channel_open_session → request_pty(xterm-256color, cols/rows, 其余默认)
    /// → exec(true, cmd) → 返回 [`russh::Channel`]。
    ///
    /// 返回 `Channel<client::Msg>` 而非 `ChannelStream`:调用方需要在同一通道上
    /// 并发做三件事 —— 读输出(`wait()`)、写 stdin(`make_writer()`)、
    /// 终端resize(`window_change()`);`into_stream()` 会消费通道且不再暴露
    /// `window_change`,`make_reader`/`make_writer` 的具体类型
    /// (`channels::io::ChannelTx`)因 `mod channels` 为私有而不可命名。
    /// 保留完整 `Channel` 后,写入端用 `make_writer()`(0.46 中该返回值
    /// 不借用 self,可安全移交给独立写任务),resize 直接走通道上的
    /// `window_change`,关闭走 `close()`。
    pub async fn exec_interactive(
        &mut self,
        cmd: &str,
        cols: u32,
        rows: u32,
    ) -> Result<russh::Channel<client::Msg>, String> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SSH 打开交互式会话通道失败: {}", e)))?;
        channel
            .request_pty(true, "xterm-256color", cols, rows, 0, 0, &[])
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SSH 请求伪终端(PTY)失败: {}", e)))?;
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SSH 启动交互式命令 ({}) 失败: {}", cmd, e)))?;
        Ok(channel)
    }
}

/// 阶段九/十追加:流式日志与 SFTP 下载(独立 impl 块,纯追加,零修改既有行)。
impl SshClient {
    /// 阶段九(UPGRADE-PLAN 第二批):流式执行命令,输出逐行实时回调。
    ///
    /// 与 [`SshClient::exec`](一次性 exec,输出收齐才返回)的区别:wait 循环
    /// 外层包了 `tokio::select!` 监听 `cancel_rx` —— 取消信号(`()` 消息)到达时
    /// **主动 `channel.close()` 并立即返回**(不再等远端自然结束),用于
    /// live-follow 日志的「关闭即停流」。输出行为与 `exec` 一致:stdout+stderr
    /// 合并、按完整行回调、不完整尾行在通道关闭时输出(被取消时丢弃尾行)。
    ///
    /// 取消通道:`tokio::sync::mpsc::Receiver<()>`,调用方(命令层)持有对应
    /// Sender,`stop` 命令发送 `()` 即停流。注意 `recv()` 返回 `Err`(全部
    /// Sender 已 drop)时按**主动取消**处理 —— 命令层在停流后 drop Sender 的
    /// 场景下,流必然已由 stop 信号先行结束;此约定保证 select 分支恒可收敛。
    ///
    /// 返回值:`Ok(true)` = 命令自然结束(退出码写入 `exit_code`,未收到为 -1);
    /// `Ok(false)` = 被取消方主动取消(通道已 close)。
    pub async fn exec_streaming(
        &mut self,
        cmd: &str,
        cancel_rx: &mut tokio::sync::mpsc::Receiver<()>,
        exit_code: &mut i32,
        on_output: &mut (dyn FnMut(&str) + Send),
    ) -> Result<bool, String> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SSH 打开会话通道失败: {}", e)))?;
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SSH 执行命令失败 ({}): {}", cmd, e)))?;

        *exit_code = -1;
        let mut buf: Vec<u8> = Vec::new();
        let cancelled = loop {
            tokio::select! {
                // 取消与通道消息并发就绪时先查取消(biased 固定轮询顺序)
                biased;
                // Ok(()) = 收到 stop 信号;Err = Sender 全部 drop(命令层收尾),
                // 两种情况都以「主动取消」收场:关通道、丢弃不完整尾行
                _ = cancel_rx.recv() => {
                    let _ = channel.close().await;
                    break true;
                }
                msg = channel.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { ref data })
                        | Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                            buf.extend_from_slice(data);
                            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                                let line: Vec<u8> = buf.drain(..=pos).collect();
                                on_output(&String::from_utf8_lossy(&line));
                            }
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => {
                            *exit_code = exit_status as i32;
                        }
                        Some(ChannelMsg::Eof) => {
                            // 服务端输出结束,继续等待 Close
                        }
                        Some(ChannelMsg::Close) | None => break false,
                        _ => {}
                    }
                }
            }
        };
        if !cancelled && !buf.is_empty() {
            on_output(&String::from_utf8_lossy(&buf));
        }
        Ok(!cancelled)
    }

    /// 阶段十(UPGRADE-PLAN 第二批):SFTP 下载远端文件到本地路径。
    ///
    /// 与上传的 [`copy_file_to_remote`] 对称:远端文件按 64KB 块读出写本地,
    /// 回调 `(已下字节, 总字节)`;显式 shutdown 等待远端确认。
    /// 远端路径经 sftp 子系统直读(不经 shell),无注入面。
    pub async fn sftp_download(
        &mut self,
        remote_path: &str,
        local: &Path,
        on_progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<(), String> {
        let sftp = self.open_sftp().await?;
        let mut remote_file = sftp
            .open(remote_path.to_string())
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("SFTP 打开远端文件失败 ({}): {}", remote_path, e)))?;
        let total = remote_file
            .metadata()
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        on_progress(0, total);

        if let Some(parent) = local.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("无法创建本地目录 {}: {}", parent.display(), e)))?;
            }
        }
        let mut local_file = tokio::fs::File::create(local)
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("无法创建本地文件 {}: {}", local.display(), e)))?;

        let mut received: u64 = 0;
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            let n = remote_file
                .read(&mut buf)
                .await
                .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("SFTP 读取远端文件失败 ({}): {}", remote_path, e)))?;
            if n == 0 {
                break;
            }
            local_file
                .write_all(&buf[..n])
                .await
                .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("写入本地文件失败 ({}): {}", local.display(), e)))?;
            received += n as u64;
            on_progress(received, total);
        }
        local_file
            .flush()
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("刷新本地文件失败 ({}): {}", local.display(), e)))?;
        // 显式关闭远端句柄(等价 File::close)以等待远端确认
        remote_file
            .shutdown()
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Fs, format!("SFTP 关闭远端文件失败 ({}): {}", remote_path, e)))?;
        Ok(())
    }

    /// 阶段十(UPGRADE-PLAN 第二批):打开原始 exec 通道(不经行拆分)。
    ///
    /// 供二进制流(`docker save | gzip`)的消费方使用:返回
    /// [`russh::Channel`],调用方自行 `wait()` 读 `Data` 块(二进制安全,
    /// 与 [`SshClient::exec`]/[`SshClient::exec_streaming`] 的按行拆分不同)。
    pub(crate) async fn raw_exec_channel(
        &mut self,
        cmd: &str,
    ) -> Result<russh::Channel<client::Msg>, String> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SSH 打开会话通道失败: {}", e)))?;
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| crate::errors::tagged(crate::errors::ErrCode::Transport, format!("SSH 执行命令失败 ({}): {}", cmd, e)))?;
        Ok(channel)
    }
}
