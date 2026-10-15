// 選單（使用者選單、列的「更多」）：按鈕 aria-haspopup／aria-expanded；選單 role=menu，
// ↑↓ Home End 移動、Enter／Space 執行、Esc 關閉並回到按鈕、Tab 關閉。

import { h, icon } from '../core/dom.js';

let active = null;

function closeActive(focusAnchor) {
  if (!active) return;
  const { el, anchor, onDoc } = active;
  el.remove();
  anchor.setAttribute('aria-expanded', 'false');
  document.removeEventListener('pointerdown', onDoc, true);
  active = null;
  if (focusAnchor) anchor.focus();
}

/**
 * 在按鈕下方開啟選單。
 * @param {HTMLElement} anchor  觸發的按鈕
 * @param {Array} items  { text, iconName, onSelect, href, danger } 或 'separator'
 */
export function openMenu(anchor, items, { label } = {}) {
  if (active && active.anchor === anchor) {
    closeActive(true);
    return;
  }
  closeActive(false);
  const nodes = [];
  const el = h(
    'ul',
    { class: 'ds-menu', role: 'menu', 'aria-label': label || anchor.getAttribute('aria-label') || anchor.textContent.trim() },
    items.map((it) => {
      if (it === 'separator') return h('li', { role: 'separator' });
      const tag = it.href ? 'a' : 'button';
      const node = h(
        tag,
        {
          role: 'menuitem',
          tabindex: '-1',
          class: it.danger ? 'is-danger' : null,
          href: it.href,
          type: tag === 'button' ? 'button' : null,
          on: {
            click: () => {
              closeActive(!it.href);
              if (it.onSelect) it.onSelect();
            },
          },
        },
        it.iconName ? icon(it.iconName) : null,
        it.text,
      );
      nodes.push(node);
      return h('li', { role: 'none' }, node);
    }),
  );
  const r = anchor.getBoundingClientRect();
  document.body.appendChild(el);
  const w = el.offsetWidth;
  el.style.setProperty('top', `${Math.round(r.bottom + 4)}px`);
  el.style.setProperty('left', `${Math.round(Math.max(8, Math.min(r.right - w, window.innerWidth - w - 8)))}px`);
  anchor.setAttribute('aria-expanded', 'true');
  const onDoc = (ev) => {
    if (!el.contains(ev.target) && !anchor.contains(ev.target)) closeActive(false);
  };
  document.addEventListener('pointerdown', onDoc, true);
  el.addEventListener('keydown', (ev) => {
    const i = nodes.indexOf(document.activeElement);
    if (ev.key === 'ArrowDown') {
      ev.preventDefault();
      nodes[(i + 1) % nodes.length].focus();
    } else if (ev.key === 'ArrowUp') {
      ev.preventDefault();
      nodes[(i - 1 + nodes.length) % nodes.length].focus();
    } else if (ev.key === 'Home') {
      ev.preventDefault();
      nodes[0].focus();
    } else if (ev.key === 'End') {
      ev.preventDefault();
      nodes[nodes.length - 1].focus();
    } else if (ev.key === 'Escape') {
      ev.preventDefault();
      closeActive(true);
    } else if (ev.key === 'Tab') {
      closeActive(false);
    }
  });
  active = { el, anchor, onDoc };
  if (nodes[0]) nodes[0].focus();
}

export function closeMenu() {
  closeActive(false);
}
