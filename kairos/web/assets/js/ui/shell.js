// 全域框架（設計包 ia.md §2）：頂列（Logo、全域搜尋、系統狀態燈、使用者選單）、全域橫幅、側邊導覽、主要內容。
// 側邊導覽依目前的權限顯示（nav.js）；權限改變時（session.onChange）重畫。
//
// 系統健康（GET /api/v1/system/health，每 60 秒）同時供給三處（ia.md §2、§7）：
// - 頂列系統狀態燈：整體狀態與最重要一項的摘要；
// - 側邊導覽「系統健康」的計數泡泡：狀態為警告或嚴重的項目數（有嚴重時用嚴重樣式）；
// - 全站 Banner（design-system README §6.13）：只用於「全體主機受影響」的項目——CRL 與伺服器憑證即將到期或已過期、
//   回報持續無法寫入、以未驗證的方式接受 agent 連線、告警引擎沒有在執行（通知送不出去）。嚴重不可關閉；
//   警告可「24 小時內不再顯示」（只記在這個瀏覽器，同一個問題升級為嚴重時照常顯示）。

import { h, icon, isTyping, mount } from '../core/dom.js';
import { t, healthItemText } from '../core/strings.js';
import { codeTag } from './components.js';
import * as session from '../core/session.js';
import * as prefs from '../core/prefs.js';
import { api, isOnline } from '../core/api.js';
import * as fmt from '../core/format.js';
import { NAV } from './nav.js';
import { openMenu } from './menu.js';
import { openSearch } from './search.js';

let shell = null;
let healthTimer = null;
let lastOkAt = null;
// 側邊導覽的計數泡泡（ia.md §7）：nav.js 項目的 count 鍵 → { n, tone, label }（由各功能設定，例如告警）
const navCounts = new Map();

const navCountListeners = new Set();

/** 設定導覽項目的計數泡泡；value 為 null 時不顯示 */
export function setNavCount(key, value) {
  if (value) navCounts.set(key, value);
  else navCounts.delete(key);
  renderNav();
  for (const fn of navCountListeners) fn(key);
}

/** 目前的計數（{ n, tone, label } 或 undefined） */
export function getNavCount(key) {
  return navCounts.get(key);
}

/** 計數改變時通知（分頁標題的未確認數；ia.md §7） */
export function onNavCount(fn) {
  navCountListeners.add(fn);
  return () => navCountListeners.delete(fn);
}

function countBubble(key) {
  const c = key && navCounts.get(key);
  if (!c || !c.n) return null;
  return [
    h('span', { class: ['ds-sidenav__count', `k-count--${c.tone || 'critical'}`], 'aria-hidden': 'true', dataset: { count: key } }, c.n > 99 ? '99+' : String(c.n)),
    h('span', { class: 'ds-visually-hidden' }, `，${c.label}`),
  ];
}

// ---- 全站 Banner（系統層級，ia.md §2） ----
const DISMISS_MS = 24 * 3600 * 1000;

function dismissedUntil(key) {
  const v = Number(prefs.get(`banner.${key}`) || 0);
  return Number.isFinite(v) ? v : 0;
}

