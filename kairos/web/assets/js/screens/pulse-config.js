// S24 集中設定：設定檔清單（/pulse/config）與指派（/pulse/config/assignments）
// （設計包 kairos/screens.md S24；API.md 第 17.1 節）。設定檔的編輯在 pulse-config-edit.js。
//
// - 頁首的同步狀態：GET /config/status 的計數，每個數字連到 S04 並帶入「設定狀態」篩選；SSE config.host 時更新。
// - 設定檔：名稱、說明、鍵數、指派（只有一個時寫出對象與優先序）、影響台數、最後修改。
// - 指派：新增（設定檔、對象類型與值、優先序〔預設依類型〕、到期）。欄位變動後 600 ms 試算影響（POST /config/preview，
//   aria-live）；相同優先序的衝突（409 assignment_conflict）在對話框內以 InlineAlert 說明；解除指派為 L1（顯示影響台數）。
//   影響 1,000 台以上時以 L1 確認（withImpact）。
// - 權限：config:write（管理員、維運者）才有新增、解除；稽核員唯讀。功能關閉（503）時整頁 EmptyState。

import { h, icon, mount, uid, announce } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import * as fmt from '../core/format.js';
import { pageHead, card, emptyState, errorState, errorAlert, inlineAlert, field, input, skeletonBlock, setBusy } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { linkTabs, confirmL1 } from './admin-common.js';
import { isUnavailable, unavailableState, configStateLabel, selectorText, withImpact, CANCELLED, impactText, CONFIG_STATES } from './pulse-common.js';

const STATUS_ORDER = ['in_sync', 'pending', 'rejected', 'rolled_back', 'overridden', 'never_reported', 'disabled', 'unsupported'];
const KINDS = ['global', 'group', 'tag', 'host'];
const DEFAULT_PRIORITY = { global: 0, group: 100, tag: 200, host: 1000 };

function syncLine(d) {
  return h(
    'p',
    { class: 'k-sync-line', id: 'config-sync' },
    h('span', { class: 'strong' }, t('rm.cfg.syncLabel', { n: fmt.num(d.hosts || 0) })),
    STATUS_ORDER.filter((k) => CONFIG_STATES.includes(k)).map((k) => {
      const n = (d.counts && d.counts[k]) || 0;
      const label = configStateLabel(k);
      return h(
        'a',
        { href: `/hosts${qs({ config_state: k })}`, 'aria-label': t('rm.cfg.syncItemAria', { label, n: fmt.num(n) }), dataset: { state: k } },
        t('rm.cfg.syncItem', { label, n: fmt.num(n) }),
      );
    }),
  );
}

function assignSummary(p, assigns) {
  const mine = assigns.filter((a) => a.profile_id === p.id);
  if (!mine.length) return h('span', { class: 'muted' }, t('rm.cfg.assignNone'));
  if (mine.length > 1) return t('rm.cfg.assignN', { n: fmt.num(mine.length) });
  const a = mine[0];
  return [
    t('rm.cfg.assignOne', { sel: selectorText(a.selector_kind, a.selector_value), p: fmt.num(a.priority) }),
    a.expires_at ? t('rm.cfg.expires', { t: fmt.dateTime(a.expires_at, { seconds: false }) }) : '',
  ].join('');
}

