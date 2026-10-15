// S06 告警事件清單（設計包 kairos/screens.md S06；API GET /api/v1/alerts、/alerts/summary、POST /alerts/ack、SSE alert.*）。
//
// - 檢視（進行中／需要我處理／已恢復待結案／全部）與篩選寫在網址；排序固定（嚴重度 → 未確認優先 → 開始時間，伺服器端）。
// - 即時更新：已載入的列就地更新（不重排），新事件以「有 N 筆事件變更 · 更新列表」提示；清單原本是空的時直接重新載入。
// - 規劃提醒與紀錄只在「全部」，列上不顯示勾選框與「確認」（alerts-ux.md 2.3）。
// - 0.7.0 隱藏（decisions/INTEGRATION.md §28.6）：FilterBar「群組」「負責人」（沒有後端）；單鍵快速鍵只有 j／k
//   （S23 沒有關閉快速鍵的開關，不提供會改變狀態的單鍵 a）。

import { h, icon, mount, announce, isTyping } from '../core/dom.js';
import { t, severityLabel } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import * as fmt from '../core/format.js';
import { replaceQuery, navigate } from '../core/router.js';
import { pageHead, emptyState, errorState, inlineAlert, field, button, setBusy } from '../ui/components.js';
import { createTable } from '../ui/table.js';
import { toast } from '../ui/toast.js';
import { short } from '../core/errors.js';
import {
  SEVERITIES, STATES, CATEGORIES, sevBadge, stateText, attentionTag, flags, needsAction, durationSecs,
  engineNotice, unavailableNotice, ackDialog, assignDialog, resolveDialog, who,
} from './alerts-common.js';

const VIEWS = ['active', 'needs_me', 'recovered', 'all'];
const RANGES = { '': null, '24h': 86400, '7d': 7 * 86400, '30d': 30 * 86400 };
const PAGE = 100;

function parseState(q) {
  const list = (k, allowed) => (q.get(k) || '').split(',').map((s) => s.trim()).filter((s) => s && (!allowed || allowed.includes(s)));
  const view = q.get('view') || 'active';
  return {
    view: VIEWS.includes(view) ? view : 'active',
    severity: list('severity', SEVERITIES),
    state: list('state', STATES),
    category: list('category', CATEGORIES),
    rule: list('rule'),
    range: Object.prototype.hasOwnProperty.call(RANGES, q.get('range') || '') ? q.get('range') || '' : '',
    q: q.get('q') || '',
  };
}

