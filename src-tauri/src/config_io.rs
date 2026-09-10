//! 配置导出 / 导入 / 清除(UPGRADE-PLAN 阶段三「连接与数据安全」)。
//!
//! 供用户把服务器 / 项目 / 通知配置整体迁移到另一台机器。因配置内的
//! SSH 密码、私钥口令、SMTP 密码均为 DPAPI 密文(绑定当前 Windows 用户,
//! 不可迁移),导出时**解密为明文**放入加密 blob,导入时逐字段**重新 DPAPI
//! 加密**写回 —— blob 本身由用户输入的导出口令保护,明文永不落盘。
//!
//! 加密容器格式(`{"version":1,"kdf":"argon2id",...}` JSON 文件):
//! - 密钥:Argon2id(默认参数 m=19MiB, t=2, p=1)由导出口令派生 32 字节 AES-256 密钥;
//! - 盐 16 字节 / nonce 12 字节均随机生成,base64(标准)编码存放;
//! - `data_b64` = AES-256-GCM(认证加密)的 JSON 明文:
//!   `{"servers":[...],"projects":[...],"notify":{...}}`,其中 servers 的
//!   `password_enc` / `key_pass_enc` 与 notify 邮箱的 `password_enc` 字段
//!   为**明文**(字段名与配置文件保持一致,值不加密blob 外不可读)。
//! - GCM 认证失败(口令错误 / 文件被篡改损坏)→ 统一报「导出口令错误或文件已损坏」。
//!
//! 命令:
//! - [`config_export_file`] / [`config_import_file`] / [`config_wipe`]。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::config::{
    config_dir, load_config, write_json_atomic, AuthType, DesktopNotify, NotifyConfig, NotifyEvents,
    ProjectConfig, ServerConfig, default_security, default_smtp_port,
};
use crate::crypto::{dpapi_protect, dpapi_unprotect};

/// 容器格式版本(当前仅支持 1)。
const EXPORT_VERSION: u32 = 1;
/// KDF 标识(Argon2id)。
const KDF_NAME: &str = "argon2id";
/// Argon2 盐长度(字节)。
const SALT_LEN: usize = 16;
/// AES-GCM nonce 长度(字节,GCM 标准 96 bit)。
const NONCE_LEN: usize = 12;
/// AES-256 密钥长度(字节)。
const KEY_LEN: usize = 32;

// ===== 加密容器 =====

/// 导出文件的最外层 JSON 结构(加密信封)。
#[derive(Debug, Serialize, Deserialize)]
struct ExportEnvelope {
    version: u32,
    kdf: String,
    salt_b64: String,
    nonce_b64: String,
    data_b64: String,
}

/// Argon2id 由口令 + 盐派生 32 字节 AES-256 密钥。
fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; KEY_LEN], String> {
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default());
    let mut key = [0u8; KEY_LEN];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| format!("导出口令密钥派生失败: {}", e))?;
    Ok(key)
}

/// 加密:payload JSON → Argon2id 派生密钥 + AES-256-GCM → 加密信封。
fn seal_blob(password: &str, payload_json: &str) -> Result<ExportEnvelope, String> {
    let mut salt = [0u8; SALT_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    let key = derive_key(password, &salt)?;

    let mut nonce = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("加密初始化失败: {}", e))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), payload_json.as_bytes())
        .map_err(|e| format!("配置导出加密失败: {}", e))?;

    Ok(ExportEnvelope {
        version: EXPORT_VERSION,
        kdf: KDF_NAME.to_string(),
        salt_b64: BASE64_STANDARD.encode(salt),
        nonce_b64: BASE64_STANDARD.encode(nonce),
        data_b64: BASE64_STANDARD.encode(ciphertext),
    })
}

