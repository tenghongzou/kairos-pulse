// 註冊與憑證畫面（S12～S15）的共用呈現：動作對話框（L1 表單、L2 輸入確認）、註冊端點模式與簽發方式的提示、
// 異常旗標、申請與 token 的狀態標籤、進度條、分頁、下載文字檔。
//
// 對話框沿用 ui/dialog.js 的 openDialog（原生 <dialog>）；這裡另外處理「送出中、錯誤留在對話框內、輸入確認字串」，
// 讓 L2（DangerConfirmDialog，screens.md §0.3）在送出失敗時不必重新輸入。

import { h, icon, uid, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import { openDialog } from '../ui/dialog.js';
import { inlineAlert, errorAlert, setBusy, neutralBadge, statusBadge, copyButton } from '../ui/components.js';

/**
 * 動作對話框：L1（表單＋動詞按鈕）或 L2（另需輸入確認字串）。
 * @param {object} o
 *   title        問句標題
 *   lead         L2 的醒目說明（危險圖示＋粗體）
 *   content      節點或節點陣列（後果說明、欄位）
 *   phrase       需要輸入的確認字串（L2）；沒有時為 L1
 *   confirmText  動詞按鈕
 *   danger       危險樣式
 *   size         sm／lg
 *   collect()    送出前收集與檢查欄位；回傳 null 表示有錯誤（由呼叫端顯示在欄位下）
 *   submit(v)    送出（async）；丟出錯誤時以三段式顯示在對話框內，不關閉
 * @returns {Promise<any|null>}  submit 的結果；取消時 null
 */
export function actionDialog(o) {
  return new Promise((resolve) => {
    let done = false;
    const errBox = h('div', { class: 'k-stack--sm' });
    let phraseInput = null;
    const okBtn = h(
      'button',
      { type: 'button', class: ['ds-button', o.danger ? 'ds-button--danger' : 'ds-button--primary'] },
      o.confirmText,
    );
    const cancelBtn = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
    const parts = [];
    if (o.lead) parts.push(h('p', { class: 'k-danger-lead' }, icon('status-critical'), h('span', null, o.lead)));
    if (o.content) parts.push(...(Array.isArray(o.content) ? o.content : [o.content]));
    if (o.phrase) {
      const id = uid('phrase');
      const hintId = `${id}-hint`;
      phraseInput = h('input', {
        class: 'ds-input',
        id,
        type: 'text',
        autocomplete: 'off',
        spellcheck: 'false',
        'aria-describedby': hintId,
      });
      parts.push(
        h(
          'div',
          { class: 'ds-field' },
          h('label', { class: 'ds-field__label', for: id }, t('pki.dlg.typeToConfirm'), ' ', h('code', { class: 'k-code-tag' }, o.phrase)),
          phraseInput,
          h('p', { class: 'ds-field__hint', id: hintId }, t('pki.dlg.typeHint')),
        ),
      );
      const sync = () => {
        const ok = phraseInput.value.trim() === o.phrase;
        okBtn.disabled = !ok;
      };
      phraseInput.addEventListener('input', sync);
      sync();
    }
    parts.push(errBox);
    const finish = (v) => {
      if (done) return;
      done = true;
      d.close();
      resolve(v);
    };
    cancelBtn.addEventListener('click', () => finish(null));
    okBtn.addEventListener('click', async () => {
      if (phraseInput && phraseInput.value.trim() !== o.phrase) return;
      const values = o.collect ? o.collect() : {};
      if (values == null) return;
      mount(errBox);
      setBusy(okBtn, true, t('common.working'));
      cancelBtn.disabled = true;
      try {
        const r = o.submit ? await o.submit(values) : values;
        setBusy(okBtn, false, t('common.working'));
        finish(r == null ? true : r);
      } catch (err) {
        setBusy(okBtn, false, t('common.working'));
        cancelBtn.disabled = false;
        if (phraseInput) okBtn.disabled = phraseInput.value.trim() !== o.phrase;
        // 欄位格式錯誤已顯示在欄位下（README §4.3），不另外顯示三段式
        if (err && err.fieldShown) return;
        const box = errorAlert(err, { title: o.errorTitle });
        mount(errBox, box);
        box.focus();
      }
    });
    const d = openDialog({
      title: o.title,
      size: o.size || 'sm',
      danger: o.danger,
      body: h('div', { class: 'k-stack--sm' }, parts),
      actions: [cancelBtn, okBtn],
      onClose: () => {
        if (!done) {
          done = true;
          resolve(null);
        }
      },
    });
    // 焦點：有欄位時第一個欄位，否則「取消」（L1 的慣例，避免誤按動詞）
    const first = d.el.querySelector('.ds-dialog__body input:not([type=hidden]), .ds-dialog__body select, .ds-dialog__body textarea');
    (first || cancelBtn).focus();
  });
}

/** 選擇欄位（label 在上） */
export function selectField({ label, name, options, value, hint, required }) {
  const id = uid('sel');
  const sel = h(
    'select',
    { class: 'ds-select', id, name, 'aria-required': required ? 'true' : null },
    options.map(([v, text]) => h('option', { value: v, selected: v === value }, text)),
  );
  const hintId = hint ? `${id}-hint` : null;
  if (hintId) sel.setAttribute('aria-describedby', hintId);
  const el = h(
    'div',
    { class: 'ds-field' },
    h('label', { class: 'ds-field__label', for: id }, label, required ? t('form.required') : null),
    sel,
    hint ? h('p', { class: 'ds-field__hint', id: hintId }, hint) : null,
  );
  return { el, input: sel };
}

/** 文字區塊欄位（含錯誤） */
export function textareaField({ label, name, hint, required, rows = 3, mono }) {
  const id = uid('ta');
  const ta = h('textarea', { class: ['ds-textarea', mono ? 'ds-input--mono' : null], id, name, rows, spellcheck: 'false', 'aria-required': required ? 'true' : null });
  const hintId = `${id}-hint`;
  const errId = `${id}-err`;
  const err = h('p', { class: 'ds-field__error', id: errId, hidden: true }, icon('status-critical'), h('span'));
  const el = h(
    'div',
    { class: 'ds-field' },
    h('label', { class: 'ds-field__label', for: id }, label, required ? t('form.required') : null),
    ta,
    hint ? h('p', { class: 'ds-field__hint', id: hintId }, hint) : null,
    err,
  );
  const desc = (on) => [hint ? hintId : null, on ? errId : null].filter(Boolean).join(' ');
  if (desc(false)) ta.setAttribute('aria-describedby', desc(false));
  el.setError = (msg) => {
    err.querySelector('span').textContent = msg || '';
    err.hidden = !msg;
    const d = desc(Boolean(msg));
    if (d) ta.setAttribute('aria-describedby', d);
    if (msg) ta.setAttribute('aria-invalid', 'true');
    else ta.removeAttribute('aria-invalid');
  };
  return { el, input: ta };
}

/** 核取方塊（說明以 aria-describedby 關聯） */
export function checkbox({ label, name, checked, hint, disabled, why }) {
  const id = uid('cb');
  const hintId = hint || why ? `${id}-hint` : null;
  const cb = h('input', { type: 'checkbox', id, name, checked, disabled, 'aria-describedby': hintId });
  const el = h(
    'div',
    { class: 'ds-field' },
    h('label', { class: 'ds-checkbox', for: id }, cb, label),
    hint || why ? h('p', { class: 'ds-field__hint', id: hintId }, why || hint) : null,
  );
  return { el, input: cb };
}

/** 分頁（網址式；ds-tabs 的 nav＋連結，aria-current 標示目前） */
export function linkTabs(label, items) {
  return h(
    'nav',
    { class: 'ds-tabs', 'aria-label': label },
    items.map((it) => h('a', { href: it.href, 'aria-current': it.current ? 'page' : null, dataset: { tab: it.id } }, it.text, it.count != null ? [' ', h('span', { class: 'num' }, it.count)] : null)),
  );
}

/** 進度條（ProgressBar；有文字） */
export function progressBar(now, max, label) {
  const pct = max > 0 ? Math.min(100, Math.round((now / max) * 100)) : 0;
  return h(
    'div',
    { class: 'ds-progress', role: 'progressbar', 'aria-valuemin': '0', 'aria-valuemax': String(max), 'aria-valuenow': String(now), 'aria-label': label },
    h('div', { class: 'ds-progress__track' }, h('div', { class: 'ds-progress__bar', style: { width: `${pct}%` } })),
    h('span', { class: 'ds-progress__text' }, `${now.toLocaleString('en-US')}／${max.toLocaleString('en-US')}`),
  );
}

/**
 * CodeBlock：等寬、可換行，「複製」在區塊右上（copy 為 null 時不提供複製，例如註冊端點不是開放）。
 * 內容在 <code> 中（textContent 不含按鈕文字）。
 */
export function codeBlock(text, { labelledby, copy, copyLabel, id } = {}) {
  return h(
    'pre',
    { class: 'ds-code-block', 'aria-labelledby': labelledby, id },
    h('code', null, text),
    copy ? copyButton(copy, copyLabel) : null,
  );
}

/** 文字檔下載（不經伺服器；CSR 佇列、憑證 PEM） */
export function downloadText(filename, text, type = 'text/plain') {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = h('a', { href: url, download: filename, dataset: { native: '1' }, hidden: true });
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

// ---------------------------------------------------------------------------
// 註冊端點模式與簽發方式（GET /api/v1/enroll/mode；API.md 第 11.4、11.7 節）

/** 讀取模式（失敗時 null：提示是附加資訊，不拖垮整頁） */
export async function fetchMode() {
  try {
    return await api('/api/v1/enroll/mode');
  } catch {
    return null;
  }
}

/**
 * 簽發方式的提示（S12 錯誤、S13「已核准，待簽發」、S15）：
 *   離線簽發 KRS-E1302 → info；線上但私鑰無法使用 KRS-E1303 → critical（error 樣式）。
 */
export function issuanceAlert(mode, { where = 'tokens' } = {}) {
  const iss = mode && mode.issuance;
  if (!iss || iss.status === 'online') return null;
  if (iss.status === 'offline') {
    return inlineAlert({
      kind: 'info',
      title: t('pki.issuance.offline.title'),
      code: 'KRS-E1302',
      body: t(where === 'pending' ? 'pki.issuance.offline.pending' : 'pki.issuance.offline.body'),
      actions: [h('a', { class: 'ds-button ds-button--secondary ds-button--sm', href: '/certificates?state=pending_issuance' }, t('pki.issuance.toQueue'))],
    });
  }
  return inlineAlert({
    kind: 'error',
    title: t('pki.issuance.unavailable.title'),
    code: 'KRS-E1303',
    body: h(
      'div',
      null,
      h('p', null, t('pki.issuance.unavailable.body')),
      iss.reason ? h('p', { class: 'k-code-line' }, iss.reason) : null,
    ),
    actions: where === 'ca' ? null : [h('a', { class: 'ds-button ds-button--secondary ds-button--sm', href: '/certificates/ca' }, t('pki.issuance.toCa'))],
  });
}

/** S12 頁首的註冊端點模式提示（decisions/enroll-pause.md §4.7）；開放時 null */
export function modeAlert(mode) {
  if (!mode) return null;
  if (mode.state === 'open_paused') {
    return inlineAlert({ kind: 'info', title: t('pki.mode.paused.title'), body: t('pki.mode.paused.body') });
  }
  // 能改模式的人（管理員）看到設定的連結；其他人看到「請管理員…」（screens.md S12「註冊端點模式提示」）
  const canMode = session.can('enroll.mode:write');
  const toSettings = session.can('settings:read') ? h('a', { href: '/system/settings/security' }, t('pki.mode.toSettings')) : null;
  if (mode.state === 'renew_only') {
    return inlineAlert({ kind: 'warning', title: t('pki.mode.renewOnly.title'), body: h('div', null, h('p', null, t('pki.mode.renewOnly.body')), h('p', null, canMode ? toSettings : t('pki.mode.renewOnly.askAdmin'))) });
  }
  if (mode.state === 'closed') {
    return inlineAlert({ kind: 'warning', title: t('pki.mode.closed.title'), body: h('div', null, h('p', null, t('pki.mode.closed.body')), toSettings ? h('p', null, toSettings) : null) });
  }
  if (mode.listening === false && mode.not_listening_reason) {
    return inlineAlert({ kind: 'warning', title: t('pki.mode.notListening.title'), body: mode.not_listening_reason });
  }
  return null;
}

/** 註冊端點不是開放時，安裝字串與指令的「複製」停用（S12） */
export function installBlocked(mode) {
  return Boolean(mode && (mode.state === 'renew_only' || mode.state === 'closed'));
}

// ---------------------------------------------------------------------------
// 標籤

export const FLAGS = [
  'source_outside_cidrs',
  'duplicate_hostname',
  'agent_id_exists',
  'burst_from_ip',
  'license_overflow',
  'unverified_server',
  'reenroll_after_revocation',
  'fingerprint_mismatch',
];

export function flagLabel(f) {
  return FLAGS.includes(f) ? t(`enroll.flag.${f}`) : f;
}

/** 異常旗標（警告色＋圖示＋文字） */
export function flagBadge(f) {
  return h('span', { class: 'k-flag' }, icon('status-warning'), flagLabel(f));
}

export function kindLabel(k) {
  return ['enroll', 'grace_renewal', 'renewal'].includes(k) ? t(`enroll.kind.${k}`) : k;
}

/** 申請狀態：待核准沿用狀態詞彙；結束的申請為中性文字＋圖示（pulse/enrollment-ux.md 第 11 節） */
export function requestStateBadge(state) {
  switch (state) {
    case 'pending_approval':
      return statusBadge('pending');
    case 'approved':
      return statusBadge('pending', t('enroll.state.approved'));
    case 'issued':
      return neutralBadge(t('enroll.state.issued'), 'shield-check');
    case 'rejected':
      return neutralBadge(t('enroll.state.rejected'), 'shield-x');
    case 'withdrawn':
      return neutralBadge(t('enroll.state.withdrawn'), 'logout');
    case 'request_expired':
      return neutralBadge(t('enroll.state.request_expired'), 'clock');
    case 'replaced':
      return neutralBadge(t('enroll.state.replaced'), 'refresh');
    default:
      return neutralBadge(state);
  }
}

export function tokenStateLabel(s) {
  return ['active', 'expired', 'exhausted', 'disabled'].includes(s) ? t(`token.state.${s}`) : s;
}

/** 憑證狀態（S14：憑證狀態不是主機狀態，借用圖示與色彩，一律有文字） */
export function certStateBadge(state) {
  switch (state) {
    case 'valid':
      return statusBadge('online', t('cert.state.valid'));
    case 'expiring':
      return statusBadge('warning', t('cert.state.expiring'));
    case 'revocation_pending':
      return statusBadge('warning', t('cert.state.revocation_pending'));
    case 'expired_in_grace':
      return statusBadge('critical', t('cert.state.expired_in_grace'));
    case 'expired_beyond_grace':
      return statusBadge('critical', t('cert.state.expired_beyond_grace'));
    case 'revoked':
      return neutralBadge(t('cert.state.revoked'), 'shield-x');
    default:
      return neutralBadge(state);
  }
}

/** 序號的短顯示（1A2F…09）；完整值放在 title 與可及名稱 */
export function shortSerial(serial) {
  if (!serial) return '—';
  return serial.length > 10 ? `${serial.slice(0, 4)}…${serial.slice(-4)}` : serial;
}

/** agent_id 前 8 碼 */
export function shortId(id) {
  return id ? id.slice(0, 8) : '—';
}

/** OS 物件 → 一行文字 */
export function osLine(os) {
  if (!os || typeof os !== 'object') return '—';
  return [os.name, os.version].filter(Boolean).join(' ') || '—';
}

/** 所有 token（id → token；篩選、顯示名稱用）。失敗時空的 Map */
export async function loadTokenMap() {
  try {
    const list = await api('/api/v1/enroll/tokens?state=all&limit=1000');
    return new Map(list.map((x) => [x.token_id, x]));
  } catch {
    return new Map();
  }
}
