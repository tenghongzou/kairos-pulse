// S05 主機詳情的「更多」：匯出個資、刪除主機資料（商業審計 S6；設計包 kairos/screens.md S05「更多」選單、
// §0.3 L2「刪除主機資料」；API GET/POST /api/v1/hosts/{id}/purge、GET /api/v1/hosts/{id}/personal-data）。
//
// - 只有管理員（hosts:purge、hosts:personal_data）；沒有權限時不顯示按鈕（screens.md §0.2）。
// - 刪除：先取影響預覽（各資料的筆數，0 筆照常列出；會撤銷的憑證、保留的稽核日誌與 CRL 所需的憑證紀錄），
//   主機仍連線中時說明並停止（有 pki.certificates:revoke 時附「撤銷憑證…」捷徑）；否則 L2 DangerConfirmDialog
//   （輸入主機名稱）＋選填原因。送出時帶 impact_token，數字已改變（409 impact_changed）時在對話框內說明，
//   請重新開啟；刪除沒有完成（503）時說明重新執行即可接續完成。
// - 假名化的主機（privacy.hostname = "pseudonymize"）：pseudonymNote() 放在 S05 頁首名稱旁。
// - 匯出：下載 JSON（kairos.host.personal-data/v1），檔名含 agent_id；內容含個資，提示妥善保管。

import { h, icon } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as fmt from '../core/format.js';
import { navigate } from '../core/router.js';
import { dl, errorAlert, inlineAlert } from '../ui/components.js';
import { openDialog } from '../ui/dialog.js';
import { openMenu } from '../ui/menu.js';
import { toast } from '../ui/toast.js';
import { confirmL2 } from './admin-common.js';
import { downloadText } from './pki-common.js';

/** 頁首的「更多」按鈕（沒有權限時 null） */
export function hostActions(hst) {
  const canPurge = session.can('hosts:purge');
  const canExport = session.can('hosts:personal_data');
  if (!canPurge && !canExport) return null;
  const name = hst.display_name || hst.hostname || hst.agent_id;
  const b = h(
    'button',
    { type: 'button', class: 'ds-button ds-button--secondary', 'aria-haspopup': 'menu', 'aria-expanded': 'false', id: 'host-more' },
    icon('more'),
    t('privacy.more'),
  );
  b.addEventListener('click', () => {
    const items = [];
    if (canExport) items.push({ text: t('privacy.export'), iconName: 'download', onSelect: () => exportPersonalData(hst) });
    if (canPurge) {
      if (items.length) items.push('separator');
      items.push({ text: t('privacy.purge'), iconName: 'trash', danger: true, onSelect: () => purgeHost(hst) });
    }
    openMenu(b, items, { label: t('privacy.moreLabel', { name }) });
  });
  return b;
}

// 假名的格式（kairos/PRIVACY.md 第 3 節）：`host-` ＋ RFC 4648 base32 小寫的前 12 個字元。
// 主機 API 沒有「已假名化」的欄位，以主機名稱的格式判斷（隨機名稱剛好符合的機率可忽略）
const PSEUDONYM = /^host-[a-z2-7]{12}$/;

/** S05 頁首名稱旁：假名化主機的可見說明與查詢指令（不是假名時 null；不提供「顯示原名」，D53） */
export function pseudonymNote(hst) {
  if (!hst || !PSEUDONYM.test(hst.hostname || '')) return null;
  return h(
    'span',
    { class: 'k-pseudonym' },
    icon('info'),
    h('span', null, t('privacy.pseudonym'), ' ', h('code', { class: 'k-code-tag' }, t('privacy.pseudonymCmd')), t('privacy.pseudonymEnd')),
  );
}

async function exportPersonalData(hst) {
  try {
    const doc = await api(`/api/v1/hosts/${encodeURIComponent(hst.agent_id)}/personal-data`);
    downloadText(`kairos-host-${hst.agent_id}-personal-data.json`, `${JSON.stringify(doc, null, 2)}\n`, 'application/json');
    toast({ kind: 'success', title: t('privacy.exported'), desc: t('privacy.exportedDesc') });
  } catch (e) {
    toast({ kind: 'error', title: t('privacy.exportFailed'), desc: e.message });
  }
}

