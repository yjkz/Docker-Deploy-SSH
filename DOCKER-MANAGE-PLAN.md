# 远程 Docker 管理模块 — 三阶段实施计划

> 在 docker-deploy-ssh 中新增「服务器 Docker 管理」页面（第 05 页），通过现有 SSH 通道在远程服务器执行 docker 命令，实现容器 / 镜像等资源的查看与操作。
>
> 硬约束：**低耦合，禁止对已有功能及代码修改造成功能失效。**

**进度**：A 阶段 ✅ 已完成（v4.5.0 / 2026-09-05）｜B 阶段 ⬜ 待开始｜C 阶段 ⬜ 待开始

---

## 1. 设计原则（低耦合）

| 原则 | 具体做法 |
|---|---|
| **新增为主** | 后端新建 `manage.rs`、前端新建 `manage.js`，所有新逻辑在新文件内 |
| **仅追加不修改** | `lib.rs` 只加 `pub mod manage;` + 命令列表末尾追加；`index.html` 只加导航按钮 + section + script；`style.css` 只在文件末尾追加新样式 |
| **现有模块零改动** | `commands.rs` / `docker.rs` / `config.rs` / `crypto.rs` / `stack.rs` / `history.rs` 全程不改；`ssh.rs` 在 A、B 阶段零改动，仅 C 阶段**纯追加** `exec_interactive` 方法（不改现有方法） |
| **前端旧逻辑零改动** | `app.js` / `check.js` / `images.js` / `servers.js` / `deploy.js` 不改 |
| **自行组装连接** | `manage.rs` 自己实现 `find_server` / `with_timeout` / `connect_server`，通过公开 API 组合：`config::load_config` → `commands::resolve_password`（pub）→ `ssh::SshClient::connect` → `ssh::exec_collect`（pub(crate)），不调用 commands.rs 私有函数 |
| **不依赖本机 Docker** | 新页面不在 `LOCKED_PAGES` 中，环境检测未通过也可进入（纯远程操作） |

---

## 2. 架构

### 2.1 模块边界

```
┌─ 后端 src-tauri/src/ ─────────────────────────────────────┐
│  manage.rs              ← 新增:所有远程管理命令 + 数据结构  │
│  commands.rs            ← 不改(仅复用其 pub resolve_password)│
│  ssh.rs/config.rs/crypto.rs ← A/B 不改,经 pub API 调用;      │
│                              ssh.rs 仅 C 阶段追加交互方法     │
└───────────────────────────────────────────────────────────┘
┌─ 前端 ui/ ────────────────────────────────────────────────┐
│  manage.js              ← 新增:页面逻辑                    │
│  index.html             ← 追加:05 导航 + section + script  │
│  style.css              ← 追加:新页面样式                   │
│  app.js / 其他页面 JS    ← 不改                             │
└───────────────────────────────────────────────────────────┘
```

### 2.2 数据流

```
前端选服务器 → AppBus.invoke('manage_xxx', {server_id, password_plain})
    → manage.rs: load_config → find_server → resolve_password → SshClient::connect
    → exec_collect 执行 docker 命令(--format json)
    → serde_json 解析为结构化数据
    → 返回前端渲染
```

### 2.3 远程命令策略

