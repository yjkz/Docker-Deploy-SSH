/* ============================================================
 * deploy.js — 部署向导页逻辑(依赖 app.js 提供的全局工具)
 *
 * 后端命令(对象字段为 Rust snake_case 原样序列化,JS 参数名 camelCase;
 * 例外:ReleaseBrief 标注 serde rename_all=camelCase,字段即 hasManifest /
 * hasComposeCopy,与本文件读取处一致):
 * - list_images() -> ImageInfo[] { repository, tag, size_bytes, created, id }
 * - get_config() -> AppConfig { servers: [{ id, name, ... }],
 *                               projects: [{ id, name, ... }] }
 * - server_env_check({ serverId })
 *     -> ServerCheckReport { docker, compose, gzip, remote_dir_exists,
 *                            disk_free_gb, errors: string[] }
 * - deploy({ req }) -> 同步返回 null,结果只经事件:
 *     req = { image, repository, server_id, project_id,
 *             use_date_tag, skip_unchanged, password_plain }  // 必须 snake_case
 *     (skip_unchanged 智能传输:远端同标签镜像 ID 一致时跳过导出/上传/装载,
 *      仅 use_date_tag = false 时生效)
 * - parse_compose({ projectId }) -> ComposeStack
 *     ComposeStack = { project_name, services: StackService[], errors: string[] }
 *     StackService = { service, image, has_build, mode: "Local"|"Pull",
 *                      match_state: "Exact"|"RepoOnly"|"Missing",
 *                      local_tag, warning }(按项目解析,含 overrides 与本地镜像实时匹配)
 * - deploy_stack({ req }) -> 同步返回 null,结果只经事件:
 *     req = { project_id, server_id,
 *             services: [{ service, image, mode: "Local"|"Pull" }],
 *             skip_unchanged, force_archive, password_plain } // 必须 snake_case
 *     (force_archive 强制留档:未变化镜像仍打包进 release 目录供回滚,仅跳过装载)
 * - cancel_deploy() -> 置位后端全局取消标志
 * - preview_stack_changes({ serverId, projectId, passwordPlain? }) -> StackPreview
 *     StackPreview = { entries: [{ service, image, mode: "Local"|"Pull",
 *                      action: "Recreate"|"Create"|"Unchanged"|"Pull"|"Absent" }],
 *                      errors: string[] }(独立 dry-run,只读不落盘)
 * - get_history() -> DeployRecord[](倒序 = 最新在前)
 *     DeployRecord = { ts, mode: "single"|"stack"|"rollback", server_name,
 *                      project_name, images: string[], success, message,
 *                      duration_secs, release_dir,
 *                      server_id, project_id }  // 后两者第四批新增(旧记录为空,回滚按名回退)
 * 一键回滚(复用 deploy-log / deploy-done 事件体系;成功落 mode="rollback" 历史):
 * - rollback_list_releases({ serverId, projectId })
 *     -> ReleaseBrief[] { ts, files, services, hasManifest, hasComposeCopy }
 *        (发布目录列表,新 → 旧;无清单旧版 services 为空)
 * - rollback_list_tags({ serverId, projectId, repository })
 *     -> TagBrief[] { tag, id, created }(创建时间倒序)
 * - rollback_execute_stack({ serverId, projectId, releaseTs })
 * - rollback_execute_single({ serverId, projectId, repository, dateTag, targetRef })
 *
 * 断点续传(UPGRADE-PLAN 阶段六;事件与正常部署完全一致,前端仅多一条横幅入口):
 * - deploy_resume_status({ serverId, projectId }) -> ResumeView | null
 *     ResumeView = { key, mode: "single"|"stack", stepNext, stepLabel, ts,
 *                    serverName, projectName }(同服务器+项目多条断点取 ts 最新)
 * - deploy_resume_start({ key, passwordPlain? }) -> 同步返回 null,结果只经事件
 *     (管线从断点 stepNext 起,deploy-progress / deploy-log / deploy-done 与
 *      正常部署一致,进度条无需特殊处理;发起前按断点模式对齐页签节点集)
 * - deploy_resume_discard({ key })(删断点 + 清本地临时 tar + 尽力清远端临时产物)
 *
 * 事件(AppBus.on,模块级守卫保证只注册一次):
 * - 'deploy-progress' { step, total, message }
 *     单镜像模式 step 1..5(打标签/导出压缩/上传镜像/同步文件/服务器部署);
 *     整栈模式  step 1..6(分类确认/打包/上传/装载/拉取/启动)
 * - 'deploy-log'      string(带 [HH:MM:SS] 前缀的一行日志)
 * - 'deploy-done'     { success, message }(取消固定 message === "部署已取消")
 *
 * 交互约定:
 * - 页首「单镜像 / 整栈部署(compose)」双 tab 切换模式;单镜像模式行为不变;
 *   整栈模式:项目下拉选中即自动 parse_compose 渲染服务分类表(可手动重新解析),
 *   Local/Pull 文字按钮逐服务切换,「保存为默认分类」写回 project.service_overrides;
 *   compose errors 非空时红框并阻断开始部署。
 * - 部署中(deploy 起点至 deploy-done)「开始部署」禁用、「取消部署」可用,
 *   且不得再次发起 deploy(后端全局取消标志会在新部署开始时被重置);
 *   部署 / 预检期间同时禁用模式切换与服务器、项目、镜像下拉。
 * - 进入页面时重新拉取 list_images + get_config(镜像与配置可能变化);
 *   若 window.__pendingDeployImage 存在(镜像页「部署」按钮带入),自动选中
 *   对应下拉项,用后即删(置 null);找不到则 toast 提示并忽略。
 * - 日志区自动滚底,但用户向上滚动查看历史时不强制拉底
 *   (仅在 scrollTop 接近底部时才 autoscroll)。
 * - 「部署预览」为独立 dry-run(preview_stack_changes):不自动触发、
 *   不影响开始部署;切换项目/服务器后预览结果隐藏,需重新预览。
 * - 「部署历史」折叠面板(get_history):进入页面自动刷新一次,
 *   部署结束(deploy-done)后亦刷新;上限渲染 50 条 + 总数计数。
 *   每条记录带「回滚」操作(rollback 模式记录显示灰色 —,防回滚链):
 *   打开 #deploy-modal(整栈选历史发布 / 单镜像选历史日期标签 + 目标引用,
 *   服务器与项目按记录名称从当前配置反查),执行期复用 st.deploying 互斥,
 *   模态内等宽日志区镜像 deploy-log,执行中禁止关闭模态(Esc/遮罩均拦截)。
 * - 断点续传横幅:deploy-done(失败/取消)后、进入页面与切换服务器/项目时查询
 *   deploy_resume_status,命中当前选中服务器+项目时显示「从步骤 N 继续 / 放弃断点」;
 *   「继续」复用 st.deploying 互斥发起 deploy_resume_start(密码不传,用后端已存
 *   密文),之后事件渲染与正常部署一致;「放弃」就地二次确认后 deploy_resume_discard。
 *   开始新部署 / 续传发起 / 放弃成功 / 部署成功(断点已清)时隐藏横幅。
 *
 * 安全说明:所有来自后端/配置的数据一律 createElement + textContent 渲染,
 * 不使用 innerHTML 拼接;提示一律用 toast / 自绘错误框,不调用系统对话框。
 * ============================================================ */
