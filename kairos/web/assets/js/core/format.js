// 時間、數字、持續時間、容量的格式（設計包 design-system/copywriting.md §5、§6）。
// - 顯示時區：個人偏好（prefs.timeZone，預設 Asia/Taipei），不自動採用瀏覽器時區（§5.1）。
// - 「現在」以伺服器時間為準：每個 API 回應的 server_time 校正瀏覽器時鐘（API.md 第 1 節）。
// - 相對時間每 30 秒更新一次（§5.3；startRelativeTicker）。

import * as prefs from './prefs.js';
import { h } from './dom.js';
import { t } from './strings.js';

let clockOffsetMs = 0;

/** 以 API 回應的 server_time 校正（伺服器時間 − 瀏覽器時間） */
export function syncServerTime(serverTime) {
  const ms = Date.parse(serverTime);
  if (!Number.isNaN(ms)) clockOffsetMs = ms - Date.now();
}

/** 伺服器的「現在」（毫秒） */
export function serverNow() {
  return Date.now() + clockOffsetMs;
}

function toMs(v) {
  if (v == null) return null;
  if (typeof v === 'number') return v;
  const ms = Date.parse(v);
  return Number.isNaN(ms) ? null : ms;
}

const partsCache = new Map();
function formatter(tz) {
  let f = partsCache.get(tz);
  if (!f) {
    try {
      f = new Intl.DateTimeFormat('en-US', {
        timeZone: tz,
        year: 'numeric',
        month: '2-digit',
        day: '2-digit',
        hour: '2-digit',
        minute: '2-digit',
        second: '2-digit',
        hourCycle: 'h23',
        weekday: 'short',
      });
    } catch {
      f = formatter('Asia/Taipei');
    }
    partsCache.set(tz, f);
  }
  return f;
}

function parts(ms, tz = prefs.get('timeZone')) {
  const out = {};
  for (const p of formatter(tz).formatToParts(new Date(ms))) out[p.type] = p.value;
  return out;
}

/** 時區相對 UTC 的位移（分鐘） */
export function tzOffsetMinutes(ms = serverNow(), tz = prefs.get('timeZone')) {
  const p = parts(ms, tz);
  const asUtc = Date.UTC(+p.year, +p.month - 1, +p.day, +p.hour, +p.minute, +p.second);
  return Math.round((asUtc - Math.floor(ms / 1000) * 1000) / 60000);
}

/** `UTC+8`、`UTC-3:30`（§5.1） */
export function tzLabel(ms = serverNow()) {
  const off = tzOffsetMinutes(ms);
  const sign = off < 0 ? '-' : '+';
  const a = Math.abs(off);
  const hh = Math.floor(a / 60);
  const mm = a % 60;
  return `UTC${sign}${hh}${mm ? `:${String(mm).padStart(2, '0')}` : ''}`;
}

/** `2026-10-09 14:03:27`（§5.2；顯示時區） */
export function dateTime(v, { seconds = true } = {}) {
  const ms = toMs(v);
  if (ms == null) return '—';
  const p = parts(ms);
  return `${p.year}-${p.month}-${p.day} ${p.hour}:${p.minute}${seconds ? `:${p.second}` : ''}`;
}

/** `2026-10-09` */
export function date(v) {
  const ms = toMs(v);
  if (ms == null) return '—';
  const p = parts(ms);
  return `${p.year}-${p.month}-${p.day}`;
}

/** `14:03:27`；圖表軸可省秒 */
export function time(v, { seconds = true } = {}) {
  const ms = toMs(v);
  if (ms == null) return '—';
  const p = parts(ms);
  return `${p.hour}:${p.minute}${seconds ? `:${p.second}` : ''}`;
}

/** `10-09 14:03`（圖表的軸與提示） */
export function monthDayTime(v) {
  const ms = toMs(v);
  if (ms == null) return '—';
  const p = parts(ms);
  return `${p.month}-${p.day} ${p.hour}:${p.minute}`;
}

/** UTC 原值：`2026-10-09 06:03:27 UTC`（Tooltip） */
export function utc(v) {
  const ms = toMs(v);
  if (ms == null) return '—';
  return `${new Date(ms).toISOString().replace('T', ' ').replace(/\.\d+Z$/, '')} UTC`;
}

/** 持續時間（§5.4：最多兩個單位） */
export function duration(secs) {
  if (secs == null || Number.isNaN(secs)) return '—';
  let s = Math.max(0, Math.floor(secs));
  if (s < 60) return t('time.secs', { n: s });
  const d = Math.floor(s / 86400);
  s -= d * 86400;
  const hr = Math.floor(s / 3600);
  s -= hr * 3600;
  const m = Math.floor(s / 60);
  const sec = s - m * 60;
  if (d > 0) return hr ? t('time.daysHours', { d, h: hr }) : t('time.days', { d });
  if (hr > 0) return m ? t('time.hoursMins', { h: hr, m }) : t('time.hours', { h: hr });
  return sec ? t('time.minsSecs', { m, s: sec }) : t('time.mins', { m });
}

