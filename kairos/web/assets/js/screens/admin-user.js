// S16 使用者詳情（/admin/users/:username；設計包 screens.md S16 的「使用者詳情」，以獨立頁面取代 Drawer）。
//
// API：GET /api/v1/users/{u}（有效權限、工作階段、擁有的 API token）、GET /api/v1/roles（權限差異）、
// PUT …/roles（L1，顯示權限差異）、POST …/disable（L1）／enable／unlock、POST …/password（L1；一次性密碼只顯示一次）、
// POST …/mfa/reset（L1）、POST …/sessions/revoke-all（L1，D25）、POST …/delete（L2，輸入帳號）。
//
// 防呆（S16）：不能停用或刪除自己；最後一位全站管理員（啟用中、有全站管理員個人指派）不能停用、刪除或移除管理員；
// 對象是自己時「結束其他所有工作階段」（保留目前這個，與個人設定相同）。目錄服務帳號沒有重設密碼與重設兩步驟驗證。

import { h, icon, mount, announce } from '../core/dom.js';
import { t, permissionLabel } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { navigate } from '../core/router.js';
import { pageHead, card, dl, errorState, errorAlert, field, input, inlineAlert, statusBadge, skeletonBlock, neutralBadge } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { short } from '../core/errors.js';
import { assignmentText, assignmentKey, confirmL1, confirmL2, showSecretOnce, myUsername, permissionDiff, disabledButton } from './admin-common.js';
import { userStatusBadge, mfaText, sourceText, assignmentEditor } from './admin-users.js';

function isGlobalAdmin(u) {
  return (u.roles || []).some((a) => a.role === 'admin' && (!a.scope || a.scope.type !== 'group'));
}

function diffList(diff) {
  const block = (title, list) =>
    list.length ? [h('p', { class: 'strong' }, title), h('ul', null, list.map((p) => h('li', null, permissionLabel(p))))] : null;
  if (!diff.added.length && !diff.removed.length) return h('p', null, t('adm.user.rolesNoPermChange'));
  return h('div', null, block(t('adm.user.permAdded'), diff.added), block(t('adm.user.permRemoved'), diff.removed));
}

function deviceText(s) {
  const ua = s.user_agent || '';
  const browser = /Edg\//.test(ua) ? 'Edge' : /Firefox\//.test(ua) ? 'Firefox' : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : ua ? ua.slice(0, 40) : t('account.unknownDevice');
  return browser;
}