/** 健康項目 → 全站 Banner（{ key, level, title, body, code, href, linkText }）；只列影響全部主機的項目 */
export function systemBanners(items) {
  const out = [];
  const byId = new Map((items || []).map((i) => [i.id, i]));
  const tls = byId.get('tls');
  if (tls && tls.status !== 'ok') {
    const list = (tls.values && tls.values.expiries) || [];
    for (const kind of ['crl', 'server_certificate']) {
      const bad = list.filter((e) => e.kind === kind && e.status !== 'ok').sort((a, b) => a.days_left - b.days_left)[0];
      if (!bad) continue;
      const days = Math.max(0, Math.floor(bad.days_left));
      const expired = bad.days_left <= 0;
      const level = bad.status === 'critical' ? 'critical' : 'warning';
      if (kind === 'crl') {
        out.push({
          key: `crl.${expired ? 'expired' : level}`,
          level,
          title: expired ? t('banner.crlExpired') : t('banner.crlSoon', { n: days }),
          body: t('banner.crlBody'),
          code: level === 'critical' ? 'KRS-E1203' : 'KRS-W1201',
          href: session.can('pki:read') ? '/certificates/ca' : '/system/health',
          linkText: session.can('pki:read') ? t('banner.toCa') : t('banner.toHealth'),
        });
      } else {
        out.push({
          key: `cert.${expired ? 'expired' : level}`,
          level,
          title: expired ? t('banner.certExpired') : t('banner.certSoon', { n: days }),
          body: t('banner.certBody'),
          code: 'KRS-W1001',
          href: '/system/health',
          linkText: t('banner.toHealth'),
        });
      }
    }
  }
  const writes = byId.get('report_writes');
  if (writes && writes.status === 'critical') {
    out.push({ key: 'writes', level: 'critical', title: t('banner.writes'), body: t('banner.writesBody'), code: 'KRS-E4101', href: '/system/health', linkText: t('banner.toHealth') });
  }
  const auth = byId.get('auth');
  if (auth && auth.status === 'critical') {
    out.push({ key: 'unauth', level: 'critical', title: t('banner.unauth'), body: t('banner.unauthBody'), code: 'KRS-W0302', href: '/system/health', linkText: t('banner.toHealth') });
  }
  const alerts = byId.get('alerts');
  if (alerts && alerts.status === 'critical') {
    out.push({ key: 'alerts', level: 'critical', title: t('banner.alerts'), body: t('banner.alertsBody'), code: null, href: '/system/health', linkText: t('banner.toHealth') });
  }
  return out;
}

function renderSystemBanners(items) {
  if (!shell) return;
  const now = Date.now();
  const list = systemBanners(items).filter((b) => b.level === 'critical' || dismissedUntil(b.key) <= now);
  mount(
    shell.sysBanner,
    list.map((b) => {
      const dismiss =
        b.level === 'warning'
          ? h(
              'button',
              {
                type: 'button',
                class: 'ds-button ds-button--ghost ds-button--sm',
                on: {
                  click: () => {
                    prefs.set(`banner.${b.key}`, String(Date.now() + DISMISS_MS));
                    renderSystemBanners(items);
                    const title = document.getElementById('page-title');
                    if (title) title.focus();
                  },
                },
              },
              t('banner.dismiss'),
            )
          : null;
      return h(
        'div',
        { class: ['ds-banner', `ds-banner--${b.level}`], role: b.level === 'critical' ? 'alert' : 'status', dataset: { banner: b.key } },
        icon(`status-${b.level}`),
        h('div', { class: 'ds-banner__body' }, h('p', { class: 'strong' }, b.title, b.code ? [' ', codeTag(b.code)] : null), b.body ? h('p', null, b.body) : null),
        h('div', { class: 'ds-banner__actions' }, h('a', { class: 'ds-button ds-button--secondary ds-button--sm', href: b.href }, b.linkText), dismiss),
      );
    }),
  );
}

function navVisible(item) {
  if (item.perm && !session.can(item.perm)) return false;
  if (item.permAny && !session.canAny(item.permAny)) return false;
  if (item.allow && !item.allow()) return false;
  return true;
}

function renderNav() {
  if (!shell) return;
  const path = location.pathname;
  const sections = NAV.map((sec) => {
    const items = sec.items.filter(navVisible);
    if (!items.length) return null;
    const headId = `nav-${sec.section.replace(/\W/g, '-')}`;
    return h(
      'div',
      { class: 'ds-sidenav__section' },
      h('p', { class: 'ds-sidenav__heading', id: headId }, t(sec.section)),
      h(
        'ul',
        { 'aria-labelledby': headId },
        items.map((it) =>
          h(
            'li',
            null,
            h(
              'a',
              {
                class: 'ds-sidenav__link',
                href: it.path,
                'aria-current': it.match(path) ? 'page' : null,
                title: t(it.label),
                on: { click: () => setNavOpen(false) },
              },
              icon(it.icon),
              h('span', { class: 'ds-sidenav__label' }, t(it.label)),
              countBubble(it.count),
            ),
          ),
        ),
      ),
    );
  }).filter(Boolean);
  const collapsed = document.documentElement.classList.contains('is-nav-collapsed');
  const toggle = h(
    'button',
    {
      type: 'button',
      class: 'ds-button ds-button--ghost ds-button--sm ds-sidenav__toggle',
      'aria-expanded': collapsed ? 'false' : 'true',
      'aria-controls': 'sidenav',
      on: { click: toggleCollapsed },
    },
    icon(collapsed ? 'sidebar-expand' : 'sidebar-collapse'),
    h('span', { class: 'ds-sidenav__label' }, collapsed ? t('nav.expand') : t('nav.collapse')),
  );
  mount(shell.nav, ...sections, h('div', { class: 'ds-sidenav__footer' }, toggle));
}

