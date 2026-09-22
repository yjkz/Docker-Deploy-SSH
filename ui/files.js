/* ============================================================
 * files.js — 文件管理(第三十三批):容器 rootfs / 数据卷 / 部署目录(只读)三源
 *
 * 普通 script 加载(在 app.js 之后),不依赖任何框架;对外只暴露 window.FilesKit
 * (单次赋值,键由 manage.js 消费 —— 见 verify/bridge-integrity.js)。
 *
 * 通道与边界(与后端 manage_files.rs 同口径):
 * - 容器:docker cp 通道(容器无任何二进制也能传),目录列举/改名/删除需要容器有
 *   shell;没有时列表返回 unsupported,本页降级为「按完整路径下载/上传」。
 * - 数据卷:后端经临时容器挂载(工具来自镜像,总是可用)。
 * - 部署目录:**只读**(下载/查看;上传/改名/删除按钮禁用并说明)。
 *
 * 传输是同步命令(等待完成),期间本页锁定并订阅 files-transfer-progress 显示进度;
 * 「取消传输」调用 manage_files_cancel(块级生效)。
 * ============================================================ */
(function () {
  'use strict';

  var $ = function (id) { return document.getElementById(id); };

  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined && text !== null) n.textContent = String(text);
    return n;
  }

  function fmtSize(n) {
    var v = Number(n) || 0;
    if (v < 1024) return v + ' B';
    if (v < 1024 * 1024) return (v / 1024).toFixed(1) + ' KB';
    if (v < 1024 * 1024 * 1024) return (v / 1024 / 1024).toFixed(1) + ' MB';
    return (v / 1024 / 1024 / 1024).toFixed(2) + ' GB';
  }

  var st = {
    open: false,
    view: 'list',          // list | edit | snapshot | distribute
    serverId: '',
    kind: 'container',     // container | volume | deployDir
    target: '',
    path: '',
    entries: [],
    filter: '',
    busy: false,
    note: '',
    unsupported: false,
    // 编辑视图
    editPath: '', editText: '', editBytes: 0,
    // 快照视图
    snapshot: null, showSecrets: false,
    // 分发视图
    dist: null,
    progress: null,        // { phase, done, total }
    opLabel: '',
    targets: { container: [], volume: [] },
    dirBase: '',
    err: ''
  };

  function invoke(cmd, args) { return window.AppBus.invoke(cmd, args); }

  function baseArgs() {
    return { serverId: st.serverId, kind: st.kind, target: st.target };
  }

  // ===== 模态骨架 =====

  function overlay() { return $('files-modal'); }

  function canClose() {
    if (st.busy) {
      window.toast('文件传输进行中,请先等待完成或点「取消传输」', 'warn');
      return false;
    }
    return true;
  }

  function close() {
    if (!canClose()) return;
    var ov = overlay();
    if (!ov) return;
    ov.classList.add('hidden');
    st.open = false;
    if (typeof window.modalFocusClose === 'function') window.modalFocusClose(ov);
  }

  function bindOnce() {
    var ov = overlay();
    if (!ov || ov.dataset.bound === '1') return;
    ov.dataset.bound = '1';
    var closeBtn = $('files-modal-close');
    if (closeBtn) closeBtn.addEventListener('click', close);
    ov.addEventListener('click', function (e) { if (e.target === ov) close(); });
    document.addEventListener('keydown', function (e) {
      if (e.key !== 'Escape') return;
      var ov2 = overlay();
      if (!ov2 || ov2.classList.contains('hidden')) return;
      if (window.isTopModal && !window.isTopModal('files-modal')) return;
      close();
    });
  }

  // ===== 打开入口 =====

  function openWith(opts) {
    st.open = true;
    st.serverId = String(opts.serverId || '');
    st.kind = opts.kind || 'container';
    st.target = String(opts.target || '');
    st.path = '';
    st.filter = '';
    st.entries = [];
    st.note = '';
    st.unsupported = false;
    st.err = '';
    st.progress = null;
    st.view = 'list';
    st.snapshot = null;
    var ov = overlay();
    if (!ov) return;
    ov.classList.remove('hidden');
    if (typeof window.modalFocusOpen === 'function') window.modalFocusOpen(ov);
    refreshTargets(function () {
      refreshList();
    });
  }

  /** 容器行「文件」入口 */
  function openFor(serverId, containerId, containerName) {
    openWith({ serverId: serverId, kind: 'container', target: containerId });
    void containerName;
  }

  /** 卷列表入口(05 页 卷 Tab) */
  function openForVolume(serverId, volumeName) {
    openWith({ serverId: serverId, kind: 'volume', target: volumeName });
  }

  /** 容器行「快照」入口 */
  function openSnapshot(serverId, containerId) {
    openWith({ serverId: serverId, kind: 'container', target: containerId });
    st.view = 'snapshot';
    st.snapshot = null;
    loadSnapshot();
  }

  // ===== 目标列表(容器 / 卷 / 部署目录默认值)=====

  function refreshTargets(done) {
    var jobs = [];
    jobs.push(invoke('manage_list_containers', { serverId: st.serverId }).then(function (list) {
      st.targets.container = Array.isArray(list) ? list : [];
    }).catch(function () { st.targets.container = []; }));
    jobs.push(invoke('manage_list_volumes', { serverId: st.serverId }).then(function (list) {
      st.targets.volume = Array.isArray(list) ? list : [];
    }).catch(function () { st.targets.volume = []; }));
    if (!st.dirBase) {
      jobs.push(invoke('get_config').then(function (cfg) {
        var s = (cfg && cfg.servers || []).filter(function (x) { return x.id === st.serverId; })[0];
        st.dirBase = s && s.remote_dir ? String(s.remote_dir) : '/';
      }).catch(function () { st.dirBase = '/'; }));
    }
    Promise.all(jobs).then(function () { if (done) done(); });
  }

  // ===== 列举 =====

  function refreshList() {
    if (!st.serverId) return;
    st.busy = true;
    st.err = '';
    render();
    invoke('manage_files_list', Object.assign(baseArgs(), { path: st.path }))
      .then(function (res) {
        st.busy = false;
        res = res || {};
        st.entries = Array.isArray(res.entries) ? res.entries : [];
        st.unsupported = !!res.unsupported;
        st.note = res.note || '';
        st.path = res.path || '';
        render();
      })
      .catch(function (e) {
        st.busy = false;
        st.err = window.errText(e) || '未知错误';
        render();
      });
  }

  function visibleEntries() {
    var f = st.filter.trim().toLowerCase();
    var list = st.entries.filter(function (e) {
      return !f || String(e.name).toLowerCase().indexOf(f) >= 0;
    });
    list.sort(function (a, b) {
      if (!!a.isDir !== !!b.isDir) return a.isDir ? -1 : 1;
      return String(a.name).localeCompare(String(b.name));
    });
    return list;
  }

  function joinPath(rel) {
    return st.path ? st.path + '/' + rel : rel;
  }

  function enterDir(name) {
    st.path = joinPath(name);
    st.filter = '';
    refreshList();
  }

  function goUp() {
    var parts = st.path.split('/').filter(Boolean);
    parts.pop();
    st.path = parts.join('/');
    st.filter = '';
    refreshList();
  }

  // ===== 传输(进度 / 取消)=====

  function subscribeProgress() {
    if (!window.AppBus || typeof window.AppBus.on !== 'function') return Promise.resolve(null);
    return window.AppBus.on('files-transfer-progress', function (ev) {
      var p = ev && ev.payload ? ev.payload : {};
      st.progress = { phase: String(p.phase || ''), done: Number(p.done) || 0, total: Number(p.total) || 0 };
      renderProgress();
    }).catch(function () { return null; });
  }

  function phaseLabel(phase) {
    if (phase === 'upload') return '上传中';
    if (phase === 'download') return '下载中';
    if (phase === 'backup') return '备份原文件中';
    return '处理中';
  }

  function runTransfer(label, fn) {
    if (st.busy) return;
    st.busy = true;
    st.opLabel = label;
    st.progress = null;
    st.err = '';
    render();
    subscribeProgress().then(function (un) {
      fn().then(function (res) {
        st.busy = false;
        st.progress = null;
        if (un) un();
        render();
        if (res) window.toast(res, 'ok');
      }).catch(function (e) {
        st.busy = false;
        st.progress = null;
        if (un) un();
        var msg = window.errText(e) || '未知错误';
        st.err = msg;
        render();
        window.toast(label + '失败:' + msg, 'warn');
      });
    });
  }

  // ===== 操作 =====

  function doUpload() {
    window.AppBus.pickPath({ directory: false, title: '选择要上传的文件' }).then(function (picked) {
      if (!picked) return;
      var src = String(picked);
      var backup = window.confirm('上传 ' + src.split(/[\\/]/).pop() + ' 到 ' + (st.path || '根目录') +
        '\n\n若目标已存在同名文件,将先备份到本机(配置目录 fm-backups,同文件保留 3 份)。\n继续?');
      if (!backup) return;
      runTransfer('上传', function () {
        return invoke('manage_files_upload', Object.assign(baseArgs(), {
          destDir: st.path, localPath: src, backup: true
        })).then(function (res) {
          refreshList();
          return res && res.backedUp ? '已上传;原文件已备份到 ' + res.backedUp : '已上传';
        });
      });
    }).catch(function () {});
  }

  function doDownload(relPath) {
    window.AppBus.pickPath({ directory: true, title: '选择保存到本机目录' }).then(function (picked) {
      if (!picked) return;
      runTransfer('下载', function () {
        return invoke('manage_files_download', Object.assign(baseArgs(), {
          path: relPath, localDir: String(picked)
        })).then(function (res) { return '已保存到 ' + res; });
      });
    }).catch(function () {});
  }

  function doEdit(relPath) {
    st.busy = true; st.err = ''; render();
    invoke('manage_files_read_text', Object.assign(baseArgs(), { path: relPath }))
      .then(function (res) {
        st.busy = false;
        res = res || {};
        if (res.truncated) {
          window.toast('文件超过 512 KB,请用「下载」后本地编辑再上传', 'warn');
          render();
          return;
        }
        if (res.binary) {
          window.toast('该文件不是文本(二进制或非 UTF-8),请用「下载」', 'warn');
          render();
          return;
        }
        st.view = 'edit';
        st.editPath = relPath;
        st.editText = String(res.text || '');
        st.editBytes = Number(res.bytes) || 0;
        render();
      })
      .catch(function (e) {
        st.busy = false;
        st.err = window.errText(e) || '未知错误';
        render();
      });
  }

  function doSaveEdit() {
    var rel = st.editPath;
    var text = st.editText;
    if (!window.confirm('覆盖写入 ' + rel + '?\n\n原文件将先备份到本机(配置目录 fm-backups,同文件保留 3 份)。')) return;
    var b64;
    try {
      b64 = btoa(unescape(encodeURIComponent(text)));
    } catch (e) {
      window.toast('内容编码失败(可能含非法字符)', 'warn');
      return;
    }
    runTransfer('保存', function () {
      return invoke('manage_files_write_text', Object.assign(baseArgs(), {
        path: rel, contentB64: b64, backup: true
      })).then(function (res) {
        st.view = 'list';
        refreshList();
        return res && res.backedUp ? '已保存;原文件已备份到 ' + res.backedUp : '已保存';
      });
    });
  }

  function doMkdir() {
    var name = window.prompt('新目录名称(将在 ' + (st.path || '根目录') + ' 下创建)');
    if (!name) return;
    if (name.indexOf('/') >= 0 || name === '.' || name === '..') {
      window.toast('目录名不合法', 'warn');
      return;
    }
    runTransfer('新建目录', function () {
      return invoke('manage_files_fs_op', Object.assign(baseArgs(), {
        path: joinPath(name), op: 'mkdir'
      })).then(function () { refreshList(); return '已创建 ' + name; });
    });
  }

  function doRename(name) {
    var next = window.prompt('新名称(原名:' + name + ')', name);
    if (!next || next === name) return;
    runTransfer('改名', function () {
      return invoke('manage_files_fs_op', Object.assign(baseArgs(), {
        path: joinPath(name), op: 'rename', newName: next
      })).then(function () { refreshList(); return '已改名为 ' + next; });
    });
  }

  function doDelete(name, isDir) {
    var what = isDir ? '目录(含其中全部内容)' : '文件';
    if (!window.confirm('删除' + what + ' ' + name + ' ?\n\n此操作不可撤销(可先「下载」留底)。')) return;
    runTransfer('删除', function () {
      return invoke('manage_files_fs_op', Object.assign(baseArgs(), {
        path: joinPath(name), op: 'delete'
      })).then(function () { refreshList(); return '已删除 ' + name; });
    });
  }

  function doCancel() {
    invoke('manage_files_cancel', {}).then(function () { return undefined; }).catch(function () { return undefined; });
  }

  // ===== 快照(容器)=====

  function loadSnapshot() {
    st.busy = true; st.err = ''; st.snapshot = null; render();
    var cid = st.target;
    var proj = currentProjectDir();
    invoke('manage_container_snapshot', {
      serverId: st.serverId, containerId: cid, projectDir: proj || undefined
    }).then(function (res) {
      st.busy = false;
      st.snapshot = res || null;
      render();
    }).catch(function (e) {
      st.busy = false;
      st.err = window.errText(e) || '未知错误';
      render();
    });
  }

  function currentContainer() {
    return (st.targets.container || []).filter(function (c) { return c.id === st.target; })[0] || null;
  }

  function currentProjectDir() {
    return st.dirBase && st.dirBase !== '/' ? st.dirBase : '';
  }

  // ===== 分发到同栈(前端串行编排,复用 upload)=====

  /** 容器状态 → 徽章轨道与中文(与 05 页容器表同口径) */
  function stateKind(state) {
    var s = String(state || '').toLowerCase();
    if (s === 'running') return 'running';
    if (s === 'exited') return 'exited';
    if (s === 'paused') return 'paused';
    return 'info';
  }

  function stateText(state) {
    var s = String(state || '').toLowerCase();
    if (s === 'running') return '运行中';
    if (s === 'exited') return '已停止';
    if (s === 'paused') return '已暂停';
    if (s === 'created') return '已创建';
    if (s === 'restarting') return '重启中';
    return String(state || '');
  }

  function openDistribute() {
    window.AppBus.pickPath({ directory: false, title: '选择要分发的文件' }).then(function (picked) {
      if (!picked) return;
      var cur = currentContainer();
      var project = cur ? String(cur.compose_project || '') : '';
      var peers = (st.targets.container || []).filter(function (c) {
        return project && String(c.compose_project || '') === project && c.id !== st.target;
      });
      st.view = 'distribute';
      st.dist = {
        localPath: String(picked),
        destDir: st.path,
        project: project,
        peers: peers,
        picked: peers.map(function (c) { return c.id; }),
        results: null
      };
      render();
    }).catch(function () {});
  }

  function runDistribute() {
    var d = st.dist;
    if (!d) return;
    var targets = (d.peers || []).filter(function (c) { return d.picked.indexOf(c.id) >= 0; });
    if (!targets.length) { window.toast('请至少勾选一个容器', 'warn'); return; }
    if (st.busy) return;
    st.busy = true; st.progress = null; st.opLabel = '分发'; st.err = '';
    render();
    subscribeProgress().then(function (un) {
      var results = [];
      var next = function (i) {
        if (i >= targets.length) {
          st.busy = false; st.progress = null; if (un) un();
          var ok = results.filter(function (r) { return r.ok; }).length;
          d.results = results;
          render();
          window.toast('分发完成:' + ok + '/' + results.length + ' 成功', ok === results.length ? 'ok' : 'warn');
          return;
        }
        var c = targets[i];
        invoke('manage_files_upload', {
          serverId: st.serverId, kind: 'container', target: c.id,
          destDir: d.destDir, localPath: d.localPath, backup: true
        }).then(function () {
          results.push({ name: c.names || c.id, ok: true, message: '已写入' });
          next(i + 1);
        }).catch(function (e) {
          results.push({ name: c.names || c.id, ok: false, message: window.errText(e) || '失败' });
          next(i + 1);
        });
      };
      next(0);
    });
  }

  // ===== 渲染 =====
  // 说明:列表/编辑器等视图整块重绘(数据量小、实现简单);所有动态数据走 textContent。

  function render() {
    var body = $('files-modal-body');
    if (!body) return;
    body.textContent = '';
    if (!st.open) return;
    body.appendChild(renderHead());
    if (st.err) {
      var errBox = el('div', 'files-err', '错误:' + st.err);
      body.appendChild(errBox);
    }
    if (st.view === 'edit') body.appendChild(renderEditor());
    else if (st.view === 'snapshot') body.appendChild(renderSnapshot());
    else if (st.view === 'distribute') body.appendChild(renderDistribute());
    else body.appendChild(renderList());
  }

  function renderHead() {
    var wrap = el('div', 'files-head');
    // 源切换
    var seg = el('div', 'files-seg');
    [['container', '容器'], ['volume', '数据卷'], ['deployDir', '部署目录(只读)']].forEach(function (pair) {
      var b = el('button', 'btn files-seg-btn' + (st.kind === pair[0] ? ' active' : ''), pair[1]);
      b.type = 'button';
      b.addEventListener('click', function () {
        if (st.busy) return;
        st.kind = pair[0];
        st.path = '';
        st.filter = '';
        st.view = 'list';
        var list = st.kind === 'container' ? st.targets.container : (st.kind === 'volume' ? st.targets.volume : []);
        if (st.kind === 'deployDir') st.target = st.dirBase || '/';
        else if (list.length && !list.filter(function (x) { return (x.id || x.name) === st.target; }).length) {
          var first = list[0];
          st.target = String(first.id || first.name || '');
        }
        render();
        if (st.kind !== 'deployDir') refreshList(); else refreshList();
      });
      seg.appendChild(b);
    });
    wrap.appendChild(seg);

    // 目标选择
    if (st.kind === 'deployDir') {
      var inp = el('input', 'input files-dirinput');
      inp.type = 'text';
      inp.value = st.target || (st.dirBase || '/');
      inp.placeholder = '/opt/app';
      inp.setAttribute('aria-label', '部署目录(绝对路径)');
      var openBtn = el('button', 'btn', '打开');
      openBtn.type = 'button';
      openBtn.addEventListener('click', function () {
        st.target = inp.value.trim();
        st.path = '';
        refreshList();
      });
      wrap.appendChild(inp);
      wrap.appendChild(openBtn);
    } else {
      var sel = el('select', 'select files-target');
      sel.setAttribute('aria-label', '选择目标');
      var list = st.kind === 'container' ? st.targets.container : st.targets.volume;
      list.forEach(function (x) {
        var id = String(x.id || x.name || '');
        var label = st.kind === 'container'
          ? (String(x.names || id) + (x.compose_project ? ' · ' + x.compose_project : ''))
          : String(x.name || id);
        var o = el('option', null, label);
        o.value = id;
        if (id === st.target) o.selected = true;
        sel.appendChild(o);
      });
      sel.addEventListener('change', function () {
        st.target = sel.value;
        st.path = '';
        refreshList();
      });
      wrap.appendChild(sel);
    }
    return wrap;
  }

  function renderPathBar() {
    var bar = el('div', 'files-pathbar');
    bar.appendChild(el('span', 'files-label', '路径'));
    var crumb = el('div', 'files-crumb mono');
    var root = el('button', 'files-crumb-btn', '/');
    root.type = 'button';
    root.addEventListener('click', function () { st.path = ''; refreshList(); });
    crumb.appendChild(root);
    var acc = '';
    st.path.split('/').filter(Boolean).forEach(function (seg) {
      acc = acc ? acc + '/' + seg : seg;
      var target = acc;
      crumb.appendChild(el('span', 'files-crumb-sep', '/'));
      var b = el('button', 'files-crumb-btn', seg);
      b.type = 'button';
      b.addEventListener('click', function () { st.path = target; refreshList(); });
      crumb.appendChild(b);
    });
    bar.appendChild(crumb);
    var up = el('button', 'btn', '上级');
    up.type = 'button';
    up.disabled = !st.path || st.busy;
    up.addEventListener('click', goUp);
    bar.appendChild(up);
    var f = el('input', 'input files-filter');
    f.type = 'text';
    f.placeholder = '过滤当前目录…';
    f.value = st.filter;
    f.setAttribute('aria-label', '过滤当前目录');
    f.addEventListener('input', function () { st.filter = f.value; render(); });
    bar.appendChild(f);
    return bar;
  }

  function renderActions() {
    var wrap = el('div', 'files-actions');
    var readOnly = st.kind === 'deployDir';
    var mk = function (label, fn, disabled, title) {
      var b = el('button', 'btn', label);
      b.type = 'button';
      b.disabled = !!disabled;
      if (title) b.title = title;
      b.addEventListener('click', fn);
      wrap.appendChild(b);
      return b;
    };
    mk('上传文件', doUpload, st.busy || readOnly || st.unsupported,
      readOnly ? '部署目录为只读源' : (st.unsupported ? '该容器无 shell,请用完整路径下载/上传(本页暂不支持)' : ''));
    mk('新建目录', doMkdir, st.busy || readOnly || st.unsupported, '');
    mk('刷新', function () { refreshList(); }, st.busy, '');
    if (st.kind === 'container') {
      mk('容器快照', function () { st.view = 'snapshot'; st.snapshot = null; render(); loadSnapshot(); }, st.busy, '环境变量 / 端口 / 进程一览');
      var peers = st.targets.container.filter(function (c) {
        var cur = currentContainer();
        return cur && cur.compose_project && String(c.compose_project || '') === String(cur.compose_project) && c.id !== st.target;
      });
      mk('分发到同栈(' + peers.length + ')', openDistribute, st.busy || readOnly || peers.length === 0,
        peers.length ? '把本地文件推到同一 compose 项目的其它容器' : '该容器不属于多容器 compose 项目');
    }
    if (st.busy) {
      var cancel = el('button', 'btn btn-danger', '取消传输');
      cancel.type = 'button';
      cancel.addEventListener('click', doCancel);
      wrap.appendChild(cancel);
    }
    return wrap;
  }

  function renderProgress() {
    var box = $('files-progress');
    if (!box) return;
    box.textContent = '';
    if (!st.busy && !st.progress) { box.classList.add('hidden'); return; }
    box.classList.remove('hidden');
    var p = st.progress || { phase: '', done: 0, total: 0 };
    box.appendChild(el('div', 'files-progress-text',
      st.opLabel + '… ' + (p.phase ? phaseLabel(p.phase) + ' ' : '') +
      (p.total ? fmtSize(p.done) + ' / ' + fmtSize(p.total) : (p.done ? fmtSize(p.done) : ''))));
    var track = el('div', 'files-progress-track');
    var fill = el('div', 'files-progress-fill');
    var pct = p.total ? Math.min(100, Math.round(p.done * 100 / p.total)) : (st.busy ? 5 : 0);
    fill.style.width = pct + '%';
    track.appendChild(fill);
    box.appendChild(track);
  }

  function renderList() {
    var wrap = el('div', 'files-list');
    wrap.appendChild(renderPathBar());
    wrap.appendChild(renderActions());
    if (st.note) wrap.appendChild(el('div', 'files-note', st.note));
    var prog = el('div', 'files-progress hidden');
    prog.id = 'files-progress';
    wrap.appendChild(prog);
    setTimeout(renderProgress, 0);

    if (st.busy && !st.entries.length) {
      wrap.appendChild(el('div', 'files-empty', '读取中…'));
      return wrap;
    }
    var list = visibleEntries();
    if (!list.length) {
      wrap.appendChild(el('div', 'files-empty',
        st.entries.length ? '无匹配条目' : (st.unsupported ? '该容器无 shell:请直接使用完整路径(结合终端)下载/上传' : '目录为空')));
      return wrap;
    }
    var tableWrap = el('div', 'table-wrap');
    var table = el('table', 'data-table files-table');
    var thead = el('thead');
    var tr = el('tr');
    ['名称', '类型', '大小', '权限', '时间', '操作'].forEach(function (h) {
      tr.appendChild(el('th', null, h));
    });
    thead.appendChild(tr);
    table.appendChild(thead);
    var tbody = el('tbody');
    list.forEach(function (e) {
      var row = el('tr');
      var nameTd = el('td', 'files-name-cell');
      nameTd.appendChild(el('span', 'files-name', e.name));
      if (e.isDir) {
        nameTd.style.cursor = 'pointer';
        nameTd.addEventListener('click', function () { enterDir(e.name); });
      }
      row.appendChild(nameTd);
      var typeTd = el('td');
      typeTd.appendChild(window.fillBadge(el('span'), e.isDir || e.isLink ? 'info' : 'ok',
        e.isDir ? '目录' : (e.isLink ? '链接' : '文件')));
      row.appendChild(typeTd);
      row.appendChild(el('td', 'mono', e.isDir ? '—' : fmtSize(e.size)));
      row.appendChild(el('td', 'mono', e.mode || ''));
      row.appendChild(el('td', 'mono', e.mtime || ''));
      var act = el('td', 'files-row-actions');
      if (e.isDir) {
        act.appendChild(rowBtn('进入', function () { enterDir(e.name); }));
      } else {
        act.appendChild(rowBtn('下载', function () { doDownload(joinPath(e.name)); }));
        act.appendChild(rowBtn('编辑', function () { doEdit(joinPath(e.name)); }));
      }
      if (st.kind !== 'deployDir' && !st.unsupported) {
        act.appendChild(rowBtn('改名', function () { doRename(e.name); }));
        act.appendChild(rowBtn('删除', function () { doDelete(e.name, e.isDir); }, true));
      }
      row.appendChild(act);
      tbody.appendChild(row);
    });
    table.appendChild(tbody);
    tableWrap.appendChild(table);
    wrap.appendChild(tableWrap);
    return wrap;
  }

  function rowBtn(label, fn, danger) {
    var b = el('button', 'btn btn-sm' + (danger ? ' btn-danger' : ''), label);
    b.type = 'button';
    b.disabled = st.busy;
    b.addEventListener('click', fn);
    return b;
  }

  function renderEditor() {
    var wrap = el('div', 'files-editor');
    var bar = el('div', 'files-actions');
    var back = el('button', 'btn', '返回列表');
    back.type = 'button';
    back.disabled = st.busy;
    back.addEventListener('click', function () { st.view = 'list'; render(); });
    bar.appendChild(back);
    var save = el('button', 'btn btn-danger', '保存并覆盖');
    save.type = 'button';
    save.disabled = st.busy;
    save.addEventListener('click', function () {
      var ta = $('files-editor-area');
      if (ta) st.editText = ta.value;
      doSaveEdit();
    });
    bar.appendChild(save);
    bar.appendChild(el('span', 'files-note', st.editPath + '(原 ' + fmtSize(st.editBytes) + ',编辑后保存将先备份原文件)'));
    wrap.appendChild(bar);
    var ta = el('textarea', 'manage-env-editor files-editor-area');
    ta.id = 'files-editor-area';
    ta.value = st.editText;
    ta.spellcheck = false;
    ta.setAttribute('aria-label', '文件内容');
    wrap.appendChild(ta);
    var prog = el('div', 'files-progress hidden');
    prog.id = 'files-progress';
    wrap.appendChild(prog);
    setTimeout(renderProgress, 0);
    return wrap;
  }

  function renderSnapshot() {
    var wrap = el('div', 'files-snapshot');
    var bar = el('div', 'files-actions');
    var back = el('button', 'btn', '返回列表');
    back.type = 'button';
    back.addEventListener('click', function () { st.view = 'list'; render(); });
    bar.appendChild(back);
    var again = el('button', 'btn', '刷新');
    again.type = 'button';
    again.disabled = st.busy;
    again.addEventListener('click', loadSnapshot);
    bar.appendChild(again);
    var toggle = el('button', 'btn', st.showSecrets ? '隐藏值' : '显示值');
    toggle.type = 'button';
    toggle.addEventListener('click', function () { st.showSecrets = !st.showSecrets; render(); });
    bar.appendChild(toggle);
    var copy = el('button', 'btn', '复制全部');
    copy.type = 'button';
    copy.addEventListener('click', copySnapshot);
    bar.appendChild(copy);
    wrap.appendChild(bar);
    if (st.busy) { wrap.appendChild(el('div', 'files-empty', '读取中…')); return wrap; }
    var s = st.snapshot;
    if (!s) { wrap.appendChild(el('div', 'files-empty', '暂无数据')); return wrap; }

    wrap.appendChild(el('div', 'files-note',
      '容器 ' + (s.name || s.containerId) + ' · 镜像 ' + (s.image || '') + ' · 状态 ' + (s.status || '') +
      ' · 重启 ' + (s.restartCount || 0) + ' 次'));

    // tech=true:按列宽补齐的原文(docker top / 端口 / 挂载三元组)放进横向滚动块,
    // 保空格不换行(否则 HTML 折叠或折行都会打散分栏)
    var section = function (title, rows, emptyText, tech) {
      wrap.appendChild(el('div', 'files-section-title', title));
      if (!rows || !rows.length) { wrap.appendChild(el('div', 'files-empty', emptyText || '(空)')); return; }
      if (tech) {
        var box = el('div', 'files-tech');
        rows.forEach(function (r) { box.appendChild(r); });
        wrap.appendChild(box);
      } else {
        rows.forEach(function (r) { wrap.appendChild(r); });
      }
    };

    // 声明对比(有差异才提示)
    if (s.missingEnvKeys && s.missingEnvKeys.length) {
      wrap.appendChild(el('div', 'files-note is-warn',
        'compose 声明但容器里没有的环境变量:' + s.missingEnvKeys.join('、')));
    }
    if (s.extraEnvKeys && s.extraEnvKeys.length) {
      wrap.appendChild(el('div', 'files-note', '容器里有但 compose 未声明的变量:' + s.extraEnvKeys.join('、')));
    }
    if (s.note) wrap.appendChild(el('div', 'files-note', s.note));

    section('环境变量(' + (s.env || []).length + ')', (s.env || []).map(function (kv) {
      var row = el('div', 'files-kv');
      row.appendChild(el('span', 'files-kv-key mono', kv[0]));
      row.appendChild(el('span', 'files-kv-val', st.showSecrets ? String(kv[1]) : '••••••'));
      return row;
    }), '无环境变量');
    section('端口映射(' + (s.ports || []).length + ')', (s.ports || []).map(function (p) {
      return el('div', 'files-line is-tech', p);
    }), '未映射端口', true);
    section('挂载(' + (s.mounts || []).length + ')', (s.mounts || []).map(function (m) {
      return el('div', 'files-line is-tech', m);
    }), '无挂载', true);
    section('进程(docker top)', (s.processes || []).map(function (p) {
      return el('div', 'files-line is-tech', p);
    }), '未取得进程表(容器可能未运行)', true);
    return wrap;
  }

  function copySnapshot() {
    var s = st.snapshot;
    if (!s) return;
    var lines = [];
    lines.push('容器 ' + (s.name || s.containerId) + ' · 镜像 ' + (s.image || '') + ' · 状态 ' + (s.status || ''));
    lines.push('== 环境变量 ==');
    (s.env || []).forEach(function (kv) { lines.push(kv[0] + '=' + (st.showSecrets ? kv[1] : '••••••')); });
    lines.push('== 端口 ==');
    (s.ports || []).forEach(function (p) { lines.push(p); });
    lines.push('== 挂载 ==');
    (s.mounts || []).forEach(function (m) { lines.push(m); });
    lines.push('== 进程 ==');
    (s.processes || []).forEach(function (p) { lines.push(p); });
    if (s.missingEnvKeys && s.missingEnvKeys.length) lines.push('compose 声明但容器缺失:' + s.missingEnvKeys.join('、'));
    if (s.extraEnvKeys && s.extraEnvKeys.length) lines.push('容器多出:' + s.extraEnvKeys.join('、'));
    var text = lines.join('\n');
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(function () { window.toast('已复制', 'ok'); })
        .catch(function () { window.toast('复制失败,请手动选择复制', 'warn'); });
    } else {
      window.toast('当前环境不支持剪贴板', 'warn');
    }
  }

  function renderDistribute() {
    var wrap = el('div', 'files-distribute');
    var bar = el('div', 'files-actions');
    var back = el('button', 'btn', '返回列表');
    back.type = 'button';
    back.disabled = st.busy;
    back.addEventListener('click', function () { st.view = 'list'; render(); });
    bar.appendChild(back);
    var go = el('button', 'btn btn-danger', '开始分发');
    go.type = 'button';
    go.disabled = st.busy;
    go.addEventListener('click', function () {
      var inp = $('files-dist-dest');
      if (inp) st.dist.destDir = inp.value.trim();
      runDistribute();
    });
    bar.appendChild(go);
    wrap.appendChild(bar);
    var d = st.dist;
    if (!d) { wrap.appendChild(el('div', 'files-empty', '暂无数据')); return wrap; }
    wrap.appendChild(el('div', 'files-note', '把本地文件推到同一 compose 项目的其它容器(串行执行,结束后逐台给出结果)'));
    var row = el('div', 'files-kv');
    row.appendChild(el('span', 'files-kv-key', '本机文件'));
    row.appendChild(el('span', 'files-kv-val mono', d.localPath));
    wrap.appendChild(row);
    var row2 = el('div', 'files-kv');
    row2.appendChild(el('span', 'files-kv-key', '目标目录'));
    var dest = el('input', 'input files-dirinput');
    dest.type = 'text';
    dest.id = 'files-dist-dest';
    dest.value = d.destDir || '';
    dest.placeholder = '容器内目录(如 /app/conf)';
    dest.setAttribute('aria-label', '目标目录');
    row2.appendChild(dest);
    wrap.appendChild(row2);
    wrap.appendChild(el('div', 'files-section-title', '目标容器(项目 ' + (d.project || '—') + ')'));
    (d.peers || []).forEach(function (c) {
      var line = el('label', 'files-check');
      var cb = el('input', null, null);
      cb.type = 'checkbox';
      cb.checked = d.picked.indexOf(c.id) >= 0;
      cb.disabled = st.busy;
      cb.addEventListener('change', function () {
        if (cb.checked) { if (d.picked.indexOf(c.id) < 0) d.picked.push(c.id); }
        else d.picked = d.picked.filter(function (x) { return x !== c.id; });
      });
      line.appendChild(cb);
      line.appendChild(el('span', 'files-check-name', String(c.names || c.id)));
      if (c.state) line.appendChild(window.fillBadge(el('span'), stateKind(c.state), stateText(c.state)));
      wrap.appendChild(line);
    });
    if (!(d.peers || []).length) wrap.appendChild(el('div', 'files-empty', '该项目没有其它容器'));
    if (d.results) {
      wrap.appendChild(el('div', 'files-section-title', '逐台结果'));
      d.results.forEach(function (r) {
        var line = el('div', 'files-kv');
        line.appendChild(window.fillBadge(el('span'), r.ok ? 'ok' : 'fail', r.ok ? '成功' : '失败'));
        line.appendChild(el('span', 'files-kv-key', r.name));
        line.appendChild(el('span', 'files-kv-val', r.message));
        wrap.appendChild(line);
      });
    }
    var prog = el('div', 'files-progress hidden');
    prog.id = 'files-progress';
    wrap.appendChild(prog);
    setTimeout(renderProgress, 0);
    return wrap;
  }

  bindOnce();

  // ===== 对外桥接(单次赋值)=====
  window.FilesKit = {
    openFor: openFor,
    openForVolume: openForVolume,
    openSnapshot: openSnapshot
  };
})();
