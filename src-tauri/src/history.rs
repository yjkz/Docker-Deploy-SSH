//! 部署历史模块(Task 1)。
//!
//! 部署记录持久化在 `config/deployments.json`(与 `servers.json` / `projects.json`
//! 同目录的便携布局),以 JSON 数组按时间正序追加;超过 [`MAX_RECORDS`] 条时从
//! 最旧开始裁剪。写入复用 config 层的原子写(`.tmp` 写完 sync 后 rename)模式,
//! 崩溃不会留下截断的 JSON。文件损坏时读取返回空数组并告警(容错,不让 UI 崩)。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 部署模式:单镜像部署。
pub const MODE_SINGLE: &str = "single";
/// 部署模式:compose 整栈部署。
pub const MODE_STACK: &str = "stack";

/// 历史记录上限:超过后从最旧开始裁剪。
const MAX_RECORDS: usize = 200;

/// 单条部署历史记录(`config/deployments.json` 数组元素)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployRecord {
    /// 部署开始时间(本地时间,`%F %T`,如 2026-08-29 12:34:56)
    pub ts: String,
    /// 部署模式:`single`(单镜像)或 `stack`(整栈)
    pub mode: String,
    pub server_name: String,
    pub project_name: String,
    /// 本次部署涉及的镜像引用列表
    pub images: Vec<String>,
    pub success: bool,
    /// 结果消息(成功为"部署完成";失败为中文错误;取消固定"部署已取消")
    pub message: String,
    /// 部署耗时(整秒)
    pub duration_secs: u64,
}

impl DeployRecord {
    /// 按当前本地时间新建记录骨架;`success` / `message` / `duration_secs`
    /// 由部署管线出口填充。
    pub fn new_skeleton(
        mode: &str,
        server_name: &str,
        project_name: &str,
        images: Vec<String>,
    ) -> Self {
        Self {
            ts: chrono::Local::now().format("%F %T").to_string(),
            mode: mode.to_string(),
            server_name: server_name.to_string(),
            project_name: project_name.to_string(),
            images,
            success: false,
            message: String::new(),
            duration_secs: 0,
        }
    }
}

/// `deployments.json` 路径(config 目录下,与 servers.json 同级)。
fn history_path() -> PathBuf {
    crate::config::config_dir().join("deployments.json")
}

/// 追加一条部署记录(读改写;超过 [`MAX_RECORDS`] 条从最旧开始裁剪)。
///
/// 尽力而为:写入失败仅记录告警,不影响部署结果(调用点在 `deploy-done`
/// emit 之后,此时部署已结束)。
pub fn append_record(record: DeployRecord) {
    let mut records = load_history();
    records.push(record);
    if records.len() > MAX_RECORDS {
        let overflow = records.len() - MAX_RECORDS;
        records.drain(0..overflow);
    }
    if let Err(e) = write_records(&records) {
        log::warn!("{}", e);
    }
}

/// 读取全部部署记录(文件内为时间正序;展示层可自行倒序取"最新在前")。
///
/// 文件不存在(从未部署)返回空;文件损坏或读取失败返回空并告警(容错)。
pub fn load_history() -> Vec<DeployRecord> {
    let path = history_path();
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(records) => records,
            Err(e) => {
                log::warn!(
                    "部署历史文件损坏,按空历史处理 ({}): {}",
                    path.display(),
                    e
                );
                Vec::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            log::warn!("读取部署历史失败 ({}): {}", path.display(), e);
            Vec::new()
        }
    }
}

