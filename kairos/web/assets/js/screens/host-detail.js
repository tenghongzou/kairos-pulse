// S05 主機詳情（設計包 kairos/screens.md S05；API GET /api/v1/hosts/{id}、/metrics/{name}、/connections）。
//
// 分頁（以連結實作，網址可分享）：概覽 /hosts/:id、趨勢 /hosts/:id/metrics、行程 /hosts/:id/processes、
// 資產資訊 /hosts/:id/inventory、連線與憑證 /hosts/:id/connection。0.8.0〔草稿〕（F4，設計包 §30）起有行程與
// 資產資訊（host-processes.js、host-inventory.js），趨勢分為六個指標群組（host-metrics.js）。
// 隱藏（沒有後端或沒有畫面）：事件（S06／S07 之後）、稽核紀錄（S18 之後）、
// 編輯顯示名稱、進入維護、除役、撤銷憑證。0.9 起頁首的「更多」有匯出個資與刪除主機資料（管理員；host-privacy.js）。
// 斷線紀錄：GET /hosts/{id}/connections（Kairos 這次啟動後的上線與離線，記憶體中）＋24 小時上線時間軸
// （由回報資料的空缺推算）。憑證：詳情的 certificate 摘要；有 pki:read 時另列序號、指紋與簽發方式。

import { h, icon, mount } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import * as fmt from '../core/format.js';
import { pageHead, card, dl, emptyState, errorState, inlineAlert, statusBadge, copyButton, skeletonBlock, codeTag } from '../ui/components.js';
import { sparkline } from '../ui/chart.js';
import { hostName, reasonText, osText, memRatio, isOldPulse, osFamily, oldPulseCell } from './host-common.js';
import { metricsTab } from './host-metrics.js';
import { processesTab } from './host-processes.js';
import { inventoryTab } from './host-inventory.js';
import { hostActions } from './host-privacy.js';

const TABS = ['overview', 'metrics', 'processes', 'inventory', 'connection'];

function tabs(id, active) {
  const items = [
    ['overview', `/hosts/${id}`, t('host.tab.overview')],
    ['metrics', `/hosts/${id}/metrics`, t('host.tab.metrics')],
    ['processes', `/hosts/${id}/processes`, t('host.tab.processes')],
    ['inventory', `/hosts/${id}/inventory`, t('host.tab.inventory')],
    ['connection', `/hosts/${id}/connection`, t('host.tab.connection')],
  ];
  return h(
    'nav',
    { class: 'ds-tabs', 'aria-label': t('host.tabs') },
    items.map(([k, href, label]) => h('a', { href, 'aria-current': k === active ? 'page' : null }, label)),
  );
}

function summary(hst) {
  const parts = [
    hst.group ? t('host.groupIs', { g: hst.group }) : null,
    hst.tags && hst.tags.length ? t('host.tagsAre', { tags: hst.tags.join('、') }) : null,
    hst.ip,
    osText(hst.os) !== '—' ? osText(hst.os) : null,
    hst.agent_version ? `Pulse ${hst.agent_version}` : null,
  ].filter(Boolean);
  return h(
    'section',
    { class: 'ds-card', 'aria-label': t('host.summary') },
    h(
      'p',
      { class: 'k-row' },
      statusBadge(hst.status),
      hst.offline_for_secs != null ? h('span', { class: 'secondary' }, fmt.duration(hst.offline_for_secs)) : null,
      hst.status === 'offline' && hst.status_detail && hst.status_detail.offline_reason
        ? h('span', { class: 'secondary' }, reasonText(hst.status_detail.offline_reason))
        : null,
      hst.status_detail && hst.status_detail.long_offline ? h('span', { class: 'k-sub' }, t('host.longOffline')) : null,
    ),
    h('p', { class: 'k-row' }, h('span', { class: 'secondary' }, 'agent_id'), codeTag(hst.agent_id), copyButton(hst.agent_id, t('host.copyId'))),
    hst.display_name && hst.display_name !== hst.hostname ? h('p', { class: 'secondary' }, t('host.hostnameIs', { name: hst.hostname })) : null,
    parts.length ? h('p', { class: 'secondary' }, parts.join(' · ')) : null,
  );
}

