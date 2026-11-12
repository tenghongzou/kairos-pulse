// S26 升級推送：清單（/pulse/rollouts）（設計包 kairos/screens.md S26；API.md 第 17.3 節）。
//
// - 篩選：狀態（進行中、已暫停、已完成、已取消、草稿；?state=，可分享）。
// - 欄位：名稱、目標、狀態、進度（成功／失敗／進行中／未開始；長條只是輔助，數字是可見文字）、批次、下一步
//   （next_action 的代碼＋參數由 pulse-common.js 組文案；已暫停寫出原因）、建立。
// - 即時更新：SSE rollout.progress 就地更新該列（狀態、批次、進度、下一步），不重排（screens.md §0.2）。
// - 空狀態：「還沒有升級推送…」；版本庫為空時主要動作改為「前往 Pulse 版本庫」。
// - 權限：建立只限管理員（rollouts:write）；維運者、稽核員檢視。0.9 不顯示 0.10 的升級時段、雙人覆核。

import { h, icon, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import * as fmt from '../core/format.js';
import { replaceQuery } from '../core/router.js';
import { pageHead, card, emptyState, errorState, skeletonBlock } from '../ui/components.js';
import { isUnavailable, unavailableState, rolloutStateBadge, rolloutStateLabel, progressCell, nextActionText, warnText, ROLLOUT_STATES } from './pulse-common.js';

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
  let state = ROLLOUT_STATES.includes(ctx.query.get('state')) ? ctx.query.get('state') : '';
  let disposed = false;
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
  mount(root, pageHead({ title: t('rm.ro.title'), actions: newBtn }), h('p', { class: 'caption' }, t('rm.ro.mvp')), filters, body);

  function row(r) {
    return h(
      'tr',
      { dataset: { rollout: r.id } },
      h('td', null, h('a', { href: `/pulse/rollouts/${encodeURIComponent(r.id)}` }, r.name), h('span', { class: 'k-host-id mono' }, r.id)),
      h('td', null, r.target_version),
      h('td', null, rolloutStateBadge(r.state)),
      h('td', null, r.state === 'draft' ? '—' : progressCell(r.progress)),
      h('td', null, waveText(r)),
      h('td', null, nextCell(r)),
      h('td', null, fmt.dateTime(r.created_at, { seconds: false }), r.created_by ? h('span', { class: 'k-host-id' }, r.created_by) : null),
    );
  }

  async function load() {
    let list;
    let releases = null;
    try {
      list = await api(`/api/v1/rollouts${qs({ state })}`);
      if (!list.length && !state && session.can('releases:read')) releases = await api('/api/v1/releases').catch(() => null);
    } catch (err) {
      if (disposed) return;
      if (isUnavailable(err)) {
        if (newBtn) newBtn.hidden = true;
        filters.hidden = true;
        mount(body, unavailableState(err, 'upgrade'));
        return;
      }
      mount(body, errorState(err, { title: t('rm.ro.loadFailed'), onRetry: load }));
      return;
    }
    if (disposed) return;
    rowEls.clear();
    rowsData.clear();
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