/// 解密:加密信封 → Argon2id + AES-256-GCM 认证解密 → payload JSON。
/// GCM 认证失败(口令错误 / 文件损坏)→ 统一中文报错。
fn open_blob(password: &str, envelope: &ExportEnvelope) -> Result<String, String> {
    if envelope.version != EXPORT_VERSION || envelope.kdf != KDF_NAME {
        return Err(format!(
            "不支持的导出文件格式(version: {}, kdf: {}),请使用相同版本的应用导出",
            envelope.version, envelope.kdf
        ));
    }
    let salt = BASE64_STANDARD
        .decode(&envelope.salt_b64)
        .map_err(|_| "导出文件已损坏(盐字段无效)".to_string())?;
    let nonce_bytes = BASE64_STANDARD
        .decode(&envelope.nonce_b64)
        .map_err(|_| "导出文件已损坏(nonce 字段无效)".to_string())?;
    if nonce_bytes.len() != NONCE_LEN {
        return Err("导出文件已损坏(nonce 长度无效)".to_string());
    }
    let ciphertext = BASE64_STANDARD
        .decode(&envelope.data_b64)
        .map_err(|_| "导出文件已损坏(数据字段无效)".to_string())?;

    let key = derive_key(password, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("解密初始化失败: {}", e))?;
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce_bytes), ciphertext.as_ref())
        // GCM 校验标签失败 = 口令不对或内容被篡改,二者不可区分,统一提示
        .map_err(|_| "导出口令错误或文件已损坏".to_string())?;
    String::from_utf8(plaintext).map_err(|_| "导出文件已损坏(内容不是有效的 UTF-8)".to_string())
}

// ===== blob 内的明文镜像结构 =====

/// 服务器认证信息(blob 内明文镜像):字段名与 [`crate::config::AuthConfig`]
/// 一致,但 `password_enc` / `key_pass_enc` 存放**明文**(可移植)。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportAuth {
    auth_type: AuthType,
    #[serde(default)]
    key_path: Option<String>,
    #[serde(default)]
    password_enc: Option<String>,
    #[serde(default)]
    key_pass_enc: Option<String>,
}

/// 服务器配置(blob 内明文镜像,结构与 [`crate::config::ServerConfig`] 一致)。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportServer {
    id: String,
    name: String,
    host: String,
    port: u16,
    username: String,
    auth: ExportAuth,
    remote_dir: String,
    #[serde(default)]
    host_key_sha256: Option<String>,
}

/// SMTP 邮箱配置(blob 内明文镜像):`password_enc` 为明文。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportEmail {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    smtp_host: String,
    #[serde(default = "default_smtp_port")]
    port: u16,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password_enc: Option<String>,
    #[serde(default = "default_security")]
    security: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: Vec<String>,
}

/// 通知配置(blob 内明文镜像;desktop/events 无敏感字段,原样携带)。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportNotify {
    #[serde(default)]
    desktop: DesktopNotify,
    email: ExportEmail,
    #[serde(default)]
    events: NotifyEvents,
}

/// blob 内的完整配置载荷(projects 无敏感字段,原样携带)。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportPayload {
    servers: Vec<ExportServer>,
    projects: Vec<ProjectConfig>,
    notify: ExportNotify,
}

/// 导入结果摘要(camelCase 序列化,前端直接读取)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub servers: usize,
    pub projects: usize,
}

// ===== 导出 / 导入转换 =====

/// 把 DPAPI 密文解密为明文;空值/空串按未配置处理(返回 None)。
fn decrypt_secret(enc: &Option<String>) -> Result<Option<String>, String> {
    match enc.as_deref().filter(|e| !e.is_empty()) {
        Some(e) => Ok(Some(dpapi_unprotect(e)?)),
        None => Ok(None),
    }
}

/// ServerConfig → 明文镜像(解密 SSH 密码与私钥口令)。
fn export_server(s: &ServerConfig) -> Result<ExportServer, String> {
    Ok(ExportServer {
        id: s.id.clone(),
        name: s.name.clone(),
        host: s.host.clone(),
        port: s.port,
        username: s.username.clone(),
        auth: ExportAuth {
            auth_type: s.auth.auth_type.clone(),
            key_path: s.auth.key_path.clone(),
            password_enc: decrypt_secret(&s.auth.password_enc)?,
            key_pass_enc: decrypt_secret(&s.auth.key_pass_enc)?,
        },
        remote_dir: s.remote_dir.clone(),
        host_key_sha256: s.host_key_sha256.clone(),
    })
}

