// 主機相關畫面共用的呈現（設計包 ia.md §5：狀態、附加標記；screens.md S05「最後中斷原因」對照表）。

import { h } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import * as fmt from '../core/format.js';
import { statusBadge, codeTag } from '../ui/components.js';

/** 認得的離線原因（strings.js 有中文） */
function knownReason(code) {
  return Boolean(code) && has(`reason.${code}`);
}

/**
 * 離線原因的中文（協定代碼以等寬字附在後面；ia.md §8）。不認得的值（較新版 Kairos 的列舉）一律「其他原因」，
 * 不因新值而報錯（設計包 §31.6、S05「server_lost」）
 */
export function reasonText(code) {
  if (!code) return null;
  return knownReason(code) ? t(`reason.${code}`) : t('reason.other');
}

/** 詳情與紀錄中的離線原因：中文＋協定代碼；不認得的值為「其他原因」，原始值放在「技術細節」 */
export function reasonDetail(code) {
  if (!code) return null;
  if (knownReason(code)) return [t(`reason.${code}`), ' ', codeTag(code)];
  return [
    t('reason.other'),
    h('details', { class: 'k-details' }, h('summary', null, t('err.label.technical')), h('p', { class: 'k-code-line' }, `offline_reason = ${code}`)),
  ];
}

/** 篩選選項的文字：不認得的值寫出原始值，避免多個「其他原因」無法分辨 */
export function reasonFacet(code) {
  if (!code) return null;
  return knownReason(code) ? t(`reason.${code}`) : t('reason.otherFacet', { code });
}

/** 顯示名稱（沒有時為主機名稱） */
export function hostName(hst) {
  return hst.display_name || hst.hostname;
}

/** 主機名稱（第一行，連到詳情）＋ agent_id 前 8 碼（第二行） */
export function hostNameCell(hst, { link = true } = {}) {
  const name = hostName(hst);
  return [
    link
      ? h('a', { href: `/hosts/${hst.agent_id}`, class: 'k-host-name', title: name }, name)
      : h('span', { class: 'k-host-name', title: name }, name),
    h('span', { class: 'k-host-id' }, hst.agent_id.slice(0, 8)),
  ];
}

/** 狀態欄：StatusBadge＋附加標記（離線原因、長期離線、從未連線、暖機中；ia.md §5.2） */
export function statusCell(hst) {
  const d = hst.status_detail || {};
  const marks = [];
  if (hst.status === 'offline' && d.offline_reason) marks.push(reasonText(d.offline_reason));
  if (d.long_offline) marks.push(t('host.longOffline'));
  if (d.never_connected) marks.push(t('host.neverConnected'));
  if (d.starting) marks.push(t('host.starting'));
  return [statusBadge(hst.status), marks.length ? h('span', { class: 'k-sub' }, marks.join(' · ')) : null];
}

/** OS 的一行文字 */
export function osText(os) {
  if (!os) return '—';
  return [os.name, os.version].filter(Boolean).join(' ') || '—';
}

/** 記憶體使用比例 */
export function memRatio(r) {
  if (!r || !r.memory_total_bytes || r.memory_used_bytes == null) return null;
  return (r.memory_used_bytes / r.memory_total_bytes) * 100;
}

/** 離線持續（線上為「—」） */
export function offlineFor(hst) {
  return hst.offline_for_secs == null ? '—' : fmt.duration(hst.offline_for_secs);
}

/** F4（指標擴充）需要的 Pulse 版本（設計包 §30：舊版 Pulse 的空狀態） */
export const F4_VERSION = '0.8.0';

/** agent 版本是否早於 0.8.0（不回報負載、置換空間、磁碟讀寫、行程、運作時間與資產）；不知道時 false */
export function isOldPulse(hst) {
  const m = /^(\d+)\.(\d+)/.exec(hst.agent_version || '');
  if (!m) return false;
  return Number(m[1]) === 0 && Number(m[2]) < 8;
}

/** 作業系統家族（API 的 os_family；沒有時由 OS 名稱推斷） */
export function osFamily(hst) {
  if (hst.os_family) return hst.os_family;
  const n = ((hst.os && hst.os.name) || '').toLowerCase();
  if (!n) return null;
  return n.includes('windows') ? 'windows' : 'linux';
}

/** 「—」＋灰字的「需要 Pulse 0.8.0 以上」（表格與卡片的一列；完整句放在 title） */
export function oldPulseCell() {
  return h('span', { class: 'muted', title: t('host.na.oldPulse', { version: F4_VERSION }) }, `— ${t('host.na.oldPulseShort', { version: F4_VERSION })}`);
}
