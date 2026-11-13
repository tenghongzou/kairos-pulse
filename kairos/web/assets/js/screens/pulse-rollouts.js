// S26 升級推送：清單（/pulse/rollouts）（設計包 kairos/screens.md S26；API.md 第 17.3 節）。
//
// - 篩選：狀態（進行中、已暫停、已完成、已取消、草稿；?state=，可分享）。
// - 欄位：名稱、目標、狀態、進度（成功／失敗／進行中／未開始；長條只是輔助，數字是可見文字）、批次、下一步
//   （next_action 的代碼＋參數由 pulse-common.js 組文案；已暫停寫出原因）、建立。
// - 即時更新：SSE rollout.progress 就地更新該列（狀態、批次、進度、下一步），不重排（screens.md §0.2）。
// - 空狀態：「還沒有升級推送…」；版本庫為空時主要動作改為「前往 Pulse 版本庫」。
// - 權限：建立只限管理員（rollouts:write）；維運者、稽核員檢視。
// - 檢視（0.10，ADR-0002 決策點 2a）：「全部」與「待核准」（?view=pending_approval，可分享）。待核准的欄位：名稱、
//   目標版本、可升級台數、送出者與時間、已等待多久、風險標示、「我可以核准」——台數、風險與能否核准都由伺服器
//   計算（每一筆的 submission 與 viewer；清單的 meta.pending_approval／pending_approval_actionable）。
// - 雙人覆核在 30 天內被關閉（或啟用）時頁首說明（meta.rollout_approval_change；啟動時與資料庫的前值比對）。

import { h, icon, mount } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import * as fmt from '../core/format.js';
import { replaceQuery } from '../core/router.js';
import { pageHead, card, emptyState, errorState, inlineAlert, skeletonBlock, statusBadge, neutralBadge } from '../ui/components.js';
import { isUnavailable, unavailableState, rolloutStateBadge, rolloutStateLabel, progressCell, nextActionText, warnText, waitedText, sourceLabel, unapprovedBadge, ROLLOUT_STATES } from './pulse-common.js';

const VIEW_PENDING = 'pending_approval';
// 「待核准」檢視的風險標示（submission.warnings 的固定代碼中屬於內容風險的；完整的提醒在詳情頁的送審摘要）
const RISK_CODES = ['downgrade', 'no_rollback', 'single_wave', 'artifact_problem'];

function waveText(r) {
  if (r.state === 'completed') return t('rm.ro.waveDone');
  if (r.state === 'draft' || r.state === 'cancelled' || r.state === 'scheduled' || r.state === 'pending_approval') return '—';
  return t('rm.ro.waveOf', { n: fmt.num((r.current_wave || 0) + 1), total: fmt.num(r.waves_total || 0) });
}

function nextCell(r) {
  const text = nextActionText(r.next_action);
  return r.state === 'paused' ? warnText(text) : text;
}