/** 影響預覽的內容（各資料的筆數與保留的項目） */
function impactBody(p) {
  const c = p.counts || {};
  // 0 筆的列照常顯示「0 筆」（讓人知道每一類都檢查過；設計包 S05「0.9 規格」）
  const n = (v) => t('privacy.countValue', { n: fmt.num(v || 0) });
  const rows = [
    [t('privacy.count.reports'), n(c.reports)],
    [t('privacy.count.rollups'), n((c.rollup_5m || 0) + (c.rollup_1h || 0))],
    [t('privacy.count.processes'), n(c.process_snapshots)],
    [t('privacy.count.inventory'), n((c.inventory || 0) + (c.inventory_changes || 0))],
    [t('privacy.count.alerts'), n(c.alert_events)],
    [t('privacy.count.requests'), n(c.enroll_requests)],
    [t('privacy.count.certificates'), n(c.certificates)],
  ];
  return h(
    'div',
    { class: 'k-stack--sm' },
    h('p', null, t('privacy.purgeBody')),
    dl(rows),
    p.revoke && p.revoke.certificates ? inlineAlert({ kind: 'warning', title: t('privacy.revokeTitle', { n: fmt.num(p.revoke.certificates) }), body: t('privacy.revokeBody') }) : null,
    h(
      'ul',
      { class: 'k-note-list' },
      h('li', null, t('privacy.keptAudit')),
      p.kept && p.kept.certificates_for_crl ? h('li', null, t('privacy.keptCrl', { n: fmt.num(p.kept.certificates_for_crl) })) : null,
      h('li', null, t('privacy.keptLogs')),
    ),
  );
}

async function purgeHost(hst) {
  const id = encodeURIComponent(hst.agent_id);
  let p;
  try {
    p = await api(`/api/v1/hosts/${id}/purge`);
  } catch (e) {
    toast({ kind: 'error', title: t('privacy.previewFailed'), desc: e.message });
    return;
  }
  const name = p.display_name || p.hostname || hst.agent_id;
  if (p.online) {
    // 仍連線中：說明並停止（只有「關閉」；有撤銷權限時附「撤銷憑證…」捷徑，到憑證清單篩選這台主機）
    const close = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('common.close'));
    const revoke = session.can('pki.certificates:revoke')
      ? h('a', { class: 'ds-button ds-button--secondary ds-button--sm', href: `/certificates${qs({ q: hst.agent_id })}` }, icon('shield-x'), t('privacy.onlineRevoke'))
      : null;
    const d = openDialog({
      title: t('privacy.onlineTitle', { name }),
      size: 'sm',
      body: inlineAlert({ kind: 'warning', title: t('privacy.onlineLead'), code: p.blocked && p.blocked.product_code, body: t('privacy.onlineBody'), actions: revoke }),
      actions: [close],
      initialFocus: close,
    });
    close.addEventListener('click', () => d.close());
    if (revoke) revoke.addEventListener('click', () => d.close());
    return;
  }
  let done = null;
  await confirmL2({
    title: t('privacy.purgeTitle', { name }),
    lead: t('privacy.purgeLead'),
    body: impactBody(p),
    phrase: p.confirm_text,
    confirmText: t('privacy.purgeConfirm'),
    reason: 'optional',
    onConfirm: async ({ reason, errBox }) => {
      try {
        done = await api(`/api/v1/hosts/${id}/purge`, {
          method: 'POST',
          body: { confirm: p.confirm_text, impact_token: p.impact_token, reason },
        });
        return true;
      } catch (e) {
        let a;
        if (e.status === 503) {
          // 刪除沒有完成（KRS-E4205）：代碼總表的範例是「仍連線中」，這裡改寫成接續完成的說明；伺服器訊息放技術細節
          const tech = [`HTTP ${e.status}`, e.code, e.message].filter(Boolean).join(' · ');
          a = inlineAlert({
            kind: 'error',
            title: t('privacy.purgeFailed'),
            code: e.productCode || 'KRS-E4205',
            body: [
              h('p', null, t('privacy.purgeIncomplete')),
              h('details', { class: 'k-details' }, h('summary', null, t('err.label.technical')), h('p', { class: 'k-code-line' }, tech)),
            ],
          });
          a.setAttribute('tabindex', '-1');
        } else if (e.code === 'host_online') {
          a = inlineAlert({ kind: 'error', title: t('privacy.onlineLead'), body: t('privacy.onlineBody'), code: e.productCode || undefined });
          a.setAttribute('tabindex', '-1');
        } else {
          a = errorAlert(e, { title: e.code === 'impact_changed' ? t('privacy.impactChanged') : t('privacy.purgeFailed') });
        }
        errBox.replaceChildren(a);
        a.focus();
        return false;
      }
    },
  });
  if (!done) return;
  toast({
    kind: 'success',
    title: t('privacy.purged', { name }),
    desc: t('privacy.purgedDesc', { n: fmt.num((done.deleted && done.deleted.total) || 0) }),
  });
  navigate('/hosts');
}