- **列表类命令统一用 `--format json`（= `--format '{{json .}}'` 简写），输出是 NDJSON**：每个资源一行独立 JSON 对象，不是 JSON 数组。Rust 端按行 split、跳过空行后逐行 `serde_json::from_str` 收集为 `Vec`（已对照 Docker 官方文档确认 ps / images / system df / volume ls / network ls 均为此行为）
- **Docker 输出字段为 PascalCase**（`ID` / `Names` / `Image` / `State` / `Status` / `Ports` / `CreatedAt` / `Repository` / `Tag` / `Size`），Rust struct 必须加 `#[serde(rename_all = "PascalCase")]`，否则反序列化全部失败
- 单对象命令：`docker info --format json` 输出**单个** JSON 对象；`docker inspect <id>` 输出 **JSON 数组**（即使只查一个，前端取 `[0]`）
- 容器 ID / 镜像名等参数经单引号转义（manage.rs 内自实现 `shell_quote`，模式同 ssh.rs 的 `shell_single_quote`）
- 操作类命令（start/stop/rm/pull 等）检查退出码，非 0 时把合并输出原文作为 Err 带回前端
- **权限兜底**：SSH 用户若不在 docker 组，所有命令报 `permission denied ... /var/run/docker.sock`；overview 首次执行命中该错误时返回明确中文提示（当前用户无 Docker 权限，请加入 docker 组或使用 root），不笼统报连接失败
- **前后端命名约定（沿用现有规则）**：JS 侧 `invoke` 参数名用 camelCase（`serverId` / `passwordPlain` / `containerId`），Tauri 2 自动映射到 Rust snake_case；返回结构体内部字段一律 snake_case（serde 未设 rename_all 时），前端按 snake_case 读取

---

## 3. A 阶段：核心双对象（MVP）✅ 已完成

> 目标：容器 + 镜像的完整查看与操作，覆盖日常 80% 运维场景。

### 3.1 后端命令清单（manage.rs）

| # | 命令 | 远程执行 | 返回 |
|---|---|---|---|
| 1 | `manage_list_servers()` | — | `Vec<{id, name, host}>` |
| 2 | `manage_overview(server_id, password_plain)` | `docker info --format json` + `docker system df --format json` | 版本 / OS / 内核 / 容器统计 / 镜像数 / 各类型磁盘占用（一次 SSH 连接内顺序执行两条命令） |
| 3 | `manage_list_containers(server_id, password_plain)` | `docker ps -a --format json` | `Vec<ContainerInfo>` |
| 4 | `manage_container_inspect(server_id, password_plain, container_id)` | `docker inspect <id>` | `serde_json::Value`（原始 JSON，前端挑关键字段展示） |
| 5 | `manage_container_action(server_id, password_plain, container_id, action)` | `docker start/stop/restart/rm <id>` | `{success, message}` |
| 6 | `manage_container_logs(server_id, password_plain, container_id, tail)` | `docker logs --tail <n> <id>` | `String`（合并 stdout+stderr） |
| 7 | `manage_list_images(server_id, password_plain)` | `docker images --format json` | `Vec<ImageInfo>` |
| 8 | `manage_image_pull(server_id, password_plain, image)` | `docker pull <image>` | `{success, message}` |
| 9 | `manage_image_remove(server_id, password_plain, image_id, force)` | `docker rmi [-f] <id>` | `{success, message}` |
| 10 | `manage_image_tag(server_id, password_plain, image, new_tag)` | `docker tag <image> <new_tag>` | `{success, message}` |

**数据结构（字段名对齐 Docker 的 PascalCase 输出）：**

```rust
// docker ps -a --format json 每行一个对象;字段名来自 Docker 官方文档
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerInfo {
    id: String,         // .ID        短 ID(默认截断为 12 位)
    names: String,      // .Names     容器名
    image: String,      // .Image     镜像名(非 ID)
    state: String,      // .State     running/exited/paused/created/dead/restarting
    status: String,     // .Status    人类可读(Up 2 hours / Exited (0) 3 days ago)
    ports: String,      // .Ports     端口映射文本
    created_at: String, // .CreatedAt 绝对时间(如 2026-09-01 12:00:00 +0000 UTC)
    // 其余字段按需加 #[serde(default)] 忽略缺失
}

// docker images --format json 每行一个对象
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageInfo {
    repository: String, // .Repository(dangling 时为 "<none>")
    tag: String,        // .Tag(dangling 时为 "<none>")
    id: String,         // .ID
    size: String,       // .Size 人类可读大小
    created_at: String, // .CreatedAt;另有 .CreatedSince 相对时间可直接展示
}
```

