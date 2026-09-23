//! 层级增量传输(L1,第三十七批)
//!
//! 做什么:本地 `docker save` 出来的包里,**丢掉服务器已经持有的层 blob**,只传增量。
//! 部署侧在导出阶段按 `plan_drop` 裁一刀,装载失败时用整包兜底(见 commands/deploy.rs)。
//!
//! 口径全部来自 0 期 spike(第三十六批,两台真机 + 两种 image store),勿凭直觉改:
//! - **判据只能用 diffID**(store 无关):containerd 产包的 blob 名是**压缩摘要**,经典
//!   store 产包的 blob 名**就是 diffID**;而目标机的可用清单(`docker image inspect`
//!   的 `RootFS.Layers`)给的是 diffID → 两侧在同一坐标系里对账。
//! - **失败语义相反**:containerd 侧缺层时 `docker load` **rc=0**,只在 **stdout** 打
//!   `Error unpacking image …` 且**留下带标签的坏镜像**;经典侧 rc=1(stderr
//!   `no such file or directory`)且无半成品 → 判定必须 **rc + 文本双判**。
//! - 两种 store 都接受裁剪包(实测 6/6,内容 md5 逐字节一致),**不需要能力门控**。
//!
//! 内存纪律:层 blob 可能有 GB 级 —— 本模块**只按需读取**小 JSON blob(上限
//! `MAX_JSON_BLOB`),其余一律 `io::copy` 流式搬运,绝不整块读进内存。

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
/// 单个 JSON blob(manifest / config / index.json)的读取上限。
/// 实测量级:索引几百字节、config 数 KB、带 attestation 的清单 1 KB 级;
/// 4 MB 是防御性上限(异常包直接放弃裁剪,回退整包)。
const MAX_JSON_BLOB: u64 = 4 * 1024 * 1024;

/// 包内 blob 所在目录(`docker save` 的 OCI 布局)。
const BLOB_PREFIX: &str = "blobs/sha256/";

/// 包里的一层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveLayer {
    /// blob 文件名(64 位十六进制,不含 `blobs/sha256/` 前缀)。
    /// containerd 产包是压缩摘要,经典 store 产包是 diffID —— 对本模块而言只是「一个名字」。
    pub blob: String,
    /// 该 blob 在包里的字节数(压缩后,若来源为 containerd)。
    pub size: u64,
    /// 未压缩层摘要(与目标机 `RootFS.Layers` 同坐标系)。
    pub diff_id: String,
}

/// 解析出的包信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavePackage {
    pub config_digest: String,
    /// 底 → 顶。
    pub layers: Vec<SaveLayer>,
}

impl SavePackage {
    /// 层数。
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }
}

/// 归一化一个摘要:剥 `sha256:` 前缀并转小写(两侧口径统一)。
pub fn normalize_digest(raw: &str) -> String {
    let s = raw.trim();
    let s = s.strip_prefix("sha256:").unwrap_or(s);
    s.to_ascii_lowercase()
}

/// 解析 `docker image inspect --format '{{json .RootFS.Layers}}'` 的输出。
///
/// 形态:每个镜像一行 JSON 数组(如 `["sha256:ab…","sha256:cd…"]`);本函数逐行解析、
/// 汇总成一个 diffID 集合。坏行跳过(容错:某台服务器上有个别镜像 inspect 失败不该
/// 让整个判定失效 —— 少算一个层只会让裁剪更保守)。
pub fn parse_layer_inventory(out: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(values) = serde_json::from_str::<Vec<String>>(line) else {
            continue;
        };
        for v in values {
            let d = normalize_digest(&v);
            if !d.is_empty() {
                set.insert(d);
            }
        }
    }
    set
}

