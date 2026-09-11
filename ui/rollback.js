/**
 * 回滚中心(06 页,第三批新增独立模块)。
 *
 * 定位:现有回滚入口埋在「部署向导 → 部署历史」里,只能回到本应用部署过的项目。
 * 本页以**服务器真实目录**为准扫描项目(文件系统 compose + docker compose labels
 * 合并),列出每个项目的发布归档与旧版本镜像标签,提供两级回滚:
 *   - 整栈回滚到某个归档(rollback_execute_stack_at:重载镜像包 + 恢复 compose 副本 + up -d)
 *   - 单镜像切回某个日期标签(需要该目录 compose 声明的仓库名与标签)
 *
 * 后端命令(见 src-tauri/src/commands.rs):
 *   - rollback_scan_projects({ serverId, passwordPlain?, scanRoot? }) -> RollbackProject[]
 *   - rollback_project_detail({ serverId, passwordPlain?, dir }) -> RollbackProjectDetail
 *   - rollback_execute_stack_at({ serverId, passwordPlain?, dir, releaseTs }) -> null(结果走 deploy-log/deploy-done)
 *   - rollback_execute_single({ serverId, passwordPlain?, projectId, repository, dateTag, targetRef }) -> null
 *
 * 约定(与 manage.js 一致):
 *   - IIFE + DOMContentLoaded 绑定;pagechange 进出页面;离页停止监听与计时;
 *   - 事件订阅先于 invoke(防竞态);单一事件监听守卫;
 *   - 日志与 deploy-log 共用同一套事件,但写入本页独立日志面板。
 *
 * 注意:单镜像回滚依赖应用内项目(rollback_execute_single 需要 projectId 与
 * compose 文件配置),因此本页只对「已配置项目」显示标签回滚按钮;未配置的
 * 项目提示去「服务器管理」新增项目。
 */
