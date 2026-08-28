/* ============================================================
 * images.js — 镜像列表页逻辑(依赖 app.js 提供的全局工具)
 *
 * 后端命令(字段为 Rust snake_case 原样序列化):
 * - list_images() -> ImageInfo[]
 *   ImageInfo = { repository, tag, size_bytes(1024 进制字节数),
 *                 created(如 "2026-08-01 10:00:00 +0800 CST"), id }
 *
 * 页面进入时机:app.js 的 showPage() 成功切换页面后会在 window 上派发
 * 'pagechange' 自定义事件(detail.page = 页面名),本文件监听该事件,
 * 在首次进入镜像页时自动加载一次;此后仅通过「刷新」按钮重新加载。
 *
 * 安全说明:镜像名虽来自本机 docker,但所有单元格一律使用
 * createElement + textContent 渲染,不使用 innerHTML 拼接。
 * ============================================================ */
(function () {
  'use strict';

  var st = {
    all: [],        // list_images 的完整结果
    keyword: '',    // 当前搜索关键字(按输入原文保存,比较时转小写)
    loading: false,
    loaded: false   // 是否已成功加载过(用于“首次进入自动加载一次”)
  };

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

  /** 1024 进制字节数 → "1.2 GB" / "300.0 MB"(B/KB/MB/GB/TB,保留 1 位小数) */
  function formatBytes(bytes) {
    var n = Number(bytes);
    if (!isFinite(n) || n < 0) n = 0;
    var units = ['B', 'KB', 'MB', 'GB', 'TB'];
    var i = 0;
    while (n >= 1024 && i < units.length - 1) {
      n /= 1024;
      i++;
    }
    return n.toFixed(1) + ' ' + units[i];
  }

  /** 按关键字过滤(不区分大小写,匹配仓库名或标签);无关键字时返回全部 */
  function applyFilter() {
    var kw = st.keyword.trim().toLowerCase();
    if (!kw) return st.all.slice();
    return st.all.filter(function (img) {
      var repo = String(img.repository || '').toLowerCase();
      var tag = String(img.tag || '').toLowerCase();
      return repo.indexOf(kw) !== -1 || tag.indexOf(kw) !== -1;
    });
  }

  // ===== 渲染 =====

  /** 计数行:可选 override 文案(加载中 / 失败提示);有关键字时显示过滤计数 */
  function renderCount(override) {
    var node = document.getElementById('images-count');
    if (!node) return;
    if (override) {
      node.textContent = override;
      return;
    }
    var total = st.all.length;
    var shown = applyFilter().length;
    node.textContent = st.keyword
      ? '共 ' + total + ' 个镜像,过滤后 ' + shown + ' 个'
      : '共 ' + total + ' 个镜像';
  }

  /** 仓库名 / TAG 单元格:缺失时灰色 <none>,否则等宽字体 */
  function nameCell(value) {
    var td = document.createElement('td');
    if (isNone(value)) {
      td.className = 'none-text';
      td.textContent = '<none>';
    } else {
      td.className = 'mono';
      td.textContent = String(value);
    }
    return td;
  }

  /** “部署”按钮:记录待部署镜像并跳转部署向导(部署页由 Task 9 读取) */
  function actionCell(img) {
    var td = document.createElement('td');
    td.className = 'col-action';
    var btn = el('button', 'btn btn-primary btn-sm', '部署');
    btn.type = 'button';
    btn.addEventListener('click', function () {
      window.__pendingDeployImage = {
        repository: String(img.repository || ''),
        tag: String(img.tag || '')
      };
      window.showPage('deploy');
    });
    td.appendChild(btn);
    return td;
  }

  /** 用一行占位文案填充表体(加载中 / 无结果) */
  function renderEmptyRow(text) {
    var tbody = document.getElementById('images-tbody');
    if (!tbody) return;
    tbody.textContent = '';
    var tr = document.createElement('tr');
    var td = el('td', 'empty-cell', text);
    td.colSpan = 5;
    tr.appendChild(td);
    tbody.appendChild(tr);
  }

  function renderTable() {
    var tbody = document.getElementById('images-tbody');
    if (!tbody) return;
    var rows = applyFilter();

    if (rows.length === 0) {
      renderEmptyRow(st.keyword.trim() ? '无匹配镜像' : '暂无本地镜像');
      return;
    }

    tbody.textContent = '';
    rows.forEach(function (img) {
      var tr = document.createElement('tr');

      tr.appendChild(nameCell(img.repository));
      tr.appendChild(nameCell(img.tag));

      var sizeTd = document.createElement('td');
      sizeTd.className = 'nowrap';
      sizeTd.textContent = formatBytes(img.size_bytes);
      tr.appendChild(sizeTd);

      var createdTd = document.createElement('td');
      createdTd.className = 'nowrap';
      if (isNone(img.created)) {
        createdTd.classList.add('none-text');
        createdTd.textContent = '<none>';
      } else {
        createdTd.textContent = String(img.created);
      }
      tr.appendChild(createdTd);

      tr.appendChild(actionCell(img));
      tbody.appendChild(tr);
    });
  }

  function renderAll() {
    renderTable();
    renderCount();
  }

  // ===== 错误框 =====

  function showError(msg) {
    var box = document.getElementById('images-error');
    var wrap = document.getElementById('images-table-wrap');
    if (wrap) wrap.classList.add('hidden');
    if (!box) return;
    box.textContent = '';
    box.appendChild(el('span', 'images-error-text', msg || '加载镜像列表失败'));
    var retry = el('button', 'btn', '重试');
    retry.type = 'button';
    retry.addEventListener('click', function () {
      loadImages();
    });
    box.appendChild(retry);
    box.classList.remove('hidden');
  }

  function hideError() {
    var box = document.getElementById('images-error');
    var wrap = document.getElementById('images-table-wrap');
    if (box) {
      box.textContent = '';
      box.classList.add('hidden');
    }
    if (wrap) wrap.classList.remove('hidden');
  }

  // ===== 加载 =====

  function setRefreshing(refreshing) {
    var btn = document.getElementById('images-refresh-btn');
    if (btn) btn.disabled = refreshing;
  }

  function loadImages() {
    if (st.loading) return;
    st.loading = true;
    setRefreshing(true);
    hideError();
    renderEmptyRow('正在加载镜像列表…');
    renderCount('正在加载…');

    window.AppBus.invoke('list_images')
      .then(function (list) {
        st.all = Array.isArray(list) ? list : [];
        st.loaded = true;
        renderAll();
      })
      .catch(function (err) {
        renderEmptyRow('');
        showError(errText(err));
        renderCount('加载失败');
      })
      .then(function () {
        st.loading = false;
        setRefreshing(false);
      });
  }

  // ===== 初始化 =====

  function init() {
    var search = document.getElementById('images-search');
    if (search) {
      search.addEventListener('input', function () {
        st.keyword = search.value;
        renderTable();
        renderCount();
      });
    }

    var refresh = document.getElementById('images-refresh-btn');
    if (refresh) {
      refresh.addEventListener('click', function () {
        loadImages();
      });
    }

    // 首次进入镜像页时自动加载一次(页面切换事件由 app.js 的 showPage 派发)
    window.addEventListener('pagechange', function (e) {
      if (e && e.detail && e.detail.page === 'images' && !st.loaded && !st.loading) {
        loadImages();
      }
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
