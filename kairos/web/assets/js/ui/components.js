// 設計系統元件（design-system/README.md §6；class 名稱與原型相同）。
// 所有元件回傳 DOM 節點；文字一律經由 h() 以 textContent 插入。

import { h, icon, uid, announce, mount } from '../core/dom.js';
import { t, statusLabel } from '../core/strings.js';
import { describe } from '../core/errors.js';

const STATUS_ICON = {
  online: 'status-online',
  warning: 'status-warning',
  critical: 'status-critical',
  offline: 'status-offline',
  unknown: 'status-unknown',
  pending: 'status-pending',
  info: 'info',
  emergency: 'severity-emergency',
};

/** StatusBadge（§6.4）：顏色＋圖示＋文字 */
export function statusBadge(status, label) {
  return h(
    'span',
    { class: `ds-status-badge ds-status-badge--${status}`, dataset: { status } },
    icon(STATUS_ICON[status] || 'info'),
    label || statusLabel(status),
  );
}

/** 中性的文字標籤（生命週期、規劃提醒等不是狀態的標記） */
export function neutralBadge(text, iconName) {
  return h('span', { class: 'ds-status-badge ds-status-badge--neutral' }, iconName ? icon(iconName) : null, text);
}

/** 圓點樣式（側邊導覽、圖例；仍有文字） */
export function dotBadge(status, text) {
  return h('span', { class: `ds-status-badge ds-status-badge--dot k-dot--${status}` }, text);
}

/** 按鈕（§6.1）。variant：primary／secondary／ghost／danger／danger-secondary／icon */
export function button({ text, variant = 'secondary', size, iconName, label, onClick, type = 'button', attrs = {}, cls } = {}) {
  const b = h(
    'button',
    {
      type,
      class: ['ds-button', `ds-button--${variant}`, size ? `ds-button--${size}` : null, cls],
      'aria-label': label,
      title: variant === 'icon' ? label : undefined,
      on: { click: onClick },
      ...attrs,
    },
    iconName ? icon(iconName) : null,
    variant === 'icon' ? null : text,
  );
  return b;
}

/** 進行中：按鈕不可重複觸發，文字改為進行式（§6.1 loading） */
export function setBusy(btn, busy, busyText) {
  if (busy) {
    btn.dataset.text = btn.textContent;
    btn.setAttribute('aria-busy', 'true');
    btn.disabled = true;
    if (busyText) {
      mount(btn, h('span', { class: 'ds-spinner', 'aria-hidden': 'true' }), busyText);
    }
  } else {
    btn.removeAttribute('aria-busy');
    btn.disabled = false;
    if (btn.dataset.text != null && busyText) btn.textContent = btn.dataset.text;
  }
}

/** 頁首：麵包屑、標題（h1，換頁時聚焦）、說明、動作（ia.md §2） */
export function pageHead({ title, crumbs, meta, actions } = {}) {
  const h1 = h('h1', { class: 'k-page-title', tabindex: '-1', id: 'page-title' }, title);
  return h(
    'div',
    null,
    crumbs && crumbs.length
      ? h(
          'nav',
          { class: 'k-breadcrumb', 'aria-label': t('a11y.breadcrumb') },
          h(
            'ol',
            null,
            crumbs.map((c, i) =>
              h('li', null, c.href && i < crumbs.length - 1 ? h('a', { href: c.href }, c.text) : h('span', { 'aria-current': i === crumbs.length - 1 ? 'page' : null }, c.text)),
            ),
          ),
        )
      : null,
    h(
      'div',
      { class: 'k-page-head' },
      h1,
      meta ? h('span', { class: 'k-page-head__meta' }, meta) : null,
      actions ? h('div', { class: 'k-page-head__actions' }, actions) : null,
    ),
  );
}

/** Card（§6.5） */
export function card({ title, titleTag = 'h2', actions, body, cls, status, id } = {}) {
  const titleId = id || uid('card');
  return h(
    'section',
    { class: ['ds-card', status ? `ds-card--status-${status}` : null, cls], 'aria-labelledby': title ? titleId : null },
    title || actions
      ? h(
          'div',
          { class: 'ds-card__head' },
          title ? h(titleTag, { class: 'ds-card__title ds-card__title--sm', id: titleId }, title) : null,
          actions ? h('div', { class: 'ds-card__actions' }, actions) : null,
        )
      : null,
    body,
  );
}

