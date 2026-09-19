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
    /// 服务器标签(第二十七批 B3);旧导出文件缺字段 → serde default 空列表。
    #[serde(default)]
    tags: Vec<String>,
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
    /// 成功通知最小部署耗时(秒;第二十批阶段五;旧导出文件缺失 → 0 恒通知)
    #[serde(default)]
    min_duration_secs: u32,
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

/// 导入预览摘要(camelCase;第二十批阶段二):**不落盘**的试运行结果 ——
/// 前端据此弹「将覆盖 X 台 / 新增 N 台」确认,用户确认后才调
/// [`config_import_file`] 真导入。跨机场景 `smtp_password_present = true`
/// 时前端明示「导入后需在新机重录 SMTP 密码」(DPAPI 密文不可跨机)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreview {
    /// 备份内服务器/项目数量
    pub servers: usize,
    pub projects: usize,
    /// 当前配置中的服务器/项目数量(将被整体替换)
    pub current_servers: usize,
    pub current_projects: usize,
    /// 备份内 SMTP 密码非空 → true(跨机导入后失效,需重录)
    pub smtp_password_present: bool,
}

// ===== 导出 / 导入转换 =====

/// 把 DPAPI 密文解密为明文;空值/空串按未配置处理(返回 None)。
/// 哨兵/非 base64 的损坏密文(如 v6.1.0 曾落盘的 `"*"`)给出可操作报错,
/// 指明是哪台服务器的哪个字段 —— 替代晦涩的 base64 解码失败(v6.1.3)。
fn decrypt_secret(enc: &Option<String>) -> Result<Option<String>, String> {
    match enc.as_deref().filter(|e| !e.is_empty()) {
        Some(e) => {
            if e == crate::commands::CIPHER_SENTINEL || !e.bytes().all(is_b64_char) {
                return Err(
                    "存在未有效保存的密码(密文占位符或损坏)。请先在「服务器管理」页逐台重录密码后再导出"
                        .to_string(),
                );
            }
            Ok(Some(dpapi_unprotect(e)?))
        }
        None => Ok(None),
    }
}

/// base64 标准字母表字符判定(与 commands::mod 同口径,预检挡下哨兵/乱码)。
fn is_b64_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='
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
        tags: s.tags.clone(),
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
        tags: s.tags,
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
        min_duration_secs: n.min_duration_secs,
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
        min_duration_secs: n.min_duration_secs.min(crate::notify::MIN_DURATION_SECS_MAX),
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
/// 「读文件 → 校验信封 → 解密 → 解析」的共用实现(导入预览与正式导入同源,
/// DRY;第二十批阶段二抽出)。
fn parse_export_file(path: &str, password: &str) -> Result<ExportPayload, String> {
    let raw =
        std::fs::read(path).map_err(|e| format!("读取导出文件失败 ({}): {}", path, e))?;
    let envelope: ExportEnvelope = serde_json::from_slice(&raw)
        .map_err(|e| format!("导出文件格式不正确: {}", e))?;
    let payload_json = open_blob(password, &envelope)?;
    let payload: ExportPayload = serde_json::from_str(&payload_json)
        .map_err(|e| format!("导出文件内容损坏(结构解析失败): {}", e))?;
    Ok(payload)
}

/// 导入预览(第二十批阶段二,不落盘):解密解析备份 + 读取当前配置,
/// 返回「将覆盖/新增」摘要;口令错误/文件损坏与正式导入同口径报错,
/// 但**绝不写任何文件** —— 确认后才由 [`config_import_file`] 落盘。
#[tauri::command]
pub fn config_import_preview(path: String, password: String) -> Result<ImportPreview, String> {
    let payload = parse_export_file(&path, &password)?;
    let smtp_password_present = payload
        .notify
        .email
        .password_enc
        .as_deref()
        .map(|p| !p.trim().is_empty())
        .unwrap_or(false);
    let (current_servers, current_projects) = match crate::config::load_config() {
        Ok(cfg) => (cfg.servers.len(), cfg.projects.len()),
        // 当前配置读不到(首次运行/损坏):按 0 计,不阻断预览
        Err(_) => (0, 0),
    };
    Ok(ImportPreview {
        servers: payload.servers.len(),
        projects: payload.projects.len(),
        current_servers,
        current_projects,
        smtp_password_present,
    })
}

