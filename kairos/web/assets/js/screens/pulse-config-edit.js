// S24 設定檔編輯（/pulse/config/:profileId；新增為 /pulse/config/new）
// （設計包 kairos/screens.md S24「設定檔編輯」；API.md 第 17.1 節）。
//
// - 表單依 GET /config/schema 產生（主控台不寫死鍵）：只顯示有填的鍵，每個鍵一列——整數（範圍）、列舉（select）、
//   布林（開啟／關閉）、字串清單（textarea 一行一個）；說明寫出範圍與套用方式（立即套用／會重新連線）。
//   隱私敏感的鍵只列出更保守的選項並註明；local_only_values（例如 processes.command = full）不列出。
//   內建預設值由 Pulse 提供，API 不回傳，這裡不顯示（與設計包線框的差異）。
// - 「新增設定」：白名單中其餘的鍵；移除一個鍵＝這些主機回到本機設定或內建預設。
// - 影響預覽：欄位變動後 600 ms 試算（POST /config/preview，aria-live="polite"，只讀出最後一次）。
//   影響 1,000 台以上時「儲存」以 L1 確認（pulse-common.js 的 withImpact；數字改變時以新的數字再確認一次）。
// - 驗證：client 端先檢查範圍；伺服器的 400 config_invalid（KRS-E0502）依 details.errors[].key 顯示在欄位下。
//   相同優先序的衝突（409 assignment_conflict）以 InlineAlert 說明。
// - 版本歷史：每一版的時間、修改者、原因、改了哪些鍵；「回復到這一版」以舊內容建立新的一版（L1，顯示影響）。
//   版本之間的並排差異在 0.10。
// - 刪除設定檔：L1；仍有指派時 aria-disabled 並說明「請先解除 N 個指派」。
// - 權限：config:write 才能編輯；稽核員唯讀（欄位停用、不顯示儲存與刪除）。未儲存的變更離開時詢問（isDirty）。

import { h, icon, mount } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { navigate } from '../core/router.js';
import { pageHead, card, emptyState, errorState, errorAlert, inlineAlert, field, input, skeletonBlock, setBusy } from '../ui/components.js';
import { toast } from '../ui/toast.js';
import { confirmL1, disabledButton } from './admin-common.js';
import { forbidden } from './errors.js';
import { isUnavailable, unavailableState, withImpact, CANCELLED, impactText, valueText, flatten, nest } from './pulse-common.js';

const CRUMB = { text: t('rm.cfg.title'), href: '/pulse/config' };

/** 這個鍵可以選的值（列舉；隱私敏感與 local_only 的值不列出） */
function enumValues(spec) {
  const local = new Set(spec.local_only_values || []);
  return (spec.value.values || []).filter((v) => !local.has(v));
}

function hintFor(spec) {
  const v = spec.value || {};
  const parts = [];
  if (v.type === 'integer') parts.push(t('rm.ed.range', { min: fmt.num(v.min), max: fmt.num(v.max) }));
  if (v.type === 'string_list') parts.push(spec.merge === 'union' ? t('rm.ed.listHint', { n: v.max_items }) : t('rm.ed.listHintPlain', { n: v.max_items }));
  parts.push(t(`rm.apply.${spec.apply}`));
  if (spec.min_pulse && spec.min_pulse !== '0.9.0') parts.push(t('rm.ed.minPulse', { v: spec.min_pulse }));
  if (spec.privacy_restrict_only) parts.push(t('rm.ed.privacy'));
  for (const lv of spec.local_only_values || []) parts.push(t('rm.ed.localOnly', { v: lv }));
  return parts.join(' · ');
}

