// 遠端管理（F8：S24 集中設定、S25 Pulse 版本庫、S26 升級推送、S04／S05 的整合）共用的呈現。
//
// - 功能關閉（[remote] 停用或 JSONL 儲存）時 API 回 503 remote_unavailable：S24～S26 整頁 EmptyState（設計包 §31.8 F8 #6），
//   側邊導覽的「Pulse 管理」區塊仍顯示，讓管理員知道功能存在。
// - next_action 是固定代碼＋參數（API.md 第 17.3 節），文案由這裡組出。
// - 影響 1,000 台以上的集中設定變更（L1，screens.md S24）：先不帶 impact_token 送出，伺服器回 400 confirmation_required
//   （沒有寫入，details 為預覽）時以 ConfirmDialog 列出改變的鍵與台數，確認後帶 token 重送；數字在確認後改變時
//   伺服器回 409 impact_changed，以新的數字再確認一次。

import { h, icon } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import * as fmt from '../core/format.js';
import { emptyState, statusBadge, neutralBadge } from '../ui/components.js';
import { confirmL1 } from './admin-common.js';

/** 503 remote_unavailable（功能關閉） */
export function isUnavailable(err) {
  return Boolean(err && err.code === 'remote_unavailable');
}

/** 功能關閉時的整頁 EmptyState。area：'config'（集中設定）或 'upgrade'（版本庫與推送） */
export function unavailableState(err, area) {
  return emptyState({
    title: t(area === 'config' ? 'rm.unavailable.config' : 'rm.unavailable.upgrade'),
    body: h(
      'span',
      null,
      t('rm.unavailable.body', { key: area === 'config' ? 'config_enabled' : 'upgrade_enabled' }),
      err && err.message && err.message !== err.code ? [h('br'), h('span', { class: 'caption' }, t('rm.unavailable.detail', { msg: err.message }))] : null,
    ),
    iconName: 'settings',
  });
}

// ---- 集中設定的狀態 ----
export const CONFIG_STATES = ['in_sync', 'pending', 'rejected', 'rolled_back', 'overridden', 'never_reported', 'disabled', 'unsupported'];
const CONFIG_TONE = { in_sync: 'online', pending: 'pending', never_reported: 'pending', rejected: 'warning', rolled_back: 'warning', overridden: 'info' };

export function configStateLabel(s) {
  return s && has(`rm.cs.${s}`) ? t(`rm.cs.${s}`) : s || '—';
}

export function configStateBadge(s) {
  if (!s) return '—';
  const tone = CONFIG_TONE[s];
  return tone ? statusBadge(tone, configStateLabel(s)) : neutralBadge(configStateLabel(s));
}

// ---- 遠端升級的本機狀態（install.upgrade；沒有 install 為 none） ----
export const UPGRADE_STATES = ['ready', 'disabled', 'no_trust_anchor', 'no_helper', 'unsafe_path', 'unsupported', 'none'];

export function upgradeKey(install) {
  return install && install.upgrade ? install.upgrade : 'none';
}

export function upgradeLabel(v) {
  if (!v) return t('rm.up.none');
  return has(`rm.up.${v}`) ? t(`rm.up.${v}`) : t('rm.up.other', { v });
}

export function upgradeBadge(v) {
  if (v === 'ready') return statusBadge('online', upgradeLabel(v));
  if (v === 'no_trust_anchor' || v === 'no_helper' || v === 'unsafe_path') return statusBadge('warning', upgradeLabel(v));
  return neutralBadge(upgradeLabel(v));
}

/** 非就緒時的一行處理方式 */
export function upgradeFix(v) {
  return v && v !== 'ready' && has(`rm.upFix.${v}`) ? t(`rm.upFix.${v}`) : null;
}

export function methodLabel(install) {
  if (!install || !install.method) return t('rm.method.none');
  const m = install.method;
  return has(`rm.method.${m}`) ? t(`rm.method.${m}`) : m;
}

