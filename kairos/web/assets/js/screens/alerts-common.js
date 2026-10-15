// 告警畫面（S06～S11）的共用呈現與動作：嚴重度徽章、事件狀態、告警引擎橫幅、確認／指派／暫停／解決的對話框、
// 主機挑選器、範圍欄位、時間輸入（顯示時區）。API 見 kairos/API.md 第 13 節。

import { h, icon, uid, mount, announce } from '../core/dom.js';
import { t, has, severityLabel } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import * as prefs from '../core/prefs.js';
import { statusBadge, neutralBadge, inlineAlert, field, input, button, setBusy, errorAlert } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { short } from '../core/errors.js';

export const SEVERITIES = ['emergency', 'critical', 'warning', 'info'];
export const STATES = ['firing', 'acknowledged', 'recovered', 'resolved'];
export const CATEGORIES = ['host', 'aggregate', 'system', 'identity'];
export const ROOT_CAUSES = ['hardware', 'network', 'unregistered_maintenance', 'software', 'false_positive', 'other'];
/** 使用者可以建立的規則種類（API.md 第 13.3 節；憑證即將到期、版本過舊延後，不列） */
export const USER_KINDS = ['host_offline', 'host_long_offline', 'report_stale', 'metric_threshold', 'identity_conflict'];

/** 告警嚴重度的 StatusBadge（緊急是唯一的實心徽章；design-system README §6.4.1） */
export function sevBadge(sev) {
  return statusBadge(SEVERITIES.includes(sev) ? sev : 'info', severityLabel(sev));
}

/** 事件狀態的文字（觸發中以粗體） */
export function stateText(e) {
  const label = t(`alerts.state.${e.state}`);
  if (e.state === 'recovered' && !e.acked && e.attention === 'action') {
    return h('span', null, label, h('span', { class: 'k-sub' }, t('alerts.state.unacked')));
  }
  return e.state === 'firing' ? h('strong', null, label) : label;
}

/** 處理類型的中性標籤（規劃提醒、紀錄；alerts-ux.md 2.3） */
export function attentionTag(e) {
  if (e.attention === 'advisory') return neutralBadge(t('alerts.attention.advisory'));
  if (e.attention === 'record') return neutralBadge(t('alerts.attention.record'));
  return null;
}

/** 靜音中、維護中、抖動中的附加標記 */
export function flags(e) {
  const out = [];
  if (e.silenced) out.push(h('span', { class: 'k-flag k-flag--muted' }, icon('bell-off'), e.in_maintenance ? t('alerts.flag.maintenance') : t('alerts.flag.silenced')));
  if (e.flapping) out.push(h('span', { class: 'k-flag' }, icon('refresh'), t('alerts.flag.flapping')));
  return out;
}

/** 執行者、負責人的顯示（API 為 `user <帳號>`、`token <id>`、`system`） */
export function who(s) {
  if (!s) return s;
  if (s.startsWith('user ')) return s.slice(5);
  if (s.startsWith('user:')) return s.slice(5);
  if (s.startsWith('token ')) return t('alerts.who.token', { id: s.slice(6) });
  if (s === 'system') return t('alerts.tl.system');
  return s;
}

export function needsAction(e) {
  return e.attention === 'action';
}

/** 事件持續的秒數（未結束時到現在） */
export function durationSecs(e) {
  const end = Date.parse(e.resolved_at || e.recovered_at || '') || fmt.serverNow();
  return Math.max(0, (end - Date.parse(e.started_at)) / 1000);
}

/** 規則種類的中文 */
export function kindLabel(kind) {
  return has(`alerts.kind.${kind}`) ? t(`alerts.kind.${kind}`) : kind;
}

/** 根因 */
export function rootCauseLabel(c) {
  return has(`alerts.rootCause.${c}`) ? t(`alerts.rootCause.${c}`) : c;
}

/** 處理紀錄的一行文字 */
export function timelineText(item) {
  const d = item.details || {};
  const key = `alerts.tl.${item.action}`;
  let text = has(key) ? t(key) : item.action;
  const extra = [];
  if (item.action === 'acknowledged' && d.assign) extra.push(t('alerts.tl.assignSelf'));
  if (item.action === 'acknowledged' && d.eta_minutes) extra.push(t('alerts.tl.eta', { n: d.eta_minutes }));
  if (item.action === 'assigned' && d.assignee) extra.push(t('alerts.tl.assignee', { who: d.assignee }));
  if (item.action === 'snoozed' && d.hours) extra.push(t('alerts.tl.hours', { n: d.hours }));
  if (item.action === 'resolved' && d.root_cause) extra.push(t('alerts.tl.rootCause', { c: rootCauseLabel(d.root_cause) }));
  if (item.action === 'notified' && Array.isArray(d.channels) && d.channels.length) extra.push(d.channels.join('、'));
  if (item.action === 'escalated' && d.severity) extra.push(severityLabel(d.severity));
  if (extra.length) text = `${text}（${extra.join('，')}）`;
  return text;
}

