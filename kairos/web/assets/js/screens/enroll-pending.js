// S13 待核准佇列（設計包 kairos/screens.md S13；API.md 第 11.3 節）。
//
// - 檢視：待核准（預設）／最近結束（view=closed，30 天）；篩選 token、異常旗標（無異常／有異常）、類型、搜尋
//   （申請編號含不含連字號、主機名稱、IP、agent_id 前綴）；條件寫在網址。
// - 「全選」只選無異常的申請，並說明排除了幾筆（S13）；批次核准（L1）、拒絕（L2：單筆輸入主機名稱、
//   批次輸入「拒絕 N 筆」，原因必填）。結果訊息取得焦點；部分失敗不是全有全無（API 逐筆回報）。
// - 申請詳情以對話框呈現（設計系統尚未定義 Drawer）：申請編號大字可複製、公鑰指紋前 8 碼、旗標說明與建議。
// - 每 10 秒重新確認待核准的清單：有新申請或有申請結束（撤回、他人處理、逾期）時顯示「清單有變化 · 更新列表」，
//   不讓列跳動；已勾選但已結束的申請自動取消勾選並說明。
// - 0.7.0 隱藏：授權用量（S22 之後）、OS 與來源網段篩選（API 沒有這兩個條件）、排隊中的主機數與 ProgressBar
//   （核准為同步回應）、佇列已滿的提示（沒有資料來源）。

