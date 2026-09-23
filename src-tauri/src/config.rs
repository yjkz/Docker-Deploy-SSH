//! Configuration module: reads/writes `servers.json` / `projects.json` under
//! the application folder's `config/` subdirectory (portable layout).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthType {
    Key,
    Password,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    pub auth_type: AuthType,
    #[serde(default)]
    pub key_path: Option<String>,
    #[serde(default)]
    pub password_enc: Option<String>,
    /// DPAPI 加密后的私钥口令(base64 密文;仅加密私钥需要;旧版配置无此字段,
    /// serde default 兼容)。导出/导入时按明文随加密 blob 携带(见 config_io)。
    #[serde(default)]
    pub key_pass_enc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    pub auth: AuthConfig,
    #[serde(default)]
    pub remote_dir: String,
    /// 首次连接(TOFU)时记录的服务器主机密钥 OpenSSH 风格指纹
    /// (`SHA256:` + base64(nopad)(SHA-256(公钥 SSH blob)));None = 尚未信任。
    /// 后续连接指纹不一致即拒绝(防中间人;重装/换 IP 后可在管理页重新信任)。
    /// 旧版配置无此字段,serde default 兼容。
    #[serde(default)]
    pub host_key_sha256: Option<String>,
    /// 服务器标签(第二十七批 B3;空列表 = 未分组)。
    ///
    /// 用途:03 页列表按**首标签**分节、部署页等服务器下拉用 `<optgroup>`
    /// 分组。旧配置无此字段 → serde default 兼容。写路径一律经
    /// [`save_server_entry`](crate::commands::save_server_entry)(内部调
    /// [`normalize_tags`] 归一:trim、去空、去重保序、cap 8 条 / 每条 24 字符)。
    #[serde(default)]
    pub tags: Vec<String>,
}

/// 标签上限(条数 / 单条字符数;第二十七批 B3)。超限截断而非报错 ——
/// 标签是展示性元数据,不值得为超长阻断保存。
pub const MAX_SERVER_TAGS: usize = 8;
pub const MAX_SERVER_TAG_CHARS: usize = 24;

/// 归一化服务器标签(纯函数):trim → 去空 → 去重保序 → cap 条数 / 截断长度。
/// 截断按**字符数**(非字节),保证不会切断多字节字符。
pub fn normalize_tags(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in raw {
        let s = t.trim();
        if s.is_empty() {
            continue;
        }
        let s: String = s.chars().take(MAX_SERVER_TAG_CHARS).collect();
        if !out.iter().any(|x| x == &s) {
            out.push(s);
        }
        if out.len() >= MAX_SERVER_TAGS {
            break;
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMapping {
    pub local: String,
    pub remote: String,
    pub is_dir: bool,
}

/// 镜像传输方式(整栈部署按 compose 服务逐个分类,serde 序列化为 "Local"/"Pull")。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferMode {
    /// 本地打包上传(服务在本地构建或本地已有镜像)
    Local,
    /// 服务器自行从镜像仓库拉取
    Pull,
}

/// 单个 compose 服务的传输方式覆盖(用户在分类表中保存的默认分类)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceOverride {
    pub service: String,
    pub mode: TransferMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub id: String,
    pub name: String,
    pub image_filter: String,
    pub compose_file: String,
    pub file_mappings: Vec<FileMapping>,
    /// 整栈部署:各服务的传输方式默认分类;旧版配置无此字段,反序列化为空 Vec(旧行为不变)
    #[serde(default)]
    pub service_overrides: Vec<ServiceOverride>,
    /// 部署后健康检查等待秒数(0=关闭);>0 时 up 后轮询服务状态直至全部就绪
    #[serde(default)]
    pub health_wait_secs: u32,
    /// 部署前钩子命令(远端执行,可选;失败中止部署)
    #[serde(default)]
    pub pre_deploy_cmd: Option<String>,
    /// 部署后钩子命令(远端执行,可选)
    #[serde(default)]
    pub post_deploy_cmd: Option<String>,
    /// 部署完成通知的 webhook URL(可选)
    #[serde(default)]
    pub notify_webhook: Option<String>,
    /// 导入来源的 compose 绝对路径(手工项目为 None);用于「从源更新」比对
    #[serde(default)]
    pub source_compose_path: Option<String>,
    /// 导入时 compose + .env + override 内容的 sha256(十六进制);
    /// 与源文件当前哈希不同即视为「源已变更」。旧配置无此字段 → None(不参与比对)
    #[serde(default)]
    pub source_hash: Option<String>,
    /// **项目级**远程部署目录(第四批):留空/None = 沿用所属服务器的 `remote_dir`。
    ///
    /// 为什么需要:`ServerConfig.remote_dir` 是**服务器级**的单一目录,同一台服务器
    /// 上的多个项目会共用同一部署目录与 `docker-compose.yml`;项目各自有部署目录
    /// 后,切换项目不必再改服务器配置。旧配置无此字段 → None,行为与历史完全一致。
    #[serde(default)]
    pub remote_dir: Option<String>,
    /// 该项目常用的服务器 id(第四批,可选):部署页选中项目时据此自动带出服务器。
    /// 仅作便利,不做强制校验(临时跨服务器部署仍然允许)。旧配置无此字段 → None。
    #[serde(default)]
    pub default_server_id: Option<String>,
    /// **发布归档保留数量**(第五批,可选):部署成功后按此清理旧 releases 目录,
    /// 只保留最新的 N 个;`None` = 用 [`DEFAULT_RELEASE_KEEP`](5 个),即旧行为不变。
    /// 取值上限见 [`RELEASE_KEEP_MAX`];0 = 部署后清空历史归档(仅留本次)。
    #[serde(default)]
    pub release_keep: Option<u32>,
}

/// 发布归档默认保留数量(`release_keep` 未配置时用它,等于历史行为的「最新 5 个」)。
pub const DEFAULT_RELEASE_KEEP: u32 = 5;
/// 发布归档保留数量上限(表单校验与后端兜底共用)。
pub const RELEASE_KEEP_MAX: u32 = 50;

/// 解析某项目实际使用的归档保留数量:配置值(夹到 `0..=MAX`)→ 默认值。
pub fn release_keep_of(project: &ProjectConfig) -> u32 {
    project
        .release_keep
        .map(|n| n.min(RELEASE_KEEP_MAX))
        .unwrap_or(DEFAULT_RELEASE_KEEP)
}

// ===== 通知中心配置(UPGRADE-PLAN 阶段二,serde default 兼容旧配置文件)=====

/// 桌面系统通知配置。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DesktopNotify {
    /// 是否启用桌面通知
    #[serde(default)]
    pub enabled: bool,
}

/// SMTP 端口默认值:465(SSL 隐式 TLS)。pub(crate):config_io 导出镜像结构复用。
pub(crate) fn default_smtp_port() -> u16 {
    465
}

/// SMTP 加密方式默认值:ssl。pub(crate):config_io 导出镜像结构复用。
pub(crate) fn default_security() -> String {
    "ssl".to_string()
}

/// 事件订阅开关默认值:true。
fn default_true() -> bool {
    true
}

/// 终端日志保留天数默认值:30(第二十三批;缺字段的旧 settings.json 按此补齐)。
fn default_term_log_keep_days() -> u32 {
    30
}

/// 资源阈值默认值:90% 使用率(第二十四批;缺字段的旧 settings.json 按此补齐)。
fn default_alert_percent() -> u32 {
    90
}

/// 邮件(SMTP)通知配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailNotify {
    /// 是否启用邮件通知
    #[serde(default)]
    pub enabled: bool,
    /// SMTP 服务器主机名(如 smtp.example.com)
    #[serde(default)]
    pub smtp_host: String,
    /// SMTP 端口,默认 465(SSL 隐式 TLS;starttls 常用 587)
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    /// SMTP 用户名(通常为发件邮箱)
    #[serde(default)]
    pub username: String,
    /// DPAPI 加密后的 SMTP 密码(base64 密文,加密绑定当前 Windows 用户)
    #[serde(default)]
    pub password_enc: Option<String>,
    /// 加密方式:"ssl"(隐式 TLS,465)| "starttls"(587)| "none"(明文);
    /// 自由字符串存储,读取时经 [`normalize_security`] 归一化(非法值按 ssl 兜底)
    #[serde(default = "default_security")]
    pub security: String,
    /// 发件人(如 "DockerDeploy <bot@example.com>" 或 "bot@example.com")
    #[serde(default)]
    pub from: String,
    /// 收件人列表(逐个 To)
    #[serde(default)]
    pub to: Vec<String>,
}

impl Default for EmailNotify {
    fn default() -> Self {
        Self {
            enabled: false,
            smtp_host: String::new(),
            port: default_smtp_port(),
            username: String::new(),
            password_enc: None,
            security: default_security(),
            from: String::new(),
            to: Vec::new(),
        }
    }
}

/// 通知事件订阅开关(部署收尾事件 → 是否通知)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyEvents {
    /// 部署成功时通知(默认开)
    #[serde(default = "default_true")]
    pub on_success: bool,
    /// 部署失败时通知(默认开)
    #[serde(default = "default_true")]
    pub on_failure: bool,
    /// 部署取消时通知(默认关)
    #[serde(default)]
    pub on_cancel: bool,
    /// 服务器探活状态翻转时通知(第十七批;默认关)—— 仅在「在线→离线 /
    /// 离线→恢复」翻转时发,不做每轮轰炸
    #[serde(default)]
    pub on_probe: bool,
    /// 服务器资源超阈值时通知(第二十四批;默认关)—— 磁盘/内存/CPU 连续
    /// 2 轮超阈才发(防瞬态尖峰),恢复后单次回落到阈值下即发「已恢复」;
    /// 阈值与采样间隔在设置中心配(`alert_*` 系列字段)
    #[serde(default)]
    pub on_alert: bool,
    /// 部署日报(第二十八批 B2;默认关)—— 每天在设置中心指定的整点后,
    /// 聚合当天部署记录为一条摘要发一次(当天无部署不发空日报)
    #[serde(default)]
    pub on_digest: bool,
}

