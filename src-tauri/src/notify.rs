//! 通知中心(UPGRADE-PLAN 阶段二):桌面系统通知 + SMTP 邮件通知。
//!
//! - [`notify_get_config`] / [`notify_save_config`]:读写 `AppConfig.notify`
//!   (随 `save_config` 持久化到 `config/notify.json`);邮件密码明文入参 →
//!   DPAPI 加密存储,查询时脱敏(只回传 `password_saved`,不回传明文/密文);
//!   启用邮件通知时做保存校验(主机/收件人必填且逐项含 @,见
//!   [`validate_email_save`]);
//! - [`notify_test_desktop`] / [`notify_test_email`]:按当前表单值发送测试
//!   通知/测试邮件(测试邮件不要求先保存配置);
//! - `fire`:部署收尾(成功/失败/取消)统一入口(见 [`fire`] 的文档),
//!   按事件订阅与渠道开关分发;内部 tokio 异步任务执行不阻塞调用方,
//!   任何失败仅 `log::warn!` 不上抛,绝不影响部署结果与 history。
//!
//! SMTP 发送基于 lettre 0.11(smtp-transport + rustls-tls + builder,rustls
//! ring provider 避免 openssl 原生依赖);builder 组合见 [`send_email`]。

use std::time::Duration;

use lettre::message::{header::ContentType, Mailbox};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use crate::config::{load_config, normalize_security, save_config, EmailNotify, NotifyConfig};
use crate::crypto::{dpapi_protect, dpapi_unprotect};

/// 桌面测试通知标题(固定)。
const DESKTOP_TEST_TITLE: &str = "DockerDeploy SSH";
/// 桌面测试通知正文(固定)。
const DESKTOP_TEST_BODY: &str = "这是一条测试通知";
/// 测试邮件主题(固定)。
const EMAIL_TEST_SUBJECT: &str = "DockerDeploy SSH 测试邮件";
/// SMTP 命令/连接整体超时(秒)。
const SMTP_TIMEOUT_SECS: u64 = 30;

// ===== 前端视图与入参(camelCase)=====

/// 桌面通知配置视图。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopNotifyView {
    pub enabled: bool,
}

/// 邮件配置视图:密码不回传(明文/密文都不出后端),只回传 `password_saved`。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailNotifyView {
    pub enabled: bool,
    pub smtp_host: String,
    pub port: u16,
    pub username: String,
    /// 是否已保存 SMTP 密码(DPAPI 密文)
    pub password_saved: bool,
    pub security: String,
    pub from: String,
    pub to: Vec<String>,
}

/// 事件订阅开关视图。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyEventsView {
    pub on_success: bool,
    pub on_failure: bool,
    pub on_cancel: bool,
    pub on_probe: bool,
}

/// `notify_get_config` 的返回(camelCase 序列化给前端)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyConfigView {
    pub desktop: DesktopNotifyView,
    pub email: EmailNotifyView,
    pub events: NotifyEventsView,
}

/// `notify_save_config` / `notify_test_email` 的桌面部分入参。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DesktopNotifyInput {
    #[serde(default)]
    pub enabled: bool,
}

/// `notify_save_config` 的邮件部分入参:密码字段为明文,
/// `None`/空 = 保留已存密文(「密码留空 = 保持已存值」)。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmailNotifyInput {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_input_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub security: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: Vec<String>,
}

/// `notify_save_config` 的事件订阅入参。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NotifyEventsInput {
    #[serde(default = "default_input_true")]
    pub on_success: bool,
    #[serde(default = "default_input_true")]
    pub on_failure: bool,
    #[serde(default)]
    pub on_cancel: bool,
    #[serde(default)]
    pub on_probe: bool,
}

/// `notify_save_config` 的入参(camelCase)。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NotifyConfigInput {
    #[serde(default)]
    pub desktop: DesktopNotifyInput,
    #[serde(default)]
    pub email: EmailNotifyInput,
    #[serde(default)]
    pub events: NotifyEventsInput,
}

/// `notify_test_email` 的入参:表单当前值(不要求先保存配置)。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmailTestInput {
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_input_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    /// `None`/空 → 解密已存密文使用(都没有则报错)
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub security: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: Vec<String>,
}