/** Windows 的安裝方式（遠端升級在 0.10） */
export function isWindowsMethod(install) {
  return Boolean(install && (install.method === 'msi' || install.method === 'zip'));
}

// ---- 推送與主機的狀態 ----
export const ROLLOUT_STATES = ['running', 'pending_approval', 'scheduled', 'paused', 'completed', 'cancelled', 'draft'];

export function rolloutStateLabel(s) {
  return has(`rm.rs.${s}`) ? t(`rm.rs.${s}`) : s;
}

export function rolloutStateBadge(s) {
  if (s === 'running') return statusBadge('info', rolloutStateLabel(s));
  if (s === 'paused') return statusBadge('warning', rolloutStateLabel(s));
  if (s === 'completed') return statusBadge('online', rolloutStateLabel(s));
  if (s === 'scheduled' || s === 'pending_approval') return statusBadge('pending', rolloutStateLabel(s));
  return neutralBadge(rolloutStateLabel(s));
}

/**
 * 沒有經過核准就開始（或排定開始）的推送的標記（API 的 approval = 'not_required'；0.10.1）。
 * `emphasize`：目前的設定要求雙人覆核（這個推送是在覆核關閉的期間送出的）時以警告呈現，否則是中性的標籤。
 */
export function unapprovedBadge(emphasize) {
  const b = emphasize ? statusBadge('warning', t('rm.appr.badge')) : neutralBadge(t('rm.appr.badge'));
  b.dataset.approval = 'not_required';
  b.title = t('rm.appr.badgeAria');
  return b;
}

export const HOST_STATES = ['rollback_failed', 'failed', 'rolled_back', 'unknown', 'applying', 'staged', 'accepted', 'offered', 'pending', 'succeeded', 'declined', 'expired', 'skipped', 'skipped_offline', 'cancelled'];
const HOST_TONE = { succeeded: 'online', failed: 'warning', rolled_back: 'warning', rollback_failed: 'critical', unknown: 'unknown', applying: 'info', staged: 'info', accepted: 'info', offered: 'info', pending: 'pending' };

export function hostStateLabel(s) {
  return has(`rm.hs.${s}`) ? t(`rm.hs.${s}`) : s;
}

export function hostStateBadge(s) {
  const tone = HOST_TONE[s];
  return tone ? statusBadge(tone, hostStateLabel(s)) : neutralBadge(hostStateLabel(s));
}

/** 原因代碼的文案；不認得的代碼（之後的版本新增的）顯示為「其他原因（代碼）」，台數與主機照常列出 */
export function skipLabel(reason) {
  if (!reason) return null;
  return has(`rm.skip.${reason}`) ? t(`rm.skip.${reason}`) : t('rm.skip.other', { code: reason });
}

/** 風險提醒（預覽與送審摘要的 warnings[]：伺服器回傳固定代碼）；沒有時 null */
export function warningsList(codes) {
  if (!codes || !codes.length) return null;
  return h(
    'div',
    { class: 'k-ro-warnings' },
    h('h3', { class: 'k-section-title' }, t('rm.warn.title')),
    h(
      'ul',
      { class: 'k-plain-list' },
      codes.map((c) => h('li', { dataset: { warning: c } }, icon('status-warning'), ' ', has(`rm.warn.${c}`) ? t(`rm.warn.${c}`) : t('rm.warn.other', { code: c }))),
    ),
  );
}

/** 已等待多久（待核准的推送）：分鐘、小時、天 */
export function waitedText(since) {
  const ms = fmt.serverNow() - new Date(since).getTime();
  if (!Number.isFinite(ms) || ms < 60000) return t('rm.ro.waited.now');
  const mins = Math.floor(ms / 60000);
  if (mins < 60) return t('rm.ro.waited.minutes', { n: fmt.num(mins) });
  if (mins < 48 * 60) return t('rm.ro.waited.hours', { n: fmt.num(Math.floor(mins / 60)) });
  return t('rm.ro.waited.days', { n: fmt.num(Math.floor(mins / 1440)) });
}

