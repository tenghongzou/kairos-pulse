// S10 靜音與維護時段（設計包 kairos/screens.md S10；API GET/POST/PUT /api/v1/silences、POST /silences/preview、
// POST /silences/{id}/end、GET /alerts/summary）。
//
// - 路由 /alerts/silences（?status=active|upcoming|ended）、/alerts/silences/new（?type=silence|maintenance、
//   ?event=<事件編號>、?hosts=<agent_id,…>、?rules=<rule_id,…>）、/alerts/silences/:id（檢視或修改）。
// - 符合台數、範圍的文字、時長上限與 L1 確認的理由都由伺服器的 preview 提供，畫面直接顯示（S10「0.6 後端現況」）。
// - 時長上限由伺服器驗證：超過時 400 silence_too_long，在「結束時間」欄位就地顯示上限與原因。
// - 0.7.0 隱藏（decisions/INTEGRATION.md §28.6）：「週期性」分頁與「週期」選項、範圍的「群組」；「不計入上線率報表」
//   保留並標示「之後的報表會套用」。

import { h, icon, uid, mount, announce } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { replaceQuery } from '../core/router.js';
import { pageHead, emptyState, errorState, inlineAlert, field, input, button, setBusy, neutralBadge } from '../ui/components.js';
import { openDialog, confirm } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { describe } from '../core/errors.js';
import { hostPicker, splitList, toLocalInput, fromLocalInput, unavailableNotice } from './alerts-common.js';

const STATUSES = ['active', 'upcoming', 'ended'];
const MIN = 60;
const HOUR = 3600;

function kindTag(s) {
  return neutralBadge(t(`alerts.sl.kind.${s.kind}`), s.kind === 'maintenance' ? 'calendar' : 'bell-off');
}

function timeText(s) {
  const start = fmt.dateTime(s.starts_at, { seconds: false });
  const end = fmt.dateTime(s.effective_end || s.ends_at, { seconds: false });
  if (s.status === 'active' && s.remaining_secs != null) return [h('span', null, t('alerts.sl.remaining', { d: fmt.duration(s.remaining_secs) })), h('span', { class: 'k-sub' }, `${start} – ${end}`)];
  return `${start} – ${end}`;
}

/** 明天 08:00（顯示時區）距離現在的秒數 */
function untilTomorrow8() {
  const now = fmt.serverNow();
  const today = toLocalInput(now).slice(0, 10);
  const [y, m, d] = today.split('-').map(Number);
  const next = new Date(Date.UTC(y, m - 1, d + 1));
  const v = `${next.getUTCFullYear()}-${String(next.getUTCMonth() + 1).padStart(2, '0')}-${String(next.getUTCDate()).padStart(2, '0')}T08:00`;
  return Math.round((fromLocalInput(v) - now) / 1000);
}