impl Default for NotifyEvents {
    fn default() -> Self {
        Self {
            on_success: true,
            on_failure: true,
            on_cancel: false,
            on_probe: false,
            on_alert: false,
            on_digest: false,
        }
    }
}

/// 通知中心总配置(`AppConfig.notify`,独立持久化于 `config/notify.json`)。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NotifyConfig {
    /// 桌面系统通知
    #[serde(default)]
    pub desktop: DesktopNotify,
    /// 邮件(SMTP)通知
    #[serde(default)]
    pub email: EmailNotify,
    /// 事件订阅开关
    #[serde(default)]
    pub events: NotifyEvents,
    /// 成功通知的最小部署耗时(秒,第二十批阶段五;0 = 恒通知)——
    /// 成功且耗时 < 阈值时跳过通知(夜间批量不再逐台轰炸);失败/取消
    /// 恒通知(它们需要人看,阈值只作用于 success)。上限夹到 3600。
    #[serde(default)]
    pub min_duration_secs: u32,
}

/// 归一化 `security` 字段(纯函数,便于单测):trim + 转小写后仅接受
/// "ssl" / "starttls" / "none";空值/非法值一律按 "ssl" 处理(宽松兜底,
/// 保证手改配置文件引入的非法值不至于让邮件渠道整体失效)。
pub fn normalize_security(security: &str) -> &'static str {
    match security.trim().to_ascii_lowercase().as_str() {
        "starttls" => "starttls",
        "none" => "none",
        _ => "ssl",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub servers: Vec<ServerConfig>,
    pub projects: Vec<ProjectConfig>,
    /// 通知中心配置(serde default 兼容旧调用方;独立持久化到 config/notify.json)
    #[serde(default)]
    pub notify: NotifyConfig,
}

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config io error: {}", e),
            ConfigError::Json(e) => write!(f, "config json error: {}", e),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            ConfigError::Json(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        ConfigError::Json(e)
    }
}

/// Returns the portable application folder that hosts the `config/` (and
/// `logs/`) subdirectories: the directory of the running executable, or the
/// path given by `DD_CONFIG_DIR` (test injection / portable override).
///
/// 第二十批 P2 修复:环境变量分支仅在 debug 断言(开发/测试构建)生效 ——
/// release 构建中任何外部注入的 `DD_CONFIG_DIR` 都不再重定向配置根(此前
/// 无守卫,启动脚本/快捷方式可把 `config/` 指向攻击者可控目录,伪造
/// `host_key_sha256` 跳过 TOFU 告警;更现实的危害是用户环境残留该变量导致
/// 「配置神秘丢失」假象)。测试依赖该注入(`cargo test` 默认 dev profile,
/// `cfg(test)` 与 `debug_assertions` 均为真,32 处使用点不受影响;CI 跑的
/// 也是默认 dev profile,见 .github/workflows/ci.yml)。
pub fn app_dir() -> PathBuf {
    // #[cfg] 是编译期裁剪:release 构建整条环境变量分支被剔除
    #[cfg(debug_assertions)]
    if let Ok(dir) = std::env::var("DD_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let mut exe = std::env::current_exe().expect("failed to locate current executable");
    exe.pop();
    exe
}

/// Returns the directory that holds `servers.json` / `projects.json`.
///
/// When the `DD_CONFIG_DIR` environment variable is set (test injection /
/// portable override; debug 构建专属,见 [`app_dir`]) it points at the
/// application folder and `config/` is appended; otherwise the `config/`
/// subdirectory next to the running executable is used.
pub fn config_dir() -> PathBuf {
    app_dir().join("config")
}

/// Loads the whole application config.
///
/// `servers.json` and `projects.json` are independent files; a missing file
/// (first run) yields an empty list instead of an error. 通知配置独立存放于
/// `notify.json`(缺失/损坏时回退默认值,不拖垮整个应用配置的加载)。
pub fn load_config() -> Result<AppConfig> {
    let dir = config_dir();
    Ok(AppConfig {
        servers: load_json_list(&dir.join("servers.json"))?,
        projects: load_json_list(&dir.join("projects.json"))?,
        notify: load_notify_config(&dir.join("notify.json")),
    })
}

/// notify.json「读取异常」标志(进程级):`load_notify_config` 读到存在但
/// 损坏/不可读的 notify.json 时置位(缺失不算 —— 首次运行属正常情况)。
///
/// 置位期间 [`save_config`] 跳过 notify.json 写回:此时内存中的 notify 是
/// 回退的默认值(原文件里的 DPAPI 密文等已不可得),照常写回会用默认值
/// 覆盖原文件、造成不可恢复的配置丢失;用户在通知中心显式保存配置(视为
/// 已知晓并重填)时经 [`clear_notify_unhealthy`] 清除,重启应用亦可复位。
static NOTIFY_UNHEALTHY: AtomicBool = AtomicBool::new(false);

/// 清除 notify.json 读取异常标志(通知中心显式保存配置前调用,
/// 让本次保存能把新配置写回 notify.json)。
pub(crate) fn clear_notify_unhealthy() {
    NOTIFY_UNHEALTHY.store(false, Ordering::Release);
}

/// 读取通知配置(独立文件 `config/notify.json`)。
///
/// 与 servers/projects 不同:文件缺失时返回默认值,读取/解析失败时也告警后
/// 返回默认值而不是 `Err` —— 通知是尽力而为的旁路功能,损坏的通知配置不应
/// 导致部署页/服务器页(全部依赖 `load_config`)整体不可用;保底行为 = 不发通知。
/// 读取/解析失败(非缺失)同时置位 [`NOTIFY_UNHEALTHY`],见其文档。
fn load_notify_config(path: &Path) -> NotifyConfig {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(cfg) => cfg,
            Err(e) => {
                log::warn!(
                    "通知配置解析失败,已回退为默认配置 ({}): {}",
                    path.display(),
                    e
                );
                // 置位读取异常标志:防止后续 save_config 用回退的默认值覆盖原文件
                NOTIFY_UNHEALTHY.store(true, Ordering::Release);
                NotifyConfig::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => NotifyConfig::default(),
        Err(e) => {
            log::warn!(
                "通知配置读取失败,已回退为默认配置 ({}): {}",
                path.display(),
                e
            );
            // 置位读取异常标志:防止后续 save_config 用回退的默认值覆盖原文件
            NOTIFY_UNHEALTHY.store(true, Ordering::Release);
            NotifyConfig::default()
        }
    }
}

/// Saves the whole application config.
///
/// Each file is written to a `.tmp` sibling first and then atomically renamed
/// over the destination, so a crash mid-write never leaves truncated JSON.
/// 通知配置(`notify.json`)与 servers/projects 一并原子写入:前端「读全量 →
/// 改 servers/projects → 全量写回」的路径会原样带回 notify 字段,不会被丢失;
/// 例外:notify.json 读取异常(损坏/不可读,`NOTIFY_UNHEALTHY` 置位)期间
/// 跳过 notify.json 写回,避免用回退的默认值覆盖原文件(丢密文),见
/// [`NOTIFY_UNHEALTHY`] 文档。
///
/// **配置版本历史(第二十二批)**:写盘前先把**当前磁盘上的三件套**快照到
/// `config/.history/<yyyyMMdd-HHMMSS>/`(folder-per-event,三文件同快照保持
/// 同一时刻一致性;与最新快照全等时跳过 —— TOFU 指纹记录等高频写点不刷屏)。
/// 快照失败仅 `log::warn`(**绝不阻断保存主流程** —— 版本历史是兜底能力,
/// 其故障不该影响配置写入)。cap [`HISTORY_KEEP`] 份裁最旧。
/// 不变量:「先快照、后覆盖」,快照内容永远至少包含上一个已落盘版本。
pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    snapshot_config(&dir);
    write_json_atomic(&dir.join("servers.json"), &cfg.servers)?;
    write_json_atomic(&dir.join("projects.json"), &cfg.projects)?;
    if NOTIFY_UNHEALTHY.load(Ordering::Acquire) {
        log::warn!("notify.json 读取异常,本次保存跳过通知配置写回,请重启应用修复后重试");
    } else {
        write_json_atomic(&dir.join("notify.json"), &cfg.notify)?;
    }
    Ok(())
}

// ===== 配置版本历史(第二十二批)=====

/// 快照保留份数(超限按目录名 = 时间戳排序裁最旧)。
pub const HISTORY_KEEP: usize = 20;

/// 快照三件套文件名(与主配置同名;恢复时原样拷回)。
const SNAPSHOT_FILES: [&str; 3] = ["servers.json", "projects.json", "notify.json"];

/// 快照根目录 `config/.history/`。
pub(crate) fn history_dir() -> PathBuf {
    config_dir().join(".history")
}