function toggleCollapsed() {
  const root = document.documentElement;
  const v = !root.classList.contains('is-nav-collapsed');
  root.classList.toggle('is-nav-collapsed', v);
  prefs.set('navCollapsed', v ? '1' : '0');
  renderNav();
  const btn = shell.nav.querySelector('.ds-sidenav__toggle');
  if (btn) btn.focus();
}

function setNavOpen(open) {
  if (!shell) return;
  document.documentElement.classList.toggle('is-nav-open', open);
  shell.burger.setAttribute('aria-expanded', open ? 'true' : 'false');
}

function renderUser() {
  const name = session.displayName();
  mount(shell.userBtn, icon('user'), h('span', { class: 'k-user-name k-hide-sm' }, name), icon('chevron-down'));
  shell.userBtn.setAttribute('aria-label', t('shell.userMenu', { name }));
}

async function refreshHealth() {
  if (!shell) return;
  const light = shell.light;
  if (!session.can('health:read')) {
    light.hidden = true;
    setNavCount('health', null);
    mount(shell.sysBanner);
    return;
  }
  try {
    const hdata = await api('/api/v1/system/health');
    if (!shell) return;
    // 嚴重的項目排在前面：狀態燈的摘要寫最重要的一項（ia.md §2）
    const bad = (hdata.items || []).filter((i) => i.status !== 'ok').sort((a, b) => (a.status === 'critical' ? 0 : 1) - (b.status === 'critical' ? 0 : 1));
    const status = hdata.status === 'critical' ? 'critical' : hdata.status === 'warning' ? 'warning' : 'online';
    const first = bad[0] && (bad[0].restricted || !bad[0].summary ? healthItemText({ ...bad[0], summary: t(`adm.health.${bad[0].status}`) }) : healthItemText(bad[0]));
    const text = !bad.length ? t('shell.healthOk') : bad.length === 1 ? t('shell.healthIssues', { first }) : t('shell.healthIssuesMore', { first, m: bad.length - 1 });
    mount(light, icon(status === 'online' ? 'status-online' : `status-${status}`), h('span', { class: 'k-status-light__text' }, t('shell.healthPrefix'), text));
    light.className = `k-status-light k-fg--${status}`;
    light.title = text;
    light.hidden = false;
    setNavCount('health', bad.length ? { n: bad.length, tone: bad.some((i) => i.status === 'critical') ? 'critical' : 'warning', label: t('nav.badge.health', { n: bad.length }) } : null);
    renderSystemBanners(hdata.items || []);
  } catch {
    light.hidden = true;
    setNavCount('health', null);
  }
}


/** 連線中斷的橫幅（screens.md §0.2「資料過時」） */
export function setOffline(offline) {
  if (!shell) return;
  const region = shell.offlineBanner;
  if (!offline) {
    lastOkAt = fmt.serverNow();
    mount(region);
    for (const el of document.querySelectorAll('.k-data-time')) el.classList.remove('is-stale');
    return;
  }
  const at = lastOkAt ? fmt.time(lastOkAt) : '—';
  mount(region, 
    h(
      'div',
      { class: 'ds-banner ds-banner--warning', role: 'status' },
      icon('status-warning'),
      h('div', { class: 'ds-banner__body' }, t('shell.offline', { time: at })),
    ),
  );
  for (const el of document.querySelectorAll('.k-data-time')) el.classList.add('is-stale');
}

function onKey(ev) {
  if ((ev.ctrlKey || ev.metaKey) && (ev.key === 'k' || ev.key === 'K')) {
    ev.preventDefault();
    openSearch();
  } else if (ev.key === '/' && !isTyping(ev.target) && !document.querySelector('dialog[open]')) {
    ev.preventDefault();
    openSearch();
  } else if (ev.key === 'Escape' && document.documentElement.classList.contains('is-nav-open')) {
    setNavOpen(false);
    shell.burger.focus();
  }
}