fn default_input_port() -> u16 {
    465
}

fn default_input_true() -> bool {
    true
}

/// 端口 0(空输入)的兜底默认端口:按归一化加密方式映射
/// (ssl→465 隐式 TLS、starttls→587、none→25),与 [`send_email`] 实际
/// 使用的加密通道口径一致,避免「无加密却兜底到 465」的端口/加密错配。
fn default_port_for_security(security: &str) -> u16 {
    match normalize_security(security) {
        "starttls" => 587,
        "none" => 25,
        _ => 465,
    }
}

/// 邮件通知保存校验(纯函数,便于单测):`enabled = false` 直接通过
/// (桌面通知无必填项,不做校验);启用时要求 SMTP 主机非空、收件人列表
/// 非空且逐项含 `@`。不满足返回中文错误,经前端 onSave 的 catch toast 展示。
fn validate_email_save(enabled: bool, smtp_host: &str, to: &[String]) -> Result<(), String> {
    if !enabled {
        return Ok(());
    }
    if smtp_host.trim().is_empty() {
        return Err("SMTP 主机为空,启用邮件通知时请先填写服务器地址".to_string());
    }
    if to.is_empty() {
        return Err("收件人列表为空,启用邮件通知时请至少填写一个收件人".to_string());
    }
    let invalid: Vec<&str> = to
        .iter()
        .map(|s| s.trim())
        .filter(|t| !t.contains('@'))
        .collect();
    if !invalid.is_empty() {
        return Err(format!(
            "收件人地址无效(需包含 @):{}",
            invalid.join("、")
        ));
    }
    Ok(())
}

// ===== Tauri 命令 =====

/// 读取通知配置(邮件密码脱敏:只回传 password_saved)。
#[tauri::command]
pub async fn notify_get_config() -> Result<NotifyConfigView, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    Ok(to_view(&cfg.notify))
}

/// 保存通知配置:密码 Some(非空) → DPAPI 加密存储;None/空 → 保留已存密文。
///
/// 保存校验:`email.enabled = true` 时要求 SMTP 主机非空、收件人列表非空且
/// 逐项含 `@`(见 [`validate_email_save`]),不满足直接返回中文错误。
/// 保存成功前清除 config 层的 notify.json 读取异常标志(用户显式保存通知
/// 配置视为对异常的知情修复,本次新配置允许写回覆盖)。
#[tauri::command]
pub async fn notify_save_config(cfg: NotifyConfigInput) -> Result<(), String> {
    // 保存校验(仅邮件启用时;不满足直接返回错误,不落任何盘)
    validate_email_save(cfg.email.enabled, &cfg.email.smtp_host, &cfg.email.to)?;
    let mut app_cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    // 密码:表单输入了非空明文 → 重新 DPAPI 加密;否则保留已存密文(可能为 None)
    let password_enc = match cfg.email.password.as_deref().map(str::trim) {
        Some(p) if !p.is_empty() => Some(dpapi_protect(p)?),
        _ => app_cfg.notify.email.password_enc.clone(),
    };
    // 加密方式先归一化:端口 0(空输入)的兜底默认值按归一化结果映射
    let security = normalize_security(&cfg.email.security).to_string();
    app_cfg.notify = NotifyConfig {
        desktop: crate::config::DesktopNotify {
            enabled: cfg.desktop.enabled,
        },
        email: EmailNotify {
            enabled: cfg.email.enabled,
            smtp_host: cfg.email.smtp_host.trim().to_string(),
            // 端口 0(空输入)按加密方式兜底默认端口(ssl→465/starttls→587/none→25)
            port: if cfg.email.port == 0 {
                default_port_for_security(&security)
            } else {
                cfg.email.port
            },
            username: cfg.email.username.trim().to_string(),
            password_enc,
            security,
            from: cfg.email.from,
            to: cfg.email.to,
        },
        events: crate::config::NotifyEvents {
            on_success: cfg.events.on_success,
            on_failure: cfg.events.on_failure,
            on_cancel: cfg.events.on_cancel,
            on_probe: cfg.events.on_probe,
        },
    };
    // 用户显式保存通知配置 → 先清除读取异常标志,让本次保存能把新配置
    // (含新密文)写回 notify.json
    crate::config::clear_notify_unhealthy();
    save_config(&app_cfg).map_err(|e| format!("保存配置失败: {}", e))
}

