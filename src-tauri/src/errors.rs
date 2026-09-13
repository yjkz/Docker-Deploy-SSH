//! 结构化错误码(第十六批):给错误串挂机器可读的类别标记,消除「前端/后端
//! 依赖中文字面子串做流程判定」的隐性契约(wiki/07 限制 13)。
//!
//! ## 设计:前缀标记旁路(零签名改动)
//!
//! 全后端错误通道统一是 `Result<T, String>`(94 个命令 + 12 个事件 payload),
//! 全面结构化(改 `Result<T, ErrEnum>`)要动全部命令签名与前端契约,面大且
//! 破坏既有调用。本模块走**旁路**:错误串仍是一个 String,只是头部带
//! `[dderr:<code>]` 标记:
//!
//! ```text
//! [dderr:canceled]部署已取消
//! ```
//!
//! - **生产**:`tagged(ErrCode::X, msg)` 包一层;未包的错误串(大量既有
//!   生产点)解析得 `None`,行为与旧版完全一致 —— **可渐进采纳**;
//! - **判定**:`code_of(s)` 取码,取代 `e == CANCELLED_MSG` / `is_transport_error`
//!   等文案匹配;判定点**只认码,不认文案**;
//! - **展示**:`strip(s)` 剥掉标记取人类文案(或直接用原串——标记用控制字符
//!   前缀,见下)。
//!
//! ## 标记格式:US 控制字符 + `[dderr:...]`
//!
//! 标记格式为 `\u{001f}[dderr:<code>]`(单元分隔符开头):
//! - 控制字符在 UI 文本中不可见且不会与用户数据撞车(错误串的其余部分是
//!   中文/英文人类文案,首字符必非控制字符);
//! - 老版本前端收到带标记的错误串时,肉眼不可见(US 不渲染),不破坏旧展示;
//! - 新前端 `code_of` 取码后 `strip` 展示,人类文案不受影响。
//!
//! ## 增减纪律(低耦合的核心)
//!
//! - **加类**:本枚举加一个变体 + `as_str` 加一行;使用点自行取用;前端
//!   `parseErrCode` 需要区分时加分支(不认识的码一律回退按无码处理,不需改);
//! - **删类**:删除变体后编译器逐点报出所有使用处(枚举穷尽匹配),不会漏;
//! - **前后端契约**:码名(as_str 的 ASCII 串)是契约本体,列于 wiki/04;
//!   判定方不认识的码必须按「无码」降级,不得报错 —— 前后端版本错配时安全。

use std::fmt;

/// 错误类别。`as_str` 的 ASCII 串是前后端契约(见 wiki/04 错误码表)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrCode {
    /// 用户取消(部署/回滚/迁移被主动中止;前端据此区分「已跳过/已取消」与失败)
    Cancelled,
    /// SSH/SFTP 传输层失败(连接不可信,需要重连;监控熔断按此计数)
    Transport,
    /// SSH 认证失败(密码/私钥/口令错误或被拒绝)
    Auth,
    /// 权限拒绝(SSH 用户无 docker.sock 权限;监控立即停止不重试)
    PermDenied,
    /// 超时(连接/执行/上传超时;与传输失败的区别:连接可能仍可用)
    Timeout,
    /// 网络类失败(检查更新的 HTTP 请求/DNS/代理;与 SSH Transport 分域)
    Network,
    /// 远端命令/容器操作以非零退出码失败(命令送达了、执行失败了)
    Protocol,
    /// 配置问题(服务器/项目配置缺失、字段非法、配置文件损坏)
    Config,
    /// 解析失败(compose 解析 / NDJSON / 响应 JSON 等)
    Parse,
    /// 本地/远端文件系统操作失败(读写/建目录/元数据)
    Fs,
    /// 用户输入不合法(表单值越界、路径格式错误等;在管线入口即拒绝)
    Input,
    /// 内部错误(panic 兜底、不变量破坏;文案统一提示看日志)
    Internal,
}

impl ErrCode {
    /// 契约串(ASCII,snake_case;写进错误串与前端契约表)。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ErrCode::Cancelled => "canceled",
            ErrCode::Transport => "transport",
            ErrCode::Auth => "auth",
            ErrCode::PermDenied => "perm_denied",
            ErrCode::Timeout => "timeout",
            ErrCode::Network => "network",
            ErrCode::Protocol => "protocol",
            ErrCode::Config => "config",
            ErrCode::Parse => "parse",
            ErrCode::Fs => "fs",
            ErrCode::Input => "input",
            ErrCode::Internal => "internal",
        }
    }

    /// 由契约串解析(解析器 `code_of` 使用;未知串返回 None —— 前后端版本
    /// 错配时按无码降级,不报错)。
    fn from_str_raw(s: &str) -> Option<Self> {
        match s {
            "canceled" => Some(ErrCode::Cancelled),
            "transport" => Some(ErrCode::Transport),
            "auth" => Some(ErrCode::Auth),
            "perm_denied" => Some(ErrCode::PermDenied),
            "timeout" => Some(ErrCode::Timeout),
            "network" => Some(ErrCode::Network),
            "protocol" => Some(ErrCode::Protocol),
            "config" => Some(ErrCode::Config),
            "parse" => Some(ErrCode::Parse),
            "fs" => Some(ErrCode::Fs),
            "input" => Some(ErrCode::Input),
            "internal" => Some(ErrCode::Internal),
            _ => None,
        }
    }
}

