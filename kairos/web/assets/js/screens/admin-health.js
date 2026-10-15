// S20 系統健康（/system/health；設計包 screens.md S20；API.md 第 5.5 節）。
//
// - 只讀一支 API：GET /api/v1/system/health（每 30 秒更新）。0.8 起簽發 CA、註冊端點、告警引擎、/metrics 端點也由
//   後端併入（INTEGRATION §29.6）：頁首「整體」與頂列系統狀態燈都依 API 的 status，兩處一致；沒有對應權限的
//   項目只有狀態與代碼（restricted），不顯示數值。
// - 「下載診斷包」沒有 API：改為「如何產生診斷包」與可複製的 kairos diag 指令；「目錄服務」列隱藏（§28.6）。
// - 唯讀（沒有 settings:read）只看整體狀態與各項 StatusBadge，不看數值細節（S20 權限差異）。
// - 健康 API 本身無回應：整頁 Banner「無法取得健康狀態」，與「全部正常」在視覺上完全不同。

import { h, icon, mount } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { pageHead, card, dl, errorState, statusBadge, skeletonBlock, codeTag, copyButton } from '../ui/components.js';

const LEVEL = { ok: ['online', 'adm.health.ok'], warning: ['warning', 'adm.health.warning'], critical: ['critical', 'adm.health.critical'] };

export function levelBadge(level) {
  const [s, k] = LEVEL[level] || ['unknown', null];
  return statusBadge(s, k ? t(k) : level);
}

function itemLabel(id) {
  return has(`adm.health.item.${id}`) ? t(`adm.health.item.${id}`) : has(`health.${id}`) ? t(`health.${id}`) : id;
}

/** 各項的處置捷徑（依權限；S20「處理捷徑」） */
function shortcut(it) {
  if (it.id === 'connections' && session.can('settings:read')) return h('a', { href: '/system/settings/security' }, t('adm.health.toSettings'));
  if (it.id === 'enroll' && session.can('settings:read')) return h('a', { href: '/system/settings/security' }, t('adm.health.toEnrollMode'));
  if ((it.id === 'issuing_ca' || it.id === 'tls') && session.can('pki:read') && it.status !== 'ok') return h('a', { href: '/certificates/ca' }, t('adm.health.toCa'));
  if (it.id === 'alerts' && session.can('alerts:read') && it.status !== 'ok') return h('a', { href: '/alerts/events' }, t('adm.health.toAlerts'));
  return null;
}

export function render(ctx) {
  const root = ctx.root;
  const detail = session.can('settings:read');
  const meta = h('span', { class: 'k-page-head__meta' });
  const listEl = h('div', null, skeletonBlock(8));
  const serviceEl = h('div');
  const refreshBtn = h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', on: { click: () => load() } }, icon('refresh'), t('common.refresh'));
  const diagCmd = (cfg) => `sudo /opt/kairos/kairos diag --config ${cfg || '/opt/kairos/kairos.toml'}`;
  const diagCode = h('code', null, diagCmd());
  const diag = h(
    'details',
    { class: 'k-details' },
    h('summary', null, t('adm.health.diagHow')),
    h('p', null, t('adm.health.diagBody')),
    h('pre', { class: 'ds-code-block' }, diagCode, copyButton(() => diagCode.textContent, t('adm.health.diagCopy'))),
    h('p', { class: 'caption' }, t('adm.health.diagRedact')),
  );
  mount(
    root,
    pageHead({ title: t('adm.health.title'), meta, actions: refreshBtn }),
    detail ? diag : null,
    card({ title: t('adm.health.items'), body: listEl }),
    serviceEl,
    h('p', { class: 'caption' }, t('adm.health.external')),
  );

  let timer = null;
  let disposed = false;
  let seq = 0;
  async function load() {
    const my = ++seq;
    let hd;
    try {
      hd = await api('/api/v1/system/health');
    } catch (err) {
      if (my !== seq || disposed) return;
      mount(meta);
      mount(
        listEl,
        h('div', { class: 'ds-banner ds-banner--critical', role: 'alert' }, icon('status-critical'), h('div', { class: 'ds-banner__body' }, h('p', { class: 'strong' }, t('adm.health.unavailable')), h('p', null, t('adm.health.unavailableBody')))),
        errorState(err, { title: t('adm.health.loadFailed'), onRetry: load }),
      );
      return;
    }
    if (my !== seq || disposed) return;
    const items = [...(hd.items || [])];
    const order = { critical: 0, warning: 1, ok: 2 };
    items.sort((a, b) => (order[a.status] ?? 3) - (order[b.status] ?? 3));
    const bad = items.filter((i) => i.status !== 'ok');
    // 整體狀態只依 API（與頂列系統狀態燈相同，screens.md S20）
    const worst = hd.status || 'ok';
    mount(meta, t('adm.health.overall'), ' ', levelBadge(worst), bad.length ? ` ${t('adm.health.issues', { n: bad.length })}` : '', ' · ', t('adm.health.updated'), ' ', fmt.timeEl(fmt.serverNow()));
    mount(
      listEl,
      h(
        'ul',
        { class: 'k-check-list', 'aria-label': t('adm.health.items') },
        items.map((it) =>
          h(
            'li',
            { dataset: { item: it.id, status: it.status } },
            levelBadge(it.status),
            h(
              'div',
              { class: 'k-grow' },
              h('p', { class: 'strong' }, itemLabel(it.id), it.product_code && it.status !== 'ok' ? [' ', codeTag(it.product_code)] : null),
              detail && it.summary && !it.restricted ? h('p', { class: 'secondary' }, it.summary) : null,
              detail && it.restricted ? h('p', { class: 'caption' }, t('adm.health.restricted')) : null,
              // /metrics 端點：說明一律附反向代理的限制（screens.md S20「/metrics 端點」列，D42）
              detail && it.id === 'metrics' ? h('p', { class: 'caption' }, t('adm.health.metrics.proxy'), ' ', codeTag('KRS-W0304')) : null,
              detail ? (() => {
                const a = shortcut(it);
                return a ? h('p', null, a) : null;
              })() : null,
            ),
          ),
        ),
      ),
      detail ? null : h('p', { class: 'caption' }, t('adm.health.summaryOnly')),
    );
    if (detail && hd.service) {
      const s = hd.service;
      diagCode.textContent = diagCmd(s.config_file);
      mount(
        serviceEl,
        card({
          title: t('adm.health.service'),
          body: dl([
            [t('adm.health.version'), `Kairos ${s.version}${s.commit ? `（commit ${s.commit}）` : ''}`],
            [t('adm.health.uptime'), `${fmt.duration(s.uptime_secs)}（${t('adm.health.since', { t: `${fmt.dateTime(s.started_at)} ${fmt.tzLabel()}` })}）`],
            [t('adm.health.configFile'), s.config_file ? h('span', { class: 'mono' }, s.config_file) : '—'],
          ]),
        }),
      );
    }
  }
  load();
  timer = setInterval(load, 30000);
  return {
    dispose() {
      disposed = true;
      clearInterval(timer);
    },
  };
}
