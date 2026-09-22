// 静态完整性守护(第三十四批):两道词法级检查,补上 scope-integrity(运行时)与
// bridge-integrity(显式桥)之外的两类空白 ——
//
// A. 未声明自由标识符(词法级)
//    scope-integrity.js 只在运行时执行「顶层 + DOMContentLoaded 回调」;元素级
//    addEventListener / setTimeout 等嵌套回调永不执行(getElementById 恒返回 null
//    还会短路以元素存在为前置的分支),故「未来在事件/定时器回调里新增裸引用」
//    这一形态没有守护 —— v5.14.0 的 `$` 事故正是此类,只是恰好落在初始化路径
//    才被运行时抓到(ROADMAP「质量发现 2026-09-18」)。
//    本检查按词法把**所有函数(含嵌套回调)**的作用域链静态建出来:
//    任意位置的自由标识符不在 作用域链 ∪ window 全局赋值集合 ∪ 内建白名单 中即报。
//    刻意放宽(避免误报,宁可漏报;全部写在这里,不做隐性妥协):
//      - ES5 语法假设(前端纪律):无箭头/class/let/const/模板字面量/解构;
//        模板字面量整体按字符串跳过(`${}` 内不检查);
//      - 对象字面量键、成员访问、`x:` 形态(键/标签)跳过;
//      - `typeof x` 的操作数跳过(特征探测是合法用法);
//      - 引用判定在整文件收集完成后统一进行(等价于 var/function 提升)。
//
// B. DOM id 完整性(静态)
//    第三十二批 F6 真 bug:两页各建 `id="rb-precheck-box"`,getElementById 取到
//    DOM 中更靠前者 → 预检结果写进另一页、模态空白。本检查固化为三条:
//      B1 index.html 内静态 id 不得重复;
//      B2 getElementById('x') / querySelector('#x') / $('x') 的字面量必须能在
//         index.html 静态 id 或 JS 动态赋值 id 中找到(引用不存在的 id 是静默失效
//         —— 第三十四批实测抓到 `settings-btn`(应为 `settings-entry-btn`,
//         见 app.js 版本徽点入口);修复后由本检查回归);
//      B3 动态赋值的 id 字面量重复(或与静态 id 撞名)默认失败;经评审确认
//         「任一时刻至多一个实例」的登记在 DUP_ALLOW(附理由)。
//
// 零依赖;不参与构建。退出码非零 = 有问题。
const fs = require('fs');
const path = require('path');
const ROOT = path.join(__dirname, '..');

// ===== 0. 白名单 =====

// 浏览器内建 / 语言内建名称(裸用合法)。宁可多收 —— 多收只可能漏报同名笔误,
// 少收会误报既有合法代码。
const BUILTIN_GLOBALS = new Set([
  // 语言内建
  'undefined', 'NaN', 'Infinity', 'arguments', 'globalThis',
  'Object', 'Array', 'String', 'Number', 'Boolean', 'Symbol', 'Date', 'RegExp',
  'Error', 'TypeError', 'RangeError', 'SyntaxError', 'ReferenceError', 'EvalError', 'URIError',
  'JSON', 'Math', 'Promise', 'Proxy', 'Reflect', 'Map', 'Set', 'WeakMap', 'WeakSet',
  'parseInt', 'parseFloat', 'isNaN', 'isFinite', 'encodeURI', 'decodeURI',
  'encodeURIComponent', 'decodeURIComponent', 'escape', 'unescape', 'eval', 'Function',
  // 浏览器 BOM/DOM
  'window', 'self', 'top', 'parent', 'frames', 'opener', 'document', 'location', 'history',
  'navigator', 'screen', 'console', 'event', 'localStorage', 'sessionStorage', 'indexedDB',
  'setTimeout', 'clearTimeout', 'setInterval', 'clearInterval', 'requestAnimationFrame',
  'cancelAnimationFrame', 'queueMicrotask', 'addEventListener', 'removeEventListener',
  'dispatchEvent', 'getComputedStyle', 'matchMedia', 'alert', 'confirm', 'prompt',
  'atob', 'btoa', 'fetch', 'XMLHttpRequest', 'WebSocket', 'EventSource', 'Worker',
  'CustomEvent', 'Event', 'AbortController', 'AbortSignal', 'URL', 'URLSearchParams',
  'FormData', 'Blob', 'File', 'FileReader', 'Image', 'Notification', 'performance', 'crypto',
  'MutationObserver', 'IntersectionObserver', 'ResizeObserver', 'DOMParser', 'TextEncoder',
  'TextDecoder', 'structuredClone', 'scrollTo', 'open', 'close', 'print', 'focus', 'blur',
  // DOM 接口与构造器(instanceof / new 的操作数)
  'HTMLElement', 'Element', 'Node', 'NodeList', 'HTMLCollection', 'EventTarget', 'DOMRect',
  'HTMLInputElement', 'HTMLSelectElement', 'HTMLButtonElement', 'HTMLTextAreaElement',
  'HTMLDivElement', 'HTMLSpanElement', 'HTMLFormElement', 'HTMLTableElement', 'HTMLLabelElement',
  'MouseEvent', 'KeyboardEvent', 'FocusEvent', 'ClipboardEvent', 'DragEvent', 'InputEvent',
  'WheelEvent', 'TouchEvent', 'getSelection', 'ArrayBuffer', 'Uint8Array', 'Intl',
  // Tauri 注入(全站经 window.__TAURI__ 访问;裸用也合法)
  '__TAURI__',
]);

