# ROADMAP — 分阶段路线图与执行状态(第九批起)

> **本文件是分阶段执行的进度事实来源**;各批技术细节的权威记录在 `UPGRADE-PLAN.md`
> (第九批起),本文件维护「已完成 / 进行中 / 待办」视图与每阶段的开工前置条件。
> 执行协议:每个阶段完成 → 走完整验证链 → commit + push → **停下汇报,用户批复后才进下一阶段**;版本号在批复时可调。

---

## 执行协议(每阶段固定动作)

1. 开工前:核实前置条件(每阶段列有「开工前必核实」项)
2. 实施:遵守硬约束(见下)
3. 验证链:`cargo test` **显式读 `test result: ok` 行**(不许把失败吞进管道)→ `cargo clippy` 新代码零警告 → 改 JS 跑 `node --check` → 设计改动用「浏览器 + Tauri 桩」渲染截图交 judge(不用 computer-use,用户在玩游戏)
4. 文档:UPGRADE-PLAN 批次记录 + wiki 同步(受影响篇)+ 三处版本号(tauri.conf.json / Cargo.toml / wiki)
5. `git -c http.proxy=http://127.0.0.1:12450 -c https.proxy=http://127.0.0.1:12450 push origin main`(直连 github 不通;SSH 连服务器不走代理)
6. 发版 tag 仅在用户明确说发时打

## 硬约束(违反即返工)

1. 字体轨道:`--font-mono/--font-cond` 只承载 ASCII;含中文的元素一律 `--font-cjk`
2. flex:文字列 `min-width: 0`,整行独占的子项显式 `width: 100%`,按钮 `flex: none`
3. 圆角:全站 0(控件 2-4px);唯一例外 `#update-confirm-modal` 圆润卡片
4. 动效走 `--ark-dur-*` token,缓动 `--ark-ease-out`;flex 容器内子项外边距**不折叠**
5. 禁区:`st.rb*` 回滚体系(已拆至 deploy-rollback.js,先例口径不变);`deploy_batch` 后端编排是死代码勿动勿调;防 XSS 一律 createElement/textContent
6. 新命令必在 lib.rs 注册 + wiki/04 登记(计数同步);契约命名:默认 snake_case,camelCase 例外清单见 wiki/04:5-11(读错静默 undefined)
7. 配置保存一律「新鲜 get_config → 就地改 → save_config_cmd 全量写回」

## 验证工作流(第十二批起)

UI 验证用**浏览器 + Tauri 桩**(computer-use 已弃用):`ui/_tauri-stub.js`(假 `window.__TAURI__`,invoke 按命令名返回 MOCK)+ `ui/_judge-preview.html`(index.html 副本在 app.js 前注桩)→ `python -m http.server 8799`(repo 根)→ browser-use(IAB)打开 `http://127.0.0.1:8799/ui/_judge-preview.html`,切页用 `tab.playwright.evaluate("window.showPage('xxx')")`(dock 按钮 getByRole click 会超时)→ 截图 writeFile 存 PNG 交 judge → **用完删桩/停服务器/关预览页**。详见记忆 `browser-verify-workflow`。

---

## 已完成

### 第九批 v5.8.2(阶段一:细节优化)— commit 至 171f783
- 设置中心「打开日志文件夹」(`open_logs_dir`,第 94 个命令,explorer 打开 app_dir()/logs)
- errText 8 处重复上收 app.js `window.errText`(超集语义:空值兜底「未知错误」)
- 文档计数同步(测试 269→283、命令 92/93→94、deploy_batch 死代码标注)

### 第十批 v5.9.0(阶段二:概览磁盘用量)— commit 至 e79f436
- `host_metrics_cmd` 追加 `==DISK` 段(`df -kP / /var/lib/docker`);挂载点恰为 /var/lib/docker = 独立文件系统才单列
- ManageOverview +7 展示串字段;前端磁盘占用宽格第二行 `.overview-sub`(不新增概览格,防 3 格空洞)
- 已知取舍:Docker 自定义 data-root 未探测(按默认 /var/lib/docker)