/// 从加密文件导入配置(覆盖 servers.json / projects.json / notify.json)。
///
/// 读文件 → 校验信封 → AES-256-GCM 解密(口令错误/损坏统一报错)→
/// 解析校验 servers/projects/notify 结构 → 逐字段 DPAPI 重加密 →
/// 三个配置文件原子覆盖写。内存状态不自动重载(前端导入成功后自行刷新)。
#[tauri::command]
pub fn config_import_file(path: String, password: String) -> Result<ImportSummary, String> {
    let payload = parse_export_file(&path, &password)?;

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
    // 导入是整体替换(最需要兜底的场景之一)→ 覆盖前先快照当前状态
    // (第二十二批配置版本历史;失败仅告警不阻断)
    crate::config::snapshot_config(&dir);
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

// ===== 配置版本历史(第二十二批)=====

/// 一条快照的元信息(camelCase 直通前端)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSnapshot {
    /// 快照目录名 = `yyyyMMdd-HHMMSS`(恢复时的 id)
    pub id: String,
    /// 含哪些配置文件(servers/projects/notify 子集)
    pub files: Vec<String>,
    /// 三文件字节数合计
    pub size_bytes: u64,
}

/// 列出全部配置快照(新 → 旧)。
#[tauri::command]
pub fn config_history_list() -> Vec<ConfigSnapshot> {
    let root = crate::config::history_dir();
    let mut dirs = crate::config::list_snapshot_dirs(&root);
    dirs.reverse(); // 目录名 = 时间戳,倒序 = 新 → 旧
    dirs.into_iter()
        .filter_map(|path| {
            let id = path.file_name()?.to_string_lossy().to_string();
            let mut files = Vec::new();
            let mut size: u64 = 0;
            for name in ["servers.json", "projects.json", "notify.json"] {
                if let Ok(meta) = std::fs::metadata(path.join(name)) {
                    files.push(name.to_string());
                    size += meta.len();
                }
            }
            if files.is_empty() {
                return None; // 空快照目录(异常残留)不展示
            }
            Some(ConfigSnapshot { id, files, size_bytes: size })
        })
        .collect()
}