/// 快照当前配置三件套(尽力而为;失败仅告警)。
///
/// - 与**最新快照全等**时跳过(防高频写点 —— 如 TOFU 指纹每次连接写入 ——
///   生成大量同内容快照);
/// - 目录名 `<yyyyMMdd-HHMMSS>`,同秒重复调用时以 `-N` 后缀避让;
/// - 写后按目录名排序裁最旧,保留 [`HISTORY_KEEP`] 份。
pub(crate) fn snapshot_config(dir: &Path) {
    // 1) 收集当前三件套内容;全部不存在(首次运行)时无可快照
    let mut contents: Vec<(&str, Vec<u8>)> = Vec::new();
    for name in SNAPSHOT_FILES {
        match std::fs::read(dir.join(name)) {
            Ok(bytes) => contents.push((name, bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                log::warn!("配置快照读取 {} 失败,跳过本次快照: {}", name, e);
                return;
            }
        }
    }
    if contents.is_empty() {
        return;
    }

    // 2) 与最新快照全等 → 跳过(同内容不重复留档)
    let root = dir.join(".history");
    if let Some(latest) = list_snapshot_dirs(&root).last() {
        let same = contents.iter().all(|(name, bytes)| {
            std::fs::read(latest.join(name)).map(|b| b == *bytes).unwrap_or(false)
        });
        if same {
            return;
        }
    }

    // 3) 建目录写文件(同秒冲突加后缀)
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut target = root.join(&ts);
    let mut seq = 1;
    while target.exists() {
        seq += 1;
        target = root.join(format!("{}-{}", ts, seq));
    }
    if let Err(e) = std::fs::create_dir_all(&target) {
        log::warn!("配置快照建目录失败 ({}): {}", target.display(), e);
        return;
    }
    for (name, bytes) in &contents {
        if let Err(e) = std::fs::write(target.join(name), bytes) {
            log::warn!("配置快照写入 {} 失败: {}", name, e);
            let _ = std::fs::remove_dir_all(&target); // 半份快照不留
            return;
        }
    }

    // 4) 裁剪最旧
    let mut dirs = list_snapshot_dirs(&root);
    if dirs.len() > HISTORY_KEEP {
        dirs.sort();
        let cut = dirs.len() - HISTORY_KEEP;
        for old in dirs.drain(0..cut) {
            if let Err(e) = std::fs::remove_dir_all(&old) {
                log::warn!("清理旧快照失败 ({}): {}", old.display(), e);
            }
        }
    }
}

/// 列出快照目录(只含目录项,按目录名排序 = 时间序;读失败返回空表)。
pub(crate) fn list_snapshot_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// 快照 id 合法性(纯函数,便于单测):`yyyyMMdd-HHMMSS` 或带 `-N` 后缀序。
/// 恢复命令用它防路径穿越(不接受 `/`、`\`、`.` 等字符)。
pub fn is_valid_snapshot_id(id: &str) -> bool {
    let s = id.trim();
    if s.is_empty() || s.len() > 24 || s.contains('/') || s.contains('\\') || s.contains('.') {
        return false;
    }
    let mut parts = s.splitn(3, '-');
    let date = parts.next().unwrap_or("");
    let time = parts.next().unwrap_or("");
    let tail = parts.next();
    let digit_ok = |x: &str, n: usize| x.len() == n && x.chars().all(|c| c.is_ascii_digit());
    if !digit_ok(date, 8) || !digit_ok(time, 6) {
        return false;
    }
    match tail {
        None => true,
        Some(t) => !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()),
    }
}

// ===== 配置读改写收口(第二十批 P2-4)=====

/// 进程级配置写互斥:所有「load → 改 → save」三步必须整体持锁,否则两个
/// 并发命令(如服务器编辑 + 通知中心保存)后写覆盖先写,合法地丢掉先到的
/// 修改(与 v6.1.1 哨兵事故同为「合法写坏数据」形态,UNHEALTHY 机制不覆盖)。
static CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 断点表(resume-deploy.json)的 RMW 互斥:批量 + 单发并发部署时
/// save_checkpoint/remove_checkpoint 不互踩(与 CONFIG_LOCK 分离 —— 断点
/// 旁路数据与主配置三件互不相干,合用一把锁会让部署写断点阻塞配置保存)。
static RESUME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 在同一把进程锁内执行「load → 改 → save」(`F` 负责改,可返回任意值)。
///
/// **锁保「读-改-写」原子性**:闭包拿到的必是持锁瞬间磁盘上的最新配置,
/// 闭包返回后立即落盘 —— 并发调用天然串行,后写不再覆盖先写。
/// 错误统一为 `String`(与全后端 `Result<T, String>` 命令口径一致;
/// 调用点自行格式化「读取/保存配置失败」上下文)。
///
/// 约束(违反会死锁/失去保护):
/// - `F` 内**不得**再调用任何会取 `CONFIG_LOCK` 的函数(`update_config`
///   不可嵌套);
/// - `F` 内只做内存修改,不要做耗时 IO(锁窗口内串行);
/// - `F` 返回 `Err` 时**不落盘**(本次修改整体放弃,磁盘保持闭包前状态)。
pub fn update_config<T, F>(mutate: F) -> std::result::Result<T, String>
where
    F: FnOnce(&mut AppConfig) -> std::result::Result<T, String>,
{
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let out = mutate(&mut cfg)?;
    save_config(&cfg).map_err(|e| format!("保存配置失败: {}", e))?;
    Ok(out)
}

fn load_json_list<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// `.tmp` 临时文件写完并 sync 后 rename 原子覆盖目标(history.rs 复用同一模式)。
pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.flush()?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ===== 应用设置(UPGRADE-PLAN 阶段四「桌面体验」,独立持久化于 config/settings.json)=====

/// 应用级设置(`settings.json`,serde camelCase 对齐前端 JS 字段)。
/// Default 直接 derive:bool 默认 false(关闭到托盘关,旧行为不变)、
/// String 默认空串(代理为空 = 直连),与手写默认值语义一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    /// 关闭主窗口时隐藏到托盘(false = 关闭即退出;默认关)
    pub close_to_tray: bool,
    /// 检查更新使用的代理地址(http:// 或 socks5:// 前缀;空串 = 直连)
    pub proxy: String,
    /// 启动时自动比对源 compose 并更新项目(第三批;缺省 true = 默认开启)
    pub auto_update_from_source: bool,
    /// 服务器定时探活间隔分钟(第十七批;0 = 关闭,默认关;>0 时每 N 分钟
    /// TCP 探活全部已配置服务器,状态翻转经通知中心分发)
    #[serde(default)]
    pub probe_interval_mins: u32,
    /// 启动时静默检查更新(第二十一批;缺省 true = 默认开启):启动后拉取一次
    /// GitHub releases/latest,有新版仅在 dock 版本号旁加徽点提示,**不弹窗**;
    /// 点徽点进设置中心查看详情。检查失败静默(不打扰)。
    #[serde(default = "default_true")]
    pub auto_check_update: bool,
    /// 终端会话日志保留天数(第二十三批,时间制;`0` = 永久保留,默认 30)。
    /// 清理时机:应用启动 + 每次终端会话创建前;判定时间戳取自文件名,
    /// 越界值在读侧夹取(见 `manage_exec::TERM_LOG_KEEP_DAYS_MAX`)。
    #[serde(default = "default_term_log_keep_days")]
    pub term_log_keep_days: u32,
    /// 资源阈值告警采样间隔分钟(第二十四批;`0` = 关闭,默认关;>0 时每 N
    /// 分钟逐台 SSH 采样磁盘/内存/CPU,超阈经通知中心分发;与探活任务独立)
    #[serde(default)]
    pub alert_interval_mins: u32,
    /// 磁盘使用率阈值(百分比,默认 90;`0` = 该项不告警)—— 根分区与
    /// Docker 数据盘取两者较差值判定
    #[serde(default = "default_alert_percent")]
    pub alert_disk_percent: u32,
    /// 内存使用率阈值(百分比,默认 90;`0` = 该项不告警)
    #[serde(default = "default_alert_percent")]
    pub alert_mem_percent: u32,
    /// CPU 占用阈值(百分比,默认 90;`0` = 该项不告警)
    #[serde(default = "default_alert_percent")]
    pub alert_cpu_percent: u32,
    /// 整栈部署**健康检查失败**时自动回滚到上一份归档(第二十五批;
    /// 缺省 **false** —— 自动回滚会改线上状态,必须用户显式开启,
    /// 同时保证升级不改变既有行为)。设计与边界见 `auto_rollback` 模块注释。
    #[serde(default)]
    pub auto_rollback_on_failure: bool,
    /// 自定义 compose 文件名(第二十六批;空列表 = 用内置四标准名)。
    /// 项目/栈扫描时按此匹配 —— 支持 `docker-compose.prod.yml` 类变体命名。
    /// **安全**:名字会拼进远端 find 命令,读取侧经
    /// `compose_scan::normalize_compose_names` 严格校验(仅 [A-Za-z0-9._-],
    /// 拒绝路径分隔符与 `..`),非法项丢弃、全非法回退默认。
    #[serde(default)]
    pub compose_file_names: Vec<String>,
    /// 扫描最大深度(第二十六批;0/缺省 = 4,夹取 1..=8)。
    #[serde(default)]
    pub compose_scan_max_depth: u32,
    /// 卷搬运(tar)所用镜像引用(第二十七批;空串 = 内置候选链
    /// busybox → alpine → ubuntu)。服务器有私有 registry / 只允许白名单镜像时,
    /// 可指定服务器上已存在的自备 tar 能力镜像。**读取侧经
    /// `migrate_project::normalize_tar_image` 严格校验**(镜像引用字符集,
    /// 拒绝 shell 元字符与 `-` 开头),非法值回退内置候选。
    #[serde(default)]
    pub tar_image: String,
    /// 开机自启(第二十九批 S1;默认关)。
    ///
    /// **注意真值来源**:实际生效状态存在 Windows 注册表
    /// (`HKCU\...\Run\DockerDeploySSH`),用户可能在任务管理器里禁用 ——
    /// 故 `app_settings_get` 返回时用 `autostart::with_actual_state` **覆盖**
    /// 本字段为注册表实况;**本字段只记录「上次意图」**,不作为判定依据。
    #[serde(default)]
    pub auto_start: bool,
    /// 部署日报发送时刻(第二十八批 B2;`None` = 关闭,默认关)。
    ///
    /// 到该整点后(含应用晚启动的补发)聚合当天 `DeployRecord` 为一条摘要,
    /// 经通知管道以 `kind = "digest"` 发出;发送标记存独立文件
    /// `config/digest-state.json`(**不放本结构** —— settings 表单保存会整量
    /// 覆盖未知字段,标记会被静默清空导致当天重发)。
    /// 越界值(>23)按关闭处理。
    #[serde(default)]
    pub digest_hour: Option<u32>,
    /// 宿主机(部署目录源)可写目录白名单(第三十四批(五);**空列表 = 不可写**,
    /// 维持第三十三批的只读口径)。
    ///
    /// 仅作用于文件管理「部署目录」源:写操作的目标路径在服务器上
    /// `readlink -f` canonicalize 后必须落在某条白名单前缀内(目录边界匹配,
    /// 见 [`path_within_any`]);容器 / 卷源不受影响。保存侧经
    /// [`normalize_host_write_paths`] 归一化(非法项丢弃)。
    #[serde(default)]
    pub host_write_paths: Vec<String>,
    /// 层级增量传输(第三十七批;**缺省 true = 默认开启**)。
    ///
    /// 开启后部署会先问服务器「你已有哪些层」(`docker image inspect` 的 diffID 集合),
    /// 把本地 `docker save` 包里这些层的 blob 丢掉再传 —— 只传增量;本地同时保留整份,
    /// 装载失败自动改用整包重传(0 期实测:两种 image store 都接受裁剪包,判据只能
    /// 用 diffID,失败判定必须 rc + 文本双判)。关闭 = 一律整包,与历史行为一致。
    #[serde(default = "default_true")]
    pub incremental_transfer: bool,
}

