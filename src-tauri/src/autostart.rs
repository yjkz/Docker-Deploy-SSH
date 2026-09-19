//! 开机自启(第二十九批 S1)。
//!
//! **为什么不用 `tauri-plugin-autostart`**:该插件要新增一个 crate 依赖,
//! 而本项目的构建环境拉取新 crates 不可靠(离线/代理受限),且 Windows 上
//! 「开机自启」本身只需一个注册表值 —— 用系统工具实现即可,零新依赖、
//! 无构建风险。
//!
//! **状态真值 = 注册表,不是我们的配置**:用户可能在「任务管理器 → 启动」
//! 里禁用本项,或手工删掉注册表值;若以 `AppSettings.auto_start` 为准,
//! 设置界面会显示与系统实际不符的状态。故 [`is_enabled`] 每次现读注册表,
//! 设置中心的开关也以它为准(见 `app_settings_get` 的覆盖逻辑)。
//!
//! **仅 Windows**:其余平台 [`is_enabled`] 恒 false、[`set_enabled`] 返回
//! 明确错误(本项目发版目标只有 Windows,不做跨平台自启的假承诺)。

#[cfg(target_os = "windows")]
use std::process::Command;

/// 统一的子进程入口(Windows 下附加 CREATE_NO_WINDOW,避免 GUI 子系统
/// 每次 spawn 控制台程序时闪出黑窗;与 `docker.rs` 的同名纪律一致)。
#[cfg(target_os = "windows")]
fn reg_cmd() -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut c = Command::new("reg");
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// 注册表路径:当前用户的开机启动项(HKCU,不需要管理员权限)。
#[cfg(target_os = "windows")]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// 值名(出现在任务管理器「启动」列表里的名字)。
#[cfg(target_os = "windows")]
const VALUE_NAME: &str = "DockerDeploySSH";

/// 取当前可执行文件路径(带引号,防路径含空格时注册表项被截断)。
///
/// **为什么必须引号**:`Run` 键的值被当作命令行执行,未加引号的
/// `C:\Program Files\x\app.exe` 会被解析成 `C:\Program` + 参数。
#[cfg(target_os = "windows")]
fn exe_command() -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("无法定位当前可执行文件: {}", e))?;
    // 开发期(exe 在 target/debug)也允许写入 —— 但设置界面会提示
    // 「当前为开发构建」,避免把产物路径写进注册表后长期失效
    Ok(format!("\"{}\"", exe.display()))
}

/// 查询是否已设置开机自启(现读注册表)。
///
/// 读的是**实际值**而非我们的配置 —— 用户可能在任务管理器里禁用,
/// 只有现读才能反映真实状态。读取失败(键不存在/权限问题)→ false。
#[cfg(target_os = "windows")]
pub fn is_enabled() -> bool {
    let out = reg_cmd()
        .args(["query", RUN_KEY, "/v", VALUE_NAME])
        .output();
    match out {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

#[cfg(not(target_os = "windows"))]
pub fn is_enabled() -> bool {
    false
}

/// 设置开机自启:`enabled = true` 写入注册表值,false 删除。
///
/// 写入的值 = 当前 exe 的**引号包裹**路径(见 [`exe_command`])。
/// 注册表操作失败 → 中文错误(设置中心 toast 原文)。
#[cfg(target_os = "windows")]
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if enabled {
        let cmd = exe_command()?;
        let out = reg_cmd()
            .args([
                "add",
                RUN_KEY,
                "/v",
                VALUE_NAME,
                "/t",
                "REG_SZ",
                "/d",
                &cmd,
                "/f",
            ])
            .output()
            .map_err(|e| format!("写入开机自启项失败: {}", e))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "写入开机自启项失败:{}",
                err.trim().chars().take(200).collect::<String>()
            ));
        }
        Ok(())
    } else {
        let out = reg_cmd()
            .args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])
            .output()
            .map_err(|e| format!("移除开机自启项失败: {}", e))?;
        // 删除不存在的值会返回非 0:按「已经是关闭状态」处理,不报错
        // (幂等 —— 用户连点两次「取消自启」不该看到失败提示)
        let _ = out;
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
pub fn set_enabled(_enabled: bool) -> Result<(), String> {
    Err("开机自启当前仅支持 Windows".to_string())
}

/// 把设置里的开关落到注册表(供 `app_settings_set` 调用)。
///
/// **不对账配置与注册表**:设置界面显示的开关值来自 [`is_enabled`]
/// (现读),用户点保存时传的是他看到的那个状态 —— 直接落地即可。
/// 若用户没碰过这个开关,传回的状态与注册表一致,重复写/删无害(幂等)。
#[cfg(target_os = "windows")]
pub fn apply(enabled: bool) -> Result<(), String> {
    if enabled == is_enabled() {
        return Ok(()); // 已一致:省一次系统调用(也避免无谓的注册表写)
    }
    set_enabled(enabled)
}

#[cfg(not(target_os = "windows"))]
pub fn apply(_enabled: bool) -> Result<(), String> {
    Ok(())
}

/// 供 `app_settings_get` 用:把真实自启状态覆盖进返回的设置对象
/// (**注册表是唯一真值**;配置里的字段只作为「上次意图」的备份)。
pub fn with_actual_state(settings: &mut crate::config::AppSettings) {
    settings.auto_start = is_enabled();
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn test_exe_command_is_quoted() {
        // 引号是硬要求:Run 键的值被当命令行解析,未加引号时含空格路径会被截断
        let cmd = exe_command().expect("测试进程必然能定位自身 exe");
        assert!(cmd.starts_with('"'), "必须以引号开头: {}", cmd);
        assert!(cmd.ends_with('"'), "必须以引号结尾: {}", cmd);
        assert!(cmd.len() > 2, "引号内应有路径: {}", cmd);
    }

    #[test]
    fn test_is_enabled_reads_registry_without_panic() {
        // 只验证「能读且不 panic」——真实值取决于本机状态(测试不得依赖环境)
        let _ = is_enabled();
    }

    #[test]
    fn test_apply_is_idempotent_for_current_state() {
        // apply(当前状态) 必须是空操作(不得改动用户机器上的注册表)
        let now = is_enabled();
        assert!(apply(now).is_ok(), "与现状一致时应成功且不做事");
        assert_eq!(is_enabled(), now, "状态不得被改变");
    }
}
