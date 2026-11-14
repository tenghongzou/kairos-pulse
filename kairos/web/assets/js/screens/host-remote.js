// S05 主機詳情的遠端管理（F8 MVP；設計包 screens.md S05「0.9 規格：遠端管理」、API.md 第 17 節）：
//
// - 概覽「狀態」卡片的「集中設定」摘要列（只需要 hosts:read；主機清單的 config_state）。
// - 「升級」卡片：安裝方式、遠端升級（install.upgrade；非就緒時附一行處理方式）、目前版本（hosts:read）；
//   有 rollouts:read 時另有最近 3 次升級、升級中的進度與「回滾失敗」的 InlineAlert，
//   「標記為已處理…」（rollouts:write；L1、原因必填 → POST /rollouts/{id}/hosts/{agent}/acknowledge）。
//   Windows 的安裝方式寫「遠端升級在 0.10 提供」（D51）。
// - 「設定」分頁（config:read）：GET /hosts/{id}/config 的生效值、集中設定的值與來源、本機覆蓋；
//   SSE config.host（這台主機）時重新載入。

import { h, mount } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import * as fmt from '../core/format.js';
import { card, dl, emptyState, errorState, errorAlert, inlineAlert, skeletonBlock, codeDetails } from '../ui/components.js';
import { toast } from '../ui/toast.js';
import { confirmL1 } from './admin-common.js';
import { refreshRolloutCount } from './pulse-boot.js';
import {
  configStateBadge,
  configStateLabel,
  upgradeBadge,
  upgradeFix,
  upgradeKey,
  methodLabel,
  isWindowsMethod,
  hostStateBadge,
  hostStateLabel,
  degradedNote,
  valueText,
  flatten,
  isUnavailable,
} from './pulse-common.js';

const IN_PROGRESS = ['offered', 'accepted', 'staged', 'applying'];

/** 概覽「狀態」卡片的「集中設定」列（hosts:read；有 config:read 時連到「設定」分頁） */
export function configSummaryRow(hst) {
  if (!hst.config_state) return null;
  const v = [configStateBadge(hst.config_state)];
  if (session.can('config:read')) v.push(' ', h('a', { href: `/hosts/${hst.agent_id}/config` }, t('host.tab.config')));
  return [t('rm.host.configRow'), v];
}

/** 「升級」卡片 */
export function upgradeCard(hst) {
  const inst = hst.install;
  const key = upgradeKey(inst);
  const win = isWindowsMethod(inst);
  const fix = win ? null : upgradeFix(key);
  const rows = [
    [t('rm.host.installRow'), methodLabel(inst)],
    win ? null : [t('rm.host.upgradeState'), [upgradeBadge(key), fix ? h('span', { class: 'k-sub' }, fix) : null]],
    [t('rm.host.version'), hst.agent_version ? `Pulse ${hst.agent_version}` : '—'],
    // Pulse 宣告的 install.external_url（REMOTE.md 第 2.1 節）：有宣告時才顯示；不認得的值原樣顯示
    !win && inst && inst.external_url ? [t('rm.host.externalUrl'), h('span', { dataset: { externalUrl: inst.external_url } }, has(`rm.ext.${inst.external_url}`) ? t(`rm.ext.${inst.external_url}`) : t('rm.up.other', { v: inst.external_url }))] : null,
    // Pulse 宣告的 install.rollback（REMOTE.md 第 2.1 節；ADR-0003 階段 2）：有宣告時才顯示；不認得的值原樣顯示
    !win && inst && inst.rollback ? [t('rm.host.rollback'), h('span', { dataset: { rollback: inst.rollback } }, has(`rm.rb.${inst.rollback}`) ? t(`rm.rb.${inst.rollback}`) : t('rm.up.other', { v: inst.rollback }))] : null,
  ];
  const alertBox = h('div');
  const recent = h('div');
  const body = [alertBox, dl(rows), recent];
  if (session.can('rollouts:read')) {
    mount(recent, skeletonBlock(2));
    loadUpgrades(hst, alertBox, recent);
  }
  return card({ title: t('rm.host.upgradeCard'), body, id: 'host-upgrade-title' });
}

