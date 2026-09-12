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

## 待完成

### 阶段五(第十三批,建议 v5.12.0):表单校验体系 — 下一项
- 范围:失焦(blur)校验 + Enter 提交语义;app.js 已有 `formLabel / setFieldError / clearAllFieldErrors / formFailLoud / formErrorBox / confirmBlock` 助手,补统一 blur 校验接线
- 顺序:servers → deploy → notify 高频表单先行,不一次全改(全库当前 0 个 `<form>`,引入 form 语义时注意与现有 div+按钮提交的兼容与 Enter 默认行为)
- **开工前必做**:出细案(哪些表单、校验规则清单、Enter 提交与 confirmBlock 二次确认的交互)交用户批复
- 参考:wiki/07 已知限制 57-59(无输入即校验/失焦校验未接入、无 form 语义)

### 阶段六(第十四批):批量部署增强 — 排队
- 目标:批量落断点/续传入口 + 「停止批量」当前台可取消(现只能停在台边界)
- **开工前必核实**:批量走同一 deploy/deploy_stack 命令,断点可能已由后端落盘——疑点在「前端失败后没给续传入口」而非后端;先读 deploy.js 批量队列失败分支与 deploy_resume_status 查询条件再定方案;出细案批复
- 参考:wiki/07 限制 21/24

### 候选池(任意批复节点可插入,无先后承诺)
| 项 | 要点 |
|---|---|
| 容器级指标 | 概览只有宿主机维度;manage_stats 流扩展容器聚合 |
| 部署前强制预览 | `preview_stack_changes` 已实现未接入部署流程(wiki/03 明示「独立功能」);做成部署前可选/强制预览 |
| 服务器定时探活+通知 | notify 体系已有桌面/SMTP/webhook;定时探活失败推送 |
| 结构化错误码 | 现依赖中文文案子串匹配(is_transport_error 等,wiki/07 限制 13);改结构化错误枚举 |
| 托盘 tooltip 动态态 | 固定文案 → 反映部署/监控状态(wiki/07 限制 19) |
| image_filter 消费 | 配置字段未被消费(项目下拉过滤,wiki/07 限制 3) |
| .env 非 UTF-8 | lossy 替换会乱码(wiki/07 限制 17) |
| Docker data-root 探测 | 概览磁盘按默认 /var/lib/docker 采样;可加 `docker info -f` 探真实数据根 |

### 遗留待真机验证(用户下次实机操作时顺手确认)
1. 版本详情保存版本标题/说明(路径翻倍修复后)
2. 整栈部署填标题/说明 → 回滚中心看归档自动补写
3. 05 概览磁盘数字与服务器 `df -h` 对照(后端 df 解析只过了单测)
4. 「迁移项目…」模态能正常打开(insertBefore 修复后)

---

## 当前状态速览

- 版本 v5.11.0;main = origin/main;基线 `cargo test` 290 passed / 13 ignored
- 命令 94 个(lib.rs 注册;wiki/04 已同步);JS 15 文件(index.html 加载顺序见 wiki/03:13)
- 前端结构:12 模态;commands/ 11 文件;三大 JS 主文件 2338/2013/2537 行
- dev 实例:target/debug/config(正式版数据拷贝);前端资源编译期内嵌,**改 JS 后必须重编译重启动才生效**
