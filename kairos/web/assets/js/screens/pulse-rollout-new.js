// S26 建立推送（/pulse/rollouts/new；rollouts:write）（設計包 kairos/screens.md S26「建立推送」；API.md 第 17.3 節）。
//
// 單頁表單（設計包 §31.7 #6 預定 0.10 改為精靈，但設計包還沒有精靈的規格：0.10 先在同一頁加上「中間的批次」
// 與「排程與升級時段」兩個區塊）：
// 1. 目標版本（版本庫）；breaking 時要求勾選「我已閱讀破壞性變更與升級步驟」；目前的 Kairos 低於 min_kairos 時
//    就地錯誤 KRS-W8415，不能送出。
// 2. 對象：全部主機，或群組、標籤、主機（agent_id）的聯集，另可排除主機。下方即時顯示資格預覽
//    （POST /rollouts/preview，600 ms 合併；可升級台數以 aria-live 朗讀）：不會升級的依原因分列，每類可展開前 100 台。
// 3. 批次：先行批次（%、上限台數或指定主機）＋觀察時間（人工繼續）；中間的批次（0.10：可增減，大小為剩餘主機的 %、
//    觀察時間、失敗上限；「套用建議的四批範本」）；其餘的失敗上限；同時升級的上限、健康確認期限。最多 10 批。
// 3b. 排程與升級時段（0.10）：開始／結束時間（選填）、每週的時段（星期、開始、結束；跨午夜可）。時區可輸入或從
//    建議清單選擇 IANA 名稱（Asia/Taipei、Europe/Berlin…）或固定位移，留空為 Kairos 的 alerts.timezone（預覽回傳
//    default_timezone）；伺服器驗證名稱（400 schedule.timezone → 欄位錯誤）。預覽回報目前是否在時段內，並列出
//    接下來的時段實際的開始與結束（upcoming_windows：當地時間含位移、實際長度、日光節約時間的影響）。
// 4. 選項：允許無法自動回滾的主機、允許降版。
// 5. 送出：建立草稿（POST /rollouts）→ L2 DangerConfirmDialog 輸入預覽的 confirm_text（「升級 N 台」）→ submit；
//    降版時 L3（「降版 N 台」＋原因必填；0.9 不要求重新驗證密碼）。台數在送出時改變（400 confirmation_required）
//    時對話框內換成新的確認字串；主機已在另一個推送（409 rollout_overlap）時列出主機並提供「從對象排除這些主機」。
// 6. 0.10（ADR-0002 S1、S2；精靈之前先加在這個表單）：預覽與送出對話框顯示伺服器回傳的風險提醒（warnings[] 的
//    固定代碼）、目標版本的檔案遺失（artifact_problems，KRS-W8417；需要它的主機分類為 artifact_missing）；啟用
//    雙人覆核時寫出誰可以核准，沒有可核准者（approvers_available = 0）時警告但不阻擋。
// 7. 外部 URL 的版本（REMOTE.md 第 4.7.2 節）：預覽的 external_url（base_url、undeclared、allowlist）顯示為一則說明
//    ——白名單是空的主機（url_not_allowed）已列在「不會升級」，沒有回報與回報有白名單的台數另外註明，附 pulse.toml 的片段。
// Windows 不顯示。

import { h, icon, mount, uid, announce } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api } from '../core/api.js';
import * as fmt from '../core/format.js';
import { navigate } from '../core/router.js';
import { pageHead, card, emptyState, errorState, errorAlert, inlineAlert, field, input, skeletonBlock, setBusy } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import * as session from '../core/session.js';
import { isUnavailable, unavailableState, skipLabel, hostLink, warningsList } from './pulse-common.js';
import { refreshRolloutCount } from './pulse-boot.js';

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const CRUMB = { text: t('rm.ro.title'), href: '/pulse/rollouts' };
const DAYS = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'];
const MAX_WAVES = 10;
const MAX_WINDOWS = 16;

/** 各批的台數（「2 → 20 → 89 → 89」） */
function waveList(waves) {
  return (waves || []).map((n) => fmt.num(n || 0)).join(' → ');
}

/** 伺服器換算的當地時間（`2026-03-29T03:00:00+02:00`）→「2026-03-29 03:00（UTC+02:00）」；不經瀏覽器的時區 */
function localStamp(s) {
  const m = /^(\d{4}-\d{2}-\d{2})T(\d{2}:\d{2}):\d{2}([+-]\d{2}:\d{2})$/.exec(String(s || ''));
  return m ? t('rm.new.winStamp', { date: m[1], time: m[2], offset: m[3] }) : String(s || '');
}

/** 時段的實際長度（分鐘 →「3 小時」「1 小時 30 分鐘」「30 分鐘」） */
function durationText(minutes) {
  const m = Number(minutes) || 0;
  const [hh, mm] = [Math.floor(m / 60), m % 60];
  if (hh && mm) return t('rm.new.winDurHM', { h: fmt.num(hh), m: fmt.num(mm) });
  return hh ? t('rm.new.winDurH', { h: fmt.num(hh) }) : t('rm.new.winDurM', { m: fmt.num(mm) });
}

function cmpVersion(a, b) {
  const pa = String(a).split(/[.-]/).map((x) => Number.parseInt(x, 10) || 0);
  const pb = String(b).split(/[.-]/).map((x) => Number.parseInt(x, 10) || 0);
  for (let i = 0; i < 3; i += 1) if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) - (pb[i] || 0);
  return 0;
}

/**
 * 送出推送的確認對話框（L2：輸入「升級 N 台」；降版 L3：「降版 N 台」＋原因必填）。
 * @param {object} o  name、preview（含 confirm_text、confirm_level、waves、waves_spec、downgrade、no_rollback）、
 *                    submit(confirm, reason) → 推送、onExclude(agentIds)（主機已在另一個推送時）
 * @returns {Promise<object|null>}  送出後的推送；取消時 null
 */
