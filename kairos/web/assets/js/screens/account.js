// S23 個人設定（設計包 kairos/screens.md S23「0.6 後端現況」；API.md 第 12.4 節）。
//
// 有後端：變更密碼（其他工作階段結束、保留目前這個）、兩步驟驗證（啟用：密碼 → QR code 與金鑰 → 驗證碼 →
// 10 組復原碼只顯示一次；停用：密碼＋驗證碼或復原碼）、我的工作階段（逐一結束、結束其他所有工作階段 L1，D25）。
// 只存在這個瀏覽器（沒有伺服器端的偏好儲存，畫面標示）：主題、減少動態效果、時間顯示、時區。
// 隱藏（§28.6）：我的通知；「我的 API token」改為一行說明。目錄服務帳號的密碼與兩步驟驗證由 AD 管理。

import { h, icon, uid, announce, mount } from '../core/dom.js';
import { t, roleLabel } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as prefs from '../core/prefs.js';
import * as fmt from '../core/format.js';
import { pageHead, card, dl, field, input, errorAlert, inlineAlert, setBusy, copyButton, statusBadge, errorState, skeletonBlock } from '../ui/components.js';
import { confirm, openDialog } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { short } from '../core/errors.js';
import { qrSvg } from '../ui/qr.js';

const TIMEZONES = ['Asia/Taipei', 'UTC', 'Asia/Tokyo', 'Asia/Shanghai', 'Asia/Singapore', 'Europe/London', 'America/New_York', 'America/Los_Angeles'];

function radioGroup(name, legend, options, value, onChange, hint) {
  const gname = uid(name);
  return h(
    'fieldset',
    null,
    h('legend', null, legend),
    h(
      'div',
      { class: 'ds-radio-group' },
      options.map(([v, label]) =>
        h(
          'label',
          { class: 'ds-radio' },
          h('input', { type: 'radio', name: gname, value: v, checked: v === value, on: { change: () => onChange(v) } }),
          label,
        ),
      ),
    ),
    hint ? h('p', { class: 'ds-field__hint' }, hint) : null,
  );
}

function profileCard(me) {
  const u = me.user;
  const roles = (me.roles || []).map((r) => {
    const scope = r.scope && r.scope.type === 'group' ? t('account.scopeGroup', { g: r.scope.group }) : t('account.scopeGlobal');
    const via = r.via_group ? t('account.viaGroup', { g: r.via_group }) : '';
    return `${roleLabel(r.role)}（${scope}）${via}`;
  });
  return card({
    title: t('account.profile'),
    body: dl([
      [t('account.username'), h('span', { class: 'mono' }, u.username)],
      [t('account.displayName'), u.display_name || '—'],
      [t('account.source'), u.source === 'ldap' ? t('account.sourceLdap') : t('account.sourceLocal')],
      [t('account.roles'), roles.length ? roles.join('；') : t('account.noRoles')],
      [t('account.sessionExpires'), `${fmt.dateTime(me.session.expires_at)} ${fmt.tzLabel()}`],
    ]),
  });
}

function appearanceCard() {
  const body = h('div');
  const draw = () => {
    mount(body, 
      inlineAlert({ kind: 'info', title: t('account.localOnly') }),
      radioGroup('theme', t('account.theme'), [['system', t('account.themeSystem')], ['light', t('account.themeLight')], ['dark', t('account.themeDark')]], prefs.get('theme'), (v) => {
        prefs.set('theme', v);
        prefs.applyAppearance();
        announce(t('account.saved'));
      }),
      radioGroup('motion', t('account.motion'), [['system', t('account.motionSystem')], ['reduce', t('account.motionReduce')]], prefs.get('motion'), (v) => {
        prefs.set('motion', v);
        prefs.applyAppearance();
        announce(t('account.saved'));
      }),
      radioGroup('timeDisplay', t('account.timeDisplay'), [['relative', t('account.timeRelative')], ['absolute', t('account.timeAbsolute')]], prefs.get('timeDisplay'), (v) => {
        prefs.set('timeDisplay', v);
        announce(t('account.saved'));
      }),
    );
    const tzSel = h(
      'select',
      { class: 'ds-select', name: 'timeZone' },
      TIMEZONES.map((z) => h('option', { value: z, selected: prefs.get('timeZone') === z }, z === 'Asia/Taipei' ? t('account.tzTaipei') : z)),
    );
    tzSel.addEventListener('change', () => {
      prefs.set('timeZone', tzSel.value);
      announce(t('account.saved'));
      draw();
    });
    body.appendChild(field({ label: t('account.timeZone'), input: tzSel, hint: t('account.timeZoneHint', { now: `${fmt.dateTime(fmt.serverNow())} ${fmt.tzLabel()}` }) }));
  };
  draw();
  return card({ title: t('account.appearance'), body });
}

