/* ============================================================
 * servers.js — 服务器管理页逻辑(依赖 app.js 提供的全局工具)
 *
 * 后端命令(对象字段为 Rust snake_case 原样序列化,JS 参数名 camelCase):
 * - get_config() -> AppConfig { servers: Server[], projects: Project[] }
 *   Server  = { id, name, host, port, username, remote_dir,
 *               auth: { auth_type: "Key"|"Password", key_path, password_enc } }
 *   Project = { id, name, image_filter, compose_file,
 *               file_mappings: [{ local, remote, is_dir }], service_overrides,
 *               health_wait_secs, pre_deploy_cmd, post_deploy_cmd, notify_webhook }
 * - save_config_cmd({ cfg })                全量保存配置
 * - encrypt_password({ plain }) -> string   base64 密文,存 auth.password_enc
 * - test_server / server_env_check({ serverId, passwordPlain? })
 *     -> ServerCheckReport { docker, compose, gzip, remote_dir_exists,
 *                            disk_free_gb, errors: string[] }
 *     (两者等价:连接 + 远端环境检测;passwordPlain 一般不传,后端用 DPAPI
 *      解密已存密文;auth_type=Key 时后端忽略 passwordPlain)
 * - install_server_docker({ serverId })     过程输出经 'server-log' 事件逐行推送
 * - create_remote_dir({ serverId })
 * - prune_server({ serverId, passwordPlain? }) -> null
 *     清理悬空镜像与已退出容器;输出经 'server-log' 事件逐行推送(300s 超时)
 * - preview_compose({ sourcePath }) -> ComposeStack
 *     ComposeStack = { project_name, services: StackService[], errors: string[] }
 *     StackService = { service, image, has_build, mode: "Local"|"Pull",
 *                      match_state: "Exact"|"RepoOnly"|"Missing",
 *                      local_tag, warning }(静态只读解析,供导入前预览)
 * - import_compose({ sourcePath, name }) -> ProjectConfig
 *     校验并复制 compose(连同同目录 .env)到应用配置目录,以解析默认分类
 *     建新项目并写回配置;返回的 ProjectConfig 含 id/compose_file 副本路径/
 *     service_overrides,由前端并入项目列表后 save_config_cmd 补齐其余字段
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

  /**
   * 部署钩子预设模板(chips,不默认加载;点击插入 textarea 后可再编辑)。
   * $(date ...) 由服务器端 shell 展开,前端原样插入。
   */
  var PRESET_CMDS = {
    pre: [
      {
        label: 'MySQL 全库备份',
        cmd: "mkdir -p backups && docker compose exec -T db sh -c 'mysqldump -uroot -p\"$MYSQL_ROOT_PASSWORD\" --all-databases' | gzip > backups/db-$(date +%F-%H%M).sql.gz"
      },
      {
        label: 'PostgreSQL 全库备份',
        cmd: "mkdir -p backups && docker compose exec -T db sh -c 'pg_dumpall -U\"$POSTGRES_USER\"' | gzip > backups/pg-$(date +%F-%H%M).sql.gz"
      }
    ],
    post: [
      { label: '悬空镜像清理', cmd: 'docker image prune -f' },
      { label: '发布日志记录', cmd: 'echo "$(date +%F-%T) deployed" >> releases.log' }
    ]
  };

  var st = {
    cfg: null,            // get_config 的完整结果(AppConfig)
    loaded: false,        // 是否已成功加载过配置
    loading: false,       // 配置加载中(防重复请求)
    checks: {},           // serverId -> ServerCheckReport(卡片内嵌检测结果)
    checking: {},         // serverId -> true(检测进行中,防重复触发)
    installing: {},       // serverId -> true(Docker 安装进行中)
    creating: {},         // serverId -> true(创建远程目录进行中)
    pruning: {},          // serverId -> true(清理优化进行中)
    logs: []              // server-log 事件累积的输出行
  };

  /** server-log 事件监听守卫:只注册一次,防止重复绑定 */
  var logListenerBound = false;

  /**
   * 新增项目表单的导入预览状态(模块级:保存时 saveProject 需读取)。
   * path = 已成功预览的 compose 路径;stack = 对应 ComposeStack。
   * 两者一致时保存可直接复用预览结果,路径变化后重新解析。
   */
  var importPreview = { path: '', stack: null };

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

  /** 取路径的文件名并去掉扩展名(D:/a/b/my-stack.yml → my-stack);用于导入时预填项目名 */
  function fileNameNoExt(p) {
    if (!p) return '';
    var s = String(p).replace(/[\\/]+$/, '');
    var idx = Math.max(s.lastIndexOf('\\'), s.lastIndexOf('/'));
    var base = idx >= 0 ? s.slice(idx + 1) : s;
    var dot = base.lastIndexOf('.');
    if (dot > 0) base = base.slice(0, dot);
    return base;
  }

  /**
   * compose 服务匹配徽章(三级匹配,与部署页整栈分类表同视觉):
   * Exact → 青淡底 check「已匹配」;RepoOnly → 琥珀底 run「标签不一致」;
   * Missing → 墨底白字 cross「本地不存在」。warning 有值时悬浮展示。
   */
  function matchBadge(svc) {
    var kind = svc.match_state === 'Exact' ? 'ok'
      : (svc.match_state === 'RepoOnly' ? 'warn' : 'fail');
    var text = svc.match_state === 'Exact' ? '已匹配'
      : (svc.match_state === 'RepoOnly' ? '标签不一致' : '本地不存在');
    var badge = window.fillBadge(el('span'), kind, text);
    if (svc.warning) {
      badge.title = String(svc.warning);
    } else if (svc.match_state === 'RepoOnly') {
      badge.title = '本地标签与 compose 不一致';
    }
    return badge;
  }

  /** 传输分类徽章(默认分类;has_build 的 Local 服务附 build 标记) */
  function modeBadge(mode, hasBuild) {
    var text = mode === 'Local' ? '本地传输' : '服务器拉取';
    if (mode === 'Local' && hasBuild) text += ' · build';
    return window.fillBadge(el('span'), 'info', text);
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

  /** 表单内联错误提示(自绘,不调用系统对话框) */
  function formFail(errId, msg) {
    var box = document.getElementById(errId);
    if (box) {
      box.textContent = msg;
      box.classList.remove('hidden');
    }
    return false;
  }

  /**
   * 把错误框滚入可视区。延迟 60ms:点击保存会触发浏览器对按钮的原生焦点滚动,
   * 同步滚动会被其覆盖,延后一拍才能生效。
   */
  function scrollErrorVisible(errId) {
    setTimeout(function () {
      var box = document.getElementById(errId);
      if (box && box.scrollIntoView) {
        try { box.scrollIntoView({ block: 'nearest' }); } catch (_) { box.scrollIntoView(); }
      }
    }, 60);
  }

  /**
   * 表单失败强化通道:内联错误框 + 滚动到可视区 + toast。
   * 用于所有表单的校验与保存失败,确保任何失败都不可能被错过
   * (错误框可能位于长表单顶部,而用户视口停在底部操作按钮处)。
   */
  function formFailLoud(errId, msg) {
    formFail(errId, msg);
    scrollErrorVisible(errId);
    window.toast(msg, 'fail');
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
    st.cfg.servers.forEach(function (server, i) {
      list.appendChild(serverCard(server, i));
    });
  }

  /** 键值对节点(键 = 大写微标签,值 = mono) */
  function kvPair(key, val) {
    var node = el('div', 'kv');
    node.appendChild(el('span', 'kv-key', key));
    node.appendChild(el('span', 'kv-val', val));
    return node;
  }

  /** 服务器卡片 = dossier 技术面板:SRV 编号 + 名称 + 操作组 / 键值区 / 检测区 */
  function serverCard(server, index) {
    var auth = server.auth || {};
    var isPassword = auth.auth_type === 'Password';
    var checking = !!st.checking[server.id];

    var card = el('div', 'server-card');

    // 头部:SRV-XX 编号 + 名称 + 操作按钮组(文字按钮)
    var head = el('div', 'server-head');
    head.appendChild(el('span', 'server-index', 'SRV-' + ('0' + (index + 1)).slice(-2)));
    head.appendChild(el('span', 'server-name', server.name));

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

    // 清理优化:自绘确认条 → prune_server(输出经 server-log 回显)
    var pruning = !!st.pruning[server.id];
    var pruneBtn = el('button', 'btn btn-sm', pruning ? '清理中…' : '清理优化');
    pruneBtn.type = 'button';
    pruneBtn.disabled = pruning || checking || !!st.installing[server.id];
    if (!pruneBtn.disabled) {
      pruneBtn.addEventListener('click', function () { showPruneConfirm(card, server); });
    } else if (pruning) {
      pruneBtn.title = '正在清理,输出见底部「运行日志」';
    }
    actions.appendChild(pruneBtn);

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

    // 键值区:host:port / 用户 / 认证 / 远程目录
    var kv = el('div', 'server-kv');
    kv.appendChild(kvPair('主机 HOST', server.host + ':' + server.port));
    kv.appendChild(kvPair('用户 USER', server.username));
    kv.appendChild(kvPair('认证 AUTH', isPassword ? '密码' : '私钥'));
    kv.appendChild(kvPair('远程目录 REMOTE_DIR', server.remote_dir));
    card.appendChild(kv);

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
    badges.appendChild(window.fillBadge(el('span'), diskKind, diskText));
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
    return window.fillBadge(el('span'), ok ? 'ok' : 'fail',
      label + ':' + (ok ? '通过' : '未通过'));
  }

  /** 自绘确认条(不调用系统对话框):文案 + 确认/取消 内联按钮 */
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

  /** 清理优化自绘确认条:插入卡片内嵌确认条(检测区上方),取消即移除 */
  function showPruneConfirm(card, server) {
    var old = card.querySelector('.server-prune-actions');
    if (old) old.remove();
    var bar = el('div', 'server-check-actions server-prune-actions');
    bar.appendChild(el('span', 'confirm-text', '将清理悬空镜像与已退出容器,确认?'));
    var ok = el('button', 'btn btn-danger btn-sm', '确认清理');
    ok.type = 'button';
    ok.addEventListener('click', function () { startPrune(server); });
    bar.appendChild(ok);
    var cancel = el('button', 'btn btn-sm', '取消');
    cancel.type = 'button';
    cancel.addEventListener('click', function () { bar.remove(); });
    bar.appendChild(cancel);
    var check = card.querySelector('.server-check');
    if (check) card.insertBefore(bar, check);
    else card.appendChild(bar);
  }

  /** 清理服务器(prune_server):输出经 server-log 事件写入底部运行日志 */
  function startPrune(server) {
    var id = server.id;
    if (st.pruning[id] || st.checking[id] || st.installing[id]) return;
    st.pruning[id] = true;
    setLogOpen(true); // 自动展开 server-log 终端,便于观察清理输出
    window.toast('开始清理服务器,输出见底部「运行日志」', 'info');
    renderServers(); // 重渲染:按钮变「清理中…」禁用,确认条随卡片重建消失

    window.AppBus.invoke('prune_server', { serverId: id })
      .then(function () {
        window.toast('清理完成', 'ok');
      })
      .catch(function (err) {
        saveToastFail(err, '服务器清理失败');
      })
      .then(function () {
        st.pruning[id] = false;
        renderServers();
      });
  }

  // ===== 远端操作(test / env check / install / mkdir)=====

  /**
   * 连接 + 环境检测。
   * @param {Object} server 服务器对象(仅使用其 id)
   * @param {string} mode 'test' = test_server;'env' = server_env_check(两者等价)
   */
  function runEnvCheck(server, mode) {
    var id = server.id;
    if (st.checking[id] || st.installing[id] || st.pruning[id]) return;
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
    if (st.installing[id] || st.checking[id] || st.pruning[id]) return;
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
    return save;
  }

  function appendErrorBox(body, errId) {
    var err = el('div', 'form-error');
    err.id = errId;
    err.classList.add('hidden');
    body.appendChild(err);
  }

  // ===== 项目表单:部署钩子区块 + 预设 chips(Task 7 生产加固)=====

  /**
   * 钩子命令区块:标签 + 预设 chips 行 + 多行 textarea(等宽字体)。
   * chips 不默认加载任何模板;点击把模板文本插入 textarea(已有内容时另起一行追加)。
   */
  function appendHookBlock(body, labelText, textareaId, presets, prevValue) {
    var row = el('div', 'form-row');
    row.appendChild(el('label', 'form-label', labelText));

    var chipRow = el('div', 'chip-row');
    presets.forEach(function (preset) {
      var chip = el('button', 'preset-chip', preset.label);
      chip.type = 'button';
      chip.title = '点击插入预设模板,插入后可修改';
      chip.addEventListener('click', function () { insertPresetCmd(textareaId, preset.cmd); });
      chipRow.appendChild(chip);
    });
    row.appendChild(chipRow);

    var ta = document.createElement('textarea');
    ta.className = 'form-textarea';
    ta.id = textareaId;
    ta.rows = 3;
    ta.spellcheck = false;
    ta.autocomplete = 'off';
    ta.placeholder = '留空表示不执行该钩子';
    if (prevValue) ta.value = String(prevValue);
    row.appendChild(ta);
    body.appendChild(row);
    return ta;
  }

  /** 预设 chip 点击:把模板命令插入对应 textarea(已有内容时先换行再追加,可再编辑) */
  function insertPresetCmd(textareaId, cmd) {
    var ta = document.getElementById(textareaId);
    if (!ta) return;
    var current = String(ta.value).replace(/\s+$/, '');
    ta.value = current ? current + '\n' + cmd : cmd;
    ta.focus();
    ta.selectionStart = ta.selectionEnd = ta.value.length;
  }

  /**
   * 收集并校验生产加固表单字段:健康等待秒数 / pre-post 钩子 / webhook。
   * 校验失败已 formFail 提示并返回 null;钩子留空存 null,webhook 留空存 null。
   */
  function collectProjectExtras(errId) {
    var healthRaw = fieldVal('prjf-health-wait');
    var healthWait = 0;
    if (healthRaw !== '') {
      if (!/^\d+$/.test(healthRaw) || Number(healthRaw) > 86400) {
        formFailLoud(errId, '健康检查等待秒数需为 0 - 86400 之间的整数(0 为关闭)');
        return null;
      }
      healthWait = Number(healthRaw);
    }

    var webhook = fieldVal('prjf-webhook');
    if (webhook && !/^https?:\/\//i.test(webhook)) {
      formFailLoud(errId, '完成通知 webhook 需以 http:// 或 https:// 开头,或留空');
      return null;
    }

    var preCmd = fieldVal('prjf-pre-cmd');
    var postCmd = fieldVal('prjf-post-cmd');
    return {
      health_wait_secs: healthWait,
      pre_deploy_cmd: preCmd ? preCmd : null,
      post_deploy_cmd: postCmd ? postCmd : null,
      notify_webhook: webhook ? webhook : null
    };
  }

  // ===== 项目表单:导入 compose 文件区块(仅新增项目)=====

  /**
   * 「导入 compose 文件」区块:路径输入 + 解析预览(preview_compose)
   * + 预览表(服务/镜像/匹配徽章/默认分类徽章,errors 红框)。
   * 路径变化时项目名自动预填为文件名去扩展名(可手动改)。
   */
  function appendImportBlock(body) {
    // 路径输入
    var pathRow = el('div', 'form-row');
    pathRow.appendChild(el('label', 'form-label', '导入 compose 文件(可选)'));
    var pathInput = document.createElement('input');
    pathInput.className = 'form-input';
    pathInput.id = 'prjf-import-path';
    pathInput.type = 'text';
    pathInput.autocomplete = 'off';
    pathInput.placeholder = '如:D:\\apps\\myapp\\docker-compose.yml';
    pathRow.appendChild(pathInput);
    pathRow.appendChild(el('div', 'form-hint',
      '填写 compose 文件绝对路径,保存时复制到应用配置目录并按解析结果自动生成服务传输分类;留空则手工填写下方 compose 相对路径'));
    body.appendChild(pathRow);

    // 解析预览按钮 + 状态
    var previewRow = el('div', 'form-row');
    var bar = el('div', 'preview-bar');
    var previewBtn = el('button', 'btn btn-sm', '解析预览');
    previewBtn.id = 'prjf-preview-btn';
    previewBtn.type = 'button';
    var status = el('span', 'preview-status');
    status.id = 'prjf-preview-status';
    bar.appendChild(previewBtn);
    bar.appendChild(status);

    // 预览表 + 错误框(默认隐藏)
    var box = el('div', 'hidden');
    box.id = 'prjf-preview-box';
    var wrap = el('div', 'table-wrap');
    var table = document.createElement('table');
    table.className = 'data-table';
    var thead = document.createElement('thead');
    var headTr = document.createElement('tr');
    ['服务 SERVICE', '镜像 IMAGE', '匹配 MATCH', '默认分类 MODE'].forEach(function (text) {
      headTr.appendChild(el('th', '', text));
    });
    thead.appendChild(headTr);
    table.appendChild(thead);
    var tbody = document.createElement('tbody');
    tbody.id = 'prjf-preview-tbody';
    table.appendChild(tbody);
    wrap.appendChild(table);
    box.appendChild(wrap);
    var perr = el('div', 'check-error');
    perr.id = 'prjf-preview-errors';
    perr.classList.add('hidden');
    box.appendChild(perr);
    bar.appendChild(box);
    previewRow.appendChild(bar);
    body.appendChild(previewRow);

    previewBtn.addEventListener('click', function () { runImportPreview(); });
    pathInput.addEventListener('input', function () { onImportPathInput(); });
  }

  /** 路径输入变化:预填项目名(文件名去扩展名,已手改则不覆盖)+ 切换手工路径置灰 + 清理过期预览 */
  function onImportPathInput() {
    var p = fieldVal('prjf-import-path');
    var base = fileNameNoExt(p);
    var nameInput = document.getElementById('prjf-name');
    if (nameInput && base) {
      var current = String(nameInput.value).trim();
      // 名称尚为空或仍是上一次自动预填值时才覆盖,保留用户手改内容
      if (!current || current === importPreview.autoName) {
        nameInput.value = base;
      }
    }
    importPreview.autoName = base;

    var manual = document.getElementById('prjf-compose');
    if (manual) manual.disabled = !!p;

    if (!p) {
      // 清空路径:预览与错误一并复位
      importPreview.path = '';
      importPreview.stack = null;
      var box = document.getElementById('prjf-preview-box');
      if (box) box.classList.add('hidden');
      var perr = document.getElementById('prjf-preview-errors');
      if (perr) { perr.textContent = ''; perr.classList.add('hidden'); }
      var status = document.getElementById('prjf-preview-status');
      if (status) status.textContent = '';
    }
  }

  /** 解析预览:preview_compose(静态只读,不落盘);失败进表单错误框 */
  function runImportPreview() {
    var p = fieldVal('prjf-import-path');
    if (!p) {
      formFailLoud('prjf-error', '请先填写 compose 文件路径');
      return;
    }
    formClearError('prjf-error');
    var btn = document.getElementById('prjf-preview-btn');
    var status = document.getElementById('prjf-preview-status');
    if (btn) btn.disabled = true;
    if (status) status.textContent = '解析中…';

    window.AppBus.invoke('preview_compose', { sourcePath: p })
      .then(function (stack) {
        importPreview.path = p;
        importPreview.stack = stack || { project_name: '', services: [], errors: [] };
        renderImportPreview(importPreview.stack);
        if (status) {
          status.textContent = (importPreview.stack.errors || []).length > 0
            ? '解析完成,存在需要处理的问题'
            : '解析完成';
        }
      })
      .catch(function (err) {
        importPreview.path = '';
        importPreview.stack = null;
        var box = document.getElementById('prjf-preview-box');
        if (box) box.classList.add('hidden');
        formFailLoud('prjf-error', '解析失败:' + (errText(err) || '未知错误'));
        if (status) status.textContent = '';
      })
      .then(function () {
        if (btn) btn.disabled = false;
      });
  }

  /** 渲染预览表:服务/镜像/匹配徽章/默认分类徽章;errors 红框(阻断保存) */
  function renderImportPreview(stack) {
    var box = document.getElementById('prjf-preview-box');
    var tbody = document.getElementById('prjf-preview-tbody');
    var perr = document.getElementById('prjf-preview-errors');
    if (!box || !tbody || !perr) return;

    tbody.textContent = '';
    perr.textContent = '';

    var errors = Array.isArray(stack.errors) ? stack.errors : [];
    if (errors.length > 0) {
      perr.appendChild(el('div', 'servers-error-text', '以下问题将阻断整栈部署:'));
      errors.forEach(function (line) {
        perr.appendChild(el('div', 'servers-error-text', line));
      });
      perr.classList.remove('hidden');
    } else {
      perr.classList.add('hidden');
    }

    var services = Array.isArray(stack.services) ? stack.services : [];
    if (services.length === 0) {
      emptyRow(tbody, 4, 'compose 未定义任何服务');
    }
    services.forEach(function (svc) {
      var tr = document.createElement('tr');

      var tdSvc = document.createElement('td');
      tdSvc.className = 'mono';
      tdSvc.textContent = String(svc.service);
      tr.appendChild(tdSvc);

      var tdImg = document.createElement('td');
      tdImg.className = 'mono';
      if (svc.image) {
        tdImg.textContent = String(svc.image);
      } else {
        tdImg.appendChild(el('span', 'none-text', '(未设 image 字段)'));
      }
      tr.appendChild(tdImg);

      var tdMatch = document.createElement('td');
      tdMatch.appendChild(matchBadge(svc));
      tr.appendChild(tdMatch);

      var tdMode = document.createElement('td');
      tdMode.appendChild(modeBadge(svc.mode, !!svc.has_build));
      tr.appendChild(tdMode);

      tbody.appendChild(tr);
    });

    box.classList.remove('hidden');
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

      var saveBtn = appendActions(body, 'srvf-error', closeModal, function () {
        saveServer(prev, saveBtn);
      });
    });
  }

  /**
   * 服务器表单保存:校验 → (密码加密) → get_config → 全量写回。
   * 任何失败双通道提示(表单内联错误框滚动到可视区 + toast),避免"点了没反应"。
   * 保存成功后自动对新服务器发起一次「测试连接」,结果呈现在卡片上。
   */
  function saveServer(prev, saveBtn) {
    formClearError('srvf-error');

    function fail(msg) {
      formFail('srvf-error', msg);
      var boxNode = document.getElementById('srvf-error');
      if (boxNode && boxNode.scrollIntoView) {
        try { boxNode.scrollIntoView({ block: 'nearest' }); } catch (_) { boxNode.scrollIntoView(); }
      }
      window.toast(msg, 'fail');
      return false;
    }
    function setSaving(saving) {
      if (saveBtn) {
        saveBtn.disabled = saving;
        saveBtn.textContent = saving ? '保存中…' : '保存';
      }
    }

    var name = fieldVal('srvf-name');
    var host = fieldVal('srvf-host');
    var portRaw = fieldVal('srvf-port');
    var username = fieldVal('srvf-username');
    var remoteDir = fieldVal('srvf-remote-dir');
    var authType = (document.getElementById('srvf-auth-pass') || {}).checked ? 'Password' : 'Key';
    var keyPath = fieldVal('srvf-key-path');
    var passNode = document.getElementById('srvf-password');
    var newPass = passNode ? passNode.value : '';

    // 缺项聚合提示:一次告知所有未填的必填项
    var missing = [];
    if (!name) missing.push('名称');
    if (!host) missing.push('主机地址');
    if (!username) missing.push('用户名');
    if (!remoteDir) missing.push('远程部署目录');
    if (missing.length > 0) return fail('请填写:' + missing.join('、'));
    if (!/^\d+$/.test(portRaw) || Number(portRaw) < 1 || Number(portRaw) > 65535) {
      return fail('端口需为 1 - 65535 之间的整数');
    }
    if (authType === 'Key' && !keyPath) return fail('私钥认证需填写私钥路径');

    var prevAuth = (prev && prev.auth) ? prev.auth : {};
    var hasSavedPassword = authType === 'Password' && !!prevAuth.password_enc;
    if (authType === 'Password' && !newPass && !hasSavedPassword) {
      return fail('密码认证需填写登录密码');
    }
    setSaving(true);

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

    var savedId = null;
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
        savedId = server.id;
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
        window.toast('已保存,正在测试连接…', 'ok');
        return loadConfig();
      })
      .then(function () {
        // 保存成功后自动发起一次「测试连接」,结果呈现在服务器卡片上
        if (savedId) runEnvCheck({ id: savedId }, 'test');
      })
      .catch(function (err) {
        fail(errText(err) || '保存失败');
        saveToastFail(err, '保存失败');
        setSaving(false);
      });
  }

  // ===== 项目编辑表单 =====

  function openProjectModal(project) {
    var prev = project || null;
    importPreview = { path: '', stack: null, autoName: '' }; // 每次打开表单重置导入预览状态

    openModal(prev ? '编辑项目' : '新增项目', function (body) {
      appendErrorBox(body, 'prjf-error');

      appendField(body, '名称', 'prjf-name', 'text',
        prev ? prev.name : '', '如:我的应用');

      if (!prev) {
        appendImportBlock(body);
      }

      appendField(body, '镜像过滤关键字', 'prjf-filter', 'text',
        prev ? prev.image_filter : '', '如:myapp', '部署时按该关键字匹配本地镜像仓库名,留空匹配全部镜像');
      var composeInput = appendField(body, 'compose 文件相对路径', 'prjf-compose', 'text',
        prev ? prev.compose_file : '', '如:docker-compose.yml', '相对远程部署目录的路径');
      if (!prev) {
        // 走导入流程时 compose 相对路径不再参与保存,置灰防误解
        composeInput.disabled = !!fieldVal('prjf-import-path');
      }

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

      // ===== 生产加固(Task 7):健康检查 / pre-post 钩子 / webhook =====
      appendField(body, '健康检查', 'prjf-health-wait', 'number',
        prev ? (prev.health_wait_secs || 0) : 0, '',
        '部署后轮询容器状态的最长等待秒数,0 为关闭',
        { min: '0', max: '86400', step: '1' });

      appendHookBlock(body, 'pre-deploy 命令', 'prjf-pre-cmd', PRESET_CMDS.pre,
        prev && prev.pre_deploy_cmd ? String(prev.pre_deploy_cmd) : '');
      appendHookBlock(body, 'post-deploy 命令', 'prjf-post-cmd', PRESET_CMDS.post,
        prev && prev.post_deploy_cmd ? String(prev.post_deploy_cmd) : '');
      // 两个钩子块共用的 chips 说明
      var hookHint = el('div', 'form-row');
      hookHint.appendChild(el('div', 'form-hint',
        '预设仅为模板,点击插入后可修改;留空表示不执行钩子。pre 失败将中止部署'));
      body.appendChild(hookHint);

      appendField(body, '完成通知 webhook', 'prjf-webhook', 'text',
        prev && prev.notify_webhook ? String(prev.notify_webhook) : '',
        'https://hook.example.com/xxx',
        '部署结束后 POST JSON 结果;留空关闭');

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

  /** 收集文件映射编辑行;行校验失败时已 formFail 提示并返回 null */
  function collectMappings(errId) {
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
        formFailLoud(errId,
          '文件映射第 ' + (i + 1) + ' 行需同时填写本地路径与服务器相对路径');
        return null;
      }
      mappings.push({
        local: local,
        remote: remote,
        is_dir: !!(dirNode && dirNode.checked)
      });
    }
    return mappings;
  }

  /** 项目表单保存:校验 →(导入流程:preview 校验 + import_compose)→ get_config → 全量写回 */
  function saveProject(prev) {
    formClearError('prjf-error');

    var name = fieldVal('prjf-name');
    var filter = fieldVal('prjf-filter');
    var compose = fieldVal('prjf-compose');
    var importPath = fieldVal('prjf-import-path'); // 编辑表单无此输入框,得空串

    // 缺项聚合提示:一次告知所有未填的必填项
    var missing = [];
    if (!name) missing.push('名称');
    if (!importPath && !compose) missing.push('compose 文件相对路径');
    if (missing.length > 0) return formFailLoud('prjf-error', '请填写:' + missing.join('、'));

    var mappings = collectMappings('prjf-error');
    if (mappings === null) return false;

    // 生产加固字段:健康等待秒数 / pre-post 钩子 / webhook(编辑与新建都要带上)
    var extras = collectProjectExtras('prjf-error');
    if (extras === null) return false;

    // 导入流程:路径非空时校验解析(有未解决问题则阻止保存)→ import_compose 建项目
    if (importPath) {
      saveProjectViaImport(name, filter, importPath, mappings, extras);
      return false;
    }

    window.AppBus.invoke('get_config')
      .then(function (cfg) {
        cfg = normalizeCfg(cfg);
        var pid = (prev && prev.id) ? prev.id : uuid();
        var idx = -1;
        for (var j = 0; j < cfg.projects.length; j++) {
          if (cfg.projects[j].id === pid) { idx = j; break; }
        }
        if (idx >= 0) {
          // 编辑:在配置中的原条目上就地改表单承载的字段,保留 service_overrides
          // 等表单未承载的字段(整对象替换会经 serde(default) 把它们清空)
          cfg.projects[idx].name = name;
          cfg.projects[idx].image_filter = filter;
          cfg.projects[idx].compose_file = compose;
          cfg.projects[idx].file_mappings = mappings;
          cfg.projects[idx].health_wait_secs = extras.health_wait_secs;
          cfg.projects[idx].pre_deploy_cmd = extras.pre_deploy_cmd;
          cfg.projects[idx].post_deploy_cmd = extras.post_deploy_cmd;
          cfg.projects[idx].notify_webhook = extras.notify_webhook;
        } else {
          cfg.projects.push({
            id: pid,
            name: name,
            image_filter: filter,
            compose_file: compose,
            file_mappings: mappings,
            service_overrides: [],
            health_wait_secs: extras.health_wait_secs,
            pre_deploy_cmd: extras.pre_deploy_cmd,
            post_deploy_cmd: extras.post_deploy_cmd,
            notify_webhook: extras.notify_webhook
          });
        }
        return window.AppBus.invoke('save_config_cmd', { cfg: cfg });
      })
      .then(function () {
        closeModal();
        window.toast('已保存', 'ok');
        return loadConfig();
      })
      .catch(function (err) {
        formFailLoud('prjf-error', errText(err) || '保存失败');
        saveToastFail(err, '保存失败');
      });
  }

  /**
   * 导入流程保存:
   * 1. preview_compose 复核(已有同路径成功预览则直接复用);解析存在
   *    errors(如服务既无 image 也无 build)时阻止保存并显示错误;
   * 2. import_compose 复制 compose 到配置目录,返回新 ProjectConfig
   *    (含 id / compose_file 副本路径 / service_overrides 默认分类);
   * 3. 并入项目列表,补齐表单中的名称/镜像过滤/文件映射后 save_config_cmd 全量写回。
   */
  function saveProjectViaImport(name, filter, importPath, mappings, extras) {
    var ensured = (importPreview.stack && importPreview.path === importPath)
      ? Promise.resolve(importPreview.stack)
      : window.AppBus.invoke('preview_compose', { sourcePath: importPath });

    ensured
      .then(function (stack) {
        var errors = stack && Array.isArray(stack.errors) ? stack.errors : [];
        if (errors.length > 0) {
          formFailLoud('prjf-error', 'compose 存在未解决问题,已阻止保存:' + errors.join(';'));
          return null;
        }
        return window.AppBus.invoke('import_compose', { sourcePath: importPath, name: name })
          .then(function (imported) {
            if (!imported || !imported.id) {
              throw new Error('导入结果异常(缺少项目 id)');
            }
            var merged = {
              id: String(imported.id),
              name: name,
              image_filter: filter,
              compose_file: String(imported.compose_file || ''),
              file_mappings: mappings,
              service_overrides: Array.isArray(imported.service_overrides)
                ? imported.service_overrides
                : [],
              health_wait_secs: extras.health_wait_secs,
              pre_deploy_cmd: extras.pre_deploy_cmd,
              post_deploy_cmd: extras.post_deploy_cmd,
              notify_webhook: extras.notify_webhook
            };
            return window.AppBus.invoke('get_config').then(function (cfg) {
              cfg = normalizeCfg(cfg);
              var idx = -1;
              for (var i = 0; i < cfg.projects.length; i++) {
                if (cfg.projects[i].id === merged.id) { idx = i; break; }
              }
              // import_compose 已把项目写入配置:此处覆盖为补齐表单字段后的版本
              if (idx >= 0) cfg.projects[idx] = merged;
              else cfg.projects.push(merged);
              return window.AppBus.invoke('save_config_cmd', { cfg: cfg })
                .then(function () { return true; });
            });
          });
      })
      .then(function (done) {
        if (done !== true) return; // 被校验阻止,错误已显示
        closeModal();
        window.toast('已导入 compose 并保存项目', 'ok');
        return loadConfig();
      })
      .catch(function (err) {
        formFailLoud('prjf-error', errText(err) || '导入失败');
        saveToastFail(err, '导入失败');
      });
  }

  // ===== 运行日志面板(底部折叠,默认收起)=====

  function setLogOpen(open) {
    var body = document.getElementById('servers-log-body');
    var btn = document.getElementById('servers-log-toggle');
    if (!body || !btn) return;
    body.classList.toggle('hidden', !open);
    btn.textContent = open ? '− 运行日志' : '+ 运行日志(安装/检测输出在此显示)';
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
