//! 托盘状态与 tooltip 文本(第十五批:托盘 tooltip 动态态)。
//!
//! 背景:应用支持「关闭到托盘」(settings.close_to_tray)与部署完成后的自动重启,
//! 窗口隐藏时**托盘是用户唯一能看到的界面**。原实现 tooltip 是编译期写死的
//! `"DockerDeploy SSH"`(见 lib.rs 的 TrayIconBuilder),窗口一关就完全看不到
//! 部署进度/成败(见 wiki/07 限制 19)。
//!
//! 设计要点:
//! - **状态由后端自己维护**,不依赖前端上报 —— 窗口隐藏、前端卡死时 tooltip 仍准确;
//! - tooltip 文本由**纯函数** [`tooltip_text`] 生成(便于单测覆盖各种状态组合);
//! - 通过 [`set_deploy_status`] / [`set_monitor_status`] 更新,内部淡化为
//!   `tray.set_tooltip()` 调用,托盘不存在(如非桌面平台/构建失败)时静默忽略。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// 托盘前缀(空闲时的完整 tooltip,也是运行态的固定前缀)。
const TRAY_BASE: &str = "DockerDeploy SSH";

/// 部署运行态快照(用于生成 tooltip)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DeployStatus {
    /// 部署进行中
    pub running: bool,
    /// 部署模式:true = 整栈(compose),false = 单镜像(仅 running 时有意义)
    pub stack: bool,
    /// 当前步骤(1-based;running 且 >0 时展示)
    pub step: u8,
    /// 总步骤数
    pub total: u8,
    /// 目标描述(如「生产服务器 / 我的应用」;空则不展示)
    pub target: String,
    /// 批次前缀:批量部署时为「批量 N/M」,普通部署为空
    pub batch: String,
}

/// 最近一次部署的终态:成功 / 失败 / 取消(空 = 尚未有结果)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum LastOutcome {
    #[default]
    None,
    Success,
    Failed,
    Canceled,
}

impl LastOutcome {
    /// 终态在 tooltip 上的后缀(无结果时为空串)
    fn suffix(&self) -> &'static str {
        match self {
            LastOutcome::None => "",
            LastOutcome::Success => " — 上次部署成功",
            LastOutcome::Failed => " — 上次部署失败",
            LastOutcome::Canceled => " — 上次部署已取消",
        }
    }
}

/// 托盘状态快照(crate 内可见:与 [`tooltip_text`] 的可见性配套,
/// 避免 private_interfaces 警告;字段仅由本模块的 set_* 接口改写)
#[derive(Debug, Default)]
pub(crate) struct Status {
    deploy: DeployStatus,
    outcome: LastOutcome,
    /// 回滚执行中(一键回滚 / 独立回滚中心;复用 deploy-done 事件体系)
    rolling_back: bool,
    /// 远程管理实时监控进行中(manage_stats 流)
    monitoring: bool,
    /// 监控中的服务器名(空则不展示)
    monitor_server: String,
}

static STATUS: Mutex<Option<Status>> = Mutex::new(None);

/// 托盘是否已就绪:未就绪时状态照常记录,但不调 set_tooltip
/// (lib.rs 建托盘成功后置位;前端可能早于托盘初始化发命令)
static TRAY_READY: AtomicBool = AtomicBool::new(false);

/// 生成 tooltip 文本(纯函数,便于单测;crate 内可见 —— `Status` 是私有
/// 类型,`pub` 会触发 private_interfaces 警告)。
///
/// 优先级:部署中 > 回滚执行中 > 监控中 > 空闲;部署/监控都不在时,若最近一次部署有终态
/// 则附在空闲文案后(用户关窗后回来第一眼能看到"上次成没成")。
///
/// 形态示例:
/// - `DockerDeploy SSH`
/// - `DockerDeploy SSH — 部署中 生产服务器 / 我的应用(步骤 3/5)`
/// - `DockerDeploy SSH — 批量部署 2/4 · 部署中(步骤 1/6)`
/// - `DockerDeploy SSH — 监控中 生产服务器`
/// - `DockerDeploy SSH — 上次部署失败`
pub(crate) fn tooltip_text(s: &Status) -> String {
    // 部署优先级最高(关键路径);回滚次之(与部署互斥由应用层保证,此处
    // 顺序是防御性兜底:若两态异常并存,展示更关键的部署)
    if s.deploy.running {
        let kind = if s.deploy.stack { "整栈部署" } else { "部署" };
        // 批量前缀优先展示批次进度(用户最关心"还剩几台")
        let head = if s.deploy.batch.is_empty() {
            format!("{kind}中")
        } else {
            format!("{} · {kind}中", s.deploy.batch)
        };
        let target = if s.deploy.target.is_empty() {
            String::new()
        } else {
            format!(" {}", s.deploy.target)
        };
        let step = if s.deploy.total > 0 {
            format!("(步骤 {}/{})", s.deploy.step, s.deploy.total)
        } else {
            String::new()
        };
        return format!("{TRAY_BASE} — {head}{target}{step}");
    }
    if s.rolling_back {
        return format!("{TRAY_BASE} — 回滚执行中(期间服务会短暂重启)");
    }
    if s.monitoring {
        let who = if s.monitor_server.is_empty() {
            String::new()
        } else {
            format!(" {}", s.monitor_server)
        };
        return format!("{TRAY_BASE} — 监控中{who}");
    }
    format!("{TRAY_BASE}{}", s.outcome.suffix())
}

