/* ============================================================
 * config-io.js — 配置中心(导出 / 导入 / 危险区清除数据)
 *
 * 入口:03 服务器管理页 page-tools 的「配置中心」按钮(#config-io-btn),
 * 打开独立模态 #config-io-modal(骨架在 index.html,内容由本文件构建)。
 *
 * 后端命令(src-tauri/src/config_io.rs,JS 参数名为 Tauri camelCase):
 * - config_export_file({ password, path }) -> Ok/Err
 *     收集 servers / projects / notify → Argon2id(口令)+ AES-256-GCM
 *     加密 → 写入 JSON 信封文件;口令为空后端报「导出口令不能为空」
 * - config_import_file({ path, password }) -> ImportSummary
 *     ImportSummary = { servers: number, projects: number }(camelCase 序列化);
 *     原子覆盖 servers.json / projects.json / notify.json;成功后前端刷新应用
 * - config_wipe() -> Ok/Err
 *     删除 servers / projects / notify / deployments(部署历史)四个配置文件,
 *     运行日志保留;不可恢复
 *
 * 对话框:
 * - 导入选文件:AppBus.pickPath(tauri-plugin-dialog 的 dialog.open 封装,
 *   支持 filters)→ filters *.json
 * - 导出到…:AppBus.pickPath 只封装了 dialog.open(打开文件/目录),不支持
 *   保存对话框,故直接调用 window.__TAURI__.dialog.save(与 pickPath 同款
 *   防御封装:组件不可用/调用失败 → toast + null),filters 限定 *.json
 *
 * 安全说明:动态内容一律 createElement + textContent,不使用 innerHTML;
 * 口令输入框 type=password 且不回显;任一操作进行中禁用全部操作按钮。
 * ============================================================ */
