// 用户可见文案与事实的一致性守护(第二十九批 D1)。
//
// 为什么需要:2026-09-19 的 UX 审查发现 `ui/help.js`(全站帮助,用户实际会读)
// 里有一句「批量不做断点记录,中途打断会留下无法续传的半成品」—— 这个前提在
// 第十四批就被核实**不成立**(批量逐台复用单发 `deploy`/`deploy_stack`,后端恒
// `checkpoint = true`,断点必落),wiki/06、wiki/07 与三处代码注释当时都改了,
// 唯独用户可见的帮助文案漏改,**一漂就是十几个版本**。
//
// 现有守护的覆盖空白:`doc-consistency.js` 只查「版本号 / 命令数 / 测试数 / 页首
// 戳」这类**计数与元数据**;`contract-smoke.js` 只查命令/事件集合。两者都不看
// **用户能读到的操作说明**,所以这类「事实改了、文案没改」的漂移无人拦截。
//
// 本脚本的定位:**不是**通用文案检查(那会误报),而是把**历史上真实发生过的
// 口径翻案**固化成断言 —— 每条都注明「为什么这条被翻案、依据在哪」。
// 零依赖,不参与构建。

const fs = require('fs');
const path = require('path');

const ROOT = path.join(__dirname, '..');
const HELP = path.join(ROOT, 'ui', 'help.js');
const ROLLBACK_UI = path.join(ROOT, 'ui', 'rollback.js');
const DEPLOY_ROLLBACK_UI = path.join(ROOT, 'ui', 'deploy-rollback.js');

let pass = 0, fail = 0;
function ok(cond, label, extra) {
  if (cond) { pass++; console.log('  PASS  ' + label); }
  else { fail++; console.log('  FAIL  ' + label + (extra ? '\n        → ' + extra : '')); }
}
function read(p) {
  try { return fs.readFileSync(p, 'utf8'); } catch (e) { return null; }
}

// ===== 1. 已翻案口径不得复现 =====
// 每条:被翻案的旧说法 + 为什么错 + 依据
console.log('\n=== 1. 已翻案口径(不得在用户可见文案里复现) ===');
{
  const help = read(HELP);
  ok(help !== null, 'ui/help.js 可读');
  if (help) {
    // 1a) 「批量不落断点」——第十四批核实推翻
    //     依据:批量逐台复用单发 deploy/deploy_stack,后端恒 checkpoint=true;
    //     「批量不落断点」只描述无调用点的死代码 commands::deploy_batch
    ok(!/批量不做断点|批量不落断点/.test(help),
      'help.js 不含「批量不(做|落)断点」旧口径',
      '第十四批已核实:批量恒落断点(复用单发管线)。若确要改口径,须同时改本守护与 wiki/06、wiki/07');

    // 1b) 断点续传「只能从头发起」类说法(续传是既有能力)
    ok(!/断点.{0,6}无法(续传|恢复)|不能续传/.test(help),
      'help.js 不含「断点无法续传」类旧口径',
      '第十四批起:失败/取消台可从面板「续传此台」或「续传未完成服务器」继续');

    // 1c) 单镜像回滚「不支持」类说法(独立回滚中心早已支持单镜像)
    ok(!/单镜像.{0,8}(不支持|无法).{0,8}回滚/.test(help),
      'help.js 不含「单镜像不支持回滚」旧口径',
      '独立回滚中心支持单镜像回滚(需项目在软件内配置,见 wiki/06)');
  }
}

// ===== 2. 用户可见文案必须提到的能力(存在但没说 = 用户找不到) =====
console.log('\n=== 2. 关键能力在帮助里可被检索到 ===');
{
  const help = read(HELP) || '';
  // 回滚可用性预检(R1;第二十九批):这是用户判断「能不能回」的唯一入口
  ok(/回滚预检|可用性预检/.test(help),
    'help.js 提到「回滚预检」',
    '第二十九批 R1 新增:执行前核对每个服务的镜像来源(归档有包 / 按 ID 命中 / 回不去)');
  // 部署日报(B2)
  ok(/部署日报/.test(help),
    'help.js 提到「部署日报」',
    '第二十八批 B2:按天聚合部署记录发一条摘要;需在设置中心与通知中心两处开启');
  // 允许执行时段(B4)
  ok(/允许执行时段|执行时段/.test(help),
    'help.js 提到「允许执行时段」',
    '第二十八批 B4:日程可限定只在某时段内执行,到点不在时段内则当天跳过');
  // 多项目编排(B1)
  ok(/项目多选|多项目/.test(help),
    'help.js 提到「多项目」维度',
    '第二十八批 B1:批量模态可切换维度(服务器多选 / 项目多选)');
  // 开机自启(S1)
  ok(/开机.*自启|自启/.test(help),
    'help.js 提到「开机自启」',
    '第二十九批 S1:设置中心「通用」区可设置登录后自动启动');
}