export function submitDialog({ name, preview, submit, onExclude }) {
  return new Promise((resolve) => {
    let phrase = preview.confirm_text;
    let level = preview.confirm_level || 'L2';
    let done = false;
    const errBox = h('div');
    const tid = uid('confirm');
    const typed = h('input', { class: 'ds-input ds-input--mono', id: tid, type: 'text', name: 'confirm', autocomplete: 'off', spellcheck: 'false' });
    const labelText = h('span', null, t('rm.new.typeToConfirm', { phrase }));
    const matchState = h('span', { class: 'caption', 'aria-live': 'polite' }, t('rm.new.mismatch'));
    const rid = uid('reason');
    const reasonIn = h('textarea', { class: 'ds-textarea', id: rid, name: 'reason', rows: '2', maxlength: '500' });
    const reasonLabel = h('label', { class: 'ds-field__label', for: rid }, level === 'L3' ? t('rm.new.reasonRequired') : t('rm.new.reasonOptional'));
    const reasonErr = h('p', { class: 'ds-field__error', hidden: true }, icon('status-critical'), h('span', null, t('rm.new.reasonMissing')));
    const ok = h('button', { type: 'button', class: 'ds-button ds-button--danger', disabled: true }, phrase);
    const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
    const waves = preview.waves || [];
    const soak = preview.waves_spec && preview.waves_spec[0] ? preview.waves_spec[0].soak_minutes : 60;
    const leadText = waves.length > 2 ? t('rm.new.submitLeadN', { n: fmt.num(waves.length), list: waveList(waves), m: fmt.num(soak) }) : t('rm.new.submitLead', { a: fmt.num(waves[0] || 0), m: fmt.num(soak), b: fmt.num(waves[1] || 0) });
    const lead = h('p', { class: 'k-danger-lead' }, icon('status-critical'), h('span', null, leadText));
    const sch = preview.schedule || {};
    const later = sch.starts_at && new Date(sch.starts_at).getTime() > fmt.serverNow();
    const win = preview.window || {};
    const l3Note = h('p', { class: 'caption', hidden: level !== 'L3' }, t('rm.new.l3Note'));
    const sync = () => {
      const m = typed.value.trim() === phrase;
      ok.disabled = !m;
      matchState.textContent = m ? t('rm.new.match') : t('rm.new.mismatch');
    };
    typed.addEventListener('input', sync);
    const d = openDialog({
      title: t('rm.new.submitTitle', { name }),
      size: 'md',
      danger: true,
      body: h(
        'div',
        { class: 'k-stack--sm' },
        lead,
        preview.approval_required ? h('p', { class: 'strong' }, t('rm.new.submitLeadApproval')) : null,
        // 誰可以核准（三人邊界）與沒有可核准者的警告（不阻擋：之後可能新增帳號；ADR-0002 決策點 2c、2g）
        preview.approval_required ? h('p', { id: 'submit-who-approves' }, t('rm.new.whoApproves')) : null,
        preview.approval_required && preview.approvers_available === 0 ? h('div', { id: 'submit-no-approvers' }, inlineAlert({ kind: 'warning', title: t('rm.new.noApprovers'), body: t('rm.new.noApproversBody') })) : null,
        later ? h('p', null, t('rm.new.submitLeadLater', { t: fmt.dateTime(sch.starts_at, { seconds: false }) })) : null,
        sch.windows && sch.windows.length ? h('p', null, win.open === false && win.next ? t('rm.new.windowClosed', { t: fmt.dateTime(win.next, { seconds: false }) }) : t('rm.new.submitLeadWindow')) : null,
        preview.downgrade ? h('p', { class: 'strong' }, t('rm.new.submitLeadDown', { n: fmt.num(preview.downgrade) })) : null,
        preview.no_rollback ? h('p', null, t('rm.new.submitNoRollback', { n: fmt.num(preview.no_rollback) })) : null,
        // 風險提醒：與預覽、送審摘要、核准對話框同一組（伺服器回傳的固定代碼）。降版、無法自動回滾與沒有可核准者
        // 上面已經各有一句，這裡不重複
        warningsList((preview.warnings || []).filter((c) => !['downgrade', 'no_rollback', 'no_approvers'].includes(c))),
        errBox,
        h('div', { class: 'ds-field' }, reasonLabel, reasonIn, reasonErr),
        h('div', { class: 'ds-field' }, h('label', { class: 'ds-field__label', for: tid }, labelText), typed, matchState),
        l3Note,
      ),
      actions: [cancel, ok],
      initialFocus: typed,
      onClose: () => {
        if (!done) {
          done = true;
          resolve(null);
        }
      },
    });
    cancel.addEventListener('click', () => d.close());
    ok.addEventListener('click', async () => {
      if (typed.value.trim() !== phrase) return;
      const reason = reasonIn.value.trim();
      reasonErr.hidden = !(level === 'L3' && !reason);
      if (level === 'L3' && !reason) {
        reasonIn.setAttribute('aria-invalid', 'true');
        reasonIn.focus();
        return;
      }
      mount(errBox);
      setBusy(ok, true, t('common.working'));
      cancel.disabled = true;
      try {
        const r = await submit(phrase, reason || undefined);
        done = true;
        d.close();
        resolve(r);
      } catch (err) {
        setBusy(ok, false, t('common.working'));
        cancel.disabled = false;
        if (err.code === 'confirmation_required' && err.details && err.details.confirm_text && err.details.confirm_text !== phrase) {
          // 可升級的台數改變了：換成新的確認字串
          phrase = err.details.confirm_text;
          level = err.details.confirm_level || level;
          labelText.textContent = t('rm.new.typeToConfirm', { phrase });
          ok.textContent = phrase;
          reasonLabel.textContent = level === 'L3' ? t('rm.new.reasonRequired') : t('rm.new.reasonOptional');
          l3Note.hidden = level !== 'L3';
          typed.value = '';
          sync();
          const a = inlineAlert({ kind: 'warning', role: 'status', body: t('rm.new.changedPhrase') });
          mount(errBox, a);
          typed.focus();
          return;
        }
        if (err.code === 'rollout_overlap') {
          const ids = (err.details && err.details.agent_ids) || [];
          const hosts = (err.details && err.details.hosts) || [];
          const a = inlineAlert({
            kind: 'error',
            title: t('rm.new.overlap', { n: fmt.num((err.details && err.details.count) || ids.length) }),
            body: h('ul', { class: 'k-plain-list' }, hosts.slice(0, 100).map((x) => h('li', null, h('span', { class: 'mono' }, x.agent_id), ' → ', h('a', { href: `/pulse/rollouts/${encodeURIComponent(x.rollout_id)}` }, x.rollout_id)))),
            actions: onExclude
              ? [
                  h(
                    'button',
                    {
                      type: 'button',
                      class: 'ds-button ds-button--secondary ds-button--sm',
                      on: {
                        click: () => {
                          done = true;
                          d.close();
                          onExclude(ids);
                          resolve(null);
                        },
                      },
                    },
                    t('rm.new.overlapExclude'),
                  ),
                ]
              : null,
          });
          a.setAttribute('tabindex', '-1');
          mount(errBox, a);
          a.focus();
          return;
        }
        const a = errorAlert(err, { title: t('rm.new.submitFailed') });
        mount(errBox, a);
        a.focus();
      }
    });
  });
}

