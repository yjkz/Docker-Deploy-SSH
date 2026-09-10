# DockerDeploy SSH — 项目 Code Wiki

> 一套文档读完全项目。新会话从这里开始。

## 这个项目是什么

**Windows 桌面客户端(Tauri 2),把"本地构建的 Docker 镜像部署到自己的生产服务器"做成一键操作。**

背景:不使用第三方镜像仓库(Docker Hub/ACR 等)、不搭 CI/CD,部署走"方案 A"离线传输:

```
本地 docker save → gzip 压缩 → SFTP 上传 → 服务器 docker load → docker compose up -d
```

支持两种部署模式:

| 模式 | 单位 | 流程 | 适用 |
|---|---|---|---|
| **单镜像部署** | 一个镜像 | 打日期 tag → save+gzip → 上传 → 文件同步 → load → compose up(5 步) | 单服务项目 |
| **整栈部署(compose)** | 整个 compose 栈 | 导入 compose → 服务分类(本地传输/服务器拉取)→ 多镜像打包上传 → load → compose pull/up(6 步) | compose 编排的项目 |

配套能力:宿主机/服务器环境检测闸门(未通过禁止部署,提供一键启动/安装)、多服务器管理(SSH 密钥/密码/**加密私钥口令**三认证,密码 DPAPI 加密存储,主机密钥 TOFU 校验)、部署项目配置持久化、**智能传输(跳过未变化镜像)**、**部署断点续传**(失败/取消后从步骤 N 继续)、**多服务器批量部署**(逐台串行,独立历史可逐台回滚)、服务器端 releases 留档**一键回滚**、**通知中心**(桌面+SMTP 邮件)、**配置中心**(口令加密导出/导入/一键清除)、**设置中心**(系统托盘/检查更新/主题跟随系统)、**清理分析**(悬空镜像/停止容器/未用卷/build cache 预览+定向 prune)、**远程管理面板**(05 页:容器/镜像/卷/网络/Compose 栈/.env/实时监控/PTY 交互终端/**日志实时跟随**/**跨服务器镜像迁移**)、全站帮助系统(12 章节)。

## 文档目录(按阅读顺序)

| 文档 | 内容 | 何时读 |
|---|---|---|
| [01-架构总览.md](01-架构总览.md) | 技术栈、系统分层、三条数据流、目录结构 | 第一篇,建立全局图景 |
| [02-后端模块.md](02-后端模块.md) | 14 个功能 Rust 模块职责、关键结构体/函数签名、超时常量 | 改后端前 |
| [03-前端说明.md](03-前端说明.md) | 页面结构、JS 模块(含 manage/notify/config-io/settings/help)、全局约定(AppState/AppBus)、设计语言 | 改前端前 |
| [04-契约参考.md](04-契约参考.md) | **86 个命令 + 9 个事件 + 配置 JSON Schema 完整速查** | 跨前后端改动的对账表 |
| [05-构建与运行.md](05-构建与运行.md) | 环境要求、dev/build 命令、安装布局、日志、配置文件夹 | 跑起来之前 |
| [06-部署流程与回滚.md](06-部署流程与回滚.md) | 两条部署管线的逐步语义、智能传输、一键回滚、独立回滚中心(06 页)、环境检测闸门规则 | 理解/调试部署行为 |
| [07-安全与已知取舍.md](07-安全与已知取舍.md) | 敏感数据数据流、导出加密格式、注入防护、设计决策记录、已知限制 | 评估改动影响时 |

## 30 秒速览

```
仓库:E:\github\Docker-Deploy-SSH(git,主分支 main)
代码量:Rust ~19,280 行(17 文件,含 main.rs)+ 原生 JS/HTML/CSS ~15,700 行(无框架无打包器,字体 woff2 自捆绑)
技术:Tauri 2 + tokio + russh/russh-sftp + flate2 + serde_yaml + windows-dpapi + tauri-plugin-dialog
      + tauri-plugin-notification + lettre(rustls)+ argon2 + aes-gcm + reqwest(rustls+socks)+ ureq
前端调用后端:window.__TAURI__.core.invoke(86 个命令),事件 9 个,字段默认 snake_case
      (notify/config-io/settings/update/回滚列表与回滚中心/.env/批量/断点/清理/项目源更新/日志流/迁移契约为 camelCase,例外清单见 04)
页面:6 页导航(01 检测 / 02 镜像 / 03 服务器 / 04 部署 / 05 远程管理 / 06 回滚中心)+ 设置中心 + 全站帮助
构建:npm run tauri dev / npm run tauri build(产物 NSIS 安装包 ~8.2MB)
配置:安装目录 config/ 下 servers.json + projects.json + notify.json + settings.json +
      deployments.json + resume-deploy.json(便携式;原子写);日志 logs/app.log
测试:cargo test(纯函数单测 235 passed;真机测试 #[ignore] 12 个)
当前版本:v5.4.1(tauri.conf.json / Cargo.toml;第三批 + 自动更新重启/项目源绑定修复)
```

## 权威计划/完成记录(仓库根目录)

- `UPGRADE-PLAN.md` — 三批升级计划与完成记录,是 v4.6.0 后全部功能的事实来源:**第一批** v4.7-v5.1 五阶段(智能传输/一键回滚 → 通知中心 → 连接与数据安全 → 桌面体验 → 远程管理补遗,已一次性并入 v5.1.0 发布);**第二批** 阶段六至十(断点续传 ✅ / 批量部署 ✅ / 清理分析 ✅ / 实时日志 ✅ / 镜像迁移 ✅),阶段六起以 **v5.2.0** 发版,阶段九/十随 **v5.3.0** 收官;**第三批** 阶段十一至十四(清理分析重构与分项目清理 ✅ / 项目源更新 ✅ / 独立回滚中心 ✅ / 映射默认名 ✅),随 **v5.4.0** 发布(修复并入 **v5.4.1**:自动更新装完自动重启并提示、项目源绑定入口与检查汇总栏)
- `DOCKER-MANAGE-PLAN.md` — 远程管理模块三阶段实施计划与 A/B/C 完成记录(v4.5.0,历史事实来源)

## 历史过程文档(已清理)

v1 设计 spec、实现计划、UI 设计简报等过程产物原存放于 `docs/`,已在 v4.6.0 收尾(commit 3d6f9f3)整体删除;"为什么这么做"的记录由本 wiki 的设计决策记录(见 07)与 `UPGRADE-PLAN.md` / `DOCKER-MANAGE-PLAN.md` 的完成记录承接。冲突时以代码与本 wiki 为准。
