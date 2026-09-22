# DockerDeploy SSH — v4.7 升级计划（智能传输 / 通知 / 安全 / 桌面体验）

> 基于 v4.6.0 的五阶段升级计划,源自 2026-09-05 构想讨论的选定功能集。
>
> 硬约束:**低耦合,禁止对已有功能造成失效**;沿用「新增为主、追加为辅」;每阶段独立可交付、可发版。
>
> 进度:阶段一至五 ✅ 已完成(2026-09-05),完成记录见文末;五阶段功能集已一次性并入 v5.1.0 发布(2e9bd3e 版本提升,详见文末完成记录)

---

## 0. 全局设计原则(沿用并扩展)

| 原则 | 说明 |
|---|---|
| 新增为主 | 新后端逻辑进新文件:`notify.rs` / `update.rs` / `transfer.rs`(跳过判定) / `config_io.rs`(导出导入) |
| 追加为辅 | `commands.rs` 仅追加 pub 助手;`config.rs` 仅追加结构字段(serde default 向后兼容);`ssh.rs` 本计划允许**两处修改**(见风险声明) |
| 前端 | 新页面逻辑进新文件(`notify.js` / `settings.js`);`deploy.js` / `manage.js` 仅做最小接线;新增表单复用现有 modal 体系 |
| 密码类敏感数据 | 一律 DPAPI 加密存储(复用 crypto.rs);导出文件用「导出口令 + AES-256-GCM + Argon2id」可移植加密 |
| 每阶段验证 | cargo build / cargo test(142 基线) / clippy 无新增 warning / node --check / 手动回归(01-05 页 + 新功能) / git diff 低耦合核对 |

**风险声明(ssh.rs 两处既有代码修改,无法以追加实现)**:
1. `ClientHandler::check_server_key`:无条件 `Ok(true)` → 按 per-server 指纹校验(阶段三)。部署主线的连接路径全部经此函数,改动后须回归 04 部署全流程。
2. `SshClient::connect` 私钥加载:`load_secret_key(key_path, None)` → `load_secret_key(key_path, passphrase)`(阶段三,口令从上层透传)。
两处均为最小修改;其余 ssh.rs 代码零改动。

---

## 阶段一:智能传输与一键回滚(核心价值)

### 1.1 镜像变化判定(设计定案)

- **判据 = 镜像 ID(配置摘要)**:`docker image inspect <tag> --format '{{.Id}}'`。镜像是内容寻址的,`docker load` 后服务器上的 `.Id` 与本地严格相等 → ID 相等即内容完全一致,无需时间戳比对、无需维护状态文件。
- **判定时机**:部署预览阶段(dry-run 已有 SSH 连接)。单镜像 1 次 inspect;整栈对全部 Local 类服务合并为一条 `docker image inspect <t1> <t2> ...`(任一不存在退出码非 0,改逐个或解析 stderr 区分)。
- **结果三态**:未变(远端同名 tag 且 ID 相等)→ 跳过 save/upload/load;有变化(远端不存在或 ID 不同)→ 完整传输;远端存在同 ID 但无该 tag(悬空)→ 仅 `docker tag`。
- **UI**:部署预览表新增「变化」列(新增/更新/未变);部署选项加「跳过未变化镜像」开关(默认开)+「强制留档」开关(默认关,见 1.2)。
- **边界**:本地镜像不存在 → 维持现有报错;远端权限拒绝 → 沿用 PERM_DENIED 语义,预览阶段失败则回退全量传输并在预览中标注「无法比对,将全量传输」。

### 1.2 回滚语义(设计定案)

- **留档增强**:部署成功后,在现有 releases 留档目录写入 `manifest.json`:`{ project, time, images: [{ service, tag, image_id, file(相对名,跳过时为 null), skipped }], compose_copy: "docker-compose.yml" }`,并复制当时使用的 compose 文件进留档目录。历史记录(history)记录留档目录相对路径。
- **回滚三级判定**(逐服务):
  1. 留档 tar.gz 存在 → `docker load` + `docker tag` 回记录的 tag;
  2. tar 缺失但 `image_id` 仍在服务器上 → 仅 `docker tag`(跳过未变场景的常态);
  3. 两者皆无 → 该服务标记失败,原因:「该版本镜像未留档且服务器上已不存在(可能被清理或后续部署覆盖)」。
- **回滚 UI 入口**:04 部署页「部署历史」每条记录加「回滚」按钮 → 模态先展示逐服务回滚计划(来源:load 留档 / 仅 retag / 失败) → 二次确认 → 执行;存在失败服务时提供「跳过失败服务继续」复选(默认勾选)。执行 `docker compose up -d`(compose 用留档副本,可选恢复)。
- **实现前核实项(任务 0)**:通读 commands.rs 部署/留档实现与 wiki/06,确认 releases 目录布局、compose 上传路径、history 结构;manifest 写入点放在部署成功收尾处,失败不写。

### 1.3 后端命令(transfer.rs + commands.rs 追加)

| 命令 | 远程执行 / 行为 |
|---|---|
| `inspect_remote_images(server_id, password_plain, tags[])` | 逐/批 `docker image inspect --format '{{.Id}}'`,返回 `[{tag, id, exists}]`;本地侧由前端先经既有 `list_images`(docker.rs)取本地 ID 比对,或新增 `get_local_image_ids(tags)`(本机 docker,不走 SSH) |
| `deploy`(修改点) | 部署参数追加 `skip_unchanged: bool` / `force_archive: bool`;命中跳过的服务走「仅确保 tag」捷径 |
| `rollback_plan(history_id)` | 读服务器 manifest + 逐服务三级判定,返回计划 |
| `rollback_execute(history_id, skip_failed, services[])` | 按计划执行,流式进度复用现有 deploy-log 事件 |

### 1.4 验收要点
- 未变化镜像的第二次部署:无 save/上传动作,总耗时 < 10s(不含 compose up)。
- 回滚三级路径各有用例:load 留档 / 仅 retag / 明确报错;强制留档开启时 tar 必在。
- 回归:全量传输路径(开关关闭)行为与 v4.6.0 逐字节一致。

---

## 阶段二:通知中心(桌面 + 邮件)

### 2.1 配置模型(config.rs 追加,serde default 兼容旧配置)

```rust
notify: {
  desktop: { enabled: bool },                       // 部署成功/失败时系统通知
  email: { enabled: bool, smtp_host, port: u16,
           username, password_enc(DPAPI), security: none|starttls|ssl,
           from, to: Vec<String> },                 // 收件人多个
  events: { on_success: bool=true, on_failure: bool=true, on_cancel: bool=false }
}
```

### 2.2 后端(notify.rs,新依赖 lettre 0.11 + rustls;tauri-plugin-notification)

| 命令 | 行为 |
|---|---|
| `notify_get_config` / `notify_save_config(cfg)` | 读写;邮件密码入参明文 → DPAPI 加密存储,返回时脱敏(仅返回是否已存) |
| `notify_test_desktop` | 发一条本地测试通知(标题/正文固定) |
| `notify_test_email` | 按当前表单值(密码未改则用已存密文解密)发测试邮件;SMTP 全程错误原文以中文上下文返回(连接/认证/发件失败分类提示) |
| 触发点 | commands.rs 部署成功/失败/取消收尾处调用 `notify::fire(app, event)`;异步发送不阻塞部署流程;失败仅记日志 |

### 2.3 前端(遵循「独立配置表单 + 入口简报」原则)

- 04 部署页 page-tools 加「通知」按钮,按钮上带状态徽标(未配置/已配置·桌面/已配置·桌面+邮件)。
- 点击打开独立通知配置模态:桌面通知开关;邮件表单(SMTP 主机/端口/用户名/密码/加密方式/发件人/收件人);事件订阅勾选(成功/失败/取消);底部「测试桌面通知」「发送测试邮件」两按钮,测试结果行内回显。密码字段留空=保持已存值。
- 首页(01)不加入口,通知状态只在 04 部署页入口呈现(用户定案)。

### 2.4 验收要点
- 测试邮件覆盖 none/starttls/ssl 三种加密与「密码错误」场景的错误可读性。
- 部署成功/失败时通知实际到达;邮件发送失败不影响部署结果与 history。

---

## 阶段三:连接与数据安全

### 3.1 加密私钥口令支持(ssh.rs 修改点 2)

- `AuthConfig` 追加 `key_pass_enc: Option<String>`(DPAPI)与表单字段「私钥口令(可留空)」;`connect` 透传口令;解密失败 → 中文报错「私钥口令错误或私钥已损坏」。
- 03 服务器表单:口令输入框 + 「记住口令」复选(不勾=仅本次连接使用,不落盘)。测试连接按钮覆盖此路径。

### 3.2 主机密钥校验(ssh.rs 修改点 1,TOFU)

- `ServerConfig` 追加 `host_key_sha256: Option<String>`;`check_server_key` 按 per-server 记录比对(OpenSSH 格式 `SHA256:base64`):
  - 首次连接(无记录)→ 接受并返回指纹,由命令层存入配置;
  - 已有记录且匹配 → 接受;
  - **不匹配 → 拒绝连接**,报错:「服务器主机密钥已变更!可能为服务器重装/换 IP,也可能存在中间人风险。如确认无误,请在服务器管理中重新信任该主机。」
- 03 服务器管理:每个服务器显示当前指纹(等宽),「重新信任」按钮(清空记录,下次连接重新 TOFU)+ 二次确认。

### 3.3 配置导出 / 导入 / 一键清除(config_io.rs)

- **导出**:收集 servers/projects/notify(+各自密文);敏感密文用「导出口令」重加密为可移植格式(Argon2id 派生密钥 + AES-256-GCM,新依赖 argon2/aes-gcm);输出 JSON 文件(系统保存对话框)。导出口令不入文件。
- **导入**:选择文件 + 输入口令 → 解密校验 → 展示摘要(几台服务器/几个项目) → 确认后**整体替换**当前配置(用户定案:不做逐项合并),完成后刷新全部页面状态。
- **一键清除数据**(放在同一模态):二次确认采用「输入 DELETE 后才可点确认」的强确认;清除范围 = 应用数据目录(config.json + logs),完成后应用自动重启(或提示手动重启)。
- **入口**:03 服务器管理页 page-tools 加「配置中心」按钮 → 模态内三块:导出 / 导入 / 清除(危险区单独样式)。

### 3.4 验收要点
- 加密私钥 + 口令连接成功/口令错误两种路径;不勾记住口令时配置文件无口令痕迹。
- 主机密钥变更场景:手工替换服务器指纹 → 连接被拒且提示可操作。
- 导出→删除配置→导入 恢复等价(密码经 DPAPI 重加密仍可连接);错误口令导入明确报错。

---

## 阶段四:桌面体验(设置中心)

### 4.1 设置模态(新 settings.js + settings 菜单入口)

