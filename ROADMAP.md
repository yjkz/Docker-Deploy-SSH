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
- **真机复测 ✅(2026-09-16)**:保存版本标题/说明 + 整栈部署后回滚中心看自动补写

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
- **真机复测 ✅(2026-09-16)**:单镜像回滚模态能正常打开(DeployKit 修复后)+ 各表单失焦校验手感

### 第十四批 v5.13.0(阶段六:批量部署增强)— 已完成(commit 见本批)
- **开工前核实推翻了仓库文档记载**:批量逐台复用单发 `deploy`/`deploy_stack`,后端那两个命令**恒 `checkpoint = true`** → **批量失败/取消的台早就落盘了断点**;「批量不落断点」只描述未接入的死代码 `deploy_batch`。该错误记载是「批量失败后没有续传入口」长期存在的根因(wiki/06、wiki/07 限制 21/40/41 + 三处代码注释已全部更正)
- **A 续传入口**:批量结束后,行内「续传此台」(单台)+ 头部「续传未完成服务器(N 台)」(一键批量);点击时用 `deploy_resume_status` 现查断点(可能已被清理/被同键新部署覆盖);待续传台组装成普通批量队列(`resumeKey` 分支复用 `deploy_resume_start`),串行执行、收尾口径与单发一致
- **B 停止即时中止**:「停止批量」在当前台部署中时发 `cancel_deploy`(步骤边界生效),中断台进入待续传列表;**不自动续传**(停止是显式意图)
- **D 修断点裁剪泄漏**:断点表超上限裁剪时,被裁条目的本地临时 tar 一并回收(此前只删表项 → tar 永久留在临时盘且无法经「放弃断点」回收);新增单测
- **顺带修复严重存量 bug**:`st.batch.deferred` 存成对象却被当函数调用 → **批量部署第一台结束后永久卡死**(TypeError 被事件监听器吞掉);用 HEAD 版对照复现后改为可调用且带 `.promise` 的把手
- **视觉**:judge 首轮打回徽章跨行错列,修复后复审 pass(徽章列 + 定宽动作槽对齐)
- **顺带修复用户反馈的 UI bug**:回滚中心「已备注」徽章被拉满整行 —— 父容器 `.rollback-release-info` 是 column flex,`align-items` 默认 `stretch`。修法是 `.rollback-release-info > .badge { align-self: flex-start }`,**必须用后代选择器**:`window.fillBadge` 会整体重写 `node.className`,给徽章挂自定义类会被抹掉(首版按自定义类写,实测 `alignSelf: auto`、徽章 419px 仍拉满;改后代选择器后 50px/419px、不再拉伸)。
- 验证:`cargo test` 291 passed(基线 290 + 新单测)/ clippy 与基线逐条比对零新增 / 浏览器桩跑通全部续传路径(含断点已清理的边界)/ judge pass
- **真机复测 ✅(2026-09-16)**:真实服务器上的批量失败 → 续传此台/一键续传;停止批量的即时中止手感

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
- 验证:`cargo test` 302 passed(基线 292 + 10)/ clippy 零新增 / 真机冒烟进程存活;**tooltip 悬停效果已确认 ✅(2026-09-16)**(系统绘制,无自动化断言手段)

### 第十七批 v5.16.0(候选池:远程管理四件套 + 探活)— 已完成
- **image_filter 消费**(wiki/07 限制 3 解除):部署页选中项目 → 镜像下拉按「镜像过滤关键字」筛选(`repository:tag` 子串,不区分大小写);提示条区分 过滤生效/无匹配/无可用镜像;项目切换即时重填
- **.env 非 UTF-8 无损往返**(限制 17 解除):`StackEnv` 增 `notUtf8` + `rawB64`;保存分两路 —— 未改动 → `rawB64` 原始字节回写(零损坏),改动 → 拒绝并提示在服务器上编辑(替换符落盘会永久损坏);UTF-8 文件路径不变
- **Docker data-root 探测**:概览采样命令增 `==DROOT` 段(`docker info` 取 Docker Root Dir),Docker 盘按探测路径匹配 df 行(改过 data-root 的部署此前恒误判为与根分区同盘);段缺失回退 /var/lib/docker
- **容器级指标**:manage_stats 每轮附 `aggregate`{count + Top3 CPU + Top3 内存}(纯函数聚合 + 单测);监控页顶部聚合条展示「N 个容器 · CPU 前列 · 内存前列」,停止/失败轮隐藏
- **服务器定时探活 + 通知**:新模块 probe.rs(TCP 连 host:port 5s 超时,不做 SSH 认证);设置中心新增「探活间隔(分钟,0=关)」保存即启停任务;**状态翻转才通知**(在线→离线 / 离线→恢复),通知事件类型增 `probe`(订阅开关 onProbe,默认关);首轮建基线不通知
- 验证:`cargo test` 313 passed(基线 308 + probe 2 + aggregate 2 + data-root 1)/ 三 verify 全 PASS

### 第十六批 v5.15.0(候选池:结构化错误码)— 已完成
- 新模块 `src-tauri/src/errors.rs`:错误串头部 `[dderr:<code>]` 前缀旁路(US 控制字符开头,UI 不渲染),**零命令签名改动**;12 个错误类(canceled/transport/auth/perm_denied/timeout/network/protocol/config/parse/fs/input/internal),加类 = 枚举加变体两行,删类 = 编译器穷尽检查兜底;7 单测
- 后端:取消链路(mod.rs/deploy.rs/rollback.rs/migrate)生产→`errors::cancelled()`、判定→`code_of`;ssh.rs 全域挂码(认证/传输/超时/Fs/Protocol);`with_timeout` 挂 timeout;`exec_json_list` 挂 protocol/transport;stats 传输判定 `is_transport_error` 改按码;`DeployDone`/`StatsPayload` 增 `errorCode` 字段(camelCase 可选)
- 前端:`parseErrCode`/`errStripCode`(app.js);`errText` 自动剥码(68 处调用零改动);deploy.js 取消判定 5 处改码优先+文案回退;历史徽章/批量循环/监控错误展示兼容新旧格式
- 文案匹配存留区:仅远端工具原样输出解析(docker permission denied / pull 401),见 wiki/04
- 验证:`cargo test` 308 passed(基线 302 + errors 6,含 is_transport_error 测试改造);三 verify 全 PASS;行为断言(码解析/剥码/防误读)

### 第十五批补丁 v5.14.1(修复 05 远程管理页监听全断)— 已完成
- 根因:第十二批拆分把 manage.js 的 IIFE 局部助手 `$` 留在宿主,manage-stacks.js 40+ 处裸 `$(...)` 在 DOMContentLoaded 即抛 `ReferenceError` → **栈/监控/终端/日志跟随按钮监听全断**(点击无反应、invoke 不发出、后端日志零记录),潜伏 v5.11.0 → v5.14.0 三个版本;「容器/镜像/卷/网络还能用」的边界恰好等于拆分边界
- 修复(4 行):manage-stacks.js 补 `$` 定义;**新守护 `verify/scope-integrity.js`**(全链加载 + 触发 DOM 回调,专抓初始化回调里的自由标识符断裂,TDD 先红后绿 + 行为级验证点击发出 `manage_stats_start`)
- 排查中排除 v5.14.0 托盘挂钩(`set_tooltip` 同步等主线程,常驻时毫秒级返回,非根因)
- 验证:三 verify 脚本全 PASS / `cargo test` 302 passed 与基线一致(纯前端修复)

### 第十八批 v6.0.0(安全强化与并发互斥)— 已完成(commit 63b2064)
- **驱动**:八路并行审查(wiki 01/02/03/04/06/07 对齐 + Rust + JS + 安全契约),先修文档与低风险项,再落地用户拍板的全部待决策项
- **russh 0.46→0.60.3**(ring 后端,修 RUSTSEC-2026-0153/0154 两个 HIGH):API 适配 PublicKey 路径/PrivateKeyWithHashAlg/AuthResult/Handler 原生 impl Future(去 async_trait);测试密钥改 Ed25519Keypair::from_seed 确定性构造
- **严格 CSP 启用**(csp:null → default-src 'self' 等):内联防闪白主题脚本外置 theme-init.js(加入 verify CHAIN)
- **get_config 密文最小化**:密文哨兵 "*" 只读视图;新增 `save_server_entry` merge 命令(命令 94→95,+单测);servers.js 编辑保存改走新命令
- **open_external cmd 注入修复**(explorer 直开 + 元字符校验);cleanup/sync_files/rollback 路径校验(+cleanup 单测)
- **并发互斥**:06 页回滚接入部署互斥(共享锁 window.ddRemoteOp,双向);批量间隙锁族(tab/setMode/回滚钮/模态回滚/续传入口入 batchActive);关清理模态不误清 pruning;监控先订阅后 invoke + 订阅失败停后端
- **功能修复 14 处**:版本说明从未写成功(deploy.js writeReleaseNotes 缺 req 包裹)、check 误报、整栈批量空指针、config-io id、rollback deploy-done 过滤+剥码、setBtnBusy 兜底、stats 码判定、migrate stop 吞错/tmp 残留、parse_du_output 空格、save_gzip 兜底、escHtml 引号等
- **wiki 全量对齐**(01/02/03/04/06/07/README,30+ 处):补 errors/tray_status/probe 三模块章节;修正栈扫描深度/批量断点/续传复用条件/契约结构字段;各文件行数与命令计数
- 验证:`cargo test` 315 passed(基线 313 + 2)/ clippy 零新增 / release build 通过 / 三 verify 全 PASS
- **真机确认 ✅(2026-09-16)**:russh 0.60 连接回归(密码/私钥/口令+TOFU)、CSP 渲染、save_server_entry 编辑保存密文保留、托盘 tooltip