/** 預覽的呈現（資格分類、批次、重疊、不相容） */
export function previewView(p, { onExclude, version } = {}) {
  const skipped = Object.entries(p.skipped || {}).sort((a, b) => b[1] - a[1]);
  return h(
    'div',
    { class: 'k-stack--sm', id: 'rollout-preview' },
    p.incompatible ? inlineAlert({ kind: 'error', title: t('rm.rel.incompatible', { v: version || '', min: p.incompatible.min_kairos }), code: p.incompatible.code || 'KRS-W8415' }) : null,
    h('p', { class: 'k-preview-big' }, h('span', { class: 'strong' }, t('rm.new.eligible', { n: fmt.num(p.eligible || 0) })), ' · ', t('rm.new.matched', { n: fmt.num(p.matched || 0) })),
    p.downgrade ? h('p', null, t('rm.new.downgradeN', { n: fmt.num(p.downgrade) })) : null,
    p.no_rollback ? h('p', null, t('rm.new.noRollbackN', { n: fmt.num(p.no_rollback) })) : null,
    // 可升級的 deb／rpm 主機預期的回滾方式（rollback：archive、repository、degraded_hosts；ADR-0003 第 5.4 節）
    p.rollback && p.rollback.archive ? h('p', { dataset: { rollback: 'archive' } }, t('rm.new.rollbackArchiveN', { n: fmt.num(p.rollback.archive) })) : null,
    p.rollback && p.rollback.repository ? h('p', { dataset: { rollback: 'repository' } }, t('rm.new.rollbackRepositoryN', { n: fmt.num(p.rollback.repository) })) : null,
    p.rollback && p.rollback.degraded_hosts ? h('p', { dataset: { rollback: 'degraded' } }, t('rm.new.degradedHostsN', { n: fmt.num(p.rollback.degraded_hosts) })) : null,
    p.waves && p.waves.length ? h('p', { id: 'rollout-preview-waves' }, p.waves.length > 2 ? t('rm.new.wavesN', { n: fmt.num(p.waves.length), list: waveList(p.waves) }) : t('rm.new.waves', { a: fmt.num(p.waves[0] || 0), b: fmt.num(p.waves[1] || 0) })) : null,
    p.window && p.window.open === false
      ? inlineAlert({ kind: p.window.next ? 'info' : 'warning', role: 'status', body: p.window.next ? t('rm.new.windowClosed', { t: fmt.dateTime(p.window.next, { seconds: false }) }) : t('rm.new.windowNone') })
      : null,
    p.loses_remote_management && p.eligible ? inlineAlert({ kind: 'warning', body: t('rm.new.loses') }) : null,
    // 外部 URL 的版本（REMOTE.md 第 4.7.2 節）：宣告白名單是空的主機已分類為 url_not_allowed；沒有宣告與宣告
    // 有白名單的主機照舊算可升級，這裡註明台數（Kairos 不知道白名單的內容）
    p.external_url
      ? h(
          'div',
          { id: 'rollout-preview-external' },
          inlineAlert({
            kind: 'info',
            role: 'status',
            title: t('rm.new.extTitle'),
            body: [
              h('p', null, t('rm.new.extBody')),
              p.external_url.undeclared ? h('p', { dataset: { ext: 'undeclared' } }, t('rm.new.extUndeclared', { n: fmt.num(p.external_url.undeclared) })) : null,
              p.external_url.allowlist ? h('p', { dataset: { ext: 'allowlist' } }, t('rm.new.extAllowlist', { n: fmt.num(p.external_url.allowlist) })) : null,
              h('p', null, t('rm.new.extSnippet')),
              h('pre', { class: 'ds-code-block', tabindex: '0' }, `[upgrade]\nurl_allowlist = ["${p.external_url.base_url}"]`),
            ],
          }),
        )
      : null,
    // 目標版本有檔案遺失或大小不符的發佈檔（KRS-W8417，ADR-0002 F5）：需要它的主機已分類為 artifact_missing
    p.artifact_problems && p.artifact_problems.length
      ? h(
          'div',
          { id: 'rollout-preview-files' },
          inlineAlert({
            kind: 'warning',
            title: t('rm.new.artifactProblem', { v: version || p.artifact_problems[0].version, n: fmt.num(p.artifact_problems.length) }),
            code: 'KRS-W8417',
            body: [h('p', null, t('rm.new.artifactProblemBody')), h('ul', { class: 'k-plain-list' }, p.artifact_problems.map((f) => h('li', null, f.filename)))],
            actions: session.can('releases:read') ? [h('a', { class: 'ds-button ds-button--secondary ds-button--sm', href: `/pulse/releases/${encodeURIComponent(version || p.artifact_problems[0].version)}` }, t('rm.new.artifactProblemLink', { v: version || p.artifact_problems[0].version }))] : null,
          }),
        )
      : null,
    // 沒有可核准者（啟用雙人覆核時；決策點 2g）
    p.approval_required && p.approvers_available === 0 ? h('div', { id: 'rollout-preview-no-approvers' }, inlineAlert({ kind: 'warning', title: t('rm.new.noApprovers'), body: t('rm.new.noApproversBody') })) : null,
    // 風險提醒（伺服器回傳的固定代碼；檔案與可核准者上面已經各有一則）
    warningsList((p.warnings || []).filter((c) => !['artifact_problem', 'no_approvers'].includes(c))),
    p.overlap && p.overlap.count
      ? inlineAlert({
          kind: 'warning',
          title: t('rm.new.overlap', { n: fmt.num(p.overlap.count) }),
          actions: onExclude ? [h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => onExclude(p.overlap.hosts.map((x) => x.agent_id)) } }, t('rm.new.overlapExclude'))] : null,
        })
      : null,
    skipped.length
      ? [
          h('h3', { class: 'k-section-title' }, t('rm.new.skippedTitle')),
          h(
            'ul',
            { class: 'k-ro-skip' },
            skipped.map(([reason, n]) => {
              const samples = (p.samples && p.samples[reason]) || [];
              return h(
                'li',
                { dataset: { reason } },
                t('rm.new.skipLine', { label: skipLabel(reason), n: fmt.num(n) }),
                samples.length
                  ? h('details', { class: 'k-details' }, h('summary', null, t('rm.new.samples', { n: fmt.num(samples.length) })), h('ul', { class: 'k-plain-list' }, samples.map((s) => h('li', null, hostLink(s.agent_id, s.hostname)))))
                  : null,
              );
            }),
          ),
        ]
      : null,
  );
}