### 第十一批 v5.10.0(阶段三:部署时预填版本标题/说明)— commit 至 773b842
- 整栈选项区可选「版本标题」+「版本说明」;成功后前端 `writeReleaseNotes` 用历史 `release_dir`(lastIndexOf 拆 dir/ts)调 `rollback_set_release_notes`;失败/取消不写(续传成功同入口补写);批量恒不写
- **后端顺序调整**:部署历史落盘挪到 emit deploy-done **之前**(原 emit→通知(await)→落历史有竞态)
- 真机反馈修复:版本详情保存路径翻倍(第七批潜伏 bug)——`RollbackReleaseDetail.dir` 是归档完整路径,模态保存误当项目目录;改用 selectedDir
- **待真机复测**:保存版本标题/说明 + 整栈部署后回滚中心看自动补写

### 第十二批 v5.11.0(阶段四:结构治理)— commit 至 6cee803
- Rust:`commands.rs`(10541 行)→ `src/commands/` 11 文件(mod 1261 共享设施+再导出 / deploy 2057 / rollback 1470 / cleanup 1243 / migrate 475 / compose_sources 494 / host_server 367 / resume 434 / batch 292 / preview 281 / tests 2249);**lib.rs 零改动**;可见性最小放宽(pub(crate))
- JS:deploy.js 3420→2338(+deploy-rollback 620 / deploy-migrate 532);manage.js 3257→2013(+manage-stacks 1274,沿原作者「C 阶段独立状态」缝);servers.js 3019→2537(+servers-cleanup 490);桥接 = 宿主尾部 `window.DeployKit/ManageKit/ServersKit` + 入口同名 var 别名
- 存量 bug 修复:迁移模态 `insertBefore(标题, 未挂载 grid)` 必抛错(打开即崩,与拆分无关);改为挂载后紧贴插入
- 补测:manage_logs/manage_stats 代号守卫 + is_transport_error(+5)
- 基线:**290 passed / 13 ignored**(ssh 5 + docker 7 + update 1 真机测试,跑法 `cargo test -- --ignored`)

---

### 第十三批 v5.12.0(阶段五:表单校验体系)— 已完成(commit 见本批)
- `app.js` 三个统一助手:**`bindFieldValidation`**(失焦校验接线器,一份规则清单兼作提交期整体校验)、**`bindFormEnter`**(Enter 提交:仅单行文本 input、判 IME 组合、不越二次确认)、**`beginForm`**(表单语义:`<form novalidate>` + submit 恒拦截)
- 五处模态接入真实 `<form>` 语义:服务器表单 / 项目表单 / 通知模态 / 迁移模态(计划区与日志区留在 form 外)/ 单镜像回滚
- 规则清单:服务器 7 条、项目 7 条、通知 5 条、迁移 2 条、回滚 1 条、清理 1 条(新增候选:远程部署目录/迁移目录/清理起点绝对路径、迁移归档数量非阻断提示、通知主机·发件人·端口、部署页三下拉字段级错误)
- `setFieldError` 锚点修正:`closest('.form-row')`(路径类字段的父节点是 `.input-btn-row` flex 行,原实现把提示塞进按钮行抢宽度)+ 错误插在 `form-hint` 之前
- **顺带修复第十二批拆分的存量 bug**:`window.DeployKit` 被赋值两次(后者覆盖前者)→ 拆出的 deploy-rollback.js 顶层 6 个键为 undefined,**单镜像回滚模态打开即崩**;合并为单对象 + 新增 `verify/bridge-integrity.js` 桥接守护
- 验证:自建本地校验器 54 项断言全过 + 浏览器桩全链路 + 截图交 judge 4/4 pass;`cargo test` 290 passed 不变
- **待真机复测**:单镜像回滚模态能正常打开(DeployKit 修复后)+ 各表单失焦校验手感