import { h, icon, announce, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { replaceQuery } from '../core/router.js';
import { pageHead, emptyState, errorState, field, inlineAlert, copyButton, dl, skeletonBlock, button } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { createTable } from '../ui/table.js';
import { actionDialog, selectField, textareaField, linkTabs, fetchMode, issuanceAlert, flagBadge, flagLabel, FLAGS, kindLabel, requestStateBadge, shortId, osLine, loadTokenMap } from './pki-common.js';

const REASON_CODES = ['unknown_host', 'source_mismatch', 'duplicate', 'other'];
const POLL_MS = 10000;

function parseState(q) {
  return {
    view: q.get('view') === 'closed' ? 'closed' : 'pending',
    token: q.get('token') || '',
    flag: ['none', 'any', ...FLAGS].includes(q.get('flag')) ? q.get('flag') : '',
    kind: ['enroll', 'grace_renewal', 'renewal'].includes(q.get('kind')) ? q.get('kind') : '',
    q: q.get('q') || '',
  };
}

/** 主機名稱（申請編號）：確認對話框、結果訊息 */
function who(r) {
  return t('enroll.who', { host: r.hostname, code: r.request_code });
}

export function render(ctx) {
  const root = ctx.root;
  const st = parseState(ctx.query);
  const canDecide = session.can('enroll.requests:decide');
  let rows = [];
  let total = 0;
  let tokens = new Map();
  let disposed = false;
  let ctrl = null;
  let pollTimer = null;
  let knownIds = null;
  const selected = new Set();

  const alerts = h('div', { class: 'k-stack--sm' });
  const tabsBox = h('div');
  const result = h('div', { class: 'ds-table__summary', role: 'status', tabindex: '-1', hidden: true });
  const changeBar = h('div', { class: 'ds-table__summary', hidden: true });
  const bulkbar = h('div', { class: 'ds-table__bulkbar', hidden: true, role: 'region', 'aria-label': t('enroll.bulk.region') });

  // ---- 篩選 ----
  const search = h('input', { class: 'ds-input', type: 'search', name: 'q', value: st.q, autocomplete: 'off', spellcheck: 'false', placeholder: t('enroll.searchPlaceholder') });
  const searchField = field({ label: t('enroll.search'), input: search });
  searchField.classList.add('ds-filterbar__search');
  let searchTimer = null;
  search.addEventListener('input', () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      st.q = search.value.trim();
      reload();
    }, 300);
  });
  search.addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter') {
      ev.preventDefault();
      clearTimeout(searchTimer);
      st.q = search.value.trim();
      reload();
    }
  });
  const tokenSel = h('select', { class: 'ds-select', name: 'token' });
  tokenSel.addEventListener('change', () => {
    st.token = tokenSel.value;
    reload();
  });
  const kindSel = h(
    'select',
    { class: 'ds-select', name: 'kind' },
    h('option', { value: '' }, t('enroll.filter.all')),
    ['enroll', 'grace_renewal', 'renewal'].map((k) => h('option', { value: k, selected: st.kind === k }, kindLabel(k))),
  );
  kindSel.addEventListener('change', () => {
    st.kind = kindSel.value;
    reload();
  });
  const chipRow = h('div', { class: 'ds-filterbar__row', role: 'group', 'aria-label': t('enroll.filter.flags') });
  const filterbar = h(
    'section',
    { class: 'ds-filterbar', 'aria-label': t('enroll.filters') },
    h('div', { class: 'ds-filterbar__row' }, searchField),
    h(
      'div',
      { class: 'k-filter-selects' },
      h('label', { class: 'k-labelled-select' }, t('enroll.filter.token'), tokenSel),
      h('label', { class: 'k-labelled-select' }, t('enroll.filter.kind'), kindSel),
      h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', on: { click: clearFilters } }, t('enroll.clear')),
    ),
    chipRow,
  );

  function renderTokenSelect() {
    const list = [...tokens.values()].sort((a, b) => a.name.localeCompare(b.name));
    mount(
      tokenSel,
      h('option', { value: '' }, t('enroll.filter.all')),
      list.map((x) => h('option', { value: x.token_id, selected: st.token === x.token_id }, `${x.name}（${x.hint}）`)),
      st.token && !tokens.has(st.token) ? h('option', { value: st.token, selected: true }, st.token) : null,
    );
  }

  function renderChips(counts) {
    if (st.view !== 'pending') {
      mount(chipRow);
      return;
    }
    mount(
      chipRow,
      ['none', 'any'].map((f) => {
        const on = st.flag === f;
        return h(
          'button',
          {
            type: 'button',
            class: 'ds-filterbar__chip',
            'aria-pressed': on ? 'true' : 'false',
            dataset: { flag: f },
            on: {
              click: () => {
                st.flag = on ? '' : f;
                reload();
              },
            },
          },
          icon(f === 'none' ? 'success' : 'status-warning'),
          t(`enroll.flag.chip.${f}`),
          counts && counts[f] != null ? h('span', { class: 'k-n' }, fmt.num(counts[f])) : null,
        );
      }),
    );
  }

  function hasFilters() {
    return Boolean(st.token || st.flag || st.kind || st.q);
  }
  function clearFilters() {
    st.token = '';
    st.flag = '';
    st.kind = '';
    st.q = '';
    search.value = '';
    kindSel.value = '';
    reload();
  }

  // ---- 表格 ----
  const headCb = h('input', { type: 'checkbox', class: 'ds-check', 'aria-label': t('enroll.selectAllClean') });
  headCb.addEventListener('change', () => {
    if (headCb.checked) {
      const clean = rows.filter((r) => r.state === 'pending_approval' && !(r.flags || []).length);
      const flagged = rows.filter((r) => r.state === 'pending_approval' && (r.flags || []).length);
      for (const r of clean) selected.add(r.request_id);
      if (flagged.length) announceResult(t('enroll.excluded', { n: fmt.num(flagged.length) }), false);
    } else {
      selected.clear();
    }
    syncChecks();
  });

  function rowCheckbox(r) {
    const cb = h('input', {
      type: 'checkbox',
      class: 'ds-check',
      checked: selected.has(r.request_id),
      'aria-label': t('enroll.selectRow', { host: r.hostname, code: r.request_code }),
      dataset: { rid: r.request_id },
    });
    cb.addEventListener('change', () => {
      if (cb.checked) selected.add(r.request_id);
      else selected.delete(r.request_id);
      syncChecks();
    });
    return cb;
  }

  function hostButton(r) {
    // 可及名稱包含申請編號（螢幕報讀使用者逐列比對；S13）
    return h(
      'button',
      {
        type: 'button',
        class: 'k-host-name k-linklike',
        'aria-label': t('enroll.hostA11y', { host: r.hostname, code: r.request_code }),
        dataset: { open: r.request_id },
        on: { click: () => openDetail(r.request_id) },
      },
      r.hostname,
    );
  }

  const tokenName = (id) => {
    const x = id && tokens.get(id);
    return x ? x.name : id ? shortId(id) : '—';
  };

  const pendingCols = [
    canDecide ? { key: 'sel', label: headCb, render: rowCheckbox } : null,
    { key: 'code', label: t('enroll.col.code'), render: (r) => h('span', { class: 'mono' }, r.request_code) },
    { key: 'host', label: t('enroll.col.host'), cls: 'k-cell-host', render: hostButton },
    { key: 'agent', label: t('enroll.col.agent'), render: (r) => h('span', { class: 'mono', title: r.agent_id }, shortId(r.agent_id)) },
    { key: 'ip', label: t('enroll.col.ip'), render: (r) => h('span', { class: 'mono' }, r.source_ip || '—') },
    { key: 'os', label: t('enroll.col.os'), render: (r) => osLine(r.os) },
    { key: 'token', label: t('enroll.col.token'), render: (r) => tokenName(r.token_id) },
    { key: 'kind', label: t('enroll.col.kind'), render: (r) => kindLabel(r.kind) },
    { key: 'created', label: t('enroll.col.created', { tz: fmt.tzLabel() }), render: (r) => fmt.timeEl(r.created_at) },
    {
      key: 'flags',
      label: t('enroll.col.flags'),
      render: (r) => {
        const list = [...(r.flags || []).map(flagBadge)];
        if (r.state === 'approved') list.unshift(requestStateBadge('approved'));
        return list.length ? h('span', { class: 'k-row' }, list) : '—';
      },
    },
  ].filter(Boolean);
  const closedCols = [
    { key: 'code', label: t('enroll.col.code'), render: (r) => h('span', { class: 'mono' }, r.request_code) },
    { key: 'host', label: t('enroll.col.host'), cls: 'k-cell-host', render: hostButton },
    { key: 'ip', label: t('enroll.col.ip'), render: (r) => h('span', { class: 'mono' }, r.source_ip || '—') },
    { key: 'token', label: t('enroll.col.token'), render: (r) => tokenName(r.token_id) },
    { key: 'created', label: t('enroll.col.created', { tz: fmt.tzLabel() }), render: (r) => fmt.timeEl(r.created_at) },
    { key: 'closed', label: t('enroll.col.closed', { tz: fmt.tzLabel() }), render: (r) => fmt.timeEl(r.closed_at || r.withdrawn_at || r.decided_at) },
    { key: 'result', label: t('enroll.col.result'), render: (r) => requestStateBadge(r.state) },
    { key: 'by', label: t('enroll.col.by'), render: closedBy },
  ];
  const table = createTable({
    caption: st.view === 'closed' ? t('enroll.caption.closed') : t('enroll.caption.pending'),
    columns: st.view === 'closed' ? closedCols : pendingCols,
    rowKey: (r) => r.request_id,
  });

  function closedBy(r) {
    if (r.state === 'withdrawn') return t('enroll.by.withdrawn', { how: r.withdraw_reason ? withdrawText(r.withdraw_reason) : '—' });
    if (r.state === 'rejected') return [r.decided_by || '—', r.reject_reason ? h('span', { class: 'k-sub' }, r.reject_reason) : null];
    if (r.state === 'request_expired') return t('enroll.by.expired');
    return r.decided_by || '—';
  }

  function withdrawText(reason) {
    return reason === 'uninstall_purge' ? t('enroll.withdraw.purge') : reason === 'cancel' || reason === 'user_cancel' ? t('enroll.withdraw.cancel') : reason;
  }

  // ---- 批次操作列 ----
  function syncChecks() {
    for (const cb of root.querySelectorAll('input[data-rid]')) cb.checked = selected.has(cb.dataset.rid);
    const pend = rows.filter((r) => r.state === 'pending_approval');
    headCb.checked = pend.length > 0 && pend.filter((r) => !(r.flags || []).length).every((r) => selected.has(r.request_id)) && selected.size > 0;
    headCb.indeterminate = selected.size > 0 && !headCb.checked;
    renderBulk();
  }

  function selectedRows() {
    return rows.filter((r) => selected.has(r.request_id));
  }

  function renderBulk() {
    if (!canDecide || st.view !== 'pending' || !selected.size) {
      bulkbar.hidden = true;
      mount(bulkbar);
      return;
    }
    const n = selected.size;
    bulkbar.hidden = false;
    mount(
      bulkbar,
      h('span', { class: 'strong' }, t('enroll.bulk.selected', { n: fmt.num(n) })),
      button({ text: t('enroll.bulk.approve', { n: fmt.num(n) }), variant: 'primary', size: 'sm', iconName: 'check', onClick: () => approve(selectedRows()) }),
      button({ text: t('enroll.bulk.reject'), variant: 'danger-secondary', size: 'sm', iconName: 'close', onClick: () => reject(selectedRows()) }),
      h('span', { class: 'k-sep', 'aria-hidden': 'true' }),
      button({
        text: t('enroll.bulk.clear'),
        variant: 'ghost',
        size: 'sm',
        onClick: () => {
          selected.clear();
          syncChecks();
          headCb.focus();
        },
      }),
    );
  }

  function announceResult(text, focus = true, kind = 'info') {
    result.hidden = false;
    mount(result, icon(kind === 'error' ? 'status-warning' : 'info'), h('span', null, text));
    if (focus) result.focus();
    else announce(text);
  }

  // ---- 核准（L1）與拒絕（L2） ----
  function summaryText(res, list) {
    const s = res.summary || {};
    const byId = new Map(list.map((r) => [r.request_id, r]));
    const named = (kind) =>
      (res.results || [])
        .filter((x) => x.result === kind)
        .slice(0, 3)
        .map((x) => (byId.get(x.request_id) ? who(byId.get(x.request_id)) : x.request_id))
        .join('、');
    const parts = [];
    const ok = (s.issued || 0) + (s.approved || 0);
    parts.push(t('enroll.result.approved', { n: fmt.num(ok) }));
    if (s.approved) parts.push(t('enroll.result.awaiting', { n: fmt.num(s.approved) }));
    if (s.withdrawn) parts.push(t('enroll.result.withdrawn', { n: fmt.num(s.withdrawn), who: named('withdrawn') }));
    if (s.already_decided) parts.push(t('enroll.result.already', { n: fmt.num(s.already_decided) }));
    if (s.license_exhausted) parts.push(t('enroll.result.license', { n: fmt.num(s.license_exhausted) }));
    const failed = (s.failed || 0) + (s.not_found || 0);
    if (failed) parts.push(t('enroll.result.failed', { n: fmt.num(failed), who: named('failed') || named('not_found') }));
    return { text: parts.join('；'), failed };
  }

  async function approve(list, { fromDetail } = {}) {
    if (!list.length) return;
    const codes = list.length <= 3 ? h('ul', { class: 'k-impact' }, list.map((r) => h('li', null, who(r)))) : null;
    const mode = await fetchMode();
    const offline = mode && mode.issuance && mode.issuance.status !== 'online';
    const res = await actionDialog({
      title: list.length === 1 ? t('enroll.approve.titleOne', { who: who(list[0]) }) : t('enroll.approve.title', { n: fmt.num(list.length) }),
      confirmText: list.length === 1 ? t('enroll.approve.confirmOne') : t('enroll.approve.confirm', { n: fmt.num(list.length) }),
      errorTitle: t('enroll.approve.failed'),
      content: [
        h('p', null, t('enroll.approve.body')),
        codes,
        offline ? inlineAlert({ kind: 'info', title: t('enroll.approve.offline') }) : null,
      ].filter(Boolean),
      submit: () => api('/api/v1/enroll/requests/approve', { method: 'POST', body: { request_ids: list.map((r) => r.request_id) } }),
    });
    if (!res) return;
    for (const r of list) selected.delete(r.request_id);
    await afterDecision(res, list, 'approve', fromDetail);
  }

  async function reject(list, { fromDetail } = {}) {
    if (!list.length) return;
    const single = list.length === 1;
    const phrase = single ? list[0].hostname : t('enroll.reject.phrase', { n: list.length });
    const code = selectField({
      label: t('enroll.reject.code'),
      name: 'reason_code',
      value: 'unknown_host',
      options: REASON_CODES.map((c) => [c, t(`enroll.reason.${c}`)]),
      required: true,
    });
    const reason = textareaField({ label: t('enroll.reject.reason'), name: 'reason', hint: t('enroll.reject.reasonHint'), required: true, rows: 2 });
    const res = await actionDialog({
      title: single ? t('enroll.reject.titleOne', { who: who(list[0]) }) : t('enroll.reject.title', { n: fmt.num(list.length) }),
      lead: t('enroll.reject.lead'),
      confirmText: single ? t('enroll.reject.confirmOne') : t('enroll.reject.confirm', { n: fmt.num(list.length) }),
      danger: true,
      phrase,
      errorTitle: t('enroll.reject.failed'),
      content: [list.length <= 3 && !single ? h('ul', { class: 'k-impact' }, list.map((r) => h('li', null, who(r)))) : null, code.el, reason.el].filter(Boolean),
      collect() {
        const text = reason.input.value.trim();
        reason.el.setError(text ? '' : t('enroll.reject.reasonRequired'));
        if (!text) {
          reason.input.focus();
          return null;
        }
        return { reason_code: code.input.value, reason: text };
      },
      submit: (v) =>
        api('/api/v1/enroll/requests/reject', {
          method: 'POST',
          body: { request_ids: list.map((r) => r.request_id), expected_count: list.length, reason_code: v.reason_code, reason: v.reason },
        }),
    });
    if (!res) return;
    for (const r of list) selected.delete(r.request_id);
    await afterDecision(res, list, 'reject', fromDetail);
  }

  async function afterDecision(res, list, kind, fromDetail) {
    const idx = fromDetail ? rows.findIndex((r) => r.request_id === list[0].request_id) : -1;
    let text;
    let failed = 0;
    if (kind === 'approve') {
      ({ text, failed } = summaryText(res, list));
    } else {
      const s = res.summary || {};
      text = t('enroll.result.rejected', { n: fmt.num(s.rejected || 0) });
      if (s.withdrawn) text += `；${t('enroll.result.withdrawnReject', { n: fmt.num(s.withdrawn) })}`;
      if (s.already_decided) text += `；${t('enroll.result.already', { n: fmt.num(s.already_decided) })}`;
      failed = (s.failed || 0) + (s.not_found || 0);
    }
    await reload({ keepResult: true });
    text += `${t('enroll.result.remaining', { n: fmt.num(total) })}`;
    if (failed) toast({ kind: 'error', title: t('enroll.result.toastFailed', { n: fmt.num(failed) }), desc: text });
    if (fromDetail && idx >= 0) {
      // 從詳情單筆處理後，焦點移到原本位置的下一筆（S13）
      announceResult(text, false, failed ? 'error' : 'info');
      const next = root.querySelectorAll('button[data-open]')[Math.min(idx, rows.length - 1)];
      if (next) next.focus();
      else result.focus();
    } else {
      announceResult(text, true, failed ? 'error' : 'info');
    }
  }

  // ---- 詳情（Drawer 的替代：對話框） ----
  async function openDetail(id) {
    const holder = h('div', null, skeletonBlock(5));
    const actions = [];
    const closeBtn = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.close'));
    actions.push(closeBtn);
    const d = openDialog({ title: t('enroll.detail.title'), size: 'lg', body: holder, actions, initialFocus: closeBtn });
    closeBtn.addEventListener('click', () => d.close());
    let r;
    try {
      r = await api(`/api/v1/enroll/requests/${encodeURIComponent(id)}`);
    } catch (e) {
      mount(holder, errorState(e, { title: t('enroll.detail.failed') }));
      return;
    }
    if (!d.el.isConnected) return;
    const noteId = 'rq-state-note';
    const pending = r.state === 'pending_approval';
    const flagRows = (r.flags || []).map((f) => h('li', null, h('strong', null, flagLabel(f)), '：', t(`enroll.flagHelp.${FLAGS.includes(f) ? f : 'other'}`)));
    mount(
      holder,
      !pending && r.state === 'withdrawn'
        ? h('div', { 'aria-live': 'polite', id: noteId }, inlineAlert({ kind: 'info', title: t('enroll.detail.withdrawn', { at: fmt.dateTime(r.withdrawn_at, { seconds: false }) }), body: r.withdraw_reason ? withdrawText(r.withdraw_reason) : null }))
        : !pending
          ? h('div', { id: noteId }, inlineAlert({ kind: 'info', title: t('enroll.detail.closed'), body: h('span', null, requestStateBadge(r.state)) }))
          : null,
      h('p', { class: 'muted' }, t('enroll.detail.codeLabel')),
      h('div', { class: 'k-row' }, h('code', { class: 'k-req-code mono' }, r.request_code), copyButton(r.request_code, t('enroll.detail.copyCode'))),
      dl([
        [t('enroll.detail.spki'), h('span', { class: 'mono' }, r.spki_fingerprint_short)],
        [t('enroll.detail.expires'), fmt.timeEl(r.expires_at, { mode: 'absolute' })],
        [t('enroll.col.host'), r.hostname],
        r.display_name ? [t('enroll.detail.displayName'), r.display_name] : null,
        [t('enroll.detail.agentId'), h('span', { class: 'mono' }, r.agent_id)],
        [t('enroll.col.ip'), h('span', { class: 'mono' }, r.source_ip || '—')],
        [t('enroll.col.os'), osLine(r.os)],
        [t('enroll.detail.version'), r.pulse_version],
        [t('enroll.col.token'), tokenName(r.token_id)],
        [t('enroll.col.kind'), kindLabel(r.kind)],
        [t('enroll.detail.verification'), t(`enroll.verify.${['ca_pin', 'ca_file', 'tofu'].includes(r.server_verification) ? r.server_verification : 'other'}`)],
        [t('enroll.detail.csr'), t('enroll.detail.csrText', { id: r.agent_id })],
        [t('enroll.detail.fingerprint'), r.host_fingerprint ? h('span', { class: 'mono' }, r.host_fingerprint) : '—'],
        [t('enroll.col.created', { tz: fmt.tzLabel() }), fmt.timeEl(r.created_at)],
        r.decided_at ? [t('enroll.detail.decided'), [fmt.timeEl(r.decided_at), h('span', { class: 'k-sub' }, r.decided_by || '')]] : null,
        r.reject_reason ? [t('enroll.detail.rejectReason'), r.reject_reason] : null,
        r.issue_error ? [t('enroll.detail.issueError'), r.issue_error] : null,
        [t('enroll.detail.requestId'), h('span', { class: 'mono' }, r.request_id)],
      ]),
      flagRows.length ? [h('h3', { class: 'ds-card__title--sm' }, t('enroll.detail.flags')), h('ul', { class: 'k-impact' }, flagRows)] : null,
    );
    if (pending && canDecide) {
      const ap = button({ text: t('enroll.approve.confirmOne'), variant: 'primary', onClick: () => { d.close(); approve([r], { fromDetail: true }); } });
      const rj = button({ text: t('enroll.reject.confirmOne'), variant: 'danger-secondary', onClick: () => { d.close(); reject([r], { fromDetail: true }); } });
      d.el.querySelector('.ds-dialog__foot').prepend(rj, ap);
    }
  }

  // ---- 載入 ----
  mount(root, pageHead({ title: t('enroll.title') }), alerts, tabsBox, filterbar, changeBar, result, bulkbar, table.el);

  async function loadSide() {
    const [tk, mode, pendAll, closedAll] = await Promise.all([
      loadTokenMap(),
      fetchMode(),
      api('/api/v1/enroll/requests?view=pending&limit=1', { raw: true }).catch(() => null),
      api('/api/v1/enroll/requests?view=closed&limit=1000', { raw: true }).catch(() => null),
    ]);
    if (disposed) return;
    tokens = tk;
    renderTokenSelect();
    mount(
      tabsBox,
      linkTabs(t('enroll.tabs'), [
        { id: 'pending', href: '/enrollment/pending', text: t('enroll.tab.pending'), count: pendAll ? fmt.num(pendAll.page.total) : null, current: st.view === 'pending' },
        { id: 'closed', href: '/enrollment/pending?view=closed', text: t('enroll.tab.closed'), count: closedAll ? fmt.num(closedAll.page.total) : null, current: st.view === 'closed' },
      ]),
    );
    const notes = [issuanceAlert(mode, { where: 'pending' })];
    const clock = mode && mode.stats && mode.stats.clock_rejected_hosts_15m;
    if (clock) notes.push(inlineAlert({ kind: 'info', title: t('enroll.note.clock', { n: fmt.num(clock) }), code: 'KRS-W8302', body: t('enroll.note.clockBody') }));
    const dayAgo = fmt.serverNow() - 86400000;
    const withdrawn = closedAll ? closedAll.data.filter((r) => r.state === 'withdrawn' && Date.parse(r.withdrawn_at || r.closed_at || 0) >= dayAgo).length : 0;
    if (withdrawn && st.view === 'pending') {
      notes.push(inlineAlert({ kind: 'info', title: t('enroll.note.withdrawn', { n: fmt.num(withdrawn) }), actions: [h('a', { class: 'ds-button ds-button--secondary ds-button--sm', href: '/enrollment/pending?view=closed' }, t('enroll.note.view'))] }));
    }
    const low = [...tokens.values()].filter((x) => x.state === 'active' && x.max_uses > 0 && x.used / x.max_uses >= 0.9);
    if (low.length) notes.push(inlineAlert({ kind: 'info', title: t('enroll.note.tokenLow', { name: low[0].name, n: fmt.num(low[0].remaining_uses ?? low[0].max_uses - low[0].used) }) }));
    mount(alerts, notes.filter(Boolean));
  }

  async function loadFlagCounts() {
    if (st.view !== 'pending') {
      renderChips(null);
      return;
    }
    const base = { view: 'pending', token: st.token, kind: st.kind, q: st.q, limit: 1 };
    try {
      const [n, a] = await Promise.all([
        api(`/api/v1/enroll/requests${qs({ ...base, flag: 'none' })}`, { raw: true }),
        api(`/api/v1/enroll/requests${qs({ ...base, flag: 'any' })}`, { raw: true }),
      ]);
      if (!disposed) renderChips({ none: n.page.total, any: a.page.total });
    } catch {
      if (!disposed) renderChips(null);
    }
  }

  function params() {
    return { view: st.view === 'closed' ? 'closed' : null, token: st.token, flag: st.flag, kind: st.kind, q: st.q };
  }

  async function reload({ keepResult = false } = {}) {
    replaceQuery(params());
    if (!keepResult) result.hidden = true;
    changeBar.hidden = true;
    if (ctrl) ctrl.abort();
    ctrl = new AbortController();
    const signal = ctrl.signal;
    table.setLoading();
    loadFlagCounts();
    try {
      const res = await api(`/api/v1/enroll/requests${qs({ ...params(), view: st.view, limit: 1000 })}`, { raw: true, signal });
      if (disposed) return;
      rows = res.data;
      total = st.view === 'pending' && !hasFilters() ? res.page.total : await pendingTotal();
      if (st.view === 'pending') knownIds = new Set(rows.map((r) => r.request_id));
      // 已不在清單中的勾選取消
      for (const id of [...selected]) if (!rows.some((r) => r.request_id === id)) selected.delete(id);
      if (!rows.length) {
        table.setEmpty(emptyFor());
      } else {
        table.setRows(rows, { total: res.page.total, hasMore: false });
      }
      syncChecks();
    } catch (e) {
      if (e.name === 'AbortError' || disposed) return;
      table.setError(e, reload, t('enroll.failed'));
    }
  }

  async function pendingTotal() {
    try {
      const r = await api('/api/v1/enroll/requests?view=pending&limit=1', { raw: true });
      return r.page.total;
    } catch {
      return 0;
    }
  }

  function emptyFor() {
    if (hasFilters()) {
      return emptyState({ title: t('enroll.empty.noMatch'), iconName: 'filter', actions: [h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: clearFilters } }, t('enroll.clear'))] });
    }
    if (st.view === 'closed') return emptyState({ title: t('enroll.empty.closed'), iconName: 'success' });
    const active = [...tokens.values()].filter((x) => x.state === 'active');
    return emptyState({
      title: t('enroll.empty.pending'),
      body: active.length ? t('enroll.empty.tokenLeft', { name: active[0].name, n: fmt.num(active[0].remaining_uses ?? 0) }) : null,
      iconName: 'success',
    });
  }

  // ---- 定期確認（不重排；顯示「清單有變化」） ----
  async function poll() {
    if (disposed || st.view !== 'pending' || !knownIds || document.visibilityState !== 'visible') return;
    try {
      const res = await api(`/api/v1/enroll/requests${qs({ ...params(), view: 'pending', limit: 1000 })}`, { raw: true });
      if (disposed) return;
      const now = new Set(res.data.map((r) => r.request_id));
      const added = [...now].filter((id) => !knownIds.has(id)).length;
      const gone = [...knownIds].filter((id) => !now.has(id));
      const goneSelected = gone.filter((id) => selected.has(id));
      if (goneSelected.length) {
        for (const id of goneSelected) selected.delete(id);
        syncChecks();
        announceResult(t('enroll.change.deselected', { n: fmt.num(goneSelected.length) }), false);
      }
      if (added || gone.length) {
        changeBar.hidden = false;
        mount(
          changeBar,
          h('span', { role: 'status' }, t('enroll.change.text', { added: fmt.num(added), gone: fmt.num(gone.length) })),
          h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => reload() } }, icon('refresh'), t('enroll.change.refresh')),
        );
      }
    } catch {
      /* 下次再試 */
    }
  }

  renderChips(null);
  loadSide().then(() => {
    if (!disposed && rows.length) table.setRows(rows, { total: rows.length, hasMore: false });
    if (!disposed) syncChecks();
  });
  reload();
  pollTimer = setInterval(poll, POLL_MS);
  return {
    dispose() {
      disposed = true;
      clearInterval(pollTimer);
      clearTimeout(searchTimer);
      if (ctrl) ctrl.abort();
    },
  };
}
