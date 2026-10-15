// S16 使用者與角色：清單（/admin/users）與「角色與權限」分頁（/admin/roles）
// （設計包 kairos/screens.md S16「0.6 後端現況」；API.md 第 12.5 節）。
//
// - 清單：GET /api/v1/users（來源、狀態、角色、搜尋在伺服器端；MFA 在畫面上篩選），條件寫在網址。
// - 新增使用者（users:write）：本機帳號；初始密碼由系統產生並只顯示一次（showSecretOnce），或由管理員輸入。
// - 隱藏（INTEGRATION.md §28.6）：「從目錄服務加入」、使用者權限清冊 CSV 匯出。
// - 角色與權限：GET /api/v1/roles 的權限矩陣（唯讀）與各角色的成員數。

import { h, icon, mount } from '../core/dom.js';
import { t, roleLabel, permissionLabel } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { replaceQuery, navigate } from '../core/router.js';
import { pageHead, card, emptyState, errorState, errorAlert, field, input, statusBadge, neutralBadge, skeletonBlock, setBusy } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { ROLES, assignmentText, assignmentKey, linkTabs, showSecretOnce } from './admin-common.js';

const STATUS_BADGE = { active: ['online', 'adm.user.status.active'], disabled: ['offline', 'adm.user.status.disabled'], locked: ['warning', 'adm.user.status.locked'] };

export function userStatusBadge(status) {
  const [s, key] = STATUS_BADGE[status] || ['unknown', null];
  return statusBadge(s, key ? t(key) : status);
}

export function mfaText(u) {
  if (u.mfa === 'totp') return t('adm.user.mfa.totp');
  if (u.mfa === 'directory') return t('adm.user.mfa.directory');
  return t('adm.user.mfa.none');
}

export function sourceText(src) {
  return src === 'ldap' ? t('adm.user.source.ldap') : t('adm.user.source.local');
}

/** 使用者／角色與權限的分頁 */
export function adminTabs(active) {
  return linkTabs(t('adm.users.tabs'), [['users', '/admin/users', t('adm.users.tab.users')], ['roles', '/admin/roles', t('adm.users.tab.roles')]], active);
}

/**
 * 角色指派的編輯器（新增使用者、編輯角色共用）：已有的指派可移除；新增一列：角色＋範圍（全站／主機群組）。
 * 群組範圍時說明「目前只能檢視該群組的主機」（D39）。
 * @returns {{el, value(): Array<{role, scope}>}}
 */
export function assignmentEditor(initial = []) {
  let list = initial.map((a) => ({ role: a.role, scope: a.scope && a.scope.type === 'group' ? { type: 'group', group: a.scope.group } : { type: 'global' } }));
  const listEl = h('ul', { class: 'k-check-list', 'aria-label': t('adm.roles.current') });
  const roleSel = h('select', { class: 'ds-select', name: 'role' }, ROLES.map((r) => h('option', { value: r, selected: r === 'viewer' }, roleLabel(r))));
  const scopeSel = h('select', { class: 'ds-select', name: 'scope' }, h('option', { value: 'global' }, t('adm.scope.global')), h('option', { value: 'group' }, t('adm.scope.groupOption')));
  const groupIn = input({ name: 'group', maxlength: 64 });
  const groupField = field({ label: t('adm.roles.groupName'), input: groupIn, hint: t('adm.roles.groupHint') });
  groupField.hidden = true;
  scopeSel.addEventListener('change', () => {
    groupField.hidden = scopeSel.value !== 'group';
    if (!groupField.hidden) groupIn.focus();
  });
  const addBtn = h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm' }, icon('plus'), t('adm.roles.add'));
  const draw = () => {
    mount(
      listEl,
      ...(list.length
        ? list.map((a, i) =>
            h(
              'li',
              null,
              h('span', { class: 'k-grow' }, assignmentText(a)),
              h(
                'button',
                {
                  type: 'button',
                  class: 'ds-button ds-button--ghost ds-button--sm',
                  'aria-label': t('adm.roles.removeAria', { a: assignmentText(a) }),
                  on: {
                    click: () => {
                      list.splice(i, 1);
                      draw();
                      addBtn.focus();
                    },
                  },
                },
                t('adm.roles.remove'),
              ),
            ),
          )
        : [h('li', { class: 'muted' }, t('adm.roles.none'))]),
    );
  };
  addBtn.addEventListener('click', () => {
    let scope = { type: 'global' };
    if (scopeSel.value === 'group') {
      const g = groupIn.value.trim();
      if (!g) {
        groupField.setError(t('adm.roles.groupMissing'));
        groupIn.focus();
        return;
      }
      groupField.setError('');
      scope = { type: 'group', group: g };
    }
    const a = { role: roleSel.value, scope };
    if (!list.some((x) => assignmentKey(x) === assignmentKey(a))) list.push(a);
    groupIn.value = '';
    draw();
  });
  draw();
  const el = h(
    'fieldset',
    null,
    h('legend', null, t('adm.roles.legend')),
    listEl,
    h('div', { class: 'k-filter-selects' }, h('label', { class: 'k-labelled-select' }, t('adm.roles.role'), roleSel), h('label', { class: 'k-labelled-select' }, t('adm.roles.scope'), scopeSel), addBtn),
    groupField,
  );
  return { el, value: () => list.slice() };
}

