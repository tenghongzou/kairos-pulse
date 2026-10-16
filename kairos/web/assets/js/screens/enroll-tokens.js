// S12 註冊 token（設計包 kairos/screens.md S12；API.md 第 11.2 節）：清單、詳情（部署進度漏斗）、建立、停用。
//
// - token 的完整值只在建立的回應中出現一次（D33）：只顯示在「建立完成」對話框（SecretField，預設遮蔽），
//   不寫入網址、localStorage、sessionStorage、主控台日誌；對話框關閉後連同 DOM 一起丟棄。之後只顯示前 4 碼（hint）。
// - 安裝指令：註冊端點不提供下載時（`downloads.available = false`，INTEGRATION §27.4 #3）隱藏 Linux 一行指令、
//   MSI 下載與「下載指令檔」，改顯示「以安裝套件安裝後執行 pulse enroll --install-string …」。0.9 起設定了
//   enroll.download_dir 時另顯示一行指令（curl 下載 pulse-install.sh，以 --base-url 指向 8444 的下載端點）。
// - 註冊端點不是開放（僅續簽、關閉）時，安裝字串與指令的「複製」停用，token 仍可建立（S12 模式提示）。
// - 權限：建立、停用需要 enroll.tokens:write；自動核准需要 enroll.tokens:auto_approve（維運者看到停用的選項與原因）。
// - 0.7.0 隱藏：「時鐘偏差暫不受理」與「排隊中」的主機清單（只有計數，沒有清單 API）、「已簽發未上線」的主機清單、
//   「查看此項的稽核紀錄」（S18 之後）、改為開放（S21）。

