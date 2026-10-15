// S21 系統設定（/system/settings/:section；設計包 screens.md S21「設定與 kairos.toml 的關係」、D32；
// API.md 第 11.7 節）。
//
// - 每個設定鍵的生效值與來源（GET /api/v1/settings，與 kairos config show --effective 相同）：環境變數 ＞ 主控台
//   （資料庫）＞ kairos.toml ＞ 內建預設。來源文字與 aria-describedby 關聯，不只以顏色或圖示表達。
//   主控台的值：修改者與時間、被覆寫的設定檔值；環境變數：「無法在此修改」。
// - 可從主控台修改的鍵：註冊端點模式與自動暫停（PUT /api/v1/enroll/mode；L1，關閉且有受影響主機時 L2 輸入
//   「停止續簽 N 台」、原因必填）。憑證政策（pki.policy.*）在憑證管理（S14）修改，這裡顯示來源並可「改用設定檔的值」。
//   「改用設定檔的值」＝刪除資料庫的值（POST /api/v1/settings/reset，L1）。被環境變數鎖定的鍵唯讀。
// - 影響預覽（0.8）：「改用設定檔的值」先以 GET /api/v1/settings/reset/impact 取得影響——改回去會關閉註冊端點或縮短
//   過期寬限期時，對話框列出會停止續簽的主機並帶 impact_token；有主機受影響時為 L2（輸入「停止續簽 N 台」、原因必填，
//   screens.md §0.3）。送出時數字已改變（409 impact_changed）：關閉對話框，以新的數字重新確認（不沿用舊的確認字串）。
//   套用註冊端點模式同樣處理。
// - 分頁：一般、連線與安全、資料保留與儲存、整合（API token 在「整合」）；「主機群組與標籤」沒有後端，隱藏。
//   維運者只看得到「連線與安全」（唯讀，D16）；稽核員全部唯讀。
// - 隱藏（§28.6）：「整合」的 [log]／[observability] 沒有修改 API，唯讀並標示「在 kairos.toml 修改、需重新啟動」；
//   需要重新啟動的設定（監聽位址、連線上限…）本版只能在 kairos.toml 修改（沒有寫入 API），一律唯讀。

import { h, uid, mount } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { pageHead, card, errorState, errorAlert, field, inlineAlert, skeletonBlock, neutralBadge, codeTag } from '../ui/components.js';
import { toast } from '../ui/toast.js';
import { linkTabs, confirmL1, confirmL2, valueText } from './admin-common.js';
import { tokensCard } from './admin-tokens.js';

const TABS = ['general', 'security', 'storage', 'integrations'];
const TAB_SECTIONS = {
  general: ['accounts', 'alerts'],
  security: ['server', 'console', 'auth', 'limits', 'timeouts', 'enroll', 'pki'],
  storage: ['storage', 'audit'],
  integrations: ['log', 'observability'],
};
const MODE_KEYS = ['enroll.mode', 'enroll.auto_pause_enabled', 'enroll.auto_pause_after_hours'];

function allTabs() {
  // 維運者（沒有 audit:read）只有「連線與安全」（ia.md §4.2、D16）
  return session.can('audit:read') ? TABS : ['security'];
}

function writePerm(key) {
  if (key.startsWith('enroll.')) return 'enroll.mode:write';
  if (key.startsWith('pki.')) return 'pki.policy:write';
  return null;
}

function sourceLabel(src) {
  return t(`adm.set.src.${src}`);
}

function fileValueOf(s) {
  const o = (s.overridden || []).find((x) => x.source === 'file');
  return o ? o.value : undefined;
}

/** 來源的說明文字（screens.md S21「來源標示」） */
function sourceText(s) {
  const parts = [];
  if (s.source === 'env') parts.push(t('adm.set.srcText.env', { v: s.env_var }));
  else if (s.source === 'console') parts.push(t('adm.set.srcText.console', { by: s.updated_by || '—', at: s.updated_at ? `${fmt.dateTime(s.updated_at, { seconds: false })} ${fmt.tzLabel()}` : '—' }));
  else if (s.source === 'default') parts.push(t('adm.set.srcText.default'));
  else parts.push(t('adm.set.srcText.file'));
  for (const o of s.overridden || []) {
    parts.push(t(o.source === 'file' ? 'adm.set.overriddenFile' : 'adm.set.overriddenConsole', { v: valueText(o.value) }));
  }
  return parts.join(' ');
}