function clockSkew(hst) {
  const r = hst.last_report;
  if (!r || !r.ts || !r.recv_at) return null;
  const diff = Math.abs(Date.parse(r.recv_at) - Date.parse(r.ts));
  // durable 的補送資料本來就比收到時間早，不是時鐘問題
  if (hst.replaying || diff <= 5 * 60 * 1000) return null;
  return inlineAlert({
    kind: 'warning',
    title: t('host.clockSkew', { d: fmt.duration(diff / 1000) }),
    code: 'KRS-W8302',
    body: t('host.clockSkewBody'),
  });
}

// ---------------------------------------------------------------------------
// 概覽
function overviewTab(hst, el) {
  const d = hst.status_detail || {};
  const reason = d.offline_reason;
  const statusRows = [
    hst.status === 'offline' ? [t('host.lastReason'), reason ? [reasonText(reason), ' ', codeTag(reason)] : t('host.reasonUnknown')] : null,
    [t('host.lastSeen'), [fmt.timeEl(hst.last_seen), h('span', { class: 'k-sub' }, `${fmt.dateTime(hst.last_seen)} ${fmt.tzLabel()}`)]],
    [t('host.firstSeen'), `${fmt.dateTime(hst.first_seen)} ${fmt.tzLabel()}`],
    [t('host.threshold'), hst.offline_after_ms ? t('host.thresholdValue', { s: fmt.num(Math.round(hst.offline_after_ms / 1000)) }) : '—'],
    [t('host.interval'), hst.report_interval_ms ? t('host.intervalValue', { s: fmt.num(Math.round(hst.report_interval_ms / 1000)) }) : '—'],
    [t('host.delivery'), hst.durable == null ? t('host.deliveryUnknown') : hst.durable ? (hst.replaying ? t('host.replaying') : t('host.durable')) : t('host.notDurable')],
    hst.source === 'import' ? [t('host.source'), t('host.sourceImport')] : null,
  ];
  const suggestion = hst.status === 'offline' && reason && has(`reason.${reason}.next`) ? inlineAlert({ kind: 'info', title: t('host.nextStep'), body: t(`reason.${reason}.next`) }) : null;
  const neverConnected =
    d.never_connected ? inlineAlert({ kind: 'info', title: t('host.neverTitle'), body: [h('p', null, t('host.neverBody')), h('pre', { class: 'ds-code-block' }, 'pulse status\nsudo pulse check-config --connect')] }) : null;

  const r = hst.last_report;
  const online = hst.status === 'online';
  const valuesBody = r ? currentValues(hst, r, online) : h('p', { class: 'muted' }, t('host.noReport'));

  const sparkCpu = h('div', null, skeletonBlock(1));
  const sparkMem = h('div', null, skeletonBlock(1));
  const uptime = h('div', null, skeletonBlock(1));
  mount(el, 
    clockSkew(hst),
    neverConnected,
    h(
      'div',
      { class: 'k-grid k-grid--1-1' },
      card({ title: t('host.statusCard'), body: [dl(statusRows), suggestion] }),
      card({ title: t('host.valuesCard'), body: valuesBody }),
    ),
    h(
      'div',
      { class: 'k-stack' },
      card({
        title: t('host.lastHour'),
        actions: h('a', { href: `/hosts/${hst.agent_id}/metrics` }, t('host.seeTrends')),
        body: h('div', { class: 'k-grid k-grid--1-1' }, h('div', null, h('p', { class: 'strong' }, t('col.cpu')), sparkCpu), h('div', null, h('p', { class: 'strong' }, t('col.memory')), sparkMem)),
      }),
      card({ title: t('host.uptime24h'), body: uptime }),
    ),
  );
  // 最近 1 小時
  metricSeries(hst.agent_id, 'system.cpu.usage', '1h')
    .then((res) => {
      const pts = res.points.map((p) => p.avg);
      mount(sparkCpu, 
        pts.length > 1 ? sparkline(pts, { width: 280, height: 40, max: 100, label: t('host.sparkCpu', { last: fmt.pct(pts[pts.length - 1]) }) }) : h('p', { class: 'muted' }, t('host.noData')),
      );
    })
    .catch((e) => mount(sparkCpu, h('p', { class: 'muted' }, e.code === 'metric_not_found' ? t('host.noData') : t('host.loadFailed'))));
  memoryPercent(hst.agent_id, '1h')
    .then((res) => {
      const pts = res.points.map((p) => p.avg);
      mount(sparkMem, 
        pts.length > 1 ? sparkline(pts, { width: 280, height: 40, max: 100, label: t('host.sparkMem', { last: fmt.pct(pts[pts.length - 1]) }) }) : h('p', { class: 'muted' }, t('host.noData')),
      );
    })
    .catch(() => mount(sparkMem, h('p', { class: 'muted' }, t('host.noData'))));
  // 上線時間軸（24 小時）：有回報的 5 分鐘桶為「有資料」，空缺為「沒有回報」（離線或未保存）
  metricSeries(hst.agent_id, 'system.cpu.usage', '24h')
    .then((res) => mount(uptime, uptimeBar(res)))
    .catch(() => mount(uptime, h('p', { class: 'muted' }, t('host.noData'))));
}