// 动态 id 重复白名单:仅登记「已评审为同一容器顺序重建、任一时刻至多一个实例」的项。
// 新增重复若不在表内 → 直接 FAIL,提示逐条评审。
const DUP_ALLOW = [
  { id: 'rb-plan', reason: 'deploy-rollback.js 整栈/单镜像/确认三视图顺序写入同一共享模态体(deploy-modal-body,各 render 函数先清空 body),任一时刻至多一个实例' },
  { id: 'rb-exec-btn', reason: '同上(rb-plan 同名三处一并重建)' },
  { id: 'files-progress', reason: 'files.js 四视图顺序写入同一模态体(render() 先 body.textContent=\'\'),任一时刻至多一个实例' },
  { id: 'deploy-resume-discard-btn', reason: 'deploy.js「确认放弃 / 未确认」两个互斥 if/else 分支各建一次(渲染前 box.textContent=\'\' 清空),任一时刻至多一个实例;下行按 id 统一禁用时只操作在场的那一个' },
];

// 自由标识符允许列表(项目级补充;window 全局赋值集合自动收集,不在这里维护)
const EXTRA_GLOBALS = new Set([]);

// ===== 1. 词法器(ES5 子集) =====
const KEYWORDS = new Set([
  'if', 'else', 'for', 'while', 'do', 'switch', 'case', 'default', 'break', 'continue',
  'return', 'function', 'var', 'new', 'delete', 'typeof', 'instanceof', 'in', 'of',
  'void', 'this', 'null', 'true', 'false', 'try', 'catch', 'finally', 'throw',
  'class', 'extends', 'super', 'const', 'let', 'export', 'import', 'yield', 'await',
  'debugger', 'with',
]);

function isIdStart(c) { return /[A-Za-z_$]/.test(c); }
function isIdChar(c) { return /[A-Za-z0-9_$]/.test(c); }