### 第十四批 v5.13.0(阶段六:批量部署增强)— 已完成(commit 见本批)
- **开工前核实推翻了仓库文档记载**:批量逐台复用单发 `deploy`/`deploy_stack`,后端那两个命令**恒 `checkpoint = true`** → **批量失败/取消的台早就落盘了断点**;「批量不落断点」只描述未接入的死代码 `deploy_batch`。该错误记载是「批量失败后没有续传入口」长期存在的根因(wiki/06、wiki/07 限制 21/40/41 + 三处代码注释已全部更正)
- **A 续传入口**:批量结束后,行内「续传此台」(单台)+ 头部「续传未完成服务器(N 台)」(一键批量);点击时用 `deploy_resume_status` 现查断点(可能已被清理/被同键新部署覆盖);待续传台组装成普通批量队列(`resumeKey` 分支复用 `deploy_resume_start`),串行执行、收尾口径与单发一致
- **B 停止即时中止**:「停止批量」在当前台部署中时发 `cancel_deploy`(步骤边界生效),中断台进入待续传列表;**不自动续传**(停止是显式意图)
- **D 修断点裁剪泄漏**:断点表超上限裁剪时,被裁条目的本地临时 tar 一并回收(此前只删表项 → tar 永久留在临时盘且无法经「放弃断点」回收);新增单测
- **顺带修复严重存量 bug**:`st.batch.deferred` 存成对象却被当函数调用 → **批量部署第一台结束后永久卡死**(TypeError 被事件监听器吞掉);用 HEAD 版对照复现后改为可调用且带 `.promise` 的把手
- **视觉**:judge 首轮打回徽章跨行错列,修复后复审 pass(徽章列 + 定宽动作槽对齐)
- **顺带修复用户反馈的 UI bug**:回滚中心「已备注」徽章被拉满整行 —— 父容器 `.rollback-release-info` 是 column flex,`align-items` 默认 `stretch`。修法是 `.rollback-release-info > .badge { align-self: flex-start }`,**必须用后代选择器**:`window.fillBadge` 会整体重写 `node.className`,给徽章挂自定义类会被抹掉(首版按自定义类写,实测 `alignSelf: auto`、徽章 419px 仍拉满;改后代选择器后 50px/419px、不再拉伸)。
- 验证:`cargo test` 291 passed(基线 290 + 新单测)/ clippy 与基线逐条比对零新增 / 浏览器桩跑通全部续传路径(含断点已清理的边界)/ judge pass
- **待真机复测**:真实服务器上的批量失败 → 续传此台/一键续传;停止批量的即时中止手感

### 第十四批补丁 v5.13.1 — 修复「未能获取更新说明」
- **根因**:`update_check` 主路径把**剥了 v 前缀**的 `info.latest`(`"5.13.0"`)当 tag 传给 `releases/tags/{tag}` 端点,而 GitHub 要求真实 tag(`v5.13.0`)→ **恒 404**;抓取函数四个失败分支当时全部静默 `return String::new()` 且无日志,故前端只看到「未能获取更新说明」兜底。**自第八批引入,恒定失败(非偶发)**,用户跨版本升级时才暴露
- **修复**:主路径返回 `RedirectProbe { info, tag }`,抓取改用真实 tag;抓取函数四个分支补 `log::warn!`(带 URL 与状态码);真机测试强化为「probe.tag 等于 gh 权威 tag」+「真实 tag 抓到非空 / 剥前缀拿到空」对照断言
- 验证:`cargo test` 292 passed / clippy 零新增 / 真机测试 `--ignored` 实测通过(走真实代理与 API)
- 另核实:`reqwest::Proxy::all("127.0.0.1:12450")`(无 scheme 的代理串)返回 Ok,不是代理解析问题

