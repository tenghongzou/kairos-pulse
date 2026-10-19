// 遠端管理（F8）的路由：S24 集中設定、S25 Pulse 版本庫、S26 升級推送（設計包 ia.md §3「Pulse 管理」、§4.2 權限）。
// main.js 以 ROUTES.push(...PULSE_ROUTES) 併入；欄位同 main.js 的路由表。固定的路徑（assignments、new）要排在參數之前。

export const PULSE_ROUTES = [
  { path: '/pulse/config', perm: 'config:read', title: 'rm.cfg.title', load: () => import('./screens/pulse-config.js') },
  { path: '/pulse/config/assignments', perm: 'config:read', title: 'rm.cfg.title', load: () => import('./screens/pulse-config.js') },
  { path: '/pulse/config/:profileId', perm: 'config:read', title: 'rm.cfg.title', load: () => import('./screens/pulse-config-edit.js') },
  { path: '/pulse/rollouts', perm: 'rollouts:read', title: 'rm.ro.title', load: () => import('./screens/pulse-rollouts.js') },
  { path: '/pulse/rollouts/new', perm: 'rollouts:write', title: 'rm.new.title', load: () => import('./screens/pulse-rollout-new.js') },
  { path: '/pulse/rollouts/:rolloutId', perm: 'rollouts:read', title: 'rm.ro.title', load: () => import('./screens/pulse-rollout.js') },
  { path: '/pulse/releases', perm: 'releases:read', title: 'rm.rel.title', load: () => import('./screens/pulse-releases.js') },
  { path: '/pulse/releases/:version', perm: 'releases:read', title: 'rm.rel.title', load: () => import('./screens/pulse-releases.js') },
];