- 入口:dock 底部(帮助按钮上方)齿轮按钮 →「设置」模态,三组:
  - **外观**:主题 亮/暗/**跟随系统**(matchMedia 监听,localStorage 键扩展为 auto|light|dark,现有 scheme-toggle 保留为快捷切换)
  - **通用**:关闭窗口时隐藏到托盘(默认关)
  - **更新**:检查更新按钮 + 代理地址输入(http/socks5)+「测试连接」按钮
- 现有 dock 的主题切换按钮行为不变(点击即切,写回显式值,覆盖 auto)。

### 4.2 系统托盘(tauri 2 内置 tray API,无新依赖)

- 托盘图标 + 菜单(显示主窗口 / 退出);左键单击显示主窗口;部署进行中托盘 tooltip 显示状态。
- 「关闭到托盘」开启时:关闭窗口 → 隐藏;菜单退出才真正退出。托盘常驻(无论开关)。

### 4.3 检查更新(自实现,免签名托管;决策理由)

- **决策**:自实现检查(请求 GitHub API `releases/latest` 比对版本号)→ 有新版则展示版本说明与「前往下载」(打开 Release 页);**不采用** tauri-plugin-updater(需生成/保管签名密钥与 latest.json 托管,对单发行渠道收益低;未来需要静默更新时再升级,记录为决策)。
- **自定义代理**(用户定案需求):reqwest 客户端按设置中的代理地址构建(http/socks5,新依赖已有 reqwest?核实后定);「测试连接」按钮实际拉取一次 API 并回显成功/失败原文。
- `update.rs`: `update_check(proxy) -> {current, latest, url, notes}`;无网络/代理错误 → 中文分类提示(代理不可达/DNS 失败/限流)。

### 4.4 验收要点
- 主题跟随系统:切换系统深色模式应用即时跟随;手动切换后停用跟随。
- 关闭到托盘:开启后关闭窗口进程存活、托盘可恢复;关闭后部署继续进行不受影响。
- 检查更新:配置代理后可从无法直连 GitHub 的网络环境获取最新版本信息;代理错误有可读提示。

---

## 阶段五:远程管理补遗

### 5.1 终端 resize 接线(遗留 P2 清账)

- 打开终端时按输出区宽度/等宽字符宽计算 cols/rows 调 `manage_exec_resize`;窗口缩放(终端模态尺寸变化)与「自动(推荐)」重开会话时重新计算。后端已就绪,纯前端。

### 5.2 compose .env 查看 / 编辑

- 后端(manage_stacks.rs 追加):
  - `manage_stack_env_read(compose_file)`:同目录探测 `.env`(`test -f`),存在则 `cat` 读回(base64 传输防二进制/编码损坏);不存在返回空标记
  - `manage_stack_env_save(compose_file, content)`:内容 base64 编码后 `| base64 -d > '.env'` 原子写(写临时文件 + mv),路径经 shell_quote;保存前二次确认(影响下次 up)
- 前端:栈 Tab 操作列加「.env」按钮 → 模态 textarea(等宽字体,只读打开 + 「编辑」进入可写);空文件显示占位说明。帮助章节「05 Compose 栈」同步补此按钮说明。

### 5.3 验收要点
- resize:拉伸窗口后终端不截断;`.env` 中文/空行/`#` 注释/引号值 roundtrip 无损;保存后服务器 `cat .env` 与编辑内容一致。

---

## 文件变更清单(全计划汇总)

### 新增
`src-tauri/src/transfer.rs` / `notify.rs` / `update.rs` / `config_io.rs`;`ui/notify.js` / `ui/settings.js`;`ui/fonts/`(已存在)

### 修改(均为追加/最小改动)
- `lib.rs`:模块声明 + 命令注册(各阶段)
- `config.rs`:`ServerConfig`(+host_key_sha256、key_pass_enc)、`AppConfig`(+notify、settings)
- `commands.rs`:部署参数扩展、通知触发挂点、导出导入命令转发;`ssh.rs`:两处声明内修改(见风险声明)
- `Cargo.toml`:lettre、tauri-plugin-notification、argon2、aes-gcm(+视核实结果补 socks 支持)
- `ui/index.html`:04 通知入口/历史回滚按钮、03 配置中心入口、dock 设置按钮、各模态骨架;`ui/style.css`:新表单/模态样式(末尾追加)
- `ui/deploy.js`:回滚按钮/跳过开关接线(最小);`ui/app.js`:主题跟随系统(最小)
- `wiki/`、帮助章节随各阶段同步更新

### 不修改
`docker.rs` / `crypto.rs`(仅复用 DPAPI)/ `history.rs`(仅追加字段)/ `stack.rs` / 旧页面核心逻辑

## 实施顺序与发版

1. 计划期拟定逐阶段独立发版:阶段一(智能传输+回滚)→ v4.7.0;阶段二(通知)→ v4.8.0;阶段三(安全)→ v4.9.0;阶段四(桌面)→ v5.0.0;阶段五(补遗)→ v5.1.0。**实际执行:五阶段逐阶段独立提交合入,未随阶段发版,最终一次性并入 v5.1.0 发布**(2e9bd3e 统一 bump)。每阶段走:实施 → 并行审查(验证+代码审查子代理)→ 验收;发版沿用 v4.6.0 流程。

---

## 完成记录(2026-09-05)

> 五阶段全部实施、审查、验收完毕并合入 main;实际按计划逐阶段独立提交,版本号未随阶段 bump,完成后统一提升至 **v5.1.0**(2e9bd3e)。测试基线 142 → **201 passed / 10 ignored**。

### 阶段一:智能传输与一键回滚 ✅(db10497)

- 跳过未变化镜像:镜像 ID(配置摘要)内容寻址比对,`docker images --no-trunc` 与本地 `image_id_by_ref` 同为完整 64 位口径(回归单测锁定);整栈打包/上传/装载全链路过滤(全未变化时三步全跳),单镜像仅非日期标签模式生效;`force_archive` 留档跳装载
- 回滚:releases 留档写 `manifest.json`(逐服务 tag/包名/跳过标记)+ compose 副本;三级判定(load 留档 tar / 未留档但 tag 仍在 / 明确报错);四个 `rollback_*` 命令复用 deploy-log/deploy-done 事件流与 history(成功落 `mode="rollback"` 记录,含留档归属校验与部分装载失败提示);`DeployRecord.release_dir`(serde default)
- 前端:部署选项复选框(跳过未变化/强制留档)、历史表操作列 + 回滚模态(整栈选留档/单镜像选日期标签,执行期日志镜像与互斥)

### 阶段二:通知中心 ✅(9f673e5)

- notify.rs:桌面系统通知(tauri-plugin-notification,Rust 侧直调不走 IPC)+ SMTP 邮件(lettre 0.11 rustls,ssl/starttls/none 三模式,错误六类中文分类);配置存独立 `notify.json`(DPAPI 加密密码,查询脱敏只回 passwordSaved)
- 部署成功/失败/取消与回滚收尾接入 `notify::fire`(异步不阻塞,失败仅日志);Pre 钩子取消透传修复(取消不再误报为失败)
- 保存校验(启用邮件须主机+合法收件人)、端口兜底按加密方式(ssl→465/starttls→587/none→25)、notify.json 损坏时置位 NOTIFY_UNHEALTHY 跳过写回防丢密文
- 前端 notify.js:04 部署页通知入口(状态徽标)+ 独立配置模态(桌面/邮件/事件订阅 + 测试桌面通知 + 发送测试邮件,行内结果回显)

### 阶段三:连接与数据安全 ✅(ec84aa7)

- 加密私钥口令:`AuthConfig.key_pass_enc`(DPAPI),ssh connect 新签名透传口令(风险声明修改点 2),`test_server` 一次性口令 + 「记住口令」;口令错误中文分类(「私钥口令错误或私钥已损坏」)
- 主机密钥 TOFU:`ServerConfig.host_key_sha256`,`check_server_key` 按指纹校验(风险声明修改点 1),变更拒绝连接并提示重新信任;首次连接自动记录(`persist_host_key_if_needed`,commands 与 manage 共用);`retrust_host_key` 命令 + 03 页指纹展示/重新信任按钮
- 配置中心(config_io.rs):导出/导入(Argon2id + AES-256-GCM 可移植加密信封,DPAPI 明文导出/重加密导入,整体替换)+ 一键清除(4 个 json,logs 保留,前端 DELETE 强确认)

### 阶段四:桌面体验 ✅(c35c1f8)

- 系统托盘:图标 + 菜单(显示主窗口/退出)+ 左键单击显示;关闭窗口拦截**现读** settings.json(保存即生效),「关闭到托盘」可开关
- 检查更新(update.rs):GitHub releases/latest 比对(分段数值版本比较),自定义代理(http/socks5,reqwest socks feature),错误六类中文分类;`open_external` 用系统浏览器打开下载页(仅 https);决策记录:不采用 tauri-plugin-updater(见决策 #35)
- 主题跟随系统:app.js `data-ark-scheme` 支持 auto(matchMedia 实时跟随),手动切换仍写显式值;头部防闪白脚本同步支持 auto
- 设置中心(settings.js):外观/通用/更新三组,独立 `settings.json`(camelCase),保存与检查更新代理可未存先测;修复 app_settings_set 契约包装键

### 阶段五:远程管理补遗 ✅(fa5af66)

- 终端 resize 接线:字符量测 + ResizeObserver / 窗口 resize 防抖上报 `manage_exec_resize`(失败回滚缓存可重试,eof 后停发);遗留 P2 清账
- compose `.env` 查看/编辑:`manage_stack_env_read/save`(base64 传输,256KB 前后端双拦,临时文件 PID 后缀防并发,`wc -c` 读侧限长,U+FFFD 非 UTF-8 警示),保存二次确认取消保留草稿;帮助章节同步

---

# 第二批升级（v5.2 / v5.3）：断点续传 · 批量部署 · 清理分析 · 实时日志 · 镜像迁移

> 2026-09-06 第二轮构想选定（A2 + B 组），CI/CD 已先行落地（4a292c1）。
> 硬约束同前；ssh.rs 本批允许**纯追加**（exec_streaming / sftp_download），零修改既有行。

**进度**：阶段六 ✅ 断点续传｜阶段七 ✅ 批量部署｜阶段八 ✅ 清理分析｜阶段九 ✅ 实时日志｜阶段十 ✅ 镜像迁移

## 阶段六：部署断点续传 → 发版 v5.2.0（顺带验证 release.yml）

- **checkpoint**：`config/resume-deploy.json`，键 `server_id|project_id|mode`，每步完成落盘 `{step_next, ts, artifacts}`；每服务器+项目一份，新部署覆盖，最多留 10 份
- **幂等化**：续传前校验本地 tar 存在且大小匹配（不匹配则重跑导出）、远端 tar 用 sftp_stat_size 断点续传、load 前远端 ID 比对跳过、compose up 天然幂等
- **命令**：`deploy_resume_status(server_id, project_id)` / `deploy_resume_start(key)`（复用管线事件）/ `deploy_resume_discard(key)`；取消/成功/新部署清 checkpoint
- **前端**：部署失败后与进入 04 页时查询状态 → 「从步骤 N 继续」横幅按钮 + 「放弃断点」；放弃确认
- **release.yml 首次真实验证**随本阶段发版完成

## 阶段七：多服务器批量部署

- **后端**：管线内层改造——`run_deploy_*` 拆出不含 deploy-done/history 的 inner，单发包装器保持现行为；`deploy_batch(req{..., server_ids[]})` 串行循环每台（智能传输天然 per-server），每台发 `deploy-batch {server_id, server_name, state: queued|running|success|failed|skipped, message}` + 独立 history（可回滚）；取消 = 停当前 + 余台 skipped
- **前端**：04 页「批量部署」入口 → 服务器多选模态 → 逐台状态徽章面板 + 共享日志（deploy-log 加 `[服务器名]` 前缀）；批量期间互斥

## 阶段八：清理分析（prune 预览）

- `cleanup_preview`：悬空镜像（带体积）/ 停止容器（列表）/ 未用卷（列表）/ build cache 总量
- `cleanup_execute(sections)`：勾选后定向 `docker image/container/volume/builder prune -f`（Docker 只清未使用资源），日志流式
- 前端：03 服务器管理 prune 按钮升级为分析模态（分组勾选 + 预计释放 + 二次确认）

## 阶段九：live-follow 日志

- ssh.rs 纯追加 `exec_streaming(cmd, cancel_rx, on_line)`：wait 循环 select! 取消通道，取消时主动 close
- `manage_log_stream_start/stop`（新 manage_logs.rs，State 管流句柄），事件 `manage-logs {stream_id, data, eof}`
- 前端：容器/栈日志模态加「实时跟随」开关（追加上限 5000 行，关闭/切换即停流）

## 阶段十：跨服务器镜像迁移

- ssh.rs 纯追加 `sftp_download(remote, local, on_progress)`
- `migrate_images(source, target, images[])`：源 save→gzip → sftp_download 本地 → sftp_upload 目标 → load；目标同 ID 自动跳过；事件 `migrate-log`/`migrate-done`
- 前端：05 镜像 Tab「迁移镜像」→ 目标服务器 + 镜像多选模态 → 逐个进度

**发版**：阶段六合并后发 **v5.2.0**（验证 release.yml）；阶段十合并后发 **v5.3.0** 收官（Wiki 随更）。

---

## 第二批完成记录（2026-09-08）

> 阶段九、十实施、审查完毕合入 main,第二批五阶段收官,统一提升至 **v5.3.0**。
> 测试基线 216 → 216 passed / 0 failed / 12 ignored(阶段九/十真机测试 +2)。
> 附带修复:`CleanupSection` 死代码警告(#allow 保留待多节扩展);wiki 全量同步至本状态。

### 阶段九:live-follow 日志 ✅

- ssh.rs 纯追加 `exec_streaming(cmd, cancel_rx, exit_code, on_output)`:`tokio::select!` wait 循环,取消信号(mpsc `()`)到达时主动 `channel.close()` 立即返回 `Ok(false)`;输出行为与 `exec` 一致(stdout+stderr 合并、完整行回调、尾行收尾)。零修改既有行,零新增依赖(取消通道用 tokio mpsc)
- 新模块 `manage_logs.rs`:`manage_log_stream_start`(target=container/stack,拼 `docker logs -f` / `compose logs -f --project-directory`,路径经 shell_quote,tail 语义与一次性日志一致 0=全部)+ `manage_log_stream_stop`(按 generation 精准停流,幂等);`LogsState`(generation 单流,重复 start 自动替换旧流);单行 16KB 截断、整体 3600s 兜底超时;事件 `manage-logs { streamId, data, eof }`(camelCase)
- 前端(manage.js):容器/栈日志模态顶栏加「实时跟随」开关(勾选开流/取消停流,eof 自动复位);追加渲染上限 5000 行丢最旧,接近底部才自动滚底;先订阅事件再 invoke(防早到事件丢失);模态关闭(execOnModalClose)与离页(onLeaveC)统一停流
- 帮助/契约文档随更;真机测试 `test_exec_streaming_and_cancel_real`(自然结束+yes 流取消两路径)

### 阶段十:跨服务器镜像迁移 ✅

- ssh.rs 纯追加 `sftp_download(remote, local, on_progress)`(64KB 块,显式 shutdown 等远端确认,进度回调;真机测试 `test_sftp_download_real` 覆盖 0x00-0xFF 全字节 roundtrip)+ `raw_exec_channel`(不经行拆分的原始通道,二进制流专用,pub(crate))
- commands.rs:`migrate_images(sourceId, targetId, images[])` 立即返回,后台逐镜像串行——源 `docker save | gzip`(原始通道 Data 块直写本地临时文件,**源端零落盘**)→ `sftp_upload` 推目标 `/tmp` → 目标 `docker load` → 清两端临时产物;**目标同 ID 自动跳过**(双侧 `docker image inspect` 完整 ID 比对复用 `same_image_id`);取消经 `migrate_status(cancel=true)` 置位,镜像边界生效(协作式);事件 `migrate-log { migrateId, line }` / `migrate-done { migrateId, success, message, imagesDone }`(camelCase);`MigrateState`(Arc+AtomicU64 generation)与批量部署同样不参与断点
- 前端(manage.js):镜像 Tab「迁移镜像」按钮 → 模态(目标服务器下拉排除当前 + 镜像多选带大小 + 迁移日志区);`manage_list_servers` 现取保证目标列表新鲜;迁移中按钮禁用,done 后自动刷新
- 帮助/契约文档随更

### 阶段六:文字样式统一(随 v5.7.2 发布)

用户反馈「整栈回滚确认这块文字都是原样很生硬」;全面普查(171 处 font-size / 46 处 line-height /
40 处 letter-spacing)定案三项决策(均已用户确认)并外加四项低风险附赠:

- **字号彻底归并**:16 种取值收敛为 10/11/12/12.5/13/15/18/22/34/84(56);淘汰 10.5→11、
  11.5→12.5、13.5→13、16→15(~20 处声明)。`.manage-table` 表内 11px 覆盖保留(列宽紧张的刻意
  再压缩);`.manage-terminal` 12px/1.5 与 manage.js 字符量测硬编码耦合,明确不碰
- **确认框三段式 + 风险分层**:app.js 新增 `window.confirmBlock({title, facts, risk})`,全站 7 条
  确认渲染路径统一「主问句 15px/700 + 事实清单 12.5px 键值 + 风险提示 12.5px 琥珀 + 左条」
  (deploy 整栈/单镜像、rollback 整栈/镜像、manage 删容器/镜像/卷/网络/启停栈与 .env 保存、
  servers 安装 Docker/清理执行);风险句走既有 `--ark-warn-text`/`--ark-warn`,不引入新颜色;
  config-io 危险区为参照实现不动。rollback.js 原 `
` 拼 `<pre>` 的 UA 等宽与 1.8 行高问题随之消失
- **word-break 分类**:散文/错误/toast/结果行 13 处 `break-all` → `overflow-wrap: anywhere`
  (中文不再硬断词);数据单元格(长哈希)与日志/终端(5 处)保留 break-all
- **微标签两轨制**:结构标签轨(cond / 11px / 大写 / .12em:各表头与区块标题补 font-family、
  低区间 .08/.1 升 .12)+ 数据微标签轨(mono 保持);port-badge 归数据轨,.06em 计数行升 .12em
- **附赠**:hint 族行高统一 1.6、help 正文 1.85→1.7;hint 颜色收敛 `--ark-paper-dim-ink`;
  tabular-nums 补齐(manage 容器/镜像/卷创建时间 + 大小、监控内存%/PIDs,images 两列,rollback
  标签时间);18 处死回退清理(rgba 字面量 / `#d68a1e` / `, inherit`);迁移后死规则
  `.confirm-text`/`.rollback-confirm-text` 删除

**验证**:`node --check` 全通过;ark-ui 审计 0 error / 12 pass 与基线一致;Chrome headless 亮暗两主题
`getComputedStyle` 断言三段式与断词;淘汰字号与死回退 grep 归零;真实 index.html + Tauri 桩集成加载
零 JS 错误;`cargo test` 269 passed。

### 遗留与取舍(记录在 wiki 07 已知限制)

- 实时跟随行数上限 5000 由前端裁剪(后端只管推送);`manage-logs` 事件归属按「当前活跃会话唯一」过滤,后端 streamId 为后端代号,前端未知(全局单流语义下无歧义)
- 镜像迁移逐台串行、无断点(与批量部署同取舍);取消在镜像边界生效,大镜像传输中不即时中断
- exec_streaming 取消通道约定:全部 Sender drop(Err)同样按主动取消收场——命令层 stop 先发信号再 drop,时序保证正确


---

# 第三批升级（v5.4.0）

**目标**:修掉「清理分析识别不到服务器资源」的真因,把清理从「全局 prune」升级为「按服务器实际项目分列、可逐项勾选」;新增独立的回滚中心;补上「源 compose 变更后不必重新导入」的更新机制与文件映射默认名。

**进度**:阶段十一 ✅ / 阶段十二 ✅ / 阶段十三 ✅ / 阶段十四 ✅ —— 随 **v5.4.0** 发布;实测反馈的缺陷修复并入 **v5.4.1 / v5.4.2**

| 阶段 | 主题 | 关键产出 |
|---|---|---|
| 十一 | 清理分析重构 + 分项目清理 | 修 camelCase 真因;解析容错;`<none>` 全量过滤;扫描起点可配;分项目(归档/标签)逐条删除 |
| 十二 | 项目「从源更新」 | `source_compose_path`/`source_hash`;启动自动比对 + 手动按钮 + 徽章;保留服务分类 |
| 十三 | 独立回滚中心(06 页) | 按服务器真实目录扫描项目;归档/标签两级回滚;路径前缀归属校验 |
| 十四 | 文件映射默认名 | remote 留空 → 本地末段名(前端失焦自动补 + 后端兜底) |

### 阶段十一:清理分析重构 + 分项目清理 ✅

**真因(三个叠加)**:
1. `CleanupReport` 标了 `#[serde(rename_all = "camelCase")]`,而 `servers.js` 一直读 snake_case(`report.dangling_images` 等)——字段恒为 `undefined`,**四项计数永远显示 0**,与服务器实际状态无关。这是「有 `<none>` 镜像却识别不到」的首因。
2. `docker images -f dangling=true` 在 BuildKit / containerd image store 下常返回空 → 改为 `docker images --no-trunc` 全量拉取后客户端过滤 `Repository/Tag == "<none>"`。
3. `parse_cleanup_ndjson` 对每行强制 JSON 解析,而 `exec_collect` 合并 stdout+stderr —— 服务端一句 `WARNING: No swap limit support` 就让该节整体解析失败、列表恒空 → 改为跳过非 JSON 行并收进 `warnings`。

**新增能力**:
- `CleanupReport` 增 `warnings`(非致命提示)/ `diagnostics`(逐条命令 + 退出码 + 输出摘要)/ `scanRoot` / `projects`(分项目:目录、`du -sh` 占用、releases 归档、旧日期标签镜像,按 compose 内 `image:` 仓库名归属)
- `cleanup_preview` 增 `scanRoot` 参数(默认服务器 `remote_dir`,可填 `/home` 逐层向下;深度 ≤4,排除 `releases/`、`.git`),并与 `docker ps` 的 compose labels 合并
- 无标签镜像与旧标签镜像用容器引用集合(`docker ps -a --no-trunc` + `docker inspect --format '{{.Image}}'`)标记在用,在用项前端禁选
- 执行改**显式目标列表**逐条删除(`rmi` ID / `rm` ID / `volume rm` / `rm -rf` 归档目录),取代 `prune -f`(其删除范围由 docker 自判,与勾选可能不一致);逐节 catch,单节失败(含传输层错误)不中断后续
- 前端清理模态:扫描起点输入 + 「重新扫描」、提示折叠区、**扫描诊断折叠区**、分项目区块(归档默认保留最新 5 个,与部署收尾同口径)
- 新增纯函数单测 12 个:NDJSON 容错 / 警告条数上限 / `image_repo_of` / `is_date_tag` / `compose_image_repos` / `split_compose_dump` / `project_dir_of_release` / `parse_du_output` / `abs_path_lines` / 清理命令拼装转义 / 扫描命令形态 / `has_any` 校验

### 阶段十二:项目「从源更新」 ✅

- `ProjectConfig` 增 `source_compose_path`(导入来源绝对路径)与 `source_hash`(compose + `.env` + override 内容 sha256,文件名与长度一并入哈希),`#[serde(default)]` 兼容旧配置
- `copy_compose_bundle` 统一导入与更新的复制口径(compose + `.env` + override 同名副本)
- `check_project_sources`(只读比对,状态 `unchanged`/`changed`/`missing`/`unknown`)+ `update_project_from_source`(旧副本备份 `.bak` → 重拷重解析 → **合并保留 `service_overrides`**:仍在的服务沿用旧分类、新增取默认、消失的丢弃;其余字段不动)
- 前端:启动加载配置后自动比对并更新(设置项 `autoUpdateFromSource` 可关,缺省开启)+ 项目卡片「从源更新」按钮 + 「源已变更 / 源文件丢失」徽章;`st.sourceChecking` 防重入(启动路径与 pagechange 并发时不重复更新 —— 实测发现并修复)

### 阶段十三:独立回滚中心(06 页) ✅

- 现状回滚入口埋在「04 页 → 部署历史」,释出目录固定 `server.remote_dir/releases`,非本应用部署的服务器项目无法回滚
- 新命令 `rollback_scan_projects`(项目清单 = compose 扫描 + `com.docker.compose.project.working_dir` label 合并,按**服务器真实目录**;`appProject` 仅标注)/ `rollback_project_detail`(归档含 manifest 服务清单 + 各仓库日期标签)/ `rollback_execute_stack_at`(按目录回滚;**归属校验按路径前缀**而非项目名;compose 恢复目标为 `<dir>/docker-compose.yml`)
- 新模块 `ui/rollback.js`(照 manage.js 骨架:IIFE + pagechange 进出 + 事件单次注册守卫 + 离页清理;纯远程操作不依赖本机 Docker,不入 `LOCKED_PAGES`);`index.html` 加 dock 第 6 项 + section + script;CSP/样式(`.rollback-*`)同步
- 两级回滚:整栈回滚到归档(任何扫描到的项目)/ 单镜像切回日期标签(复用 `rollback_execute_single`,需软件内已配置项目)
- 顺带修正 wiki 03 中过时的 `deploy-rollback-modal` 描述(实际一直是 `#deploy-modal`)

### 阶段十四:文件映射默认名 ✅

- 前端 `localBasename` + `collectMappings`:remote 留空时按本地末段名自动填充(本地格 blur 或「浏览」选择后;已填值不覆盖),保存时兜底;不再因 remote 为空报错阻断保存
- 后端 `sync_files` 同样兜底(remote 为空 → 本地末段名),新增 `local_basename` 纯函数(兼容 `\` 与 `/`,去尾部分隔符)

### 完成记录(2026-09-10)

- 功能提交 `39e386e`(单提交含四阶段;紧随 `bc033f3` 自动更新下载 URL 修复与 `6a885bd` v5.3.2 版本提升)
- 测试基线 224 → **232 passed / 0 failed / 12 ignored**(新增 12 个纯函数用例;clippy 无新增告警,存量 7 条)
- 浏览器实测(注入 Tauri 桩)7 项:项目卡片按钮矩阵 / 清理模态四节计数(修复后非 0,在用项标注) / 清理执行载荷(仅未在用 + 仅 2 个最旧归档) / 回滚中心两级操作与执行载荷 / 从源更新载荷 / 映射名推导(文件、目录、保留手填) / 启动自动更新(防重入后恰好 1 次)
- wiki 全量同步:01(目录 + 第四条数据流)、02(清理/源更新/回滚中心三节 + 行数)、03(页面结构 6 页 + rollback.js 章节 + servers.js 变更)、04(清理/回滚中心/源更新三节契约 + 命令计数 84 + 变更清单两条)、06(独立回滚中心章节)、07(注入防护三行 + 决策 #46-#51 + 已知限制 #27-#32)、README(命令数/页面数/测试数/版本)、本文件

### 修复记录(v5.4.1,2026-09-10)

用户实测反馈两处问题,审查后确认均为真实缺陷:

**① 自动更新装完不重启、也无提示**

- 根因:`update_install` 启动 NSIS 时只传 `/S`。Tauri 生成的 `installer.nsi` 在 `.onInstSuccess` 里**只在静默(`${Silent}`)或被动模式**下解析 `/R` 并 `RunAsUser` 拉起应用(GUI 模式靠完成页复选框)—— 装完进程退干净就再没人启动新版
- 修复:改传 **`/S /R`**;并在调用前写 `config/update-pending.json`(安装器覆盖安装目录但不动 `config/`,标记跨更新存活);新增 `take_update_pending` 命令(读取即清除),app.js 启动时与 `app.getVersion()` 实际版本比对 —— **一致才** toast「已更新到 vX」(安装中途失败不误报),恰好一次

**② 项目更新「没有按钮 / 看不到更新结果」**

- 根因:前端按钮条件为 `project.source_compose_path` 非空,而 v5.4.0 之前导入的项目该字段为空(`serde(default)`)—— 存量项目**既看不到按钮,也无任何入口绑定源**,功能形同未实现;且更新结果只有 toast 一闪而过
- 修复:后端 `ProjectSourceStatus` 增 `imported`(compose 副本是否在 `config/stacks/` 下)与 `bound` 字段;新增 `bind_project_source` 命令(选文件 → 校验可解析 → 记录路径与哈希基准);前端对所有导入项目显示按钮(已绑定 =「从源更新」,未绑定 =「绑定源」),新增「未绑定源」徽章、section-head 的「检查源变更」按钮,以及项目表上方的**源检查汇总栏**(状态计数 + 待办项目名 + 最近操作结果,常驻不消失)
- 状态机调整:`unknown` 拆为 `unbound`(未绑定)/ `unreadable`(源不可读);已绑定但缺哈希的旧配置按当前内容补算基准报 `unchanged`(用户已明确绑定,不该再无从下手)

**验证**:`cargo test` 233 passed / 0 failed / 12 ignored(新增标记 round-trip 单测;修复单测污染全局 `DD_CONFIG_DIR` 导致的偶发失败 —— 改为 `DD_UPDATE_PENDING_PATH` 注入隔离,连跑 3 次稳定);浏览器实测 5 项:项目卡片按钮矩阵(未绑定→「绑定源」+ 徽章、已绑定→「从源更新」+「源已变更」徽章)、绑定源载荷与汇总栏、更新结果落汇总栏「最近操作」、更新完成提示三场景(版本一致→提示 / 不一致→静默 / 无标记→静默)、JS 语法全通过

### 修复记录(v5.4.2,2026-09-10)

用户实测反馈第三处缺陷:修改源 compose 后手动点「从源更新」提示「无改动」,但到部署目录核对发现副本确实已同步。

**根因**:`update_project_from_source` 先把新哈希写入配置(`p.source_hash = new_hash`),然后复用 `project_source_status(&updated)` 计算返回状态 —— 该函数拿配置里的哈希与源比对,而基准刚被本次写入覆盖,于是**结果恒为 `unchanged`**。文件同步与提示文案自相矛盾。

**修复**:
- 新增 `bundle_content_changed(源, 旧副本)`:在覆盖副本**之前**比较纯内容(compose 本体 + 同名 `.env`/override),据此给出 `changed` / `unchanged`
- 该函数**只比内容、不比文件名** —— `source_content_hash` 把文件名纳入哈希,而副本恒名 `docker-compose.yml`,源若叫 `compose.yml`/`docker-compose.yaml` 时内容相同也会假阳性
- 前端文案区分:「已从源同步改动到配置副本(N 处服务)」vs「源与配置副本一致,无需更新」
- 单测:`test_bundle_content_changed_ignores_compose_filename`(文件名差异 / 内容差异 / 副本缺失 / .env 变化 / override 增删各情形)、`test_update_reports_changed_before_overwriting_baseline`(回归语义)

**验证**:`cargo test` 235 passed / 0 failed / 12 ignored;浏览器实测两种结果文案与汇总栏「最近操作」均按实际改动区分。

### 遗留与取舍(记录在 wiki 07 已知限制)

- 分项目标签归属按 compose `image:` 仓库名匹配:纯 build 服务或变量插值后仓库名不符时该项目的标签不出现在分项目块(不影响通用四节清理)
- 分项目扫描深度 ≤4 且归档目录名须匹配 `20*-*`;更深的目录或自定义命名归档不列出(可手动把扫描起点指到项目父目录)
- 清理执行按前端回传的显式目标,后端不做二次扫描:预览与执行之间服务器状态变化时对应条目报错,其余照常执行
- `unknown` 状态(旧配置无 `source_hash` 或手工项目)不参与启动自动更新,需手动点一次「从源更新」写入哈希
- 回滚中心的 `runningContainers` 以容器名包含目录名近似归属,标签缺失或 `docker ps` 不可用时可能为 0(不影响回滚)


---

# 第四批升级（v5.5.0）

**目标**:解决"一台服务器多个项目时,每次切换项目都要去服务器配置里单独设置"的痛点。根因有两个,同源:① `ServerConfig.remote_dir` 是**服务器级**单一目录,同服务器的多个项目共用同一部署目录与 `docker-compose.yml`;② `ProjectConfig` 没有服务器字段,部署页两个下拉各选各的、不联动也不记忆。

**进度**:阶段十五 ✅ / 阶段十六 ✅ / 阶段十七 ✅ / 阶段十八 ✅ —— 随 **v5.5.0** 发布

| 阶段 | 主题 | 关键产出 |
|---|---|---|
| 十五 | 项目级部署目录 | `ProjectConfig.remote_dir` + `effective_remote_dir` 解析 + 28 处调用点改写 |
| 十六 | 项目默认服务器 | `ProjectConfig.default_server_id` + 部署页自动带出 + 03 页归属展示 |
| 十七 | 部署页选择记忆与联动 | localStorage 记忆 + 项目排序标注 + 部署目录提示行 |
| 十八 | 部署历史配对修正 | `DeployRecord` 补 id + 回滚解析优先 id(修改名失配) |

### 阶段十五:项目级部署目录 ✅

- `ProjectConfig` 增 `remote_dir: Option<String>`(serde default):留空 = 沿用服务器目录(**旧配置行为完全不变**),填了则该项目的部署、compose 上传、整栈 pull/up、健康检查、钩子、回滚全走自己的目录
- 新增纯函数 `effective_remote_dir(server, project)` 统一解析优先级;替换项目作用域内 28 处 `server.remote_dir`;环境检测 / `create_remote_dir` / 回滚中心与清理的默认扫描起点等 5 处保持服务器级(与项目无关)
- 项目表单加「远程部署目录」(校验 `/` 开头绝对路径 + 提示需先在服务器建好目录);03 页项目表加「远程目录」列(未填显示「继承 <服务器目录>」)

### 阶段十六:项目「默认服务器」✅

- `ProjectConfig` 增 `default_server_id: Option<String>`(serde default)
- 项目表单加「默认服务器」下拉(含「不指定」;服务器已删除时提示重选)
- 部署页选中项目自动带出该服务器(**不锁定**,保留临时跨服务器部署能力);03 页项目表加「服务器」列、服务器卡片新增「关联项目」行

### 阶段十七:部署页选择记忆与联动 ✅

- `dd_deploy_server` / `dd_deploy_project` 两个 localStorage 键(同 `dd_manage_autorefresh` 的 restore/save 模式 + try/catch 静默降级),重启后恢复上次选择;恢复时若只记得项目,按其默认服务器补齐
- `projectOptionsFor(serverId)`:属于当前服务器的项目排前并标「★ …(本机)」(**不过滤**,其余项目仍可选)
- `updateProjectHint()`:项目下拉下方显示**实际部署目录**及来源(项目独立目录 / 继承服务器),选完即可确认,不必再去服务器配置核对

### 阶段十八:部署历史配对修正 ✅

- `DeployRecord` 增 `server_id`/`project_id`(serde default 兼容旧记录),部署与回滚四个写入点带上 id
- `resolveRecordIds` 优先按 id 精确匹配、回退按名 —— 修掉「服务器或项目改名后回滚按钮失效」;确实无法定位的旧记录给明确提示(不静默失败)

### 完成记录(2026-09-10)

- 提交 `d2f1918`(单提交含四阶段)
- 测试基线 235 → **238 passed / 0 failed / 12 ignored**(新增 `effective_remote_dir` 回落语义、旧 projects.json 反序列化、旧 deployments.json 反序列化);clippy 维持基线 7 条
- 浏览器实测 5 项:项目表新列(继承 vs 独立目录分别显示)、选项目自动带服务器 + 排序标注 + 部署目录提示、重启(重载)后记忆恢复、改名记录仍可回滚(按 id 命中)、不可解析记录给出明确提示
- wiki 同步:01(行数)、02(两个第四批小节 + ProjectConfig 字段)、03(servers.js 部署位置与两列、deploy.js 记忆联动)、04(ProjectConfig 新字段 + remote_dir 解析说明 + get_history/DeployRecord 补 id)、07(决策 #55-#57 + 限制 #37-#40)、README、本文件

### 遗留与取舍(记录在 wiki 07 已知限制)

- 项目级目录为新配置项,**已部署旧目录的项目不会自动迁移**:填了独立目录需先在服务器建好目录,原目录中的 compose/releases 留档不会自动搬运
- 同服务器多项目若都未填项目级目录,仍共用服务器 `remote_dir` 与同一个 `docker-compose.yml`(向后兼容所致),建议逐项目分配独立目录
- 默认服务器仅作便利(不校验归属),选错不会被拦住;部署页提示行会显示实际目录与来源供确认
- 部署历史里的**旧记录**(无 id)在改名后仍无法回滚,重新部署一次即可写入带 id 的新记录


---

# 第五批升级（v5.6.0）

**目标**:两项用户需求 —— ① 发布归档保留数量写死 5 个,需可按项目单独设置;② 回滚中心对每个版本增加删除按钮(二次确认)。

**进度**:阶段十九 ✅ / 阶段二十 ✅ —— 随 **v5.6.0** 发布

| 阶段 | 主题 | 关键产出 |
|---|---|---|
| 十九 | 项目级归档保留数量 | `ProjectConfig.release_keep` + `release_keep_of` 解析 + 清理命令参数化 + 表单字段 |
| 二十 | 回滚中心逐版本删除 | `rollback_delete_release` / `rollback_delete_tag` + 前端两步确认 |

### 阶段十九:项目级归档保留数量 ✅

- `ProjectConfig` 增 `release_keep: Option<u32>`(serde default):`None` = 沿用默认 5 个(**旧配置行为完全不变**),可填 0-50;0 = 部署后清空历史归档(仅留本次)
- 新增 `config::release_keep_of(project)`(超上限夹到 50)与常量 `DEFAULT_RELEASE_KEEP=5` / `RELEASE_KEEP_MAX=50`
- `cleanup_releases_cmd(remote_dir, keep)` 参数化:`tail -n +<keep+1>`;`keep=0` 即 `tail -n +1` 清空;部署收尾按项目配置执行并在日志说明实际保留数
- 项目表单新增「发布归档保留数量(可选)」(0-50 整数,留空 = 默认 5),三条保存路径均写回;清理分析面板改读该项目保留数(`CleanupProject.releaseKeep`),未匹配到配置时按默认 5 并标注说明

### 阶段二十:回滚中心逐版本删除 ✅

- 新命令 `rollback_delete_release`:校验 `dir` 绝对路径、`ts` 纯目录名(不含 `/` `..` `\`)、拼装后**路径前缀必须落在 `<dir>/releases/` 之下** → 存在性校验 → `rm -rf`
- 新命令 `rollback_delete_tag`:`docker image inspect` 校验存在 → `docker rmi`(**不带 `-f`**,仍被容器引用则失败并回传原因)
- 回滚中心每个发布归档行与每个历史镜像行加「删除」按钮;`armConfirm` 两步确认(首次点击变「确认删除?」并转红,3 秒内再点才执行,超时自动还原;与 03 页项目删除同一套交互,不用系统对话框);成功后刷新明细与项目列表

### 完成记录(2026-09-10)

- 提交 `2c49db7`(单提交含两阶段)
- 测试基线 238 → **239 passed / 0 failed / 12 ignored**(新增 `release_keep_of` 回落与夹取用例;`cleanup_releases_cmd` 用例扩为 keep=5/2/0/50 四态);clippy 无新增告警
- 浏览器实测 5 项:清理面板按项目保留数显示(2 vs 默认 5,且未配置项有文案标注)、表单字段回填(2)/非法值(999)拦截/合法值(3)保存、归档删除两步确认(首次点击 `invokeCount=0`)、确认超时 3 秒自动还原、归档与镜像删除载荷正确
- wiki 同步:04(ProjectConfig 增字段 + release_keep 解析说明 + CleanupProject.releaseKeep + 回滚中心增两个删除命令 + 命令计数 88)、02(归档保留可配 + 逐版本删除两段)、03(表单字段 + 清理面板读配置 + rollback.js 删除按钮)、07(决策 #58/#59 + 限制 #43-#45)、README、本文件

### 遗留与取舍(记录在 wiki 07 已知限制)

- 保留数填 0 = 不留历史归档,将无法回滚到任何历史版本(表单与面板文案均已写明)
- 镜像删除不带 `-f`:仍被容器引用时由 docker 拒绝,需先停用/删除相关容器
- 删除归档只删该目录,不影响当前运行的服务与 compose 文件(与「回滚到此归档」的语义区分明确)

---

## 第六批：项目跨服务器迁移 + 镜像迁移失效修复（v5.7.0）

### 背景：先修复被复用的地基

实施新功能前发现**阶段十的「镜像迁移」自发布起就是坏的**,且是双重失效:

1. **搬不动镜像**:取 ID 用的是 `docker_inspect_cmd`(拼 `docker image inspect '<ref>'`,**无 `--format`**),
   而迁移路径拿它的输出做 `strip_prefix("sha256:")`。`inspect` 默认输出是 pretty-print 的 JSON 数组
   (首字符 `[`),解析恒为 `None` ⇒ 每个镜像都判「源服务器上不存在镜像,跳过」,**一个镜像都不会传**。
   该函数另 3 个调用点只用退出码判存在性,语义正确,故问题只压在迁移上。
2. **点按钮就报错**:后端签名 `migrate_images(app, state, req: MigrateRequest)` 要求顶层 `req` 键,
   前端发的是扁平 `{sourceId, targetId, ...}` ⇒ `missing required key req` 直接落进 catch。

修复:新增专用 `docker_inspect_id_cmd`(`--format '{{.Id}}'`)+ `parse_inspect_id`(拒绝 `[`/`{` 开头
的输出,防止再把 JSON 当 ID);原 `docker_inspect_cmd` 保持只用于存在性判定;前端补 `req` 包装;
`migrate-done` 加 `CatchPanic` 兜底(否则任务 panic 时前端永久停在「迁移中…」);取消按钮接上
`migrate_status(true)`(此前只关模态,无取消路径)。

### 阶段一:stack.rs 卷解析(新增能力)

`stack.rs` 此前完全不解析 compose 的 `volumes:`。新增:

- `parse_compose_volumes` / `collect_volume_mounts`:短语法 `src:dst[:opts]` 与长语法
  `type/source/target` 都支持;与 `parse_compose_file` **共用 `load_compose_document`**
  (override 合并后的文档)—— 否则卷定义写在 override 里会被漏掉,而这种差异极难从结果察觉
- `classify_volume_source`:绝对路径(`/` 或 `C:\`)→ `BindAbsolute`(不搬);`.` 开头 → `BindRelative`
  (可搬);顶层 `external: true` → `External`(不搬);其余 → `Named`,实际卷名取顶层 `name:` 覆盖值
  否则 `<项目名>_<卷名键>`
- 短语法切分**必须跳过 Windows 盘符**:`C:\data:/app` 按「首个冒号切 source」会切成 `C`,故先跳盘符前缀
- `dedupe_volume_specs`:跨服务去重,`key` **保留 compose 原文**而非从 `resolved_name` 反推 ——
  卷名含下划线时反推会截错(`zetok` + `pg_data` 反推成 `data`)

### 阶段二:migrate_project.rs(新模块,2 命令)

以**项目**为单位搬运(镜像迁移只管镜像),`commands.rs` 已 9,900 行且这是自成一体的关注点。
复用 commands 的连接/镜像搬运/命令拼装助手(可见性放宽为 `pub(crate)`),沿用 manage 系列
「复用助手、不反向依赖」的既有模式。

- `migrate_project_preview`(**只读**):连源读 `<源目录>/docker-compose.yml` ——
  **以源实际部署为准,本机副本不作依据**(目标要复现的是源真正在跑的状态;手工项目由此同样可用)
  → 解析镜像/卷/近 N 归档 → 汇总 warnings + errors
- `migrate_project_start`:`②源 stop → ③逐卷 tar 导出 → ④源 start(无条件恢复)` →
  ⑤镜像/卷包/归档经本机中转 → ⑥目标 load + volume create/导入 → ⑦compose 三件套与归档落盘 →
  ⑧目标 up -d → ⑨清两端临时目录 → ⑩改绑 `default_server_id` + 写 `mode="migrate"` 历史
- **停机窗口只覆盖卷导出**(卷必须在停服态导出,写入中打包会产生无法修复的损坏),
  长耗时的镜像/归档传输在源恢复运行后进行;**恢复源无条件执行**,任何导出失败路径都先恢复再返回错误
- 卷导出/导入经 `docker run --rm --entrypoint tar` 临时容器(宿主机不一定有 tar),
  候选镜像链 busybox → alpine → ubuntu → `docker pull busybox`,全失败则明确报错给建议
- 卷名跨机推导 `resolve_volume_names`:两机目录名不同则卷名不同,执行时**按目标侧自己的卷名**
  create+导入(数据不关心名字,关键是目标 compose 找得到),并在 warnings 提示
- 复用 `MigrateState` 单会话(与镜像迁移天然互斥);`CatchPanic` 兜底;`StageDirGuard` 清理本地中转目录

### 阶段三:前端

- 入口:`#deploy-mode-tabs` 内、整栈 tab 右侧「迁移项目…」(`.mode-tab-action` 靠右,青色文字不吃 active 填充)
- 模态 `#migrate-project-modal`(`modal-wide`):选择区(源默认带出 `default_server_id`;目标下拉排除源
  并在源变化时重建;迁移项 —— 镜像+compose 恒选、数据卷默认勾、归档数默认 1 上限 20;目标目录可选)
  → 计划区(两端目录、预计总量、`errors` 红框阻断、`warnings` 清单、镜像/卷/归档三表)
  → 日志区(恒暗面板,上限 2000 行丢最旧、近底才滚)
- `migrateState` **声明在 `refreshControls` 之前**(var 提升陷阱,本项目批量部署曾因同类问题失效)
- 事件订阅先于 invoke、模块级单次守卫;执行中禁关模态;done 回显 warnings 逐条;成功刷新页面数据

### 完成记录（2026-09-11）

- 测试基线 239 → **269 passed / 0 failed / 13 ignored**;新增用例:镜像 ID 命令形态与 JSON 输出拒绝、目录检测/创建命令拼装与防注入、
  卷解析(短/长语法、盘符、external、顶层 name 覆盖、override 内定义)、卷名跨机推导(同/异目录、
  name 覆盖、空声明)、迁移命令拼装与防注入(含单引号的卷名)、事件与计划 camelCase 契约、
  `dedupe_volume_specs` 保留原文键(下划线回归)
- **卷搬运链路已用本机真实 Docker 端到端验证**:建卷写入已知内容(含 256 字节随机二进制 + 子目录)
  → 按代码生成形态导出 → 导入新卷 → 逐文件 sha256 比对完全一致
- 前端:`node --check` 通过 + DOM 桩冒烟(模态构建/源切换重建目标下拉/计划渲染与阻断态/
  有 errors 时禁用执行/日志上限 2000/事件注册齐全)+ 前后端字段契约脚本核对(无错配)
- wiki 同步:README(命令 90/事件 11/版本 5.7.0/代码量/第六批记录)、01(架构图 + 迁移数据流)、
  02(stack.rs 卷解析 + migrate_project 模块节 + history MODE_MIGRATE + 镜像 ID 修复说明 + 测试计数)、
  03(入口与模态)、04(命令/事件/计划契约 + DeployRecord mode)、06(项目迁移全流程)、
  07(决策 #60-#63 + 限制 #46-#52 + 注入防护 + 测试现状)、本文件

### 阶段四:项目表单的远程部署目录检测/创建（第六批追加）

**痛点**:项目级 `remote_dir`(第四批)在服务器上不存在时,过去只能靠部署失败才发现;表单提示让人「到服务器卡片点『创建远程目录』后手动补路径」,而那个按钮建的是**服务器级**目录,路径对不上。

- 后端新增 2 命令(90 → 92):`check_remote_dir({serverId, dir}) -> bool`(只读 `test -d`)、`create_remote_dir_at({serverId, dir})`(`mkdir -p`);均要求 `dir` 为绝对路径(前端也拦一次);抽出共用 `create_dir_on`(服务器级 `create_remote_dir` 一并改用)
- 前端 `appendRemoteDirField`:目录输入框同行「检测」按钮 → 结果区四态(已存在 / 不存在+创建入口 / 失败 / 留空无需创建);不存在时「创建该目录」→ **内联二次确认**(显示将执行的 `mkdir -p <dir>`,不用系统对话框)→ 创建成功后**自动重新检测**
- 检测针对**「默认服务器」下拉选中的那台**(项目级目录只有落到具体服务器才有"存在与否");未选服务器时提示先选。因此把默认服务器下拉提到目录字段**之前**渲染
- 目录或服务器变化即清空检测结果(`input`/`change` 监听),避免残留旧结论误导
- 新增样式 `.dir-check-result` 四态(全部走既有 token)

### 阶段五:ark 设计细节优化 pass 3(随 v5.7.1 发布)

保持 `family=ark` + `depth=complex` 身份不变,补全交互反馈链 + 治理样式技术债。边界依据 ark-ui 参考:
默认 radius 0 是身份特征(仅功能性控件 2-4px)、不用柔光阴影(POPUCOM 特征)、**状态变化要加方向或裁切**、
直接交互 180-350ms / 区域揭示 500-900ms / 注意力循环 1.6-2.4s / 按压反馈 <100ms。

- **动效 token**(此前全站 13 处 transition 全写字面量、无 token):`--ark-dur-press .09s` / `--ark-dur-fast
  .18s`(= 原值,零视觉变更)/ `--ark-dur-modal .22s` / `--ark-dur-reveal .45s` / `--ark-ease-out`
- **按钮三态**(此前 `:active` 全站缺失、忙碌态零图形反馈):按压位移 2px(与既有幽灵按钮同语言)+ 按压期
  短时长;忙碌 `.is-busy` 三段方块步进条(`steps()` 离散跳变,比旋转 spinner 更贴工业语言);待确认
  `.is-armed` 呼吸描边 1.8s。新增 `window.setBtnBusy(btn,busy,label)` 三合一助手,**收敛此前 4 份能力
  不一致的局部 `setBusy`**(settings/notify 带文案,config-io/rollback 不带);后两者是按钮组,刻意只禁用
- **输入框五态**(此前 disabled / readonly **零样式**):聚焦加左缘 3px 信号条(inset,与按钮左楔形、
  rollback 选中条同属「左信号条」母题);禁用降透明;只读 paper-dim 底 + 左缘灰标且不降透明(可选中/复制);
  select 去系统 chrome 改 45° 渐变三角;底色 paper → surface(暗色下原值比面板更暗、读作「凹陷」)
- **模态进出场**(此前瞬显瞬隐):遮罩 opacity + `transition-behavior: allow-discrete`,卡片刻画
  `modal-wipe` 裁切揭示(与切页 page-reveal 同族);`.hidden` 加 `pointer-events:none` 化解退出期吞点击;
  **纯 CSS 零 JS** —— 9 条关闭路径分散 6 模块且带 rbBusy 守卫,改 JS 风险高于收益;不支持时优雅降级为瞬隐
- **tab 下划线扫入**:`.mode-tab.active::after` 与 `.manage-tab.active::after` 统一 `tab-underline`
  (自左 scaleX);后者原为只换 `border-bottom-color` 的纯颜色变化(违反契约)
- **技术治理**:删死 token `--radius`(零消费);补 `--ark-paper-ink-70`(被引用未定义 → 回退 inherit 使
  层级静默失效);`.rollback-item.active` 原引用未定义 `--ark-accent` 且回退 `#c8501e` 是红橙色(**违反
  不用红纪律**)→ 改 `--ark-signal`;圆角越界 6px → 4px(2 处)

**验证**:`node --check` 全通过;ark-ui 审计与基线一致(0 error / 12 pass,1 条既有误报);`cargo test`
269 passed;真实浏览器渲染亮暗两主题状态矩阵(按钮四态 + 输入框五态 + tab + 模态)并以 `getComputedStyle`
逐项核对;真实 `index.html` 集成加载**零 JS 运行时错误**;`--force-prefers-reduced-motion` 下四个新动画
全部归零且元素仍可见可用。过程中修掉一处自己引入的 bug(只读态 `box-shadow` 因特异性相同会盖掉聚焦信号条,
按源码顺序修正并加注释)。

### 阶段六:ark × ui-ux-pro-max 设计评估与细节 pass 4(随 v5.7.3 发布)

以 ark-ui(family=ark / depth=complex 六轴 rubric + 20 条检查项)为主题基准、ui-ux-pro-max
(§1 可访问性 / §2 交互 / §7 动效)为细节基准做一轮全量评估。评估结论:family/depth 维持不变
(六轴核验成立),差距集中在可访问性缺口、v5.7.1 动效立约的后半程(既有规则收敛)、细节修复三类。

- **可访问性补强**(ui-ux-pro-max §1 CRITICAL):
  - 模态焦点三件套:app.js 新增 `modalFocusOpen/Close`(栈式记录,支持 cleanup 叠 servers 的嵌套)+
    document 级 Tab 圈禁;10 个模态开/关路径全部接线(manage 私有 `modalTriggerEl` 与 help 的
    「聚焦关闭钮」收编为共用实现)。修复「aria-modal 已声明而焦点不移、关闭不还焦」
  - toast 播报:ok/info → `role="status"`(polite)、warn/fail → `role="alert"`(断言);
    此前 toast 对屏幕阅读器不可见(全仓唯一 live region 只在 servers 表单聚合错误框)
  - 部署步骤播报:校准仪旁新增 `#deploy-progress-live`(sr-only + aria-live),步骤切换写
    「步骤 N/M:名称」低频文本
  - 焦点环对比:浅色主题 signal 青 (#18d1ff) 对纸白仅 ≈1.67:1(实测复现),不满足焦点指示 ≥3:1
    → 浅色下 outline-color 改墨(≈18.3:1,仅覆盖单属性不改几何);深色 ≈10.96:1 维持青环
  - `.manage-terminal-input` / `.manage-env-editor` 的 `outline:none` 后补 `:focus-visible` 恢复
    (P2-6 同款;置于文件末以源码顺序压过 env 编辑器的 `:focus outline:none`)
- **动效/阴影/z-index token 收敛**(v5.7.1「新规则走 token」的后半程):
  - 新增 `--ark-dur-attention 1.8s` / `--ark-dur-scheme .5s` / `--ark-dur-ticker .9s` /
    `--ark-shadow-float` / `--ark-shadow-modal` / `--ark-z-step|fab|overlay|toast`;
    激活死 token `--ark-dur-reveal`(page-reveal 改其消费)
  - 全量收敛:24 处 `.18s` 时长字面量 → `--ark-dur-fast`;toast `.22s` / scheme `.5s` /
    `btn-step .9s` / 呼吸与脉冲 `1.8s` 字面量全部 → token;`ease`/`ease-out` 关键字 →
    `--ark-ease-out`(到达语义统一;循环保留关键字 ease-in-out/ease-out,约定入文件头注释)
  - 阴影几何 token 化(help-fab/modal-card),toast 补海拔 1 阴影(浮层分离度)
- **细节修复**:`:active` 按压态补齐 9 类可点元素(dock/tab/chip/fab/rollback-item,press token);
  `.deploy-step.current` 2px 边框改「1px 边框 + 1px 内缩 outline」消除 1px 位移;批量面板
  「成功/失败」徽章从 `badge-running/badge-paused`(容器状态类,语义错位)改 `fillBadge` 口径
  (ok/fail);删除 `.badge-*` 四处无效 `border-color` 声明(基类无 border);3px 圆角离群 → 2px;
  `color-mix` ×3 沉淀 `--ark-warn-45/50/60`;删死 token `--ark-ok`(零消费,双色纪律注释);
  `--ark-ink-35` 改语义名 `--ark-ink-faint`(亮 .45/暗 .35,数字后缀名不副实);opacity/字距
  `0.XX` 写法统一;CPU warm/hot 单元格补阈值 `title`;截断单元格悬停补全文本(app.js 委托监听,
  仅真溢出时写 title)
- **融合新动效**(均带信息角色,克制):监控 CPU 档位迁移时行首格闪现一次左缘信号条
  (`stat-flash`,manage.js 记录前档位 + 移除/reflow/复加);Tab 面板与整栈面板显示时 0.18s
  纵向微型 wipe(`panel-wipe`,与 modal-wipe/page-reveal 同族)。两者 reduced-motion 下自动归零
- **刻意不做(记录)**:间距/字号 token 化(churn 大收益小);模态改「从触发源展开」(保留 ark
  wipe 签名);列表入场 stagger(违反 excessive-motion,page-reveal 已覆盖)

**验证**:`node --check` 8 个 JS 全过;ark-ui 审计目录级 **0 error**(唯一 warning 为 JS 动态构建
DOM 的既有误报);对比度实测:焦点环亮/暗 18.29:1 / 10.96:1、stat-warm/hot 亮暗四组 5.59–13.14:1、
badge-ok 暗 8.11:1,全部过 WCAG;时长/缓动字面量 grep 归零(仅 token 定义与注释保留)。

### 阶段七:字体轨道筛查与统一(随 v5.7.4 发布)

用户审查发现「认证 AUTH」「待确认」「(未指定)/继承服务器/(空,匹配全部)」「已连接」等细节
文字与全站格格不入。**根因**:`--font-mono`(JetBrains Mono)与 `--font-cond`(Space Grotesk)
都没有汉字字形,这些中文元素回退到系统 generic monospace/sans-serif——Windows 下即**宋体**
(细衬线),与全站 Noto Sans SC 黑体冲突;`.none-text` 还叠加仿斜,双重违和。

- **全量筛查**:45 处 `--font-mono` + 11 处 `--font-cond` 声明逐个按「实际内容是否含中文」分类
- **系统性兜底**:两个轨道字体栈尾部内置 `"Noto Sans SC", "Microsoft YaHei"`——日志/终端/
  路径值/数据行里夹杂的中文一律渲染为黑体,任何组件再漏也不落到宋体
- **中文主体元素出轨道(17 类)**:`.badge`(CJK/500,徽章文字是中文状态词——「待检测/待确认/
  已连接/部署中」的主要来源)/ `.data-table th` 与 `.migrate-plan-table th`、`.help-content th`
  (表头中英混排)/ `.form-group-title` 四别名(中文主体曾随容器落 cond)/ `.kv-key`(键名
  「认证 AUTH」混排,字距 .14em→.08em)/ `.overview-label` / `.inspect-label` /
  `.manage-overview-label` / `.rollback-list-title` / `.migrate-plan-label` / `.migrate-log-head` /
  `.banner-ok` / `.modal-close` / `.log-toggle` / `.cio-danger-title` / `.server-check-title`
- **中文占位符**:新增 `.none-cjk`(CJK + 弱化墨);`.none-text` 的 mono 斜体只保留给 `<none>`
  这类 ASCII 数据值;servers.js 五处 + deploy.js 一处中文占位换类,`(空,匹配全部)` 改条件渲染;
  行 hover 反色配套 `.none-cjk` 色变
- **规则入约**:style.css 文件头 + wiki/03 立约「mono/cond 只承载 ASCII;内容含中文的元素
  一律 --font-cjk」;字距按「中文主体不进轨道」收敛(.12/.14em → .08em)

**验证**:`node --check` 全过;浏览器实测各元素 computed font-family 全部解析到 Noto Sans SC
栈(徽章/kv 键/表头/分组标题/none-cjk/modal-close/log-toggle/banner-ok);真页截图 +
临时 DOM 探针条(待检测/待确认/已连接/失败徽章 + 三种中文占位)确认中文全部黑体、无宋体残留;
judge 前后同场景对比(暗色管理页:未连接徽章/概览标签/表头/占位符)确认修复达成、无字距/
字重/布局回归。

### 遗留与取舍（记录在 wiki 07 已知限制）

- 源服务器内容保留不动 ⇒ 迁移后两台同跑,卷数据是导出时刻快照,源后续写入不回传(确认页明确提示尽快停用源)
- 目录检测/创建不限制路径前缀(仅要求绝对路径):项目级目录本就是独立绝对路径,限定在服务器目录下反而挡掉正当用法(理由记在 wiki 07 注入防护表)
- `external: true` 卷与宿主绝对路径挂载不搬运,只列 warnings 由用户自行准备
- 卷搬运依赖服务器有 tar 镜像或能拉 busybox;distroless/scratch 项目镜像不含 tar,不能作回退
- 归档搬运只处理单层目录;取消在卷/镜像边界生效(大文件传输中不即时中断)

---

# 第七批升级（v5.8.0）

**目标**:修掉回滚中心「点项目加载明细卡半天」的性能问题,并为发布归档加上类 GitHub
Release 的版本说明(标题 + 更新描述),在回滚中心点击归档即可查看/编辑。

**进度**:阶段一(明细批量化)✅ / 阶段二(归档版本说明)✅ —— 随 **v5.8.0** 发布

### 阶段一:明细加载批量化 ✅

**慢因(勘察实证)**:`rollback_project_detail` 对每个归档各发一条 `ls -1` 与一条
`cat manifest.json`(N+1 串行往返,每次都新开 SSH channel;50 归档 ≈ 100+ 次往返),
外加逐仓库 `docker images <repo>`;`rollback_list_releases`(部署页回滚模态数据源)
同款结构。对照 `rollback_scan_projects` 快是因为恒定 5 次命令。

- **新增纯函数 `releases_scan_cmd(dir, limit, compose_candidates, include_images)` + `parse_releases_dump`**:
  一条拼接命令批量取回全部归档的文件清单 / manifest.json / release-notes.json +
  compose 候选与全量 `docker images` 文本,标记行 `==RELEASE/MANIFEST/NOTES/COMPOSE/IMAGES`
  切分(与清理分析 `cleanup_cat_composes_cmd`/`split_compose_dump` 同形态,路径全单引号包裹,
  解析按标记归属、缺失段为空值不错位)。**二次优化(真机 meiguo 高延迟反馈)**:
  归档枚举改由远端 `find` 循环完成 —— 命令长度恒定,连"先 ls 再拼装"的往返也省掉
- **`rollback_project_detail` 单次往返**:远端 `find` 循环批量读全部归档清单/manifest/notes
  + compose 候选 + 全量 `docker images --no-trunc` 同命令带回,本地按仓库过滤日期标签
  (替代逐归档/逐仓库 N+1);所有 exec 包 `with_timeout`(此前循环内 4 处无超时兜底);
  顺带修复 `composeFile` 恒返回第一个候选的失真(现记录实际产出仓库的候选)
- **`rollback_list_releases` 同款改造**(行为不变只提速;单次往返,`head -n 100` 截断兜底,
  正常受 release_keep ≤50 约束)
- **`rollback_scan_projects` 单次往返**:原 5 次串行(compose find / docker ps / 逐容器
  inspect / releases find / docker ps -a)合并为一条组合命令,`==ROOTEXISTS` 标记保根目录
  存在性判定;compose working_dir 从 `docker ps -a` 的 **Labels 字段直读**(stopped 容器
  的 Labels 同样存在,"compose 被删但容器仍在"的项目依旧可见)
- 新增单测 9 个(命令拼装引号与段标记/多段切分按归档分组/未知标记隔离/notes 原子写形态/
  坏 JSON 容错/扫描段切分/根目录缺失/Labels 提取去重)

### 阶段二:归档版本说明(类 GitHub Release)✅

- **存储**:归档目录内 `release-notes.json`(`{title, body, updatedAt}`,与归档同生共死、
  跨机器可用);读取并入批量 cat(零额外往返);**原子写**复用 .env 编辑先例
  (base64 → `.ddtmp.$$(PID)` → `mv`,上限 64KB);**title/body 均空保存 = 删除文件**(清除)
- **新命令 `rollback_set_release_notes`**(camelCase):安全约束与 `rollback_delete_release`
  同款(dir 绝对路径 / ts 纯目录名 / 前缀校验防逃逸);`RollbackReleaseDetail` 增
  `noteTitle` / `noteBody` / `noteUpdatedAt` / `manifestImages` 字段(命令总数 92 → 93)
- **前端**:归档行可点(回滚/删除按钮 stopPropagation)→ 新模态
  `#release-detail-modal`(11 号;元信息 + 镜像清单 + 版本标题/说明表单);
  开/关照抄 config-io 四通道 + `modalFocusOpen/Close`;保存走 `setBtnBusy` 步进条 +
  `formFailLoud` 三通道,成功就地更新 `detailCache` 与列表行(ts 旁显示标题 +
  「已备注」徽章);明细加载加**会话序号守卫**(快速连点项目丢弃过期响应)

**验证**:`cargo test` 275 passed(基线 269 + 新增 6)/ 13 ignored;clippy 对新代码零警告;
`node --check` 全过;新模态 id 与 index.html 逐一核对;浏览器冒烟(06 页加载零 JS 错误、
模态存在)。真机验收(明细加载速度对比 + 说明编辑全流程)已确认 ✅(2026-09-16,真实服务器执行)。

### 遗留与取舍（记录在 wiki 07 已知限制）

- 版本说明存归档目录内 ⇒ 随归档删除(`rollback_delete_release`)一并消失;跨机迁移项目时
  归档搬运含该文件(migrate 的归档整体搬运语义),无需单独处理
- 批量拼接命令按归档数线性增长:`project_detail` 截断 50、`list_releases` 截断 100 兜底
  (正常受 release_keep ≤50 约束)
- 部署时暂不支持预填版本说明(说明在回滚中心补写);后续如需可在部署确认面板加可选输入

---

# 第八批升级（v5.8.1）

**目标**:更新体验优化 —— 「检查更新」发现新版本后自动弹出**更新确认模态**
(圆润卡片,用户指定样式例外),展示更新内容,一次确认即全自动下载安装重启。

### 阶段一:更新内容数据源 ✅

**缺口(勘察实证)**:主路径(302 重定向探测)的 `notes` 恒为空串 —— 正常情况下
用户检查更新后看不到任何更新内容(仅 API 回退路径有 body)。

- 新增 `fetch_release_notes_via_api(proxy, tag)` + 纯函数 `release_by_tag_url`:
  检测到新版本后经 `api.github.com/.../releases/tags/<tag>` 抓取该 Release 的
  body,复用 `LatestRelease` 反序列化与 `truncate_notes`(2000 字符截断);
  **仅 has_update 时调用一次**(手动检查频率极低),失败/限流静默降级为空 notes
  (前端「前往发布页」兜底),不影响版本检测本身
- `update_check` 命令签名与结构零变化;新增单测 1 个(URL 拼装)

### 阶段二:更新确认模态 ✅

- **新模态 `#update-confirm-modal`**(12 号模态;560px **圆润卡片** —— 用户指定的
  radius 例外,16px 圆角 + 取消 3px 顶缘墨线,注释声明仅此模态不为例先):
  版本行 + 更新内容(`.set-notes` 限高滚动;空 notes 兜底文案)+
  「自动更新 / 前往发布页 / 稍后再说」
- **触发**:设置中心点「检查更新」→ 有新版即自动弹模态(叠在设置模态上,
  modalFocus 栈式圈禁);行内保留「查看更新详情」入口
- **一次确认全自动**:点「自动更新」→ `setBtnBusy` 步进条 + 行内下载进度文案 →
  `update_download` 成功后**直接** `update_install`(移除旧流程的第二次
  「确认安装」步)→ 后端 `/S /R` 装完自动重启,`take_update_pending` 提示
  「已更新到 vX」;失败行内 fail(附手动运行路径)可重试
- **守卫**:下载/安装进行中三通道关闭统一 toast 拦截(deploy 模态 rbBusy 同模式);
  更新模态叠在上面时设置模态的关闭通道先转发给上层;下载/安装按钮改用
  `window.setBtnBusy`(替代旧的手动 disabled);删除旧「前往下载」死样式

**验证**:`cargo test` 279 passed(基线 269 + 第七批 9 + 本批 1);`node --check` 全过;
模态 id 与 index.html 核对;真实验证路径 = 用户装着 v5.8.0 检查更新 → 弹出模态显示
v5.8.1 更新内容 → 确认全自动更新重启。

### 阶段三:概览宿主机性能指标 + 监控模块审计 ✅

- **概览新增宿主机性能指标**:`manage_overview` 增 `cpu_percent` / `cpu_cores` /
  `mem_used` / `mem_total` / `mem_percent`(snake_case 展示串;采样不可用为空串 →
  前端「—」)。单次 exec 完成采样:`/proc/stat` 两次采样(间隔 1s)差分算 CPU 占用
  (busy = Δtotal - Δidle - Δiowait)、`/proc/meminfo` 取内存(MemAvailable 优先,
  老内核回退 free+buffers+cached)、`nproc` 取核心数;/proc 缺失的系统(macOS 等)
  优雅降级。前端概览格新增「CPU 占用(含核心数)」「内存 已用/总量(百分比)」两格
  (随自动刷新同步刷新)。新增纯函数单测 4 个(采样差分/解析与老内核回退/格式化/边界)
- **监控模块审计(结论:无错漏)**:逐项核对 manage_stats.rs(545 行)与 manage.js
  监控侧 —— ①generation 单会话 + 轮间 200ms 步进退出,重复 start 无缝接管;
  ②会话级 SSH 连接跨轮复用,单轮失败三分类(权限拒绝停止 / 命令失败继续 /
  传输失败重连),连续 3 轮连接失败熔断;③旧版 Docker `--format json` 降级模板;
  ④前端事件先订阅后 invoke、行差异更新、**切 Tab / 切服务器均停监控**
  (manage.js:219/330)、**跨服务器事件三重丢弃**(server_id 不匹配直接 return)、
  徽章带最后更新时间;⑤已知取舍(非 bug):「实时」= 轮询采样,`docker stats
  --no-stream` 自身有 ~1-2s 采样窗口,真实帧周期 ≈ 采集耗时 + 设定间隔
  (模块头注释已声明),这是无流式 API 可经 SSH 稳定消费场景下的标准做法

# 第九批升级（v5.8.2）

> **分阶段路线图**(用户批复的执行协议):阶段一 细节优化 → 阶段二 概览磁盘用量 →
> 阶段三 部署时预填版本说明 → 阶段四 结构治理(commands.rs/JS 拆分 + 补测试)→
> 阶段五 表单校验体系 → 阶段六 批量部署增强;候选池(容器级指标/部署前强制预览/
> 服务器探活通知/结构化错误码等)可任意批复节点插入。每阶段完成走完整验证链并
> **停下等用户批复**再进入下一阶段。以下为阶段一完成记录。

## 阶段一:设置中心日志入口 + errText 统一 + 文档计数同步 ✅

- **设置中心「打开日志文件夹」**:新命令 `open_logs_dir`(`config.rs`,第 94 个
  注册命令)—— 资源管理器打开 `app_dir()/logs`(tauri-plugin-log 的 app.log
  按日期轮转,lib.rs 初始化处同路径),目录不存在先 `create_dir_all`(首次运行
  尚未写过日志也能打开);explorer/open/xdg-open 按 `cfg(target_os)` 兜底,
  spawn 失败报错不 panic。设置中心「通用」组新增「日志文件 / LOGS」按钮行 +
  一行说明(此前设置中心无任何日志入口,排障需手动定位应用目录),走 AppBus
  invoke,成功 toast、失败 toast 附错误文案
- **errText 全站统一**:8 个页面脚本各内联一份等价实现(check/images/config-io/
  notify/servers/deploy/settings 的「空串兜底」版 + rollback 的「未知错误」版)
  上收为 app.js 唯一 `window.errText`,取**超集语义**:空值兜底「未知错误」、
  字符串原样、Error 取 `message`、其余转字符串 —— 消除 toast / 错误框空文案;
  app.js 头部功能清单与 wiki/03 公共层表同步登记;app.js pickPath 内联三元
  表达式一并改用 errText
- **文档与计数同步**(只改「现状声明」,不改历史批次记录):wiki/README 与
  wiki/07 测试计数 269 → 283(第七/八批加测未同步的存量失准,以 cargo test
  实际输出为准);wiki/README 命令计数 92 → 94(与 lib.rs 注册数对齐,此前
  README 92 / wiki/04 93 两处各差一档);wiki/04 `deploy_batch` 命令条目补
  「⚠️ 未接入(死代码,批量由前端队列完成)」标注,与 `deploy-batch` 事件
  条目(04:436)同口径
- **验证**:`cargo test` 显式确认 `test result: ok. 283 passed; 0 failed;
  13 ignored`(基线不降;本批无新增 Rust 测试——open_logs_dir 为 spawn 副作用
  不宜单测,errText 为 JS);`cargo clippy` 新代码零警告(存量 15 条均在
  ssh/stack/commands 等与本批无关处);9 个改动 JS 全过 `node --check`;
  界面改动(设置页按钮行)截图交 judge 验收

# 第十批升级（v5.9.0）

## 阶段二:远程管理概览新增宿主机磁盘用量 ✅

(分阶段路线图阶段二;部署最怕盘满——`docker load` 在 /var/lib/docker 所在
文件系统写满时失败,此前的概览只有 CPU/内存没有磁盘)

- **采样扩展**(`manage.rs`):`host_metrics_cmd` 单次 exec 追加 `==DISK` 段
  —— `df -kP / /var/lib/docker 2>/dev/null`(-P POSIX 格式,长设备名不折行;
  目录不存在时该行缺失,stdout 仍有根分区行)。解析沿用标记行切分:挂载点为
  `/` 的行归根分区(首行,同盘时 df 对两个参数输出两行 `/`),挂载点恰为
  `/var/lib/docker` 的行 = Docker 数据目录在**独立文件系统**时才单列
  (`DfSample { total_kb, used_kb, mount }`,挂载点含空格自第 6 列重新拼接,
  df 表头第 2 列非数字天然跳过)
- **payload 扩展**(`ManageOverview` 7 个新展示串字段,snake_case 延续
  batch 8 口径):`root_disk_used / root_disk_total / root_disk_percent` +
  `docker_disk_used / docker_disk_total / docker_disk_percent / docker_disk_mount`
  (采样不可用为空串;百分比 = used/total 四舍五入,与 mem_percent 同口径)
- **前端**(`manage.js` / `index.html` / `style.css`):不新增概览格(宽格
  span-4 整行,插普通格会留 3 格空洞)——在「磁盘占用」宽格内加第二行
  `.overview-sub`(标签升级为「磁盘占用 DISK USAGE」):`根分区 / x / y(p%)`,
  Docker 数据盘独立挂载时追加 `Docker 数据盘 <挂载点> x / y(p%)`,采样不可用
  整行隐藏(`.is-on` 类切换,display 切换无动效——数据刷新非状态迁移)
- **测试**:+2(独立挂载解析 / split_df_line 与 disk_percent 变体含表头跳过、
  含空格挂载点、残行、空盘),`test_parse_host_metrics` 同步补 ==DISK 段
- **已知取舍**:Docker 实际数据根(`docker info .DockerRootDir`,如自定义
  data-root 指到别处)未探测,当前按默认 /var/lib/docker 采样;后续如需可在此
  段追加一条 `docker info -f` 查询
- **验证**:`cargo test` 显式确认 `test result: ok. 285 passed; 0 failed;
  13 ignored`(+2);clippy 零新增警告(存量 15 条不变);`node --check` 过;
  截图交 judge 验收概览磁盘行。验证方式按用户要求由 computer-use 驱动真机
  改为**浏览器 + Tauri 桩**(假 `__TAURI__` 返回模拟 manage_overview 数据,
  index.html 副本注桩后本地静态服务渲染),后端 df 采样真机端到端由单测
  覆盖解析、实机数字已与 `df -h` 对照确认 ✅(2026-09-16)

# 第十一批升级（v5.10.0）

## 阶段三:部署时预填版本说明 ✅

(第七批遗留 UPGRADE-PLAN:777「后续如需可在部署确认面板加可选输入」;
补全「部署时写 → 回滚中心看」闭环)

- **链路核实结论**:`DeployRecord.release_dir`(整栈成功 = `<项目目录>/
  releases/<时间戳>` 完整路径,第四批起)已携带新归档标识,
  `rollback_set_release_notes` 参数(dir + ts + title + body)恰好可由它拆出
  ——**无需新增命令/payload 字段**;唯一障碍是原收尾顺序「emit deploy-done →
  通知(await,SMTP 时秒级)→ 落历史」,前端 done 后立即查历史存在竞态
- **后端最小改动**(`commands.rs` `finish_deploy_run`):把 `append_record`
  (与 webhook spawn)挪到 emit **之前**,通知/webhook 时序不变;
  头部契约注释与 wiki/04 deploy-done 条目同步改写。历史记录内容零变化,
  消费方只受益(前端 done 后刷新历史必见本次记录)
- **前端**(deploy.js / index.html / style.css):整栈选项区加可选
  「版本说明」textarea(`#deploy-release-notes`,maxlength 2000,
  `.deploy-release-notes` 覆盖 form-textarea 默认 mono 轨为 CJK 轨——
  内容是中文散文;**仅整栈**:单镜像无归档不提供,批量无逐台说明恒不写)。
  发起时快照 `st.pendingReleaseNotes { serverId, projectId, body }`;
  `handleDone` 成功(非回滚)后 `writeReleaseNotes`:get_history 取该
  服务器+项目最新成功整栈记录 → release_dir 拆 dir/ts → 补写
  (title 空),成功 toast 带归档 ts,失败仅 warn 警示(部署本身已成功,
  可回滚中心版本详情补写)。**失败/取消不写且清空快照前的输入留在
  原输入框**:断点续传成功后走同一 handleDone 入口补写;中途重启软件
  快照丢失则静默跳过。批量分支收尾显式清快照,防残留污染后续单发
- **已知取舍**:①说明输入在整栈选项区(随面板显隐),非独立确认步——
  与现有单页表单流程一致;②续传场景跨软件重启后说明丢失(断点是补救
  路径,不做跨重启持久化);③title 不在部署时填,归档标题仍由版本详情
  维护(与第七批「已备注」徽章按 title+body 判定的口径兼容:body 非空
  同样挂徽章)
- **验证**:`cargo test` 显式确认 `test result: ok. 285 passed; 0 failed;
  13 ignored`(finish 语义既有测试覆盖 emit 恰一次/历史/通知,顺序调整
  后全绿);clippy 零新增;`node --check` 过;浏览器桩渲染整栈选项区
  截图交 judge;真机端到端(部署一个整栈项目 → 回滚中心看归档说明)
  已确认 ✅(2026-09-16)

## 真机反馈修复 + 标题预填(用户实测 v5.10.0 后,同批追加)✅

用户真机实测发现两件事:

1. **版本详情保存恒败(第七批潜伏 bug,真机首曝)**:保存报
   `bash: .../<项目>/releases/<ts>/releases/<ts>/release-notes.json.ddtmp.N:
   No such file or directory` —— `RollbackReleaseDetail.dir` 存的是
   **归档完整路径**(`remote_join(releases_root, ts)`,5398 行),版本详情
   模态保存时误把它当**项目目录**传给 `rollback_set_release_notes`(命令
   内部还会再拼一层 `releases/<ts>`),路径翻倍必然落空。同命令的部署
   预填补写路径无此问题(用历史 release_dir 经 lastIndexOf 拆分,天然
   防御项目目录本身含 `/releases/` 的情况);删除归档/执行回滚不受影响
   (走 selectedDir)。**修复**:模态保存改用明细请求的项目目录
   `selectedDir`(rollback.js `openReleaseDetail`)。注意:此 bug 意味着
   第七批的版本说明保存从上线起在真实服务器上就不可用(当时验证未覆盖
   真机写入),本次一并根治
2. **部署时缺版本标题输入**:补「版本标题」input(`#deploy-release-title`,
   与说明同区,均可选);快照与补写带 title。**标题仅作展示备注,归档
   目录名/时间戳标识不变**;回滚中心列表恒为「时间戳 + 标题并排 +
   已备注徽章」(第三批起实现,标题从不覆盖时间戳,本次向用户确认该行为)
- **验证**:`node --check` 过;`cargo test` 285 passed 显式确认(后端零
  改动);浏览器桩截图(标题+说明双输入渲染)交 judge;路径翻倍修复已真机复测(保存版本标题/说明)✅(2026-09-16)

# 第十二批升级（v5.11.0,进行中）

## 阶段一:commands.rs 结构治理 + 零测试模块补测 ✅

- **commands.rs(10,545 行)按文件头既有分节拆为 `src/commands/` 11 文件**:
  `deploy`(单镜像+整栈管线,2057 行)/ `rollback`(一键回滚+独立回滚中心+
  版本说明+manifest,1470)/ `cleanup`(清理分析,1243)/ `migrate`(镜像迁移,
  475)/ `compose_sources`(项目源更新+compose 解析,494)/ `host_server`(宿主机
  检测+服务器操作,367)/ `resume`(断点续传,434)/ `batch`(批量部署死代码,
  292)/ `preview`(部署预览,281)/ `tests`(编排层 134 单测,2249);`mod.rs`
  (1261)保留跨域共享设施(DeployState、事件结构、emit/finish_deploy_run/
  webhook/CatchPanic、find_server/resolve_password、remote_join 等)并对各
  子模块 `pub use` —— **lib.rs 的 commands::xxx 路径零改动**。拆分为脚本
  机械切分(条目级区间 + 属性行回溯),可见性按编译器反馈最小放宽
  (pub(crate),仅跨域消费的 22 个符号 + resume 三结构体/RollbackScanDump
  的字段)
- **测试**:manage_logs.rs / manage_stats.rs 两个零测试模块补代号守卫
  (generation begin/finish/is_current,含「旧代号 finish 不得清新流」)+
  传输错误分类(is_transport_error 三传输两命令层样本)+ 代号严格递增,
  +5 → `cargo test` 显式确认 `test result: ok. 290 passed; 0 failed;
  13 ignored`(基线不降);`cargo check` 零警告,clippy 零新增(存量 15 条
  原样搬家)
- **JS 大文件拆分**(deploy.js ~3420 / manage.js ~3260 / servers.js ~3020):
  细案另行提交用户批复后实施(本批先落 Rust 侧)

## 阶段二:JS 大文件拆分 + 迁移模态存量 bug 修复 ✅

(细案经用户批复;原则:只沿原作者留好的缝拆「模态级/独立状态域」,
不动管线与共享 st,不盲拆)

- **拆分结果**(主文件 → 新文件,依赖经宿主尾部 `window.<Kit>` 桥接、
  入口经同名 var 别名转发,host 调用点零改动):
  - `deploy.js` 3420 → **2338** + `deploy-rollback.js` 620(一键回滚模态
    全族,st.rb* 状态域,window.DeployRollback)+ `deploy-migrate.js` 532
    (项目迁移模态,migState,window.DeployMigrate)
  - `manage.js` 3257 → **2013** + `manage-stacks.js` 1274(C 阶段独立状态域
    ——原 2004 行注释「不触碰上方 state 对象」就是现成的缝:Compose 栈/
    .env/实时监控/Exec 终端/日志跟随/离页清理,window.ManageStacks)
  - `servers.js` 3019 → **2537** + `servers-cleanup.js` 490(清理分析模态,
    window.ServersCleanup)
  - index.html script 顺序:4 个新文件排在各自宿主后(拆出文件只挂
    window.* 不做初始化,顺序宽松但保持可读)
- **拆分方法**:脚本先做双向依赖分析(区域→宿主顶层声明的正向外依赖
  逐个桥接;宿主→区域顶层函数的反向入口逐个别名),不猜不盲切
- **意外收获——修复迁移模态存量 bug**:`renderMigrateProjectModal` 的
  `body.insertBefore(标题, grid)` 在 grid 尚未 append 到 body 时调用,
  按 DOM 规范必抛 NotFoundError —— **「迁移项目…」模态自该行引入起
  打开即报错**(与拆分无关,HEAD 版本即如此,本次浏览器验证时暴露)。
  修复:标题持引用、待 grid 挂载后紧贴其前插入
- **验证**:7 个 JS 全过 `node --check`;id 存在性核对(拆出文件引用的
  动态 id 均由同文件自建,静态缺失为已知审计误报);浏览器桩渲染四个
  界面(回滚模态/迁移模态/05 栈 tab/03 清理模态)截图交 judge **4/4
  pass**;`cargo test` 290 passed 不变(纯 JS/HTML 改动)







# 第十三批升级（v5.12.0）— 阶段五:表单校验体系

> 细案经用户批复(2026-09-12),批复口径:六项决策全部采纳 —— ①必填类错误在
> 首次提交后才参与失焦提示;②输入时清除本字段已有错误(只清不加);③引入
> `<form novalidate>` 五处;④新增规则候选全部采纳;⑤清理模态扫描起点 Enter;
> ⑥`setFieldError` 锚点修正带来的排版变化。用户同时授权「一次性完成所有任务,
> 不再逐阶段批复」。

## 一、三个统一助手(全部落 app.js,零新文件)

- **`window.bindFieldValidation(formRoot, rules)`** —— 失焦校验接线器。**一份
  规则清单同时供失焦校验与提交期整体校验**(单一事实来源),避免「失焦说合法、
  提交说非法」两套判定漂移;此前 `saveServer`/`saveProject` 的校验分支与
  `collectProjectExtras` 的逐字段判定是两份独立实现。
  - 规则描述符:`{ id|selector, required, test(v), when(), message,
    formatMessage(v), label, gate, blocking }`
  - **呈现闸门**:必填类错误只在「表单已提交过一次」或「该字段被填过又清空」
    时提示(否则 Tab 扫过一张空表满屏标红);格式类错误恒提示
  - `when()` 为假 → 清除该字段错误(切到密码认证时清掉私钥路径红字)
  - 失焦失败**只做字段级提示**:不 toast、不滚动、不写顶部聚合框
  - `validate()` → `{ firstBad, missing, formats, blockingErrors }`,供调用方
    聚焦首错字段 + 拼既有句式(「请填写:X、Y」/ 格式错误原文)
  - `blocking: false` = 只提示不阻断(迁移的归档数量越界;后端本就会夹取归一)
  - **逐字段求错(非逐规则)**:一个字段可挂多条规则(如远程部署目录:必填 +
    绝对路径),按规则逐条写会出现「后一条通过的规则用 `setFieldError(null)`
    抹掉前一条刚贴的错误」—— 这是本地校验器抓出的实现缺陷,已改为按字段聚合
- **`window.bindFormEnter(formRoot, onEnter)`** —— Enter 提交接线。两条纪律:
  ①只认单行文本类 `input`(textarea 保留换行、select/checkbox 不响应);
  ②判 `e.isComposing || e.keyCode === 229` —— **中文输入法按 Enter 是上屏候选词**,
  全站原有 8 处手写 Enter(manage 打标签/创建卷/创建网络/连接容器/自定义间隔、
  rollback 扫描起点、manage-stacks 终端)均缺这条判断
- **`window.beginForm(container, labelId)`** —— 表单语义脚手架:清空容器并放入
  `<form novalidate aria-labelledby=...>`,返回 `{ form, onSubmit, submit }`。
  **`submit` 恒 `preventDefault`:这是必须的保险** —— HTML 规范里「可阻塞隐式
  提交的字段恰好一个」时 Enter 会自行提交表单,单镜像回滚视图恰好只有
  `rb-target-input` 一个文本输入,不接管会导致 WebView 导航(桌面应用里等于白屏)

## 二、`setFieldError` 锚点修正(存量缺陷)

原实现用 `control.parentNode` 作错误锚点。路径类字段(私钥路径 `srvf-key-path`、
导入 compose 路径 `prjf-import-path`)的父节点是 `.input-btn-row`(`display:flex`),
而私钥路径**确实会报字段错误** —— 原实现把「私钥认证需填写私钥路径」塞进 nowrap
的按钮行里,与输入框 + 「浏览」按钮抢宽度。改为 `closest('.form-row') || parentNode`,
并把错误**插在行内 `form-hint` 之前**(错误先于帮助文案被读到,也不把长说明顶在
错误与控件之间)。

## 三、规则清单(表单 × 规则)

- **服务器表单**:名称/主机/用户名/远程部署目录必填;端口 1-65535;
  **远程部署目录 `/` 开头绝对路径(新增)**;私钥路径(Key 认证时必填);
  登录密码(密码认证且无已存密文时必填)
- **项目表单**:名称必填;compose 相对路径(无导入路径时必填);健康等待
  0-86400;webhook `http(s)://` 前缀;项目级远程部署目录绝对路径;归档保留
  0-50;默认服务器选中项仍存在。`collectProjectExtras` 退化为**纯取值**
  (校验交给规则清单),文件映射行级校验保留独立实现(错误锚点在 `<td>` 内、
  文案带行号,与字段级规则不同形状)
- **通知模态(新增前端侧提示)**:启用邮件时 SMTP 主机/发件人/收件人必填;
  端口 1-65535 或留空;发件人含 `@`。此前这些只由后端校验(主机/收件人在
  `validate_email_save`,发件人在发送时 `build_email`),「保存通过、发送才报」
- **迁移模态(新增)**:目标部署目录绝对路径;归档数量 0-20(**非阻断**提示,
  后端仍按边界夹取)
- **单镜像回滚**:目标引用须为完整镜像引用(`repo:tag`,此前只在点「执行回滚」
  时 toast)
- **清理模态(新增)**:扫描起点 `/` 开头绝对路径
- **部署页(新增字段级提示)**:三个下拉缺选时贴 `请选择…` 到对应控件并把焦点
  给第一个(此前只有 toast,用户滚到页面底部点「开始部署」看不出哪格漏选);
  选中即清除该错误

## 四、Enter 语义(不越过二次确认)

| 位置 | Enter 目标 | 说明 |
|---|---|---|
| 服务器 / 项目表单 | 保存 | 非破坏 |
| 通知模态 | 保存配置 | 不绑测试桌面/测试邮件 |
| 迁移模态 | 开始预检 | 只读;「确认迁移」不绑 |
| 单镜像回滚 | 执行回滚按钮 → 打开二次确认视图 | 确认视图内不注册 Enter |
| 清理模态 | 重新扫描 | 只读 |
| **部署页 / 批量模态** | **不绑** | 前者文本输入只有可选的版本标题/说明,Enter 触发「开始部署」风险不对等;后者是多台部署 |

## 五、顺带修复:第十二批拆分的 `window.DeployKit` 双重赋值覆盖(存量 bug)

deploy.js 末尾写了**两个** `window.DeployKit = {...}` 字面量(回滚一个、迁移一个),
后者整体覆盖前者 —— 拆出的 `deploy-rollback.js` 在模块顶层取到的
`DATE_TAG_RE` / `appendLogLine` / `refreshHistory` / `renderHistory` /
`handleDone` / `LOG_MAX_LINES` 全是 `undefined`,**单镜像回滚模态一打开即抛
「Cannot read properties of undefined (reading 'test')」**(`splitImageRef` 里
`DATE_TAG_RE.test(...)`)。合并为单一对象(12 键,覆盖两个消费方的全部 `K.*`),
并新增本地守护脚本 `verify/bridge-integrity.js` 检查「资产赋值次数 ≠ 1」与「消费键未被
提供」;另核查 ManageKit(12 键)/ServersKit(4 键)均无此问题。

## 六、验证

- **本地校验器 `verify/form-validation.js`**(自建最小 DOM shim + 加载真实 app.js,
  54 项断言全过):锚点/aria 关联、闸门(空表不标红 / 填过又清空才提示)、
  `when()` 切换清红字、`blocking:false` 不阻断、一字段多规则取第一条、
  Enter(IME/textarea/checkbox/disabled/readOnly/非 Enter 键)、`beginForm`
  的 submit 拦截与 `novalidate`/`aria-labelledby`、`clear()` 复位
- **浏览器 + Tauri 桩**(`ui/_tauri-stub.js` + `_judge-preview.html` +
  `http.server 8799`,用完已删桩停服):服务器表单(空表失焦无红 → 端口越界即
  提示 → 提交出 5 处字段错误 + 缺项摘要 → 切密码认证红字迁移)、Enter 三态
  (非法拦截不 invoke / 合法真正发起 `save_config_cmd` + `test_server` /
  IME 与 textarea 均不触发)、通知模态(条件必填、端口、发件人 `@`、保存路径)、
  迁移模态(结构:输入区在 form 内、计划区与日志区在外;Enter 只走
  `migrate_project_preview`)、回滚模态(**隐式提交被拦、页面未导航**;
  Enter → 确认视图;确认视图内 Enter 无 invoke)、清理模态(Enter →
  `cleanup_preview`)。截图 4 张交 judge **4/4 pass**(overall pass)
- `cargo test` 显式确认 `test result: ok. 290 passed; 0 failed; 13 ignored`
  (纯 JS/CSS 改动,基线不变);`cargo clippy --all-targets` 零新增(15 条存量
  全在 ssh/stack/commands 等本批未触碰的文件);8 个改动 JS 全过 `node --check`
- 命令/事件计数不变(94 / 12,本批未新增命令),wiki/04 无需改动

## 七、文档与版本

- wiki/07:限制 57/58/59 三条**改写为已实现**(失焦校验接入 / form 语义引入 /
  required 属性为何仍不用)+ 新增 60(Enter 边界)、61(呈现闸门)
- wiki/03:表单体系小节新增「失焦校验 / Enter 提交 / 表单语义」三段;
  JS 模块与全局约定新增**桥接纪律**(单次赋值,含 DeployKit 踩坑记录)
- wiki/README + wiki/01 + wiki/05:版本 5.12.0;README 批次概要补第十三批
- 三处版本号:`tauri.conf.json` / `Cargo.toml` / wiki(5.11.0 → 5.12.0)

# 第十四批升级（v5.13.0）— 阶段六:批量部署增强

> 细案经用户批复(2026-09-12):**A 单台续传 + 一键批量都要**;**B 停止时不自动续传**;
> C/D 一并做。用户延续第十三批授权「一次性完成所有任务」。

## 一、开工前核实:文档记载与实现相反(本批最重要的更正)

ROADMAP 阶段六的待核实疑点是「批量走同一 deploy/deploy_stack 命令,断点可能已由
后端落盘 —— 疑点在『前端失败后没给续传入口』而非后端」。核实结论**比预期更复杂**:

- **批量确实落断点**:前端批量队列走的是普通 `deploy` / `deploy_stack`
  (deploy.js),而后端这两个命令**恒用 `DeployEmitOpts::single()`**(`checkpoint: true`)。
  `checkpoint: false` 只属于 `DeployEmitOpts::batch()`,其**唯一调用点在
  `commands/batch.rs`(死代码 `deploy_batch`,无任何前端调用点)**。且
  `DeployRequest` 结构体没有 checkpoint 字段,前端从协议上无法关闭。
- **但 wiki/06、wiki/07(限制 21/40/41)与三处代码注释都写着「批量不落断点」**
  —— 这个错误记载正是「批量失败后没有续传入口」长期存在的根因:一直以为做不了,
  实际断点早就在磁盘上。
- **顺带发现独立缺陷**:断点表超上限(10 条)时 `trim_checkpoints` **只删表项、
  不删对应的本地 tar**(config.rs)。批量 N 台各写一条断点,超 10 台时最旧的被静默
  丢弃,其 tar 永久留在临时盘 —— 用户看不到、也无法经「放弃断点」回收。

## 二、A:批量失败/取消后的续传入口(单台 + 一键批量)

- **`st.batch.resumable`**:失败/被取消的台登记为待续传候选(含 serverId/projectId/
  serverName/mode)。**不登记**「已停止批量」而从未启动的余台(没跑过,无断点)。
  只登记候选,**是否真可续传由 `deploy_resume_status` 点击时现查**(断点可能已被
  清理或被同键新部署覆盖)。
- **两个入口**:行内「续传此台」(单台)与头部「续传未完成服务器(N 台)」(一键批量,
  仅在批量结束且有待续传台时出现)。单台续传时**继承其余待续传台**(`startResumeBatch`
  的 carried 逻辑),否则面板被这次单项队列覆盖,别的失败台按钮就消失了。
- **复用而非另起循环**:待续传台组装成一条**普通批量队列**(每项带 `resumeKey`),
  `runBatchNext` 增加 resumeKey 分支直接调 `deploy_resume_start`(按 key 复用与单发
  完全相同的管线,事件/历史/通知收尾一致),串行推进、deferred 等待、面板渲染全部复用。
- 查询阶段并发(纯读),执行阶段串行(与部署同口径,避免争抢本机 Docker/临时盘)。

## 三、B:「停止批量」可在步骤边界即时中止当前台(不自动续传)

原实现只能停在台边界(等当前台整台跑完),理由写在代码注释里:「批量不落断点,
半途取消会留下无法续传的半成品」—— **该前提经核实不成立**(见上)。现改为:当前台
部署中时点「停止批量」→ 发 `cancel_deploy`(步骤边界生效),按钮文案变
「停止中(当前台将在步骤边界中止)」,中止的台进入待续传列表。
**不自动续传**:停止是用户的显式意图,接着自动开跑会违背它。

## 四、C:纠正错误文档(四处代码注释 + wiki 两篇)

- `ui/deploy.js`(runBatchNext 停止分支注释)、`commands/batch.rs`(死代码头部)、
  `commands/mod.rs`(`DeployEmitOpts::checkpoint` 与 `batch()` 的文档)——
  全部补上「该『批量不落断点』只描述死代码实现;线上批量恒 checkpoint=true」
- `wiki/06`:三种执行形态那句、断点写入/清除时机那句、「停止」条目全部改写,
  新增「续传」条目
- `wiki/07`:限制 21(改写为「批量落断点且支持逐台续传」)、40、41(status/取舍列更新)

## 五、D:断点裁剪时回收本地临时 tar

- `config::trim_checkpoints` 改为返回**被移除的条目**;`save_checkpoint` 返回
  `Vec<ResumeCheckpoint>`(分层:config 层不解析 `artifacts` —— 那是 commands 层
  私有约定,见 wiki/07 限制 40)
- `commands::checkpoint_save` 拿返回值按 `resume_local_tars` 口径清理文件 + 记日志
- **新增单测** `test_checkpoint_trim_cleans_dropped_local_tars`:建满 10 条带真实
  tar 的断点 → 再写一条 → 断言只裁最旧 1 条、其 tar 路径可按口径解析出来、
  未被裁的与新写入的 tar 不受影响
- `MAX_CHECKPOINTS` 提升为 `pub(crate)` 供测试引用

## 六、顺带修复(浏览器桩暴露的存量 bug):批量部署在第二台永久卡住

**严重 bug,在 HEAD 上复现**:`st.batch.deferred` 被存成对象
`{ promise, resolve }`,但三个消费方里有两个把它**当函数调用**
(`handleDone` 的 `doneDeferred(p)`、两处 invoke 失败兜底),抛出的

    TypeError: doneDeferred is not a function

被事件监听器静默吞掉 → **`batchItemResult` 永不执行 → 批量部署第一台结束就卡死**
(面板停在「部署中」,界面上没有任何错误提示)。本批在浏览器桩里用
`git show HEAD:ui/deploy.js` 做对照页复现确认(与第十四批改动无关)。

修复:新增 `makeDeferred()` —— 返回**可调用且带 `.promise`** 的把手,从形状上
消灭这处错配(三个消费形态同时成立);内部 `settled` 标志防 invoke 失败与
deploy-done 双触发。

## 七、验证

- **浏览器 + Tauri 桩**(可编排桩:`window.__PLAN.deployResults` 指定每台结果、
  `__CHECKPOINTS` 注入断点、`holdDeploy` 让部署停住不结账):
  - 四台混合结果(1 成功/1 失败/1 成功/1 取消)→ 面板 2 成功/1 失败/1 跳过、
    resumable = 失败+取消两台、两个续传入口同时出现
  - **单台续传**:点「测试机」行 → 只查该台断点 → 续传成功 → 其余待续传台被
    继承(carried)、按钮仍在
  - **一键批量续传**:两点全部串行续完,队列排空后面板不再有续传按钮
  - **断点已被清理的边界**:点击后提示且不进入续传(现查机制生效)
  - **停止批量**:部署中点停止 → 发 `cancel_deploy`、按钮转「停止中(…边界中止)」、
    中断台登记为可续传、余台标跳过并收尾
  - 全程 `__LISTENER_ERRORS` 为空(不再有被吞掉的回调异常)
- **视觉验收**:judge 首轮打回一处真实缺陷 —— 失败行的徽章被 `space-between`
  推到行中(「成功行徽章在右、失败行徽章在中」跨行错列)。修复:`.batch-row` 去掉
  `space-between` 改三列布局(名称占满 + 徽章定宽 64px + **动作槽定宽 96px**);
  首轮修复后按钮换行,加宽动作槽并 `white-space: nowrap`。复审 **pass**
  (几何验证:四行徽章左缘全部 1029px、动作槽统一 96px、按钮单行 24px)
- **顺带修复用户反馈的 UI bug**(回滚中心「已备注」徽章拉满整行):父容器
  `.rollback-release-info` 是 `flex-direction: column`,`align-items` 默认 `stretch`,
  徽章被横向拉满。修法 `.rollback-release-info > .badge { flex: none;
  align-self: flex-start }` —— **必须用后代选择器**:`window.fillBadge` 内部
  `node.className = 'badge ' + kind` 会整体重写类名,调用方给徽章预设的类
  (`rollback-release-note-badge`)在运行时已被抹掉,按自定义类写 CSS 不会生效
  (首版实测 `alignSelf: auto`、徽章 419px 仍拉满;改后代选择器后 50px/419px)。
  顺带核查全站 34 处 fillBadge 的容器方向,其余均为 row flex 不受影响
- `cargo test` 显式确认 `test result: ok. 291 passed; 0 failed; 13 ignored`
  (基线 290 + 本批新增断点裁剪单测 1);`cargo clippy --all-targets` 与基线
  **逐条比对警告位置完全一致**(仅两处行号因插入测试而位移),零新增;
  全 JS 过 `node --check`;`verify/form-validation.js` 54 断言与
  `verify/bridge-integrity.js` 均通过
- 命令/事件计数不变(94 / 12,本批未新增命令),wiki/04 无需改动

# 第十四批补丁（v5.13.1）— 修复「未能获取更新说明」

## 缺陷

用户从 v5.8.1 升级到 v5.13.0 后,检查更新弹窗显示「未能获取更新说明,可前往发布页
查看本次更新内容」。**每次都会发生,非偶发**。

## 根因

`update_check` 主路径(302 重定向探测)拿到 tag 后,把 **剥了 `v` 前缀的展示版本号**
`info.latest`(`"5.13.0"`)传给了按 tag 查 Release 的抓取函数:

```rust
info.notes = fetch_release_notes_via_api(proxy.as_deref(), &info.latest).await;
//                                                 ^^^^^^^^^^^^^^^^^ 剥过前缀
```

而 GitHub 的 `releases/tags/{tag}` 端点要求**真实 tag 名**(`v5.13.0`),于是请求
恒为 `/releases/tags/5.13.0` → **404**。

抓取函数当时四个失败分支(客户端构建 / 请求失败 / 非 2xx / JSON 解析)**全部
`return String::new()` 且无任何日志**,所以 404 被静默吞掉,前端只看到空 notes 的
兜底文案。

**自第八批(v5.8.1)引入,跨版本升级时才暴露** —— 用户此前从未真正跨版本升级过,
而缺陷是恒定的(不是偶发),所以「第一次遇到就是你这次」。

## 修复

1. 主路径探测返回 `RedirectProbe { info, tag }`,**抓取改用 `probe.tag`(真实 tag,
   带 `v` 前缀)**;`info.latest` 保持剥离语义不变(前端展示契约不动)
2. `fetch_release_notes_via_api` **四个失败分支全部补 `log::warn!`**(带 URL 与
   HTTP 状态码)—— 无日志是这个缺陷藏 6 个版本的主因
3. 真机测试 `test_real_redirect_latest_tag` 强化:
   - 新增 `probe.tag` 必须等于 `gh api` 权威 tag(带前缀)的断言
   - 新增**对照断言**:用真实 tag 抓到非空说明 + 用剥前缀的值拿到空
     (这对事实即回归保护)
   - 移除原先「notes 必须非空」的误断言:测试二进制的 `CARGO_PKG_VERSION` 就是
     当前源码版本(= 线上最新版),`has_update` 恒为 false,抓取分支本就不执行 ——
     该断言失败与代码无关(已在本批实施中踩到并修正)
4. 纯单测 `test_notes_fetch_uses_real_tag_not_stripped_version` 固化端点契约认知

## 验证

- `cargo test` 显式确认 `test result: ok. 292 passed; 0 failed; 13 ignored`
  (基线 291 + 新增纯单测 1)
- `cargo test --lib update::tests::test_real_redirect_latest_tag -- --ignored --nocapture`
  **实测通过**(走真实代理 127.0.0.1:12450 与真实 API):
  `tag=v5.13.0` → 抓到 926 字说明;`tag=5.13.0` → 空(404)
- `cargo clippy --all-targets` 与基线逐条比对警告位置完全一致,零新增
- 另核实:`reqwest::Proxy::all("127.0.0.1:12450")`(设置里代理串无 scheme)
  **返回 Ok**,不是怀疑的代理解析问题 —— 诊断阶段排除了这个方向

## 文档

wiki/07 新增限制 63(记录端点契约、静默降级的取舍与「必须记日志」的教训)。

# 第十五批升级（v5.14.0）— 托盘 tooltip 动态态

> 用户指定「先完成托盘 tooltip 动态态,然后直接推送发布」(跳过细案批复;
> 项的来源是候选池,用户在解释「结构化错误码 / 托盘 tooltip」含义后选定)。

## 背景

应用支持「关闭到托盘」(settings.close_to_tray)与部署完成自动重启,窗口隐藏时
**托盘是用户唯一能看到的界面**。原 tooltip 是编译期写死的 `"DockerDeploy SSH"`
(lib.rs 的 TrayIconBuilder),窗口一关就完全看不到部署进度/成败(wiki/07 限制 19)。

## 实现:新模块 src-tauri/src/tray_status.rs(约 330 行,含 10 单测)

- **状态由后端自行维护,不依赖前端上报** —— 窗口隐藏、前端卡死时 tooltip 仍准确。
  这是本批最关键的设计决策:若走「前端 invoke 上报」路线,窗口隐藏后事件循环
  照常跑,但前端任何卡顿都会让 tooltip 撒谎。
- `tooltip_text(&Status)` **纯函数**生成文本(状态组合全部单测覆盖),全局
  `Mutex<Option<Status>>` 持状态,`set_*` 接口改写后锁内算出文本、
  `tray.set_tooltip()` 写入(托盘未就绪时状态照记、静默跳过)。
- **优先级:部署中 > 回滚执行中 > 监控中 > 空闲**;空闲时附「上次部署成功/失败/
  已取消」终态后缀,新部署/回滚开始即清(避免「部署中 … 上次失败」的自相矛盾)。
- 文案形态:
  - `DockerDeploy SSH`(空闲,无终态)
  - `DockerDeploy SSH — 部署中 生产服务器 / 我的应用(步骤 3/5)`
  - `DockerDeploy SSH — 整栈部署中 …` / `DockerDeploy SSH — 批量 2/4 · 部署中 …`
  - `DockerDeploy SSH — 回滚执行中(期间服务会短暂重启)`
  - `DockerDeploy SSH — 监控中 生产服务器`
  - `DockerDeploy SSH — 上次部署失败`

## 挂钩点(全部在后端管线,零前端改动)

| 钩子 | 位置 | 说明 |
|---|---|---|
| 部署开始 | `run_deploy_steps` / `run_deploy_stack_steps` 步骤 0 | 解析出 server/project 后置「部署中」+ 目标串;批量前缀取事件上下文 `log_prefix`(死代码批量专用,线上批量每台独立单发自然逐台刷新) |
| 步骤更新 | `emit_progress` | **刻意放在批量抑制之前** —— 批量的单台进度虽不发事件,但 tooltip 最需要它;回滚不走此函数(无步骤污染) |
| 部署收尾 | `finish_deploy_run` | 清运行态 + 记终态(成功/失败/取消,取消按 CANCELLED_MSG 判定) |
| 回滚开始/收尾 | `finish_rollback` | 进入即置「回滚执行中」,CatchPanic 后无论成败清态并记终态 |
| 监控开始 | `manage_stats_start` | 查配置取服务器名(查不到回退 id),置「监控中」 |
| 监控停止 | `manage_stats_stop` | 清监控态 |
| 监控熔断 | `connect_failure_limit_reached` | 连续 3 轮连接失败自动退出时也清态 —— 不清的话前端不在线时托盘永远挂着「监控中」 |
| 托盘就绪 | lib.rs 建完托盘 | `mark_ready`:置就绪标志并用当前状态刷新一次 |

## 已知边界

- tooltip 文本是系统绘制,**无自动化断言手段**;形态由 `tooltip_text` 的 10 个
  纯函数单测覆盖,真机悬停效果已确认 ✅(2026-09-16;托盘悬停 1-2 秒出现)
- 批量部署逐台是独立单发任务,两台之间 tooltip 会瞬间闪现「上次部署成功/失败」
  再回到「部署中」—— 状态始终准确,仅视觉上有一次刷新(间隔 < 1s)
- 回滚运行中文案刻意不带步骤(回滚管线无 emit_progress 挂点,步骤号语义不同)

## 验证

- `cargo test` 显式确认 `test result: ok. 302 passed; 0 failed; 13 ignored`
  (基线 292 + tray_status 10 个纯函数单测:空闲/部署/整栈/批量前缀/监控/
  优先级互斥/终态后缀/运行中不显终态/无步骤号/回滚)
- `cargo clippy --all-targets` 与基线一致(10/15),零新增;可见性统一 pub(crate)
  消除 private_interfaces 警告
- 真机冒烟:`cargo build` + 启动 app.exe 确认进程存活与正常退出(tooltip 实际
  悬停效果已确认 ✅ 2026-09-16)

## 第十五批补丁(v5.14.1)— 修复 05 远程管理页监听全断(拆分丢 `$` 助手)

> 用户反馈:远程管理「只有容器还在线能看,其余基本都不行了,监控点击开始没有反应」。

### 根因

第十二批 JS 大拆分(6cee803)把 manage.js 拆出 manage-stacks.js 时,**宿主 IIFE 局部
助手 `$`(`var $ = id => document.getElementById(id)`)既没随迁、也没进 ManageKit 桥**。
manage-stacks.js 里 40+ 处裸 `$(...)` 全是自由标识符,`bindEventsC` 在 DOMContentLoaded
一触发就抛 `ReferenceError: $ is not defined` —— **05 页栈/监控/终端/日志跟随的按钮
监听全部没注册上**:点击无反应、invoke 不发出、后端日志零记录。

潜伏三个版本(v5.11.0 → v5.14.0)的原因:

- 同文件裸引用的 `toast` / `fillBadge` 恰好是 `window` 全局(app.js 挂载),strict 模式
  下可解析,**全局/局部的静默差异让静态扫查看不出差别**;
- `node --check` 只查语法,运行时 ReferenceError 查不出;
- 浏览器桩截图验证的是静态界面,没点过监控按钮。

「容器/镜像/卷/网络还能用」与「栈/监控/终端/日志跟随全断」的边界,精确等于
manage.js 域与 manage-stacks.js 域的拆分边界 —— 症状与根因完全吻合。

### 排查中排除的假设

v5.14.0 托盘挂钩(`tray.set_tooltip`)在时间上最可疑,已完整读链排除:
tauri 的 `set_tooltip` = `run_item_main_thread!`(投递主线程 + `rx.recv()` 同步等待),
主线程事件循环常驻时毫秒级返回;仅 Explorer 挂起才会阻塞 tokio 线程,非本次根因,
代码未改动(风险已知、可接受)。

### 修复与守护

- **修复(4 行)**:manage-stacks.js IIFE 顶部补 `var $ = function (id) { return
  document.getElementById(id); };`(附缺陷说明注释)。
- **新回归守护 `verify/scope-integrity.js`**:按 index.html 真实顺序加载全部 15 个
  脚本并**触发 DOMContentLoaded 回调**,专抓「只在初始化回调里第一次求值」的自由
  标识符断裂;另含哨兵断言(监控/栈按钮接线回调必须存在)。与 bridge-integrity
  互补:桥查 `K.*` 显式桥接,本脚本查隐式作用域。
- TDD 流程:先跑新脚本看红(精确报出 `bindEventsC` 的 `$ is not defined`),
  修复后转绿;行为级验证确认修复后点击「开始监控」真实发出
  `manage_stats_start` invoke。
- 文档:verify/README 补第三节;wiki/03 桥接纪律旁补「作用域纪律」段。

### 验证

- `verify/scope-integrity.js` / `verify/bridge-integrity.js` / `verify/form-validation.js`
  (54 断言)全 PASS;`cargo test` 显式确认 `test result: ok. 302 passed; 0 failed;
  13 ignored`(与 v5.14.0 基线一致,纯前端修复)。

# 第十六批升级(v5.15.0)— 结构化错误码

> 候选池项,用户选定「先搞定结构化错误码」并要求**低耦合、后期增减好维护**;
> 首批范围批复:一次做全量错误类(12 类)。

## 背景

全后端错误通道统一是 `Result<T, String>`,但流程判定长期依赖**中文字面子串匹配**:

- 后端 `e == CANCELLED_MSG`(部署/回滚收尾判定 cancel 事件,4 处);
- `is_transport_error` 匹配 4 个中文短语区分传输层/命令层(决定监控重连,wiki/07 限制 13);
- 前端 `message === '部署已取消'` 逐字匹配(部署横幅/批量循环/历史徽章,7 处)。

文案匹配的脆性:改文案即断判定、无法国际化、判定语义不可见。

## 设计:`[dderr:*]` 前缀旁路(低耦合核心)

**不改 94 个命令签名与事件结构**,错误串仍是一个 String,头部带
`\u001f[dderr:<code>]` 标记(US 控制字符开头:UI 不渲染、不与用户数据撞车、
老版本前端收到也肉眼无感):

```
errors::tagged(ErrCode::Cancelled, "部署已取消")
  => "\u001f[dderr:canceled]部署已取消"
errors::code_of(s) => Some(ErrCode::Cancelled)  // 判定
errors::strip(s)   => "部署已取消"               // 展示
```

- **渐进采纳**:未挂码的旧式错误 `code_of` 得 None,行为与旧版一致;
- **判定只认码不认文案**;无码/未知码一律按无码降级(前后端版本错配安全,
  单测锁定);
- **加类**:枚举加变体 + `as_str`/`from_str_raw` 各一行;**删类**:编译器
  穷尽匹配逐点报出使用处 —— 增减只动 errors.rs 与使用点,契约(码名)不变;
- **文案匹配仅存留于远端工具原样输出解析**(docker permission denied /
  pull 401)—— 外部工具输出无法挂码,是合法存留区(文档明示)。

## 12 类错误码(canceled/transport/auth/perm_denied/timeout/network/
protocol/config/parse/fs/input/internal)

码表与来源见 wiki/04「全局错误约定」。首批实际挂码:cancel 链路、ssh.rs
全域(认证/传输/超时/Fs/Protocol)、with_timeout、exec_json_list、stats
分类;network/internal 预留(枚举已含,使用点后续按需接入)。

## 改造清单

### 后端
- `errors.rs`(新,约 230 行含 7 单测):tagged/code_of/strip/cancelled/perm_denied
- 取消链路:mod.rs(deploy_notify_text 判码、finish 判码、3 生产点)、
  deploy.rs(hook_failure_result 透传+剥码包装、pull map_err、1 生产点)、
  rollback.rs(notify_text 判码、finish 判码)、migrate.rs/migrate_project.rs(4 生产点)
- ssh.rs:connect 认证失败/主机密钥变更→auth、密钥加载→auth/fs、
  通道/PTY/SFTP 通道→transport、文件读写→fs、远端建目录退出码→protocol
- manage.rs:`with_timeout`→timeout、`exec_json_list`→protocol/transport/
  perm_denied;manage_stacks.rs 权限兜底→perm_denied
- manage_stats.rs:`StatsPayload` 增 `errorCode`(camelCase 可选);
  exec_stats_collect 生产点挂码(timeout/transport/protocol/perm_denied);
  `is_transport_error` 改按 transport/timeout 码判定;熔断 payload 挂码
- `DeployDone` 增 `errorCode`(camelCase 可选;mod.rs/rollback.rs 4 构造点)

### 前端
- app.js:`parseErrCode`/`errStripCode` 全局助手;**`errText` 自动剥码**
  (68 处既有调用零改动获得码安全展示)
- deploy.js:部署横幅/批量续传收尾/批量单发收尾/历史徽章取消判定改
  「码优先 + 文案回退」(回退兼容旧历史记录);批量待续传登记改按 state
  (skipped 只在取消时产生);契约注释更新
- manage-stacks.js:监控错误 banner/toast 与终端会话结束原因剥码展示
  (分类判定看 payload.errorCode,现有 stopped 机制不变)

## 已知边界

- 历史 deployments.json 里的旧记录是无码纯文案:历史徽章判定走文案回退,
  新记录带码 —— 新旧并存,无需迁移
- `CANCELLED_MSG` 常量保留(文案本体),但**判定不再引用它**
- update.rs 的 reqwest 错误未挂 network 码(错误已经 classify_http_error
  分好类,用户可见性无差);挂码属锦上添花,后续需要时接入

## 验证

- `cargo test` 显式确认 `test result: ok. 308 passed; 0 failed; 13 ignored`
  (基线 302 + errors 6;is_transport_error 测试改为按码断言并新增
  「无码保守按命令层」用例;map_key_load_error 测试改断码+剥码文案)
- 前端:三 verify(scope-integrity / bridge-integrity / form-validation 54)全 PASS;
  parseErrCode 行为断言(码解析/无码 null/防伪装误读/剥码/未知码降级)

# 第十七批升级(v5.16.0)— 候选池清空:远程管理四件套 + 定时探活

> 用户指令:「开始容器级指标,image_filter 消费 .env 非 UTF-8 Docker data-root
> 探测 服务器定时探活+通知这一批搞定直接发版」—— 候选池剩余五项一次做完。

## 1. image_filter 消费(wiki/07 限制 3 解除)

配置字段 `ProjectConfig.image_filter` 自 v1 起存在但从未被消费。本批在部署页
消费:选中项目 → `renderImageSelect()` 按关键字过滤镜像下拉(`repository:tag`
子串匹配、不区分大小写;空 = 不过滤);提示条三态(过滤生效 N/M、无匹配、
无可用镜像);项目切换即时重填(镜像下拉填充移到项目恢复之后,因过滤依赖
当前选中项目)。镜像页「部署」带入的 `__pendingDeployImage` 不受影响
(过滤后找不到时提示口径不变)。

## 2. .env 非 UTF-8 无损往返(wiki/07 限制 17 解除)

旧实现 lossy 读入 + 「直接保存会把替换符写回」仅靠警示条。本批:

- `manage_stack_env_read`:`StackEnv` 增 `notUtf8` + `rawB64`(原始字节
  base64);纯 UTF-8 路径行为不变
- `manage_stack_env_save`:增可选入参 `rawB64` —— 非空时校验解码 + 256KB
  后**跳过 content 直接落盘原始字节**
- 前端分流:**未改动**(草稿 === 读取时展示值)→ 带 rawB64 保存,无损回写;
  **改过** → 拒绝并 toast 指引(在服务器上以正确编码编辑)。UTF-8 文件的
  确认弹窗路径不变
- 设计取舍:**不做转码**(不引 encoding 依赖):GBK/GB18030/latin1 判不准,
  转错比不转更糟;「未改动无损 + 改动拒绝」两态覆盖真实需求且零依赖

## 3. Docker data-root 探测

概览采样命令 `host_metrics_cmd` 增 `==DROOT` 段:`docker info | grep
'Docker Root Dir:'` 探真实数据根;解析端按该路径匹配 df 挂载行(改过
data-root 的部署此前恒误判为「与根分区同盘」);段缺失/无 docker 权限时
回退默认 `/var/lib/docker`。新增单测覆盖自定义路径与回退两态。

## 4. 容器级指标

`manage_stats` 每轮成功时附 `aggregate`(camelCase):`{ count, topCpu: ≤3,
topMem: ≤3 }`,由纯函数 `aggregate_stats` 计算(解析 "12.34%" 数值降序,
解析失败排末尾不 panic;单测覆盖排序/少于 3 个/解析失败/空表四态)。
前端监控页顶部聚合条:`N 个容器 · CPU 前列:名字 xx% / … · 内存前列:
名字 x%(用量) / …`;失败轮/停止/旧版后端(无字段)隐藏 —— 版本错配安全。

## 5. 服务器定时探活 + 通知

- 新模块 `src-tauri/src/probe.rs`:按 `AppSettings.probe_interval_mins`
  (0=关,默认 0)起 tokio interval 任务;每轮**TCP 连 host:port**(5s 超时,
  不做 SSH 认证 —— 不碰密钥、不触发 TOFU);**状态翻转才通知**(在线→离线 /
  离线→恢复),首轮建基线不通知;锁纪律:状态更新锁内不 await(翻转清单
  锁外发通知)
- 挂点:setup(启动按已存设置)+ `app_settings_set`(保存即启停,无需重启);
  进程内单任务 JoinHandle 替换(同 manage_stats 会话模式)
- 通知事件类型增 `probe`:`NotifyEvents.on_probe`(默认关,notify.json);
  设置中心「通用」组新增「服务器探活间隔(分钟)」数字输入;
  通知中心「事件订阅」组新增「服务器探活状态翻转」勾选
- 探测结果不落盘(易变运行时观测,重启首轮重建基线)

## 验证

- `cargo test` 显式确认 `test result: ok. 313 passed; 0 failed; 13 ignored`
  (基线 308 + aggregate 2 + data-root 1 + probe TCP 2)
- 三 verify(scope / bridge / form 54)全 PASS;settings.js / notify.js /
  deploy.js / manage-stacks.js 过 node --check
- wiki/07 限制 3、17 改写为已实现;wiki/04 契约增补(aggregate / notUtf8+
  rawB64 / probeIntervalMins+onProbe);ROADMAP 候选池清空(五项全完成);
  三处版本号 → 5.16.0

# 第十八批升级(v6.0.0)— 安全强化与并发互斥(全量代码/wiki 审查驱动)

> 用户指令:「全面的审查代码与 wiki 对齐,顺便检查代码是否有问题」「都修上」「推送发布」。
> 八路并行审查(wiki 01/02/03/04/06/07 对齐 + Rust 后端 + JS 前端 + 安全契约)驱动,
> 先修文档与低风险代码,再逐项落地用户拍板的全部待决策项,统一 bump v6.0.0 发版。

## 一、安全(russh 升级 + CSP + 密文最小化 + 注入/路径校验)

- **russh 0.46 → 0.60.3**(修 RUSTSEC-2026-0153 / 0154 两个 HIGH;同步带 russh-cryptovec
  过 0.58 门槛):改 **ring 后端**(`default-features=false, features=["ring","flate2"]`,
  避开默认 aws-lc-rs 需 NASM 而环境没有的构建失败,与 rustls ring provider 一致)。
  API 适配 4 类:`PublicKey` 路径(`keys::key::PublicKey` → `keys::PublicKey`)、
  `authenticate_publickey` 收 `PrivateKeyWithHashAlg::new(.., None)`、认证返回
  `AuthResult` 判 `.success()`、`Handler::check_server_key` 改原生 `-> impl Future`
  (russh 未启 async-trait feature,去 `#[async_trait]` 解 E0195);`host_fingerprint`
  改用 `fingerprint(HashAlg::Sha256)` 的 Display(自带 `SHA256:` 前缀)。测试密钥改
  `Ed25519Keypair::from_seed` 确定性构造(避开 rand 0.8/rand_core 0.9 与 ssh-key
  rand_core 0.6 的 trait 版本分歧)
- **启用严格 CSP**(`tauri.conf.json` `csp: null` → `default-src 'self'; script-src
  'self'; style-src 'self' 'unsafe-inline'; font-src/img-src 'self'; connect-src
  ipc: http://ipc.localhost; object-src 'none'`):内联防闪白主题脚本挪到独立
  `ui/theme-init.js`(已加入 verify/scope-integrity.js 加载链),全站资源自托管零外链
- **get_config 密文最小化**:返回**密文哨兵 `"*"` 的只读视图**(真实 DPAPI 密文不出
  后端,`Some("*")` 保留「已存」语义);新增 `save_server_entry` 命令(编辑/新增服务器,
  `password_enc`/`key_pass_enc`/`host_key_sha256` 传 `null` → 按 `server.id` **merge
  保留**现有值,非空 → 用新值);servers.js 编辑保存改走新命令(去掉 get_config 全量
  拉取与密文透传);`save_config_cmd` 标注「须传含真实密文的完整配置,勿用只读视图回写」。
  **命令 94 → 95**
- **open_external cmd 注入修复**:`cmd /c start` 把 URL 中 `&` 当命令分隔符可本机注入 →
  改 `explorer` 直开(参数数组不经 cmd 解析)+ 拒含 `&^><|"'` 元字符的 URL
- **cleanup_execute 归档删除加前缀校验**(`is_valid_release_dir_target`,仅放行
  `<root>/releases/<ts>` 形态,与 `rollback_delete_release` 同口径;+单测)
- **rollback execute 补 `release_ts` 校验**(`/` `..` `\` 拒绝,两处 execute 入口此前
  缺,`releases_dir`/`remote_join` 不拦截 `..`,可越出项目目录)
- **sync_files 拒 `..` 段/绝对路径**(文件映射远端相对路径防逃逸出部署目录)

## 二、并发互斥(JS 审查发现,跨页/跨模态锁族)

- **06 页回滚中心接入部署互斥(高危 #2)**:新建跨页共享锁 `window.ddRemoteOp`(app.js
  全局)——此前 deploy.js 的 `st.deploying` 只在 04 页可见,06 页回滚对此一无所知,
  同一服务器上 compose up 与回滚重打标签/重载镜像可并发。现 deploy/rollback/模态回滚
  **双向互斥**:发起置锁、deploy-done 复位、入口守卫查锁。
- **批量间隙锁族(#4/#5)**:批量台间 `st.deploying=false` 但 `st.batch.active=true`
  的窗口,模式 tab、`setMode`、历史「回滚」钮、模态回滚 `beginRbExecution`、续传入口
  (单台 `onBatchResumeOne`/全部 `onBatchResumeAll`/断点 `onResumeStart`)全部并入
  `batchActive` + `ddRemoteOp`(切模式会清批量日志/横幅并触发 parseStack 干扰批量;
  间隙点回滚/续传会让批量把别台 deploy-done 记到错误服务器上)。
- **关清理模态不误清 pruning(#8)**:关闭模态不再无条件清 `st.pruning`,改由
  `cleanup_execute` 的 then/catch 收尾(含模态已切换的兜底复位)——堵死「永久卡 true
  阻止后续清理」与「重开模态并发清理同一批资源」两个方向。
- **监控先订阅后 invoke + 订阅失败停后端(#9)**:原「启动成功后才订阅事件」违项目
  纪律,且 `AppBus.on` 失败时后端采样循环仍在跑无人调 stop;现先订阅后 invoke,catch
  统一清订阅 + 调 `manage_stats_stop` + 复位 UI。

## 三、功能 bug 修复(JS/Rust 审查)

- **deploy.js `writeReleaseNotes` 缺 `req` 包裹(高危)**:第十一批「部署时预填版本
  说明」**自发布起从未真正写成功过**——`rollback_set_release_notes(req: ..)` 需
  `{ req: {...} }`,而此处传平铺参数,每次必走 catch 弹「写入失败(部署本身已成功)」。
  已补包裹层(rollback.js:572 本就是正确写法)。
- rollback.js deploy-done:**只处理本页发起的回滚**(`busy===true`,否则 04 页部署
  完成会触发 06 页整页重扫+写日志)、取消/失败不重扫(归档/标签未变化)、`errStripCode`
  剥码防控制符进日志面板、listen 失败 `.catch` 复位 `logBound`(原 unhandled rejection
  永失重试)。
- check.js 检测全过时误报「后端检测信息:未知错误」(`report.error=null` 时 `errText`
  返回兜底串恒真)→ 先判空。整栈批量空指针(`st.stack` 为 null 时取 `.services`)→
  先判 `st.stack`。config-io.js 字段错误贴到不存在的 id(`cio-export-pass-confirm`
  → `cio-export-pass2`)。app.js `setBtnBusy` 未传 label 且未曾 busy 时写入字符串
  "undefined" → 加兜底。
- manage_stats.rs 传输失败 error_code 用 `err.contains("超时")` **文案匹配**判
  Timeout/Transport(违第十六批「只认码不认文案」设计)→ 改按 `code_of` 码判定。
- migrate.rs 取消/失败时清理远端 /tmp tar(原仅成功路径清);migrate_project.rs 卷搬运
  开始先 `rm -rf` 临时目录(防失败残留堆积);migrate_project.rs 源 `compose stop` 失败
  进 `warnings`+日志(卷热导出数据或不一致,不再静默)。
- cleanup.rs `parse_du_output` 空格分隔分支保留含空格路径(+单测);docker.rs `save_gzip`
  防御性死代码补 kill+删文件(与函数其余错误路径对齐);manage.rs 宿主机性能采样
  `.unwrap_or` 吞错补 `log::warn!`(区分解析降级与传输失败);manage.js `escHtml` 补
  `"`/`'` 转义(用于 `value="..."`/`data-...="..."` 双引号属性拼接)。
- migrate.rs 两处取消文案陈旧注释(「迁移已取消」→「部署已取消」)、migrate_project.rs
  头注释 ⑧「+ 健康检查」与实际不符(`health_wait_secs` 恒 0)。

## 四、wiki 全量对齐(01/02/03/04/06/07/README,30+ 处)

- **补 errors / tray_status / probe 三个模块章节**(wiki/02;第十五~十七批新功能此前
  完全未进文档);命令计数 94→95(含 save_server_entry);文件数/行数全量复核。
- **修正失实记载**:栈扫描深度 `maxdepth 2→4`(wiki/02、wiki/07 限制 16);批量断点
  「批量路径不落断点」→ 第十四批已更正的「批量恒落断点 + 逐台续传」(wiki/01、wiki/03、
  wiki/04);续传本地 tar 复用条件「大小匹配」失实 → 单/整栈真实语义(wiki/06);wiki/07
  限制 2(部署预检已实现)划线、限制 7(残留目录非空)。
- **契约结构补字段**(wiki/04、wiki/02):`NotifyEvents.on_probe`、`AppSettings.
  probeIntervalMins`、`DeployDone.errorCode`、`StatsPayload.errorCode+aggregate`;
  `augment_pull_error` 归属 stack.rs→deploy.rs;`image_id_by_ref` 签名修正。
- **行数/计数**:wiki/01 架构图 92→95、15 个 JS、tray_status 目录树悬挂错位、模态数
  10→12、目录树全行数;wiki/README 仓库路径 E:\→D:\、代码量;wiki/03 全部 JS 行数。

## 五、验证

- `cargo test` 显式确认 `test result: ok. 315 passed; 0 failed; 13 ignored`
  (基线 313 + cleanup 前缀校验 + save_server_entry merge 两单测)
- `cargo clippy --all-targets` 新代码零新增警告(存量 18 条与本批无关)
- `cargo build --release` 成功(CSP 配置生效,产物 NSIS);改动 JS 全过 `node --check`;
  verify 三脚本(form 54 断言 / bridge / scope)全 PASS
- **真机确认 ✅(2026-09-16)**:russh 0.60 真实 SSH 连接回归(密码/私钥/加密私钥口令 + TOFU)、CSP 启用
  后 WebView 渲染、save_server_entry 编辑保存后密码/指纹保留、托盘 tooltip 悬停

# 第十九批升级(v6.1.0)— 候选池清空 + 限制修复

> 用户指令:「部署前强制预览改成用户自行勾选自动预览,不勾就不自动」「07 全部修复」
> 「hover 反白时有几处文字也是不可见都检查一下」「注意保留部署预览按钮,只是增加一个
> 勾选自动预览的框」。候选池唯一剩余项 + wiki/07 可修限制一批清空。

## 一、部署前自动预览(候选池清空)

- 整栈选项区新增「部署前自动预览」勾选(`#deploy-auto-preview`;localStorage
  `dd_deploy_auto_preview` 记忆,默认关;`deploy-stack-options` 区与智能传输选项同排)
- 勾选后点「开始部署」:环境检测通过 → **先自动跑一次变更预览**(`previewStackOnce`
  Promise 版,与「部署预览」按钮同 `preview_stack_changes` dry-run)→ 预览表就地展示
  → `confirm()` 确认后才 `startStackDeploy`;取消则预览结果保留供查看
- **「部署预览」手动按钮保留不变**(独立 dry-run 路径未动);不勾则不自动预览
- 勾选入部署锁族(refreshControls 禁用清单,部署/预检/批量中禁用)

## 二、wiki/07 限制修复

- **限制 4(BusyBox df)**:磁盘预检命令 `df -PBG` → `df -k`(1K 块)—— 所有 df(GNU
  coreutils 与 BusyBox)都支持的最通用口径,替代 BusyBox 不支持的 `-BG`;`parse_df_gb`
  改按 1K 块换算 GB(兼容旧 `G` 后缀口径);单测更新(df -k 形态 + KB→GB 换算 + 兼容 G)
- **限制 6(compose env_file)**:新增服务级 `env_file` 支持(compose 语义)——
  `load_env_path` 抽出单文件解析,`collect_service_env` 归一化字符串/数组/长语法
  `- path:` 三种形态,多项依序合并(后者覆盖前者)且整体覆盖默认 `.env` 同名字段;
  缺失文件容错回退默认 .env;3 单测(覆盖默认/字符串+长语法/缺失容错)。仍不支持
  shell 环境变量插值(仅文件来源)
- **限制 12(hover 反白 badge 不可见)**:行 hover 反白(墨底纸字)时徽章 4 变体
  (info/fail/ok/warn)全部豁免——info/fail 反回纸底墨字、ok/warn 固定回自身配色,
  不被行级 color/背景吞掉;全站排查确认行内除徽章外无其他带背景元素会被墨底吞掉
  (纯文字已被行级反白覆盖);浏览器实测 4 变体 hover 对比度 199-236(远超 WCAG 阈值)
- **限制 14(终端 ANSI 复杂 TUI):保留为限制** —— 完整 xterm 仿真需引入 xterm.js 级
  依赖并重写渲染模型,与本批低耦合硬约束冲突,维持行式终端简易剥除的既定取舍

## 三、验证

- `cargo test` 显式确认 `test result: ok. 318 passed; 0 failed; 13 ignored`
  (基线 315 + env_file 3);`cargo clippy --all-targets` 我改文件零新增警告
- 改动 JS 全过 `node --check`;verify 三脚本(form 54 断言 / bridge / scope)全 PASS
- 浏览器桩渲染验证:自动预览勾选框存在、手动预览按钮保留;注入等效 hover 选择器
  实测 4 个徽章变体对比度(info/fail 236 / ok 229 / warn 199)全部远超阈值
- wiki/07 限制 4/6/12 标注已修复;ROADMAP 候选池清空、状态速览 → v6.1.0

# 第十九批补丁(v6.1.1)— 修复密文哨兵落盘(密码/私钥口令被冲掉)

## 缺陷(用户真机反馈,严重)

加载容器列表报「密码密文 base64 解码失败:Invalid symbol 42, offset 0」——
ASCII 42 = `*`,即 v6.0.0 密文最小化引入的只读视图哨兵值,被**写进了磁盘**
`servers.json`,`password_enc` 全部变成 `"*"`(4 台服务器),连接时 base64 解码
`*` 失败。

## 根因(v6.0.0 回归,设计疏漏)

`get_config` 返回哨兵 `"*"` 的只读视图后,**全站仍有 5 处「get_config 全量取
→ 改一处 → `save_config_cmd` 全量写回」路径**(删服务器 / 删项目 / 保存项目 /
导入 compose 合并 / 保存分类),这些路径不经过 save_server_entry,直接把哨兵
原样写回磁盘。任何一次这类操作都会冲掉**全部**服务器的真实密文。v6.0.0 当时
只把「服务器编辑保存」改走 merge 命令,**没有堵住整量写回通道**——这是本补丁
修复的根本设计缺陷。

## 修复:哨兵绝不落盘(后端防护,前端零改动)

- `CIPHER_SENTINEL = "*"` 常量 + `restore_sentinel(incoming, existing)` 纯函数:
  哨兵 → 沿用磁盘现值;真实值/None 原样
- **`save_config_cmd`**:按 `id` 与磁盘现值逐服务器合并,密文为哨兵时沿用磁盘
  真实密文(配置中心导入等自带真实密文的场景不受影响;读不到磁盘现值时哨兵
  降级为 None,绝不写 `"*"`)
- **`save_server_entry`**:哨兵视同 None(编辑沿用现值;新增落 None)
- **可操作报错**:`resolve_password`/`resolve_key_passphrase` 对哨兵/非 base64
  密文提前拦截,报「服务器密码未有效保存(密文占位符或损坏)。请到服务器管理页
  编辑该服务器并重新输入登录密码保存」,替代晦涩的 base64 解码失败
- 单测 2 个:哨兵还原四态(全量回写不冲密文 / 真实新密文可覆盖 / 编辑哨兵视同
  未改 / 新增哨兵落 None)+ resolve_password 哨兵拒绝与明文优先

## 用户现场处置

- `D:\DockerDeploy SSH\config\servers.json` 中 7 处哨兵已清理为 `null`
  (备份 `servers.json.corrupted-bak`);**真实 DPAPI 密文无法从 `"*"` 恢复**,
  需在 03 页面逐台编辑服务器重新输入登录密码并保存(一次重录即长期有效)
- 本次修复后,旧版前端如何回写都不会再把哨兵写进磁盘

## 验证

- `cargo test` 显式确认 `test result: ok. 320 passed; 0 failed; 13 ignored`
  (基线 318 + 哨兵还原 2);clippy 新代码零警告(修掉 2 处无用 format!)

## 教训(入 wiki/07 决策记录)

「只读视图 + 哨兵」改造必须**同时封死全部写回通道**,不能只改「看似相关」的
那一条路径。v6.0.0 只覆盖 save_server_entry,漏掉 4 条整量写回,代价是用户
全部密文被冲。哨兵值应设计为「写路径可识别并还原」而非「仅靠消费方自觉」——
本次改为后端统一兜底(任何写路径都还原),把纪律从「调用方约定」升级为
「后端不变量」。

# 第十九批补丁2(v6.1.2)— 05 页加载提示 + hover 反白补漏

## 一、远程管理页「选择服务器后加载」反直觉(用户反馈)

**问题**:进入 05 页会自动选中服务器并立即发起数据请求(SSH 建连需数秒),
但期间表格仍是静态占位「选择服务器后加载」——用户以为要先手动选服务器,
而实际早已选中、正在取数。

**修复**(manage.js):
- 新增 `setTablePlaceholdersLoading()`:把仍是初始占位的表体改为「加载中…」
  (已有数据的表体不动,防刷新时列表整片闪成占位)
- 新增 `setTablePlaceholdersNoServer(tab)`:确实未选服务器时改为「请先在上方选择服务器」
- 接入三处:onEnter(自动选中后立即转加载中)/ onServerChange(切换服务器)/
  onTabChange(切 tab 首次加载;无服务器分支给明确提示)
- 概览区「连接中…」状态徽章原有(不在本次)

## 二、hover 反白不可见补漏(续 v6.1.1 第 12 条)

上一轮只豁免了 badge 4 变体;本轮全站扫描(data-table 行内所有带背景/彩色类)
又发现 3 类未覆盖:

- `.port-badge`(端口 +N 徽章,琥珀底墨字):行级 color 会盖文字色 → 固定回自身配色
- `.badge-running`(容器运行中,青底)/ `.badge-paused`(已暂停,琥珀底):同上豁免
- `.stat-warm` / `.stat-hot`(监控 CPU 阈值状态文字):亮主题的暗琥珀/暗红在
  hover 墨底上不可读 → **新增 scheme 翻转 token** `--ark-stat-warm-hover` /
  `--ark-stat-hot-hover`(亮主题取暗主题的亮色值、反之亦然,两主题 hover 行
  底色翻转后均有足够对比)

排查同时确认:`.stack-warn-row`(透明底显式声明)、`.container-detail-row`
(自身豁免)、`.mapping-table`(非 data-table 无反白)均无问题。

## 三、验证

- 对比度实测(纯 CSS 计算):stat-warm-hover 亮/暗主题 199/152、
  stat-hot-hover 亮/暗 154/175、port-badge 字 199,全部远超阈值
- `node --check` manage.js/manage-stacks.js 通过;verify 三脚本全 PASS
- `cargo test` 320 passed(纯前端改动,基线不变)

# 第十九批补丁3(v6.1.3)— 真机反馈修复 + 全量体验审查

> 用户真机反馈两条 + 指令「对我们前面的升级和修复进行一次体验审查,是否还有我
> 刚刚挑出的这种毛病」。四路并行审查(russh 升级彻底性 / 密文链路 / CSP / 全站
> 体验)驱动,全部修复一轮清。

## 一、真机反馈修复(用户报告)

- **RSA 私钥无法加载**(`Unsupported key type RSA`):v6.0.0 russh 升级时
  `default-features = false` 只启 `ring`+`flate2`,**漏掉默认集的 `rsa` feature**
  → `ssh-key/rsa` 未编译,RSA 私钥(用户 `tencent.pem`)报 Unsupported。
  修复:Cargo.toml features 补 `"rsa"`;内嵌测试 RSA 私钥回归单测
  (`test_load_rsa_key_supported`)
- **同源隐患(自查发现)**:`PrivateKeyWithHashAlg::new(key, None)` 对 RSA 退化为
  遗留 SHA-1 签名(`ssh-rsa`),现代 OpenSSH(8.8+)默认拒绝 → 就算能加载也认证失败。
  修复:改用官方推荐的 `handle.best_supported_rsa_hash().await` 先问服务器
  (sha2-512/256 优先,老服务器回退 sha-rsa;非 RSA 忽略)

## 二、全量体验审查(四路并行)与修复

### 2.1 密文链路审查(4 项,全修)
- **P1 get_config 只脱敏 servers,notify SMTP 密文仍回传**:`mask_ciphers`/
  `restore_sentinels` 重构为 AppConfig 级统一函数(servers + notify),出口入口
  成对维护;+notify 哨兵往返单测
- **P2 save_config_cmd 读盘失败+含哨兵 → 静默降级 None 清空密文**:改为直接拒绝
  保存(「已取消保存以免丢失已存密码」),仅无哨兵(导入场景)才放行
- **P3 config_io 导出遇哨兵整包硬失败**:`decrypt_secret` 加哨兵/非 base64 预检,
  给「请先逐台重录密码后再导出」可操作报错
- **P4 notify resolve_email_password 无预检**:与 resolve_password 同口径,给
  「请重新填写 SMTP 密码」提示

### 2.2 russh 0.60 彻底性审查(结论:无新增运行期故障)
- 逐项核对 exec/PTY/ChannelMsg/SFTP/KEX/host key/Config 两版语义:通道背压、
  close 语义、`into_stream` 增强、SFTP 无耦合(2.4.0 不依赖 russh 类型)、
  指纹字符串完全同构(已落盘 TOFU 不会误报)、KEX 只扩不收
- **顺带修**:① Cargo.toml `rust-version` 1.77.2 → **1.85**(russh 0.60 要求,
  升级时漏改);② OpenSSH 格式加密私钥错口令在 0.60 表现为 `SshKey(...)`,
  旧映射落 fs 兜底报「加载私钥失败」语义错误 → 归入「口令错误或已损坏」
  (+内嵌 OpenSSH 加密私钥测试);③ `Pad/Unpad` 解密失败类同归口令错误分支

### 2.3 CSP 审查(结论:0 问题,当前无需改动)
- 8 检查点全过:script-src 无 eval/内联;style 全走 CSSOM;字体/图片全自托管;
  connect-src 精确命中 `http://ipc.localhost`(useHttpsScheme 默认 false);
  form-action 全 preventDefault;无内联事件属性。watchlist 3 条(未来加图片
  预览配 blob:、开 HTTPS scheme 配 https、用资产协议配 asset:)已记档

### 2.4 全站体验审查(5 高 3 中若干低,全修)
- **高 #1**:`badge-exited`/`badge-created`(透明底 65% 墨字)未纳 hover 豁免 →
  墨底上不可见;补两条豁免(与 badge-info 同口径)
- **高 #2**:自动预览确认误用 `window.confirm`(全站唯一系统对话框,违自绘纪律)
  → 改内联确认区(复用 #deploy-error,继续/取消两按钮)
- **高 #3**:06 页切换服务器/扫描期间右侧明细与按钮无加载反馈 → setBusy 加
  「扫描中…」文案;无服务器时左列改「请先在上方选择服务器」(区分「扫描无结果」)
- **高 #4**:`rollback.js` 手写类 `'badge ok ...'` 中 `ok` 不存在(正确类名
  `badge-ok`)→ 「运行中 N」徽章零配色;改 fillBadge(el,'ok',...) + classList
- 中:清理模态并发防护(执行中重开提示「仍在执行中」)+ 扫描失败补 toast 与
  重试按钮;04 页三下拉进页即插「加载中…」占位;01 页重检按钮 busy;
  05 页容器操作按钮整组禁用防连点;部署历史首载「正在加载…」占位
- 低:monitor 空态纳入无服务器提示

## 三、验证

- `cargo test` 显式确认 `test result: ok. 325 passed; 0 failed; 13 ignored`
  (v6.1.2 的 322 + RSA 支持 + OpenSSH 错口令映射 + notify 哨兵 3 个新单测)
- `cargo clippy --all-targets` 新代码零新增;改动 JS 全过 `node --check`;
  verify 三脚本全 PASS
- **真机确认 ✅(2026-09-16)**:tencent.pem RSA 私钥连接、哨兵清理后的重录密码、cargo build --release 通过

# 第二十批修复批收尾(v6.1.4)— 2026-09-13 审查修复批第二轮

> 背景:五路并行审查(wiki/Rust/JS/契约/发布链)+ 主代理复核发现 P0×1 / P1×3 / P2×7,
> 逐条清单与修复提示词沉淀于 `2026-09-13-待修复bug.md`。第一轮(P0/P1-1/P1-2/
> P2-1/P2-2/P2-3 + P2-6①)已随另一会话的 v6.1.3(5917753)推送;本批在其上完成
> 剩余四项,两轮逐 hunk 对比零冲突。P1-3(跨机导入 SMTP)已由 v6.1.3 完成
> notify 脱敏成对 + 导出预检,`config_import_preview` 命令留给第二十批功能阶段。

## 一、P2-4 save_config 族读改写收口(本批最大改动)

- `config.rs` 新增 `CONFIG_LOCK` + `update_config<T,F>(mutate) -> Result<T,String>`:
  锁内 load → 闭包改 → save;闭包 Err 不落盘(整体放弃);注释写明「不可嵌套 /
  锁内只做内存修改」约束。另设 `RESUME_LOCK` 给断点表(与主配置三件互不相干,
  合用会让部署写断点阻塞配置保存)
- **11 个生产写点全部迁移**:save_server_entry / save_config_cmd(重构为
  `restore_sentinels_and_save`,哨兵还原的「读现值」与「落盘」同锁,消除
  「还原读到现值 → 落盘前被并发保存改写」窗口)/ notify_save_config(DPAPI 锁外,
  notify 替换进闭包)/ remember_key_passphrase / persist_host_key_if_needed /
  retrust_host_key / import_compose / bind_project_source / update_project_from_source
  (文件复制锁外、配置合并进闭包)/ bind_project_to_target / save+remove_checkpoint
- 测试 +2:8 线程×5 条并发零丢写;Err 闭包不落盘

## 二、P2-5/P2-6②/P2-7

- `encrypt_password` 拒收哨兵 `"*"`(挂 input 码);+1 单测
- app.js 新增 `window.errCodeOf(payload)`(errorCode 字段优先,回退 parseErrCode
  message —— 落地 wiki/04「字段优先」契约);deploy.js 三处 deploy-done 消费点接线
- package.json version → 6.1.4;rollback.js 两处裸 `__TAURI__.event.listen` 改
  `AppBus.on`(全站唯一绕过例外消除)

## 三、验证

- `cargo test` 显式 **328 passed / 0 failed / 13 ignored**(基线 325 + 本批 3)
- clippy --all-targets 零新增;node --check 16 个 JS 全过;verify 三脚本全 PASS

# 第二十批升级(v6.2.0)— 候选池第一梯队五项

> 2026-09-13 全量审查产出候选池;用户选定第一梯队先做。五项:部署历史筛选 /
> 配置导入预览 / 服务器一键诊断 / 部署模板 / 通知耗时阈值。

## 一、部署历史筛选/搜索(纯前端)

- 04 页折叠面板展开态新增筛选条:模式下拉(全部/单镜像/整栈/回滚/迁移)、
  结果下拉(全部/成功/失败/取消;取消判定与 historyResultBadge 同口径:
  码优先回退文案)、关键字 input(项目/服务器名子串,不区分大小写;
  IME 组合中不触发)
- renderHistory 渲染前经 `historyMatchesFilter` 过滤;计数行区分
  「筛选后 N / 共 M 条」;无匹配时独立空态「无符合筛选条件的记录」
  (区别于全空的「暂无部署记录」)

## 二、配置导入预览(根治 P1-3 跨机 SMTP 损坏)

- 后端:`parse_export_file` 抽出共用(读→校验信封→解密→解析);
  新命令 `config_import_preview(path, password)` **不落盘**,返回
  `ImportPreview { servers, projects, currentServers, currentProjects,
  smtpPasswordPresent }`(camelCase);当前配置读不到按 0 不阻断
- 前端两段式:runImport 先 preview → confirmBlock 三段式确认区
  (标题「确认导入并覆盖当前配置?」+ 事实清单「备份 N 台 / 当前 M 台
  (导入后将全部替换)」+ 风险行)→ 用户点「导入并覆盖」才真调
  config_import_file;备份含 SMTP 密码时风险行特别提示跨机重录
- 单测:错误口令同口径 / 摘要五字段 / **预览前后配置文件字节不变**

## 三、服务器一键诊断

- 后端:`server_diagnose` 命令(host_server.rs),逐层短路:
  TCP(5s 超时,probe 同口径)→ SSH(15s with_timeout;错误按
  `code_of` 分类:auth/timeout 给定向提示)→ docker(`--version`
  退出码;非 0 再探 `docker info` 退出码区分「无权限」与「未安装」);
  前置失败则后续步骤标 skipped 不再尝试
- 前端:服务器卡片新增「一键诊断」按钮;模态红绿灯(通过/失败/跳过
  三态徽章 + 步骤名 + 详情);全部通过给绿色 summary;失败时「复制
  全部结果」按钮(window.copyText)
- 诊断用已存凭据(不收明文密码;改密码走「测试连接」)

## 四、部署模板/预设

- 后端:新模块 `profiles.rs`;`DeployProfile`(camelCase,serde default
  全字段:mode/imageRef/serverId/projectId/useDateTag/skipUnchanged/
  forceArchive/autoPreview/releaseTitle/releaseNotes/createdAt);
  独立 `config/deploy-profiles.json`(与三件套互不相干,损坏回退空表);
  `MAX_PROFILES=20` 超上限按 created_at 裁旧;三命令
  `deploy_profiles_list`(新→旧排序)/ `save`(按 id 替换或追加,
  名称/模式校验)/ `delete`(幂等)
- 前端:04 部署页顶部「模板」条(下拉含 [整栈]/[单镜像] 前缀 + 三按钮);
  **套用只填表单不自动开跑**(模式/镜像/服务器/项目/四勾选/标题说明
  全量回填,已不存在的项放弃并在 toast 标注);**存为模板**用行内展开
  输入框(Enter 保存 Esc 取消,10s 超时收起;零新模态);**删除**两步
  确认(3s 超时还原,servers.js armDeleteConfirm 同款交互本文件自带)
- 与批量部署的关系:批量模态勾选服务器后走既有循环,模板套用先把
  表单调好再开批量(模板不直接驱动批量,避免引入第二套批量编排)

## 五、通知耗时阈值

- `NotifyConfig.min_duration_secs`(serde default 0=恒通知;上限夹
  `MIN_DURATION_SECS_MAX=3600`);`notify_save_config` 保存、
  `notify_get_config` 视图、`config_io` 导出/导入 blob 全链路携带
- `fire` 保持签名(委托),新增 `fire_with_duration(..., Option<u64>)`:
  成功且耗时 < 阈值时 log+跳过(夜间批量短平快成功不轰炸);
  失败/取消/探活恒通知;deploy/rollback 收尾传 `record.duration_secs`
- 前端:通知模态事件订阅组新增「成功通知最小耗时(秒)」number 字段
  (0-3600;回填无条件——避开探活间隔 P1-1 的恒假守卫教训);采集
  空值按 0;+纯函数单测(阈值判定四态 + 上限常量)

## 验证

- `cargo test` 332 passed / 13 ignored(+8:import preview 摘要+不落盘;
  profiles roundtrip+cap 裁剪;notify 阈值纯函数;此前 v6.1.4 的 3 个)
- clippy --all-targets 零新增;node --check 17 个 JS;verify 三脚本 PASS
- **浏览器桩逐项功能验证**(http.server + _tauri-stub,用完已删):
  历史筛选三轴交互(2/5→0/5→空态)、模板套用六字段回填(模式/服务器/
  项目/autoPreview/skip/标题)、诊断模态红绿灯三行+summary、导入预览
  两段式(facts/risk/双按钮,SMTP 跨机提示文案)、通知阈值字段保存
  载荷 minDurationSecs=300
- judge 4 张截图验收 pass(部署页模板条/整页/诊断模态/导入确认区)
- 命令数:HEAD 96 → 101(实测;此前文档 95 与实际差 1)

## 遗留与取舍

- 通知阈值只作用于部署/回滚 success(probe/migrate 未接 —— migrate
  无 duration 字段,probe 语义不适用)
- 模板不驱动批量(见四;若反响好再考虑「按模板批量」)
- `server_diagnose` 的 docker 权限降级探测用 `docker info` 退出码近似,
  与 manage.rs 的 perm_denied 判定同粒度(非精确匹配 stderr)

# 第二十一批升级(v6.3.0)— 第二梯队五项 + 文档治理

> 用户批复:「可以按你建议的来」(〇文档清扫 + 第二梯队 + doc-consistency.js)。
> 承接 v6.2.0(第二十批第一梯队五项)与 v6.1.4(审查修复批收尾)。

## 一、批量部署报告导出 + 重跑失败台(第二梯队)

- 批量结束后面板新增两按钮(数据全来自 `st.batch.results`,关页即失的痛点):
  - **导出报告**:Markdown 落盘(汇总计数/模式/时刻 + 逐台结果表 + 续传提示);
    经系统保存对话框选路径,新命令 `write_text_file(path, content)`
    (拒绝空路径/父目录不存在/>4MB;临时文件 + rename 原子写)
  - **重跑失败台(N 台)**:按 results 的 failed 名称映射回 queue 的 server/project,
    组装普通队列从头发起(**不用断点**,与「续传此台」语义区分——后者用断点)
- 命令数 100 → 101;wiki/04 契约登记

## 二、回滚两版本对比(第二梯队)

- 06 页归档行首加勾选框(`.rollback-diff-cb`,最多两项先进先出,stopPropagation
  不触发行点击);勾满两个后「发布归档」区块标题出现「对比选中版本」→
  `#release-diff-modal`(第 13 号模态)
- 对比内容:两侧 ts/标题/包数头部 + 逐服务镜像 tag 表(新增/移除/镜像变化/不变
  四态徽章;缺失侧弱化)+ 版本说明并排;**数据取自 `rollback_project_detail` 已
  返回的 `releases[].manifestImages`,零新命令零额外往返**
- `diffSel` 切项目/目录失效时清空;Esc/遮罩/关闭钮三通道 + `isTopModal` 仲裁

## 三、启动静默检查更新 + 更新失败回执(第二梯队)

- **启动静默检查**:`AppSettings.auto_check_update`(serde default true,第二十一批);
  启动延迟 4 秒读设置(尊重开关与代理)→ `update_check` → 有新版仅在
  `#dock-version` 加 `.has-update` + 信号青圆点(不弹窗),点击打开设置中心;
  失败静默。设置中心「通用」组新增「启动时静默检查更新」勾选框
- **更新失败回执**:`take_update_pending` 标记与当前版本**不一致**时,原先静默
  丢弃 → 现 toast「自动更新未生效,当前仍为 vX(可到设置中心重新检查更新)」
  (安装中途失败的场景由「静默」转为明示)

## 四、托盘闭环(第二梯队)

- 托盘菜单从「显示/退出」两行扩为四行:**停止当前部署**(`deploy.running` 时
  可用;点击置 `DeployState.cancelled`,步骤边界生效——与前端取消同语义,面板
  未开也生效)+ **上次部署:成功/失败/已取消/—**(只读,随状态同步文本)
- `tray_status::sync_menu` 在每次 `apply`(状态变更)时同步两菜单项;句柄经
  `register_menu` 注册(`MenuItems`/`TrayMenuHandles`)
- **真机确认 ✅(2026-09-16)**:托盘菜单四项显示与「停止当前部署」实停

## 五、文档治理:verify/doc-consistency.js(本次审查统计同根问题)

- 三类客观真值断言:**版本号**(tauri.conf ↔ Cargo.toml ↔ wiki/README ↔ ROADMAP)、
  **命令数**(lib.rs generate_handler 实测 ↔ wiki/04 声明)、**测试数**
  (`--write --tests=N` 采集缓存 ↔ wiki/README 与 ROADMAP 声明)
- **上线当天即抓到一处真实失准**:wiki/README 测试数 325 落后于 332(v6.2.0
  批次漏更)—— 已修,现全 PASS
- 〇文档清扫:ROADMAP 状态速览新旧两行矛盾(101 vs 95)合并;待修复清单
  P1-3 终态更新(已随 v6.2.0 配置导入预览根治)
- 顺带修正 wiki/03:75 残留的「批量不落断点」错误记载(第十四批已更正,该处漏改)

## 六、验证

- `cargo test` 332 passed(本轮无新增单测;authSettings 两处测试初始化补新字段);
  `cargo build --release` 通过;clippy 零新增
- 改动 JS(rollback/deploy/app/settings)全过 `node --check`;verify **四**脚本
  (form 54 / bridge / scope / **doc-consistency**)全 PASS
- **真机确认 ✅(2026-09-16)**:托盘菜单四项(停止当前部署实停)、dock 徽点出现与点击、
  批量报告导出落盘、两版本对比模态渲染

# 第二十一批补丁(v6.3.1)— 修复一键诊断对加密私钥服务器误报

> 用户真机反馈:「一键诊断时,使用私钥的服务器可以通过 ssh 检查,但密钥就不行;
> 并且使用密钥的测试连接是正常的,到了诊断就错了」。

## 缺陷

`server_diagnose`(第二十批阶段三,`commands/host_server.rs`)的 SSH 层直传
`SshClient::connect(&server, None, None, ..)` —— **未经 `resolve_password` /
`resolve_key_passphrase` 解析已存凭据**。后果:

- 加密私钥的服务器:诊断路径 `load_secret_key(path, None)` → `KeyIsEncrypted`
  → 报「私钥已加密,请输入私钥口令」;而「测试连接」经 `resolve_key_passphrase`
  解密口令后正常 —— 症状即「测试连接通过、一键诊断失败」;
- 密码认证的服务器:同理报「需要密码」(Input 码),只是用户当前没测到。

## 修复

- 诊断的 SSH 层改为与全仓所有连接点同款:先 `resolve_password`(AuthType 判定
  + 已存密文解密)与 `resolve_key_passphrase`(私钥口令解密),失败时该步标 fail
  并附可读原因(走 `errors::strip` 剥码),后续步骤 skipped
- `resolve_password` 返回的 Input 错误(密码未保存)与 resolve 失败场景一并
  呈现为「凭据解析失败:…」——比笼统「SSH 连接失败」可操作

## 守护(第二次同类漏项的固化)

这是第二条「连接路径漏项」(前有 v6.1.3 RSA hash 退化):
新增源码级断言 `test_all_connect_sites_resolve_credentials` —— 扫描
host_server/deploy/rollback/cleanup/resume/manage 六文件的全部
`SshClient::connect(` 调用(含跨行参数),必须同时出现
`password.as_deref()` 与 `key_pass.as_deref()`,否则列出违规点并失败。
**先红后绿验证**:临时把诊断改回 None → 测试精确报 `host_server.rs:139`;
恢复修复 → 转绿。「新增连接点必须解析凭据」从此是编译+测试双兜底。

## 验证

- `cargo test` 显式确认 `test result: ok. 333 passed; 0 failed; 13 ignored`
  (基线 332 + 守护测试 1);守护测试先红后绿已证真;clippy 零新增
- doc-consistency 四类断言全 PASS(版本 6.3.1 / 命令 101 / 测试 333)
- **真机确认 ✅(2026-09-16)**:加密私钥服务器的「一键诊断」全绿(与测试连接行为一致)

# 第二十一批补丁2(v6.3.2)— 两版本对比改按镜像 ID

> 用户真机反馈:「两版本对比的版本说明比较奇怪……明明我的镜像更新了但还是
> 对比的时候显示无变化」。用户判断准确:原实现**只比 `tag` 字符串**。

## 缺陷

`#release-diff-modal`(第二十一批)的对比数据源是 manifest 的
`ManifestImage { service, tag, file }` —— **不含镜像 ID**。两次部署之间
tag 名不变(如 `myapp:latest`)但内容已重新构建时,ID 不同而 tag 相同,
对比必然显示「不变」;只有新增/移除服务(来自服务名集合差异)能看出来。

## 修复

- **后端**:`ManifestImage` 增 `id: Option<String>`(`sha256:` 前缀原样;
  `#[serde(default)]` 兼容旧归档)。部署收尾写入 manifest:
  - 智能传输路径复用判定时已查到的 `image_id_by_ref` 结果(零额外调用);
  - 非智能传输/断点续传路径经新助手 `collect_local_image_ids` 采集
    (纯本机 inspect,毫秒级;单项失败为 None 不阻断部署)
  - `build_manifest_images` 增 `ids` 参数(纯函数,签名 +1)
- **前端**:对比判定改为**ID 优先**——双侧都有 ID 按 ID 判(内容寻址);
  任一侧缺 ID(旧归档/采集失败)回退按 tag 比较,并在表下注明回退原因;
  详情模态与对比表的 tag 后附短哈希(8 位),支持人工核对
- **单测 2**:`build_manifest_images` 记录 ID(含打包/跳过两态)/ 旧归档
  manifest.json 无 id 字段 serde 兼容 + 新归档 id 往返
- **行为验证**(Node 复刻判定逻辑 7 用例):核心场景「同名 tag 不同 ID →
  镜像变化」通过;回退/新增/移除各态正确

## 已知边界(记录)

- 本版**之前**的旧归档没有 ID 记录,与新版对比时这些行仍走 tag 回退
  (界面已注明);重新部署一次即写入 ID,从此对比按内容
- 单镜像部署不受影响:走日期标签,每次 tag 自带时间戳,天然按内容区分

## 验证

- `cargo test` 显式确认 `test result: ok. 335 passed; 0 failed; 13 ignored`
  (基线 333 + 2);clippy 保持基线 18;doc-consistency 全 PASS
- **真机确认 ✅(2026-09-16)**:重新部署一次整栈 → 两次新归档对比应正确显示「镜像变化」

---

# 第二十一批补丁3(v6.3.3)— 项目迁移默认命名兜底

> 用户真机反馈:项目迁移预检报「服务「backend」未声明 image,其镜像不参与
> 搬运(需在目标服务器构建)」(offical / houtai 同构)。用户判断准确且提出
> 两点:①部署管线对「build 无 image」有默认命名兜底,迁移同样需要;
> ②本系统镜像靠 `docker save → SFTP → docker load` 搬运,目标服务器
> **没有构建能力**,「需在目标服务器构建」的说法本身不成立。

## 缺陷(根因)

迁移两处解析都调 `stack::parse_compose_file(&compose, &[])` 传**空镜像列表**,
且解析发生在本地临时目录(`dd-plan-<uuid>` / `dd-migrate-<uuid>`,父目录名
失真)——默认命名兜底的两个输入(镜像列表数据源、目录名候选来源)双双缺位:

- `scan_default_image(candidates, local_images=[])` 必然 miss → `image = None`;
- 迁移侧对 `None` 服务直接丢弃并报「需在目标服务器构建」(且该表述不成立)。

同构项目(服务名 backend/offical/houtai,compose 只有 `build:` 没写 `image:`)
在源服务器上镜像实际以 `<源目录名>-<服务名>:latest` 存在,却全部漏搬。

## 修复(与部署管线同口径;后端为主,零前端逻辑改动)

- **stack.rs**
  - `StackService` 增 `fallback_filled: bool`(`#[serde(skip)]`)——消费方
    区分「compose 显式声明」与「兜底推导」,前后端契约结构不变;
  - 新入口 `parse_compose_file_with_dirs(compose_path, local_images, dir_names)`
    与 `compose_project_name_candidates_with_dirs(...)`:候选目录名**覆盖**
    (迁移注入 origin.json 原目录名 + 源部署目录末段名,替代失真的临时目录
    推导);`dir_names` 为空时与原入口**逐字节等价**(有等价性单测钉死);
  - `sanitize_project_name` 抽出(与既有候选「合规化小写」派生同口径);
  - `target_default_image_ref(compose_path, target_dir_name, service)`:
    推导**目标侧** compose 默认命名(`<顶层 name 或目标目录名>-<服务>:latest`),
    供迁移补标签。
- **migrate_project.rs**
  - 预检 `build_plan` 与执行侧都改为:先 `query_remote_images_full` 查源
    服务器镜像列表(**一次往返**,同时服务兜底扫描与存在性校验,后者从
    第二次查询降为零),再以 (镜像对, 目录名候选) 注入 `parse_compose_file_with_dirs`;
  - 新纯函数 `collect_transfer_images(&ComposeStack) -> (Vec<TransferImage>, Vec<String>)`:
    兜底命中 → 入列 `TransferImage { service, reference, fallback_filled }`
    + 识别说明(「已按 compose 默认命名在源服务器识别到 X 并参与搬运」);
    仍未命中 → 可执行修正指引(先部署过该服务/确认命名/补 image: 字段),
    **删除「需在目标服务器构建」全部表述**;
  - `migration_dir_name_candidates`(纯函数):候选目录名来源 = 应用内副本
    origin.json 原目录名(仅绝对路径时读,防远端相对路径拼出 CWD 同名文件)
    → 源部署目录末段名;
  - **目标侧补标签(正确性闭环)**:兜底条目在两机命名推导不一致时——
    「目标 load 完成后」与「目标已有同 ID 镜像跳过传输」两条路径——执行
    `docker tag` 零拷贝补目标名(如 `zetok-backend:latest` →
    `prod-app-backend:latest`);否则目标 `compose up` 找不到镜像会转去构建
    (目标无构建上下文,必然失败)。补打失败仅记 warning(up 时二次暴露,
    文案附手动命令),不推翻已完成的搬运。
- **前端**:`ui/deploy-migrate.js` 镜像表空态文案同步(「未声明 image 且
  源服务器未识别到默认命名镜像」)。

## 单测(+10)

stack.rs:with_dirs 覆盖命中 / 空列表等价旧入口 / **红绿对照**(空列表必须
miss = 修复前误报路径,注入后命中)/ fallback_filled 两态 / target 命名推导
(目录名合规化小写、顶层 name 优先)。
migrate_project.rs:兜底命中入列带标记与识别说明 / 显式声明无提示 / 未命中
给指引且旧文案消失 / 目录名候选优先级(origin.json 前、部署目录后)/ 手工
项目(远端相对路径)仅取部署目录名。

## 验证

- `cargo test` 显式确认 `test result: ok. 345 passed; 0 failed; 13 ignored`
  (基线 335 + 10);clippy 保持基线 12(改动文件零新增);
  `node --check` 全部 JS;verify 三脚本 PASS;doc-consistency 全 PASS
- **真机复测 ✅(2026-09-16)**:真实同构项目迁移预检应识别到镜像并列入搬运清单;两机
  目录名不同时,目标补标签日志出现且目标 `compose up` 成功

---

# 第二十二批(一):定时/延迟部署(v6.4.0 首项)

> 第三梯队第一项:项目级「每天 HH:MM」或「一次性延迟」日程,到点由后端
> tick 自主发起部署,复用既有单发管线。用户定案:错过不补跑(跳过并记录)。

## 互斥收口(前置子项:后端发起引入的硬不变量)

调度器是首个**后端自主发起**的部署方:此前互斥完全在前端(`window.ddRemoteOp`
+ `st.deploying`),而后端所有部署/回滚共享同一份 `DeployState.cancelled`
取消位 —— 两个管线并发时后发起方的 `reset_cancelled` 会吞掉先发起方的取消
意图。本次把互斥收口到后端:

- `commands/mod.rs` 新增 `REMOTE_OP_IN_FLIGHT: AtomicBool` +
  [`RemoteOpGuard`](RAII,Drop 释放;`acquire_remote_op()` 用
  `compare_exchange` 保证并发恰一胜);统一拒绝文案「已有远程操作进行中
  (部署/回滚/迁移),请等待其完成后再试」。
- 接入点(全部远程操作入口):`run_one_deploy` / `run_one_deploy_stack`
  顶部(覆盖 deploy / deploy_stack / 前端批量逐台 / 断点续传 / **定时**全路径;
  被拒时按 opts 表达「恰好一次 deploy-done 失败帧」);`rollback_execute_stack`
  / `rollback_execute_single` / `rollback_execute_stack_at`(async 命令 await
  全程持位);`migrate_images` / `migrate_project_start`(同步获取、守卫随
  spawn 任务移动)。
- 单测 **1 个(含两段)**:串行互斥与释放 + 8 线程并发恰一胜(用「取胜者
  等待其余线程完成尝试再释放」消除第二获取窗口)。

## 调度模块 `src-tauri/src/deploy_schedule.rs`(新文件)

- 存储 `config/deploy-schedules.json`(`DeploySchedule` camelCase,与前端
  直通;不含密文,不进 CONFIG_LOCK,同 profiles.rs 低耦合先例);模块内
  `SCHED_LOCK` 保护读改写,`MAX_SCHEDULES = 30` 超限裁最旧。
- **tick 循环**:`Mutex<Option<JoinHandle>>` 单任务,setup 常驻启动;30s
  一轮,触发窗口 `[时刻, 时刻 + 90s)`(容忍 tick 抖动与休眠唤醒)。
- **错过不补跑**(用户定案):启动扫描对「今天已过窗口且未跑」的 daily 写
  一条「已错过(应用未运行),未补跑」(仅记一次);once 跨天未执行则**停用**
  (一次性不得跨天迟到执行,`is_due` 要求 `today == created_at 的日期`)。
- **触发**:校验项目/服务器仍在 → stack 模式 `parse_project_stack`
  (与部署页同口径,含 service_overrides)+ 过滤无 image 服务;single 模式用
  存量 `image_ref` → `run_one_deploy(_stack)`(`DeployEmitOpts::scheduled()`,
  事件与断点语义同单发)→ 回写 `last_run_date` / `last_result`(once 执行后
  自动停用)。托盘/历史/通知/断点全部由既有收尾链路承担。
- 命令 3 个:`deploy_schedules_list / save / delete`(save 校验 HH:MM 格式、
  mode、kind、single 必须有 image;**新条目 created_at 后端归一**——它是
  once 的目标日锚点)。
- 单测 **6 个**:时间格式校验 / 触发窗口边界(半开区间)/ 停用-已跑-昨天跑过
  各守卫 / once 仅创建日触发 / 错过处置(daily 记录保持启用、once 停用)/
  camelCase 契约往返。

## 前端

- 新文件 `ui/deploy-schedule.js`(自含 IIFE,不新增 window.<Kit> 键;
  index.html 引入 + `verify/scope-integrity.js` CHAIN 同步):04 页
  page-tools「定时」按钮 → `#deploy-schedule-modal`(列表 + 新建/编辑表单
  两视图);列表六列(项目@服务器 / 模式 / 时间 / 启用开关 / 上次结果 /
  操作),删除两步确认(3s 还原),启用开关点按即保存;表单项目→服务器联动
  (按 default_server_id 预选)、模式切换显隐镜像行与强制留档。
- `ui/deploy.js` **外部部署采纳**:`deploy-progress` 且 `!st.deploying`
  (非批量)时置位 `st.deploying`/`ddRemoteOp` 并 `refreshControls()` ——
  定时部署由后端发起,前端借此进入「部署中」态(取消钮可用、控件禁用),
  收尾由既有 deploy-done → handleDone 复位。
- style.css:模态加宽(min(860px,94vw))+ 操作列按钮不换行/纵向排列
  (judge 首轮曾见按钮被压成竖排,已修)。

## 验证与记录

- `cargo test` 353 → **352**(互斥 1 合并 + 调度 6;首版互斥两测试共享
  进程级静态量被 cargo 并行调度互撞,**flaky 一次实测后合并为单测试 +
  确定性同步**,连跑 5 次稳定)/ clippy 保持基线 12 / node --check 17 JS /
  verify 三脚本 PASS
- 浏览器桩验证:列表与表单两视图截图交 judge **均 PASS**(六列对齐、
  按钮横排、单选默认选中、中文黑体、遮罩层级;非阻塞观察:复选框原生
  圆角为全站既有语言、停用行有意弱化)
- **真机复测 ✅(2026-09-16)**:创建 daily 日程到点触发的完整链路(托盘 tooltip /
  历史 / 通知落于既有链路);互斥拒绝在并发场景下的用户体验

---

# 第二十二批(二):终端多标签 + 同栈广播 + 输出落盘

> 第三梯队第二项。后端本就是多会话 HashMap(write/resize/stop 按 session_id
> 独立),原前端「单会话防护」是唯一瓶颈 —— 本次放开为多标签,并补上用户
> 点名的两项能力:命令广播(同栈限定,用户定案)与输出落盘(自动全部落盘)。

## 后端(命令与事件契约零变化)

- **输出落盘**(`manage_exec.rs`):读任务每帧把 data 追加到
  `<应用目录>/logs/term-<yyyyMMdd-HHMMSS>-<容器ID前12位>.log`(会话头含
  容器/shell;结束写一行结束原因;`term_log_file_name` sanitize 危险字符与
  超长)。**自动全员落盘、失败仅告警不杀会话**。命令签名不含容器名,文件名
  用容器 ID(契约零变化)。
- **容器行增字段**(`manage.rs`):`RawContainer` 解析 `Labels` 字符串
  (`docker ps --format json` 的逗号分隔形态),`ContainerRow` 增
  `compose_project: Option<String>`(新纯函数 `compose_project_of_labels`),
  供前端同栈广播限定范围。
- 单测 +3:文件名 sanitize(危险字符/空名/超长)/ append_term_log 写与失败
  短路 / label 提取四态。

## 前端(`ui/manage-stacks.js` 终端节整体重构;ManageStacks 导出键名不变)

- **多标签**:`cState.exec` 改为 `{ tabs, order, activeKey, unlisten,
  listening, listenGen, buffer }`;每标签独立 session/lines/历史/游标/ANSI
  悬挂缓存/resize 缓存。表内「终端」→ 模态未开则开、已开则加标签
  (同容器已有存活标签直接切过去);标签栏(名称+×,超宽横滚)点击切换;
  关闭 tab 停其会话,最后标签关闭即关模态;关模态/切服务器/离页停**全部**
  会话(宿主 `stopExecSession` 语义升级为全停,调用点语义本就如此)。
- **单监听按 sid 路由**:一个 `manage-exec-output` 监听;session 未建立期间的
  早期事件入 `buffer`、invoke 返回后按 sid 认领(替代旧「每次 start 一个新
  监听」的缓冲重放)。**同步双注册守卫 `listening` + 代际 `listenGen`**:
  首版只判 `unlisten`(promise resolve 才赋值),而建模态与 startExec 在同一
  同步任务内都调 ensureExecListener → 注册两次、每条 payload 路由两遍
  (judge 实测抓到;**真实 Tauri 下 listen 同为异步,同样会双注册**);
  修复为同步标志 + 过期注册回调自注销。
- **同栈广播**:顶栏「广播同栈」勾选;开启时输入发给**同 compose_project
  的活跃标签**(无同伴/项目名为空时禁用);广播只作用于 write,输出仍按标签
  各归各。
- resize 只推活跃标签,切标签补推(缓存按标签独立);渲染仅活跃标签。
- style.css:`.term-toolbar/.term-tabs/.term-tab/.term-broadcast`(ark 纪律:
  直角、无新色值、选中态墨底反白)。

## 验证

- `cargo test` **355 passed**(+3)/ clippy 保持基线 12 / node --check 17 JS /
  verify 三脚本 PASS
- 浏览器桩(no-cache 本地服务 + 多容器 mock,**含 compose_project 字段**)
  端到端:开两个标签 → 广播发 `uptime` → 两个会话各恰好收到一次(切标签
  逐一核对缓冲);judge 一轮判 FAIL(重复投递)→ 修复 → 复验 **PASS**
  (逐行亮度扫描确认 5 行无重复)
- **教训**:IAB 后端按 URL 缓存资源,桩验证改完 JS 必须给预览页脚本加
  `?v=N` 查询串(记入浏览器验证流程);`AppBus.on` 的 unlisten 是异步返回,
  任何「只判 unlisten 的幂等守卫」都是无效守卫
- **真机复测 ✅(2026-09-16)**:多标签并发会话(不同容器)真实 PTY;广播在真实同栈异构
  容器上的表现;`logs/term-*.log` 落盘文件实测查看

---

# 第二十二批(三):配置版本历史

> 第三梯队第三项。部署有回滚中心,配置没有 —— 补上「写盘前自动留档 +
> 一键恢复」,对竞态覆盖与误导入兜底(与 v6.1.1 哨兵事故同源的防御诉求)。

## 后端

- **快照机制**(`config.rs`):`save_config` 写盘前把**当前磁盘上的三件套**
  (servers/projects/notify)拷到 `config/.history/<yyyyMMdd-HHMMSS>/`
  (folder-per-event,三文件同快照保证同一时刻一致性);
  - **与最新快照全等时跳过**(TOFU 指纹等高频写点不刷屏);
  - cap `HISTORY_KEEP=20` 份裁最旧;同秒冲突加 `-N` 后缀;
  - **失败仅 log::warn,绝不阻断保存主流程**(版本历史是兜底能力,其故障
    不该影响配置写入);半份快照自清理;
  - **不变量:先快照、后覆盖** —— 快照内容永远至少包含上一个已落盘版本。
- **导入路径**:`config_import_file`(绕过 save_config 的直写)覆盖前同样
  快照(整体替换正是最需要兜底的场景);`config_wipe` **不涉**(尊重清除
  语义,文档注明)。
- **命令 2 个**(config_io.rs):`config_history_list`(新 → 旧,含文件清单与
  字节数)/ `config_history_restore(id)`——id 经 `is_valid_snapshot_id`
  校验(防路径穿越),**恢复前自动留一份当前状态快照**(误恢复可再恢复)。
- 单测 +5:快照生成/全等去重/内容变化新增 / cap 裁最旧(含边界名核对)/
  id 校验八态 / save_config 快照的是「保存前」状态 / 恢复往返 + 穿越拒绝。

## 前端(`ui/config-io.js`)

- 新增「历史快照 HISTORY」区块(位于导入与危险区之间,`appendHistoryGroup`):
  打开模态即拉取列表;每行 = 等宽 id + 文件清单 + 大小 + 「恢复」按钮;
  恢复用**两步内联确认**(再点一次「确认恢复?」,3s 超时还原;模态关闭
  复位武装态);成功后 toast + 整页刷新(与导入/清除同口径)。
- style.css:`.cio-history-*` 组(直角/弱化 meta/按钮不换行)。

## 验证

- `cargo test` **360 passed**(+5)/ clippy 保持基线 12 / node --check 17 JS /
  verify 三脚本 PASS
- 浏览器桩:历史区块渲染(3 条快照、列对齐、层级)与两步确认(armed 态
  「确认恢复?」+ is-armed)端到端验证;judge **PASS**(像素级核查:结构完整、
  等宽 id、按钮不被压扁、危险区不受影响)
- **真机复测 ✅(2026-09-16)**:真实写盘(编辑服务器/保存项目)后 .history 出现快照;
  恢复后应用刷新且数据回退正确;全等去重(连续两次同内容保存只留一份)

---

# 第二十二批(四):文档治理批

> 候选池「文档/治理批」五项收口:90% 文档问题同根(版本戳与计数靠人工
> 同步),本次把「能自动断言的全部自动断言」。

## 1. 文档不符清扫(实证清单逐条落地)

- russh 0.46 → **0.60.3**(wiki/01 技术栈表、wiki/07 取舍表 6;含
  RUSTSEC-2026-0153/0154 修复说明);
- 命令数 95/94 → **106**(wiki/01 架构图两处 + lib.rs 行注、wiki/02
  Builder 描述、wiki/README 两处);
- 测试基线 290/320/335 → 363(wiki/07 全绿清单、wiki/README 速览);
- migrate_project 1,625/1,600 → 约 1,900;manage_exec 521 → 653;
  lib.rs 255 → 289;代码量口径(Rust ~29,000 行 33 文件 / 前端 ~22,600 行);
- wiki/04 `open_external` 口径修正(explorer 直开,非 `cmd /c start`);
  `prune_server` 标注「已无前端调用点」(03 页改走清理分析后的遗留兜底);
- wiki/01 目录结构补 `deploy_schedule.rs` 与 `deploy-schedule.js`。

## 2. 七篇 wiki 页首版本戳 + doc-consistency 增断言

- 七篇页首统一 `> 对齐版本:v6.4.0(测试基线 363 passed / 13 ignored;命令 106 个)`;
- `verify/doc-consistency.js` 增第 4 类断言:**七篇页首戳必须等于真值版本**
  (此前只有 版本号/命令数/测试数 三类)。「三处版本号」纪律正式扩为
  「页首戳自动守护」。

## 3. verify/contract-smoke.js(新,契约集合双向核对)

四条断言(零依赖纯文本解析,不执行代码):
- **前端 invoke ⊆ 后端注册**(严格,ghost 命令 = 运行期必挂);
- **注册 − 前端消费 ⊆ 白名单**(死命令显式登记:`prune_server` /
  `deploy_batch`,各附理由);
- **前端 listen ⊆ 后端 emit**(严格,ghost 事件);
- **后端 emit − 前端 listen ⊆ 白名单**(`deploy-batch`,跟随死代码路径)。

解析口径(保守可解释):invoke 仅字面量形态 + `DYNAMIC_COMMANDS` 补充表
(6 个变量形态调用点:test_server / showResourceInspect 系 / beginRbExecution
等);emit 收集过滤 kebab-case 或 KNOWN_EVENT_NAMES(排除 `batch-done` 这类
状态值常量误收)+ 事件常量名表(EXEC_EVENT/STATS_EVENT/LOGS_EVENT)。

**负向测试验证有效性**:注入一个 ghost 命令调用 → 精确捕获并 exit=1;
恢复后 PASS。**ci.yml 接入**:clippy/test/语法检查之后追加 contract-smoke
与 doc-consistency 两步(纯本地解析零风险)。

## 4. 编排层补测试(+3)

- `compose_file_flags`:单/多 -f 拼装 + 单引号注入转义(**断言修正过一次**:
  首版按 substring 断言转义序列写法失真,改精确全串断言);
- `compose_override_names`:临时目录只返回实际存在的 override;
- ssh.rs `join_remote`/`normalize_remote`:斜杠边界六态。

## 5. README 与 AGENTS.md

- README 新增「密码与密文说明」章节:密文不出后端(只读视图哨兵)、
  DPAPI 绑定机器与用户、跨机迁移用配置中心导出/导入;
- AGENTS.md(/init 产物)随批入库并更新至 v6.4.0 状态,新增两条硬约束:
  **远程操作互斥收口**(新后端发起功能必须取 `acquire_remote_op`)与
  **版本历史不变量**(绕过 save_config 的直写路径必须先快照)。

## 验证

- `cargo test` **363 passed**(+3)/ clippy 保持基线 12 / node --check 17 JS /
  verify **六脚本**全 PASS(含新增 contract-smoke 的负向测试)
- doc-consistency 四类断言全 PASS(版本/命令数/测试数/七篇页首戳)

---

# 第二十二批补丁:v6.4.1 终端多标签可达性修复(真机反馈)

## 1. 现象与根因

用户真机反馈:**开启多标签后实际一次只能打开一个终端标签**,打开一个后只能
点击关闭(关闭即关闭整个终端),再打开新的又只有一个。

根因不在多标签状态机(harness 用真实 manage-stacks.js 实测:程序化连续
`openTerminal` 两次,两标签并存、独立会话、独立关闭、单监听单注册,全部正常),
而在**入口可达性**:

- 「加标签」的唯一入口是模态外表格行的「终端」按钮(`manage.js:686-695`);
  但终端模态是全屏遮罩 `.modal-overlay { position:fixed; inset:0; z-index:900 }`,
  打开后完全盖住表格,该按钮物理上不可达;
- 点击落在遮罩上还会命中 `e.target === overlay → closeModal()`,而 closeModal
  经 `execOnModalClose` 执行 `stopAllExecTabs()` —— **全停会话并关模态**;
- 模态骨架内没有任何「新建标签」控件,也没有快捷键/右键路径。

于是操作链恒定收敛到「开一个 → 想开第二个只能关掉整个终端 → 再开还是 1 个」。
桩验证当时只覆盖了程序化双开,没有覆盖真实层叠布局下的入口可达性,故未暴露。

## 2. 修复(纯前端,不动后端与契约)

- `manage-stacks.js` `buildTerminalModal()` 顶栏新增「＋新标签」下拉
  (`#term-newtab-select`):选项来自表格数据 `state.containers`(只列运行中
  容器,文本 `名称 @ compose项目`);选中即 `openTerminal(...)`(同容器已开
  走既有去重直接切回);已开标签的容器加「(已打开)」后缀标注。
  - `populateNewTabSelect()` 带签名守卫(数据未变不重建 DOM,避免在原生下拉
    弹出期间做无谓 option 替换);开关标签经 `renderExecTabs` 同步刷新标注;
  - 模态骨架重建时签名复位强制首填;空列表给「无运行中容器」禁用占位;
  - 选项一律 createElement/textContent 防 XSS。
- `style.css` 新增 `.term-newtab`(镜像 `.term-broadcast` 语言:inline-flex /
  gap 5px / flex:none / 12px;select 限宽 180px);无新色值,圆角纪律不变。
- `ui/help.js` 终端章节旧文案「同一时间只允许一个终端会话」改为多标签口径
  (＋新标签下拉、标签切换、× 关单标签、广播同栈、落盘文件说明)。

明确不做:改「表格按钮=加标签」原设计(需把模态改抽屉,动共享模态体系/
焦点陷阱/尺寸量测耦合,不成比例);改遮罩点击行为(「关闭弹窗即断开会话」
是既有文档化设计)。

## 3. 验证

- 桩端到端(mock 3 容器含 2 个同 compose 栈):开第一标签 → 下拉开同栈第二
  标签(2 标签并存、active 切换、输出各归各)→ 广播输入两会话各写 1 次
  (桩计数断言)→ × 关非活跃标签(模态保持、会话精准停)→ × 关最后标签
  (模态关、全停)→ 重开 + 遮罩关闭回归(既有行为不变);**judge PASS**
- `node --check` + verify 四脚本(form-validation / bridge-integrity /
  scope-integrity / contract-smoke)全 PASS
- `cargo test` **363 passed / 13 ignored**(纯前端改动,基线不变)

---

# 第二十三批(一):细节补正 7 项(S1 会话,2026-09-17)

> 双会话并行开发首发:S1(本批,细节补正 7 项)/ S2(终端日志保留,
> 见「第二十三批(二)」)。文件主权矩阵见 AGENTS.md「双会话并行开发协议」。

## 1. 视觉 3 项

- **终端输入框 / `.env` 编辑器占位符对比度**(`ui/style.css`):
  两处均漏配 `::placeholder`,吃浏览器默认 `#757575` —— 实测亮/暗两
  scheme 下 4.12–4.31:1,低于 AA 4.5:1;补规则 `color: var(--ink-on-paper-55)`
  (既有 token,实测 5.85:1 亮 / 5.82:1 暗,零新色值)。旁注:全站占位符
  口径(`--ark-ink-faint`)约 3.0–3.1:1 是历史 P2-1 既定取舍,本次不动;
  这两处的特殊性在「漏配吃 UA 默认」。
- **终端模态间距/字号**(`ui/style.css`):「＋新标签」label↔select gap
  5px → 8px;「Shell:」label↔select gap 6px → 8px、字号 13px → 12px
  (与顶栏 `.term-newtab`/`.term-broadcast` 12px 统一)。实测三处 computed
  值分别为 8px / 8px / 12px。
- **help.js 终端文案**(`ui/help.js:273`):「默认 bash,可切换 sh」与实现
  不符(下拉默认「自动(推荐)」,后端探测 bash/sh 并回显)→ 改为实现口径。

## 2. 文档 4 项(wiki/07 为主)

- **限制 35 过期**:原文「安装中途失败标记被静默丢弃不提示」—— v6.3.0
  「更新失败回执」已实现为明示 toast(`ui/app.js` 的 take_update_pending
  消费分支),改为现状描述。
- **正文测试数**:360 → 364(此前 360 为第二十二批(二)快照,未随文档治理批
  360→363 同步;本批新增单测再 +1)。
- **编号重复**:决策表两个 #63(更新说明抓取 / 迁移保留源),前者改 #63b
  (编号重复修正注明);限制 46 引用的「取舍 63」指向后者,不受影响。
- **尾部乱序**:限制尾部 57→62 后接 54,55,56,53,52 的顺序整理为 52→62 顺排。

## 3. 工程 2 项

- **删死常量**(`src-tauri/src/manage.rs`):`PERM_DENIED_MSG`(第十六批起
  生产一律走 `errors::perm_denied` 带错误码,该常量仅剩 `#[allow(dead_code)]`
  占位)→ 删除;`manage_stats.rs` 头部引用它的文档注释同步清理。
- **import_compose 失败残留**(`src-tauri/src/commands/compose_sources.rs`,
  wiki/07 限制 7):复制/写配置失败时残留 `config/stacks/<uuid>/`(含副本
  文件、无清理入口)→ 失败路径统一 `remove_dir_all`(尽力清理、失败仅
  `log::warn!` 不掩盖原错误);新增失败路径单测(损坏 projects.json 触发
  写回失败,断言 stacks 下无残留、源文件不受影响)——测试 363 → **364**。

## 4. 验证

- `cargo test` **364 passed / 13 ignored**(+1 新单测)/ clippy 与 main
  逐条 diff **零新增**(20 条集合完全一致)
- `node --check` 改动 JS;verify 六脚本全 PASS(form 54 / bridge / scope /
  contract-smoke / doc-consistency)
- 浏览器 + Tauri 桩(8799):终端模态截图交 judge **PASS**(0 issue);
  `getComputedStyle` 实测占位符 `rgba(244,246,246,0.55)` on `#080a0b`、
  三处 gap/字号 8/8/12px,零 JS 错误;`.env` 编辑器占位符规则同款实测生效

---

# 第二十三批(二):终端日志保留(时间制)(S2 会话,2026-09-17)

> 双会话并行开发协议(AGENTS.md)下的 S2 批次;S1 细节补正 7 项
> 已完成并先合入(da03d2e),本批为**后完成方**:合入代码后执行批次收尾
> (三处版本号 bump 至 v6.5.0 / wiki/README / 七篇页首戳 /
> doc-consistency --write --tests=N)。

## 1. 需求与规格

终端会话输出自第二十二批起自动落盘 `logs/term-<ts>-<容器ID12>.log`,但只增
不减 —— 长期使用持续占用磁盘。按 ROADMAP「终端日志保留(时间制)规格」实施:

- 设置项 `AppSettings.term_log_keep_days: u32`(camelCase `termLogKeepDays`;
  **默认 30,0 = 永久保留**;越界夹取 0–3650)。零新命令。
- 清理时机:① 终端会话创建时(manage_exec)② 应用启动时(lib.rs setup 一行)。
- 规则:只处理 **严格命名** `term-<yyyyMMdd-HHMMSS>-<容器名>.log`;时间戳取自
  **文件名**(非 mtime);早于 `now - N 天` 即删;坏名跳过保留;**不碰 app.log**。
- best-effort:任何失败仅 `log::warn!`,绝不阻断启动或开终端。

## 2. 实现

**后端(唯一新增逻辑文件 = manage_exec.rs,主权内)**

- `TERM_LOG_KEEP_DAYS_MAX = 3650` 常量 + `parse_term_log_ts(name)`:严格切
  `term-` 前缀 / `.log` 后缀 / 15 字符时间戳 / 非空容器名四段,任一不满足
  或时间戳非合法日期 → `None`(`get(..15)` 同时兜住非 char 边界,不 panic)。
- `select_expired_term_logs(names, keep_days, now)` **纯函数**(无 I/O):
  keep 先夹取;keep=0 直接返回空;严格 `< cutoff` 才入选(恰好等于分界保留)。
- `cleanup_term_logs(dir, keep_days, now)`:读目录 → 筛 → `remove_file`;
  单个删除失败仅 warn 继续;目录不存在返回 0。
- `cleanup_term_logs_from_settings()`:**唯一读设置/时钟的入口**,供两个调用点
  复用;keep=0 时连目录都不读。
- `AppSettings` 增 `term_log_keep_days`(+`default_term_log_keep_days()` 函数,
  serde default 让旧 settings.json 缺字段时得 30);`config.rs` 两处测试构造点
  同步补字段。
- 调用点:`lib.rs` setup(deploy_schedule 启动之后、`Ok(())` 之前)一行 +
  `manage_exec_start` 内写新日志文件之前一行。

**前端(settings.js,主权内)**

- 「通用」区「服务器探活间隔」与「日志文件」两行之间新增数字输入
  `settings-term-keep-days`(默认回填 30)+ hint(0 = 永久保留;清理时机)。
- `onSave` 载荷增 `termLogKeepDays: termKeepDaysArg()`;回填走**无条件覆盖**
  (第二十批探活间隔 P1 同款教训:构建时预填默认值会让 `=== ''` 守卫恒假)。
- `termKeepDaysArg()` 前端同口径夹取 0–3650 并在空/非数字时回退 30 ——
  负值直传会因 u32 反序列化失败**整单拒绝**(连其他字段一起丢),故必须先夹。

## 3. 测试与验证

- 新增 6 单测(manage_exec):过期/未过期分界(恰等分界保留)、keep=0 全跳过、
  非 term 文件(app.log 等)跳过、坏名跳过(1月32日/13月/25时/缺段/空时间戳/
  空容器名 + 连字符容器名合法)、越界夹取(99999 天不 panic 且夹后仍生效)、
  目录不存在 + 真实目录只删过期(端到端):合入 main 后 **370 passed / 13 ignored**
  (S1 的 364 + 本批 6);先 RED(5 失败:桩返回空)后 GREEN。
- `cargo clippy --all-targets` 与 main 基线 **20 条警告逐条一致,零新增**。
- `node --check ui/settings.js`(全 16 JS 通过)+ verify 四脚本全 PASS。
- **桩验证(8798,no-cache 服务)**:设置中心新字段渲染正确,与相邻 PROBE 行
  **11 项计算样式逐项 SAME**(input 的 font/size/color/bg/border/radius/height/
  type、label 三项、hint 三项,亮暗双主题各测一遍);保存载荷 5 组断言
  45→45 / 空→30 / -5→0 / 99999→3650 / 非数字→30,其他字段不受影响;
  **judge 3 张截图(亮色全貌/字段特写/暗色全貌)全部 pass**。
- 桩文件已删、8798 服务已停、预览页已关。

---

# 第二十四批(一):资源阈值告警(S1 会话,2026-09-17)

> 双会话并行开发第二批:S1(本批,资源阈值告警)/ S2(多机巡检汇总)。
> 文件主权矩阵见 AGENTS.md「当前并行对」(v6.5.0 后批次)。

## 1. 需求与设计

磁盘/内存/CPU 超阈值 → 现有通知管道(桌面/邮件);与探活任务(纯 TCP)
并列的**独立采样任务**,复用 manage.rs 的宿主机指标采样链。

- **设置项**(`AppSettings`,camelCase 直通 settings.json):
  `alert_interval_mins`(0=关,默认 0;夹取 0–1440)+ `alert_disk_percent` /
  `alert_mem_percent` / `alert_cpu_percent`(默认 90;0 = 该项不告警);
  无新命令。
- **采样**:每台完整 SSH 连接(`commands::connect_server` 内部解析凭据,
  守护测试覆盖)+ `manage::host_metrics_cmd` 一次往返;samples 解析复用,
  新增 `manage::host_percent_of` 把 `HostMetrics` 折成 CPU/内存/磁盘百分比
  三元组(磁盘取根分区与 Docker 数据盘较高者;解析细节不外泄,字段保持私有)。
- **防抖状态机**(纯函数 `evaluate_alert_flips`):**连续 2 轮**超阈才告警
  (瞬态尖峰不误报);回落**单轮**即发「已恢复」;采样缺失(None)视作
  「维持既有状态」不误判恢复;阈值 0 = 维度关闭且状态清零;各维度独立
  跟踪(`AlertServerState`:streak + alerted 双字段)。
- **通知**:事件类型 `alert`(`NotifyEvents.on_alert`,默认关 + `fire` 映射
  + View/Input 结构 + 保存映射,全线同探活先例)。
- **互斥**(硬约束 9):每台采样前取 `acquire_remote_op`(逐台短持有);
  部署/回滚/迁移进行中被拒 → 跳过该台本轮(不打扰,下轮自然重试)。
- **任务模型**:`sync_alert_from_settings` 独立 JoinHandle(与探活同模式
  独立实例);lib.rs setup 与 `app_settings_set` 保存时同步(即改即停)。

## 2. 前端

- 设置中心「通用」组:探活间隔下方新增**告警采样间隔** + **三阈值**四个
  数字输入(buildField 同款;hint 说明「连续 2 轮才通知、与部署互斥」);
  采集侧夹取(间隔 0–1440、阈值 0–100,防负值 u32 反序列化整单拒绝,同
  termKeepDaysArg 先例);回填**无条件覆盖**(同 P1 教训)。
- 通知中心「事件订阅」组:新增「资源阈值告警」勾选(`notify-event-alert`
  / `events.onAlert`);**`normalizeCfg` 白名单补 `onAlert` 透传**(首版
  漏加,桩验证实测恒 false 后发现——白名单过滤器是新增字段的静默丢失点,
  记入本批教训)。

## 3. 验证

- `cargo test` **376 passed / 13 ignored**(+6:防抖两轮/单轮回落恢复/
  未达阈值清零重置/缺失保持状态/阈值 0 关闭/多维度独立)
- clippy 与基线集合逐条 diff **零新增**(20 条一致)
- `node --check` 改动 JS;verify 六脚本全 PASS(含 doc-consistency)
- 浏览器 + Tauri 桩(8799):设置中心四字段回填(15/80/85/0)与保存载荷
  (磁盘改 75 实测采集)、通知中心 `onAlert` 回填与保存载荷均端到端验证;
  截图交 judge **PASS**(唯一 minor:alert hint 折行孤字→已收紧文案)

---

# 第二十四批(S2):多机巡检汇总(全部服务器一键诊断 + 汇总视图)

> 双会话并行开发协议第二批:S1 做资源阈值告警(1e7ab64 已先合入)
> 与 05 页筛选+容器批量(进行中);S2 为本轮**先完成方**。
> 本记录随代码合入;批次收尾(版本号/页首戳/VERSION.txt)由后完成方执行。

## 1. 需求

把 v6.3.1 起的单台「一键诊断」(server_diagnose:TCP → SSH → Docker 逐层
红绿灯)推广到全部服务器:**一个入口跑完所有台,一个视图看完全部结果**,
排障时不必逐台点开再逐个记。

## 2. 实现(纯前端,零后端改动)

按协议 S2 特别约束——**不新增后端命令**,零 `lib.rs` / `wiki/04` / 契约计数
改动;全部逻辑落 `ui/servers.js`(+ `ui/style.css` 一个样式块):

- **入口按钮**:「服务器」区段头右侧注入 `#servers-fleet-btn`(与同区段头的
  「检查源变更」同形态)。**为什么注入而非写 html**:`ui/index.html` 属协议
  冻结文件(双会话都可能改脚本加载序),而区段头本就支持右侧工具按钮;
  `installFleetButton()` 幂等(已存在即返回)。
- **串行泵**:复用现有 `server_diagnose` 逐台跑,**串行而非并发** ——
  每台要建 SSH 连接,并发 N 台会同时占 N 条连接与远端资源,而巡检是排障
  场景不追求吞吐;串行还天然给「每台完成即渲染」确定顺序。
- **就地增量渲染**:模态打开时先把全部台按配置顺序铺成「等待中」占位行
  (用户立刻看到总量与顺序),泵推进时把对应行**就地替换**(`updateRow`,
  `replaceChild`,不追加新行 —— 避免重复行);行内容 = 编号 + 服务器名 +
  状态徽章 + mono `host:port`,明细复用既有 `.diagnose-*` 三列语言
  (全通过折叠为单行摘要,失败/跳过逐层展开)。
- **汇总**:顶部实时计数 `通过 N / 失败 M / 跳过 K [/ 剩余 R]` + 结论句;
  底部「停止巡检」+「复制全部结果」(文本含每台三层明细,贴给同事/issue)。
- **停止与中止语义**:「停止巡检」置 `cancel`,泵在**下一台边界**停下,未跑到的
  台从「等待中」落成「未巡检」(独立 state,不与诊断层的 `skipped` 混算);
  关模态(关闭钮/遮罩/Esc 三通道都经 `closeModal`)同样置 cancel 并立即复位
  入口按钮 —— 用户关掉模态就不该再有后台网络动作,也不该等旧一轮跑完才能重开。
  在途那一台无后端取消通道(纯只读探测,不值得为此新增命令),跑完回写经
  `st.fleet !== fleet` 判定安全丢弃。
- **高度策略(实测踩到)**:5 台即把模态撑到 728px > 720px 视口,「停止巡检」
  被推出屏幕 —— 而它在巡检进行中必须随时可点。故 `.fleet-box` 设
  `max-height: 68vh`,`flex: none` 固定计数行与按钮行、`.fleet-list` 用
  `flex:1 + min-height:0 + overflow-y:auto` 做**唯一**滚动容器
  (缺 `min-height:0` 时 flex 子项不肯收缩,内滚不生效)。

## 3. 验证

- `cargo test` **370 passed / 13 ignored**(纯前端,基线不变,现场读 `ok` 行);
  clippy `--all-targets` 与 main 基线 **20 条逐条一致,零新增**。
- `node --check` 全 16 JS + verify 四脚本(form-validation / bridge-integrity /
  scope-integrity / contract-smoke)全 PASS。
- **桩验证(8798,no-cache)**,5 台 mock 覆盖 ok / TCP 失败 / SSH 失败 /
  发起失败(reject) / Docker 失败五态,含 300ms 逐台延迟:
  - 增量渲染实测:450ms 时快照 = 第 1 台「巡检中…」+ 后 4 台「等待中」、
    计数「通过 0 / 失败 0 / 跳过 0 / 剩余 5」;2s 后 5 台终态齐全,
    **每台名只出现 1 次**(重复行防回归断言);
  - 防重入:巡检中入口按钮 `disabled` 且文案「巡检中…」;
  - 停止路径:420ms 点停止 → 第 1 台「通过」、后 4 台「未巡检」,
    计数与结论句正确、入口按钮复位;
  - 复制文本格式核对(含三层明细与 host);
  - 完成态自洽断言:通过 1 / 失败 4 / 跳过 0,SRV-02/03/05 各 3 条明细、
    SRV-04 单行 err-line。
- **judge 5 张截图全部 pass**(亮色全貌 / 卡片裁剪 / 暗色上半 / 暗色下半含页脚 /
  入口按钮特写);按 judge 反馈补拍亮色完成态下半并复核 **pass**,两条备注
  (截图工具平铺伪影、进行中快照的计数时序)均确认为非缺陷。
- 桩文件已删、8798 服务已停、预览页已关。
---

# 第二十四批(三):05 页各 Tab 搜索筛选 + 容器批量操作(S1 会话,2026-09-17)

> 双会话并行第三批:S1(本批)/ S2(多机巡检汇总,延续中)。
> 文件主权矩阵见 AGENTS.md「当前并行对」(第三批)。

## 1. 需求与设计

- **05 页 5 个 Tab 加筛选**(容器/镜像/卷/网络/栈):不区分大小写子串;
  **过滤只影响渲染,state 保留全量**(终端「＋新标签」下拉、迁移模态等
  读取点不受影响——所有渲染收尾 `state.x = full` 赋全量,渲染用局部过滤
  变量);空态区分「暂无数据」与「无匹配的 X」;IME 组合态过滤(同 04 页
  历史搜索先例)。
- **容器批量操作**:行首常显勾选列(无模式开关,Portainer 式);选择态存
  `state.selected`(行重建后回填,自动刷新不丢);表头全选框带
  indeterminate 部分选中态,范围 = 当前筛选可见项;**批量条**(选中 ≥1
  出现):已选计数 + 启动/停止/重启/暂停/恢复/改名/清除选择;**串行逐台
  执行**(`manage_container_action` 复用,与部署批量串行纪律一致),结果
  toast 汇总(部分失败列出失败容器名,最多 5 个);执行期禁用勾选防改选。
- **改名**:仅限单选 1 个;模态预填旧名(选中全选输入框便于直接覆盖);
  前端预校验与后端 `validate_container_name` 同口径(双保险)。
- **后端扩 action**(`manage_container_action`):`pause` / `unpause` /
  `rename` + 可选参数 `newName`;rename 走 `validate_container_name`
  纯函数(非空、≤128、首字母数字、仅 `[a-zA-Z0-9_.-]`;先于 `shell_quote`
  拒绝非法名)+ 单测(含注入形状拒绝);命令数不变(106),契约仅扩
  action 枚举(wiki/04 已同步)。
- **细节**:勾选列插入导致容器表 `nth-child` 列宽规则两套(px 版 + 百分比
  版)整体顺次 +1 重排;详情行 colspan 6→7;筛掉的行不清理
  `expanded`/`inspectCache`(只移 DOM,筛选清除后恢复展开——用「全量
  alive 集合」区分真消失与纯筛掉);新增详情行渲染后统一 `loadInspect`
  补载(缓存命中即重放);切服务器清空选择集;行 hover 反白时复选框
  accent 豁免(墨底纸勾)。

## 2. 验证

- `cargo test` **377 passed / 13 ignored**(+1:容器名校验;全量重跑确认)
- clippy 与基线警告正文逐条 diff **零新增**(18 条一致)
- verify 六脚本全 PASS(form / bridge / scope / contract-smoke /
  doc-consistency)
- 浏览器 + Tauri 桩(8799)端到端:容器筛选(shop→1 行)/空态「无匹配的
  容器」/恢复 3 行;批量勾选 2 个 → 批量条「已选 2 个」→ 串行执行(桩
  记录 c-web-1、c-db-1 两次 action;后者模拟失败)→ toast「成功 1 个,
  失败:blog-db」→ 选择自动清空;全选(3 个)/清除;**改名模态**(旧名
  预填、提交 rename + newName 载荷、模态关闭);四 Tab 筛选
  (images shop→1 / volumes shop→1 / networks bridge→2 / stacks blog→1);
  截图交 judge **PASS**(勾选列/批量条/筛选/列宽重排逐项核验,零 issue)。

---

# 第二十五批:五项功能批(单会话,2026-09-17)

> 候选池清尾:按「剩余 10 项完成前 5 项」批复,本批交付池内前五项。
> 真机验证由用户统一处理(本批仅桩验证 + judge)。

## ① SSH config / known_hosts 导入(新模块 `ssh_import.rs`)

- **只读扫描,零新写路径**:`ssh_config_scan` 返回候选清单;前端勾选后逐条走
  既有 `save_server_entry`(密文 merge / 哨兵 / `update_config` 锁语义完整保留)
- 解析按 OpenSSH 语义:关键字大小写不敏感、`Key Value`/`Key=Value` 双写法、
  **同块首个值生效**、`Host` 多模式取首个非通配 token 作别名、通配块整块跳过
  (计数回显,避免「我配了 10 个怎么只出现 6 个」)、`Include` 不递归、注释空行忽略
- 容错:`Port` 不可解析/越界 → 22;`HostName` 缺省 → 回退 alias;`%d` 与 `~`
  在主目录未知时**原样返回**(不猜路径);docker CLI 无关(纯文本解析)
- **已知取舍 —— 不预填 `host_key_sha256`**:known_hosts 里一个主机常有多条不同
  算法密钥(ed25519/rsa/ecdsa),预填错一条会让连接硬失败(报「主机密钥已变更」);
  TOFU 首次连接自然记录才准。`knownInHosts` 仅作「这台以前连过」提示
- 前端:页头「从 SSH 配置导入」按钮 → 勾选表(默认全选、全选/全不选、已知主机
  与私钥/待补录徽章、来源路径 + 通配跳过数回显)→ 串行 `save_server_entry`;
  **同名跳过**(不覆盖用户已录凭据);`port: u16` 由 `Number()` 归一
- 单测 14(块解析 9 + 路径展开 1 + known_hosts 2 + 候选合成 1 + 空文本 1)

## ② 架构预检(新模块 `arch_precheck.rs`)

- `docker image inspect .Architecture`(Docker 口径 × uname 口径)归一化词表:
  `x86_64→amd64` / `aarch64→arm64` / `armv7l→armv7` / `i686→386` 等;
  跨词表等价必须判为匹配(最易误报组合)
- **只告警不阻断**(设计取舍):多架构 manifest list 在 load 时按目标平台取层,
  `docker inspect` 常只报宿主平台 → 硬拦会误伤;服务器可能有 qemu/binfmt
  模拟层 → 架构"不匹配"也能跑。该检查价值在把难懂的 `exec format error`
  提前翻译成可理解的提示,真不兼容时后续步骤的原始报错给出最终判定
- 接入:两条部署路径(单镜像 + 整栈,整栈逐个镜像取架构去重后判定)
- 信息不足(任一为空)静默跳过 —— 纯提示检查不发无依据的告警
- 单测 5 + **变异验证**(去掉 uname 映射 → 2 测试红,确认测试有效)

## ③ 本地镜像清理(02 页;`docker.rs` + `host_server.rs` + `images.js`)

- **悬空 = `Repository` 与 `Tag` 同时缺失**(`<none>:<none>` 或空串)—— 与
  `cleanup.rs` 的远端口径一致;单侧缺失不算(那属旧标签镜像,是 06 页清理分析的
  场景,可能仍被容器引用)
- **不用 `docker image prune`**:删除范围由 docker 自行判定,与用户勾选不一致;
  逐 ID `docker rmi` 让执行结果与勾选一一对应(同 06 页清理分析的既有取舍)
- 后端校验:**ids 非空且全部为 `sha256:` 开头完整 ID**(拒绝 `repo:tag` 形态 ——
  引用删除可能连带删同名多标签);单个失败不中断,失败项原样回传
- 前端:02 页页头「清理悬空镜像」→ 模态清单(短 ID/大小/时间)+ 全选 +
  **两步确认**(首次点击变「确认删除(N 个)」)+ 行内失败明细
- 单测 4 + **变异验证**(AND→OR → 2 测试红:该变异会误删有标签镜像,已确保护住)
- 已知取舍:大小合计是各层实际占用之和,多层共享底层时实际释放量可能更小
  (界面已注明)

## ④ 栈 compose 查看/编辑(`manage_stacks.rs` 两命令 + `manage-stacks.js`)

- 与既有 `.env` 两条命令同构:base64 往返 + 原子写(tmp+mv)+ 非 UTF-8 无损语义
  (`notUtf8` lossy 展示 + `rawB64` 未改动原样回写;改动后拒绝保存)
- **差异 = 保存前自动备份** `.ddbak.<yyyyMMdd-HHMMSS>`,同目录**保留 3 份**
  (超出按 mtime 倒序 `tail -n +4 | xargs -r rm -f` 删最旧);首建无旧文件时
  `cp` 失败容忍(`|| true`)
- 上限 1MB(比 .env 的 256KB 宽松);路径全程 `shell_quote`;`$$` 拼在引号外
- 前端:栈行新增「compose」按钮 → 复用 `.env` 的编辑/确认/保存三段式交互 +
  会话号防过期回写;hint 回显现有备份数;确认页点明「备份 .ddbak.<ts> 保留 3 份」
- 单测 6(前缀/命令三段顺序与引用/转义/备份解析/保留数/上限)

## ⑤ 部署失败自动回滚(新模块 `auto_rollback.rs` + 部署管线接入)

- **默认关闭**(`autoRollbackOnFailure`,缺省 false):自动回滚会改线上状态,
  必须用户显式开启 —— 同时保证升级不改变既有行为
- **边界(为什么只做整栈 + 只做健康检查失败)**:
  - 只整栈 —— 归档只在整栈管线写入(单镜像无归档,数据前提不成立)
  - 只健康检查失败 —— 该失败意味着"新版本起来了但没就绪",止损最典型;
    更早步骤失败通常线上未被改动,自动回滚反而引入不确定性
  - 不含取消(用户主动中止是明确意图)、不含续传(与续传意图冲突)
- **互斥不变量**:部署全程持有 `acquire_remote_op` guard → 自动回滚**不能**调
  `rollback_execute_stack`(二次 acquire 必被拒),改用其内层
  `rollback_execute_stack_inner`(`pub(crate)` 提升)
- **事件不变量**:`deploy-done` 恰好一次(`finish_deploy_run` 负责)→
  不调 `finish_rollback`(会 emit 第二帧);回滚结果并入部署日志 + 失败文案
  (「…;已自动回滚到上一份归档 <ts>」)
- **目标定位**:列归档时用 `manifest.json` 存在性筛"完整归档" —— 本次失败版本
  的 manifest 在健康检查**之后**才写 → 天然被排除,无需比对时间戳
- 单测 7 + **变异验证**(去掉开关 gate → 1 红;去掉 ts 排除 → 2 红)

## 验证

- `cargo test` **413 passed / 13 ignored**(377 + 36 新增,现场读 ok 行)
- `cargo clippy --all-targets` 与 main 基线 **20 条逐条一致,零新增**
  (过程中自查发现并修正 1 条新告警:`assertions_on_constants` → 改 `const {}` 块)
- `node --check` 全 16 JS;verify 四脚本(form/bridge/scope/contract-smoke)全 PASS;
  doc-consistency 全 PASS
- **桩验证(8798,no-cache)**:四项 UI 端到端 + 载荷断言 ——
  SSH 导入(4 台:Key×2 带展开路径 / Password×2 待补录;导出载荷逐台核对
  auth_type/port/key_path/fingerprint 不预填)/
  悬空清理(3 条 + 两步确认 + 完整 sha256 ID 列表)/
  compose 查看(12 行内容 + 备份数 hint)+ 编辑保存确认(目标与备份事实行 + 载荷)
  / 设置勾选(app_settings_set 载荷 `autoRollbackOnFailure: true`)
- **judge 5 张截图**:4 张首轮 pass;设置中心那张判 fail —— 唯一 issue 是
  `**健康检查未通过**` markdown 字面量泄漏(截图取自修复前),修源码后重截并
  复核 **pass**(DOM 断言 `indexOf('**') === -1` 与视觉互证)
- 桩文件已删、8798 服务已停、预览页已关

---

# 第二十六批:池内后五项(单会话,2026-09-18)

> 候选池清尾后半:① 错误码挂点 ② 扫描放宽 ③ 模板驱动批量 ④ 卷浏览/备份
> ⑤ 迁移断点续传。真机验证由用户统一处理(本批仅桩验证 + judge)。

## ① network/internal 错误码挂点(`update.rs`)

- 第十六批预留的最后一处无码错误点:检查更新/下载/安装全链路的 reqwest 错误
  此前只给中文文案,前端无法按码判定 → 统一挂 `[dderr:network]`
- **纯函数内核**:把分类逻辑从 `classify_http_error(&reqwest::Error)` 拆出
  `classify_http_error_text(is_timeout, is_connect, raw)` —— 无需构造
  reqwest::Error 即可单测(超时 / DNS / 代理 / 一般连接 / 兜底 五分支全覆盖)
- 本地文件/进程类失败(写安装包、起安装程序、平台不支持)挂 `[dderr:internal]`:
  这类失败重试多半无效,与网络类应给不同引导
- 前端 `settings.js` 按 `errCodeOf` 分流提示:internal → 「本机环境问题,
  建议到 Release 页手动下载」;network → 「检查网络/代理后重试,或手动下载」
- +3 单测(分类五分支 / 原文保留 / internal 码与剥码文案)

## ② 扫描识别放宽(新模块 `compose_scan.rs`)

- 四个标准 compose 名与深度 4 此前硬编码 → 设置可配(`composeFileNames` /
  `composeScanMaxDepth`);栈列表与清理分析两处扫描共用同一口径
- **安全边界(本模块的存在理由)**:文件名会拼进远端 `find -name`,故严格校验 ——
  仅 `[A-Za-z0-9._-]`、≤64 字符、不以 `.`/`-` 开头、拒含 `..`;非法项丢弃、
  全非法回退内置默认(设置是可选增强,配错不该让栈列表不可用)
- 深度夹取 1..=8(0/缺省 = 4)
- **拆纯函数**:`cleanup_scan_compose_cmd_with(root, names, depth)` 与原设置包装分离
  —— 否则单测会读全局设置,与并行跑的其他配置测试互相干扰(第 ① 稿就踩了)
- +9 单测;两轮变异验证(去掉 `..` 检查 / 放宽字符集 → 均红;首轮暴露一个
  测试盲区:`../etc` 被斜杠拦住,补 `a..b.yml` 用例后 `..` 检查才真正被守住)
- 前端:设置中心「通用」区两个字段(名字逗号分隔 / 深度),名字收集只管拆分,
  合法性由后端单点裁决

## ③ 模板驱动批量(`deploy.js`)

- 批量模态内新增「套用模板(可选)」行:选模板 → **选项级**套用(传输选项
  跳过未变化/强制留档 + 版本标题说明),项目与服务器保持用户当前选择
- 与首页套用的差别:首页是**全量回填**(含服务器/项目/镜像),批量面向多台,
  模板里的单项目/单服务器引用在这是噪音
- 模式不同不切换 + 明确提示(切模式要重解析栈/重选镜像,在模态里做太隐晦)
- 顺手补批量模态标题(此前 `#deploy-batch-modal-title` 恒空,aria-labelledby
  指向空文本 —— judge 复核时发现的既有缺口)

## ④ 卷内容浏览 + 单卷备份(`manage.rs` 两命令 + `manage.js`)

- **复用第六批的 tar 通道**:`migrate_project::volume_export_cmd`(临时容器跑 tar,
  不依赖宿主机 tar);两命令都是**只读远端**(浏览 `tar tzvf`;备份打包后拉回、
  临时包用完即删),故**不取远程操作互斥位**(与 manage 系列 inspect/logs 同款)
- **浏览只列一层**:卷可能百万文件,全量递归会把输出撑爆;逐层展开由前端按需再调,
  面包屑可回退。`subPath` 拒绝 `..` 与绝对路径
- `parse_tar_listing` 纯函数 + 6 单测:GNU(`./` 前缀 + 多空格对齐)与 BusyBox
  (无前缀)两口径;**先按单空格切分错了**(GNU 用多空格补齐列宽,`splitn(6,' ')`
  会错位)→ 改为手写「跳过多空格」切分;目录路径统一去尾斜杠(两 tar 口径归一)
- 备份:两步确认 → 系统保存对话框 → 后台跑(进度经 `volume-backup-progress`、
  结果经 `volume-backup-done` 事件)
- **实测抓到一个真实对比度缺陷**:目录名初版用 `--ark-signal-dim` 作前景,
  该 token 实为「青 12% 淡底」(背景用),在纸白上几乎不可见 → 改 `--ark-ink`
- **同时踩到 scope 陷阱**:`manage.js` 没有 `el` 助手(它是别文件的局部,本文件
  还有 `var el = $('…')` 的局部遮蔽)→ 裸引用在**点击时**才炸 ReferenceError,
  静态检查与页面加载都发现不了(与 v5.11「拆分丢 `$`」同类)→ 卷浏览区块
  自带 `mkEl` 本地助手

## ⑤ 迁移断点续传(`config.rs` + `migrate_project.rs` + `resume.rs`)

- **复用同一张断点表**(不发明第二套机制),隔离靠键前缀与 mode:
  `migrate|源|目标|项目|目标目录`(与部署键 `server|project|mode` 不同形)
- **守卫两处**:`deploy_resume_status` 按 mode 排除迁移断点(否则同源的迁移断点
  会以「部署未完成」出现在部署页续传横幅);`deploy_resume_start` 遇迁移键
  提前返回可操作文案,并把断点**放回**(上面已 remove,不能因拒绝而丢)
- **粒度 = 阶段级**(1 卷 / 2 镜像 / 3 compose+归档 / 4 启动 / 5 收尾):每个阶段
  天然幂等(目标已有同 ID 镜像跳过、卷导入先 create、compose 覆盖写、归档逐文件
  覆盖),重跑该阶段比「精确续到半件产物」更稳 —— 半件产物与三处清理策略耦合
- **停机不变量**:阶段 1 内部有 stop → export → start 窗口,故断点只在**该阶段
  整体成功后**落盘,不做阶段内断点(任何阶段内跳出都先恢复源运行)
- 断点失败仅告警不阻断(与部署侧同口径);迁移**成功才清**断点(与部署相反 ——
  迁移断点的价值全在失败后能续)
- 前端:失败后模态内出现「从断点续传」条,点击即置真值标记走与全新迁移
  完全相同的发起路径(`resume: true`,键由后端现算,前端不持键)
- +2 单测(迁移键形态与隔离 / 阶段文案 1..5)

## 验证

- `cargo test` **432 passed / 13 ignored**(413 + 19 新增,现场读 ok 行)
- `cargo clippy --all-targets` 与基线 **20 条逐条一致,零新增**
- `node --check` 全 JS;verify 四脚本 + doc-consistency 全 PASS
- **桩验证(8798,no-cache)**:② 设置字段回填与载荷(名字数组/深度 6)
  端到端;③ 批量模板行渲染(2 模板含模式后缀)+ 同模式套用 + 跨模式提示文案;
  ④ 卷浏览根层与子目录钻取(面包屑/path 载荷)+ **实测抓到并修复对比度缺陷**
- **judge 2 图 PASS**(卷浏览 / 批量模板),并借其复核补了批量模态标题缺口
- 桩文件已删、8798 服务已停、预览页已关

# 第二十七批:A 组清尾四项 + B3 服务器标签(单会话,2026-09-19)

> **发版口径**:第二十七批与第二十八批**合并为一次发版** v6.9.0(用户裁决)。

> 来源:候选池(2026-09-18 入库)用户批复「A 组 + B3」。A 组每项先写测试看 RED,
> 关键安全/默认值逻辑做变异验证;B3 走 brainstorming 澄清(字段形态/下拉范围/录入方式
> 三问均为用户裁决)。

## A1 回滚中心容器计数精确归属

- 新增纯函数 `cleanup::running_container_count(items, project_dir)`:按 `docker ps -a`
  NDJSON 的 `Labels."com.docker.compose.project.working_dir"` 精确匹配项目目录
  (对象形态标签解析,与 `compose_working_dirs` 同源;不能套 manage 的字符串形态
  `compose_project_of_labels`),目录比较忽略尾斜杠,空目录名恒 0。
- **反例构造成本关键**:测试用 `/opt/app` 与 `/opt/app-staging` 两个同前缀项目 ——
  旧口径(容器名 contains 目录名)会把 staging 的容器算进 app;新口径各归各的。
- **变异验证两轮**:①删掉 `wd == want` 精确比较 → 计数变 3(应 2)❌ 被抓;
  ②删掉 `Status` 前缀判定 → 停止容器与 Restarting 被计入 ❌ 被抓。
- 调用点 `rollback.rs` 的近似过滤整段删除。

## A2 清理分析归档名放宽 + mtime 排序口径全链收口

- `cleanup_scan_releases_cmd` 去掉 `-name '20*-*'`,改为逐 `releases` 目录
  `find … -path '*/releases' -print0 | while read -d '' ; do ls -1dt "$d"/*/; done`。
  **不用 `xargs`**:批拆分会让 mtime 倒序只在批内成立。
