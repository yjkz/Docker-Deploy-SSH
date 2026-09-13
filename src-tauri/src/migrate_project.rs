//! 项目跨服务器迁移(服务器 A → 服务器 B)。
//!
//! 解决的问题:把一个部署项目从一台服务器整体搬到另一台 —— 镜像、compose
//! 三件套、数据卷、发布归档一并搬运,并把项目配置改绑到新服务器。与阶段十的
//! **镜像迁移**([`crate::commands::migrate_images`],只搬镜像)的区别在于本
//! 模块以**项目**为单位,额外处理数据卷与归档,并负责改绑配置与写迁移历史。
//!
//! ## 执行时序(停机窗口最小化)
//!
//! ```text
//! ① 连源/目标
//! ② 【停服】源: compose stop          ← 停机窗口开始
//! ③ 【导出】源: 逐卷 tar 到源临时目录
//! ④ 【恢复】源: compose start         ← 停机窗口结束(长传输在源运行中做)
//! ⑤ 【传输】镜像/text>卷包/归档:源 → 本机中转 → 目标
//! ⑥ 【装载】目标: docker load + 卷导入(按目标侧卷名)
//! ⑦ 【文件】目标: compose 三件套 + releases 归档落盘
//! ⑧ 【启服】目标: compose up -d + 健康检查
//! ⑨ 清理两端临时产物
//! ⑩ 改绑 default_server_id + 写迁移历史
//! ```
//!
//! **源恢复必须无条件执行**:③④ 被包成一个单元,任何导出失败路径都先尝试
//! `compose start` 再返回错误,否则用户的服务会停在停止状态。
//!
//! ## 已知取舍(详见 wiki 07)
//!
//! - 保留源服务器 + 自动启动目标 ⇒ 迁移后两边同时运行。卷数据是「导出时刻」
//!   的快照,源后续写入不回传,建议尽快停用源;确认页有明确提示。
//! - `external: true` 卷与宿主绝对路径挂载**不搬**(前者可能是多项目共享,
//!   后者可能是系统文件),只列警告由用户决定。
//! - 卷导出/导入依赖临时容器执行 `tar`(宿主机不一定有 tar,`docker run -v`
//!   是唯一稳妥路径),候选链见 [`pick_tar_image`]。

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::commands::{
    self, connect_server, exec_forwarded_via_event, format_log_line, query_remote_image_id_map,
    save_gzip_remote, same_image_id, shell_single_quote, with_timeout, TempFileGuard,
    SSH_EXEC_TIMEOUT_SECS, STACK_COMPOSE_TIMEOUT_SECS,
};use crate::config::{self, ProjectConfig, ServerConfig};
use crate::ssh::{exec_collect, mkdir_p_cmd, SshClient};
use crate::stack::{self, VolumeKind};

/// 源/目标临时目录(搬运中转):两端都在此落盘,收尾统一清理。
const MIGRATE_TMP_DIR: &str = "/tmp/dd-migrate-project";

/// 单条卷/归档导出的超时(GB 级数据需要余量)。
const VOLUME_TAR_TIMEOUT_SECS: u64 = 1800;

/// 卷 tar 的候选镜像:按序探测,首个「存在且含 tar」者胜出。
/// busybox 约 2MB 且必带 tar;项目自身镜像作为兜底(distroless 除外)。
const TAR_IMAGE_CANDIDATES: [&str; 3] = ["busybox:latest", "alpine:latest", "ubuntu:latest"];

/// 归档搬运上限兜底(与请求值夹取,防手改配置传入过大值)。
const RELEASE_COUNT_MAX: u32 = 20;

// ===== 事件与请求/返回契约(camelCase)=====

/// `migrate-project-log` 事件载荷(camelCase)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateProjectLogEvent {
    pub migrate_id: u64,
    pub line: String,
}

/// `migrate-project-done` 事件载荷(camelCase)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateProjectDoneEvent {
    pub migrate_id: u64,
    pub success: bool,
    pub message: String,
    /// 执行过程中收集的非致命问题(警告清单,供结果页展示)
    pub warnings: Vec<String>,
}

/// `migrate_project_start` 的请求参数(camelCase)。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateProjectRequest {
    pub project_id: String,
    pub source_server_id: String,
    pub target_server_id: String,
    /// 是否搬运数据卷(默认 false)
    #[serde(default)]
    pub include_volumes: bool,
    /// 搬运最近 N 个发布归档(0 = 不搬;上限 [`RELEASE_COUNT_MAX`])
    #[serde(default)]
    pub release_count: u32,
    /// 目标服务器部署目录(留空 = 沿用项目/源服务器的目录解析结果)
    #[serde(default)]
    pub target_remote_dir: Option<String>,
    pub source_password_plain: Option<String>,
    pub target_password_plain: Option<String>,
}