async function loadUpgrades(hst, alertBox, recent) {
  let d;
  try {
    d = await api(`/api/v1/hosts/${encodeURIComponent(hst.agent_id)}/upgrade`);
  } catch (err) {
    // 清冊沒有這台（從未連線）或功能關閉：不顯示紀錄
    if (err.code === 'host_not_found' || isUnavailable(err)) {
      mount(recent);
      return;
    }
    mount(recent, errorState(err, { title: t('rm.host.recentFailed'), onRetry: () => loadUpgrades(hst, alertBox, recent) }));
    return;
  }
  const list = d.upgrades || [];
  // 回滾失敗（尚未標記為已處理）
  const failed = list.find((u) => u.state === 'rollback_failed' && !u.acknowledged_at);
  let failedAlert = null;
  if (failed) {
    const actions = session.can('rollouts:write')
      ? [h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => acknowledge(hst, failed, alertBox, recent) } }, t('rm.host.ack'))]
      : null;
    failedAlert = inlineAlert({
      kind: 'error',
      title: t('rm.host.rbTitle'),
      code: failed.code || 'PLS-E8408',
      body: h('p', null, t('rm.host.rbBody', { to: failed.to_version || '—', from: failed.from_version || '—' })),
      actions,
    });
  }
  // 需要修復套件（降級回滾之後；package_repair_needed 由伺服器依 Pulse 的宣告與升級紀錄算出）
  const degraded = list.find((u) => Array.isArray(u.degraded));
  const repairAlert = d.package_repair_needed
    ? h(
        'div',
        { id: 'host-package-repair' },
        inlineAlert({
          kind: 'warning',
          title: t('rm.host.repairTitle'),
          code: (degraded && degraded.code) || 'PLS-W8418',
          body: [h('p', null, t('rm.host.repairBody')), degraded ? h('p', null, degradedNote(degraded.degraded)) : null],
        }),
      )
    : null;
  mount(alertBox, failedAlert, repairAlert);
  const top = list.slice(0, 3);
  const cur = list.find((u) => IN_PROGRESS.includes(u.state));
  let progressLine = null;
  if (cur) {
    const p = cur.progress && cur.progress.total ? fmt.pct((cur.progress.bytes / cur.progress.total) * 100) : null;
    const text =
      cur.state === 'applying'
        ? t('rm.host.upgradingApply', { v: cur.to_version || '—' })
        : p && cur.state === 'accepted'
          ? t('rm.host.upgradingPct', { p, v: cur.to_version || '—' })
          : t('rm.host.upgrading', { state: hostStateLabel(cur.state), v: cur.to_version || '—' });
    progressLine = h('p', { class: 'strong', role: 'status' }, text);
  }
  if (!top.length) {
    mount(recent, progressLine, h('p', { class: 'muted' }, t('rm.host.recentNone')));
    return;
  }
  mount(
    recent,
    progressLine,
    h('h3', { class: 'k-section-title' }, t('rm.host.recent')),
    h(
      'div',
      { class: 'k-scroll-x' },
      h(
        'table',
        { class: 'k-mini-table' },
        h('caption', { class: 'ds-visually-hidden' }, t('rm.host.recentCaption')),
        h(
          'thead',
          null,
          h(
            'tr',
            null,
            h('th', { scope: 'col' }, t('rm.host.col.time', { tz: fmt.tzLabel() })),
            h('th', { scope: 'col' }, t('rm.host.col.versions')),
            h('th', { scope: 'col' }, t('rm.host.col.result')),
            h('th', { scope: 'col' }, t('rm.host.col.rollout')),
          ),
        ),
        h(
          'tbody',
          null,
          top.map((u) =>
            h(
              'tr',
              null,
              h('td', null, fmt.dateTime(u.finished_at || u.updated_at, { seconds: false })),
              h('td', null, t('rm.versions', { from: u.from_version || '—', to: u.to_version || '—' })),
              h('td', null, hostStateBadge(u.state), u.code ? [' ', h('code', { class: 'k-code-tag' }, u.code)] : null, degradedNote(u.degraded)),
              h('td', null, session.can('rollouts:read') ? h('a', { href: `/pulse/rollouts/${encodeURIComponent(u.rollout_id)}` }, u.rollout_id) : u.rollout_id),
            ),
          ),
        ),
      ),
    ),
  );
}

