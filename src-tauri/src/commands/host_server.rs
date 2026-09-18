// // 宿主机检测与服务器操作

use super::*;

/// 检测宿主机 Docker 环境(内部多次调用 docker CLI,放 blocking 线程池避免卡 UI)。
#[tauri::command]
pub async fn host_check() -> Result<HostCheckReport, String> {
    Ok(tauri::async_runtime::spawn_blocking(check_host)
        .await
        .map_err(|e| format!("宿主机检测任务失败: {}", e))?)
}

/// 拉起 Docker 守护进程(可能阻塞最长 60 秒,放 blocking 线程池)。
#[tauri::command]
pub async fn start_docker() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(start_daemon)
        .await
        .map_err(|e| format!("启动 Docker 任务失败: {}", e))?
}

/// 列出本地全部镜像。
#[tauri::command]
pub async fn list_images() -> Result<Vec<ImageInfo>, String> {
    tauri::async_runtime::spawn_blocking(crate::docker::list_images)
        .await
        .map_err(|e| format!("获取镜像列表任务失败: {}", e))?
}

/// 扫描本地悬空镜像(第二十五批;`<none>:<none>` 层,只读)。
#[tauri::command]
pub async fn list_dangling_images() -> Result<Vec<crate::docker::DanglingImage>, String> {
    tauri::async_runtime::spawn_blocking(crate::docker::list_dangling_images)
        .await
        .map_err(|e| format!("扫描悬空镜像任务失败: {}", e))?
}

/// 删除指定 ID 的本地镜像(第二十五批;逐个 `docker rmi`,返回执行摘要)。
///
/// 校验:ids 非空且**全部**为 `sha256:` 开头的完整 ID(id 来自
/// [`list_dangling_images`] 的 `--no-trunc` 输出;拒绝 `repo:tag` 形态 ——
/// 用引用删除可能连带删掉同名多标签,与用户勾选不符)。单个失败不中断。
#[tauri::command]
pub async fn remove_local_images(ids: Vec<String>) -> Result<LocalImageRemoval, String> {
    if ids.is_empty() {
        return Err("未指定要删除的镜像".to_string());
    }
    if let Some(bad) = ids
        .iter()
        .find(|i| !i.starts_with("sha256:") || i.len() <= "sha256:".len())
    {
        return Err(format!(
            "镜像 ID 格式不合法({}):须为 sha256: 开头的完整 ID",
            bad
        ));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let (removed, failed) = crate::docker::remove_images_by_id(&ids);
        LocalImageRemoval { removed, failed }
    })
    .await
    .map_err(|e| format!("删除本地镜像任务失败: {}", e))
}

/// [`remove_local_images`] 的执行摘要(camelCase 契约)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalImageRemoval {
    pub removed: usize,
    /// 失败项(格式 `id: 原因`),前端原样展示
    pub failed: Vec<String>,
}

// ===== 服务器一键诊断(第二十批阶段三)=====

/// 诊断步骤的结果(前端红绿灯渲染;camelCase)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnoseStep {
    /// 步骤键:`tcp` / `ssh` / `docker`
    pub step: String,
    /// `ok` / `fail` / `skipped`(前置失败则后续 skipped,不再尝试)
    pub state: String,
    /// 人类可读说明(成功为通过描述;失败为原因;skipped 为未执行原因)
    pub detail: String,
}

/// 服务器一键诊断的整份报告(逐层短路:TCP 不通则 SSH/Docker 不再尝试)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnoseReport {
    pub steps: Vec<DiagnoseStep>,
    /// 全部步骤 ok(= 可部署的最低保障:连得上 + docker 可用)
    pub all_ok: bool,
}

