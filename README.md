# DockerDeploy SSH

Windows 桌面客户端(Tauri 2):把本地构建的 Docker 镜像一键部署到自己的生产服务器——`docker save → gzip → SFTP → docker load → compose up`,不依赖第三方镜像仓库与 CI/CD。支持单镜像与 compose 整栈两种部署模式。

## 文档

**完整项目文档在 [wiki/](wiki/README.md)** ——新会话/新成员从 [wiki/README.md](wiki/README.md) 读起,即可理解架构、模块、契约、构建与部署全貌。

历史设计过程文档在 `docs/`(spec / 实现计划 / UI 简报)。

## 快速开始

```bash
npm install          # 仅装 @tauri-apps/cli
npm run tauri dev    # 开发运行
npm run tauri build  # 发布构建(NSIS 安装包)
```

前置要求与详细说明见 [wiki/05-构建与运行.md](wiki/05-构建与运行.md)。