(function () {
  'use strict';

  /** 部署日志最多保留的行数(超出丢弃最早的,防内存膨胀) */
  var LOG_MAX_LINES = 2000;
  /** 远端磁盘剩余空间低于该值(GB)视为未通过,阻止部署 */
  var DISK_MIN_GB = 2;
  /** 部署日志距离底部多少像素以内视为「在底部」(才自动滚底) */
  var LOG_BOTTOM_GAP = 40;
  /** 部署历史渲染上限(超过只渲染最新 N 条,计数展示总数) */
  var HISTORY_MAX_ROWS = 50;
  /** 部署历史镜像列展示上限字符数(超长截断,完整内容放 title) */
  var HISTORY_IMAGES_MAX = 64;
  /** 日期标签形态(release ts / 打标签生成的 tag,形如 20260905-101010) */
  var DATE_TAG_RE = /^\d{8}-\d{6}$/;

  /**
   * 部署页选择记忆(第四批):记住上次选的服务器与项目,重启后恢复。
   * 与 manage.js 的 dd_manage_autorefresh 同一套做法(模块级 key + 启动
   * restore + 变更即 save + try/catch 静默降级)。
   */
  var PREF_SERVER_KEY = 'dd_deploy_server';
  var PREF_PROJECT_KEY = 'dd_deploy_project';

  /**
   * 两组进度节点(与后端 deploy-progress 步骤一一对应):
   * 单镜像 5 节点 / 整栈 6 节点;按当前模式选节点集渲染。
   */
  var STEP_SETS = {
    single: {
      names: ['打标签', '导出压缩', '上传镜像', '同步文件', '服务器部署'],
      ens: ['TAG', 'PACK', 'UPLOAD', 'SYNC', 'APPLY']
    },
    stack: {
      names: ['分类确认', '打包', '上传', '装载', '拉取', '启动'],
      ens: ['CONFIRM', 'PACK', 'UPLOAD', 'LOAD', 'PULL', 'UP']
    }
  };

  var st = {
    images: [],        // 过滤 <none> 后的可用镜像(ImageInfo[])
    cfg: null,         // get_config 的完整结果(AppConfig)
    loading: false,    // 页面数据加载中(list_images + get_config)
    checking: false,   // 部署前预检中(server_env_check)
    deploying: false,  // 部署中(invoke deploy/deploy_stack 成功 → deploy-done)
    logs: [],          // deploy-log 事件累积的日志行
    mode: 'single',    // 'single' 单镜像 | 'stack' 整栈部署(compose)
    stack: null,       // parse_compose 结果(ComposeStack),整栈模式服务分类表数据源
    stackProjectId: '',// st.stack 对应的项目 id(切换项目后需重新解析)
    parsing: false,    // parse_compose 请求进行中(防重复解析)
    previewing: false, // preview_stack_changes 请求进行中(防重复预览)
    history: [],       // get_history 结果(DeployRecord[],倒序 = 最新在前)
    historyLoaded: false, // 是否已成功拉取过部署历史
    historyLoading: false, // 部署历史加载中(防重复请求)
    rbKind: '',        // 回滚模态类型:'stack' | 'single'(空串 = 模态未打开)
    rbRecord: null,    // 触发回滚的部署历史记录
    rbIds: null,       // 按记录名称反查出的 { serverId, projectId }
    rbRepository: '',  // 单镜像回滚的仓库名(记录 images[0] 冒号前部分)
    rbReleases: [],    // rollback_list_releases 结果(新 → 旧)
    rbTags: [],        // rollback_list_tags 结果(创建时间倒序)
    rbBusy: false,     // 回滚执行中(发起 invoke → deploy-done;期间模态禁止关闭)
    batch: null,       // 批量部署:{ active, queue[], idx, success, failed, skipped, aborted, deferred };前端编排串行队列,后端零改动
    rbLogs: [],        // 回滚模态内日志区累积的日志行(deploy-log 镜像)
    resume: null,      // deploy_resume_status 查询到的断点视图(ResumeView;无断点为 null)
    resumeBusy: false, // deploy_resume_discard 请求进行中(防重复提交)
    pendingReleaseNotes: null // 发起整栈部署时快照的版本备注 { serverId, projectId,
                              // title, body };成功后经 rollback_set_release_notes
                              // 补写,失败/取消保留(断点续传成功后于 handleDone
                              // 补写);批量部署恒 null
  };

  /**
   * 项目跨服务器迁移状态。**必须声明在 refreshControls 之前**:
   * 该函数读取 migState.active 计算入口按钮禁用态,而 var 声明会被提升,
   * 声明若晚于使用点即读到 undefined(本项目批量部署曾因同类问题导致
   * 批量期间控件锁全失效,见 8eab7b4)。
   */
  var migState = {
    active: false,      // 迁移执行中(禁关闭模态/禁重复发起/控制入口禁用)
    previewing: false,  // 预检请求进行中
    plan: null,         // migrate_project_preview 结果(MigrateProjectPlan)
    listenerBound: false
  };

  /** 部署事件监听守卫:只注册一次,防止重复绑定 */
  var listenersBound = false;

  // ===== 小工具 =====

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  /** docker 对缺失的仓库名 / 标签显示 <none>;空串同样按 <none> 处理 */
  function isNone(value) {
    if (value === null || value === undefined) return true;
    var s = String(value).trim();
    return s === '' || s === '<none>';
  }

  function normalizeCfg(cfg) {
    var out = (cfg && typeof cfg === 'object') ? cfg : {};
    if (!Array.isArray(out.servers)) out.servers = [];
    if (!Array.isArray(out.projects)) out.projects = [];
    return out;
  }

  /** 过滤悬空镜像:repository 或 tag 为 <none> 的不列入下拉 */
  function filterUsable(list) {
    return (Array.isArray(list) ? list : []).filter(function (img) {
      return img && !isNone(img.repository) && !isNone(img.tag);
    });
  }

  function findImageByRef(ref) {
    for (var i = 0; i < st.images.length; i++) {
      var img = st.images[i];
      if (String(img.repository) + ':' + String(img.tag) === ref) return img;
    }
    return null;
  }

  function findById(list, id) {
    if (!Array.isArray(list)) return null;
    for (var i = 0; i < list.length; i++) {
      if (list[i] && list[i].id === id) return list[i];
    }
    return null;
  }

  // ===== 错误框(#deploy-error:预检未通过 / 预检失败 / 加载失败)=====

  /** @param {boolean} withJump 是否附带「跳转服务器管理」按钮 */
  function showErrorBox(lines, withJump) {
    var box = document.getElementById('deploy-error');
    if (!box) return;
    box.textContent = '';
    (Array.isArray(lines) ? lines : [lines]).forEach(function (line) {
      if (line) box.appendChild(el('div', 'servers-error-text', line));
    });
    if (withJump) {
      var jump = el('button', 'btn', '跳转服务器管理');
      jump.type = 'button';
      jump.addEventListener('click', function () {
        window.showPage('servers');
      });
      box.appendChild(jump);
    }
    box.classList.remove('hidden');
  }

  function hideErrorBox() {
    var box = document.getElementById('deploy-error');
    if (box) {
      box.textContent = '';
      box.classList.add('hidden');
    }
  }

  // ===== 顶部横幅(结束状态,持续到下次开始部署)=====

  function showBanner(kind, text) {
    var banner = document.getElementById('deploy-banner');
    if (!banner) return;
    banner.className = 'banner banner-' + kind;
    banner.textContent = text;
  }

  function hideBanner() {
    var banner = document.getElementById('deploy-banner');
    if (!banner) return;
    banner.className = 'banner banner-ok hidden';
    banner.textContent = '';
  }

  // ===== 断点续传横幅(deploy_resume_status / start / discard)=====

  /** 隐藏续传横幅并清空已查询到的断点(开始新部署 / 放弃成功 / 无断点时) */
  function hideResumeBanner() {
    var box = document.getElementById('deploy-resume-banner');
    if (box) {
      box.textContent = '';
      box.classList.add('hidden');
    }
    st.resume = null;
  }

  /**
   * 渲染续传横幅(与其他渲染一致:createElement + textContent,不拼 innerHTML)。
   * @param {object} view deploy_resume_status 返回的 ResumeView
   * @param {boolean} confirming true = 「放弃断点」的就地二次确认态
   */
  function renderResumeBanner(view, confirming) {
    var box = document.getElementById('deploy-resume-banner');
    if (!box || !view) return;
    st.resume = view;

    var modeText = String(view.mode) === 'stack' ? '整栈' : '单镜像';
    box.textContent = '';
    box.appendChild(el('div', 'servers-error-text',
      '检测到 ' + String(view.ts || '') + ' 一次未完成的' + modeText + '部署（服务器 ' +
      String(view.serverName || '') + ' / 项目 ' + String(view.projectName || '') +
      '），中断于步骤：' + String(view.stepLabel || '未知')));

    if (confirming) {
      box.appendChild(el('div', 'servers-error-text',
        '放弃将删除断点,并清理该次部署保留的本地临时文件与服务器上的临时产物,确认放弃?'));
      var sure = el('button', 'btn btn-danger', '确认放弃');
      sure.type = 'button';
      sure.id = 'deploy-resume-discard-btn';
      sure.addEventListener('click', onResumeDiscard);
      var keep = el('button', 'btn', '保留断点');
      keep.type = 'button';
      keep.id = 'deploy-resume-keep-btn';
      keep.addEventListener('click', function () { renderResumeBanner(st.resume, false); });
      box.appendChild(sure);
      box.appendChild(keep);
    } else {
      var go = el('button', 'btn btn-primary',
        '从步骤 ' + (Number(view.stepNext) || 1) + ' 继续');
      go.type = 'button';
      go.id = 'deploy-resume-continue-btn';
      go.addEventListener('click', onResumeStart);
      var drop = el('button', 'btn', '放弃断点');
      drop.type = 'button';
      drop.id = 'deploy-resume-discard-btn';
      drop.addEventListener('click', function () { renderResumeBanner(st.resume, true); });
      box.appendChild(go);
      box.appendChild(drop);
    }

    // 渲染时即与互斥状态对齐(部署/预检/放弃请求进行中一律禁用,口径同 refreshControls)
    var locked = st.deploying || st.checking || st.resumeBusy;
    ['deploy-resume-continue-btn', 'deploy-resume-discard-btn', 'deploy-resume-keep-btn']
      .forEach(function (id) {
        var node = document.getElementById(id);
        if (node) node.disabled = locked;
      });
    box.classList.remove('hidden');
  }

  /**
   * 查询当前选中服务器/项目的部署断点并同步横幅显隐:
   * 命中(后端按键 server|project 过滤,取 ts 最新一条)则显示,无断点则隐藏。
   * 查询失败静默降级(console.warn),不影响部署主流程。
   */
  function refreshResumeStatus() {
    var srvSel = document.getElementById('deploy-server');
    var prjSel = document.getElementById('deploy-project');
    var serverId = srvSel ? String(srvSel.value) : '';
    var projectId = prjSel ? String(prjSel.value) : '';
    if (!serverId || !projectId) {
      hideResumeBanner();
      return;
    }

    window.AppBus.invoke('deploy_resume_status',
        { serverId: serverId, projectId: projectId })
      .then(function (view) {
        // 过期响应丢弃:期间选中项已变化,或已进入部署/预检/放弃请求(新运行
        // 开始时已清横幅;放弃进行中不重绘,显隐由 discard 结果决定)
        var curSrv = srvSel ? String(srvSel.value) : '';
        var curPrj = prjSel ? String(prjSel.value) : '';
        if (curSrv !== serverId || curPrj !== projectId) return;
        if (st.deploying || st.checking || st.resumeBusy) return;
        if (view && view.key) renderResumeBanner(view, false);
        else hideResumeBanner();
      })
      .catch(function (err) {
        if (window.console && console.warn) {
          console.warn('[deploy] deploy_resume_status 查询失败:', err);
        }
      });
  }

  /** 「从步骤 N 继续」:复用 st.deploying 互斥,之后事件渲染与正常部署完全一致 */
  function onResumeStart() {
    if (st.deploying || st.checking) return; // 并发防护:部署 / 预检中不得再次发起
    var cp = st.resume;
    if (!cp || !cp.key) return;
    var key = String(cp.key);

    // 断点模式与当前页签不一致时先切模式(deploy-progress 步骤号按对应节点集渲染;
    // 横幅仅按当前选中服务器+项目展示,表单里必已选中该服务器与项目)
    var wantMode = String(cp.mode) === 'stack' ? 'stack' : 'single';
    if (st.mode !== wantMode) setMode(wantMode);

    resetRunView(); // 清横幅/错误框/预检条/日志,进度归零(续传开始即隐藏横幅)

    st.deploying = true;
    refreshControls();
    renderProgress(0, '');

    // 密码用后端已存密文,不传 passwordPlain(与预检/预览口径一致)
    window.AppBus.invoke('deploy_resume_start', { key: key })
      .catch(function (err) {
        // invoke 级失败(断点已被清理/参数异常):续传未真正启动,立即还原控件
        st.deploying = false;
        refreshControls();
        showErrorBox(['发起续传失败:' + (errText(err) || '未知错误')], false);
        refreshResumeStatus(); // 断点仍在时重查恢复横幅
      });
  }

  /** 「放弃断点」确认后的执行:deploy_resume_discard 删断点并清理临时产物 */
  function onResumeDiscard() {
    var cp = st.resume;
    if (!cp || !cp.key || st.resumeBusy) return;
    st.resumeBusy = true;
    refreshControls(); // 确认态两按钮同步禁用,防重复提交

    window.AppBus.invoke('deploy_resume_discard', { key: String(cp.key) })
      .then(function () {
        st.resumeBusy = false;
        hideResumeBanner();
        window.toast('已放弃断点,临时文件已清理', 'ok');
      })
      .catch(function (err) {
        st.resumeBusy = false;
        refreshControls();
        renderResumeBanner(st.resume, false); // 退出确认态,保留横幅等待重试
        window.toast('放弃断点失败:' + (errText(err) || '未知错误'), 'fail');
      });
  }

  // ===== 下拉渲染(每次进入页面重建;尽量保留原选中项)=====

  /**
   * 填充下拉。占位项必须是「未选中时的 fallback 选项」而不能是 disabled:
   * WebView2(Chromium)对「当前选中项为 disabled option」的 select 弹层
   * 渲染有缺陷 —— 弹层高度塌缩且选项文字不显示,表现为「点开只下拉一点点、
   * 无任何内容」。改为普通 option(空值)+ 未选中时置于选择态,弹层即可
   * 正常按选项数撑开并显示文字;空值在提交校验处已被拦截(「请先选择…」)。
   */
  function fillSelect(select, placeholderText, options, restoreValue) {
    if (!select) return;
    // 恢复优先级:显式传入的 restoreValue(如 localStorage 记忆)→ 当前值
    var prev = (restoreValue !== undefined && restoreValue !== null && restoreValue !== '')
      ? restoreValue
      : select.value;
    select.textContent = '';

    var ph = document.createElement('option');
    ph.value = '';
    ph.textContent = placeholderText;
    select.appendChild(ph);

    options.forEach(function (opt) {
      var node = document.createElement('option');
      node.value = opt.value;
      node.textContent = opt.text;
      select.appendChild(node);
    });

    var restored = false;
    for (var i = 0; i < select.options.length; i++) {
      if (select.options[i].value === prev && prev !== '') {
        select.value = prev;
        restored = true;
        break;
      }
    }
    if (!restored) select.value = '';
  }

  /** 读记忆的选择(异常/不可用静默返回空串 = 不恢复) */
  function readPref(key) {
    try {
      var v = window.localStorage.getItem(key);
      return (typeof v === 'string') ? v : '';
    } catch (e) {
      return '';
    }
  }

  /** 写记忆的选择(静默降级:隐私模式等场景不影响正常使用) */
  function savePref(key, value) {
    try {
      if (value) window.localStorage.setItem(key, String(value));
      else window.localStorage.removeItem(key);
    } catch (e) { /* 忽略 */ }
  }

  /**
   * 按「是否属于当前所选服务器」给项目排序并打标注(第四批,不过滤)。
   *
   * 需求背景:一台服务器常有多个项目,项目下拉此前是全局平铺、也不联动,
   * 每次切换都得在服务器与项目两个下拉里各选一遍。这里把属于当前服务器的
   * 项目排到最前并加「本机」标注,其余项目仍可选(保留跨服务器部署能力)。
   */
  function projectOptionsFor(serverId) {
    var projects = (st.cfg && Array.isArray(st.cfg.projects)) ? st.cfg.projects : [];
    var own = [];
    var others = [];
    projects.forEach(function (p) {
      var isOwn = serverId && String(p.default_server_id || '') === String(serverId);
      var label = String(p.name);
      // 项目自带独立目录时标注,便于区分同服务器的多个项目
      if (isOwn) own.push({ value: String(p.id), text: '★ ' + label + '(本机)' });
      else others.push({ value: String(p.id), text: label });
    });
    return own.concat(others);
  }

  function renderSelects() {
    var imgSel = document.getElementById('deploy-image');
    var srvSel = document.getElementById('deploy-server');
    var prjSel = document.getElementById('deploy-project');

    var noImages = st.images.length === 0;
    fillSelect(imgSel, noImages ? '暂无可用镜像' : '请选择镜像',
      st.images.map(function (img) {
        return { value: String(img.repository) + ':' + String(img.tag), text: String(img.repository) + ':' + String(img.tag) };
      }));

    // 全部为悬空镜像时的提示文字(「下拉空 + 提示」)
    var hint = document.getElementById('deploy-images-hint');
    if (hint) {
      hint.textContent = noImages
        ? '未找到可用镜像(仓库名或标签为 <none> 的悬空镜像不可部署),请先构建或拉取镜像'
        : '';
      hint.classList.toggle('hidden', !noImages);
    }

    // 服务器 / 项目下拉:恢复上次选择(localStorage),项目按当前服务器排序
    fillSelect(srvSel, '请选择服务器',
      (st.cfg ? st.cfg.servers : []).map(function (s) {
        return { value: String(s.id), text: String(s.name) };
      }), readPref(PREF_SERVER_KEY));
    fillSelect(prjSel, '请选择项目',
      projectOptionsFor(srvSel ? srvSel.value : ''), readPref(PREF_PROJECT_KEY));

    // 记忆里选中的项目带有默认服务器时,若服务器尚未选择则一并带出
    // (重启后恢复"上次那台服务器 + 那个项目"的完整上下文)
    if (srvSel && !srvSel.value && prjSel && prjSel.value) {
      syncServerForProject(prjSel.value);
    }
    updateProjectHint();
  }

  /**
   * 按项目带出默认服务器(第四批):仅当项目配了 default_server_id 且该
   * 服务器仍存在时自动切换;不锁定,用户仍可改。
   */
  function syncServerForProject(projectId) {
    if (!st.cfg || !projectId) return false;
    var prj = findById(st.cfg.projects, projectId);
    if (!prj || !prj.default_server_id) return false;
    var srv = findById(st.cfg.servers, prj.default_server_id);
    if (!srv) return false;
    var srvSel = document.getElementById('deploy-server');
    if (!srvSel || srvSel.value === String(srv.id)) return false;
    srvSel.value = String(srv.id);
    savePref(PREF_SERVER_KEY, srv.id);
    return true;
  }

  /**
   * 项目下拉下方的提示:说明当前项目的部署目录来源(项目级 / 继承服务器)。
   * 让"这个项目部署到哪"在选完就能看到,不必去服务器配置里核对。
   */
  function updateProjectHint() {
    var hint = document.getElementById('deploy-project-hint');
    if (!hint || !st.cfg) return;
    var prjSel = document.getElementById('deploy-project');
    var prj = (prjSel && prjSel.value) ? findById(st.cfg.projects, prjSel.value) : null;
    if (!prj) {
      hint.textContent = '在「服务器管理」页维护';
      return;
    }
    if (prj.remote_dir) {
      hint.textContent = '部署目录:' + String(prj.remote_dir) + '(项目独立目录)';
      return;
    }
    var srvSel = document.getElementById('deploy-server');
    var srv = (srvSel && srvSel.value) ? findById(st.cfg.servers, srvSel.value) : null;
    if (srv) {
      hint.textContent = '部署目录:' + String(srv.remote_dir || '?') +
        '(继承服务器「' + String(srv.name || srv.host) + '」)';
    } else {
      hint.textContent = '部署目录继承所选服务器,选择服务器后显示';
    }
  }

  // ===== 页面数据加载(每次进入都拉取)=====

  function loadPageData() {
    if (st.loading) return;
    st.loading = true;
    hideErrorBox();

    var errs = [];

    var imgReq = window.AppBus.invoke('list_images')
      .then(function (list) {
        st.images = filterUsable(list);
      })
      .catch(function (err) {
        errs.push('加载镜像列表失败:' + (errText(err) || '未知错误'));
      });

    var cfgReq = window.AppBus.invoke('get_config')
      .then(function (cfg) {
        st.cfg = normalizeCfg(cfg);
      })
      .catch(function (err) {
        st.cfg = normalizeCfg(null);
        errs.push('读取配置失败:' + (errText(err) || '未知错误'));
      });

    Promise.all([imgReq, cfgReq]).then(function () {
      st.loading = false;
      renderSelects();
      refreshControls();
      if (errs.length > 0) {
        showErrorBox(errs, false);
      }
      applyPendingImage();
      // 进入页面(pagechange 触发本函数)后按当前选中服务器+项目重查断点横幅;
      // 放在 renderSelects 之后,保证查询用的是恢复后的选中项
      refreshResumeStatus();
      // 整栈模式:已选项目与已解析结果不一致(或尚无解析结果)时自动解析
      if (st.mode === 'stack') {
        var prjSel = document.getElementById('deploy-project');
        var projectId = prjSel ? String(prjSel.value) : '';
        if (projectId && st.stackProjectId !== projectId) parseStack();
      }
    });
  }

  // ===== 预填:镜像页「部署」按钮带入的待部署镜像 =====

  function applyPendingImage() {
    var pending = window.__pendingDeployImage;
    if (!pending) return;
    window.__pendingDeployImage = null; // 用后即删

    var ref = String(pending.repository) + ':' + String(pending.tag);
    var sel = document.getElementById('deploy-image');
    if (!sel) return;
    for (var i = 0; i < sel.options.length; i++) {
      if (sel.options[i].value === ref) {
        sel.value = ref;
        return;
      }
    }
    // 下拉里找不到(悬空镜像被过滤 / 列表已变化 / 加载失败):提示并忽略
    window.toast('所选镜像已不在列表中', 'warn');
  }

  // ===== 控件状态(开始 / 取消 / 下拉禁用)=====

  function refreshControls() {
    var start = document.getElementById('deploy-start-btn');
    var cancel = document.getElementById('deploy-cancel-btn');
    // 批量活跃标志(先于所有使用点计算;此前声明在使用点之后,依赖 var 提升
    // 读到 undefined,批量期间「开始部署」文案与批量入口禁用全部失效)
    var batchActive = !!(st.batch && st.batch.active);

    if (start) {
      // 第六批:四个状态统一走共享助手 —— 部署中/检测中/批量中带步进条,
      // 空闲态复位(会清掉步进条子节点并解除禁用)。环境闸门不在此处判定:
      // 未通过宿主机检测时由 showPage 的 LOCKED_PAGES 在进页时拦截,行为与
      // 改动前一致(此处只负责按运行时状态切换文案与禁用)。
      var busy = batchActive || st.deploying || st.checking;
      var label = batchActive ? '批量部署中…'
        : (st.deploying ? '部署中…' : (st.checking ? '检测中…' : '开始部署'));
      window.setBtnBusy(start, busy, label);
    }
    // 取消按钮:仅在部署中可用(起点 = 发起 deploy,终点 = deploy-done)
    if (cancel) cancel.disabled = !st.deploying;

    // 批量部署入口:批量/部署/预检期间禁用
    var batchBtn = document.getElementById('deploy-batch-btn');
    if (batchBtn) batchBtn.disabled = batchActive || st.deploying || st.checking;

    // 部署 / 预检 / 批量进行期间锁定选择区,避免中途改动造成误解
    ['deploy-image', 'deploy-server', 'deploy-project', 'deploy-date-tag',
      'deploy-skip-unchanged', 'deploy-stack-skip', 'deploy-stack-archive']
      .forEach(function (id) {
        var node = document.getElementById(id);
        if (node) node.disabled = st.deploying || st.checking || batchActive;
      });

    // 续传横幅按钮与部署互斥同步(部署 / 预检 / 放弃请求进行中一律禁用;
    // 横幅未显示或处于无按钮状态时元素不存在,静默跳过)
    ['deploy-resume-continue-btn', 'deploy-resume-discard-btn', 'deploy-resume-keep-btn']
      .forEach(function (id) {
        var node = document.getElementById(id);
        if (node) node.disabled = st.deploying || st.checking || st.resumeBusy;
      });

    // 「跳过未变化镜像」与「打日期标签」互斥:日期标签每次生成全新 tag,
    // 不存在「未变化」,勾选日期标签时置灰禁用并给出说明(后端同样不生效,双保险)
    var skipSingle = document.getElementById('deploy-skip-unchanged');
    var dateTagChk = document.getElementById('deploy-date-tag');
    if (skipSingle) {
      var gated = !!(dateTagChk && dateTagChk.checked);
      var batchActive2 = !!(st.batch && st.batch.active);
      skipSingle.disabled = st.deploying || st.checking || batchActive2 || gated;
      skipSingle.title = gated
        ? '日期标签模式每次均为全新标签,不存在「未变化」,该选项不适用'
        : '远端同标签镜像 ID 一致时跳过导出/上传/装载,部署更快';
    }

    // 模式切换 tab 与整栈面板按钮同步禁用(与现有并发防护一致;解析/预览中锁对应按钮)
    var locked = st.deploying || st.checking;
    ['deploy-mode-single', 'deploy-mode-stack',
      'deploy-mode-tabs', 'deploy-stack-parse-btn', 'deploy-stack-save-btn',
      'deploy-stack-preview-btn']
      .forEach(function (id) {
        var node = document.getElementById(id);
        if (!node) return;
        node.disabled = locked ||
          (id === 'deploy-stack-parse-btn' && st.parsing) ||
          (id === 'deploy-stack-preview-btn' && st.previewing);
      });
    // 迁移项目入口:与部署/预检/批量互斥;迁移执行中同样禁用
    var migBtn = document.getElementById('deploy-migrate-project-btn');
    if (migBtn) {
      migBtn.disabled = locked || batchActive || migState.active;
      migBtn.textContent = migState.active ? '迁移中…' : '迁移项目…';
    }
    var panel = document.getElementById('deploy-stack-panel');
    if (panel) {
      var panelBtns = panel.querySelectorAll('button');
      if (locked || st.parsing) {
        Array.prototype.forEach.call(panelBtns, function (btn) {
          btn.disabled = true;
        });
      } else if (st.stack) {
        // 解锁:按数据源重渲染,恢复每个切换按钮的固有禁用态
        // (无 image 字段的服务保持禁用;st.stack 为空时不重渲染,保留解析失败红框)
        renderStackPanel();
      }
    }
  }

  // ===== 预检结果条(5 项徽章 + 错误明细)=====

  function hideCheck() {
    var box = document.getElementById('deploy-check');
    if (box) {
      box.textContent = '';
      box.classList.add('hidden');
    }
  }

  /** 汇总未通过项名称(4 个布尔项 + 磁盘空间) */
  function collectFailures(report) {
    var r = report || {};
    var fails = [];
    if (!r.docker) fails.push('Docker');
    if (!r.compose) fails.push('Compose');
    if (!r.gzip) fails.push('gzip');
    if (!r.remote_dir_exists) fails.push('远程目录');
    var disk = Number(r.disk_free_gb);
    if (isFinite(disk) && disk < DISK_MIN_GB) fails.push('磁盘空间');
    return fails;
  }

  function renderCheck(report) {
    var box = document.getElementById('deploy-check');
    if (!box) return;
    var r = report || {};
    box.textContent = '';

    box.appendChild(el('div', 'server-check-title', '服务器环境预检结果'));

    var badges = el('div', 'server-check-badges');
    badges.appendChild(window.fillBadge(el('span'),
      r.docker ? 'ok' : 'fail',
      'Docker:' + (r.docker ? '通过' : '未通过')));
    badges.appendChild(window.fillBadge(el('span'),
      r.compose ? 'ok' : 'fail',
      'Compose:' + (r.compose ? '通过' : '未通过')));
    badges.appendChild(window.fillBadge(el('span'),
      r.gzip ? 'ok' : 'fail',
      'gzip:' + (r.gzip ? '通过' : '未通过')));
    badges.appendChild(window.fillBadge(el('span'),
      r.remote_dir_exists ? 'ok' : 'fail',
      '远程目录:' + (r.remote_dir_exists ? '存在' : '不存在')));

    var disk = Number(r.disk_free_gb);
    var diskText = isFinite(disk)
      ? '磁盘 ' + disk.toFixed(1) + ' GB'
      : '磁盘未知';
    badges.appendChild(window.fillBadge(el('span'),
      isFinite(disk) && disk >= DISK_MIN_GB ? 'ok' : 'warn',
      diskText));
    box.appendChild(badges);

    var errors = Array.isArray(r.errors) ? r.errors : [];
    if (errors.length > 0) {
      var errList = el('div', 'server-errors');
      errors.forEach(function (line) {
        errList.appendChild(el('div', 'server-error-line', line));
      });
      box.appendChild(errList);
    }

    box.classList.remove('hidden');
  }

  // ===== 进度条(双节点集:单镜像 5 节点 / 整栈 6 节点)=====

  /** 当前模式的节点数(deploy-done 时用 step > total 点亮全部完成态) */
  function stepCount() {
    var set = STEP_SETS[st.mode] || STEP_SETS.single;
    return set.names.length;
  }

  /**
   * 按当前模式重建进度节点(结构与 index.html 静态五节点一致:
   * .deploy-step > .deploy-step-box(.deploy-step-num + .deploy-step-tick>svg)
   * + .deploy-step-name + .deploy-step-en + .deploy-step-msg)。
   */
  function buildSteps(mode) {
    var wrap = document.getElementById('deploy-steps');
    if (!wrap) return;
    var set = STEP_SETS[mode] || STEP_SETS.single;
    wrap.textContent = '';
    set.names.forEach(function (name, i) {
      var node = el('div', 'deploy-step');
      node.id = 'deploy-step-' + (i + 1);

      var box = el('div', 'deploy-step-box');
      box.appendChild(el('span', 'deploy-step-num', ('0' + (i + 1)).slice(-2)));
      var tick = el('span', 'deploy-step-tick');
      tick.appendChild(window.appIcon('ok'));
      box.appendChild(tick);
      node.appendChild(box);

      node.appendChild(el('div', 'deploy-step-name', name));
      node.appendChild(el('div', 'deploy-step-en', set.ens[i] || ''));
      node.appendChild(el('div', 'deploy-step-msg'));
      wrap.appendChild(node);
    });
  }

  /**
   * 渲染进度(按当前模式节点集;编号与对勾图标由 buildSteps 承载,此处只切状态类)。
   * @param {number} step 当前步骤 1..N;0 表示重置(全部灰色待命)
   * @param {string} message 当前节点文案(deploy-progress 的 message)
   */
  function renderProgress(step, message) {
    var total = stepCount();
    for (var i = 1; i <= total; i++) {
      var node = document.getElementById('deploy-step-' + i);
      if (!node) continue;
      node.classList.toggle('done', step > i);
      node.classList.toggle('current', step === i);
      var msg = node.querySelector('.deploy-step-msg');
      if (msg) msg.textContent = step === i ? String(message || '') : '';
    }
    // 屏幕阅读器播报(pass 4):校准仪的视觉状态(done/current 类)SR 感知
    // 不到,写入 #deploy-progress-live(sr-only + aria-live=polite)低频文本;
    // 仅步骤边界更新(每步一次),高频 deploy-log 不播。0=重置,清空不播。
    var live = document.getElementById('deploy-progress-live');
    if (live) {
      if (step > 0) {
        var cur = document.getElementById('deploy-step-' + step);
        var nameEl = cur ? cur.querySelector('.deploy-step-name') : null;
        live.textContent = '步骤 ' + step + '/' + total + ':'
          + (String(message || '') || (nameEl ? nameEl.textContent : '') || '');
      } else {
        live.textContent = '';
      }
    }
  }

  // ===== 部署日志(自动滚底,上限 2000 行,右下角行计数)=====

  /** 更新日志面板右下角的行计数(元素缺失时静默跳过) */
  function renderLogCount() {
    var counter = document.getElementById('deploy-log-count');
    if (counter) counter.textContent = st.logs.length + ' 行';
  }

  function renderLog() {
    var body = document.getElementById('deploy-log');
    if (!body) return;
    body.textContent = st.logs.length > 0 ? st.logs.join('\n') : '(暂无日志)';
    body.scrollTop = body.scrollHeight;
    renderLogCount();
  }

  function appendLogLine(line) {
    var body = document.getElementById('deploy-log');

    // 追加前先判断是否在底部:用户向上翻看历史时不强制拉底
    var nearBottom = false;
    if (body) {
      nearBottom = body.scrollHeight - body.scrollTop - body.clientHeight < LOG_BOTTOM_GAP;
    }

    st.logs.push(line === null || line === undefined ? '' : String(line));
    if (st.logs.length > LOG_MAX_LINES) {
      st.logs.splice(0, st.logs.length - LOG_MAX_LINES);
    }

    if (body) {
      body.textContent = st.logs.join('\n');
      if (nearBottom) body.scrollTop = body.scrollHeight;
    }
    renderLogCount();
  }

  // ===== 模式切换(单镜像 / 整栈部署)=====

  /**
   * 切换部署模式:tab 激活态、单镜像专属格子显隐、整栈面板显隐、
   * 项目提示文案、进度节点集重建。部署 / 预检中禁止切换。
   */
  function setMode(mode) {
    if (st.deploying || st.checking) {
      window.toast('部署进行中,无法切换模式', 'warn');
      return;
    }
    if (st.mode === mode) return;
    st.mode = mode;

    var form = document.getElementById('deploy-form');
    if (form) form.classList.toggle('mode-stack', mode === 'stack');

    var tabSingle = document.getElementById('deploy-mode-single');
    var tabStack = document.getElementById('deploy-mode-stack');
    if (tabSingle) tabSingle.classList.toggle('active', mode === 'single');
    if (tabStack) tabStack.classList.toggle('active', mode === 'stack');
    // ARIA 状态同步:与 active 类保持一致(初始态见 index.html 的 aria-selected)
    if (tabSingle) tabSingle.setAttribute('aria-selected', String(mode === 'single'));
    if (tabStack) tabStack.setAttribute('aria-selected', String(mode === 'stack'));

    var panel = document.getElementById('deploy-stack-panel');
    if (panel) {
      panel.classList.toggle('hidden', mode !== 'stack');
      // pass 4(P4-F):整栈面板显示时纵向微型 wipe(同 manage Tab 面板,
      // 与 modal-wipe/page-reveal 同族裁切语言)
      if (!panel.classList.contains('hidden')) {
        panel.classList.remove('panel-wipe');
        void panel.offsetWidth;
        panel.classList.add('panel-wipe');
      }
    }

    var hint = document.getElementById('deploy-project-hint');
    if (hint) {
      hint.textContent = mode === 'stack'
        ? '选择项目后自动解析 compose 服务分类'
        : '在「服务器管理」页维护';
    }

    buildSteps(mode);
    // 切模式即重置本次运行视图:清横幅/预检条/错误框/日志,进度按新节点集归零
    resetRunView();
    // 断点续传横幅与页签无关(后端取同服务器+项目最近断点):重查恢复显示
    refreshResumeStatus();

    // 切到整栈:已选项目且尚未解析(或解析的是别的项目)时自动解析
    if (mode === 'stack') {
      var prjSel = document.getElementById('deploy-project');
      var projectId = prjSel ? String(prjSel.value) : '';
      if (projectId && st.stackProjectId !== projectId) parseStack();
    }
  }

  // ===== 整栈模式:服务分类(parse_compose → 分类表 → 默认分类写回)=====

  /** 匹配徽章:Exact→ok「已匹配」/ RepoOnly→warn「标签不一致」/ Missing→fail「本地不存在」 */
  function matchBadge(svc) {
    var kind = svc.match_state === 'Exact' ? 'ok'
      : (svc.match_state === 'RepoOnly' ? 'warn' : 'fail');
    var text = svc.match_state === 'Exact' ? '已匹配'
      : (svc.match_state === 'RepoOnly' ? '标签不一致' : '本地不存在');
    var badge = window.fillBadge(el('span'), kind, text);
    if (svc.warning) {
      badge.title = String(svc.warning);
    } else if (svc.match_state === 'RepoOnly') {
      badge.title = '本地标签与 compose 不一致';
    }
    return badge;
  }

  /** 传输方式切换按钮:文字按钮显示当前 mode;Local + has_build 附加 build 标记 */
  function transferButton(svc) {
    var btn = el('button', 'btn btn-sm',
      svc.mode === 'Local' ? '本地传输' : '服务器拉取');
    btn.type = 'button';
    if (svc.mode === 'Local' && svc.has_build) {
      btn.appendChild(el('span', 'transfer-build', 'build'));
    }
    if (!svc.image) {
      // 无 image 字段:无法由服务器拉取,锁定为本地传输
      btn.disabled = true;
      btn.title = 'compose 未设 image 字段,无法由服务器拉取,请保留本地传输或在 compose 补 image:';
    } else {
      btn.addEventListener('click', function () { toggleServiceMode(svc.service); });
      // 部署 / 预检 / 解析进行中一并禁用(refreshControls 解锁时按数据源恢复)
      btn.disabled = st.deploying || st.checking || st.parsing;
    }
    return btn;
  }

  /** 单个服务行:服务名(mono)/ 镜像(mono)/ 匹配徽章 / 传输方式切换 */
  function stackRow(svc) {
    var tr = document.createElement('tr');

    var tdSvc = document.createElement('td');
    tdSvc.className = 'mono';
    tdSvc.textContent = String(svc.service);
    tr.appendChild(tdSvc);

    var tdImg = document.createElement('td');
    tdImg.className = 'mono';
    if (svc.image) {
      tdImg.textContent = String(svc.image);
    } else {
      tdImg.appendChild(el('span', 'none-cjk', '(未设 image 字段)'));
    }
    tr.appendChild(tdImg);

    var tdMatch = document.createElement('td');
    tdMatch.appendChild(matchBadge(svc));
    tr.appendChild(tdMatch);

    var tdAct = document.createElement('td');
    tdAct.className = 'col-action';
    tdAct.appendChild(transferButton(svc));
    tr.appendChild(tdAct);

    return tr;
  }

  /** 服务行下方的非阻断警告小字行(warning 有值时渲染) */
  function stackWarnRow(warning) {
    var tr = document.createElement('tr');
    tr.className = 'stack-warn-row';
    var td = document.createElement('td');
    td.colSpan = 4;
    td.textContent = String(warning);
    tr.appendChild(td);
    return tr;
  }

  /** 渲染整栈面板:状态行 + errors 红框 + 服务分类表(st.stack 为数据源) */
  function renderStackPanel() {
    var tbody = document.getElementById('deploy-stack-tbody');
    var wrap = document.getElementById('deploy-stack-table-wrap');
    var errBox = document.getElementById('deploy-stack-errors');
    var status = document.getElementById('deploy-stack-status');
    if (!tbody || !wrap || !errBox || !status) return;

    tbody.textContent = '';
    errBox.textContent = '';

    var stack = st.stack;
    if (!stack) {
      wrap.classList.add('hidden');
      errBox.classList.add('hidden');
      status.textContent = '';
      return;
    }

    var project = findById(st.cfg ? st.cfg.projects : [], st.stackProjectId);
    var services = Array.isArray(stack.services) ? stack.services : [];
    var localCount = services.filter(function (s) { return s.mode === 'Local'; }).length;
    status.textContent = '项目「' + (project ? project.name : stack.project_name) + '」共 '
      + services.length + ' 个服务:本地传输 ' + localCount
      + ' / 服务器拉取 ' + (services.length - localCount);

    var errors = Array.isArray(stack.errors) ? stack.errors : [];
    if (errors.length > 0) {
      errBox.appendChild(el('div', 'servers-error-text',
        'compose 解析存在以下问题,修正后重新解析(未解决前无法开始部署):'));
      errors.forEach(function (line) {
        errBox.appendChild(el('div', 'servers-error-text', line));
      });
      errBox.classList.remove('hidden');
    } else {
      errBox.classList.add('hidden');
    }

    if (services.length === 0) {
      var tr = document.createElement('tr');
      var td = el('td', 'empty-cell', 'compose 未定义任何服务');
      td.colSpan = 4;
      tr.appendChild(td);
      tbody.appendChild(tr);
    }
    services.forEach(function (svc) {
      tbody.appendChild(stackRow(svc));
      if (svc.warning) tbody.appendChild(stackWarnRow(svc.warning));
    });

    wrap.classList.remove('hidden');
  }

  /** 切换单个服务的传输方式(Local ↔ Pull)并重渲染分类表 */
  function toggleServiceMode(serviceName) {
    if (st.deploying || st.checking || !st.stack) return;
    var services = Array.isArray(st.stack.services) ? st.stack.services : [];
    for (var i = 0; i < services.length; i++) {
      if (services[i].service === serviceName) {
        services[i].mode = services[i].mode === 'Local' ? 'Pull' : 'Local';
        break;
      }
    }
    renderStackPanel();
  }

  /** 解析所选项目的 compose(parse_compose);项目下拉选中时自动触发 */
  function parseStack() {
    if (st.deploying || st.checking || st.parsing) return;
    var prjSel = document.getElementById('deploy-project');
    var projectId = prjSel ? String(prjSel.value) : '';
    if (!projectId) {
      window.toast('请先选择部署项目', 'warn');
      return;
    }
    var project = findById(st.cfg ? st.cfg.projects : [], projectId);
    if (!project) {
      window.toast('所选项目已变化,请重新选择', 'warn');
      return;
    }

    st.parsing = true;
    refreshControls();
    var status = document.getElementById('deploy-stack-status');
    if (status) status.textContent = '正在解析 compose…';

    window.AppBus.invoke('parse_compose', { projectId: projectId })
      .then(function (stack) {
        st.stack = stack || { project_name: '', services: [], errors: [] };
        st.stackProjectId = projectId;
        renderStackPanel();
      })
      .catch(function (err) {
        st.stack = null;
        st.stackProjectId = '';
        renderStackPanel();
        var errBox = document.getElementById('deploy-stack-errors');
        if (errBox) {
          errBox.textContent = '';
          errBox.appendChild(el('div', 'servers-error-text',
            '解析失败:' + (errText(err) || '未知错误')));
          errBox.classList.remove('hidden');
        }
      })
      .then(function () {
        st.parsing = false;
        refreshControls();
      });
  }

  /** 「保存为默认分类」:当前 services 的 mode 组装为 service_overrides 写回项目 */
  function onSaveStackDefaults() {
    if (st.deploying || st.checking || st.parsing) return;
    if (!st.stack || !st.stackProjectId) {
      window.toast('请先解析服务分类', 'warn');
      return;
    }
    var services = Array.isArray(st.stack.services) ? st.stack.services : [];
    if (services.length === 0) {
      window.toast('当前解析结果没有任何服务,无需保存', 'warn');
      return;
    }
    var overrides = services.map(function (s) {
      return { service: String(s.service), mode: s.mode === 'Local' ? 'Local' : 'Pull' };
    });
    var projectId = st.stackProjectId;

    window.AppBus.invoke('get_config')
      .then(function (cfg) {
        cfg = normalizeCfg(cfg);
        var project = findById(cfg.projects, projectId);
        if (!project) throw new Error('项目已不存在,请刷新页面后重试');
        project.service_overrides = overrides;
        return window.AppBus.invoke('save_config_cmd', { cfg: cfg });
      })
      .then(function () {
        // 同步本地缓存,避免下次解析覆盖前显示过期分类
        var localProject = findById(st.cfg ? st.cfg.projects : [], projectId);
        if (localProject) localProject.service_overrides = overrides;
        window.toast('已保存为该项目默认分类', 'ok');
      })
      .catch(function (err) {
        window.toast('保存默认分类失败:' + (errText(err) || '未知错误'), 'fail');
      });
  }

  // ===== 部署预览(preview_stack_changes:独立 dry-run,不影响开始部署)=====

  /** 隐藏预览结果区(切换项目/服务器后旧快照不再展示) */
  function hidePreviewBox() {
    var box = document.getElementById('deploy-preview-box');
    if (box) box.classList.add('hidden');
  }

  /**
   * 预览变更徽章(ark 三态语言,与既有徽章同视觉):
   * Recreate→琥珀底「重建」/ Create→青淡底「新建」/ Unchanged→空心「不变」
   * / Pull→青淡底「拉取」/ Absent→墨底「缺失」(未知值按空心原样显示)。
   */
  function previewActionBadge(action) {
    var map = {
      Recreate: ['warn', '重建'],
      Create: ['ok', '新建'],
      Unchanged: ['info', '不变'],
      Pull: ['ok', '拉取'],
      Absent: ['fail', '缺失']
    };
    var hit = map[action];
    if (!hit) return window.fillBadge(el('span'), 'info', String(action));
    return window.fillBadge(el('span'), hit[0], hit[1]);
  }

  /** 渲染预览表:服务 / 镜像 / 变更徽章;errors 非阻断,墨线框逐条列出 */
  function renderStackPreview(preview) {
    var box = document.getElementById('deploy-preview-box');
    var tbody = document.getElementById('deploy-preview-tbody');
    var errBox = document.getElementById('deploy-preview-errors');
    if (!box || !tbody || !errBox) return;

    tbody.textContent = '';
    errBox.textContent = '';

    var errors = Array.isArray(preview.errors) ? preview.errors : [];
    if (errors.length > 0) {
      errBox.appendChild(el('div', 'servers-error-text', '预览存在以下问题(不阻断部署):'));
      errors.forEach(function (line) {
        errBox.appendChild(el('div', 'servers-error-text', line));
      });
      errBox.classList.remove('hidden');
    } else {
      errBox.classList.add('hidden');
    }

    var entries = Array.isArray(preview.entries) ? preview.entries : [];
    if (entries.length === 0 && errors.length === 0) {
      var emptyTr = document.createElement('tr');
      var emptyTd = el('td', 'empty-cell', '没有可预览的服务变更');
      emptyTd.colSpan = 3;
      emptyTr.appendChild(emptyTd);
      tbody.appendChild(emptyTr);
    }
    entries.forEach(function (entry) {
      var e = entry || {};
      var tr = document.createElement('tr');

      var tdSvc = document.createElement('td');
      tdSvc.className = 'mono';
      tdSvc.textContent = String(e.service || '');
      tr.appendChild(tdSvc);

      var tdImg = document.createElement('td');
      tdImg.className = 'mono';
      tdImg.textContent = String(e.image || '');
      tr.appendChild(tdImg);

      var tdAct = document.createElement('td');
      tdAct.appendChild(previewActionBadge(e.action));
      tr.appendChild(tdAct);

      tbody.appendChild(tr);
    });

    box.classList.remove('hidden');
  }

  /** 「部署预览」:对比本地 compose 与远端实际状态;预览期间按钮禁用 */
  function runStackPreview() {
    if (st.deploying || st.checking || st.parsing || st.previewing) return;
    var srvSel = document.getElementById('deploy-server');
    var prjSel = document.getElementById('deploy-project');
    var serverId = srvSel ? String(srvSel.value) : '';
    var projectId = prjSel ? String(prjSel.value) : '';

    var missing = [];
    if (!projectId) missing.push('项目');
    if (!serverId) missing.push('服务器');
    if (missing.length > 0) {
      window.toast('请先选择:' + missing.join('、'), 'warn');
      return;
    }
    if (!findById(st.cfg ? st.cfg.projects : [], projectId)) {
      window.toast('所选项目已变化,请重新选择', 'warn');
      return;
    }

    st.previewing = true;
    refreshControls();
    var btn = document.getElementById('deploy-stack-preview-btn');
    if (btn) btn.textContent = '预览中…';
    hidePreviewBox();

    // 密码用后端已存密文,不传 passwordPlain
    window.AppBus.invoke('preview_stack_changes',
        { serverId: serverId, projectId: projectId })
      .then(function (preview) {
        renderStackPreview(preview || { entries: [], errors: [] });
      })
      .catch(function (err) {
        // 连接失败等 invoke 级错误:就地展示在预览错误框(与解析失败同风格)
        renderStackPreview({
          entries: [],
          errors: ['预览失败:' + (errText(err) || '未知错误')]
        });
      })
      .then(function () {
        st.previewing = false;
        if (btn) btn.textContent = '部署预览';
        refreshControls();
      });
  }

  // ===== 部署流程 =====

  /** 每次点击「开始部署」:清空横幅 / 错误框 / 预检条 / 日志,进度重置 */
  function resetRunView() {
    hideBanner();
    // 续传横幅一并隐藏:横幅只按当前选中服务器+项目展示,此处发起的新部署
    // 即为同键(后端断点会被新部署逐步覆盖)
    hideResumeBanner();
    hideErrorBox();
    hideCheck();
    st.logs = [];
    renderLog();
    renderProgress(0, '');
  }

  function onStartDeploy() {
    if (st.deploying || st.checking) return; // 并发防护:部署 / 预检中不得再次发起
    if (st.mode === 'stack') {
      onStartStackDeploy();
      return;
    }

    var imgRef = '';
    var serverId = '';
    var projectId = '';
    var imgSel = document.getElementById('deploy-image');
    var srvSel = document.getElementById('deploy-server');
    var prjSel = document.getElementById('deploy-project');
    if (imgSel) imgRef = String(imgSel.value);
    if (srvSel) serverId = String(srvSel.value);
    if (prjSel) projectId = String(prjSel.value);

    // 三项都必须已选
    var missing = [];
    if (!imgRef) missing.push('镜像');
    if (!serverId) missing.push('服务器');
    if (!projectId) missing.push('项目');
    if (missing.length > 0) {
      window.toast('请先选择:' + missing.join('、'), 'warn');
      return;
    }

    var img = findImageByRef(imgRef);
    var server = findById(st.cfg ? st.cfg.servers : [], serverId);
    var project = findById(st.cfg ? st.cfg.projects : [], projectId);
    if (!img || !server || !project) {
      window.toast('所选数据已变化,请重新进入页面后选择', 'warn');
      return;
    }

    resetRunView();

    // 预检:server_env_check(密码用后端已存密文,不传 passwordPlain)
    st.checking = true;
    refreshControls();

    window.AppBus.invoke('server_env_check', { serverId: serverId })
      .then(function (report) {
        renderCheck(report);
        var fails = collectFailures(report);
        if (fails.length > 0) {
          showErrorBox([
            '服务器环境未通过检测(未通过:' + fails.join('、') + '),请先到服务器管理页处理'
          ], true);
          return;
        }
        window.toast('环境检测通过,开始部署', 'ok');
        startDeploy(img, server, project);
      })
      .catch(function (err) {
        showErrorBox(['服务器预检失败:' + (errText(err) || '未知错误')], true);
      })
      .then(function () {
        st.checking = false;
        refreshControls();
      });
  }

  /** 发起部署(req 字段必须 snake_case);成功后等待 deploy-done 事件收尾 */
  function startDeploy(img, server, project) {
    var dateTag = document.getElementById('deploy-date-tag');
    var skipChk = document.getElementById('deploy-skip-unchanged');
    var useDateTag = !!(dateTag && dateTag.checked);
    var req = {
      image: String(img.repository) + ':' + String(img.tag),
      repository: String(img.repository),
      server_id: String(server.id),
      project_id: String(project.id),
      use_date_tag: useDateTag,
      // 智能传输(勾选框已随日期标签联动禁用,此处按条件再兜底一次)
      skip_unchanged: !!(skipChk && skipChk.checked && !useDateTag),
      password_plain: null
    };

    st.deploying = true;
    refreshControls();
    renderProgress(0, '');

    window.AppBus.invoke('deploy', { req: req })
      .catch(function (err) {
        // invoke 本身失败:部署未真正启动,立即还原控件
        st.deploying = false;
        refreshControls();
        // 批量模式:没有 deploy-done,必须就地收尾该台,否则批量循环挂起
        if (st.batch && st.batch.active && st.batch.deferred) {
          var deferred = st.batch.deferred;
          st.batch.deferred = null;
          deferred({ success: false, message: '发起部署失败:' + (errText(err) || '未知错误') });
          return;
        }
        showErrorBox(['发起部署失败:' + (errText(err) || '未知错误')], false);
      });
  }

  // ===== 整栈部署流程(预检复用,管线走 deploy_stack)=====

  /** 整栈开始部署:项目/服务器校验 → 分类表校验(errors 阻断)→ 预检 → deploy_stack */
  function onStartStackDeploy() {
    var srvSel = document.getElementById('deploy-server');
    var prjSel = document.getElementById('deploy-project');
    var serverId = srvSel ? String(srvSel.value) : '';
    var projectId = prjSel ? String(prjSel.value) : '';

    var missing = [];
    if (!projectId) missing.push('项目');
    if (!serverId) missing.push('服务器');
    if (missing.length > 0) {
      window.toast('请先选择:' + missing.join('、'), 'warn');
      return;
    }

    var server = findById(st.cfg ? st.cfg.servers : [], serverId);
    var project = findById(st.cfg ? st.cfg.projects : [], projectId);
    if (!server || !project) {
      window.toast('所选数据已变化,请重新进入页面后选择', 'warn');
      return;
    }

    // 服务分类表必须已按当前项目解析
    if (!st.stack || st.stackProjectId !== projectId) {
      window.toast('请先解析项目服务分类', 'warn');
      parseStack();
      return;
    }
    var stack = st.stack;
    var services = Array.isArray(stack.services) ? stack.services : [];

    // compose errors 非空:红框已在面板显示,阻断开始部署
    if (Array.isArray(stack.errors) && stack.errors.length > 0) {
      window.toast('compose 存在未解决问题,无法开始部署(详见服务分类表上方)', 'fail');
      return;
    }
    if (services.length === 0) {
      window.toast('compose 未定义任何服务,无法部署', 'warn');
      return;
    }
    // 与后端 validate_stack_choices 对齐的前置校验:Local 类服务镜像引用必须非空
    for (var i = 0; i < services.length; i++) {
      var svc = services[i];
      if (svc.mode === 'Local' && !svc.image) {
        window.toast('服务「' + svc.service + '」未设 image 字段,无法本地传输,请先修正 compose',
          'fail');
        return;
      }
    }

    resetRunView();

    // 预检:server_env_check(密码用后端已存密文,不传 passwordPlain)
    st.checking = true;
    refreshControls();

    window.AppBus.invoke('server_env_check', { serverId: serverId })
      .then(function (report) {
        renderCheck(report);
        var fails = collectFailures(report);
        if (fails.length > 0) {
          showErrorBox([
            '服务器环境未通过检测(未通过:' + fails.join('、') + '),请先到服务器管理页处理'
          ], true);
          return;
        }
        window.toast('环境检测通过,开始整栈部署', 'ok');
        startStackDeploy(server, project);
      })
      .catch(function (err) {
        showErrorBox(['服务器预检失败:' + (errText(err) || '未知错误')], true);
      })
      .then(function () {
        st.checking = false;
        refreshControls();
      });
  }

  /** 发起整栈部署(req 字段 snake_case);成功后等待 deploy-done 事件收尾 */
  function startStackDeploy(server, project) {
    var services = Array.isArray(st.stack.services) ? st.stack.services : [];
    var skipChk = document.getElementById('deploy-stack-skip');
    var archChk = document.getElementById('deploy-stack-archive');
    // 版本标题/说明(第十一批):发起时快照输入(均可选,标题仅作展示备注,
    // 不影响归档时间戳命名);批量部署无逐台备注,恒不写
    var titleEl = document.getElementById('deploy-release-title');
    var notesEl = document.getElementById('deploy-release-notes');
    var notesTitle = titleEl ? String(titleEl.value).trim() : '';
    var notesBody = notesEl ? String(notesEl.value).trim() : '';
    if (st.batch && st.batch.active) {
      st.pendingReleaseNotes = null;
    } else if (notesTitle || notesBody) {
      st.pendingReleaseNotes = {
        serverId: String(server.id),
        projectId: String(project.id),
        title: notesTitle,
        body: notesBody
      };
    } else {
      st.pendingReleaseNotes = null;
    }
    var req = {
      project_id: String(project.id),
      server_id: String(server.id),
      // 前端分类表逐服务确认后的传输分类(image 缺失时传空串,仅允许 Pull 类)
      services: services.map(function (s) {
        return {
          service: String(s.service),
          image: s.image ? String(s.image) : '',
          mode: s.mode === 'Local' ? 'Local' : 'Pull'
        };
      }),
      // 智能传输:未变化服务跳过打包/上传/装载;强制留档时仍打包进 release 供回滚
      skip_unchanged: !!(skipChk && skipChk.checked),
      force_archive: !!(archChk && archChk.checked),
      password_plain: null
    };

    st.deploying = true;
    refreshControls();
    renderProgress(0, '');

    window.AppBus.invoke('deploy_stack', { req: req })
      .catch(function (err) {
        // invoke 本身失败:部署未真正启动,立即还原控件
        st.deploying = false;
        refreshControls();
        // 批量模式:没有 deploy-done,必须就地收尾该台,否则批量循环挂起
        if (st.batch && st.batch.active && st.batch.deferred) {
          var deferred = st.batch.deferred;
          st.batch.deferred = null;
          deferred({ success: false, message: '发起整栈部署失败:' + (errText(err) || '未知错误') });
          return;
        }
        showErrorBox(['发起整栈部署失败:' + (errText(err) || '未知错误')], false);
      });
  }

  function onCancelDeploy() {
    if (!st.deploying) return;
    window.AppBus.invoke('cancel_deploy')
      .then(function () {
        window.toast('取消请求已发送(将在当前步骤结束后生效)', 'info');
      })
      .catch(function (err) {
        window.toast('取消部署失败:' + (errText(err) || '未知错误'), 'fail');
      });
  }

  /** deploy-done:展示结束横幅并还原控件(横幅持续到下次开始部署) */
  function handleDone(payload) {
    var p = payload || {};
    var success = p.success === true;
    var message = p.message ? String(p.message) : '';
    // 一键回滚同样经 deploy-done 收尾(模态打开期间发起),文案区分回滚/部署
    var isRollback = st.rbKind !== '';

    // 批量部署:单台收尾只做记录与刷新(历史逐台落档),横幅/最终收尾交给批量循环
    if (st.batch && st.batch.active && st.batch.deferred) {
      var doneDeferred = st.batch.deferred;
      st.batch.deferred = null;
      st.deploying = false;
      st.pendingReleaseNotes = null; // 批量无逐台说明,清防残留污染后续单发
      refreshControls();
      refreshHistory();
      doneDeferred(p);
      return;
    }

    st.deploying = false;
    refreshControls();

    if (success) {
      renderProgress(stepCount() + 1, ''); // step > total:当前模式全部节点置为完成态
      showBanner('ok', isRollback ? '回滚完成' : '部署完成');
      window.toast(isRollback ? '回滚完成' : '部署完成', 'ok');
    } else if (message === '部署已取消') {
      showBanner('warn', '已取消');
    } else {
      showBanner('fail', (isRollback ? '回滚失败:' : '部署失败:') + (message || '未知错误'));
    }

    // 版本说明补写(第十一批):整栈部署(非回滚)成功且发起时填了说明 →
    // 历史已先于 deploy-done 落盘(后端 finish 顺序,第十一批同批调整),
    // 此处查最新成功记录取 release_dir 拆参补写。失败/取消不写(说明保留
    // 在输入框,断点续传成功后走同一入口补写)
    if (success && !isRollback && st.pendingReleaseNotes) {
      var notesCtx = st.pendingReleaseNotes;
      st.pendingReleaseNotes = null;
      writeReleaseNotes(notesCtx);
    }

    // 部署/回滚结束(成功/失败/取消均落历史)后刷新部署历史;
    // 并重查断点:失败/取消保留断点 → 横幅给出「从步骤 N 继续」,
    // 成功则断点已被后端清除,查询为空,横幅保持隐藏
    refreshHistory();
    refreshResumeStatus();
  }

  /**
   * 补写归档版本备注(第十一批):按发起时快照的 { serverId, projectId,
   * title, body },从部署历史取该服务器+项目最新的成功整栈记录,其
   * `release_dir` = `<项目目录>/releases/<时间戳>` 拆出 dir + ts,调
   * `rollback_set_release_notes` 原子写归档内 release-notes.json(标题仅作
   * 展示备注,归档目录名/时间戳不变)。写失败只 toast 警示(部署本身已
   * 成功,不回滚不重试,用户可在回滚中心版本详情里补写);历史缺记录/
   * 目录形态异常同样警示后放弃。
   */
  function writeReleaseNotes(ctx) {
    window.AppBus.invoke('get_history').then(function (records) {
      var rec = null;
      for (var i = 0; i < records.length; i++) {
        var r = records[i];
        if (r && r.mode === 'stack' && r.success === true && r.release_dir &&
            r.server_id === ctx.serverId && r.project_id === ctx.projectId) {
          rec = r; // 倒序 = 最新在前,首个匹配即本次发布
          break;
        }
      }
      var releaseDir = rec ? String(rec.release_dir) : '';
      var marker = '/releases/';
      var idx = releaseDir.lastIndexOf(marker);
      if (idx <= 0) {
        window.toast('版本说明未写入:未在部署历史中定位到本次发布归档', 'warn');
        return;
      }
      var dir = releaseDir.slice(0, idx);
      var ts = releaseDir.slice(idx + marker.length);
      window.AppBus.invoke('rollback_set_release_notes', {
        serverId: ctx.serverId,
        dir: dir,
        ts: ts,
        title: ctx.title || '',
        body: ctx.body || ''
      }).then(function () {
        window.toast('版本说明已写入发布归档(' + ts + ')', 'ok');
      }).catch(function (err) {
        window.toast('版本说明写入失败(部署本身已成功):' + errText(err), 'warn');
      });
    }).catch(function (err) {
      window.toast('版本说明写入失败(部署本身已成功):' + errText(err), 'warn');
    });
  }

  // ===== 批量部署(前端编排:按服务器队列串行调用单发部署,后端零改动)=====
  // - 复用全部单发能力:环境预检/智能传输/通知/历史/断点(checkpoint 按服务器+项目独立)
  // - 逐台状态经 deploy-batch 面板展示;deploy-done 由 handleDone 批量分支转交
  // - 取消当前台(部署已取消)或点「停止批量」→ 余台标记「已跳过」

  function batchVal(id) {
    var n = document.getElementById(id);
    return n ? String(n.value) : '';
  }

  function openBatchModal() {
    if (st.deploying || st.checking || (st.batch && st.batch.active)) return;
    var projectId = batchVal('deploy-project');
    var project = findById(st.cfg ? st.cfg.projects : [], projectId);
    if (!project) { window.toast('请先选择项目', 'warn'); return; }
    var img = null;
    if (st.mode === 'single') {
      img = findImageByRef(batchVal('deploy-image'));
      if (!img) { window.toast('请先选择镜像', 'warn'); return; }
    }
    if (st.mode === 'stack' &&
        (!Array.isArray(st.stack.services) || st.stack.services.length === 0)) {
      window.toast('整栈批量需要先完成服务分类(解析 compose)', 'warn');
      return;
    }
    if (!Array.isArray(st.cfg.servers) || st.cfg.servers.length < 2) {
      window.toast('服务器不足两台,无需批量部署', 'warn');
      return;
    }

    var body = document.getElementById('deploy-batch-modal-body');
    if (!body) return;
    body.innerHTML = '';
    var modeText = st.mode === 'stack' ? '整栈部署' : '单镜像部署';
    var hint = document.createElement('p');
    hint.className = 'confirm-msg';
    hint.textContent = '对选中的多台服务器串行执行' + modeText +
      '(项目:「' + (project.name || projectId) + '」),每台独立写历史、可回滚;' +
      '传输选项沿用当前页设置。';
    body.appendChild(hint);

    for (var i = 0; i < st.cfg.servers.length; i++) {
      var srv = st.cfg.servers[i];
      var row = document.createElement('label');
      row.className = 'deploy-checkbox';
      var chk = document.createElement('input');
      chk.type = 'checkbox';
      chk.setAttribute('data-batch-server', srv.id);
      chk.checked = true;
      row.appendChild(chk);
      var span = document.createElement('span');
      span.textContent = (srv.name || srv.id) + ' (' + (srv.host || '') + ')';
      row.appendChild(span);
      body.appendChild(row);
    }

    var actions = document.createElement('div');
    actions.className = 'modal-actions';
    var startBtn = document.createElement('button');
    startBtn.id = 'deploy-batch-start-btn';
    startBtn.className = 'btn btn-primary';
    startBtn.type = 'button';
    startBtn.textContent = '开始批量部署';
    startBtn.addEventListener('click', onBatchStart);
    actions.appendChild(startBtn);
    body.appendChild(actions);

    var modal = document.getElementById('deploy-batch-modal');
    if (modal) {
      modal.classList.remove('hidden');
      window.modalFocusOpen(modal);
    }
  }

  function closeBatchModal() {
    var modal = document.getElementById('deploy-batch-modal');
    if (modal) {
      modal.classList.add('hidden');
      window.modalFocusClose(modal);
    }
  }

  /**
   * 关闭批量模态的统一入口(关闭钮/遮罩/Esc 三处共用):
   * 阶段七曾引用未定义的 closeBatchModalSafe → ReferenceError 在 bindDeployEvents
   * 内抛出并中断 init,pagechange 监听(在 bindDeployEvents 之后注册)永不建立,
   * loadPageData 永不执行 —— 04 页三个下拉因此全空。统一收敛到本函数。
   */
  function closeBatchModalSafe() {
    closeBatchModal();
  }

  function onBatchStart() {
    var nodes = document.querySelectorAll('#deploy-batch-modal-body input[data-batch-server]');
    var ids = [];
    for (var i = 0; i < nodes.length; i++) {
      if (nodes[i].checked) ids.push(nodes[i].getAttribute('data-batch-server'));
    }
    if (ids.length === 0) { window.toast('请至少勾选一台服务器', 'warn'); return; }
    var projectId = batchVal('deploy-project');
    var project = findById(st.cfg ? st.cfg.projects : [], projectId);
    if (!project) { window.toast('所选项目已变化,请重试', 'warn'); return; }
    var img = st.mode === 'single' ? findImageByRef(batchVal('deploy-image')) : null;
    if (st.mode === 'single' && !img) { window.toast('所选镜像已变化,请重试', 'warn'); return; }

    var queue = [];
    for (var j = 0; j < ids.length; j++) {
      var srv = findById(st.cfg ? st.cfg.servers : [], ids[j]);
      if (srv) queue.push({ serverId: srv.id, server: srv, project: project, img: img });
    }
    if (queue.length === 0) { window.toast('没有可用的服务器', 'warn'); return; }

    closeBatchModal();
    st.batch = {
      active: true, mode: st.mode, queue: queue, idx: 0,
      success: 0, failed: 0, skipped: 0, aborted: false,
      deferred: null, results: []
    };
    renderBatchPanel();
    runBatchNext();
  }

  /** 当前台:环境预检 → 复用单发部署;结果经 deploy-done 批量分支转交 */
  function runBatchNext() {
    if (!st.batch || !st.batch.active) return;

    // 「停止批量」:把余下服务器逐台标记「已跳过」并收尾。
    //
    // 这里只停队列、**不中止当前台**:批量路径不落断点(见 wiki 06),
    // 半途取消会把该台留在「部分完成且无法续传」的状态;要立即中止当前台
    // 请用「取消部署」(那条路径会把当前台记为已跳过并同样停掉余台)。
    // 用 while 而非递归,避免队列较长时堆栈增长。
    if (st.batch.aborted) {
      while (st.batch.idx < st.batch.queue.length) {
        var pending = st.batch.queue[st.batch.idx];
        st.batch.results.push({
          serverName: pending && pending.server
            ? (pending.server.name || pending.server.id) : '',
          state: 'skipped',
          message: '已停止批量'
        });
        st.batch.skipped++;
        st.batch.idx++;
      }
      renderBatchPanel();
      finishBatch();
      return;
    }

    if (st.batch.idx >= st.batch.queue.length) { finishBatch(); return; }
    var item = st.batch.queue[st.batch.idx];
    renderBatchPanel();

    // 每台独立环境预检(与单发一致);失败计为该台 failed,不中断批量
    window.AppBus.invoke('server_env_check', { serverId: item.serverId })
      .then(function (report) {
        if (!st.batch || !st.batch.active) return;
        // 预检期间用户点了「停止批量」:不再发起本台部署(已停止的批量
        // 不该因为一个在途请求又启动一台)
        if (st.batch.aborted) { runBatchNext(); return; }
        var fails = collectFailures(report);
        if (fails.length > 0) {
          batchItemResult('failed', '环境检测未通过:' + fails.join('、'));
          return;
        }
        resetRunView();
        var deferred = {};
        deferred.promise = new Promise(function (resolve) { deferred.resolve = resolve; });
        st.batch.deferred = deferred;
        if (st.batch.mode === 'stack') startStackDeploy(item.server, item.project);
        else startDeploy(item.img, item.server, item.project);
        deferred.promise.then(function (payload) {
          if (!st.batch || !st.batch.active) return;
          var p = payload || {};
          var state;
          if (p.success === true) state = 'success';
          else if (p.message === '部署已取消') { state = 'skipped'; st.batch.aborted = true; }
          else state = 'failed';
          batchItemResult(state, p.message || '');
        });
      })
      .catch(function (err) {
        if (!st.batch || !st.batch.active) return;
        batchItemResult('failed', '环境预检失败:' + (errText(err) || '未知错误'));
      });
  }

  function batchItemResult(state, message) {
    if (!st.batch) return;
    if (state === 'success') st.batch.success++;
    else if (state === 'skipped') st.batch.skipped++;
    else st.batch.failed++;
    var item = st.batch.queue[st.batch.idx];
    st.batch.results.push({
      serverName: item && item.server ? (item.server.name || item.server.id) : '',
      state: state, message: message || ''
    });
    st.batch.idx++;
    renderBatchPanel();
    runBatchNext();
  }

  function finishBatch() {
    if (!st.batch) return;
    st.batch.active = false;
    // 释放可能仍在等待的 deferred:若某台部署的 deploy-done 因故未到,
    // 不清掉会让页面控件停在「部署中」的锁死状态(与 invoke 失败兜底同口径)
    if (st.batch.deferred) {
      var dangling = st.batch.deferred;
      st.batch.deferred = null;
      dangling({ success: false, message: '批量已结束' });
    }
    refreshControls();
    refreshResumeStatus();
    var summary = '批量部署结束:' + st.batch.success + ' 成功 / ' +
      st.batch.failed + ' 失败 / ' + st.batch.skipped + ' 跳过';
    showBanner(st.batch.failed > 0 ? 'warn' : 'ok', summary);
    window.toast(summary, st.batch.failed > 0 ? 'warn' : 'ok');
    renderBatchPanel();
  }

  function onStopBatch() {
    if (!st.batch || !st.batch.active) return;
    st.batch.aborted = true;
    // 当前台仍在部署中:等它跑完(批量不落断点,半途取消会留下无法续传的
    // 半成品);仅预检中或已无在途部署时,立即把余台标为跳过并收尾。
    if (st.deploying) {
      window.toast('当前服务器部署完成后,余下服务器将跳过', 'info');
      renderBatchPanel();
      return;
    }
    renderBatchPanel();
    runBatchNext();
  }

  function renderBatchPanel() {
    var panel = document.getElementById('deploy-batch-panel');
    if (!panel) return;
    if (!st.batch) { panel.classList.add('hidden'); return; }
    panel.classList.remove('hidden');
    panel.innerHTML = '';

    var head = document.createElement('div');
    head.className = 'batch-head';
    var total = st.batch.queue.length;
    var title = document.createElement('strong');
    title.textContent = '批量部署(' + (st.batch.mode === 'stack' ? '整栈' : '单镜像') + ' · ' +
      st.batch.idx + '/' + total + ')';
    head.appendChild(title);
    var stat = document.createElement('span');
    stat.className = 'batch-stat';
    stat.textContent = st.batch.success + ' 成功 / ' + st.batch.failed + ' 失败 / ' +
      st.batch.skipped + ' 跳过';
    head.appendChild(stat);
    if (st.batch.active) {
      var stopBtn = document.createElement('button');
      stopBtn.id = 'deploy-batch-stop-btn';
      stopBtn.className = 'btn btn-sm';
      stopBtn.type = 'button';
      stopBtn.textContent = st.batch.aborted ? '停止中(当前台完成后停止)' : '停止批量';
      stopBtn.disabled = st.batch.aborted;
      stopBtn.addEventListener('click', onStopBatch);
      head.appendChild(stopBtn);
    }
    panel.appendChild(head);

    for (var i = 0; i < st.batch.queue.length; i++) {
      var item = st.batch.queue[i];
      var row = document.createElement('div');
      row.className = 'batch-row';
      var name = document.createElement('span');
      name.textContent = (item.server.name || item.server.id) + ' (' + (item.server.host || '') + ')';
      row.appendChild(name);
      var badge = document.createElement('span');
      var state, kind;
      if (i < st.batch.idx) {
        var res = st.batch.results[i];
        if (res) {
          // pass 4:成功/失败改用全站徽章口径(ok=青淡底 / fail=墨底),
          // 原先错用容器状态类 badge-running/badge-paused —— 「成功」与
          // 「部署中」同款、「失败」与「暂停」同款,语义错位
          if (res.state === 'success') { state = '成功'; kind = 'ok'; }
          else if (res.state === 'skipped') { state = '已跳过'; kind = 'info'; }
          else { state = '失败'; kind = 'fail'; }
          if (res.message) row.title = res.message;
        } else { state = '—'; kind = 'info'; }
      } else if (i === st.batch.idx && st.batch.active) {
        state = '部署中'; kind = 'running';
      } else {
        state = '等待'; kind = 'info';
      }
      if (kind === 'running') {
        badge.className = 'badge badge-running';
        badge.textContent = state;
      } else {
        window.fillBadge(badge, kind, state);
      }
      row.appendChild(badge);
      panel.appendChild(row);
    }
  }

  // ===== 部署历史(get_history:折叠面板,进入页面自动刷新一次)=====

  function setHistoryOpen(open) {
    var body = document.getElementById('deploy-history-body');
    var btn = document.getElementById('deploy-history-toggle');
    if (!body || !btn) return;
    body.classList.toggle('hidden', !open);
    btn.textContent = open ? '− 部署历史' : '+ 部署历史';
    if (open && !st.historyLoaded && !st.historyLoading) refreshHistory();
  }

  /** 拉取部署历史(倒序,最新在前);失败就地显示在表体空行 */
  function refreshHistory() {
    if (st.historyLoading) return;
    st.historyLoading = true;
    var btn = document.getElementById('deploy-history-refresh-btn');
    if (btn) btn.disabled = true;

    window.AppBus.invoke('get_history')
      .then(function (records) {
        st.history = Array.isArray(records) ? records : [];
        st.historyLoaded = true;
        renderHistory();
      })
      .catch(function (err) {
        st.history = [];
        st.historyLoaded = true;
        renderHistory('读取部署历史失败:' + (errText(err) || '未知错误'));
      })
      .then(function () {
        st.historyLoading = false;
        if (btn) btn.disabled = false;
      });
  }

  /** 模式徽章:单镜像 / 整栈 / 回滚(分类信息,统一用空心信息徽章) */
  function historyModeBadge(mode) {
    var m = String(mode);
    var text = m === 'stack' ? '整栈' : (m === 'rollback' ? '回滚' : '单镜像');
    return window.fillBadge(el('span'), 'info', text);
  }

  /** 结果徽章:成功→badge-ok / 取消→badge-warn / 失败→badge-fail(title 带结果消息) */
  function historyResultBadge(rec) {
    var message = String(rec.message || '');
    var badge;
    if (rec.success === true) {
      badge = window.fillBadge(el('span'), 'ok', '成功');
    } else if (message === '部署已取消') {
      badge = window.fillBadge(el('span'), 'warn', '取消');
    } else {
      badge = window.fillBadge(el('span'), 'fail', '失败');
    }
    badge.title = message || (rec.success === true ? '部署完成' : '未知错误');
    return badge;
  }

  /** 镜像列文本:join ", ";超长截断,完整内容放 title */
  function historyImagesText(images) {
    var full = (Array.isArray(images) ? images : []).map(String).join(', ');
    if (!full) return { text: '(空)', title: '' };
    if (full.length > HISTORY_IMAGES_MAX) {
      return { text: full.slice(0, HISTORY_IMAGES_MAX - 1) + '…', title: full };
    }
    return { text: full, title: full };
  }

  /** 耗时格式化:>60 秒显示「m 分 s 秒」,否则「N 秒」(异常值显示 -) */
  function formatDuration(secs) {
    var n = Number(secs);
    if (!isFinite(n) || n < 0) return '-';
    if (n > 60) {
      return Math.floor(n / 60) + ' 分 ' + (n % 60) + ' 秒';
    }
    return n + ' 秒';
  }

  /**
   * 渲染部署历史表:上限 50 条 + 计数;
   * 传入 errMsg 时(读取失败)以空行形式就地展示错误。
   */
  function renderHistory(errMsg) {
    var tbody = document.getElementById('deploy-history-tbody');
    var count = document.getElementById('deploy-history-count');
    if (!tbody || !count) return;
    tbody.textContent = '';

    if (errMsg) {
      count.textContent = '';
      var errTr = document.createElement('tr');
      var errTd = el('td', 'empty-cell', errMsg);
      errTd.colSpan = 7;
      errTr.appendChild(errTd);
      tbody.appendChild(errTr);
      return;
    }

    var records = st.history;
    count.textContent = records.length + ' 条记录' +
      (records.length > HISTORY_MAX_ROWS
        ? '(显示最新 ' + HISTORY_MAX_ROWS + ' 条)' : '');

    if (records.length === 0) {
      var emptyTr = document.createElement('tr');
      var emptyTd = el('td', 'empty-cell', '暂无部署记录');
      emptyTd.colSpan = 7;
      emptyTr.appendChild(emptyTd);
      tbody.appendChild(emptyTr);
      return;
    }

    records.slice(0, HISTORY_MAX_ROWS).forEach(function (rec) {
      var r = rec || {};
      var tr = document.createElement('tr');

      var tdTs = document.createElement('td');
      tdTs.className = 'mono nowrap';
      tdTs.textContent = String(r.ts || '');
      tr.appendChild(tdTs);

      var tdMode = document.createElement('td');
      tdMode.appendChild(historyModeBadge(r.mode));
      tr.appendChild(tdMode);

      var tdTarget = document.createElement('td');
      tdTarget.textContent = String(r.project_name || '') + ' @ ' + String(r.server_name || '');
      tr.appendChild(tdTarget);

      var img = historyImagesText(r.images);
      var tdImages = document.createElement('td');
      tdImages.className = 'mono';
      tdImages.textContent = img.text;
      if (img.title && img.title !== img.text) tdImages.title = img.title;
      tr.appendChild(tdImages);

      var tdResult = document.createElement('td');
      tdResult.appendChild(historyResultBadge(r));
      tr.appendChild(tdResult);

      var tdCost = document.createElement('td');
      tdCost.className = 'mono nowrap';
      tdCost.textContent = formatDuration(r.duration_secs);
      tr.appendChild(tdCost);

      // 操作列:可回滚记录给「回滚」按钮;回滚产生的记录显示灰色 —(防回滚链)
      var tdAct = document.createElement('td');
      tdAct.className = 'col-action';
      if (String(r.mode) === 'rollback') {
        tdAct.appendChild(el('span', 'none-text', '—'));
      } else {
        var rbBtn = el('button', 'btn btn-sm', '回滚');
        rbBtn.type = 'button';
        rbBtn.title = '读取该记录服务器/项目的历史留档,回滚到指定时点';
        rbBtn.disabled = st.deploying || st.rbBusy;
        rbBtn.addEventListener('click', function () { openRollbackModal(r); });
        tdAct.appendChild(rbBtn);
      }
      tr.appendChild(tdAct);

      tbody.appendChild(tr);
    });
  }

  // ===== 事件监听(模块级守卫:只注册一次)=====

  function bindDeployEvents() {
    if (listenersBound) return;
    listenersBound = true;

    function warn(event, err) {
      if (window.console && console.warn) {
        console.warn('[deploy] ' + event + ' 事件监听注册失败:', err);
      }
    }

    window.AppBus.on('deploy-progress', function (event) {
      var p = (event && event.payload) || {};
      renderProgress(Number(p.step) || 0, String(p.message || ''));
    }).catch(function (err) { warn('deploy-progress', err); });

    window.AppBus.on('deploy-log', function (event) {
      var line = event ? event.payload : '';
      appendLogLine(line);
      appendRbLog(line); // 回滚执行期同步镜像到模态内日志区(非执行期为空操作)
    }).catch(function (err) { warn('deploy-log', err); });

    window.AppBus.on('deploy-done', function (event) {
      var payload = event ? event.payload : {};
      handleDone(payload);          // 互斥解除 + 横幅 + toast + refreshHistory
      handleRollbackDone(payload);  // 回滚模态打开时同步收尾(展示结果/解锁关闭)
    }).catch(function (err) { warn('deploy-done', err); });

    // 回滚模态:关闭钮 / 遮罩点击 / Esc(执行中统一被 closeRollbackModal 拦截;
    // 仅本模态可见时生效,不影响其他模态各自的 Esc 监听)
    var rbClose = document.getElementById('deploy-modal-close');
    if (rbClose) rbClose.addEventListener('click', closeRollbackModal);
    var rbOverlayNode = document.getElementById('deploy-modal');
    if (rbOverlayNode) {
      rbOverlayNode.addEventListener('click', function (e) {
        if (e.target === rbOverlayNode) closeRollbackModal();
      });
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && !rbOverlayNode.classList.contains('hidden')) {
          closeRollbackModal();
        }
      });
    }

    // 批量部署:入口按钮 + 配置模态关闭(关闭钮/遮罩/Esc,仅本模态可见时生效)
    var batchBtn = document.getElementById('deploy-batch-btn');
    if (batchBtn) batchBtn.addEventListener('click', openBatchModal);
    var batchClose = document.getElementById('deploy-batch-modal-close');
    if (batchClose) batchClose.addEventListener('click', closeBatchModalSafe);
    var batchOverlay = document.getElementById('deploy-batch-modal');
    if (batchOverlay) {
      batchOverlay.addEventListener('click', function (e) {
        if (e.target === batchOverlay) closeBatchModalSafe();
      });
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && !batchOverlay.classList.contains('hidden')) {
          closeBatchModalSafe();
        }
      });
    }

    // 项目迁移:入口按钮 + 模态关闭(关闭钮/遮罩/Esc;执行中拦截由
    // closeMigrateProjectModal 统一负责)
    var migEntry = document.getElementById('deploy-migrate-project-btn');
    if (migEntry) migEntry.addEventListener('click', openMigrateProjectModal);
    var migClose = document.getElementById('migrate-project-modal-close');
    if (migClose) migClose.addEventListener('click', closeMigrateProjectModal);
    var migOverlay = document.getElementById('migrate-project-modal');
    if (migOverlay) {
      migOverlay.addEventListener('click', function (e) {
        if (e.target === migOverlay) closeMigrateProjectModal();
      });
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && !migOverlay.classList.contains('hidden')) {
          closeMigrateProjectModal();
        }
      });
    }
  }

  // ===== 初始化 =====

  function init() {
    var start = document.getElementById('deploy-start-btn');
    if (start) {
      start.addEventListener('click', onStartDeploy);
    }
    var cancel = document.getElementById('deploy-cancel-btn');
    if (cancel) {
      cancel.addEventListener('click', onCancelDeploy);
    }

    // 模式切换 tab
    var tabSingle = document.getElementById('deploy-mode-single');
    if (tabSingle) {
      tabSingle.addEventListener('click', function () { setMode('single'); });
    }
    var tabStack = document.getElementById('deploy-mode-stack');
    if (tabStack) {
      tabStack.addEventListener('click', function () { setMode('stack'); });
    }

    // 整栈面板:解析(选中项目自动触发 + 按钮手动重解析)与保存默认分类
    var parseBtn = document.getElementById('deploy-stack-parse-btn');
    if (parseBtn) {
      parseBtn.addEventListener('click', parseStack);
    }
    var saveBtn = document.getElementById('deploy-stack-save-btn');
    if (saveBtn) {
      saveBtn.addEventListener('click', onSaveStackDefaults);
    }
    // 部署预览:独立 dry-run 按钮
    var previewBtn = document.getElementById('deploy-stack-preview-btn');
    if (previewBtn) {
      previewBtn.addEventListener('click', runStackPreview);
    }
    var prjSel = document.getElementById('deploy-project');
    if (prjSel) {
      prjSel.addEventListener('change', function () {
        hidePreviewBox(); // 项目变化后旧预览快照失效
        // 第四批:记住选择;并带出该项目的默认服务器(仍可手动改)
        savePref(PREF_PROJECT_KEY, prjSel.value);
        var switched = syncServerForProject(prjSel.value);
        // 服务器可能被带出:重建项目下拉以更新「本机」排序标注
        if (switched) {
          var srvSelNow = document.getElementById('deploy-server');
          fillSelect(prjSel, '请选择项目',
            projectOptionsFor(srvSelNow ? srvSelNow.value : ''), prjSel.value);
        }
        updateProjectHint();
        refreshResumeStatus(); // 项目变化后按新键重查断点横幅
        if (st.mode === 'stack') parseStack(); // 整栈模式:选中即自动解析
      });
    }
    var srvSel = document.getElementById('deploy-server');
    if (srvSel) {
      srvSel.addEventListener('change', function () {
        hidePreviewBox(); // 服务器变化后旧预览失效
        savePref(PREF_SERVER_KEY, srvSel.value);
        // 第四批:按新服务器重排项目下拉(属于它的排前并标注),保留当前选择
        var prjSelNow = document.getElementById('deploy-project');
        if (prjSelNow) {
          fillSelect(prjSelNow, '请选择项目', projectOptionsFor(srvSel.value), prjSelNow.value);
        }
        updateProjectHint();
        refreshResumeStatus(); // 服务器变化后按新键重查断点横幅
      });
    }

    // 「打日期标签」变化时联动「跳过未变化镜像」禁用态(互斥说明见 refreshControls)
    var dateTagChk = document.getElementById('deploy-date-tag');
    if (dateTagChk) {
      dateTagChk.addEventListener('change', refreshControls);
    }

    // 部署历史:折叠开关 + 手动刷新
    var histToggle = document.getElementById('deploy-history-toggle');
    if (histToggle) {
      histToggle.addEventListener('click', function () {
        var body = document.getElementById('deploy-history-body');
        setHistoryOpen(!!(body && body.classList.contains('hidden')));
      });
    }
    var histRefresh = document.getElementById('deploy-history-refresh-btn');
    if (histRefresh) {
      histRefresh.addEventListener('click', refreshHistory);
    }

    // 节点集与静态 HTML 一致化(单镜像 5 节点;切整栈时按 6 节点重建)
    buildSteps(st.mode);
    renderLog();
    renderProgress(0, '');
    refreshControls();
    bindDeployEvents(); // 常驻监听,内部有守卫防重复注册

    // 每次进入部署页都重新拉取镜像与配置(可能已变化),并处理预填镜像;
    // 同时自动刷新一次部署历史
    window.addEventListener('pagechange', function (e) {
      if (!e || !e.detail || e.detail.page !== 'deploy') return;
      loadPageData();
      refreshHistory();
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }

  // ===== 第十二批 JS 拆分桥接:共享设施暴露给拆出文件,入口转发 =====
  window.DeployKit = { st: st, el: el, findById: findById, appendLogLine: appendLogLine, DATE_TAG_RE: DATE_TAG_RE, LOG_MAX_LINES: LOG_MAX_LINES, refreshControls: refreshControls, refreshHistory: refreshHistory, renderHistory: renderHistory, handleDone: handleDone };
  var openRollbackModal = function () { return window.DeployRollback.openRollbackModal.apply(null, arguments); };
  var closeRollbackModal = function () { return window.DeployRollback.closeRollbackModal.apply(null, arguments); };
  var appendRbLog = function () { return window.DeployRollback.appendRbLog.apply(null, arguments); };
  var handleRollbackDone = function () { return window.DeployRollback.handleRollbackDone.apply(null, arguments); };

  // ===== 第十二批 JS 拆分桥接:共享设施暴露给拆出文件,入口转发 =====
  window.DeployKit = { st: st, el: el, findById: findById, migState: migState, refreshControls: refreshControls, loadPageData: loadPageData };
  var openMigrateProjectModal = function () { return window.DeployMigrate.openMigrateProjectModal.apply(null, arguments); };
  var closeMigrateProjectModal = function () { return window.DeployMigrate.closeMigrateProjectModal.apply(null, arguments); };
})();