/** 目前數值（設計包 S05「目前數值」、§30 D45）：CPU、記憶體、置換空間、負載、磁碟、運作時間、行程 */
function currentValues(hst, r, online) {
  const old = isOldPulse(hst);
  const windows = osFamily(hst) === 'windows';
  const before = (v) => (online ? v : t('host.beforeOffline', { v }));
  const cpu = r.cpu_usage != null ? fmt.pct(r.cpu_usage) : '—';
  const mem = r.memory_total_bytes
    ? t('host.values.memoryValue', { pct: fmt.pct(memRatio(r)), used: fmt.gib(r.memory_used_bytes), total: fmt.gib(r.memory_total_bytes) })
    : '—';
  // 置換空間
  let swap;
  if (r.swap && r.swap.total === 0) swap = t('host.values.noSwap');
  else if (r.swap && r.swap.total != null) swap = before(t('host.values.swapValue', { pct: fmt.pct(r.swap.usage), used: fmt.gib(r.swap.used), total: fmt.gib(r.swap.total) }));
  else swap = old ? oldPulseCell() : t('host.na.notYet');
  // 負載
  let load;
  if (windows) load = h('span', { class: 'muted' }, `— ${t('host.na.windowsLoad')}`);
  else if (r.load && r.load['5m'] != null) {
    const cores = hst.inventory && hst.inventory.cpu_logical_cores;
    load = [
      before(t('host.values.loadValue', { l1: fmt.load(r.load['1m']), l5: fmt.load(r.load['5m']), l15: fmt.load(r.load['15m']) })),
      cores ? h('span', { class: 'k-sub' }, t('host.values.loadPerCore', { value: fmt.load(r.load['5m'] / cores), count: fmt.num(cores) })) : null,
    ];
  } else load = old ? oldPulseCell() : t('host.na.notYet');
  // 磁碟：使用率最高的掛載點＋「＋N 個掛載點」展開
  const fs = (r.filesystems || []).filter((f) => f.usage != null).sort((a, b) => b.usage - a.usage);
  let disk = '—';
  let diskNote = null;
  if (fs.length) {
    const est = (f) => (f.estimated ? t('host.estimatedSuffix') : '');
    const fullMark = (f) => (f.usage >= 95 ? h('span', { class: 'k-full' }, icon('status-critical'), t('host.values.diskFull')) : null);
    const top = fs[0];
    const listId = `mounts-${hst.agent_id.slice(0, 8)}`;
    const list = h(
      'ul',
      { class: 'k-mount-list', id: listId, hidden: true },
      fs.map((f) =>
        h(
          'li',
          null,
          t('host.values.mountLine', { mount: f.mountpoint, pct: `${fmt.pct(f.usage)}${est(f)}`, used: fmt.bytes(f.used), total: fmt.bytes(f.total), fs: f.fstype ? `，${f.fstype}` : '' }),
          ' ',
          fullMark(f),
        ),
      ),
    );
    const toggle =
      fs.length > 1
        ? h(
            'button',
            {
              type: 'button',
              class: 'ds-button ds-button--ghost ds-button--sm',
              'aria-expanded': 'false',
              'aria-controls': listId,
              on: {
                click: (ev) => {
                  const b = ev.currentTarget;
                  const open = b.getAttribute('aria-expanded') !== 'true';
                  b.setAttribute('aria-expanded', open ? 'true' : 'false');
                  b.textContent = open ? t('host.values.lessMounts') : t('host.values.moreMounts', { count: fmt.num(fs.length - 1) });
                  list.hidden = !open;
                },
              },
            },
            t('host.values.moreMounts', { count: fmt.num(fs.length - 1) }),
          )
        : null;
    disk = [h('span', { class: 'k-top-mount' }, before(`${top.mountpoint} ${fmt.pct(top.usage)}${est(top)}`)), ' ', fullMark(top), toggle, list];
    if (fs.some((f) => f.estimated)) diskNote = h('p', { class: 'caption' }, t('host.estimated.desc', { version: hst.agent_version || '—' }));
  }
  // 運作時間
  let uptime;
  if (hst.uptime_secs != null) {
    const booted = fmt.serverNow() - hst.uptime_secs * 1000;
    uptime = [fmt.duration(hst.uptime_secs), h('span', { class: 'k-sub' }, t('host.values.bootedAt', { time: fmt.dateTime(hst.boot_time || booted, { seconds: false }) }))];
  } else if (!online && r.uptime_secs != null) {
    uptime = t('host.values.uptimeOffline', { d: fmt.duration(r.uptime_secs) });
  } else uptime = old ? oldPulseCell() : t('host.na.notYet');
  // 行程
  const procs =
    r.process_count != null
      ? [before(t('host.values.processCount', { n: fmt.num(r.process_count) })), ' · ', h('a', { href: `/hosts/${hst.agent_id}/processes` }, t('host.values.seeProcesses'))]
      : old
        ? oldPulseCell()
        : t('host.na.notYet');
  return [
    dl([
      [t('col.cpu'), before(cpu)],
      [t('col.memory'), before(mem)],
      [t(windows ? 'host.values.swapWindows' : 'host.values.swap'), swap],
      [t('host.values.load'), load],
      [t('host.values.disk'), disk],
      [t('host.values.uptime'), uptime],
      [t('host.values.processes'), procs],
      [t('host.reportAt'), r.recv_at ? [fmt.timeEl(r.recv_at)] : '—'],
      r.metrics != null ? [t('host.metricsCount'), fmt.num(r.metrics)] : null,
    ]),
    diskNote,
  ];
}