/** KpiTile（§6.6）：狀態色條＋圖示＋文字；有 href 時整張是連結 */
export function kpiTile({ status, label, value, unit, delta, sub, href, extra, tone = true }) {
  const tag = href ? 'a' : 'div';
  return h(
    tag,
    // 數值為 0 或無法取得（—）的磚不帶狀態色（screens.md S03）
    { class: ['ds-kpi', status && tone !== false ? `ds-kpi--${status}` : null], href },
    h('span', { class: 'ds-kpi__label' }, status ? icon(STATUS_ICON[status] || 'info') : null, label),
    h('span', { class: 'ds-kpi__value', dataset: { kpi: status || label } }, value, unit ? [' ', h('span', { class: 'ds-kpi__unit' }, unit)] : null),
    delta ? h('span', { class: 'ds-kpi__delta' }, delta) : null,
    sub ? h('span', { class: 'ds-kpi__delta' }, sub) : null,
    extra || null,
  );
}

/** KPI 骨架（載入中不可顯示 0；screens.md §0.2） */
export function kpiSkeleton(label) {
  return h(
    'div',
    { class: 'ds-kpi', 'aria-busy': 'true' },
    h('span', { class: 'ds-kpi__label' }, label),
    h('span', { class: 'ds-kpi__value' }, h('span', { class: 'ds-skeleton k-skel-cell', 'aria-hidden': 'true' })),
    h('span', { class: 'ds-visually-hidden' }, t('common.loading')),
  );
}

/** EmptyState（§6.11）：說明為什麼是空的＋下一步 */
export function emptyState({ title, body, actions, iconName = 'info' } = {}) {
  return h(
    'div',
    { class: 'ds-empty' },
    icon(iconName, 'ds-icon--xl'),
    h('p', { class: 'ds-empty__title' }, title),
    body ? h('p', null, body) : null,
    actions ? h('div', { class: 'ds-button-row' }, actions) : null,
  );
}

/** 三段式內容（標題外的部分）：原因、處理方式、代碼、技術細節 */
function threePart(d) {
  return [
    h(
      'dl',
      { class: 'k-3part' },
      d.reason ? [h('dt', null, t('err.label.reason')), h('dd', null, d.reason)] : null,
      d.steps && d.steps.length
        ? [
            h('dt', null, t('err.label.fix')),
            h('dd', null, d.steps.length === 1 ? d.steps[0] : h('ol', { class: 'k-fix-steps' }, d.steps.map((s) => h('li', null, s)))),
          ]
        : null,
      d.code ? [h('dt', null, t('err.label.code')), h('dd', null, h('code', { class: 'k-code-tag' }, d.code))] : null,
    ),
    d.technical
      ? h(
          'details',
          { class: 'k-details' },
          h('summary', null, t('err.label.technical')),
          h('p', { class: 'k-code-line' }, d.technical),
        )
      : null,
  ];
}

/** ErrorState（§6.13）：區塊級錯誤，三段式＋重試 */
export function errorState(err, { title, onRetry } = {}) {
  const d = describe(err, { title });
  return h(
    'div',
    { class: 'ds-alert ds-alert--error k-error-state', role: 'alert' },
    icon('status-critical'),
    h(
      'div',
      { class: 'ds-alert__body' },
      h('p', { class: 'ds-alert__title' }, d.title),
      threePart(d),
      onRetry ? h('div', { class: 'ds-button-row' }, button({ text: t('common.retry'), iconName: 'refresh', onClick: onRetry, size: 'sm' })) : null,
    ),
  );
}

/** InlineAlert（§6.13）。kind：info／success／warning／error */
export function inlineAlert({ kind = 'info', title, body, code, actions, role } = {}) {
  const iconName = { info: 'info', success: 'success', warning: 'status-warning', error: 'status-critical' }[kind];
  return h(
    'div',
    { class: `ds-alert ds-alert--${kind}`, role: role || (kind === 'error' ? 'alert' : null) },
    icon(iconName),
    h(
      'div',
      { class: 'ds-alert__body' },
      title ? h('p', { class: 'ds-alert__title' }, title, code ? [' ', h('code', { class: 'k-code-tag' }, code)] : null) : null,
      body ? (body instanceof Node || Array.isArray(body) ? body : h('p', null, body)) : null,
      actions ? h('div', { class: 'ds-button-row' }, actions) : null,
    ),
  );
}

