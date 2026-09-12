/* ============================================================
 * app.js — 全站脚本(普通 script 加载,先于各页面脚本执行)
 *
 * 提供:
 * - window.showPage(name)         页面切换(section[data-page])
 * - window.AppState.hostOk        全局环境检测状态(镜像到 localStorage['dd_hostOk'])
 * - window.refreshNav()           根据 AppState.hostOk 刷新导航禁用态
 * - window.AppBus.invoke / on     Tauri 命令与事件的薄封装
 * - window.AppBus.pickPath(opts)  系统文件/目录选择对话框(tauri-plugin-dialog)
 *                                 → Promise<string|null>(null = 取消/不可用)
 * - window.toast(msg, type)       右下角 Toast(2.5 秒自动消失;ok/info=status
 *                                 礼貌播报,warn/fail=alert 断言播报)
 * - window.modalFocusOpen(el)     模态打开后调用:记录触发源 + 移焦入卡
 * - window.modalFocusClose(el)    模态关闭后调用:归还该模态触发源焦点
 *                                 (Tab 圈禁由本文件的 document 级监听承担,
 *                                 10 个模态共用;见「模态焦点管理」节)
 * - window.copyText(text)         复制文本到剪贴板(成功 toast「已复制」)
 * - window.errText(err)           错误值 → 可展示文本(全站统一口径,
 *                                 空值兜底「未知错误」,非 Error 对象转字符串)
 * - window.bindFieldValidation(formRoot, rules)
 *                                 失焦校验接线器(同份规则兼作提交前整体校验;
 *                                 必填类错误在首次提交后才提示,格式类恒提示)
 * - window.bindFormEnter(formRoot, onEnter)
 *                                 Enter 提交接线(仅单行文本 input;判 IME 组合;
 *                                 不越过二次确认 —— 确认视图不注册即可)
 * - window.beginForm(container, labelId)
 *                                 表单语义脚手架:清空容器并放入 <form
 *                                 novalidate>,拦下原生隐式提交转主入口
 * - window.toggleScheme(evt)      亮暗主题切换(View Transitions 圆形扩散揭示,
 *                                 持久化 localStorage['dd_scheme'])
 * ============================================================ */
