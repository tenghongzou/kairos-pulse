// API token（S21「整合」；設計包 screens.md S21「API token」、API.md 第 12.6 節）。
//
// - 清單：GET /api/v1/api-tokens（預設只有有效的；「顯示已撤銷與已過期」加 all=true）。不含秘密；token 只顯示
//   前 4 碼（kat_xxxx…）。剩餘 14 天以內的有效 token 標示警告。舊範圍（0.4／0.5）註明「建議以角色重新建立」。
// - 建立（api_tokens:write）：角色（預設唯讀）或屬於使用者（可再選角色取交集）、有效期限（預設 90 天、最長 3,650 天，
//   不提供不過期）。完整的 token 只在建立後顯示一次（showSecretOnce），不進入 localStorage、網址或日誌。
// - 撤銷（api_tokens:write）：L1，原因選填，立即生效。

import { h, icon, mount } from '../core/dom.js';
import { t, roleLabel } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { card, emptyState, errorState, errorAlert, field, input, statusBadge, skeletonBlock, setBusy } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { ROLES, confirmL1, showSecretOnce } from './admin-common.js';

const DAY = 86400e3;
const EXPIRY_DAYS = [30, 90, 180, 365, 3650];

/** token 的提示：只顯示前 4 碼（設計包 D33 的原則；token_id 是公開的識別碼，秘密從不顯示） */
export function tokenHint(id) {
  return `kat_${String(id || '').slice(0, 4)}…`;
}

function stateBadge(tk) {
  if (tk.state === 'active') {
    const left = tk.expires_at ? Date.parse(tk.expires_at) - fmt.serverNow() : null;
    if (left != null && left <= 14 * DAY) return statusBadge('warning', t('adm.token.expiringSoon', { n: Math.max(0, Math.ceil(left / DAY)) }));
    return statusBadge('online', t('adm.token.state.active'));
  }
  return statusBadge('offline', t(`adm.token.state.${tk.state}`));
}

function grantText(tk) {
  if (tk.scope) return t('adm.token.grant.legacy', { s: tk.scope });
  if (tk.owner) return tk.role ? t('adm.token.grant.userRole', { u: tk.owner, r: roleLabel(tk.role) }) : t('adm.token.grant.user', { u: tk.owner });
  return t('adm.token.grant.role', { r: roleLabel(tk.role) });
}

