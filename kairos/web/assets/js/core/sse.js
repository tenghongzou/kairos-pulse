// 即時變更推送（SSE，GET /api/v1/events；kairos/API.md 第 6 節）的管理。
//
// - 整個主控台共用一條 EventSource（cookie 驗證），有訂閱者時才開啟，沒有時關閉（console.max_streams 預設 16）。
// - 事件：ready、resync、host.added／host.online／host.offline、alert.*；畫面以 subscribe(fn) 接收。
// - 重連：瀏覽器的 EventSource 會依 `retry:` 自動重連並帶 Last-Event-ID；但伺服器回非 200（401、503
//   too_many_streams）或主動關閉時 EventSource 不再重連，這時由這裡以退避（5～60 秒）重新開啟，
//   並先以 GET /api/v1/me 確認身分：401 → 回登入頁；權限改變 → 外框重畫（session.onChange）。
// - 伺服器每 15 秒的心跳也會重新計算權限，沒有讀取主機的權限時關閉串流（API.md 第 12.3 節）。
// - 狀態（connected）供頁首的「與 Kairos 伺服器的連線中斷」橫幅使用。

import * as session from './session.js';

const handlers = new Set();
const stateListeners = new Set();
let es = null;
let lastEventId = null;
let retryTimer = null;
let backoffMs = 5000;
let connected = false;
let enabled = true;

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
  for (const fn of handlers) {
    try {
      fn({ type, id: ev.lastEventId || null, data });
    } catch (e) {
      console.error(e);
    }
  }
}

const TYPES = ['ready', 'resync', 'host.added', 'host.online', 'host.offline', 'alert.created', 'alert.updated', 'alert.resolved'];

function open() {
  if (es || !enabled || handlers.size === 0) return;
  const url = lastEventId ? `/api/v1/events?last_event_id=${encodeURIComponent(lastEventId)}` : '/api/v1/events';
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

/** 連線狀態改變（true：已連上） */
export function onState(fn) {
  stateListeners.add(fn);
  return () => stateListeners.delete(fn);
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