import { h, icon, announce, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { replaceQuery } from '../core/router.js';
import { pageHead, card, emptyState, errorState, field, input, inlineAlert, copyButton, dl, skeletonBlock, button } from '../ui/components.js';
import { openDialog, confirm } from '../ui/dialog.js';
import { openMenu } from '../ui/menu.js';
import { toast } from '../ui/toast.js';
import { createTable } from '../ui/table.js';
import { actionDialog, selectField, textareaField, checkbox, linkTabs, progressBar, fetchMode, modeAlert, issuanceAlert, installBlocked, tokenStateLabel, codeBlock } from './pki-common.js';

const STATES = ['active', 'expired', 'exhausted', 'disabled', 'all'];
const EXPIRY_DAYS = [1, 7, 30, 90];

function splitList(text) {
  return text
    .split(/[\s,，、]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/** 核准方式的文字（含批次部署模式） */
function approvalText(tk) {
  const a = tk.approval === 'auto' ? t('token.approval.auto') : t('token.approval.manual');
  return tk.batch_mode ? `${a} · ${t('token.batchMode')}` : a;
}

// ---------------------------------------------------------------------------
// 建立（也供 S14「建立限定這些主機的 token…」預填使用）

/**
 * 開啟建立 token 的對話框。
 * @param {object} prefill  name、group、tags、max_uses、allowed_agent_ids、expiry_days
 * @param {Function} onCreated  建立完成且「完成」對話框關閉後
 */
export async function openCreateToken({ prefill = {}, onCreated } = {}) {
  const canAuto = session.can('enroll.tokens:auto_approve');
  // 憑證政策的有效天數（提示用；沒有 settings:read 時省略）
  let policyDays = null;
  if (session.can('settings:read')) {
    try {
      const s = await api('/api/v1/settings');
      const row = (s.settings || []).find((x) => x.key === 'pki.policy.cert_lifetime_days');
      policyDays = row ? row.value : null;
    } catch {
      policyDays = null;
    }
  }
  const name = input({ name: 'name', maxlength: 64, value: prefill.name || '' });
  const fName = field({ label: t('token.f.name'), input: name, required: true, hint: t('token.f.nameHint') });
  const group = input({ name: 'group', maxlength: 64, value: prefill.group || '' });
  const fGroup = field({ label: t('token.f.group'), input: group, required: true });
  const tags = input({ name: 'tags', value: (prefill.tags || []).join(', ') });
  const fTags = field({ label: t('token.f.tags'), input: tags, hint: t('token.f.tagsHint') });
  const exp = selectField({
    label: t('token.f.expiry'),
    name: 'expiry',
    value: String(prefill.expiry_days || 7),
    options: EXPIRY_DAYS.map((d) => [String(d), d === 90 ? t('token.f.expiryMax', { d }) : t('token.f.expiryDays', { d })]),
  });
  const maxUses = input({ name: 'max_uses', inputmode: 'numeric', value: prefill.max_uses ? String(prefill.max_uses) : '' });
  const fMax = field({ label: t('token.f.maxUses'), input: maxUses, required: true, hint: t('token.f.maxUsesHint') });

  // 核准方式（radio card；自動核准限管理員，維運者看到停用與原因：screens.md §0.2）
  const apId = 'tk-ap';
  const whyId = 'tk-ap-why';
  const rManual = h('input', { type: 'radio', name: apId, value: 'manual', checked: true });
  const rAuto = h('input', { type: 'radio', name: apId, value: 'auto', disabled: !canAuto, 'aria-describedby': whyId });
  const approval = h(
    'fieldset',
    { class: 'ds-field' },
    h('legend', { class: 'ds-field__label' }, t('token.f.approval')),
    h('label', { class: 'ds-radio-card' }, rManual, h('span', null, h('strong', null, t('token.approval.manualLong')), h('br'), h('span', { class: 'muted' }, t('token.approval.manualDesc')))),
    h(
      'label',
      { class: ['ds-radio-card', canAuto ? null : 'is-disabled'] },
      rAuto,
      h('span', null, h('strong', null, t('token.approval.auto')), h('br'), h('span', { class: 'muted', id: whyId }, canAuto ? t('token.approval.autoDesc') : t('token.approval.autoAdminOnly'))),
    ),
  );
  const cidrs = textareaField({ label: t('token.f.cidrs'), name: 'allowed_cidrs', hint: t('token.f.cidrsHint'), rows: 2, mono: true });
  const batch = checkbox({ label: t('token.f.batch'), name: 'batch_mode', hint: t('token.f.batchHint') });
  const agents = textareaField({ label: t('token.f.agents'), name: 'allowed_agent_ids', hint: t('token.f.agentsHint'), rows: 2, mono: true });
  if (prefill.allowed_agent_ids) agents.input.value = prefill.allowed_agent_ids.join('\n');
  const days = input({ name: 'cert_lifetime_days', inputmode: 'numeric', value: '' });
  const fDays = field({
    label: t('token.f.certDays'),
    input: days,
    hint: policyDays ? t('token.f.certDaysHintPolicy', { d: policyDays }) : t('token.f.certDaysHint'),
  });
  // 自動核准或批次部署模式時「限定來源網段」必填（限定主機時可不設）
  const cidrLabel = cidrs.el.querySelector('label');
  const syncCidrLabel = () => {
    const need = rAuto.checked || batch.input.checked;
    cidrLabel.textContent = need ? t('token.f.cidrsRequired') : t('token.f.cidrs');
  };
  rAuto.addEventListener('change', syncCidrLabel);
  rManual.addEventListener('change', syncCidrLabel);
  batch.input.addEventListener('change', syncCidrLabel);

  const fields = { name: fName, group: fGroup, tags: fTags, max_uses: fMax, allowed_cidrs: cidrs.el, allowed_agent_ids: agents.el, cert_lifetime_days: fDays };
  const clearErrors = () => Object.values(fields).forEach((f) => f.setError(''));

  const created = await actionDialog({
    title: t('token.create.title'),
    size: 'lg',
    confirmText: t('token.create.submit'),
    errorTitle: t('token.create.failed'),
    content: [
      fName,
      h('div', { class: 'ds-form-row' }, fGroup, fTags),
      h('div', { class: 'ds-form-row' }, exp.el, fMax),
      approval,
      cidrs.el,
      batch.el,
      agents.el,
      fDays,
    ],
    collect() {
      clearErrors();
      const v = {
        name: name.value.trim(),
        group: group.value.trim(),
        approval: rAuto.checked ? 'auto' : 'manual',
        expires_in_secs: Number(exp.input.value) * 86400,
        batch_mode: batch.input.checked,
      };
      const tg = splitList(tags.value);
      if (tg.length) v.tags = tg;
      const cl = splitList(cidrs.input.value);
      if (cl.length) v.allowed_cidrs = cl;
      const al = splitList(agents.input.value).map((s) => s.toLowerCase());
      if (al.length) v.allowed_agent_ids = al;
      let bad = null;
      const err = (f, msg) => {
        fields[f].setError(msg);
        if (!bad) bad = f;
      };
      if (!v.name) err('name', t('token.err.name'));
      if (!v.group) err('group', t('token.err.group'));
      const mu = Number(maxUses.value.trim());
      if (!/^\d+$/.test(maxUses.value.trim()) || mu < 1 || mu > 100000) err('max_uses', t('token.err.maxUses'));
      else v.max_uses = mu;
      if ((v.approval === 'auto' || v.batch_mode) && !cl.length && !(v.approval !== 'auto' && al.length)) {
        if (v.approval === 'auto' && !al.length) err('allowed_cidrs', t('token.err.cidrsAuto'));
        else if (v.batch_mode) err('allowed_cidrs', t('token.err.cidrsBatch'));
      }
      if (days.value.trim()) {
        const d = Number(days.value.trim());
        if (!/^\d+$/.test(days.value.trim()) || d < 90 || d > 825) err('cert_lifetime_days', t('token.err.certDays'));
        else v.cert_lifetime_days = d;
      }
      if (bad) {
        const target = fields[bad].querySelector('input, textarea, select');
        if (target) target.focus();
        return null;
      }
      return v;
    },
    async submit(v) {
      try {
        return await api('/api/v1/enroll/tokens', { method: 'POST', body: v });
      } catch (e) {
        // 欄位格式錯誤顯示在欄位下（README §4.3）
        const p = e.code === 'invalid_parameter' && e.details && e.details.parameter;
        if (p && fields[p]) {
          fields[p].setError(e.message);
          const target = fields[p].querySelector('input, textarea, select');
          if (target) target.focus();
          throw Object.assign(e, { fieldShown: true });
        }
        throw e;
      }
    },
  });
  if (!created || created === true) return;
  showCreated(created, onCreated);
}

/** 建立完成：token 只顯示這一次（SecretField；不寫入網址或 storage） */
function showCreated(data, onCreated) {
  let token = data.token;
  let install = data.install_string || '';
  const blocked = installBlocked({ state: data.enroll_mode });
  const secret = h('input', { class: 'ds-input', id: 'tk-secret', type: 'password', readonly: true, value: token, autocomplete: 'off', spellcheck: 'false' });
  const eye = h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', 'aria-pressed': 'false', 'aria-controls': 'tk-secret' }, icon('eye'), t('token.done.show'));
  eye.addEventListener('click', () => {
    const show = secret.type === 'password';
    secret.type = show ? 'text' : 'password';
    eye.setAttribute('aria-pressed', show ? 'true' : 'false');
    mount(eye, icon(show ? 'eye-off' : 'eye'), show ? t('token.done.hide') : t('token.done.show'));
  });
  const pinText = Array.isArray(data.ca_pin_display) ? data.ca_pin_display.join('\n') : data.ca_pin_display || data.ca_pin || '—';
  const cmd = install ? `pulse enroll --install-string '${install}'` : '';
  // 下載端點可用時（enroll.download_dir；API 的 downloads.base_url）另顯示一行安裝指令
  const dlBase = data.downloads && data.downloads.available ? data.downloads.base_url : null;
  const oneLiner =
    install && dlBase
      ? `curl -fsSLO --cacert kairos-ca.pem ${dlBase}/pulse-install.sh && sudo PULSE_INSTALL_STRING='${install}' sh pulse-install.sh --base-url ${dlBase} --ca-file kairos-ca.pem --allowed-signers /etc/kairos-pulse/allowed_signers`
      : '';
  const body = h(
    'div',
    { class: 'k-stack--sm', id: 'tk-created' },
    inlineAlert({ kind: 'warning', title: t('token.done.once'), body: t('token.done.onceBody') }),
    blocked ? inlineAlert({ kind: 'warning', title: t('token.done.blocked'), body: data.enroll_mode === 'closed' ? t('pki.mode.closed.body') : t('pki.mode.renewOnly.body') }) : null,
    h(
      'div',
      { class: 'ds-field' },
      h('label', { class: 'ds-field__label', for: 'tk-secret' }, t('token.done.token')),
      h('div', { class: 'ds-secret-field' }, secret, eye, copyButton(() => token, t('token.done.copyToken'))),
    ),
    h('div', { class: 'ds-field' }, h('p', { class: 'ds-field__label', id: 'tk-pin-l' }, t('token.done.pin')), codeBlock(pinText, { labelledby: 'tk-pin-l', copy: () => data.ca_pin || pinText, copyLabel: t('token.done.copyPin') })),
    install
      ? h(
          'div',
          { class: 'ds-field' },
          h('p', { class: 'ds-field__label', id: 'tk-is-l' }, t('token.done.install')),
          codeBlock(install, { labelledby: 'tk-is-l', copy: blocked ? null : () => install, copyLabel: t('token.done.copyInstall'), id: 'tk-install' }),
        )
      : inlineAlert({ kind: 'warning', title: t('token.done.noInstall') }),
    oneLiner
      ? h(
          'div',
          { class: 'ds-field' },
          h('p', { class: 'ds-field__label', id: 'tk-one-l' }, t('token.done.commandDownload')),
          h('p', { class: 'ds-field__hint' }, t('token.done.commandDownloadHint')),
          codeBlock(oneLiner, { labelledby: 'tk-one-l', copy: blocked ? null : () => oneLiner, copyLabel: t('token.done.copyCommand'), id: 'tk-oneliner' }),
        )
      : null,
    install
      ? h(
          'div',
          { class: 'ds-field' },
          h('p', { class: 'ds-field__label', id: 'tk-cmd-l' }, t('token.done.command')),
          h('p', { class: 'ds-field__hint' }, oneLiner ? t('token.done.commandManual') : t('token.done.noDownloads')),
          codeBlock(cmd, { labelledby: 'tk-cmd-l', copy: blocked ? null : () => cmd, copyLabel: t('token.done.copyCommand') }),
        )
      : null,
    h('p', { class: 'muted' }, t('token.done.ports', { agent: data.agent_endpoint || '—', enroll: data.enroll_endpoint || '—' })),
  );
  const doneBtn = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('token.done.finish'));
  const d = openDialog({
    title: t('token.done.title', { name: data.name }),
    size: 'lg',
    body,
    actions: [doneBtn],
    initialFocus: doneBtn,
    onClose: () => {
      // 丟棄秘密（對話框的 DOM 已移除）
      token = null;
      install = null;
      secret.value = '';
      if (onCreated) onCreated(data);
    },
  });
  doneBtn.addEventListener('click', () => d.close());
  announce(t('token.done.title', { name: data.name }));
  if (data.auto_pause_cleared) toast({ kind: 'success', title: t('token.done.resumed') });
}

/** 停用 token（L1；已簽發的憑證不受影響） */
async function disableToken(tk, after) {
  const ok = await confirm({
    title: t('token.disable.title', { name: tk.name }),
    body: t('token.disable.body'),
    confirmText: t('token.disable.confirm'),
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/v1/enroll/tokens/${encodeURIComponent(tk.token_id)}/disable`, { method: 'POST', body: {} });
    toast({ kind: 'success', title: t('token.disable.done', { name: tk.name }) });
    if (after) after();
  } catch (e) {
    toast({ kind: 'error', title: t('token.disable.failed'), desc: e.message });
  }
}

// ---------------------------------------------------------------------------
// 清單

export function render(ctx) {
  if (ctx.params.id) return renderDetail(ctx);
  const root = ctx.root;
  const canWrite = session.can('enroll.tokens:write');
  let state = STATES.includes(ctx.query.get('state')) ? ctx.query.get('state') : 'active';
  let all = [];
  let disposed = false;
  const alerts = h('div', { class: 'k-stack--sm' });
  const tabsBox = h('div');
  const createBtn = canWrite ? button({ text: t('token.create.open'), variant: 'primary', iconName: 'plus', onClick: () => openCreateToken({ onCreated: () => load() }) }) : null;

  const table = createTable({
    caption: t('token.caption'),
    columns: [
      {
        key: 'name',
        label: t('token.col.name'),
        cls: 'k-cell-host',
        render: (r) => [h('a', { href: `/enrollment/tokens/${encodeURIComponent(r.token_id)}` }, r.name), h('span', { class: 'k-sub mono', title: t('token.hintTitle') }, r.hint)],
      },
      { key: 'state', label: t('token.col.state'), render: (r) => tokenStateLabel(r.state) },
      { key: 'group', label: t('token.col.group'), render: (r) => r.group || '—' },
      { key: 'approval', label: t('token.col.approval'), render: approvalText },
      { key: 'uses', label: t('token.col.uses'), num: true, render: (r) => `${fmt.num(r.used)}／${fmt.num(r.max_uses)}` },
      { key: 'expires', label: t('token.col.expires', { tz: fmt.tzLabel() }), render: (r) => fmt.dateTime(r.expires_at, { seconds: false }) },
      {
        key: 'progress',
        label: t('token.col.progress'),
        render: (r) => (r.progress ? progressBar(r.progress.online || 0, r.progress.used || 0, t('token.progressLabel', { name: r.name })) : '—'),
      },
      {
        key: 'more',
        label: h('span', { class: 'ds-visually-hidden' }, t('pki.col.actions')),
        render: (r) => {
          const b = h('button', { type: 'button', class: 'ds-button ds-button--icon ds-button--sm', 'aria-label': t('token.more', { name: r.name }), title: t('token.more', { name: r.name }), 'aria-haspopup': 'menu', 'aria-expanded': 'false' }, icon('more'));
          b.addEventListener('click', () => {
            const items = [{ text: t('token.menu.detail'), iconName: 'info', href: `/enrollment/tokens/${encodeURIComponent(r.token_id)}` }];
            if (r.progress && r.progress.pending) items.push({ text: t('token.menu.pending'), iconName: 'status-pending', href: `/enrollment/pending?token=${encodeURIComponent(r.token_id)}` });
            if (canWrite && r.state === 'active') items.push('separator', { text: t('token.menu.disable'), iconName: 'pause', danger: true, onSelect: () => disableToken(r, load) });
            openMenu(b, items, { label: t('token.more', { name: r.name }) });
          });
          return b;
        },
      },
    ],
    rowKey: (r) => r.token_id,
    rowHref: (r) => `/enrollment/tokens/${encodeURIComponent(r.token_id)}`,
  });

  mount(root, pageHead({ title: t('token.title'), actions: createBtn }), alerts, tabsBox, table.el);

  function renderTabs() {
    const count = (s) => (s === 'all' ? all.length : all.filter((x) => x.state === s).length);
    mount(
      tabsBox,
      linkTabs(
        t('token.tabs'),
        STATES.map((s) => ({ id: s, href: s === 'active' ? '/enrollment/tokens' : `/enrollment/tokens?state=${s}`, text: t(`token.tab.${s}`), count: count(s), current: s === state })),
      ),
    );
    for (const a of tabsBox.querySelectorAll('a')) {
      a.addEventListener('click', (ev) => {
        ev.preventDefault();
        state = a.dataset.tab;
        replaceQuery(state === 'active' ? {} : { state });
        renderTabs();
        renderRows();
        const cur = tabsBox.querySelector(`[data-tab="${state}"]`);
        if (cur) cur.focus();
      });
    }
  }

  function renderRows() {
    const rows = state === 'all' ? all : all.filter((x) => x.state === state);
    if (!rows.length) {
      if (!all.length) {
        table.setEmpty(
          emptyState({
            title: t('token.empty.title'),
            body: t('token.empty.body'),
            iconName: 'key',
            actions: canWrite ? [button({ text: t('token.empty.create'), variant: 'primary', onClick: () => openCreateToken({ onCreated: () => load() }) })] : null,
          }),
        );
      } else {
        table.setEmpty(emptyState({ title: t('token.empty.none', { state: t(`token.tab.${state}`) }), iconName: 'filter' }));
      }
      return;
    }
    table.setRows(rows, { total: rows.length, hasMore: false });
  }

  async function load() {
    table.setLoading();
    const modeP = fetchMode();
    try {
      all = await api('/api/v1/enroll/tokens?state=all&limit=1000');
    } catch (e) {
      if (disposed) return;
      mount(tabsBox);
      table.setError(e, load, t('token.failed'));
      return;
    }
    if (disposed) return;
    renderTabs();
    renderRows();
    const mode = await modeP;
    if (disposed) return;
    mount(alerts, modeAlert(mode), issuanceAlert(mode));
  }

  load();
  return {
    dispose() {
      disposed = true;
    },
  };
}

// ---------------------------------------------------------------------------
// 詳情（部署進度漏斗）

function renderDetail(ctx) {
  const root = ctx.root;
  const id = ctx.params.id;
  const canWrite = session.can('enroll.tokens:write');
  let disposed = false;
  const head = h('div');
  const body = h('div', { class: 'k-stack' });
  mount(root, head, body);
  mount(head, pageHead({ title: t('token.detail.loading'), crumbs: [{ text: t('token.title'), href: '/enrollment/tokens' }, { text: '…' }] }));
  mount(body, skeletonBlock(6));

  async function load() {
    let tk;
    try {
      tk = await api(`/api/v1/enroll/tokens/${encodeURIComponent(id)}`);
    } catch (e) {
      if (disposed) return;
      if (e.status === 404) {
        ctx.app.notFound();
        return;
      }
      mount(body, errorState(e, { title: t('token.detail.failed'), onRetry: load }));
      return;
    }
    if (disposed) return;
    ctx.setTitle(tk.name);
    const actions = canWrite && tk.state === 'active' ? button({ text: t('token.menu.disable'), variant: 'danger-secondary', iconName: 'pause', onClick: () => disableToken(tk, load) }) : null;
    const wasFocused = document.activeElement && document.activeElement.id === 'page-title';
    mount(head, pageHead({ title: tk.name, crumbs: [{ text: t('token.title'), href: '/enrollment/tokens' }, { text: tk.name }], meta: tokenStateLabel(tk.state), actions }));
    if (wasFocused) ctx.focusTitle();
    const p = tk.progress || {};
    const mode = await fetchMode();
    if (disposed) return;
    const attrs = dl([
      [t('token.d.hint'), h('code', { class: 'k-code-tag' }, tk.hint)],
      [t('token.d.state'), tokenStateLabel(tk.state)],
      [t('token.col.group'), tk.group],
      [t('token.f.tags'), (tk.tags || []).join('、')],
      [t('token.col.approval'), approvalText(tk)],
      [t('token.col.uses'), `${fmt.num(tk.used)}／${fmt.num(tk.max_uses)}`],
      [t('token.d.expires'), fmt.timeEl(tk.expires_at, { mode: 'absolute' })],
      [t('token.f.cidrs'), (tk.allowed_cidrs || []).length ? h('span', { class: 'mono' }, tk.allowed_cidrs.join('、')) : t('token.d.noLimit')],
      [t('token.f.agents'), (tk.allowed_agent_ids || []).length ? t('token.d.agents', { n: fmt.num(tk.allowed_agent_ids.length) }) : t('token.d.noLimit')],
      [t('token.f.certDays'), tk.cert_lifetime_days ? t('token.d.days', { d: tk.cert_lifetime_days }) : t('token.d.policy')],
      [t('token.d.created'), [fmt.timeEl(tk.created_at), tk.created_by ? h('span', { class: 'k-sub' }, tk.created_by) : null]],
      [t('token.d.lastUsed'), fmt.timeEl(tk.last_used_at)],
      tk.disabled_at ? [t('token.d.disabled'), [fmt.timeEl(tk.disabled_at), h('span', { class: 'k-sub' }, [tk.disabled_by, tk.disable_reason].filter(Boolean).join(' · '))]] : null,
    ]);
    const steps = [
      ['queued', p.queued, null],
      ['clock_rejected', p.clock_rejected, null],
      ['used', p.used, null],
      ['pending', p.pending, p.pending ? `/enrollment/pending?token=${encodeURIComponent(tk.token_id)}` : null],
      ['rejected', p.rejected, p.rejected ? `/enrollment/pending?view=closed&token=${encodeURIComponent(tk.token_id)}` : null],
      ['withdrawn', p.withdrawn, p.withdrawn ? `/enrollment/pending?view=closed&token=${encodeURIComponent(tk.token_id)}` : null],
      ['issued', p.issued, null],
      ['online', p.online, null],
      ['issued_not_online', p.issued_not_online, null],
      // 時鐘偏差只在 N＞0 時出現（S12）
    ].filter(([k, n]) => !(k === 'clock_rejected' && !n) && !(k === 'withdrawn' && !n));
    const funnel = h(
      'ol',
      { class: 'k-funnel', 'aria-label': t('token.d.funnel') },
      steps.map(([k, n, href]) =>
        h('li', { class: 'k-funnel__step', dataset: { step: k } }, h('span', { class: 'muted' }, t(`token.funnel.${k}`)), ' ', href ? h('a', { href, class: 'strong' }, fmt.num(n || 0)) : h('strong', null, fmt.num(n || 0))),
      ),
    );
    const notes = [];
    if (p.clock_rejected) notes.push(inlineAlert({ kind: 'info', title: t('token.clock.title', { n: fmt.num(p.clock_rejected) }), code: 'KRS-W8302', body: t('token.clock.body') }));
    if (p.issued_not_online) notes.push(inlineAlert({ kind: 'info', title: t('token.notOnline.title', { n: fmt.num(p.issued_not_online) }), body: t('token.notOnline.body') }));
    mount(
      body,
      modeAlert(mode),
      issuanceAlert(mode),
      card({ title: t('token.d.progress'), body: h('div', { class: 'k-stack--sm' }, funnel, notes) }),
      card({ title: t('token.d.attrs'), body: attrs }),
    );
  }

  load();
  return {
    dispose() {
      disposed = true;
    },
  };
}