/** 設定來源的文字（file／environment／default） */
export function sourceLabel(src) {
  return has(`rm.src.${src}`) ? t(`rm.src.${src}`) : src || '—';
}

/**
 * 送審摘要（ADR-0002 決策點 2b）：待核准的詳情頁與核准對話框共用。數字、確認字串與風險提醒都由伺服器計算
 * （r.submission），這裡只組文案；不認得的原因與提醒代碼照常列出。
 */
export function submissionView(r) {
  const sub = r.submission || {};
  const skipped = Object.entries(sub.skipped || {}).sort((a, b) => b[1] - a[1]);
  const waves = sub.waves || [];
  const files = sub.artifact_problems || [];
  const sch = scheduleSummary(r.schedule);
  return h(
    'div',
    { class: 'k-stack--sm k-ro-submission' },
    h('p', { class: 'k-preview-big' }, h('span', { class: 'strong' }, t('rm.sub.eligible', { n: fmt.num(sub.eligible || 0) })), ' · ', t('rm.sub.target', { v: r.target_version })),
    sub.downgrade ? h('p', { class: 'strong' }, t('rm.sub.downgrade', { n: fmt.num(sub.downgrade) })) : null,
    sub.no_rollback ? h('p', null, t('rm.sub.noRollback', { n: fmt.num(sub.no_rollback) })) : null,
    waves.length ? h('p', null, t('rm.sub.wavesList', { n: fmt.num(waves.length), list: waves.map((n) => fmt.num(n || 0)).join(' → ') })) : null,
    h('p', null, sch || t('rm.sub.scheduleNone')),
    warningsList(sub.warnings),
    files.length
      ? [
          h('h3', { class: 'k-section-title' }, t('rm.sub.files')),
          h(
            'ul',
            { class: 'k-plain-list' },
            files.map((f) => h('li', null, t('rm.sub.fileLine', { v: f.version, f: f.filename, p: has(`rm.rel.problem.${f.file_problem}`) ? t(`rm.rel.problem.${f.file_problem}`) : f.file_problem, n: fmt.num(f.hosts || 0) }))),
          ),
        ]
      : null,
    h('h3', { class: 'k-section-title' }, t('rm.sub.skipped')),
    skipped.length
      ? h(
          'ul',
          { class: 'k-ro-skip' },
          skipped.map(([reason, n]) => h('li', { dataset: { reason } }, t('rm.new.skipLine', { label: skipLabel(reason), n: fmt.num(n) }))),
        )
      : h('p', { class: 'caption' }, t('rm.sub.skippedNone')),
  );
}

/** 批次的名稱：兩批以上時第一批為「先行批次」、最後一批為「其餘」，中間為「第 N 批」 */
export function waveName(i, total) {
  if (total > 1 && i === 0) return t('rm.det.wave0');
  if (i === total - 1) return t('rm.det.wave1');
  return t('rm.det.waveN', { n: fmt.num(i + 1) });
}

/** 排程的摘要（詳情）：開始、結束、每週的時段與時區；沒有排程時 null */
export function scheduleSummary(s) {
  if (!s || (!s.starts_at && !s.ends_at && !(s.windows && s.windows.length))) return null;
  const parts = [];
  if (s.starts_at) parts.push(t('rm.det.startsAt', { t: fmt.dateTime(s.starts_at, { seconds: false }) }));
  if (s.ends_at) parts.push(t('rm.det.endsAt', { t: fmt.dateTime(s.ends_at, { seconds: false }) }));
  if (s.windows && s.windows.length) {
    const list = s.windows
      .map((w) => {
        const days = w.days && w.days.length && w.days.length < 7 ? w.days.map((d) => t(`rm.day.${d}`)).join('、') : t('rm.day.every');
        return t('rm.det.window', { days, start: w.start, end: w.end });
      })
      .join('；');
    parts.push(t('rm.det.windows', { list, tz: s.timezone || '+08:00' }));
  }
  return parts.join(' · ');
}