/// 「宿主机可写目录」白名单上限(条)。
pub const HOST_WRITE_PATHS_MAX: usize = 10;

/// 归一化「宿主机可写目录」白名单(纯函数,第三十四批(五)):去空白与尾斜杠;
/// 必须绝对路径、不含 `..` 段 / NUL / 换行、长度 ≤ 512;去重(保序);
/// cap [`HOST_WRITE_PATHS_MAX`] 条。非法条目**静默丢弃**(读侧只认这里的结果)。
pub fn normalize_host_write_paths(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in raw {
        let s = item.trim();
        if s.is_empty() || !s.starts_with('/') {
            continue;
        }
        if s.contains('\0') || s.contains('\n') || s.contains('\r') || s.len() > 512 {
            continue;
        }
        let trimmed = s.trim_end_matches('/');
        let normalized = if trimmed.is_empty() { "/" } else { trimmed };
        if normalized.split('/').any(|seg| seg == "..") {
            continue;
        }
        if !out.iter().any(|x| x == normalized) {
            out.push(normalized.to_string());
        }
        if out.len() >= HOST_WRITE_PATHS_MAX {
            break;
        }
    }
    out
}

/// 路径是否位于白名单前缀之内(纯函数;目录边界匹配:`/a` 不匹配 `/ab`;
/// `"/"` 前缀恒真)。两侧都应已归一化(去尾斜杠)。
pub fn path_within_any(path: &str, prefixes: &[String]) -> bool {
    for p in prefixes {
        if p == "/" {
            if path.starts_with('/') {
                return true;
            }
            continue;
        }
        if path == p || path.starts_with(&format!("{}/", p)) {
            return true;
        }
    }
    false
}

/// `AppSettings::default` 的手写实现:`auto_update_from_source` 缺省为 **true**
/// (启动自动比对是第三批的默认行为),其余字段沿用 `Default` 语义。
impl Default for AppSettings {
    fn default() -> Self {
        Self {
            close_to_tray: false,
            proxy: String::new(),
            auto_update_from_source: true,
            probe_interval_mins: 0,
            auto_check_update: true,
            term_log_keep_days: default_term_log_keep_days(),
            alert_interval_mins: 0,
            alert_disk_percent: default_alert_percent(),
            alert_mem_percent: default_alert_percent(),
            alert_cpu_percent: default_alert_percent(),
            auto_rollback_on_failure: false,
            compose_file_names: Vec::new(),
            compose_scan_max_depth: 0,
            tar_image: String::new(),
            digest_hour: None,
            auto_start: false,
            host_write_paths: Vec::new(),
            incremental_transfer: true,
        }
    }
}

/// 读取应用设置(独立文件 `config/settings.json`)。
///
/// 与 notify.json 同模式:文件缺失(首次运行)返回默认值,损坏/不可读时
/// 告警并回退默认值 —— 设置项不含密文等不可再生数据,回退默认值无不可恢复
/// 损失,故无需 notify 的 UNHEALTHY「禁止写回」保护机制。
pub fn load_app_settings() -> AppSettings {
    load_app_settings_from(&config_dir().join("settings.json"))
}

/// `load_app_settings` 的路径注入版本(单测用,生产路径经 [`config_dir`])。
fn load_app_settings_from(path: &Path) -> AppSettings {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(settings) => settings,
            Err(e) => {
                log::warn!(
                    "应用设置解析失败,已回退为默认设置 ({}): {}",
                    path.display(),
                    e
                );
                AppSettings::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AppSettings::default(),
        Err(e) => {
            log::warn!(
                "应用设置读取失败,已回退为默认设置 ({}): {}",
                path.display(),
                e
            );
            AppSettings::default()
        }
    }
}

/// 保存应用设置(`settings.json`;复用 [`write_json_atomic`] 原子写:
/// .tmp 写入 + rename 覆盖,崩溃不留半截 JSON)。
pub fn save_app_settings(settings: &AppSettings) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    write_json_atomic(&dir.join("settings.json"), settings)
}

// ===== 应用设置命令(设置中心前端契约,camelCase 序列化) =====

/// 读取应用设置(关闭到托盘 / 更新代理)。
#[tauri::command]
pub fn app_settings_get() -> AppSettings {
    let mut s = load_app_settings();
    // 开机自启第二十九批 S1:注册表是唯一真值 —— 用户可能在任务管理器里
    // 禁用或手工删掉注册表值,若照配置回显会与系统实际状态不符
    crate::autostart::with_actual_state(&mut s);
    s
}

/// 保存应用设置(关闭到托盘 / 更新代理 / 定时探活间隔;托盘拦截在关闭事件
/// 时现读文件,保存即生效)。探活间隔变更时同步(重)启/停后端探活任务。
#[tauri::command]
pub fn app_settings_set(app: tauri::AppHandle, settings: AppSettings) -> std::result::Result<(), String> {
    // 白名单归一化在落盘前收口(第三十四批(五);与 termLogKeepDays 读侧夹取同精神)
    let mut settings = settings;
    settings.host_write_paths = normalize_host_write_paths(&settings.host_write_paths);
    save_app_settings(&settings).map_err(|e| format!("保存设置失败: {}", e))?;
    crate::probe::sync_from_settings(&app);
    // 资源阈值告警(第二十四批):保存即按新设置启停(同探活口径)
    crate::probe::sync_alert_from_settings(&app);
    // 部署日报(第二十八批 B2):小时字段变更即时启停(同探活口径)
    crate::digest::sync_from_settings(&app);
    // 开机自启(第二十九批 S1):写/删注册表项。**失败要报错** —— 用户点了
    // 开关却什么都没发生是最糟的体验;此刻设置文件已保存,但自启没生效,
    // 必须让用户知道(前端 toast 原文)。
    crate::autostart::apply(settings.auto_start)?;
    Ok(())
}

/// 打开日志文件夹(设置中心「诊断」入口;日志由 tauri-plugin-log 写于
/// `app_dir()/logs` 的 app.log,按日期轮转)。目录尚不存在时先创建,
/// 保证首次运行(还没写过日志)也能打开。
#[tauri::command]
pub fn open_logs_dir() -> std::result::Result<(), String> {
    let dir = app_dir().join("logs");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败: {}", e))?;
    // Windows 为发版目标;cfg 兜底其余平台的惯用打开器,命令缺失时报错不 panic
    #[cfg(target_os = "windows")]
    let opener = "explorer";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let opener = "xdg-open";
    std::process::Command::new(opener)
        .arg(&dir)
        .spawn()
        .map_err(|e| format!("打开日志文件夹失败: {}", e))?;
    Ok(())
}

/// 把文本写入指定路径(第二十一批:批量报告导出落盘)。
///
/// 路径来自系统保存对话框(用户显式选定),但仍按不可信输入处理:
/// 仅写入**已存在目录**下的文件(父目录不存在直接报错,不代建)、拒绝空路径;
/// 写入用「临时文件 + rename」原子写,避免中断留半成品。上限 4MB(报告类
/// 文本远超不了,防误传大内容)。
#[tauri::command]
pub fn write_text_file(path: String, content: String) -> std::result::Result<(), String> {
    use std::io::Write as _;
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    if path.trim().is_empty() {
        return Err("保存路径为空".to_string());
    }
    if content.len() > MAX_BYTES {
        return Err("内容过大(超过 4MB),已拒绝写入".to_string());
    }
    let target = std::path::PathBuf::from(&path);
    let parent = target
        .parent()
        .ok_or_else(|| format!("保存路径无效: {}", path))?;
    if !parent.is_dir() {
        return Err(format!("保存目录不存在: {}", parent.display()));
    }
    // 临时文件名 = 原名 + .ddtmp 后缀(不用 with_extension:它会替换掉 .md)
    let tmp = {
        let mut t = target.clone();
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "report".to_string());
        t.set_file_name(format!("{}.ddtmp", name));
        t
    };
    {
        let mut f = std::fs::File::create(&tmp)
            .map_err(|e| format!("创建临时文件失败 ({}): {}", tmp.display(), e))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("写入失败 ({}): {}", tmp.display(), e))?;
        f.flush().map_err(|e| format!("刷新失败 ({}): {}", tmp.display(), e))?;
    }
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("保存文件失败 ({}): {}", target.display(), e)
    })?;
    Ok(())
}