export function render(ctx) {
  const root = ctx.root;
  const st = parseState(ctx.query);
  const canWrite = session.can('alerts:write');
  let disposed = false;
  let ctrl = null;
  let nextCursor = null;
  let total = 0;
  let counts = null;
  let rules = null;
  const rows = new Map();
  const selected = new Set();
  const changed = new Set();
  const expanded = new Set();
  let summaryTimer = null;

  const dataTime = h('span', { class: 'k-data-time' });
  const notices = h('div', { class: 'k-stack k-stack--sm' });
  const tabs = h('nav', { class: 'ds-tabs', 'aria-label': t('alerts.events.views') });
  const bulk = h('div');
  const changeBar = h('div', { class: 'ds-table__summary', hidden: true });

  // ---- 篩選 ----
  const selects = h('div', { class: 'k-filter-selects' });
  const search = h('input', { class: 'ds-input', type: 'search', name: 'q', value: st.q, autocomplete: 'off', spellcheck: 'false', placeholder: t('alerts.events.searchPlaceholder') });
  const searchField = field({ label: t('alerts.events.search'), input: search });
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

  function select(name, label, options, value, onChange) {
    const sel = h(
      'select',
      { class: 'ds-select', name, on: { change: () => onChange(sel.value) } },
      options.map(([v, text]) => h('option', { value: v, selected: v === value }, text)),
    );
    return h('label', { class: 'k-labelled-select' }, label, sel);
  }

  function renderSelects() {
    const bySev = (counts && counts.active_by_severity) || {};
    const sevValue = st.severity.join(',');
    const sevOpts = [['', t('hosts.filter.all')], ...SEVERITIES.map((s) => [s, counts ? t('alerts.events.sevOption', { s: severityLabel(s), n: fmt.num(bySev[s] || 0) }) : severityLabel(s)])];
    if (sevValue && !SEVERITIES.includes(sevValue)) sevOpts.push([sevValue, st.severity.map(severityLabel).join('、')]);
    const stateValue = st.state.join(',');
    const stateOpts = [['', t('hosts.filter.all')], ...STATES.map((s) => [s, t(`alerts.state.${s}`)])];
    if (stateValue && !STATES.includes(stateValue)) stateOpts.push([stateValue, st.state.map((s) => t(`alerts.state.${s}`)).join('、')]);
    const catValue = st.category.join(',');
    const catOpts = [['', t('hosts.filter.all')], ...CATEGORIES.map((c) => [c, t(`alerts.category.${c}`)])];
    if (catValue && !CATEGORIES.includes(catValue)) catOpts.push([catValue, st.category.map((c) => t(`alerts.category.${c}`)).join('、')]);
    const ruleValue = st.rule.join(',');
    mount(
      selects,
      select('severity', t('alerts.col.severity'), sevOpts, sevValue, (v) => { st.severity = v ? v.split(',') : []; reload(); }),
      select('state', t('alerts.col.state'), stateOpts, stateValue, (v) => { st.state = v ? v.split(',') : []; reload(); }),
      select('category', t('alerts.events.category'), catOpts, catValue, (v) => { st.category = v ? v.split(',') : []; reload(); }),
      rules
        ? select(
            'rule',
            t('alerts.events.rule'),
            [['', t('hosts.filter.all')], ...rules.map((r) => [r.rule_id, r.name]), ...(ruleValue && !rules.some((r) => r.rule_id === ruleValue) ? [[ruleValue, ruleValue]] : [])],
            ruleValue,
            (v) => { st.rule = v ? [v] : []; reload(); },
          )
        : null,
      select('range', t('alerts.events.range', { tz: fmt.tzLabel() }), [['', t('alerts.range.all')], ['24h', t('alerts.range.24h')], ['7d', t('alerts.range.7d')], ['30d', t('alerts.range.30d')]], st.range, (v) => { st.range = v; reload(); }),
      hasFilters() ? h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', on: { click: clearFilters } }, t('hosts.clear')) : null,
    );
  }

  function hasFilters() {
    return Boolean(st.severity.length || st.state.length || st.category.length || st.rule.length || st.range || st.q);
  }
  function clearFilters() {
    st.severity = [];
    st.state = [];
    st.category = [];
    st.rule = [];
    st.range = '';
    st.q = '';
    search.value = '';
    reload();
  }

  function renderTabs() {
    const n = (v) => {
      if (!counts) return null;
      const map = { active: counts.active, needs_me: counts.needs_me, recovered: counts.recovered };
      return map[v] == null ? null : h('span', { class: 'num' }, fmt.num(map[v]));
    };
    mount(
      tabs,
      VIEWS.map((v) =>
        h(
          'a',
          {
            href: `/alerts/events${qs({ view: v === 'active' ? null : v })}`,
            'aria-current': v === st.view ? 'page' : null,
            on: {
              click: (ev) => {
                ev.preventDefault();
                st.view = v;
                reload();
                renderTabs();
                const a = tabs.querySelector('[aria-current="page"]');
                if (a) a.focus();
              },
            },
          },
          t(`alerts.view.${v}`),
          n(v),
        ),
      ),
    );
  }

  // ---- 表格 ----
  function ackButton(e) {
    if (!canWrite) return null;
    if (!needsAction(e)) return e.state !== 'resolved' ? h('span', { class: 'caption' }, t('alerts.noAck')) : null;
    if (e.acked || e.state === 'resolved') return null;
    return button({
      text: t('alerts.ack.short'),
      size: 'sm',
      attrs: { 'aria-label': t('alerts.ack.aria', { title: e.title }) },
      onClick: async () => {
        if (await ackDialog(e)) refreshRow(e.event_id);
      },
    });
  }
  function checkbox(e) {
    if (!needsAction(e) || e.state === 'resolved') return null;
    const cb = h('input', { type: 'checkbox', class: 'ds-check', checked: selected.has(e.event_id), 'aria-label': t('alerts.select', { title: e.title }) });
    cb.addEventListener('change', () => {
      if (cb.checked) selected.add(e.event_id);
      else selected.delete(e.event_id);
      renderBulk();
    });
    return cb;
  }
  function eventCell(e) {
    const isParent = e.category === 'aggregate' || e.category === 'system';
    const toggle = isParent && !e.parent_id
      ? h(
          'button',
          {
            type: 'button',
            class: 'ds-button ds-button--icon ds-button--sm',
            'aria-expanded': expanded.has(e.event_id) ? 'true' : 'false',
            'aria-label': t(expanded.has(e.event_id) ? 'alerts.collapse' : 'alerts.expand', { title: e.title }),
            title: t(expanded.has(e.event_id) ? 'alerts.collapse' : 'alerts.expand', { title: e.title }),
            on: { click: () => toggleChildren(e) },
          },
          icon(expanded.has(e.event_id) ? 'chevron-down' : 'chevron-right'),
        )
      : null;
    return h(
      'div',
      { class: 'k-event-cell' },
      toggle,
      h(
        'span',
        null,
        h('a', { href: `/alerts/events/${encodeURIComponent(e.event_id)}`, class: 'k-event-link' }, e.title),
        ' ',
        h('span', { class: 'caption mono' }, e.event_id),
        e.code ? [' ', h('code', { class: 'k-code-tag' }, e.code)] : null,
        ' ',
        attentionTag(e),
        flags(e),
      ),
    );
  }
  const cols = [
    canWrite ? { key: 'sel', label: h('span', { class: 'ds-visually-hidden' }, t('alerts.col.select')), render: checkbox } : null,
    { key: 'severity', label: t('alerts.col.severity'), render: (e) => sevBadge(e.severity) },
    { key: 'state', label: t('alerts.col.state'), render: stateText },
    { key: 'title', label: t('alerts.col.event'), cls: 'k-cell-wrap', render: eventCell },
    { key: 'subject', label: t('alerts.col.subject'), render: (e) => (e.subject ? h('span', { class: 'mono' }, e.subject) : '—') },
    { key: 'started', label: t('alerts.col.started', { tz: fmt.tzLabel() }), render: (e) => fmt.timeEl(e.started_at) },
    { key: 'duration', label: t('alerts.col.duration'), num: true, render: (e) => fmt.duration(durationSecs(e)) },
    { key: 'assignee', label: t('alerts.col.assignee'), render: (e) => who(e.assignee) || '—' },
    canWrite ? { key: 'act', label: h('span', { class: 'ds-visually-hidden' }, t('alerts.col.actions')), render: ackButton } : null,
  ].filter(Boolean);
  const table = createTable({
    caption: t('alerts.events.caption'),
    columns: cols,
    rowKey: (e) => e.event_id,
    unit: t('unit.events'),
    onLoadMore: () => loadMore(),
  });
  // 緊急事件列首 4px 色條（未解決）
  const origUpdate = table.updateRow.bind(table);
  function decorate(e) {
    const tr = table.el.querySelector(`tr[data-key="${CSS.escape(e.event_id)}"]`);
    if (tr) tr.classList.toggle('is-emergency', e.severity === 'emergency' && e.state !== 'resolved');
  }

  async function toggleChildren(e) {
    const id = e.event_id;
    for (const c of table.el.querySelectorAll(`tr[data-parent="${CSS.escape(id)}"]`)) c.remove();
    if (expanded.has(id)) {
      expanded.delete(id);
      origUpdate(e);
      decorate(e);
      focusToggle(id);
      return;
    }
    expanded.add(id);
    origUpdate(e);
    decorate(e);
    focusToggle(id);
    const span = cols.length;
    const loading = h('tr', { class: 'is-child', dataset: { parent: id } }, h('td', { colspan: span }, t('common.loading')));
    const row = table.el.querySelector(`tr[data-key="${CSS.escape(id)}"]`);
    if (row) row.after(loading);
    try {
      const d = await api(`/api/v1/alerts/${encodeURIComponent(id)}`);
      const kids = (d.children && d.children.items) || [];
      const totalKids = (d.children && d.children.total) || 0;
      const nodes = kids.slice(0, 5).map((c) =>
        h(
          'tr',
          { class: 'is-child', dataset: { parent: id } },
          h('td', { colspan: span }, sevBadge(c.severity), ' ', stateText(c), ' ', h('a', { href: `/alerts/events/${encodeURIComponent(c.event_id)}` }, c.title), ' ', h('span', { class: 'caption' }, t('alerts.absorbed'))),
        ),
      );
      if (!totalKids) nodes.push(h('tr', { class: 'is-child', dataset: { parent: id } }, h('td', { colspan: span }, t('alerts.noChildren'))));
      if (totalKids > 5) nodes.push(h('tr', { class: 'is-child', dataset: { parent: id } }, h('td', { colspan: span }, h('a', { href: `/alerts/events/${encodeURIComponent(id)}` }, t('alerts.showAllChildren', { n: fmt.num(totalKids) })))));
      loading.replaceWith(...nodes);
    } catch (err) {
      loading.replaceWith(h('tr', { class: 'is-child', dataset: { parent: id } }, h('td', { colspan: span }, short(err))));
    }
  }
  function focusToggle(id) {
    const b = table.el.querySelector(`tr[data-key="${CSS.escape(id)}"] button[aria-expanded]`);
    if (b) b.focus();
  }

  // ---- 批次 ----
  function renderBulk() {
    if (!canWrite || !selected.size) {
      mount(bulk);
      return;
    }
    const evs = [...selected].map((id) => rows.get(id)).filter(Boolean);
    const ackBtn = button({
      text: t('alerts.ack.short'),
      size: 'sm',
      onClick: async () => {
        setBusy(ackBtn, true, t('common.working'));
        try {
          const r = await api('/api/v1/alerts/ack', { method: 'POST', body: { event_ids: [...selected] } });
          const s = r.summary || {};
          toast({ kind: s.failed ? 'warning' : 'success', title: t('alerts.batch.acked', { n: fmt.num(s.acknowledged || 0) }), desc: [r.message, s.failed ? t('alerts.batch.someFailed', { n: s.failed }) : null].filter(Boolean).join(' ') || null });
          selected.clear();
          reload();
        } catch (err) {
          setBusy(ackBtn, false, true);
          toast({ kind: 'error', title: short(err, { title: t('alerts.ack.failed') }) });
        }
      },
    });
    mount(
      bulk,
      h(
        'div',
        { class: 'ds-table__bulkbar', role: 'region', 'aria-label': t('alerts.batch.region') },
        h('strong', null, t('alerts.batch.selected', { n: fmt.num(selected.size) })),
        h('span', { class: 'k-sep' }),
        ackBtn,
        button({ text: t('alerts.assign.menu'), size: 'sm', onClick: async () => { if (await assignDialog(evs)) { selected.clear(); reload(); } } }),
        button({ text: t('alerts.resolve.menu'), size: 'sm', onClick: async () => { if (await resolveDialog(evs)) { selected.clear(); reload(); } } }),
        session.can('silences:write')
          ? button({
              text: t('alerts.silence.fromEvents'),
              size: 'sm',
              iconName: 'bell-off',
              onClick: () => {
                const hosts = [...new Set(evs.map((e) => e.agent_id).filter(Boolean))];
                const ruleIds = [...new Set(evs.map((e) => e.rule_id))];
                navigate(`/alerts/silences/new${qs({ type: 'silence', hosts, rules: ruleIds, event: evs.length === 1 ? evs[0].event_id : null })}`);
              },
            })
          : null,
        h('span', { class: 'k-sep' }),
        button({ text: t('alerts.batch.clear'), variant: 'ghost', size: 'sm', onClick: () => { selected.clear(); reload(); } }),
      ),
    );
  }

  function renderChangeBar() {
    if (!changed.size) {
      changeBar.hidden = true;
      return;
    }
    changeBar.hidden = false;
    mount(
      changeBar,
      h('span', { role: 'status' }, t('alerts.events.changed', { n: fmt.num(changed.size) })),
      button({ text: t('hosts.refreshList'), iconName: 'refresh', size: 'sm', onClick: () => reload() }),
    );
  }

  function apiParams(cursor) {
    const secs = RANGES[st.range];
    return {
      view: st.view,
      severity: st.severity,
      state: st.state,
      category: st.category,
      rule: st.rule,
      q: st.q,
      from: secs ? new Date(fmt.serverNow() - secs * 1000).toISOString() : null,
      limit: PAGE,
      cursor,
    };
  }
  function urlParams() {
    return {
      view: st.view === 'active' ? null : st.view,
      severity: st.severity,
      state: st.state,
      category: st.category,
      rule: st.rule,
      range: st.range,
      q: st.q,
    };
  }

  mount(
    root,
    pageHead({
      title: t('alerts.events.title'),
      meta: dataTime,
      actions: [
        session.can('silences:read') ? h('a', { class: 'ds-button ds-button--secondary', href: '/alerts/silences' }, icon('bell-off'), t('nav.silences')) : null,
        session.can('alert-rules:read') ? h('a', { class: 'ds-button ds-button--secondary', href: '/alerts/rules' }, t('alerts.events.rulesLink')) : null,
      ].filter(Boolean),
    }),
    notices,
    tabs,
    h('section', { class: 'ds-filterbar', 'aria-label': t('alerts.events.filters') }, h('div', { class: 'ds-filterbar__row' }, searchField), selects),
    changeBar,
    bulk,
    table.el,
    h('p', { class: 'caption k-keys-hint' }, t('alerts.events.keys')),
  );

  async function loadNotices() {
    const parts = [];
    let summary = null;
    try {
      summary = await api('/api/v1/alerts/summary');
    } catch (err) {
      if (err.code === 'alerts_unavailable') parts.push(unavailableNotice());
    }
    if (summary) {
      counts = summary.counts;
      const en = engineNotice(summary.engine);
      if (en) parts.push(en);
      if (summary.silences && summary.silences.active > 0) {
        parts.push(
          inlineAlert({
            kind: 'info',
            body: h(
              'p',
              null,
              t('alerts.events.silenced', { n: fmt.num(summary.silences.active), hosts: fmt.num(summary.silences.hosts) }),
              ' ',
              h('a', { href: '/alerts/silences' }, t('alerts.events.viewSilences')),
            ),
          }),
        );
      }
    }
    // 沒有通知管道：告警只會出現在主控台（S11 空狀態；連結只對能新增管道的角色）
    if (session.can('channels:read')) {
      try {
        const ch = await api('/api/v1/channels');
        if (!ch.length) {
          parts.push(
            inlineAlert({
              kind: 'warning',
              body: h('p', null, t('alerts.events.noChannels'), session.can('channels:write') ? [' ', h('a', { href: '/alerts/channels' }, t('alerts.events.addChannel'))] : null),
            }),
          );
        }
      } catch {
        /* 管道清單失敗不影響事件清單 */
      }
    }
    if (disposed) return;
    mount(notices, ...parts);
    renderTabs();
    renderSelects();
  }

  async function loadRules() {
    if (!session.can('alert-rules:read')) return;
    try {
      rules = await api('/api/v1/alert-rules');
      if (!disposed) renderSelects();
    } catch {
      rules = null;
    }
  }

  async function reload() {
    replaceQuery(urlParams());
    renderTabs();
    renderSelects();
    changed.clear();
    renderChangeBar();
    rows.clear();
    selected.clear();
    expanded.clear();
    renderBulk();
    nextCursor = null;
    if (ctrl) ctrl.abort();
    ctrl = new AbortController();
    const signal = ctrl.signal;
    table.setLoading();
    try {
      const res = await api(`/api/v1/alerts${qs(apiParams())}`, { raw: true, signal });
      if (disposed) return;
      total = res.page.total;
      nextCursor = res.page.next_cursor;
      if (res.meta && res.meta.counts) counts = res.meta.counts;
      mount(dataTime, t('common.dataTime', { time: fmt.time(fmt.serverNow()) }));
      renderTabs();
      renderSelects();
      for (const e of res.data) rows.set(e.event_id, e);
      if (!res.data.length) table.setEmpty(emptyFor());
      else {
        table.setRows(res.data, { total, hasMore: Boolean(nextCursor) });
        res.data.forEach(decorate);
      }
      announce(t('alerts.events.loaded', { n: fmt.num(total) }));
    } catch (err) {
      if (err.name === 'AbortError' || disposed) return;
      if (err.code === 'alerts_unavailable') {
        table.setEmpty(errorState(err, { title: t('alerts.events.failed') }));
        mount(notices, unavailableNotice());
        return;
      }
      if (err.code === 'invalid_parameter' && err.details && err.details.parameter === 'q') {
        searchField.setError(err.message);
      }
      table.setError(err, reload, t('alerts.events.failed'));
    }
  }

  function emptyFor() {
    if (!hasFilters()) {
      if (st.view === 'all') return emptyState({ title: t('alerts.events.emptyAll'), iconName: 'success' });
      return emptyState({ title: t(`alerts.events.empty.${st.view}`), body: t('alerts.events.emptyBody'), iconName: 'success' });
    }
    return emptyState({
      title: t('alerts.events.noMatch'),
      iconName: 'filter',
      actions: [h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: clearFilters } }, t('hosts.clear'))],
    });
  }

  async function loadMore() {
    if (!nextCursor) return;
    table.setLoadingMore();
    try {
      const res = await api(`/api/v1/alerts${qs(apiParams(nextCursor))}`, { raw: true });
      if (disposed) return;
      nextCursor = res.page.next_cursor;
      total = res.page.total;
      for (const e of res.data) rows.set(e.event_id, e);
      table.appendRows(res.data, { total, hasMore: Boolean(nextCursor) });
      res.data.forEach(decorate);
    } catch (err) {
      if (!disposed) table.setError(err, loadMore, t('alerts.events.failed'));
    }
  }

  async function refreshRow(id) {
    try {
      const e = await api(`/api/v1/alerts/${encodeURIComponent(id)}`);
      if (disposed) return;
      applyEvent(e);
      scheduleSummary();
    } catch {
      /* 列保持原樣 */
    }
  }

  function applyEvent(data) {
    const old = rows.get(data.event_id);
    if (!old) return false;
    const merged = { ...old, ...data };
    rows.set(data.event_id, merged);
    if (!(merged.attention === 'action' && merged.state !== 'resolved')) selected.delete(merged.event_id);
    const focusedInRow = document.activeElement && document.activeElement.closest && document.activeElement.closest(`tr[data-key="${CSS.escape(data.event_id)}"]`);
    table.updateRow(merged);
    decorate(merged);
    if (focusedInRow) {
      const a = table.el.querySelector(`tr[data-key="${CSS.escape(data.event_id)}"] .k-event-link`);
      if (a) a.focus();
    }
    renderBulk();
    return true;
  }

  function scheduleSummary() {
    if (summaryTimer) return;
    summaryTimer = setTimeout(async () => {
      summaryTimer = null;
      try {
        const s = await api('/api/v1/alerts/summary');
        if (disposed) return;
        counts = s.counts;
        renderTabs();
        renderSelects();
      } catch {
        /* 計數保持原樣 */
      }
    }, 2000);
  }

  // ---- 即時更新 ----
  const unsub = sse.subscribe((ev) => {
    if (ev.type === 'resync') {
      changed.add('*');
      renderChangeBar();
      return;
    }
    if (!ev.type.startsWith('alert.') || !ev.data || !ev.data.event_id) return;
    scheduleSummary();
    if (applyEvent(ev.data)) return;
    if (ev.data.parent_id) return;
    // 清單是空的：直接重新載入（沒有正在檢視的列會被打亂）。事件可能比資料庫的寫入早到，
    // 稍等再抓；仍沒有這筆時再試一次
    if (table.count === 0 && ctrl) {
      emptyCatchUp(ev.data.event_id, 2);
      return;
    }
    changed.add(ev.data.event_id);
    renderChangeBar();
  });

  let catchUpTimer = null;
  function emptyCatchUp(id, tries) {
    clearTimeout(catchUpTimer);
    catchUpTimer = setTimeout(async () => {
      if (disposed || table.count > 0) return;
      await reload();
      if (!disposed && !rows.has(id) && tries > 1) emptyCatchUp(id, tries - 1);
    }, 1500);
  }

  // j／k：上下移動到事件連結
  function onKey(ev) {
    if (ev.ctrlKey || ev.metaKey || ev.altKey || isTyping(ev.target) || document.querySelector('dialog[open]')) return;
    if (ev.key !== 'j' && ev.key !== 'k') return;
    const links = [...table.el.querySelectorAll('tbody .k-event-link')];
    if (!links.length) return;
    ev.preventDefault();
    const i = links.indexOf(document.activeElement);
    const next = ev.key === 'j' ? Math.min(links.length - 1, i + 1) : Math.max(0, i - 1);
    links[next].focus();
  }
  document.addEventListener('keydown', onKey);

  renderTabs();
  renderSelects();
  loadNotices();
  loadRules();
  reload();
  return {
    dispose() {
      disposed = true;
      unsub();
      clearTimeout(searchTimer);
      clearTimeout(summaryTimer);
      clearTimeout(catchUpTimer);
      document.removeEventListener('keydown', onKey);
      if (ctrl) ctrl.abort();
    },
  };
}
