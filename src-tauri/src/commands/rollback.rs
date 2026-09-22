// // 一键回滚 + 独立回滚中心 + 归档版本说明 + manifest

use super::*;

/// manifest.json 中单个服务的镜像条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestImage {
    /// compose 服务名
    pub service: String,
    /// 镜像引用(`repo:tag`;`docker load` 会恢复包内镜像的原引用)
    pub tag: String,
    /// 发布目录内的镜像包文件名;未打包(跳过传输)为 `null`
    pub file: Option<String>,
    /// 本次发布装载的镜像完整 ID(`sha256:` 前缀原样;第二十一批补丁新增)。
    /// `None` = 旧归档(本字段引入前的 manifest)或采集失败 —— 两版本对比
    /// 见到 `None` 时回退按 `tag` 比较(v6.3.1 前的行为)。**有 ID 时按 ID
    /// 比较**:同名 tag 重新构建后 ID 不同,旧行为会误判「不变」。
    #[serde(default)]
    pub id: Option<String>,
}

/// 回滚时某个服务的镜像来源(第二十九批 R1;纯判定,便于单测)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackImageSource {
    /// 归档内有镜像包 → `docker load` 使用(本次部署变化过的服务)
    Archived,
    /// 归档内无包(**智能传输跳过**),但远端仍持有 manifest 记录的镜像 ID
    /// **且该标签正指向它** → 直接可用,无需重新上传(这是 R1 要支持的核心场景)
    RemoteById,
    /// 归档内无包、远端**仍持有该镜像 ID**,但 compose 期望的标签已指向别的
    /// ID(v6.12.0 新增):执行链 up 前会以「按 ID 收敛」步骤把标签**零拷贝
    /// 指回**该 ID,故不是「回不去」,但必须显式展示(真机案例:
    /// goodlaser-backend:latest 指向 265b2e14d9a6,归档 ID c313095267ee 仍在
    /// 服务器上挂于其它标签 —— 旧行为报「回不去」,用户毫无出路)。
    RemoteByIdTagMoved,
    /// 跳过且远端已无该 ID:旧镜像已被覆盖或清理 —— **回滚不了这个服务**,
    /// 必须让用户知道(此前是静默沿用当前镜像,界面还报「回滚完成」)
    Missing,
    /// 旧归档(manifest 无 id 记录):无从核对,保守标阻断并给指引
    Unknown,
}

impl RollbackImageSource {
    /// 契约串(camelCase 返回体里的 `source` 字段;前端据此渲染四态)。
    pub fn as_str(self) -> &'static str {
        match self {
            RollbackImageSource::Archived => "archived",
            RollbackImageSource::RemoteById => "remoteById",
            RollbackImageSource::RemoteByIdTagMoved => "tagRestore",
            RollbackImageSource::Missing => "missing",
            RollbackImageSource::Unknown => "unknown",
        }
    }
}

/// [`RollbackImageSource`] → 契约串(测试与调用方便捷入口;同 [`RollbackImageSource::as_str`])。
pub fn rollback_source_str(source: RollbackImageSource) -> &'static str {
    source.as_str()
}

/// 单个服务的回滚可用性结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackImagePlan {
    pub service: String,
    pub tag: String,
    pub source: RollbackImageSource,
    /// 阻断项(须用户确认后才继续)
    pub blocking: bool,
    /// 面向用户的说明(缺失原因 / 可用来源)
    pub detail: String,
}

/// 回滚预检汇总(供 UI 文案与「是否有阻断项」判定)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollbackPrecheckSummary {
    pub archived: usize,
    pub remote_by_id: usize,
    /// tag 被占、但 ID 仍在 → 执行时可自动指回(v6.12.0;不计入阻断)
    pub tag_restore: usize,
    pub missing: usize,
    pub unknown: usize,
}

impl RollbackPrecheckSummary {
    pub fn has_blocking(&self) -> bool {
        self.missing > 0 || self.unknown > 0
    }
}

/// 判断远端镜像列表里**是否存在**给定 ID(忽略 `sha256:` 前缀差异;纯函数)。
fn remote_has_image_id(remote: &[(String, String)], id: &str) -> bool {
    let want = id.trim().strip_prefix("sha256:").unwrap_or(id.trim());
    if want.is_empty() {
        return false;
    }
    remote.iter().any(|(_, rid)| {
        let got = rid.trim().strip_prefix("sha256:").unwrap_or(rid.trim());
        got == want
    })
}

/// 取远端列表中某 tag 当前指向的 ID(无该 tag → `None`;忽略 `sha256:` 前缀差异)。
/// 用于核对「compose 期望的标签是否仍指向归档记录的那个镜像」。
fn remote_id_of_tag(remote: &[(String, String)], tag: &str) -> Option<String> {
    remote
        .iter()
        .find(|(r, _)| r == tag)
        .map(|(_, rid)| {
            rid.trim()
                .strip_prefix("sha256:")
                .unwrap_or(rid.trim())
                .to_string()
        })
}

/// 生成回滚可用性计划(纯函数,便于单测;第二十九批 R1)。
///
/// **为什么需要**:智能传输会跳过未变化镜像的打包,归档里因此没有它们的包
/// (`ManifestImage.file = null`);而回滚此前只按「目录里实际有哪些 tar.gz」
/// 装载 —— 缺包的服务被**静默跳过**,`docker compose up -d` 用服务器上当前的
/// 镜像成功启动,界面报「回滚完成」而服务其实没回退。本函数把这件事变成
/// 显式的三态结论,让调用方要么按 ID 用上仍然存在的镜像,要么明确告知用户。
///
/// `remote` = 服务器当前镜像列表 `(repo:tag, id)`(由 `query_remote_images_full`
/// 取得)。**跳过服务的镜像只要还在远端,就能直接用**(部署时之所以能跳过,正是
/// 因为远端已有同 ID 镜像)。
pub fn plan_rollback(
    images: &[ManifestImage],
    remote: &[(String, String)],
) -> Vec<RollbackImagePlan> {
    // 不带目录文件清单的调用 = 假定 manifest 记录的包都在(仅测试与
    // 「清单不可得」的降级路径用)。真实调用请用 [`plan_rollback_with_files`]。
    plan_rollback_with_files(images, remote, &[])
}

/// [`plan_rollback`] 的完整版:额外核对**归档目录里实际有哪些文件**。
///
/// **为什么必须核对**(补丁审查发现的第二处漏洞):manifest 是**部署当时**
/// 写下的记录,目录内容事后可能变(清理分析逐目录删除、用户手工 rm 等)。
/// 首版只看 `manifest.images[].file` 是否为 Some 就判「归档有包 → 可用」,
/// 而装载循环是按**实际文件清单**走的 —— 包不在时 `docker load` 被静默跳过,
/// `up -d` 用当前镜像启动,界面报「回滚完成」。与本批根除的失效路径同类。
///
/// `actual_files` = `ls <release_dir>` 的结果(空 = 不核对,退化为首版行为)。
pub fn plan_rollback_with_files(
    images: &[ManifestImage],
    remote: &[(String, String)],
    actual_files: &[String],
) -> Vec<RollbackImagePlan> {
    plan_rollback_full(images, remote, actual_files, true)
}

/// [`plan_rollback_with_files`] 的完整版:`remote_available=false` 表示**查询远端
/// 镜像列表失败**(daemon 异常/权限),此时不能把「查不到」说成「镜像丢了」——
/// 两者的用户处置完全不同(前者重试即可,后者得重新部署)。都阻断(fail-closed),
/// 但文案必须区分。
pub fn plan_rollback_full(
    images: &[ManifestImage],
    remote: &[(String, String)],
    actual_files: &[String],
    remote_available: bool,
) -> Vec<RollbackImagePlan> {
    let check_files = !actual_files.is_empty();
    images
        .iter()
        .map(|img| {
            if let Some(f) = img.file.as_deref().filter(|f| !f.trim().is_empty()) {
                if check_files && !actual_files.iter().any(|a| a == f) {
                    // manifest 说在,目录里没有 → 静默跳过的根源,必须阻断
                    return RollbackImagePlan {
                        service: img.service.clone(),
                        tag: img.tag.clone(),
                        source: RollbackImageSource::Missing,
                        blocking: true,
                        detail: format!(
                            "{}:发布清单记录的镜像包 {} 不在归档目录里(可能被清理过),无法从归档恢复到该版本",
                            img.tag, f
                        ),
                    };
                }
                // 归档内有包:最可靠来源
                return RollbackImagePlan {
                    service: img.service.clone(),
                    tag: img.tag.clone(),
                    source: RollbackImageSource::Archived,
                    blocking: false,
                    detail: "归档内有镜像包,docker load 后使用".to_string(),
                };
            }
            match img.id.as_deref() {
                Some(id) if !id.trim().is_empty() => {
                    if !remote_available {
                        // 查询失败 ≠ 镜像丢了:文案要给出「重试」这条可执行出路
                        return RollbackImagePlan {
                            service: img.service.clone(),
                            tag: img.tag.clone(),
                            source: RollbackImageSource::Unknown,
                            blocking: true,
                            detail: format!(
                                "{}:无法查询服务器镜像列表(docker 异常或无权限),暂时核对不了该镜像是否可用; 请确认服务器 Docker 正常后重试预检",
                                img.tag
                            ),
                        };
                    }
                    let id_present = remote_has_image_id(remote, id);
                    // **必须同时**核对该 ID 是否被 compose 期望的 tag 引用
                    // (补丁审查修正):整栈回滚无 `docker tag` 步骤,只靠
                    // `docker load` 恢复标签;跳过服务没有包 → `up -d` 按
                    // compose 里的 `repo:tag` 找镜像。若 ID 还在但那个 tag
                    // 已指向别的 ID(版本被覆盖),up 会拉起**新版本** ——
                    // 界面报「回滚完成」而服务实际没回退。
                    let want_id = id.trim().strip_prefix("sha256:").unwrap_or(id.trim());
                    let tag_now = remote_id_of_tag(remote, &img.tag);
                    let tag_points_here = tag_now.as_deref() == Some(want_id);
                    if id_present && tag_points_here {
                        RollbackImagePlan {
                            service: img.service.clone(),
                            tag: img.tag.clone(),
                            source: RollbackImageSource::RemoteById,
                            blocking: false,
                            detail: format!(
                                "智能传输跳过打包,服务器上仍持有该镜像({})且标签指向它,直接使用",
                                short_id(id)
                            ),
                        }
                    } else if id_present {
                        // ID 还在,但期望的 tag 已指向别的镜像 → v6.12.0 起不再是
                        // 死路:执行链 up 前按 ID 收敛(零拷贝 docker tag 指回)。
                        // 仍显式展示,让用户知道执行时会动标签。
                        RollbackImagePlan {
                            service: img.service.clone(),
                            tag: img.tag.clone(),
                            source: RollbackImageSource::RemoteByIdTagMoved,
                            blocking: false,
                            detail: match tag_now {
                                Some(cur) => format!(
                                    "{}:镜像 {} 仍存在于服务器(挂在其它的标签下),该标签现指向 {} —— 执行时会自动把标签指回归档版本(零拷贝)",
                                    img.tag,
                                    short_id(id),
                                    short_id(&cur)
                                ),
                                None => format!(
                                    "{}:镜像 {} 仍存在于服务器,但该标签已不存在 —— 执行时会自动按该 ID 重建标签",
                                    img.tag,
                                    short_id(id)
                                ),
                            },
                        }
                    } else {
                        // 同 tag 是否被别的 ID 占用,决定给哪句提示
                        let tag_taken = remote.iter().any(|(r, _)| r == &img.tag);
                        RollbackImagePlan {
                            service: img.service.clone(),
                            tag: img.tag.clone(),
                            source: RollbackImageSource::Missing,
                            blocking: true,
                            detail: if tag_taken {
                                format!(
                                    "{}:镜像 {} 已不在服务器上(该标签现指向其它镜像,版本已被覆盖),无法回退到本次归档的版本",
                                    img.tag,
                                    short_id(id)
                                )
                            } else {
                                format!(
                                    "{}:镜像 {} 已不在服务器上(可能被清理),无法回退",
                                    img.tag,
                                    short_id(id)
                                )
                            },
                        }
                    }
                }
                _ => RollbackImagePlan {
                    service: img.service.clone(),
                    tag: img.tag.clone(),
                    source: RollbackImageSource::Unknown,
                    blocking: true,
                    detail: format!(
                        "{}:该归档未记录镜像 ID(早于本功能),无法核对是否可用; 如需可回滚请重新部署一次(新归档会记录 ID)",
                        img.tag
                    ),
                },
            }
        })
        .collect()
}

/// 汇总计划(纯函数)。
pub fn rollback_precheck_summary(plan: &[RollbackImagePlan]) -> RollbackPrecheckSummary {
    let mut s = RollbackPrecheckSummary::default();
    for p in plan {
        match p.source {
            RollbackImageSource::Archived => s.archived += 1,
            RollbackImageSource::RemoteById => s.remote_by_id += 1,
            RollbackImageSource::RemoteByIdTagMoved => s.tag_restore += 1,
            RollbackImageSource::Missing => s.missing += 1,
            RollbackImageSource::Unknown => s.unknown += 1,
        }
    }
    s
}

