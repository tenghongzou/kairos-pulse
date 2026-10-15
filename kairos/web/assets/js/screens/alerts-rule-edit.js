// S09 告警規則編輯（設計包 kairos/screens.md S09；API POST/PUT /api/v1/alert-rules、POST /alert-rules/preview、
// POST /silences/preview 的 matching_hosts 作為「符合條件：N 台」）。
//
// - 路由 /alerts/rules/new（?copy=<rule_id> 複製）與 /alerts/rules/:ruleId。
// - 自訂規則的類型：主機離線、主機長期失聯、回報延遲、指標門檻（CPU、記憶體）、身分衝突；「憑證即將到期」「版本過舊」
//   延後，類型選單不列（decisions/INTEGRATION.md §28.6）。
// - 系統預設規則：維運者可改嚴重度（系統類不可低於預設）、通知管道、通知設定與說明；條件只有管理員
//   （alert-rules:admin）能改，其他角色以停用＋說明呈現（screens.md §0.2「部分權限」）。稽核員以唯讀開啟。
// - 歷史試算只支援指標門檻與回報延遲；主機離線類回 supported: false，預覽區寫「目前無法以歷史資料試算」，不顯示 0 次。
// - 範圍不支援群組（隱藏）；範圍為 0 台時不能儲存。

import { h, icon, uid, mount, announce } from '../core/dom.js';
import { t, has, severityLabel } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { navigate } from '../core/router.js';
import { pageHead, card, emptyState, errorState, inlineAlert, field, input, button, setBusy, skeletonBlock, codeTag } from '../ui/components.js';
import { confirm } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { describe } from '../core/errors.js';
import { SEVERITIES, USER_KINDS, kindLabel, hostPicker, splitList, unavailableNotice } from './alerts-common.js';

/** 嚴重度的預設通知（ALERTS.md 第 6 節；model.rs NotifyPolicy::effective） */
const NOTIFY_DEFAULTS = {
  emergency: { repeat: 15, max: 8, recovery: true, escalate: 15 },
  critical: { repeat: 60, max: 3, recovery: true, escalate: 60 },
  warning: { repeat: null, max: 0, recovery: false, escalate: null },
  info: { repeat: null, max: 0, recovery: false, escalate: null },
};

/** 每種規則的條件欄位：[參數, 單位換算（秒→顯示單位）, 標籤 key, 範圍] */
const PARAMS = {
  host_offline: [{ key: 'grace_secs', unit: 60, label: 'alerts.edit.p.grace', def: 180, min: 0, max: 86400 }],
  host_long_offline: [{ key: 'after_secs', unit: 3600, label: 'alerts.edit.p.after', def: 86400, min: 3600, max: 90 * 86400 }],
  report_stale: [{ key: 'factor', unit: 1, label: 'alerts.edit.p.factor', def: 3, min: 2, max: 100 }],
  identity_conflict: [
    { key: 'window_secs', unit: 60, label: 'alerts.edit.p.window', def: 3600, min: 60, max: 86400 },
    { key: 'min_count', unit: 1, label: 'alerts.edit.p.minCount', def: 5, min: 2, max: 1000 },
    { key: 'min_sources', unit: 1, label: 'alerts.edit.p.minSources', def: 2, min: 1, max: 100 },
  ],
};

/** 指標門檻的指標（設計包 S09「指標門檻的新指標與範圍」、§30 D48；API.md、ALERTS.md 第 5.3 節） */
const METRICS = ['cpu', 'memory', 'disk', 'swap', 'load', 'network_rx', 'network_tx'];
const metricMax = (m) => (m === 'network_rx' || m === 'network_tx' ? 1000000 : 100);
const metricUnit = (m) => (m === 'network_rx' || m === 'network_tx' ? t('alerts.unit.mbps') : m === 'load' ? t('alerts.unit.load') : '%');
const scopeKey = (m) => (m === 'disk' ? 'mountpoints' : m === 'network_rx' || m === 'network_tx' ? 'devices' : null);

/** S08「從範本新增」（alerts-ux.md 2.2）：/alerts/rules/new?template= */
export const TEMPLATES = {
  cpu90: { name: 'alerts.template.name.cpu90', severity: 'warning', params: { metric: 'cpu', op: '>', threshold: 90, for_secs: 600, clear_threshold: null, clear_for_secs: 0 } },
  disk90: { name: 'alerts.template.name.disk90', severity: 'warning', params: { metric: 'disk', op: '>', threshold: 90, for_secs: 600, clear_threshold: 85, clear_for_secs: 0 } },
  disk95: { name: 'alerts.template.name.disk95', severity: 'critical', params: { metric: 'disk', op: '>', threshold: 95, for_secs: 300, clear_threshold: null, clear_for_secs: 0 } },
};

function numInput(name, value, { step = 'any', min, max, disabled } = {}) {
  const i = h('input', { class: 'ds-input ds-input--num', type: 'number', name, value: value == null ? '' : String(value), step, min, max, inputmode: 'decimal' });
  i.disabled = Boolean(disabled);
  return i;
}