function passwordCard(me) {
  if (me.user.source === 'ldap') {
    return card({ title: t('account.password'), body: h('p', null, t('account.ldapManaged')) });
  }
  const errBox = h('div');
  const cur = input({ type: 'password', name: 'current', autocomplete: 'current-password' });
  const nw = input({ type: 'password', name: 'new', autocomplete: 'new-password' });
  const nw2 = input({ type: 'password', name: 'confirm', autocomplete: 'new-password' });
  const f1 = field({ label: t('pw.current'), input: cur, required: true });
  const f2 = field({ label: t('pw.new'), input: nw, required: true });
  const f3 = field({ label: t('pw.confirm'), input: nw2, required: true });
  const submit = h('button', { type: 'submit', class: 'ds-button ds-button--primary' }, t('pw.submit'));
  const form = h(
    'form',
    { novalidate: true },
    errBox,
    f1,
    f2,
    h('ul', { class: 'k-password-rules' }, h('li', null, t('pw.rule.length')), h('li', null, t('pw.rule.username')), h('li', null, t('pw.rule.common'))),
    f3,
    h('p', { class: 'caption' }, t('account.passwordNote')),
    h('div', { class: 'ds-button-row' }, submit),
  );
  form.addEventListener('submit', async (ev) => {
    ev.preventDefault();
    mount(errBox);
    f1.setError(cur.value ? '' : t('pw.currentRequired'));
    f2.setError(nw.value ? '' : t('pw.newRequired'));
    f3.setError(nw.value === nw2.value ? '' : t('pw.mismatch'));
    if (!cur.value) return cur.focus();
    if (!nw.value) return nw.focus();
    if (nw.value !== nw2.value) return nw2.focus();
    setBusy(submit, true, t('pw.submitting'));
    try {
      const r = await api('/api/v1/me/password', { method: 'POST', body: { current_password: cur.value, new_password: nw.value } });
      cur.value = '';
      nw.value = '';
      nw2.value = '';
      toast({ kind: 'success', title: t('account.passwordChanged'), desc: t('account.otherEnded', { n: r.other_sessions_ended ?? 0 }) });
    } catch (err) {
      if (err.code === 'invalid_parameter' && err.details && err.details.parameter === 'new_password') {
        f2.setError(err.message);
        nw.focus();
      } else {
        const a = errorAlert(err, { title: t('pw.failed') });
        mount(errBox, a);
        a.focus();
      }
    } finally {
      setBusy(submit, false, t('pw.submitting'));
    }
  });
  return card({ title: t('account.password'), body: form });
}

function mfaCard(me, reload) {
  if (me.user.source === 'ldap') {
    return card({ title: t('account.mfa'), body: h('p', null, t('account.ldapMfa')) });
  }
  // /api/v1/me 的 user.mfa 為布林（每個請求依資料庫重新判定）
  const enabled = me.user.mfa === true || me.user.mfa === 'totp';
  const btn = h(
    'button',
    { type: 'button', class: ['ds-button', enabled ? 'ds-button--danger-secondary' : 'ds-button--primary'] },
    enabled ? t('account.mfaDisable') : t('account.mfaEnable'),
  );
  btn.addEventListener('click', () => (enabled ? disableMfa(reload) : enableMfa(reload)));
  return card({
    title: t('account.mfa'),
    body: [
      h('p', { class: 'k-row' }, enabled ? statusBadge('online', t('account.mfaOn')) : statusBadge('unknown', t('account.mfaOff'))),
      h('p', null, enabled ? t('account.mfaOnBody') : t('account.mfaOffBody')),
      h('div', { class: 'ds-button-row' }, btn),
    ],
  });
}