/// 取状态锁并改写;返回改写后生成的 tooltip 文本(锁内算好,避免二次加锁)。
fn with_status<F>(f: F) -> String
where
    F: FnOnce(&mut Status),
{
    let mut guard = match STATUS.lock() {
        Ok(g) => g,
        // 中毒(PoisonError)时取回内部值继续:tooltip 是装饰性信息,不该因
        // 一次 panic 永久失效(与项目内其他锁的容错口径一致)
        Err(e) => e.into_inner(),
    };
    let status = guard.get_or_insert_with(Status::default);
    f(status);
    tooltip_text(status)
}

/// 把文本写进托盘 tooltip(托盘未就绪时静默忽略)。
fn apply(app: &tauri::AppHandle, text: &str) {
    if !TRAY_READY.load(Ordering::SeqCst) {
        return;
    }
    if let Some(tray) = app.tray_by_id("main-tray") {
        if let Err(e) = tray.set_tooltip(Some(text)) {
            log::warn!("设置托盘 tooltip 失败: {e}");
        }
    }
}

/// 标记托盘已就绪,并用当前状态刷新一次 tooltip(lib.rs 建完托盘后调用)。
pub fn mark_ready(app: &tauri::AppHandle) {
    TRAY_READY.store(true, Ordering::SeqCst);
    refresh(app);
}

/// 按当前状态刷新 tooltip(不改变状态)。
pub fn refresh(app: &tauri::AppHandle) {
    let text = with_status(|_| {});
    apply(app, &text);
}

/// 部署开始(单发/续传/每台批量):置运行态并刷新。
pub fn set_deploy_running(
    app: &tauri::AppHandle,
    stack: bool,
    step: u8,
    total: u8,
    target: &str,
    batch: &str,
) {
    let text = with_status(|s| {
        s.deploy = DeployStatus {
            running: true,
            stack,
            step,
            total,
            target: target.to_string(),
            batch: batch.to_string(),
        };
        // 新一轮开始:清掉上一次的终态(否则运行中文案会带上"上次失败")
        s.outcome = LastOutcome::None;
    });
    apply(app, &text);
}

/// 部署进度更新(emit_deploy_progress 内部调用)
pub fn set_deploy_step(app: &tauri::AppHandle, step: u8, total: u8) {
    let text = with_status(|s| {
        if !s.deploy.running {
            return;
        }
        s.deploy.step = step;
        s.deploy.total = total;
    });
    apply(app, &text);
}

/// 部署收尾(成功/失败/取消):清运行态并记录终态。
pub fn set_deploy_finished(app: &tauri::AppHandle, success: bool, canceled: bool) {
    let text = with_status(|s| {
        s.deploy.running = false;
        s.outcome = if success {
            LastOutcome::Success
        } else if canceled {
            LastOutcome::Canceled
        } else {
            LastOutcome::Failed
        };
    });
    apply(app, &text);
}

/// 回滚开始/结束(一键回滚与独立回滚中心共用;回滚复用 deploy-done 事件
/// 体系但不走 deploy 管线的进度事件,故独立挂状态)。
pub fn set_rolling_back(app: &tauri::AppHandle, running: bool) {
    let text = with_status(|s| {
        s.rolling_back = running;
        if running {
            // 回滚开始同样清上次终态,避免「回滚执行中 … 上次失败」的歧义
            s.outcome = LastOutcome::None;
        }
    });
    apply(app, &text);
}

