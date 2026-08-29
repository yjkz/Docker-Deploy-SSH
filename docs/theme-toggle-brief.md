# 简报:亮暗双主题 + 切换动效(ark-ui 实现契约)

遵循 ark-ui skill:family(ark)与 depth(complex)不变;新增 **scheme 轴**(light/dark),通过根节点属性切换,不复制页面实现。当前亮色 = A′ 白纸作战图;暗色 = 方案 A 原设定「作战控制台」。

## 1. 主题机制

- `<html data-ark-theme="ark" data-ark-depth="complex" data-ark-scheme="light|dark">`
- 默认 light;用户选择持久化 `localStorage['dd_scheme']`
- **防闪白**:`ui/index.html` `<head>` 内联一段微型脚本(早于 CSS 渲染执行):读 localStorage → 设置 `data-ark-scheme`(异常时静默回退 light)。约 3 行,不用模块。
- 实现方式:**优先"token 值翻转"**——现有样式大量引用 `var(--ark-*)`,在 `:root[data-ark-scheme="dark"]` 下重定义这些变量的值即可全局翻转;禁止为暗色复制一套组件样式。

## 2. 暗色 token 映射(方案 A 原设定:近黑舞台 + 白规则线 + 青 #18d1ff)

| 变量 | light(现状) | dark |
|---|---|---|
| `--ark-paper`(舞台/纸) | `#f4f6f6` | `#080a0b`(近黑) |
| `--ark-ink`(墨/文字/线) | `#080a0b` | `#f4f6f6`(纸白文字) |
| `--ark-paper-dim`(次级表面) | `#e8ebec` | `#101416` |
| 新增 `--ark-surface`(面板/卡片) | `#f4f6f6` 或按现状 | `#0d1113` |
| `--ark-line` | `rgba(8,10,11,.85)` | `rgba(244,246,246,.85)` |
| `--ark-line-soft` | `rgba(8,10,11,.18)` | `rgba(244,246,246,.18)` |
| 文字透明度变体(rgba(8,10,11,.x) 散字面量) | — | 换成 `rgba(244,246,246,.x)` 同档;建议抽成 `--ark-ink-65` 等变量,两套主题各自定义 |
| `--ark-signal` | `#18d1ff` 不变 | 不变(语义一致:激活/进行中/主操作) |
| `--ark-signal-dim` | `#d7f4fd` | `rgba(24,209,255,.14)` |
| `--ark-ok` | `#c8eb21` 底+墨字 | `rgba(200,235,33,.16)` 底+`#e2f36a` 字 |
| `--ark-warn` | `#ffd23f` 底+墨字 | 同 light(琥珀在暗底可读,保留墨字) |
| **badge-fail / badge-warn 之外的三态徽章** | ok=signal-dim 底墨字;fail=ink 底 paper 字 | ok=signal-dim(dark 版)底+青字;**fail=paper 底+ink 字(反转)**;warn 不变 |
| 顶部 dock | `--ark-ink` 黑底纸白字 | `#050607`(比舞台更深一档),文字规则不变 |
| 日志/终端面板(现为墨黑) | `#080a0b` | `#0d1113` + 1px `--ark-line` 描边(与舞台区分) |
| `.btn-primary` | 墨底纸白字 | 纸白底+墨字(反转),楔形仍青 |
| 悬停反白(hover 用 ink 反白 paper 的地方) | 墨↔纸 | 反转:纸↔墨 |

实现提示:先 grep style.css 里所有颜色字面量(`#` 与 `rgba(`),逐个归类到上述变量;禁止在组件规则里保留主题相关字面量。SVG 一律 `currentColor`,自动跟随。

## 3. 切换按钮(dock 右端)

- 位置:顶部 dock 最右侧,与导航项同高度;32×32px 图标按钮
- 图标:内联 SVG 太阳/月亮(1.5px 描边,`currentColor`),显示"当前可切换到"的目标主题(light 显示月亮=切到暗,dark 显示太阳=切到亮);也可做太阳↔月亮的 CSS 形变,任选其一,保持简单可靠优先
- 无障碍:`aria-label="切换亮暗主题"` + `title` 同文案;可见 focus(现有 focus-visible 规范);40px 命中区域不足时用 padding 补足
- 按钮配色随 dock 文字色(currentColor)

