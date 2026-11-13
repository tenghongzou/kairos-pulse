// 遠端管理的全域部分（main.js 載入一次）：側邊導覽「升級推送」的計數泡泡（設計包 ia.md §3、§7；§31.5）。
//
// - 計數＝已暫停的推送＋回滾失敗、尚未標記為已處理的主機＋目前使用者可以核准的待核准推送（需要人處理的數量）：
//   GET /api/v1/rollouts 的 meta.paused＋meta.rollback_failed_unacknowledged＋meta.pending_approval_actionable
//   （伺服器計算，不受篩選影響）。
// - 有 rollouts:read 時每 30 秒更新一次；收到 SSE rollout.progress 時（2 秒內合併）與 S26／S05 的操作後
//   （refreshRolloutCount）立即更新。功能關閉（503）或其他錯誤時不顯示泡泡。

import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import { setNavCount } from '../ui/shell.js';
import { registerProvider } from '../ui/search.js';

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
    // 雙人覆核（0.10，ADR-0002 決策點 2a）：只計「目前使用者可以核准的」待核准推送——由後端依這次請求的身分
    // 計算（meta.pending_approval_actionable：有 rollouts:approve、以帳號登入、不是建立者或送出者），主控台不以
    // 帳號字串比對。可存取名稱寫出組成：「3 項需要處理：2 個等你核准、1 個已暫停」
    const approve = meta.pending_approval_actionable || 0;
    const paused = meta.paused || 0;
    const failed = meta.rollback_failed_unacknowledged || 0;
    const n = approve + paused + failed;
    if (my !== seq) return;
    const parts = [approve ? t('nav.badge.rolloutsApprove', { n: approve }) : null, paused ? t('nav.badge.rolloutsPaused', { n: paused }) : null, failed ? t('nav.badge.rolloutsFailed', { n: failed }) : null].filter(Boolean);
    setNavCount('rollouts', n ? { n, tone: 'warning', label: t('nav.badge.rolloutsParts', { n, parts: parts.join(t('nav.badge.rolloutsSep')) }) } : null);
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

// 全域搜尋（Ctrl+K；設計包 ia.md §2）：升級推送（名稱、編號、目標版本；需要 rollouts:read）與設定檔（名稱、說明；
// 需要 config:read）。清單端點的 q 由伺服器篩選；功能關閉（503）時這一組顯示錯誤訊息，不影響主機的結果。
const SEARCH_MAX = 6;
registerProvider({
  id: 'rollouts',
  group: () => t('search.group.rollouts'),
  allowed: () => session.can('rollouts:read'),
  async search(q, signal) {
    const [rows, common] = await Promise.all([api(`/api/v1/rollouts${qs({ q })}`, { signal }), import('./pulse-common.js')]);
    return rows.slice(0, SEARCH_MAX).map((r) => ({
      text: r.name,
      sub: [r.id, t('search.rolloutTarget', { v: r.target_version })].join(' · '),
      href: `/pulse/rollouts/${encodeURIComponent(r.id)}`,
      badge: common.rolloutStateBadge(r.state),
    }));
  },
});
registerProvider({
  id: 'profiles',
  group: () => t('search.group.profiles'),
  allowed: () => session.can('config:read'),
  async search(q, signal) {
    const rows = await api(`/api/v1/config/profiles${qs({ q })}`, { signal });
    return rows.slice(0, SEARCH_MAX).map((p) => ({
      text: p.name,
      sub: [p.description, t('search.profileHosts', { n: p.hosts || 0 })].filter(Boolean).join(' · '),
      href: `/pulse/config/${encodeURIComponent(p.id)}`,
    }));
  },
});

session.onChange((me) => {
  if (me && session.can('rollouts:read')) start();
  else stop();
});
