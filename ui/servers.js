/* ============================================================
 * servers.js — 服务器管理页逻辑(依赖 app.js 提供的全局工具)
 *
 * 后端命令(对象字段为 Rust snake_case 原样序列化,JS 参数名 camelCase):
 * - get_config() -> AppConfig { servers: Server[], projects: Project[] }
 *   Server  = { id, name, host, port, username, remote_dir,
 *               host_key_sha256,          // 主机密钥 TOFU 指纹(阶段三,null=未记录)
 *               auth: { auth_type: "Key"|"Password", key_path, password_enc,
 *                       key_pass_enc } }  // key_pass_enc=DPAPI 加密的私钥口令(阶段三)
 *   Project = { id, name, image_filter, compose_file,
 *               file_mappings: [{ local, remote, is_dir }], service_overrides,
 *               health_wait_secs, pre_deploy_cmd, post_deploy_cmd, notify_webhook }
 * - save_config_cmd({ cfg })                全量保存配置
 * - encrypt_password({ plain }) -> string   base64 密文,存 auth.password_enc
 * - test_server({ serverId, passwordPlain?, keyPassphrase?, rememberKeyPass? })
 *     keyPassphrase = 一次性私钥口令(优先于已存 auth.key_pass_enc);
 *     rememberKeyPass = 勾选「记住口令」时透传 true,后端在连接成功且口令
 *     非空时才加密保存(是否满足持久化条件由后端判定,前端仅透传)
 * - server_env_check({ serverId, passwordPlain?, keyPassphrase? })
 *     -> ServerCheckReport { docker, compose, gzip, remote_dir_exists,
 *                            disk_free_gb, errors: string[] }
 *     (两者等价:连接 + 远端环境检测;passwordPlain 一般不传,后端用 DPAPI
 *      解密已存密文;auth_type=Key 时后端忽略 passwordPlain)
 * - retrust_host_key({ serverId })          重置主机密钥 TOFU 指纹(清空
 *     host_key_sha256,下次连接重新接受并记录;服务器重装/换 IP 后调用)
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
    projectSources: null, // projectId -> ProjectSourceStatus(源 compose 变更检测缓存)
    sourceChecked: false, // 本次启动是否已做过源比对(启动自动比对只跑一次)
    sourceChecking: false,// 源比对进行中(防并发重入导致重复更新)
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
        // 启动自动比对源 compose(仅本次启动首次加载触发;可在设置中心关闭)
        if (st.loaded && !st.sourceChecked) {
          st.sourceChecked = true;
          window.AppBus.invoke('app_settings_get')
            .then(function (s) {
              var enabled = !s || s.autoUpdateFromSource !== false;
              return checkProjectSources(enabled);
            })
            .catch(function () { /* 设置读取失败:静默跳过自动比对 */ });
        }
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
    // 关联项目(第四批):列出把本服务器设为默认的项目,一眼看清归属
    if (st.cfg && Array.isArray(st.cfg.projects)) {
      var owned = st.cfg.projects.filter(function (p) {
        return String(p.default_server_id || '') === String(server.id);
      });
      if (owned.length > 0) {
        kv.appendChild(kvPair('关联项目 PROJECTS',
          owned.map(function (p) { return String(p.name); }).join('、')));
      }
    }
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
  /** 「清理优化」:打开清理分析模态(预览 → 勾选 → 定向执行) */
  function showPruneConfirm(card, server) {
    openCleanupModal(server);
  }

  function cleanupModal() { return document.getElementById('cleanup-modal'); }
  function cleanupBody() { return document.getElementById('cleanup-modal-body'); }

  function closeCleanupModal() {
    var m = cleanupModal();
    if (m) m.classList.add('hidden');
    // 模态关闭即解除卡片 busy(执行标志由关闭时统一复位)
    st.pruning[st.cleanupServerId || ''] = false;
    st.cleanupServerId = null;
    if (!document.querySelector('.page.active[data-page="servers"]')) return;
    renderServers();
  }

  /** 打开清理分析:先 cleanup_preview,加载中显示占位 */
  function openCleanupModal(server) {
    var modal = cleanupModal();
    if (!modal) return;
    st.cleanupServerId = server.id;
    // 扫描起点:默认服务器部署目录;每次打开重置为该默认值
    st.cleanupScanRoot = server.remote_dir || '';
    // 记住本次报告:执行时按它把勾选项翻译成显式目标列表
    st.cleanupReport = null;
    modal.classList.remove('hidden');
    var body = cleanupBody();
    body.innerHTML = '';
    var title = document.getElementById('cleanup-modal-title');
    if (title) title.textContent = '清理分析 — ' + (server.name || server.host);
    loadCleanupPreview(server);
  }

  /** 拉取预览(可带自定义扫描起点)并渲染 */
  function loadCleanupPreview(server) {
    var body = cleanupBody();
    if (!body) return;
    body.innerHTML = '';
    body.appendChild(el('div', 'cleanup-hint', '正在扫描可清理项…'));
    window.AppBus.invoke('cleanup_preview', {
      serverId: server.id,
      scanRoot: st.cleanupScanRoot || undefined
    })
      .then(function (report) {
        if (st.cleanupServerId !== server.id) return; // 模态已被关闭/切换
        st.cleanupReport = report || {};
        renderCleanupSections(server, st.cleanupReport);
      })
      .catch(function (err) {
        if (st.cleanupServerId !== server.id) return;
        body.innerHTML = '';
        body.appendChild(el('div', 'cleanup-hint',
          '扫描失败:' + (err && err.message ? err.message : err)));
      });
  }

  /** 扫描起点输入区 + 「重新扫描」按钮 */
  function buildCleanupScanBar(server) {
    var bar = el('div', 'cleanup-scanbar');
    var label = el('span', 'cleanup-scanbar-label', '扫描起点:');
    bar.appendChild(label);
    var input = document.createElement('input');
    input.className = 'form-input cleanup-scanbar-input';
    input.type = 'text';
    input.id = 'cleanup-scan-root';
    input.value = st.cleanupScanRoot || '';
    input.placeholder = '如 /home(留空则用服务器部署目录)';
    input.autocomplete = 'off';
    bar.appendChild(input);
    var btn = el('button', 'btn btn-sm', '重新扫描');
    btn.type = 'button';
    btn.addEventListener('click', function () {
      var v = (input.value || '').trim();
      st.cleanupScanRoot = v || (server.remote_dir || '');
      loadCleanupPreview(server);
    });
    bar.appendChild(btn);
    return bar;
  }

  /** 诊断折叠区:逐条命令的退出码与输出摘要(排查"识别不到"用) */
  function buildCleanupDiag(diags) {
    if (!Array.isArray(diags) || diags.length === 0) return null;
    var wrap = el('details', 'cleanup-diag');
    var sum = el('summary', 'cleanup-diag-summary',
      '扫描诊断(' + diags.length + ' 条命令)');
    wrap.appendChild(sum);
    for (var i = 0; i < diags.length; i++) {
      var d = diags[i];
      var codeText = (d.exitCode === null || d.exitCode === undefined)
        ? '传输失败' : ('退出码 ' + d.exitCode);
      wrap.appendChild(el('div', 'cleanup-diag-row',
        d.label + ' — ' + codeText));
      wrap.appendChild(el('div', 'cleanup-diag-cmd mono', d.cmd || ''));
      if (d.output) {
        wrap.appendChild(el('pre', 'cleanup-diag-out mono', d.output));
      }
    }
    return wrap;
  }

  /** 渲染各清理节的勾选与明细(含分项目区块) */
  function renderCleanupSections(server, report) {
    var body = cleanupBody();
    if (!body) return;
    body.innerHTML = '';

    body.appendChild(buildCleanupScanBar(server));

    var errList = Array.isArray(report.errors) ? report.errors : [];
    for (var ei = 0; ei < errList.length; ei++) {
      body.appendChild(el('div', 'cleanup-error', errList[ei]));
    }
    var warnList = Array.isArray(report.warnings) ? report.warnings : [];
    if (warnList.length > 0) {
      var warnBox = el('details', 'cleanup-warn');
      warnBox.appendChild(el('summary', 'cleanup-warn-summary',
        '提示(' + warnList.length + ' 条)'));
      for (var wi = 0; wi < warnList.length; wi++) {
        warnBox.appendChild(el('div', 'cleanup-warn-row', warnList[wi]));
      }
      body.appendChild(warnBox);
    }

    var dangling = report.danglingImages || [];
    var stopped = report.stoppedContainers || [];
    var volumes = report.unusedVolumes || [];
    var projects = report.projects || [];

    var sections = [
      {
        key: 'images', label: '无标签镜像', disabledLabel: '在用',
        count: dangling.length,
        items: dangling.map(function (im) {
          return (im.repository || '<none>') + ':' + (im.tag || '<none>') +
            ' (' + (im.size || '?') + ')' + (im.inUse ? ' — 使用中,不可删' : '');
        }),
        // 在用的禁选
        selectable: dangling.filter(function (im) { return !im.inUse; }).length
      },
      {
        key: 'containers', label: '已停止容器',
        count: stopped.length,
        items: stopped.map(function (c) { return (c.names || c.id) + ' — ' + (c.status || ''); }),
        selectable: stopped.length
      },
      {
        key: 'volumes', label: '未使用卷',
        count: volumes.length,
        items: volumes.map(function (v) { return v.name || ''; }),
        selectable: volumes.length
      }
    ];

    for (var i = 0; i < sections.length; i++) {
      var sec = sections[i];
      var box = el('div', 'cleanup-section');
      var head = el('div', 'cleanup-section-head');
      var chk = el('input');
      chk.type = 'checkbox';
      chk.setAttribute('data-cleanup-section', sec.key);
      chk.disabled = sec.selectable === 0;
      chk.checked = sec.selectable > 0;
      head.appendChild(chk);
      head.appendChild(el('span', 'cleanup-section-label', sec.label));
      var countText = sec.count + ' 项';
      if (sec.count !== sec.selectable) {
        countText += '(' + (sec.count - sec.selectable) + ' 项在用)';
      }
      head.appendChild(el('span', 'cleanup-section-count', countText));
      box.appendChild(head);

      if (sec.count > 0 && sec.items.length > 0) {
        var shown = sec.items.slice(0, 5);
        for (var k = 0; k < shown.length; k++) {
          box.appendChild(el('div', 'cleanup-item mono', shown[k]));
        }
        if (sec.items.length > 5) {
          box.appendChild(el('div', 'cleanup-item cleanup-more', '… 共 ' + sec.items.length + ' 项'));
        }
      }
      body.appendChild(box);
    }

    // 构建缓存
    var cacheSize = report.buildCacheSize || '0B';
    var cacheBox = el('div', 'cleanup-section');
    var cacheHead = el('div', 'cleanup-section-head');
    var cacheChk = el('input');
    cacheChk.type = 'checkbox';
    cacheChk.setAttribute('data-cleanup-section', 'builder');
    cacheChk.disabled = cacheSize === '0B' || cacheSize === '';
    cacheChk.checked = !cacheChk.disabled;
    cacheHead.appendChild(cacheChk);
    cacheHead.appendChild(el('span', 'cleanup-section-label', '构建缓存'));
    cacheHead.appendChild(el('span', 'cleanup-section-count', cacheSize));
    cacheBox.appendChild(cacheHead);
    body.appendChild(cacheBox);

    // ===== 分项目区块 =====
    if (projects.length > 0) {
      body.appendChild(el('div', 'cleanup-section-group-title',
        '分项目可清理项(按服务器实际目录)'));
      for (var pi = 0; pi < projects.length; pi++) {
        body.appendChild(buildCleanupProjectBox(projects[pi], pi));
      }
    } else if (report.scanRoot) {
      body.appendChild(el('div', 'cleanup-hint',
        '在「' + report.scanRoot + '」下未扫描到含 compose 文件的项目目录。' +
        '可修改扫描起点(如 /home)后重新扫描。'));
    }

    var diag = buildCleanupDiag(report.diagnostics);
    if (diag) body.appendChild(diag);

    body.appendChild(el(
      'div', 'cleanup-hint',
      '清理仅移除未被使用的资源(运行中容器与其挂载卷不受影响);' +
      '构建缓存清理为 docker 自判范围。执行过程输出见底部「运行日志」。'
    ));

    var actions = el('div', 'modal-actions');
    var execBtn = el('button', 'btn btn-danger', '执行清理');
    execBtn.type = 'button';
    execBtn.addEventListener('click', function () { onCleanupExecute(server); });
    actions.appendChild(execBtn);
    body.appendChild(actions);
    refreshCleanupExecState(execBtn);
    body.querySelectorAll('input[data-cleanup-section], input[data-cleanup-project]')
      .forEach(function (node) {
        node.addEventListener('change', function () { refreshCleanupExecState(execBtn); });
      });
    st.cleanupExecBtn = execBtn;
  }

  /** 单个项目的清理区块:占用 + 归档勾选 + 旧标签勾选 */
  function buildCleanupProjectBox(prj, index) {
    var box = el('div', 'cleanup-project');
    var head = el('div', 'cleanup-project-head');
    head.appendChild(el('span', 'cleanup-project-dir mono', prj.dir || ''));
    head.appendChild(el('span', 'cleanup-project-size', prj.size || '?'));
    if (prj.appProject) {
      head.appendChild(el('span', 'cleanup-project-badge', '已配置:' + prj.appProject));
    }
    box.appendChild(head);
    if (prj.composeFile) {
      box.appendChild(el('div', 'cleanup-item mono', prj.composeFile));
    }

    // 归档:按该项目配置的保留数量(第五批;未配置 = 默认 5 个),只勾选更早的
    var releases = prj.releases || [];
    var keep = (typeof prj.releaseKeep === 'number' && prj.releaseKeep >= 0)
      ? prj.releaseKeep : 5;
    var prunable = releases.slice(keep);
    var relRow = el('div', 'cleanup-project-row');
    var relChk = el('input');
    relChk.type = 'checkbox';
    relChk.setAttribute('data-cleanup-project', String(index));
    relChk.setAttribute('data-cleanup-kind', 'releases');
    relChk.disabled = prunable.length === 0;
    relChk.checked = prunable.length > 0;
    relRow.appendChild(relChk);
    relRow.appendChild(el('span', 'cleanup-project-label',
      '旧发布归档:' + prunable.length + ' 个可清理(共 ' + releases.length +
      ' 个,保留最新 ' + keep + ' 个' +
      (prj.appProject ? '' : ';该项目未在软件内配置,按默认 5 个') + ')'));
    box.appendChild(relRow);
    var relShow = prunable.slice(0, 3);
    for (var ri = 0; ri < relShow.length; ri++) {
      box.appendChild(el('div', 'cleanup-item mono', relShow[ri]));
    }
    if (prunable.length > 3) {
      box.appendChild(el('div', 'cleanup-item cleanup-more',
        '… 共 ' + prunable.length + ' 个待清理'));
    }

    // 旧日期标签镜像:在用禁选
    var tagImages = prj.tagImages || [];
    var freeTags = tagImages.filter(function (t) { return !t.inUse; });
    var tagRow = el('div', 'cleanup-project-row');
    var tagChk = el('input');
    tagChk.type = 'checkbox';
    tagChk.setAttribute('data-cleanup-project', String(index));
    tagChk.setAttribute('data-cleanup-kind', 'tags');
    tagChk.disabled = freeTags.length === 0;
    tagChk.checked = freeTags.length > 0;
    tagRow.appendChild(tagChk);
    var tagText = '旧版本镜像:' + freeTags.length + ' 个可清理';
    if (tagImages.length !== freeTags.length) {
      tagText += '(' + (tagImages.length - freeTags.length) + ' 个使用中)';
    }
    tagRow.appendChild(el('span', 'cleanup-project-label', tagText));
    box.appendChild(tagRow);
    var tagShow = freeTags.slice(0, 3);
    for (var ti = 0; ti < tagShow.length; ti++) {
      box.appendChild(el('div', 'cleanup-item mono',
        tagShow[ti].reference + ' (' + (tagShow[ti].size || '?') + ')'));
    }
    if (freeTags.length > 3) {
      box.appendChild(el('div', 'cleanup-item cleanup-more',
        '… 共 ' + freeTags.length + ' 个待清理'));
    }
    return box;
  }

  /** 无勾选(或勾选项全无可删目标)时禁用执行按钮 */
  function refreshCleanupExecState(execBtn) {
    if (!execBtn) return;
    var any = false;
    document.querySelectorAll('#cleanup-modal-body input[data-cleanup-section]')
      .forEach(function (n) {
        if (n.checked && !n.disabled) any = true;
      });
    document.querySelectorAll('#cleanup-modal-body input[data-cleanup-project]')
      .forEach(function (n) {
        if (n.checked && !n.disabled) any = true;
      });
    execBtn.disabled = !any || st.pruning[st.cleanupServerId || ''] === true;
  }

  /** 把当前勾选状态翻译为后端要的显式目标列表(写什么删什么) */
  function collectCleanupSelection() {
    var report = st.cleanupReport || {};
    var sel = {
      images: false, containers: false, volumes: false, builder: false,
      imageIds: [], containerIds: [], volumeNames: [], projects: []
    };
    var checked = {};
    document.querySelectorAll('#cleanup-modal-body input[data-cleanup-section]')
      .forEach(function (n) { checked[n.getAttribute('data-cleanup-section')] = n.checked; });

    // 无标签镜像:只取未被容器引用的
    if (checked.images) {
      sel.images = true;
      sel.imageIds = (report.danglingImages || [])
        .filter(function (im) { return !im.inUse; })
        .map(function (im) { return im.id; });
    }
    if (checked.containers) {
      sel.containers = true;
      sel.containerIds = (report.stoppedContainers || []).map(function (c) { return c.id; });
    }
    if (checked.volumes) {
      sel.volumes = true;
      sel.volumeNames = (report.unusedVolumes || []).map(function (v) { return v.name; });
    }
    sel.builder = !!checked.builder;

    // 分项目:按项目索引收集勾选的归档与标签
    var byProject = {};
    document.querySelectorAll('#cleanup-modal-body input[data-cleanup-project]')
      .forEach(function (n) {
        var idx = parseInt(n.getAttribute('data-cleanup-project'), 10);
        if (isNaN(idx)) return;
        var kind = n.getAttribute('data-cleanup-kind');
        if (!byProject[idx]) byProject[idx] = { releases: false, tags: false };
        byProject[idx][kind] = n.checked && !n.disabled;
      });
    var projects = report.projects || [];
    Object.keys(byProject).forEach(function (key) {
      var idx = parseInt(key, 10);
      var prj = projects[idx];
      if (!prj) return;
      var want = byProject[key];
      var target = { dir: prj.dir || '', releaseDirs: [], imageRefs: [] };
      if (want.releases) {
        // 与 UI 一致:按项目配置的保留数量(默认 5)取更早的部分
        var keepN = (typeof prj.releaseKeep === 'number' && prj.releaseKeep >= 0)
          ? prj.releaseKeep : 5;
        target.releaseDirs = (prj.releases || []).slice(keepN);
      }
      if (want.tags) {
        target.imageRefs = (prj.tagImages || [])
          .filter(function (t) { return !t.inUse; })
          .map(function (t) { return t.reference; });
      }
      if (target.releaseDirs.length > 0 || target.imageRefs.length > 0) {
        sel.projects.push(target);
      }
    });
    return sel;
  }

  /** 执行清理:模态内二次确认 → cleanup_execute → 逐节结果 → 自动重新预览 */
  function onCleanupExecute(server) {
    var body = cleanupBody();
    if (!body) return;
    var selection = collectCleanupSelection();

    var totalOps = selection.imageIds.length + selection.containerIds.length +
      selection.volumeNames.length + (selection.builder ? 1 : 0);
    var projOps = 0;
    for (var pi = 0; pi < selection.projects.length; pi++) {
      projOps += selection.projects[pi].releaseDirs.length +
        selection.projects[pi].imageRefs.length;
    }

    body.innerHTML = '';
    var confirmText = '确认清理勾选项?该操作不可撤销。' +
      '(无标签镜像 ' + selection.imageIds.length + ' 项、停止容器 ' +
      selection.containerIds.length + ' 个、未使用卷 ' + selection.volumeNames.length +
      ' 个' + (selection.builder ? '、构建缓存' : '') +
      (projOps > 0 ? '、分项目 ' + projOps + ' 项' : '') + ')';
    body.appendChild(el('div', 'cleanup-hint', confirmText));
    var actions = el('div', 'modal-actions');
    var ok = el('button', 'btn btn-danger', '确认执行');
    ok.type = 'button';
    var cancel = el('button', 'btn', '返回');
    cancel.type = 'button';
    actions.appendChild(cancel);
    actions.appendChild(ok);
    body.appendChild(actions);
    if (totalOps + projOps === 0) {
      ok.disabled = true;
    }

    cancel.addEventListener('click', function () {
      if (st.cleanupServerId !== server.id) return;
      renderCleanupSections(server, st.cleanupReport || {});
    });
    ok.addEventListener('click', function () {
      st.pruning[server.id] = true;
      body.innerHTML = '';
      body.appendChild(el('div', 'cleanup-hint', '清理执行中,输出见底部「运行日志」…'));
      setLogOpen(true);
      refreshCleanupExecState(st.cleanupExecBtn);
      window.AppBus.invoke('cleanup_execute', { serverId: server.id, sections: selection })
        .then(function (results) {
          if (st.cleanupServerId !== server.id) return;
          st.pruning[server.id] = false;
          var list = Array.isArray(results) ? results : [];
          body.innerHTML = '';
          body.appendChild(el('div', 'cleanup-hint', '清理完成:'));
          for (var i = 0; i < list.length; i++) {
            var r = list[i];
            var line = r.label + ':' + (r.ok ? '完成' : '失败') +
              (r.output ? ' — ' + r.output.split('\n')[0] : '');
            body.appendChild(el('div', r.ok ? 'cleanup-item' : 'cleanup-error', line));
          }
          var again = el('button', 'btn btn-sm', '重新扫描');
          again.type = 'button';
          again.addEventListener('click', function () {
            if (st.cleanupServerId !== server.id) return;
            loadCleanupPreview(server);
          });
          body.appendChild(again);
          refreshCleanupExecState(st.cleanupExecBtn);
        })
        .catch(function (err) {
          if (st.cleanupServerId !== server.id) return;
          st.pruning[server.id] = false;
          body.innerHTML = '';
          body.appendChild(el('div', 'cleanup-error',
            '清理失败:' + (err && err.message ? err.message : err)));
        });
    });
  }

  // ===== 远端操作(test / env check / install / mkdir)=====

  /**
   * 连接 + 环境检测。
   * @param {Object} server 服务器对象(仅使用其 id)
   * @param {string} mode 'test' = test_server;'env' = server_env_check(两者等价)
   * @param {Object} [extras] 可选透传参数(服务器表单保存后的自动测试使用):
   *   { keyPassphrase: string, rememberKeyPass: boolean }
   *   keyPassphrase = 一次性私钥口令(仅非空时携带);
   *   rememberKeyPass = 「记住口令」勾选状态(仅 test_server 支持,后端在
   *   连接成功且口令非空时才持久化,前端不判空直接透传)
   */
  function runEnvCheck(server, mode, extras) {
    var id = server.id;
    if (st.checking[id] || st.installing[id] || st.pruning[id]) return;
    st.checking[id] = true;
    renderServers();

    var cmd = mode === 'test' ? 'test_server' : 'server_env_check';
    var args = { serverId: id };
    // passwordPlain 不传:后端用 DPAPI 解密已存密码;Key 认证时后端忽略该参数
    if (extras && extras.keyPassphrase) args.keyPassphrase = extras.keyPassphrase;
    if (cmd === 'test_server' && extras && extras.rememberKeyPass) {
      args.rememberKeyPass = true;
    }
    window.AppBus.invoke(cmd, args)
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

  /**
   * 重新信任主机密钥(retrust_host_key):清空 host_key_sha256,下次连接
   * 重新接受并记录当前指纹(服务器重装/换 IP 后由用户显式调用)。
   * 成功后表单内原地刷新为「无指纹」态,并重载配置刷新列表卡片。
   */
  function retrustHostKey(serverId, fpInput, btn) {
    if (btn.disabled) return;
    btn.disabled = true;
    btn.textContent = '处理中…';
    window.AppBus.invoke('retrust_host_key', { serverId: serverId })
      .then(function () {
        window.toast('已重置主机密钥信任,下次连接将重新记录指纹', 'ok');
        if (fpInput) fpInput.value = '';
        btn.classList.add('hidden'); // 指纹已清空:隐藏「重新信任」,灰字提示由 placeholder 呈现
        return loadConfig();
      })
      .catch(function (err) {
        window.toast('重新信任失败:' + (errText(err) || '未知错误'), 'fail');
        btn.disabled = false;
        btn.textContent = '重新信任';
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
      emptyRow(tbody, 7, '配置加载失败,请点击上方重试');
      return;
    }
    if (st.cfg.projects.length === 0) {
      emptyRow(tbody, 7, '暂无部署项目,点击上方「新增项目」添加');
      return;
    }
    st.cfg.projects.forEach(function (project) {
      var tr = document.createElement('tr');

      var nameTd = document.createElement('td');
      nameTd.className = 'nowrap';
      nameTd.textContent = String(project.name);
      // 源状态徽章(第三批):由 st.projectSources 缓存驱动(启动/手动检查时填充)。
      // - changed → 源已变更(可更新)
      // - missing → 源文件丢失
      // - unbound 且 imported → 导入项目未绑定源(点「绑定源」补上)
      var srcState = st.projectSources ? st.projectSources[project.id] : null;
      if (srcState && srcState.state === 'changed') {
        // fillBadge 统一 badge-ok/fail/warn 类名(手写 'badge fail' 不匹配 CSS)
        var badge = window.fillBadge(el('span', 'src-badge'), 'fail', '源已变更');
        badge.title = srcState.detail || '';
        nameTd.appendChild(document.createTextNode(' '));
        nameTd.appendChild(badge);
      } else if (srcState && srcState.state === 'missing') {
        var badgeMiss = window.fillBadge(el('span', 'src-badge'), 'fail', '源文件丢失');
        badgeMiss.title = srcState.detail || '';
        nameTd.appendChild(document.createTextNode(' '));
        nameTd.appendChild(badgeMiss);
      } else if (srcState && srcState.state === 'unbound' && srcState.imported) {
        var badgeUnbound = window.fillBadge(el('span', 'src-badge'), 'warn', '未绑定源');
        badgeUnbound.title = srcState.detail || '尚未绑定源 compose,无法检测变更';
        nameTd.appendChild(document.createTextNode(' '));
        nameTd.appendChild(badgeUnbound);
      }
      tr.appendChild(nameTd);

      // 服务器(第四批):显示默认服务器名;未指定/服务器已删除时给出明确提示。
      // 便于一眼看出"哪个项目在哪台服务器",不必靠记忆。
      var srvTd = document.createElement('td');
      srvTd.className = 'nowrap';
      var srvMatch = null;
      if (project.default_server_id && st.cfg) {
        srvMatch = st.cfg.servers.filter(function (s) {
          return String(s.id) === String(project.default_server_id);
        })[0] || null;
      }
      if (srvMatch) {
        srvTd.textContent = String(srvMatch.name || srvMatch.host);
      } else if (project.default_server_id) {
        srvTd.appendChild(el('span', 'none-text', '(服务器已删除)'));
      } else {
        srvTd.appendChild(el('span', 'none-text', '(未指定)'));
      }
      tr.appendChild(srvTd);

      // 远程目录(第四批):项目级目录优先,留空显示继承自哪台服务器的目录
      var dirTd = document.createElement('td');
      dirTd.className = 'mono';
      if (project.remote_dir) {
        dirTd.textContent = String(project.remote_dir);
      } else if (srvMatch) {
        dirTd.appendChild(el('span', 'none-text', '继承 ' + String(srvMatch.remote_dir || '?')));
      } else {
        dirTd.appendChild(el('span', 'none-text', '继承服务器'));
      }
      tr.appendChild(dirTd);

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

      // 源操作按钮(第三批修复):导入项目一律显示,不再要求已绑定源。
      // - 已绑定 → 「从源更新」(重拷重解析)
      // - 未绑定 → 「绑定源」(选原 compose 建立比对基准)
      // 旧实现只在 source_compose_path 非空时显示按钮,导致 v5.4.0 之前
      // 导入的项目**没有任何入口**绑定源、也无从触发更新。
      if (srcState && srcState.imported) {
        var bound = !!srcState.bound || !!project.source_compose_path;
        var srcBtn = el('button', 'btn btn-sm', bound ? '从源更新' : '绑定源');
        srcBtn.type = 'button';
        srcBtn.title = bound
          ? '重新读取源 compose(.env 与 override 一并同步)并重解析,保留已保存的服务分类'
          : '选择一个本地 compose 文件作为该项目的更新源(用于检测变更并同步)';
        if (!bound) srcBtn.classList.add('btn-primary');
        srcBtn.addEventListener('click', function () {
          if (bound) updateProjectSource(project, srcBtn);
          else bindProjectSource(project, srcBtn);
        });
        actTd.appendChild(srcBtn);
        actTd.appendChild(document.createTextNode(' '));
      } else if (project.source_compose_path) {
        // 兜底:源检查尚未返回(如启动竞态)但配置里已有源 → 仍显示更新按钮
        var updBtn0 = el('button', 'btn btn-sm', '从源更新');
        updBtn0.type = 'button';
        updBtn0.title = '重新读取源 compose(.env 与 override 一并同步)并重解析,保留已保存的服务分类';
        updBtn0.addEventListener('click', function () { updateProjectSource(project, updBtn0); });
        actTd.appendChild(updBtn0);
        actTd.appendChild(document.createTextNode(' '));
      }

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

  // ===== 项目「从源更新」(第三批)=====

  /**
   * 检查所有项目的源 compose 是否变更(只读),结果缓存到 st.projectSources。
   * `auto` = true 时(启动自动比对)对 changed 的项目直接执行更新并汇总提示;
   * 否则只刷新徽章,等用户点「从源更新」。
   *
   * 进行中标记防重入:启动路径与页面切换可能并发触发,若不拦截会重复
   * 对同一项目发起更新(后端会重复备份与重解析)。
   */
  function checkProjectSources(auto) {
    if (st.sourceChecking) return Promise.resolve([]);
    st.sourceChecking = true;
    return window.AppBus.invoke('check_project_sources')
      .then(function (list) {
        var arr = Array.isArray(list) ? list : [];
        var map = {};
        for (var i = 0; i < arr.length; i++) map[arr[i].projectId] = arr[i];
        st.projectSources = map;
        renderProjects();
        renderSourceSummary(arr);

        if (!auto) return arr;

        // 启动自动更新:只处理「源已变更」的项目;missing/unknown 一律不动
        var changed = arr.filter(function (s) { return s.state === 'changed'; });
        if (changed.length === 0) return arr;

        var done = 0;
        var failed = [];
        var chain = Promise.resolve();
        changed.forEach(function (s) {
          chain = chain.then(function () {
            return window.AppBus.invoke('update_project_from_source', { projectId: s.projectId })
              .then(function () { done++; })
              .catch(function (err) {
                failed.push(s.projectName + ':' + (err && err.message ? err.message : err));
              });
          });
        });
        return chain.then(function () {
          if (done > 0) {
            window.toast('已从源更新 ' + done + ' 个项目(' + changed.map(function (s) {
              return s.projectName;
            }).join('、') + ')', 'ok');
            st.lastSourceResult = {
              name: changed.map(function (s) { return s.projectName; }).join('、'),
              state: 'unchanged',
              detail: '启动自动更新 ' + done + ' 个项目并重解析',
              ts: new Date().toTimeString().slice(0, 8)
            };
            // 更新后重新加载配置并复检(徽章恢复到"源未变更")
            return loadConfig().then(function () { return checkProjectSources(false); });
          }
          if (failed.length > 0) {
            window.toast('源自动更新失败:' + failed[0], 'fail');
          }
          return arr;
        });
      })
      .catch(function (err) {
        // 自动比对失败不打扰用户(配置可能尚未就绪);手动调用时提示
        if (!auto) {
          window.toast('检查源变更失败:' + (err && err.message ? err.message : err), 'fail');
        }
        return [];
      })
      .then(function (v) {
        st.sourceChecking = false;
        return v;
      });
  }

  /**
   * 手动「检查源变更」的结果反馈(第四批修复):toast 摘要 + 结果落汇总栏。
   *
   * 为什么单独做:此前手动检查只刷新徽章与汇总栏,而汇总栏在"没有导入项目"
   * 时会整块隐藏、也不弹 toast,用户点完看不到任何变化(反馈"点了没反应")。
   * 这里对各种情形都给出明确文案 —— 包括"没有可检查的项目"。
   */
  function reportSourceCheckResult(list) {
    var imported = list.filter(function (s) { return s.imported; });
    var changed = list.filter(function (s) { return s.state === 'changed'; });
    var missing = list.filter(function (s) { return s.state === 'missing'; });
    var unbound = list.filter(function (s) { return s.state === 'unbound' && s.imported; });

    var summary;
    if (list.length === 0) {
      summary = '没有可检查的项目';
    } else if (imported.length === 0) {
      // 手工项目(远端 compose 路径)无源可查 —— 明确说明,不让用户以为坏了
      summary = '没有可检查的导入项目(' + list.length + ' 个项目均为手工项目,无源文件可比对)';
    } else {
      var parts = [];
      if (changed.length > 0) {
        parts.push(changed.length + ' 个源已变更(' + changed.map(function (s) {
          return s.projectName;
        }).join('、') + ')');
      }
      if (unbound.length > 0) parts.push(unbound.length + ' 个未绑定源');
      if (missing.length > 0) parts.push(missing.length + ' 个源文件丢失');
      summary = parts.length > 0
        ? '检查完成:' + parts.join(';')
        : '检查完成:' + imported.length + ' 个导入项目的源均无变化';
    }

    var kind = (changed.length > 0 || missing.length > 0) ? 'warn' : 'ok';
    window.toast(summary, kind);

    // 结果同时落汇总栏(常驻可复查);无可检查项时也给一行,避免"看起来没反应"
    st.lastSourceResult = {
      name: '源检查',
      state: changed.length > 0 ? 'changed' : 'unchanged',
      detail: summary,
      ts: new Date().toTimeString().slice(0, 8)
    };
    renderSourceSummary(list, true);
  }

  /**
   * 源检查汇总栏(第三批):把比对结果常驻显示在项目表上方。
   *
   * 为什么需要它:更新结果此前只经 toast 一闪而过,用户反馈"没看到体现
   * 更新结果"。这里按状态聚合计数并列出需要动作的项目,结果可复查。
   *
   * `forceShow` = true(手动点「检查源变更」)时,即使没有导入项目也显示
   * 一行说明 —— 手动操作必须有可见反馈。
   */
  function renderSourceSummary(list, forceShow) {
    var box = document.getElementById('projects-src-summary');
    if (!box) return;
    var arr = Array.isArray(list) ? list : [];
    var changed = arr.filter(function (s) { return s.state === 'changed'; });
    var missing = arr.filter(function (s) { return s.state === 'missing'; });
    var unbound = arr.filter(function (s) { return s.state === 'unbound' && s.imported; });
    var readable = arr.filter(function (s) { return s.imported; });

    if (readable.length === 0 && !forceShow) {
      box.classList.add('hidden');
      box.textContent = '';
      return;
    }

    box.textContent = '';
    box.classList.remove('hidden');
    if (readable.length === 0) {
      // 手动检查但没有导入项目:给出明确说明而不是隐藏整块
      box.className = 'src-summary';
      box.appendChild(el('span', 'src-summary-text',
        arr.length === 0
          ? '源检查:没有可检查的项目'
          : '源检查:没有导入项目(' + arr.length + ' 个均为手工项目,无源文件可比对)'));
      if (st.lastSourceResult) {
        box.appendChild(el('span', 'src-summary-last',
          '最近操作 [' + st.lastSourceResult.ts + '] ' + st.lastSourceResult.detail));
      }
      return;
    }
    var parts = ['导入项目 ' + readable.length + ' 个'];
    if (changed.length > 0) parts.push('源已变更 ' + changed.length + ' 个');
    if (unbound.length > 0) parts.push('未绑定源 ' + unbound.length + ' 个');
    if (missing.length > 0) parts.push('源文件丢失 ' + missing.length + ' 个');

    var cls = 'src-summary';
    if (changed.length > 0 || missing.length > 0) cls += ' src-summary-warn';
    box.className = cls;

    box.appendChild(el('span', 'src-summary-text', '源检查:' + parts.join(' · ')));

    // 需要用户动作的项目列出名字(最多 5 个,避免刷屏)
    var needAction = changed.concat(unbound).concat(missing);
    if (needAction.length > 0) {
      var names = needAction.slice(0, 5).map(function (s) {
        var tag = s.state === 'changed' ? '可更新' : (s.state === 'unbound' ? '待绑定' : '源丢失');
        return s.projectName + '(' + tag + ')';
      });
      if (needAction.length > 5) names.push('…共 ' + needAction.length + ' 个');
      box.appendChild(el('span', 'src-summary-list', names.join('、')));
    }

    // 最近一次手动操作结果(持久可见,不随 toast 消失)
    if (st.lastSourceResult) {
      var r = st.lastSourceResult;
      box.appendChild(el('span', 'src-summary-last',
        '最近操作 [' + r.ts + '] ' + r.name + ':' + r.detail));
    }
  }

  /** 手动「从源更新」单个项目 */
  function updateProjectSource(project, btn) {
    if (btn) { btn.disabled = true; btn.textContent = '更新中…'; }
    window.AppBus.invoke('update_project_from_source', { projectId: project.id })
      .then(function (status) {
        var detail = status && status.detail ? status.detail : '';
        // state 由后端按「更新前副本 vs 源」判定:changed = 确实同步了新内容
        var changed = status && status.state === 'changed';
        window.toast('项目「' + project.name + '」' +
          (changed
            ? '已从源同步改动到配置副本' + (detail ? '(' + detail + ')' : '')
            : '源与配置副本一致,无需更新'), 'ok');
        // 结果落到页面上的检查汇总栏,不只依赖 toast(用户反馈"看不到更新结果")
        st.lastSourceResult = {
          name: project.name,
          state: status ? status.state : 'unknown',
          detail: detail,
          ts: new Date().toTimeString().slice(0, 8)
        };
        return loadConfig().then(function () { return checkProjectSources(false); });
      })
      .catch(function (err) {
        window.toast('更新失败:' + (err && err.message ? err.message : err), 'fail');
      })
      .then(function () {
        if (btn) { btn.disabled = false; btn.textContent = '从源更新'; }
      });
  }

  /**
   * 为导入项目绑定源 compose:调用系统文件对话框选文件 → 后端记录路径与
   * 当前内容基准(不立刻覆盖副本)。绑定后即可用「从源更新」。
   */
  function bindProjectSource(project, btn) {
    if (btn) btn.disabled = true;
    window.AppBus.pickPath({
      directory: false,
      title: '选择该项目的源 compose 文件',
      filters: [{ name: 'Compose', extensions: ['yml', 'yaml'] }]
    }).then(function (picked) {
      if (picked === null) {
        if (btn) btn.disabled = false;
        return null;
      }
      if (btn) btn.textContent = '绑定中…';
      return window.AppBus.invoke('bind_project_source', {
        projectId: project.id,
        sourcePath: picked
      }).then(function (status) {
        window.toast('项目「' + project.name + '」已绑定源:' + picked +
          '(后续源变更会在启动时自动同步)', 'ok');
        st.lastSourceResult = {
          name: project.name,
          state: status ? status.state : 'unchanged',
          detail: '已绑定源文件并建立比对基准',
          ts: new Date().toTimeString().slice(0, 8)
        };
        return loadConfig().then(function () { return checkProjectSources(false); });
      });
    }).catch(function (err) {
      window.toast('绑定源失败:' + (err && err.message ? err.message : err), 'fail');
    }).then(function () {
      if (btn) { btn.disabled = false; btn.textContent = '从源更新'; }
    });
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

  /**
   * 「默认服务器」下拉(第四批):选中后,部署页选中该项目时会自动带出该服务器。
   * 选项含「(不指定)」;已删除的服务器 id 会退化为不指定并在提示里说明。
   */
  function appendServerSelect(body, prev) {
    var row = el('div', 'form-row');
    var label = el('label', 'form-label', '默认服务器(可选)');
    label.setAttribute('for', 'prjf-default-server');
    row.appendChild(label);

    var sel = document.createElement('select');
    sel.className = 'form-input';
    sel.id = 'prjf-default-server';
    var noneOpt = document.createElement('option');
    noneOpt.value = '';
    noneOpt.textContent = '(不指定)';
    sel.appendChild(noneOpt);
    var servers = (st.cfg && st.cfg.servers) ? st.cfg.servers : [];
    servers.forEach(function (s) {
      var o = document.createElement('option');
      o.value = String(s.id);
      o.textContent = String(s.name || s.host);
      sel.appendChild(o);
    });
    var wanted = prev && prev.default_server_id ? String(prev.default_server_id) : '';
    if (wanted && servers.some(function (s) { return String(s.id) === wanted; })) {
      sel.value = wanted;
    }
    row.appendChild(sel);

    var hintText = '部署页选中本项目时自动带出该服务器(仍可临时改选);仅作便利,不做强制校验。';
    if (wanted && !servers.some(function (s) { return String(s.id) === wanted; })) {
      hintText = '原默认服务器已不存在,请重新选择。' + hintText;
    }
    row.appendChild(el('div', 'form-hint', hintText));
    body.appendChild(row);
    return sel;
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

    // 部署位置(第四批):项目级目录 + 默认服务器
    var remoteDir = fieldVal('prjf-remote-dir');
    if (remoteDir && remoteDir.indexOf('/') !== 0) {
      formFailLoud(errId, '远程部署目录需为以 / 开头的绝对路径(如 /home/henghao/site),或留空沿用服务器目录');
      return null;
    }
    var defaultServer = fieldVal('prjf-default-server');
    // 选中的服务器必须仍存在(st.cfg 现取);空 = 不指定
    if (defaultServer && st.cfg &&
        !st.cfg.servers.some(function (s) { return String(s.id) === defaultServer; })) {
      formFailLoud(errId, '所选默认服务器已不存在,请重新选择');
      return null;
    }

    // 发布归档保留数量(第五批):留空 = 用默认 5;填了须为 0-50 整数
    var keepRaw = fieldVal('prjf-release-keep');
    var releaseKeep = null;
    if (keepRaw !== '') {
      if (!/^\d+$/.test(keepRaw) || Number(keepRaw) > 50) {
        formFailLoud(errId, '发布归档保留数量需为 0 - 50 之间的整数,或留空使用默认 5 个');
        return null;
      }
      releaseKeep = Number(keepRaw);
    }

    return {
      health_wait_secs: healthWait,
      pre_deploy_cmd: preCmd ? preCmd : null,
      post_deploy_cmd: postCmd ? postCmd : null,
      notify_webhook: webhook ? webhook : null,
      remote_dir: remoteDir ? remoteDir : null,
      default_server_id: defaultServer ? defaultServer : null,
      release_keep: releaseKeep
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
    var pathInputRow = el('div', 'input-btn-row');
    pathInputRow.appendChild(pathInput);
    var pathBrowseBtn = el('button', 'btn', '浏览');
    pathBrowseBtn.type = 'button';
    pathBrowseBtn.addEventListener('click', function () {
      window.AppBus.pickPath({
        directory: false,
        filters: [{ name: 'Compose 文件', extensions: ['yml', 'yaml'] }],
        title: '选择 compose 文件'
      }).then(function (picked) {
        if (picked === null) return;
        pathInput.value = picked;
        // 与手输一致:触发 input 事件联动(onImportPathInput 绑定在 input 上)
        pathInput.dispatchEvent(new Event('input'));
      });
    });
    pathInputRow.appendChild(pathBrowseBtn);
    pathRow.appendChild(pathInputRow);
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

      // 私钥路径(Key):输入框 + 「浏览」按钮同行(系统选择对话框)
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
      var keyRow = el('div', 'input-btn-row');
      keyRow.appendChild(keyInput);
      var keyBrowseBtn = el('button', 'btn', '浏览');
      keyBrowseBtn.type = 'button';
      keyBrowseBtn.addEventListener('click', function () {
        window.AppBus.pickPath({ directory: false, title: '选择私钥文件' })
          .then(function (picked) {
            if (picked !== null) keyInput.value = picked;
          });
      });
      keyRow.appendChild(keyBrowseBtn);
      keyBlock.appendChild(keyRow);
      keyBlock.appendChild(el('div', 'form-hint', '本机私钥文件的绝对路径'));
      body.appendChild(keyBlock);

      // 私钥口令(Key,阶段三):一次性输入,优先于已存 key_pass_enc;
      // placeholder 依已存口令(key_pass_enc)有无切换(不回显明文/密文)
      var hasSavedKeyPass = !!(prevAuth.key_pass_enc || (prev && prev.key_pass_enc));
      var keyPassBlock = el('div', 'form-row');
      keyPassBlock.id = 'srvf-key-pass-block';
      var keyPassLabel = el('label', 'form-label', '私钥口令');
      keyPassLabel.setAttribute('for', 'srvf-key-pass');
      keyPassBlock.appendChild(keyPassLabel);
      var keyPassInput = document.createElement('input');
      keyPassInput.className = 'form-input';
      keyPassInput.id = 'srvf-key-pass';
      keyPassInput.type = 'password';
      keyPassInput.autocomplete = 'new-password';
      keyPassInput.placeholder = hasSavedKeyPass ? '已保存(留空保持不变)' : '无';
      keyPassBlock.appendChild(keyPassInput);
      keyPassBlock.appendChild(el('div', 'form-hint',
        '仅加密私钥需要;留空则使用已保存口令(无则按无口令私钥加载)'));
      body.appendChild(keyPassBlock);

      // 记住口令复选框(Key,阶段三):保存后自动测试连接时透传,后端在
      // 连接成功且本次口令非空时才 DPAPI 加密保存(部署时免输入)
      var rememberBlock = el('div', 'form-row');
      rememberBlock.id = 'srvf-remember-block';
      var rememberLabel = el('label', 'deploy-checkbox');
      var rememberInput = document.createElement('input');
      rememberInput.type = 'checkbox';
      rememberInput.id = 'srvf-remember-key-pass';
      rememberLabel.appendChild(rememberInput);
      rememberLabel.appendChild(el('span', '', '记住口令(用于部署)'));
      rememberBlock.appendChild(rememberLabel);
      rememberBlock.appendChild(el('div', 'form-hint',
        '勾选且本次输入了口令时,测试连接成功后口令将被加密保存,部署时免输入'));
      body.appendChild(rememberBlock);

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
        keyPassBlock.classList.toggle('hidden', isPass);
        rememberBlock.classList.toggle('hidden', isPass);
        passBlock.classList.toggle('hidden', !isPass);
      }
      radioKey.addEventListener('change', syncAuthBlocks);
      radioPass.addEventListener('change', syncAuthBlocks);
      syncAuthBlocks();

      appendField(body, '远程部署目录', 'srvf-remote-dir', 'text',
        prev ? prev.remote_dir : '', '如:/opt/myapp');

      // 主机密钥指纹(阶段三,TOFU):等宽只读展示 + 「重新信任」;
      // 无指纹(首次连接/已重置)时以灰字 placeholder 提示
      var fingerprint = (prev && prev.host_key_sha256) ? String(prev.host_key_sha256) : '';
      var fpRow = el('div', 'form-row');
      fpRow.id = 'srvf-fp-block';
      fpRow.appendChild(el('label', 'form-label', '主机密钥指纹'));
      var fpLine = el('div', 'input-btn-row');
      var fpInput = document.createElement('input');
      fpInput.className = 'form-input mono';
      fpInput.id = 'srvf-fp-value';
      fpInput.type = 'text';
      fpInput.readOnly = true;
      fpInput.autocomplete = 'off';
      if (fingerprint) {
        fpInput.value = fingerprint;
      } else {
        fpInput.placeholder = '首次连接时自动记录';
      }
      fpLine.appendChild(fpInput);
      if (fingerprint) {
        var retrustBtn = el('button', 'btn', '重新信任');
        retrustBtn.type = 'button';
        retrustBtn.title = '服务器重装或换 IP 后使用:重置指纹,下次连接重新记录';
        retrustBtn.addEventListener('click', function () {
          retrustHostKey(prev.id, fpInput, retrustBtn);
        });
        fpLine.appendChild(retrustBtn);
      }
      fpRow.appendChild(fpLine);
      fpRow.appendChild(el('div', 'form-hint',
        '首次连接记录的服务器指纹,之后指纹不一致将被拒绝连接(防中间人)'));
      body.appendChild(fpRow);

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
    // 阶段三:私钥口令按原样读取(不 trim,口令可能含首尾空格),仅 Key 分支生效
    var keyPassNode = document.getElementById('srvf-key-pass');
    var newKeyPass = (authType === 'Key' && keyPassNode) ? keyPassNode.value : '';
    var rememberNode = document.getElementById('srvf-remember-key-pass');
    var rememberKeyPass = !!(authType === 'Key' && rememberNode && rememberNode.checked);

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

    // Key → 只存 key_path(password_enc / key_pass_enc 原样保留,便于切回
    // 密码认证与继续使用已存口令);Password → 输入了新密码时先加密,否则沿用已存密文
    var auth = {
      auth_type: authType,
      key_path: authType === 'Key' ? keyPath : null,
      password_enc: prevAuth.password_enc || null,
      // 阶段三:已存私钥口令密文原样保留(表单不承载密文;口令更新经
      // test_server 的 remember_key_pass 由后端落盘,不经本表单回写)
      key_pass_enc: prevAuth.key_pass_enc || null
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
        var pid = (prev && prev.id) ? prev.id : uuid();
        var idx = -1;
        for (var i = 0; i < cfg.servers.length; i++) {
          if (cfg.servers[i].id === pid) { idx = i; break; }
        }
        // 字段保全(阶段三):编辑回写是「get_config 全量取 → 整对象替换」
        // 模式,表单不承载的字段必须以配置现值为基底透传回去,否则整对象
        // 替换会经 serde(default) 把它们清空——auth.key_pass_enc(私钥口令
        // 密文)与 host_key_sha256(主机密钥指纹)
        var base = idx >= 0 ? cfg.servers[idx] : prev;
        if (base) {
          auth.key_pass_enc = (base.auth && base.auth.key_pass_enc)
            ? base.auth.key_pass_enc
            : null;
        }
        var server = {
          id: pid,
          name: name,
          host: host,
          port: Number(portRaw),
          username: username,
          auth: auth,
          remote_dir: remoteDir,
          host_key_sha256: (base && base.host_key_sha256) ? base.host_key_sha256 : null
        };
        savedId = pid;
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
        // 保存成功后自动发起一次「测试连接」,结果呈现在服务器卡片上;
        // 表单中的一次性私钥口令与「记住口令」勾选随本次测试透传
        // (是否持久化由后端判定:remember 勾选且口令非空才加密保存)
        if (savedId) {
          var extras = {};
          if (newKeyPass) extras.keyPassphrase = newKeyPass;
          if (rememberKeyPass) extras.rememberKeyPass = true;
          runEnvCheck({ id: savedId }, 'test', extras);
        }
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

      // ===== 部署位置(第四批):项目级目录 + 默认服务器 =====
      // 背景:服务器只有一个 remote_dir,同服务器多项目共用同一部署目录与
      // docker-compose.yml,切换项目要改服务器配置 —— 这里让项目自带目录。
      appendField(body, '远程部署目录(可选)', 'prjf-remote-dir', 'text',
        prev && prev.remote_dir ? String(prev.remote_dir) : '',
        '留空 = 用服务器的部署目录',
        '本项目独立的部署根目录(绝对路径,如 /home/henghao/site);留空则沿用所属服务器配置的远程目录。'
        + '填了独立目录后,需在服务器上先建好该目录(可到 03 页服务器卡片点「创建远程目录」后手动补路径)。');
      appendServerSelect(body, prev);

      // 发布归档保留数量(第五批):部署成功后按此清理旧 releases 目录
      appendField(body, '发布归档保留数量(可选)', 'prjf-release-keep', 'number',
        prev && prev.release_keep !== null && prev.release_keep !== undefined
          ? prev.release_keep : '',
        '留空 = 默认 5 个',
        '部署成功后每次清理旧发布归档(releases/),只保留最新的 N 个(0 - 50,可填 0 表示不留历史)。' +
        '留空用默认 5 个。「服务器管理 → 清理优化」的归档清理也按此数量。',
        { min: '0', max: '50', step: '1' });

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
    remoteInput.placeholder = '留空 = 用本地名称';
    tdRemote.appendChild(remoteInput);
    tr.appendChild(tdRemote);

    // 本地路径填完(失焦/选择)后,若服务器相对路径仍为空 → 自动填入本地末段名
    var autofillRemote = function () {
      if (remoteInput.value.trim()) return;
      var name = localBasename(localInput.value);
      if (name) remoteInput.value = name;
    };
    localInput.addEventListener('blur', autofillRemote);

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
    var mapBrowseBtn = el('button', 'btn btn-sm', '浏览');
    mapBrowseBtn.type = 'button';
    // 闭包引用本行元素:点击时读该行「目录」勾选状态决定选文件还是选目录
    mapBrowseBtn.addEventListener('click', function () {
      var isDir = dirBox.checked;
      window.AppBus.pickPath({
        directory: isDir,
        title: isDir ? '选择本地目录' : '选择本地文件'
      }).then(function (picked) {
        if (picked !== null) {
          localInput.value = picked;
          autofillRemote(); // 浏览选完后同样补默认相对路径
        }
      });
    });
    tdAct.appendChild(mapBrowseBtn);
    var delBtn = el('button', 'btn btn-sm', '删除');
    delBtn.type = 'button';
    delBtn.style.marginLeft = '6px';
    delBtn.addEventListener('click', function () { tr.remove(); });
    tdAct.appendChild(delBtn);
    tr.appendChild(tdAct);

    tbody.appendChild(tr);
  }

  /** 收集文件映射编辑行;行校验失败时已 formFail 提示并返回 null */
  /**
   * 取本地路径的末段名(兼容 `\` 与 `/`;去尾部分隔符)。
   * 与后端 commands::local_basename 同一口径(前端用于表单默认值,
   * 后端用于防御性兜底)。
   */
  function localBasename(path) {
    var p = String(path || '').trim().replace(/[\/\\]+$/, '');
    if (!p) return '';
    var parts = p.split(/[\/\\]/);
    var last = (parts[parts.length - 1] || '').trim();
    if (!last || last === '.' || last === '..') return '';
    return last;
  }

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
      if (!local) {
        formFailLoud(errId, '文件映射第 ' + (i + 1) + ' 行需填写本地路径');
        return null;
      }
      // 服务器相对路径未填 → 默认用本地路径末段名(目录传目录名、文件传文件名),
      // 与部署时后端兜底一致;不再要求用户手填同名
      if (!remote) {
        remote = localBasename(local);
        if (!remote) {
          formFailLoud(errId, '文件映射第 ' + (i + 1) + ' 行无法从本地路径推导名称,请手动填写服务器相对路径');
          return null;
        }
        if (remoteNode) remoteNode.value = remote;
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
          cfg.projects[idx].remote_dir = extras.remote_dir;
          cfg.projects[idx].default_server_id = extras.default_server_id;
          cfg.projects[idx].release_keep = extras.release_keep;
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
            notify_webhook: extras.notify_webhook,
            remote_dir: extras.remote_dir,
            default_server_id: extras.default_server_id,
            release_keep: extras.release_keep
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
              notify_webhook: extras.notify_webhook,
              remote_dir: extras.remote_dir,
              default_server_id: extras.default_server_id,
              release_keep: extras.release_keep
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
    // 手动触发源变更检查(第三批:让"有没有更新"随时可查,不必等启动)
    var srcCheckBtn = document.getElementById('projects-src-check-btn');
    if (srcCheckBtn) {
      srcCheckBtn.addEventListener('click', function () {
        if (st.sourceChecking) return; // 进行中:不重复发起
        srcCheckBtn.disabled = true;
        srcCheckBtn.textContent = '检查中…';
        // 手动检查不自动改配置,只刷新状态(用户想改可点「从源更新」)
        checkProjectSources(false).then(function (list) {
          srcCheckBtn.disabled = false;
          srcCheckBtn.textContent = '检查源变更';
          // 手动点击必须**总是**有可见反馈:此前只在结果栏渲染(无导入项目时
          // 整块隐藏)且不弹提示,点完看起来"什么都没发生"。
          reportSourceCheckResult(Array.isArray(list) ? list : []);
        });
      });
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

    // 清理分析模态:关闭按钮 / 遮罩点击 / Esc(与 servers-modal 同一套)
    var cleanupClose = document.getElementById('cleanup-modal-close');
    if (cleanupClose) {
      cleanupClose.addEventListener('click', closeCleanupModal);
    }
    var cleanupOverlay = document.getElementById('cleanup-modal');
    if (cleanupOverlay) {
      cleanupOverlay.addEventListener('click', function (e) {
        if (e.target === cleanupOverlay) closeCleanupModal();
      });
    }
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') {
        closeModal();
        if (cleanupModal() && !cleanupModal().classList.contains('hidden')) {
          closeCleanupModal();
        }
      }
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
