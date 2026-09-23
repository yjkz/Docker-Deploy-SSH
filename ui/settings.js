/* ============================================================
 * settings.js — 设置中心(UPGRADE-PLAN 阶段四「桌面体验」)
 *
 * 入口:dock 底部的齿轮按钮(#settings-entry-btn,主题切换按钮旁),
 * 打开独立模态 #settings-modal(骨架在 index.html,内容由本文件构建)。
 *
 * 后端命令(src-tauri/src/config.rs / update.rs;JS 参数名为 Tauri camelCase):
 * - app_settings_get()
 *     -> { closeToTray, proxy, autoUpdateFromSource, probeIntervalMins,
 *          autoCheckUpdate, termLogKeepDays }
 *     (独立持久化于 config/settings.json;文件缺失/损坏后端回退默认值)
 * - app_settings_set({ closeToTray, proxy, autoUpdateFromSource,
 *                      probeIntervalMins, autoCheckUpdate, termLogKeepDays,
 *                      alertIntervalMins, alertDiskPercent, alertMemPercent,
 *                      alertCpuPercent }) -> Ok/Err
 *     (顶层参数包;托盘与「关闭窗口隐藏到托盘」的拦截在后端事件里现读
 *      settings.json,保存后立即生效,无需重启)
 * - update_check({ proxy }) -> { current, latest, hasUpdate, url, notes }
 *     proxy 传设置里的值或 null(空串等价直连,统一传 null);
 *     「检查更新」与「测试连接」共用本命令 —— 测试连接 = 实际拉取一次
 *     GitHub API 并回显成功/失败原文。
 *
 * 交互约定(参照 notify.js 先例):
 * - 六分区(第三十五批):外观 / 通用 / 部署 / 监控 / 更新 / 关于,左侧导航切换,
 *   任一时刻只显示一区(分区 DOM 一次性全建、靠 .hidden 切换,不重建 —— 未保存的
 *   输入与 getElementById 契约都不受影响;卡片定高,只有内容区滚动,底部动作行
 *   常驻)。「外观」改动即时写 localStorage['dd_scheme'](与 app.js 同键)并应用;
 *   其余改动需点「保存设置」(app_settings_set)。
 * - 模态开合带会话序号(st.session,每次打开 +1):读取/保存/检查更新的
 *   异步收尾先校验会话未过期再回写,防止旧 promise 回写重开后的新模态;
 *   关闭模态时复位防重标志。Esc / 遮罩 / 关闭钮均可关闭。
 * - 「检查更新 / 测试连接」使用输入框当前代理值(未保存也能测),进行中
 *   禁用两钮防重复;结果行内回显,发现新版本时自动弹出「更新确认模态」
 *   (#update-confirm-modal,用户指定的圆润卡片样式):展示更新内容,
 *   点「自动更新」一次确认全自动(下载 → 静默安装 → 重启新版);
 *   下载进行中禁止关闭模态(三通道统一守卫,同 deploy 模态 rbBusy 先例)。
 *
 * 安全说明:与全站一致,一律 createElement + textContent 构建,
 * 不使用 innerHTML 拼接;提示一律 toast / 行内回显,不调用系统对话框。
 * ============================================================ */
