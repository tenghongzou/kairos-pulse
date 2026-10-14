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