### 第十九批 v6.1.0(候选池清空 + 限制修复)— 已完成
- **部署前自动预览**(候选池唯一剩余项,用户定案「勾选才自动」):整栈选项区新增「部署前自动预览」勾选(`#deploy-auto-preview`,localStorage `dd_deploy_auto_preview` 记忆,默认关);勾选后点「开始部署」在环境检测通过后**先自动跑一次变更预览**(`previewStackOnce` Promise 版,与「部署预览」按钮同 `preview_stack_changes`),展示预览表并经 `confirm()` 确认后才真正部署,取消则预览结果保留;不勾则不自动预览。**「部署预览」手动按钮保留不变**;勾选入部署锁族(refreshControls 禁用清单)
- **限制 4 BusyBox df**:磁盘预检命令 `df -PBG` → `df -k`(1K 块,GNU/BusyBox 通用的最宽口径);`parse_df_gb` 改按 1K 块换算 GB(兼容旧 G 后缀);单测更新
- **限制 6 compose env_file**:新增服务级 `env_file` 支持(字符串/数组/长语法 `- path:`,多项依序合并后者覆盖前者,整体覆盖默认 `.env`;缺失容错),`load_env_path` 抽出单文件解析 + `collect_service_env` 服务级合并;3 单测
- **限制 12 hover 反白 badge 不可见**:行 hover 反白(墨底纸字)时徽章 4 变体(info/fail/ok/warn)全部豁免——info/fail 反回纸底墨字、ok/warn 固定回自身配色;全站排查确认行内无其他带背景元素会被吞;浏览器实测 4 变体 hover 对比度 199-236
- **限制 14 终端 ANSI 复杂 TUI:保留为限制**(完整 xterm 仿真需引入 xterm.js 级依赖并重写渲染模型,与低耦合硬约束冲突)
- 验证:`cargo test` 318 passed(基线 315 + env_file 3)/ clippy 我改文件零警告 / 三 verify 全 PASS / 浏览器桩 badge hover 对比度验证

### 第十九批补丁 v6.1.1(修复密文哨兵落盘)— 已完成
- **用户真机报错**「密码密文 base64 解码失败:Invalid symbol 42」:ASCII 42 = `*`,即 v6.0.0 密文最小化哨兵被**写进磁盘**冲掉 4 台服务器全部真实密文
- **根因(v6.0.0 回归)**:get_config 返回哨兵只读视图后,4 条「get_config 全量取 → save_config_cmd 全量写回」路径(删服务器/删项目/保存项目/导入 compose/保存分类)不经过 save_server_entry,把哨兵原样写回磁盘;v6.0.0 只改了服务器编辑保存一条路径,漏封整量写回通道
- **修复(后端不变量)**:`CIPHER_SENTINEL` + `restore_sentinel`;save_config_cmd/save_server_entry 均按 id 与磁盘现值合并,哨兵一律还原现值(读不到降级 None,绝不写 `*`);resolve_password/key_passphrase 对哨兵提前给可操作报错;2 单测(还原四态 + 拒绝)
- **现场处置**:用户 servers.json 7 处哨兵已清理为 null(备份 .corrupted-bak);真实 DPAPI 密文不可从 `*` 恢复,需逐台重录登录密码
- 验证:`cargo test` 320 passed(基线 318 + 2)/ clippy 新代码零警告

### 第十九批补丁2 v6.1.2(05 页加载提示 + hover 补漏)— 已完成
- **05 页「选择服务器后加载」反直觉**(用户反馈):进入页/切服务器/切 tab 三处表格占位改为「加载中…」(已自动选中、正在 SSH 取数);确实未选时改「请先在上方选择服务器」
- **hover 反白补漏**:上轮只豁免 badge 4 变体,本轮全站扫描补 `.port-badge` / `.badge-running` / `.badge-paused` 固定自身配色,`.stat-warm/hot` 新增 scheme 翻转 token(亮暗主题 hover 行底色翻转后对比均足够,实测 152-199)
- 验证:对比度计算全过 / node --check / verify 三脚本 PASS / cargo test 320 不变

### 第十九批补丁3 v6.1.3(真机反馈 + 全量体验审查)— 已完成
- **RSA 私钥修复**(用户真机 tencent.pem):russh 0.60 features 补 "rsa"(v6.0.0 升级漏项);同源隐患 PrivateKeyWithHashAlg None → SHA-1 被现代 OpenSSH 拒 → 改 best_supported_rsa_hash 探测
- **四路审查**(密文链路/russh 彻底性/CSP/体验):密文 P1-P4 全修(notify 脱敏/拒绝保存/导出预检/邮件预检);russh 无新增故障(顺带修 MSRV 1.85 + OpenSSH 错口令映射);CSP 0 问题
- **体验审查 5 高 3 中全修**:badge-exited/created hover 豁免、window.confirm 改内联确认、06 页扫描反馈、`'badge ok'` 类名错、清理并发防护、下拉加载占位、重检/操作按钮 busy 等
- **随批并入 2026-09-13 审查修复批第一轮**(P0/P1-1/P1-2/P2-1/P2-2/P2-3 + P2-6① 后端挂码,细节见 `2026-09-13-待修复bug.md`):批量续传互斥置位 / 探活回填 / Esc 逐层关模态(isTopModal 全局仲裁,11 处)/ 单镜像建连超时 / manage_logs 顶替自清理+stop 竞态 / DD_CONFIG_DIR debug 守卫
- 验证:325 passed(+3 单测)/ clippy 零新增 / 三 verify PASS

### 第二十批修复批收尾 v6.1.4(2026-09-13 审查修复批第二轮)— 已完成
- **P2-4 save_config 族互斥**(本批最大改动):config.rs 新增 `update_config<T,F>`(CONFIG_LOCK 锁内 load→闭包改→save,Err 不落盘)+ RESUME_LOCK(断点表独立);**11 个生产写点全部迁移**(save_server_entry / save_config_cmd(哨兵还原并入锁段,消除「还原读到现值→落盘前被改」窗口)/ notify_save_config / remember_key_passphrase / persist_host_key_if_needed / retrust_host_key / import_compose / bind+update_project_from_source / bind_project_to_target / save+remove_checkpoint);DPAPI 加密、文件复制在锁外;+3 单测(8 线程×5 并发零丢写 / Err 不落盘 / 哨兵拒绝)
- **P2-5 哨兵纵深防御**:`encrypt_password` 拒收 `"*"`(挂 input 码,文案指向重输/留空沿用);+1 单测
- **P2-6② errorCode 字段消费**:app.js 新增 `window.errCodeOf(payload)`(字段优先,回退 parse message——落地 wiki/04:644 契约);deploy.js 三处 deploy-done 消费点接线(①后端挂码已随 v6.1.3)
- **P2-7 杂项**:package.json version 1.0.0→6.1.4 对齐(「三处版本号」外第 4 处,发版链不消费、纯一致);rollback.js 两处裸 `__TAURI__.event.listen` → `AppBus.on`(全站唯一绕过例外消除,失败复位守卫保留)
- 版本:v6.1.3(5917753)之上顺延 v6.1.4;两轮逐 hunk 对比零冲突
- 验证:328 passed(基线 325 + 3)/ clippy 零新增 / node --check 16 JS / 三 verify PASS

---

## 待完成

> **候选池(2026-09-17 入库)已清空**:16 项功能候选分四批(第二十三~二十六批)全部落地,
> 详情见下方各「已完成」节与 UPGRADE-PLAN。**当前待办池 = 「候选池(2026-09-18 入库)」
> (A 组清尾 / B 组新功能 / C 组交互中风险)**,含已裁决不做项存档。

### 细节补正池 ✅ 全部完成(第二十三批(一),2026-09-17;版本号随批次收尾由 S2 统一 bump)

> 7 项全量落地(记录见 UPGRADE-PLAN「第二十三批(一)」):占位符对比度/间距字号/
> help 文案/wiki07 四处/servers.js 注释/死常量/import 残留清理;测试 363→364,
> clippy 零新增,桩验证 judge PASS。

