# UI 重构简报:方案 A′「白纸作战图」(ark + complex, 纸面反转)

遵循 ark-ui skill(design-language / depth-levels / recipes / family-depth-matrix)。
设计契约:`data-ark-theme="ark" data-ark-depth="complex"`。只改 ui/ 呈现层,不改任何 invoke/事件逻辑与业务行为。

## 1. Tokens(语义化 CSS 变量,挂 :root[data-ark-theme="ark"])

```css
--ark-ink: #080a0b;        /* 墨黑:文字/1px规则线/黑壳 */
--ark-paper: #f4f6f6;      /* 纸白:主表面,≥75% 构图 */
--ark-paper-dim: #e8ebec;  /* 次级表面:表头/悬停/分隔带 */
--ark-signal: #18d1ff;     /* 青:激活/进行中/主操作,仅色块与细线,不做长正文色 */
--ark-signal-dim: #d7f4fd; /* 青的 12% 淡底:通过水印/选中底 */
--ark-ok: #c8eb21;         /* 酸绿:仅成功徽章底,配墨字 */
--ark-warn: #ffd23f;       /* 警示底(墨字)——从 ink 系派生的中性琥珀,仅一处 */
--ark-fail: #080a0b;       /* 失败徽章:墨底白字(不用红,保持双色纪律) */
--ark-line: rgba(8,10,11,.85);
--ark-line-soft: rgba(8,10,11,.18);
--font-cjk: "Noto Sans SC", "Source Han Sans SC", "PingFang SC", "Microsoft YaHei", sans-serif;
--font-cond: "Arial Narrow", "Roboto Condensed", "DIN Condensed", sans-serif;
--font-mono: "IBM Plex Mono", "SFMono-Regular", Consolas, monospace;
--radius: 0;               /* 禁圆角;功能半径最多 2px(输入框) */
```

## 2. 全局壳(Shell)

- **左栏 64px 纯黑 edge dock**(`--ark-ink`):纵向四项导航。每项 = 等宽数字索引(`01`…`04`,10px,letter-spacing .14em,rgba 白 .55)+ 竖排?不——竖排中文禁用(可读性),用水平小字(11px)。激活态:左侧 3px 青色实心条 + 数字变青 + 文字变白;非激活文字 rgba 白 .72;禁用态(LOCKED_PAGES)文字 rgba 白 .30 + cursor not-allowed + title 保留"环境检测未通过"。底部 dock 末端放版本号 `v0.1.0`(9px,rgba 白 .4,竖向字距)。
- **顶部 4px 黑发丝线**横贯内容区;内容区舞台为纸白。
- **主内容区**:左缘 1px `--ark-line-soft` 竖线与 dock 分隔;舞台内 padding 40-56px;最大宽度 1280px 居中偏左(不对称平衡,右侧留白)。
- **页首体系(每页)**:超大 condensed 编号(72-96px,`--font-cond`,色 `--ark-ink`,line-height .82,右下压一条 1px 墨线延伸到右缘)+ 下方双语标题:中文 22px 粗 + 英文全大写微标签(11px,letter-spacing .18em,色 rgba 墨 .5)。例:`02 / 镜像列表 IMAGES`。
- **按钮体系**(`.btn`):方形,1px 墨线,纸底墨字,内边距 10px 18px;左侧 4px 实心楔形(clip-path 三角)默认墨色。主操作 `.btn-primary`:墨底纸白字 + 楔形青色。危险/确认 `.btn-danger`:墨底白字 + 楔形用 `--ark-fail` 同色(即纯黑,靠文案"确认删除?"表意)。hover:整体平移 1px + 楔形变青。focus-visible:2px 青色 outline + 1px offset。
- **输入框**:1px `--ark-line` 底边 + 无框其余三边?不——全 1px 边框,底边 2px 墨(强调),radius 2px,高 36px;focus 底边变青 2px。

## 3. 状态仪器(Badge/Mark)

- 徽章 `.badge`:高 22px,内边 2px 8px,11px 等宽字,方形。
  - 通过:`--ark-signal-dim` 底 + 墨字 + 前置 SVG ✓
  - 未通过:`--ark-ink` 底 + 纸白字 + 前置 SVG ×
  - 进行中/警告:`--ark-warn` 底墨字 / 或 1px 墨线空心 + ▸
- SVG 内联(14×14,stroke 1.5px,currentColor):✓=polyline,×=两 line,▸=polygon。定义一次 `<symbol>` 复用。
- 进度/百分比值一律 `--font-mono` tabular-nums。

## 4. 各页改编

### 01 环境检测 PREFLIGHT
- 页首:`01 / 环境检测 PREFLIGHT`
- 清单:每行一条 1px `--ark-line-soft` 底线,行高 52px:左=中文项名(15px)+ 英文微标签(如 DOCKER DAEMON);右=状态徽章 + 值(等宽);未通过行行尾放操作按钮组。
- 全部通过:清单容器右上角盖一条 45° 斜切 `--ark-signal-dim` 水印条(clip-path 平行四边形,内印"环境就绪 READY"墨字 11px)——替代原绿色横幅。顶部横幅区域改为这条斜切带,置于清单上方右对齐。
- "一键启动 Docker"= `.btn-primary`;复制命令按钮 = `.btn`;命令文本显示在 1px 墨框等宽小面板内。