/** 影響預覽（GET …/impact 的 data）：關閉註冊端點的 N／M／L 與縮短寬限期的 K、最早受影響的主機 */
function impactBlock(imp) {
  const items = [];
  if (imp.mode) {
    items.push(h('li', null, t('adm.mode.impactN', { n: fmt.num(imp.mode.N) })), h('li', null, t('adm.mode.impactM', { n: fmt.num(imp.mode.M) })), h('li', null, t('adm.mode.impactL', { n: fmt.num(imp.mode.L) })));
  }
  if (imp.grace) {
    const g = imp.grace;
    items.push(h('li', null, t('adm.impact.grace', { from: g.from_days, to: g.to_days, k: fmt.num(g.K) })));
  }
  const earliest = (imp.grace && imp.grace.earliest) || [];
  return h(
    'div',
    { class: 'k-impact-box' },
    h('p', { class: 'strong' }, t('adm.impact.title')),
    h('ul', { class: 'k-impact' }, items),
    earliest.length
      ? [
          h('p', null, t('adm.impact.earliest')),
          h(
            'ul',
            null,
            earliest.map((e) => h('li', null, h('span', null, e.hostname), ' ', h('span', { class: 'caption' }, t('adm.impact.graceEnds', { t: `${fmt.dateTime(e.grace_ends_at, { seconds: false })} ${fmt.tzLabel()}` })))),
            imp.grace.K > earliest.length ? h('li', { class: 'caption' }, t('adm.impact.more', { n: fmt.num(imp.grace.K - earliest.length) })) : null,
          ),
        ]
      : null,
  );
}

function keyLabel(key) {
  return has(`adm.set.key.${key}`) ? t(`adm.set.key.${key}`) : null;
}

