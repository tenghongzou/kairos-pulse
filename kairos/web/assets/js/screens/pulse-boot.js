// 遠端管理的全域部分（main.js 載入一次）：側邊導覽「升級推送」的計數泡泡（設計包 ia.md §3、§7；§31.5）。
//
// - 計數＝已暫停的推送＋回滾失敗、尚未標記為已處理的主機（需要人處理的數量）。
// - 有 rollouts:read 時每 30 秒更新一次；收到 SSE rollout.progress 時（2 秒內合併）與 S26／S05 的操作後
//   （refreshRolloutCount）立即更新。功能關閉（503）或其他錯誤時不顯示泡泡。

import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
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
    const list = (await api('/api/v1/rollouts')) || [];
    let n = list.filter((r) => r.state === 'paused').length;
    // 回滾失敗：只算尚未標記為已處理的主機（progress 的計數含已處理的，逐一確認）
    for (const r of list.filter((x) => x.progress && x.progress.rollback_failed)) {
      const hosts = await api(`/api/v1/rollouts/${encodeURIComponent(r.id)}/hosts${qs({ state: 'rollback_failed', limit: 1000 })}`);
      n += (hosts || []).filter((h) => !h.acknowledged_at).length;
    }
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
