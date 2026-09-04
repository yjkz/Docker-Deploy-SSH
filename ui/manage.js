/* ============================================================
 * manage.js — 05 远程管理页(普通 script 加载,在 app.js 之后)
 *
 * 通过 SSH 在远程服务器执行 docker 命令,实现容器 / 镜像的查看与操作。
 * 低耦合:不修改 app.js / 其他页面 JS;仅依赖全局 AppBus / toast / showPage。
 *
 * 功能:
 * - 服务器选择 + 概览面板(docker info + system df)
 * - 容器列表(按 ID 差异更新,保留已展开 inspect 面板)
 * - 容器操作:启动 / 停止 / 重启 / 删除 / 日志 / 详情
 * - 镜像列表 + 拉取 / 删除 / 打标签
 * - 定时自动刷新(开关 + 预设/自定义间隔 3-300s,localStorage 持久化)
 * - 定时器生命周期:离页清理、切服务器/切 Tab 重置、防重入、操作期暂停
 * ============================================================ */
(function () {
  'use strict';

  // ===== 常量 =====
  var AR_KEY = 'dd_manage_autorefresh';
  var INTERVAL_KEY = 'dd_manage_interval';
  var MIN_INTERVAL = 3;
  var MAX_INTERVAL = 300;

  // ===== 状态 =====
  var state = {
    serverId: null,
    tab: 'containers',
    autoRefresh: false,
    interval: 30,
    inFlight: false,
    opInProgress: false,
    timer: null,
    expanded: {},       // containerId -> true (inspect 面板展开)
    expandedPorts: {},  // containerId -> true (端口列表展开)
    inspectCache: {},   // containerId -> inspect JSON (避免重复请求)
    containers: [],
    images: []
  };

  // ===== DOM 引用(延迟获取,确保 DOM 就绪) =====
  var $ = function (id) { return document.getElementById(id); };

  // ===== 初始化 =====
  document.addEventListener('DOMContentLoaded', function () {
    bindEvents();
    restorePrefs();
  });

  // 页面切换:进入 05 加载数据 + 启动定时器;离开清理定时器
  window.addEventListener('pagechange', function (e) {
    if (e.detail && e.detail.page === 'manage') {
      onEnter();
    } else {
      onLeave();
    }
  });

  function bindEvents() {
    // 服务器下拉
    var sel = $('manage-server-select');
    if (sel) sel.addEventListener('change', onServerChange);

    // 手动刷新
    var btn = $('manage-refresh-btn');
    if (btn) btn.addEventListener('click', function () { refreshAll(); });

    // Tab 切换
    var tabs = document.querySelectorAll('.manage-tab');
    for (var i = 0; i < tabs.length; i++) {
      tabs[i].addEventListener('click', function () {
        switchTab(this.getAttribute('data-tab'));
      });
    }

    // 自动刷新开关
    var toggle = $('manage-autorefresh-toggle');
    if (toggle) toggle.addEventListener('change', onAutoRefreshToggle);

    // 间隔选择
    var ivSel = $('manage-interval-select');
    if (ivSel) ivSel.addEventListener('change', onIntervalChange);

    // 镜像拉取
    var pullBtn = $('manage-pull-btn');
    if (pullBtn) pullBtn.addEventListener('click', onPullImage);
    var pullInput = $('manage-pull-input');
    if (pullInput) pullInput.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') onPullImage();
    });

    // 模态框关闭
    var closeBtn = $('manage-modal-close');
    if (closeBtn) closeBtn.addEventListener('click', closeModal);
    var overlay = $('manage-modal');
    if (overlay) overlay.addEventListener('click', function (e) {
      if (e.target === overlay) closeModal();
    });
  }

  function restorePrefs() {
    try {
      state.autoRefresh = localStorage.getItem(AR_KEY) === 'true';
      var iv = parseInt(localStorage.getItem(INTERVAL_KEY), 10);
      if (!isNaN(iv) && iv >= MIN_INTERVAL && iv <= MAX_INTERVAL) {
        state.interval = iv;
      }
    } catch (e) { /* localStorage 不可用时忽略 */ }

    var toggle = $('manage-autorefresh-toggle');
    if (toggle) toggle.checked = state.autoRefresh;
    var ivSel = $('manage-interval-select');
    if (ivSel) {
      // 匹配预设值,否则设为 custom
      var matched = false;
      for (var i = 0; i < ivSel.options.length; i++) {
        if (ivSel.options[i].value === String(state.interval)) {
          ivSel.value = String(state.interval);
          matched = true;
          break;
        }
      }
      if (!matched) ivSel.value = 'custom';
      ivSel.disabled = !state.autoRefresh;
    }
  }

  function savePrefs() {
    try {
      localStorage.setItem(AR_KEY, String(state.autoRefresh));
      localStorage.setItem(INTERVAL_KEY, String(state.interval));
    } catch (e) { /* 忽略 */ }
  }

  // ===== 页面进入 / 离开 =====
  function onEnter() {
    loadServers().then(function () {
      if (state.serverId) {
        refreshAll();
      }
    });
    startTimerIfEnabled();
  }

  function onLeave() {
    stopTimer();
  }

  // ===== 服务器列表 =====
  function loadServers() {
    return AppBus.invoke('manage_list_servers').then(function (servers) {
      var sel = $('manage-server-select');
      if (!sel) return;
      var prev = state.serverId;
      sel.innerHTML = '';
      if (!servers || servers.length === 0) {
        var opt = document.createElement('option');
        opt.value = '';
        opt.textContent = '暂无服务器,请先在「服务器管理」页添加';
        sel.appendChild(opt);
        state.serverId = null;
        setStatus('未连接', 'info');
        return;
      }
      for (var i = 0; i < servers.length; i++) {
        var s = servers[i];
        var o = document.createElement('option');
        o.value = s.id;
        o.textContent = s.name + ' (' + s.host + ')';
        sel.appendChild(o);
      }
      // 恢复上次选择或默认第一个
      if (prev && servers.some(function (s) { return s.id === prev; })) {
        sel.value = prev;
        state.serverId = prev;
      } else {
        state.serverId = servers[0].id;
        sel.value = state.serverId;
      }
    }).catch(function (err) {
      showError('加载服务器列表失败: ' + (err && err.message ? err.message : err));
    });
  }

  function onServerChange() {
    var sel = $('manage-server-select');
    if (!sel) return;
    state.serverId = sel.value || null;
    // 切换服务器:清空展开状态与缓存
    state.expanded = {};
    state.expandedPorts = {};
    state.inspectCache = {};
    hideError();
    refreshAll();
    resetTimer();
  }

  // ===== Tab 切换 =====
  function switchTab(tab) {
    if (state.tab === tab) return;
    state.tab = tab;
    var tabs = document.querySelectorAll('.manage-tab');
    for (var i = 0; i < tabs.length; i++) {
      tabs[i].classList.toggle('active', tabs[i].getAttribute('data-tab') === tab);
    }
    $('manage-containers-panel').classList.toggle('hidden', tab !== 'containers');
    $('manage-images-panel').classList.toggle('hidden', tab !== 'images');
    // 切到对应 Tab 时若尚未加载过则加载
    if (state.serverId) {
      if (tab === 'containers') refreshContainers();
      else refreshImages();
    }
    resetTimer();
  }

  // ===== 刷新总入口 =====
  function refreshAll() {
    if (!state.serverId) return;
    refreshOverview();
    if (state.tab === 'containers') refreshContainers();
    else refreshImages();
  }

  // ===== 概览 =====
  function refreshOverview() {
    if (!state.serverId) return;
    setStatus('连接中…', 'info');
    AppBus.invoke('manage_overview', { serverId: state.serverId }).then(function (ov) {
      setStatus('已连接', 'ok');
      $('ov-version').textContent = ov.docker_version || '—';
      $('ov-os').textContent = ov.os || '—';
      $('ov-kernel').textContent = ov.kernel || '—';
      $('ov-arch').textContent = ov.arch || '—';
      $('ov-containers').textContent =
        (ov.containers_running || '0') + ' / ' +
        (ov.containers_paused || '0') + ' / ' +
        (ov.containers_stopped || '0') + ' / ' +
        (ov.containers_total || '0');
      $('ov-images').textContent = ov.images_total || '0';
      $('ov-disk').textContent = ov.disk_used || '—';
      hideError();
    }).catch(function (err) {
      var msg = err && err.message ? err.message : String(err);
      setStatus('连接失败', 'fail');
      showError(msg);
    });
  }

  // ===== 端口解析与时间格式化工具 =====
  function parsePorts(portsStr) {
    if (!portsStr) return [];
    return portsStr.split(',').map(function (p) {
      return p.trim();
    }).filter(function (p) { return p.length > 0; });
  }

  // "0.0.0.0:8080->80/tcp" -> "8080→80"; ":::5432->5432/tcp" -> "5432→5432"; "80/tcp" -> "80"
  function simplifyPort(port) {
    var m = port.match(/(\d+)->(\d+)/);
    if (m) return m[1] + '→' + m[2];
    m = port.match(/(\d+)\/(tcp|udp)/);
    if (m) return m[1];
    return port;
  }

  // "2026-09-01 12:00:00 +0000 UTC" -> "09-01 12:00"
  function formatTime(createdAt) {
    if (!createdAt) return '—';
    var m = createdAt.match(/(\d{4})-(\d{2})-(\d{2})\s+(\d{2}):(\d{2})/);
    if (m) return m[2] + '-' + m[3] + ' ' + m[4] + ':' + m[5];
    return createdAt;
  }

  // 切换端口展开/收起
  function togglePorts(containerId) {
    if (state.expandedPorts[containerId]) {
      delete state.expandedPorts[containerId];
    } else {
      state.expandedPorts[containerId] = true;
    }
    var row = document.querySelector('tr[data-cid="' + containerId + '"]');
    if (row) {
      var c = state.containers.find(function (x) { return x.id === containerId; });
      if (c) updateContainerRow(row, c);
    }
  }

  // ===== 容器列表(按 ID 差异更新) =====
  function refreshContainers() {
    if (!state.serverId || state.inFlight) return;
    state.inFlight = true;
    AppBus.invoke('manage_list_containers', { serverId: state.serverId }).then(function (list) {
      state.inFlight = false;
      renderContainers(list || []);
    }).catch(function (err) {
      state.inFlight = false;
      var msg = err && err.message ? err.message : String(err);
      showError('加载容器列表失败: ' + msg);
    });
  }

  function renderContainers(list) {
    var tbody = $('manage-containers-tbody');
    if (!tbody) return;

    // 移除空占位行
    var emptyCell = tbody.querySelector('.empty-cell');
    if (emptyCell) {
      var emptyRow = emptyCell.closest('tr');
      if (emptyRow) emptyRow.remove();
    }

    if (list.length === 0) {
      tbody.innerHTML = '<tr><td class="empty-cell" colspan="6">暂无容器</td></tr>';
      state.containers = [];
      return;
    }

    // 索引现有数据行
    var rowMap = {};
    var rows = tbody.querySelectorAll('tr[data-cid]');
    for (var i = 0; i < rows.length; i++) {
      rowMap[rows[i].getAttribute('data-cid')] = rows[i];
    }

    var seen = {};
    var frag = document.createDocumentFragment();

    for (var j = 0; j < list.length; j++) {
      var c = list[j];
      seen[c.id] = true;
      var row = rowMap[c.id];
      if (row) {
        updateContainerRow(row, c);
      } else {
        row = createContainerRow(c);
      }
      frag.appendChild(row);

      // 详情行(inspect 折叠面板)
      var detailSel = 'tr[data-cid-detail="' + c.id + '"]';
      var detailRow = tbody.querySelector(detailSel);
      if (state.expanded[c.id]) {
        if (!detailRow) {
          detailRow = createDetailRow(c.id);
        }
        frag.appendChild(detailRow);
      } else if (detailRow) {
        detailRow.remove();
      }
    }

    // 移除已不存在的容器行
    for (var id in rowMap) {
      if (!seen[id]) {
        rowMap[id].remove();
        var d = tbody.querySelector('tr[data-cid-detail="' + id + '"]');
        if (d) d.remove();
        delete state.expanded[id];
        delete state.expandedPorts[id];
        delete state.inspectCache[id];
      }
    }

    tbody.appendChild(frag);
    state.containers = list;
  }

  function createContainerRow(c) {
    var tr = document.createElement('tr');
    tr.setAttribute('data-cid', c.id);
    tr.className = 'container-row';
    updateContainerRow(tr, c);
    return tr;
  }

  function updateContainerRow(tr, c) {
    var stateBadge = containerStateBadge(c.state);
    var actions = containerActionButtons(c);

    tr.innerHTML = '';
    // 状态
    var tdState = document.createElement('td');
    tdState.appendChild(stateBadge);
    tr.appendChild(tdState);
    // 名称(点击展开详情,超长截断)
    var tdName = document.createElement('td');
    tdName.className = 'mono container-name-cell text-truncate';
    tdName.textContent = c.names || c.id;
    tdName.title = (c.names || c.id) + ' (点击查看详情)';
    tdName.style.cursor = 'pointer';
    tdName.addEventListener('click', function () { toggleInspect(c.id); });
    tr.appendChild(tdName);
    // 镜像(超长截断+tooltip)
    var tdImage = document.createElement('td');
    tdImage.className = 'mono text-truncate';
    tdImage.textContent = c.image || '—';
    if (c.image) tdImage.title = c.image;
    tr.appendChild(tdImage);
    // 端口(<=2个直接显示简化版;>2个显示前2个+N徽章,点击展开完整列表)
    var tdPorts = document.createElement('td');
    tdPorts.className = 'mono port-cell';
    var ports = parsePorts(c.ports);
    if (ports.length === 0) {
      tdPorts.textContent = '—';
    } else if (state.expandedPorts[c.id]) {
      // 展开态:完整原始格式,每行一个
      tdPorts.title = '点击收起端口';
      tdPorts.style.cursor = 'pointer';
      ports.forEach(function (p) {
        var line = document.createElement('div');
        line.className = 'port-line';
        line.textContent = p;
        tdPorts.appendChild(line);
      });
      tdPorts.addEventListener('click', function (e) {
        e.stopPropagation();
        togglePorts(c.id);
      });
    } else {
      // 折叠态:前2个简化端口
      tdPorts.textContent = ports.slice(0, 2).map(simplifyPort).join(', ');
      if (ports.length > 2) {
        var badge = document.createElement('span');
        badge.className = 'port-badge';
        badge.textContent = '+' + (ports.length - 2);
        tdPorts.appendChild(badge);
        tdPorts.title = '点击展开全部 ' + ports.length + ' 个端口';
        tdPorts.style.cursor = 'pointer';
        tdPorts.addEventListener('click', function (e) {
          e.stopPropagation();
          togglePorts(c.id);
        });
      }
    }
    tr.appendChild(tdPorts);
    // 创建时间(格式化 MM-DD HH:mm,tooltip 显示完整)
    var tdCreated = document.createElement('td');
    tdCreated.textContent = formatTime(c.created_at);
    if (c.created_at) tdCreated.title = c.created_at;
    tr.appendChild(tdCreated);
    // 操作
    var tdAction = document.createElement('td');
    tdAction.className = 'col-action';
    tdAction.appendChild(actions);
    tr.appendChild(tdAction);
  }

  function containerStateBadge(stateStr) {
    var badge = document.createElement('span');
    var s = (stateStr || '').toLowerCase();
    var cls, text;
    if (s === 'running') { cls = 'badge-running'; text = '运行中'; }
    else if (s === 'paused') { cls = 'badge-paused'; text = '已暂停'; }
    else if (s === 'restarting') { cls = 'badge-paused'; text = '重启中'; }
    else if (s === 'exited' || s === 'dead') { cls = 'badge-exited'; text = s === 'dead' ? '已死亡' : '已停止'; }
    else if (s === 'created') { cls = 'badge-created'; text = '已创建'; }
    else { cls = 'badge-info'; text = stateStr || '未知'; }
    badge.className = 'badge ' + cls;
    badge.textContent = text;
    return badge;
  }

  function containerActionButtons(c) {
    var wrap = document.createElement('div');
    wrap.className = 'action-btn-group';
    var s = (c.state || '').toLowerCase();

    if (s === 'running') {
      wrap.appendChild(makeActionBtn('停止', 'stop', c.id));
      wrap.appendChild(makeActionBtn('重启', 'restart', c.id));
    } else {
      wrap.appendChild(makeActionBtn('启动', 'start', c.id));
    }
    wrap.appendChild(makeActionBtn('日志', 'logs', c.id));
    wrap.appendChild(makeActionBtn('删除', 'rm', c.id, true));

    return wrap;
  }

  function makeActionBtn(label, action, containerId, danger) {
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'btn btn-sm' + (danger ? ' btn-danger' : '');
    btn.textContent = label;
    btn.addEventListener('click', function (e) {
      e.stopPropagation();
      if (action === 'logs') { showLogs(containerId); return; }
      if (action === 'rm') { confirmRemoveContainer(containerId); return; }
      doContainerAction(containerId, action, label);
    });
    return btn;
  }

  function doContainerAction(containerId, action, label) {
    if (!state.serverId) return;
    state.opInProgress = true;
    stopTimer();
    AppBus.invoke('manage_container_action', {
      serverId: state.serverId,
      containerId: containerId,
      action: action
    }).then(function (res) {
      state.opInProgress = false;
      if (res.success) {
        toast(label + '成功', 'ok');
        refreshContainers();
        refreshOverview();
      } else {
        toast(label + '失败: ' + (res.message || '未知错误'), 'fail');
      }
      startTimerIfEnabled();
    }).catch(function (err) {
      state.opInProgress = false;
      var msg = err && err.message ? err.message : String(err);
      toast(label + '失败: ' + msg, 'fail');
      startTimerIfEnabled();
    });
  }

  function confirmRemoveContainer(containerId) {
    var c = state.containers.find(function (x) { return x.id === containerId; });
    var name = c ? (c.names || containerId) : containerId;
    var isRunning = c && (c.state || '').toLowerCase() === 'running';
    var msg = isRunning
      ? '容器「' + name + '」正在运行,删除将强制停止并删除该容器,确定继续吗?'
      : '确定删除容器「' + name + '」吗?';

    openModal('删除容器', buildConfirmBody(msg, '删除', function () {
      closeModal();
      doContainerAction(containerId, 'rm', '删除容器');
    }));
  }

  // ===== 容器详情(inspect) =====
  function createDetailRow(containerId) {
    var tr = document.createElement('tr');
    tr.setAttribute('data-cid-detail', containerId);
    tr.className = 'container-detail-row';
    var td = document.createElement('td');
    td.colSpan = 6;
    var panel = document.createElement('div');
    panel.className = 'manage-detail-panel';
    panel.id = 'detail-panel-' + containerId;
    panel.textContent = '加载中…';
    td.appendChild(panel);
    tr.appendChild(td);
    return tr;
  }

  function toggleInspect(containerId) {
    if (state.expanded[containerId]) {
      state.expanded[containerId] = false;
      var row = document.querySelector('tr[data-cid-detail="' + containerId + '"]');
      if (row) row.remove();
    } else {
      state.expanded[containerId] = true;
      // 插入详情行到数据行之后
      var dataRow = document.querySelector('tr[data-cid="' + containerId + '"]');
      if (dataRow) {
        var detailRow = createDetailRow(containerId);
        dataRow.parentNode.insertBefore(detailRow, dataRow.nextSibling);
        loadInspect(containerId);
      }
    }
  }

  function loadInspect(containerId) {
    var panel = $('detail-panel-' + containerId);
    if (!panel) return;

    if (state.inspectCache[containerId]) {
      renderInspect(panel, state.inspectCache[containerId]);
      return;
    }

    AppBus.invoke('manage_container_inspect', {
      serverId: state.serverId,
      containerId: containerId
    }).then(function (data) {
      // docker inspect 返回数组,取 [0]
      var info = Array.isArray(data) ? data[0] : data;
      state.inspectCache[containerId] = info;
      renderInspect(panel, info);
    }).catch(function (err) {
      var msg = err && err.message ? err.message : String(err);
      panel.textContent = '加载详情失败: ' + msg;
    });
  }

  function renderInspect(panel, info) {
    if (!info) { panel.textContent = '无数据'; return; }
    var name = info.Name || '—';
    var stateStr = info.State && info.State.Status ? info.State.Status : '—';
    var restartCount = info.RestartCount != null ? info.RestartCount : '—';
    var startedAt = info.State && info.State.StartedAt ? info.State.StartedAt : '—';
    var image = info.Config && info.Config.Image ? info.Config.Image : '—';
    var ip = info.NetworkSettings && info.NetworkSettings.IPAddress ? info.NetworkSettings.IPAddress : '—';
    var mounts = info.Mounts && info.Mounts.length ? info.Mounts.map(function (m) {
      return (m.Source || '?') + ' → ' + (m.Destination || '?');
    }).join('; ') : '—';

    var html = '<div class="inspect-grid">' +
      '<div class="inspect-item"><span class="inspect-label">名称</span><span class="inspect-value mono">' + escHtml(name) + '</span></div>' +
      '<div class="inspect-item"><span class="inspect-label">状态</span><span class="inspect-value">' + escHtml(stateStr) + '</span></div>' +
      '<div class="inspect-item"><span class="inspect-label">重启次数</span><span class="inspect-value">' + escHtml(String(restartCount)) + '</span></div>' +
      '<div class="inspect-item"><span class="inspect-label">启动时间</span><span class="inspect-value mono">' + escHtml(startedAt) + '</span></div>' +
      '<div class="inspect-item"><span class="inspect-label">镜像</span><span class="inspect-value mono">' + escHtml(image) + '</span></div>' +
      '<div class="inspect-item"><span class="inspect-label">IP 地址</span><span class="inspect-value mono">' + escHtml(ip) + '</span></div>' +
      '<div class="inspect-item inspect-item-wide"><span class="inspect-label">挂载</span><span class="inspect-value mono">' + escHtml(mounts) + '</span></div>' +
      '</div>';
    panel.innerHTML = html;
  }

  // ===== 容器日志 =====
  function showLogs(containerId) {
    var c = state.containers.find(function (x) { return x.id === containerId; });
    var name = c ? (c.names || containerId) : containerId;
    var tail = 100;

    var body = document.createElement('div');
    body.innerHTML =
      '<div class="log-tail-bar">' +
      '<label>显示行数:' +
      '<select id="log-tail-select" class="form-input form-input-sm">' +
      '<option value="100">100</option>' +
      '<option value="500">500</option>' +
      '<option value="1000">1000</option>' +
      '<option value="0">全部</option>' +
      '</select></label>' +
      '<button id="log-copy-btn" class="btn btn-sm" type="button">复制日志</button>' +
      '</div>' +
      '<pre id="log-content" class="manage-log-body">加载中…</pre>';

    openModal('容器日志 — ' + name, body);

    var tailSel = $('log-tail-select');
    if (tailSel) tailSel.addEventListener('change', function () {
      tail = parseInt(tailSel.value, 10) || 100;
      fetchLogs(containerId, tail);
    });
    var copyBtn = $('log-copy-btn');
    if (copyBtn) copyBtn.addEventListener('click', function () {
      var content = $('log-content');
      if (content) copyText(content.textContent);
    });

    fetchLogs(containerId, tail);
  }

  function fetchLogs(containerId, tail) {
    var content = $('log-content');
    if (!content) return;
    content.textContent = '加载中…';
    AppBus.invoke('manage_container_logs', {
      serverId: state.serverId,
      containerId: containerId,
      tail: tail
    }).then(function (logs) {
      content.textContent = logs || '(无日志输出)';
    }).catch(function (err) {
      var msg = err && err.message ? err.message : String(err);
      content.textContent = '加载日志失败: ' + msg;
    });
  }

  // ===== 镜像列表 =====
  function refreshImages() {
    if (!state.serverId || state.inFlight) return;
    state.inFlight = true;
    AppBus.invoke('manage_list_images', { serverId: state.serverId }).then(function (list) {
      state.inFlight = false;
      renderImages(list || []);
    }).catch(function (err) {
      state.inFlight = false;
      var msg = err && err.message ? err.message : String(err);
      showError('加载镜像列表失败: ' + msg);
    });
  }

  function renderImages(list) {
    var tbody = $('manage-images-tbody');
    if (!tbody) return;

    if (list.length === 0) {
      tbody.innerHTML = '<tr><td class="empty-cell" colspan="6">暂无镜像</td></tr>';
      state.images = [];
      return;
    }

    // 按复合 key(repo:tag:id)做差异更新
    var rowMap = {};
    var rows = tbody.querySelectorAll('tr[data-iid]');
    for (var i = 0; i < rows.length; i++) {
      rowMap[rows[i].getAttribute('data-iid')] = rows[i];
    }

    var seen = {};
    var frag = document.createDocumentFragment();

    for (var j = 0; j < list.length; j++) {
      var img = list[j];
      var key = img.repository + ':' + img.tag + ':' + img.id;
      seen[key] = true;
      var row = rowMap[key];
      if (row) {
        updateImageRow(row, img);
      } else {
        row = createImageRow(img, key);
      }
      frag.appendChild(row);
    }

    for (var k in rowMap) {
      if (!seen[k]) rowMap[k].remove();
    }

    tbody.appendChild(frag);
    state.images = list;
  }

  function createImageRow(img, key) {
    var tr = document.createElement('tr');
    tr.setAttribute('data-iid', key);
    updateImageRow(tr, img);
    return tr;
  }

  function updateImageRow(tr, img) {
    tr.innerHTML = '';
    // 仓库(超长截断+tooltip)
    var tdRepo = document.createElement('td');
    tdRepo.className = 'mono text-truncate';
    tdRepo.textContent = img.repository || '<none>';
    if (img.repository) tdRepo.title = img.repository;
    tr.appendChild(tdRepo);
    // 标签(超长截断+tooltip)
    var tdTag = document.createElement('td');
    tdTag.className = 'mono text-truncate';
    tdTag.textContent = img.tag || '<none>';
    if (img.tag) tdTag.title = img.tag;
    tr.appendChild(tdTag);
    // ID(sha256 超长截断+tooltip)
    var tdId = document.createElement('td');
    tdId.className = 'mono text-truncate';
    tdId.textContent = img.id || '—';
    if (img.id) tdId.title = img.id;
    tr.appendChild(tdId);
    // 大小
    var tdSize = document.createElement('td');
    tdSize.textContent = img.size || '—';
    tr.appendChild(tdSize);
    // 创建时间(格式化 MM-DD HH:mm,tooltip 显示完整)
    var tdCreated = document.createElement('td');
    tdCreated.textContent = formatTime(img.created_at);
    if (img.created_at) tdCreated.title = img.created_at;
    tr.appendChild(tdCreated);
    // 操作
    var tdAction = document.createElement('td');
    tdAction.className = 'col-action';
    var wrap = document.createElement('div');
    wrap.className = 'action-btn-group';

    var fullImage = (img.repository && img.repository !== '<none>')
      ? img.repository + ':' + (img.tag || 'latest')
      : img.id;

    var tagBtn = document.createElement('button');
    tagBtn.type = 'button';
    tagBtn.className = 'btn btn-sm';
    tagBtn.textContent = '打标签';
    tagBtn.addEventListener('click', function () { showTagModal(fullImage); });
    wrap.appendChild(tagBtn);

    var rmBtn = document.createElement('button');
    rmBtn.type = 'button';
    rmBtn.className = 'btn btn-sm btn-danger';
    rmBtn.textContent = '删除';
    rmBtn.addEventListener('click', function () { confirmRemoveImage(img.id, fullImage); });
    wrap.appendChild(rmBtn);

    tdAction.appendChild(wrap);
    tr.appendChild(tdAction);
  }

  // ===== 镜像拉取 =====
  function onPullImage() {
    var input = $('manage-pull-input');
    if (!input) return;
    var image = input.value.trim();
    if (!image) { toast('请输入镜像名', 'warn'); return; }

    state.opInProgress = true;
    stopTimer();
    var btn = $('manage-pull-btn');
    if (btn) { btn.disabled = true; btn.textContent = '拉取中…'; }

    AppBus.invoke('manage_image_pull', {
      serverId: state.serverId,
      image: image
    }).then(function (res) {
      state.opInProgress = false;
      if (btn) { btn.disabled = false; btn.textContent = '拉取镜像'; }
      if (res.success) {
        toast('拉取成功: ' + image, 'ok');
        input.value = '';
        refreshImages();
        refreshOverview();
      } else {
        toast('拉取失败: ' + (res.message || '未知错误'), 'fail');
      }
      startTimerIfEnabled();
    }).catch(function (err) {
      state.opInProgress = false;
      if (btn) { btn.disabled = false; btn.textContent = '拉取镜像'; }
      var msg = err && err.message ? err.message : String(err);
      toast('拉取失败: ' + msg, 'fail');
      startTimerIfEnabled();
    });
  }

  // ===== 镜像删除 =====
  function confirmRemoveImage(imageId, fullImage) {
    openModal('删除镜像', buildConfirmBody(
      '确定删除镜像「' + fullImage + '」吗?如果该镜像被容器引用将删除失败,可勾选强制删除。',
      '删除',
      function () {
        var forceCheck = $('manage-force-check');
        var force = forceCheck ? forceCheck.checked : false;
        closeModal();
        doImageRemove(imageId, force);
      },
      true  // 显示 force 复选框
    ));
  }

  function doImageRemove(imageId, force) {
    state.opInProgress = true;
    stopTimer();
    AppBus.invoke('manage_image_remove', {
      serverId: state.serverId,
      imageId: imageId,
      force: force
    }).then(function (res) {
      state.opInProgress = false;
      if (res.success) {
        toast('镜像删除成功', 'ok');
        refreshImages();
        refreshOverview();
      } else {
        toast('删除失败: ' + (res.message || '未知错误'), 'fail');
      }
      startTimerIfEnabled();
    }).catch(function (err) {
      state.opInProgress = false;
      var msg = err && err.message ? err.message : String(err);
      toast('删除失败: ' + msg, 'fail');
      startTimerIfEnabled();
    });
  }

  // ===== 镜像打标签 =====
  function showTagModal(sourceImage) {
    var body = document.createElement('div');
    body.innerHTML =
      '<div class="form-row">' +
      '<label class="form-label" for="tag-source">源镜像</label>' +
      '<input id="tag-source" class="form-input" type="text" value="' + escHtml(sourceImage) + '" readonly>' +
      '</div>' +
      '<div class="form-row">' +
      '<label class="form-label" for="tag-new">新标签(格式:仓库名:标签)</label>' +
      '<input id="tag-new" class="form-input" type="text" placeholder="例如:myrepo/myapp:v1">' +
      '</div>' +
      '<div class="modal-actions">' +
      '<button id="tag-confirm-btn" class="btn btn-primary" type="button">确认打标签</button>' +
      '</div>';

    openModal('打标签', body);
    var newInput = $('tag-new');
    if (newInput) {
      newInput.focus();
      newInput.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') doTag();
      });
    }
    var confirmBtn = $('tag-confirm-btn');
    if (confirmBtn) confirmBtn.addEventListener('click', doTag);
  }

  function doTag() {
    var source = $('tag-source');
    var newTag = $('tag-new');
    if (!source || !newTag) return;
    var image = source.value.trim();
    var tag = newTag.value.trim();
    if (!tag) { toast('请输入新标签', 'warn'); return; }

    state.opInProgress = true;
    stopTimer();
    AppBus.invoke('manage_image_tag', {
      serverId: state.serverId,
      image: image,
      newTag: tag
    }).then(function (res) {
      state.opInProgress = false;
      if (res.success) {
        toast('打标签成功', 'ok');
        closeModal();
        refreshImages();
      } else {
        toast('打标签失败: ' + (res.message || '未知错误'), 'fail');
      }
      startTimerIfEnabled();
    }).catch(function (err) {
      state.opInProgress = false;
      var msg = err && err.message ? err.message : String(err);
      toast('打标签失败: ' + msg, 'fail');
      startTimerIfEnabled();
    });
  }

  // ===== 自动刷新定时器 =====
  function onAutoRefreshToggle() {
    var toggle = $('manage-autorefresh-toggle');
    state.autoRefresh = toggle ? toggle.checked : false;
    var ivSel = $('manage-interval-select');
    if (ivSel) ivSel.disabled = !state.autoRefresh;
    savePrefs();
    if (state.autoRefresh) startTimer();
    else stopTimer();
  }

  function onIntervalChange() {
    var ivSel = $('manage-interval-select');
    if (!ivSel) return;
    var val = ivSel.value;
    if (val === 'custom') {
      var input = window.prompt('请输入刷新间隔(秒),范围 ' + MIN_INTERVAL + '-' + MAX_INTERVAL + ':');
      if (input === null) {
        // 取消:回退到上次有效值
        ivSel.value = String(state.interval);
        return;
      }
      var secs = parseInt(input, 10);
      if (isNaN(secs) || secs < MIN_INTERVAL || secs > MAX_INTERVAL) {
        toast('无效间隔,请输入 ' + MIN_INTERVAL + '-' + MAX_INTERVAL + ' 之间的正整数', 'warn');
        ivSel.value = String(state.interval);
        return;
      }
      state.interval = secs;
    } else {
      state.interval = parseInt(val, 10) || 30;
    }
    savePrefs();
    resetTimer();
  }

  function startTimerIfEnabled() {
    if (state.autoRefresh && isOnManagePage()) startTimer();
  }

  function startTimer() {
    stopTimer();
    if (!state.autoRefresh || !state.serverId) return;
    state.timer = window.setInterval(tick, state.interval * 1000);
  }

  function stopTimer() {
    if (state.timer) {
      window.clearInterval(state.timer);
      state.timer = null;
    }
  }

  function resetTimer() {
    if (state.autoRefresh) startTimer();
  }

  function tick() {
    // 防重入:上次刷新未返回或有操作进行中时跳过
    if (state.inFlight || state.opInProgress) return;
    if (!isOnManagePage()) { stopTimer(); return; }
    refreshAll();
  }

  function isOnManagePage() {
    var section = document.querySelector('section[data-page="manage"]');
    return section && section.classList.contains('active');
  }

  // ===== 模态框 =====
  function openModal(title, bodyEl) {
    var modal = $('manage-modal');
    var titleEl = $('manage-modal-title');
    var body = $('manage-modal-body');
    if (!modal || !body) return;
    if (titleEl) titleEl.textContent = title;
    body.innerHTML = '';
    if (bodyEl) body.appendChild(bodyEl);
    modal.classList.remove('hidden');
  }

  function closeModal() {
    var modal = $('manage-modal');
    if (modal) modal.classList.add('hidden');
  }

  function buildConfirmBody(message, confirmLabel, onConfirm, showForce) {
    var div = document.createElement('div');
    div.innerHTML =
      '<p class="confirm-msg">' + escHtml(message) + '</p>' +
      (showForce ? '<label class="deploy-checkbox"><input type="checkbox" id="manage-force-check"> <span>强制删除(-f)</span></label>' : '') +
      '<div class="modal-actions">' +
      '<button id="confirm-cancel-btn" class="btn" type="button">取消</button>' +
      '<button id="confirm-ok-btn" class="btn btn-danger" type="button">' + escHtml(confirmLabel) + '</button>' +
      '</div>';
    var cancelBtn = div.querySelector('#confirm-cancel-btn');
    if (cancelBtn) cancelBtn.addEventListener('click', closeModal);
    var okBtn = div.querySelector('#confirm-ok-btn');
    if (okBtn) okBtn.addEventListener('click', onConfirm);
    return div;
  }

  // ===== 辅助函数 =====
  function setStatus(text, kind) {
    var badge = $('manage-status-badge');
    if (!badge) return;
    fillBadge(badge, kind, text);
  }

  function showError(msg) {
    var el = $('manage-error');
    if (!el) return;
    el.textContent = msg;
    el.classList.remove('hidden');
  }

  function hideError() {
    var el = $('manage-error');
    if (el) el.classList.add('hidden');
  }

  function escHtml(s) {
    var div = document.createElement('div');
    div.textContent = String(s == null ? '' : s);
    return div.innerHTML;
  }

})();
