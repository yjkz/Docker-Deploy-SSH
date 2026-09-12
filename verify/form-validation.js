// Headless verification of the phase-5 form-validation wiring.
// Loads ui/app.js under a minimal DOM shim, then drives bindFieldValidation /
// bindFormEnter / beginForm directly — screenshot-style checks cannot see
// whether the blur gating or the implicit-submit interception actually work.
const path = require('path');
require(path.join(__dirname, 'dom-shim.js'));
const fs = require('fs');
const vm = require('vm');

const ROOT = path.join(__dirname, '..');
const ctx = vm.createContext(global);
vm.runInContext(fs.readFileSync(path.join(ROOT, 'ui/app.js'), 'utf8'), ctx, { filename: 'app.js' });

const W = global.window;
const D = global.document;
let pass = 0, fail = 0;
function ok(cond, label, extra) {
  if (cond) { pass++; console.log('  PASS  ' + label); }
  else { fail++; console.log('  FAIL  ' + label + (extra ? '  → ' + extra : '')); }
}
function mkForm() {
  D.body = new global.Element('body');
  D._byId = {};
  return D.body;
}
function field(row, id, type, value) {
  const inp = D.createElement('input');
  inp.type = type || 'text';
  inp.id = id;
  inp.value = value === undefined ? '' : value;
  inp.classList.add('form-input');
  row.appendChild(inp);
  return inp;
}
function rowIn(parent, cls) {
  const r = D.createElement('div');
  r.classList.add.apply(r.classList, (cls || 'form-row').split(' '));
  parent.appendChild(r);
  return r;
}

console.log('\n=== 1. setFieldError 锚点修正 ===');
{
  const root = mkForm();
  const row = rowIn(root);
  const btnRow = D.createElement('div'); btnRow.classList.add('input-btn-row');
  row.appendChild(btnRow);
  const input = D.createElement('input'); input.id = 'k'; input.type = 'text';
  btnRow.appendChild(input);
  const hint = D.createElement('div'); hint.classList.add('form-hint');
  row.appendChild(hint);
  D.registerIds();
  W.setFieldError(input, '必填');
  const err = D.getElementById('k-error');
  ok(!!err, '错误元素已创建');
  ok(err && err.parentNode === row, '错误挂在 .form-row(而非 flex 的 .input-btn-row)', err && err.parentNode && err.parentNode.className);
  const kids = row.children;
  ok(kids.indexOf(err) === kids.indexOf(hint) - 1, '错误插在 form-hint 之前');
  ok(input.classList.contains('has-error'), '控件带 has-error');
  ok(input.getAttribute('aria-invalid') === 'true', 'aria-invalid=true');
  ok(input.getAttribute('aria-describedby') === 'k-error', 'aria-describedby 指向错误');
  ok(input.classList.contains('has-error'));
  ok(input.getAttribute('aria-describedby') === 'k-error', 'aria-describedby 关联');
  W.setFieldError(input, null);
  ok(!D.getElementById('k-error'), '清除后错误元素移除');
  ok(!input.classList.contains('has-error'), '清除后 has-error 移除');
  ok(input.getAttribute('aria-describedby') === null, '清除后 aria-describedby 移除');
}

console.log('\n=== 2. blur 闸门:必填要等填过/已提交,格式恒提示 ===');
{
  const root = mkForm();
  const r1 = rowIn(root); const name = field(r1, 'f-name');
  const r2 = rowIn(root); const port = field(r2, 'f-port', 'number', 'abc');
  D.registerIds();
  const v = W.bindFieldValidation(root, [
    { id: 'f-name', required: true, label: '名称', message: '此项必填' },
    { id: 'f-port', test: (x) => /^\d+$/.test(x), message: '需为整数' }
  ]);
  // Tab 扫过空表:必填不提示
  name.dispatch('focusout');
  ok(!name.classList.contains('has-error'), '空必填字段失焦不标红(闸门生效)');
  // 格式错误恒提示
  port.dispatch('focusout');
  ok(port.classList.contains('has-error'), '格式错误失焦即提示');
  // 填过又清空 → 必填提示
  name.value = 'x'; name.dispatch('focusout');
  ok(!name.classList.contains('has-error'), '填合法值失焦不标红');
  name.value = ''; name.dispatch('focusout');
  ok(name.classList.contains('has-error'), '填过又清空 → 必填失焦提示');
  // 输入时清除错误
  name.value = 'y'; name.dispatch('input');
  ok(!name.classList.contains('has-error'), 'input 清除本字段错误');
  // 提交期整体校验:必填与格式都出
  name.value = '';
  const res = v.validate();
  ok(name.classList.contains('has-error') && port.classList.contains('has-error'), '提交期两字段都标红');
  ok(res.missing.join(',') === '名称', 'missing 含「名称」', JSON.stringify(res.missing));
  ok(res.formats.length === 1, 'formats 记录格式错误', JSON.stringify(res.formats));
  ok(res.firstBad === name, 'firstBad 为登记顺序首个出错字段');
  ok(res.blockingErrors.length === 2, 'blockingErrors 计 2 条', JSON.stringify(res.blockingErrors));
}

