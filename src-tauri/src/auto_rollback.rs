//! 部署失败自动回滚(第二十五批):整栈部署在**健康检查失败**时,自动回滚到
//! 上一份可用归档,把「部署把线上搞挂了」的止损时间从「人去点回滚」压到秒级。
//!
//! ## 设计边界(为什么只做整栈 + 只做健康检查失败)
//!
//! - **只整栈**:归档(releases/<ts> 的 tar + manifest + compose 副本)只在整栈
//!   管线写入(单镜像部署不产归档)→ 单镜像失败时没有可回滚的数据前提。
//! - **只健康检查失败**:该失败意味着"新版本起来了但没通过就绪判定",这是自动
//!   回滚最典型的止损场景;更早步骤的失败(上传中断/load 失败)通常意味着线上
//!   从未被改动,或状态不明(load 到一半),此时自动回滚会**引入**更大的不确定性
//!   —— 交给用户判断。
//! - **不含取消**:用户主动取消是明确意图,不该被自动回滚覆盖。
//!
//! ## 互斥与事件不变量(实现时最易踩的两条)
//!
//! 1. 部署管线**全程持有** `acquire_remote_op()` 的 guard(在 `run_one_deploy_stack`
//!    顶部获取)→ 自动回滚**不能**调用 `rollback_execute_stack`(它会再次
//!    acquire,compare_exchange 必然失败)。故直接调用其内层
//!    `rollback_execute_stack_inner`(不取锁、不 emit 自己的 done 事件)。
//! 2. `deploy-done` 有「**恰好一次**」不变量(管线外层的 `finish_deploy_run` 负责
//!    emit)→ 自动回滚**不能**调 `finish_rollback`(它会 emit 第二帧并发通知)。
//!    回滚结果只以**日志行**的形式并入本次部署的日志流,最终结果仍由部署管线
//!    的失败帧表达(文案里已含「已自动回滚」字样)。
//!
//! 本模块只放**纯函数**(可单测);执行编排在 deploy.rs 内。

/// 自动回滚的触发判定(纯函数)。
///
/// 输入:
/// - `enabled`:设置开关(`auto_rollback_on_failure`,默认 **false** —— 自动回滚
///   会改线上状态,必须用户显式开启;这也保证升级不改变既有行为);
/// - `is_stack`:是否整栈部署(单镜像无归档,见模块注释);
/// - `is_resume`:是否断点续传(续传的语义是"把上次没跑完的跑完",中途失败自动
///   回滚会与用户的续传意图冲突,故不自动回滚);
/// - `error`:本次失败的错误串(取消 → 不触发)。
///
/// 返回是否应当触发自动回滚。
pub(crate) fn should_auto_rollback(
    enabled: bool,
    is_stack: bool,
    is_resume: bool,
    error: &str,
) -> bool {
    if !enabled || !is_stack || is_resume {
        return false;
    }
    // 取消不触发(用户主动中止;判定按错误码,不比对文案)
    if crate::errors::code_of(error) == Some(crate::errors::ErrCode::Cancelled) {
        return false;
    }
    true
}

/// 从归档时间戳列表(新 → 旧)选自动回滚目标(纯函数)。
///
/// `current_ts` = 本次失败部署的时间戳(它的归档可能就是失败的新版本,
/// **必须排除** —— 回滚到刚失败的那一版等于没回滚)。
/// 返回比当前更旧的、最近的一份;没有可回滚目标时返回 None
/// (首次部署:releases 里只有本次这一版,没有"上一版"可用)。
pub(crate) fn pick_rollback_target(current_ts: &str, releases_new_to_old: &[String]) -> Option<String> {
    releases_new_to_old
        .iter()
        .find(|ts| ts.as_str() != current_ts)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== should_auto_rollback =====

    #[test]
    fn test_should_auto_rollback_requires_explicit_opt_in() {
        // 默认关闭:即使整栈、非续传、真实失败,也不触发
        assert!(!should_auto_rollback(false, true, false, "健康检查未通过:web up"));
        // 开启后同样的错误才触发
        assert!(should_auto_rollback(true, true, false, "健康检查未通过:web up"));
    }

    #[test]
    fn test_should_auto_rollback_only_for_stack() {
        // 单镜像部署无归档 → 永不触发(即便开关开启)
        assert!(!should_auto_rollback(true, false, false, "load 失败"));
    }

    #[test]
    fn test_should_auto_rollback_skips_resume() {
        // 续传失败不自动回滚(与用户的续传意图冲突)
        assert!(!should_auto_rollback(true, true, true, "健康检查未通过:x up"));
    }

    #[test]
    fn test_should_auto_rollback_skips_cancel() {
        // 取消(带错误码标记)不触发;普通失败触发
        let canceled = crate::errors::cancelled();
        assert!(!should_auto_rollback(true, true, false, &canceled));
        assert!(should_auto_rollback(true, true, false, "健康检查未通过:db up"));
    }

    // ===== pick_rollback_target =====

    #[test]
    fn test_pick_rollback_target_skips_current() {
        // 归档列表新→旧;当前失败版本在列表中,必须跳过它选下一份
        let releases = vec![
            "20260917-120000".to_string(), // 本次失败的新版本
            "20260916-100000".to_string(), // 上一份可用版本 ← 目标
            "20260915-090000".to_string(),
        ];
        assert_eq!(
            pick_rollback_target("20260917-120000", &releases).as_deref(),
            Some("20260916-100000")
        );
    }

    #[test]
    fn test_pick_rollback_target_none_when_first_deploy() {
        // 首次部署:只有本次这一版 → 无目标
        let releases = vec!["20260917-120000".to_string()];
        assert_eq!(pick_rollback_target("20260917-120000", &releases), None);
        // 空列表同样 None
        assert_eq!(pick_rollback_target("20260917-120000", &[]), None);
    }

    #[test]
    fn test_pick_rollback_target_handles_current_absent_from_list() {
        // 当前 ts 不在列表里(例如本次归档写入失败)→ 取列表首个(最近的旧版)
        let releases = vec!["20260916-100000".to_string(), "20260915-090000".to_string()];
        assert_eq!(
            pick_rollback_target("20260917-120000", &releases).as_deref(),
            Some("20260916-100000")
        );
    }
}