/// 明文镜像 → ServerConfig(SSH 密码与私钥口令重新 DPAPI 加密)。
fn import_server(s: ExportServer) -> Result<ServerConfig, String> {
    let protect = |plain: &Option<String>| -> Result<Option<String>, String> {
        match plain.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => Ok(Some(dpapi_protect(p)?)),
            None => Ok(None),
        }
    };
    Ok(ServerConfig {
        id: s.id,
        name: s.name,
        host: s.host,
        port: s.port,
        username: s.username,
        auth: crate::config::AuthConfig {
            auth_type: s.auth.auth_type,
            key_path: s.auth.key_path,
            password_enc: protect(&s.auth.password_enc)?,
            key_pass_enc: protect(&s.auth.key_pass_enc)?,
        },
        remote_dir: s.remote_dir,
        host_key_sha256: s.host_key_sha256,
    })
}

/// NotifyConfig → 明文镜像(解密 SMTP 密码)。
fn export_notify(n: &NotifyConfig) -> Result<ExportNotify, String> {
    Ok(ExportNotify {
        desktop: n.desktop.clone(),
        email: ExportEmail {
            enabled: n.email.enabled,
            smtp_host: n.email.smtp_host.clone(),
            port: n.email.port,
            username: n.email.username.clone(),
            password_enc: decrypt_secret(&n.email.password_enc)?,
            security: n.email.security.clone(),
            from: n.email.from.clone(),
            to: n.email.to.clone(),
        },
        events: n.events.clone(),
    })
}

/// 明文镜像 → NotifyConfig(SMTP 密码重新 DPAPI 加密)。
fn import_notify(n: ExportNotify) -> Result<NotifyConfig, String> {
    let password_enc = match n.email.password_enc.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => Some(dpapi_protect(p)?),
        None => None,
    };
    Ok(NotifyConfig {
        desktop: n.desktop,
        email: crate::config::EmailNotify {
            enabled: n.email.enabled,
            smtp_host: n.email.smtp_host,
            port: n.email.port,
            username: n.email.username,
            password_enc,
            security: n.email.security,
            from: n.email.from,
            to: n.email.to,
        },
        events: n.events,
    })
}

// ===== Tauri 命令 =====

/// 导出配置到加密文件(`path` 由前端 `dialog.save` 提供)。
///
/// 收集 servers / projects / notify → 解密 DPAPI 敏感字段为明文 →
/// Argon2id(导出口令)+ AES-256-GCM 加密 → 写入 JSON 信封文件。
#[tauri::command]
pub fn config_export_file(password: String, path: String) -> Result<(), String> {
    if password.is_empty() {
        return Err("导出口令不能为空".to_string());
    }
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let servers: Vec<ExportServer> = cfg
        .servers
        .iter()
        .map(export_server)
        .collect::<Result<Vec<_>, String>>()?;
    let payload = ExportPayload {
        servers,
        projects: cfg.projects.clone(),
        notify: export_notify(&cfg.notify)?,
    };
    let payload_json =
        serde_json::to_string(&payload).map_err(|e| format!("配置序列化失败: {}", e))?;
    let envelope = seal_blob(&password, &payload_json)?;
    let file_json = serde_json::to_string_pretty(&envelope)
        .map_err(|e| format!("导出文件序列化失败: {}", e))?;
    std::fs::write(&path, file_json)
        .map_err(|e| format!("写入导出文件失败 ({}): {}", path, e))?;
    Ok(())
}