/** 告警引擎的狀態橫幅（S06「告警引擎未在執行」比空清單更重要）；回 null 表示正常 */
export function engineNotice(engine) {
  if (!engine) return null;
  if (engine.running === false) {
    return h(
      'div',
      { class: 'ds-banner ds-banner--critical k-alert-banner', role: 'alert' },
      icon('status-critical'),
      h('div', { class: 'ds-banner__body' }, h('strong', null, t('alerts.engine.stopped')), ' ', engine.last_error ? t('alerts.engine.lastError', { e: engine.last_error }) : t('alerts.engine.stoppedFix')),
    );
  }
  if (engine.warmup && engine.warmup.active) {
    return inlineAlert({
      kind: 'info',
      title: t('alerts.engine.warmup'),
      body: t('alerts.engine.warmupBody', { time: fmt.time(engine.warmup.until, { seconds: false }), n: fmt.num(engine.warmup.missing_hosts) }),
    });
  }
  return null;
}

/** 告警引擎沒有啟動（503 alerts_unavailable） */
export function unavailableNotice() {
  return h(
    'div',
    { class: 'ds-banner ds-banner--critical k-alert-banner', role: 'alert' },
    icon('status-critical'),
    h('div', { class: 'ds-banner__body' }, h('strong', null, t('alerts.engine.stopped')), ' ', t('err.alerts_unavailable.reason')),
  );
}

// ---------------------------------------------------------------------------
// 時間輸入（datetime-local；顯示時區為個人偏好，不採用瀏覽器時區）

/** 毫秒 → `YYYY-MM-DDTHH:mm`（顯示時區） */
export function toLocalInput(ms) {
  const d = fmt.dateTime(ms, { seconds: false });
  return d === '—' ? '' : d.replace(' ', 'T');
}

/** `YYYY-MM-DDTHH:mm`（顯示時區）→ 毫秒；格式錯誤時 null */
export function fromLocalInput(v) {
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})/.exec(v || '');
  if (!m) return null;
  const asUtc = Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5]);
  // 以近似時刻的位移換算，再以結果的位移校正一次（跨日光節約時間）
  let ms = asUtc - fmt.tzOffsetMinutes(asUtc, prefs.get('timeZone')) * 60000;
  ms = asUtc - fmt.tzOffsetMinutes(ms, prefs.get('timeZone')) * 60000;
  return ms;
}

// ---------------------------------------------------------------------------
// 文字清單欄位（標籤、網段：以逗號或換行分隔）

export function splitList(v) {
  return String(v || '')
    .split(/[,，\n]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/**
 * 主機挑選器：以主機名稱、IP 搜尋（GET /api/v1/hosts?q=），選取的主機以 agent_id 送出、以名稱顯示。
 * @returns {{el, get(): string[], set(list: {agent_id, name}[]), onChange(fn)}}
 */
export function hostPicker({ label, hint, disabled = false, initial = [] } = {}) {
  let selected = initial.slice();
  const listeners = [];
  const chips = h('div', { class: 'k-picker__chips', 'aria-live': 'polite' });
  const results = h('ul', { class: 'k-picker__results' });
  const q = input({ type: 'search', name: 'host-search', placeholder: t('alerts.picker.placeholder') });
  q.disabled = disabled;
  q.setAttribute('autocomplete', 'off');
  let timer = null;
  let ctrl = null;
  const fire = () => listeners.forEach((fn) => fn(selected));
  function drawChips() {
    mount(
      chips,
      selected.length
        ? selected.map((s) =>
            h(
              'span',
              { class: 'ds-filterbar__applied-chip', dataset: { agent: s.agent_id } },
              s.name || s.agent_id,
              disabled
                ? null
                : h(
                    'button',
                    {
                      type: 'button',
                      'aria-label': t('alerts.picker.remove', { name: s.name || s.agent_id }),
                      title: t('alerts.picker.remove', { name: s.name || s.agent_id }),
                      on: {
                        click: () => {
                          selected = selected.filter((x) => x.agent_id !== s.agent_id);
                          drawChips();
                          fire();
                          q.focus();
                        },
                      },
                    },
                    icon('close'),
                  ),
            ),
          )
        : h('span', { class: 'muted' }, t('alerts.picker.none')),
    );
  }
  async function search() {
    const v = q.value.trim();
    if (ctrl) ctrl.abort();
    if (v.length < 2) {
      mount(results);
      return;
    }
    ctrl = new AbortController();
    try {
      const rows = await api(`/api/v1/hosts${qs({ q: v, limit: 8, sort: 'hostname' })}`, { signal: ctrl.signal });
      mount(
        results,
        rows.length
          ? rows.map((r) =>
              h(
                'li',
                null,
                h(
                  'button',
                  {
                    type: 'button',
                    class: 'ds-button ds-button--ghost ds-button--sm',
                    on: {
                      click: () => {
                        if (!selected.some((x) => x.agent_id === r.agent_id)) selected.push({ agent_id: r.agent_id, name: r.display_name || r.hostname });
                        q.value = '';
                        mount(results);
                        drawChips();
                        fire();
                        q.focus();
                        announce(t('alerts.picker.added', { name: r.display_name || r.hostname }));
                      },
                    },
                  },
                  icon('plus'),
                  r.display_name || r.hostname,
                  r.ip ? h('span', { class: 'k-sub mono' }, r.ip) : null,
                ),
              ),
            )
          : h('li', { class: 'muted' }, t('alerts.picker.noMatch')),
      );
    } catch (e) {
      if (e.name !== 'AbortError') mount(results, h('li', { class: 'muted' }, short(e)));
    }
  }
  q.addEventListener('input', () => {
    clearTimeout(timer);
    timer = setTimeout(search, 250);
  });
  q.addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter') {
      ev.preventDefault();
      clearTimeout(timer);
      search();
    }
  });
  const f = field({ label, input: q, hint });
  const el = h('div', { class: 'k-picker' }, f, chips, results);
  drawChips();
  return {
    el,
    get: () => selected.map((s) => s.agent_id),
    set(list) {
      selected = list.slice();
      drawChips();
    },
    onChange(fn) {
      listeners.push(fn);
    },
  };
}

