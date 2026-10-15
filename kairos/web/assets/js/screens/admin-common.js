// 管理畫面（S16～S18、S20、S21）共用的呈現與對話框。
//
// - 角色指派的文字：「維運者（B廠產線）」「管理員（全站）」（screens.md S16 範圍 D39）。
// - 只顯示一次的秘密（一次性密碼、API token）：只放在對話框的唯讀欄位中，關閉時清空；
//   不寫入 localStorage、網址或主控台日誌。關閉前要求勾選「我已記下」（screens.md S16「新增使用者」）。
// - 危險操作的確認（screens.md §0.3）：L1 ConfirmDialog（可帶原因）、L2 DangerConfirmDialog（輸入確認字串）。

import { h, icon, uid, announce, mount } from '../core/dom.js';
import { t, roleLabel } from '../core/strings.js';
import * as session from '../core/session.js';
import { openDialog } from '../ui/dialog.js';
import { copyButton, field, input } from '../ui/components.js';

export const ROLES = ['admin', 'operator', 'auditor', 'viewer'];

/** 範圍的文字：全站／主機群組名稱 */
export function scopeText(scope) {
  if (scope && scope.type === 'group') return t('adm.scope.group', { g: scope.group });
  return t('adm.scope.global');
}

/** 指派（API 的 `{role, scope}`）的文字：「維運者（B廠產線）」 */
export function assignmentText(a) {
  return t('adm.assignment', { role: roleLabel(a.role), scope: scopeText(a.scope) });
}

/** 指派 → API 的文字形式（`admin`、`operator@group:B廠產線`） */
export function assignmentKey(a) {
  return a.scope && a.scope.type === 'group' ? `${a.role}@group:${a.scope.group}` : a.role;
}

/** 目前登入的帳號（以工作階段登入時） */
export function myUsername() {
  const me = session.current();
  return me && me.type === 'session' && me.user ? me.user.username : null;
}

/** 頁面內的分頁（連結；aria-current 標示目前的分頁） */
export function linkTabs(label, items, active) {
  return h(
    'nav',
    { class: 'ds-tabs', 'aria-label': label },
    items.map(([key, href, text]) => h('a', { href, 'aria-current': key === active ? 'page' : null }, text)),
  );
}

/** 「為什麼不能按」的說明（aria-disabled 的按鈕以 aria-describedby 關聯；screens.md §0.2） */
export function disabledButton(text, why, { variant = 'secondary', size } = {}) {
  const id = uid('why');
  const b = h(
    'button',
    { type: 'button', class: ['ds-button', `ds-button--${variant}`, size ? `ds-button--${size}` : null], 'aria-disabled': 'true', 'aria-describedby': id },
    text,
  );
  b.addEventListener('click', () => announce(why));
  return [b, h('p', { class: 'caption', id }, why)];
}

function reasonField({ required, label }) {
  const el = h('textarea', { class: 'ds-textarea', name: 'reason', rows: '2', maxlength: '500' });
  const f = field({ label: label || (required ? t('adm.reasonRequired') : t('adm.reasonOptional')), input: el, required });
  return { el, f };
}

/**
 * L1 ConfirmDialog（可帶原因欄）：回傳 Promise<{reason}|null>。
 * @param {object} o  title（問句）、body（字串或節點）、confirmText（動詞）、danger、reason（'optional'｜'required'｜null）
 */
export function confirmL1({ title, body, confirmText, danger = false, reason = 'optional', onConfirm }) {
  return new Promise((resolve) => {
    let done = false;
    const errBox = h('div');
    const r = reason ? reasonField({ required: reason === 'required' }) : null;
    const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
    const ok = h('button', { type: 'button', class: ['ds-button', danger ? 'ds-button--danger' : 'ds-button--primary'] }, confirmText);
    const d = openDialog({
      title,
      size: 'sm',
      body: h('div', null, typeof body === 'string' ? h('p', null, body) : body, errBox, r ? r.f : null),
      actions: [cancel, ok],
      initialFocus: cancel,
      onClose: () => {
        if (!done) {
          done = true;
          resolve(null);
        }
      },
    });
    cancel.addEventListener('click', () => d.close());
    ok.addEventListener('click', async () => {
      const text = r ? r.el.value.trim() : '';
      if (r && reason === 'required' && !text) {
        r.f.setError(t('adm.reasonMissing'));
        r.el.focus();
        return;
      }
      if (onConfirm) {
        // 送出時的錯誤顯示在對話框內（不關閉，讓使用者修正後重試）
        ok.disabled = true;
        ok.setAttribute('aria-busy', 'true');
        try {
          const res = await onConfirm({ reason: text || null, errBox });
          if (res === false) return;
        } finally {
          ok.disabled = false;
          ok.removeAttribute('aria-busy');
        }
      }
      done = true;
      d.close();
      resolve({ reason: text || null });
    });
  });
}

/**
 * L2 DangerConfirmDialog：輸入確認字串（帳號、「停止續簽 N 台」…）後才能按危險按鈕。
 * 回傳 Promise<{reason}|null>。
 */
