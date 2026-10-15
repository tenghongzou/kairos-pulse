// 目前的身分與權限（GET /api/v1/me；kairos/API.md 第 12.1、12.4 節）。
//
// - 權限名稱是 API 的穩定識別字（`hosts:read`、`alerts:write`…），畫面只依它決定顯示什麼：
//   選單、按鈕、分頁都以 can()／canAny() 判斷（設計包 ia.md §4.2；無權限的動作隱藏，screens.md §0.2）。
// - `permissions` 是全站範圍的權限；`group_permissions` 是群組範圍（只授予該群組主機的讀取，§12.1）。
// - 權限可能在使用中改變（管理員調整角色、SSE 斷線重連時）：refresh() 重新讀取，onChange 通知外框重畫。

import { api, setCsrf } from './api.js';

let me = null;
let perms = new Set();
let anyScopePerms = new Set();
const listeners = new Set();

function signature(m) {
  if (!m) return '';
  return JSON.stringify([m.permissions, m.group_permissions, m.user && m.user.username]);
}

function apply(m) {
  const before = signature(me);
  me = m;
  perms = new Set((m && m.permissions) || []);
  anyScopePerms = new Set(perms);
  for (const list of Object.values((m && m.group_permissions) || {})) {
    for (const p of list) anyScopePerms.add(p);
  }
  setCsrf(m && m.csrf_token);
  if (before !== signature(m)) for (const fn of listeners) fn(me);
}

/** 讀取目前的身分；未登入時回 null（不跳頁） */
export async function load() {
  try {
    const m = await api('/api/v1/me', { auth: false });
    apply(m);
  } catch (e) {
    if (e.status === 401) {
      apply(null);
    } else if (e.code === 'password_change_required') {
      // /me 在必須變更密碼時仍可使用；其他錯誤往上丟
      throw e;
    } else {
      throw e;
    }
  }
  return me;
}

/** 重新讀取（權限可能改變）；回傳是否仍登入 */
export async function refresh() {
  try {
    apply(await api('/api/v1/me', { auth: false }));
    return true;
  } catch (e) {
    if (e.status === 401) {
      apply(null);
      return false;
    }
    return Boolean(me);
  }
}

/** 登入成功後直接套用（登入回應已有 user 與 csrf_token；再讀一次 /me 取得權限） */
export function clear() {
  apply(null);
}

export function current() {
  return me;
}

export function isLoggedIn() {
  return Boolean(me);
}

/** 以帳號登入的工作階段（個人設定只對工作階段開放；API token 開啟網頁時沒有） */
export function isSession() {
  return Boolean(me && me.type === 'session');
}

export function mustChangePassword() {
  return Boolean(me && me.user && me.user.must_change_password);
}

/** 全站範圍有這個權限 */
export function can(perm) {
  return perms.has(perm);
}

/** 全站或任一群組範圍有這個權限（主機的讀取） */
export function canAny(perm) {
  return anyScopePerms.has(perm);
}

/** 只有群組範圍（顯示「範圍：○○群組」） */
export function scopedGroups(perm = 'hosts:read') {
  if (can(perm) || !me) return [];
  return Object.entries(me.group_permissions || {})
    .filter(([, list]) => list.includes(perm))
    .map(([g]) => g);
}

/** 顯示名稱 */
export function displayName() {
  if (!me) return '';
  if (me.type === 'session') return me.user.display_name || me.user.username;
  return me.name || me.token_id;
}

export function onChange(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}
