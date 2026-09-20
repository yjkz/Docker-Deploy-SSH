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
 *   - rollback_set_release_notes({ req: { serverId, passwordPlain?, dir, ts, title, body } })
 *     -> { updatedAt }(第七批:归档版本说明,存于归档目录内 release-notes.json)
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
  /** 明细加载会话序号:快速连点项目时丢弃过期响应(防旧数据覆盖新数据) */
  var detailSeq = 0;
  /** 版本详情模态当前编辑的归档 { dir, ts } */
  var editingRelease = null;
  /** 两版本对比勾选(第二十一批):最多两项,先进先出;切项目/重载明细时按现存归档过滤 */
  var diffSel = [];

  function $(id) { return document.getElementById(id); }

  function el(tag, cls, text) {
    var node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function showError(msg) {
    var box = $('rollback-error');
    if (!box) return;
    box.textContent = msg || '';
    box.classList.toggle('hidden', !msg);
  }

  // 刷新/重扫是一对组按钮(同时禁用 + 「扫描中…」文案变化),同 config-io 的理由
  // 不接 setBtnBusy 的步进条(两条同跑无意义);busy 期间文案反馈告知已生效。
  function setBusy(v) {
    busy = v;
    var btn = $('rollback-refresh-btn');
    if (btn) btn.disabled = v;
    var rescan = $('rollback-rescan-btn');
    if (rescan) {
      rescan.disabled = v;
      rescan.textContent = v ? '扫描中…' : '重新扫描';
    }
  }

  // ===== 日志面板 =====

  /**
   * 置 04 页「部署中」标志(R3;第二十九批)。
   *
   * 为什么需要:04 页「取消部署」按钮判 `st.deploying`,托盘「停止当前部署」
   * 另有独立判据 —— 06 页发起回滚时两者都不成立,用户无法中止(只能等
   * `docker load` 逐个跑完)。标志经 DeployKit 拿到的是**同一份** st(非副本),
   * 置位后 04 页 refreshControls 即让取消按钮可用。
   */
  function beginRemoteOpFlag() {
    try {
      var K = window.DeployKit;
      if (K && K.st) K.st.deploying = true;
      if (K && typeof K.refreshControls === 'function') K.refreshControls();
    } catch (e) {
      // 04 页模块未加载(理论上不会):退化为不可取消,不阻断回滚本身
      if (window.console && console.warn) console.warn('[rollback] 置取消标志失败:', e);
    }
  }

  /** 清 04 页「部署中」标志(收尾;与 beginRemoteOpFlag 成对) */
  function endRemoteOpFlag() {
    try {
      var K = window.DeployKit;
      if (K && K.st) K.st.deploying = false;
      if (K && typeof K.refreshControls === 'function') K.refreshControls();
    } catch (e) { /* 不影响收尾 */ }
  }

  function bindLogEvents() {
    if (logBound) return;
    logBound = true;
    // AppBus.on 返回 Promise:失败时复位 logBound 以便下次重试(否则永失监听);
    // 逐个 .catch 拦 Promise rejection(第二十批 P2-7:改走 AppBus.on ——
    // 与其它 7 个模块同口径,带 __TAURI__ 缺失防护;此前本模块是全站唯一
    // 裸用 __TAURI__.event.listen 的例外,违反 wiki/03「前端一律经 AppBus」纪律)
    var onListenFail = function (err) {
      logBound = false;
      if (window.console && console.warn) console.warn('[rollback] 事件监听失败:', err);
    };
    window.AppBus.on('deploy-log', function (e) {
      appendLog(typeof e.payload === 'string' ? e.payload : String(e.payload || ''));
    }).catch(onListenFail);
    window.AppBus.on('deploy-done', function (e) {
      var p = e.payload || {};
      // 只处理本页发起的回滚(busy===true):04 页部署/批量完成也发 deploy-done,
      // 不应触发 06 页整页重扫或写日志(来源过滤)
      if (!busy) return;
      window.ddRemoteOp = null; // 释放跨页互斥锁(本页回滚已收尾)
      endRemoteOpFlag();        // R3:复位 04 页「部署中」(取消入口随之禁用)
      var msg = window.errStripCode ? window.errStripCode(p.message) : (p.message || '');
      appendLog(p.success ? ('✔ ' + (msg || '回滚完成')) : ('✘ ' + (msg || '回滚失败')));
      setBusy(false);
      // 取消/失败时归档与标签未变化,无需重扫;仅成功才刷新明细与项目列表
      if (p.success) {
        if (selectedDir) loadDetail(selectedDir, true);
        loadProjects(true);
      }
    }).catch(onListenFail);
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
        // B3:按归属标签(首标签)分组成 optgroup(与部署页/03 页同一口径:
        // 未分组内联在其首次出现处,组顺序 = 首次出现顺序)
        window.appendGroupedOptions(sel, window.serverOptionsFor(servers));
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
      // 无服务器 ≠ 扫描无结果:给明确文案(与 05 页同口径),避免误读为「项目为空」
      renderProjectsNoServer();
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

  /** 未选择服务器时的左列空态(与「扫描无结果」区分的明确文案) */
  function renderProjectsNoServer() {
    var box = $('rollback-projects');
    if (!box) return;
    box.textContent = '';
    box.appendChild(el('div', 'rollback-empty', '请先在上方选择服务器'));
    renderDetailEmpty();
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
        // v6.1.3 修复:原手写类 'badge ok ...' 里的 ok 不存在(正确类名 badge-ok)
        // → 徽章零配色。改用 fillBadge 生成后补自定义类(与 338 行「已备注」同模式;
        // fillBadge 会整体重写 className,自定义类只能后加)
        var runBadge = window.fillBadge(el('span'), 'ok', '运行中 ' + p.runningContainers);
        runBadge.classList.add('rollback-item-badge');
        head.appendChild(runBadge);
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
      diffSel = [];
      renderDetailEmpty();
    }
  }

  function selectProject(dir) {
    selectedDir = dir;
    diffSel = []; // 切项目清对比勾选(跨项目的归档不可比)
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
    var seq = ++detailSeq; // 会话守卫:只有最新一次请求允许落渲染
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
        if (seq !== detailSeq) return;
        detailCache = detail || {};
        renderDetail(detail || {});
      })
      .catch(function (err) {
        if (seq !== detailSeq) return;
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
    var relTitle = el('div', 'rollback-section-title', '发布归档 RELEASES');
    // 勾选恰两个时出现「对比选中版本」(纯前端,数据都在本 detail 里)
    diffSel = diffSel.filter(function (ts) {
      return (detail.releases || []).some(function (x) { return x.ts === ts; });
    });
    if (diffSel.length === 2) {
      var diffBtn = el('button', 'btn btn-sm rollback-diff-btn', '对比选中版本');
      diffBtn.type = 'button';
      diffBtn.addEventListener('click', function () {
        openDiffModal(detail);
      });
      relTitle.appendChild(diffBtn);
    }
    box.appendChild(relTitle);
    var releases = detail.releases || [];
    if (releases.length === 0) {
      box.appendChild(el('div', 'rollback-empty-inline',
        '该项目没有发布归档(从未经本应用整栈部署,或归档已被清理)'));
    } else {
      releases.forEach(function (r) {
        var row = el('div', 'rollback-release');
        row.title = '点击查看版本详情与说明';
        row.addEventListener('click', function () {
          openReleaseDetail(r);
        });
        // 两版本对比(第二十一批):行首勾选框,勾满两个出现「对比选中版本」
        var diffCb = document.createElement('input');
        diffCb.type = 'checkbox';
        diffCb.className = 'rollback-diff-cb';
        diffCb.setAttribute('aria-label', '选择用于对比的版本 ' + r.ts);
        diffCb.checked = diffSel.indexOf(r.ts) !== -1;
        diffCb.addEventListener('click', function (e) {
          e.stopPropagation(); // 不触发行「查看详情」
        });
        diffCb.addEventListener('change', function () {
          if (diffCb.checked) {
            diffSel.push(r.ts);
            if (diffSel.length > 2) diffSel.shift(); // 最多两个,先进先出
          } else {
            var idx = diffSel.indexOf(r.ts);
            if (idx !== -1) diffSel.splice(idx, 1);
          }
          renderDetail(detailCache); // 重渲染以刷新对比按钮与勾选态
        });
        row.appendChild(diffCb);
        var info = el('div', 'rollback-release-info');
        info.appendChild(el('span', 'rollback-release-ts mono', r.ts));
        // 版本标题(第七批):设置了说明的归档在 ts 旁展示
        if (r.noteTitle) {
          info.appendChild(el('span', 'rollback-release-title', r.noteTitle));
        }
        var parts = [];
        if (r.packages && r.packages.length) parts.push(r.packages.length + ' 个镜像包');
        if (r.services && r.services.length) parts.push('服务:' + r.services.join('、'));
        if (r.hasComposeCopy) parts.push('含 compose 副本');
        info.appendChild(el('span', 'rollback-release-meta', parts.join(' · ') || '(空归档)'));
        // 「已备注」徽章:该归档有版本说明(标题或正文)
        if (r.noteTitle || r.noteBody) {
          var noteBadge = el('span', 'rollback-release-note-badge');
          window.fillBadge(noteBadge, 'info', '已备注');
          info.appendChild(noteBadge);
        }
        row.appendChild(info);

        var btn = el('button', 'btn btn-sm btn-danger', '回滚到此归档');
        btn.type = 'button';
        btn.addEventListener('click', function (e) {
          e.stopPropagation(); // 行点击是「查看详情」,按钮动作不触发
          planStackRollback(detail.dir, r.ts, r.services || []);
        });
        row.appendChild(btn);

        // 删除该归档(第五批):两步确认(先变「确认删除?」,3 秒内再点才执行),
        // 与 03 页项目删除同一套交互,避免误删唯一可回滚的版本。
        var delBtn = el('button', 'btn btn-sm rollback-del-btn', '删除');
        delBtn.type = 'button';
        delBtn.title = '删除服务器上的这个发布归档(不可恢复)';
        delBtn.addEventListener('click', function (e) {
          e.stopPropagation();
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

  // ===== 版本详情模态(第七批;类 GitHub Release 的标题 + 更新说明)=====
  //
  // 点击归档行打开;说明持久化在归档目录内 release-notes.json(后端
  // rollback_set_release_notes 原子写),标题与正文均留空保存 = 清除。

  /** 打开版本详情模态:元信息 + 镜像清单(manifest)+ 可编辑的标题与说明 */
  function openReleaseDetail(rel) {
    var overlay = $('release-detail-modal');
    var body = $('release-detail-modal-body');
    if (!overlay || !body) return;
    // dir 必须是**项目目录**(rollback_set_release_notes 会自行拼 releases/<ts>;
    // 第十一批修复:此前误用详情行自带的归档完整路径 rel.dir,拼接后变成
    // <项目>/releases/<ts>/releases/<ts>,保存恒报 No such file or directory。
    // selectedDir 即本次明细请求(rollback_project_detail)的项目目录)
    editingRelease = { dir: selectedDir, ts: rel.ts };
    body.textContent = '';

    var errBox = window.formErrorBox('rn-error');
    body.appendChild(errBox);

    // 元信息行(复用归档行的 meta 风格)
    var metaParts = ['归档 ' + rel.ts];
    if (rel.packages && rel.packages.length) metaParts.push(rel.packages.length + ' 个镜像包');
    if (rel.services && rel.services.length) metaParts.push('服务:' + rel.services.join('、'));
    metaParts.push(rel.hasComposeCopy ? '含 compose 副本' : '无 compose 副本');
    body.appendChild(el('div', 'rollback-release-meta', metaParts.join(' · ')));

    // 镜像清单(manifest 逐服务条目;旧归档无 manifest 时优雅降级)
    body.appendChild(window.formGroupTitle('镜像清单', 'IMAGES'));
    if (rel.manifestImages && rel.manifestImages.length) {
      rel.manifestImages.forEach(function (img) {
        // 镜像 ID 短哈希(v6.3.2):有采集时附在条目尾部,支持「两次部署同 tag
        // 但内容不同」的人工核对(两版本对比按 ID 判变化的同源数据)
        var idHint = '';
        if (img.id) {
          var body8 = (img.id.indexOf('sha256:') === 0) ? img.id.slice(7) : img.id;
          idHint = ' · ' + body8.slice(0, 8);
        }
        body.appendChild(el('div', 'rollback-release-meta mono',
          img.service + ' → ' + img.tag + idHint + (img.file ? ('(' + img.file + ')') : '(未留档,仅跳过传输)')));
      });
    } else {
      body.appendChild(el('div', 'rollback-hint-inline',
        rel.hasManifest ? 'manifest 存在但未记录镜像条目。' : '该归档无 manifest(旧版本发布),镜像以包内标签恢复。'));
    }

    // 版本标题 + 版本说明(可编辑;保存到归档目录)
    var titleRow = el('div', 'form-row');
    titleRow.appendChild(window.formLabel('版本标题', 'TITLE', false, 'rn-title'));
    var titleInput = el('input', 'form-input');
    titleInput.id = 'rn-title';
    titleInput.type = 'text';
    titleInput.maxLength = 120;
    titleInput.placeholder = '如:v1.2.0 修复登录超时';
    titleInput.value = rel.noteTitle || '';
    titleRow.appendChild(titleInput);
    body.appendChild(titleRow);

    var bodyRow = el('div', 'form-row');
    bodyRow.appendChild(window.formLabel('版本说明', 'NOTES', false, 'rn-body'));
    var ta = document.createElement('textarea');
    ta.id = 'rn-body';
    ta.className = 'form-textarea';
    ta.rows = 6;
    ta.placeholder = '此次版本更新了什么?(标题与说明都留空并保存 = 清除说明)';
    ta.value = rel.noteBody || '';
    bodyRow.appendChild(ta);
    body.appendChild(bodyRow);

    var result = el('div', 'cio-result');
    result.id = 'rn-result';
    if (rel.noteUpdatedAt) result.textContent = '上次保存:' + rel.noteUpdatedAt;
    body.appendChild(result);

    var actions = el('div', 'modal-actions');
    var closeBtn = el('button', 'btn', '关闭');
    closeBtn.type = 'button';
    closeBtn.addEventListener('click', closeReleaseDetail);
    var saveBtn = el('button', 'btn btn-primary', '保存说明');
    saveBtn.id = 'rn-save';
    saveBtn.type = 'button';
    saveBtn.addEventListener('click', function () {
      saveReleaseNotes(saveBtn);
    });
    actions.appendChild(closeBtn);
    actions.appendChild(saveBtn);
    body.appendChild(actions);

    overlay.classList.remove('hidden');
    window.modalFocusOpen(overlay);
  }

  /** 保存版本说明:标题与正文均空 → 后端删除 notes 文件(= 清除) */
  function saveReleaseNotes(saveBtn) {
    var server = currentServer();
    var editing = editingRelease;
    if (!server || !editing) return;
    var title = ($('rn-title') || {}).value || '';
    var noteBody = ($('rn-body') || {}).value || '';
    window.setBtnBusy(saveBtn, true, '保存中…');
    window.AppBus.invoke('rollback_set_release_notes', {
      req: {
        serverId: server.id,
        dir: editing.dir,
        ts: editing.ts,
        title: title,
        body: noteBody
      }
    })
      .then(function (saved) {
        var cleared = !title.trim() && !noteBody.trim();
        window.toast(cleared ? '已清除版本说明' : '已保存版本说明', 'ok');
        // 同步缓存并就地刷新明细区(模态保持打开,可继续编辑)
        applyNotesToCache(editing.ts, cleared ? null : {
          title: title.trim(),
          body: noteBody.trim(),
          at: (saved && saved.updatedAt) || ''
        });
        if (selectedDir && detailCache) renderDetail(detailCache);
        var result = $('rn-result');
        if (result) {
          result.textContent = cleared
            ? '已清除版本说明'
            : '已保存 · ' + ((saved && saved.updatedAt) || '');
        }
      })
      .catch(function (err) {
        window.formFailLoud('rn-error', '保存版本说明失败:' + errText(err));
      })
      .then(function () {
        window.setBtnBusy(saveBtn, false, '保存说明');
      });
  }

  /** 把保存结果同步进 detailCache(渲染与徽章的单一数据源) */
  function applyNotesToCache(ts, notes) {
    if (!detailCache || !Array.isArray(detailCache.releases)) return;
    detailCache.releases.forEach(function (r) {
      if (r.ts !== ts) return;
      r.noteTitle = notes ? notes.title : null;
      r.noteBody = notes ? notes.body : null;
      r.noteUpdatedAt = notes ? notes.at : null;
    });
  }

  function closeReleaseDetail() {
    var overlay = $('release-detail-modal');
    if (overlay) overlay.classList.add('hidden');
    window.modalFocusClose(overlay);
    var body = $('release-detail-modal-body');
    if (body) body.textContent = '';
    editingRelease = null;
  }

  // ===== 两版本对比(第二十一批)=====

  /**
   * 打开对比模态:把 detail.releases 中勾选的两个归档并排对比。
   * 数据源:RollbackProjectDetail.releases 每项已含 manifestImages(逐服务
   * image tag)与服务清单——纯前端计算,零额外往返。
   * 对比维度:服务新增/移除、同名服务的镜像 tag 变化;附版本说明与归档包数。
   */
  function openDiffModal(detail) {
    var overlay = $('release-diff-modal');
    var body = $('release-diff-modal-body');
    if (!overlay || !body) return;
    detailCache = detail || null;
    var releases = (detail && Array.isArray(detail.releases)) ? detail.releases : [];
    var older = null;
    var newer = null;
    // diffSel 依勾选顺序 push;按 ts 排序让「旧 → 新」阅读方向稳定(ts 可排序)
    var picked = releases.filter(function (r) { return diffSel.indexOf(r.ts) !== -1; });
    picked.sort(function (a, b) { return String(a.ts).localeCompare(String(b.ts)); });
    older = picked[0] || null;
    newer = picked[1] || null;

    body.textContent = '';
    if (!older || !newer) {
      body.appendChild(el('div', 'rollback-empty', '请勾选两个版本后再对比'));
      overlay.classList.remove('hidden');
      window.modalFocusOpen(overlay);
      return;
    }

    // 头部:两侧 ts + 标题/备注
    var head = el('div', 'diff-head');
    [older, newer].forEach(function (r) {
      var col = el('div', 'diff-head-col');
      col.appendChild(el('div', 'diff-head-ts mono', r.ts));
      if (r.noteTitle) col.appendChild(el('div', 'diff-head-note', r.noteTitle));
      var meta = (r.packages && r.packages.length ? r.packages.length + ' 个镜像包' : '') +
        (r.hasComposeCopy ? ' · 含 compose' : '');
      if (meta) col.appendChild(el('div', 'diff-head-meta', meta));
      head.appendChild(col);
    });
    body.appendChild(head);

    // 逐服务镜像对比(manifestImages:service + tag + id)。
    // **变化判定优先按镜像 ID(内容寻址)**:同名 tag 重新构建后 ID 不同 ——
    // 只比 tag 名会把「镜像换了」误报为「不变」(用户真机反馈)。任一旧/新项
    // 缺 ID(旧归档)时回退按 tag 比较(v6.3.2 前的行为),并在表下注明。
    var mapOf = function (r) {
      var m = {};
      (r.manifestImages || []).forEach(function (img) {
        m[img.service] = { tag: img.tag || '', id: img.id || '' };
      });
      return m;
    };
    var mOld = mapOf(older);
    var mNew = mapOf(newer);
    var names = {};
    Object.keys(mOld).forEach(function (k) { names[k] = true; });
    Object.keys(mNew).forEach(function (k) { names[k] = true; });
    var svcNames = Object.keys(names).sort();
    // 短哈希展示(sha256: 前缀后取 8 位;空则空串)
    var shortId = function (id) {
      var s = String(id || '');
      if (!s) return '';
      var body = s.indexOf('sha256:') === 0 ? s.slice(7) : s;
      return body.slice(0, 8);
    };
    var anyFallback = false;

    var table = document.createElement('table');
    table.className = 'data-table diff-table';
    var thead = document.createElement('thead');
    var htr = document.createElement('tr');
    ['服务 SERVICE', older.ts, newer.ts, '变化'].forEach(function (t) {
      var th = document.createElement('th');
      th.textContent = t;
      htr.appendChild(th);
    });
    thead.appendChild(htr);
    table.appendChild(thead);
    var tbody = document.createElement('tbody');
    if (svcNames.length === 0) {
      var etr = document.createElement('tr');
      var etd = el('td', 'empty-cell', '两个归档都没有服务清单(旧版本发布无 manifest),仅可对比上面的元信息');
      etd.colSpan = 4;
      etr.appendChild(etd);
      tbody.appendChild(etr);
    }
    svcNames.forEach(function (name) {
      var hasOld = Object.prototype.hasOwnProperty.call(mOld, name);
      var hasNew = Object.prototype.hasOwnProperty.call(mNew, name);
      var o = hasOld ? mOld[name] : null;
      var n = hasNew ? mNew[name] : null;
      var tr = document.createElement('tr');
      tr.appendChild(el('td', 'mono', name));
      // 单元格:tag + 短哈希副行(ID 采集到才展示)
      var cellFor = function (v, present) {
        if (!present) return el('td', 'mono diff-cell diff-absent', '—');
        var td = el('td', 'mono diff-cell');
        td.appendChild(el('div', '', v.tag));
        var sid = shortId(v.id);
        if (sid) td.appendChild(el('div', 'diff-id-hint', sid));
        return td;
      };
      tr.appendChild(cellFor(o, hasOld));
      tr.appendChild(cellFor(n, hasNew));
      var change;
      if (!hasOld) {
        change = '新增服务';
      } else if (!hasNew) {
        change = '移除服务';
      } else if (o.id && n.id) {
        // 双侧都有 ID:按内容判(核心修复)
        change = (o.id === n.id) ? '不变' : '镜像变化';
      } else {
        // 任一侧缺 ID(旧归档/采集失败):回退按 tag,并注明
        anyFallback = true;
        change = (o.tag === n.tag) ? '不变' : '镜像变化';
      }
      var tdChg = document.createElement('td');
      var kind = (change === '不变') ? 'info' : (change === '移除服务' ? 'fail' : 'warn');
      tdChg.appendChild(window.fillBadge(el('span'), kind, change));
      tr.appendChild(tdChg);
      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
    body.appendChild(table);
    if (anyFallback) {
      body.appendChild(el('div', 'rollback-hint-inline',
        '部分服务缺少镜像 ID 记录(旧版本归档未采集):这些行仅按标签名比较,' +
        '同名标签重新构建可能显示「不变」;此后新部署的归档会记录 ID,对比更准确。'));
    }

    // 版本说明对比(两条各一段;空的不渲染)
    if (older.noteBody || newer.noteBody) {
      body.appendChild(el('div', 'rollback-section-title', '版本说明 NOTES'));
      [older, newer].forEach(function (r) {
        if (!r.noteBody) return;
        var block = el('div', 'diff-note');
        block.appendChild(el('div', 'diff-note-ts mono', r.ts));
        block.appendChild(el('div', 'diff-note-body', r.noteBody));
        body.appendChild(block);
      });
    }

    overlay.classList.remove('hidden');
    window.modalFocusOpen(overlay);
  }

  function closeDiffModal() {
    var overlay = $('release-diff-modal');
    if (overlay) overlay.classList.add('hidden');
    window.modalFocusClose(overlay);
    var body = $('release-diff-modal-body');
    if (body) body.textContent = '';
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
      // 预检上下文(供 renderConfirm 的「回滚预检」按钮调用)
      precheck: { dir: dir, ts: ts },
      run: function (allowPartial) {
        var server = currentServer();
        if (!server) return Promise.resolve();
        // 跨页互斥:04 页部署/批量进行中时不发起(共享锁,与 deploy.js 双向)
        if (window.ddRemoteOp) {
          window.toast('另一项远程操作进行中(部署/回滚),请等待完成', 'warn');
          return Promise.resolve();
        }
        window.ddRemoteOp = 'rollback';
        // R3(第二十九批):同步置 04 页「部署中」—— 04 页取消按钮与托盘停止
        // 都以此为条件;此前 06 页发起的回滚无法取消(只能硬等 docker load
        // 逐个跑完)。复位在 deploy-done 监听里(见下)。
        beginRemoteOpFlag();
        clearLog();
        setBusy(true);
        appendLog('开始整栈回滚:' + dir + ' → ' + ts);
        return window.AppBus.invoke('rollback_execute_stack_at', {
          serverId: server.id,
          dir: dir,
          releaseTs: ts,
          // 预检已把阻断项展示给用户 → 允许部分回滚(R1;与 04 页同口径)
          allowPartial: allowPartial === true
        }).catch(function (err) {
          window.ddRemoteOp = null;
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
        // 第二参数忽略(单镜像回滚无 manifest 可预检;签名与整栈对齐便于统一调用)
        run: function (targetRef) {
          var server = currentServer();
          if (!server) return Promise.resolve();
          // 跨页互斥:04 页部署/批量进行中时不发起(共享锁,与 deploy.js 双向)
          if (window.ddRemoteOp) {
            window.toast('另一项远程操作进行中(部署/回滚),请等待完成', 'warn');
            return Promise.resolve();
          }
          window.ddRemoteOp = 'rollback';
          beginRemoteOpFlag(); // R3:同整栈路径(见上)
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
            window.ddRemoteOp = null;
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

    // 回滚预检(R1;第二十九批;06 回滚中心路径):
    // 与 04 页同款交互(先预检 → 看结果 → 再确认),但这里**不强制**先预检 ——
    // 本页允许直接执行(单镜像回滚无 manifest 可查,不适用预检)。
    // 整栈回滚点「回滚预检」时,结果就地展示在本面板内,阻断项执行按钮改文案。
    var preWrap = null;
    var precheckBtn = null;
    if (pending && pending.precheck) {
      precheckBtn = el('button', 'btn', '回滚预检');
      precheckBtn.type = 'button';
      precheckBtn.title = '核对每个服务在该归档里的镜像来源(归档内有包 / 服务器上按镜像 ID 命中 / 回不去)';
      preWrap = el('div', 'rb-precheck hidden');
      preWrap.id = 'rb-precheck-box';
      box.appendChild(preWrap);
    }

    actions.appendChild(cancel);
    if (precheckBtn) actions.appendChild(precheckBtn);
    actions.appendChild(ok);
    box.appendChild(actions);

    if (precheckBtn) {
      precheckBtn.addEventListener('click', function () {
        runPagePrecheck(precheckBtn, ok, preWrap);
      });
    }

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
      // 预检已确认(阻断项或 .env 插值漂移已展示)→ 允许继续(v6.12.0 加漂移)
      var pr = plan.precheckResult;
      var allowPartial = !!(pr && (pr.hasBlocking ||
        (Array.isArray(pr.envDrift) && pr.envDrift.length)));
      pending = null;
      plan.run(target, allowPartial).catch(function () { /* 错误已记入日志 */ });
    });
  }

  /**
   * 06 页回滚预检(R1;第二十九批):按目录查(本页无 projectId),
   * 结果就地展示 —— 与 04 页同一后端命令、同一展示口径。
   */
  function runPagePrecheck(btn, okBtn, box) {
    var plan = pending;
    if (!plan || !plan.precheck) return;
    var server = currentServer();
    if (!server) { showError('请先选择服务器'); return; }
    window.setBtnBusy(btn, true, '预检中…');
    box.classList.remove('hidden'); // 结果区初始 hidden(与 04 页同款);此处揭示
    box.textContent = '';
    box.appendChild(el('div', 'rb-precheck-title', '回滚可用性预检中…'));
    if (okBtn) okBtn.disabled = true;
    window.AppBus.invoke('rollback_precheck', {
      serverId: server.id,
      dir: plan.precheck.dir,
      releaseTs: plan.precheck.ts
    })
      .then(function (res) {
        window.setBtnBusy(btn, false, '回滚预检');
        plan.precheckResult = res || null;
        renderPagePrecheckResult(res || {}, box, okBtn);
      })
      .catch(function (err) {
        window.setBtnBusy(btn, false, '回滚预检');
        box.textContent = '';
        box.appendChild(el('div', 'rb-precheck-title', '预检失败'));
        box.appendChild(el('div', 'rb-precheck-detail', errText(err) || '未知错误'));
        if (okBtn) okBtn.disabled = false; // 预检失败不阻断执行(用户自行判断)
      });
  }

  /** 渲染 06 页预检结果(与 04 页同口径:阻断项置顶 + 标签待指回 + 漂移) */
  function renderPagePrecheckResult(res, box, okBtn) {
    box.textContent = '';
    if (res.noManifest) {
      box.appendChild(el('div', 'rb-precheck-title',
        '该归档无发布清单(旧版本或未完成的部署),无法逐服务核对;将按目录内的镜像包恢复'));
      if (okBtn) okBtn.disabled = false;
      return;
    }
    var items = Array.isArray(res.items) ? res.items : [];
    box.appendChild(el('div', 'rb-precheck-title',
      '共 ' + items.length + ' 个服务:归档内有包 ' + (res.archived || 0) +
      ' · 服务器上按镜像 ID 命中 ' + (res.remoteById || 0) +
      (res.tagRestore ? ' · 标签待指回 ' + res.tagRestore : '') +
      (res.missing ? ' · 回不去 ' + res.missing : '') +
      (res.unknown ? ' · 无法核对 ' + res.unknown : '')));
    var rank = function (it) {
      if (it.blocking) return 0;
      return it.source === 'tagRestore' ? 1 : 2;
    };
    var sorted = items.slice().sort(function (a, b) { return rank(a) - rank(b); });
    sorted.forEach(function (it) {
      var row = el('div', 'rb-precheck-row' + (it.blocking ? ' is-blocking' : ''));
      var kind = it.blocking ? 'fail' : (it.source === 'tagRestore' ? 'warn' : 'ok');
      var label = it.blocking ? '回不去' : (it.source === 'tagRestore' ? '标签待指回' : '可用');
      row.appendChild(window.fillBadge(el('span'), kind, label));
      row.appendChild(el('span', 'rb-precheck-svc', String(it.service || '')));
      row.appendChild(el('span', 'rb-precheck-detail', String(it.detail || '')));
      box.appendChild(row);
    });
    // 插值漂移(v6.12.0):.env 不入归档,归档 compose 的 ${VAR} 会按当前 .env
    // 重新解析 —— 与归档记录不同者必须让用户看到(须确认后才执行)
    var drift = Array.isArray(res.envDrift) ? res.envDrift : [];
    if (drift.length) {
      box.appendChild(el('div', 'rb-precheck-title',
        '.env 插值漂移 ' + drift.length + ' 项:归档 compose 的镜像引用会按服务器当前 .env 解析'));
      drift.forEach(function (d) {
        var row = el('div', 'rb-precheck-row is-blocking');
        row.appendChild(window.fillBadge(el('span'), 'warn', '插值漂移'));
        row.appendChild(el('span', 'rb-precheck-svc', String(d.service || '')));
        row.appendChild(el('span', 'rb-precheck-detail',
          '归档记录 ' + String(d.expected || '') + ';按当前 .env 会解析成 ' + String(d.resolved || '')));
        box.appendChild(row);
      });
    }
    if (okBtn) {
      okBtn.disabled = false;
      if (res.hasBlocking || drift.length) {
        okBtn.textContent = '仍要回滚(部分)';
        okBtn.title = res.hasBlocking
          ? '上述「回不去」的服务将沿用服务器当前镜像,版本可能与归档不一致'
          : '存在 .env 插值漂移:这些服务将按当前 .env 解析出的引用启动';
      } else {
        okBtn.textContent = '确认执行回滚';
        okBtn.title = '';
      }
    }
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

    // 版本详情模态:关闭钮 / 遮罩 / Esc 三通道(与 notify/config-io 同款)
    var rdClose = $('release-detail-modal-close');
    if (rdClose) rdClose.addEventListener('click', closeReleaseDetail);
    var rdOverlay = $('release-detail-modal');
    if (rdOverlay) {
      rdOverlay.addEventListener('click', function (e) {
        if (e.target === rdOverlay) closeReleaseDetail();
      });
      // Esc 关闭(仅当自己是顶层模态;第二十批 P1 修复:全局仲裁
      // window.isTopModal 见 app.js,叠模态一次 Esc 只关一层)
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && window.isTopModal('release-detail-modal')) {
          closeReleaseDetail();
        }
      });
    }

    // 两版本对比模态(第二十一批):关闭钮 / 遮罩 / Esc 三通道
    var dfClose = $('release-diff-modal-close');
    if (dfClose) dfClose.addEventListener('click', closeDiffModal);
    var dfOverlay = $('release-diff-modal');
    if (dfOverlay) {
      dfOverlay.addEventListener('click', function (e) {
        if (e.target === dfOverlay) closeDiffModal();
      });
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && window.isTopModal('release-diff-modal')) {
          closeDiffModal();
        }
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
