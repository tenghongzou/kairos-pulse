// 圖表：純 SVG，不引入圖表套件（design-system/README.md §6.7 Sparkline；screens.md S05 趨勢）。
//
// - 資料空缺（離線、storage_unavailable）不補 0：線條斷開，空缺以淺灰底標示（S05）。
// - 彙總資料（5 分鐘、1 小時）以平均線＋最小／最大範圍呈現。
// - 圖是 role="img" 並有文字摘要；滑鼠停留顯示數值與時間（顯示時區＋UTC）；
//   鍵盤與螢幕報讀器使用者以「以表格檢視資料」取得同樣的數值。

import { h, s, uid } from '../core/dom.js';
import { t } from '../core/strings.js';
import * as fmt from '../core/format.js';

/** Sparkline：values 為數字陣列（null 為空缺） */
export function sparkline(values, { width = 120, height = 28, label, max } = {}) {
  const vals = values.filter((v) => v != null);
  const svg = s('svg', {
    class: 'ds-sparkline',
    width,
    height,
    viewBox: `0 0 ${width} ${height}`,
    role: 'img',
    'aria-label': label || t('chart.sparkline'),
  });
  if (vals.length < 2) return svg;
  const hi = max != null ? max : Math.max(...vals);
  const lo = max != null ? 0 : Math.min(...vals);
  const span = hi - lo || 1;
  const step = width / (values.length - 1);
  let d = '';
  let pen = false;
  values.forEach((v, i) => {
    if (v == null) {
      pen = false;
      return;
    }
    const x = (i * step).toFixed(1);
    const y = (height - 2 - ((v - lo) / span) * (height - 4)).toFixed(1);
    d += `${pen ? 'L' : 'M'}${x} ${y} `;
    pen = true;
  });
  svg.appendChild(s('path', { class: 'k-line', d: d.trim() }));
  return svg;
}

function niceMax(v) {
  if (v <= 0) return 1;
  const p = 10 ** Math.floor(Math.log10(v));
  for (const m of [1, 2, 2.5, 5, 10]) if (m * p >= v) return m * p;
  return 10 * p;
}

/**
 * 趨勢圖。
 * @param {object} o
 *   label       圖的名稱（「CPU 使用率」）
 *   from, to    查詢範圍（ISO）
 *   bucketSecs  每點的時間寬度（原始資料為回報間隔）
 *   points      [{ t, min, max, avg, count }]（同一條序列）
 *   yMax        固定上限（百分比為 100）；省略時依資料
 *   format(v)   數值的文字
 *   onCursor(ms) 滑鼠移到某個點時呼叫（選用；例：「查看 HH:MM 的行程排行」）
 * @returns {HTMLElement}
 */
