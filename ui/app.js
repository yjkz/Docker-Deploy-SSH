/* ============================================================
 * app.js — 全站脚本(普通 script 加载,先于各页面脚本执行)
 *
 * 提供:
 * - window.showPage(name)         页面切换(section[data-page])
 * - window.AppState.hostOk        全局环境检测状态(镜像到 localStorage['dd_hostOk'])
 * - window.refreshNav()           根据 AppState.hostOk 刷新导航禁用态
 * - window.AppBus.invoke / on     Tauri 命令与事件的薄封装
 * - window.toast(msg, type)       右下角 Toast(2.5 秒自动消失)
 * - window.copyText(text)         复制文本到剪贴板(成功 toast「已复制」)
 * ============================================================ */
(function () {
  'use strict';

  var HOST_OK_KEY = 'dd_hostOk';
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
    var items = document.querySelectorAll('.nav-item');
    for (i = 0; i < items.length; i++) {
      items[i].classList.toggle('active', items[i].getAttribute('data-nav') === name);
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
      var item = document.querySelector('.nav-item[data-nav="' + name + '"]');
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
      var active = document.querySelector('.nav-item.active');
      if (active && LOCKED_PAGES.indexOf(active.getAttribute('data-nav')) !== -1) {
        window.showPage('check');
      }
    }
  };

  // ===== Toast:右下角,2.5 秒自动消失 =====
  window.toast = function (message, type) {
    var container = document.getElementById('toast-container');
    if (!container) return;
    var el = document.createElement('div');
    el.className = 'toast toast-' + (type || 'info');
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

  // ===== 初始化:绑定导航点击 + 刷新禁用态 =====
  document.addEventListener('DOMContentLoaded', function () {
    var items = document.querySelectorAll('.nav-item');
    Array.prototype.forEach.call(items, function (item) {
      item.addEventListener('click', function () {
        window.showPage(item.getAttribute('data-nav'));
      });
    });
    window.refreshNav();
  });
})();
