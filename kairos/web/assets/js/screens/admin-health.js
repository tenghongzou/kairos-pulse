// S20 系統健康（/system/health；設計包 screens.md S20「0.6 後端現況」；API.md 第 5.5 節）。
//
// - 檢查項目：GET /api/v1/system/health（資料庫與容量、保留清除、回報寫入、補送、連線、伺服器憑證與 CRL、監聽、
//   檔案描述元、身分驗證），每 30 秒更新。
// - 依權限另外加入（各自的 API，失敗時只略過該列）：簽發 CA 到期（pki:read，GET /api/v1/pki/ca）、註冊端點
//   （enroll:read，GET /api/v1/enroll/mode）、告警引擎（alerts:read，GET /api/v1/alerts/summary 的 engine）、
//   /metrics 端點（settings:read，GET /api/v1/settings 的 observability.*）。
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

async function extraItems(now) {
  const out = [];
  const jobs = [];
  if (session.can('pki:read')) {
    jobs.push(
      api('/api/v1/pki/ca')
        .then((d) => {
          const active = (d.issuing_cas || []).filter((c) => c.state === 'active');
          if (!active.length) {
            out.push({ id: 'issuing_ca', status: 'ok', summary: d.pki_mode === 'offline' ? t('adm.health.ca.offline') : t('adm.health.ca.none') });
            return;
          }
          for (const c of active) {
            const left = c.not_after ? (Date.parse(c.not_after) - now) / 86400e3 : null;
            const status = left != null && left <= 0 ? 'critical' : c.expiry_warning ? 'warning' : 'ok';
            out.push({
              id: 'issuing_ca',
              status,
              product_code: status === 'critical' ? 'KRS-E1303' : c.expiry_warning || null,
              summary: left == null ? t('adm.health.ca.noExpiry', { s: c.subject }) : t('adm.health.ca.expires', { s: c.subject, n: Math.max(0, Math.floor(left)), d: fmt.date(c.not_after) }),
            });
          }
        })
        .catch(() => {}),
    );
  }
  if (session.can('enroll:read')) {
    jobs.push(
      api('/api/v1/enroll/mode')
        .then((m) => {
          const modeLabel = has(`adm.mode.${m.state}`) ? t(`adm.mode.${m.state}`) : m.state;
          const shouldListen = m.mode !== 'closed';
          const status = shouldListen && !m.listening ? 'critical' : 'ok';
          out.push({
            id: 'enroll',
            status,
            product_code: status === 'critical' ? 'KRS-E2001' : null,
            summary: shouldListen ? (m.listening ? t('adm.health.enroll.listening', { mode: modeLabel, addr: m.listen_addr || '—' }) : t('adm.health.enroll.notListening', { mode: modeLabel })) : t('adm.health.enroll.closed'),
          });
        })
        .catch(() => {}),
    );
  }
  if (session.can('alerts:read')) {
    jobs.push(
      api('/api/v1/alerts/summary')
        .then((s) => {
          const e = s.engine || {};
          out.push({
            id: 'alerts',
            status: e.running ? 'ok' : 'critical',
            summary: e.running
              ? e.warmup && e.warmup.active
                ? t('adm.health.alerts.warmup', { t: fmt.time(e.warmup.until) })
                : t('adm.health.alerts.running', { t: e.last_tick_at ? fmt.relative(e.last_tick_at) : '—' })
              : t('adm.health.alerts.stopped', { err: e.last_error || '—' }),
          });
        })
        .catch((err) => {
          if (err && err.status === 503) out.push({ id: 'alerts', status: 'ok', summary: t('adm.health.alerts.disabled') });
        }),
    );
  }
  if (session.can('settings:read')) {
    jobs.push(
      api('/api/v1/settings')
        .then((d) => {
          const get = (k) => {
            const s = (d.settings || []).find((x) => x.key === k);
            return s ? s.value : null;
          };
          const onConsole = get('observability.on_console') !== false;
          const listen = get('observability.listen') || '';
          const token = get('observability.metrics_token_file') || '';
          const parts = [];
          parts.push(onConsole ? t('adm.health.metrics.console') : t('adm.health.metrics.noConsole'));
          if (onConsole) parts.push(token ? t('adm.health.metrics.token') : t('adm.health.metrics.loopback'));
          if (listen) parts.push(t('adm.health.metrics.listen', { addr: listen }));
          out.push({ id: 'metrics', status: 'ok', summary: parts.join(' · ') });
        })
        .catch(() => {}),
    );
  }
  await Promise.all(jobs);
  return out;
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
    const extra = detail || session.can('pki:read') || session.can('alerts:read') ? await extraItems(fmt.serverNow()) : [];
    if (my !== seq || disposed) return;
    const items = [...(hd.items || []), ...extra];
    const order = { critical: 0, warning: 1, ok: 2 };
    items.sort((a, b) => (order[a.status] ?? 3) - (order[b.status] ?? 3));
    const bad = items.filter((i) => i.status !== 'ok');
    const worst = items.some((i) => i.status === 'critical') ? 'critical' : bad.length ? 'warning' : 'ok';
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
              detail && it.summary ? h('p', { class: 'secondary' }, it.summary) : null,
              detail && it.id === 'connections' && session.can('settings:read') ? h('p', null, h('a', { href: '/system/settings/security' }, t('adm.health.toSettings'))) : null,
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