// ===== 部署断点续传(UPGRADE-PLAN 阶段六,独立持久化于 config/resume-deploy.json)=====

/// 断点条目上限:超出后按 `ts` 从最旧开始裁剪(防止无限膨胀)。
/// 裁剪会连带清理被裁条目的本地临时 tar(见 [`trim_checkpoints`])。
pub(crate) const MAX_CHECKPOINTS: usize = 10;

/// 迁移断点的模式串(第二十六批「迁移断点续传」)。
///
/// **为什么复用同一张断点表**:迁移与部署的断点形态一致(阶段 + 产物),
/// 复用一个机制比发明第二套更不容易出错;隔离靠 `key` 前缀(见
/// [`migrate_checkpoint_key`])与 `mode` 值,`deploy_resume_status` 的
/// 「同服务器 + 同项目」查询按 `mode` 过滤,不会把迁移断点当部署断点展示。
pub(crate) const MODE_MIGRATE: &str = "migrate";

/// 迁移断点键:`migrate|<源服务器>|<目标服务器>|<项目>|<目标目录>`。
///
/// 与部署断点键(`server|project|mode`)刻意不同形 —— 迁移的「同一迁移」
/// 判定要含目标服务器与目标目录(改目标重迁是一次新迁移,不应复用旧断点);
/// 前缀 `migrate|` 也让两类断点在同一个文件里一眼可辨。
pub fn migrate_checkpoint_key(
    source_server_id: &str,
    target_server_id: &str,
    project_id: &str,
    target_dir: &str,
) -> String {
    format!(
        "migrate|{}|{}|{}|{}",
        source_server_id, target_server_id, project_id, target_dir
    )
}

/// 单条部署断点(`resume-deploy.json` 的值,键见 [`checkpoint_key`])。
///
/// 记录一次未完成部署的进度:每个步骤完成的收尾处落盘一次,
/// `step_next` = 下一个待执行的步骤号(失败/取消发生在该步骤,
/// 续传时从它开始重跑)。`artifacts` 为步骤产物(JSON,按 `mode` 结构不同,
/// 由 commands 层构造与解析:单镜像含镜像引用/本地与远端 tar 路径,
/// 整栈含服务分类/发布时间戳/镜像包文件名)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeCheckpoint {
    /// 断点键(`server_id|project_id|mode`,与文件中的键一致)
    pub key: String,
    /// 部署模式:`single`(单镜像)/ `stack`(整栈)
    pub mode: String,
    /// 下一个待执行的步骤号(1 起;失败步骤号 = step_next)
    pub step_next: u32,
    /// 最近一次落盘时间(本地时间 `%F %T`,用于展示与最旧裁剪)
    pub ts: String,
    pub server_id: String,
    pub project_id: String,
    /// 服务器名称快照(续传列表展示用;以当前配置为准校验存在性)
    pub server_name: String,
    /// 项目名称快照
    pub project_name: String,
    /// 步骤产物(结构随 mode 不同,见 [`ResumeCheckpoint`] 文档)
    pub artifacts: serde_json::Value,
}

/// 断点键:`server_id|project_id|mode`(同一键的新部署覆盖旧断点)。
pub fn checkpoint_key(server_id: &str, project_id: &str, mode: &str) -> String {
    format!("{}|{}|{}", server_id, project_id, mode)
}

/// `resume-deploy.json` 路径(config 目录下,与 servers.json 同级)。
fn resume_path() -> PathBuf {
    config_dir().join("resume-deploy.json")
}

/// 读取全部部署断点(键 → [`ResumeCheckpoint`])。
///
/// 容错口径与 history/notify 一致:文件缺失(从未有未完成部署)返回空表,
/// 损坏/不可读时告警后返回空表 —— 断点是尽力而为的旁路数据,不应让部署页
/// 或续传入口整体不可用;后续保存会以空表起步重建。
pub fn load_resume_map() -> HashMap<String, ResumeCheckpoint> {
    let path = resume_path();
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(map) => map,
            Err(e) => {
                log::warn!(
                    "部署断点文件损坏,按空断点表处理 ({}): {}",
                    path.display(),
                    e
                );
                HashMap::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(e) => {
            log::warn!("读取部署断点失败 ({}): {}", path.display(), e);
            HashMap::new()
        }
    }
}

/// 保存一条断点:读 → 插入(同键覆盖 = 新部署同键覆盖旧断点)→ 超上限裁剪
/// 最旧 → 原子写回(`.tmp` + rename,复用 [`write_json_atomic`])。
///
/// 被裁剪掉的条目**只从表里移除**;其断点期保留的本地临时 tar 由调用方
/// ([`crate::commands::checkpoint_save`]) 按 `commands` 层的产物口径清理
/// —— 本层不解析 `artifacts`(分层约定见 wiki/07 限制 40)。
///
/// v6.1.3(第二十批 P2-4):RMW 整体持 [`RESUME_LOCK`] —— 批量 + 单发
/// 并发部署时两个 save_checkpoint 不再互踩丢条目。
pub fn save_checkpoint(cp: &ResumeCheckpoint) -> Result<Vec<ResumeCheckpoint>> {
    let _guard = RESUME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut map = load_resume_map();
    map.insert(cp.key.clone(), cp.clone());
    let dropped = trim_checkpoints(&mut map);
    write_resume_map(&map)?;
    Ok(dropped)
}

/// 删除一条断点,返回被删的条目(供调用方清理其临时产物;无则 `None`)。
pub fn remove_checkpoint(key: &str) -> Result<Option<ResumeCheckpoint>> {
    let _guard = RESUME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut map = load_resume_map();
    let removed = map.remove(key);
    if removed.is_some() {
        write_resume_map(&map)?;
    }
    Ok(removed)
}

/// 断点条目裁剪:超过 [`MAX_CHECKPOINTS`] 条时按 `ts` 从最旧开始移除
/// (`ts` 为 `%F %T` 文本,字典序即时间序),**返回被移除的条目**供调用方
/// 清理其临时产物。
///
/// 分层说明:本函数只认断点表结构(不解析 `artifacts` —— 那是 commands 层
/// 的私有约定,见 wiki/07 限制 40),故把条目交回调用方,由
/// [`save_checkpoint`] 用 `commands` 层的 [`resume_local_tars`] 口径清理。
pub fn trim_checkpoints(map: &mut HashMap<String, ResumeCheckpoint>) -> Vec<ResumeCheckpoint> {
    if map.len() <= MAX_CHECKPOINTS {
        return Vec::new();
    }
    // 按 ts 升序取出最旧的若干条键,逐个移除
    let mut oldest: Vec<(String, String)> = map
        .iter()
        .map(|(k, v)| (v.ts.clone(), k.clone()))
        .collect();
    oldest.sort_by(|a, b| a.0.cmp(&b.0));
    let overflow = map.len() - MAX_CHECKPOINTS;
    let mut dropped = Vec::with_capacity(overflow);
    for (_, key) in oldest.into_iter().take(overflow) {
        if let Some(cp) = map.remove(&key) {
            dropped.push(cp);
        }
    }
    dropped
}

/// 原子写断点表(config 目录不存在则创建)。
fn write_resume_map(map: &HashMap<String, ResumeCheckpoint>) -> Result<()> {
    let path = resume_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_json_atomic(&path, map)
}