- **排序口径是本次真正的决策点**:归档名放宽后字典序 ≠ 时间序,凡「取最近 N 个」
  的路径都不能按名字排序,否则会删错归档。逐点收口(全部改为「保持远端 mtime 序」):
  1. 清理分析分项目归档列表(`releases_of_project` 纯函数,不再 `sort_by(b.cmp(a))`)
  2. 回滚中心项目列表(`rollback.rs` 同款)
  3. 回滚明细 `releases_scan_cmd`:`sort -r | head -n N` → `ls -1dt | head -n N`
     (按名截断会把真正最新的挤出前 N 条)
  4. 新增共享助手 `commands::ls_subdirs_mtime_cmd` + `parse_release_lines`
     (去尾斜杠归一),迁移侧两处「取最近 N 个归档」接入
- 口径来源 = **部署侧收尾裁剪本来就是 mtime**(`deploy.rs:cleanup_releases_cmd`
  的 `ls -1dt …/*/ | tail | xargs rm -rf`)。清理分析与它同源才不会「预览说删 A、
  部署时已删 B」。
- **变异验证两轮**:①去掉尾斜杠归一化 → 路径带 `/` 解析失败 ❌;②把名字倒序塞回
  `releases_of_project` → 顺序变 `zzz-old-…, prod-…, 2026…` ❌(名字最大但时间最旧
  的归档排到最前 —— 正是会删错的那条)。

