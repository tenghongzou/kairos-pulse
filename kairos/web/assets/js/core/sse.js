// 即時變更推送（SSE，GET /api/v1/events；kairos/API.md 第 6 節）的管理。
//
// - 整個主控台共用一條 EventSource（cookie 驗證），有訂閱者時才開啟，沒有時關閉（console.max_streams 預設 16）。
// - 事件：ready、resync、host.added／host.online／host.offline、alert.*；畫面以 subscribe(fn) 接收。
// - 重連：瀏覽器的 EventSource 會依 `retry:` 自動重連並帶 Last-Event-ID；但伺服器回非 200（401、503
//   too_many_streams）或主動關閉時 EventSource 不再重連，這時由這裡以退避（5～60 秒）重新開啟，
//   並先以 GET /api/v1/me 確認身分：401 → 回登入頁；權限改變 → 外框重畫（session.onChange）。
// - 伺服器每 15 秒的心跳也會重新計算權限，沒有讀取主機的權限時關閉串流（API.md 第 12.3 節）。
// - 狀態（connected）供頁首的「與 Kairos 伺服器的連線中斷」橫幅使用。
// - resync（screens.md §0.2）：onResync(fn) 收到原因（restarted／failover／too_old／其他）供外框顯示 Banner；
//   畫面重新抓取完清單後呼叫 resyncSettled()，外框在 3 秒後移除 Banner。onResync 不算訂閱者（不會開啟串流）。

import * as session from './session.js';

const handlers = new Set();
const stateListeners = new Set();
const resyncListeners = new Set();
const settleListeners = new Set();
let es = null;
let lastEventId = null;
let retryTimer = null;
let backoffMs = 5000;
let connected = false;
let enabled = true;
let watchedRollout = null;

function setConnected(v) {
  if (v !== connected) {
    connected = v;
    for (const fn of stateListeners) fn(v);
  }
}

function dispatch(type, ev) {
  let data = null;
  try {
    data = ev.data ? JSON.parse(ev.data) : null;
  } catch {
    data = null;
  }
  if (ev.lastEventId) lastEventId = ev.lastEventId;
  if (type === 'resync') {
    const reason = (data && data.reason) || null;
    for (const fn of resyncListeners) {
      try {
        fn(reason);
      } catch (e) {
        console.error(e);
      }
    }
  }
  for (const fn of handlers) {
    try {
      fn({ type, id: ev.lastEventId || null, data });
    } catch (e) {
      console.error(e);
    }
  }
}

// 0.9：遠端管理的 config.host（需要 config:read）、rollout.progress（需要 rollouts:read；API.md 第 17.5 節）
// 0.10：rollout.host（單台主機的升級狀態；只送給以 ?rollout_id= 訂閱那個推送的連線，見 watchRollout）
const TYPES = ['ready', 'resync', 'host.added', 'host.online', 'host.offline', 'alert.created', 'alert.updated', 'alert.resolved', 'config.host', 'rollout.progress', 'rollout.host'];

function open() {
  if (es || !enabled || handlers.size === 0) return;
  const params = [];
  if (lastEventId) params.push(`last_event_id=${encodeURIComponent(lastEventId)}`);
  if (watchedRollout) params.push(`rollout_id=${encodeURIComponent(watchedRollout)}`);
  const url = params.length ? `/api/v1/events?${params.join('&')}` : '/api/v1/events';
  es = new EventSource(url, { withCredentials: true });
  for (const type of TYPES) {
    es.addEventListener(type, (ev) => {
      if (type === 'ready') {
        backoffMs = 5000;
        setConnected(true);
      }
      dispatch(type, ev);
    });
  }
  es.onerror = () => {
    if (!es) return;
    if (es.readyState === EventSource.CLOSED) {
      // 伺服器拒絕（401、503）或關閉了串流：自己重連
      es = null;
      setConnected(false);
      scheduleReconnect();
    } else {
      // CONNECTING：瀏覽器會自動重連
      setConnected(false);
    }
  };
}

function scheduleReconnect() {
  if (retryTimer || handlers.size === 0) return;
  const wait = backoffMs;
  backoffMs = Math.min(backoffMs * 2, 60000);
  retryTimer = setTimeout(async () => {
    retryTimer = null;
    // 先確認身分：工作階段結束 → session 會通知 main.js 回登入頁；權限可能已改變
    const still = await session.refresh();
    if (!still) return;
    if (!session.canAny('hosts:read')) return;
    open();
  }, wait);
}

/** 訂閱事件；回傳取消訂閱的函式 */
export function subscribe(fn) {
  handlers.add(fn);
  open();
  return () => {
    handlers.delete(fn);
    if (handlers.size === 0) close();
  };
}

/**
 * 訂閱一個推送的 rollout.host（S26 推送詳情）：整個主控台共用一條串流，所以以 ?rollout_id= 重新開啟它
 * （帶 last_event_id 接續，不會漏掉事件）。回傳取消的函式；同一時間只有一個畫面訂閱。
 */
export function watchRollout(id) {
  const reopen = () => {
    if (!es) return;
    es.close();
    es = null;
    open();
  };
  watchedRollout = id;
  reopen();
  return () => {
    if (watchedRollout !== id) return;
    watchedRollout = null;
    reopen();
  };
}

/** 連線狀態改變（true：已連上） */
export function onState(fn) {
  stateListeners.add(fn);
  return () => stateListeners.delete(fn);
}

/** 收到 resync（參數為原因）；回傳取消的函式。不會開啟串流 */
export function onResync(fn) {
  resyncListeners.add(fn);
  return () => resyncListeners.delete(fn);
}

/** 畫面因 resync 重新抓取完成（外框據此在 3 秒後移除 Banner） */
export function resyncSettled() {
  for (const fn of settleListeners) fn();
}

/** resyncSettled() 的通知；回傳取消的函式 */
export function onResyncSettled(fn) {
  settleListeners.add(fn);
  return () => settleListeners.delete(fn);
}

export function isConnected() {
  return connected;
}

/** 最新的事件編號（列表的 meta.last_event_id 之後的事件才套用；API.md 第 6 節） */
export function lastId() {
  return lastEventId;
}

/** 關閉（登出、沒有訂閱者） */
export function close() {
  if (retryTimer) {
    clearTimeout(retryTimer);
    retryTimer = null;
  }
  if (es) {
    es.close();
    es = null;
  }
  setConnected(false);
}

/** 登出時停用，登入後啟用 */
export function setEnabled(v) {
  enabled = v;
  if (!v) {
    close();
    lastEventId = null;
  } else {
    open();
  }
}

/** 比較兩個事件編號（`<epoch>-<序號>`）：a 是否比 b 新；不同 epoch 一律視為較新 */
export function isNewer(a, b) {
  if (!a) return false;
  if (!b) return true;
  const [ea, sa] = a.split('-');
  const [eb, sb] = b.split('-');
  if (ea !== eb) return true;
  return Number(sa) > Number(sb);
}
