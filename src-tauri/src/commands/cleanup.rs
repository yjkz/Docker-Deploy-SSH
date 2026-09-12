// // 清理分析(扫描 + 定向删除)

use super::*;

/// 清理分析单条目:未使用镜像(无标签/悬空)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupImage {
    pub id: String,
    pub repository: String,
    pub tag: String,
    pub size: String,
    /// 是否被任一容器(含停止/创建态)引用:true 时前端禁选(删不掉,rmi 会失败)
    pub in_use: bool,
}

/// 清理分析单条目:停止容器。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupContainer {
    pub id: String,
    pub names: String,
    pub image: String,
    pub status: String,
}

/// 清理分析单条目:未使用卷。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupVolume {
    pub name: String,
}

/// 分项目视图单条目:日期标签镜像(`repo:YYYYmmdd-HHMMSS`)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupTagImage {
    pub reference: String,
    pub id: String,
    pub size: String,
    pub created: String,
    /// 被任一容器引用时为 true,前端禁选并跳过删除
    pub in_use: bool,
}

/// 分项目视图:服务器上扫描到的项目目录及其可清理项。
///
/// 列表以**服务器真实目录**为准(扫描起点可配),应用内项目仅作标注;
/// 归档与标签的归属靠该目录下 compose 文件里的 `image:` 仓库名匹配。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupProject {
    /// 项目目录(绝对路径)
    pub dir: String,
    /// 该项目使用的 compose 文件(未扫到时为空串)
    pub compose_file: String,
    /// `du -sh` 输出(如 "1.2G";取不到为 "?")
    pub size: String,
    /// 发布归档目录(完整远端路径,新→旧)
    pub releases: Vec<String>,
    /// 该项目的日期标签镜像(新→旧)
    pub tag_images: Vec<CleanupTagImage>,
    /// 匹配到的应用内项目名(仅标注;空串=服务器上存在但软件内未配置)
    pub app_project: String,
    /// 该项目的发布归档保留数量(第五批):来自匹配到的应用内项目配置,
    /// 未匹配到或未配置时为 [`crate::config::DEFAULT_RELEASE_KEEP`]。
    /// 前端据此计算「可清理的旧归档」与部署收尾同一口径。
    pub release_keep: u32,
}

/// 单条扫描命令的诊断信息(命令原文、退出码、输出摘要)。
///
/// 用于「清理识别不到」类问题的自证:即使某节为空,也能看到命令实际
/// 退出码与服务器返回,而不必猜。
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CleanupDiag {
    pub label: String,
    pub cmd: String,
    /// 传输层失败时为 None
    pub exit_code: Option<i32>,
    /// 输出摘要(最多 [`CLEANUP_DIAG_OUTPUT_LINES`] 行)
    pub output: String,
}

/// 清理分析报告(各节互不影响,单项查询失败记入 errors/warnings 不阻断其余)。
#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CleanupReport {
    pub dangling_images: Vec<CleanupImage>,
    pub stopped_containers: Vec<CleanupContainer>,
    pub unused_volumes: Vec<CleanupVolume>,
    pub build_cache_size: String,
    /// 分项目可清理项(第三批新增)
    pub projects: Vec<CleanupProject>,
    /// 本次实际使用的扫描起点
    pub scan_root: String,
    /// 非致命提示(命令 stderr 混入、解析跳过的行等)
    pub warnings: Vec<String>,
    /// 逐条命令诊断
    pub diagnostics: Vec<CleanupDiag>,
    /// 致命错误(该节整体不可用)
    pub errors: Vec<String>,
}

/// 清理执行结果(逐节)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupSectionResult {
    pub label: String,
    pub ok: bool,
    pub output: String,
}

/// 分项目清理目标(前端勾选后原样回传:只删用户看到并勾选的条目)。
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CleanupProjectTarget {
    /// 项目目录(仅用于结果标签)
    pub dir: String,
    /// 待删归档目录(完整远端路径)
    #[serde(default)]
    pub release_dirs: Vec<String>,
    /// 待删镜像引用(`repo:tag`)
    #[serde(default)]
    pub image_refs: Vec<String>,
}

/// 清理执行勾选项(分节布尔 + 各节显式目标列表)。
///
/// 为什么带目标列表:清理只应删除用户在预览里看到并勾选的条目。
/// 逐条显式传入既避免 `prune -f` 的"清理范围外扩"(例如 volume prune
/// 会连未列出的未使用卷一起删),也让执行结果与预览一一对应。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupSections {
    pub images: bool,
    pub containers: bool,
    pub volumes: bool,
    pub builder: bool,
    /// 待删无标签镜像 ID 列表
    #[serde(default)]
    pub image_ids: Vec<String>,
    /// 待删停止容器 ID 列表
    #[serde(default)]
    pub container_ids: Vec<String>,
    /// 待删未使用卷名列表
    #[serde(default)]
    pub volume_names: Vec<String>,
    /// 分项目清理目标
    #[serde(default)]
    pub projects: Vec<CleanupProjectTarget>,
}

impl CleanupSections {
    /// 是否至少勾选了一项可执行内容(供「至少勾选一项」校验)。
    pub(crate) fn has_any(&self) -> bool {
        (self.images && !self.image_ids.is_empty())
            || (self.containers && !self.container_ids.is_empty())
            || (self.volumes && !self.volume_names.is_empty())
            || self.builder
            || !self.projects.is_empty()
    }
}