(function () {
  'use strict';

  // ===== 常量 =====

  /** 主题持久化键(与 app.js 的 SCHEME_KEY 同键,勿改) */
  var SCHEME_KEY = 'dd_scheme';
  /** 外观单选选项(value 与 app.js applyArkScheme / 头部防闪白脚本口径一致) */
  var SCHEME_OPTIONS = [
    { value: 'light', label: '亮色' },
    { value: 'dark', label: '暗色' },
    { value: 'auto', label: '跟随系统' }
  ];
  /** notes 展示最长字符数(后端已截到 2000,前端再截防撑爆模态) */
  var NOTES_SHOW_MAX = 600;
  /** 更新区按钮文案常量(setBusy 复位时恢复) */
  var CHECK_LABEL = '检查更新';
  var TEST_LABEL = '测试连接';

  // ===== 「关于」栏常量(第四批)=====
  /** 作者展示名 */
  var AUTHOR_NAME = '夜艺';
  /** 头像:随应用内嵌(ui/images/avatar.png),离线也能显示;不依赖每次联网抓取 */
  var AVATAR_SRC = 'images/avatar.png';
  /** 项目主页 / GitHub 主页 / 个人主页(点击经 open_external 用系统浏览器打开) */
  var LINK_PROJECT = 'https://github.com/yjkz/Docker-Deploy-SSH';
  var LINK_GITHUB = 'https://github.com/yjkz';
  var LINK_HOME = 'https://blog.yeyeyiyi.online';

  // ===== 状态 =====

  var st = {
    saving: false,   // 保存设置中(防重复提交;随模态关闭复位)
    checking: false, // 检查更新/测试连接进行中(共用网络通道,防重复;随模态关闭复位)
    session: 0       // 模态会话序号:每次打开 +1,异步收尾据此丢弃过期回调
  };

  // ===== 小工具 =====

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  /** 读取输入框当前值(元素缺失返回空串) */
  function fieldVal(id) {
    var node = document.getElementById(id);
    return node ? String(node.value) : '';
  }

  /** 读取复选框勾选态(元素缺失按 false) */
  function isChecked(id) {
    var node = document.getElementById(id);
    return !!(node && node.checked);
  }

  function setChecked(id, checked) {
    var node = document.getElementById(id);
    if (node) node.checked = checked === true;
  }

  /** 复选框行(deploy-checkbox 体系,同 notify.js 先例):label > input + span 文案 */
  function checkboxRow(id, text, checked) {
    var label = el('label', 'deploy-checkbox');
    var input = document.createElement('input');
    input.type = 'checkbox';
    input.id = id;
    if (checked) input.checked = true;
    label.appendChild(input);
    label.appendChild(el('span', null, text));
    return label;
  }

  /** 分组标题(中文 + 大写英文;样式见 .form-group-title) */
  function groupTitle(zh, en) {
    return window.formGroupTitle(zh, en);
  }

  /** 组内说明文字(弱化段落) */
  function hint(text) {
    return el('div', 'set-hint', text);
  }

  /** 按钮忙碌态切换(执行保存/检查期间禁用防重复;第六批收敛到共享助手) */
  function setBusy(id, busy, label) {
    window.setBtnBusy(document.getElementById(id), busy, label);
  }

  /** 模态可见性(#settings-modal 未带 hidden 类) */
  function isModalVisible() {
    var overlay = document.getElementById('settings-modal');
    return !!(overlay && !overlay.classList.contains('hidden'));
  }

  /** 模态会话是否仍有效:捕获后未再重开过模态(会话序号未变) */
  function sessionAlive(session) {
    return session === st.session;
  }

  // ===== 外观:主题模式读写与应用 =====

  /** 读取主题模式(localStorage['dd_scheme'],非法值/异常按显式 light) */
  function schemeMode() {
    var mode = null;
    try { mode = localStorage.getItem(SCHEME_KEY); } catch (e) { /* 忽略 */ }
    for (var i = 0; i < SCHEME_OPTIONS.length; i++) {
      if (mode === SCHEME_OPTIONS[i].value) return mode;
    }
    return 'light';
  }

  /**
   * 应用主题模式:优先复用 app.js 暴露的 window.applyArkScheme
   * (本文件在 app.js 之后加载,正常路径恒走此分支;逻辑来源见 app.js
   * 「主题跟随系统」段,IIFE 内部实现不可直接引用,此处仅调其 window 出口)。
   * 兜底分支与 app.js window.applyArkScheme 同逻辑,防脚本加载顺序异常。
   */
  function applySchemeMode(mode) {
    if (typeof window.applyArkScheme === 'function') {
      window.applyArkScheme(mode);
      return;
    }
    var dark = mode === 'dark' || (mode === 'auto' && window.matchMedia &&
      window.matchMedia('(prefers-color-scheme: dark)').matches);
    document.documentElement.dataset.arkScheme = dark ? 'dark' : 'light';
  }

  /** 外观单选 change:即时写 localStorage['dd_scheme'] 并应用(auto = 跟随系统) */
  function onSchemeRadioChange() {
    var mode = schemeMode(); // 现值兜底
    for (var i = 0; i < SCHEME_OPTIONS.length; i++) {
      var input = document.getElementById('settings-scheme-' + SCHEME_OPTIONS[i].value);
      if (input && input.checked) mode = SCHEME_OPTIONS[i].value;
    }
    try { localStorage.setItem(SCHEME_KEY, mode); } catch (e) { /* 忽略 */ }
    applySchemeMode(mode);
  }

  /** 勾选外观单选(fill 用;仅改选中态,不触发应用) */
  function markSchemeRadios(mode) {
    SCHEME_OPTIONS.forEach(function (opt) {
      setChecked('settings-scheme-' + opt.value, opt.value === mode);
    });
  }

  // ===== 模态 body 构建(每次打开重建,事件随元素重建,无重复绑定)=====

  /** 单字段行:标签 + 输入框(+ 可选提示);返回输入元素 */
  function buildField(labelText, en, inputId, inputType, value, placeholder, hint) {
    var row = el('div', 'form-row');
    row.appendChild(window.formLabel(labelText, en, false, inputId));
    var input = document.createElement('input');
    input.className = 'form-input';
    input.id = inputId;
    input.type = inputType;
    if (value !== undefined && value !== null) input.value = String(value);
    if (placeholder) input.placeholder = placeholder;
    input.autocomplete = 'off';
    input.spellcheck = false;
    row.appendChild(input);
    if (hint) row.appendChild(el('div', 'form-hint', hint));
    return row;
  }

  /**
   * 分区定义(第三十五批:左侧分类导航;数组顺序 = 导航顺序)。
   *
   * 拆区依据:原「通用」组独吞 17 项、单列约 3 屏,长模态没有导航也没有吸顶,
   * 用户要滑半天才见底。六个分区任一时刻只显示一区(≤6 项),滚动基本消失;
   * 「关于」从右侧常驻栏改为末位分区,正文因此拿到全宽(hint 换行更少)。
   */
  var SECTIONS = [
    { key: 'appearance', zh: '外观', en: 'APPEARANCE' },
    { key: 'general', zh: '通用', en: 'GENERAL' },
    { key: 'deploy', zh: '部署', en: 'DEPLOY' },
    { key: 'monitor', zh: '监控', en: 'MONITOR' },
    { key: 'update', zh: '更新', en: 'UPDATE' },
    { key: 'about', zh: '关于', en: 'ABOUT' }
  ];

  /**
   * 切换分区:导航项 active + 分区 .hidden 互斥切换。
   *
   * 只切显示、不重建 DOM —— 未保存的输入与更新结果区内容都留在原处,
   * fillSettings / onSave 的 getElementById 契约因此完全不受影响。
   */
  function selectSection(key) {
    var items = document.querySelectorAll('#settings-modal .settings-nav-item');
    for (var i = 0; i < items.length; i++) {
      items[i].classList.toggle('active', items[i].getAttribute('data-section') === key);
    }
    var panels = document.querySelectorAll('#settings-modal .settings-section');
    for (var j = 0; j < panels.length; j++) {
      panels[j].classList.toggle('hidden', panels[j].getAttribute('data-section') !== key);
    }
    var content = document.querySelector('#settings-modal .settings-content');
    if (content) content.scrollTop = 0; // 换区回到顶部(各区独立滚动)
  }

  function buildBody(body) {
    body.textContent = '';

    // 内联错误框(本轮补齐,同 config-io 的理由:设置是三组内容的长模态,
    // 保存失败只弹 toast 会一闪而过)
    body.appendChild(window.formErrorBox('settings-error'));

    // 左分类导航 + 右内容区 + 底部常驻动作行(第三十五批)。
    // 分区 DOM 一次性全建、靠 .hidden 切换;卡片定高(见 .settings-modal-card),
    // 只有 .settings-content 滚动 —— 导航与「保存设置」永不滚走。
    var layout = el('div', 'settings-layout');
    var nav = el('nav', 'settings-nav');
    nav.setAttribute('aria-label', '设置分类');
    var content = el('div', 'settings-content');
    layout.appendChild(nav);
    layout.appendChild(content);
    body.appendChild(layout);

    var sections = {};
    SECTIONS.forEach(function (def, i) {
      var btn = el('button', 'settings-nav-item');
      btn.type = 'button';
      btn.setAttribute('data-section', def.key);
      btn.appendChild(el('span', 'settings-nav-zh', def.zh));
      // 英文微标签复用 .form-label-en(cond 轨道 + 大写 + 字距),选中态由
      // .settings-nav-item.active 翻色(见 style.css 设置段)
      btn.appendChild(el('span', 'form-label-en', def.en));
      btn.addEventListener('click', function () { selectSection(def.key); });
      nav.appendChild(btn);

      var panel = el('section', 'settings-section');
      panel.setAttribute('data-section', def.key);
      if (i > 0) panel.classList.add('hidden'); // 默认落在第一区(外观)
      sections[def.key] = panel;
      content.appendChild(panel);
    });

    // main = 当前分区游标:下方各字段块按归属改指 sections.<key>,
    // 各分区内部保持原有先后顺序
    var main = sections.appearance;

    // ── 外观 APPEARANCE(单选,即时生效)──
    main.appendChild(groupTitle('外观', 'APPEARANCE'));
    var radioRow = el('div', 'radio-row');
    var current = schemeMode();
    SCHEME_OPTIONS.forEach(function (opt) {
      var label = el('label');
      var input = document.createElement('input');
      input.type = 'radio';
      input.name = 'settings-scheme';
      input.id = 'settings-scheme-' + opt.value;
      input.value = opt.value;
      if (opt.value === current) input.checked = true;
      input.addEventListener('change', onSchemeRadioChange);
      label.appendChild(input);
      label.appendChild(el('span', null, opt.label));
      radioRow.appendChild(label);
    });
    main.appendChild(radioRow);
    main.appendChild(hint(
      '「跟随系统」随操作系统的深色模式实时切换;点击 dock 主题按钮会写入' +
      '显式亮 / 暗并停用跟随'));

    // ── 通用 GENERAL(启动行为 / 日志)──
    main = sections.general;
    main.appendChild(groupTitle('通用', 'GENERAL'));
    main.appendChild(checkboxRow('settings-close-tray', '关闭窗口时隐藏到托盘', false));
    main.appendChild(hint('开启后点关闭仅隐藏窗口(部署继续),托盘菜单「退出」才真正退出;保存后立即生效'));
    main.appendChild(checkboxRow('settings-auto-update-src', '启动时自动从源更新项目', true));
    main.appendChild(hint(
      '开启后打开软件会比对导入项目的源 compose(含 .env / override)与配置内副本,' +
      '发现变化即自动同步并重解析(保留已保存的服务分类);关闭后仍可在项目列表点「从源更新」手动执行'));
    // 启动静默检查更新(第二十一批):有新版仅 dock 版本号旁加徽点,不弹窗
    main.appendChild(checkboxRow('settings-auto-check-update', '启动时静默检查更新', true));
    main.appendChild(hint(
      '开启后打开软件会延迟数秒检查一次新版本;有新版时仅在左下角版本号旁亮起圆点' +
      '(点击进本设置页),不弹窗打扰;检查失败(网络/代理不可达)静默忽略'));
    // ── 监控 MONITOR(探活 / 资源告警 / 日报)──
    main = sections.monitor;
    main.appendChild(groupTitle('监控', 'MONITOR'));

    // 服务器定时探活(第十七批):间隔分钟数,0 = 关闭;保存即启停后端探活任务
    main.appendChild(buildField('服务器探活间隔(分钟)', 'PROBE',
      'settings-probe-interval-input', 'number', '0',
      '0 = 关闭;例如 5 表示每 5 分钟探活一次',
      '按间隔 TCP 探活全部已配置服务器;状态翻转(在线→离线 / 离线→恢复)时经通知中心提醒,' +
      '订阅开关在通知中心的「事件」区;探活不触发 SSH 认证,不碰密钥'));

    // 资源阈值告警(第二十四批):采样间隔 + 三项阈值(0 = 该项不告警)。
    // 判定口径:连续 2 轮超阈才告警(防瞬态尖峰),回落单轮即恢复通知;
    // 订阅开关在通知中心「事件」区的「资源阈值告警」勾选
    main.appendChild(buildField('资源告警采样间隔(分钟)', 'ALERT',
      'settings-alert-interval-input', 'number', '0',
      '0 = 关闭;例如 10 表示每 10 分钟采样一次磁盘 / 内存 / CPU',
      '采样经 SSH 逐台获取服务器资源占用;与部署 / 回滚互斥(执行中被跳过,下轮重试);' +
      '订阅开关与通知渠道见通知中心'));
    main.appendChild(buildField('磁盘告警阈值(%)', 'DISK',
      'settings-alert-disk-input', 'number', '90',
      null,
      '根分区与 Docker 数据盘取较高者;0 = 不告警该项'));
    main.appendChild(buildField('内存告警阈值(%)', 'MEM',
      'settings-alert-mem-input', 'number', '90',
      null,
      '0 = 不告警该项'));
    main.appendChild(buildField('CPU 告警阈值(%)', 'CPU',
      'settings-alert-cpu-input', 'number', '90',
      null,
      '0 = 不告警该项'));

    // ── 部署 DEPLOY(扫描识别 / 回滚 / 搬运)──
    main = sections.deploy;
    main.appendChild(groupTitle('部署', 'DEPLOY'));

    // 部署失败自动回滚(第二十五批):仅整栈 + 仅健康检查失败时触发;
    // 默认关(自动回滚会改线上状态,须显式开启)
    main.appendChild(checkboxRow('settings-auto-rollback', '整栈部署健康检查失败时自动回滚', false));
    main.appendChild(hint(
      '仅整栈部署在「健康检查未通过」时触发:自动回滚到上一份完整归档(需存在上一版;' +
      '首次部署无归档时不触发)。续传中不触发;用户取消不触发。' +
      '单镜像部署不产归档,故不适用。回滚结果以日志形式并入本次部署记录'));

    // 扫描识别放宽(第二十六批):自定义 compose 文件名 + 扫描深度。
    // 名字会拼进远端 find 命令,后端严格校验(仅 [A-Za-z0-9._-]),非法项丢弃、
    // 全非法回退内置四标准名 —— 前端只做逗号分隔的原始收集
    main.appendChild(buildField('compose 文件名(逗号分隔)', 'SCAN NAMES',
      'settings-compose-names-input', 'text', '',
      '留空 = 内置四标准名(docker-compose.yml/.yaml、compose.yml/.yaml)',
      '栈列表与清理分析的扫描按这些名字匹配;支持 docker-compose.prod.yml 等自定义命名(仅字母数字与 . _ -,单个 ≤64 字符)'));
    main.appendChild(buildField('扫描最大深度', 'SCAN DEPTH',
      'settings-compose-depth-input', 'number', '4',
      '默认 4;1-8',
      '扫描部署目录的目录层级上限;栈放得较深时调大(越大扫描越慢)'));

    // 卷搬运 tar 镜像(第二十七批):服务器有私有 registry / 镜像白名单时,
    // 内置候选(busybox/alpine/ubuntu)都可能取不到 —— 允许指定自备 tar 能力镜像。
    // 形态合法性由后端 normalize_tar_image 单点裁决(与 compose 文件名同纪律)
    main.appendChild(buildField('卷搬运 tar 镜像', 'TAR IMAGE',
      'settings-tar-image-input', 'text', '',
      '留空 = 内置候选(busybox → alpine → ubuntu)',
      '项目迁移搬运数据卷时用哪个镜像执行 tar;服务器有私有 registry 或镜像白名单时填自备镜像' +
      '(如 registry.local/tools/tar:1)。该镜像需已存在于服务器且自带 tar'));

    // 开机自启归「通用」(第三十五批切区;第二十九批 S1):真值在注册表
    // (用户可能在任务管理器里禁用),回填与保存都以读到的实况为准
    // —— 见后端 autostart::with_actual_state
    main = sections.general;
    main.appendChild(checkboxRow('settings-auto-start', '开机时自动启动', false));
    main.appendChild(hint(
      '登录 Windows 后自动启动本软件(仅当前用户,不需要管理员权限)。' +
      '若在「任务管理器 → 启动」里禁用过,这里的勾选状态会跟随系统实际设置'));

    // 部署日报归「监控」(第三十五批):留空 = 关闭;填 0-23 = 该整点后发一条汇总
    main = sections.monitor;
    main.appendChild(buildField('部署日报时刻', 'DIGEST HOUR',
      'settings-digest-hour', 'number', '',
      '留空 = 关闭;填 0-23 表示每天该整点后发一条当天部署汇总',
      '需在通知中心勾选「部署日报」订阅;当天无部署时不发空日报'));

    // 终端日志保留归「通用」(第三十五批):天数,0 = 永久保留(默认 30)。
    // hint 同时说明清理时机(打开终端 / 启动软件时 best-effort 清理)
    main = sections.general;
    main.appendChild(buildField('终端日志保留(天)', 'TERM LOGS',
      'settings-term-keep-days', 'number', '30',
      '0 = 永久保留;例如 30 表示只保留最近 30 天',
      '超过保留天数的终端会话日志(term-<时间戳>-<容器>.log)在打开终端或启动软件时自动清理;' +
      '运行日志 app.log 不受影响'));

    // 诊断日志:此前设置中心没有日志入口,排障需手动定位应用目录 logs/。
    // 打开动作经 open_logs_dir 由系统资源管理器完成(后端确保目录存在)
    var logRow = el('div', 'form-row');
    logRow.appendChild(window.formLabel('日志文件', 'LOGS', false, 'settings-open-logs-btn'));
    var logBtn = el('button', 'btn', '打开日志文件夹');
    logBtn.type = 'button';
    logBtn.id = 'settings-open-logs-btn';
    logBtn.addEventListener('click', onOpenLogs);
    logRow.appendChild(logBtn);
    logRow.appendChild(el('div', 'form-hint',
      '运行日志按日期轮转写在应用目录 logs/ 下,部署 / 连接问题可在此排查'));
    main.appendChild(logRow);

    // ── 宿主机可写目录(第三十四批(五);归「部署」区,故先切回 deploy)──
    main = sections.deploy;
    // 文件管理「部署目录」源的写权限白名单:留空 = 全只读(维持第三十三批口径)
    var wpRow = el('div', 'form-row');
    wpRow.appendChild(window.formLabel('宿主机可写目录', 'HOST WRITE', false, 'settings-host-write-paths'));
    var wpTa = el('textarea', 'form-textarea');
    wpTa.id = 'settings-host-write-paths';
    wpTa.rows = 3;
    wpTa.placeholder = '/opt/app\n/srv/data';
    wpTa.spellcheck = false;
    wpRow.appendChild(wpTa);
    wpRow.appendChild(el('div', 'form-hint',
      '每行一个绝对路径;文件管理「部署目录」源只允许写入这些目录之内(留空 = 只读,不放开写)。' +
      '写入前会在服务器上解析真实路径(软链不能逃逸),最多 10 条'));
    main.appendChild(wpRow);

    // 层级增量传输(第三十七批):默认开;关掉即回到「一律整包」的历史行为
    main.appendChild(checkboxRow('settings-incremental', '层级增量传输(只传服务器缺少的层)', true));
    main.appendChild(hint(
      '开启后部署会先问服务器「你已有哪些层」,把本地镜像包里这些层裁掉再传 —— ' +
      '同一 Dockerfile 重建、层大量重叠时能省下绝大部分上传量。' +
      '装载失败会自动改用整包重传(本地同时保留整份),不会因此漏传;' +
      '整栈部署装载成功后由服务器重建完整归档包,回滚仍可直接装载;' +
      '首次部署某个镜像、或服务器上没有可复用的层时,与原来完全一致'));

    // ── 更新 UPDATE(代理 + 连通性实测;两钮作用于本区输入框)──
    main = sections.update;
    main.appendChild(groupTitle('更新', 'UPDATE'));
    main.appendChild(buildField('代理地址', 'PROXY', 'settings-proxy-input', 'text', '',
      '留空直连;支持 http:// 与 socks5://,例如 http://127.0.0.1:7890',
      '仅用于检查更新访问 GitHub;「检查更新 / 测试连接」使用上方输入框当前值,未保存也可测试'));

    // 两钮紧跟代理字段(它们只作用于该字段);结果区排在它们下方
    var testGroup = el('div', 'settings-test-group');
    var checkBtn = el('button', 'btn', CHECK_LABEL);
    checkBtn.type = 'button';
    checkBtn.id = 'settings-check-btn';
    checkBtn.addEventListener('click', onCheckUpdate);
    testGroup.appendChild(checkBtn);

    var testBtn = el('button', 'btn', TEST_LABEL);
    testBtn.type = 'button';
    testBtn.id = 'settings-test-btn';
    testBtn.addEventListener('click', onTestConnection);
    testGroup.appendChild(testBtn);
    main.appendChild(testGroup);

    // 更新结果区(行内回显 + notes 截断 + 前往下载,内容见 showUpdate*)
    var area = el('div', 'set-update-area');
    area.id = 'settings-update-area';
    main.appendChild(area);

    // ── 底部常驻动作行(第三十五批:移出滚动区,钉在卡片底部)──
    var actions = el('div', 'modal-actions settings-actions');
    var saveBtn = el('button', 'btn btn-primary', '保存设置');
    saveBtn.type = 'button';
    saveBtn.id = 'settings-save-btn';
    saveBtn.addEventListener('click', onSave);
    actions.appendChild(saveBtn);
    body.appendChild(actions);

    // ── 关于 ABOUT:末位分区(原右侧常驻栏,正文因此获得全宽)──
    buildAboutPanel(sections.about);
  }

  /**
   * 「关于」分区(第四批;第三十五批从右侧常驻栏改为末位分区)。
   * 头像 → 名字 → 项目主页 / GitHub / 个人主页。
   *
   * 头像随应用内嵌(`ui/images/avatar.png`),不依赖联网抓取;三处地址经
   * `open_external` 交给系统浏览器(WebView 内直接跳外链会被拦)。
   */
  function buildAboutPanel(panel) {
    panel.appendChild(groupTitle('关于', 'ABOUT'));

    var card = el('div', 'about-card');

    // 头像(加载失败时退化为首字占位,不让空框留在界面上)
    var avatarWrap = el('div', 'about-avatar-wrap');
    var img = document.createElement('img');
    img.className = 'about-avatar';
    img.src = AVATAR_SRC;
    img.alt = AUTHOR_NAME + ' 的头像';
    img.addEventListener('error', function () {
      avatarWrap.textContent = '';
      avatarWrap.appendChild(el('div', 'about-avatar about-avatar-fallback',
        AUTHOR_NAME.slice(0, 1)));
    });
    avatarWrap.appendChild(img);
    card.appendChild(avatarWrap);

    card.appendChild(el('div', 'about-name', AUTHOR_NAME));

    var links = el('div', 'about-links');
    [
      { label: '项目主页', url: LINK_PROJECT },
      { label: 'GitHub 主页', url: LINK_GITHUB },
      { label: '个人主页', url: LINK_HOME }
    ].forEach(function (item) {
      var row = el('button', 'about-link', item.label);
      row.type = 'button';
      row.title = item.url;
      row.setAttribute('data-url', item.url);
      row.addEventListener('click', function () {
        window.AppBus.invoke('open_external', { url: item.url })
          .catch(function (err) {
            window.toast('打开链接失败:' + (errText(err) || '未知错误'), 'fail');
          });
      });
      links.appendChild(row);
    });
    card.appendChild(links);

    panel.appendChild(card);
  }

  // ===== 更新结果区(行内回显;失败用 --ark-stat-hot 暗红)=====

  function updateArea() {
    return document.getElementById('settings-update-area');
  }

  /** 结果区单行文案(kind: ok | info | fail) */
  function showUpdateMessage(kind, text) {
    var area = updateArea();
    if (!area) return; // 模态已关闭(body 已清空):静默丢弃
    area.textContent = '';
    var line = el('div', 'set-result');
    line.appendChild(el('span', 'set-result-' + kind, text));
    area.appendChild(line);
  }

  /** 更新区两钮统一禁用/恢复(共用网络通道,防交叠写同一结果区) */
  function setUpdateBusy(busy, checkLabel, testLabel) {
    setBusy('settings-check-btn', busy, checkLabel || CHECK_LABEL);
    setBusy('settings-test-btn', busy, testLabel || TEST_LABEL);
  }

  /** 有更新:结果行 + 「查看更新详情」入口(更新内容与确认收进独立模态) */
  function renderUpdateAvailable(info) {
    var area = updateArea();
    if (!area) return;
    area.textContent = '';
    var line = el('div', 'set-result');
    line.appendChild(el('span', 'set-result-ok',
      '发现新版本:v' + info.latest + '(当前 v' + info.current + ')'));
    area.appendChild(line);

    var btn = el('button', 'btn', '查看更新详情');
    btn.type = 'button';
    btn.id = 'settings-update-detail-btn';
    btn.addEventListener('click', function () { openUpdateConfirmModal(info); });
    area.appendChild(btn);
  }

  // ===== 更新确认模态(检查更新发现新版本时自动弹出;一次确认全自动)=====
  //
  // 形态:#update-confirm-modal(圆润卡片,用户指定样式例外)。展示更新内容
  // (Release 说明),点「自动更新」后全自动:下载 → 直接静默安装 → 重启新版
  // (不再有第二次「确认安装」步骤)。下载进行中三通道关闭统一被守卫拦截。

  var updating = false;  // 下载/安装进行中(防重复;期间禁止关闭模态)
  var updateInfo = null; // 当前模态展示的 UpdateInfo

  function updateModal() {
    return document.getElementById('update-confirm-modal');
  }

  function isUpdateModalVisible() {
    var overlay = updateModal();
    return !!(overlay && !overlay.classList.contains('hidden'));
  }

  /** 打开更新确认模态:版本行 + 更新内容(空 notes 兜底)+ 三个动作按钮 */
  function openUpdateConfirmModal(info) {
    var overlay = updateModal();
    var body = document.getElementById('update-confirm-modal-body');
    if (!overlay || !body) return;
    updateInfo = info;
    body.textContent = '';

    var line = el('div', 'set-result');
    line.appendChild(el('span', 'set-result-ok',
      '新版本 v' + (info.latest || '') + '(当前 v' + (info.current || '') + ')'));
    body.appendChild(line);

    // 更新内容(Release 说明;后端已截 2000 字符,前端再截防撑爆模态)
    var notes = String(info.notes || '');
    if (notes.length > NOTES_SHOW_MAX) notes = notes.slice(0, NOTES_SHOW_MAX) + '…';
    if (notes) {
      body.appendChild(el('div', 'set-notes', notes));
    } else {
      body.appendChild(hint('未能获取更新说明,可前往发布页查看本次更新内容。'));
    }

    var result = el('div', 'set-result');
    result.id = 'update-confirm-result';
    body.appendChild(result);

    var actions = el('div', 'modal-actions');
    var laterBtn = el('button', 'btn', '稍后再说');
    laterBtn.type = 'button';
    laterBtn.addEventListener('click', closeUpdateConfirmModal);
    var ghBtn = el('button', 'btn', '前往发布页');
    ghBtn.type = 'button';
    ghBtn.addEventListener('click', function () {
      window.AppBus.invoke('open_external', { url: String(info.url || '') })
        .catch(function (e) { window.toast('打开浏览器失败: ' + errText(e), 'fail'); });
    });
    var goBtn = el('button', 'btn btn-primary', '自动更新');
    goBtn.id = 'update-confirm-go-btn';
    goBtn.addEventListener('click', function () { onAutoUpdate(goBtn); });
    actions.appendChild(laterBtn);
    actions.appendChild(ghBtn);
    actions.appendChild(goBtn);
    body.appendChild(actions);

    overlay.classList.remove('hidden');
    window.modalFocusOpen(overlay);
  }

  /** 关闭模态:下载/安装进行中禁止(三通道统一走此守卫) */
  function closeUpdateConfirmModal() {
    if (updating) {
      window.toast('正在下载更新,完成后才能关闭', 'warn');
      return;
    }
    var overlay = updateModal();
    if (overlay) {
      overlay.classList.add('hidden');
      window.modalFocusClose(overlay);
    }
    var body = document.getElementById('update-confirm-modal-body');
    if (body) body.textContent = '';
    updateInfo = null;
  }

  /** 模态内结果行(kind: ok | info | fail) */
  function setUpdateResult(kind, text) {
    var node = document.getElementById('update-confirm-result');
    if (!node) return;
    node.textContent = '';
    node.appendChild(el('span', 'set-result-' + kind, text));
  }

  /** 确认自动更新:下载成功后直接安装并重启(无第二次确认) */
  function onAutoUpdate(btn) {
    var info = updateInfo;
    if (!info || updating) return;
    updating = true;
    window.setBtnBusy(btn, true, '正在下载 v' + (info.latest || '') + '…');
    setUpdateResult('info', '正在下载 v' + (info.latest || '') + ' 安装包(约 9 MB,视网络而定)…');

    window.AppBus.invoke('update_download', { version: String(info.latest || ''), proxy: proxyArg() })
      .then(function (dl) {
        var d = dl || {};
        var mb = (Number(d.sizeBytes) || 0) / 1024 / 1024;
        setUpdateResult('info', '下载完成(' + mb.toFixed(1) + ' MB),正在安装并重启应用…');
        return window.AppBus.invoke('update_install', {
          setupPath: String(d.setupPath || ''),
          version: String(d.version || info.latest || '')
        }).then(function () {
          // 后端 500ms 后 exit(0),安装器 /R 装完自动拉起新版;
          // 重启后由 take_update_pending 提示「已更新到 vX」(app.js)
          updating = false;
          window.setBtnBusy(btn, false, '自动更新');
        }).catch(function (err) {
          updating = false;
          window.setBtnBusy(btn, false, '自动更新');
          if (!isUpdateModalVisible()) return;
          setUpdateResult('fail', '启动安装失败:' + (errText(err) || '未知错误') +
            '(可手动运行:' + (d.setupPath || '') + ')');
        });
      })
      .catch(function (err) {
        updating = false;
        window.setBtnBusy(btn, false, '自动更新');
        if (!isUpdateModalVisible()) return;
        // 第二十六批:按错误码分引导 —— network(可重试/换代理)与 internal
        // (本机写盘/进程问题,重试多数无效 → 引导到 Release 页手动下载)
        var code = window.errCodeOf(err);
        var hint = code === 'internal'
          ? '(本机环境问题,重试多数无效;建议到 Release 页手动下载)'
          : (code === 'network' ? '(可检查网络或代理后重试,或到 Release 页手动下载)' : '');
        setUpdateResult('fail', '下载失败:' + (errText(err) || '未知错误') + hint);
      });
  }

  // ===== 动作:保存 / 检查更新 / 测试连接 =====

  /** 代理输入框当前值 → update_check 的 proxy 入参(空串 = 直连 = null) */
  function proxyArg() {
    var value = fieldVal('settings-proxy-input').trim();
    return value === '' ? null : value;
  }

  /**
   * 终端日志保留天数输入 → app_settings_set 载荷(u32)。
   * 非数字/空回退 30(与后端 serde default 同口径);夹取 0–3650(与后端
   * 读取侧 `TERM_LOG_KEEP_DAYS_MAX` 同口径)——负值直接传给后端会因 u32
   * 反序列化失败整单拒绝,故前端先夹取。
   */
  function termKeepDaysArg() {
    var raw = fieldVal('settings-term-keep-days').trim();
    var n = raw === '' ? 30 : parseInt(raw, 10);
    if (isNaN(n)) n = 30;
    return Math.min(3650, Math.max(0, n));
  }

  /**
   * 资源告警字段采集(第二十四批)。间隔:非数字/空回退 0(关)、夹取
   * 0–1440(一天);阈值:非数字/空回退 90、夹取 0–100(越界直传会被
   * 后端 u32 反序列化失败整单拒绝,同 termKeepDaysArg 的先例)。
   */
  function alertIntervalArg() {
    var raw = fieldVal('settings-alert-interval-input').trim();
    var n = raw === '' ? 0 : parseInt(raw, 10);
    if (isNaN(n)) n = 0;
    return Math.min(1440, Math.max(0, n));
  }

  function alertPercentArg(id) {
    var raw = fieldVal(id).trim();
    var n = raw === '' ? 90 : parseInt(raw, 10);
    if (isNaN(n)) n = 90;
    return Math.min(100, Math.max(0, n));
  }

  /**
   * compose 文件名输入 → 载荷数组(逗号/换行分隔,trim 去空)。
   * 合法性由后端 `compose_scan::normalize_compose_names` 统一裁决
   * (安全边界在拼远端命令处,前端不做重复校验 —— 单点权威)。
   */
  /**
   * 部署日报时刻(B2):空/非法 → null(关闭);否则夹取 0-23。
   * 后端 `digest_hour: Option<u32>`(camelCase `digestHour`)。
   */
  function digestHourArg() {
    var raw = fieldVal('settings-digest-hour').trim();
    if (raw === '') return null;
    var n = parseInt(raw, 10);
    if (isNaN(n)) return null;
    return Math.min(23, Math.max(0, n));
  }

  function composeNamesArg() {
    var raw = fieldVal('settings-compose-names-input');
    if (!raw) return [];
    return raw.split(/[,\n]/).map(function (s) { return s.trim(); })
      .filter(function (s) { return s !== ''; });
  }

  /** 扫描深度 → 载荷(空/非数字回退 0 = 后端按默认 4;越界后端夹取 1-8) */
  function composeDepthArg() {
    var raw = fieldVal('settings-compose-depth-input').trim();
    if (raw === '') return 0;
    var n = parseInt(raw, 10);
    return isNaN(n) ? 0 : n;
  }

  /** 宿主机可写目录白名单:每行一个绝对路径(后端再归一化:丢弃非法项、cap 10) */
  function hostWritePathsArg() {
    var ta = document.getElementById('settings-host-write-paths');
    if (!ta) return [];
    return String(ta.value || '')
      .split(/\r?\n/)
      .map(function (s) { return s.trim(); })
      .filter(function (s) { return s.length > 0; });
  }

  function onSave() {
    if (st.saving) return;
    var session = st.session; // 捕获模态会话,异步收尾校验是否已过期
    st.saving = true;
    setBusy('settings-save-btn', true, '保存中…');
    window.AppBus.invoke('app_settings_set', {
      settings: {
        closeToTray: isChecked('settings-close-tray'),
        proxy: fieldVal('settings-proxy-input').trim(),
        autoUpdateFromSource: isChecked('settings-auto-update-src'),
        probeIntervalMins: (parseInt(fieldVal('settings-probe-interval-input'), 10) || 0),
        autoCheckUpdate: isChecked('settings-auto-check-update'),
        termLogKeepDays: termKeepDaysArg(),
        alertIntervalMins: alertIntervalArg(),
        alertDiskPercent: alertPercentArg('settings-alert-disk-input'),
        alertMemPercent: alertPercentArg('settings-alert-mem-input'),
        alertCpuPercent: alertPercentArg('settings-alert-cpu-input'),
        autoRollbackOnFailure: isChecked('settings-auto-rollback'),
        composeFileNames: composeNamesArg(),
        composeScanMaxDepth: composeDepthArg(),
        tarImage: fieldVal('settings-tar-image-input').trim(),
        digestHour: digestHourArg(),
        autoStart: isChecked('settings-auto-start'),
        hostWritePaths: hostWritePathsArg(),
        incrementalTransfer: isChecked('settings-incremental')
      }
    }).then(function () {
      // 过期会话(保存期间模态被关闭甚至重开)→ 静默丢弃,防旧 promise 回写新模态
      if (!sessionAlive(session) || !isModalVisible()) return;
      st.saving = false;
      setBusy('settings-save-btn', false, '保存设置');
      window.toast('设置已保存', 'ok');
      // 结果区在「更新」分区内(第三十五批):保存时用户多半不在该区,行内回显
      // 既看不见、事后重进该区又变成过期文案 —— 保存成功反馈统一由 toast 承担,
      // 这里只清掉可能残留的旧结果
      showUpdateMessage('info', '');
    }).catch(function (err) {
      if (!sessionAlive(session)) return;
      st.saving = false;
      setBusy('settings-save-btn', false, '保存设置');
      window.formFailLoud('settings-error', '保存设置失败:' + (errText(err) || '未知错误'));
    });
  }

  /** 检查更新 / 测试连接共用:用输入框当前代理值实际拉取一次 GitHub API */
  function runUpdateCheck(session, onOk) {
    st.checking = true;
    setUpdateBusy(true, '检查中…', '测试中…');
    window.AppBus.invoke('update_check', { proxy: proxyArg() })
      .then(function (info) {
        // 过期会话 → 静默丢弃(防重标志已随模态关闭复位,不再回写新模态)
        if (!sessionAlive(session) || !isModalVisible()) return;
        st.checking = false;
        setUpdateBusy(false);
        onOk(info || {});
      })
      .catch(function (err) {
        if (!sessionAlive(session)) return;
        st.checking = false;
        setUpdateBusy(false);
        // 失败原文回显(后端为分类中文提示,附响应原文便于现场排查)
        showUpdateMessage('fail', errText(err) || '未知错误');
      });
  }

  function onCheckUpdate() {
    if (st.checking) return;
    var session = st.session;
    showUpdateMessage('info', '正在检查更新…');
    runUpdateCheck(session, function (info) {
      if (info.hasUpdate === true) {
        renderUpdateAvailable(info);
        // 发现新版本 → 自动弹出更新确认模态(展示更新内容,确认后全自动更新)
        openUpdateConfirmModal(info);
      } else {
        showUpdateMessage('ok', '已是最新:当前版本 v' + (info.current || ''));
      }
    });
  }

  function onTestConnection() {
    if (st.checking) return;
    var session = st.session;
    showUpdateMessage('info', '正在测试连接(GitHub API)…');
    runUpdateCheck(session, function (info) {
      // 成功原文:实际拉到的最新版本即连通性证据
      showUpdateMessage('ok', '连接成功:最新版本 v' + (info.latest || '') +
        '(当前 v' + (info.current || '') + ')');
    });
  }

  /** 打开日志文件夹:open_logs_dir 由资源管理器打开应用目录 logs/(后端确保目录存在) */
  function onOpenLogs() {
    window.AppBus.invoke('open_logs_dir').then(function () {
      window.toast('已打开日志文件夹', 'ok');
    }).catch(function (err) {
      window.toast('打开日志文件夹失败:' + errText(err), 'fail');
    });
  }

  // ===== 模态开合(结构仿 notify-modal:Esc / 遮罩 / 关闭钮三条通道)=====

  function openSettingsModal() {
    var overlay = document.getElementById('settings-modal');
    var body = document.getElementById('settings-modal-body');
    if (!overlay || !body) return;

    st.session += 1; // 开启新会话:此前打开模态发起的异步回调全部视为过期
    buildBody(body);
    overlay.classList.remove('hidden');
    window.modalFocusOpen(overlay);
    showUpdateMessage('info', '正在读取设置…');

    // 外观单选来自 localStorage(即时生效,不经后端);通用/更新来自 app_settings_get
    markSchemeRadios(schemeMode());
    window.AppBus.invoke('app_settings_get')
      .then(function (settings) {
        if (!isModalVisible()) return; // 读取期间已被关闭
        var s = (settings && typeof settings === 'object') ? settings : {};
        setChecked('settings-close-tray', s.closeToTray === true);
        // 缺省(旧 settings.json)视为开启 —— 与后端 serde default 同口径
        setChecked('settings-auto-update-src', s.autoUpdateFromSource !== false);
        var probe = document.getElementById('settings-probe-interval-input');
        // 无条件回填(第二十批 P1 修复):该 input 构建时预填默认值 '0',
        // 若保留 value === '' 守卫则恒假 —— 保存值(如 5)永不回显,用户误以为
        // 未保存而重存 0,经 probe.rs「按设置启停」联动即静默关闭探活。
        if (probe) probe.value = String(s.probeIntervalMins || 0);
        // 启动静默检查更新(第二十一批):缺省视为开启(与后端 serde default 同口径)
        setChecked('settings-auto-check-update', s.autoCheckUpdate !== false);
        // 终端日志保留天数(第二十三批):无条件回填(同探活间隔的 P1 教训 ——
        // 构建时预填默认值会恒假,须覆盖);缺省视为 30(与后端 serde default 同口径)
        var termKeep = document.getElementById('settings-term-keep-days');
        if (termKeep) termKeep.value = String(s.termLogKeepDays == null ? 30 : s.termLogKeepDays);
        // 资源告警(第二十四批):无条件回填(同 P1 教训);缺省视为 0/90
        var alertIv = document.getElementById('settings-alert-interval-input');
        if (alertIv) alertIv.value = String(s.alertIntervalMins || 0);
        // 自动回滚(第二十五批):缺省视为关闭(与后端 serde default 同口径)
        setChecked('settings-auto-rollback', s.autoRollbackOnFailure === true);
        // 扫描识别放宽(第二十六批):名字数组回填为逗号分隔;深度 0/缺省显示 4
        var namesInput = document.getElementById('settings-compose-names-input');
        if (namesInput) {
          namesInput.value = Array.isArray(s.composeFileNames) ? s.composeFileNames.join(', ') : '';
        }
        var depthInput = document.getElementById('settings-compose-depth-input');
        if (depthInput) depthInput.value = String(s.composeScanMaxDepth || 4);
        // 卷搬运 tar 镜像(第二十七批):无条件回填(同 P1 教训);缺省空串
        var tarImg = document.getElementById('settings-tar-image-input');
        if (tarImg) tarImg.value = String(s.tarImage || '');
        // 开机自启(第二十九批 S1):值来自注册表实况(后端已覆盖),无条件回填
        setChecked('settings-auto-start', s.autoStart === true);
        // 部署日报(第二十八批 B2):无条件回填(同 P1 教训);null/缺省 = 关
        var digestH = document.getElementById('settings-digest-hour');
        if (digestH) digestH.value = (s.digestHour === null || s.digestHour === undefined) ? '' : String(s.digestHour);
        var alertDisk = document.getElementById('settings-alert-disk-input');
        if (alertDisk) alertDisk.value = String(s.alertDiskPercent == null ? 90 : s.alertDiskPercent);
        var alertMem = document.getElementById('settings-alert-mem-input');
        if (alertMem) alertMem.value = String(s.alertMemPercent == null ? 90 : s.alertMemPercent);
        var alertCpu = document.getElementById('settings-alert-cpu-input');
        if (alertCpu) alertCpu.value = String(s.alertCpuPercent == null ? 90 : s.alertCpuPercent);
        // 层级增量传输(第三十七批):缺省视为开启(与后端 serde default 同口径)
        setChecked('settings-incremental', s.incrementalTransfer !== false);
        // 代理字段预填 ''(非 '0'),保留 === '' 守卫即可满足「未改动不覆盖」
        var proxy = document.getElementById('settings-proxy-input');
        if (proxy && proxy.value === '') proxy.value = String(s.proxy || '');
        // 宿主机可写目录白名单(第三十四批(五);无条件回填)
        var wp = document.getElementById('settings-host-write-paths');
        if (wp) {
          wp.value = Array.isArray(s.hostWritePaths) ? s.hostWritePaths.join('\n') : '';
        }
        showUpdateMessage('info', '');
      })
      .catch(function (err) {
        if (!isModalVisible()) return;
        // 读取失败必须无条件可见(第三十五批):结果区在「更新」分区内,用它报错
        // 会在默认显示的「外观」区里静默 —— 改走常驻模态顶部的错误框
        window.formFailLoud('settings-error', '读取设置失败:' + (errText(err) || '未知错误'));
      });
  }

  function closeSettingsModal() {
    // 更新确认模态叠在上面时,Esc/遮罩先作用于上层(由其自身守卫决定能否关闭)
    var updateOverlay = updateModal();
    if (updateOverlay && !updateOverlay.classList.contains('hidden')) {
      closeUpdateConfirmModal();
      return;
    }
    var overlay = document.getElementById('settings-modal');
    var body = document.getElementById('settings-modal-body');
    if (overlay) {
      overlay.classList.add('hidden');
      window.modalFocusClose(overlay);
    }
    if (body) body.textContent = ''; // 移除结果区,进行中的异步回写时自动丢弃
    // 会话收尾:复位进行中的防重标志,重开后的新模态可立即操作
    st.saving = false;
    st.checking = false;
  }

  // ===== 初始化(入口按钮 / 模态三通道关闭)=====

  function bindStatic() {
    var entry = document.getElementById('settings-entry-btn');
    if (entry) entry.addEventListener('click', openSettingsModal);

    var closeBtn = document.getElementById('settings-modal-close');
    if (closeBtn) closeBtn.addEventListener('click', closeSettingsModal);

    var overlay = document.getElementById('settings-modal');
    if (overlay) {
      overlay.addEventListener('click', function (e) {
        if (e.target === overlay) closeSettingsModal();
      });
      // Esc 关闭(第二十批 P1 修复:判顶层模态 window.isTopModal,全局仲裁见
      // app.js;更新确认模态叠上面时它自己是顶层,本监听不响应,由其自身
      // 监听关闭 —— closeSettingsModal 内的转发逻辑保留作为双保险)
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && window.isTopModal('settings-modal')) {
          closeSettingsModal();
        }
      });
    }

    // 更新确认模态:关闭钮 / 遮罩 / Esc 三通道(统一走 closeUpdateConfirmModal
    // 守卫 —— 下载进行中 toast 拦截,与 deploy 模态 rbBusy 同模式)
    var ucClose = document.getElementById('update-confirm-modal-close');
    if (ucClose) ucClose.addEventListener('click', closeUpdateConfirmModal);
    var ucOverlay = updateModal();
    if (ucOverlay) {
      ucOverlay.addEventListener('click', function (e) {
        if (e.target === ucOverlay) closeUpdateConfirmModal();
      });
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && window.isTopModal('update-confirm-modal')) {
          closeUpdateConfirmModal();
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