function uptimeBar(res) {
  const from = Date.parse(res.from);
  const to = Date.parse(res.to);
  const bucket = (res.bucket_secs || 300) * 1000;
  const span = to - from;
  const have = res.points.map((p) => Date.parse(p.t)).sort((a, b) => a - b);
  const segs = [];
  let cursor = from;
  let i = 0;
  while (cursor < to) {
    const inData = i < have.length && have[i] <= cursor + bucket && have[i] + bucket > cursor;
    const kind = inData ? 'on' : 'off';
    if (segs.length && segs[segs.length - 1].kind === kind) segs[segs.length - 1].end = cursor + bucket;
    else segs.push({ kind, start: cursor, end: cursor + bucket });
    cursor += bucket;
    while (i < have.length && have[i] + bucket <= cursor) i += 1;
  }
  const offline = segs.filter((s) => s.kind === 'off');
  const offSecs = offline.reduce((a, s) => a + (Math.min(s.end, to) - s.start) / 1000, 0);
  const bar = h(
    'div',
    { class: 'k-uptime', role: 'img', 'aria-label': t('host.uptimeAria', { off: fmt.duration(offSecs), n: offline.length }) },
    segs.map((s) => {
      const span1 = h('span', { class: s.kind === 'on' ? 'k-tl-online' : 'k-tl-offline', title: `${fmt.dateTime(s.start, { seconds: false })}～${fmt.time(Math.min(s.end, to), { seconds: false })} ${s.kind === 'on' ? t('host.hasData') : t('host.noReportGap')}` });
      span1.style.setProperty('width', `${(((Math.min(s.end, to) - s.start) / span) * 100).toFixed(3)}%`);
      return span1;
    }),
  );
  return h(
    'div',
    null,
    h('div', { class: 'k-legend' }, h('span', null, h('span', { class: 'k-swatch k-tl-online' }), t('host.hasData')), h('span', null, h('span', { class: 'k-swatch k-tl-offline' }), t('host.noReportGap'))),
    bar,
    h('div', { class: 'k-uptime-axis' }, h('span', null, fmt.monthDayTime(from)), h('span', null, fmt.monthDayTime(to))),
    h('p', { class: 'caption' }, offline.length ? t('host.uptimeGaps', { n: offline.length, off: fmt.duration(offSecs) }) : t('host.uptimeFull')),
  );
}