/// 宽松解析 NDJSON 行:跳过非 JSON 行(收进 warnings),不再让整节失败。
///
/// 服务器上 `docker` 可能把 `WARNING: No swap limit support` 之类的提示
/// 写进 stderr,而 [`exec_collect`] 把 stdout+stderr 合并返回 —— 旧实现
/// 只要有一行不是 JSON 就整节解析失败、列表恒为空,表现为"扫描不到"。
pub(crate) fn parse_cleanup_ndjson(text: &str) -> (Vec<serde_json::Value>, Vec<String>) {
    let mut items = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // 只对 `{` 开头的行尝试 JSON;其余按提示行收集(限 3 条防刷屏)
        if t.starts_with('{') {
            match serde_json::from_str::<serde_json::Value>(t) {
                Ok(v) => items.push(v),
                Err(e) => {
                    if warnings.len() < 3 {
                        warnings.push(format!("第 {} 行解析失败: {}", i + 1, e));
                    }
                }
            }
        } else if warnings.len() < 3 {
            warnings.push(t.to_string());
        }
    }
    (items, warnings)
}

/// 输出摘要保留行数(诊断区展示)。
const CLEANUP_DIAG_OUTPUT_LINES: usize = 8;

/// 取文本前 n 行(诊断输出摘要;超长时追加省略标记)。
fn head_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines[..lines.len().min(n)].join("\n");
    if lines.len() > n {
        out.push_str(&format!("\n… (共 {} 行)", lines.len()));
    }
    out
}

pub(crate) fn jstr(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// 单条扫描命令的结果:解析后的条目 + 非致命提示 + 诊断信息。
struct CleanupQuery {
    /// JSON 模式下的解析条目;文本模式下为空
    items: Vec<serde_json::Value>,
    warnings: Vec<String>,
    diag: CleanupDiag,
    /// 完整输出(文本模式解析用;诊断里只有截断摘要,不进前端)
    full_output: String,
}

/// 执行一条清理扫描命令(`json_mode` 决定是否按 NDJSON 解析)。
///
/// 传输层错误与退出码非 0 都只记 warnings(不向上传播),使单节失败
/// 不影响其余节的扫描结果 —— 这正是"为什么某一节是 0"可排查的前提。
async fn run_cleanup_query(
    client: &mut SshClient,
    label: &str,
    cmd: &str,
    json_mode: bool,
) -> CleanupQuery {
    let mut diag = CleanupDiag {
        label: label.to_string(),
        cmd: cmd.to_string(),
        exit_code: None,
        output: String::new(),
    };
    match exec_collect(client, cmd).await {
        Ok((code, out)) => {
            diag.exit_code = Some(code);
            diag.output = head_lines(&out, CLEANUP_DIAG_OUTPUT_LINES);
            if code != 0 {
                return CleanupQuery {
                    items: Vec::new(),
                    warnings: vec![format!(
                        "{}查询失败(退出码 {}): {}",
                        label,
                        code,
                        head_lines(&out, 1)
                    )],
                    diag,
                    full_output: out,
                };
            }
            let (items, warnings) = if json_mode {
                parse_cleanup_ndjson(&out)
            } else {
                (Vec::new(), Vec::new())
            };
            CleanupQuery {
                items,
                warnings,
                diag,
                full_output: out,
            }
        }
        Err(e) => {
            diag.output = format!("(传输层错误) {}", e);
            CleanupQuery {
                items: Vec::new(),
                warnings: vec![format!("{}查询失败: {}", label, e)],
                diag,
                full_output: String::new(),
            }
        }
    }
}

/// 从命令输出的纯文本行里取绝对路径(过滤空行与摘要省略标记;纯函数,便于单测)。
pub(crate) fn abs_path_lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('/') && !l.starts_with("…"))
        .map(String::from)
        .collect()
}

/// 把镜像引用裁成仓库名(去 `:tag` / `@digest`;纯函数,便于单测)。
/// 例:`myapp:latest` → `myapp`;`registry:5000/app` → `registry:5000/app`。
pub(crate) fn image_repo_of(reference: &str) -> String {
    let r = reference.trim();
    if r.is_empty() {
        return String::new();
    }
    // 有 @digest 时以 digest 之前为准
    let base = r.split('@').next().unwrap_or(r);
    // 最后一个 ':' 若在最后一个 '/' 之后才是 tag 分隔符(避免误切 registry:port)
    match (base.rfind(':'), base.rfind('/')) {
        (Some(c), Some(s)) if c < s => base.to_string(),
        (Some(c), None) => base[..c].to_string(),
        (Some(c), Some(_)) => base[..c].to_string(),
        (None, _) => base.to_string(),
    }
}

/// 判断字符串是否为日期标签 `YYYYmmdd-HHMMSS`(纯函数,便于单测)。
pub(crate) fn is_date_tag(tag: &str) -> bool {
    let t = tag.trim();
    if t.len() != 15 {
        return false;
    }
    let b = t.as_bytes();
    for (i, ch) in b.iter().enumerate() {
        if i == 8 {
            if *ch != b'-' {
                return false;
            }
        } else if !ch.is_ascii_digit() {
            return false;
        }
    }
    true
}

/// 从 compose 文本提取全部 `services.*.image` 的仓库名(纯函数,便于单测)。
/// 解析失败或无 image 字段 → 空 Vec(调用方据此跳过该项目的标签清理,不误删)。
pub(crate) fn compose_image_repos(yaml_text: &str) -> Vec<String> {
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(yaml_text) else {
        return Vec::new();
    };
    let Some(services) = doc.get("services").and_then(|s| s.as_mapping()) else {
        return Vec::new();
    };
    let mut repos: Vec<String> = Vec::new();
    for (_name, svc) in services {
        if let Some(image) = svc.get("image").and_then(|i| i.as_str()) {
            let repo = image_repo_of(image);
            if !repo.is_empty() && !repos.contains(&repo) {
                repos.push(repo);
            }
        }
    }
    repos
}