/** 錯誤的 InlineAlert（表單送出失敗）：三段式 */
export function errorAlert(err, { title } = {}) {
  const d = describe(err, { title });
  return h(
    'div',
    { class: 'ds-alert ds-alert--error', role: 'alert', tabindex: '-1' },
    icon('status-critical'),
    h('div', { class: 'ds-alert__body' }, h('p', { class: 'ds-alert__title' }, d.title), threePart(d)),
  );
}

/** 表單欄位（§6.2）：標籤在上、說明、錯誤以 aria-describedby 關聯 */
export function field({ label, input, hint, error, required, id }) {
  const fid = id || input.id || uid('f');
  input.id = fid;
  const hintId = hint ? `${fid}-hint` : null;
  const errId = `${fid}-err`;
  const errEl = h('p', { class: 'ds-field__error', id: errId, hidden: !error }, icon('status-critical'), h('span', null, error || ''));
  const describedBy = [hintId, error ? errId : null].filter(Boolean).join(' ');
  if (describedBy) input.setAttribute('aria-describedby', describedBy);
  if (error) input.setAttribute('aria-invalid', 'true');
  const wrap = h(
    'div',
    { class: 'ds-field' },
    h('label', { class: 'ds-field__label', for: fid }, label, required ? t('form.required') : null),
    input,
    hint ? h('p', { class: 'ds-field__hint', id: hintId }, hint) : null,
    errEl,
  );
  wrap.setError = (msg) => {
    const span = errEl.querySelector('span');
    span.textContent = msg || '';
    errEl.hidden = !msg;
    const ids = [hintId, msg ? errId : null].filter(Boolean).join(' ');
    if (ids) input.setAttribute('aria-describedby', ids);
    else input.removeAttribute('aria-describedby');
    if (msg) input.setAttribute('aria-invalid', 'true');
    else input.removeAttribute('aria-invalid');
  };
  return wrap;
}

/** 文字輸入框 */
export function input({ type = 'text', name, value = '', autocomplete, mono, inputmode, placeholder, maxlength, cls, required } = {}) {
  return h('input', {
    class: ['ds-input', mono ? 'ds-input--mono' : null, cls],
    type,
    name,
    value,
    autocomplete,
    inputmode,
    placeholder,
    maxlength,
    spellcheck: type === 'text' ? 'false' : null,
    'aria-required': required ? 'true' : null,
  });
}

/** 複製按鈕（成功時暫時顯示「已複製」並宣告；§6.2 SecretField） */
export function copyButton(text, label) {
  const txt = h('span', null, t('common.copy'));
  const b = h(
    'button',
    { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', 'aria-label': label || null },
    icon('copy'),
    txt,
  );
  b.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(typeof text === 'function' ? text() : text);
      txt.textContent = t('common.copied');
      announce(t('common.copied'));
      setTimeout(() => {
        txt.textContent = t('common.copy');
      }, 2000);
    } catch {
      announce(t('common.copyFailed'));
    }
  });
  return b;
}

/** 等寬的程式碼標籤 */
export function codeTag(text) {
  return h('code', { class: 'k-code-tag' }, text);
}

/** 描述清單 */
export function dl(rows, cls = 'k-dl') {
  return h(
    'dl',
    { class: cls },
    rows.filter(Boolean).map(([k, v]) => [h('dt', null, k), h('dd', null, v == null || v === '' ? '—' : v)]),
  );
}

/** 骨架（區塊載入中） */
export function skeletonBlock(lines = 3) {
  return h(
    'div',
    { 'aria-busy': 'true' },
    h('span', { class: 'ds-visually-hidden' }, t('common.loading')),
    Array.from({ length: lines }, () => h('div', { class: 'ds-skeleton', 'aria-hidden': 'true', style: { height: '14px', margin: '8px 0', width: '80%' } })),
  );
}