/** 啟用：密碼 → QR code 與金鑰 → 驗證碼 → 復原碼（只顯示一次） */
function enableMfa(reload) {
  const errBox = h('div');
  const body = h('div');
  const nextBtn = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('account.mfaNext'));
  const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
  const d = openDialog({
    title: t('account.mfaEnableTitle'),
    size: 'md',
    body: h('div', null, errBox, body),
    actions: [cancel, nextBtn],
    onClose: () => {
      if (step === 'codes') reload();
    },
  });
  cancel.addEventListener('click', () => d.close());
  let step = 'password';
  const pw = input({ type: 'password', name: 'password', autocomplete: 'current-password' });
  const pwf = field({ label: t('account.mfaPassword'), input: pw, required: true });
  const code = input({ name: 'code', autocomplete: 'one-time-code', inputmode: 'numeric', maxlength: 6, cls: 'k-otp-input' });
  const codef = field({ label: t('login.mfaCode'), input: code, hint: t('account.mfaCodeHint'), required: true });
  mount(body, h('p', null, t('account.mfaStep1')), pwf);
  pw.focus();
  const run = async () => {
    mount(errBox);
    setBusy(nextBtn, true, t('common.working'));
    try {
      if (step === 'password') {
        if (!pw.value) {
          pwf.setError(t('pw.currentRequired'));
          pw.focus();
          return;
        }
        const s = await api('/api/v1/me/totp/setup', { method: 'POST', body: { password: pw.value } });
        pw.value = '';
        step = 'code';
        const grouped = s.secret.replace(/(.{4})/g, '$1 ').trim();
        let qr;
        try {
          qr = qrSvg(s.otpauth_uri, t('account.qrLabel'));
        } catch {
          qr = null;
        }
        mount(body, 
          h('p', null, t('account.mfaStep2')),
          qr,
          h('p', { class: 'secondary' }, t('account.mfaManual')),
          h('pre', { class: 'ds-code-block' }, grouped, copyButton(s.secret, t('account.copySecret'))),
          codef,
        );
        code.focus();
      } else if (step === 'code') {
        const v = code.value.replace(/\s/g, '');
        if (!/^\d{6}$/.test(v)) {
          codef.setError(t('account.mfaCodeFormat'));
          code.focus();
          return;
        }
        const r = await api('/api/v1/me/totp/enable', { method: 'POST', body: { code: v } });
        step = 'codes';
        showRecovery(d, body, nextBtn, cancel, r.recovery_codes);
      }
    } catch (err) {
      const a = errorAlert(err);
      mount(errBox, a);
      if (step === 'code') {
        code.value = '';
        code.focus();
      } else {
        pw.value = '';
        pw.focus();
      }
    } finally {
      if (step !== 'codes') setBusy(nextBtn, false, t('common.working'));
    }
  };
  nextBtn.addEventListener('click', run);
  body.addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter' && ev.target.tagName === 'INPUT') {
      ev.preventDefault();
      run();
    }
  });
}