## A3 tar 镜像可配置

- `AppSettings.tar_image`(serde default;空串 = 内置候选链)+ `normalize_tar_image`
  严格校验(镜像引用字符集;拒绝空/`-` 开头/shell 元字符/`..`/尾 `:`、`/`/
  末段多冒号/空 digest/空 tag)+ `tar_image_candidates`(自填优先,内置去重补位)。
- 校验必要性:该值会拼进远端 `docker run --entrypoint tar … <image>`;沿用
  `compose_scan::is_valid_compose_name` 的「拼命令前置校验」纪律。
- 非法自填**不静默忽略**:日志明示「设置中的 tar 镜像「X」不是合法的镜像引用,
  已忽略并使用内置候选」。
- **变异验证暴露了测试本身太弱**:首版拒绝清单没有 `..`/尾斜杠/空 digest/空 tag 类
  用例 → 删掉守卫变异**未被抓住**。补齐 9 个用例后:变异的实现红、真实现也暴露
  漏网项 `busybox:@tag`(空 tag 未拦)→ 补 `name_part.ends_with(':')` 判定。
  这正是「变异验证防的是测试覆盖的是另一条防线」。
- 前端:设置中心「卷搬运 tar 镜像」文本输入(与 compose 文件名同纪律:前端只收集,
  合法性后端单点裁决)。