export function render(ctx) {
  const root = ctx.root;
  const tab = location.pathname === '/pulse/config/assignments' ? 'assignments' : 'profiles';
  const canWrite = session.can('config:write');
  let disposed = false;
  let schema = null;
  let profiles = [];
  let assigns = [];
  let statusTimer = null;

  const statusBox = h('div', null, skeletonBlock(1));
  const body = h('div', null, skeletonBlock(6));
  const newProfile = canWrite ? h('a', { class: 'ds-button ds-button--primary', href: '/pulse/config/new' }, icon('plus'), t('rm.cfg.new')) : null;
  const newAssign = canWrite ? h('button', { type: 'button', class: 'ds-button ds-button--primary', on: { click: () => assignDialog() } }, icon('plus'), t('rm.as.new')) : null;
  mount(
    root,
    pageHead({ title: t('rm.cfg.title'), actions: tab === 'profiles' ? newProfile : newAssign }),
    statusBox,
    linkTabs(t('rm.cfg.tabs'), [
      ['profiles', '/pulse/config', t('rm.cfg.tab.profiles')],
      ['assignments', '/pulse/config/assignments', t('rm.cfg.tab.assignments')],
    ], tab),
    body,
  );

  async function loadStatus() {
    try {
      const d = await api('/api/v1/config/status');
      if (!disposed) mount(statusBox, card({ body: syncLine(d) }));
    } catch (err) {
      if (disposed) return;
      if (isUnavailable(err)) mount(statusBox);
      else mount(statusBox, errorState(err, { title: t('rm.cfg.syncFailed'), onRetry: loadStatus }));
    }
  }

  async function load() {
    try {
      [profiles, assigns, schema] = await Promise.all([api('/api/v1/config/profiles'), api('/api/v1/config/assignments'), schema ? Promise.resolve(schema) : api('/api/v1/config/schema')]);
    } catch (err) {
      if (disposed) return;
      if (isUnavailable(err)) {
        mount(statusBox);
        mount(body, unavailableState(err, 'config'));
        if (newProfile) newProfile.hidden = true;
        if (newAssign) newAssign.hidden = true;
        return;
      }
      mount(body, errorState(err, { title: tab === 'profiles' ? t('rm.cfg.loadFailed') : t('rm.as.loadFailed'), onRetry: load }));
      return;
    }
    if (disposed) return;
    if (tab === 'profiles') renderProfiles();
    else renderAssignments();
  }

  function renderProfiles() {
    if (!profiles.length) {
      mount(
        body,
        emptyState({
          title: t('rm.cfg.empty'),
          body: h('span', null, t('rm.cfg.emptyBody'), h('br'), h('span', { class: 'caption' }, t('rm.cfg.emptyKeys', { keys: (schema.keys || []).map((k) => k.key).join('、') }))),
          iconName: 'settings',
          actions: canWrite ? [h('a', { class: 'ds-button ds-button--primary', href: '/pulse/config/new' }, icon('plus'), t('rm.cfg.new'))] : null,
        }),
      );
      return;
    }
    mount(
      body,
      card({
        body: [
          h('p', { class: 'caption' }, t('rm.cfg.intro')),
          h(
            'div',
            { class: 'k-scroll-x' },
            h(
              'table',
              { class: 'k-mini-table', id: 'config-profiles' },
              h('caption', { class: 'ds-visually-hidden' }, t('rm.cfg.caption')),
              h(
                'thead',
                null,
                h(
                  'tr',
                  null,
                  h('th', { scope: 'col' }, t('rm.cfg.col.name')),
                  h('th', { scope: 'col' }, t('rm.cfg.col.desc')),
                  h('th', { scope: 'col', class: 'num' }, t('rm.cfg.col.keys')),
                  h('th', { scope: 'col' }, t('rm.cfg.col.assign')),
                  h('th', { scope: 'col', class: 'num' }, t('rm.cfg.col.hosts')),
                  h('th', { scope: 'col' }, t('rm.cfg.col.updated', { tz: fmt.tzLabel() })),
                ),
              ),
              h(
                'tbody',
                null,
                profiles.map((p) =>
                  h(
                    'tr',
                    { dataset: { profile: p.name } },
                    h('td', null, h('a', { href: `/pulse/config/${p.id}` }, p.name)),
                    h('td', null, p.description || '—'),
                    h('td', { class: 'num' }, fmt.num(p.keys)),
                    h('td', null, assignSummary(p, assigns)),
                    h('td', { class: 'num' }, fmt.num(p.hosts)),
                    h('td', null, fmt.dateTime(p.updated_at, { seconds: false }), p.updated_by ? h('span', { class: 'k-host-id' }, p.updated_by) : null),
                  ),
                ),
              ),
            ),
          ),
        ],
      }),
    );
  }

  function renderAssignments() {
    if (!assigns.length) {
      mount(
        body,
        emptyState({
          title: t('rm.as.empty'),
          body: t('rm.as.emptyBody'),
          iconName: 'settings',
          actions: canWrite && profiles.length ? [h('button', { type: 'button', class: 'ds-button ds-button--primary', on: { click: () => assignDialog() } }, icon('plus'), t('rm.as.new'))] : null,
        }),
      );
      return;
    }
    mount(
      body,
      card({
        body: h(
          'div',
          { class: 'k-scroll-x' },
          h(
            'table',
            { class: 'k-mini-table', id: 'config-assignments' },
            h('caption', { class: 'ds-visually-hidden' }, t('rm.as.caption')),
            h(
              'thead',
              null,
              h(
                'tr',
                null,
                h('th', { scope: 'col' }, t('rm.as.col.profile')),
                h('th', { scope: 'col' }, t('rm.as.col.target')),
                h('th', { scope: 'col', class: 'num' }, t('rm.as.col.priority')),
                h('th', { scope: 'col' }, t('rm.as.col.expires', { tz: fmt.tzLabel() })),
                h('th', { scope: 'col', class: 'num' }, t('rm.as.col.hosts')),
                h('th', { scope: 'col' }, t('rm.as.col.created')),
                canWrite ? h('th', { scope: 'col' }, h('span', { class: 'ds-visually-hidden' }, t('account.actions'))) : null,
              ),
            ),
            h(
              'tbody',
              null,
              assigns.map((a) => {
                const target = selectorText(a.selector_kind, a.selector_value);
                return h(
                  'tr',
                  { dataset: { assignment: String(a.id) } },
                  h('td', null, h('a', { href: `/pulse/config/${a.profile_id}` }, a.profile_name || `#${a.profile_id}`)),
                  h('td', null, target, a.selector_kind === 'host' ? h('span', { class: 'k-host-id mono' }, a.selector_value) : null),
                  h('td', { class: 'num' }, fmt.num(a.priority)),
                  h('td', null, a.expires_at ? fmt.dateTime(a.expires_at, { seconds: false }) : h('span', { class: 'muted' }, t('rm.as.noExpiry'))),
                  h('td', { class: 'num' }, fmt.num(a.hosts)),
                  h('td', null, a.created_by || '—', h('span', { class: 'k-host-id' }, fmt.dateTime(a.created_at, { seconds: false }))),
                  canWrite
                    ? h(
                        'td',
                        null,
                        h(
                          'button',
                          { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', 'aria-label': t('rm.as.unassignAria', { p: a.profile_name || a.profile_id, target }), on: { click: () => unassign(a) } },
                          t('rm.as.unassign'),
                        ),
                      )
                    : null,
                );
              }),
            ),
          ),
        ),
      }),
    );
  }

  async function unassign(a) {
    const target = selectorText(a.selector_kind, a.selector_value);
    let n = a.hosts;
    try {
      const p = await api('/api/v1/config/preview', { method: 'POST', body: { unassign: a.id } });
      n = p.affected_hosts;
    } catch {
      /* 試算失敗時以指派的比對台數說明 */
    }
    const ok = await confirmL1({
      title: t('rm.as.unassignTitle'),
      body: t('rm.as.unassignBody', { p: a.profile_name || `#${a.profile_id}`, target, n: fmt.num(n) }),
      confirmText: t('rm.as.unassignConfirm'),
      danger: true,
      reason: null,
    });
    if (!ok) return;
    try {
      // 影響 1,000 台以上時伺服器要求 impact_token：L1 已確認過，直接以伺服器回傳的 token 重送一次
      const r = await withImpact((token) => api(`/api/v1/config/assignments/${a.id}`, { method: 'DELETE', body: token ? { impact_token: token } : {} }));
      if (r === CANCELLED) return;
      toast({ kind: 'success', title: t('rm.as.unassigned') });
      load();
      loadStatus();
    } catch (err) {
      toast({ kind: 'error', title: t('rm.as.unassignFailed'), desc: errorText(err) });
    }
  }

  function errorText(err) {
    return err && err.message && err.message !== err.code ? err.message : err && err.code;
  }

  async function assignDialog() {
    if (!profiles.length) {
      toast({ kind: 'info', title: t('rm.as.noProfiles') });
      return;
    }
    let facets = null;
    try {
      facets = await api('/api/v1/hosts/facets');
    } catch {
      facets = null;
    }
    const errBox = h('div');
    const profSel = h('select', { class: 'ds-select', name: 'profile' }, profiles.map((p) => h('option', { value: String(p.id) }, p.name)));
    const fprof = field({ label: t('rm.as.profile'), input: profSel, required: true });
    const kindName = uid('kind');
    const radios = KINDS.map((k) => h('input', { type: 'radio', name: kindName, value: k, checked: k === 'global' }));
    const listId = uid('values');
    const datalist = h('datalist', { id: listId });
    const value = input({ name: 'value', autocomplete: 'off' });
    value.setAttribute('list', listId);
    const fvalue = field({ label: t('rm.as.value.group'), input: value, hint: t('rm.as.valueHint.group'), required: true });
    const prio = input({ type: 'number', name: 'priority', value: '0', cls: 'ds-input--num' });
    prio.setAttribute('step', '1');
    const fprio = field({ label: t('rm.as.priority'), input: prio, hint: t('rm.as.priorityHint') });
    const exp = input({ type: 'datetime-local', name: 'expires' });
    const fexp = field({ label: t('rm.as.expiresAt', { tz: fmt.tzLabel() }), input: exp, hint: t('rm.as.expiresHint') });
    const impact = h('p', { class: 'k-impact-line', 'aria-live': 'polite' });
    const conflictBox = h('div');
    const kind = () => radios.find((r) => r.checked).value;
    let prioTouched = false;
    prio.addEventListener('input', () => {
      prioTouched = true;
    });
    const syncKind = () => {
      const k = kind();
      fvalue.hidden = k === 'global';
      if (k !== 'global') {
        fvalue.querySelector('label').firstChild.textContent = t(`rm.as.value.${k}`);
        fvalue.querySelector('.ds-field__hint').textContent = t(`rm.as.valueHint.${k}`);
        const src = k === 'group' ? facets && facets.group : k === 'tag' ? facets && facets.tag : null;
        mount(datalist, ...((src && src.values) || []).map((v) => h('option', { value: v.value })));
      }
      if (!prioTouched) prio.value = String(DEFAULT_PRIORITY[k]);
    };
    for (const r of radios) r.addEventListener('change', () => {
      syncKind();
      schedule();
    });
    const selector = () => {
      const k = kind();
      if (k === 'global') return 'global';
      const v = value.value.trim();
      return v ? `${k}:${k === 'host' ? v.toLowerCase() : v}` : null;
    };
    const expiresIso = () => {
      if (!exp.value) return null;
      const ms = Date.parse(exp.value);
      return Number.isFinite(ms) ? new Date(ms).toISOString() : null;
    };
    let timer = null;
    let seq = 0;
    function schedule() {
      clearTimeout(timer);
      timer = setTimeout(preview, 600);
    }
    async function preview() {
      const sel = selector();
      mount(conflictBox);
      if (!sel) {
        impact.textContent = '';
        return;
      }
      const my = ++seq;
      impact.textContent = t('rm.as.previewing');
      const body = { assign: { profile_id: Number(profSel.value), selector: sel } };
      if (prio.value.trim() !== '' && Number.isInteger(Number(prio.value))) body.assign.priority = Number(prio.value);
      const e = expiresIso();
      if (e) body.assign.expires_at = e;
      try {
        const d = await api('/api/v1/config/preview', { method: 'POST', body });
        if (my !== seq) return;
        impact.textContent = `${t('rm.impact.label')}：${impactText(d)}`;
        if (d.conflicts && d.conflicts.length) showConflict(d.conflicts[0]);
      } catch (err) {
        if (my !== seq) return;
        impact.textContent = t('rm.impact.failed', { msg: errorText(err) });
      }
    }
    function showConflict(c) {
      const ids = c.assignments || [];
      const other = ids.map((id) => assigns.find((a) => a.id === id)).find(Boolean);
      const me = profiles.find((p) => String(p.id) === profSel.value);
      mount(
        conflictBox,
        inlineAlert({
          kind: 'error',
          title: t('rm.as.conflictTitle'),
          body: t('rm.as.conflict', { a: me ? me.name : '—', b: other ? other.profile_name : t('rm.as.otherProfile'), keys: (c.keys || []).join('、'), n: fmt.num(c.hosts || 0) }),
        }),
      );
    }
    for (const el of [profSel, value, prio, exp]) el.addEventListener('input', schedule);
    profSel.addEventListener('change', schedule);

    const ok = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('rm.as.create'));
    const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
    const d = openDialog({
      title: t('rm.as.createTitle'),
      size: 'md',
      body: h(
        'div',
        { class: 'k-stack--sm' },
        errBox,
        fprof,
        h('fieldset', { class: 'k-fieldset' }, h('legend', null, t('rm.as.kind')), h('div', { class: 'ds-radio-group' }, KINDS.map((k, i) => h('label', { class: 'ds-radio' }, radios[i], t(`rm.selKind.${k}`))))),
        fvalue,
        datalist,
        fprio,
        fexp,
        impact,
        conflictBox,
      ),
      actions: [cancel, ok],
      initialFocus: profSel,
      onClose: () => clearTimeout(timer),
    });
    syncKind();
    schedule();
    cancel.addEventListener('click', () => d.close());
    ok.addEventListener('click', async () => {
      mount(errBox);
      mount(conflictBox);
      const sel = selector();
      fvalue.setError(sel ? '' : t('rm.as.valueMissing'));
      if (!sel) return value.focus();
      const pv = prio.value.trim();
      const prioOk = pv === '' || Number.isInteger(Number(pv));
      fprio.setError(prioOk ? '' : t('rm.as.priorityBad'));
      if (!prioOk) return prio.focus();
      const e = expiresIso();
      const past = e && Date.parse(e) <= fmt.serverNow();
      fexp.setError(past ? t('rm.as.expiresPast') : '');
      if (past) return exp.focus();
      const body = { profile_id: Number(profSel.value), selector: sel };
      if (pv !== '') body.priority = Number(pv);
      if (e) body.expires_at = e;
      setBusy(ok, true, t('common.working'));
      try {
        const r = await withImpact((token) => api('/api/v1/config/assignments', { method: 'POST', body: token ? { ...body, impact_token: token } : body }));
        setBusy(ok, false, t('common.working'));
        if (r === CANCELLED) return;
        d.close();
        const p = profiles.find((x) => x.id === body.profile_id);
        toast({ kind: 'success', title: t('rm.as.created', { p: p ? p.name : '', target: selectorText(kind(), sel.slice(sel.indexOf(':') + 1)) }) });
        load();
        loadStatus();
      } catch (err) {
        setBusy(ok, false, t('common.working'));
        if (err.code === 'assignment_conflict') {
          showConflict(err.details || {});
          announce(t('rm.as.conflictTitle'));
          return;
        }
        if (err.code === 'invalid_parameter' && err.details && err.details.parameter === 'selector') {
          fvalue.setError(err.message);
          return value.focus();
        }
        if (err.code === 'invalid_parameter' && err.details && err.details.parameter === 'expires_at') {
          fexp.setError(err.message);
          return exp.focus();
        }
        const a = errorAlert(err, { title: t('rm.as.createFailed') });
        mount(errBox, a);
        a.focus();
      }
    });
  }

  loadStatus();
  load();
  const unsub = sse.subscribe((ev) => {
    if (ev.type !== 'config.host' && ev.type !== 'resync') return;
    clearTimeout(statusTimer);
    statusTimer = setTimeout(() => {
      loadStatus();
      if (ev.type === 'resync') load().finally(() => sse.resyncSettled());
    }, ev.type === 'resync' ? 0 : 2000);
  });
  return {
    dispose() {
      disposed = true;
      clearTimeout(statusTimer);
      unsub();
    },
  };
}