export function render(ctx) {
  const root = ctx.root;
  const tabs = allTabs();
  const section = ctx.params.section || tabs[0];
  if (!TABS.includes(section)) {
    ctx.app.notFound();
    return {};
  }
  if (!tabs.includes(section)) {
    mount(
      root,
      pageHead({ title: t('adm.set.title') }),
      linkTabs(t('adm.set.tabs'), tabs.map((k) => [k, `/system/settings/${k}`, t(`adm.set.tab.${k}`)]), section),
      inlineAlert({ kind: 'warning', title: t('adm.set.needAdmin'), body: t('adm.set.needAdminBody'), code: 'KRS-E6104' }),
    );
    return {};
  }
  const readOnlyAll = !session.can('enroll.mode:write') && !session.can('pki.policy:write');
  // 註冊端點模式卡片的「尚未套用」變更（screens.md S21：離開頁面時的「未儲存變更」提示；main.js 依 isDirty() 詢問）
  let modeDirty = null;
  const body = h('div', null, skeletonBlock(10));
  mount(
    root,
    pageHead({ title: t('adm.set.title') }),
    linkTabs(t('adm.set.tabs'), tabs.map((k) => [k, `/system/settings/${k}`, t(`adm.set.tab.${k}`)]), section),
    h('p', { class: 'caption' }, t('adm.set.headCaption'), ' ', codeTag('kairos config show --effective')),
    readOnlyAll ? inlineAlert({ kind: 'info', title: t('adm.set.readOnlyTitle'), body: t('adm.set.readOnlyBody') }) : null,
    body,
  );

  async function load() {
    let list;
    try {
      const d = await api('/api/v1/settings');
      list = d.settings || [];
    } catch (err) {
      mount(body, errorState(err, { title: t('adm.set.loadFailed'), onRetry: load }));
      return;
    }
    const byKey = new Map(list.map((s) => [s.key, s]));
    const blocks = [];
    if (section === 'security' && session.can('enroll:read')) blocks.push(modeCard(byKey));
    for (const sec of TAB_SECTIONS[section]) {
      const rows = list.filter((s) => s.key.startsWith(`${sec}.`) && !(sec === 'enroll' && MODE_KEYS.includes(s.key) && session.can('enroll:read')));
      if (!rows.length) continue;
      blocks.push(sectionCard(sec, rows));
    }
    if (section === 'general') blocks.push(card({ title: t('adm.set.section.ldap'), body: h('p', null, t('adm.set.ldapNote'), ' ', session.can('directory:read') ? h('a', { href: '/admin/directory' }, t('adm.set.toDirectory')) : null) }));
    if (section === 'integrations' && session.can('api_tokens:read')) blocks.push(tokensCard());
    mount(body, h('div', { class: 'k-stack' }, blocks));
  }

  /** 改用設定檔的值：先查影響，再依等級確認（數字改變時以新的數字重新確認） */
  async function resetFlow(s, imp, notice) {
    const fileVal = fileValueOf(s);
    const label = fileVal !== undefined ? t('adm.set.useFile') : t('adm.set.useDefault');
    let changed = null;
    const send = async ({ reason, errBox }) => {
      try {
        await api('/api/v1/settings/reset', { method: 'POST', body: { keys: [s.key], reason: reason || undefined, impact_token: imp.required ? imp.impact_token : undefined } });
        toast({ kind: 'success', title: fileVal !== undefined ? t('adm.set.usedFile') : t('adm.set.usedDefault'), desc: s.key });
        load();
        return true;
      } catch (err) {
        if (err.code === 'impact_changed' && err.details) {
          changed = err.details;
          return true;
        }
        const a = errorAlert(err, { title: t('adm.set.resetFailed') });
        mount(errBox, a);
        a.focus();
        return false;
      }
    };
    const summary = h(
      'div',
      null,
      notice,
      h('p', null, codeTag(s.key)),
      h('ul', null, h('li', null, t('adm.set.nowConsole', { v: valueText(s.value) })), h('li', null, fileVal !== undefined ? t('adm.set.thenFile', { v: valueText(fileVal) }) : t('adm.set.thenDefault'))),
      imp.required ? impactBlock(imp) : null,
      h('p', null, t('adm.set.resetEffect')),
    );
    if (imp.level === 'L2') {
      await confirmL2({
        title: fileVal !== undefined ? t('adm.set.useFileTitle') : t('adm.set.useDefaultTitle'),
        lead: t('adm.impact.lead', { n: fmt.num(imp.count) }),
        body: summary,
        phrase: imp.confirm_text,
        confirmText: label,
        reason: 'required',
        onConfirm: send,
      });
    } else {
      await confirmL1({
        title: fileVal !== undefined ? t('adm.set.useFileTitle') : t('adm.set.useDefaultTitle'),
        body: summary,
        confirmText: label,
        reason: imp.required ? 'optional' : null,
        onConfirm: send,
      });
    }
    if (changed) await resetFlow(s, changed, inlineAlert({ kind: 'warning', title: t('adm.impact.changed'), body: t('adm.impact.changedBody') }));
  }

  function resetButton(s) {
    const perm = writePerm(s.key);
    if (s.source !== 'console' || !perm || !session.can(perm)) return null;
    const fileVal = fileValueOf(s);
    const label = fileVal !== undefined ? t('adm.set.useFile') : t('adm.set.useDefault');
    const btn = h(
      'button',
      {
        type: 'button',
        class: 'ds-button ds-button--secondary ds-button--sm',
        'aria-label': `${label}：${s.key}`,
        on: {
          click: async () => {
            // 影響預覽：改回去會不會關閉註冊端點或縮短寬限期（GET /api/v1/settings/reset/impact）
            let imp;
            btn.setAttribute('aria-busy', 'true');
            try {
              imp = await api(`/api/v1/settings/reset/impact${qs({ keys: s.key })}`);
            } catch (err) {
              toast({ kind: 'error', title: t('adm.impact.failed'), desc: err.message });
              return;
            } finally {
              btn.removeAttribute('aria-busy');
            }
            await resetFlow(s, imp, null);
          },
        },
      },
      label,
    );
    return btn;
  }

  function settingRow(s) {
    const srcId = uid('src');
    const label = keyLabel(s.key);
    return h(
      'tr',
      { dataset: { key: s.key, source: s.source } },
      h('th', { scope: 'row' }, label ? [h('span', null, label), h('span', { class: 'k-host-id mono' }, s.key)] : h('span', { class: 'mono' }, s.key)),
      h('td', { 'aria-describedby': srcId }, h('span', { class: 'mono k-break' }, valueText(s.value))),
      h('td', null, neutralBadge(sourceLabel(s.source), s.source === 'env' ? 'lock' : null), h('p', { class: 'caption', id: srcId }, sourceText(s))),
      h('td', null, resetButton(s)),
    );
  }

  function sectionCard(sec, rows) {
    const restart = ['server', 'console', 'auth', 'limits', 'timeouts', 'storage', 'audit', 'log', 'observability', 'accounts'].includes(sec);
    return card({
      title: has(`adm.set.section.${sec}`) ? t(`adm.set.section.${sec}`) : sec,
      body: [
        h('p', { class: 'caption' }, restart ? t('adm.set.fileOnly') : sec === 'pki' ? t('adm.set.pkiNote') : t('adm.set.consoleKeys')),
        // 「整合」：/metrics 經反向代理時必須設定 token（screens.md S21「整合」、D42）
        sec === 'observability' ? h('p', { class: 'caption' }, t('adm.health.metrics.proxy'), ' ', codeTag('KRS-W0304')) : null,
        h(
          'div',
          { class: 'k-scroll-x' },
          h(
            'table',
            { class: 'k-mini-table k-settings-table' },
            h('caption', { class: 'ds-visually-hidden' }, has(`adm.set.section.${sec}`) ? t(`adm.set.section.${sec}`) : sec),
            h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('adm.set.col.key')), h('th', { scope: 'col' }, t('adm.set.col.value')), h('th', { scope: 'col' }, t('adm.set.col.source')), h('th', { scope: 'col' }, h('span', { class: 'ds-visually-hidden' }, t('account.actions'))))),
            h('tbody', null, rows.map(settingRow)),
          ),
        ),
      ],
    });
  }

  // ---- 註冊端點模式（立即生效） ----
  function modeCard(byKey) {
    const el = h('div', null, skeletonBlock(5));
    const c = card({ title: t('adm.mode.title'), body: el });
    (async () => {
      let m;
      let impact = null;
      try {
        m = await api('/api/v1/enroll/mode');
        impact = await api(`/api/v1/enroll/mode/impact${qs({ mode: 'closed' })}`).catch(() => null);
      } catch (err) {
        mount(el, errorState(err, { title: t('adm.mode.loadFailed') }));
        return;
      }
      drawMode(el, m, impact, byKey);
    })();
    return c;
  }

  function drawMode(el, m, impact, byKey) {
    const sMode = byKey.get('enroll.mode') || { source: 'default', key: 'enroll.mode' };
    const sAuto = byKey.get('enroll.auto_pause_enabled') || { source: 'default', key: 'enroll.auto_pause_enabled' };
    const sHours = byKey.get('enroll.auto_pause_after_hours') || { source: 'default', key: 'enroll.auto_pause_after_hours' };
    const canWrite = session.can('enroll.mode:write');
    const grace = byKey.get('pki.policy.expired_renew_grace_days');
    const graceZero = grace && Number(grace.value) === 0;
    const modeLocked = sMode.source === 'env';
    const autoLocked = sAuto.source === 'env';
    const hoursLocked = sHours.source === 'env';
    const gname = uid('mode');
    const srcIds = { mode: uid('src'), auto: uid('src'), hours: uid('src') };
    const radios = {};
    const opt = (v) => {
      const disabled = !canWrite || modeLocked;
      const r = h('input', { type: 'radio', name: gname, value: v, checked: m.mode === v, disabled, 'aria-describedby': `${srcIds.mode}${v === 'renew_only' && graceZero ? ` ${gname}-grace` : ''}` });
      if (v === 'renew_only' && graceZero) {
        r.setAttribute('aria-disabled', 'true');
        r.addEventListener('click', (ev) => {
          ev.preventDefault();
        });
      }
      radios[v] = r;
      return h('label', { class: ['ds-radio-card', v === 'renew_only' && graceZero ? 'is-disabled' : null] }, r, h('span', null, h('span', { class: 'strong' }, t(`adm.mode.opt.${v}`)), h('span', { class: 'k-block caption' }, t(`adm.mode.opt.${v}.desc`))));
    };
    const autoBox = h('input', { type: 'checkbox', class: 'ds-check', name: 'auto_pause_enabled', checked: Boolean(m.auto_pause_enabled), disabled: !canWrite || autoLocked, 'aria-describedby': srcIds.auto });
    const hours = h('input', { class: 'ds-input ds-input--num', type: 'text', inputmode: 'numeric', name: 'auto_pause_after_hours', value: String(m.auto_pause_after_hours), maxlength: '3', disabled: !canWrite || hoursLocked, 'aria-describedby': srcIds.hours });
    const fh = field({ label: t('adm.mode.hours'), input: hours, hint: t('adm.mode.hoursHint') });
    // field() 以提示文字取代 aria-describedby：來源說明另外加上（螢幕報讀器讀到欄位時一併讀出來源）
    hours.setAttribute('aria-describedby', `${hours.getAttribute('aria-describedby') || ''} ${srcIds.hours}`.trim());
    const pending = h('span', { class: 'caption', 'aria-live': 'polite' });
    const applyBtn = canWrite ? h('button', { type: 'button', class: 'ds-button ds-button--primary', 'aria-disabled': 'true', title: t('adm.mode.same') }, t('adm.mode.apply')) : null;
    const optEls = [opt('open'), opt('renew_only'), opt('closed')];
    const current = () => Object.keys(radios).find((k) => radios[k].checked) || m.mode;
    const changes = () => {
      const out = {};
      if (current() !== m.mode) out.mode = current();
      if (!autoLocked && autoBox.checked !== Boolean(m.auto_pause_enabled)) out.auto_pause_enabled = autoBox.checked;
      if (!hoursLocked && hours.value.trim() !== String(m.auto_pause_after_hours)) out.auto_pause_after_hours = hours.value.trim();
      return out;
    };
    const refresh = () => {
      if (!applyBtn) return;
      const ch = Object.keys(changes()).length > 0;
      if (ch) {
        applyBtn.removeAttribute('aria-disabled');
        applyBtn.removeAttribute('title');
      } else {
        applyBtn.setAttribute('aria-disabled', 'true');
        applyBtn.title = t('adm.mode.same');
      }
      pending.textContent = ch ? t('adm.mode.notApplied') : '';
      const isOpen = current() === 'open';
      autoBox.disabled = !canWrite || autoLocked || !isOpen;
      hours.disabled = !canWrite || hoursLocked || !isOpen || !autoBox.checked;
    };
    modeDirty = canWrite ? () => Object.keys(changes()).length > 0 : null;
    for (const r of Object.values(radios)) r.addEventListener('change', refresh);
    autoBox.addEventListener('change', refresh);
    hours.addEventListener('input', () => {
      if (hours.getAttribute('aria-invalid')) fh.setError(validHours(hours.value) ? '' : t('adm.mode.hoursInvalid'));
      refresh();
    });
    const N = impact ? impact.N : null;
    const M = impact ? impact.M : null;
    const impactInfo = impact
      ? N + M + impact.L === 0
        ? h('p', null, t('adm.mode.noImpact'))
        : h(
            'ul',
            { class: 'k-impact' },
            h('li', null, t('adm.mode.impactN', { n: fmt.num(N) })),
            h('li', null, t('adm.mode.impactM', { n: fmt.num(M) })),
            h('li', null, t('adm.mode.impactL', { n: fmt.num(impact.L) })),
          )
      : null;
    if (applyBtn) applyBtn.addEventListener('click', () => apply());

    function validHours(v) {
      return /^\d+$/.test(v.trim()) && Number(v) >= 1 && Number(v) <= 168;
    }

    async function apply() {
      const ch = changes();
      if (!Object.keys(ch).length) return;
      if ('auto_pause_after_hours' in ch) {
        if (!validHours(ch.auto_pause_after_hours)) {
          fh.setError(t('adm.mode.hoursInvalid'));
          hours.focus();
          return;
        }
        fh.setError('');
        ch.auto_pause_after_hours = Number(ch.auto_pause_after_hours);
      }
      let imp = null;
      if (ch.mode === 'closed') {
        try {
          imp = await api(`/api/v1/enroll/mode/impact${qs({ mode: 'closed' })}`);
        } catch (err) {
          toast({ kind: 'error', title: t('adm.mode.impactFailed') });
          return;
        }
      }
      await confirmMode(ch, imp, null);
    }

    /** 套用模式的確認；送出時影響數字已改變（409 impact_changed）就關閉對話框、以新的數字重新確認 */
    async function confirmMode(ch, imp, notice) {
      const target = ch.mode || m.mode;
      if (imp) ch.impact_token = imp.impact_token;
      let changed = null;
      const send = async ({ reason, errBox }) => {
        try {
          await api('/api/v1/enroll/mode', { method: 'PUT', body: { ...ch, reason: reason || undefined } });
          toast({ kind: 'success', title: t('adm.mode.applied', { m: t(`adm.mode.opt.${target}`) }), desc: t('adm.mode.appliedDesc') });
          load();
          return true;
        } catch (err) {
          if (err.code === 'impact_changed' && err.details) {
            changed = err.details;
            return true;
          }
          const a = errorAlert(err, { title: t('adm.mode.applyFailed') });
          mount(errBox, a);
          a.focus();
          return false;
        }
      };
      const again = () => (changed ? confirmMode(ch, changed, inlineAlert({ kind: 'warning', title: t('adm.impact.changed'), body: t('adm.impact.changedBody') })) : null);
      const lines = [];
      if (ch.mode) lines.push(t('adm.mode.change.mode', { from: t(`adm.mode.opt.${m.mode}`), to: t(`adm.mode.opt.${ch.mode}`) }));
      if ('auto_pause_enabled' in ch) lines.push(ch.auto_pause_enabled ? t('adm.mode.change.autoOn') : t('adm.mode.change.autoOff'));
      if ('auto_pause_after_hours' in ch) lines.push(t('adm.mode.change.hours', { from: m.auto_pause_after_hours, to: ch.auto_pause_after_hours }));
      const consequence = ch.mode ? t(`adm.mode.effect.${ch.mode}`, { t: fmt.num(imp ? imp.T : impact ? impact.T : 0), r: fmt.num(imp ? imp.R : impact ? impact.R : 0) }) : null;
      if (imp && imp.level === 'L2') {
        await confirmL2({
          title: t('adm.mode.closeTitle'),
          lead: t('adm.mode.closeLead', { n: fmt.num(imp.N + imp.M) }),
          body: h(
            'div',
            null,
            notice,
            h('ul', { class: 'k-impact' }, h('li', null, t('adm.mode.impactN', { n: fmt.num(imp.N) })), h('li', null, t('adm.mode.impactM', { n: fmt.num(imp.M) })), h('li', null, t('adm.mode.impactL', { n: fmt.num(imp.L) })), h('li', null, t('adm.mode.effect.closed', { t: fmt.num(imp.T), r: fmt.num(imp.R) }))),
            inlineAlert({ kind: 'info', title: t('adm.mode.considerRenewOnly') }),
          ),
          phrase: imp.confirm_text,
          confirmText: t('adm.mode.closeConfirm'),
          reason: 'required',
          onConfirm: send,
        });
        await again();
        return;
      }
      await confirmL1({
        title: ch.mode ? t(`adm.mode.confirmTitle.${ch.mode}`) : t('adm.mode.confirmTitle.auto'),
        body: h('div', null, notice, h('ul', null, lines.map((l) => h('li', null, l))), consequence ? h('p', null, consequence) : null, h('p', { class: 'caption' }, t('adm.mode.audited'))),
        confirmText: ch.mode ? t(`adm.mode.confirm.${ch.mode}`) : t('adm.mode.apply').replace('…', ''),
        onConfirm: send,
      });
      await again();
    }

    const srcLine = (s, id) => h('p', { class: 'caption', id }, neutralBadge(sourceLabel(s.source), s.source === 'env' ? 'lock' : null), ' ', sourceText(s), ' ', resetButton(s));
    mount(
      el,
      h(
        'p',
        null,
        t('adm.mode.current'),
        ' ',
        h('span', { class: 'strong' }, t(`adm.mode.${m.state}`)),
        m.paused_since ? ` · ${t('adm.mode.pausedSince', { t: `${fmt.dateTime(m.paused_since, { seconds: false })} ${fmt.tzLabel()}` })}` : '',
        ' · ',
        m.listening ? t('adm.mode.listening', { a: m.listen_addr || '—' }) : t('adm.mode.notListening'),
        ' · ',
        session.can('audit:read') ? h('a', { href: '/audit?category=settings' }, t('adm.mode.viewAudit')) : null,
      ),
      graceZero && m.mode === 'renew_only' ? inlineAlert({ kind: 'warning', title: t('adm.mode.graceZeroAlert'), code: 'KRS-W2011' }) : null,
      modeLocked ? inlineAlert({ kind: 'info', title: t('adm.mode.lockedByEnv', { v: sMode.env_var }) }) : null,
      h(
        'fieldset',
        { disabled: !canWrite || modeLocked ? true : null },
        h('legend', null, t('adm.mode.legend')),
        h('div', { class: 'ds-radio-group' }, optEls),
        graceZero ? h('p', { class: 'caption', id: `${gname}-grace` }, t('adm.mode.graceZero')) : null,
        srcLine(sMode, srcIds.mode),
      ),
      h(
        'div',
        { class: 'k-stack k-stack--sm' },
        h('label', { class: 'ds-checkbox' }, autoBox, t('adm.mode.auto')),
        srcLine(sAuto, srcIds.auto),
        fh,
        srcLine(sHours, srcIds.hours),
      ),
      h('h3', { class: 'k-section-title' }, t('adm.mode.impactTitle')),
      impactInfo || h('p', { class: 'muted' }, '—'),
      applyBtn ? h('div', { class: 'ds-button-row' }, applyBtn, pending) : null,
    );
    refresh();
  }

  load();
  return { isDirty: () => Boolean(modeDirty && modeDirty()) };
}
