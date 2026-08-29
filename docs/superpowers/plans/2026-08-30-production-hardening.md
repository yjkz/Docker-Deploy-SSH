# 生产加固批次 实现计划(P0+P1+传输优化)

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Step 用 checkbox 跟踪。

**Goal:** P0(健康检查/部署钩子/服务器磁盘预检/部署历史)+ P1(override 合并/私有 registry 提示/服务器清理/dry-run 预览/webhook 通知)+ 传输优化(断点续传/并行打包),v0.4.0。

**Spec:** 沿用 wiki 与既有 spec;本文档为增量规格(需求讨论记录见会话,要点已内嵌下文)。

## Global Constraints

- 全局约束沿用 `2026-08-28` 计划:中文错误、无 unwrap/expect(非测试)、snake_case 契约字段、防注入(shell_single_quote/参数数组)、serde default 兼容旧配置。
- 新增依赖仅 `ureq = "2"`(blocking webhook)。
- 不改既有 16+4 契约的语义;新增命令见各任务。

---

### Task 1: 配置扩展 + 部署历史模块

- `ProjectConfig` 增(全部 `#[serde(default)]`):`health_wait_secs: u32`(0=关)、`pre_deploy_cmd: Option<String>`、`post_deploy_cmd: Option<String>`、`notify_webhook: Option<String>`
- 新建 `src-tauri/src/history.rs`:`DeployRecord { ts, mode, server_name, project_name, images: Vec<String>, success: bool, message: String, duration_secs: u64 }`;`append_record()`(写 `config/deployments.json`,上限 200 条裁剪旧)、`load_history()`;原子写复用 config 模式;单测往返+裁剪
- 管线接点:`spawn_deploy_task` 完成路径 emit deploy-done 后调用 append(需要 server/project 名与镜像列表传入——`run_deploy`/`run_deploy_stack` 组装 `DeployRecord`,含开始 Instant 计时;取消也记录,message="部署已取消")
- 测试:往返/裁剪/默认字段兼容;`cargo test` 全绿;commit `feat: project config extensions and deploy history`

### Task 2: 服务器侧安全网(磁盘预检 + 清理)

- `docker.rs` 或 `ssh.rs`:`docker_root_cmd()` = `docker info -f '{{.DockerRootDir}}'`;`df_free_gb_cmd(path)`(复用 check_server_env 的 df 口径, BusyBox 失败→返回 None 跳过预检并告警)
- 管线接点:两条管线在上传前(整栈 step2 前/单镜像 step2 前,SSH 建连后)执行:取 docker root → df free ≥ 镜像总和×1.5,不足中文报错中止(本地导出预检保留)
- 服务器清理命令:`pub fn prune_cmd() -> String` = `docker image prune -f; docker container prune -f`;新命令 `prune_server(server_id, password_plain)`(exec + 输出转发 server-log;超时 300s)
- 单测:命令拼装/预检判定纯函数;commit `feat: remote disk precheck and server prune`

### Task 3: 钩子 + 健康检查管线

- 钩子:`server_hook(app, client, project, which: Pre|Post, remote_dir)` —— pre 在「上传完成、装载之前」执行(旧容器仍在运行);post 在「健康检查成功后」执行;命令在 `cd '<remote_dir>' && (<cmd>)` 下执行,超时 600s;pre 失败→中止(此时尚未 load/un?次序见下);post 失败→log 警告继续
  - 单镜像次序:上传→同步文件→pre→load→retag→up→健康→post→清 tar
  - 整栈次序:打包→上传(releases+compose+文件)→pre→load→pull→up→健康→post→清旧 releases
- 健康检查:`health_wait_secs>0` 时,up 后每 5s 轮询 `docker compose -f <f> ps --format json`(900s 上限即 wait_secs;0=跳过):解析每服务 state=="running"(有 healthcheck 的服务要求 health=="healthy");失败→exec `docker compose logs --tail 50` 输出进日志→报错"健康检查未通过:<服务> <状态>"
- 命令拼装函数(`compose_ps_json_cmd/compose_logs_cmd`)单测;commit `feat: deploy hooks and post-up health check`

### Task 4: 传输优化(断点续传 + 并行打包)

