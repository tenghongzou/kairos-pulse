// S05 主機詳情的「更多」：匯出個資、刪除主機資料（商業審計 S6；設計包 kairos/screens.md S05「更多」選單、
// §0.3 L2「刪除主機資料」；API GET/POST /api/v1/hosts/{id}/purge、GET /api/v1/hosts/{id}/personal-data）。
//
// - 只有管理員（hosts:purge、hosts:personal_data）；沒有權限時不顯示按鈕（screens.md §0.2）。
// - 刪除：先取影響預覽（各資料的筆數、會撤銷的憑證、保留的稽核日誌與 CRL 所需的憑證紀錄），主機仍連線中時
//   說明並停止；否則 L2 DangerConfirmDialog（輸入主機名稱）＋選填原因。送出時帶 impact_token，數字已改變
//   （409 impact_changed）時在對話框內說明，請重新開啟。
// - 匯出：下載 JSON（kairos.host.personal-data/v1），檔名含 agent_id；內容含個資，提示妥善保管。

import { h, icon } from '../core/dom.js';
import { t } from '../core/strings.js';
import { api } from '../core/api.js';
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
  const rows = [
    [t('privacy.count.reports'), fmt.num(c.reports || 0)],
    [t('privacy.count.rollups'), fmt.num((c.rollup_5m || 0) + (c.rollup_1h || 0))],
    [t('privacy.count.processes'), fmt.num(c.process_snapshots || 0)],
    [t('privacy.count.inventory'), fmt.num((c.inventory || 0) + (c.inventory_changes || 0))],
    [t('privacy.count.alerts'), fmt.num(c.alert_events || 0)],
    [t('privacy.count.requests'), fmt.num(c.enroll_requests || 0)],
    [t('privacy.count.certificates'), fmt.num(c.certificates || 0)],
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
    // 仍連線中：說明並停止（不提供確認按鈕）
    const close = h('button', { type: 'button', class: 'ds-button ds-button--primary' }, t('common.close'));
    const d = openDialog({
      title: t('privacy.onlineTitle', { name }),
      size: 'sm',
      body: inlineAlert({ kind: 'warning', title: t('privacy.onlineLead'), code: p.blocked && p.blocked.product_code, body: t('privacy.onlineBody') }),
      actions: [close],
      initialFocus: close,
    });
    close.addEventListener('click', () => d.close());
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
        const title = e.code === 'impact_changed' ? t('privacy.impactChanged') : e.code === 'host_online' ? t('privacy.onlineLead') : t('privacy.purgeFailed');
        const a = errorAlert(e, { title });
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