/// 发送一条本地系统测试通知(标题/正文固定)。
#[tauri::command]
pub async fn notify_test_desktop(app: AppHandle) -> Result<(), String> {
    show_desktop_notification(&app, DESKTOP_TEST_TITLE, DESKTOP_TEST_BODY)
}

/// 按表单当前值发送测试邮件(不要求先保存配置;密码未改则用已存密文解密)。
#[tauri::command]
pub async fn notify_test_email(_app: AppHandle, cfg: EmailTestInput) -> Result<(), String> {
    // 密码:表单非空明文优先;否则解密已存密文;都没有 → 明确报错
    let saved_enc = load_config()
        .map_err(|e| format!("读取配置失败: {}", e))?
        .notify
        .email
        .password_enc;
    let password = resolve_email_password(cfg.password.as_deref(), saved_enc.as_deref())?;

    // 加密方式先归一化:端口 0(空输入)的兜底默认值按归一化结果映射
    let security = normalize_security(&cfg.security).to_string();
    let email_cfg = EmailNotify {
        enabled: true,
        smtp_host: cfg.smtp_host.trim().to_string(),
        // 端口 0(空输入)按加密方式兜底默认端口(ssl→465/starttls→587/none→25)
        port: if cfg.port == 0 {
            default_port_for_security(&security)
        } else {
            cfg.port
        },
        username: cfg.username.trim().to_string(),
        password_enc: None,
        security,
        from: cfg.from,
        to: cfg.to,
    };
    let body = format!(
        "这是一封来自 DockerDeploy SSH 的测试邮件。\r\n发送时间:{}\r\n\r\n收到此邮件说明 SMTP 通知配置有效。",
        chrono::Local::now().format("%F %T")
    );
    // lettre 的 SMTP 发送是阻塞 IO,放 blocking 线程池避免卡异步运行时
    tauri::async_runtime::spawn_blocking(move || {
        build_and_send(&email_cfg, &password, EMAIL_TEST_SUBJECT, body)
    })
    .await
    .map_err(|e| format!("发送测试邮件任务失败: {}", e))?
}

// ===== 桌面通知 =====

/// 发送桌面系统通知(tauri-plugin-notification 的 Rust API)。
///
/// Rust 侧直接调用不经过 IPC,不受 capability 权限约束(`notification:default`
/// 仅约束前端 invoke 通道,已加入 capabilities/default.json 备前端使用)。
fn show_desktop_notification(app: &AppHandle, title: &str, body: &str) -> Result<(), String> {
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|e| format!("发送系统通知失败: {}", e))
}

// ===== 邮件通知(测试命令与 fire 共用的发送路径)=====

/// 解析邮件密码(纯逻辑,便于单测):表单非空明文优先;否则 DPAPI 解密已存
/// 密文;两者都没有 → Err(中文提示)。
fn resolve_email_password(plain: Option<&str>, enc: Option<&str>) -> Result<String, String> {
    match plain.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => Ok(p.to_string()),
        None => match enc.map(str::trim).filter(|e| !e.is_empty()) {
            Some(enc) => dpapi_unprotect(enc),
            None => Err("请先填写 SMTP 密码或先保存配置".to_string()),
        },
    }
}

/// 构造纯文本邮件:`from` 单发件人,`to` 逐个 To,UTF-8 纯文本正文
/// (中文正文/主题由 lettre 自动做 RFC 2047 头编码与 quoted-printable 体编码)。
fn build_email(
    from: &str,
    to: &[String],
    subject: &str,
    body: String,
) -> Result<Message, String> {
    let from = from.trim();
    if from.is_empty() {
        return Err("发件人为空,请先填写发件人地址".to_string());
    }
    if to.is_empty() {
        return Err("收件人列表为空,请至少填写一个收件人".to_string());
    }
    let from_mbox: Mailbox = from
        .parse()
        .map_err(|e| format!("发件人地址无效「{}」: {}", from, e))?;
    let mut builder = Message::builder()
        .from(from_mbox)
        .subject(subject)
        .date_now()
        .header(ContentType::TEXT_PLAIN);
    for addr in to {
        let mbox: Mailbox = addr.trim().parse().map_err(|e| {
            format!("收件人地址无效「{}」: {}", addr, e)
        })?;
        builder = builder.to(mbox);
    }
    builder.body(body).map_err(|e| format!("构造邮件失败: {}", e))
}

