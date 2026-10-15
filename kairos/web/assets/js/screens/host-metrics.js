// S05 主機詳情的「趨勢」分頁（設計包 kairos/screens.md S05「F4 指標擴充」、decisions/INTEGRATION.md §30 D45；
// API GET /api/v1/hosts/{id}/metrics、/metrics/{name}，kairos/API.md 第 5.3 節）。
//
// - 六個指標群組（?metric=）：CPU 與記憶體、磁碟使用率、磁碟讀寫、網路、負載、置換空間。
// - 磁碟讀寫與網路以「裝置」「網路介面」選單切換（?dev=），最後一項「全部（小倍數）」；磁碟使用率超過 3 個
//   掛載點時改為小倍數。一張圖最多 3 條線，以線型＋圖例文字區分。
// - 計數器（讀寫、收發）由 API 回傳每秒速率（transform=rate）；重新開機時計數器重置，該段沒有速率（不補 0）。
// - 舊版 Pulse（0.2～0.7）：磁碟使用率為推算值（標示「推算」），沒有磁碟讀寫、負載、置換空間（EmptyState）。
// - 每張圖都有表格檢視；「匯出 CSV」匯出目前群組的圖表資料（欄名為指標名稱與 label）。

import { h, announce, mount, icon } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as fmt from '../core/format.js';
import { card, emptyState, errorState, inlineAlert, skeletonBlock } from '../ui/components.js';
import { trendChart, multiChart } from '../ui/chart.js';
import { F4_VERSION, isOldPulse, osFamily } from './host-common.js';

const RANGES = ['1h', '24h', '7d', '30d'];
export const GROUPS = ['cpu', 'fs', 'io', 'net', 'load', 'swap'];

async function fetchMetric(id, name, range, signal, extra = {}) {
  return api(`/api/v1/hosts/${id}/metrics/${name}${qs({ range, ...extra })}`, { signal });
}

/** 一條序列的點（沒有時空陣列） */
function firstPoints(res) {
  const s = res && res.series && res.series[0];
  return s ? s.points : [];
}

/** a ÷ b × 100（同一個時間桶；記憶體、置換空間的使用率） */
function ratioPoints(a, b) {
  const totals = new Map(b.map((p) => [p.t, p.avg]));
  const pct = (v, tt) => (tt ? (v / tt) * 100 : null);
  return a
    .map((p) => {
      const tt = totals.get(p.t);
      if (!tt) return null;
      return { t: p.t, avg: pct(p.avg, tt), min: pct(p.min, tt), max: pct(p.max, tt), count: p.count };
    })
    .filter(Boolean);
}

function labelsOf(list, name) {
  return list.filter((m) => m.name === name).map((m) => m.labels || {});
}

