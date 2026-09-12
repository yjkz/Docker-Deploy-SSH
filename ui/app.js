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
    var row = control.parentNode;
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
      row.appendChild(existing);
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