(function () {
  'use strict';

  var HOST_OK_KEY = 'dd_hostOk';
  var SCHEME_KEY = 'dd_scheme';
  /** 环境检测未通过时禁用的页面(服务器管理页除外:配置编辑不依赖 Docker) */
  var LOCKED_PAGES = ['images', 'deploy'];

  // ===== 全局状态:hostOk 变更时自动镜像到 localStorage =====
  window.AppState = {};
  Object.defineProperty(window.AppState, 'hostOk', {
    enumerable: true,
    get: function () { return this._hostOk === true; },
    set: function (v) {
      this._hostOk = v === true;
      try {
        localStorage.setItem(HOST_OK_KEY, String(this._hostOk));
      } catch (e) { /* localStorage 不可用(隐私模式等)时忽略 */ }
    }
  });

  // 启动时从 localStorage 恢复(check.js 随后会用真实检测结果覆盖)
  (function restoreHostOk() {
    var saved = null;
    try { saved = localStorage.getItem(HOST_OK_KEY); } catch (e) { /* 忽略 */ }
    window.AppState.hostOk = saved === 'true';
  })();

  /** 上一次选择对话框所在的父目录(跨表单记忆,defaultPath 起点) */
  var lastPickDir = null;

  /** 取路径的父目录(D:/a/b/c.txt → D:/a/b);无分隔符(非绝对路径)返回 null */
  function dirnameOf(p) {
    var s = String(p);
    var idx = Math.max(s.lastIndexOf('\\'), s.lastIndexOf('/'));
    if (idx <= 0) return null;
    return s.slice(0, idx);
  }

  // ===== AppBus:Tauri 2 全局 API(window.__TAURI__)薄封装 =====
  window.AppBus = {
    /**
     * 调用 Rust 命令:AppBus.invoke('host_check')
     * @returns {Promise}
     */
    invoke: function (cmd, args) {
      var core = (window.__TAURI__ || {}).core;
      if (!core || typeof core.invoke !== 'function') {
        return Promise.reject(new Error('Tauri API 不可用,请在桌面窗口中运行'));
      }
      return core.invoke(cmd, args);
    },
    /**
     * 监听后端事件:AppBus.on('deploy-log', function (event) { ... })
     * @returns {Promise<Function>} resolve 出 unlisten 函数
     */
    on: function (event, handler) {
      var ev = (window.__TAURI__ || {}).event;
      if (!ev || typeof ev.listen !== 'function') {
        return Promise.reject(new Error('Tauri 事件 API 不可用,请在桌面窗口中运行'));
      }
      return ev.listen(event, handler);
    },
    /**
     * 系统文件/目录选择对话框(tauri-plugin-dialog,经全局对象 __TAURI__.dialog)。
     * @param {Object} [opts] { directory:boolean=false, filters:Array|null=null, title:string='选择文件' }
     * @returns {Promise<string|null>} 选中路径;取消/组件不可用/调用失败返回 null
     */
    pickPath: function (opts) {
      var options = opts || {};
      var dialog = (window.__TAURI__ || {}).dialog;
      // 防御:插件命名空间未挂载(非桌面环境 / withGlobalTauri 未暴露)时不抛异常
      if (!dialog || typeof dialog.open !== 'function') {
        window.toast('对话框组件不可用', 'fail');
        return Promise.resolve(null);
      }
      var openOpts = {
        multiple: false,
        directory: options.directory === true,
        title: options.title || '选择文件'
      };
      if (options.filters) openOpts.filters = options.filters;
      if (lastPickDir) openOpts.defaultPath = lastPickDir;
      return dialog.open(openOpts).then(function (picked) {
        // multiple=false 时返回 string | null(null = 用户取消)
        if (typeof picked !== 'string' || !picked) return null;
        var dir = dirnameOf(picked);
        if (dir) lastPickDir = dir; // 记住上次目录,下次对话框从这里开始
        return picked;
      }, function (err) {
        // 调用失败(权限缺失等)不静默:toast 提示后按取消处理
        window.toast('打开选择对话框失败:' + window.errText(err), 'fail');
        return null;
      });
    }
  };

  // ===== 页面切换 =====
  window.showPage = function (name) {
    var section = document.querySelector('section[data-page="' + name + '"]');
    if (!section) return;

    // 未通过环境检测时禁止进入被锁定的页面
    // (服务器管理页不在 LOCKED_PAGES 中:配置编辑不需要本机 Docker)
    if (LOCKED_PAGES.indexOf(name) !== -1 && !window.AppState.hostOk) {
      window.toast('环境检测未通过,请先完成环境检测', 'warn');
      return;
    }

    var i;
    var sections = document.querySelectorAll('section[data-page]');
    for (i = 0; i < sections.length; i++) {
      sections[i].classList.toggle('active', sections[i] === section);
    }

    // 切页动画:给新 section 挂 page-reveal 触发一次 clip-path masked reveal
    // (CSS animation 播完自动结束;先移除并强制 reflow 以便重复切换时可重触发)
    section.classList.remove('page-reveal');
    void section.offsetWidth;
    section.classList.add('page-reveal');

    var items = document.querySelectorAll('.dock-item');
    for (i = 0; i < items.length; i++) {
      var isActive = items[i].getAttribute('data-nav') === name;
      items[i].classList.toggle('active', isActive);
      // ARIA 当前页标记:与 active 类同步维护(当前项设 page,其余移除)
      if (isActive) items[i].setAttribute('aria-current', 'page');
      else items[i].removeAttribute('aria-current');
    }

    // 派发页面切换事件(window 上的 'pagechange',detail.page = 页面名),
    // 供各页面脚本在进入页面时执行加载(如镜像页首次自动加载镜像列表)。
    // 仅在切换成功后派发;被锁定页面在上方的提前 return 中不会到达此处。
    window.dispatchEvent(new CustomEvent('pagechange', { detail: { page: name } }));
  };

  // ===== 导航禁用态:hostOk=false 时其余 3 项灰置并带 tooltip =====
  window.refreshNav = function () {
    var ok = window.AppState.hostOk;
    LOCKED_PAGES.forEach(function (name) {
      var item = document.querySelector('.dock-item[data-nav="' + name + '"]');
      if (!item) return;
      if (ok) {
        item.classList.remove('disabled');
        item.removeAttribute('title');
        item.removeAttribute('aria-disabled');
      } else {
        item.classList.add('disabled');
        item.setAttribute('title', '环境检测未通过');
        item.setAttribute('aria-disabled', 'true');
      }
    });

    // 兜底:若当前停留在被锁定的页面,退回环境检测页
    if (!ok) {
      var active = document.querySelector('.dock-item.active');
      if (active && LOCKED_PAGES.indexOf(active.getAttribute('data-nav')) !== -1) {
        window.showPage('check');
      }
    }
  };

  // ===== Toast:右下角,2.5 秒自动消失 =====
  window.toast = function (message, type) {
    var container = document.getElementById('toast-container');
    if (!container) return;
    var kind = type || 'info';
    var el = document.createElement('div');
    el.className = 'toast toast-' + kind;
    // 屏幕阅读器播报(契约 toast-accessibility:不抢焦点 + aria-live):
    // 成功/信息走 status 礼貌播报;警示/失败走 alert 断言播报 —— 此前 toast
    // 是纯文本 div,操作结果对 SR 完全不可见(全仓唯一的 live region 只在
    // servers.js 的表单聚合错误框上)。
    if (kind === 'warn' || kind === 'fail') {
      el.setAttribute('role', 'alert');
    } else {
      el.setAttribute('role', 'status');
      el.setAttribute('aria-live', 'polite');
    }
    el.textContent = message;
    container.appendChild(el);
    window.requestAnimationFrame(function () {
      el.classList.add('toast-show');
    });
    window.setTimeout(function () {
      el.classList.remove('toast-show');
      window.setTimeout(function () { el.remove(); }, 300);
    }, 2500);
  };

  // ===== errText:错误值 → 可展示文本(全站统一口径)=====
  // 此前 8 个页面脚本各自内联一份等价实现(7 处空串兜底 + rollback 的
  // 「未知错误」版),现上收 app.js,取超集语义:空值兜底为「未知错误」,
  // 非 Error 对象转字符串 —— 避免 toast / 错误框出现空文案。
  window.errText = function (err) {
    if (!err) return '未知错误';
    if (typeof err === 'string') return err;
    if (err.message) return err.message;
    return String(err);
  };

  // ===== 呈现辅助:内联 SVG 图标 + 状态徽章(供各页面脚本构造 DOM)=====
  var SVG_NS = 'http://www.w3.org/2000/svg';

  /**
   * 构造引用 index.html 内 <symbol> 的内联 SVG 图标(check / cross / run)
   * @param {string} name 'ok' | 'x' | 'run'
   * @returns {SVGElement}
   */
  window.appIcon = function (name) {
    var svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('class', 'badge-ico');
    svg.setAttribute('aria-hidden', 'true');
    var use = document.createElementNS(SVG_NS, 'use');
    use.setAttribute('href', '#icon-' + name);
    svg.appendChild(use);
    return svg;
  };

  /**
   * 填充状态徽章:同步类名 + 前置图标 + 文本(kind 对应图标:ok=check fail=cross warn=run info=无)
   * @param {HTMLElement} node 徽章元素(通常为新建 span)
   * @param {string} kind 'ok' | 'fail' | 'warn' | 'info'
   * @param {string} text 徽章文字
   * @returns {HTMLElement} 传入的节点(便于链式 append)
   */
  window.fillBadge = function (node, kind, text) {
    if (!node) return node;
    node.className = 'badge' + (kind ? ' badge-' + kind : '');
    node.textContent = '';
    var icon = kind === 'ok' ? 'ok'
      : (kind === 'fail' ? 'x'
        : (kind === 'warn' ? 'run' : null));
    if (icon) node.appendChild(window.appIcon(icon));
    if (text !== undefined && text !== null) {
      node.appendChild(document.createTextNode(String(text)));
    }
    return node;
  };

  // ===== 按钮忙碌态(第六批:统一四处局部 setBusy 的能力差异)=====
  /**
   * 设置按钮的忙碌态:禁用 + 文案 + 步进条三合一。
   *
   * 此前全站忙碌态一律只是「禁用 + 改文案」,零图形反馈;且 settings/notify/
   * config-io/rollback 各自有一份 setBusy,能力还不一致(后两者不改文案)。
   * 本助手是唯一口径 —— 步进条由 CSS `.btn-busy-bar` 承载(用真实元素而非
   * ::after:伪元素已被小按钮命中区扩展占用)。
   *
   * @param {HTMLElement} btn 目标按钮
   * @param {boolean} busy true = 进入忙碌态
   * @param {string} [label] busy 期间显示的文案;省略则保留原文案
   * @returns {HTMLElement} 传入的按钮(便于链式)
   */
  window.setBtnBusy = function (btn, busy, label) {
    if (!btn) return btn;
    // 首次进入时记住原文案,便于调用方只传 busy 也能还原
    if (busy && btn.dataset.idleText === undefined) {
      btn.dataset.idleText = btn.textContent;
    }
    btn.classList.toggle('is-busy', !!busy);
    // 注意顺序:textContent 赋值会清空全部子节点(含步进条),故先写文案与
    // 状态,最后再挂步进条,避免"刚插入就被清掉再补插"的绕路逻辑。
    if (busy) {
      btn.disabled = true;
      btn.textContent = label || btn.dataset.idleText;
      var bar = document.createElement('span');
      bar.className = 'btn-busy-bar';
      bar.setAttribute('aria-hidden', 'true');
      btn.appendChild(bar);
    } else {
      btn.disabled = false;
      btn.textContent = label || btn.dataset.idleText;
      delete btn.dataset.idleText;
    }
    return btn;
  };

  // ===== 模态焦点管理(pass 4;契约:focus-management / keyboard-nav / escape-routes)=====
  // 约定:打开时移焦入卡(容器 tabindex=-1 落点,SR 报 dialog 名)、关闭时
  // 归还触发源、Tab 循环圈禁在卡内。此前 10 个模态只有 manage(关时还焦)与
  // help(开时移焦)各做了半套 —— aria-modal="true" 已声明而焦点仍留在触发
  // 按钮,屏幕阅读器既不知道对话框打开,关闭后也回不到上下文。
  // 栈式记录:cleanup-modal 可叠在 servers-modal 之上,逐层开关互不串位。
  var _modalFocusStack = [];

  /** 卡内可聚焦元素选择器(可见性由 Tab 圈禁处按 offsetParent 过滤) */
  var MODAL_FOCUSABLE = 'a[href], button:not([disabled]), '
    + 'input:not([disabled]):not([type="hidden"]), select:not([disabled]), '
    + 'textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

  /**
   * 模态打开后调用:记录触发源 + 移焦入卡(容器落点)。
   * @param {HTMLElement} overlay .modal-overlay 根节点
   */
  window.modalFocusOpen = function (overlay) {
    if (!overlay) return;
    var card = overlay.querySelector('.modal-card');
    if (!card) return;
    _modalFocusStack.push({
      card: card,
      trigger: (document.activeElement instanceof HTMLElement) ? document.activeElement : null
    });
    card.setAttribute('tabindex', '-1');
    card.focus({ preventScroll: true });
  };

  /**
   * 模态关闭后调用:归还该模态的触发源焦点(触发源已从 DOM 移除则跳过)。
   * @param {HTMLElement} [overlay] 模态根节点;省略按栈顶处理
   */
  window.modalFocusClose = function (overlay) {
    var idx = -1;
    if (overlay) {
      var card = overlay.querySelector('.modal-card');
      for (var i = _modalFocusStack.length - 1; i >= 0; i--) {
        if (_modalFocusStack[i].card === card) { idx = i; break; }
      }
    } else if (_modalFocusStack.length) {
      idx = _modalFocusStack.length - 1;
    }
    if (idx === -1) return;
    var rec = _modalFocusStack.splice(idx, 1)[0];
    if (rec.trigger && rec.trigger.isConnected) {
      rec.trigger.focus({ preventScroll: true });
    }
  };

  // Tab 圈禁:模态开启期间 Tab/Shift+Tab 在卡内循环(取 DOM 序最后的开启
  // 模态 = 视觉最上层,嵌套场景圈在最上层卡内)。容器落点(card 自身持有
  // 焦点)时 Tab 自然进第一个控件,无需特殊处理。
  document.addEventListener('keydown', function (e) {
    if (e.key !== 'Tab') return;
    var open = document.querySelectorAll('.modal-overlay:not(.hidden)');
    if (!open.length) return;
    var card = open[open.length - 1].querySelector('.modal-card');
    if (!card) return;
    var items = [];
    var all = card.querySelectorAll(MODAL_FOCUSABLE);
    for (var i = 0; i < all.length; i++) {
      // offsetParent===null 排除 .hidden 面板内的控件(模态内无 fixed 容器)
      if (all[i].offsetParent !== null) items.push(all[i]);
    }
    if (!items.length) return;
    var first = items[0];
    var last = items[items.length - 1];
    var active = document.activeElement;
    // 容器落点(card 自身持有焦点)视作「圈外」:Tab → 首个,Shift+Tab → 末个
    var inCard = active !== card && card.contains(active);
    if (e.shiftKey) {
      if (!inCard || active === first) {
        e.preventDefault();
        last.focus();
      }
    } else if (!inCard || active === last) {
      e.preventDefault();
      first.focus();
    }
  });

  // ===== 表单构建与错误处理(本轮统一;此前各模块各自手搓且能力不一)=====

  /**
   * 构造字段标签:中文主体 + 可选大写英文微标签 + 可选必填标记。
   *
   * 契约要求标签用「中文 + 大写英文微标签」双语(Space Grotesk、字距 .08-.18em),
   * 此前只有 notify 模块合规,服务器/项目/迁移等全部纯中文;把英文与中文拆成
   * 两个元素承载,才能各自满足字体与字距要求。
   *
   * @param {string} zh 中文标签主体
   * @param {string} [en] 英文微标签(自动大写);省略则只渲染中文
   * @param {boolean} [required] 是否必填(渲染信号色方块标记)
   * @param {string} [htmlFor] 关联的控件 id
   * @returns {HTMLLabelElement}
   */
  window.formLabel = function (zh, en, required, htmlFor) {
    var label = document.createElement('label');
    label.className = 'form-label';
    if (htmlFor) label.setAttribute('for', htmlFor);
    label.appendChild(document.createTextNode(String(zh)));
    if (en) {
      var enSpan = document.createElement('span');
      enSpan.className = 'form-label-en';
      enSpan.textContent = String(en);
      label.appendChild(enSpan);
    }
    if (required) {
      var mark = document.createElement('span');
      mark.className = 'form-label-req';
      // 必填对屏幕阅读器也要可读:方块是视觉标记,语义靠这段文本承载
      mark.setAttribute('aria-hidden', 'true');
      label.appendChild(mark);
      var sr = document.createElement('span');
      sr.className = 'sr-only';
      sr.textContent = '(必填)';
      label.appendChild(sr);
    }
    return label;
  };

  /** 表单分组小标题(中文 + 大写英文;样式见 .form-group-title) */
  window.formGroupTitle = function (zh, en) {
    var node = document.createElement('div');
    node.className = 'form-group-title';
    node.appendChild(document.createTextNode(String(zh)));
    if (en) {
      var enSpan = document.createElement('span');
      enSpan.className = 'form-label-en';
      enSpan.textContent = String(en);
      node.appendChild(enSpan);
    }
    return node;
  };

  /**
   * 表单失败三通道:内联错误框 + 滚动到可视区 + toast。
   *
   * 此前五个助手(formFail/formFailLoud/scrollErrorVisible/formClearError/
   * appendErrorBox)封闭在 servers.js 内,导致服务器表单不得不再写一份等价
   * 逻辑,而 notify/settings/manage 等 8 个表单干脆只有 toast —— 长表单里
   * toast 会先消失、顶部内联框又看不见。现上收为全局唯一口径。
   *
   * @param {string|HTMLElement} boxOrId 内联错误框的元素或 id
   * @param {string} msg 错误文案
   * @returns {boolean} 恒为 false(便于 `return window.formFailLoud(...)` 收尾)
   */
  window.formFailLoud = function (boxOrId, msg) {
    var box = typeof boxOrId === 'string'
      ? document.getElementById(boxOrId) : boxOrId;
    if (box) {
      box.textContent = msg;
      box.classList.remove('hidden');
      // 延迟 60ms:点击按钮会触发浏览器对按钮的原生焦点滚动,同步滚动会被其
      // 覆盖,延后一拍才能生效(servers.js 既有经验,此处保持同口径)。
      setTimeout(function () {
        if (box.scrollIntoView) {
          try { box.scrollIntoView({ block: 'nearest' }); }
          catch (_) { box.scrollIntoView(); }
        }
      }, 60);
    }
    window.toast(msg, 'fail');
    return false;
  };

  /** 清除内联错误框(表单重开/重新提交前调用) */
  window.formClearError = function (boxOrId) {
    var box = typeof boxOrId === 'string'
      ? document.getElementById(boxOrId) : boxOrId;
    if (box) {
      box.textContent = '';
      box.classList.add('hidden');
    }
  };

  /** 构造一个隐藏的内联错误框(表单顶部聚合错误用) */
  window.formErrorBox = function (id) {
    var box = document.createElement('div');
    box.className = 'form-error hidden';
    if (id) box.id = id;
    return box;
  };

  /**
   * 标记/清除字段级错误:把提示贴在字段下方,并给控件加错误态类。
   *
   * 契约要求「错误贴近字段并可通过 aria-describedby 关联」—— 此前全站只有
   * 表单顶部聚合框,没有任何字段级错误。本助手负责三件事:在字段行内插入
   * 提示、给控件加 .has-error(底边转信号色)、建立 aria-describedby 关联;
   * 传入 null/空消息即清除。
   *
   * @param {HTMLElement} control 出错的控件(须已插入 DOM)
   * @param {string|null} msg 错误文案;空则清除该字段的错误
   */
  window.setFieldError = function (control, msg) {
    if (!control || !control.parentNode) return;
    var errId = (control.id || 'field') + '-error';
    var existing = document.getElementById(errId);
    if (!msg) {
      control.classList.remove('has-error');
      control.removeAttribute('aria-invalid');
      control.removeAttribute('aria-describedby');
      if (existing && existing.parentNode) existing.parentNode.removeChild(existing);
      return;
    }
    control.classList.add('has-error');
    control.setAttribute('aria-invalid', 'true');
    control.setAttribute('aria-describedby', errId);
    if (!existing) {
      existing = document.createElement('div');
      existing.id = errId;
      existing.className = 'form-field-error';
      // 锚点取 .form-row 而非 parentNode:路径类字段(私钥路径 / 导入 compose 路径)
      // 的父节点是 .input-btn-row(display:flex),直接 append 会把提示塞进
      // nowrap 的按钮行里与输入框抢宽度。取最近 .form-row 后,提示恒在
      // 「控件 + 按钮」整行之下。
      var row = control.closest('.form-row') || control.parentNode;
      // 插在行内提示(form-hint)之前:错误先于帮助文案被读到,也不把
      // 长说明顶在错误与控件之间。行内无 form-hint 时追加到行尾。
      var hint = null;
      for (var i = 0; i < row.children.length; i++) {
        if (row.children[i].classList.contains('form-hint')) { hint = row.children[i]; break; }
      }
      if (hint) row.insertBefore(existing, hint);
      else row.appendChild(existing);
    }
    existing.textContent = String(msg);
  };

  /** 批量清除一行/一组的字段级错误(表单重开或整体重校验前调用) */
  window.clearAllFieldErrors = function (root) {
    var scope = root || document;
    var marked = scope.querySelectorAll('.has-error');
    for (var i = 0; i < marked.length; i++) {
      window.setFieldError(marked[i], null);
    }
  };

  /**
   * 失焦校验接线器:把一份「规则清单」同时接到 blur 校验与提交前整体校验。
   *
   * 契约 inline-validation 要求「失焦时校验、不要按键就校验」—— 此前全站
   * 字段级错误只在点保存时产生(Tab 扫过一张填错的表毫无提示)。本接线器
   * 让同一份规则同时供两条路径使用,避免「blur 说合法、提交说非法」两套
   * 判定漂移。
   *
   * 三条呈现纪律(与提交期校验不同,避免 Tab 走路时刷屏):
   * 1. **必填类**错误(required)只在「表单已提交过一次」或「该字段被填过又
   *    清空」时呈现 —— 否则新建空表上 Tab 一次会挨个标红;
   * 2. **格式类**错误(给了 test)恒呈现;
   * 3. blur 失败**只做字段级提示**:不 toast、不滚动、不写顶部聚合框。
   *
   * 规则描述符(单条):
   * - `id` | `selector` 定位控件(动态行用 selector,事件时解析)
   * - `required` 是否必填;`test(v)` 返回 false 表示格式非法
   * - `when()` 返回 false 时该规则不适用,并**清除**本字段已有错误
   *   (如切到密码认证时清掉私钥路径的红字)
   * - `message` 错误文案;`formatMessage(v)` 出具动态文案(如「第 3 行」)
   * - `label` 该字段在顶部聚合摘要里的名字(如「名称」),供提交期拼
   *   「请填写:名称、主机地址」的既有句式;省略则不进摘要
   * - `gate` 失焦提示是否受「填过 / 已提交」闸门约束。默认:required 规则
   *   受闸门(test 规则不受);条件存在类规则(「Key 认证时必填私钥路径」)
   *   用 `{ test, gate: true }` 表达 —— 它语义上是必填(不该在空表上提前
   *   标红),但提交期要保留自己那句具体文案而非并进「请填写:…」。
   * - `blocking` 是否阻断调用方的主动作(默认 true)。用 `blocking: false`
   *   表达「只提示、不阻断」的规则(如后端本就会夹取归一的上限值)——
   *   这类错误仍贴到字段上,但不进 `blockingErrors`,调用方可照常继续。
   *
   * @param {HTMLElement} formRoot 表单根(代理 focusout / change 的最小范围)
   * @param {Array<Object>} rules 规则清单
   * @returns {{validate:function, checkField:function, clearField:function,
   *            markSubmitted:function, clear:function}} validate() →
   *   `{ firstBad, missing, formats, blockingErrors }`
   */
  window.bindFieldValidation = function (formRoot, rules) {
    // 每个字段的「是否被填过」:必填错误只在填过又清空后即时提示
    var touched = {};
    // 「是否已提交过」:一旦为真,必填类错误也参与 blur 提示
    var submitted = false;
    var list = (Array.isArray(rules) ? rules : []).filter(Boolean);

    function controlOf(rule) {
      if (rule.id) return document.getElementById(rule.id);
      if (rule.selector && formRoot) return formRoot.querySelector(rule.selector);
      return null;
    }

    function keyOf(rule) { return rule.id || rule.selector || ''; }

    /**
     * 求一个字段的错误:{ kind, msg, gated } —— kind 区分「必填」与「格式」,
     * 提交期摘要按 kind 分批(既有文案是「请填写:X、Y」与具体格式句混排);
     * gated 决定失焦时是否需等「填过 / 已提交」。合法或规则不适用返回 null。
     */
    function errorOf(rule, control) {
      if (rule.when && !rule.when()) return null;
      var raw = control ? String(control.value === undefined ? '' : control.value) : '';
      // 口令类字段首尾空格有意义,不做 trim(与各表单一贯口径一致)
      var value = (control && control.type === 'password') ? raw : raw.trim();
      if (rule.required && value === '') {
        return {
          kind: 'required', msg: String(rule.message),
          gated: rule.gate !== false,
          blocking: rule.blocking !== false
        };
      }
      if (rule.test && !rule.test(value)) {
        var msg = rule.formatMessage ? rule.formatMessage(value) : rule.message;
        return {
          kind: 'format', msg: String(msg || ''),
          gated: rule.gate === true,
          blocking: rule.blocking !== false
        };
      }
      return null;
    }

    /** blur 呈现口径:必填类仅在「已提交过」或「填过又清空」时提示 */
    function blurMessageOf(rule, control) {
      var err = errorOf(rule, control);
      if (!err) return '';
      if (err.kind === 'required' && !submitted && !touched[keyOf(rule)]) return '';
      return err.msg;
    }

    /** 校验一个字段并按结果呈现/清除字段级错误(供 blur 专用) */
    function checkField(rule) {
      var control = controlOf(rule);
      if (!control) return;
      var msg = blurMessageOf(rule, control);
      if (msg) window.setFieldError(control, msg);
      else window.setFieldError(control, null);
    }

    /**
     * 整体校验(提交期):逐字段呈现字段级错误,并返回结构化结果 ——
     * `firstBad` 首个出错控件(焦点管理)、`missing` 必填缺失的字段名清单
     * (拼「请填写:…」摘要)、`formats` 格式错误文案清单、
     * `blockingErrors` 需阻断主动作的错误文案(`blocking: false` 的规则
     * 只提示不阻断,故不进此列)。
     */
    function validateAll() {
      submitted = true;
      var firstBad = null;
      var missing = [];
      var formats = [];
      var blockingErrors = [];
      // 逐字段(而非逐规则)求错:一个字段可挂多条规则,若按规则逐条写,
      // 后一条通过的规则会用 setFieldError(control, null) 抹掉前一条刚
      // 贴上的错误文案。
      var seen = [];
      for (var i = 0; i < list.length; i++) {
        var control = controlOf(list[i]);
        if (!control || seen.indexOf(control) !== -1) continue;
        seen.push(control);
        var hit = firstErrorOf(rulesFor(control), control);
        if (!hit) { window.setFieldError(control, null); continue; }
        window.setFieldError(control, hit.err.msg);
        if (!hit.err.blocking) continue; // 只提示不阻断
        if (!firstBad) firstBad = control;
        if (hit.err.kind === 'required') {
          // 条件类必填(无 label)不进「请填写:…」摘要
          if (hit.rule.label) missing.push(hit.rule.label);
        } else {
          formats.push(hit.err.msg);
        }
        blockingErrors.push(hit.err.msg);
      }
      return {
        firstBad: firstBad, missing: missing, formats: formats,
        blockingErrors: blockingErrors
      };
    }

    /**
     * 一个字段可挂多条规则(如远程部署目录:必填 + 绝对路径):按登记顺序
     * 取**第一条**不通过的 —— 与提交期「一字段一条错误」的既有呈现一致。
     * 返回 `{ rule, err }`(无错返回 null),rule 供摘要读取 label。
     */
    function firstErrorOf(rulesOfField, control) {
      for (var i = 0; i < rulesOfField.length; i++) {
        var err = errorOf(rulesOfField[i], control);
        if (err) return { rule: rulesOfField[i], err: err };
      }
      return null;
    }

    /** 取某控件的全部规则(按登记顺序) */
    function rulesFor(control) {
      var hit = [];
      for (var i = 0; i < list.length; i++) {
        if (controlOf(list[i]) === control) hit.push(list[i]);
      }
      return hit;
    }

    /** 失焦校验(委托):受闸门的规则要等「填过 / 已提交」才提示 */
    function onFieldBlur(control) {
      var rulesOfField = rulesFor(control);
      if (!rulesOfField.length) return;
      var value = String(control.value === undefined ? '' : control.value);
      var key = keyOf(rulesOfField[0]);
      if (value.trim() !== '') touched[key] = true;
      var hit = firstErrorOf(rulesOfField, control);
      if (hit && hit.err.gated && !submitted && !touched[key]) {
        // 未曾填过且尚未提交:不提示(避免 Tab 扫过空表满屏标红)
        window.setFieldError(control, null);
        return;
      }
      if (hit) window.setFieldError(control, hit.err.msg);
      else window.setFieldError(control, null);
    }

    // 失焦校验:focusout 冒泡委托,一次监听覆盖全部规则字段(含动态行)
    if (formRoot) {
      formRoot.addEventListener('focusout', function (e) {
        var control = e.target;
        if (!control || !control.classList) return;
        if (control.tagName !== 'INPUT' && control.tagName !== 'TEXTAREA'
            && control.tagName !== 'SELECT') return;
        onFieldBlur(control);
      });
      // select 在部分交互路径下不发 focusout(纯键盘选择):change 兜一次
      formRoot.addEventListener('change', function (e) {
        var control = e.target;
        if (!control || control.tagName !== 'SELECT') return;
        onFieldBlur(control);
      });
      // 输入时清除本字段已有错误(只清不加:不构成「输入即校验」)
      formRoot.addEventListener('input', function (e) {
        var control = e.target;
        if (!control || !control.classList || !control.classList.contains('has-error')) return;
        window.setFieldError(control, null);
      });
    }

    return {
      /** 提交期整体校验 → { firstBad, missing, formats } */
      validate: validateAll,
      /** 手动校验单个字段(按 id 或 selector 定位,供联动场景调用) */
      checkField: function (idOrSelector) {
        for (var i = 0; i < list.length; i++) {
          var rule = list[i];
          if (rule.id === idOrSelector || rule.selector === idOrSelector) checkField(rule);
        }
      },
      /** 清单个字段的错误(联动切换时用) */
      clearField: function (idOrSelector) {
        var node = document.getElementById(idOrSelector);
        if (node) window.setFieldError(node, null);
      },
      /** 标记「已提交过」:此后必填类错误也参与 blur 提示 */
      markSubmitted: function () { submitted = true; },
      /** 复位全部状态与字段级错误(表单重开时调用) */
      clear: function () {
        touched = {};
        submitted = false;
        window.clearAllFieldErrors(formRoot || document);
      }
    };
  };

  /**
   * Enter 提交接线:把 Enter 映射到表单的「非破坏性主入口」。
   *
   * 全站此前有 8 处手写 Enter(manage 的打标签/创建卷/创建网络/连接容器/
   * 自定义间隔、rollback 扫描起点、manage-stacks 终端输入),语义与守卫
   * 各写一遍且都缺 IME 判断 —— 中文输入法按 Enter 是上屏候选词,不判断
   * 就会在选词时误触发提交。本助手统一这两条纪律:
   *
   * 1. **只认单行文本类 input**:textarea 保持换行语义(版本说明 / 收件人 /
   *    pre-post 钩子都是多行),select / checkbox / radio 一律不响应;
   * 2. **不越过二次确认**:调用方在「确认视图」不注册本助手即可 —— Enter
   *    只该走到会再确认一步的主入口(预检 / 打开确认),不直接执行。
   *
   * @param {HTMLElement} formRoot 表单根(事件委托范围)
   * @param {function} onEnter 主入口动作(通常即主按钮的 click 处理器)
   * @returns {function} 解绑函数(视图切到确认态时调用)
   */
  window.bindFormEnter = function (formRoot, onEnter) {
    if (!formRoot || typeof onEnter !== 'function') return function () {};
    var TYPED = { text: 1, number: 1, password: 1, search: 1, email: 1, tel: 1, url: 1 };
    function handler(e) {
      if (e.key !== 'Enter') return;
      // 中文输入法组合中:Enter 是上屏候选词,不能当作提交
      if (e.isComposing || e.keyCode === 229) return;
      var node = e.target;
      if (!node || node.tagName !== 'INPUT') return;
      var type = String(node.type || 'text').toLowerCase();
      if (!TYPED[type]) return;
      if (node.disabled || node.readOnly) return;
      e.preventDefault(); // 拦住 form 的隐式提交,统一走主入口
      onEnter();
    }
    formRoot.addEventListener('keydown', handler);
    return function () { formRoot.removeEventListener('keydown', handler); };
  };

  /**
   * 表单语义脚手架:把「div + 按钮」的既有表单渲染进真实 <form>。
   *
   * 全库此前 0 个 <form>(wiki/07 限制 58),故无 AT 角度的表单边界与
   * Enter 语义。本助手不重建 DOM 结构,只是把容器清空后放入一个
   * `<form novalidate>` 并把渲染目标改到它 —— 调用方的「一直 append 到
   * body」写法原样成立,无需逐处改。
   *
   * 两个必须有的保险:
   * - `novalidate` 抑制浏览器原生校验气泡(文案由字段级错误承担,风格统一);
   * - `submit` 恒拦:HTML 规范里「可阻塞隐式提交的字段恰好一个」时,在该
   *   字段按 Enter 浏览器会自行提交表单 —— 回滚模态只有目标引用一个文本
   *   输入,恰好命中,不接管会导致 WebView 导航(桌面应用里等于白屏)。
   *   拦下后转给调用方登记的主入口动作。
   *
   * `labelId` 指向模态标题时,表单在 AT 里是一个有名字的 landmark。
   *
   * @param {HTMLElement} container 模态 body(内容会被清空)
   * @param {string} [labelId] 表单可访问名(通常为模态标题元素 id)
   * @returns {{form:HTMLFormElement, onSubmit:function, submit:function}}
   */
  window.beginForm = function (container, labelId) {
    var form = document.createElement('form');
    form.setAttribute('novalidate', 'novalidate');
    form.className = 'form-root';
    if (labelId) form.setAttribute('aria-labelledby', labelId);
    var action = null;
    form.addEventListener('submit', function (e) {
      // 恒拦:原生提交会导航整个 WebView
      e.preventDefault();
      if (typeof action === 'function') action();
    });
    if (container) {
      container.textContent = '';
      container.appendChild(form);
    }
    return {
      form: form,
      /** 登记主入口动作(隐式提交由它接管,通常与主按钮同一处理器) */
      onSubmit: function (fn) { action = fn; },
      /** 主动触发主入口(供确认视图的按钮等复用) */
      submit: function () { if (typeof action === 'function') action(); }
    };
  };

  /**
   * 确认文案三段式构建器:主问句 + 事实清单(键值对) + 风险提示。
   *
   * 此前 7 条确认渲染路径有 5 条是纯文本堆叠 —— 问句与「服务会短暂重启」
   * 风险句同字号同色,信息层级全靠读;config-io 危险区是唯一有结构的,本
   * 助手把它的层级语言(问句/事实/风险三段)上收为全站唯一口径。样式见
   * .confirm-title / .confirm-facts / .confirm-fact / .confirm-risk。
   *
   * 返回 DOM 节点,按钮由调用方自行追加(各处按钮语义不同)。
   *
   * @param {Object} opts
   * @param {string} opts.title 主问句(15px/700,「要…吗?」句式)
   * @param {Array<Array<string>>|Array<string>} [opts.facts]
   *   事实清单:二元数组 [键, 值] 渲染成「键: 值」(键加粗);
   *   一元字符串则渲染为普通事实行
   * @param {string} [opts.risk] 风险提示(琥珀色 + 3px 左条,末段)
   * @returns {HTMLElement}
   */
  window.confirmBlock = function (opts) {
    var root = document.createElement('div');
    root.className = 'confirm-block';
    if (opts && opts.title) {
      var title = document.createElement('p');
      title.className = 'confirm-title';
      title.textContent = String(opts.title);
      root.appendChild(title);
    }
    if (opts && opts.facts && opts.facts.length) {
      var facts = document.createElement('div');
      facts.className = 'confirm-facts';
      for (var i = 0; i < opts.facts.length; i++) {
        var item = opts.facts[i];
        var line = document.createElement('div');
        line.className = 'confirm-fact';
        if (item && item.length === 2 && typeof item[0] === 'string') {
          var key = document.createElement('b');
          key.textContent = item[0] + ':';
          line.appendChild(key);
          line.appendChild(document.createTextNode(' ' + String(item[1])));
        } else if (item) {
          line.textContent = String(Array.isArray(item) ? item.join('') : item);
        }
        facts.appendChild(line);
      }
      root.appendChild(facts);
    }
    if (opts && opts.risk) {
      var risk = document.createElement('p');
      risk.className = 'confirm-risk';
      risk.textContent = String(opts.risk);
      root.appendChild(risk);
    }
    return root;
  };

  // ===== 截断单元格悬停补全文本(pass 4;契约 truncation-strategy)=====
  // 以省略号截断的元素,悬停时若真发生了溢出(scrollWidth > clientWidth)
  // 才把完整文本写入 title —— 委托监听,零渲染路径改动,不扰未截断元素。
  // 类清单 = 全站带 ellipsis 截断的数据单元格/行(与 style.css 对应)。
  document.addEventListener('mouseover', function (e) {
    var t = e.target;
    if (!(t instanceof Element) || t.title) return;
    var mark = ' text-truncate port-cell rollback-item-name rollback-item-compose'
      + ' cleanup-item cleanup-project-dir cleanup-warn-row cleanup-diag-cmd ';
    var cls = ' ' + t.className + ' ';
    var hit = false;
    var names = mark.split(/\s+/);
    for (var i = 0; i < names.length; i++) {
      if (names[i] && cls.indexOf(' ' + names[i] + ' ') !== -1) { hit = true; break; }
    }
    if (!hit) return;
    if (t.scrollWidth > t.clientWidth + 1) t.title = t.textContent;
  });

  // ===== 复制文本到剪贴板(成功 toast「已复制」)=====
  window.copyText = function (text) {
    function done() { window.toast('已复制', 'ok'); }
    function fallback() {
      try {
        var ta = document.createElement('textarea');
        ta.value = text;
        ta.style.position = 'fixed';
        ta.style.opacity = '0';
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        document.body.removeChild(ta);
        done();
      } catch (e) {
        window.toast('复制失败,请手动复制', 'fail');
      }
    }
    if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
      navigator.clipboard.writeText(text).then(done, fallback);
    } else {
      fallback();
    }
  };

  // ===== 亮暗主题切换:新状态自按钮位置圆形扩散揭示(View Transitions)=====
  // 支持 reduce 或 API 不可用时:直接切换,无动画(首帧前主题由 index.html head 脚本恢复)。
  // 注意:startViewTransition 定义在 Document 接口(document.startViewTransition,
  // Chromium 111+);Element 级变体 Chromium 147+ 才有,故必须用 document 调用。
  window.toggleScheme = function (evt) {
    var root = document.documentElement;
    var next = root.dataset.arkScheme === 'dark' ? 'light' : 'dark';
    var reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    var apply = function () {
      root.dataset.arkScheme = next;
      try { localStorage.setItem(SCHEME_KEY, next); } catch (e) { /* 忽略 */ }
    };
    if (reduce || typeof document.startViewTransition !== 'function') { apply(); return; }
    // evt.currentTarget 仅在派发期间有效,先同步取按钮矩形,以按钮中心为圆心
    var rect = evt && evt.currentTarget && typeof evt.currentTarget.getBoundingClientRect === 'function'
      ? evt.currentTarget.getBoundingClientRect()
      : { left: window.innerWidth - 48, top: 24, width: 32, height: 32 };
    var x = rect.left + rect.width / 2;
    var y = rect.top + rect.height / 2;
    var end = Math.hypot(Math.max(x, window.innerWidth - x), Math.max(y, window.innerHeight - y));
    // 揭示圆心/半径写在根节点,供 ::view-transition-new(root) 的 clip-path 消费
    root.style.setProperty('--scheme-x', x + 'px');
    root.style.setProperty('--scheme-y', y + 'px');
    root.style.setProperty('--scheme-r', end + 'px');
    document.startViewTransition(apply);
  };

  // ===== 主题跟随系统(dd_scheme = 'auto',UPGRADE-PLAN 阶段四)=====
  // 手动切换(window.toggleScheme)恒写显式 'light'/'dark' 覆盖 auto,行为不变;
  // 仅当持久化值为 'auto' 时,系统亮暗变化经此监听实时跟随。
  var SCHEME_MQ = (typeof window.matchMedia === 'function')
    ? window.matchMedia('(prefers-color-scheme: dark)')
    : null;

  /** 读取主题模式(localStorage['dd_scheme'],异常/未存按显式 light) */
  function schemeMode() {
    try { return localStorage.getItem(SCHEME_KEY) || 'light'; } catch (e) { return 'light'; }
  }

  /**
   * 按模式应用主题:显式 'light'/'dark' 直接写;仅 'auto' 经
   * matchMedia 解析系统偏好后写根节点 data-ark-scheme。
   * 挂 window 供 settings.js「外观」单选复用(同 toggleScheme 先例)。
   */
  window.applyArkScheme = function (mode) {
    var resolved = mode === 'auto'
      ? (SCHEME_MQ && SCHEME_MQ.matches ? 'dark' : 'light')
      : (mode === 'dark' ? 'dark' : 'light');
    document.documentElement.dataset.arkScheme = resolved;
  };

  /** 系统亮暗变化:仅 auto 模式跟随(显式值不受系统偏好影响) */
  function onSystemSchemeChange() {
    if (schemeMode() === 'auto') window.applyArkScheme('auto');
  }
  if (SCHEME_MQ) {
    if (typeof SCHEME_MQ.addEventListener === 'function') {
      SCHEME_MQ.addEventListener('change', onSystemSchemeChange);
    } else if (typeof SCHEME_MQ.addListener === 'function') {
      SCHEME_MQ.addListener(onSystemSchemeChange); // 旧实现兜底(Safari < 14)
    }
  }

  // ===== 初始化:绑定导航点击 + 主题切换按钮 + 刷新禁用态 =====
  document.addEventListener('DOMContentLoaded', function () {
    var items = document.querySelectorAll('.dock-item');
    Array.prototype.forEach.call(items, function (item) {
      item.addEventListener('click', function () {
        window.showPage(item.getAttribute('data-nav'));
      });
    });
    var schemeBtn = document.getElementById('scheme-toggle');
    if (schemeBtn) {
      schemeBtn.addEventListener('click', window.toggleScheme);
    }
    window.refreshNav();

    // dock 左下角版本号:从 tauri.conf 的 package version 读取(core:default 含
    // app:get-version 权限),替代曾硬编码的 v0.1.0;读取失败保留「v…」占位。
    // 顺带消费「更新完成」标记(见下)。
    var verEl = document.getElementById('dock-version');
    var runningVersion = null;
    var versionPromise = Promise.resolve(null);
    try {
      var appApi = (window.__TAURI__ || {}).app;
      if (appApi && typeof appApi.getVersion === 'function') {
        versionPromise = appApi.getVersion().then(function (v) {
          if (v) {
            if (verEl) verEl.textContent = 'v' + String(v);
            runningVersion = String(v);
          }
          return runningVersion;
        }).catch(function () { return null; });
      }
    } catch (e) { /* 保留占位 */ }

    // 自动更新完成提示(第三批修复):静默安装前由后端写入
    // config/update-pending.json,新版首次启动读取并**清除**该标记 →
    // 恰好一次地提示「已更新到 vX」。旧实现装完既不重启也无提示,用户
    // 无法判断更新是否生效。
    // 与当前实际版本比对:安装中途失败(仍是旧版)时不误报。
    try {
      if (window.AppBus && typeof window.AppBus.invoke === 'function') {
        versionPromise.then(function (current) {
          return window.AppBus.invoke('take_update_pending').then(function (marked) {
            if (!marked) return;
            var m = String(marked).replace(/^v/i, '');
            var c = String(current || '').replace(/^v/i, '');
            // 版本一致(安装成功并重启)才提示;否则静默丢弃标记
            if (c && m && c === m) {
              window.toast('已更新到 v' + m, 'ok');
            } else if (window.console && console.info) {
              console.info('[update] 标记版本 ' + m + ' 与实际版本 ' + c + ' 不一致,不提示');
            }
          });
        }).catch(function () { /* 无标记或读取失败:静默 */ });
      }
    } catch (e) { /* 静默 */ }
  });
})();