/** 相對時間（§5.3）：12 秒前、3 分鐘前、2 小時 15 分前、3 天前；7 天以上改用絕對時間 */
export function relative(v, now = serverNow()) {
  const ms = toMs(v);
  if (ms == null) return '—';
  const diff = Math.round((now - ms) / 1000);
  if (diff < 0) {
    const a = -diff;
    if (a < 60) return t('time.inSecs', { n: a });
    if (a < 3600) return t('time.inMins', { n: Math.floor(a / 60) });
    if (a < 86400) return t('time.inHours', { n: Math.floor(a / 3600) });
    if (a < 7 * 86400) return t('time.inDays', { n: Math.floor(a / 86400) });
    return dateTime(ms, { seconds: false });
  }
  if (diff < 60) return t('time.secsAgo', { n: diff });
  if (diff < 3600) return t('time.minsAgo', { n: Math.floor(diff / 60) });
  if (diff < 86400) {
    const hr = Math.floor(diff / 3600);
    const m = Math.floor((diff % 3600) / 60);
    return m ? t('time.hoursMinsAgo', { h: hr, m }) : t('time.hoursAgo', { h: hr });
  }
  if (diff < 7 * 86400) return t('time.daysAgo', { n: Math.floor(diff / 86400) });
  return dateTime(ms, { seconds: false });
}

/**
 * `<time>` 元素：預設相對時間，title 為絕對時間（顯示時區）與 UTC（§5.3）。
 * mode：relative（依偏好 timeDisplay）、absolute
 */
export function timeEl(v, { mode } = {}) {
  const ms = toMs(v);
  if (ms == null) return h('span', { class: 'muted' }, '—');
  const iso = new Date(ms).toISOString();
  const abs = `${dateTime(ms)} ${tzLabel(ms)}`;
  const m = mode || prefs.get('timeDisplay');
  const el = h('time', { datetime: iso, title: `${abs}（${utc(ms)}）` }, m === 'absolute' ? dateTime(ms) : relative(ms));
  if (m !== 'absolute') el.dataset.rel = '1';
  return el;
}

let ticker = null;
/** 每 30 秒更新畫面上所有相對時間（不逐秒跳動，避免干擾螢幕報讀；§5.3） */
export function startRelativeTicker() {
  if (ticker) return;
  ticker = setInterval(() => {
    for (const el of document.querySelectorAll('time[data-rel]')) {
      el.textContent = relative(el.getAttribute('datetime'));
    }
  }, 30000);
}

/** 千分位（§6） */
export function num(n) {
  if (n == null || Number.isNaN(n)) return '—';
  return Number(n).toLocaleString('en-US');
}

/** 百分比：小數 1 位，0 與 100 不帶小數（§6） */
export function pct(n) {
  if (n == null || Number.isNaN(n)) return '—';
  const r = Math.round(n * 10) / 10;
  return `${r === 0 || r === 100 ? Math.round(r) : r.toFixed(1)}%`;
}

/** 容量：十進位 SI，≥ 1 GB 小數 1 位，以下整數 MB（§6） */
export function bytes(n) {
  if (n == null || Number.isNaN(n)) return '—';
  if (n >= 1e12) return `${(n / 1e12).toFixed(1)} TB`;
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${Math.round(n / 1e6)} MB`;
  if (n >= 1e3) return `${Math.round(n / 1e3)} KB`;
  return `${n} B`;
}

/** 記憶體與置換空間：二進位（§6 的例外，F4）：≥ 1 GiB 整數不帶小數、其餘小數 1 位；以下整數 MiB */
export function gib(n) {
  if (n == null || Number.isNaN(n)) return '—';
  const g = n / 1024 ** 3;
  if (g >= 1) {
    const r = Math.round(g * 10) / 10;
    return `${Number.isInteger(r) ? r : r.toFixed(1)} GiB`;
  }
  return `${Math.round(n / 1024 ** 2)} MiB`;
}

/** 磁碟讀寫速率（bytes／秒 → 十進位 KB/s、MB/s、GB/s；§6，F4） */
export function rateBytes(bps) {
  if (bps == null || Number.isNaN(bps)) return '—';
  if (bps >= 1e9) return `${(bps / 1e9).toFixed(1)} GB/s`;
  if (bps >= 1e6) return `${(bps / 1e6).toFixed(1)} MB/s`;
  return `${Math.round(bps / 1e3)} KB/s`;
}

/** 網路速率（bytes／秒 → bit：kbps、Mbps、Gbps；§6） */
export function rateBits(bps) {
  if (bps == null || Number.isNaN(bps)) return '—';
  const bits = bps * 8;
  if (bits >= 1e9) return `${(bits / 1e9).toFixed(1)} Gbps`;
  if (bits >= 1e6) return `${(bits / 1e6).toFixed(1)} Mbps`;
  return `${Math.round(bits / 1e3)} kbps`;
}

/** 負載：小數 2 位、沒有單位 */
export function load(v) {
  if (v == null || Number.isNaN(v)) return '—';
  return Number(v).toFixed(2);
}

/** 數字＋中文單位（「37 台」） */
export function count(n, unit) {
  return `${num(n)} ${unit}`;
}
