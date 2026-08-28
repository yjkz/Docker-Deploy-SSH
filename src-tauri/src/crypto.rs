//! 密码加密模块:基于 Windows DPAPI(CurrentUser 作用域)对密码进行加密,
//! 密文经 base64 编码后以字符串形式返回,便于直接存入 JSON 配置文件。
//!
//! - `dpapi_protect`:明文 -> DPAPI 加密 -> base64 密文
//! - `dpapi_unprotect`:base64 密文 -> DPAPI 解密 -> 明文
//!
//! 加密绑定当前 Windows 用户(CurrentUser 作用域),仅同一台机器上的同一
//! 用户可解密;非 Windows 平台不支持,返回错误。

/// DPAPI 加密(CurrentUser 作用域),返回 base64 编码的密文。
#[cfg(windows)]
pub fn dpapi_protect(plain: &str) -> Result<String, String> {
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use base64::Engine as _;

    let cipher = windows_dpapi::encrypt_data(plain.as_bytes(), windows_dpapi::Scope::User)
        .map_err(|e| format!("DPAPI 加密失败(CurrentUser 作用域):{e}"))?;
    Ok(BASE64_STANDARD.encode(cipher))
}

/// DPAPI 解密(CurrentUser 作用域),输入为 base64 编码的密文。
#[cfg(windows)]
pub fn dpapi_unprotect(enc: &str) -> Result<String, String> {
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use base64::Engine as _;

    let cipher = BASE64_STANDARD
        .decode(enc)
        .map_err(|e| format!("密码密文 base64 解码失败:{e}"))?;
    let plain = windows_dpapi::decrypt_data(&cipher, windows_dpapi::Scope::User)
        .map_err(|e| format!("DPAPI 解密失败(CurrentUser 作用域,仅限加密时的同一用户):{e}"))?;
    String::from_utf8(plain).map_err(|e| format!("DPAPI 解密结果不是有效的 UTF-8:{e}"))
}

/// 非 Windows 平台:DPAPI 不可用,明确返回错误。
#[cfg(not(windows))]
pub fn dpapi_protect(_plain: &str) -> Result<String, String> {
    Err("仅支持 Windows".to_string())
}

/// 非 Windows 平台:DPAPI 不可用,明确返回错误。
#[cfg(not(windows))]
pub fn dpapi_unprotect(_enc: &str) -> Result<String, String> {
    Err("仅支持 Windows".to_string())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 同机同用户可逆:加密后不是明文,且能解密还原。
    #[test]
    fn test_protect_roundtrip() {
        let enc = dpapi_protect("secret").unwrap();
        assert_ne!(enc, "secret");
        assert_eq!(dpapi_unprotect(&enc).unwrap(), "secret");
    }

    /// 非法 base64 输入应返回 Err 而不是 panic。
    #[test]
    fn test_unprotect_invalid_base64() {
        assert!(dpapi_unprotect("不是base64!!").is_err());
    }
}