/// 一键诊断:TCP 可达 → SSH 连接(横幅/TOFU/认证/传输错误分类)→ docker 可用。
///
/// 与 `test_server` 的区别:环境检测失败只给一句合并文案,用户分不清
/// 「密码错 / 网络断 / 主机密钥变了 / docker 没装」;本命令逐层给判定,
/// 前端红绿灯呈现。诊断用已存凭据(不收明文密码;改密码请走「测试连接」)。
#[tauri::command]
pub async fn server_diagnose(server_id: String) -> Result<DiagnoseReport, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    let mut steps: Vec<DiagnoseStep> = Vec::new();

    // 层 1:TCP 可达(与 probe.rs 探活同口径,5s 超时)
    let addr = format!("{}:{}", server.host, server.port);
    let tcp_ok = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            steps.push(DiagnoseStep {
                step: "tcp".into(),
                state: "fail".into(),
                detail: format!("无法连接 {}:{}({})", server.host, server.port, e),
            });
            false
        }
        Err(_) => {
            steps.push(DiagnoseStep {
                step: "tcp".into(),
                state: "fail".into(),
                detail: format!("连接 {}:{} 超时(5 秒):地址/端口不对或防火墙拦截", server.host, server.port),
            });
            false
        }
    };
    if !tcp_ok {
        steps.push(DiagnoseStep { step: "ssh".into(), state: "skipped".into(), detail: "TCP 不通,未尝试".into() });
        steps.push(DiagnoseStep { step: "docker".into(), state: "skipped".into(), detail: "TCP 不通,未尝试".into() });
        return Ok(DiagnoseReport { steps, all_ok: false });
    }
    steps.push(DiagnoseStep {
        step: "tcp".into(),
        state: "ok".into(),
        detail: format!("{}:{} 端口可达", server.host, server.port),
    });

    // 层 2:SSH 连接(错误码分类:auth = 密码/私钥/口令/TOFU 问题,transport = 网络层)
    //
    // 凭据解析(v6.3.1 修复):诊断必须与其他所有连接点同款,先经
    // resolve_password / resolve_key_passphrase 解密已存凭据 —— 此前直传
    // `None, None`,加密私钥的服务器在诊断里 load_secret_key(path, None)
    // 报「私钥已加密,需提供口令」,而「测试连接」正常(它解析了口令),
    // 症状即「测试连接通过、一键诊断失败」。
    let password = match resolve_password(
        &server.auth.auth_type,
        None,
        server.auth.password_enc.as_deref(),
    ) {
        Ok(p) => p,
        Err(e) => {
            steps.push(DiagnoseStep {
                step: "ssh".into(),
                state: "fail".into(),
                detail: format!("凭据解析失败:{}", crate::errors::strip(&e)),
            });
            steps.push(DiagnoseStep { step: "docker".into(), state: "skipped".into(), detail: "SSH 未连接,未尝试".into() });
            return Ok(DiagnoseReport { steps, all_ok: false });
        }
    };
    let key_pass = match resolve_key_passphrase(&server) {
        Ok(k) => k,
        Err(e) => {
            steps.push(DiagnoseStep {
                step: "ssh".into(),
                state: "fail".into(),
                detail: format!("凭据解析失败:{}", crate::errors::strip(&e)),
            });
            steps.push(DiagnoseStep { step: "docker".into(), state: "skipped".into(), detail: "SSH 未连接,未尝试".into() });
            return Ok(DiagnoseReport { steps, all_ok: false });
        }
    };
    let ssh_result = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await;
    match ssh_result {
        Err(e) => {
            let code = crate::errors::code_of(&e);
            let (state, hint) = match code {
                Some(crate::errors::ErrCode::Auth) => ("fail", "认证/主机密钥被拒".to_string()),
                Some(crate::errors::ErrCode::Timeout) => ("fail", "SSH 握手超时".to_string()),
                _ => ("fail", "SSH 连接失败".to_string()),
            };
            steps.push(DiagnoseStep {
                step: "ssh".into(),
                state: state.into(),
                detail: format!("{}:{}", hint, crate::errors::strip(&e)),
            });
            steps.push(DiagnoseStep { step: "docker".into(), state: "skipped".into(), detail: "SSH 未连接,未尝试".into() });
            return Ok(DiagnoseReport { steps, all_ok: false });
        }
        Ok(mut client) => {
            steps.push(DiagnoseStep {
                step: "ssh".into(),
                state: "ok".into(),
                detail: format!("SSH 连接成功(用户 {})", server.username),
            });
            // 层 3:docker 可用(docker --version 退出码;失败再探权限)
            let docker_ok = match exec_collect(&mut client, "docker --version").await {
                Ok((0, out)) => {
                    steps.push(DiagnoseStep {
                        step: "docker".into(),
                        state: "ok".into(),
                        detail: out.trim().to_string(),
                    });
                    true
                }
                Ok((code, out)) => {
                    // 退出码非 0:优先怀疑权限(docker.sock 无权 → 提示加组)
                    let perm = exec_collect(&mut client, "docker info >/dev/null 2>&1; echo $?").await;
                    let detail = match perm {
                        Ok((_, ref o)) if o.trim() == "1" => format!(
                            "docker 已安装但当前用户无权限(退出码 {}):请将用户加入 docker 组或用 root 连接。原始输出: {}",
                            code,
                            out.trim()
                        ),
                        _ => format!("docker 命令失败(退出码 {}):可能未安装。输出: {}", code, out.trim()),
                    };
                    steps.push(DiagnoseStep {
                        step: "docker".into(),
                        state: "fail".into(),
                        detail,
                    });
                    false
                }
                Err(e) => {
                    steps.push(DiagnoseStep {
                        step: "docker".into(),
                        state: "fail".into(),
                        detail: format!("探测 docker 失败:{}", crate::errors::strip(&e)),
                    });
                    false
                }
            };
            let _ = docker_ok;
            let all_ok = steps.iter().all(|s| s.state == "ok");
            Ok(DiagnoseReport { steps, all_ok })
        }
    }
}