(function () {
  'use strict';

  var LOG_MAX_LINES = 1000;
  /** 待确认的回滚计划(确认按钮 onConfirm 用) */
  var pending = null;
  var logLines = [];
  var logBound = false;
  var currentProjects = [];
  var selectedDir = null;
  var detailCache = null;
  var busy = false;

  function $(id) { return document.getElementById(id); }

  function el(tag, cls, text) {
    var node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function errText(err) {
    if (!err) return '未知错误';
    if (typeof err === 'string') return err;
    return err.message || String(err);
  }

  function showError(msg) {
    var box = $('rollback-error');
    if (!box) return;
    box.textContent = msg || '';
    box.classList.toggle('hidden', !msg);
  }

  // 刷新/重扫是一对组按钮(同时禁用),同 config-io 的理由不接 setBtnBusy 的
  // 步进条(两条同跑无意义);禁用防重复即可。
  function setBusy(v) {
    busy = v;
    var btn = $('rollback-refresh-btn');
    if (btn) btn.disabled = v;
    var rescan = $('rollback-rescan-btn');
    if (rescan) rescan.disabled = v;
  }

  // ===== 日志面板 =====

  function bindLogEvents() {
    if (logBound) return;
    logBound = true;
    try {
      window.__TAURI__.event.listen('deploy-log', function (e) {
        appendLog(typeof e.payload === 'string' ? e.payload : String(e.payload || ''));
      });
      window.__TAURI__.event.listen('deploy-done', function (e) {
        var p = e.payload || {};
        appendLog(p.success ? ('✔ ' + (p.message || '回滚完成')) : ('✘ ' + (p.message || '回滚失败')));
        setBusy(false);
        // 结束后刷新明细(归档/标签可能已变化)与项目列表
        if (selectedDir) loadDetail(selectedDir, true);
        loadProjects(true);
      });
    } catch (err) {
      if (window.console && console.warn) console.warn('[rollback] 事件监听失败:', err);
    }
  }

  function appendLog(line) {
    var pre = $('rollback-log');
    if (!pre) return;
    logLines.push(String(line));
    if (logLines.length > LOG_MAX_LINES) logLines = logLines.slice(-LOG_MAX_LINES);
    pre.textContent = logLines.join('\n');
    pre.scrollTop = pre.scrollHeight;
    var counter = $('rollback-log-count');
    if (counter) counter.textContent = logLines.length + ' 行';
  }

  function clearLog() {
    logLines = [];
    var pre = $('rollback-log');
    if (pre) pre.textContent = '';
    var counter = $('rollback-log-count');
    if (counter) counter.textContent = '';
  }

  // ===== 服务器选择 =====

  function loadServers() {
    return window.AppBus.invoke('get_config')
      .then(function (cfg) {
        var servers = (cfg && cfg.servers) ? cfg.servers : [];
        var sel = $('rollback-server-select');
        if (!sel) return [];
        var prev = sel.value;
        sel.textContent = '';
        if (servers.length === 0) {
          var opt = document.createElement('option');
          opt.value = '';
          opt.textContent = '(未配置服务器)';
          sel.appendChild(opt);
          return [];
        }
        servers.forEach(function (s) {
          var o = document.createElement('option');
          o.value = s.id;
          o.textContent = s.name || s.host;
          sel.appendChild(o);
        });
        // 保留原选择(仍存在时)
        if (prev && servers.some(function (s) { return s.id === prev; })) sel.value = prev;
        return servers;
      })
      .catch(function (err) {
        showError('读取服务器配置失败:' + errText(err));
        return [];
      });
  }

  /** 当前选中的服务器对象(id/name/remote_dir) */
  function currentServer() {
    var sel = $('rollback-server-select');
    if (!sel || !sel.value) return null;
    return { id: sel.value };
  }

  /** 扫描起点输入值(空 = 交给后端用服务器部署目录) */
  function scanRootValue() {
    var input = $('rollback-scan-root');
    return (input && input.value.trim()) ? input.value.trim() : null;
  }

  // ===== 项目列表 =====

  function loadProjects(keepSelection) {
    var server = currentServer();
    if (!server) {
      renderProjects([], keepSelection);
      return Promise.resolve();
    }
    showError('');
    setBusy(true);
    renderProjectsLoading();
    return window.AppBus.invoke('rollback_scan_projects', {
      serverId: server.id,
      scanRoot: scanRootValue() || undefined
    })
      .then(function (list) {
        currentProjects = Array.isArray(list) ? list : [];
        renderProjects(currentProjects, keepSelection);
      })
      .catch(function (err) {
        currentProjects = [];
        renderProjects([], false);
        showError('扫描项目失败:' + errText(err));
      })
      .then(function () { setBusy(false); });
  }

  function renderProjectsLoading() {
    var box = $('rollback-projects');
    if (!box) return;
    box.textContent = '';
    box.appendChild(el('div', 'rollback-empty', '正在扫描服务器项目…'));
  }

  function renderProjects(projects, keepSelection) {
    var box = $('rollback-projects');
    if (!box) return;
    box.textContent = '';
    if (!projects || projects.length === 0) {
      box.appendChild(el('div', 'rollback-empty',
        '未扫描到项目。项目 = 含 compose 文件的目录(排除 releases/)。' +
        '可修改扫描起点(如 /home)后重新扫描。'));
      if (!keepSelection) renderDetailEmpty();
      return;
    }
    projects.forEach(function (p) {
      var item = el('div', 'rollback-item');
      if (selectedDir === p.dir) item.classList.add('active');
      var head = el('div', 'rollback-item-head');
      head.appendChild(el('span', 'rollback-item-name mono', p.dir));
      if (p.runningContainers > 0) {
        head.appendChild(el('span', 'badge ok rollback-item-badge', '运行中 ' + p.runningContainers));
      }
      item.appendChild(head);

      var meta = el('div', 'rollback-item-meta');
      meta.appendChild(el('span', 'rollback-item-meta-text',
        '归档 ' + (p.releaseCount || 0) + ' 个' +
        (p.latestRelease ? '(最近 ' + p.latestRelease + ')' : '')));
      if (p.appProject) {
        meta.appendChild(el('span', 'cleanup-project-badge', '已配置:' + p.appProject));
      } else {
        meta.appendChild(el('span', 'cleanup-project-badge', '未在软件内配置'));
      }
      item.appendChild(meta);

      if (p.composeFile) {
        item.appendChild(el('div', 'rollback-item-compose mono', p.composeFile));
      }

      item.addEventListener('click', function () {
        selectProject(p.dir);
      });
      box.appendChild(item);
    });

    // 选中态若已失效(目录消失),清空明细
    if (selectedDir && !projects.some(function (p) { return p.dir === selectedDir; })) {
      selectedDir = null;
      renderDetailEmpty();
    }
  }

  function selectProject(dir) {
    selectedDir = dir;
    renderProjects(currentProjects, true);
    loadDetail(dir, false);
  }

  function renderDetailEmpty(text) {
    detailCache = null;
    var box = $('rollback-detail');
    if (!box) return;
    box.textContent = '';
    box.appendChild(el('div', 'rollback-empty',
      text || '从左侧选择一个项目查看可回滚版本'));
  }

  // ===== 项目明细(归档 + 标签)=====

  function loadDetail(dir, silent) {
    var server = currentServer();
    if (!server || !dir) { renderDetailEmpty(); return Promise.resolve(); }
    var box = $('rollback-detail');
    if (box && !silent) {
      box.textContent = '';
      box.appendChild(el('div', 'rollback-empty', '正在读取归档与标签…'));
    }
    return window.AppBus.invoke('rollback_project_detail', {
      serverId: server.id,
      dir: dir
    })
      .then(function (detail) {
        detailCache = detail || {};
        renderDetail(detail || {});
      })
      .catch(function (err) {
        renderDetailEmpty('读取明细失败:' + errText(err));
      });
  }

  function renderDetail(detail) {
    var box = $('rollback-detail');
    if (!box) return;
    box.textContent = '';

    var head = el('div', 'rollback-detail-head');
    head.appendChild(el('div', 'rollback-detail-dir mono', detail.dir || ''));
    // 该目录是否匹配到应用内项目(单镜像回滚需要 projectId)
    var matched = currentProjects.filter(function (p) { return p.dir === detail.dir; })[0];
    var appProject = matched && matched.appProject ? matched.appProject : '';
    box.appendChild(head);

    if (!appProject) {
      box.appendChild(el('div', 'rollback-hint-inline',
        '该项目未在软件内配置(缺 projectId),仅支持整栈回滚;' +
        '标签回滚请先在「服务器管理」新增对应项目。'));
    }

    // ---- 发布归档 ----
    box.appendChild(el('div', 'rollback-section-title', '发布归档 RELEASES'));
    var releases = detail.releases || [];
    if (releases.length === 0) {
      box.appendChild(el('div', 'rollback-empty-inline',
        '该项目没有发布归档(从未经本应用整栈部署,或归档已被清理)'));
    } else {
      releases.forEach(function (r) {
        var row = el('div', 'rollback-release');
        var info = el('div', 'rollback-release-info');
        info.appendChild(el('span', 'rollback-release-ts mono', r.ts));
        var parts = [];
        if (r.packages && r.packages.length) parts.push(r.packages.length + ' 个镜像包');
        if (r.services && r.services.length) parts.push('服务:' + r.services.join('、'));
        if (r.hasComposeCopy) parts.push('含 compose 副本');
        info.appendChild(el('span', 'rollback-release-meta', parts.join(' · ') || '(空归档)'));
        row.appendChild(info);

        var btn = el('button', 'btn btn-sm btn-danger', '回滚到此归档');
        btn.type = 'button';
        btn.addEventListener('click', function () {
          planStackRollback(detail.dir, r.ts, r.services || []);
        });
        row.appendChild(btn);

        // 删除该归档(第五批):两步确认(先变「确认删除?」,3 秒内再点才执行),
        // 与 03 页项目删除同一套交互,避免误删唯一可回滚的版本。
        var delBtn = el('button', 'btn btn-sm rollback-del-btn', '删除');
        delBtn.type = 'button';
        delBtn.title = '删除服务器上的这个发布归档(不可恢复)';
        delBtn.addEventListener('click', function () {
          armConfirm(delBtn, function () {
            deleteRelease(detail.dir, r.ts, delBtn);
          });
        });
        row.appendChild(delBtn);
        box.appendChild(row);
      });
    }

    // ---- 日期标签镜像 ----
    box.appendChild(el('div', 'rollback-section-title', '旧版本镜像 TAGS'));
    var repos = detail.repositories || [];
    if (repos.length === 0) {
      box.appendChild(el('div', 'rollback-empty-inline',
        '未在服务器上找到该项目的日期标签镜像(需用「日期标签部署」产生)'));
    } else {
      repos.forEach(function (repo) {
        var group = el('div', 'rollback-tag-group');
        group.appendChild(el('div', 'rollback-tag-repo mono', repo.repository));
        (repo.tags || []).forEach(function (t) {
          var row = el('div', 'rollback-tag-row');
          row.appendChild(el('span', 'rollback-tag-name mono', repo.repository + ':' + t.tag));
          row.appendChild(el('span', 'mono rollback-tag-created', t.created || ''));
          if (appProject) {
            var btn = el('button', 'btn btn-sm', '切回此版本');
            btn.type = 'button';
            btn.addEventListener('click', function () {
              planSingleRollback(appProject, repo.repository, t.tag);
            });
            row.appendChild(btn);
          }
          // 删除该日期标签镜像(第五批):两步确认;仍被容器引用时后端会拒绝
          var delTagBtn = el('button', 'btn btn-sm rollback-del-btn', '删除');
          delTagBtn.type = 'button';
          delTagBtn.title = '删除服务器上的这个历史镜像(不可恢复;仍被容器使用时会被拒绝)';
          var ref = repo.repository + ':' + t.tag;
          delTagBtn.addEventListener('click', function () {
            armConfirm(delTagBtn, function () {
              deleteTag(ref, delTagBtn);
            });
          });
          row.appendChild(delTagBtn);
          group.appendChild(row);
        });
        box.appendChild(group);
      });
    }
  }

  // ===== 删除归档 / 历史镜像(第五批;两步确认)=====

  /**
   * 两步确认(与 03 页项目删除同一套交互):首次点击把按钮变成
   * 「确认删除?」并加危险样式,3 秒内再点才真正执行;超时自动还原。
   * 用两步而非浏览器 confirm:全站约定不调用系统对话框。
   * 第六批:armed 期间额外挂 .is-armed —— 契约要求状态变化不能只靠颜色/文案,
   * 该类的呼吸描边提示「还需再点一次」。
   */
  function armConfirm(btn, onConfirm) {
    if (btn.__rbArmed) {
      if (btn.__rbTimer) {
        window.clearTimeout(btn.__rbTimer);
        btn.__rbTimer = null;
      }
      btn.__rbArmed = false;
      btn.textContent = btn.__rbText || '删除';
      btn.classList.remove('btn-danger');
      btn.classList.remove('is-armed');
      onConfirm();
      return;
    }
    btn.__rbArmed = true;
    btn.__rbText = btn.textContent;
    btn.textContent = '确认删除?';
    btn.classList.add('btn-danger');
    btn.classList.add('is-armed');
    btn.title = '再次点击确认删除(不可恢复)';
    btn.__rbTimer = window.setTimeout(function () {
      btn.__rbArmed = false;
      btn.textContent = btn.__rbText || '删除';
      btn.classList.remove('btn-danger');
      btn.classList.remove('is-armed');
      btn.__rbTimer = null;
    }, 3000);
  }

  /** 删除发布归档:后端校验路径前缀与存在性 → rm -rf → 刷新明细与项目列表 */
  function deleteRelease(dir, ts, btn) {
    var server = currentServer();
    if (!server) return;
    if (btn) { btn.disabled = true; btn.textContent = '删除中…'; }
    window.AppBus.invoke('rollback_delete_release', {
      serverId: server.id,
      dir: dir,
      releaseTs: ts
    })
      .then(function () {
        window.toast('已删除发布归档 ' + ts, 'ok');
        // 刷新明细(归档数变化)与项目列表(归档计数)
        loadDetail(dir, false);
        loadProjects(true);
      })
      .catch(function (err) {
        window.toast('删除归档失败:' + errText(err), 'fail');
      })
      .then(function () {
        // 明细刷新会重建整块,此处的按钮多半已被替换;仍兜底恢复可点
        if (btn) { btn.disabled = false; btn.textContent = '删除'; }
      });
  }

  /** 删除历史日期标签镜像:后端 inspect 校验存在 → docker rmi → 刷新明细 */
  function deleteTag(reference, btn) {
    var server = currentServer();
    if (!server) return;
    if (btn) { btn.disabled = true; btn.textContent = '删除中…'; }
    window.AppBus.invoke('rollback_delete_tag', {
      serverId: server.id,
      reference: reference
    })
      .then(function () {
        window.toast('已删除历史镜像 ' + reference, 'ok');
        if (selectedDir) loadDetail(selectedDir, true);
      })
      .catch(function (err) {
        // 常见失败:镜像仍被容器引用(docker rmi 拒绝)—— 原文已足够可读
        window.toast('删除镜像失败:' + errText(err), 'fail');
      })
      .then(function () {
        if (btn) { btn.disabled = false; btn.textContent = '删除'; }
      });
  }

  // ===== 回滚计划与确认 =====

  /**
   * 整栈回滚计划:列出将执行的动作,用户确认后调用后端。
   */
  function planStackRollback(dir, ts, services) {
    pending = {
      kind: 'stack',
      dir: dir,
      ts: ts,
      run: function () {
        var server = currentServer();
        if (!server) return Promise.resolve();
        clearLog();
        setBusy(true);
        appendLog('开始整栈回滚:' + dir + ' → ' + ts);
        return window.AppBus.invoke('rollback_execute_stack_at', {
          serverId: server.id,
          dir: dir,
          releaseTs: ts
        }).catch(function (err) {
          setBusy(false);
          appendLog('✘ ' + errText(err));
          throw err;
        });
      }
    };
    renderConfirm(
      '整栈回滚确认',
      window.confirmBlock({
        title: '确认整栈回滚到归档 ' + ts + '?',
        facts: [
          ['项目目录', dir],
          ['目标归档', ts],
          ['包含服务', services.length ? services.join('、') : '(归档无清单,按包内镜像标签恢复)'],
          ['执行步骤', '重载归档内镜像 → 恢复 compose 副本 → docker compose up -d']
        ],
        risk: '该操作会改变服务器上正在运行的服务版本,请确认目标归档正确。'
      })
    );
  }

  /**
   * 单镜像回滚计划:把 repository:dateTag 重新指到 compose 引用的标签。
   * 目标标签名默认 `<repository>:latest`(与部署页原引用常见形式一致),
   * 用户可在确认面板修改。
   */
  function planSingleRollback(appProjectName, repository, dateTag) {
    // 需要应用内 projectId:按名称在当前配置里查找
    window.AppBus.invoke('get_config').then(function (cfg) {
      var projects = (cfg && cfg.projects) ? cfg.projects : [];
      var prj = projects.filter(function (p) { return p.name === appProjectName; })[0];
      if (!prj) {
        showError('未找到项目「' + appProjectName + '」的配置,无法执行标签回滚');
        return;
      }
      var defaultTarget = repository + ':latest';
      pending = {
        kind: 'single',
        run: function (targetRef) {
          var server = currentServer();
          if (!server) return Promise.resolve();
          clearLog();
          setBusy(true);
          appendLog('开始镜像回滚:' + repository + ':' + dateTag + ' → ' + targetRef);
          return window.AppBus.invoke('rollback_execute_single', {
            serverId: server.id,
            projectId: prj.id,
            repository: repository,
            dateTag: dateTag,
            targetRef: targetRef
          }).catch(function (err) {
            setBusy(false);
            appendLog('✘ ' + errText(err));
            throw err;
          });
        }
      };
      renderConfirm(
        '镜像回滚确认',
        window.confirmBlock({
          title: '确认回滚 ' + repository + ':' + dateTag + '?',
          facts: [
            ['项目', appProjectName],
            ['历史版本', repository + ':' + dateTag],
            ['执行步骤', '把历史版本重新指向 compose 使用的标签,然后 compose up -d 重建容器'],
            ['目标标签', '请在下方确认(默认 latest)']
          ],
          risk: '重建期间容器将短暂重启。'
        }),
        { targetInput: defaultTarget }
      );
    }).catch(function (err) {
      showError('读取配置失败:' + errText(err));
    });
  }

  /** 渲染确认面板(替换明细区内容;可带目标标签输入框) */
  function renderConfirm(title, bodyNode, opts) {
    var box = $('rollback-detail');
    if (!box) return;
    box.textContent = '';
    box.appendChild(el('div', 'rollback-section-title', title));
    box.appendChild(typeof bodyNode === 'string'
      ? el('pre', 'rollback-confirm-text', bodyNode) : bodyNode);

    var targetInput = null;
    if (opts && opts.targetInput !== undefined) {
      var inputWrap = el('div', 'rollback-confirm-input');
      inputWrap.appendChild(el('span', 'rollback-release-meta', '目标标签:'));
      targetInput = document.createElement('input');
      targetInput.className = 'form-input';
      targetInput.type = 'text';
      targetInput.value = opts.targetInput;
      targetInput.id = 'rollback-target-input';
      inputWrap.appendChild(targetInput);
      box.appendChild(inputWrap);
    }

    var actions = el('div', 'modal-actions');
    var ok = el('button', 'btn btn-danger', '确认执行回滚');
    ok.type = 'button';
    var cancel = el('button', 'btn', '取消');
    cancel.type = 'button';
    actions.appendChild(cancel);
    actions.appendChild(ok);
    box.appendChild(actions);

    cancel.addEventListener('click', function () {
      pending = null;
      if (selectedDir) loadDetail(selectedDir, false);
      else renderDetailEmpty();
    });
    ok.addEventListener('click', function () {
      var plan = pending;
      if (!plan) return;
      var target = targetInput ? targetInput.value.trim() : null;
      if (plan.kind === 'single' && !target) {
        showError('目标标签不能为空');
        return;
      }
      pending = null;
      plan.run(target).catch(function () { /* 错误已记入日志 */ });
    });
  }

  // ===== 初始化与页面进出 =====

  function onEnter() {
    bindLogEvents();
    loadServers().then(function () {
      loadProjects(false);
    });
  }

  function bindEvents() {
    var refresh = $('rollback-refresh-btn');
    if (refresh) refresh.addEventListener('click', function () { loadProjects(true); });
    var rescan = $('rollback-rescan-btn');
    if (rescan) rescan.addEventListener('click', function () { loadProjects(false); });
    var sel = $('rollback-server-select');
    if (sel) {
      sel.addEventListener('change', function () {
        selectedDir = null;
        renderDetailEmpty();
        loadProjects(false);
      });
    }
    // 扫描起点输入框回车即扫描
    var root = $('rollback-scan-root');
    if (root) {
      root.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') loadProjects(false);
      });
    }

    window.addEventListener('pagechange', function (e) {
      if (!e || !e.detail) return;
      if (e.detail.page === 'rollback') onEnter();
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', bindEvents);
  } else {
    bindEvents();
  }
})();
