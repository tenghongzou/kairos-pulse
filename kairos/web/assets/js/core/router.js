// 路由：History API（網址就是畫面狀態，可分享、可加書籤；設計包 ia.md §3.2）。
// 伺服器對 /api/ 與 /assets/ 以外、沒有副檔名的路徑都回 index.html（kairos/src/api/web.rs）。
//
// 路由表在 main.js（routes.js）：{ path: '/hosts/:id/:tab?', load: () => import(...), perm, public, ... }。
// 畫面模組匯出 render(ctx)，回傳 { dispose() }（可省略）；見 kairos/web/README.md「新增畫面」。

let routes = [];
let onRoute = null;

function compile(path) {
  const segs = path.split('/').filter(Boolean);
  return (urlPath) => {
    const parts = urlPath.split('/').filter(Boolean);
    const params = {};
    let i = 0;
    for (const seg of segs) {
      const optional = seg.endsWith('?');
      const name = seg.replace(/^:/, '').replace(/\?$/, '');
      const part = parts[i];
      if (seg.startsWith(':')) {
        if (part == null) {
          if (optional) continue;
          return null;
        }
        try {
          params[name] = decodeURIComponent(part);
        } catch {
          return null;
        }
        i += 1;
      } else {
        if (part !== seg) return null;
        i += 1;
      }
    }
    return i === parts.length ? params : null;
  };
}

/** 設定路由表 */
export function setRoutes(list) {
  routes = list.map((r) => ({ ...r, match: compile(r.path) }));
}

/** 目前網址對應的路由 */
export function resolve(pathname = location.pathname) {
  for (const r of routes) {
    const params = r.match(pathname);
    if (params) return { route: r, params };
  }
  return { route: null, params: {} };
}

/** 目前的查詢參數 */
export function query() {
  return new URLSearchParams(location.search);
}

// 站內換頁前的攔截（未儲存的變更；main.js 設定）：dirty() 為 true 時以 ask() 詢問，回傳 true 才換頁。
// 只攔截使用者發起、會換到另一個路徑的換頁（連結、上一頁／下一頁、畫面呼叫 navigate）；
// replace（登出、工作階段失效、登入後轉址）一律不攔截。
let leaveGuard = null;
let shownUrl = null;

/** 設定換頁前的攔截：{ dirty: () => boolean, ask: () => Promise<boolean> } */
export function setLeaveGuard(guard) {
  leaveGuard = guard;
}

function needsAsk(toPathname, fromPathname = location.pathname) {
  if (!leaveGuard || toPathname === fromPathname) return false;
  try {
    return Boolean(leaveGuard.dirty());
  } catch {
    return false;
  }
}

async function ask() {
  try {
    return Boolean(await leaveGuard.ask());
  } catch {
    return true;
  }
}

function go() {
  shownUrl = location.pathname + location.search + location.hash;
  if (onRoute) onRoute({ pathChanged: true });
}

/** 換頁（同源路徑）；replace：取代目前的歷史紀錄（篩選條件改變時） */
export async function navigate(to, { replace = false } = {}) {
  const url = new URL(to, location.origin);
  if (url.origin !== location.origin) return;
  if (!replace && needsAsk(url.pathname) && !(await ask())) return;
  const same = url.pathname + url.search === location.pathname + location.search;
  if (replace || same) history.replaceState(null, '', url.pathname + url.search + url.hash);
  else history.pushState(null, '', url.pathname + url.search + url.hash);
  go();
}

/** 只更新查詢參數（不重畫畫面；清單的篩選、排序寫回網址） */
export function replaceQuery(params) {
  const sp = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v == null || v === '' || (Array.isArray(v) && v.length === 0)) continue;
    sp.set(k, Array.isArray(v) ? v.join(',') : String(v));
  }
  const s = sp.toString();
  history.replaceState(null, '', location.pathname + (s ? `?${s}` : ''));
}

/** 站內路徑（登入後的 next 只接受站內路徑，避免開放重新導向；screens.md S01） */
export function safeNext(next) {
  if (!next || typeof next !== 'string') return '/';
  if (!next.startsWith('/') || next.startsWith('//') || next.startsWith('/\\')) return '/';
  if (next.startsWith('/api/') || next.startsWith('/assets/') || next.startsWith('/login')) return '/';
  try {
    const u = new URL(next, location.origin);
    if (u.origin !== location.origin) return '/';
    return u.pathname + u.search;
  } catch {
    return '/';
  }
}

function isInternalLink(a, ev) {
  if (!a || ev.defaultPrevented || ev.button !== 0) return false;
  if (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey) return false;
  if (a.target && a.target !== '_self') return false;
  if (a.hasAttribute('download') || a.dataset.native === '1') return false;
  const href = a.getAttribute('href');
  if (!href || !href.startsWith('/') || href.startsWith('//')) return false;
  if (href.startsWith('/api/') || href.startsWith('/assets/')) return false;
  return true;
}

/** 開始：攔截站內連結的點擊、處理上一頁／下一頁 */
export function start(handler) {
  onRoute = handler;
  document.addEventListener('click', (ev) => {
    const a = ev.target.closest && ev.target.closest('a[href]');
    if (!isInternalLink(a, ev)) return;
    ev.preventDefault();
    navigate(a.getAttribute('href'));
  });
  window.addEventListener('popstate', async () => {
    // 上一頁／下一頁：瀏覽器已經換了網址；有未儲存的變更時先把原本的網址放回去再詢問（畫面不重畫）
    const prev = shownUrl;
    const target = location.pathname + location.search + location.hash;
    if (prev && needsAsk(location.pathname, new URL(prev, location.origin).pathname)) {
      history.pushState(null, '', prev);
      if (!(await ask())) return;
      history.replaceState(null, '', target);
    }
    go();
  });
  go();
}
