// S26 推送詳情（/pulse/rollouts/:rolloutId）（設計包 kairos/screens.md S26「詳情」；API.md 第 17.3 節）。
//
// - 頁首：名稱、目標、建立者；動作依狀態與權限——暫停…（rollouts:pause，管理員與維運者；L1、原因必填）、
//   繼續…（rollouts:write；暫停中為 resume，觀察期滿等待繼續時為 continue；L1）、取消推送…（rollouts:write；L1、原因必填）。
//   草稿另有「送出推送…」（L2／L3，與建立推送相同的對話框）。
// - 已暫停：InlineAlert 寫出原因（pause_reason）與「查看失敗的主機」「繼續…」；自動暫停（pause_cause 不是 manual）
//   標題為「已自動暫停：…」並附 KRS-W8414，主控台暫停不附代碼。觀察中與等待繼續的說明。
// - 回滾失敗的主機：嚴重的 InlineAlert 列在最上方（連到主機詳情，在「升級」卡片標記為已處理）。
// - 批次：先行批次與其餘的台數、各狀態的台數、失敗比例與上限、觀察時間、目前的批次。
// - 主機表：GET /rollouts/{id}/hosts（伺服器端分頁「載入更多」；批次、狀態、搜尋篩選；「失敗的主機」一次選取 4 種失敗狀態）；
//   欄位：主機、狀態（略過的原因）、版本、進度、代碼、時間；「重試」（rollouts:write）只對可以重試的主機。
// - 即時更新：SSE rollout.progress（這個推送）2 秒內合併後重新載入頁首、批次與主機表的第一頁（rollout.host 在 0.9 尚未提供）。