// 切词:注释/字符串/正则/数字/标识符/标点。正则与除号的歧义用「前一显著 token」
// 启发式(前为 运算符/左括号/逗号/关键字 视为正则,前为 值 视为除号)。
function tokenize(src) {
  const toks = [];
  const n = src.length;
  let i = 0;
  let line = 1;
  let prev = null;
  const push = (type, value, startLine) => {
    const t = { type, value, line: startLine };
    toks.push(t);
    prev = t;
  };
  const regexAllowed = () => {
    if (!prev) return true;
    if (prev.type === 'punct') return '([{,;:=!&|?+-*%^~<>'.indexOf(prev.value) >= 0;
    if (prev.type === 'kw') return ['this', 'true', 'false', 'null'].indexOf(prev.value) < 0;
    return false;
  };

  while (i < n) {
    const c = src[i];
    if (c === '\n') { line++; i++; continue; }
    if (c === ' ' || c === '\t' || c === '\r') { i++; continue; }
    // 注释
    if (c === '/' && src[i + 1] === '/') { i += 2; while (i < n && src[i] !== '\n') i++; continue; }
    if (c === '/' && src[i + 1] === '*') {
      i += 2;
      while (i < n && !(src[i] === '*' && src[i + 1] === '/')) { if (src[i] === '\n') line++; i++; }
      i += 2;
      continue;
    }
    // 字符串
    if (c === '"' || c === "'") {
      const quote = c;
      const startLine = line;
      let out = '';
      i++;
      while (i < n) {
        const ch = src[i];
        if (ch === '\\') { out += ch + (src[i + 1] || ''); if (src[i + 1] === '\n') line++; i += 2; continue; }
        if (ch === quote) { i++; break; }
        if (ch === '\n') line++;
        out += ch;
        i++;
      }
      push('str', out, startLine);
      continue;
    }
    // 模板字面量(前端纪律禁用;按字符串整体跳过,`${}` 内不检查 —— 见文件头)
    if (c === '`') {
      const startLine = line;
      i++;
      while (i < n) {
        const ch = src[i];
        if (ch === '\\') { i += 2; continue; }
        if (ch === '`') { i++; break; }
        if (ch === '\n') line++;
        i++;
      }
      push('str', '`template`', startLine);
      continue;
    }
    // 正则字面量
    if (c === '/' && regexAllowed()) {
      const startLine = line;
      i++;
      let inClass = false;
      while (i < n) {
        const ch = src[i];
        if (ch === '\\') { i += 2; continue; }
        if (ch === '\n') { line++; break; }
        if (ch === '[') inClass = true;
        else if (ch === ']') inClass = false;
        else if (ch === '/' && !inClass) { i++; break; }
        i++;
      }
      while (i < n && isIdChar(src[i])) i++;
      push('regex', '/', startLine);
      continue;
    }
    // 数字
    if (/[0-9]/.test(c) || (c === '.' && /[0-9]/.test(src[i + 1] || ''))) {
      const startLine = line;
      let j = i;
      if (src[j] === '0' && /[xX]/.test(src[j + 1] || '')) {
        j += 2;
        while (j < n && /[0-9a-fA-F]/.test(src[j])) j++;
      } else {
        while (j < n && /[0-9]/.test(src[j])) j++;
        if (src[j] === '.') { j++; while (j < n && /[0-9]/.test(src[j])) j++; }
        if (src[j] === 'e' || src[j] === 'E') {
          let k = j + 1;
          if (src[k] === '+' || src[k] === '-') k++;
          if (/[0-9]/.test(src[k] || '')) { j = k; while (j < n && /[0-9]/.test(src[j])) j++; }
        }
      }
      push('num', src.slice(i, j), startLine);
      i = j;
      continue;
    }
    // 标识符/关键字
    if (isIdStart(c)) {
      const startLine = line;
      let j = i;
      while (j < n && isIdChar(src[j])) j++;
      const word = src.slice(i, j);
      push(KEYWORDS.has(word) ? 'kw' : 'id', word, startLine);
      i = j;
      continue;
    }
    push('punct', c, line);
    i++;
  }
  return toks;
}

