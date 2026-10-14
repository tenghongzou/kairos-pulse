// 主控台 API 的用戶端（kairos/API.md）。
//
// - 同源、以工作階段的 cookie 驗證（`__Host-kairos_session`，HttpOnly：JavaScript 讀不到）。
// - 寫入（POST／PUT／PATCH／DELETE）帶 `X-CSRF-Token`（登入回應與 GET /api/v1/me 的 csrf_token，只存在記憶體）。
// - 每個回應的 server_time 校正時鐘（format.syncServerTime）。
// - 錯誤一律丟出 ApiError（code、productCode、details、status）；畫面以 errors.js 的三段式呈現。
// - 401（unauthorized）：工作階段已結束 → 交給 onUnauthorized（帶 next 回登入頁，§28.2 D39）。
// - 403 password_change_required：交給 onPasswordChange（回登入頁的「變更密碼」步驟）。
// - 403 forbidden：由畫面顯示「無權限」（不自動跳頁）。
// - 連不上伺服器：code = "network"，並通知 onConnectivity(false)（頁首顯示「連線中斷」橫幅）。

import { syncServerTime } from './format.js';

export class ApiError extends Error {
  constructor({ status = 0, code = 'internal', productCode = null, message = '', details = null, retryAfter = null }) {
    super(message || code);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
    this.productCode = productCode;
    this.details = details;
    this.retryAfter = retryAfter;
  }
}

let csrfToken = null;
const hooks = {
  onUnauthorized: null,
  onPasswordChange: null,
  onConnectivity: null,
};

/** 設定 CSRF token（只放在記憶體；重新整理後由 /api/v1/me 取回） */
export function setCsrf(token) {
  csrfToken = token || null;
}

/** 註冊全域處理（main.js） */
export function setHooks(h) {
  Object.assign(hooks, h);
}

let online = true;
function connectivity(ok) {
  if (ok !== online) {
    online = ok;
    if (hooks.onConnectivity) hooks.onConnectivity(ok);
  }
}

export function isOnline() {
  return online;
}

/**
 * 呼叫 API。
 * @param {string} path  `/api/v1/...`（含查詢字串）
 * @param {object} opts  method、body（物件，會轉成 JSON）、signal、
 *                       auth（false：401 不跳登入頁，例如登入本身）、raw（回傳整個 JSON 而不只 data）
 */
export async function api(path, { method = 'GET', body, signal, auth = true, raw = false } = {}) {
  const headers = { Accept: 'application/json' };
  const unsafe = !['GET', 'HEAD'].includes(method);
  if (unsafe) {
    headers['Content-Type'] = 'application/json';
    if (csrfToken) headers['X-CSRF-Token'] = csrfToken;
  }
  let resp;
  try {
    resp = await fetch(path, {
      method,
      headers,
      body: unsafe ? JSON.stringify(body ?? {}) : undefined,
      credentials: 'same-origin',
      cache: 'no-store',
      redirect: 'error',
      signal,
    });
  } catch (e) {
    if (e && e.name === 'AbortError') throw e;
    connectivity(false);
    throw new ApiError({ code: 'network', message: String(e && e.message) });
  }
  // 伺服器回應了（即使是錯誤）：連線正常
  connectivity(resp.status !== 502 && resp.status !== 504);
  let json = null;
  const type = resp.headers.get('content-type') || '';
  if (type.includes('application/json')) {
    try {
      json = await resp.json();
    } catch {
      json = null;
    }
  }
  if (json && json.server_time) syncServerTime(json.server_time);
  if (resp.ok) {
    return raw ? json : json ? json.data : null;
  }
  const e = (json && json.error) || {};
  const retry = resp.headers.get('retry-after');
  const err = new ApiError({
    status: resp.status,
    code: e.code || (resp.status === 404 ? 'not_found' : 'internal'),
    productCode: e.product_code || null,
    message: e.message || resp.statusText,
    details: e.details || null,
    retryAfter: retry ? Number(retry) : null,
  });
  if (auth && err.status === 401 && err.code === 'unauthorized' && hooks.onUnauthorized) {
    hooks.onUnauthorized(err);
  }
  if (auth && err.code === 'password_change_required' && hooks.onPasswordChange) {
    hooks.onPasswordChange(err);
  }
  throw err;
}

/** 組查詢字串（略過空值；陣列以逗號分隔，API.md 第 5.1 節） */
export function qs(params) {
  const sp = new URLSearchParams();
  for (const [k, v] of Object.entries(params || {})) {
    if (v == null || v === '' || (Array.isArray(v) && v.length === 0)) continue;
    sp.set(k, Array.isArray(v) ? v.join(',') : String(v));
  }
  const s = sp.toString();
  return s ? `?${s}` : '';
}