#[cfg(test)]
pub(crate) static TEST_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    // ===== 第三十四批(五):宿主机可写目录白名单(纯函数) =====

    #[test]
    fn test_normalize_host_write_paths() {
        let raw = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            normalize_host_write_paths(&raw(&[
                " /data/ ", "/srv/app/", "/data", "/etc", "relative", "/a/../b", ""
            ])),
            vec!["/data".to_string(), "/srv/app".to_string(), "/etc".to_string()],
            "去空白/尾斜杠、去重,相对路径与含 .. 的条目丢弃"
        );
        let many: Vec<String> = (0..15).map(|i| format!("/p{}", i)).collect();
        assert_eq!(normalize_host_write_paths(&many).len(), HOST_WRITE_PATHS_MAX);
        assert!(normalize_host_write_paths(&raw(&["  ", "x/y"])).is_empty());
    }

    #[test]
    fn test_path_within_any_boundary() {
        let ps = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let allowed = ps(&["/data", "/srv/app"]);
        assert!(path_within_any("/data", &allowed), "白名单目录本身可写");
        assert!(path_within_any("/data/x.txt", &allowed));
        assert!(path_within_any("/srv/app/sub/deep", &allowed));
        assert!(!path_within_any("/database", &allowed), "目录边界:不得吞同名前缀目录");
        assert!(!path_within_any("/etc/passwd", &allowed));
        assert!(!path_within_any("/Data/x", &allowed), "大小写敏感(远端为 Linux)");
        assert!(path_within_any("/anything", &ps(&["/"])));
    }

    #[test]
    fn test_host_write_paths_serde_default() {
        // 旧 settings.json(无该字段)反序列化 = 空列表(维持只读)
        let s: AppSettings = serde_json::from_str(
            r#"{"closeToTray":false,"proxy":"","autoUpdateFromSource":true}"#,
        )
        .unwrap();
        assert!(s.host_write_paths.is_empty());
    }

    #[test]
    fn test_roundtrip() {
        // DD_CONFIG_DIR 是进程级环境变量,与 commands 层的导入测试共用锁串行执行
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        let mut cfg = AppConfig::default();
        cfg.servers.push(ServerConfig {
            id: "s1".into(), name: "生产".into(), host: "1.2.3.4".into(), port: 22,
            username: "root".into(),
            auth: AuthConfig { auth_type: AuthType::Key, key_path: Some("C:/k".into()), password_enc: None, key_pass_enc: None },
            remote_dir: "/opt/app".into(),
            host_key_sha256: None,
            tags: Vec::new(),
        });
        cfg.projects.push(ProjectConfig {
            id: "p1".into(), name: "栈项目".into(), image_filter: String::new(),
            compose_file: "C:/app/config/stacks/x/docker-compose.yml".into(),
            file_mappings: vec![FileMapping { local: "a".into(), remote: "b".into(), is_dir: false }],
            service_overrides: vec![ServiceOverride { service: "web".into(), mode: TransferMode::Local }],
            health_wait_secs: 120,
            pre_deploy_cmd: Some("mysqldump -uroot -p'x' db > /opt/backup.sql".into()),
            post_deploy_cmd: Some("docker image prune -f".into()),
            notify_webhook: Some("https://example.com/hook".into()),
            source_compose_path: None,
            source_hash: None,
            remote_dir: None,
            default_server_id: None,
            release_keep: None,
        });
        // config_dir 依赖环境变量以便测试注入
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        save_config(&cfg).unwrap();
        let loaded = load_config().unwrap();
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].host, "1.2.3.4");
        assert_eq!(loaded.projects.len(), 1);
        // service_overrides 字段完整 roundtrip
        assert_eq!(
            loaded.projects[0].service_overrides,
            vec![ServiceOverride { service: "web".into(), mode: TransferMode::Local }]
        );
        // 新增字段完整 roundtrip
        assert_eq!(loaded.projects[0].health_wait_secs, 120);
        assert_eq!(
            loaded.projects[0].pre_deploy_cmd.as_deref(),
            Some("mysqldump -uroot -p'x' db > /opt/backup.sql")
        );
        assert_eq!(
            loaded.projects[0].post_deploy_cmd.as_deref(),
            Some("docker image prune -f")
        );
        assert_eq!(
            loaded.projects[0].notify_webhook.as_deref(),
            Some("https://example.com/hook")
        );
        assert!(dir.join("config/servers.json").exists());
        assert!(dir.join("config/projects.json").exists());
    }

    #[test]
    fn test_project_config_without_overrides_field() {
        // 旧版 projects.json 无 service_overrides 字段 → 反序列化为空 Vec(旧行为不变)
        let json = r#"{"id":"p1","name":"n","image_filter":"","compose_file":"","file_mappings":[]}"#;
        let p: ProjectConfig = serde_json::from_str(json).unwrap();
        assert!(p.service_overrides.is_empty());
    }

    #[test]
    fn test_project_config_new_fields_default() {
        // 旧版配置无新增字段 → serde default:0 / None,旧行为不变
        let json = r#"{"id":"p1","name":"n","image_filter":"","compose_file":"","file_mappings":[]}"#;
        let p: ProjectConfig = serde_json::from_str(json).unwrap();
        assert_eq!(p.health_wait_secs, 0);
        assert_eq!(p.pre_deploy_cmd, None);
        assert_eq!(p.post_deploy_cmd, None);
        assert_eq!(p.notify_webhook, None);
    }

    #[test]
    fn test_notify_config_roundtrip() {
        // 全量 notify 配置写入 notify.json → 读回逐字段相等
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        let mut cfg = AppConfig::default();
        cfg.notify.desktop.enabled = true;
        cfg.notify.email = EmailNotify {
            enabled: true,
            smtp_host: "smtp.example.com".into(),
            port: 587,
            username: "bot@example.com".into(),
            password_enc: Some("ZFa1Q2VpZg==".into()),
            security: "starttls".into(),
            from: "DockerDeploy <bot@example.com>".into(),
            to: vec!["a@example.com".into(), "b@example.com".into()],
        };
        cfg.notify.events = NotifyEvents {
            on_success: false,
            on_failure: true,
            on_cancel: true,
            on_probe: false,
            on_alert: true,
            on_digest: false,
        };
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        save_config(&cfg).unwrap();
        let loaded = load_config().unwrap();
        assert_eq!(loaded.notify, cfg.notify);
        assert!(dir.join("config/notify.json").exists());
    }

    #[test]
    fn test_notify_config_default_when_file_missing() {
        // notify.json 缺失(旧版本升级后首次启动)→ 全默认值,旧行为不变
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        let loaded = load_config().unwrap();
        assert_eq!(loaded.notify, NotifyConfig::default());
        assert!(!loaded.notify.desktop.enabled);
        assert!(!loaded.notify.email.enabled);
        assert_eq!(loaded.notify.email.port, 465);
        assert_eq!(loaded.notify.email.security, "ssl");
        assert_eq!(loaded.notify.email.password_enc, None);
        assert!(loaded.notify.events.on_success);
        assert!(loaded.notify.events.on_failure);
        assert!(!loaded.notify.events.on_cancel);
    }

    #[test]
    fn test_notify_config_partial_json_defaults() {
        // notify.json 只写部分字段 → 缺省字段按 serde default 补齐
        let json = r#"{"desktop":{"enabled":true},"email":{"enabled":true,"smtp_host":"s.com"}}"#;
        let cfg: NotifyConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.desktop.enabled);
        assert!(cfg.email.enabled);
        assert_eq!(cfg.email.smtp_host, "s.com");
        assert_eq!(cfg.email.port, 465);
        assert_eq!(cfg.email.security, "ssl");
        assert!(cfg.events.on_success && cfg.events.on_failure);
        assert!(!cfg.events.on_cancel);
    }

    #[test]
    fn test_notify_unhealthy_skips_write_until_explicit_save() {
        // notify.json 损坏(存在但解析失败)→ 读取回退默认值并置位异常标志;
        // 置位期间 save_config 跳过 notify.json 写回(不丢原文件),
        // 清除标志(用户显式保存通知配置)后恢复写回
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_notify_unhealthy(); // 清掉其他测试可能残留的标志
        let dir = std::env::temp_dir().join(format!("ddtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        let notify_path = dir.join("config/notify.json");
        std::fs::write(&notify_path, b"{oops").unwrap();

        // 损坏文件 → 回退默认值(标志随之置位)
        let loaded = load_config().unwrap();
        assert_eq!(loaded.notify, NotifyConfig::default());

        // 保存其他配置(servers/projects):notify.json 不被默认值覆盖
        let mut cfg = AppConfig::default();
        cfg.projects.push(ProjectConfig {
            id: "p1".into(), name: "n".into(), image_filter: String::new(),
            compose_file: "C:/app/docker-compose.yml".into(),
            file_mappings: Vec::new(),
            service_overrides: Vec::new(),
            health_wait_secs: 0,
            pre_deploy_cmd: None,
            post_deploy_cmd: None,
            notify_webhook: None,
            source_compose_path: None,
            source_hash: None,
            remote_dir: None,
            default_server_id: None,
            release_keep: None,
        });
        save_config(&cfg).unwrap();
        let raw = std::fs::read(&notify_path).unwrap();
        assert_eq!(raw.as_slice(), b"{oops", "损坏的 notify.json 不应被默认值覆盖");
        // servers/projects 正常落盘(跳过仅限 notify.json)
        assert!(dir.join("config/servers.json").exists());
        assert!(dir.join("config/projects.json").exists());

        // 用户显式保存通知配置(清除标志,与 notify_save_config 成功路径同口径)
        clear_notify_unhealthy();
        save_config(&cfg).unwrap();
        let raw = std::fs::read(&notify_path).unwrap();
        assert_ne!(raw.as_slice(), b"{oops", "清除标志后应恢复 notify.json 写回");

        clear_notify_unhealthy(); // 清理标志,不影响其他测试
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_normalize_security() {
        // 三合法值(含大小写与空白)归一化;空/非法值一律按 ssl 兜底
        assert_eq!(normalize_security("ssl"), "ssl");
        assert_eq!(normalize_security(" STARTTLS "), "starttls");
        assert_eq!(normalize_security("None"), "none");
        assert_eq!(normalize_security(""), "ssl");
        assert_eq!(normalize_security("tls"), "ssl");
        assert_eq!(normalize_security("垃圾值"), "ssl");
    }

    #[test]
    fn test_server_tags_serde_default_and_roundtrip() {
        // B3(第二十七批):旧配置无 tags 字段 → serde default 补齐为空列表
        let json = r#"{
            "id":"s1","name":"n","host":"1.2.3.4","port":22,"username":"root",
            "auth":{"auth_type":"Password","key_path":null,"password_enc":null},
            "remote_dir":"/opt/app"
        }"#;
        let s: ServerConfig = serde_json::from_str(json).unwrap();
        assert!(s.tags.is_empty(), "旧配置 tags 缺省为空");

        // roundtrip:tags 原样保留
        let mut s = s;
        s.tags = vec!["华东".into(), "生产".into()];
        let text = serde_json::to_string(&s).unwrap();
        let back: ServerConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back, s);
        assert!(text.contains("\"tags\""), "字段名须为 snake_case: {}", text);
    }

    #[test]
    fn test_normalize_tags_rules() {
        // B3:trim → 去空 → 去重保序 → cap 8 条 / 每条 24 字符(超出截断)
        let got = normalize_tags(&[
            "  华东  ".to_string(),
            "".to_string(),
            "生产".to_string(),
            "华东".to_string(), // 与首项重复(trim 后)
            "   ".to_string(),
        ]);
        assert_eq!(got, vec!["华东".to_string(), "生产".to_string()]);

        // cap 8 条:超出丢弃(保序保留前 8)
        let many: Vec<String> = (1..=12).map(|i| format!("t{}", i)).collect();
        let capped = normalize_tags(&many);
        assert_eq!(capped.len(), 8);
        assert_eq!(capped[0], "t1");
        assert_eq!(capped[7], "t8");

        // 每条 24 字符:超长截断(按字符数,不破坏 UTF-8)
        let long = "中".repeat(30);
        let got2 = normalize_tags(&[long]);
        assert_eq!(got2.len(), 1);
        assert_eq!(got2[0].chars().count(), 24, "按字符数截断: {}", got2[0]);
        // 全角/多字节不得被截成半个字符
        let mixed = format!("{}汉字", "x".repeat(23));
        let got3 = normalize_tags(&[mixed]);
        assert_eq!(got3[0].chars().count(), 24);

        // 空输入 → 空列表
        assert!(normalize_tags(&[]).is_empty());
    }

    #[test]
    fn test_auth_config_and_host_key_serde_defaults() {
        // 阶段三:旧版配置文件无 key_pass_enc / host_key_sha256 字段
        // → serde default 补齐为 None,旧行为不变
        let json = r#"{
            "id":"s1","name":"n","host":"1.2.3.4","port":22,"username":"root",
            "auth":{"auth_type":"Password","key_path":null,"password_enc":null},
            "remote_dir":"/opt/app"
        }"#;
        let s: ServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(s.auth.key_pass_enc, None);
        assert_eq!(s.host_key_sha256, None);

        // 新字段完整 roundtrip
        let mut s = s;
        s.auth.key_pass_enc = Some("enc-pass".into());
        s.host_key_sha256 = Some("SHA256:abcdef".into());
        let text = serde_json::to_string(&s).unwrap();
        let back: ServerConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn test_app_settings_default_values() {
        // 默认值:关闭到托盘关(旧行为不变)、代理为空串(直连)
        let settings = AppSettings::default();
        assert!(!settings.close_to_tray);
        assert_eq!(settings.proxy, "");
    }

    #[test]
    fn test_app_settings_roundtrip() {
        // save_app_settings → load_app_settings 逐字段相等(独立 settings.json)
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        let settings = AppSettings {
            close_to_tray: true,
            proxy: "socks5://127.0.0.1:1080".into(),
            auto_update_from_source: false,
            probe_interval_mins: 5,
            auto_check_update: true,
            term_log_keep_days: 90,
            alert_interval_mins: 15,
            alert_disk_percent: 80,
            alert_mem_percent: 85,
            alert_cpu_percent: 0,
            auto_rollback_on_failure: true,
            compose_file_names: vec!["docker-compose.prod.yml".into(), "app.yml".into()],
            compose_scan_max_depth: 6,
            tar_image: "registry.local/tar:1".into(),
            digest_hour: None,
            auto_start: false,
            host_write_paths: Vec::new(),
            incremental_transfer: true,
        };
        save_app_settings(&settings).unwrap();
        assert!(dir.join("config/settings.json").exists());
        let loaded = load_app_settings();
        assert_eq!(loaded, settings);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_app_settings_default_when_file_missing() {
        // settings.json 缺失(首次运行)→ 全默认值,不报错
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        let loaded = load_app_settings();
        assert_eq!(loaded, AppSettings::default());
        assert!(!loaded.close_to_tray);
        assert_eq!(loaded.proxy, "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_app_settings_corrupt_falls_back() {
        // settings.json 存在但损坏 → 告警并回退默认值(无 UNHEALTHY 机制,
        // 设置项无不可再生数据,后续保存直接用新值覆盖即可)
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        let settings_path = dir.join("config/settings.json");
        std::fs::write(&settings_path, b"{oops").unwrap();
        let loaded = load_app_settings();
        assert_eq!(loaded, AppSettings::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_app_settings_camel_case_serde() {
        // camelCase 序列化对齐前端字段;旧/部分文件缺字段 → serde default 补齐
        let settings = AppSettings {
            close_to_tray: true,
            proxy: "http://127.0.0.1:7890".into(),
            auto_update_from_source: false,
            probe_interval_mins: 0,
            auto_check_update: true,
            term_log_keep_days: 30,
            alert_interval_mins: 0,
            alert_disk_percent: 90,
            alert_mem_percent: 90,
            alert_cpu_percent: 90,
            auto_rollback_on_failure: false,
            compose_file_names: Vec::new(),
            compose_scan_max_depth: 0,
            tar_image: String::new(),
            digest_hour: None,
            auto_start: false,
            host_write_paths: Vec::new(),
            incremental_transfer: true,
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("\"closeToTray\":true"));
        assert!(json.contains("\"proxy\":\"http://127.0.0.1:7890\""));
        assert!(json.contains("\"termLogKeepDays\":30"));
        assert!(json.contains("\"autoRollbackOnFailure\":false"));

        let partial: AppSettings = serde_json::from_str(r#"{"closeToTray":true}"#).unwrap();
        assert!(partial.close_to_tray);
        assert_eq!(partial.proxy, "");
        // 旧配置无 autoUpdateFromSource → 默认开启(第三批默认行为)
        assert!(partial.auto_update_from_source);
        assert!(AppSettings::default().auto_update_from_source);
        // 旧配置无 termLogKeepDays → 默认 30(第二十三批)
        assert_eq!(partial.term_log_keep_days, 30);
        // 旧配置无 autoRollbackOnFailure → 默认 false(自动回滚须显式开启)
        assert!(!partial.auto_rollback_on_failure);
        assert!(!AppSettings::default().auto_rollback_on_failure);
        assert_eq!(AppSettings::default().term_log_keep_days, 30);
        // 旧配置无 alert 系列 → 间隔 0(关)/ 阈值 90(第二十四批)
        assert_eq!(partial.alert_interval_mins, 0);
        assert_eq!(partial.alert_disk_percent, 90);
        assert_eq!(partial.alert_mem_percent, 90);
        assert_eq!(partial.alert_cpu_percent, 90);
    }

    // ===== 部署断点续传(阶段六):键构造 / roundtrip / 覆盖 / 删除 / 裁剪 / 容错 =====

    /// 构造第 idx 条测试断点(ts 随 idx 区分,供最旧裁剪断言)。
    fn checkpoint(idx: usize, mode: &str) -> ResumeCheckpoint {
        ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", mode),
            mode: mode.into(),
            step_next: 2,
            ts: format!("2026-09-06 10:00:{:02}", idx % 60),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: format!("服务器{}", idx),
            project_name: "项目".into(),
            artifacts: serde_json::json!({ "imageRef": format!("app:20260906-1000{:02}", idx % 60) }),
        }
    }

    #[test]
    fn test_migrate_checkpoint_key_shape_and_isolation() {
        // 迁移键:含源/目标/项目/目标目录,且与部署键不同形(前缀隔离)
        let k = migrate_checkpoint_key("src-1", "tgt-2", "proj-3", "/opt/app");
        assert_eq!(k, "migrate|src-1|tgt-2|proj-3|/opt/app");
        // 换目标服务器/目录 → 不同键(改目标重迁是一次新迁移)
        assert_ne!(k, migrate_checkpoint_key("src-1", "tgt-9", "proj-3", "/opt/app"));
        assert_ne!(k, migrate_checkpoint_key("src-1", "tgt-2", "proj-3", "/srv/app"));
        // 与部署键的形态差异:部署键是 `server|project|mode`,不含 migrate 前缀
        let deploy_like = format!("{}|{}|{}", "src-1", "proj-3", "stack");
        assert!(!k.starts_with("src-1|"), "迁移键不得以服务器 id 开头(防被部署查询误命中)");
        assert_ne!(k, deploy_like);
    }

    #[test]
    fn test_checkpoint_key_format() {
        assert_eq!(checkpoint_key("s1", "p1", "single"), "s1|p1|single");
        assert_eq!(checkpoint_key("s1", "p1", "stack"), "s1|p1|stack");
        // 不同 mode 互为不同键(同服务器同项目可同时存在两类断点)
        assert_ne!(checkpoint_key("s1", "p1", "single"), checkpoint_key("s1", "p1", "stack"));
    }

    #[test]
    fn test_checkpoint_save_load_roundtrip_and_overwrite() {
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 保存 → 读回逐字段相等
        let cp = checkpoint(1, "single");
        save_checkpoint(&cp).unwrap();
        assert!(dir.join("config/resume-deploy.json").exists());
        let map = load_resume_map();
        assert_eq!(map.get(&cp.key), Some(&cp));

        // 同键再存(新部署覆盖旧断点:step_next/ts 更新)
        let mut newer = checkpoint(2, "single");
        newer.step_next = 3;
        save_checkpoint(&newer).unwrap();
        let map = load_resume_map();
        assert_eq!(map.len(), 1, "同键应覆盖而非新增");
        assert_eq!(map.get(&cp.key), Some(&newer));

        // 不同 mode 为不同键,可并存
        let stack_cp = checkpoint(3, "stack");
        save_checkpoint(&stack_cp).unwrap();
        let map = load_resume_map();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&stack_cp.key), Some(&stack_cp));

        // 删除:返回被删条目,读回为空;再删 → None
        let removed = remove_checkpoint(&cp.key).unwrap();
        assert_eq!(removed, Some(newer));
        assert!(load_resume_map().get(&cp.key).is_none());
        assert_eq!(remove_checkpoint(&cp.key).unwrap(), None);
        // 另一条不受影响
        assert_eq!(load_resume_map().get(&stack_cp.key), Some(&stack_cp));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_checkpoint_missing_file_returns_empty() {
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        assert!(load_resume_map().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_checkpoint_corrupt_file_returns_empty_and_self_heals() {
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        let path = dir.join("config/resume-deploy.json");
        std::fs::write(&path, "{oops").unwrap();

        // 损坏 → 空表(不 panic、不报错)
        assert!(load_resume_map().is_empty());
        // 后续保存以空表起步自愈
        let cp = checkpoint(1, "stack");
        save_checkpoint(&cp).unwrap();
        assert_eq!(load_resume_map().get(&cp.key), Some(&cp));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_checkpoint_trim_oldest_beyond_limit() {
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        for i in 0..(MAX_CHECKPOINTS + 3) {
            let mut cp = checkpoint(i, "single");
            // 每条不同键(server_id 区分),ts 递增
            cp.key = checkpoint_key(&format!("s{}", i), "p1", "single");
            cp.server_id = format!("s{}", i);
            save_checkpoint(&cp).unwrap();
        }
        let map = load_resume_map();
        assert_eq!(map.len(), MAX_CHECKPOINTS);
        // 最旧的 3 条(ts 0..3,即 s0/s1/s2)被裁掉
        assert!(!map.contains_key(&checkpoint_key("s0", "p1", "single")));
        assert!(!map.contains_key(&checkpoint_key("s2", "p1", "single")));
        // 最新一条仍在
        assert!(map.contains_key(&checkpoint_key(
            &format!("s{}", MAX_CHECKPOINTS + 2),
            "p1",
            "single"
        )));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_checkpoint_serde_camel_case() {
        let cp = checkpoint(1, "single");
        let json = serde_json::to_string(&cp).unwrap();
        // 文件字段为 camelCase(与前端读取约定一致)
        assert!(json.contains("\"stepNext\":2"), "实际: {}", json);
        assert!(json.contains("\"serverId\""), "实际: {}", json);
        assert!(json.contains("\"serverName\""), "实际: {}", json);
        assert!(json.contains("\"projectId\""), "实际: {}", json);
    }

    #[test]
    fn test_update_config_concurrent_no_lost_write() {
        // 第二十批 P2-4 回归:并发 update_config 不得丢写 —— 旧实现「load →
        // 改 → save」三步无锁,两个线程同时进入时后写覆盖先写,先到的修改
        // 被合法数据静默冲掉。收口后整个 RMW 持 CONFIG_LOCK,串行执行。
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        const THREADS: usize = 8;
        const PER_THREAD: usize = 5;
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        let id = format!("srv-{}-{}", t, i);
                        let server = ServerConfig {
                            id: id.clone(),
                            name: id.clone(),
                            host: "1.2.3.4".into(),
                            port: 22,
                            username: "root".into(),
                            auth: AuthConfig {
                                auth_type: AuthType::Password,
                                key_path: None,
                                password_enc: None,
                                key_pass_enc: None,
                            },
                            remote_dir: "/opt/app".into(),
                            host_key_sha256: None,
                            tags: Vec::new(),
                        };
                        update_config(|cfg| {
                            cfg.servers.push(server.clone());
                            Ok(())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let final_cfg = load_config().unwrap();
        assert_eq!(
            final_cfg.servers.len(),
            THREADS * PER_THREAD,
            "并发 {} 线程 × {} 条,最终应有全部条目(丢写 = 收口失效)",
            THREADS,
            PER_THREAD
        );

        std::env::remove_var("DD_CONFIG_DIR");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_update_config_error_aborts_write() {
        // 闭包返回 Err 时整体放弃:磁盘保持闭包前状态,不落半截修改
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 先写基线:一条服务器
        update_config(|cfg| {
            cfg.servers.push(ServerConfig {
                id: "base".into(),
                name: "base".into(),
                host: "1.2.3.4".into(),
                port: 22,
                username: "root".into(),
                auth: AuthConfig { auth_type: AuthType::Password, key_path: None, password_enc: None, key_pass_enc: None },
                remote_dir: "/opt/app".into(),
                host_key_sha256: None,
                tags: Vec::new(),
            });
            Ok(())
        })
        .unwrap();

        // 再跑一个「先加一条再报错」的闭包:修改必须整体回滚
        let err: std::result::Result<(), String> = update_config(|cfg| {
            cfg.servers.push(ServerConfig {
                id: "ghost".into(),
                name: "ghost".into(),
                host: "1.2.3.4".into(),
                port: 22,
                username: "root".into(),
                auth: AuthConfig { auth_type: AuthType::Password, key_path: None, password_enc: None, key_pass_enc: None },
                remote_dir: "/opt/app".into(),
                host_key_sha256: None,
                tags: Vec::new(),
            });
            Err("业务校验失败".to_string())
        });
        assert!(err.is_err());

        let final_cfg = load_config().unwrap();
        assert_eq!(final_cfg.servers.len(), 1, "Err 闭包不落盘:ghost 不得出现在磁盘");
        assert_eq!(final_cfg.servers[0].id, "base");

        std::env::remove_var("DD_CONFIG_DIR");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== 配置版本历史(第二十二批)=====

    /// 建隔离环境并预置 servers.json(直接写文件,不走 save_config —— 避免
    /// 被测函数自身产生快照干扰断言)。注意写的是**可被 load_config 解析的
    /// 完整结构**(ServerConfig 需 auth 等必填字段)。
    fn setup_history_env() -> (std::path::PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ddhist-{}", uuid::Uuid::new_v4()));
        let cfg_dir = dir.join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let s1 = r#"[{"id":"s1","name":"旧服务器","host":"1.1.1.1","port":22,"username":"root",
            "auth":{"auth_type":"Key","key_path":null,"password_enc":null,"key_pass_enc":null},
            "remote_dir":"/opt","host_key_sha256":null}]"#;
        std::fs::write(cfg_dir.join("servers.json"), s1).unwrap();
        (dir, cfg_dir)
    }

    #[test]
    fn test_snapshot_config_creates_and_dedupes() {
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (dir, cfg_dir) = setup_history_env();

        // 首次快照:目录生成,含 servers.json 内容
        snapshot_config(&cfg_dir);
        let snaps = list_snapshot_dirs(&cfg_dir.join(".history"));
        assert_eq!(snaps.len(), 1, "首次快照应生成一个目录");
        let content = std::fs::read_to_string(snaps[0].join("servers.json")).unwrap();
        assert!(content.contains("s1"), "快照应含当前内容: {}", content);

        // 同内容再快照:全等去重,不新增
        snapshot_config(&cfg_dir);
        assert_eq!(
            list_snapshot_dirs(&cfg_dir.join(".history")).len(),
            1,
            "内容全等时应跳过(防高频写点刷屏)"
        );

        // 内容变化后快照:新增第二份(同秒 → -N 后缀避让)
        std::fs::write(cfg_dir.join("servers.json"), b"[{\"id\":\"changed\"}]").unwrap();
        snapshot_config(&cfg_dir);
        let snaps2 = list_snapshot_dirs(&cfg_dir.join(".history"));
        assert_eq!(snaps2.len(), 2, "内容变化应新增快照");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_snapshot_config_cap_trims_oldest() {
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (dir, cfg_dir) = setup_history_env();
        // 预置超过上限的旧快照目录(名字按时间序)
        let root = cfg_dir.join(".history");
        for i in 0..(HISTORY_KEEP + 3) {
            let name = format!("20260101-0000{:02}", i);
            std::fs::create_dir_all(root.join(&name)).unwrap();
            std::fs::write(root.join(&name).join("servers.json"), b"[]").unwrap();
        }
        // 触发一次新快照(内容与既有不同)
        snapshot_config(&cfg_dir);
        let snaps = list_snapshot_dirs(&root);
        assert_eq!(snaps.len(), HISTORY_KEEP, "超限应裁最旧至保留上限");
        // 23 + 1 = 24 份,裁 4 份:000000-000003 应被裁掉,000004 起保留
        let names: Vec<String> = snaps
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(!names.contains(&"20260101-000000".to_string()), "最旧应先裁");
        assert!(!names.contains(&"20260101-000003".to_string()), "裁 4 份应到 000003");
        assert!(names.contains(&"20260101-000004".to_string()), "000004 起应保留");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_is_valid_snapshot_id() {
        // 合法:yyyyMMdd-HHMMSS 与带 -N 后缀
        assert!(is_valid_snapshot_id("20260915-101010"));
        assert!(is_valid_snapshot_id("20260915-101010-2"));
        // 非法:路径穿越 / 分隔符 / 点 / 空 / 位数不符 / 非数字
        assert!(!is_valid_snapshot_id(""));
        assert!(!is_valid_snapshot_id("../etc"));
        assert!(!is_valid_snapshot_id("20260915-101010/.."));
        assert!(!is_valid_snapshot_id("20260915\\101010"));
        assert!(!is_valid_snapshot_id("20260915-10101"));
        assert!(!is_valid_snapshot_id("2026091a-101010"));
        assert!(!is_valid_snapshot_id("20260915-101010-"));
        assert!(!is_valid_snapshot_id("20260915-101010-x"));
    }

    #[test]
    fn test_save_config_snapshots_previous_state() {
        let _guard = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (dir, cfg_dir) = setup_history_env();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 首次保存:快照的是「保存前」的旧内容(s1)
        let mut cfg = load_config().unwrap();
        cfg.servers.push(ServerConfig {
            id: "s-new".into(), name: "新".into(), host: "9.9.9.9".into(), port: 22,
            username: "root".into(),
            auth: AuthConfig { auth_type: AuthType::Key, key_path: None, password_enc: None, key_pass_enc: None },
            remote_dir: "/opt".into(),
            host_key_sha256: None,
            tags: Vec::new(),
        });
        save_config(&cfg).unwrap();

        let snaps = list_snapshot_dirs(&cfg_dir.join(".history"));
        assert_eq!(snaps.len(), 1, "保存应产生一份「保存前」快照");
        let snap = std::fs::read_to_string(snaps[0].join("servers.json")).unwrap();
        assert!(snap.contains("s1") && !snap.contains("s-new"), "快照内容应为覆盖前状态");

        std::env::remove_var("DD_CONFIG_DIR");
        std::fs::remove_dir_all(&dir).ok();
    }
}
