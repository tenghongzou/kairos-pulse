// 全域搜尋（Ctrl+K；設計包 ia.md §2「全域搜尋」）。
//
// 以「搜尋提供者」擴充：每個提供者宣告需要的權限與 search(q, signal) → [{ group, text, sub, href, badge }]。
// 0.7.0 只有主機（主機名稱、agent_id 前綴、IP、CIDR；GET /api/v1/hosts?q=）；告警事件、使用者、憑證序號
// 在對應的畫面完成時以 registerProvider() 加入（kairos/web/README.md「全域搜尋」）。
//
// 無障礙：combobox（input）＋ listbox；↑↓ 移動、Enter 開啟、Esc 關閉。

import { h, icon, uid, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import { navigate } from '../core/router.js';
import { openDialog } from './dialog.js';
import { statusBadge } from './components.js';
import { short } from '../core/errors.js';

const providers = [];

export function registerProvider(p) {
  providers.push(p);
}

// 主機：與主機清單相同的 q（API.md 第 5.1 節）
registerProvider({
  id: 'hosts',
  group: () => t('search.group.hosts'),
  allowed: () => session.canAny('hosts:read'),
  async search(q, signal) {
    const rows = await api(`/api/v1/hosts${qs({ q, limit: 8, sort: 'hostname' })}`, { signal });
    return rows.map((r) => ({
      text: r.display_name || r.hostname,
      sub: [r.agent_id.slice(0, 8), r.ip].filter(Boolean).join(' · '),
      href: `/hosts/${r.agent_id}`,
      badge: statusBadge(r.status),
    }));
  },
  more: (q) => ({ text: t('search.moreHosts', { q }), href: `/hosts${qs({ q })}` }),
});

/** 開啟搜尋對話框 */
export function openSearch(initial = '') {
  const listId = uid('sr');
  const inputEl = h('input', {
    class: 'ds-input ds-input--lg',
    type: 'search',
    role: 'combobox',
    'aria-expanded': 'false',
    'aria-controls': listId,
    'aria-autocomplete': 'list',
    'aria-label': t('search.label'),
    placeholder: t('search.placeholder'),
    autocomplete: 'off',
    spellcheck: 'false',
    value: initial,
  });
  const list = h('div', { class: 'k-search-results', role: 'listbox', id: listId, 'aria-label': t('search.results') });
  const status = h('p', { class: 'k-search-status', role: 'status' }, t('search.hint'));
  let options = [];
  let activeIndex = -1;
  let ctrl = null;
  let timer = null;

  const dlg = openDialog({
    title: t('search.title'),
    size: 'lg',
    body: h('div', null, h('div', { class: 'k-search-box' }, icon('search'), inputEl), status, list),
    actions: [],
    initialFocus: inputEl,
  });

  function setActive(i) {
    options.forEach((o, j) => o.el.setAttribute('aria-selected', j === i ? 'true' : 'false'));
    activeIndex = i;
    if (i >= 0 && options[i]) {
      inputEl.setAttribute('aria-activedescendant', options[i].el.id);
      options[i].el.scrollIntoView({ block: 'nearest' });
    } else inputEl.removeAttribute('aria-activedescendant');
  }

  function go(href) {
    dlg.close();
    navigate(href);
  }

  async function run() {
    const q = inputEl.value.trim();
    if (ctrl) ctrl.abort();
    mount(list);
    for (const e of dlg.el.querySelectorAll('.k-search-err')) e.remove();
    options = [];
    setActive(-1);
    if (q.length < 2) {
      status.textContent = t('search.hint');
      inputEl.setAttribute('aria-expanded', 'false');
      return;
    }
    ctrl = new AbortController();
    status.textContent = t('search.searching');
    const active = providers.filter((p) => p.allowed());
    const results = await Promise.all(
      active.map((p) =>
        p.search(q, ctrl.signal).then(
          (items) => ({ p, items }),
          (err) => ({ p, items: [], err }),
        ),
      ),
    ).catch(() => null);
    if (!results) return;
    let n = 0;
    for (const { p, items, err } of results) {
      if (err && err.name === 'AbortError') return;
      const groupId = uid('srg');
      const groupItems = [];
      if (err) {
        status.after(h('p', { class: 'k-search-status k-search-err' }, `${p.group()}：${short(err)}`));
        continue;
      }
      for (const it of items) {
        const id = uid('sro');
        const el = h(
          'div',
          { role: 'option', id, 'aria-selected': 'false', on: { click: () => go(it.href) } },
          it.badge || null,
          h('span', { class: 'k-sr-main' }, it.text, it.sub ? h('span', { class: 'k-sub' }, it.sub) : null),
        );
        options.push({ el, href: it.href });
        groupItems.push(el);
        n += 1;
      }
      if (p.more && items.length) {
        const m = p.more(q);
        const id = uid('sro');
        const el = h('div', { role: 'option', id, 'aria-selected': 'false', on: { click: () => go(m.href) } }, icon('search'), h('span', { class: 'k-sr-main' }, m.text));
        options.push({ el, href: m.href });
        groupItems.push(el);
      }
      if (groupItems.length) {
        list.appendChild(
          h('div', { role: 'group', 'aria-labelledby': groupId }, h('div', { class: 'k-group-label', id: groupId }, p.group()), groupItems),
        );
      }
    }
    status.textContent = n ? t('search.count', { n }) : t('search.none', { q });
    inputEl.setAttribute('aria-expanded', options.length ? 'true' : 'false');
    setActive(options.length ? 0 : -1);
  }

  inputEl.addEventListener('input', () => {
    clearTimeout(timer);
    timer = setTimeout(run, 200);
  });
  inputEl.addEventListener('keydown', (ev) => {
    if (ev.key === 'ArrowDown' && options.length) {
      ev.preventDefault();
      setActive((activeIndex + 1) % options.length);
    } else if (ev.key === 'ArrowUp' && options.length) {
      ev.preventDefault();
      setActive((activeIndex - 1 + options.length) % options.length);
    } else if (ev.key === 'Enter') {
      ev.preventDefault();
      if (activeIndex >= 0 && options[activeIndex]) go(options[activeIndex].href);
      else if (inputEl.value.trim()) go(`/hosts${qs({ q: inputEl.value.trim() })}`);
    }
  });
  if (initial) run();
}
