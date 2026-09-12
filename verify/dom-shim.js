// Minimal DOM shim: just enough to load ui/app.js and drive bindFieldValidation.
class ClassList {
  constructor(node){ this.node = node; this.s = new Set(); }
  add(...c){ c.forEach(x=>x&&this.s.add(x)); }
  remove(...c){ c.forEach(x=>this.s.delete(x)); }
  contains(c){ return this.s.has(c); }
  toggle(c,f){ if (f===undefined) f=!this.s.has(c); f?this.s.add(c):this.s.delete(c); return f; }
  get value(){ return Array.from(this.s).join(' '); }
}
let nextId = 1;
class El {
  constructor(tag){
    this.tagName = String(tag).toUpperCase();
    this.children = []; this.parentNode = null;
    this.attributes = {}; this.dataset = {};
    this._text = ''; this.value = ''; this.type = '';
    this.disabled = false; this.readOnly = false;
    this.checked = false;
    this._listeners = {};
    this.classList = new ClassList(this);
    this.id = '';
    this._uid = 'n' + (nextId++);
  }
  get className(){ return this.classList.value; }
  set className(v){ this.classList.s = new Set(String(v).split(/\s+/).filter(Boolean)); }
  get textContent(){ return this._text; }
  set textContent(v){ this._text = String(v); this.children.forEach(c=>{c.parentNode=null;}); this.children = []; }
  appendChild(c){ c.parentNode = this; this.children.push(c); return c; }
  insertBefore(c, ref){
    if (!ref){ return this.appendChild(c); }
    const i = this.children.indexOf(ref);
    if (i < 0) throw new Error('NotFoundError: insertBefore ref not a child');
    c.parentNode = this; this.children.splice(i,0,c); return c;
  }
  removeChild(c){ const i=this.children.indexOf(c); if(i>=0){this.children.splice(i,1); c.parentNode=null;} return c; }
  setAttribute(k,v){ this.attributes[k]=String(v); if(k==='id') this.id=String(v); }
  getAttribute(k){ return this.attributes[k]===undefined?null:this.attributes[k]; }
  removeAttribute(k){ delete this.attributes[k]; }
  addEventListener(t,fn){ (this._listeners[t]=this._listeners[t]||[]).push(fn); }
  removeEventListener(t,fn){ const a=this._listeners[t]||[]; const i=a.indexOf(fn); if(i>=0)a.splice(i,1); }
  dispatch(t, ev){
    const e = Object.assign({ type:t, target:this, preventDefault(){ this.defaultPrevented=true; } }, ev||{});
    let node = this;
    while (node){ (node._listeners[t]||[]).slice().forEach(fn=>fn(e)); node = node.parentNode; }
    return e;
  }
  closest(sel){ let n=this; while(n){ if (matches(n,sel)) return n; n=n.parentNode; } return null; }
  querySelector(sel){ return querySel(this, sel, true); }
  querySelectorAll(sel){ return querySel(this, sel, false); }
  get firstChild(){ return this.children[0]||null; }
  focus(){ global.document.activeElement = this; }
  // walk all descendants
  all(){ const out=[]; const walk=n=>n.children.forEach(c=>{out.push(c); walk(c);}); walk(this); return out; }
}
function matches(node, sel){
  if (!node || !node.tagName) return false;
  sel = String(sel).trim();
  const m = /^([a-zA-Z]+)?((?:\.[\w-]+)*)$/.exec(sel);
  if (!m) return false;
  const [, tag, cls] = m;
  if (tag && node.tagName !== tag.toUpperCase()) return false;
  if (cls){
    for (const c of cls.split('.').filter(Boolean)) if (!node.classList.contains(c)) return false;
  }
  return true;
}
function querySel(root, sel, first){
  // supports comma lists and simple tag/.class selectors
  const parts = String(sel).split(',').map(s=>s.trim()).filter(Boolean);
  const out = [];
  for (const p of parts){
    for (const n of root.all()) if (matches(n,p)) { out.push(n); if (first) return n; }
  }
  return first ? null : out;
}
const document = {
  readyState: 'complete',
  activeElement: null,
  _listeners: {},
  createElement(tag){ return new El(tag); },
  getElementById(id){ return document._byId[id] || null; },
  querySelector(sel){ return querySel(document.body, sel, true); },
  querySelectorAll(sel){ return querySel(document.body, sel, false); },
  addEventListener(t,fn){ (document._listeners[t]=document._listeners[t]||[]).push(fn); },
  removeEventListener(){},
  _byId: {},
};
document.body = new El('body');
document.documentElement = new El('html');
// Live scan (not a registry): app.js creates error nodes after load, so a
// snapshot registry would miss exactly the elements under test.
document.getElementById = function (id) {
  let hit = null;
  (function walk(n) {
    if (hit) return;
    if (n.id === id) { hit = n; return; }
    n.children.forEach(walk);
  })(document.body);
  return hit;
};
document.registerIds = function () { /* no-op: getElementById scans live */ };
global.document = document;
global.Element = El;
global.HTMLElement = El;
const store = {};
global.window = {
  document,
  localStorage: { getItem:k=>store[k]===undefined?null:store[k], setItem:(k,v)=>{store[k]=String(v);}, removeItem:k=>{delete store[k];} },
  addEventListener(){}, removeEventListener(){},
  crypto: { randomUUID: () => 'x' },
  navigator: { userAgent: 'node' },
  matchMedia: () => ({ matches:false, addEventListener(){}, removeEventListener(){} }),
  setTimeout: (fn,ms)=>setTimeout(fn,ms), clearTimeout: id=>clearTimeout(id),
  console,
};
global.window.window = global.window;
global.window.AppState = {};
global.navigator = global.window.navigator;
global.localStorage = global.window.localStorage;
global.setTimeout = setTimeout; global.clearTimeout = clearTimeout;
// Node's global window object semantics: app.js uses window.X and bare X
for (const k of ['document']) global.window[k] = global[k];
module.exports = { document, El, window: global.window };
