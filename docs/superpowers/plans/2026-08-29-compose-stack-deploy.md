# Compose 整栈部署 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在现有单镜像部署之外,新增"按 compose 整栈一键部署":导入 compose(持久化+自命名)→ 服务分类(本地传输/服务器拉取)与本地镜像三级匹配 → 混合传输 → 远端 compose pull/up,releases 留档支持回滚。

**Architecture:** 复用现有 ProjectConfig 实体承载栈(compose_file 指向应用配置文件夹内的副本);parse_compose 命令做 YAML 解析 + .env 插值 + 本地镜像三级匹配;deploy_stack 命令做多镜像 save/upload/load + compose pull/up 管线,复用现有事件总线与取消机制。

**Tech Stack:** 现有 Tauri 2 + Rust 栈,新增 serde_yaml。

**Spec:** `docs/superpowers/specs/2026-08-28-docker-deploy-ssh-design.md`(基础)+ 本文件为增量 spec。

## Global Constraints

- 中文文案;错误信息中文带上下文;非测试代码不得 unwrap/expect。
- 不改现有 12 个命令的行为与签名;新增命令:import_compose / parse_compose / deploy_stack。
- 复用 deploy-progress/deploy-log/deploy-done 事件与 DeployState 取消机制;密码解析规则与单镜像部署一致(Key→None;Password→password_plain 或 DPAPI 解密)。
- 防注入:远端命令一律经 ssh::shell_single_quote;本地 Docker 调用参数数组。
- 所有新 struct 需 derive Serialize/Deserialize(前端契约,字段 snake_case)。

---

### Task 1: 配置扩展 + compose 导入/解析/匹配(后端)

**Files:**
- Modify: `src-tauri/src/config.rs`(ProjectConfig 增字段)
- Modify: `src-tauri/src/commands.rs`(import_compose / parse_compose)
- New: `src-tauri/src/stack.rs`(解析+匹配纯逻辑,便于单测)
- Modify: `src-tauri/src/lib.rs`、`src-tauri/Cargo.toml`(serde_yaml = "0.9")

**Interfaces(后续任务依赖,精确签名):**
- `struct ServiceOverride { pub service: String, pub mode: TransferMode }`;`enum TransferMode { Local, Pull }`(serde 大小写与 AuthType 一致,即 "Local"/"Pull")
- `ProjectConfig` 新增 `#[serde(default)] pub service_overrides: Vec<ServiceOverride>`(旧行为不变)
- `pub struct StackService { pub service: String, pub image: Option<String>, pub has_build: bool, pub mode: TransferMode, pub match_state: MatchState, pub local_tag: Option<String>, pub warning: Option<String> }`
- `pub enum MatchState { Exact, RepoOnly, Missing }`
- `pub struct ComposeStack { pub project_name: String, pub services: Vec<StackService>, pub errors: Vec<String> }`
- `pub fn parse_compose_file(compose_path: &Path, local_images: &[(String, String)]) -> Result<ComposeStack, String>`(stack.rs;local_images 为 docker images 的 repo/tag 对)
- `fn interpolate_env(raw: &str, env: &HashMap<String,String>) -> String`(支持 `${VAR}`、`${VAR:-default}`、`$VAR`;未定义→空串+收集警告)
- 命令:
  - `import_compose(source_path: String, name: String) -> Result<ProjectConfig, String>`:校验文件存在且可解析;复制到 `config/stacks/<uuid>/docker-compose.yml`(连同同目录 `.env` 若存在);以解析结果默认 service_overrides 建新 ProjectConfig(name 为用户自命名,compose_file=副本路径),写回配置并返回
  - `parse_compose(project_id: String) -> Result<ComposeStack, String>`:list_images 一次 → parse_compose_file → 应用该项目的 service_overrides 覆盖默认分类
  - `preview_compose(source_path: String) -> Result<ComposeStack, String>`:对任意路径的 compose 做静态只读解析(不落盘、不改配置),供导入前预览(Task 3 前端依赖此命令)
- 分类默认规则:has_build→Local;仅 image→Pull;image 字段缺失且无 build→记入 errors(阻断部署);has_build 且 image 缺失→Local+warning"未设 image 字段,无法核验/传输,请在 compose 补 image:";Missing+无 build→Pull+warning"本地不存在,将由服务器拉取";RepoOnly→warning"本地标签不一致:compose 要 X,本地有 Y"

**测试(stack.rs 内单测,compose 内容用临时文件写 fixture):**
- 基本解析:2 服务(build+image)分类正确
- `${IMAGE}:${TAG}` + .env 插值
- `${VAR:-fallback}` 默认值;未定义变量警告
- 三级匹配:Exact/RepoOnly/Missing 各一例
- build 无 image 字段 → warning;无 image 无 build → errors
- service_overrides 覆盖默认分类
- 现有 config roundtrip 测试补 service_overrides 字段断言

