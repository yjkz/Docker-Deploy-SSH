//! Docker CLI 封装 + 宿主机检测。
//!
//! 一律通过调用本机 `docker` 命令行(`docker info / images / save / tag`)
//! 完成,不直连 Docker API。所有参数均作为子进程参数直接传递,
//! 不经 shell 拼接,避免注入风险。

use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Docker 安装/运行状态(brief 约定的对外枚举)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerCheck {
    /// 已安装且守护进程在运行
    Installed,
    /// 未安装 docker CLI
    NotInstalled,
    /// 已安装但守护进程未运行
    DaemonNotRunning,
}

/// 宿主机检测结果报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostCheckReport {
    pub docker_installed: bool,
    pub daemon_running: bool,
    pub compose_ok: bool,
    pub docker_version: Option<String>,
    pub arch: Option<String>,
    pub error: Option<String>,
}

/// 一条镜像信息(`docker images --format {{json .}}` 每行解析出一条)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageInfo {
    pub repository: String,
    pub tag: String,
    /// 人类可读 Size("300MB")换算后的字节数(1024 进制;解析失败记 0)
    pub size_bytes: u64,
    pub created: String,
    pub id: String,
}

/// 执行 `docker <args>`,成功返回 (stdout, stderr) 文本;
/// 失败返回面向用户可读的中文错误信息(优先取子进程 stderr)。
fn run_docker(args: &[&str]) -> Result<(String, String), String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "未找到 docker 命令,请确认已安装 Docker Desktop 并加入 PATH".to_string()
            } else {
                format!("执行 docker 命令失败: {}", e)
            }
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        Ok((stdout, stderr))
    } else if stderr.trim().is_empty() {
        Err(format!(
            "docker {} 执行失败(退出码 {:?})",
            args.join(" "),
            output.status.code()
        ))
    } else {
        Err(stderr.trim().to_string())
    }
}

/// 检测宿主机 Docker 环境:是否安装 CLI、守护进程是否运行、compose 是否可用,
/// 并顺带取回 docker 版本与 CPU 架构。
pub fn check_host() -> HostCheckReport {
    let mut report = HostCheckReport {
        docker_installed: false,
        daemon_running: false,
        compose_ok: false,
        docker_version: None,
        arch: None,
        error: None,
    };

    // 1) docker --version:判断 CLI 是否安装
    match run_docker(&["--version"]) {
        Ok((out, _)) => {
            report.docker_installed = true;
            report.docker_version = Some(out.trim().to_string());
        }
        Err(e) => {
            report.error = Some(format!("未检测到 Docker CLI:{}", e));
            return report;
        }
    }

    // 2) docker info:判断守护进程是否运行(stderr 用于 error 提示)
    match run_docker(&["info"]) {
        Ok(_) => report.daemon_running = true,
        Err(e) => {
            report.error = Some(format!("Docker 守护进程未运行:{}", e));
            return report;
        }
    }

    // 3) CPU 架构
    if let Ok((out, _)) = run_docker(&["info", "--format", "{{.Architecture}}"]) {
        let arch = out.trim().to_string();
        if !arch.is_empty() {
            report.arch = Some(arch);
        }
    }

    // 4) docker compose version:判断 compose 插件是否可用
    match run_docker(&["compose", "version"]) {
        Ok(_) => report.compose_ok = true,
        Err(e) => {
            report.error = Some(format!("docker compose 插件不可用:{}", e));
        }
    }

    report
}

