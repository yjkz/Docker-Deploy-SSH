/* ============================================================
 * images.js — 镜像列表页逻辑(依赖 app.js 提供的全局工具)
 *
 * 后端命令(字段为 Rust snake_case 原样序列化):
 * - list_images() -> ImageInfo[]
 *   ImageInfo = { repository, tag, size_bytes(1024 进制字节数),
 *                 created(如 "2026-08-01 10:00:00 +0800 CST"), id }
 * - list_dangling_images() -> DanglingImage[](第二十五批;camelCase 契约)
 *   DanglingImage = { id(sha256: 完整 ID), sizeBytes, created }
 * - remove_local_images({ ids }) -> LocalImageRemoval(camelCase)
 *   LocalImageRemoval = { removed, failed: string[] }
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

  /**
   * 仓库名 / TAG 单元格:缺失时斜体 <none>,否则等宽字体;
   * mod 为附加类(仓库名列传 'cell-repo' 走 15px 墨粗样式)
   */
  function nameCell(value, mod) {
    var td = document.createElement('td');
    if (isNone(value)) {
      td.className = 'none-text';
      td.textContent = '<none>';
    } else {
      td.className = mod ? 'mono ' + mod : 'mono';
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

      tr.appendChild(nameCell(img.repository, 'cell-repo'));
      tr.appendChild(nameCell(img.tag));

      var sizeTd = document.createElement('td');
      sizeTd.className = 'mono nowrap';
      sizeTd.textContent = formatBytes(img.size_bytes);
      tr.appendChild(sizeTd);

      var createdTd = document.createElement('td');
      createdTd.className = 'mono nowrap';
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

  // ===== 悬空镜像清理(第二十五批)=====
  //
  // 「悬空」= `<none>:<none>` 层,通常是重新构建同名镜像后残留的旧层,
  // 只占磁盘不参与运行。入口按钮在页头(page-tools),模态呈现清单 +
  // 勾选 + 两步确认(与 06 页清理分析同款交互语言)。
  //
  // 安全前提:悬空镜像必然不被任何容器引用(有引用的层不会显示为
  // `<none>:<none>`,而是完整 repo:tag 或旧标签),故无需查询容器引用 ——
  // 与 06 页清理分析(旧标签镜像可能仍被引用)的场景不同。

  var dg = {
    items: [],        // list_dangling_images 结果
    selected: {},     // id -> bool
    loading: false,
    removing: false,
    armed: false      // 二次确认已触发(按钮变「确认删除」)
  };

  /** 模态骨架访问器 */
  function dgModal() { return document.getElementById('dangling-modal'); }
  function dgBody() { return document.getElementById('dangling-modal-body'); }

  function openDanglingModal() {
    var modal = dgModal();
    var body = dgBody();
    if (!modal || !body) return;
    dg.selected = {};
    dg.armed = false;
    body.textContent = '';
    body.appendChild(el('div', 'cleanup-hint', '正在扫描悬空镜像…'));
    modal.classList.remove('hidden');
    window.modalFocusOpen(modal);
    loadDangling();
  }

  function closeDanglingModal() {
    // 删除进行中拒绝关闭(与清理分析/部署模态同款守卫)
    if (dg.removing) {
      window.toast('正在删除,请等待完成', 'warn');
      return;
    }
    var modal = dgModal();
    if (!modal) return;
    modal.classList.add('hidden');
    window.modalFocusClose(modal);
  }

  function loadDangling() {
    if (dg.loading) return;
    dg.loading = true;
    dg.armed = false;
    window.AppBus.invoke('list_dangling_images')
      .then(function (list) {
        dg.items = Array.isArray(list) ? list : [];
        renderDangling();
      })
      .catch(function (err) {
        var body = dgBody();
        if (!body) return;
        body.textContent = '';
        body.appendChild(el('div', 'cleanup-error',
          '扫描悬空镜像失败:' + (errText(err) || '未知错误')));
        var retry = el('button', 'btn btn-sm', '重试');
        retry.type = 'button';
        retry.addEventListener('click', function () { loadDangling(); });
        body.appendChild(retry);
      })
      .then(function () { dg.loading = false; });
  }

  function renderDangling() {
    var body = dgBody();
    if (!body) return;
    body.textContent = '';
    dg.armed = false;

    if (dg.items.length === 0) {
      body.appendChild(el('div', 'cleanup-hint',
        '没有悬空镜像(所有镜像层都被某个 repo:tag 引用)'));
      return;
    }

    var totalBytes = dg.items.reduce(function (acc, it) {
      return acc + (Number(it.sizeBytes) || 0);
    }, 0);
    body.appendChild(el('div', 'cleanup-hint',
      '发现 ' + dg.items.length + ' 个悬空镜像,合计 ' + formatBytes(totalBytes) +
      ';勾选后删除(悬空镜像不被任何容器引用,删除不影响运行)'));
    body.appendChild(el('div', 'cleanup-hint',
      '注:显示的大小为各镜像层实际占用,多个悬空层可能共享底层,实际释放量可能小于合计值'));

    var list = el('div', 'dg-list');
    dg.items.forEach(function (it, i) {
      var row = el('div', 'dg-row');
      var cb = document.createElement('input');
      cb.type = 'checkbox';
      cb.id = 'dg-cb-' + i;
      cb.checked = true;
      dg.selected[it.id] = true;
      cb.addEventListener('change', function () {
        dg.selected[it.id] = cb.checked;
        dg.armed = false; // 勾选变化 → 撤回二次确认
        updateDgBtn();
      });
      row.appendChild(cb);

      var idSpan = el('span', 'dg-id mono', shortId(it.id));
      idSpan.title = String(it.id || '');
      row.appendChild(idSpan);
      row.appendChild(el('span', 'dg-size mono nowrap', formatBytes(it.sizeBytes)));
      row.appendChild(el('span', 'dg-created mono nowrap', String(it.created || '')));
      list.appendChild(row);
    });
    body.appendChild(list);

    var actions = el('div', 'dg-actions');
    var allBtn = el('button', 'btn btn-sm', '全选');
    allBtn.type = 'button';
    allBtn.addEventListener('click', function () {
      var on = Object.keys(dg.selected).every(function (k) { return dg.selected[k]; });
      Object.keys(dg.selected).forEach(function (k) { dg.selected[k] = !on ? true : false; });
      body.querySelectorAll('.dg-row input[type=checkbox]').forEach(function (cb) {
        cb.checked = !on;
      });
      dg.armed = false;
      allBtn.textContent = !on ? '全不选' : '全选';
      updateDgBtn();
    });
    var removeBtn = el('button', 'btn btn-primary', '');
    removeBtn.type = 'button';
    removeBtn.id = 'dg-remove-btn';
    removeBtn.addEventListener('click', function () { doRemove(removeBtn, allBtn); });
    actions.appendChild(allBtn);
    actions.appendChild(removeBtn);
    body.appendChild(actions);

    function updateDgBtn() {
      var n = Object.keys(dg.selected).filter(function (k) { return dg.selected[k]; }).length;
      removeBtn.textContent = dg.armed
        ? '确认删除(' + n + ' 个)'
        : '删除选中项(' + n + ' 个)';
      removeBtn.disabled = n === 0 || dg.removing;
    }
    updateDgBtn();
    // 暴露给 doRemove 复用(模块内闭包,挂到 dg 上避免再查 DOM)
    dg.updateBtn = updateDgBtn;
  }

  /** takeID 前 12 位展示(`sha256:` 前缀之外取 12 字符,与 docker 惯例一致) */
  function shortId(id) {
    var s = String(id || '');
    var body = s.indexOf('sha256:') === 0 ? s.slice(7) : s;
    return body.slice(0, 12);
  }

  function doRemove(btn, allBtn) {
    if (dg.removing) return;
    var ids = Object.keys(dg.selected).filter(function (k) { return dg.selected[k]; });
    if (ids.length === 0) return;
    // 两步确认:首次点击变「确认删除」,再点才执行(与全站删除交互一致)
    if (!dg.armed) {
      dg.armed = true;
      if (dg.updateBtn) dg.updateBtn();
      return;
    }
    dg.removing = true;
    dg.armed = false;
    window.setBtnBusy(btn, true, '删除中…');
    if (allBtn) allBtn.disabled = true;

    window.AppBus.invoke('remove_local_images', { ids: ids })
      .then(function (res) {
        var removed = (res && Number(res.removed)) || 0;
        var failed = (res && Array.isArray(res.failed)) ? res.failed : [];
        if (failed.length === 0) {
          window.toast('已删除 ' + removed + ' 个悬空镜像', 'ok');
        } else {
          window.toast('已删除 ' + removed + ' 个,失败 ' + failed.length + ' 个', 'warn');
        }
        dg.removing = false;
        return loadDangling().then(function () {
          // 失败明细渲染在清单下方(原样文案,便于定位 docker 报错)
          var body = dgBody();
          if (body && failed.length > 0) {
            var failBox = el('div', 'dg-fail');
            failBox.appendChild(el('div', 'cleanup-error', '以下镜像删除失败:'));
            failed.forEach(function (f) {
              failBox.appendChild(el('div', 'dg-fail-line mono', String(f)));
            });
            body.appendChild(failBox);
          }
          // 清空后刷新主列表(02 页表格随之更新)
          st.loaded = false;
          loadImages();
        });
      })
      .catch(function (err) {
        dg.removing = false;
        window.setBtnBusy(btn, false, '删除选中项');
        if (allBtn) allBtn.disabled = false;
        window.toast('删除失败:' + (errText(err) || '未知错误'), 'fail');
        if (dg.updateBtn) dg.updateBtn();
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

    // 悬空镜像清理入口 + 模态三通道关闭(Esc 见下方统一监听)
    var dgBtn = document.getElementById('images-cleanup-btn');
    if (dgBtn) {
      dgBtn.addEventListener('click', openDanglingModal);
    }
    var dgClose = document.getElementById('dangling-modal-close');
    if (dgClose) {
      dgClose.addEventListener('click', closeDanglingModal);
    }
    var dgOverlay = dgModal();
    if (dgOverlay) {
      dgOverlay.addEventListener('click', function (e) {
        if (e.target === dgOverlay) closeDanglingModal();
      });
    }
    // Esc:仅当本模态为最顶层时响应(全站仲裁,避免叠层时一次关两层)
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && window.isTopModal('dangling-modal')) {
        closeDanglingModal();
      }
    });

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
