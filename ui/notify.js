/* ============================================================
 * notify.js — 通知中心(04 部署向导页入口 + #notify-modal 模态)
 *
 * 后端命令(字段 camelCase;notify_save_config / notify_test_email 的
 * 参数包名均为 cfg;密码 None/空 = 后端保留已存密文):
 * - notify_get_config()
 *     -> { desktop:{enabled},
 *          email:{enabled, smtpHost, port, username, passwordSaved,
 *                 security, from, to[]},
 *          events:{onSuccess, onFailure, onCancel} }
 *     (邮件密码脱敏:明文/密文都不出后端,只回传 passwordSaved)
 * - notify_save_config({ cfg:{ desktop:{enabled},
 *       email:{enabled, smtpHost, port, username, password, security,
 *              from, to[]},
 *       events:{onSuccess, onFailure, onCancel} } })
 *     // password 非空字符串 → 后端 DPAPI 加密存储;null/空 → 保留已存值
 * - notify_test_desktop() -> Ok/Err(标题/正文后端固定)
 * - notify_test_email({ cfg:{ smtpHost, port, username, password,
 *       security, from, to[] } }) -> Ok/Err
 *     // 用表单当前值发送,不要求先保存;密码空传 null → 后端解密已存密文
 * 部署成功/失败/取消时后端按事件订阅自动 fire,前端无需接线。
 *
 * 交互约定:
 * - 入口按钮(#notify-entry-btn)位于 04 页 page-tools,内含状态徽标
 *   (#notify-entry-badge):进入 04 页(pagechange)或打开模态时经
 *   notify_get_config 刷新文案。判定口径:
 *     desktop.enabled 且 (email.enabled && smtpHost 非空) → 已配置·桌面+邮件
 *     仅 desktop.enabled                                  → 已配置·桌面
 *     仅 email.enabled && smtpHost 非空                    → 已配置·邮件
 *     其余                                                 → 未配置
 * - 模态(#notify-modal)结构仿 deploy-modal,body 由本文件构建:
 *   打开时拉取配置填表(密码不回传,恒留空,placeholder 按 passwordSaved
 *   显示「已保存(留空保持不变)」/「未设置」);加密方式切换时端口若为
 *   常见默认值(465/587/25)自动跟随选项默认端口,自定义端口不动;
 *   「测试桌面通知 / 发送测试邮件」结果在行内回显原文(失败暗红
 *   --ark-stat-hot,双色纪律下的红色等价物);测试进行中对应按钮禁用
 *   防重复;Esc / 遮罩 / 关闭钮均可关闭(异步收尾回写前校验结果行是否
 *   仍在 DOM,模态已关则静默丢弃)。模态开合带会话序号(st.session,
 *   每次打开 +1):保存/测试的异步收尾先校验会话未过期再回写,保存成功
 *   且模态仍可见时才代为关闭 —— 防止保存中关闭再重开模态被旧 promise
 *   误关;关闭模态时复位 saving/testing 防重标志,避免重开后点击被静默吞。
 * - 收件人 textarea 一行一个:保存与发送测试前按行拆分 + trim + 滤空,
 *   存在无 @ 的非法项时 toast 提示并中止本次操作。
 *
 * 安全说明:与其他页面一致,一律 createElement + textContent 构建,
 * 不使用 innerHTML 拼接;提示一律 toast / 行内回显,不调用系统对话框。
 * ============================================================ */