// ===== 服务器命令 =====

/// 连接服务器并检查远端环境(docker/compose/gzip/远端目录/磁盘空间)。
///
/// `key_passphrase` 为本次测试一次性输入的私钥口令(优先于已存储的
/// `key_pass_enc`);`remember_key_pass` 为 true 且口令非空时,连接成功后
/// DPAPI 加密存入 `AuthConfig.key_pass_enc`(下次免输入)。
#[tauri::command]
pub async fn test_server(
    server_id: String,
    password_plain: Option<String>,
    key_passphrase: Option<String>,
    remember_key_pass: Option<bool>,
) -> Result<ServerCheckReport, String> {
    let report = connect_and_check(&server_id, password_plain, key_passphrase.clone()).await?;
    // 连接成功后才保存口令:口令错误时建连必失败,保存坏口令没有意义
    if remember_key_pass.unwrap_or(false) {
        remember_key_passphrase(&server_id, key_passphrase)?;
    }
    Ok(report)
}

/// 与 `test_server` 等价的环境检查(独立命令名,语义 = 部署前环境自检)。
#[tauri::command]
pub async fn server_env_check(
    server_id: String,
    password_plain: Option<String>,
    key_passphrase: Option<String>,
) -> Result<ServerCheckReport, String> {
    connect_and_check(&server_id, password_plain, key_passphrase).await
}

/// 连接 + 远端环境检查的公共实现。
async fn connect_and_check(
    server_id: &str,
    password_plain: Option<String>,
    key_passphrase: Option<String>,
) -> Result<ServerCheckReport, String> {
    let (server, mut client) = connect_server(
        server_id,
        password_plain.as_deref(),
        key_passphrase.as_deref(),
    )
    .await?;
    with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "环境检查超时",
        "请检查服务器网络后重试",
        check_server_env(&mut client, &server.remote_dir),
    )
    .await
}

/// 按 server_id 解析服务器配置、密码与私钥口令并建立 SSH 连接
/// (带连接超时兜底)。`connect_and_check` 与 [`preview_stack_changes`] 共用的
/// 建连路径,返回 `(服务器配置, 已连接的客户端)`。
///
/// - `password_plain`:前端临时输入的 SSH 密码(密码认证时优先于已保存密文);
/// - `key_passphrase_plain`:一次性输入的私钥口令(优先于已存储的
///   `key_pass_enc`;无输入时用存储值,均为 `None` 则按无口令私钥加载)。
/// - 主机密钥 TOFU:连接成功后首次观察到指纹时落盘到 `ServerConfig`
///   (见 [`persist_host_key_if_needed`])。
pub(crate) async fn connect_server(
    server_id: &str,
    password_plain: Option<&str>,
    key_passphrase_plain: Option<&str>,
) -> Result<(ServerConfig, SshClient), String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain,
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = match key_passphrase_plain.filter(|p| !p.is_empty()) {
        // 一次性输入的口令优先于已存储密文(空串视为未输入)
        Some(p) => Some(p.to_string()),
        None => resolve_key_passphrase(&server)?,
    };
    // observed:记录本次连接观察到的主机指纹,首次连接后 TOFU 落盘
    let observed = Arc::new(OnceLock::new());
    let client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::clone(&observed)),
    )
    .await?;
    persist_host_key_if_needed(&server, &observed.get().cloned());
    Ok((server, client))
}