/// 原子写全部记录:config 目录不存在则创建,再走 `.tmp` + rename 原子覆盖
/// (复用 [`crate::config::write_json_atomic`])。
fn write_records(records: &[DeployRecord]) -> Result<(), String> {
    let path = history_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建部署历史目录失败 ({}): {}", parent.display(), e))?;
    }
    crate::config::write_json_atomic(&path, &records)
        .map_err(|e| format!("写入部署历史失败 ({}): {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在全新独立临时目录中执行(DD_CONFIG_DIR 是进程级环境变量,
    /// 与 config / commands 层的环境注入测试共用锁串行执行)。
    fn with_isolated_dir<F: FnOnce(&PathBuf)>(f: F) {
        let _guard = crate::config::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        f(&dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 构造第 idx 条测试记录(ts/duration 随 idx 区分)。
    fn record(idx: usize) -> DeployRecord {
        DeployRecord {
            ts: format!("2026-08-29 10:00:{:02}", idx % 60),
            mode: MODE_SINGLE.into(),
            server_name: format!("服务器{}", idx),
            project_name: format!("项目{}", idx),
            images: vec![format!("img{}:latest", idx)],
            success: true,
            message: "部署完成".into(),
            duration_secs: idx as u64,
        }
    }

    #[test]
    fn test_append_and_load_roundtrip() {
        with_isolated_dir(|_| {
            let r1 = record(1);
            let mut r2 = record(2);
            r2.mode = MODE_STACK.into();
            r2.images = vec!["web:1".into(), "db:16".into()];
            append_record(r1.clone());
            append_record(r2.clone());
            // 文件内按追加顺序(时间正序),字段完整往返
            assert_eq!(load_history(), vec![r1, r2]);
        });
    }

    #[test]
    fn test_load_history_missing_file_returns_empty() {
        with_isolated_dir(|_| {
            assert!(load_history().is_empty());
        });
    }

    #[test]
    fn test_append_trims_oldest_beyond_200() {
        with_isolated_dir(|_| {
            for i in 0..(MAX_RECORDS + 5) {
                append_record(record(i));
            }
            let loaded = load_history();
            assert_eq!(loaded.len(), MAX_RECORDS);
            // 最旧的 5 条(0..5)被裁掉,首条为第 5 条
            assert_eq!(loaded[0].server_name, "服务器5");
            // 最新一条仍在末尾
            assert_eq!(
                loaded.last().unwrap().server_name,
                format!("服务器{}", MAX_RECORDS + 4)
            );
        });
    }

    #[test]
    fn test_corrupted_file_tolerated() {
        with_isolated_dir(|dir| {
            let path = dir.join("config").join("deployments.json");
            std::fs::write(&path, "{not valid json").unwrap();
            // 损坏 → 返回空(不 panic),后续追加以空历史起步自愈
            assert!(load_history().is_empty());
            append_record(record(7));
            let loaded = load_history();
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].server_name, "服务器7");
        });
    }

    #[test]
    fn test_dd_config_dir_isolation() {
        let _guard = crate::config::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir_a = std::env::temp_dir().join(format!("ddtest-history-{}", uuid::Uuid::new_v4()));
        let dir_b = std::env::temp_dir().join(format!("ddtest-history-{}", uuid::Uuid::new_v4()));
        for dir in [&dir_a, &dir_b] {
            std::fs::create_dir_all(dir.join("config")).unwrap();
        }
        std::env::set_var("DD_CONFIG_DIR", dir_a.to_str().unwrap());
        append_record(record(1));
        // 切到另一目录:互不可见
        std::env::set_var("DD_CONFIG_DIR", dir_b.to_str().unwrap());
        assert!(load_history().is_empty());
        append_record(record(2));
        // 切回目录 A:原记录仍在
        std::env::set_var("DD_CONFIG_DIR", dir_a.to_str().unwrap());
        let loaded = load_history();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].server_name, "服务器1");
        std::fs::remove_dir_all(&dir_a).ok();
        std::fs::remove_dir_all(&dir_b).ok();
    }

    #[test]
    fn test_new_skeleton_fills_defaults() {
        let sk = DeployRecord::new_skeleton(MODE_STACK, "srv", "proj", vec!["a:1".into()]);
        assert_eq!(sk.mode, MODE_STACK);
        assert_eq!(sk.server_name, "srv");
        assert_eq!(sk.project_name, "proj");
        assert_eq!(sk.images, vec!["a:1".to_string()]);
        assert!(!sk.success);
        assert_eq!(sk.message, "");
        assert_eq!(sk.duration_secs, 0);
        // ts 为本地时间 %F %T 格式
        let today = chrono::Local::now().format("%F").to_string();
        assert!(sk.ts.starts_with(&today), "ts 应为本地日期开头: {}", sk.ts);
    }
}
