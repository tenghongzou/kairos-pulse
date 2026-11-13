// S25 Pulse 版本庫：清單（/pulse/releases）與版本詳情（/pulse/releases/:version）
// （設計包 kairos/screens.md S25；API.md 第 17.2 節）。
//
// - 0.9：Kairos 代管（上傳檔案）、Linux 的 tar／deb／rpm；外部 URL 在 0.10（不顯示）。Windows 的發佈檔可以收進版本庫，
//   畫面標示「0.10 起可推送」。
// - 清單：版本、簽署金鑰（官方／客戶金鑰）、平台與格式（未上傳的檔案註明）、執行中、回滾目標、推送中、匯入時間；
//   Kairos 沒有設定信任錨（meta.allowed_signers_configured = false）時警告；部署中的 0.9 以上版本不在版本庫時建議匯入
//   （依主機清單的 agent 版本）。
// - 匯入（releases:write；對話框）：① 選擇 SHA256SUMS、SHA256SUMS.sig、發佈清單與 pulse-* 發佈檔（可一次選擇整個目錄）
//   → POST /releases 驗證簽章（顯示版本、commit、簽署金鑰）；② 逐一 PUT 發佈檔（每個檔案一列 ProgressBar、「✓ 雜湊相符」；
//   不在 SHA256SUMS 中的檔案就地錯誤）；③ 完成摘要。驗證失敗為 KRS-E8413 的三段式，對話框不關閉；同一版本已存在時
//   只上傳尚未上傳的檔案。上傳中離開頁面時詢問（isDirty → 全域的 beforeunload 與站內換頁的詢問）。
// - 詳情：發佈資訊（commit、簽署金鑰、min_kairos〔目前的 Kairos 不相容時 KRS-W8415〕、min_upgrade_from、breaking）、
//   發佈檔（檔名、平台、格式、大小、SHA-256 前 16 碼可複製、已上傳／待上傳）、依安裝方式與平台的主機數。
// - 刪除：L2（輸入版本號）；推送使用中或作為回滾目標時 aria-disabled 並說明（409 release_in_use）。
// - 版本庫的檔案遺失（0.10；artifacts[].file_problem，KRS-W8417）：清單與詳情的頁首是三段式（原因、處理方式、代碼、
//   檔案清單），「重新上傳 <版本>…」直接開啟該版本的上傳（ADR-0002 F4）；檔案列另有標記。

import { h, icon, mount, uid } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api, upload } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { navigate } from '../core/router.js';
import { pageHead, card, dl, emptyState, errorState, errorAlert, inlineAlert, skeletonBlock, copyButton, statusBadge, neutralBadge, setBusy } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { toast } from '../ui/toast.js';
import { confirmL2, disabledButton } from './admin-common.js';
import { isUnavailable, unavailableState } from './pulse-common.js';

const OFFICIAL = 'release@kairos-pulse';
const CRUMB = { text: t('rm.rel.title'), href: '/pulse/releases' };
let uploading = 0;

function signerCell(r) {
  if (!r.signer_fingerprint) return h('span', { class: 'muted' }, t('rm.rel.unverified'));
  const fp = r.signer_fingerprint.length > 18 ? `${r.signer_fingerprint.slice(0, 14)}…${r.signer_fingerprint.slice(-2)}` : r.signer_fingerprint;
  return [h('span', { class: 'mono', title: r.signer_fingerprint }, fp), ' ', r.signer_principal === OFFICIAL ? t('rm.rel.official') : t('rm.rel.customer', { p: r.signer_principal || '—' })];
}

/** 平台與格式：「linux x86_64：tar deb rpm」；未上傳的檔案註明 */
function platformsText(arts) {
  const by = new Map();
  for (const a of arts || []) {
    const k = `${a.os} ${a.arch}`;
    if (!by.has(k)) by.set(k, []);
    by.get(k).push(a.file_problem ? `${a.format}${t('rm.rel.fileProblemShort')}` : a.stored || a.registered ? a.format : `${a.format}${t('rm.rel.notStored')}`);
  }
  if (!by.size) return '—';
  return h('ul', { class: 'k-plain-list' }, [...by].map(([k, fmts]) => h('li', null, `${k}：${fmts.join(' ')}`)));
}