// ===== 2. 作用域收集(单遍;引用延迟到收集完成后统一判定 = var/function 提升) =====
function collectRefs(toks) {
  const root = { names: new Set(), kind: 'fn', parent: null };
  const stack = [root];
  const refs = [];
  const decls = [];        // var 声明状态机(支持多声明符与 for(var .. in/of ..))
  let pendingFn = null;    // { name, params }
  let pendingCatch = null;
  let i = 0;
  const N = toks.length;

  const nearestFn = () => {
    for (let k = stack.length - 1; k >= 0; k--) if (stack[k].kind === 'fn') return stack[k];
    return root;
  };
  const pushScope = (s) => { s.parent = stack[stack.length - 1]; stack.push(s); };

  while (i < N) {
    const t = toks[i];

    // --- var 声明状态机 ---
    if (decls.length) {
      const d = decls[decls.length - 1];
      if (t.type === 'punct') {
        if (t.value === '(' || t.value === '[' || t.value === '{') d.depth++;
        else if (t.value === ')' || t.value === ']' || t.value === '}') d.depth--;
        else if (t.value === ',' && d.depth === 0) d.expectName = true;
        else if (t.value === ';' && d.depth <= 0) decls.pop();
      } else if (t.type === 'kw' && (t.value === 'in' || t.value === 'of') && d.depth === 0 && !d.expectName) {
        decls.pop();
      } else if (t.type === 'id' && d.expectName && d.depth === 0) {
        nearestFn().names.add(t.value);
        d.expectName = false;
        i++;
        continue;
      }
    }

    // --- function 头(名字 + 形参),体在 `{` 处压 fn 作用域 ---
    // 注意:关键字可能是**属性名**(对象键 / 成员访问,如 `.catch(...)`、`{ catch: 1 }`)——
    // 属性位置的同名 token 一律不得进入关键字分支(曾据此吞掉函数体的 `{`,整文件作用域漂移)
    const kwPrev = i > 0 ? toks[i - 1] : null;
    const kwIsMember = kwPrev && kwPrev.type === 'punct' && kwPrev.value === '.';
    if (t.type === 'kw' && t.value === 'function' && !kwIsMember) {
      let j = i + 1;
      let name = null;
      if (toks[j] && toks[j].type === 'id') { name = toks[j].value; j++; }
      while (j < N && !(toks[j].type === 'punct' && toks[j].value === '(')) j++;
      const params = [];
      if (j < N) {
        let depth = 1;
        let expectParam = true;
        j++;
        while (j < N && depth > 0) {
          const p = toks[j];
          if (p.type === 'punct') {
            if (p.value === '(') depth++;
            else if (p.value === ')') depth--;
            else if (p.value === ',' && depth === 1) expectParam = true;
          } else if (p.type === 'id' && depth === 1 && expectParam) {
            params.push(p.value);
            expectParam = false;
          }
          j++;
        }
      }
      if (name) nearestFn().names.add(name); // 函数声明:提升到最近 fn(表达式多登记一次也无害)
      pendingFn = { name, params };
      i = j;
      continue;
    }

    // --- catch 形参(其块作用域在 `{` 处压栈) ---
    // 必须是真正的 catch 子句:前面不是 `.`(Promise 链的 `.catch(...)` 到处都是),后面紧跟 `(`
    if (t.type === 'kw' && t.value === 'catch' && !kwIsMember &&
        toks[i + 1] && toks[i + 1].type === 'punct' && toks[i + 1].value === '(') {
      let j = i + 1;
      while (j < N && toks[j].type !== 'id') j++;
      pendingCatch = toks[j] ? toks[j].value : null;
      i = j + 1;
      continue;
    }

    if (t.type === 'punct' && t.value === '{') {
      if (pendingFn) {
        const s = { names: new Set(), kind: 'fn', parent: null };
        if (pendingFn.name) s.names.add(pendingFn.name);
        pendingFn.params.forEach((p) => s.names.add(p));
        pendingFn = null;
        pushScope(s);
      } else if (pendingCatch !== null) {
        pushScope({ names: new Set([pendingCatch]), kind: 'block', parent: null });
        pendingCatch = null;
      } else {
        pushScope({ names: new Set(), kind: 'block', parent: null });
      }
      i++;
      continue;
    }
    if (t.type === 'punct' && t.value === '}') {
      if (stack.length > 1) stack.pop();
      i++;
      continue;
    }

    if (t.type === 'kw' && t.value === 'var') {
      decls.push({ expectName: true, depth: 0 });
      i++;
      continue;
    }

    // --- 引用候选 ---
    if (t.type === 'id') {
      const p = i > 0 ? toks[i - 1] : null;
      const np = toks[i + 1] || null;
      const isMember = p && p.type === 'punct' && p.value === '.';
      const isKey = np && np.type === 'punct' && np.value === ':' &&
        (!p || (p.type === 'punct' && (p.value === '{' || p.value === ',')));
      const afterTypeof = p && p.type === 'kw' && p.value === 'typeof';
      if (!isMember && !isKey && !afterTypeof) {
        refs.push({ name: t.value, line: t.line, chain: stack.slice() });
      }
      i++;
      continue;
    }

    i++;
  }
  return refs;
}

// 收集 window.X = / globalThis.X = 赋值(全局集合;跨文件共享,前端桥接的合法出口)
function collectWindowGlobals(toks) {
  const out = [];
  for (let k = 0; k + 3 < toks.length; k++) {
    const t = toks[k];
    if (t.type !== 'id' || (t.value !== 'window' && t.value !== 'globalThis')) continue;
    if (!(toks[k + 1].type === 'punct' && toks[k + 1].value === '.')) continue;
    if (toks[k + 2].type !== 'id') continue;
    if (!(toks[k + 3].type === 'punct' && toks[k + 3].value === '=')) continue;
    if (toks[k + 4] && toks[k + 4].type === 'punct' && toks[k + 4].value === '=') continue; // ==
    out.push(toks[k + 2].value);
  }
  return out;
}

function resolveRefs(refs, globals) {
  const findings = [];
  for (const r of refs) {
    let found = false;
    for (let k = r.chain.length - 1; k >= 0; k--) {
      if (r.chain[k].names.has(r.name)) { found = true; break; }
    }
    if (!found && !globals.has(r.name)) findings.push(r);
  }
  return findings;
}

