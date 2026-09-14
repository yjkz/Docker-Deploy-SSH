//! 部署模板/预设(第二十批阶段四)。
//!
//! 「服务器 + 项目 + 镜像(单镜像)+ 传输选项 + 版本标题/说明」固化为命名
//! 模板,部署页一键套用;批量部署可直接按模板发起。独立持久化于
//! `config/deploy-profiles.json`(与 servers/projects/notify 三件套互不相干,
//! 读写不参与 CONFIG_LOCK——模板不含密文,丢了大不了重存)。
//!
//! 低耦合:新文件新命令;零修改既有部署管线(模板只是「套用到表单」的
//! 数据源,发起部署仍走既有的 startDeploy/startStackDeploy)。

use serde::{Deserialize, Serialize};

use crate::config::{config_dir, write_json_atomic};

// ===== 数据结构 =====

/// 一份部署模板(camelCase,与前端直接互通)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DeployProfile {
    /// 模板 id(uuid,前端生成)
    pub id: String,
    /// 模板名(用户起,如「夜间发版-生产」)
    pub name: String,
    /// 模式:"single" | "stack"
    pub mode: String,
    /// 单镜像:镜像引用 repo:tag;整栈:空串(镜像由项目 compose 决定)
    pub image_ref: String,
    /// 默认服务器 id / 项目 id(下拉自动带出,不锁定)
    pub server_id: String,
    pub project_id: String,
    /// 单镜像:日期标签(每次全新 tag,恒全量传输)
    pub use_date_tag: bool,
    /// 单镜像/整栈共用:跳过未变化镜像(智能传输)
    pub skip_unchanged: bool,
    /// 整栈:强制留档(未变化服务也打包进 release 供回滚)
    pub force_archive: bool,
    /// 部署前自动预览勾选(第十九批;模板一并记忆)
    pub auto_preview: bool,
    /// 版本标题/说明(第十一批;预填到整栈选项区)
    pub release_title: String,
    pub release_notes: String,
    /// 创建时间(展示排序用,%F %T)
    pub created_at: String,
}

impl Default for DeployProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            mode: "single".into(),
            image_ref: String::new(),
            server_id: String::new(),
            project_id: String::new(),
            use_date_tag: false,
            skip_unchanged: true,
            force_archive: false,
            auto_preview: false,
            release_title: String::new(),
            release_notes: String::new(),
            created_at: String::new(),
        }
    }
}

/// 模板文件路径(`config/deploy-profiles.json`;config 目录不存在时读空表)。
fn profiles_path() -> std::path::PathBuf {
    config_dir().join("deploy-profiles.json")
}

/// 读取全部模板(缺失/损坏回退空表并告警 —— 模板非关键数据,不阻断部署页)。
fn load_profiles() -> Vec<DeployProfile> {
    match std::fs::read(profiles_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            log::warn!("部署模板文件损坏,按空表处理: {}", e);
            Vec::new()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            log::warn!("读取部署模板失败,按空表处理: {}", e);
            Vec::new()
        }
    }
}

/// 原子写全部模板(上限 20,超出按 created_at 旧的丢弃)。
fn save_profiles(list: &[DeployProfile]) -> Result<(), String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败: {}", e))?;
    write_json_atomic(&profiles_path(), &list.to_vec()).map_err(|e| format!("写入模板失败: {}", e))
}

// ===== Tauri 命令 =====

/// 上限:再多就该整理了(防手改配置写入超长清单)。
const MAX_PROFILES: usize = 20;

/// 读取全部部署模板(创建时间倒序 = 最新在前)。
#[tauri::command]
pub fn deploy_profiles_list() -> Result<Vec<DeployProfile>, String> {
    let mut list = load_profiles();
    list.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(list)
}

/// 新增或更新一条模板(按 `id` 定位:存在替换,不存在追加)。
/// 名称非空、模式合法;超上限时按 created_at 旧的先丢弃。
#[tauri::command]
pub fn deploy_profiles_save(mut profile: DeployProfile) -> Result<(), String> {
    let name = profile.name.trim().to_string();
    if name.is_empty() {
        return Err("模板名不能为空".into());
    }
    if profile.id.trim().is_empty() {
        return Err("模板 id 缺失".into());
    }
    if profile.mode != "single" && profile.mode != "stack" {
        return Err(format!("未知的部署模式: {}", profile.mode));
    }
    profile.name = name;
    let id = profile.id.clone();
    let mut list = load_profiles();
    match list.iter().position(|p| p.id == id) {
        Some(i) => list[i] = profile, // 更新:保留原 created_at(在调用方传入)
        None => list.push(profile),
    }
    if list.len() > MAX_PROFILES {
        list.sort_by(|a, b| b.created_at.cmp(&a.created_at)); // 新→旧
        list.truncate(MAX_PROFILES); // 丢最旧
    }
    save_profiles(&list)
}

