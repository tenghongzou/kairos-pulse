// Kairos 主控台的進入點：路由表、登入狀態、外框與畫面的掛載（架構見 kairos/web/README.md）。

import { t } from './core/strings.js';
import { h, mount } from './core/dom.js';
import * as prefs from './core/prefs.js';
import * as fmt from './core/format.js';
import * as session from './core/session.js';
import * as sse from './core/sse.js';
import { api, setHooks } from './core/api.js';
import { setRoutes, resolve, start, navigate, query, setLeaveGuard } from './core/router.js';
import { mountShell, unmountShell, refreshShell, setOffline, getNavCount, onNavCount } from './ui/shell.js';
import { confirm } from './ui/dialog.js';
import { closeMenu } from './ui/menu.js';
import { forbidden, notFound } from './screens/errors.js';
import { errorState } from './ui/components.js';
import './screens/alerts-boot.js';
import { ADMIN_ROUTES } from './routes-admin.js';

/**
 * 路由表。欄位：
 *   path      `/hosts/:id/:tab?`
 *   load      動態載入畫面模組（匯出 render(ctx)）
 *   title     strings.js 的 key（分頁標題「Kairos｜…」）
 *   public    不需要登入（登入頁）；bare：不顯示外框
 *   perm      需要的全站權限；permAny：全站或群組範圍（ia.md §4.2；沒有時顯示 403）
 *   session   只限以帳號登入的工作階段（個人設定）
 */
const ROUTES = [
  { path: '/login', public: true, bare: true, title: 'login.docTitle', load: () => import('./screens/login.js') },
  { path: '/', permAny: 'hosts:read', title: 'overview.title', load: () => import('./screens/overview.js') },
  { path: '/hosts', permAny: 'hosts:read', title: 'hosts.title', load: () => import('./screens/hosts.js') },
  { path: '/hosts/:id/:tab?', permAny: 'hosts:read', title: 'nav.hosts', load: () => import('./screens/host-detail.js') },
  { path: '/account', session: true, title: 'account.title', load: () => import('./screens/account.js') },
  // 告警（S06～S11）
  { path: '/alerts', perm: 'alerts:read', title: 'alerts.events.title', load: () => import('./screens/alerts-events.js') },
  { path: '/alerts/events', perm: 'alerts:read', title: 'alerts.events.title', load: () => import('./screens/alerts-events.js') },
  { path: '/alerts/events/:id', perm: 'alerts:read', title: 'alerts.events.title', load: () => import('./screens/alerts-event.js') },
  { path: '/alerts/rules', perm: 'alert-rules:read', title: 'alerts.rules.title', load: () => import('./screens/alerts-rules.js') },
  { path: '/alerts/rules/:id', perm: 'alert-rules:read', title: 'alerts.rules.title', load: () => import('./screens/alerts-rule-edit.js') },
  { path: '/alerts/silences/:id?', perm: 'silences:read', title: 'alerts.sl.title', load: () => import('./screens/alerts-silences.js') },
  { path: '/alerts/channels/:id?', perm: 'channels:read', title: 'alerts.ch.title', load: () => import('./screens/alerts-channels.js') },
  // 註冊與憑證（S12～S15）
  { path: '/enrollment/pending', perm: 'enroll:read', title: 'enroll.title', load: () => import('./screens/enroll-pending.js') },
  { path: '/enrollment/tokens/:id?', perm: 'enroll:read', title: 'token.title', load: () => import('./screens/enroll-tokens.js') },
  { path: '/certificates/ca', perm: 'pki:read', title: 'ca.title', load: () => import('./screens/pki-ca.js') },
  { path: '/certificates/:serial?', perm: 'pki:read', title: 'cert.title', load: () => import('./screens/pki-certs.js') },
];
// 稽核、管理、系統（S16～S18、S20、S21）：routes-admin.js（allow：需要任一權限時）
ROUTES.push(...ADMIN_ROUTES);

const appRoot = document.getElementById('app');
let current = null;
let renderSeq = 0;
let painted = false;

function currentPath() {
  return location.pathname + location.search;
}

// 分頁標題：「(3) Kairos｜總覽」，括號內與側邊導覽「告警事件」的計數泡泡相同（ia.md §7）
let docTitle = '';
function applyDocTitle() {
  const c = getNavCount('alerts');
  const base = docTitle ? `Kairos｜${docTitle}` : 'Kairos';
  document.title = c && c.n ? `(${c.n > 99 ? '99+' : c.n}) ${base}` : base;
}
function setDocTitle(text) {
  docTitle = text || '';
  applyDocTitle();
}
onNavCount((key) => {
  if (key === 'alerts') applyDocTitle();
});

// 未儲存的變更：畫面的 render() 回傳 isDirty() 時，站內換頁先詢問（router 的 setLeaveGuard），
// 關閉或重新整理分頁時由瀏覽器詢問（beforeunload）
function currentDirty() {
  const inst = current && current.instance;
  return Boolean(inst && typeof inst.isDirty === 'function' && inst.isDirty());
}
setLeaveGuard({
  dirty: currentDirty,
  ask: () => confirm({ title: t('app.leaveTitle'), body: t('app.leaveBody'), confirmText: t('app.leaveConfirm'), danger: true }),
});
window.addEventListener('beforeunload', (ev) => {
  if (currentDirty()) {
    ev.preventDefault();
    ev.returnValue = '';
  }
});

function focusTitle() {
  const el = document.getElementById('page-title');
  if (el) el.focus({ preventScroll: false });
}