/** 新增使用者（本機帳號） */
function createUserDialog(onCreated) {
  const errBox = h('div');
  const uname = input({ name: 'username', autocomplete: 'off', maxlength: 64, required: true });
  const dname = input({ name: 'display_name', autocomplete: 'off', maxlength: 128 });
  const email = input({ type: 'email', name: 'email', autocomplete: 'off', maxlength: 254 });
  const fu = field({ label: t('adm.user.username'), input: uname, hint: t('adm.user.usernameHint'), required: true });
  const fd = field({ label: t('adm.user.displayName'), input: dname });
  const fe = field({ label: t('adm.user.email'), input: email });
  const roles = assignmentEditor([{ role: 'viewer', scope: { type: 'global' } }]);
  const pwMode = 'pwmode';
  const genRadio = h('input', { type: 'radio', name: pwMode, value: 'generate', checked: true });
  const ownRadio = h('input', { type: 'radio', name: pwMode, value: 'provide' });
  const pw = input({ type: 'password', name: 'new-password', autocomplete: 'new-password' });
  const fpw = field({ label: t('adm.user.initialPassword'), input: pw, hint: t('adm.user.passwordRules') });
  fpw.hidden = true;
  for (const r of [genRadio, ownRadio]) r.addEventListener('change', () => (fpw.hidden = !ownRadio.checked));
  const ok = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('adm.user.create'));
  const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
  const d = openDialog({
    title: t('adm.user.createTitle'),
    size: 'md',
    body: h(
      'div',
      { class: 'k-stack k-stack--sm' },
      h('p', null, t('adm.user.createLead')),
      errBox,
      fu,
      fd,
      fe,
      roles.el,
      h(
        'fieldset',
        null,
        h('legend', null, t('adm.user.passwordLegend')),
        h('div', { class: 'ds-radio-group' }, h('label', { class: 'ds-radio' }, genRadio, t('adm.user.pwGenerate')), h('label', { class: 'ds-radio' }, ownRadio, t('adm.user.pwProvide'))),
        fpw,
        h('p', { class: 'caption' }, t('adm.user.mustChange')),
      ),
    ),
    actions: [cancel, ok],
    initialFocus: uname,
  });
  cancel.addEventListener('click', () => d.close());
  ok.addEventListener('click', async () => {
    mount(errBox);
    const name = uname.value.trim();
    fu.setError(/^[A-Za-z0-9._@-]{1,64}$/.test(name) ? '' : t('adm.user.usernameInvalid'));
    if (!/^[A-Za-z0-9._@-]{1,64}$/.test(name)) return uname.focus();
    if (ownRadio.checked && !pw.value) {
      fpw.setError(t('pw.newRequired'));
      return pw.focus();
    }
    fpw.setError('');
    const body = {
      username: name,
      display_name: dname.value.trim() || undefined,
      email: email.value.trim() || undefined,
      roles: roles.value().map(assignmentKey),
    };
    if (ownRadio.checked) body.password = pw.value;
    setBusy(ok, true, t('common.working'));
    let res;
    try {
      res = await api('/api/v1/users', { method: 'POST', body });
    } catch (err) {
      setBusy(ok, false, t('common.working'));
      const p = err.details && err.details.parameter;
      if (err.code === 'invalid_parameter' && p === 'username') {
        fu.setError(err.message);
        return uname.focus();
      }
      if (err.code === 'invalid_parameter' && p === 'password') {
        fpw.setError(err.message);
        return pw.focus();
      }
      const a = errorAlert(err, { title: t('adm.user.createFailed') });
      mount(errBox, a);
      a.focus();
      return;
    }
    pw.value = '';
    d.close();
    if (res.initial_password) {
      const secret = res.initial_password;
      res.initial_password = undefined;
      await showSecretOnce({
        title: t('adm.user.createdTitle'),
        lead: t('adm.user.created', { u: res.username }),
        label: t('adm.user.oneTimePassword'),
        secret,
        note: t('adm.user.handOver'),
        ackText: t('adm.secret.ackNoted'),
      });
    } else {
      toast({ kind: 'success', title: t('adm.user.created', { u: res.username }), desc: t('adm.user.mustChange') });
    }
    onCreated(res.username);
  });
}