/// 预检返回的迁移计划(camelCase;只读,前端据此出确认页)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateProjectPlan {
    pub project_id: String,
    pub project_name: String,
    pub source_server_name: String,
    pub target_server_name: String,
    /// 源服务器上的实际部署目录
    pub source_remote_dir: String,
    /// 目标服务器将要使用的部署目录
    pub target_remote_dir: String,
    /// 待搬运镜像(repo:tag)
    pub images: Vec<PlanImage>,
    /// 待搬运数据卷(仅可搬运项)
    pub volumes: Vec<PlanVolume>,
    /// 待搬运发布归档(新 → 旧)
    pub releases: Vec<PlanRelease>,
    /// 汇总体积(字节;取不到的体积为 None,不参与求和)
    pub total_bytes: Option<u64>,
    /// 非致命问题(不阻断,确认页显著展示)
    pub warnings: Vec<String>,
    /// 阻断性问题(非空则不允许执行)
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanImage {
    pub reference: String,
    /// 源服务器上是否已确认存在
    pub exists_on_source: bool,
    /// 目标服务器上是否已有同 ID 镜像(存在则自动跳过传输)
    pub already_on_target: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanVolume {
    /// 源服务器上的实际卷名(命名卷)或相对路径原文(项目内目录)
    pub source_key: String,
    /// 目标服务器上将要使用的卷名(命名卷按目标侧项目名重新推导)
    pub target_key: String,
    /// "Named" | "BindRelative"
    pub kind: String,
    /// 人类可读体积(取不到为 None)
    pub size: Option<String>,
    /// 卷名在两台机器上不一致(仍会按目标侧名称导入,仅提示)
    pub renamed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRelease {
    pub ts: String,
    pub size: Option<String>,
    pub services: Vec<String>,
}

// ===== 命令 =====

/// 迁移预检(**只读**):连源服务器解析实际部署状态,汇总待搬运内容与警告。
///
/// compose 来源以**源服务器上实际部署的**为准(`<源部署目录>/docker-compose.yml`
/// 及同目录 `.env`/override),而不是本机副本 —— 目标要复现的是源真正在跑的
/// 状态;手工项目(compose_file 是远端相对路径、本机无副本)也由此覆盖。
#[tauri::command]
pub async fn migrate_project_preview(
    project_id: String,
    source_server_id: String,
    target_server_id: String,
    include_volumes: Option<bool>,
    release_count: Option<u32>,
    source_password_plain: Option<String>,
) -> Result<MigrateProjectPlan, String> {
    if source_server_id == target_server_id {
        return Err("源服务器与目标服务器不能相同".to_string());
    }
    let cfg = config::load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let project = commands::find_project(&cfg, &project_id)?.clone();
    let source = commands::find_server_pub(&cfg, &source_server_id)?.clone();
    let target = commands::find_server_pub(&cfg, &target_server_id)?.clone();

    let src_dir = commands::effective_remote_dir(&source, &project);
    let tgt_dir = resolve_target_dir(&project, &target, None);

    let (_srv, mut client) = connect_server(&source_server_id, source_password_plain.as_deref(), None)
        .await?;

    build_plan(
        &mut client,
        &project,
        &source,
        &target,
        &src_dir,
        &tgt_dir,
        include_volumes.unwrap_or(false),
        release_count.unwrap_or(1),
    )
    .await
}

/// 执行项目迁移(立即返回,进度/结果只经事件)。
#[tauri::command]
pub fn migrate_project_start(
    app: AppHandle,
    migrate_state: tauri::State<'_, commands::MigrateState>,
    req: MigrateProjectRequest,
) -> Result<(), String> {
    if req.source_server_id == req.target_server_id {
        return Err("源服务器与目标服务器不能相同".to_string());
    }
    let cfg = config::load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    commands::find_project(&cfg, &req.project_id)?;
    commands::find_server_pub(&cfg, &req.source_server_id)?;
    commands::find_server_pub(&cfg, &req.target_server_id)?;

    let migrate_id = migrate_state.begin_migration();
    log::info!(
        "项目迁移启动:项目 {} {} -> {}",
        req.project_id,
        req.source_server_id,
        req.target_server_id
    );
    let state = migrate_state.inner_arc();

    tauri::async_runtime::spawn(async move {
        let (success, message, warnings) =
            match commands::CatchPanic::new(run_migrate_project(&app, &state, migrate_id, &req)).await
            {
                Ok(Ok(warnings)) => (true, "迁移完成".to_string(), warnings),
                Ok(Err((msg, warnings))) => (false, msg, warnings),
                Err(panic_info) => {
                    log::error!("项目迁移任务发生 panic: {}", panic_info);
                    (
                        false,
                        "项目迁移因内部错误中止,详情见日志".to_string(),
                        Vec::new(),
                    )
                }
            };
        let _ = app.emit(
            "migrate-project-done",
            MigrateProjectDoneEvent {
                migrate_id,
                success,
                message,
                warnings,
            },
        );
    });
    Ok(())
}

// ===== 目标目录解析 =====

/// 解析目标服务器上的部署目录。
///
/// 优先级:本次请求显式指定 → 项目的项目级 `remote_dir`(它是**与服务器无关**的
/// 绝对路径,迁移后仍应沿用)→ 目标服务器的 `remote_dir`。落空时用源目录,
/// 保证「同名卷名推导」在多数场景下与源一致。
pub fn resolve_target_dir(
    project: &ProjectConfig,
    target: &ServerConfig,
    explicit: Option<&str>,
) -> String {
    if let Some(dir) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return dir.to_string();
    }
    if let Some(dir) = project
        .remote_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return dir.to_string();
    }
    target.remote_dir.clone()
}

/// 目录末段名(Docker Compose 用它拼默认卷名 `<项目目录名>_<卷名>`)。
pub fn dir_basename(dir: &str) -> String {
    dir.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// 推导某个命名卷在两台机器上的实际名(纯函数,便于单测)。
///
/// Compose 的默认卷名是 `<项目名>_<卷名>`,项目名取**部署目录末段名**
/// (本应用按目录部署,`-p` 未被使用)。两机目录名不同则卷名不同 —— 数据本身
/// 不关心名字,但目标侧必须按**目标自己的卷名**创建,compose 才能找到它。
pub fn resolve_volume_names(
    source_dir: &str,
    target_dir: &str,
    declared_name: Option<&str>,
    volume_key: &str,
) -> (String, String, bool) {
    let resolve = |dir: &str| match declared_name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => format!("{}_{}", dir_basename(dir), volume_key),
    };
    let src = resolve(source_dir);
    let tgt = resolve(target_dir);
    let renamed = src != tgt;
    (src, tgt, renamed)
}

// ===== 预检 =====

