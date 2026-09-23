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

/// 传输块级取消判定(纯函数,便于单测;C1 第二十八批)。
///
/// - 未挂探针(`None`)→ `Ok(())`:不启用块级取消,**行为与加该功能前一致**
///   (这是默认路径 —— 47 处 `connect_server` 调用点零改动);
/// - 探针返回 false → `Ok(())`;返回 true → `Err(cancelled())`。
///
/// 提取为自由函数而非方法:方法需要真实 `SshClient`(含 SSH 连接),无法在
/// 单测里构造;而判定逻辑本身与连接无关。
pub(crate) fn transfer_cancel_check(
    probe: Option<&Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Result<(), String> {
    match probe {
        Some(p) if p() => Err(crate::errors::cancelled()),
        _ => Ok(()),
    }
}

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
/// russh 0.60 的 [`russh::keys::PublicKey::fingerprint(HashAlg::Sha256)`] 返回
/// `ssh_key::Fingerprint`,其 `Display` 直接产出与 `ssh-keygen -lf` 一致的
/// `SHA256:base64(nopad)` 形态(blob → SHA-256 → base64(nopad) + 前缀)。
fn host_fingerprint(key: &russh::keys::PublicKey) -> String {
    key.fingerprint(russh::keys::HashAlg::Sha256).to_string()
}

/// 私钥加载错误的中文映射(纯函数,便于单测)。
///
/// - 私钥已加密但未提供口令(OpenSSH 格式)→ 提示输入口令;
/// - 提供了口令仍解不开(PEM 解密失败:PKCS#8 → `Pkcs8`,OpenSSH →
///   `SshKey(解密失败)`,遗留格式 → `KeyIsCorrupt`/`Pad`/`Unpad`)
///   → 「私钥口令错误或私钥已损坏」;
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
        // 已提供口令仍解密失败:所有「解密/解填充失败」类错误都归此(OpenSSH 格式
        // 的错口令在 russh 0.60 表现为 SshKey(ssh_key::Error),v6.1.3 补上)
        russh::keys::Error::KeyIsCorrupt
        | russh::keys::Error::CouldNotReadKey
        | russh::keys::Error::SshKey(_)
        | russh::keys::Error::Pkcs8(_)
        | russh::keys::Error::Pad(_)
        | russh::keys::Error::Unpad(_)
            if had_passphrase =>
        {
            tagged(
                ErrCode::Auth,
                format!("私钥口令错误或私钥已损坏 ({})", key_path),
            )
        }
        russh::keys::Error::Pkcs8(_)
        | russh::keys::Error::Der(_)
        | russh::keys::Error::SshKey(_) => tagged(
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

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    // russh 0.60 的 Handler 用原生 `-> impl Future`(未启 async-trait feature),
    // 与 #[async_trait] 冲突(E0195 生命周期不匹配);方法体无 await,直接同步计算
    // 后包一层 async block。
    fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        let fingerprint = host_fingerprint(server_public_key);
        // 无论接受与否都记录观察值(拒绝路径的错误信息也可引用,调用方可读)
        let _ = self.observed.set(fingerprint.clone());
        let accept = match &self.expected {
            // 首次连接:接受并记录(TOFU;落盘由调用方完成)
            None => true,
            // 已有期望指纹:一致才接受,否则拒绝(russh 以 UnknownKey 错误中止建连)
            Some(expected) => expected == &fingerprint,
        };
        async move { Ok(accept) }
    }
}

