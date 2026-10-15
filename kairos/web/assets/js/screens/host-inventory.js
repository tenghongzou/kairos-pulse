// S05 主機詳情的「資產資訊」分頁（設計包 kairos/screens.md S05「資產資訊分頁」、decisions/INTEGRATION.md
// §30 D47；API GET /api/v1/hosts/{id}/inventory、/inventory/changes，kairos/API.md 第 5.9 節）。
//
// - 作業系統與硬體（<dl>）、磁碟表、網路介面表、資產變更歷史（新到舊，「載入更多」）。
// - 序號只在這裡（與 S19）出現，不在主機清單。
// - Pulse 省略的欄位：顯示上一次的值；從未取得時「—（Pulse 沒有回報：可能沒有讀取權限，或設定為不回報）」。
// - 舊版 Pulse：「作業系統」一列取 hello 的 os，下方 EmptyState。
// - 最近一次的資產被拒（KRS-W2108）：頁首 InlineAlert，仍顯示上一次成功的內容。

import { h, mount } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as fmt from '../core/format.js';
import { card, dl, emptyState, errorState, inlineAlert, skeletonBlock, copyButton } from '../ui/components.js';
import { F4_VERSION, isOldPulse } from './host-common.js';

const CHANGES_PAGE = 50;

const notReported = () => h('span', { class: 'muted' }, t('host.inventory.notReported'));
const val = (v) => (v == null || v === '' ? notReported() : v);

function hwRows(inv, d, windows) {
  const os = inv.os || {};
  const cpu = inv.cpu || {};
  const mem = inv.memory || {};
  const sys = inv.system || {};
  const cpuDetail = [
    cpu.physical_cores != null && cpu.logical_cores != null
      ? t('host.inventory.cpuCores', { p: fmt.num(cpu.physical_cores), l: fmt.num(cpu.logical_cores) })
      : cpu.logical_cores != null
        ? t('host.inventory.cpuLogical', { l: fmt.num(cpu.logical_cores) })
        : null,
    cpu.frequency_mhz ? t('host.inventory.mhz', { n: fmt.num(cpu.frequency_mhz) }) : null,
  ]
    .filter(Boolean)
    .join(' · ');
  const vendorProduct = [sys.vendor, sys.product].filter(Boolean).join(' ');
  const bios = [sys.bios_vendor, sys.bios_version].filter(Boolean).join(' ');
  return [
    [t('host.inventory.os'), val(os.long_version || [os.name, os.version].filter(Boolean).join(' '))],
    [t(windows ? 'host.inventory.build' : 'host.inventory.kernel'), val(os.kernel)],
    [t('host.inventory.arch'), val(os.arch)],
    [t('host.inventory.cpu'), cpu.model || cpuDetail ? [cpu.model || '', cpuDetail ? h('span', { class: 'k-sub' }, cpuDetail) : null] : notReported()],
    [
      t('host.inventory.memory'),
      mem.total != null
        ? mem.swap_total != null
          ? t('host.inventory.memoryValue', { mem: fmt.gib(mem.total), swap: fmt.gib(mem.swap_total) })
          : fmt.gib(mem.total)
        : notReported(),
    ],
    [t('host.inventory.vendorProduct'), val(vendorProduct)],
    [t('host.inventory.serial'), sys.serial ? [h('span', { class: 'mono' }, sys.serial), ' ', copyButton(sys.serial, t('host.inventory.copySerial'))] : notReported()],
    [t('host.inventory.bios'), val(bios)],
    [t('host.inventory.bootTime'), d.boot_time ? `${fmt.dateTime(d.boot_time, { seconds: false })} ${fmt.tzLabel()}` : notReported()],
  ];
}

