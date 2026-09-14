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
- 真机待确认:russh 0.60 连接回归(密码/私钥/口令+TOFU)、CSP 渲染、save_server_entry 编辑保存密文保留、托盘 tooltip

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

### 阶段六(第十四批):批量部署增强 ✅ 已完成(见「已完成」节第十四批)

### 候选池(任意批复节点可插入,无先后承诺)
> 2026-09-13 全量审查(五路并行 + 主代理复核)后入库;来源细节见当次审查报告。
> 顺序约定:第一梯队(第二十批,见下)→ 第二梯队 → 第三梯队 → P0/P1 修复批 → 文档/治理批。

| 项 | 要点 |
|---|---|
| ~~部署前强制预览~~ | ✅ 第十九批已实现为「部署前自动预览」勾选(用户自行勾选,不勾不自动);候选池清空 |

**第一梯队(第二十批,本次执行)** — 低成本高价值:
| 项 | 要点 |
|---|---|
| 部署历史筛选/搜索 | 04 页:模式(全部/单镜像/整栈/回滚/迁移)+ 结果(全部/成功/失败)+ 项目名关键字,纯前端过滤,零后端改动 |
| 配置导入预览 | `config_import_preview(path, password)` 解密后返回「覆盖 N 台/新增 M 台/项目 K 个/SMTP 将失效」摘要不落盘;确认后才真导入(顺带封堵跨机 SMTP 二次加密缺陷:导入预览与正式导入都明示) |
| 服务器一键诊断 | `server_diagnose(serverId)` 分层红绿灯:TCP→SSH banner→TOFU→认证→docker 可用;复用 connect/check 分层,终结「密码错/网络错/密钥错」盲猜 |
| 部署模板/预设 | 「服务器+项目+日期标签/智能传输/强制留档+版本说明」固化为命名模板,部署页一键套用;批量模态按模板发起;落 `deploy-profiles.json`(serde default,不碰既有配置) |
| 通知耗时阈值 | notify 配置增 `min_duration_secs`(默认 0=恒通知);成功且耗时 < 阈值不通知,失败/取消恒通知;`DeployRecord.duration_secs` 已在历史,后端 fire 处一个 if |

**第二梯队(待批复)** — 中等成本、闭环体验:
| 项 | 要点 |
|---|---|
| 批量部署报告导出 | 批量结束后导出 Markdown/JSON 报告(每台:结果/耗时/失败原因/断点键/发布目录);另加「整台重跑失败台(不续传)」入口;`finishBatch` 数据现成,关页即失 |
| 回滚两版本对比 | 06 页勾选两个归档 → 并排 diff 服务清单/镜像 tag/版本说明;数据在 `rollback_project_detail` 单次往返内,纯前端 |
| 启动时自动检查更新(可关) | 设置项默认开;启动静默查一次,dock 版本号旁加徽点,不弹窗打扰 |
| 更新失败回执 | `update-pending.json` 比对不一致(安装失败回滚)时 toast 明示「自动更新失败,已回到旧版」,不再静默丢弃 |
| 托盘闭环 | 托盘菜单加「上次部署:<终态 时间>」只读项 + 部署中「停止当前部署」动态项(复用 cancel_deploy + tray_status) |

**第三梯队(待批复)** — 较大工程,后端多已就位:
| 项 | 要点 |
|---|---|
| 定时/延迟部署 | 项目级「每天 HH:MM」或「延迟 N 分钟」;复用 probe.rs tokio interval 模式 + 托盘状态面;到点走单发管线(断点天然支持) |
| 终端多标签+命令广播+输出落盘 | manage_exec 后端本就是多会话表,纯前端放开多 tab;同一条命令广播到多台同栈容器;会话结束写 logs/term-<ts>.log 供审计 |
| 配置版本历史 | `save_config` 写前快照到 `config/.history/`(留 20 份)+ `config_history_list/restore`;部署有回滚中心,配置没有;对竞态覆盖与误导入兜底 |

**修复批 ✅ 已完成(v6.1.3 随批 + v6.1.4 收尾)** — 审查发现的 P0/P1/P2 真实 bug 全部修复:
| 项 | 要点 |
|---|---|
| ~~P0 批量续传脱离互斥~~ | ✅ v6.1.3:resumeKey 分支置位 `st.deploying`/`ddRemoteOp`,invoke 失败同步还原 |
| ~~P1 探活间隔不回显~~ | ✅ v6.1.3:回填去恒假守卫改无条件 |
| ~~P1 Esc 连带关模态~~ | ✅ v6.1.3:`isTopModal` 全局仲裁,11 处监听接入,一次 Esc 只关一层 |
| P1 跨机导入 SMTP 损坏 | 部分完成(v6.1.3 已做 notify 脱敏成对 + 导出预检);跨机明示与 `config_import_preview` 命令待第二十批「配置导入预览」阶段根治 |
| ~~P2 级~~ | ✅ v6.1.3(建连超时 3 处/日志流顶替自清理+stop 竞态/DD_CONFIG_DIR debug 守卫/exec 超时挂码)+ v6.1.4(`update_config` 11 写点收口互斥/encrypt_password 拒哨兵/前端 errCodeOf 消费 errorCode 字段/package.json 版本对齐/rollback.js AppBus.on);细节与逐条证据见 `2026-09-13-待修复bug.md` |