/// 一条已完成认证的 SSH 连接(exec / SFTP 每次各自开通道,可复用连接)。
pub struct SshClient {
    handle: Handle<ClientHandler>,
    /// 传输级取消位(C1;第二十八批):`Some` 时块循环逐块检查,置位即中止。
    ///
    /// 为什么放在客户端而不是改 `sftp_upload` 签名:该函数有 19 处调用点
    /// (deploy 8 / migrate_project 5 / migrate 1 / rollback 1 / ssh 内部 2 /
    /// 真机测试 2),加参数会波及全部调用点且大量是 noop 回调位置;而
    /// 「谁发起传输」天然知道该看哪个取消位 —— 由发起方在建连后
    /// [`SshClient::with_cancel_probe`] 挂上,传输函数内部读取,调用点零改动。
    /// `None`(默认)= 不做传输级取消,行为与加该功能前完全一致。
    ///
    /// 为什么是谓词而非 `Arc<AtomicBool>`:两套取消位形态不同 —— 部署侧是
    /// `DeployState.cancelled`(tauri State 持有)、迁移侧在
    /// `Arc<MigrateStateInner>` 内部;统一成「返回是否已取消」的闭包即可两处
    /// 共用,无需改动任何状态的所有权结构。
    cancel_probe: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
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
        // 连接保活(第三十九批):打包/大镜像传输期间连接可能空置数分钟,服务器或
        // 中间设备按空闲超时掐断会让「复用连接」的路径(增量预查询 → 步骤 3 上传、
        // 打包 → 上传)在真正用到时报连接错误 —— keepalive 每 30s 发一次心跳
        // (max 3 次无响应即断开,取 russh 默认),开销可忽略。
        let config = Arc::new(client::Config {
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            ..client::Config::default()
        });
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
                // russh 0.60:authenticate_publickey 收 PrivateKeyWithHashAlg。
                // RSA 密钥的 hash 必须**先问服务器**(best_supported_rsa_hash):
                // 传 None 会退化为遗留 SHA-1(`ssh-rsa`),现代 OpenSSH(8.8+)默认
                // 拒绝该签名算法 → 认证失败;非 RSA 密钥该值被忽略。
                // 服务端未宣告 server-sig-algs(极老版本)时回退 None(=sha-rsa)。
                let rsa_hash = match handle.best_supported_rsa_hash().await {
                    Ok(Some(h)) => h, // Some(Some(Sha512/Sha256)) / Some(None)
                    Ok(None) => None,  // 服务器不支持 RSA(非 RSA 密钥不受影响)
                    Err(e) => {
                        log::warn!("查询服务器 RSA 签名算法失败(回退默认): {}", e);
                        None
                    }
                };
                let ok = handle
                    .authenticate_publickey(
                        &cfg.username,
                        russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash),
                    )
                    .await
                    .map_err(|e| {
                        crate::errors::tagged(
                            crate::errors::ErrCode::Auth,
                            format!("SSH 公钥认证失败 (用户 {}): {}", cfg.username, e),
                        )
                    })?;
                if !ok.success() {
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
                if !ok.success() {
                    return Err(crate::errors::tagged(
                        crate::errors::ErrCode::Auth,
                        format!("SSH 密码认证失败: 密码错误或被拒绝 (用户 {})", cfg.username),
                    ));
                }
            }
        }

        Ok(SshClient { handle, cancel_probe: None })
    }

    /// 挂上传本级取消探针(C1;第二十八批)。Builder 形态:`connect` 后一行挂载,
    /// 传输函数(含目录上传与下载)在**每个 64KB 块**之间调用一次,返回 true
    /// 即返回取消错误。默认 `None` = 不启用(既有调用点零改动、行为不变)。
    pub fn with_cancel_probe(mut self, probe: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.cancel_probe = Some(probe);
        self
    }

    /// 传输循环的块级取消检查:已取消 → `Err(取消文案)`。
    /// `pub(crate)`:命令层(migrate 的 save_gzip_remote)也要在循环里调用。
    /// 未挂取消位时恒 `Ok`(默认路径无额外开销 —— 一次 Option 判空)。
    pub(crate) fn check_transfer_cancelled(&self) -> Result<(), String> {
        transfer_cancel_check(self.cancel_probe.as_ref())
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
        // 入口检查(C1):已取消就不必开 SFTP 会话与查远端大小
        self.check_transfer_cancelled()?;

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
                copy_file_to_remote(
                    &sftp,
                    local,
                    &remote_path,
                    offset,
                    &mut sent,
                    total,
                    on_progress,
                    self.cancel_probe.as_ref(),
                )
                .await
            }
            ResumePlan::Fresh => {
                on_progress(0, total);
                copy_file_to_remote(
                    &sftp,
                    local,
                    &remote_path,
                    0,
                    &mut sent,
                    total,
                    on_progress,
                    self.cancel_probe.as_ref(),
                )
                .await
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
            // 每个文件前检查一次(块循环内还会逐块检查;此处避免为零字节
            // 文件或在块循环开始前多走一次 SFTP 打开)
            self.check_transfer_cancelled()?;
            copy_file_to_remote(
                &sftp,
                local_path,
                remote_path,
                0,
                &mut sent,
                total,
                on_progress,
                self.cancel_probe.as_ref(),
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
    /// `docker system df` 汇总(第三十四批 L2;**可选**:未探测到为 None,
    /// 前端按缺省隐藏 —— 探不到不应把环境判定染红)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docker_df: Option<DockerDfSummary>,
    /// 容器日志文件占用字节(同上,可选;非 root 读不到时为 None)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_log_bytes: Option<u64>,
}

/// `docker system df` 的可回收空间汇总(第三十四批 L2)。单位统一为字节;
/// 十进制单位按 1000 计(与 docker 显示口径一致)。
#[derive(Debug, Default, Serialize, Clone, PartialEq, Eq)]
pub struct DockerDfSummary {
    pub images_reclaimable: u64,
    pub containers_reclaimable: u64,
    pub volumes_reclaimable: u64,
    pub build_cache_reclaimable: u64,
    pub total_size: u64,
    pub total_reclaimable: u64,
}

/// 解析 docker 人类可读体积(如 `260.8MB`、`0B`、`1.5GiB`;容忍
/// `260.8MB (100%)` 这类带占比后缀的形态)。二进制单位(iB 结尾)按 1024,
/// 十进制单位按 1000 —— 与 `docker system df` 的显示口径一致。
pub fn parse_docker_size(raw: &str) -> Option<u64> {
    let s = raw.trim();
    let unit_start = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit() && *c != '.')
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let (num_part, unit_part) = s.split_at(unit_start);
    let value: f64 = num_part.parse().ok()?;
    // 单位取到空白/左括号为止(占比后缀 " (100%)" 等)
    let unit: String = unit_part
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '(')
        .collect();
    let factor: f64 = match unit.to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "pb" => 1e15,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0f64.powi(4),
        "pib" => 1024.0f64.powi(5),
        _ => return None,
    };
    Some((value * factor) as u64)
}

