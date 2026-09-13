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
    exec: {
      sessionId: null, unlisten: null, containerId: null, name: '',
      lines: [], cur: '', curIdx: 0, eof: false, pend: '',
      history: [], histIdx: -1,
      lastCols: null, lastRows: null   // 最近一次同步给后端的终端尺寸(去重用)
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
      // 阶段五:会话建立即按当前输出区尺寸同步一次(后续变化走 resize 监听/观察器)
      observeTermOutput();
      pushTermResize();
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
      // (第十六批:原因文案剥码标记后展示)
      if (payload.error) {
        var reason = window.errStripCode(String(payload.error));
        termAppendLine('[会话已结束: ' + reason + ']');
        toast('终端会话结束: ' + reason, 'warn');
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
    // 新会话(含切换 shell 重开)远端从默认尺寸起步,清缓存强制重新上报
    cState.exec.lastCols = null;
    cState.exec.lastRows = null;
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

  // ===== 阶段五:终端尺寸自适应(接线后端 manage_exec_resize)=====
  // 行式终端(非全屏程序)resize 主要影响远端行宽(长行按新列数折行),属体验
  // 优化:同步失败仅 console.warn,不打扰用户。触发时机:
  // 1) 会话建立成功(startExec 内)同步一次;
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

  // 向后端上报当前尺寸;无会话/已 eof/尺寸未变时跳过,失败 console.warn 静默
  function pushTermResize() {
    var ex = cState.exec;
    if (!ex.sessionId || ex.eof) return; // 模态关闭后 resize 回调到这里判空直接返回
    var size = termGridSize();
    if (!size) return;
    if (ex.lastCols === size.cols && ex.lastRows === size.rows) return; // 尺寸未变不重发
    var sid = ex.sessionId;
    var prevCols = ex.lastCols;
    var prevRows = ex.lastRows;
    ex.lastCols = size.cols;
    ex.lastRows = size.rows;
    AppBus.invoke('manage_exec_resize', { sessionId: sid, cols: size.cols, rows: size.rows })
      .catch(function (err) {
        // 失败回滚缓存为旧值:后续同尺寸触发不被「未变」去重挡掉,可重试;
        // 会话已重建则不回写,避免旧会话结果污染新会话(新会话缓存起点为 null)
        if (ex.sessionId === sid) {
          ex.lastCols = prevCols;
          ex.lastRows = prevRows;
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

  // closeModal 钩子:模态框被关闭(含遮罩点击/关闭按钮/Esc)时清理终端会话
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
    var ex = cState.exec;
    if (ex.sessionId || ex.unlisten) {
      stopExecSession(true);
    }
    // 阶段五:解除输出区尺寸观察(所有关闭路径统一经这里收尾)
    unobserveTermOutput();
    // 阶段九:实时跟随日志流随模态关闭停止(关闭即停流,后端主动 close 通道)
    stopLogFollow(true);
  }

  // ===== C 阶段:离开 05 页清理 =====
  function onLeaveC() {
    monitorStop(true);
    if (cState.exec.sessionId || cState.exec.unlisten) {
      stopExecSession(true);
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