export function trendChart(o) {
  const W = 800;
  const H = 220;
  const ml = 52;
  const mr = 12;
  const mt = 10;
  const mb = 26;
  const from = Date.parse(o.from);
  const to = Date.parse(o.to);
  const span = Math.max(1, to - from);
  const pts = (o.points || [])
    .map((p) => ({ ...p, ms: Date.parse(p.t) }))
    .filter((p) => !Number.isNaN(p.ms))
    .sort((a, b) => a.ms - b.ms);
  const dataMax = pts.reduce((m, p) => Math.max(m, p.max ?? p.avg ?? 0), 0);
  const yMax = o.yMax != null ? o.yMax : niceMax(dataMax * 1.1);
  const x = (ms) => ml + ((ms - from) / span) * (W - ml - mr);
  const y = (v) => mt + (1 - Math.min(1, Math.max(0, v / yMax))) * (H - mt - mb);
  const bucketMs = Math.max(1000, (o.bucketSecs || 60) * 1000);
  const gapMs = bucketMs * 2.5;
  const titleId = uid('chart');
  const svg = s('svg', {
    class: 'k-chart',
    viewBox: `0 0 ${W} ${H}`,
    role: 'img',
    'aria-labelledby': titleId,
    preserveAspectRatio: 'none',
  });
  const desc =
    pts.length === 0
      ? t('chart.noData', { label: o.label })
      : t('chart.summary', {
          label: o.label,
          from: fmt.dateTime(o.from, { seconds: false }),
          to: fmt.dateTime(o.to, { seconds: false }),
          min: o.format(Math.min(...pts.map((p) => p.min ?? p.avg))),
          max: o.format(Math.max(...pts.map((p) => p.max ?? p.avg))),
          last: o.format(pts[pts.length - 1].avg),
        });
  svg.appendChild(s('title', { id: titleId }, desc));

  // 格線與 y 軸
  for (let i = 0; i <= 4; i += 1) {
    const v = (yMax / 4) * i;
    svg.appendChild(s('line', { class: 'k-grid-line', x1: ml, x2: W - mr, y1: y(v), y2: y(v) }));
    svg.appendChild(s('text', { x: ml - 6, y: y(v) + 4, 'text-anchor': 'end' }, o.format(v)));
  }
  // x 軸標籤
  for (let i = 0; i <= 4; i += 1) {
    const ms = from + (span / 4) * i;
    const label = span > 2 * 86400000 ? fmt.monthDayTime(ms) : fmt.time(ms, { seconds: false });
    svg.appendChild(s('text', { x: x(ms), y: H - 6, 'text-anchor': i === 0 ? 'start' : i === 4 ? 'end' : 'middle' }, label));
  }
  svg.appendChild(s('line', { class: 'k-axis', x1: ml, x2: W - mr, y1: H - mb, y2: H - mb }));

  // 分段（空缺處斷開）
  const segs = [];
  let cur = [];
  let prev = null;
  for (const p of pts) {
    if (prev && p.ms - prev.ms > gapMs) {
      segs.push(cur);
      cur = [];
    }
    cur.push(p);
    prev = p;
  }
  if (cur.length) segs.push(cur);
  // 空缺：開頭、段與段之間、結尾
  const gaps = [];
  if (pts.length === 0) gaps.push([from, to]);
  else {
    if (pts[0].ms - from > gapMs) gaps.push([from, pts[0].ms]);
    for (let i = 1; i < segs.length; i += 1) gaps.push([segs[i - 1][segs[i - 1].length - 1].ms + bucketMs, segs[i][0].ms]);
    const last = pts[pts.length - 1].ms;
    if (to - last > gapMs) gaps.push([last + bucketMs, to]);
  }
  for (const [a, b] of gaps) {
    if (b <= a) continue;
    svg.appendChild(s('rect', { class: 'k-gap', x: x(a), y: mt, width: Math.max(1, x(b) - x(a)), height: H - mt - mb }));
  }
  const aggregated = pts.some((p) => (p.count || 1) > 1);
  for (const seg of segs) {
    if (aggregated && seg.length > 1) {
      const top = seg.map((p) => `${x(p.ms).toFixed(1)},${y(p.max ?? p.avg).toFixed(1)}`);
      const bottom = seg
        .slice()
        .reverse()
        .map((p) => `${x(p.ms).toFixed(1)},${y(p.min ?? p.avg).toFixed(1)}`);
      svg.appendChild(s('polygon', { class: 'k-band', points: [...top, ...bottom].join(' ') }));
    }
    if (seg.length === 1) {
      svg.appendChild(s('circle', { class: 'k-pt', cx: x(seg[0].ms), cy: y(seg[0].avg), r: 2.5 }));
    } else {
      const d = seg.map((p, i) => `${i ? 'L' : 'M'}${x(p.ms).toFixed(1)} ${y(p.avg).toFixed(1)}`).join(' ');
      svg.appendChild(s('path', { class: 'k-s1', d }));
    }
  }

  // 滑鼠停留：最近的點
  const cursor = s('line', { class: 'k-cursor', y1: mt, y2: H - mb, x1: -10, x2: -10 });
  svg.appendChild(cursor);
  const tip = h('div', { class: 'k-chart-tip', hidden: true });
  const wrapEl = h('div', { class: 'k-chart-wrap' }, svg, tip);
  svg.addEventListener('pointermove', (ev) => {
    if (!pts.length) return;
    const r = svg.getBoundingClientRect();
    const vx = ((ev.clientX - r.left) / r.width) * W;
    const ms = from + ((vx - ml) / (W - ml - mr)) * span;
    let best = pts[0];
    for (const p of pts) if (Math.abs(p.ms - ms) < Math.abs(best.ms - ms)) best = p;
    cursor.setAttribute('x1', x(best.ms));
    cursor.setAttribute('x2', x(best.ms));
    const range = aggregated && best.min != null ? t('chart.range', { min: o.format(best.min), max: o.format(best.max) }) : '';
    tip.textContent = `${fmt.dateTime(best.ms)} ${fmt.tzLabel(best.ms)} · ${o.format(best.avg)}${range}`;
    tip.hidden = false;
    const left = ((x(best.ms) / W) * r.width) + 12;
    tip.style.setProperty('left', `${Math.min(left, r.width - 220)}px`);
    tip.style.setProperty('top', '8px');
    if (o.onCursor) o.onCursor(best.ms);
  });
  svg.addEventListener('pointerleave', () => {
    tip.hidden = true;
    cursor.setAttribute('x1', -10);
    cursor.setAttribute('x2', -10);
  });

  // 表格檢視（最新的 60 點）
  const rows = pts.slice(-60).reverse();
  const tableDetails = h(
    'details',
    { class: 'k-details' },
    h('summary', null, t('chart.asTable')),
    rows.length
      ? h(
          'div',
          { class: 'k-scroll-x' },
          h(
            'table',
            { class: 'k-mini-table' },
            h('caption', { class: 'ds-visually-hidden' }, o.label),
            h(
              'thead',
              null,
              h(
                'tr',
                null,
                h('th', { scope: 'col' }, t('chart.col.time', { tz: fmt.tzLabel() })),
                h('th', { scope: 'col', class: 'num' }, t('chart.col.avg')),
                aggregated ? h('th', { scope: 'col', class: 'num' }, t('chart.col.min')) : null,
                aggregated ? h('th', { scope: 'col', class: 'num' }, t('chart.col.max')) : null,
              ),
            ),
            h(
              'tbody',
              null,
              rows.map((p) =>
                h(
                  'tr',
                  null,
                  h('td', null, fmt.dateTime(p.ms)),
                  h('td', { class: 'num' }, o.format(p.avg)),
                  aggregated ? h('td', { class: 'num' }, o.format(p.min)) : null,
                  aggregated ? h('td', { class: 'num' }, o.format(p.max)) : null,
                ),
              ),
            ),
          ),
        )
      : h('p', { class: 'muted' }, t('chart.noData', { label: o.label })),
  );
  return h('div', null, wrapEl, tableDetails);
}

