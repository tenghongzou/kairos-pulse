// S18 稽核日誌（/audit；設計包 screens.md S18「0.6 後端現況」；API.md 第 12.8 節）。
//
// - 查詢：GET /api/v1/audit（新到舊、游標分頁；執行者、類別、目標、結果、期間），條件寫在網址（從其他畫面帶篩選進入）。
//   只有 audit:read_own（維運者）時伺服器固定為自己的操作（meta.own_only），畫面隱藏「執行者」並說明。
// - 驗證完整性（audit:read）：POST /api/v1/audit/verify，顯示檢查筆數、第一個斷點（KRS-E6201）與 HMAC 的邊界
//   （since_id 之前的紀錄只有雜湊鏈）。
// - 匯出（audit:export）：只有 JSONL（可離線驗證）；CSV、外送 syslog／SIEM、完整性告警設定、保留期限的調整隱藏
//   （INTEGRATION.md §28.6）。
// - 詳情：對話框列出完整欄位、細節（變更前後）、前一筆雜湊、雜湊與 HMAC（HMAC 起點之前的紀錄寫「啟用 HMAC 前」，
//   起點之後卻沒有 HMAC 的不這樣寫）。
// - 頁首「完整性」（screens.md S18 0.7）：列表第一頁的 meta.integrity（最近一次自我驗證或手動驗證、HMAC 起點），
//   例「✓ 已驗證至 10/15 02:20（雜湊鏈連續；HMAC 自第 11902 筆起）· 系統自動驗證」。只看自己操作的維運者沒有這一區。
// - 斷點為紀錄可能被竄改時提供「前往第 N 筆」（清除篩選、從第 N 筆開始列出）；金鑰問題不提供。

import { h, icon, mount } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { replaceQuery } from '../core/router.js';
import { pageHead, card, dl, emptyState, errorAlert, field, inlineAlert, statusBadge, setBusy, codeTag } from '../ui/components.js';
import { createTable } from '../ui/table.js';
import { openDialog } from '../ui/dialog.js';

const CATEGORIES = ['login', 'user', 'api_token', 'settings', 'enroll', 'pki', 'host', 'alerts', 'access', 'audit', 'system'];
const RESULTS = ['ok', 'failed', 'denied', 'rejected'];
const PERIODS = { '24h': 24 * 3600e3, '7d': 7 * 86400e3, '30d': 30 * 86400e3 };
const PAGE = 100;
// 目前的 HMAC 起點（meta.integrity.hmac_since_id；null＝尚未啟用）；undefined＝還不知道
let hmacSince;

export function actorText(actor) {
  if (!actor) return '—';
  if (actor === 'system') return t('adm.audit.actor.system');
  if (actor === 'anonymous') return t('adm.audit.actor.anonymous');
  if (actor.startsWith('user:')) return actor.slice(5);
  if (actor.startsWith('token:')) return t('adm.audit.actor.token', { id: actor.slice(6, 10) });
  return actor;
}

export function actionText(action) {
  const k = `adm.audit.action.${action}`;
  return has(k) ? t(k) : action;
}

function resultBadge(r) {
  const map = { ok: 'online', failed: 'critical', denied: 'warning', rejected: 'offline' };
  return statusBadge(map[r] || 'unknown', has(`adm.audit.result.${r}`) ? t(`adm.audit.result.${r}`) : r);
}

function detailsDialog(row) {
  const d = openDialog({
    title: t('adm.audit.detailTitle', { id: row.id }),
    size: 'lg',
    body: h(
      'div',
      null,
      dl([
        [t('adm.audit.col.time', { tz: fmt.tzLabel() }), `${fmt.dateTime(row.at)} ${fmt.tzLabel()}（${fmt.utc(row.at)}）`],
        [t('adm.audit.col.actor'), [actorText(row.actor), ' ', h('span', { class: 'k-host-id mono' }, row.actor)]],
        [t('adm.audit.col.ip'), row.actor_ip ? h('span', { class: 'mono' }, row.actor_ip) : '—'],
        [t('adm.audit.col.category'), catLabel(row.category)],
        [t('adm.audit.col.action'), [actionText(row.action), ' ', h('span', { class: 'k-host-id mono' }, row.action)]],
        [t('adm.audit.col.target'), row.object || '—'],
        [t('adm.audit.col.result'), resultBadge(row.result)],
        [t('adm.audit.code'), row.code ? codeTag(row.code) : '—'],
      ]),
      h('h3', { class: 'k-section-title' }, t('adm.audit.details')),
      row.details != null ? h('pre', { class: 'ds-code-block' }, JSON.stringify(row.details, null, 2)) : h('p', { class: 'muted' }, t('adm.audit.noDetails')),
      h('h3', { class: 'k-section-title' }, t('adm.audit.chain')),
      dl([
        [t('adm.audit.prevHash'), h('span', { class: 'mono k-break' }, row.prev_hash || '—')],
        [t('adm.audit.hash'), h('span', { class: 'mono k-break' }, row.hash || '—')],
        ['HMAC', row.hmac ? h('span', { class: 'mono k-break' }, row.hmac) : hmacSince != null && row.id >= hmacSince ? t('adm.audit.hmacMissing') : t('adm.audit.noHmac')],
      ]),
    ),
  });
  // 內容較長時對話框本文可捲動：讓鍵盤也能捲動（axe scrollable-region-focusable）
  const bodyEl = d.el.querySelector('.ds-dialog__body');
  if (bodyEl) bodyEl.setAttribute('tabindex', '0');
}

