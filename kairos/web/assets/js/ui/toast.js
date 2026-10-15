// Toast（design-system/README.md §6.8）：成功與資訊在 role=status（polite），錯誤在 role=alert（assertive）。
// 成功／資訊 6 秒後自動消失；錯誤需要手動關閉（不讓錯誤訊息在讀完前消失）。

import { h, icon } from '../core/dom.js';
import { t } from '../core/strings.js';

const ICONS = { success: 'success', info: 'info', warning: 'status-warning', error: 'status-critical' };

/** toast({ kind: 'success'|'info'|'warning'|'error', title, desc }) */
export function toast({ kind = 'success', title, desc, timeout } = {}) {
  const region = document.getElementById(kind === 'error' ? 'toast-alert' : 'toast-status');
  if (!region) return;
  const el = h(
    'div',
    { class: `ds-toast ds-toast--${kind}` },
    icon(ICONS[kind]),
    h('div', { class: 'ds-toast__body' }, h('p', { class: 'ds-toast__title' }, title), desc ? h('p', { class: 'ds-toast__desc' }, desc) : null),
    h(
      'button',
      { type: 'button', class: 'ds-button ds-button--icon ds-button--sm', 'aria-label': t('common.close'), title: t('common.close'), on: { click: () => el.remove() } },
      icon('close'),
    ),
  );
  region.appendChild(el);
  const ms = timeout ?? (kind === 'error' ? 0 : 6000);
  if (ms > 0) setTimeout(() => el.remove(), ms);
  return el;
}