#[allow(clippy::too_many_arguments)]
async fn build_plan(
    client: &mut SshClient,
    project: &ProjectConfig,
    source: &ServerConfig,
    target: &ServerConfig,
    src_dir: &str,
    tgt_dir: &str,
    include_volumes: bool,
    release_count: u32,
) -> Result<MigrateProjectPlan, String> {
    let mut warnings: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // ---- 1. 源服务器上的 compose(实际部署状态的唯一真相)----
    let src_compose = commands::remote_join(src_dir, "docker-compose.yml");
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "检查源 compose 超时",
        "请检查服务器网络后重试",
        exec_collect(client, &commands::test_file_cmd(&src_compose)),
    )
    .await?;
    if code != 0 {
        errors.push(format!(
            "源服务器上找不到部署 compose({}),该服务器可能没有部署过此项目",
            src_compose
        ));
    }

    // 取回 compose 文本并在本地临时目录解析(复用既有解析器,不重复实现)
    let mut images: Vec<PlanImage> = Vec::new();
    let mut volume_mounts: Vec<stack::VolumeMount> = Vec::new();
    if code == 0 {
        let (code, content) = with_timeout(
            SSH_EXEC_TIMEOUT_SECS,
            "读取源 compose 超时",
            "请检查服务器网络后重试",
            exec_collect(client, &commands::cat_file_cmd(&src_compose)),
        )
        .await?;
        if code != 0 {
            errors.push(format!("读取源 compose 失败:{}", src_compose));
        } else {
            let tmp_dir = std::env::temp_dir().join(format!("dd-plan-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&tmp_dir)
                .map_err(|e| format!("创建本地临时目录失败: {}", e))?;
            let tmp_compose = tmp_dir.join("docker-compose.yml");
            std::fs::write(&tmp_compose, &content)
                .map_err(|e| format!("写入本地临时 compose 失败: {}", e))?;

            // 服务镜像清单(本地无镜像也可解析:传空列表即可)
            match stack::parse_compose_file(&tmp_compose, &[]) {
                Ok(stack) => {
                    for svc in &stack.services {
                        let Some(image) = svc.image.clone() else {
                            // 未设 image 且本机无 match(build-only)时无法搬运,
                            // 目标侧会由 compose 自行 build/carry,这里只提示
                            warnings.push(format!(
                                "服务「{}」未声明 image,其镜像不参与搬运(需在目标服务器构建)",
                                svc.service
                            ));
                            continue;
                        };
                        images.push(PlanImage {
                            reference: image,
                            exists_on_source: false,
                            already_on_target: false,
                        });
                    }
                }
                Err(e) => errors.push(format!("解析源 compose 失败:{}", e)),
            }

            // 卷清单
            if include_volumes {
                match stack::parse_compose_volumes(&tmp_compose, &dir_basename(src_dir)) {
                    Ok(mounts) => volume_mounts = mounts,
                    Err(e) => warnings.push(format!("解析 compose 卷定义失败,卷将不搬运:{}", e)),
                }
            }

            let _ = std::fs::remove_dir_all(&tmp_dir);
        }
    }

    // ---- 2. 源侧镜像存在性(一次 docker images 全量,避免逐镜像往返)----
    if !images.is_empty() {
        match query_remote_image_id_map(client).await {
            Ok(map) => {
                for img in images.iter_mut() {
                    img.exists_on_source = map.contains_key(&img.reference);
                    if !img.exists_on_source {
                        warnings.push(format!(
                            "源服务器上不存在镜像 {},该服务需在目标服务器重新构建或拉取",
                            img.reference
                        ));
                    }
                }
            }
            Err(e) => warnings.push(format!("查询源服务器镜像列表失败,存在性未校验:{}", e)),
        }
    }

    // ---- 3. 卷体积与可搬运性 ----
    let mut volumes: Vec<PlanVolume> = Vec::new();
    let mut total: u64 = 0;
    let mut has_size = false;
    if include_volumes && !volume_mounts.is_empty() {
        let specs = stack::dedupe_volume_specs(&volume_mounts);
        // 体积:一次性 du 全部源侧卷(命名卷用卷名,相对目录用绝对路径)
        let source_keys: Vec<String> = specs
            .iter()
            .map(|s| source_key_of(s, src_dir))
            .collect();
        let sizes = query_path_sizes(client, &source_keys).await;

        for (i, spec) in specs.iter().enumerate() {
            match spec.kind {
                VolumeKind::Named => {
                    let resolved = spec
                        .resolved_name
                        .clone()
                        .unwrap_or_else(|| format!("{}_{}", dir_basename(src_dir), spec.key));
                    let (tgt_key, renamed) = target_volume_name(spec, src_dir, tgt_dir);
                    let src_key = resolved;
                    if renamed {
                        warnings.push(format!(
                            "卷 {} 在目标服务器将命名为 {}(两机部署目录名不同),数据会完整导入新名称的卷",
                            src_key, tgt_key
                        ));
                    }
                    if let Some(b) = sizes.get(i).copied().flatten() {
                        total += b;
                        has_size = true;
                    }
                    volumes.push(PlanVolume {
                        source_key: src_key,
                        target_key: tgt_key,
                        kind: "Named".to_string(),
                        size: sizes.get(i).copied().flatten().map(format_bytes),
                        renamed,
                    });
                }
                VolumeKind::BindRelative => {
                    if let Some(b) = sizes.get(i).copied().flatten() {
                        total += b;
                        has_size = true;
                    }
                    volumes.push(PlanVolume {
                        source_key: spec.key.clone(),
                        target_key: spec.key.clone(),
                        kind: "BindRelative".to_string(),
                        size: sizes.get(i).copied().flatten().map(format_bytes),
                        renamed: false,
                    });
                }
                VolumeKind::BindAbsolute => {
                    warnings.push(format!(
                        "挂载 {} 是宿主绝对路径,已跳过不搬运(可能是系统文件或跨项目共享),请确认目标服务器已具备该路径",
                        spec.key
                    ));
                }
                VolumeKind::External => {
                    warnings.push(format!(
                        "卷 {} 声明为 external,已跳过不搬运(可能是多项目共享资源),请在目标服务器自行准备",
                        spec.key
                    ));
                }
            }
        }
    }

    // ---- 4. 发布归档(倒序取前 N)----
    let keep = release_count.min(RELEASE_COUNT_MAX);
    let mut releases: Vec<PlanRelease> = Vec::new();
    if keep > 0 {
        let releases_root = commands::remote_join(src_dir, "releases");
        let (code, out) = with_timeout(
            SSH_EXEC_TIMEOUT_SECS,
            "查询发布归档超时",
            "请检查服务器网络后重试",
            exec_collect(client, &commands::ls_dir_cmd(&releases_root)),
        )
        .await?;
        if code == 0 {
            let mut ts_list = commands::parse_ls_lines(&out);
            ts_list.sort_by(|a, b| b.cmp(a)); // 倒序 = 新 → 旧
            let paths: Vec<String> = ts_list
                .iter()
                .take(keep as usize)
                .map(|ts| commands::remote_join(&releases_root, ts))
                .collect();
            let sizes = query_path_sizes(client, &paths).await;

            for (i, ts) in ts_list.iter().take(keep as usize).enumerate() {
                let dir = commands::remote_join(&releases_root, ts);
                let services = read_release_services(client, &dir).await;
                if let Some(b) = sizes.get(i).copied().flatten() {
                    total += b;
                    has_size = true;
                }
                releases.push(PlanRelease {
                    ts: ts.clone(),
                    size: sizes.get(i).copied().flatten().map(format_bytes),
                    services,
                });
            }
        } else {
            warnings.push("源服务器上没有发布归档目录(该项目可能未做过整栈部署)".to_string());
        }
    }

    // ---- 5. 目标侧冲突检查 ----
    let tgt_compose = commands::remote_join(tgt_dir, "docker-compose.yml");
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "检查目标目录超时",
        "请检查服务器网络后重试",
        exec_collect(client, &commands::test_file_cmd(&tgt_dir)),
    )
    .await?;
    if code != 0 {
        warnings.push(format!(
            "目标服务器上部署目录 {} 不存在,执行时将自动创建",
            tgt_dir
        ));
    }
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "检查目标 compose 超时",
        "请检查服务器网络后重试",
        exec_collect(client, &commands::test_file_cmd(&tgt_compose)),
    )
    .await?;
    if code == 0 {
        warnings.push(format!(
            "目标服务器已存在 {},执行时将被覆盖(原文件不备份)",
            tgt_compose
        ));
    }

    if src_dir == tgt_dir {
        errors.push(
            "源与目标使用同一部署目录:磁盘路径相同的两台服务器上,卷名也会相同,继续执行可能造成混淆。请为目标项目指定独立目录"
                .to_string(),
        );
    }

    Ok(MigrateProjectPlan {
        project_id: project.id.clone(),
        project_name: project.name.clone(),
        source_server_name: source.name.clone(),
        target_server_name: target.name.clone(),
        source_remote_dir: src_dir.to_string(),
        target_remote_dir: tgt_dir.to_string(),
        images,
        volumes,
        releases,
        total_bytes: if has_size { Some(total) } else { None },
        warnings,
        errors,
    })
}

/// 卷在**源侧**的定位符:命名卷用实际卷名(du 可算、docker run 可挂),
/// 相对目录拼成部署目录下的绝对路径。
fn source_key_of(spec: &stack::VolumeSpec, src_dir: &str) -> String {
    match spec.kind {
        VolumeKind::BindRelative => {
            commands::remote_join(src_dir, spec.key.trim_start_matches("./"))
        }
        _ => spec
            .resolved_name
            .clone()
            .unwrap_or_else(|| spec.key.clone()),
    }
}

/// 由卷名键推导目标侧的**实际卷名**(顶层 `name:` 覆盖值优先,否则
/// `<目标目录名>_<键>`)。
fn target_volume_name(spec: &stack::VolumeSpec, src_dir: &str, tgt_dir: &str) -> (String, bool) {
    let declared = spec
        .resolved_name
        .as_deref()
        // resolved_name 与 key 相同 ⇒ 来自顶层 `name:` 覆盖(与目录无关)
        .filter(|_| spec.resolved_name.as_deref() != Some(spec.key.as_str()));
    let (src, tgt, renamed) =
        resolve_volume_names(src_dir, tgt_dir, declared, &spec.key);
    let _ = src;
    (tgt, renamed)
}

/// 读取某个发布归档 manifest 的服务清单(读不到返回空,不阻断)。
async fn read_release_services(client: &mut SshClient, release_dir: &str) -> Vec<String> {
    let manifest = commands::remote_join(release_dir, "manifest.json");
    let Ok((code, out)) = exec_collect(client, &commands::cat_file_cmd(&manifest)).await else {
        return Vec::new();
    };
    if code != 0 {
        return Vec::new();
    }
    serde_json::from_str::<commands::ReleaseManifest>(&out)
        .map(|m| m.images.into_iter().map(|i| i.service).collect())
        .unwrap_or_default()
}