export function tokensCard() {
  const canWrite = session.can('api_tokens:write');
  let showAll = false;
  const body = h('div', null, skeletonBlock(4));
  const allToggle = h('input', { type: 'checkbox', class: 'ds-check', name: 'all' });
  allToggle.addEventListener('change', () => {
    showAll = allToggle.checked;
    load();
  });
  const createBtn = canWrite ? h('button', { type: 'button', class: 'ds-button ds-button--primary ds-button--sm', on: { click: () => createDialog() } }, icon('plus'), t('adm.token.create')) : null;

  async function load() {
    mount(body, skeletonBlock(4));
    try {
      const d = await api(`/api/v1/api-tokens${qs({ all: showAll ? 'true' : '' })}`);
      const list = d.tokens || [];
      if (!list.length) {
        mount(body, emptyState({ title: showAll ? t('adm.token.emptyAll') : t('adm.token.empty'), body: canWrite ? t('adm.token.emptyBody') : null }));
        return;
      }
      const legacy = list.some((x) => x.scope);
      mount(
        body,
        h(
          'div',
          { class: 'k-scroll-x' },
          h(
            'table',
            { class: 'k-mini-table', id: 'api-tokens' },
            h('caption', { class: 'ds-visually-hidden' }, t('adm.token.title')),
            h(
              'thead',
              null,
              h(
                'tr',
                null,
                h('th', { scope: 'col' }, t('adm.token.col.token')),
                h('th', { scope: 'col' }, t('adm.token.col.name')),
                h('th', { scope: 'col' }, t('adm.token.col.grant')),
                h('th', { scope: 'col' }, t('adm.token.col.state')),
                h('th', { scope: 'col' }, t('adm.token.col.created', { tz: fmt.tzLabel() })),
                h('th', { scope: 'col' }, t('adm.token.col.expires', { tz: fmt.tzLabel() })),
                h('th', { scope: 'col' }, t('adm.token.col.lastUsed')),
                canWrite ? h('th', { scope: 'col' }, h('span', { class: 'ds-visually-hidden' }, t('account.actions'))) : null,
              ),
            ),
            h(
              'tbody',
              null,
              list.map((tk) =>
                h(
                  'tr',
                  { dataset: { token: tokenHint(tk.token_id) } },
                  h('td', null, h('span', { class: 'mono' }, tokenHint(tk.token_id))),
                  h('td', null, tk.name),
                  h('td', null, h('span', null, grantText(tk)), h('span', { class: 'k-host-id mono' }, tk.grant)),
                  h('td', null, stateBadge(tk), tk.state === 'revoked' && tk.revoked_by ? h('span', { class: 'k-host-id' }, t('adm.token.revokedBy', { by: tk.revoked_by })) : null),
                  h('td', null, fmt.dateTime(tk.created_at, { seconds: false }), tk.created_by ? h('span', { class: 'k-host-id' }, tk.created_by) : null),
                  h('td', null, tk.expires_at ? fmt.dateTime(tk.expires_at, { seconds: false }) : '—'),
                  h('td', null, tk.last_used_at ? [fmt.timeEl(tk.last_used_at), tk.last_used_ip ? h('span', { class: 'k-host-id mono' }, tk.last_used_ip) : null] : h('span', { class: 'muted' }, t('adm.token.neverUsed'))),
                  canWrite
                    ? h('td', null, tk.state === 'active' ? h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', 'aria-label': t('adm.token.revokeAria', { n: tk.name, h: tokenHint(tk.token_id) }), on: { click: () => revoke(tk) } }, t('adm.token.revoke')) : null)
                    : null,
                ),
              ),
            ),
          ),
        ),
        legacy ? h('p', { class: 'caption' }, t('adm.token.legacyNote')) : null,
        h('p', { class: 'caption' }, t('adm.token.lastUsedNote')),
      );
    } catch (err) {
      mount(body, errorState(err, { title: t('adm.token.loadFailed'), onRetry: load }));
    }
  }

  function revoke(tk) {
    confirmL1({
      title: t('adm.token.revokeTitle', { n: tk.name }),
      body: t('adm.token.revokeBody', { h: tokenHint(tk.token_id) }),
      confirmText: t('adm.token.revokeConfirm'),
      danger: true,
      onConfirm: async ({ reason, errBox }) => {
        try {
          await api(`/api/v1/api-tokens/${encodeURIComponent(tk.token_id)}/revoke`, { method: 'POST', body: { reason: reason || undefined } });
          toast({ kind: 'success', title: t('adm.token.revoked', { n: tk.name }) });
          load();
          return true;
        } catch (err) {
          const a = errorAlert(err, { title: t('adm.token.revokeFailed') });
          mount(errBox, a);
          a.focus();
          return false;
        }
      },
    });
  }

  function createDialog() {
    const errBox = h('div');
    const name = input({ name: 'name', maxlength: 64, autocomplete: 'off' });
    const fname = field({ label: t('adm.token.col.name'), input: name, hint: t('adm.token.nameHint'), required: true });
    const kindRole = h('input', { type: 'radio', name: 'grant-kind', value: 'role', checked: true });
    const kindUser = h('input', { type: 'radio', name: 'grant-kind', value: 'user' });
    const roleSel = h('select', { class: 'ds-select', name: 'role' }, ROLES.map((r) => h('option', { value: r, selected: r === 'viewer' }, roleLabel(r))));
    const froles = field({ label: t('adm.token.role'), input: roleSel, hint: t('adm.token.roleHint') });
    const owner = input({ name: 'owner', autocomplete: 'off', maxlength: 64 });
    const fowner = field({ label: t('adm.token.owner'), input: owner, hint: t('adm.token.ownerHint') });
    const interSel = h('select', { class: 'ds-select', name: 'intersect' }, h('option', { value: '' }, t('adm.token.noIntersect')), ROLES.map((r) => h('option', { value: r }, roleLabel(r))));
    const finter = field({ label: t('adm.token.intersect'), input: interSel });
    const userBox = h('div', { hidden: true }, fowner, finter);
    const sync = () => {
      froles.hidden = !kindRole.checked;
      userBox.hidden = !kindUser.checked;
    };
    kindRole.addEventListener('change', sync);
    kindUser.addEventListener('change', sync);
    const expSel = h('select', { class: 'ds-select', name: 'expires' }, EXPIRY_DAYS.map((n) => h('option', { value: String(n), selected: n === 90 }, t('adm.token.days', { n: fmt.num(n) }))));
    const fexp = field({ label: t('adm.token.expires'), input: expSel, hint: t('adm.token.expiresHint') });
    const ok = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('adm.token.createBtn'));
    const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
    const d = openDialog({
      title: t('adm.token.createTitle'),
      size: 'md',
      body: h(
        'div',
        null,
        errBox,
        fname,
        h('fieldset', null, h('legend', null, t('adm.token.grantLegend')), h('div', { class: 'ds-radio-group' }, h('label', { class: 'ds-radio' }, kindRole, t('adm.token.kindRole')), h('label', { class: 'ds-radio' }, kindUser, t('adm.token.kindUser')))),
        froles,
        userBox,
        fexp,
      ),
      actions: [cancel, ok],
      initialFocus: name,
    });
    cancel.addEventListener('click', () => d.close());
    ok.addEventListener('click', async () => {
      mount(errBox);
      const n = name.value.trim();
      fname.setError(n ? '' : t('adm.token.nameMissing'));
      if (!n) return name.focus();
      const body = { name: n, expires_in_secs: Number(expSel.value) * 86400 };
      if (kindUser.checked) {
        const o = owner.value.trim();
        fowner.setError(o ? '' : t('adm.token.ownerMissing'));
        if (!o) return owner.focus();
        body.owner = o;
        if (interSel.value) body.role = interSel.value;
      } else {
        body.role = roleSel.value;
      }
      setBusy(ok, true, t('common.working'));
      let res;
      try {
        res = await api('/api/v1/api-tokens', { method: 'POST', body });
      } catch (err) {
        setBusy(ok, false, t('common.working'));
        const p = err.details && err.details.parameter;
        if (err.code === 'invalid_parameter' && p === 'name') {
          fname.setError(err.message);
          return name.focus();
        }
        if (err.code === 'user_not_found' || err.code === 'user_disabled') {
          fowner.setError(err.code === 'user_not_found' ? t('adm.token.ownerNotFound') : t('adm.token.ownerDisabled'));
          return owner.focus();
        }
        const a = errorAlert(err, { title: t('adm.token.createFailed') });
        mount(errBox, a);
        a.focus();
        return;
      }
      d.close();
      const secret = res.token;
      res.token = undefined;
      await showSecretOnce({
        title: t('adm.token.createdTitle'),
        lead: t('adm.token.created', { n: res.name, g: grantText(res), e: `${fmt.dateTime(res.expires_at, { seconds: false })} ${fmt.tzLabel()}` }),
        label: t('adm.token.secretLabel'),
        secret,
        note: t('adm.token.secretNote'),
        extra: h('p', { class: 'caption' }, t('adm.token.usage', { origin: location.origin })),
        ackText: t('adm.token.ack'),
      });
      load();
    });
  }

  load();
  return card({
    title: t('adm.token.title'),
    id: 'api-tokens-title',
    actions: [h('label', { class: 'ds-checkbox' }, allToggle, t('adm.token.showAll')), createBtn].filter(Boolean),
    body: [h('p', { class: 'caption' }, t('adm.token.intro')), body],
  });
}