function numField(label, name, value, hint, min, max) {
  const el = input({ type: 'number', name, value: String(value), cls: 'ds-input--num' });
  el.setAttribute('min', String(min));
  el.setAttribute('max', String(max));
  el.setAttribute('step', '1');
  const f = field({ label, input: el, hint });
  const read = () => {
    const s = el.value.trim();
    const n = Number(s);
    const ok = s !== '' && Number.isInteger(n) && n >= min && n <= max;
    return ok ? n : null;
  };
  return { el, f, read, min, max };
}

function idList(text) {
  return text
    .split(/[\s,]+/)
    .map((x) => x.trim().toLowerCase())
    .filter(Boolean);
}

export function render(ctx) {
  const root = ctx.root;
  let disposed = false;
  let releases = [];
  let lastPreview = null;
  let initial = '';
  let submitted = false;
  let draftId = null;
  mount(root, pageHead({ title: t('rm.new.title'), crumbs: [CRUMB, { text: t('rm.new.title') }] }), skeletonBlock(8));

  // ---- 欄位 ----
  const nameIn = input({ name: 'name', maxlength: 128, autocomplete: 'off' });
  const fname = field({ label: t('rm.new.name'), input: nameIn, hint: t('rm.new.nameHint'), required: true });
  const verSel = h('select', { class: 'ds-select', name: 'target_version' });
  const fver = field({ label: t('rm.new.version'), input: verSel, hint: t('rm.new.versionHint'), required: true });
  const breakingBox = h('div');
  const ackId = uid('ack');
  const ack = h('input', { type: 'checkbox', id: ackId, name: 'breaking_ack', class: 'ds-check' });
  const ackErr = h('p', { class: 'ds-field__error', hidden: true }, icon('status-critical'), h('span', null, t('rm.new.breakingAckMissing')));

  const scope = uid('scope');
  const allR = h('input', { type: 'radio', name: scope, value: 'all', checked: true });
  const someR = h('input', { type: 'radio', name: scope, value: 'some' });
  const groupsIn = input({ name: 'groups', autocomplete: 'off' });
  const fgroups = field({ label: t('rm.new.groups'), input: groupsIn, hint: t('rm.new.groupsHint') });
  const tagsIn = input({ name: 'tags', autocomplete: 'off' });
  const ftags = field({ label: t('rm.new.tags'), input: tagsIn, hint: t('rm.new.tagsHint') });
  const agentsIn = h('textarea', { class: 'ds-textarea ds-input--mono', name: 'agent_ids', rows: '2', spellcheck: 'false' });
  const fagents = field({ label: t('rm.new.agents'), input: agentsIn, hint: t('rm.new.agentsHint') });
  const someBox = h('div', { hidden: true, class: 'k-stack--sm' }, fgroups, ftags, fagents);
  const excludeIn = h('textarea', { class: 'ds-textarea ds-input--mono', name: 'exclude_agent_ids', rows: '2', spellcheck: 'false' });
  const fexclude = field({ label: t('rm.new.exclude'), input: excludeIn, hint: t('rm.new.excludeHint') });
  const targetErr = h('p', { class: 'ds-field__error', hidden: true, id: uid('terr') }, icon('status-critical'), h('span'));

  const canaryPct = numField(t('rm.new.canaryPct'), 'canary_pct', 1, t('rm.new.canaryPctHint'), 1, 100);
  const canaryMax = numField(t('rm.new.canaryMax'), 'canary_max', 10, null, 1, 1000000);
  const canaryAgentsIn = h('textarea', { class: 'ds-textarea ds-input--mono', name: 'canary_agent_ids', rows: '2', spellcheck: 'false' });
  const fcanaryAgents = field({ label: t('rm.new.canaryAgents'), input: canaryAgentsIn, hint: t('rm.new.canaryAgentsHint') });
  const soak = numField(t('rm.new.soak'), 'soak_minutes', 60, t('rm.new.soakHint'), 0, 10080);
  const restFail = numField(t('rm.new.restFail'), 'rest_failure_pct', 5, t('rm.new.restFailHint'), 0, 100);
  const maxIn = numField(t('rm.new.maxInProgress'), 'max_in_progress', 50, t('rm.new.maxInProgressHint'), 1, 1000);
  const health = numField(t('rm.new.health'), 'health_timeout_secs', 600, t('rm.new.healthHint'), 120, 3600);
  const nums = [canaryPct, canaryMax, soak, restFail, maxIn, health];

  // ---- 中間的批次（0.10）：先行批次與「其餘」之間，可增減 ----
  const mids = [];
  const midList = h('div', { class: 'k-stack--sm', id: 'rollout-mid-waves' });
  const midAdd = h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => addMid({}, true) } }, icon('plus'), t('rm.new.midAdd'));
  const midTemplate = h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => applyTemplate() } }, t('rm.new.midTemplate'));
  function renderMids() {
    mount(
      midList,
      mids.map((m, i) =>
        h(
          'fieldset',
          { class: 'k-fieldset', dataset: { wave: String(i + 1) } },
          h('legend', null, t('rm.new.midLegend', { n: fmt.num(i + 2) })),
          h('div', { class: 'ds-form-row' }, m.pct.f, m.soak.f, m.fail.f),
          h(
            'button',
            {
              type: 'button',
              class: 'ds-button ds-button--tertiary ds-button--sm',
              on: {
                click: () => {
                  mids.splice(i, 1);
                  renderMids();
                  schedule();
                  midAdd.focus();
                },
              },
            },
            t('rm.new.midRemove', { n: fmt.num(i + 2) }),
          ),
        ),
      ),
    );
    midAdd.disabled = mids.length + 2 >= MAX_WAVES;
  }
  function addMid({ pct = 10, soakMin = 30, fail = 5 }, focus) {
    if (mids.length + 2 >= MAX_WAVES) return;
    const k = uid('w');
    const m = {
      pct: numField(t('rm.new.midPct'), `${k}_pct`, pct, null, 1, 100),
      soak: numField(t('rm.new.midSoak'), `${k}_soak`, soakMin, null, 0, 10080),
      fail: numField(t('rm.new.midFail'), `${k}_fail`, fail, null, 0, 100),
    };
    for (const n of [m.pct, m.soak, m.fail]) n.el.addEventListener('input', schedule);
    mids.push(m);
    renderMids();
    schedule();
    if (focus) m.pct.el.focus();
  }
  function applyTemplate() {
    // REMOTE.md 第 4.7.1 節的建議範本：1%（至多 10 台、觀察 60 分鐘）→ 10% → 50%（各觀察 30 分鐘、上限 5%）→ 其餘
    canaryPct.el.value = '1';
    canaryMax.el.value = '10';
    soak.el.value = '60';
    restFail.el.value = '5';
    mids.length = 0;
    addMid({ pct: 10 });
    addMid({ pct: 50 });
    announce(t('rm.new.midTemplateDone'));
  }

  // ---- 排程與升級時段（0.10） ----
  const startsIn = input({ type: 'datetime-local', name: 'starts_at' });
  const fstarts = field({ label: t('rm.new.startsAt'), input: startsIn, hint: t('rm.new.startsAtHint') });
  const endsIn = input({ type: 'datetime-local', name: 'ends_at' });
  const fends = field({ label: t('rm.new.endsAt'), input: endsIn, hint: t('rm.new.endsAtHint') });
  const wins = [];
  const winList = h('div', { class: 'k-stack--sm', id: 'rollout-windows' });
  const winAdd = h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => addWindow(true) } }, icon('plus'), t('rm.new.winAdd'));
  const tzNote = h('p', { class: 'caption', id: 'rollout-window-tz' }, t('rm.new.winTzUnknown'));
  // 時區：IANA 名稱（瀏覽器提供建議清單，伺服器以內建的時區資料庫驗證）或固定位移；留空＝Kairos 的預設
  const tzListId = uid('tzlist');
  const tzIn = input({ name: 'timezone', placeholder: 'Asia/Taipei', maxlength: '64', autocomplete: 'off' });
  tzIn.setAttribute('list', tzListId);
  const ftz = field({ label: t('rm.new.tz'), input: tzIn, hint: t('rm.new.tzHint') });
  // 伺服器拒絕過的時區（同一個值不必再送一次才知道不行）
  let tzBad = null;
  let tzNames = [];
  try {
    tzNames = typeof Intl.supportedValuesOf === 'function' ? Intl.supportedValuesOf('timeZone') : [];
  } catch {
    tzNames = [];
  }
  const tzList = h('datalist', { id: tzListId }, ['UTC', '+08:00', ...tzNames].map((v) => h('option', { value: v })));
  // 接下來的時段實際的開始與結束（伺服器換算，含當時的位移），讓操作者在送出前確認
  const winNext = h('div', { id: 'rollout-window-next', role: 'status' });
  tzIn.addEventListener('input', schedule);
  function renderUpcoming(p) {
    const up = (p && p.upcoming_windows) || [];
    const tz = p && p.timezone;
    if (!wins.length || !tz) {
      mount(winNext);
      tzNote.textContent = p && p.default_timezone ? t('rm.new.winTz', { tz: p.default_timezone }) : t('rm.new.winTzUnknown');
      return;
    }
    tzNote.textContent = tzIn.value.trim()
      ? t(tz.kind === 'iana' ? 'rm.new.winTzIana' : 'rm.new.winTzFixed', { tz: tz.name, offset: tz.offset || '', tzdb: tz.tzdb || '' })
      : t('rm.new.winTz', { tz: p.default_timezone || tz.name });
    mount(
      winNext,
      up.length
        ? [
            h('p', { class: 'strong' }, t('rm.new.winNext', { tz: tz.name })),
            h(
              'ul',
              { class: 'k-plain-list' },
              up.map((o) =>
                h(
                  'li',
                  { dataset: { dst: o.dst || '' } },
                  t('rm.new.winNextItem', { start: localStamp(o.start_local), end: localStamp(o.end_local), dur: durationText(o.minutes) }),
                  o.dst ? h('span', { class: 'caption' }, ` ${t(o.dst === 'gap' ? 'rm.new.winDstGap' : 'rm.new.winDstFold')}`) : null,
                ),
              ),
            ),
          ]
        : h('p', { class: 'caption' }, t('rm.new.windowNone')),
    );
  }
  function renderWindows() {
    mount(
      winList,
      wins.map((w, i) =>
        h(
          'fieldset',
          { class: 'k-fieldset', dataset: { window: String(i) } },
          h('legend', null, t('rm.new.winLegend', { n: fmt.num(i + 1) })),
          h('div', { class: 'ds-form-row' }, w.fstart, w.fend),
          h('fieldset', { class: 'k-fieldset' }, h('legend', null, t('rm.new.winDays')), h('div', { class: 'ds-radio-group' }, w.days.map((d, j) => h('label', { class: 'ds-checkbox' }, d, t(`rm.day.${DAYS[j]}`))))),
          h(
            'button',
            {
              type: 'button',
              class: 'ds-button ds-button--tertiary ds-button--sm',
              on: {
                click: () => {
                  wins.splice(i, 1);
                  renderWindows();
                  schedule();
                  winAdd.focus();
                },
              },
            },
            t('rm.new.winRemove', { n: fmt.num(i + 1) }),
          ),
        ),
      ),
    );
    winAdd.disabled = wins.length >= MAX_WINDOWS;
  }
  function addWindow(focus) {
    if (wins.length >= MAX_WINDOWS) return;
    const k = uid('win');
    const start = input({ type: 'time', name: `${k}_start`, value: '01:00' });
    const end = input({ type: 'time', name: `${k}_end`, value: '05:00' });
    const w = {
      start,
      end,
      fstart: field({ label: t('rm.new.winStart'), input: start }),
      fend: field({ label: t('rm.new.winEnd'), input: end, hint: t('rm.new.winHint') }),
      days: DAYS.map((d) => h('input', { type: 'checkbox', class: 'ds-check', name: `${k}_${d}`, value: d })),
    };
    for (const el of [start, end]) el.addEventListener('input', schedule);
    for (const el of w.days) el.addEventListener('change', schedule);
    wins.push(w);
    renderWindows();
    schedule();
    if (focus) start.focus();
  }
  for (const el of [startsIn, endsIn]) el.addEventListener('input', schedule);

  const noRbId = uid('norb');
  const noRb = h('input', { type: 'checkbox', id: noRbId, name: 'allow_no_rollback', class: 'ds-check', 'aria-describedby': `${noRbId}-hint` });
  const downId = uid('down');
  const down = h('input', { type: 'checkbox', id: downId, name: 'allow_downgrade', class: 'ds-check', 'aria-describedby': `${downId}-hint` });

  const live = h('p', { class: 'ds-visually-hidden', 'aria-live': 'polite' });
  const previewBox = h('div', null, h('p', { class: 'muted' }, t('rm.new.choosePreview')));
  const errBox = h('div');
  const submitBtn = h('button', { type: 'button', class: 'ds-button ds-button--primary', on: { click: () => submit() } }, t('rm.new.submit'));
  const submitWhy = h('p', { class: 'caption', hidden: true, id: uid('why') });

  function excludeHosts(ids) {
    const cur = new Set(idList(excludeIn.value));
    for (const id of ids) cur.add(id);
    excludeIn.value = [...cur].join('\n');
    schedule();
    excludeIn.focus();
  }

  function release() {
    return releases.find((r) => r.version === verSel.value) || null;
  }

  function syncScope() {
    someBox.hidden = !someR.checked;
  }
  allR.addEventListener('change', () => {
    syncScope();
    schedule();
  });
  someR.addEventListener('change', () => {
    syncScope();
    schedule();
  });

  function syncBreaking() {
    const r = release();
    if (r && r.manifest && r.manifest.breaking) {
      mount(
        breakingBox,
        inlineAlert({
          kind: 'warning',
          title: t('rm.new.breaking', { v: r.version }),
          body: [h('p', null, t('rm.new.breakingBody')), h('label', { class: 'ds-checkbox', for: ackId }, ack, t('rm.new.breakingAck')), ackErr],
        }),
      );
    } else {
      mount(breakingBox);
      ack.checked = false;
    }
  }
  verSel.addEventListener('change', () => {
    syncBreaking();
    schedule();
  });

  /** 表單 → 推送的內容；strict 時檢查欄位並顯示錯誤（回 null） */
  function spec({ strict = false } = {}) {
    let bad = null;
    const mark = (f, el, msg) => {
      if (strict) f.setError(msg || '');
      if (msg && !bad) bad = el;
    };
    const name = nameIn.value.trim();
    if (strict) mark(fname, nameIn, name ? '' : t('rm.new.nameMissing'));
    const selector = {};
    if (allR.checked) selector.all = true;
    else {
      const groups = groupsIn.value.split(',').map((x) => x.trim()).filter(Boolean);
      const tags = tagsIn.value.split(',').map((x) => x.trim()).filter(Boolean);
      const ids = idList(agentsIn.value);
      const badId = ids.find((x) => !UUID_RE.test(x));
      mark(fagents, agentsIn, badId ? t('rm.new.badId', { v: badId }) : '');
      if (groups.length) selector.groups = groups;
      if (tags.length) selector.tags = tags;
      if (ids.length) selector.agent_ids = ids;
      const none = !groups.length && !tags.length && !ids.length;
      if (strict) {
        targetErr.hidden = !none;
        targetErr.querySelector('span').textContent = none ? t('rm.new.targetMissing') : '';
      }
      if (none) {
        if (!bad) bad = groupsIn;
        if (!strict) return null;
      }
    }
    const ex = idList(excludeIn.value);
    const badEx = ex.find((x) => !UUID_RE.test(x));
    mark(fexclude, excludeIn, badEx ? t('rm.new.badId', { v: badEx }) : '');
    if (ex.length) selector.exclude_agent_ids = ex;
    const canaryIds = idList(canaryAgentsIn.value);
    const badC = canaryIds.find((x) => !UUID_RE.test(x));
    mark(fcanaryAgents, canaryAgentsIn, badC ? t('rm.new.badId', { v: badC }) : '');
    const v = {};
    const readNum = (n) => {
      const x = n.read();
      mark(n.f, n.el, x == null ? t('rm.new.numBad', { min: fmt.num(n.min), max: fmt.num(n.max) }) : '');
      return x;
    };
    for (const n of nums) v[n.el.name] = readNum(n);
    const midWaves = mids.map((m) => {
      const [pct, soakMin, fail] = [readNum(m.pct), readNum(m.soak), readNum(m.fail)];
      return { size: `${pct}%`, soak_minutes: soakMin, max_failure_ratio: (fail ?? 0) / 100 };
    });
    // 排程：開始／結束時間以瀏覽器的本地時間輸入，送出為 RFC 3339（UTC）
    const sch = {};
    const readTime = (f, el) => {
      if (!el.value) {
        mark(f, el, '');
        return null;
      }
      const d = new Date(el.value);
      const bad = Number.isNaN(d.getTime());
      mark(f, el, bad ? t('rm.new.timeBad') : '');
      return bad ? null : d;
    };
    const st = readTime(fstarts, startsIn);
    const en = readTime(fends, endsIn);
    if (st) sch.starts_at = st.toISOString();
    if (en) {
      if (st && en <= st) mark(fends, endsIn, t('rm.new.endBeforeStart'));
      sch.ends_at = en.toISOString();
    }
    const windows = wins.map((w) => {
      const [s, e] = [w.start.value.slice(0, 5), w.end.value.slice(0, 5)];
      const bad = !s || !e || s === e;
      mark(w.fend, w.end, bad ? t('rm.new.winBad') : '');
      return { days: DAYS.filter((_, j) => w.days[j].checked), start: s, end: e };
    });
    if (windows.length) sch.windows = windows;
    // 時區只在有時段時才有意義；留空時由 Kairos 填入預設
    if (windows.length && tzIn.value.trim()) sch.timezone = tzIn.value.trim();
    if (tzBad && tzBad === tzIn.value.trim() && windows.length) mark(ftz, tzIn, t('rm.new.tzBad'));
    else if (strict) ftz.setError('');
    if (strict && release() && release().manifest && release().manifest.breaking && !ack.checked) {
      ackErr.hidden = false;
      if (!bad) bad = ack;
    } else ackErr.hidden = true;
    if (bad) {
      if (strict) bad.focus();
      return null;
    }
    return {
      name: name || 'preview',
      target_version: verSel.value,
      selector,
      waves: [
        { size: `${v.canary_pct}%`, max_hosts: v.canary_max, soak_minutes: v.soak_minutes, max_failure_ratio: 0, manual_continue: true },
        ...midWaves,
        { size: 'rest', max_failure_ratio: v.rest_failure_pct / 100 },
      ],
      ...(Object.keys(sch).length ? { schedule: sch } : {}),
      options: {
        max_in_progress: v.max_in_progress,
        health_timeout_secs: v.health_timeout_secs,
        allow_downgrade: down.checked,
        allow_no_rollback: noRb.checked,
        canary_agent_ids: canaryIds,
      },
    };
  }

  function snapshot() {
    return JSON.stringify([nameIn.value, verSel.value, allR.checked, groupsIn.value, tagsIn.value, agentsIn.value, excludeIn.value, canaryAgentsIn.value, nums.map((n) => n.el.value), noRb.checked, down.checked, ack.checked, mids.map((m) => [m.pct.el.value, m.soak.el.value, m.fail.el.value]), startsIn.value, endsIn.value, wins.map((w) => [w.start.value, w.end.value, w.days.map((d) => d.checked)]), tzIn.value]);
  }

  let timer = null;
  let seq = 0;
  function schedule() {
    clearTimeout(timer);
    timer = setTimeout(runPreview, 600);
  }
  async function runPreview() {
    const s = spec();
    if (!s || !s.target_version) return;
    const my = ++seq;
    live.textContent = t('rm.new.previewing');
    try {
      const p = await api('/api/v1/rollouts/preview', { method: 'POST', body: s });
      if (my !== seq || disposed) return;
      lastPreview = p;
      mount(previewBox, previewView(p, { onExclude: excludeHosts, version: verSel.value }));
      tzBad = null;
      ftz.setError('');
      renderUpcoming(p);
      live.textContent = `${t('rm.new.eligible', { n: fmt.num(p.eligible || 0) })}，${t('rm.new.matched', { n: fmt.num(p.matched || 0) })}`;
      syncSubmit();
    } catch (err) {
      if (my !== seq || disposed) return;
      lastPreview = null;
      // 不認得的時區：錯誤顯示在時區欄位（伺服器以內建的時區資料庫驗證）
      if (err.code === 'invalid_parameter' && err.details && err.details.parameter === 'schedule.timezone') {
        tzBad = tzIn.value.trim();
        ftz.setError(t('rm.new.tzBad'));
        mount(winNext);
        mount(previewBox, h('p', { class: 'muted' }, t('rm.new.tzBadPreview')));
        live.textContent = t('rm.new.tzBad');
        syncSubmit();
        return;
      }
      mount(previewBox, errorState(err, { title: t('rm.new.previewFailed'), onRetry: runPreview }));
      live.textContent = t('rm.new.previewFailed');
      syncSubmit();
    }
  }

  function syncSubmit() {
    let why = null;
    if (lastPreview && lastPreview.incompatible) why = t('rm.rel.incompatible', { v: verSel.value, min: lastPreview.incompatible.min_kairos });
    else if (lastPreview && !lastPreview.eligible) why = t('rm.new.noEligible');
    if (why) {
      submitBtn.setAttribute('aria-disabled', 'true');
      submitBtn.setAttribute('aria-describedby', submitWhy.id);
      submitWhy.textContent = why;
      submitWhy.hidden = false;
    } else {
      submitBtn.removeAttribute('aria-disabled');
      submitBtn.removeAttribute('aria-describedby');
      submitWhy.hidden = true;
    }
  }

  async function submit() {
    mount(errBox);
    if (submitBtn.getAttribute('aria-disabled') === 'true') {
      announce(submitWhy.textContent);
      return;
    }
    const s = spec({ strict: true });
    if (!s) return;
    // 送出前以目前的內容重新試算（確認字串以最新的預覽為準）
    let p;
    try {
      p = await api('/api/v1/rollouts/preview', { method: 'POST', body: s });
      lastPreview = p;
      mount(previewBox, previewView(p, { onExclude: excludeHosts, version: verSel.value }));
    } catch (err) {
      const a = errorAlert(err, { title: t('rm.new.previewFailed') });
      mount(errBox, a);
      a.focus();
      return;
    }
    syncSubmit();
    if (p.incompatible || !p.eligible) {
      announce(submitWhy.textContent);
      return;
    }
    const r = await submitDialog({
      name: s.name,
      preview: p,
      onExclude: excludeHosts,
      submit: async (confirm, reason) => {
        if (!draftId) {
          const created = await api('/api/v1/rollouts', { method: 'POST', body: s });
          draftId = created.id;
        } else {
          await api(`/api/v1/rollouts/${encodeURIComponent(draftId)}`, { method: 'PUT', body: s });
        }
        return api(`/api/v1/rollouts/${encodeURIComponent(draftId)}/submit`, { method: 'POST', body: { confirm, reason } });
      },
    });
    if (!r) return;
    submitted = true;
    toast({ kind: 'success', title: r.state === 'pending_approval' ? t('rm.new.submittedApproval', { name: s.name }) : r.state === 'scheduled' ? t('rm.new.submittedLater', { name: s.name, t: fmt.dateTime(r.starts_at, { seconds: false }) }) : t('rm.new.submitted', { name: s.name }) });
    refreshRolloutCount();
    navigate(`/pulse/rollouts/${encodeURIComponent(r.id || draftId)}`, { replace: true });
  }

  async function load() {
    let facets = null;
    try {
      [releases, facets] = await Promise.all([api('/api/v1/releases'), api('/api/v1/hosts/facets').catch(() => null)]);
    } catch (err) {
      if (disposed) return;
      if (isUnavailable(err)) {
        mount(root, pageHead({ title: t('rm.new.title'), crumbs: [CRUMB, { text: t('rm.new.title') }] }), unavailableState(err, 'upgrade'));
        return;
      }
      mount(root, pageHead({ title: t('rm.new.title'), crumbs: [CRUMB, { text: t('rm.new.title') }] }), errorState(err, { title: t('rm.rel.loadFailed'), onRetry: load }));
      return;
    }
    if (disposed) return;
    releases = (releases || []).slice().sort((a, b) => cmpVersion(b.version, a.version));
    if (!releases.length) {
      mount(
        root,
        pageHead({ title: t('rm.new.title'), crumbs: [CRUMB, { text: t('rm.new.title') }] }),
        emptyState({ title: t('rm.new.noReleases'), body: t('rm.new.noReleasesBody'), iconName: 'agent', actions: [h('a', { class: 'ds-button ds-button--primary', href: '/pulse/releases' }, t('rm.ro.goReleases'))] }),
      );
      ctx.focusTitle();
      return;
    }
    mount(verSel, releases.map((r) => h('option', { value: r.version }, r.version)));
    // 群組與標籤的建議值（datalist）
    const dl = (id, src) => h('datalist', { id }, ((src && src.values) || []).map((v) => h('option', { value: v.value })));
    const gid = uid('groups');
    const tgid = uid('tags');
    groupsIn.setAttribute('list', gid);
    tagsIn.setAttribute('list', tgid);
    for (const el of [nameIn, groupsIn, tagsIn, agentsIn, excludeIn, canaryAgentsIn, ...nums.map((n) => n.el)]) el.addEventListener('input', schedule);
    for (const el of [noRb, down]) el.addEventListener('change', schedule);
    mount(
      root,
      pageHead({ title: t('rm.new.title'), crumbs: [CRUMB, { text: t('rm.new.title') }] }),
      h('p', { class: 'caption' }, t('rm.ro.mvp')),
      errBox,
      h(
        'div',
        { class: 'k-grid k-grid--3-1' },
        h(
          'div',
          { class: 'k-ro-form' },
          card({ title: t('rm.new.version'), body: [fname, fver, breakingBox] }),
          card({
            title: t('rm.new.target'),
            body: [
              h('fieldset', { class: 'k-fieldset' }, h('legend', null, t('rm.new.target')), h('div', { class: 'ds-radio-group' }, h('label', { class: 'ds-radio' }, allR, t('rm.new.all')), h('label', { class: 'ds-radio' }, someR, t('rm.new.some')))),
              targetErr,
              someBox,
              fexclude,
              dl(gid, facets && facets.group),
              dl(tgid, facets && facets.tag),
            ],
          }),
          card({
            title: t('rm.new.batches'),
            body: [
              h('p', { class: 'caption' }, t('rm.new.canaryNote')),
              h('div', { class: 'ds-form-row' }, canaryPct.f, canaryMax.f, soak.f),
              fcanaryAgents,
              h('h3', { class: 'k-section-title' }, t('rm.new.midTitle')),
              h('p', { class: 'caption' }, t('rm.new.midNote', { max: fmt.num(MAX_WAVES) })),
              midList,
              h('div', { class: 'ds-button-row' }, midAdd, midTemplate),
              h('div', { class: 'ds-form-row' }, restFail.f, maxIn.f, health.f),
            ],
          }),
          card({
            title: t('rm.new.schedule'),
            body: [
              h('p', { class: 'caption' }, t('rm.new.scheduleNote')),
              h('div', { class: 'ds-form-row' }, fstarts, fends),
              h('h3', { class: 'k-section-title' }, t('rm.new.winTitle')),
              ftz,
              tzList,
              tzNote,
              winList,
              h('div', { class: 'ds-button-row' }, winAdd),
              winNext,
            ],
          }),
          card({
            title: t('rm.new.options'),
            body: [
              h('div', { class: 'ds-field' }, h('label', { class: 'ds-checkbox', for: noRbId }, noRb, t('rm.new.allowNoRollback')), h('p', { class: 'ds-field__hint', id: `${noRbId}-hint` }, t('rm.new.allowNoRollbackHint'))),
              h('div', { class: 'ds-field' }, h('label', { class: 'ds-checkbox', for: downId }, down, t('rm.new.allowDowngrade')), h('p', { class: 'ds-field__hint', id: `${downId}-hint` }, t('rm.new.allowDowngradeHint'))),
            ],
          }),
          h('div', { class: 'ds-button-row ds-button-row--end' }, h('a', { class: 'ds-button ds-button--secondary', href: '/pulse/rollouts' }, t('common.cancel')), submitBtn),
          submitWhy,
        ),
        card({ title: t('rm.new.preview'), body: [live, previewBox] }),
      ),
    );
    syncBreaking();
    initial = snapshot();
    ctx.focusTitle();
    schedule();
  }

  load();
  return {
    dispose() {
      disposed = true;
      clearTimeout(timer);
    },
    isDirty: () => !submitted && initial !== '' && snapshot() !== initial,
  };
}
