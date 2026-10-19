// S04 主機清單（設計包 kairos/screens.md S04；API GET /api/v1/hosts、/hosts/facets、SSE /api/v1/events）。
//
// - 篩選、排序、分頁都在伺服器端；條件寫在網址（可分享、可加書籤；ia.md §3.2）。
// - 搜尋：主機名稱片段、agent_id 前綴（4 碼以上）、IP、CIDR（10.30.3.0/24）。CIDR 的主機位元不為 0 時
//   API 回 400 並建議正確的寫法，顯示在搜尋欄下。
// - 即時更新：已載入的列的狀態欄就地更新（不重排），頂部顯示「有 N 台狀態變化 · 更新列表」（§6.3）。
// - 0.7.0 隱藏（沒有後端，decisions/INTEGRATION.md §28.6 的原則）：批次操作與勾選框（主機的寫入操作）、
//   匯出 CSV、「新增主機」（S12 尚無畫面）、已儲存的檢視、憑證與生命週期篩選；
//   API 為 null 的狀態（嚴重、警告、待核准）不顯示狀態 chip。
// - 0.8.0〔草稿〕（F4，設計包 §30 D49）：預設顯示「磁碟」（使用率最高的掛載點）；可選欄位（CPU 型號、邏輯處理器、
//   記憶體總量、廠牌與型號、運作時間）由「欄位設定」開啟，只存在這個瀏覽器；「作業系統」篩選依家族
//   （os_family）。序號不提供欄位（D47）。

import { h, icon, announce, mount } from '../core/dom.js';
import * as prefs from '../core/prefs.js';
import { openDialog } from '../ui/dialog.js';
import { t, statusLabel } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as sse from '../core/sse.js';
import * as fmt from '../core/format.js';
import { replaceQuery } from '../core/router.js';
import { pageHead, emptyState, field } from '../ui/components.js';
import { createTable } from '../ui/table.js';
import { hostNameCell, statusCell, osText, memRatio, offlineFor, reasonFacet, isOldPulse, F4_VERSION } from './host-common.js';

const STATUSES = ['critical', 'offline', 'warning', 'unknown', 'pending', 'online'];
const SELECTS = [
  { key: 'offline_reason', label: 'hosts.filter.reason', facet: 'offline_reason', fmt: (v) => reasonFacet(v) },
  { key: 'group', label: 'hosts.filter.group', facet: 'group' },
  { key: 'tag', label: 'hosts.filter.tag', facet: 'tag' },
  { key: 'os_family', label: 'hosts.filter.osFamily', facet: 'os_family', fmt: (v) => t(`hosts.osFamily.${v}`) },
  { key: 'version', label: 'hosts.filter.version', facet: 'version' },
];
const PAGE = 100;

function parseState(q) {
  const list = (k) => (q.get(k) || '').split(',').map((s) => s.trim()).filter(Boolean);
  const sortRaw = q.get('sort') || 'status';
  return {
    q: q.get('q') || '',
    status: list('status').filter((s) => STATUSES.includes(s)),
    offline_reason: q.get('offline_reason') || '',
    group: q.get('group') || '',
    tag: q.get('tag') || '',
    os_family: q.get('os_family') || '',
    version: q.get('version') || '',
    sort: { key: sortRaw.replace(/^-/, ''), desc: sortRaw.startsWith('-') },
  };
}

function apiParams(st, cursor) {
  return {
    q: st.q,
    status: st.status,
    offline_reason: st.offline_reason,
    group: st.group,
    tag: st.tag,
    os_family: st.os_family,
    version: st.version,
    sort: `${st.sort.desc ? '-' : ''}${st.sort.key}`,
    limit: PAGE,
    cursor,
  };
}

function urlParams(st) {
  const p = apiParams(st);
  delete p.limit;
  delete p.cursor;
  if (p.sort === 'status') delete p.sort;
  return p;
}