export function render(ctx) {
  const root = ctx.root;
  const canWrite = session.can('rollouts:write');
  let view = ctx.query.get('view') === VIEW_PENDING ? VIEW_PENDING : '';
  let state = !view && ROLLOUT_STATES.includes(ctx.query.get('state')) ? ctx.query.get('state') : '';
  let disposed = false;
  let seq = 0;
  // 目前的雙人覆核設定（meta.rollout_approval）：決定「未經核准」的標記要不要以警告呈現
  let approvalNow = '';
  const rowEls = new Map();
  const rowsData = new Map();
  const body = h('div', null, skeletonBlock(6));
  const newBtn = canWrite ? h('a', { class: 'ds-button ds-button--primary', href: '/pulse/rollouts/new' }, icon('plus'), t('rm.ro.new')) : null;
  const sel = h(
    'select',
    {
      class: 'ds-select',
      name: 'state',
      on: {
        change: () => {
          state = sel.value;
          replaceQuery({ state });
          load();
        },
      },
    },
    h('option', { value: '' }, t('rm.ro.filter.all')),
    ROLLOUT_STATES.map((s) => h('option', { value: s, selected: s === state }, rolloutStateLabel(s))),
  );
  const filters = h('section', { class: 'ds-filterbar', 'aria-label': t('hosts.filters') }, h('div', { class: 'k-filter-selects' }, h('label', { class: 'k-labelled-select' }, t('rm.ro.filter.state'), sel)));
  filters.hidden = view === VIEW_PENDING;
  const notes = h('div', { class: 'k-stack--sm' });
  const tabs = h('nav', { class: 'ds-tabs', 'aria-label': t('rm.ro.views') });
  mount(root, pageHead({ title: t('rm.ro.title'), actions: newBtn }), h('p', { class: 'caption' }, t('rm.ro.mvp')), notes, tabs, filters, body);

  function setView(v) {
    if (v === view) return;
    view = v;
    state = '';
    sel.value = '';
    filters.hidden = view === VIEW_PENDING;
    replaceQuery(view ? { view } : {});
    mount(body, skeletonBlock(6));
    load();
  }

  /** 檢視的切換（待核准附筆數；有可以由目前使用者核准的另外寫出） */
  function renderTabs(meta) {
    const total = (meta && meta.pending_approval) || 0;
    const mine = (meta && meta.pending_approval_actionable) || 0;
    const tab = (v, href, text) =>
      h(
        'a',
        {
          href,
          'aria-current': view === v ? 'page' : null,
          dataset: { view: v || 'all' },
          on: {
            click: (ev) => {
              if (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.button) return;
              ev.preventDefault();
              ev.stopPropagation();
              setView(v);
            },
          },
        },
        text,
      );
    mount(
      tabs,
      tab('', '/pulse/rollouts', t('rm.ro.view.all')),
      tab(VIEW_PENDING, `/pulse/rollouts?view=${VIEW_PENDING}`, mine ? t('rm.ro.view.pendingMine', { n: fmt.num(total), m: fmt.num(mine) }) : t('rm.ro.view.pending', { n: fmt.num(total) })),
    );
  }
  renderTabs(null);

  /** 雙人覆核最近 30 天內的改變（關閉是警告；啟用是資訊） */
  function renderNotes(meta) {
    const ch = meta && meta.rollout_approval_change;
    if (!ch || !ch.at) {
      mount(notes);
      return;
    }
    const args = { t: fmt.dateTime(ch.at, { seconds: false }), src: sourceLabel(ch.source) };
    mount(
      notes,
      ch.to === 'none'
        ? h('div', { id: 'rollout-approval-change', dataset: { to: 'none' } }, inlineAlert({ kind: 'warning', title: t('rm.ro.approvalOff', args), body: t('rm.ro.approvalOffBody') }))
        : h('div', { id: 'rollout-approval-change', dataset: { to: String(ch.to) } }, inlineAlert({ kind: 'info', body: t('rm.ro.approvalOn', args) })),
    );
  }

  function mineCell(r) {
    const v = r.viewer || {};
    const sub = r.submission || {};
    const out = [];
    if (v.can_approve) out.push(statusBadge('warning', t('rm.ro.mineYes')));
    else out.push(h('span', null, t(v.blocked === 'self_approval' ? 'rm.ro.mineSelf' : v.blocked === 'session_required' ? 'rm.ro.mineToken' : 'rm.ro.mineNoPerm')));
    if (sub.approvers_available === 0) out.push(h('span', { class: 'k-sub', dataset: { noApprovers: '1' } }, warnText(t('rm.ro.noApprovers'))));
    return out;
  }

  function riskCell(r) {
    const codes = ((r.submission && r.submission.warnings) || []).filter((c) => RISK_CODES.includes(c));
    if (!codes.length) return t('rm.ro.riskNone');
    return h('ul', { class: 'k-plain-list k-ro-risks' }, codes.map((c) => h('li', { dataset: { warning: c } }, neutralBadge(has(`rm.risk.${c}`) ? t(`rm.risk.${c}`) : c, 'status-warning'))));
  }

  function pendingRow(r) {
    const sub = r.submission || {};
    return h(
      'tr',
      { dataset: { rollout: r.id } },
      h('td', null, h('a', { href: `/pulse/rollouts/${encodeURIComponent(r.id)}` }, r.name), h('span', { class: 'k-host-id mono' }, r.id)),
      h('td', null, r.target_version),
      h('td', { class: 'num' }, t('rm.ro.hostsN', { n: fmt.num(sub.eligible ?? r.hosts_total ?? 0) })),
      h('td', null, r.submitted_by || '—', r.submitted_at ? h('span', { class: 'k-host-id' }, fmt.dateTime(r.submitted_at, { seconds: false })) : null),
      h('td', null, r.submitted_at ? waitedText(r.submitted_at) : '—'),
      h('td', null, riskCell(r)),
      h('td', null, mineCell(r)),
    );
  }

  function pendingTable(list) {
    return card({
      body: h(
        'div',
        { class: 'k-scroll-x' },
        h(
          'table',
          { class: 'k-mini-table', id: 'rollouts-pending' },
          h('caption', { class: 'ds-visually-hidden' }, t('rm.ro.pendingCaption')),
          h(
            'thead',
            null,
            h(
              'tr',
              null,
              h('th', { scope: 'col' }, t('rm.ro.col.name')),
              h('th', { scope: 'col' }, t('rm.ro.col.target')),
              h('th', { scope: 'col', class: 'num' }, t('rm.ro.pcol.eligible')),
              h('th', { scope: 'col' }, t('rm.ro.pcol.submitted')),
              h('th', { scope: 'col' }, t('rm.ro.pcol.waiting')),
              h('th', { scope: 'col' }, t('rm.ro.pcol.risk')),
              h('th', { scope: 'col' }, t('rm.ro.pcol.mine')),
            ),
          ),
          h('tbody', null, list.map(pendingRow)),
        ),
      ),
    });
  }

  function row(r) {
    return h(
      'tr',
      { dataset: { rollout: r.id } },
      h('td', null, h('a', { href: `/pulse/rollouts/${encodeURIComponent(r.id)}` }, r.name), h('span', { class: 'k-host-id mono' }, r.id)),
      h('td', null, r.target_version),
      h('td', null, rolloutStateBadge(r.state), r.approval === 'not_required' ? [' ', unapprovedBadge(approvalNow === 'required')] : null),
      h('td', null, r.state === 'draft' ? '—' : progressCell(r.progress)),
      h('td', null, waveText(r)),
      h('td', null, nextCell(r)),
      h('td', null, fmt.dateTime(r.created_at, { seconds: false }), r.created_by ? h('span', { class: 'k-host-id' }, r.created_by) : null),
    );
  }

  async function load() {
    const my = ++seq;
    let list;
    let meta;
    let releases = null;
    try {
      const res = await api(`/api/v1/rollouts${qs({ state: view === VIEW_PENDING ? VIEW_PENDING : state })}`, { raw: true });
      list = res.data || [];
      meta = res.meta || {};
      if (!list.length && !state && !view && session.can('releases:read')) releases = await api('/api/v1/releases').catch(() => null);
    } catch (err) {
      if (disposed || my !== seq) return;
      if (isUnavailable(err)) {
        if (newBtn) newBtn.hidden = true;
        filters.hidden = true;
        tabs.hidden = true;
        mount(notes);
        mount(body, unavailableState(err, 'upgrade'));
        return;
      }
      mount(body, errorState(err, { title: t('rm.ro.loadFailed'), onRetry: load }));
      return;
    }
    if (disposed || my !== seq) return;
    rowEls.clear();
    rowsData.clear();
    approvalNow = meta.rollout_approval || '';
    renderTabs(meta);
    renderNotes(meta);
    if (view === VIEW_PENDING) {
      mount(body, list.length ? pendingTable(list) : emptyState({ title: t('rm.ro.pendingEmpty'), body: t('rm.ro.pendingEmptyBody'), iconName: 'success' }));
      return;
    }
    if (!list.length) {
      if (state) {
        mount(
          body,
          emptyState({
            title: t('rm.ro.emptyFiltered'),
            iconName: 'filter',
            actions: [
              h(
                'button',
                {
                  type: 'button',
                  class: 'ds-button ds-button--secondary',
                  on: {
                    click: () => {
                      state = '';
                      sel.value = '';
                      replaceQuery({});
                      load();
                    },
                  },
                },
                t('rm.ro.clear'),
              ),
            ],
          }),
        );
        return;
      }
      const noReleases = Array.isArray(releases) && releases.length === 0;
      mount(
        body,
        emptyState({
          title: t('rm.ro.empty'),
          body: t('rm.ro.emptyBody'),
          iconName: 'upload',
          actions: noReleases
            ? [h('a', { class: 'ds-button ds-button--primary', href: '/pulse/releases' }, t('rm.ro.goReleases'))]
            : canWrite
              ? [h('a', { class: 'ds-button ds-button--primary', href: '/pulse/rollouts/new' }, icon('plus'), t('rm.ro.new'))]
              : null,
        }),
      );
      return;
    }
    const tbody = h(
      'tbody',
      null,
      list.map((r) => {
        const tr = row(r);
        rowEls.set(r.id, tr);
        rowsData.set(r.id, r);
        return tr;
      }),
    );
    mount(
      body,
      card({
        body: h(
          'div',
          { class: 'k-scroll-x' },
          h(
            'table',
            { class: 'k-mini-table', id: 'rollouts' },
            h('caption', { class: 'ds-visually-hidden' }, t('rm.ro.caption')),
            h(
              'thead',
              null,
              h(
                'tr',
                null,
                h('th', { scope: 'col' }, t('rm.ro.col.name')),
                h('th', { scope: 'col' }, t('rm.ro.col.target')),
                h('th', { scope: 'col' }, t('rm.ro.col.state')),
                h('th', { scope: 'col' }, t('rm.ro.col.progress')),
                h('th', { scope: 'col' }, t('rm.ro.col.wave')),
                h('th', { scope: 'col' }, t('rm.ro.col.next')),
                h('th', { scope: 'col' }, t('rm.ro.col.created', { tz: fmt.tzLabel() })),
              ),
            ),
            tbody,
          ),
        ),
      }),
    );
  }

  // 即時更新：只更新該列（不重排）；清單沒有的推送不加入（避免跳動，重新整理後出現）
  const unsub = sse.subscribe((ev) => {
    if (ev.type === 'resync') {
      load().finally(() => sse.resyncSettled());
      return;
    }
    if (ev.type !== 'rollout.progress' || !ev.data) return;
    const d = ev.data;
    // 「待核准」檢視：推送開始（核准之後）就不再屬於這個檢視，重新載入
    if (view === VIEW_PENDING) {
      load();
      return;
    }
    const old = rowsData.get(d.rollout_id);
    const tr = rowEls.get(d.rollout_id);
    if (!old || !tr) return;
    const r = { ...old, state: d.state ?? old.state, current_wave: d.current_wave ?? old.current_wave, progress: d.progress ?? old.progress, next_action: d.next_action ?? old.next_action };
    const fresh = row(r);
    tr.replaceWith(fresh);
    rowEls.set(r.id, fresh);
    rowsData.set(r.id, r);
  });

  load();
  return {
    dispose() {
      disposed = true;
      unsub();
    },
  };
}
