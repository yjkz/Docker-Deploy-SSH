/* ============================================================
 * deploy.js — 部署向导页逻辑(依赖 app.js 提供的全局工具)
 *
 * 后端命令(对象字段为 Rust snake_case 原样序列化,JS 参数名 camelCase):
 * - list_images() -> ImageInfo[] { repository, tag, size_bytes, created, id }
 * - get_config() -> AppConfig { servers: [{ id, name, ... }],
 *                               projects: [{ id, name, ... }] }
 * - server_env_check({ serverId })
 *     -> ServerCheckReport { docker, compose, gzip, remote_dir_exists,
 *                            disk_free_gb, errors: string[] }
 * - deploy({ req }) -> 同步返回 null,结果只经事件:
 *     req = { image, repository, server_id, project_id,
 *             use_date_tag, password_plain }   // 必须 snake_case
 * - parse_compose({ projectId }) -> ComposeStack
 *     ComposeStack = { project_name, services: StackService[], errors: string[] }
 *     StackService = { service, image, has_build, mode: "Local"|"Pull",
 *                      match_state: "Exact"|"RepoOnly"|"Missing",
 *                      local_tag, warning }(按项目解析,含 overrides 与本地镜像实时匹配)
 * - deploy_stack({ req }) -> 同步返回 null,结果只经事件:
 *     req = { project_id, server_id,
 *             services: [{ service, image, mode: "Local"|"Pull" }],
 *             password_plain }                 // 必须 snake_case
 * - cancel_deploy() -> 置位后端全局取消标志
 * - preview_stack_changes({ serverId, projectId, passwordPlain? }) -> StackPreview
 *     StackPreview = { entries: [{ service, image, mode: "Local"|"Pull",
 *                      action: "Recreate"|"Create"|"Unchanged"|"Pull"|"Absent" }],
 *                      errors: string[] }(独立 dry-run,只读不落盘)
 * - get_history() -> DeployRecord[](倒序 = 最新在前)
 *     DeployRecord = { ts, mode: "single"|"stack", server_name, project_name,
 *                      images: string[], success, message, duration_secs }
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
    historyLoading: false // 部署历史加载中(防重复请求)
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

  function errText(err) {
    if (typeof err === 'string') return err;
    if (err && err.message) return err.message;
    return '';
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

  // ===== 下拉渲染(每次进入页面重建;尽量保留原选中项)=====

  function fillSelect(select, placeholderText, options) {
    if (!select) return;
    var prev = select.value; // 重建后尽量恢复
    select.textContent = '';

    var ph = document.createElement('option');
    ph.value = '';
    ph.textContent = placeholderText;
    ph.disabled = true;
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

    fillSelect(srvSel, '请选择服务器',
      (st.cfg ? st.cfg.servers : []).map(function (s) {
        return { value: String(s.id), text: String(s.name) };
      }));
    fillSelect(prjSel, '请选择项目',
      (st.cfg ? st.cfg.projects : []).map(function (p) {
        return { value: String(p.id), text: String(p.name) };
      }));
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

    if (start) {
      if (st.deploying) {
        start.disabled = true;
        start.textContent = '部署中…';
      } else if (st.checking) {
        start.disabled = true;
        start.textContent = '检测中…';
      } else {
        start.disabled = false;
        start.textContent = '开始部署';
      }
    }
    // 取消按钮:仅在部署中可用(起点 = 发起 deploy,终点 = deploy-done)
    if (cancel) cancel.disabled = !st.deploying;

    // 部署 / 预检期间锁定选择区,避免中途改动造成误解
    ['deploy-image', 'deploy-server', 'deploy-project', 'deploy-date-tag']
      .forEach(function (id) {
        var node = document.getElementById(id);
        if (node) node.disabled = st.deploying || st.checking;
      });

    // 模式切换 tab 与整栈面板按钮同步禁用(与现有并发防护一致;解析/预览中锁对应按钮)
    var locked = st.deploying || st.checking;
    ['deploy-mode-single', 'deploy-mode-stack',
      'deploy-stack-parse-btn', 'deploy-stack-save-btn', 'deploy-stack-preview-btn']
      .forEach(function (id) {
        var node = document.getElementById(id);
        if (!node) return;
        node.disabled = locked ||
          (id === 'deploy-stack-parse-btn' && st.parsing) ||
          (id === 'deploy-stack-preview-btn' && st.previewing);
      });
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

    var panel = document.getElementById('deploy-stack-panel');
    if (panel) panel.classList.toggle('hidden', mode !== 'stack');

    var hint = document.getElementById('deploy-project-hint');
    if (hint) {
      hint.textContent = mode === 'stack'
        ? '选择项目后自动解析 compose 服务分类'
        : '在「服务器管理」页维护';
    }

    buildSteps(mode);
    // 切模式即重置本次运行视图:清横幅/预检条/错误框/日志,进度按新节点集归零
    resetRunView();

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
      tdImg.appendChild(el('span', 'none-text', '(未设 image 字段)'));
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
    var req = {
      image: String(img.repository) + ':' + String(img.tag),
      repository: String(img.repository),
      server_id: String(server.id),
      project_id: String(project.id),
      use_date_tag: !!(dateTag && dateTag.checked),
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

    st.deploying = false;
    refreshControls();

    if (success) {
      renderProgress(stepCount() + 1, ''); // step > total:当前模式全部节点置为完成态
      showBanner('ok', '部署完成');
      window.toast('部署完成', 'ok');
    } else if (message === '部署已取消') {
      showBanner('warn', '已取消');
    } else {
      showBanner('fail', '部署失败:' + (message || '未知错误'));
    }

    // 部署结束(成功/失败/取消均落历史)后刷新部署历史
    refreshHistory();
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

  /** 模式徽章:单镜像 / 整栈(分类信息,用空心信息徽章) */
  function historyModeBadge(mode) {
    return window.fillBadge(el('span'), 'info',
      String(mode) === 'stack' ? '整栈' : '单镜像');
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
      errTd.colSpan = 6;
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
      emptyTd.colSpan = 6;
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
      appendLogLine(event ? event.payload : '');
    }).catch(function (err) { warn('deploy-log', err); });

    window.AppBus.on('deploy-done', function (event) {
      handleDone(event ? event.payload : {});
    }).catch(function (err) { warn('deploy-done', err); });
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
        if (st.mode === 'stack') parseStack(); // 整栈模式:选中即自动解析
      });
    }
    var srvSel = document.getElementById('deploy-server');
    if (srvSel) {
      srvSel.addEventListener('change', hidePreviewBox); // 服务器变化后旧预览失效
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
})();