/// 尝试拉起 Docker 守护进程(Windows):
/// 1. 先试 `powershell -Command Start-Service com.docker.service`(失败不致命,如无管理员权限);
/// 2. 再启动 `"%ProgramFiles%\Docker\Docker\Docker Desktop.exe"`;
/// 3. 轮询 `docker info`,最多 60 秒,每 2 秒一次。
pub fn start_daemon() -> Result<(), String> {
    // 守护进程已在运行则直接返回,避免重复拉起 Docker Desktop
    if run_docker(&["info"]).is_ok() {
        return Ok(());
    }

    // 1) 尝试启动 Windows 服务(失败不致命)
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-Command", "Start-Service com.docker.service"])
        .output();

    // 2) 启动 Docker Desktop
    let desktop = std::env::var("ProgramFiles")
        .map(|pf| PathBuf::from(pf).join(r"Docker\Docker\Docker Desktop.exe"))
        .map_err(|_| "未找到 ProgramFiles 环境变量,无法自动启动 Docker Desktop".to_string())?;
    if desktop.exists() {
        if let Err(e) = Command::new(&desktop).spawn() {
            log::warn!("启动 Docker Desktop 失败: {}", e);
        }
    } else {
        log::warn!("未找到 Docker Desktop: {}", desktop.display());
    }

    // 3) 轮询 docker info,最多 60 秒
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if run_docker(&["info"]).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                "等待 Docker 守护进程启动超时(60 秒),请手动打开 Docker Desktop 后重试"
                    .to_string(),
            );
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// 列出本地全部镜像(逐行解析 `docker images --format {{json .}}`);
/// 单行解析失败时记日志跳过,不让整条命令失败。
pub fn list_images() -> Result<Vec<ImageInfo>, String> {
    let (stdout, _) = run_docker(&["images", "--format", "{{json .}}"])?;
    let mut images = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_image_line(line) {
            Ok(info) => images.push(info),
            Err(e) => log::warn!("跳过无法解析的镜像行: {}", e),
        }
    }
    Ok(images)
}

/// 给本地镜像打新标签;image/new_tag 直接作为子进程参数,不经 shell。
pub fn tag_image(image: &str, new_tag: &str) -> Result<(), String> {
    run_docker(&["tag", image, new_tag])
        .map(|_| ())
        .map_err(|e| format!("打标签失败: {}", e))
}

/// `docker images --format {{json .}}` 的原始 JSON 行结构
/// (docker 输出固定为大写字段名,统一改名映射到蛇形变量)。
#[derive(Debug, Deserialize)]
struct ImageJsonLine {
    #[serde(rename = "Repository", default)]
    repository: String,
    #[serde(rename = "Tag", default)]
    tag: String,
    #[serde(rename = "Size", default)]
    size: String,
    #[serde(rename = "CreatedAt", default)]
    created_at: String,
    #[serde(rename = "ID", default)]
    id: String,
}

/// 解析 `docker images --format {{json .}}` 的一行 JSON 为 [`ImageInfo`]。
/// Size 人类可读字符串换算为字节数(解析失败记 0)。
pub fn parse_image_line(line: &str) -> Result<ImageInfo, String> {
    let raw: ImageJsonLine = serde_json::from_str(line)
        .map_err(|e| format!("解析镜像信息失败: {} | 原始行: {}", e, line))?;
    Ok(ImageInfo {
        repository: raw.repository,
        tag: raw.tag,
        size_bytes: size_to_bytes(&raw.size),
        created: raw.created_at,
        id: raw.id,
    })
}

/// 将 docker 的人类可读大小("300MB"、"1.5GB")换算为字节数(1024 进制);
/// 支持单位 B/KB/MB/GB/TB(大小写不敏感);解析失败记 0。
pub fn size_to_bytes(s: &str) -> u64 {
    let s = s.trim();
    // 数字部分在前,单位部分从第一个非数字字符开始
    let (num_part, unit_part) = match s.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(i) => (&s[..i], s[i..].trim()),
        None => (s, ""),
    };
    let num: f64 = match num_part.parse() {
        Ok(v) => v,
        Err(_) => return 0, // 解析失败记 0
    };
    let mult: f64 = match unit_part.to_uppercase().as_str() {
        "" | "B" => 1.0,
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return 0, // 未知单位记 0
    };
    (num * mult) as u64
}

/// 生成部署用标签:`repository:<YYYYmmdd-HHMMSS>`(本地时间)。
/// 注意:同一秒内多次调用会生成相同标签,唯一性由上层(Task 5)保证。
pub fn make_deploy_tag(repository: &str, _tag: &str) -> String {
    format!(
        "{}:{}",
        repository,
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    )
}

