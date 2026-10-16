// 指標轉出（S21「整合」；商業審計 F7、kairos/EXPORT.md）：Prometheus exporter 與 OTLP 推送的設定與狀態。
//
// - 讀取：GET /api/v1/export/settings（settings:read）。每個主控台可修改的鍵都有來源（環境變數 ＞ 主控台 ＞
//   kairos.toml ＞ 內建預設）；環境變數設定的鍵唯讀並寫出變數名稱（與 S21 其他設定相同的規則）。
// - 修改（export:write，只有管理員）：PUT /api/v1/export/settings 只送出有變更的鍵；L1 確認列出變更、原因選填。
//   「改用設定檔的值」送 null。OTLP 的 HTTP 標頭是機密：只顯示名稱與遮蔽值，只能更換或清除。
// - 測試連線：POST /api/v1/export/otlp/test（以已儲存的設定送出空的 OTLP 請求），結果放在 aria-live 區域。
// - 狀態：Prometheus 最近一次抓取；OTLP 的佇列、送出、丟棄、最近一次成功與錯誤（代碼為暫定的 KRS-E4311～E4315）。

import { h, uid, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { card, errorState, errorAlert, field, input, inlineAlert, skeletonBlock, neutralBadge, statusBadge, codeTag, dl, setBusy } from '../ui/components.js';
import { toast } from '../ui/toast.js';
import { confirmL1, valueText } from './admin-common.js';

const K = {
  promEnabled: 'export.prometheus.enabled',
  promStale: 'export.prometheus.stale_after_secs',
  promProcesses: 'export.prometheus.include_processes',
  otlpEnabled: 'export.otlp.enabled',
  otlpEndpoint: 'export.otlp.endpoint',
  otlpInterval: 'export.otlp.interval_secs',
  otlpNaming: 'export.otlp.naming',
  otlpHeaders: 'export.otlp.headers',
};
const HEADER_NAME = /^[A-Za-z0-9!#$%&'*+.^_`|~-]{1,128}$/;
const MAX_HEADERS = 16;

function srcText(s) {
  if (!s) return '';
  if (s.source === 'env') return t('exp.src.env', { v: s.env_var });
  if (s.source === 'console') return t('exp.src.console', { by: s.updated_by || '—', at: s.updated_at ? `${fmt.dateTime(s.updated_at, { seconds: false })} ${fmt.tzLabel()}` : '—' });
  return t(`exp.src.${s.source}`);
}

function stateBadge(state) {
  const status = { ok: 'online', failing: 'critical', pending: 'unknown', not_configured: 'warning', disabled: 'offline' }[state] || 'unknown';
  return statusBadge(status, t(`exp.otlp.state.${state}`));
}

/** 指標轉出的卡片；回傳的元素帶 isDirty()（離開頁面時的未儲存變更提示） */
export function exportCard() {
  const canWrite = session.can('export:write');
  const body = h('div', null, skeletonBlock(6));
  const el = card({ title: t('exp.title'), body });
  let dirty = () => false;
  el.isDirty = () => dirty();

  async function load() {
    let d;
    try {
      d = await api('/api/v1/export/settings');
    } catch (err) {
      mount(body, errorState(err, { title: t('exp.loadFailed'), onRetry: load }));
      return;
    }
    draw(d);
  }

  function draw(d) {
    const byKey = new Map((d.settings || []).map((s) => [s.key, s]));
    const locked = (k) => !canWrite || (byKey.get(k) && byKey.get(k).locked);
    const fields = [];
    const srcLine = (k) => {
      const s = byKey.get(k);
      const id = uid('src');
      const reset = canWrite && s && s.source === 'console' && !s.locked
        ? h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', 'aria-label': `${t('exp.useFile')}：${k}`, on: { click: () => saveFlow({ [k]: null }) } }, t('exp.useFile'))
        : null;
      return { id, node: h('p', { class: 'caption', id }, s ? neutralBadge(t(`adm.set.src.${s.source}`), s.source === 'env' ? 'lock' : null) : null, ' ', srcText(s), ' ', reset) };
    };
    const describe = (inputEl, id) => inputEl.setAttribute('aria-describedby', `${inputEl.getAttribute('aria-describedby') || ''} ${id}`.trim());
    const checkbox = (k, name, label, checked) => {
      const box = h('input', { type: 'checkbox', class: 'ds-check', name, checked: Boolean(checked), disabled: locked(k) });
      const src = srcLine(k);
      describe(box, src.id);
      fields.push({ key: k, get: () => box.checked, initial: Boolean(checked), el: box });
      return h('div', { class: 'k-stack k-stack--sm' }, h('label', { class: 'ds-checkbox' }, box, label), src.node);
    };
    const textField = (k, name, label, value, hint, { numeric = false, mono = false, maxlength } = {}) => {
      const inp = input({ name, value: value == null ? '' : String(value), mono, inputmode: numeric ? 'numeric' : undefined, maxlength, autocomplete: 'off', cls: numeric ? 'ds-input--num' : null });
      inp.disabled = locked(k);
      const f = field({ label, input: inp, hint });
      const src = srcLine(k);
      describe(inp, src.id);
      f.appendChild(src.node);
      fields.push({ key: k, get: () => inp.value.trim(), initial: value == null ? '' : String(value), el: inp, f, numeric });
      return f;
    };

    // ---- Prometheus ----
    const p = d.prometheus;
    const ps = p.status || {};
    const promSection = h(
      'section',
      { class: 'k-stack', 'aria-labelledby': 'exp-prom-title' },
      h('h3', { class: 'k-section-title', id: 'exp-prom-title' }, t('exp.prom.title'), ' ', p.enabled ? statusBadge('online', t('exp.enabled')) : statusBadge('offline', t('exp.disabled'))),
      dl([
        [t('exp.prom.endpoint'), h('span', null, codeTag(`GET ${p.path}`), h('span', { class: 'k-block caption' }, t('exp.prom.endpointHint')))],
        [t('exp.prom.lastLabel'), ps.last_scrape_at ? t('exp.prom.last', { t: `${fmt.dateTime(ps.last_scrape_at)} ${fmt.tzLabel()}`, hosts: fmt.num(ps.last_hosts), series: fmt.num(ps.last_series), size: fmt.bytes(ps.last_bytes), ms: fmt.num(ps.last_duration_ms) }) : t('exp.prom.never')],
        [t('exp.prom.scrapesLabel'), t('exp.prom.scrapes', { n: fmt.num(ps.scrapes || 0) })],
      ]),
      checkbox(K.promEnabled, 'prometheus_enabled', t('exp.prom.enabled'), p.enabled),
      textField(K.promStale, 'stale_after_secs', t('exp.prom.stale'), p.stale_after_secs, t('exp.prom.staleHint'), { numeric: true, maxlength: 5 }),
      checkbox(K.promProcesses, 'include_processes', t('exp.prom.processes'), p.include_processes),
      h('p', { class: 'caption' }, t('exp.prom.processesHint')),
    );

    // ---- OTLP ----
    const o = d.otlp;
    const os = o.status || {};
    const lastErr = os.last_error;
    const namingSel = h(
      'select',
      { class: 'ds-select', name: 'naming', disabled: locked(K.otlpNaming) },
      ['semconv', 'kairos'].map((v) => h('option', { value: v, selected: o.naming === v }, t(`exp.otlp.naming.${v}`))),
    );
    const fNaming = field({ label: t('exp.otlp.naming'), input: namingSel });
    const namingSrc = srcLine(K.otlpNaming);
    describe(namingSel, namingSrc.id);
    fNaming.appendChild(namingSrc.node);
    fields.push({ key: K.otlpNaming, get: () => namingSel.value, initial: o.naming, el: namingSel });

    // 標頭：不更換／更換（名稱＋值）／清除主控台的值
    let headerMode = 'keep';
    const headerRows = h('div', { class: 'k-stack k-stack--sm' });
    const rows = [];
    const addRow = (name = '', value = '') => {
      if (rows.length >= MAX_HEADERS) return;
      const n = rows.length + 1;
      const ni = input({ name: `header_name_${n}`, value: name, autocomplete: 'off', maxlength: 128, mono: true });
      const vi = input({ type: 'password', name: `header_value_${n}`, value, autocomplete: 'new-password', maxlength: 8192, mono: true });
      const row = { ni, vi };
      const rm = h('button', { type: 'button', class: 'ds-button ds-button--ghost ds-button--sm', 'aria-label': t('exp.otlp.headerRemove', { n }) }, t('exp.otlp.remove'));
      const wrap = h('div', { class: 'k-header-row' }, field({ label: t('exp.otlp.headerName', { n }), input: ni }), field({ label: t('exp.otlp.headerValue', { n }), input: vi }), rm);
      rm.addEventListener('click', () => {
        rows.splice(rows.indexOf(row), 1);
        wrap.remove();
        refresh();
      });
      ni.addEventListener('input', refresh);
      vi.addEventListener('input', refresh);
      rows.push(row);
      headerRows.appendChild(wrap);
      refresh();
    };
    const headersLocked = locked(K.otlpHeaders);
    const hs = byKey.get(K.otlpHeaders);
    const hdrSrc = srcLine(K.otlpHeaders);
    const addBtn = h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => addRow() } }, t('exp.otlp.headerAdd'));
    const editor = h('div', { class: 'k-stack k-stack--sm', hidden: true }, headerRows, addBtn);
    const modeName = uid('hdr');
    const modeRadio = (v, label) => {
      const r = h('input', { type: 'radio', name: modeName, value: v, checked: v === 'keep', disabled: headersLocked });
      r.addEventListener('change', () => {
        headerMode = v;
        editor.hidden = v !== 'replace';
        if (v === 'replace' && !rows.length) addRow();
        refresh();
      });
      return h('label', { class: 'ds-radio' }, r, label);
    };
    const headerFieldset = h(
      'fieldset',
      { 'aria-describedby': hdrSrc.id },
      h('legend', null, t('exp.otlp.headers')),
      h(
        'p',
        null,
        (o.headers || []).length ? o.headers.map((x, i) => [i ? '、' : '', h('span', { class: 'mono' }, `${x.name}: ${x.value}`)]) : t('exp.otlp.headersNone'),
        o.headers_source && o.headers_source !== 'none' ? h('span', { class: 'k-block caption' }, t(`exp.otlp.headersFrom.${o.headers_source}`)) : null,
      ),
      hdrSrc.node,
      canWrite && !headersLocked
        ? [
            h('div', { class: 'ds-radio-group' }, modeRadio('keep', t('exp.otlp.headersKeep')), modeRadio('replace', t('exp.otlp.headersReplace')), hs && hs.source === 'console' ? modeRadio('clear', t('exp.otlp.headersClear')) : null),
            h('p', { class: 'caption' }, t('exp.otlp.headersHint')),
            editor,
          ]
        : null,
    );

    const testOut = h('div', { 'aria-live': 'polite' });
    const testBtn = canWrite
      ? h('button', { type: 'button', class: 'ds-button ds-button--secondary', disabled: !o.endpoint, title: o.endpoint ? t('exp.otlp.testHint') : t('exp.otlp.testNeedSave') }, t('exp.otlp.test'))
      : null;
    if (testBtn) testBtn.addEventListener('click', () => runTest(testBtn, testOut));

    const otlpSection = h(
      'section',
      { class: 'k-stack', 'aria-labelledby': 'exp-otlp-title' },
      h('h3', { class: 'k-section-title', id: 'exp-otlp-title' }, t('exp.otlp.title'), ' ', stateBadge(os.state)),
      dl([
        [t('exp.otlp.queue'), t('exp.otlp.queueValue', { n: fmt.num(os.queue_points || 0), max: fmt.num(o.queue_max_points) })],
        [t('exp.otlp.sent'), t('exp.otlp.sentValue', { n: fmt.num(os.sent_points || 0), ok: fmt.num(os.requests_ok || 0), failed: fmt.num(os.requests_failed || 0) })],
        [t('exp.otlp.dropped'), t('exp.otlp.droppedValue', { full: fmt.num(os.dropped_points.queue_full), failed: fmt.num(os.dropped_points.failed), rejected: fmt.num(os.dropped_points.rejected) })],
        [t('exp.otlp.lastSuccess'), os.last_success_at ? `${fmt.dateTime(os.last_success_at)} ${fmt.tzLabel()}` : '—'],
        lastErr ? [t('exp.otlp.lastError'), h('span', null, codeTag(lastErr.code), ' ', `${fmt.dateTime(lastErr.at)} ${fmt.tzLabel()}`, h('span', { class: 'k-block k-break' }, lastErr.message))] : null,
      ]),
      os.state === 'failing' && lastErr ? inlineAlert({ kind: 'warning', title: t('exp.otlp.state.failing'), body: lastErr.message, code: lastErr.code }) : null,
      checkbox(K.otlpEnabled, 'otlp_enabled', t('exp.otlp.enabled'), o.enabled),
      textField(K.otlpEndpoint, 'endpoint', t('exp.otlp.endpoint'), o.endpoint, t('exp.otlp.endpointHint'), { mono: true, maxlength: 2048 }),
      textField(K.otlpInterval, 'interval_secs', t('exp.otlp.interval'), o.interval_secs, t('exp.otlp.intervalHint'), { numeric: true, maxlength: 4 }),
      fNaming,
      headerFieldset,
      h('p', { class: 'caption' }, t('exp.otlp.fileOnly'), ' ', codeTag('kairos.toml [export.otlp]')),
      testBtn ? h('div', { class: 'ds-button-row' }, testBtn) : null,
      testOut,
    );

    // ---- 儲存 ----
    const pending = h('span', { class: 'caption', 'aria-live': 'polite' });
    const saveBtn = canWrite ? h('button', { type: 'button', class: 'ds-button ds-button--primary', 'aria-disabled': 'true', title: t('exp.same') }, t('exp.save')) : null;

    function changes() {
      const out = {};
      for (const f of fields) {
        if (f.el.disabled) continue;
        const v = f.get();
        if (String(v) !== String(f.initial)) out[f.key] = v;
      }
      if (headerMode === 'replace') out[K.otlpHeaders] = rows.map((r) => ({ name: r.ni.value.trim(), value: r.vi.value.trim() })).filter((x) => x.name || x.value);
      if (headerMode === 'clear') out[K.otlpHeaders] = null;
      return out;
    }

    function refresh() {
      if (!saveBtn) return;
      const ch = Object.keys(changes()).length > 0;
      if (ch) {
        saveBtn.removeAttribute('aria-disabled');
        saveBtn.removeAttribute('title');
      } else {
        saveBtn.setAttribute('aria-disabled', 'true');
        saveBtn.title = t('exp.same');
      }
      pending.textContent = ch ? t('exp.notSaved') : '';
    }
    for (const f of fields) {
      f.el.addEventListener(f.el.tagName === 'INPUT' && f.el.type === 'checkbox' ? 'change' : f.el.tagName === 'SELECT' ? 'change' : 'input', refresh);
    }
    dirty = () => Boolean(saveBtn) && Object.keys(changes()).length > 0;

    function validate(ch) {
      let first = null;
      const fail = (f, msg) => {
        f.f.setError(msg);
        if (!first) first = f.el;
      };
      for (const f of fields) if (f.f) f.f.setError('');
      const byK = (k) => fields.find((f) => f.key === k);
      const set = (k) => k in ch && ch[k] !== null;
      if (set(K.promStale) && !(/^\d+$/.test(ch[K.promStale]) && Number(ch[K.promStale]) >= 30 && Number(ch[K.promStale]) <= 86400)) fail(byK(K.promStale), t('exp.prom.staleInvalid'));
      if (set(K.otlpInterval) && !(/^\d+$/.test(ch[K.otlpInterval]) && Number(ch[K.otlpInterval]) >= 1 && Number(ch[K.otlpInterval]) <= 3600)) fail(byK(K.otlpInterval), t('exp.otlp.intervalInvalid'));
      const ep = byK(K.otlpEndpoint);
      const epVal = ep.get();
      if (set(K.otlpEndpoint) && epVal && !/^https?:\/\/[^\s/]+/i.test(epVal)) fail(ep, t('exp.otlp.endpointInvalid'));
      const enabled = K.otlpEnabled in ch ? ch[K.otlpEnabled] : byK(K.otlpEnabled).initial;
      if (enabled && !epVal && !ep.el.disabled && ch[K.otlpEndpoint] !== null) fail(ep, t('exp.otlp.endpointMissing'));
      if (Array.isArray(ch[K.otlpHeaders]) && ch[K.otlpHeaders].some((x) => !HEADER_NAME.test(x.name) || !x.value)) {
        toast({ kind: 'error', title: t('exp.otlp.headerInvalid') });
        if (!first && rows.length) first = rows[0].ni;
      }
      if (first) first.focus();
      return !first;
    }

    function toBody(ch) {
      const bodyOut = { prometheus: {}, otlp: {} };
      for (const [k, v] of Object.entries(ch)) {
        const [, section, name] = k.split('.');
        let val = v;
        if (v !== null && (k === K.promStale || k === K.otlpInterval)) val = Number(v);
        bodyOut[section][name] = val;
      }
      if (!Object.keys(bodyOut.prometheus).length) delete bodyOut.prometheus;
      if (!Object.keys(bodyOut.otlp).length) delete bodyOut.otlp;
      return bodyOut;
    }

    function lines(ch) {
      return Object.entries(ch).map(([k, v]) => {
        if (k === K.otlpHeaders) return v === null ? t('exp.change.headersClear') : t('exp.change.headers', { names: v.map((x) => x.name).join('、') || '—' });
        if (v === null) return t('exp.change.reset', { k });
        const s = byKey.get(k);
        return t('exp.change', { k, from: valueText(s ? s.value : ''), to: valueText(v) });
      });
    }

    async function saveFlow(ch) {
      if (!validate(ch)) return;
      await confirmL1({
        title: t('exp.confirmTitle'),
        body: h('div', null, h('ul', null, lines(ch).map((l) => h('li', null, l))), h('p', { class: 'caption' }, t('exp.confirmBody'))),
        confirmText: t('exp.confirm'),
        onConfirm: async ({ reason, errBox }) => {
          try {
            const res = await api('/api/v1/export/settings', { method: 'PUT', body: { ...toBody(ch), reason: reason || undefined } });
            toast({ kind: 'success', title: t('exp.saved') });
            draw(res);
            return true;
          } catch (err) {
            const a = errorAlert(err, { title: t('exp.saveFailed') });
            mount(errBox, a);
            a.focus();
            return false;
          }
        },
      });
    }
    if (saveBtn) saveBtn.addEventListener('click', () => {
      const ch = changes();
      if (Object.keys(ch).length) saveFlow(ch);
    });

    mount(
      body,
      h(
        'div',
        { class: 'k-stack' },
        h('p', null, t('exp.lead'), ' ', t('exp.docs'), ' ', codeTag('kairos/EXPORT.md'), '。'),
        canWrite ? null : inlineAlert({ kind: 'info', title: t('exp.readOnly') }),
        (d.problems || []).length ? inlineAlert({ kind: 'warning', title: t('exp.problems'), body: h('ul', null, d.problems.map((x) => h('li', null, x))), code: 'KRS-E4314' }) : null,
        promSection,
        otlpSection,
        saveBtn ? h('div', { class: 'ds-button-row' }, saveBtn, pending) : null,
      ),
    );
    refresh();
  }

  async function runTest(btn, out) {
    setBusy(btn, true, t('common.working'));
    mount(out);
    try {
      const r = await api('/api/v1/export/otlp/test', { method: 'POST' });
      if (r.ok) mount(out, inlineAlert({ kind: 'success', title: t('exp.otlp.testOk', { msg: r.message, ms: fmt.num(r.elapsed_ms) }) }));
      else mount(out, inlineAlert({ kind: 'error', title: t('exp.otlp.testFailed'), body: r.message, code: r.code || undefined }));
    } catch (err) {
      mount(out, errorAlert(err, { title: t('exp.otlp.testFailed') }));
    } finally {
      setBusy(btn, false, t('common.working'));
    }
  }

  load();
  return el;
}
