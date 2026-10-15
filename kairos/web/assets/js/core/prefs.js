// 個人偏好（S23）：0.7.0 沒有伺服器端的偏好儲存（設計包 decisions/INTEGRATION.md §28.6），
// 只存在這個瀏覽器的 localStorage，畫面上標示「只套用在這個瀏覽器」。
// **不在 localStorage 放任何秘密**：工作階段只在 HttpOnly cookie，CSRF token 只在記憶體。
// localStorage 可能被停用（隱私模式、政策）：一律 try/catch，讀不到時用預設值。

const PREFIX = 'kairos.';

export const DEFAULTS = Object.freeze({
  /** system／light／dark */
  theme: 'system',
  /** system／reduce */
  motion: 'system',
  /** 顯示時區：IANA 名稱；預設台北（copywriting.md §5.1：不自動採用瀏覽器時區） */
  timeZone: 'Asia/Taipei',
  /** relative／absolute：表格中的時間 */
  timeDisplay: 'relative',
  /** 側邊導覽收合 */
  navCollapsed: '0',
  /** 上次選擇的登入方式（local／ldap；不記帳號） */
  loginMethod: '',
});

const listeners = new Set();

export function get(key) {
  try {
    const v = window.localStorage.getItem(PREFIX + key);
    if (v != null) return v;
  } catch {
    /* 停用時用預設值 */
  }
  return DEFAULTS[key];
}

export function set(key, value) {
  try {
    if (value == null || value === DEFAULTS[key]) window.localStorage.removeItem(PREFIX + key);
    else window.localStorage.setItem(PREFIX + key, String(value));
  } catch {
    /* 停用時只在這次有效 */
  }
  for (const fn of listeners) fn(key, value);
}

/** 偏好改變時通知（主題、時區） */
export function onChange(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

/** 套用主題與減少動態效果到 <html>（theme-init.js 在第一次繪製前做同樣的事） */
export function applyAppearance() {
  const root = document.documentElement;
  const theme = get('theme');
  if (theme === 'light' || theme === 'dark') root.setAttribute('data-theme', theme);
  else root.removeAttribute('data-theme');
  if (get('motion') === 'reduce') root.setAttribute('data-motion', 'reduce');
  else root.removeAttribute('data-motion');
}

/** 目前實際的主題（跟隨系統時依 prefers-color-scheme） */
export function effectiveTheme() {
  const t = get('theme');
  if (t === 'light' || t === 'dark') return t;
  return window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}

/** 是否減少動態效果（偏好或作業系統） */
export function reducedMotion() {
  if (get('motion') === 'reduce') return true;
  return Boolean(window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches);
}