// ===== 3. 两处配置同一功能的提示不得丢失 =====
// 探活 / 资源告警 / 部署日报三者都是「设置填数值 + 通知中心勾订阅」,
// 只做一半等于没开 —— 帮助与设置 hint 必须点明这层耦合。
console.log('\n=== 3.「两处配置」耦合提示 ===');
{
  const help = read(HELP) || '';
  const settings = read(path.join(ROOT, 'ui', 'settings.js')) || '';
  const all = help + settings;
  // 至少在一处提示「需在通知中心勾选订阅」类耦合(以部署日报为代表)
  ok(/通知中心.{0,20}(勾选|订阅)|勾选.{0,12}订阅/.test(all),
    '存在「设置填值 + 通知中心订阅」的耦合提示',
    '探活/告警/日报三处同模式:只填设置不勾订阅 = 不生效');
}

// ===== 4. 回滚取消路径(R3)不得回退 =====
console.log('\n=== 4. 06 页回滚可取消的接线 ===');
{
  const rb = read(ROLLBACK_UI) || '';
  // R3 的实现要点:06 页发起回滚时置 04 页「部署中」标志,
  // 否则 04 页取消按钮与托盘停止都不可用(只能硬等 docker load 跑完)
  ok(/beginRemoteOpFlag/.test(rb),
    'rollback.js 置 04 页「部署中」标志(beginRemoteOpFlag)',
    '第二十九批 R3:不置位则 06 页回滚无法取消(04 页取消按钮判 st.deploying)');
  ok(/endRemoteOpFlag/.test(rb),
    'rollback.js 收尾复位该标志(endRemoteOpFlag)',
    '只置不复位会让 04 页永久停在「部署中」');
  // deploy.js 需要把取消入口暴露给 06 页(单一实现)
  const dj = read(path.join(ROOT, 'ui', 'deploy.js')) || '';
  ok(/cancelDeploy:\s*onCancelDeploy/.test(dj),
    'DeployKit 暴露 cancelDeploy(06 页复用而不是自实现一份)',
    'R3 设计:取消只应有单一实现,避免两处判定漂移');
}

// ===== 5. 回滚预检的 UI 契约 =====
console.log('\n=== 5. 回滚预检 UI 契约(前后端字段名对齐;两个入口) ===');
{
  // 第二十九批补:预检必须**两个入口都有** —— 04 页(项目 id 驱动)与
  // 06 回滚中心(目录驱动)。首版只做了 04 页,用户实测发现 06 页没有按钮;
  // 而 06 页恰是配置漂移(项目改名/被删)后的兜底入口,比 04 页更需要预检。
  const rbPage = read(ROLLBACK_UI) || '';
  ok(/rollback_precheck/.test(rbPage),
    'rollback.js(06 回滚中心)也调 rollback_precheck',
    '两个入口都必须能预检;06 页传 dir(无 projectId)');
  ok(/回滚预检/.test(rbPage),
    '06 页界面出现「回滚预检」按钮文案');
  ok(/allowPartial/.test(rbPage),
    '06 页执行时传 allowPartial(确认后允许部分回滚)');

  const drb = read(DEPLOY_ROLLBACK_UI) || '';
  // 前端读的 camelCase 字段必须与后端契约一致(读错 = 静默 undefined)
  ok(/rollback_precheck/.test(drb),
    'deploy-rollback.js 调 rollback_precheck 命令');
  ok(/hasBlocking|remoteById|noManifest/.test(drb),
    '前端消费预检的 camelCase 字段(hasBlocking / remoteById / noManifest)',
    '后端 RollbackPrecheck 用 #[serde(rename_all="camelCase")];字段名读错会静默 undefined');
  ok(/allowPartial/.test(drb),
    '前端传 allowPartial(用户确认后允许部分回滚)',
    'R1:阻断项存在时后端拒绝执行,须显式确认后才带 allowPartial=true 重发');
}

console.log('\n---------------------------------------');
console.log('PASS ' + pass + ' / FAIL ' + fail);
process.exit(fail === 0 ? 0 : 1);