/// 读一个包内条目(带上限);条目不存在返回 `None`。
///
/// 注意:tar 的 `entries()` 要求归档位于起始处,故**每次查找都重开文件** ——
/// 只扫头部(数据由 tar crate 跳过),代价与「重开一次」同级,换来调用方无需管游标。
fn read_entry_capped(path: &Path, name: &str) -> Result<Option<Vec<u8>>, String> {
    let file = File::open(path).map_err(|e| format!("打开镜像包失败: {e}"))?;
    let mut archive = tar::Archive::new(file);
    let entries = archive
        .entries()
        .map_err(|e| format!("读取归档失败: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("读取归档条目失败: {e}"))?;
        let path_in_tar = entry
            .path()
            .map_err(|e| format!("解析归档条目路径失败: {e}"))?
            .to_string_lossy()
            .into_owned();
        if path_in_tar != name {
            continue;
        }
        if entry.header().size().unwrap_or(0) > MAX_JSON_BLOB {
            return Err(format!("包内 {name} 超出 {MAX_JSON_BLOB} 字节上限,放弃裁剪"));
        }
        let mut buf = Vec::new();
        entry
            .take(MAX_JSON_BLOB + 1)
            .read_to_end(&mut buf)
            .map_err(|e| format!("读取 {name} 失败: {e}"))?;
        if buf.len() as u64 > MAX_JSON_BLOB {
            return Err(format!("包内 {name} 超出 {MAX_JSON_BLOB} 字节上限,放弃裁剪"));
        }
        return Ok(Some(buf));
    }
    Ok(None)
}

fn json_of(path: &Path, name: &str) -> Result<serde_json::Value, String> {
    let raw = read_entry_capped(path, name)?
        .ok_or_else(|| format!("包内缺少 {name}(非 OCI 布局的 docker save 产物)"))?;
    serde_json::from_slice(&raw).map_err(|e| format!("解析 {name} 失败: {e}"))
}

/// 从 `index.json` 出发定位平台 manifest(下钻可能的 image index / attestation 层)。
fn platform_manifest(path: &Path) -> Result<serde_json::Value, String> {
    let index = json_of(path, "index.json")?;
    let first = index
        .get("manifests")
        .and_then(|m| m.as_array())
        .and_then(|m| m.first())
        .and_then(|m| m.get("digest"))
        .and_then(|d| d.as_str())
        .ok_or_else(|| "index.json 缺少 manifests[0].digest".to_string())?;
    let digest = normalize_digest(first);
    let blob = json_of(path, &format!("{BLOB_PREFIX}{digest}"))?;

    // 该 blob 若仍是 image index(带 attestation 的构建会多一层),
    // 取第一个「有 platform 字段」的条目 —— attestation 条目没有 platform。
    if blob.get("manifests").is_some() {
        let picked = blob
            .get("manifests")
            .and_then(|m| m.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m.get("platform").is_some())
                    .or_else(|| arr.first())
            })
            .and_then(|m| m.get("digest"))
            .and_then(|d| d.as_str())
            .ok_or_else(|| "image index 内找不到平台 manifest".to_string())?;
        let inner = normalize_digest(picked);
        return json_of(path, &format!("{BLOB_PREFIX}{inner}"));
    }
    Ok(blob)
}

/// 解析 `docker save` 产物,得到「blob 名 ↔ diffID」的层序表。
///
/// 只支持 OCI 布局(含 legacy `manifest.json` 兼容层的混合包,即现代 docker save 的
/// 标准输出);纯 legacy 老包(manifest.json 直接列 `<dir>/layer.tar`)会返回 Err,
/// 调用方应当**放弃裁剪并回退整包** —— 不猜格式。
pub fn parse_save_package(path: &Path) -> Result<SavePackage, String> {
    let manifest = platform_manifest(path)?;
    let config_digest = manifest
        .get("config")
        .and_then(|c| c.get("digest"))
        .and_then(|d| d.as_str())
        .map(normalize_digest)
        .ok_or_else(|| "平台 manifest 缺少 config.digest".to_string())?;

    let layer_digests: Vec<String> = manifest
        .get("layers")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("digest").and_then(|d| d.as_str()))
                .map(normalize_digest)
                .collect()
        })
        .unwrap_or_default();

    // config 的 rootfs.diff_ids 与 manifest.layers 同序(OCI 规范),按下标配对。
    let config = json_of(path, &format!("{BLOB_PREFIX}{config_digest}"))?;
    let diff_ids: Vec<String> = config
        .get("rootfs")
        .and_then(|r| r.get("diff_ids"))
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d.as_str())
                .map(normalize_digest)
                .collect()
        })
        .unwrap_or_default();

    if layer_digests.len() != diff_ids.len() {
        return Err(format!(
            "包内层数不一致(manifest {} 层 vs config {} 层),放弃裁剪",
            layer_digests.len(),
            diff_ids.len()
        ));
    }

    // blob 字节数:走一遍归档的头部(不读数据)。
    let mut sizes: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    {
        let file = File::open(path).map_err(|e| format!("打开镜像包失败: {e}"))?;
        let mut archive = tar::Archive::new(file);
        let entries = archive
            .entries()
            .map_err(|e| format!("读取归档失败: {e}"))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("读取归档条目失败: {e}"))?;
            let name = entry
                .path()
                .map_err(|e| format!("解析归档条目路径失败: {e}"))?
                .to_string_lossy()
                .into_owned();
            if let Some(blob) = name.strip_prefix(BLOB_PREFIX) {
                sizes.insert(blob.to_string(), entry.header().size().unwrap_or(0));
            }
        }
    }

    let layers = layer_digests
        .into_iter()
        .zip(diff_ids)
        .map(|(blob, diff_id)| SaveLayer {
            size: sizes.get(&blob).copied().unwrap_or(0),
            blob,
            diff_id,
        })
        .collect();

    Ok(SavePackage {
        config_digest,
        layers,
    })
}

