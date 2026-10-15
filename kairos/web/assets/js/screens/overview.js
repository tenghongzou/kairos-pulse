// S03 總覽儀表板（設計包 kairos/screens.md S03；API GET /api/v1/overview、/hosts/facets、/alerts）。
//
// 0.7.0 呈現：A1 六個狀態 KpiTile、B 系統健康條、A2／C 進行中告警（有 alerts:read 時）、D 群組健康（有群組時）、
// E 離線最久、F 資源使用前 10。隱藏（沒有後端或沒有畫面）：範圍選擇器（?group=）、kiosk 模式、
// A2 的憑證磚（需要 S14）、區塊 G 最近變更（需要 S18）。事件列可行內「確認」（alerts:write）。
// 嚴重、警告的主機數在 API 為 null 時由進行中的告警推算（hostAlertCounts；沒有 alerts:read 時維持「—」）；
// 待核准 API 為 null，顯示「—」與說明，不顯示 0（S03「無法取得時不可顯示 0」）。

import { h, icon, mount } from '../core/dom.js';
import { t, statusLabel, severityLabel, healthItemText } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import * as fmt from '../core/format.js';
import { pageHead, card, kpiTile, kpiSkeleton, emptyState, errorState, statusBadge, skeletonBlock } from '../ui/components.js';
import { sparkline } from '../ui/chart.js';
import { hostNameCell, reasonText, offlineFor } from './host-common.js';
import { ackDialog, who } from './alerts-common.js';

const ORDER = ['critical', 'offline', 'warning', 'unknown', 'pending', 'online'];

function deltaText(n) {
  if (n == null) return t('overview.noDelta');
  if (n === 0) return t('overview.flat');
  return `${n > 0 ? '▲' : '▼'} ${fmt.num(Math.abs(n))} ${t('overview.vs1h')}`;
}

/**
 * 嚴重、警告的主機數（S03 A1；ia.md §5.1「線上，且有未解決的緊急或嚴重／警告等級告警」）。
 * 主機 API 尚未依告警判定這兩個狀態（ALERTS.md 第 10 節「未做」），由進行中的告警推算：
 * 有 agent_id、需要處理、未解決的事件，排除主機已離線的種類（主機離線、長期失聯、憑證被拒）。
 * 沒有 alerts:read 或告警引擎沒有啟動時回 null（磚維持「—」）。
 */
const OFFLINE_KINDS = ['host_offline', 'host_long_offline', 'cert_rejected'];
function hostAlertCounts(res) {
  if (!res || res.error || !Array.isArray(res.data)) return null;
  const crit = new Set();
  const warn = new Set();
  for (const e of res.data) {
    if (!e.agent_id || e.attention !== 'action' || OFFLINE_KINDS.includes(e.kind)) continue;
    if (e.severity === 'emergency' || e.severity === 'critical') crit.add(e.agent_id);
    else if (e.severity === 'warning') warn.add(e.agent_id);
  }
  for (const a of crit) warn.delete(a);
  return { critical: crit.size, warning: warn.size, partial: Boolean(res.page && res.page.next_cursor) };
}

function kpis(o, hostAlerts) {
  const counts = { ...(o.status_counts || {}) };
  if (hostAlerts) {
    if (counts.critical == null) counts.critical = hostAlerts.critical;
    if (counts.warning == null) counts.warning = hostAlerts.warning;
  }
  const change = o.change_1h || null;
  return h(
    'div',
    { class: 'ds-kpi-row', role: 'list', 'aria-label': t('overview.kpis') },
    ORDER.map((st) => {
      const n = counts[st];
      const label = statusLabel(st);
      let tile;
      if (n == null) {
        tile = kpiTile({ status: st, label, value: '—', sub: t('overview.unsupported'), tone: false });
      } else {
        const sub =
          st === 'online'
            ? t('overview.total', { n: fmt.num(o.total) })
            : n === 0
              ? t(`overview.none.${st}`)
              : null;
        const delta = st === 'online' || st === 'offline' ? deltaText(change ? change[st] : null) : null;
        const spark =
          st === 'online' && o.online_series && o.online_series.points && o.online_series.points.length > 1
            ? sparkline(
                o.online_series.points.map((p) => p.online),
                { width: 140, label: t('overview.onlineSpark') },
              )
            : null;
        const sortParam = st === 'offline' ? '-last_seen' : null;
        // 嚴重、警告由告警推算：連到告警事件（主機清單沒有這兩個狀態的篩選）
        const fromAlerts = hostAlerts && (st === 'critical' || st === 'warning') && (o.status_counts || {})[st] == null;
        tile = kpiTile({
          status: st,
          label,
          value: fmt.num(n) + (fromAlerts && hostAlerts.partial ? '+' : ''),
          unit: t('unit.hosts'),
          delta,
          sub: fromAlerts && n > 0 ? t(`overview.fromAlerts.${st}`) : sub,
          href: fromAlerts ? `/alerts/events${qs({ severity: st === 'critical' ? 'emergency,critical' : 'warning' })}` : `/hosts${qs({ status: st, sort: sortParam })}`,
          tone: n > 0,

          extra: spark,
        });
      }
      return h('div', { role: 'listitem' }, tile);
    }),
  );
}