/// 从加密文件导入配置(覆盖 servers.json / projects.json / notify.json)。
///
/// 读文件 → 校验信封 → AES-256-GCM 解密(口令错误/损坏统一报错)→
/// 解析校验 servers/projects/notify 结构 → 逐字段 DPAPI 重加密 →
/// 三个配置文件原子覆盖写。内存状态不自动重载(前端导入成功后自行刷新)。
#[tauri::command]
pub fn config_import_file(path: String, password: String) -> Result<ImportSummary, String> {
    let raw =
        std::fs::read(&path).map_err(|e| format!("读取导出文件失败 ({}): {}", path, e))?;
    let envelope: ExportEnvelope = serde_json::from_slice(&raw)
        .map_err(|e| format!("导出文件格式不正确: {}", e))?;
    let payload_json = open_blob(&password, &envelope)?;
    let payload: ExportPayload = serde_json::from_str(&payload_json)
        .map_err(|e| format!("导出文件内容损坏(结构解析失败): {}", e))?;

    // 逐字段 DPAPI 重加密(SSH 密码 / 私钥口令 / SMTP 密码)
    let servers: Vec<ServerConfig> = payload
        .servers
        .into_iter()
        .map(import_server)
        .collect::<Result<Vec<_>, String>>()?;
    let notify = import_notify(payload.notify)?;

    // 原子覆盖写(与 save_config 同一 .tmp + rename 原语;导入即整体替换,
    // 直接写三个文件,不经过 NOTIFY_UNHEALTHY 的「跳过写回」保护)
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败: {}", e))?;
    let server_count = servers.len();
    let projects = &payload.projects;
    write_json_atomic(&dir.join("servers.json"), &servers)
        .map_err(|e| format!("写入 servers.json 失败: {}", e))?;
    write_json_atomic(&dir.join("projects.json"), projects)
        .map_err(|e| format!("写入 projects.json 失败: {}", e))?;
    write_json_atomic(&dir.join("notify.json"), &notify)
        .map_err(|e| format!("写入 notify.json 失败: {}", e))?;

    Ok(ImportSummary {
        servers: server_count,
        projects: projects.len(),
    })
}

/// 清除本机全部部署配置:删除 servers.json / projects.json / notify.json /
/// deployments.json(部署历史),日志保留。文件不存在则忽略。
/// 内存状态不重载(前端清除成功后自行刷新)。
#[tauri::command]
pub fn config_wipe() -> Result<(), String> {
    let dir = config_dir();
    for name in ["servers.json", "projects.json", "notify.json", "deployments.json"] {
        let path = dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => log::info!("已删除配置文件: {}", path.display()),
            // 缺文件(首次运行/已清除)→ 忽略
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!("删除配置文件失败 ({}): {}", path.display(), e));
            }
        }
    }
    Ok(())
}