/// 阻断时的错误文案(纯函数,便于单测):**必须把回不去的服务逐条列出**,
/// 并给出两条出路(接受部分回滚 / 重新部署记录 ID),不能只说「有 N 项阻断」。
/// 前端据前缀 `[dderr:rollback-precheck]` 识别,并弹「仍要回滚」确认。
pub fn rollback_precheck_block_message(
    plan: &[RollbackImagePlan],
    sum: &RollbackPrecheckSummary,
) -> String {
    let mut lines = vec![format!(
        "回滚可用性预检未通过:{} 个服务无法回退到该归档(归档内有包 {} 个、服务器上按镜像 ID 命中 {} 个、标签待指回 {} 个)",
        sum.missing + sum.unknown,
        sum.archived,
        sum.remote_by_id,
        sum.tag_restore
    )];
    for p in plan.iter().filter(|p| p.blocking) {
        lines.push(format!("· {}", p.detail));
    }
    lines.push(
        "如仍要回滚,其余服务会正常恢复,上述服务将沿用服务器当前镜像(版本可能与归档不一致)。选择「仍要回滚」继续,或在命令后重试时附带确认。".to_string(),
    );
    lines.join("
")
}

/// 插值漂移的确认文案(纯函数,便于单测;v6.12.0)。
///
/// 与「回不去」同款:**必须逐条列出**哪个服务、归档记的什么、当前 `.env` 会
/// 解析成什么 —— 只说「有 N 项漂移」对用户毫无行动价值(用户要去改 `.env`
/// 或接受结果)。前端据错误码 `rollback_precheck` 弹「仍要回滚」确认。
pub fn rollback_env_drift_message(drifted: &[ImageEnvDrift]) -> String {
    let mut lines = vec![format!(
        ".env 插值漂移预检未通过:{} 个服务的镜像引用会被服务器当前 .env 解析成与归档不同的值(.env 不入归档,回滚不恢复它)",
        drifted.len()
    )];
    for d in drifted {
        lines.push(format!(
            "· {}:归档记录 {};按当前 .env 会解析成 {}",
            d.service, d.expected, d.resolved
        ));
    }
    lines.push(
        "继续回滚将按当前 .env 解析出的引用启动这些服务(可能与归档版本不符)。如要精确回到归档版本,请先把服务器上的 .env 改回部署时的值;或选择「仍要回滚」接受现状。".to_string(),
    );
    lines.join("
")
}

// ===== v6.12.0:按镜像 ID 收敛(up 前把标签指回归档 ID)=====

/// 收敛计划:`(源 ref, 目标 tag)` 列表 —— 用 `docker tag <源> <目标>` 零拷贝
/// 把 compose 期望的标签指回记录镜像 ID 的计划(统一形态,回滚/部署两侧共用)。
pub struct TagConvergencePlan {
    /// 需要执行的 `docker tag`(`(服务名, 镜像 ID 源, 目标标签)`)
    pub retag: Vec<(String, String, String)>,
    /// 收敛不了的服务名 + 展示引用(ID 已不在服务器)—— 由调用方决定是否阻断
    pub missing: Vec<(String, String)>,
}

/// 「按 ID 收敛」的**唯一实现**(纯函数,便于单测;v6.12.0)。
///
/// 输入:`expectations` = 逐服务的 `(服务名, compose 期望的完整引用, 期望的镜像 ID)`;
/// `remote` = 服务器当前镜像列表 `(repo:tag, id)`(`query_remote_images_full` 取得)。
///
/// **为什么需要**:预检/判定都只是**查询那一刻**的快照,而 `up -d` 一律按
/// compose 里的 `repo:tag` 解析镜像 —— 二者之间隔着逐包 `docker load`(秒级
/// 到分钟级)或整个上传过程,期间任何外部主体(另一台机器的本应用 / CI /
/// watchtower)移动标签都会让结论失效(第二十九批记录的 TOCTOU 假设)。
/// 治本 = up 前复核并**就地收敛**:记录过 ID 的服务,若标签没指向它,就用
/// `docker tag <ID> <tag>` 指回(零拷贝,不需要重新传输)—— 真机案例
/// `goodlaser-backend:latest` 指向 265b2e14d9a6、归档 ID c313095267ee 仍挂
/// 在其它的标签下,旧行为直接报「回不去」,用户毫无出路。
///
/// **判定三态**:ID 不在 → 记入 `missing`(唯一真正的「收敛不了」);
/// ID 在且标签已指向它 → 无动作;ID 在但标签指向别处/不存在 → 出 `retag`。
///
/// `remote` 不可得时调用方**不要**调用本函数(空列表会让所有项落入 `missing`)
/// —— 那是「查询失败」而非「镜像丢失」,两者用户处置完全不同。
pub fn plan_tag_convergence_pairs(
    expectations: &[(String, String, String)],
    remote: &[(String, String)],
) -> TagConvergencePlan {
    let mut retag: Vec<(String, String, String)> = Vec::new();
    let mut missing: Vec<(String, String)> = Vec::new();
    for (service, tag, id) in expectations {
        let id = id.trim();
        if id.is_empty() {
            continue; // 未采集到 ID 的服务无从收敛(调用方另行降级)
        }
        if !remote_has_image_id(remote, id) {
            missing.push((service.clone(), tag.clone()));
            continue;
        }
        // ID 在:标签是否已指向它?(对齐则不产生多余动作)
        let want = id.strip_prefix("sha256:").unwrap_or(id);
        let tag_now = remote_id_of_tag(remote, tag);
        if tag_now.as_deref() == Some(want) {
            continue;
        }
        // 源用记录到的 ID(避开标签歧义),目标 = compose 期望的完整引用
        retag.push((service.clone(), id.to_string(), tag.clone()));
    }
    TagConvergencePlan { retag, missing }
}

/// 回滚侧的收敛计划:`ManifestImage` → [`plan_tag_convergence_pairs`](唯一实现)。
///
/// 范围(与预检同口径):只处理「**无归档包**且记了 ID」的服务 ——
/// - 有包的服务:标签由包内元数据恢复(`docker load`),不在此收敛
///   (装载后标签必然正确;装载前查列表可能还看不到);
/// - 无 ID 的旧归档:无从收敛,保持既有降级(预检已按另一口径阻断/放行)。
pub fn plan_tag_convergence(
    images: &[ManifestImage],
    remote: &[(String, String)],
) -> TagConvergencePlan {
    let expectations: Vec<(String, String, String)> = images
        .iter()
        .filter(|img| {
            // 有包 → 走装载通道,不参与 ID 收敛
            !img.file
                .as_deref()
                .map(|f| !f.trim().is_empty())
                .unwrap_or(false)
        })
        .filter_map(|img| {
            img.id.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|id| (img.service.clone(), img.tag.clone(), id.to_string()))
        })
        .collect();
    plan_tag_convergence_pairs(&expectations, remote)
}

// ===== v6.12.0:清单驱动装载(目录里多余的包不再静默装载)=====

/// 装载选择结果(纯函数产物;见 [`select_packages`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSelection {
    /// 应装载的包(manifest 记录 ∩ 目录实际存在;**按 manifest 顺序**)
    pub to_load: Vec<String>,
    /// manifest 记录的包不在目录里(预检已阻断;执行侧防御性再报)
    pub recorded_missing: Vec<String>,
    /// 目录里存在但 manifest 未记录的 `.tar.gz`(不再静默装载)
    pub unrecorded: Vec<String>,
}

/// 按 manifest 选择要装载的包(纯函数,便于单测;v6.12.0)。
///
/// **为什么需要**(第二十九批遗留的「无反向校验」):装载循环此前按目录
/// `ls` 结果**全量**装载 —— 目录里若有 manifest 未记录的 tar(人工放入、
/// 半成品残留),会被照常 `docker load`,其包内标签静默覆盖服务器现状,
/// 而预检/界面毫无提示。修正后:只装载 manifest 记录的包,未记录的跳过并
/// 警告,记录的包缺失则单列(预检已阻断,这里再报是防御性冗余)。
///
/// `images` 为空(无 manifest 的旧归档)时 `to_load` 为空、目录里的包全部
/// 归入 `unrecorded` —— 由调用方决定是否降级为「按包恢复」并给出提示
/// (现状行为,不静默)。
pub fn select_packages(images: &[ManifestImage], actual_files: &[String]) -> PackageSelection {
    let is_tar = |f: &str| f.ends_with(".tar.gz");
    let recorded: Vec<&str> = images
        .iter()
        .filter_map(|i| i.file.as_deref())
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .collect();
    let mut to_load: Vec<String> = Vec::new();
    let mut recorded_missing: Vec<String> = Vec::new();
    for name in &recorded {
        if actual_files.iter().any(|f| f == name) {
            to_load.push((*name).to_string());
        } else {
            recorded_missing.push((*name).to_string());
        }
    }
    let unrecorded: Vec<String> = actual_files
        .iter()
        .filter(|f| is_tar(f) && !recorded.contains(&f.as_str()))
        .cloned()
        .collect();
    PackageSelection {
        to_load,
        recorded_missing,
        unrecorded,
    }
}

/// 解析批量 `docker inspect` 输出为 `(服务, 镜像 ID)`(纯函数,便于单测)。
///
/// 输入形态:`docker inspect --format '{{.Config.Labels."com.docker.compose.service"}}|{{.Image}}' <cid...>`
/// 的逐行结果(`服务名|sha256:...`);空行 / 无 `|` 的行 / 任一字段为空的行跳过。
/// 用批量形态而非逐服务两次往返:up 后校验只花 2 次 SSH(`ps -q` 一次拿全部
/// 容器 ID,`inspect` 一次拿全部服务名 + 镜像)。
pub fn parse_compose_service_images(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let (svc, img) = line.split_once('|')?;
            let svc = svc.trim();
            let img = img.trim();
            if svc.is_empty() || img.is_empty() {
                return None;
            }
            Some((svc.to_string(), img.to_string()))
        })
        .collect()
}

/// up 后校验的 `docker inspect` 模板:`<compose 服务名>|<镜像 ID>`。
///
/// **必须用 `index` 读带点的标签键**:`.Config.Labels."com.docker.compose.service"`
/// **不是合法 Go 模板**(字段链只接受标识符;带点的键要用 `index`)—— docker CLI
/// 在连接 daemon **之前**解析模板,直接报 `template parsing error: bad character
/// U+0022` 并以 **退出码 64** 退出(v6.12.0 首发真机实测:`up 后校验` 每次部署都
/// 失败,只告警不误判成败,但校验整条形同虚设)。由
/// `test_compose_service_image_template_parses` 守护(含真机 docker CLI 解析)。
pub(crate) const COMPOSE_SERVICE_IMAGE_TEMPLATE: &str =
    "{{index .Config.Labels \"com.docker.compose.service\"}}|{{.Image}}";

/// 采集 compose 项目内容器的 `(compose 服务名, 镜像 ID)`(v6.12.0;up 后校验共用)。
///
/// 2 次 SSH:`ps -q --all`(全部容器 ID;**含已退出的** —— 只查 running 会把
/// 「容器仍在跑旧镜像」和「一次性任务已退出」都漏掉/误判)+ 一次 `docker
/// inspect`(服务名 + 镜像)。容器无 compose 服务标签(非 compose 起的)时服务名
/// 解析为空 → 整行跳过(校验只关心本次部署/回滚的服务)。
///
/// 失败信息附远端输出尾部(命令输出与 stderr 合并):模板/参数类错误会直接显形
/// (v6.12.0 真机教训 —— 只报「退出码 64」时用户与维护者都要反推)。
pub(crate) async fn collect_running_images(
    client: &mut SshClient,
    compose_prefix: &str,
) -> Result<Vec<(String, String)>, String> {
    let (code, ps_out) = exec_collect(client, &format!("{} ps -q --all", compose_prefix)).await?;
    if code != 0 {
        return Err(format!(
            "compose ps 查询失败(退出码 {}):{}",
            code,
            tail_lines(&ps_out, 3)
        ));
    }
    let cids: Vec<String> = parse_ls_lines(&ps_out);
    if cids.is_empty() {
        return Ok(Vec::new());
    }
    let quoted: Vec<String> = cids
        .iter()
        .map(|c| crate::commands::shell_single_quote(c))
        .collect();
    let inspect = format!(
        "docker inspect --format {} {}",
        crate::commands::shell_single_quote(COMPOSE_SERVICE_IMAGE_TEMPLATE),
        quoted.join(" ")
    );
    let (code, ins_out) = exec_collect(client, &inspect).await?;
    if code != 0 {
        return Err(format!(
            "docker inspect 查询失败(退出码 {}):{}",
            code,
            tail_lines(&ins_out, 3)
        ));
    }
    Ok(parse_compose_service_images(&ins_out))
}

/// 从 manifest 生成 up 后校验的期望表(纯函数,便于单测):只取有 ID 的服务。
pub fn expected_images_from_manifest(images: &[ManifestImage]) -> Vec<(String, String)> {
    images
        .iter()
        .filter_map(|i| {
            i.id.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|id| (i.service.clone(), id.to_string()))
        })
        .collect()
}

/// 单镜像部署的 up 后校验(纯函数,便于单测):期望镜像 ID 是否被**任一**运行
/// 容器使用。
///
/// 单镜像管线拿不到「服务名 ↔ 镜像」映射(该映射在用户 compose 里),故不能按
/// 服务核对;可判定的是:**本次部署的镜像有没有真的跑起来**。若没有任何容器
/// 在跑它,说明 compose 的 `image:` 解析到了别的引用(如 `.env` 插值漂移)或
/// 容器未重建 —— 都必须显式告警,不能只看「up 退出码 0」。
pub fn expected_image_running_anywhere(expected_id: &str, actual: &[(String, String)]) -> bool {
    let want = expected_id
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(expected_id.trim());
    if want.is_empty() {
        return true; // 无从比对(未采集到 ID):不误报
    }
    actual.iter().any(|(_, id)| {
        let got = id.trim().strip_prefix("sha256:").unwrap_or(id.trim());
        !got.is_empty() && got == want
    })
}

// ===== v6.12.0:up 后运行镜像校验(容器实际镜像 ≠ 期望 → 显式报告)=====

/// 逐服务核对容器实际运行的镜像 ID(纯函数,便于单测)。
///
/// **为什么需要**:up 前的一切判定(预检、收敛、清单装载)都是「准备动作」
/// 的正确性,而 `up -d` 的实际结果还受 `.env` 插值漂移、compose 副本恢复
/// 失败、compose 认为无变化不重建容器等残余路径影响。这里在 up **之后**
/// 用容器实况兜底:调用方从 `compose ps -q`(容器 ID)+ `docker inspect`
/// 取 `(服务, 实际镜像 ID)`,与本函数的期望表比对。
///
/// 期望表的来源:`manifest` 记录的 ID(回滚)/ 本次采集的本地 ID(部署)。
/// `actual` 中缺服务(没起容器/被 scale 到 0)同样算不一致(实际值记空串),
/// 不能静默。
///
/// 返回:`(服务, 期望 ID, 实际 ID)` 列表(空 = 全部一致)。
pub fn select_container_image_mismatches(
    expected: &[(String, String)],
    actual: &[(String, String)],
) -> Vec<(String, String, String)> {
    let same = |a: &str, b: &str| -> bool {
        let a = a.trim().strip_prefix("sha256:").unwrap_or(a.trim());
        let b = b.trim().strip_prefix("sha256:").unwrap_or(b.trim());
        !a.is_empty() && !b.is_empty() && a == b
    };
    let mut out: Vec<(String, String, String)> = Vec::new();
    for (svc, want) in expected {
        let got = actual
            .iter()
            .find(|(s, _)| s == svc)
            .map(|(_, id)| id.clone())
            .unwrap_or_default();
        if !same(want, &got) {
            out.push((svc.clone(), want.clone(), got));
        }
    }
    out
}

/// 归档内的 override 副本名 → 原始文件名(纯函数,便于单测)。
///
/// 只认 `<compose 前缀>*.yml|.yaml.ddbak` 形态(避免把归档里其它
/// `.ddbak` 后缀物误当 override 恢复到项目目录);非该形态返回 `None`。
pub fn archived_override_original(archived_name: &str) -> Option<&str> {
    let orig = archived_name.strip_suffix(".ddbak")?;
    let is_yaml = orig.ends_with(".yml") || orig.ends_with(".yaml");
    if !is_yaml {
        return None;
    }
    let stem = orig.rsplit_once('.').map(|(s, _)| s).unwrap_or(orig);
    // override 形态 = 前缀是 compose 族名,且**点号后不止一个段**
    // (docker-compose.yml / compose.yml 是 base 本体,走 compose 副本通道,
    // 不在此恢复;docker-compose.override.yml / compose.prod.yml 才是 override)
    let is_override_like = stem.starts_with("docker-compose.") || stem.starts_with("compose.");
    if is_override_like {
        Some(orig)
    } else {
        None
    }
}