function acknowledge(hst, u, alertBox, recent) {
  confirmL1({
    title: t('rm.host.ackTitle'),
    body: t('rm.host.ackBody'),
    confirmText: t('rm.host.ackConfirm'),
    reason: 'required',
    onConfirm: async ({ reason, errBox }) => {
      try {
        await api(`/api/v1/rollouts/${encodeURIComponent(u.rollout_id)}/hosts/${encodeURIComponent(hst.agent_id)}/acknowledge`, { method: 'POST', body: { note: reason } });
        toast({ kind: 'success', title: t('rm.host.acked') });
        refreshRolloutCount();
        loadUpgrades(hst, alertBox, recent);
        return true;
      } catch (err) {
        const a = errorAlert(err, { title: t('rm.host.ackFailed') });
        mount(errBox, a);
        a.focus();
        return false;
      }
    },
  });
}

// ---------------------------------------------------------------------------
// 「設定」分頁

function stateLine(d, hst) {
  const rev = fmt.num(d.desired_rev || 0);
  const applied = fmt.num(d.applied_rev || 0);
  const n = Array.isArray(d.overridden) ? d.overridden.length : 0;
  switch (d.config_state) {
    case 'in_sync':
      return d.desired_rev ? t('rm.hc.state.in_sync', { rev }) : t('rm.hc.state.noDesired');
    case 'overridden':
      return t('rm.hc.state.overridden', { rev, n: fmt.num(n) });
    case 'pending':
      return hst.status === 'online' ? t('rm.hc.state.pending', { rev, applied }) : t('rm.hc.state.pendingOffline', { rev });
    case 'rejected':
      return t('rm.hc.state.rejected', { rev, applied, code: d.code || 'PLS-W0501' });
    case 'rolled_back':
      return t('rm.hc.state.rolled_back', { code: d.code || 'PLS-W0504' });
    case 'never_reported':
      return t('rm.hc.state.never_reported', { rev });
    default:
      return configStateLabel(d.config_state);
  }
}

function sourceText(key, d, desiredMap, overridden) {
  const o = overridden.get(key);
  if (o) {
    const s = o.source === 'env' ? t('rm.hc.src.env') : o.source === 'file' ? t('rm.hc.src.file') : o.source || '';
    return h('span', { class: 'k-row k-row--tight', title: t('rm.hc.overrideNote') }, h('span', null, t('rm.hc.src.overridden', { s })));
  }
  const dk = desiredMap.get(key);
  if (dk) return t('rm.hc.src.profile', { p: dk.source || '—' });
  return t('rm.hc.src.local');
}