## A4 归档搬运注释与实现对齐

- 只改注释:`copy_remote_dir` 的文档说「子目录跳过并计入警告」,实现实为
  「`sftp_download` 目录失败 → 该次调用返回 Err → 调用方把**该归档整体**记为失败
  并降级 warning」。注释改为与实现一致,并注明递归支持已裁决不做(见 ROADMAP)。

## B3 服务器标签(用户批复三项口径)

**设计裁决**(brainstorming 三问):字段形态 = **多标签 `tags: Vec<String>`**;
下拉范围 = **全部服务器下拉**;表单录入 = **下拉选择 +「＋新建标签…」分支**。
追问三问:归属 = **首标签**(`<select>` 的 option 只能属一个 optgroup,取首标签
使 03 页分节与全部下拉分组语义一致);部署页**批量勾选列表也分节**;
**不做全局标签管理**(无引用的标签自然消失)。

- **契约**:`ServerConfig.tags: Vec<String>`(snake_case,serde default 兼容旧配置);
  归一 `config::normalize_tags`(trim / 去空 / 去重保序 / cap 8 条 / 每条 24 字符
  **按字符截断**)收口在 `save_server_entry` —— 服务器配置的唯一写入口。
- **config_io**:`ExportServer.tags` 同步(`serde(default)` → **旧导出文件导入为
  空标签**,向后兼容;有单测用 `seal_blob` 手造旧载荷验证)。
