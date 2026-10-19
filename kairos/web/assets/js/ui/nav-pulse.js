// 側邊導覽：「Pulse 管理」區塊（設計包 ia.md §3、§3.1；2026-10-19 D55）。nav.js 把它放在「註冊與憑證」之後。
// 順序：集中設定、升級推送（計數泡泡：已暫停的推送＋回滾失敗的主機，screens/pulse-boot.js）、Pulse 版本庫。
// 功能關閉（503 remote_unavailable）時區塊仍顯示，畫面以 EmptyState 說明（設計包 §31.8 F8 #6）。

export const PULSE_NAV = [
  {
    section: 'nav.section.pulse',
    items: [
      { path: '/pulse/config', label: 'nav.pulseConfig', icon: 'settings', perm: 'config:read', match: (p) => p === '/pulse/config' || p.startsWith('/pulse/config/') },
      { path: '/pulse/rollouts', label: 'nav.rollouts', icon: 'upload', perm: 'rollouts:read', count: 'rollouts', match: (p) => p === '/pulse/rollouts' || p.startsWith('/pulse/rollouts/') },
      { path: '/pulse/releases', label: 'nav.releases', icon: 'agent', perm: 'releases:read', match: (p) => p === '/pulse/releases' || p.startsWith('/pulse/releases/') },
    ],
  },
];