// ===== 分项目扫描拼命令(纯函数,便于单测)=====

/// 扫描含 compose 文件的目录:深度 ≤ [`CLEANUP_SCAN_MAX_DEPTH`],排除
/// `releases/` 归档与 `.git`(归档里的 compose 副本不是项目)。
pub fn cleanup_scan_compose_cmd(root: &str) -> String {
    format!(
        "find {} -maxdepth {} -type f \\( -name 'docker-compose.yml' -o -name 'docker-compose.yaml' -o -name 'compose.yml' -o -name 'compose.yaml' \\) ! -path '*/releases/*' ! -path '*/.git/*' 2>/dev/null",
        shell_single_quote(root),
        CLEANUP_SCAN_MAX_DEPTH
    )
}

/// 扫描发布归档目录(`<项目>/releases/<YYYYmmdd-HHMMSS>`)的完整路径。
pub fn cleanup_scan_releases_cmd(root: &str) -> String {
    format!(
        "find {} -maxdepth {} -type d -path '*/releases/*' -name '20*-*' 2>/dev/null",
        shell_single_quote(root),
        CLEANUP_SCAN_MAX_DEPTH + 2
    )
}

/// 拼 `du -sh <dir>...`(一次调用取多个目录占用;取不到的目录由 du 自行跳过)。
pub fn cleanup_du_cmd(dirs: &[String]) -> String {
    let quoted: Vec<String> = dirs.iter().map(|d| shell_single_quote(d)).collect();
    format!("du -sh {} 2>/dev/null", quoted.join(" "))
}

/// 拼「逐个 cat compose(带路径标记行)」命令:一次往返取回多份 compose 内容,
/// 标记行形如 `==COMPOSE:<path>`,便于按项目切分。
pub fn cleanup_cat_composes_cmd(files: &[String]) -> String {
    let mut out = String::new();
    for f in files {
        // 标记行与内容都经 printf/cat 输出;路径单引号包裹防注入
        out.push_str(&format!(
            "printf '==COMPOSE:%s\\n' {}; cat {} 2>/dev/null; printf '\\n'; ",
            shell_single_quote(f),
            shell_single_quote(f)
        ));
    }
    out
}

/// 解析 `==COMPOSE:<path>` 标记切分的 compose 内容(纯函数,便于单测)。
/// 返回 `(path, content)` 列表。
pub(crate) fn split_compose_dump(out: &str) -> Vec<(String, String)> {
    let mut result: Vec<(String, String)> = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut buf = String::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("==COMPOSE:") {
            if let Some(p) = cur_path.take() {
                result.push((p, buf.clone()));
            }
            buf.clear();
            cur_path = Some(rest.trim().to_string());
        } else if cur_path.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(p) = cur_path {
        result.push((p, buf));
    }
    result
}

// ===== 回滚明细批量读取(第七批;纯函数,便于单测)=====
//
// 此前 rollback_project_detail / rollback_list_releases 对每个归档各发一条
// `ls -1` 与一条 `cat manifest.json`(N+1 串行往返,50 个归档 ≈ 100+ 次
// channel 打开,是明细加载卡顿的根因)。这里仿 [`cleanup_cat_composes_cmd`]
// 的「一条拼接命令 + 标记行切分」形态,把全部归档压进单次往返;
// 二次优化后归档枚举也由远端 `find` 循环完成 —— **整条命令长度恒定**,
// 不再需要"先 ls 再拼装"的两次往返。

/// [`parse_releases_dump`] 产出的单个归档批量读取结果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReleaseDumpEntry {
    /// 归档时间戳(目录名)
    pub ts: String,
    /// 归档内文件名清单(`ls -1`;目录不可读/被清理时为空)
    pub files: Vec<String>,
    /// manifest.json 文本(缺失/读取失败为空,调用方按"无清单"降级)
    pub manifest: String,
    /// release-notes.json 文本(缺失/读取失败为空)
    pub notes: String,
}

/// [`parse_releases_dump`] 的完整产出。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReleasesDump {
    /// 归档条目(远端返回顺序,find + sort -r 即新 → 旧;上限由命令内 head 截断)
    pub entries: Vec<ReleaseDumpEntry>,
    /// compose 候选文本(与入参候选按下标一一对应;缺失为空)
    pub compose_texts: Vec<String>,
    /// `docker images` 全量 NDJSON 文本(include_images 时;供本地按仓库过滤)
    pub images: String,
}