> 注意：不存在 `.Created` 字段，绝对时间是 `CreatedAt`、相对时间是 `RunningFor`（容器）/ `CreatedSince`（镜像），前端展示相对时间更友好。

### 3.2 前端页面结构（manage.js）

```
┌─ 页头:05 / 远程管理 / REMOTE ─────────────────────────────┐
│  [服务器下拉] [状态徽章] [手动刷新] [自动刷新 开关+间隔选择]│
├─ 概览面板(技术面板样式)───────────────────────────────────┤
│  Docker 版本 │ OS / 内核 │ 容器(运行/停止/总数) │ 镜像数 │ 磁盘占用 │
├─ Tab: [容器 CONTAINERS] [镜像 IMAGES] ────────────────────┤
│                                                          │
│  容器列表(data-table):                                    │
│  状态│名称│镜像│端口│创建时间│操作(启/停/重启/删/日志/详情)│
│  - 点击行展开 inspect 关键信息(折叠面板)                  │
│  - 日志按钮 → 模态框(等宽字体 + tail 行数选择)            │
│                                                          │
│  镜像列表(data-table):                                    │
│  仓库│标签│ID│大小│创建时间│操作(拉取/删除/打标签)        │
│  - 拉取:行内输入框 + 按钮                                 │
│  - 打标签:模态框(新镜像名:标签)                           │
└──────────────────────────────────────────────────────────┘
```

### 3.3 交互细节

- 切换服务器后立即拉取一次：概览 + 当前 Tab 列表。为避免一次切换建三次 SSH 连接，`manage_overview` 在一次连接内跑完 info + system df；容器/镜像列表各自独立命令（用户切 Tab 时才加载对应列表，不预取）
- 危险操作（rm / rmi）弹出二次确认模态框，确认后执行
- **删除运行中容器**：`docker rm` 对运行中容器会报错，确认框需明示"容器正在运行，将强制删除"并执行 `docker rm -f`；已停止容器用普通 `docker rm`
- 操作成功后自动刷新对应列表 + toast 提示
- 操作失败时 toast 显示服务端原始错误摘要
- 容器状态徽章颜色：running=信号青、exited/dead=中性灰、paused=黄、restarting=黄、created=灰
- 超时分级（manage.rs 自实现 with_timeout，对齐现有常量）：SSH 建连 15 秒、普通命令 60 秒、`docker pull` 300 秒
- 前端操作期间禁用对应按钮防重复点击，命令返回后恢复

### 3.3.1 定时自动刷新（A 阶段内置）

- **控件**：服务器栏放「自动刷新」开关（默认关闭）+ 间隔下拉，预设 `5s / 10s / 30s / 60s` + 「自定义…」
- **自定义间隔**：选中后弹出输入框填秒数，校验为正整数，范围 **3–300 秒**（下限 3s：每次刷新都要新建 SSH 连接，过于频繁会堆积连接；上限 300s）；非法输入回退上次有效值并 toast 提示
- **刷新对象**：概览面板 + 当前激活 Tab 的列表（容器或镜像），不预取非当前 Tab
- **定时器生命周期（防泄漏 / 防并发）**：
  - 仅停留在 05 页面且开关打开时运行；监听 `pagechange`，离开 05 页 `clearInterval`，回到 05 页按开关状态恢复
  - 切换服务器、切换 Tab 时重置定时器
  - **防重入**：上次刷新请求未返回时跳过本次 tick（用 in-flight 标志），不等的请求不排队，避免 SSH 连接堆积
  - 进行中的变更操作（pull / rm / rmi 等）期间暂停轮询，操作结束后立即刷新一次并恢复
- **无感知刷新**：只更新数据行与状态徽章，不折叠已展开的 inspect 详情、不关闭日志模态、不滚动用户正在查看的列表位置（按容器/镜像 ID 做 DOM 差异更新，而非整表 innerHTML 重建）
- **偏好持久化**：开关状态与间隔存 localStorage（键 `dd_manage_autorefresh` / `dd_manage_interval`），下次进入恢复；后端无需新增命令，复用现有 list / overview 命令
- 与 C 阶段 `docker stats` 实时监控的区别：定时刷新是**周期性重新拉取列表/概览**（离散、手动刷新的自动化）；C 阶段 stats 是 CPU/内存的**持续流式指标**，两者并存不冲突