### 第十五批 v5.14.0(候选池:托盘 tooltip 动态态)— 已完成(commit 见本批)
- 新模块 `src-tauri/src/tray_status.rs`:tooltip 由纯函数 `tooltip_text` 生成(10 单测覆盖状态组合),**状态由后端管线自行维护**(部署/回滚/监控挂钩,零前端改动)—— 窗口隐藏、前端卡死时托盘仍准确
- 优先级:部署中 > 回滚执行中 > 监控中 > 空闲;空闲附「上次部署成功/失败/已取消」终态(新一轮开始即清)
- 挂钩点:两部署管线步骤 0(开始 + 目标串 + 批量前缀)/ `emit_progress`(步骤,**置于批量抑制之前**)/ `finish_deploy_run`(终态)/ `finish_rollback`(进入/退出)/ 监控 start·stop·熔断
- 细节:监控熔断自动退出也清态(否则前端不在线时托盘永远「监控中」);批量逐台是独立单发,台间 tooltip 瞬闪终态再回「部署中」(状态恒准确,间隔 <1s)
- 验证:`cargo test` 302 passed(基线 292 + 10)/ clippy 零新增 / 真机冒烟进程存活;**tooltip 悬停效果待用户确认**(系统绘制,无自动化断言手段)

---

## 待完成

### 阶段六(第十四批):批量部署增强 ✅ 已完成(见「已完成」节第十四批)

### 候选池(任意批复节点可插入,无先后承诺)
| 项 | 要点 |
|---|---|
| 容器级指标 | 概览只有宿主机维度;manage_stats 流扩展容器聚合 |
| 部署前强制预览 | `preview_stack_changes` 已实现未接入部署流程(wiki/03 明示「独立功能」);做成部署前可选/强制预览 |
| 服务器定时探活+通知 | notify 体系已有桌面/SMTP/webhook;定时探活失败推送 |
| 结构化错误码 | 现依赖中文文案子串匹配(is_transport_error 等,wiki/07 限制 13);改结构化错误枚举 |
| image_filter 消费 | 配置字段未被消费(项目下拉过滤,wiki/07 限制 3) |
| .env 非 UTF-8 | lossy 替换会乱码(wiki/07 限制 17) |
| Docker data-root 探测 | 概览磁盘按默认 /var/lib/docker 采样;可加 `docker info -f` 探真实数据根 |

### 遗留待真机验证(用户下次实机操作时顺手确认)
1. 版本详情保存版本标题/说明(路径翻倍修复后)
2. 整栈部署填标题/说明 → 回滚中心看归档自动补写
3. 05 概览磁盘数字与服务器 `df -h` 对照(后端 df 解析只过了单测)
4. 「迁移项目…」模态能正常打开(insertBefore 修复后)
5. **单镜像回滚模态能正常打开**(第十三批修复 DeployKit 覆盖 bug 后;此前打开必报错)
6. 各表单失焦校验手感(服务器/项目/通知/迁移/回滚/清理)+ Enter 提交是否符合直觉
7. **批量部署的失败续传**:真实服务器上批量中途失败 → 面板「续传此台」/「续传未完成服务器」是否按断点正确续上
8. **「停止批量」的即时中止**:当前台是否在步骤边界及时停住、中断台是否出现在待续传列表

---

## 当前状态速览

- 版本 v5.14.0;main = origin/main;基线 `cargo test` 302 passed / 13 ignored
- 命令 94 个(lib.rs 注册;wiki/04 已同步);JS 15 文件(index.html 加载顺序见 wiki/03:13)
- 前端结构:12 模态;commands/ 11 文件;三大 JS 主文件 2338/2013/2537 行
- 表单体系(第十三批):5 处模态有真实 `<form novalidate>` 语义;失焦校验 + Enter 提交由 app.js 三助手统一承担
- 批量部署(第十四批):失败/取消台可续传(单台 + 一键批量);「停止批量」步骤边界即时中止;批量恒落断点(与死代码 `deploy_batch` 的「不落断点」无关)
- 本地校验脚本 `verify/`(零依赖 Node,不参与构建):`form-validation.js`(表单助手 54 断言)/ `bridge-integrity.js`(桥接完整性);改动表单助手或拆出文件桥接时先跑这两个
- dev 实例:target/debug/config(正式版数据拷贝);前端资源编译期内嵌,**改 JS 后必须重编译重启动才生效**