// ===== 单元测试 =====

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, EmailNotify};

    #[test]
    fn test_seal_open_roundtrip_and_wrong_password() {
        let payload = r#"{"servers":[],"projects":[],"notify":null}"#;
        let envelope = seal_blob("export-pass-1", payload).expect("seal 失败");

        // 信封字段与格式约定
        assert_eq!(envelope.version, 1);
        assert_eq!(envelope.kdf, "argon2id");
        assert_eq!(
            BASE64_STANDARD.decode(&envelope.salt_b64).unwrap().len(),
            SALT_LEN
        );
        assert_eq!(
            BASE64_STANDARD.decode(&envelope.nonce_b64).unwrap().len(),
            NONCE_LEN
        );

        // 正确口令 → 原文还原
        assert_eq!(open_blob("export-pass-1", &envelope).unwrap(), payload);

        // 错误口令 → 明确中文报错(GCM 认证失败)
        let err = open_blob("wrong-pass", &envelope).unwrap_err();
        assert_eq!(err, "导出口令错误或文件已损坏");
    }

    #[test]
    fn test_open_blob_rejects_unknown_version() {
        let envelope = seal_blob("p", "{}").unwrap();
        let mut tampered = envelope;
        tampered.version = 999;
        let err = open_blob("p", &tampered).unwrap_err();
        assert!(err.contains("不支持的导出文件格式"), "实际: {err}");
    }

    #[test]
    fn test_open_blob_rejects_tampered_data() {
        let envelope = seal_blob("p", r#"{"a":1}"#).unwrap();
        // 篡改密文(即使口令正确,GCM 校验也失败 → 同一口令错误/损坏文案)
        let mut tampered = envelope;
        tampered.data_b64 = BASE64_STANDARD.encode([0u8; 32]);
        assert_eq!(open_blob("p", &tampered).unwrap_err(), "导出口令错误或文件已损坏");
    }

    #[test]
    fn test_export_envelope_is_not_plaintext() {
        // 密文里不得出现明文口令/内容
        let secret = "TOP-SECRET-PAYLOAD-内容";
        let envelope = seal_blob("pass", secret).unwrap();
        assert!(!envelope.data_b64.contains("SECRET"));
        assert_ne!(BASE64_STANDARD.decode(&envelope.data_b64).unwrap(), secret.as_bytes());
    }

    /// 全链路 roundtrip(仅 Windows:DPAPI 依赖):保存配置 → 导出 → 清除 →
    /// 导入 → 服务器/项目数量一致、敏感字段经「解密→重加密」后可还原原文。
    #[cfg(windows)]
    #[test]
    fn test_export_import_wipe_roundtrip() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-io-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 1. 准备本机配置(SSH 密码 + 私钥口令 + SMTP 密码均为 DPAPI 密文)
        let mut cfg = crate::config::AppConfig::default();
        cfg.servers.push(ServerConfig {
            id: "s1".into(),
            name: "生产".into(),
            host: "1.2.3.4".into(),
            port: 22,
            username: "root".into(),
            auth: AuthConfig {
                auth_type: AuthType::Key,
                key_path: Some("C:/k.pem".into()),
                password_enc: Some(dpapi_protect("ssh-pw").unwrap()),
                key_pass_enc: Some(dpapi_protect("key-pass").unwrap()),
            },
            remote_dir: "/opt/app".into(),
            host_key_sha256: Some("SHA256:abc".into()),
        });
        cfg.projects.push(ProjectConfig {
            id: "p1".into(),
            name: "栈项目".into(),
            image_filter: String::new(),
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
        cfg.notify.email = EmailNotify {
            enabled: true,
            smtp_host: "smtp.example.com".into(),
            port: 465,
            username: "bot@example.com".into(),
            password_enc: Some(dpapi_protect("smtp-pw").unwrap()),
            security: "ssl".into(),
            from: "bot@example.com".into(),
            to: vec!["a@example.com".into()],
        };
        crate::config::save_config(&cfg).unwrap();

        // 2. 导出(口令加密)→ 导出文件必须是加密信封,不含明文
        let export_path = dir.join("export.json");
        config_export_file("io-pass".into(), export_path.to_str().unwrap().into()).unwrap();
        let raw = std::fs::read_to_string(&export_path).unwrap();
        assert!(!raw.contains("ssh-pw") && !raw.contains("key-pass") && !raw.contains("smtp-pw"));
        let env: ExportEnvelope = serde_json::from_str(&raw).unwrap();
        assert_eq!(env.version, 1);
        // blob 内敏感字段为明文(可移植)
        let payload_json = open_blob("io-pass", &env).unwrap();
        assert!(payload_json.contains("ssh-pw") && payload_json.contains("key-pass"));

        // 3. 清除本机配置(文件应全部消失;再清一次不报错)
        config_wipe().unwrap();
        assert!(!dir.join("config/servers.json").exists());
        assert!(!dir.join("config/projects.json").exists());
        assert!(!dir.join("config/notify.json").exists());
        assert!(!dir.join("config/deployments.json").exists());
        config_wipe().unwrap();

        // 4. 错误口令导入 → 明确报错,不落盘
        let err = config_import_file(
            export_path.to_str().unwrap().into(),
            "bad-pass".into(),
        )
        .unwrap_err();
        assert_eq!(err, "导出口令错误或文件已损坏");
        assert!(!dir.join("config/servers.json").exists());

        // 5. 正确口令导入 → 数量一致,敏感字段重新 DPAPI 加密且可解密还原
        let summary = config_import_file(export_path.to_str().unwrap().into(), "io-pass".into())
            .unwrap();
        assert_eq!(summary.servers, 1);
        assert_eq!(summary.projects, 1);

        let loaded = crate::config::load_config().unwrap();
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].host, "1.2.3.4");
        assert_eq!(loaded.servers[0].host_key_sha256.as_deref(), Some("SHA256:abc"));
        assert_eq!(
            dpapi_unprotect(loaded.servers[0].auth.password_enc.as_deref().unwrap()).unwrap(),
            "ssh-pw"
        );
        assert_eq!(
            dpapi_unprotect(loaded.servers[0].auth.key_pass_enc.as_deref().unwrap()).unwrap(),
            "key-pass"
        );
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].name, "栈项目");
        assert!(loaded.notify.email.enabled);
        assert_eq!(
            dpapi_unprotect(loaded.notify.email.password_enc.as_deref().unwrap()).unwrap(),
            "smtp-pw"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