function catLabel(c) {
  return has(`adm.audit.cat.${c}`) ? t(`adm.audit.cat.${c}`) : c;
}

export function render(ctx) {
  const root = ctx.root;
  const q = ctx.query;
  const canAll = session.can('audit:read');
  const canExport = session.can('audit:export');
  const st = {
    actor: canAll ? q.get('actor') || '' : '',
    category: q.get('category') || '',
    action: q.get('action') || '',
    target: q.get('target') || '',
    result: q.get('result') || '',
    period: q.get('period') || '',
  };
  let cursor = null;
  let seq = 0;

  // ---- 完整性 ----
  const integrity = h('div', { 'aria-live': 'polite' }, h('p', null, t('adm.audit.notVerified')));
  const headIntegrity = h('span', { class: 'k-integrity-head' });
  let verifiedHere = false;
  let startCursor = null;
  // 重新錨定（meta.integrity.reanchor；kairos audit reanchor）：說明卡片，以及清單中那一筆與它之前的列的標示。
  // 主控台只顯示狀態，不提供執行的入口
  let reanchor = null;
  const reanchorBox = h('div');
  const verifyBtn = canAll ? h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => verify() } }, icon('shield-check'), t('adm.audit.verify')) : null;
  const exportLink = canExport ? h('a', { class: 'ds-button ds-button--secondary', href: '#', download: '', dataset: { native: '1' } }, icon('download'), t('adm.audit.exportJsonl')) : null;

  // ---- 篩選 ----
  const textInput = (key, label, placeholder) => {
    const el = h('input', { class: 'ds-input', type: 'search', name: key, value: st[key], autocomplete: 'off', spellcheck: 'false', placeholder });
    let timer = null;
    const apply = () => {
      st[key] = el.value.trim();
      load();
    };
    el.addEventListener('input', () => {
      clearTimeout(timer);
      timer = setTimeout(apply, 400);
    });
    el.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter') {
        ev.preventDefault();
        clearTimeout(timer);
        apply();
      }
    });
    const f = field({ label, input: el });
    f.classList.add('ds-filterbar__search');
    return f;
  };
  const sel = (key, label, options) => {
    const s = h(
      'select',
      { class: 'ds-select', name: key, on: { change: () => { st[key] = s.value; load(); } } },
      h('option', { value: '' }, t('adm.filter.all')),
      options.map(([v, text]) => h('option', { value: v, selected: st[key] === v }, text)),
    );
    return h('label', { class: 'k-labelled-select' }, label, s);
  };
  const filterRow = h(
    'div',
    { class: 'ds-filterbar__row' },
    canAll ? textInput('actor', t('adm.audit.col.actor'), t('adm.audit.actorPlaceholder')) : null,
    textInput('target', t('adm.audit.col.target'), t('adm.audit.targetPlaceholder')),
  );
  const selects = h(
    'div',
    { class: 'k-filter-selects' },
    sel('category', t('adm.audit.col.category'), CATEGORIES.map((c) => [c, catLabel(c)])),
    sel('result', t('adm.audit.col.result'), RESULTS.map((r) => [r, t(`adm.audit.result.${r}`)])),
    sel('period', t('adm.audit.period'), Object.keys(PERIODS).map((p) => [p, t(`adm.audit.period.${p}`)])),
  );
  const ownNote = h('div');

  const table = createTable({
    caption: t('adm.audit.caption'),
    unit: t('adm.audit.unit'),
    columns: [
      { key: 'at', label: t('adm.audit.col.time', { tz: fmt.tzLabel() }), render: (r) => h('span', { class: 'nowrap' }, fmt.dateTime(r.at)) },
      { key: 'actor', label: t('adm.audit.col.actor'), render: (r) => h('span', { title: r.actor }, actorText(r.actor)) },
      { key: 'ip', label: t('adm.audit.col.ip'), render: (r) => (r.actor_ip ? h('span', { class: 'mono' }, r.actor_ip) : '—') },
      {
        key: 'action',
        label: t('adm.audit.col.action'),
        render: (r) => [
          h('span', null, actionText(r.action)),
          h('span', { class: 'k-host-id mono' }, r.action),
          // 重新錨定的那一筆，與它之前（完整性未驗證）的列
          reanchor && r.id === reanchor.event_id ? [' ', statusBadge('warning', t('adm.audit.reanchor.rowEvent'))] : null,
          reanchor && r.id < reanchor.event_id ? [' ', h('span', { class: 'ds-status-badge ds-status-badge--neutral', title: t('adm.audit.reanchor.rowBeforeTitle') }, t('adm.audit.reanchor.rowBefore'))] : null,
        ],
      },
      { key: 'object', label: t('adm.audit.col.target'), render: (r) => r.object || '—' },
      { key: 'result', label: t('adm.audit.col.result'), render: (r) => [resultBadge(r.result), r.code ? [' ', codeTag(r.code)] : null] },
      {
        key: 'more',
        label: h('span', { class: 'ds-visually-hidden' }, t('adm.audit.details')),
        render: (r) => h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', 'aria-label': t('adm.audit.detailAria', { id: r.id }), on: { click: () => detailsDialog(r) } }, t('adm.audit.detailBtn')),
      },
    ],
    rowKey: (r) => String(r.id),
    onLoadMore: () => loadMore(),
  });

  mount(
    root,
    pageHead({ title: t('adm.audit.title'), meta: canAll ? headIntegrity : null, actions: [verifyBtn, exportLink].filter(Boolean) }),
    canAll ? card({ title: t('adm.audit.integrity'), body: [integrity, reanchorBox, h('p', { class: 'caption' }, t('adm.audit.limits'))] }) : null,
    ownNote,
    h('section', { class: 'ds-filterbar', 'aria-label': t('adm.audit.filters') }, filterRow, selects),
    table.el,
  );

  function params(c) {
    const from = st.period && PERIODS[st.period] ? new Date(fmt.serverNow() - PERIODS[st.period]).toISOString() : undefined;
    return { actor: st.actor, category: st.category, action: st.action, target: st.target, result: st.result, from, limit: PAGE, cursor: c || startCursor || undefined };
  }

  function updateExport() {
    if (!exportLink) return;
    const p = params();
    delete p.limit;
    delete p.cursor;
    exportLink.setAttribute('href', `/api/v1/audit/export${qs(p)}`);
  }

  async function load() {
    const my = ++seq;
    replaceQuery({ actor: st.actor, category: st.category, action: st.action, target: st.target, result: st.result, period: st.period });
    updateExport();
    table.setLoading();
    try {
      const res = await api(`/api/v1/audit${qs(params())}`, { raw: true });
      if (my !== seq) return;
      cursor = res.page && res.page.next_cursor;
      mount(ownNote, res.meta && res.meta.own_only ? inlineAlert({ kind: 'info', title: t('adm.audit.ownOnly') }) : null);
      if (res.meta && res.meta.integrity) {
        reanchor = res.meta.integrity.reanchor || null;
        mount(reanchorBox, reanchor ? reanchorNotice(reanchor, { onJump: jumpTo }) : null);
      }
      if (res.meta && res.meta.integrity && !verifiedHere) {
        hmacSince = res.meta.integrity.hmac_since_id;
        mount(headIntegrity, ...integrityHead(res.meta.integrity));
      }
      const rows = res.data || [];
      if (!rows.length) {
        const filtered = Boolean(st.actor || st.category || st.action || st.target || st.result || st.period);
        table.setEmpty(emptyState({ title: filtered ? t('adm.audit.emptyFiltered') : t('adm.audit.empty') }));
        return;
      }
      table.setRows(rows, { total: res.page.total, hasMore: Boolean(cursor) });
    } catch (err) {
      if (my !== seq) return;
      table.setError(err, load, t('adm.audit.loadFailed'));
    }
  }

  async function loadMore() {
    if (!cursor) return;
    const my = seq;
    table.setLoadingMore();
    try {
      const res = await api(`/api/v1/audit${qs(params(cursor))}`, { raw: true });
      if (my !== seq) return;
      cursor = res.page && res.page.next_cursor;
      table.appendRows(res.data || [], { total: res.page.total, hasMore: Boolean(cursor) });
    } catch (err) {
      table.setError(err, loadMore, t('adm.audit.loadFailed'));
    }
  }

  /** 前往第 N 筆：清除篩選，從第 N 筆開始列出（cursor＝N＋1：列出編號小於它的） */
  function jumpTo(id) {
    for (const k of Object.keys(st)) st[k] = '';
    for (const el of root.querySelectorAll('.ds-filterbar input, .ds-filterbar select')) el.value = '';
    startCursor = String(id + 1);
    load().then(() => {
      const row = root.querySelector(`tbody tr[data-key="${id}"]`);
      if (row) {
        row.classList.add('is-highlight');
        const btn = row.querySelector('button');
        if (btn) btn.focus();
      }
    });
  }

  async function verify() {
    setBusy(verifyBtn, true, t('adm.audit.verifying'));
    try {
      const r = await api('/api/v1/audit/verify', { method: 'POST' });
      verifiedHere = true;
      if (r.hmac) hmacSince = r.hmac.since_id;
      mount(integrity, ...verifyResult(r, { onJump: jumpTo }));
      // 頁首以這次的結果更新（與重新整理後 meta.integrity 的寫法相同）
      mount(headIntegrity, ...integrityHead({ hmac_since_id: r.hmac ? r.hmac.since_id : null, last_verify: { at: fmt.serverNow(), actor: session.current() && session.current().user ? `user:${session.current().user.username}` : null, result: r.ok ? 'ok' : 'failed', self_check: false, broken: r.broken } }));
    } catch (err) {
      mount(integrity, errorAlert(err, { title: t('adm.audit.verifyFailed') }));
    } finally {
      setBusy(verifyBtn, false, t('adm.audit.verifying'));
    }
  }

  load();
  return {};
}