/// 镜像 ID 短哈希(展示用;纯函数;v6.12.0 起跨模块共用)。
pub(crate) fn short_id(id: &str) -> String {
    let bare = id.trim().strip_prefix("sha256:").unwrap_or(id.trim());
    bare.chars().take(12).collect()
}

/// 整栈部署成功时写入发布目录的 `manifest.json` 结构(回滚列表页据此展示
/// 各 release 包含的服务;`docker-compose.yml` 副本随清单一并归档)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub project: String,
    /// 发布时间戳(即 release 目录名,形如 20260905-101010)
    pub ts: String,
    pub compose_copy: String,
    pub images: Vec<ManifestImage>,
}

impl ReleaseManifest {
    /// `compose_copy` 固定为发布目录内的归档文件名。
    pub fn new(project: String, ts: String, images: Vec<ManifestImage>) -> Self {
        Self {
            project,
            ts,
            compose_copy: "docker-compose.yml".to_string(),
            images,
        }
    }
}

/// 组装 manifest 的镜像条目(纯函数,便于单测):逐个 Local 服务一条;
/// `skip[i] == true` 表示该服务未打包(skip_unchanged 剔除且未留档),
/// `file` 记 `None`,其余按顺序消费 `packed_files` 中的镜像包文件名。
/// `tag` 存完整镜像引用(无标签时按 Docker 约定补 latest,见 [`split_image_ref`])。
///
/// `ids[i]`(第二十一批补丁):该服务镜像的完整 ID(`sha256:` 前缀原样;
/// 采集失败为 `None`)。写入 manifest 供两版本对比按内容(而非 tag 名)
/// 判变化 —— 同名 tag 重新构建后 ID 不同,只比 tag 会把「镜像换了」
/// 误报为「不变」(用户真机反馈)。
pub(crate) fn build_manifest_images(
    local: &[&StackServiceChoice],
    skip: &[bool],
    packed_files: &[String],
    ids: &[Option<String>],
) -> Vec<ManifestImage> {
    let mut files = packed_files.iter();
    local
        .iter()
        .enumerate()
        .map(|(i, svc)| {
            let (repo, tag) = split_image_ref(&svc.image);
            let file = if skip.get(i).copied().unwrap_or(false) {
                None
            } else {
                files.next().cloned()
            };
            ManifestImage {
                service: svc.service.clone(),
                tag: format!("{}:{}", repo, tag),
                file,
                id: ids.get(i).cloned().flatten(),
            }
        })
        .collect()
}

/// 整栈部署收尾子步:向发布目录写入回滚资料 —— compose 副本(sftp 上传,
/// 归档名 `docker-compose.yml`,内容即本次部署使用的本地副本)与
/// `manifest.json`(base64 经远端 exec 解码写入,避免 JSON 引号/换行的
/// shell 转义问题)。
///
/// 尽力而为:任一失败仅告警 —— 发布目录缺 manifest/副本时,回滚命令会优雅
/// 降级(服务列表为空、沿用服务器现有 compose 文件),不推翻已成功的部署。
pub(crate) async fn write_release_artifacts(
    app: &AppHandle,
    client: &mut SshClient,
    project: &ProjectConfig,
    release_dir: &str,
    manifest: &ReleaseManifest,
) {
    // 1) compose 副本(部署前置已校验本地副本存在,直接复用)
    let compose_local = PathBuf::from(&project.compose_file);
    let archived = client
        .sftp_upload(&compose_local, release_dir, "docker-compose.yml", false, &|_, _| {})
        .await;
    match archived {
        Ok(()) => emit_log(app, "已存档 compose 副本到发布目录"),
        Err(e) => emit_log(
            app,
            &format!(
                "警告:存档 compose 副本失败(回滚时将沿用服务器现有 compose 文件): {}",
                e
            ),
        ),
    }

    // 1b) override 副本(R2;第二十九批):override 参与 compose 语义(服务定义/
    // 端口/环境可能被它改写),不同步归档会让回滚后的合并结果与部署当时不一致。
    // **`.env` 刻意不入归档**(用户裁决):它是环境变量,回滚它可能把密钥退回旧值,
    // 且服务器上本就保有明文;回滚不动 `.env`,由用户自理。
    // 命名加 `.ddbak.` 前缀,与根目录的同名 override 区分,回滚时按原名恢复。
    if let Some(parent) = PathBuf::from(&project.compose_file).parent() {
        for ov in find_override_files(parent) {
            let Some(name) = ov.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            let archived_name = format!("{}.ddbak", name);
            match client
                .sftp_upload(&ov, release_dir, &archived_name, false, &|_, _| {})
                .await
            {
                Ok(()) => emit_log(
                    app,
                    &format!("已存档 override 文件 {} 到发布目录(回滚时一并恢复)", name),
                ),
                Err(e) => emit_log(
                    app,
                    &format!("警告:存档 override 文件 {} 失败(回滚时该文件保持现状): {}", name, e),
                ),
            }
        }
    }

    // 2) manifest.json:echo <b64> | base64 -d > '<release_dir>/manifest.json'
    let json = match serde_json::to_string(manifest) {
        Ok(j) => j,
        Err(e) => {
            emit_log(app, &format!("警告:序列化 manifest.json 失败: {}", e));
            return;
        }
    };
    let manifest_path = remote_join(release_dir, "manifest.json");
    let cmd = format!(
        "echo {} | base64 -d > {}",
        BASE64_STANDARD.encode(json.as_bytes()),
        shell_single_quote(&manifest_path)
    );
    let code = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "写入发布清单超时",
        "请检查服务器网络后重试",
        async {
            client
                .exec(&cmd, &mut |_| {})
                .await
                .map_err(|e| format!("远端写入 manifest.json 失败: {}", e))
        },
    )
    .await;
    match code {
        Ok(0) => emit_log(app, "已写入发布清单 manifest.json"),
        Ok(c) => emit_log(
            app,
            &format!("警告:写入 manifest.json 失败(退出码 {}): {}", c, manifest_path),
        ),
        Err(e) => emit_log(app, &format!("警告:{}", e)),
    }
}

// ===== 一键回滚(整栈回滚到历史 release / 单镜像回滚到历史标签)=====

/// `rollback_list_releases` 返回的单个历史发布条目。
/// (Tauri 序列化为 camelCase,与前端 deploy.js 读取的 `hasManifest` /
/// `hasComposeCopy` 字段名一致。)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseBrief {
    /// 发布时间戳(releases 目录名,形如 20260905-101010)
    pub ts: String,
    /// 发布目录内的文件名列表(镜像包 + manifest.json / docker-compose.yml)
    pub files: Vec<String>,
    /// manifest.json 里记录的服务名列表;无清单(旧版本发布)为空
    pub services: Vec<String>,
    pub has_manifest: bool,
    pub has_compose_copy: bool,
}

/// `rollback_list_tags` 返回的单个本地镜像标签条目(按创建时间倒序)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagBrief {
    pub tag: String,
    /// 镜像 ID(`docker images` 的 ID 字段,含 `sha256:` 前缀原样返回)
    pub id: String,
    /// docker 的 CreatedAt 文本(如 `2026-08-01 10:00:00 +0800 CST`)
    pub created: String,
}

/// 列出项目服务器上的历史发布(新 → 旧),供前端渲染一键回滚列表。
///
/// `releases` 目录不存在(从未整栈部署)返回空列表,不是错误;单个发布目录
/// 缺 manifest.json / 读取失败时相应字段优雅降级(`services` 空、
/// `has_manifest` false),不让整个列表功能失败。
#[tauri::command]
pub async fn rollback_list_releases(
    server_id: String,
    password_plain: Option<String>,
    project_id: String,
) -> Result<Vec<ReleaseBrief>, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    // 项目存在性校验 + 取其项目级部署目录(第四批;未配置则回落服务器目录)
    let project = find_project(&cfg, &project_id)?.clone();
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

    let project_dir = effective_remote_dir(&server, &project);
    // 第七批二次优化:单次往返 —— find 循环在远端枚举归档并批量读取
    // (releases 目录不存在时 find 静默无输出 → 空列表,与既有口径一致);
    // 列表不需要镜像清单与 compose 文本,命令不带这两段
    let scan_cmd = releases_scan_cmd(&project_dir, 100, &[], false);
    let (_, dump_out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "读取发布列表超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &scan_cmd),
    )
    .await
    .unwrap_or((1, String::new()));
    let dump = parse_releases_dump(&dump_out, &[]);

    let mut briefs = Vec::new();
    for entry in &dump.entries {
        if entry.files.is_empty() {
            // 目录可能刚被清理或不可读,跳过该条目,不拖垮整个列表
            log::warn!("跳过无法读取的发布目录 {}(批量读取为空)", entry.ts);
            continue;
        }
        let files = entry.files.clone();
        let has_manifest = files.iter().any(|f| f == "manifest.json");
        let has_compose_copy = files.iter().any(|f| f == "docker-compose.yml");
        let services = parse_release_manifest(&entry.manifest)
            .map(|m| m.images.into_iter().map(|img| img.service).collect())
            .unwrap_or_default();
        briefs.push(ReleaseBrief {
            ts: entry.ts.clone(),
            files,
            services,
            has_manifest,
            has_compose_copy,
        });
    }
    // find + sort -r 已按新 → 旧返回(时间戳形如 20260905-101010)
    Ok(briefs)
}

/// 列出项目服务器上指定仓库的全部镜像标签(创建时间倒序),
/// 供单镜像回滚选择"回到哪个历史标签"。
#[tauri::command]
pub async fn rollback_list_tags(
    server_id: String,
    password_plain: Option<String>,
    project_id: String,
    repository: String,
) -> Result<Vec<TagBrief>, String> {
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    find_project(&cfg, &project_id)?;
    let (_server, mut client) = connect_server(&server_id, password_plain.as_deref(), None).await?;
    let cmd = format!(
        "docker images {} --format '{{{{json .}}}}'",
        shell_single_quote(&repository)
    );
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询远端标签超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &cmd),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "查询远端镜像标签失败(退出码 {}),请确认服务器 Docker 可用",
            code
        ));
    }
    let mut tags: Vec<TagBrief> = parse_image_lines(&out)
        .into_iter()
        .map(|i| TagBrief {
            tag: i.tag,
            id: i.id,
            created: i.created,
        })
        .collect();
    // 创建时间倒序(docker 的 CreatedAt 文本按字典序即时间序)
    tags.sort_by(|a, b| b.created.cmp(&a.created));
    Ok(tags)
}

/// 回滚预检的单条结果(camelCase 契约;第二十九批 R1)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackPrecheckItem {
    pub service: String,
    pub tag: String,
    /// `"archived"` | `"remoteById"` | `"missing"` | `"unknown"`
    pub source: String,
    pub blocking: bool,
    pub detail: String,
}

/// 回滚预检的单项插值漂移(camelCase 契约)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackEnvDriftItem {
    pub service: String,
    /// 归档记录的引用(部署时插值结果 = manifest 的 tag)
    pub expected: String,
    /// 按服务器当前 `.env` 会解析成的引用(up -d 实际会用的)
    pub resolved: String,
}

/// 回滚预检结果(camelCase)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackPrecheck {
    pub items: Vec<RollbackPrecheckItem>,
    pub archived: usize,
    pub remote_by_id: usize,
    /// 标签未指向归档 ID、但执行时会自动指回(v6.12.0;不计入阻断)
    pub tag_restore: usize,
    pub missing: usize,
    pub unknown: usize,
    /// 存在阻断项(前端据此把「开始回滚」变为「仍要回滚(部分)」)
    pub has_blocking: bool,
    /// `.env` 插值漂移(v6.12.0;须确认,与阻断同款处置)
    pub env_drift: Vec<RollbackEnvDriftItem>,
    /// 归档无 manifest(极旧/半成品):无法逐服务核对
    pub no_manifest: bool,
}