/// 实时监控开始/停止。
pub fn set_monitor(app: &tauri::AppHandle, running: bool, server_name: &str) {
    let text = with_status(|s| {
        s.monitoring = running;
        s.monitor_server = if running {
            server_name.to_string()
        } else {
            String::new()
        };
    });
    apply(app, &text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Status {
        Status::default()
    }

    #[test]
    fn test_tooltip_idle_is_bare_base() {
        assert_eq!(tooltip_text(&base()), "DockerDeploy SSH");
    }

    #[test]
    fn test_tooltip_deploy_with_target_and_step() {
        let s = Status {
            deploy: DeployStatus {
                running: true,
                stack: false,
                step: 3,
                total: 5,
                target: "生产服务器 / 我的应用".into(),
                batch: String::new(),
            },
            ..base()
        };
        assert_eq!(
            tooltip_text(&s),
            "DockerDeploy SSH — 部署中 生产服务器 / 我的应用(步骤 3/5)"
        );
    }

    #[test]
    fn test_tooltip_stack_deploy_label() {
        let s = Status {
            deploy: DeployStatus {
                running: true,
                stack: true,
                step: 1,
                total: 6,
                target: "srv".into(),
                batch: String::new(),
            },
            ..base()
        };
        assert!(tooltip_text(&s).contains("整栈部署中"), "{}", tooltip_text(&s));
    }

    #[test]
    fn test_tooltip_batch_prefix_takes_precedence_in_text() {
        let s = Status {
            deploy: DeployStatus {
                running: true,
                stack: true,
                step: 1,
                total: 6,
                target: "srv".into(),
                batch: "批量 2/4".into(),
            },
            ..base()
        };
        assert_eq!(
            tooltip_text(&s),
            "DockerDeploy SSH — 批量 2/4 · 整栈部署中 srv(步骤 1/6)"
        );
    }

    #[test]
    fn test_tooltip_monitor_shown_when_idle() {
        let s = Status {
            monitoring: true,
            monitor_server: "生产服务器".into(),
            ..base()
        };
        assert_eq!(tooltip_text(&s), "DockerDeploy SSH — 监控中 生产服务器");
    }

    #[test]
    fn test_tooltip_deploy_beats_monitor() {
        // 部署优先级高于监控:部署是关键路径,监控是旁路
        let s = Status {
            deploy: DeployStatus {
                running: true,
                stack: false,
                step: 2,
                total: 5,
                target: String::new(),
                batch: String::new(),
            },
            monitoring: true,
            monitor_server: "srv".into(),
            ..base()
        };
        let text = tooltip_text(&s);
        assert!(text.contains("部署中"), "{}", text);
        assert!(!text.contains("监控中"), "{}", text);
    }

    #[test]
    fn test_tooltip_outcome_suffix_when_idle() {
        for (outcome, expect) in [
            (LastOutcome::Success, "DockerDeploy SSH — 上次部署成功"),
            (LastOutcome::Failed, "DockerDeploy SSH — 上次部署失败"),
            (LastOutcome::Canceled, "DockerDeploy SSH — 上次部署已取消"),
        ] {
            let s = Status { outcome, ..base() };
            assert_eq!(tooltip_text(&s), expect);
        }
    }

    #[test]
    fn test_tooltip_outcome_hidden_while_running() {
        // 运行中不展示上次终态(避免「部署中 … 上次失败」的自相矛盾文案)
        let s = Status {
            deploy: DeployStatus {
                running: true,
                stack: false,
                step: 1,
                total: 5,
                target: String::new(),
                batch: String::new(),
            },
            outcome: LastOutcome::Failed,
            ..base()
        };
        let text = tooltip_text(&s);
        assert!(!text.contains("上次"), "{}", text);
    }

    #[test]
    fn test_tooltip_rolling_back() {
        let mut s = base();
        s.rolling_back = true;
        assert_eq!(tooltip_text(&s), "DockerDeploy SSH — 回滚执行中(期间服务会短暂重启)");
        // 回滚优先级低于部署(部署是关键路径;互斥由应用层保证,防御性兜底)
        s.deploy.running = true;
        s.deploy.target = "srv / prj".into();
        let text = tooltip_text(&s);
        assert!(text.contains("部署中"), "{}", text);
        assert!(!text.contains("回滚"), "{}", text);
    }

    #[test]
    fn test_tooltip_deploy_without_step_total() {
        // total = 0(步骤信息未知)时不渲染步骤括号,避免出现「步骤 0/0」
        let s = Status {
            deploy: DeployStatus {
                running: true,
                stack: false,
                step: 0,
                total: 0,
                target: "srv / prj".into(),
                batch: String::new(),
            },
            ..base()
        };
        assert_eq!(tooltip_text(&s), "DockerDeploy SSH — 部署中 srv / prj");
    }
}