(function () {
  'use strict';

  // ===== 常量 =====

  /** 加密方式选项(value 与后端 normalize_security 口径一致:ssl|starttls|none) */
  var SECURITY_OPTIONS = [
    { value: 'ssl', label: 'SSL(465)', defaultPort: 465 },
    { value: 'starttls', label: 'STARTTLS(587)', defaultPort: 587 },
    { value: 'none', label: '无加密(25)', defaultPort: 25 }
  ];
  /** 「端口为常见默认值」判定集:切换加密方式时才自动跟随,自定义端口不动 */
  var COMMON_PORTS = ['465', '587', '25'];

  // ===== 状态 =====

  var st = {
    saving: false,         // 保存配置中(防重复提交;随模态关闭复位)
    testingDesktop: false, // 桌面测试通知发送中(防重复;随模态关闭复位)
    testingEmail: false,   // 测试邮件发送中(防重复;随模态关闭复位)
    passwordSaved: false,  // 最近一次 get_config 的 passwordSaved(密码 placeholder)
    session: 0             // 模态会话序号:每次打开 +1,异步收尾据此丢弃过期回调
  };

  // ===== 小工具 =====

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  /** 读取表单字段值(元素缺失返回空串) */
  function fieldVal(id) {
    var node = document.getElementById(id);
    return node ? String(node.value) : '';
  }

  /** 读取复选框勾选态(元素缺失按 false) */
  function isChecked(id) {
    var node = document.getElementById(id);
    return !!(node && node.checked);
  }

  /** 复选框行(deploy-checkbox 体系):label > input + span 文案 */
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

  /** 分组标题(mono 小字,样式见 style.css .notify-group-title) */
  function groupTitle(zh, en) {
    return window.formGroupTitle(zh, en);
  }

  /** 按钮忙碌态切换(执行测试/保存期间禁用防重复;第六批收敛到共享助手) */
  function setBusy(id, busy, label) {
    window.setBtnBusy(document.getElementById(id), busy, label);
  }

  /** 模态可见性(#notify-modal 未带 hidden 类) */
  function isModalVisible() {
    var overlay = document.getElementById('notify-modal');
    return !!(overlay && !overlay.classList.contains('hidden'));
  }

  /** 模态会话是否仍有效:捕获后未再重开过模态(会话序号未变) */
  function sessionAlive(session) {
    return session === st.session;
  }

  // ===== 入口徽标(#notify-entry-badge)=====

  /** 徽标判定与文案(desktop + email.enabled && smtpHost 非空,见文件头口径) */
  function renderBadge(cfg) {
    var badge = document.getElementById('notify-entry-badge');
    if (!badge) return;
    var desk = !!(cfg.desktop && cfg.desktop.enabled);
    var mail = !!(cfg.email && cfg.email.enabled &&
      String(cfg.email.smtpHost || '').trim() !== '');
    var kind;
    var text;
    if (desk && mail) { kind = 'ok'; text = '已配置·桌面+邮件'; }
    else if (desk) { kind = 'ok'; text = '已配置·桌面'; }
    else if (mail) { kind = 'ok'; text = '已配置·邮件'; }
    else { kind = 'info'; text = '未配置'; }
    // fillBadge 重建类名与内容(前置图标 + 文字);间距由 #notify-entry-badge 承载
    window.fillBadge(badge, kind, text);
  }

  /** 进入 04 页(pagechange)时刷新徽标(读取失败不打扰,仅控制台告警) */
  function refreshBadge() {
    window.AppBus.invoke('notify_get_config')
      .then(function (cfg) {
        renderBadge(normalizeCfg(cfg));
      })
      .catch(function (err) {
        if (window.console && console.warn) {
          console.warn('[notify] 刷新通知配置失败:', errText(err) || err);
        }
      });
  }

  // ===== 配置规范化(get_config 视图 → 稳定形状)=====

  function normalizeCfg(cfg) {
    var out = (cfg && typeof cfg === 'object') ? cfg : {};
    var desk = (out.desktop && typeof out.desktop === 'object') ? out.desktop : {};
    var email = (out.email && typeof out.email === 'object') ? out.email : {};
    var events = (out.events && typeof out.events === 'object') ? out.events : {};
    return {
      desktop: { enabled: desk.enabled === true },
      email: {
        enabled: email.enabled === true,
        smtpHost: String(email.smtpHost || ''),
        port: Number(email.port) || 465,
        username: String(email.username || ''),
        passwordSaved: email.passwordSaved === true,
        security: String(email.security || 'ssl'),
        from: String(email.from || ''),
        to: Array.isArray(email.to) ? email.to.map(String) : []
      },
      // 事件订阅缺省口径与后端一致:成功/失败默认开,取消默认关
      events: {
        onSuccess: events.onSuccess !== false,
        onFailure: events.onFailure !== false,
        onCancel: events.onCancel === true
      }
    };
  }

  // ===== 模态 body 构建(每次打开重建,事件随元素重建,无重复绑定)=====

  /**
   * 追加一行输入字段:标签 + 输入框(+ 可选提示);返回输入元素。
   *
   * `zh` / `en` 分离:此前把英文塞进同一段文字(如「SMTP 主机 SMTP HOST」),
   * 导致中英共用同一字体与字距 —— 而契约要求微标签用 Space Grotesk 且字距
   * .08-.18em、中文保持正常字距。改用 window.formLabel 后两者可各自合规。
   */
  function appendField(body, zh, en, inputId, inputType, value, placeholder,
                       hint, inputAttrs) {
    var row = el('div', 'form-row');
    row.appendChild(window.formLabel(zh, en, false, inputId));

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

  /** 加密方式下拉(选项见 SECURITY_OPTIONS;切换时端口自动跟随默认值) */
  function appendSecuritySelect(body) {
    var row = el('div', 'form-row');
    var label = el('label', 'form-label', '加密方式 SECURITY');
    label.setAttribute('for', 'notify-smtp-security');
    row.appendChild(label);

    var select = document.createElement('select');
    select.className = 'form-input';
    select.id = 'notify-smtp-security';
    SECURITY_OPTIONS.forEach(function (opt) {
      var option = document.createElement('option');
      option.value = opt.value;
      option.textContent = opt.label;
      select.appendChild(option);
    });
    select.addEventListener('change', onSecurityChange);
    row.appendChild(select);
    row.appendChild(el('div', 'form-hint',
      '切换加密方式时,端口为常见默认值(465/587/25)会自动跟随,自定义端口不动'));
    body.appendChild(row);
    return select;
  }

  /** 收件人多行输入(textarea,一行一个地址) */
  function appendRecipientsField(body) {
    var row = el('div', 'form-row');
    var label = el('label', 'form-label', '收件人 TO(每行一个)');
    label.setAttribute('for', 'notify-email-to');
    row.appendChild(label);

    var ta = document.createElement('textarea');
    ta.className = 'form-textarea';
    ta.id = 'notify-email-to';
    ta.rows = 3;
    ta.spellcheck = false;
    ta.autocomplete = 'off';
    ta.placeholder = '例如:me@example.com';
    row.appendChild(ta);
    row.appendChild(el('div', 'form-hint',
      '一行一个收件人地址;保存与发送测试前会逐个校验(需包含 @)'));
    body.appendChild(row);
    return ta;
  }

  function buildBody(body) {
    body.textContent = '';
    st.passwordSaved = false;
    window.formClearError('notify-error');

    // 内联错误框(本轮补齐):此前 notify 表单保存/校验失败只有 toast ——
    // 这里是长表单(桌面/邮件/事件三组),用户停在底部按钮处时顶部内容已不可见,
    // toast 2.5s 后消失就再无线索。走全局统一三通道(内联 + 滚动 + toast)。
    var errBox = window.formErrorBox('notify-error');
    body.appendChild(errBox);

    // ── 桌面通知 ──
    body.appendChild(groupTitle('桌面通知', 'DESKTOP'));
    body.appendChild(checkboxRow('notify-desktop-enabled', '部署完成时显示系统通知', false));

    // ── 邮件通知 ──
    body.appendChild(groupTitle('邮件通知', 'EMAIL'));
    body.appendChild(checkboxRow('notify-email-enabled', '启用邮件通知', false));
    appendField(body, 'SMTP 主机', 'SMTP HOST', 'notify-smtp-host', 'text', '',
      '例如:smtp.example.com',
      '服务器填错会导致测试邮件与部署通知都发不出去');
    appendField(body, 'SMTP 端口', 'PORT', 'notify-smtp-port', 'number', '465',
      null, '留空按加密方式取默认端口(SSL 465 / STARTTLS 587 / 不加密 25)',
      { min: 1, max: 65535, step: 1 });
    appendField(body, '用户名', 'USERNAME', 'notify-smtp-username', 'text', '',
      '留空表示不认证', '多数 SMTP 服务的用户名是完整邮箱地址');
    // 密码:不回填明文,placeholder 由 fillForm 按 passwordSaved 决定
    appendField(body, '密码', 'PASSWORD', 'notify-smtp-password', 'password', '',
      '未设置', '留空表示沿用已保存密码;密码以系统加密存储,不回显');
    appendSecuritySelect(body);
    appendField(body, '发件人', 'FROM', 'notify-email-from', 'text', '',
      '例如:bot@example.com(可用「名称 <addr>」格式)');
    appendRecipientsField(body);

    // ── 事件订阅 ──
    body.appendChild(groupTitle('事件订阅', 'EVENTS'));
    body.appendChild(checkboxRow('notify-event-success', '部署成功', true));
    body.appendChild(checkboxRow('notify-event-failure', '部署失败', true));
    body.appendChild(checkboxRow('notify-event-cancel', '部署取消', false));

    // ── 测试结果行(行内回显,内容见 showResult)──
    var result = el('div', 'notify-test-result');
    result.id = 'notify-test-result';
    body.appendChild(result);

    // ── 底部按钮行:左侧测试组 + 右侧主保存按钮 ──
    var actions = el('div', 'modal-actions notify-actions');
    var testGroup = el('div', 'notify-test-group');

    var deskBtn = el('button', 'btn', '测试桌面通知');
    deskBtn.type = 'button';
    deskBtn.id = 'notify-test-desktop-btn';
    deskBtn.addEventListener('click', onTestDesktop);
    testGroup.appendChild(deskBtn);

    var mailBtn = el('button', 'btn', '发送测试邮件');
    mailBtn.type = 'button';
    mailBtn.id = 'notify-test-email-btn';
    mailBtn.addEventListener('click', onTestEmail);
    testGroup.appendChild(mailBtn);

    var saveBtn = el('button', 'btn btn-primary', '保存配置');
    saveBtn.type = 'button';
    saveBtn.id = 'notify-save-btn';
    saveBtn.addEventListener('click', onSave);

    actions.appendChild(testGroup);
    actions.appendChild(saveBtn);
    body.appendChild(actions);
  }

  // ===== 填表(get_config 结果 → 表单;密码恒留空不回填)=====

  function setChecked(id, checked) {
    var node = document.getElementById(id);
    if (node) node.checked = checked === true;
  }

  function setValue(id, value) {
    var node = document.getElementById(id);
    if (node) node.value = value === undefined || value === null ? '' : String(value);
  }

  function fillForm(c) {
    setChecked('notify-desktop-enabled', c.desktop.enabled);
    setChecked('notify-email-enabled', c.email.enabled);
    setValue('notify-smtp-host', c.email.smtpHost);
    setValue('notify-smtp-port', c.email.port > 0 ? c.email.port : 465);
    setValue('notify-smtp-username', c.email.username);

    var pwd = document.getElementById('notify-smtp-password');
    if (pwd) {
      pwd.value = '';
      pwd.placeholder = c.email.passwordSaved ? '已保存(留空保持不变)' : '未设置';
    }
    st.passwordSaved = c.email.passwordSaved;

    // 加密方式:非法值按 ssl 兜底(与后端 normalize_security 口径一致)
    var security = String(c.email.security || 'ssl').trim().toLowerCase();
    var known = SECURITY_OPTIONS.some(function (opt) { return opt.value === security; });
    setValue('notify-smtp-security', known ? security : 'ssl');

    setValue('notify-email-from', c.email.from);
    var ta = document.getElementById('notify-email-to');
    if (ta) ta.value = c.email.to.join('\n');

    setChecked('notify-event-success', c.events.onSuccess);
    setChecked('notify-event-failure', c.events.onFailure);
    setChecked('notify-event-cancel', c.events.onCancel);
  }

  // ===== 收集与校验(保存 / 测试邮件共用)=====

  /** 收件人解析:按行拆分 + trim + 滤空;无 @ 的非法项 toast 提示并返回 null */
  function parseRecipients() {
    var raw = fieldVal('notify-email-to');
    var list = [];
    var invalid = [];
    raw.split(/\r?\n/).forEach(function (line) {
      var addr = line.trim();
      if (addr === '') return;
      if (addr.indexOf('@') < 0) {
        invalid.push(addr);
        return;
      }
      list.push(addr);
    });
    if (invalid.length > 0) {
      window.formFailLoud('notify-error',
        '收件人地址无效(需包含 @):' + invalid.join('、'));
      window.setFieldError(document.getElementById('notify-email-to'),
        '需为含 @ 的邮箱地址,一行一个');
      return null;
    }
    return list;
  }

  /**
   * 收集表单 → notify_save_config 的 cfg 入参(测试邮件仅取其 email 部分);
   * 收件人校验失败时已提示并返回 null。端口非法/为 0 传 0
   * (后端按加密方式兜底默认端口:ssl→465、starttls→587、none→25)。
   */
  function collectForm() {
    var to = parseRecipients();
    if (to === null) return null;

    var port = parseInt(fieldVal('notify-smtp-port').trim(), 10);
    if (!isFinite(port) || port <= 0) port = 0;
    if (port > 65535) port = 65535;

    var password = fieldVal('notify-smtp-password').trim();

    return {
      desktop: { enabled: isChecked('notify-desktop-enabled') },
      email: {
        enabled: isChecked('notify-email-enabled'),
        smtpHost: fieldVal('notify-smtp-host').trim(),
        port: port,
        username: fieldVal('notify-smtp-username').trim(),
        // 非空字符串 → 后端加密存储;null/空 → 保留已存密文
        password: password !== '' ? password : null,
        security: securityValue(),
        from: fieldVal('notify-email-from').trim(),
        to: to
      },
      events: {
        onSuccess: isChecked('notify-event-success'),
        onFailure: isChecked('notify-event-failure'),
        onCancel: isChecked('notify-event-cancel')
      }
    };
  }

  /** 加密方式取值(选项外的值按 ssl 兜底) */
  function securityValue() {
    var value = fieldVal('notify-smtp-security');
    for (var i = 0; i < SECURITY_OPTIONS.length; i++) {
      if (SECURITY_OPTIONS[i].value === value) return value;
    }
    return 'ssl';
  }

  // ===== 测试结果行(行内回显;失败用 --ark-stat-hot 暗红)=====

  function showResult(kind, text) {
    var line = document.getElementById('notify-test-result');
    if (!line) return; // 模态已关闭(body 已清空):静默丢弃过期结果
    line.textContent = '';
    if (text) line.appendChild(el('span', 'notify-test-' + kind, text));
  }

  // ===== 动作:保存 / 测试桌面 / 测试邮件 =====

  function onSave() {
    if (st.saving) return;
    var cfg = collectForm();
    if (!cfg) return;

    var session = st.session; // 捕获模态会话,异步收尾校验是否已过期
    st.saving = true;
    setBusy('notify-save-btn', true, '保存中…');
    window.AppBus.invoke('notify_save_config', { cfg: cfg })
      .then(function () {
        // 过期会话(保存期间模态被关闭甚至重开)→ 静默丢弃:
        // 不回写 UI、不代关模态,防止旧 promise 误关重开后的新模态
        if (!sessionAlive(session)) return;
        st.saving = false;
        setBusy('notify-save-btn', false, '保存配置');
        window.toast('通知配置已保存', 'ok');
        renderBadge(normalizeCfg(cfg)); // 表单值即新配置,徽标即时同步
        // 仅模态仍可见时代为关闭(保存期间被关闭且未重开则无需再关)
        if (isModalVisible()) closeNotifyModal();
      })
      .catch(function (err) {
        if (!sessionAlive(session)) return; // 过期会话:静默丢弃
        st.saving = false;
        setBusy('notify-save-btn', false, '保存配置');
        window.formFailLoud('notify-error',
          '保存通知配置失败:' + (errText(err) || '未知错误'));
      });
  }

  function onTestDesktop() {
    if (st.testingDesktop) return;
    var session = st.session; // 捕获模态会话,异步收尾校验是否已过期
    st.testingDesktop = true;
    setBusy('notify-test-desktop-btn', true, '发送中…');
    showResult('info', '正在发送测试通知…');

    window.AppBus.invoke('notify_test_desktop')
      .then(function () {
        // 过期会话 → 静默丢弃(防重标志已随模态关闭复位,不再回写新模态)
        if (!sessionAlive(session)) return;
        st.testingDesktop = false;
        setBusy('notify-test-desktop-btn', false, '测试桌面通知');
        // Ok 无载荷:文案为前端固定提示(Err 的原文走 catch 分支)
        showResult('ok', '测试通知已发送,请查看系统通知');
      })
      .catch(function (err) {
        if (!sessionAlive(session)) return;
        st.testingDesktop = false;
        setBusy('notify-test-desktop-btn', false, '测试桌面通知');
        showResult('fail', errText(err) || '未知错误');
      });
  }

  function onTestEmail() {
    if (st.testingEmail) return;
    var cfg = collectForm(); // 表单当前值,不要求先保存
    if (!cfg) return;

    var session = st.session; // 捕获模态会话,异步收尾校验是否已过期
    st.testingEmail = true;
    setBusy('notify-test-email-btn', true, '发送中…');
    showResult('info', '正在发送测试邮件…');

    // 密码空传 null:后端解密已存密文使用(都没有则后端报错原文)
    window.AppBus.invoke('notify_test_email', {
      cfg: {
        smtpHost: cfg.email.smtpHost,
        port: cfg.email.port,
        username: cfg.email.username,
        password: cfg.email.password,
        security: cfg.email.security,
        from: cfg.email.from,
        to: cfg.email.to
      }
    }).then(function () {
      // 过期会话 → 静默丢弃(防重标志已随模态关闭复位,不再回写新模态)
      if (!sessionAlive(session)) return;
      st.testingEmail = false;
      setBusy('notify-test-email-btn', false, '发送测试邮件');
      showResult('ok', '测试邮件已发送,请查收:' + cfg.email.to.join('、'));
    }).catch(function (err) {
      if (!sessionAlive(session)) return;
      st.testingEmail = false;
      setBusy('notify-test-email-btn', false, '发送测试邮件');
      showResult('fail', errText(err) || '未知错误');
    });
  }

  // ===== 加密方式 → 端口自动跟随(仅常见默认值)=====

  function onSecurityChange() {
    var select = document.getElementById('notify-smtp-security');
    var port = document.getElementById('notify-smtp-port');
    if (!select || !port) return;
    var current = String(port.value).trim();
    if (COMMON_PORTS.indexOf(current) < 0) return; // 自定义端口不动
    for (var i = 0; i < SECURITY_OPTIONS.length; i++) {
      if (SECURITY_OPTIONS[i].value === select.value) {
        port.value = String(SECURITY_OPTIONS[i].defaultPort);
        return;
      }
    }
  }

  // ===== 模态开合(结构仿 deploy-modal:Esc / 遮罩 / 关闭钮三条通道)=====

  function openNotifyModal() {
    var overlay = document.getElementById('notify-modal');
    var body = document.getElementById('notify-modal-body');
    if (!overlay || !body) return;

    st.session += 1; // 开启新会话:此前打开模态发起的异步回调全部视为过期
    buildBody(body);
    overlay.classList.remove('hidden');
    window.modalFocusOpen(overlay);
    showResult('info', '正在读取通知配置…');

    window.AppBus.invoke('notify_get_config')
      .then(function (cfg) {
        if (overlay.classList.contains('hidden')) return; // 读取期间已被关闭
        var c = normalizeCfg(cfg);
        renderBadge(c); // 打开模态同时刷新入口徽标(口径:进入 04 页或打开模态)
        fillForm(c);
        showResult('info', '');
      })
      .catch(function (err) {
        if (overlay.classList.contains('hidden')) return;
        showResult('fail', '读取通知配置失败:' + (errText(err) || '未知错误'));
      });
  }

  function closeNotifyModal() {
    var overlay = document.getElementById('notify-modal');
    var body = document.getElementById('notify-modal-body');
    if (overlay) {
      overlay.classList.add('hidden');
      window.modalFocusClose(overlay);
    }
    if (body) body.textContent = ''; // 移除结果行,进行中的测试回写时自动丢弃
    // 会话收尾:复位进行中的防重标志,重开后的新模态可立即操作
    // (进行中的异步收尾经会话校验发现过期后不会再回写这些标志)
    st.saving = false;
    st.testingDesktop = false;
    st.testingEmail = false;
  }

  // ===== 初始化(入口按钮 / 模态三通道关闭 / pagechange 徽标刷新)=====

  function bindStatic() {
    var entry = document.getElementById('notify-entry-btn');
    if (entry) entry.addEventListener('click', openNotifyModal);

    var closeBtn = document.getElementById('notify-modal-close');
    if (closeBtn) closeBtn.addEventListener('click', closeNotifyModal);

    var overlay = document.getElementById('notify-modal');
    if (overlay) {
      overlay.addEventListener('click', function (e) {
        if (e.target === overlay) closeNotifyModal();
      });
      // Esc 关闭(仅本模态可见时生效,避免误伤其他模态各自的 Esc 监听)
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && !overlay.classList.contains('hidden')) {
          closeNotifyModal();
        }
      });
    }

    // 进入 04 部署向导页时刷新入口徽标(参照 manage.js 的 pagechange 用法)
    window.addEventListener('pagechange', function (e) {
      if (e && e.detail && e.detail.page === 'deploy') refreshBadge();
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', bindStatic);
  } else {
    bindStatic();
  }
})();
