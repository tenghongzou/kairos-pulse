// S11 通知管道（設計包 kairos/screens.md S11；API GET/POST/PUT/DELETE /api/v1/channels、POST /channels/{id}/test、
// POST /channels/test）。
//
// - 路由 /alerts/channels（清單）、/alerts/channels/new?kind=email|webhook（新增）、/alerts/channels/:id（設定與狀態）。
// - 權限：管理員（channels:write）新增、修改、刪除；維運者（channels:read＋channels:test）只能看與寄送測試。
// - 機密（SMTP 密碼、Webhook 網址、標頭、HMAC 金鑰、LINE token）只顯示遮蔽值，只能更換不能檢視；PUT 只送出更換的欄位。
// - 狀態 secrets_unavailable：機密無法解開（alerts-secrets.key 遺失或不成對，KRS-E5110），寫出處理方式。
// - 測試通知的限速（429，〔暫定〕KRS-W5111）：寫出是哪一種限制與幾秒後再試。
// - 0.7.0 隱藏（decisions/INTEGRATION.md §28.6）：每日彙整、Email 的 HTML 版（「內容格式」只有純文字）、Alertmanager 相容格式、
//   代理伺服器欄位（LINE、Teams 需要直接連外）。

import { h, icon, uid, mount } from '../core/dom.js';
import { t, has, severityLabel } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { navigate } from '../core/router.js';
import { pageHead, card, emptyState, errorState, inlineAlert, field, input, button, setBusy, statusBadge, codeTag, dl, skeletonBlock } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { openMenu } from '../ui/menu.js';
import { toast } from '../ui/toast.js';
import { describe, lookup } from '../core/errors.js';
import { unavailableNotice } from './alerts-common.js';

const FORMATS = ['kairos', 'slack', 'teams', 'line'];
const MASK = '••••';

function stateBadge(ch) {
  const map = { ok: 'online', failed: 'critical', secrets_unavailable: 'critical', disabled: 'unknown' };
  if (ch.state === 'ok' && ch.rules_using === 0) return statusBadge('unknown', t('alerts.ch.state.unused'));
  return statusBadge(map[ch.state] || 'unknown', has(`alerts.ch.state.${ch.state}`) ? t(`alerts.ch.state.${ch.state}`) : ch.state);
}

function kindText(ch) {
  if (ch.kind === 'email') return t('alerts.ch.kind.email');
  const f = (ch.config && ch.config.format) || 'kairos';
  return `${t('alerts.ch.kind.webhook')}（${t(`alerts.ch.format.${f}`)}）`;
}

function targetText(ch) {
  const c = ch.config || {};
  if (ch.kind === 'email') return t('alerts.ch.recipients', { n: fmt.num((c.to || []).length) });
  if (c.format === 'line') return t('alerts.ch.lineTo', { to: c.line_to || '—' });
  return h('span', { class: 'mono' }, (ch.secrets && ch.secrets.url) || '—');
}

function lastSend(ch) {
  if (!ch.last_attempt_at) return h('span', { class: 'muted' }, '—');
  const ok = ch.last_success_at && ch.last_success_at >= ch.last_attempt_at;
  return h('span', null, fmt.timeEl(ch.last_attempt_at), ' ', ok ? t('alerts.ch.sendOk') : t('alerts.ch.sendFailed'), !ok && ch.last_error_code ? [' ', codeTag(ch.last_error_code)] : null);
}

/** 機密無法解開的說明（KRS-E5110；ALERTS.md 第 6 節） */
function secretsNotice(n) {
  return inlineAlert({
    kind: 'error',
    title: t('alerts.ch.secretsTitle', { n }),
    code: 'KRS-E5110',
    body: h('div', null, h('p', null, t('alerts.ch.secretsReason')), h('ol', { class: 'k-fix-steps' }, h('li', null, t('alerts.ch.secretsFix1')), h('li', null, t('alerts.ch.secretsFix2')))),
  });
}