### 3.4 A 阶段范围边界（故意不做，避免范围蔓延）

- 容器：不做 pause/unpause、rename、一次性 `docker exec` 命令（交互式 exec 在 C 阶段）
- 镜像：不做 push、build、save/load、prune（prune 现有「服务器管理」页已有 `prune_server`）
- 不做实时资源监控（docker stats 流式 CPU/内存指标在 C 阶段）；A 阶段提供手动刷新 + 可自定义间隔的定时自动刷新（见 3.3.1）
- 不做多服务器同时监控，单时刻只操作一台选中的服务器

### 3.5 完成记录（v4.5.0 / 2026-09-05）

- **后端**：`manage.rs` 10 个命令全部实现并通过 `cargo build` / `cargo test`（136 passed）
- **前端**：`manage.js` 容器/镜像双 Tab + 定时自动刷新（5/10/30/60s 预设 + 自定义 3-300s）+ localStorage 持久化
- **UI 美化**：操作按钮统一幽灵风格（删除用琥珀色暗示，行 hover 不反白）；表格固定列宽 + 文本截断；端口智能折叠（+N 徽章 + 点击展开）；创建时间格式化
- **低耦合验证**：`git diff` 确认 commands.rs / docker.rs / ssh.rs / config.rs / crypto.rs / 旧前端 JS 零改动；lib.rs / index.html / style.css 仅末尾追加
- **已知技术坑**：Docker `--format json` 的 ID 字段是全大写 `ID`（非 PascalCase 的 `Id`），需 `#[serde(rename = "ID")]`；不存在 `.Created` 字段，绝对时间是 `CreatedAt`
- **Release**：NSIS 安装包 2.93 MB（LTO + strip + opt-level=s），已发布至 GitHub Release v4.5

---

## 4. B 阶段：卷与网络

> 在 A 阶段基础上追加 volume / network 管理，后端和前端均为纯追加。

### 4.1 后端新增命令

| 命令 | 远程执行 |
|---|---|
| `manage_list_volumes` | `docker volume ls --format json` |
| `manage_volume_inspect` | `docker volume inspect <name>` |
| `manage_volume_create` | `docker volume create [--driver <d>] <name>` |
| `manage_volume_remove` | `docker volume rm <name>` |
| `manage_list_networks` | `docker network ls --format json` |
| `manage_network_inspect` | `docker network inspect <id>` |
| `manage_network_create` | `docker network create [--driver <d>] <name>` |
| `manage_network_remove` | `docker network rm <id>` |
| `manage_network_connect` | `docker network connect <net> <container>` |
| `manage_network_disconnect` | `docker network disconnect <net> <container>` |

### 4.2 前端追加

- Tab 增加：**卷 VOLUMES** / **网络 NETWORKS**
- 卷列表：名称、驱动、挂载点、创建时间、操作（查看 / 删除）
- 网络列表：名称、驱动、范围、已连接容器数、操作（查看 / 删除 / 连接容器 / 断开容器）
- 创建卷 / 创建网络：模态框表单（名称 + 驱动选择）

---

## 5. C 阶段：全功能面板

### 5.1 Compose 栈管理

- 扫描服务器上的 compose 项目目录（复用现有 `remote_dir` 配置 + 自动发现 `docker-compose.yml`）
- 后端：`manage_list_stacks` / `manage_stack_action`（up / down / ps / logs）
- 前端：栈列表 → 服务状态表 → 配置查看 / 日志查看

### 5.2 实时监控（docker stats）