function healthStrip(health) {
  if (!health) return null;
  const bad = (health.items || []).filter((i) => i.status !== 'ok');
  const status = health.status === 'critical' ? 'critical' : health.status === 'warning' ? 'warning' : 'online';
  return h(
    'section',
    { class: ['ds-card', 'k-health-strip', bad.length ? `ds-card--status-${status}` : null], 'aria-label': t('overview.health') },
    bad.length === 0
      ? statusBadge('online', t('overview.healthOk'))
      : bad.map((i) =>
          h(
            'span',
            { class: 'k-health-strip__extra' },
            statusBadge(i.status === 'critical' ? 'critical' : 'warning', healthItemText(i)),
            i.product_code ? [' ', h('code', { class: 'k-code-tag' }, i.product_code)] : null,
          ),
        ),
  );
}

function alertsBlock(res, onAck) {
  if (!res) return null;
  if (res.error) {
    if (res.error.code === 'alerts_unavailable') {
      return card({ title: t('overview.alerts'), body: h('p', { class: 'muted' }, t('overview.alertsOff')) });
    }
    return card({ title: t('overview.alerts'), body: errorState(res.error, { title: t('overview.alertsFailed') }) });
  }
  const counts = (res.meta && res.meta.counts) || {};
  const bySev = counts.active_by_severity || {};
  const summary = ['emergency', 'critical', 'warning'].map((s) => `${severityLabel(s)} ${fmt.num(bySev[s] || 0)}`).join(' · ');
  const events = res.data || [];
  const body = events.length
    ? h(
        'div',
        { class: 'k-scroll-x' },
        h(
          'table',
          { class: 'k-mini-table' },
          h('caption', { class: 'ds-visually-hidden' }, t('overview.alerts')),
          h(
            'thead',
            null,
            h(
              'tr',
              null,
              h('th', { scope: 'col' }, t('overview.col.severity')),
              h('th', { scope: 'col' }, t('overview.col.event')),
              h('th', { scope: 'col' }, t('overview.col.duration')),
              h('th', { scope: 'col' }, t('overview.col.assignee')),
              onAck ? h('th', { scope: 'col' }, h('span', { class: 'ds-visually-hidden' }, t('alerts.col.actions'))) : null,
            ),
          ),
          h(
            'tbody',
            null,
            events.map((e) =>
              h(
                'tr',
                { class: e.severity === 'emergency' ? 'is-emergency' : null },
                h('td', null, statusBadge(e.severity === 'info' ? 'info' : e.severity, severityLabel(e.severity))),
                h('td', null, h('a', { href: `/alerts/events/${encodeURIComponent(e.event_id)}` }, e.title), e.subject ? h('span', { class: 'k-sub' }, e.subject) : null),
                h('td', null, fmt.duration((fmt.serverNow() - Date.parse(e.started_at)) / 1000)),
                h('td', null, who(e.assignee) || '—'),
                onAck
                  ? h('td', null, e.state === 'firing' && !e.acked && e.attention === 'action'
                    ? h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', 'aria-label': t('alerts.ack.aria', { title: e.title }), on: { click: () => onAck(e) } }, t('alerts.ack.short'))
                    : null)
                  : null,
              ),
            ),
          ),
        ),
      )
    : emptyState({ title: t('overview.noEvents'), iconName: 'success' });
  return card({
    title: t('overview.alerts'),
    actions: [
      h('span', { class: 'k-page-head__meta' }, t('overview.alertsSummary', { summary, n: fmt.num(counts.unacknowledged || 0) })),
      h('a', { href: '/alerts/events' }, t('overview.alertsAll')),
    ],
    body,
  });
}