/// 拼「归档明细单次往返」命令:远端 `find` 枚举 `<dir>/releases` 下的归档
/// (新 → 旧,`head` 截断),`while read` 循环逐归档输出文件清单、manifest 与
/// release-notes;可选附加 compose 候选文本与全量 `docker images`。
///
/// 标记行格式(解析见 [`parse_releases_dump`]):`==RELEASE:<ts>` /
/// `==MANIFEST:<ts>` / `==NOTES:<ts>` / `==COMPOSE:<path>` / `==IMAGES`。
/// 全部路径经 [`shell_single_quote`] 包裹;归档枚举在远端完成,**命令长度
/// 与归档数量无关**。
pub fn releases_scan_cmd(
    dir: &str,
    limit: usize,
    compose_candidates: &[String],
    include_images: bool,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "find {} -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort -r | head -n {} \
         | while IFS= read -r d; do \
         printf '==RELEASE:%s\\n' \"${{d##*/}}\"; ls -1 \"$d\" 2>/dev/null; printf '\\n'; \
         printf '==MANIFEST:%s\\n' \"${{d##*/}}\"; cat \"$d/manifest.json\" 2>/dev/null; printf '\\n'; \
         printf '==NOTES:%s\\n' \"${{d##*/}}\"; cat \"$d/release-notes.json\" 2>/dev/null; printf '\\n'; \
         done; ",
        shell_single_quote(&remote_join(dir, "releases")),
        limit
    ));
    for cand in compose_candidates {
        out.push_str(&format!(
            "printf '==COMPOSE:%s\\n' {}; cat {} 2>/dev/null; printf '\\n'; ",
            shell_single_quote(cand),
            shell_single_quote(cand)
        ));
    }
    if include_images {
        // REMOTE_IMAGES_CMD_FULL 含 format 花括号,直接拼接不走 format! 转义
        out.push_str("printf '==IMAGES\\n'; ");
        out.push_str(REMOTE_IMAGES_CMD_FULL);
        out.push_str(" 2>/dev/null; printf '\\n'; ");
    }
    out
}

/// 解析 [`releases_scan_cmd`] 的输出(纯函数,便于单测)。
///
/// `compose_candidates` 为空时 `compose_texts` 为空;`==IMAGES` 段只有在命令
/// 带了 `include_images` 时才出现,缺失时 `images` 为空串。未知标记与散行
/// (不属于任何已知段)一律忽略,不会错位进别的段。
pub(crate) fn parse_releases_dump(out: &str, compose_candidates: &[String]) -> ReleasesDump {
    let mut dump = ReleasesDump {
        compose_texts: vec![String::new(); compose_candidates.len()],
        ..ReleasesDump::default()
    };
    const SEC_NONE: u8 = 0;
    const SEC_FILES: u8 = 1;
    const SEC_MANIFEST: u8 = 2;
    const SEC_NOTES: u8 = 3;
    const SEC_COMPOSE: u8 = 4;
    const SEC_IMAGES: u8 = 5;
    let mut kind = SEC_NONE;
    let mut idx = usize::MAX;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("==RELEASE:") {
            kind = SEC_FILES;
            dump.entries.push(ReleaseDumpEntry {
                ts: rest.trim().to_string(),
                ..ReleaseDumpEntry::default()
            });
            idx = dump.entries.len() - 1; // 后续文件清单行归属这个新条目
        } else if let Some(rest) = line.strip_prefix("==MANIFEST:") {
            kind = SEC_MANIFEST;
            idx = dump
                .entries
                .iter()
                .position(|e| e.ts == rest.trim())
                .unwrap_or(usize::MAX);
        } else if let Some(rest) = line.strip_prefix("==NOTES:") {
            kind = SEC_NOTES;
            idx = dump
                .entries
                .iter()
                .position(|e| e.ts == rest.trim())
                .unwrap_or(usize::MAX);
        } else if let Some(rest) = line.strip_prefix("==COMPOSE:") {
            kind = SEC_COMPOSE;
            idx = compose_candidates
                .iter()
                .position(|c| *c == rest.trim())
                .unwrap_or(usize::MAX);
        } else if line.trim() == "==IMAGES" {
            kind = SEC_IMAGES;
            idx = usize::MAX;
        } else {
            match kind {
                SEC_FILES => {
                    if !line.trim().is_empty() {
                        if let Some(e) = dump.entries.get_mut(idx) {
                            e.files.push(line.trim().to_string());
                        }
                    }
                }
                SEC_MANIFEST => {
                    if let Some(e) = dump.entries.get_mut(idx) {
                        e.manifest.push_str(line);
                        e.manifest.push('\n');
                    }
                }
                SEC_NOTES => {
                    if let Some(e) = dump.entries.get_mut(idx) {
                        e.notes.push_str(line);
                        e.notes.push('\n');
                    }
                }
                SEC_COMPOSE => {
                    if let Some(t) = dump.compose_texts.get_mut(idx) {
                        t.push_str(line);
                        t.push('\n');
                    }
                }
                SEC_IMAGES => {
                    dump.images.push_str(line);
                    dump.images.push('\n');
                }
                _ => {}
            }
        }
    }
    dump
}

// ===== 回滚中心项目扫描(第七批二次优化;纯函数,便于单测)=====
//
// 此前扫描是 5 次串行往返(compose find / docker ps / 逐容器 inspect /
// releases find / docker ps -a)。组合成一条命令后单次往返带回全部数据;
// compose working_dir 从 `docker ps -a` 的 Labels 字段直读,省掉 inspect 步。

/// 拼「回滚中心项目扫描」单往返命令:compose 文件 find + releases 归档 find +
/// `docker ps -a`。`==ROOTEXISTS:0/1` 标记根目录存在性(此前由 find 退出码判定,
/// 组合命令的退出码取自末段,不再可靠)。
pub fn rollback_scan_cmd(root: &str) -> String {
    format!(
        "[ -d {root} ] && echo '==ROOTEXISTS:1' || echo '==ROOTEXISTS:0'; \
         printf '==COMPOSEFILES\\n'; {compose}; \
         printf '==RELEASES\\n'; {releases}; \
         printf '==PSALL\\n'; docker ps -a --format '{{{{json .}}}}' 2>/dev/null",
        root = shell_single_quote(root),
        compose = cleanup_scan_compose_cmd(root),
        releases = cleanup_scan_releases_cmd(root),
    )
}