export function configTab(hst, el) {
  let disposed = false;
  let timer = null;
  async function load() {
    let d;
    try {
      d = await api(`/api/v1/hosts/${encodeURIComponent(hst.agent_id)}/config`);
    } catch (err) {
      if (disposed) return;
      if (err.code === 'host_not_found' || err.code === 'not_found') {
        mount(el, emptyState({ title: t('rm.hc.unsupported'), iconName: 'settings' }));
        return;
      }
      mount(el, errorState(err, { title: t('rm.hc.loadFailed'), onRetry: load }));
      return;
    }
    if (disposed) return;
    if (d.config_state === 'unsupported') {
      mount(el, card({ title: t('host.tab.config'), body: emptyState({ title: t('rm.hc.unsupported'), iconName: 'settings' }) }));
      return;
    }
    if (d.config_state === 'disabled') {
      mount(el, card({ title: t('host.tab.config'), body: emptyState({ title: configStateLabel('disabled'), body: t('rm.hc.disabled'), iconName: 'settings' }) }));
      return;
    }
    const desiredMap = new Map((d.desired_keys || []).map((k) => [k.key, k]));
    const overridden = new Map((Array.isArray(d.overridden) ? d.overridden : []).map((o) => [o.key, o]));
    const effective = d.effective && typeof d.effective === 'object' ? flatten(d.effective) : {};
    const keys = [...new Set([...Object.keys(effective), ...desiredMap.keys()])].sort();
    const warn =
      d.config_state === 'rejected' || d.config_state === 'rolled_back'
        ? inlineAlert({
            kind: 'warning',
            title: stateLine(d, hst),
            body: [
              Array.isArray(d.errors) && d.errors.length
                ? [h('p', { class: 'strong' }, t('rm.hc.errors')), h('ul', { class: 'k-plain-list' }, d.errors.map((e) => h('li', null, h('code', { class: 'k-code-tag' }, e.key), ' ', e.message || (has(`rm.ed.fieldErr.${e.reason}`) ? t(`rm.ed.fieldErr.${e.reason}`) : e.reason))))]
                : null,
              codeDetails({ productCode: d.config_state === 'rejected' ? 'KRS-W0501' : 'KRS-W0504' }),
            ],
          })
        : null;
    const ignored = Array.isArray(d.ignored) && d.ignored.length
      ? h('div', null, h('h3', { class: 'k-section-title' }, t('rm.hc.ignored')), h('ul', { class: 'k-plain-list' }, d.ignored.map((i) => h('li', null, t('rm.hc.ignoredLine', { key: i.key, reason: has(`rm.hc.ignored.${i.reason}`) ? t(`rm.hc.ignored.${i.reason}`) : i.reason })))))
      : null;
    const table = keys.length
      ? h(
          'div',
          { class: 'k-scroll-x' },
          h(
            'table',
            { class: 'k-mini-table', id: 'host-config' },
            h('caption', { class: 'ds-visually-hidden' }, t('rm.hc.caption')),
            h(
              'thead',
              null,
              h('tr', null, h('th', { scope: 'col' }, t('rm.hc.col.key')), h('th', { scope: 'col' }, t('rm.hc.col.effective')), h('th', { scope: 'col' }, t('rm.hc.col.desired')), h('th', { scope: 'col' }, t('rm.hc.col.source'))),
            ),
            h(
              'tbody',
              null,
              keys.map((k) => {
                const dk = desiredMap.get(k);
                return h(
                  'tr',
                  { dataset: { key: k } },
                  h('th', { scope: 'row' }, h('code', { class: 'k-code-tag' }, k)),
                  h('td', null, k in effective ? valueText(effective[k]) : '—'),
                  h('td', null, dk ? [valueText(dk.value), dk.source ? h('span', { class: 'k-sub' }, dk.source) : null] : '—'),
                  h('td', null, sourceText(k, d, desiredMap, overridden)),
                );
              }),
            ),
          ),
        )
      : h('p', { class: 'muted' }, t('rm.hc.noEffective'));
    mount(
      el,
      warn,
      card({
        title: t('host.tab.config'),
        actions: d.config_state ? configStateBadge(d.config_state) : null,
        body: [
          d.config_state === 'rejected' || d.config_state === 'rolled_back' ? null : h('p', { class: 'strong', role: 'status' }, stateLine(d, hst)),
          d.reported_at ? h('p', { class: 'caption' }, t('rm.hc.reportedAt', { t: `${fmt.dateTime(d.reported_at)} ${fmt.tzLabel()}` })) : null,
          table,
          overridden.size ? h('p', { class: 'caption' }, t('rm.hc.overrideNote')) : null,
          ignored,
        ],
      }),
    );
  }
  mount(el, skeletonBlock(6));
  load();
  const unsub = sse.subscribe((ev) => {
    if (ev.type !== 'config.host' || !ev.data || ev.data.agent_id !== hst.agent_id) return;
    clearTimeout(timer);
    timer = setTimeout(load, 500);
  });
  return () => {
    disposed = true;
    clearTimeout(timer);
    unsub();
  };
}
