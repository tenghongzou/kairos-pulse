// S15 CA 與撤銷清單（設計包 kairos/screens.md S15；API.md 第 11.5 節 GET /pki/ca、/pki/crl、POST /pki/crl/regenerate）。
//
// - 撤銷清單：每個簽發 CA 目前的 CRL（版本、發布、nextUpdate、撤銷筆數、來源）；nextUpdate 已過為 critical
//   （KRS-E1203），7 天內為 warning（KRS-W1201，與系統健康相同的門檻）；「立即更新 CRL」（L1）只在線上簽發且
//   簽發 CA 可用時出現（pki.certificates:revoke，維運者也可以：O1 的救火動作）。
// - 憑證鏈：信任錨（根 CA 指紋）、簽發 CA（狀態、有效期、私鑰位置、簽發張數、KRS-W1304 即將到期）、簽發方式
//   （線上／離線）；線上但私鑰無法使用時頂端 KRS-E1303（critical）。
// - 0.7.0 隱藏（API 沒有提供，INTEGRATION §27.9 #2）：上傳 CRL、下載目前的 CRL、設定自動更新、CA 輪替精靈、
//   改為離線／線上簽發、更換伺服器憑證；簽發 CA 的建立、安裝、啟用與退役以 CLI 進行（技術細節列出指令）。
//   離線簽發的「匯出撤銷清單」API 未實作，改為說明。