/// [`rollback_scan_cmd`] 的解析产出。
#[derive(Debug, Default)]
pub(crate) struct RollbackScanDump {
    /// 扫描起点目录存在(不存在时调用方返回与旧版一致的中文错误)
    pub(crate) root_exists: bool,
    /// compose 文件绝对路径清单
    pub(crate) compose_paths: Vec<String>,
    /// releases 归档目录绝对路径清单
    pub(crate) release_paths: Vec<String>,
    /// `docker ps -a` 的 NDJSON 条目(docker 不可用/失败时为空)
    pub(crate) ps_items: Vec<serde_json::Value>,
}

/// 解析 [`rollback_scan_cmd`] 的输出(纯函数,便于单测)。
/// 各段以标记行切分;PS 段为 NDJSON,逐行解析、坏行跳过(与清理分析同口径)。
pub(crate) fn parse_rollback_scan(out: &str) -> RollbackScanDump {
    let mut dump = RollbackScanDump::default();
    const S_NONE: u8 = 0;
    const S_COMPOSE: u8 = 1;
    const S_RELEASES: u8 = 2;
    const S_PS: u8 = 3;
    let mut kind = S_NONE;
    let mut ps_buf = String::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("==ROOTEXISTS:") {
            kind = S_NONE;
            dump.root_exists = rest.trim() == "1";
        } else if line.trim() == "==COMPOSEFILES" {
            kind = S_COMPOSE;
        } else if line.trim() == "==RELEASES" {
            kind = S_RELEASES;
        } else if line.trim() == "==PSALL" {
            kind = S_PS;
        } else {
            match kind {
                S_COMPOSE => {
                    let t = line.trim();
                    if t.starts_with('/') {
                        dump.compose_paths.push(t.to_string());
                    }
                }
                S_RELEASES => {
                    let t = line.trim();
                    if t.starts_with('/') {
                        dump.release_paths.push(t.to_string());
                    }
                }
                S_PS => {
                    ps_buf.push_str(line);
                    ps_buf.push('\n');
                }
                _ => {}
            }
        }
    }
    dump.ps_items = parse_cleanup_ndjson(&ps_buf).0;
    dump
}

/// 从 `docker ps -a` 的 NDJSON 条目提取 compose 项目工作目录
/// (`Labels."com.docker.compose.project.working_dir"`,去重;纯函数,便于单测)。
/// 替代此前「docker ps 取 ID → 逐容器 inspect」的往返;stopped 容器的 Labels
/// 同样存在,因此"compose 文件被删但容器仍在"的项目依旧可见。
pub(crate) fn compose_working_dirs(items: &[serde_json::Value]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in items {
        let wd = v
            .get("Labels")
            .and_then(|l| l.get("com.docker.compose.project.working_dir"))
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !wd.is_empty() && !out.iter().any(|d| d == &wd) {
            out.push(wd);
        }
    }
    out
}

/// 项目目录 = 发布归档路径去末尾两级(`<dir>/releases/<ts>` → `<dir>`)。
/// 纯函数,便于单测。
pub(crate) fn project_dir_of_release(release_path: &str) -> String {
    let p = release_path.trim_end_matches('/');
    let without_ts = match p.rfind('/') {
        Some(i) => &p[..i],
        None => return String::new(),
    };
    match without_ts.rfind('/') {
        Some(i) => without_ts[..i].to_string(),
        None => String::new(),
    }
}

/// 解析 `du -sh` 输出为 `路径 → 占用`(纯函数,便于单测)。
/// GNU du 输出形如 `1.2G\t/home/x/proj`(大小以制表符或空格分隔路径)。
pub(crate) fn parse_du_output(out: &str) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // 以最后一个制表符切分;无制表符时退化为按首个空白切分
        if let Some((size, path)) = t.rsplit_once('\t') {
            rows.push((path.trim().to_string(), size.trim().to_string()));
        } else if let Some((size, path)) = t.split_once(char::is_whitespace) {
            rows.push((path.trim().to_string(), size.trim().to_string()));
        }
    }
    rows
}

/// 拼「逐路径取字节数」命令(`du -s -B1`):供迁移预检估算卷与归档体积。
///
/// 与 [`cleanup_du_cmd`] 的 `du -sh` 区别:这里要**字节数**而非人类可读串,
/// 便于求和与前端统一格式化。`2>/dev/null` 让不存在/无权限的路径静默跳过。
pub fn du_bytes_cmd(paths: &[String]) -> String {
    let quoted: Vec<String> = paths.iter().map(|d| shell_single_quote(d)).collect();
    format!("du -s -B1 {} 2>/dev/null", quoted.join(" "))
}

/// 解析 [`du_bytes_cmd`] 的输出为「与入参路径一一对应」的字节数列表。
///
/// **按路径匹配回填**(不是按行序):某条路径读不到时它对应 `None`,不会让
/// 后续体积错位到别的项上(`du` 会跳过失败项,行序与入参不再等长)。
pub fn parse_du_bytes(out: &str, paths: &[String]) -> Vec<Option<u64>> {
    let mut result = vec![None; paths.len()];
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some((size, path)) = t.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(bytes) = size.trim().parse::<u64>() else {
            continue;
        };
        let path = path.trim().trim_end_matches('/');
        if let Some(idx) = paths
            .iter()
            .position(|p| p.trim_end_matches('/') == path)
        {
            result[idx] = Some(bytes);
        }
    }
    result
}