/** 頁首「完整性」（screens.md S18 0.7：「✓ 已驗證至 02:20（雜湊鏈連續；HMAC 自第 11902 筆起）」） */
export function integrityHead(ig) {
  const v = ig && ig.last_verify;
  if (!v) return [t('adm.audit.head.label'), ' ', statusBadge('unknown', t('adm.audit.head.never'))];
  const at = `${fmt.dateTime(v.at, { seconds: false })} ${fmt.tzLabel()}`;
  const by = v.self_check || v.actor === 'system' ? t('adm.audit.head.bySystem') : t('adm.audit.head.byUser', { who: actorText(v.actor) });
  if (v.result === 'ok') {
    const hm = ig.hmac_since_id != null ? t('adm.audit.head.hmacSince', { n: ig.hmac_since_id }) : t('adm.audit.head.hmacNone');
    // 重新錨定過：頁首也寫出來（驗證通過的是重新錨定之後的部分）
    const ra = ig.reanchor ? [' · ', t('adm.audit.reanchor.head', { at: fmt.dateTime(ig.reanchor.at, { seconds: false }), id: ig.reanchor.event_id })] : [];
    return [t('adm.audit.head.label'), ' ', statusBadge('online', t('adm.audit.head.okBadge')), ' ', t('adm.audit.head.ok', { at, hmac: hm }), ...ra, ' · ', by];
  }
  const b = v.broken || {};
  const reason = BREAK_REASONS.includes(b.reason) ? t(`adm.audit.break.${b.reason}`) : b.reason || '—';
  return [t('adm.audit.head.label'), ' ', statusBadge('critical', t('adm.audit.head.failedBadge')), ' ', t('adm.audit.head.failed', { at, id: b.id ?? '—', r: reason }), ' ', codeTag('KRS-E6201'), ' · ', by];
}