// ---------------------------------------------------------------------------
// 事件的動作（API.md 第 13.2 節）。每個函式成功時 resolve(true)，取消時 false。

function radioGroup(name, items, checked) {
  return h(
    'div',
    { class: 'ds-radio-group' },
    items.map(([value, text]) =>
      h('label', { class: 'ds-radio' }, h('input', { type: 'radio', name, value, checked: value === checked }), text),
    ),
  );
}

/** 確認並指派給我（ETA：15 分／30 分／1 小時／不指定） */
export function ackDialog(e) {
  return new Promise((resolve) => {
    const name = `eta-${uid('r')}`;
    const errBox = h('div');
    const ok = button({ text: t('alerts.ack.submit'), variant: 'primary' });
    const cancel = button({ text: t('common.cancel'), onClick: () => d.close() });
    let done = false;
    const d = openDialog({
      title: t('alerts.ack.title'),
      size: 'sm',
      body: h(
        'div',
        null,
        h('p', null, e.title),
        h('fieldset', null, h('legend', null, t('alerts.ack.eta')), radioGroup(name, [['15', t('alerts.ack.eta15')], ['30', t('alerts.ack.eta30')], ['60', t('alerts.ack.eta60')], ['', t('alerts.ack.etaNone')]], '30')),
        h('p', { class: 'caption' }, t('alerts.ack.note')),
        errBox,
      ),
      actions: [cancel, ok],
      initialFocus: ok,
      onClose: () => {
        if (!done) resolve(false);
      },
    });
    ok.addEventListener('click', async () => {
      const v = d.el.querySelector(`input[name="${name}"]:checked`);
      const body = { assign_to_me: true };
      if (v && v.value) body.eta_minutes = Number(v.value);
      setBusy(ok, true, t('common.working'));
      try {
        await api(`/api/v1/alerts/${encodeURIComponent(e.event_id)}/ack`, { method: 'POST', body });
        done = true;
        d.close();
        toast({ kind: 'success', title: t('alerts.ack.done', { id: e.event_id }), desc: e.title });
        resolve(true);
      } catch (err) {
        setBusy(ok, false, true);
        mount(errBox, errorAlert(err, { title: t('alerts.ack.failed') }));
      }
    });
  });
}