export function confirmL2({ title, lead, body, phrase, confirmText, reason = 'optional', onConfirm }) {
  return new Promise((resolve) => {
    let done = false;
    const errBox = h('div');
    const r = reason ? reasonField({ required: reason === 'required' }) : null;
    const typed = input({ name: 'confirm', autocomplete: 'off', mono: true });
    const status = h('span', { class: 'caption', 'aria-live': 'polite' }, t('adm.l2.mismatch'));
    const tf = field({ label: t('adm.l2.typeToConfirm', { phrase }), input: typed, required: true });
    tf.appendChild(status);
    const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
    const ok = h('button', { type: 'button', class: 'ds-button ds-button--danger', disabled: true }, confirmText);
    const leadId = uid('lead');
    const d = openDialog({
      title,
      size: 'md',
      danger: true,
      body: h(
        'div',
        null,
        lead ? h('p', { class: 'k-danger-lead', id: leadId }, icon('status-critical'), lead) : null,
        typeof body === 'string' ? h('p', null, body) : body,
        errBox,
        r ? r.f : null,
        tf,
      ),
      actions: [cancel, ok],
      initialFocus: typed,
      onClose: () => {
        if (!done) {
          done = true;
          resolve(null);
        }
      },
    });
    if (lead) d.el.setAttribute('aria-describedby', leadId);
    let matched = false;
    typed.addEventListener('input', () => {
      const m = typed.value.trim() === phrase;
      if (m !== matched) {
        matched = m;
        status.textContent = m ? t('adm.l2.match') : t('adm.l2.mismatch');
      }
      ok.disabled = !m;
    });
    cancel.addEventListener('click', () => d.close());
    ok.addEventListener('click', async () => {
      if (typed.value.trim() !== phrase) return;
      const text = r ? r.el.value.trim() : '';
      if (r && reason === 'required' && !text) {
        r.f.setError(t('adm.reasonMissing'));
        r.el.focus();
        return;
      }
      if (onConfirm) {
        ok.disabled = true;
        try {
          const res = await onConfirm({ reason: text || null, errBox });
          if (res === false) return;
        } finally {
          ok.disabled = typed.value.trim() !== phrase;
        }
      }
      done = true;
      d.close();
      resolve({ reason: text || null });
    });
  });
}

/**
 * 只顯示一次的秘密（一次性密碼、API token）。關閉前必須勾選「我已記下」；關閉後欄位清空、
 * 節點移除，畫面上不再保留秘密。回傳 Promise（關閉時 resolve）。
 * @param {object} o  title、lead（成功訊息）、label（欄位名稱）、secret、note（說明）、extra（其他節點）、ackText
 */
export function showSecretOnce({ title, lead, label, secret, note, extra, ackText }) {
  return new Promise((resolve) => {
    const fid = uid('secret');
    const box = h('input', { class: 'ds-input ds-input--mono', id: fid, type: 'text', readonly: true, value: secret, autocomplete: 'off', spellcheck: 'false', dataset: { secret: '1' } });
    const ack = h('input', { type: 'checkbox', class: 'ds-check', name: 'ack' });
    const ackLabel = h('label', { class: 'ds-checkbox' }, ack, ackText || t('adm.secret.ack'));
    const closeBtn = h('button', { type: 'button', class: 'ds-button ds-button--primary', 'aria-disabled': 'true' }, t('common.close'));
    const d = openDialog({
      title,
      size: 'md',
      body: h(
        'div',
        { class: 'k-stack k-stack--sm' },
        lead ? h('div', { class: 'ds-alert ds-alert--success', role: 'status' }, icon('success'), h('div', { class: 'ds-alert__body' }, h('p', { class: 'ds-alert__title' }, lead))) : null,
        h('p', { class: 'strong' }, t('adm.secret.once')),
        h(
          'div',
          { class: 'ds-field' },
          h('label', { class: 'ds-field__label', for: fid }, label),
          h('div', { class: 'ds-secret-field' }, box, copyButton(() => box.value, t('adm.secret.copy', { what: label }))),
        ),
        note ? h('p', null, note) : null,
        extra || null,
        ackLabel,
      ),
      actions: [closeBtn],
      initialFocus: box,
      onClose: () => {
        box.value = '';
        resolve();
      },
    });
    ack.addEventListener('change', () => {
      if (ack.checked) closeBtn.removeAttribute('aria-disabled');
      else closeBtn.setAttribute('aria-disabled', 'true');
    });
    const guard = () => {
      if (!ack.checked) {
        ack.focus();
        announce(t('adm.secret.ackFirst'));
        return false;
      }
      return true;
    };
    d.setGuard(guard);
    closeBtn.addEventListener('click', () => {
      if (guard()) d.close();
    });
    box.addEventListener('focus', () => box.select());
  });
}

/** 角色的權限（/api/v1/roles）→ 指派清單的全站權限（群組範圍只授予主機的讀取，不計入全站） */
export function globalPermissions(assignments, roleMatrix) {
  const set = new Set();
  for (const a of assignments) {
    if (a.scope && a.scope.type === 'group') continue;
    const r = (roleMatrix || []).find((x) => x.role === a.role);
    if (r) for (const p of r.permissions) set.add(p);
  }
  return set;
}

/** 權限差異（新增、移除） */
export function permissionDiff(before, after, roleMatrix) {
  const b = globalPermissions(before, roleMatrix);
  const a = globalPermissions(after, roleMatrix);
  return {
    added: [...a].filter((p) => !b.has(p)),
    removed: [...b].filter((p) => !a.has(p)),
  };
}

/** 值的顯示（設定、稽核的細節）：布林、陣列、空字串 */
export function valueText(v) {
  if (v == null) return '—';
  if (v === true) return t('adm.value.true');
  if (v === false) return t('adm.value.false');
  if (Array.isArray(v)) return v.length ? v.map((x) => valueText(x)).join('、') : t('adm.value.emptyList');
  if (typeof v === 'object') return JSON.stringify(v);
  if (v === '') return t('adm.value.empty');
  return String(v);
}

/** 載入中的區塊 → 內容（mount 的簡寫） */
export function fill(el, ...children) {
  return mount(el, ...children);
}