function showRecovery(d, body, nextBtn, cancel, codes) {
  const text = `${t('account.recoveryFileHead')}\n\n${codes.join('\n')}\n`;
  const ack = h('input', { type: 'checkbox', class: 'ds-check', name: 'ack' });
  const ackLabel = h('label', { class: 'ds-checkbox' }, ack, t('account.recoveryAck'));
  const download = h('a', { class: 'ds-button ds-button--secondary ds-button--sm', href: '#', download: 'kairos-recovery-codes.txt', dataset: { native: '1' } }, icon('download'), t('account.recoveryDownload'));
  download.addEventListener('click', () => {
    const url = URL.createObjectURL(new Blob([text], { type: 'text/plain;charset=utf-8' }));
    download.setAttribute('href', url);
    setTimeout(() => URL.revokeObjectURL(url), 10000);
  });
  mount(body, 
    inlineAlert({ kind: 'success', title: t('account.mfaEnabled') }),
    h('p', { class: 'strong' }, t('account.recoveryOnce')),
    h('ul', { class: 'k-recovery-codes', 'aria-label': t('account.recoveryCodes') }, codes.map((c) => h('li', null, c))),
    h('div', { class: 'ds-button-row' }, copyButton(codes.join('\n'), t('account.recoveryCopy')), download),
    ackLabel,
  );
  cancel.remove();
  // 「下一步」換成「關閉」：勾選「我已保存復原碼」之前為 aria-disabled（仍可聚焦，按下時說明原因）
  const closeBtn = h('button', { type: 'button', class: 'ds-button ds-button--primary', 'aria-disabled': 'true' }, t('common.close'));
  nextBtn.replaceWith(closeBtn);
  ack.addEventListener('change', () => {
    if (ack.checked) closeBtn.removeAttribute('aria-disabled');
    else closeBtn.setAttribute('aria-disabled', 'true');
  });
  // 關閉前必須勾選（Esc、右上角與「關閉」都一樣）
  const guard = () => {
    if (!ack.checked) {
      ack.focus();
      announce(t('account.recoveryAckFirst'));
      return false;
    }
    return true;
  };
  d.setGuard(guard);
  closeBtn.addEventListener('click', () => {
    if (!guard()) return;
    d.close();
  });
  ack.focus();
}

/** 停用：密碼＋目前的驗證碼或一組復原碼 */
function disableMfa(reload) {
  const errBox = h('div');
  const pw = input({ type: 'password', name: 'password', autocomplete: 'current-password' });
  const code = input({ name: 'code', autocomplete: 'one-time-code', cls: 'k-otp-input' });
  const pwf = field({ label: t('account.mfaPassword'), input: pw, required: true });
  const codef = field({ label: t('account.mfaCodeOrRecovery'), input: code, required: true });
  const ok = h('button', { type: 'button', class: 'ds-button ds-button--danger' }, t('account.mfaDisable'));
  const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
  const d = openDialog({
    title: t('account.mfaDisableTitle'),
    size: 'sm',
    body: h('div', null, h('p', null, t('account.mfaDisableBody')), errBox, pwf, codef),
    actions: [cancel, ok],
    initialFocus: pw,
  });
  cancel.addEventListener('click', () => d.close());
  ok.addEventListener('click', async () => {
    mount(errBox);
    pwf.setError(pw.value ? '' : t('pw.currentRequired'));
    codef.setError(code.value.trim() ? '' : t('login.mfaRequired'));
    if (!pw.value) return pw.focus();
    if (!code.value.trim()) return code.focus();
    setBusy(ok, true, t('common.working'));
    try {
      await api('/api/v1/me/totp/disable', { method: 'POST', body: { password: pw.value, code: code.value.replace(/\s/g, '') } });
      d.close();
      toast({ kind: 'success', title: t('account.mfaDisabled') });
      reload();
    } catch (err) {
      setBusy(ok, false, t('common.working'));
      mount(errBox, errorAlert(err));
      code.value = '';
      code.focus();
    }
  });
}

function deviceText(s) {
  const ua = s.user_agent || '';
  const browser = /Edg\//.test(ua) ? 'Edge' : /Firefox\//.test(ua) ? 'Firefox' : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : ua ? ua.slice(0, 40) : t('account.unknownDevice');
  const os = /Windows/.test(ua) ? 'Windows' : /Mac OS X/.test(ua) ? 'macOS' : /Android/.test(ua) ? 'Android' : /Linux/.test(ua) ? 'Linux' : /iPhone|iPad/.test(ua) ? 'iOS' : '';
  return [browser, os].filter(Boolean).join('（') + (os ? '）' : '');
}

