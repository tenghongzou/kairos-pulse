// 側邊導覽：稽核、管理、系統（設計包 ia.md §3；權限 §4.2）。nav.js 以 NAV.push(...ADMIN_NAV) 併入。
// allow：需要「任一」權限時（稽核日誌：audit:read 或 audit:read_own；shell.js 的 navVisible 支援）。

import * as session from '../core/session.js';

export const ADMIN_NAV = [
  {
    section: 'nav.section.audit',
    items: [
      { path: '/audit', label: 'nav.audit', icon: 'audit', allow: () => session.can('audit:read') || session.can('audit:read_own'), match: (p) => p === '/audit' },
    ],
  },
  {
    section: 'nav.section.admin',
    items: [
      { path: '/admin/users', label: 'nav.users', icon: 'users', perm: 'users:read', match: (p) => p.startsWith('/admin/users') || p === '/admin/roles' },
      { path: '/admin/directory', label: 'nav.directory', icon: 'user', perm: 'directory:read', match: (p) => p === '/admin/directory' },
    ],
  },
  {
    section: 'nav.section.system',
    items: [
      { path: '/system/health', label: 'nav.health', icon: 'health', perm: 'health:read', match: (p) => p === '/system/health' },
      { path: '/system/settings', label: 'nav.settings', icon: 'settings', perm: 'settings:read', match: (p) => p === '/system/settings' || p.startsWith('/system/settings/') },
    ],
  },
];