function disksTable(disks, filesystems) {
  const usage = new Map((filesystems || []).map((f) => [f.mountpoint, f]));
  const kind = (d) =>
    [has(`host.inventory.kind.${d.kind}`) ? t(`host.inventory.kind.${d.kind}`) : t('host.inventory.kind.unknown'), d.removable ? t('host.inventory.removable') : null, d.read_only ? t('host.inventory.readOnly') : null]
      .filter(Boolean)
      .join('、');
  return h(
    'div',
    { class: 'k-scroll-x' },
    h(
      'table',
      { class: 'k-mini-table' },
      h('caption', { class: 'ds-visually-hidden' }, t('host.inventory.disks')),
      h(
        'thead',
        null,
        h(
          'tr',
          null,
          [t('host.inventory.mountpoint'), t('host.inventory.device'), t('host.inventory.filesystem'), t('host.inventory.diskKind')].map((c) => h('th', { scope: 'col' }, c)),
          h('th', { scope: 'col', class: 'num' }, t('host.inventory.capacity')),
          h('th', { scope: 'col', class: 'num' }, t('host.inventory.used')),
        ),
      ),
      h(
        'tbody',
        null,
        disks.map((d) => {
          const u = usage.get(d.mountpoint);
          const full = u && u.usage != null && u.usage >= 95;
          return h(
            'tr',
            null,
            h('td', null, h('span', { class: 'mono' }, d.mountpoint)),
            h('td', null, d.device ? h('span', { class: 'mono' }, d.device) : '—'),
            h('td', null, d.fstype || '—'),
            h('td', null, kind(d)),
            h('td', { class: 'num' }, fmt.bytes(d.total)),
            h('td', { class: 'num' }, u && u.usage != null ? [fmt.pct(u.usage), u.estimated ? t('host.estimatedSuffix') : '', full ? h('span', { class: 'k-sub' }, t('host.values.diskFull')) : null] : '—'),
          );
        }),
      ),
    ),
  );
}

function ifacesTable(ifaces) {
  return h(
    'div',
    { class: 'k-scroll-x' },
    h(
      'table',
      { class: 'k-mini-table' },
      h('caption', { class: 'ds-visually-hidden' }, t('host.inventory.interfaces')),
      h(
        'thead',
        null,
        h('tr', null, [t('host.inventory.name'), t('host.inventory.mac'), t('host.inventory.ip')].map((c) => h('th', { scope: 'col' }, c)), h('th', { scope: 'col', class: 'num' }, t('host.inventory.mtu'))),
      ),
      h(
        'tbody',
        null,
        ifaces.map((i) =>
          h(
            'tr',
            null,
            h('td', null, h('span', { class: 'mono' }, i.name)),
            h('td', null, i.mac ? h('span', { class: 'mono' }, i.mac) : notReported()),
            h('td', null, i.addresses && i.addresses.length ? i.addresses.map((a) => h('div', { class: 'mono' }, a)) : notReported()),
            h('td', { class: 'num' }, i.mtu != null ? fmt.num(i.mtu) : '—'),
          ),
        ),
      ),
    ),
  );
}

/** 變更歷史的一個值（容量以 GiB 或十進位、物件以摘要） */
function changeValue(path, v, isBefore) {
  if (v == null) return h('span', { class: 'muted' }, isBefore ? t('host.inventory.absent') : t('host.inventory.removed'));
  if (typeof v === 'number' && /memory\.(total|swap_total)$/.test(path)) return fmt.gib(v);
  if (typeof v === 'number' && /\.total$/.test(path)) return fmt.bytes(v);
  if (Array.isArray(v)) return v.length ? v.map((x) => h('div', { class: 'mono' }, String(x))) : '—';
  if (typeof v === 'object') {
    const parts = [v.total != null ? fmt.bytes(v.total) : null, v.fstype, v.mac, ...(v.addresses || [])].filter(Boolean);
    return [isBefore ? '' : `${t('host.inventory.added')} `, parts.length ? `（${parts.join('，')}）` : ''];
  }
  return String(v);
}