function sessionsCard() {
  const body = h('div', null, skeletonBlock(3));
  const endOthers = h('button', { type: 'button', class: 'ds-button ds-button--danger-secondary', hidden: true }, t('account.endOthers'));
  async function load() {
    try {
      const d = await api('/api/v1/me/sessions');
      const list = (d.sessions || []).filter((s) => s.active !== false);
      const others = list.filter((s) => !s.current);
      endOthers.hidden = others.length === 0;
      endOthers.onclick = async () => {
        const ok = await confirm({
          title: t('account.endOthersTitle', { n: others.length }),
          body: h(
            'div',
            null,
            h('p', null, t('account.endOthersBody')),
            h('ul', null, others.map((s) => h('li', null, `${deviceText(s)} · ${s.ip || '—'} · ${t('account.lastActive', { t: fmt.relative(s.last_seen_at) })}`))),
            h('p', null, t('account.keepCurrent')),
          ),
          confirmText: t('account.endOthers'),
          danger: true,
        });
        if (!ok) return;
        try {
          const r = await api('/api/v1/me/sessions/revoke-others', { method: 'POST' });
          toast({ kind: 'success', title: t('account.endedOthers', { n: r.ended }), desc: t('account.keepCurrent') });
          load();
        } catch (err) {
          toast({ kind: 'error', title: short(err, { title: t('account.endFailed') }) });
        }
      };
      mount(body, 
        h(
          'div',
          { class: 'k-scroll-x' },
          h(
            'table',
            { class: 'k-mini-table' },
            h('caption', { class: 'ds-visually-hidden' }, t('account.sessions')),
            h(
              'thead',
              null,
              h('tr', null, h('th', { scope: 'col' }, t('account.device')), h('th', { scope: 'col' }, t('col.ip')), h('th', { scope: 'col' }, t('account.signedIn', { tz: fmt.tzLabel() })), h('th', { scope: 'col' }, t('account.lastActiveCol')), h('th', { scope: 'col' }, h('span', { class: 'ds-visually-hidden' }, t('account.actions')))),
            ),
            h(
              'tbody',
              null,
              list.map((s) =>
                h(
                  'tr',
                  { dataset: { session: s.session_id } },
                  h('td', null, deviceText(s), s.current ? [' ', statusBadge('online', t('account.thisSession'))] : null, h('span', { class: 'k-host-id' }, s.method === 'ldap' ? t('account.sourceLdap') : t('account.sourceLocal'))),
                  h('td', null, s.ip ? h('span', { class: 'mono' }, s.ip) : '—'),
                  h('td', null, fmt.dateTime(s.created_at)),
                  h('td', null, fmt.timeEl(s.last_seen_at)),
                  h(
                    'td',
                    null,
                    s.current
                      ? null
                      : h(
                          'button',
                          {
                            type: 'button',
                            class: 'ds-button ds-button--ghost ds-button--sm',
                            'aria-label': t('account.endOneAria', { d: deviceText(s), ip: s.ip || '' }),
                            on: {
                              click: async () => {
                                const ok = await confirm({ title: t('account.endOneTitle'), body: t('account.endOneBody', { d: deviceText(s), ip: s.ip || '—' }), confirmText: t('account.endOne'), danger: true });
                                if (!ok) return;
                                try {
                                  await api(`/api/v1/me/sessions/${encodeURIComponent(s.session_id)}/revoke`, { method: 'POST' });
                                  toast({ kind: 'success', title: t('account.endedOne') });
                                  load();
                                } catch (err) {
                                  toast({ kind: 'error', title: short(err, { title: t('account.endFailed') }) });
                                }
                              },
                            },
                          },
                          t('account.endOne'),
                        ),
                  ),
                ),
              ),
            ),
          ),
        ),
      );
    } catch (err) {
      mount(body, errorState(err, { title: t('account.sessionsFailed'), onRetry: load }));
    }
  }
  load();
  return card({ title: t('account.sessions'), actions: endOthers, body });
}

export function render(ctx) {
  const root = ctx.root;
  const draw = () => {
    const me = session.current();
    if (!session.isSession()) {
      mount(root, pageHead({ title: t('account.title') }), inlineAlert({ kind: 'info', title: t('account.tokenOnly') }));
      return;
    }
    mount(root, 
      pageHead({ title: t('account.title') }),
      h(
        'div',
        { class: 'k-settings-grid' },
        profileCard(me),
        appearanceCard(),
        passwordCard(me),
        mfaCard(me, reload),
        sessionsCard(),
        card({ title: t('account.apiTokens'), body: h('p', null, t('account.apiTokensNote')) }),
      ),
    );
  };
  async function reload() {
    await session.refresh();
    draw();
    ctx.focusTitle();
  }
  draw();
  return {};
}
