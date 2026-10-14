// 對話框（design-system/README.md §6.9、§6.10）：原生 <dialog>＋showModal（焦點留在對話框內、Esc 關閉），
// 關閉後焦點回到開啟它的元素。ConfirmDialog（L1）：問句標題＋後果說明＋「取消」／動詞按鈕。

import { h, icon, uid } from '../core/dom.js';
import { t } from '../core/strings.js';

/**
 * 開啟對話框。
 * @param {object} o  title、body（節點）、actions（按鈕節點陣列；預設只有「關閉」）、size（sm／md／lg）、
 *                    initialFocus（節點）、onClose(result)、dismissible（預設 true：Esc 與關閉鈕）
 * @returns {{el, close(result)}}
 */
export function openDialog(o) {
  const opener = document.activeElement;
  const titleId = uid('dlg');
  const close = (result) => {
    if (!dlg.open) return;
    dlg.close();
    dlg.remove();
    if (opener && opener.isConnected && typeof opener.focus === 'function') opener.focus();
    if (o.onClose) o.onClose(result);
  };
  // 使用者以 Esc 或右上角關閉時先問 guard（例如復原碼尚未確認已保存）
  let guard = null;
  const userClose = () => {
    if (guard && guard() === false) return;
    close();
  };
  const actions = o.actions || [h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => close() } }, t('common.close'))];
  const dlg = h(
    'dialog',
    { class: ['ds-dialog', o.size ? `ds-dialog--${o.size}` : null, o.danger ? 'ds-dialog--danger' : null], 'aria-labelledby': titleId },
    h(
      'div',
      { class: 'ds-dialog__head' },
      h('h2', { class: 'ds-dialog__title', id: titleId }, o.title),
      o.dismissible === false
        ? null
        : h('button', { type: 'button', class: 'ds-button ds-button--icon ds-button--sm', 'aria-label': t('common.close'), title: t('common.close'), on: { click: () => userClose() } }, icon('close')),
    ),
    h('div', { class: 'ds-dialog__body' }, o.body),
    actions.length ? h('div', { class: 'ds-dialog__foot' }, actions) : null,
  );
  dlg.addEventListener('cancel', (ev) => {
    ev.preventDefault();
    if (o.dismissible !== false) userClose();
  });
  document.body.appendChild(dlg);
  dlg.showModal();
  if (o.initialFocus) o.initialFocus.focus();
  return {
    el: dlg,
    close,
    setGuard(fn) {
      guard = fn;
    },
  };
}

/**
 * ConfirmDialog（L1）：回傳 Promise<boolean>。
 * @param {object} o  title（問句）、body（後果說明，字串或節點）、confirmText（動詞，重複標題中的動詞）、danger
 */
export function confirm({ title, body, confirmText, danger = false }) {
  return new Promise((resolve) => {
    let done = false;
    const finish = (v) => {
      if (done) return;
      done = true;
      d.close(v);
      resolve(v);
    };
    const cancelBtn = h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => finish(false) } }, t('common.cancel'));
    const okBtn = h(
      'button',
      { type: 'button', class: ['ds-button', danger ? 'ds-button--danger' : 'ds-button--primary'], on: { click: () => finish(true) } },
      confirmText,
    );
    const d = openDialog({
      title,
      size: 'sm',
      body: typeof body === 'string' ? h('p', null, body) : body,
      actions: [cancelBtn, okBtn],
      initialFocus: cancelBtn,
      onClose: () => {
        if (!done) {
          done = true;
          resolve(false);
        }
      },
    });
  });
}