/// 删除一条模板(按 id;不存在也返回 Ok,幂等)。
#[tauri::command]
pub fn deploy_profiles_delete(id: String) -> Result<(), String> {
    let mut list = load_profiles();
    let before = list.len();
    list.retain(|p| p.id != id);
    if list.len() != before {
        save_profiles(&list)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// roundtrip + 上限裁剪 + 幂等删除(注入临时 config 目录)。
    #[test]
    fn test_profiles_save_list_delete_roundtrip() {
        let _guard = crate::config::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-profiles-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 空 → 空
        assert!(deploy_profiles_list().unwrap().is_empty());

        // 存两条(不同 created_at,旧的排后)
        let mut p1 = DeployProfile {
            id: "t1".into(),
            name: "夜间发版".into(),
            mode: "stack".into(),
            ..Default::default()
        };
        p1.created_at = "2026-09-14 10:00:00".into();
        let mut p2 = DeployProfile {
            id: "t2".into(),
            name: "快速单发".into(),
            mode: "single".into(),
            image_ref: "app:latest".into(),
            use_date_tag: true,
            ..Default::default()
        };
        p2.created_at = "2026-09-14 11:00:00".into();
        deploy_profiles_save(p1.clone()).unwrap();
        deploy_profiles_save(p2.clone()).unwrap();

        // 列表:新在前,字段 roundtrip 完整
        let list = deploy_profiles_list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "t2");
        assert_eq!(list[0].image_ref, "app:latest");
        assert!(list[0].use_date_tag);
        assert_eq!(list[1].id, "t1");
        assert_eq!(list[1].name, "夜间发版");
        assert_eq!(list[1].mode, "stack");

        // 更新同名 id:替换不追加
        let mut p1b = p1.clone();
        p1b.name = "夜间发版-改".into();
        deploy_profiles_save(p1b).unwrap();
        let list = deploy_profiles_list().unwrap();
        assert_eq!(list.len(), 2, "同 id 替换不追加");
        assert!(list.iter().any(|p| p.name == "夜间发版-改"));

        // 非法输入拒绝
        assert!(deploy_profiles_save(DeployProfile {
            name: "  ".into(),
            ..Default::default()
        })
        .is_err());
        assert!(deploy_profiles_save(DeployProfile {
            id: "x".into(),
            name: "n".into(),
            mode: "other".into(),
            ..Default::default()
        })
        .is_err());

        // 删除(幂等)
        deploy_profiles_delete("t1".into()).unwrap();
        assert_eq!(deploy_profiles_list().unwrap().len(), 1);
        deploy_profiles_delete("t1".into()).unwrap(); // 再删不报错
        assert_eq!(deploy_profiles_list().unwrap().len(), 1);

        std::env::remove_var("DD_CONFIG_DIR");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 上限 20:超出的最旧条目被裁剪。
    #[test]
    fn test_profiles_cap_trims_oldest() {
        let _guard = crate::config::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-profiles-cap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        for i in 0..(MAX_PROFILES + 3) {
            let mut p = DeployProfile {
                id: format!("cap-{}", i),
                name: format!("模板{}", i),
                ..Default::default()
            };
            // 0 最旧 → 应被裁掉;MAX_PROFILES+2 最新 → 保留
            p.created_at = format!("2026-09-14 {:02}:00:00", i);
            deploy_profiles_save(p).unwrap();
        }
        let list = deploy_profiles_list().unwrap();
        assert_eq!(list.len(), MAX_PROFILES, "超上限裁剪到 20");
        assert!(
            !list.iter().any(|p| p.id == "cap-0"),
            "最旧的 cap-0 应被裁掉"
        );
        assert!(
            !list.iter().any(|p| p.id == "cap-1"),
            "次旧的 cap-1 也应被裁掉"
        );
        assert!(
            !list.iter().any(|p| p.id == "cap-2"),
            "第三旧的 cap-2 也应被裁掉"
        );
        assert!(list.iter().any(|p| p.id == "cap-3"), "cap-3 起保留");

        std::env::remove_var("DD_CONFIG_DIR");
        std::fs::remove_dir_all(&dir).ok();
    }
}
