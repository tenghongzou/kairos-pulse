/*
 * 在第一次繪製前套用個人偏好的主題與減少動態效果（避免閃爍）。
 * CSP 不允許 inline script，所以是獨立的一般 script（不是 module，同步執行）。
 * 偏好只存在這個瀏覽器（localStorage；kairos/web/README.md「偏好」）：被停用時沿用作業系統的設定。
 */
(function () {
  'use strict';
  try {
    var root = document.documentElement;
    var theme = window.localStorage.getItem('kairos.theme');
    if (theme === 'light' || theme === 'dark') root.setAttribute('data-theme', theme);
    if (window.localStorage.getItem('kairos.motion') === 'reduce') root.setAttribute('data-motion', 'reduce');
    if (window.localStorage.getItem('kairos.navCollapsed') === '1') root.classList.add('is-nav-collapsed');
  } catch (e) {
    /* localStorage 不可用：沿用作業系統的主題 */
  }
})();