const BREAK_REASONS = ['hash_mismatch', 'prev_mismatch', 'missing_hash', 'anchor_mismatch', 'head_mismatch', 'missing_hmac', 'hmac_mismatch', 'hmac_boundary_mismatch', 'hmac_key_unavailable', 'hmac_key_mismatch', 'reanchor_mismatch', 'reanchored'];

/**
 * 重新錨定的說明（meta.integrity.reanchor）：時間、執行者、原因；之前的紀錄完整性未驗證；
 * 告警保持開啟的期間內寫明原因是重新錨定。只是狀態，沒有任何操作（重新錨定只能在 Kairos 主機上以 CLI 執行）
 */
export function reanchorNotice(ra, { onJump } = {}) {
  const until = ra.alert_until && Date.parse(ra.alert_until) > fmt.serverNow() ? `${fmt.dateTime(ra.alert_until, { seconds: false })} ${fmt.tzLabel()}` : null;
  return inlineAlert({
    kind: 'warning',
    title: t('adm.audit.reanchor.title'),
    body: h(
      'div',
      { class: 'k-stack--sm', id: 'audit-reanchor' },
      h('p', null, t('adm.audit.reanchor.when', { at: `${fmt.dateTime(ra.at, { seconds: false })} ${fmt.tzLabel()}`, by: ra.by || '—', id: ra.event_id })),
      h('p', null, ra.reason ? t('adm.audit.reanchor.reason', { r: ra.reason }) : t('adm.audit.reanchor.noReason')),
      h('p', null, ra.unverified != null ? t('adm.audit.reanchor.unverified', { id: ra.event_id, n: fmt.num(ra.unverified) }) : t('adm.audit.reanchor.unverifiedUnknown', { id: ra.event_id })),
      until ? h('p', null, t('adm.audit.reanchor.alertHeld', { until })) : null,
      onJump ? h('p', null, h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => onJump(ra.event_id) } }, t('adm.audit.reanchor.jump'))) : null,
    ),
  });
}