const SERIES_CLASSES = ['k-s1', 'k-s2', 'k-s3'];

/** 圖例中的線型樣本（顏色之外以實線／虛線／點線區分；design-system/README.md §6.7） */
function legendSwatch(cls) {
  const svg = s('svg', { viewBox: '0 0 28 10', 'aria-hidden': 'true', focusable: 'false', class: 'k-chart' });
  svg.appendChild(s('line', { class: cls, x1: 1, x2: 27, y1: 5, y2: 5 }));
  return svg;
}

/**
 * 多條線的趨勢圖（F4：磁碟讀寫、網路、負載、多個掛載點；設計包 S05「F4 指標擴充」）。最多 3 條線，以
 * 線型＋圖例文字區分（顏色不是唯一線索）；彙總資料只畫平均線（最小、最大見表格）。資料空缺不補 0。
 * 圖可以聚焦，以左右方向鍵（Home／End）逐點檢視，數值寫在圖下方的 aria-live 區；另有「以表格檢視資料」。
 * @param {object} o
 *   label            圖的名稱
 *   from, to         查詢範圍（ISO）
 *   bucketSecs       每點的時間寬度
 *   series           [{ label, points: [{ t, avg, min, max, count, estimated }] }]（最多 3 條）
 *   yMax             固定上限（百分比為 100）；省略時依資料
 *   format(v)        數值的文字
 *   refLines         [{ value, label }]：參考線（--ds-chart-axis 虛線＋文字；不是告警門檻）
 *   onCursor(ms)     游標（滑鼠或方向鍵）移到某個時間時呼叫
 */
