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

### 遗留与取舍(记录在 wiki 07 已知限制)

- 实时跟随行数上限 5000 由前端裁剪(后端只管推送);`manage-logs` 事件归属按「当前活跃会话唯一」过滤,后端 streamId 为后端代号,前端未知(全局单流语义下无歧义)
- 镜像迁移逐台串行、无断点(与批量部署同取舍);取消在镜像边界生效,大镜像传输中不即时中断
- exec_streaming 取消通道约定:全部 Sender drop(Err)同样按主动取消收场——命令层 stop 先发信号再 drop,时序保证正确


---

# 第三批升级（v5.4.0）

**目标**:修掉「清理分析识别不到服务器资源」的真因,把清理从「全局 prune」升级为「按服务器实际项目分列、可逐项勾选」;新增独立的回滚中心;补上「源 compose 变更后不必重新导入」的更新机制与文件映射默认名。

**进度**:阶段十一 ✅ / 阶段十二 ✅ / 阶段十三 ✅ / 阶段十四 ✅ —— 随 **v5.4.0** 发布

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

### 遗留与取舍(记录在 wiki 07 已知限制)

- 分项目标签归属按 compose `image:` 仓库名匹配:纯 build 服务或变量插值后仓库名不符时该项目的标签不出现在分项目块(不影响通用四节清理)
- 分项目扫描深度 ≤4 且归档目录名须匹配 `20*-*`;更深的目录或自定义命名归档不列出(可手动把扫描起点指到项目父目录)
- 清理执行按前端回传的显式目标,后端不做二次扫描:预览与执行之间服务器状态变化时对应条目报错,其余照常执行
- `unknown` 状态(旧配置无 `source_hash` 或手工项目)不参与启动自动更新,需手动点一次「从源更新」写入哈希
- 回滚中心的 `runningContainers` 以容器名包含目录名近似归属,标签缺失或 `docker ps` 不可用时可能为 0(不影响回滚)