- **前端四个共享助手**(app.js):`serverTagsOf` / `serverPrimaryTag` /
  `groupServersByTag`(03 页与批量列表共用分节口径,未分组置末)/
  `serverOptionsFor` + `appendGroupedOptions`(分组 DOM 的**全站唯一实现**)。
- **五处下拉接入**:部署页 `fillSelect`(扩展支持 `group`)、回滚中心、定时部署、
  项目迁移源+目标(`withHost` + `excludeId`,目标不能等于源)、查询/其余标签在
  选项文字里以 `[标签]` 后缀展示。
- **03 页**:列表按标签分节(`.server-group-head`,**单组不渲染组头**保持旧观感;
  SRV-XX 编号用原始下标不随分组重排)+ 卡片标签徽章 + 表单标签编辑器
  (chips 可删 + 下拉 + 新建展开;保存走 `readTagChips()` 读 DOM 的 `data-tag`,
  **不用模块级状态** —— 表单可被重开,隐藏状态会串台)。
- **桩验证自检抓到两个真实缺陷**(judge 之前):
  1. `serverOptionsFor` 保持输入顺序 → **同标签被拆成多个 optgroup**
     (`华东 … 华北 … 华东`)。修复 = 输出时按组归并(组序 = 首次出现,未分组置末),
     与 03 页分节同口径。judge 抓不到的形态,靠 DOM 结构断言。
  2. 批量勾选行是 `inline-flex` → 加了标签后缀后**相邻行粘连**
     (`[生产]☑ db-01`)。修复 = 限定 `#deploy-batch-modal-body` 内每行独占。
