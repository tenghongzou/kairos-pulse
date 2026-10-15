// 註冊的全域部分（main.js 載入一次）：側邊導覽「待核准」的計數泡泡（設計包 ia.md §7）。
//
// - 計數＝待核准的申請數（GET /api/v1/enroll/requests?view=pending&state=pending_approval&limit=1 的 page.total）；
//   已逾期的申請不在待核准檢視中，已核准、待簽發的不算（不需要人處理）。
// - 有 enroll:read 時每 30 秒更新一次；S13 核准或拒絕後立即更新（refreshPendingCount）。

import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import { setNavCount } from '../ui/shell.js';

let poll = null;
let seq = 0;

/** 重新計算待核准數（S13 的操作後呼叫） */
export async function refreshPendingCount() {
  const my = ++seq;
  if (!session.isLoggedIn() || !session.can('enroll:read')) {
    setNavCount('pending', null);
    return;
  }
  try {
    const r = await api(`/api/v1/enroll/requests${qs({ view: 'pending', state: 'pending_approval', limit: 1 })}`, { raw: true });
    if (my !== seq) return;
    const n = (r.page && r.page.total) || 0;
    setNavCount('pending', n ? { n, tone: 'pending', label: t('nav.badge.pending', { n }) } : null);
  } catch {
    // 註冊服務沒有啟動（503）等：不顯示泡泡，不把錯誤當成 0 以外的數字
    if (my === seq) setNavCount('pending', null);
  }
}

function start() {
  if (poll) return;
  poll = setInterval(refreshPendingCount, 30000);
  refreshPendingCount();
}

function stop() {
  clearInterval(poll);
  poll = null;
  seq++;
  setNavCount('pending', null);
}

session.onChange((me) => {
  if (me && session.can('enroll:read')) start();
  else stop();
});