export function render(ctx) {
  const root = ctx.root;
  const canWrite = session.can('silences:write');
  let status = STATUSES.includes(ctx.query.get('status')) ? ctx.query.get('status') : 'active';
  let disposed = false;
  let list = [];
  let rules = null;

  const summary = h('p', { class: 'secondary k-lead' });
  const tabs = h('nav', { class: 'ds-tabs', 'aria-label': t('alerts.sl.views') });
  const body = h('div');

  mount(
    root,
    pageHead({
      title: t('alerts.sl.title'),
      crumbs: [{ text: t('nav.section.alerts'), href: '/alerts/events' }, { text: t('alerts.sl.title') }],
      actions: canWrite
        ? [
            button({ text: t('alerts.sl.newSilence'), iconName: 'bell-off', attrs: { id: 'sl-new-silence' }, onClick: (ev) => openForm(null, { kind: 'silence' }, ev.currentTarget) }),
            button({ text: t('alerts.sl.newMaint'), variant: 'primary', iconName: 'plus', attrs: { id: 'sl-new-maint' }, onClick: (ev) => openForm(null, { kind: 'maintenance' }, ev.currentTarget) }),
          ]
        : null,
    }),
    canWrite ? null : inlineAlert({ kind: 'info', title: t('alerts.readOnly'), body: t('alerts.sl.readOnlyBody') }),
    summary,
    tabs,
    body,
    h('p', { class: 'caption' }, t('alerts.sl.footnote')),
  );

  function renderTabs() {
    mount(
      tabs,
      STATUSES.map((s) =>
        h(
          'a',
          {
            href: `/alerts/silences${qs({ status: s === 'active' ? null : s })}`,
            'aria-current': s === status ? 'page' : null,
            on: {
              click: (ev) => {
                ev.preventDefault();
                status = s;
                replaceQuery({ status: s === 'active' ? null : s });
                renderTabs();
                load();
                const a = tabs.querySelector('[aria-current="page"]');
                if (a) a.focus();
              },
            },
          },
          t(`alerts.sl.status.${s}`),
        ),
      ),
    );
  }

  async function loadSummary() {
    try {
      const s = await api('/api/v1/alerts/summary');
      if (disposed) return;
      summary.textContent = s.silences.active
        ? t('alerts.sl.summary', { n: fmt.num(s.silences.active), hosts: fmt.num(s.silences.hosts) })
        : t('alerts.sl.summaryNone');
      summary.appendChild(document.createTextNode(` ${t('alerts.sl.summaryNote')}`));
    } catch {
      summary.textContent = t('alerts.sl.summaryNote');
    }
  }

  async function load() {
    mount(body, h('div', { class: 'ds-skeleton', 'aria-busy': 'true', style: { height: '160px' } }, h('span', { class: 'ds-visually-hidden' }, t('common.loading'))));
    try {
      list = await api(`/api/v1/silences${qs({ status })}`);
      if (disposed) return;
      draw();
    } catch (err) {
      if (disposed) return;
      mount(body, err.code === 'alerts_unavailable' ? unavailableNotice() : null, errorState(err, { title: t('alerts.sl.failed'), onRetry: load }));
    }
  }

  function draw() {
    if (!list.length) {
      mount(
        body,
        emptyState({
          title: t(`alerts.sl.empty.${status}`),
          body: status === 'ended' ? null : t('alerts.sl.emptyBody'),
          iconName: 'bell-off',
          actions: canWrite && status !== 'ended' ? [button({ text: t('alerts.sl.newMaint'), variant: 'primary', onClick: (ev) => openForm(null, { kind: 'maintenance' }, ev.currentTarget) })] : null,
        }),
      );
      return;
    }
    mount(
      body,
      h(
        'div',
        { class: 'ds-card ds-card--flush' },
        h(
          'div',
          { class: 'ds-table-wrap' },
          h(
            'table',
            { class: 'ds-table ds-table--wrap' },
            h('caption', { class: 'ds-visually-hidden' }, t('alerts.sl.caption', { s: t(`alerts.sl.status.${status}`) })),
            h(
              'thead',
              null,
              h(
                'tr',
                null,
                h('th', { scope: 'col' }, t('alerts.sl.col.kind')),
                h('th', { scope: 'col' }, t('alerts.sl.col.name')),
                h('th', { scope: 'col' }, t('alerts.sl.col.scope')),
                h('th', { scope: 'col' }, t('alerts.sl.col.time', { tz: fmt.tzLabel() })),
                h('th', { scope: 'col' }, t('alerts.sl.col.by')),
                canWrite ? h('th', { scope: 'col' }, h('span', { class: 'ds-visually-hidden' }, t('alerts.col.actions'))) : null,
              ),
            ),
            h(
              'tbody',
              null,
              list.map((s) =>
                h(
                  'tr',
                  { dataset: { key: s.silence_id } },
                  h('td', null, kindTag(s)),
                  h(
                    'td',
                    null,
                    h('a', { href: `/alerts/silences/${encodeURIComponent(s.silence_id)}`, dataset: { sl: s.silence_id }, on: { click: (ev) => { ev.preventDefault(); openForm(s, {}, ev.currentTarget); } } }, s.name),
                    h('span', { class: 'k-sub' }, s.reason),
                  ),
                  h('td', null, s.scope_short || t('alerts.scope.all'), s.matching_hosts != null ? h('span', { class: 'k-sub' }, t('alerts.sl.hostsN', { n: fmt.num(s.matching_hosts) })) : null),
                  h('td', null, timeText(s)),
                  h('td', null, s.created_by, s.ended_by ? h('span', { class: 'k-sub' }, t('alerts.sl.endedBy', { who: s.ended_by })) : null),
                  canWrite
                    ? h(
                        'td',
                        null,
                        s.status !== 'ended'
                          ? button({ text: s.status === 'upcoming' ? t('alerts.sl.cancel') : t('alerts.sl.end'), size: 'sm', attrs: { 'aria-label': t('alerts.sl.endAria', { verb: s.status === 'upcoming' ? t('alerts.sl.cancel') : t('alerts.sl.end'), name: s.name }) }, onClick: (ev) => endSilence(s, ev.currentTarget) })
                          : null,
                      )
                    : null,
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }

  /** 提前結束（或取消排程）：L1，原因選填 */
  function endSilence(s, trigger) {
    const verb = s.status === 'upcoming' ? t('alerts.sl.cancel') : t('alerts.sl.end');
    const reason = input({ name: 'reason', maxlength: 500 });
    const f = field({ label: t('alerts.sl.endReason'), input: reason });
    const ok = button({ text: verb, variant: 'primary' });
    const cancel = button({ text: t('common.cancel'), onClick: () => d.close() });
    const d = openDialog({
      title: t('alerts.sl.endTitle', { verb, name: s.name }),
      size: 'sm',
      body: h('div', null, h('p', null, s.status === 'upcoming' ? t('alerts.sl.cancelBody') : t('alerts.sl.endBody'), s.status === 'active' && s.notify_on_end ? ` ${t('alerts.sl.endSummary')}` : ''), f),
      actions: [cancel, ok],
      initialFocus: cancel,
    });
    ok.addEventListener('click', async () => {
      setBusy(ok, true, t('common.working'));
      try {
        await api(`/api/v1/silences/${encodeURIComponent(s.silence_id)}/end`, { method: 'POST', body: { reason: reason.value.trim() || undefined } });
        d.close();
        toast({ kind: 'success', title: t('alerts.sl.ended', { verb, name: s.name }) });
        await Promise.all([load(), loadSummary()]);
        const tl = document.getElementById('page-title');
        if (tl) tl.focus();
      } catch (err) {
        setBusy(ok, false, true);
        f.setError(describe(err, { title: t('alerts.sl.endFailed') }).title);
      }
    });
  }

  async function ensureRules() {
    if (rules || !session.can('alert-rules:read')) return rules;
    try {
      rules = await api('/api/v1/alert-rules');
    } catch {
      rules = null;
    }
    return rules;
  }

  /**
   * 建立／修改／檢視的表單（對話框）。
   * @param {object|null} s  既有的靜音；null 為建立
   * @param {object} pre     預帶：kind、hosts（[{agent_id,name}]）、subnets、rules、name、from_event、emergency
   */
  async function openForm(s, pre, trigger, { onClosed } = {}) {
    const isNew = !s;
    const ended = s && s.status === 'ended';
    const ro = !canWrite || ended;
    await ensureRules();
    let kind = s ? s.kind : pre.kind || 'maintenance';
    const scope = s ? s.scope : { tags: [], subnets: pre.subnets || [], hosts: (pre.hosts || []).map((x) => x.agent_id) };
    const selectedRules = new Set(s ? s.rules : pre.rules || []);
    // 不可靜音的系統規則（API 的 unsilenceable）：不能勾選；從事件帶進來的預選也拿掉（伺服器回 400 rule_unsilenceable）
    const unsilenceable = new Set((rules || []).filter((r) => r.unsilenceable).map((r) => r.rule_id));
    if (isNew) for (const id of unsilenceable) selectedRules.delete(id);
    let preview = null;

    const nameIn = input({ name: 'name', value: s ? s.name : pre.name || '', maxlength: 100 });
    nameIn.disabled = ro;
    const nameF = field({ label: t('alerts.sl.f.name'), input: nameIn, required: true });
    const kindName = uid('k');
    const kindFs = h(
      'fieldset',
      { disabled: ro || !isNew },
      h('legend', null, t('alerts.sl.f.kind')),
      h(
        'div',
        { class: 'ds-radio-group' },
        ['maintenance', 'silence'].map((k) =>
          h('label', { class: 'ds-radio' }, h('input', { type: 'radio', name: kindName, value: k, checked: k === kind, on: { change: () => { kind = k; drawTime(); refresh(); } } }), t(`alerts.sl.f.kind.${k}`)),
        ),
      ),
    );

    // 範圍
    const tagsIn = input({ name: 'tags', value: (scope.tags || []).join(', ') });
    const subnetsIn = input({ name: 'subnets', value: (scope.subnets || []).join(', '), mono: true });
    tagsIn.disabled = ro;
    subnetsIn.disabled = ro;
    const tagsF = field({ label: t('alerts.scope.tags'), input: tagsIn, hint: t('alerts.scope.listHint') });
    const subnetsF = field({ label: t('alerts.scope.subnets'), input: subnetsIn, hint: t('alerts.scope.subnetHint') });
    const picker = hostPicker({ label: t('alerts.scope.hosts'), hint: t('alerts.scope.hostsHint'), disabled: ro, initial: pre.hosts || (scope.hosts || []).map((a) => ({ agent_id: a, name: a })) });
    if (s && (scope.hosts || []).length) {
      Promise.all(scope.hosts.map((a) => api(`/api/v1/hosts/${encodeURIComponent(a)}`).then((x) => ({ agent_id: a, name: x.display_name || x.hostname }), () => ({ agent_id: a, name: a })))).then((l) => picker.set(l));
    }
    const rulesId = uid('rl');
    const rulesBox = rules
      ? h(
          'div',
          { class: 'k-check-columns', role: 'group', 'aria-labelledby': rulesId },
          rules.map((r) =>
            h(
              'label',
              { class: 'ds-checkbox' },
              h('input', { type: 'checkbox', name: 'rules', value: r.rule_id, checked: selectedRules.has(r.rule_id), disabled: ro || (r.unsilenceable && !selectedRules.has(r.rule_id)), on: { change: (ev) => { if (ev.target.checked) selectedRules.add(r.rule_id); else selectedRules.delete(r.rule_id); refresh(); } } }),
              r.name,
              r.unsilenceable ? h('span', { class: 'caption' }, t('alerts.sl.f.ruleUnsilenceable')) : null,
              r.severity === 'emergency' || r.max_severity === 'emergency' ? h('span', { class: 'caption' }, t('alerts.sl.f.ruleEmergency')) : null,
              r.category === 'system' ? h('span', { class: 'caption' }, t('alerts.sl.f.ruleSystem')) : null,
            ),
          ),
        )
      : h('p', { class: 'secondary' }, selectedRules.size ? [...selectedRules].join('、') : t('alerts.sl.f.allRules'));
    const impact = h('div', { class: 'k-impact-box', 'aria-live': 'polite' });

    // 時間
    const timeBox = h('div');
    const quickHint = h('p', { class: 'ds-field__hint', id: uid('qh') });
    const endIn = h('input', { class: 'ds-input', type: 'datetime-local', name: 'ends_at', value: toLocalInput(s ? s.ends_at : fmt.serverNow() + HOUR * 1000) });
    const startIn = h('input', { class: 'ds-input', type: 'datetime-local', name: 'starts_at', value: toLocalInput(s ? s.starts_at : fmt.serverNow() + 86400 * 1000 - (fmt.serverNow() % 3600000)) });
    const maintEnd = h('input', { class: 'ds-input', type: 'datetime-local', name: 'maint_ends_at', value: toLocalInput(s ? s.ends_at : fmt.serverNow() + 86400 * 1000 - (fmt.serverNow() % 3600000) + 4 * HOUR * 1000) });
    for (const el of [endIn, startIn, maintEnd]) el.disabled = ro;
    const endF = field({ label: t('alerts.sl.f.end', { tz: fmt.tzLabel() }), input: endIn });
    const startF = field({ label: t('alerts.sl.f.start', { tz: fmt.tzLabel() }), input: startIn });
    const maintEndF = field({ label: t('alerts.sl.f.end', { tz: fmt.tzLabel() }), input: maintEnd });
    const quickName = uid('q');
    const QUICK = [
      ['30m', t('alerts.sl.q.30m'), 30 * MIN],
      ['1h', t('alerts.sl.q.1h'), HOUR],
      ['4h', t('alerts.sl.q.4h'), 4 * HOUR],
      ['tomorrow', t('alerts.sl.q.tomorrow'), untilTomorrow8()],
      ['custom', t('alerts.sl.q.custom'), null],
    ];
    let quick = isNew ? '1h' : 'custom';
    const quickInputs = [];
    function drawTime() {
      quickInputs.length = 0;
      if (kind === 'silence') {
        mount(
          timeBox,
          h(
            'fieldset',
            { disabled: ro },
            h('legend', null, t('alerts.sl.f.duration')),
            isNew
              ? h(
                  'div',
                  { class: 'ds-radio-group', role: 'radiogroup', 'aria-describedby': quickHint.id },
                  QUICK.map(([v, label, secs]) => {
                    const inp = h('input', { type: 'radio', name: quickName, value: v, checked: v === quick, dataset: { secs: secs == null ? '' : String(secs) } });
                    inp.addEventListener('click', (ev) => {
                      if (inp.getAttribute('aria-disabled') === 'true') {
                        ev.preventDefault();
                        announce(t('alerts.sl.q.tooLong', { max: preview && preview.limit ? preview.limit.max_text : '' }));
                        return;
                      }
                      quick = v;
                      endF.hidden = v !== 'custom';
                    });
                    quickInputs.push(inp);
                    return h('label', { class: 'ds-radio' }, inp, label);
                  }),
                )
              : null,
            quickHint,
            endF,
          ),
        );
        endF.hidden = isNew && quick !== 'custom';
      } else {
        mount(timeBox, h('fieldset', { disabled: ro }, h('legend', null, t('alerts.sl.f.window')), h('div', { class: 'ds-form-row' }, startF, maintEndF), h('p', { class: 'ds-field__hint' }, t('alerts.sl.f.maintHint'))));
      }
      applyLimit();
    }
    function applyLimit() {
      if (kind !== 'silence') {
        quickHint.textContent = '';
        return;
      }
      const lim = preview && preview.limit;
      quickHint.textContent = lim ? t('alerts.sl.limit', { max: lim.max_text || fmt.duration(lim.max_secs), reason: lim.reason || '' }) : '';
      for (const inp of quickInputs) {
        const secs = Number(inp.dataset.secs || 0);
        const over = lim && secs && secs > lim.max_secs;
        if (over) {
          inp.setAttribute('aria-disabled', 'true');
          inp.closest('label').classList.add('is-disabled');
          if (inp.checked) {
            const fallback = quickInputs.find((x) => x.value === '1h' && x.getAttribute('aria-disabled') !== 'true') || quickInputs.find((x) => x.value === '30m');
            if (fallback) {
              fallback.checked = true;
              quick = fallback.value;
            }
          }
        } else {
          inp.removeAttribute('aria-disabled');
          inp.closest('label').classList.remove('is-disabled');
        }
      }
    }

    // 期間與結束
    const notifyEnd = h('input', { type: 'checkbox', name: 'notify_on_end', checked: s ? s.notify_on_end : true, disabled: ro });
    const excludeUptime = h('input', { type: 'checkbox', name: 'exclude_uptime', checked: s ? s.exclude_uptime : false, disabled: ro });
    const reasonIn = input({ name: 'reason', value: s ? s.reason : '', maxlength: 500 });
    reasonIn.disabled = ro;
    const reasonF = field({ label: t('alerts.sl.f.reason'), input: reasonIn, required: true, hint: t('alerts.sl.f.reasonHint') });
    const errBox = h('div');

    const readScope = () => ({ tags: splitList(tagsIn.value), subnets: splitList(subnetsIn.value), hosts: picker.get() });
    let timer = null;
    let pctrl = null;
    async function refresh() {
      clearTimeout(timer);
      timer = setTimeout(async () => {
        if (pctrl) pctrl.abort();
        pctrl = new AbortController();
        subnetsF.setError('');
        try {
          preview = await api('/api/v1/silences/preview', {
            method: 'POST',
            body: { kind, scope: readScope(), rules: [...selectedRules], from_event: pre.from_event || (s && s.from_event) || undefined },
            signal: pctrl.signal,
          });
          mount(
            impact,
            h('p', null, h('strong', { dataset: { matching: preview.matching_hosts } }, t('alerts.scope.matching', { n: fmt.num(preview.matching_hosts) }))),
            h('p', { class: 'caption' }, preview.scope_text),
            h('p', { class: 'caption' }, preview.rules_text),
          );
          applyLimit();
        } catch (err) {
          if (err.name === 'AbortError') return;
          preview = null;
          if (err.code === 'invalid_parameter' && err.details && String(err.details.parameter).startsWith('scope.subnets')) subnetsF.setError(err.message);
          mount(impact, h('p', { class: 'muted' }, err.code === 'invalid_parameter' ? err.message : t('alerts.scope.unknown')));
        }
      }, 300);
    }
    tagsIn.addEventListener('input', refresh);
    subnetsIn.addEventListener('input', refresh);
    picker.onChange(refresh);

    const form = h(
      'form',
      { novalidate: true, class: 'k-stack', id: uid('slf') },
      ended ? inlineAlert({ kind: 'info', title: t('alerts.sl.endedNote') }) : !canWrite ? inlineAlert({ kind: 'info', title: t('alerts.readOnly'), body: t('alerts.sl.readOnlyBody') }) : null,
      pre.emergency ? inlineAlert({ kind: 'warning', title: t('alerts.sl.fromEmergency'), body: t('alerts.sl.fromEmergencyBody') }) : null,
      nameF,
      kindFs,
      h(
        'fieldset',
        null,
        h('legend', null, t('alerts.sl.f.scope')),
        h('p', { class: 'ds-field__hint' }, t('alerts.scope.combine')),
        h('div', { class: 'ds-form-row' }, tagsF, subnetsF),
        picker.el,
        h('p', { class: 'ds-field__label', id: rulesId }, t('alerts.sl.f.rules')),
        rulesBox,
        impact,
      ),
      timeBox,
      h(
        'fieldset',
        null,
        h('legend', null, t('alerts.sl.f.behavior')),
        h('p', { class: 'caption' }, t('alerts.sl.f.behaviorFixed')),
        h('label', { class: 'ds-checkbox' }, notifyEnd, t('alerts.sl.f.notifyEnd')),
        h('label', { class: 'ds-checkbox' }, excludeUptime, t('alerts.sl.f.excludeUptime')),
        h('p', { class: 'ds-field__hint' }, t('alerts.sl.f.excludeUptimeHint')),
      ),
      reasonF,
      errBox,
    );
    const cancelBtn = button({ text: ro ? t('common.close') : t('common.cancel'), onClick: () => d.close() });
    const d = openDialog({
      title: isNew ? t(`alerts.sl.newTitle.${kind}`) : s.name,
      size: 'lg',
      body: form,
      actions: ro ? [cancelBtn] : [cancelBtn, h('button', { type: 'submit', class: 'ds-button ds-button--primary', form: form.id }, isNew ? t('alerts.sl.create') : t('alerts.sl.save'))],
      initialFocus: ro ? cancelBtn : nameIn,
      onClose: () => {
        if (onClosed) onClosed();
      },
    });
    const submit = d.el.querySelector('button[type="submit"]');
    drawTime();
    refresh();

    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      if (ro) return;
      mount(errBox);
      nameF.setError('');
      reasonF.setError('');
      endF.setError('');
      maintEndF.setError('');
      if (!nameIn.value.trim()) {
        nameF.setError(t('alerts.sl.f.nameRequired'));
        nameIn.focus();
        return;
      }
      if (!reasonIn.value.trim()) {
        reasonF.setError(t('alerts.sl.f.reasonRequired'));
        reasonIn.focus();
        return;
      }
      const body = {
        name: nameIn.value.trim(),
        reason: reasonIn.value.trim(),
        scope: readScope(),
        rules: [...selectedRules],
        notify_on_end: notifyEnd.checked,
        exclude_uptime: excludeUptime.checked,
      };
      if (isNew) body.kind = kind;
      if (isNew && pre.from_event) body.from_event = pre.from_event;
      if (kind === 'silence') {
        const sel = quickInputs.find((x) => x.checked);
        if (isNew && sel && sel.value !== 'custom') {
          body.duration_secs = Number(sel.dataset.secs);
        } else {
          const ms = fromLocalInput(endIn.value);
          if (ms == null) {
            endF.setError(t('alerts.sl.f.timeRequired'));
            endIn.focus();
            return;
          }
          body.ends_at = new Date(ms).toISOString();
        }
      } else {
        const a = fromLocalInput(startIn.value);
        const b = fromLocalInput(maintEnd.value);
        if (a == null || b == null) {
          (a == null ? startF : maintEndF).setError(t('alerts.sl.f.timeRequired'));
          (a == null ? startIn : maintEnd).focus();
          return;
        }
        body.starts_at = new Date(a).toISOString();
        body.ends_at = new Date(b).toISOString();
      }
      // L1：範圍很大、所有規則＋全部主機、或包含緊急等級（伺服器的 preview 決定）
      if (preview && preview.confirm && preview.confirm.required) {
        const ok = await confirm({
          title: t('alerts.sl.confirmTitle'),
          body: h('div', null, h('ul', null, preview.confirm.reasons.map((x) => h('li', null, x))), h('p', null, t('alerts.sl.confirmBody', { n: fmt.num(preview.matching_hosts) }))),
          confirmText: isNew ? t('alerts.sl.create') : t('alerts.sl.save'),
          danger: true,
        });
        if (!ok) return;
      }
      setBusy(submit, true, t('common.working'));
      try {
        const saved = isNew
          ? await api('/api/v1/silences', { method: 'POST', body })
          : await api(`/api/v1/silences/${encodeURIComponent(s.silence_id)}`, { method: 'PUT', body });
        d.close();
        toast({ kind: 'success', title: t(isNew ? 'alerts.sl.created' : 'alerts.sl.saved', { name: saved.name }), desc: t('alerts.sl.createdDesc', { n: fmt.num(saved.matching_hosts || 0) }) });
        status = saved.status === 'upcoming' ? 'upcoming' : 'active';
        replaceQuery({ status: status === 'active' ? null : status });
        renderTabs();
        await Promise.all([load(), loadSummary()]);
        const link = root.querySelector(`[data-sl="${CSS.escape(saved.silence_id)}"]`);
        if (link) link.focus();
      } catch (err) {
        setBusy(submit, false, true);
        const param = err.details && err.details.parameter;
        if (err.code === 'silence_too_long') {
          const f = kind === 'silence' ? endF : maintEndF;
          if (kind === 'silence' && isNew) {
            const custom = quickInputs.find((x) => x.value === 'custom');
            if (custom) {
              custom.checked = true;
              quick = 'custom';
              endF.hidden = false;
            }
          }
          f.setError(t('alerts.sl.tooLong', { max: fmt.duration(err.details.max_secs), msg: err.message }));
          (kind === 'silence' ? endIn : maintEnd).focus();
          return;
        }
        if (param === 'name') {
          nameF.setError(err.message);
          nameIn.focus();
          return;
        }
        if (param === 'reason') {
          reasonF.setError(err.message);
          reasonIn.focus();
          return;
        }
        if (param === 'ends_at' || param === 'starts_at') {
          const f = param === 'starts_at' ? startF : kind === 'silence' ? endF : maintEndF;
          f.setError(err.message);
          return;
        }
        if (param && String(param).startsWith('scope.subnets')) {
          subnetsF.setError(err.message);
          subnetsIn.focus();
          return;
        }
        const dsc = describe(err, { title: t('alerts.sl.saveFailed') });
        mount(errBox, inlineAlert({ kind: 'error', title: dsc.title, body: err.code === 'invalid_parameter' ? err.message : dsc.reason, code: dsc.code }));
        errBox.firstChild.setAttribute('tabindex', '-1');
        errBox.firstChild.focus();
      }
    });
    return d;
  }

  // ---- 深層連結：/alerts/silences/new、/alerts/silences/:id ----
  async function deepLink() {
    const id = ctx.params.id;
    if (!id) return;
    const reset = () => history.replaceState(null, '', `/alerts/silences${qs({ status: status === 'active' ? null : status })}`);
    if (id === 'new') {
      if (!canWrite) {
        reset();
        toast({ kind: 'warning', title: t('alerts.sl.noWrite') });
        return;
      }
      const q = ctx.query;
      const pre = { kind: q.get('type') === 'silence' ? 'silence' : 'maintenance', rules: splitList(q.get('rules')), hosts: [] };
      const hostIds = splitList(q.get('hosts'));
      const evId = q.get('event');
      if (evId) {
        try {
          const ev = await api(`/api/v1/alerts/${encodeURIComponent(evId)}`);
          pre.from_event = ev.event_id;
          pre.name = t('alerts.sl.fromEventName', { title: ev.title });
          pre.emergency = ev.severity === 'emergency';
          if (!pre.rules.length) pre.rules = [ev.rule_id];
          if (ev.agent_id && !hostIds.includes(ev.agent_id)) hostIds.push(ev.agent_id);
          if (ev.category === 'aggregate' && ev.details && ev.details.subnet) pre.subnets = [ev.details.subnet];
        } catch {
          /* 事件不存在：只預帶其他參數 */
        }
      }
      pre.hosts = await Promise.all(hostIds.map((a) => api(`/api/v1/hosts/${encodeURIComponent(a)}`).then((x) => ({ agent_id: a, name: x.display_name || x.hostname }), () => ({ agent_id: a, name: a }))));
      if (disposed) return;
      openForm(null, pre, null, { onClosed: reset });
      return;
    }
    try {
      const s = await api(`/api/v1/silences/${encodeURIComponent(id)}`);
      if (!disposed) openForm(s, {}, null, { onClosed: reset });
    } catch {
      reset();
      toast({ kind: 'warning', title: t('alerts.sl.notFound', { id }) });
    }
  }

  renderTabs();
  loadSummary();
  load();
  deepLink();
  return {
    dispose() {
      disposed = true;
    },
  };
}