**Steps:** TDD 按函数逐个红→绿;`cargo test` 全量;`git add src-tauri/ && git commit -m "feat: compose import parse and local image matching"`

---

### Task 2: deploy_stack 栈部署管线(后端)

**Files:**
- Modify: `src-tauri/src/commands.rs`(deploy_stack + helper)
- Modify: `src-tauri/src/lib.rs`(注册)

**Interfaces:**
- `pub struct StackDeployRequest { pub project_id: String, pub server_id: String, pub services: Vec<StackServiceChoice>, pub password_plain: Option<String> }`
- `pub struct StackServiceChoice { pub service: String, pub image: String, pub mode: TransferMode }`(前端从 parse 结果逐服务确认后回传)
- `deploy_stack(req: StackDeployRequest, app: AppHandle) -> Result<(), String>`:立即返回,async_runtime::spawn;panic 兜底复用 CatchPanic;取消复用 DeployState;密码解析复用 resolve_password

**管线(progress total=6,步骤名:1 分类确认 2 打包 3 上传 4 装载 5 拉取 6 启动):**
1. emit step1;校验 req.services 非空、project/server 存在;Local 类服务解析出的 image 必须非空
2. step2:Local 类逐个 `docker image inspect --format {{.Size}}` 求和 ×1.5 磁盘预检;逐个 save_gzip(temp_dir/<uuid>.tar.gz,TempFileGuard)
3. step3:SFTP 连接;远端建 `<remote_dir>/releases/<yyyymmdd-HHMMSS>/`;逐包 sftp_upload 到 releases 目录(进度按包序号+字节);上传 project.fileMappings(复用 sync_files 逻辑)
4. step4:逐包 exec `docker load -i <releases 路径>`(timeout 600s,输出转发 deploy-log)
5. step5:Pull 类非空时 exec `cd <remote_dir> && docker compose -f <compose_file> pull <services...>`(timeout 900s;失败→中文报错提示检查服务器出网/或在分类中改为本地传输);Pull 类为空则跳过并 log
6. step6:exec `cd <remote_dir> && docker compose -f <compose_file> up -d`(timeout 900s)
7. 收尾:releases 目录仅保留最新 5 个(`ls -1dt <remote_dir>/releases/*/ | tail -n +6 | xargs -r rm -rf`,失败仅告警);本地 tar 全删;emit deploy-done
- 任何一步失败:中止 + deploy-done failure(中文,含失败步骤名);cancelled 检查点在各步骤边界与 exec 输出行回调

**测试:** services 为空/全 Pull 跳过打包等分支的纯逻辑抽函数单测;`cargo test` 全量;commit `feat: compose stack deploy pipeline with releases archive`

---

### Task 3: 前端整栈模式(导入 + 部署)

**Files:**
- Modify: `ui/deploy.js`(模式切换 + 栈流程)、`ui/servers.js`(项目表单导入 compose)、`ui/index.html`、`ui/style.css`

**行为规格:**
1. **项目表单(servers.js)**:新增「导入 compose 文件」区块:compose 路径输入框(手动填绝对路径)+「解析预览」按钮 → invoke('parse_compose' 不适用——此命令按 project_id;导入前预览改用新命令 `preview_compose(source_path: String) -> Result<ComposeStack, String>`(Task 1 需一并实现,静态只读解析,不落盘)→ 预览表(服务/镜像/匹配徽章/默认分类)→ 项目名称输入(默认=compose 文件名去扩展名)→ 保存走 `import_compose(source_path, name)` 返回的 ProjectConfig 合入表单其余字段(file_mappings 照旧编辑)后 save_config_cmd
2. **部署页(deploy.js)**:顶部模式切换两个 tab:「单镜像」「整栈部署(compose)」;整栈模式:项目下拉选中 → invoke('parse_compose') 渲染服务分类表:服务名/镜像/匹配徽章(✓ 已匹配 signal-dim 底、△ 标签不一致 warn 底、✗ 本地不存在 ink 底白字,warning 悬浮 title)/传输方式切换按钮(Local↔Pull 文字按钮)——errors 非空时红框阻断;「保存为默认分类」按钮 → 把当前 services 的 mode 写入 project.service_overrides 后 save_config_cmd
3. 预检(服务器环境检测)复用;开始部署 → invoke('deploy_stack', { req }) req snake_case;进度条渲染 6 节点(stack 模式节点集:分类确认/打包/上传/装载/拉取/启动,单模式保持 5 节点);deploy-log/deploy-done/取消逻辑复用;部署中禁止重复发起与切换模式
4. 匹配徽章/表沿用 UI 重构后的 ark-light 设计语言(徽章三态、1px 线、等宽数字),不引入新颜色

**验证:** node --check;id 对照;`npm run tauri build` 成功;commit `feat: stack deploy ui with compose import and classification table`

---

### Task 4: 验收

- `npm run tauri build` 产出安装包;手测:导入 compose(自命名/持久化/下拉可见)→ 分类调整 → 真实服务器整栈部署 → releases 目录留档确认