// ===== 3. DOM id 扫描(token 级,天然免疫注释/字符串里的假象) =====
const ID_NAME_OK = /^[A-Za-z][A-Za-z0-9_-]*$/;
const SIMPLE_ID_SELECTOR = /^#([A-Za-z][A-Za-z0-9_-]*)$/;

// 顶层实参切分:openIdx 指向 '(';返回每个实参的 token 数组(空调用返回 [])
function splitCallArgs(toks, openIdx) {
  const args = [];
  let cur = null;
  let depth = 0;
  for (let k = openIdx; k < toks.length; k++) {
    const t = toks[k];
    if (t.type === 'punct') {
      if (t.value === '(' || t.value === '[' || t.value === '{') {
        depth++;
        if (depth === 1) { cur = []; continue; }
      } else if (t.value === ')' || t.value === ']' || t.value === '}') {
        depth--;
        if (depth === 0) { if (cur && cur.length) args.push(cur); return args; }
      } else if (t.value === ',' && depth === 1) {
        if (cur && cur.length) args.push(cur);
        cur = [];
        continue;
      }
    }
    if (cur) cur.push(t);
  }
  return args;
}

// 收集「id 助手」:形如 `x.id = <形参>` / `setAttribute('id', <形参>)` 的函数,
// 记录该形参的**位置索引**;再把「把形参按位转交给已知 id 助手」的函数链式补入
// (前端字段构造助手统一形态:buildField / appendField / checkboxRow 等)。
// 返回 Map<函数名, 形参索引>。
function collectIdHelpers(toks) {
  const fns = [];
  for (let i = 0; i < toks.length; i++) {
    const t = toks[i];
    if (t.type !== 'kw' || t.value !== 'function') continue;
    const prev = i > 0 ? toks[i - 1] : null;
    if (prev && prev.type === 'punct' && prev.value === '.') continue;
    let name = null;
    let j = i + 1;
    if (toks[j] && toks[j].type === 'id') { name = toks[j].value; j++; }
    else if (prev && prev.type === 'punct' && prev.value === '=') {
      const before = i > 1 ? toks[i - 2] : null;
      if (before && before.type === 'id') name = before.value;
    }
    while (j < toks.length && !(toks[j].type === 'punct' && toks[j].value === '(')) j++;
    const params = [];
    if (j < toks.length) {
      let depth = 1;
      let expect = true;
      j++;
      while (j < toks.length && depth > 0) {
        const p = toks[j];
        if (p.type === 'punct') {
          if (p.value === '(' || p.value === '[' || p.value === '{') depth++;
          else if (p.value === ')' || p.value === ']' || p.value === '}') depth--;
          else if (p.value === ',' && depth === 1) expect = true;
        } else if (p.type === 'id' && depth === 1 && expect) {
          params.push(p.value);
          expect = false;
        }
        j++;
      }
    }
    if (!(toks[j] && toks[j].type === 'punct' && toks[j].value === '{')) continue;
    let depth = 0;
    let end = -1;
    for (let k = j; k < toks.length; k++) {
      const tt = toks[k];
      if (tt.type === 'punct' && tt.value === '{') depth++;
      else if (tt.type === 'punct' && tt.value === '}') { depth--; if (depth === 0) { end = k; break; } }
    }
    if (end < 0) continue;
    fns.push({ name, params, bodyStart: j, bodyEnd: end });
  }
  const helpers = new Map();
  for (const fn of fns) {
    if (!fn.name) continue;
    for (let k = fn.bodyStart + 1; k < fn.bodyEnd; k++) {
      const tt = toks[k];
      if (tt.type === 'id' && tt.value === 'id' &&
          toks[k - 1] && toks[k - 1].type === 'punct' && toks[k - 1].value === '.' &&
          toks[k + 1] && toks[k + 1].type === 'punct' && toks[k + 1].value === '=' &&
          toks[k + 2] && toks[k + 2].type === 'id' && fn.params.indexOf(toks[k + 2].value) >= 0) {
        helpers.set(fn.name, fn.params.indexOf(toks[k + 2].value));
        break;
      }
      if (tt.type === 'id' && tt.value === 'setAttribute' &&
          toks[k + 1] && toks[k + 1].type === 'punct' && toks[k + 1].value === '(' &&
          toks[k + 2] && toks[k + 2].type === 'str' && toks[k + 2].value === 'id' &&
          toks[k + 3] && toks[k + 3].type === 'punct' && toks[k + 3].value === ',' &&
          toks[k + 4] && toks[k + 4].type === 'id' && fn.params.indexOf(toks[k + 4].value) >= 0) {
        helpers.set(fn.name, fn.params.indexOf(toks[k + 4].value));
        break;
      }
    }
  }
  let changed = true;
  while (changed) {
    changed = false;
    for (const fn of fns) {
      if (!fn.name || helpers.has(fn.name)) continue;
      for (let k = fn.bodyStart + 1; k < fn.bodyEnd; k++) {
        const tt = toks[k];
        if (tt.type !== 'id' || !helpers.has(tt.value)) continue;
        if (!(toks[k + 1] && toks[k + 1].type === 'punct' && toks[k + 1].value === '(')) continue;
        const args = splitCallArgs(toks, k + 1);
        const a = args[helpers.get(tt.value)];
        if (a && a.length === 1 && a[0].type === 'id' && fn.params.indexOf(a[0].value) >= 0) {
          helpers.set(fn.name, fn.params.indexOf(a[0].value));
          changed = true;
          break;
        }
      }
    }
  }
  return helpers;
}

