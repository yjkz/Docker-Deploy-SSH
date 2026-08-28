# DockerDeploy SSH 桌面客户端 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建一个 Tauri 桌面客户端,实现"环境检测 → 镜像列表 → 多服务器管理 → 一键部署(save/gzip → SFTP 上传 → load → compose up)"的完整流程。

**Architecture:** Tauri 2.x,Rust 后端通过 Docker CLI 操作本机 Docker,用 `flate2` 做 gzip 压缩,`russh`/`russh-sftp` 做 SSH 部署与文件传输,配置以 JSON 存于应用文件夹,前端为无框架 HTML/JS 多页单窗口应用,通过 Tauri command + event 通信。

**Tech Stack:** Tauri 2、Rust(crate: tokio, flate2, russh, russh-sftp, serde, base64, windows-dpapi, chrono, uuid)、HTML/CSS/JS(原生)。

**Spec:** `docs/superpowers/specs/2026-08-28-docker-deploy-ssh-design.md`

## Global Constraints

- 仅 Windows 平台优先;配置存储在应用安装目录下 `config/`(便携式);日志在应用文件夹 `logs/`。
- SSH 认证支持私钥与账号密码两种;密码用 Windows DPAPI 加密存储,禁止明文落盘。
- 部署前置闸门:宿主机检测未通过 → 镜像/部署页操作禁用;服务器检测未通过 → 该服务器不可选为部署目标。
- 不引入前端框架;不写服务器端常驻组件。
- 所有用户可见文案为中文。
- 部署流程五步:打日期 tag(可关)→ save+gzip → SFTP 上传 → 文件同步 → `docker load` + `docker compose up -d`,每步实时回显,任一步失败即中止。

---

### Task 0: 开发环境准备

**Files:** 无项目文件,仅环境。

- [ ] **Step 1: 安装 Rust 工具链**

```powershell
winget install Rustlang.Rustup
rustup default stable-x86_64-pc-windows-msvc
```

若 MSVC 构建工具缺失,追加:`winget install Microsoft.VisualStudio.2022.BuildTools --override "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`

- [ ] **Step 2: 验证**

Run: `rustc --version && cargo --version`
Expected: 输出版本号(≥ 1.77)。

- [ ] **Step 3: 安装 Tauri CLI 与脚手架**

```bash
cd /d/Github-repositories/docker-deploy-ssh
npm init -y && npm install -D @tauri-apps/cli@latest
npx tauri init --app-name docker-deploy-ssh --window-title "DockerDeploy SSH" --frontend-dist ../ui --dev-url http://localhost:5173
```

- [ ] **Step 4: 初始化 git 并首次提交**

```bash
git init && printf 'node_modules/\ntarget/\nsrc-tauri/target/\ndist/\n' > .gitignore
git add -A && git commit -m "chore: scaffold tauri project"
```

---

### Task 1: 配置模块(读写 servers.json / projects.json)

**Files:**
- Create: `src-tauri/src/config.rs`
- Modify: `src-tauri/src/lib.rs`(注册模块)
- Test: `src-tauri/src/config.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `struct ServerConfig { id, name, host, port, username, auth: AuthConfig, remote_dir }`,`struct AuthConfig { auth_type: AuthType, key_path: Option<String>, password_enc: Option<String> }`,`enum AuthType { Key, Password }`
  - `struct ProjectConfig { id, name, image_filter, compose_file, file_mappings: Vec<FileMapping> }`,`struct FileMapping { local, remote, is_dir }`
  - `struct AppConfig { servers: Vec<ServerConfig>, projects: Vec<ProjectConfig> }`
  - `fn config_dir() -> PathBuf`(exe 同目录 `config/`);`fn load_config() -> Result<AppConfig>`;`fn save_config(cfg: &AppConfig) -> Result<()>`(保存时先写 `.tmp` 再原子替换)

- [ ] **Step 1: 在 Cargo.toml 添加依赖**

```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
uuid = { version = "1", features = ["v4"] }