/** CSV（RFC 4180；欄名為指標名稱與 label，值與畫面相同） */
function csvOf(columns) {
  const times = [...new Set(columns.flatMap((c) => c.points.map((p) => p.t)))].sort();
  const esc = (v) => (/[",\n]/.test(v) ? `"${v.replace(/"/g, '""')}"` : v);
  const lines = [['time', ...columns.map((c) => c.name)].map(esc).join(',')];
  for (const tm of times) {
    lines.push([tm, ...columns.map((c) => {
      const p = c.points.find((q) => q.t === tm);
      return p && p.avg != null ? String(p.avg) : '';
    })].map(esc).join(','));
  }
  return `${lines.join('\n')}\n`;
}

function seriesName(name, labels) {
  const l = Object.entries(labels || {}).map(([k, v]) => `${k}=${v}`).join(',');
  return l ? `${name}{${l}}` : name;
}

/**
 * 趨勢分頁。
 * @param {object} hst  主機詳情（GET /api/v1/hosts/{id}）
 * @param {HTMLElement} el
 * @param {object} ctx  路由的 ctx（query）
 * @returns {() => void} 清除
 */
export function metricsTab(hst, el, ctx) {
  const id = hst.agent_id;
  let range = RANGES.includes(ctx.query.get('range')) ? ctx.query.get('range') : '24h';
  let group = GROUPS.includes(ctx.query.get('metric')) ? ctx.query.get('metric') : 'cpu';
  let dev = ctx.query.get('dev') || '';
  let ctrl = null;
  let available = null;
  let disks = [];
  let csvColumns = [];
  const old = isOldPulse(hst);
  const windows = osFamily(hst) === 'windows';
  const cores = hst.inventory && hst.inventory.cpu_logical_cores;
  const bucketDefault = (hst.report_interval_ms || 5000) / 1000;

  const syncUrl = () => {
    const p = new URLSearchParams();
    if (range !== '24h') p.set('range', range);
    if (group !== 'cpu') p.set('metric', group);
    if (dev && (group === 'io' || group === 'net')) p.set('dev', dev);
    const q = p.toString();
    history.replaceState(null, '', `${location.pathname}${q ? `?${q}` : ''}`);
  };

  const rangeGroup = h(
    'div',
    { class: 'k-timerange', role: 'group', 'aria-label': t('host.range') },
    RANGES.map((r) =>
      h(
        'button',
        {
          type: 'button',
          'aria-pressed': r === range ? 'true' : 'false',
          dataset: { range: r },
          on: {
            click: () => {
              range = r;
              for (const b of rangeGroup.querySelectorAll('button')) b.setAttribute('aria-pressed', b.dataset.range === r ? 'true' : 'false');
              syncUrl();
              load();
            },
          },
        },
        t(`host.range.${r}`),
      ),
    ),
  );
  const groupBar = h(
    'div',
    { class: 'k-timerange k-metric-groups', role: 'group', 'aria-label': t('host.metrics.groups') },
    GROUPS.map((g) =>
      h(
        'button',
        {
          type: 'button',
          'aria-pressed': g === group ? 'true' : 'false',
          dataset: { metric: g },
          on: {
            click: () => {
              group = g;
              dev = '';
              for (const b of groupBar.querySelectorAll('button')) b.setAttribute('aria-pressed', b.dataset.metric === g ? 'true' : 'false');
              syncUrl();
              announce(t('host.metrics.shown', { g: t(`host.metrics.group.${g}`) }));
              load();
            },
          },
        },
        t(`host.metrics.group.${g}`),
      ),
    ),
  );
  const csvLink = h('a', { class: 'ds-button ds-button--secondary ds-button--sm', href: '#', download: '', dataset: { native: '1' } }, icon('download'), t('host.metrics.csv'));
  csvLink.addEventListener('click', () => {
    const url = URL.createObjectURL(new Blob([csvOf(csvColumns)], { type: 'text/csv;charset=utf-8' }));
    csvLink.setAttribute('href', url);
    csvLink.setAttribute('download', `${hst.hostname}-${group}-${range}.csv`.replace(/[^\w.-]+/g, '_'));
    setTimeout(() => URL.revokeObjectURL(url), 10000);
  });
  const info = h('p', { class: 'caption', role: 'status' });
  const body = h('div', { class: 'k-stack' });
  const limited = hst.series_limited_at
    ? inlineAlert({
        kind: 'warning',
        title: t('host.metrics.seriesLimited'),
        code: 'KRS-W4104',
        body: t('host.metrics.seriesLimitedBody', { at: `${fmt.dateTime(hst.series_limited_at)} ${fmt.tzLabel()}` }),
      })
    : null;
  mount(
    el,
    limited,
    groupBar,
    h('div', { class: 'k-chart-head' }, h('span', { class: 'secondary' }, t('host.rangeTz', { tz: fmt.tzLabel() })), rangeGroup, csvLink),
    info,
    body,
  );
  csvLink.setAttribute('aria-label', t('host.metrics.csvLabel', { g: t(`host.metrics.group.${group}`) }));

  const setInfo = (res) => {
    if (!res) return;
    const resText = has(`host.res.${res.resolution}`) ? t(`host.res.${res.resolution}`) : res.resolution;
    const notes = (res.notes || []).join(' ');
    info.textContent = `${t('host.resolution', { r: resText })}${notes ? ` · ${notes}` : ''}`;
  };
  const failed = (box, e) => {
    if (e.name === 'AbortError') return;
    mount(box, e.code === 'metric_not_found' ? emptyState({ title: t('host.na.notYet'), iconName: 'clock' }) : errorState(e, { title: t('host.trendFailed'), onRetry: load }));
  };
  const oldPulseEmpty = () =>
    emptyState({
      title: t('host.na.oldPulseTitle'),
      body: t('host.na.oldPulseBody', { version: F4_VERSION, current: hst.agent_version || '—' }),
      iconName: 'info',
    });

  /** 裝置／網路介面選單（2 個以上才出現；最後一項「全部（小倍數）」） */
  function deviceSelect(labelKey, devices, onChange) {
    if (devices.length < 2) return null;
    const sel = h(
      'select',
      { class: 'ds-select', name: 'dev', on: { change: () => onChange(sel.value) } },
      devices.map((d) => h('option', { value: d.value, selected: dev === d.value }, d.text)),
      h('option', { value: '*', selected: dev === '*' }, t('host.metrics.allMultiples')),
    );
    return h('label', { class: 'k-labelled-select' }, t(labelKey), sel);
  }

  // ---- CPU 與記憶體 ----
  async function cpuGroup(signal) {
    const cpuBox = h('div', null, skeletonBlock(5));
    const memBox = h('div', null, skeletonBlock(5));
    const procLink = h('p', { class: 'caption' });
    mount(body, card({ title: t('host.chartCpu'), body: [cpuBox, procLink] }), card({ title: t('host.chartMem'), body: memBox }));
    const showProc = (ms) => {
      if (old) return;
      const at = new Date(ms).toISOString();
      mount(procLink, h('a', { href: `/hosts/${id}/processes${qs({ at })}` }, t('host.metrics.processesAt', { time: fmt.time(ms, { seconds: false }) })));
    };
    try {
      const res = await fetchMetric(id, 'system.cpu.usage', range, signal);
      setInfo(res);
      const pts = firstPoints(res);
      csvColumns.push({ name: 'system.cpu.usage', points: pts });
      mount(
        cpuBox,
        pts.length
          ? trendChart({ label: t('host.chartCpu'), from: res.from, to: res.to, bucketSecs: res.bucket_secs || bucketDefault, points: pts, yMax: 100, format: (v) => fmt.pct(v), onCursor: showProc })
          : emptyState({ title: t('host.noDataRange'), iconName: 'clock' }),
      );
      if (pts.length) showProc(Date.parse(pts[pts.length - 1].t));
    } catch (e) {
      failed(cpuBox, e);
    }
    try {
      const [used, total] = await Promise.all([fetchMetric(id, 'system.memory.used', range, signal), fetchMetric(id, 'system.memory.total', range, signal)]);
      const pts = ratioPoints(firstPoints(used), firstPoints(total));
      csvColumns.push({ name: 'memory_used_percent', points: pts });
      mount(
        memBox,
        pts.length
          ? trendChart({ label: t('host.chartMem'), from: used.from, to: used.to, bucketSecs: used.bucket_secs || bucketDefault, points: pts, yMax: 100, format: (v) => fmt.pct(v) })
          : emptyState({ title: t('host.noDataRange'), iconName: 'clock' }),
      );
    } catch (e) {
      failed(memBox, e);
    }
  }

  // ---- 磁碟使用率 ----
  async function fsGroup(signal) {
    const box = h('div', null, skeletonBlock(5));
    const note = h('div');
    mount(body, card({ title: t('host.metrics.group.fs'), body: [box, note] }));
    try {
      const res = await fetchMetric(id, 'system.filesystem.usage', range, signal);
      setInfo(res);
      const series = res.series || [];
      if (!series.length) {
        mount(box, emptyState({ title: t('host.na.notYet'), iconName: 'clock' }));
        return;
      }
      const estimated = series.some((s) => s.derived === 'estimated' || s.points.some((p) => p.estimated));
      const name = (s) => `${s.labels.mountpoint || '—'}${s.derived === 'estimated' ? t('host.estimatedSuffix') : ''}`;
      for (const s of series) csvColumns.push({ name: seriesName('system.filesystem.usage', s.labels), points: s.points });
      const opts = { from: res.from, to: res.to, bucketSecs: res.bucket_secs || bucketDefault, yMax: 100, format: (v) => fmt.pct(v) };
      if (series.length <= 3) {
        mount(box, multiChart({ ...opts, label: t('host.metrics.chart.fsAll'), series: series.map((s) => ({ label: name(s), points: s.points })) }));
      } else {
        mount(
          box,
          h(
            'div',
            { class: 'k-multiples' },
            series.map((s) => h('section', { class: 'k-multiple' }, h('h3', { class: 'k-multiple__title' }, name(s)), multiChart({ ...opts, label: t('host.metrics.chart.fs', { mount: name(s) }), series: [{ label: t('host.metrics.series.usage'), points: s.points }] }))),
          ),
        );
      }
      mount(note, estimated ? h('p', { class: 'caption' }, t('host.estimated.desc', { version: hst.agent_version || '—' })) : null);
    } catch (e) {
      failed(box, e);
    }
  }

  // ---- 磁碟讀寫、網路（每個裝置：兩條線） ----
  async function ioGroup(signal, kind) {
    const isNet = kind === 'net';
    if (!isNet && old) {
      mount(body, card({ title: t('host.metrics.group.io'), body: oldPulseEmpty() }));
      return;
    }
    const readName = isNet ? 'system.network.received' : 'system.disk.read';
    const writeName = isNet ? 'system.network.transmitted' : 'system.disk.written';
    const devices = labelsOf(available, readName)
      .map((l) => l.device)
      .filter(Boolean)
      .sort();
    if (!devices.length) {
      mount(body, card({ title: t(`host.metrics.group.${kind}`), body: emptyState({ title: t('host.na.notYet'), body: t('host.metrics.noDevices'), iconName: 'clock' }) }));
      return;
    }
    // 磁碟的選項寫「裝置（掛載點）」：資產的磁碟清單有裝置與掛載點的對應（沒有時只寫裝置）
    const mounts = new Map(disks.filter((d) => d.device).map((d) => [d.device, d.mountpoint]));
    const opts = devices.map((d) => ({ value: d, text: !isNet && mounts.get(d) ? `${d}（${mounts.get(d)}）` : d }));
    if (!dev || (dev !== '*' && !devices.includes(dev))) dev = devices[0];
    const fmtRate = isNet ? fmt.rateBits : fmt.rateBytes;
    const sel = deviceSelect(isNet ? 'host.metrics.interface' : 'host.metrics.device', opts, (v) => {
      dev = v;
      syncUrl();
      load();
    });
    const chosen = dev === '*' ? devices : [dev];
    const box = h('div', null, skeletonBlock(5));
    const busyBox = h('div');
    const errBox = h('div');
    const unit = h('p', { class: 'caption' }, `${t(isNet ? 'host.metrics.unit.netRate' : 'host.metrics.unit.diskRate')} ${t('host.metrics.counterReset')}`);
    mount(body, card({ title: t(`host.metrics.group.${kind}`), body: [sel, box, busyBox, errBox, unit] }));
    try {
      const [rd, wr] = await Promise.all([fetchMetric(id, readName, range, signal), fetchMetric(id, writeName, range, signal)]);
      setInfo(rd);
      const pick = (res, d) => {
        const s = (res.series || []).find((x) => x.labels.device === d);
        return s ? s.points : [];
      };
      const base = { from: rd.from, to: rd.to, bucketSecs: rd.bucket_secs || bucketDefault, format: fmtRate };
      const label = (d) => t(isNet ? 'host.metrics.chart.net' : 'host.metrics.chart.io', { dev: !isNet && mounts.get(d) ? `${d}（${mounts.get(d)}）` : d });
      const charts = chosen.map((d) => {
        const a = pick(rd, d);
        const b = pick(wr, d);
        csvColumns.push({ name: seriesName(readName, { device: d }), points: a }, { name: seriesName(writeName, { device: d }), points: b });
        const ch = multiChart({
          ...base,
          label: label(d),
          series: [
            { label: t(isNet ? 'host.metrics.series.rx' : 'host.metrics.series.read'), points: a },
            { label: t(isNet ? 'host.metrics.series.tx' : 'host.metrics.series.write'), points: b },
          ],
        });
        return chosen.length > 1 ? h('section', { class: 'k-multiple' }, h('h3', { class: 'k-multiple__title' }, label(d)), ch) : ch;
      });
      mount(box, chosen.length > 1 ? h('div', { class: 'k-multiples' }, charts) : charts[0]);
      // 磁碟的忙碌時間比例（只有 Linux；速率 ms/s ÷ 10 = %）
      if (!isNet && !windows && labelsOf(available, 'system.disk.io_time').length && chosen.length === 1) {
        const busy = await fetchMetric(id, 'system.disk.io_time', range, signal, { 'label.device': chosen[0] });
        const pts = firstPoints(busy).map((p) => ({ ...p, avg: Math.min(100, p.avg / 10), min: p.min / 10, max: p.max / 10 }));
        csvColumns.push({ name: `${seriesName('system.disk.io_time', { device: chosen[0] })}/10`, points: pts });
        const busyLabel = t('host.metrics.chart.busy', { dev: chosen[0] });
        mount(busyBox, h('h3', { class: 'k-multiple__title' }, busyLabel), multiChart({ ...base, yMax: 100, format: (v) => fmt.pct(v), label: busyLabel, series: [{ label: t('host.metrics.series.busy'), points: pts }] }));
      }
      // 網路的錯誤數：這段時間的合計（速率 × 桶長）
      if (isNet && labelsOf(available, 'system.network.errors_received').length) {
        const [er, et] = await Promise.all([fetchMetric(id, 'system.network.errors_received', range, signal), fetchMetric(id, 'system.network.errors_transmitted', range, signal)]);
        const secs = er.bucket_secs || bucketDefault;
        const total = (res, d) => Math.round((((res.series || []).find((x) => x.labels.device === d) || { points: [] }).points).reduce((n, p) => n + (p.avg || 0) * secs, 0));
        mount(errBox, chosen.map((d) => h('p', { class: 'caption' }, t('host.metrics.netErrors', { dev: d, rx: fmt.num(total(er, d)), tx: fmt.num(total(et, d)) }))));
      }
    } catch (e) {
      failed(box, e);
    }
  }

  // ---- 負載 ----
  async function loadGroup(signal) {
    if (windows) {
      mount(body, card({ title: t('host.metrics.group.load'), body: emptyState({ title: t('host.na.windowsLoad'), body: t('host.na.windowsLoadBody'), iconName: 'info' }) }));
      return;
    }
    if (old) {
      mount(body, card({ title: t('host.metrics.group.load'), body: oldPulseEmpty() }));
      return;
    }
    const box = h('div', null, skeletonBlock(5));
    mount(body, card({ title: t('host.metrics.chart.load'), body: [box, h('p', { class: 'caption' }, t('host.metrics.unit.load'))] }));
    try {
      const names = ['system.cpu.load_1m', 'system.cpu.load_5m', 'system.cpu.load_15m'];
      const res = await Promise.all(names.map((n) => fetchMetric(id, n, range, signal)));
      setInfo(res[0]);
      names.forEach((n, i) => csvColumns.push({ name: n, points: firstPoints(res[i]) }));
      if (!res.some((r) => firstPoints(r).length)) {
        mount(box, emptyState({ title: t('host.noDataRange'), iconName: 'clock' }));
        return;
      }
      mount(
        box,
        multiChart({
          label: t('host.metrics.chart.load'),
          from: res[0].from,
          to: res[0].to,
          bucketSecs: res[0].bucket_secs || bucketDefault,
          format: (v) => fmt.load(v),
          series: ['load_1m', 'load_5m', 'load_15m'].map((k, i) => ({ label: t(`host.metrics.series.${k}`), points: firstPoints(res[i]) })),
          refLines: cores ? [{ value: cores, label: t('host.metrics.ref.cores', { count: fmt.num(cores) }) }] : [],
        }),
      );
    } catch (e) {
      failed(box, e);
    }
  }

  // ---- 置換空間 ----
  async function swapGroup(signal) {
    if (old) {
      mount(body, card({ title: t('host.metrics.group.swap'), body: oldPulseEmpty() }));
      return;
    }
    const box = h('div', null, skeletonBlock(5));
    mount(body, card({ title: t('host.metrics.chart.swap'), body: box }));
    try {
      const [used, total] = await Promise.all([fetchMetric(id, 'system.swap.used', range, signal), fetchMetric(id, 'system.swap.total', range, signal)]);
      setInfo(used);
      const totals = firstPoints(total);
      if (totals.length && totals.every((p) => !p.max)) {
        mount(box, emptyState({ title: t('host.metrics.noSwap'), iconName: 'info' }));
        return;
      }
      const pts = ratioPoints(firstPoints(used), totals);
      csvColumns.push({ name: 'swap_used_percent', points: pts });
      mount(
        box,
        pts.length
          ? multiChart({ label: t('host.metrics.chart.swap'), from: used.from, to: used.to, bucketSecs: used.bucket_secs || bucketDefault, yMax: 100, format: (v) => fmt.pct(v), series: [{ label: t('host.metrics.series.usage'), points: pts }] })
          : emptyState({ title: t('host.noDataRange'), iconName: 'clock' }),
      );
    } catch (e) {
      failed(box, e);
    }
  }

  async function load() {
    if (ctrl) ctrl.abort();
    ctrl = new AbortController();
    const { signal } = ctrl;
    csvColumns = [];
    info.textContent = '';
    csvLink.setAttribute('aria-label', t('host.metrics.csvLabel', { g: t(`host.metrics.group.${group}`) }));
    mount(body, skeletonBlock(5));
    try {
      if (!available) {
        const [list, inv] = await Promise.all([
          api(`/api/v1/hosts/${id}/metrics`, { signal }),
          api(`/api/v1/hosts/${id}/inventory`, { signal }).catch(() => null),
        ]);
        available = list.metrics || [];
        disks = (inv && inv.inventory && inv.inventory.disks) || [];
      }
      if (group === 'cpu') await cpuGroup(signal);
      else if (group === 'fs') await fsGroup(signal);
      else if (group === 'io' || group === 'net') await ioGroup(signal, group);
      else if (group === 'load') await loadGroup(signal);
      else await swapGroup(signal);
    } catch (e) {
      if (e.name !== 'AbortError') mount(body, errorState(e, { title: t('host.trendFailed'), onRetry: load }));
    }
  }
  load();
  return () => ctrl && ctrl.abort();
}
