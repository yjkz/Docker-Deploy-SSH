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
  // 文件管理(第三十三批):三源 + 边界 + 备份口径都要在帮助里可检索
  ok(/文件管理/.test(help) && /docker cp/.test(help),
    'help.js 提到「文件管理」与 docker cp 通道',
    '第三十三批:容器/卷/部署目录三源;无 shell 镜像与已停止容器的边界要写明');
  ok(/宿主机可写目录/.test(help) && /白名单外一律只读/.test(help),
    'help.js 写明部署目录写权限由「宿主机可写目录」白名单决定(白名单外只读)',
    '第三十四批(五)用户裁决:白名单可写;白名单外维持只读(避免绕过「归档即回滚点」)');
  ok(/批量下载/.test(help) && /批量上传/.test(help),
    'help.js 提到多选批量传输(批量下载 / 批量上传)',
    '第三十四批(四)用户裁决:容器内多选批量传输(第三十三批留档清尾)');
  ok(/fm-backups/.test(help) && /保留 3 份/.test(help),
    'help.js 写明覆盖前备份到 fm-backups(同文件保留 3 份)',
    '覆盖类操作不可撤销,必须让用户知道备份位置与份数');
  ok(/分发到同栈/.test(help),
    'help.js 提到「分发到同栈」',
    '第三十三批 D:同 compose 项目的其它容器逐个分发');
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
  // v6.12.0:预检新增 tagRestore(标签待指回)与 envDrift(插值漂移)两个字段 ——
  // 两个入口都必须消费(读错/漏读 = 新状态静默不可见,用户无从确认)
  ok(/tagRestore/.test(drb) && /envDrift/.test(drb),
    'deploy-rollback.js 消费 tagRestore / envDrift(v6.12.0 新状态)',
    '四态渲染与漂移展示是「不静默」的落点;字段读错会静默退化成旧行为');
  ok(/tagRestore/.test(rbPage) && /envDrift/.test(rbPage),
    'rollback.js(06 页)同样消费 tagRestore / envDrift',
    '两入口同口径(单一事实来源纪律):只改一处会让 06 页缺新状态展示');
  ok(/envDrift[\s\S]{0,120}allowPartial|allowPartial[\s\S]{0,120}envDrift/.test(drb),
    '04 页把 envDrift 纳入 allowPartial 判定(漂移也须确认)',
    '漂移不确认 = 又一处「报告与实际不符」的静默失效');
}

console.log('\n---------------------------------------');
// ===== 6. 第三十一批 P1/P2:启动阶段加固的用户可见口径 =====
// 两条都是**行为变更**:P1 会删除同项目内的孤儿容器(用户必须知情,否则是
// 「静默删容器」);P2 把「隐式拉取」改成「显式报错」(不写清会被当成新故障)。
// 文案口径若再改,须同步本守护与 wiki/06、wiki/07。
console.log('\n=== 6. 启动阶段加固口径(P1 孤儿容器 / P2 禁止隐式拉取) ===');
{
  const help = read(HELP) || '';
  ok(/不在当前 compose 里的旧服务容器会被一并移除/.test(help),
    'help.js 写明「同项目内不在当前 compose 的旧服务容器会被一并移除」(P1)',
    '第三十一批 P1:up 带 --remove-orphans —— 会删容器,属行为变更,必须可见');
  ok(/不会隐式拉取镜像/.test(help),
    'help.js 写明「启动不会隐式拉取镜像,镜像缺失立即报错」(P2)',
    '第三十一批 P2:up 带 --pull never,把静默拉取变成显式报错');
  ok(/服务器拉取[\s\S]{0,40}不受影响/.test(help),
    'help.js 说明「服务器拉取」分类不受 --pull never 影响',
    'P2 的边界:拉取类服务在部署步骤 5 已显式 pull,不受影响');
  // 精确到 05 页两行的 code 片段(06 页行也含 up -d --remove-orphans,
  // 宽松写法会被它误命中 —— 首次变异测试即漏网,故收紧)
  ok(/docker compose up -d --remove-orphans<\/code>/.test(help),
    'help.js 的栈「启动」命令写明 --remove-orphans(05 页)',
    '第三十一批 P1:栈启动同口径;up 侧刻意不加 --pull never');
  ok(/docker compose down --remove-orphans<\/code>/.test(help),
    'help.js 的栈「停止」命令写明 --remove-orphans(05 页)',
    '第三十一批 P1:停止时孤儿容器与网络一并回收');
  // 第三十二批 F3:build 兜底(镜像缺失时 up 现场构建非归档版本)必须写明
  ok(/不会现场构建镜像|--no-build/.test(help),
    'help.js 写明「启动不会现场构建镜像」(F3,--no-build)',
    '第三十二批 F3:服务写了 build: 而镜像缺失时 up 会构建非归档版本;该旗标把它变成显式报错');
  // 第三十二批 F1/F2/F5:compose 与 override 的口径(显式 -f 链 = 归档为准)
  ok(/compose 与 override 以归档为准/.test(help),
    'help.js 说明「compose 与 override 以归档为准」(F1/F2/F5)',
    '显式 -f 链取代默认解析:遮蔽文件(compose.yaml 优先)与「最多一个 override」都不再影响回滚');
  // 第三十二批 F6:预检「归档无 compose 副本」提示必须两个入口都消费
  {
    const rbPage = read(ROLLBACK_UI) || '';
    const drbPage = read(DEPLOY_ROLLBACK_UI) || '';
    ok(/noComposeCopy/.test(rbPage) && /noComposeCopy/.test(drbPage),
      '两个回滚入口都消费 noComposeCopy(F6)',
      'RollbackPrecheck.no_compose_copy(camelCase);漏读 = 用户不知道本次 compose 是沿用的');
    // 第三十二批实测发现的真 bug:两个入口的预检容器若用同一 id,04 页模态
    // 的 getElementById 会取到 DOM 更靠前的 06 页元素 → 04 页预检结果静默不可见
    const rbId = (rbPage.match(/preWrap\.id = '([^']+)'/) || [])[1];
    const drbId = (drbPage.match(/pre\.id = '([^']+)'/) || [])[1];
    ok(!!rbId && !!drbId && rbId !== drbId,
      '两个入口的预检容器 id 不同(' + rbId + ' vs ' + drbId + ')',
      '同 id 时 DOM 靠前的 06 页元素会抢走 04 页模态的渲染目标(实测:模态内空白)');
    ok(/getElementById\('rb-precheck-box'\)/.test(drbPage),
      '04 页仍按 #rb-precheck-box 取渲染目标(与上一条配对,改名需同步两处)');
  }
}

console.log('PASS ' + pass + ' / FAIL ' + fail);
process.exit(fail === 0 ? 0 : 1);