/// 批量查询远端路径体积(`du -s` 逐条;返回与入参等长的列表,取不到为 None)。
async fn query_path_sizes(client: &mut SshClient, paths: &[String]) -> Vec<Option<u64>> {
    if paths.is_empty() {
        return Vec::new();
    }
    let cmd = commands::du_bytes_cmd(paths);
    match with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询体积超时",
        "请检查服务器网络后重试",
        exec_collect(client, &cmd),
    )
    .await
    {
        Ok((0, out)) => commands::parse_du_bytes(&out, paths),
        _ => vec![None; paths.len()],
    }
}

/// 字节数 → 人类可读(1024 进制,与前端 formatBytes 同口径)。
pub fn format_bytes(b: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", b, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

// ===== 执行 =====

type RunResult = Result<Vec<String>, (String, Vec<String>)>;

async fn run_migrate_project(
    app: &AppHandle,
    state: &Arc<commands::MigrateStateInner>,
    migrate_id: u64,
    req: &MigrateProjectRequest,
) -> RunResult {
    let emit_line: Arc<dyn Fn(&str) + Send + Sync> = {
        let app = app.clone();
        Arc::new(move |msg: &str| {
            let _ = app.emit(
                "migrate-project-log",
                MigrateProjectLogEvent {
                    migrate_id,
                    line: format_log_line("", msg),
                },
            );
        })
    };

    let cfg = config::load_config().map_err(|e| (format!("读取配置失败: {}", e), Vec::new()))?;
    let project = commands::find_project(&cfg, &req.project_id)
        .map_err(|e| (e, Vec::new()))?
        .clone();
    let source = commands::find_server_pub(&cfg, &req.source_server_id)
        .map_err(|e| (e, Vec::new()))?
        .clone();
    let target = commands::find_server_pub(&cfg, &req.target_server_id)
        .map_err(|e| (e, Vec::new()))?
        .clone();

    let src_dir = commands::effective_remote_dir(&source, &project);
    let tgt_dir = resolve_target_dir(&project, &target, req.target_remote_dir.as_deref());

    let mut warnings: Vec<String> = Vec::new();

    // 取消检查:迁移边界生效(与镜像迁移同口径)
    macro_rules! check_cancel {
        () => {
            if state.is_cancelled_pub() {
                return Err((crate::errors::cancelled(), warnings));
            }
        };
    }

    emit_line(&format!(
        "开始迁移项目「{}」:{} → {}",
        project.name, source.name, target.name
    ));
    emit_line(&format!("源部署目录:{}", src_dir));
    emit_line(&format!("目标部署目录:{}", tgt_dir));

    let (_srv_s, mut src) =
        connect_server(&req.source_server_id, req.source_password_plain.as_deref(), None)
            .await
            .map_err(|e| (e, warnings.clone()))?;
    emit_line(&format!("已连接源服务器「{}」", source.name));
    let (_srv_t, mut dst) =
        connect_server(&req.target_server_id, req.target_password_plain.as_deref(), None)
            .await
            .map_err(|e| (e, warnings.clone()))?;
    emit_line(&format!("已连接目标服务器「{}」", target.name));

    // 源 compose 文本(目标要复现源实际部署的状态)
    let src_compose = commands::remote_join(&src_dir, "docker-compose.yml");
    let (code, compose_text) = exec_collect(&mut src, &commands::cat_file_cmd(&src_compose))
        .await
        .map_err(|e| (format!("读取源 compose 失败: {}", e), warnings.clone()))?;
    if code != 0 {
        return Err((format!("源服务器上找不到 {}", src_compose), warnings));
    }

    // 本地中转目录(全部产物落此,离开作用域即递归清理)
    let stage_dir = std::env::temp_dir().join(format!("dd-migrate-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&stage_dir)
        .map_err(|e| (format!("创建本地中转目录失败: {}", e), warnings.clone()))?;
    let _stage_guard = StageDirGuard(stage_dir.clone());

    // 源 compose 落本地一份(供解析卷定义与镜像清单复用,避免重复往返)
    let local_compose = stage_dir.join("docker-compose.yml");
    std::fs::write(&local_compose, &compose_text)
        .map_err(|e| (format!("写入本地临时 compose 失败: {}", e), warnings.clone()))?;

    // ---- 目标目录准备 ----
    check_cancel!();
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "创建目标目录超时",
        "请检查服务器网络后重试",
        exec_collect(&mut dst, &mkdir_p_cmd(&tgt_dir)),
    )
    .await
    .map_err(|e| (e, warnings.clone()))?;
    if code != 0 {
        return Err((
            format!("创建目标部署目录失败:{}", commands::tail_lines(&out, 3)),
            warnings,
        ));
    }

    // ---- ① 卷:停源 → 导出 → 恢复源(必须无条件恢复)----
    if req.include_volumes {
        check_cancel!();
        let mut mounts: Vec<stack::VolumeMount> = Vec::new();
        match stack::parse_compose_volumes(&local_compose, &dir_basename(&src_dir)) {
            Ok(m) => mounts = m,
            Err(e) => warnings.push(format!("解析卷定义失败,跳过卷搬运:{}", e)),
        }

        let specs: Vec<stack::VolumeSpec> = stack::dedupe_volume_specs(&mounts);
        let movable: Vec<stack::VolumeSpec> = specs
            .into_iter()
            .filter(|s| matches!(s.kind, VolumeKind::Named | VolumeKind::BindRelative))
            .collect();

        // 不搬运的类型在这里一次性提示(预检也会提示,执行路径同样需要)
        for m in &mounts {
            match m.kind {
                VolumeKind::BindAbsolute => warnings.push(format!(
                    "挂载 {} 是宿主绝对路径,未搬运;请确认目标服务器已具备该路径",
                    m.source
                )),
                VolumeKind::External => warnings.push(format!(
                    "卷 {} 声明为 external,未搬运;请在目标服务器自行准备",
                    m.source
                )),
                _ => {}
            }
        }

        if movable.is_empty() {
            emit_line("未发现可搬运的数据卷(命名卷或项目内目录),跳过卷搬运");
        } else {
            let (code, out) = with_timeout(
                SSH_EXEC_TIMEOUT_SECS,
                "创建源临时目录超时",
                "请检查服务器网络后重试",
                exec_collect(&mut src, &mkdir_p_cmd(MIGRATE_TMP_DIR)),
            )
            .await
            .map_err(|e| (e, warnings.clone()))?;
            if code != 0 {
                return Err((
                    format!("创建源临时目录失败:{}", commands::tail_lines(&out, 3)),
                    warnings,
                ));
            }

            // 选一个含 tar 的镜像(导出/导入都靠它)
            let tar_image = pick_tar_image(&mut src, &emit_line)
                .await
                .map_err(|e| (e, warnings.clone()))?;

            // ---- 停机窗口开始 ----
            emit_line(&format!(
                "停止源服务器服务({} 个卷待导出,导出完成后立即恢复)…",
                movable.len()
            ));
            let stop_started = std::time::Instant::now();
            let stop_err = compose_simple_action(&mut src, &src_dir, "stop", &emit_line)
                .await
                .err();

            // 导出(无论成败都要尝试恢复源,故先取结果再统一恢复)
            let export_result = export_volumes(
                &mut src,
                &src_dir,
                &tgt_dir,
                &movable,
                &tar_image,
                &stage_dir,
                &mut warnings,
                &emit_line,
            )
            .await;

            // ---- 停机窗口结束:恢复源 ----
            emit_line("导出结束,恢复源服务器服务…");
            match compose_simple_action(&mut src, &src_dir, "start", &emit_line).await {
                Ok(()) => {
                    if stop_err.is_none() {
                        emit_line(&format!(
                            "源服务器服务已恢复(停机 {} 秒)",
                            stop_started.elapsed().as_secs()
                        ));
                    }
                }
                Err(e) => {
                    warnings.push(format!(
                        "源服务器服务未能自动恢复({}),请手动启动:cd {} && docker compose up -d",
                        e, src_dir
                    ));
                    emit_line(&format!("警告:源服务恢复失败 —— {}", e));
                }
            }

            let volume_packages =
                export_result.map_err(|e| (e, warnings.clone()))?;

            // 上传卷包到目标
            check_cancel!();
            emit_line("上传数据卷包到目标服务器…");
            let noop = |_a: u64, _b: u64| {};
            for (_src_key, tgt_key, local, pkg_name) in &volume_packages {
                dst.sftp_upload(local, MIGRATE_TMP_DIR, pkg_name, false, &noop)
                    .await
                    .map_err(|e| {
                        (format!("上传卷包 {} 失败: {}", tgt_key, e), warnings.clone())
                    })?;
                emit_line(&format!("已上传卷包 → {}", tgt_key));
            }

            // 在目标侧创建并按目标卷名导入
            check_cancel!();
            emit_line("在目标服务器创建并导入数据卷…");
            for (src_key, tgt_key, _local, pkg_name) in &volume_packages {
                let remote_pkg = commands::remote_join(MIGRATE_TMP_DIR, pkg_name);
                import_volume(&mut dst, tgt_key, &remote_pkg, &tar_image, &emit_line)
                    .await
                    .map_err(|e| (format!("导入卷 {} 到目标失败:{}", src_key, e), warnings.clone()))?;
                emit_line(&format!("卷数据已导入:{}", tgt_key));
            }
        }
    }

    // ---- ② 镜像搬运 ----
    check_cancel!();
    let images: Vec<String> = match stack::parse_compose_file(&local_compose, &[]) {
        Ok(stack) => stack.services.iter().filter_map(|s| s.image.clone()).collect(),
        Err(e) => {
            warnings.push(format!("解析 compose 失败,跳过镜像搬运:{}", e));
            Vec::new()
        }
    };
    emit_line(&format!("待搬运镜像 {} 个", images.len()));

    let noop = |_a: u64, _b: u64| {};
    for (i, image) in images.iter().enumerate() {
        check_cancel!();
        emit_line(&format!("({}/{}) 搬运镜像 {}", i + 1, images.len(), image));

        // 目标已有同 ID → 跳过(复用镜像迁移的同口径判定)
        let src_id = remote_image_id(&mut src, image)
            .await
            .map_err(|e| (e, warnings.clone()))?;
        let Some(src_id) = src_id else {
            warnings.push(format!("源服务器上不存在镜像 {},已跳过", image));
            emit_line(&format!("警告:源服务器上不存在镜像 {},跳过", image));
            continue;
        };
        let dst_id = remote_image_id(&mut dst, image)
            .await
            .map_err(|e| (e, warnings.clone()))?;
        if let Some(dst_id) = dst_id {
            if same_image_id(&src_id, &dst_id) {
                emit_line(&format!("目标服务器已有同 ID 镜像,跳过传输: {}", image));
                continue;
            }
        }

        let tar_name = format!("img-{}.tar.gz", uuid::Uuid::new_v4());
        let local_path = stage_dir.join(&tar_name);
        let guard = TempFileGuard::new_pub(local_path.clone());

        save_gzip_remote(&mut src, image, &local_path, &emit_line)
            .await
            .map_err(|e| (e, warnings.clone()))?;

        dst.sftp_upload(&local_path, MIGRATE_TMP_DIR, &tar_name, false, &noop)
            .await
            .map_err(|e| (format!("上传镜像包 {} 失败: {}", image, e), warnings.clone()))?;

        let remote_tar = commands::remote_join(MIGRATE_TMP_DIR, &tar_name);
        let load_cmd = format!("docker load -i {}", shell_single_quote(&remote_tar));
        exec_forwarded_via_event(&mut dst, &load_cmd, &emit_line, "镜像装载")
            .await
            .map_err(|e| (e, warnings.clone()))?;

        let _ = exec_collect(&mut dst, &format!("rm -f {}", shell_single_quote(&remote_tar))).await;
        drop(guard);
        emit_line(&format!("镜像已装载:{}", image));
    }

    // ---- ③ compose 三件套 + 归档落目标 ----
    check_cancel!();
    emit_line("上传 compose 文件(含 .env 与 override)…");
    upload_compose_bundle(
        &mut dst,
        &mut src,
        &src_dir,
        &tgt_dir,
        &local_compose,
        &stage_dir,
        &emit_line,
    )
    .await
    .map_err(|e| (e, warnings.clone()))?;

    let keep = req.release_count.min(RELEASE_COUNT_MAX);
    if keep > 0 {
        check_cancel!();
        emit_line(&format!("搬运最近 {} 个发布归档…", keep));
        let releases_root = commands::remote_join(&src_dir, "releases");
        let (code, out) = exec_collect(&mut src, &commands::ls_dir_cmd(&releases_root))
            .await
            .map_err(|e| (format!("查询源发布归档失败: {}", e), warnings.clone()))?;
        if code == 0 {
            let mut ts_list = commands::parse_ls_lines(&out);
            ts_list.sort_by(|a, b| b.cmp(a)); // 倒序 = 新 → 旧
            let tgt_releases = commands::remote_join(&tgt_dir, "releases");
            let (code, _) = exec_collect(&mut dst, &mkdir_p_cmd(&tgt_releases))
                .await
                .map_err(|e| (e, warnings.clone()))?;
            if code != 0 {
                warnings.push("创建目标 releases 目录失败,跳过归档搬运".to_string());
            } else {
                for ts in ts_list.iter().take(keep as usize) {
                    check_cancel!();
                    let from = commands::remote_join(&releases_root, ts);
                    let to = commands::remote_join(&tgt_releases, ts);
                    match copy_remote_dir(&mut src, &mut dst, &from, &to, &stage_dir, &emit_line).await
                    {
                        Ok(n) => emit_line(&format!("归档 {} 已搬运({} 个文件)", ts, n)),
                        Err(e) => warnings.push(format!("搬运归档 {} 失败:{}", ts, e)),
                    }
                }
            }
        } else {
            emit_line("源服务器没有发布归档目录,跳过");
        }
    }

    // ---- ④ 启动 ----
    check_cancel!();
    emit_line("在目标服务器启动服务…");
    compose_simple_action(&mut dst, &tgt_dir, "up", &emit_line)
        .await
        .map_err(|e| (e, warnings.clone()))?;

    // ---- ⑤ 清理两端临时产物 ----
    let _ = exec_collect(
        &mut src,
        &format!("rm -rf {}", shell_single_quote(MIGRATE_TMP_DIR)),
    )
    .await;
    let _ = exec_collect(
        &mut dst,
        &format!("rm -rf {}", shell_single_quote(MIGRATE_TMP_DIR)),
    )
    .await;

    // ---- ⑥ 改绑配置 + 写历史 ----
    bind_project_to_target(&project.id, &target.id)
        .map_err(|e| (e, warnings.clone()))?;
    emit_line(&format!("项目已改绑到「{}」", target.name));

    commands::append_migration_history(
        &project.name,
        &source.name,
        &target.name,
        &images,
        &warnings,
    );

    Ok(warnings)
}

/// 远程取镜像完整 ID(取不到返回 None,不视为错误)。
async fn remote_image_id(client: &mut SshClient, image: &str) -> Result<Option<String>, String> {
    let (code, out) = exec_collect(client, &commands::docker_inspect_id_cmd(image))
        .await
        .map_err(|e| format!("查询镜像 {} 的 ID 失败: {}", image, e))?;
    if code != 0 {
        return Ok(None);
    }
    Ok(commands::parse_inspect_id(&out))
}

/// 选一个可用于 tar 的镜像:优先服务器上已有的,其次尝试拉取 busybox。
async fn pick_tar_image(
    client: &mut SshClient,
    emit_line: &Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<String, String> {
    for cand in TAR_IMAGE_CANDIDATES {
        let (code, _) = exec_collect(client, &commands::docker_inspect_cmd(cand))
            .await
            .map_err(|e| format!("探测 {} 失败: {}", cand, e))?;
        if code == 0 {
            emit_line(&format!("卷搬运将使用镜像 {} 执行 tar", cand));
            return Ok(cand.to_string());
        }
    }
    // 都没有:尝试拉取 busybox(约 2MB,需服务器出网)
    emit_line("服务器上没有可用的 tar 镜像,尝试拉取 busybox…");
    let (code, out) = with_timeout(
        STACK_COMPOSE_TIMEOUT_SECS,
        "拉取 busybox 超时",
        "请检查服务器能否出网,或在服务器上预置 busybox 镜像",
        exec_collect(client, "docker pull busybox:latest"),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "无法搬运数据卷:服务器上没有 busybox/alpine/ubuntu 镜像,且拉取失败({})。\
             请在服务器上执行 docker pull busybox,或手动导出卷数据",
            commands::tail_lines(&out, 2)
        ));
    }
    emit_line("已拉取 busybox:latest");
    Ok("busybox:latest".to_string())
}

/// 导出可搬运的卷到源侧临时目录,并下载到本地中转目录。
/// 返回 `(源键, 目标键, 本地包路径, 远端包名)` 列表。
#[allow(clippy::too_many_arguments)]
async fn export_volumes(
    client: &mut SshClient,
    src_dir: &str,
    tgt_dir: &str,
    movable: &[stack::VolumeSpec],
    tar_image: &str,
    stage_dir: &std::path::Path,
    warnings: &mut Vec<String>,
    emit_line: &Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<Vec<(String, String, PathBuf, String)>, String> {
    let mut packages = Vec::new();

    for (i, spec) in movable.iter().enumerate() {
        let pkg_name = format!("vol-{}.tar.gz", i);
        let src_path = source_key_of(spec, src_dir);
        let remote_pkg = commands::remote_join(MIGRATE_TMP_DIR, &pkg_name);

        emit_line(&format!("导出卷 {}…", src_path));
        let cmd = volume_export_cmd(tar_image, &src_path, &remote_pkg);
        let (code, out) = with_timeout(
            VOLUME_TAR_TIMEOUT_SECS,
            "导出数据卷超时",
            "卷数据较大时请耐心等待,或改为手动导出",
            exec_collect(client, &cmd),
        )
        .await
        .map_err(|e| format!("导出卷 {} 失败: {}", src_path, e))?;
        if code != 0 {
            return Err(format!(
                "导出卷 {} 失败:{}",
                src_path,
                commands::tail_lines(&out, 3)
            ));
        }

        // 拉回本地(中转),再统一上传到目标
        let local_pkg = stage_dir.join(&pkg_name);
        let noop = |_a: u64, _b: u64| {};
        client
            .sftp_download(&remote_pkg, &local_pkg, &noop)
            .await
            .map_err(|e| format!("下载卷包 {} 失败: {}", src_path, e))?;

        let size = std::fs::metadata(&local_pkg).map(|m| m.len()).unwrap_or(0);
        emit_line(&format!("卷 {} 已导出({})", src_path, format_bytes(size)));

        // 目标卷名:命名卷按目标侧部署目录名重新推导(见 resolve_volume_names)
        let tgt_key = match spec.kind {
            VolumeKind::Named => target_volume_name(spec, src_dir, tgt_dir).0,
            _ => spec.key.clone(),
        };
        if spec.kind == VolumeKind::Named && tgt_key != src_path {
            warnings.push(format!(
                "卷 {} 在目标服务器将导入为 {}",
                src_path, tgt_key
            ));
        }

        packages.push((src_path, tgt_key, local_pkg, pkg_name));
    }
    Ok(packages)
}

/// 在目标侧创建卷并导入数据。
async fn import_volume(
    client: &mut SshClient,
    target_key: &str,
    remote_pkg: &str,
    tar_image: &str,
    emit_line: &Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<(), String> {
    emit_line(&format!("创建并导入卷 {}…", target_key));
    let cmd = volume_import_cmd(tar_image, target_key, remote_pkg);
    let (code, out) = with_timeout(
        VOLUME_TAR_TIMEOUT_SECS,
        "导入数据卷超时",
        "卷数据较大时请耐心等待",
        exec_collect(client, &cmd),
    )
    .await?;
    if code != 0 {
        return Err(commands::tail_lines(&out, 3));
    }
    Ok(())
}

/// 卷导出命令(经单引号转义,防注入)。
///
/// `docker run --rm` 起临时容器执行 tar:宿主机不一定有 tar(`docker run -v` 是
/// 唯一稳妥路径)。`--entrypoint tar` 避免依赖镜像自身的 entrypoint 行为。
pub fn volume_export_cmd(tar_image: &str, volume_or_path: &str, remote_pkg: &str) -> String {
    format!(
        "docker run --rm --entrypoint tar -v {}:/from -v {}:/to {} czf /to/{} -C /from .",
        shell_single_quote(&volume_mount_spec(volume_or_path)),
        shell_single_quote(MIGRATE_TMP_DIR),
        shell_single_quote(tar_image),
        shell_single_quote(&pkg_basename(remote_pkg)),
    )
}

/// 卷导入命令:先建卷(幂等),再解包。
pub fn volume_import_cmd(tar_image: &str, target_volume: &str, remote_pkg: &str) -> String {
    format!(
        "docker volume create {} >/dev/null && docker run --rm --entrypoint tar -v {}:/to -v {}:/from {} xzf /from/{} -C /to",
        shell_single_quote(target_volume),
        shell_single_quote(target_volume),
        shell_single_quote(MIGRATE_TMP_DIR),
        shell_single_quote(tar_image),
        shell_single_quote(&pkg_basename(remote_pkg)),
    )
}

/// 卷的 `-v` 参数值:命名卷就是卷名,项目内相对目录需转为绝对路径。
fn volume_mount_spec(volume_or_path: &str) -> String {
    volume_or_path.to_string()
}

/// 取远端包的 basename(命令里只用文件名,目录另经 `-v` 挂载)。
fn pkg_basename(remote_pkg: &str) -> String {
    remote_pkg.rsplit('/').next().unwrap_or(remote_pkg).to_string()
}

/// 源 compose 三件套 → 目标部署目录(compose 固定名 `docker-compose.yml`)。
///
/// compose 本体用已取回的本地中转副本;`.env` 与 override 逐个从源服务器取回,
/// 保证目标上跑的是**源实际部署的内容**(而非本机副本的旧版本)。
async fn upload_compose_bundle(
    dst: &mut SshClient,
    src: &mut SshClient,
    src_dir: &str,
    tgt_dir: &str,
    local_compose: &std::path::Path,
    stage_dir: &std::path::Path,
    emit_line: &Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<(), String> {
    let noop = |_a: u64, _b: u64| {};
    dst.sftp_upload(local_compose, tgt_dir, "docker-compose.yml", false, &noop)
        .await
        .map_err(|e| format!("上传 compose 到目标失败: {}", e))?;

    // .env + override:在**远端**逐个探测(不能用 stack::find_override_files ——
    // 它做的是本地 is_file 检查,喂远端路径会恒为空,导致 override 静默丢失)
    let mut extra: Vec<String> = vec![".env".to_string()];
    for name in [
        "compose.override.yaml",
        "compose.override.yml",
        "docker-compose.override.yaml",
        "docker-compose.override.yml",
    ] {
        let remote = commands::remote_join(src_dir, name);
        if let Ok((0, _)) = exec_collect(src, &commands::test_file_cmd(&remote)).await {
            extra.push(name.to_string());
        }
    }

    for name in extra {
        let remote = commands::remote_join(src_dir, &name);
        let (code, content) = match exec_collect(src, &commands::cat_file_cmd(&remote)).await {
            Ok(v) => v,
            Err(e) => {
                emit_line(&format!("警告:读取源 {} 失败,已跳过:{}", name, e));
                continue;
            }
        };
        if code != 0 {
            continue; // 源上没有该文件,属正常
        }
        let local = stage_dir.join(&name);
        std::fs::write(&local, &content)
            .map_err(|e| format!("写入本地临时 {} 失败: {}", name, e))?;
        dst.sftp_upload(&local, tgt_dir, &name, false, &noop)
            .await
            .map_err(|e| format!("上传 {} 到目标失败: {}", name, e))?;
        emit_line(&format!("已同步 {}", name));
    }
    Ok(())
}

/// 远端目录整体搬运(经本地中转):逐文件下载 → 上传。
///
/// 归档目录是**单层**的(镜像包 + manifest.json + compose 副本,无子目录),
/// 故只处理文件;遇到子目录会跳过并计入警告(由调用方汇总)。
async fn copy_remote_dir(
    src: &mut SshClient,
    dst: &mut SshClient,
    src_dir: &str,
    tgt_dir: &str,
    stage_dir: &std::path::Path,
    emit_line: &Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<usize, String> {
    let (code, out) = exec_collect(src, &commands::ls_dir_cmd(src_dir)).await?;
    if code != 0 {
        return Err(format!("读取目录 {} 失败", src_dir));
    }
    let (code, _) = exec_collect(dst, &mkdir_p_cmd(tgt_dir)).await?;
    if code != 0 {
        return Err(format!("创建目录 {} 失败", tgt_dir));
    }

    let files = commands::parse_ls_lines(&out);
    let noop = |_a: u64, _b: u64| {};
    let mut count = 0;
    for name in &files {
        let remote_src = commands::remote_join(src_dir, name);
        let local = stage_dir.join(format!("rel-{}", name));
        src.sftp_download(&remote_src, &local, &noop)
            .await
            .map_err(|e| format!("下载 {} 失败: {}", name, e))?;
        dst.sftp_upload(&local, tgt_dir, name, false, &noop)
            .await
            .map_err(|e| format!("上传 {} 失败: {}", name, e))?;
        let _ = std::fs::remove_file(&local);
        count += 1;
        emit_line(&format!("  归档文件已搬运:{}", name));
    }
    Ok(count)
}

/// 在源/目标执行一条 compose 动作(`stop` / `start` / `up`)。
async fn compose_simple_action(
    client: &mut SshClient,
    dir: &str,
    action: &str,
    emit_line: &Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<(), String> {
    let cmd = match action {
        "up" => format!(
            "cd {} && docker compose -f {} up -d",
            shell_single_quote(dir),
            shell_single_quote(&commands::remote_join(dir, "docker-compose.yml"))
        ),
        other => format!(
            "cd {} && docker compose -f {} {}",
            shell_single_quote(dir),
            shell_single_quote(&commands::remote_join(dir, "docker-compose.yml")),
            other
        ),
    };
    // 输出逐行 emit(便于用户看到 stop/start 的实际结果)
    let mut buf = String::new();
    let mut on_line = |line: &str| {
        emit_line(line.trim_end());
        buf.push_str(line);
    };
    let code = with_timeout(
        STACK_COMPOSE_TIMEOUT_SECS,
        "compose 动作超时",
        "请检查服务器网络后重试",
        async {
            client
                .exec(&cmd, &mut on_line)
                .await
                .map_err(|e| format!("执行 compose {} 失败: {}", action, e))
        },
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "compose {} 失败(退出码 {}): {}",
            action,
            code,
            buf.trim().lines().last().unwrap_or("无输出")
        ));
    }
    Ok(())
}

/// 改绑项目的默认服务器到目标。
fn bind_project_to_target(project_id: &str, target_server_id: &str) -> Result<(), String> {
    let mut cfg = config::load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let Some(project) = cfg.projects.iter_mut().find(|p| p.id == project_id) else {
        return Err(format!("未找到项目配置:{}", project_id));
    };
    project.default_server_id = Some(target_server_id.to_string());
    config::save_config(&cfg).map_err(|e| format!("保存配置失败: {}", e))
}

/// 本地中转目录的 Drop 守卫(整个目录递归删除)。
struct StageDirGuard(PathBuf);

impl Drop for StageDirGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("删除本地中转目录失败 ({}): {}", self.0.display(), e);
            }
        }
    }
}

