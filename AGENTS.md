# AGENTS.md — DockerDeploy SSH

> 给未来代理的工作说明。项目文档全为中文。**细节以 [wiki/](wiki/README.md) 为准**，本文件只收录「不看会踩坑」的硬事实。

## 项目是什么

Windows 桌面客户端（Tauri 2）：把本地构建的 Docker 镜像一键部署到自己的服务器 —— `docker save → gzip → SFTP → docker load → compose up`，不依赖镜像仓库与 CI/CD。

- 后端：纯 Rust（`src-tauri/src/`，全部业务逻辑）；前端：原生 HTML/CSS/JS（`ui/`，**无框架、无打包器、无 npm 运行时依赖**）。
- 主分支 `main`；当前版本 v6.4.0，`cargo test` 基线 **363 passed / 13 ignored**，命令 **106** 个（实测口径 = 数 `lib.rs` 的 `generate_handler![]`）。

## 目录导览

| 路径 | 内容 |
|---|---|
| `src-tauri/src/lib.rs` | Builder 组装 + **命令注册唯一权威** |
| `src-tauri/src/commands/` | 编排层（部署/回滚/清理/迁移/断点/批量/预览等 11 文件） |
| `src-tauri/src/*.rs` | 功能模块：config / ssh / docker / stack / notify / update / manage* / migrate_project / probe / profiles / **deploy_schedule** / errors / tray_status |
| `ui/` | 前端整目录即 Tauri frontendDist（**编译期内嵌**）；`index.html` 的 script 加载顺序有依赖 |
| `wiki/` | 唯一权威文档（7 篇） |
| `verify/` | 零依赖 Node 校验脚本（不参与构建） |
| `UPGRADE-PLAN.md` / `ROADMAP.md` | 批次技术记录 / 进度与执行协议（事实来源） |
| `2026-09-13-待修复bug.md` | 历史审查清单（P0~P2 已全部修复，留档） |

## 常用命令

```bash
npm run tauri dev            # 开发运行（改 JS/HTML/CSS 必须重编译重启才生效）
npm run tauri build          # 发布构建（NSIS 安装包）
cd src-tauri && cargo test   # 单测；真机测试: cargo test docker:: -- --ignored（需本机 Docker 运行）
cd src-tauri && cargo clippy # 与基线逐条比对：新代码零新增告警（CI 不加 -D warnings）
node --check ui/xxx.js       # 改 JS 后逐个语法检查
node verify/form-validation.js && node verify/bridge-integrity.js && node verify/scope-integrity.js
node verify/contract-smoke.js    # 前后端命令/事件集合双向核对（新增动态调用点需同步其补充表）
node verify/doc-consistency.js   # 版本号/命令数/测试数/七篇页首版本戳；发版后 --write --tests=N 更新缓存
```

- **不要直接运行 `src-tauri/target/debug/app.exe`** —— debug 构建内嵌 dev 地址，会连不上 127.0.0.1。
- `DD_CONFIG_DIR` 环境变量可重定向配置目录（仅 debug；release 已编译期剔除）。

## 硬约束（违反即返工）

1. **契约命名**：invoke 参数名 camelCase（Tauri 自动映射）；对象字段与事件载荷**默认 snake_case**。camelCase 例外清单在 `wiki/04-契约参考.md` 开头 —— 读错命名会静默拿到 `undefined`（已多次踩坑）。
2. **新命令**必须三处同步：`lib.rs` 注册 + `wiki/04` 登记 + 命令计数更新。
3. **配置保存**一律「新鲜 `get_config` → 就地改字段 → 全量写回」；服务器编辑保存走 `save_server_entry`（密文 merge）。
4. **密文哨兵 `*` 绝不落盘**：`get_config` 返回的是只读视图，还原是后端不变量（曾把全部真实密文冲掉，见 `wiki/07`）。
5. **连接凭据**：任何新 connect 点必须经 `resolve_password` / `resolve_key_passphrase` 解析后透传（有源码级守护测试扫描全仓，直传 `None` 会红）。
6. **错误处理**：判定按错误码（后端 `[dderr:<code>]` 前缀；前端 `parseErrCode` / `errCodeOf`），不要新写文案匹配。
7. **死代码勿动**：后端 `deploy_batch` / `run_batch_deploy` 无任何调用点；批量部署是**前端编排**（`ui/deploy.js` 的 `st.batch`）。
8. **配置写互斥**：新增配置写点一律走 `config.rs` 的 `update_config`（锁内 load→改→save）。
9. **远程操作互斥收口（第二十二批）**：任何新的「后端自主发起/长时占用服务器」功能必须取 `acquire_remote_op()`（RAII guard）——现有接入点：`run_one_deploy(_stack)` 顶部 / 回滚三入口 / 迁移两入口 / 定时调度。被拒文案统一、按路径表达（详见 wiki/07）。
10. **版本历史不变量**：`save_config` 先快照后覆盖（`config/.history/`，全等去重、cap 20、失败仅告警不阻断）；新增**绕过 save_config 的直写路径**（如导入）必须在覆盖前调 `snapshot_config`。