/**
 * 頁首的三段式（KRS-W8417，ADR-0002 F4）：版本庫記錄已存入、但檔案不存在或大小不符的發佈檔（artifacts[].file_problem）。
 * 標題、原因、處理方式、代碼，另列出每個檔案；有 releases:write 時每個版本一個「重新上傳…」直接開啟該版本的上傳。
 * 沒有這類檔案時 null。
 */
function missingAlert(releases, onUpload) {
  const files = [];
  for (const r of releases || []) for (const a of r.artifacts || []) if (a.file_problem) files.push({ r, a });
  if (!files.length) return null;
  const versions = [...new Set(files.map((x) => x.r.version))];
  const first = files[0];
  return h(
    'div',
    { id: 'release-files-missing' },
    inlineAlert({
      kind: 'warning',
      title: files.length > 1 ? t('rm.rel.missingTitleMore', { v: first.r.version, f: first.a.filename, n: fmt.num(files.length) }) : t('rm.rel.missingTitle', { v: first.r.version, f: first.a.filename }),
      code: 'KRS-W8417',
      body: [
        h(
          'dl',
          { class: 'k-3part' },
          h('dt', null, t('err.label.reason')),
          h('dd', null, t('rm.rel.missingReason')),
          h('dt', null, t('err.label.fix')),
          h('dd', null, h('ol', { class: 'k-fix-steps' }, h('li', null, t('rm.rel.missingFix1')), h('li', null, t('rm.rel.missingFix2')))),
        ),
        h('p', { class: 'strong' }, t('rm.rel.missingList')),
        h(
          'ul',
          { class: 'k-plain-list' },
          files.map(({ r, a }) => h('li', { dataset: { file: a.filename } }, t('rm.rel.missingItem', { v: r.version, f: a.filename, p: t(a.file_problem === 'size_mismatch' ? 'rm.rel.problem.size_mismatch' : 'rm.rel.problem.missing') }))),
        ),
      ],
      actions: onUpload
        ? versions.map((v) => h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => onUpload(files.find((x) => x.r.version === v).r) } }, icon('upload'), t('rm.rel.missingReupload', { v })))
        : null,
    }),
  );
}

function cmpVersion(a, b) {
  const pa = String(a).split(/[.-]/).map((x) => Number.parseInt(x, 10) || 0);
  const pb = String(b).split(/[.-]/).map((x) => Number.parseInt(x, 10) || 0);
  for (let i = 0; i < 3; i += 1) if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) - (pb[i] || 0);
  return 0;
}

export function render(ctx) {
  if (ctx.params.version) return renderDetail(ctx);
  return renderList(ctx);
}

// ---------------------------------------------------------------------------
// 清單

