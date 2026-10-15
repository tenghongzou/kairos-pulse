// S08 告警規則清單（設計包 kairos/screens.md S08；API GET /api/v1/alert-rules、POST …/enable|disable、DELETE）。
//
// - 啟用開關（Switch）立即生效；停用系統預設規則改開 ConfirmDialog（L1）＋必填原因。
// - 7 天觸發次數與雜訊（fires_7d、noise_7d、noisy）由 API 提供。
// - 0.7.0：未實作的系統規則 API 不回傳，畫面不列出（不顯示停用中的假規則）；「⋮」選單只有「檢視觸發歷史」
//   「複製」「刪除」（稽核紀錄的連結等 S18 完成）。

import { h, icon, mount, announce } from '../core/dom.js';
import { t, severityLabel } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { replaceQuery, navigate } from '../core/router.js';
import { pageHead, emptyState, errorState, inlineAlert, field, input, button, neutralBadge } from '../ui/components.js';
import { openDialog, confirm } from '../ui/dialog.js';
import { openMenu } from '../ui/menu.js';
import { toast } from '../ui/toast.js';
import { short } from '../core/errors.js';
import { SEVERITIES, sevBadge, kindLabel, unavailableNotice } from './alerts-common.js';

function parseState(q) {
  return {
    severity: SEVERITIES.includes(q.get('severity')) ? q.get('severity') : '',
    enabled: ['true', 'false'].includes(q.get('enabled')) ? q.get('enabled') : '',
    type: ['builtin', 'custom'].includes(q.get('type')) ? q.get('type') : '',
    q: q.get('q') || '',
  };
}

/** 嚴重度欄：依天數升級的規則顯示「警告 → 緊急」 */
function sevCell(r) {
  if (r.max_severity && r.max_severity !== r.severity) {
    return h('span', { class: 'k-row k-row--tight' }, sevBadge(r.severity), h('span', { 'aria-hidden': 'true' }, '→'), h('span', { class: 'ds-visually-hidden' }, t('alerts.rules.escalatesTo')), sevBadge(r.max_severity));
  }
  return sevBadge(r.severity);
}

function noiseCell(r) {
  if (r.noise_7d == null) return h('span', { class: 'muted' }, '—');
  if (r.noisy) return h('span', { class: 'k-flag' }, icon('status-warning'), t('alerts.rules.noiseHigh', { p: fmt.pct(r.noise_7d * 100) }));
  return t('alerts.rules.noiseLow', { p: fmt.pct(r.noise_7d * 100) });
}

