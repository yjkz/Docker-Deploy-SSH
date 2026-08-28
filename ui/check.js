/* ============================================================
 * check.js — 环境检测页逻辑(依赖 app.js 提供的全局工具)
 *
 * 后端命令(字段为 Rust snake_case 原样序列化):
 * - host_check() -> { docker_installed, daemon_running, compose_ok,
 *                     docker_version, arch, error, disk_free_gb }
 * - start_docker() -> 阻塞直至守护进程就绪或 60 秒失败(reject 中文字符串)
 * ============================================================ */
(function () {
  'use strict';

  /** 临时目录所在盘剩余空间低于该值(GB)视为未通过 */
  var DISK_MIN_GB = 2;

  var st = {
    starting: false,  // 正在执行 start_docker(含其后的复检)
    pollTimer: null   // 守护进程未通过时的 5 秒轮询定时器
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

  function getRow(key) {
    return document.getElementById('check-' + key);
  }

  function setBadge(key, kind, text) {
    var row = getRow(key);
    if (!row) return;
    var badge = row.querySelector('.badge');
    if (!badge) return;
    badge.className = 'badge badge-' + kind;
    badge.textContent = text;
  }

  function setDetail(key, text) {
    var row = getRow(key);
    if (!row) return;
    var detail = row.querySelector('.check-detail');
    if (!detail) return;
    detail.textContent = text;
    detail.classList.remove('hidden');
  }

  function clearAction(key) {
    var row = getRow(key);
    if (!row) return;
    var action = row.querySelector('.check-action');
    if (!action) return;
    action.textContent = '';
    action.classList.add('hidden');
  }

  /** 命令展示框:命令文本 + 一键复制按钮 */
  function cmdBox(command) {
    var box = el('div', 'cmd-box');
    box.appendChild(el('code', 'cmd-text', command));
    var btn = el('button', 'btn btn-mini', '复制');
    btn.type = 'button';
    btn.addEventListener('click', function () {
      window.copyText(command);
    });
    box.appendChild(btn);
    return box;
  }

  // ===== 检测项骨架(每行 = 徽章 + 名称 + 详情 + 操作区)=====

  function buildRows() {
    var list = document.getElementById('check-list');
    if (!list) return;
    list.textContent = '';
    [
      { key: 'docker_installed', name: 'Docker 已安装' },
      { key: 'daemon_running', name: 'Docker 守护进程运行中' },
      { key: 'compose_ok', name: 'docker compose 可用' },
      { key: 'disk', name: '磁盘空间充足(临时目录)' },
      { key: 'arch', name: '本机架构' }
    ].forEach(function (item) {
      var row = el('div', 'check-item');
      row.id = 'check-' + item.key;
      row.appendChild(el('span', 'badge badge-info', '待检测'));
      var body = el('div', 'check-body');
      body.appendChild(el('div', 'check-name', item.name));
      body.appendChild(el('div', 'check-detail hidden'));
      body.appendChild(el('div', 'check-action hidden'));
      row.appendChild(body);
      list.appendChild(row);
    });
  }

  // ===== 未通过时的操作区 =====

  /** docker_installed 未通过:winget 安装命令 + 复制 */
  function renderInstallAction() {
    var action = getRow('docker_installed').querySelector('.check-action');
    action.textContent = '';
    action.classList.remove('hidden');
    action.appendChild(el('div', 'action-hint',
      '未检测到 Docker,请先安装 Docker Desktop(以管理员身份在 PowerShell 中运行):'));
    var list = el('div', 'cmd-list');
    list.appendChild(cmdBox('winget install Docker.DockerDesktop'));
    action.appendChild(list);
  }

  /** daemon_running 未通过:一键启动按钮 + 可复制命令区 */
  function renderStartAction(starting) {
    var action = getRow('daemon_running').querySelector('.check-action');
    action.textContent = '';
    action.classList.remove('hidden');

    var btn = el('button', 'btn btn-primary', starting ? '启动中…' : '一键启动 Docker');
    btn.type = 'button';
    btn.id = 'start-docker-btn';
    btn.disabled = !!starting;
    btn.addEventListener('click', onStartDocker);
    action.appendChild(btn);

    action.appendChild(el('div', 'action-hint',
      '也可手动处理(管理员 PowerShell 执行命令,或直接启动 Docker Desktop):'));
    var list = el('div', 'cmd-list');
    list.appendChild(cmdBox('Start-Service com.docker.service'));
    list.appendChild(cmdBox('%ProgramFiles%\\Docker\\Docker\\Docker Desktop.exe'));
    action.appendChild(list);
  }

  /** compose_ok 未通过:升级提示 */
  function renderComposeAction() {
    var action = getRow('compose_ok').querySelector('.check-action');
    action.textContent = '';
    action.classList.remove('hidden');
    action.appendChild(el('div', 'action-hint',
      '请升级 Docker Desktop 至包含 docker compose 插件的版本后重新检测。'));
  }

  /** 磁盘空间未通过:清理提示 */
  function renderDiskAction(freeGb) {
    var action = getRow('disk').querySelector('.check-action');
    action.textContent = '';
    action.classList.remove('hidden');
    action.appendChild(el('div', 'action-hint',
      '剩余空间不足 ' + DISK_MIN_GB + ' GB(当前 ' + freeGb.toFixed(1) + ' GB)。' +
      '请清理临时目录所在盘:运行 Windows 磁盘清理,或执行 docker image prune、' +
      'docker container prune 删除无用镜像与容器后重新检测。'));
  }

  // ===== 渲染 =====

  function renderBanner(allPass) {
    var banner = document.getElementById('check-banner');
    if (!banner) return;
    if (allPass) {
      banner.className = 'banner banner-ok';
      banner.textContent = '环境就绪,可以开始部署';
    } else {
      banner.className = 'banner banner-fail';
      banner.textContent = '环境检测未通过,请处理下方未通过项后重新检测';
    }
  }

  function renderErrorNote(report) {
    var note = document.getElementById('check-error');
    if (!note) return;
    var msg = report ? errText(report.error) : '';
    if (msg) {
      note.textContent = '后端检测信息:' + msg;
      note.classList.remove('hidden');
    } else {
      note.textContent = '';
      note.classList.add('hidden');
    }
  }

  /**
   * 用 host_check 结果渲染整页,并同步 AppState.hostOk + 导航禁用态。
   * 后端在守护进程未运行时提前返回:此时 compose_ok 恒为 false、arch 为 None,
   * 前端据此显示「待确认」而非误导性的升级提示。
   */
  function render(report) {
    var daemonUp = !!report.daemon_running;

    // 1) Docker 已安装
    if (report.docker_installed) {
      setBadge('docker_installed', 'ok', '通过');
      setDetail('docker_installed', report.docker_version || '已检测到 docker 命令');
      clearAction('docker_installed');
    } else {
      setBadge('docker_installed', 'fail', '未通过');
      setDetail('docker_installed', '未检测到 docker 命令');
      renderInstallAction();
    }

    // 2) 守护进程
    if (daemonUp) {
      setBadge('daemon_running', 'ok', '通过');
      setDetail('daemon_running', '守护进程响应正常');
      clearAction('daemon_running');
    } else if (st.starting) {
      setBadge('daemon_running', 'warn', '启动中');
      setDetail('daemon_running', '正在启动 Docker 守护进程…');
      renderStartAction(true);
    } else {
      setBadge('daemon_running', 'fail', '未通过');
      setDetail('daemon_running', '守护进程未响应');
      renderStartAction(false);
    }

    // 3) docker compose
    if (report.compose_ok) {
      setBadge('compose_ok', 'ok', '通过');
      setDetail('compose_ok', 'docker compose 插件可用');
      clearAction('compose_ok');
    } else if (!daemonUp) {
      setBadge('compose_ok', 'warn', '待确认');
      setDetail('compose_ok', '守护进程未运行,无法确认(启动 Docker 后自动复检)');
      clearAction('compose_ok');
    } else {
      setBadge('compose_ok', 'fail', '未通过');
      setDetail('compose_ok', '未检测到可用的 docker compose 插件');
      renderComposeAction();
    }

    // 4) 磁盘空间(查询失败为非关键信息,不阻塞整体结论)
    var diskOk;
    if (report.disk_free_gb === null || report.disk_free_gb === undefined) {
      setBadge('disk', 'warn', '警告');
      setDetail('disk', '无法获取磁盘剩余空间(不影响部署)');
      clearAction('disk');
      diskOk = true;
    } else {
      var freeGb = Number(report.disk_free_gb);
      setDetail('disk', '临时目录所在盘剩余 ' + freeGb.toFixed(1) + ' GB');
      if (freeGb >= DISK_MIN_GB) {
        setBadge('disk', 'ok', '通过');
        clearAction('disk');
        diskOk = true;
      } else {
        setBadge('disk', 'fail', '未通过');
        renderDiskAction(freeGb);
        diskOk = false;
      }
    }

    // 5) 本机架构(仅展示行)
    setBadge('arch', 'info', '信息');
    setDetail('arch', report.arch || (daemonUp ? '未知' : '未知(守护进程未运行)'));
    clearAction('arch');

    // 顶部横幅 + 全局状态 + 导航
    var allPass = !!report.docker_installed && daemonUp && !!report.compose_ok && diskOk;
    renderBanner(allPass);
    renderErrorNote(report);
    window.AppState.hostOk = allPass;
    window.refreshNav();
  }

  /** host_check 本身失败(Tauri 不可用等):整页错误态 */
  function renderCheckError(err) {
    var list = document.getElementById('check-list');
    if (list) {
      list.textContent = '';
      var row = el('div', 'check-item');
      row.appendChild(el('span', 'badge badge-fail', '错误'));
      var body = el('div', 'check-body');
      body.appendChild(el('div', 'check-name', '环境检测失败'));
      body.appendChild(el('div', 'check-detail', errText(err) || '未知错误'));
      row.appendChild(body);
      list.appendChild(row);
    }
    var banner = document.getElementById('check-banner');
    if (banner) {
      banner.className = 'banner banner-fail';
      banner.textContent = '环境检测失败';
    }
    renderErrorNote(null);
    window.AppState.hostOk = false;
    window.refreshNav();
  }

  // ===== 检测与轮询 =====

  function stopPoll() {
    if (st.pollTimer) {
      window.clearInterval(st.pollTimer);
      st.pollTimer = null;
    }
  }

  /** 守护进程未通过时每 5 秒重查一次,直到通过 */
  function schedulePoll() {
    if (st.pollTimer) return;
    st.pollTimer = window.setInterval(function () {
      window.AppBus.invoke('host_check')
        .then(function (report) {
          render(report);
          if (report.daemon_running) {
            stopPoll();
            window.toast('Docker 守护进程已就绪', 'ok');
          }
        })
        .catch(function () { /* 单次轮询失败忽略,等待下个周期 */ });
    }, 5000);
  }

  /** 执行一次 host_check 并渲染;返回 Promise<report|null> */
  function runCheck() {
    stopPoll();
    return window.AppBus.invoke('host_check')
      .then(function (report) {
        render(report);
        return report;
      })
      .catch(function (err) {
        renderCheckError(err);
        return null;
      });
  }

  // ===== 一键启动 Docker =====

  function onStartDocker() {
    if (st.starting) return;
    st.starting = true;
    stopPoll();

    setBadge('daemon_running', 'warn', '启动中');
    setDetail('daemon_running', '正在启动 Docker 守护进程(最长约 60 秒)…');
    renderStartAction(true);

    window.AppBus.invoke('start_docker')
      .catch(function (err) {
        window.toast(errText(err) || '启动 Docker 失败', 'fail');
      })
      .then(function () {
        // 无论成功或失败,都立即重新检测刷新界面
        return runCheck();
      })
      .then(function (report) {
        st.starting = false;
        if (report && report.daemon_running) {
          window.toast('Docker 守护进程已就绪', 'ok');
        } else {
          // 仍未通过:恢复按钮可点击,并进入 5 秒轮询直到通过
          setBadge('daemon_running', 'fail', '未通过');
          setDetail('daemon_running', '守护进程未响应');
          renderStartAction(false);
          schedulePoll();
          window.toast('Docker 尚未就绪,将每 5 秒自动重新检测', 'warn');
        }
      });
  }

  // ===== 初始化 =====

  function init() {
    buildRows();
    var recheck = document.getElementById('recheck-btn');
    if (recheck) {
      recheck.addEventListener('click', function () {
        runCheck();
      });
    }
    runCheck();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
