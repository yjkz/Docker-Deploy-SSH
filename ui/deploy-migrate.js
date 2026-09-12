// deploy-migrate.js — 项目跨服务器迁移模态(preview → 确认 → migrate_project_start)
// 第十二批 JS 结构治理:自 deploy.js 拆出;外部依赖经
// window.DeployKit 桥接(定义见宿主文件尾部),入口经 window.DeployMigrate 供宿主回调。
(function () {
  'use strict';
  var K = window.DeployKit;
  var st = K.st;
  var el = K.el;
  var findById = K.findById;
  var migState = K.migState;
  var refreshControls = K.refreshControls;
  var loadPageData = K.loadPageData;

  // ===== 项目跨服务器迁移(源服务器 → 目标服务器)=====
  // - 入口:部署配置区的「迁移项目…」(整栈部署 tab 右侧)
  // - 流程:选源/目标 → 预检(只读,migrate_project_preview)→ 计划确认 → 执行
  //   (migrate_project_start → migrate-project-log / migrate-project-done 事件)
  // - 与部署/批量互斥(migState 声明在文件上方 refreshControls 之前)

  /** 迁移日志区追加一行(上限 2000 行丢最旧,近底才自动滚) */
  var MIGRATE_LOG_MAX = 2000;
  function appendMigrateProjectLine(line) {
    var body = document.getElementById('migrate-project-log');
    if (!body) return;
    var nearBottom = body.scrollHeight - body.scrollTop - body.clientHeight < 40;
    var existing = body.textContent ? body.textContent.split('\n') : [];
    existing.push(line === null || line === undefined ? '' : String(line));
    if (existing.length > MIGRATE_LOG_MAX) {
      existing.splice(0, existing.length - MIGRATE_LOG_MAX);
    }
    body.textContent = existing.join('\n');
    if (nearBottom) body.scrollTop = body.scrollHeight;
  }

  /** 迁移事件订阅(先订阅再 invoke;模块级单次注册) */
  function bindMigrateProjectListeners() {
    if (migState.listenerBound) return;
    migState.listenerBound = true;
    window.AppBus.on('migrate-project-log', function (event) {
      var p = (event && event.payload) || {};
      appendMigrateProjectLine(p.line || '');
    }).catch(function (err) {
      if (window.console && console.warn) {
        console.warn('[deploy] migrate-project-log 注册失败:', err);
      }
    });
    window.AppBus.on('migrate-project-done', function (event) {
      var p = (event && event.payload) || {};
      migState.active = false;
      refreshControls();
      var ok = p.success === true;
      var msg = String(p.message || (ok ? '迁移完成' : '迁移失败'));
      appendMigrateProjectLine('—— ' + msg + ' ——');
      var warns = Array.isArray(p.warnings) ? p.warnings : [];
      if (warns.length > 0) {
        appendMigrateProjectLine('—— 警告(' + warns.length + ' 条)——');
        warns.forEach(function (w) { appendMigrateProjectLine('  · ' + w); });
      }
      // 收尾按钮复位:成功后可关闭;失败允许重试(重新预检)
      setMigrateButtons(ok ? 'done' : 'failed');
      window.toast(ok ? '项目迁移完成' : '项目迁移失败: ' + msg, ok ? 'ok' : 'fail');
      // 配置已改绑(成功时),刷新页面数据让项目列表/部署页反映新服务器
      if (ok) loadPageDataOnce();
    }).catch(function (err) {
      if (window.console && console.warn) {
        console.warn('[deploy] migrate-project-done 注册失败:', err);
      }
    });
  }

  /** 迁移模态按钮状态机:'idle'(可预检)| 'previewed'(可执行)| 'running' | 'done' | 'failed' */
  function setMigrateButtons(phase) {
    var previewBtn = document.getElementById('migrate-project-preview-btn');
    var startBtn = document.getElementById('migrate-project-start-btn');
    var closeBtn = document.getElementById('migrate-project-modal-close');
    var running = phase === 'running';
    // 第六批:running 阶段带步进条(迁移是长时间等待,此处最需要图形反馈)。
    // setBtnBusy(btn, false, label) 会清掉所有子节点(含步进条)并解除禁用,
    // 故非 running 阶段同样走它来复位,保证「进得去、出得来」。
    if (previewBtn) {
      window.setBtnBusy(previewBtn, running, running ? '迁移中…' : '重新预检');
    }
    if (startBtn) {
      // 「确认迁移」恒可见、预检通过(errors 为空)后才可用 —— 比显隐切换
      // 更容易理解下一步在哪(本项目对"点不动"类问题的既定口径)
      window.setBtnBusy(startBtn, running, running ? '迁移中…' : '确认迁移');
      if (!running) {
        var canStart = phase === 'previewed' &&
          !!(migState.plan && (!migState.plan.errors || migState.plan.errors.length === 0));
        startBtn.disabled = !canStart;
      }
    }
    if (closeBtn) closeBtn.disabled = running;
  }

  function migVal(id) {
    var n = document.getElementById(id);
    return n ? String(n.value) : '';
  }

  /** 迁移成功后刷新页面数据(配置已改绑到目标服务器) */
  function loadPageDataOnce() {
    loadPageData();
  }

  function openMigrateProjectModal() {
    if (migState.active) return;
    if (st.deploying || st.checking || (st.batch && st.batch.active)) {
      window.toast('部署进行中,无法发起迁移', 'warn');
      return;
    }
    var projectId = migVal('deploy-project');
    var project = findById(st.cfg ? st.cfg.projects : [], projectId);
    if (!project) { window.toast('请先选择项目', 'warn'); return; }
    var servers = (st.cfg && st.cfg.servers) || [];
    if (servers.length < 2) {
      window.toast('至少需要两台服务器才能迁移', 'warn');
      return;
    }
    // 源服务器默认带出:项目默认服务器 → 当前部署页所选 → 第一台
    var defaultSrc = project.default_server_id || migVal('deploy-server') ||
      (servers[0] && servers[0].id) || '';
    if (findById(servers, defaultSrc) === null) defaultSrc = (servers[0] && servers[0].id) || '';

    migState.plan = null;
    bindMigrateProjectListeners();
    renderMigrateProjectModal(project, servers, defaultSrc);
    var modal = document.getElementById('migrate-project-modal');
    if (modal) {
      modal.classList.remove('hidden');
      window.modalFocusOpen(modal);
    }
  }

  function closeMigrateProjectModal() {
    if (migState.active) {
      window.toast('迁移执行中,完成后才能关闭', 'warn');
      return;
    }
    var modal = document.getElementById('migrate-project-modal');
    if (modal) {
      modal.classList.add('hidden');
      window.modalFocusClose(modal);
    }
  }

  /** 构建迁移模态内容(选择区 + 计划区 + 日志区) */
  function renderMigrateProjectModal(project, servers, defaultSrc) {
    var body = document.getElementById('migrate-project-modal-body');
    if (!body) return;

    // 表单语义:选择区/目录输入进 <form novalidate>(第十三批);
    // 计划区与日志区是**结果输出**不是表单内容,留在 form 之外 ——
    // 包进去会让「预检结果」成为表单的一部分(B/S 语义与 Enter 提交范围都不对)。
    var formApi = window.beginForm(body, 'migrate-project-modal-title');
    var form = formApi.form;
    form.classList.add('migrate-form');

    var hint = el('p', 'confirm-msg',
      '把项目「' + (project.name || project.id) + '」的镜像、compose 文件、数据卷与发布归档' +
      '从一台服务器搬到另一台,并在目标服务器上启动。源服务器的内容会保留不动。');
    form.appendChild(hint);

    // ---- 选择区 ----
    var grid = el('div', 'migrate-form-grid');

    // 第十二批修复:原 body.insertBefore(标题, grid) 在 grid 尚未挂载时调用,
    // 按 DOM 规范必抛 NotFoundError —— 迁移模态自该行引入起打开即报错。改为
    // 先持引用,待 grid 挂载后紧贴其前插入(顺序意图不变)
    var gridTitle = window.formGroupTitle('迁移目标', 'TARGET');
    var srcRow = el('div', 'form-row');
    srcRow.appendChild(window.formLabel('源服务器(内容所在)', 'SOURCE', true, 'migrate-project-source'));
    var srcSel = document.createElement('select');
    // 本轮修复:此前漏 .form-input,控件退化为 19px 原生外观(与全站 36px
    // 墨线 + 底边 2px + 45° 三角的规范控件明显不一致)
    srcSel.className = 'form-input';
    srcSel.id = 'migrate-project-source';
    servers.forEach(function (s) {
      var opt = document.createElement('option');
      opt.value = s.id;
      opt.textContent = (s.name || s.id) + ' (' + (s.host || '') + ')';
      if (s.id === defaultSrc) opt.selected = true;
      srcSel.appendChild(opt);
    });
    srcRow.appendChild(srcSel);
    grid.appendChild(srcRow);

    var tgtRow = el('div', 'form-row');
    tgtRow.appendChild(window.formLabel('目标服务器(迁移到)', 'TARGET', true, 'migrate-project-target'));
    var tgtSel = document.createElement('select');
    tgtSel.className = 'form-input';   // 同 srcSel:补回缺失的控件样式
    tgtSel.id = 'migrate-project-target';
    servers.forEach(function (s) {
      if (s.id === defaultSrc) return; // 目标不能等于源
      var opt = document.createElement('option');
      opt.value = s.id;
      opt.textContent = (s.name || s.id) + ' (' + (s.host || '') + ')';
      tgtSel.appendChild(opt);
    });
    tgtRow.appendChild(tgtSel);
    grid.appendChild(tgtRow);

    // 源变化时重建目标下拉(排除新源)
    srcSel.addEventListener('change', function () {
      var cur = String(srcSel.value);
      var prev = String(tgtSel.value);
      tgtSel.innerHTML = '';
      servers.forEach(function (s) {
        if (s.id === cur) return;
        var opt = document.createElement('option');
        opt.value = s.id;
        opt.textContent = (s.name || s.id) + ' (' + (s.host || '') + ')';
        tgtSel.appendChild(opt);
      });
      if (prev && prev !== cur) tgtSel.value = prev;
    });

    // ---- 迁移项 ----
    var optsRow = el('div', 'form-row');
    optsRow.appendChild(window.formLabel('迁移内容', 'CONTENT'));
    var optsBox = el('div', 'migrate-options');

    var alwaysChk = document.createElement('input');
    alwaysChk.type = 'checkbox';
    alwaysChk.checked = true;
    alwaysChk.disabled = true;
    alwaysChk.id = 'migrate-opt-required';
    var alwaysLbl = el('label', 'deploy-checkbox');
    alwaysLbl.appendChild(alwaysChk);
    alwaysLbl.appendChild(el('span', '', '镜像 + compose 文件(必选)'));
    optsBox.appendChild(alwaysLbl);

    var volLbl = el('label', 'deploy-checkbox');
    var volChk = document.createElement('input');
    volChk.type = 'checkbox';
    volChk.id = 'migrate-opt-volumes';
    volChk.checked = true;
    volLbl.appendChild(volChk);
    volLbl.appendChild(el('span', '', '数据卷(含数据库等持久数据)'));
    volLbl.title = '导出时会临时停止源服务器上的服务以保证数据一致,导出完成立即恢复';
    optsBox.appendChild(volLbl);

    var relLbl = el('label', 'deploy-checkbox migrate-release-count');
    var relInput = document.createElement('input');
    relInput.type = 'number';
    relInput.id = 'migrate-opt-releases';
    relInput.min = '0';
    relInput.max = '20';
    relInput.value = '1';
    relLbl.appendChild(relInput);
    relLbl.appendChild(el('span', '', '个旧版本归档(0 = 不搬;用于回滚到历史版本)'));
    optsBox.appendChild(relLbl);

    optsRow.appendChild(optsBox);
    grid.appendChild(optsRow);

    // 目标部署目录(可选覆盖)
    var dirRow = el('div', 'form-row');
    dirRow.appendChild(window.formLabel('目标部署目录', 'REMOTE DIR', false, 'migrate-project-dir'));
    var dirInput = document.createElement('input');
    dirInput.className = 'form-input';  // 同 srcSel:补回缺失的控件样式
    dirInput.type = 'text';
    dirInput.id = 'migrate-project-dir';
    dirInput.placeholder = '留空则沿用项目/服务器配置的目录';
    dirRow.appendChild(dirInput);
    // 路径类字段补常驻说明(此前只有 placeholder,输入即消失)
    dirRow.appendChild(el('div', 'form-hint',
      '留空 = 沿用项目或服务器的部署目录;填了独立目录需在目标服务器上先建好'));
    grid.appendChild(dirRow);

    form.appendChild(grid);
    form.insertBefore(gridTitle, grid);

    // ---- 动作按钮 ----
    var actions = el('div', 'modal-actions');
    var previewBtn = el('button', 'btn btn-primary', '开始预检');
    previewBtn.id = 'migrate-project-preview-btn';
    previewBtn.type = 'button';
    previewBtn.addEventListener('click', onMigratePreview);
    actions.appendChild(previewBtn);

    var startBtn = el('button', 'btn btn-primary', '确认迁移');
    startBtn.id = 'migrate-project-start-btn';
    startBtn.type = 'button';
    startBtn.disabled = true;
    startBtn.title = '需先完成预检并解决阻断问题';
    startBtn.addEventListener('click', onMigrateStart);
    actions.appendChild(startBtn);
    form.appendChild(actions);

    // 表单语义 + 失焦校验 + Enter(第十三批):Enter 只走到**预检**(只读),
    // 「确认迁移」是执行型动作,必须点击,Enter 不越过它。
    var v = window.bindFieldValidation(form, [
      {
        id: 'migrate-project-dir',
        test: function (val) { return val === '' || val.indexOf('/') === 0; },
        message: '目标部署目录需为以 / 开头的绝对路径(如 /opt/myapp),或留空沿用配置'
      },
      {
        id: 'migrate-opt-releases',
        // 后端按 0-20 夹取,此前超界静默改值;改为提示。**不阻断**:归一
        // 仍在后端生效,提示只是让用户知道实际会用哪个值。
        blocking: false,
        test: function (val) {
          if (val === '') return true;
          return /^\d+$/.test(val) && Number(val) >= 0 && Number(val) <= 20;
        },
        message: '归档数量需为 0 - 20 之间的整数(0 = 不搬;超界将按边界值执行)'
      }
    ]);
    window.bindFormEnter(form, onMigratePreview);
    formApi.onSubmit(onMigratePreview);
    // 源下拉变化会重建目标下拉:重校验(避免残留「源与目标相同」类旧提示)
    srcSel.addEventListener('change', function () { v.checkField('migrate-project-target'); });

    // ---- 计划区 ----
    var planBox = el('div', 'migrate-plan hidden');
    planBox.id = 'migrate-project-plan';
    body.appendChild(planBox);

    // ---- 日志区(恒暗面板)----
    var logHead = el('div', 'migrate-log-head', '执行日志');
    body.appendChild(logHead);
    var log = el('pre', 'migrate-log-body');
    log.id = 'migrate-project-log';
    body.appendChild(log);

    setMigrateButtons('idle');
  }

  function onMigratePreview() {
    if (migState.previewing) return;
    var projectId = migVal('deploy-project');
    var srcId = migVal('migrate-project-source');
    var tgtId = migVal('migrate-project-target');
    if (!srcId || !tgtId) { window.toast('请选择源与目标服务器', 'warn'); return; }
    if (srcId === tgtId) { window.toast('源与目标服务器不能相同', 'warn'); return; }

    var volChk = document.getElementById('migrate-opt-volumes');
    var relInput = document.getElementById('migrate-opt-releases');
    var releaseCount = parseInt(relInput ? relInput.value : '1', 10);
    if (isNaN(releaseCount) || releaseCount < 0) releaseCount = 0;
    if (releaseCount > 20) releaseCount = 20;

    migState.previewing = true;
    var previewBtn = document.getElementById('migrate-project-preview-btn');
    // 第六批:预检要连服务器读 compose/卷/归档,是本页最长的一次等待,走共享
    // 助手带步进条(此前只是禁用+改文案)
    window.setBtnBusy(previewBtn, true, '预检中…');

    window.AppBus.invoke('migrate_project_preview', {
      projectId: projectId,
      sourceServerId: srcId,
      targetServerId: tgtId,
      includeVolumes: !!(volChk && volChk.checked),
      releaseCount: releaseCount
    }).then(function (plan) {
      migState.previewing = false;
      window.setBtnBusy(previewBtn, false, '重新预检');
      migState.plan = plan || null;
      renderMigratePlan(plan);
      setMigrateButtons('previewed');
    }).catch(function (err) {
      migState.previewing = false;
      window.setBtnBusy(previewBtn, false, '开始预检');
      migState.plan = null;
      renderMigratePlanError(errText(err) || '未知错误');
      setMigrateButtons('idle');
    });
  }

  /** 渲染预检计划(镜像/卷/归档清单 + 警告与阻断项) */
  function renderMigratePlan(plan) {
    var box = document.getElementById('migrate-project-plan');
    if (!box) return;
    box.innerHTML = '';
    box.classList.remove('hidden');
    if (!plan) return;

    var p = plan;
    var meta = el('div', 'migrate-plan-meta');
    meta.appendChild(el('div', '', '源:' + (p.sourceServerName || '') + ' — ' + (p.sourceRemoteDir || '')));
    meta.appendChild(el('div', '', '目标:' + (p.targetServerName || '') + ' — ' + (p.targetRemoteDir || '')));
    if (p.totalBytes !== null && p.totalBytes !== undefined) {
      meta.appendChild(el('div', '', '预计搬运总量:' + formatBytesLocal(p.totalBytes)));
    }
    box.appendChild(meta);

    // 阻断项(非空则不允许执行)
    if (Array.isArray(p.errors) && p.errors.length > 0) {
      var errBox = el('div', 'check-error');
      p.errors.forEach(function (e) { errBox.appendChild(el('div', '', '✗ ' + e)); });
      box.appendChild(errBox);
    }

    // 警告项
    if (Array.isArray(p.warnings) && p.warnings.length > 0) {
      var warnBox = el('div', 'migrate-plan-warnings');
      warnBox.appendChild(el('div', 'migrate-plan-label', '注意事项(' + p.warnings.length + ' 条)'));
      p.warnings.forEach(function (w) { warnBox.appendChild(el('div', '', '· ' + w)); });
      box.appendChild(warnBox);
    }

    // 镜像
    var imgs = Array.isArray(p.images) ? p.images : [];
    box.appendChild(el('div', 'migrate-plan-label', '镜像 ' + imgs.length + ' 个'));
    if (imgs.length === 0) {
      box.appendChild(el('div', 'migrate-plan-empty', '(无:compose 未声明 image,或解析失败)'));
    } else {
      var imgTable = buildPlanTable(['镜像', '状态']);
      imgs.forEach(function (it) {
        var status, cls;
        if (!it.existsOnSource) { status = '源上不存在'; cls = 'badge-fail'; }
        else if (it.alreadyOnTarget) { status = '目标已有,将跳过'; cls = 'badge-info'; }
        else { status = '待搬运'; cls = 'badge-ok'; }
        imgTable.appendChild(buildPlanRow([
          el('span', 'mono', it.reference || ''),
          badgeSpan(cls, status)
        ]));
      });
      box.appendChild(imgTable);
    }

    // 卷
    var vols = Array.isArray(p.volumes) ? p.volumes : [];
    if (vols.length > 0) {
      box.appendChild(el('div', 'migrate-plan-label', '数据卷 ' + vols.length + ' 项'));
      var volTable = buildPlanTable(['源', '目标', '体积']);
      vols.forEach(function (v) {
        volTable.appendChild(buildPlanRow([
          el('span', 'mono', v.sourceKey || ''),
          el('span', 'mono', (v.targetKey || '') + (v.renamed ? '(名称变化)' : '')),
          el('span', '', v.size || '?')
        ]));
      });
      box.appendChild(volTable);
    }

    // 归档
    var rels = Array.isArray(p.releases) ? p.releases : [];
    if (rels.length > 0) {
      box.appendChild(el('div', 'migrate-plan-label', '发布归档 ' + rels.length + ' 个'));
      var relTable = buildPlanTable(['时间戳', '服务', '体积']);
      rels.forEach(function (r) {
        relTable.appendChild(buildPlanRow([
          el('span', 'mono', r.ts || ''),
          el('span', '', (Array.isArray(r.services) && r.services.length > 0)
            ? r.services.join(', ') : '(无 manifest)'),
          el('span', '', r.size || '?')
        ]));
      });
      box.appendChild(relTable);
    }

    // 停机提示(恒显示,因为卷导出必然停服)
    var volChk = document.getElementById('migrate-opt-volumes');
    if (volChk && volChk.checked) {
      var note = el('div', 'migrate-plan-note',
        '执行时会先停止源服务器上的服务以导出数据卷(保证一致性),导出完成后立即恢复;' +
        '迁移完成后两台服务器会同时运行,建议尽快停用源服务器,否则两边数据会各自变化。');
      box.appendChild(note);
    }
  }

  function renderMigratePlanError(msg) {
    var box = document.getElementById('migrate-project-plan');
    if (!box) return;
    box.innerHTML = '';
    box.classList.remove('hidden');
    var errBox = el('div', 'check-error');
    errBox.appendChild(el('div', '', '预检失败:' + msg));
    box.appendChild(errBox);
  }

  function buildPlanTable(headers) {
    var table = el('table', 'migrate-plan-table');
    var thead = document.createElement('thead');
    var tr = document.createElement('tr');
    headers.forEach(function (h) {
      var th = document.createElement('th');
      th.textContent = h;
      tr.appendChild(th);
    });
    thead.appendChild(tr);
    table.appendChild(thead);
    var tbody = document.createElement('tbody');
    tbody.className = 'migrate-plan-rows';
    table.appendChild(tbody);
    return tbody;
  }

  function buildPlanRow(cells) {
    var tr = document.createElement('tr');
    cells.forEach(function (c) {
      var td = document.createElement('td');
      td.appendChild(c);
      tr.appendChild(td);
    });
    return tr;
  }

  function badgeSpan(cls, text) {
    var s = el('span', 'badge ' + cls, text);
    return s;
  }

  /** 与前端一致的人类可读体积(后端已给出 size 字符串,此处仅用于总量) */
  function formatBytesLocal(b) {
    var v = Number(b) || 0;
    var units = ['B', 'KB', 'MB', 'GB', 'TB'];
    var i = 0;
    while (v >= 1024 && i < units.length - 1) { v = v / 1024; i++; }
    return (i === 0 ? v : v.toFixed(1)) + ' ' + units[i];
  }

  function onMigrateStart() {
    if (migState.active) return;
    var plan = migState.plan;
    if (!plan) { window.toast('请先执行预检', 'warn'); return; }
    if (Array.isArray(plan.errors) && plan.errors.length > 0) {
      window.toast('存在阻断问题,无法执行迁移', 'fail');
      return;
    }
    var srcId = migVal('migrate-project-source');
    var tgtId = migVal('migrate-project-target');
    if (!srcId || !tgtId || srcId === tgtId) {
      window.toast('源与目标服务器无效', 'warn');
      return;
    }
    var volChk = document.getElementById('migrate-opt-volumes');
    var relInput = document.getElementById('migrate-opt-releases');
    var releaseCount = parseInt(relInput ? relInput.value : '1', 10);
    if (isNaN(releaseCount) || releaseCount < 0) releaseCount = 0;
    if (releaseCount > 20) releaseCount = 20;
    var dirInput = document.getElementById('migrate-project-dir');
    var targetDir = dirInput ? String(dirInput.value || '').trim() : '';

    migState.active = true;
    refreshControls();
    setMigrateButtons('running');
    appendMigrateProjectLine('—— 迁移已发起,请勿关闭窗口 ——');

    window.AppBus.invoke('migrate_project_start', {
      req: {
        projectId: migVal('deploy-project'),
        sourceServerId: srcId,
        targetServerId: tgtId,
        includeVolumes: !!(volChk && volChk.checked),
        releaseCount: releaseCount,
        targetRemoteDir: targetDir || null,
        sourcePasswordPlain: null,
        targetPasswordPlain: null
      }
    }).then(function () {
      // 同步返回不代表成功;结果只经 migrate-project-done
    }).catch(function (err) {
      migState.active = false;
      refreshControls();
      setMigrateButtons('failed');
      var msg = errText(err) || '未知错误';
      appendMigrateProjectLine('—— 迁移发起失败:' + msg + ' ——');
      window.toast('迁移发起失败:' + msg, 'fail');
    });
  }

  window.DeployMigrate = { openMigrateProjectModal: openMigrateProjectModal, closeMigrateProjectModal: closeMigrateProjectModal };
})();