console.log('\n=== 3. when() 条件规则:不适用即清除 + 切换重校验 ===');
{
  const root = mkForm();
  const r1 = rowIn(root); const key = field(r1, 'k-path');
  const r2 = rowIn(root); const pass = field(r2, 'k-pass', 'password');
  D.registerIds();
  let useKey = true;
  const v = W.bindFieldValidation(root, [
    { id: 'k-path', required: true, when: () => useKey, message: '私钥认证需填写私钥路径' },
    { id: 'k-pass', required: true, when: () => !useKey, message: '密码认证需填写登录密码' }
  ]);
  v.markSubmitted();
  v.checkField('k-path'); v.checkField('k-pass');
  ok(key.classList.contains('has-error'), 'Key 分支:私钥路径必填提示');
  ok(!pass.classList.contains('has-error'), 'Key 分支:密码不提示(规则不适用)');
  // 切到密码认证
  useKey = false;
  v.checkField('k-path'); v.checkField('k-pass');
  ok(!key.classList.contains('has-error'), '切密码后:私钥路径红字被清除');
  ok(pass.classList.contains('has-error'), '切密码后:密码必填提示');
  const res = v.validate();
  ok(res.missing.length === 0, '条件必填(无 label)不进「请填写」摘要', JSON.stringify(res.missing));
  ok(res.blockingErrors.length === 1, '但仍在 blockingErrors 中(摘要走原文兜底)', JSON.stringify(res.blockingErrors));
}

console.log('\n=== 4. blocking:false 只提示不阻断 ===');
{
  const root = mkForm();
  const r1 = rowIn(root); const n = field(r1, 'b-n', 'number', '99');
  D.registerIds();
  const v = W.bindFieldValidation(root, [
    { id: 'b-n', blocking: false, test: (x) => Number(x) <= 20, message: '需为 0 - 20' }
  ]);
  const res = v.validate();
  ok(n.classList.contains('has-error'), '非阻断规则照常贴字段提示');
  ok(res.blockingErrors.length === 0, '但不进 blockingErrors(调用方不阻断)');
  ok(res.firstBad === null, '也不占 firstBad');
}

console.log('\n=== 5. 一字段多规则:按登记顺序取第一条 ===');
{
  const root = mkForm();
  const r1 = rowIn(root); const dir = field(r1, 'm-dir', 'text', 'relative/path');
  D.registerIds();
  const v = W.bindFieldValidation(root, [
    { id: 'm-dir', required: true, label: '目录', message: '此项必填' },
    { id: 'm-dir', test: (x) => x === '' || x[0] === '/', message: '需为绝对路径' }
  ]);
  v.validate();
  const e = D.getElementById('m-dir-error');
  ok(e && e.textContent === '需为绝对路径', '非空时取格式规则文案', e && e.textContent);
  dir.value = '';
  v.validate();
  const e2 = D.getElementById('m-dir-error');
  ok(e2 && e2.textContent === '此项必填', '空时取必填规则文案', e2 && e2.textContent);
}

