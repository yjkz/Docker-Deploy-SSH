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
    networks: [],  // B 阶段追加
    // 各 Tab 筛选关键字(第二十四批三;空串 = 不过滤)。过滤只影响渲染,
    // 下方四组全量列表保持原语义(终端「＋新标签」/迁移模态等读取点不受影响)
    filter: { containers: '', images: '', volumes: '', networks: '' },
    // 容器多选批量(第二十四批三):selected = { containerId: true },
    // 勾选列常显(无模式开关);选择态存 state 而非 DOM(自动刷新重建行
    // 后由 updateContainerRow 回填),切服务器整组清空。
    selected: {}
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
    // 自定义间隔的行内输入:应用按钮 + Enter 提交(与镜像拉取条同款键盘行为)
    var ivApply = $('manage-interval-apply');
    if (ivApply) ivApply.addEventListener('click', applyCustomInterval);
    var ivInput = $('manage-interval-custom');
    if (ivInput) ivInput.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') applyCustomInterval();
    });

    // 镜像拉取
    var pullBtn = $('manage-pull-btn');
    if (pullBtn) pullBtn.addEventListener('click', onPullImage);
    var pullInput = $('manage-pull-input');
    if (pullInput) pullInput.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') onPullImage();
    });

    // 阶段十:跨服务器镜像迁移
    var migrateBtn = $('manage-migrate-btn');
    if (migrateBtn) migrateBtn.addEventListener('click', openMigrateModal);

    // 模态框关闭
    var closeBtn = $('manage-modal-close');
    if (closeBtn) closeBtn.addEventListener('click', closeModal);
    var overlay = $('manage-modal');
    if (overlay) {
      overlay.addEventListener('click', function (e) {
        if (e.target === overlay) closeModal();
      });
      // Esc 关闭(仅当自己是顶层模态;第二十批 P1 修复:全局仲裁
      // window.isTopModal 见 app.js,叠模态一次 Esc 只关一层)
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && window.isTopModal('manage-modal')) closeModal();
      });
    }

    // B 阶段:创建卷 / 创建网络入口
    var vBtn = $('manage-volume-create-btn');
    if (vBtn) vBtn.addEventListener('click', showVolumeCreateModal);
    var nBtn = $('manage-network-create-btn');
    if (nBtn) nBtn.addEventListener('click', showNetworkCreateModal);

    // 第二十四批三:各 Tab 筛选输入(变更即重渲染,零请求;过滤 IME 组合态,
    // 同 04 页历史搜索先例——中文输入法组合中不触发,避免半成品关键字闪烁)
    bindFilterInput('manage-container-filter', 'containers');
    bindFilterInput('manage-image-filter', 'images');
    bindFilterInput('manage-volume-filter', 'volumes');
    bindFilterInput('manage-network-filter', 'networks');

    // 第二十四批三:容器批量操作(勾选列常显,无模式开关)
    var pickAll = $('manage-pick-all');
    if (pickAll) pickAll.addEventListener('change', function () { togglePickAll(pickAll.checked); });
    bindBatchAction('manage-batch-start', 'start', '启动');
    bindBatchAction('manage-batch-stop', 'stop', '停止');
    bindBatchAction('manage-batch-restart', 'restart', '重启');
    bindBatchAction('manage-batch-pause', 'pause', '暂停');
    bindBatchAction('manage-batch-unpause', 'unpause', '恢复');
    var renameBtn = $('manage-batch-rename');
    if (renameBtn) renameBtn.addEventListener('click', onBatchRename);
    var clearBtn = $('manage-batch-clear');
    if (clearBtn) clearBtn.addEventListener('click', function () { clearSelection(); });
  }

  /** 筛选输入接线:input 变更即按关键字重渲染该 Tab(IME 组合态过滤) */
  function bindFilterInput(inputId, tab) {
    var el = $(inputId);
    if (!el) return;
    el.addEventListener('input', function (e) {
      if (e.isComposing || e.keyCode === 229) return;
      state.filter[tab] = String(el.value || '').trim().toLowerCase();
      rerenderTab(tab);
    });
  }

  /** 按 Tab 名用当前全量列表重渲染(过滤生效,零网络请求) */
  function rerenderTab(tab) {
    if (tab === 'containers') renderContainers(state.containers);
    else if (tab === 'images') renderImages(state.images);
    else if (tab === 'volumes') renderVolumes(state.volumes);
    else if (tab === 'networks') renderNetworks(state.networks);
    else if (tab === 'stacks') window.ManageStacks.refreshVisibleStacks();
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
        // 已自动选中(恢复上次或默认第一台):表格立即转「加载中…」,
        // 明确告知正在连接取数(SSH 建连需数秒,原占位文案易被误读为要手动选)
        setTablePlaceholdersLoading();
        refreshAll();
      } else {
        setTablePlaceholdersNoServer(state.tab);
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
    // 第二十四批三:批量选择随之清空(新服务器容器集完全不同)
    state.selected = {};
    hideError();
    if (state.serverId) setTablePlaceholdersLoading();
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
      var tabActive = tabs[i].getAttribute('data-tab') === tab;
      tabs[i].classList.toggle('active', tabActive);
      // ARIA 状态同步:与 active 类保持一致(初始态见 index.html 的 aria-selected)
      tabs[i].setAttribute('aria-selected', tabActive ? 'true' : 'false');
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
    // pass 4(P4-F):面板显示时纵向微型 wipe(裁切揭示,与 modal-wipe /
    // page-reveal 同族)。「移除 → reflow → 复加」重放一次性动画;
    // reduced-motion 下由全局规则归零。
    var shown = document.querySelector('.manage-tab-panel:not(.hidden)');
    if (shown) {
      shown.classList.remove('panel-wipe');
      void shown.offsetWidth;
      shown.classList.add('panel-wipe');
    }
    if (tab !== 'monitor') monitorStop(true); // C 阶段追加:离开监控 Tab 自动停止
    // 面板高度已切换:立即恢复滚动位置(缓解钳制跳顶)
    applyPendingTabScrollNow();
    // 切到对应 Tab 时若尚未加载过则加载
    if (state.serverId) {
      // 尚未取过数据的表体仍是占位 → 转「加载中…」(已有数据的表体不动)
      if (tab === 'containers') { setTablePlaceholdersLoading(); refreshContainers(); }
      else if (tab === 'images') { setTablePlaceholdersLoading(); refreshImages(); }
      else if (tab === 'volumes') { setTablePlaceholdersLoading(); refreshVolumes(); }
      else if (tab === 'networks') { setTablePlaceholdersLoading(); refreshNetworks(); }
      else if (tab === 'stacks') { setTablePlaceholdersLoading(); refreshStacks(); } // C 阶段追加
    } else {
      pendingTabScroll = null; // 无服务器不会触发渲染,丢弃记忆
      setTablePlaceholdersNoServer(tab); // 明确提示(而非停留在旧占位文案)
    }
    resetTimer();
  }

  /**
   * 未选择服务器时(tab 切换进入)把该 Tab 的表体占位改为明确提示。
   * 与 [`setTablePlaceholdersLoading`] 分工:本函数用于「确实没选服务器」,
   * 后者用于「已选中、正在取数」。
   */
  function setTablePlaceholdersNoServer(tab) {
    var idByTab = {
      containers: 'manage-containers-tbody',
      images: 'manage-images-tbody',
      volumes: 'manage-volumes-tbody',
      networks: 'manage-networks-tbody',
      stacks: 'manage-stacks-tbody',
      // 监控表(体验审查 5-3):空态原是「点击「开始监控」获取实时数据」,
      // 未选服务器时点了才 toast,先把占位纠正为选择提示
      monitor: 'monitor-tbody'
    };
    var tbody = $(idByTab[tab] || '');
    if (!tbody) return;
    var cell = tbody.querySelector('.empty-cell');
    if (cell) cell.textContent = '请先在上方选择服务器';
  }

  // ===== 刷新总入口 =====
  function refreshAll() {
    if (!state.serverId) return;
    setTablePlaceholdersLoading();
    refreshOverview();
    if (state.tab === 'containers') refreshContainers();
    else if (state.tab === 'images') refreshImages();
    else if (state.tab === 'volumes') refreshVolumes();
    else if (state.tab === 'networks') refreshNetworks();
    else if (state.tab === 'stacks') refreshStacks(); // C 阶段追加
  }

  /**
   * 把仍处于初始占位(「选择服务器后加载」)的表体改为「加载中…」。
   * 反直觉场景:进入 05 页会自动选中服务器并发起请求,但 SSH 连接需数秒,
   * 期间表格仍是静态占位文案,用户以为要自己先选服务器。加载态文案让其
   * 明确「已选中、正在取数」。仅替换仍是占位的表体,已有数据的表体不动
   * (避免刷新时把列表整片闪成占位)。
   */
  function setTablePlaceholdersLoading() {
    ['manage-containers-tbody', 'manage-images-tbody', 'manage-volumes-tbody',
      'manage-networks-tbody', 'manage-stacks-tbody'].forEach(function (id) {
        var tbody = $(id);
        if (!tbody) return;
        var cell = tbody.querySelector('.empty-cell');
        if (cell && /选择服务器后加载/.test(cell.textContent || '')) {
          cell.textContent = '加载中…';
        }
      });
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
      // 宿主机性能指标(空串 = 采样不可用,/proc 缺失等 → 「—」)
      $('ov-cpu').textContent = ov.cpu_percent
        ? ov.cpu_percent + (ov.cpu_cores ? '(' + ov.cpu_cores + ' 核)' : '')
        : '—';
      $('ov-mem').textContent = ov.mem_total
        ? (ov.mem_used || '—') + ' / ' + ov.mem_total + (ov.mem_percent ? '(' + ov.mem_percent + ')' : '')
        : '—';
      // 宿主机文件系统用量(第十批):根分区恒展示;Docker 数据目录
      // (/var/lib/docker)独立挂载时追加「Docker 数据盘」一段 —— 与根分区
      // 同盘时后端不单列,隐藏该段(宽格第二行,采样不可用则整行不出现)
      var hostDisk = $('ov-hostdisk');
      var diskParts = [];
      if (ov.root_disk_total) {
        diskParts.push('根分区 / ' + (ov.root_disk_used || '—') + ' / ' + ov.root_disk_total +
          (ov.root_disk_percent ? '(' + ov.root_disk_percent + ')' : ''));
      }
      if (ov.docker_disk_mount && ov.docker_disk_total) {
        diskParts.push('Docker 数据盘 ' + ov.docker_disk_mount + ' ' +
          (ov.docker_disk_used || '—') + ' / ' + ov.docker_disk_total +
          (ov.docker_disk_percent ? '(' + ov.docker_disk_percent + ')' : ''));
      }
      hostDisk.textContent = diskParts.join(' · ');
      hostDisk.classList.toggle('is-on', diskParts.length > 0);
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
      tbody.innerHTML = '<tr><td class="empty-cell" colspan="7">暂无容器</td></tr>';
      state.containers = [];
      renderBatchBar();
      return;
    }

    // 第二十四批三:筛选只影响渲染(收尾 state.containers 仍赋全量;
    // 「＋新标签」下拉等读取点不受影响);空态区分「无匹配」与「暂无数据」
    var fullList = list;
    var visible = list.filter(containerMatchesFilter);
    if (visible.length === 0) {
      tbody.innerHTML = '<tr><td class="empty-cell" colspan="7">无匹配的容器</td></tr>';
      state.containers = list;
      renderBatchBar();
      return;
    }
    list = visible;

    // 索引现有数据行
    var rowMap = {};
    var rows = tbody.querySelectorAll('tr[data-cid]');
    for (var i = 0; i < rows.length; i++) {
      rowMap[rows[i].getAttribute('data-cid')] = rows[i];
    }

    var seen = {};
    var frag = document.createDocumentFragment();
    var needInspect = []; // 新造详情行待载(见上方注释)

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

      // 详情行(inspect 折叠面板);新建行登记待载(append 后统一 loadInspect,
      // 覆盖「筛掉又恢复」路径的缓存重放)
      var detailSel = 'tr[data-cid-detail="' + c.id + '"]';
      var detailRow = tbody.querySelector(detailSel);
      if (state.expanded[c.id]) {
        if (!detailRow) {
          detailRow = createDetailRow(c.id);
          needInspect.push(c.id);
        }
        frag.appendChild(detailRow);
      } else if (detailRow) {
        detailRow.remove();
      }
    }

    // 移除行:区分「真正消失」(清 expanded/cache)与「仅被筛掉」(只移 DOM,
    // 保住展开态与 inspect 缓存 —— 筛选清除后恢复展开,第二十四批三)
    var alive = {};
    for (var m = 0; m < fullList.length; m++) alive[fullList[m].id] = true;
    for (var id in rowMap) {
      if (!seen[id]) {
        rowMap[id].remove();
        var d = tbody.querySelector('tr[data-cid-detail="' + id + '"]');
        if (d) d.remove();
        if (!alive[id]) {
          delete state.expanded[id];
          delete state.expandedPorts[id];
          delete state.inspectCache[id];
        }
      }
    }

    tbody.appendChild(frag);
    state.containers = fullList;
    // 第二十四批三:容器已不存在的选中项清理 + 批量条/全选复位
    pruneSelection(fullList);
    renderBatchBar();
    // 新造详情行补载(缓存命中即渲染,未命中走请求;append 后行已在 DOM)
    for (var ni = 0; ni < needInspect.length; ni++) loadInspect(needInspect[ni]);
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
    // 勾选单元格(第二十四批三;常显)。勾选态存 state.selected,
    // 行重建后按 state 回填 —— 自动刷新重建行不丢勾选。
    var tdPick = document.createElement('td');
    tdPick.className = 'col-pick';
    var pick = document.createElement('input');
    pick.type = 'checkbox';
    pick.checked = !!state.selected[c.id];
    pick.title = '选择容器';
    pick.setAttribute('aria-label', '选择容器 ' + (c.names || c.id));
    pick.addEventListener('change', function () {
      if (pick.checked) state.selected[c.id] = true;
      else delete state.selected[c.id];
      renderBatchBar();
    });
    tdPick.appendChild(pick);
    tr.appendChild(tdPick);
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
    tdCreated.className = 'mono';
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
        // 第二十二批:传 compose 项目名(同栈广播按它限定范围;非 compose 容器为 null)
        openTerminal(c.id, c.names || c.id, c.compose_project || null);
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
    // 忙碌反馈(体验审查 5-2):SSH 往返数秒,期间按钮转忙防连点/误判未生效。
    // 按 data-cid 定位该行操作按钮组(整组禁用,setBtnBusy 只管单个)
    setRowActionsBusy(containerId, true);
    AppBus.invoke('manage_container_action', {
      serverId: state.serverId,
      containerId: containerId,
      action: action
    }).then(function (res) {
      state.opInProgress = false;
      setRowActionsBusy(containerId, false);
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
      setRowActionsBusy(containerId, false);
      var msg = err && err.message ? err.message : String(err);
      toast(label + '失败: ' + msg, 'fail');
      startTimerIfEnabled();
    });
  }

  /** 按 data-cid 定位行内操作按钮组并整体启停(行可能已被刷新移除 → 静默跳过) */
  function setRowActionsBusy(containerId, busy) {
    var row = document.querySelector('tr[data-cid="' + containerId + '"]');
    if (!row) return;
    var btns = row.querySelectorAll('.action-btn-group .btn');
    for (var i = 0; i < btns.length; i++) btns[i].disabled = busy;
  }

  function confirmRemoveContainer(containerId) {
    var c = state.containers.find(function (x) { return x.id === containerId; });
    var name = c ? (c.names || containerId) : containerId;
    var isRunning = c && (c.state || '').toLowerCase() === 'running';
    var title = '确定删除容器「' + name + '」吗?';
    var risk = isRunning ? '该容器正在运行,删除将强制停止并删除该容器。' : null;

    openModal('删除容器', buildConfirmBody(title, '删除', function () {
      closeModal();
      doContainerAction(containerId, 'rm', '删除容器');
    }, null, risk));
  }

  // ===== 第二十四批三:容器筛选 + 多选批量 =====

  /** 容器筛选谓词(不区分大小写子串;名称/镜像/状态三字段;空关键字恒真) */
  function containerMatchesFilter(c) {
    var kw = state.filter.containers;
    if (!kw) return true;
    var hay = [(c.names || ''), (c.image || ''), (c.state || '')].join(' ').toLowerCase();
    return hay.indexOf(kw) !== -1;
  }

  /** 当前「筛选后可见」的容器列表(批量执行与全选均以其为范围) */
  function visibleContainers() {
    return state.containers.filter(containerMatchesFilter);
  }

  /** 选中项清理:容器已不存在于最新全量列表时移除(切服务器时另有整组清空) */
  function pruneSelection(list) {
    var alive = {};
    for (var i = 0; i < list.length; i++) alive[list[i].id] = true;
    for (var id in state.selected) {
      if (!alive[id]) delete state.selected[id];
    }
  }

  function selectedIds() {
    return Object.keys(state.selected);
  }

  function clearSelection() {
    state.selected = {};
    // 行内勾选框状态同步(DOM 重建成本低,直接对可见行复位)
    var boxes = document.querySelectorAll('#manage-containers-tbody input[type=checkbox]');
    for (var i = 0; i < boxes.length; i++) boxes[i].checked = false;
    renderBatchBar();
  }

  /** 批量条渲染:计数 + 全选框三态(未选/部分/全选);无选中时隐藏操作按钮行 */
  function renderBatchBar() {
    var bar = $('manage-batch-bar');
    var countEl = $('manage-batch-count');
    var pickAll = $('manage-pick-all');
    if (!bar) return;
    var n = selectedIds().length;
    if (n === 0) {
      bar.classList.add('hidden');
    } else {
      bar.classList.remove('hidden');
    }
    if (countEl) countEl.textContent = '已选 ' + n + ' 个';
    // 全选框三态:可见项全选 = checked;部分 = indeterminate
    if (pickAll) {
      var vis = visibleContainers();
      var selectedVisible = 0;
      for (var i = 0; i < vis.length; i++) {
        if (state.selected[vis[i].id]) selectedVisible++;
      }
      pickAll.checked = vis.length > 0 && selectedVisible === vis.length;
      pickAll.indeterminate = selectedVisible > 0 && selectedVisible < vis.length;
    }
  }

  /** 全选/取消全选(范围 = 当前筛选后可见项;隐藏未选中行不受影响) */
  function togglePickAll(checked) {
    var vis = visibleContainers();
    for (var i = 0; i < vis.length; i++) {
      if (checked) state.selected[vis[i].id] = true;
      else delete state.selected[vis[i].id];
    }
    // 可见行勾选框同步
    for (var j = 0; j < vis.length; j++) {
      var row = document.querySelector('tr[data-cid="' + vis[j].id + '"]');
      if (row) {
        var box = row.querySelector('.col-pick input');
        if (box) box.checked = !!state.selected[vis[j].id];
      }
    }
    renderBatchBar();
  }

  /** 批量条动作按钮接线(执行循环共用,见 runBatchAction) */
  function bindBatchAction(btnId, action, label) {
    var btn = $(btnId);
    if (!btn) return;
    btn.addEventListener('click', function () { runBatchAction(action, label); });
  }

  /**
   * 批量执行:对选中容器**串行**逐台调用 manage_container_action(与部署
   * 批量的串行纪律一致;避免瞬时并发连发 SSH 会话风暴),完成后一次刷新。
   * 结果 toast 汇总:全部成功 / 部分失败列出失败容器名(最多 5 个)。
   */
  function runBatchAction(action, label) {
    if (!state.serverId) return;
    var ids = selectedIds();
    if (ids.length === 0) return;
    if (state.opInProgress) { toast('已有操作进行中,请稍候', 'warn'); return; }
    state.opInProgress = true;
    stopTimer();
    setBatchBusy(true);
    var nameOf = {};
    for (var i = 0; i < state.containers.length; i++) {
      nameOf[state.containers[i].id] = state.containers[i].names || state.containers[i].id;
    }
    var failed = [];
    var done = 0;
    var total = ids.length;
    var next = function () {
      if (done >= total) {
        state.opInProgress = false;
        setBatchBusy(false);
        if (failed.length === 0) {
          toast('批量' + label + '成功(' + total + ' 个)', 'ok');
        } else {
          var shown = failed.slice(0, 5).join('、');
          var more = failed.length > 5 ? ' 等 ' + failed.length + ' 个' : '';
          toast('批量' + label + '完成:成功 ' + (total - failed.length) + ' 个,失败:' + shown + more, 'fail');
        }
        clearSelection();
        refreshContainers();
        refreshOverview();
        startTimerIfEnabled();
        return;
      }
      var cid = ids[done];
      done++;
      AppBus.invoke('manage_container_action', {
        serverId: state.serverId,
        containerId: cid,
        action: action
      }).then(function (res) {
        if (!res || res.success !== true) {
          failed.push(nameOf[cid] || cid);
        }
        next();
      }).catch(function () {
        failed.push(nameOf[cid] || cid);
        next();
      });
    };
    next();
  }

  /** 批量执行期禁用批量条按钮与勾选框(防中途改选/连点) */
  function setBatchBusy(busy) {
    var bar = $('manage-batch-bar');
    if (bar) {
      var btns = bar.querySelectorAll('button');
      for (var i = 0; i < btns.length; i++) btns[i].disabled = busy;
    }
    var pickAll = $('manage-pick-all');
    if (pickAll) pickAll.disabled = busy;
    var boxes = document.querySelectorAll('#manage-containers-tbody .col-pick input');
    for (var j = 0; j < boxes.length; j++) boxes[j].disabled = busy;
  }

  /** 批量改名:仅选中 1 个时可用;模态输入新名 → manage_container_action(rename) */
  function onBatchRename() {
    var ids = selectedIds();
    if (ids.length !== 1) {
      toast('改名仅支持选中 1 个容器', 'warn');
      return;
    }
    var cid = ids[0];
    var c = null;
    for (var i = 0; i < state.containers.length; i++) {
      if (state.containers[i].id === cid) { c = state.containers[i]; break; }
    }
    var oldName = c ? (c.names || cid) : cid;

    var body = document.createElement('div');
    body.innerHTML =
      '<div class="form-row">' +
      '<label class="form-label" for="rename-old">当前名称</label>' +
      '<input id="rename-old" class="form-input" type="text" value="' + escHtml(oldName) + '" readonly>' +
      '</div>' +
      '<div class="form-row">' +
      '<label class="form-label" for="rename-new">新名称</label>' +
      '<input id="rename-new" class="form-input" type="text" placeholder="字母或数字开头,可含 _ . -" autocomplete="off">' +
      '</div>' +
      '<div class="modal-actions">' +
      '<button id="rename-confirm-btn" class="btn btn-primary" type="button">确认改名</button>' +
      '</div>';

    openModal('容器改名', body);
    var newInput = $('rename-new');
    if (newInput) {
      newInput.value = oldName;
      newInput.focus();
      newInput.select();
      newInput.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') doBatchRename(cid, oldName);
      });
    }
    var confirmBtn = $('rename-confirm-btn');
    if (confirmBtn) confirmBtn.addEventListener('click', function () { doBatchRename(cid, oldName); });
  }

  function doBatchRename(containerId, oldName) {
    var input = $('rename-new');
    if (!input) return;
    var name = input.value.trim();
    // 前端预校验(与后端 validate_container_name 同口径;后端仍有权威校验)
    if (!/^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,127}$/.test(name)) {
      window.setFieldError(input, '须字母或数字开头,仅含字母、数字、下划线、点与连字符,长度 1-128');
      toast('容器名不合法', 'warn');
      return;
    }
    if (name === oldName) {
      toast('新名称与当前名称相同', 'warn');
      return;
    }
    state.opInProgress = true;
    stopTimer();
    AppBus.invoke('manage_container_action', {
      serverId: state.serverId,
      containerId: containerId,
      action: 'rename',
      newName: name
    }).then(function (res) {
      state.opInProgress = false;
      if (res && res.success) {
        toast('改名成功:「' + oldName + '」→「' + name + '」', 'ok');
        closeModal();
        clearSelection();
        refreshContainers();
      } else {
        toast('改名失败: ' + ((res && res.message) || '未知错误'), 'fail');
      }
      startTimerIfEnabled();
    }).catch(function (err) {
      state.opInProgress = false;
      var msg = err && err.message ? err.message : String(err);
      toast('改名失败: ' + msg, 'fail');
      startTimerIfEnabled();
    });
  }

  // ===== 容器详情(inspect) =====
  function createDetailRow(containerId) {
    var tr = document.createElement('tr');
    tr.setAttribute('data-cid-detail', containerId);
    tr.className = 'container-detail-row';
    var td = document.createElement('td');
    td.colSpan = 7; // 第二十四批三:勾选列 +1
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
    // 查看类弹窗放大标记:openModal 据此给共用 modal-card 加 .modal-wide
    body.className = 'manage-wide-modal';
    var bar1 = document.createElement('div');
    bar1.className = 'log-tail-bar';
    bar1.innerHTML =
      '<label>显示行数:' +
      '<select id="log-tail-select" class="form-input form-input-sm">' +
      '<option value="100">100</option>' +
      '<option value="500">500</option>' +
      '<option value="1000">1000</option>' +
      '<option value="0">全部</option>' +
      '</select></label>' +
      '<button id="log-copy-btn" class="btn btn-sm" type="button">复制日志</button>';
    // 阶段九:实时跟随开关(勾选开流,取消停流;eof 后自动复位)
    bar1.appendChild(buildFollowToggle('container', containerId, 'log-tail-select', 'log-content'));
    var pre1 = document.createElement('pre');
    pre1.id = 'log-content';
    pre1.className = 'manage-log-body';
    pre1.textContent = '加载中…';
    body.appendChild(bar1);
    body.appendChild(pre1);

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

    // 第二十四批三:筛选只影响渲染(收尾 state.images 仍赋全量,迁移模态读全量)
    var full = list;
    var kwImg = state.filter.images;
    if (kwImg) {
      list = list.filter(function (img) {
        return ((img.repository || '') + ' ' + (img.tag || '')).toLowerCase().indexOf(kwImg) !== -1;
      });
      if (list.length === 0) {
        tbody.innerHTML = '<tr><td class="empty-cell" colspan="6">无匹配的镜像</td></tr>';
        state.images = full;
        return;
      }
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
    state.images = full;
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
    tdSize.className = 'mono';
    tdSize.textContent = img.size || '—';
    tr.appendChild(tdSize);
    // 创建时间(格式化 MM-DD HH:mm,tooltip 显示完整)
    var tdCreated = document.createElement('td');
    tdCreated.className = 'mono';
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
    if (!image) {
      window.setFieldError(input, '此项必填');
      toast('请输入镜像名', 'warn');
      return;
    }

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
      '确定删除镜像「' + fullImage + '」吗?',
      '删除',
      function () {
        var forceCheck = $('manage-force-check');
        var force = forceCheck ? forceCheck.checked : false;
        closeModal();
        doImageRemove(imageId, force);
      },
      true,  // 显示 force 复选框
      '如果该镜像被容器引用将删除失败,可勾选下方强制删除。'
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
    if (!tag) {
      window.setFieldError(newTag, '此项必填');
      toast('请输入新标签', 'warn');
      return;
    }

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

  // ===== 阶段十:跨服务器镜像迁移 =====
  // - openMigrateModal:镜像多选 + 目标服务器下拉(排除当前服务器)
  // - migrate_start:manage_log... 复用事件先订阅再 invoke 的纪律(migrate-log /
  //   migrate-done);逐镜像进度与结果经事件,invoke 立即返回
  // - migrate_status(true) 取消(镜像边界生效);模态关闭不中断迁移,
  //   迁移中禁「迁移镜像」按钮与刷新
  var migState = {
    active: false,      // 是否有迁移在执行
    id: 0,              // 最近一次 migrate_status 查询到的代号(事件归属判别)
    listenerBound: false,
    selected: []        // 开流时勾选的镜像快照(事件到来时高亮用)
  };

  function bindMigrateListener() {
    if (migState.listenerBound) return;
    migState.listenerBound = true;
    AppBus.on('migrate-log', function (event) {
      var p = (event && event.payload) || {};
      appendMigrateLine(String(p.line || ''));
    }).catch(function (err) {
      if (window.console && console.warn) console.warn('[manage] migrate-log 注册失败:', err);
    });
    AppBus.on('migrate-done', function (event) {
      var p = (event && event.payload) || {};
      migState.active = false;
      setMigrateBusy(false);
      refreshImages(); // 镜像列表不因迁移而变,但刷新 overview 的容器状态无害
      var ok = p.success === true;
      var msg = String(p.message || (ok ? '迁移完成' : '迁移失败'));
      appendMigrateLine('—— ' + msg + ' ——');
      toast(ok ? '镜像迁移完成' : '镜像迁移失败: ' + msg, ok ? 'ok' : 'fail');
    }).catch(function (err) {
      if (window.console && console.warn) console.warn('[manage] migrate-done 注册失败:', err);
    });
  }

  function appendMigrateLine(line) {
    var content = $('migrate-log-content');
    if (!content) return;
    var nearBottom = content.scrollHeight - content.scrollTop - content.clientHeight < 40;
    content.appendChild(document.createTextNode(line + '\n'));
    if (nearBottom) content.scrollTop = content.scrollHeight;
  }

  function setMigrateBusy(busy) {
    var btn = $('manage-migrate-btn');
    if (btn) {
      btn.disabled = busy || !state.serverId;
      btn.textContent = busy ? '迁移中…' : '迁移镜像';
    }
  }

  function openMigrateModal() {
    if (migState.active) return;
    if (!state.serverId) { toast('请先选择源服务器', 'warn'); return; }
    var images = state.images || [];
    if (images.length === 0) { toast('当前服务器没有可迁移的镜像', 'warn'); return; }
    // 先取最新服务器列表(构建目标下拉用),再打开模态
    AppBus.invoke('manage_list_servers').then(function (servers) {
      var others = (servers || []).filter(function (s) { return s.id !== state.serverId; });
      if (others.length === 0) { toast('没有其他服务器可作为迁移目标', 'warn'); return; }
      openMigrateModalBody(others, images);
    }).catch(function (err) {
      var msg = err && err.message ? err.message : String(err);
      toast('加载服务器列表失败: ' + msg, 'fail');
    });
  }

  function openMigrateModalBody(servers, images) {
    bindMigrateListener();

    var body = document.createElement('div');
    // 查看类弹窗放大标记(日志区较宽)
    body.className = 'manage-wide-modal';
    var bar = document.createElement('div');
    bar.className = 'log-tail-bar';
    var selHtml = '<option value="">选择目标服务器…</option>';
    for (var i = 0; i < servers.length; i++) {
      selHtml += '<option value="' + escHtml(servers[i].id) + '">' +
        escHtml(servers[i].name || servers[i].id) + ' (' + escHtml(servers[i].host || '') + ')</option>';
    }
    bar.innerHTML = '<label>目标服务器:<select id="migrate-target-select" class="form-input form-input-sm">' + selHtml + '</select></label>';
    body.appendChild(bar);

    var listWrap = document.createElement('div');
    listWrap.className = 'migrate-image-list';
    for (var j = 0; j < images.length; j++) {
      var img = images[j];
      var ref = (img.repository || '') + ':' + (img.tag || 'latest');
      var row = document.createElement('label');
      row.className = 'deploy-checkbox migrate-image-item';
      row.innerHTML =
        '<input type="checkbox" data-migrate-image="' + escHtml(ref) + '">' +
        '<span>' + escHtml(ref) + ' <span class="migrate-image-size">' + escHtml(img.size || '') + '</span></span>';
      listWrap.appendChild(row);
    }
    body.appendChild(listWrap);

    var log = document.createElement('pre');
    log.id = 'migrate-log-content';
    log.className = 'manage-log-body migrate-log-body';
    log.textContent = '选择目标服务器与镜像后,点「开始迁移」。';
    body.appendChild(log);

    var actions = document.createElement('div');
    actions.className = 'modal-actions';
    actions.innerHTML =
      '<button id="migrate-cancel-btn" class="btn" type="button">取消</button>' +
      '<button id="migrate-start-btn" class="btn btn-primary" type="button">开始迁移</button>';
    body.appendChild(actions);

    openModal('迁移镜像 — 当前服务器', body);

    var cancelBtn = $('migrate-cancel-btn');
    if (cancelBtn) {
      cancelBtn.addEventListener('click', function () {
        // 迁移进行中:先请求取消(镜像边界生效),再关模态;未在迁移则直接关
        if (migState.active) {
          AppBus.invoke('migrate_status', { cancel: true }).then(function () {
            toast('已请求取消,当前镜像传输完成后生效', 'info');
          }).catch(function (err) {
            var msg = err && err.message ? err.message : String(err);
            toast('取消请求失败: ' + msg, 'warn');
          });
        }
        closeModal();
      });
    }
    var startBtn = $('migrate-start-btn');
    if (startBtn) {
      startBtn.addEventListener('click', function () {
        var targetSel = $('migrate-target-select');
        var targetId = targetSel ? String(targetSel.value) : '';
        if (!targetId) { toast('请选择目标服务器', 'warn'); return; }
        var nodes = document.querySelectorAll('input[data-migrate-image]');
        var imagesToMigrate = [];
        for (var k = 0; k < nodes.length; k++) {
          if (nodes[k].checked) imagesToMigrate.push(nodes[k].getAttribute('data-migrate-image'));
        }
        if (imagesToMigrate.length === 0) { toast('请至少勾选一个镜像', 'warn'); return; }
        var content = $('migrate-log-content');
        if (content) content.textContent = '';
        migState.active = true;
        setMigrateBusy(true);
        startBtn.disabled = true;
        AppBus.invoke('migrate_images', {
          req: {
            sourceId: state.serverId,
            targetId: targetId,
            images: imagesToMigrate,
            sourcePasswordPlain: null,
            targetPasswordPlain: null
          }
        }).then(function () {
          // 同步返回 null 不代表成功;结果只经 migrate-done 事件
        }).catch(function (err) {
          migState.active = false;
          setMigrateBusy(false);
          if (startBtn) startBtn.disabled = false;
          var msg = err && err.message ? err.message : String(err);
          appendMigrateLine('—— 迁移发起失败: ' + msg + ' ——');
        });
      });
    }
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

    // 第二十四批三:筛选只影响渲染(收尾 state.volumes 仍赋全量)
    var full = list;
    var kwVol = state.filter.volumes;
    if (kwVol) {
      list = list.filter(function (v) {
        return ((v.name || '') + ' ' + (v.driver || '') + ' ' + (v.mountpoint || '')).toLowerCase().indexOf(kwVol) !== -1;
      });
      if (list.length === 0) {
        tbody.innerHTML = '<tr><td class="empty-cell" colspan="5">无匹配的卷</td></tr>';
        state.volumes = full;
        return;
      }
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
    state.volumes = full;
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
    tdCreated.className = 'mono';
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

    // 内容浏览 + 单卷备份(第二十六批)
    var browseBtn = document.createElement('button');
    browseBtn.type = 'button';
    browseBtn.className = 'btn btn-sm';
    browseBtn.textContent = '内容';
    browseBtn.title = '浏览卷内文件(逐层展开,只读)';
    browseBtn.addEventListener('click', function () { showVolumeBrowser(v.name); });
    wrap.appendChild(browseBtn);

    var backupBtn = document.createElement('button');
    backupBtn.type = 'button';
    backupBtn.className = 'btn btn-sm';
    backupBtn.textContent = '备份';
    backupBtn.title = '把整个卷打包为 tar.gz 下载到本机';
    backupBtn.addEventListener('click', function () { confirmVolumeBackup(v.name); });
    wrap.appendChild(backupBtn);

    var rmBtn = document.createElement('button');
    rmBtn.type = 'button';
    rmBtn.className = 'btn btn-sm btn-danger';
    rmBtn.textContent = '删除';
    rmBtn.addEventListener('click', function () { confirmRemoveVolume(v.name); });
    wrap.appendChild(rmBtn);

    tdAction.appendChild(wrap);
    tr.appendChild(tdAction);
  }

  // ===== 卷内容浏览 + 单卷备份(第二十六批)=====
  //
  // 浏览:「内容」按钮 → 模态内逐层列目录(每层一次 manage_volume_browse;
  // 卷可能有百万文件,全量列会把输出撑爆,故**按需展开**而非一次递归)。
  // 备份:「备份」按钮 → 两步确认 → 系统保存对话框选本地路径 →
  // manage_volume_backup 后台跑(进度经 volume-backup-progress 事件)。
  // 后端两条命令都是**只读**远端(浏览 tar tzvf;备份 tar 打包后拉回、
  // 临时包用完即删),故不取远程操作互斥位。

  var volBrowse = { name: '', stack: [] };   // stack = 路径栈(面包屑)

  /**
   * 卷浏览专用元素助手(本地定义)。
   *
   * **为什么不用 `el`**:manage.js 里没有全局/模块级 `el` —— 它是其它文件的局部
   * 助手,且本文件若干函数有 `var el = $('#…')` 的局部遮蔽。裸引用会在**点击时**
   * 才炸(ReferenceError),静态检查与页面加载都发现不了(与 v5.11「拆分丢 $」
   * 同一类事故)。本文件新增的卷浏览代码一律走本助手,或直接用 createElement。
   */
  function mkEl(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  /** 1024 进制字节 → "1.2 MB"(卷浏览行内展示;本地实现,不跨文件依赖) */
  function volFormatBytes(bytes) {
    var n = Number(bytes);
    if (!isFinite(n) || n < 0) n = 0;
    var units = ['B', 'KB', 'MB', 'GB', 'TB'];
    var i = 0;
    while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
    return n.toFixed(1) + ' ' + units[i];
  }

  /** 打开卷浏览模态(根层) */
  function showVolumeBrowser(volumeName) {
    volBrowse.name = volumeName;
    volBrowse.stack = [''];
    renderVolumeBrowser();
  }

  /** 拉取当前层并渲染(失败就地给错误与重试) */
  function renderVolumeBrowser() {
    var name = volBrowse.name;
    var sub = volBrowse.stack[volBrowse.stack.length - 1];
    var body = document.createElement('div');
    body.className = 'manage-wide-modal';
    var head = document.createElement('div');
    head.className = 'log-tail-bar';
    head.appendChild(mkEl('span', 'manage-env-hint', '卷:' + name + (sub ? '/' + sub : '/')));
    body.appendChild(head);
    var box = document.createElement('div');
    box.className = 'vol-browse';
    box.appendChild(mkEl('div', 'server-check-hint', '正在读取目录…'));
    body.appendChild(box);
    openModal('卷内容 — ' + name, body);

    AppBus.invoke('manage_volume_browse', {
      serverId: state.serverId,
      volumeName: name,
      subPath: sub || null
    }).then(function (list) {
      renderVolumeBrowserInto(box, sub, list || []);
    }).catch(function (err) {
      box.textContent = '';
      box.appendChild(mkEl('div', 'cleanup-error',
        '读取目录失败:' + (err && err.message ? err.message : String(err))));
      var retry = mkEl('button', 'btn btn-sm', '重试');
      retry.type = 'button';
      retry.addEventListener('click', function () { renderVolumeBrowser(); });
      box.appendChild(retry);
    });
  }

  /** 渲染一层:面包屑 + 目录/文件行(目录可点击进入) */
  function renderVolumeBrowserInto(box, sub, list) {
    box.textContent = '';

    // 面包屑:卷名 / a / b(每段可点回退)
    var crumb = mkEl('div', 'vol-crumb');
    var rootBtn = mkEl('button', 'btn btn-sm', name0());
    function name0() { return '根'; }
    rootBtn.type = 'button';
    rootBtn.addEventListener('click', function () { gotoStack(0); });
    crumb.appendChild(rootBtn);
    volBrowse.stack.forEach(function (seg, i) {
      if (i === 0) return; // 根已在上面
      crumb.appendChild(mkEl('span', 'vol-crumb-sep', '/'));
      var btn = mkEl('button', 'btn btn-sm', seg);
      btn.type = 'button';
      btn.addEventListener('click', function () { gotoStack(i); });
      crumb.appendChild(btn);
    });
    box.appendChild(crumb);

    function gotoStack(i) {
      volBrowse.stack = volBrowse.stack.slice(0, i + 1);
      renderVolumeBrowser();
    }

    if (list.length === 0) {
      box.appendChild(mkEl('div', 'server-check-hint', '该目录为空'));
      return;
    }
    var ul = mkEl('div', 'vol-list');
    list.forEach(function (ent) {
      var row = mkEl('div', 'vol-row');
      row.appendChild(window.fillBadge(mkEl('span'), 'info', ent.isDir ? '目录' : '文件'));
      if (ent.isDir) {
        var nameBtn = mkEl('button', 'btn-link', ent.path);
        nameBtn.type = 'button';
        nameBtn.addEventListener('click', function () {
          volBrowse.stack.push(ent.path);
          renderVolumeBrowser();
        });
        row.appendChild(nameBtn);
      } else {
        row.appendChild(mkEl('span', 'vol-file mono', ent.path));
      }
      row.appendChild(mkEl('span', 'vol-size mono nowrap', ent.isDir ? '—' : volFormatBytes(ent.size)));
      ul.appendChild(row);
    });
    box.appendChild(ul);
  }

  /** 备份确认(两步:确认 → 选路径 → 后台跑) */
  function confirmVolumeBackup(volumeName) {
    var div = document.createElement('div');
    div.appendChild(window.confirmBlock({
      title: '把卷「' + volumeName + '」打包下载到本机?',
      facts: [
        ['内容', '整个卷的全部文件(经临时容器 tar 打包,pull 后临时包自动清理)'],
        ['时长', '取决于卷大小;大卷可能数分钟']
      ],
      risk: '备份期间请勿关闭应用;完成后 tar.gz 落在你选择的位置'
    }));
    div.innerHTML +=
      '<div class="modal-actions">' +
      '<button id="vol-backup-cancel" class="btn" type="button">取消</button>' +
      '<button id="vol-backup-ok" class="btn btn-primary" type="button">选择保存位置…</button>' +
      '</div>';
    var cancel = div.querySelector('#vol-backup-cancel');
    if (cancel) cancel.addEventListener('click', closeModal);
    var ok = div.querySelector('#vol-backup-ok');
    if (ok) {
      ok.addEventListener('click', function () {
        // 系统保存对话框(config-io 先例:AppBus.pickPath 只有 open,save 直调插件)
        var safeName = String(volumeName).replace(/[^A-Za-z0-9._-]/g, '_');
        window.__TAURI__.dialog.save({
          title: '保存卷备份',
          defaultPath: safeName + '.tar.gz',
          filters: [{ name: 'tar.gz', extensions: ['tar.gz', 'gz'] }]
        }).then(function (picked) {
          if (!picked) return; // 用户取消
          startVolumeBackup(volumeName, String(picked));
        }).catch(function (err) {
          window.toast('打开保存对话框失败:' + window.errText(err), 'fail');
        });
      });
    }
    openModal('备份卷', div);
  }

  /** 发起备份:一次性订阅事件(先订阅再 invoke,项目纪律),完成/失败收尾 */
  function startVolumeBackup(volumeName, localPath) {
    closeModal();
    window.toast('开始备份卷「' + volumeName + '」…', 'info');
    var unlistenP = AppBus.on('volume-backup-progress', function (ev) {
      var p = (ev && ev.payload) || {};
      if (p.volumeName !== volumeName) return;
      // 进度只在 toast 上给个粗提示(大卷时会频繁触发,不做逐帧 UI)
      state.volBackup = { done: p.done || 0, total: p.total || 0 };
    });
    var unlistenD = AppBus.on('volume-backup-done', function (ev) {
      var p = (ev && ev.payload) || {};
      if (p.volumeName !== volumeName) return;
      window.toast(p.ok ? ('卷「' + volumeName + '」' + (p.message || '备份完成'))
        : ('卷「' + volumeName + '」备份失败:' + (p.message || '未知错误')),
        p.ok ? 'ok' : 'fail');
      // 清理订阅(一次性)
      Promise.all([unlistenP, unlistenD]).then(function (fns) {
        fns.forEach(function (fn) { try { fn(); } catch (e) { /* 忽略 */ } });
      });
    });
    AppBus.invoke('manage_volume_backup', {
      serverId: state.serverId,
      volumeName: volumeName,
      localPath: localPath
    }).catch(function (err) {
      window.toast('发起备份失败:' + (err && err.message ? err.message : String(err)), 'fail');
      Promise.all([unlistenP, unlistenD]).then(function (fns) {
        fns.forEach(function (fn) { try { fn(); } catch (e) { /* 忽略 */ } });
      });
    });
  }

  function confirmRemoveVolume(name) {
    openModal('删除卷', buildConfirmBody(
      '确定删除卷「' + name + '」吗?',
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
      },
      null,
      '该卷正被容器使用时将删除失败。'
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
    if (!name) {
      window.setFieldError(nameInput, '此项必填');
      toast('请输入卷名称', 'warn');
      return;
    }
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

    // 第二十四批三:筛选只影响渲染(收尾 state.networks 仍赋全量)
    var full = list;
    var kwNet = state.filter.networks;
    if (kwNet) {
      list = list.filter(function (n) {
        return ((n.name || '') + ' ' + (n.driver || '')).toLowerCase().indexOf(kwNet) !== -1;
      });
      if (list.length === 0) {
        tbody.innerHTML = '<tr><td class="empty-cell" colspan="5">无匹配的网络</td></tr>';
        state.networks = full;
        return;
      }
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
    state.networks = full;
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
      '确定删除网络「' + (name || id) + '」吗?',
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
      },
      null,
      '有容器连接在该网络上时将删除失败。'
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
    if (!name) {
      window.setFieldError(nameInput, '此项必填');
      toast('请输入网络名称', 'warn');
      return;
    }
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
    if (!container) {
      window.setFieldError(input, '此项必填');
      toast('请输入容器名或容器 ID', 'warn');
      return;
    }

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
      // 查看类弹窗放大标记:openModal 据此给共用 modal-card 加 .modal-wide
      pre.className = 'manage-log-body manage-wide-modal';
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

  /** 显示/隐藏自定义间隔的行内输入 */
  function setCustomIntervalVisible(on) {
    var wrap = $('manage-interval-custom-wrap');
    if (wrap) wrap.classList.toggle('hidden', !on);
    if (on) {
      var input = $('manage-interval-custom');
      if (input && !input.value) input.value = String(state.interval);
    }
  }

  /**
   * 应用自定义间隔(原来走 window.prompt —— 全站唯一的系统对话框,违反本项目
   * 「自绘输入/确认、不调用系统对话框」的纪律;改为下拉旁的行内数字输入)。
   */
  function applyCustomInterval() {
    var ivSel = $('manage-interval-select');
    var input = $('manage-interval-custom');
    if (!ivSel || !input) return;
    var secs = parseInt(String(input.value || '').trim(), 10);
    if (isNaN(secs) || secs < MIN_INTERVAL || secs > MAX_INTERVAL) {
      window.setFieldError(input,
        '需为 ' + MIN_INTERVAL + '-' + MAX_INTERVAL + ' 之间的整数');
      toast('无效间隔,请输入 ' + MIN_INTERVAL + '-' + MAX_INTERVAL + ' 之间的正整数', 'warn');
      return;
    }
    window.setFieldError(input, null);
    state.interval = secs;
    setCustomIntervalVisible(false);
    savePrefs();
    resetTimer();
  }

  function onIntervalChange() {
    var ivSel = $('manage-interval-select');
    if (!ivSel) return;
    var val = ivSel.value;
    if (val === 'custom') {
      // 展开行内输入并把焦点交给它(不再弹系统对话框)
      setCustomIntervalVisible(true);
      var input = $('manage-interval-custom');
      if (input) { try { input.focus(); input.select(); } catch (_) {} }
      return;
    }
    window.setFieldError($('manage-interval-custom'), null);
    setCustomIntervalVisible(false);
    state.interval = parseInt(val, 10) || 30;
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
  // 焦点管理(pass 4 收编到 app.js 共用三件套):打开移焦入卡、关闭归还触发源、
  // Tab 圈禁在卡内(document 级监听)。此前本模块私有的 modalTriggerEl 记忆
  // 已由 window.modalFocusOpen/Close 的栈式记录取代。

  function openModal(title, bodyEl) {
    var modal = $('manage-modal');
    var titleEl = $('manage-modal-title');
    var body = $('manage-modal-body');
    if (!modal || !body) return;
    // 仅在模态从关闭态打开时记录触发元素(模态内重开不覆盖)
    var wasHidden = modal.classList.contains('hidden');
    if (titleEl) titleEl.textContent = title;
    body.innerHTML = '';
    if (bodyEl) body.appendChild(bodyEl);
    // 终端会话专用放大:仅当本次打开的是终端弹窗(body 带 .manage-terminal-modal
    // 标记)时给共用 modal-card 加修饰类;其余弹窗(日志/确认/打标签等)显式移除,
    // 保证共用模态的尺寸互不影响
    // 查看类弹窗放大:body 带 .manage-wide-modal 标记(容器/栈日志、栈服务状态、
    // 卷/网络 inspect)时给 modal-card 加 .modal-wide;其余弹窗显式移除,
    // 与 modal-terminal 同款互斥写法,保证共用模态的尺寸互不影响
    var card = modal.querySelector('.modal-card');
    if (card) {
      var isTerm = !!(bodyEl && bodyEl.classList && bodyEl.classList.contains('manage-terminal-modal'));
      if (isTerm) card.classList.add('modal-terminal');
      else card.classList.remove('modal-terminal');
      var isWide = !!(bodyEl && bodyEl.classList && bodyEl.classList.contains('manage-wide-modal'));
      if (isWide) card.classList.add('modal-wide');
      else card.classList.remove('modal-wide');
    }
    modal.classList.remove('hidden');
    if (wasHidden) window.modalFocusOpen(modal);
  }

  function closeModal() {
    var modal = $('manage-modal');
    if (modal) modal.classList.add('hidden');
    execOnModalClose(); // C 阶段追加:模态框关闭时清理终端会话
    // 焦点归还:归还触发源(栈式记录;触发源已被重渲染移除则静默跳过)
    window.modalFocusClose(modal);
  }

  function buildConfirmBody(message, confirmLabel, onConfirm, showForce, risk) {
    var div = document.createElement('div');
    div.appendChild(window.confirmBlock(risk ? { title: message, risk: risk } : { title: message }));
    div.innerHTML +=
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

  // textContent→innerHTML 只转义 & < >;本模块另有用它拼 value="..." / data-...="..."
  // 双引号属性值,需补 " 与 ' 的转义,否则含引号的值会截断属性(理论注入点)
  function escHtml(s) {
    var div = document.createElement('div');
    div.textContent = String(s == null ? '' : s);
    return div.innerHTML.replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }

  /* ============================================================
   * C 阶段追加:Compose 栈 / 实时监控 / 容器 Exec 终端
   * - 栈列表:启动 / 停止(二次确认) / 服务状态 / 日志,按 compose_file 行差异更新
   * - 监控:manage_stats_start/stop + manage-stats 事件整表刷新,CPU% 阈值着色
   * - 终端:manage_exec_start/write/stop + manage-exec-output 事件,简易 ANSI 处理
   * 生命周期:切走监控 Tab / 离开 05 页自动停止监控与终端会话,unlisten 防泄漏
   * ============================================================ */

  // ===== 第十二批 JS 拆分桥接:共享设施暴露给拆出文件,入口转发 =====
  window.ManageKit = { state: state, buildConfirmBody: buildConfirmBody, closeModal: closeModal, openModal: openModal, escHtml: escHtml, showError: showError, refreshOverview: refreshOverview, renderContainers: renderContainers, consumePendingTabScroll: consumePendingTabScroll, stopTimer: stopTimer, startTimerIfEnabled: startTimerIfEnabled, withStageScrollGuard: withStageScrollGuard };
  var buildFollowToggle = function () { return window.ManageStacks.buildFollowToggle.apply(null, arguments); };
  var execOnModalClose = function () { return window.ManageStacks.execOnModalClose.apply(null, arguments); };
  var monitorStop = function () { return window.ManageStacks.monitorStop.apply(null, arguments); };
  var onLeaveC = function () { return window.ManageStacks.onLeaveC.apply(null, arguments); };
  var openTerminal = function () { return window.ManageStacks.openTerminal.apply(null, arguments); };
  var refreshStacks = function () { return window.ManageStacks.refreshStacks.apply(null, arguments); };
  var stopExecSession = function () { return window.ManageStacks.stopExecSession.apply(null, arguments); };
})();
