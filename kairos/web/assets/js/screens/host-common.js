// 主機相關畫面共用的呈現（設計包 ia.md §5：狀態、附加標記；screens.md S05「最後中斷原因」對照表）。

import { h } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import * as fmt from '../core/format.js';
import { statusBadge } from '../ui/components.js';

/** 離線原因的中文（協定代碼以等寬字附在後面；ia.md §8） */
export function reasonText(code) {
  if (!code) return null;
  return has(`reason.${code}`) ? t(`reason.${code}`) : code;
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