const app = {
  /** 登入完成（login.js） */
  loggedIn() {
    sse.setEnabled(true);
    resetIdle();
  },
  /** 登出：結束這個工作階段（伺服器端）並回登入頁 */
  async logout(reason = 'logged_out') {
    try {
      await api('/api/v1/auth/logout', { method: 'POST', auth: false });
    } catch {
      /* 已經失效也沒關係 */
    }
    session.clear();
    sse.setEnabled(false);
    navigate(`/login?reason=${reason}`, { replace: true });
  },
  notFound() {
    if (current && current.root) notFound(current.root);
    setDocTitle(t('error.404.title'));
    focusTitle();
  },
};

function dispose() {
  closeMenu();
  for (const d of document.querySelectorAll('dialog[open]')) d.remove();
  if (current && current.instance && current.instance.dispose) {
    try {
      current.instance.dispose();
    } catch (e) {
      console.error(e);
    }
  }
  current = null;
}

async function onRoute() {
  const seq = ++renderSeq;
  // 第一次繪製時不移動焦點（讓第一個 Tab 是「跳到主要內容」）；之後換頁時焦點移到頁面標題
  const focus = painted ? focusTitle : () => {};
  dispose();
  const { route, params } = resolve();
  // 不需要登入的頁面
  if (route && route.public) {
    unmountShell();
    const mod = await route.load();
    if (seq !== renderSeq) return;
    current = { root: appRoot };
    current.instance = mod.render({ root: appRoot, params, query: query(), app, setTitle: setDocTitle, focusTitle: focus });
    return;
  }
  if (!session.isLoggedIn()) {
    navigate(`/login?next=${encodeURIComponent(currentPath())}`, { replace: true });
    return;
  }
  if (session.mustChangePassword()) {
    navigate(`/login?next=${encodeURIComponent(currentPath())}`, { replace: true });
    return;
  }
  const content = mountShell(appRoot, { onLogout: () => app.logout() });
  current = { root: content };
  if (!route) {
    notFound(content);
    setDocTitle(t('error.404.title'));
    focus();
    return;
  }
  if ((route.perm && !session.can(route.perm)) || (route.permAny && !session.canAny(route.permAny)) || (route.session && !session.isSession()) || (route.allow && !route.allow())) {
    forbidden(content, { perm: route.perm || route.permAny });
    setDocTitle(t('error.403.title'));
    focus();
    return;
  }
  setDocTitle(t(route.title));
  let mod;
  try {
    mod = await route.load();
  } catch (e) {
    mount(content, errorState(e, { title: t('app.loadFailed'), onRetry: () => location.reload() }));
    return;
  }
  if (seq !== renderSeq) return;
  current.instance = mod.render({ root: content, params, query: query(), app, setTitle: setDocTitle, focusTitle: focus });
  focus();
}

// ---- 閒置逾時 ----
// 總覽與清單會定期自動更新，伺服器把這些請求也當成活動；為了讓「閒置 N 分鐘後登出」（accounts.session_idle_minutes）
// 仍然成立，瀏覽器端另外追蹤使用者的操作：超過工作階段的閒置時間沒有操作就結束工作階段並回登入頁。
let lastInteraction = Date.now();
function resetIdle() {
  lastInteraction = Date.now();
}
for (const ev of ['pointerdown', 'keydown', 'wheel', 'touchstart']) {
  window.addEventListener(ev, resetIdle, { passive: true, capture: true });
}
setInterval(() => {
  const me = session.current();
  if (!me || me.type !== 'session' || !me.session) return;
  const idle = (me.session.idle_timeout_secs || 1800) * 1000;
  if (Date.now() - lastInteraction > idle) {
    lastInteraction = Date.now();
    app.logout('session_expired');
  }
}, 30000);

// ---- 全域處理 ----
setHooks({
  onUnauthorized() {
    if (location.pathname === '/login') return;
    session.clear();
    sse.setEnabled(false);
    navigate(`/login?next=${encodeURIComponent(currentPath())}&reason=session_expired`, { replace: true });
  },
  onPasswordChange() {
    if (location.pathname === '/login') return;
    navigate(`/login?next=${encodeURIComponent(currentPath())}`, { replace: true });
  },
  onConnectivity(ok) {
    setOffline(!ok);
  },
});

session.onChange((me) => {
  if (!me) return;
  refreshShell();
  // 權限改變：目前的畫面可能已不允許（或剛被允許）
  const { route } = resolve();
  if (route && !route.public && current && current.permSig !== JSON.stringify(me.permissions)) {
    onRoute();
  }
});

// 回到這個分頁時重新確認身分與權限（可能已在其他地方登出或被調整角色）
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible' && session.isLoggedIn()) {
    session.refresh().then((ok) => {
      if (!ok && location.pathname !== '/login') {
        sse.setEnabled(false);
        navigate(`/login?next=${encodeURIComponent(currentPath())}&reason=session_expired`, { replace: true });
      }
    });
  }
});

async function boot() {
  prefs.applyAppearance();
  prefs.onChange(() => prefs.applyAppearance());
  fmt.startRelativeTicker();
  setRoutes(ROUTES);
  try {
    await session.load();
  } catch (e) {
    mount(appRoot, h('main', { class: 'k-bare', id: 'main' }, errorState(e, { title: t('app.bootFailed'), onRetry: () => location.reload() })));
    return;
  }
  if (session.isLoggedIn()) sse.setEnabled(true);
  start(async () => {
    await onRoute();
    if (current && session.current()) current.permSig = JSON.stringify(session.current().permissions);
    painted = true;
  });
}

boot();