/** 推送清單的「下一步」（next_action 的固定代碼＋參數） */
export function nextActionText(na) {
  if (!na || !na.code) return '—';
  switch (na.code) {
    case 'soaking':
      return t('rm.next.soaking', { m: fmt.num(na.minutes ?? 0) });
    case 'pending_approval':
      return t('rm.next.pending_approval');
    case 'scheduled':
      return na.starts_at ? t('rm.next.scheduled', { t: fmt.dateTime(na.starts_at, { seconds: false }) }) : t('rm.next.scheduledNoTime');
    case 'waiting_window':
      return na.until ? t('rm.next.waiting_window', { t: fmt.dateTime(na.until, { seconds: false }) }) : t('rm.next.waiting_windowNone');
    case 'paused':
      return na.reason ? t('rm.next.paused', { reason: na.reason }) : t('rm.next.pausedNoReason');
    case 'waiting_artifact':
      return t('rm.next.waiting_artifact', { n: fmt.num(na.hosts ?? 0) });
    default:
      return has(`rm.next.${na.code}`) ? t(`rm.next.${na.code}`) : na.code;
  }
}

/** 進度的分組：成功／失敗／進行中／未開始／其他（progress 的鍵為 15 個主機狀態） */
export function progressGroups(p) {
  const g = (k) => (p && p[k]) || 0;
  const ok = g('succeeded');
  const fail = g('failed') + g('rolled_back') + g('rollback_failed') + g('unknown');
  const run = g('offered') + g('accepted') + g('staged') + g('applying');
  const todo = g('pending');
  const other = g('declined') + g('expired') + g('skipped_offline') + g('cancelled');
  return { ok, fail, run, todo, other, total: ok + fail + run + todo + other };
}

/** 進度（堆疊的長條＋數字；長條只是輔助，數字是可見文字） */
export function progressCell(p) {
  const g = progressGroups(p);
  const total = Math.max(1, g.total);
  const seg = (n, cls) => {
    if (!n) return null;
    const el = h('span', { class: `k-ro-bar__seg ${cls}` });
    el.style.setProperty('width', `${((n / total) * 100).toFixed(2)}%`);
    return el;
  };
  return h(
    'div',
    { class: 'k-ro-progress' },
    h('div', { class: 'k-ro-bar', 'aria-hidden': 'true' }, seg(g.ok, 'is-ok'), seg(g.fail, 'is-fail'), seg(g.run, 'is-run')),
    h(
      'span',
      { class: 'k-ro-progress__text', 'aria-label': t('rm.ro.progressAria', { ok: fmt.num(g.ok), fail: fmt.num(g.fail), run: fmt.num(g.run), todo: fmt.num(g.todo), total: fmt.num(g.total) }) },
      t('rm.ro.progressText', { ok: fmt.num(g.ok), fail: fmt.num(g.fail), run: fmt.num(g.run), todo: fmt.num(g.todo) }),
    ),
    g.other ? h('span', { class: 'k-sub' }, t('rm.ro.other', { n: fmt.num(g.other) })) : null,
  );
}

// ---- 指派的對象 ----
export function selectorText(kind, value) {
  if (kind === 'global') return t('rm.sel.global');
  if (kind === 'host') return t('rm.sel.host', { v: value && value.length > 13 ? `${value.slice(0, 8)}…` : value });
  if (kind === 'group' || kind === 'tag') return t(`rm.sel.${kind}`, { v: value });
  return `${kind}:${value}`;
}

// ---- 設定值 ----
export function valueText(v) {
  if (v === undefined) return t('rm.value.unset');
  if (v == null) return '—';
  if (v === true) return t('rm.value.true');
  if (v === false) return t('rm.value.false');
  if (Array.isArray(v)) return v.length ? v.join('、') : t('rm.value.emptyList');
  if (typeof v === 'object') return JSON.stringify(v);
  return typeof v === 'number' ? fmt.num(v) : String(v);
}

