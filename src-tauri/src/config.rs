//! Configuration module: reads/writes `servers.json` / `projects.json` under
//! the application folder's `config/` subdirectory (portable layout).

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub servers: Vec<ServerConfig>,
    pub projects: Vec<ProjectConfig>,
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
/// (first run) yields an empty list instead of an error.
pub fn load_config() -> Result<AppConfig> {
    let dir = config_dir();
    Ok(AppConfig {
        servers: load_json_list(&dir.join("servers.json"))?,
        projects: load_json_list(&dir.join("projects.json"))?,
    })
}

/// Saves the whole application config.
///
/// Each file is written to a `.tmp` sibling first and then atomically renamed
/// over the destination, so a crash mid-write never leaves truncated JSON.
pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    write_json_atomic(&dir.join("servers.json"), &cfg.servers)?;
    write_json_atomic(&dir.join("projects.json"), &cfg.projects)?;
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
            auth: AuthConfig { auth_type: AuthType::Key, key_path: Some("C:/k".into()), password_enc: None },
            remote_dir: "/opt/app".into(),
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
}