/// 恢复指定快照到当前配置。
///
/// 安全与语义:
/// - `id` 经 [`crate::config::is_valid_snapshot_id`] 校验(防路径穿越),
///   且恢复目录必须存在;
/// - **恢复前先把当前状态快照一份**(误恢复可再恢复回去);
/// - 快照内存在的文件原样拷回(三件套子集;缺失的跳过)。
#[tauri::command]
pub fn config_history_restore(id: String) -> Result<(), String> {
    if !crate::config::is_valid_snapshot_id(&id) {
        return Err(format!("快照标识不合法:{}", id));
    }
    let root = crate::config::history_dir();
    let src = root.join(id.trim());
    if !src.is_dir() {
        return Err(format!("快照不存在:{}", id));
    }
    let dir = config_dir();
    // 恢复前留一份当前状态(可回退)
    crate::config::snapshot_config(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败: {}", e))?;
    let mut restored: Vec<&str> = Vec::new();
    for name in ["servers.json", "projects.json", "notify.json"] {
        let from = src.join(name);
        if !from.is_file() {
            continue;
        }
        let bytes = std::fs::read(&from)
            .map_err(|e| format!("读取快照文件失败 ({}): {}", name, e))?;
        std::fs::write(dir.join(name), bytes)
            .map_err(|e| format!("写入配置文件失败 ({}): {}", name, e))?;
        restored.push(name);
    }
    if restored.is_empty() {
        return Err("该快照不含可恢复的配置文件".to_string());
    }
    log::info!("配置已从快照 {} 恢复: {:?}", id, restored);
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
            tags: vec!["华东".into(), "生产".into()],
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
        // B3:标签随导出/导入往返(第二十七批)
        assert_eq!(loaded.servers[0].tags, vec!["华东".to_string(), "生产".to_string()]);
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

    /// B3(第二十七批):**旧导出文件**(本版前导出,blob 内无 tags 字段)
    /// 导入不报错,标签落为空列表(向后兼容),其余字段照常。
    #[test]
    fn test_import_legacy_export_without_tags() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        crate::config::save_config(&crate::config::AppConfig::default()).unwrap();

        // 手工构造旧版载荷:服务器对象**不含** tags 字段
        let payload = r#"{
            "servers":[{
                "id":"old1","name":"旧机","host":"9.9.9.9","port":22,"username":"root",
                "auth":{"auth_type":"Password","key_path":null,"password_enc":null,"key_pass_enc":null},
                "remote_dir":"/opt/old","host_key_sha256":null
            }],
            "projects":[],
            "notify":{
                "desktop":{"enabled":false},
                "email":{"enabled":false,"smtp_host":"","port":465,"username":"",
                         "password_enc":null,"security":"ssl","from":"","to":[]},
                "events":{}
            }
        }"#;
        let env = seal_blob("pw", payload).unwrap();
        let path = dir.join("legacy.json");
        std::fs::write(&path, serde_json::to_string(&env).unwrap()).unwrap();

        let summary = config_import_file(path.to_str().unwrap().into(), "pw".into())
            .expect("旧导出文件必须可导入(serde default 兼容)");
        assert_eq!(summary.servers, 1);
        let loaded = crate::config::load_config().unwrap();
        assert_eq!(loaded.servers[0].id, "old1");
        assert!(loaded.servers[0].tags.is_empty(), "旧文件导入后标签为空列表");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 导入预览(第二十批阶段二):摘要字段正确;口令错误与导入同口径;
    /// **预览不落盘**(当前配置文件字节不变)。
    #[test]
    fn test_import_preview_summary_and_no_write() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-preview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 当前配置:1 台服务器 / 1 个项目 / SMTP 密码已存
        let mut cfg = crate::config::AppConfig::default();
        cfg.servers.push(ServerConfig {
            id: "cur".into(),
            name: "当前".into(),
            host: "5.6.7.8".into(),
            port: 22,
            username: "root".into(),
            auth: AuthConfig { auth_type: AuthType::Password, key_path: None, password_enc: None, key_pass_enc: None },
            remote_dir: "/opt/cur".into(),
            host_key_sha256: None,
            tags: Vec::new(),
        });
        cfg.projects.push(ProjectConfig {
            id: "p-cur".into(),
            name: "当前项目".into(),
            image_filter: String::new(),
            compose_file: "C:/cur/docker-compose.yml".into(),
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
            to: Vec::new(),
        };
        crate::config::save_config(&cfg).unwrap();

        // 备份:含 2 台服务器 / 3 个项目 / SMTP 密码明文(blob 内)
        let payload = ExportPayload {
            servers: vec![
                ExportServer {
                    id: "b1".into(), name: "备份1".into(), host: "1.1.1.1".into(), port: 22,
                    username: "root".into(),
                    auth: ExportAuth { auth_type: AuthType::Password, key_path: None, password_enc: None, key_pass_enc: None },
                    remote_dir: "/opt/b1".into(), host_key_sha256: None,
                    tags: vec!["备份组".into()],
                },
                ExportServer {
                    id: "b2".into(), name: "备份2".into(), host: "2.2.2.2".into(), port: 22,
                    username: "root".into(),
                    auth: ExportAuth { auth_type: AuthType::Password, key_path: None, password_enc: None, key_pass_enc: None },
                    remote_dir: "/opt/b2".into(), host_key_sha256: None,
                    tags: Vec::new(),
                },
            ],
            projects: (0..3)
                .map(|i| ProjectConfig {
                    id: format!("bp{}", i),
                    name: format!("备份项目{}", i),
                    image_filter: String::new(),
                    compose_file: "C:/b/docker-compose.yml".into(),
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
                })
                .collect(),
            notify: ExportNotify {
                desktop: Default::default(),
                email: ExportEmail {
                    enabled: false,
                    smtp_host: String::new(),
                    port: 0,
                    username: String::new(),
                    // blob 内 SMTP 密码是明文(导出口径)
                    password_enc: Some("smtp-plain-in-blob".into()),
                    security: String::new(),
                    from: String::new(),
                    to: Vec::new(),
                },
                events: Default::default(),
                min_duration_secs: 0,
            },
        };
        let payload_json = serde_json::to_string(&payload).unwrap();
        let envelope = seal_blob("pv-pass", &payload_json).unwrap();
        let export_path = dir.join("export.json");
        std::fs::write(
            &export_path,
            serde_json::to_string_pretty(&envelope).unwrap(),
        )
        .unwrap();

        // 预览前抓当前配置字节(断言不落盘用)
        let before = std::fs::read(dir.join("config/servers.json")).unwrap();

        // 错误口令:与导入同口径报错
        assert_eq!(
            config_import_preview(export_path.to_str().unwrap().into(), "bad".into())
                .unwrap_err(),
            "导出口令错误或文件已损坏"
        );

        // 正确口令:摘要正确
        let pv = config_import_preview(export_path.to_str().unwrap().into(), "pv-pass".into())
            .unwrap();
        assert_eq!(pv.servers, 2, "备份内服务器数");
        assert_eq!(pv.projects, 3, "备份内项目数");
        assert_eq!(pv.current_servers, 1, "当前服务器数(将被覆盖)");
        assert_eq!(pv.current_projects, 1, "当前项目数(将被覆盖)");
        assert!(pv.smtp_password_present, "备份含 SMTP 密码 → 跨机需重录提示");

        // 预览不落盘:配置文件字节不变
        let after = std::fs::read(dir.join("config/servers.json")).unwrap();
        assert_eq!(before, after, "预览不得写任何配置文件");
        let loaded = crate::config::load_config().unwrap();
        assert_eq!(loaded.servers.len(), 1, "当前配置仍是 1 台(未被替换)");

        std::env::remove_var("DD_CONFIG_DIR");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== 配置版本历史(第二十二批)=====

    #[test]
    fn test_history_restore_roundtrip_and_reject() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddhistio-{}", uuid::Uuid::new_v4()));
        let cfg_dir = dir.join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 预置一个快照目录(合法 id),内容为标记版 servers.json
        let root = cfg_dir.join(".history");
        let snap_id = "20260915-101010";
        std::fs::create_dir_all(root.join(snap_id)).unwrap();
        std::fs::write(root.join(snap_id).join("servers.json"), br#"[{"id":"snap-marker"}]"#).unwrap();
        // 当前配置为另一个内容
        std::fs::write(cfg_dir.join("servers.json"), br#"[{"id":"current"}]"#).unwrap();

        // 列表:含该快照
        let list = config_history_list();
        assert!(list.iter().any(|s| s.id == snap_id), "列表应含预置快照");
        let entry = list.iter().find(|s| s.id == snap_id).unwrap();
        assert!(entry.files.iter().any(|f| f == "servers.json"));

        // 恢复:文件被替换为快照内容
        config_history_restore(snap_id.into()).unwrap();
        let restored = std::fs::read_to_string(cfg_dir.join("servers.json")).unwrap();
        assert!(restored.contains("snap-marker"), "恢复后应为快照内容: {}", restored);
        // 恢复前自动留了当前状态快照(可回退)
        let snaps = crate::config::list_snapshot_dirs(&root);
        assert!(snaps.len() >= 2, "恢复前应自动留一份当前状态快照");

        // 非法 id:路径穿越拒绝
        assert!(config_history_restore("../etc".into()).is_err());
        assert!(config_history_restore("20260915-101010/..".into()).is_err());
        // 合法格式但不存在
        assert!(config_history_restore("20990101-000000".into()).is_err());

        std::env::remove_var("DD_CONFIG_DIR");
        std::fs::remove_dir_all(&dir).ok();
    }
}