/** 測試通知的結果（就地顯示；429 寫出限制） */
function testResult(res, err) {
  if (err) {
    if (err.status === 429) {
      const d = err.details || {};
      return inlineAlert({
        kind: 'warning',
        title: t('alerts.test.limited'),
        code: err.productCode || 'KRS-W5111',
        body: h('p', null, d.scope === 'channel' ? t('alerts.test.limitedChannel', { n: d.limit, s: d.window_secs }) : t('alerts.test.limitedUser', { n: d.limit, m: Math.round((d.window_secs || 600) / 60) }), ' ', t('alerts.test.retryAfter', { s: err.retryAfter || '—' })),
      });
    }
    if (err.code === 'channel_invalid') {
      return inlineAlert({ kind: 'error', title: t('alerts.test.invalid'), code: err.productCode, body: err.message });
    }
    const d = describe(err, { title: t('alerts.test.failed') });
    return inlineAlert({ kind: 'error', title: d.title, code: d.code, body: d.reason });
  }
  if (res.ok) {
    return inlineAlert({
      kind: 'success',
      title: t('alerts.test.ok'),
      body: t('alerts.test.okBody', { sev: severityLabel(res.severity), code: res.response_code ? t('alerts.delivery.http', { n: res.response_code }) : t('alerts.test.noCode'), s: (res.elapsed_ms / 1000).toFixed(1) }),
    });
  }
  const entry = lookup(res.product_code);
  return inlineAlert({
    kind: 'error',
    title: t('alerts.test.notDelivered'),
    code: res.product_code,
    body: h('div', null, h('p', null, res.message), entry && entry.text ? h('p', { class: 'caption' }, entry.text.split('（')[0]) : null, res.response_code ? h('p', { class: 'caption' }, t('alerts.delivery.http', { n: res.response_code })) : null),
  });
}

export function render(ctx) {
  const id = ctx.params.id;
  if (id) return renderDetail(ctx, id);
  return renderList(ctx);
}

// ===========================================================================
// 清單
function renderList(ctx) {
  const root = ctx.root;
  const canWrite = session.can('channels:write');
  const canTest = session.can('channels:test');
  let disposed = false;
  const body = h('div');
  const addBtn = canWrite
    ? h('button', { type: 'button', class: 'ds-button ds-button--primary', 'aria-haspopup': 'menu', 'aria-expanded': 'false' }, icon('plus'), t('alerts.ch.add'), icon('chevron-down'))
    : null;
  if (addBtn) {
    addBtn.addEventListener('click', () =>
      openMenu(addBtn, [
        { text: t('alerts.ch.addEmail'), iconName: 'plus', href: '/alerts/channels/new?kind=email' },
        { text: t('alerts.ch.addWebhook'), iconName: 'plus', href: '/alerts/channels/new?kind=webhook' },
      ], { label: t('alerts.ch.add') }),
    );
  }
  mount(
    root,
    pageHead({ title: t('alerts.ch.title'), crumbs: [{ text: t('nav.section.alerts'), href: '/alerts/events' }, { text: t('alerts.ch.title') }], actions: addBtn }),
    canWrite ? null : inlineAlert({ kind: 'info', title: t('alerts.ch.operatorTitle'), body: t('alerts.ch.operatorBody') }),
    body,
  );
  mount(body, skeletonBlock(5));

  async function testRow(ch, btn, out) {
    setBusy(btn, true, t('alerts.test.sending'));
    try {
      const r = await api(`/api/v1/channels/${encodeURIComponent(ch.channel_id)}/test`, { method: 'POST', body: {} });
      mount(out, testResult(r));
    } catch (err) {
      mount(out, testResult(null, err));
    }
    setBusy(btn, false, true);
  }

  async function load() {
    try {
      const list = await api('/api/v1/channels');
      if (disposed) return;
      if (!list.length) {
        mount(
          body,
          emptyState({
            title: t('alerts.ch.empty'),
            body: t('alerts.ch.emptyBody'),
            iconName: 'bell',
            actions: canWrite ? [h('a', { class: 'ds-button ds-button--primary', href: '/alerts/channels/new?kind=email' }, t('alerts.ch.addEmail')), h('a', { class: 'ds-button ds-button--secondary', href: '/alerts/channels/new?kind=webhook' }, t('alerts.ch.addWebhook'))] : null,
          }),
        );
        return;
      }
      const locked = list.filter((c) => c.state === 'secrets_unavailable').length;
      const out = h('div', { 'aria-live': 'polite' });
      mount(
        body,
        locked ? secretsNotice(locked) : null,
        out,
        h(
          'div',
          { class: 'ds-card ds-card--flush' },
          h(
            'div',
            { class: 'ds-table-wrap' },
            h(
              'table',
              { class: 'ds-table' },
              h('caption', { class: 'ds-visually-hidden' }, t('alerts.ch.caption')),
              h(
                'thead',
                null,
                h(
                  'tr',
                  null,
                  [t('col.status'), t('alerts.ch.col.name'), t('alerts.ch.col.kind'), t('alerts.ch.col.target'), t('alerts.ch.col.rules'), t('alerts.ch.col.last')].map((c, i) => h('th', { scope: 'col', class: i === 4 ? 'num' : null }, c)),
                  canTest ? h('th', { scope: 'col' }, h('span', { class: 'ds-visually-hidden' }, t('alerts.col.actions'))) : null,
                ),
              ),
              h(
                'tbody',
                null,
                list.map((ch) => {
                  const btn = canTest && ch.state !== 'secrets_unavailable' && ch.enabled
                    ? button({ text: t('alerts.test.button'), size: 'sm', attrs: { 'aria-label': t('alerts.test.aria', { name: ch.name }) } })
                    : null;
                  if (btn) btn.addEventListener('click', () => testRow(ch, btn, out));
                  return h(
                    'tr',
                    { dataset: { key: ch.channel_id } },
                    h('td', null, stateBadge(ch)),
                    h('td', null, h('a', { href: `/alerts/channels/${encodeURIComponent(ch.channel_id)}` }, ch.name)),
                    h('td', null, kindText(ch)),
                    h('td', null, targetText(ch)),
                    h('td', { class: 'num' }, fmt.num(ch.rules_using || 0)),
                    h('td', null, lastSend(ch)),
                    canTest ? h('td', null, btn) : null,
                  );
                }),
              ),
            ),
          ),
        ),
      );
    } catch (err) {
      if (disposed) return;
      mount(body, err.code === 'alerts_unavailable' ? unavailableNotice() : null, errorState(err, { title: t('alerts.ch.failed'), onRetry: load }));
    }
  }
  load();
  return {
    dispose() {
      disposed = true;
    },
  };
}