/** 指派給…（負責人為帳號；指派不等於確認） */
export function assignDialog(events) {
  return new Promise((resolve) => {
    const inp = input({ name: 'assignee', autocomplete: 'off', maxlength: 100 });
    const f = field({ label: t('alerts.assign.who'), input: inp, hint: t('alerts.assign.hint'), required: true });
    const errBox = h('div');
    const ok = button({ text: t('alerts.assign.submit'), variant: 'primary' });
    const cancel = button({ text: t('common.cancel'), onClick: () => d.close() });
    let done = false;
    const d = openDialog({
      title: events.length === 1 ? t('alerts.assign.title1') : t('alerts.assign.titleN', { n: events.length }),
      size: 'sm',
      body: h('div', null, f, errBox),
      actions: [cancel, ok],
      initialFocus: inp,
      onClose: () => {
        if (!done) resolve(false);
      },
    });
    ok.addEventListener('click', async () => {
      const who = inp.value.trim();
      if (!who) {
        f.setError(t('alerts.assign.required'));
        inp.focus();
        return;
      }
      f.setError('');
      setBusy(ok, true, t('common.working'));
      let failed = 0;
      let lastErr = null;
      for (const e of events) {
        try {
          await api(`/api/v1/alerts/${encodeURIComponent(e.event_id)}/assign`, { method: 'POST', body: { assignee: who } });
        } catch (err) {
          failed += 1;
          lastErr = err;
        }
      }
      if (failed === events.length) {
        setBusy(ok, false, true);
        mount(errBox, errorAlert(lastErr, { title: t('alerts.assign.failed') }));
        return;
      }
      done = true;
      d.close();
      toast({ kind: failed ? 'warning' : 'success', title: t('alerts.assign.done', { who, n: events.length - failed }), desc: failed ? t('alerts.batch.someFailed', { n: failed }) : null });
      resolve(true);
    });
  });
}

/** 解決…（根因必填；仍有未恢復的子事件時提示會成為獨立事件） */
export function resolveDialog(events, { unrecoveredChildren = 0 } = {}) {
  return new Promise((resolve) => {
    const sel = h(
      'select',
      { class: 'ds-select', name: 'root_cause' },
      h('option', { value: '' }, t('alerts.resolve.choose')),
      ROOT_CAUSES.map((c) => h('option', { value: c }, rootCauseLabel(c))),
    );
    const cf = field({ label: t('alerts.resolve.cause'), input: sel, required: true });
    const note = h('textarea', { class: 'ds-textarea', name: 'note', maxlength: 2000 });
    const nf = field({ label: t('alerts.resolve.note'), input: note });
    const errBox = h('div');
    const ok = button({ text: t('alerts.resolve.submit'), variant: 'primary' });
    const cancel = button({ text: t('common.cancel'), onClick: () => d.close() });
    let done = false;
    const d = openDialog({
      title: events.length === 1 ? t('alerts.resolve.title1', { id: events[0].event_id }) : t('alerts.resolve.titleN', { n: events.length }),
      size: 'md',
      body: h(
        'div',
        null,
        unrecoveredChildren ? inlineAlert({ kind: 'warning', title: t('alerts.resolve.children', { n: unrecoveredChildren }), body: t('alerts.resolve.childrenBody') }) : null,
        cf,
        nf,
        h('p', { class: 'caption' }, t('alerts.resolve.audit')),
        errBox,
      ),
      actions: [cancel, ok],
      initialFocus: sel,
      onClose: () => {
        if (!done) resolve(false);
      },
    });
    ok.addEventListener('click', async () => {
      if (!sel.value) {
        cf.setError(t('alerts.resolve.causeRequired'));
        sel.focus();
        return;
      }
      cf.setError('');
      setBusy(ok, true, t('common.working'));
      let failed = 0;
      let lastErr = null;
      let detached = 0;
      for (const e of events) {
        try {
          const r = await api(`/api/v1/alerts/${encodeURIComponent(e.event_id)}/resolve`, { method: 'POST', body: { root_cause: sel.value, note: note.value.trim() || undefined } });
          detached += (r && r.result && r.result.detached_children) || 0;
        } catch (err) {
          failed += 1;
          lastErr = err;
        }
      }
      if (failed === events.length) {
        setBusy(ok, false, true);
        mount(errBox, errorAlert(lastErr, { title: t('alerts.resolve.failed') }));
        return;
      }
      done = true;
      d.close();
      toast({
        kind: failed ? 'warning' : 'success',
        title: t('alerts.resolve.done', { n: events.length - failed }),
        desc: [detached ? t('alerts.resolve.detached', { n: detached }) : null, failed ? t('alerts.batch.someFailed', { n: failed }) : null].filter(Boolean).join(' ') || null,
      });
      resolve(true);
    });
  });
}

/** 暫停通知 1 小時（只停止這個事件的重送，不改變狀態） */
export async function snooze(e, hours = 1) {
  try {
    await api(`/api/v1/alerts/${encodeURIComponent(e.event_id)}/snooze`, { method: 'POST', body: { hours } });
    toast({ kind: 'success', title: t('alerts.snooze.done', { n: hours }), desc: t('alerts.snooze.desc') });
    return true;
  } catch (err) {
    toast({ kind: 'error', title: short(err, { title: t('alerts.snooze.failed') }) });
    return false;
  }
}

/** 寫入權限（全站範圍；ia.md §4.2） */
export const can = (p) => session.can(p);
