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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub id: String,
    pub name: String,
    pub image_filter: String,
    pub compose_file: String,
    pub file_mappings: Vec<FileMapping>,
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

/// Returns the directory that holds `servers.json` / `projects.json`.
///
/// When the `DD_CONFIG_DIR` environment variable is set (test injection /
/// portable override) it points at the application folder and `config/` is
/// appended; otherwise the `config/` subdirectory next to the running
/// executable is used.
pub fn config_dir() -> PathBuf {
    let base = match std::env::var("DD_CONFIG_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let mut exe = std::env::current_exe().expect("failed to locate current executable");
            exe.pop();
            exe
        }
    };
    base.join("config")
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

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
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
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ddtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        let mut cfg = AppConfig::default();
        cfg.servers.push(ServerConfig {
            id: "s1".into(), name: "生产".into(), host: "1.2.3.4".into(), port: 22,
            username: "root".into(),
            auth: AuthConfig { auth_type: AuthType::Key, key_path: Some("C:/k".into()), password_enc: None },
            remote_dir: "/opt/app".into(),
        });
        // config_dir 依赖环境变量以便测试注入
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        save_config(&cfg).unwrap();
        let loaded = load_config().unwrap();
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].host, "1.2.3.4");
        assert!(dir.join("config/servers.json").exists());
    }
}