/// 按 security 组装 lettre 发送通道并投递(阻塞,调用方须放 blocking 线程池)。
///
/// builder 组合(经 lettre 0.11.23 源码核实,`SmtpTransportBuilder` 链式消费):
///
/// - ssl:`SmtpTransport::relay(host)` → `Tls::Wrapper` 隐式 TLS(默认 465);
/// - starttls:`SmtpTransport::starttls_relay(host)` → `Tls::Required`(默认 587);
/// - none:`SmtpTransport::builder_dangerous(host)` → 无 TLS(默认 25)。
///
/// 三者均再链 `.port(port)` 允许自定义端口;用户名非空时链 `.credentials(...)`
/// (AUTH 机制按服务器宣告在 PLAIN/LOGIN 间自动协商)。
fn send_email(cfg: &EmailNotify, password: &str, email: &Message) -> Result<(), String> {
    let host = cfg.smtp_host.trim();
    if host.is_empty() {
        return Err("SMTP 主机为空,请先填写服务器地址".to_string());
    }
    let builder = match normalize_security(&cfg.security) {
        "starttls" => SmtpTransport::starttls_relay(host)
            .map_err(|e| format!("TLS 配置失败(请检查 SMTP 主机名): {}", e))?,
        "none" => SmtpTransport::builder_dangerous(host),
        // ssl(默认兜底):465 隐式 TLS
        _ => SmtpTransport::relay(host)
            .map_err(|e| format!("TLS 配置失败(请检查 SMTP 主机名): {}", e))?,
    };
    let mut transport = builder.port(cfg.port);
    if !cfg.username.is_empty() {
        transport = transport.credentials(Credentials::new(
            cfg.username.clone(),
            password.to_string(),
        ));
    }
    let sender = transport
        .timeout(Some(Duration::from_secs(SMTP_TIMEOUT_SECS)))
        .build();
    match sender.send(email) {
        Ok(resp) => {
            log::info!(
                "通知邮件已投递:{}",
                resp.message().next().unwrap_or("服务器已受理")
            );
            Ok(())
        }
        Err(e) => Err(smtp_error_message(&e)),
    }
}

/// 构造邮件并投递(测试命令与 fire 分发的共享路径;阻塞,放 blocking 线程池)。
fn build_and_send(
    cfg: &EmailNotify,
    password: &str,
    subject: &str,
    body: String,
) -> Result<(), String> {
    let email = build_email(&cfg.from, &cfg.to, subject, body)?;
    send_email(cfg, password, &email)
}

/// lettre SMTP 错误 → 中文分类提示(附原始错误文本,便于现场排查)。
///
/// 分类口径(lettre 0.11 `smtp::Error` 的 kind 判定式,经源码核实):
/// TLS 握手 / 超时 / 认证相关 5xx(530/534/535/538)/ 其他 4xx-5xx(发件被拒)/
/// 其余(Connection/Network/Client 等)归为连接失败。
fn smtp_error_message(e: &lettre::transport::smtp::Error) -> String {
    let raw = e.to_string();
    if e.is_tls() {
        format!(
            "TLS 握手失败(加密方式与端口不匹配、证书不受信任或服务器不支持 TLS):{}",
            raw
        )
    } else if e.is_timeout() {
        format!("连接或发送超时(请检查 SMTP 主机/端口/网络):{}", raw)
    } else if e.is_permanent() {
        // 认证失败通常为服务器对 AUTH 命令回复 530/534/535/538
        let code = e.status().map(|c| c.to_string()).unwrap_or_default();
        if ["530", "534", "535", "538"].iter().any(|c| code.starts_with(c)) {
            format!(
                "认证失败(用户名或密码错误;部分邮箱需使用授权码而非登录密码):{}",
                raw
            )
        } else {
            format!("发件被服务器拒绝(永久):{}", raw)
        }
    } else if e.is_transient() {
        format!("发件被服务器暂时拒绝(可稍后重试):{}", raw)
    } else if e.is_client() {
        // 典型:服务器宣告的 AUTH 机制与 PLAIN/LOGIN 均不兼容
        format!("SMTP 客户端错误(服务器可能不支持所需的认证方式):{}", raw)
    } else {
        // Connection / Network / Response / TransportShutdown 等:建连阶段失败
        format!(
            "连接失败(请检查 SMTP 主机、端口与网络可达性):{}",
            raw
        )
    }
}