/// 给可能长时间无响应的 SSH future 整体套一层超时(兜底 russh 自身不带连接超时)。
///
/// 超时错误格式 `{desc}({secs} 秒):{hint}`,如
/// “连接超时(15 秒):请检查服务器地址与网络”。
pub(crate) async fn with_timeout<T>(
    secs: u64,
    desc: &str,
    hint: &str,
    fut: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(res) => res,
        Err(_) => Err(format!("{}({} 秒):{}", desc, secs, hint)),
    }
}

/// 在远端执行官方脚本安装 Docker(最长 1800 秒),输出逐行 emit `server-log`。
#[tauri::command]
pub async fn install_server_docker(
    server_id: String,
    password_plain: Option<String>,
    app: AppHandle,
) -> Result<(), String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    // 建连超时兜底(第二十批 P2 修复):与 connect_server 同款 15s 包裹
    // (安装 Docker 全流程另有 1800s 整体超时,但建连挂起会先耗在这里)
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    let mut on_output = |line: &str| {
        let _ = app.emit("server-log", line.trim_end().to_string());
    };
    let fut = client.exec(INSTALL_DOCKER_CMD, &mut on_output);
    let code = match tokio::time::timeout(Duration::from_secs(1800), fut).await {
        Ok(res) => res.map_err(|e| format!("执行 Docker 安装命令失败: {}", e))?,
        Err(_) => return Err("安装 Docker 超时(1800 秒),请检查服务器网络后重试".to_string()),
    };
    if code != 0 {
        return Err(format!(
            "Docker 安装脚本退出码 {},请根据安装日志排查(常见原因:网络不通、需要 root)",
            code
        ));
    }
    Ok(())
}

/// 在远端创建服务器配置的部署根目录(`mkdir -p <remote_dir>`)。
#[tauri::command]
pub async fn create_remote_dir(
    server_id: String,
    password_plain: Option<String>,
) -> Result<(), String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;
    create_dir_on(&mut client, &server.remote_dir).await
}

/// 检测远端任意目录是否存在(项目表单的「检测目录」用)。
///
/// 与 [`create_remote_dir`] 的区别:目录由调用方给出(项目级 `remote_dir` 可能
/// 与服务器级目录不同),且**只读**。不校验路径是否在服务器目录之下 —— 项目级
/// 目录本就是独立绝对路径(见 [`effective_remote_dir`]),限制前缀反而会挡住正当用法;
/// 前端已强制绝对路径,这里再兜一次防误传相对路径在服务器上乱建。
#[tauri::command]
pub async fn check_remote_dir(
    server_id: String,
    dir: String,
    password_plain: Option<String>,
) -> Result<bool, String> {
    let dir = dir.trim().to_string();
    if !dir.starts_with('/') {
        return Err("远程部署目录需为以 / 开头的绝对路径".to_string());
    }
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    find_server(&cfg, &server_id)?;
    let (_srv, mut client) = connect_server(&server_id, password_plain.as_deref(), None).await?;
    let (code, _out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "检测目录超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &test_dir_cmd(&dir)),
    )
    .await?;
    Ok(code == 0)
}

/// 在远端创建任意指定目录(项目表单的「创建该目录」用)。
///
/// 与 [`check_remote_dir`] 同一路径口径:接受项目级独立目录,仅要求绝对路径。
#[tauri::command]
pub async fn create_remote_dir_at(
    server_id: String,
    dir: String,
    password_plain: Option<String>,
) -> Result<(), String> {
    let dir = dir.trim().to_string();
    if !dir.starts_with('/') {
        return Err("远程部署目录需为以 / 开头的绝对路径".to_string());
    }
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    find_server(&cfg, &server_id)?;
    let (_srv, mut client) = connect_server(&server_id, password_plain.as_deref(), None).await?;
    create_dir_on(&mut client, &dir).await
}