function groupsBlock(facets) {
  if (!facets) return null;
  const groups = (facets.group && facets.group.values) || [];
  if (!groups.length) return null;
  const rows = groups
    .map((g) => ({ ...g, bad: g.count - g.online }))
    .sort((a, b) => b.bad - a.bad || a.value.localeCompare(b.value));
  return card({
    title: t('overview.groups'),
    body: h(
      'table',
      { class: 'k-mini-table' },
      h('caption', { class: 'ds-visually-hidden' }, t('overview.groups')),
      h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('overview.col.group')), h('th', { scope: 'col', class: 'num' }, t('overview.col.onlineTotal')), h('th', { scope: 'col' }, t('overview.col.status')))),
      h(
        'tbody',
        null,
        rows.map((g) =>
          h(
            'tr',
            null,
            h('td', null, h('a', { href: `/hosts${qs({ group: g.value })}` }, g.value)),
            h('td', { class: 'num' }, `${fmt.num(g.online)} / ${fmt.num(g.count)}`),
            h('td', null, g.bad ? statusBadge('offline', t('overview.groupBad', { n: fmt.num(g.bad) })) : statusBadge('online')),
          ),
        ),
      ),
    ),
  });
}

function longestBlock(list) {
  return card({
    title: t('overview.longest'),
    actions: h('a', { href: '/hosts?status=offline&sort=last_seen' }, t('overview.seeAll')),
    body: list && list.length
      ? h(
          'table',
          { class: 'k-mini-table' },
          h('caption', { class: 'ds-visually-hidden' }, t('overview.longest')),
          h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('col.host')), h('th', { scope: 'col' }, t('col.offlineFor')), h('th', { scope: 'col' }, t('col.lastReason')))),
          h(
            'tbody',
            null,
            list.map((r) =>
              h('tr', null, h('td', null, hostNameCell(r)), h('td', null, offlineFor(r)), h('td', null, reasonText(r.offline_reason) || '—')),
            ),
          ),
        )
      : emptyState({ title: t('overview.noOffline'), iconName: 'success' }),
  });
}

function topBlock(o) {
  let mode = 'cpu';
  const body = h('div');
  const btnCpu = h('button', { type: 'button', 'aria-pressed': 'true' }, t('overview.top.cpu'));
  const btnMem = h('button', { type: 'button', 'aria-pressed': 'false' }, t('overview.top.memory'));
  const draw = () => {
    btnCpu.setAttribute('aria-pressed', mode === 'cpu' ? 'true' : 'false');
    btnMem.setAttribute('aria-pressed', mode === 'memory' ? 'true' : 'false');
    const list = (mode === 'cpu' ? o.top_cpu : o.top_memory) || [];
    mount(body, 
      list.length
        ? h(
            'table',
            { class: 'k-mini-table' },
            h('caption', { class: 'ds-visually-hidden' }, mode === 'cpu' ? t('overview.top.cpuCaption') : t('overview.top.memCaption')),
            h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('col.host')), h('th', { scope: 'col', class: 'num' }, t('col.cpu')), h('th', { scope: 'col', class: 'num' }, t('col.memory')))),
            h(
              'tbody',
              null,
              list.map((r) =>
                h(
                  'tr',
                  null,
                  h('td', null, hostNameCell(r)),
                  h('td', { class: 'num' }, fmt.pct(r.cpu_usage)),
                  h('td', { class: 'num' }, r.memory_used_ratio == null ? '—' : fmt.pct(r.memory_used_ratio * 100)),
                ),
              ),
            ),
          )
        : h('p', { class: 'muted' }, t('overview.top.none')),
    );
  };
  btnCpu.addEventListener('click', () => {
    mode = 'cpu';
    draw();
  });
  btnMem.addEventListener('click', () => {
    mode = 'memory';
    draw();
  });
  draw();
  return card({
    title: t('overview.top'),
    actions: h('div', { class: 'k-timerange', role: 'group', 'aria-label': t('overview.top.switch') }, btnCpu, btnMem),
    body,
  });
}

