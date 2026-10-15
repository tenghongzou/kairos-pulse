// 管理與系統畫面的路由（S16、S17、S18、S20、S21；設計包 ia.md §3 網站地圖、§4.2 權限）。
// main.js 以 ROUTES.push(...ADMIN_ROUTES) 併入；欄位同 main.js 的路由表，另有：
//   allow   () => boolean：需要「任一」權限時（稽核日誌：audit:read 或 audit:read_own）

import * as session from './core/session.js';

export const ADMIN_ROUTES = [
  { path: '/audit', allow: () => session.can('audit:read') || session.can('audit:read_own'), title: 'adm.audit.title', load: () => import('./screens/admin-audit.js') },
  { path: '/admin/users', perm: 'users:read', title: 'adm.users.title', load: () => import('./screens/admin-users.js') },
  { path: '/admin/users/:username', perm: 'users:read', title: 'adm.users.title', load: () => import('./screens/admin-user.js') },
  { path: '/admin/roles', perm: 'users:read', title: 'adm.users.tab.roles', load: () => import('./screens/admin-users.js') },
  { path: '/admin/directory', perm: 'directory:read', title: 'adm.dir.title', load: () => import('./screens/admin-directory.js') },
  { path: '/system/health', perm: 'health:read', title: 'adm.health.title', load: () => import('./screens/admin-health.js') },
  { path: '/system/settings/:section?', perm: 'settings:read', title: 'adm.set.title', load: () => import('./screens/admin-settings.js') },
];
