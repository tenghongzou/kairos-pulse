// S17 目錄服務（/admin/directory；設計包 screens.md S17「0.6 後端現況」；API.md 第 12.7 節）。
//
// - 區塊 1「連線」：唯讀（kairos.toml 的 [ldap]，需要重新啟動；bind 密碼一律 ********），「上傳…」「更換」「＋備援」隱藏。
//   未加密（allow_plaintext）時頁首警告。「測試連線」以「以帳號測試登入」代替（POST /api/v1/directory/test，directory:write）。
// - 區塊 2「群組對應角色」：PUT /api/v1/directory/mappings（L1，取代全部）；「目前成員」顯示「—」（只在登入時判斷）。
// - 區塊 3「同步」：延後，整區隱藏；改在區塊 1 下方說明只在登入時讀取群組與帳號狀態。

import { h, icon, mount } from '../core/dom.js';
import { t, roleLabel, permissionLabel } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import { lookup, fixSteps } from '../core/errors.js';
import { pageHead, card, dl, errorState, errorAlert, field, input, inlineAlert, skeletonBlock, setBusy } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { ROLES, assignmentText, assignmentKey, confirmL1, scopeText } from './admin-common.js';

export function render(ctx) {
  const root = ctx.root;
  const canWrite = session.can('directory:write');

  async function load() {
    mount(root, pageHead({ title: t('adm.dir.title') }), skeletonBlock(8));
    let d;
    try {
      d = await api('/api/v1/directory');
    } catch (err) {
      mount(root, pageHead({ title: t('adm.dir.title') }), errorState(err, { title: t('adm.dir.loadFailed'), onRetry: load }));
      return;
    }
    draw(d);
  }

  function draw(d) {
    const urls = d.urls || [];
    const enc = urls[0] && urls[0].startsWith('ldaps://') ? 'LDAPS' : d.starttls ? 'StartTLS' : t('adm.dir.plain');
    const testBtn = canWrite && d.enabled ? h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => testLogin() } }, icon('user'), t('adm.dir.test')) : null;
    const conn = card({
      title: t('adm.dir.connection'),
      actions: testBtn,
      body: [
        h('p', { class: 'caption' }, t('adm.dir.inFile')),
        dl([
          [t('adm.dir.enabled'), d.enabled ? t('adm.value.true') : t('adm.dir.disabledText')],
          [t('adm.dir.server'), urls.length ? h('span', { class: 'mono' }, urls[0]) : '—'],
          urls.length > 1 ? [t('adm.dir.fallback'), urls.slice(1).map((u) => h('span', { class: 'mono k-block' }, u))] : null,
          [t('adm.dir.encryption'), enc],
          [t('adm.dir.caFile'), d.ca_file ? h('span', { class: 'mono' }, d.ca_file) : '—'],
          [t('adm.dir.bindDn'), d.bind_dn ? h('span', { class: 'mono' }, d.bind_dn) : t('adm.dir.anonymous')],
          [t('adm.dir.bindPassword'), d.bind_password ? h('span', { class: 'mono' }, d.bind_password) : '—'],
          [t('adm.dir.userBase'), d.user_base ? h('span', { class: 'mono' }, d.user_base) : '—'],
          [t('adm.dir.userFilter'), d.user_filter ? h('span', { class: 'mono' }, d.user_filter) : '—'],
          [t('adm.dir.attrs'), h('span', { class: 'mono' }, [d.login_attribute, d.display_name_attribute, d.email_attribute, d.group_attribute].filter(Boolean).join(' · '))],
          [t('adm.dir.timeout'), t('adm.dir.secs', { n: d.timeout_secs })],
        ]),
        h('p', { class: 'caption' }, t('adm.dir.verifyNote')),
        inlineAlert({ kind: 'info', title: t('adm.dir.syncTitle'), body: h('ul', null, h('li', null, t('adm.dir.sync1')), h('li', null, t('adm.dir.sync2')), h('li', null, t('adm.dir.sync3'))) }),
      ],
    });

    const mappings = d.mappings || [];
    const addBtn = canWrite ? h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => addMapping(mappings) } }, icon('plus'), t('adm.dir.addMapping')) : null;
    const map = card({
      title: t('adm.dir.mappings'),
      actions: addBtn,
      body: mappings.length
        ? [
            h(
              'div',
              { class: 'k-scroll-x' },
              h(
                'table',
                { class: 'k-mini-table' },
                h('caption', { class: 'ds-visually-hidden' }, t('adm.dir.mappings')),
                h(
                  'thead',
                  null,
                  h('tr', null, h('th', { scope: 'col' }, t('adm.dir.group')), h('th', { scope: 'col' }, t('adm.roles.role')), h('th', { scope: 'col' }, t('adm.roles.scope')), h('th', { scope: 'col' }, t('adm.dir.members')), canWrite ? h('th', { scope: 'col' }, h('span', { class: 'ds-visually-hidden' }, t('account.actions'))) : null),
                ),
                h(
                  'tbody',
                  null,
                  mappings.map((m, i) =>
                    h(
                      'tr',
                      null,
                      h('td', null, h('span', { class: 'mono' }, m.group_dn)),
                      h('td', null, roleLabel(m.role)),
                      h('td', null, scopeText(m.scope)),
                      h('td', null, h('span', { title: t('adm.dir.membersNote') }, '—')),
                      canWrite
                        ? h('td', null, h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', 'aria-label': t('adm.dir.removeAria', { g: m.group_dn, a: assignmentText(m) }), on: { click: () => removeMapping(mappings, i) } }, t('adm.roles.remove')))
                        : null,
                    ),
                  ),
                ),
              ),
            ),
            h('p', { class: 'caption' }, t('adm.dir.membersNote')),
          ]
        : h('p', { class: 'muted' }, t('adm.dir.noMappings')),
    });

    mount(
      root,
      pageHead({ title: t('adm.dir.title'), meta: d.enabled ? t('adm.dir.statusEnabled') : t('adm.dir.statusDisabled') }),
      d.allow_plaintext ? h('div', { class: 'ds-banner ds-banner--warning', role: 'status' }, icon('status-warning'), h('div', { class: 'ds-banner__body' }, t('adm.dir.plaintextBanner'))) : null,
      !d.enabled ? inlineAlert({ kind: 'info', title: t('adm.dir.notEnabled'), body: t('adm.dir.notEnabledBody') }) : null,
      h('div', { class: 'k-settings-grid' }, conn, map),
    );
  }

  async function saveMappings(list, { title, body, okText }) {
    return confirmL1({
      title,
      body,
      confirmText: okText,
      reason: null,
      onConfirm: async ({ errBox }) => {
        try {
          await api('/api/v1/directory/mappings', { method: 'PUT', body: { mappings: list.map((m) => ({ group_dn: m.group_dn, role: assignmentKey(m) })) } });
          toast({ kind: 'success', title: t('adm.dir.saved') });
          load();
          return true;
        } catch (err) {
          const a = errorAlert(err, { title: t('adm.dir.saveFailed') });
          mount(errBox, a);
          a.focus();
          return false;
        }
      },
    });
  }

  function removeMapping(mappings, i) {
    const m = mappings[i];
    const next = mappings.filter((_, j) => j !== i);
    saveMappings(next, { title: t('adm.dir.removeTitle'), body: t('adm.dir.removeBody', { g: m.group_dn, a: assignmentText(m) }), okText: t('adm.dir.removeConfirm') });
  }

  function addMapping(mappings) {
    const errBox = h('div');
    const dn = input({ name: 'group_dn', mono: true, maxlength: 1024, placeholder: 'CN=IT-Ops,OU=Groups,DC=example,DC=gov,DC=tw' });
    const fdn = field({ label: t('adm.dir.group'), input: dn, required: true });
    const roleSel = h('select', { class: 'ds-select', name: 'role' }, ROLES.map((r) => h('option', { value: r, selected: r === 'viewer' }, roleLabel(r))));
    const scopeSel = h('select', { class: 'ds-select', name: 'scope' }, h('option', { value: 'global' }, t('adm.scope.global')), h('option', { value: 'group' }, t('adm.scope.groupOption')));
    const grp = input({ name: 'group', maxlength: 64 });
    const fgrp = field({ label: t('adm.roles.groupName'), input: grp, hint: t('adm.roles.groupHint') });
    fgrp.hidden = true;
    scopeSel.addEventListener('change', () => (fgrp.hidden = scopeSel.value !== 'group'));
    const ok = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('adm.next'));
    const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
    const d = openDialog({
      title: t('adm.dir.addTitle'),
      size: 'md',
      body: h('div', null, errBox, fdn, field({ label: t('adm.roles.role'), input: roleSel }), field({ label: t('adm.roles.scope'), input: scopeSel }), fgrp),
      actions: [cancel, ok],
      initialFocus: dn,
    });
    cancel.addEventListener('click', () => d.close());
    ok.addEventListener('click', () => {
      const g = dn.value.trim();
      fdn.setError(g ? '' : t('adm.dir.groupMissing'));
      if (!g) return dn.focus();
      let scope = { type: 'global' };
      if (scopeSel.value === 'group') {
        if (!grp.value.trim()) {
          fgrp.setError(t('adm.roles.groupMissing'));
          return grp.focus();
        }
        scope = { type: 'group', group: grp.value.trim() };
      }
      const m = { group_dn: g, role: roleSel.value, scope };
      d.close();
      saveMappings([...mappings, m], { title: t('adm.dir.addConfirmTitle'), body: t('adm.dir.addConfirmBody', { g, a: assignmentText(m) }), okText: t('adm.dir.addConfirm') });
    });
  }

  function testLogin() {
    const errBox = h('div');
    const result = h('div', { 'aria-live': 'polite' });
    const un = input({ name: 'username', autocomplete: 'off' });
    const pw = input({ type: 'password', name: 'password', autocomplete: 'off' });
    const ok = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('adm.dir.testRun'));
    const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.close'));
    const d = openDialog({
      title: t('adm.dir.testTitle'),
      size: 'md',
      body: h('div', null, h('p', null, t('adm.dir.testLead')), errBox, field({ label: t('adm.user.username'), input: un, required: true }), field({ label: t('login.password'), input: pw, required: true }), result),
      actions: [cancel, ok],
      initialFocus: un,
    });
    cancel.addEventListener('click', () => d.close());
    ok.addEventListener('click', async () => {
      mount(errBox);
      mount(result);
      if (!un.value.trim() || !pw.value) return (un.value.trim() ? pw : un).focus();
      setBusy(ok, true, t('common.working'));
      try {
        const r = await api('/api/v1/directory/test', { method: 'POST', body: { username: un.value.trim(), password: pw.value } });
        pw.value = '';
        if (r.ok) {
          mount(
            result,
            inlineAlert({
              kind: 'success',
              title: t('adm.dir.testOk'),
              body: dl([
                ['DN', h('span', { class: 'mono' }, r.user.dn)],
                [t('adm.user.displayName'), r.user.display_name || '—'],
                [t('adm.dir.groups'), (r.user.groups || []).length ? r.user.groups.map((g) => h('span', { class: 'mono k-block' }, g)) : '—'],
                [t('adm.dir.resultRoles'), (r.roles || []).length ? r.roles.map((a) => h('span', { class: 'k-block' }, `${assignmentText(a)}${a.via_group ? ` ← ${a.via_group}` : ''}`)) : t('adm.dir.noRoles')],
                [t('adm.user.effective'), (r.permissions || []).length ? r.permissions.map((p) => h('span', { class: 'k-block' }, permissionLabel(p))) : '—'],
              ]),
            }),
          );
        } else {
          const e = lookup(r.product_code);
          mount(
            result,
            inlineAlert({
              kind: 'error',
              title: e ? e.title || e.text : t('adm.dir.testFailed'),
              code: r.product_code,
              body: h('div', null, e && e.reason ? h('p', null, e.reason) : null, e && e.fix ? h('ol', { class: 'k-fix-steps' }, fixSteps(e.fix).map((s) => h('li', null, s))) : null, r.detail ? h('details', { class: 'k-details' }, h('summary', null, t('adm.dir.adminDetail')), h('p', { class: 'k-code-line' }, r.detail)) : null),
            }),
          );
        }
      } catch (err) {
        pw.value = '';
        const a = errorAlert(err, { title: t('adm.dir.testFailed') });
        mount(errBox, a);
        a.focus();
      } finally {
        setBusy(ok, false, t('common.working'));
      }
    });
  }

  load();
  return {};
}