- 后端：`manage_stats_start` / `manage_stats_stop`，通过 Tauri 事件 `manage-stats` 流式推送
- 远程执行 `docker stats --no-stream --format json`，定时轮询（默认 2 秒）
- 前端：实时资源面板（CPU% / 内存% / 网络 IO / 块 IO），信号色进度条

### 5.3 容器 Exec 终端

- 引入轻量终端组件（xterm.js 或同等），通过 SSH 建立交互式 shell（`docker exec -it <c> bash`）
- 后端：`manage_exec_start` / `manage_exec_write` / 事件流 `manage-exec-output`
- 前端：模态框内嵌入终端，支持 bash / sh 选择
- **已定方案：给 ssh.rs 追加 `exec_interactive` 方法（C 阶段执行，A/B 阶段 ssh.rs 仍零改动）**。交互式终端需要长期持有 channel 并双向写 stdin，而现有 `SshClient::exec` 是一次性模型（执行后读到通道 Close 才返回），且 `SshClient.handle` 是私有字段，manage.rs 无法自行开交互式通道。因此在 ssh.rs 中**纯新增**一个方法（不改现有任何方法签名与逻辑，不影响部署流程），能力为：开 session channel、request_pty、拉起 shell，返回可写 stdin 的句柄与输出回调通道；manage.rs 调用它实现 exec 终端

---

## 6. UI 设计规范（ark-ui）

### 6.1 设计契约

- **Family**: `ark`（黑白青工业信息系统）— 与现有页面一致
- **Depth**: `complex` — 与现有 `data-ark-depth="complex"` 一致
- **Scheme**: 跟随全局亮暗切换（`data-ark-scheme`）

### 6.2 配色

| 角色 | 色值 | 用途 |
|---|---|---|
| 墨 Ink | `#080a0b` | 暗色背景、dock、文本 |
| 纸 Paper | `#f4f6f6` | 亮色舞台背景 |
| 信号青 Signal | `#18d1ff` | 选中态、运行中容器、主操作、进度 |
| 状态黄 | `#c8eb21` | paused 容器、警告 |
| 危险红 | 复用现有 `.btn-danger` | 删除操作 |

### 6.3 复用现有组件

- `.page-head`（编号 + 中文标题 + 英文副标题）
- `.data-table` / `.table-wrap`（数据表格）
- `.badge` / `fillBadge()`（状态徽章）
- `.btn` / `.btn-primary` / `.btn-danger` / `.btn-sm`（按钮）
- `.modal-overlay` / `.modal-card`（模态框，复用 `servers-modal` 模式或新建独立模态）
- `.log-shell` / `.deploy-log`（日志面板样式）
- `.banner`（状态横幅）

### 6.4 新增样式（追加到 style.css 末尾）

- `.manage-server-bar`（服务器选择栏）
- `.manage-autorefresh`（自动刷新开关 + 间隔下拉/自定义输入控件组）
- `.manage-overview`（概览技术面板：1px 边框 + 左上角编号 + 状态行）
- `.manage-tabs`（Tab 分段控件，dock 风格）
- `.manage-detail-panel`（容器 inspect 折叠面板）
- `.manage-log-modal`（日志模态框内的等宽字体区域）
- 容器状态徽章变体：`.badge-running` / `.badge-exited` / `.badge-paused`

### 6.5 排版

- 中文标题用现有字体栈；ID / 端口 / 日志用等宽字体（`IBM Plex Mono, Consolas, monospace`）
- 英文微标签大写 + 字间距 `.08em–.18em`（如 CONTAINERS / IMAGES / REMOTE）
- 数字用等宽数字（tabular-nums）

---

## 7. 验证与回归

### 7.1 编译与测试

- `cargo build` 编译通过
- `cargo test` 现有全部测试通过（证明未破坏已有功能）
- `cargo clippy` 无新增 warning（manage.rs 内部）

### 7.2 手动回归（npm run tauri dev）