/// 清理分析:无标签镜像 / 停止容器 / 未使用卷 / build cache 占用 / 分项目可清理项。
/// 只读查询,不做任何清理;单项失败不影响其余(记入 warnings,诊断逐条可查)。
///
/// 无标签镜像为什么不用 `docker images -f dangling=true`:该过滤在新版
/// Docker(BuildKit / containerd image store)下常返回空,而 `docker images`
/// 明明列得出 `<none>:<none>` 条目 —— 表现为"有悬空镜像但识别不到"。这里改为
/// 全量拉取后在客户端过滤 `Repository == "<none>"`,并用容器引用集合标记在用项。
#[tauri::command]
pub async fn cleanup_preview(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    scan_root: Option<String>,
) -> Result<CleanupReport, String> {
    let _ = app; // 与 prune_server 等命令签名风格一致(结果经返回值而非事件)
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

    let mut report = CleanupReport::default();
    // 扫描起点:显式传入优先,否则用服务器配置的部署目录
    let scan_root = scan_root
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| server.remote_dir.clone());
    report.scan_root = scan_root.clone();

    // 0. 先取容器列表与它们引用的镜像 ID(用于标记在用的无标签/旧标签镜像)
    let containers_q = run_cleanup_query(
        &mut client,
        "容器列表",
        "docker ps -a --no-trunc --format '{{json .}}'",
        true,
    )
    .await;
    report.diagnostics.push(containers_q.diag.clone());
    report.warnings.extend(containers_q.warnings.clone());
    let container_rows = containers_q.items;

    // 容器条目只有镜像名,需 inspect 才拿到镜像 ID(sha256:...)
    let cids: Vec<String> = container_rows
        .iter()
        .map(|v| jstr(v, "ID"))
        .filter(|s| !s.is_empty())
        .collect();
    let mut in_use_image_ids: Vec<String> = Vec::new();
    if !cids.is_empty() {
        let quoted: Vec<String> = cids.iter().map(|c| shell_single_quote(c)).collect();
        let inspect_cmd = format!(
            "docker inspect --format '{{{{.Image}}}}' {} 2>/dev/null",
            quoted.join(" ")
        );
        let inspect_q = run_cleanup_query(&mut client, "容器镜像引用", &inspect_cmd, false).await;
        // 纯文本输出:逐行取 sha256:...(不要求 JSON)
        for line in inspect_q.full_output.lines() {
            let t = line.trim();
            if t.starts_with("sha256:") {
                in_use_image_ids.push(t.to_string());
            }
        }
        report.diagnostics.push(inspect_q.diag);
    }

    // 1. 无标签镜像(<none>:<none> 及 <none> 仓库)—— 全量拉取后客户端过滤
    let images_q = run_cleanup_query(
        &mut client,
        "镜像列表",
        "docker images --no-trunc --format '{{json .}}'",
        true,
    )
    .await;
    report.diagnostics.push(images_q.diag.clone());
    report.warnings.extend(images_q.warnings.clone());
    for v in &images_q.items {
        let repo = jstr(v, "Repository");
        let tag = jstr(v, "Tag");
        if repo != "<none>" && tag != "<none>" {
            continue;
        }
        let id = jstr(v, "ID");
        let in_use = in_use_image_ids.iter().any(|x| x == &id);
        report.dangling_images.push(CleanupImage {
            repository: repo,
            tag,
            size: jstr(v, "Size"),
            in_use,
            id,
        });
    }

    // 2. 停止容器(exited 与 created;已在第 0 步取回,直接复用)
    report.stopped_containers = container_rows
        .iter()
        .filter(|v| {
            let state = jstr(v, "State").to_lowercase();
            let status = jstr(v, "Status").to_lowercase();
            state == "exited"
                || state == "created"
                || status.starts_with("exited")
                || status.starts_with("created")
        })
        .map(|v| CleanupContainer {
            id: jstr(v, "ID"),
            names: jstr(v, "Names"),
            image: jstr(v, "Image"),
            status: jstr(v, "Status"),
        })
        .collect();

    // 3. 未使用卷
    let volumes_q = run_cleanup_query(
        &mut client,
        "卷列表",
        "docker volume ls -f dangling=true --format '{{json .}}'",
        true,
    )
    .await;
    report.diagnostics.push(volumes_q.diag.clone());
    report.warnings.extend(volumes_q.warnings.clone());
    report.unused_volumes = volumes_q
        .items
        .iter()
        .map(|v| CleanupVolume {
            name: jstr(v, "Name"),
        })
        .collect();

    // 4. build cache 占用(docker system df 的 Build Cache 行)
    let df_q = run_cleanup_query(
        &mut client,
        "磁盘占用",
        "docker system df --format '{{json .}}'",
        true,
    )
    .await;
    report.diagnostics.push(df_q.diag.clone());
    report.warnings.extend(df_q.warnings.clone());
    report.build_cache_size = df_q
        .items
        .iter()
        .find(|v| jstr(v, "Type") == "Build Cache")
        .map(|v| jstr(v, "Size"))
        .unwrap_or_else(|| "0B".to_string());

    // 5. 分项目扫描:compose 文件 → 目录 → du 占用 → releases 归档 → 日期标签镜像
    match scan_cleanup_projects(&mut client, &scan_root, &cfg, &in_use_image_ids).await {
        Ok((projects, mut diags, mut warnings)) => {
            report.projects = projects;
            report.diagnostics.append(&mut diags);
            report.warnings.append(&mut warnings);
        }
        Err(e) => report.errors.push(format!("分项目扫描失败: {}", e)),
    }

    Ok(report)
}