function renderList(ctx) {
  const root = ctx.root;
  const canWrite = session.can('releases:write');
  let disposed = false;
  const notes = h('div', { class: 'k-stack--sm' });
  const body = h('div', null, skeletonBlock(6));
  const importBtn = canWrite ? h('button', { type: 'button', class: 'ds-button ds-button--primary', on: { click: () => importDialog({ onDone: load }) } }, icon('upload'), t('rm.rel.import')) : null;
  mount(root, pageHead({ title: t('rm.rel.title'), actions: importBtn }), notes, body);

  async function load() {
    let res;
    let facets = null;
    try {
      [res, facets] = await Promise.all([api('/api/v1/releases', { raw: true }), api('/api/v1/hosts/facets').catch(() => null)]);
    } catch (err) {
      if (disposed) return;
      if (isUnavailable(err)) {
        if (importBtn) importBtn.hidden = true;
        mount(notes);
        mount(body, unavailableState(err, 'upgrade'));
        return;
      }
      mount(body, errorState(err, { title: t('rm.rel.loadFailed'), onRetry: load }));
      return;
    }
    if (disposed) return;
    const list = res.data || [];
    const meta = res.meta || {};
    const n = [];
    // 版本庫的檔案遺失（KRS-W8417）：頁首的三段式與「重新上傳…」
    const missing = missingAlert(list, canWrite ? (r) => importDialog({ release: r, onDone: load }) : null);
    if (missing) n.push(missing);
    if (meta.allowed_signers_configured === false) n.push(inlineAlert({ kind: 'warning', title: t('rm.rel.noTrust'), body: t('rm.rel.noTrustBody') }));
    // 部署中、0.9.0 以上、不在版本庫的版本：建議匯入（自動回滾需要它的檔案）
    const have = new Set(list.map((r) => r.version));
    const deployed = ((facets && facets.version && facets.version.values) || []).filter((v) => cmpVersion(v.value, '0.9.0') >= 0 && !have.has(v.value));
    for (const v of deployed.slice(0, 3)) {
      n.push(
        inlineAlert({
          kind: 'warning',
          body: t('rm.rel.suggest', { v: v.value, n: fmt.num(v.count) }),
          actions: canWrite ? [h('button', { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', on: { click: () => importDialog({ onDone: load }) } }, t('rm.rel.import'))] : null,
        }),
      );
    }
    mount(notes, ...n);
    if (!list.length) {
      mount(
        body,
        emptyState({
          title: t('rm.rel.empty'),
          body: t('rm.rel.emptyBody'),
          iconName: 'agent',
          actions: canWrite ? [h('button', { type: 'button', class: 'ds-button ds-button--primary', on: { click: () => importDialog({ onDone: load }) } }, icon('upload'), t('rm.rel.import'))] : null,
        }),
      );
      return;
    }
    const anyWindows = list.some((r) => (r.artifacts || []).some((a) => a.os === 'windows'));
    mount(
      body,
      card({
        body: [
          h(
            'div',
            { class: 'k-scroll-x' },
            h(
              'table',
              { class: 'k-mini-table', id: 'releases' },
              h('caption', { class: 'ds-visually-hidden' }, t('rm.rel.caption')),
              h(
                'thead',
                null,
                h(
                  'tr',
                  null,
                  h('th', { scope: 'col' }, t('rm.rel.col.version')),
                  h('th', { scope: 'col' }, t('rm.rel.col.signer')),
                  h('th', { scope: 'col' }, t('rm.rel.col.platforms')),
                  h('th', { scope: 'col', class: 'num' }, t('rm.rel.col.running')),
                  h('th', { scope: 'col', class: 'num' }, t('rm.rel.col.rollback')),
                  h('th', { scope: 'col', class: 'num' }, t('rm.rel.col.rollouts')),
                  h('th', { scope: 'col' }, t('rm.rel.col.imported', { tz: fmt.tzLabel() })),
                ),
              ),
              h(
                'tbody',
                null,
                list.map((r) =>
                  h(
                    'tr',
                    { dataset: { version: r.version } },
                    h('td', null, h('a', { href: `/pulse/releases/${encodeURIComponent(r.version)}` }, r.version), r.manifest && r.manifest.breaking ? h('span', { class: 'k-sub' }, t('rm.rel.breaking')) : null),
                    h('td', null, signerCell(r)),
                    h('td', null, platformsText(r.artifacts)),
                    h('td', { class: 'num' }, fmt.num(r.hosts_on_version || 0)),
                    h('td', { class: 'num' }, fmt.num(r.rollback_target_for || 0)),
                    h('td', { class: 'num' }, fmt.num(r.in_use_by_rollouts || 0)),
                    h('td', null, fmt.dateTime(r.imported_at, { seconds: false }), r.imported_by ? h('span', { class: 'k-host-id' }, r.imported_by) : null),
                  ),
                ),
              ),
            ),
          ),
          anyWindows ? h('p', { class: 'caption' }, t('rm.rel.windowsLater')) : null,
          h('p', { class: 'caption' }, t('rm.rel.cli')),
        ],
      }),
    );
  }
  load();
  return {
    dispose() {
      disposed = true;
    },
    isDirty: () => uploading > 0,
  };
}

// ---------------------------------------------------------------------------
// 詳情

function renderDetail(ctx) {
  const root = ctx.root;
  const version = String(ctx.params.version);
  const canWrite = session.can('releases:write');
  let disposed = false;
  mount(root, pageHead({ title: t('rm.rel.detailTitle', { v: version }), crumbs: [CRUMB, { text: version }] }), skeletonBlock(8));
  ctx.setTitle(t('rm.rel.detailTitle', { v: version }));

  async function load() {
    let r;
    try {
      r = await api(`/api/v1/releases/${encodeURIComponent(version)}`);
    } catch (err) {
      if (disposed) return;
      if (isUnavailable(err)) {
        mount(root, pageHead({ title: t('rm.rel.title'), crumbs: [CRUMB, { text: version }] }), unavailableState(err, 'upgrade'));
        return;
      }
      if (err.code === 'release_not_found') {
        mount(
          root,
          pageHead({ title: t('rm.rel.notFound'), crumbs: [CRUMB, { text: version }] }),
          emptyState({ title: t('rm.rel.notFound'), body: t('rm.rel.notFoundBody', { v: version }), iconName: 'agent', actions: [h('a', { class: 'ds-button ds-button--secondary', href: '/pulse/releases' }, icon('chevron-left'), t('rm.rel.back'))] }),
        );
        ctx.focusTitle();
        return;
      }
      mount(root, pageHead({ title: t('rm.rel.detailTitle', { v: version }), crumbs: [CRUMB, { text: version }] }), errorState(err, { title: t('rm.rel.loadFailed'), onRetry: load }));
      return;
    }
    if (disposed) return;
    const m = r.manifest;
    const actions = [];
    if (canWrite) {
      if (r.artifacts && r.artifacts.some((a) => (!a.stored && !a.registered) || a.file_problem)) {
        actions.push(h('button', { type: 'button', class: 'ds-button ds-button--secondary', on: { click: () => importDialog({ release: r, onDone: load }) } }, icon('upload'), t('rm.rel.upload')));
      }
      const why = r.in_use_by_rollouts ? t('rm.rel.inUseRollouts', { n: fmt.num(r.in_use_by_rollouts) }) : r.rollback_target_for ? t('rm.rel.inUseRollback', { n: fmt.num(r.rollback_target_for) }) : null;
      if (why) actions.push(...disabledButton(t('rm.rel.delete'), why, { variant: 'danger-secondary' }));
      else actions.push(h('button', { type: 'button', class: 'ds-button ds-button--danger-secondary', on: { click: () => del(r) } }, icon('trash'), t('rm.rel.delete')));
    }
    const incompatible = m && m.kairos_compatible === false;
    const byPlatform = Object.entries(r.hosts_by_platform || {});
    mount(
      root,
      pageHead({ title: t('rm.rel.detailTitle', { v: r.version }), crumbs: [CRUMB, { text: r.version }], actions: actions.length ? actions : null }),
      incompatible ? inlineAlert({ kind: 'error', title: t('rm.rel.incompatible', { v: r.version, min: m.min_kairos }), code: 'KRS-W8415' }) : null,
      missingAlert([r], canWrite ? (x) => importDialog({ release: x, onDone: load }) : null),
      m ? null : inlineAlert({ kind: 'info', body: t('rm.rel.noManifest') }),
      r.source === 'external' ? inlineAlert({ kind: 'info', body: t('rm.rel.externalNote') }) : null,
      h(
        'div',
        { class: 'k-grid k-grid--1-1' },
        card({
          title: t('rm.rel.info'),
          body: dl([
            [t('rm.rel.commit'), h('span', { class: 'mono' }, r.commit || '—')],
            [t('rm.rel.signer'), signerCell(r)],
            r.source === 'external' ? [t('rm.rel.baseUrl'), h('span', { class: 'mono k-break', id: 'release-base-url' }, r.base_url || '—')] : null,
            m ? [t('rm.rel.releasedAt'), m.released_at || '—'] : null,
            m ? [t('rm.rel.minKairos'), m.min_kairos || '—'] : null,
            m ? [t('rm.rel.minUpgradeFrom'), m.min_upgrade_from || '—'] : null,
            m ? [t('rm.rel.breaking'), m.breaking ? statusBadge('warning', t('rm.rel.breakingYes')) : t('rm.rel.breakingNo')] : null,
            m ? [t('rm.rel.features'), (m.remote_features || []).join('、') || '—'] : null,
            [t('rm.rel.importedBy'), [fmt.dateTime(r.imported_at, { seconds: false }), ' ', r.imported_by || '']],
          ]),
        }),
        card({
          title: t('rm.rel.usage'),
          body: [
            dl([
              [t('rm.rel.usageRunning'), t('rm.hostsN', { n: fmt.num(r.hosts_on_version || 0) })],
              [t('rm.rel.usageRollback'), t('rm.hostsN', { n: fmt.num(r.rollback_target_for || 0) })],
              [t('rm.rel.usageRollouts'), fmt.num(r.in_use_by_rollouts || 0)],
            ]),
            h('h3', { class: 'k-section-title' }, t('rm.rel.byPlatform')),
            byPlatform.length ? h('ul', { class: 'k-plain-list' }, byPlatform.map(([k, n]) => h('li', null, `${k}：${t('rm.hostsN', { n: fmt.num(n) })}`))) : h('p', { class: 'muted' }, t('rm.rel.byPlatformNone')),
          ],
        }),
      ),
      card({
        title: t('rm.rel.files'),
        body: [
          h(
            'div',
            { class: 'k-scroll-x' },
            h(
              'table',
              { class: 'k-mini-table', id: 'release-files' },
              h('caption', { class: 'ds-visually-hidden' }, t('rm.rel.filesCaption')),
              h(
                'thead',
                null,
                h(
                  'tr',
                  null,
                  h('th', { scope: 'col' }, t('rm.rel.col.file')),
                  h('th', { scope: 'col' }, t('rm.rel.col.platform')),
                  h('th', { scope: 'col' }, t('rm.rel.col.format')),
                  h('th', { scope: 'col', class: 'num' }, t('rm.rel.col.size')),
                  h('th', { scope: 'col' }, t('rm.rel.col.sha')),
                  h('th', { scope: 'col' }, t('rm.rel.col.stored')),
                ),
              ),
              h(
                'tbody',
                null,
                (r.artifacts || []).map((a) =>
                  h(
                    'tr',
                    { dataset: { file: a.filename } },
                    h('td', { class: 'k-break' }, a.filename),
                    h('td', null, `${a.os} ${a.arch}`, a.os === 'windows' ? h('span', { class: 'k-sub' }, t('rm.rel.windowsLater')) : null),
                    h('td', null, a.format),
                    h('td', { class: 'num' }, a.size ? fmt.bytes(a.size) : '—'),
                    h('td', null, h('span', { class: 'mono', title: a.sha256 }, String(a.sha256 || '').slice(0, 16)), ' ', copyButton(a.sha256, t('rm.rel.copySha', { f: a.filename }))),
                    h('td', null, a.file_problem ? statusBadge('warning', t(a.file_problem === 'size_mismatch' ? 'rm.rel.fileSizeMismatch' : 'rm.rel.fileMissing')) : a.stored ? statusBadge('online', t('rm.rel.stored')) : a.registered ? statusBadge('online', t('rm.rel.registered')) : neutralBadge(t('rm.rel.pending'))),
                  ),
                ),
              ),
            ),
          ),
        ],
      }),
    );
    ctx.focusTitle();
  }

  function del(r) {
    confirmL2({
      title: t('rm.rel.deleteTitle', { v: r.version }),
      lead: t('rm.rel.deleteLead'),
      body: t('rm.rel.deleteBody'),
      phrase: r.version,
      confirmText: t('rm.rel.deleteConfirm'),
      reason: 'optional',
      onConfirm: async ({ reason, errBox }) => {
        try {
          await api(`/api/v1/releases/${encodeURIComponent(r.version)}`, { method: 'DELETE', body: { confirm: r.version, reason: reason || undefined } });
          toast({ kind: 'success', title: t('rm.rel.deleted', { v: r.version }) });
          navigate('/pulse/releases', { replace: true });
          return true;
        } catch (err) {
          const a = errorAlert(err, { title: t('rm.rel.deleteFailed') });
          mount(errBox, a);
          a.focus();
          return false;
        }
      },
    });
  }

  load();
  return {
    dispose() {
      disposed = true;
    },
    isDirty: () => uploading > 0,
  };
}

// ---------------------------------------------------------------------------
// 匯入對話框

const VERSION_RE = /^pulse-(\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?)-([0-9a-f]{7,40})-/;

function readText(file) {
  return file.text();
}

/**
 * 匯入新版本（release 省略），或為既有的版本上傳發佈檔（release 為版本的 JSON）。
 */
function importDialog({ release = null, onDone } = {}) {
  const fid = uid('files');
  const fileIn = h('input', { type: 'file', id: fid, name: 'files', multiple: true, class: 'ds-input' });
  const chosen = h('p', { class: 'caption', 'aria-live': 'polite' });
  // 外部 URL 的版本庫（0.10；只在匯入新版本時）
  const bid = uid('base');
  const baseIn = h('input', { type: 'url', id: bid, name: 'base_url', class: 'ds-input ds-input--mono', placeholder: 'https://repo.example.gov.tw/kairos-pulse/0.10.0/', maxlength: '1800', 'aria-describedby': `${bid}-hint` });
  const errBox = h('div');
  const status = h('div', { class: 'k-stack--sm' });
  const fileList = h('ul', { class: 'k-imp-files', id: 'import-files' });
  const ok = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, release ? t('rm.rel.upload').replace('…', '') : t('rm.imp.verify'));
  const cancel = h('button', { type: 'button', class: 'ds-button ds-button--secondary' }, t('common.cancel'));
  let finished = false;
  let busy = false;
  fileIn.addEventListener('change', () => {
    chosen.textContent = t('rm.imp.selected', { n: fileIn.files.length });
  });
  const d = openDialog({
    title: release ? t('rm.imp.uploadTitle', { v: release.version }) : t('rm.imp.title'),
    size: 'lg',
    body: h(
      'div',
      { class: 'k-stack--sm' },
      h('p', null, release ? t('rm.imp.step1Upload') : t('rm.imp.step1')),
      h('div', { class: 'ds-field' }, h('label', { class: 'ds-field__label', for: fid }, t('rm.imp.files')), fileIn, chosen),
      release ? null : h('div', { class: 'ds-field' }, h('label', { class: 'ds-field__label', for: bid }, t('rm.imp.baseUrl')), baseIn, h('p', { class: 'ds-field__hint', id: `${bid}-hint` }, t('rm.imp.baseUrlHint'))),
      errBox,
      status,
      fileList,
      h('p', { class: 'caption' }, t('rm.rel.cli')),
    ),
    actions: [cancel, ok],
    initialFocus: fileIn,
    onClose: () => {
      if (finished && onDone) onDone();
    },
  });
  d.setGuard(() => !busy);
  cancel.addEventListener('click', () => {
    if (!busy) d.close();
  });

  async function uploadAll(rel, files) {
    const listed = new Map((rel.artifacts || []).map((a) => [a.filename, a]));
    let good = 0;
    let bad = 0;
    mount(fileList);
    const rows = files.map((f) => {
      const bar = h('div', { class: 'ds-progress__bar' });
      bar.style.setProperty('width', '0%');
      const prog = h('div', { class: 'ds-progress', role: 'progressbar', 'aria-valuemin': '0', 'aria-valuemax': '100', 'aria-valuenow': '0', 'aria-label': t('rm.imp.progress', { f: f.name }) }, h('div', { class: 'ds-progress__track' }, bar));
      const result = h('p', { class: 'k-imp-result caption' });
      const li = h('li', { dataset: { file: f.name } }, h('span', { class: 'k-imp-name mono' }, f.name), prog, result);
      fileList.appendChild(li);
      return { f, bar, prog, result };
    });
    for (const r of rows) {
      const a = listed.get(r.f.name);
      if (!a) {
        r.prog.hidden = true;
        mount(r.result, h('span', { class: 'k-fg--critical' }, icon('status-critical'), ' ', t('rm.imp.notListed')));
        bad += 1;
        continue;
      }
      // 已上傳過的不重傳；但已存入而檔案遺失或大小不符的（file_problem，例如還原備份之後）照常上傳補上
      if (((a.stored && !a.file_problem) || a.registered) && release) {
        r.prog.hidden = true;
        r.result.textContent = t('rm.imp.already');
        continue;
      }
      try {
        uploading += 1;
        await upload(`/api/v1/releases/${encodeURIComponent(rel.version)}/artifacts/${encodeURIComponent(r.f.name)}`, r.f, {
          onProgress: (loaded, total) => {
            const pct = total ? Math.round((loaded / total) * 100) : 0;
            r.bar.style.setProperty('width', `${pct}%`);
            r.prog.setAttribute('aria-valuenow', String(pct));
          },
        });
        r.bar.style.setProperty('width', '100%');
        r.prog.setAttribute('aria-valuenow', '100');
        mount(r.result, h('span', { class: 'k-fg--online' }, rel.source === 'external' ? t('rm.imp.hashOkExternal') : t('rm.imp.hashOk')));
        good += 1;
      } catch (err) {
        r.prog.hidden = true;
        const msg = err.code === 'artifact_not_listed' ? t('rm.imp.notListed') : err.code === 'artifact_hash_mismatch' ? `${err.message}（KRS-E8413）` : err.message || err.code;
        mount(r.result, h('span', { class: 'k-fg--critical' }, icon('status-critical'), ' ', t('rm.imp.failedFile', { msg })));
        bad += 1;
      } finally {
        uploading -= 1;
      }
    }
    return { good, bad };
  }

  ok.addEventListener('click', async () => {
    if (finished) {
      d.close();
      return;
    }
    mount(errBox);
    const files = Array.from(fileIn.files || []);
    const sums = files.find((f) => f.name === 'SHA256SUMS');
    const sig = files.find((f) => f.name === 'SHA256SUMS.sig');
    const manifest = files.find((f) => /^pulse-.+-release\.json$/.test(f.name));
    const artifacts = files.filter((f) => f !== sums && f !== sig && f !== manifest);
    if (!release && (!sums || !sig)) {
      mount(errBox, inlineAlert({ kind: 'error', title: t('rm.imp.needFiles'), body: [!sums ? t('rm.imp.missing', { f: 'SHA256SUMS' }) : null, !sig ? t('rm.imp.missing', { f: 'SHA256SUMS.sig' }) : null].filter(Boolean).join('、') }));
      fileIn.focus();
      return;
    }
    busy = true;
    cancel.disabled = true;
    fileIn.disabled = true;
    baseIn.disabled = true;
    setBusy(ok, true, release ? t('rm.imp.uploading') : t('rm.imp.verifying'));
    let rel = release;
    try {
      if (!rel) {
        const body = { sha256sums: await readText(sums), signature: await readText(sig) };
        if (manifest) body.manifest = await readText(manifest);
        if (baseIn.value.trim()) {
          body.source = 'external';
          body.base_url = baseIn.value.trim();
        }
        try {
          rel = await api('/api/v1/releases', { method: 'POST', body });
          mount(
            status,
            inlineAlert({
              kind: 'success',
              role: 'status',
              title: t('rm.imp.verified', { v: rel.version, c: rel.commit || '—' }),
              body: rel.signer_fingerprint ? t('rm.imp.signer', { fp: rel.signer_fingerprint, who: rel.signer_principal === OFFICIAL ? t('rm.rel.official') : t('rm.rel.customer', { p: rel.signer_principal || '—' }) }) : t('rm.imp.signerUnverified'),
            }),
          );
          finished = true;
          toast({ kind: 'success', title: t('rm.imp.imported', { v: rel.version }) });
        } catch (err) {
          // 同一版本已存在：只上傳尚未上傳的發佈檔
          const m = err.code === 'release_exists' ? (manifest && manifest.name.match(VERSION_RE)) || artifacts.map((f) => f.name.match(VERSION_RE)).find(Boolean) : null;
          if (!m) throw err;
          rel = await api(`/api/v1/releases/${encodeURIComponent(m[1])}`);
          mount(status, inlineAlert({ kind: 'info', role: 'status', body: t('rm.imp.exists', { v: rel.version }) }));
          release = rel;
        }
      }
      finished = true;
      if (!artifacts.length) {
        status.appendChild(h('p', { class: 'caption' }, t('rm.imp.nothingToUpload')));
      } else {
        const { good, bad } = await uploadAll(rel, artifacts);
        status.appendChild(h('p', { class: 'strong', role: 'status' }, t('rm.imp.done', { ok: fmt.num(good), bad: bad ? t('rm.imp.doneBad', { n: fmt.num(bad) }) : '' })));
      }
      setBusy(ok, false, t('rm.imp.uploading'));
      ok.textContent = t('rm.imp.close');
      cancel.hidden = true;
    } catch (err) {
      setBusy(ok, false, t('rm.imp.verifying'));
      fileIn.disabled = false;
      baseIn.disabled = false;
      cancel.disabled = false;
      const a = errorAlert(err, { title: t('rm.imp.failed') });
      mount(errBox, a);
      a.focus();
    } finally {
      busy = false;
      cancel.disabled = false;
    }
  });
}