(function () {
  'use strict';

  var st = {
    importPath: '',  // 已选择的备份文件路径(取消选择时保持原值)
    busy: false      // 任一操作进行中(禁用导出/导入/清除按钮防重复触发)
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

  /** 取输入框当前值(不 trim:口令可能含首尾空格,按原样使用) */
  function passVal(id) {
    var node = document.getElementById(id);
    return node ? String(node.value) : '';
  }

  /** 分组标题(中文 + 大写英文;样式见 .form-group-title) */
  function groupTitle(zh, en) {
    return window.formGroupTitle(zh, en);
  }

  /** 组内说明文字(两行以内的弱化段落) */
  function note(text) {
    return el('div', 'cio-note', text);
  }

  /**
   * 操作按钮统一禁用/恢复(导出/导入/浏览/清除四处)。
   *
   * 第六批:这里**刻意不接** window.setBtnBusy —— 四处是一个按钮组、同时进入
   * 忙碌态,若每个都挂步进条会有四条同时跑,视觉噪音过大;而它们没有单按钮
   * 语义,也不该各自改文案。各操作自身有行内回显,反馈不缺。
   */
  function setBusy(busy) {
    st.busy = busy;
    ['cio-export-btn', 'cio-import-btn', 'cio-import-browse', 'cio-wipe-btn']
      .forEach(function (id) {
        var btn = document.getElementById(id);
        if (btn) btn.disabled = busy;
      });
  }

  // ===== 保存对话框(直接调用 dialog.save;pickPath 仅封装了 dialog.open)=====

  /**
   * 系统保存文件对话框(tauri-plugin-dialog 的 dialog.save 薄封装)。
   * @param {Object} opts { title:string, defaultPath:string, filters:Array }
   * @returns {Promise<string|null>} 保存路径;取消/组件不可用/调用失败返回 null
   */
  function pickSavePath(opts) {
    var dialog = (window.__TAURI__ || {}).dialog;
    if (!dialog || typeof dialog.save !== 'function') {
      window.toast('对话框组件不可用,请在桌面窗口中运行', 'fail');
      return Promise.resolve(null);
    }
    return dialog.save(opts).then(function (picked) {
      return (typeof picked === 'string' && picked) ? picked : null;
    }, function (err) {
      window.toast('打开保存对话框失败:' + (err && err.message ? err.message : err), 'fail');
      return null;
    });
  }

  /** 导出文件默认名:dockerdeploy-backup-YYYYMMDD-HHMMSS.json */
  function defaultExportName() {
    var d = new Date();
    function p(n) { return ('0' + n).slice(-2); }
    return 'dockerdeploy-backup-' + d.getFullYear() + p(d.getMonth() + 1) +
      p(d.getDate()) + '-' + p(d.getHours()) + p(d.getMinutes()) + p(d.getSeconds()) + '.json';
  }

  // ===== 导出 =====

  function runExport() {
    var pass = passVal('cio-export-pass');
    var pass2 = passVal('cio-export-pass2');
    if (!pass) {
      window.formFailLoud('cio-error', '请输入导出口令');
      window.setFieldError(document.getElementById('cio-export-pass'), '此项必填');
      return;
    }
    if (pass !== pass2) {
      window.formFailLoud('cio-error', '两次输入的导出口令不一致');
      window.setFieldError(document.getElementById('cio-export-pass-confirm'), '与上格口令不一致');
      return;
    }
    if (pass.length < 6) {
      window.toast('口令少于 6 位,建议使用更长的口令', 'warn'); // 提示但允许
    }

    pickSavePath({
      title: '导出配置备份',
      defaultPath: defaultExportName(),
      filters: [{ name: '配置备份文件', extensions: ['json'] }]
    }).then(function (path) {
      if (!path) return; // 用户取消
      setBusy(true);
      window.AppBus.invoke('config_export_file', { password: pass, path: path })
        .then(function () {
          window.toast('已导出到 ' + path, 'ok');
        })
        .catch(function (err) {
          window.formFailLoud('cio-error', '导出失败:' + (errText(err) || '未知错误'));
        })
        .then(function () {
          setBusy(false);
        });
    });
  }

  // ===== 导入 =====

  function browseImportFile() {
    window.AppBus.pickPath({
      directory: false,
      title: '选择配置备份文件',
      filters: [{ name: '配置备份文件', extensions: ['json'] }]
    }).then(function (picked) {
      if (picked === null) return;
      st.importPath = picked;
      var input = document.getElementById('cio-import-path');
      if (input) input.value = picked;
    });
  }

  function runImport() {
    var result = document.getElementById('cio-import-result');
    function showResult(text, cls) {
      if (result) {
        result.textContent = text;
        result.className = 'cio-result' + (cls ? ' ' + cls : '');
      }
    }

    if (!st.importPath) {
      window.formFailLoud('cio-error', '请先选择配置备份文件');
      return;
    }
    var pass = passVal('cio-import-pass');
    if (!pass) {
      window.formFailLoud('cio-error', '请输入文件口令');
      window.setFieldError(document.getElementById('cio-import-pass'), '此项必填');
      return;
    }

    setBusy(true);
    showResult('正在导入…', 'cio-result-ok');
    window.AppBus.invoke('config_import_file', { path: st.importPath, password: pass })
      .then(function (summary) {
        var s = summary || {};
        var servers = Number(s.servers) || 0;
        var projects = Number(s.projects) || 0;
        showResult('已导入 ' + servers + ' 台服务器 / ' + projects +
          ' 个项目,将刷新应用…', 'cio-result-ok');
        window.toast('导入成功,将刷新应用', 'ok');
        // 摘要停留一拍再刷新,让内存中的新配置随页面重建重新加载
        window.setTimeout(function () { window.location.reload(); }, 1600);
      })
      .catch(function (err) {
        showResult('导入失败:' + (errText(err) || '未知错误'), 'cio-result-fail');
        window.formFailLoud('cio-error', '导入失败:' + (errText(err) || '未知错误'));
        setBusy(false);
      });
  }

  // ===== 危险区:清除数据 =====

  function runWipe() {
    var input = document.getElementById('cio-wipe-confirm');
    if (!input || String(input.value).trim() !== 'DELETE') return;
    setBusy(true);
    window.AppBus.invoke('config_wipe')
      .then(function () {
        window.toast('已清除全部配置数据,将刷新应用', 'ok');
        window.setTimeout(function () { window.location.reload(); }, 1200);
      })
      .catch(function (err) {
        window.formFailLoud('cio-error', '清除失败:' + (errText(err) || '未知错误'));
        setBusy(false);
      });
  }

  function bindWipeConfirm() {
    var input = document.getElementById('cio-wipe-confirm');
    var btn = document.getElementById('cio-wipe-btn');
    if (!input || !btn) return;
    btn.disabled = true;
    input.addEventListener('input', function () {
      btn.disabled = st.busy || String(input.value).trim() !== 'DELETE';
    });
  }

  // ===== 模态框组装 =====

  function appendExportGroup(body) {
    body.appendChild(groupTitle('导出', 'EXPORT'));
    body.appendChild(note(
      '将全部服务器 / 部署项目 / 通知配置导出为加密备份文件;' +
      'SSH 密码与私钥口令以加密形式写入文件,导出口令用于派生加密密钥' +
      '(建议 6 位以上,不作强制)'));
    body.appendChild(buildField('导出口令', 'cio-export-pass', 'password', '用于加密备份文件'));
    body.appendChild(buildField('确认口令', 'cio-export-pass2', 'password', '再次输入同一口令'));

    var row = el('div', 'form-actions');
    var btn = el('button', 'btn btn-primary', '导出到…');
    btn.type = 'button';
    btn.id = 'cio-export-btn';
    btn.addEventListener('click', function () { runExport(); });
    row.appendChild(btn);
    body.appendChild(row);
  }

  function appendImportGroup(body) {
    body.appendChild(groupTitle('导入', 'IMPORT'));
    body.appendChild(note(
      '从备份文件恢复配置,导入将整体覆盖当前的服务器 / 部署项目 / 通知配置'));

    // 备份文件:只读路径展示 + 「选择文件…」(AppBus.pickPath,filters *.json)
    var fileRow = el('div', 'form-row');
    fileRow.appendChild(el('label', 'form-label', '备份文件'));
    var line = el('div', 'input-btn-row');
    var pathInput = document.createElement('input');
    pathInput.className = 'form-input';
    pathInput.id = 'cio-import-path';
    pathInput.type = 'text';
    pathInput.readOnly = true;
    pathInput.autocomplete = 'off';
    pathInput.placeholder = '尚未选择文件';
    line.appendChild(pathInput);
    var browseBtn = el('button', 'btn', '选择文件…');
    browseBtn.type = 'button';
    browseBtn.id = 'cio-import-browse';
    browseBtn.addEventListener('click', function () { browseImportFile(); });
    line.appendChild(browseBtn);
    fileRow.appendChild(line);
    body.appendChild(fileRow);

    body.appendChild(buildField('文件口令', 'cio-import-pass', 'password',
      '导出该备份文件时设置的口令'));

    var row = el('div', 'form-actions');
    var importBtn = el('button', 'btn btn-primary', '导入并覆盖当前配置');
    importBtn.type = 'button';
    importBtn.id = 'cio-import-btn';
    importBtn.addEventListener('click', function () { runImport(); });
    row.appendChild(importBtn);
    body.appendChild(row);

    var result = el('div', 'cio-result');
    result.id = 'cio-import-result';
    body.appendChild(result);
  }

  function appendDangerGroup(body) {
    var box = el('div', 'cio-danger-box');
    // 危险区标题保留自己的视觉(已有专属 .cio-danger-title),仅把英文拆出
    var dangerTitle = el('div', 'cio-danger-title', '危险区');
    dangerTitle.appendChild(el('span', 'form-label-en', 'DANGER ZONE'));
    box.appendChild(dangerTitle);
    box.appendChild(el('div', 'cio-danger-text',
      '将删除全部服务器、部署项目、通知配置与部署历史;运行日志保留。' +
      '操作立即生效且不可恢复。'));

    var row = el('div', 'form-row');
    var label = el('label', 'form-label', '确认操作');
    label.setAttribute('for', 'cio-wipe-confirm');
    row.appendChild(label);
    var input = document.createElement('input');
    input.className = 'form-input';
    input.id = 'cio-wipe-confirm';
    input.type = 'text';
    input.autocomplete = 'off';
    input.placeholder = '输入 DELETE 以确认清除';
    row.appendChild(input);
    box.appendChild(row);

    var btn = el('button', 'btn btn-danger', '永久清除');
    btn.type = 'button';
    btn.id = 'cio-wipe-btn';
    btn.disabled = true; // 输入 DELETE 前保持禁用(bindWipeConfirm 接管)
    btn.addEventListener('click', function () { runWipe(); });
    box.appendChild(btn);

    body.appendChild(box);
  }

  /** 单字段行:标签 + 输入框(+ 可选提示) */
  function buildField(labelText, inputId, inputType, hint) {
    var row = el('div', 'form-row');
    var label = el('label', 'form-label', labelText);
    label.setAttribute('for', inputId);
    row.appendChild(label);
    var input = document.createElement('input');
    input.className = 'form-input';
    input.id = inputId;
    input.type = inputType;
    input.autocomplete = inputType === 'password' ? 'new-password' : 'off';
    row.appendChild(input);
    if (hint) row.appendChild(el('div', 'form-hint', hint));
    return row;
  }

  function openConfigIoModal() {
    st.importPath = '';
    st.busy = false;
    var overlay = document.getElementById('config-io-modal');
    var body = document.getElementById('config-io-modal-body');
    if (!overlay || !body) return;
    body.textContent = '';
    // 内联错误框(本轮补齐):此前导出/导入/清除失败只有 toast —— 三组内容
    // 都在同一模态里,用户停在按钮处时看不到已消失的提示。统一走三通道
    // (内联框 + 滚动到可视区 + toast)。
    body.appendChild(window.formErrorBox('cio-error'));
    appendExportGroup(body);
    appendImportGroup(body);
    appendDangerGroup(body);
    bindWipeConfirm();
    overlay.classList.remove('hidden');
  }

  function closeConfigIoModal() {
    var overlay = document.getElementById('config-io-modal');
    if (overlay) overlay.classList.add('hidden');
  }

  // ===== 初始化(入口按钮 / 模态三通道关闭)=====

  function bindStatic() {
    var entry = document.getElementById('config-io-btn');
    if (entry) entry.addEventListener('click', openConfigIoModal);

    var closeBtn = document.getElementById('config-io-modal-close');
    if (closeBtn) closeBtn.addEventListener('click', closeConfigIoModal);

    var overlay = document.getElementById('config-io-modal');
    if (overlay) {
      overlay.addEventListener('click', function (e) {
        if (e.target === overlay) closeConfigIoModal();
      });
      // Esc 关闭(仅本模态可见时生效,避免误伤其他模态各自的 Esc 监听)
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && !overlay.classList.contains('hidden')) {
          closeConfigIoModal();
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