/// 扫描服务器上的项目目录,汇总每个项目的占用/归档/旧标签镜像(只读)。
///
/// 步骤:find compose(排除 releases)→ 目录去重 → du -sh 取占用 →
/// 逐目录 cat compose 提取镜像仓库名 → 匹配 releases 归档与日期标签镜像。
/// 注意:纯文本命令的解析必须用 `full_output`(诊断里只有截断摘要)。
async fn scan_cleanup_projects(
    client: &mut SshClient,
    scan_root: &str,
    cfg: &crate::config::AppConfig,
    in_use_image_ids: &[String],
) -> Result<(Vec<CleanupProject>, Vec<CleanupDiag>, Vec<String>), String> {
    let mut diags: Vec<CleanupDiag> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // 5.1 扫 compose 文件
    let compose_q = run_cleanup_query(
        client,
        "项目扫描",
        &cleanup_scan_compose_cmd(scan_root),
        false,
    )
    .await;
    let compose_paths = abs_path_lines(&compose_q.full_output);
    diags.push(compose_q.diag);
    if compose_paths.is_empty() {
        return Ok((Vec::new(), diags, warnings));
    }

    // 项目目录去重(compose 文件所在目录)
    let mut dirs: Vec<String> = Vec::new();
    for p in &compose_paths {
        if let Some((parent, _)) = p.rsplit_once('/') {
            if !parent.is_empty() && !dirs.iter().any(|d| d == parent) {
                dirs.push(parent.to_string());
            }
        }
    }
    dirs.sort();

    // 5.2 du -sh 取占用
    let du_q = run_cleanup_query(client, "项目占用", &cleanup_du_cmd(&dirs), false).await;
    let du_rows = parse_du_output(&du_q.full_output);
    diags.push(du_q.diag);

    // 5.3 逐目录 cat compose(带标记行),提取镜像仓库名
    let cat_q = run_cleanup_query(
        client,
        "compose 内容",
        &cleanup_cat_composes_cmd(&compose_paths),
        false,
    )
    .await;
    let dumped = split_compose_dump(&cat_q.full_output);
    diags.push(cat_q.diag);
    let mut dir_repos: Vec<(String, Vec<String>)> = Vec::new();
    for (path, content) in &dumped {
        let Some((dir, _)) = path.rsplit_once('/') else {
            continue;
        };
        let repos = compose_image_repos(content);
        if let Some(entry) = dir_repos.iter_mut().find(|(d, _)| d == dir) {
            for r in repos {
                if !entry.1.contains(&r) {
                    entry.1.push(r);
                }
            }
        } else {
            dir_repos.push((dir.to_string(), repos));
        }
    }

    // 5.4 扫 releases 归档(一次 find 取全量,再按项目目录归组)
    let rel_q = run_cleanup_query(
        client,
        "发布归档",
        &cleanup_scan_releases_cmd(scan_root),
        false,
    )
    .await;
    let release_paths = abs_path_lines(&rel_q.full_output);
    diags.push(rel_q.diag);

    // 5.5 全量镜像列表(取日期标签镜像;含 ID/大小/创建时间)
    let tag_q = run_cleanup_query(
        client,
        "镜像标签",
        "docker images --format '{{json .}}'",
        true,
    )
    .await;
    let all_images: Vec<serde_json::Value> = tag_q.items.clone();
    warnings.extend(tag_q.warnings.clone());
    diags.push(tag_q.diag);

    // 5.6 组项目视图
    let mut projects: Vec<CleanupProject> = Vec::new();
    for dir in &dirs {
        let compose_file = compose_paths
            .iter()
            .find(|p| p.rsplit_once('/').map(|(d, _)| d) == Some(dir.as_str()))
            .cloned()
            .unwrap_or_default();
        let repos = dir_repos
            .iter()
            .find(|(d, _)| d == dir)
            .map(|(_, r)| r.clone())
            .unwrap_or_default();
        let size = du_rows
            .iter()
            .find(|(p, _)| p == dir)
            .map(|(_, s)| s.clone())
            .unwrap_or_else(|| "?".to_string());

        // 该项目下的归档目录(新→旧,按目录名时间戳倒序)
        let mut releases: Vec<String> = release_paths
            .iter()
            .filter(|r| project_dir_of_release(r) == *dir)
            .cloned()
            .collect();
        releases.sort_by(|a, b| b.cmp(a));

        // 该项目可清理的日期标签镜像(仓库名匹配 + 未被容器引用)
        let tag_images: Vec<CleanupTagImage> = all_images
            .iter()
            .filter_map(|v| {
                let repo = jstr(v, "Repository");
                let tag = jstr(v, "Tag");
                if repo == "<none>" || !is_date_tag(&tag) {
                    return None;
                }
                if !repos.iter().any(|r| r == &repo) {
                    return None;
                }
                let id = jstr(v, "ID");
                Some(CleanupTagImage {
                    reference: format!("{}:{}", repo, tag),
                    in_use: in_use_image_ids.iter().any(|x| x == &id),
                    id,
                    size: jstr(v, "Size"),
                    created: jstr(v, "CreatedAt"),
                })
            })
            .collect();

        // 应用内项目标注:按「项目名 == 目录名」或 compose 文件名匹配
        let dir_name = dir.rsplit('/').next().unwrap_or("");
        // 归属匹配:按「项目名 == 目录名」或 compose 副本路径命中应用内项目;
        // 同时取其归档保留数量(未命中/未配置 = 默认 5),与部署收尾同口径
        let matched = cfg.projects.iter().find(|p| {
            p.name == dir_name
                || p.compose_file
                    .rsplit_once('/')
                    .map(|(d, _)| d == dir)
                    .unwrap_or(false)
        });
        let app_project = matched.map(|p| p.name.clone()).unwrap_or_default();
        let release_keep = matched
            .map(crate::config::release_keep_of)
            .unwrap_or(crate::config::DEFAULT_RELEASE_KEEP);

        projects.push(CleanupProject {
            dir: dir.clone(),
            compose_file,
            size,
            releases,
            tag_images,
            app_project,
            release_keep,
        });
    }

    Ok((projects, diags, warnings))
}