import { h, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { pageHead, card, errorState, inlineAlert, dl, skeletonBlock, button, statusBadge, neutralBadge } from '../ui/components.js';
import { confirm } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { createTable } from '../ui/table.js';
import { fetchMode, issuanceAlert, codeBlock } from './pki-common.js';

const DAY = 86400000;

function caStateBadge(s) {
  switch (s) {
    case 'active':
      return statusBadge('online', t('ca.state.active'));
    case 'trusted':
      return neutralBadge(t('ca.state.trusted'), 'shield-check');
    case 'pending':
      return statusBadge('pending', t('ca.state.pending'));
    case 'retired':
      return neutralBadge(t('ca.state.retired'), 'shield-off');
    default:
      return neutralBadge(s);
  }
}

function sourceText(s) {
  return ['scheduled', 'startup', 'revocation', 'manual', 'upload'].includes(s) ? t(`ca.crl.source.${s}`) : s || '—';
}

export function render(ctx) {
  const root = ctx.root;
  const canRegen = session.can('pki.certificates:revoke');
  let disposed = false;
  const body = h('div', { class: 'k-stack' }, skeletonBlock(8));
  mount(root, pageHead({ title: t('ca.title'), crumbs: [{ text: t('cert.title'), href: '/certificates' }, { text: t('ca.title') }] }), body);

  async function load() {
    let ca;
    let crl;
    let mode;
    try {
      [ca, crl, mode] = await Promise.all([api('/api/v1/pki/ca'), api('/api/v1/pki/crl'), fetchMode()]);
    } catch (e) {
      if (!disposed) mount(body, errorState(e, { title: t('ca.failed'), onRetry: load }));
      return;
    }
    if (disposed) return;
    const online = ca.pki_mode === 'online';
    const issuing = mode && mode.issuance ? mode.issuance.status : online ? 'online' : 'offline';
    const cas = new Map((ca.issuing_cas || []).map((c) => [c.ca_id, c]));
    const now = fmt.serverNow();

    // ---- 撤銷清單 ----
    const crlCards = (crl.crls || []).map((x) => {
      const c = x.current;
      const caRow = cas.get(x.ca_id);
      const name = caRow ? caRow.subject : `#${x.ca_id}`;
      let status = null;
      let alert = null;
      if (!c) {
        status = 'warning';
        alert = inlineAlert({ kind: 'warning', title: t('ca.crl.none') });
      } else {
        const next = Date.parse(c.next_update);
        const days = Math.floor((next - now) / DAY);
        if (next <= now) {
          status = 'critical';
          alert = inlineAlert({ kind: 'error', title: t('ca.crl.expired'), code: 'KRS-E1203', body: t('ca.crl.expiredBody') });
        } else if (days <= 7) {
          status = 'warning';
          alert = inlineAlert({ kind: 'warning', title: t('ca.crl.soon', { d: days, at: `${fmt.dateTime(next, { seconds: false })} ${fmt.tzLabel(next)}` }), code: 'KRS-W1201', body: t('ca.crl.soonBody') });
        }
      }
      const actions = [];
      if (canRegen && online && issuing === 'online') {
        actions.push(button({ text: t('ca.crl.regen'), iconName: 'refresh', variant: status === 'critical' ? 'primary' : 'secondary', onClick: regenerate }));
      }
      return card({
        title: t('ca.crl.title', { name }),
        status,
        body: h(
          'div',
          { class: 'k-stack--sm' },
          alert,
          c
            ? dl([
                [t('ca.crl.version'), h('span', { class: 'mono' }, `#${c.crl_number}`)],
                [t('ca.crl.thisUpdate'), fmt.timeEl(c.this_update, { mode: 'absolute' })],
                [t('ca.crl.nextUpdate'), [fmt.timeEl(c.next_update, { mode: 'absolute' }), h('span', { class: 'k-sub' }, fmt.relative(c.next_update))]],
                [t('ca.crl.revoked'), t('ca.crl.revokedN', { n: fmt.num(c.revoked_count) })],
                [t('ca.crl.source'), sourceText(c.source)],
              ])
            : null,
          h('p', { class: 'muted' }, online ? t('ca.crl.autoOnline') : t('ca.crl.autoOffline')),
          actions.length ? h('div', { class: 'ds-button-row' }, actions) : null,
        ),
      });
    });

    // ---- 憑證鏈 ----
    const chainRows = (ca.issuing_cas || []).map((c) =>
      h(
        'li',
        { class: 'k-stack--sm', dataset: { ca: c.ca_id } },
        h('div', { class: 'k-row' }, h('strong', null, t('ca.chain.issuing')), h('span', null, c.subject), caStateBadge(c.state)),
        dl([
          [t('ca.chain.validity'), c.not_after ? `${fmt.date(c.not_before)} ～ ${fmt.date(c.not_after)}` : '—'],
          [t('ca.chain.key'), c.key === 'on_server' ? t('ca.chain.keyOnServer') : t('ca.chain.keyOffline')],
          [t('ca.chain.issued'), t('ca.chain.issuedN', { n: fmt.num(c.issued_count) })],
          [t('ca.chain.created'), fmt.timeEl(c.created_at)],
          c.retired_at ? [t('ca.chain.retired'), fmt.timeEl(c.retired_at)] : null,
        ]),
        c.expiry_warning
          ? inlineAlert({
              kind: 'warning',
              title: t('ca.chain.expiring', { at: fmt.date(c.not_after), d: fmt.num(Math.max(0, Math.floor((Date.parse(c.not_after) - now) / DAY))) }),
              code: c.expiry_warning,
              body: t('ca.chain.expiringBody', { th: fmt.num(ca.expiry_warning_days) }),
            })
          : null,
      ),
    );
    const chain = card({
      title: t('ca.chain.title'),
      body: h(
        'div',
        { class: 'k-stack--sm' },
        dl([
          [t('ca.chain.root'), ca.server_ca ? h('span', { class: 'mono' }, ca.server_ca.ca_pin) : t('ca.chain.noRoot')],
          ca.server_ca && ca.server_ca.path ? [t('ca.chain.rootPath'), h('span', { class: 'mono' }, ca.server_ca.path)] : null,
          [t('ca.chain.mode'), online ? t('ca.chain.modeOnline') : t('ca.chain.modeOffline')],
          ca.client_ca_file ? [t('ca.chain.clientCa'), h('span', { class: 'mono' }, ca.client_ca_file)] : null,
        ]),
        chainRows.length ? h('ul', { class: 'k-ca-list' }, chainRows) : inlineAlert({ kind: 'warning', title: t('ca.chain.noIssuing') }),
        !online ? inlineAlert({ kind: 'info', title: t('ca.chain.offlineNote'), actions: [h('a', { class: 'ds-button ds-button--secondary ds-button--sm', href: '/certificates?state=pending_issuance' }, t('pki.issuance.toQueue'))] }) : null,
        h(
          'details',
          { class: 'k-details' },
          h('summary', null, t('ca.chain.cli')),
          h('p', { class: 'muted' }, t('ca.chain.cliBody')),
          codeBlock('kairos pki issuing-ca list\nkairos pki issuing-ca new --out issuing.csr\nkairos pki issuing-ca install issuing.pem\nkairos pki issuing-ca activate <ca_id>\nkairos pki issuing-ca retire <ca_id>'),
        ),
      ),
    });

    // ---- CRL 版本歷史 ----
    const hist = [];
    for (const x of crl.crls || []) for (const c of x.history || []) hist.push({ ...c, ca_id: x.ca_id });
    hist.sort((a, b) => Date.parse(b.this_update) - Date.parse(a.this_update));
    const table = createTable({
      caption: t('ca.hist.title'),
      columns: [
        { key: 'ca', label: t('ca.hist.ca'), render: (r) => (cas.get(r.ca_id) ? cas.get(r.ca_id).subject : `#${r.ca_id}`) },
        { key: 'n', label: t('ca.crl.version'), num: true, render: (r) => `#${r.crl_number}` },
        { key: 'this', label: t('ca.hist.this', { tz: fmt.tzLabel() }), render: (r) => fmt.dateTime(r.this_update, { seconds: false }) },
        { key: 'next', label: t('ca.hist.next', { tz: fmt.tzLabel() }), render: (r) => fmt.dateTime(r.next_update, { seconds: false }) },
        { key: 'count', label: t('ca.crl.revoked'), num: true, render: (r) => fmt.num(r.revoked_count) },
        { key: 'src', label: t('ca.crl.source'), render: (r) => sourceText(r.source) },
      ],
      rowKey: (r) => `${r.ca_id}-${r.crl_number}`,
    });
    if (hist.length) table.setRows(hist.slice(0, 100), { total: hist.length, hasMore: false });
    else table.setEmpty(h('p', { class: 'muted' }, t('ca.hist.empty')));

    mount(
      body,
      issuanceAlert(mode, { where: 'ca' }),
      h('h2', { class: 'k-section-title' }, t('ca.crl.section')),
      crlCards.length ? crlCards : inlineAlert({ kind: 'warning', title: t('ca.crl.noCa') }),
      chain,
      h('h2', { class: 'k-section-title' }, t('ca.hist.title')),
      table.el,
    );
  }

  async function regenerate() {
    const ok = await confirm({ title: t('ca.crl.regenTitle'), body: t('ca.crl.regenBody'), confirmText: t('ca.crl.regen') });
    if (!ok) return;
    try {
      const r = await api('/api/v1/pki/crl/regenerate', { method: 'POST', body: {} });
      const nums = (r.crls || []).map((c) => `#${c.crl_number}`).join('、');
      toast({ kind: 'success', title: t('ca.crl.regenDone', { n: nums || '—' }) });
      load();
    } catch (e) {
      toast({ kind: 'error', title: t('ca.crl.regenFailed'), desc: e.productCode ? `${e.productCode} ${e.message}` : e.message });
    }
  }

  load();
  return {
    dispose() {
      disposed = true;
    },
  };
}