/// 回滚预检(只读;第二十九批 R1):列出该归档每个服务的镜像来源,
/// 让用户**在执行前**看清「哪些服务回不去」。
///
/// 与执行链内的同款预检共用 [`plan_rollback`] 纯函数 —— 单一事实来源,
/// 避免「预检说可用、执行时拦下」两套判定漂移。
#[tauri::command]
pub async fn rollback_precheck(
    server_id: String,
    password_plain: Option<String>,
    // 入口 A(04 页一键回滚):项目 id,归档目录由配置推导
    project_id: Option<String>,
    // 入口 B(06 回滚中心):项目目录直接给出 —— 该页是**目录驱动**的,
    // 项目不必在软件内配置(v6.11.1 补:首版只有 project_id,06 页传 dir 必然
    // 报「missing required key」,预检按钮恒失败)
    dir: Option<String>,
    release_ts: String,
) -> Result<RollbackPrecheck, String> {
    // 与执行链同款 ts 校验(防逃逸;预检也要挡住非法输入)
    if release_ts.contains('/')
        || release_ts.contains("..")
        || release_ts.contains('\\')
        || release_ts.trim().is_empty()
    {
        return Err(format!("发布标识不合法:{}", release_ts));
    }
    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &server_id)?.clone();
    // 目标目录:入口 B 的 dir 优先(与执行链 `_at` 同口径:绝对路径);
    // 入口 A 走「项目级 remote_dir → 服务器级 remote_dir」的有效目录推导
    let target_dir = match dir.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => {
            if !d.starts_with('/') {
                return Err(format!("项目目录必须是绝对路径:{}", d));
            }
            d.trim_end_matches('/').to_string()
        }
        None => {
            let pid = project_id
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| "缺少项目 id 或项目目录(二者至少给一个)".to_string())?;
            let project = find_project(&cfg, pid)?.clone();
            effective_remote_dir(&server, &project)
        }
    };
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

    let release_dir = releases_dir(&target_dir, &release_ts);
    let manifest_path = remote_join(&release_dir, "manifest.json");
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "读取发布清单超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &cat_file_cmd(&manifest_path)),
    )
    .await?;
    if code != 0 {
        // 归档不存在 / 无 manifest(旧版本或半成品)
        return Ok(RollbackPrecheck {
            items: Vec::new(),
            archived: 0,
            remote_by_id: 0,
            tag_restore: 0,
            missing: 0,
            unknown: 0,
            has_blocking: false,
            env_drift: Vec::new(),
            no_manifest: true,
        });
    }
    let m = parse_release_manifest(&out)
        .ok_or_else(|| "发布清单已损坏,无法预检".to_string())?;

    // 查询失败不再直接 Err(那会让预检整体不可用),而是按「不可得」语义
    // 逐项阻断并给「重试」指引(与执行链同口径)
    let (remote, remote_available) = match query_remote_images_full(&mut client).await {
        Ok(list) => (list, true),
        Err(e) => {
            log::warn!("预检:查询服务器镜像列表失败: {}", e);
            (Vec::new(), false)
        }
    };
    let remote_pairs: Vec<(String, String)> = remote
        .iter()
        .map(|i| (format!("{}:{}", i.repository, i.tag), i.id.clone()))
        .collect();
    // 目录实际文件清单(补丁审查):manifest 是部署当时的记录,目录内容可能
    // 事后被清理/手工删除 —— 必须核对,否则「说可用而 load 找不到包」
    let (code, ls_out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询发布目录超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &ls_dir_cmd(&release_dir)),
    )
    .await?;
    let actual_files = if code == 0 { parse_ls_lines(&ls_out) } else { Vec::new() };
    let plan = plan_rollback_full(&m.images, &remote_pairs, &actual_files, remote_available);
    let sum = rollback_precheck_summary(&plan);
    let items = plan
        .iter()
        .map(|p| RollbackPrecheckItem {
            service: p.service.clone(),
            tag: p.tag.clone(),
            source: p.source.as_str().to_string(),
            blocking: p.blocking,
            detail: p.detail.clone(),
        })
        .collect();

    // ---- 插值漂移预检(v6.12.0;与执行链同口径)----
    // 归档 compose 是部署当时的副本,但 `image: ${VAR}` 由服务器**当前** `.env`
    // 重新插值(.env 不入归档)。这里重算并与 manifest 的 tag 比对,凡漂移者在
    // 预检结果里列出(前端与「回不去」同款:未确认不得执行)。
    let mut env_drift: Vec<RollbackEnvDriftItem> = Vec::new();
    if actual_files.iter().any(|f| f == "docker-compose.yml") {
        let archived_compose = remote_join(&release_dir, "docker-compose.yml");
        if let Ok((0, compose_text)) =
            exec_collect(&mut client, &cat_file_cmd(&archived_compose)).await
        {
            // 归档内 override(目录枚举,与恢复顺序同源)
            let mut ov_texts: Vec<String> = Vec::new();
            for name in &actual_files {
                if archived_override_original(name).is_some() {
                    if let Ok((0, t)) =
                        exec_collect(&mut client, &cat_file_cmd(&remote_join(&release_dir, name))).await
                    {
                        ov_texts.push(t);
                    }
                }
            }
            // 部署目录当前 `.env`
            let env_table = match exec_collect(&mut client, &cat_file_cmd(&remote_join(&target_dir, ".env"))).await {
                Ok((0, t)) if !t.trim().is_empty() => parse_env_text(&t),
                _ => Default::default(),
            };
            match image_refs_with_env(&compose_text, &ov_texts, &env_table) {
                Ok(refs) => {
                    let pairs: Vec<(String, String)> = m
                        .images
                        .iter()
                        .map(|i| (i.service.clone(), i.tag.clone()))
                        .collect();
                    env_drift = detect_image_env_drift(&refs, &pairs)
                        .into_iter()
                        .map(|d| RollbackEnvDriftItem {
                            service: d.service,
                            expected: d.expected,
                            resolved: d.resolved,
                        })
                        .collect();
                }
                Err(e) => log::warn!("预检:插值漂移检查跳过({})", e),
            }
        }
    }

    Ok(RollbackPrecheck {
        items,
        archived: sum.archived,
        remote_by_id: sum.remote_by_id,
        tag_restore: sum.tag_restore,
        missing: sum.missing,
        unknown: sum.unknown,
        has_blocking: sum.has_blocking(),
        env_drift,
        no_manifest: false,
    })
}

/// 整栈一键回滚:把指定历史 release 的镜像包重新 `docker load`(自动恢复
/// 镜像原标签),恢复 compose 副本并 `compose up -d`。复用 deploy-log /
/// deploy-done 事件体系,`deploy-done` 恰好 emit 一次;成功后落一条
/// `mode = "rollback"` 的部署历史。
#[tauri::command]
pub async fn rollback_execute_stack(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    project_id: String,
    release_ts: String,
    // 预检发现阻断项时仍继续(用户已在预检结果里确认「部分回滚」);
    // false/缺省 = 有阻断项即中止并回传预检明细,由前端展示
    allow_partial: Option<bool>,
) -> Result<(), String> {
    // 远程操作互斥(第二十二批):回滚与部署/迁移互斥;被拒返回命令 Err
    // (前端 toast 原文),不进入管线
    let _guard = acquire_remote_op()?;
    let failed = rollback_failure_skeleton(&server_id, Some(&project_id));
    finish_rollback_with(
        &app,
        failed,
        rollback_execute_stack_inner(
            &app,
            &server_id,
            password_plain.as_deref(),
            &project_id,
            &release_ts,
            allow_partial.unwrap_or(false),
        ),
    )
    .await
}

/// [`rollback_execute_stack`] 的管线主体:成功返回组装好的部署历史记录
/// (由 [`finish_rollback`] 落历史),失败返回中文错误。
pub(crate) async fn rollback_execute_stack_inner(
    app: &AppHandle,
    server_id: &str,
    password_plain: Option<&str>,
    project_id: &str,
    release_ts: &str,
    allow_partial: bool,
) -> Result<DeployRecord, String> {
    // 部分回滚时未回退的服务名(顿号分隔);None = 全部服务都回退了
    let mut partial_note: Option<String> = None;
    let started = std::time::Instant::now();
    // 每次回滚开始时重置取消标志(与部署管线一致)
    reset_cancelled(app);

    // ts 校验(与 rollback_delete_release 同口径):防 `..`/路径分隔符逃逸出
    // <dir>/releases/(releases_dir/remote_join 不拦截 `..`,execute 路径此前缺此校验)
    if release_ts.contains('/')
        || release_ts.contains("..")
        || release_ts.contains('\\')
        || release_ts.trim().is_empty()
    {
        return Err(format!("发布标识不合法:{}", release_ts));
    }

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, server_id)?.clone();
    let project = find_project(&cfg, project_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain,
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut record = DeployRecord::new_skeleton(MODE_ROLLBACK, &server.name, &project.name, Vec::new());
    record.server_id = Some(server.id.clone());
    record.project_id = Some(project.id.clone());

    emit_log(
        app,
        &format!(
            "开始整栈回滚:服务器「{}」/ 项目「{}」,目标发布 {}",
            server.name, project.name, release_ts
        ),
    );

    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    let release_dir = releases_dir(&effective_remote_dir(&server, &project), release_ts);

    // ---- 校验发布目录存在 ----
    ensure_not_cancelled(app)?;
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验发布目录超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &test_dir_cmd(&release_dir)),
    )
    .await?;
    if code != 0 {
        return Err(format!("回滚目标发布目录不存在: {}", release_dir));
    }

    // ---- 列出发布目录内容(镜像包 + manifest.json + compose 副本)----
    ensure_not_cancelled(app)?;
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询发布目录超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &ls_dir_cmd(&release_dir)),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "查询发布目录内容失败(退出码 {}): {}",
            code, release_dir
        ));
    }
    let files = parse_ls_lines(&out);
    let packages: Vec<String> = files
        .iter()
        .filter(|f| f.ends_with(".tar.gz"))
        .cloned()
        .collect();
    let has_compose_copy = files.iter().any(|f| f == "docker-compose.yml");

    // ---- 读 manifest(存在才解析):校验留档归属并登记镜像引用,供回滚历史展示 ----
    let mut manifest: Option<ReleaseManifest> = None;
    if files.iter().any(|f| f == "manifest.json") {
        let manifest_path = remote_join(&release_dir, "manifest.json");
        let (code, out) = with_timeout(
            SSH_EXEC_TIMEOUT_SECS,
            "读取发布清单超时",
            "请检查服务器网络后重试",
            exec_collect(&mut client, &cat_file_cmd(&manifest_path)),
        )
        .await?;
        if code == 0 {
            if let Some(m) = parse_release_manifest(&out) {
                // 留档归属校验:同服务器多项目共用 remote_dir 时,防止误回滚
                // 对方项目的留档(manifest 在任何 docker load 之前读取,此处
                // 中止时远端状态未被改动)
                if m.project != project.name {
                    return Err(format!(
                        "留档 {} 属于项目「{}」,与当前项目「{}」不符,已中止",
                        release_ts, m.project, project.name
                    ));
                }
                record.images = m.images.iter().map(|i| i.tag.clone()).collect();
                manifest = Some(m);
            }
        } else {
            emit_log(app, "警告:读取发布清单失败,按无清单处理");
        }
    }

    // ---- 回滚可用性预检(R1;第二十九批)----
    // 为什么需要:智能传输会跳过未变化镜像的打包,归档里因此没有它们的包;
    // 而装载此前只按「目录里实际有哪些 tar.gz」—— 缺包服务被静默跳过,
    // `up -d` 用服务器当前镜像成功启动,界面却报「回滚完成」,服务实际没回退。
    // 这里按 manifest 逐服务核对来源,把「回不去的」显式暴露给用户。
    let plan = match &manifest {
        Some(m) => {
            let remote = match query_remote_images_full(&mut client).await {
                Ok(list) => list
                    .into_iter()
                    .map(|i| (format!("{}:{}", i.repository, i.tag), i.id))
                    .collect::<Vec<_>>(),
                Err(e) => {
                    // 查不到远端列表:不能据此判定「镜像丢失」(可能是查询失败),
                    // 但也无法放行 —— 保守起见按 Unknown 逐项阻断,并说明原因
                    emit_log(
                        app,
                        &format!("警告:查询服务器镜像列表失败({}),无法核对回滚可用性", e),
                    );
                    Vec::new()
                }
            };
            plan_rollback_with_files(&m.images, &remote, &files)
        }
        None => Vec::new(),
    };
    if !plan.is_empty() {
        let sum = rollback_precheck_summary(&plan);
        emit_log(
            app,
            &format!(
                "回滚可用性预检:归档内有包 {} 个 / 服务器上按 ID 命中 {} 个 / 标签待指回 {} 个 / 回不去 {} 个 / 无法核对 {} 个",
                sum.archived, sum.remote_by_id, sum.tag_restore, sum.missing, sum.unknown
            ),
        );
        for p in plan.iter().filter(|p| p.blocking) {
            emit_log(app, &format!("  [回不去] {}", p.detail));
        }
        for p in plan.iter().filter(|p| !p.blocking) {
            emit_log(app, &format!("  [可用] {}:{} — {}", p.service, p.tag, p.detail));
        }
        if sum.has_blocking() && !allow_partial {
            return Err(crate::errors::tagged(
                crate::errors::ErrCode::RollbackPrecheck,
                rollback_precheck_block_message(&plan, &sum),
            ));
        }
        if sum.has_blocking() {
            // 部分回滚必须留痕(v6.11.1 补):否则历史里看是「成功回滚」,
            // 实际混了未回退的服务 —— 事后复盘完全看不出,用户会以为已恢复。
            let names: Vec<String> = plan
                .iter()
                .filter(|p| p.blocking)
                .map(|p| p.service.clone())
                .collect();
            emit_log(
                app,
                &format!(
                    "用户已确认部分回滚:{} 沿用服务器现有镜像(未回退到本归档版本),其余服务正常回退",
                    names.join("、")
                ),
            );
            partial_note = Some(names.join("、"));
        }
    }

    // ---- 插值漂移预检(v6.12.0;在可用性预检之后、装载之前)----
    // `.env` 刻意不入归档(用户裁决,见 wiki/07),而 compose 的 `image: ${VAR}`
    // 由 **up -d 当时的** `.env` 插值:部署后若有人改过 `.env`,`up` 解析出的
    // 镜像引用会与 manifest 记录(部署当时的插值结果 —— 它就是插值产物)不同,
    // 「归档 compose + 新 .env」得到一个两边都不对的结果。这里用归档 compose
    // (含归档 override)+ 服务器当前 `.env` 重算,与 manifest 的 tag 逐服务比对。
    // 漂移 → **须确认**(与「回不去」同款:error 码相同,前端弹「仍要回滚」),
    // 确认后按实际解析结果执行并在历史里留痕。
    let mut env_drift: Vec<ImageEnvDrift> = Vec::new();
    if let Some(m) = &manifest {
        if has_compose_copy {
            let compose_path = remote_join(&release_dir, "docker-compose.yml");
            let (code, compose_text) = with_timeout(
                SSH_EXEC_TIMEOUT_SECS,
                "读取归档 compose 超时",
                "请检查服务器网络后重试",
                exec_collect(&mut client, &cat_file_cmd(&compose_path)),
            )
            .await?;
            if code == 0 {
                // 归档 override 也参与合并(与恢复顺序同源)
                let mut ov_texts: Vec<String> = Vec::new();
                for ov_name in compose_override_names(&project.compose_file) {
                    let archived = remote_join(&release_dir, &format!("{}.ddbak", ov_name));
                    if let Ok((0, t)) = exec_collect(&mut client, &cat_file_cmd(&archived)).await {
                        ov_texts.push(t);
                    }
                }
                // 服务器当前 `.env`(缺失 → 空表:与 compose 实际行为一致)
                let env_path = remote_join(&effective_remote_dir(&server, &project), ".env");
                let env_table = match exec_collect(&mut client, &cat_file_cmd(&env_path)).await {
                    Ok((0, t)) if !t.trim().is_empty() => parse_env_text(&t),
                    _ => Default::default(),
                };
                match image_refs_with_env(&compose_text, &ov_texts, &env_table) {
                    Ok(refs) => {
                        let pairs: Vec<(String, String)> = m
                            .images
                            .iter()
                            .map(|i| (i.service.clone(), i.tag.clone()))
                            .collect();
                        env_drift = detect_image_env_drift(&refs, &pairs);
                    }
                    Err(e) => emit_log(app, &format!("警告:插值漂移检查跳过({})", e)),
                }
            } else {
                emit_log(app, "警告:读取归档 compose 失败,插值漂移检查跳过");
            }
            for d in &env_drift {
                emit_log(
                    app,
                    &format!(
                        "  [插值漂移] {}:归档记录 {} ,但服务器当前 .env 会解析成 {}",
                        d.service, d.expected, d.resolved
                    ),
                );
            }
            if !env_drift.is_empty() && !allow_partial {
                return Err(crate::errors::tagged(
                    crate::errors::ErrCode::RollbackPrecheck,
                    rollback_env_drift_message(&env_drift),
                ));
            }
            if !env_drift.is_empty() {
                let names: Vec<String> =
                    env_drift.iter().map(|d| d.service.clone()).collect();
                emit_log(
                    app,
                    &format!(
                        "用户已确认插值漂移:{} 将按服务器当前 .env 解析出的引用启动(与归档记录可能不同)",
                        names.join("、")
                    ),
                );
                let note = format!("{} 存在 .env 插值漂移", names.join("、"));
                partial_note = Some(match partial_note.take() {
                    Some(prev) => format!("{};{}", prev, note),
                    None => note,
                });
            }
        }
    }

    // ---- 清单驱动选择装载包(v6.12.0:目录里多余的 tar 不再静默装载)----
    // 装载此前按目录 `ls` 全量走;目录里若有 manifest 未记录的包(人工放入 /
    // 半成品残留),其包内标签会静默覆盖服务器现状且界面无提示。修正为
    // 只装载清单记录的包,未记录的逐条告警。
    let selection = match &manifest {
        Some(m) => select_packages(&m.images, &files),
        None => {
            // 无清单(旧归档/半成品):保持现状行为(按目录内全部包恢复),
            // 但必须提示 —— 无法反向核对
            emit_log(
                app,
                "该归档无发布清单,无法核对目录内容:将按目录内全部镜像包恢复",
            );
            PackageSelection {
                to_load: packages.clone(),
                recorded_missing: Vec::new(),
                unrecorded: Vec::new(),
            }
        }
    };
    for name in &selection.unrecorded {
        emit_log(
            app,
            &format!(
                "警告:归档目录内 {} 未被发布清单记录,已跳过装载(防止其覆盖清单记录的标签)",
                name
            ),
        );
    }
    for name in &selection.recorded_missing {
        emit_log(
            app,
            &format!("警告:发布清单记录的镜像包 {} 不在归档目录里(预检应已阻断)", name),
        );
    }

    // ---- 逐包 docker load(load 自动恢复镜像原标签;按清单顺序)----
    let packages = selection.to_load;
    let n = packages.len();
    if n == 0 {
        emit_log(app, "发布目录内无可装载的镜像包,跳过 docker load");
    }
    for (i, name) in packages.iter().enumerate() {
        ensure_not_cancelled(app)?;
        let remote_tar = remote_join(&release_dir, name);
        emit_log(
            app,
            &format!(
                "回滚装载镜像包 ({}/{}): docker load -i {}",
                i + 1,
                n,
                remote_tar
            ),
        );
        let load_cmd = format!("docker load -i {}", shell_single_quote(&remote_tar));
        if let Err(e) = exec_forwarded(app, &mut client, &load_cmd, STACK_LOAD_TIMEOUT_SECS).await {
            // 部分失败提示:中止时点之前的包已装载成功(镜像标签已恢复),
            // 但容器尚未重建 —— 明确当前状态,避免误以为回滚未产生任何效果
            return Err(format!(
                "装载镜像包 {}/{} 失败:{};已装载 {}/{} 个镜像包,这些包的镜像标签已恢复,容器未重建(可排除问题后重新发起回滚)",
                i + 1,
                n,
                e,
                i,
                n
            ));
        }
    }

    // ---- 恢复 compose 副本(发布目录归档了本次部署使用的 compose 文件)----
    ensure_not_cancelled(app)?;
    let remote_compose = remote_compose_path(&effective_remote_dir(&server, &project));
    if has_compose_copy {
        let cp_cmd = format!(
            "cp {} {}",
            shell_single_quote(&remote_join(&release_dir, "docker-compose.yml")),
            shell_single_quote(&remote_compose)
        );
        emit_log(app, &format!("恢复 compose 文件: {}", cp_cmd));
        if let Err(e) = exec_forwarded(app, &mut client, &cp_cmd, SSH_EXEC_TIMEOUT_SECS).await {
            // 降级继续(与"无副本沿用现有 compose"同口径):base compose 仍在
            // 远端根目录(部署时已上传),副本恢复失败不阻断回滚语义
            emit_log(
                app,
                &format!(
                    "警告:恢复 compose 副本失败({}),沿用服务器现有 compose 文件继续回滚",
                    e
                ),
            );
        }
    } else {
        emit_log(app, "发布目录无 compose 副本,沿用服务器现有 compose 文件");
    }

    // override 文件名与单镜像回滚同口径:部署时 upload_compose_files 已把
    // override 上传到远端根目录,回滚按文件名直接引用,保证 -f 文件链与
    // 部署时 pull/up 一致(override-only 服务不逃逸)
    let override_names = compose_override_names(&project.compose_file);

    // ---- 恢复 override 副本(R2;第二十九批)----
    // 归档内的 override 以 `<原名>.ddbak` 存放(与根目录同名文件区分),
    // 恢复时按原名覆盖回部署目录 —— 保证回滚后的 compose 合并结果与
    // 部署当时一致(override 可改服务定义,旧 compose + 新 override 会得到
    // 一个两边都不对的合并结果)。旧归档无此文件 → 保持现状并注明。
    ensure_not_cancelled(app)?;
    let mut restored_overrides = 0usize;
    for ov_name in &override_names {
        let archived = remote_join(&release_dir, &format!("{}.ddbak", ov_name));
        // 存在性检查(旧归档没有;不存在时保持服务器现状)
        let (code, _) = exec_collect(&mut client, &test_file_cmd(&archived)).await?;
        if code != 0 {
            continue;
        }
        let cp_cmd = format!(
            "cp {} {}",
            shell_single_quote(&archived),
            shell_single_quote(&remote_join(&effective_remote_dir(&server, &project), ov_name))
        );
        emit_log(app, &format!("恢复 override 文件 {}: {}", ov_name, cp_cmd));
        if let Err(e) = exec_forwarded(app, &mut client, &cp_cmd, SSH_EXEC_TIMEOUT_SECS).await {
            emit_log(
                app,
                &format!("警告:恢复 override 文件 {} 失败({}),该文件保持现状", ov_name, e),
            );
        } else {
            restored_overrides += 1;
        }
    }
    if restored_overrides == 0 && !override_names.is_empty() {
        emit_log(
            app,
            "发布目录无 override 副本(旧归档);override 文件保持服务器现状",
        );
    }

    // ---- 按 ID 收敛(v6.12.0;up 前最后一道复核)----
    // 预检是**查询那一刻**的快照,到 up 之间隔着逐包 load;期间外部主体
    // (另一台机器的本应用 / CI / watchtower)移动标签会让结论失效(TOCTOU)。
    // 这里按 manifest 记录的 ID 复核一次:ID 还在但标签没指向它 → `docker tag`
    // 零拷贝指回(真机案例 goodlaser-backend:latest);ID 不在 → 重算预检并
    // 按同一口径处置(未确认过就阻断)。
    if let Some(m) = &manifest {
        let remote = match query_remote_images_full(&mut client).await {
            Ok(list) => list
                .into_iter()
                .map(|i| (format!("{}:{}", i.repository, i.tag), i.id))
                .collect::<Vec<_>>(),
            Err(e) => {
                emit_log(
                    app,
                    &format!("警告:up 前复核查询镜像列表失败({});按预检结论继续", e),
                );
                Vec::new()
            }
        };
        let convergence = plan_tag_convergence(&m.images, &remote);
        for (svc, id, tag) in &convergence.retag {
            emit_log(
                app,
                &format!(
                    "{}:标签未指向归档版本,自动指回(docker tag {} {} )",
                    svc,
                    short_id(id),
                    tag
                ),
            );
            let cmd = docker_tag_cmd(id, tag);
            if let Err(e) = exec_forwarded(app, &mut client, &cmd, SSH_EXEC_TIMEOUT_SECS).await {
                return Err(format!("自动指回标签失败({}):{}", tag, e));
            }
        }
        if !convergence.missing.is_empty() {
            // up 前的复核发现预检后 ID 被删:预检结论已失效,必须重走确认
            let details = convergence
                .missing
                .iter()
                .map(|(svc, tag)| format!("· {}:期望镜像已不在服务器上({})", svc, tag))
                .collect::<Vec<_>>()
                .join("
");
            if !allow_partial {
                return Err(crate::errors::tagged(
                    crate::errors::ErrCode::RollbackPrecheck,
                    format!(
                        "up 前复核发现预检结论已失效(可能有其它主体在操作服务器):\n{}\n如仍要回滚,这些服务将沿用服务器当前镜像。选择「仍要回滚」继续。",
                        details
                    ),
                ));
            }
            emit_log(
                app,
                &format!("up 前复核:以下服务镜像已不在服务器,将沿用当前镜像:{}", details),
            );
        }
    }

    // ---- compose up -d(镜像标签已恢复,up 按引用重建容器)----
    ensure_not_cancelled(app)?;
    let up_cmd = compose_up_cmd(&effective_remote_dir(&server, &project), &remote_compose, &override_names);
    emit_log(app, &format!("启动服务: {}", up_cmd));
    exec_forwarded(app, &mut client, &up_cmd, STACK_COMPOSE_TIMEOUT_SECS).await?;

    // ---- up 后运行镜像校验(v6.12.0;兜住所有残余路径)----
    // up 前的一切都是「准备动作」的正确性;实际结果还受 .env 漂移、compose 副本
    // 恢复失败、compose 认为无变化未重建容器等影响。这里按容器实况兜底:
    // 期望 = manifest 记录的 ID,实际 = 容器 Image。
    if let Some(m) = &manifest {
        // 与 up 命令逐字同源的 prefix(04 页路径 up 用 -f 链)
        let prefix = format!(
            "cd {} && docker compose {}",
            shell_single_quote(&effective_remote_dir(&server, &project)),
            compose_file_flags(&remote_compose, &override_names)
        );
        let expected = expected_images_from_manifest(&m.images);
        let report = verify_running_images(app, &mut client, &prefix, &expected).await;
        if let Err(e) = report {
            emit_log(app, &format!("警告:up 后运行镜像校验未能完成({})", e));
        }
    }

    emit_log(app, "整栈回滚完成");
    record.success = true;
    record.message = match partial_note {
        // 部分回滚/插值漂移要在历史/通知里写明,不能只说「回滚到 X」
        Some(ref names) => format!("回滚到 {}({})", release_ts, names),
        None => format!("回滚到 {}", release_ts),
    };
    record.duration_secs = started.elapsed().as_secs();
    Ok(record)
}

