# DockerDeploy SSH — v4.7 升级计划（智能传输 / 通知 / 安全 / 桌面体验）

> 基于 v4.6.0 的五阶段升级计划,源自 2026-09-05 构想讨论的选定功能集。
>
> 硬约束:**低耦合,禁止对已有功能造成失效**;沿用「新增为主、追加为辅」;每阶段独立可交付、可发版。
>
> 进度:阶段一 ⬜|阶段二 ⬜|阶段三 ⬜|阶段四 ⬜|阶段五 ⬜

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

1. 阶段一(智能传输+回滚)→ 发 v4.7.0;2. 阶段二(通知)→ v4.8.0;3. 阶段三(安全)→ v4.9.0;4. 阶段四(桌面)→ v5.0.0(托盘+更新值得主版本);5. 阶段五(补遗)→ v5.1.0。每阶段走:实施 → 并行审查(验证+代码审查子代理)→ 验收 → 构筑推送发版(沿用 v4.6.0 流程)。