/// 计算「哪些层可以不传」以及能省下的字节数。
///
/// 判据:层的 diffID 命中目标机已有清单 → 该 blob 可丢。命中集为空表示没有增益,
/// 调用方应当走原路径(与今天字节一致)。
pub fn plan_drop(pkg: &SavePackage, remote_diff_ids: &HashSet<String>) -> (Vec<String>, u64) {
    let mut drop = Vec::new();
    let mut saved = 0u64;
    for layer in &pkg.layers {
        if remote_diff_ids.contains(&layer.diff_id) {
            drop.push(layer.blob.clone());
            saved += layer.size;
        }
    }
    (drop, saved)
}

/// 从包中提取**被裁掉**的层的 diffID(第三十八批:归档契约)。
///
/// 用途:整栈把裁剪包上传进 release 目录后,若服务器无法用 `docker save` 重建
/// 自包含整包,manifest 必须记下「本次归档依赖服务器已有的哪些层」——
/// 回滚预检据此核对服务器是否仍持有这些层,缺层时落「增量包待补层」态。
pub fn dropped_diff_ids(pkg: &SavePackage, drop: &HashSet<String>) -> Vec<String> {
    pkg.layers
        .iter()
        .filter(|l| drop.contains(&l.blob))
        .map(|l| l.diff_id.clone())
        .collect()
}

/// 写裁剪包:除 `drop` 里的 blob 外,其余条目**原样流式搬运**(含目录条目与 JSON)。
///
/// 返回写入的字节数(压缩前)。
pub fn write_trimmed_tar<F: FnMut(u64)>(
    src: &Path,
    dst: &Path,
    drop: &HashSet<String>,
    mut on_bytes: F,
) -> Result<u64, String> {
    let file = File::open(src).map_err(|e| format!("打开镜像包失败: {e}"))?;
    let mut archive = tar::Archive::new(file);
    let out = File::create(dst).map_err(|e| format!("创建裁剪包失败: {e}"))?;
    let mut builder = tar::Builder::new(out);

    let mut written = 0u64;
    let entries = archive
        .entries()
        .map_err(|e| format!("读取归档失败: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("读取归档条目失败: {e}"))?;
        let name = entry
            .path()
            .map_err(|e| format!("解析归档条目路径失败: {e}"))?
            .to_string_lossy()
            .into_owned();
        let dropped = name
            .strip_prefix(BLOB_PREFIX)
            .map(|blob| drop.contains(blob))
            .unwrap_or(false);
        if dropped {
            continue;
        }
        let size = entry.header().size().unwrap_or(0);
        let header = entry.header().clone();
        // 流式拷贝:层 blob 可能是 GB 级,绝不整块进内存
        let before = written;
        builder
            .append(&header, CountingReader::new(&mut entry, &mut written))
            .map_err(|e| format!("写入裁剪包失败: {e}"))?;
        debug_assert_eq!(written - before, size, "搬运字节数应与头声明一致");
        on_bytes(written);
    }
    builder
        .finish()
        .map_err(|e| format!("收尾裁剪包失败: {e}"))?;
    Ok(written)
}

/// 把「已读字节计数」挂在读侧:tar 的 `Builder` 只负责写,进度得从源条目统计。
struct CountingReader<'a, R: Read> {
    inner: &'a mut R,
    counter: &'a mut u64,
}