/// 单镜像一键回滚:把服务器的 `repository:date_tag`(历史标签)重新指到
/// `target_ref`(compose 引用的标签,如 myapp:latest),再 `compose up -d`。
/// 复用 deploy-log / deploy-done 事件体系,`deploy-done` 恰好 emit 一次;
/// 成功后落一条 `mode = "rollback"` 的部署历史。
#[tauri::command]
pub async fn rollback_execute_single(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    project_id: String,
    repository: String,
    date_tag: String,
    target_ref: String,
) -> Result<(), String> {
    // 远程操作互斥(第二十二批):与部署/迁移互斥
    let _guard = acquire_remote_op()?;
    let failed = rollback_failure_skeleton(&server_id, Some(&project_id));
    finish_rollback_with(
        &app,
        failed,
        rollback_execute_single_inner(
            &app,
            &server_id,
            password_plain.as_deref(),
            &project_id,
            &repository,
            &date_tag,
            &target_ref,
        ),
    )
    .await
}

/// [`rollback_execute_single`] 的管线主体:成功返回组装好的部署历史记录,
/// 失败返回中文错误。
async fn rollback_execute_single_inner(
    app: &AppHandle,
    server_id: &str,
    password_plain: Option<&str>,
    project_id: &str,
    repository: &str,
    date_tag: &str,
    target_ref: &str,
) -> Result<DeployRecord, String> {
    let started = std::time::Instant::now();
    reset_cancelled(app);

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, server_id)?.clone();
    let project = find_project(&cfg, project_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain,
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let mut record = DeployRecord::new_skeleton(
        MODE_ROLLBACK,
        &server.name,
        &project.name,
        vec![target_ref.to_string()],
    );
    record.server_id = Some(server.id.clone());
    record.project_id = Some(project.id.clone());

    let source = format!("{}:{}", repository, date_tag);
    emit_log(
        app,
        &format!(
            "开始镜像回滚:服务器「{}」/ 项目「{}」:{} -> {}",
            server.name, project.name, source, target_ref
        ),
    );

    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    // ---- 校验历史标签存在(不存在 → 明确报错,不盲目 docker tag)----
    ensure_not_cancelled(app)?;
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验镜像超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &docker_inspect_cmd(&source)),
    )
    .await?;
    if code != 0 {
        return Err(format!("服务器上不存在镜像 {},无法回滚到该标签", source));
    }

    // ---- 远端 compose 文件检查(与单镜像部署 prepare_single_compose 同口径)----
    // 导入项目:部署时上传到远端根目录的副本;旧版手工项目:compose_file 即远端路径
    let (compose_path, overrides) = if Path::new(&project.compose_file).is_file() {
        (
            remote_compose_path(&effective_remote_dir(&server, &project)),
            compose_override_names(&project.compose_file),
        )
    } else if is_windows_absolute_path(&project.compose_file) {
        return Err(format!(
            "本地 compose 文件不存在:{};请确认路径或重新导入 compose 文件",
            project.compose_file
        ));
    } else {
        (project.compose_file.clone(), Vec::new())
    };
    ensure_not_cancelled(app)?;
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验 compose 文件超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &test_file_cmd(&compose_path)),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "远端 compose 文件不存在:{},请先完成一次部署",
            compose_path
        ));
    }

    // ---- docker tag:把 compose 引用的标签重新指回历史镜像(零拷贝)----
    ensure_not_cancelled(app)?;
    let tag_cmd = docker_tag_cmd(&source, target_ref);
    emit_log(app, &format!("回滚标签: {}", tag_cmd));
    exec_forwarded(app, &mut client, &tag_cmd, SSH_EXEC_TIMEOUT_SECS).await?;

    // ---- compose up -d ----
    ensure_not_cancelled(app)?;
    let up_cmd = compose_up_cmd(&effective_remote_dir(&server, &project), &compose_path, &overrides);
    emit_log(app, &format!("启动服务: {}", up_cmd));
    exec_forwarded(app, &mut client, &up_cmd, STACK_COMPOSE_TIMEOUT_SECS).await?;

    emit_log(app, "镜像回滚完成");
    record.success = true;
    record.message = format!("回滚到 {}", source);
    record.duration_secs = started.elapsed().as_secs();
    Ok(record)
}

// ===== 独立回滚模块(第三批:按服务器真实项目分列)=====
//
// 现状 `rollback_*` 全部以应用内 `project_id` 为入口,释出目录固定取
// `server.remote_dir/releases`;服务器上真实存在的项目(尤其不是本应用部署的)
// 无法被看到或回滚。本组命令按**目录**工作:先扫描服务器真实项目,
// 再对指定目录列出发布归档/日期标签并执行回滚。

/// 服务器上的一个真实项目(回滚模块列表项)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackProject {
    /// 项目目录(绝对路径)
    pub dir: String,
    /// compose 文件路径(未扫到为空)
    pub compose_file: String,
    /// 该项目的发布归档目录(完整路径,新→旧)
    pub releases: Vec<String>,
    /// 归档数量
    pub release_count: usize,
    /// 最近的归档时间戳(无归档为空)
    pub latest_release: String,
    /// 运行中的容器数(按 compose project label 归属;取不到为 0)
    pub running_containers: usize,
    /// 匹配到的应用内项目名(仅标注;空串 = 服务器上存在但软件内未配置)
    pub app_project: String,
}