// ===== 纯函数单测 =====

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dir_basename() {
        assert_eq!(dir_basename("/home/user/zetok"), "zetok");
        assert_eq!(dir_basename("/home/user/zetok/"), "zetok");
        assert_eq!(dir_basename("/opt/app"), "app");
        assert_eq!(dir_basename("/"), "");
    }

    #[test]
    fn test_resolve_volume_names_same_dir() {
        let (src, tgt, renamed) = resolve_volume_names("/opt/zetok", "/opt/zetok", None, "pgdata");
        assert_eq!(src, "zetok_pgdata");
        assert_eq!(tgt, "zetok_pgdata");
        assert!(!renamed, "同目录名时卷名一致,无需提示改绑");
    }

    #[test]
    fn test_resolve_volume_names_different_dir() {
        // 两机目录名不同 ⇒ 卷名不同;数据仍会导入目标侧名称
        let (src, tgt, renamed) = resolve_volume_names("/opt/zetok", "/srv/prod-app", None, "pgdata");
        assert_eq!(src, "zetok_pgdata");
        assert_eq!(tgt, "prod-app_pgdata");
        assert!(renamed);
    }

    #[test]
    fn test_resolve_volume_names_declared_override_wins() {
        // 顶层 volumes: 写了 name: 时两机同名,不随目录变化
        let (src, tgt, renamed) =
            resolve_volume_names("/opt/a", "/opt/b", Some("shared_data"), "pgdata");
        assert_eq!(src, "shared_data");
        assert_eq!(tgt, "shared_data");
        assert!(!renamed);
    }

    #[test]
    fn test_resolve_volume_names_empty_declared_falls_back() {
        // 空串/空白声明的 name 视为未声明
        let (src, _, _) = resolve_volume_names("/opt/zetok", "/opt/x", Some("  "), "data");
        assert_eq!(src, "zetok_data");
    }

    #[test]
    fn test_resolve_target_dir_priority() {
        let mut project = sample_project();
        let target = sample_server("srv-b", "/srv/default");

        // 显式指定优先
        assert_eq!(
            resolve_target_dir(&project, &target, Some("/explicit")),
            "/explicit"
        );
        // 其次项目级 remote_dir
        project.remote_dir = Some("/opt/proj".to_string());
        assert_eq!(resolve_target_dir(&project, &target, None), "/opt/proj");
        // 最后回落服务器目录
        project.remote_dir = None;
        assert_eq!(resolve_target_dir(&project, &target, None), "/srv/default");
        // 空串的显式值等同未指定
        project.remote_dir = Some("/opt/proj".to_string());
        assert_eq!(resolve_target_dir(&project, &target, Some("  ")), "/opt/proj");
    }

    #[test]
    fn test_volume_export_cmd_shape_and_quoting() {
        let cmd = volume_export_cmd("busybox:latest", "zetok_pgdata", "/tmp/dd-migrate-project/vol-0.tar.gz");
        assert!(cmd.contains("docker run --rm"));
        assert!(cmd.contains("--entrypoint tar"));
        assert!(cmd.contains("-v 'zetok_pgdata':/from"), "卷名须引号包裹: {}", cmd);
        assert!(cmd.contains("czf /to/'vol-0.tar.gz'"), "包名只传 basename: {}", cmd);
        assert!(cmd.contains("-C /from ."));
    }

    #[test]
    fn test_volume_import_cmd_shape_and_quoting() {
        let cmd = volume_import_cmd("busybox:latest", "prod_pgdata", "/tmp/dd-migrate-project/vol-0.tar.gz");
        assert!(cmd.contains("docker volume create 'prod_pgdata'"), "{}", cmd);
        assert!(cmd.contains("-v 'prod_pgdata':/to"));
        assert!(cmd.contains("-C /to"));
    }

    #[test]
    fn test_volume_cmds_escape_injection_attempts() {
        // 卷名/镜像名含单引号与 shell 元字符时必须转义,不留注入面
        let evil = "vol'; rm -rf /; echo '";
        let export = volume_export_cmd("busybox:latest", evil, "/tmp/x.tar.gz");
        assert!(!export.contains("vol'; rm"), "未转义的引号会闭合字符串: {}", export);
        assert!(export.contains(r"'\''"), "应转义为 '\\'': {}", export);

        let import = volume_import_cmd(evil, evil, "/tmp/x.tar.gz");
        assert!(!import.contains("vol'; rm"), "{}", import);
        assert!(import.contains(r"'\''"), "{}", import);

        // 反引号与 $() 在单引号内不展开,但必须确保整体被引号包裹
        let dollar = "vol$(whoami)";
        let cmd = volume_export_cmd("busybox:latest", dollar, "/tmp/x.tar.gz");
        assert!(cmd.contains("'vol$(whoami)'"), "{}", cmd);
    }

    #[test]
    fn test_pkg_basename() {
        assert_eq!(pkg_basename("/tmp/dd-migrate-project/vol-0.tar.gz"), "vol-0.tar.gz");
        assert_eq!(pkg_basename("vol-0.tar.gz"), "vol-0.tar.gz");
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024 * 3 / 2), "1.5 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 2), "2.0 GB");
    }

    #[test]
    fn test_event_and_request_camel_case_contract() {
        // 前端按 camelCase 读取:改名会静默 undefined,故用断言锁死
        let log = MigrateProjectLogEvent {
            migrate_id: 7,
            line: "[12:00:00] x".into(),
        };
        let v = serde_json::to_value(&log).unwrap();
        assert!(v.get("migrateId").is_some(), "须为 migrateId: {:?}", v);
        assert!(v.get("migrate_id").is_none());

        let done = MigrateProjectDoneEvent {
            migrate_id: 7,
            success: true,
            message: "ok".into(),
            warnings: vec!["w".into()],
        };
        let v = serde_json::to_value(&done).unwrap();
        assert!(v.get("migrateId").is_some());
        assert!(v.get("success").is_some());
        assert!(v.get("message").is_some());
        assert!(v.get("warnings").is_some());

        // 请求体同理
        let req: MigrateProjectRequest = serde_json::from_str(
            r#"{"projectId":"p","sourceServerId":"a","targetServerId":"b",
                "includeVolumes":true,"releaseCount":2,"targetRemoteDir":null,
                "sourcePasswordPlain":null,"targetPasswordPlain":null}"#,
        )
        .unwrap();
        assert_eq!(req.project_id, "p");
        assert!(req.include_volumes);
        assert_eq!(req.release_count, 2);

        // 缺省字段(前端未传)应取默认
        let req2: MigrateProjectRequest = serde_json::from_str(
            r#"{"projectId":"p","sourceServerId":"a","targetServerId":"b",
                "sourcePasswordPlain":null,"targetPasswordPlain":null}"#,
        )
        .unwrap();
        assert!(!req2.include_volumes);
        assert_eq!(req2.release_count, 0);
    }

    #[test]
    fn test_plan_serializes_camel_case() {
        let plan = MigrateProjectPlan {
            project_id: "p".into(),
            project_name: "zetok".into(),
            source_server_name: "A".into(),
            target_server_name: "B".into(),
            source_remote_dir: "/opt/zetok".into(),
            target_remote_dir: "/opt/zetok".into(),
            images: vec![PlanImage {
                reference: "app:1".into(),
                exists_on_source: true,
                already_on_target: false,
            }],
            volumes: vec![],
            releases: vec![],
            total_bytes: Some(1024),
            warnings: vec![],
            errors: vec![],
        };
        let v = serde_json::to_value(&plan).unwrap();
        for key in [
            "projectId",
            "projectName",
            "sourceServerName",
            "targetServerName",
            "sourceRemoteDir",
            "targetRemoteDir",
            "totalBytes",
        ] {
            assert!(v.get(key).is_some(), "缺字段 {}: {:?}", key, v);
        }
        // 镜像条目内层同样 camelCase
        assert!(v["images"][0].get("existsOnSource").is_some());
        assert!(v["images"][0].get("alreadyOnTarget").is_some());
    }

    // ===== 测试辅助 =====

    fn sample_project() -> ProjectConfig {
        ProjectConfig {
            id: "p1".into(),
            name: "zetok".into(),
            image_filter: String::new(),
            compose_file: "docker-compose.yml".into(),
            file_mappings: vec![],
            service_overrides: vec![],
            health_wait_secs: 0,
            pre_deploy_cmd: None,
            post_deploy_cmd: None,
            notify_webhook: None,
            source_compose_path: None,
            source_hash: None,
            remote_dir: None,
            default_server_id: None,
            release_keep: None,
        }
    }

    fn sample_server(id: &str, dir: &str) -> ServerConfig {
        ServerConfig {
            id: id.into(),
            name: id.into(),
            host: "10.0.0.1".into(),
            port: 22,
            username: "root".into(),
            auth: crate::config::AuthConfig {
                auth_type: crate::config::AuthType::Key,
                key_path: None,
                password_enc: None,
                key_pass_enc: None,
            },
            remote_dir: dir.into(),
            host_key_sha256: None,
        }
    }
}