// ===== 部署收尾通知分发 =====

/// 部署收尾通知分发入口(commands.rs 三个收尾点在 emit `deploy-done` 之后调用)。
///
/// `kind`:`"success"` / `"failure"` / `"cancel"`,对应事件订阅开关
/// `events.on_success` / `on_failure` / `on_cancel`。
///
/// - 内部 `tauri::async_runtime::spawn` 异步执行,不阻塞调用方(部署收尾路径);
/// - 读配置失败、事件未订阅、渠道未启用或发送失败一律仅 `log::warn!`,
///   不上抛、不影响部署结果与 history。
pub(crate) async fn fire(app: AppHandle, kind: &str, title: String, body: String) {
    let kind = kind.to_string();
    tauri::async_runtime::spawn(async move {
        // 事件订阅判断:未知事件类型直接跳过
        let notify = match load_config() {
            Ok(cfg) => cfg.notify,
            Err(e) => {
                log::warn!("部署通知跳过:读取配置失败: {}", e);
                return;
            }
        };
        let subscribed = match kind.as_str() {
            "success" => notify.events.on_success,
            "failure" => notify.events.on_failure,
            "cancel" => notify.events.on_cancel,
            "probe" => notify.events.on_probe,
            other => {
                log::warn!("通知跳过:未知事件类型「{}」", other);
                return;
            }
        };
        if !subscribed {
            return;
        }
        // 渠道 1:桌面系统通知
        if notify.desktop.enabled {
            if let Err(e) = show_desktop_notification(&app, &title, &body) {
                log::warn!("部署桌面通知发送失败: {}", e);
            }
        }
        // 渠道 2:邮件(SMTP 发送是阻塞 IO,放 blocking 线程池)
        if notify.email.enabled {
            match resolve_email_password(None, notify.email.password_enc.as_deref()) {
                Ok(password) => {
                    let subject = title.clone();
                    // JoinHandle 输出 = Result<发送结果, JoinError>:内层是发送/构造错误
                    match tauri::async_runtime::spawn_blocking(move || {
                        build_and_send(&notify.email, &password, &subject, body)
                    })
                    .await
                    {
                        Ok(Ok(())) => log::info!("部署通知邮件已发送:{}", title),
                        Ok(Err(e)) => log::warn!("部署通知邮件发送失败:{}", e),
                        Err(e) => log::warn!("部署通知邮件任务失败:{}", e),
                    }
                }
                Err(e) => log::warn!("部署通知邮件跳过:{}", e),
            }
        }
    });
}

// ===== 视图转换 =====

