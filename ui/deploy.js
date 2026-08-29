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
 * - cancel_deploy() -> 置位后端全局取消标志
 *
 * 事件(AppBus.on,模块级守卫保证只注册一次):
 * - 'deploy-progress' { step, total, message }(step 1..5)
 * - 'deploy-log'      string(带 [HH:MM:SS] 前缀的一行日志)
 * - 'deploy-done'     { success, message }(取消固定 message === "部署已取消")
 *
 * 交互约定:
 * - 部署中(deploy 起点至 deploy-done)「开始部署」禁用、「取消部署」可用,
 *   且不得再次发起 deploy(后端全局取消标志会在新部署开始时被重置)。
 * - 进入页面时重新拉取 list_images + get_config(镜像与配置可能变化);
 *   若 window.__pendingDeployImage 存在(镜像页「部署」按钮带入),自动选中
 *   对应下拉项,用后即删(置 null);找不到则 toast 提示并忽略。
 * - 日志区自动滚底,但用户向上滚动查看历史时不强制拉底
 *   (仅在 scrollTop 接近底部时才 autoscroll)。
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
  /** 五个进度节点名称(与后端 deploy-progress 步骤一一对应) */
  var STEP_NAMES = ['打标签', '导出压缩', '上传镜像', '同步文件', '服务器部署'];
  /** 部署日志距离底部多少像素以内视为「在底部」(才自动滚底) */
  var LOG_BOTTOM_GAP = 40;

  var st = {
    images: [],        // 过滤 <none> 后的可用镜像(ImageInfo[])
    cfg: null,         // get_config 的完整结果(AppConfig)
    loading: false,    // 页面数据加载中(list_images + get_config)
    checking: false,   // 部署前预检中(server_env_check)
    deploying: false,  // 部署中(invoke deploy 成功 → deploy-done)
    logs: []           // deploy-log 事件累积的日志行
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

  // ===== 进度条 =====

  /**
   * 渲染五步进度(编号 01..05 与对勾图标由 index.html 静态承载,此处只切状态类)。
   * @param {number} step 当前步骤 1..5;0 表示重置(全部灰色待命)
   * @param {string} message 当前节点文案(deploy-progress 的 message)
   */
  function renderProgress(step, message) {
    for (var i = 1; i <= STEP_NAMES.length; i++) {
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
      renderProgress(6, ''); // step > 5:全部 5 个节点置为完成态
      showBanner('ok', '部署完成');
      window.toast('部署完成', 'ok');
    } else if (message === '部署已取消') {
      showBanner('warn', '已取消');
    } else {
      showBanner('fail', '部署失败:' + (message || '未知错误'));
    }
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

    renderLog();
    renderProgress(0, '');
    refreshControls();
    bindDeployEvents(); // 常驻监听,内部有守卫防重复注册

    // 每次进入部署页都重新拉取镜像与配置(可能已变化),并处理预填镜像
    window.addEventListener('pagechange', function (e) {
      if (!e || !e.detail || e.detail.page !== 'deploy') return;
      loadPageData();
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
