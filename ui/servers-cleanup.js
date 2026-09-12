// servers-cleanup.js — 清理分析模态(cleanup_preview 预览 → 勾选 → cleanup_execute 定向删除)
// 第十二批 JS 结构治理:自 servers.js 拆出;外部依赖经
// window.ServersKit 桥接(定义见宿主文件尾部),入口经 window.ServersCleanup 供宿主回调。
(function () {
  'use strict';
  var K = window.ServersKit;
  var st = K.st;
  var el = K.el;
  var renderServers = K.renderServers;
  var setLogOpen = K.setLogOpen;

  function cleanupModal() { return document.getElementById('cleanup-modal'); }
  function cleanupBody() { return document.getElementById('cleanup-modal-body'); }

  function closeCleanupModal() {
    var m = cleanupModal();
    if (m) {
      m.classList.add('hidden');
      window.modalFocusClose(m);
    }
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
    window.modalFocusOpen(modal);
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
    var rescan = function () {
      var v = (input.value || '').trim();
      st.cleanupScanRoot = v || (server.remote_dir || '');
      loadCleanupPreview(server);
    };
    btn.addEventListener('click', rescan);
    bar.appendChild(btn);

    // 失焦校验 + Enter 重新扫描(第十三批;与回滚中心扫描起点同款语义:
    // 扫描是只读动作,Enter 直接触发不越过任何确认)
    window.bindFieldValidation(bar, [
      {
        id: 'cleanup-scan-root',
        test: function (val) { return val === '' || val.indexOf('/') === 0; },
        message: '扫描起点需为以 / 开头的绝对路径(如 /home),或留空用服务器部署目录'
      }
    ]);
    window.bindFormEnter(bar, rescan);
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
    body.appendChild(window.confirmBlock({
      title: '确认清理勾选项?',
      facts: [
        ['无标签镜像', selection.imageIds.length + ' 项'],
        ['停止容器', selection.containerIds.length + ' 个'],
        ['未使用卷', selection.volumeNames.length + ' 个'],
        ['构建缓存', selection.builder ? '包含' : '不包含'],
        ['分项目清理', projOps > 0 ? projOps + ' 项' : '无']
      ],
      risk: '该操作不可撤销,删除的数据无法恢复。'
    }));
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

  window.ServersCleanup = { openCleanupModal: openCleanupModal, closeCleanupModal: closeCleanupModal, cleanupModal: cleanupModal };
})();