import { h, icon, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import * as fmt from '../core/format.js';
import { pageHead, card, emptyState, errorState, errorAlert, inlineAlert, field, skeletonBlock } from '../ui/components.js';
import { createTable } from '../ui/table.js';
import { toast } from '../ui/toast.js';
import { confirmL1 } from './admin-common.js';
import { refreshRolloutCount } from './pulse-boot.js';
import { submitDialog, previewView } from './pulse-rollout-new.js';
import {
  isUnavailable,
  unavailableState,
  rolloutStateBadge,
  hostStateBadge,
  hostStateLabel,
  skipLabel,
  progressGroups,
  hostLink,
  HOST_STATES,
} from './pulse-common.js';

const CRUMB = { text: t('rm.ro.title'), href: '/pulse/rollouts' };
const FAILED = ['failed', 'rolled_back', 'rollback_failed', 'unknown'];
const RETRYABLE = ['failed', 'rolled_back', 'expired', 'skipped_offline', 'declined', 'unknown'];
const PAGE = 100;

function countsLine(progress) {
  const items = HOST_STATES.filter((s) => progress && progress[s]).map((s) => t('rm.det.countItem', { label: hostStateLabel(s), n: fmt.num(progress[s]) }));
  return items.join(' · ') || '—';
}

export function render(ctx) {
  const root = ctx.root;
  const id = String(ctx.params.rolloutId || '');
  let disposed = false;
  let r = null;
  let timer = null;
  const filter = { wave: '', state: '', q: '' };
  let nextCursor = null;
  let total = 0;

  const head = h('div');
  const alerts = h('div', { class: 'k-stack--sm' });
  const wavesBox = h('div');
  const hostsBox = h('div');
  mount(root, pageHead({ title: t('common.loading'), crumbs: [CRUMB, { text: id }] }), skeletonBlock(8));

  // ---- 主機表 ----
  const canRetry = () => session.can('rollouts:write') && r && (r.state === 'running' || r.state === 'paused');
  const retryable = (x) => RETRYABLE.includes(x.state) || (x.state === 'rollback_failed' && x.acknowledged_at);
  const table = createTable({
    caption: t('rm.det.hostsCaption'),
    unit: t('rm.det.unit'),
    rowKey: (x) => x.agent_id,
    columns: [
      { key: 'host', label: t('rm.det.col.host'), cls: 'k-cell-host', render: (x) => [hostLink(x.agent_id, x.hostname), h('span', { class: 'k-host-id mono' }, x.agent_id.slice(0, 8))] },
      { key: 'state', label: t('rm.det.col.state'), render: (x) => [hostStateBadge(x.state), x.reason ? h('span', { class: 'k-sub' }, skipLabel(x.reason)) : null] },
      { key: 'version', label: t('rm.det.col.version'), render: (x) => (x.to_version && x.from_version !== x.to_version ? t('rm.versions', { from: x.from_version || '—', to: x.to_version }) : x.from_version || '—') },
      { key: 'progress', label: t('rm.det.col.progress'), num: true, render: (x) => (x.progress && x.progress.total ? fmt.pct((x.progress.bytes / x.progress.total) * 100) : '—') },
      { key: 'code', label: t('rm.det.col.code'), render: (x) => (x.code ? h('code', { class: 'k-code-tag' }, x.code) : '—') },
      { key: 'time', label: t('rm.det.col.time'), render: (x) => fmt.timeEl(x.updated_at) },
      {
        key: 'actions',
        label: t('rm.det.col.actions'),
        render: (x) =>
          canRetry() && retryable(x)
            ? h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', 'aria-label': t('rm.det.retryAria', { h: x.hostname || x.agent_id }), on: { click: () => retry(x) } }, t('rm.det.retry'))
            : '',
      },
    ],
    onLoadMore: () => loadMore(),
  });

  const waveSel = h('select', { class: 'ds-select', name: 'wave' });
  const stateSel = h(
    'select',
    { class: 'ds-select', name: 'state' },
    h('option', { value: '' }, t('rm.ro.filter.all')),
    h('option', { value: FAILED.join(',') }, t('rm.det.filter.failed')),
    HOST_STATES.map((s) => h('option', { value: s }, hostStateLabel(s))),
  );
  const qIn = h('input', { class: 'ds-input', type: 'search', name: 'q', autocomplete: 'off', spellcheck: 'false' });
  const fq = field({ label: t('rm.det.filter.q'), input: qIn, hint: t('rm.det.filter.qHint') });
  fq.classList.add('ds-filterbar__search');
  let qTimer = null;
  waveSel.addEventListener('change', () => {
    filter.wave = waveSel.value;
    loadHosts();
  });
  stateSel.addEventListener('change', () => {
    filter.state = stateSel.value;
    loadHosts();
  });
  qIn.addEventListener('input', () => {
    clearTimeout(qTimer);
    qTimer = setTimeout(() => {
      filter.q = qIn.value.trim();
      loadHosts();
    }, 300);
  });

  function hostParams(cursor) {
    return { wave: filter.wave, state: filter.state, q: filter.q, limit: PAGE, cursor };
  }

  async function loadHosts() {
    table.setLoading();
    nextCursor = null;
    try {
      const res = await api(`/api/v1/rollouts/${encodeURIComponent(id)}/hosts${qs(hostParams())}`, { raw: true });
      if (disposed) return;
      total = res.page.total;
      nextCursor = res.page.next_cursor;
      if (!res.data.length) table.setEmpty(emptyState({ title: t('rm.det.hostsEmpty'), iconName: 'filter' }));
      else table.setRows(res.data, { total, hasMore: Boolean(nextCursor) });
    } catch (err) {
      if (!disposed) table.setError(err, loadHosts, t('rm.det.hostsFailed'));
    }
  }
  async function loadMore() {
    if (!nextCursor) return;
    table.setLoadingMore();
    try {
      const res = await api(`/api/v1/rollouts/${encodeURIComponent(id)}/hosts${qs(hostParams(nextCursor))}`, { raw: true });
      if (disposed) return;
      nextCursor = res.page.next_cursor;
      total = res.page.total;
      table.appendRows(res.data, { total, hasMore: Boolean(nextCursor) });
    } catch (err) {
      if (!disposed) table.setError(err, loadMore, t('rm.det.hostsFailed'));
    }
  }

  async function retry(x) {
    try {
      await api(`/api/v1/rollouts/${encodeURIComponent(id)}/hosts/${encodeURIComponent(x.agent_id)}/retry`, { method: 'POST', body: {} });
      toast({ kind: 'success', title: t('rm.det.retried', { h: x.hostname || x.agent_id }) });
      loadHosts();
    } catch (err) {
      toast({ kind: 'error', title: t('rm.det.retryFailed'), desc: err.message || err.code });
    }
  }

  // ---- 動作 ----
  function action(kind) {
    const name = r.name;
    const conf = {
      pause: { title: t('rm.det.pauseTitle', { name }), body: t('rm.det.pauseBody'), confirmText: t('rm.det.pauseConfirm'), reason: 'required', danger: true, path: 'pause', done: t('rm.det.paused') },
      resume: { title: t('rm.det.resumeTitle', { name }), body: t('rm.det.resumeBody'), confirmText: t('rm.det.resumeConfirm'), reason: null, path: 'resume', done: t('rm.det.resumed') },
      continue: { title: t('rm.det.continueTitle'), body: t('rm.det.continueBody'), confirmText: t('rm.det.resumeConfirm'), reason: null, path: 'continue', done: t('rm.det.resumed') },
      cancel: { title: t('rm.det.cancelTitle', { name }), body: t('rm.det.cancelBody'), confirmText: t('rm.det.cancelConfirm'), reason: 'required', danger: true, path: 'cancel', done: t('rm.det.cancelled') },
    }[kind];
    confirmL1({
      title: conf.title,
      body: conf.body,
      confirmText: conf.confirmText,
      danger: conf.danger,
      reason: conf.reason,
      onConfirm: async ({ reason, errBox }) => {
        try {
          await api(`/api/v1/rollouts/${encodeURIComponent(id)}/${conf.path}`, { method: 'POST', body: reason ? { reason } : {} });
          toast({ kind: 'success', title: conf.done });
          refreshRolloutCount();
          load();
          return true;
        } catch (err) {
          const a = errorAlert(err, { title: t('rm.det.actionFailed') });
          mount(errBox, a);
          a.focus();
          return false;
        }
      },
    });
  }

  async function submitDraft() {
    let p = r.preview;
    try {
      const d = await api(`/api/v1/rollouts/${encodeURIComponent(id)}`);
      p = d.preview || p;
    } catch {
      /* 沿用載入時的預覽 */
    }
    if (!p || !p.eligible) {
      toast({ kind: 'error', title: t('rm.new.noEligible') });
      return;
    }
    const res = await submitDialog({
      name: r.name,
      preview: p,
      submit: (confirm, reason) => api(`/api/v1/rollouts/${encodeURIComponent(id)}/submit`, { method: 'POST', body: { confirm, reason } }),
    });
    if (!res) return;
    toast({ kind: 'success', title: t('rm.new.submitted', { name: r.name }) });
    refreshRolloutCount();
    load();
  }

  function btn(text, onClick, variant = 'secondary', iconName = null) {
    return h('button', { type: 'button', class: ['ds-button', `ds-button--${variant}`], on: { click: onClick } }, iconName ? icon(iconName) : null, text);
  }

  // ---- 呈現 ----
  function renderHead() {
    const acts = [];
    const write = session.can('rollouts:write');
    if (r.state === 'draft' && write) acts.push(btn(t('rm.det.submitDraft'), submitDraft, 'primary'));
    if (r.state === 'running' && session.can('rollouts:pause')) acts.push(btn(t('rm.det.pause'), () => action('pause'), 'secondary', 'pause'));
    if (r.state === 'running' && r.awaiting_continue && write) acts.push(btn(t('rm.det.resume'), () => action('continue'), 'primary', 'play'));
    if (r.state === 'paused' && write) acts.push(btn(t('rm.det.resume'), () => action('resume'), 'primary', 'play'));
    if ((r.state === 'running' || r.state === 'paused') && write) acts.push(btn(t('rm.det.cancel'), () => action('cancel'), 'danger-secondary'));
    ctx.setTitle(r.name);
    mount(
      head,
      pageHead({
        title: r.name,
        crumbs: [CRUMB, { text: r.name }],
        meta: [rolloutStateBadge(r.state), ' ', t('rm.det.meta', { v: r.target_version, who: r.created_by || '—', time: fmt.dateTime(r.created_at, { seconds: false }) })],
        actions: acts.length ? acts : null,
      }),
    );
  }

  async function renderAlerts() {
    const out = [];
    if (r.state === 'draft') out.push(inlineAlert({ kind: 'info', body: t('rm.det.draftNote') }));
    if (r.state === 'paused') {
      const auto = Boolean(r.pause_cause) && r.pause_cause !== 'manual';
      const acts = [
        btn(t('rm.det.showFailed'), () => {
          stateSel.value = FAILED.join(',');
          filter.state = stateSel.value;
          loadHosts();
          stateSel.focus();
        }),
      ];
      if (session.can('rollouts:write')) acts.push(btn(t('rm.det.resume'), () => action('resume')));
      out.push(
        inlineAlert({
          kind: 'warning',
          // 自動暫停（pause_cause 不是 manual：失敗比例、回滾失敗、簽章失敗）才附 KRS-W8414
          title: auto
            ? r.pause_reason
              ? t('rm.det.autoPausedTitle', { reason: r.pause_reason })
              : t('rm.det.autoPausedTitleNoReason')
            : r.pause_reason
              ? t('rm.det.pausedTitle', { reason: r.pause_reason })
              : t('rm.det.pausedTitleNoReason'),
          code: auto ? 'KRS-W8414' : null,
          body: t('rm.det.pausedBody'),
          actions: acts,
        }),
      );
    }
    const na = r.next_action || {};
    if (r.state === 'running' && na.code === 'soaking') out.push(inlineAlert({ kind: 'info', role: 'status', body: t('rm.det.soaking', { m: fmt.num(na.minutes || 0), t: fmt.dateTime(na.until, { seconds: false }) }) }));
    if (r.state === 'running' && na.code === 'waiting_continue') out.push(inlineAlert({ kind: 'info', role: 'status', body: t('rm.det.awaiting') }));
    if (r.state === 'cancelled' && r.cancel_reason) out.push(h('p', { class: 'caption' }, t('rm.det.cancelReason', { r: r.cancel_reason })));
    // 回滾失敗（最上方，嚴重）：只有尚未標記為已處理的主機（rollback_failed_unacknowledged）
    if (r.rollback_failed_unacknowledged > 0) {
      try {
        const list = (await api(`/api/v1/rollouts/${encodeURIComponent(id)}/hosts${qs({ state: 'rollback_failed', limit: 100 })}`)) || [];
        const open = list.filter((x) => !x.acknowledged_at);
        if (open.length) {
          out.unshift(
            inlineAlert({
              kind: 'error',
              title: t('rm.det.rollbackFailed', { n: fmt.num(open.length) }),
              code: 'KRS-E8408',
              body: [h('p', null, t('rm.det.rollbackFailedBody')), h('ul', { class: 'k-plain-list' }, open.map((x) => h('li', null, hostLink(x.agent_id, x.hostname), x.code ? [' ', h('code', { class: 'k-code-tag' }, x.code)] : null)))],
            }),
          );
        }
      } catch {
        /* 主機表仍會列出 */
      }
    }
    if (!disposed) mount(alerts, ...out);
  }

  function renderWaves() {
    if (r.state === 'draft') {
      mount(wavesBox, r.preview ? card({ title: t('rm.new.preview'), body: previewView(r.preview, { version: r.target_version }) }) : null);
      return;
    }
    const waves = r.waves || [];
    mount(
      wavesBox,
      card({
        title: t('rm.det.waves'),
        body: h(
          'ol',
          { class: 'k-ro-waves', id: 'rollout-waves' },
          waves.map((w, i) => {
            const g = progressGroups(w.progress);
            const meta = [
              w.soak_minutes ? t('rm.det.waveSoak', { m: fmt.num(w.soak_minutes) }) : null,
              w.manual_continue ? t('rm.det.waveManual') : null,
              t('rm.det.waveFail', { r: fmt.pct((w.failure_ratio || 0) * 100), max: fmt.pct((w.max_failure_ratio || 0) * 100) }),
            ].filter(Boolean);
            return h(
              'li',
              { class: w.current && r.state !== 'completed' && r.state !== 'cancelled' ? 'is-current' : null, dataset: { wave: String(i) } },
              h('span', { class: 'k-ro-wave__n', 'aria-hidden': 'true' }, String(i + 1)),
              h(
                'div',
                null,
                h('p', { class: 'strong' }, i === 0 && waves.length > 1 ? t('rm.det.wave0') : t('rm.det.wave1'), ' · ', t('rm.det.waveLine', { n: fmt.num(w.hosts || 0) }), w.current && r.state !== 'completed' && r.state !== 'cancelled' ? [' · ', t('rm.det.waveCurrent')] : null),
                h('p', null, g.total ? countsLine(w.progress) : '—'),
                h('p', { class: 'caption' }, meta.join(' · ')),
              ),
            );
          }),
        ),
      }),
    );
  }

  function renderHostsCard() {
    if (r.state === 'draft') {
      mount(hostsBox);
      return;
    }
    const prev = waveSel.value;
    mount(waveSel, h('option', { value: '' }, t('rm.ro.filter.all')), (r.waves || []).map((w, i) => h('option', { value: String(i) }, i === 0 && (r.waves || []).length > 1 ? t('rm.det.wave0') : t('rm.det.wave1'))), h('option', { value: '-1' }, t('rm.det.skippedWave')));
    waveSel.value = prev;
    if (hostsBox.firstChild) return;
    mount(
      hostsBox,
      card({
        title: t('rm.det.hosts'),
        body: [
          h(
            'section',
            { class: 'ds-filterbar', 'aria-label': t('hosts.filters') },
            h('div', { class: 'ds-filterbar__row' }, fq),
            h('div', { class: 'k-filter-selects' }, h('label', { class: 'k-labelled-select' }, t('rm.det.filter.wave'), waveSel), h('label', { class: 'k-labelled-select' }, t('rm.det.filter.state'), stateSel)),
          ),
          table.el,
        ],
      }),
    );
  }

  async function load({ first = false } = {}) {
    try {
      r = await api(`/api/v1/rollouts/${encodeURIComponent(id)}`);
    } catch (err) {
      if (disposed) return;
      if (isUnavailable(err)) {
        mount(root, pageHead({ title: t('rm.ro.title'), crumbs: [CRUMB, { text: id }] }), unavailableState(err, 'upgrade'));
        return;
      }
      if (err.code === 'rollout_not_found') {
        ctx.setTitle(t('rm.det.notFound'));
        mount(
          root,
          pageHead({ title: t('rm.det.notFound'), crumbs: [CRUMB, { text: id }] }),
          emptyState({ title: t('rm.det.notFound'), body: t('rm.det.notFoundBody'), iconName: 'upload', actions: [h('a', { class: 'ds-button ds-button--secondary', href: '/pulse/rollouts' }, icon('chevron-left'), t('rm.det.back'))] }),
        );
        ctx.focusTitle();
        return;
      }
      mount(root, pageHead({ title: t('rm.ro.title'), crumbs: [CRUMB, { text: id }] }), errorState(err, { title: t('rm.det.loadFailed'), onRetry: () => load({ first }) }));
      return;
    }
    if (disposed) return;
    if (first || !root.contains(head)) mount(root, head, alerts, wavesBox, hostsBox);
    renderHead();
    renderWaves();
    renderHostsCard();
    await renderAlerts();
    if (r.state !== 'draft' && table.count <= PAGE) loadHosts();
    if (first) ctx.focusTitle();
  }

  load({ first: true });
  const unsub = sse.subscribe((ev) => {
    if (ev.type === 'resync') {
      load().finally(() => sse.resyncSettled());
      return;
    }
    if (ev.type !== 'rollout.progress' || !ev.data || ev.data.rollout_id !== id) return;
    clearTimeout(timer);
    timer = setTimeout(() => load(), 2000);
  });
  return {
    dispose() {
      disposed = true;
      clearTimeout(timer);
      clearTimeout(qTimer);
      unsub();
    },
  };
}
