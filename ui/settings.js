/* ============================================================
 * settings.js — 设置中心(UPGRADE-PLAN 阶段四「桌面体验」)
 *
 * 入口:dock 底部的齿轮按钮(#settings-entry-btn,主题切换按钮旁),
 * 打开独立模态 #settings-modal(骨架在 index.html,内容由本文件构建)。
 *
 * 后端命令(src-tauri/src/config.rs / update.rs;JS 参数名为 Tauri camelCase):
 * - app_settings_get()
 *     -> { closeToTray: bool, proxy: string }
 *     (独立持久化于 config/settings.json;文件缺失/损坏后端回退默认值)
 * - app_settings_set({ closeToTray, proxy }) -> Ok/Err
 *     (顶层参数包;托盘与「关闭窗口隐藏到托盘」的拦截在后端事件里现读
 *      settings.json,保存后立即生效,无需重启)
 * - update_check({ proxy }) -> { current, latest, hasUpdate, url, notes }
 *     proxy 传设置里的值或 null(空串等价直连,统一传 null);
 *     「检查更新」与「测试连接」共用本命令 —— 测试连接 = 实际拉取一次
 *     GitHub API 并回显成功/失败原文。
 *
 * 交互约定(参照 notify.js 先例):
 * - 三组:外观(主题单选,即时生效)/ 通用(关闭到托盘)/ 更新(代理 + 检查)。
 *   「外观」改动即时写 localStorage['dd_scheme'](与 app.js 同键)并应用;
 *   「通用 / 更新」改动需点「保存设置」(app_settings_set)。
 * - 模态开合带会话序号(st.session,每次打开 +1):读取/保存/检查更新的
 *   异步收尾先校验会话未过期再回写,防止旧 promise 回写重开后的新模态;
 *   关闭模态时复位防重标志。Esc / 遮罩 / 关闭钮均可关闭。
 * - 「检查更新 / 测试连接」使用输入框当前代理值(未保存也能测),进行中
 *   禁用两钮防重复;结果行内回显,有更新时展示 latest + notes 截断 +
 *   「前往下载」(window.open 打开 Release 页)。
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

  function errText(err) {
    if (typeof err === 'string') return err;
    if (err && err.message) return err.message;
    return '';
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

  /** 分组标题(mono 小字,样式见 style.css .set-group-title) */
  function groupTitle(text) {
    return el('div', 'set-group-title', text);
  }

  /** 组内说明文字(弱化段落) */
  function hint(text) {
    return el('div', 'set-hint', text);
  }

  /** 按钮忙碌态切换(执行保存/检查期间禁用防重复) */
  function setBusy(id, busy, label) {
    var btn = document.getElementById(id);
    if (!btn) return;
    btn.disabled = busy;
    if (label) btn.textContent = label;
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
  function buildField(labelText, inputId, inputType, value, placeholder, hint) {
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
    input.spellcheck = false;
    row.appendChild(input);
    if (hint) row.appendChild(el('div', 'form-hint', hint));
    return row;
  }

  function buildBody(body) {
    body.textContent = '';

    // ── 外观 APPEARANCE(单选,即时生效)──
    body.appendChild(groupTitle('外观 APPEARANCE'));
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
    body.appendChild(radioRow);
    body.appendChild(hint(
      '「跟随系统」随操作系统的深色模式实时切换;点击 dock 主题按钮会写入' +
      '显式亮 / 暗并停用跟随'));

    // ── 通用 GENERAL ──
    body.appendChild(groupTitle('通用 GENERAL'));
    body.appendChild(checkboxRow('settings-close-tray', '关闭窗口时隐藏到托盘', false));
    body.appendChild(hint('开启后点关闭仅隐藏窗口(部署继续),托盘菜单「退出」才真正退出;保存后立即生效'));

    // ── 更新 UPDATE ──
    body.appendChild(groupTitle('更新 UPDATE'));
    body.appendChild(buildField('代理地址 PROXY', 'settings-proxy-input', 'text', '',
      '留空直连;支持 http:// 与 socks5://,例如 http://127.0.0.1:7890',
      '仅用于检查更新访问 GitHub;「检查更新 / 测试连接」使用上方输入框当前值,未保存也可测试'));

    // 更新结果区(行内回显 + notes 截断 + 前往下载,内容见 showUpdate*)
    var area = el('div', 'set-update-area');
    area.id = 'settings-update-area';
    body.appendChild(area);

    // ── 底部按钮行:左侧更新组 + 右侧主保存按钮 ──
    var actions = el('div', 'modal-actions settings-actions');
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

    var saveBtn = el('button', 'btn btn-primary', '保存设置');
    saveBtn.type = 'button';
    saveBtn.id = 'settings-save-btn';
    saveBtn.addEventListener('click', onSave);

    actions.appendChild(testGroup);
    actions.appendChild(saveBtn);
    body.appendChild(actions);
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

  /** 有更新:latest + notes 截断展示 + 「前往下载」 */
  function renderUpdateAvailable(info) {
    var area = updateArea();
    if (!area) return;
    area.textContent = '';
    var line = el('div', 'set-result');
    line.appendChild(el('span', 'set-result-ok',
      '发现新版本:v' + info.latest + '(当前 v' + info.current + ')'));
    area.appendChild(line);

    var notes = String(info.notes || '');
    if (notes.length > NOTES_SHOW_MAX) notes = notes.slice(0, NOTES_SHOW_MAX) + '…';
    if (notes) area.appendChild(el('div', 'set-notes', notes));

    var btn = el('button', 'btn btn-primary', '前往下载');
    btn.type = 'button';
    btn.id = 'settings-download-btn';
    btn.addEventListener('click', function () {
      // Release 页面链接(后端 html_url,兜底 releases/latest 页)
      window.AppBus.invoke('open_external', { url: info.url }).catch(function (e) { toast('打开浏览器失败: ' + (e && e.message ? e.message : e), 'fail'); });
    });
    area.appendChild(btn);
  }

  // ===== 动作:保存 / 检查更新 / 测试连接 =====

  /** 代理输入框当前值 → update_check 的 proxy 入参(空串 = 直连 = null) */
  function proxyArg() {
    var value = fieldVal('settings-proxy-input').trim();
    return value === '' ? null : value;
  }

  function onSave() {
    if (st.saving) return;
    var session = st.session; // 捕获模态会话,异步收尾校验是否已过期
    st.saving = true;
    setBusy('settings-save-btn', true, '保存中…');
    window.AppBus.invoke('app_settings_set', {
      settings: {
        closeToTray: isChecked('settings-close-tray'),
        proxy: fieldVal('settings-proxy-input').trim()
      }
    }).then(function () {
      // 过期会话(保存期间模态被关闭甚至重开)→ 静默丢弃,防旧 promise 回写新模态
      if (!sessionAlive(session) || !isModalVisible()) return;
      st.saving = false;
      setBusy('settings-save-btn', false, '保存设置');
      window.toast('设置已保存', 'ok');
      showUpdateMessage('ok', '设置已保存,关闭到托盘与代理已立即生效');
    }).catch(function (err) {
      if (!sessionAlive(session)) return;
      st.saving = false;
      setBusy('settings-save-btn', false, '保存设置');
      window.toast('保存设置失败:' + (errText(err) || '未知错误'), 'fail');
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

  // ===== 模态开合(结构仿 notify-modal:Esc / 遮罩 / 关闭钮三条通道)=====

  function openSettingsModal() {
    var overlay = document.getElementById('settings-modal');
    var body = document.getElementById('settings-modal-body');
    if (!overlay || !body) return;

    st.session += 1; // 开启新会话:此前打开模态发起的异步回调全部视为过期
    buildBody(body);
    overlay.classList.remove('hidden');
    showUpdateMessage('info', '正在读取设置…');

    // 外观单选来自 localStorage(即时生效,不经后端);通用/更新来自 app_settings_get
    markSchemeRadios(schemeMode());
    window.AppBus.invoke('app_settings_get')
      .then(function (settings) {
        if (!isModalVisible()) return; // 读取期间已被关闭
        var s = (settings && typeof settings === 'object') ? settings : {};
        setChecked('settings-close-tray', s.closeToTray === true);
        var proxy = document.getElementById('settings-proxy-input');
        if (proxy && proxy.value === '') proxy.value = String(s.proxy || '');
        showUpdateMessage('info', '');
      })
      .catch(function (err) {
        if (!isModalVisible()) return;
        showUpdateMessage('fail', '读取设置失败:' + (errText(err) || '未知错误'));
      });
  }

  function closeSettingsModal() {
    var overlay = document.getElementById('settings-modal');
    var body = document.getElementById('settings-modal-body');
    if (overlay) overlay.classList.add('hidden');
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
      // Esc 关闭(仅本模态可见时生效,避免误伤其他模态各自的 Esc 监听)
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && !overlay.classList.contains('hidden')) {
          closeSettingsModal();
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
