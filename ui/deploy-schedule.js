// // 定时部署模态(第二十二批):日程列表 + 新建/编辑表单。
// //
// // - 数据源:`deploy_schedules_list/save/delete` 三命令(camelCase 契约,
// //   与后端 deploy_schedule.rs 直通);项目/服务器来自 get_config,
// //   单镜像模式的镜像来自 list_images。
// // - 日程语义:每天 HH:MM(daily)或一次性延迟(once;目标日 = 保存当天,
// //   过点未运行即停用);到点由**后端**调度器发起部署,本模态只维护日程。
// // - 关闭三通道 + Esc(window.isTopModal 仲裁)+ 焦点三件套,与全站一致。
// // - 本文件为自含 IIFE:不新增 window.<Kit> 键(模块间无消费关系),
// //   verify/scope-integrity.js 的 CHAIN 已加入本文件。
(function () {
  'use strict';

  var st = {
    schedules: [],   // deploy_schedules_list 结果
    cfg: null,       // get_config 结果(projects/servers)
    images: [],      // list_images 结果(单镜像模式用)
    editing: null,   // 正在编辑的日程(null = 列表视图);'new' = 新建
    busy: false,
    armDeleteId: null, // 两步删除:已武装的日程 id
    armTimer: null
  };

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function findById(list, id) {
    if (!Array.isArray(list)) return null;
    for (var i = 0; i < list.length; i++) {
      if (list[i] && String(list[i].id) === String(id)) return list[i];
    }
    return null;
  }

  function modeLabel(mode) {
    return mode === 'single' ? '单镜像' : '整栈';
  }

  function kindLabel(kind) {
    return kind === 'once' ? '一次性' : '每天';
  }

  // ===== 打开/关闭 =====

  function openModal() {
    var overlay = document.getElementById('deploy-schedule-modal');
    if (!overlay) return;
    st.editing = null;
    st.armDeleteId = null;
    overlay.classList.remove('hidden');
    window.modalFocusOpen(overlay);
    loadAll();
  }

  function closeModal() {
    var overlay = document.getElementById('deploy-schedule-modal');
    if (!overlay) return;
    if (st.busy) {
      window.toast('保存中,请稍候…', 'warn');
      return;
    }
    if (st.armTimer) { clearTimeout(st.armTimer); st.armTimer = null; }
    st.armDeleteId = null;
    overlay.classList.add('hidden');
    window.modalFocusClose(overlay);
  }

  // ===== 数据加载 =====

  function loadAll() {
    var body = document.getElementById('deploy-schedule-modal-body');
    if (body) {
      body.innerHTML = '';
      body.appendChild(el('div', 'migrate-plan-empty', '加载中…'));
    }
    Promise.all([
      window.AppBus.invoke('deploy_schedules_list').catch(function () { return []; }),
      window.AppBus.invoke('get_config').catch(function () { return null; }),
      window.AppBus.invoke('list_images').catch(function () { return []; })
    ]).then(function (res) {
      st.schedules = Array.isArray(res[0]) ? res[0] : [];
      st.cfg = res[1] || null;
      st.images = Array.isArray(res[2]) ? res[2] : [];
      render();
    });
  }

  // ===== 渲染入口 =====

  function render() {
    var body = document.getElementById('deploy-schedule-modal-body');
    if (!body) return;
    body.innerHTML = '';
    if (st.editing !== null) {
      renderForm(body);
    } else {
      renderList(body);
    }
  }

  // ===== 列表视图 =====

  function renderList(body) {
    var headRow = el('div', 'sched-head-row');
    headRow.appendChild(el('div', 'sched-hint',
      '到点由后端自动发起部署(应用需在运行中;错过不补跑,详见上次结果)。'));
    var newBtn = el('button', 'btn btn-sm btn-primary', '新建日程');
    newBtn.type = 'button';
    newBtn.addEventListener('click', function () {
      st.editing = 'new';
      render();
    });
    headRow.appendChild(newBtn);
    body.appendChild(headRow);

    if (!st.cfg) {
      body.appendChild(el('div', 'check-error', '读取配置失败,请检查后重试'));
      return;
    }
    if (st.schedules.length === 0) {
      body.appendChild(el('div', 'migrate-plan-empty', '(暂无定时日程,点「新建日程」添加)'));
      return;
    }

    var table = el('table', 'data-table sched-table');
    var thead = el('thead');
    var tr = el('tr');
    ['项目 @ 服务器', '模式', '时间', '启用', '上次结果', '操作'].forEach(function (h) {
      var th = el('th', '', h);
      th.scope = 'col';
      tr.appendChild(th);
    });
    thead.appendChild(tr);
    table.appendChild(thead);

    var tbody = el('tbody');
    st.schedules.forEach(function (s) {
      var row = el('tr');
      if (!s.enabled) row.className = 'sched-disabled';

      var prj = findById(st.cfg.projects, s.projectId);
      var srv = findById(st.cfg.servers, s.serverId);
      var nameCell = el('td');
      nameCell.appendChild(el('div', '', (prj ? prj.name : '(项目已删除)') + ' @ ' + (srv ? srv.name : '(服务器已删除)')));
      if (s.mode === 'single' && s.imageRef) {
        nameCell.appendChild(el('div', 'sched-sub mono', s.imageRef));
      }
      row.appendChild(nameCell);

      row.appendChild(el('td', '', modeLabel(s.mode) + (s.mode === 'stack' ? (s.forceArchive ? ' · 强制留档' : '') : '')));
      row.appendChild(el('td', 'mono', s.time + ' · ' + kindLabel(s.kind)));

      // 启用开关(点按即保存,不改编辑态)
      var toggleCell = el('td', 'sched-op-cell');
      var toggle = el('button', 'btn btn-sm' + (s.enabled ? ' btn-primary' : ''), s.enabled ? '已启用' : '已停用');
      toggle.type = 'button';
      toggle.title = s.enabled ? '点击停用' : '点击启用';
      toggle.addEventListener('click', function () {
        var next = {};
        for (var k in s) { if (Object.prototype.hasOwnProperty.call(s, k)) next[k] = s[k]; }
        next.enabled = !s.enabled;
        save(next, true);
      });
      toggleCell.appendChild(toggle);
      row.appendChild(toggleCell);

      row.appendChild(el('td', 'sched-result', s.lastResult || '—'));

      var ops = el('td', 'sched-op-cell');
      var editBtn = el('button', 'btn btn-sm', '编辑');
      editBtn.type = 'button';
      editBtn.addEventListener('click', function () {
        st.editing = s.id;
        render();
      });
      ops.appendChild(editBtn);

      var delBtn = el('button', 'btn btn-sm btn-danger', st.armDeleteId === s.id ? '确认删除?' : '删除');
      delBtn.type = 'button';
      if (st.armDeleteId === s.id) delBtn.classList.add('is-armed');
      delBtn.addEventListener('click', function () {
        if (st.armDeleteId === s.id) {
          if (st.armTimer) { clearTimeout(st.armTimer); st.armTimer = null; }
          st.armDeleteId = null;
          remove(s.id);
        } else {
          if (st.armTimer) clearTimeout(st.armTimer);
          st.armDeleteId = s.id;
          render();
          st.armTimer = setTimeout(function () {
            st.armTimer = null;
            if (st.armDeleteId === s.id) { st.armDeleteId = null; render(); }
          }, 3000);
        }
      });
      ops.appendChild(delBtn);
      row.appendChild(ops);

      tbody.appendChild(row);
    });
    table.appendChild(tbody);
    body.appendChild(table);
  }

  function remove(id) {
    st.busy = true;
    window.AppBus.invoke('deploy_schedules_delete', { id: id })
      .then(function () {
        window.toast('日程已删除', 'ok');
        return refreshSchedules();
      })
      .catch(function (err) {
        window.toast('删除失败:' + (window.errText(err) || '未知错误'), 'fail');
      })
      .then(function () { st.busy = false; render(); });
  }

  function refreshSchedules() {
    return window.AppBus.invoke('deploy_schedules_list').then(function (list) {
      st.schedules = Array.isArray(list) ? list : [];
    });
  }

  function save(schedule, quiet) {
    st.busy = true;
    return window.AppBus.invoke('deploy_schedules_save', { schedule: schedule })
      .then(function () {
        window.toast(quiet ? '已更新' : '日程已保存', 'ok');
        st.editing = null;
        return refreshSchedules();
      })
      .catch(function (err) {
        window.toast('保存失败:' + (window.errText(err) || '未知错误'), 'fail');
      })
      .then(function () { st.busy = false; render(); });
  }

  // ===== 表单视图 =====

  function renderForm(body) {
    var editing = st.editing === 'new' ? null : findById(st.schedules, st.editing);
    var isEdit = !!editing;

    var form = document.createElement('form');
    form.className = 'sched-form';
    form.setAttribute('novalidate', 'novalidate');

    // --- 项目 ---
    var prjRow = el('div', 'form-row');
    prjRow.appendChild(window.formLabel('项目', 'PROJECT', true, 'schedf-project'));
    var prjSel = document.createElement('select');
    prjSel.id = 'schedf-project';
    prjSel.className = 'form-input';
    var projects = (st.cfg && Array.isArray(st.cfg.projects)) ? st.cfg.projects : [];
    if (projects.length === 0) {
      var opt0 = el('option', '', '(暂无项目,请先在 03 页创建)');
      opt0.value = '';
      prjSel.appendChild(opt0);
    }
    projects.forEach(function (p) {
      var o = el('option', '', p.name);
      o.value = String(p.id);
      prjSel.appendChild(o);
    });
    prjRow.appendChild(prjSel);
    form.appendChild(prjRow);

    // --- 服务器 ---
    var srvRow = el('div', 'form-row');
    srvRow.appendChild(window.formLabel('服务器', 'SERVER', true, 'schedf-server'));
    var srvSel = document.createElement('select');
    srvSel.id = 'schedf-server';
    srvSel.className = 'form-input';
    srvRow.appendChild(srvSel);
    form.appendChild(srvRow);

    // --- 模式 ---
    var modeRow = el('div', 'form-row');
    modeRow.appendChild(window.formLabel('部署模式', 'MODE', true));
    var modeWrap = el('div', 'sched-radio-row');
    var modeStack = radioInput('schedf-mode-stack', 'sched-mode', 'stack', '整栈部署(compose)');
    var modeSingle = radioInput('schedf-mode-single', 'sched-mode', 'single', '单镜像');
    modeWrap.appendChild(modeStack.label);
    modeWrap.appendChild(modeSingle.label);
    modeRow.appendChild(modeWrap);
    form.appendChild(modeRow);

    // --- 镜像(单镜像模式)---
    var imgRow = el('div', 'form-row');
    imgRow.appendChild(window.formLabel('镜像', 'IMAGE', false, 'schedf-image'));
    var imgSel = document.createElement('select');
    imgSel.id = 'schedf-image';
    imgSel.className = 'form-input';
    buildImageOptions(imgSel);
    imgRow.appendChild(imgSel);
    imgRow.id = 'schedf-image-row';
    form.appendChild(imgRow);

    // --- 日程类型 + 时间 ---
    var kindRow = el('div', 'form-row');
    kindRow.appendChild(window.formLabel('日程类型', 'KIND', true));
    var kindWrap = el('div', 'sched-radio-row');
    var kindDaily = radioInput('schedf-kind-daily', 'sched-kind', 'daily', '每天');
    var kindOnce = radioInput('schedf-kind-once', 'sched-kind', 'once', '一次性(今天)');
    kindWrap.appendChild(kindDaily.label);
    kindWrap.appendChild(kindOnce.label);
    kindRow.appendChild(kindWrap);
    form.appendChild(kindRow);

    var timeRow = el('div', 'form-row');
    timeRow.appendChild(window.formLabel('触发时刻', 'TIME', true, 'schedf-time'));
    var timeInput = document.createElement('input');
    timeInput.id = 'schedf-time';
    timeInput.className = 'form-input mono';
    timeInput.type = 'text';
    timeInput.maxLength = 5;
    timeInput.placeholder = 'HH:MM,如 03:30';
    timeInput.autocomplete = 'off';
    timeRow.appendChild(timeInput);
    var timeHint = el('div', 'form-hint', '本地时间;一次性日程 = 今天该时刻(过点未运行即停用,不会补跑)');
    timeRow.appendChild(timeHint);
    form.appendChild(timeRow);

    // --- 选项 ---
    var optRow = el('div', 'form-row');
    optRow.appendChild(window.formLabel('选项', 'OPTIONS', false));
    var optWrap = el('div', 'sched-check-col');
    var skipChk = checkboxInput('schedf-skip', '跳过未变化镜像(智能传输,推荐)');
    var archChk = checkboxInput('schedf-archive', '强制留档(整栈;未变化服务也打包含入归档)');
    optWrap.appendChild(skipChk.label);
    optWrap.appendChild(archChk.label);
    optRow.appendChild(optWrap);
    form.appendChild(optRow);

    // --- 动作 ---
    var actions = el('div', 'form-actions');
    var backBtn = el('button', 'btn', '返回列表');
    backBtn.type = 'button';
    backBtn.addEventListener('click', function () {
      st.editing = null;
      render();
    });
    var saveBtn = el('button', 'btn btn-primary', isEdit ? '保存修改' : '创建日程');
    saveBtn.type = 'submit';
    actions.appendChild(backBtn);
    actions.appendChild(saveBtn);
    form.appendChild(actions);

    body.appendChild(form);

    // --- 回填与联动 ---
    if (editing) {
      prjSel.value = String(editing.projectId || '');
      modeStack.input.checked = editing.mode !== 'single';
      modeSingle.input.checked = editing.mode === 'single';
      kindDaily.input.checked = editing.kind !== 'once';
      kindOnce.input.checked = editing.kind === 'once';
      timeInput.value = editing.time || '';
      skipChk.input.checked = !!editing.skipUnchanged;
      archChk.input.checked = !!editing.forceArchive;
      if (editing.imageRef) {
        // 存量镜像不在本机列表时补一个占位项,避免选择被重置
        ensureImageOption(imgSel, editing.imageRef);
        imgSel.value = editing.imageRef;
      }
    } else {
      skipChk.input.checked = true;
      modeStack.input.checked = true; // 新建默认整栈模式
      kindDaily.input.checked = true; // 新建默认每天
      // 新建:服务器按项目默认服务器预选
      syncServerOptions(srvSel, prjSel.value, null);
    }

    prjSel.addEventListener('change', function () {
      syncServerOptions(srvSel, prjSel.value, null);
    });
    // 编辑回填时服务器预选
    if (editing) {
      syncServerOptions(srvSel, editing.projectId, editing.serverId);
    }
    syncModeVisibility();

    function currentMode() {
      return modeSingle.input.checked ? 'single' : 'stack';
    }
    function syncModeVisibility() {
      var single = currentMode() === 'single';
      imgRow.classList.toggle('hidden', !single);
      // 强制留档仅整栈有意义
      archChk.label.classList.toggle('hidden', single);
    }
    modeStack.input.addEventListener('change', syncModeVisibility);
    modeSingle.input.addEventListener('change', syncModeVisibility);

    form.addEventListener('submit', function (e) {
      e.preventDefault();
      var timeVal = String(timeInput.value || '').trim();
      if (!/^([01][0-9]|2[0-3]):[0-5][0-9]$/.test(timeVal)) {
        window.toast('时间格式应为 HH:MM(00:00-23:59)', 'warn');
        timeInput.focus();
        return;
      }
      if (!prjSel.value) { window.toast('请先选择项目', 'warn'); return; }
      if (!srvSel.value) { window.toast('请先选择服务器', 'warn'); return; }
      if (currentMode() === 'single' && !imgSel.value) {
        window.toast('单镜像模式需要选择镜像', 'warn');
        return;
      }
      var sched = {
        id: editing ? editing.id : newUuid(),
        projectId: prjSel.value,
        serverId: srvSel.value,
        mode: currentMode(),
        imageRef: currentMode() === 'single' ? String(imgSel.value) : '',
        kind: kindOnce.input.checked ? 'once' : 'daily',
        time: timeVal,
        skipUnchanged: !!skipChk.input.checked,
        forceArchive: !!archChk.input.checked,
        enabled: editing ? !!editing.enabled : true,
        createdAt: editing ? editing.createdAt : '',
        lastRunDate: editing ? editing.lastRunDate : '',
        lastResult: editing ? editing.lastResult : ''
      };
      save(sched, false);
    });
  }

  function syncServerOptions(srvSel, projectId, preferServerId) {
    srvSel.innerHTML = '';
    var servers = (st.cfg && Array.isArray(st.cfg.servers)) ? st.cfg.servers : [];
    var prj = findById(st.cfg ? st.cfg.projects : [], projectId);
    var defaultSrv = prj && prj.default_server_id ? String(prj.default_server_id) : '';
    // B3:按归属标签(首标签)分组成 optgroup(与部署页/回滚中心同口径)
    window.appendGroupedOptions(srvSel, window.serverOptionsFor(servers));
    var pick = preferServerId || defaultSrv;
    if (pick && findById(servers, pick)) srvSel.value = String(pick);
  }

  function buildImageOptions(sel) {
    sel.innerHTML = '';
    var seen = {};
    st.images.forEach(function (img) {
      if (!img || !img.repository || img.repository === '<none>') return;
      var ref = img.repository + ':' + img.tag;
      if (seen[ref]) return;
      seen[ref] = true;
      var o = el('option', '', ref);
      o.value = ref;
      sel.appendChild(o);
    });
    if (sel.options.length === 0) {
      var o0 = el('option', '', '(本机暂无镜像)');
      o0.value = '';
      sel.appendChild(o0);
    }
  }

  function ensureImageOption(sel, ref) {
    for (var i = 0; i < sel.options.length; i++) {
      if (sel.options[i].value === ref) return;
    }
    var o = el('option', '', ref);
    o.value = ref;
    sel.insertBefore(o, sel.firstChild);
  }

  function radioInput(id, name, value, labelText) {
    var input = document.createElement('input');
    input.type = 'radio';
    input.id = id;
    input.name = name;
    input.value = value;
    var label = el('label', 'sched-radio', '');
    label.setAttribute('for', id);
    label.appendChild(input);
    label.appendChild(document.createTextNode(' ' + labelText));
    return { input: input, label: label };
  }

  function checkboxInput(id, labelText) {
    var input = document.createElement('input');
    input.type = 'checkbox';
    input.id = id;
    var label = el('label', 'sched-chk', '');
    label.setAttribute('for', id);
    label.appendChild(input);
    label.appendChild(document.createTextNode(' ' + labelText));
    return { input: input, label: label };
  }

  function newUuid() {
    if (window.crypto && typeof window.crypto.randomUUID === 'function') {
      return window.crypto.randomUUID();
    }
    // 回退:时间戳 + 随机(与后端仅要求非空唯一)
    return 'sched-' + Date.now() + '-' + Math.random().toString(16).slice(2, 10);
  }

  // ===== 初始化 =====

  function bindStatic() {
    var entry = document.getElementById('deploy-schedule-btn');
    if (entry) entry.addEventListener('click', openModal);

    var closeBtn = document.getElementById('deploy-schedule-modal-close');
    if (closeBtn) closeBtn.addEventListener('click', closeModal);

    var overlay = document.getElementById('deploy-schedule-modal');
    if (overlay) {
      overlay.addEventListener('click', function (e) {
        if (e.target === overlay) closeModal();
      });
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && window.isTopModal('deploy-schedule-modal')) {
          closeModal();
        }
      });
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', bindStatic);
  } else {
    bindStatic();
  }
})();