export function render(ctx) {
  const root = ctx.root;
  const st = parseState(ctx.query);
  const canWrite = session.can('alert-rules:write');
  let rules = [];
  let channels = null;
  let disposed = false;

  const body = h('div');
  const selRow = h('div', { class: 'k-filter-selects' });
  const search = h('input', { class: 'ds-input', type: 'search', name: 'q', value: st.q, autocomplete: 'off', spellcheck: 'false', placeholder: t('alerts.rules.searchPlaceholder') });
  const searchField = field({ label: t('alerts.rules.search'), input: search });
  searchField.classList.add('ds-filterbar__search');
  search.addEventListener('input', () => {
    st.q = search.value.trim();
    draw();
  });

  function select(name, label, options, value, onChange) {
    const sel = h('select', { class: 'ds-select', name, on: { change: () => onChange(sel.value) } }, options.map(([v, text]) => h('option', { value: v, selected: v === value }, text)));
    return h('label', { class: 'k-labelled-select' }, label, sel);
  }
  function renderSelects() {
    mount(
      selRow,
      select('type', t('alerts.rules.type'), [['', t('hosts.filter.all')], ['builtin', t('alerts.rules.builtin')], ['custom', t('alerts.rules.custom')]], st.type, (v) => { st.type = v; draw(); }),
      select('severity', t('alerts.col.severity'), [['', t('hosts.filter.all')], ...SEVERITIES.map((s) => [s, severityLabel(s)])], st.severity, (v) => { st.severity = v; draw(); }),
      select('enabled', t('alerts.rules.enabled'), [['', t('hosts.filter.all')], ['true', t('alerts.rules.on')], ['false', t('alerts.rules.off')]], st.enabled, (v) => { st.enabled = v; draw(); }),
    );
  }

  mount(
    root,
    pageHead({
      title: t('alerts.rules.title'),
      crumbs: [{ text: t('nav.section.alerts'), href: '/alerts/events' }, { text: t('alerts.rules.title') }],
      actions: canWrite
        ? [
            h(
              'button',
              {
                type: 'button',
                class: 'ds-button ds-button--secondary',
                'aria-haspopup': 'menu',
                on: {
                  click: (ev) =>
                    openMenu(
                      ev.currentTarget,
                      ['cpu90', 'disk90', 'disk95'].map((k) => ({ text: t(`alerts.template.${k}`), onSelect: () => navigate(`/alerts/rules/new${qs({ template: k })}`) })),
                      { label: t('alerts.rules.templates') },
                    ),
                },
              },
              t('alerts.rules.templates'),
              icon('chevron-down'),
            ),
            h('a', { class: 'ds-button ds-button--primary', href: '/alerts/rules/new' }, icon('plus'), t('alerts.rules.new')),
          ]
        : null,
    }),
    canWrite ? null : inlineAlert({ kind: 'info', title: t('alerts.readOnly'), body: t('alerts.rules.readOnlyBody') }),
    h('section', { class: 'ds-filterbar', 'aria-label': t('alerts.rules.filters') }, h('div', { class: 'ds-filterbar__row' }, searchField), selRow),
    body,
  );
  renderSelects();
  mount(body, h('div', { class: 'ds-skeleton', 'aria-busy': 'true', style: { height: '240px' } }, h('span', { class: 'ds-visually-hidden' }, t('common.loading'))));

  function channelNames(ids) {
    if (!ids || !ids.length) return h('span', { class: 'muted' }, '—');
    if (!channels) return t('alerts.rules.channelsN', { n: ids.length });
    return ids.map((id) => (channels.find((c) => c.channel_id === id) || {}).name || id).join('、');
  }

  function filtered() {
    const q = st.q.toLowerCase();
    return rules.filter((r) => {
      if (st.type === 'builtin' && !r.builtin) return false;
      if (st.type === 'custom' && r.builtin) return false;
      if (st.severity && r.severity !== st.severity && r.max_severity !== st.severity) return false;
      if (st.enabled && String(r.enabled) !== st.enabled) return false;
      if (q && !r.name.toLowerCase().includes(q) && !r.rule_id.toLowerCase().includes(q)) return false;
      return true;
    });
  }

  function switchFor(r) {
    const label = t('alerts.rules.switchAria', { name: r.name });
    if (!canWrite) return h('span', null, r.enabled ? t('alerts.rules.on') : t('alerts.rules.off'));
    const b = h(
      'button',
      { type: 'button', role: 'switch', class: 'k-switch', 'aria-checked': r.enabled ? 'true' : 'false', 'aria-label': label, title: label, dataset: { rule: r.rule_id } },
      h('span', { class: 'k-switch__track', 'aria-hidden': 'true' }, h('span', { class: 'k-switch__thumb' })),
      h('span', { class: 'k-switch__text' }, r.enabled ? t('alerts.rules.on') : t('alerts.rules.off')),
    );
    b.addEventListener('click', () => toggle(r, b));
    return b;
  }

  async function toggle(r, btn) {
    const on = !r.enabled;
    let reason;
    if (!on && r.builtin) {
      reason = await disableReason(r);
      if (reason == null) return;
    }
    btn.disabled = true;
    try {
      const updated = await api(`/api/v1/alert-rules/${encodeURIComponent(r.rule_id)}/${on ? 'enable' : 'disable'}`, { method: 'POST', body: reason ? { reason } : {} });
      Object.assign(r, { enabled: updated.enabled, disabled_reason: updated.disabled_reason });
      toast({ kind: 'success', title: t(on ? 'alerts.rules.enabledToast' : 'alerts.rules.disabledToast', { name: r.name }), desc: on ? null : t('alerts.rules.disabledDesc') });
      draw();
      const nb = body.querySelector(`button[data-rule="${CSS.escape(r.rule_id)}"]`);
      if (nb) nb.focus();
    } catch (err) {
      btn.disabled = false;
      toast({ kind: 'error', title: short(err, { title: t('alerts.rules.toggleFailed') }) });
    }
  }

  /** 停用系統預設規則（L1，原因必填） */
  function disableReason(r) {
    return new Promise((resolve) => {
      const inp = input({ name: 'reason', maxlength: 500 });
      const f = field({ label: t('alerts.rules.reason'), input: inp, required: true, hint: t('alerts.rules.reasonHint') });
      const ok = button({ text: t('alerts.rules.disable'), variant: 'danger' });
      const cancel = button({ text: t('common.cancel'), onClick: () => d.close() });
      let done = false;
      const d = openDialog({
        title: t('alerts.rules.disableTitle', { name: r.name }),
        size: 'sm',
        body: h('div', null, h('p', null, t('alerts.rules.disableBody')), f),
        actions: [cancel, ok],
        initialFocus: inp,
        onClose: () => {
          if (!done) resolve(null);
        },
      });
      ok.addEventListener('click', () => {
        const v = inp.value.trim();
        if (!v) {
          f.setError(t('alerts.rules.reasonRequired'));
          inp.focus();
          return;
        }
        done = true;
        d.close();
        resolve(v);
      });
    });
  }

  async function remove(r) {
    const ok = await confirm({ title: t('alerts.rules.deleteTitle', { name: r.name }), body: t('alerts.rules.deleteBody'), confirmText: t('alerts.rules.delete'), danger: true });
    if (!ok) return;
    try {
      await api(`/api/v1/alert-rules/${encodeURIComponent(r.rule_id)}`, { method: 'DELETE' });
      rules = rules.filter((x) => x.rule_id !== r.rule_id);
      toast({ kind: 'success', title: t('alerts.rules.deleted', { name: r.name }) });
      draw();
      const tl = document.getElementById('page-title');
      if (tl) tl.focus();
    } catch (err) {
      toast({ kind: 'error', title: short(err, { title: t('alerts.rules.deleteFailed') }) });
    }
  }

  function rowMenu(r) {
    const b = h('button', { type: 'button', class: 'ds-button ds-button--icon ds-button--sm', 'aria-label': t('alerts.rules.more', { name: r.name }), title: t('alerts.rules.more', { name: r.name }), 'aria-haspopup': 'menu', 'aria-expanded': 'false' }, icon('more'));
    b.addEventListener('click', () =>
      openMenu(
        b,
        [
          session.can('alerts:read') ? { text: t('alerts.rules.history'), iconName: 'bell', href: `/alerts/events${qs({ view: 'all', rule: r.rule_id })}` } : null,
          canWrite && !r.builtin ? { text: t('alerts.rules.copy'), iconName: 'copy', onSelect: () => navigate(`/alerts/rules/new${qs({ copy: r.rule_id })}`) } : null,
          canWrite && !r.builtin ? 'separator' : null,
          canWrite && !r.builtin ? { text: t('alerts.rules.delete'), iconName: 'trash', danger: true, onSelect: () => remove(r) } : null,
        ].filter(Boolean),
        { label: t('alerts.rules.more', { name: r.name }) },
      ),
    );
    return b;
  }

  function draw() {
    replaceQuery({ type: st.type, severity: st.severity, enabled: st.enabled, q: st.q });
    renderSelects();
    const list = filtered();
    const custom = rules.filter((r) => !r.builtin);
    const hint = !custom.length && canWrite
      ? inlineAlert({ kind: 'info', body: h('p', null, t('alerts.rules.noCustom'), ' ', h('a', { href: '/alerts/rules/new' }, t('alerts.rules.new'))) })
      : null;
    if (!list.length) {
      mount(body, hint, emptyState({ title: t('alerts.rules.noMatch'), iconName: 'filter', actions: [button({ text: t('hosts.clear'), onClick: () => { st.type = ''; st.severity = ''; st.enabled = ''; st.q = ''; search.value = ''; draw(); } })] }));
      return;
    }
    mount(
      body,
      hint,
      h(
        'div',
        { class: 'ds-card ds-card--flush' },
        h(
          'div',
          { class: 'ds-table-wrap' },
          h(
            'table',
            { class: 'ds-table' },
            h('caption', { class: 'ds-visually-hidden' }, t('alerts.rules.caption')),
            h(
              'thead',
              null,
              h(
                'tr',
                null,
                [t('alerts.rules.enabled'), t('alerts.rules.name'), t('alerts.rules.kind'), t('alerts.col.severity'), t('alerts.rules.scope'), t('alerts.rules.channels'), t('alerts.rules.fires7d'), t('alerts.rules.noise')].map((c, i) =>
                  h('th', { scope: 'col', class: i === 6 ? 'num' : null }, c),
                ),
                h('th', { scope: 'col' }, h('span', { class: 'ds-visually-hidden' }, t('alerts.col.actions'))),
              ),
            ),
            h(
              'tbody',
              null,
              list.map((r) =>
                h(
                  'tr',
                  { dataset: { key: r.rule_id }, class: r.enabled ? null : 'is-disabled' },
                  h('td', null, switchFor(r)),
                  h(
                    'td',
                    { class: 'k-cell-wrap' },
                    h('a', { href: `/alerts/rules/${encodeURIComponent(r.rule_id)}` }, r.name),
                    r.builtin ? [' ', neutralBadge(t('alerts.rules.builtinTag'))] : null,
                    !r.enabled && r.disabled_reason ? h('span', { class: 'k-sub' }, t('alerts.rules.disabledBecause', { r: r.disabled_reason })) : null,
                  ),
                  h('td', null, kindLabel(r.kind)),
                  h('td', null, sevCell(r)),
                  h('td', { class: 'k-cell-wrap' }, r.category === 'system' ? h('span', { class: 'muted' }, '—') : r.scope_text || t('alerts.scope.all')),
                  h('td', { class: 'k-cell-wrap' }, channelNames(r.channels)),
                  h('td', { class: 'num' }, fmt.num(r.fires_7d)),
                  h('td', null, noiseCell(r)),
                  h('td', null, rowMenu(r)),
                ),
              ),
            ),
          ),
        ),
      ),
      h('p', { class: 'caption' }, t('alerts.rules.count', { n: fmt.num(list.length), total: fmt.num(rules.length) })),
    );
  }

  async function load() {
    try {
      const [r, ch] = await Promise.all([
        api('/api/v1/alert-rules'),
        session.can('channels:read') ? api('/api/v1/channels').catch(() => null) : Promise.resolve(null),
      ]);
      if (disposed) return;
      rules = r.slice().sort((a, b) => Number(b.builtin) - Number(a.builtin) || a.name.localeCompare(b.name, 'zh-Hant'));
      channels = ch;
      draw();
      announce(t('alerts.rules.loaded', { n: fmt.num(rules.length) }));
    } catch (err) {
      if (disposed) return;
      mount(body, err.code === 'alerts_unavailable' ? unavailableNotice() : null, errorState(err, { title: t('alerts.rules.failed'), onRetry: load }));
    }
  }
  load();
  return {
    dispose() {
      disposed = true;
    },
  };
}

