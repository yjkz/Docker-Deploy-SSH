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
 *
 * B 阶段追加:
 * - 卷列表:查看(inspect) / 删除 / 创建(名称+驱动)
 * - 网络列表:查看 / 删除 / 创建(名称+驱动) / 连接容器 / 断开容器
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
    images: [],
    volumes: [],   // B 阶段追加
    networks: []   // B 阶段追加
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

    // B 阶段:创建卷 / 创建网络入口
    var vBtn = $('manage-volume-create-btn');
    if (vBtn) vBtn.addEventListener('click', showVolumeCreateModal);
    var nBtn = $('manage-network-create-btn');
    if (nBtn) nBtn.addEventListener('click', showNetworkCreateModal);
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
    onLeaveC(); // C 阶段追加:离开 05 页清理监控 / 终端会话
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
    // C 阶段追加:切换服务器时停掉旧服务器上的监控与终端会话
    monitorStop(true);
    stopExecSession(true);
    // 切换服务器:清空展开状态与缓存
    state.expanded = {};
    state.expandedPorts = {};
    state.inspectCache = {};
    hideError();
    refreshAll();
    resetTimer();
  }

  // ===== 滚动位置保持 =====
  // 05 页唯一滚动容器是 main.stage(body overflow:hidden,.layout 100vh)。
  // 切 Tab 时新旧面板高度不同:目标面板更矮(或首次进入还是「选择服务器后加载」
  // 占位行)时,.stage.scrollHeight 变小,浏览器会在下次布局时把 scrollTop 钳到
  // 新上限——面板为空时上限为 0,即用户看到的「切换列表后跳回顶部」。且该钳制
  // 是持久的:异步数据随后到达把列表填高,滚动位置也不会自己回来。
  var stageEl = null;
  function getStage() {
    if (stageEl && stageEl.isConnected) return stageEl;
    stageEl = document.querySelector('main.stage') || document.querySelector('.stage');
    return stageEl;
  }

  // 把滚动位置钳到 min(y, 当前内容可滚上限)并返回实际生效值
  function clampStageScroll(y) {
    var st = getStage();
    if (!st) return y;
    var max = st.scrollHeight - st.clientHeight;
    if (max < 0) max = 0;
    var target = Math.min(y, max);
    if (st.scrollTop !== target) st.scrollTop = target;
    return target;
  }

  // 切 Tab 的一次性滚动记忆:{ tab, y, applied, ts }
  // y=切换前位置;applied=面板切换后立即恢复到的位置;ts 用于过期丢弃
  var pendingTabScroll = null;
  var PENDING_SCROLL_TTL = 10000;

  // 切 Tab 面板切换后立即恢复:目标面板足够高则保持原位置;不够高则落在
  // min(原位置, 新内容高度-视口),不产生比钳制更差的跳顶
  function applyPendingTabScrollNow() {
    var p = pendingTabScroll;
    if (!p) return;
    p.applied = clampStageScroll(p.y);
  }

  // 各渲染函数收尾调用:仅当本次渲染正好是「切 Tab 目标面板」的首次渲染、
  // 且用户在数据返回前没有手动滚动过时,才把位置恢复到 min(原位置, 新上限)
  function consumePendingTabScroll(tab) {
    var p = pendingTabScroll;
    if (!p || p.tab !== tab || state.tab !== tab) return;
    pendingTabScroll = null;
    if (Date.now() - p.ts > PENDING_SCROLL_TTL) return;
    var st = getStage();
    if (!st) return;
    // applied 可能是 0(合法值),必须用 != null 判断
    if (p.applied != null && Math.abs(st.scrollTop - p.applied) > 1) return;
    clampStageScroll(p.y);
  }

  // 渲染期防钳制兜底:渲染前后记录/恢复 .stage 滚动位置。
  // 仅当内容真的变矮(原位置超出新上限)时才恢复到 min(原位置, 新上限);
  // 其余情况(含浏览器滚动锚定的自主调整)一律不干预,不会与原生行为冲突。
  function withStageScrollGuard(mutate) {
    var st = getStage();
    var saved = st ? st.scrollTop : 0;
    mutate();
    if (!st) return;
    var max = st.scrollHeight - st.clientHeight;
    if (max < 0) max = 0;
    if (saved > max && st.scrollTop !== max) st.scrollTop = max;
  }

  // ===== Tab 切换 =====
  function switchTab(tab) {
    if (state.tab === tab) return;
    state.tab = tab;
    // 记录切换前滚动位置:面板切换会把 .stage 钳回顶部,先记住,切换后立即
    // 恢复;目标列表若尚未加载,数据到达渲染完成后再恢复一次(consume*)
    var stBefore = getStage();
    pendingTabScroll = stBefore
      ? { tab: tab, y: stBefore.scrollTop, applied: null, ts: Date.now() }
      : null;
    var tabs = document.querySelectorAll('.manage-tab');
    for (var i = 0; i < tabs.length; i++) {
      tabs[i].classList.toggle('active', tabs[i].getAttribute('data-tab') === tab);
    }
    $('manage-containers-panel').classList.toggle('hidden', tab !== 'containers');
    $('manage-images-panel').classList.toggle('hidden', tab !== 'images');
    var vp = $('manage-volumes-panel');
    if (vp) vp.classList.toggle('hidden', tab !== 'volumes');
    var np = $('manage-networks-panel');
    if (np) np.classList.toggle('hidden', tab !== 'networks');
    var sp = $('manage-stacks-panel');   // C 阶段追加
    if (sp) sp.classList.toggle('hidden', tab !== 'stacks');
    var mp = $('manage-monitor-panel');  // C 阶段追加
    if (mp) mp.classList.toggle('hidden', tab !== 'monitor');
    if (tab !== 'monitor') monitorStop(true); // C 阶段追加:离开监控 Tab 自动停止
    // 面板高度已切换:立即恢复滚动位置(缓解钳制跳顶)
    applyPendingTabScrollNow();
    // 切到对应 Tab 时若尚未加载过则加载
    if (state.serverId) {
      if (tab === 'containers') refreshContainers();
      else if (tab === 'images') refreshImages();
      else if (tab === 'volumes') refreshVolumes();
      else if (tab === 'networks') refreshNetworks();
      else if (tab === 'stacks') refreshStacks(); // C 阶段追加
    } else {
      pendingTabScroll = null; // 无服务器不会触发渲染,丢弃记忆
    }
    resetTimer();
  }

  // ===== 刷新总入口 =====
  function refreshAll() {
    if (!state.serverId) return;
    refreshOverview();
    if (state.tab === 'containers') refreshContainers();
    else if (state.tab === 'images') refreshImages();
    else if (state.tab === 'volumes') refreshVolumes();
    else if (state.tab === 'networks') refreshNetworks();
    else if (state.tab === 'stacks') refreshStacks(); // C 阶段追加
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
    // 渲染期滚动保护 + 切 Tab 后首渲染恢复位置(见 withStageScrollGuard)
    withStageScrollGuard(function () { renderContainersInto(tbody, list); });
    consumePendingTabScroll('containers');
  }

  function renderContainersInto(tbody, list) {
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
      // C 阶段追加:终端按钮(仅 running 容器)
      var execBtn = document.createElement('button');
      execBtn.type = 'button';
      execBtn.className = 'btn btn-sm';
      execBtn.textContent = '终端';
      execBtn.addEventListener('click', function (e) {
        e.stopPropagation();
        openTerminal(c.id, c.names || c.id);
      });
      wrap.appendChild(execBtn);
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
    // 渲染期滚动保护 + 切 Tab 后首渲染恢复位置(见 withStageScrollGuard)
    withStageScrollGuard(function () { renderImagesInto(tbody, list); });
    consumePendingTabScroll('images');
  }

  function renderImagesInto(tbody, list) {
    if (list.length === 0) {
      tbody.innerHTML = '<tr><td class="empty-cell" colspan="6">暂无镜像</td></tr>';
      state.images = [];
      return;
    }

    // 移除初始占位行(参照 renderContainers,否则占位行残留在数据行上方)
    var emptyCell = tbody.querySelector('.empty-cell');
    if (emptyCell) {
      var emptyRow = emptyCell.closest('tr');
      if (emptyRow) emptyRow.remove();
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

  // ===== 卷列表(B 阶段追加) =====
  function refreshVolumes() {
    if (!state.serverId || state.inFlight) return;
    state.inFlight = true;
    AppBus.invoke('manage_list_volumes', { serverId: state.serverId }).then(function (list) {
      state.inFlight = false;
      renderVolumes(list || []);
    }).catch(function (err) {
      state.inFlight = false;
      var msg = err && err.message ? err.message : String(err);
      showError('加载卷列表失败: ' + msg);
    });
  }

  function renderVolumes(list) {
    var tbody = $('manage-volumes-tbody');
    if (!tbody) return;
    // 渲染期滚动保护 + 切 Tab 后首渲染恢复位置(见 withStageScrollGuard)
    withStageScrollGuard(function () { renderVolumesInto(tbody, list); });
    consumePendingTabScroll('volumes');
  }

  function renderVolumesInto(tbody, list) {
    if (list.length === 0) {
      tbody.innerHTML = '<tr><td class="empty-cell" colspan="5">暂无卷</td></tr>';
      state.volumes = [];
      return;
    }

    // 移除初始占位行(参照 renderContainers,否则占位行残留在数据行上方)
    var emptyCell = tbody.querySelector('.empty-cell');
    if (emptyCell) {
      var emptyRow = emptyCell.closest('tr');
      if (emptyRow) emptyRow.remove();
    }

    // 按卷名(唯一)做差异更新
    var rowMap = {};
    var rows = tbody.querySelectorAll('tr[data-vid]');
    for (var i = 0; i < rows.length; i++) {
      rowMap[rows[i].getAttribute('data-vid')] = rows[i];
    }

    var seen = {};
    var frag = document.createDocumentFragment();

    for (var j = 0; j < list.length; j++) {
      var v = list[j];
      seen[v.name] = true;
      var row = rowMap[v.name];
      if (row) {
        updateVolumeRow(row, v);
      } else {
        row = document.createElement('tr');
        row.setAttribute('data-vid', v.name);
        updateVolumeRow(row, v);
      }
      frag.appendChild(row);
    }

    for (var k in rowMap) {
      if (!seen[k]) rowMap[k].remove();
    }

    tbody.appendChild(frag);
    state.volumes = list;
  }

  function updateVolumeRow(tr, v) {
    tr.innerHTML = '';
    // 名称
    var tdName = document.createElement('td');
    tdName.className = 'mono text-truncate';
    tdName.textContent = v.name || '—';
    if (v.name) tdName.title = v.name;
    tr.appendChild(tdName);
    // 驱动
    var tdDriver = document.createElement('td');
    tdDriver.textContent = v.driver || '—';
    tr.appendChild(tdDriver);
    // 挂载点(超长截断+tooltip)
    var tdMount = document.createElement('td');
    tdMount.className = 'mono text-truncate';
    tdMount.textContent = v.mountpoint || '—';
    if (v.mountpoint) tdMount.title = v.mountpoint;
    tr.appendChild(tdMount);
    // 创建时间(Docker 25+ 才有,缺失显示 —)
    var tdCreated = document.createElement('td');
    tdCreated.textContent = v.created_at ? formatTime(v.created_at) : '—';
    if (v.created_at) tdCreated.title = v.created_at;
    tr.appendChild(tdCreated);
    // 操作:查看 / 删除
    var tdAction = document.createElement('td');
    tdAction.className = 'col-action';
    var wrap = document.createElement('div');
    wrap.className = 'action-btn-group';

    var viewBtn = document.createElement('button');
    viewBtn.type = 'button';
    viewBtn.className = 'btn btn-sm';
    viewBtn.textContent = '查看';
    viewBtn.addEventListener('click', function () { showResourceInspect('manage_volume_inspect', v.name, '卷详情 — ' + v.name, 'volumeName'); });
    wrap.appendChild(viewBtn);

    var rmBtn = document.createElement('button');
    rmBtn.type = 'button';
    rmBtn.className = 'btn btn-sm btn-danger';
    rmBtn.textContent = '删除';
    rmBtn.addEventListener('click', function () { confirmRemoveVolume(v.name); });
    wrap.appendChild(rmBtn);

    tdAction.appendChild(wrap);
    tr.appendChild(tdAction);
  }

  function confirmRemoveVolume(name) {
    openModal('删除卷', buildConfirmBody(
      '确定删除卷「' + name + '」吗?如果该卷正被容器使用将删除失败。',
      '删除',
      function () {
        closeModal();
        state.opInProgress = true;
        stopTimer();
        AppBus.invoke('manage_volume_remove', {
          serverId: state.serverId,
          volumeName: name
        }).then(function (res) {
          state.opInProgress = false;
          if (res.success) {
            toast('卷删除成功', 'ok');
            refreshVolumes();
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
    ));
  }

  // ===== 创建卷(B 阶段追加) =====
  function showVolumeCreateModal() {
    var body = document.createElement('div');
    body.innerHTML =
      '<div class="form-row">' +
      '<label class="form-label" for="volume-name-input">卷名称</label>' +
      '<input id="volume-name-input" class="form-input" type="text" placeholder="例如:mydata">' +
      '</div>' +
      '<div class="form-row">' +
      '<label class="form-label" for="volume-driver-input">驱动(留空默认 local)</label>' +
      '<input id="volume-driver-input" class="form-input" type="text" placeholder="local">' +
      '</div>' +
      '<div class="modal-actions">' +
      '<button id="volume-create-confirm" class="btn btn-primary" type="button">创建</button>' +
      '</div>';

    openModal('创建卷', body);
    var nameInput = $('volume-name-input');
    if (nameInput) {
      nameInput.focus();
      nameInput.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') doVolumeCreate();
      });
    }
    var confirmBtn = $('volume-create-confirm');
    if (confirmBtn) confirmBtn.addEventListener('click', doVolumeCreate);
  }

  function doVolumeCreate() {
    var nameInput = $('volume-name-input');
    var driverInput = $('volume-driver-input');
    if (!nameInput) return;
    var name = nameInput.value.trim();
    if (!name) { toast('请输入卷名称', 'warn'); return; }
    var driver = driverInput ? driverInput.value.trim() : '';

    state.opInProgress = true;
    stopTimer();
    AppBus.invoke('manage_volume_create', {
      serverId: state.serverId,
      volumeName: name,
      driver: driver || null
    }).then(function (res) {
      state.opInProgress = false;
      if (res.success) {
        toast('卷创建成功: ' + name, 'ok');
        closeModal();
        refreshVolumes();
      } else {
        toast('创建失败: ' + (res.message || '未知错误'), 'fail');
      }
      startTimerIfEnabled();
    }).catch(function (err) {
      state.opInProgress = false;
      var msg = err && err.message ? err.message : String(err);
      toast('创建失败: ' + msg, 'fail');
      startTimerIfEnabled();
    });
  }

  // ===== 网络列表(B 阶段追加) =====
  function refreshNetworks() {
    if (!state.serverId || state.inFlight) return;
    state.inFlight = true;
    AppBus.invoke('manage_list_networks', { serverId: state.serverId }).then(function (list) {
      state.inFlight = false;
      renderNetworks(list || []);
    }).catch(function (err) {
      state.inFlight = false;
      var msg = err && err.message ? err.message : String(err);
      showError('加载网络列表失败: ' + msg);
    });
  }

  function renderNetworks(list) {
    var tbody = $('manage-networks-tbody');
    if (!tbody) return;
    // 渲染期滚动保护 + 切 Tab 后首渲染恢复位置(见 withStageScrollGuard)
    withStageScrollGuard(function () { renderNetworksInto(tbody, list); });
    consumePendingTabScroll('networks');
  }

  function renderNetworksInto(tbody, list) {
    if (list.length === 0) {
      tbody.innerHTML = '<tr><td class="empty-cell" colspan="5">暂无网络</td></tr>';
      state.networks = [];
      return;
    }

    // 移除初始占位行(参照 renderContainers,否则占位行残留在数据行上方)
    var emptyCell = tbody.querySelector('.empty-cell');
    if (emptyCell) {
      var emptyRow = emptyCell.closest('tr');
      if (emptyRow) emptyRow.remove();
    }

    // 按网络 ID(唯一)做差异更新
    var rowMap = {};
    var rows = tbody.querySelectorAll('tr[data-nid]');
    for (var i = 0; i < rows.length; i++) {
      rowMap[rows[i].getAttribute('data-nid')] = rows[i];
    }

    var seen = {};
    var frag = document.createDocumentFragment();

    for (var j = 0; j < list.length; j++) {
      var n = list[j];
      seen[n.id] = true;
      var row = rowMap[n.id];
      if (row) {
        updateNetworkRow(row, n);
      } else {
        row = document.createElement('tr');
        row.setAttribute('data-nid', n.id);
        updateNetworkRow(row, n);
      }
      frag.appendChild(row);
    }

    for (var k in rowMap) {
      if (!seen[k]) rowMap[k].remove();
    }

    tbody.appendChild(frag);
    state.networks = list;
  }

  function updateNetworkRow(tr, n) {
    tr.innerHTML = '';
    // 名称
    var tdName = document.createElement('td');
    tdName.className = 'mono text-truncate';
    tdName.textContent = n.name || '—';
    if (n.name) tdName.title = n.name;
    tr.appendChild(tdName);
    // 驱动
    var tdDriver = document.createElement('td');
    tdDriver.textContent = n.driver || '—';
    tr.appendChild(tdDriver);
    // 范围
    var tdScope = document.createElement('td');
    tdScope.textContent = n.scope || '—';
    tr.appendChild(tdScope);
    // 已连接容器数
    var tdCount = document.createElement('td');
    tdCount.className = 'mono';
    tdCount.textContent = String(n.containers != null ? n.containers : 0);
    tr.appendChild(tdCount);
    // 操作:查看 / 连接容器 / 断开容器 / 删除
    var tdAction = document.createElement('td');
    tdAction.className = 'col-action';
    var wrap = document.createElement('div');
    wrap.className = 'action-btn-group';

    var viewBtn = document.createElement('button');
    viewBtn.type = 'button';
    viewBtn.className = 'btn btn-sm';
    viewBtn.textContent = '查看';
    viewBtn.addEventListener('click', function () { showResourceInspect('manage_network_inspect', n.id, '网络详情 — ' + (n.name || n.id), 'networkId'); });
    wrap.appendChild(viewBtn);

    var connectBtn = document.createElement('button');
    connectBtn.type = 'button';
    connectBtn.className = 'btn btn-sm';
    connectBtn.textContent = '连接容器';
    connectBtn.addEventListener('click', function () { showNetworkContainerModal(n, 'connect'); });
    wrap.appendChild(connectBtn);

    var disconnectBtn = document.createElement('button');
    disconnectBtn.type = 'button';
    disconnectBtn.className = 'btn btn-sm';
    disconnectBtn.textContent = '断开容器';
    disconnectBtn.addEventListener('click', function () { showNetworkContainerModal(n, 'disconnect'); });
    wrap.appendChild(disconnectBtn);

    // 内置网络(bridge/host/none)不可删除
    var builtin = n.name === 'bridge' || n.name === 'host' || n.name === 'none';
    if (!builtin) {
      var rmBtn = document.createElement('button');
      rmBtn.type = 'button';
      rmBtn.className = 'btn btn-sm btn-danger';
      rmBtn.textContent = '删除';
      rmBtn.addEventListener('click', function () { confirmRemoveNetwork(n.id, n.name); });
      wrap.appendChild(rmBtn);
    }

    tdAction.appendChild(wrap);
    tr.appendChild(tdAction);
  }

  function confirmRemoveNetwork(id, name) {
    openModal('删除网络', buildConfirmBody(
      '确定删除网络「' + (name || id) + '」吗?如果有容器连接在该网络上将删除失败。',
      '删除',
      function () {
        closeModal();
        state.opInProgress = true;
        stopTimer();
        AppBus.invoke('manage_network_remove', {
          serverId: state.serverId,
          networkId: id
        }).then(function (res) {
          state.opInProgress = false;
          if (res.success) {
            toast('网络删除成功', 'ok');
            refreshNetworks();
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
    ));
  }

  // ===== 创建网络(B 阶段追加) =====
  function showNetworkCreateModal() {
    var body = document.createElement('div');
    body.innerHTML =
      '<div class="form-row">' +
      '<label class="form-label" for="network-name-input">网络名称</label>' +
      '<input id="network-name-input" class="form-input" type="text" placeholder="例如:mynet">' +
      '</div>' +
      '<div class="form-row">' +
      '<label class="form-label" for="network-driver-input">驱动(留空默认 bridge)</label>' +
      '<input id="network-driver-input" class="form-input" type="text" placeholder="bridge">' +
      '</div>' +
      '<div class="modal-actions">' +
      '<button id="network-create-confirm" class="btn btn-primary" type="button">创建</button>' +
      '</div>';

    openModal('创建网络', body);
    var nameInput = $('network-name-input');
    if (nameInput) {
      nameInput.focus();
      nameInput.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') doNetworkCreate();
      });
    }
    var confirmBtn = $('network-create-confirm');
    if (confirmBtn) confirmBtn.addEventListener('click', doNetworkCreate);
  }

  function doNetworkCreate() {
    var nameInput = $('network-name-input');
    var driverInput = $('network-driver-input');
    if (!nameInput) return;
    var name = nameInput.value.trim();
    if (!name) { toast('请输入网络名称', 'warn'); return; }
    var driver = driverInput ? driverInput.value.trim() : '';

    state.opInProgress = true;
    stopTimer();
    AppBus.invoke('manage_network_create', {
      serverId: state.serverId,
      networkName: name,
      driver: driver || null
    }).then(function (res) {
      state.opInProgress = false;
      if (res.success) {
        toast('网络创建成功: ' + name, 'ok');
        closeModal();
        refreshNetworks();
      } else {
        toast('创建失败: ' + (res.message || '未知错误'), 'fail');
      }
      startTimerIfEnabled();
    }).catch(function (err) {
      state.opInProgress = false;
      var msg = err && err.message ? err.message : String(err);
      toast('创建失败: ' + msg, 'fail');
      startTimerIfEnabled();
    });
  }

  // ===== 连接 / 断开容器(B 阶段追加) =====
  function showNetworkContainerModal(n, mode) {
    var isConnect = mode === 'connect';
    var title = (isConnect ? '连接容器到网络 — ' : '从网络断开容器 — ') + (n.name || n.id);
    var body = document.createElement('div');
    body.innerHTML =
      '<p class="confirm-msg">' +
      (isConnect
        ? '输入要连接到该网络的容器名或容器 ID(运行中的容器)'
        : '输入要从该网络断开的容器名或容器 ID') +
      '</p>' +
      '<div class="form-row">' +
      '<label class="form-label" for="network-container-input">容器名 / 容器 ID</label>' +
      '<input id="network-container-input" class="form-input" type="text" placeholder="例如:myapp-web">' +
      '</div>' +
      '<div class="modal-actions">' +
      '<button id="network-container-confirm" class="btn ' + (isConnect ? 'btn-primary' : 'btn-danger') + '" type="button">' +
      (isConnect ? '连接' : '断开') +
      '</button>' +
      '</div>';

    openModal(title, body);
    var input = $('network-container-input');
    if (input) {
      input.focus();
      input.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') doNetworkContainer(n.id, isConnect);
      });
    }
    var confirmBtn = $('network-container-confirm');
    if (confirmBtn) confirmBtn.addEventListener('click', function () { doNetworkContainer(n.id, isConnect); });
  }

  function doNetworkContainer(networkId, isConnect) {
    var input = $('network-container-input');
    if (!input) return;
    var container = input.value.trim();
    if (!container) { toast('请输入容器名或容器 ID', 'warn'); return; }

    state.opInProgress = true;
    stopTimer();
    var cmd = isConnect ? 'manage_network_connect' : 'manage_network_disconnect';
    AppBus.invoke(cmd, {
      serverId: state.serverId,
      networkId: networkId,
      containerId: container
    }).then(function (res) {
      state.opInProgress = false;
      if (res.success) {
        toast((isConnect ? '已连接容器: ' : '已断开容器: ') + container, 'ok');
        closeModal();
        refreshNetworks();
      } else {
        toast((isConnect ? '连接失败: ' : '断开失败: ') + (res.message || '未知错误'), 'fail');
      }
      startTimerIfEnabled();
    }).catch(function (err) {
      state.opInProgress = false;
      var msg = err && err.message ? err.message : String(err);
      toast((isConnect ? '连接失败: ' : '断开失败: ') + msg, 'fail');
      startTimerIfEnabled();
    });
  }

  // ===== 资源 inspect 查看(卷 / 网络通用,B 阶段追加) =====
  function showResourceInspect(command, resourceId, title, paramName) {
    openModal(title, (function () {
      var pre = document.createElement('pre');
      pre.className = 'manage-log-body';
      pre.textContent = '加载中…';
      var params = { serverId: state.serverId };
      params[paramName] = resourceId;
      AppBus.invoke(command, params).then(function (data) {
        // inspect 返回数组,取 [0]
        var info = Array.isArray(data) ? data[0] : data;
        pre.textContent = info ? JSON.stringify(info, null, 2) : '无数据';
      }).catch(function (err) {
        var msg = err && err.message ? err.message : String(err);
        pre.textContent = '加载详情失败: ' + msg;
      });
      return pre;
    })());
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
    // 终端会话专用放大:仅当本次打开的是终端弹窗(body 带 .manage-terminal-modal
    // 标记)时给共用 modal-card 加修饰类;其余弹窗(日志/确认/打标签等)显式移除,
    // 保证共用模态的尺寸互不影响
    var card = modal.querySelector('.modal-card');
    if (card) {
      var isTerm = !!(bodyEl && bodyEl.classList && bodyEl.classList.contains('manage-terminal-modal'));
      if (isTerm) card.classList.add('modal-terminal');
      else card.classList.remove('modal-terminal');
    }
    modal.classList.remove('hidden');
  }

  function closeModal() {
    var modal = $('manage-modal');
    if (modal) modal.classList.add('hidden');
    execOnModalClose(); // C 阶段追加:模态框关闭时清理终端会话
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

  /* ============================================================
   * C 阶段追加:Compose 栈 / 实时监控 / 容器 Exec 终端
   * - 栈列表:启动 / 停止(二次确认) / 服务状态 / 日志,按 compose_file 行差异更新
   * - 监控:manage_stats_start/stop + manage-stats 事件整表刷新,CPU% 阈值着色
   * - 终端:manage_exec_start/write/stop + manage-exec-output 事件,简易 ANSI 处理
   * 生命周期:切走监控 Tab / 离开 05 页自动停止监控与终端会话,unlisten 防泄漏
   * ============================================================ */

  // ===== C 阶段独立状态(不触碰上方 state 对象) =====
  var cState = {
    stacks: [],
    mon: { running: false, unlisten: null, errShown: false },
    exec: {
      sessionId: null, unlisten: null, containerId: null, name: '',
      lines: [], cur: '', curIdx: 0, eof: false, pend: '',
      history: [], histIdx: -1
    }
  };

  document.addEventListener('DOMContentLoaded', bindEventsC);

  function bindEventsC() {
    var sr = $('manage-stack-refresh-btn');
    if (sr) sr.addEventListener('click', function () { refreshStacks(); });

    var ms = $('monitor-start-btn');
    if (ms) ms.addEventListener('click', monitorStart);
    var mstop = $('monitor-stop-btn');
    if (mstop) mstop.addEventListener('click', function () { monitorStop(false); });
  }

  // ===== Compose 栈列表 =====
  function refreshStacks() {
    if (!state.serverId || state.inFlight) return;
    state.inFlight = true;
    AppBus.invoke('manage_list_stacks', { serverId: state.serverId }).then(function (list) {
      state.inFlight = false;
      renderStacks(list || []);
    }).catch(function (err) {
      state.inFlight = false;
      var msg = err && err.message ? err.message : String(err);
      showError('加载栈列表失败: ' + msg);
    });
  }

  function renderStacks(list) {
    var tbody = $('manage-stacks-tbody');
    if (!tbody) return;
    // 渲染期滚动保护 + 切 Tab 后首渲染恢复位置(见 withStageScrollGuard)
    withStageScrollGuard(function () { renderStacksInto(tbody, list); });
    consumePendingTabScroll('stacks');
  }

  function renderStacksInto(tbody, list) {
    if (list.length === 0) {
      tbody.innerHTML = '<tr><td class="empty-cell" colspan="3">未在服务器目录中发现 compose 项目</td></tr>';
      cState.stacks = [];
      return;
    }

    // 移除初始占位行(参照 renderContainers,否则占位行残留在数据行上方)
    var emptyCell = tbody.querySelector('.empty-cell');
    if (emptyCell) {
      var emptyRow = emptyCell.closest('tr');
      if (emptyRow) emptyRow.remove();
    }

    // 按 compose_file 做行差异更新(同 B 阶段卷/网络模式)
    var rowMap = {};
    var rows = tbody.querySelectorAll('tr[data-skid]');
    for (var i = 0; i < rows.length; i++) {
      rowMap[rows[i].getAttribute('data-skid')] = rows[i];
    }

    var seen = {};
    var frag = document.createDocumentFragment();
    for (var j = 0; j < list.length; j++) {
      var st = list[j];
      seen[st.compose_file] = true;
      var row = rowMap[st.compose_file];
      if (row) updateStackRow(row, st);
      else {
        row = document.createElement('tr');
        row.setAttribute('data-skid', st.compose_file);
        updateStackRow(row, st);
      }
      frag.appendChild(row);
    }
    for (var k in rowMap) {
      if (!seen[k]) rowMap[k].remove();
    }
    tbody.appendChild(frag);
    cState.stacks = list;
  }

  function updateStackRow(tr, st) {
    tr.innerHTML = '';
    // 目录
    var tdDir = document.createElement('td');
    tdDir.className = 'mono text-truncate';
    tdDir.textContent = st.dir || '—';
    if (st.dir) tdDir.title = st.dir;
    tr.appendChild(tdDir);
    // compose 文件
    var tdFile = document.createElement('td');
    tdFile.className = 'mono text-truncate';
    tdFile.textContent = st.compose_file || '—';
    if (st.compose_file) tdFile.title = st.compose_file;
    tr.appendChild(tdFile);
    // 操作:启动 / 停止 / 服务状态 / 日志
    var tdAction = document.createElement('td');
    tdAction.className = 'col-action';
    var wrap = document.createElement('div');
    wrap.className = 'action-btn-group';

    var upBtn = document.createElement('button');
    upBtn.type = 'button';
    upBtn.className = 'btn btn-sm';
    upBtn.textContent = '启动';
    upBtn.addEventListener('click', function () { confirmStackAction(st, 'up'); });
    wrap.appendChild(upBtn);

    var downBtn = document.createElement('button');
    downBtn.type = 'button';
    downBtn.className = 'btn btn-sm';
    downBtn.textContent = '停止';
    downBtn.addEventListener('click', function () { confirmStackAction(st, 'down'); });
    wrap.appendChild(downBtn);

    var psBtn = document.createElement('button');
    psBtn.type = 'button';
    psBtn.className = 'btn btn-sm';
    psBtn.textContent = '服务状态';
    psBtn.addEventListener('click', function () { showStackPs(st); });
    wrap.appendChild(psBtn);

    var logBtn = document.createElement('button');
    logBtn.type = 'button';
    logBtn.className = 'btn btn-sm';
    logBtn.textContent = '日志';
    logBtn.addEventListener('click', function () { showStackLogs(st); });
    wrap.appendChild(logBtn);

    tdAction.appendChild(wrap);
    tr.appendChild(tdAction);
  }

  function confirmStackAction(st, action) {
    var label = action === 'up' ? '启动' : '停止';
    openModal(label + '栈', buildConfirmBody(
      '确定' + label + ' compose 栈「' + (st.dir || st.compose_file) + '」吗?',
      label,
      function () {
        closeModal();
        doStackAction(st, action);
      }
    ));
  }

  function doStackAction(st, action) {
    if (!state.serverId) return;
    state.opInProgress = true;
    stopTimer();
    AppBus.invoke('manage_stack_action', {
      serverId: state.serverId,
      composeFile: st.compose_file,
      action: action
    }).then(function (res) {
      state.opInProgress = false;
      var label = action === 'up' ? '启动' : '停止';
      if (res.success) {
        toast(label + '成功', 'ok');
        refreshStacks();
        refreshOverview();
      } else {
        toast(label + '失败: ' + (res.message || '未知错误'), 'fail');
      }
      startTimerIfEnabled();
    }).catch(function (err) {
      state.opInProgress = false;
      var msg = err && err.message ? err.message : String(err);
      var label = action === 'up' ? '启动' : '停止';
      toast(label + '失败: ' + msg, 'fail');
      startTimerIfEnabled();
    });
  }

  // 栈服务状态:模态框内小表格
  function showStackPs(st) {
    var body = document.createElement('div');
    body.innerHTML =
      '<div class="table-wrap"><table class="data-table stack-ps-table">' +
      '<thead><tr><th>服务 SERVICE</th><th>状态 STATE</th></tr></thead>' +
      '<tbody id="stack-ps-tbody"><tr><td class="empty-cell" colspan="2">加载中…</td></tr></tbody>' +
      '</table></div>';
    openModal('服务状态 — ' + (st.dir || st.compose_file), body);

    AppBus.invoke('manage_stack_ps', {
      serverId: state.serverId,
      composeFile: st.compose_file
    }).then(function (list) {
      var tb = $('stack-ps-tbody');
      if (!tb) return;
      list = list || [];
      if (list.length === 0) {
        tb.innerHTML = '<tr><td class="empty-cell" colspan="2">无运行中的服务</td></tr>';
        return;
      }
      tb.innerHTML = '';
      for (var i = 0; i < list.length; i++) {
        var svc = list[i];
        var tr = document.createElement('tr');
        var tdName = document.createElement('td');
        tdName.className = 'mono';
        tdName.textContent = svc.name || svc.service || '—';
        tr.appendChild(tdName);
        var tdState = document.createElement('td');
        var s = (svc.state || '').toLowerCase();
        var badge = document.createElement('span');
        badge.className = 'badge ' +
          (s === 'running' ? 'badge-running' : (s === 'exited' ? 'badge-exited' : 'badge-info'));
        badge.textContent = svc.state || '未知';
        tdState.appendChild(badge);
        tr.appendChild(tdState);
        tb.appendChild(tr);
      }
    }).catch(function (err) {
      var tb = $('stack-ps-tbody');
      var msg = err && err.message ? err.message : String(err);
      if (tb) tb.innerHTML = '<tr><td class="empty-cell" colspan="2">加载失败: ' + escHtml(msg) + '</td></tr>';
    });
  }

  // 栈日志:复用日志模态的 tail 选择模式
  function showStackLogs(st) {
    var tail = 100;
    var body = document.createElement('div');
    body.innerHTML =
      '<div class="log-tail-bar">' +
      '<label>显示行数:' +
      '<select id="stack-log-tail-select" class="form-input form-input-sm">' +
      '<option value="100">100</option>' +
      '<option value="500">500</option>' +
      '<option value="1000">1000</option>' +
      '<option value="0">全部</option>' +
      '</select></label>' +
      '</div>' +
      '<pre id="stack-log-content" class="manage-log-body">加载中…</pre>';

    openModal('栈日志 — ' + (st.dir || st.compose_file), body);

    var tailSel = $('stack-log-tail-select');
    if (tailSel) tailSel.addEventListener('change', function () {
      tail = parseInt(tailSel.value, 10) || 100;
      fetchStackLogs(st, tail);
    });
    fetchStackLogs(st, tail);
  }

  function fetchStackLogs(st, tail) {
    var content = $('stack-log-content');
    if (!content) return;
    content.textContent = '加载中…';
    AppBus.invoke('manage_stack_logs', {
      serverId: state.serverId,
      composeFile: st.compose_file,
      tail: tail
    }).then(function (logs) {
      content.textContent = logs || '(无日志输出)';
    }).catch(function (err) {
      var msg = err && err.message ? err.message : String(err);
      content.textContent = '加载日志失败: ' + msg;
    });
  }

  // ===== 实时监控 =====
  function monitorStart() {
    if (!state.serverId) { toast('请先选择服务器', 'warn'); return; }
    if (cState.mon.running) { toast('监控已在运行中', 'info'); return; }
    var ivSel = $('monitor-interval-select');
    var intervalSecs = ivSel ? (parseInt(ivSel.value, 10) || 2) : 2;
    hideMonitorError();
    cState.mon.errShown = false;

    AppBus.invoke('manage_stats_start', {
      serverId: state.serverId,
      intervalSecs: intervalSecs
    }).then(function () {
      // 启动成功后再订阅事件,避免残留订阅
      return AppBus.on('manage-stats', onStatsEvent).then(function (unlisten) {
        cState.mon.unlisten = unlisten;
        cState.mon.running = true;
        updateMonitorUi();
      });
    }).catch(function (err) {
      var msg = err && err.message ? err.message : String(err);
      toast('启动监控失败: ' + msg, 'fail');
    });
  }

  function monitorStop(silent) {
    var had = cState.mon.running || cState.mon.unlisten;
    if (!had) return;
    cState.mon.running = false;
    if (cState.mon.unlisten) {
      try { cState.mon.unlisten(); } catch (e) { /* 忽略 */ }
      cState.mon.unlisten = null;
    }
    AppBus.invoke('manage_stats_stop', {}).catch(function () { /* 后端已停止时忽略 */ });
    updateMonitorUi();
    if (!silent) toast('监控已停止', 'info');
  }

  function updateMonitorUi() {
    var badge = $('monitor-badge');
    if (badge) {
      if (cState.mon.running) fillBadge(badge, 'running', '监控中');
      else fillBadge(badge, 'info', '已停止');
    }
    var startBtn = $('monitor-start-btn');
    if (startBtn) startBtn.disabled = cState.mon.running;
    var stopBtn = $('monitor-stop-btn');
    if (stopBtn) stopBtn.disabled = !cState.mon.running;
    var ivSel = $('monitor-interval-select');
    if (ivSel) ivSel.disabled = cState.mon.running;
  }

  function showMonitorError(msg, stopped) {
    var el = $('monitor-error');
    if (!el) return;
    el.textContent = '监控数据错误: ' + msg + (stopped ? '(监控已自动停止)' : '(下一轮将自动重试)');
    el.classList.remove('hidden');
  }

  function hideMonitorError() {
    var el = $('monitor-error');
    if (el) el.classList.add('hidden');
  }

  // 注意:Tauri 2 listen 回调参数是事件包裹对象 { event, id, payload },
  // 真实数据在 .payload 上(与 deploy.js / servers.js 的既有事件处理一致)
  function onStatsEvent(event) {
    var payload = event ? event.payload : null;
    if (!payload) return;
    // 其他服务器的数据(已切换服务器但未重启监控)直接丢弃
    if (payload.server_id && state.serverId && payload.server_id !== state.serverId) return;
    if (payload.error) {
      // 仅在后端已自行终止(权限拒绝/连续连接失败,payload.stopped=true)时
      // 停止本地监控;普通单轮失败后端会继续轮询,前端只提示不打断。
      // 每轮失败都有 banner;首轮失败再加 toast 强化提醒(避免用户没注意
      // banner 误以为「只刷了一次就停了」),同一失败段内不重复弹。
      showMonitorError(String(payload.error), !!payload.stopped);
      if (!cState.mon.errShown) {
        cState.mon.errShown = true;
        toast('监控数据异常: ' + String(payload.error), 'warn');
      }
      if (payload.stopped) monitorStop(true);
      return;
    }
    hideMonitorError();
    cState.mon.errShown = false;
    renderStats(payload.stats || []);
    // 徽章带上最后更新时间:帧是否还在持续到达一目了然
    // (docker stats 采集本身可能每轮 2~10s 以上,慢不等于停)
    var badge = $('monitor-badge');
    if (badge && cState.mon.running) {
      fillBadge(badge, 'running', '监控中 · ' + new Date().toTimeString().slice(0, 8));
    }
  }

  function renderStats(list) {
    var tbody = $('monitor-tbody');
    if (!tbody) return;
    // 渲染期滚动保护 + 切监控 Tab 后首帧恢复位置(见 withStageScrollGuard)
    withStageScrollGuard(function () { renderStatsInto(tbody, list); });
    consumePendingTabScroll('monitor');
  }

  function renderStatsInto(tbody, list) {
    if (list.length === 0) {
      tbody.innerHTML = '<tr><td class="empty-cell" colspan="7">暂无数据</td></tr>';
      return;
    }
    // 移除空占位行(与其它列表一致,否则占位行残留在数据行上方)
    var emptyCell = tbody.querySelector('.empty-cell');
    if (emptyCell) {
      var emptyRow = emptyCell.closest('tr');
      if (emptyRow) emptyRow.remove();
    }
    // 按容器 ID/名称做行差异更新。原实现每次事件整表 innerHTML 重建:同步重建
    // 本身不丢滚动位置,但容器数变化(监控期间启停容器)时内容高度突变,会把
    // 正在浏览中下部的 .stage 钳上去;差异更新保持未变行不动,消除该路径。
    var rowMap = {};
    var rows = tbody.querySelectorAll('tr[data-stat-key]');
    for (var i = 0; i < rows.length; i++) {
      rowMap[rows[i].getAttribute('data-stat-key')] = rows[i];
    }

    var seen = {};
    var frag = document.createDocumentFragment();
    for (var j = 0; j < list.length; j++) {
      var s = list[j];
      // docker stats 恒有 container_id;缺失时退化为名称,重名再加序号保唯一
      var base = s.container_id || s.name || 'row';
      var key = base;
      var dup = 1;
      while (seen[key]) { key = base + '~' + (dup++); }
      seen[key] = true;
      var tr = rowMap[key];
      if (!tr) {
        tr = document.createElement('tr');
        tr.setAttribute('data-stat-key', key);
      }
      updateStatRow(tr, s);
      frag.appendChild(tr);
    }

    for (var k in rowMap) {
      if (!seen[k]) rowMap[k].remove();
    }
    tbody.appendChild(frag);
  }

  function updateStatRow(tr, s) {
    tr.innerHTML = '';
    tr.appendChild(mkStatTd(s.name || s.container_id || '—', true));
    tr.appendChild(mkCpuTd(s.cpu_percent));
    tr.appendChild(mkStatTd(s.mem_usage || '—', true));
    tr.appendChild(mkStatTd(s.mem_percent != null ? String(s.mem_percent) : '—', false));
    tr.appendChild(mkStatTd(s.net_io || '—', true));
    tr.appendChild(mkStatTd(s.block_io || '—', true));
    tr.appendChild(mkStatTd(s.pids != null ? String(s.pids) : '—', false));
  }

  function mkStatTd(text, mono) {
    var td = document.createElement('td');
    if (mono) td.className = 'mono';
    td.textContent = text;
    return td;
  }

  function mkCpuTd(val) {
    var td = document.createElement('td');
    td.className = 'mono';
    var num = parseFloat(val);
    if (!isNaN(num)) {
      td.textContent = String(val);
      // CPU% 阈值着色:<50 正常,50-80 黄,>80 红
      if (num > 80) td.classList.add('stat-hot');
      else if (num >= 50) td.classList.add('stat-warm');
    } else {
      td.textContent = val || '—';
    }
    return td;
  }

  // ===== 容器 Exec 终端 =====
  function openTerminal(containerId, name) {
    // 多开防护:同一时间只允许一个终端会话
    if (cState.exec.sessionId || cState.exec.unlisten) {
      toast('已有终端会话,请先关闭当前终端', 'warn');
      return;
    }

    var body = document.createElement('div');
    // 终端弹窗专属标记:openModal 据此给共用 modal-card 加 .modal-terminal 放大
    body.className = 'manage-terminal-modal';
    body.innerHTML =
      '<div class="log-tail-bar">' +
      '<button id="term-close-btn" class="btn btn-sm btn-danger" type="button">关闭终端</button>' +
      '</div>' +
      '<pre id="term-output" class="manage-terminal">正在连接…</pre>' +
      '<div class="manage-terminal-input-row">' +
      '<label class="manage-terminal-shell">Shell:' +
      '<select id="term-shell-select" class="form-input form-input-sm">' +
      '<option value="">自动(推荐)</option>' +
      '<option value="bash">bash</option>' +
      '<option value="sh">sh</option>' +
      '</select></label>' +
      '<input id="term-input" class="manage-terminal-input" type="text" autocomplete="off" ' +
      'spellcheck="false" placeholder="输入命令,Enter 发送;↑/↓ 切换历史">' +
      '</div>';

    openModal('终端 — ' + name, body);

    var shellSel = $('term-shell-select');
    if (shellSel) shellSel.addEventListener('change', function () {
      // 切换 shell:停掉当前会话,用新 shell 重开
      stopExecSession(true);
      var out = $('term-output');
      resetTermBuffer();
      if (out) out.textContent = '正在连接…';
      startExec(containerId, name, shellSel.value);
    });

    var closeBtn = $('term-close-btn');
    if (closeBtn) closeBtn.addEventListener('click', function () {
      stopExecSession(false);
      closeModal();
    });

    var input = $('term-input');
    if (input) {
      input.addEventListener('keydown', onTermInputKey);
      input.focus();
    }
    var out = $('term-output');
    if (out) {
      // 用户向上滚动时暂停自动滚
      out.addEventListener('scroll', function () { /* 渲染时按位置判断,无需额外状态 */ });
    }

    startExec(containerId, name, shellSel ? shellSel.value : '');
  }

  function startExec(containerId, name, shell) {
    // 先订阅再 invoke:后端在命令返回前就可能开始推送(快速失败场景 eof
    // 会先于订阅到达),订阅期间的事件先入缓冲,拿到 session_id 后重放
    var buffered = [];
    var buffering = true;
    function bufferedHandler(payload) {
      if (buffering) { buffered.push(payload); return; }
      onExecOutput(payload);
    }
    var unsubscribe = null;

    AppBus.on('manage-exec-output', bufferedHandler).then(function (unlisten) {
      unsubscribe = unlisten;
      // invoke 已返回(正常路径):直接进入实时处理并重放缓冲;
      // 否则保持缓冲,由 invoke 的 then 分支接管
      if (!buffering) {
        cState.exec.unlisten = unlisten;
        var list = buffered || [];
        buffered = null;
        for (var i = 0; i < list.length; i++) onExecOutput(list[i]);
      }
    });

    AppBus.invoke('manage_exec_start', {
      serverId: state.serverId,
      containerId: containerId,
      // 空/未选 → null,由后端自动探测容器内可用 shell(bash 优先,退回 sh)
      shell: shell || null
    }).then(function (res) {
      // 模态框可能在等待期间被关闭
      if (!$('term-output')) {
        buffering = false;
        if (unsubscribe) { try { unsubscribe(); } catch (e) { /* 忽略 */ } }
        AppBus.invoke('manage_exec_stop', { sessionId: res.session_id }).catch(function () {});
        return;
      }
      cState.exec.sessionId = res.session_id;
      cState.exec.containerId = containerId;
      cState.exec.name = name || containerId;
      resetTermBuffer();
      // 回显后端返回的实际 shell(选「自动」时为探测结果,可能与所选不同)
      termAppendLine('已连接到容器「' + cState.exec.name + '」(shell: ' +
        (res.shell || shell || 'bash') + ')');
      renderTerm();
      buffering = false;
      // 订阅已就绪:挂载正式 unlisten 并重放缓冲中的早期事件(含快速 eof);
      // 订阅尚未 resolve:保持缓冲,由其 then 分支重放
      if (unsubscribe) {
        cState.exec.unlisten = unsubscribe;
        var list = buffered || [];
        buffered = null;
        for (var i = 0; i < list.length; i++) onExecOutput(list[i]);
      }
    }).catch(function (err) {
      buffering = false;
      if (unsubscribe) { try { unsubscribe(); } catch (e) { /* 忽略 */ } }
      var msg = err && err.message ? err.message : String(err);
      var out = $('term-output');
      if (out) out.textContent = '连接失败: ' + msg;
      toast('打开终端失败: ' + msg, 'fail');
    });
  }

  // 注意:Tauri 2 listen 回调参数是事件包裹对象 { event, id, payload }(同 onStatsEvent)
  function onExecOutput(event) {
    var payload = event ? event.payload : null;
    if (!payload) return;
    // 只处理当前会话的数据(旧会话残留事件丢弃)
    if (payload.session_id !== cState.exec.sessionId) return;
    if (payload.data) termWrite(String(payload.data));
    if (payload.eof) {
      // 后端附带结束原因(写失败/远端退出码/通道关闭);用户主动关闭不带原因
      if (payload.error) {
        termAppendLine('[会话已结束: ' + String(payload.error) + ']');
        toast('终端会话结束: ' + String(payload.error), 'warn');
      } else {
        termAppendLine('[会话已结束]');
      }
      cState.exec.eof = true;
      // eof 后释放会话与监听,避免泄漏
      releaseExecListener();
      cState.exec.sessionId = null;
      var input = $('term-input');
      if (input) input.disabled = true;
      renderTerm();
    }
  }

  // 简易 ANSI 处理:剥除 ESC 转义序列;\r 回到行首覆盖;\n 换行
  function termWrite(data) {
    // 先拼接上一块残留的不完整 ESC 序列,再缓存本块尾部的不完整序列
    if (cState.exec.pend) {
      data = cState.exec.pend + data;
      cState.exec.pend = '';
    }
    var idx = data.lastIndexOf('\x1b');
    if (idx !== -1 && /^\x1b(\[[0-9;?]*|\][^\x07]*)?$/.test(data.slice(idx))) {
      cState.exec.pend = data.slice(idx);
      data = data.slice(0, idx);
    }
    data = data.replace(/\x1b(\[[0-9;?]*[A-Za-z]|\][^\x07]*\x07|[@-Z\\-_])/g, '')
               .replace(/\x07/g, '');

    var ex = cState.exec;
    for (var i = 0; i < data.length; i++) {
      var ch = data[i];
      if (ch === '\n') {
        ex.lines.push(ex.cur);
        if (ex.lines.length > 1000) ex.lines.shift();
        ex.cur = '';
        ex.curIdx = 0;
      } else if (ch === '\r') {
        ex.curIdx = 0; // 回到行首,后续字符覆盖
      } else if (ch === '\t') {
        var pad = 4 - (ex.cur.length % 4);
        for (var t = 0; t < pad; t++) { ex.cur += ' '; ex.curIdx++; }
      } else if (ch >= ' ') {
        if (ex.curIdx < ex.cur.length) {
          ex.cur = ex.cur.slice(0, ex.curIdx) + ch + ex.cur.slice(ex.curIdx + 1);
        } else {
          ex.cur += ch;
        }
        ex.curIdx++;
      }
    }
    renderTerm();
  }

  function termAppendLine(text) {
    cState.exec.lines.push(text);
    if (cState.exec.lines.length > 1000) cState.exec.lines.shift();
  }

  function resetTermBuffer() {
    cState.exec.lines = [];
    cState.exec.cur = '';
    cState.exec.curIdx = 0;
    cState.exec.pend = '';
    cState.exec.eof = false;
  }

  function renderTerm() {
    var out = $('term-output');
    if (!out) return;
    // 用户未向上滚动(贴近底部)时才自动滚到底
    var atBottom = out.scrollTop + out.clientHeight >= out.scrollHeight - 40;
    var ex = cState.exec;
    out.textContent = ex.lines.join('\n') + (ex.lines.length ? '\n' : '') + ex.cur;
    if (atBottom) out.scrollTop = out.scrollHeight;
  }

  function onTermInputKey(e) {
    var input = e.target;
    var ex = cState.exec;
    if (e.key === 'Enter') {
      var line = input.value;
      if (!ex.sessionId) { toast('会话已结束,请关闭终端', 'warn'); return; }
      AppBus.invoke('manage_exec_write', { sessionId: ex.sessionId, data: line + '\r' })
        .catch(function () { /* 写失败忽略,输出流会体现 */ });
      if (line) {
        ex.history.push(line);
        if (ex.history.length > 100) ex.history.shift();
      }
      ex.histIdx = -1;
      input.value = '';
      e.preventDefault();
    } else if (e.key === 'ArrowUp') {
      if (ex.history.length === 0) return;
      if (ex.histIdx === -1) ex.histIdx = ex.history.length - 1;
      else if (ex.histIdx > 0) ex.histIdx--;
      input.value = ex.history[ex.histIdx];
      e.preventDefault();
    } else if (e.key === 'ArrowDown') {
      if (ex.histIdx === -1) return;
      if (ex.histIdx < ex.history.length - 1) {
        ex.histIdx++;
        input.value = ex.history[ex.histIdx];
      } else {
        ex.histIdx = -1;
        input.value = '';
      }
      e.preventDefault();
    }
  }

  // 关闭终端:通知后端停止会话 + unlisten(防泄漏)
  function stopExecSession(quiet) {
    var ex = cState.exec;
    if (ex.sessionId) {
      var sid = ex.sessionId;
      ex.sessionId = null;
      AppBus.invoke('manage_exec_stop', { sessionId: sid }).catch(function () { /* 忽略 */ });
    }
    releaseExecListener();
    ex.containerId = null;
    ex.history = [];
    ex.histIdx = -1;
    if (!quiet) toast('终端已关闭', 'info');
  }

  function releaseExecListener() {
    var ex = cState.exec;
    if (ex.unlisten) {
      try { ex.unlisten(); } catch (e) { /* 忽略 */ }
      ex.unlisten = null;
    }
  }

  // closeModal 钩子:模态框被关闭(含遮罩点击/关闭按钮)时清理终端会话
  function execOnModalClose() {
    // 还原共用模态尺寸:任何关闭路径(关闭按钮/遮罩点击/关闭终端)都经
    // closeModal 走到这里,移除终端态修饰类
    var modal = $('manage-modal');
    if (modal) {
      var card = modal.querySelector('.modal-card');
      if (card) card.classList.remove('modal-terminal');
    }
    var ex = cState.exec;
    if (ex.sessionId || ex.unlisten) {
      stopExecSession(true);
    }
  }

  // ===== C 阶段:离开 05 页清理 =====
  function onLeaveC() {
    monitorStop(true);
    if (cState.exec.sessionId || cState.exec.unlisten) {
      stopExecSession(true);
    }
    hideMonitorError();
  }

})();