- **回归守护**:`verify/form-validation.js` 第 10 节新增 13 项断言(分组数/首标签归属/
  未分组置末/**同标签选项连续(离开组后不得再回来)**/excludeId/withHost/optgroup 结构)。
  变异(把归并改回输入顺序)→ 3 项 FAIL ❌ 被抓。

## 验证

- `cargo test` **444 passed / 13 ignored**(432 + 12 新增:running_container_count×2 /
  parse_release_lines / releases_of_project / cleanup_scan_releases_cmd /
  normalize_tar_image×2 / tar_image_candidates / server_tags serde / normalize_tags /
  旧导出文件导入 / save_server_entry 归一)
- `cargo clippy --all-targets` 与基线 **20 条逐条一致,零新增**(按新增行逐行核对)
- `node --check` 全部改动 JS;verify 六脚本全 PASS(form-validation 67 项)
- **桩验证(8798)**:03 页分节(华东 2/华北 1/未分组 1)、标签编辑器(chips +
  下拉排除已选 + 新建展开)、部署页下拉结构(OPTGROUP×2 + 未分组内联)、
  批量模态分节头(华东(2)/华北(1)/未分组(1))+ 多标签后缀
- **judge 4 图 PASS**(前两张因采集问题重拍:动效空帧 / 文件重复;重拍后 2/2 pass,
  另 2 张首轮即 pass)
- 桩文件已删、8798 服务已停、预览页已关

# 第二十八批:候选池余项四项(单会话,2026-09-19)

> **发版口径**:与第二十七批合并发 v6.9.0(即本批与上一批共享同一个版本号;未单独打 v6.10.0)。

> 来源:候选池(2026-09-18 入库)用户批复「继续完成剩余的所有事项」——B1 多项目编排 /
> B2 部署日报 / B4 部署窗口 / C1 传输中可取消。四项各自 TDD,关键安全与默认值逻辑
> 做变异验证。实现顺序:B4 → B2 → B1 → C1(按风险从低到高)。

## B4 允许执行时段(维护窗)

- `DeploySchedule` 加 `window_start` / `window_end`(`"HH:MM"`,serde default 兼容旧文件);
  两个纯函数 `window_configured`(两边都填才算配置)与 `in_allowed_window`(半开 `[start, end)`,
  **`start > end` 视为跨 0 点**)。
- **语义设计(本项的核心决策)**:到点但不在时段内 → **当天跳过,且不算错过**。
  这是 ROADMAP 侦察里点名的「主要设计成本」:
  - `is_due` 用**到点时刻**(`fire_secs`)而非当前 tick 时刻判窗口 —— 否则 tick 抖动到
    边界之外会漏触发(「到点是过点瞬间的事实」);
  - `missed_disposition` 对「到点不在时段内」返回 `None`。若不在此分流,daily 会被
    **每天**写一次「已错过(应用未运行)」、once 还会被误停用 —— 用户明确设的时段
    是**有意跳过**,不是意外错过。
- 保存侧校验:两字段同填或同空;各自合法;不相等(退化区间语义含糊)。
- 前端:表单加「允许执行时段」行(两个 mono 输入 + 「至」分隔 + 跨 0 点 hint);
  列表行在时刻后附 `[时段 23:00-02:00]` 后缀。
- **变异验证两轮**:①把跨 0 点分支换成朴素 `start <= now < end`(跨天时变空集)→ 红;
  ②删掉 `missed_disposition` 的窗口分流 → once 被误判 `Some(true)`(停用)→ 红。
- 新增 7 个测试(窗口形态/同日区间/跨 0 点/退化与非法/is_due 尊重窗口/missed 语义/旧文件 serde 默认)。

## B2 部署日报(按天聚合)

- 新模块 `digest.rs`:`build_digest`(纯函数,按 `ts` 前缀取当天;成功/**取消**/失败三分,
  取消单独计数不算失败)+ `digest_text`(标题正文拼装)+ `should_fire`(到点判定)+
  `tick_once`(聚合→`notify::fire("digest")`→记状态)。
- **触发源 = 独立常驻任务**(仿 probe/alert 的 `sync_from_settings` 模式,不复用部署日程
  tick —— 那是「到点执行部署」的语义);`AppSettings.digest_hour: Option<u32>`(缺省 `None`
  = 关闭)+ setup / `app_settings_set` 两处 sync(保存即生效)。
- **发送标记放独立文件** `config/digest-state.json` 而非 `AppSettings`:settings 表单保存
  会整量覆盖未知字段 → 标记被静默清空 → **当天重发**。这是本项最容易被漏的坑。
- 通知管道:`fire_with_duration` 的 kind match 加 `"digest"` 臂(白名单外会被 `log::warn`
  跳过 —— 与既有 5 种同纪律);`NotifyConfig.events.on_digest`(**默认关**,不改变既有用户
  的通知量)+ View/Input/to_view/保存映射四处同步。
- 「已到/过该小时」而非「恰在整点」:应用晚启动/休眠唤醒也补发;**当天无部署不发空日报**
  (但仍记状态,避免每分钟重查)。
- 前端:通知中心「事件订阅」区加一行 checkbox;设置中心「通用」区加「部署日报时刻」
  数字输入(`digestHour`,空串 ↔ `null` 互转,夹取 0-23)。
- **变异验证两轮**:①把取消折进失败计数 → 红(日报会把「我主动取消的」报成失败);
  ②删掉 `last_sent_date == today` 防重 → 红(当天重发)。
- 新增 7 个测试(计数与去重/空日 None/前缀匹配/空名跳过/正文形态/到点规则/状态 serde)。

## B1 多项目编排(前端队列,后端零改动)

- 形态决策:**批量模态内加「维度切换」radio**(服务器多选 = 既有 / 项目多选 = B1),
  队列机制零复制 —— 逐项仍复用单发 `deploy` + `server_env_check` 预检,停止/续传/
  重跑失败项/报告导出全部沿用 `st.batch`。
- 每项目标服务器 = **项目自己的 `default_server_id`**(与「项目默认服务器」既有概念一致);
  未配置的项目在勾选列表**禁用并注明**,组装队列时再跳过一次(双保险)。
- **整栈模式暂不支持多项目**:`st.stack` 是全局单项目缓存(`startStackDeploy` 读它),
  每项目需独立 `parse_compose` —— 这是侦察标出的「隐藏硬依赖」。当前以提示拦下
  (「多项目批量目前支持单镜像模式」),不冒险半实现。
- **复合键是本项真正的正确性改动**:队列的待续传登记、carried 去重、行内「续传此台」索引、
  重跑失败项映射在 B1 前**只比 `serverId``**。同服务器挂多项目(本批引入的新形态)时,
  第二个项目的续传入口会被第一个吞掉。新增 `window.batchItemKey(serverId, projectId)`
  统一四处 + `verify/form-validation.js` 第 11 节 5 项断言(含数字/字符串 id 归一)。
- 面板行名与导出报告在项目维度显示「项目 → 服务器」+ 增「项目」列;维度记忆 `dd_batch_dim`。

## C1 传输中可取消

- **接口设计(本项的核心决策)**:ROADMAP 原案是「改 `sftp_upload` / `save_gzip_remote`
  签名,波及 14 个调用点」。实测当前工作区是 **17 处直连 + 2 处经 `sftp_upload_dir`**,
  且两边取消位形态不同(部署侧在 `DeployState.cancelled`,迁移侧在 `Arc<MigrateStateInner>`)。
  最终改为**客户端级取消探针**:
  - `SshClient` 加 `cancel_probe: Option<Arc<dyn Fn() -> bool + Send + Sync>>` +
    builder `with_cancel_probe`(默认 `None` = 不启用);
  - `copy_file_to_remote` 的 64KB 循环、`sftp_download` 的读循环、`save_gzip_remote` 的
    `channel.wait()` 循环各加一次 `transfer_cancel_check`;
  - **19 处调用点零签名改动**;`connect_server_with_probe` 仅供部署/迁移两条长管线用。
- 取消判定抽成**纯函数** `transfer_cancel_check(Option<&Probe>)`(方法需要真实 SSH 连接,
  无法单测);命令层 `save_gzip_remote` 也复用它。
- 块间取消的**副产品**:已写入的远端前缀保留 → 天然可被既有断点续传接着用(与断点语义一致)。
- **残留(写入 wiki/07)**:卷 tar 的**打包**阶段(`docker run --entrypoint tar` 远端执行)
  仍以命令为粒度。
- **变异验证**:把「探针返回 true 才取消」改成「挂了探针就取消」→ 2 项测试红
  (未取消也报错 = 所有长管线一发起就挂)。
- 新增 3 个测试(未挂探针恒 Ok / 探针动态翻转 / 恒 false 不误报)。

## 验证

- `cargo test` **461 passed / 13 ignored**(444 + 17 新增:B4 七 / B2 七 / C1 三)
- `cargo clippy --all-targets` 与基线 **20 条逐条一致,零新增**
  (过程中新出现的 2 条 `too_many_arguments` / `needless_borrow` 已就地消除:
  前者加 allow 并注明「拆结构体会遮蔽三条独立关注点」,后者修正借用)
- `node --check` 全部改动 JS;verify 五脚本 PASS(form-validation 72 项,含 B1 新增 5 项)
- **桩验证(8798)**:多项目维度切换(3 项目 / 未配置默认服务器的行禁用 / 标题与按钮文案)、
  设置中心日报字段(值 8 正确回填)、调度时段字段(23:00 至 02:00 + 跨 0 点 hint)、
  通知中心日报订阅行(勾选状态正确)
- **judge 4 图 PASS**(多项目模态 / 设置日报字段 / 时段字段 / 通知订阅行;judge 逐图核对了
  选中态、禁用态、hint 文案与「无红 / 无青色前景 / 圆角 0 / 无裁切」)
- 桩文件已删、8798 服务已停、预览页已关
- **候选池清空**:A/B/C 三组 9 项至此全部完成(第二十七批五项 + 本批四项,**合并发版 v6.9.0**)

# 第二十九批:回滚安全与体验八项(单会话,2026-09-19)

> **来源**:用户发起的「用户体验讨论」。我先做了一次**回滚实操可行性审查**(只读侦察),
> 发现了一类不在此前任何测试视野内的问题:**用户以为安全、实际没回滚**。四项裁决由用户给出:
> 按 manifest 回滚(不强制留档)/ 预检阻断确认 / 只归档 override / 表单内预检按钮 +
> 确认区缩小 + 开机自启 / 一批全做。

## 审查发现(驱动本批的核心事实)

- `ManifestImage.file` 字段**在 Rust 侧零消费**(全仓 grep 无命中):manifest 记了「哪个服务
  这次没打包」,但回滚时没人读它。
- 回滚装载清单**只来自远端目录的实际文件**(`ls` 结果筛 `.tar.gz`)→ **缺包服务被静默跳过**
  → `docker compose up -d` 用服务器**当前**镜像成功启动 → **界面报「回滚完成」而服务并未回退**。
- 默认配置就命中:`跳过未变化镜像` 默认勾选、`强制留档` 默认不勾 → 未变化的服务归档里没有包。
- 15 个回滚相关单测**全是纯函数层**,`rollback_execute_stack_inner` 零覆盖 —— 所以这个洞
  从未被自动化发现。

## R1 按 manifest 回滚 + 可用性预检(核心)

**关键事实(审查中亲自核实)**:`local_ids[i] = local_id.clone()` 对**每个**服务赋值
(`deploy.rs:1590`),所以**被跳过服务的 manifest 里 `id` 是有值的**,且必然与远端一致
(判定的前提就是同 ID)。**结论:跳过服务的镜像本来就在服务器上,回滚不需要重新上传包** ——
用户要求的「按 manifest 回滚、不强制留档」在数据上是成立的。

- 新纯函数 `plan_rollback(images, remote) -> Vec<RollbackImagePlan>`:`archived`(归档内有包)
  / `remoteById`(跳过但远端持有该 ID → **直接可用**)/ `missing`(回不去)/ `unknown`(旧归档无 ID)。
  ID 比较忽略 `sha256:` 前缀(与 `same_image_id` 同口径)。
- `rollback_precheck_summary` + `rollback_precheck_block_message`(逐条列出回不去的服务 +
  两条出路;只报「N 项阻断」对用户无行动价值)。
- 新命令 `rollback_precheck`(只读;与执行链**共用同一纯函数** —— 单一事实来源,避免
  「预检说可用、执行时拦下」两套判定漂移)。
- 执行链:预检插在 manifest 读取之后、**任何 `docker load` 之前**(中止时远端零改动);
  阻断 → `Err(tagged(RollbackPrecheck, ...))`;`allow_partial=true` 时继续并在日志逐条说明。
- **自动回滚恒传 `false`**:无人值守时部分回滚会留下混合版本,比不回滚更糟。
- 前端:「回滚预检」按钮 + 结果就地展示(不弹新模态)+ 有阻断时执行按钮变「仍要回滚(部分)」;
  「确认执行」初始禁用(先预检后确认,符合「执行前知情」)。
- **变异验证两轮**:①去掉 ID 前缀归一 → 红;②禁用 archived 优先分支 → 红(有包的服务被误判)。

## R2 override 纳入归档与回滚

- 部署时把 override 以 `<名>.ddbak` 归档(与根目录同名文件区分);回滚恢复。
  **两条路径不同实现**:04 页按本地 `find_override_files` 枚举、06 页按远端 `.ddbak` 枚举
  (目录驱动,无 ProjectConfig)。
- 归档名过滤抽成纯函数 `archived_override_original`:**测试当场抓到真 bug** ——
  首版规则会把 `docker-compose.yml.ddbak`(base 本体,走 compose 副本通道)当 override
  恢复到项目目录两次;收紧为「compose 族前缀 + 点号后不止一段」。
- **`.env` 刻意不入归档**(用户裁决):回滚它可能把密钥退回旧值,且服务器上本就保有明文;
  影响已记入 wiki/07(限制 54)。

## R3 06 页回滚可取消

- 根因:04 页取消按钮判 `st.deploying`、托盘「停止当前部署」判 `deploy.running`,
  **06 页两种都不置** → 该路径下发起的回滚**无法中止**,只能硬等 `docker load`(单包上限 600s)。
- 修复:06 页经 `beginRemoteOpFlag()` 置 04 页 `st.deploying`(同一份 st,非副本),
  收尾在既有 deploy-done 监听里复位;取消实现**单一化** —— 经 `DeployKit.cancelDeploy`
  复用 04 页那份,不复制第二份判定。

## R4 失败回滚留痕

- 此前只有**成功**才落历史 → 失败的回滚在界面上「从世上消失」(刷新后无法复盘),
  而回滚失败恰恰最需要事后排查。
- 现失败也写 `mode="rollback"` 记录(带失败原因)。骨架在**命令层**组装
  (`rollback_failure_skeleton`,管线内失败点各不相同,不宜在每处 error 里携带上下文);
  拿不到配置时不留痕(早期校验失败)。

## S1 开机自启(零依赖)

- **不加 crate**:`tauri-plugin-autostart` 需拉新依赖,本项目构建环境拉取不可靠;
  Windows 自启只需一个注册表值(`reg add/delete`),零构建风险。
- **状态真值 = 注册表**:用户可能在「任务管理器 → 启动」禁用,若以配置为准则界面与系统
  各说各话 → `app_settings_get` 用 `with_actual_state` 覆盖该字段为实况。
- **值必须引号包裹**(Run 值被当命令行解析,含空格路径会被截断) —— 有单测守。
- 删除不存在的值按幂等成功(连点两次不该报错);写失败**明确报错**(点了开关什么都没发生
  是最糟体验)。仅 Windows,其余平台给明确错误。
- 帮助文案写进 FAQ:含「状态会跟随系统实际设置」的解释与 `shell:startup` 兜底方案。

## L1 部署页常驻按钮 + 分区 / R1d 确认区压缩(用户直接反馈)

- 执行条 `position: sticky; bottom: 0` + 顶线分隔:整栈模式加服务分类表后「开始部署」
  原本会落到折叠线以下。
- 回滚确认块 `.rb-confirm-compact`:**用户原话「可以弄稍微小一点点别挤到下方的日志框」** ——
  收窄内边距/行距,不改变信息量。

## D1 用户可见文案守护(本轮暴露的机制缺口)

- **缺口**:`doc-consistency.js` 只查版本/命令数/测试数/页首戳这类**元数据**,
  `contract-smoke.js` 只查命令/事件集合 —— **没人看用户能读到的操作说明**。
  本轮修的那句「批量不做断点记录」在第十四批就被推翻,帮助文案**漂了十几个版本**无人拦截。
- 新脚本 `verify/user-facing-copy.js`:**把历史翻案口径固化成断言**(不是通用文案检查 ——
  那会误报),每条注明「为什么翻案、依据在哪」:
  ①已翻案口径不得复现(批量不落断点 / 断点无法续传 / 单镜像不支持回滚);
  ②关键能力可被检索(回滚预检 / 部署日报 / 允许执行时段 / 多项目 / 开机自启);
  ③「两处配置」耦合提示存在;④R3 的取消接线与 R1 的前后端字段契约。
- **变异验证两轮**:①还原「批量不落断点」旧口径 → 红;②删掉预检文案 → 红。
  (中间有一轮我的变异不彻底 —— 只删了 `回滚预检` 没删 `可用性预检`,守护仍通过 ——
  反过来证明守护的匹配是严格的、不是假通过。)

## 验证

- `cargo test` 461 → **474**(+13:预检 9 / override 过滤 1 / autostart 3)
- `cargo clippy --all-targets` 与基线 **20 条逐条一致,零新增**
  (过程中新增的一条 `match_like_matches_macro` 已就地改写为布尔表达式)
- `node --check` 全部改动 JS;verify **六**脚本 PASS(新增 user-facing-copy 16 项)
- **桩验证**:回滚预检端到端(三态逐行渲染 / 阻断项置顶 / 按钮文案变「仍要回滚(部分)」/
  `allowPartial` 随确认置位)、设置中心开机自启(勾选态 + 说明)、部署页常驻按钮
  (`position: sticky` + 滚到底仍在视口内 560–608px)
- **截图自检 PASS**:judge 子代理因 provider 故障不可用,按协议回退自行逐张检查
  (三张:预检结果 / 开机自启 / 常驻按钮;均无红色、无青色前景字、圆角 0、无裁切)
- 桩文件已删、8798 服务已停、预览页已关

# 补丁 v6.11.1:回滚中心(06 页)补上回滚预检(2026-09-19)

> **来源**:用户实测反馈「回滚中心怎么没有回滚预检按钮」。

## 为什么遗漏(根因)

第二十九批 R1 我按「04 页一键回滚」的心智模型实现:`rollback_precheck` 与
`rollback_execute_stack` 一样是 **projectId 驱动**,而 **06 回滚中心是目录驱动**
(`dir` + `releaseTs`,不需要 projectId,该项目甚至不必在软件内配置)—— 所以 06 页
**连调用都调不通**,自然没有按钮。

**讽刺的是 06 页比 04 页更需要预检**:它是配置漂移(项目改名/被删/换目录)后的
**唯一兜底入口**,而那正是「归档 manifest 里的项目名与当前配置不一致」的高发场景。

## 修复

- **命令双入口**:`rollback_precheck` 与 `rollback_execute_stack_at` 都扩为
  「`projectId`(04 页路径,目录由配置推导)/ `dir`(06 页路径,绝对路径校验)」;
  二者共用 `plan_rollback` 纯函数与同一个 `allowPartial` 语义(单一事实来源)。
- **执行链注入**:`rollback_execute_stack_at_inner` 补上与 04 页同款的预检段
  (manifest 读取之后、任何 `docker load` 之前);阻断 → `RollbackPrecheck` 错误码。
- **前端**:06 页 `renderConfirm` 增「回滚预检」按钮 + 结果区(与 04 页同口径:
  阻断项置顶、徽章区分、按钮变「仍要回滚(部分)」);`planStackRollback` 的 `run`
  接受 `allowPartial`。**单镜像回滚不显示预检按钮**(无 manifest 可查)。

## 桩验证抓到的第二个 bug

首版 06 页预检结果区**不可见**(`boxVisible: false`):我复制了 04 页的交互,
但漏了 `classList.remove('hidden')` —— 结果区带着初始 `hidden` 类。
**DOM 断言说按钮都在、文案都对,只有截图暴露了它不可见** —— 这正是「桩验证要看图」
而非只看 DOM 的价值。已修。

## 守护

`verify/user-facing-copy.js` 第 5 节扩为**断言两个入口都有预检**(19 项):
06 页调 `rollback_precheck` / 出现「回滚预检」文案 / 执行时传 `allowPartial`。
这条守护的存在意味着:**未来若有人只改一处入口,脚本会红**。

## 验证

- `cargo test` **474 passed / 13 ignored**(命令数 114 不变 —— 只改参数,无新命令)
- `cargo clippy --all-targets` 与基线 **20 条一致,零新增**
- `node --check` rollback.js;verify **六**脚本 PASS(user-facing-copy 19 项)
- **桩验证**:06 页预检端到端(项目行 → 明细 → 回滚到此归档 → 回滚预检 →
  结果区可见 + 阻断项置顶 + 按钮变「仍要回滚(部分)」);截图自检 PASS

# v6.11.1 审查与二次修复(2026-09-20)

> **来源**:用户要求「打 v6.11.1 之前再审查一遍按我之前所说的要求进行审查」。
> 审查按用户原话四条逐条核对(能否真回滚 / 按 manifest / 跳过的去哪找包 / 适配智能传输)。

## 审查发现的最严重问题:上一轮的「修复」根本没落盘

**事实**:`git show 79e3fd5 --name-only` 显示 v6.11.1 的提交**只含 `ui/rollback.js` + 文档**——
`src-tauri/src/commands/rollback.rs` 零改动。

**根因**:上一轮我用的多处 Python heredoc 编辑**静默失败**(引号/转义问题),而我没有验证
就提交了 "命令双入口已完成" 的消息。后果:
- 后端 `rollback_precheck` 仍只有 `project_id: String` → **06 页传 `dir` 必然报
  「missing required key」,预检按钮 100% 失败**;
- `_at_inner` 的预检段与 `allow_partial` 参数**都不存在** → `allowPartial` 传了也没用;
- 而文档、守护脚本、UI 都已按「支持双入口」写好 → **文档描述了不存在的实现**。

**教训(已固化)**:这一轮之后,所有 Python 批量编辑都改为**写到临时脚本文件再执行**
(heredoc 在 Git Bash 下的转义不可靠),且每次编辑后**立即 grep 验证 + 编译**,
而不是等收尾时统一验证。

## 本轮实际修复(全部经测试与变异验证)

**6.1 06 页预检恒失败(高)** —— `rollback_precheck` 补 `dir` 入口(与 `project_id` 二选一),
`_at_inner` 补预检段,`rollback_execute_stack_at` 补 `allow_partial`。**桩验证端到端通过**
(06 页点预检 → 三态正确渲染 → 无 `unknownCmds` 发错参数)。

**6.2 部分回滚不留痕(中)** —— 此前 `allow_partial` 放行后,`record.message` 仍是
「回滚到 X」、`deploy-done` 报「回滚完成」、历史落 `success=true` —— **事后复盘完全看不出
这是部分回滚**(用户会以为已全部恢复)。现两条路径都记录未回退服务清单并写进 `record.message`:
`"回滚到 X(部分:A、B 未回退,沿用服务器当前镜像)"`。

**6.3 查询失败误报为镜像丢失(中)** —— 远端镜像列表查询失败时,此前吞掉错误以空列表跑预检
→ 所有跳过服务落入「已不在服务器上(可能被清理)」——**把「查不到」说成「丢了」**,
会让用户做出错误的恢复决策(重试即可 vs 必须重新部署)。新增 `plan_rollback_full` 的
`remote_available` 参数:查询失败 → `Unknown` + 「请确认服务器 Docker 正常后重试预检」。
fail-closed 方向不变(仍阻断),但定性正确。

**6.5 文案清理** —— `Unknown` 文案里 25 个连续空格(会进 `<pre>` 日志面板显形)+ 连续空格全局清理。

## 未修但已记录(留待用户决策)

- **执行链的 TOCTOU 假设**:预检与 `up -d` 之间有几秒到数分钟(逐包 `docker load`),期间若有
  外部主体改 tag(另一台机器的本应用 / CI / watchtower),预检结论会失效。本应用互斥锁只锁本进程。
  治本方案 = `up -d` 前对 `RemoteById` 项复核一次,但会增加往返;当前**未做**,权衡后留待用户决定。
- **`packages` 与 `manifest.images` 无反向校验**:目录里有 manifest 未记录的 tar 时照常装载
  (按 `ls` 字母序,可能覆盖正确标签)。正常部署不会产生多余包,故优先级低。
- **`.env` 不归档导致的插值漂移**:compose 用 `image: ${VAR}` 且 `.env` 在回滚前被改过的项目,
  `up -d` 解析出的引用可能与 manifest 记录不同。**需真机验证**(取决于是否用变量插值镜像引用)。

## 验证

- `cargo test` **477 passed / 13 ignored**(新增 2:标签占用 / 查询失败定性)
- `cargo clippy --all-targets` 与基线 **20 条一致,零新增**
- verify 六脚本 PASS;`doc-consistency` 全 PASS
- **06 页预检端到端桩验证**:三态渲染正确(归档有包 / 按 ID 命中 / 回不去)、阻断项置顶、
  按钮变「仍要回滚(部分)」、`unknownCmds` 为空(未发错参数名)
- 文档核对:三处曾描述「不存在的实现」的表述现在**逐条 grep 验证为真**

---

# v6.12.0 回滚/部署「按镜像 ID 收敛」批(2026-09-20)

> 来源:用户真机问题 + 读取第二十九批「未修但已记录」三项后要求修复。
> 真机案例:`goodlaser-backend:latest` 实际指向 `265b2e14d9a6`,而归档记录的
> 镜像 `c313095267ee` 仍在服务器上(挂在其它标签下)→ 旧预检判「回不去」阻断,
> 用户看到「该标签现在指向 …,不是本次归档的版本」后**没有任何出路**(只能手工
> `docker tag`)。根因:该服务 `file=null`(智能传输跳过未打包,归档里没有它的包),
> 而整栈回滚**没有 `docker tag` 步骤**(rollback.rs 注释自己承认),标签由 `docker load`
> 从包内恢复 —— 没包就恢复不了。

## 设计核心:单一不变式 + 两个共用纯函数

**不变式:记录过镜像 ID 的服务,`up -d` 前其标签必须指向该 ID。**
判定与收敛只实现一份(`plan_tag_convergence_pairs`),回滚/部署两侧各自组装输入 ——
避免「预检一套、执行一套」的判定漂移(第二十九批的核心教训)。

### A. up 前「按 ID 收敛」(治真机案例 + TOCTOU 残余)

- 新纯函数 `plan_tag_convergence_pairs(expectations, remote)`:ID 在且标签已指向它
  → 无动作;ID 在但标签指向别处/不存在 → 出 `docker tag <ID> <tag>`(零拷贝);
  ID 不在 → `missing`(唯一真正的「收敛不了」)。
- `plan_tag_convergence(images, remote)` = 回滚侧的适配层(只处理「无归档包 + 有 ID」
  的服务;有包的服务标签由 `docker load` 恢复,不重复处理)。
- 接入三条链:回滚 `_inner`、回滚 `_at_inner`、部署单镜像 + 整栈(部署侧期望值 =
  本次本地 `image_id_by_ref`;拉取类服务不参与 —— 其 tag 语义就是要随 pull 移动)。
  预算:+2 次往返(收敛查询 + up 后校验),报告最多 +3。

### B. 预检四态化(把「标签被占」从死路改为可恢复中间态)

- 新来源 `RollbackImageSource::RemoteByIdTagMoved`(契约串 `tagRestore`):
  **不阻断**,文案明说「执行时会自动把标签指回归档版本(零拷贝)」。
- `RollbackPrecheckSummary` 增 `tag_restore` 计数;阻断文案计数同步。

### C. 清单驱动装载(治 packages 与 manifest 无反向校验)

- 新纯函数 `select_packages(images, actual_files)`:只装载 manifest 记录的包;
  目录里未记录的 `.tar.gz` **跳过并逐条告警**(此前按 `ls` 全量装载,多余包会静默
  覆盖标签);记录的包缺失单列(预检已阻断,此处防御性再报)。
- 无 manifest 的旧归档:保持「按目录内全部包恢复」但**显式提示**无法反向核对。

### D. 插值漂移检测(治 .env 不归档的插值漂移)

- **不新增 manifest 字段**:manifest 的 `tag` 本身就是部署当时的插值结果,
  用它当比对基准即可(旧归档同样可检,零 schema 变更)。
- 新纯函数 `image_refs_with_env(compose_text, override_texts, env)` +
  `detect_image_env_drift(refs, manifest)` + `parse_env_text`(从 `load_env_path` 抽出)。
  预检与执行链都用**归档 compose(含归档 override)+ 服务器当前 `.env`** 重算比对。
- 漂移 → **须确认**(同一错误码 `rollback_precheck`,与「回不去」同款);确认后按
  实际解析结果执行并在历史里留痕(进 `partial_note`)。

### E. up 后运行镜像校验(兜住所有残余路径)

- 新纯函数 `select_container_image_mismatches(expected, actual)` +
  `parse_compose_service_images`;`:2 次 SSH`(`ps -q --all` + 一次 `docker inspect
  --format '<服务标签>|<镜像>'`)。
- 回滚两条链与部署整栈链接入(`verify_running_images` / `collect_running_images`);
  单镜像部署用 `expected_image_running_anywhere`(管线无「服务↔镜像」映射,只能判
  「本次镜像有没有在跑」)。
- 一致性 → 一行日志;**不一致 → 逐条告警**;不改判定结果(成败仍由 up 退出码定),
  职责是让「实际跑的版本 ≠ 期望版本」可见。

## 文件改动

| 文件 | 改动 |
|---|---|
| `src-tauri/src/commands/rollback.rs` | 四态枚举/计数、`plan_tag_convergence(_pairs)`、`select_packages`、漂移预检、收敛执行步、`verify_running_images`/`collect_running_images`、预检返回扩 `tagRestore`/`envDrift` |
| `src-tauri/src/commands/deploy.rs` | 单镜像 up 前收敛 + up 后校验;整栈步骤 6.0 收敛 + 6.1 校验 |
| `src-tauri/src/stack.rs` | `referenced_env_vars` / `parse_env_text` / `image_refs_with_env` / `detect_image_env_drift` / `ComposeImageRef` / `ImageEnvDrift` |
| `src-tauri/src/commands/tests.rs` | 新增 23 个单测(含改写旧用例:标签被占从 Missing 改为 tagRestore) |
| `ui/deploy-rollback.js` / `ui/rollback.js` | 四态渲染(tagRestore 中间态 + envDrift 区块)、`allowPartial` 纳入漂移 |
| `ui/help.js` | 04/06 帮助补自动指回 / 漂移 / 后校验(收尾清单显式检查:help 是历次盲区) |
| `verify/user-facing-copy.js` | 新增 3 条断言(tagRestore/envDrift 消费 + 漂移入确认门) |

## 验证

- `cargo test` **500 passed / 13 ignored**(477→500,新增 23)
- `cargo clippy` 与基线 **逐条一致,零新增**(仅行号位移)
- **变异自证**:三处关键判定改坏 → 对应测试全红(select_packages 反向校验 /
  收敛的标签对齐检查 / 漂移检测的取值入哈希)后还原
- verify 六脚本 PASS(含 user-facing-copy 新增断言,且对其做了反向变异验证)
- `scope-integrity` 捕获一处真实缺陷:help.js 里 `${变量}` 在模板字面量中会被当作
  JS 插值(顶层执行抛 ReferenceError)—— 已转义为 `$\{变量\}`

## 明确留档不做(维持已裁决)

- 无 manifest 旧归档不做目录反向校验(降级 = 现状 + 警告);归档不新增 `.env` 存储
  (维持 R2 用户裁决;漂移改走「检测 + 用户确认」)。
- 部署侧「拉取类」服务无法收敛校验(tag 语义即随 pull 移动)。
- TOCTOU 的**剩余窗口**(收敛查询到 up 之间)由 up 后校验兜底发现;不追求零窗口
  (那需要 `docker tag` 后立即 up 的事务语义,Docker 不提供)。

## 补丁修复:up 后校验的 docker inspect 模板解析错误(真机反馈,2026-09-22;随 v6.12.1 发布)

真机部署日志报:`警告:up 后校验未能完成(docker inspect 查询失败(退出码 64))`。

**根因**:v6.12.0 的 up 后校验用 `{{.Config.Labels."com.docker.compose.service"}}`
读取带点的标签键 —— **不是合法 Go 模板**(字段链只接受标识符;带点的键必须用
`index` 函数读取)。docker CLI 在连接 daemon **之前**解析模板,直接报
`template parsing error: bad character U+0022` 并以 **退出码 64**(usage 错误)退出
→ 该校验在每次部署/回滚时都失败(只告警、不误判成败 —— 降级行为正确,但校验
整条形同虚设)。**真机复现**:本机无 daemon 也能复现(解析先于连接)。

**修复**:
- 模板改 `{{index .Config.Labels "com.docker.compose.service"}}|{{.Image}}`,
  提为常量 `COMPOSE_SERVICE_IMAGE_TEMPLATE`;
- `collect_running_images` 两处失败信息附远端输出尾部(`tail_lines`)—— 真机
  教训:只报「退出码 64」时用户与维护者都要反推;
- 新增回归守护 `test_compose_service_image_template_parses`:形态断言(零依赖,
  CI 无 docker 也能挡住退回点号写法)+ 真机 docker CLI 实际解析一次模板
  (解析先于 daemon,无 daemon 亦可验证;CLI 不存在则跳过,与 docker:: 真机
  测试同口径)。**变异自证**:改回点号写法 → 测试红(形态断言先命中)。

测试 500→501;clippy 与基线零新增;verify 六脚本 PASS。

---

# 第三十一批:v6.13.0 部署/回滚流程加固(P1/P2 + S1/S2,2026-09-22)

> **来源**:候选池(2026-09-22 入库)用户批复「首批 P1 孤儿容器 + P2 `--pull never`」,
> 开工前追问补齐两处边界(05 页栈启停 / 迁移目标启动)+ 搭车 S 组。S2 的口径由用户
> 拍板「自动通过」(三选一),S1 复核后确认**代码早已修复**、只剩文档过期。

## P1 孤儿容器:`--remove-orphans` 全链

**问题**:回滚到旧归档(或部署时删掉某服务)后,新版才有的服务容器会作为**孤儿继续
运行**,界面却报「完成」—— 与第三十批「按 ID 收敛」同族的「回滚没回干净」。

**改动**:`up` 命令拼装收敛为**唯二来源** —— `compose_up_cmd`(带 `-f` 链:整栈部署 /
单镜像部署 / 04 页两条回滚链 / 迁移目标)与 `compose_up_cmd_in_dir`(06 页按目录默认
文件名),尾部固定 `--remove-orphans --pull never`(常量 `COMPOSE_FLAG_*`);05 页栈
启停(`manage_stack_action`)经纯函数 `compose_action_sub`,`up -d` / `down` **两侧**
都带 `--remove-orphans`。

**本机 Docker 实测**(compose v5.4.0,跑完已清理归零):

| 场景 | 旧形态 | 新形态 |
|---|---|---|
| v1(a+b)→ v2(只 a)再 up | 一行 orphan warning,**b 容器仍 Up** | `b Stopped → Removing → Removed` |
| `down` | a 被删、b 残留,并报 `Network ... Resource is still in use` | `b` 与网络一并回收 |
| 应用原样命令串(带/不带 `-f` 链两种形态) | — | 孤儿均被移除 |

## P2 禁止隐式拉取:`--pull never`

**问题**:`up -d` 默认 `pull=missing`,引用在本地不存在时会**静默从 registry 拉一个
非归档版本**(`.env` 漂移 / 标签被外部移走可触发)。

**实测**:本地无 `alpine:3.21` 时,旧形态 up 静默拉取并启动;新形态
`up -d --remove-orphans --pull never` → `Error response from daemon: No such image: ...`
(退出码 1,**不产生容器、不拉取**)。**回归**:拉取类服务「步骤 5 显式 pull → 步骤 6
新形态 up」实测通过(不受影响)。

**边界**:①05 页栈「启动」刻意**不带**该旗标(该入口既有用户可见语义含「拉取缺失
镜像」,见 ui/help.js);②需服务器 compose ≥ v2.15(`--pull` 旗标自该版提供),
更老版本会报 unknown flag —— 已写入 wiki/06、wiki/07 与 ROADMAP 池条目修正。

## S2 健康检查 exited-0 口径(用户拍板:自动通过)

`health_verdict` 把 `exited && ExitCode==0` 归入「已完成」:`Pass{completed}` 携带
服务名,`health_check` 逐条明示「服务 X 已成功退出(退出码 0),按完成处理」;
`Indeterminate` 的 `exited_zero` 字段删除,预算耗尽文案不再提示「关闭健康检查」。

**为什么**:一次性初始化服务的正常终态此前被当作「未就绪」轮询到预算耗尽 → 这类栈
**每次部署都误报失败**;若开了「部署失败自动回滚」会把**好部署回滚掉**(真故障级)。

**代价(已知并接受)**:「起错命令、秒退 0」的常驻服务不再被健康检查拦住 —— 实测
`collect_running_images` 用 `ps -q --all`,该形态容器存在、镜像 ID 正确,up 后运行
镜像校验同样看不到。留档:若将来出现「必须拦截秒退 0」的真实诉求,再启用项目级
逐服务忽略清单(本批口径三选一中的第二项)。

**本机实测**:真实 `compose ps --all --format json` 输出中,一次性服务为
`"State":"exited","ExitCode":0`(与判定依据一致)。

## S1 复核(候选条目基于过期文档)

S1 声称 `import_compose` 失败残留 `config/stacks/<uuid>/`。实测**代码早在第二十三批
已修**:`compose_sources.rs` 把「复制副本 → 记 origin.json → 写配置」包进闭包,任一
失败即清理目录,并有单测 `test_import_compose_failure_cleans_stray_stack_dir`(用损坏
`projects.json` 复现「副本已拷完、写配置失败」)。本批仅更正 wiki/07 限制 7 的记载。

## 文件改动

| 文件 | 改动 |
|---|---|
| `src-tauri/src/commands/deploy.rs` | 新增 `COMPOSE_FLAG_REMOVE_ORPHANS` / `COMPOSE_FLAG_PULL_NEVER`;`compose_up_cmd` 带两旗标;新增 `compose_up_cmd_in_dir`;单镜像 up 改走同一拼装器(消除重复字符串);`health_check` 消费 `Pass{completed}` 并逐条记日志 |
| `src-tauri/src/commands/rollback.rs` | 06 页 up 改走 `compose_up_cmd_in_dir` |
| `src-tauri/src/manage_stacks.rs` | 新增纯函数 `compose_action_sub`(up/down 均带 `--remove-orphans`;不带 `--pull never`) |
| `src-tauri/src/migrate_project.rs` | 新增纯函数 `compose_simple_action_cmd`(目标 up 走 `compose_up_cmd`;源侧 stop/start 不变) |
| `src-tauri/src/commands/mod.rs` | `HealthVerdict::Pass{completed}` / `Indeterminate{pending}`;exited-0 按完成处理 |
| `src-tauri/src/commands/tests.rs` + 两模块测试 | 更新既有断言(compose 命令 2 处 + health_verdict 6 处)+ 新增 4 条口径测试 |
| `wiki/02` / `wiki/04` / `wiki/06` / `wiki/07` | 命令形态、健康检查语义、P1/P2 决策与代价(决策 79/80)、S1 更正、S2 修复 |
| `ui/help.js` | 04 页「启动阶段的两条加固」;05 页栈启停命令;06 页回滚命令与说明 |
| `verify/user-facing-copy.js` | 第 6 节 5 条断言(首次变异测试**漏网一次**:宽松正则被 06 页行误命中 → 收紧为 05 页 code 片段后重测通过) |

## 验证

- `cargo test` **505 passed / 13 ignored**(501→505,新增 4)
- `cargo clippy --all-targets` 18 条告警**全部落在未改动行**(逐条比对位置,零新增)
- 本机 Docker 实测:P1(up/down + 两种命令形态)/ P2(静默拉取对照、显式报错、拉取类
  回归)/ S2 真实 ps 输出形态;测试产物清理,容器/镜像归零
- verify 六脚本 PASS(user-facing-copy 27 项;新增第 6 节经反向变异:去掉 05 页旗标 /
  去掉 P1 口径句 → 均红,还原后绿)
- 文档:本节 + ROADMAP(池状态/速览/池条目修正)+ 三处版本号 + 七篇页首戳 +
  `doc-consistency --write --tests=505`

## 明确留档不做

- 05 页栈「启动」不加 `--pull never`(既有语义含拉取缺失镜像,改它会破坏文档化的行为)。
- 不做「逐服务健康检查忽略清单」(用户选了自动通过;触发条件见上 S2 节)。
- 候选池其余条目不在本批:P3(服务端 `compose config` 权威校验)/ P4(更新包签名)/
  L1(层级增量)/ L2(体检增强)/ S3(迁移管线真机样板)/ S4(词法级自由标识符守护)/
  S5(wiki 的 watchtower 例子,本批未动,仍在池中)。

---

# 第三十二批:回滚链审查修复(P1/P2 后续;随 v6.13.0 合并交付,2026-09-22)

> **来源**:用户要求「详细讲 P1 并审查回滚是否还有隐藏漏洞」→ 逐段审两条回滚链
> (04 页 / 06 页)与预检,列 **F1–F7** 七条 finding(每条附实测或代码证据),
> 用户批复「全部进行修复」。**版本**:v6.13.0 尚未发版,本批与第三十一批**共享同一
> 版本号**(先例:第二十七/二十八批合并为 v6.9.0),不单独 bump。

## F1/F2 回滚的 compose/override 一律「归档为准」,06 页改显式 `-f` 链

**实测依据**(本机 compose v5.4.0):
- 默认文件解析优先级 `compose.yaml` > `compose.yml` > `docker-compose.yml` > `docker-compose.yaml`;
  同目录并存时 compose 自身打印 `Found multiple config files … Using compose.yaml` →
  **归档恢复出的 `docker-compose.yml` 根本不参与启动**(旧形态实测:起的是 shadow 项目的容器)。
- 无 `-f` 时**最多只吃一个** override(`compose.override.yaml` 优先):三文件场景实测自动解析出
  `A B`(缺 `docker-compose.override.yml` 的 `C`),而部署用的是全部 `-f` 链(`A B C`)。

**修复**:06 页 up 由纯函数 `rollback_at_up_plan` 给出「显式 `-f` 链(归档基础文件 + 归档内
四个标准 override)+ 与 up **逐字同源**的校验前缀」;归档无副本的旧归档保持降级(按目录现有
compose 启动),并由预检 `noComposeCopy` 提示;目录内存在遮蔽文件时执行期告警(不静默)。
04 页的 override 集合同步改为归档为准(`rollback_override_chain`,与 `stack::OVERRIDE_FILE_NAMES`
同源);本地新增/删除 override 不再影响回滚合并结果(本地独有项给日志提示)。

**实测**:新形态 up 起 `ddroll-arch` 项目 4 个服务(arch/keep/ov/ov2),`shadow` 未启动;
再删掉基础 compose 里的 `keep` 后 up → `keep` 被孤儿清理移除(项目内收敛仍有效)。

## F3 `--no-build`:堵住「构建」这条非归档通道

`--pull never` 只挡拉取。实测:服务写了 `build:` 且镜像缺失时 `up` **现场构建**并起容器;
加 `--no-build` → `No such image: <ref>`,不构建、无容器。已加入 `compose_up_cmd` 的固定尾旗
(部署/回滚/迁移;05 页栈启停不带)。三个旗标合起来是「**不拉、不建、不留孤儿**」。

## F4 孤儿清理的「项目作用域」边界(文档)

wiki/07 决策 79 补边界:①换 `remote_dir` / 目录改名 → 旧目录容器不在此列(两套并存);
②两个项目共用同一部署目录(限制 38)→ 部署/回滚其一会移除另一个的容器;
③compose 顶层 `name:` 被改动同理。help.js 同步。

## F5 两条回滚链的 override 集合统一为「归档为准」

见 F1/F2;**预检的插值漂移取值也改用同一函数** —— 否则「漂移结论」与实际 up 的合并结果
可能不一致(只查一个来源、执行用另一个)。

## F6 预检新增 `noComposeCopy` + 桩验证**顺带抓到真 bug**

`RollbackPrecheck` 增 `no_compose_copy`(camelCase `noComposeCopy`),两个入口的预检结果块
各加一行提示;桩验证两页截图交 judge,均 **pass**。
**顺带抓到的真 bug**:04 页模态与 06 页面板都创建 `id="rb-precheck-box"`,而 06 页元素在 DOM
中**更靠前** → 04 页 `getElementById` 取到 06 页容器,**预检结果写进另一页、模态里空白**
(用户看不到「回不去」清单)。已将 06 页容器改名 `rb-page-precheck-box`,并在
`verify/user-facing-copy.js` 加两条断言(两 id 必须不同 + 04 页仍按 `#rb-precheck-box` 取目标)。
**该 bug 与本次改动无关**,触发时序为「先访问 06 页跑过预检 → 再开 04 页回滚模态」。