export function render(ctx) {
  const root = ctx.root;
  const username = ctx.params.username;
  const canWrite = session.can('users:write');
  const canRevoke = session.can('sessions:revoke');
  let roleMatrix = null;
  let adminCount = null;

  async function load(focus) {
    mount(root, pageHead({ title: username, crumbs: [{ text: t('adm.users.title'), href: '/admin/users' }, { text: username }] }), skeletonBlock(8));
    let u;
    try {
      const [user, roles, users] = await Promise.all([
        api(`/api/v1/users/${encodeURIComponent(username)}`),
        roleMatrix ? Promise.resolve({ roles: roleMatrix }) : api('/api/v1/roles'),
        canWrite ? api('/api/v1/users') : Promise.resolve(null),
      ]);
      u = user;
      roleMatrix = roles.roles;
      if (users) adminCount = (users.users || []).filter((x) => x.status === 'active' && isGlobalAdmin(x)).length;
    } catch (err) {
      if (err.status === 404) {
        mount(root, pageHead({ title: t('adm.user.notFound'), crumbs: [{ text: t('adm.users.title'), href: '/admin/users' }, { text: username }] }), h('p', null, t('adm.user.notFoundBody')), h('a', { class: 'ds-button ds-button--secondary', href: '/admin/users' }, icon('chevron-left'), t('adm.user.backToList')));
        return;
      }
      mount(root, pageHead({ title: username, crumbs: [{ text: t('adm.users.title'), href: '/admin/users' }, { text: username }] }), errorState(err, { title: t('adm.user.loadFailed'), onRetry: () => load() }));
      return;
    }
    draw(u);
    if (focus) ctx.focusTitle();
  }

  function draw(u) {
    const self = myUsername() === u.username;
    const local = u.source === 'local';
    const lastAdmin = isGlobalAdmin(u) && u.status === 'active' && adminCount === 1;
    ctx.setTitle(u.display_name || u.username);

    // ---- 動作 ----
    const actions = [];
    if (canWrite) {
      actions.push(h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => editProfile(u) } }, icon('edit'), t('adm.user.editProfile')));
      if (u.status === 'locked') actions.push(h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => unlock(u) } }, icon('lock'), t('adm.user.unlock')));
      if (local) actions.push(h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => resetPassword(u) } }, icon('key'), t('adm.user.resetPassword')));
      if (local && u.mfa === 'totp') actions.push(h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => resetMfa(u) } }, t('adm.user.resetMfa')));
    }
    const dangerActions = [];
    if (canWrite) {
      if (u.status === 'disabled') {
        dangerActions.push(h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => enable(u) } }, t('adm.user.enable')));
      } else if (self) {
        dangerActions.push(...disabledButton(t('adm.user.disable'), t('adm.user.cannotSelf'), { variant: 'danger-secondary' }));
      } else if (lastAdmin) {
        dangerActions.push(...disabledButton(t('adm.user.disable'), t('adm.user.lastAdminWhy'), { variant: 'danger-secondary' }));
      } else {
        dangerActions.push(h('button', { type: 'button', class: 'ds-button ds-button--danger-secondary', on: { click: () => disable(u) } }, t('adm.user.disable')));
      }
      if (!self && !lastAdmin) {
        dangerActions.push(h('button', { type: 'button', class: 'ds-button ds-button--danger-secondary', on: { click: () => del(u) } }, icon('trash'), t('adm.user.delete')));
      } else {
        // 自己或最後一位全站管理員：「刪除」與「停用」同樣以 aria-disabled 顯示，共用同一段原因（screens.md S16）
        const why = self ? t('adm.user.cannotSelf') : t('adm.user.lastAdminWhy');
        const note = dangerActions.find((el) => el.tagName === 'P' && el.id);
        const del2 = h('button', { type: 'button', class: 'ds-button ds-button--danger-secondary', 'aria-disabled': 'true', 'aria-describedby': note ? note.id : null }, icon('trash'), t('adm.user.delete'));
        del2.addEventListener('click', () => announce(why));
        if (note) dangerActions.splice(dangerActions.indexOf(note), 0, del2);
        else dangerActions.push(del2);
      }
    }

    const profile = card({
      title: t('adm.user.profile'),
      body: dl([
        [t('adm.user.username'), h('span', { class: 'mono' }, u.username)],
        [t('adm.user.displayName'), u.display_name || '—'],
        [t('adm.user.email'), u.email || '—'],
        [t('adm.users.col.source'), [sourceText(u.source), u.ldap_dn ? h('span', { class: 'k-host-id mono' }, u.ldap_dn) : null]],
        [t('adm.users.col.status'), [userStatusBadge(u.status), u.must_change_password ? [' ', neutralBadge(t('adm.user.mustChangeBadge'))] : null, u.locked_until ? h('span', { class: 'k-host-id' }, t('adm.user.lockedUntil', { t: `${fmt.dateTime(u.locked_until)} ${fmt.tzLabel()}` })) : null]],
        u.status === 'disabled' ? [t('adm.user.disabledBy'), [u.disabled_by || '—', u.disabled_at ? `（${fmt.dateTime(u.disabled_at)} ${fmt.tzLabel()}）` : '', u.disable_reason ? h('span', { class: 'k-host-id' }, t('adm.user.reasonIs', { r: u.disable_reason })) : null]] : null,
        [t('adm.users.col.mfa'), mfaText(u)],
        [t('adm.user.lastLogin'), u.last_login_at ? [fmt.timeEl(u.last_login_at), u.last_login_ip ? h('span', { class: 'k-host-id mono' }, u.last_login_ip) : null] : t('adm.user.neverLoggedIn')],
        [t('adm.user.createdAt'), `${fmt.dateTime(u.created_at)} ${fmt.tzLabel()}${u.created_by ? ` · ${u.created_by}` : ''}`],
      ]),
    });

    const roleCard = card({
      title: t('adm.user.roles'),
      actions: canWrite ? h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => editRoles(u, lastAdmin) } }, icon('edit'), t('adm.user.editRoles')) : null,
      body: [
        u.roles && u.roles.length ? h('ul', null, u.roles.map((a) => h('li', null, assignmentText(a)))) : h('p', { class: 'muted' }, t('adm.roles.none')),
        u.source === 'ldap' ? h('p', { class: 'caption' }, t('adm.user.ldapGroupsNote')) : null,
        u.roles && u.roles.some((a) => a.scope && a.scope.type === 'group') ? h('p', { class: 'caption' }, t('adm.roles.groupHint')) : null,
        h('h3', { class: 'k-section-title' }, t('adm.user.effective')),
        u.permissions && u.permissions.length
          ? h('ul', { class: 'k-perm-list' }, u.permissions.map((p) => h('li', null, permissionLabel(p))))
          : h('p', { class: 'muted' }, t('adm.user.noGlobalPerms')),
      ],
    });

    // ---- 工作階段 ----
    const sessions = (u.sessions || []).filter((s) => s.active !== false);
    let sessBtn = null;
    if (self && session.isSession()) {
      const others = Math.max(0, sessions.length - 1);
      sessBtn = others
        ? h('button', { type: 'button', class: 'ds-button ds-button--danger-secondary ds-button--sm', 'aria-describedby': 'self-sess-note', on: { click: () => endOthers(others) } }, t('account.endOthers'))
        : disabledButton(t('account.endOthers'), t('adm.user.onlyCurrent'), { variant: 'danger-secondary', size: 'sm' });
    } else if (canRevoke) {
      sessBtn = sessions.length
        ? h('button', { type: 'button', class: 'ds-button ds-button--danger-secondary ds-button--sm', on: { click: () => endAll(u) } }, t('adm.user.endAll'))
        : null;
    }
    const sessCard = card({
      title: t('adm.user.sessions', { n: sessions.length }),
      actions: Array.isArray(sessBtn) ? sessBtn[0] : sessBtn,
      body: [
        Array.isArray(sessBtn) ? sessBtn[1] : null,
        self ? h('p', { class: 'caption', id: 'self-sess-note' }, t('adm.user.selfSessionsNote')) : null,
        sessions.length
          ? h(
              'div',
              { class: 'k-scroll-x' },
              h(
                'table',
                { class: 'k-mini-table' },
                h('caption', { class: 'ds-visually-hidden' }, t('adm.user.sessions', { n: sessions.length })),
                h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('account.device')), h('th', { scope: 'col' }, t('col.ip')), h('th', { scope: 'col' }, t('account.signedIn', { tz: fmt.tzLabel() })), h('th', { scope: 'col' }, t('account.lastActiveCol')))),
                h('tbody', null, sessions.map((s) => h('tr', null, h('td', null, deviceText(s), h('span', { class: 'k-host-id' }, s.method === 'ldap' ? t('account.sourceLdap') : t('account.sourceLocal'))), h('td', null, s.ip ? h('span', { class: 'mono' }, s.ip) : '—'), h('td', null, fmt.dateTime(s.created_at)), h('td', null, fmt.timeEl(s.last_seen_at))))),
              ),
            )
          : h('p', { class: 'muted' }, t('adm.user.noSessions')),
      ],
    });

    const tokens = u.api_tokens || [];
    const tokenCard = card({
      title: t('adm.user.tokens'),
      body: [
        tokens.length
          ? h('ul', null, tokens.map((x) => h('li', null, h('span', { class: 'mono' }, `kat_${x.token_id.slice(0, 4)}…`), ` ${x.name} · `, t(`adm.token.state.${x.state}`))))
          : h('p', { class: 'muted' }, t('adm.user.noTokens')),
        session.can('api_tokens:read') ? h('p', null, h('a', { href: '/system/settings/integrations' }, t('adm.user.manageTokens'))) : null,
      ],
    });

    mount(
      root,
      pageHead({
        title: u.display_name || u.username,
        crumbs: [{ text: t('adm.users.title'), href: '/admin/users' }, { text: u.display_name || u.username }],
        meta: self ? statusBadge('info', t('adm.user.thisIsYou')) : null,
        actions: session.can('audit:read') ? h('a', { class: 'ds-button ds-button--ghost', href: `/audit?target=${encodeURIComponent(u.username)}` }, icon('audit'), t('adm.user.viewAudit')) : null,
      }),
      actions.length ? h('div', { class: 'ds-button-row k-page-actions' }, actions) : null,
      h('div', { class: 'k-settings-grid' }, profile, roleCard, sessCard, tokenCard, dangerActions.length ? card({ title: t('adm.user.dangerZone'), body: h('div', { class: 'ds-button-row' }, dangerActions) }) : null),
    );
  }

  // ---- 操作 ----
  async function run(fn, okTitle, failTitle, errBox) {
    try {
      const r = await fn();
      if (okTitle) toast({ kind: 'success', title: typeof okTitle === 'function' ? okTitle(r) : okTitle });
      return r;
    } catch (err) {
      if (errBox) {
        const a = errorAlert(err, { title: failTitle });
        mount(errBox, a);
        a.focus();
      } else {
        toast({ kind: 'error', title: short(err, { title: failTitle }) });
      }
      throw err;
    }
  }

  function editProfile(u) {
    const errBox = h('div');
    const dn = input({ name: 'display_name', value: u.display_name || '', maxlength: 128 });
    const em = input({ type: 'email', name: 'email', value: u.email || '', maxlength: 254 });
    const ok = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('adm.save'));
    const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
    const d = openDialog({ title: t('adm.user.editProfileTitle'), size: 'sm', body: h('div', null, errBox, field({ label: t('adm.user.displayName'), input: dn }), field({ label: t('adm.user.email'), input: em })), actions: [cancel, ok], initialFocus: dn });
    cancel.addEventListener('click', () => d.close());
    ok.addEventListener('click', async () => {
      try {
        await run(() => api(`/api/v1/users/${encodeURIComponent(u.username)}`, { method: 'PUT', body: { display_name: dn.value.trim() || null, email: em.value.trim() || null } }), t('adm.saved'), t('adm.user.saveFailed'), errBox);
        d.close();
        load(true);
      } catch {
        /* 顯示在對話框 */
      }
    });
  }

  function editRoles(u, lastAdmin) {
    const editor = assignmentEditor(u.roles || []);
    const errBox = h('div');
    const ok = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('adm.next'));
    const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
    const d = openDialog({
      title: t('adm.user.editRolesTitle', { u: u.display_name || u.username }),
      size: 'md',
      body: h('div', { class: 'k-stack k-stack--sm' }, errBox, lastAdmin ? inlineAlert({ kind: 'info', title: t('adm.user.lastAdminWhy') }) : null, editor.el),
      actions: [cancel, ok],
    });
    cancel.addEventListener('click', () => d.close());
    ok.addEventListener('click', async () => {
      const next = editor.value();
      const before = (u.roles || []).map(assignmentKey).sort().join(',');
      if (next.map(assignmentKey).sort().join(',') === before) {
        d.close();
        return;
      }
      if (lastAdmin && !next.some((a) => a.role === 'admin' && a.scope.type !== 'group')) {
        mount(errBox, inlineAlert({ kind: 'error', title: t('adm.user.lastAdminWhy') }));
        return;
      }
      d.close();
      const diff = permissionDiff(u.roles || [], next, roleMatrix);
      await confirmL1({
        title: t('adm.user.rolesConfirmTitle', { u: u.display_name || u.username }),
        body: h(
          'div',
          null,
          h('p', null, t('adm.user.rolesConfirmBody', { before: (u.roles || []).map(assignmentText).join('、') || t('adm.roles.none'), after: next.map(assignmentText).join('、') || t('adm.roles.none') })),
          diffList(diff),
        ),
        confirmText: t('adm.user.rolesConfirm'),
        onConfirm: async ({ reason, errBox: eb }) => {
          try {
            const r = await run(() => api(`/api/v1/users/${encodeURIComponent(u.username)}/roles`, { method: 'PUT', body: { roles: next.map(assignmentKey), reason: reason || undefined } }), null, t('adm.user.rolesFailed'), eb);
            toast({ kind: 'success', title: t('adm.user.rolesChanged'), desc: t('adm.user.rolesChangedDesc', { a: (r.permissions_added || []).length, r: (r.permissions_removed || []).length }) });
            load(true);
            return true;
          } catch {
            return false;
          }
        },
      });
    });
  }

  function disable(u) {
    confirmL1({
      title: t('adm.user.disableTitle', { u: u.display_name || u.username }),
      body: t('adm.user.disableBody'),
      confirmText: t('adm.user.disableConfirm'),
      danger: true,
      onConfirm: async ({ reason, errBox }) => {
        try {
          await run(() => api(`/api/v1/users/${encodeURIComponent(u.username)}/disable`, { method: 'POST', body: { reason: reason || undefined } }), (r) => t('adm.user.disabled', { u: u.username, n: r.sessions_ended }), t('adm.user.actionFailed'), errBox);
          load(true);
          return true;
        } catch {
          return false;
        }
      },
    });
  }

  async function enable(u) {
    try {
      await run(() => api(`/api/v1/users/${encodeURIComponent(u.username)}/enable`, { method: 'POST' }), t('adm.user.enabled', { u: u.username }), t('adm.user.actionFailed'));
      load(true);
    } catch {
      /* toast */
    }
  }

  async function unlock(u) {
    try {
      await run(() => api(`/api/v1/users/${encodeURIComponent(u.username)}/unlock`, { method: 'POST' }), t('adm.user.unlocked', { u: u.username }), t('adm.user.actionFailed'));
      load(true);
    } catch {
      /* toast */
    }
  }

  function resetPassword(u) {
    let secret = null;
    confirmL1({
      title: t('adm.user.resetPasswordTitle', { u: u.display_name || u.username }),
      body: t('adm.user.resetPasswordBody'),
      confirmText: t('adm.user.resetPasswordConfirm'),
      reason: null,
      onConfirm: async ({ errBox }) => {
        try {
          const r = await run(() => api(`/api/v1/users/${encodeURIComponent(u.username)}/password`, { method: 'POST', body: {} }), null, t('adm.user.actionFailed'), errBox);
          secret = r.password || null;
          r.password = undefined;
          return true;
        } catch {
          return false;
        }
      },
    }).then(async (res) => {
      if (!res) return;
      if (secret) {
        const s = secret;
        secret = null;
        await showSecretOnce({ title: t('adm.user.resetDoneTitle'), lead: t('adm.user.resetDone', { u: u.username }), label: t('adm.user.oneTimePassword'), secret: s, note: t('adm.user.handOver'), ackText: t('adm.secret.ackNoted') });
      }
      load(true);
    });
  }

  function resetMfa(u) {
    confirmL1({
      title: t('adm.user.resetMfaTitle', { u: u.display_name || u.username }),
      body: t('adm.user.resetMfaBody'),
      confirmText: t('adm.user.resetMfaConfirm'),
      danger: true,
      onConfirm: async ({ reason, errBox }) => {
        try {
          await run(() => api(`/api/v1/users/${encodeURIComponent(u.username)}/mfa/reset`, { method: 'POST', body: { reason: reason || undefined } }), t('adm.user.mfaResetDone'), t('adm.user.actionFailed'), errBox);
          load(true);
          return true;
        } catch {
          return false;
        }
      },
    });
  }

  function endAll(u) {
    const name = u.display_name || u.username;
    confirmL1({
      title: t('adm.user.endAllTitle', { u: name }),
      body: t('adm.user.endAllBody'),
      confirmText: t('adm.user.endAllConfirm'),
      danger: true,
      onConfirm: async ({ reason, errBox }) => {
        try {
          await run(() => api(`/api/v1/users/${encodeURIComponent(u.username)}/sessions/revoke-all`, { method: 'POST', body: { reason: reason || undefined } }), t('adm.user.endAllDone', { u: name }), t('account.endFailed'), errBox);
          load(true);
          return true;
        } catch {
          return false;
        }
      },
    });
  }

  function endOthers(n) {
    confirmL1({
      title: t('account.endOthersTitle', { n }),
      body: t('account.keepCurrent'),
      confirmText: t('account.endOthers').replace('…', ''),
      danger: true,
      reason: null,
      onConfirm: async ({ errBox }) => {
        try {
          await run(() => api('/api/v1/me/sessions/revoke-others', { method: 'POST' }), (r) => t('account.endedOthers', { n: r.ended }), t('account.endFailed'), errBox);
          load(true);
          return true;
        } catch {
          return false;
        }
      },
    });
  }

  function del(u) {
    confirmL2({
      title: t('adm.user.deleteTitle', { u: u.display_name || u.username }),
      lead: t('adm.user.deleteLead'),
      body: t('adm.user.deleteBody'),
      phrase: u.username,
      confirmText: t('adm.user.deleteConfirm'),
      onConfirm: async ({ reason, errBox }) => {
        try {
          await run(() => api(`/api/v1/users/${encodeURIComponent(u.username)}/delete`, { method: 'POST', body: { confirm: u.username, reason: reason || undefined } }), t('adm.user.deleted', { u: u.username }), t('adm.user.actionFailed'), errBox);
          navigate('/admin/users');
          return true;
        } catch {
          return false;
        }
      },
    });
  }

  load(false);
  return {};
}