/// `NotifyConfig` → 前端视图(密码脱敏)。
fn to_view(cfg: &NotifyConfig) -> NotifyConfigView {
    NotifyConfigView {
        desktop: DesktopNotifyView {
            enabled: cfg.desktop.enabled,
        },
        email: EmailNotifyView {
            enabled: cfg.email.enabled,
            smtp_host: cfg.email.smtp_host.clone(),
            port: cfg.email.port,
            username: cfg.email.username.clone(),
            password_saved: cfg
                .email
                .password_enc
                .as_deref()
                .map(|e| !e.trim().is_empty())
                .unwrap_or(false),
            security: cfg.email.security.clone(),
            from: cfg.email.from.clone(),
            to: cfg.email.to.clone(),
        },
        events: NotifyEventsView {
            on_success: cfg.events.on_success,
            on_failure: cfg.events.on_failure,
            on_cancel: cfg.events.on_cancel,
            on_probe: cfg.events.on_probe,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表单明文优先于已存密文,且 trim 生效。
    #[test]
    fn test_resolve_password_plain_priority() {
        assert_eq!(
            resolve_email_password(Some("  fresh "), Some("ZmFrZQ==")).unwrap(),
            "fresh"
        );
    }

    /// 空明文回退到已存密文;两者皆无 → 明确报错。
    #[test]
    fn test_resolve_password_fallback_and_missing() {
        assert!(resolve_email_password(Some("  "), None)
            .unwrap_err()
            .contains("请先填写 SMTP 密码或先保存配置"));
        assert!(resolve_email_password(None, None)
            .unwrap_err()
            .contains("请先填写 SMTP 密码或先保存配置"));
    }

    /// 非法 base64 密文 → 解密报错而不是 panic(Windows 真实 DPAPI 路径)。
    #[cfg(windows)]
    #[test]
    fn test_resolve_password_dpapi_roundtrip() {
        let enc = dpapi_protect("secret").unwrap();
        assert_eq!(resolve_email_password(None, Some(&enc)).unwrap(), "secret");
        assert!(resolve_email_password(None, Some("不是base64!!")).is_err());
    }

    /// 地址校验:空发件人 / 空收件人 / 非法地址 → 中文报错。
    #[test]
    fn test_build_email_validation() {
        let to = vec!["a@example.com".to_string()];
        assert!(build_email("", &to, "s", "b".into()).unwrap_err().contains("发件人"));
        assert!(build_email("f@example.com", &[], "s", "b".into())
            .unwrap_err()
            .contains("收件人"));
        assert!(build_email("不是邮箱", &to, "s", "b".into())
            .unwrap_err()
            .contains("发件人地址无效"));
        let bad_to = vec!["a@example.com".to_string(), "b#bad".to_string()];
        assert!(build_email("f@example.com", &bad_to, "s", "b".into())
            .unwrap_err()
            .contains("收件人地址无效"));
    }

    /// 合法地址可构造出含多收件人与 UTF-8 主题/正文的邮件。
    #[test]
    fn test_build_email_ok() {
        let to = vec!["a@example.com".into(), "b@example.com".into()];
        let msg = build_email(
            "DockerDeploy <f@example.com>",
            &to,
            "测试主题",
            "中文正文".to_string(),
        )
        .unwrap();
        let raw = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(raw.contains("a@example.com"));
        assert!(raw.contains("b@example.com"));
        // 主题经 RFC 2047 编码,不应出现裸中文(防止部分服务器拒收)
        assert!(raw.contains("Subject: =?utf-8?"));
    }

    /// 保存校验:未启用直接通过;启用时 SMTP 主机/收件人必填且逐项含 @。
    #[test]
    fn test_validate_email_save() {
        // 未启用:不做校验(桌面通知无必填项)
        assert!(validate_email_save(false, "", &[]).is_ok());
        // 启用:SMTP 主机必填(trim 后判空)
        assert!(validate_email_save(true, "  ", &["a@example.com".into()])
            .unwrap_err()
            .contains("SMTP 主机"));
        // 启用:收件人列表非空
        assert!(validate_email_save(true, "smtp.example.com", &[])
            .unwrap_err()
            .contains("收件人"));
        // 启用:逐项需含 @(非法项列入报错原文)
        let bad_to = vec!["a@example.com".to_string(), "b#bad".to_string()];
        let err = validate_email_save(true, "smtp.example.com", &bad_to).unwrap_err();
        assert!(err.contains("收件人地址无效"));
        assert!(err.contains("b#bad"));
        // 全部合法 → 通过
        let ok_to = vec!["a@example.com".to_string()];
        assert!(validate_email_save(true, "smtp.example.com", &ok_to).is_ok());
    }

    /// 端口 0 兜底按归一化加密方式:ssl→465、starttls→587、none→25;
    /// 空/非法值按 ssl 兜底 → 465。
    #[test]
    fn test_default_port_for_security() {
        assert_eq!(default_port_for_security("ssl"), 465);
        assert_eq!(default_port_for_security("STARTTLS"), 587);
        assert_eq!(default_port_for_security(" none "), 25);
        assert_eq!(default_port_for_security(""), 465);
        assert_eq!(default_port_for_security("垃圾值"), 465);
    }
}
