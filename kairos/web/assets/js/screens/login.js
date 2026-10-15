// S01 登入（設計包 kairos/screens.md S01；kairos/API.md 第 12.2～12.4 節）。
//
// 步驟（同一張卡片）：帳號密碼 →（啟用兩步驟驗證時）驗證碼或復原碼 →（管理員建立或重設密碼的帳號）變更密碼
// → 前往 next（只接受站內路徑）。工作階段結束後由任何頁面帶 next 與 reason=session_expired 回到這裡。

import { h, icon, uid, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as prefs from '../core/prefs.js';
import { safeNext, navigate } from '../core/router.js';
import { field, input, errorAlert, inlineAlert, setBusy } from '../ui/components.js';
import * as fmt from '../core/format.js';

const PAGE_LABEL = {
  '/': 'nav.overview',
  '/hosts': 'nav.hosts',
  '/account': 'account.title',
};

function nextLabel(next) {
  if (next === '/' || !next) return null;
  const base = next.split('?')[0];
  if (PAGE_LABEL[base]) return t(PAGE_LABEL[base]);
  if (base.startsWith('/hosts/')) return t('login.nextHost');
  return base;
}

export function render(ctx) {
  const q = ctx.query;
  const next = safeNext(q.get('next'));
  const root = ctx.root;
  const state = { methods: { local: true, ldap: false }, mfaToken: null, lockTimer: null };
  const cardBody = h('div');
  const titleEl = h('h1', { class: 'ds-card__title', id: 'page-title', tabindex: '-1' }, t('login.title'));
  const errorRegion = h('div', { 'aria-live': 'polite' });
  const health = h('p', { class: 'k-login-health', hidden: true });

  const reason = q.get('reason');
  const banner =
    reason === 'session_expired'
      ? inlineAlert({ kind: 'info', title: t('login.sessionExpired.title'), body: t('login.sessionExpired.body') })
      : reason === 'logged_out'
        ? inlineAlert({ kind: 'success', title: t('login.loggedOut') })
        : null;
  const nl = nextLabel(next);

  mount(root, 
    h(
      'main',
      { class: 'k-bare', id: 'main' },
      h('div', { class: 'k-login-brand' }, h('span', { class: 'k-brand__mark', 'aria-hidden': 'true' }, 'K'), h('span', { class: 'k-brand' }, 'Kairos')),
      h(
        'div',
        { class: 'k-login-card' },
        banner,
        h(
          'section',
          { class: 'ds-card', 'aria-labelledby': 'page-title' },
          h('div', { class: 'ds-card__head' }, titleEl),
          nl ? h('p', { class: 'secondary' }, t('login.next', { page: nl })) : null,
          errorRegion,
          cardBody,
        ),
      ),
      h('div', { class: 'k-login-foot' }, health, h('p', null, t('login.foot'))),
    ),
  );
  document.title = t('login.docTitle');

  function showError(err, title) {
    mount(errorRegion, errorAlert(err, { title }));
  }
  function clearError() {
    mount(errorRegion);
    if (state.lockTimer) {
      clearInterval(state.lockTimer);
      state.lockTimer = null;
    }
  }

  async function done() {
    await session.load();
    if (session.mustChangePassword()) {
      showChange();
      return;
    }
    ctx.app.loggedIn();
    navigate(next, { replace: true });
  }

  function lockedMessage(err) {
    const until = err.details && err.details.locked_until;
    const p = h('p');
    const update = () => {
      const left = until ? Math.max(0, Math.ceil((Date.parse(until) - fmt.serverNow()) / 1000)) : err.retryAfter || 0;
      p.textContent = left > 0 ? t('login.lockedLeft', { time: fmt.duration(left), at: until ? fmt.time(until) : '' }) : t('login.lockedOver');
      if (left <= 0 && state.lockTimer) {
        clearInterval(state.lockTimer);
        state.lockTimer = null;
      }
    };
    update();
    state.lockTimer = setInterval(update, 1000 * 15);
    return p;
  }

  // ---- 第一步：帳號密碼 ----
  function showCredentials(message) {
    state.mfaToken = null;
    titleEl.textContent = t('login.title');
    const user = input({ name: 'username', autocomplete: 'username', required: true });
    const pass = input({ type: 'password', name: 'password', autocomplete: 'current-password', required: true });
    const show = h(
      'button',
      { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', 'aria-pressed': 'false', 'aria-label': t('login.showPassword') },
      icon('eye'),
      t('login.show'),
    );
    show.addEventListener('click', () => {
      const on = pass.type === 'password';
      pass.type = on ? 'text' : 'password';
      show.setAttribute('aria-pressed', on ? 'true' : 'false');
    });
    const uf = field({ label: t('login.username'), input: user, required: true });
    const pf = field({ label: t('login.password'), input: pass, required: true });
    pf.querySelector('input').after(h('div', { class: 'ds-button-row' }, show));
    let methodRow = null;
    let method = 'local';
    if (state.methods.ldap) {
      const remembered = prefs.get('loginMethod');
      method = remembered === 'local' || remembered === 'ldap' ? remembered : 'ldap';
      const name = uid('m');
      const radio = (value, label) =>
        h(
          'label',
          { class: 'ds-radio' },
          h('input', { type: 'radio', name, value, checked: method === value, on: { change: () => (method = value) } }),
          label,
        );
      methodRow = h('fieldset', null, h('legend', null, t('login.method')), h('div', { class: 'ds-radio-group' }, radio('ldap', t('login.methodLdap')), radio('local', t('login.methodLocal'))));
    }
    const submit = h('button', { type: 'submit', class: 'ds-button ds-button--primary ds-button--lg' }, t('login.submit'));
    const form = h('form', { novalidate: true }, methodRow, uf, pf, h('div', { class: 'ds-button-row' }, submit));
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      clearError();
      uf.setError(user.value.trim() ? '' : t('login.usernameRequired'));
      pf.setError(pass.value ? '' : t('login.passwordRequired'));
      if (!user.value.trim()) return user.focus();
      if (!pass.value) return pass.focus();
      setBusy(submit, true, t('login.submitting'));
      try {
        if (state.methods.ldap) prefs.set('loginMethod', method);
        const data = await api('/api/v1/auth/login', {
          method: 'POST',
          auth: false,
          body: { username: user.value.trim(), password: pass.value, method },
        });
        if (data.mfa_required) {
          state.mfaToken = data.mfa_token;
          showMfa();
          return;
        }
        await done();
      } catch (err) {
        setBusy(submit, false, t('login.submitting'));
        pass.value = '';
        if (err.code === 'account_locked') {
          mount(errorRegion, 
            inlineAlert({ kind: 'error', title: t('login.locked'), code: 'KRS-E6003', body: lockedMessage(err), role: 'alert' }),
          );
        } else {
          showError(err);
        }
        pass.focus();
      }
    });
    mount(cardBody, message ? inlineAlert({ kind: 'warning', title: message }) : null, form);
    user.focus();
  }

  // ---- 第二步：兩步驟驗證 ----
  function showMfa() {
    titleEl.textContent = t('login.mfaTitle');
    let recovery = false;
    const code = input({ name: 'code', autocomplete: 'one-time-code', inputmode: 'numeric', cls: 'k-otp-input', maxlength: 6, required: true });
    const cf = field({ label: t('login.mfaCode'), input: code, hint: t('login.mfaHint'), required: true });
    const toggle = h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm' }, t('login.useRecovery'));
    toggle.addEventListener('click', () => {
      recovery = !recovery;
      code.value = '';
      code.setAttribute('inputmode', recovery ? 'text' : 'numeric');
      code.setAttribute('maxlength', recovery ? '11' : '6');
      code.setAttribute('autocomplete', recovery ? 'off' : 'one-time-code');
      cf.querySelector('label').firstChild.textContent = recovery ? t('login.recoveryCode') : t('login.mfaCode');
      cf.querySelector('.ds-field__hint').textContent = recovery ? t('login.recoveryHint') : t('login.mfaHint');
      toggle.textContent = recovery ? t('login.useTotp') : t('login.useRecovery');
      code.focus();
    });
    const submit = h('button', { type: 'submit', class: 'ds-button ds-button--primary ds-button--lg' }, t('login.verify'));
    const back = h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--lg', on: { click: () => showCredentials() } }, t('login.back'));
    const form = h('form', { novalidate: true }, cf, h('div', { class: 'ds-button-row' }, toggle), h('div', { class: 'ds-button-row' }, submit, back));
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      clearError();
      const v = code.value.replace(/\s/g, '');
      if (!v) {
        cf.setError(t('login.mfaRequired'));
        return code.focus();
      }
      cf.setError('');
      setBusy(submit, true, t('login.verifying'));
      try {
        await api('/api/v1/auth/login/mfa', { method: 'POST', auth: false, body: { mfa_token: state.mfaToken, code: v } });
        await done();
      } catch (err) {
        setBusy(submit, false, t('login.verifying'));
        code.value = '';
        if (err.code === 'mfa_challenge_expired') {
          showCredentials(t('login.mfaExpired'));
          return;
        }
        showError(err);
        code.focus();
      }
    });
    mount(cardBody, form);
    code.focus();
  }

  // ---- 必須先變更密碼 ----
  function showChange() {
    titleEl.textContent = t('login.changeTitle');
    const cur = input({ type: 'password', name: 'current', autocomplete: 'current-password', required: true });
    const nw = input({ type: 'password', name: 'new', autocomplete: 'new-password', required: true });
    const nw2 = input({ type: 'password', name: 'confirm', autocomplete: 'new-password', required: true });
    const f1 = field({ label: t('pw.current'), input: cur, required: true });
    const f2 = field({ label: t('pw.new'), input: nw, required: true });
    const f3 = field({ label: t('pw.confirm'), input: nw2, required: true });
    const submit = h('button', { type: 'submit', class: 'ds-button ds-button--primary ds-button--lg' }, t('pw.submit'));
    const logout = h(
      'button',
      {
        type: 'button',
        class: 'ds-button ds-button--secondary ds-button--lg',
        on: {
          click: async () => {
            await ctx.app.logout();
          },
        },
      },
      t('shell.logout'),
    );
    const form = h(
      'form',
      { novalidate: true },
      h('p', null, t('login.changeIntro')),
      f1,
      f2,
      h('ul', { class: 'k-password-rules' }, h('li', null, t('pw.rule.length')), h('li', null, t('pw.rule.username')), h('li', null, t('pw.rule.common'))),
      f3,
      h('div', { class: 'ds-button-row' }, submit, logout),
    );
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      clearError();
      f1.setError(cur.value ? '' : t('pw.currentRequired'));
      f2.setError(nw.value ? '' : t('pw.newRequired'));
      f3.setError(nw.value === nw2.value ? '' : t('pw.mismatch'));
      if (!cur.value) return cur.focus();
      if (!nw.value) return nw.focus();
      if (nw.value !== nw2.value) return nw2.focus();
      setBusy(submit, true, t('pw.submitting'));
      try {
        await api('/api/v1/me/password', { method: 'POST', body: { current_password: cur.value, new_password: nw.value } });
        await done();
      } catch (err) {
        setBusy(submit, false, t('pw.submitting'));
        if (err.code === 'invalid_parameter' && err.details && err.details.parameter === 'new_password') {
          f2.setError(err.message);
          nw.focus();
        } else {
          showError(err, t('pw.failed'));
        }
      }
    });
    mount(cardBody, form);
    titleEl.focus();
  }

  // 登入公開健康燈（/healthz；observability.on_console = false 時沒有這個端點，隱藏；D36）
  fetch('/healthz', { cache: 'no-store', credentials: 'omit' })
    .then(async (r) => {
      const type = r.headers.get('content-type') || '';
      if (!type.includes('json')) return;
      const body = await r.json();
      if (r.status === 404 || !body || !body.status) return;
      const ok = r.ok && body.status === 'ok';
      mount(health, 
        h('span', { class: `ds-status-badge ds-status-badge--dot k-dot--${ok ? 'online' : 'critical'}` }, ok ? t('login.healthOk') : t('login.healthBad')),
      );
      health.hidden = false;
    })
    .catch(() => {});

  // 已登入：直接前往（必須變更密碼時留在這裡）
  if (session.isLoggedIn()) {
    if (session.mustChangePassword()) showChange();
    else {
      navigate(next, { replace: true });
      return {};
    }
  } else {
    api('/api/v1/auth/methods', { auth: false })
      .then((m) => {
        state.methods = m || state.methods;
      })
      .catch(() => {})
      .finally(() => {
        if (!state.mfaToken && !session.isLoggedIn()) showCredentials();
      });
  }
  return {
    dispose() {
      if (state.lockTimer) clearInterval(state.lockTimer);
    },
  };
}