/// 发布归档明细(回滚选择项)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackReleaseDetail {
    pub ts: String,
    pub dir: String,
    /// 归档内的镜像包文件名
    pub packages: Vec<String>,
    /// manifest 记录的服务名(无清单为空)
    pub services: Vec<String>,
    /// manifest 记录的逐服务镜像条目(第七批:详情模态展示;无清单为空)
    pub manifest_images: Vec<ManifestImage>,
    pub has_manifest: bool,
    pub has_compose_copy: bool,
    /// 版本说明标题(归档内 release-notes.json;未设置/解析失败为 None)
    pub note_title: Option<String>,
    /// 版本说明正文(同上)
    pub note_body: Option<String>,
    /// 版本说明最近保存时间(RFC3339;同上)
    pub note_updated_at: Option<String>,
}

/// 日期标签镜像明细(单镜像回滚选择项;复用 [`TagBrief`] 字段口径)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackTagDetail {
    pub repository: String,
    pub tags: Vec<TagBrief>,
}

/// 项目明细:发布归档 + 各仓库的日期标签(供回滚面板两级选择)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackProjectDetail {
    pub dir: String,
    pub compose_file: String,
    pub releases: Vec<RollbackReleaseDetail>,
    pub repositories: Vec<RollbackTagDetail>,
}

/// 扫描服务器上的真实项目(回滚模块入口)。
///
/// 项目来源 = 文件系统扫描(含 compose 文件的目录)+ `docker ps` 的
/// compose labels 合并去重;以服务器真实目录为准,应用内项目仅作标注。
/// `scan_root` 缺省用服务器配置的 `remote_dir`。
#[tauri::command]
pub async fn rollback_scan_projects(
    server_id: String,
    password_plain: Option<String>,
    scan_root: Option<String>,
) -> Result<Vec<RollbackProject>, String> {
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

    let root = scan_root
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| server.remote_dir.clone());

    // 第七批二次优化:单次往返 —— 组合命令一次带回 compose 文件清单、
    // releases 归档清单与 `docker ps -a`(Labels 直读 compose working_dir,
    // 替代此前「docker ps 取 ID → 逐容器 inspect」的两次额外往返)
    let (_, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "扫描项目超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &rollback_scan_cmd(&root)),
    )
    .await?;
    let scan = parse_rollback_scan(&out);
    if !scan.root_exists {
        return Err(format!(
            "扫描目录「{}」失败(目录可能不存在或无权限)",
            root
        ));
    }

    // 项目目录 = compose 文件父目录 + docker Labels 的 working_dir 合并去重
    let mut dirs: Vec<String> = Vec::new();
    for p in &scan.compose_paths {
        if let Some((parent, _)) = p.rsplit_once('/') {
            if !parent.is_empty() && !dirs.iter().any(|d| d == parent) {
                dirs.push(parent.to_string());
            }
        }
    }
    for wd in compose_working_dirs(&scan.ps_items) {
        if !dirs.iter().any(|d| d == &wd) {
            dirs.push(wd);
        }
    }
    dirs.sort();

    let mut projects = Vec::new();
    for dir in dirs {
        let compose_file = scan
            .compose_paths
            .iter()
            .find(|p| p.rsplit_once('/').map(|(d, _)| d) == Some(dir.as_str()))
            .cloned()
            .unwrap_or_default();
        // 归档列表(新→旧):远端 mtime 倒序直接沿用 —— 不得按路径名排序,
        // 归档名放宽后字典序 ≠ 时间序(A2),回滚选「最近一版」会选错
        let releases: Vec<String> = releases_of_project(&scan.release_paths, &dir);
        let latest = releases
            .first()
            .and_then(|r| r.rsplit('/').next().map(String::from))
            .unwrap_or_default();
        let dir_name = dir.rsplit('/').next().unwrap_or("");
        let app_project = cfg
            .projects
            .iter()
            .find(|p| {
                p.name == dir_name
                    || p.compose_file
                        .rsplit_once('/')
                        .map(|(d, _)| d == dir)
                        .unwrap_or(false)
            })
            .map(|p| p.name.clone())
            .unwrap_or_default();
        // 运行容器数:按 compose working_dir 标签精确归属(第二十七批 A1;
        // 此前按容器名包含目录名近似,同前缀项目会互相串数)
        let running = running_container_count(&scan.ps_items, &dir);
        projects.push(RollbackProject {
            dir,
            compose_file,
            release_count: releases.len(),
            releases,
            latest_release: latest,
            running_containers: running,
            app_project,
        });
    }
    Ok(projects)
}

/// 列出某项目目录下的发布归档与日期标签(回滚面板明细)。
#[tauri::command]
pub async fn rollback_project_detail(
    server_id: String,
    password_plain: Option<String>,
    dir: String,
) -> Result<RollbackProjectDetail, String> {
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

    let dir = dir.trim().trim_end_matches('/').to_string();
    let releases_root = remote_join(&dir, "releases");
    // ① 单次往返批量读取(第七批二次优化):归档枚举由远端 find 循环完成 ——
    // 命令长度恒定,连"先 ls 再拼装"的往返都省掉;manifest / 版本说明 /
    // compose 候选 / 全量镜像列表一条命令全部带回,整次明细仅 1 次 SSH 往返
    let compose_candidates = [
        remote_join(&dir, "docker-compose.yml"),
        remote_join(&dir, "compose.yml"),
    ];
    let scan_cmd = releases_scan_cmd(&dir, 50, &compose_candidates, true);
    let (_, dump_out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "读取归档详情超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &scan_cmd),
    )
    .await
    .unwrap_or((1, String::new()));
    let dump = parse_releases_dump(&dump_out, &compose_candidates);

    let mut releases: Vec<RollbackReleaseDetail> = Vec::new();
    for entry in &dump.entries {
        if entry.files.is_empty() {
            // 文件清单为空 = 归档目录刚被清理或不可读:跳过,不拖垮明细(与既有口径一致)
            continue;
        }
        let packages: Vec<String> = entry
            .files
            .iter()
            .filter(|f| f.ends_with(".tar.gz"))
            .cloned()
            .collect();
        let has_manifest = entry.files.iter().any(|f| f == "manifest.json");
        let has_compose_copy = entry.files.iter().any(|f| f == "docker-compose.yml");
        let (services, manifest_images) = match parse_release_manifest(&entry.manifest) {
            Some(m) => (
                m.images.iter().map(|img| img.service.clone()).collect(),
                m.images,
            ),
            None => (Vec::new(), Vec::new()),
        };
        let (note_title, note_body, note_updated_at) = match parse_release_notes(&entry.notes) {
            Some(n) => (Some(n.title), Some(n.body), Some(n.updated_at)),
            None => (None, None, None),
        };
        releases.push(RollbackReleaseDetail {
            ts: entry.ts.clone(),
            dir: remote_join(&releases_root, &entry.ts),
            packages,
            services,
            manifest_images,
            has_manifest,
            has_compose_copy,
            note_title,
            note_body,
            note_updated_at,
        });
    }

    // ② 日期标签:compose 候选解析仓库名;镜像列表已在 ① 带回,本地按仓库过滤(零往返)
    let mut repositories: Vec<RollbackTagDetail> = Vec::new();
    let mut compose_file = String::new();
    let image_items = parse_cleanup_ndjson(&dump.images).0;
    for (i, cand) in compose_candidates.iter().enumerate() {
        let repos = compose_image_repos(&dump.compose_texts[i]);
        if repos.is_empty() {
            continue;
        }
        if compose_file.is_empty() {
            // 修复:此前恒返回第一个候选名;现记录实际产出仓库的候选
            compose_file = cand.clone();
        }
        for repo in repos {
            if repo.trim().is_empty() || repositories.iter().any(|r| r.repository == repo) {
                continue;
            }
            let mut tags: Vec<TagBrief> = image_items
                .iter()
                .filter(|v| jstr(v, "Repository") == repo && is_date_tag(&jstr(v, "Tag")))
                .map(|v| TagBrief {
                    tag: jstr(v, "Tag"),
                    id: jstr(v, "ID"),
                    created: jstr(v, "CreatedAt"),
                })
                .collect();
            tags.sort_by(|a, b| b.tag.cmp(&a.tag));
            if !tags.is_empty() {
                repositories.push(RollbackTagDetail { repository: repo, tags });
            }
        }
        break; // 与既有口径一致:第一个产出仓库的候选生效
    }
    if compose_file.is_empty() {
        // compose 可读但未声明任何 image(或两个候选都读不到):回退到可读到
        // 文本的候选,保持"能识别到 compose 文件"这一信息不丢失
        if let Some(i) = dump
            .compose_texts
            .iter()
            .position(|t| !t.trim().is_empty())
        {
            compose_file = compose_candidates[i].clone();
        }
    }

    Ok(RollbackProjectDetail {
        dir,
        compose_file,
        releases,
        repositories,
    })
}

/// 删除指定的发布归档目录(回滚中心「删除」按钮;二次确认由前端负责)。
///
/// 安全约束(防误删/防注入):
/// - `dir` 必须是绝对路径;
/// - `ts` 必须是纯目录名(不含 `/` 与 `..`),与 `dir` 拼成
/// `<dir>/releases/<ts>` 后**校验路径前缀**,越出该项目 releases 目录即拒;
/// - 只执行 `rm -rf` 这一个由后端拼装、单引号包裹的路径。
#[tauri::command]
pub async fn rollback_delete_release(
    server_id: String,
    password_plain: Option<String>,
    dir: String,
    release_ts: String,
) -> Result<(), String> {
    let dir = dir.trim().trim_end_matches('/').to_string();
    if !dir.starts_with('/') {
        return Err(format!("项目目录必须是绝对路径:{}", dir));
    }
    let ts = release_ts.trim().to_string();
    if ts.is_empty() || ts.contains('/') || ts.contains("..") || ts.contains('\\') {
        return Err(format!("发布标识不合法:{}", release_ts));
    }
    let release_dir = remote_join(&dir, &format!("releases/{}", ts));
    // 前缀校验:必须严格位于 <dir>/releases/ 之下(防 ts 注入逃逸)
    if !release_dir.starts_with(&format!("{}/releases/", dir)) {
        return Err(format!("目标越出项目 releases 目录:{}", release_dir));
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

    // 存在性校验:不存在则明确报错(而非静默成功)
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验归档超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &test_dir_cmd(&release_dir)),
    )
    .await?;
    if code != 0 {
        return Err(format!("归档目录不存在:{}", release_dir));
    }

    let cmd = format!("rm -rf {}", shell_single_quote(&release_dir));
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "删除归档超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &cmd),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "删除归档失败(退出码 {}): {}",
            code,
            out.trim()
        ));
    }
    Ok(())
}

/// 删除指定的日期标签镜像(回滚中心「删除」按钮;二次确认由前端负责)。
///
/// 安全约束:`reference` 形如 `repo:YYYYmmdd-HHMMSS`,仅校验非空且不含
/// shell 元字符风险字符(实际经 [`shell_single_quote`] 包裹);删除前先
/// `docker image inspect` 校验存在,避免 `docker rmi` 的模糊匹配误删。
#[tauri::command]
pub async fn rollback_delete_tag(
    server_id: String,
    password_plain: Option<String>,
    reference: String,
) -> Result<(), String> {
    let reference = reference.trim().to_string();
    if reference.is_empty() || reference.contains(char::is_whitespace) {
        return Err(format!("镜像引用不合法:{}", reference));
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

    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验镜像超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &docker_inspect_cmd(&reference)),
    )
    .await?;
    if code != 0 {
        return Err(format!("服务器上不存在镜像 {}", reference));
    }

    // 不使用 -f:若仍被容器引用则命令失败并回传原因(前端二次确认已提示),
    // 比强制删除更安全 —— 与「清理分析」跳过在用镜像的口径一致。
    let cmd = format!("docker rmi {}", shell_single_quote(&reference));
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "删除镜像超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &cmd),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "删除镜像失败(退出码 {}): {}",
            code,
            out.trim()
        ));
    }
    Ok(())
}

// ===== 归档版本说明(第七批;类 GitHub Release 的标题 + 更新描述)=====
//
// 说明持久化在归档目录内的 `release-notes.json`(与归档同生共死、跨机器可用),
// 读取并入 [`releases_dump_cmd`] 批量 cat(零额外往返);写入复用 manage_stacks
// .env 的「base64 → tmp($$ PID 后缀)→ mv」原子写先例。

/// 单条版本说明的内容上限(UTF-8 字节数)。与 .env 编辑的量级对齐——远小于
/// exec 命令行安全长度,base64 膨胀 4/3 后仍充裕。
const RELEASE_NOTES_MAX_BYTES: usize = 64 * 1024;

/// 归档的版本说明(归档目录内 `release-notes.json`;camelCase 契约)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseNotes {
    /// 版本标题(类似 GitHub Release 的 tag 名,如「v1.2.0 修复登录问题」)
    pub title: String,
    /// 更新说明正文(多行)
    pub body: String,
    /// 最近一次保存时间(RFC3339 UTC)
    pub updated_at: String,
}

/// 解析 release-notes.json 文本(纯函数,便于单测):损坏/缺字段 → `None`
/// (调用方按"未设置说明"降级,不影响详情展示)。
pub(crate) fn parse_release_notes(json: &str) -> Option<ReleaseNotes> {
    serde_json::from_str(json.trim()).ok()
}

/// 拼 release-notes.json 的原子写入命令(纯函数,便于单测)。
///
/// 形态与 manage_stacks 的 `env_write_cmd` 同源:tmp 名 = 引号包裹的路径 +
/// `.ddtmp.` + 引号外的 `$$`(PID 展开为纯数字,防并发交错;引号内 `$$`
/// 不展开,故必须留在外侧),`mv` 同文件系统 rename 原子,中断不损坏旧文件。
pub(crate) fn release_notes_write_cmd(notes_path: &str, b64: &str) -> String {
    let tmp = format!("{}'.ddtmp.'", shell_single_quote(notes_path));
    format!(
        "echo {} | base64 -d > {}$$ && mv {}$$ {}",
        b64,
        tmp,
        tmp,
        shell_single_quote(notes_path)
    )
}

/// [`rollback_set_release_notes`] 入参(camelCase 契约)。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseNotesRequest {
    pub server_id: String,
    pub password_plain: Option<String>,
    /// 项目目录(绝对路径)
    pub dir: String,
    /// 归档时间戳(纯目录名)
    pub ts: String,
    pub title: String,
    pub body: String,
}

/// [`rollback_set_release_notes`] 返回:保存/清除后的最近更新时间。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseNotesSaved {
    pub updated_at: String,
}