## 前端（CSS/UI）纪律

完整版见 `wiki/03-前端说明.md` 的「前端硬约束」「设计语言」两节；最常被 judge 打回的：

- 字体轨道：`--font-mono/--font-cond` 只承载 ASCII；含中文元素一律 `--font-cjk`。
- 圆角全站 0（功能性控件 2–4px）；唯一例外 `#update-confirm-modal`（用户指定，勿推广）。
- flex：文字列 `min-width: 0`；整行独占子项显式 `width: 100%`；按钮 `flex: none`。
- 动效走 `--ark-dur-*` / `--ark-ease-out` token；只用 `var(--ark-*)` 颜色，禁新色值、禁 emoji、禁红（失败 = 墨底白字）。
- 防 XSS：动态数据一律 `createElement`/`textContent`；innerHTML 仅限静态骨架/文案。
- 数据表行 hover 是整行反白 —— 行内任何身份色元素必须同步补 hover 豁免（已两轮漏网）。
- 新增模态：焦点三件套 + Esc 用 `window.isTopModal` 仲裁 + 关闭守卫先例。
- 拆 JS 文件：桥接只允许**单次** `window.<Kit>` 赋值；拆出文件要么进桥、要么自带定义（`verify/bridge-integrity.js` + `scope-integrity.js` 守护）。
- 事件监听模块级单次注册守卫；监控/终端**先订阅事件再 invoke**，unlisten 成对清理。

## 验证与发布流程

- 逻辑类改动先跑 `verify/` 四脚本（比开浏览器快）；UI 视觉验证用**浏览器 + Tauri 桩**（`ui/_tauri-stub.js` + `ui/_judge-preview.html` + 本机无缓存静态服务，browser-use 截图交 judge）——**不用 computer-use**（用户常在全屏游戏）。**用完删桩、停服务、关页面**。
  - **IAB 按 URL 缓存资源**：桩验证前给预览页所有脚本加 `?v=N`（或起 no-cache 服务），否则改完 JS 看到的还是旧代码；`AppBus.on` 的 unlisten 是异步返回——任何「只判 unlisten 的幂等守卫」都无效（终端双注册事故，见 UPGRADE-PLAN 第二十二批二）。
- **push 前**：`cargo test` 必须**亲眼确认 `test result: ok`**；绝不把验证与提交串进同一条 `&&` 管道（grep/head 会吞失败输出，曾把编译失败的提交推上 main）。
- **GitHub 走本地代理**：`git -c http.proxy=http://127.0.0.1:12450 -c https.proxy=http://127.0.0.1:12450 push origin main`；SSH 连服务器直连、不走代理。
- **发版仅在用户明说时**：打附注 tag（`vX.Y.Z` 须与 `tauri.conf.json` 严格一致，`release.yml` 有防呆）→ push tag 触发发布；不要逐批打 tag。
- 版本号真值 = `tauri.conf.json` ↔ `Cargo.toml`，同步 `wiki/README.md`、`ROADMAP.md`（及 `package.json`），收尾跑 `node verify/doc-consistency.js`。
- 设计改动基准：`ark-ui` skill（主题/身份）+ `ui-ux-pro-max`（细节）；结论记录进 `UPGRADE-PLAN.md` + `wiki/03` + 三处版本号。

## 先读什么

- 改动前：`wiki/README.md` → 对应篇（架构 `01` / 后端 `02` / 前端 `03` / 契约 `04` / 构建 `05` / 部署回滚 `06` / 安全 `07`）。
- 当前进度与每阶段执行协议：`ROADMAP.md` 的「执行协议」「硬约束」「当前状态速览」。
- 动密文、哨兵、注入防护、TOFU、导出加密等安全敏感区前：必读 `wiki/07-安全与已知取舍.md`。