export function render(ctx) {
  const root = ctx.root;
  const st = parseState(ctx.query);
  let facets = null;
  let lastEventId = null;
  let nextCursor = null;
  let total = 0;
  let ctrl = null;
  let disposed = false;
  const rows = new Map();
  const changed = new Set();
  // 最近的 host.* 事件：重新抓取的列表可能比事件舊（索引最多 1 秒舊，API.md 第 9 節），
  // 編號比列表的 meta.last_event_id 新的事件要再套用一次（API.md 第 6 節「建議用法」）
  const recent = [];
  let catchUp = null;

  const totalMeta = h('span', null);
  // ---- 搜尋 ----
  const search = h('input', {
    class: 'ds-input',
    type: 'search',
    name: 'q',
    value: st.q,
    autocomplete: 'off',
    spellcheck: 'false',
    placeholder: t('hosts.searchPlaceholder'),
  });
  const searchField = field({ label: t('hosts.search'), input: search, hint: t('hosts.searchHint') });
  searchField.classList.add('ds-filterbar__search');
  let searchTimer = null;
  search.addEventListener('input', () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      st.q = search.value.trim();
      reload();
    }, 300);
  });
  search.addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter') {
      ev.preventDefault();
      clearTimeout(searchTimer);
      st.q = search.value.trim();
      reload();
    }
  });

  // ---- 狀態 chip ----
  const chipRow = h('div', { class: 'ds-filterbar__row', role: 'group', 'aria-label': t('hosts.filter.status') });
  function renderChips(counts) {
    mount(chipRow, 
      ...STATUSES.filter((s) => counts && counts[s] != null).map((s) => {
        const on = st.status.includes(s);
        const n = counts[s];
        return h(
          'button',
          {
            type: 'button',
            class: ['ds-filterbar__chip', n === 0 ? 'is-zero' : null],
            'aria-pressed': on ? 'true' : 'false',
            dataset: { status: s },
            on: {
              click: () => {
                st.status = on ? st.status.filter((x) => x !== s) : [...st.status, s];
                reload();
              },
            },
          },
          icon(s === 'online' ? 'status-online' : `status-${s}`),
          statusLabel(s),
          h('span', { class: 'k-n' }, fmt.num(n)),
        );
      }),
    );
  }

  // ---- 下拉選單 ----
  const selectsRow = h('div', { class: 'k-filter-selects' });
  function renderSelects() {
    mount(selectsRow, 
      ...SELECTS.map((s) => {
        const values = (facets && facets[s.facet] && facets[s.facet].values) || [];
        if (!values.length && !st[s.key]) return null;
        const sel = h(
          'select',
          {
            class: 'ds-select',
            name: s.key,
            on: {
              change: () => {
                st[s.key] = sel.value;
                reload();
              },
            },
          },
          h('option', { value: '' }, t('hosts.filter.all')),
          values.map((v) => h('option', { value: v.value, selected: st[s.key] === v.value }, `${s.fmt ? s.fmt(v.value) : v.value}（${fmt.num(v.count)}）`)),
          st[s.key] && !values.some((v) => v.value === st[s.key]) ? h('option', { value: st[s.key], selected: true }, st[s.key]) : null,
        );
        return h('label', { class: 'k-labelled-select' }, t(s.label), sel);
      }).filter(Boolean),
      h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', on: { click: clearFilters } }, t('hosts.clear')),
    );
  }

  // ---- 已套用 ----
  const applied = h('div', { class: 'ds-filterbar__applied', 'aria-live': 'polite' });
  function renderApplied() {
    const chips = [];
    const add = (label, onRemove) =>
      chips.push(
        h(
          'span',
          { class: 'ds-filterbar__applied-chip' },
          label,
          h('button', { type: 'button', 'aria-label': t('hosts.removeFilter', { f: label }), title: t('hosts.removeFilter', { f: label }), on: { click: onRemove } }, icon('close')),
        ),
      );
    if (st.q) add(`${t('hosts.search')}＝${st.q}`, () => { st.q = ''; search.value = ''; reload(); });
    if (st.status.length) add(`${t('hosts.filter.status')}＝${st.status.map(statusLabel).join('、')}`, () => { st.status = []; reload(); });
    for (const s of SELECTS) {
      if (st[s.key]) add(`${t(s.label)}＝${s.fmt ? s.fmt(st[s.key]) : st[s.key]}`, () => { st[s.key] = ''; reload(); });
    }
    mount(applied, ...(chips.length ? [h('span', null, t('hosts.applied')), ...chips] : []));
  }

  function hasFilters() {
    return Boolean(st.q || st.status.length || SELECTS.some((s) => st[s.key]));
  }
  function clearFilters() {
    st.q = '';
    search.value = '';
    st.status = [];
    for (const s of SELECTS) st[s.key] = '';
    reload();
  }

  // ---- 狀態變化提示 ----
  const changeBar = h('div', { class: 'ds-table__summary', hidden: true });
  function renderChangeBar() {
    if (!changed.size) {
      changeBar.hidden = true;
      return;
    }
    changeBar.hidden = false;
    mount(changeBar, 
      h('span', { role: 'status' }, t('hosts.changed', { n: fmt.num(changed.size) })),
      h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => reload() } }, icon('refresh'), t('hosts.refreshList')),
    );
  }

  // ---- 表格 ----
  // 資產欄位：舊版 Pulse（沒有資產）時「—」，title 寫完整的原因（設計包 S04）
  const na = (r) => (isOldPulse(r) ? h('span', { class: 'muted', title: t('host.na.oldPulse', { version: F4_VERSION }) }, '—') : '—');
  const inv = (r, k) => (r.inventory && r.inventory[k] != null ? r.inventory[k] : null);
  const diskCell = (r) => {
    const d = r.last_report && r.last_report.disk;
    if (!d || d.usage == null) return na(r);
    return [
      h('span', { title: t('col.diskTitle') }, `${d.mountpoint} ${fmt.pct(d.usage)}`),
      d.usage >= 95 ? h('span', { class: 'k-full' }, icon('status-critical'), t('host.values.diskFull')) : null,
      d.estimated ? h('span', { class: 'k-sub' }, t('host.estimated')) : null,
    ];
  };
  const OPTIONAL = [
    { code: 'm', key: 'cpu_model', label: t('col.cpuModel'), render: (r) => inv(r, 'cpu_model') || na(r) },
    { code: 'c', key: 'cores', label: t('col.cores'), num: true, render: (r) => (inv(r, 'cpu_logical_cores') != null ? fmt.num(inv(r, 'cpu_logical_cores')) : na(r)) },
    { code: 't', key: 'memory_total', label: t('col.memoryTotal'), num: true, render: (r) => (inv(r, 'memory_total') != null ? fmt.gib(inv(r, 'memory_total')) : na(r)) },
    { code: 'v', key: 'vendor_product', label: t('col.vendorProduct'), render: (r) => [inv(r, 'vendor'), inv(r, 'product')].filter(Boolean).join(' ') || na(r) },
    { code: 'u', key: 'uptime', label: t('col.uptime'), sort: 'uptime', num: true, render: (r) => (r.uptime_secs != null ? fmt.duration(r.uptime_secs) : r.status === 'online' ? na(r) : '—') },
  ];
  const chosen = () => new Set(String(prefs.get('hostColumns') || '').split(',').filter(Boolean));
  const baseCols = [
    { key: 'status', label: t('col.status'), sort: 'status', render: statusCell },
    { key: 'hostname', label: t('col.host'), sort: 'hostname', cls: 'k-cell-host', render: (r) => hostNameCell(r) },
    { key: 'group', label: t('col.group'), render: (r) => r.group || '—' },
    { key: 'ip', label: t('col.ip'), sort: 'ip', render: (r) => (r.ip ? h('span', { class: 'mono' }, r.ip) : '—') },
    { key: 'os', label: t('col.os'), render: (r) => osText(r.os) },
    { key: 'version', label: t('col.version'), sort: 'agent_version', render: (r) => r.agent_version || '—' },
    { key: 'last_seen', label: t('col.lastSeen', { tz: fmt.tzLabel() }), sort: 'last_seen', defaultDesc: true, render: (r) => fmt.timeEl(r.last_seen) },
    { key: 'offline_for', label: t('col.offlineFor'), render: offlineFor },
    { key: 'cpu', label: t('col.cpu'), sort: 'cpu', num: true, defaultDesc: true, render: (r) => (r.status === 'online' && r.last_report ? fmt.pct(r.last_report.cpu_usage) : '—') },
    { key: 'memory', label: t('col.memory'), sort: 'memory', num: true, defaultDesc: true, render: (r) => (r.status === 'online' ? fmt.pct(memRatio(r.last_report)) : '—') },
    { key: 'disk', label: t('col.disk'), sort: 'disk', defaultDesc: true, render: (r) => (r.status === 'online' ? diskCell(r) : '—') },
  ];
  const cols = [];
  const setColumns = () => {
    const on = chosen();
    cols.length = 0;
    cols.push(...baseCols, ...OPTIONAL.filter((c) => on.has(c.code)));
  };
  setColumns();
  function columnSettings() {
    const on = chosen();
    const boxes = OPTIONAL.map((c) => {
      const cb = h('input', { type: 'checkbox', class: 'ds-check', name: `col-${c.key}`, checked: on.has(c.code), dataset: { code: c.code } });
      return h('label', { class: 'ds-checkbox' }, cb, c.label);
    });
    const apply = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('hosts.columns.apply'));
    const d = openDialog({
      title: t('hosts.columns.title'),
      size: 'sm',
      body: [h('p', { class: 'secondary' }, t('hosts.columns.hint')), h('fieldset', { class: 'k-fieldset' }, h('legend', null, t('hosts.columns.optional')), boxes)],
      actions: [h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => d.close() } }, t('common.cancel')), apply],
    });
    apply.addEventListener('click', () => {
      const codes = boxes.map((b) => b.querySelector('input')).filter((i) => i.checked).map((i) => i.dataset.code);
      prefs.set('hostColumns', codes.join(','));
      d.close();
      setColumns();
      // 排序依據的欄位被隱藏時回到預設排序
      if (!cols.some((c) => c.sort === st.sort.key)) st.sort = { key: 'status', desc: false };
      announce(t('hosts.columns.applied'));
      reload();
    });
  }
  const table = createTable({
    caption: t('hosts.caption'),
    columns: cols,
    rowKey: (r) => r.agent_id,
    rowHref: (r) => `/hosts/${r.agent_id}`,
    sort: st.sort,
    unit: t('unit.hosts'),
    onSort: (key, desc) => {
      st.sort = { key, desc };
      reload();
    },
    onLoadMore: () => loadMore(),
  });

  mount(root, 
    pageHead({
      title: t('hosts.title'),
      meta: totalMeta,
      actions: h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: columnSettings } }, icon('settings'), t('hosts.columns.title')),
    }),
    h(
      'section',
      { class: 'ds-filterbar', 'aria-label': t('hosts.filters') },
      h('div', { class: 'ds-filterbar__row' }, searchField),
      chipRow,
      selectsRow,
      applied,
    ),
    changeBar,
    table.el,
  );

  async function loadFacets() {
    try {
      facets = await api('/api/v1/hosts/facets');
      totalMeta.textContent = t('hosts.total', { n: fmt.num(facets.total) });
    } catch {
      facets = null;
    }
    if (!disposed) renderSelects();
  }

  async function reload({ retry = false } = {}) {
    replaceQuery(urlParams(st));
    renderApplied();
    renderSelects();
    changed.clear();
    renderChangeBar();
    rows.clear();
    nextCursor = null;
    if (ctrl) ctrl.abort();
    ctrl = new AbortController();
    const signal = ctrl.signal;
    table.setSort(st.sort);
    table.setLoading();
    searchField.setError('');
    try {
      const res = await api(`/api/v1/hosts${qs(apiParams(st))}`, { raw: true, signal });
      if (disposed) return;
      lastEventId = res.meta && res.meta.last_event_id;
      total = res.page.total;
      nextCursor = res.page.next_cursor;
      renderChips(res.meta.status_counts);
      for (const r of res.data) rows.set(r.agent_id, r);
      if (!res.data.length) {
        table.setEmpty(emptyFor());
      } else {
        table.setRows(res.data, { total, hasMore: Boolean(nextCursor) });
      }
      const newer = recent.filter((ev) => sse.isNewer(ev.id, lastEventId));
      for (const ev of newer) apply(ev.data);
      // 列表比事件舊：狀態欄已就地更新，1.2 秒後再抓一次讓計數與篩選一致（只重試一次）
      if (newer.length && !retry) {
        clearTimeout(catchUp);
        catchUp = setTimeout(() => reload({ retry: true }), 1200);
      }
      announce(t('hosts.loaded', { n: fmt.num(total) }));
    } catch (err) {
      if (err.name === 'AbortError' || disposed) return;
      if (err.code === 'invalid_parameter' && err.details && ['q', 'subnet'].includes(err.details.parameter)) {
        searchField.setError(err.message);
        table.setEmpty(emptyState({ title: t('hosts.badSearch'), iconName: 'search' }));
        return;
      }
      table.setError(err, reload, t('hosts.failed'));
    }
  }

  function emptyFor() {
    if (!hasFilters()) {
      return emptyState({ title: t('overview.empty.title'), body: t('overview.empty.bodyNoPerm'), iconName: 'server' });
    }
    if (st.status.length === 1 && st.status[0] === 'offline' && !st.q && !SELECTS.some((s) => st[s.key])) {
      return emptyState({ title: t('hosts.noOffline'), iconName: 'success' });
    }
    return emptyState({
      title: t('hosts.noMatch'),
      iconName: 'filter',
      actions: [h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: clearFilters } }, t('hosts.clear'))],
    });
  }

  async function loadMore() {
    if (!nextCursor) return;
    table.setLoadingMore();
    try {
      const res = await api(`/api/v1/hosts${qs(apiParams(st, nextCursor))}`, { raw: true });
      if (disposed) return;
      nextCursor = res.page.next_cursor;
      total = res.page.total;
      for (const r of res.data) rows.set(r.agent_id, r);
      table.appendRows(res.data, { total, hasMore: Boolean(nextCursor) });
    } catch (err) {
      if (disposed) return;
      table.setError(err, loadMore, t('hosts.failed'));
    }
  }

  // ---- 即時更新 ----
  function apply(d) {
    const r = rows.get(d.agent_id);
    if (!r) return;
    r.status = d.status;
    r.status_detail = { ...(r.status_detail || {}), offline_reason: d.status === 'offline' ? d.reason || null : null, starting: false, never_connected: false };
    if (d.status === 'online') {
      r.offline_for_secs = null;
      r.last_seen = d.at;
    } else {
      r.offline_for_secs = 0;
    }
    if (d.ip) r.ip = d.ip;
    table.updateRow(r);
  }
  const unsub = sse.subscribe((ev) => {
    if (ev.type === 'resync') {
      // 重新抓取清單與計數（篩選與搜尋保留；screens.md §0.2）；完成後外框的 Banner 在 3 秒後移除
      Promise.all([reload(), loadFacets()]).finally(() => sse.resyncSettled());
      return;
    }
    if (!ev.type.startsWith('host.') || !ev.data) return;
    if (ev.id) {
      recent.push(ev);
      if (recent.length > 500) recent.shift();
    }
    if (lastEventId && ev.id && !sse.isNewer(ev.id, lastEventId)) return;
    changed.add(ev.data.agent_id);
    apply(ev.data);
    renderChangeBar();
  });

  renderApplied();
  loadFacets();
  reload();
  return {
    dispose() {
      disposed = true;
      unsub();
      clearTimeout(searchTimer);
      clearTimeout(catchUp);
      if (ctrl) ctrl.abort();
    },
  };
}