/// 解析 `docker system df --format '{{json .}}'` 的 NDJSON 输出
/// (逐行一个 JSON 对象;第三十四批 L2)。无任何有效行返回 None。
pub fn parse_docker_df(lines: &str) -> Option<DockerDfSummary> {
    let mut summary = DockerDfSummary::default();
    let mut seen = 0usize;
    for line in lines.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ty = v.get("Type").and_then(|t| t.as_str()).unwrap_or("");
        let size = v
            .get("Size")
            .and_then(|s| s.as_str())
            .and_then(parse_docker_size)
            .unwrap_or(0);
        let reclaim = v
            .get("Reclaimable")
            .and_then(|s| s.as_str())
            .and_then(parse_docker_size)
            .unwrap_or(0);
        seen += 1;
        summary.total_size += size;
        summary.total_reclaimable += reclaim;
        match ty {
            "Images" => summary.images_reclaimable = reclaim,
            "Containers" => summary.containers_reclaimable = reclaim,
            "Local Volumes" => summary.volumes_reclaimable = reclaim,
            "Build Cache" => summary.build_cache_reclaimable = reclaim,
            _ => {}
        }
    }
    if seen == 0 {
        None
    } else {
        Some(summary)
    }
}

/// 取输出中首个十进制整数(容器日志 `du -bc | tail -1` 的字节数;失败/空返回 None)。
pub fn parse_first_u64(out: &str) -> Option<u64> {
    let digits: String = out
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
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

    // 可回收空间 + 容器日志体积(第三十四批 L2)----
    // 目的:生产头号事故是磁盘被悬空镜像 / 容器日志撑满;磁盘紧张时前端据此提示
    // 「清理分析可回收 X」。**尽力而为**:探测失败不进 `errors`(可选增强信息,
    // 权限不足等不应把环境判定染红),字段保持 None,由前端按缺省隐藏。
    if report.docker {
        let (code, out) = exec_collect(client, "docker system df --format '{{json .}}'").await?;
        if code == 0 {
            report.docker_df = parse_docker_df(&out);
        }
        // 容器日志落盘文件体积(通常 root 私有;无权读取时 du 无输出 → None)。
        // 不依赖 GNU 专属旗标(BusyBox xargs 对空输入本就跳过):
        // docker ps -aq → inspect 取 LogPath → du -bc 求和 → 取总字节
        let log_cmd = "docker ps -aq | xargs docker inspect --format '{{.LogPath}}' 2>/dev/null | xargs du -bc 2>/dev/null | tail -1 | awk '{print $1}'";
        let (code, out) = exec_collect(client, log_cmd).await?;
        if code == 0 {
            report.container_log_bytes = parse_first_u64(&out);
        }
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
// 8 参数(含 C1 加的 cancel_probe):拆结构体反而遮蔽「进度/取消/偏移」三条
// 独立关注点,调用处也会变啰嗦(仅 3 处内部调用点)
#[allow(clippy::too_many_arguments)]
async fn copy_file_to_remote(
    sftp: &SftpSession,
    local: &Path,
    remote_path: &str,
    start_offset: u64,
    sent: &mut u64,
    total: u64,
    on_progress: &(dyn Fn(u64, u64) + Send + Sync),
    cancel_probe: Option<&Arc<dyn Fn() -> bool + Send + Sync>>,
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
        // 块级取消(C1;第二十八批):每 64KB 块之间检查一次 —— 大镜像/大卷
        // 传输中置取消位可在**一个块内**停下(此前只能等整件传完)。
        // 已写入的远端前缀保留 → 天然可被断点续传接着用(与既有断点语义一致)。
        transfer_cancel_check(cancel_probe)?;
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
    #[test]
    fn test_join_remote_and_normalize() {
        // join_remote:去尾部 '/' 后拼接(多余斜杠不产生双斜杠)
        assert_eq!(join_remote("/opt/app", "file.txt"), "/opt/app/file.txt");
        assert_eq!(join_remote("/opt/app/", "file.txt"), "/opt/app/file.txt");
        assert_eq!(join_remote("/", "f"), "/f");
        // normalize_remote:去尾部 '/',根目录保持 '/'
        assert_eq!(normalize_remote("/opt/app/"), "/opt/app");
        assert_eq!(normalize_remote("/opt/app///"), "/opt/app");
        assert_eq!(normalize_remote("/"), "/");
        assert_eq!(normalize_remote(""), "/");
    }

    fn test_mkdir_p_cmd_nested_path() {
        assert_eq!(
            mkdir_p_cmd("/data/app/releases/2026"),
            "mkdir -p '/data/app/releases/2026'"
        );
    }

    // ===== Task 4:resume_plan 续传决策 =====

    // ===== 传输块级取消(C1;第二十八批)=====

    #[test]
    fn test_transfer_cancel_check_unarmed_is_ok() {
        // 未挂探针:恒 Ok —— 这是 47 处既有 connect_server 调用点的默认路径,
        // 「不启用 = 行为与加该功能前完全一致」是 C1 的零回归前提
        assert!(transfer_cancel_check(None).is_ok());
    }

    #[test]
    fn test_transfer_cancel_check_follows_probe() {
        // 探针可动态翻转(真实场景 = 用户点「取消部署」置位取消位):
        // 未取消 → Ok;取消后 → Err(且文案带取消码)
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f2 = Arc::clone(&flag);
        let probe: Arc<dyn Fn() -> bool + Send + Sync> =
            Arc::new(move || f2.load(std::sync::atomic::Ordering::SeqCst));
        assert!(transfer_cancel_check(Some(&probe)).is_ok(), "未取消应放行");
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let err = transfer_cancel_check(Some(&probe)).unwrap_err();
        assert_eq!(crate::errors::code_of(&err), Some(crate::errors::ErrCode::Cancelled));
    }

    #[test]
    fn test_transfer_cancel_check_probe_false_stays_ok() {
        // 探针恒 false(取消位存在但未置位)不应误报取消
        let probe: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| false);
        for _ in 0..3 {
            assert!(transfer_cancel_check(Some(&probe)).is_ok());
        }
    }

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

        let key = russh::keys::PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[1u8; 32]));
        let pk = key.public_key().clone();
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
        let pk2 = russh::keys::PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[2u8; 32]))
            .public_key().clone();
        assert_ne!(host_fingerprint(&pk2), fp);
    }

    /// TOFU 判定:期望为 None → 接受并记录观察值;一致 → 接受;不一致 → 拒绝。
    #[tokio::test]
    async fn test_check_server_key_tofu() {
        use russh::client::Handler as _;

        let pk = russh::keys::PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[3u8; 32]))
            .public_key().clone();
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
        let key = russh::keys::PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[4u8; 32]));
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

    /// OpenSSH 格式加密私钥的错口令映射回归(v6.1.3):russh 0.60 下错口令表现为
    /// `SshKey(ssh_key::Error)`(而非旧版 KeyIsCorrupt),此前落进 fs 兜底分支
    /// 报「加载私钥失败」语义错误;本测试固化「口令错误 → 可操作提示」。
    #[test]
    fn test_load_openssh_encrypted_key_wrong_pass() {
        use crate::errors::{code_of, strip, ErrCode};
        // 测试专用 ed25519 加密私钥(ssh-keygen -N testpass123,仅用于本单测)
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABBLMRrBE8\n\
19GJ0b0ywMA4hbAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIF83UzTSJfcWUyQ2\n\
Qh6gucIsQNn5d1r8/mHreyig9H4FAAAAoLNJJ55vOLyuYUQ1EIZwipN174ojZRjRPmuL9Z\n\
EgANE11mVoUvh6rF6hYwgusUPtLJkWPHLr1v01iWCdhZ7aleBpO8QL6faPb6ZbZGFk/LpY\n\
3FiGGm+Ry/6Xub9eA9DMdZ138s7ak1Ut+H6N1LKTU/gfV5Hhuh6aiZoD26DXVUbNILVseD\n\
oVumCneaqWxPAiR8nXp/W2PmrSCKZH0pc1gVQ=\n\
-----END OPENSSH PRIVATE KEY-----\n";
        let path = std::env::temp_dir().join(format!("dd-enc-oss-{}.pem", uuid::Uuid::new_v4()));
        std::fs::write(&path, pem).unwrap();

        // 正确口令 → 解开
        assert!(russh::keys::load_secret_key(&path, Some("testpass123")).is_ok());
        // 错口令 → 映射为「口令错误或已损坏」,且不落 fs 兜底
        let err = russh::keys::load_secret_key(&path, Some("wrong")).unwrap_err();
        let mapped = map_key_load_error(&path.to_string_lossy(), true, err);
        assert!(
            strip(&mapped).contains("私钥口令错误或私钥已损坏"),
            "错口令应给可操作提示,实际: {}",
            strip(&mapped)
        );
        assert_eq!(code_of(&mapped), Some(ErrCode::Auth), "不应落 fs 兜底");
        // 无口令 → 提示已加密需口令
        let err2 = russh::keys::load_secret_key(&path, None).unwrap_err();
        let mapped2 = map_key_load_error("x", false, err2);
        assert!(strip(&mapped2).contains("私钥已加密") || strip(&mapped2).contains("可能已加密"));
        std::fs::remove_file(&path).ok();
    }

    /// 会丢掉默认集里的 `rsa` feature → RSA 私钥报「Unsupported key type RSA」
    /// (用户真机反馈 tencent.pem)。本测试用内嵌测试 RSA 私钥固化
    /// 「rsa feature 必须启用、RSA 私钥可被解析」——再犯此错本测试即红。
    #[test]
    fn test_load_rsa_key_supported() {
        // 测试专用 RSA 2048 私钥(ssh-keygen 生成,无口令,仅用于本单测)
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAABFwAAAAdzc2gtcn\n\
NhAAAAAwEAAQAAAQEApUESd71F013QuSyBHasR9HLdz9u9k2RzQ+XaEkI//IiCPIKsawc9\n\
8OkWfe2H17wggPWporQdoneF2nLAieR6k/kQvyMEfumLzYVkwC+x9mbf1OnRkB9VF0p0uO\n\
5RszBGJhQpooKVsJp7C42XVbdXTU0Gva9G4XpZujBi+ivM+aNU2Spa5WrDNvKPOB+Ms3fb\n\
tJTW+UUn7jxd0bRKAyo41Uw3q7tUVrZ5SiFcorY9776mvv+RVghvg49iiuA44IHMhsjA4F\n\
59knfVfqawKYErY+8Wn4Q2MYxt+YeQKkpB2G6rr2Luemu+w8Qhaan3jWKfUTFzpRrBGb6w\n\
jYKo5GDl4wAAA9An6hdXJ+oXVwAAAAdzc2gtcnNhAAABAQClQRJ3vUXTXdC5LIEdqxH0ct\n\
3P272TZHND5doSQj/8iII8gqxrBz3w6RZ97YfXvCCA9amitB2id4XacsCJ5HqT+RC/IwR+\n\
6YvNhWTAL7H2Zt/U6dGQH1UXSnS47lGzMEYmFCmigpWwmnsLjZdVt1dNTQa9r0bhelm6MG\n\
L6K8z5o1TZKlrlasM28o84H4yzd9u0lNb5RSfuPF3RtEoDKjjVTDeru1RWtnlKIVyitj3v\n\
vqa+/5FWCG+Dj2KK4DjggcyGyMDgXn2Sd9V+prApgStj7xafhDYxjG35h5AqSkHYbquvYu\n\
56a77DxCFpqfeNYp9RMXOlGsEZvrCNgqjkYOXjAAAAAwEAAQAAAQAYwCdGRyiRy2el5F7Y\n\
6l9VD8MzDO6sMwukgWTo/oKGOI0xB5f6q79WOW2Z93KKGEN8rSP2xNKkxmaw3NEDlh8vJy\n\
BKpbGtrx22RCKz5NuFqSDLIH4M/58HTv/ZFttRDYuOy870kmmzEArFJIl5rW0Q8dbESt/r\n\
M3UEPZewGnv9GNYR+Z0Ih0kxJ3H44F8dfOkbj7nlmZlWYVodMU1VXSyRU/ZCKLrV/E/Xwx\n\
y0+LTEE8rAJZ3XU2qmwDxdeMc7ePtjrSL5S0+YzsyX4r5l2Amy03cJLkBdUuw5pJa4ZSL8\n\
yXnH8cq1dw3DHoJ4NFMNI12n+xKsgfVnDdlUnIKeO3YZAAAAgQC/tST8yC5faWuWmGuE+L\n\
8b8hLKmjf5sqQ3vur7PETO2oS6y4LKghwYTybPAe6+rS6tVJhiYbqETlVuPTcwu+KJe+1o\n\
b9dl1iPMoRJhG/dC9nrmVGnyvA6nskssi9qZMZ5Vqg9OMjSVfhed7lMvWRZo2/4HdKyH4w\n\
jXDn539m3pRwAAAIEA2IOwxqz7BCRWW6FqW7xdju6Dv5i8vrHDTmJw8AzElynnXI6QDEJR\n\
Qeq+fWcMQReOrekbqvU+l3CzIulEgMZ2WgtxAKduqEyf5iw742TUBMeZ8R1CvzNn/IalxY\n\
APJhChvmURglPAXehIWzVZqDHMjW09d8aHJK6pHICkBNVHkPcAAACBAMNkOAMB/2XUjCQj\n\
+DXYlDHLPC6h8qX4W6m/uo+/7VlO8zB7VpoRTigdCbc7UG7mudSnhsZIaeKE3L22mXviKC\n\
GEx2ufYiFofF0RvKAM7021kxMUNqlvcDgyCF99qaD32vGMQueO+Prk/Lgyn7Orb0Il74zq\n\
W3hdnIKXGhKqXUN1AAAAFTE5OTMzQExBUFRPUC00SlVPMUwxMwECAwQF\n\
-----END OPENSSH PRIVATE KEY-----\n";
        let path = std::env::temp_dir().join(format!("dd-rsa-key-{}.pem", uuid::Uuid::new_v4()));
        std::fs::write(&path, pem).unwrap();
        let loaded = russh::keys::load_secret_key(&path, None);
        std::fs::remove_file(&path).ok();
        match loaded {
            Ok(k) => {
                use russh::keys::ssh_key::Algorithm;
                assert!(
                    matches!(k.algorithm(), Algorithm::Rsa { .. }),
                    "应为 RSA 密钥,实际: {:?}",
                    k.algorithm()
                );
            }
            Err(russh::keys::Error::UnsupportedKeyType { key_type_string, .. }) => {
                panic!(
                    "RSA feature 未启用(UnsupportedKeyType: {}),检查 Cargo.toml 的 russh features 必须含 \"rsa\"",
                    key_type_string
                );
            }
            Err(e) => panic!("测试 RSA 私钥应可加载,实际错误: {:?}", e),
        }
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
            tags: Vec::new(),
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

    // ===== 第三十四批 L2:环境体检解析纯函数 =====

    #[test]
    fn test_parse_docker_size_units() {
        assert_eq!(parse_docker_size("0B"), Some(0));
        assert_eq!(parse_docker_size("260.8MB"), Some(260_800_000));
        assert_eq!(parse_docker_size("260.8MB (100%)"), Some(260_800_000));
        assert_eq!(parse_docker_size("3.655MB"), Some(3_655_000));
        assert_eq!(parse_docker_size("12kB"), Some(12_000));
        assert_eq!(parse_docker_size("1.5GiB"), Some(1_610_612_736));
        assert_eq!(parse_docker_size("n/a"), None);
    }

    #[test]
    fn test_parse_docker_df_golden() {
        // golden:本机 `docker system df --format '{{json .}}'` 实测抓样
        // (空镜像 + 遗留卷 + 构建缓存;Reclaimable 带占比后缀)
        let out = concat!(
            "{\"Active\":\"0\",\"Reclaimable\":\"0B\",\"Size\":\"0B\",\"TotalCount\":\"0\",\"Type\":\"Images\"}\n",
            "{\"Active\":\"0\",\"Reclaimable\":\"0B\",\"Size\":\"0B\",\"TotalCount\":\"0\",\"Type\":\"Containers\"}\n",
            "{\"Active\":\"0\",\"Reclaimable\":\"260.8MB (100%)\",\"Size\":\"260.8MB\",\"TotalCount\":\"9\",\"Type\":\"Local Volumes\"}\n",
            "{\"Active\":\"0\",\"Reclaimable\":\"3.655MB\",\"Size\":\"3.655MB\",\"TotalCount\":\"5\",\"Type\":\"Build Cache\"}\n"
        );
        let df = parse_docker_df(out).expect("四行都应解析");
        assert_eq!(df.volumes_reclaimable, 260_800_000);
        assert_eq!(df.build_cache_reclaimable, 3_655_000);
        assert_eq!(df.images_reclaimable, 0);
        assert_eq!(df.total_reclaimable, 264_455_000);
        assert_eq!(df.total_size, 264_455_000);
        assert!(parse_docker_df("no docker here").is_none());
        assert!(parse_docker_df("").is_none());
    }

    #[test]
    fn test_parse_first_u64() {
        assert_eq!(parse_first_u64("21861888894\t\n"), Some(21_861_888_894));
        assert_eq!(parse_first_u64(" 42 total"), Some(42));
        assert_eq!(parse_first_u64(""), None);
        assert_eq!(parse_first_u64("du: cannot access '/x'"), None);
    }

    // ===== 第三十四批 S3:项目迁移「目标链」真机样板 =====

    /// 覆盖迁移链里**卷 roundtrip 未覆盖**的一段:源/目标目录的三件套经 SFTP
    /// 搬运后,①归档字节级一致;②目标侧 compose 可被**服务端权威解析**
    /// (`docker compose config --format json`,与第三十四批 P3 同一命令)且
    /// `.env` 插值随行;③目标侧按该 compose 能真正 `up` 起容器(迁移的
    /// 「目标启动」步骤)。
    ///
    /// 需要服务器可执行 docker 且能拉取 busybox(或已存在 busybox)。
    /// 运行:`DD_SSH_TEST_HOST=... cargo test migrate_project_target -- --ignored`
    #[tokio::test]
    #[ignore = "需要真实 SSH 服务器 + docker(见函数注释的运行方式)"]
    async fn test_migrate_project_target_chain_real() {
        let cfg = test_cfg_from_env()
            .expect("请设置 DD_SSH_TEST_HOST / DD_SSH_TEST_USER / (DD_SSH_TEST_PASSWORD | DD_SSH_TEST_KEY)");
        let pw = std::env::var("DD_SSH_TEST_PASSWORD").ok();
        let mut client = SshClient::connect(&cfg, pw.as_deref(), None, Arc::default())
            .await
            .expect("connect 失败");

        let root = "/tmp/dd-mig-target-test";
        let src = format!("{root}/src");
        let dst = format!("{root}/dst");

        // 本地准备:compose + .env(插值占位)+ 模拟归档(含二进制字节)
        let local_tmp = std::env::temp_dir().join("dd-mig-target-test");
        std::fs::create_dir_all(&local_tmp).expect("本地临时目录创建失败");
        let compose_local = local_tmp.join("docker-compose.yml");
        let compose_text =
            "services:\n  app:\n    image: busybox:${BTAG}\n    command: [\"sh\", \"-c\", \"sleep 30\"]\n";
        std::fs::write(&compose_local, compose_text).expect("写本地 compose 失败");
        let env_local = local_tmp.join("env");
        std::fs::write(&env_local, "BTAG=latest\n").expect("写本地 .env 失败");
        let archive_local = local_tmp.join("2026-01-01.tar.gz");
        let archive_bytes: Vec<u8> = b"dd-migrate-archive-bytes-\x00\x01\x02\n".to_vec();
        std::fs::write(&archive_local, &archive_bytes).expect("写本地归档失败");

        // busybox 就绪(与卷 roundtrip 同款:有则用,无则拉)
        let (code, _) = exec_collect(&mut client, "docker image inspect busybox:latest")
            .await
            .expect("inspect 失败");
        if code != 0 {
            let (code, out) = exec_collect(&mut client, "docker pull busybox:latest")
                .await
                .expect("pull 失败");
            assert_eq!(code, 0, "无法获取 busybox(需服务器出网或预置): {out}");
        }

        // 准备远端目录(源/目标)
        let (code, _) = exec_collect(
            &mut client,
            &format!("rm -rf {root} && mkdir -p {src} {dst}"),
        )
        .await
        .expect("准备远端目录失败");
        assert_eq!(code, 0);

        // 搬运 1:源侧三件套经 SFTP 上传(迁移「compose 三件套 + 归档」段同款通道)
        let nop = |_: u64, _: u64| {};
        client
            .sftp_upload(&compose_local, &src, "docker-compose.yml", false, &nop)
            .await
            .expect("上传 compose 失败");
        client
            .sftp_upload(&env_local, &src, ".env", false, &nop)
            .await
            .expect("上传 .env 失败");
        client
            .sftp_upload(&archive_local, &src, "2026-01-01.tar.gz", false, &nop)
            .await
            .expect("上传归档失败");

        // 解析 1(源侧):服务端权威解析应把 ${BTAG} 插值为 latest
        let cfg_cmd = crate::commands::compose_config_json_cmd(&src, &format!("{src}/docker-compose.yml"), &[]);
        let (code, out) = exec_collect(&mut client, &cfg_cmd).await.expect("config 执行失败");
        assert_eq!(code, 0, "源侧 compose config 失败: {out}");
        let resolved = crate::stack::parse_compose_config_json(&out).expect("解析 config JSON 失败");
        let app = resolved
            .iter()
            .find(|s| s.service == "app")
            .expect("解析结果缺少 app 服务");
        assert_eq!(app.image.as_deref(), Some("busybox:latest"), "服务端插值结果应为 busybox:latest");

        // 搬运 2:归档下载 → 字节比对 → 上传目标(迁移的「搬过去」语义)
        let archive_back = local_tmp.join("2026-01-01-back.tar.gz");
        client
            .sftp_download(&format!("{src}/2026-01-01.tar.gz"), &archive_back, &nop)
            .await
            .expect("下载归档失败");
        let back_bytes = std::fs::read(&archive_back).expect("读回下载文件失败");
        assert_eq!(back_bytes, archive_bytes, "归档经 SFTP 往返应逐字节一致");
        client
            .sftp_upload(&archive_back, &dst, "2026-01-01.tar.gz", false, &nop)
            .await
            .expect("上传归档到目标失败");
        client
            .sftp_upload(&compose_local, &dst, "docker-compose.yml", false, &nop)
            .await
            .expect("上传 compose 到目标失败");
        client
            .sftp_upload(&env_local, &dst, ".env", false, &nop)
            .await
            .expect("上传 .env 到目标失败");

        // 目标链:服务端解析 → up(与部署/迁移同款加固旗标)→ 自证运行
        let dst_cfg_cmd = crate::commands::compose_config_json_cmd(&dst, &format!("{dst}/docker-compose.yml"), &[]);
        let (code, out) = exec_collect(&mut client, &dst_cfg_cmd).await.expect("目标 config 失败");
        assert_eq!(code, 0, "目标侧 compose config 失败: {out}");
        let (code, out) = exec_collect(
            &mut client,
            &format!(
                "cd {dst} && docker compose -f {dst}/docker-compose.yml up -d --remove-orphans --pull never --no-build"
            ),
        )
        .await
        .expect("目标 up 执行失败");
        assert_eq!(code, 0, "目标侧 up 失败: {out}");
        let (code, out) = exec_collect(
            &mut client,
            &format!("docker compose -f {dst}/docker-compose.yml ps -q"),
        )
        .await
        .expect("目标 ps 失败");
        assert_eq!(code, 0, "目标 ps 失败: {out}");
        let cid = out.trim();
        assert!(!cid.is_empty(), "目标 side up 后应有容器");
        let (code, out) = exec_collect(
            &mut client,
            &format!("docker inspect --format '{{{{.State.Running}}}}' {cid}"),
        )
        .await
        .expect("inspect 失败");
        assert_eq!(code, 0, "inspect 失败: {out}");
        assert_eq!(out.trim(), "true", "迁移目标链应有容器在运行");

        // 清理:目标 down(含卷)+ 远端目录 + 本地临时文件
        let _ = exec_collect(
            &mut client,
            &format!("cd {dst} && docker compose -f {dst}/docker-compose.yml down --remove-orphans -v 2>/dev/null; rm -rf {root}; true"),
        )
        .await
        .ok();
        let _ = std::fs::remove_dir_all(&local_tmp);
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
            // 块级取消(C1;第二十八批):下载侧同款逐块检查(大卷包下载中可即时停)
            self.check_transfer_cancelled()?;
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
