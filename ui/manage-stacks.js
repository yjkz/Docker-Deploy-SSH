// manage-stacks.js — C 阶段独立状态域(Compose 栈/.env/实时监控/Exec 终端/日志跟随/离页清理)
// 第十二批 JS 结构治理:自 manage.js 拆出;外部依赖经
// window.ManageKit 桥接(定义见宿主文件尾部),入口经 window.ManageStacks 供宿主回调。
(function () {
  'use strict';
  var K = window.ManageKit;
  // `$` 是 manage.js 的 IIFE 局部助手,拆分时不随迁(它不在 ManageKit 桥上);
  // 本文件 40+ 处裸 `$(...)` 依赖此定义,缺失则 bindEventsC 一执行就抛
  // ReferenceError,监控/栈/终端/日志跟随的按钮监听全部注册不上(v5.14.0 真实回归)
  var $ = function (id) { return document.getElementById(id); };
  var state = K.state;
  var buildConfirmBody = K.buildConfirmBody;
  var closeModal = K.closeModal;
  var openModal = K.openModal;
  var escHtml = K.escHtml;
  var showError = K.showError;
  var refreshOverview = K.refreshOverview;
  var renderContainers = K.renderContainers;
  var consumePendingTabScroll = K.consumePendingTabScroll;
  var stopTimer = K.stopTimer;
  var startTimerIfEnabled = K.startTimerIfEnabled;
  var withStageScrollGuard = K.withStageScrollGuard;

  // ===== C 阶段独立状态(不触碰上方 state 对象) =====
  var cState = {
    stacks: [],
    mon: { running: false, unlisten: null, errShown: false },
    // Exec 终端(第二十二批:多标签;字段含义见「容器 Exec 终端」节头注释)
    exec: {
      tabs: {},        // key → Tab
      order: [],       // 标签顺序(key 数组,左侧先开)
      activeKey: null, // 当前标签 key
      unlisten: null,  // 全局单监听(manage-exec-output;懒注册,模态关闭时释放)
      listening: false,// 同步双注册守卫(注册 promise 未 resolve 期间为 true)
      listenGen: 0,    // 监听代际(release 时自增;过期 promise 回调据此自注销)
      buffer: []       // invoke 未返回期间的早期事件(session 建立后按 sid 认领)
    }
  };

  document.addEventListener('DOMContentLoaded', bindEventsC);

  function bindEventsC() {
    var sr = $('manage-stack-refresh-btn');
    if (sr) sr.addEventListener('click', function () { refreshStacks(); });
    // 「显示归档副本」勾选切换即自动刷新列表(默认不勾 = 排除归档副本)
    var archivedToggle = $('manage-stacks-archived-toggle');
    if (archivedToggle) {
      archivedToggle.addEventListener('change', function () { refreshStacks(); });
    }

    var ms = $('monitor-start-btn');
    if (ms) ms.addEventListener('click', monitorStart);
    var mstop = $('monitor-stop-btn');
    if (mstop) mstop.addEventListener('click', function () { monitorStop(false); });

    // 阶段五:终端尺寸自适应 —— window resize 防抖同步(全程仅注册一次;
    // 回调内部对会话判空,无终端会话时直接返回,不随模态反复注册)
    window.addEventListener('resize', scheduleTermResize);
  }

  // ===== Compose 栈列表 =====
  /** 「显示归档副本」勾选态(默认不勾 = 排除 releases/ 归档内的 compose 副本) */
  function stacksArchivedFlag() {
    var t = $('manage-stacks-archived-toggle');
    return !!(t && t.checked);
  }

  function refreshStacks() {
    if (!state.serverId || state.inFlight) return;
    state.inFlight = true;
    AppBus.invoke('manage_list_stacks', {
      serverId: state.serverId,
      includeArchived: stacksArchivedFlag()
    }).then(function (list) {
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
    // 操作:启动 / 停止 / 服务状态 / 日志 / .env
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

    // 阶段五:.env 查看/编辑(compose 文件同目录的环境变量文件)
    var envBtn = document.createElement('button');
    envBtn.type = 'button';
    envBtn.className = 'btn btn-sm';
    envBtn.textContent = '.env';
    envBtn.addEventListener('click', function () { showStackEnv(st); });
    wrap.appendChild(envBtn);

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
      },
      null,
      label === '停止' ? '停止后该栈的全部容器将退出。' : '将按 compose 文件拉起该栈的全部服务。'
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
    // 查看类弹窗放大标记:openModal 据此给共用 modal-card 加 .modal-wide
    body.className = 'manage-wide-modal';
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
    // 查看类弹窗放大标记:openModal 据此给共用 modal-card 加 .modal-wide
    body.className = 'manage-wide-modal';
    var bar2 = document.createElement('div');
    bar2.className = 'log-tail-bar';
    bar2.innerHTML =
      '<label>显示行数:' +
      '<select id="stack-log-tail-select" class="form-input form-input-sm">' +
      '<option value="100">100</option>' +
      '<option value="500">500</option>' +
      '<option value="1000">1000</option>' +
      '<option value="0">全部</option>' +
      '</select></label>';
    // 阶段九:实时跟随开关(栈:compose logs -f)
    bar2.appendChild(buildFollowToggle('stack', st.compose_file, 'stack-log-tail-select', 'stack-log-content'));
    var pre2 = document.createElement('pre');
    pre2.id = 'stack-log-content';
    pre2.className = 'manage-log-body';
    pre2.textContent = '加载中…';
    body.appendChild(bar2);
    body.appendChild(pre2);

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

  // ===== 阶段五:栈 .env 查看 / 编辑 =====
  // 读:manage_stack_env_read(serverId, composeFile) → { exists, content }
  // 写:manage_stack_env_save(serverId, composeFile, content) → { success, message }
  // 交互:默认只读,「编辑」切换可写并显示「保存 .env / 取消编辑」;保存前经
  // 自定义确认弹窗二次确认(确认弹窗会替换编辑器主体,「取消」需重建编辑体
  // 回填草稿,见 buildStackEnvSaveConfirm,不能复用 buildConfirmBody 的默认
  // 取消=closeModal),成功后重开只读态并回读;256KB 上限前端先拦
  // (与后端 STACK_ENV_MAX_BYTES 同口径)。busy 防重复提交;会话序号(每次打开
  // +1)丢弃模态重开前的旧异步回写(参照 notify.js 先例)。Esc/遮罩关闭由
  // #manage-modal 的既有处理器承担。
  // rawB64/notUtf8(第 N 批):非 UTF-8 文件的原始字节 base64 与标志 ——
  // 未改动保存时原样回传后端,原始字节无损落盘(消除 U+FFFD 回写乱码)
  var envState = { session: 0, busy: false, loaded: '', known: false, rawB64: '', notUtf8: false };
  // 非 UTF-8 字节告警文案:读回内容含 U+FFFD(后端 from_utf8_lossy 替换所致)时
  // 在提示区展示;未改动保存走原样回写无损落盘,改动后保存会被拒绝(见 doStackEnvSave)
  var ENV_FFFD_WARN = '文件包含非 UTF-8 字节(可能为 GBK 编码),显示为替换符;未改动保存将按原始字节无损回写,改动后需在服务器上以正确编码编辑';

  function showStackEnv(st) {
    var session = ++envState.session; // 开启新会话:此前打开的旧 promise 收尾失效
    envState.busy = false;
    openModal('.env — ' + (st.dir || st.compose_file), buildStackEnvBody(st, session));
    loadStackEnv(st, session);
  }

  // 构建编辑器主体:顶栏(提示 + 操作按钮)+ 等宽 textarea(样式见 .manage-env-editor)
  function buildStackEnvBody(st, session) {
    var body = document.createElement('div');
    // 查看类弹窗放大标记:openModal 据此给共用 modal-card 加 .modal-wide(同 showStackLogs)
    body.className = 'manage-wide-modal';
    body.innerHTML =
      '<div class="log-tail-bar manage-env-bar">' +
      '<span id="stack-env-hint" class="manage-env-hint hidden"></span>' +
      '<div class="manage-env-actions">' +
      '<button id="stack-env-edit-btn" class="btn btn-sm" type="button">编辑</button>' +
      '<button id="stack-env-save-btn" class="btn btn-sm btn-danger hidden" type="button">保存 .env</button>' +
      '<button id="stack-env-cancel-btn" class="btn btn-sm hidden" type="button">取消编辑</button>' +
      '</div>' +
      '</div>' +
      '<textarea id="stack-env-editor" class="manage-env-editor" spellcheck="false" readonly></textarea>';

    var editBtn = body.querySelector('#stack-env-edit-btn');
    if (editBtn) editBtn.addEventListener('click', function () { setStackEnvMode(true); });

    var saveBtn = body.querySelector('#stack-env-save-btn');
    if (saveBtn) saveBtn.addEventListener('click', function () { onStackEnvSave(st, session); });

    var cancelBtn = body.querySelector('#stack-env-cancel-btn');
    if (cancelBtn) {
      cancelBtn.addEventListener('click', function () {
        var ta = $('stack-env-editor');
        if (!ta) return;
        setStackEnvMode(false);
        if (envState.known) ta.value = envState.loaded; // 回退到最近读到的服务器内容
        else loadStackEnv(st, session); // 保存失败重开场景:服务器内容未知,回读
      });
    }
    return body;
  }

  // 读取服务器 .env 并回填(只读态);exists=false 给「将创建」提示而非报错
  function loadStackEnv(st, session) {
    envState.loaded = '';
    envState.known = false;
    var ta = $('stack-env-editor');
    if (ta) { ta.value = ''; ta.placeholder = '加载中…'; }
    AppBus.invoke('manage_stack_env_read', { serverId: state.serverId, composeFile: st.compose_file })
      .then(function (res) {
        if (session !== envState.session) return; // 模态已重开:旧回写丢弃
        var ta2 = $('stack-env-editor');
        if (!ta2) return;
        var exists = !!(res && res.exists);
        envState.loaded = exists ? String(res.content || '') : '';
        envState.known = true;
        // 第 N 批:非 UTF-8 文件记录原始字节与标志(未改动保存 → 无损回写)
        envState.notUtf8 = !!(res && res.notUtf8);
        envState.rawB64 = envState.notUtf8 ? String(res.rawB64 || '') : '';
        if (ta2.readOnly) ta2.value = envState.loaded; // 用户已进编辑态则不打断草稿
        ta2.placeholder = '';
        // 非 UTF-8:提示区警告(未改动保存无损回写,改动后拒绝);UTF-8 但
        // 内容恰好含 U+FFFD 字面量的边缘情况也走同一条无损回写路径,安全
        if (exists && envState.notUtf8) {
          setStackEnvHint(ENV_FFFD_WARN);
        } else {
          setStackEnvHint(exists ? '' : '该栈目录暂无 .env 文件,保存后将创建');
        }
      })
      .catch(function (err) {
        if (session !== envState.session) return;
        var ta2 = $('stack-env-editor');
        if (ta2 && ta2.readOnly) { ta2.value = ''; ta2.placeholder = ''; }
        var msg = err && err.message ? err.message : String(err);
        setStackEnvHint('读取 .env 失败: ' + msg);
      });
  }

  // 只读 ↔ 可写切换:可写态显示「保存 .env」「取消编辑」,隐藏「编辑」
  function setStackEnvMode(editing) {
    var ta = $('stack-env-editor');
    var editBtn = $('stack-env-edit-btn');
    var saveBtn = $('stack-env-save-btn');
    var cancelBtn = $('stack-env-cancel-btn');
    if (!ta || !editBtn || !saveBtn || !cancelBtn) return;
    ta.readOnly = !editing;
    editBtn.classList.toggle('hidden', editing);
    saveBtn.classList.toggle('hidden', !editing);
    cancelBtn.classList.toggle('hidden', !editing);
    if (editing) ta.focus();
  }

  function setStackEnvHint(text) {
    var hint = $('stack-env-hint');
    if (!hint) return;
    if (text) {
      hint.textContent = text;
      hint.classList.remove('hidden');
    } else {
      hint.classList.add('hidden');
    }
  }

  // 保存入口:busy 防重 + 256KB 前端先拦(按 UTF-8 字节,与后端校验同口径)+
  // 自定义确认弹窗二次确认(布局同 buildConfirmBody,「取消」行为不同,见下)
  function onStackEnvSave(st, session) {
    if (envState.busy) return;
    var ta = $('stack-env-editor');
    if (!ta) return;
    var draft = ta.value;
    var bytes = window.TextEncoder ? new TextEncoder().encode(draft).length : draft.length;
    if (bytes > 256 * 1024) {
      toast('.env 内容过大(上限 256KB),请精简后再保存', 'warn');
      return;
    }
    // 第 N 批(非 UTF-8 无损往返):非 UTF-8 文件的两种保存走向 ——
    // ① 未改动(草稿 === 读取时的 lossy 展示):原样回写原始字节,无损落盘;
    // ② 改过:拒绝保存(替换符落盘会永久损坏原始字节),提示上服务器改
    if (envState.notUtf8) {
      if (draft === envState.loaded) {
        doStackEnvSave(st, session, draft, envState.rawB64);
        return;
      }
      toast('内容已修改但原文件含非 UTF-8 字节,保存会把替换字符写入文件;请在服务器上以正确编码编辑,或取消修改后原样保存', 'fail');
      return;
    }
    // UTF-8 文件:内容恰好含 U+FFFD 字面量时仍走正常保存(用户显式输入的字符)
    openModal('保存 .env', buildStackEnvSaveConfirm(st, session, draft, null));
  }

  // 保存确认弹窗主体:布局同 buildConfirmBody,但「取消」不能走默认的 closeModal
  // ——确认弹窗替换编辑器主体后 textarea 已销毁,直接关整个模态会丢草稿且无路径
  // 返回;这里「取消」改为 reopenStackEnvEdit 重建可写编辑体并回填草稿(带会话
  // 校验),确认按钮才 closeModal + 执行保存
  function buildStackEnvSaveConfirm(st, session, draft, risk) {
    var div = document.createElement('div');
    div.appendChild(window.confirmBlock({
      title: '保存将覆盖服务器上的 .env,影响下次 compose up,确定?',
      risk: risk
    }));
    div.innerHTML +=
      '<div class="modal-actions">' +
      '<button id="confirm-cancel-btn" class="btn" type="button">取消</button>' +
      '<button id="confirm-ok-btn" class="btn btn-danger" type="button">保存</button>' +
      '</div>';
    var cancelBtn = div.querySelector('#confirm-cancel-btn');
    if (cancelBtn) {
      cancelBtn.addEventListener('click', function () {
        // 草稿含 U+FFFD 时提示区一并保留风险说明
        var hint = '已取消保存,编辑内容已保留;可重试保存或取消编辑';
        if (draft.indexOf('\uFFFD') !== -1) hint = ENV_FFFD_WARN + '; ' + hint;
        reopenStackEnvEdit(st, draft, session, hint);
      });
    }
    var okBtn = div.querySelector('#confirm-ok-btn');
    if (okBtn) okBtn.addEventListener('click', function () { doStackEnvSave(st, session, draft); });
    return div;
  }

  function doStackEnvSave(st, session, draft, rawB64) {
    envState.busy = true;
    closeModal(); // 确认弹窗关闭,保存结果决定后续走向
    // rawB64 非空 = 非 UTF-8 文件未改动:原样回写原始字节(无损);
    // 空 = 正常 UTF-8 保存(或后端拿不到 raw 时的兜底路径)
    var payload = {
      serverId: state.serverId,
      composeFile: st.compose_file,
      content: draft
    };
    if (rawB64) payload.rawB64 = rawB64;
    AppBus.invoke('manage_stack_env_save', payload).then(function (res) {
      envState.busy = false;
      if (session !== envState.session) return; // 期间模态已重开:丢弃旧回写
      if (res && res.success) {
        toast('已保存 .env,下次 compose up 生效', 'ok');
        showStackEnv(st); // 重开只读态并回读服务器内容(顺带校验落盘结果)
      } else {
        toast('保存 .env 失败: ' + ((res && res.message) || '未知错误'), 'fail');
        reopenStackEnvEdit(st, draft, session);
      }
    }).catch(function (err) {
      envState.busy = false;
      if (session !== envState.session) return;
      var msg = err && err.message ? err.message : String(err);
      toast('保存 .env 失败: ' + msg, 'fail');
      reopenStackEnvEdit(st, draft, session);
    });
  }

  // 重开可写编辑体并恢复草稿(hint 可选:保存失败/取消保存等场景定制提示;
  // 此态下「取消编辑」因服务器内容未知会回读)
  function reopenStackEnvEdit(st, draft, session, hint) {
    if (session !== envState.session) return;
    openModal('.env — ' + (st.dir || st.compose_file), buildStackEnvBody(st, session));
    var ta = $('stack-env-editor');
    if (ta) ta.value = draft;
    setStackEnvMode(true);
    setStackEnvHint(hint || '保存失败,已保留你的修改;可重试保存或取消编辑');
  }

  // ===== 实时监控 =====
  function monitorStart() {
    if (!state.serverId) { toast('请先选择服务器', 'warn'); return; }
    if (cState.mon.running) { toast('监控已在运行中', 'info'); return; }
    var ivSel = $('monitor-interval-select');
    var intervalSecs = ivSel ? (parseInt(ivSel.value, 10) || 2) : 2;
    hideMonitorError();
    cState.mon.errShown = false;

    // 先订阅后 invoke(项目纪律,防首帧丢失);订阅失败要在 catch 里停掉后端
    // 采样循环,否则后端仍跑而前端显示「已停止」无人调 manage_stats_stop
    AppBus.on('manage-stats', onStatsEvent).then(function (unlisten) {
      cState.mon.unlisten = unlisten;
      return AppBus.invoke('manage_stats_start', {
        serverId: state.serverId,
        intervalSecs: intervalSecs
      }).then(function () {
        cState.mon.running = true;
        updateMonitorUi();
      });
    }).catch(function (err) {
      // 任一环节失败:清订阅 + 停后端采样(后端可能已成功启动)
      if (cState.mon.unlisten) {
        try { cState.mon.unlisten(); } catch (e) { /* 忽略 */ }
        cState.mon.unlisten = null;
      }
      AppBus.invoke('manage_stats_stop', {}).catch(function () { /* 后端未启动时忽略 */ });
      cState.mon.running = false;
      updateMonitorUi();
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
    // 聚合条随停止隐藏(表内历史数据保留,与旧口径一致)
    var agg = $('monitor-aggregate');
    if (agg) agg.classList.add('hidden');
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
      // (第十六批:error 可带码标记,展示一律剥掉;分类判定看 payload.errorCode)
      var errText = window.errStripCode(String(payload.error));
      showMonitorError(errText, !!payload.stopped);
      if (!cState.mon.errShown) {
        cState.mon.errShown = true;
        toast('监控数据异常: ' + errText, 'warn');
      }
      if (payload.stopped) monitorStop(true);
      return;
    }
    hideMonitorError();
    cState.mon.errShown = false;
    renderStats(payload.stats || []);
    renderMonitorAggregate(payload.aggregate);
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

  /**
   * 容器级聚合条(第十七批):「N 个容器 · CPU 前列:名字 xx% / … · 内存前列:名字 x% (占用) / …」。
   * payload.aggregate 由后端每轮计算(count + Top3 CPU + Top3 内存);
   * 旧版后端无此字段(版本错配)→ 隐藏聚合条,监控表不受影响。
   */
  function renderMonitorAggregate(agg) {
    var el = $('monitor-aggregate');
    if (!el) return;
    if (!agg || !agg.count) {
      el.classList.add('hidden');
      return;
    }
    var fmtTop = function (list, unit) {
      return list.map(function (t) {
        var v = String(t.cpu_percent || '—');
        return String(t.name || '—') + ' ' + v + (v === '—' ? '' : unit);
      }).join(' / ');
    };
    var memTop = (agg.top_mem || []).map(function (t) {
      return String(t.name || '—') + ' ' + String(t.mem_percent || '—') +
        (t.mem_usage ? '(' + String(t.mem_usage).split(' / ')[0] + ')' : '');
    }).join(' / ');
    var parts = [];
    parts.push(agg.count + ' 个容器');
    if (agg.top_cpu && agg.top_cpu.length) parts.push('CPU 前列:' + fmtTop(agg.top_cpu, ''));
    if (memTop) parts.push('内存前列:' + memTop);
    el.textContent = parts.join(' · ');
    el.classList.remove('hidden');
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

  /** CPU 档位:<50 normal,50-80 warm,>80 hot;非数值返回 null */
  function cpuTierOf(val) {
    var num = parseFloat(val);
    if (isNaN(num)) return null;
    return num > 80 ? 'hot' : (num >= 50 ? 'warm' : 'normal');
  }

  function updateStatRow(tr, s) {
    // pass 4(P4-E):档位迁移检测须在重建行之前(旧档位挂在行属性上)。
    // 先摘除 stat-flash 再按需「reflow + 复加」:类若常驻,重建的 td 每次刷新
    // 都会重新命中动画选择器,变成每个刷新周期闪一次。
    var tier = cpuTierOf(s.cpu_percent);
    var prevTier = tr.getAttribute('data-stat-tier');
    tr.classList.remove('stat-flash');
    if (tier !== null) {
      if (prevTier !== null && prevTier !== tier) {
        void tr.offsetWidth;
        tr.classList.add('stat-flash');
      }
      tr.setAttribute('data-stat-tier', tier);
    }
    tr.innerHTML = '';
    tr.appendChild(mkStatTd(s.name || s.container_id || '—', true));
    tr.appendChild(mkCpuTd(s.cpu_percent, tier));
    tr.appendChild(mkStatTd(s.mem_usage || '—', true));
    tr.appendChild(mkStatTd(s.mem_percent != null ? String(s.mem_percent) : '—', true));
    tr.appendChild(mkStatTd(s.net_io || '—', true));
    tr.appendChild(mkStatTd(s.block_io || '—', true));
    tr.appendChild(mkStatTd(s.pids != null ? String(s.pids) : '—', true));
  }

  function mkStatTd(text, mono) {
    var td = document.createElement('td');
    if (mono) td.className = 'mono';
    td.textContent = text;
    return td;
  }

  function mkCpuTd(val, tier) {
    var td = document.createElement('td');
    td.className = 'mono';
    var num = parseFloat(val);
    if (!isNaN(num)) {
      td.textContent = String(val);
      // CPU% 阈值着色:<50 正常,50-80 预警,>80 过热;阈值语义以 title 承载
      // (a11y color-not-only:档位不依赖纯颜色,悬停/辅助技术可读)
      if (tier === 'hot') { td.classList.add('stat-hot'); td.title = 'CPU 高于 80%'; }
      else if (tier === 'warm') { td.classList.add('stat-warm'); td.title = 'CPU 50%–80%'; }
    } else {
      td.textContent = val || '—';
    }
    return td;
  }

  // ===== 容器 Exec 终端(第二十二批:多标签 + 同栈广播)=====
  // 状态模型:cState.exec = { tabs:{key:Tab}, order:[key], activeKey, unlisten, buffer }
  // Tab = { key, sessionId, containerId, name, composeProject, shell,
  //         lines, cur, curIdx, eof, pend, history, histIdx, lastCols, lastRows }
  // - 单个 manage-exec-output 监听,按 payload.session_id 路由到对应 tab;
  //   session_id 未知的早期事件入 buffer,invoke 返回后按 sid 认领(防早到丢失)
  // - 模态骨架常驻:已开着终端时再点「终端」= 加标签,不重建模态;
  //   但模态是全屏遮罩会盖住表格,表格按钮在模态打开时不可达——故顶栏
  //   另有「新标签」下拉(运行中容器,选中即开/切),作为加标签的可达闭环
  //   (第二十二批修复:v6.4.0 只有表格入口,实际第二个标签开不出来)
  // - 广播:勾选后输入发给**同 compose 栈**(同 compose_project)的活跃标签;
  //   无同栈同伴/项目名为空时禁用

  function execTabsAll() {
    var ex = cState.exec;
    return ex.order.map(function (k) { return ex.tabs[k]; }).filter(Boolean);
  }

  function execTabByKey(key) { return cState.exec.tabs[key] || null; }

  function execActiveTab() {
    var ex = cState.exec;
    return ex.activeKey ? ex.tabs[ex.activeKey] || null : null;
  }

  function execTabBySession(sid) {
    var list = execTabsAll();
    for (var i = 0; i < list.length; i++) if (list[i].sessionId === sid) return list[i];
    return null;
  }

  function isTerminalOpen() {
    var modal = $('manage-modal');
    return !!(modal && !modal.classList.contains('hidden') && $('term-output'));
  }

  function newExecTab(containerId, name, composeProject) {
    return {
      key: 'tab-' + Date.now() + '-' + Math.random().toString(16).slice(2, 8),
      sessionId: null,
      containerId: containerId,
      name: name || containerId,
      composeProject: composeProject || null,
      shell: '',
      lines: [], cur: '', curIdx: 0, eof: false, pend: '',
      history: [], histIdx: -1,
      lastCols: null, lastRows: null
    };
  }

  function openTerminal(containerId, name, composeProject) {
    // 同容器已有存活标签 → 直接切过去(防重复会话)
    var existing = execTabsAll().filter(function (t) {
      return t.containerId === containerId && !t.eof;
    })[0];
    if (existing && isTerminalOpen()) {
      selectExecTab(existing.key);
      return;
    }

    var tab = newExecTab(containerId, name, composeProject);
    var ex = cState.exec;
    ex.tabs[tab.key] = tab;
    ex.order.push(tab.key);

    if (!isTerminalOpen()) {
      buildTerminalModal();
    }
    ex.activeKey = tab.key;
    renderExecTabs();
    selectExecTab(tab.key);

    var shellSel = $('term-shell-select');
    startExec(tab, shellSel ? shellSel.value : '');
  }

  // 构建终端模态骨架(仅首次打开时;之后加标签只更新 DOM 片段)
  function buildTerminalModal() {
    var body = document.createElement('div');
    body.className = 'manage-terminal-modal';
    body.innerHTML =
      '<div class="log-tail-bar term-toolbar">' +
      '<div id="term-tabs" class="term-tabs" role="tablist"></div>' +
      '<label class="term-newtab" id="term-newtab-label" title="选择运行中的容器,打开/切换到其终端标签">＋新标签' +
      '<select id="term-newtab-select" class="form-input form-input-sm"></select></label>' +
      '<label class="term-broadcast" id="term-broadcast-label" title="需同时打开同一 compose 栈的多个容器终端">' +
      '<input type="checkbox" id="term-broadcast-cb">广播同栈</label>' +
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

    openModal('终端', body);

    // 新标签下拉:选项来自表格数据(state.containers);打开/展开时重填,
    // 保证与表格最新一致(容器可能被启动/删除)
    newTabSelSig = ''; // 骨架是全新 DOM,签名复位强制首填
    populateNewTabSelect();
    var newTabSel = $('term-newtab-select');
    if (newTabSel) {
      newTabSel.addEventListener('focus', populateNewTabSelect);
      newTabSel.addEventListener('mousedown', populateNewTabSelect);
      newTabSel.addEventListener('change', function () {
        var cid = newTabSel.value;
        newTabSel.value = '';
        if (!cid) return;
        var c = state.containers.filter(function (x) { return x.id === cid; })[0];
        if (!c) return;
        // 与表格「终端」按钮同口径:同容器已开标签时 openTerminal 内部去重切换
        openTerminal(c.id, c.names || c.id, c.compose_project || null);
      });
    }

    var shellSel = $('term-shell-select');
    if (shellSel) shellSel.addEventListener('change', function () {
      // 切换 shell:重启**当前标签**的会话(其余标签不受影响)
      var tab = execActiveTab();
      if (!tab) return;
      stopExecTab(tab, true);
      resetTermBuffer(tab);
      if (execActiveTab() === tab) {
        var out = $('term-output');
        if (out) out.textContent = '正在连接…';
      }
      startExec(tab, shellSel.value);
    });

    var broadcastCb = $('term-broadcast-cb');
    if (broadcastCb) broadcastCb.addEventListener('change', updateBroadcastState);

    var closeBtn = $('term-close-btn');
    if (closeBtn) closeBtn.addEventListener('click', function () {
      stopAllExecTabs();
      closeModal();
    });

    var input = $('term-input');
    if (input) {
      input.addEventListener('keydown', onTermInputKey);
      input.focus();
    }

    ensureExecListener();
  }

  // 重填「新标签」下拉:仅运行中容器(state.containers 为表格数据,
  // 与容器渲染同源);名称@compose 项目便于同栈广播时辨认。
  // 数据为空给禁用占位;createElement/textContent 防 XSS。
  // 签名守卫:数据未变时不重建 DOM(避免在 mousedown/弹出原生下拉期间
  // 做无谓的 option 替换)。
  var newTabSelSig = '';
  function populateNewTabSelect() {
    var sel = $('term-newtab-select');
    if (!sel) return;
    var list = (state.containers || []).filter(function (c) {
      return (c.state || '').toLowerCase() === 'running';
    });
    // 签名含「是否已开标签」(标注后缀依赖它),标签开/关时也需重建
    var sig = list.map(function (c) {
      var opened = execTabsAll().some(function (t) {
        return t.containerId === c.id && !t.eof;
      });
      return c.id + '|' + (c.names || '') + '|' + (c.compose_project || '') + '|' + (opened ? '1' : '0');
    }).join(';');
    if (sig === newTabSelSig) return; // 数据未变不重建
    newTabSelSig = sig;
    sel.innerHTML = '';
    if (list.length === 0) {
      var opt0 = document.createElement('option');
      opt0.value = '';
      opt0.textContent = '无运行中容器';
      sel.appendChild(opt0);
      sel.disabled = true;
      return;
    }
    sel.disabled = false;
    var placeholder = document.createElement('option');
    placeholder.value = '';
    placeholder.textContent = '选择容器…';
    sel.appendChild(placeholder);
    for (var i = 0; i < list.length; i++) {
      var c = list[i];
      var opt = document.createElement('option');
      opt.value = c.id;
      opt.textContent = (c.names || c.id) + (c.compose_project ? ' @ ' + c.compose_project : '');
      // 已开标签的容器加后缀标注(选中 = 切过去,不是重复开会话)
      var opened = execTabsAll().some(function (t) {
        return t.containerId === c.id && !t.eof;
      });
      if (opened) opt.textContent += '(已打开)';
      sel.appendChild(opt);
    }
  }

  // 全局单监听(懒注册一次):按 session_id 路由;未知 sid 入 buffer
  // 双注册守卫必须用**同步**标志:AppBus.on 的 unlisten 要等 promise resolve
  // 才赋值,而 buildTerminalModal 与 startExec 在同一同步任务内都会调用本函数
  // ——只判 ex.unlisten 会注册两次、每条 payload 路由两遍(judge 实测发现)。
  // 代际守卫:release 后注册 promise 才 resolve 时(模态已关),立即注销新
  // 订阅,防「释放后回挂」泄漏。
  function ensureExecListener() {
    var ex = cState.exec;
    if (ex.unlisten || ex.listening) return;
    ex.listening = true;
    var gen = ex.listenGen;
    AppBus.on('manage-exec-output', function (event) {
      var payload = event ? event.payload : null;
      if (!payload) return;
      var sid = payload.session_id;
      var tab = execTabBySession(sid);
      if (tab) {
        routeExecPayload(tab, payload);
      } else {
        // 新会话早期事件(invoke 未返回):缓冲,session 建立后认领
        ex.buffer.push(payload);
      }
    }).then(function (unlisten) {
      if (ex.listenGen !== gen) {
        // 期间已 release:直接注销本次注册
        try { unlisten(); } catch (e) { /* 忽略 */ }
        return;
      }
      ex.unlisten = unlisten;
      ex.listening = false;
    }).catch(function (err) {
      ex.listening = false;
      console.warn('[manage] manage-exec-output 监听注册失败:', err);
    });
  }

  // invoke 返回新 session 后:认领 buffer 中属于它的早期事件
  function claimBufferedEvents(tab) {
    var ex = cState.exec;
    if (!tab.sessionId || ex.buffer.length === 0) return;
    var rest = [];
    for (var i = 0; i < ex.buffer.length; i++) {
      var p = ex.buffer[i];
      if (p && p.session_id === tab.sessionId) routeExecPayload(tab, p);
      else rest.push(p);
    }
    ex.buffer = rest;
  }

  function routeExecPayload(tab, payload) {
    if (payload.data) termWrite(tab, String(payload.data));
    if (payload.eof) {
      if (payload.error) {
        var reason = window.errStripCode(String(payload.error));
        termAppendLine(tab, '[会话已结束: ' + reason + ']');
        if (execActiveTab() === tab) toast('终端会话结束: ' + reason, 'warn');
      } else {
        termAppendLine(tab, '[会话已结束]');
      }
      tab.eof = true;
      tab.sessionId = null;
      if (execActiveTab() === tab) renderTermEditor();
      renderExecTabs();
    }
  }

  // ===== 标签栏渲染与切换 =====

  function renderExecTabs() {
    var bar = $('term-tabs');
    if (!bar) return;
    bar.innerHTML = '';
    execTabsAll().forEach(function (t) {
      var item = document.createElement('span');
      item.className = 'term-tab' + (t.key === cState.exec.activeKey ? ' active' : '') + (t.eof ? ' ended' : '');
      var label = document.createElement('button');
      label.type = 'button';
      label.className = 'term-tab-label';
      label.textContent = t.name + (t.eof ? '(已结束)' : '');
      label.title = t.name + (t.composeProject ? ' @ ' + t.composeProject : '');
      label.addEventListener('click', function () { selectExecTab(t.key); });
      item.appendChild(label);
      var x = document.createElement('button');
      x.type = 'button';
      x.className = 'term-tab-close';
      x.textContent = '×';
      x.title = '关闭该标签';
      x.addEventListener('click', function (e) {
        e.stopPropagation();
        closeExecTab(t.key);
      });
      item.appendChild(x);
      bar.appendChild(item);
    });
    updateBroadcastState();
    // 标签开/关会改变「(已打开)」标注,同步刷新新标签下拉(签名守卫去重)
    populateNewTabSelect();
  }

  function selectExecTab(key) {
    var tab = execTabByKey(key);
    if (!tab) return;
    cState.exec.activeKey = key;
    renderExecTabs();
    // Shell 下拉回显该标签会话
    var shellSel = $('term-shell-select');
    if (shellSel && tab.shell) shellSel.value = tab.shell;
    renderTermEditor();
    // 切标签后按当前输出区尺寸补推一次(各标签尺寸缓存独立)
    pushTermResize();
    var input = $('term-input');
    if (input && !tab.eof) input.focus();
    updateBroadcastState();
  }

  function closeExecTab(key) {
    var tab = execTabByKey(key);
    if (!tab) return;
    stopExecTab(tab, true);
    delete cState.exec.tabs[key];
    var idx = cState.exec.order.indexOf(key);
    if (idx >= 0) cState.exec.order.splice(idx, 1);
    if (cState.exec.activeKey === key) {
      cState.exec.activeKey = cState.exec.order.length
        ? cState.exec.order[Math.max(0, idx - 1)] || cState.exec.order[0]
        : null;
    }
    if (cState.exec.order.length === 0) {
      // 最后一个标签:关模态(走统一清理)
      closeModal();
      return;
    }
    renderExecTabs();
    if (cState.exec.activeKey) selectExecTab(cState.exec.activeKey);
  }

  // 渲染输出区为**当前标签**的内容;无标签时留空
  function renderTermEditor() {
    var tab = execActiveTab();
    var input = $('term-input');
    if (!tab) return;
    renderTerm(tab);
    if (input) {
      input.disabled = !!tab.eof;
      input.placeholder = tab.eof
        ? '会话已结束(关闭标签或切换 shell 重开)'
        : '输入命令,Enter 发送;↑/↓ 切换历史';
    }
  }

  // ===== 广播(同 compose 栈)=====

  function broadcastPeers() {
    var at = execActiveTab();
    if (!at || !at.sessionId || at.eof || !at.composeProject) return [];
    return execTabsAll().filter(function (t) {
      return t !== at && t.sessionId && !t.eof && t.composeProject === at.composeProject;
    });
  }

  function updateBroadcastState() {
    var cb = $('term-broadcast-cb');
    if (!cb) return;
    var can = broadcastPeers().length > 0;
    if (!can && cb.checked) cb.checked = false;
    cb.disabled = !can;
  }

  // ===== 会话生命周期 =====

  function startExec(tab, shell) {
    // 先订阅(listener 已常驻/懒注册),invoke 返回前的事件进 buffer 后认领
    ensureExecListener();

    AppBus.invoke('manage_exec_start', {
      serverId: state.serverId,
      containerId: tab.containerId,
      // 空/未选 → null,由后端自动探测容器内可用 shell(bash 优先,退回 sh)
      shell: shell || null
    }).then(function (res) {
      // 标签可能已被关闭(或模态关闭导致全部清理)
      if (!cState.exec.tabs[tab.key]) {
        AppBus.invoke('manage_exec_stop', { sessionId: res.session_id }).catch(function () {});
        return;
      }
      tab.sessionId = res.session_id;
      tab.shell = res.shell || shell || 'bash';
      resetTermBuffer(tab);
      termAppendLine(tab, '已连接到容器「' + tab.name + '」(shell: ' + tab.shell + ')');
      if (execActiveTab() === tab) {
        var shellSel = $('term-shell-select');
        if (shellSel) shellSel.value = tabShellOption(tab.shell);
        renderTermEditor();
        // 会话建立即按当前输出区尺寸同步一次(后续变化走 resize 监听/观察器)
        observeTermOutput();
        pushTermResize();
      }
      renderExecTabs();
      claimBufferedEvents(tab);
    }).catch(function (err) {
      var msg = err && err.message ? err.message : String(err);
      termAppendLine(tab, '连接失败: ' + msg);
      tab.eof = true;
      if (execActiveTab() === tab) renderTermEditor();
      renderExecTabs();
      toast('打开终端失败: ' + msg, 'fail');
    });
  }

  // shell 值 → 下拉选项值(探测到 bash/sh 之外的值时回退「自动」)
  function tabShellOption(shell) {
    return (shell === 'bash' || shell === 'sh') ? shell : '';
  }

  // 停止单个标签的会话(quiet 时不提示)
  function stopExecTab(tab, quiet) {
    if (tab.sessionId) {
      var sid = tab.sessionId;
      tab.sessionId = null;
      AppBus.invoke('manage_exec_stop', { sessionId: sid }).catch(function () { /* 忽略 */ });
    }
    tab.history = [];
    tab.histIdx = -1;
    if (!quiet) toast('终端已关闭', 'info');
  }

  function stopAllExecTabs() {
    execTabsAll().forEach(function (t) { stopExecTab(t, true); });
    cState.exec.tabs = {};
    cState.exec.order = [];
    cState.exec.activeKey = null;
  }

  // 简易 ANSI 处理:剥除 ESC 转义序列;\r 回到行首覆盖;\n 换行
  function termWrite(tab, data) {
    if (tab.pend) {
      data = tab.pend + data;
      tab.pend = '';
    }
    var idx = data.lastIndexOf('\x1b');
    if (idx !== -1 && /^\x1b(\[[0-9;?]*|\][^\x07]*)?$/.test(data.slice(idx))) {
      tab.pend = data.slice(idx);
      data = data.slice(0, idx);
    }
    data = data.replace(/\x1b(\[[0-9;?]*[A-Za-z]|\][^\x07]*\x07|[@-Z\\-_])/g, '')
               .replace(/\x07/g, '');

    for (var i = 0; i < data.length; i++) {
      var ch = data[i];
      if (ch === '\n') {
        tab.lines.push(tab.cur);
        if (tab.lines.length > 1000) tab.lines.shift();
        tab.cur = '';
        tab.curIdx = 0;
      } else if (ch === '\r') {
        tab.curIdx = 0; // 回到行首,后续字符覆盖
      } else if (ch === '\t') {
        var pad = 4 - (tab.cur.length % 4);
        for (var t = 0; t < pad; t++) { tab.cur += ' '; tab.curIdx++; }
      } else if (ch >= ' ') {
        if (tab.curIdx < tab.cur.length) {
          tab.cur = tab.cur.slice(0, tab.curIdx) + ch + tab.cur.slice(tab.curIdx + 1);
        } else {
          tab.cur += ch;
        }
        tab.curIdx++;
      }
    }
    if (execActiveTab() === tab) renderTerm(tab);
  }

  function termAppendLine(tab, text) {
    tab.lines.push(text);
    if (tab.lines.length > 1000) tab.lines.shift();
    if (execActiveTab() === tab) renderTerm(tab);
  }

  function resetTermBuffer(tab) {
    tab.lines = [];
    tab.cur = '';
    tab.curIdx = 0;
    tab.pend = '';
    tab.eof = false;
    // 新会话(含切换 shell 重开)远端从默认尺寸起步,清缓存强制重新上报
    tab.lastCols = null;
    tab.lastRows = null;
  }

  function renderTerm(tab) {
    var out = $('term-output');
    if (!out || execActiveTab() !== tab) return;
    // 用户未向上滚动(贴近底部)时才自动滚到底
    var atBottom = out.scrollTop + out.clientHeight >= out.scrollHeight - 40;
    out.textContent = tab.lines.join('\n') + (tab.lines.length ? '\n' : '') + tab.cur;
    if (atBottom) out.scrollTop = out.scrollHeight;
  }

  function onTermInputKey(e) {
    var input = e.target;
    var tab = execActiveTab();
    if (!tab) return;
    if (e.key === 'Enter') {
      var line = input.value;
      if (!tab.sessionId) { toast('会话已结束,请关闭标签或切换 shell 重开', 'warn'); return; }
      // 广播(勾选且有同栈同伴):发给同 compose_project 的全部活跃标签
      var cb = $('term-broadcast-cb');
      var targets = [tab];
      if (cb && cb.checked) {
        var peers = broadcastPeers();
        if (peers.length > 0) targets = [tab].concat(peers);
      }
      for (var i = 0; i < targets.length; i++) {
        (function (t) {
          AppBus.invoke('manage_exec_write', { sessionId: t.sessionId, data: line + '\r' })
            .catch(function () { /* 写失败忽略,输出流会体现 */ });
        })(targets[i]);
      }
      if (line) {
        tab.history.push(line);
        if (tab.history.length > 100) tab.history.shift();
      }
      tab.histIdx = -1;
      input.value = '';
      e.preventDefault();
    } else if (e.key === 'ArrowUp') {
      if (tab.history.length === 0) return;
      if (tab.histIdx === -1) tab.histIdx = tab.history.length - 1;
      else if (tab.histIdx > 0) tab.histIdx--;
      input.value = tab.history[tab.histIdx];
      e.preventDefault();
    } else if (e.key === 'ArrowDown') {
      if (tab.histIdx === -1) return;
      if (tab.histIdx < tab.history.length - 1) {
        tab.histIdx++;
        input.value = tab.history[tab.histIdx];
      } else {
        tab.histIdx = -1;
        input.value = '';
      }
      e.preventDefault();
    }
  }

  // 兼容入口(宿主 manage.js 调用:切服务器时清终端):停**全部**标签会话。
  // 第二十二批多标签后语义升级为「停全部」——宿主调用点语义均为
  // 「环境已变,终端不应存活」,全停即正确行为。
  function stopExecSession(quiet) {
    stopAllExecTabs();
    if (!quiet) toast('终端已关闭', 'info');
  }

  // ===== 阶段五:终端尺寸自适应(接线后端 manage_exec_resize)=====
  // 行式终端(非全屏程序)resize 主要影响远端行宽(长行按新列数折行),属体验
  // 优化:同步失败仅 console.warn,不打扰用户。触发时机:
  // 1) 会话建立成功(startExec 内)与切换标签时各同步一次(尺寸缓存按标签独立);
  // 2) window resize(防抖 300ms,监听器已在 bindEventsC 注册一次);
  // 3) 输出区自身尺寸变化(ResizeObserver,覆盖弹窗 min(1000px,94vw) 宽与
  //    输出区 60vh 高随窗口/布局的变化;无 RO 的老内核仅靠 window resize 兜底)。
  var TERM_RESIZE_DEBOUNCE = 300;
  var termCharSize = null;    // 等宽字符测量缓存 { w: 字符宽(px), h: 行高(px) }
  var termResizeTimer = null; // resize 防抖句柄
  var termResizeObs = null;   // 输出区 ResizeObserver(懒创建)

  // 量测等宽字符尺寸:临时 span 排 100 个 "M",宽/100 = 单字符宽;
  // inline-block + line-height 使 span 高度恰为一行行高
  // (与 .manage-terminal 的 var(--font-mono) / 12px / line-height:1.5 一致)
  function measureTermCharSize() {
    if (termCharSize) return termCharSize;
    var span = document.createElement('span');
    span.style.cssText = 'position:absolute;top:-9999px;left:0;visibility:hidden;' +
      'display:inline-block;white-space:pre;' +
      'font-family:var(--font-mono);font-size:12px;line-height:1.5;';
    span.textContent = new Array(101).join('M'); // 100 个 M
    (document.body || document.documentElement).appendChild(span);
    var rect = span.getBoundingClientRect();
    var w = rect.width / 100;
    var h = rect.height;
    if (span.parentNode) span.parentNode.removeChild(span);
    // 量测异常兜底:12px 等宽字体常见值(字符宽≈0.6em,行高 12×1.5)
    if (!(w > 0)) w = 7.2;
    if (!(h > 0)) h = 18;
    termCharSize = { w: w, h: h };
    return termCharSize;
  }

  // 由 .manage-terminal 输出区客户区推算列/行数(扣内边距后整除字符尺寸)
  function termGridSize() {
    var out = $('term-output');
    if (!out) return null;
    var cw = out.clientWidth;
    var chh = out.clientHeight;
    if (cw <= 0 || chh <= 0) return null; // 模态不可见/尚未布局,量测无意义
    var cs = window.getComputedStyle(out);
    var contentW = cw - (parseFloat(cs.paddingLeft) || 0) - (parseFloat(cs.paddingRight) || 0);
    var contentH = chh - (parseFloat(cs.paddingTop) || 0) - (parseFloat(cs.paddingBottom) || 0);
    var m = measureTermCharSize();
    // 钳制下限:≥20 列 / ≥5 行,防极端小窗算出过小值被远端拒绝
    return {
      cols: Math.max(20, Math.floor(contentW / m.w)),
      rows: Math.max(5, Math.floor(contentH / m.h))
    };
  }

  // 向后端上报**当前标签**的终端尺寸;无会话/已 eof/尺寸未变时跳过,
  // 失败 console.warn 静默(缓存按标签独立)
  function pushTermResize() {
    var tab = execActiveTab();
    if (!tab || !tab.sessionId || tab.eof) return; // 模态关闭后 resize 回调到这里判空直接返回
    var size = termGridSize();
    if (!size) return;
    if (tab.lastCols === size.cols && tab.lastRows === size.rows) return; // 尺寸未变不重发
    var sid = tab.sessionId;
    var prevCols = tab.lastCols;
    var prevRows = tab.lastRows;
    tab.lastCols = size.cols;
    tab.lastRows = size.rows;
    AppBus.invoke('manage_exec_resize', { sessionId: sid, cols: size.cols, rows: size.rows })
      .catch(function (err) {
        // 失败回滚缓存为旧值:后续同尺寸触发不被「未变」去重挡掉,可重试;
        // 会话已重建则不回写,避免旧会话结果污染新会话(新会话缓存起点为 null)
        if (tab.sessionId === sid) {
          tab.lastCols = prevCols;
          tab.lastRows = prevRows;
        }
        console.warn('[manage] 终端尺寸同步失败:', err && err.message ? err.message : err);
      });
  }

  // 防抖入口:window resize 与 ResizeObserver 共用,300ms 内合并为一次上报
  function scheduleTermResize() {
    if (termResizeTimer) window.clearTimeout(termResizeTimer);
    termResizeTimer = window.setTimeout(function () {
      termResizeTimer = null;
      pushTermResize();
    }, TERM_RESIZE_DEBOUNCE);
  }

  // 观察输出区自身尺寸变化(observe 挂载时会先触发一次,与会话建立时的
  // 主动同步互为兜底;回调只进防抖,最终由 pushTermResize 判空/去重)
  function observeTermOutput() {
    if (typeof ResizeObserver === 'undefined') return;
    if (!termResizeObs) {
      termResizeObs = new ResizeObserver(function () { scheduleTermResize(); });
    }
    var out = $('term-output');
    if (out) termResizeObs.observe(out);
  }

  // 解除观察(execOnModalClose / onLeaveC 统一调用,顺带清掉待触发的防抖)
  function unobserveTermOutput() {
    if (termResizeObs) termResizeObs.disconnect();
    if (termResizeTimer) {
      window.clearTimeout(termResizeTimer);
      termResizeTimer = null;
    }
  }

  // closeModal 钩子:模态框被关闭(含遮罩点击/关闭按钮/Esc)时清理**全部**终端会话
  function execOnModalClose() {
    // 还原共用模态尺寸:任何关闭路径(关闭按钮/遮罩点击/Esc/关闭终端)都经
    // closeModal 走到这里,无条件移除终端态/查看态修饰类
    var modal = $('manage-modal');
    if (modal) {
      var card = modal.querySelector('.modal-card');
      if (card) {
        card.classList.remove('modal-terminal');
        card.classList.remove('modal-wide');
      }
    }
    if (cState.exec.order.length > 0 || cState.exec.unlisten) {
      stopAllExecTabs();
      releaseExecListener();
    }
    // 阶段五:解除输出区尺寸观察(所有关闭路径统一经这里收尾)
    unobserveTermOutput();
    // 阶段九:实时跟随日志流随模态关闭停止(关闭即停流,后端主动 close 通道)
    stopLogFollow(true);
  }

  function releaseExecListener() {
    var ex = cState.exec;
    if (ex.unlisten) {
      try { ex.unlisten(); } catch (e) { /* 忽略 */ }
      ex.unlisten = null;
    }
    // 复位同步守卫:注册 promise 尚未 resolve 时关闭模态,防 listening 卡死
    // 导致下次打开不再注册监听;代际自增使在途注册回调自注销(见 ensureExecListener)
    ex.listening = false;
    ex.listenGen++;
    ex.buffer = [];
  }

  // ===== C 阶段:离开 05 页清理 =====
  function onLeaveC() {
    monitorStop(true);
    if (cState.exec.order.length > 0 || cState.exec.unlisten) {
      stopAllExecTabs();
      releaseExecListener();
    }
    unobserveTermOutput(); // 阶段五:离开页面同样解除终端尺寸观察
    hideMonitorError();
    stopLogFollow(true);   // 阶段九:离开页面停掉实时日志流
  }

  // ===== 阶段九:容器/栈日志实时跟随 =====
  // - startLogFollow(kind, target, tail, contentId):开流 manage_log_stream_start
  //   (manage-logs 事件逐行追加;事件先订阅再 invoke,防早到事件丢失)
  // - stopLogFollow(silent):manage_log_stream_stop(后端 select 取消并主动
  //   close 通道);模态关闭/离页统一调用;eof 事件(后端自然结束/出错)复位开关
  // - 追加行上限 LOG_FOLLOW_MAX_LINES = 5000,超限丢最旧
  // - 后端 payload.streamId 为后端代号(前端未知):过滤口径 = 仅处理
  //   「当前活跃会话」的事件,旧流残余因 finishLogFollow 置空而不匹配
  var logFollow = {
    active: false,      // 是否有流在跟随
    kind: null,         // 'container' | 'stack'
    target: null,       // containerId 或 compose_file
    contentId: null     // 输出区 pre 元素 id
  };
  var LOG_FOLLOW_MAX_LINES = 5000;

  /** 订阅 manage-logs 事件(模块级一次;先于任何 start invoke) */
  var followListenerBound = false;
  function bindLogFollowListener() {
    if (followListenerBound) return;
    followListenerBound = true;
    AppBus.on('manage-logs', function (event) {
      var p = (event && event.payload) || {};
      // 只处理当前活跃会话的事件(无活跃流 → 旧流残余/迟到事件一律忽略)
      if (!logFollow.active) return;
      var content = $(logFollow.contentId);
      if (!content) return;
      if (p.eof) {
        // 流结束:被停止(data 为空)→ 静默复位;自然结束/出错 → 提示原因
        var reason = p.data ? String(p.data) : '';
        finishLogFollow();
        if (reason) {
          appendFollowLine(content, '—— ' + reason + ' ——');
        }
        return;
      }
      appendFollowLine(content, String(p.data || ''));
    }).catch(function (err) {
      if (window.console && console.warn) {
        console.warn('[manage] manage-logs 事件监听注册失败:', err);
      }
    });
  }

  /** 追加一行到跟随输出区(上限裁剪 + 自动滚底:接近底部才跟随) */
  function appendFollowLine(content, line) {
    var nearBottom =
      content.scrollHeight - content.scrollTop - content.clientHeight < 40;
    content.appendChild(document.createTextNode(line + '\n'));
    while (content.childNodes.length > LOG_FOLLOW_MAX_LINES) {
      content.removeChild(content.firstChild);
    }
    if (nearBottom) content.scrollTop = content.scrollHeight;
  }

  /** 开启实时跟随(容器 or 栈);返回是否已发起 */
  function startLogFollow(kind, target, tail, contentId) {
    if (logFollow.active) stopLogFollow(false);
    bindLogFollowListener();
    logFollow.active = true;
    logFollow.kind = kind;
    logFollow.target = target;
    logFollow.contentId = contentId;
    var content = $(contentId);
    if (content) {
      content.appendChild(document.createTextNode('—— 实时跟随已开启 ——\n'));
      content.scrollTop = content.scrollHeight;
    }
    AppBus.invoke('manage_log_stream_start', {
      serverId: state.serverId,
      passwordPlain: null,
      tgt: {
        target: kind,
        containerId: kind === 'container' ? target : null,
        composeFile: kind === 'stack' ? target : null,
        tail: tail
      }
    }).then(function () {
      // invoke 成功不代表已连上;数据只经事件(契约约定),无需处理返回值
    }).catch(function (err) {
      var msg = err && err.message ? err.message : String(err);
      var wasActive = logFollow.active;
      finishLogFollow();
      var content2 = $(contentId);
      if (content2 && wasActive) {
        appendFollowLine(content2, '—— 实时跟随开启失败: ' + msg + ' ——');
      }
    });
    return true;
  }

  /** 结束跟随的本地状态(不调后端;eof/出错路径用) */
  function finishLogFollow() {
    logFollow.active = false;
    var btn = $('log-follow-btn');
    if (btn) btn.checked = false;
  }

  /** 停流:调后端 stop + 本地状态复位(silent=true 不改输出区文案) */
  function stopLogFollow(silent) {
    if (!logFollow.active) return;
    var contentId = logFollow.contentId;
    finishLogFollow();
    if (!silent) {
      var content = $(contentId);
      if (content) {
        content.appendChild(document.createTextNode('—— 实时跟随已关闭 ——\n'));
        content.scrollTop = content.scrollHeight;
      }
    }
    AppBus.invoke('manage_log_stream_stop', {}).catch(function () {});
  }

  /** 构建日志模态顶栏的「实时跟随」开关(容器/栈共用) */
  function buildFollowToggle(kind, target, tailSelId, contentId) {
    var bar = document.createElement('label');
    bar.className = 'log-follow-toggle';
    bar.innerHTML =
      '<input type="checkbox" id="log-follow-btn">' +
      '<span>实时跟随(容器/栈日志)</span>';
    var chk = bar.querySelector('#log-follow-btn');
    if (chk) {
      chk.addEventListener('change', function () {
        if (chk.checked) {
          var tail = 0;
          var sel = $(tailSelId);
          if (sel) tail = parseInt(sel.value, 10) || 0;
          startLogFollow(kind, target, tail, contentId);
        } else {
          stopLogFollow(false);
        }
      });
    }
    return bar;
  }

  window.ManageStacks = { buildFollowToggle: buildFollowToggle, execOnModalClose: execOnModalClose, monitorStop: monitorStop, onLeaveC: onLeaveC, openTerminal: openTerminal, refreshStacks: refreshStacks, stopExecSession: stopExecSession };
})();