function scanJsIds(files) {
  const dynamic = []; // { id, file, line }
  const refs = [];    // { id, file, line, kind }
  for (const f of files) {
    const toks = f.toks;
    const helpers = collectIdHelpers(toks);
    for (let k = 0; k < toks.length; k++) {
      const t = toks[k];
      // 字符串里的 HTML 静态骨架:innerHTML 只允许静态骨架/文案,其中的 id="..." 也是
      // 动态创建的 id 来源(前端纪律:动态数据一律 createElement/textContent)
      if (t.type === 'str') {
        const re = /\bid\s*=\s*(?:"([^"]*)"|'([^']*)')/g;
        let m;
        while ((m = re.exec(t.value)) !== null) {
          const id = m[1] !== undefined ? m[1] : m[2];
          if (ID_NAME_OK.test(id)) dynamic.push({ id, file: f.name, line: t.line });
        }
      }
      // .id = 'x'
      if (t.type === 'id' && t.value === 'id' &&
          k > 0 && toks[k - 1].type === 'punct' && toks[k - 1].value === '.' &&
          toks[k + 1] && toks[k + 1].type === 'punct' && toks[k + 1].value === '=' &&
          toks[k + 2] && toks[k + 2].type === 'str' && ID_NAME_OK.test(toks[k + 2].value)) {
        dynamic.push({ id: toks[k + 2].value, file: f.name, line: t.line });
      }
      // setAttribute('id', 'x')
      if (t.type === 'id' && t.value === 'setAttribute' &&
          toks[k + 1] && toks[k + 1].type === 'punct' && toks[k + 1].value === '(' &&
          toks[k + 2] && toks[k + 2].type === 'str' && toks[k + 2].value === 'id' &&
          toks[k + 3] && toks[k + 3].type === 'punct' && toks[k + 3].value === ',' &&
          toks[k + 4] && toks[k + 4].type === 'str' && ID_NAME_OK.test(toks[k + 4].value)) {
        dynamic.push({ id: toks[k + 4].value, file: f.name, line: t.line });
      }
      // 经字段构造助手创建的 id(helper 名 + 形参索引;$ 是「读取器」不是「创建器」)
      if (t.type === 'id' && helpers.has(t.value) && t.value !== '$' &&
          toks[k + 1] && toks[k + 1].type === 'punct' && toks[k + 1].value === '(') {
        const args = splitCallArgs(toks, k + 1);
        const a = args[helpers.get(t.value)];
        if (a && a.length === 1 && a[0].type === 'str' && ID_NAME_OK.test(a[0].value)) {
          dynamic.push({ id: a[0].value, file: f.name, line: t.line });
        }
      }
      // getElementById('x') / $('x')
      if (t.type === 'id' && (t.value === 'getElementById' || t.value === '$') &&
          toks[k + 1] && toks[k + 1].type === 'punct' && toks[k + 1].value === '(' &&
          toks[k + 2] && toks[k + 2].type === 'str' && ID_NAME_OK.test(toks[k + 2].value) &&
          toks[k + 3] && toks[k + 3].type === 'punct' && toks[k + 3].value === ')') {
        refs.push({ id: toks[k + 2].value, file: f.name, line: t.line, kind: t.value === '$' ? "$('id')" : 'getElementById' });
      }
      // querySelector('#x') / querySelectorAll('#x')
      if (t.type === 'id' && (t.value === 'querySelector' || t.value === 'querySelectorAll') &&
          toks[k + 1] && toks[k + 1].type === 'punct' && toks[k + 1].value === '(' &&
          toks[k + 2] && toks[k + 2].type === 'str') {
        const m = toks[k + 2].value.match(SIMPLE_ID_SELECTOR);
        if (m) refs.push({ id: m[1], file: f.name, line: t.line, kind: t.value + "('#id')" });
      }
    }
  }
  return { dynamic, refs };
}