| 检查项 | 预期 |
|---|---|
| 01 环境检测 | 功能正常，hostOk 状态正确 |
| 02 镜像列表 | 本机镜像加载正常 |
| 03 服务器管理 | 服务器 / 项目 CRUD 正常 |
| 04 部署向导 | 单镜像 / 整栈部署流程正常 |
| **05 远程管理（新）** | 服务器选择 → 概览 → 容器列表 → 启停/日志/详情 → 镜像列表 → 拉取/删除/打标签 |
| 定时自动刷新 | 开关 / 预设间隔 / 自定义（3–300s 校验）生效；离开 05 页停止、返回恢复；操作期间暂停；慢响应下无请求堆积；刷新不打断已展开详情/日志模态 |
| 导航切换 | 5 个页面切换流畅，新页面不依赖 hostOk |
| 亮暗主题 | 新页面在 light/dark 下均正常 |

### 7.3 低耦合验证

- `git diff` 确认（A、B 阶段）：commands.rs / docker.rs / ssh.rs / config.rs / crypto.rs / 旧前端 JS 无改动
- lib.rs / index.html / style.css 的改动均为末尾追加
- C 阶段例外：ssh.rs 仅允许末尾追加 `exec_interactive`，diff 中不得出现对现有方法的修改

---

## 8. 文件变更清单

### 新增文件

| 文件 | 说明 |
|---|---|
| `src-tauri/src/manage.rs` | 远程 Docker 管理后端：10+ 命令 + 数据结构 + 连接辅助 |
| `ui/manage.js` | 远程管理页面前端逻辑 |

### 修改文件（仅追加）

| 文件 | 改动 |
|---|---|
| `src-tauri/src/lib.rs` | +`pub mod manage;`；`generate_handler![...]` 末尾追加 10 个命令 |
| `ui/index.html` | dock-nav 追加 `05 远程管理` 按钮（**初始不带 `disabled` 类**，与 03 服务器管理一致——不依赖本机 Docker；切勿照抄 02/04 的 disabled，否则 `refreshNav()` 只解锁 LOCKED_PAGES 会导致按钮永久灰置）；main 追加 `<section data-page="manage">`；body 末尾追加 `<script src="manage.js">` |
| `ui/style.css` | 文件末尾追加新页面样式（约 150–250 行） |

### 不修改文件（A、B 阶段）

- `src-tauri/src/commands.rs` / `docker.rs` / `config.rs` / `crypto.rs` / `stack.rs` / `history.rs` / `main.rs`
- `src-tauri/src/ssh.rs`（**A、B 阶段零改动**；仅 C 阶段在文件内**追加** `exec_interactive` 方法，不改现有方法）
- `ui/app.js` / `check.js` / `images.js` / `servers.js` / `deploy.js`
- `src-tauri/tauri.conf.json` / `Cargo.toml` / `package.json`（C 阶段若引入 xterm.js 才需改 package.json/前端依赖，届时单独评估）

---

## 9. 实施顺序建议

1. ✅ **A 阶段后端**：manage.rs 骨架 + 连接辅助 + list_servers + overview + list_containers + list_images（先跑通只读）
2. ✅ **A 阶段前端骨架**：index.html 追加 + manage.js 页面框架 + 服务器选择 + 概览渲染 + 手动刷新
3. ✅ **A 阶段容器操作**：action / inspect / logs + 前端交互
4. ✅ **A 阶段镜像操作**：pull / remove / tag + 前端交互
5. ✅ **A 阶段定时自动刷新**：开关 + 预设/自定义间隔 + 定时器生命周期（离页清理、防重入、操作期暂停、按 ID 差异更新）+ localStorage 持久化
6. ✅ **A 阶段样式**：style.css 追加 + 亮暗适配 + 按钮美化 + 列表优化（固定列宽/端口折叠/时间格式化）
7. ✅ **回归验证**：编译 + 测试 + 手动 5 页回归 + 自动刷新并发/泄漏检查
8. ⬜ **B 阶段**：卷 / 网络（纯追加）
9. ⬜ **C 阶段**：ssh.rs 追加 exec_interactive → Compose / stats / exec（按需）