function itemLabel(path) {
  const top = path.replace(/[[.].*$/, '');
  return has(`host.inventory.path.${top}`) ? t(`host.inventory.path.${top}`) : top;
}

/**
 * 資產資訊分頁。
 * @returns {() => void} 清除
 */
export function inventoryTab(hst, el) {
  const id = hst.agent_id;
  const ctrl = new AbortController();
  mount(el, skeletonBlock(6));
  const windows = (hst.os_family || '').toLowerCase() === 'windows';
  (async () => {
    let d;
    try {
      d = await api(`/api/v1/hosts/${id}/inventory`, { signal: ctrl.signal });
    } catch (e) {
      if (e.name !== 'AbortError') mount(el, errorState(e, { title: t('host.inventory.failed') }));
      return;
    }
    const inv = d.inventory || {};
    const custom = card({ title: t('host.inventory.custom'), body: h('p', { class: 'muted' }, t('host.inventory.customBody')) });
    if (d.source !== 'inventory') {
      // 舊版 Pulse（或還沒收到）：只有 hello 的作業系統一列
      const os = inv.os;
      mount(
        el,
        h(
          'div',
          { class: 'k-grid k-grid--1-1' },
          card({
            title: t('host.inventory.hw'),
            body: [
              os ? dl([[t('host.inventory.os'), [[os.name, os.version].filter(Boolean).join(' ') || '—', h('span', { class: 'k-sub' }, t('host.inventory.fromHello'))]]]) : null,
              isOldPulse(hst) || !d.supported
                ? emptyState({ title: t('host.na.oldPulseTitle'), body: t('host.na.oldPulseBody', { version: F4_VERSION, current: hst.agent_version || '—' }), iconName: 'info' })
                : emptyState({ title: t('host.na.notYet'), iconName: 'clock' }),
            ],
          }),
          custom,
        ),
      );
      return;
    }
    const rejected = d.last_rejection
      ? inlineAlert({ kind: 'warning', title: t('host.inventory.rejected', { time: `${fmt.dateTime(d.collected_at, { seconds: false })} ${fmt.tzLabel()}` }), code: 'KRS-W2108', body: d.last_rejection.reason })
      : null;
    const changesBox = h('div', null, skeletonBlock(3));
    const filesystems = hst.last_report && hst.last_report.filesystems;
    mount(
      el,
      rejected,
      h(
        'div',
        { class: 'k-grid k-grid--1-1' },
        card({
          title: t('host.inventory.hw'),
          body: [
            dl(hwRows(inv, d, windows)),
            h('p', { class: 'caption' }, t('host.inventory.collected', { time: `${fmt.dateTime(d.collected_at, { seconds: false })}（${fmt.relative(d.collected_at)}）`, changed: fmt.dateTime(d.changed_at, { seconds: false }) })),
          ],
        }),
        custom,
      ),
      card({ title: t('host.inventory.disks'), body: inv.disks && inv.disks.length ? disksTable(inv.disks, filesystems) : notReported() }),
      card({ title: t('host.inventory.interfaces'), body: inv.network_interfaces && inv.network_interfaces.length ? ifacesTable(inv.network_interfaces) : notReported() }),
      card({ title: t('host.inventory.changes'), body: changesBox }),
    );
    // 資產變更歷史
    const rows = h('tbody');
    const more = h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', hidden: true }, t('host.inventory.loadMore'));
    let next = null;
    const firstRow = () => h('tr', null, h('td', null, '—'), h('td', { colspan: 3 }, t('host.inventory.firstSeen')));
    async function page() {
      try {
        const res = await api(`/api/v1/hosts/${id}/inventory/changes${qs({ limit: CHANGES_PAGE, before: next })}`, { signal: ctrl.signal });
        for (const item of res.items) {
          item.changes.forEach((c, i) =>
            rows.appendChild(
              h(
                'tr',
                null,
                h('td', null, i === 0 ? fmt.dateTime(item.changed_at, { seconds: false }) : ''),
                h('td', null, itemLabel(c.path), ' ', h('code', { class: 'k-sub' }, c.path)),
                h('td', null, changeValue(c.path, c.before, true)),
                h('td', null, changeValue(c.path, c.after, false)),
              ),
            ),
          );
        }
        next = res.next;
        more.hidden = !next;
        if (!next) rows.appendChild(firstRow());
      } catch (e) {
        if (e.name !== 'AbortError') mount(changesBox, errorState(e, { title: t('host.inventory.failed') }));
      }
    }
    more.addEventListener('click', page);
    mount(
      changesBox,
      h(
        'div',
        { class: 'k-scroll-x' },
        h(
          'table',
          { class: 'k-mini-table' },
          h('caption', { class: 'ds-visually-hidden' }, t('host.inventory.changes')),
          h(
            'thead',
            null,
            h(
              'tr',
              null,
              h('th', { scope: 'col' }, t('host.inventory.col.time', { tz: fmt.tzLabel() })),
              h('th', { scope: 'col' }, t('host.inventory.col.item')),
              h('th', { scope: 'col' }, t('host.inventory.col.before')),
              h('th', { scope: 'col' }, t('host.inventory.col.after')),
            ),
          ),
          rows,
        ),
      ),
      more,
      h('p', { class: 'caption' }, t('host.inventory.changesNote')),
    );
    await page();
  })();
  return () => ctrl.abort();
}
