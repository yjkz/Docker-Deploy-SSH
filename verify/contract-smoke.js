// 契约 smoke 测试(第二十二批):前后端命令/事件集合双向核对。
//
// 背景:硬约束 6 要求「新命令必在 lib.rs 注册 + wiki/04 登记」,但注册与
// 消费两侧长期靠人工核对。本脚本把两侧集合做成断言(零依赖,只用 Node
// 内置模块;**纯文本解析,不执行任何代码**)。
//
// 四条断言:
//   1. 前端 invoke 集合 ⊆ 后端注册集合(严格)——调用了未注册的命令 =
//      运行期必挂(ghost);
//   2. 后端注册 − 前端消费 ⊆ 白名单——注册了但无人调用的命令必须显式列
//      白名单(死命令/预留),防两个方向的腐化;
//   3. 前端 listen 集合 ⊆ 后端 emit 集合(严格)——监听了从不 emit 的事件;
//   4. 后端 emit − 前端 listen ⊆ 白名单(deploy-batch 跟随死代码路径)。
//
// 解析口径(保守、可解释):
// - 前端 invoke:**仅字面量形态** `invoke('cmd'` / `invoke("cmd"`(含
//   AppBus.invoke;命令名以 `_` 或已知前缀出现)。此外维护一张「动态形态
//   命令」补充表(DYNAMIC_COMMANDS)——`AppBus.invoke(cmd, ...)` 这类无法
//   静态枚举的调用点,其候选值在此显式登记(新增动态调用点时需同步);
// - 后端注册:`lib.rs` 的 generate_handler![...] 内 `mod::cmd,` 条目;
// - 后端 emit:`emit("name"` 且**名字过滤**:必须是 kebab-case(含 `-`)或
//   位于 EVENTS 白名单内 —— 排除 batch.rs 的 `batch-done` 这类「状态值
//   常量」误收(事件名体系见 wiki/04,全部为 kebab-case);
// - 前端 listen:`AppBus.on('name'` 字面量。
//
// 退出码非零 = 有不一致;零依赖;不参与构建,建议与 verify 族一起跑。
const fs = require('fs');
const path = require('path');
const ROOT = path.join(__dirname, '..');

function read(p) { return fs.readFileSync(path.join(ROOT, p), 'utf8'); }

// ===== 白名单与补充表(每条附理由;契约变更时同步维护) =====

// 动态形态 invoke 的命令(`AppBus.invoke(cmd, ...)` 配变量赋值处,无法静态枚举)
const DYNAMIC_COMMANDS = [
  'test_server',            // servers.js:`cmd = mode === 'test' ? 'test_server' : 'server_env_check'`
  'manage_volume_inspect',  // manage.js:showResourceInspect('manage_volume_inspect', ...)
  'manage_network_inspect', // manage.js:showResourceInspect('manage_network_inspect', ...)
  'manage_network_connect', // manage.js:`cmd = isConnect ? 'manage_network_connect' : 'manage_network_disconnect'`
  'manage_network_disconnect',
  'rollback_execute_stack', // deploy-rollback.js:beginRbExecution('rollback_execute_stack', ...) 内部转发
];

// 注册了但前端无调用点(死命令/预留;原因见条目)
const UNUSED_COMMAND_WHITELIST = [
  'prune_server', // 03 页「清理优化」改走 cleanup_preview/cleanup_execute 后的遗留兜底(见 wiki/04)
  'deploy_batch', // 阶段七后端编排死代码(批量改为前端编排;wiki/01 注、wiki/04)
];

// 后端 emit 但前端无监听的事件
const UNLISTENED_EVENT_WHITELIST = [
  'deploy-batch', // 跟随 deploy_batch 死代码路径(前端编排批量不发此事件;wiki/01 注)
];

// 事件白名单:全部事件名(用于 emit 收集过滤;kebab-case 事件全集,wiki/04)
const KNOWN_EVENT_NAMES = new Set([
  'deploy-progress', 'deploy-log', 'deploy-done', 'deploy-batch',
  'server-log', 'manage-stats', 'manage-exec-output', 'manage-logs',
  'migrate-log', 'migrate-done', 'migrate-project-log', 'migrate-project-done',
]);

