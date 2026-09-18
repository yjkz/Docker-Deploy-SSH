//! 架构预检(第二十五批):部署前比对**本地镜像架构**与**服务器架构**,
//! 不匹配时提前告警 —— 否则用户会在 `docker load` 或 `compose up` 后才看到
//! 难懂的 `exec format error`(即使当时看到,也很难联想到是架构问题)。
//!
//! 语义:**只告警不阻断**。理由 ——
//! - 多架构镜像(manifest list)在 load 时按目标平台取层,Docker Desktop 的
//!   `docker image inspect` 常只报宿主平台,据此硬拦会误伤正常部署;
//! - 服务器上可能有 qemu/binfmt 模拟层(如树莓派跑 x86 镜像),架构"不匹配"
//!   也能正常工作;
//! - 该检查的价值在**提示**,不在拦截 —— 真的不兼容时后续步骤会给出更精确的
//!   报错(docker load / compose up 的原始输出),届时用户已有"架构可能不匹配"
//!   的先验,排查方向明确。
//!
//! 纯函数设计:[`normalize_arch`] 把两个不同来源的架构名归一化到同一词表,
//! [`arch_mismatch_hint`] 只做判定与文案,不碰 IO —— 全部可单测。

/// 把架构名归一化到统一词表(`amd64` / `arm64` / `armv7` / `386` / `未知原样`)。
///
/// 两个来源用词不同,必须归一才能比:
/// - Docker 口径(`docker image inspect .Architecture` / `docker info .Architecture`):
///   `amd64` / `arm64` / `arm` / `386` / `ppc64le` / `s390x`
/// - uname 口径(`uname -m`):`x86_64` / `aarch64` / `armv7l` / `armv6l` / `i686`
///
/// 大小写不敏感;未知架构原样小写返回(仍可参与相等判定)。
pub(crate) fn normalize_arch(raw: &str) -> String {
    let a = raw.trim().to_ascii_lowercase();
    match a.as_str() {
        // x86 64 位
        "amd64" | "x86_64" | "x64" => "amd64".to_string(),
        // ARM 64 位
        "arm64" | "aarch64" | "arm64v8" => "arm64".to_string(),
        // ARM 32 位(armv7l/armhf/arm 都是同一族的 32 位用户态)
        "armv7" | "armv7l" | "armhf" | "arm" | "arm32v7" => "armv7".to_string(),
        "armv6" | "armv6l" | "arm32v6" => "armv6".to_string(),
        // x86 32 位
        "386" | "i386" | "i686" | "x86" => "386".to_string(),
        _ => a,
    }
}

/// 判定架构是否不匹配,返回给用户的告警文案(匹配/信息不足 → None)。纯函数。
///
/// 三项全部成立才告警:两侧都拿到了非空架构名、归一化后不等。
/// 任一缺失(远端 `uname` 失败、本地 inspect 失败)→ None(静默跳过,
/// 与磁盘预检"信息不足时告警跳过"的既有口径一致但**连告警都不发** ——
/// 该检查是纯提示,噪声越少越好)。
pub(crate) fn arch_mismatch_hint(
    image_arch: Option<&str>,
    server_arch: Option<&str>,
) -> Option<String> {
    let img = normalize_arch(image_arch?);
    let srv = normalize_arch(server_arch?);
    if img.is_empty() || srv.is_empty() || img == srv {
        return None;
    }
    Some(format!(
        "警告:镜像架构({})与服务器架构({})不一致,部署后可能报 exec format error; \
         若镜像为多架构 manifest 或服务器装有 qemu 模拟层可忽略本提示",
        img, srv
    ))
}

/// 服务器架构查询命令(`uname -m`;Linux 目标通用,不依赖 Docker)。
pub(crate) fn server_arch_cmd() -> String {
    "uname -m".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_arch_docker_and_uname_vocabularies() {
        // Docker 口径
        assert_eq!(normalize_arch("amd64"), "amd64");
        assert_eq!(normalize_arch("arm64"), "arm64");
        assert_eq!(normalize_arch("arm"), "armv7");
        assert_eq!(normalize_arch("386"), "386");
        // uname 口径
        assert_eq!(normalize_arch("x86_64"), "amd64");
        assert_eq!(normalize_arch("aarch64"), "arm64");
        assert_eq!(normalize_arch("armv7l"), "armv7");
        assert_eq!(normalize_arch("i686"), "386");
        // 大小写与空白
        assert_eq!(normalize_arch("  AMD64  "), "amd64");
        assert_eq!(normalize_arch("X86_64"), "amd64");
        // 未知架构:原样小写(仍可参与相等判定)
        assert_eq!(normalize_arch("s390x"), "s390x");
        assert_eq!(normalize_arch("ppc64le"), "ppc64le");
    }

    #[test]
    fn test_arch_mismatch_hint_matches_across_vocabularies() {
        // 跨词表的等价必须判为匹配(最容易误报的组合)
        assert_eq!(arch_mismatch_hint(Some("amd64"), Some("x86_64")), None);
        assert_eq!(arch_mismatch_hint(Some("arm64"), Some("aarch64")), None);
        assert_eq!(arch_mismatch_hint(Some("arm64"), Some("arm64")), None);
        assert_eq!(arch_mismatch_hint(Some("arm"), Some("armv7l")), None);
    }

    #[test]
    fn test_arch_mismatch_hint_detects_real_mismatch() {
        let hint = arch_mismatch_hint(Some("amd64"), Some("aarch64"));
        let hint = hint.expect("跨架构必须告警");
        assert!(hint.contains("amd64"), "文案含镜像架构: {}", hint);
        assert!(hint.contains("arm64"), "文案含归一化服务器架构: {}", hint);
        assert!(hint.contains("exec format error"), "文案点明典型症状: {}", hint);
        assert!(hint.contains("警告"), "是警告不是错误: {}", hint);
        // 反向同样告警
        assert!(arch_mismatch_hint(Some("arm64"), Some("x86_64")).is_some());
        // 32/64 位 ARM 混用也告警(armv7 镜像上 arm64 服务器跑不了)
        assert!(arch_mismatch_hint(Some("arm"), Some("aarch64")).is_some());
    }

    #[test]
    fn test_arch_mismatch_hint_silent_when_info_missing() {
        // 信息不足一律静默(纯提示检查,不做无依据的告警)
        assert_eq!(arch_mismatch_hint(None, Some("amd64")), None);
        assert_eq!(arch_mismatch_hint(Some("amd64"), None), None);
        assert_eq!(arch_mismatch_hint(None, None), None);
        assert_eq!(arch_mismatch_hint(Some(""), Some("amd64")), None);
        assert_eq!(arch_mismatch_hint(Some("amd64"), Some("   ")), None);
    }

    #[test]
    fn test_server_arch_cmd_is_plain_uname() {
        // 不依赖 Docker(服务器可能只装了 compose 而 docker CLI 在非 PATH 位置)
        assert_eq!(server_arch_cmd(), "uname -m");
    }
}