/**
 * 掛上外框；回傳內容區（畫面 render 到這裡）。已掛上時只更新導覽的目前位置。
 * @param {Function} onLogout
 */
export function mountShell(root, { onLogout }) {
  if (shell && shell.root === root && root.contains(shell.content)) {
    renderNav();
    return shell.content;
  }
  const burger = h(
    'button',
    {
      type: 'button',
      class: 'ds-button ds-button--icon k-nav-burger',
      'aria-label': t('shell.menu'),
      title: t('shell.menu'),
      'aria-expanded': 'false',
      'aria-controls': 'sidenav',
      on: { click: () => setNavOpen(!document.documentElement.classList.contains('is-nav-open')) },
    },
    icon('menu'),
  );
  const searchBtn = h(
    'button',
    { type: 'button', class: 'k-search-trigger', title: t('shell.searchLabel'), on: { click: () => openSearch() } },
    icon('search'),
    h('span', { class: 'k-label' }, t('shell.searchPlaceholder')),
    h('kbd', null, 'Ctrl K'),
  );
  // 系統狀態燈：點選到系統健康（ia.md §2）
  const light = h('a', { class: 'k-status-light', href: '/system/health', hidden: true });
  const userBtn = h('button', { type: 'button', class: 'ds-button ds-button--ghost k-user-btn', id: 'user-menu-btn', 'aria-haspopup': 'menu', 'aria-expanded': 'false' });
  userBtn.addEventListener('click', () =>
    openMenu(
      userBtn,
      [
        session.isSession() ? { text: t('shell.account'), iconName: 'settings', href: '/account' } : null,
        { text: t('shell.logout'), iconName: 'logout', onSelect: onLogout },
      ].filter(Boolean),
      { label: t('shell.userMenuShort') },
    ),
  );
  const nav = h('nav', { class: 'ds-sidenav', id: 'sidenav', 'aria-label': t('shell.mainNav') });
  const content = h('div', { class: 'k-content', id: 'content' });
  // 全站 Banner：系統層級（健康 API）在上、連線中斷在下（各自更新，互不覆蓋）
  const sysBanner = h('div', { class: 'k-banner-stack' });
  const offlineBanner = h('div', { class: 'k-banner-stack' });
  const banner = h('div', { id: 'banner-region' }, sysBanner, offlineBanner);
  mount(root, 
    h('a', { class: 'k-skip', href: '#main', on: { click: (ev) => { ev.preventDefault(); const t1 = document.getElementById('page-title'); if (t1) t1.focus(); } } }, t('shell.skip')),
    h(
      'header',
      { class: 'k-topbar' },
      burger,
      h('a', { class: 'k-brand', href: '/', title: 'Kairos' }, h('span', { class: 'k-brand__mark', 'aria-hidden': 'true' }, 'K'), 'Kairos'),
      h('span', { class: 'k-topbar__spacer' }),
      searchBtn,
      light,
      userBtn,
    ),
    banner,
    h('div', { class: 'k-shell' }, nav, h('div', { class: 'k-nav-scrim', on: { click: () => setNavOpen(false) } }), h('main', { class: 'k-main', id: 'main' }, content)),
  );
  shell = { root, nav, content, banner, sysBanner, offlineBanner, burger, light, userBtn };
  renderNav();
  renderUser();
  refreshHealth();
  if (!healthTimer) healthTimer = setInterval(refreshHealth, 60000);
  if (!isOnline()) setOffline(true);
  if (!mountShell.keyBound) {
    document.addEventListener('keydown', onKey);
    mountShell.keyBound = true;
  }
  return content;
}

/** 權限或使用者改變：重畫導覽與使用者選單 */
export function refreshShell() {
  if (!shell) return;
  renderNav();
  renderUser();
  refreshHealth();
}

/** 離開外框（登入頁） */
export function unmountShell() {
  shell = null;
  if (healthTimer) {
    clearInterval(healthTimer);
    healthTimer = null;
  }
  document.documentElement.classList.remove('is-nav-open');
}

export function isMounted() {
  return Boolean(shell);
}