function rolesView(root) {
  const body = h('div', null, skeletonBlock(6));
  mount(root, pageHead({ title: t('adm.users.title') }), adminTabs('roles'), body);
  (async () => {
    try {
      const d = await api('/api/v1/roles');
      const roles = d.roles || [];
      mount(
        body,
        card({
          title: t('adm.roles.matrixTitle'),
          body: [
            h('p', { class: 'caption' }, t('adm.roles.matrixNote')),
            h(
              'div',
              { class: 'k-scroll-x' },
              h(
                'table',
                { class: 'ds-table k-matrix' },
                h('caption', { class: 'ds-visually-hidden' }, t('adm.roles.matrixTitle')),
                h(
                  'thead',
                  null,
                  h(
                    'tr',
                    null,
                    h('th', { scope: 'col' }, t('adm.roles.permission')),
                    roles.map((r) => h('th', { scope: 'col' }, roleLabel(r.role), h('span', { class: 'k-host-id' }, t('adm.roles.members', { n: fmt.num(r.members) })))),
                  ),
                ),
                h(
                  'tbody',
                  null,
                  (d.permissions || []).map((p) =>
                    h(
                      'tr',
                      null,
                      h('th', { scope: 'row' }, permissionLabel(p)),
                      roles.map((r) =>
                        r.permissions.includes(p)
                          ? h('td', null, icon('check'), h('span', { class: 'ds-visually-hidden' }, t('adm.roles.has')))
                          : h('td', null, h('span', { 'aria-hidden': 'true' }, '—'), h('span', { class: 'ds-visually-hidden' }, t('adm.roles.hasNot'))),
                      ),
                    ),
                  ),
                ),
              ),
            ),
            h('p', { class: 'caption' }, t('adm.roles.scopeNote')),
          ],
        }),
      );
    } catch (err) {
      mount(body, errorState(err, { title: t('adm.roles.loadFailed') }));
    }
  })();
  return {};
}