### 02 镜像列表 IMAGES
- 页首 `02 / 镜像列表 IMAGES` + 右侧同一基线的搜索框(宽 280px)与刷新按钮。
- 表格:表头 `--ark-paper-dim` 底、11px 大写微标签、letter-spacing .12em;行高 44px,1px 分隔线;仓库名列 15px 墨粗,tag 用 `--font-mono`;大小/时间等宽数字;悬停整行 `--ark-ink` 反白纸字(装饰性 hover,不承载状态);行尾"部署"= `.btn-primary` 小号。
- `<none>`:rgba 墨 .35 斜体 mono。
- 计数行:表格上方右对齐,"共 X / 过滤后 Y"(mono)。

### 03 服务器管理 SERVERS
- 页首 `03 / 服务器管理 SERVERS` + 右侧"新增服务器""新增项目"按钮。
- 服务器卡 = dossier 技术面板:纸底,顶部 2px 墨上边框,左上角 `SRV-01` 编号(mono 11px)+ 名称 18px 粗;右上操作按钮组(文字按钮)。面板主体 host:port/用户名/认证/remote_dir 用两列键值对(键=11px 大写微标签 rgba 墨 .5,值=mono 14px)。底部检测结果区:1px 顶线分隔,徽章行 + errors 红改墨(未通过信息用 `--ark-ink` 底白字小徽章逐条列)。
- hostOk=false 黄横幅:`--ark-warn` 底墨字窄条,左缘 4px 墨楔形。
- 项目区同 dossier 语法,编号 `PRJ-01`;文件映射表同镜像表格语法。
- server-log 终端区:墨黑面板(全页唯一深色块)——`--ark-ink` 底、青色时间戳、纸白正文、11px mono,左缘 4px 青条;折叠开关为文字按钮"+ 日志 / − 日志"。

### 04 部署向导 DEPLOY
- 页首 `04 / 部署向导 DEPLOY`。
- 选择区:四个控件横排一条 action strip,下衬 1px 墨线;"开始部署"为 `.btn-primary` 大号(48px 高),右侧"取消部署"次级。
- **五步索引化校准仪**(核心):横向五节点,节点间 1px 墨线连接;每节点 = 大编号(28px mono:01…05)+ 中文步名 + 英文微标签(TAG/PACK/UPLOAD/SYNC/APPLY);状态:完成=编号压青色实心方块(墨字)+ ✓;进行中=青色 2px 描边方块 + 呼吸脉冲(box-shadow 扩散 1.8s 循环;reduced-motion 静止);未来=1px 墨线空心。节点下 1px 基线整体延伸到右缘。
- 日志终端:同 03 的墨黑面板规格,高 320px,右下角行计数。
- 横幅:成功=斜切青水印条(同 01);失败=墨底纸白横条左缘青楔;取消=`--ark-warn` 底墨字条。

## 5. 动效

- 切页:当前 section `clip-path inset` 从右向左 masked reveal 300ms ease-out(页面切换时对新 section 应用一次)。
- 部署进行中节点 pulse(1.8s);其余仅交互反馈 180-220ms。
- `@media (prefers-reduced-motion: reduce)`:全部动画/过渡归零,pulse 静态化。
- 禁止:氛围循环、视差、漂浮。

## 6. 图标/装饰禁令

- 删除全部 emoji 图标(现有 🐳📦🖥🚀 等)。
- 不新增任何图标字体/图片;仅允许第 3 节三个内联 SVG symbol 与纯 CSS 几何(楔形/斜切/规则线/编号)。
- 不做:六边形、扫描线、故障噪声、霓虹渐变、玻璃拟态、圆角卡片。

## 7. 工程约束(必须遵守)

1. **不改任何 JS 业务逻辑**:invoke 命令名/参数、事件监听(deploy-progress/deploy-log/deploy-done/server-log/pagechange)、AppState/AppBus/toast/copyText 逻辑、LOCKED_PAGES、localStorage key 全部保持。改的只是 DOM 结构/类名/样式与 createElement 的节点构造细节。
2. **js 里 document.getElementById/querySelector 引用的 id 必须逐一保留**(完成前逐一核对清单,报告里列对照表)。类名可改但需同步 JS 与 CSS。
3. 无 alert/confirm/prompt;全中文文案(微标签英文除外);防 XSS 约定不变(createElement/textContent)。
4. toast 改为墨底纸白条、左缘青楔、右下角(逻辑不动)。
5. 完成后 `node --check` 全部 js;对照 id 清单;报告列出改动文件与自检结果。
6. 布局需在 1024px 宽下可用(内容区可横向滚动表格,不塌陷);dock 在 <900px 时转为顶部横向黑条(响应式重排,不是缩放)。
