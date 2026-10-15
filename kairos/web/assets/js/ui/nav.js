// 側邊導覽的項目（設計包 ia.md §3 網站地圖、§4.2 權限矩陣）。
//
// 每個項目宣告需要的權限：perm（全站範圍）或 permAny（全站或群組範圍，例如主機的讀取）。
// 沒有權限的項目不顯示（ia.md §4.2「—＝SideNav 不顯示，直接存取回 403」）；整個區塊都沒有可見項目時
// 區塊標題也不顯示。只列出**已實作**的畫面：沒有畫面的功能不顯示停用中的假選單
// （decisions/INTEGRATION.md §28.6「隱藏」的原則）。新增畫面時在這裡加一項，並在 routes.js 加路由。
import { ADMIN_NAV } from './nav-admin.js';

export const NAV = [
  {
    section: 'nav.section.monitoring',
    items: [
      { path: '/', label: 'nav.overview', icon: 'dashboard', permAny: 'hosts:read', match: (p) => p === '/' },
      { path: '/hosts', label: 'nav.hosts', icon: 'server', permAny: 'hosts:read', match: (p) => p === '/hosts' || p.startsWith('/hosts/') },
      // 告警（S06～S11）；count：計數泡泡的鍵（shell.setNavCount，screens/alerts-boot.js）
      { path: '/alerts/events', label: 'nav.alertEvents', icon: 'bell', perm: 'alerts:read', count: 'alerts', match: (p) => p === '/alerts' || p.startsWith('/alerts/events') },
      { path: '/alerts/rules', label: 'nav.alertRules', icon: 'filter', perm: 'alert-rules:read', match: (p) => p.startsWith('/alerts/rules') },
      { path: '/alerts/silences', label: 'nav.silences', icon: 'bell-off', perm: 'silences:read', match: (p) => p.startsWith('/alerts/silences') },
      { path: '/alerts/channels', label: 'nav.channels', icon: 'network', perm: 'channels:read', match: (p) => p.startsWith('/alerts/channels') },
      // 註冊與憑證（S12～S15）、稽核（S18）、管理（S16、S17）、系統（S20～S22）：後續加入
    ],
  },
  {
    // 註冊與憑證（S12～S15；ia.md §3）
    section: 'nav.section.pki',
    items: [
      { path: '/enrollment/pending', label: 'nav.pending', icon: 'enrollment', perm: 'enroll:read', count: 'pending', match: (p) => p.startsWith('/enrollment/pending') },
      { path: '/enrollment/tokens', label: 'nav.tokens', icon: 'key', perm: 'enroll:read', match: (p) => p.startsWith('/enrollment/tokens') },
      { path: '/certificates', label: 'nav.certificates', icon: 'certificate', perm: 'pki:read', match: (p) => p === '/certificates' || (p.startsWith('/certificates/') && p !== '/certificates/ca') },
      { path: '/certificates/ca', label: 'nav.ca', icon: 'shield-check', perm: 'pki:read', match: (p) => p === '/certificates/ca' },
    ],
  },
];
// 稽核、管理、系統（S16～S18、S20、S21）：nav-admin.js
NAV.push(...ADMIN_NAV);