export function render(ctx) {
  const root = ctx.root;
  const isNew = ctx.params.id === 'new';
  const ruleId = isNew ? null : ctx.params.id;
  const canWrite = session.can('alert-rules:write');
  const canAdmin = session.can('alert-rules:admin');
  let disposed = false;
  let dirty = false;
  let base = null;
  let channels = null;
  const crumbs = (name) => [{ text: t('nav.section.alerts'), href: '/alerts/events' }, { text: t('alerts.rules.title'), href: '/alerts/rules' }, { text: name }];

  if (isNew && !canWrite) {
    mount(root, pageHead({ title: t('alerts.edit.newTitle'), crumbs: crumbs(t('alerts.edit.newTitle')) }), emptyState({ title: t('error.403.title'), body: t('alerts.edit.noWrite'), iconName: 'lock' }));
    return {};
  }
  mount(root, pageHead({ title: isNew ? t('alerts.edit.newTitle') : ruleId, crumbs: crumbs(isNew ? t('alerts.edit.newTitle') : ruleId) }), skeletonBlock(8));

  // 未儲存的變更：main.js 依 isDirty() 在站內換頁與關閉分頁時詢問

  async function load() {
    try {
      const copyId = isNew ? ctx.query.get('copy') : null;
      const [r, ch] = await Promise.all([
        ruleId || copyId ? api(`/api/v1/alert-rules/${encodeURIComponent(ruleId || copyId)}`) : Promise.resolve(null),
        session.can('channels:read') ? api('/api/v1/channels').catch(() => null) : Promise.resolve(null),
      ]);
      if (disposed) return;
      channels = ch;
      const tpl = isNew ? TEMPLATES[ctx.query.get('template')] : null;
      if (tpl) {
        base = { kind: 'metric_threshold', builtin: false, category: 'host', name: t(tpl.name), severity: tpl.severity, params: { ...tpl.params }, scope: { tags: [], subnets: [], hosts: [] }, channels: [], notify: { escalate_channels: [] }, description: '', enabled: true };
      }
      if (copyId && r) {
        base = { ...r, rule_id: null, builtin: false, name: t('alerts.edit.copyName', { name: r.name }), enabled: true };
      } else if (!tpl) {
        base = r;
      }
      draw();
    } catch (err) {
      if (disposed) return;
      if (err.code === 'rule_not_found') {
        mount(root, pageHead({ title: t('alerts.edit.notFound'), crumbs: crumbs(ruleId) }), emptyState({ title: t('alerts.edit.notFound'), body: t('alerts.edit.notFoundBody', { id: ruleId }), iconName: 'search', actions: [h('a', { class: 'ds-button ds-button--secondary', href: '/alerts/rules' }, t('alerts.edit.back'))] }));
        return;
      }
      mount(root, pageHead({ title: ruleId || t('alerts.edit.newTitle'), crumbs: crumbs(ruleId || '') }), err.code === 'alerts_unavailable' ? unavailableNotice() : null, errorState(err, { title: t('alerts.edit.failed'), onRetry: load }));
    }
  }

  function draw() {
    const r = base || {
      kind: 'metric_threshold',
      builtin: false,
      category: 'host',
      name: '',
      severity: 'warning',
      params: { metric: 'cpu', op: '>', threshold: 90, for_secs: 600, clear_threshold: null, clear_for_secs: 0 },
      scope: { tags: [], subnets: [], hosts: [] },
      channels: [],
      notify: { escalate_channels: [] },
      description: '',
      enabled: true,
    };
    const builtin = Boolean(r.builtin);
    const system = r.category === 'system';
    const readOnly = !canWrite;
    const paramsLocked = readOnly || (builtin && !canAdmin);
    let kind = r.kind;
    let severity = r.severity;
    const fields = {};
    const markDirty = () => {
      dirty = true;
    };

    // ---- 1 類型 ----
    const kindSection = h('section', { class: 'k-form-section', 'aria-labelledby': 'rs-kind' });
    if (isNew && !builtin) {
      const name = uid('kind');
      mount(
        kindSection,
        h('h2', { class: 'k-section-title', id: 'rs-kind' }, t('alerts.edit.s1')),
        h(
          'div',
          { class: 'ds-radio-group', role: 'radiogroup', 'aria-labelledby': 'rs-kind' },
          USER_KINDS.map((k) =>
            h(
              'label',
              { class: 'ds-radio' },
              h('input', { type: 'radio', name, value: k, checked: k === kind, on: { change: () => { kind = k; markDirty(); drawCondition(); schedulePreview(); } } }),
              kindLabel(k),
            ),
          ),
        ),
      );
    } else {
      mount(kindSection, h('h2', { class: 'k-section-title', id: 'rs-kind' }, t('alerts.edit.s1')), h('p', null, kindLabel(kind), builtin ? [' ', h('span', { class: 'caption' }, t('alerts.edit.builtinNote'))] : null));
    }

    // ---- 2 條件 ----
    const condBody = h('div');
    const condSection = h('section', { class: 'k-form-section', 'aria-labelledby': 'rs-cond' }, h('h2', { class: 'k-section-title', id: 'rs-cond' }, t('alerts.edit.s2')), condBody);
    const params = { ...(r.params || {}) };
    function drawCondition() {
      fields.params = {};
      const nodes = [];
      if (paramsLocked && !readOnly && builtin) nodes.push(h('p', { class: 'k-why-disabled caption' }, icon('lock'), t('alerts.edit.adminOnly')));
      if (kind === 'metric_threshold') {
        const p = { metric: 'cpu', op: '>', threshold: 90, for_secs: 600, clear_threshold: null, clear_for_secs: 0, ...params };
        const max = metricMax(p.metric);
        const unit = metricUnit(p.metric);
        const metric = h('select', { class: 'ds-select', name: 'metric', 'aria-label': t('alerts.edit.metric') }, METRICS.map((m) => h('option', { value: m, selected: p.metric === m }, t(`alerts.metric.${m}`))));
        const op = h('select', { class: 'ds-select', name: 'op', 'aria-label': t('alerts.edit.op') }, ['>', '>=', '<', '<='].map((o) => h('option', { value: o, selected: p.op === o }, t(`alerts.op.${o}`))));
        const thr = numInput('threshold', p.threshold, { min: 0, max });
        thr.setAttribute('aria-label', t('alerts.edit.threshold'));
        const forM = numInput('for_minutes', p.for_secs / 60, { min: 0, max: 1440 });
        forM.setAttribute('aria-label', t('alerts.edit.forMinutes'));
        const clr = numInput('clear_threshold', p.clear_threshold, { min: 0, max });
        clr.setAttribute('aria-label', t('alerts.edit.clearThreshold'));
        const clrFor = numInput('clear_for_minutes', p.clear_for_secs / 60, { min: 0, max: 1440 });
        clrFor.setAttribute('aria-label', t('alerts.edit.clearForMinutes'));
        const clrDir = h('span', { 'aria-hidden': 'true' });
        const setDir = () => {
          clrDir.textContent = op.value.startsWith('>') ? t('alerts.edit.below') : t('alerts.edit.above');
        };
        setDir();
        // 範圍：磁碟為掛載點、網路為網路介面（glob，最多 16 個；所有／只限指定）
        const sk = scopeKey(p.metric);
        const what = sk ? t(`alerts.edit.scope.${sk}`) : '';
        const scopeList = sk && Array.isArray(p[sk]) ? p[sk] : [];
        const scopeMode = sk
          ? h(
              'select',
              { class: 'ds-select', name: 'scope_mode', 'aria-label': what },
              h('option', { value: 'all', selected: !scopeList.length }, t('alerts.edit.scope.all', { what })),
              h('option', { value: 'some', selected: scopeList.length > 0 }, t('alerts.edit.scope.some', { what })),
            )
          : null;
        const scopeText = sk ? h('textarea', { class: 'ds-input k-mono-area', name: sk, rows: 3, spellcheck: 'false' }, scopeList.join('\n')) : null;
        const scopeField = sk ? field({ label: t('alerts.edit.scope.list', { what }), input: scopeText, hint: t('alerts.edit.scope.hint', { what }) }) : null;
        if (scopeField) scopeField.hidden = !scopeList.length;
        if (scopeMode) {
          scopeMode.addEventListener('change', () => {
            scopeField.hidden = scopeMode.value !== 'some';
          });
        }
        const readScopeList = () =>
          sk && scopeMode.value === 'some'
            ? scopeText.value
                .split('\n')
                .map((x) => x.trim())
                .filter(Boolean)
            : [];
        for (const el of [metric, op, thr, forM, clr, clrFor, scopeMode, scopeText].filter(Boolean)) {
          el.disabled = paramsLocked;
          el.addEventListener('input', () => { markDirty(); setDir(); schedulePreview(); });
          el.addEventListener('change', () => { markDirty(); setDir(); schedulePreview(); });
        }
        // 換指標：單位、範圍上限與範圍欄位跟著換（保留其他已填的值）
        metric.addEventListener('change', () => {
          Object.assign(params, fields.params.read(), { metric: metric.value });
          delete params.mountpoints;
          delete params.devices;
          drawCondition();
          const again = condBody.querySelector('select[name="metric"]');
          if (again) again.focus();
        });
        const noteKey = p.metric === 'network_rx' || p.metric === 'network_tx' ? 'net' : p.metric;
        const errThr = h('p', { class: 'ds-field__error', id: uid('err'), hidden: true });
        const errClr = h('p', { class: 'ds-field__error', id: uid('err'), hidden: true });
        thr.setAttribute('aria-describedby', errThr.id);
        clr.setAttribute('aria-describedby', errClr.id);
        nodes.push(
          h('div', { class: 'ds-sentence' }, t('alerts.edit.when'), metric, op, thr, unit, t('alerts.edit.for'), forM, t('alerts.edit.minutes')),
          errThr,
          has(`alerts.edit.metricNote.${noteKey}`) ? h('p', { class: 'ds-field__hint' }, t(`alerts.edit.metricNote.${noteKey}`)) : null,
          sk ? h('div', { class: 'ds-sentence' }, scopeMode) : null,
          scopeField,
          h('div', { class: 'ds-sentence' }, t('alerts.edit.clearWhen'), clrDir, clr, unit, t('alerts.edit.for'), clrFor, t('alerts.edit.minutes')),
          h('p', { class: 'ds-field__hint' }, t('alerts.edit.clearHint')),
          errClr,
        );
        fields.params = {
          read() {
            const out = { metric: metric.value, op: op.value, threshold: Number(thr.value), for_secs: Math.round(Number(forM.value || 0) * 60) };
            if (clr.value !== '') out.clear_threshold = Number(clr.value);
            out.clear_for_secs = Math.round(Number(clrFor.value || 0) * 60);
            const list = readScopeList();
            if (sk && list.length) out[sk] = list;
            return out;
          },
          validate() {
            const show = (el, msg) => {
              el.textContent = msg || '';
              el.hidden = !msg;
            };
            show(errThr, '');
            show(errClr, '');
            thr.removeAttribute('aria-invalid');
            clr.removeAttribute('aria-invalid');
            const v = Number(thr.value);
            if (thr.value === '' || Number.isNaN(v) || v < 0 || v > max) {
              show(errThr, max === 100 ? t('alerts.edit.thresholdRange') : t('alerts.edit.thresholdRangeN', { max: fmt.num(max) }));
              thr.setAttribute('aria-invalid', 'true');
              return thr;
            }
            if (sk && scopeMode.value === 'some') {
              const list = readScopeList();
              const msg = !list.length ? t('alerts.edit.scope.empty') : list.length > 16 ? t('alerts.edit.scope.tooMany') : '';
              scopeField.setError(msg);
              if (msg) return scopeText;
            }
            if (clr.value !== '') {
              const c = Number(clr.value);
              const above = op.value.startsWith('>');
              if (Number.isNaN(c) || c < 0 || c > max || (above && c > v) || (!above && c < v)) {
                show(errClr, above ? t('alerts.edit.clearAbove') : t('alerts.edit.clearBelow'));
                clr.setAttribute('aria-invalid', 'true');
                return clr;
              }
            }
            return null;
          },
          showError(param, msg) {
            if (sk && (param === 'params.mountpoints' || param === 'params.devices')) {
              scopeField.setError(msg);
              return scopeText;
            }
            const target = param === 'params.clear_threshold' || param === 'params.clear_for_secs' ? errClr : errThr;
            target.textContent = msg;
            target.hidden = false;
            (target === errClr ? clr : thr).setAttribute('aria-invalid', 'true');
            return target === errClr ? clr : thr;
          },
        };
      } else if (PARAMS[kind]) {
        const inputs = PARAMS[kind].map((d) => {
          const v = params[d.key] ?? d.def;
          const inp = numInput(d.key, Math.round((v / d.unit) * 100) / 100, { min: d.min / d.unit, max: d.max / d.unit });
          inp.disabled = paramsLocked;
          inp.addEventListener('input', () => { markDirty(); schedulePreview(); });
          const f = field({ label: t(d.label), input: inp, hint: t('alerts.edit.range', { min: fmt.num(d.min / d.unit), max: fmt.num(d.max / d.unit) }) });
          return { d, inp, f };
        });
        nodes.push(h('div', { class: 'ds-form-row' }, inputs.map((x) => x.f)));
        if (kind === 'host_offline') nodes.push(h('p', { class: 'ds-field__hint' }, t('alerts.edit.offlineHint')));
        fields.params = {
          read() {
            const out = {};
            for (const { d, inp } of inputs) out[d.key] = Math.round(Number(inp.value) * d.unit);
            return out;
          },
          validate() {
            for (const { d, inp, f } of inputs) {
              const v = Number(inp.value) * d.unit;
              if (inp.value === '' || Number.isNaN(v) || v < d.min || v > d.max) {
                f.setError(t('alerts.edit.range', { min: fmt.num(d.min / d.unit), max: fmt.num(d.max / d.unit) }));
                return inp;
              }
              f.setError('');
            }
            return null;
          },
          showError(param, msg) {
            const x = inputs.find((i) => `params.${i.d.key}` === param) || inputs[0];
            x.f.setError(msg);
            return x.inp;
          },
        };
      } else {
        // 系統預設規則：參數中的數值（只有管理員能改）
        const keys = Object.keys(params).filter((k) => typeof params[k] === 'number');
        const inputs = keys.map((k) => {
          const inp = numInput(k, params[k]);
          inp.disabled = paramsLocked;
          inp.addEventListener('input', markDirty);
          return { k, inp, f: field({ label: h('code', null, k), input: inp }) };
        });
        const others = Object.keys(params).filter((k) => typeof params[k] !== 'number');
        nodes.push(
          inputs.length ? h('div', { class: 'ds-form-row' }, inputs.map((x) => x.f)) : h('p', { class: 'secondary' }, t('alerts.edit.noParams')),
          others.length ? h('p', { class: 'caption' }, others.map((k) => [codeTag(k), `＝${JSON.stringify(params[k])} `])) : null,
        );
        fields.params = {
          read() {
            const out = {};
            for (const { k, inp } of inputs) if (Number(inp.value) !== params[k]) out[k] = Number(inp.value);
            return out;
          },
          validate: () => null,
          showError(param, msg) {
            const x = inputs.find((i) => `params.${i.k}` === param) || inputs[0];
            if (x) x.f.setError(msg);
            return x ? x.inp : null;
          },
        };
      }
      mount(condBody, ...nodes);
    }
    drawCondition();

    // ---- 3 範圍 ----
    const scope = r.scope || { tags: [], subnets: [], hosts: [] };
    const tagsIn = input({ name: 'tags', value: (scope.tags || []).join(', ') });
    const subnetsIn = input({ name: 'subnets', value: (scope.subnets || []).join(', '), mono: true });
    tagsIn.disabled = readOnly;
    subnetsIn.disabled = readOnly;
    const tagsF = field({ label: t('alerts.scope.tags'), input: tagsIn, hint: t('alerts.scope.listHint') });
    const subnetsF = field({ label: t('alerts.scope.subnets'), input: subnetsIn, hint: t('alerts.scope.subnetHint') });
    const picker = hostPicker({ label: t('alerts.scope.hosts'), hint: t('alerts.scope.hostsHint'), disabled: readOnly, initial: (scope.hosts || []).map((a) => ({ agent_id: a, name: a })) });
    // 已選的主機：以名稱顯示
    if ((scope.hosts || []).length) {
      Promise.all(scope.hosts.map((a) => api(`/api/v1/hosts/${encodeURIComponent(a)}`).then((x) => ({ agent_id: a, name: x.display_name || x.hostname }), () => ({ agent_id: a, name: a })))).then((list) => {
        if (!disposed) picker.set(list);
      });
    }
    const impact = h('div', { class: 'k-impact-box', 'aria-live': 'polite' });
    let matching = null;
    const scopeSection = system
      ? null
      : h(
          'section',
          { class: 'k-form-section', 'aria-labelledby': 'rs-scope' },
          h('h2', { class: 'k-section-title', id: 'rs-scope' }, t('alerts.edit.s3')),
          h('p', { class: 'ds-field__hint' }, t('alerts.scope.combine')),
          h('div', { class: 'ds-form-row' }, tagsF, subnetsF),
          picker.el,
          impact,
        );
    const readScope = () => ({ tags: splitList(tagsIn.value), subnets: splitList(subnetsIn.value), hosts: picker.get() });
    let scopeTimer = null;
    let scopeCtrl = null;
    async function refreshScope() {
      if (system) return;
      if (scopeCtrl) scopeCtrl.abort();
      scopeCtrl = new AbortController();
      subnetsF.setError('');
      try {
        const p = await api('/api/v1/silences/preview', { method: 'POST', body: { kind: 'silence', scope: readScope() }, signal: scopeCtrl.signal });
        matching = p.matching_hosts;
        mount(impact, h('p', null, h('strong', null, t('alerts.scope.matching', { n: fmt.num(p.matching_hosts) }))), h('p', { class: 'caption' }, p.scope_text));
        if (!p.matching_hosts) impact.appendChild(h('p', { class: 'ds-field__error' }, icon('status-critical'), t('alerts.edit.scopeZero')));
      } catch (err) {
        if (err.name === 'AbortError') return;
        matching = null;
        if (err.code === 'invalid_parameter' && err.details && String(err.details.parameter).startsWith('scope.subnets')) subnetsF.setError(err.message);
        mount(impact, h('p', { class: 'muted' }, err.code === 'invalid_parameter' ? err.message : t('alerts.scope.unknown')));
      }
    }
    const scheduleScope = () => {
      markDirty();
      clearTimeout(scopeTimer);
      scopeTimer = setTimeout(() => { refreshScope(); schedulePreview(); }, 400);
    };
    tagsIn.addEventListener('input', scheduleScope);
    subnetsIn.addEventListener('input', scheduleScope);
    picker.onChange(scheduleScope);

    // ---- 4 嚴重度 ----
    const sevName = uid('sev');
    const sevLive = h('p', { class: 'ds-visually-hidden', 'aria-live': 'polite' });
    const sevSection = h(
      'section',
      { class: 'k-form-section', 'aria-labelledby': 'rs-sev' },
      h('h2', { class: 'k-section-title', id: 'rs-sev' }, t('alerts.edit.s4')),
      h(
        'div',
        { role: 'radiogroup', 'aria-labelledby': 'rs-sev' },
        SEVERITIES.map((s) =>
          h(
            'label',
            { class: 'ds-radio-card' },
            h('input', {
              type: 'radio',
              name: sevName,
              value: s,
              checked: s === severity,
              disabled: readOnly,
              on: {
                change: () => {
                  severity = s;
                  markDirty();
                  // 切換嚴重度時通知設定回到該等級的預設（screens.md S09）
                  notify.repeat_interval_secs = null;
                  notify.repeat_max = null;
                  notify.notify_recovery = null;
                  notify.escalate_after_secs = null;
                  drawNotify();
                  announce(t('alerts.edit.sevChanged', { s: severityLabel(s), d: t(`alerts.edit.sevDesc.${s}`) }));
                },
              },
            }),
            h('span', null, h('strong', null, severityLabel(s)), h('span', { class: 'k-sub' }, t(`alerts.edit.sevDesc.${s}`))),
          ),
        ),
      ),
      builtin && system ? h('p', { class: 'caption' }, t('alerts.edit.systemSevNote')) : null,
      sevLive,
    );

    // ---- 5 通知 ----
    const notify = { escalate_channels: [], ...(r.notify || {}) };
    const notifyBody = h('div');
    const notifySection = h('section', { class: 'k-form-section', 'aria-labelledby': 'rs-notify' }, h('h2', { class: 'k-section-title', id: 'rs-notify' }, t('alerts.edit.s5')), notifyBody);
    const chosen = new Set(r.channels || []);
    const escChosen = new Set(notify.escalate_channels || []);
    function channelChecks(set, name, labelId) {
      const list = channels || [];
      const ids = new Set([...list.map((c) => c.channel_id), ...set]);
      if (!ids.size) {
        return h('p', { class: 'secondary' }, t('alerts.edit.noChannels'), session.can('channels:write') ? [' ', h('a', { href: '/alerts/channels' }, t('alerts.events.addChannel'))] : null);
      }
      return h(
        'div',
        { class: 'ds-radio-group', role: 'group', 'aria-labelledby': labelId },
        [...ids].map((id) => {
          const c = list.find((x) => x.channel_id === id);
          return h(
            'label',
            { class: 'ds-checkbox' },
            h('input', {
              type: 'checkbox',
              name,
              value: id,
              checked: set.has(id),
              disabled: readOnly,
              on: { change: (ev) => { if (ev.target.checked) set.add(id); else set.delete(id); markDirty(); } },
            }),
            c ? c.name : id,
            c && c.kind ? h('span', { class: 'caption' }, `（${t(`alerts.ch.kind.${c.kind}`)}）`) : null,
          );
        }),
      );
    }
    let repeatIn;
    let maxIn;
    let recSel;
    let escIn;
    let escErr;
    function drawNotify() {
      const d = NOTIFY_DEFAULTS[severity];
      const chId = uid('ch');
      const escId = uid('esc');
      repeatIn = numInput('repeat_minutes', notify.repeat_interval_secs != null ? notify.repeat_interval_secs / 60 : '', { min: 1, max: severity === 'emergency' ? 30 : 1440, disabled: readOnly });
      repeatIn.placeholder = d.repeat ? t('alerts.edit.defaultN', { n: d.repeat }) : t('alerts.edit.noRepeat');
      maxIn = numInput('repeat_max', notify.repeat_max != null ? notify.repeat_max : '', { min: 0, max: 100, step: 1, disabled: readOnly });
      maxIn.placeholder = t('alerts.edit.defaultN', { n: d.max });
      recSel = h(
        'select',
        { class: 'ds-select', name: 'notify_recovery' },
        h('option', { value: '', selected: notify.notify_recovery == null }, t('alerts.edit.recoveryDefault', { v: d.recovery ? t('alerts.edit.yes') : t('alerts.edit.no') })),
        h('option', { value: 'true', selected: notify.notify_recovery === true }, t('alerts.edit.recoveryOn')),
        h('option', { value: 'false', selected: notify.notify_recovery === false }, t('alerts.edit.recoveryOff')),
      );
      recSel.disabled = readOnly || severity === 'emergency';
      escIn = numInput('escalate_minutes', notify.escalate_after_secs != null ? notify.escalate_after_secs / 60 : '', { min: 1, max: 1440, disabled: readOnly });
      escIn.placeholder = d.escalate ? t('alerts.edit.defaultN', { n: d.escalate }) : '—';
      for (const el of [repeatIn, maxIn, recSel, escIn]) el.addEventListener('input', markDirty);
      escErr = h('p', { class: 'ds-field__error', id: uid('err'), hidden: true });
      const warnOnly = severity === 'warning' || severity === 'info';
      mount(
        notifyBody,
        h('p', { class: 'ds-field__label', id: chId }, t('alerts.edit.channels')),
        channelChecks(chosen, 'channels', chId),
        warnOnly ? h('p', { class: 'ds-field__hint' }, t(`alerts.edit.notifyNote.${severity}`)) : null,
        h(
          'div',
          { class: 'ds-form-row' },
          field({ label: t('alerts.edit.repeat'), input: repeatIn, hint: severity === 'emergency' ? t('alerts.edit.repeatEmergency') : t('alerts.edit.blankDefault') }),
          field({ label: t('alerts.edit.repeatMax'), input: maxIn, hint: t('alerts.edit.blankDefault') }),
          field({ label: t('alerts.edit.recovery'), input: recSel, hint: severity === 'emergency' ? t('alerts.edit.recoveryEmergency') : null }),
        ),
        h(
          'fieldset',
          { class: 'k-fieldset' },
          h('legend', null, t('alerts.edit.escalation'), severity === 'emergency' ? t('form.required') : null),
          field({ label: t('alerts.edit.escalateAfter'), input: escIn, hint: t('alerts.edit.blankDefault') }),
          h('p', { class: 'ds-field__label', id: escId }, t('alerts.edit.escalateTo')),
          channelChecks(escChosen, 'escalate_channels', escId),
          escErr,
          severity === 'emergency' ? h('p', { class: 'ds-field__hint' }, t('alerts.edit.emergencyNote')) : null,
        ),
      );
    }
    drawNotify();

    // ---- 6 名稱與說明 ----
    const nameIn = input({ name: 'name', value: r.name || '', maxlength: 100 });
    nameIn.disabled = readOnly || builtin;
    const nameF = field({ label: t('alerts.edit.name'), input: nameIn, required: true });
    const descIn = h('textarea', { class: 'ds-textarea', name: 'description', maxlength: 2000 }, r.description || '');
    descIn.disabled = readOnly;
    const descF = field({ label: t('alerts.edit.description'), input: descIn, hint: t('alerts.edit.descriptionHint') });
    nameIn.addEventListener('input', markDirty);
    descIn.addEventListener('input', markDirty);
    const nameSection = h('section', { class: 'k-form-section', 'aria-labelledby': 'rs-name' }, h('h2', { class: 'k-section-title', id: 'rs-name' }, t('alerts.edit.s6')), nameF, descF);

    // ---- 預覽 ----
    const previewBody = h('div', { 'aria-live': 'polite' });
    let previewTimer = null;
    let previewCtrl = null;
    function schedulePreview() {
      clearTimeout(previewTimer);
      previewTimer = setTimeout(runPreview, 600);
    }
    async function runPreview() {
      if (system) {
        mount(previewBody, h('p', { class: 'secondary' }, t('alerts.preview.system')));
        return;
      }
      if (fields.params.validate && fields.params.validate()) {
        mount(previewBody, h('p', { class: 'secondary' }, t('alerts.preview.fixFirst')));
        return;
      }
      if (previewCtrl) previewCtrl.abort();
      previewCtrl = new AbortController();
      mount(previewBody, h('p', { class: 'secondary' }, t('alerts.preview.running')));
      try {
        const p = await api('/api/v1/alert-rules/preview', { method: 'POST', body: { kind, params: fields.params.read(), scope: readScope(), days: 7 }, signal: previewCtrl.signal });
        if (disposed) return;
        mount(previewBody, previewView(p));
      } catch (err) {
        if (err.name === 'AbortError') return;
        mount(previewBody, h('p', { class: 'secondary' }, describe(err, { title: t('alerts.preview.failed') }).title, err.message && err.code !== 'internal' ? h('span', { class: 'k-sub' }, err.message) : null));
      }
    }
    function previewView(p) {
      if (!p.supported) return h('div', null, h('p', null, h('strong', null, t('alerts.preview.unsupported'))), h('p', { class: 'caption' }, p.reason || ''));
      const max = Math.max(1, ...(p.per_day || []).map((x) => x.fires));
      return h(
        'div',
        null,
        h('p', null, t('alerts.preview.intro', { d: p.days })),
        h('p', { class: 'k-preview-big' }, t('alerts.preview.fires', { n: fmt.num(p.fires), hosts: fmt.num(p.hosts_involved) })),
        h('p', null, t('alerts.preview.quick', { n: fmt.num(p.self_recovered_within_10m) })),
        Object.entries(p.not_reported || {})
          .filter(([, n]) => n > 0)
          .map(([reason, n]) => h('p', null, t('alerts.preview.missing', { count: fmt.num(n), reason: has(`alerts.preview.missing.${reason}`) ? t(`alerts.preview.missing.${reason}`) : reason }))),
        p.swap_total_zero ? h('p', null, t('alerts.preview.swapZero', { count: fmt.num(p.swap_total_zero) })) : null,
        p.items_in_scope != null && scopeKey(fields.params.read().metric)
          ? h('p', { class: 'caption' }, t('alerts.preview.items', { count: fmt.num(p.items_in_scope), what: t(`alerts.edit.scope.${scopeKey(fields.params.read().metric)}`) }))
          : null,
        p.noisy
          ? inlineAlert({
              kind: 'warning',
              title: t('alerts.preview.noisy'),
              body: p.suggestion
                ? h(
                    'div',
                    null,
                    h('p', null, t('alerts.preview.suggest', { m: Math.round(p.suggestion.for_secs / 60), n: fmt.num(p.suggestion.fires) })),
                    readOnly || paramsLocked ? null : button({
                      text: t('alerts.preview.apply'),
                      size: 'sm',
                      onClick: () => {
                        const el = condBody.querySelector('input[name="for_minutes"]');
                        if (el) {
                          el.value = String(Math.round(p.suggestion.for_secs / 60));
                          el.dispatchEvent(new Event('input'));
                          el.focus();
                        }
                      },
                    }),
                  )
                : null,
            })
          : null,
        (p.per_day || []).length
          ? h(
              'table',
              { class: 'k-mini-table k-bars' },
              h('caption', null, t('alerts.preview.perDay')),
              h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('alerts.preview.date')), h('th', { scope: 'col' }, t('alerts.preview.count')))),
              h(
                'tbody',
                null,
                p.per_day.map((x) =>
                  h('tr', null, h('td', null, x.date), h('td', null, h('span', { class: 'k-bar', 'aria-hidden': 'true', style: { width: `${Math.round((x.fires / max) * 100)}%` } }), h('span', { class: 'num' }, fmt.num(x.fires)))),
                ),
              ),
            )
          : null,
        (p.notes || []).length ? h('ul', { class: 'caption' }, p.notes.map((n) => h('li', null, n))) : null,
        h('p', { class: 'caption' }, t('alerts.preview.approx', { r: p.resolution_secs >= 3600 ? t('alerts.preview.res1h') : t('alerts.preview.res5m') })),
      );
    }
    const previewCard = card({ title: t('alerts.preview.title'), body: previewBody, cls: 'k-preview-card' });

    // ---- 儲存 ----
    const errBox = h('div');
    const saveBtn = button({ text: isNew ? t('alerts.edit.create') : t('alerts.edit.save'), variant: 'primary', type: 'submit' });
    const cancelBtn = h('a', { class: 'ds-button ds-button--secondary', href: '/alerts/rules' }, t('common.cancel'));
    const delBtn = !isNew && !builtin && canWrite ? button({ text: t('alerts.rules.delete'), variant: 'danger-secondary', iconName: 'trash', onClick: () => removeRule() }) : null;

    async function removeRule() {
      const ok = await confirm({ title: t('alerts.rules.deleteTitle', { name: r.name }), body: t('alerts.rules.deleteBody'), confirmText: t('alerts.rules.delete'), danger: true });
      if (!ok) return;
      try {
        await api(`/api/v1/alert-rules/${encodeURIComponent(ruleId)}`, { method: 'DELETE' });
        dirty = false;
        toast({ kind: 'success', title: t('alerts.rules.deleted', { name: r.name }) });
        navigate('/alerts/rules');
      } catch (err) {
        mount(errBox, h('div', { role: 'alert' }, describe(err).title));
      }
    }

    function numOrNull(el, mul) {
      return el.value === '' ? null : Math.round(Number(el.value) * mul);
    }

    function buildBody() {
      const n = {
        repeat_interval_secs: numOrNull(repeatIn, 60),
        repeat_max: numOrNull(maxIn, 1),
        notify_recovery: recSel.value === '' ? null : recSel.value === 'true',
        escalate_after_secs: numOrNull(escIn, 60),
        escalate_channels: [...escChosen],
      };
      const body = { severity, channels: [...chosen], notify: n, description: descIn.value.trim() || null };
      if (!builtin) {
        body.name = nameIn.value.trim();
        body.params = fields.params.read();
        body.scope = readScope();
        if (isNew) {
          body.kind = kind;
          body.enabled = r.enabled !== false;
        }
      } else {
        if (canAdmin) {
          const p = fields.params.read();
          if (Object.keys(p).length) body.params = p;
        }
        if (!system) body.scope = readScope();
      }
      return body;
    }

    function showServerError(err) {
      const param = err.details && err.details.parameter;
      let focusEl = null;
      if (param === 'name') {
        nameF.setError(err.message);
        focusEl = nameIn;
      } else if (param && param.startsWith('params')) {
        focusEl = fields.params.showError(param, err.message);
      } else if (param === 'notify.escalate_channels' || param === 'notify.escalate_after_secs') {
        escErr.textContent = err.message;
        escErr.hidden = false;
        focusEl = escIn;
      } else if (param && param.startsWith('scope.subnets')) {
        subnetsF.setError(err.message);
        focusEl = subnetsIn;
      }
      const d = describe(err, { title: t('alerts.edit.saveFailed') });
      mount(
        errBox,
        h(
          'div',
          { class: 'ds-alert ds-alert--error', role: 'alert', tabindex: '-1' },
          icon('status-critical'),
          h('div', { class: 'ds-alert__body' }, h('p', { class: 'ds-alert__title' }, t('alerts.edit.saveFailed'), d.code ? [' ', codeTag(d.code)] : null), h('p', null, err.code === 'rule_invalid' || err.code === 'invalid_parameter' ? err.message : d.reason)),
        ),
      );
      (focusEl || errBox.firstChild).focus();
    }

    const form = h(
      'form',
      { class: 'k-rule-form', novalidate: true },
      readOnly ? inlineAlert({ kind: 'info', title: t('alerts.readOnly'), body: t('alerts.edit.readOnlyBody') }) : null,
      builtin && !readOnly && !canAdmin ? inlineAlert({ kind: 'info', body: t('alerts.edit.builtinOperator') }) : null,
      kindSection,
      condSection,
      scopeSection,
      sevSection,
      notifySection,
      nameSection,
      errBox,
      readOnly ? null : h('div', { class: 'ds-button-row' }, cancelBtn, saveBtn, delBtn),
    );
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      if (readOnly) return;
      mount(errBox);
      nameF.setError('');
      escErr.hidden = true;
      if (!builtin && !nameIn.value.trim()) {
        nameF.setError(t('alerts.edit.nameRequired'));
        nameIn.focus();
        return;
      }
      const bad = fields.params.validate && fields.params.validate();
      if (bad) {
        bad.focus();
        return;
      }
      if (severity === 'emergency' && !builtin && !escChosen.size) {
        escErr.textContent = t('alerts.edit.escalateRequired');
        escErr.hidden = false;
        const first = notifyBody.querySelector('input[name="escalate_channels"]') || escIn;
        first.focus();
        return;
      }
      if (!system && matching === 0) {
        mount(errBox, inlineAlert({ kind: 'error', title: t('alerts.edit.scopeZero') }));
        tagsIn.focus();
        return;
      }
      setBusy(saveBtn, true, t('common.working'));
      try {
        const body = buildBody();
        const saved = isNew
          ? await api('/api/v1/alert-rules', { method: 'POST', body })
          : await api(`/api/v1/alert-rules/${encodeURIComponent(ruleId)}`, { method: 'PUT', body });
        dirty = false;
        toast({ kind: 'success', title: t(isNew ? 'alerts.edit.created' : 'alerts.edit.saved', { name: saved.name }) });
        navigate('/alerts/rules');
      } catch (err) {
        setBusy(saveBtn, false, true);
        showServerError(err);
      }
    });

    const title = isNew ? (base && base.name ? base.name : t('alerts.edit.newTitle')) : r.name;
    ctx.setTitle(title);
    mount(
      root,
      pageHead({ title, crumbs: crumbs(isNew ? t('alerts.edit.newTitle') : r.name), meta: builtin ? t('alerts.rules.builtinTag') : null }),
      h('div', { class: 'k-grid k-grid--side' }, h('div', null, form), h('div', null, previewCard)),
    );
    refreshScope();
    runPreview();
  }

  load();
  return {
    dispose() {
      disposed = true;
    },
    isDirty: () => dirty,
  };
}
