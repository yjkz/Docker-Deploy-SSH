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

配套能力:宿主机/服务器环境检测闸门(未通过禁止部署,提供一键启动/安装)、多服务器管理(SSH 密钥/密码双认证,密码 DPAPI 加密存储)、部署项目配置持久化、服务器端 releases 留档回滚。

## 文档目录(按阅读顺序)

| 文档 | 内容 | 何时读 |
|---|---|---|
| [01-架构总览.md](01-架构总览.md) | 技术栈、系统分层、数据流、目录结构 | 第一篇,建立全局图景 |
| [02-后端模块.md](02-后端模块.md) | 7 个 Rust 模块职责、关键结构体/函数签名、超时常量 | 改后端前 |
| [03-前端说明.md](03-前端说明.md) | 页面结构、JS 模块、全局约定(AppState/AppBus)、设计语言 | 改前端前 |
| [04-契约参考.md](04-契约参考.md) | **19 个命令 + 4 个事件 + 配置 JSON Schema 完整速查** | 跨前后端改动的对账表 |
| [05-构建与运行.md](05-构建与运行.md) | 环境要求、dev/build 命令、安装布局、日志、配置文件夹 | 跑起来之前 |
| [06-部署流程与回滚.md](06-部署流程与回滚.md) | 两条部署管线的逐步语义、releases 回滚操作、环境检测闸门规则 | 理解/调试部署行为 |
| [07-安全与已知取舍.md](07-安全与已知取舍.md) | 密码数据流、注入防护、设计决策记录、已知限制 | 评估改动影响时 |

## 30 秒速览

```
仓库:E:\github\Docker-Deploy-SSH(git,主分支 main)
代码量:Rust ~7,560 行(7 模块)+ 原生 JS/HTML/CSS ~5,000 行(无框架无打包器)
技术:Tauri 2 + tokio + russh/russh-sftp + flate2 + serde_yaml + windows-dpapi + tauri-plugin-dialog + ureq
前端调用后端:window.__TAURI__.core.invoke(19 个命令),事件 4 个,字段 snake_case
构建:npm run tauri dev / npm run tauri build(产物 NSIS 安装包 ~3.0MB)
配置:安装目录 config/ 下 servers.json + projects.json + deployments.json(便携式);日志 logs/app.log
测试:cargo test(纯函数单测 134+;真机测试 #[ignore])
当前版本:v0.4.3
```

## 历史文档(docs/ 下,过程产物)

- `docs/superpowers/specs/2026-08-28-docker-deploy-ssh-design.md` — v1 设计 spec
- `docs/superpowers/plans/2026-08-28-docker-deploy-ssh.md` — v1 实现计划
- `docs/superpowers/plans/2026-08-29-compose-stack-deploy.md` — 整栈部署增量计划
- `docs/ui-redesign-brief-ark-light.md` — UI 设计语言简报(ark-light)

这些文档记录"为什么这么做";本 wiki 记录"现在是什么样"。冲突时以代码与本 wiki 为准。
