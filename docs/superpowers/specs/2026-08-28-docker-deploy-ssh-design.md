# DockerDeploy SSH 桌面客户端 — 设计文档

日期:2026-08-28
状态:待用户评审

## 1. 目标

为"本地构建 Docker 镜像 → 传输到生产服务器部署(方案 A:save/gzip → SFTP 上传 → load → compose up)"提供一键化的轻量桌面客户端。不依赖第三方镜像仓库或 CI/CD。

非目标(YAGNI):镜像仓库管理、日志分析、定时任务、服务器端常驻组件、跨平台(仅 Windows 优先)。

## 2. 技术栈与结构

- **Tauri 2.x + 原生 HTML/JS 前端**(不引入大型前端框架)
- **Rust 后端**:
  - Docker 操作:调用本机 Docker CLI(`docker info / images / save / start` 等)
  - 压缩:Rust `flate2` 库(gzip),不依赖系统 gzip 命令
  - SSH/SFTP:`russh` + `russh-sftp`(支持私钥与账号密码两种认证)
  - 密码加密存储:Windows DPAPI
- **项目根目录**:`D:\Github-repositories\docker-deploy-ssh`
- **配置存储**:应用文件夹(即安装目录下的 `config/` 子目录,便携式配置),含 `servers.json`(服务器列表)、`projects.json`(部署项目配置)

## 3. 环境检测模块(前置闸门)

部署动作前必须通过环境检测,否则禁用相关操作按钮。

### 3.1 宿主机(Windows 本机)检测项

| 检测项 | 检测方式 | 不通过时提供 |
|---|---|---|
| Docker 已安装 | `docker --version` | 显示安装指引/命令(Docker Desktop 下载链接 + winget 命令) |
| Docker 守护进程运行中 | `docker info` 退出码 | 一键启动按钮(启动 Docker Desktop / `Start-Service docker`)+ 命令文本 |
| docker compose 可用 | `docker compose version` | 安装指引 |
| Docker 与本机架构 | 读取 `docker info` 中 architecture,与镜像架构比对提示 | 提示文本 |
| 磁盘空间(临时目录) | 检查导出目录剩余空间 ≥ 镜像大小 × 1.5 | 清理提示 |

### 3.2 服务器(Linux)检测项

在"服务器管理页"提供"环境检测"按钮,SSH 执行探测命令:

| 检测项 | 检测方式 | 不通过时提供 |
|---|---|---|
| docker 命令 | `docker --version` | 显示 apt/yum 一键安装按钮(SSH 执行安装脚本)+ 命令文本 |
| docker compose 插件 | `docker compose version` | 同上 |
| gzip | `gzip --version` | 同上 |
| 远程项目路径存在 | `test -d <path> && echo ok` | 提供"一键创建目录"按钮 |
| 磁盘空间 | `df` 解析 | 清理提示 |

- 一键安装:SSH 执行标准安装脚本(如 `curl -fsSL https://get.docker.com | sh`),执行过程实时回显输出,失败时显示原始输出与命令,允许用户复制后自行安装。
- 检测结果缓存显示;每次进入部署向导前强制重新检测宿主机项。

## 4. 界面(4 页)

1. **环境检测页**(首页):宿主机检测项逐条显示 通过/未通过;未通过项含一键启动/安装按钮与可复制的安装命令。全部通过后其他页面可用。
2. **镜像列表页**:`docker images` 表格(仓库、tag、大小、创建时间),搜索过滤,每行"部署"按钮进入部署向导。
3. **服务器管理页**:服务器 CRUD(名称、IP、端口、用户名、私钥路径或密码、远程项目路径),测试连接、环境检测(见 3.2)。
4. **部署向导页**:选择 镜像 + 服务器(项目配置含文件映射列表:本地路径 → 服务器相对路径)→ 按序执行:
   1. 镜像打日期 tag(可选,默认开,如 `myapp:20260828-HHmmss`)
   2. `docker save` 导出 + flate2 gzip 压缩(进度)
   3. SFTP 上传镜像包 + 文件映射(进度)
   4. SSH:`docker load` → 同步文件就位 → `docker compose up -d`
   5. 每步实时输出回显,失败即中止并展示错误与日志位置

## 5. 配置结构

```jsonc
// servers.json
[{ "id": "...", "name": "生产", "host": "1.2.3.4", "port": 22,
   "username": "root", "auth": {"type": "key"|"password", "keyPath": "...", "passwordEnc": "..."},
   "remoteDir": "/opt/myapp" }]

// projects.json
[{ "id": "...", "name": "我的应用", "imageFilter": "myapp",
   "fileMappings": [{"local": "D:/proj/docker-compose.yml", "remote": "docker-compose.yml"},
                    {"local": "D:/proj/sql", "remote": "sql", "dir": true}],
   "composeFile": "docker-compose.yml" }]
```

## 6. 错误处理与日志

- 每步失败即中止,保留已上传文件;界面显示失败步骤、原始输出;日志写入应用文件夹 `logs/`。
- 上传大文件支持后续版本的断点续传(本期不做)。

## 7. 测试策略

- Rust 单元测试:配置读写、文件映射解析、ssh 命令拼装(不真正连接)。
- 手工集成测试清单:检测页各失败分支(模拟 docker 未启动)、密钥/密码两种 SSH、真实部署到测试服务器、回滚(重新部署旧 tag)。
