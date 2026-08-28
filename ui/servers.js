/* ============================================================
 * servers.js — 服务器管理页逻辑(依赖 app.js 提供的全局工具)
 *
 * 后端命令(对象字段为 Rust snake_case 原样序列化,JS 参数名 camelCase):
 * - get_config() -> AppConfig { servers: Server[], projects: Project[] }
 *   Server  = { id, name, host, port, username, remote_dir,
 *               auth: { auth_type: "Key"|"Password", key_path, password_enc } }
 *   Project = { id, name, image_filter, compose_file,
 *               file_mappings: [{ local, remote, is_dir }] }
 * - save_config_cmd({ cfg })                全量保存配置
 * - encrypt_password({ plain }) -> string   base64 密文,存 auth.password_enc
 * - test_server / server_env_check({ serverId, passwordPlain? })
 *     -> ServerCheckReport { docker, compose, gzip, remote_dir_exists,
 *                            disk_free_gb, errors: string[] }
 *     (两者等价:连接 + 远端环境检测;passwordPlain 一般不传,后端用 DPAPI
 *      解密已存密文;auth_type=Key 时后端忽略 passwordPlain)
 * - install_server_docker({ serverId })     过程输出经 'server-log' 事件逐行推送
 * - create_remote_dir({ serverId })
 *
 * 页面进入时机:app.js 的 showPage() 成功切换页面后会在 window 上派发
 * 'pagechange'(detail.page = 页面名),本文件在首次进入服务器页时加载配置。
 *
 * 安全说明:所有来自配置/后端的数据一律 createElement + textContent 渲染,
 * 不使用 innerHTML 拼接;密码输入框 type=password 且不回显明文。
 * ============================================================ */