/** 一個鍵的輸入元件：{ el（欄位）, read() → {ok, value, error} } */
function control(spec, value, disabled) {
  const v = spec.value || {};
  let el;
  let read;
  if (v.type === 'integer') {
    el = input({ type: 'number', name: spec.key, value: value == null ? '' : String(value), cls: 'ds-input--num', mono: true });
    el.setAttribute('min', String(v.min));
    el.setAttribute('max', String(v.max));
    el.setAttribute('step', '1');
    read = () => {
      const s = el.value.trim();
      const n = Number(s);
      if (s === '' || !Number.isInteger(n) || n < v.min || n > v.max) return { ok: false, error: t('rm.ed.intBad', { min: fmt.num(v.min), max: fmt.num(v.max) }) };
      return { ok: true, value: n };
    };
  } else if (v.type === 'boolean') {
    // 隱私敏感（false 才是保守的值）：只列出「關閉」；既有的 true（以 API 寫入）照樣顯示
    const conservative = spec.privacy && spec.privacy.rule === 'false_is_conservative';
    const opts = conservative && value !== true ? [false] : [true, false];
    el = h('select', { class: 'ds-select', name: spec.key }, opts.map((b) => h('option', { value: String(b), selected: value === b || (value == null && b === opts[0]) }, t(`rm.value.${b}`))));
    read = () => ({ ok: true, value: el.value === 'true' });
  } else if (v.type === 'enum') {
    const vals = enumValues(spec);
    if (value != null && !vals.includes(value)) vals.push(value);
    el = h('select', { class: 'ds-select', name: spec.key }, vals.map((x) => h('option', { value: x, selected: x === value }, x)));
    read = () => ({ ok: true, value: el.value });
  } else {
    el = h('textarea', { class: 'ds-textarea ds-input--mono', name: spec.key, rows: '3', spellcheck: 'false' });
    el.value = Array.isArray(value) ? value.join('\n') : '';
    read = () => {
      const list = el.value.split('\n').map((x) => x.trim()).filter(Boolean);
      const enc = new TextEncoder();
      if (list.length > (v.max_items || 1e9) || list.some((x) => enc.encode(x).length > (v.max_item_bytes || 1e9))) return { ok: false, error: t('rm.ed.listBad', { n: v.max_items, b: v.max_item_bytes }) };
      return { ok: true, value: list };
    };
  }
  el.disabled = Boolean(disabled);
  const f = field({ label: h('code', { class: 'k-code-tag' }, spec.key), input: el, hint: hintFor(spec) });
  return { f, el, read };
}