**文档/治理批(待批复)** — 本次审查 90% 文档问题同根(版本戳靠人工同步):
| 项 | 要点 |
|---|---|
| 文档不符清扫 | russh 0.46→0.60.3(wiki/01:9、07:93);wiki/03:73「批量不落断点」残句;测试基线 290/269→320(wiki/02:487、07:229);wiki/02:145 断点复用口径、02:282/04:363 open_external、04:51 prune_server 死命令标注、01:35 94→95;README 补「密文不出后端」与密码重录流程 |
| verify/doc-consistency.js | 零依赖脚本:版本号(tauri.conf/Cargo.toml/ROADMAP/wiki)/ 测试数 / 命令数 不一致即非零退出,入 verify/ 族 |
| wiki 页首版本戳 | 7 篇页首统一「对齐版本:vX.Y.Z(测试基线 N)」;「三处版本号」纪律扩为七处 |
| 契约 smoke 测试 | verify 断言前端 invoke 集合 ↔ lib.rs 注册集合、前端 listen ↔ 后端 emit 集合严格相等(硬约束 6 自动化) |
| 编排层补测试 | commands/deploy.rs 2100 行、rollback.rs 1513 行 0 个 in-file 测试;ssh.rs 安全核心有效测试仅 12 个,逐步补模块内测试 |

### 遗留待真机验证(用户下次实机操作时顺手确认)
1. 版本详情保存版本标题/说明(路径翻倍修复后)
2. 整栈部署填标题/说明 → 回滚中心看归档自动补写
3. 05 概览磁盘数字与服务器 `df -h` 对照(后端 df 解析只过了单测)
4. 「迁移项目…」模态能正常打开(insertBefore 修复后)
5. **单镜像回滚模态能正常打开**(第十三批修复 DeployKit 覆盖 bug 后;此前打开必报错)
6. 各表单失焦校验手感(服务器/项目/通知/迁移/回滚/清理)+ Enter 提交是否符合直觉
7. **批量部署的失败续传**:真实服务器上批量中途失败 → 面板「续传此台」/「续传未完成服务器」是否按断点正确续上
8. **「停止批量」的即时中止**:当前台是否在步骤边界及时停住、中断台是否出现在待续传列表
9. **部署前自动预览**:勾选后整栈部署先自动预览再 confirm 确认的手感(第十九批)
10. **部署时填版本说明能真正写入归档**(第十八批 writeReleaseNotes req 包裹修复,需一次真实整栈部署)

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


---

## 当前状态速览

- 版本 **v6.2.0**;main = origin/main;基线 `cargo test` **332 passed** / 13 ignored
- 命令 **101** 个(lib.rs awk 实测;wiki/04 待同步)
- 命令 **95** 个(lib.rs 注册;wiki/04 已同步);JS **16** 文件(含 theme-init.js;index.html 加载顺序见 wiki/03:13)
- 前端结构:12 模态;commands/ 11 文件;三大 JS 主文件 2338/2013/2537 行
- 表单体系(第十三批):5 处模态有真实 `<form novalidate>` 语义;失焦校验 + Enter 提交由 app.js 三助手统一承担
- 批量部署(第十四批):失败/取消台可续传(单台 + 一键批量);「停止批量」步骤边界即时中止;批量恒落断点(与死代码 `deploy_batch` 的「不落断点」无关)
- **安全(第十八批 v6.0.0)**:russh 0.60.3(ring+rsa 后端,RUSTSEC 修复);严格 CSP 启用;get_config 密文最小化(只读视图 + save_server_entry merge;v6.1.3 起含 notify);open_external 注入修复;cleanup/sync_files/rollback 路径校验;encrypt_password 拒哨兵(v6.1.4)
- **并发互斥(第十八批 + v6.1.4)**:部署/回滚跨页互斥(共享锁 `window.ddRemoteOp`);批量间隙锁族(续传台置位 v6.1.3 起);配置写互斥 `update_config`(CONFIG_LOCK,11 写点收口;RESUME_LOCK 断点独立,v6.1.4)
- **候选池清空(第十九批 v6.1.0)**:部署前自动预览(勾选才自动);BusyBox df -k 通用口径;compose 服务级 env_file;hover 反白 badge 豁免
- **Esc 逐层关模态(v6.1.3)**:全局仲裁 `window.isTopModal`,全站 11 处监听接入;**errorCode 字段消费(v6.1.4)**:`window.errCodeOf`(字段优先,回退 parse message)
- 本地校验脚本 `verify/`(零依赖 Node,不参与构建):`form-validation.js`(表单助手 54 断言)/ `bridge-integrity.js`(桥接完整性)/ `scope-integrity.js`(全链加载 + DOM 回调作用域完整性,v5.14.1 起);改动表单助手、拆出文件桥接或 JS 拆分时先跑这三个
- dev 实例:target/debug/config(正式版数据拷贝);前端资源编译期内嵌,**改 JS 后必须重编译重启动才生效**
