// S14 憑證（設計包 kairos/screens.md S14；API.md 第 11.5 節）：清冊、狀態 KpiTile、撤銷（L2）、寬限期內與
// 需重新註冊檢視、允許再次自動續簽（L1，限管理員）、建立限定這些主機的 token、待簽發（離線簽發的 CSR 佇列）。
//
// - 憑證狀態不是主機狀態：有效借用 online、將到期與撤銷待生效借用 warning、已過期借用 critical，已撤銷為中性文字；
//   一律有文字（pki-common.certStateBadge）。
// - 詳情以對話框呈現，網址為 /certificates/:serial（可分享；關閉後回到清單的網址）。
// - 0.7.0 隱藏：匯出憑證清冊、手動簽發 CSR、重新簽發（輪替）、除役、憑證政策的修改（API 沒有寬限期縮短的影響數字，
//   改在頁首唯讀顯示目前的政策）、依群組與簽發方式篩選、「最後嘗試」欄（沒有資料來源）、以 PEM 目錄匯入簽發結果。

import { h, icon, announce, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { replaceQuery } from '../core/router.js';
import { pageHead, emptyState, errorState, field, inlineAlert, dl, skeletonBlock, button, kpiTile, kpiSkeleton, statusBadge } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { openMenu } from '../ui/menu.js';
import { toast } from '../ui/toast.js';
import { createTable } from '../ui/table.js';
import { actionDialog, selectField, textareaField, checkbox, fetchMode, issuanceAlert, certStateBadge, shortSerial, shortId, kindLabel, downloadText, codeBlock } from './pki-common.js';
import { openCreateToken } from './enroll-tokens.js';

const STATES = ['valid', 'expiring', 'revoked', 'revocation_pending', 'expired_in_grace', 'expired_beyond_grace', 'pending_issuance'];
const WITHIN = ['7d', '30d', '90d'];
const REASONS = ['keyCompromise', 'cessationOfOperation', 'superseded', 'unspecified'];
const DAY = 86400000;

function parseState(q) {
  return {
    state: STATES.includes(q.get('state')) ? q.get('state') : '',
    expires_within: WITHIN.includes(q.get('expires_within')) ? q.get('expires_within') : '',
    q: q.get('q') || '',
  };
}

function ms(v) {
  return v ? Date.parse(v) : null;
}

/** 外部 CA 簽發、匯入的憑證（ca_id 為 null；不在 Kairos 的 CRL 中，撤銷在下一次重新連線時才生效，PKI.md 第 16 節） */
function isExternal(c) {
  return c.issued_via === 'import' && c.ca_id == null;
}

/** 主機的名稱（沒有時 agent_id 前 8 碼） */
function hostText(c) {
  return c.hostname || shortId(c.agent_id);
}

export function render(ctx) {
  const root = ctx.root;
  const st = parseState(ctx.query);
  const canRevoke = session.can('pki.certificates:revoke');
  const canGrace = session.can('pki.grace:override');
  const canToken = session.can('enroll.tokens:write');
  const canQueue = session.can('pki.queue:manage');
  let rows = [];
  let graceDays = null;
  let mode = null;
  let disposed = false;
  let ctrl = null;
  const selected = new Set();
  const listUrl = () => location.pathname.startsWith('/certificates/') ? `/certificates${location.search}` : location.pathname + location.search;

  const kpis = h('div', { class: 'ds-kpi-row', 'aria-busy': 'true' });
  const alerts = h('div', { class: 'k-stack--sm' });
  const result = h('div', { class: 'ds-table__summary', role: 'status', tabindex: '-1', hidden: true });
  const bulkbar = h('div', { class: 'ds-table__bulkbar', hidden: true, role: 'region', 'aria-label': t('cert.bulk.region') });
  const listBox = h('div');

  // ---- 篩選 ----
  const search = h('input', { class: 'ds-input', type: 'search', name: 'q', value: st.q, autocomplete: 'off', spellcheck: 'false', placeholder: t('cert.searchPlaceholder') });
  const searchField = field({ label: t('cert.search'), input: search });
  searchField.classList.add('ds-filterbar__search');
  let searchTimer = null;
  search.addEventListener('input', () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      st.q = search.value.trim();
      reload();
    }, 300);
  });
  const stateSel = h(
    'select',
    { class: 'ds-select', name: 'state' },
    h('option', { value: '' }, t('cert.filter.all')),
    STATES.map((s) => h('option', { value: s, selected: st.state === s }, t(`cert.state.${s}`))),
  );
  stateSel.addEventListener('change', () => {
    st.state = stateSel.value;
    selected.clear();
    reload();
  });
  const withinSel = h(
    'select',
    { class: 'ds-select', name: 'expires_within' },
    h('option', { value: '' }, t('cert.filter.all')),
    WITHIN.map((w) => h('option', { value: w, selected: st.expires_within === w }, t(`cert.within.${w}`))),
  );
  withinSel.addEventListener('change', () => {
    st.expires_within = withinSel.value;
    reload();
  });
  const filterbar = h(
    'section',
    { class: 'ds-filterbar', 'aria-label': t('cert.filters') },
    h('div', { class: 'ds-filterbar__row' }, searchField),
    h(
      'div',
      { class: 'k-filter-selects' },
      h('label', { class: 'k-labelled-select' }, t('cert.filter.state'), stateSel),
      h('label', { class: 'k-labelled-select' }, t('cert.filter.within'), withinSel),
      h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', on: { click: clearFilters } }, t('cert.clear')),
    ),
  );
  function hasFilters() {
    return Boolean(st.state || st.expires_within || st.q);
  }
  function clearFilters() {
    st.state = '';
    st.expires_within = '';
    st.q = '';
    search.value = '';
    stateSel.value = '';
    withinSel.value = '';
    reload();
  }

  // ---- 剩餘 ----
  function remaining(c) {
    const now = fmt.serverNow();
    const end = ms(c.not_after);
    if (c.state === 'revoked' || c.state === 'revocation_pending') return '—';
    if (c.state === 'expired_in_grace') {
      const ago = Math.floor((now - end) / DAY);
      return graceDays != null ? t('cert.remain.inGrace', { d: ago, g: Math.max(0, graceDays - ago) }) : t('cert.remain.inGraceNoPolicy', { d: ago });
    }
    if (c.state === 'expired_beyond_grace') return t('cert.remain.beyond');
    return t('cert.remain.days', { d: Math.max(0, Math.floor((end - now) / DAY)) });
  }

  // ---- 表格 ----
  const revocable = (c) => !['revoked', 'revocation_pending'].includes(c.state);
  const selectable = (c) => (st.state === 'expired_beyond_grace' ? canGrace || canToken || canRevoke : canRevoke && revocable(c));
  const headCb = h('input', { type: 'checkbox', class: 'ds-check', 'aria-label': t('cert.selectAll') });
  headCb.addEventListener('change', () => {
    if (headCb.checked) rows.filter(selectable).forEach((c) => selected.add(c.serial));
    else selected.clear();
    syncChecks();
  });
  function rowCb(c) {
    if (!selectable(c)) return '';
    const cb = h('input', { type: 'checkbox', class: 'ds-check', checked: selected.has(c.serial), 'aria-label': t('cert.selectRow', { host: hostText(c), serial: c.serial }), dataset: { serial: c.serial } });
    cb.addEventListener('change', () => {
      if (cb.checked) selected.add(c.serial);
      else selected.delete(c.serial);
      syncChecks();
    });
    return cb;
  }
  const showSelect = () => canRevoke || (st.state === 'expired_beyond_grace' && (canGrace || canToken));
  function columns() {
    return [
      showSelect() ? { key: 'sel', label: headCb, render: rowCb } : null,
      { key: 'state', label: t('cert.col.state'), render: (c) => certStateBadge(c.state) },
      {
        key: 'serial',
        label: t('cert.col.serial'),
        render: (c) =>
          h('button', { type: 'button', class: 'k-linklike mono', title: c.serial, 'aria-label': t('cert.serialA11y', { serial: shortSerial(c.serial), host: hostText(c) }), dataset: { open: c.serial }, on: { click: () => openDetail(c.serial) } }, shortSerial(c.serial)),
      },
      {
        key: 'host',
        label: t('cert.col.host'),
        cls: 'k-cell-host',
        render: (c) => [session.canAny('hosts:read') ? h('a', { href: `/hosts/${encodeURIComponent(c.agent_id)}` }, hostText(c)) : hostText(c), h('span', { class: 'k-sub mono' }, shortId(c.agent_id))],
      },
      { key: 'group', label: t('cert.col.group'), render: (c) => c.group || '—' },
      { key: 'issued', label: t('cert.col.issued', { tz: fmt.tzLabel() }), render: (c) => fmt.date(c.issued_at) },
      { key: 'expires', label: t('cert.col.expires', { tz: fmt.tzLabel() }), render: (c) => fmt.date(c.not_after) },
      { key: 'remain', label: t('cert.col.remain'), render: remaining },
      { key: 'via', label: t('cert.col.via'), render: (c) => viaText(c.issued_via) },
      {
        key: 'more',
        label: h('span', { class: 'ds-visually-hidden' }, t('pki.col.actions')),
        render: (c) => {
          const b = h('button', { type: 'button', class: 'ds-button ds-button--icon ds-button--sm', 'aria-label': t('cert.more', { host: hostText(c) }), title: t('cert.more', { host: hostText(c) }), 'aria-haspopup': 'menu', 'aria-expanded': 'false' }, icon('more'));
          b.addEventListener('click', () => {
            const items = [{ text: t('cert.menu.detail'), iconName: 'info', onSelect: () => openDetail(c.serial) }];
            if (session.canAny('hosts:read')) items.push({ text: t('cert.menu.host'), iconName: 'server', href: `/hosts/${encodeURIComponent(c.agent_id)}` });
            items.push({ text: t('cert.menu.download'), iconName: 'download', onSelect: () => downloadPem(c.serial) });
            if (canRevoke && revocable(c)) items.push('separator', { text: t('cert.menu.revoke'), iconName: 'shield-x', danger: true, onSelect: () => revoke([c]) });
            openMenu(b, items, { label: t('cert.more', { host: hostText(c) }) });
          });
          return b;
        },
      },
    ].filter(Boolean);
  }

  function viaText(v) {
    return ['token', 'grace_renewal', 'renewal', 'manual', 'import'].includes(v) ? t(`cert.via.${v}`) : v || '—';
  }

  let table = null;
  function makeTable() {
    table = createTable({ caption: t('cert.caption'), columns: columns(), rowKey: (c) => c.serial });
    mount(listBox, table.el);
  }

  function syncChecks() {
    for (const cb of root.querySelectorAll('input[data-serial]')) cb.checked = selected.has(cb.dataset.serial);
    const can = rows.filter(selectable);
    headCb.checked = can.length > 0 && can.every((c) => selected.has(c.serial));
    headCb.indeterminate = selected.size > 0 && !headCb.checked;
    renderBulk();
  }

  function renderBulk() {
    if (!selected.size || st.state === 'pending_issuance') {
      bulkbar.hidden = true;
      mount(bulkbar);
      return;
    }
    const list = rows.filter((c) => selected.has(c.serial));
    const n = list.length;
    const items = [h('span', { class: 'strong' }, t('cert.bulk.selected', { n: fmt.num(n) }))];
    if (st.state === 'expired_beyond_grace') {
      if (canGrace) {
        items.push(button({ text: t('cert.grace.open'), variant: 'primary', size: 'sm', onClick: () => graceOverride(list) }));
      } else {
        // 維運者看得到但不能使用（D12；screens.md §0.2 部分權限：停用＋說明）
        const whyId = 'cert-grace-why';
        items.push(
          h('button', { type: 'button', class: 'ds-button ds-button--primary ds-button--sm', 'aria-disabled': 'true', 'aria-describedby': whyId, on: { click: () => announce(t('cert.grace.adminOnly')) } }, t('cert.grace.open')),
          h('span', { class: 'muted', id: whyId }, t('cert.grace.adminOnly')),
        );
      }
      if (canToken) items.push(button({ text: t('cert.grace.token'), size: 'sm', onClick: () => tokenFor(list) }));
    }
    if (canRevoke && list.some(revocable)) items.push(button({ text: t('cert.bulk.revoke', { n: fmt.num(list.filter(revocable).length) }), variant: 'danger-secondary', size: 'sm', iconName: 'shield-x', onClick: () => revoke(list.filter(revocable)) }));
    items.push(
      h('span', { class: 'k-sep', 'aria-hidden': 'true' }),
      button({
        text: t('cert.bulk.clear'),
        variant: 'ghost',
        size: 'sm',
        onClick: () => {
          selected.clear();
          syncChecks();
          headCb.focus();
        },
      }),
    );
    bulkbar.hidden = false;
    mount(bulkbar, items);
  }

  function showResult(text, focus = true) {
    result.hidden = false;
    mount(result, icon('info'), h('span', null, text));
    if (focus) result.focus();
  }

  // ---- 撤銷（L2） ----
  async function revoke(list) {
    if (!list.length) return;
    const single = list.length === 1;
    const c0 = list[0];
    let conn = null;
    if (single && session.canAny('hosts:read')) {
      try {
        const host = await api(`/api/v1/hosts/${encodeURIComponent(c0.agent_id)}`);
        conn = statusBadge(host.status);
      } catch {
        conn = null;
      }
    }
    const reason = selectField({ label: t('cert.revoke.reason'), name: 'reason', value: 'keyCompromise', options: REASONS.map((r) => [r, t(`cert.reason.${r}`)]), required: true });
    const comment = textareaField({ label: t('cert.revoke.comment'), name: 'comment', required: true, rows: 2, hint: t('cert.revoke.commentHint') });
    const phrase = single ? hostText(c0) : t('cert.revoke.phrase', { n: list.length });
    const offline = mode && mode.pki_mode === 'offline';
    const targets = single
      ? dl([
          [t('cert.col.host'), hostText(c0)],
          [t('cert.revoke.agent'), h('span', { class: 'mono' }, shortId(c0.agent_id))],
          [t('cert.col.group'), c0.group || '—'],
          [t('cert.col.serial'), h('span', { class: 'mono' }, c0.serial)],
          conn ? [t('cert.revoke.conn'), conn] : null,
        ])
      : h('ul', { class: 'k-impact' }, list.slice(0, 10).map((c) => h('li', null, `${hostText(c)}（${shortSerial(c.serial)}）`)), list.length > 10 ? h('li', null, t('cert.revoke.more', { n: fmt.num(list.length - 10) })) : null);
    const res = await actionDialog({
      title: single ? t('cert.revoke.titleOne', { host: hostText(c0) }) : t('cert.revoke.title', { n: fmt.num(list.length) }),
      lead: offline ? t('cert.revoke.leadOffline') : t('cert.revoke.lead'),
      confirmText: single ? t('cert.revoke.confirmOne') : t('cert.revoke.confirm', { n: fmt.num(list.length) }),
      danger: true,
      phrase,
      size: 'lg',
      errorTitle: t('cert.revoke.failed'),
      content: [targets, list.some(isExternal) ? inlineAlert({ kind: 'info', body: t('cert.import.externalNote') }) : null, reason.el, comment.el].filter(Boolean),
      collect() {
        const text = comment.input.value.trim();
        comment.el.setError(text ? '' : t('cert.revoke.commentRequired'));
        if (!text) {
          comment.input.focus();
          return null;
        }
        return { reason: reason.input.value, comment: text };
      },
      submit: (v) => api('/api/v1/pki/certificates/revoke', { method: 'POST', body: { serials: list.map((c) => c.serial), expected_count: list.length, reason: v.reason, comment: v.comment } }),
    });
    if (!res) return;
    for (const c of list) selected.delete(c.serial);
    const s = res.summary || {};
    const parts = [];
    if (s.revoked) parts.push(t('cert.revoke.doneOnline', { n: fmt.num(s.revoked) }));
    if (s.revocation_pending) parts.push(t('cert.revoke.donePending', { n: fmt.num(s.revocation_pending) }));
    if (s.already_revoked) parts.push(t('cert.revoke.already', { n: fmt.num(s.already_revoked) }));
    if (s.not_found) parts.push(t('cert.revoke.notFound', { n: fmt.num(s.not_found) }));
    await reload({ keepResult: true });
    loadKpis();
    showResult(parts.join('；'));
    toast({ kind: 'success', title: t('cert.revoke.toast', { n: fmt.num((s.revoked || 0) + (s.revocation_pending || 0)) }) });
  }

  // ---- 允許再次自動續簽（L1，限管理員，原因必填） ----
  async function graceOverride(list) {
    const ids = [...new Set(list.map((c) => c.agent_id))];
    const until = selectField({ label: t('cert.grace.until'), name: 'until', value: '30', options: ['7', '14', '30', '60', '90'].map((d) => [d, t('cert.grace.days', { d })]) });
    const reason = textareaField({ label: t('cert.grace.reason'), name: 'reason', required: true, rows: 2 });
    const closed = mode && mode.mode === 'closed';
    const also = closed ? checkbox({ label: t('cert.grace.alsoRenewOnly'), name: 'also', checked: true }) : null;
    const res = await actionDialog({
      title: t('cert.grace.title', { n: fmt.num(ids.length) }),
      confirmText: t('cert.grace.confirm'),
      size: 'lg',
      errorTitle: t('cert.grace.failed'),
      content: [
        h('p', null, t('cert.grace.body')),
        until.el,
        closed ? inlineAlert({ kind: 'warning', title: t('cert.grace.closed') }) : h('p', { class: 'muted' }, t('cert.grace.mode', { mode: mode ? t(`cert.mode.${mode.state}`) : '—' })),
        also ? also.el : null,
        reason.el,
      ].filter(Boolean),
      collect() {
        const text = reason.input.value.trim();
        reason.el.setError(text ? '' : t('cert.grace.reasonRequired'));
        if (!text) {
          reason.input.focus();
          return null;
        }
        return { until: new Date(fmt.serverNow() + Number(until.input.value) * DAY).toISOString(), reason: text, also: also ? also.input.checked : false };
      },
      submit: (v) => api('/api/v1/pki/grace-overrides', { method: 'POST', body: { agent_ids: ids, until: v.until, reason: v.reason, also_set_renew_only: v.also } }),
    });
    if (!res) return;
    selected.clear();
    const ok = (res.results || []).filter((r) => r.result === 'ok').length;
    await reload({ keepResult: true });
    showResult(t('cert.grace.done', { n: fmt.num(ok) }));
  }

  // ---- 建立限定這些主機的 token（S12 的建立表單預填） ----
  function tokenFor(list) {
    const ids = [...new Set(list.map((c) => c.agent_id))];
    const groups = [...new Set(list.map((c) => c.group).filter(Boolean))];
    openCreateToken({
      prefill: { name: t('cert.grace.tokenName', { n: ids.length }), group: groups.length === 1 ? groups[0] : '', max_uses: ids.length, allowed_agent_ids: ids, expiry_days: 7 },
    });
  }

  // ---- 詳情 ----
  async function downloadPem(serial) {
    try {
      const c = await api(`/api/v1/pki/certificates/${encodeURIComponent(serial)}`);
      downloadText(`${serial}.pem`, c.cert_pem, 'application/x-pem-file');
    } catch (e) {
      toast({ kind: 'error', title: t('cert.downloadFailed'), desc: e.message });
    }
  }

  async function openDetail(serial) {
    const back = listUrl();
    history.replaceState(null, '', `/certificates/${encodeURIComponent(serial)}`);
    const holder = h('div', null, skeletonBlock(6));
    const closeBtn = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.close'));
    const d = openDialog({
      title: t('cert.detail.title'),
      size: 'lg',
      body: holder,
      actions: [closeBtn],
      initialFocus: closeBtn,
      onClose: () => {
        if (location.pathname.startsWith('/certificates/')) history.replaceState(null, '', back);
      },
    });
    closeBtn.addEventListener('click', () => d.close());
    let c;
    try {
      c = await api(`/api/v1/pki/certificates/${encodeURIComponent(serial)}`);
    } catch (e) {
      mount(holder, errorState(e, { title: t('cert.detail.failed') }));
      return;
    }
    if (!d.el.isConnected) return;
    mount(
      holder,
      dl([
        [t('cert.col.state'), certStateBadge(c.state)],
        [t('cert.col.serial'), h('span', { class: 'mono' }, c.serial)],
        [t('cert.col.host'), session.canAny('hosts:read') ? h('a', { href: `/hosts/${encodeURIComponent(c.agent_id)}` }, hostText(c)) : hostText(c)],
        [t('cert.detail.agentId'), h('span', { class: 'mono' }, c.agent_id)],
        [t('cert.col.group'), c.group],
        [t('cert.detail.validity'), `${fmt.dateTime(c.not_before, { seconds: false })} ～ ${fmt.dateTime(c.not_after, { seconds: false })}（${fmt.tzLabel()}）`],
        [t('cert.col.remain'), remaining(c)],
        [t('cert.col.via'), viaText(c.issued_via)],
        [t('cert.detail.approvedBy'), c.approved_by],
        [t('cert.detail.firstUsed'), fmt.timeEl(c.first_used_at)],
        [t('cert.detail.spki'), h('span', { class: 'mono' }, c.spki_fingerprint_short)],
        [t('cert.detail.ca'), c.ca_id != null ? `#${c.ca_id}` : '—'],
        c.revoked_at ? [t('cert.detail.revokedAt'), fmt.timeEl(c.revoked_at, { mode: 'absolute' })] : null,
        c.revoked_at ? [t('cert.detail.effectiveAt'), c.revoke_effective_at ? fmt.timeEl(c.revoke_effective_at, { mode: 'absolute' }) : t('cert.detail.pendingCrl')] : null,
        c.revocation_reason ? [t('cert.revoke.reason'), REASONS.includes(c.revocation_reason) ? t(`cert.reason.${c.revocation_reason}`) : c.revocation_reason] : null,
        c.revoked_by ? [t('cert.detail.revokedBy'), c.revoked_by] : null,
        c.revoke_comment ? [t('cert.revoke.comment'), c.revoke_comment] : null,
        c.crl_number != null ? [t('cert.detail.crl'), `#${c.crl_number}`] : null,
        [t('cert.detail.requestId'), c.request_id ? h('span', { class: 'mono' }, c.request_id) : '—'],
      ]),
      c.state === 'revocation_pending' ? inlineAlert({ kind: 'warning', title: t('cert.state.revocation_pending'), body: mode && mode.pki_mode === 'offline' ? t('cert.revoke.pendingOffline') : t('cert.revoke.pendingOnline') }) : null,
    );
    const foot = d.el.querySelector('.ds-dialog__foot');
    const dlBtn = button({ text: t('cert.menu.download'), iconName: 'download', cls: 'k-foot-left', onClick: () => downloadText(`${c.serial}.pem`, c.cert_pem, 'application/x-pem-file') });
    foot.prepend(dlBtn);
    if (canRevoke && revocable(c)) {
      foot.insertBefore(
        button({
          text: t('cert.menu.revoke'),
          variant: 'danger-secondary',
          iconName: 'shield-x',
          onClick: () => {
            d.close();
            revoke([c]);
          },
        }),
        closeBtn,
      );
    }
  }

  // ---- 待簽發（離線簽發；PKI.md 第 6 節） ----
  async function renderQueue() {
    mount(listBox, skeletonBlock(4));
    let list;
    try {
      list = await api('/api/v1/pki/signing-queue');
    } catch (e) {
      if (!disposed) mount(listBox, errorState(e, { title: t('cert.queue.failed'), onRetry: renderQueue }));
      return;
    }
    if (disposed) return;
    const actions = [];
    if (canQueue) {
      actions.push(button({ text: t('cert.queue.export'), iconName: 'download', onClick: exportQueue }));
      const file = h('input', { type: 'file', accept: '.json,application/json', class: 'ds-visually-hidden', id: 'cert-import-file', tabindex: '-1', 'aria-label': t('cert.queue.import') });
      file.addEventListener('change', () => importQueue(file));
      actions.push(button({ text: t('cert.queue.import'), iconName: 'upload', onClick: () => file.click() }), file);
    }
    const qt = createTable({
      caption: t('cert.queue.caption'),
      columns: [
        { key: 'code', label: t('enroll.col.code'), render: (r) => h('span', { class: 'mono' }, r.request_code) },
        { key: 'host', label: t('cert.col.host'), render: (r) => r.hostname },
        { key: 'agent', label: t('enroll.col.agent'), render: (r) => h('span', { class: 'mono' }, shortId(r.agent_id)) },
        { key: 'kind', label: t('enroll.col.kind'), render: (r) => kindLabel(r.kind) },
        { key: 'by', label: t('cert.queue.approvedBy'), render: (r) => r.decided_by || '—' },
        { key: 'at', label: t('cert.queue.approvedAt', { tz: fmt.tzLabel() }), render: (r) => fmt.timeEl(r.decided_at) },
      ],
      rowKey: (r) => r.request_id,
    });
    if (list.length) qt.setRows(list, { total: list.length, hasMore: false });
    else qt.setEmpty(emptyState({ title: t('cert.queue.empty'), iconName: 'success' }));
    mount(
      listBox,
      h(
        'div',
        { class: 'k-stack--sm' },
        inlineAlert({ kind: 'info', title: t('cert.queue.title', { n: fmt.num(list.length) }), body: t('cert.queue.body'), actions: actions.length ? actions : null }),
        h('details', { class: 'k-details' }, h('summary', null, t('cert.queue.cli')), codeBlock('kairos pki sign-bundle --in kairos-csr-queue.json --ca-cert issuing.pem --ca-key issuing.key --out kairos-signed-certs.json')),
        qt.el,
      ),
    );
  }

  async function exportQueue() {
    try {
      const doc = await api('/api/v1/pki/signing-queue/export');
      downloadText('kairos-csr-queue.json', `${JSON.stringify(doc, null, 2)}\n`, 'application/json');
      toast({ kind: 'success', title: t('cert.queue.exported') });
    } catch (e) {
      toast({ kind: 'error', title: t('cert.queue.exportFailed'), desc: e.message });
    }
  }

  async function importQueue(fileInput) {
    const f = fileInput.files && fileInput.files[0];
    fileInput.value = '';
    if (!f) return;
    let doc;
    try {
      doc = JSON.parse(await f.text());
    } catch {
      toast({ kind: 'error', title: t('cert.queue.importBadFile') });
      return;
    }
    try {
      const res = await api('/api/v1/pki/signing-queue/import', { method: 'POST', body: doc });
      const s = res.summary || {};
      const bad = (res.results || []).filter((r) => r.result === 'rejected');
      showResult(t('cert.queue.imported', { ok: fmt.num(s.issued || 0), bad: fmt.num(s.rejected || 0) }) + (bad.length ? `：${bad.slice(0, 3).map((r) => `${r.request_id}（${r.reason}）`).join('、')}` : ''));
      if (bad.length) toast({ kind: 'error', title: t('cert.queue.importRejected', { n: fmt.num(bad.length) }), desc: 'KRS-E1305' });
      renderQueue();
      loadKpis();
    } catch (e) {
      toast({ kind: 'error', title: t('cert.queue.importFailed'), desc: e.message });
    }
  }

  // 手動簽發（外部 CA）的既有憑證匯入簽發紀錄（POST /api/v1/pki/certificates/import；PKI.md 第 16 節）
  async function importCertificates() {
    const f = textareaField({ label: t('cert.import.pem'), name: 'pem', rows: 8, mono: true, required: true });
    const r = await actionDialog({
      title: t('cert.import.title'),
      size: 'md',
      confirmText: t('cert.import.submit'),
      // 底部固定說明（設計包 S14「0.9 規格」）：外部 CA 簽發的憑證撤銷後，下一次重新連線時才被拒絕
      content: [h('p', null, t('cert.import.body')), f.el, inlineAlert({ kind: 'info', body: t('cert.import.externalNote') })],
      collect: () => {
        const v = f.input.value.trim();
        if (!v.includes('BEGIN CERTIFICATE')) {
          f.el.setError(t('cert.import.empty'));
          f.input.focus();
          return null;
        }
        f.el.setError('');
        return { pem: v };
      },
      submit: (v) => api('/api/v1/pki/certificates/import', { method: 'POST', body: { certificates: v.pem } }),
      errorTitle: t('cert.import.failed'),
    });
    if (!r || r === true) return;
    const s = r.summary || {};
    const bad = (r.results || []).filter((x) => x.result === 'rejected');
    const text = t('cert.import.done', { ok: fmt.num(s.imported || 0), already: fmt.num(s.already_imported || 0), rejected: fmt.num(s.rejected || 0) });
    showResult(bad.length ? `${text}：${bad.slice(0, 3).map((x) => t('cert.import.rejected', { source: x.source, reason: x.reason })).join('、')}` : text);
    toast({ kind: bad.length ? 'warning' : 'success', title: text });
    reload({ keepResult: true });
    loadKpis();
  }

  // ---- 載入 ----
  const importBtn = session.can('pki.queue:manage')
    ? h('button', { type: 'button', class: 'ds-button ds-button--secondary', id: 'cert-import', on: { click: () => importCertificates() } }, icon('upload'), t('cert.import'))
    : null;
  mount(root, pageHead({ title: t('cert.title'), actions: importBtn }), kpis, alerts, filterbar, result, bulkbar, listBox);

  async function loadKpis() {
    mount(kpis, ['valid', 'expiring', 'revoked', 'revocation_pending', 'crl'].map((k) => kpiSkeleton(t(`cert.kpi.${k}`))));
    const count = (s) => api(`/api/v1/pki/certificates?state=${s}&limit=1`, { raw: true }).then((r) => r.page.total).catch(() => null);
    const [valid, expiring, revoked, pending, inGrace, beyond, crl] = await Promise.all([
      count('valid'),
      count('expiring'),
      count('revoked'),
      count('revocation_pending'),
      count('expired_in_grace'),
      count('expired_beyond_grace'),
      api('/api/v1/pki/crl').catch(() => null),
    ]);
    if (disposed) return;
    const v = (n) => (n == null ? '—' : fmt.num(n));
    const tile = (status, key, n, state) => kpiTile({ status, label: t(`cert.kpi.${key}`), value: v(n), tone: Boolean(n), href: `/certificates?state=${state}` });
    let crlTile = kpiTile({ label: t('cert.kpi.crl'), value: '—', href: '/certificates/ca' });
    const next = crl && (crl.crls || []).map((c) => c.current && ms(c.current.next_update)).filter(Boolean);
    if (next && next.length) {
      const days = Math.floor((Math.min(...next) - fmt.serverNow()) / DAY);
      const status = days < 0 ? 'critical' : days <= 7 ? 'warning' : 'online';
      crlTile = kpiTile({ status, label: t('cert.kpi.crl'), value: days < 0 ? t('cert.kpi.crlExpired') : t('cert.kpi.crlDays', { d: days }), href: '/certificates/ca' });
    }
    mount(
      kpis,
      tile('online', 'valid', valid, 'valid'),
      tile('warning', 'expiring', expiring, 'expiring'),
      kpiTile({ label: t('cert.kpi.revoked'), value: v(revoked), href: '/certificates?state=revoked' }),
      tile('warning', 'revocation_pending', pending, 'revocation_pending'),
      crlTile,
      tile('critical', 'expired_in_grace', inGrace, 'expired_in_grace'),
      tile('critical', 'expired_beyond_grace', beyond, 'expired_beyond_grace'),
    );
    kpis.removeAttribute('aria-busy');
  }

  async function loadSide() {
    const [m, settings] = await Promise.all([
      fetchMode(),
      session.can('settings:read') ? api('/api/v1/settings').catch(() => null) : Promise.resolve(null),
    ]);
    if (disposed) return;
    mode = m;
    const g = settings && (settings.settings || []).find((x) => x.key === 'pki.policy.expired_renew_grace_days');
    graceDays = g ? Number(g.value) : null;
    renderAlerts();
    if (rows.length && table) table.setRows(rows, { total: rows.length, hasMore: false });
  }

  function renderAlerts() {
    const notes = [issuanceAlert(mode, { where: 'certs' })];
    if (st.state === 'expired_in_grace') notes.push(inlineAlert({ kind: 'info', title: t('cert.view.inGrace'), body: graceDays != null ? t('cert.view.inGraceBody', { g: graceDays }) : t('cert.view.inGraceBodyNoPolicy') }));
    if (st.state === 'expired_beyond_grace') notes.push(inlineAlert({ kind: 'info', title: t('cert.view.beyond'), body: graceDays != null ? t('cert.view.beyondBody', { g: graceDays }) : t('cert.view.beyondBodyNoPolicy') }));
    mount(alerts, notes.filter(Boolean));
  }

  function urlParams() {
    return { state: st.state, expires_within: st.expires_within, q: st.q };
  }

  async function reload({ keepResult = false } = {}) {
    if (location.pathname === '/certificates') replaceQuery(urlParams());
    if (!keepResult) result.hidden = true;
    renderAlerts();
    if (st.state === 'pending_issuance') {
      rows = [];
      selected.clear();
      renderBulk();
      filterbar.querySelector('.ds-filterbar__row').hidden = true;
      withinSel.closest('label').hidden = true;
      renderQueue();
      return;
    }
    filterbar.querySelector('.ds-filterbar__row').hidden = false;
    withinSel.closest('label').hidden = false;
    makeTable();
    if (ctrl) ctrl.abort();
    ctrl = new AbortController();
    table.setLoading();
    try {
      const res = await api(`/api/v1/pki/certificates${qs({ ...urlParams(), limit: 1000 })}`, { raw: true, signal: ctrl.signal });
      if (disposed) return;
      rows = res.data;
      for (const s of [...selected]) if (!rows.some((c) => c.serial === s)) selected.delete(s);
      if (!rows.length) {
        table.setEmpty(
          hasFilters()
            ? emptyState({ title: t('cert.empty.noMatch'), iconName: 'filter', actions: [h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: clearFilters } }, t('cert.clear'))] })
            : emptyState({ title: t('cert.empty.title'), iconName: 'certificate', actions: session.can('enroll:read') ? [h('a', { class: 'ds-button ds-button--secondary', href: '/enrollment/tokens' }, t('cert.empty.toTokens'))] : null }),
        );
      } else {
        table.setRows(rows, { total: res.page.total, hasMore: false });
      }
      syncChecks();
    } catch (e) {
      if (e.name === 'AbortError' || disposed) return;
      table.setError(e, reload, t('cert.failed'));
    }
  }

  loadKpis();
  loadSide();
  reload();
  if (ctx.params.serial) openDetail(ctx.params.serial);
  return {
    dispose() {
      disposed = true;
      clearTimeout(searchTimer);
      if (ctrl) ctrl.abort();
    },
  };
}
