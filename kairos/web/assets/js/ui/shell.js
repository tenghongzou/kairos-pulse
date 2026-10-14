// 全域框架（設計包 ia.md §2）：頂列（Logo、全域搜尋、系統狀態燈、使用者選單）、全域橫幅、側邊導覽、主要內容。
// 側邊導覽依目前的權限顯示（nav.js）；權限改變時（session.onChange）重畫。

import { h, icon, isTyping, mount } from '../core/dom.js';
import { t, healthItemText } from '../core/strings.js';
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

function navVisible(item) {
  if (item.perm && !session.can(item.perm)) return false;
  if (item.permAny && !session.canAny(item.permAny)) return false;
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
    return;
  }
  try {
    const hdata = await api('/api/v1/system/health');
    const bad = (hdata.items || []).filter((i) => i.status !== 'ok');
    const status = hdata.status === 'critical' ? 'critical' : hdata.status === 'warning' ? 'warning' : 'online';
    const text = bad.length ? t('shell.healthIssues', { n: bad.length, first: healthItemText(bad[0]) }) : t('shell.healthOk');
    mount(light, icon(status === 'online' ? 'status-online' : `status-${status}`), h('span', { class: 'k-status-light__text' }, t('shell.healthPrefix'), text));
    light.className = `k-status-light k-fg--${status}`;
    light.title = text;
    light.hidden = false;
  } catch {
    light.hidden = true;
  }
}

/** 連線中斷的橫幅（screens.md §0.2「資料過時」） */
export function setOffline(offline) {
  if (!shell) return;
  const region = shell.banner;
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
  const light = h('span', { class: 'k-status-light', hidden: true });
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
  const banner = h('div', { id: 'banner-region' });
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
  shell = { root, nav, content, banner, burger, light, userBtn };
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