/// 统计实际落盘字节数的包装层:夹在 GzEncoder 与 BufWriter 之间,
/// 因此统计到的是 gzip 压缩后的字节(即最终文件大小)。
struct CountingWriter<W: Write> {
    inner: W,
    counter: Arc<AtomicU64>,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.counter.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// 将镜像导出为 gzip 压缩的 tar 文件:
/// `docker save <image>` 子进程 stdout → flate2 流式压缩 → out_path,
/// 不依赖系统 gzip 命令。
///
/// * `progress_cb`:每读完一段数据回调一次,参数 = 已写入的压缩后字节数;
///   注意回调为 `Fn`,需要在回调里累计状态时请用 `Cell`/`AtomicU64` 等共享句柄。
/// * 返回值 = 压缩后文件总字节数。
pub fn save_gzip(image: &str, out_path: &Path, progress_cb: impl Fn(u64)) -> Result<u64, String> {
    // 父目录不存在时自动创建
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("无法创建输出目录 {}: {}", parent.display(), e))?;
        }
    }
    // 先建输出文件,再起子进程,保证任何失败路径上管道都能正常关闭
    let file = std::fs::File::create(out_path)
        .map_err(|e| format!("无法创建输出文件 {}: {}", out_path.display(), e))?;

    let mut child = Command::new("docker")
        .args(["save", image])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            let _ = std::fs::remove_file(out_path); // 清理空文件
            format!("无法启动 docker save {}: {}", image, e)
        })?;

    // 后台线程持续排空 stderr,避免管道写满导致子进程阻塞
    let stderr_pipe = child.stderr.take();
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut s) = stderr_pipe {
            let _ = Read::read_to_string(&mut s, &mut buf);
        }
        buf
    });

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取 docker save 的标准输出".to_string())?;

    // docker save stdout → 8KB+ 缓冲循环读写 → GzEncoder → 计数层 → BufWriter → 文件
    let counter = Arc::new(AtomicU64::new(0));
    let counting = CountingWriter {
        inner: std::io::BufWriter::new(file),
        counter: Arc::clone(&counter),
    };
    let mut encoder = GzEncoder::new(counting, Compression::default());
    let mut reader = BufReader::with_capacity(64 * 1024, stdout_pipe);
    let mut buf = vec![0u8; 64 * 1024];
    let mut copy_err: Option<String> = None;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                if let Err(e) = encoder.write_all(&buf[..n]) {
                    copy_err = Some(format!("写入压缩数据失败: {}", e));
                    break;
                }
                progress_cb(counter.load(Ordering::Relaxed));
            }
            Err(e) => {
                copy_err = Some(format!("读取 docker save 输出失败: {}", e));
                break;
            }
        }
    }
    if let Some(e) = copy_err {
        // 出错:终止子进程、丢弃半成品文件
        let _ = child.kill();
        let _ = child.wait();
        drop(encoder);
        let _ = std::fs::remove_file(out_path);
        return Err(e);
    }

    // 写 gzip 尾部并刷盘
    let mut counting = match encoder.finish() {
        Ok(w) => w,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(out_path);
            return Err(format!("完成 gzip 压缩失败: {}", e));
        }
    };
    if let Err(e) = counting.inner.flush() {
        let _ = std::fs::remove_file(out_path);
        return Err(format!("写盘失败: {}", e));
    }
    // gzip 头/尾在 finish 时落盘,补发一次最终进度,保证最后一次回调值 == 返回的总字节数
    progress_cb(counter.load(Ordering::Relaxed));

    let status = child
        .wait()
        .map_err(|e| format!("等待 docker save 退出失败: {}", e))?;
    let stderr_msg = stderr_thread.join().unwrap_or_default();
    if !status.success() {
        let _ = std::fs::remove_file(out_path); // 半成品文件不可用
        let detail = stderr_msg.trim();
        return Err(if detail.is_empty() {
            format!("docker save {} 失败(退出码 {:?})", image, status.code())
        } else {
            format!("docker save {} 失败: {}", image, detail)
        });
    }
    Ok(counter.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    // brief 中的原样测试
    #[test]
    fn test_parse_images_json_line() {
        let line = r#"{"Containers":"N/A","CreatedAt":"2026-08-01 10:00:00 +0800 CST","ID":"abc123","Labels":null,"Repository":"myapp","Tag":"latest","Size":"300MB"}"#;
        let info = parse_image_line(line).unwrap();
        assert_eq!(info.repository, "myapp");
        assert_eq!(info.tag, "latest");
    }

    // 补充:验证 Size/CreatedAt/ID 字段完整映射
    #[test]
    fn test_parse_image_line_maps_all_fields() {
        let line = r#"{"Containers":"N/A","CreatedAt":"2026-08-01 10:00:00 +0800 CST","ID":"sha256:abc123def","Labels":null,"Repository":"myapp","Tag":"v1","Size":"1.5GB"}"#;
        let info = parse_image_line(line).unwrap();
        assert_eq!(info.size_bytes, 1_610_612_736);
        assert_eq!(info.created, "2026-08-01 10:00:00 +0800 CST");
        assert_eq!(info.id, "sha256:abc123def");
    }

    #[test]
    fn test_parse_image_line_bad_json() {
        assert!(parse_image_line("not json").is_err());
    }

    // 注:brief 原测试为与第二次 chrono::Local::now() 精确比较时间字符串,
    // 两次取时间跨秒时必然 flaky;故只断言前缀 `myapp:` 与时间戳格式
    // (总长度 = repository.len() + 1 + 15,15 = YYYYmmdd-HHMMSS)。
    #[test]
    fn test_deploy_tag_format() {
        let t = make_deploy_tag("myapp", "latest");
        assert!(t.starts_with("myapp:"), "应以 myapp: 开头,实际: {}", t);
        let ts = &t["myapp:".len()..];
        assert_eq!(ts.len(), 15, "时间戳应为 YYYYmmdd-HHMMSS 共 15 位,实际: {}", ts);
        assert!(ts[..8].bytes().all(|b| b.is_ascii_digit()), "前 8 位应为数字: {}", ts);
        assert_eq!(&ts[8..9], "-");
        assert!(ts[9..].bytes().all(|b| b.is_ascii_digit()), "后 6 位应为数字: {}", ts);
    }

    #[test]
    fn test_size_to_bytes() {
        assert_eq!(size_to_bytes("300MB"), 314_572_800);
        assert_eq!(size_to_bytes("1.5GB"), 1_610_612_736);
        assert_eq!(size_to_bytes("512B"), 512);
        assert_eq!(size_to_bytes("2KB"), 2_048);
        assert_eq!(size_to_bytes("1TB"), 1_099_511_627_776);
        // 解析失败记 0
        assert_eq!(size_to_bytes(""), 0);
        assert_eq!(size_to_bytes("N/A"), 0);
        assert_eq!(size_to_bytes("abc"), 0);
    }

    // ===== 以下测试依赖本机 Docker 守护进程,默认忽略 =====
    // 手工运行:cargo test docker:: -- --ignored

    #[test]
    #[ignore]
    fn test_check_host_real() {
        let report = check_host();
        println!("{:?}", report);
        assert!(report.docker_installed);
        assert!(report.daemon_running);
    }

    #[test]
    #[ignore]
    fn test_start_daemon_real() {
        start_daemon().expect("start_daemon 应成功(已在运行或成功拉起)");
    }

    #[test]
    #[ignore]
    fn test_list_images_real() {
        let images = list_images().expect("list_images 应成功");
        println!("镜像数量: {}", images.len());
        for img in images.iter().take(5) {
            println!("  {}:{} ({} bytes, id={})", img.repository, img.tag, img.size_bytes, img.id);
        }
    }

    #[test]
    #[ignore]
    fn test_tag_image_real() {
        let images = list_images().expect("list_images 应成功");
        let Some(first) = images.first() else {
            println!("本机无镜像,跳过");
            return;
        };
        let image = format!("{}:{}", first.repository, first.tag);
        let new_tag = format!("dd-task2-selftest:{}", std::process::id());
        tag_image(&image, &new_tag).expect("tag_image 应成功");
        let after = list_images().unwrap();
        assert!(
            after.iter().any(|i| i.repository == "dd-task2-selftest"),
            "打标签后应能在列表中看到新仓库"
        );
        // 删除该标签(rmi 一个标签只移除标签,不影响底层镜像层)
        let _ = run_docker(&["rmi", &new_tag]);
    }

    #[test]
    #[ignore]
    fn test_save_gzip_real() {
        let images = list_images().expect("list_images 应成功");
        let Some(first) = images.first() else {
            println!("本机无镜像,跳过");
            return;
        };
        let image = format!("{}:{}", first.repository, first.tag);
        let out = std::env::temp_dir().join(format!("dd-save-test-{}.tar.gz", std::process::id()));
        let last = std::cell::Cell::new(0u64);
        let total = save_gzip(&image, &out, |n| last.set(n)).expect("save_gzip 应成功");
        println!("镜像 {} 压缩后总字节: {},回调末值: {}", image, total, last.get());
        assert!(total > 0);
        assert_eq!(last.get(), total, "最后一次进度回调应等于总字节数");
        assert_eq!(std::fs::metadata(&out).unwrap().len(), total, "文件大小应与返回值一致");
        // gzip 魔数 1f 8b
        let head = std::fs::read(&out).unwrap();
        assert_eq!(&head[..2], &[0x1f, 0x8b]);
        std::fs::remove_file(&out).ok();
    }
}