## 4. 切换动效(重点,要"有设计感"且合乎 skill 动效规范)

采用 **View Transitions API + 从按钮位置扩散的圆形揭示**(Ark 的方向性揭示语言,非单纯淡入淡出):

```js
// app.js(或独立 theme.js,自行判断放哪)
function toggleScheme(evt) {
  const next = document.documentElement.dataset.arkScheme === 'dark' ? 'light' : 'dark';
  const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
  const apply = () => {
    document.documentElement.dataset.arkScheme = next;
    try { localStorage.setItem('dd_scheme', next); } catch (_) {}
  };
  if (reduce || !document.startViewTransition) { apply(); return; }
  // 以按钮中心为圆心
  const r = evt && evt.currentTarget ? evt.currentTarget.getBoundingClientRect() : {left:innerWidth-48,top:24,width:32,height:32};
  const x = r.left + r.width / 2, y = r.top + r.height / 2;
  const end = Math.hypot(Math.max(x, innerWidth - x), Math.max(y, innerHeight - y));
  document.documentElement.style.setProperty('--scheme-x', x + 'px');
  document.documentElement.style.setProperty('--scheme-y', y + 'px');
  document.documentElement.style.setProperty('--scheme-r', end + 'px');
  document.startViewTransition(apply);
}
```

```css
/* 旧状态定格,新状态从按钮圆形扩散揭示 */
::view-transition-old(root), ::view-transition-new(root) { animation: none; mix-blend-mode: normal; }
::view-transition-new(root) {
  animation: scheme-reveal .5s cubic-bezier(.22,.61,.36,1);
  clip-path: circle(0 at var(--scheme-x,100%) var(--scheme-y,0));
}
@keyframes scheme-reveal { to { clip-path: circle(var(--scheme-r,150%) at var(--scheme-x,100%) var(--scheme-y,0)); } }
::view-transition-old(root) { animation: scheme-fade .5s ease both; }
@keyframes scheme-fade { to { opacity: .35; } }  /* 旧界面退为暗淡底,非纯 opacity 依赖(新层在上方揭示) */
@media (prefers-reduced-motion: reduce) { ::view-transition-*(root) { animation: none !important; } }
```

- 事件绑定用事件委托或直接绑按钮 click(注意 evt.currentTarget 取按钮矩形)
- `document.startViewTransition` 不可用(理论上 WebView2/Chromium 支持)或 reduce 时:直接切,无动画
- 加载时机:theme 初始化脚本独立于 AppBus,保证首帧前生效

## 5. 工程约束

1. 不改任何 invoke/事件/业务逻辑;新增的只有:head 内联脚本、按钮 DOM、toggle 函数、CSS 变量与动画
2. `node --check` 全部 js;id/class 引用核对;无 emoji;无新增"颜色字面量散落"(允许在 :root 两套主题块内定义)
3. a11y:切换后按钮 aria-label 不变;对比度:暗色下正文/次级文字/青色文字都要过 WCAG AA(青 #18d1ff 在 #080a0b 上对比度很高,可放心用于微标签)
4. **验证项(报告必附)**:
   - grep 两套主题的 :root 块覆盖了 style.css 里全部颜色字面量(改动前后 `grep -c '#\|rgba(' style.css` 对比,组件区应为 0 残留——SVG/注释除外)
   - localStorage 键 'dd_scheme';head 脚本早于样式表执行
   - `node --check` 通过;减少动效媒体查询存在
5. 提交:`git add ui/ && git commit -m "feat: dark scheme with view-transition toggle"`

## 6. 不做的事

- 不做"跟随系统"检测(默认 light,用户手动切)
- 不做每个面板独立的主题切换;不引第三方动画库
- 不改 deploy/check 等页面逻辑
