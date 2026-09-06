//! Configuration module: reads/writes `servers.json` / `projects.json` under
//! the application folder's `config/` subdirectory (portable layout).

use serde::{Deserialize, Serialize};
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
    pub key_path: Option<String>,
    pub password_enc: Option<String>,
    /// DPAPI 加密后的私钥口令(base64 密文;仅加密私钥需要;旧版配置无此字段,
    /// serde default 兼容)。导出/导入时按明文随加密 blob 携带(见 config_io)。
    #[serde(default)]
    pub key_pass_enc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthConfig,
    pub remote_dir: String,
    /// 首次连接(TOFU)时记录的服务器主机密钥 OpenSSH 风格指纹
    /// (`SHA256:` + base64(nopad)(SHA-256(公钥 SSH blob)));None = 尚未信任。
    /// 后续连接指纹不一致即拒绝(防中间人;重装/换 IP 后可在管理页重新信任)。
    /// 旧版配置无此字段,serde default 兼容。
    #[serde(default)]
    pub host_key_sha256: Option<String>,
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
}

impl Default for NotifyEvents {
    fn default() -> Self {
        Self {
            on_success: true,
            on_failure: true,
            on_cancel: false,
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
pub fn app_dir() -> PathBuf {
    match std::env::var("DD_CONFIG_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let mut exe = std::env::current_exe().expect("failed to locate current executable");
            exe.pop();
            exe
        }
    }
}

/// Returns the directory that holds `servers.json` / `projects.json`.
///
/// When the `DD_CONFIG_DIR` environment variable is set (test injection /
/// portable override) it points at the application folder and `config/` is
/// appended; otherwise the `config/` subdirectory next to the running
/// executable is used.
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
pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    write_json_atomic(&dir.join("servers.json"), &cfg.servers)?;
    write_json_atomic(&dir.join("projects.json"), &cfg.projects)?;
    if NOTIFY_UNHEALTHY.load(Ordering::Acquire) {
        log::warn!("notify.json 读取异常,本次保存跳过通知配置写回,请重启应用修复后重试");
    } else {
        write_json_atomic(&dir.join("notify.json"), &cfg.notify)?;
    }
    Ok(())
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    /// 关闭主窗口时隐藏到托盘(false = 关闭即退出;默认关)
    pub close_to_tray: bool,
    /// 检查更新使用的代理地址(http:// 或 socks5:// 前缀;空串 = 直连)
    pub proxy: String,
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
    load_app_settings()
}

/// 保存应用设置(关闭到托盘 / 更新代理;托盘拦截在关闭事件时现读文件,保存即生效)。
#[tauri::command]
pub fn app_settings_set(settings: AppSettings) -> std::result::Result<(), String> {
    save_app_settings(&settings).map_err(|e| format!("保存设置失败: {}", e))
}

#[cfg(test)]
pub(crate) static TEST_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

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
        // camelCase 序列化对齐前端字段;旧/部分文件缺 proxy 字段 → serde default 补齐
        let settings = AppSettings {
            close_to_tray: true,
            proxy: "http://127.0.0.1:7890".into(),
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("\"closeToTray\":true"));
        assert!(json.contains("\"proxy\":\"http://127.0.0.1:7890\""));

        let partial: AppSettings = serde_json::from_str(r#"{"closeToTray":true}"#).unwrap();
        assert!(partial.close_to_tray);
        assert_eq!(partial.proxy, "");
    }
}
