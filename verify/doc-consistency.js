// 文档一致性守护(第二十一批)：版本号 / 命令数 / 测试数三类声明跨文档一致。
//
// 背景:2026-09-13 全量审查发现 90% 的文档失准同根 —— 版本戳与计数靠人工
// 同步,任何一次发版/加命令都可能漏某处。本脚本把三类「客观真值」做成断言:
//   真值源 1(版本号): src-tauri/tauri.conf.json 与 Cargo.toml 必须一致;
//   真值源 2(命令数): lib.rs generate_handler 注册条目实测;
//   真值源 3(测试数): 由 `--write` 模式在跑完 cargo test 后写入 VERSION.txt 缓存,
//                     随后与文档声明比对(避免本脚本自己跑 cargo test 太慢)。
//
// 检查对象(声明处):
//   - wiki/README.md      「当前版本:vX.Y.Z」「纯函数单测 N passed」
//   - ROADMAP.md          「版本 **vX.Y.Z**」「基线 `cargo test` **N passed**」
//   - wiki/04-契约参考.md  「## 1. 命令(N 个)」
//
// 用法:
//   node verify/doc-consistency.js            # 校验(读 VERSION.txt 缓存;缺失则跳过测试数比对)
//   node verify/doc-consistency.js --write    # 采集:读真值源并写 VERSION.txt(跑完 cargo test 后执行)
//
// 零依赖;不参与构建。退出码非零 = 有不一致。
const fs = require('fs');
const path = require('path');
const ROOT = path.join(__dirname, '..');

function read(p) { return fs.readFileSync(path.join(ROOT, p), 'utf8'); }

// ---- 真值源 ----
const tauriConf = read('src-tauri/tauri.conf.json');
const versionTauri = (tauriConf.match(/"version"\s*:\s*"([^"]+)"/) || [])[1];
const cargoToml = read('src-tauri/Cargo.toml');
const versionCargo = (cargoToml.match(/^version\s*=\s*"([^"]+)"/m) || [])[1];

const libRs = read('src-tauri/src/lib.rs');
const handlerBlock = libRs.slice(libRs.indexOf('generate_handler!['));
const commandCount = (handlerBlock.match(/^\s+[a-z_]+::[a-z_]+,?\s*$/gm) || []).length;

// 测试数缓存(由 --write 采集;不存在则本项跳过)
const cachePath = path.join(__dirname, 'VERSION.txt');
let cached = null;
try { cached = JSON.parse(fs.readFileSync(cachePath, 'utf8')); } catch (e) { /* 无缓存 */ }

// ---- --write 模式:采集真值 ----
if (process.argv.includes('--write')) {
  // 测试数需调用方先在 src-tauri 跑 cargo test 并传 --tests=N,或直接写缓存
  const testsArg = (process.argv.find(a => a.startsWith('--tests=')) || '').slice(8);
  const tests = testsArg ? Number(testsArg) : (cached && cached.tests) || null;
  fs.writeFileSync(cachePath, JSON.stringify({
    version: versionTauri,
    commands: commandCount,
    tests: tests,
    updatedAt: new Date().toISOString().slice(0, 10)
  }, null, 2) + '\n');
  console.log(`VERSION.txt 已更新: version=${versionTauri} commands=${commandCount} tests=${tests}`);
  process.exit(0);
}

let fail = 0;
function check(label, actual, expected) {
  const ok = String(actual) === String(expected);
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${label}: 文档=${expected} 实际=${actual}`);
  if (!ok) fail++;
}

console.log('--- 版本号(真值源: tauri.conf.json / Cargo.toml 必须一致)---');
check('tauri.conf.json ↔ Cargo.toml', versionTauri, versionCargo);

const wikiReadme = read('wiki/README.md');
const roadmap = read('ROADMAP.md');

const mWikiVer = wikiReadme.match(/当前版本:\s*v([0-9.]+)/);
check('wiki/README 当前版本', versionTauri, mWikiVer ? mWikiVer[1] : '(未找到声明)');

const mRoadVer = roadmap.match(/版本 \*\*v([0-9.]+)\*\*/);
check('ROADMAP 版本', versionTauri, mRoadVer ? mRoadVer[1] : '(未找到声明)');

console.log('\n--- 命令数(真值源: lib.rs generate_handler 实测)---');
const mWiki04 = read('wiki/04-契约参考.md').match(/## 1\. 命令\((\d+) 个/);
check('wiki/04 命令数声明', commandCount, mWiki04 ? mWiki04[1] : '(未找到声明)');

console.log('\n--- 测试数(对比缓存 VERSION.txt 与文档声明)---');
if (cached && cached.tests) {
  const mWikiTests = wikiReadme.match(/纯函数单测 (\d+) passed/);
  const mRoadTests = roadmap.match(/基线 `cargo test` \*\*(\d+) passed\*\*/);
  check('wiki/README 测试数', cached.tests, mWikiTests ? mWikiTests[1] : '(未找到声明)');
  check('ROADMAP 测试数', cached.tests, mRoadTests ? mRoadTests[1] : '(未找到声明)');
} else {
  console.log('SKIP  无 VERSION.txt 测试数缓存;跑完 cargo test 后执行 `node verify/doc-consistency.js --write --tests=N` 采集');
}

console.log(fail ? `\n不一致 ${fail} 处` : '\n文档一致性:全部通过');
process.exit(fail ? 1 : 0);
