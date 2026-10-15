// 告警的全域部分（main.js 載入一次）：側邊導覽「告警事件」的計數泡泡（ia.md §7）與全域搜尋的事件提供者。
//
// - 計數＝觸發中、未確認、未靜音、需要處理的頂層事件（API meta.counts.badge）；其中有觸發中的緊急事件時泡泡改用
//   緊急樣式，可存取名稱寫出「其中 N 筆緊急」。
// - 有 alerts:read 時才訂閱 SSE 的 alert.*（最多每 3 秒重新計算一次），另外每 60 秒更新一次。

import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import { setNavCount } from '../ui/shell.js';
import { registerProvider } from '../ui/search.js';
import { sevBadge } from './alerts-common.js';

let unsub = null;
let poll = null;
let pending = null;

async function refresh() {
  if (!session.isLoggedIn() || !session.can('alerts:read')) return;
  try {
    const [s, em] = await Promise.all([
      api('/api/v1/alerts/summary'),
      api(`/api/v1/alerts${qs({ view: 'active', state: 'firing', severity: 'emergency', limit: 1 })}`, { raw: true }).then((r) => r.page.total, () => 0),
    ]);
    const n = (s.counts && s.counts.badge) || 0;
    const emergency = Math.min(em, n);
    setNavCount('alerts', n ? { n, tone: emergency ? 'emergency' : 'critical', label: emergency ? t('alerts.badge.labelEmergency', { n, e: emergency }) : t('alerts.badge.label', { n }) } : null);
  } catch {
    setNavCount('alerts', null);
  }
}

function schedule() {
  if (pending) return;
  pending = setTimeout(() => {
    pending = null;
    refresh();
  }, 3000);
}

function start() {
  if (unsub) return;
  unsub = sse.subscribe((ev) => {
    if (ev.type.startsWith('alert.') || ev.type === 'resync') schedule();
  });
  poll = setInterval(refresh, 60000);
  refresh();
}

function stop() {
  if (unsub) unsub();
  unsub = null;
  clearInterval(poll);
  poll = null;
  clearTimeout(pending);
  pending = null;
  setNavCount('alerts', null);
}

session.onChange((me) => {
  if (me && session.can('alerts:read')) start();
  else stop();
});

// 全域搜尋：事件編號、標題、對象（GET /api/v1/alerts?view=all&q=）
registerProvider({
  id: 'alerts',
  group: () => t('search.group.alerts'),
  allowed: () => session.can('alerts:read'),
  async search(q, signal) {
    const rows = await api(`/api/v1/alerts${qs({ view: 'all', q, limit: 6 })}`, { signal });
    return rows.map((e) => ({
      text: e.title,
      sub: [e.event_id, t(`alerts.state.${e.state}`)].join(' · '),
      href: `/alerts/events/${encodeURIComponent(e.event_id)}`,
      badge: sevBadge(e.severity),
    }));
  },
  more: (q) => ({ text: t('search.moreAlerts', { q }), href: `/alerts/events${qs({ view: 'all', q })}` }),
});
