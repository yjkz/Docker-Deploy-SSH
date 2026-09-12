// deploy-rollback.js — 一键回滚模态(部署历史 ACTION 列;st.rb* 状态族,模态 busy 期禁关)
// 第十二批 JS 结构治理:自 deploy.js 拆出;外部依赖经
// window.DeployKit 桥接(定义见宿主文件尾部),入口经 window.DeployRollback 供宿主回调。
(function () {
  'use strict';
  var K = window.DeployKit;
  var st = K.st;
  var el = K.el;
  var findById = K.findById;
  var appendLogLine = K.appendLogLine;
  var DATE_TAG_RE = K.DATE_TAG_RE;
  var LOG_MAX_LINES = K.LOG_MAX_LINES;
  var refreshControls = K.refreshControls;
  var refreshHistory = K.refreshHistory;
  var renderHistory = K.renderHistory;
  var handleDone = K.handleDone;

  // ===== 一键回滚(部署历史 ACTION 列 → #deploy-modal;复用 deploy 事件体系)=====

  /**
   * 按部署历史记录解析目标 server/project 的 id(第四批改为优先用记录里的 id)。
   *
   * 旧记录只落名称、无 id(v5.4.x 之前),且项目/服务器**改名后按名反查即失配**
   * (回滚按钮变灰),因此新记录写入 server_id/project_id;这里优先按 id 精确
   * 匹配,命不中再回退按名匹配(兼容旧记录),两路都失败返回 null 由调用方提示。
   */
  function resolveRecordIds(rec) {
    var servers = (st.cfg && st.cfg.servers) || [];
    var projects = (st.cfg && st.cfg.projects) || [];
    var server = rec.server_id ? findById(servers, String(rec.server_id)) : null;
    var project = rec.project_id ? findById(projects, String(rec.project_id)) : null;
    var i;
    // 回退:按名称匹配(旧记录无 id;或 id 对应项已被删除时尽量沿用旧行为)
    if (!server) {
      for (i = 0; i < servers.length; i++) {
        if (servers[i] && String(servers[i].name) === String(rec.server_name)) {
          server = servers[i];
          break;
        }
      }
    }
    if (!project) {
      for (i = 0; i < projects.length; i++) {
        if (projects[i] && String(projects[i].name) === String(rec.project_name)) {
          project = projects[i];
          break;
        }
      }
    }
    if (!server || !project) return null;
    return { serverId: String(server.id), projectId: String(project.id) };
  }

  /** 拆镜像引用(取最后一个冒号前/后);无冒号时 tag 为空串 */
  function splitImageRef(ref) {
    var full = String(ref || '');
    var ci = full.lastIndexOf(':');
    if (ci <= 0) return { repository: full, tag: '' };
    return { repository: full.slice(0, ci), tag: full.slice(ci + 1) };
  }

  /** 发布目录内的镜像包数(.tar.gz;manifest.json / compose 副本不计入) */
  function rbPackageCount(files) {
    return (Array.isArray(files) ? files : []).filter(function (f) {
      return /\.tar\.gz$/.test(String(f));
    }).length;
  }

  function rbOverlay() { return document.getElementById('deploy-modal'); }
  function rbBody() { return document.getElementById('deploy-modal-body'); }

  function setRbCloseDisabled(disabled) {
    var btn = document.getElementById('deploy-modal-close');
    if (btn) btn.disabled = disabled;
  }

  /** 关闭回滚模态;执行中(rbBusy)禁止关闭(关闭钮/Esc/遮罩统一走此守卫) */
  function closeRollbackModal() {
    if (st.rbBusy) {
      window.toast('回滚执行中,暂不能关闭', 'warn');
      return;
    }
    var overlay = rbOverlay();
    if (overlay) {
      overlay.classList.add('hidden');
      window.modalFocusClose(overlay);
    }
    st.rbKind = '';
    st.rbRecord = null;
    st.rbIds = null;
    st.rbRepository = '';
    st.rbReleases = [];
    st.rbTags = [];
    var body = rbBody();
    if (body) body.textContent = '';
  }

  /** 加载中 / 空列表 / 读取失败的提示块(附「关闭」按钮) */
  function renderRbNotice(text) {
    var body = rbBody();
    if (!body) return;
    body.textContent = '';
    if (text) body.appendChild(el('div', 'rb-empty', text));
    var actions = el('div', 'modal-actions');
    var btn = el('button', 'btn', '关闭');
    btn.type = 'button';
    btn.addEventListener('click', function () { closeRollbackModal(); });
    actions.appendChild(btn);
    body.appendChild(actions);
  }

  /** 打开回滚模态:按记录模式决定列表类型;部署/预检/回滚期间禁止打开 */
  function openRollbackModal(rec) {
    if (st.deploying || st.checking || st.rbBusy) {
      window.toast('部署进行中,请等待结束后再试', 'warn');
      return;
    }
    var r = rec || {};
    var kind = String(r.mode) === 'stack' ? 'stack' : 'single';
    var ids = resolveRecordIds(r);
    if (!ids) {
      window.toast('无法定位该记录对应的服务器/项目(配置可能已变更),请刷新页面后重试', 'warn');
      return;
    }

    st.rbKind = kind;
    st.rbRecord = r;
    st.rbIds = ids;
    st.rbBusy = false;
    st.rbLogs = [];
    st.rbReleases = [];
    st.rbTags = [];

    var overlay = rbOverlay();
    if (!overlay) return;
    overlay.classList.remove('hidden');
    window.modalFocusOpen(overlay);
    setRbCloseDisabled(false);

    var title = document.getElementById('deploy-modal-title');
    if (title) {
      title.textContent = (kind === 'stack' ? '整栈回滚 — ' : '镜像回滚 — ') +
        String(r.project_name || '') + ' @ ' + String(r.server_name || '');
    }

    renderRbNotice('正在读取历史' + (kind === 'stack' ? '发布' : '标签') + '…');
    if (kind === 'stack') fetchRbReleases(); else fetchRbTags();
  }

  /** 拉取历史发布列表(rollback_list_releases,新 → 旧) */
  function fetchRbReleases() {
    var ids = st.rbIds || {};
    window.AppBus.invoke('rollback_list_releases',
        { serverId: ids.serverId, projectId: ids.projectId })
      .then(function (list) {
        if (st.rbKind !== 'stack') return; // 模态已关闭,丢弃过期结果
        st.rbReleases = Array.isArray(list) ? list : [];
        renderRbStackList();
      })
      .catch(function (err) {
        if (st.rbKind !== 'stack') return;
        renderRbNotice('读取历史发布失败:' + (errText(err) || '未知错误'));
      });
  }

  /** 拉取仓库历史标签(rollback_list_tags,创建时间倒序) */
  function fetchRbTags() {
    var ids = st.rbIds || {};
    var rec = st.rbRecord || {};
    var first = (Array.isArray(rec.images) && rec.images.length) ? String(rec.images[0]) : '';
    var repository = splitImageRef(first).repository;
    if (!repository) {
      renderRbNotice('该记录没有可识别的镜像引用,无法回滚');
      return;
    }
    st.rbRepository = repository;
    window.AppBus.invoke('rollback_list_tags',
        { serverId: ids.serverId, projectId: ids.projectId, repository: repository })
      .then(function (list) {
        if (st.rbKind !== 'single') return;
        st.rbTags = Array.isArray(list) ? list : [];
        renderRbSingleList();
      })
      .catch(function (err) {
        if (st.rbKind !== 'single') return;
        renderRbNotice('读取历史标签失败:' + (errText(err) || '未知错误'));
      });
  }

  /** 表头行构造(选择 / 数据列按列表类型给定) */
  function rbHeadRow(titles) {
    var tr = document.createElement('tr');
    titles.forEach(function (text) {
      var th = document.createElement('th');
      th.setAttribute('scope', 'col');
      th.textContent = text;
      tr.appendChild(th);
    });
    return tr;
  }

  /** 整栈回滚视图:发布单选列表(新 → 旧)+ 动作说明 + 执行按钮 */
  function renderRbStackList() {
    var body = rbBody();
    if (!body) return;
    if (st.rbReleases.length === 0) {
      renderRbNotice('服务器上没有可用的历史留档(从未整栈部署成功,或留档已被清理),无法回滚');
      return;
    }
    body.textContent = '';

    body.appendChild(el('div', 'rb-list-hint', '选择要回滚到的历史发布(新 → 旧):'));

    var wrap = el('div', 'table-wrap');
    var table = el('table', 'data-table');
    var thead = el('thead');
    thead.appendChild(rbHeadRow(['选择', '发布 TS', '内容 CONTENT']));
    table.appendChild(thead);

    var tbody = document.createElement('tbody');
    st.rbReleases.forEach(function (rel, idx) {
      var r = rel || {};
      var tr = document.createElement('tr');

      var tdPick = document.createElement('td');
      var radio = document.createElement('input');
      radio.type = 'radio';
      radio.name = 'rb-release';
      radio.value = String(idx);
      radio.addEventListener('change', updateRbStackPlan);
      tdPick.appendChild(radio);
      tr.appendChild(tdPick);

      var tdTs = document.createElement('td');
      tdTs.className = 'mono nowrap';
      tdTs.textContent = String(r.ts || '');
      tr.appendChild(tdTs);

      // 内容列:有清单 → 服务数;无清单 → 「旧版留档(无清单,按包恢复)」+ 包数
      var tdContent = document.createElement('td');
      var pkgs = rbPackageCount(r.files);
      var services = Array.isArray(r.services) ? r.services : [];
      if (r.hasManifest) {
        var badge = window.fillBadge(el('span'), 'info', '有清单');
        badge.title = '清单服务:' + (services.map(String).join('、') || '(空)');
        tdContent.appendChild(badge);
        tdContent.appendChild(document.createTextNode(' ' + services.length + ' 个服务'));
      } else {
        tdContent.appendChild(window.fillBadge(el('span'), 'warn', '旧版留档(无清单,按包恢复)'));
        tdContent.appendChild(document.createTextNode(' ' + pkgs + ' 个镜像包'));
      }
      tr.appendChild(tdContent);

      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
    wrap.appendChild(table);
    body.appendChild(wrap);

    // 动作说明(选中发布后填充)+ 执行区
    var plan = el('div', 'rb-plan hidden');
    plan.id = 'rb-plan';
    body.appendChild(plan);

    var actions = el('div', 'modal-actions');
    var cancel = el('button', 'btn', '取消');
    cancel.type = 'button';
    cancel.addEventListener('click', function () { closeRollbackModal(); });
    var exec = el('button', 'btn btn-danger', '执行回滚');
    exec.type = 'button';
    exec.id = 'rb-exec-btn';
    exec.disabled = true;
    exec.addEventListener('click', confirmRbStack);
    actions.appendChild(cancel);
    actions.appendChild(exec);
    body.appendChild(actions);
  }

  /** 当前选中的历史发布(未选中返回 null) */
  function selectedRbRelease() {
    var radios = document.querySelectorAll('input[name="rb-release"]');
    for (var i = 0; i < radios.length; i++) {
      if (radios[i].checked) return st.rbReleases[Number(radios[i].value)] || null;
    }
    return null;
  }

  /** 整栈:选中发布后填充动作说明并启用执行按钮 */
  function updateRbStackPlan() {
    var rel = selectedRbRelease();
    var plan = document.getElementById('rb-plan');
    var exec = document.getElementById('rb-exec-btn');
    if (!plan || !exec) return;
    if (!rel) {
      plan.classList.add('hidden');
      plan.textContent = '';
      exec.disabled = true;
      return;
    }
    plan.textContent = '';
    plan.appendChild(el('div', null,
      '加载 ' + rbPackageCount(rel.files) + ' 个镜像包 → 恢复 compose 副本(如有)→ compose up 重建全部服务'));
    plan.appendChild(el('div', 'rb-plan-sub',
      rel.hasManifest
        ? '该发布含清单,记录 ' + (Array.isArray(rel.services) ? rel.services.length : 0) + ' 个服务'
        : '该发布无清单,按镜像包恢复(docker load 自动恢复镜像原标签)'));
    if (!rel.hasComposeCopy) {
      plan.appendChild(el('div', 'rb-plan-sub', '发布目录无 compose 副本,将沿用服务器现有 compose 文件'));
    }
    plan.classList.remove('hidden');
    exec.disabled = false;
  }

  /**
   * 单镜像目标引用默认值:未打日期标签的记录 images[0] 即原始引用,原样使用;
   * 打过日期标签的记录(images[0] = repository:YYYYmmdd-HHMMSS)原始标签不在
   * 记录里,取服务器上最新的非日期标签兜底(部署时会同步原标签到服务器);
   * 全是日期标签时退回 images[0] 自身(标签存在,仍可指回)。
   */
  function rbDefaultTarget() {
    var rec = st.rbRecord || {};
    var first = (Array.isArray(rec.images) && rec.images.length) ? String(rec.images[0]) : '';
    var ref = splitImageRef(first);
    if (!DATE_TAG_RE.test(ref.tag)) return first;
    for (var i = 0; i < st.rbTags.length; i++) {
      var tag = st.rbTags[i] && st.rbTags[i].tag ? String(st.rbTags[i].tag) : '';
      if (tag && !DATE_TAG_RE.test(tag)) return ref.repository + ':' + tag;
    }
    return first;
  }

  /** 单镜像回滚视图:日期标签单选列表 + 目标引用输入框 + 动作说明 + 执行按钮 */
  function renderRbSingleList() {
    var body = rbBody();
    if (!body) return;
    if (st.rbTags.length === 0) {
      renderRbNotice('服务器上没有镜像仓库「' + (st.rbRepository || '') + '」的历史标签,无法回滚');
      return;
    }
    body.textContent = '';

    body.appendChild(el('div', 'rb-list-hint', '选择要回滚到的历史日期标签(按创建时间倒序):'));

    var wrap = el('div', 'table-wrap');
    var table = el('table', 'data-table');
    var thead = el('thead');
    thead.appendChild(rbHeadRow(['选择', '标签 TAG', '镜像 ID', '创建时间 CREATED']));
    table.appendChild(thead);

    var tbody = document.createElement('tbody');
    st.rbTags.forEach(function (t) {
      var tag = t || {};
      var tr = document.createElement('tr');

      var tdPick = document.createElement('td');
      var radio = document.createElement('input');
      radio.type = 'radio';
      radio.name = 'rb-date-tag';
      radio.value = String(tag.tag || '');
      radio.addEventListener('change', updateRbSinglePlan);
      tdPick.appendChild(radio);
      tr.appendChild(tdPick);

      var tdTag = document.createElement('td');
      tdTag.className = 'mono';
      tdTag.textContent = String(tag.tag || '');
      tr.appendChild(tdTag);

      var tdId = document.createElement('td');
      tdId.className = 'mono';
      var fullId = String(tag.id || '');
      // 短 ID 展示(去 sha256: 前缀取 12 位),完整 ID 放 title
      var shortId = fullId.replace(/^sha256:/, '').slice(0, 12);
      tdId.textContent = shortId || '-';
      if (fullId) tdId.title = fullId;
      tr.appendChild(tdId);

      var tdCreated = document.createElement('td');
      tdCreated.className = 'mono';
      tdCreated.textContent = String(tag.created || '');
      tr.appendChild(tdCreated);

      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
    wrap.appendChild(table);
    body.appendChild(wrap);

    // 目标引用输入框:默认原始引用,可编辑
    var row = el('div', 'form-row');
    var label = el('label', 'form-label', '目标引用 TARGET REF');
    label.setAttribute('for', 'rb-target-input');
    row.appendChild(label);
    var input = document.createElement('input');
    input.className = 'form-input';
    input.id = 'rb-target-input';
    input.type = 'text';
    input.value = rbDefaultTarget();
    input.autocomplete = 'off';
    input.addEventListener('input', updateRbSinglePlan);
    row.appendChild(input);
    row.appendChild(el('div', 'form-hint',
      '回滚会把所选日期标签重新指向此引用(通常为 compose 引用的原始标签),可编辑'));
    body.appendChild(row);

    var plan = el('div', 'rb-plan hidden');
    plan.id = 'rb-plan';
    body.appendChild(plan);

    var actions = el('div', 'modal-actions');
    var cancel = el('button', 'btn', '取消');
    cancel.type = 'button';
    cancel.addEventListener('click', function () { closeRollbackModal(); });
    var exec = el('button', 'btn btn-danger', '执行回滚');
    exec.type = 'button';
    exec.id = 'rb-exec-btn';
    exec.disabled = true;
    exec.addEventListener('click', confirmRbSingle);
    actions.appendChild(cancel);
    actions.appendChild(exec);
    body.appendChild(actions);
  }

  /** 当前选中的日期标签(未选中返回空串) */
  function selectedRbDateTag() {
    var radios = document.querySelectorAll('input[name="rb-date-tag"]');
    for (var i = 0; i < radios.length; i++) {
      if (radios[i].checked) return String(radios[i].value);
    }
    return '';
  }

  function rbTargetInputValue() {
    var input = document.getElementById('rb-target-input');
    return input ? String(input.value).trim() : '';
  }

  /** 单镜像:选中标签后填充动作说明并启用执行按钮(目标引用实时读取) */
  function updateRbSinglePlan() {
    var dateTag = selectedRbDateTag();
    var plan = document.getElementById('rb-plan');
    var exec = document.getElementById('rb-exec-btn');
    if (!plan || !exec) return;
    if (!dateTag) {
      plan.classList.add('hidden');
      plan.textContent = '';
      exec.disabled = true;
      return;
    }
    var target = rbTargetInputValue();
    plan.textContent = '将把服务器上的 ' + (st.rbRepository || '') + ':' + dateTag +
      ' 重新指到「' + (target || '(未填写)') + '」并 compose up';
    plan.classList.remove('hidden');
    exec.disabled = false;
  }

  /** 模态内二次确认视图:确认执行后进入执行视图,取消返回列表视图 */
  function renderRbConfirm(block, onConfirm, onCancel) {
    var body = rbBody();
    if (!body) return;
    body.textContent = '';
    body.appendChild(window.confirmBlock(block));
    var actions = el('div', 'modal-actions');
    var cancel = el('button', 'btn', '取消');
    cancel.type = 'button';
    cancel.addEventListener('click', onCancel);
    var exec = el('button', 'btn btn-danger', '确认执行');
    exec.type = 'button';
    exec.addEventListener('click', onConfirm);
    actions.appendChild(cancel);
    actions.appendChild(exec);
    body.appendChild(actions);
  }

  /** 整栈执行前二次确认(确认后才真正发起 rollback_execute_stack) */
  function confirmRbStack() {
    var rel = selectedRbRelease();
    if (!rel || st.rbBusy) return;
    var rec = st.rbRecord || {};
    renderRbConfirm({
      title: '确认把项目「' + String(rec.project_name || '') + '」@ 服务器「' +
        String(rec.server_name || '') + '」回滚到发布 ' + String(rel.ts || '') + '?',
      facts: [
        ['镜像包数', rbPackageCount(rel.files) + ' 个'],
        ['服务清单', rel.hasManifest && Array.isArray(rel.services) && rel.services.length
          ? rel.services.join('、') : '无清单记录,按镜像包恢复(docker load 自动恢复镜像原标签)'],
        ['恢复方式', rel.hasComposeCopy ? '恢复 compose 副本后 compose up 重建' : '沿用服务器现有 compose 文件重建']
      ],
      risk: '期间服务会短暂重启;目标侧容器将被该归档版本重建。'
    }, function () {
      beginRbExecution('rollback_execute_stack', {
        serverId: st.rbIds.serverId,
        projectId: st.rbIds.projectId,
        releaseTs: String(rel.ts || '')
      });
    }, renderRbStackList);
  }

  /** 单镜像执行前二次确认(确认后才真正发起 rollback_execute_single) */
  function confirmRbSingle() {
    var dateTag = selectedRbDateTag();
    var target = rbTargetInputValue();
    if (!dateTag || st.rbBusy) return;
    if (!target || target.indexOf(':') < 0) {
      window.toast('目标引用需为完整镜像引用(如 myapp:latest)', 'warn');
      return;
    }
    var rec = st.rbRecord || {};
    renderRbConfirm({
      title: '确认把服务器「' + String(rec.server_name || '') + '」上的 ' +
        (st.rbRepository || '') + ':' + dateTag + ' 重新指到「' + target + '」?',
      facts: [
        ['操作方式', '零拷贝 docker tag(不重新传输镜像)'],
        ['重建范围', 'compose up 重建引用该标签的服务']
      ],
      risk: '期间服务会短暂重启。'
    }, function () {
      beginRbExecution('rollback_execute_single', {
        serverId: st.rbIds.serverId,
        projectId: st.rbIds.projectId,
        repository: st.rbRepository,
        dateTag: dateTag,
        targetRef: target
      });
    }, renderRbSingleList);
  }

  /**
   * 进入执行视图:复用 st.deploying 互斥(主页面「开始部署」同步禁用、
   * 「取消部署」可用),模态内日志区镜像 deploy-log,等待 deploy-done 收尾。
   */
  function beginRbExecution(invokeName, args) {
    // 二次防线:确认到执行之间页面状态可能变化(预检/部署被键盘等途径触发)
    if (st.deploying || st.checking || st.rbBusy) {
      window.toast('部署进行中,无法执行回滚', 'warn');
      return;
    }
    st.deploying = true;
    st.rbBusy = true;
    st.rbLogs = [];
    refreshControls();
    setRbCloseDisabled(true); // 执行中禁止关闭模态(关闭钮/Esc/遮罩三处同步拦截)

    var body = rbBody();
    if (body) {
      body.textContent = '';
      body.appendChild(el('div', 'rb-list-hint', '回滚执行中,请勿关闭窗口:'));
      var log = el('pre', 'deploy-log deploy-modal-log');
      log.id = 'rb-modal-log';
      body.appendChild(log);
      var status = el('div', 'rb-status', '执行中…');
      status.id = 'rb-modal-status';
      body.appendChild(status);
    }
    renderRbLog();

    window.AppBus.invoke(invokeName, args)
      .catch(function (err) {
        // invoke 级失败(参数/连接异常):deploy-done 可能不到来,就地解锁收尾
        if (!st.rbBusy) return; // deploy-done 已先行收尾
        st.rbBusy = false;
        st.deploying = false;
        refreshControls();
        setRbCloseDisabled(false);
        var status = document.getElementById('rb-modal-status');
        if (status) status.textContent = '回滚发起失败:' + (errText(err) || '未知错误');
        window.toast('回滚发起失败:' + (errText(err) || '未知错误'), 'fail');
      });
  }

  /** 模态内日志区渲染(回滚执行期由 appendRbLog 增量驱动) */
  function renderRbLog() {
    var body = document.getElementById('rb-modal-log');
    if (!body) return;
    body.textContent = st.rbLogs.length > 0 ? st.rbLogs.join('\n') : '(暂无日志)';
    body.scrollTop = body.scrollHeight;
  }

  /** deploy-log 镜像到回滚模态日志区(仅执行期;主日志 appendLogLine 不受影响) */
  function appendRbLog(line) {
    if (!st.rbBusy) return;
    st.rbLogs.push(line === null || line === undefined ? '' : String(line));
    if (st.rbLogs.length > LOG_MAX_LINES) {
      st.rbLogs.splice(0, st.rbLogs.length - LOG_MAX_LINES);
    }
    renderRbLog();
  }

  /**
   * 局部恢复历史表「回滚」按钮可用态:回滚执行期(renderHistory 在 rbBusy
   * 生效中触发过)渲染的按钮 disabled 已固化,收尾清标志后就地同步解锁,
   * 不必等 refreshHistory 异步重渲染(disabled 口径与 renderHistory 一致)。
   */
  function unlockHistoryRollbackButtons() {
    var tbody = document.getElementById('deploy-history-tbody');
    if (!tbody) return;
    var buttons = tbody.querySelectorAll('button');
    for (var i = 0; i < buttons.length; i++) {
      buttons[i].disabled = st.deploying || st.rbBusy;
    }
  }

  /** deploy-done(回滚发起期间):模态内展示结果并解锁关闭;互斥/历史由 handleDone 收尾 */
  function handleRollbackDone(payload) {
    if (st.rbKind === '') return; // 普通部署结束,回滚模态未参与
    var p = payload || {};
    st.rbBusy = false;
    refreshControls();
    setRbCloseDisabled(false);
    unlockHistoryRollbackButtons(); // rbBusy 期间渲染的历史「回滚」按钮就地恢复可用
    var status = document.getElementById('rb-modal-status');
    if (status) {
      status.textContent = p.success === true
        ? '回滚完成,可关闭本窗口'
        : '回滚失败:' + (p.message ? String(p.message) : '未知错误') + '(详见上方日志)';
    }
  }

  window.DeployRollback = { openRollbackModal: openRollbackModal, closeRollbackModal: closeRollbackModal, appendRbLog: appendRbLog, handleRollbackDone: handleRollbackDone };
})();
