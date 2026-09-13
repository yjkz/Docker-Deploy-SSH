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
模态存在)。真机验收(明细加载速度对比 + 说明编辑全流程)待用户在真实服务器执行。

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
  覆盖解析、实机数字待用户下次部署时与 `df -h` 对照

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
  待用户下次实际部署时确认

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
  改动);浏览器桩截图(标题+说明双输入渲染)交 judge;路径翻倍修复
  待用户真机复测保存版本标题/说明

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
  纯函数单测覆盖,真机悬停效果待用户确认(托盘悬停 1-2 秒出现)
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
  悬停效果待用户确认)

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