function scanHtmlIds(html) {
  const out = [];
  const re = /\bid\s*=\s*("([^"]*)"|'([^']*)')/g;
  let m;
  while ((m = re.exec(html)) !== null) {
    const id = m[2] !== undefined ? m[2] : m[3];
    const line = html.slice(0, m.index).split('\n').length;
    out.push({ id, line });
  }
  return out;
}

// 供夹具使用:id 三查(静态重复 / 悬空引用 / 动态重复+撞静态)
function checkDomIds(html, jsFiles, dupAllow) {
  const problems = [];
  const statics = scanHtmlIds(html);
  const staticCount = new Map();
  statics.forEach((s) => staticCount.set(s.id, (staticCount.get(s.id) || 0) + 1));
  staticCount.forEach((count, id) => {
    if (count > 1) problems.push('静态 id 重复: "' + id + '" 在 index.html 出现 ' + count + ' 次');
  });
  const staticSet = new Set(statics.map((s) => s.id));

  const { dynamic, refs } = scanJsIds(jsFiles);
  const dynamicSet = new Set(dynamic.map((d) => d.id));
  const allow = new Set((dupAllow || []).map((a) => a.id));
  const notes = [];

  refs.forEach((r) => {
    if (!staticSet.has(r.id) && !dynamicSet.has(r.id)) {
      problems.push('悬空 id 引用: ' + r.file + ':' + r.line + ' ' + r.kind + ' -> "' + r.id + '"(静态与动态 id 均不存在)');
    }
  });

  const byId = new Map();
  dynamic.forEach((d) => {
    if (!byId.has(d.id)) byId.set(d.id, []);
    byId.get(d.id).push(d);
  });
  byId.forEach((list, id) => {
    const collideStatic = staticSet.has(id);
    if (list.length > 1 || collideStatic) {
      const where = list.map((d) => d.file + ':' + d.line).join(', ');
      const what = (list.length > 1 ? '动态重复 ×' + list.length : '') +
        (list.length > 1 && collideStatic ? ' 且' : '') +
        (collideStatic ? '与静态 id 撞名' : '');
      if (allow.has(id)) notes.push('NOTE  已登记重名(评审判定任一时刻至多一个实例): "' + id + '" ' + what + ' @ ' + where);
      else problems.push('动态 id 重名未评审: "' + id + '" ' + what + ' @ ' + where);
    }
  });
  return { problems, notes };
}

