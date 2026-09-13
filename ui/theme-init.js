// theme-init.js — 防闪白主题初始化(自 index.html 内联脚本挪出,CSP 兼容)
// 必须早于样式表生效(故 <link rel="stylesheet"> 之前以 <script src> 同步引入)——
// 恢复上次选择的亮暗主题(异常/未存储时回退 light)。
// dd_scheme = 'auto'(跟随系统,UPGRADE-PLAN 阶段四)时按系统偏好解析,
// 运行期实时跟随由 app.js 的 matchMedia change 监听接管。
(function () {
  var __ddScheme = null;
  try { __ddScheme = localStorage.getItem('dd_scheme'); } catch (e) { /* 忽略 */ }
  if (__ddScheme === 'dark' ||
      (__ddScheme === 'auto' && window.matchMedia &&
       window.matchMedia('(prefers-color-scheme: dark)').matches)) {
    document.documentElement.dataset.arkScheme = 'dark';
  } else {
    document.documentElement.dataset.arkScheme = 'light';
  }
})();