// ===========================================================================
// 新增、設定與狀態
function renderDetail(ctx, id) {
  const root = ctx.root;
  const isNew = id === 'new';
  const canWrite = session.can('channels:write');
  const canTest = session.can('channels:test');
  let disposed = false;
  let dirty = false;
  const crumbs = (name) => [{ text: t('nav.section.alerts'), href: '/alerts/events' }, { text: t('alerts.ch.title'), href: '/alerts/channels' }, { text: name }];

  if (isNew && !canWrite) {
    mount(root, pageHead({ title: t('alerts.ch.newTitle'), crumbs: crumbs(t('alerts.ch.newTitle')) }), emptyState({ title: t('error.403.title'), body: t('alerts.ch.noWrite'), iconName: 'lock' }));
    return {};
  }
  mount(root, pageHead({ title: isNew ? t('alerts.ch.newTitle') : id, crumbs: crumbs(isNew ? t('alerts.ch.newTitle') : id) }), skeletonBlock(8));
  // 未儲存的變更：main.js 依 isDirty() 在站內換頁與關閉分頁時詢問

  async function load() {
    if (isNew) {
      const kind = ctx.query.get('kind') === 'webhook' ? 'webhook' : 'email';
      draw({ kind, name: '', enabled: true, config: kind === 'webhook' ? { format: 'kairos', method: 'POST' } : { security: 'starttls', port: 587, to: [] }, secrets: {} }, null);
      return;
    }
    try {
      const [ch, all] = await Promise.all([api(`/api/v1/channels/${encodeURIComponent(id)}`), api('/api/v1/channels').catch(() => [])]);
      if (disposed) return;
      draw(ch, all);
    } catch (err) {
      if (disposed) return;
      if (err.code === 'channel_not_found') {
        mount(root, pageHead({ title: t('alerts.ch.notFound'), crumbs: crumbs(id) }), emptyState({ title: t('alerts.ch.notFound'), body: t('alerts.ch.notFoundBody', { id }), iconName: 'search', actions: [h('a', { class: 'ds-button ds-button--secondary', href: '/alerts/channels' }, t('alerts.ch.back'))] }));
        return;
      }
      mount(root, pageHead({ title: id, crumbs: crumbs(id) }), err.code === 'alerts_unavailable' ? unavailableNotice() : null, errorState(err, { title: t('alerts.ch.failed'), onRetry: load }));
    }
  }

  function draw(ch, all) {
    const ro = !canWrite;
    const kind = ch.kind;
    const c = ch.config || {};
    const masked = ch.secrets || {};
    const markDirty = () => {
      dirty = true;
    };
    const f = {};
    const mk = (name, value, opts = {}) => {
      const el = input({ name, value: value == null ? '' : String(value), mono: opts.mono, type: opts.type || 'text', autocomplete: 'off', placeholder: opts.placeholder });
      el.disabled = ro;
      el.addEventListener('input', markDirty);
      return el;
    };
    const sel = (name, value, options) => {
      const el = h('select', { class: 'ds-select', name }, options.map(([v, text]) => h('option', { value: v, selected: v === value }, text)));
      el.disabled = ro;
      el.addEventListener('change', markDirty);
      return el;
    };
    const check = (name, checked) => {
      const el = h('input', { type: 'checkbox', name, checked: Boolean(checked) });
      el.disabled = ro;
      el.addEventListener('change', markDirty);
      return el;
    };

    /** 機密欄位（SecretField）：既有的值只顯示遮蔽值，「更換」後才能輸入；回傳 { el, value() → undefined（不變）|字串 } */
    function secretField(name, label, { hint, mono = true, multiline = false } = {}) {
      const has0 = masked[name] != null && masked[name] !== '' && !(typeof masked[name] === 'object' && !Object.keys(masked[name]).length);
      const shown = typeof masked[name] === 'string' ? masked[name] : has0 ? MASK : '';
      let editing = !has0 || isNew;
      const ctl = multiline ? h('textarea', { class: 'ds-textarea ds-input--mono', name, rows: 3, autocomplete: 'off' }) : input({ name, type: name === 'url' ? 'text' : 'password', mono, autocomplete: 'new-password' });
      ctl.disabled = ro;
      ctl.addEventListener('input', markDirty);
      const changeBtn = ro ? null : button({ text: t('alerts.ch.change'), size: 'sm', attrs: { 'aria-label': t('alerts.ch.changeAria', { label }) }, onClick: () => { editing = true; draw2(); ctl.focus(); markDirty(); } });
      const wrap = h('div', { class: 'ds-secret-field' });
      const fld = field({ label, input: ctl, hint });
      const fid = ctl.id;
      // 已儲存的值：唯讀的遮蔽值（同一個 id，標籤仍然對應），按「更換」後換成輸入框
      const view = h('input', { class: 'ds-input ds-input--mono', readonly: true, value: shown || MASK, 'aria-describedby': ctl.getAttribute('aria-describedby') });
      function draw2() {
        if (editing) {
          view.removeAttribute('id');
          ctl.id = fid;
          mount(wrap, ctl);
        } else {
          ctl.removeAttribute('id');
          view.id = fid;
          mount(wrap, view, changeBtn);
        }
      }
      // field() 把輸入框放在標籤後面；改成放 wrap
      ctl.replaceWith(wrap);
      draw2();
      return {
        el: fld,
        field: fld,
        value: () => (editing ? (multiline ? ctl.value : ctl.value.trim()) : undefined),
        editing: () => editing,
      };
    }

    // ---- 共用欄位 ----
    f.name = mk('name', ch.name);
    const nameF = field({ label: t('alerts.ch.f.name'), input: f.name, required: true });
    f.enabled = check('enabled', ch.enabled !== false);
    const isWebhook = kind === 'webhook';
    f.min_severity = sel('min_severity', c.min_severity || '', [
      ['', isWebhook ? t('alerts.ch.f.minDefaultWebhook') : t('alerts.ch.f.minDefaultEmail')],
      ['emergency', severityLabel('emergency')],
      ['critical', severityLabel('critical')],
      ['warning', severityLabel('warning')],
    ]);
    f.show_ip = sel('show_ip', c.show_ip || '', [['', t('alerts.ch.f.ipDefault')], ['full', t('alerts.ch.f.ipFull')], ['subnet', t('alerts.ch.f.ipSubnet')], ['none', t('alerts.ch.f.ipNone')]]);
    f.timezone = mk('timezone', c.timezone, { mono: true, placeholder: '+08:00' });
    f.timeout_secs = mk('timeout_secs', c.timeout_secs, { type: 'number', placeholder: '10' });
    f.notify_ack = check('notify_ack', c.notify_ack);
    f.ca_file = mk('ca_file', c.ca_file, { mono: true, placeholder: '/etc/kairos/smtp-ca.pem' });
    const fields = {
      name: nameF,
      min_severity: field({ label: t('alerts.ch.f.minSeverity'), input: f.min_severity, hint: isWebhook ? t('alerts.ch.f.minHintWebhook') : t('alerts.ch.f.minHintEmail') }),
      show_ip: field({ label: t('alerts.ch.f.showIp'), input: f.show_ip }),
      timezone: field({ label: t('alerts.ch.f.timezone'), input: f.timezone, hint: t('alerts.ch.f.timezoneHint') }),
      timeout_secs: field({ label: t('alerts.ch.f.timeout'), input: f.timeout_secs, hint: t('alerts.ch.f.timeoutHint') }),
      ca_file: field({ label: t('alerts.ch.f.caFile'), input: f.ca_file, hint: t('alerts.ch.f.caFileHint') }),
    };
    const secrets = {};

    // ---- 種類專屬 ----
    let kindBody;
    let formatSel = null;
    if (kind === 'email') {
      f.host = mk('host', c.host, { mono: true, placeholder: 'mail.example.gov.tw' });
      f.port = mk('port', c.port, { type: 'number', placeholder: '587' });
      f.security = sel('security', c.security || 'starttls', [['starttls', 'STARTTLS'], ['tls', 'TLS'], ['none', t('alerts.ch.f.secNone')]]);
      f.username = mk('username', c.username, { mono: true });
      f.from = mk('from', c.from, { mono: true, placeholder: 'kairos@example.gov.tw' });
      f.to = h('textarea', { class: 'ds-textarea ds-input--mono', name: 'to', rows: 3 }, (c.to || []).join('\n'));
      f.to.disabled = ro;
      f.to.addEventListener('input', markDirty);
      secrets.password = secretField('password', t('alerts.ch.f.password'), { hint: t('alerts.ch.f.passwordHint') });
      Object.assign(fields, {
        host: field({ label: t('alerts.ch.f.host'), input: f.host, required: true }),
        port: field({ label: t('alerts.ch.f.port'), input: f.port }),
        security: field({ label: t('alerts.ch.f.security'), input: f.security }),
        username: field({ label: t('alerts.ch.f.username'), input: f.username }),
        from: field({ label: t('alerts.ch.f.from'), input: f.from, required: true }),
        to: field({ label: t('alerts.ch.f.to'), input: f.to, required: true, hint: t('alerts.ch.f.toHint') }),
      });
      kindBody = [
        h('div', { class: 'ds-form-row' }, fields.host, fields.port, fields.security),
        h('div', { class: 'ds-form-row' }, fields.username, secrets.password.el),
        h('div', { class: 'ds-form-row' }, fields.from),
        fields.to,
        h('p', { class: 'ds-field__hint' }, t('alerts.ch.f.formatPlain')),
        fields.ca_file,
      ];
    } else {
      formatSel = sel('format', c.format || 'kairos', FORMATS.map((x) => [x, t(`alerts.ch.format.${x}`)]));
      f.format = formatSel;
      f.method = sel('method', c.method || 'POST', [['POST', 'POST'], ['PUT', 'PUT']]);
      f.allow_http = check('allow_http', c.allow_http);
      f.line_to = mk('line_to', c.line_to, { mono: true, placeholder: 'C1234567890abcdef…' });
      secrets.url = secretField('url', t('alerts.ch.f.url'), { hint: t('alerts.ch.f.urlHint') });
      secrets.headers = secretField('headers', t('alerts.ch.f.headers'), { hint: t('alerts.ch.f.headersHint'), multiline: true });
      secrets.hmac_secret = secretField('hmac_secret', t('alerts.ch.f.hmac'), { hint: t('alerts.ch.f.hmacHint') });
      secrets.line_token = secretField('line_token', t('alerts.ch.f.lineToken'));
      Object.assign(fields, {
        format: field({ label: t('alerts.ch.f.format'), input: formatSel }),
        method: field({ label: t('alerts.ch.f.method'), input: f.method }),
        line_to: field({ label: t('alerts.ch.f.lineTo'), input: f.line_to, required: true, hint: t('alerts.ch.f.lineToHint') }),
      });
      const lineBox = h('div', null, fields.line_to, secrets.line_token.el);
      const outboundNote = h('div');
      const sync = () => {
        const fm = formatSel.value;
        lineBox.hidden = fm !== 'line';
        mount(outboundNote, fm === 'line' || fm === 'teams' ? inlineAlert({ kind: 'info', body: t('alerts.ch.f.outbound') }) : null);
      };
      formatSel.addEventListener('change', sync);
      kindBody = [
        h('div', { class: 'ds-form-row' }, fields.format, fields.method),
        outboundNote,
        secrets.url.el,
        h('label', { class: 'ds-checkbox' }, f.allow_http, t('alerts.ch.f.allowHttp')),
        h('p', { class: 'ds-field__hint' }, t('alerts.ch.f.allowHttpHint')),
        lineBox,
        secrets.headers.el,
        secrets.hmac_secret.el,
        fields.ca_file,
      ];
      sync();
    }

    function readConfig() {
      const cfg = {};
      const set = (k, v) => {
        if (v !== '' && v != null) cfg[k] = v;
      };
      if (kind === 'email') {
        set('host', f.host.value.trim());
        if (f.port.value) cfg.port = Number(f.port.value);
        cfg.security = f.security.value;
        set('username', f.username.value.trim());
        set('from', f.from.value.trim());
        cfg.to = f.to.value.split(/[\s,，;]+/).map((x) => x.trim()).filter(Boolean);
      } else {
        cfg.format = f.format.value;
        cfg.method = f.method.value;
        if (f.allow_http.checked) cfg.allow_http = true;
        if (f.format.value === 'line') set('line_to', f.line_to.value.trim());
      }
      set('min_severity', f.min_severity.value);
      set('show_ip', f.show_ip.value);
      set('timezone', f.timezone.value.trim());
      if (f.timeout_secs.value) cfg.timeout_secs = Number(f.timeout_secs.value);
      if (f.notify_ack.checked) cfg.notify_ack = true;
      set('ca_file', f.ca_file.value.trim());
      return cfg;
    }

    /** 機密：沒有更換的欄位不送出（沿用原值）；清空的欄位送 null（刪除） */
    function readSecrets({ full = false } = {}) {
      const out = {};
      for (const [k, s] of Object.entries(secrets)) {
        const v = s.value();
        if (v === undefined) continue;
        if (k === 'headers') {
          if (!v.trim()) {
            if (!isNew) out.headers = null;
            continue;
          }
          const obj = {};
          for (const line of v.split('\n')) {
            const i = line.indexOf(':');
            if (i > 0) obj[line.slice(0, i).trim()] = line.slice(i + 1).trim();
          }
          out.headers = obj;
        } else if (v === '') {
          if (!isNew && !full) out[k] = null;
        } else {
          out[k] = v;
        }
      }
      return out;
    }

    function clearErrors() {
      for (const fl of Object.values(fields)) fl.setError('');
      for (const s of Object.values(secrets)) s.field.setError('');
    }
    function showParamError(err) {
      const param = (err.details && err.details.parameter) || '';
      const key = param.replace(/^(config|secrets)\./, '');
      const target = param.startsWith('secrets.') ? secrets[key] && secrets[key].field : fields[key];
      if (target) {
        target.setError(err.message);
        const inp = target.querySelector('input, select, textarea, button');
        if (inp) inp.focus();
        return true;
      }
      return false;
    }

    // ---- 測試通知 ----
    const testOut = h('div', { 'aria-live': 'polite', class: 'k-test-out' });
    const testSev = h('select', { class: 'ds-select', name: 'test_severity' }, ['emergency', 'critical', 'warning'].map((s) => h('option', { value: s }, severityLabel(s))));
    const testBtn = button({ text: t('alerts.test.button'), iconName: 'bell', attrs: { id: 'ch-test' } });
    const testNote = h('p', { class: 'ds-field__hint' }, isNew ? t('alerts.test.unsavedNote') : t('alerts.test.savedNote'));
    testBtn.addEventListener('click', async () => {
      clearErrors();
      setBusy(testBtn, true, t('alerts.test.sending'));
      try {
        let r;
        if (isNew) {
          r = await api('/api/v1/channels/test', { method: 'POST', body: { kind, name: f.name.value.trim() || undefined, config: readConfig(), secrets: readSecrets({ full: true }), severity: testSev.value } });
        } else {
          r = await api(`/api/v1/channels/${encodeURIComponent(ch.channel_id)}/test`, { method: 'POST', body: { severity: testSev.value } });
        }
        mount(testOut, testResult(r));
      } catch (err) {
        if (err.code === 'channel_invalid') showParamError(err);
        mount(testOut, testResult(null, err));
      }
      setBusy(testBtn, false, true);
    });
    const canRunTest = isNew ? canWrite : canTest && ch.state !== 'secrets_unavailable';
    const testCard = canRunTest
      ? card({
          title: t('alerts.test.title'),
          body: [h('div', { class: 'k-row' }, h('label', { class: 'k-labelled-select' }, t('alerts.test.severity'), testSev), testBtn), testNote, testOut],
        })
      : null;

    // ---- 儲存 ----
    const errBox = h('div');
    const saveBtn = button({ text: isNew ? t('alerts.ch.create') : t('alerts.ch.save'), variant: 'primary', type: 'submit' });
    const form = h(
      'form',
      { novalidate: true, class: 'k-stack' },
      ch.state === 'secrets_unavailable' ? secretsNotice(1) : null,
      ro ? inlineAlert({ kind: 'info', title: t('alerts.ch.operatorTitle'), body: t('alerts.ch.operatorBody') }) : null,
      card({ title: t(`alerts.ch.section.${kind}`), body: [fields.name, h('label', { class: 'ds-checkbox' }, f.enabled, t('alerts.ch.f.enabled')), ...kindBody] }),
      card({
        title: t('alerts.ch.section.common'),
        body: [
          h('div', { class: 'ds-form-row' }, fields.min_severity, fields.show_ip),
          h('div', { class: 'ds-form-row' }, fields.timezone, fields.timeout_secs),
          h('label', { class: 'ds-checkbox' }, f.notify_ack, t('alerts.ch.f.notifyAck')),
          h('p', { class: 'ds-field__hint' }, t('alerts.ch.f.secretsKey')),
        ],
      }),
      errBox,
      ro ? null : h('div', { class: 'ds-button-row' }, h('a', { class: 'ds-button ds-button--secondary', href: '/alerts/channels' }, t('common.cancel')), saveBtn, !isNew ? button({ text: t('alerts.ch.delete'), variant: 'danger-secondary', iconName: 'trash', onClick: () => removeDialog(ch, all) }) : null),
    );
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      if (ro) return;
      clearErrors();
      mount(errBox);
      if (!f.name.value.trim()) {
        fields.name.setError(t('alerts.ch.f.nameRequired'));
        f.name.focus();
        return;
      }
      setBusy(saveBtn, true, t('common.working'));
      const body = { name: f.name.value.trim(), enabled: f.enabled.checked, config: readConfig(), secrets: readSecrets() };
      if (isNew) body.kind = kind;
      try {
        const saved = isNew
          ? await api('/api/v1/channels', { method: 'POST', body })
          : await api(`/api/v1/channels/${encodeURIComponent(ch.channel_id)}`, { method: 'PUT', body });
        dirty = false;
        toast({ kind: 'success', title: t(isNew ? 'alerts.ch.created' : 'alerts.ch.saved', { name: saved.name }) });
        if (isNew) navigate(`/alerts/channels/${encodeURIComponent(saved.channel_id)}`);
        else load();
      } catch (err) {
        setBusy(saveBtn, false, true);
        if (err.code === 'channel_invalid' || err.code === 'invalid_parameter') {
          if (showParamError(err)) return;
        }
        const d = describe(err, { title: t('alerts.ch.saveFailed') });
        mount(errBox, inlineAlert({ kind: 'error', title: t('alerts.ch.saveFailed'), code: d.code, body: err.code === 'channel_invalid' ? err.message : d.reason }));
      }
    });

    // ---- 狀態（既有的管道） ----
    let statusCards = [];
    if (!isNew) {
      statusCards = [
        card({
          title: t('alerts.ch.statusTitle'),
          body: dl([
            [t('col.status'), stateBadge(ch)],
            [t('alerts.ch.failures'), fmt.num(ch.consecutive_failures || 0)],
            ch.failed_since ? [t('alerts.ch.failedSince'), fmt.timeEl(ch.failed_since)] : null,
            [t('alerts.ch.lastSuccess'), fmt.timeEl(ch.last_success_at)],
            ch.last_error_code || ch.last_error ? [t('alerts.ch.lastError'), [ch.last_error_code ? codeTag(ch.last_error_code) : null, ' ', ch.last_error || '']] : null,
            [t('alerts.ch.updated'), [fmt.timeEl(ch.updated_at), ' · ', ch.updated_by]],
          ]),
        }),
        card({
          title: t('alerts.ch.rulesUsing', { n: fmt.num((ch.rules || []).length) }),
          body: (ch.rules || []).length
            ? h('ul', { class: 'k-plain-list' }, ch.rules.map((r) => h('li', null, session.can('alert-rules:read') ? h('a', { href: `/alerts/rules/${encodeURIComponent(r.rule_id)}` }, r.name) : r.name)))
            : h('p', { class: 'secondary' }, t('alerts.ch.noRules')),
        }),
        card({
          title: t('alerts.ch.deliveries'),
          body: (ch.deliveries || []).length
            ? h(
                'div',
                { class: 'k-scroll-x' },
                h(
                  'table',
                  { class: 'k-mini-table' },
                  h('caption', { class: 'ds-visually-hidden' }, t('alerts.ch.deliveries')),
                  h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('col.time', { tz: fmt.tzLabel() })), h('th', { scope: 'col' }, t('alerts.ch.col.kind')), h('th', { scope: 'col' }, t('col.status')), h('th', { scope: 'col' }, t('alerts.ch.col.event')))),
                  h(
                    'tbody',
                    null,
                    ch.deliveries.map((d) =>
                      h(
                        'tr',
                        null,
                        h('td', null, fmt.dateTime(d.sent_at || d.created_at)),
                        h('td', null, has(`alerts.deliveryKind.${d.kind}`) ? t(`alerts.deliveryKind.${d.kind}`) : d.kind),
                        h('td', null, has(`alerts.delivery.${d.state}`) ? t(`alerts.delivery.${d.state}`) : d.state, d.response_code ? ` · ${d.response_code}` : '', d.error_code ? [' ', codeTag(d.error_code)] : null),
                        h('td', null, d.event_id ? h('a', { href: `/alerts/events/${encodeURIComponent(d.event_id)}` }, d.event_id) : '—'),
                      ),
                    ),
                  ),
                ),
              )
            : h('p', { class: 'secondary' }, t('alerts.ch.noDeliveries')),
        }),
      ];
    }

    const title = isNew ? t(`alerts.ch.newTitle.${kind}`) : ch.name;
    ctx.setTitle(title);
    mount(
      root,
      pageHead({ title, crumbs: crumbs(isNew ? t('alerts.ch.newTitle') : ch.name), meta: isNew ? null : kindText(ch) }),
      h('div', { class: 'k-grid k-grid--side' }, h('div', null, form), h('div', { class: 'k-stack' }, testCard, ...statusCards)),
    );
  }

  /** 刪除管道（L2）：列出受影響的規則，選擇替代管道或確認「沒有通知對象」，輸入管道名稱 */
  function removeDialog(ch, all) {
    const rules = ch.rules || [];
    const others = (all || []).filter((x) => x.channel_id !== ch.channel_id);
    const name = uid('rep');
    const confirmIn = input({ name: 'confirm_name', autocomplete: 'off' });
    const confirmF = field({ label: t('alerts.ch.del.type', { name: ch.name }), input: confirmIn, required: true });
    const repSel = h('select', { class: 'ds-select', name: 'replacement', 'aria-label': t('alerts.ch.del.replaceWith') }, others.map((o) => h('option', { value: o.channel_id }, o.name)));
    const errBox = h('div');
    const ok = button({ text: t('alerts.ch.delete'), variant: 'danger' });
    const cancel = button({ text: t('common.cancel'), onClick: () => d.close() });
    const choice = rules.length
      ? h(
          'fieldset',
          null,
          h('legend', null, t('alerts.ch.del.choice')),
          others.length
            ? h('label', { class: 'ds-radio' }, h('input', { type: 'radio', name, value: 'replace', checked: true }), t('alerts.ch.del.replace'), repSel)
            : null,
          h('label', { class: 'ds-radio' }, h('input', { type: 'radio', name, value: 'none', checked: !others.length }), t('alerts.ch.del.none')),
        )
      : null;
    const d = openDialog({
      title: t('alerts.ch.del.title', { name: ch.name }),
      size: 'md',
      danger: true,
      body: h(
        'div',
        { class: 'k-stack' },
        h('p', null, rules.length ? t('alerts.ch.del.rules', { n: rules.length }) : t('alerts.ch.del.noRules')),
        rules.length ? h('ul', null, rules.map((r) => h('li', null, r.name))) : null,
        choice,
        confirmF,
        errBox,
      ),
      actions: [cancel, ok],
      initialFocus: cancel,
    });
    ok.addEventListener('click', async () => {
      if (confirmIn.value.trim() !== ch.name) {
        confirmF.setError(t('alerts.ch.del.mismatch'));
        confirmIn.focus();
        return;
      }
      const body = { confirm_name: confirmIn.value.trim() };
      if (rules.length) {
        const v = d.el.querySelector(`input[name="${name}"]:checked`);
        if (v && v.value === 'replace') body.replacement_channel_id = repSel.value;
        else body.accept_no_channel = true;
      }
      setBusy(ok, true, t('common.working'));
      try {
        await api(`/api/v1/channels/${encodeURIComponent(ch.channel_id)}`, { method: 'DELETE', body });
        dirty = false;
        d.close();
        toast({ kind: 'success', title: t('alerts.ch.deleted', { name: ch.name }) });
        navigate('/alerts/channels');
      } catch (err) {
        setBusy(ok, false, true);
        const dd = describe(err, { title: t('alerts.ch.deleteFailed') });
        mount(errBox, inlineAlert({ kind: 'error', title: dd.title, body: err.message }));
      }
    });
  }

  load();
  return {
    dispose() {
      disposed = true;
    },
    isDirty: () => dirty,
  };
}