export function render(ctx) {
  if (ctx.params.view === 'roles' || location.pathname === '/admin/roles') return rolesView(ctx.root);
  const root = ctx.root;
  const q = ctx.query;
  const st = { q: q.get('q') || '', source: q.get('source') || '', status: q.get('status') || '', role: q.get('role') || '', mfa: q.get('mfa') || '' };
  const canWrite = session.can('users:write');
  const totalMeta = h('span');
  const search = h('input', { class: 'ds-input', type: 'search', name: 'q', value: st.q, autocomplete: 'off', spellcheck: 'false', placeholder: t('adm.users.searchPlaceholder') });
  const searchField = field({ label: t('adm.users.search'), input: search });
  searchField.classList.add('ds-filterbar__search');
  let timer = null;
  search.addEventListener('input', () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      st.q = search.value.trim();
      load();
    }, 300);
  });
  const sel = (key, label, options) => {
    const s = h(
      'select',
      { class: 'ds-select', name: key, on: { change: () => { st[key] = s.value; load(); } } },
      h('option', { value: '' }, t('adm.filter.all')),
      options.map(([v, text]) => h('option', { value: v, selected: st[key] === v }, text)),
    );
    return h('label', { class: 'k-labelled-select' }, label, s);
  };
  const selects = h(
    'div',
    { class: 'k-filter-selects' },
    sel('source', t('adm.users.col.source'), [['local', t('adm.user.source.local')], ['ldap', t('adm.user.source.ldap')]]),
    sel('role', t('adm.users.col.roles'), ROLES.map((r) => [r, roleLabel(r)])),
    sel('status', t('adm.users.col.status'), [['active', t('adm.user.status.active')], ['disabled', t('adm.user.status.disabled')], ['locked', t('adm.user.status.locked')]]),
    sel('mfa', t('adm.users.col.mfa'), [['totp', t('adm.user.mfa.totp')], ['none', t('adm.user.mfa.none')], ['directory', t('adm.user.mfa.directory')]]),
  );
  const tbody = h('tbody');
  const region = h('div');
  const table = h(
    'div',
    { class: 'ds-table-wrap' },
    h(
      'table',
      { class: 'ds-table ds-table--two-line' },
      h('caption', { class: 'ds-visually-hidden' }, t('adm.users.caption')),
      h(
        'thead',
        null,
        h(
          'tr',
          null,
          ['name', 'username', 'source', 'roles', 'mfa', 'lastLogin', 'status'].map((k) =>
            h('th', { scope: 'col' }, k === 'lastLogin' ? t('adm.users.col.lastLogin', { tz: fmt.tzLabel() }) : t(`adm.users.col.${k}`)),
          ),
        ),
      ),
      tbody,
    ),
  );
  const createBtn = canWrite
    ? h('button', { type: 'button', class: 'ds-button ds-button--primary', on: { click: () => createUserDialog((u) => navigate(`/admin/users/${encodeURIComponent(u)}`)) } }, icon('plus'), t('adm.users.create'))
    : null;
  mount(
    root,
    pageHead({ title: t('adm.users.title'), meta: totalMeta, actions: createBtn }),
    adminTabs('users'),
    h('p', { class: 'caption' }, t('adm.users.ldapNote')),
    h('section', { class: 'ds-filterbar', 'aria-label': t('adm.users.filters') }, h('div', { class: 'ds-filterbar__row' }, searchField), selects),
    h('div', { class: 'k-table-card' }, region),
  );

  let seq = 0;
  async function load() {
    const my = ++seq;
    replaceQuery({ q: st.q, source: st.source, status: st.status, role: st.role, mfa: st.mfa });
    mount(region, skeletonBlock(5));
    try {
      const d = await api(`/api/v1/users${qs({ q: st.q, source: st.source, status: st.status, role: st.role })}`);
      if (my !== seq) return;
      const users = (d.users || []).filter((u) => !st.mfa || u.mfa === st.mfa);
      totalMeta.textContent = t('adm.users.count', { n: fmt.num(users.length) });
      if (!users.length) {
        const filtered = Boolean(st.q || st.source || st.status || st.role || st.mfa);
        mount(
          region,
          emptyState({
            title: filtered ? t('adm.users.emptyFiltered') : t('adm.users.empty'),
            actions: filtered
              ? h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => { Object.assign(st, { q: '', source: '', status: '', role: '', mfa: '' }); search.value = ''; for (const s of selects.querySelectorAll('select')) s.value = ''; load(); } } }, t('adm.filter.clear'))
              : null,
          }),
        );
        return;
      }
      mount(
        tbody,
        ...users.map((u) =>
          h(
            'tr',
            { dataset: { key: u.username } },
            h('td', null, h('a', { class: 'k-host-name', href: `/admin/users/${encodeURIComponent(u.username)}` }, u.display_name || u.username)),
            h('td', null, h('span', { class: 'mono' }, u.username)),
            h('td', null, sourceText(u.source)),
            h('td', null, u.roles && u.roles.length ? u.roles.map((a) => h('span', { class: 'k-block' }, assignmentText(a))) : h('span', { class: 'muted' }, t('adm.roles.none'))),
            h('td', null, mfaText(u)),
            h('td', null, fmt.timeEl(u.last_login_at)),
            h('td', null, userStatusBadge(u.status), u.must_change_password ? [' ', neutralBadge(t('adm.user.mustChangeBadge'))] : null),
          ),
        ),
      );
      mount(region, table);
    } catch (err) {
      if (my !== seq) return;
      totalMeta.textContent = '';
      mount(region, errorState(err, { title: t('adm.users.loadFailed'), onRetry: load }));
    }
  }
  load();
  return { dispose() { clearTimeout(timer); } };
}