/// `mkdir -p` 远端目录(两个创建命令共用;路径经单引号转义)。
async fn create_dir_on(client: &mut SshClient, dir: &str) -> Result<(), String> {
    let cmd = mkdir_p_cmd(dir);
    let code = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "创建目录超时",
        "请检查服务器网络后重试",
        async {
            client
                .exec(&cmd, &mut |_| {})
                .await
                .map_err(|e| format!("远端创建目录失败: {}", e))
        },
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "远端创建目录 {} 失败(退出码 {},常见原因:无写入权限)",
            dir, code
        ));
    }
    Ok(())
}

// ===== 服务器清理 + 远端磁盘预检(Task 2)=====

/// 清理服务器:删除悬空镜像与已停止容器(见 [`prune_cmd`]),输出逐行 emit
/// `server-log`(与 [`install_server_docker`] 同通道,前端服务器卡片可回显),
/// 超时 [`PRUNE_TIMEOUT_SECS`] 秒。
#[tauri::command]
pub async fn prune_server(
    server_id: String,
    password_plain: Option<String>,
    app: AppHandle,
) -> Result<(), String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain.as_deref(),
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    let mut on_output = |line: &str| {
        let _ = app.emit("server-log", line.trim_end().to_string());
    };
    let cmd = prune_cmd();
    let fut = client.exec(&cmd, &mut on_output);
    let code = with_timeout(
        PRUNE_TIMEOUT_SECS,
        "服务器清理超时",
        "请检查服务器网络后重试",
        async { fut.await.map_err(|e| format!("执行服务器清理命令失败: {}", e)) },
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "服务器清理命令退出码 {},请根据输出排查(常见原因:无 docker 权限)",
            code
        ));
    }
    Ok(())
}

/// 拼装服务器清理命令:删除悬空镜像与已停止容器(`-f` 免交互;用 `;` 串联,
/// 第二项不受第一项退出码影响,两项均尽力执行)。
pub fn prune_cmd() -> String {
    "docker image prune -f; docker container prune -f".to_string()
}

/// 拼装查询 Docker 数据根目录的命令(`docker info` 的 Go 模板,单行输出)。
pub fn docker_root_cmd() -> String {
    "docker info -f '{{.DockerRootDir}}'".to_string()
}

/// 拼装查询 `path` 所在文件系统剩余空间(KB)的命令。
/// 用 `df -k`(1K 块)—— 所有 df(GNU coreutils 与 BusyBox)都支持的最通用口径;
/// 此前用 `-PBG`(以 GB 为块 + POSIX 单行),BusyBox 的 df 不支持 `-B` 会报错拿不到数据
/// (wiki/07 限制 4)。`tail -1` 取数据行,`awk` 取第 4 列(Available,KB);路径单引号包裹防注入。
pub fn df_free_gb_cmd(path: &str) -> String {
    format!(
        "df -k {} | tail -1 | awk '{{print $4}}'",
        shell_single_quote(path)
    )
}

/// 解析 `df -k` 第 4 列的 Available 值(1K 块数)为 GB 数;
/// 空输出或非数字返回 `None`。兼容传入已带 `G` 后缀的旧口径(去后缀按 GB 直读)。
pub fn parse_df_gb(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 旧口径带 G 后缀 → 已是 GB
    if let Some(g) = trimmed.strip_suffix('G') {
        return g.trim().parse::<f64>().ok();
    }
    // 新口径:1K 块数 → GB(1024^2)
    trimmed.parse::<f64>().ok().map(|kb| kb / 1024.0 / 1024.0)
}

/// 远端磁盘预检判定(纯函数):剩余空间(GB)小于 `need_bytes`(已含余量)换算的
/// GB 数 → 返回中文错误(含所需/实际 GB);`free_gb` 为 `None` 表示无法获取剩余
/// 空间,跳过预检返回 `Ok(())`(告警由调用方负责)。
pub fn precheck_remote_disk(free_gb: Option<f64>, need_bytes: u64) -> Result<(), String> {
    let free = match free_gb {
        Some(v) => v,
        None => return Ok(()),
    };
    let need_gb = need_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    if free < need_gb {
        return Err(format!(
            "服务器磁盘剩余空间不足:本次部署约需 {:.1} GB,Docker 根目录所在盘仅剩 {:.1} GB,请先清理服务器磁盘后重试",
            need_gb, free
        ));
    }
    Ok(())
}

// ===== 部署断点续传(UPGRADE-PLAN 阶段六)=====