impl<'a, R: Read> CountingReader<'a, R> {
    fn new(inner: &'a mut R, counter: &'a mut u64) -> Self {
        Self { inner, counter }
    }
}

impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        *self.counter += n as u64;
        Ok(n)
    }
}

/// 「静默装载失败」判定:rc=0 但输出里带失败标志(containerd store 的缺层形态)。
///
/// **为什么不能只看退出码**:0 期实测 —— containerd 侧缺层时 `docker load` 仍然
/// rc=0,只在 stdout 打 `Error unpacking image …`,并且**留下一个带标签的坏镜像**;
/// 只看 rc 会把失败报成成功(并把坏镜像留在服务器上)。经典侧则是 rc=1 + stderr,
/// 由调用方按既有链路处理。返回 Some(原因) 即视为装载失败。
pub fn detect_silent_load_failure(stdout: &str, stderr: &str) -> Option<String> {
    // containerd snapshotter:解包阶段失败但已注册镜像元数据
    if let Some(line) = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|l| l.starts_with("Error unpacking image"))
    {
        return Some(line.to_string());
    }
    // 经典 store:层文件缺失(rc 通常非 0,这里兜住 rc 异常为 0 的场景)
    let joined = format!("{stdout}\n{stderr}");
    if joined.contains("/blobs/sha256/")
        && (joined.contains("no such file or directory") || joined.contains("not found"))
    {
        if let Some(line) = joined
            .lines()
            .map(str::trim)
            .find(|l| l.contains("/blobs/sha256/"))
        {
            return Some(line.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== 第三十七批:L1 层级增量传输(判据与解析)=====

    // golden:0 期 spike 在 kkhuawei(Docker 29.8.1 / containerd store)缺层时的**原文**
    const GOLDEN_UNPACK_FAIL: &str = "Error unpacking image dd-l1big:v2: apply layer error for \"docker.io/library/dd-l1big:v2\": failed to extract layer sha256:8545326772665328f52ed58dbd3992dd483dc91b4f508e2c7ae7852152ba9c80: NotFound: failed to get reader from content store: content digest sha256:461805191c290ee559515564397bdf94ae1024947bcfb776d106c664f63fac00: not found";
    // golden:0 期 spike 在 tencent2(Docker 28.0.1 / 经典 store)缺层时的**原文**(stderr)
    const GOLDEN_CLASSIC_FAIL: &str = "open /var/lib/docker/tmp/docker-import-852735047/blobs/sha256/461805191c290ee559515564397bdf94ae1024947bcfb776d106c664f63fac00: no such file or directory";

    #[test]
    fn test_detect_silent_load_failure_catches_containerd_rc0_case() {
        // 这条是「退出码骗人」的核心回归:rc=0 也必须判失败
        assert!(
            detect_silent_load_failure(GOLDEN_UNPACK_FAIL, "").is_some(),
            "containerd 侧缺层的 stdout 标志串必须被抓住"
        );
    }

    #[test]
    fn test_detect_silent_load_failure_catches_classic_case() {
        assert!(
            detect_silent_load_failure("", GOLDEN_CLASSIC_FAIL).is_some(),
            "经典侧的层文件缺失必须被抓住(即便 rc 异常为 0)"
        );
    }

    #[test]
    fn test_detect_silent_load_failure_passes_normal_output() {
        let ok_stdout = "Loaded image: dd-l1:v2\n";
        assert!(detect_silent_load_failure(ok_stdout, "").is_none());
        // 正常日志里出现 "not found" 但无 blob 路径 → 不误判
        assert!(detect_silent_load_failure("Loaded image: x\n警告:标签 not found", "").is_none());
    }

    #[test]
    fn test_normalize_digest_and_inventory() {
        assert_eq!(normalize_digest("sha256:AB"), "ab");
        assert_eq!(normalize_digest("  ab  "), "ab");
        // 形态来自 `docker image inspect --format '{{json .RootFS.Layers}}'`:每镜像一行 JSON 数组
        let out = "[\"sha256:1e8fb136ca91\",\"sha256:33c716213e8e\"]\n[\"sha256:1e8fb136ca91\"]\n坏行跳过\n";
        let set = parse_layer_inventory(out);
        assert_eq!(set.len(), 2, "去重后应剩两个 diffID");
        assert!(set.contains("1e8fb136ca91"));
        assert!(set.contains("33c716213e8e"));
        assert!(parse_layer_inventory("").is_empty());
    }

    #[test]
    fn test_plan_drop_hits_only_remote_layers() {
        let pkg = SavePackage {
            config_digest: "c0ffee".into(),
            layers: vec![
                SaveLayer {
                    blob: "bc914f20".into(),
                    size: 3_146_869,
                    diff_id: "1e8fb136".into(),
                },
                SaveLayer {
                    blob: "5a796258".into(),
                    size: 126,
                    diff_id: "5d33d31d".into(),
                },
            ],
        };
        let mut remote = HashSet::new();
        remote.insert("1e8fb136".to_string());
        let (drop, saved) = plan_drop(&pkg, &remote);
        assert_eq!(drop, vec!["bc914f20".to_string()], "只丢命中层");
        assert_eq!(saved, 3_146_869, "省下的字节 = 被丢 blob 之和");

        let (drop_none, saved_none) = plan_drop(&pkg, &HashSet::new());
        assert!(drop_none.is_empty(), "无命中 → 不丢任何层(调用方走原路径)");
        assert_eq!(saved_none, 0);
    }

    #[test]
    fn test_dropped_diff_ids_reports_only_dropped() {
        let pkg = SavePackage {
            config_digest: "c0ffee".into(),
            layers: vec![
                SaveLayer { blob: "bc914f20".into(), size: 10, diff_id: "1e8fb136".into() },
                SaveLayer { blob: "5a796258".into(), size: 20, diff_id: "5d33d31d".into() },
            ],
        };
        let mut drop = HashSet::new();
        drop.insert("bc914f20".to_string());
        assert_eq!(
            dropped_diff_ids(&pkg, &drop),
            vec!["1e8fb136".to_string()],
            "只报被裁层的 diffID(归档依赖清单)"
        );
        assert!(dropped_diff_ids(&pkg, &HashSet::new()).is_empty(), "无裁剪 → 无依赖层");
    }

    /// 造一个最小 OCI 混合包(结构照 0 期实测的 docker save 产物:index.json → image index
    /// → 平台 manifest → config + 层 blob),用于解析与裁剪的单测。
    fn write_fixture(path: &Path, layer_blobs: &[(&str, &str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut b = tar::Builder::new(file);

        let layers_json: Vec<String> = layer_blobs
            .iter()
            .map(|(blob, _, data)| {
                format!(
                    "{{\"mediaType\":\"application/vnd.oci.image.layer.v1.tar+gzip\",\"digest\":\"sha256:{blob}\",\"size\":{}}}",
                    data.len()
                )
            })
            .collect();
        let diff_ids_json: Vec<String> = layer_blobs
            .iter()
            .map(|(_, diff, _)| format!("\"sha256:{diff}\""))
            .collect();

        let platform = format!(
            "{{\"schemaVersion\":2,\"mediaType\":\"application/vnd.oci.image.manifest.v1+json\",\"config\":{{\"mediaType\":\"application/vnd.oci.image.config.v1+json\",\"digest\":\"sha256:cfg000\",\"size\":9}},\"layers\":[{}]}}",
            layers_json.join(",")
        );
        let config = format!(
            "{{\"architecture\":\"amd64\",\"os\":\"linux\",\"rootfs\":{{\"type\":\"layers\",\"diff_ids\":[{}]}}}}",
            diff_ids_json.join(",")
        );
        // index.json → 平台 manifest(无 attestation 层)
        let index = "{\"schemaVersion\":2,\"mediaType\":\"application/vnd.oci.image.index.v1+json\",\"manifests\":[{\"mediaType\":\"application/vnd.oci.image.manifest.v1+json\",\"digest\":\"sha256:man000\",\"size\":10}]}";

        let mut add = |name: &str, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, data).unwrap();
        };
        add("index.json", index.as_bytes());
        add("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#);
        add("blobs/sha256/man000", platform.as_bytes());
        add("blobs/sha256/cfg000", config.as_bytes());
        for (blob, _, data) in layer_blobs {
            add(&format!("blobs/sha256/{blob}"), data);
        }
        b.finish().unwrap();
    }

    #[test]
    fn test_parse_save_package_pairs_blob_with_diff_id() {
        let dir = std::env::temp_dir().join(format!("dd-inc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pkg = dir.join("fixture.tar");
        write_fixture(
            &pkg,
            &[("bc914f20", "1e8fb136", b"base-layer"), ("5a796258", "5d33d31d", b"app")],
        );

        let parsed = parse_save_package(&pkg).expect("应能解析 OCI 混合包");
        assert_eq!(parsed.config_digest, "cfg000");
        assert_eq!(parsed.len(), 2, "两层");
        assert_eq!(parsed.layers[0].blob, "bc914f20");
        assert_eq!(parsed.layers[0].diff_id, "1e8fb136", "blob 与 diffID 按下标配对");
        assert_eq!(parsed.layers[0].size, 10, "blob 大小取自归档头(base-layer 10 字节)");
        assert_eq!(parsed.layers[1].diff_id, "5d33d31d");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_write_trimmed_tar_drops_only_named_blob() {
        let dir = std::env::temp_dir().join(format!("dd-inc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.tar");
        let dst = dir.join("out.tar");
        write_fixture(
            &src,
            &[("bc914f20", "1e8fb136", b"base-layer"), ("5a796258", "5d33d31d", b"app")],
        );

        let mut drop = HashSet::new();
        drop.insert("bc914f20".to_string());
        let mut ticks = 0u32;
        let written = write_trimmed_tar(&src, &dst, &drop, |_| ticks += 1).unwrap();
        assert!(ticks > 0, "应上报写入进度");
        assert!(written > 0);

        // 重新解析:裁剪包仍可解析(JSON 都在),但层 blob 少了一个
        let after = parse_save_package(&dst).expect("裁剪包应仍可解析");
        assert_eq!(after.len(), 2, "层序表来自 manifest,不因裁剪而变");
        let names: Vec<String> = {
            let f = File::open(&dst).unwrap();
            let mut a = tar::Archive::new(f);
            a.entries()
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert!(!names.iter().any(|n| n.ends_with("bc914f20")), "被丢的 blob 不在包里");
        assert!(names.iter().any(|n| n.ends_with("5a796258")), "保留层仍在包内");
        assert!(names.iter().any(|n| n == "index.json"), "元数据条目原样保留");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_parse_save_package_rejects_non_oci_package() {
        // 纯 legacy 老包(无 index.json)→ 必须报错,由调用方回退整包(不猜格式)
        let dir = std::env::temp_dir().join(format!("dd-inc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pkg = dir.join("legacy.tar");
        {
            let file = File::create(&pkg).unwrap();
            let mut b = tar::Builder::new(file);
            let mut h = tar::Header::new_gnu();
            let data = b"[]";
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, "manifest.json", &data[..]).unwrap();
            b.finish().unwrap();
        }
        assert!(
            parse_save_package(&pkg).is_err(),
            "缺少 index.json 的包应被拒(回退整包)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_write_trimmed_tar_keeps_bytes_identical_for_kept_entries() {
        // 保护「原样搬运」:保留条目的字节必须与分析源逐字节一致(否则 load 会莫名其妙失败)
        let dir = std::env::temp_dir().join(format!("dd-inc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.tar");
        let dst = dir.join("out.tar");
        let payload: Vec<u8> = (0u16..=511).map(|i| (i % 251) as u8).collect();
        write_fixture(&src, &[("aa11", "dd11", &payload)]);

        write_trimmed_tar(&src, &dst, &HashSet::new(), |_| {}).unwrap();

        let read = |p: &Path, name: &str| -> Vec<u8> {
            let f = File::open(p).unwrap();
            let mut a = tar::Archive::new(f);
            for e in a.entries().unwrap() {
                let mut e = e.unwrap();
                if e.path().unwrap().to_string_lossy().ends_with(name) {
                    let mut v = Vec::new();
                    e.read_to_end(&mut v).unwrap();
                    return v;
                }
            }
            panic!("条目不存在: {name}");
        };
        assert_eq!(read(&src, "aa11"), read(&dst, "aa11"), "保留层字节一致");
        assert_eq!(read(&src, "index.json"), read(&dst, "index.json"), "元数据字节一致");

        std::fs::remove_dir_all(&dir).ok();
    }
}