async function metricSeries(id, name, range, signal) {
  const d = await api(`/api/v1/hosts/${id}/metrics/${name}${qs({ range })}`, { signal });
  const s = d.series && d.series[0];
  return { ...d, points: s ? s.points : [], unit: s ? s.unit : null };
}

/** 記憶體使用比例：used ÷ total（同一個時間桶） */
async function memoryPercent(id, range, signal) {
  const [used, total] = await Promise.all([metricSeries(id, 'system.memory.used', range, signal), metricSeries(id, 'system.memory.total', range, signal)]);
  const totals = new Map(total.points.map((p) => [p.t, p.avg]));
  const pct = (v, tt) => (tt ? (v / tt) * 100 : null);
  const points = used.points
    .map((p) => {
      const tt = totals.get(p.t);
      if (!tt) return null;
      return { t: p.t, avg: pct(p.avg, tt), min: pct(p.min, tt), max: pct(p.max, tt), count: p.count };
    })
    .filter(Boolean);
  return { ...used, points };
}

// ---------------------------------------------------------------------------
// 連線與憑證
function certStateText(c) {
  if (!c) return null;
  if (c.state === 'revoked') return [statusBadge('critical', t('cert.revoked')), ' ', `${fmt.dateTime(c.revoked_at)}`];
  if (c.state === 'expired') return [statusBadge('critical', t('cert.expired')), ' ', t('cert.expiredAt', { d: fmt.date(c.not_after) })];
  if (c.state === 'expiring') {
    const days = Math.max(0, Math.floor((Date.parse(c.not_after) - fmt.serverNow()) / 86400000));
    return [statusBadge('warning', t('cert.expiring', { n: days })), ' ', t('cert.until', { d: fmt.date(c.not_after) })];
  }
  return [statusBadge('online', t('cert.valid')), ' ', t('cert.until', { d: fmt.date(c.not_after) })];
}

