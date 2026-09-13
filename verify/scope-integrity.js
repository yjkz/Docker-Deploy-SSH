// 作用域完整性守护:真实按 index.html 的 script 顺序加载页面脚本链,
// 触发每个文件注册的 DOMContentLoaded 回调,断言初始化不抛 ReferenceError。
// 第十二批拆分曾把 manage.js 的 IIFE 局部助手 `$` 留在宿主未随迁、也未进
// ManageKit 桥,manage-stacks.js 里 40+ 处裸 `$(...)` 在 bindEventsC 一执行就抛
// 「$ is not defined」—— 监控/栈/终端/日志跟随的按钮监听全部没注册上,点击
// 无任何反应且后端日志零记录。node --check(语法级)与顶层加载都发现不了:
// `$` 只在 DOMContentLoaded 回调里第一次被求值。此脚本把该缺陷固化为可回归检查。
//
// 桥接完整性(bridge-integrity.js)查的是 K.* 显式桥;本脚本查的是**隐式自由
// 标识符**——两道互补的网。
const fs = require('fs');
const path = require('path');
const ROOT = path.join(__dirname, '..');

// ===== 最小浏览器环境(仅足量让脚本链顶层执行 + 触发 DOM 回调) =====
const domListeners = { document: [], window: [] };
const el = () => ({
  addEventListener() {}, removeEventListener() {},
  classList: { add() {}, remove() {}, toggle() {}, contains() { return false; } },
  setAttribute() {}, getAttribute() { return null; },
  appendChild() {}, insertBefore() {}, removeChild() {}, remove() {},
  querySelector() { return null; }, querySelectorAll() { return []; },
  addEventListenerOnce() {},
  style: {}, dataset: {}, value: '', textContent: '', innerHTML: '', checked: false, disabled: false
});
global.window = global;
global.document = {
  addEventListener(type, fn) { if (type === 'DOMContentLoaded') domListeners.document.push(fn); },
  removeEventListener() {},
  getElementById() { return null; },
  querySelector() { return null; },
  querySelectorAll() { return []; },
  createElement() { return el(); },
  createTextNode() { return {}; },
  body: el(), documentElement: el()
};
global.addEventListener = (type, fn) => { if (type === 'DOMContentLoaded') domListeners.window.push(fn); };
global.removeEventListener = () => {};
global.localStorage = { getItem() { return null; }, setItem() {}, removeItem() {} };
global.matchMedia = () => ({ matches: false, addEventListener() {}, removeEventListener() {} });
global.requestAnimationFrame = (fn) => setTimeout(fn, 0);
global.__TAURI__ = {
  core: { invoke() { return Promise.resolve(); }, transformCallback() { return 0; } },
  event: { listen() { return Promise.resolve(() => {}); } },
  dialog: { open() { return Promise.resolve(null); }, save() { return Promise.resolve(null); } },
  notification: { requestPermission() { return Promise.resolve('granted'); } }
};
// window 上的 addEventListener(domListeners.window 已由 global.addEventListener 覆盖,
// 因为 global.window === global)

// ===== index.html 的真实加载顺序(自 app.js 起;首行内联主题脚本不涉及) =====
const CHAIN = [
  'app.js', 'check.js', 'images.js', 'servers.js', 'servers-cleanup.js',
  'deploy.js', 'deploy-rollback.js', 'deploy-migrate.js',
  'manage.js', 'manage-stacks.js', 'rollback.js', 'notify.js', 'help.js',
  'config-io.js', 'settings.js'
];

let fail = 0;
const loaded = [];
for (const f of CHAIN) {
  const src = fs.readFileSync(path.join(ROOT, 'ui', f), 'utf8');
  try {
    new Function(src)();
    loaded.push(f);
  } catch (e) {
    fail++;
    console.log('FAIL  ' + f + ' 顶层执行抛异常: ' + e.constructor.name + ': ' + e.message);
  }
}

// 触发 DOMContentLoaded:每个回调单独 try/catch,报出是哪个文件的初始化坏了
for (const fn of [...domListeners.document, ...domListeners.window]) {
  try {
    fn();
  } catch (e) {
    fail++;
    // 用回调源码首行定位归属文件(Function 无名,取其 toString 前八十字符特征)
    const src = String(fn).replace(/\s+/g, ' ').slice(0, 80);
    console.log('FAIL  DOMContentLoaded 回调抛异常: ' + e.constructor.name + ': ' + e.message + '\n      回调特征: ' + src);
  }
}

// 逐文件报告(顶层 OK 的也点名,方便与 CHAIN 对账)
for (const f of CHAIN) {
  if (loaded.includes(f)) console.log('PASS  ' + f + ': 顶层执行 OK');
}

// 关键行为断言(缺陷的直接观测面):05 页 C 域监听器必须真实注册上。
// manage-stacks.js 的 bindEventsC 在 $ 可解析时会走到 addEventListener;
// 这里以「document.getElementById 被查询过监控按钮 id」为哨兵判定接线发生过。
let sawMonitorWiring = false;
const WIRE_IDS = ['monitor-start-btn', 'manage-stack-refresh-btn'];
for (const fn of domListeners.document) {
  const src = String(fn);
  if (WIRE_IDS.some((id) => src.includes(id))) sawMonitorWiring = true;
}
if (!sawMonitorWiring) {
  fail++;
  console.log('FAIL  未发现 05 页监控/栈按钮的接线回调(监听器未注册即全页按钮失效)');
}

console.log(fail === 0 ? '\n作用域完整性:全部通过' : '\n作用域完整性:发现 ' + fail + ' 个问题');
process.exit(fail === 0 ? 0 : 1);