[target.'cfg(windows)'.dependencies]
windows-dpapi = "0.1"
```

- [ ] **Step 2: 写失败测试** — 临时目录中往返读写:

```rust
#[test]
fn test_roundtrip() {
    let dir = std::env::temp_dir().join(format!("ddtest-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("config")).unwrap();
    let mut cfg = AppConfig::default();
    cfg.servers.push(ServerConfig {
        id: "s1".into(), name: "生产".into(), host: "1.2.3.4".into(), port: 22,
        username: "root".into(),
        auth: AuthConfig { auth_type: AuthType::Key, key_path: Some("C:/k".into()), password_enc: None },
        remote_dir: "/opt/app".into(),
    });
    // config_dir 依赖环境变量以便测试注入
    std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
    save_config(&cfg).unwrap();
    let loaded = load_config().unwrap();
    assert_eq!(loaded.servers.len(), 1);
    assert_eq!(loaded.servers[0].host, "1.2.3.4");
    assert!(dir.join("config/servers.json").exists());
}
```

实现注:`config_dir()` 优先读环境变量 `DD_CONFIG_DIR`(测试用),否则为 `std::env::current_exe()` 的父目录下的 `config/`。servers 与 projects 各存一个 JSON 文件。

- [ ] **Step 3: 运行确认失败 → 实现 → 确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml config::` (先 FAIL 后 PASS)

- [ ] **Step 4: Commit** `git commit -m "feat: config module with atomic json persistence"`

---

### Task 2: Docker CLI 封装 + 宿主机检测

**Files:**
- Create: `src-tauri/src/docker.rs`
- Test: `src-tauri/src/docker.rs` 内单测(命令拼装纯函数部分)

**Interfaces:**
- Produces:
  - `enum DockerCheck { Installed, NotInstalled, DaemonNotRunning }`
  - `fn check_host() -> HostCheckReport`(字段:`docker_installed: bool, daemon_running: bool, compose_ok: bool, docker_version: Option<String>, arch: Option<String>, error: Option<String>`)
  - `fn start_daemon() -> Result<()>`(Windows:先试 `Start-Service com.docker.service`,失败则启动 `"%ProgramFiles%\Docker\Docker\Docker Desktop.exe"`,轮询 `docker info` 最多 60s)
  - `fn list_images() -> Result<Vec<ImageInfo>>`(`ImageInfo { repository, tag, size_bytes, created, id }`,解析 `docker images --format {{json .}}`)
  - `fn tag_image(image: &str, new_tag: &str) -> Result<()>`;`fn save_gzip(image: &str, out_path: &Path, progress_cb: impl Fn(u64)) -> Result<u64>`(实现:`docker save` 子进程 stdout → `flate2::write::GzEncoder` 写文件,按已写入字节数回调;返回压缩后字节数)

- [ ] **Step 1: 写失败测试**(格式解析与 tag 拼装,不依赖真实 docker):

```rust
#[test]
fn test_parse_images_json_line() {
    let line = r#"{"Containers":"N/A","CreatedAt":"2026-08-01 10:00:00 +0800 CST","ID":"abc123","Labels":null,"Repository":"myapp","Tag":"latest","Size":"300MB"}"#;
    let info = parse_image_line(line).unwrap();
    assert_eq!(info.repository, "myapp");
    assert_eq!(info.tag, "latest");
}

#[test]
fn test_deploy_tag_format() {
    assert_eq!(make_deploy_tag("myapp", "latest"), format!("myapp:{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
}
```

- [ ] **Step 2: 失败 → 实现 → 通过 → Commit** `feat: docker cli wrapper and host checks`

---

### Task 3: SSH/SFTP 模块

**Files:**
- Create: `src-tauri/src/ssh.rs`
- Test: `src-tauri/src/ssh.rs` 内单测(纯函数部分)

**Interfaces:**
- Produces:
  - `struct SshClient`,构造:`SshClient::connect(cfg: &ServerConfig, password_plain: Option<&str>) -> Result<Self>`(Key 类型读私钥文件,Password 类型用解密后密码)
  - `async fn exec(&mut self, cmd: &str, on_output: impl Fn(&str)) -> Result<i32>`(实时回显 stdout/stderr,返回退出码)
  - `async fn sftp_upload(&mut self, local: &Path, remote_dir: &str, remote_name: &str, on_progress: impl Fn(u64, u64)) -> Result<()>`(按文件总大小回调进度)
  - `async fn sftp_upload_dir(&mut self, local_dir: &Path, remote_dir: &str, ...) -> Result<()>`(递归,自动 mkdir -p)
  - `fn check_server_env(report: ...)`:在既有连接上依次 exec `docker --version`、`docker compose version`、`gzip --version`、`test -d <remote_dir>`,产出 `ServerCheckReport { docker, compose, gzip, remote_dir_exists, disk_free_gb, errors: Vec<String> }`
  - `fn install_docker_cmd() -> &'static str` = `"curl -fsSL https://get.docker.com | sh"`(供一键安装按钮使用)

- [ ] **Step 1: Cargo.toml 添加** `tokio = { version = "1", features = ["full"] }`、`russh = "0.45"`、`russh-sftp = "2"`、`async-trait = "0.1"`
- [ ] **Step 2: 写失败测试**(命令拼装):

```rust
#[test]
fn test_mkdir_p_command() {
    assert_eq!(mkdir_p_cmd("/opt/a/b/c"), "mkdir -p '/opt/a/b/c'");
}
```

- [ ] **Step 3: 失败 → 实现 → 通过 → Commit** `feat: ssh/sftp module with key and password auth`

---

### Task 4: 密码 DPAPI 加密

**Files:**
- Create: `src-tauri/src/crypto.rs`

**Interfaces:**
- Produces: `fn dpapi_protect(plain: &str) -> Result<String>`(返回 base64),`fn dpapi_unprotect(enc: &str) -> Result<String>`(Windows DPAPI CurrentUser 作用域)

- [ ] **Step 1: 写失败测试**(同机同用户可逆):

```rust
#[cfg(windows)]
#[test]
fn test_protect_roundtrip() {
    let enc = dpapi_protect("secret").unwrap();
    assert_ne!(enc, "secret");
    assert_eq!(dpapi_unprotect(&enc).unwrap(), "secret");
}
```

- [ ] **Step 2: 失败 → 实现 → 通过 → Commit** `feat: dpapi password protection`

---

### Task 5: Tauri commands 与事件总线

**Files:**
- Create: `src-tauri/src/commands.rs`
- Modify: `src-tauri/src/lib.rs`(invoke_handler 注册)

**Interfaces:**
- Consumes: Task 1-4 全部模块。
- Produces(前端可 invoke 的命令,统一返回 `Result<T, String>`):
  - `get_config() -> AppConfig` / `save_config_cmd(cfg)`
  - `host_check() -> HostCheckReport` / `start_docker()`
  - `list_images() -> Vec<ImageInfo>`
  - `test_server(server_id, password_plain: Option<String>) -> ServerCheckReport`
  - `server_env_check(server_id) -> ServerCheckReport` / `install_server_docker(server_id, password_plain)`
  - `deploy(req: DeployRequest)` — 异步 spawn,进度通过 `emit("deploy-progress", {step, total, current, message})` 与 `emit("deploy-log", line)` 推送;`DeployRequest { image, repository, tag, server_id, project_id, use_date_tag: bool, password_plain: Option<String> }`
  - `cancel_deploy()`(AtomicBool 置位,各步骤检查后中止)

- [ ] **Step 1: 实现 commands.rs**(组装:deploy 流程 = tag → save_gzip 到临时目录 → sftp 上传包与 fileMappings → exec `docker load -i <remote_tmp>` → exec `docker compose -f <compose> up -d` → 清理远端 tar)
- [ ] **Step 2: `cargo check` 通过,前端先不接** — Commit `feat: tauri commands and deploy pipeline with progress events`

---

### Task 6: 前端 — 环境检测页 + 布局框架

**Files:**
- Create: `ui/index.html`, `ui/app.js`, `ui/style.css`, `ui/check.js`

**Interfaces:**
- Consumes: `invoke("host_check")`、`invoke("start_docker")`、事件监听。
- Produces: 左侧导航(检测/镜像/服务器/部署 4 页,单页切换 `data-page`),全局状态 `window.AppState = { hostOk: bool }` 存 localStorage 供其他页读取。

- [ ] **Step 1: 实现 4 页导航壳 + 检测页**:检测项逐条渲染(✅/❌ + 名称);未通过项显示"一键启动/一键安装"按钮与"复制命令"按钮(`navigator.clipboard.writeText`);全部通过后设置 `AppState.hostOk=true` 并刷新导航中其他页可用态(未通过时其他页按钮 disabled + tooltip 提示"环境检测未通过")。
- [ ] **Step 2: `npm run tauri dev` 手工验证**:停止 Docker Desktop → 检测页显示未通过 → 点一键启动 → 轮询变绿。Commit `feat: ui shell and host check page`

---

### Task 7: 前端 — 镜像列表页

**Files:**
- Create: `ui/images.js`
- Modify: `ui/index.html`, `ui/app.js`

- [ ] **Step 1: 实现** `invoke("list_images")` 渲染表格(仓库、tag、大小、创建时间),顶部搜索框过滤,行尾"部署"按钮跳转部署页并携带选中镜像。
- [ ] **Step 2: 手工验证 + Commit** `feat: image list page`

---

### Task 8: 前端 — 服务器管理页

**Files:**
- Create: `ui/servers.js`
- Modify: `ui/index.html`, `ui/app.js`

- [ ] **Step 1: 实现**:服务器列表卡片;新增/编辑表单(名称/IP/端口/用户名/认证方式切换[私钥路径|密码]/远程项目路径);"测试连接"调用 `test_server`;展开区显示服务器环境检测结果(同检测页样式),未通过项带"一键安装 Docker"与"创建远程目录"按钮;"部署项目"配置区维护 projects(文件映射行:本地路径 + 服务器相对路径 + 目录勾选)。
- [ ] **Step 2: 手工验证 + Commit** `feat: server management page`

---

### Task 9: 前端 — 部署向导页

**Files:**
- Create: `ui/deploy.js`
- Modify: `ui/index.html`, `ui/app.js`

- [ ] **Step 1: 实现**:选择镜像(下拉)+ 服务器(下拉)+ 项目(下拉)+ "打日期 tag"勾选 → "开始部署";五步进度条(当前步骤高亮 + 百分比),下方终端风格日志区(监听 `deploy-log`);部署中"取消"按钮可用;结束显示成功/失败横幅(失败含失败步骤名)。部署前调用 `server_env_check`,未通过则拦截并提示跳转服务器页。
- [ ] **Step 2: 手工验证** 向真实测试服务器完整部署一次。Commit `feat: deploy wizard page`

---

### Task 10: 打包与验收

**Files:** Modify: `src-tauri/tauri.conf.json`

- [ ] **Step 1: 配置 NSIS 打包**(`"bundle": { "targets": ["nsis"] }`,productName/图标),Run: `npm run tauri build`,确认生成安装包并可安装运行。
- [ ] **Step 2: 验收清单执行**(spec 第 7 节手工集成测试清单逐项过:检测页失败分支、双认证方式、真实部署、旧 tag 回滚)。
- [ ] **Step 3: Commit** `chore: release build config` + 打 tag `v0.1.0`。