(function () {
  'use strict';

  /** 远端磁盘剩余空间低于该值(GB)时磁盘徽章显示警告 */
  var DISK_MIN_GB = 2;
  /** 运行日志最多保留的行数(超出丢弃最早的) */
  var LOG_MAX_LINES = 500;

  var st = {
    cfg: null,            // get_config 的完整结果(AppConfig)
    loaded: false,        // 是否已成功加载过配置
    loading: false,       // 配置加载中(防重复请求)
    checks: {},           // serverId -> ServerCheckReport(卡片内嵌检测结果)
    checking: {},         // serverId -> true(检测进行中,防重复触发)
    installing: {},       // serverId -> true(Docker 安装进行中)
    creating: {},         // serverId -> true(创建远程目录进行中)
    logs: []              // server-log 事件累积的输出行
  };

  /** server-log 事件监听守卫:只注册一次,防止重复绑定 */
  var logListenerBound = false;

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

  /** 生成服务器/项目 id:优先 crypto.randomUUID,不可用时退化为时间戳+随机串 */
  function uuid() {
    if (window.crypto && typeof window.crypto.randomUUID === 'function') {
      return window.crypto.randomUUID();
    }
    return 'id-' + Date.now().toString(36) + '-' + Math.random().toString(36).slice(2, 10);
  }

  /** 按 id 取输入框/文本域的当前值(trim 后);节点不存在返回空串 */
  function fieldVal(id) {
    var node = document.getElementById(id);
    return node ? String(node.value).trim() : '';
  }

  function normalizeCfg(cfg) {
    var out = (cfg && typeof cfg === 'object') ? cfg : {};
    if (!Array.isArray(out.servers)) out.servers = [];
    if (!Array.isArray(out.projects)) out.projects = [];
    return out;
  }

  function findServer(id) {
    if (!st.cfg) return null;
    for (var i = 0; i < st.cfg.servers.length; i++) {
      if (st.cfg.servers[i].id === id) return st.cfg.servers[i];
    }
    return null;
  }

  function findProject(id) {
    if (!st.cfg) return null;
    for (var i = 0; i < st.cfg.projects.length; i++) {
      if (st.cfg.projects[i].id === id) return st.cfg.projects[i];
    }
    return null;
  }

  /** 表单内联错误提示(自绘,不用 alert) */
  function formFail(errId, msg) {
    var box = document.getElementById(errId);
    if (box) {
      box.textContent = msg;
      box.classList.remove('hidden');
    }
    return false;
  }

  function formClearError(errId) {
    var box = document.getElementById(errId);
    if (box) {
      box.textContent = '';
      box.classList.add('hidden');
    }
  }

  /** 顶部横幅:本机 hostOk=false 时显示黄色提示(但仍允许编辑配置) */
  function refreshBanner() {
    var banner = document.getElementById('servers-banner');
    if (!banner) return;
    if (window.AppState.hostOk) {
      banner.classList.add('hidden');
    } else {
      banner.textContent = '环境检测未通过,SSH 功能可能不可用(配置编辑不受影响)';
      banner.classList.remove('hidden');
    }
  }

  // ===== 配置加载与错误框 =====

  function showError(msg) {
    var box = document.getElementById('servers-error');
    if (!box) return;
    box.textContent = '';
    box.appendChild(el('span', 'servers-error-text', msg || '读取配置失败'));
    var retry = el('button', 'btn', '重试');
    retry.type = 'button';
    retry.addEventListener('click', function () { loadConfig(); });
    box.appendChild(retry);
    box.classList.remove('hidden');
  }

  function hideError() {
    var box = document.getElementById('servers-error');
    if (box) {
      box.textContent = '';
      box.classList.add('hidden');
    }
  }

  function loadConfig() {
    if (st.loading) return Promise.resolve(null);
    st.loading = true;
    hideError();

    return window.AppBus.invoke('get_config')
      .then(function (cfg) {
        st.cfg = normalizeCfg(cfg);
        st.loaded = true;
      })
      .catch(function (err) {
        st.loaded = false;
        st.cfg = normalizeCfg(null);
        showError(errText(err) || '读取配置失败');
      })
      .then(function () {
        st.loading = false;
        renderServers();
        renderProjects();
      });
  }

  /** 全量保存兜底流程已由各保存函数实现;此处仅用于统一错误文案 */
  function saveToastFail(err, fallback) {
    window.toast(errText(err) || fallback, 'fail');
  }

  // ===== 服务器列表渲染 =====

  function renderServers() {
    var list = document.getElementById('servers-list');
    if (!list) return;
    list.textContent = '';

    if (!st.loaded) {
      list.appendChild(el('div', 'list-hint', st.cfg && st.cfg.servers.length === 0 && !st.loading
        ? '配置加载失败,请点击上方重试'
        : '正在加载配置…'));
      return;
    }
    if (st.cfg.servers.length === 0) {
      list.appendChild(el('div', 'list-hint', '暂无服务器,点击右上角「新增服务器」添加'));
      return;
    }
    st.cfg.servers.forEach(function (server) {
      list.appendChild(serverCard(server));
    });
  }

  function serverCard(server) {
    var auth = server.auth || {};
    var isPassword = auth.auth_type === 'Password';
    var checking = !!st.checking[server.id];

    var card = el('div', 'server-card');

    // 头部:名称 + host:port + 认证徽章 + 操作按钮
    var head = el('div', 'server-head');
    head.appendChild(el('span', 'server-name', server.name));
    head.appendChild(el('span', 'server-addr', server.host + ':' + server.port));
    head.appendChild(el('span', 'badge badge-info', isPassword ? '密码' : '私钥'));

    var actions = el('div', 'server-actions');

    var testBtn = el('button', 'btn btn-sm', checking ? '检测中…' : '测试连接');
    testBtn.type = 'button';
    testBtn.disabled = checking;
    testBtn.addEventListener('click', function () { runEnvCheck(server, 'test'); });
    actions.appendChild(testBtn);

    var envBtn = el('button', 'btn btn-sm', checking ? '检测中…' : '环境检测');
    envBtn.type = 'button';
    envBtn.disabled = checking;
    envBtn.addEventListener('click', function () { runEnvCheck(server, 'env'); });
    actions.appendChild(envBtn);

    var editBtn = el('button', 'btn btn-sm', '编辑');
    editBtn.type = 'button';
    editBtn.addEventListener('click', function () {
      var current = findServer(server.id);
      openServerModal(current);
    });
    actions.appendChild(editBtn);

    var delBtn = el('button', 'btn btn-sm', '删除');
    delBtn.type = 'button';
    delBtn.addEventListener('click', function () {
      armDeleteConfirm(delBtn, function () { deleteServerById(server.id); });
    });
    actions.appendChild(delBtn);

    head.appendChild(actions);
    card.appendChild(head);

    // 元信息:用户名 + 远程目录
    var meta = el('div', 'server-meta');
    meta.appendChild(el('span', '', '用户:' + server.username));
    meta.appendChild(el('span', '', '远程目录:' + server.remote_dir));
    card.appendChild(meta);

    // 内嵌环境检测结果区
    card.appendChild(checkSection(server));
    return card;
  }

  /** 卡片内嵌检测结果区:徽章行 + 红字错误列表 + 未通过项操作 */
  function checkSection(server) {
    var box = el('div', 'server-check');

    if (st.checking[server.id]) {
      box.appendChild(el('div', 'server-check-hint', '正在连接并检测远端环境…'));
      return box;
    }
    var report = st.checks[server.id];
    if (!report) {
      box.appendChild(el('div', 'server-check-hint', '尚未检测,点击「测试连接」或「环境检测」查看远端环境'));
      return box;
    }

    box.appendChild(el('div', 'server-check-title', '环境检测结果'));

    var badges = el('div', 'server-check-badges');
    badges.appendChild(badgeBool('Docker', report.docker));
    badges.appendChild(badgeBool('Compose', report.compose));
    badges.appendChild(badgeBool('gzip', report.gzip));
    badges.appendChild(badgeBool('远程目录', report.remote_dir_exists));

    var disk = Number(report.disk_free_gb);
    var diskText = isFinite(disk) ? '磁盘 ' + disk.toFixed(1) + ' GB' : '磁盘未知';
    var diskKind = (isFinite(disk) && disk >= DISK_MIN_GB) ? 'ok' : 'warn';
    badges.appendChild(el('span', 'badge badge-' + diskKind, diskText));
    box.appendChild(badges);

    var errors = Array.isArray(report.errors) ? report.errors : [];
    if (errors.length > 0) {
      var errBox = el('div', 'server-errors');
      errors.forEach(function (line) {
        errBox.appendChild(el('div', 'server-error-line', line));
      });
      box.appendChild(errBox);
    }

    // 未通过项对应操作
    var act = el('div', 'server-check-actions');
    var hasAction = false;

    if (!report.docker) {
      hasAction = true;
      var installBtn = el('button', 'btn btn-primary btn-sm',
        st.installing[server.id] ? '安装中…' : '一键安装 Docker');
      installBtn.type = 'button';
      installBtn.disabled = !!st.installing[server.id];
      installBtn.addEventListener('click', function () {
        showInstallConfirm(act, server);
      });
      act.appendChild(installBtn);
    }
    if (!report.remote_dir_exists) {
      hasAction = true;
      var mkdirBtn = el('button', 'btn btn-sm',
        st.creating[server.id] ? '创建中…' : '创建远程目录');
      mkdirBtn.type = 'button';
      mkdirBtn.disabled = !!st.creating[server.id];
      mkdirBtn.addEventListener('click', function () { createRemoteDir(server); });
      act.appendChild(mkdirBtn);
    }
    if (hasAction) box.appendChild(act);
    return box;
  }

  function badgeBool(label, ok) {
    return el('span', 'badge ' + (ok ? 'badge-ok' : 'badge-fail'),
      label + ':' + (ok ? '通过' : '未通过'));
  }

  /** 自绘确认条(不用 confirm()):文案 + 确认/取消 内联按钮 */
  function showInstallConfirm(container, server) {
    container.textContent = '';
    container.appendChild(el('span', 'confirm-text',
      '将通过 SSH 执行官方安装脚本,可能需要数分钟,确认?'));
    var ok = el('button', 'btn btn-danger btn-sm', '确认安装');
    ok.type = 'button';
    ok.addEventListener('click', function () { startInstallDocker(server); });
    container.appendChild(ok);
    var cancel = el('button', 'btn btn-sm', '取消');
    cancel.type = 'button';
    cancel.addEventListener('click', function () { renderServers(); });
    container.appendChild(cancel);
  }

  // ===== 远端操作(test / env check / install / mkdir)=====

  /**
   * 连接 + 环境检测。
   * @param {Object} server 服务器对象(仅使用其 id)
   * @param {string} mode 'test' = test_server;'env' = server_env_check(两者等价)
   */
  function runEnvCheck(server, mode) {
    var id = server.id;
    if (st.checking[id] || st.installing[id]) return;
    st.checking[id] = true;
    renderServers();

    var cmd = mode === 'test' ? 'test_server' : 'server_env_check';
    // passwordPlain 不传:后端用 DPAPI 解密已存密码;Key 认证时后端忽略该参数
    window.AppBus.invoke(cmd, { serverId: id })
      .then(function (report) {
        st.checks[id] = report || {};
        var errors = report && Array.isArray(report.errors) ? report.errors : [];
        if (errors.length > 0) {
          window.toast('检测完成,部分项未通过,详见卡片', 'warn');
        } else {
          window.toast(mode === 'test' ? '连接成功,环境正常' : '环境检测通过', 'ok');
        }
      })
      .catch(function (err) {
        window.toast((mode === 'test' ? '连接失败:' : '检测失败:') +
          (errText(err) || '未知错误'), 'fail');
      })
      .then(function () {
        st.checking[id] = false;
        renderServers();
      });
  }

  /** 一键安装 Docker(确认条点击「确认安装」后调用),输出写入底部运行日志 */
  function startInstallDocker(server) {
    var id = server.id;
    if (st.installing[id] || st.checking[id]) return;
    st.installing[id] = true;
    setLogOpen(true); // 自动展开日志面板,便于观察安装输出
    window.toast('开始安装 Docker,过程输出见底部「运行日志」', 'info');
    renderServers();

    window.AppBus.invoke('install_server_docker', { serverId: id })
      .then(function () {
        window.toast('Docker 安装完成', 'ok');
      })
      .catch(function (err) {
        saveToastFail(err, '安装 Docker 失败');
      })
      .then(function () {
        st.installing[id] = false;
        // 安装结束后自动复检一次,刷新徽章(quiet:结果由徽章呈现,不再弹 toast)
        runEnvCheckQuiet(id);
      });
  }

  /** 静默复检:不弹 toast,仅刷新卡片徽章 */
  function runEnvCheckQuiet(id) {
    if (st.checking[id]) return;
    st.checking[id] = true;
    renderServers();
    window.AppBus.invoke('server_env_check', { serverId: id })
      .then(function (report) {
        st.checks[id] = report || {};
      })
      .catch(function (err) {
        window.toast('复检失败:' + (errText(err) || '未知错误'), 'fail');
      })
      .then(function () {
        st.checking[id] = false;
        renderServers();
      });
  }

  function createRemoteDir(server) {
    var id = server.id;
    if (st.creating[id] || st.checking[id] || st.installing[id]) return;
    st.creating[id] = true;
    renderServers();

    window.AppBus.invoke('create_remote_dir', { serverId: id })
      .then(function () {
        window.toast('远程目录已创建', 'ok');
        if (st.checks[id]) st.checks[id].remote_dir_exists = true;
      })
      .catch(function (err) {
        saveToastFail(err, '创建远程目录失败');
      })
      .then(function () {
        st.creating[id] = false;
        renderServers();
      });
  }

  function deleteServerById(id) {
    window.AppBus.invoke('get_config')
      .then(function (cfg) {
        cfg = normalizeCfg(cfg);
        cfg.servers = cfg.servers.filter(function (s) { return s.id !== id; });
        return window.AppBus.invoke('save_config_cmd', { cfg: cfg });
      })
      .then(function () {
        delete st.checks[id];
        window.toast('已删除服务器', 'ok');
        return loadConfig();
      })
      .catch(function (err) {
        saveToastFail(err, '删除服务器失败');
      });
  }

  function deleteProjectById(id) {
    window.AppBus.invoke('get_config')
      .then(function (cfg) {
        cfg = normalizeCfg(cfg);
        cfg.projects = cfg.projects.filter(function (p) { return p.id !== id; });
        return window.AppBus.invoke('save_config_cmd', { cfg: cfg });
      })
      .then(function () {
        window.toast('已删除项目', 'ok');
        return loadConfig();
      })
      .catch(function (err) {
        saveToastFail(err, '删除项目失败');
      });
  }

  /** 内联二次确认:第一次点击变「确认删除?」,3 秒未点击自动恢复 */
  function armDeleteConfirm(btn, onConfirm) {
    if (btn.__ddArmed) {
      if (btn.__ddTimer) {
        window.clearTimeout(btn.__ddTimer);
        btn.__ddTimer = null;
      }
      btn.__ddArmed = false;
      btn.textContent = btn.__ddText;
      btn.classList.remove('btn-danger');
      onConfirm();
      return;
    }
    btn.__ddArmed = true;
    btn.__ddText = btn.textContent;
    btn.textContent = '确认删除?';
    btn.classList.add('btn-danger');
    btn.__ddTimer = window.setTimeout(function () {
      btn.__ddArmed = false;
      btn.textContent = btn.__ddText;
      btn.classList.remove('btn-danger');
      btn.__ddTimer = null;
    }, 3000);
  }

  // ===== 项目列表渲染 =====

  function renderProjects() {
    var tbody = document.getElementById('projects-tbody');
    if (!tbody) return;
    tbody.textContent = '';

    if (!st.loaded) {
      emptyRow(tbody, 5, '配置加载失败,请点击上方重试');
      return;
    }
    if (st.cfg.projects.length === 0) {
      emptyRow(tbody, 5, '暂无部署项目,点击上方「新增项目」添加');
      return;
    }
    st.cfg.projects.forEach(function (project) {
      var tr = document.createElement('tr');

      var nameTd = document.createElement('td');
      nameTd.className = 'nowrap';
      nameTd.textContent = String(project.name);
      tr.appendChild(nameTd);

      var filterTd = document.createElement('td');
      filterTd.className = 'mono';
      filterTd.textContent = String(project.image_filter || '(空,匹配全部)');
      tr.appendChild(filterTd);

      var composeTd = document.createElement('td');
      composeTd.className = 'mono';
      composeTd.textContent = String(project.compose_file);
      tr.appendChild(composeTd);

      var mapsTd = document.createElement('td');
      var count = Array.isArray(project.file_mappings) ? project.file_mappings.length : 0;
      mapsTd.className = 'nowrap';
      mapsTd.textContent = count + ' 项';
      tr.appendChild(mapsTd);

      var actTd = document.createElement('td');
      actTd.className = 'col-action';

      var editBtn = el('button', 'btn btn-sm', '编辑');
      editBtn.type = 'button';
      editBtn.addEventListener('click', function () {
        openProjectModal(findProject(project.id));
      });
      actTd.appendChild(editBtn);

      var delBtn = el('button', 'btn btn-sm', '删除');
      delBtn.type = 'button';
      delBtn.style.marginLeft = '6px';
      delBtn.addEventListener('click', function () {
        armDeleteConfirm(delBtn, function () { deleteProjectById(project.id); });
      });
      actTd.appendChild(delBtn);

      tr.appendChild(actTd);
      tbody.appendChild(tr);
    });
  }

  function emptyRow(tbody, colSpan, text) {
    var tr = document.createElement('tr');
    var td = el('td', 'empty-cell', text);
    td.colSpan = colSpan;
    tr.appendChild(td);
    tbody.appendChild(tr);
  }

  // ===== 自绘模态框 =====

  function openModal(title, buildBody) {
    var overlay = document.getElementById('servers-modal');
    var titleEl = document.getElementById('servers-modal-title');
    var bodyEl = document.getElementById('servers-modal-body');
    if (!overlay || !titleEl || !bodyEl) return;
    titleEl.textContent = title;
    bodyEl.textContent = '';
    buildBody(bodyEl);
    overlay.classList.remove('hidden');
  }

  function closeModal() {
    var overlay = document.getElementById('servers-modal');
    if (overlay) overlay.classList.add('hidden');
  }

  /** 在 body 内追加一行:标签 + 输入框(+ 可选提示) */
  function appendField(body, labelText, inputId, inputType, value, placeholder, hint, inputAttrs) {
    var row = el('div', 'form-row');
    var label = el('label', 'form-label', labelText);
    label.setAttribute('for', inputId);
    row.appendChild(label);

    var input = document.createElement('input');
    input.className = 'form-input';
    input.id = inputId;
    input.type = inputType;
    if (value !== undefined && value !== null) input.value = String(value);
    if (placeholder) input.placeholder = placeholder;
    input.autocomplete = 'off';
    if (inputAttrs) {
      Object.keys(inputAttrs).forEach(function (key) {
        input.setAttribute(key, String(inputAttrs[key]));
      });
    }
    row.appendChild(input);
    if (hint) row.appendChild(el('div', 'form-hint', hint));
    body.appendChild(row);
    return input;
  }

  function appendActions(body, errId, onCancel, onSave, saveText) {
    var actions = el('div', 'form-actions');
    var cancel = el('button', 'btn', '取消');
    cancel.type = 'button';
    cancel.addEventListener('click', closeModal);
    var save = el('button', 'btn btn-primary', saveText || '保存');
    save.type = 'button';
    save.addEventListener('click', onSave);
    actions.appendChild(cancel);
    actions.appendChild(save);
    body.appendChild(actions);
  }

  function appendErrorBox(body, errId) {
    var err = el('div', 'form-error');
    err.id = errId;
    err.classList.add('hidden');
    body.appendChild(err);
  }

  // ===== 服务器编辑表单 =====

  function openServerModal(server) {
    var prev = server || null;
    var prevAuth = (prev && prev.auth) ? prev.auth : { auth_type: 'Key', key_path: null, password_enc: null };
    var prevIsPassword = prevAuth.auth_type === 'Password';

    openModal(prev ? '编辑服务器' : '新增服务器', function (body) {
      appendErrorBox(body, 'srvf-error');

      appendField(body, '名称', 'srvf-name', 'text',
        prev ? prev.name : '', '如:生产服务器');
      appendField(body, '主机(IP 或域名)', 'srvf-host', 'text',
        prev ? prev.host : '', '如:192.168.1.100');
      appendField(body, '端口', 'srvf-port', 'number',
        prev ? prev.port : 22, '', '取值范围 1 - 65535,默认 22',
        { min: '1', max: '65535', step: '1' });
      appendField(body, '用户名', 'srvf-username', 'text',
        prev ? prev.username : '', '如:root');

      // 认证方式单选
      var authRow = el('div', 'form-row');
      authRow.appendChild(el('label', 'form-label', '认证方式'));
      var radioRow = el('div', 'radio-row');

      var radioKey = document.createElement('input');
      radioKey.type = 'radio';
      radioKey.name = 'srvf-auth';
      radioKey.id = 'srvf-auth-key';
      radioKey.value = 'Key';
      radioKey.checked = !prevIsPassword;

      var radioPass = document.createElement('input');
      radioPass.type = 'radio';
      radioPass.name = 'srvf-auth';
      radioPass.id = 'srvf-auth-pass';
      radioPass.value = 'Password';
      radioPass.checked = prevIsPassword;

      var labelKey = el('label', '');
      labelKey.setAttribute('for', 'srvf-auth-key');
      labelKey.appendChild(radioKey);
      labelKey.appendChild(el('span', '', '私钥'));
      var labelPass = el('label', '');
      labelPass.setAttribute('for', 'srvf-auth-pass');
      labelPass.appendChild(radioPass);
      labelPass.appendChild(el('span', '', '密码'));
      radioRow.appendChild(labelKey);
      radioRow.appendChild(labelPass);
      authRow.appendChild(radioRow);
      body.appendChild(authRow);

      // 私钥路径(Key)
      var keyBlock = el('div', 'form-row');
      keyBlock.id = 'srvf-key-block';
      var keyLabel = el('label', 'form-label', '私钥路径');
      keyLabel.setAttribute('for', 'srvf-key-path');
      keyBlock.appendChild(keyLabel);
      var keyInput = document.createElement('input');
      keyInput.className = 'form-input';
      keyInput.id = 'srvf-key-path';
      keyInput.type = 'text';
      keyInput.autocomplete = 'off';
      keyInput.value = prevAuth.key_path ? String(prevAuth.key_path) : '';
      keyInput.placeholder = '如:C:\\Users\\you\\.ssh\\id_rsa';
      keyBlock.appendChild(keyInput);
      keyBlock.appendChild(el('div', 'form-hint', '本机私钥文件的绝对路径'));
      body.appendChild(keyBlock);

      // 密码(Password)
      var passBlock = el('div', 'form-row');
      passBlock.id = 'srvf-pass-block';
      var passLabel = el('label', 'form-label', '登录密码');
      passLabel.setAttribute('for', 'srvf-password');
      passBlock.appendChild(passLabel);
      var passInput = document.createElement('input');
      passInput.className = 'form-input';
      passInput.id = 'srvf-password';
      passInput.type = 'password';
      passInput.autocomplete = 'new-password';
      passBlock.appendChild(passInput);
      passBlock.appendChild(el('div', 'form-hint',
        (prev && prevIsPassword && prevAuth.password_enc)
          ? '留空表示沿用已保存密码;输入新密码将覆盖已保存密码'
          : '留空表示沿用已保存密码'));
      body.appendChild(passBlock);

      function syncAuthBlocks() {
        var isPass = radioPass.checked;
        keyBlock.classList.toggle('hidden', isPass);
        passBlock.classList.toggle('hidden', !isPass);
      }
      radioKey.addEventListener('change', syncAuthBlocks);
      radioPass.addEventListener('change', syncAuthBlocks);
      syncAuthBlocks();

      appendField(body, '远程部署目录', 'srvf-remote-dir', 'text',
        prev ? prev.remote_dir : '', '如:/opt/myapp');

      appendActions(body, 'srvf-error', closeModal, function () {
        saveServer(prev);
      });
    });
  }

  /** 服务器表单保存:校验 → (密码加密) → get_config → 全量写回 */
  function saveServer(prev) {
    formClearError('srvf-error');

    var name = fieldVal('srvf-name');
    var host = fieldVal('srvf-host');
    var portRaw = fieldVal('srvf-port');
    var username = fieldVal('srvf-username');
    var remoteDir = fieldVal('srvf-remote-dir');
    var authType = (document.getElementById('srvf-auth-pass') || {}).checked ? 'Password' : 'Key';
    var keyPath = fieldVal('srvf-key-path');
    var passNode = document.getElementById('srvf-password');
    var newPass = passNode ? passNode.value : '';

    if (!name) return formFail('srvf-error', '请填写名称');
    if (!host) return formFail('srvf-error', '请填写主机地址');
    if (!/^\d+$/.test(portRaw) || Number(portRaw) < 1 || Number(portRaw) > 65535) {
      return formFail('srvf-error', '端口需为 1 - 65535 之间的整数');
    }
    if (!username) return formFail('srvf-error', '请填写用户名');
    if (!remoteDir) return formFail('srvf-error', '请填写远程部署目录');
    if (authType === 'Key' && !keyPath) return formFail('srvf-error', '私钥认证需填写私钥路径');

    var prevAuth = (prev && prev.auth) ? prev.auth : {};
    var hasSavedPassword = authType === 'Password' && !!prevAuth.password_enc;
    if (authType === 'Password' && !newPass && !hasSavedPassword) {
      return formFail('srvf-error', '密码认证需填写登录密码');
    }

    // Key → 只存 key_path(password_enc 原样保留,便于切回密码认证);
    // Password → 输入了新密码时先加密,否则沿用已存密文
    var auth = {
      auth_type: authType,
      key_path: authType === 'Key' ? keyPath : null,
      password_enc: prevAuth.password_enc || null
    };

    var encPromise = (authType === 'Password' && newPass)
      ? window.AppBus.invoke('encrypt_password', { plain: newPass })
      : Promise.resolve(null);

    encPromise
      .then(function (enc) {
        if (authType === 'Password' && enc) auth.password_enc = enc;
        return window.AppBus.invoke('get_config');
      })
      .then(function (cfg) {
        cfg = normalizeCfg(cfg);
        var server = {
          id: (prev && prev.id) ? prev.id : uuid(),
          name: name,
          host: host,
          port: Number(portRaw),
          username: username,
          auth: auth,
          remote_dir: remoteDir
        };
        var idx = -1;
        for (var i = 0; i < cfg.servers.length; i++) {
          if (cfg.servers[i].id === server.id) { idx = i; break; }
        }
        if (idx >= 0) cfg.servers[idx] = server;
        else cfg.servers.push(server);
        return window.AppBus.invoke('save_config_cmd', { cfg: cfg });
      })
      .then(function () {
        closeModal();
        window.toast('已保存', 'ok');
        return loadConfig();
      })
      .catch(function (err) {
        formFail('srvf-error', errText(err) || '保存失败');
        saveToastFail(err, '保存失败');
      });
  }

  // ===== 项目编辑表单 =====

  function openProjectModal(project) {
    var prev = project || null;

    openModal(prev ? '编辑项目' : '新增项目', function (body) {
      appendErrorBox(body, 'prjf-error');

      appendField(body, '名称', 'prjf-name', 'text',
        prev ? prev.name : '', '如:我的应用');
      appendField(body, '镜像过滤关键字', 'prjf-filter', 'text',
        prev ? prev.image_filter : '', '如:myapp', '部署时按该关键字匹配本地镜像仓库名,留空匹配全部镜像');
      appendField(body, 'compose 文件相对路径', 'prjf-compose', 'text',
        prev ? prev.compose_file : '', '如:docker-compose.yml', '相对远程部署目录的路径');

      // 文件映射编辑表格
      var mapRow = el('div', 'form-row');
      mapRow.appendChild(el('label', 'form-label', '文件映射(本地 → 服务器)'));
      var wrap = document.createElement('div');
      wrap.className = 'table-wrap';
      var table = document.createElement('table');
      table.className = 'mapping-table';
      var thead = document.createElement('thead');
      var headTr = document.createElement('tr');
      ['本地路径', '服务器相对路径', '目录', '操作'].forEach(function (text) {
        headTr.appendChild(el('th', '', text));
      });
      thead.appendChild(headTr);
      table.appendChild(thead);

      var tbody = document.createElement('tbody');
      tbody.id = 'prjf-mappings-body';
      table.appendChild(tbody);
      wrap.appendChild(table);
      mapRow.appendChild(wrap);
      mapRow.appendChild(el('div', 'form-hint',
        '本地路径可粘贴绝对路径(如 D:\\app\\conf);勾选「目录」表示映射整个目录;两格都留空的行保存时将被忽略'));

      var addBtn = el('button', 'btn btn-sm mapping-add', '+ 添加映射行');
      addBtn.type = 'button';
      addBtn.addEventListener('click', function () {
        appendMappingRow(tbody, null);
      });
      mapRow.appendChild(addBtn);
      body.appendChild(mapRow);

      if (prev && Array.isArray(prev.file_mappings)) {
        prev.file_mappings.forEach(function (m) {
          appendMappingRow(tbody, m);
        });
      }
      if (!tbody.querySelector('tr')) {
        appendMappingRow(tbody, null); // 至少给一行,方便直接填写
      }

      appendActions(body, 'prjf-error', closeModal, function () {
        saveProject(prev);
      });
    });
  }

  /** 追加一行文件映射编辑行(本地路径 / 服务器相对路径 / 目录勾选 / 删除) */
  function appendMappingRow(tbody, mapping) {
    var m = mapping || {};
    var tr = document.createElement('tr');

    var tdLocal = document.createElement('td');
    var localInput = document.createElement('input');
    localInput.className = 'form-input map-local';
    localInput.type = 'text';
    localInput.autocomplete = 'off';
    localInput.value = m.local ? String(m.local) : '';
    localInput.placeholder = '如:D:\\app\\nginx.conf';
    tdLocal.appendChild(localInput);
    tr.appendChild(tdLocal);

    var tdRemote = document.createElement('td');
    var remoteInput = document.createElement('input');
    remoteInput.className = 'form-input map-remote';
    remoteInput.type = 'text';
    remoteInput.autocomplete = 'off';
    remoteInput.value = m.remote ? String(m.remote) : '';
    remoteInput.placeholder = '如:conf/nginx.conf';
    tdRemote.appendChild(remoteInput);
    tr.appendChild(tdRemote);

    var tdDir = document.createElement('td');
    var dirLabel = el('label', 'mapping-dir');
    var dirBox = document.createElement('input');
    dirBox.type = 'checkbox';
    dirBox.className = 'map-dir';
    dirBox.checked = !!m.is_dir;
    dirLabel.appendChild(dirBox);
    dirLabel.appendChild(el('span', '', '目录'));
    tdDir.appendChild(dirLabel);
    tr.appendChild(tdDir);

    var tdAct = document.createElement('td');
    var delBtn = el('button', 'btn btn-sm', '删除');
    delBtn.type = 'button';
    delBtn.addEventListener('click', function () { tr.remove(); });
    tdAct.appendChild(delBtn);
    tr.appendChild(tdAct);

    tbody.appendChild(tr);
  }

  /** 项目表单保存:校验 → get_config → 全量写回 */
  function saveProject(prev) {
    formClearError('prjf-error');

    var name = fieldVal('prjf-name');
    var filter = fieldVal('prjf-filter');
    var compose = fieldVal('prjf-compose');

    if (!name) return formFail('prjf-error', '请填写名称');
    if (!compose) return formFail('prjf-error', '请填写 compose 文件相对路径');

    var mappings = [];
    var tbody = document.getElementById('prjf-mappings-body');
    var rows = tbody ? tbody.querySelectorAll('tr') : [];
    for (var i = 0; i < rows.length; i++) {
      var localNode = rows[i].querySelector('.map-local');
      var remoteNode = rows[i].querySelector('.map-remote');
      var dirNode = rows[i].querySelector('.map-dir');
      var local = localNode ? localNode.value.trim() : '';
      var remote = remoteNode ? remoteNode.value.trim() : '';
      if (!local && !remote) continue; // 两格都空:忽略该行
      if (!local || !remote) {
        return formFail('prjf-error',
          '文件映射第 ' + (i + 1) + ' 行需同时填写本地路径与服务器相对路径');
      }
      mappings.push({
        local: local,
        remote: remote,
        is_dir: !!(dirNode && dirNode.checked)
      });
    }

    window.AppBus.invoke('get_config')
      .then(function (cfg) {
        cfg = normalizeCfg(cfg);
        var project = {
          id: (prev && prev.id) ? prev.id : uuid(),
          name: name,
          image_filter: filter,
          compose_file: compose,
          file_mappings: mappings
        };
        var idx = -1;
        for (var j = 0; j < cfg.projects.length; j++) {
          if (cfg.projects[j].id === project.id) { idx = j; break; }
        }
        if (idx >= 0) cfg.projects[idx] = project;
        else cfg.projects.push(project);
        return window.AppBus.invoke('save_config_cmd', { cfg: cfg });
      })
      .then(function () {
        closeModal();
        window.toast('已保存', 'ok');
        return loadConfig();
      })
      .catch(function (err) {
        formFail('prjf-error', errText(err) || '保存失败');
        saveToastFail(err, '保存失败');
      });
  }

  // ===== 运行日志面板(底部折叠,默认收起)=====

  function setLogOpen(open) {
    var body = document.getElementById('servers-log-body');
    var btn = document.getElementById('servers-log-toggle');
    if (!body || !btn) return;
    body.classList.toggle('hidden', !open);
    btn.textContent = open ? '运行日志(点击收起)' : '运行日志(点击展开,安装/检测输出在此显示)';
    if (open) rebuildLog();
  }

  function rebuildLog() {
    var body = document.getElementById('servers-log-body');
    if (!body) return;
    body.textContent = st.logs.length > 0 ? st.logs.join('\n') : '(暂无输出)';
    body.scrollTop = body.scrollHeight;
  }

  function appendLogLine(line) {
    st.logs.push(line === null || line === undefined ? '' : String(line));
    if (st.logs.length > LOG_MAX_LINES) {
      st.logs.splice(0, st.logs.length - LOG_MAX_LINES);
    }
    var body = document.getElementById('servers-log-body');
    if (!body || body.classList.contains('hidden')) return;
    body.textContent = st.logs.join('\n');
    body.scrollTop = body.scrollHeight;
  }

  /** 常驻监听 'server-log':模块级守卫变量保证只注册一次 */
  function bindLogListener() {
    if (logListenerBound) return;
    logListenerBound = true;
    window.AppBus.on('server-log', function (event) {
      appendLogLine(event ? event.payload : '');
    }).catch(function (err) {
      // 浏览器直接打开(无 Tauri)时事件 API 不可用:仅记录,不打扰用户
      logListenerBound = false;
      if (window.console && console.warn) {
        console.warn('[servers] server-log 事件监听注册失败:', err);
      }
    });
  }

  // ===== 初始化 =====

  function bindStaticEvents() {
    var addServer = document.getElementById('servers-add-btn');
    if (addServer) {
      addServer.addEventListener('click', function () { openServerModal(null); });
    }
    var addProject = document.getElementById('projects-add-btn');
    if (addProject) {
      addProject.addEventListener('click', function () { openProjectModal(null); });
    }
    var logToggle = document.getElementById('servers-log-toggle');
    if (logToggle) {
      logToggle.addEventListener('click', function () {
        var body = document.getElementById('servers-log-body');
        var willOpen = !!(body && body.classList.contains('hidden'));
        setLogOpen(willOpen);
      });
    }

    var modalClose = document.getElementById('servers-modal-close');
    if (modalClose) {
      modalClose.addEventListener('click', closeModal);
    }
    var overlay = document.getElementById('servers-modal');
    if (overlay) {
      overlay.addEventListener('click', function (e) {
        if (e.target === overlay) closeModal();
      });
    }
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') closeModal();
    });
  }

  function init() {
    bindStaticEvents();
    bindLogListener(); // 常驻监听,内部有守卫防重复注册
    refreshBanner();

    window.addEventListener('pagechange', function (e) {
      if (!e || !e.detail || e.detail.page !== 'servers') return;
      refreshBanner();
      if (!st.loaded && !st.loading) loadConfig();
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