export function render(ctx) {
  const root = ctx.root;
  const rawId = String(ctx.params.profileId || '');
  const isNew = rawId === 'new';
  const canWrite = session.can('config:write');
  if (isNew && !canWrite) {
    forbidden(root, { perm: 'config:write' });
    ctx.setTitle(t('error.403.title'));
    return {};
  }
  if (!isNew && !/^\d+$/.test(rawId)) {
    ctx.app.notFound();
    return {};
  }
  let disposed = false;
  let schema = null;
  let profile = null;
  let initial = '';
  let saving = false;
  const rows = new Map(); // key → { spec, f, el, read, li }
  let order = [];
  let previewTimer = null;
  let previewSeq = 0;

  mount(root, pageHead({ title: isNew ? t('rm.ed.newTitle') : t('common.loading'), crumbs: [CRUMB, { text: '…' }] }), h('div', null, skeletonBlock(8)));

  const nameIn = input({ name: 'name', maxlength: 128, autocomplete: 'off' });
  const fname = field({ label: t('rm.ed.name'), input: nameIn, required: true });
  const descIn = h('textarea', { class: 'ds-textarea', name: 'description', rows: '2', maxlength: '1000' });
  const fdesc = field({ label: t('rm.ed.desc'), input: descIn, hint: t('rm.ed.descHint') });
  const rowsList = h('ul', { class: 'k-cfg-rows', id: 'config-rows' });
  const noKeys = h('p', { class: 'muted' }, t('rm.ed.noKeys'));
  const addSel = h('select', { class: 'ds-select', name: 'add-key' });
  const addBtn = h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => addKey() } }, icon('plus'), t('rm.ed.add'));
  const faddSel = field({ label: t('rm.ed.addLabel'), input: addSel });
  const addRow = h('div', { class: 'k-cfg-add' }, faddSel, addBtn);
  const allAdded = h('p', { class: 'caption', hidden: true }, t('rm.ed.allAdded'));
  const impactEl = h('p', { class: 'k-impact-line', 'aria-live': 'polite', id: 'config-impact' });
  const errBox = h('div');
  const reasonIn = h('textarea', { class: 'ds-textarea', name: 'reason', rows: '2', maxlength: '500' });
  const freason = field({ label: t('rm.ed.reason'), input: reasonIn });
  const dirtyNote = h('span', { class: 'caption', 'aria-live': 'polite' });
  const saveBtn = h('button', { type: 'button', class: 'ds-button ds-button--primary', on: { click: () => save() } }, t('rm.ed.save'));
  const cancelBtn = h('a', { class: 'ds-button ds-button--secondary', href: '/pulse/config' }, t('rm.ed.cancel'));
  const revBox = h('div');

  function snapshot() {
    const vals = {};
    for (const k of order) {
      const r = rows.get(k);
      vals[k] = r.el.value;
    }
    return JSON.stringify([nameIn.value.trim(), descIn.value.trim(), order, vals]);
  }
  function dirty() {
    return canWrite && !saving && initial !== '' && snapshot() !== initial;
  }
  function onChange() {
    dirtyNote.textContent = dirty() ? t('rm.ed.unsaved') : '';
    schedulePreview();
  }
  for (const el of [nameIn, descIn]) el.addEventListener('input', () => (dirtyNote.textContent = dirty() ? t('rm.ed.unsaved') : ''));

  function refreshAdd() {
    const rest = (schema.keys || []).filter((s) => !rows.has(s.key));
    mount(addSel, h('option', { value: '' }, t('rm.ed.addChoose')), rest.map((s) => h('option', { value: s.key }, s.key)));
    addRow.hidden = !canWrite || rest.length === 0;
    allAdded.hidden = !canWrite || rest.length !== 0;
    noKeys.hidden = order.length > 0;
  }

  function addRowFor(spec, value, focus) {
    const c = control(spec, value, !canWrite);
    const li = h('li', { class: 'k-cfg-row', dataset: { key: spec.key } }, c.f);
    if (canWrite) {
      li.appendChild(
        h(
          'button',
          {
            type: 'button',
            class: 'ds-button ds-button--ghost ds-button--sm',
            'aria-label': t('rm.ed.removeAria', { key: spec.key }),
            on: {
              click: () => {
                rows.delete(spec.key);
                order = order.filter((k) => k !== spec.key);
                li.remove();
                refreshAdd();
                onChange();
                addSel.focus();
              },
            },
          },
          icon('trash'),
          t('rm.ed.remove'),
        ),
      );
    }
    c.el.addEventListener('input', onChange);
    c.el.addEventListener('change', onChange);
    rows.set(spec.key, { spec, ...c, li });
    order.push(spec.key);
    rowsList.appendChild(li);
    if (focus) c.el.focus();
  }

  function addKey() {
    const key = addSel.value;
    if (!key) {
      faddSel.setError(t('rm.ed.addChoose'));
      addSel.focus();
      return;
    }
    faddSel.setError('');
    const spec = schema.keys.find((s) => s.key === key);
    const v = spec.value || {};
    const def = v.type === 'integer' ? null : v.type === 'boolean' ? (spec.privacy && spec.privacy.rule === 'false_is_conservative' ? false : true) : v.type === 'enum' ? enumValues(spec)[0] : [];
    addRowFor(spec, def, true);
    refreshAdd();
    onChange();
  }

  /** 收集設定（有錯誤時回 null，錯誤顯示在欄位下） */
  function collect({ quiet = false } = {}) {
    const flat = {};
    let bad = null;
    for (const k of order) {
      const r = rows.get(k);
      const v = r.read();
      if (!quiet) r.f.setError(v.ok ? '' : v.error);
      if (!v.ok) {
        bad = bad || r.el;
        continue;
      }
      flat[k] = v.value;
    }
    if (bad) {
      if (!quiet) bad.focus();
      return null;
    }
    return nest(flat);
  }

  function schedulePreview() {
    clearTimeout(previewTimer);
    previewTimer = setTimeout(preview, 600);
  }
  async function preview() {
    if (isNew) {
      impactEl.textContent = t('rm.impact.newProfile');
      return;
    }
    const settings = collect({ quiet: true });
    if (!settings) return;
    const my = ++previewSeq;
    try {
      const d = await api('/api/v1/config/preview', { method: 'POST', body: { profile_id: profile.id, settings } });
      if (my !== previewSeq || disposed) return;
      impactEl.textContent = `${t('rm.impact.label')}：${impactText(d)}`;
    } catch (err) {
      if (my !== previewSeq || disposed) return;
      impactEl.textContent = t('rm.impact.failed', { msg: err.message || err.code });
    }
  }

  function showFieldErrors(err) {
    const list = (err.details && err.details.errors) || [];
    let first = null;
    for (const e of list) {
      const r = rows.get(e.key);
      if (!r) continue;
      r.f.setError(e.message || (has(`rm.ed.fieldErr.${e.reason}`) ? t(`rm.ed.fieldErr.${e.reason}`) : e.reason));
      first = first || r.el;
    }
    if (first) first.focus();
    return Boolean(first);
  }

  async function save() {
    mount(errBox);
    const name = nameIn.value.trim();
    fname.setError(name ? '' : t('rm.ed.nameMissing'));
    if (!name) return nameIn.focus();
    const settings = collect();
    if (!settings) return undefined;
    const reason = reasonIn.value.trim() || undefined;
    const body = { name, description: descIn.value.trim(), settings, reason };
    saving = true;
    setBusy(saveBtn, true, t('common.working'));
    try {
      const res = await withImpact((token) =>
        isNew
          ? api('/api/v1/config/profiles', { method: 'POST', body })
          : api(`/api/v1/config/profiles/${profile.id}`, { method: 'PUT', body: token ? { ...body, impact_token: token } : body }),
      );
      setBusy(saveBtn, false, t('common.working'));
      if (res === CANCELLED) {
        saving = false;
        return undefined;
      }
      if (isNew) {
        toast({ kind: 'success', title: t('rm.ed.created', { name: res.name }) });
        initial = '';
        navigate(`/pulse/config/${res.id}`, { replace: true });
        return undefined;
      }
      toast({ kind: 'success', title: t('rm.ed.saved', { name: res.name, n: fmt.num(res.revision) }) });
      reasonIn.value = '';
      saving = false;
      await load();
    } catch (err) {
      saving = false;
      setBusy(saveBtn, false, t('common.working'));
      if (err.code === 'config_invalid' && showFieldErrors(err)) return undefined;
      if (err.code === 'profile_exists' || (err.code === 'invalid_parameter' && err.details && err.details.parameter === 'name')) {
        fname.setError(err.message);
        return nameIn.focus();
      }
      if (err.code === 'assignment_conflict') {
        const d = err.details || {};
        const a = inlineAlert({ kind: 'error', title: t('rm.as.conflictTitle'), body: err.message || (d.keys || []).join('、') });
        a.setAttribute('tabindex', '-1');
        mount(errBox, a);
        a.focus();
        return undefined;
      }
      const a = errorAlert(err, { title: t('rm.ed.saveFailed') });
      mount(errBox, a);
      a.focus();
    }
    return undefined;
  }

  function deleteProfile() {
    confirmL1({
      title: t('rm.ed.deleteTitle', { name: profile.name }),
      body: t('rm.ed.deleteBody'),
      confirmText: t('rm.ed.deleteConfirm'),
      danger: true,
      reason: 'optional',
      onConfirm: async ({ reason, errBox: box }) => {
        try {
          await api(`/api/v1/config/profiles/${profile.id}`, { method: 'DELETE', body: reason ? { reason } : {} });
          toast({ kind: 'success', title: t('rm.ed.deleted', { name: profile.name }) });
          initial = '';
          navigate('/pulse/config', { replace: true });
          return true;
        } catch (err) {
          const a = errorAlert(err, { title: t('rm.ed.deleteFailed') });
          mount(box, a);
          a.focus();
          return false;
        }
      },
    });
  }

  async function loadRevisions() {
    mount(revBox, skeletonBlock(3));
    let list;
    try {
      list = await api(`/api/v1/config/profiles/${profile.id}/revisions`);
    } catch (err) {
      if (!disposed) mount(revBox, errorState(err, { title: t('rm.rev.loadFailed'), onRetry: loadRevisions }));
      return;
    }
    if (disposed) return;
    mount(
      revBox,
      h('p', { class: 'caption' }, t('rm.rev.diffLater')),
      h(
        'ol',
        { class: 'k-timeline', 'aria-label': t('rm.rev.caption') },
        list.map((r) => {
          const changes = r.changes ? Object.entries(r.changes) : null;
          const isCurrent = r.revision === profile.revision;
          return h(
            'li',
            { dataset: { revision: String(r.revision) } },
            h('time', { datetime: r.changed_at }, fmt.monthDayTime(r.changed_at)),
            h(
              'div',
              null,
              h('p', { class: 'strong' }, t('rm.rev.item', { n: fmt.num(r.revision) }), ' · ', r.changed_by || '—', isCurrent ? [' · ', t('rm.rev.current')] : null),
              changes == null
                ? h('p', { class: 'secondary' }, t('rm.rev.first'))
                : changes.length
                  ? h('ul', { class: 'k-plain-list k-rev-changes' }, changes.map(([k, c]) => h('li', null, h('code', { class: 'k-code-tag' }, k), ' ', t('rm.rev.change', { before: valueText(c.before ?? undefined), after: valueText(c.after ?? undefined) }))))
                  : h('p', { class: 'secondary' }, t('rm.rev.noChange')),
              r.reason ? h('p', { class: 'caption' }, t('rm.rev.reasonLine', { r: r.reason })) : null,
              canWrite && !isCurrent
                ? h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', 'aria-label': t('rm.rev.revertAria', { n: r.revision }), on: { click: () => revert(r) } }, t('rm.rev.revert'))
                : null,
            ),
          );
        }),
      ),
    );
  }

  async function revert(r) {
    let summary = '';
    try {
      const d = await api('/api/v1/config/preview', { method: 'POST', body: { profile_id: profile.id, settings: r.settings || {} } });
      summary = `${t('rm.impact.label')}：${impactText(d)}`;
    } catch {
      summary = '';
    }
    const ok = await confirmL1({
      title: t('rm.rev.revertTitle', { n: fmt.num(r.revision) }),
      body: h('div', null, h('p', null, t('rm.rev.revertBody', { n: fmt.num(r.revision) })), summary ? h('p', null, summary) : null),
      confirmText: t('rm.rev.revertConfirm'),
      reason: 'optional',
    });
    if (!ok) return;
    try {
      const res = await withImpact((token) => api(`/api/v1/config/profiles/${profile.id}/revert`, { method: 'POST', body: { revision: r.revision, reason: ok.reason || undefined, impact_token: token } }));
      if (res === CANCELLED) return;
      toast({ kind: 'success', title: t('rm.rev.reverted', { n: fmt.num(r.revision), m: fmt.num(res.revision) }) });
      await load();
    } catch (err) {
      toast({ kind: 'error', title: t('rm.rev.revertFailed'), desc: err.message || err.code });
    }
  }

  function build() {
    const title = isNew ? t('rm.ed.newTitle') : profile.name;
    ctx.setTitle(title);
    rows.clear();
    order = [];
    mount(rowsList);
    nameIn.value = isNew ? '' : profile.name;
    descIn.value = isNew ? '' : profile.description || '';
    nameIn.disabled = !canWrite;
    descIn.disabled = !canWrite;
    const flat = isNew ? {} : flatten(profile.settings);
    for (const spec of schema.keys || []) if (spec.key in flat) addRowFor(spec, flat[spec.key], false);
    // 白名單已不含的鍵（舊資料）：唯讀列出，儲存時伺服器會拒絕（unknown_key）
    refreshAdd();
    const actions = [];
    if (!isNew) {
      actions.push(h('a', { class: 'ds-button ds-button--secondary', href: '#config-revisions' }, icon('clock'), t('rm.ed.history')));
      if (canWrite) {
        if (profile.assignments > 0) actions.push(...disabledButton(t('rm.ed.delete'), t('rm.ed.deleteInUse', { n: fmt.num(profile.assignments) }), { variant: 'danger-secondary' }));
        else actions.push(h('button', { type: 'button', class: 'ds-button ds-button--danger-secondary', on: { click: deleteProfile } }, icon('trash'), t('rm.ed.delete')));
      }
    }
    mount(
      root,
      pageHead({ title, crumbs: [CRUMB, { text: title }], meta: isNew ? null : t('rm.ed.revision', { n: fmt.num(profile.revision) }), actions: actions.length ? actions : null }),
      canWrite ? null : inlineAlert({ kind: 'info', body: t('rm.ed.readOnly') }),
      errBox,
      card({ title: t('rm.ed.basic'), body: [fname, fdesc] }),
      card({
        title: t('rm.ed.settings'),
        body: [h('p', { class: 'caption' }, t('rm.cfg.intro')), noKeys, rowsList, canWrite ? addRow : null, allAdded, canWrite ? h('p', { class: 'caption' }, t('rm.ed.removeNote')) : null],
      }),
      canWrite
        ? card({
            body: [
              impactEl,
              freason,
              h('div', { class: 'ds-button-row ds-button-row--end' }, dirtyNote, cancelBtn, saveBtn),
            ],
          })
        : null,
      isNew ? null : card({ title: t('rm.rev.title'), id: 'config-revisions', body: revBox }),
    );
    initial = snapshot();
    dirtyNote.textContent = '';
    impactEl.textContent = isNew ? t('rm.impact.newProfile') : '';
    if (!isNew) {
      preview();
      loadRevisions();
    }
  }

  async function load() {
    try {
      const [s, p] = await Promise.all([schema ? Promise.resolve(schema) : api('/api/v1/config/schema'), isNew ? Promise.resolve(null) : api(`/api/v1/config/profiles/${rawId}`)]);
      if (disposed) return;
      schema = s;
      profile = p;
    } catch (err) {
      if (disposed) return;
      if (isUnavailable(err)) {
        mount(root, pageHead({ title: t('rm.cfg.title'), crumbs: [CRUMB, { text: '…' }] }), unavailableState(err, 'config'));
        return;
      }
      if (err.code === 'profile_not_found' || err.code === 'not_found') {
        ctx.setTitle(t('rm.ed.notFound'));
        mount(
          root,
          pageHead({ title: t('rm.ed.notFound'), crumbs: [CRUMB, { text: rawId }] }),
          emptyState({ title: t('rm.ed.notFound'), body: t('rm.ed.notFoundBody'), iconName: 'settings', actions: [h('a', { class: 'ds-button ds-button--secondary', href: '/pulse/config' }, icon('chevron-left'), t('rm.ed.back'))] }),
        );
        ctx.focusTitle();
        return;
      }
      mount(root, pageHead({ title: t('rm.cfg.title'), crumbs: [CRUMB, { text: '…' }] }), errorState(err, { title: t('rm.ed.loadFailed'), onRetry: load }));
      return;
    }
    build();
    ctx.focusTitle();
  }

  load();
  return {
    dispose() {
      disposed = true;
      clearTimeout(previewTimer);
    },
    isDirty: () => dirty(),
  };
}