// ===== 4. 夹具自测(先证明检查器抓得住,再拿它跑真代码) =====
function runFixtures() {
  let bad = 0;
  const fixtures = [
    {
      name: '嵌套回调内的裸引用(scope-integrity 盲区形态)',
      src: '(function () { function boot() { $(\'a\'); } boot(); })();',
      expect: ['$'],
    },
    {
      name: '跨文件局部名外泄(在 setTimeout 回调里)',
      src: '(function () { setTimeout(function () { helperFn(); }, 0); })();',
      expect: ['helperFn'],
    },
    {
      name: '外层已声明 → 不报(零误报面)',
      src: '(function () { var $ = function () { return 1; }; setTimeout(function () { $(); }, 0); })();',
      expect: [],
    },
    {
      name: '字符串/注释/正则中的名字 → 不报',
      src: '(function () { var s = "$nope"; /* $also */ var r = /\\$x/.test(s); })();',
      expect: [],
    },
    {
      name: '成员访问 / 对象键 / window 赋值 → 不报',
      src: '(function () { var o = { a: 1 }; o.a; window.foo = 1; })();',
      expect: [],
    },
    {
      name: '三元中段引用 → 报',
      src: '(function () { var a = 1; var b = missingOne ? a : a; })();',
      expect: ['missingOne'],
    },
  ];
  const globals = new Set(BUILTIN_GLOBALS);
  fixtures.forEach((fx) => {
    const found = resolveRefs(collectRefs(tokenize(fx.src)), globals).map((r) => r.name).sort();
    const want = fx.expect.slice().sort();
    const ok = found.length === want.length && found.every((x, idx) => x === want[idx]);
    if (!ok) {
      bad++;
      console.log('FAIL  [夹具] ' + fx.name + ': 期望 [' + want.join(',') + '],实际 [' + found.join(',') + ']');
    } else {
      console.log('PASS  [夹具] ' + fx.name);
    }
  });

  // id 三查夹具
  const idFx = [
    {
      name: '悬空 id 引用被抓',
      html: '<div id="a"></div>',
      js: [{ name: 'fx.js', src: '(function(){ document.getElementById("b"); })();' }],
      expectProblems: 1,
    },
    {
      name: '静态 id 重复被抓',
      html: '<div id="a"></div><span id="a"></span>',
      js: [],
      expectProblems: 1,
    },
    {
      name: '动态 id 重名未评审被抓',
      html: '<div></div>',
      js: [
        { name: 'fx1.js', src: '(function(){ var n = 1; n.id = "dup"; })();' },
        { name: 'fx2.js', src: '(function(){ var n = 2; n.id = "dup"; })();' },
      ],
      expectProblems: 1,
    },
    {
      name: '全绿:引用命中动态 id',
      html: '<div></div>',
      js: [{ name: 'fx.js', src: '(function(){ var n = 1; n.id = "made"; $("made"); })();' }],
      expectProblems: 0,
    },
  ];
  idFx.forEach((fx) => {
    const jsFiles = fx.js.map((j) => ({ name: j.name, toks: tokenize(j.src) }));
    const res = checkDomIds(fx.html, jsFiles, []);
    const ok = res.problems.length === fx.expectProblems;
    if (!ok) {
      bad++;
      console.log('FAIL  [夹具] ' + fx.name + ': 期望 ' + fx.expectProblems + ' 个问题,实际 ' + res.problems.length + ' 个: ' + res.problems.join(' | '));
    } else {
      console.log('PASS  [夹具] ' + fx.name);
    }
  });
  return bad;
}

// ===== 5. 跑真代码 =====
let fail = 0;
fail += runFixtures();

const uiDir = path.join(ROOT, 'ui');
const jsNames = fs.readdirSync(uiDir)
  .filter((f) => /\.js$/.test(f) && f[0] !== '_')   // 排除临时桩(_tauri-stub.js 之类)
  .sort();
const files = jsNames.map((name) => {
  const src = fs.readFileSync(path.join(uiDir, name), 'utf8');
  return { name, src, toks: tokenize(src) };
});

console.log('\n--- A. 未声明自由标识符(词法级,含嵌套回调)---');
const globals = new Set(BUILTIN_GLOBALS);
files.forEach((f) => collectWindowGlobals(f.toks).forEach((g) => globals.add(g)));
EXTRA_GLOBALS.forEach((g) => globals.add(g));
console.log('PASS  window 全局赋值集合收集: ' + (globals.size - BUILTIN_GLOBALS.size) + ' 个名称(跨文件)');
let freeFindings = [];
files.forEach((f) => {
  freeFindings = freeFindings.concat(
    resolveRefs(collectRefs(f.toks), globals).map((r) => ({ file: f.name, line: r.line, name: r.name }))
  );
});
freeFindings.forEach((r) => {
  fail++;
  console.log('FAIL  ' + r.file + ':' + r.line + ': "' + r.name + '" 未声明(不在作用域链,也不在 window 全局/内建白名单)');
});
if (!freeFindings.length) console.log('PASS  ' + files.length + ' 个 JS 文件:无未声明自由标识符');

console.log('\n--- B. DOM id 完整性(静态 + 动态)---');
const html = fs.readFileSync(path.join(uiDir, 'index.html'), 'utf8');
const domRes = checkDomIds(html, files, DUP_ALLOW);
domRes.notes.forEach((n) => console.log(n));
domRes.problems.forEach((p) => {
  fail++;
  console.log('FAIL  ' + p);
});
if (!domRes.problems.length) {
  const staticCount = scanHtmlIds(html).length;
  const { dynamic, refs } = scanJsIds(files);
  console.log('PASS  index.html 静态 id ' + staticCount + ' 个无重复;动态 id ' + dynamic.length + ' 处;引用 ' + refs.length + ' 处全部可解析');
}

console.log(fail === 0 ? '\n静态完整性:全部通过' : '\n静态完整性:发现 ' + fail + ' 个问题');
process.exit(fail === 0 ? 0 : 1);
