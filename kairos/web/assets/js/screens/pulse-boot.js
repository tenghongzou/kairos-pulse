// 遠端管理的全域部分（main.js 載入一次）：側邊導覽「升級推送」的計數泡泡（設計包 ia.md §3、§7；§31.5）。
//
// - 計數＝已暫停的推送＋回滾失敗、尚未標記為已處理的主機（需要人處理的數量）：GET /api/v1/rollouts 的
//   meta.paused＋meta.rollback_failed_unacknowledged（伺服器計算，不受篩選影響）。
// - 有 rollouts:read 時每 30 秒更新一次；收到 SSE rollout.progress 時（2 秒內合併）與 S26／S05 的操作後
//   （refreshRolloutCount）立即更新。功能關閉（503）或其他錯誤時不顯示泡泡。

import { t } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import { setNavCount } from '../ui/shell.js';

let poll = null;
let seq = 0;
let unsub = null;
let soon = null;

/** 重新計算（暫停、繼續、取消、標記為已處理之後呼叫） */
export async function refreshRolloutCount() {
  const my = ++seq;
  if (!session.isLoggedIn() || !session.can('rollouts:read')) {
    setNavCount('rollouts', null);
    return;
  }
  try {
    // 推送清單的 meta 直接給總數（已暫停的推送、回滾失敗且尚未標記為已處理的主機），不必逐一查主機
    const res = await api('/api/v1/rollouts', { raw: true });
    const meta = (res && res.meta) || {};
    const n = (meta.paused || 0) + (meta.rollback_failed_unacknowledged || 0);
    if (my !== seq) return;
    setNavCount('rollouts', n ? { n, tone: 'warning', label: t('nav.badge.rollouts', { n }) } : null);
  } catch {
    if (my === seq) setNavCount('rollouts', null);
  }
}

function start() {
  if (poll) return;
  poll = setInterval(refreshRolloutCount, 30000);
  unsub = sse.subscribe((ev) => {
    if (ev.type !== 'rollout.progress' && ev.type !== 'resync') return;
    clearTimeout(soon);
    soon = setTimeout(refreshRolloutCount, 2000);
  });
  refreshRolloutCount();
}

function stop() {
  clearInterval(poll);
  poll = null;
  clearTimeout(soon);
  if (unsub) unsub();
  unsub = null;
  seq++;
  setNavCount('rollouts', null);
}

session.onChange((me) => {
  if (me && session.can('rollouts:read')) start();
  else stop();
});
