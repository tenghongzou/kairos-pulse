// 錯誤頁（設計包 ia.md §3「錯誤頁（403／404／503）」）：在外框內顯示，不換網址（重新整理後仍是同一個判斷）。

import { h, icon, mount } from '../core/dom.js';
import { t, permissionLabel } from '../core/strings.js';
import { pageHead, inlineAlert } from '../ui/components.js';

export function forbidden(root, { perm } = {}) {
  mount(root, 
    h(
      'div',
      { class: 'k-error-page' },
      h('p', { class: 'k-error-page__code', 'aria-hidden': 'true' }, '403'),
      pageHead({ title: t('error.403.title') }),
      inlineAlert({
        kind: 'warning',
        title: perm ? t('error.403.perm', { perm: permissionLabel(perm) }) : t('error.403.body'),
        code: 'KRS-E6104',
        body: t('error.403.fix'),
      }),
      h('a', { class: 'ds-button ds-button--secondary', href: '/' }, icon('chevron-left'), t('error.home')),
    ),
  );
}

export function notFound(root) {
  mount(root, 
    h(
      'div',
      { class: 'k-error-page' },
      h('p', { class: 'k-error-page__code', 'aria-hidden': 'true' }, '404'),
      pageHead({ title: t('error.404.title') }),
      h('p', null, t('error.404.body')),
      h('a', { class: 'ds-button ds-button--secondary', href: '/' }, icon('chevron-left'), t('error.home')),
    ),
  );
}