console.log('\n=== 6. bindFormEnter:仅单行 input、判 IME、不越确认 ===');
{
  const root = mkForm();
  const r1 = rowIn(root); const txt = field(r1, 'e-txt', 'text');
  const r2 = rowIn(root); const ta = D.createElement('textarea'); ta.id = 'e-ta'; r2.appendChild(ta);
  const r3 = rowIn(root); const num = field(r3, 'e-num', 'number');
  const r4 = rowIn(root); const chk = D.createElement('input'); chk.type = 'checkbox'; chk.id='e-chk'; r4.appendChild(chk);
  D.registerIds();
  let fired = 0;
  W.bindFormEnter(root, () => { fired++; });
  // Enter on text input → fires
  txt.dispatch('keydown', { key:'Enter' });
  ok(fired === 1, 'text input Enter 触发主入口', 'fired=' + fired);
  // textarea → no
  ta.dispatch('keydown', { key:'Enter' });
  ok(fired === 1, 'textarea Enter 不触发(保留换行)', 'fired=' + fired);
  // checkbox → no
  chk.dispatch('keydown', { key:'Enter' });
  ok(fired === 1, 'checkbox Enter 不触发', 'fired=' + fired);
  // IME composing → no
  txt.dispatch('keydown', { key:'Enter', isComposing:true });
  ok(fired === 1, 'IME 组合中 Enter 不触发(isComposing)', 'fired=' + fired);
  txt.dispatch('keydown', { key:'Enter', keyCode:229 });
  ok(fired === 1, 'keyCode 229(旧式 IME)不触发', 'fired=' + fired);
  // number type ok
  num.dispatch('keydown', { key:'Enter' });
  ok(fired === 2, 'number input Enter 触发', 'fired=' + fired);
  // other key → no
  txt.dispatch('keydown', { key:'a' });
  ok(fired === 2, '非 Enter 键不触发', 'fired=' + fired);
  // disabled / readOnly → no
  txt.disabled = true;
  txt.dispatch('keydown', { key:'Enter' });
  ok(fired === 2, 'disabled 字段 Enter 不触发', 'fired=' + fired);
  txt.disabled = false; txt.readOnly = true;
  txt.dispatch('keydown', { key:'Enter' });
  ok(fired === 2, 'readOnly 字段 Enter 不触发', 'fired=' + fired);
}

console.log('\n=== 7. beginForm:form 语义 + 隐式提交被拦 ===');
{
  const container = mkForm();
  container.appendChild(D.createElement('div'));
  D.registerIds();
  let action = 0;
  const api = W.beginForm(container, 'the-title');
  ok(api.form.tagName === 'FORM', '容器内容换成 <form>');
  ok(container.children.length === 1 && container.children[0] === api.form, 'form 是 body 的唯一子节点');
  ok(api.form.getAttribute('novalidate') === 'novalidate', '带 novalidate');
  ok(api.form.getAttribute('aria-labelledby') === 'the-title', 'aria-labelledby 指向模态标题');
  const ev = api.form.dispatch('submit');
  ok(ev.defaultPrevented === true, 'submit 被 preventDefault(不会导航 WebView)');
  ok(action === 0, '未登记动作时 submit 不炸也不调');
  api.onSubmit(() => { action++; });
  api.form.dispatch('submit');
  ok(action === 1, '登记后 submit 转主入口');
  api.submit();
  ok(action === 2, 'api.submit() 主动触发');
}

console.log('\n=== 8. clear() 复位闸门状态 ===');
{
  const root = mkForm();
  const r1 = rowIn(root); const f = field(r1, 'c-f');
  D.registerIds();
  const v = W.bindFieldValidation(root, [
    { id: 'c-f', required: true, label: 'X', message: '此项必填' }
  ]);
  v.markSubmitted();
  v.validate();
  ok(f.classList.contains('has-error'), '已标记错误');
  v.clear();
  ok(!f.classList.contains('has-error'), 'clear 清掉字段错误');
  // 复位后必填重新受闸门约束
  f.dispatch('focusout');
  ok(!f.classList.contains('has-error'), 'clear 后必填回到闸门约束(不立即标红)');
}

console.log('\n=== 9. select 走 change ===');
{
  const root = mkForm();
  const r1 = rowIn(root);
  const sel = D.createElement('select'); sel.id = 's-sel'; sel.value = ''; r1.appendChild(sel);
  D.registerIds();
  const v = W.bindFieldValidation(root, [
    { id: 's-sel', test: (x) => x !== '', message: '请选择' }
  ]);
  sel.dispatch('change');
  ok(sel.classList.contains('has-error'), 'select change 触发校验');
  sel.value = 'v';
  sel.dispatch('change');
  ok(!sel.classList.contains('has-error'), 'select 改值后清除错误');
}

console.log('\n---------------------------------------');
console.log('PASS ' + pass + ' / FAIL ' + fail);
process.exit(fail === 0 ? 0 : 1);
