// API 錯誤 → 畫面上的三段式說明（設計包 copywriting.md §4：標題、原因、處理方式、代碼、技術細節）。
//
// 依序：
// 1. 回應的 product_code（KRS-…）在 error-codes.js（copywriting.md §4.2 範例）有三段式 → 用它；
//    呼叫端可以給更貼近情境的標題（例：「無法載入主機清單」），原因與處理方式仍用範例。
// 2. 機器代碼（error.code）有對應的就地說明（strings.js 的 err.<code>）→ 用它。
// 3. 其他：標題用呼叫端給的情境標題，原因為「Kairos 回應了未預期的錯誤」，伺服器的 message 只放技術細節
//    （API.md 第 1.2 節：message 給開發者看，不保證固定）。

import { ERROR_CODES } from './error-codes.js';
import { LOCAL_ERROR_CODES } from './error-codes-local.js';
import { t, has, permissionLabel, roleLabel } from './strings.js';

/** 代碼的總表說明（找不到時 null）：設計包產生的 error-codes.js 優先，其次是工程暫行的 error-codes-local.js */
export function lookup(code) {
  return (code && (ERROR_CODES[code] || LOCAL_ERROR_CODES[code])) || null;
}

/** 處理方式的文字 → 步驟陣列（「1. 」開頭的行為編號步驟；copywriting.md §4.1） */
export function fixSteps(fix) {
  if (!fix) return [];
  const lines = fix.split('\n').map((l) => l.trim()).filter(Boolean);
  return lines.map((l) => l.replace(/^\d+\.\s*/, ''));
}

/**
 * 錯誤的三段式。
 * @param {ApiError|Error} err
 * @param {object} ctx  title（情境標題，例如「無法載入主機清單」）
 * @returns {{title, reason, steps, code, technical}}
 */
export function describe(err, ctx = {}) {
  const code = err && err.productCode;
  const tech = [];
  if (err && err.status) tech.push(`HTTP ${err.status}`);
  if (err && err.code) tech.push(err.code);
  if (err && err.message && err.message !== err.code) tech.push(err.message);
  const technical = tech.join(' · ');
  const local = err && err.code && has(`err.${err.code}.title`) ? err.code : null;

  if (err && err.code === 'forbidden') {
    const perm = err.details && err.details.permission;
    const req = err.details && err.details.required;
    return {
      title: ctx.title || t('err.forbidden.title'),
      reason: perm
        ? t('err.forbidden.reasonPerm', { perm: permissionLabel(perm), role: roleLabel(req) || req || '—' })
        : t('err.forbidden.reason'),
      steps: [t('err.forbidden.fix')],
      code: code || 'KRS-E6104',
      technical,
    };
  }
  if (err && err.code === 'rate_limited') {
    return {
      title: t('err.rate_limited.title'),
      reason: t('err.rate_limited.reason', { n: err.retryAfter || 6 }),
      steps: [t('err.rate_limited.fix', { n: err.retryAfter || 6 })],
      code: code || 'KRS-W6011',
      technical,
    };
  }
  const entry = lookup(code);
  if (entry && entry.title) {
    return {
      title: ctx.title || entry.title,
      reason: ctx.title && ctx.title !== entry.title ? `${entry.title}。${entry.reason || ''}` : entry.reason || '',
      steps: fixSteps(entry.fix),
      code,
      technical,
    };
  }
  if (local) {
    return {
      title: ctx.title || t(`err.${local}.title`),
      reason: t(`err.${local}.reason`),
      steps: has(`err.${local}.fix`) ? [t(`err.${local}.fix`)] : [],
      code: code || null,
      technical,
    };
  }
  return {
    title: ctx.title || t('err.unexpected.title'),
    reason: entry ? entry.text : t('err.unexpected.reason'),
    steps: [t('err.unexpected.fix')],
    code: code || null,
    technical,
  };
}

/** 一行的錯誤訊息（Toast、欄位錯誤）：標題（代碼） */
export function short(err, ctx = {}) {
  const d = describe(err, ctx);
  return d.code ? `${d.title}（${d.code}）` : d.title;
}
