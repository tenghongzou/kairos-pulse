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
// - 即時更新（REMOTE.md 第 8.4 節）：以 sse.watchRollout(id) 訂閱這個推送的 rollout.host，收到就地更新那一列
//   （不重新抓取、不重排；不在目前頁面的主機略過）；rollout.progress 2 秒內合併後重新載入頁首、說明與批次的統計，
//   主機表只在伺服器要求（hosts_resync：一輪改變的主機太多，不逐台送）、出現表中沒有的主機、或這段期間沒有收到
//   任何 rollout.host（沒有訂閱成功時的後援）時才重新載入第一頁。resync 時整頁重新抓取。

import { h, icon, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import * as fmt from '../core/format.js';
import { pageHead, card, emptyState, errorState, errorAlert, inlineAlert, field, skeletonBlock, dl, setBusy } from '../ui/components.js';
import { createTable } from '../ui/table.js';
import { toast } from '../ui/toast.js';
import { confirmL1, confirmL2 } from './admin-common.js';
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
  waveName,
  scheduleSummary,
  submissionView,
  waitedText,
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
  // 即時更新：上一則 rollout.progress 之後收到的 rollout.host 數、下一次是否要重新載入主機表
  let hostEvents = 0;
  let reloadHosts = false;

  const head = h('div');
  const alerts = h('div', { class: 'k-stack--sm' });
  // 待核准：送審摘要；送出過的推送：覆核紀錄
  const reviewBox = h('div');
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
      reject: { title: t('rm.det.rejectTitle', { name }), body: t('rm.det.rejectBody'), confirmText: t('rm.det.rejectConfirm'), reason: 'required', danger: true, path: 'reject', done: t('rm.det.rejectedDone') },
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
    toast({ kind: 'success', title: res.state === 'pending_approval' ? t('rm.new.submittedApproval', { name: r.name }) : t('rm.new.submitted', { name: r.name }) });
    refreshRolloutCount();
    load();
  }

  /**
   * 核准（L2；含降版時為 L3 字串＋原因必填；ADR-0002 決策點 2b）：對話框內嵌送審摘要，輸入伺服器算出的確認字串
   * （submission.confirm_text）後送出 confirm 與 digest——伺服器在核准的交易中重新計算並比對，畫面上的比對只是
   * 防止誤按。摘要過期（409 submission_changed，或確認字串已經不同）時關閉對話框並重新載入摘要。
   */
  async function approve() {
    // 開啟對話框前重新取一次，讓核准者看到的是最新的摘要
    try {
      r = await api(`/api/v1/rollouts/${encodeURIComponent(id)}`);
    } catch {
      /* 沿用載入時的內容；送出時伺服器仍會驗證 */
    }
    if (disposed) return;
    const sub = r.submission;
    if (r.state !== 'pending_approval' || !sub || !(r.viewer && r.viewer.can_approve)) {
      load();
      return;
    }
    const l3 = sub.confirm_level === 'L3';
    const starts = r.schedule && r.schedule.starts_at && new Date(r.schedule.starts_at).getTime() > fmt.serverNow() ? r.schedule.starts_at : null;
    const stale = () => {
      toast({ kind: 'warning', title: t('rm.det.submissionChanged') });
      refreshRolloutCount();
      load();
      return true;
    };
    confirmL2({
      title: t('rm.det.approveTitle', { name: r.name }),
      lead: t('rm.det.approveLead', { n: fmt.num(sub.eligible || 0), v: r.target_version }),
      body: h('div', { class: 'k-stack--sm', id: 'approve-summary' }, h('p', { class: 'strong' }, starts ? t('rm.det.approveStartsLater', { t: fmt.dateTime(starts, { seconds: false }) }) : t('rm.det.approveStartsNow')), submissionView(r)),
      phrase: sub.confirm_text,
      confirmText: sub.confirm_text,
      reason: l3 ? 'required' : 'optional',
      onConfirm: async ({ reason, errBox }) => {
        try {
          await api(`/api/v1/rollouts/${encodeURIComponent(id)}/approve`, { method: 'POST', body: { confirm: sub.confirm_text, digest: sub.digest, reason: reason || undefined } });
          toast({ kind: 'success', title: t('rm.det.approved') });
          refreshRolloutCount();
          load();
          return true;
        } catch (err) {
          if (err.code === 'submission_changed') return stale();
          if (err.code === 'confirmation_required' && err.details && err.details.confirm_text && err.details.confirm_text !== sub.confirm_text) return stale();
          if (err.code === 'invalid_state') {
            // 同時有人核准、退回、修改或取消：重新載入目前的狀態
            toast({ kind: 'warning', title: t('rm.det.actionFailed'), desc: err.message });
            refreshRolloutCount();
            load();
            return true;
          }
          const a = errorAlert(err, { title: err.code === 'artifact_missing' ? t('rm.det.approveFileMissing') : t('rm.det.approveFailed') });
          mount(errBox, a);
          a.focus();
          return false;
        }
      },
    });
  }

  async function copyLink() {
    try {
      await navigator.clipboard.writeText(window.location.href);
      toast({ kind: 'success', title: t('rm.det.copied') });
    } catch {
      toast({ kind: 'error', title: t('rm.det.copyFailed') });
    }
  }

  /** 完整的主機清單（CSV）：以分頁的主機表端點取回已固定的對象，在瀏覽器組成檔案 */
  async function exportHosts(button) {
    setBusy(button, true, t('rm.sub.exporting'));
    const cell = (v) => {
      const text = v == null ? '' : String(v);
      // 以 = + - @ 開頭的儲存格前面加單引號，避免試算表當成公式
      const safe = /^[=+\-@\t\r]/.test(text) ? `'${text}` : text;
      return /[",\n\r]/.test(safe) ? `"${safe.replace(/"/g, '""')}"` : safe;
    };
    try {
      const rows = [];
      let cursor = null;
      do {
        const res = await api(`/api/v1/rollouts/${encodeURIComponent(id)}/hosts${qs({ limit: 1000, cursor })}`, { raw: true });
        rows.push(...res.data);
        cursor = res.page.next_cursor;
      } while (cursor && !disposed);
      if (disposed) return;
      const lines = [t('rm.sub.csvHead'), ...rows.map((x) => [x.agent_id, x.hostname, x.wave, x.state, x.reason, x.from_version].map(cell).join(','))];
      const url = URL.createObjectURL(new Blob([`﻿${lines.join('\r\n')}\r\n`], { type: 'text/csv;charset=utf-8' }));
      const a = h('a', { href: url, download: `${id}-hosts.csv`, dataset: { native: '1' }, hidden: true });
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 10000);
      toast({ kind: 'success', title: t('rm.sub.exported', { n: fmt.num(rows.length) }) });
    } catch (err) {
      toast({ kind: 'error', title: t('rm.sub.exportFailed'), desc: err.message || err.code });
    } finally {
      setBusy(button, false, t('rm.sub.exporting'));
    }
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
    // 雙人覆核（0.10）：能否核准由後端依這次請求的身分計算（r.viewer）——有 rollouts:approve、以帳號登入、不是
    // 建立者或送出者。可以核准的人才看到「核准…」「退回…」；建立者與送出者看到的是「取消推送…」與「複製連結」
    if (r.state === 'pending_approval') {
      if (r.viewer && r.viewer.can_approve) {
        acts.push(btn(t('rm.det.approve'), approve, 'primary'));
        acts.push(btn(t('rm.det.reject'), () => action('reject'), 'secondary'));
      }
      acts.push(btn(t('rm.det.copyLink'), copyLink, 'secondary', 'copy'));
    }
    if ((r.state === 'running' || r.state === 'paused' || r.state === 'scheduled' || r.state === 'pending_approval') && write) acts.push(btn(t('rm.det.cancel'), () => action('cancel'), 'danger-secondary'));
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
    if (r.state === 'draft' && r.reject_reason) out.push(inlineAlert({ kind: 'warning', title: t('rm.det.rejected'), body: t('rm.det.rejectedBody', { reason: r.reject_reason }) }));
    if (r.state === 'pending_approval') {
      // 依身分的說明（ADR-0002 決策點 2c、2d）：可以核准的人、建立者或送出者、其他人
      const v = r.viewer || {};
      const who = r.submitted_by || '—';
      const time = fmt.dateTime(r.submitted_at, { seconds: false });
      const waited = waitedText(r.submitted_at);
      const whoApproves = r.created_by === r.submitted_by ? t('rm.det.pendingWhoSame', { creator: r.created_by || '—' }) : t('rm.det.pendingWho', { creator: r.created_by || '—', submitter: who });
      const notes = [];
      if (v.can_approve) notes.push(t('rm.det.pendingForApprover', { who, time, waited }));
      else if (v.is_creator || v.is_submitter) notes.push(t('rm.det.pendingForOwner', { time, waited }), t('rm.det.pendingSelf'), whoApproves, session.can('rollouts:write') ? t('rm.det.pendingOwnerActions') : null);
      else notes.push(t('rm.det.pendingBody', { who, time }), whoApproves, v.blocked === 'session_required' ? t('rm.det.pendingToken') : null);
      out.push(inlineAlert({ kind: 'info', role: 'status', title: t('rm.det.pendingTitle'), body: notes.filter(Boolean).map((x) => h('p', null, x)) }));
      // 沒有可核准者（決策點 2g）
      if (r.submission && r.submission.approvers_available === 0) out.push(inlineAlert({ kind: 'warning', title: t('rm.det.noApproversTitle'), body: t('rm.det.noApproversBody') }));
    }
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
    if (r.state === 'scheduled') out.push(inlineAlert({ kind: 'info', role: 'status', body: na.starts_at ? t('rm.det.scheduledNote', { t: fmt.dateTime(na.starts_at, { seconds: false }) }) : t('rm.det.scheduledNoteNoTime') }));
    if (r.state === 'running' && na.code === 'waiting_window') out.push(inlineAlert({ kind: 'info', role: 'status', body: na.until ? t('rm.det.waitingWindow', { t: fmt.dateTime(na.until, { seconds: false }) }) : t('rm.det.waitingWindowNone') }));
    // 等待版本庫的檔案（ADR-0002 F3）：不計入失敗、不暫停，補檔後自動繼續
    if (r.state === 'running' && na.code === 'waiting_artifact') {
      out.push(
        inlineAlert({
          kind: 'warning',
          role: 'status',
          title: t('rm.det.waitingArtifact', { n: fmt.num(na.hosts || 0) }),
          code: 'KRS-W8417',
          body: t('rm.det.waitingArtifactBody'),
          actions: session.can('releases:read') ? [h('a', { class: 'ds-button ds-button--secondary ds-button--sm', href: `/pulse/releases/${encodeURIComponent(r.target_version)}` }, t('rm.det.goReleases'))] : null,
        }),
      );
    }
    const sch = scheduleSummary(r.schedule);
    if (sch) out.push(h('p', { class: 'caption', id: 'rollout-schedule' }, sch));
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

  /** 送審摘要（待核准）與覆核紀錄（送出過的推送；目前這一輪，完整的歷史在稽核日誌） */
  function renderReview() {
    const out = [];
    if (r.state === 'pending_approval' && r.submission) {
      const exportBtn = h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm' }, icon('download'), t('rm.sub.export'));
      exportBtn.addEventListener('click', () => exportHosts(exportBtn));
      out.push(
        card({
          title: t('rm.sub.title'),
          id: 'rollout-submission',
          actions: exportBtn,
          body: [submissionView(r), h('p', { class: 'caption' }, t('rm.sub.digest'), '：', h('code', { class: 'mono', id: 'rollout-digest' }, r.submission.digest), ' ', t('rm.sub.digestHint'))],
        }),
      );
    }
    if (r.submitted_at || r.reject_reason) {
      const whoAt = (who, at) => (at ? t('rm.rev.whoAt', { who: who || '—', time: fmt.dateTime(at, { seconds: false }) }) : t('rm.rev.none'));
      out.push(
        card({
          title: t('rm.rev.title'),
          id: 'rollout-review',
          body: [
            dl([
              [t('rm.rev.created'), whoAt(r.created_by, r.created_at)],
              [t('rm.rev.submitted'), whoAt(r.submitted_by, r.submitted_at)],
              r.approved_at ? [t('rm.rev.approved'), whoAt(r.approved_by, r.approved_at)] : null,
              r.reject_reason ? [t('rm.rev.rejected'), r.reject_reason] : null,
            ]),
            session.can('audit:read') ? h('p', null, h('a', { href: `/admin/audit${qs({ target: id })}` }, t('rm.rev.audit'))) : null,
          ],
        }),
      );
    }
    mount(reviewBox, ...out);
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
                h('p', { class: 'strong' }, waveName(i, waves.length), ' · ', t('rm.det.waveLine', { n: fmt.num(w.hosts || 0) }), w.current && r.state !== 'completed' && r.state !== 'cancelled' ? [' · ', t('rm.det.waveCurrent')] : null),
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
    mount(waveSel, h('option', { value: '' }, t('rm.ro.filter.all')), (r.waves || []).map((w, i) => h('option', { value: String(i) }, waveName(i, (r.waves || []).length))), h('option', { value: '-1' }, t('rm.det.skippedWave')));
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

  async function load({ first = false, hosts = true } = {}) {
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
    if (first || !root.contains(head)) mount(root, head, alerts, reviewBox, wavesBox, hostsBox);
    renderHead();
    renderReview();
    renderWaves();
    renderHostsCard();
    await renderAlerts();
    if (r.state !== 'draft' && hosts && table.count <= PAGE) loadHosts();
    if (first) ctx.focusTitle();
  }

  load({ first: true });
  const unsub = sse.subscribe((ev) => {
    if (ev.type === 'resync') {
      load().finally(() => sse.resyncSettled());
      return;
    }
    if (!ev.data || ev.data.rollout_id !== id) return;
    if (ev.type === 'rollout.host') {
      hostEvents += 1;
      // 就地更新；表中沒有的主機（剛送出、還沒載入）留給下一次重新載入（有篩選或分頁時不在表中是正常的）
      if (!table.updateRow(ev.data) && !nextCursor && !filter.wave && !filter.state && !filter.q) reloadHosts = true;
      return;
    }
    if (ev.type !== 'rollout.progress') return;
    if (ev.data.hosts_resync || hostEvents === 0) reloadHosts = true;
    hostEvents = 0;
    clearTimeout(timer);
    timer = setTimeout(() => {
      const hosts = reloadHosts;
      reloadHosts = false;
      load({ hosts });
    }, 2000);
  });
  const unwatch = sse.watchRollout(id);
  return {
    dispose() {
      disposed = true;
      clearTimeout(timer);
      clearTimeout(qTimer);
      unsub();
      unwatch();
    },
  };
}