/// 保存归档的版本说明(回滚中心「版本详情」表单)。
///
/// 写入 `<dir>/releases/<ts>/release-notes.json`;**title 与 body 均为空时删除
/// 该文件**(保持归档目录干净,详情回退为「未设置」)。
/// 安全约束与 [`rollback_delete_release`] 同款:dir 绝对路径、ts 纯目录名、
/// 拼接后前缀校验防逃逸;内容经 base64 编码(字符集无 shell 元字符)。
#[tauri::command]
pub async fn rollback_set_release_notes(req: ReleaseNotesRequest) -> Result<ReleaseNotesSaved, String> {
    let dir = req.dir.trim().trim_end_matches('/').to_string();
    if !dir.starts_with('/') {
        return Err(format!("项目目录必须是绝对路径:{}", req.dir));
    }
    let ts = req.ts.trim().to_string();
    if ts.is_empty() || ts.contains('/') || ts.contains("..") || ts.contains('\\') {
        return Err(format!("发布标识不合法:{}", req.ts));
    }
    let release_dir = remote_join(&dir, &format!("releases/{}", ts));
    if !release_dir.starts_with(&format!("{}/releases/", dir)) {
        return Err(format!("目标越出项目 releases 目录:{}", release_dir));
    }
    let title = req.title.trim().to_string();
    let body = req.body.trim().to_string();
    if title.len() + body.len() > RELEASE_NOTES_MAX_BYTES {
        return Err(format!(
            "版本说明过长(上限 {} KB)",
            RELEASE_NOTES_MAX_BYTES / 1024
        ));
    }

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, &req.server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        req.password_plain.as_deref(),
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

    let notes_path = remote_join(&release_dir, "release-notes.json");
    let updated_at = chrono::Utc::now().to_rfc3339();
    let cmd = if title.is_empty() && body.is_empty() {
        // 两字段均空 = 清除说明:删除文件,详情回退为「未设置」
        format!("rm -f {}", shell_single_quote(&notes_path))
    } else {
        let notes = ReleaseNotes {
            title,
            body,
            updated_at: updated_at.clone(),
        };
        let json = serde_json::to_string(&notes).map_err(|e| format!("序列化版本说明失败: {}", e))?;
        release_notes_write_cmd(&notes_path, &BASE64_STANDARD.encode(json.as_bytes()))
    };
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "保存版本说明超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &cmd),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "保存版本说明失败(退出码 {}): {}",
            code,
            out.trim()
        ));
    }
    Ok(ReleaseNotesSaved { updated_at })
}

/// 按**服务器项目目录**执行整栈回滚(独立回滚模块入口)。
///
/// 与 [`rollback_execute_stack`] 的差异:不依赖应用内项目配置 —— 释出目录
/// 直接取自传入的项目目录,compose 恢复目标为 `<dir>/docker-compose.yml`。
/// 归属校验改为"归档目录必须位于该项目目录下"(路径前缀比对),避免跨项目误回滚。
/// 复用 deploy-log / deploy-done 事件体系与部署历史记录。
#[tauri::command]
pub async fn rollback_execute_stack_at(
    app: AppHandle,
    server_id: String,
    password_plain: Option<String>,
    dir: String,
    release_ts: String,
    // 预检阻断时仍继续(用户已确认部分回滚);缺省 = 有阻断即中止
    allow_partial: Option<bool>,
) -> Result<(), String> {
    // 远程操作互斥(第二十二批):与部署/迁移互斥
    let _guard = acquire_remote_op()?;
    // 06 页路径无 projectId:骨架里项目名留空(留痕仍有服务器与目标归档)
    let failed = rollback_failure_skeleton(&server_id, None);
    finish_rollback_with(
        &app,
        failed,
        rollback_execute_stack_at_inner(
            &app,
            &server_id,
            password_plain.as_deref(),
            &dir,
            &release_ts,
            allow_partial.unwrap_or(false),
        ),
    )
    .await
}

/// [`rollback_execute_stack_at`] 的管线主体。
async fn rollback_execute_stack_at_inner(
    app: &AppHandle,
    server_id: &str,
    password_plain: Option<&str>,
    dir: &str,
    release_ts: &str,
    // 预检阻断时仍继续(用户已确认部分回滚);见 rollback_execute_stack 同参数
    allow_partial: bool,
) -> Result<DeployRecord, String> {
    // 部分回滚时未回退的服务名(顿号分隔);None = 全部服务都回退了
    let mut partial_note: Option<String> = None;
    let started = std::time::Instant::now();
    reset_cancelled(app);

    // ts 校验(与 rollback_delete_release 同口径):防 `..`/路径分隔符逃逸出
    // <dir>/releases/(releases_dir/remote_join 不拦截 `..`,execute 路径此前缺此校验)
    if release_ts.contains('/')
        || release_ts.contains("..")
        || release_ts.contains('\\')
        || release_ts.trim().is_empty()
    {
        return Err(format!("发布标识不合法:{}", release_ts));
    }

    let cfg = load_config().map_err(|e| format!("读取配置失败: {}", e))?;
    let server = find_server(&cfg, server_id)?.clone();
    let password = resolve_password(
        &server.auth.auth_type,
        password_plain,
        server.auth.password_enc.as_deref(),
    )?;
    let key_pass = resolve_key_passphrase(&server)?;
    let dir = dir.trim().trim_end_matches('/').to_string();
    if !dir.starts_with('/') {
        return Err(format!("项目目录必须是绝对路径:{}", dir));
    }
    let project_name = dir.rsplit('/').next().unwrap_or(&dir).to_string();
    let mut record =
        DeployRecord::new_skeleton(MODE_ROLLBACK, &server.name, &project_name, Vec::new());

    emit_log(
        app,
        &format!(
            "开始整栈回滚:服务器「{}」/ 项目目录 {} ,目标发布 {}",
            server.name, dir, release_ts
        ),
    );

    let mut client = with_timeout(
        SSH_CONNECT_TIMEOUT_SECS,
        "连接超时",
        "请检查服务器地址与网络",
        SshClient::connect(&server, password.as_deref(), key_pass.as_deref(), Arc::default()),
    )
    .await?;

    // 归档目录 = <dir>/releases/<ts>;归属校验靠"必须位于该项目目录下"
    let release_dir = remote_join(&dir, &format!("releases/{}", release_ts));
    if !release_dir.starts_with(&format!("{}/releases/", dir)) {
        return Err(format!("回滚目标越出项目目录:{}", release_dir));
    }

    ensure_not_cancelled(app)?;
    let (code, _) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "校验发布目录超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &test_dir_cmd(&release_dir)),
    )
    .await?;
    if code != 0 {
        return Err(format!("回滚目标发布目录不存在: {}", release_dir));
    }

    ensure_not_cancelled(app)?;
    let (code, out) = with_timeout(
        SSH_EXEC_TIMEOUT_SECS,
        "查询发布目录超时",
        "请检查服务器网络后重试",
        exec_collect(&mut client, &ls_dir_cmd(&release_dir)),
    )
    .await?;
    if code != 0 {
        return Err(format!("查询发布目录内容失败(退出码 {}): {}", code, release_dir));
    }
    let files = parse_ls_lines(&out);
    let packages: Vec<String> = files.iter().filter(|f| f.ends_with(".tar.gz")).cloned().collect();
    let has_compose_copy = files.iter().any(|f| f == "docker-compose.yml");

    // manifest:历史展示(归属由目录前缀保证,不以项目名卡)
    let mut manifest: Option<ReleaseManifest> = None;
    if files.iter().any(|f| f == "manifest.json") {
        let mp = remote_join(&release_dir, "manifest.json");
        if let Ok((0, m_out)) = exec_collect(&mut client, &cat_file_cmd(&mp)).await {
            if let Some(m) = parse_release_manifest(&m_out) {
                record.images = m.images.iter().map(|i| i.tag.clone()).collect();
                manifest = Some(m);
            }
        }
    }

    // ---- 回滚可用性预检(R1;第二十九批;06 回滚中心路径)----
    // 与 04 页路径**共用同一纯函数**(单一事实来源)。06 页是配置漂移后的兜底
    // 入口(项目改名/被删时 04 页会中止),它的预检同样必要 —— 「回不去」的
    // 服务在这里也必须显式暴露,不能静默沿用当前镜像。
    // (v6.11.1 补:`allow_partial` 此前只加进了签名、预检段漏注入 → 该参数
    //  形同虚设,阻断项会被直接忽略。)
    let plan = match &manifest {
        Some(m) => {
            let (remote, remote_available) = match query_remote_images_full(&mut client).await {
                Ok(list) => (
                    list.into_iter()
                        .map(|i| (format!("{}:{}", i.repository, i.tag), i.id))
                        .collect::<Vec<_>>(),
                    true,
                ),
                Err(e) => {
                    emit_log(
                        app,
                        &format!("警告:查询服务器镜像列表失败({}),无法核对回滚可用性", e),
                    );
                    (Vec::new(), false)
                }
            };
            plan_rollback_full(&m.images, &remote, &files, remote_available)
        }
        None => Vec::new(),
    };
    if !plan.is_empty() {
        let sum = rollback_precheck_summary(&plan);
        emit_log(
            app,
            &format!(
                "回滚可用性预检:归档内有包 {} 个 / 服务器上按 ID 命中 {} 个 / 标签待指回 {} 个 / 回不去 {} 个 / 无法核对 {} 个",
                sum.archived, sum.remote_by_id, sum.tag_restore, sum.missing, sum.unknown
            ),
        );
        for p in plan.iter().filter(|p| p.blocking) {
            emit_log(app, &format!("  [回不去] {}", p.detail));
        }
        for p in plan.iter().filter(|p| !p.blocking) {
            emit_log(app, &format!("  [可用] {}:{} — {}", p.service, p.tag, p.detail));
        }
        if sum.has_blocking() && !allow_partial {
            return Err(crate::errors::tagged(
                crate::errors::ErrCode::RollbackPrecheck,
                rollback_precheck_block_message(&plan, &sum),
            ));
        }
        if sum.has_blocking() {
            // 部分回滚必须留痕(v6.11.1 补):否则历史里看是「成功回滚」,
            // 实际混了未回退的服务 —— 事后复盘完全看不出,用户会以为已恢复。
            let names: Vec<String> = plan
                .iter()
                .filter(|p| p.blocking)
                .map(|p| p.service.clone())
                .collect();
            emit_log(
                app,
                &format!(
                    "用户已确认部分回滚:{} 沿用服务器现有镜像(未回退到本归档版本),其余服务正常回退",
                    names.join("、")
                ),
            );
            partial_note = Some(names.join("、"));
        }
    }

    // ---- 插值漂移预检(v6.12.0;06 页路径)----
    // 与 04 页同口径:归档 compose(含远端枚举到的 override)+ 该项目目录当前
    // `.env` 重算,与 manifest 的 tag 比对。漂移 → 须确认(同一错误码)。
    let mut env_drift: Vec<ImageEnvDrift> = Vec::new();
    if let Some(m) = &manifest {
        if has_compose_copy {
            let archived_compose = remote_join(&release_dir, "docker-compose.yml");
            if let Ok((0, compose_text)) =
                exec_collect(&mut client, &cat_file_cmd(&archived_compose)).await
            {
                // override 在远端枚举(本路径无 ProjectConfig)
                let mut ov_texts: Vec<String> = Vec::new();
                if let Ok((0, ls_out)) = exec_collect(&mut client, &ls_dir_cmd(&release_dir)).await {
                    for name in parse_ls_lines(&ls_out) {
                        if archived_override_original(&name).is_some() {
                            if let Ok((0, t)) =
                                exec_collect(&mut client, &cat_file_cmd(&remote_join(&release_dir, &name)))
                                    .await
                            {
                                ov_texts.push(t);
                            }
                        }
                    }
                }
                // 部署目录当前 `.env`
                let env_table = match exec_collect(&mut client, &cat_file_cmd(&remote_join(&dir, ".env"))).await {
                    Ok((0, t)) if !t.trim().is_empty() => parse_env_text(&t),
                    _ => Default::default(),
                };
                match image_refs_with_env(&compose_text, &ov_texts, &env_table) {
                    Ok(refs) => {
                        let pairs: Vec<(String, String)> = m
                            .images
                            .iter()
                            .map(|i| (i.service.clone(), i.tag.clone()))
                            .collect();
                        env_drift = detect_image_env_drift(&refs, &pairs);
                    }
                    Err(e) => emit_log(app, &format!("警告:插值漂移检查跳过({})", e)),
                }
            } else {
                emit_log(app, "警告:读取归档 compose 失败,插值漂移检查跳过");
            }
            for d in &env_drift {
                emit_log(
                    app,
                    &format!(
                        "  [插值漂移] {}:归档记录 {} ,但当前 .env 会解析成 {}",
                        d.service, d.expected, d.resolved
                    ),
                );
            }
            if !env_drift.is_empty() && !allow_partial {
                return Err(crate::errors::tagged(
                    crate::errors::ErrCode::RollbackPrecheck,
                    rollback_env_drift_message(&env_drift),
                ));
            }
            if !env_drift.is_empty() {
                let names: Vec<String> = env_drift.iter().map(|d| d.service.clone()).collect();
                emit_log(
                    app,
                    &format!(
                        "用户已确认插值漂移:{} 将按当前 .env 解析出的引用启动(与归档记录可能不同)",
                        names.join("、")
                    ),
                );
                let note = format!("{} 存在 .env 插值漂移", names.join("、"));
                partial_note = Some(match partial_note.take() {
                    Some(prev) => format!("{};{}", prev, note),
                    None => note,
                });
            }
        }
    }

    // ---- 清单驱动选择装载包(v6.12.0;06 页路径)----
    let selection = match &manifest {
        Some(m) => select_packages(&m.images, &files),
        None => {
            emit_log(
                app,
                "该归档无发布清单,无法核对目录内容:将按目录内全部镜像包恢复",
            );
            PackageSelection {
                to_load: packages.clone(),
                recorded_missing: Vec::new(),
                unrecorded: Vec::new(),
            }
        }
    };
    for name in &selection.unrecorded {
        emit_log(
            app,
            &format!(
                "警告:归档目录内 {} 未被发布清单记录,已跳过装载(防止其覆盖清单记录的标签)",
                name
            ),
        );
    }
    for name in &selection.recorded_missing {
        emit_log(
            app,
            &format!("警告:发布清单记录的镜像包 {} 不在归档目录里(预检应已阻断)", name),
        );
    }

    let packages = selection.to_load;
    let n = packages.len();
    if n == 0 {
        emit_log(app, "发布目录内无可装载的镜像包,跳过 docker load");
    }
    for (i, name) in packages.iter().enumerate() {
        ensure_not_cancelled(app)?;
        let remote_tar = remote_join(&release_dir, name);
        emit_log(
            app,
            &format!("回滚装载镜像包 ({}/{}): docker load -i {}", i + 1, n, remote_tar),
        );
        let load_cmd = format!("docker load -i {}", shell_single_quote(&remote_tar));
        if let Err(e) = exec_forwarded(app, &mut client, &load_cmd, STACK_LOAD_TIMEOUT_SECS).await {
            return Err(format!(
                "装载镜像包 {}/{} 失败:{};已装载 {}/{} 个镜像包,这些包的镜像标签已恢复,容器未重建(可排除问题后重新发起回滚)",
                i + 1, n, e, i, n
            ));
        }
    }

    // 恢复 compose 副本到该项目目录(而非 server.remote_dir)
    ensure_not_cancelled(app)?;
    let target_compose = remote_join(&dir, "docker-compose.yml");
    if has_compose_copy {
        let cp_cmd = format!(
            "cp {} {}",
            shell_single_quote(&remote_join(&release_dir, "docker-compose.yml")),
            shell_single_quote(&target_compose)
        );
        emit_log(app, &format!("恢复 compose 文件: {}", cp_cmd));
        if let Err(e) = exec_forwarded(app, &mut client, &cp_cmd, SSH_EXEC_TIMEOUT_SECS).await {
            emit_log(
                app,
                &format!("警告:恢复 compose 副本失败({}),沿用现有 compose 文件继续回滚", e),
            );
        }
    } else {
        emit_log(app, "发布目录无 compose 副本,沿用现有 compose 文件");
    }

    // ---- 恢复 override 副本(R2;第二十九批;06 页路径)----
    // 本路径由目录驱动、无 ProjectConfig,故在**远端**枚举归档内的
    // `<名>.ddbak` 再按原名恢复 —— 比本地文件名枚举更稳(不依赖本地副本),
    // 且旧归档(无 `.ddbak`)自然跳过。
    ensure_not_cancelled(app)?;
    let (code, bak_out) = exec_collect(&mut client, &ls_dir_cmd(&release_dir)).await?;
    if code == 0 {
        let mut restored = 0usize;
        for name in parse_ls_lines(&bak_out) {
            // 只处理 compose override 形态(避免误恢复其它 .ddbak 后缀物)
            let Some(orig) = archived_override_original(&name) else {
                continue;
            };
            let cp_cmd = format!(
                "cp {} {}",
                shell_single_quote(&remote_join(&release_dir, &name)),
                shell_single_quote(&remote_join(&dir, orig))
            );
            emit_log(app, &format!("恢复 override 文件 {}: {}", orig, cp_cmd));
            match exec_forwarded(app, &mut client, &cp_cmd, SSH_EXEC_TIMEOUT_SECS).await {
                Ok(()) => restored += 1,
                Err(e) => emit_log(
                    app,
                    &format!("警告:恢复 override 文件 {} 失败({}),该文件保持现状", orig, e),
                ),
            }
        }
        if restored == 0 {
            emit_log(app, "发布目录无 override 副本(旧归档);override 文件保持现状");
        }
    }

    // compose up -d:cd 到项目目录,按目录内 compose 文件启动
    // (override 文件按远端同名约定自动生效,无需显式 -f 链)
    ensure_not_cancelled(app)?;
    // ---- 按 ID 收敛(v6.12.0;up 前最后一道复核;06 页路径)----
    // 与 04 页同口径:预检快照到 up 之间隔着逐包 load,期间外部改标签会让结论
    // 失效(TOCTOU)。这里按 manifest 记录的 ID 复核一次并零拷贝指回。
    if let Some(m) = &manifest {
        match query_remote_images_full(&mut client).await {
            Ok(list) => {
                let remote: Vec<(String, String)> = list
                    .into_iter()
                    .map(|i| (format!("{}:{}", i.repository, i.tag), i.id))
                    .collect();
                let convergence = plan_tag_convergence(&m.images, &remote);
                for (svc, id, tag) in &convergence.retag {
                    emit_log(
                        app,
                        &format!(
                            "{}:标签未指向归档版本,自动指回(docker tag {} {} )",
                            svc,
                            short_id(id),
                            tag
                        ),
                    );
                    let cmd = docker_tag_cmd(id, tag);
                    if let Err(e) =
                        exec_forwarded(app, &mut client, &cmd, SSH_EXEC_TIMEOUT_SECS).await
                    {
                        return Err(format!("自动指回标签失败({}):{}", tag, e));
                    }
                }
                if !convergence.missing.is_empty() {
                    let details = convergence
                        .missing
                        .iter()
                        .map(|(svc, tag)| format!("· {}:期望镜像已不在服务器上({})", svc, tag))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !allow_partial {
                        return Err(crate::errors::tagged(
                            crate::errors::ErrCode::RollbackPrecheck,
                            format!(
                                "up 前复核发现预检结论已失效(可能有其它主体在操作服务器):\n{}\n如仍要回滚,这些服务将沿用服务器当前镜像。选择「仍要回滚」继续。",
                                details
                            ),
                        ));
                    }
                    emit_log(
                        app,
                        &format!("up 前复核:以下服务镜像已不在服务器,将沿用当前镜像:{}", details),
                    );
                }
            }
            Err(e) => emit_log(
                app,
                &format!("警告:up 前复核查询镜像列表失败({});按预检结论继续", e),
            ),
        }
    }

    // ---- compose up -d:cd 到项目目录,按目录内 compose 文件启动 ----
    // (override 文件按远端同名约定自动生效,无需显式 -f 链;
    //  拼装走 compose_up_cmd_in_dir:P1/P2 加固旗标随之内联)
    ensure_not_cancelled(app)?;
    let up_cmd = compose_up_cmd_in_dir(&dir);
    emit_log(app, &format!("启动服务: {}", up_cmd));
    exec_forwarded(app, &mut client, &up_cmd, STACK_COMPOSE_TIMEOUT_SECS).await?;

    // ---- up 后运行镜像校验(v6.12.0;06 页路径)----
    if let Some(m) = &manifest {
        // 与 06 页 up 命令逐字同源(该路径无 -f 链,靠 cd 后的默认文件)
        let prefix = format!("cd {} && docker compose", shell_single_quote(&dir));
        let expected = expected_images_from_manifest(&m.images);
        let report = verify_running_images(app, &mut client, &prefix, &expected).await;
        if let Err(e) = report {
            emit_log(app, &format!("警告:up 后运行镜像校验未能完成({})", e));
        }
    }

    emit_log(app, "整栈回滚完成");
    record.success = true;
    record.message = match partial_note {
        // 部分回滚/插值漂移要在历史/通知里写明,不能只说「回滚到 X」
        Some(ref names) => format!("回滚到 {}({})", release_ts, names),
        None => format!("回滚到 {}", release_ts),
    };
    record.release_dir = Some(release_dir);
    record.duration_secs = started.elapsed().as_secs();
    Ok(record)
}

