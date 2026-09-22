// 桥接完整性守护:宿主 window.<Kit> 暴露的设施必须覆盖所有拆出文件消费的键。
// 第十二批拆分曾把 window.DeployKit 写成两个字面量(后者整体覆盖前者),
// 导致 deploy-rollback.js 顶层取到 6 个 undefined —— 单镜像回滚模态一打开
// 就抛「Cannot read properties of undefined (reading 'test')」。此脚本把
// 该缺陷固化为可回归检查:任何新增 K.* 消费而未补桥接键,这里立刻失败。
const fs = require('fs');
const path = require('path');
const ROOT = path.join(__dirname, '..');

/** 宿主文件 → { kitKey, consumers[] } */
const WIRING = [
  {
    host: 'ui/deploy.js', kit: 'DeployKit',
    consumers: ['ui/deploy-rollback.js', 'ui/deploy-migrate.js']
  },
  {
    host: 'ui/manage.js', kit: 'ManageKit',
    consumers: ['ui/manage-stacks.js']
  },
  {
    host: 'ui/servers.js', kit: 'ServersKit',
    consumers: ['ui/servers-cleanup.js']
  },
  {
    // 第三十三批:文件管理模态(容器/卷/部署目录三源 + 容器快照)
    host: 'ui/files.js', kit: 'FilesKit',
    consumers: ['ui/manage.js']
  }
];

let fail = 0;
for (const cfg of WIRING) {
  const hostSrc = fs.readFileSync(path.join(ROOT, cfg.host), 'utf8');
  const assignments = hostSrc.match(new RegExp('window\\.' + cfg.kit + '\\s*=', 'g')) || [];
  // 从赋值处扫描到配对的花括号(字面量可能单行也可能多行缩进)
  const start = hostSrc.search(new RegExp('window\\.' + cfg.kit + '\\s*=\\s*\\{'));
  let body = '';
  if (start !== -1) {
    const open = hostSrc.indexOf('{', start);
    let depth = 0, end = -1;
    for (let i = open; i < hostSrc.length; i++) {
      if (hostSrc[i] === '{') depth++;
      else if (hostSrc[i] === '}') { depth--; if (depth === 0) { end = i; break; } }
    }
    if (end !== -1) body = hostSrc.slice(open + 1, end);
  }
  const provided = [];
  {
    const re = /([A-Za-z_$][\w$]*)\s*:\s*([A-Za-z_$][\w$]*)/g;
    let m;
    while ((m = re.exec(body))) provided.push(m[1]);
  }
  const need = new Set();
  for (const f of cfg.consumers) {
    const s = fs.readFileSync(path.join(ROOT, f), 'utf8');
    for (const k of (s.match(new RegExp('K\\.[A-Za-z_$][\\w$]*', 'g')) || [])) need.add(k.slice(2));
    // 第三十三批:宿主文件内联消费 `window.<Kit>.<键>`(未写 K.* 别名,懒取值避免加载顺序耦合)
    const reInline = new RegExp('window[.]' + cfg.kit + '[.][A-Za-z_$][A-Za-z0-9_$]*', 'g');
    for (const full of (s.match(reInline) || [])) need.add(full.split('.').pop());
  }
  const missing = [...need].filter(k => !provided.includes(k));
  const okAssign = assignments.length === 1;
  if (!okAssign) {
    fail++;
    console.log('FAIL  ' + cfg.kit + ' 被赋值 ' + assignments.length + ' 次(必须恰好 1 次,否则后者覆盖前者)');
  }
  if (missing.length) {
    fail++;
    console.log('FAIL  ' + cfg.kit + ' 缺少拆出文件消费的键: ' + missing.join(', '));
  }
  if (okAssign && !missing.length) {
    console.log('PASS  ' + cfg.kit + ': 单次赋值,覆盖 ' + need.size + ' 个消费键');
  }
}
console.log(fail === 0 ? '\n桥接完整性:全部通过' : '\n桥接完整性:发现 ' + fail + ' 个问题');
process.exit(fail === 0 ? 0 : 1);