| 项 | 要点 | 位置 |
|---|---|---|
| 终端输入框占位符对比度 | `.manage-terminal-input` / `.manage-env-editor` 漏配 `::placeholder`(吃 UA 默认 #757575,实测 4.1:1 低于 AA)→ 补 `var(--ink-on-paper-55)`(实测 5.8:1,零新色值) | `ui/style.css:2499/3319` |
| 终端模态间距/字号 | 「＋新标签」gap 5px vs「Shell:」6px 不一致 → 统一 8px;Shell 标签 13px → 12px(与工具栏一致) | `ui/style.css:4253/2886/2889` |
| help.js 终端文案 | 「默认 bash,可切换 sh」→「默认自动(推荐),后端探测 bash/sh 并回显」(与实现对齐) | `ui/help.js:273` |
| wiki/07 四处 | 限制 35 过期(已实现「未生效」toast)、正文测试数 360→363、#63 编号重复改 #63b、限制尾部乱序整理 | `wiki/07` |
| servers.js 注释漂移 | `:36/:369` 仍写 prune_server 旧流程 → 改 openCleanupModal 实际 | `ui/servers.js` |
| manage.rs 死常量 | 删 `PERM_DENIED_MSG`(注释自陈无引用,文案已收编 errors::perm_denied) | `src-tauri/src/manage.rs:33` |
| import_compose 残留 | 失败路径清理 `stacks/<uuid>/`(含副本文件),成功/失败分支统一;补单测 | `src-tauri/src/commands/compose_sources.rs` |

### 功能候选池(16 项)✅ 全部完成(第二十三~二十六批)

| 项 | 要点 | 规模 |
|---|---|---|
| ~~终端日志保留(时间制)~~ ✅ 已完成 | 第二十三批(二),S2 会话(见下「已完成」区) | 小 |
| ~~05 页各 Tab 搜索筛选~~ ✅ 已完成 | 第二十四批(三),S1 会话(见 UPGRADE-PLAN) | 小-中 |
| ~~容器批量操作~~ ✅ 已完成 | 第二十四批(三),S1 会话:pause/unpause/rename + 常显勾选列 + 批量条串行执行 + 改名模态(见 UPGRADE-PLAN) | 中 |
| ~~资源阈值告警~~ ✅ 已完成 | 第二十四批(一),S1 会话:独立采样任务(SSH 复用 host_metrics 链)+ 连续 2 轮防抖 + `alert` 通知类型;设置中心四字段 + 通知中心勾选(记录见 UPGRADE-PLAN) | 中 |
| ~~栈 compose 查看/编辑~~ ✅ 已完成 | 第二十五批④:`manage_stack_compose_read/save` 两命令(备份 .ddbak.<ts> 保留 3 份)+ 栈行「compose」按钮 + 编辑/确认/保存三段式 | 中 |
| ~~部署失败自动回滚~~ ✅ 已完成 | 第二十五批⑤:设置项 `autoRollbackOnFailure`(默认关)+ 仅整栈/仅健康检查失败/非续传/非取消;复用 `rollback_execute_stack_inner`(部署 guard 内不可二次 acquire) | 大 |
| ~~多机巡检汇总~~ ✅ 已完成 | 第二十四批(S2),纯前端串行复用 server_diagnose + 汇总视图(见下「已完成」区) | 小 |
| ~~架构预检~~ ✅ 已完成 | 第二十五批②:`arch_precheck.rs` 词表归一 + 两条部署路径接入;**只告警不阻断**(多架构镜像与 qemu 会误伤) | 中 |
| ~~SSH config/known_hosts 导入~~ ✅ 已完成 | 第二十五批①:`ssh_import.rs` 只读扫描 + 勾选批量走 `save_server_entry`;**不预填指纹**(多算法密钥预填致硬失败) | 小 |
| ~~本地镜像清理(02 页)~~ ✅ 已完成 | 第二十五批③:`list_dangling_images`/`remove_local_images`(逐 ID rmi,校验完整 sha256)+ 02 页模态两步确认 | 中 |
| ~~迁移断点续传~~ ✅ 已完成 | 第二十六批⑤:阶段级断点(卷/镜像/compose/启动/收尾),复用同一张断点表 + 键前缀与 mode 隔离;停机阶段只在整体成功后落盘 | 大 |
| ~~卷内容浏览/单卷备份~~ ✅ 已完成 | 第二十六批④:`manage_volume_browse`(逐层 tar tzvf,只列一层)+ `manage_volume_backup`(tar.gz 下载到本机);复用第六批临时容器 tar 通道 | 大 |
| ~~network/internal 错误码挂点~~ ✅ 已完成 | 第二十六批①:reqwest 传输错误挂 `[dderr:network]`、本地文件/进程类挂 `[dderr:internal]`;前端按码分流引导 | 小 |
| ~~扫描识别放宽~~ ✅ 已完成 | 第二十六批②:`compose_scan.rs`;设置可配文件名与深度,名字经严格校验(拼远端 find 命令的安全边界) | 小-中 |
| ~~模板驱动批量~~ ✅ 已完成 | 第二十六批③:批量模态内「套用模板」行,选项级套用(传输选项 + 版本标题说明),项目/服务器保持当前选择 | 中 |

#### 终端日志保留(时间制)规格(S2 按此实施)

- **设置项**:`AppSettings.term_log_keep_days: u32`(settings.json,camelCase `termLogKeepDays`;**默认 30,0 = 永久保留**;越界夹取 0–3650)。无新命令。
- **清理时机**:① 终端会话创建时(manage_exec.rs,写新日志前)② 应用启动时(lib.rs setup 一行)。best-effort,失败仅 `log::warn!`,绝不阻断。
- **清理规则**:只处理 `logs/term-<yyyyMMdd-HHMMSS>-*.log` 严格文件名(不碰 app.log 及其他文件);时间戳取自文件名(非 mtime);早于 `now - N 天` 即删;文件名不可解析 → 跳过保留;keep=0 → 整段跳过;目录不存在 → 返回。
- **实现**:纯函数筛待删列表 + 单测(过期/未过期/非 term 跳过/keep=0/坏文件名跳过);调用点删除 + 告警。
- **前端**:设置中心「通用」区「日志文件」行附近数字输入(`settings-term-keep-days`,hint:0 = 永久保留;超过保留天数的终端日志在打开终端/启动软件时自动清理)。
- **文档**:wiki/02(manage_exec 落盘段补保留策略)、wiki/03(设置中心条目)。

### 候选池(2026-09-18 入库,第二十七批预备)

> **来源**:用户在「新功能征集」批复「已知限制清尾 + 纯产品向新功能」两个方向;
> 全部条目已做落点侦察(坐标与规模判断见各项),不是凭空设想。
> **选取方式**:用户批复节点按需挑选;建议小步快跑(A 组)或 A+B1 组合。

#### A 组|已知限制清尾(低风险,可直接收口)

| # | 项 | 落点与要点 | 规模 | 侦察依据(2026-09-18) |
|---|---|---|---|---|
| ~~A1~~ ✅ 已完成(第二十七批) | 回滚中心容器计数**精确归属** | `rollback.rs:813-821` 现按「容器名包含目录名」近似 → 改用同一条命令已返回的 `com.docker.compose.project.working_dir` 标签精确匹配(与 `cleanup.rs:670-685` 的 `compose_working_dirs` 同口径) | 小 | 精确数据**已在同一份 `ps_items` 里**,只是计数没用它;需仿写对象形态标签解析(NDJSON 对象 ≠ manage 的字符串形态,不能直接套 `compose_project_of_labels`) |
| ~~A2~~ ✅ 已完成(第二十七批) | 清理分析**归档名放宽** | `cleanup.rs:374` 的 `-name '20*-*'` 是**全仓唯一**模式定义;放宽后自定义命名归档(如 `prod-2026-09-18`)可被列出 | 小-中 | 唯一硬解析点 `project_dir_of_release`(`cleanup.rs:687`)按 `/` 切分,与目录名内容无关 → 兼容。**真正要决策的是排序口径**:字典序不再等于时间序,需退回按 mtime(`ls -t`)或混合排序。`is_valid_release_dir_target` 不依赖该模式,删除安全性不受影响 |
| ~~A3~~ ✅ 已完成(第二十七批) | **tar 镜像可配置** | `migrate_project.rs:1342-1373` 的 `pick_tar_image` 现只在内置候选(busybox/alpine/ubuntu)里挑;加设置字段允许自填(服务器有私有 registry 时用) | 小 | 改动面 = 配置字段 + 候选取值 + 表单;校验复用既有「镜像引用」形态 |
| ~~A4~~ ✅ 已完成(第二十七批) | 归档搬运**注释与实现对齐** | `migrate_project.rs:1556-1559` 注释说「子目录跳过并计入警告」,实现实为「`sftp_download` 目录失败 → 该归档中断 + 降级 warning」 | 微小 | **只改注释**(不做递归支持;理由见「已裁决不做」表的「归档子目录支持」行) |

#### B 组|产品向新功能

| # | 项 | 落点与要点 | 规模 | 侦察依据(2026-09-18) |
|---|---|---|---|---|
| ~~B1~~ ✅ 已完成(第二十八批) | **多项目编排**(一次部署 N 个项目,按序) | **前端队列**,复用批量部署成熟形态(`deploy.js` 的 `st.batch`:逐台预检 → 复用单发 → deferred 收尾 → 停止/续传);项目多选替代服务器多选 | 中 | `ProjectConfig` 无项目间关联字段;后端 `StackDeployRequest` 是单项目的(`deploy.rs:1466`)。前端队列先例已被第十四批验证(含逐台续传/停止/报告导出),**后端零改动**是最大优势。若要求后端持久化编排/依赖图 → 规模变大,不推荐 |
| ~~B2~~ ✅ 已完成(第二十八批) | **部署日报/汇总** | 按天聚合 `DeployRecord`(`ts` 前 10 字符即日期)成一条摘要,经通知管道发一次 | 中 | 通知现为逐事件即时发送(五种 kind 固定,`notify.rs:539-549`);**无聚合结构**,需新 kind(白名单外会被 `log::warn` 跳过)或新发送函数。主要工程点是**触发源**(复用调度 tick? 手动?)与 kind 语义 |
| ~~B3~~ ✅ 已完成(第二十七批) | **服务器分组/标签** | **实施口径**:`ServerConfig.tags: Vec<String>`(多标签,归一 cap 8/24 字);归属 = **首标签**使分节与下拉语义一致;五处下拉(部署/回滚中心/定时/迁移×2)+ 批量勾选列表分节 + 03 页分节/徽章 + 表单 chips 编辑器;不做全局标签管理 | 小-中 | `fillSelect`(`deploy.js:472-502`)是纯 option 追加,**无 optgroup 支持需扩展**;`config_io.rs` 是显式字段镜像,导入/导出需同步;服务器 >10 台时收益明显 |
| ~~B4~~ ✅ 已完成(第二十八批) | **部署窗口/维护窗** | 调度日程加「允许执行时段」;`is_due`(`deploy_schedule.rs:149-164`)外再判定 | 小-中 | 现只有「到点触发」(90s 窗口)。**主要设计成本是错过语义**:「窗口内没到点」vs「到点但不在窗口」的处置需明确(参见既有 `missed_disposition` 172-198 的口径) |

#### C 组|交互中风险(动共享接口,单列)

| # | 项 | 落点与要点 | 规模 | 侦察依据(2026-09-18) |
|---|---|---|---|---|
| ~~C1~~ ✅ 已完成(第二十八批) | **传输中可取消** | 给 `sftp_upload`(`ssh.rs:313-320`)/`save_gzip_remote`(`migrate.rs:389-453`)加块级取消检查,让大镜像/大卷传输中能即时中断 | 中 | 现状:取消只在镜像/卷边界生效(`migrate.rs` 检查点 268/322/347)。**可插点已有**(`copy_file_to_remote` 的 64KB 块循环 `ssh.rs:668-682`、保存流的 `channel.wait()` 循环),但**签名变更波及 14 个调用点**(deploy 6 / migrate_project 5 / migrate 1 / rollback 1 / ssh 测试 2)→ 属接口级扩展,机械改动多但语义简单 |

#### 已裁决不做(存档,避免重提;附理由)

| 项 | 裁决理由 |
|---|---|
| 归档**子目录**支持(限制 50) | 当前行为是「该归档搬运中断 + warning」而非丢数据;单层结构是本应用自己写入的(manifest/compose 副本/镜像包全平铺),子目录只在用户手工塞入时出现。支持需动递归列举 + 中转命名 + 逐项警告三处,**成本 > 收益**;只修注释(A4) |
| 迁移**改名**(限制 52) | 与回滚归属校验直接冲突(`rollback.rs:418-425` 的 `manifest.project != project.name` 即中止);改名须重写归档 manifest 且旧回滚入口兼容双名。**除非改名成为明确产品需求** |
| 迁移后**自动停源**(限制 63) | wiki 明示「双源双跑验证」是刻意保留的设计(用户需验证目标可用再下线源);自动停源把不可逆动作引入后置步骤。**工程小、产品上不该默认做** |
| **本地构建后直接部署** | 面积远超其它候选(新事件流 + 构建日志 UI + 构建取消域 + 上下文路径字段),且现有「镜像来自本地已有」的假设链(智能传输/日期 tag/架构预检)全部建立在此之上。目标用户已有 CI/本地 docker 工作流 |
| 多项目编排的**后端状态机**版本 | 与前端队列重复,且与全局互斥位(`acquire_remote_op`)+ 断点体系纠缠;批量部署已证明前端形态可行(第十四批)。**要做就复用前端队列**(B1) |

#### 质量发现(不在候选内,留档备查)

- **`run_migrate_project` 零管线级测试是现实上限**:该函数直接 `connect_server`(`migrate_project.rs:894-903`)无 trait 抽象,拆纯函数会把测试变成「另一个实现」。可测纯函数已拆 12 个且有单测;**更大覆盖只能靠真机 `#[ignore]` 测试**(已有 volume roundtrip 样板,`ssh.rs:1219`),或先做一次 IO 抽象重构(工程债另计,规模大)
- **`scope-integrity.js` 的守护止于初始化路径**:实测(2026-09-18)注入三种故障——DCL 回调内裸引用**被抓**、`AppBus.on` 监听回调内**放过**、`addEventListener('click')` 处理器内**放过**。静态扫描确认**当前无真实隐患**(各文件助手声明完整);唯一空白是「未来在事件/定时器回调里新增裸引用」这一形态,历史上无此形态事故。如需补守护,低成本路线是加词法级「未声明自由标识符」静态检查(新增守护,非修 bug)

### 候选池(2026-09-22 入库,第三十一批预备)

> **来源**:用户要求「盘点未修项 + 针对实现流程上网调研」(2026-09-22);全部条目已做落点侦察,
> P1/P2 另有**本机 Docker 实测**证据。**选取方式**:按需挑选;建议先做 P1+P2 小批
> (up 命令参数级,直击「回滚回干净 / 不静默」,与第三十批「按 ID 收敛」同主题)。
>
> **状态(2026-09-22)**:P1 / P2 / S1 / S2 已随**第三十一批(v6.13.0)**完成(S1 复核后
> 为文档更正:代码早已修复);S4 / S5 已随**第三十四批(一)(v6.15.0)**完成;P3 已随
> **第三十四批(二)(v6.16.0)**完成;L2 / S3 已随**第三十四批(三)(v6.16.0)**完成;P4 经
> 用户裁决**跳过留档**(待密钥管理拍板后另行开工);**余 L1(层级增量传输评估)**。

#### P 组|部署/回滚流程加固(有新证据)

| # | 项 | 落点与要点 | 规模 | 侦察依据(2026-09-22) |
|---|---|---|---|---|
| ~~P1~~ ✅ 已完成(第三十一批 v6.13.0) | 孤儿容器:**`--remove-orphans`** | 部署/回滚/栈停止全链未用该参数 → 回滚到旧归档(或部署时删掉某服务)后,**新版才有的服务容器作为孤儿继续运行**,界面报「完成」——与第三十批「按 ID 收敛」同族的「回滚没回干净」。建议回滚两条链 `up` 必加;部署链一并加(行为变更写 wiki/help:会清掉同项目内不在当前 compose 的容器) | 小 | **本机 Docker 实测**:v1(a+b)→ v2(只 a)再 up,不加参数 → 仅一行 orphan warning、b 容器仍在;加参数 → b 被移除。出处:distr.sh 生产清单「Pass `--remove-orphans` on every `compose up`/`down`」 |
| ~~P2~~ ✅ 已完成(第三十一批 v6.13.0) | **`--pull never`**(防意外静默拉取) | `up -d` 默认 pull=missing:引用在本地不存在时会**静默从 registry 拉一个非归档版本**(.env 漂移/外部移标签可触发)。本应用拉取类服务已在步骤 5 显式 pull、本地传输服务由 load 保证 → 加 `--pull never` 只把「意外缺失」从静默拉取变成显式报错 | 小 | ~~compose v2 全支持该参数~~ **修正(第三十一批实测)**:`--pull` 旗标自 compose **v2.15** 起提供,更老版本会报 unknown flag;需回归「拉取类服务」路径。与本项目「不静默」哲学一致 |
| ~~P3~~ ✅ 已完成(第三十四批(二) v6.16.0) | up 前**服务端权威解析校验**(`docker compose config`) | 已落地:`config --format json` 逐服务比对(**部署阻断 / 回滚只告警**,含语法预检;+1 次 SSH);纯函数三件套 + golden 单测;同批 D(部署侧 up 后不一致数并入结果文案) | 小-中 | simplified.guide 回滚指南(「`config --images` 在 recreate 前抓住错误 tag」);本地 `image_refs_with_env` 重算属近似,服务端 config 为权威 |
| ⏸ P4(跳过留档) | **更新包签名校验**(安全) | `update_download` → `update_install` 现仅路径白名单、**无签名验证**:Release 资产被替换即静默装篡改版。①轻量:release.yml 出 `.sig`(`tauri signer`)+ 内嵌公钥本地验签后再安装;②完整:切官方 `tauri-plugin-updater`(签名强制) | 中 | 需用户参与密钥管理(私钥进 CI secret、公钥内嵌);Tauri 官方文档:签名 cannot be disabled。**2026-09-22 用户裁决跳过留档,未实现** |

#### S 组|已记录限制清尾(小修)

| # | 项 | 落点与要点 | 规模 | 侦察依据 |
|---|---|---|---|---|
| ~~S1~~ ✅ 已完成(第三十一批 v6.13.0) | `import_compose` 失败残留 `config/stacks/<uuid>/` 目录 | **复核结论:代码早在第二十三批已修**(闭包式失败清理 + 单测);本批只更正 wiki/07 的过期记载 | 极小 | wiki/07 限制 7 |
| ~~S2~~ ✅ 已完成(第三十一批 v6.13.0) | 健康检查对 **exit-0 一次性初始化服务**误报未就绪 | 用户拍板**自动通过**:`exited && ExitCode==0` → `Pass{completed}` 并逐条记日志(不选逐服务忽略清单,代价见 wiki/07 决策 16) | 小 | wiki/07 限制 11 |
| ~~S3~~ ✅ 已完成(第三十四批(三) v6.16.0) | `run_migrate_project` 零管线级测试 | 加 `#[ignore]` 真机样板(先例:volume roundtrip `ssh.rs:1219`) → 已落地 `test_migrate_project_target_chain_real`(三件套 SFTP 往返 + 目标侧权威解析 + 目标 up 自证;补卷 roundtrip 未覆盖段) | 小-中 | 质量发现(2026-09-18) |
| ~~S4~~ ✅ 已完成(第三十四批(一) v6.15.0) | `scope-integrity.js` 对「回调内新增裸露引用」不覆盖 | 词法级「未声明自由标识符」静态检查 → 已落地 `verify/static-integrity.js`(含 DOM id 三查;首跑抓出 2 处真 bug) | 小 | 质量发现(2026-09-18) |
| ~~S5~~ ✅ 已完成(第三十四批(一) v6.15.0) | wiki/06、07 的 TOCTOU 例子提到 **watchtower(2025 已归档)** | 措辞换成 CI / 运维脚本 / 另一台机器的本应用 | 微小 | 2026-09-22 调研 |

#### L 组|评估项(先量数据再定规模)

| # | 项 | 落点与要点 | 规模 | 侦察依据(2026-09-22) |
|---|---|---|---|---|
| L1 | **层级增量传输**(只传服务器缺失的层) | 现状「镜像变化即整包传」(500MB 镜像改几 MB 也全量)。网上成熟做法:`docker pussh` / `unregistry`(HN 讨论里差值常达 90%+)。**先量**:典型镜像大小 + `docker history` 层复用率,再决定是否做 | 大 | 工程 = 解析 save tar + 远端层清单 + 流式重排 |
| ~~L2~~ ✅ 已完成(第三十四批(三) v6.16.0) | **服务器体检增强**(可回收空间 / 容器日志) | `server_env_check` 增 `docker system df` 摘要(磁盘紧张时提示「清理分析可回收 X GB」)+ 容器日志体积 —— 已落地(可选字段;02 页展示 + 部署预检磁盘紧张提示) | 小-中 | distr.sh:生产头号事故 = 磁盘被容器日志/悬空镜像撑满 |

**结论备查(确认性,非待办)**:compose 的 `up` 会对比容器标签 `com.docker.compose.image`
(创建时的镜像 ID)与当前解析出的镜像 ID,不同即重建 —— 即 Docker Compose 本就检测镜像变化
(podman-compose issue #1453 以其为对照行为)。因此第三十批的「收敛 retag → up」语义正确,
**无需 `--force-recreate`**。

### 候选池历史(存档:2026-09-13 入库 → 2026-09-16 全部完成)

| 梯队 | 项 | 落地批次 |
|---|---|---|
| 前置 | 部署前自动预览(勾选式) | 第十九批 v6.1.0 |
| 一 | 部署历史筛选/搜索、配置导入预览、服务器一键诊断、部署模板/预设、通知耗时阈值 | 第二十批 v6.2.0 |
| 二 | 批量部署报告导出+重跑失败台、回滚两版本对比、启动静默检查更新、更新失败回执、托盘闭环 | 第二十一批 v6.3.0 |
| 三 | 定时/延迟部署、终端多标签+同栈广播+输出落盘、配置版本历史 | 第二十二批 v6.4.0 |
| 修复 | P0 批量续传互斥 / P1 探活回显·Esc 逐层·跨机 SMTP / P2 七项(建连超时/日志流竞态/互斥收口/哨兵等) | v6.1.3 + v6.1.4 |
| 治理 | 文档不符清扫、doc-consistency、页首版本戳、契约 smoke、编排层补测试 | 第二十二批 v6.4.0(测试数/命令数/页首戳四类断言上线;contract-smoke 入 CI) |

### 遗留待真机验证 — 全部确认 ✅(存档:2026-09-16)

> 2026-09-16 用户实机操作,下列全部条目确认通过,清单清空;新项随后续批次另行入库。

**第二十二批(v6.4.0/v6.4.1):**
1. ✅ **定时部署到点触发**:创建 daily 日程 → 到点看后端自动发起(托盘 tooltip/历史/通知/USB 链路);应用未运行时错过 → 下次打开看「已错过,未补跑」记录
2. ✅ **终端多标签与广播**(v6.4.1 修复「只能开一个标签」后):弹窗顶栏「＋新标签」下拉选不同容器开多个标签并发会话;同栈多容器勾「广播同栈」发命令看双端执行;`logs/term-*.log` 落盘文件实测
3. ✅ **配置版本历史**:编辑服务器/保存项目后 `config/.history/` 出现快照;「恢复」后数据回退且恢复前自动留底

**早前批次:**
4. ✅ 05 概览磁盘数字与服务器 `df -h` 对照(后端 df 解析只过了单测)
5. ✅ 各表单失焦校验手感 + Enter 提交是否符合直觉(服务器/项目/通知/迁移/回滚/清理)
6. ✅ 批量部署的失败续传(「续传此台」/「续传未完成服务器」)与「停止批量」步骤边界即时中止
7. ✅ 部署前自动预览手感(勾选后整栈部署先预览再确认)
8. ✅ 迁移项目两机目录名不同时,目标补标签日志出现且目标 up 成功(v6.3.3 修复)

---

## 
### 第二十批 v6.2.0(候选池第一梯队五项)— 已完成
- **部署历史筛选/搜索**:04 页折叠面板展开态新增模式/结果/关键字三轴筛选条(纯前端,已加载记录过滤);计数行区分「筛选后 N / 共 M」;无匹配时给独立空态
- **配置导入预览**:抽取共用 `parse_export_file`,新增 `config_import_preview` 命令(不落盘,只读解密+摘要);前端两段式——预览显示「备份 N 台 / 当前 M 台(将全部替换)/ SMTP 跨机提示」+ 复用的 `confirmBlock` 三段式确认区;若 SMTP 跨机,风险行特别提示需在通知中心重录(顺带根治 P1-3)
- **服务器一键诊断**:新模块 `server_diagnose` 命令(TCP 5s → SSH 15s + 错误码分类 auth/timeout → docker `--version` 退出码 + 权限降级);逐层短路:前置失败则后续 skipped;前端红绿灯模态(通过/失败/跳过 三种徽章,失败时显示「复制全部结果」按钮);服务器卡片新增「一键诊断」入口(贴「测试连接」前)
- **部署模板/预设**:新模块 `profiles.rs`(`DeployProfile` camelCase,`config/deploy-profiles.json` 独立存储,`MAX_PROFILES=20` 超上限裁旧,`update_config`/`save_checkpoint` 外的读改写收口);三命令 `deploy_profiles_list/save/delete`(id 由前端生成,`# deploy-profiles-list/save/delete` 注册);04 部署页顶部新增「模板」条(下拉 + 套用/存为模板/删除三按钮);**套用只填表单不自动开跑**(保留用户确认权,避免误触发);**存为模板**用 row 内展开输入框(零新模态,不用系统对话框);删除两步确认(2s 超时还原,armDeleteConfirm 同款交互)
- **通知耗时阈值**:`NotifyConfig.min_duration_secs`(0=恒通知,上限夹 3600);`fire_with_duration` 替 `fire`(可选耗时),成功且耗时 < 阈值时跳过通知(夜间批量短平快成功不轰炸),失败/取消/探活恒通知;deploy/rollback 收尾传 `record.duration_secs`;前端通知模态新增「成功通知最小耗时(秒)」字段(填表 + 采集都按 camelCase)
- 验证:`cargo test` 332 passed(+4:并表 cap+并发/err 闭包零落盘/哨兵拒绝/import preview 摘要+不落盘;+3:profiles save_roundtrip+cap 裁剪/notify 阈值纯函数);clippy 零新增;node --check 17 个 JS;verify 三脚本 PASS;**5 项功能均经浏览器桩(本机 http.server + Tauri-stub)逐项验证**(历史筛选三轴、模板套用回填表单六字段、诊断模态红绿灯、导入预览两段式确认区、通知阈值字段保存载荷 minDurationSecs=300);judge 4 张截图全部 pass
- 命令数:HEAD 基线 96 → 101(config_import_preview / server_diagnose / deploy_profiles_list+save+delete;此前文档记载 95 与实际差 1,本批以 awk 实测为准)

### 第二十一批 v6.3.0(第二梯队五项 + 文档治理)— 已完成
- **批量报告导出 + 重跑失败台**:批量结束面板新增「导出报告」(Markdown 落盘,新命令 `write_text_file` 原子写)与「重跑失败台(N 台)」(从头跑不用断点,与「续传此台」区分)
- **回滚两版本对比**:06 页归档勾选两个 → `#release-diff-modal` 逐服务镜像 diff(四态徽章)+ 版本说明并排;纯前端(数据在 `rollback_project_detail` 已返回),零新命令
- **启动静默检查更新**:`autoCheckUpdate`(缺省开)+ dock 版本号徽点(不弹窗,点击进设置);**更新失败回执**:标记版本不一致明示「已回到旧版」
- **托盘闭环**:菜单加「停止当前部署」(动态启用,置取消位)+「上次部署:…」只读项
- **verify/doc-consistency.js**:版本号/命令数/测试数三断言;上线当天抓到 wiki/README 测试数失准(325→332)并修复;〇清扫 ROADMAP 矛盾行与待修复清单 P1-3 终态
- 验证:332 passed / clippy 零新增 / release build 通过 / verify 四脚本 PASS

### 第二十一批补丁 v6.3.1(诊断凭据修复)— 已完成
- **真机反馈**:加密私钥服务器「测试连接正常、一键诊断报私钥已加密」——`server_diagnose` 的 SSH 层直传 `None, None`,未走 resolve_password/resolve_key_passphrase(全仓唯一漏点)
- **修复**:诊断同款解析已存凭据(密码 + 私钥口令),解析失败给可操作文案;密码认证服务器同场景一并修复
- **守护**:新源码级测试 `test_all_connect_sites_resolve_credentials` 扫描六文件全部 connect 调用必须传解析后凭据(先红后绿验证精确命中 `host_server.rs:139`)
- 验证:333 passed(+1)/ clippy 零新增 / doc-consistency 全 PASS

### 第二十一批补丁2 v6.3.2(版本对比按镜像 ID)— 已完成
- **真机反馈**:两版本对比「明明镜像更新了还显示无变化」——原对比只比 `manifest.tag` 字符串,同名 tag 重新构建后 ID 不同却被判「不变」
- **修复**:`ManifestImage` 增 `id`(部署时采集本地镜像完整 ID 写入 manifest;`#[serde(default)]` 兼容旧归档);前端对比**ID 优先**、任一侧缺 ID 回退按 tag 并注明;详情模态与对比表展示短哈希
- **单测**:build_manifest_images 记录 ID / 旧归档无 id 字段 serde 兼容;行为验证 7 用例(核心:同名 tag 不同 ID → 镜像变化)
- 验证:335 passed(+2)/ clippy 保持基线 / doc-consistency 全 PASS
- 注意:旧归档(本版前部署)无 ID,对比其与新版仍回退按 tag;重部署一次即写入 ID

### 第二十一批补丁3 v6.3.3(项目迁移默认命名兜底)— 已完成
- **真机反馈**:项目迁移预检报「服务「backend」未声明 image,其镜像不参与搬运(需在目标服务器构建)」(offical/houtai 同)。用户判断准确:①部署管线对「build 无 image」有默认命名兜底(`<项目名>-<服务名>` 扫描候选),迁移侧没有;②本系统镜像一律 `docker save/load` 搬运,目标服务器**没有构建能力**,「需在目标服务器构建」的说法本身不成立
- **根因**:迁移两处解析(`build_plan` 与执行侧)都调 `stack::parse_compose_file(&compose, &[])` 传**空镜像列表**,且解析发生在本地临时目录(父目录名失真)——兜底数据源(镜像列表)与候选来源(目录名)双双缺位,`scan_default_image` 必然 miss → 服务被丢弃 + 误报文案
- **修复(与部署管线同口径)**:
  - `stack.rs`:`StackService` 增 `fallback_filled`(`#[serde(skip)]`,区分「声明」与「兜底推导」,契约结构不变);新增 `parse_compose_file_with_dirs` / `compose_project_name_candidates_with_dirs`(候选目录名覆盖:迁移注入 origin.json 原目录名 + 源部署目录末段名,替代失真的临时目录推导;空列表行为与旧入口等价,有等价性单测);新增 `target_default_image_ref`(按目标项目名/目录名推导 compose 期望的镜像名)
  - `migrate_project.rs`:预检与执行两处都改为「先查源服务器镜像列表(`query_remote_images_full`,一次往返,复用存在性校验)→ 注入解析」;新增纯函数 `collect_transfer_images`(命中→搬运+识别说明;未命中→可执行修正指引,删除「需在目标服务器构建」表述)与 `migration_dir_name_candidates`
  - **目标侧补标签**(迁移正确性闭环):兜底条目在目标 load/同 ID 跳过后,若目标 compose 期望名(目标目录名推导)与源镜像名不同,执行 `docker tag` 零拷贝补标签并 emit 日志——否则目标 `compose up` 找不到镜像会转去构建(目标无构建上下文,必然失败);补打失败仅告警(up 时二次暴露)+ 指引手动命令
- **单测 +10**:兜底命中入列带标记 / 显式声明无提示 / 未命中给指引且旧文案消失 / 目录名候选优先级(origin.json 前、部署目录后)/ 手工项目仅取部署目录名 / with_dirs 覆盖命中 / 空列表等价旧入口 / 红绿对照(空列表必须 miss=修复前误报路径)/ target 命名推导(目录名合规化、顶层 name 优先)/ fallback_filled 两态
- 前端:迁移空态文案同步(`ui/deploy-migrate.js` 镜像表空态说明兜底口径)
- 验证:345 passed(+10)/ clippy 保持基线 12 / node --check 全部 JS / verify 三脚本 PASS / doc-consistency 全 PASS
- **真机复测 ✅(2026-09-16)**:真实项目(backend/offical/houtai 同构)迁移预检应识别到镜像并列入搬运清单;两机目录名不同时目标补标签日志与目标 up 成功

---

### 第二十二批(进行中)— 第三梯队三项 + 文档治理批

**① 定时/延迟部署 ✅ 已完成**(记录见 UPGRADE-PLAN「第二十二批(一)」)
- 前置:后端远程操作互斥收口(`REMOTE_OP_IN_FLIGHT` + RAII guard,接入部署双入口/回滚三入口/迁移两入口)
- 新模块 `deploy_schedule.rs`:30s tick、触发窗口 90s、错过不补跑(once 跨天停用)、3 命令
- 前端 `deploy-schedule.js`(13 号模态)+ deploy.js 外部部署采纳;jadge 两视图 PASS
- 命令 101 → **104**;测试 345 → **352**

**② 终端增强 ✅ 已完成**(记录见 UPGRADE-PLAN「第二十二批(二)」)
- 后端:会话输出自动落盘 logs/term-*.log;ContainerRow 增 compose_project
- 前端:终端多标签(tabs 字典 + 按 sid 路由的单监听)+ 同栈广播勾选
- judge 抓出双注册缺陷(同步守卫缺失)已修并复验 PASS;测试 352 → **355**
- **补丁 v6.4.1(真机反馈修复)**:「只能开一个标签」——多标签状态机本身正确,
  但加标签的唯一入口(表格行「终端」按钮)被全屏模态遮罩盖死,模态内无任何
  新建入口;修复 = 模态顶栏增「＋新标签」下拉(运行中容器,选中即开/切,同容器
  去重切回),`ui/help.js` 旧「单会话」文案同步改多标签口径;桩端到端 judge PASS
**③ 配置版本历史 ✅ 已完成**(记录见 UPGRADE-PLAN「第二十二批(三)」)
- save_config 写前自动快照三件套到 config/.history/(全等去重、cap 20);
  config_import_file 导入前同样快照;config_wipe 不涉(清除语义)
- 新命令 config_history_list / config_history_restore(id 校验防穿越 +
  恢复前自动留当前态);config-io.js 新增「历史快照」区块(两步确认)
- 测试 355 → **360**;命令 104 → **106**
**④ 文档治理批 ✅ 已完成**
- 文档不符清扫:russh 0.46→0.60.3 等实证漂移全清;命令数/测试数/行数引用对齐实测
- 七篇 wiki 页首统一版本戳 + doc-consistency.js 增「页首戳」断言(7 项)
- verify/contract-smoke.js:前后端命令/事件集合双向核对(含动态调用点补充表;
  负向测试验证有效);ci.yml 增 contract-smoke + doc-consistency 两步
- 编排层补测试 +3(compose_file_flags 转义/compose_override_names/ssh 路径助手)
- README 补「密码与密文说明」章节;AGENTS.md 补互斥收口与版本历史两条硬约束
- 测试 360 → **363**

---

### 第二十三批(2026-09-17)— 双会话并行开发首对(S1 细节补正 / S2 终端日志保留)

> 协议见 AGENTS.md「双会话并行开发协议」;每会话一个 worktree + 独立分支,
> 文件主权互不重叠,**合入串行**:S1 先合入(da03d2e,测试 363→364),
> S2 后合入并执行批次收尾(三处版本号 bump 至 v6.5.0 / wiki/README /
> 七篇页首戳 / doc-consistency --write --tests=N)。

**① S1 细节补正 7 项 ✅ 已完成**(记录见 UPGRADE-PLAN「第二十三批(一)」)
- 视觉 3 项(占位符对比度 AA / 终端模态 gap 8px 与字号 12px / help 文案)、
  文档 4 项(wiki/07 四处)、工程 2 项(死常量 / import_compose 残留清理+单测)
- 测试 363 → **364**;桩验证 8799 judge PASS

**② S2 终端日志保留(时间制)✅ 已完成**(记录见 UPGRADE-PLAN「第二十三批(二)」)
- `AppSettings.term_log_keep_days`(camelCase `termLogKeepDays`;默认 30,
  0 = 永久;夹取 0–3650);零新命令
- 清理时机 = 开终端时(manage_exec)+ 应用启动时(lib.rs setup 一行);
  `select_expired_term_logs` 纯函数按**文件名时间戳**筛,坏名/非 term 跳过,
  best-effort 失败仅 warn
- 设置中心「通用」区新增数字输入 + hint;前端同口径夹取(负值会整单拒绝)
- 测试 364 → **369**(+6);桩验证 5 组载荷断言 + 亮暗双主题 11 项计算样式
  比对,judge 3 图 PASS
- **批次收尾(本批为后完成方)**:S1+S2 合并基线 **370 passed**;三处版本号
  bump **v6.5.0** + wiki/README + 七篇页首戳 + doc-consistency 采集

---

### 第二十四批(2026-09-17)— 双会话并行开发(第二/三批)

> 协议见 AGENTS.md;第二轮 S1 资源阈值告警 / S2 多机巡检汇总,第三轮 S1
> 05 页筛选 + 容器批量操作。**S2 为先完成方**(f244592 合入代码 + 记录),
> **S1 为后完成方**:rebase 合入 + 批次收尾(版本号 bump / 页首戳 /
> VERSION.txt)统一执行。

**① 资源阈值告警 ✅ 已完成**(S1,commit 1e7ab64)
- 独立采样 + 连续 2 轮防抖 + alert 通知(磁盘/内存/CPU 超阈值 → 通知管道)

**② 多机巡检汇总 ✅ 已完成**(S2,commit f244592,记录见 UPGRADE-PLAN「第二十四批(S2)」)
- 03 页区段头新增「全部巡检」按钮(JS 注入,不动冻结的 index.html);
  **纯前端串行**复用现有 `server_diagnose` 逐台跑,**零新命令**、零契约改动
- 汇总模态:占位行预铺 + 每台完成**就地增量渲染**(防重复行)+ 顶部实时计数
  (通过/失败/跳过/剩余)+ 底部「停止巡检」与「复制全部结果」;明细复用既有
  `.diagnose-*` 三列语言
- 中止语义:停止或关模态即在下一台边界停下,未跑到的台落「未巡检」
  (独立 state,不与诊断层 skipped 混算);入口按钮即时复位
- 高度策略:台多时 `.fleet-box` 封顶 68vh 只让台列表内滚,停止/复制按钮常驻
  可见(实测 5 台即撑破 720px 视口,属必修)
- 验证:370 passed 基线不变 / clippy 零新增 / verify 四脚本 PASS /
  桩验证五态 mock(含防重入·增量·停止·复制文本·完成态自洽断言)/
  **judge 5 图 PASS**(补拍亮色完成态下半后复核 PASS)

**③ 05 页筛选 + 容器批量操作 ✅ 已完成**(S1,记录见 UPGRADE-PLAN「第二十四批(三)」)
- 5 Tab 筛选(容器/镜像/卷/网络/栈;不区分大小写子串,只影响渲染,
  state 保留全量);空态区分「暂无数据 / 无匹配」
- 容器勾选列(常显)+ 表头全选 indeterminate + 批量条(启动/停止/重启/
  暂停/恢复/改名/清除选择);串行逐台执行 + 结果汇总;改名模态(单选,
  预填旧名);后端 `manage_container_action` 扩 pause/unpause/rename
  (+`validate_container_name` 校验与单测;命令数不变)
- 验证:**377 passed**(+1)/ clippy 零新增 / verify 六脚本 PASS /
  桩验证筛选-空态-批量-改名-四Tab筛选端到端全通,**judge PASS**

---

### 第二十五批(2026-09-17)— 五项功能批(单会话)

> 候选池清尾:按「剩余 10 项完成前 5 项」批复交付。真机验证由用户统一处理.

**① SSH config/known_hosts 导入 ✅**(记录见 UPGRADE-PLAN「第二十五批①」)
- 新模块 `ssh_import.rs`(1 命令)+ 14 单测;只读扫描,导入走既有 `save_server_entry`
- 前端:页头「从 SSH 配置导入」→ 勾选表(已知主机/私钥/待补录徽章)→ 串行导入

**② 架构预检 ✅**(「第二十五批②」)
- 新模块 `arch_precheck.rs`(纯函数)+ 5 单测 + 变异验证;两条部署路径接入
- Docker 口径 ↔ uname 口径词表归一;**只告警不阻断**

**③ 本地镜像清理 ✅**(「第二十五批③」)
- `docker.rs` 悬空判定 + 4 单测 + 变异验证;2 新命令(`list_dangling_images`/`remove_local_images`)
- 02 页模态:勾选 + 两步确认 + 帧内失败明细;逐 ID rmi,校验完整 sha256

**④ 栈 compose 查看/编辑 ✅**(「第二十五批④」)
- `manage_stacks.rs` 两命令(与 .env 同构 + **保存前 .ddbak 备份保留 3 份**)+ 6 单测
- 栈行「compose」按钮 + 编辑/确认/保存三段式

**⑤ 部署失败自动回滚 ✅**(「第二十五批⑤」)
- 新模块 `auto_rollback.rs` + 7 单测 + 变异验证;设置项 `autoRollbackOnFailure` 默认关
- 仅整栈 / 仅健康检查失败 / 非续传 / 非取消;复用 `_inner` 避二次 acquire

**验证**:`cargo test` 377 → **413**(+36)/ clippy 20 条**零新增** /
verify 六脚本 PASS / 桩验证四项 UI + 载荷断言 / judge 5 图 PASS(1 张 fail→修复后复核 PASS)

**命令数**:106 → **111**(+5:`ssh_config_scan` / `list_dangling_images` /
`remove_local_images` / `manage_stack_compose_read` / `manage_stack_compose_save`)

---

### 第二十六批(2026-09-18)— 池内后五项(单会话)

> **候选池清空**:至此 16 项功能候选全部完成(第二十三~二十六批)。

**① network/internal 错误码挂点 ✅** — 纯函数拆内核 + 五分支单测;前端按码分流
**② 扫描识别放宽 ✅** — 新模块 `compose_scan.rs`(9 单测 + 两轮变异);两处扫描共口径
**③ 模板驱动批量 ✅** — 批量模态内选项级套用模板 + 跨模式提示;顺补模态标题
**④ 卷内容浏览/单卷备份 ✅** — 2 命令 + 6 单测;实测修复对比度缺陷与 scope 陷阱
**⑤ 迁移断点续传 ✅** — 阶段级断点 + 两处守卫(status 过滤 / start 拒收放回)

**验证**:`cargo test` 413 → **432**(+19)/ clippy 20 条零新增 /
verify 六脚本 PASS / 桩验证 ②③④ + judge 2 图 PASS

**命令数**:111 → **113**(+2:`manage_volume_browse` / `manage_volume_backup`)

---

### 第二十七批(2026-09-19;与第二十八批合并发版 v6.9.0)— A 组清尾四项 + B3 服务器标签(单会话)

> **来源**:候选池(2026-09-18 入库)用户批复「A 组 + B3」。B3 经 brainstorming 澄清
> 三项口径(多标签 / 全部下拉 / 下拉+新建),追问三项(首标签归属 / 批量列表也分节 /
> 不做全局标签管理)。

**A1 回滚容器计数精确归属 ✅** — 新纯函数 `running_container_count` 按
`com.docker.compose.project.working_dir` 精确匹配(回滚中心整段近似过滤删除);
反例用同前缀项目 `/opt/app` vs `/opt/app-staging`;**两轮变异均被抓**
**A2 归档名放宽 + mtime 排序全链 ✅** — 去掉 `-name '20*-*'`;
**排序口径是真正决策点**:凡「取最近 N 个」的路径一律不得按名字排序 →
四处收口(清理分析 / 回滚列表 / 回滚明细截断 / 迁移取 N 个)+ 共享助手
`ls_subdirs_mtime_cmd`/`parse_release_lines`;口径来源 = 部署侧收尾裁剪本就是 mtime;
**两轮变异均被抓**(去尾斜杠归一 / 名字倒序塞回)
**A3 tar 镜像可配置 ✅** — `AppSettings.tarImage` + `normalize_tar_image` 严格校验
(拼远端命令前置校验纪律);**变异验证暴露首版测试太弱**(漏 `..`/空 digest 类)→
补 9 用例后连真实现的漏网项 `busybox:@tag` 一并抓到
**A4 归档搬运注释对齐 ✅** — 仅注释(递归支持已裁决不做)
**B3 服务器标签 ✅** — `ServerConfig.tags`(归一 cap 8 条/24 字符,收口
`save_server_entry`)+ config_io 同步(旧导出文件导入为空标签)+ 四个共享助手
(`serverTagsOf`/`serverPrimaryTag`/`groupServersByTag`/`serverOptionsFor`+
`appendGroupedOptions`)+ 五处下拉 optgroup + 03 页分节/徽章 + 表单标签编辑器;
**桩自检抓到两个真实缺陷**(同标签重复 optgroup / 批量勾选行粘连),均已修并补回归断言

**验证**:`cargo test` 432 → **444**(+12)/ clippy 20 条零新增(按新增行逐行核对)/
node --check / verify 六脚本 PASS(form-validation 67 项,含 B3 新增 13 项 + 变异被抓)/
桩验证 03 页分节·标签编辑器·部署下拉结构·批量分节 + judge 4 图 PASS(2 张采集问题重拍)

**命令数**:113 不变(无新命令)

---

### 第二十八批(2026-09-19;与第二十七批合并发版 v6.9.0)— 候选池余项四项(单会话)

> **来源**:候选池(2026-09-18 入库)用户批复「继续完成剩余的所有事项」。
> 实现顺序按风险从低到高:B4 → B2 → B1 → C1。

**B4 允许执行时段 ✅** — `DeploySchedule.windowStart/windowEnd`(半开区间、支持跨 0 点);
**核心语义 = 到点不在时段内则当天跳过且不算错过**(`missed_disposition` 显式分流,
否则 daily 每天误标「已错过」、once 被误停用);两轮变异均被抓;7 测试
**B2 部署日报 ✅** — 新模块 `digest.rs`;独立常驻任务(仿 probe 先例)+
`AppSettings.digestHour`(缺省关)+ 通知 kind `digest`(默认关订阅);
**发送标记放独立文件**(放 settings 会被表单整量覆盖 → 当天重发);
取消单独计数不算失败;两轮变异均被抓;7 测试
**B1 多项目编排 ✅** — 批量模态内维度切换(项目多选 → 各自 `default_server_id`);
队列机制零复制(停止/续传/重跑/导出全沿用 `st.batch`);
**复合键 `batchItemKey(serverId, projectId)` 是本项真正的正确性改动**(四处只比 serverId
会让同服务器多项目互相吞续传入口);整栈模式明确不支持多项目(全局 `st.stack` 硬依赖);
`verify/form-validation.js` 第 11 节 5 项断言
**C1 传输中可取消 ✅** — **接口设计改为客户端级取消探针**(ROADMAP 原案「改签名波及 14 处」
实测为 17+2 处,且两侧取消位形态不同)→ `SshClient.with_cancel_probe` + 三处块循环检查,
**19 处调用点零签名改动**;判定抽纯函数 `transfer_cancel_check`(可单测);
变异(挂了探针就取消)被抓;3 测试;残留:卷 tar 打包阶段仍以命令为粒度

**验证**:`cargo test` 444 → **461**(+17)/ clippy 20 条零新增 / node --check /
verify 五脚本 PASS(form-validation 72 项)/ 桩验证 4 视图 + judge 4 图 PASS

**命令数**:113 不变(无新命令)

**候选池状态**:A/B/C 三组 9 项**全部完成**(v6.9.0 五项:A1-A4+B3;本批四项:B4/B2/B1/C1)

---

### 第二十九批(2026-09-19)— 回滚安全与体验八项(单会话)

> **来源**:用户发起的「用户体验讨论」——先做回滚实操可行性审查(发现会误导用户的静默失效),
> 再逐项定策。用户裁决:按 manifest 回滚(不强制留档)+ 预检阻断确认 + 只归档 override +
> 表单内预检按钮 + 确认区缩小 + 设置加开机自启 + 一批全做。

**R1 按 manifest 回滚 + 可用性预检 ✅**(核心)—— 审查发现:智能传输跳过的服务 `file=null`,
装载只按目录内实际 tar.gz,**缺包服务被静默跳过、`up -d` 用当前镜像成功、界面报「回滚完成」**。
修复:`plan_rollback` 纯函数逐服务三态(归档有包 / 服务器按 manifest 的 ID 命中 → **直接可用,
不必强制留档** / 回不去);新命令 `rollback_precheck` + 表单内「回滚预检」按钮;
阻断项逐条列出并要求确认(`allowPartial=true` 才继续);**自动回滚恒 false**(无人值守不部分回滚);
新错误码 `rollback_precheck`。两轮变异(前缀归一 / archived 优先级)均被抓;9 测试
**R2 override 纳入归档与回滚 ✅** —— 部署时归档 `<名>.ddbak`,回滚按原名恢复(两条路径:
04 页按本地文件枚举、06 页按远端 `.ddbak` 枚举);`.env` 刻意不入(用户裁决,见 wiki/07);
**测试抓到真 bug**:首版规则会把 `docker-compose.yml.ddbak`(base 本体)当 override 恢复
**R3 06 页回滚可取消 ✅** —— 根因:04 页取消按钮判 `st.deploying`、托盘停止判 `deploy.running`,
06 页发起时两者都不成立 → 只能硬等 `docker load`。修复:06 页置/复位该标志(经 DeployKit);
取消实现单一化(`DeployKit.cancelDeploy`)
**R4 失败回滚留痕 ✅** —— 此前只有成功落历史,失败的**从界面上消失**(刷新即无法复盘);
现失败也写 `mode="rollback"` 记录(带原因);命令层组装骨架(管线内失败点不一,不宜逐处携带)
**S1 开机自启 ✅** —— **零依赖**(不加 crate,新依赖拉取不可靠):写/删 HKCU Run 注册表项;
**状态真值 = 注册表**(用户可能在任务管理器禁用),`app_settings_get` 用实况覆盖配置字段;
值必须引号包裹(含空格路径会被截断);3 测试
**L1 部署页常驻按钮 + 分区 ✅** —— 执行条 `position: sticky; bottom: 0`(整栈加服务分类表后
按钮原本落到折叠线以下);顶线分隔配置区与执行区
**R1d 回滚确认区压缩 ✅** —— 用户反馈「别挤到下方日志框」:紧凑内边距/行距
**D1 文案漂移守护 ✅** —— 新脚本 `verify/user-facing-copy.js`:把**历史翻案口径**固化成断言
(如「批量不落断点」「断点无法续传」「单镜像不支持回滚」不得复现)+ 关键能力可检索 +
「两处配置」耦合提示 + R3/R1 的前后端契约对齐;两轮变异均被抓(其中一轮是我变异不彻底,
反而验证了守护的严格性)

**验证**:`cargo test` 461 → **474**(+13)/ clippy 20 条零新增 / node --check /
verify **六**脚本 PASS(新增 user-facing-copy)/ 桩验证 3 视图 + 截图自检 PASS
(judge 子代理因 provider 故障不可用,按协议回退自行检查)

**命令数**:113 → **114**(+1:`rollback_precheck`)

---

## 当前状态速览

- 版本 **v6.16.0**;main = origin/main;基线 `cargo test` **531 passed** / 14 ignored
- 命令 **122** 个(generate_handler 实测;第三十三批 +8 文件管理 114→122;口径与 wiki/04 一致)
- **第三十四批(三)/ v6.16.0(2026-09-22,与(二)同一未发版版本)**:服务器体检增强(L2)+ 迁移「目标链」真机样板(S3) —— ①`server_env_check` 增 `docker system df` 可回收空间摘要(镜像 / 容器 / 卷 / 构建缓存四类,十进制单位换算)与容器日志体积(`docker ps -aq → inspect LogPath → du -bc`,非 root 读不到则缺省);两者为**可选字段**(`#[serde(skip_serializing_if)]`,探测失败不进 errors、不参与判定);02 页服务器卡新增两行展示(「可回收空间 ≈ X」/「容器日志占用 ≈ Y」),部署页预检在**磁盘紧张时**提示「服务器可回收空间 ≈ X(可用 03 页「清理优化」定向清理)」;②L2 纯函数 3 个(`parse_docker_size` / `parse_docker_df` / `parse_first_u64`,golden 来自本机 `docker system df` 实测抓样)+ 3 单测;③S3 真机 `#[ignore]` 样板 `test_migrate_project_target_chain_real`:三件套经 SFTP 往返(归档字节级一致)→ 目标侧复用 P3 的 `compose config --format json` 权威解析 → `up` 并自证容器在跑(补上迁移链里卷 roundtrip 未覆盖的段落);④help(03 页检测行说明)与 wiki/04(契约可选字段)、wiki/06(闸门表)同步。测试 528→**531**,真机样板 14→**15**,命令 122 不变
- **第三十四批(二)/ v6.16.0(2026-09-22)**:up 前**服务端权威解析校验**(P3)+ 口径统一(D) —— ①up 前跑 `docker compose config --format json`(+1 次 SSH,`-f` 链与 up 同源),服务端权威解析的 `services.*.image` 与记录逐服务比对(两侧同口径规范化:补 `:latest`、剥 `docker.io` 前缀;**构建类无 image 只提示**);**部署侧不一致即阻断**(附逐服务明细与常见原因:服务器 shell 环境变量 / .env 漂移 / compose 副本不一致),compose 语法错误提前暴露;**回滚侧只告警**(插值漂移预检已是确认制,再阻断会与刚做出的确认冲突;实际结果由 up 后校验兜底);老版 compose 报 unknown flag → 告警跳过;②D:up 后校验不一致数并入部署 `record.message`(「部署完成;up 后校验:N 个服务…不一致(详见日志)」),与回滚链同口径;③纯函数三件套 + golden 单测(本机 compose v5.4.0 实测抓样:插值 / 注册表 / 无 tag / build 无 image / 合并流 warning / 语法错误 exit=1),**变异自证两轮均被抓**;④help FAQ 与 wiki/02·06·07 同步(决策 87)。测试 519→**528**,命令 122 不变
- **第三十四批(一)/ v6.15.0(2026-09-22;已发版)**:静态守护 + 文档清尾 —— ①新 `verify/static-integrity.js`(词法级**未声明自由标识符**,补 scope-integrity 的运行时盲区;DOM id 三查 = 静态唯一 / 引用可解析 / 动态重名评审制),10 条负向夹具随脚本常跑,已入 CI 与本地链;②**首跑抓出两处真 bug 并修复**:`ui/deploy.js` 批量续传登记 `serverId: sid`(第二十八批 B1 复合键重构漏删声明的回归,失败/取消台登记抛 ReferenceError → 队列停摆且续传入口不登记)、`ui/app.js` 设置入口取 `settings-btn`(实际 `settings-entry-btn`,dock 版本徽点「点击打开设置中心」静默失效);③顺带:管理页 .env 确认按钮 id 命名空间化(`env-confirm-*`,消除跨文件重名)+ `scope-integrity.js` 头注释漂移修正;④S5:wiki/06、07 TOCTOU 例子的 watchtower(已归档)改「CI / 运维脚本」;⑤P4 经用户裁决**跳过留档**(见 UPGRADE-PLAN 第三十四批(一)「裁决记录」)。测试 519 不变、命令 122 不变
- **第三十一批 / v6.13.0(2026-09-22)已完成**:部署/回滚流程加固 —— ①**P1 孤儿容器**:部署/回滚/迁移/栈启停的 `up`(与栈 `down`)统一带 `--remove-orphans`(`compose_up_cmd` / `compose_up_cmd_in_dir` 唯二拼装来源;`manage_stack_action` 走纯函数 `compose_action_sub`),同项目内不在当前 compose 的容器一并移除 —— 治「回滚到旧归档 / 删掉服务后孤儿继续跑、界面报完成」;②**P2 禁止隐式拉取**:部署/回滚/迁移的 `up` 带 `--pull never`,镜像缺失显式报错(05 页栈启动刻意不加);③**S2 健康检查 exited-0 口径**(用户拍板自动通过):`Pass{completed}` + 逐条日志,不再让一次性初始化服务误报失败(此前叠加自动回滚会回滚好部署);④**S1 复核**:代码早已修复,wiki/07 过期记载更正;⑤**本机 Docker 实测**:孤儿 up/down 移除、`--pull never` 显式报错、拉取类回归、真实 `ps --all` exited-0 形态;测试 501→505,命令 114 不变
- **第三十三批 / v6.14.0(2026-09-22)已完成**:远程管理新增**文件管理** —— ①三源(容器 rootfs / 数据卷 / 部署目录**只读**)+ 一个模态(列表 / 编辑器 / 快照 / 分发四视图);②通道:`docker cp`(实测零二进制镜像与已停止容器均可双向传)+ 服务器 `/tmp` 中转守卫清理,数据卷经临时容器挂载(`tarImage` 设置优先);③文本编辑 ≤512KB 且仅 UTF-8,二进制/超大引导走下载;④**覆盖前默认备份到本机 `config/fm-backups/`(保留 3 份)**,部署目录只读(用户裁决);⑤**容器快照**:实际 env(默认掩码)/ 端口 / 进程 / 挂载 + 与 compose `environment:` **键名**双向对比;⑥**同栈分发**:前端串行复用上传,逐台结果汇总;⑦8 条新命令(命令 114→**122**),测试 510→**519**(含真机 `#[ignore]` 样板);桩验证三视图 + judge 三轮(快照视图:比例字体 → 空格折叠两轮修复后 PASS)
- **第三十二批 / v6.13.0(2026-09-22,审查修复;与第三十一批同版本合并)**:用户要求「讲透 P1 并审查回滚隐藏漏洞」→ 两条回滚链 + 预检逐段审查,列 F1–F7 并全部修复 —— ①**F1/F2**:回滚 compose/override **归档为准**,06 页改显式 `-f` 链(本机实测:默认解析被 `compose.yaml` 遮蔽、且只吃一个 override);②**F3**:`--no-build`(实测缺镜像 + build 段会现场构建非归档版本);③**F4**:wiki/07 补孤儿清理的项目作用域边界(换目录/共用目录/顶层 name);④**F5**:两条链 override 集合统一(含预检漂移取值同源);⑤**F6**:预检新增 `noComposeCopy` 提示 —— 桩验证**顺带抓到真 bug**(两页预检容器同 id → 04 页结果写进 06 页容器、模态空白),已修+守护;⑥**F7**:up 后不一致数写进回滚结果文案(历史/通知不撒谎)。测试 505→510,命令 114 不变;桩验证 + judge 两页 PASS
- **候选池(2026-09-22 入库,第三十一批预备)**:~~P1 孤儿容器~~ / ~~P2 `--pull never`~~ / ~~S1~~ / ~~S2~~ **已随 v6.13.0 完成**;~~S4~~ / ~~S5~~ **已随第三十四批(一) v6.15.0 完成**;~~P3~~ **已随第三十四批(二) v6.16.0 完成**;~~S3~~ / ~~L2~~ **已随第三十四批(三) v6.16.0 完成**;P4 用户裁决**跳过留档**;余 L1(层级增量传输评估,34-6 处理)。详见「待完成 → 候选池(2026-09-22 入库)」
- **补丁 v6.11.1(2026-09-19)**:回滚中心(06 页)补上回滚预检 —— 第二十九批遗漏了该入口(用户实测发现);两个命令扩为双入口(项目 id / 项目目录)
- **补丁 v6.12.1(2026-09-22)**:修复 up 后校验的 `docker inspect` 模板解析错误(真机反馈)—— 带点标签键误用字段链(`.Config.Labels."k"` 非法 Go 模板)致退出码 64,校验每次部署皆失败(只告警不误判);改 `index` 读取 + 失败信息附输出尾部 + 回归守护测试(形态 + 真机 CLI 解析);测试 500→501,命令 114 不变
- **第三十批 / v6.12.0(2026-09-20)已完成**:回滚/部署「按镜像 ID 收敛」——用户真机问题(标签被外部移走、归档 ID 仍在服务器 → 旧预检「回不去」且无出路)驱动;up 前按 ID 收敛(零拷贝指回,治 TOCTOU + 真机案例)/ 预检四态(tagRestore)/ up 后运行镜像校验 / 清单驱动装载(.tar 反向校验)/ .env 插值漂移预检(须确认);测试 477→500,命令 114 不变
- **第二十九批(2026-09-19)已完成**:回滚安全与体验八项(按 manifest 回滚+预检 / override 归档 / 06 页可取消 / 失败留痕 / 开机自启 / 部署页常驻按钮 / 确认区压缩 / 文案守护)——**由 UX 讨论直接驱动**;测试 461→474,命令 113→**114**
- **第二十八批(2026-09-19)已完成**:候选池余项四项(部署窗口 / 部署日报 / 多项目编排 / 传输中可取消)——**候选池就此清空**;测试 444→461,命令 113 不变。**与第二十七批合并发版 v6.9.0**(两批共享同一版本号,未单独打 v6.10.0)
- **第二十七批(2026-09-19)已完成**:A 组清尾四项(回滚容器计数精确归属 / 清理归档名放宽+ mtime 排序全链收口 / tar 镜像可配置 / 归档搬运注释对齐)+ B3 服务器标签(tags + 03 页分节+ 四处下拉 optgroup + 批量勾选分节 + 标签编辑器);测试 432→444,命令 113 不变
- **第二十六批(2026-09-18)已完成**:池内后五项(错误码挂点/扫描放宽/模板批量/卷浏览备份/迁移断点)——**功能候选池就此清空**;测试 413→432,命令 111→113
- **第二十五批(2026-09-17,五项功能批)已完成**:SSH 配置导入 / 架构预检 / 本地镜像清理 / 栈 compose 查看编辑 / 部署失败自动回滚;测试 377→413,命令 106→111
- **第二十三批(2026-09-17,双会话并行首发)已完成**:S1 细节补正 7 项 + S2 终端日志保留(时间制);批次收尾由 S2 统一执行(版本 v6.5.0)
- **第二十四批(进行中,双会话并行第三批)**:S1 资源阈值告警 ✅(「第二十四批(一)」)+ 05页筛选/容器批量 ✅(「第二十四批(三)」)/ S2 多机巡检汇总 ✅ 已合入(f244592);批次收尾由 S1(后完成方)统一执行
- **第二十二批(v6.4.0)第三梯队三项全完成**:定时部署 / 终端多标签+广播+落盘 / 配置版本历史;文档治理批完成(contract-smoke 上线);**补丁 v6.4.1**:终端多标签可达性修复(「＋新标签」下拉);**遗留待真机验证 8 项全部确认 ✅(2026-09-16),清单清空**
- verify/ **七脚本**:form-validation / bridge-integrity / scope-integrity / static-integrity / contract-smoke / user-facing-copy / doc-consistency(VERSION.txt 为测试数缓存,非脚本;static-integrity 已入 CI)
- JS **18** 文件(含 theme-init.js;index.html 加载顺序见 wiki/03:13)
- 前端结构:12 模态;commands/ 11 文件;三大 JS 主文件 2338/2013/2537 行
- 表单体系(第十三批):5 处模态有真实 `<form novalidate>` 语义;失焦校验 + Enter 提交由 app.js 三助手统一承担
- 批量部署(第十四批):失败/取消台可续传(单台 + 一键批量);「停止批量」步骤边界即时中止;批量恒落断点(与死代码 `deploy_batch` 的「不落断点」无关)
- **安全(第十八批 v6.0.0)**:russh 0.60.3(ring+rsa 后端,RUSTSEC 修复);严格 CSP 启用;get_config 密文最小化(只读视图 + save_server_entry merge;v6.1.3 起含 notify);open_external 注入修复;cleanup/sync_files/rollback 路径校验;encrypt_password 拒哨兵(v6.1.4)
- **并发互斥(第十八批 + v6.1.4)**:部署/回滚跨页互斥(共享锁 `window.ddRemoteOp`);批量间隙锁族(续传台置位 v6.1.3 起);配置写互斥 `update_config`(CONFIG_LOCK,11 写点收口;RESUME_LOCK 断点独立,v6.1.4)
- **候选池清空(第十九批 v6.1.0)**:部署前自动预览(勾选才自动);BusyBox df -k 通用口径;compose 服务级 env_file;hover 反白 badge 豁免
- **Esc 逐层关模态(v6.1.3)**:全局仲裁 `window.isTopModal`,全站 11 处监听接入;**errorCode 字段消费(v6.1.4)**:`window.errCodeOf`(字段优先,回退 parse message)
- 本地校验脚本 `verify/`(零依赖 Node,不参与构建):`form-validation.js`(表单助手 54 断言)/ `bridge-integrity.js`(桥接完整性)/ `scope-integrity.js`(全链加载 + DOM 回调作用域完整性,v5.14.1 起);改动表单助手、拆出文件桥接或 JS 拆分时先跑这三个
- dev 实例:target/debug/config(正式版数据拷贝);前端资源编译期内嵌,**改 JS 后必须重编译重启动才生效**
