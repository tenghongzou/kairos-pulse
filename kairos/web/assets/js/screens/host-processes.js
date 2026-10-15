// S05 主機詳情的「行程」分頁（設計包 kairos/screens.md S05「行程分頁」、decisions/INTEGRATION.md §30 D46；
// API GET /api/v1/hosts/{id}/processes?at=，kairos/API.md 第 5.10 節）。
//
// - 兩張表（依 CPU、依記憶體，各前 N），排序依據的欄位標題寫「（排序依據）」並加 aria-sort。
// - 取樣時間一律寫出（顯示時區，精確到秒）；較早一筆（at = sampled_at − 1 ms）、較晚一筆、回到最新、
//   跳到時間（datetime-local；?at= 可分享）。切換後焦點留在按下的按鈕，aria-live 宣告。
// - 命令列只在 Pulse 有回報時出現：超過 48 個字元以 <details> 截斷＋展開，表格上方提示可能含敏感資訊。
// - 舊版 Pulse：EmptyState「這台主機的 Pulse 版本不回報此資訊」；新版沒有資料：「尚未收到回報」。

import { h, mount, announce } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as fmt from '../core/format.js';
import * as prefs from '../core/prefs.js';
import { card, emptyState, errorState, inlineAlert, skeletonBlock } from '../ui/components.js';
import { F4_VERSION, isOldPulse, osFamily } from './host-common.js';

const COMMAND_SHOWN = 48;
/** 「較晚一筆」往後找的範圍：行程排行每 60 秒一筆（Pulse processes.interval 預設） */
const LATER_WINDOW_MS = 90 * 1000;

/** datetime-local 的值（顯示時區的牆上時間）→ Unix 毫秒 */
function fromLocalInput(value) {
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2}))?$/.exec(value || '');
  if (!m) return null;
  const wall = Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +(m[6] || 0));
  const off = fmt.tzOffsetMinutes(wall, prefs.get('timeZone'));
  return wall - off * 60000;
}

/** Unix 毫秒 → datetime-local 的值（顯示時區） */
function toLocalInput(ms) {
  return fmt.dateTime(ms, { seconds: false }).replace(' ', 'T');
}

function commandCell(cmd) {
  if (!cmd) return '—';
  if (cmd.length <= COMMAND_SHOWN) return h('code', { class: 'k-code-line' }, cmd);
  return h('details', { class: 'k-details k-cmd' }, h('summary', null, h('code', null, `${cmd.slice(0, COMMAND_SHOWN)}…`)), h('code', { class: 'k-code-line' }, cmd));
}

function processTable(list, caption, sortKey, withCommand) {
  const sortTh = (key, label) =>
    h('th', { scope: 'col', class: key === 'name' || key === 'command' ? null : 'num', 'aria-sort': key === sortKey ? 'descending' : null }, label, key === sortKey ? h('span', { class: 'k-sub' }, t('host.processes.sortKey')) : null);
  return h(
    'div',
    { class: 'k-scroll-x' },
    h(
      'table',
      { class: 'k-mini-table' },
      h('caption', null, caption),
      h(
        'thead',
        null,
        h(
          'tr',
          null,
          h('th', { scope: 'col', class: 'num' }, t('host.processes.col.rank')),
          sortTh('name', t('host.processes.col.name')),
          sortTh('pid', t('host.processes.col.pid')),
          sortTh('cpu', t('host.processes.col.cpu')),
          sortTh('memory', t('host.processes.col.memory')),
          withCommand ? sortTh('command', t('host.processes.col.command')) : null,
        ),
      ),
      h(
        'tbody',
        null,
        list.length
          ? list.map((p, i) =>
              h(
                'tr',
                null,
                h('td', { class: 'num' }, String(i + 1)),
                h('td', null, h('span', { class: 'mono' }, p.name)),
                h('td', { class: 'num' }, String(p.pid)),
                h('td', { class: 'num' }, fmt.pct(p.cpu_usage)),
                h('td', { class: 'num' }, fmt.gib(p.memory)),
                withCommand ? h('td', null, commandCell(p.command)) : null,
              ),
            )
          : h('tr', null, h('td', { colspan: withCommand ? 6 : 5, class: 'muted' }, t('host.processes.empty'))),
      ),
    ),
  );
}