export function multiChart(o) {
  const W = 800;
  const H = 220;
  const ml = 60;
  const mr = 12;
  const mt = 10;
  const mb = 26;
  const from = Date.parse(o.from);
  const to = Date.parse(o.to);
  const span = Math.max(1, to - from);
  const series = (o.series || []).slice(0, 3).map((sr, i) => ({
    ...sr,
    cls: SERIES_CLASSES[i],
    pts: (sr.points || [])
      .map((p) => ({ ...p, ms: Date.parse(p.t) }))
      .filter((p) => !Number.isNaN(p.ms) && p.avg != null && !Number.isNaN(p.avg))
      .sort((a, b) => a.ms - b.ms),
  }));
  const all = series.flatMap((sr) => sr.pts);
  const refMax = (o.refLines || []).reduce((m, r) => Math.max(m, r.value), 0);
  const dataMax = all.reduce((m, p) => Math.max(m, p.avg), refMax);
  const yMax = o.yMax != null ? o.yMax : niceMax(dataMax * 1.1);
  const x = (ms) => ml + ((ms - from) / span) * (W - ml - mr);
  const y = (v) => mt + (1 - Math.min(1, Math.max(0, v / yMax))) * (H - mt - mb);
  const bucketMs = Math.max(1000, (o.bucketSecs || 60) * 1000);
  const gapMs = bucketMs * 2.5;
  const titleId = uid('chart');
  const svg = s('svg', { class: 'k-chart', viewBox: `0 0 ${W} ${H}`, role: 'img', 'aria-labelledby': titleId, preserveAspectRatio: 'none', tabindex: '0' });
  const lastText = series
    .filter((sr) => sr.pts.length)
    .map((sr) => `${sr.label} ${o.format(sr.pts[sr.pts.length - 1].avg)}`)
    .join('、');
  const desc = all.length
    ? t('chart.multiSummary', {
        label: o.label,
        from: fmt.dateTime(o.from, { seconds: false }),
        to: fmt.dateTime(o.to, { seconds: false }),
        last: lastText,
      })
    : t('chart.noData', { label: o.label });
  svg.appendChild(s('title', { id: titleId }, desc));
  for (let i = 0; i <= 4; i += 1) {
    const v = (yMax / 4) * i;
    svg.appendChild(s('line', { class: 'k-grid-line', x1: ml, x2: W - mr, y1: y(v), y2: y(v) }));
    svg.appendChild(s('text', { x: ml - 6, y: y(v) + 4, 'text-anchor': 'end' }, o.format(v)));
  }
  for (let i = 0; i <= 4; i += 1) {
    const ms = from + (span / 4) * i;
    const label = span > 2 * 86400000 ? fmt.monthDayTime(ms) : fmt.time(ms, { seconds: false });
    svg.appendChild(s('text', { x: x(ms), y: H - 6, 'text-anchor': i === 0 ? 'start' : i === 4 ? 'end' : 'middle' }, label));
  }
  svg.appendChild(s('line', { class: 'k-axis', x1: ml, x2: W - mr, y1: H - mb, y2: H - mb }));
  for (const r of o.refLines || []) {
    svg.appendChild(s('line', { class: 'k-ref', x1: ml, x2: W - mr, y1: y(r.value), y2: y(r.value) }));
    svg.appendChild(s('text', { x: W - mr - 4, y: y(r.value) - 4, 'text-anchor': 'end' }, r.label));
  }
  // 空缺（所有序列都沒有資料的時段）
  const times = [...new Set(all.map((p) => p.ms))].sort((a, b) => a - b);
  const gaps = [];
  if (!times.length) gaps.push([from, to]);
  else {
    if (times[0] - from > gapMs) gaps.push([from, times[0]]);
    for (let i = 1; i < times.length; i += 1) if (times[i] - times[i - 1] > gapMs) gaps.push([times[i - 1] + bucketMs, times[i]]);
    if (to - times[times.length - 1] > gapMs) gaps.push([times[times.length - 1] + bucketMs, to]);
  }
  for (const [a, b] of gaps) {
    if (b > a) svg.appendChild(s('rect', { class: 'k-gap', x: x(a), y: mt, width: Math.max(1, x(b) - x(a)), height: H - mt - mb }));
  }
  for (const sr of series) {
    let seg = [];
    const flush = () => {
      if (seg.length === 1) svg.appendChild(s('circle', { class: 'k-pt', cx: x(seg[0].ms), cy: y(seg[0].avg), r: 2.5 }));
      else if (seg.length > 1) svg.appendChild(s('path', { class: sr.cls, d: seg.map((p, i) => `${i ? 'L' : 'M'}${x(p.ms).toFixed(1)} ${y(p.avg).toFixed(1)}`).join(' ') }));
      seg = [];
    };
    let prev = null;
    for (const p of sr.pts) {
      if (prev && p.ms - prev.ms > gapMs) flush();
      seg.push(p);
      prev = p;
    }
    flush();
  }
  const cursor = s('line', { class: 'k-cursor', y1: mt, y2: H - mb, x1: -10, x2: -10 });
  svg.appendChild(cursor);
  const tip = h('div', { class: 'k-chart-tip', hidden: true });
  const live = h('p', { class: 'caption', 'aria-live': 'polite' });
  let idx = -1;
  const showAt = (i, rect) => {
    if (!times.length) return;
    idx = Math.max(0, Math.min(times.length - 1, i));
    const ms = times[idx];
    cursor.setAttribute('x1', x(ms));
    cursor.setAttribute('x2', x(ms));
    const vals = series
      .map((sr) => {
        const p = sr.pts.find((q) => q.ms === ms);
        return p ? `${sr.label} ${o.format(p.avg)}${p.estimated ? t('host.estimatedSuffix') : ''}` : null;
      })
      .filter(Boolean)
      .join('、');
    tip.textContent = `${fmt.dateTime(ms)} ${fmt.tzLabel(ms)} · ${vals}`;
    tip.hidden = false;
    const left = (x(ms) / W) * rect.width + 12;
    tip.style.setProperty('left', `${Math.max(0, Math.min(left, rect.width - 220))}px`);
    tip.style.setProperty('top', '8px');
    if (o.onCursor) o.onCursor(ms);
  };
  svg.addEventListener('pointermove', (ev) => {
    if (!times.length) return;
    const r = svg.getBoundingClientRect();
    const ms = from + ((((ev.clientX - r.left) / r.width) * W - ml) / (W - ml - mr)) * span;
    let best = 0;
    for (let i = 0; i < times.length; i += 1) if (Math.abs(times[i] - ms) < Math.abs(times[best] - ms)) best = i;
    showAt(best, r);
  });
  svg.addEventListener('pointerleave', () => {
    tip.hidden = true;
  });
  svg.addEventListener('keydown', (ev) => {
    if (!times.length) return;
    const step = { ArrowLeft: -1, ArrowRight: 1, Home: -Infinity, End: Infinity }[ev.key];
    if (step == null) return;
    ev.preventDefault();
    let next;
    if (step === -Infinity) next = 0;
    else if (step === Infinity) next = times.length - 1;
    else next = idx < 0 ? times.length - 1 : idx + step;
    showAt(next, svg.getBoundingClientRect());
    live.textContent = tip.textContent;
  });
  svg.addEventListener('blur', () => {
    tip.hidden = true;
  });
  const legend = h(
    'div',
    { class: 'k-legend' },
    series.map((sr) => h('span', null, legendSwatch(sr.cls), sr.label)),
    (o.refLines || []).map((r) => h('span', null, legendSwatch('k-ref'), r.label)),
    gaps.length ? h('span', null, h('span', { class: 'k-swatch k-tl-offline' }), t('chart.gapLegend')) : null,
  );
  const rows = times.slice(-60).reverse();
  const tableDetails = h(
    'details',
    { class: 'k-details' },
    h('summary', null, t('chart.asTable')),
    rows.length
      ? h(
          'div',
          { class: 'k-scroll-x' },
          h(
            'table',
            { class: 'k-mini-table' },
            h('caption', { class: 'ds-visually-hidden' }, o.label),
            h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('chart.col.time', { tz: fmt.tzLabel() })), series.map((sr) => h('th', { scope: 'col', class: 'num' }, sr.label)))),
            h(
              'tbody',
              null,
              rows.map((ms) =>
                h(
                  'tr',
                  null,
                  h('td', null, fmt.dateTime(ms)),
                  series.map((sr) => {
                    const p = sr.pts.find((q) => q.ms === ms);
                    return h('td', { class: 'num' }, p ? `${o.format(p.avg)}${p.estimated ? t('host.estimatedSuffix') : ''}` : '—');
                  }),
                ),
              ),
            ),
          ),
        )
      : h('p', { class: 'muted' }, t('chart.noData', { label: o.label })),
  );
  return h('div', null, legend, h('div', { class: 'k-chart-wrap' }, svg, tip), live, tableDetails);
}