export function render(ctx) {
  const root = ctx.root;
  const dataTime = h('span', { class: 'k-data-time' }, t('common.loading'));
  const refreshBtn = h(
    'button',
    { type: 'button', class: 'ds-button ds-button--icon ds-button--sm', 'aria-label': t('common.refresh'), title: t('common.refresh') },
    icon('refresh'),
  );
  const scoped = session.scopedGroups();
  const body = h('div');
  mount(root, 
    pageHead({
      title: t('overview.title'),
      meta: scoped.length ? t('overview.scope', { groups: scoped.join('、') }) : null,
      actions: [dataTime, refreshBtn],
    }),
    body,
  );
  // 載入中：KPI 骨架（不顯示 0）
  mount(body, h('div', { class: 'ds-kpi-row' }, ORDER.map((s) => kpiSkeleton(statusLabel(s)))), skeletonBlock(4));

  let ctrl = null;
  let disposed = false;
  let pending = null;

  async function load() {
    if (ctrl) ctrl.abort();
    ctrl = new AbortController();
    const signal = ctrl.signal;
    const wantAlerts = session.can('alerts:read');
    const [ov, facets, alerts, hostEvents] = await Promise.all([
      api('/api/v1/overview', { signal }).then((d) => ({ d }), (e) => ({ e })),
      api('/api/v1/hosts/facets', { signal }).then((d) => d, () => null),
      wantAlerts
        ? api('/api/v1/alerts?view=active&limit=8', { signal, raw: true }).then((r) => r, (e) => ({ error: e }))
        : Promise.resolve(null),
      wantAlerts
        ? api(`/api/v1/alerts${qs({ view: 'active', category: 'host,identity', limit: 1000 })}`, { signal, raw: true }).then((r) => r, (e) => ({ error: e }))
        : Promise.resolve(null),
    ]);
    if (disposed || signal.aborted) return;
    if (ov.e) {
      if (ov.e.name === 'AbortError') return;
      mount(body, errorState(ov.e, { title: t('overview.failed'), onRetry: load }));
      return;
    }
    const o = ov.d;
    mount(dataTime, t('common.dataTime', { time: fmt.time(fmt.serverNow()) }));
    if (o.total === 0) {
      mount(body, 
        emptyState({
          title: t('overview.empty.title'),
          body: session.can('enroll.tokens:write') ? t('overview.empty.body') : t('overview.empty.bodyNoPerm'),
          iconName: 'server',
        }),
      );
      return;
    }
    mount(body, 
      kpis(o, hostAlertCounts(hostEvents)),
      healthStrip(o.health),
      h(
        'div',
        { class: 'k-stack' },
        (() => {
          const a = alertsBlock(alerts, session.can('alerts:write') ? ackFromOverview : null);
          const g = groupsBlock(facets);
          if (a && g) return h('div', { class: 'k-grid k-grid--3-1' }, a, g);
          return a || g;
        })(),
        h('div', { class: 'k-grid k-grid--1-1' }, longestBlock(o.longest_offline), topBlock(o)),
      ),
    );
  }

  async function ackFromOverview(e) {
    if (await ackDialog(e)) load();
  }

  refreshBtn.addEventListener('click', load);
  load();
  // 即時更新：狀態變化後重新抓取（最多每 5 秒一次）；另外每 30 秒更新一次
  const unsub = sse.subscribe((ev) => {
    if (!ev.type.startsWith('host.') && !ev.type.startsWith('alert.') && ev.type !== 'resync') return;
    if (pending) return;
    pending = setTimeout(() => {
      pending = null;
      load();
    }, 5000);
  });
  const timer = setInterval(load, 30000);
  return {
    dispose() {
      disposed = true;
      unsub();
      clearInterval(timer);
      if (pending) clearTimeout(pending);
      if (ctrl) ctrl.abort();
    },
  };
}