function connectionTab(hst, el) {
  const c = hst.connection;
  const rej = hst.certificate_rejection;
  const certBox = h('div');
  const logBox = h('div', null, skeletonBlock(3));
  mount(el, 
    h(
      'div',
      { class: 'k-grid k-grid--1-1' },
      card({
        title: t('host.connCard'),
        body: dl([
          [t('host.peer'), hst.peer ? h('span', { class: 'mono' }, hst.peer) : '—'],
          [t('host.connectedAt'), hst.connected_at ? `${fmt.dateTime(hst.connected_at)} ${fmt.tzLabel()}` : '—'],
          [t('host.identity'), hst.identity_verified ? t('host.identityVerified') : t('host.identityNot')],
          [t('host.delivery'), c ? (c.durable ? t('host.durable') : t('host.notDurable')) : t('host.deliveryUnknown')],
          c && c.last_backfill_at ? [t('host.lastBackfill'), t('host.lastBackfillValue', { at: fmt.dateTime(c.last_backfill_at), lag: fmt.duration((c.last_backfill_lag_ms || 0) / 1000) })] : null,
          [t('host.fingerprint'), hst.host_fingerprint ? h('span', { class: 'mono' }, `${hst.host_fingerprint.slice(0, 23)}…`) : '—'],
        ]),
      }),
      card({ title: t('host.certCard'), body: certBox }),
    ),
    rej
      ? inlineAlert({
          kind: 'warning',
          title: t('host.rejected', { at: fmt.dateTime(rej.at), n: fmt.num(rej.count) }),
          code: rej.product_code,
          body: dl([
            [t('host.rejectReason'), has(`cert.reject.${rej.reason}`) ? t(`cert.reject.${rej.reason}`) : rej.reason],
            session.can('pki:read') ? [t('cert.serial'), rej.serial ? h('span', { class: 'mono' }, rej.serial) : '—'] : null,
            [t('host.peer'), rej.peer || '—'],
          ]),
        })
      : null,
    card({ title: t('host.connLog'), body: logBox }),
  );
  // 憑證：摘要（所有角色）；序號與指紋只給有 pki:read 的角色（S05 權限差異）
  const sumRows = [[t('cert.status'), hst.certificate ? certStateText(hst.certificate) : t('cert.none')]];
  mount(certBox, dl(sumRows));
  if (session.can('pki:read') && hst.certificate) {
    api(`/api/v1/pki/certificates${qs({ agent_id: hst.agent_id, state: 'all', limit: 5 })}`)
      .catch(() => api(`/api/v1/pki/certificates${qs({ agent_id: hst.agent_id, limit: 5 })}`))
      .then((list) => {
        const cur = (list || [])[0];
        if (!cur) return;
        mount(certBox, 
          dl([
            ...sumRows,
            [t('cert.serial'), h('span', { class: 'mono' }, cur.serial)],
            [t('cert.fingerprint'), h('span', { class: 'mono' }, cur.spki_fingerprint_short)],
            [t('cert.validity'), `${fmt.date(cur.not_before)}～${fmt.date(cur.not_after)}`],
            [t('cert.issuedVia'), cur.issued_via],
            [t('cert.approvedBy'), cur.approved_by || '—'],
          ]),
        );
      })
      .catch(() => {});
  }
  api(`/api/v1/hosts/${hst.agent_id}/connections`)
    .then((d) => {
      const note = h('p', { class: 'caption' }, t('host.connLogNote', { since: `${fmt.dateTime(d.since)} ${fmt.tzLabel()}`, n: d.max_entries }));
      if (!d.entries.length) {
        mount(logBox, note, h('p', { class: 'muted' }, t('host.connLogEmpty')));
        return;
      }
      mount(logBox, 
        note,
        h(
          'div',
          { class: 'k-scroll-x' },
          h(
            'table',
            { class: 'k-mini-table' },
            h('caption', { class: 'ds-visually-hidden' }, t('host.connLog')),
            h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('col.time', { tz: fmt.tzLabel() })), h('th', { scope: 'col' }, t('col.event')), h('th', { scope: 'col' }, t('col.reason')), h('th', { scope: 'col' }, t('col.ip')))),
            h(
              'tbody',
              null,
              d.entries.map((e) =>
                h(
                  'tr',
                  null,
                  h('td', null, fmt.dateTime(e.at)),
                  h('td', null, e.event === 'online' ? statusBadge('online', t('host.evOnline')) : statusBadge('offline', t('host.evOffline'))),
                  h('td', null, e.reason ? [reasonText(e.reason), ' ', codeTag(e.reason)] : '—'),
                  h('td', null, e.ip ? h('span', { class: 'mono' }, e.ip) : '—'),
                ),
              ),
            ),
          ),
        ),
      );
    })
    .catch((e) => mount(logBox, errorState(e, { title: t('host.connLogFailed') })));
}