- `ssh.rs::sftp_upload` 增 `resume: bool`:先查远端同名文件 size;remote<local → 不带 TRUNCATE 打开+seek(remote) 续写(进度从 remote 起算);remote≥local 或 resume=false → 现行为;调用点:上传镜像包传 true,文件映射传 false
- 并行打包:整栈 step2 用 `futures::stream`(`futures` crate 或 tokio spawn_blocking+JoinSet)并发 `export_image`,并发度 `min(3, 可用并行)`,进度按完成镜像数+字节汇总(单镜像管线不变);注意 TempFileGuard 集合与取消检查(每完成一个检查一次)
- 单测:resume 判定纯函数(远端 size vs 本地 size 决策);commit `feat: sftp resume and parallel image packing`

### Task 5: 解析增强(override 合并 + 私有仓库提示)

- `stack.rs`:检测 compose 同目录 `compose.override.yaml`→`compose.override.yml`→`docker-compose.override.yaml`→`docker-compose.override.yml`(存在即按序全部合并,后覆盖前);服务级合并:image/build 存在性以 override 为准;记录 `overrides: Vec<String>` 到 ComposeStack(新字段 serde default)
- `import_compose`/`run_deploy_stack`:副本与远端上传包含 override 文件(远端放同 basename);pull/up 命令对每个 override 追加 `-f <override>`(compose_pull_cmd/compose_up_cmd 增参数)
- 私有仓库:`StackService` 增 `registry: Option<String>`;Pull 类且 registry 非 docker.io(解析规则:首段含 `.`/`:` 或为 localhost)→ warning "私有仓库,请确认服务器已 docker login";pull 失败错误若含 401/Unauthorized/denied → 专属中文提示
- 单测:合并次序/覆盖规则/registry 判定/默认字段兼容;commit `feat: compose override merge and private registry hints`

### Task 6: dry-run 预览 + webhook 通知

- dry-run(独立功能):新命令 `preview_stack_changes(server_id, project_id, password_plain) -> StackPreview { entries: Vec<StackPreviewEntry>, errors }`;`StackPreviewEntry { service, image, mode, action: "Recreate"|"Create"|"Unchanged"|"Pull"|"Absent" }`;实现:远端 `docker ps -a --filter label=com.docker.compose.project=<remote_dir 基名> --format json`(服务名+镜像 ID)+ 远端 `docker images` + 本地 parse_compose 对比分类;不落盘不改状态
- webhook:依赖 `ureq = "2"`;`notify_webhook(Some(url))` 时,`spawn_deploy_task` 完成(deploy-done 同步)后 `spawn_blocking` POST JSON `{"event":"deploy","success":..,"message":..,"server":..,"project":..,"duration_secs":..,"ts":..}`,超时 10s,失败仅 log 告警
- Cargo.toml 加 ureq;单测:分类纯函数/webhook 载荷序列化;commit `feat: stack change preview and deploy webhook`

### Task 7: 前端集成

- 项目表单:新增 健康等待秒数(number,默认 0=关闭,提示>0 启用)、pre/post 命令多行文本(**预设 chips:4 个模板[MySQL 全库备份/PostgreSQL 全库备份/悬空镜像清理(post)/发布日志(post)],点击=插入文本可编辑,不默认加载**,pre 列 backup/migrate 类、post 列 prune/log 类)、webhook URL 输入;保存走既有就地改字段路径
- 服务器卡片:「清理优化」按钮(自绘确认→`prune_server`→server-log 回显)
- 部署页整栈面板:「部署预览」独立按钮(→`preview_stack_changes`→action 表:重建/新建/不变/拉取/缺失 徽章化,ark 三态语言);「部署历史」折叠面板(`load_history` 新命令或 get_history)列表(时间/项目/服务器/结果徽章/耗时)
- 顺带把部署完成消息接历史刷新;全部 ark-light+dark 适配、防 XSS、单次注册守卫;commit `feat: production hardening ui`

### Task 8: 构建验收

- `npm run tauri build`;手测清单:钩子成功/失败中止、健康检查失败报错、断点续传(中断后续传)、预览分类、历史记录、webhook(用 webhook.site)、清理按钮
- 版本 0.4.0;commit `chore: release v0.4.0`