// 事件常量名(后端以 const 定义、emit(常量名, ...) 调用;新增事件常量时同步)
const EVENT_CONSTANTS = [
  'EXEC_EVENT', 'STATS_EVENT', 'LOGS_EVENT',
];

// ===== 后端注册集合 =====
const libRs = read('src-tauri/src/lib.rs');
const handlerBlock = libRs.slice(libRs.indexOf('generate_handler!['));
const registered = new Set(
  [...handlerBlock.matchAll(/^\s+[a-z_]+::([a-z_]+),?\s*$/gm)].map((m) => m[1])
);

// ===== 前端 invoke 集合(字面量 + 动态补充表) =====
const uiFiles = fs.readdirSync(path.join(ROOT, 'ui')).filter((f) => f.endsWith('.js'));
const invoked = new Set(DYNAMIC_COMMANDS);
for (const f of uiFiles) {
  const src = read(path.join('ui', f));
  for (const m of src.matchAll(/(?:AppBus\.invoke|invoke)\(\s*['"]([a-z_]+)['"]/g)) {
    invoked.add(m[1]);
  }
}

// ===== 后端 emit 集合(仅 KNOWN_EVENT_NAMES ∪ kebab-case) =====
const rsFiles = [];
(function walk(dir) {
  for (const entry of fs.readdirSync(path.join(ROOT, dir), { withFileTypes: true })) {
    const rel = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(rel);
    else if (entry.name.endsWith('.rs')) rsFiles.push(rel);
  }
})('src-tauri/src');
const emitted = new Set();
for (const f of rsFiles) {
  const src = read(f);
  // 字面量形态:emit("name", ...) —— 仅 kebab-case 或已知事件名
  for (const m of src.matchAll(/\bemit\(\s*['"]([a-z-]+)['"]/g)) {
    if (m[1].includes('-') || KNOWN_EVENT_NAMES.has(m[1])) emitted.add(m[1]);
  }
  // 常量形态:emit(CONST_NAME, ...) —— 常量的值经 `const CONST: &str = "..."` 定义
  for (const constName of EVENT_CONSTANTS) {
    const useRe = new RegExp(`\\bemit\\(\\s*(?:\\w+\\.)?${constName}\\b`);
    if (useRe.test(src)) {
      const defRe = new RegExp(`const\\s+${constName}\\s*:\\s*&str\\s*=\\s*"([a-z-]+)"`);
      const def = src.match(defRe);
      if (def) emitted.add(def[1]);
    }
  }
}

// ===== 前端 listen 集合 =====
const listened = new Set();
for (const f of uiFiles) {
  const src = read(path.join('ui', f));
  for (const m of src.matchAll(/AppBus\.on\(\s*['"]([a-z-]+)['"]/g)) {
    listened.add(m[1]);
  }
}

// ===== 断言 =====
let fail = 0;
function report(label, bad) {
  if (bad.length === 0) {
    console.log(`PASS  ${label}`);
  } else {
    fail++;
    console.log(`FAIL  ${label}`);
    for (const item of bad) console.log(`      - ${item}`);
  }
}

console.log(`--- 命令集合(注册=${registered.size} 前端消费=${invoked.size})---`);
const ghostCommands = [...invoked].filter((c) => !registered.has(c));
report('前端调用 ⊆ 注册(无 ghost 命令)', ghostCommands);

const unusedCommands = [...registered]
  .filter((c) => !invoked.has(c))
  .filter((c) => !UNUSED_COMMAND_WHITELIST.includes(c));
report('注册未消费 ⊆ 白名单', unusedCommands);

console.log(`\n--- 事件集合(后端 emit=${emitted.size} 前端 listen=${listened.size})---`);
const ghostEvents = [...listened].filter((e) => !emitted.has(e));
report('前端监听 ⊆ 后端 emit(无 ghost 事件)', ghostEvents);

const unlistenedEvents = [...emitted]
  .filter((e) => !listened.has(e))
  .filter((e) => !UNLISTENED_EVENT_WHITELIST.includes(e));
report('后端 emit 未监听 ⊆ 白名单', unlistenedEvents);

console.log(fail ? `\n契约不一致 ${fail} 处` : '\n契约 smoke:全部通过');
process.exit(fail ? 1 : 0);