// ---------------------------------------------------------------------------
export function render(ctx) {
  const id = String(ctx.params.id || '').toLowerCase();
  const tab = ctx.params.tab ? (TABS.includes(ctx.params.tab) && ctx.params.tab !== 'overview' ? ctx.params.tab : null) : 'overview';
  const root = ctx.root;
  if (!tab) {
    ctx.app.notFound();
    return {};
  }
  const head = h('div');
  const tabBody = h('div');
  mount(root, 
    pageHead({ title: t('host.loading'), crumbs: [{ text: t('nav.hosts'), href: '/hosts' }, { text: '…' }] }),
    head,
    tabs(id, tab),
    tabBody,
  );
  mount(tabBody, skeletonBlock(6));
  let disposed = false;
  let cleanup = null;
  let refreshTimer = null;

  async function load() {
    try {
      const hst = await api(`/api/v1/hosts/${encodeURIComponent(id)}`);
      if (disposed) return;
      const name = hostName(hst);
      ctx.setTitle(name);
      root.firstChild.replaceWith(
        pageHead({
          title: name,
          crumbs: [{ text: t('nav.hosts'), href: '/hosts' }, hst.group ? { text: hst.group, href: `/hosts${qs({ group: hst.group })}` } : null, { text: name }].filter(Boolean),
          actions: hostActions(hst),
        }),
      );
      mount(head, summary(hst));
      if (cleanup) cleanup();
      cleanup = null;
      if (tab === 'overview') overviewTab(hst, tabBody);
      else if (tab === 'metrics') cleanup = metricsTab(hst, tabBody, ctx);
      else if (tab === 'processes') cleanup = processesTab(hst, tabBody, ctx);
      else if (tab === 'inventory') cleanup = inventoryTab(hst, tabBody);
      else connectionTab(hst, tabBody);
      ctx.focusTitle();
    } catch (e) {
      if (disposed) return;
      if (e.code === 'host_not_found') {
        ctx.setTitle(t('host.notFound'));
        mount(root, 
          pageHead({ title: t('host.notFound'), crumbs: [{ text: t('nav.hosts'), href: '/hosts' }, { text: id.slice(0, 8) }] }),
          emptyState({
            title: t('host.notFoundTitle'),
            body: t('host.notFoundBody', { id }),
            iconName: 'server',
            actions: [h('a', { class: 'ds-button ds-button--secondary', href: '/hosts' }, icon('chevron-left'), t('host.backToList'))],
          }),
        );
        ctx.focusTitle();
        return;
      }
      mount(tabBody, errorState(e, { title: t('host.failed'), onRetry: load }));
    }
  }
  load();
  // 這台主機的狀態變化：更新頁首與概覽（趨勢分頁不重畫，避免使用者正在看的圖跳動）
  const unsub = sse.subscribe((ev) => {
    if (!ev.type.startsWith('host.') || !ev.data || ev.data.agent_id !== id) return;
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(() => {
      if (tab === 'overview' || tab === 'connection') load();
    }, 1000);
  });
  return {
    dispose() {
      disposed = true;
      unsub();
      clearTimeout(refreshTimer);
      if (cleanup) cleanup();
    },
  };
}