/// 标记前缀(单元分隔符 US,控制字符,UI 不渲染)。
const TAG_PREFIX: &str = "\u{001f}[dderr:";

impl fmt::Display for ErrCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 给错误串挂类别标记:`tagged(ErrCode::Cancelled, "部署已取消")` 产出
/// `\u{001f}[dderr:canceled]部署已取消`。文案原样保留(用户可见部分不变)。
pub(crate) fn tagged(code: ErrCode, msg: impl Into<String>) -> String {
    format!("{TAG_PREFIX}{}]{}", code.as_str(), msg.into())
}

/// 取错误串的类别码(无标记/未知码 → None)。
/// 判定方拿到 None 时按「无码」走旧逻辑或默认分支,**不得报错**。
pub(crate) fn code_of(err: &str) -> Option<ErrCode> {
    let rest = err.strip_prefix(TAG_PREFIX)?;
    let end = rest.find(']')?;
    ErrCode::from_str_raw(&rest[..end])
}

/// 剥掉标记取人类文案(无标记时原样返回;展示层用)。
pub(crate) fn strip(err: &str) -> &str {
    match err.strip_prefix(TAG_PREFIX) {
        Some(rest) => {
            let end = rest.find(']').expect("tagged() 生成的串必有右括号");
            &rest[end + 1..]
        }
        None => err,
    }
}

/// 取消类错误便捷构造(全后端统一的取消文案;原 CANCELLED_MSG 语义)。
pub(crate) fn cancelled() -> String {
    tagged(ErrCode::Cancelled, "部署已取消")
}

/// docker 权限拒绝类错误便捷构造(原 PERM_DENIED_MSG 语义)。
pub(crate) fn perm_denied() -> String {
    tagged(
        ErrCode::PermDenied,
        "当前 SSH 用户无 Docker 权限(无法访问 /var/run/docker.sock),请将该用户加入 docker 组或使用 root 用户连接",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tagged_and_code_of_roundtrip() {
        let e = tagged(ErrCode::Cancelled, "部署已取消");
        assert_eq!(code_of(&e), Some(ErrCode::Cancelled));
        assert_eq!(strip(&e), "部署已取消");
    }

    #[test]
    fn test_plain_error_has_no_code() {
        // 未包标记的旧式错误:解析得 None,strip 原样返回
        assert_eq!(code_of("SSH 密码认证失败: 密码错误"), None);
        assert_eq!(strip("SSH 密码认证失败: 密码错误"), "SSH 密码认证失败: 密码错误");
    }

    #[test]
    fn test_unknown_code_degrades_to_none() {
        // 前后端版本错配:新码旧解析器 → None(不得报错)
        assert_eq!(code_of("\u{001f}[dderr:future_code]新错误"), None);
        // strip 仍能剥出文案(展示不受未知码影响)
        assert_eq!(strip("\u{001f}[dderr:future_code]新错误"), "新错误");
    }

    #[test]
    fn test_user_message_with_bracket_not_misread() {
        // 用户文案里含 [dderr:] 但无 US 前缀 → 不是标记,不误读
        assert_eq!(code_of("[dderr:canceled] 伪装"), None);
        assert_eq!(strip("[dderr:canceled] 伪装"), "[dderr:canceled] 伪装");
    }

    #[test]
    fn test_all_codes_roundtrip() {
        for code in [
            ErrCode::Cancelled,
            ErrCode::Transport,
            ErrCode::Auth,
            ErrCode::PermDenied,
            ErrCode::Timeout,
            ErrCode::Network,
            ErrCode::Protocol,
            ErrCode::Config,
            ErrCode::Parse,
            ErrCode::Fs,
            ErrCode::Input,
            ErrCode::Internal,
        ] {
            let e = tagged(code, "x");
            assert_eq!(code_of(&e), Some(code), "码 {} 往返失败", code.as_str());
        }
    }

    #[test]
    fn test_cancelled_and_perm_denied_helpers() {
        assert_eq!(code_of(&cancelled()), Some(ErrCode::Cancelled));
        assert_eq!(strip(&cancelled()), "部署已取消");
        let pd = perm_denied();
        assert_eq!(code_of(&pd), Some(ErrCode::PermDenied));
        assert!(strip(&pd).starts_with("当前 SSH 用户无 Docker 权限"));
    }
}