/** 驗證結果：雜湊鏈與 HMAC 的邊界 */
export function verifyResult(r, { onJump } = {}) {
  const at = `${fmt.dateTime(fmt.serverNow())} ${fmt.tzLabel()}`;
  const hm = r.hmac || {};
  const hmacLine =
    hm.status === 'verified'
      ? t('adm.audit.hmac.verified', { n: fmt.num(hm.verified), since: hm.since_id ?? '—', chain: fmt.num(hm.chain_only || 0) })
      : hm.status === 'not_started'
        ? t('adm.audit.hmac.notStarted')
        : t(`adm.audit.hmac.${['key_unavailable', 'key_mismatch', 'failed'].includes(hm.status) ? hm.status : 'failed'}`);
  if (r.ok) {
    return [
      inlineAlert({
        kind: 'success',
        title: t('adm.audit.verifiedOk', { at }),
        body: h(
          'ul',
          null,
          h('li', null, t('adm.audit.checkedOk', { n: fmt.num(r.checked), first: r.first_id ?? '—', last: r.last_id ?? '—' })),
          h('li', null, hmacLine),
          r.reanchor ? h('li', null, t('adm.audit.reanchor.verified', { id: r.reanchor.event_id, n: fmt.num(r.reanchor.unverified) })) : null,
          r.last_hash ? h('li', null, t('adm.audit.lastHash'), ' ', h('span', { class: 'mono k-break' }, r.last_hash)) : null,
        ),
      }),
    ];
  }
  const b = r.broken || {};
  const reason = BREAK_REASONS.includes(b.reason) ? t(`adm.audit.break.${b.reason}`) : b.reason;
  const keyIssue = b.reason === 'hmac_key_unavailable' || b.reason === 'hmac_key_mismatch';
  return [
    h(
      'div',
      { class: 'ds-banner ds-banner--critical', role: 'alert' },
      icon('status-critical'),
      h(
        'div',
        { class: 'ds-banner__body' },
        // 同為 KRS-E6201 的兩種寫法（screens.md S18、copywriting.md 範例 95）：紀錄可能被竄改／金鑰問題
        keyIssue
          ? h('p', { class: 'strong' }, t('adm.audit.brokenKey'), ' ', codeTag(b.product_code || 'KRS-E6201'))
          : h('p', { class: 'strong' }, t('adm.audit.broken', { id: b.id ?? '—' }), ' ', codeTag(b.product_code || 'KRS-E6201')),
        h('p', null, keyIssue ? t('adm.audit.brokenKeyReason', { what: t(`adm.audit.brokenKeyWhat.${b.reason}`) }) : t('adm.audit.brokenReason', { r: reason })),
        h('p', null, keyIssue ? t('adm.audit.brokenKeyFix') : t('adm.audit.brokenFix')),
        // 斷裂之後的出路：從備份還原，或在主機上重新錨定（主控台不提供）
        keyIssue ? null : h('p', null, t('adm.audit.reanchor.cli')),
        h('p', null, t('adm.audit.checked', { n: fmt.num(r.checked), first: r.first_id ?? '—', last: r.last_id ?? '—' })),
        h('p', null, hmacLine),
      ),
      // 紀錄可能被竄改：「前往第 N 筆」；金鑰問題沒有特定的紀錄可看（screens.md S18）
      !keyIssue && b.id != null && onJump ? h('div', { class: 'ds-banner__actions' }, h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => onJump(b.id) } }, t('adm.audit.jumpTo', { id: b.id }))) : null,
    ),
  ];
}