/** 兩層的設定（`{"report":{"interval":10}}`）→ 扁平（`{"report.interval":10}`） */
export function flatten(settings) {
  const out = {};
  for (const [sec, keys] of Object.entries(settings || {})) {
    if (!keys || typeof keys !== 'object') continue;
    for (const [k, v] of Object.entries(keys)) out[`${sec}.${k}`] = v;
  }
  return out;
}

/** 扁平 → 兩層 */
export function nest(flat) {
  const out = {};
  for (const [key, v] of Object.entries(flat)) {
    const i = key.lastIndexOf('.');
    const sec = key.slice(0, i);
    const name = key.slice(i + 1);
    (out[sec] = out[sec] || {})[name] = v;
  }
  return out;
}

// ---- 影響 1,000 台以上的確認 ----
export const CANCELLED = Symbol('cancelled');

function impactBody(d, changedFrom) {
  const keys = Object.entries(d.by_change || {}).sort((a, b) => b[1] - a[1]);
  return h(
    'div',
    { class: 'k-stack--sm' },
    changedFrom != null ? h('p', { class: 'strong', role: 'status' }, t('rm.impact.changed', { a: fmt.num(changedFrom), b: fmt.num(d.affected_hosts) })) : null,
    keys.length ? [h('p', null, t('rm.impact.confirmBody')), h('ul', { class: 'k-plain-list' }, keys.map(([k, n]) => h('li', null, h('code', { class: 'k-code-tag' }, k), ' ', t('rm.hostsN', { n: fmt.num(n) }))))] : null,
    d.overridden_locally ? h('p', { class: 'caption' }, t('rm.impact.overridden', { n: fmt.num(d.overridden_locally) })) : null,
  );
}

/**
 * 執行一個需要影響範圍確認的寫入：run(impactToken) 第一次不帶 token；伺服器要求確認時顯示 L1 再重送。
 * 回傳 run 的結果，使用者取消時回傳 CANCELLED。
 */
export async function withImpact(run) {
  let token;
  let confirmedN = null;
  for (let i = 0; i < 4; i += 1) {
    try {
      return await run(token);
    } catch (err) {
      const d = err && err.details;
      if (!(err.code === 'confirmation_required' || err.code === 'impact_changed') || !d || !d.impact_token) throw err;
      const changedFrom = err.code === 'impact_changed' && confirmedN != null && confirmedN !== d.affected_hosts ? confirmedN : null;
      const ok = await confirmL1({
        title: t('rm.impact.confirmTitle', { n: fmt.num(d.affected_hosts) }),
        body: impactBody(d, changedFrom),
        confirmText: t('rm.impact.confirm', { n: fmt.num(d.affected_hosts) }),
        reason: null,
      });
      if (!ok) return CANCELLED;
      token = d.impact_token;
      confirmedN = d.affected_hosts;
    }
  }
  throw new Error('impact confirmation loop');
}

/** 影響預覽的一行（aria-live 的區域內） */
export function impactText(d) {
  if (!d) return '';
  if (!d.affected_hosts) {
    return d.unsupported ? `${t('rm.impact.none')} · ${t('rm.impact.unsupported', { n: fmt.num(d.unsupported) })}` : t('rm.impact.none');
  }
  return [
    t('rm.impact.text', { n: fmt.num(d.affected_hosts) }),
    d.overridden_locally ? t('rm.impact.overridden', { n: fmt.num(d.overridden_locally) }) : null,
    d.unsupported ? t('rm.impact.unsupported', { n: fmt.num(d.unsupported) }) : null,
  ]
    .filter(Boolean)
    .join(' · ');
}

/** 小工具：連到主機詳情 */
export function hostLink(agentId, name) {
  return h('a', { href: `/hosts/${agentId}`, class: 'k-host-name' }, name || agentId);
}

/** 警告的小圖示＋文字（下一步的已暫停） */
export function warnText(text) {
  return h('span', { class: 'k-row k-row--tight' }, icon('status-warning'), h('span', null, text));
}