/// 拼装删除无标签镜像的命令(逐 ID `docker rmi`,不使用 prune)。
///
/// 为什么不用 `docker image prune -f`:prune 删除的范围由 docker 自行判定,
/// 与用户在预览里勾选的条目未必一致;逐 ID 删除让执行结果与勾选一一对应。
pub(crate) fn rmi_ids_cmd(ids: &[String]) -> String {
    let quoted: Vec<String> = ids.iter().map(|i| shell_single_quote(i)).collect();
    format!("docker rmi {}", quoted.join(" "))
}

/// 拼装删除停止容器的命令(逐 ID `docker rm`)。
fn rm_container_ids_cmd(ids: &[String]) -> String {
    let quoted: Vec<String> = ids.iter().map(|i| shell_single_quote(i)).collect();
    format!("docker rm {}", quoted.join(" "))
}

/// 拼装删除未使用卷的命令(逐名 `docker volume rm`;卷被占用时该条失败不影响其余)。
pub(crate) fn rm_volume_names_cmd(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| shell_single_quote(n)).collect();
    format!("docker volume rm {}", quoted.join(" "))
}

/// 拼装删除发布归档目录的命令(逐目录 `rm -rf`;路径由后端拼自服务器扫描结果)。
pub(crate) fn rm_release_dirs_cmd(dirs: &[String]) -> String {
    let quoted: Vec<String> = dirs.iter().map(|d| shell_single_quote(d)).collect();
    format!("rm -rf {}", quoted.join(" "))
}

/// 定向执行勾选的清理项(逐节流式输出 server-log,与既有清理同通道)。
/// 至少勾选一项;各节独立执行,单节失败不影响其余。
///
/// 每节都按前端回传的**显式目标列表**逐条删除(见 [`CleanupSections`] 注释);
/// 分项目清理复用同一命令,避免两套实现。
#[tauri::command]
pub async fn cleanup_execute(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    sections: CleanupSections,
) -> Result<Vec<CleanupSectionResult>, String> {
    if !sections.has_any() {
        return Err("请至少勾选一项要清理的内容".to_string());
    }
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

    let mut plan: Vec<(String, String)> = Vec::new();
    if sections.images && !sections.image_ids.is_empty() {
        plan.push((
            format!("无标签镜像({} 项)", sections.image_ids.len()),
            rmi_ids_cmd(&sections.image_ids),
        ));
    }
    if sections.containers && !sections.container_ids.is_empty() {
        plan.push((
            format!("停止容器({} 个)", sections.container_ids.len()),
            rm_container_ids_cmd(&sections.container_ids),
        ));
    }
    if sections.volumes && !sections.volume_names.is_empty() {
        plan.push((
            format!("未使用卷({} 个)", sections.volume_names.len()),
            rm_volume_names_cmd(&sections.volume_names),
        ));
    }
    if sections.builder {
        plan.push(("构建缓存".to_string(), "docker builder prune -f".to_string()));
    }
    // 分项目:归档删除与标签删除各聚合成一条命令(减少往返),逐项结果由输出体现
    let all_release_dirs: Vec<String> = sections
        .projects
        .iter()
        .flat_map(|p| p.release_dirs.iter().cloned())
        .collect();
    if !all_release_dirs.is_empty() {
        plan.push((
            format!("旧发布归档({} 个)", all_release_dirs.len()),
            rm_release_dirs_cmd(&all_release_dirs),
        ));
    }
    let all_image_refs: Vec<String> = sections
        .projects
        .iter()
        .flat_map(|p| p.image_refs.iter().cloned())
        .collect();
    if !all_image_refs.is_empty() {
        plan.push((
            format!("旧版本镜像({} 个)", all_image_refs.len()),
            rmi_ids_cmd(&all_image_refs),
        ));
    }

    let mut results = Vec::new();
    for (label, cmd) in plan {
        let mut lines: Vec<String> = Vec::new();
        // 借用作用域:on_output 持有 &mut client,出块后释放,便于循环下一节继续用
        let outcome: Result<i32, String> = {
            let mut on_output = |line: &str| {
                let t = line.trim_end();
                let _ = app.emit("server-log", t.to_string());
                lines.push(t.to_string());
            };
            let fut = client.exec(&cmd, &mut on_output);
            with_timeout(
                PRUNE_TIMEOUT_SECS,
                "服务器清理超时",
                "请检查服务器网络后重试",
                async { fut.await.map_err(|e| format!("执行清理命令失败: {}", e)) },
            )
            .await
        };
        // 单节传输层失败只记为该节失败,不中断后续节(与"各节独立"语义一致)
        match outcome {
            Ok(code) => {
                if code != 0 {
                    let _ = app.emit(
                        "server-log",
                        format!("[{}] 清理失败(退出码 {})", label, code),
                    );
                }
                results.push(CleanupSectionResult {
                    label,
                    ok: code == 0,
                    output: lines.join("\n"),
                });
            }
            Err(e) => {
                let _ = app.emit("server-log", format!("[{}] {}", label, e));
                results.push(CleanupSectionResult {
                    label,
                    ok: false,
                    output: format!("{}\n{}", lines.join("\n"), e),
                });
            }
        }
    }
    Ok(results)
}

// ===== 跨服务器镜像迁移(阶段十:源 save → 下载本地 → 上传目标 → load)=====