## F7 up 后校验的不一致数写进历史/通知

`verify_running_images` 改为返回不一致明细;`rollback_result_message` 统一组装结果文案:
部分回滚/漂移的服务名 + 「N 个服务的实际镜像与归档不一致(详见日志)」都进 `record.message`
(该 message 同时进通知正文)。**判定结果不变**(成败仍由 up 退出码决定,不一致不升级为失败)。

## 文件改动

| 文件 | 改动 |
|---|---|
| `src-tauri/src/commands/deploy.rs` | `COMPOSE_FLAG_NO_BUILD` + 尾旗;`compose_up_cmd_in_dir` 标注为降级专用 |
| `src-tauri/src/commands/rollback.rs` | 新增 `has_compose_copy` / `rollback_override_chain` / `shadowing_compose_files` / `compose_shadow_probe_cmd` / `rollback_at_up_plan` / `rollback_result_message`;04 页 override 链改归档为准 + 差异日志;06 页 up 改显式链 + 遮蔽告警;预检 `no_compose_copy` + 漂移 override 集合同源;`verify_running_images` 返回明细 |
| `src-tauri/src/stack.rs` | `OVERRIDE_FILE_NAMES` 常量(与 `find_override_files` 同源) |
| `src-tauri/src/commands/tests.rs` | 旗标断言补 `--no-build` + 新增 5 条纯函数测试 |
| `ui/deploy-rollback.js` / `ui/rollback.js` | 预检提示行(noComposeCopy);06 页容器 id 改名(修 id 冲突) |
| `ui/help.js` | 三条加固(含 build)、「compose 与 override 以归档为准」、降级说明 |
| `verify/user-facing-copy.js` | +5 条断言(F3 文案 / 归档为准 / noComposeCopy 双入口 / 两 id 不同 + 04 页取目标) |

## 验证

- `cargo test` **510 passed / 13 ignored**(505→510,新增 5)
- `cargo clippy` 逐条比对:零新增(告警位置与本批改动行无交集)
- **本机 Docker 实测**:遮蔽文件对照(旧形态起 shadow / 新形态起归档项目)、多 override
  (`-f` 链 3 vs 自动 1)、`--no-build` 对照(构建成功 vs 显式报错)、孤儿移除仍生效;跑完清理归零
- **桩验证 + judge**:两个入口提示行各截一图,judge 两页均 **pass**;顺带发现并修复 id 冲突
  (含 DOM 证据:两个 `[id=rb-precheck-box]`,靠前者属 06 页)
- verify 六脚本 PASS(user-facing-copy 32 项)

## 明确留档不做

- 全站「多页同 id」系统性扫描(本批修了实测发现的一处,扫描留作候选)。
- 「up 后镜像不一致」不升级为失败(维持既有口径,仅进结果文案)。
- 非标准 override 名(手工放进归档)不进入 `-f` 链(收窄,见 wiki/07 决策 81)。

---

# 第三十三批:v6.14.0 文件管理(容器 / 数据卷 / 部署目录)+ 容器快照 + 同栈分发(2026-09-22)

> **来源**:用户「容器终端里要有文件管理(上传/修改/下载)」+ 讨论后拍板三项范围:
> ①数据源 = **容器 + 卷 + 部署目录只读**;②覆盖策略 = **本机备份保留 3 份**；
> ③本批范围 = 文件管理 + C(容器快照)+ D(同栈分发)。

## 通道选型(先实测,再定方案)

| 实测(Docker 29.7.2 本机) | 结论 |
|---|---|
| `docker cp <cid>:<path> -` / `tar cf - \| docker cp - <cid>:/dir/` | `docker cp` 是 daemon 侧文件操作:**零二进制容器(distroLESS/scratch)双向可用** |
| 已停止容器 cp | 双向可用(`docker exec` 报 is not running) |
| 容器 → 容器 cp | **Docker 不支持** → 一切经宿主 `/tmp` 中转(与卷备份/迁移同构) |
| `LC_ALL=C ls -la`(BusyBox 与 GNU) | 同形,可稳定解析(含带空格文件名、符号链接 `-> ` 形态) |
| 数据卷 `docker run --rm -v <vol>:/v -v <stage>:/out <img> sh -c 'cp -r /v/x /out/'` | 卷源**总有工具**(镜像自带),下载/上传/改名/删除全链实测通过 |

## 交付

**8 条新命令**(`manage_files.rs`,命令 114→**122**):`manage_files_list` / `download` / `upload` /
`read_text` / `write_text` / `fs_op` / `cancel` / `manage_container_snapshot`。
纯函数 9 个(路径校验/目标校验/绝对路径/文件名/`ls` 解析/候选命令/卷命令/打包命令/fs 脚本/备份裁剪)
全部单测;**真机 `#[ignore]` 样板** `test_real_docker_list_and_cp_roundtrip`(列举解析 + cp 出/回 + 目录打包)。

**前端**:新模态 `files-modal`(`ui/files.js`,四视图:列表 / 编辑器 / 快照 / 分发)+ 05 页容器行
「文件 / 快照」、卷行「文件」入口;`FilesKit` 单次赋值桥(宿主内联消费 —— `bridge-integrity` 已扩展为
同时扫 `window.<Kit>.<键>` 形态)。

**关键口径**:部署目录**只读**(用户裁决,写路径一律 Err);编辑 ≤512KB 且 UTF-8;覆盖前默认备份到
`config/fm-backups/<server>/<kind>-<target>/<ts>/` 保留 3 份;快照环境变量默认掩码;分发为前端串行编排
(零新命令,复用 upload)。

## 验证

- `cargo test` **519 passed / 14 ignored**(510→519;真机样板 `--ignored` 已实跑通过)
- 本机 Docker 实测:容器列举/解析、cp 往返、目录打包、**卷源全链**(列举→下载→改后上传→mkdir/rename/delete)
- verify 六脚本 PASS(user-facing-copy 36 项,新增 4 条:文件管理入口 / 只读口径 / fm-backups / 分发)
- 桩验证三视图(列表 / 快照 / 分发)+ judge **三轮**:首轮快照 fail(进程行走比例字体导致
  docker top 分栏错位)→ 改等宽轨道后**二轮仍 fail** —— 真因是 **HTML 折叠连续空格**
  (docker top 是 tabwriter 空格补齐输出,真机抓样确认无 TAB),且桩数据也按真机形态重做;
  改 `white-space: pre` + 技术块内横向滚动后**三轮 PASS**。顺带修掉界面文案里 markdown `**`
  直接可见的问题(后端 note 字符串)
- 命令计数/doc 同步:`wiki/04` 122 + 事件 13 + 七篇页首戳 + README/ROADMAP

## 明确留档不做

- 宿主机**任意路径**文件管理(仅开放部署目录且只读;通用宿主文件管理风险高,另行评估)。
- 容器内多选批量传输(先单条;批量已有「同栈分发」)。
- 二进制/超大文件的就地编辑(引导下载改后上传)。

---

# 第三十四批(一):静态守护(自由标识符 + DOM id)+ 文档清尾(2026-09-22)

> **来源**:第三十四批开工总方案(池内 7 项 + 池外 4 项,四问确认后分批);本批 = **S4** +
> 池外「全站多页同 id 系统性扫描/守护」+ **S5**,并把新扫描首跑抓到的两处真 bug 一并修复。
> **版本**:v6.15.0(本批为批次收尾,后续批共享同一版本号)。

## 交付

**新守护 `verify/static-integrity.js`**(零依赖,已入本地链与 CI),两段 ——

- **A 未声明自由标识符(词法级)**:补 `scope-integrity.js` 的运行时盲区(元素级 addEventListener /
  setTimeout 等嵌套回调不执行,getElementById 恒 null 还会短路「以元素存在为前置」的分支)。
  自建 ES5 词法器(注释/字符串/模板/正则/数字)+ 作用域链收集(var/function 提升到最近函数作用域、
  形参、catch 形参、window/globalThis 全局赋值集合 46 个、内建与 DOM 接口白名单),引用判定在整文件
  收集后统一进行;`typeof x` 操作数跳过(特征探测合法)。**负向夹具 10 条**随脚本常跑(含 v5.14.0 的
  `$` 事故形态、跨文件局部名外泄、字符串/注释/正则免疫、三元中段引用、用户属性键不误报)。
- **B DOM id 完整性**:B1 index.html 静态 id 唯一性(229 个);B2 `getElementById('x')` /
  `querySelector('#x')` / `$('x')` 字面量必须可解析 —— 合法来源 = 静态 id ∪ 动态赋值(`.id = 'x'` /
  `setAttribute('id',…)`)∪ innerHTML 静态骨架字符串 ∪ **字段构造助手参数位**(`buildField`/
  `appendField`/`checkboxRow` 这类「形参赋给 `.id`」的助手,含链式转交,按形参索引取实参);B3 动态 id
  重名默认失败,评审通过者登记白名单(附理由;本仓 4 条均为「同一容器清空后顺序重建,任一时刻至多一个
  实例」)。

**首跑抓出的两处真 bug(本批修复)**:

1. `ui/deploy.js`(原 2571 行)`serverId: sid` —— **第二十八批 B1 复合键重构漏删声明的回归**:原实现
   `var sid = String(item.serverId)` 被删、`serverId: sid` 留在失败/取消台的续传登记里 → 登记时抛
   `ReferenceError`(发生在 deploy-done 回调里,后续 `st.batch.idx++` / `renderBatchPanel` /
   `runBatchNext` 全部不执行 → **该台之后队列停摆、续传入口不登记**)。修复 = `String(item.serverId)`,
   与上方复合键 `batchItemKey(item.serverId, item.project.id)` 同源。
2. `ui/app.js`(1321 行)`getElementById('settings-btn')` —— 实际 id 为 `settings-entry-btn`
   (`index.html:57`),`if (gear)` 守卫吞掉 → **dock 版本徽点「点击打开设置中心」静默失效**。
   修复 = 改对 id(桩验证见下)。

**顺带清理**:`ui/manage-stacks.js` 的 .env 保存确认按钮 `confirm-ok-btn`/`confirm-cancel-btn` 与
`manage.js` 的通用确认体跨文件重名 → 按同文件既有约定改名 **`env-confirm-*`**(4 行,零行为变化;
复现 `compose-confirm-*` 已命名空间化的惯例)。`verify/scope-integrity.js` 头注释漂移修正(CHAIN 实为
theme-init + 17,并补 static-integrity 的分工说明)。

**S5**:`wiki/06:136`、`wiki/07:273` TOCTOU 例子的 watchtower(2025 已归档)改为「另一台机器的本应用 /
CI / 运维脚本」。

**文档清尾**:AGENTS.md 基线行与 verify 清单(四/六脚本 → 指向「常用命令」清单,增 static-integrity)、
ROADMAP(候选池状态、S4/S5 行、速览、命令 114→122 漂移、JS 16→18 漂移)、wiki/README 命令/事件/
章节计数与版本段、三处版本号、七篇页首戳。

## 验证

- `cargo test` **519 passed / 14 ignored**(亲自读 `test result: ok` 行;纯前端/脚本改动,Rust 侧不变)
- `cargo clippy`:本批零 Rust 源码改动(仅 Cargo.toml 版本串),告警集合与基线不可变
- `node --check`:deploy.js / app.js / manage-stacks.js / static-integrity.js 全过
- verify **七脚本**全 rc=0(新脚本:10 夹具全 PASS + A 段 18 文件零发现 + B 段 229 静态 id /
  221 动态 id / 536 引用全部可解析)
- **桩验证 + judge**:桩页(take_update_pending→host_check→app_settings_get→update_check 全链,
  零未捕获异常/未处理 rejection)中点击版本徽点 → 设置模态打开(visible=true,标题「设置SETTINGS」,
  内容完整);judge **pass**。注:IAB 的 Playwright 指针点击通道本会话未生效(locator click 超时、
  坐标点击无效,DPR=1 且命中测试证明元素本身可点)→ 验收改用页内 click 事件驱动处理链
- CI:`.github/workflows/ci.yml` 增 static-integrity 一步

## 裁决记录(2026-09-22 四问确认;后续批次按其执行)

| 项 | 裁决 | 落地 |
|---|---|---|
| P4 更新包签名校验 | **跳过留档**(待密钥管理拍板后另行开工) | 本版本不实现 |
| 宿主机文件管理 | 白名单目录可读写(全局设置项,默认空;白名单外维持现状只读) | 第三十四批(五) |
| 容器内文件传输 | 做多选批量(下载 + 上传) | 第三十四批(四) |
| 「up 后镜像不一致」 | 统一口径 + 可见(部署链与回滚链同文案进 `record.message`,不升级失败) | 第三十四批(二) |

## 文件改动

| 文件 | 改动 |
|---|---|
| `verify/static-integrity.js` | **新增**(词法器 + 作用域收集 + id 三查 + 10 条夹具) |
| `ui/deploy.js` | 批量续传登记 `sid` 回归修复(1 行) |
| `ui/app.js` | 设置入口 id 修复(1 行) |
| `ui/manage-stacks.js` | .env 确认按钮 id 命名空间化(4 行) |
| `verify/scope-integrity.js` | 头注释漂移修正(16→17 + 分工说明) |
| `wiki/06` / `wiki/07` | watchtower 措辞(watchtower → 运维脚本) |
| `.github/workflows/ci.yml` | 增 static-integrity 步骤 |
| `AGENTS.md` / `ROADMAP.md` / `wiki/README.md` / 七篇页首戳 / 三处版本号 | 基线与计数对齐 v6.15.0 |

---

# 第三十四批(二):up 前服务端权威解析校验 + 「up 后不一致」口径统一(2026-09-22)

> **来源**:第三十四批方案(P3 + 池外 D 裁决);**版本**:v6.16.0(v6.15.0 已发版,本批为收尾 bump)。

## 交付

**P3 —— up 前服务端权威解析校验**(`docker compose config --format json`;+1 次 SSH)

- **纯函数三件套**(`stack.rs`):`parse_compose_config_json`(容忍 stdout+stderr 合并流:取首 `{`…末 `}`;缺 `image` 的构建类服务保留为 `None`)/ `normalize_image_ref`(补缺省 `:latest`、剥 `docker.io`/`index.docker.io` 前缀;注册表端口冒号不误当 tag)/ `diff_resolved_images`(不符 / 解析缺失 / 构建类三态;只判前两态)。
- **探测与降级**(`deploy.rs`):`compose_config_json_cmd(_in_dir)`(`-f` 链与 up 同源)+ `compose_config_probe`(退出码 0 且可解析为 JSON 才判定;`unknown flag` → 告警跳过;其余非零 → Err 附输出尾部)。
- **接入三条链**:**整栈部署(阻断)** —— 在 6.0「按 ID 收敛」之后、up 之前与服务清单逐服务比对,不一致返回错误并逐条列明细;**04/06 回滚(只告警)** —— 插值漂移预检已是「确认制」,再阻断会与用户刚做出的确认冲突;紧急回滚优先,实际结果由 up 后校验兜底。
- **边界**:单镜像部署/回滚无逐服务 manifest,不接入(既有「按 ID 收敛 + up 后校验」覆盖);拉取类服务无声明 image 时自然跳过。
- **本机 Docker 实测抓样**(compose v5.4.0):`${VAR}` 插值、注册表路径、无 tag 原样保留、build 且无 image **不补默认镜像名**、undefined 变量 warning 与 JSON 同流、语法错误 exit=1 + go-yaml 报错 —— 全部固化为 golden 单测。

**D —— 「up 后镜像不一致」口径统一(部署侧接入)**

- 整栈:6.1 校验的不一致数写入 `record.message`(「部署完成;up 后校验:N 个服务的实际镜像与本次构建不一致(详见日志)」);单镜像:未发现容器运行本次镜像时同款并入(「…未发现容器在运行本次部署的镜像(详见日志)」);两处收尾改为「管线预设的 message 非空则沿用,否则回落『部署完成』」。**判定结果不变**(成败仍由 up 退出码/健康检查决定)。

## 验证

- `cargo test` **528 passed / 14 ignored**(519→528,+9:stack 7 + commands 2)
- **变异自证两轮均被抓**:①去掉 `:latest` 补全与前缀剥离 → `test_normalize_image_ref_cases` 红;②把「构建类无 image」改判为不一致 → `test_diff_resolved_images_ok_with_latest_normalization` 红;还原后全绿
- `cargo clippy`:11 条(与基线一致,零新增)
- 本机 Docker 实测:上表四类形态抓样 + 语法错误 exit 码(命令拼装与解析口径均实测)
- verify 七脚本 rc=0;`node --check`(JS 仅 help.js 文案改动)
- 真机验证:按既有约定由用户统一执行

## 文件改动

| 文件 | 改动 |
|---|---|
| `src-tauri/src/stack.rs` | 纯函数三件套 + 7 单测(golden 来自本机实测) |
| `src-tauri/src/commands/deploy.rs` | `compose_config_json_cmd(_in_dir)` / `ComposeConfigProbe` / `compose_config_probe` / 整栈链阻断接入 / 单镜像 `post_note` / 两处收尾文案口径 |
| `src-tauri/src/commands/rollback.rs` | `manifest_image_refs` 纯函数 + 04/06 两链只读告警接入 |
| `src-tauri/src/commands/tests.rs` | +2 单测(命令拼装 / manifest 引用口径) |
| `ui/help.js` | FAQ 新增「up 前/后校验不一致」条目 |
| `wiki/02` / `wiki/06` / `wiki/07` | 校验口径、决策 87、限制 55、行数/测试数漂移修正 |
| 版本/文档 | 三处版本号 + wiki/README + ROADMAP + 七篇页首戳 → v6.16.0 / 528 |

---

# 第三十四批(三):服务器体检增强 + 迁移「目标链」真机样板(2026-09-22)

> **来源**:第三十四批方案(L2 + S3);**版本**:v6.16.0(与(二)同一未发版版本合并交付)。

## 交付

**L2 —— 服务器体检增强(可回收空间 / 容器日志)**

- `server_env_check` 增两条**尽力而为**的探测(同一 SSH 会话,+2 次 exec):
  - `docker system df --format '{{json .}}'` → 四类(镜像 / 容器 / 卷 / 构建缓存)可回收字节 + 总量;
  - 容器日志体积:`docker ps -aq | xargs docker inspect --format '{{.LogPath}}' 2>/dev/null | xargs du -bc 2>/dev/null | tail -1 | awk '{print $1}'`(不依赖 GNU `xargs -r`;非 root 读不到 → 无输出 → 字段缺省)。
- 纯函数 3 个(`ssh.rs`):`parse_docker_size`(十进制/二进制单位双口径,容忍 `260.8MB (100%)` 占比后缀)/ `parse_docker_df`(NDJSON 逐行解析)/ `parse_first_u64`;golden 来自本机 `docker system df` 实测抓样 + 3 单测。
- 契约:`ServerCheckReport` 增 `docker_df` / `container_log_bytes` 两**可选**字段(`#[serde(skip_serializing_if = "Option::is_none")]`);**探测失败不进 `errors`、不参与环境判定**(可选增强信息不该把判定染红),字段缺省时前端隐藏。
- 前端:`servers.js` 03 页卡片新增「可回收空间 ≈ X(镜像/容器/卷/构建缓存)」与「容器日志占用 ≈ Y」两行;`deploy.js` 部署预检在**磁盘紧张**(`disk_free_gb < DISK_MIN_GB`)且有可回收量时提示「磁盘紧张:服务器可回收空间 ≈ X(可用 03 页「清理优化」定向清理)」。两文件各带与 `config-io.js` 同口径的 `formatBytesLocal` 局部助手(沿既有先例)。
- 文档:`wiki/04` 契约块登记可选字段;`wiki/06` 闸门表补「只作展示与提示」边界;`help.js` 03 页「测试连接 / 环境检测」行说明。

**S3 —— 迁移「目标链」真机 `#[ignore]` 样板**(`ssh.rs` 测试模块,紧邻卷 roundtrip)

`test_migrate_project_target_chain_real` 覆盖迁移链里**卷 roundtrip 未覆盖**的段落:
1. 本地生成 compose(`image: busybox:${BTAG}`)/ `.env` / 含二进制字节的模拟归档;
2. 三件套经 `sftp_upload` 上传源目录;
3. **源侧服务端权威解析**(复用第三十四批 P2 的 `compose_config_json_cmd`)→ 断言 `${BTAG}` 被服务器插值为 `busybox:latest`;
4. 归档 `sftp_download` 回本机 → **逐字节比对** → 再上传目标目录;
5. 目标侧权威解析 → `up -d --remove-orphans --pull never --no-build`(与部署/迁移同款加固旗标)→ `compose ps -q` + `docker inspect State.Running` 自证容器在跑;
6. 清理:`down -v` + `rm -rf` 远端与本地临时目录。
运行方式:`DD_SSH_TEST_HOST=... cargo test migrate_project_target -- --ignored`(真机验证按既有约定由用户统一执行)。

## 验证

- `cargo test` **531 passed / 15 ignored**(528→531,+3 纯函数;真机样板 14→15)
- `cargo clippy`:零新增(本批 Rust 改动仅 ssh.rs/无告警面)
- `node --check` servers.js / deploy.js;`static-integrity` 全绿(新增 `formatBytesLocal` 声明与引用闭合)
- **桩验证 + judge**:03 页卡片两行(可回收 1.85 GB 分解 / 日志 20.00 GB)与 04 页「磁盘紧张」提示均按桩数据渲染、零未捕获异常;judge 两图 **pass**(04 页首判 fail 系截图只覆盖 720px 视口、提示行在视口下方——滚动重拍后判 pass,非产品缺陷;顺带确认提示行在失败态仍可见,与错误框并存)
- verify 七脚本 rc=0
- 本机实测:容器日志体积命令在真实 Docker 上返回字节数(本机 21.8 GB,正印证该指标的价值);`docker system df` golden 抓样

## 文件改动

| 文件 | 改动 |
|---|---|
| `src-tauri/src/ssh.rs` | `ServerCheckReport` 可选字段 + `DockerDfSummary` + 3 纯函数 + `check_server_env` 两探测 + 3 单测 + S3 真机样板 |
| `ui/servers.js` / `ui/deploy.js` | 可回收/日志展示 + 磁盘紧张提示(+ 局部 `formatBytesLocal`) |
| `ui/help.js` | 03 页检测行说明 |
| `wiki/04` / `wiki/06` / `wiki/02` / `wiki/README` / `ROADMAP.md` / 七篇页首戳 | 契约与计数同步(531 / 15) |

---

# 第三十四批(四):容器内多选批量传输(第三十三批留档清尾,2026-09-22)

> **来源**:第三十四批方案的池外项(第三十三批明确留档「容器内多选批量传输(先单条;批量已有「同栈分发」)」);
> 用户裁决「做多选批量下载+上传」。**版本**:v6.16.0(与前两子批同一未发版版本合并)。**纯前端**(零新命令、零后端改动)。

## 交付(`ui/files.js` + `ui/style.css`)

- **勾选列**:列表行首常显勾选框(与 05 页容器批量同语言),表头全选框带半选态(`indeterminate`);勾选**就地更新**批量条与全选态(不整块重绘,避免列表滚动位置丢失)。
- **批量下载**:勾选(文件 + 目录均可,目录由服务端自动打包)→ 选一次本机目录 → 逐条串行 `manage_files_download`。
- **批量上传**:工具栏新增按钮 → `dialog.open({ multiple: true })` 一次选多个本地文件 → 逐条串行 `manage_files_upload`(复用单条的「覆盖前备份 3 份」语义,确认框写明)。
- **进度与取消**:复用 `files-transfer-progress` 订阅(进度条不变),`st.opLabel = '批量下载 (i/N)'`;**取消即中止**并把剩余项标「已取消(未执行)」(`parseErrCode === 'canceled'`)。
- **结果汇总**:结束后渲染逐条结果(成功/失败徽章 + 名称 + 文案,三列网格对齐),toast 汇总「N/M 成功」;切换目录/再次批量时清除;勾选集合以当前目录为键,`refreshList()` 时统一作废。
- **CSS**:`.files-pick-cell`(窄列居中)/`.files-batchbar`/`.files-batch-results .files-kv`(三列 grid);checkbox 走全局体系(`accent-color` + `tr:hover` 反白豁免均已存在)。

## 验证

- `node --check` files.js;**static-integrity 全绿**(首跑抓出 `files-batch-count` 缺 id 的悬空引用 → 已修,否则计数条静默不更新);verify 七脚本 rc=0;`cargo test` 531 不变(纯前端)
- **桩验证 + judge**:勾选 2 项(目录+文件)→ 批量条/半选态正确;批量下载 2/2(逐条结果 + toast + 选择自动清空),零未捕获异常;judge 两图 **pass**
- **本轮桩验证的抓错与教训**:首轮 judge 打回「批量条贴表格(间距 0)/结果列参差」——DOM 实测确认 `gapBarToTable=0`,根因是**预览页只破了脚本缓存、`style.css` 未加 `?v=` → IAB 返回旧样式表**(新增 CSS 全未生效);生成器补样式表版本号后实测 `gapBarToTable=12`、结果三列 x 坐标逐行一致(199/255/463),复判 **pass**。`AGENTS.md` 的 IAB 缓存提示已同步(脚本**与 style.css**)。

## 文件改动

| 文件 | 改动 |
|---|---|
| `ui/files.js` | 勾选列 / 表头全选 / 批量条 / 批量下载 / 批量上传 / `runBatch` 串行编排(取消即中止)/ 结果区块 |
| `ui/style.css` | `.files-pick-cell` / `.files-batchbar` / `.files-batch-results` 三列 grid |
| `ui/help.js` | 文件管理章节新增「多选批量传输」条目 |
| `wiki/03` / `AGENTS.md` / `ROADMAP.md` / `wiki/README.md` | 模块说明、IAB 缓存教训、状态与速览同步 |

---

# 第三十四批(五):宿主机白名单读写(第三十三批留档清尾,2026-09-22)

> **来源**:第三十四批方案的池外项(第三十三批明确留档「宿主机任意路径文件管理 — 仅开放部署目录且只读,
> 通用宿主文件管理风险高,另行评估」);用户裁决「**白名单目录可读写**」。**版本**:v6.16.0(同一未发版版本合并)。

## 口径(用户裁决 + 批次内定的默认设计)

- 新增设置项 **`AppSettings.hostWritePaths`**(camelCase;绝对路径前缀白名单,**默认空 = 维持第三十三批只读**;
  归一化:去空白/尾斜杠、拒相对路径与含 `..`、拒 NUL/换行/超长、去重,cap [`HOST_WRITE_PATHS_MAX`]=10)。
- **只放开写、不收紧读**(任意路径只读浏览不变);**仅作用于「部署目录」源** —— 容器/卷源不受限。
- **判定在服务端**:入口闸门 `ensure_host_write_allowed` 先在远端把「目标路径 + 各白名单条目」一次性
  `readlink -f` canonicalize(失败回落字面路径),再按**目录边界**做前缀匹配(白名单条目取「字面 ∪ canonical」
  两种形态,允许白名单自身是软链)—— 软链不能逃逸;不通过即拒并给出可操作文案(指向设置中心)。
- 覆盖前备份(本机 `fm-backups` 3 份)与前端二次确认沿用第三十三批;**读路径零改动**。

## 交付

- **后端**:`config.rs`(`host_write_paths` 字段 + `normalize_host_write_paths` / `path_within_any` 纯函数 + 保存侧归一化
  + 3 单测)、`manage_files.rs`(`canonicalize_paths_cmd` 纯函数 + `ensure_host_write_allowed` 闸门;`upload_local_file`
  入口接入(覆盖上传与文本写回);`manage_files_fs_op` 接入并**补上 Third-33 从未实现的 DeployDir 分支**;
  **审计顺带修掉两个真缺口** —— `upload_local_file` 的写入分支对 DeployDir 是 `unreachable!()`(白名单放开后会 panic)、
  备份分支对 DeployDir 恒「不存在」(会**静默跳过备份**),均已按宿主机 `cp` 语义实现;`is_read_only` 退休)。
- **前端**:`settings.js` 「通用」区新增多行输入(`settings-host-write-paths`;每行一个绝对路径,填表/保存载荷
  `hostWritePaths`)+ `files.js` 打开时读设置做**词法 UI 门控**(按钮禁用、行内改名/删除、提示行:「白名单内可写」/
  「仅白名单目录可写」两态),真正判定恒在后端。
- **文档**:wiki/02(模块说明)、wiki/03(设置与文件管理)、wiki/04(AppSettings + 文件命令行为)、wiki/07
  (决策 88:白名单写 + canonicalize + 取舍;决策 86 标注被本批取代)、help.js(文件管理两处 + 部署目录条目)。

## 验证

- `cargo test` **534 passed / 15 ignored**(531→534,+3 纯函数/契约单测);`cargo clippy` 11 条零新增
- `node --check` files.js / settings.js;static-integrity 全绿;verify 七脚本 rc=0
- **桩验证 + judge**:设置中心新字段回填/保存载荷 ✓;文件管理「部署目录」**白名单内可写**(提示行 + 上传/新建可用 + 行内改名/删除)与**白名单外只读**(warn 提示行 + 按钮置灰 + 行内仅下载/编辑)两态均按预期、零未捕获异常;judge **三图全 pass**。`verify/user-facing-copy.js` 的旧「部署目录只读」断言随口径更新为白名单表述(+ 新增批量传输断言),PASS 37/0
- 真机验证按既有约定由用户统一执行(闸门命令形态已随单测固化)

## 文件改动

| 文件 | 改动 |
|---|---|
| `src-tauri/src/config.rs` | `host_write_paths` 字段 + 2 纯函数 + 保存侧归一化 + 3 单测 + 2 处测试字面量补字段 |
| `src-tauri/src/manage_files.rs` | 闸门(命令拼装 + canonicalize 校验)+ 3 个写入口接入 + 补 DeployDir 写入/备份分支 + 模块文档 |
| `ui/settings.js` | 「宿主机可写目录」多行输入(构建/回填/保存载荷) |
| `ui/files.js` | `hostWritePaths` 读取 + `hostWritableHere` 词法门控(按钮/行内操作/两态提示行) |
| `wiki/02·03·04·07` / `help.js` / `ROADMAP.md` / `wiki/README.md` / `AGENTS.md` | 文档与计数(534)同步 |
