# verify/ — 无依赖的本地校验脚本

零依赖(只用 Node 内置模块)的回归脚本。**不参与构建与运行,不影响应用产物**;
改动 `ui/app.js` 的表单助手或拆出文件的桥接时,先跑这两个脚本比开浏览器快得多。

```bash
node verify/form-validation.js    # 表单校验助手 54 项断言(需全 PASS)
node verify/bridge-integrity.js   # 拆出文件的桥接完整性(3 个 Kit 全 PASS)
node verify/scope-integrity.js    # 全链脚本加载 + DOM 回调作用域完整性(需全 PASS)
```

## 为什么存在

`verify/form-validation.js` 覆盖的东西**浏览器截图看不到**:失焦校验的呈现闸门
(空表不标红 / 填过又清空才提示)、`when()` 切换是否清掉旧红字、`blocking:false`
是否真的不阻断、Enter 在 IME 组合 / textarea / disabled / readOnly 下是否被正确忽略、
`beginForm` 的 `submit` 是否真的被 `preventDefault`(不拦会导航整个 WebView)。
它自己带一个最小 DOM shim(`dom-shim.js`),加载**真实的 `ui/app.js`**,不是复制实现。

`verify/bridge-integrity.js` 源于一个真实缺陷:第十二批拆 JS 时 `ui/deploy.js` 末尾
写了**两个** `window.DeployKit = {...}` 字面量,后者整体覆盖前者,导致拆出的
`deploy-rollback.js` 顶层拿到 6 个 `undefined` —— **单镜像回滚模态一打开就抛
「Cannot read properties of undefined (reading 'test')」**,而 `node --check` 与
截图都发现不了。脚本把「资产赋值次数 ≠ 1」与「消费键未被提供」都判为失败。

`verify/scope-integrity.js` 源于另一个真实缺陷(v5.14.0 现场暴露):同批拆分把
`ui/manage.js` 的 IIFE 局部助手 `$` 留在宿主,**没随迁进 `manage-stacks.js`、也没进
ManageKit 桥**——`$` 不是全局,而 `toast`/`fillBadge` 恰好是 `window` 全局可裸引用,
这种差异静态扫不出来。结果 manage-stacks.js 的 `bindEventsC` 在 DOMContentLoaded
一触发就抛 `ReferenceError: $ is not defined`,**05 页栈/监控/终端/日志跟随的按钮
监听全部没注册上**:点击无反应、invoke 不发出、后端日志零记录。`node --check`
(语法级)与顶层加载(回调未执行)都发现不了——本脚本按 index.html 真实顺序加载
全链并**触发 DOMContentLoaded 回调**,专抓这类「只在初始化回调里第一次求值」的自由
标识符断裂。它与 bridge-integrity 互补:桥查 `K.*` 显式桥接,本脚本查隐式作用域。

## 边界

- 只覆盖 **JS 逻辑**;Rust 侧仍以 `cargo test` 为准,UI 视觉仍以浏览器 + Tauri 桩截图交
  judge 为准(见 ROADMAP「验证工作流」)。
- `dom-shim.js` 是**故意最小**的:只实现 app.js 表单助手用到的 DOM 子集
  (`classList` / `closest` / `querySelector(All)` / 事件冒泡 / `insertBefore` 语义)。
  它不追求通用,别拿它测其他模块。