/**
 * 行程分頁。
 * @returns {() => void} 清除
 */
export function processesTab(hst, el, ctx) {
  const id = hst.agent_id;
  if (isOldPulse(hst)) {
    mount(
      el,
      card({
        title: t('host.processes.title'),
        body: emptyState({ title: t('host.na.oldPulseTitle'), body: t('host.na.oldPulseBody', { version: F4_VERSION, current: hst.agent_version || '—' }), iconName: 'info' }),
      }),
    );
    return () => {};
  }
  let ctrl = null;
  let latestMs = null;
  let current = null;
  const atParam = ctx.query.get('at');
  let at = atParam && !Number.isNaN(Date.parse(atParam)) ? Date.parse(atParam) : null;

  const sampled = h('p', { class: 'strong' });
  const live = h('p', { class: 'ds-visually-hidden', 'aria-live': 'polite' });
  const btn = (label, onClick) => h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: (ev) => { if (ev.currentTarget.getAttribute('aria-disabled') === 'true') return; onClick(); } } }, label);
  const prevBtn = btn(t('host.processes.prev'), () => go(current - 1, 'prev'));
  const nextBtn = btn(t('host.processes.next'), () => later());
  const latestBtn = btn(t('host.processes.latest'), () => go(null, 'latest'));
  const jumpInput = h('input', { class: 'ds-input', type: 'datetime-local', name: 'at', step: '60' });
  const jumpLabel = h('label', { class: 'k-labelled-select' }, t('host.processes.jump', { tz: fmt.tzLabel() }), jumpInput);
  const jumpBtn = btn(t('host.processes.go'), () => {
    const ms = fromLocalInput(jumpInput.value);
    if (ms == null) {
      announce(t('host.processes.badTime'));
      jumpInput.focus();
      return;
    }
    go(ms, 'jump');
  });
  jumpInput.addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter') {
      ev.preventDefault();
      jumpBtn.click();
    }
  });
  const retention = h('p', { class: 'caption' });
  const tables = h('div', null, skeletonBlock(6));
  const notes = h('div');
  mount(
    el,
    card({
      title: t('host.processes.title'),
      body: [
        sampled,
        live,
        h('div', { class: 'ds-button-row k-row-wrap' }, prevBtn, nextBtn, latestBtn, jumpLabel, jumpBtn),
        retention,
        tables,
        notes,
      ],
    }),
  );

  const setDisabled = (b, off) => {
    if (off) b.setAttribute('aria-disabled', 'true');
    else b.removeAttribute('aria-disabled');
  };

  async function fetchAt(ms, signal) {
    return api(`/api/v1/hosts/${id}/processes${qs(ms == null ? {} : { at: new Date(ms).toISOString() })}`, { signal });
  }

  function render(d) {
    const top = Math.max((d.top_cpu || []).length, (d.top_memory || []).length) || 5;
    retention.textContent = t('host.processes.retention', { days: fmt.num(d.retention_days ?? 7) });
    if (!d.sampled_at) {
      current = null;
      mount(sampled, '');
      setDisabled(prevBtn, true);
      setDisabled(nextBtn, at == null);
      mount(
        tables,
        at == null
          ? emptyState({ title: t('host.processes.none'), body: t('host.processes.noneBody'), iconName: 'clock' })
          : emptyState({ title: t('host.processes.outOfRetention'), body: t('host.processes.outOfRetentionBody', { days: fmt.num(d.retention_days ?? 7) }), iconName: 'clock' }),
      );
      mount(notes, null);
      return;
    }
    current = Date.parse(d.sampled_at);
    if (at == null) latestMs = current;
    mount(
      sampled,
      `${t('host.processes.sampledAt', { tz: fmt.tzLabel(current) })} ${fmt.dateTime(current)}`,
      h('span', { class: 'secondary' }, at == null ? t('host.processes.latestNote') : t('host.processes.atNote', { time: fmt.dateTime(at, { seconds: false }) })),
    );
    jumpInput.value = toLocalInput(current);
    setDisabled(nextBtn, latestMs != null && current >= latestMs);
    setDisabled(latestBtn, at == null);
    const withCommand = [...(d.top_cpu || []), ...(d.top_memory || [])].some((p) => p.command);
    const full = withCommand && [...(d.top_cpu || []), ...(d.top_memory || [])].some((p) => p.command && /\s/.test(p.command));
    mount(
      tables,
      withCommand
        ? inlineAlert({ kind: 'warning', title: t('host.processes.sensitive.title'), body: t(full ? 'host.processes.sensitive.full' : 'host.processes.sensitive.exe') })
        : null,
      h(
        'div',
        { class: 'k-grid k-grid--1-1' },
        processTable(d.top_cpu || [], t('host.processes.byCpu', { count: fmt.num(top) }), 'cpu', withCommand),
        processTable(d.top_memory || [], t('host.processes.byMemory', { count: fmt.num(top) }), 'memory', withCommand),
      ),
    );
    mount(
      notes,
      h('p', { class: 'caption' }, t('host.processes.note')),
      osFamily(hst) === 'windows' ? h('p', { class: 'caption' }, t('host.processes.noteWindows')) : null,
    );
  }

  async function go(ms, why) {
    if (ctrl) ctrl.abort();
    ctrl = new AbortController();
    try {
      const d = await fetchAt(ms, ctrl.signal);
      // 較早一筆：已經是最早的一筆時不換頁
      if (why === 'prev' && !d.sampled_at) {
        setDisabled(prevBtn, true);
        return;
      }
      at = ms;
      const u = new URLSearchParams(location.search);
      if (at == null) u.delete('at');
      else u.set('at', new Date(at).toISOString());
      history.replaceState(null, '', `${location.pathname}${u.toString() ? `?${u}` : ''}`);
      render(d);
      setDisabled(prevBtn, !d.sampled_at);
      if (d.sampled_at && why) live.textContent = t('host.processes.shown', { time: fmt.time(d.sampled_at) });
    } catch (e) {
      if (e.name !== 'AbortError') mount(tables, errorState(e, { title: t('host.processes.failed'), onRetry: () => go(at, null) }));
    }
  }

  async function later() {
    if (current == null) return go(null, 'latest');
    if (ctrl) ctrl.abort();
    ctrl = new AbortController();
    try {
      const d = await fetchAt(current + LATER_WINDOW_MS, ctrl.signal);
      if (!d.sampled_at || Date.parse(d.sampled_at) <= current) {
        // 這段時間內沒有較晚的快照：直接到最新
        return go(null, 'latest');
      }
      return go(Date.parse(d.sampled_at), 'next');
    } catch (e) {
      if (e.name !== 'AbortError') mount(tables, errorState(e, { title: t('host.processes.failed') }));
      return null;
    }
  }

  // 第一次：先取最新一筆（較晚一筆的上限），指定時間時再取那一筆
  (async () => {
    try {
      ctrl = new AbortController();
      const latest = await fetchAt(null, ctrl.signal);
      latestMs = latest.sampled_at ? Date.parse(latest.sampled_at) : null;
      if (at == null) render(latest);
      else await go(at, null);
    } catch (e) {
      if (e.name !== 'AbortError') mount(tables, errorState(e, { title: t('host.processes.failed'), onRetry: () => go(at, null) }));
    }
  })();
  return () => ctrl && ctrl.abort();
}