/// 组装回滚收尾通知的标题与正文(纯函数,便于单测)。///
/// 标题:成功=「回滚成功」;取消(错误码 canceled)=「回滚已取消」;
/// 其余失败=「回滚失败」。正文含项目名 + 服务器名 + 结果消息 + 耗时
/// (成功时消息为「回滚到 <目标 release ts / 镜像标签>」,回滚目标随之入文);
/// `record` 为 `None`(panic 或配置读取等早期失败,记录未组装/随错误丢失)
/// 时用兜底文案。`message` 允许带错误码标记:正文剥标记后展示(第十六批)。
pub(crate) fn rollback_notify_text(
    success: bool,
    message: &str,
    record: &Option<DeployRecord>,
) -> (String, String) {
    let title = if success {
        "回滚成功".to_string()
    } else if crate::errors::code_of(message) == Some(crate::errors::ErrCode::Cancelled) {
        "回滚已取消".to_string()
    } else {
        "回滚失败".to_string()
    };
    let body = match record {
        Some(r) => format!(
            "项目「{}」@ 服务器「{}」:{}(耗时 {} 秒)",
            r.project_name,
            r.server_name,
            crate::errors::strip(message),
            r.duration_secs
        ),
        None => format!("{}(回滚详情缺失,详见应用日志)", crate::errors::strip(message)),
    };
    (title, body)
}

/// 回滚命令的统一收尾:任何路径(成功/失败/panic)下 `deploy-done` 恰好
/// emit 一次;成功后落地 `mode = "rollback"` 的部署历史记录(append 失败
/// 仅告警,不影响回滚结果);emit 之后调 [`crate::notify::fire`] 分发通知
/// 中心通知(成功/失败/取消,事件订阅复用部署的 AppConfig.notify.events,
/// 失败仅告警,不影响回滚结果)。
/// up 后运行镜像校验(v6.12.0;回滚/部署两侧共用)。
///
/// `compose_prefix` = 调用方 up 时用的完整命令前缀(`cd '<dir>' && docker
/// compose [-f ...]`;各链的 `-f` 链不同,必须与 up **逐字一致**,否则
/// compose 的项目名/文件集不同,`ps -q` 可能查不到容器)。
///
/// 只说 2 次 SSH:`ps -q`(全部容器 ID)+ 一次 `docker inspect`(服务名 + 镜像)。
/// 期望表 = manifest/本地采集的 ID;**只校验有 ID 的服务**(旧归档无从比对)。
/// 服务无容器 → 实际值空串 → 判不一致(**不能静默**:up 后没起容器本身就是
/// 用户必须知道的状态)。
///
/// 一致 → 一行日志;不一致 → 逐条告警。**不改判定结果**:成败由 up 的退出码
/// 决定,这里只负责让「实际跑的版本 ≠ 期望版本」在日志与历史里可见
/// (兜住 .env 漂移、compose 副本恢复失败、compose 未重建容器等残余路径)。
pub(crate) async fn verify_running_images(
    app: &AppHandle,
    client: &mut SshClient,
    compose_prefix: &str,
    expected: &[(String, String)],
) -> Result<(), String> {
    if expected.is_empty() {
        return Ok(());
    }
    let actual = collect_running_images(client, compose_prefix).await?;
    if actual.is_empty() {
        // 无任何容器:全部服务按「无容器」判不一致(不静默)
        for (svc, want) in expected {
            emit_log(
                app,
                &format!(
                    "警告:{} 容器实际运行的镜像与期望不一致(期望 {},实际 无容器)—— 请检查 .env 插值 / compose 文件 / 容器是否重建",
                    svc,
                    short_id(want)
                ),
            );
        }
        return Ok(());
    }
    let mismatches = select_container_image_mismatches(expected, &actual);
    if mismatches.is_empty() {
        emit_log(
            app,
            &format!("up 后运行镜像校验:{} 个服务的实际镜像与期望一致", expected.len()),
        );
        return Ok(());
    }
    for (svc, want, got) in &mismatches {
        emit_log(
            app,
            &format!(
                "警告:{} 容器实际运行的镜像与期望不一致(期望 {},实际 {})—— 请检查 .env 插值 / compose 文件 / 容器是否重建",
                svc,
                short_id(want),
                if got.is_empty() {
                    "无容器".to_string()
                } else {
                    short_id(got)
                }
            ),
        );
    }
    Ok(())
}

/// 组装回滚失败的留痕骨架(R4;第二十九批;纯查配置,失败给 `None`)。
///
/// 为什么在命令层组装:管线失败时没有 record 返回,而失败留痕需要
/// 「服务器名 / 项目名」这类上下文 —— 命令层刚好能拿到配置(管线内失败点
/// 各不相同,不宜在每处 error 里携带)。
fn rollback_failure_skeleton(
    server_id: &str,
    project_id: Option<&str>,
) -> Option<DeployRecord> {
    let cfg = load_config().ok()?;
    let server = cfg.servers.iter().find(|s| s.id == server_id)?;
    let project_name = project_id
        .and_then(|pid| cfg.projects.iter().find(|p| p.id == pid))
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let mut rec = DeployRecord::new_skeleton(
        MODE_ROLLBACK,
        &server.name,
        &project_name,
        Vec::new(),
    );
    rec.server_id = Some(server.id.clone());
    rec.project_id = project_id.map(String::from);
    Some(rec)
}

/// 失败时用于留痕的记录骨架(R4;第二十九批):`inner` 失败时没有 record
/// 可返回,故由调用方提供一个**带上下文的骨架**(服务器/项目名),收尾时写入
/// 失败原因。`None` = 该入口拿不到上下文(如早期校验失败),不留痕。
async fn finish_rollback_with<F>(
    app: &AppHandle,
    failed_record: Option<DeployRecord>,
    fut: F,
) -> Result<(), String>
where
    F: std::future::Future<Output = Result<DeployRecord, String>> + Send,
{
    // 托盘 tooltip(第十五批):回滚执行中。回滚期间服务会重启,窗口隐藏时
    // 用户应当能从托盘看出「现在不是空闲,别动手动脚」;收尾(成功/失败)在
    // 下方 match 内清态
    crate::tray_status::set_rolling_back(app, true);
    let result = match CatchPanic::new(fut).await {
        Ok(res) => res,
        Err(panic_info) => {
            log::error!("回滚管线发生 panic: {}", panic_info);
            Err("回滚过程发生内部错误,详情见日志".to_string())
        }
    };
    // 无论成败都清回滚态;成败后的空闲文案分别带「上次部署成功/失败」语义
    // (回滚的成败也记进同一终态:用户视角「回滚完成」即部署态的一种恢复)
    crate::tray_status::set_deploy_finished(app, result.is_ok(), false);
    match result {
        Ok(record) => {
            // 通知文案在 record 被消费前组装(正文含项目名与回滚目标)
            let (title, body) = rollback_notify_text(true, &record.message, &Some(record.clone()));
            let _ = app.emit(
                "deploy-done",
                DeployDone {
                    success: true,
                    message: "回滚完成".to_string(),
                    error_code: None,
                },
            );
            // 通知中心:回滚成功(emit deploy-done 之后异步分发,不阻塞收尾);
            // 第二十批阶段五:带耗时(低于 notify.min_duration_secs 阈值时跳过)
            let dur = record.duration_secs;
            crate::notify::fire_with_duration(app.clone(), "success", title, body, Some(dur))
                .await;
            append_record(record);
            Ok(())
        }
        Err(e) => {
            emit_log(app, &format!("回滚失败: {}", crate::errors::strip(&e)));
            // 取消导致的失败按错误码分发(第十六批,不再比对文案)
            let kind = if crate::errors::code_of(&e) == Some(crate::errors::ErrCode::Cancelled) {
                "cancel"
            } else {
                "failure"
            };
            let (title, body) = rollback_notify_text(false, &e, &None);
            let _ = app.emit(
                "deploy-done",
                DeployDone {
                    success: false,
                    message: e.clone(),
                    error_code: crate::errors::code_of(&e)
                        .map(|c| c.as_str()),
                },
            );
            // 通知中心:回滚失败/取消(emit deploy-done 之后异步分发)
            crate::notify::fire(app.clone(), kind, title, body).await;
            // R4(第二十九批):**失败也留痕**。此前只有成功才落历史,失败的回滚
            // 在界面上"从世上消失"(刷新页面后无法复盘发生了什么),而回滚失败
            // 恰恰是最需要事后排查的场景。取消单独记(与部署历史口径一致:
            // mode 仍为 rollback,message 说明取消/失败原因)。
            if let Some(mut rec) = failed_record {
                rec.success = false;
                rec.message = crate::errors::strip(&e).to_string();
                append_record(rec);
            }
            Err(e)
        }
    }
}

/// 解析发布目录的 manifest.json 内容(纯函数):损坏 / 缺字段 → `None`
/// (调用方按"无清单"降级,不让列表与回滚功能失败)。
pub fn parse_release_manifest(json: &str) -> Option<ReleaseManifest> {
    serde_json::from_str(json.trim()).ok()
}

// ===== 钩子/健康检查纯逻辑(便于单测,Task 3)=====

