// 權限的整體檢查（三組畫面合併後）：每個內建角色登入後逐頁比對設計包 ia.md §4.2 的權限矩陣，
// 並直接呼叫寫入 API 確認後端的 RBAC 與畫面一致。
//
// - 側邊導覽：每個角色看到的項目與順序（ia.md §3 的區塊順序）。
// - 每一頁：該角色應該看得到的頁面不顯示 403，且載入期間沒有任何 API 回 403（畫面不呼叫自己沒有權限的端點；
//   有的話代表畫面的權限判斷比後端寬）；不該看到的頁面顯示 403。
// - 稽核員與唯讀：可見的頁面沒有寫入按鈕（稽核員的「匯出」「驗證完整性」屬於讀取；個人設定是自己的帳號，不列入）。
// - 直接呼叫寫入 API：角色沒有該權限時一律 403，details.permission 是角色確實沒有的權限（不存在的對象也先回 403，
//   不洩漏存在與否）；以工作階段呼叫但不帶 X-CSRF-Token 時 403。
// 只對「沒有權限」的角色呼叫寫入端點，對象都是不存在的名稱，即使後端誤放行也不會改到測試資料。

import assert from 'node:assert/strict';

const PAGES = [
  // [路徑, 頁面標題（可見時）, 可見的角色]
  ['/', '總覽', ['admin', 'operator', 'auditor', 'viewer']],
  ['/hosts', '主機', ['admin', 'operator', 'auditor', 'viewer']],
  // F4：主機詳情的行程與資產資訊（hosts:read；METRICS.md 第 10 節）
  ['/hosts/11111111-1111-4111-8111-000000000001/processes', 'web-01', ['admin', 'operator', 'auditor', 'viewer']],
  ['/hosts/11111111-1111-4111-8111-000000000001/inventory', 'web-01', ['admin', 'operator', 'auditor', 'viewer']],
  ['/hosts/11111111-1111-4111-8111-000000000001/metrics?metric=io', 'web-01', ['admin', 'operator', 'auditor', 'viewer']],
  ['/alerts/events', '告警事件', ['admin', 'operator', 'auditor', 'viewer']],
  ['/alerts/rules', '告警規則', ['admin', 'operator', 'auditor']],
  ['/alerts/silences', '靜音與維護時段', ['admin', 'operator', 'auditor', 'viewer']],
  ['/alerts/channels', '通知管道', ['admin', 'operator']],
  ['/enrollment/pending', '待核准', ['admin', 'operator', 'auditor']],
  ['/enrollment/tokens', '註冊 token', ['admin', 'operator', 'auditor']],
  ['/certificates', '憑證', ['admin', 'operator', 'auditor']],
  ['/certificates/ca', 'CA 與撤銷清單', ['admin', 'operator', 'auditor']],
  ['/audit', '稽核日誌', ['admin', 'operator', 'auditor']],
  ['/admin/users', '使用者與角色', ['admin', 'auditor']],
  ['/admin/roles', '', ['admin', 'auditor']],
  ['/admin/directory', '目錄服務', ['admin', 'auditor']],
  ['/system/health', '系統健康', ['admin', 'operator', 'auditor', 'viewer']],
  ['/system/settings', '系統設定', ['admin', 'operator', 'auditor']],
  ['/account', '個人設定', ['admin', 'operator', 'auditor', 'viewer']],
];

const ALL_NAV = ['總覽', '主機', '告警事件', '告警規則', '靜音與維護', '通知管道', '待核准', '註冊 token', '憑證', 'CA 與撤銷清單', '稽核日誌', '使用者與角色', '目錄服務', '系統健康', '系統設定'];
const NAV = {
  admin: ALL_NAV,
  operator: ['總覽', '主機', '告警事件', '告警規則', '靜音與維護', '通知管道', '待核准', '註冊 token', '憑證', 'CA 與撤銷清單', '稽核日誌', '系統健康', '系統設定'],
  auditor: ['總覽', '主機', '告警事件', '告警規則', '靜音與維護', '待核准', '註冊 token', '憑證', 'CA 與撤銷清單', '稽核日誌', '使用者與角色', '目錄服務', '系統健康', '系統設定'],
  viewer: ['總覽', '主機', '告警事件', '靜音與維護', '系統健康'],
};
const SECTIONS = {
  admin: ['監控', '註冊與憑證', '稽核與報表', '管理', '系統'],
  operator: ['監控', '註冊與憑證', '稽核與報表', '系統'],
  auditor: ['監控', '註冊與憑證', '稽核與報表', '管理', '系統'],
  viewer: ['監控', '系統'],
};

// [方法, 路徑, 所需權限]
const NOBODY = 'rbac-nobody-x';
const WRITES = [
  ['POST', `/api/v1/alerts/E-${NOBODY}/ack`, 'alerts:write'],
  ['POST', `/api/v1/alerts/E-${NOBODY}/resolve`, 'alerts:write'],
  ['POST', '/api/v1/alert-rules', 'alert-rules:write'],
  ['PUT', `/api/v1/alert-rules/${NOBODY}`, 'alert-rules:write'],
  ['DELETE', `/api/v1/alert-rules/${NOBODY}`, 'alert-rules:write'],
  ['POST', '/api/v1/silences', 'silences:write'],
  ['POST', '/api/v1/channels', 'channels:write'],
  ['DELETE', `/api/v1/channels/${NOBODY}`, 'channels:write'],
  ['POST', '/api/v1/channels/test', 'channels:write'], // 未儲存的設定（API.md 第 13 節）
  ['POST', `/api/v1/channels/${NOBODY}/test`, 'channels:test'],
  ['POST', '/api/v1/enroll/tokens', 'enroll.tokens:write'],
  ['POST', `/api/v1/enroll/tokens/${NOBODY}/disable`, 'enroll.tokens:write'],
  ['POST', '/api/v1/enroll/requests/approve', 'enroll.requests:decide'],
  ['POST', '/api/v1/enroll/requests/reject', 'enroll.requests:decide'],
  ['PUT', '/api/v1/enroll/mode', 'enroll.mode:write'],
  ['POST', '/api/v1/pki/certificates/revoke', 'pki.certificates:revoke'],
  ['POST', '/api/v1/pki/crl/regenerate', 'pki.certificates:revoke'],
  ['POST', '/api/v1/pki/grace-overrides', 'pki.grace:override'],
  ['POST', '/api/v1/pki/signing-queue/import', 'pki.queue:manage'],
  ['PUT', '/api/v1/pki/policy', 'pki.policy:write'],
  ['POST', '/api/v1/users', 'users:write'],
  ['PUT', `/api/v1/users/${NOBODY}`, 'users:write'],
  ['PUT', `/api/v1/users/${NOBODY}/roles`, 'users:write'],
  ['POST', `/api/v1/users/${NOBODY}/password`, 'users:write'],
  ['POST', `/api/v1/users/${NOBODY}/delete`, 'users:write'],
  ['POST', `/api/v1/users/${NOBODY}/sessions/revoke-all`, 'sessions:revoke'],
  ['POST', '/api/v1/api-tokens', 'api_tokens:write'],
  ['POST', `/api/v1/api-tokens/${NOBODY}/revoke`, 'api_tokens:write'],
  ['PUT', '/api/v1/directory/mappings', 'directory:write'],
  ['POST', '/api/v1/directory/test', 'directory:write'],
];

// 稽核員與唯讀不應看到的寫入按鈕（開頭的動詞；ds-button 與連結樣式的按鈕）
const WRITE_VERBS = /^(新增|建立|核准|拒絕|撤銷|停用|啟用|刪除|儲存|套用|確認事件|確認並|指派|解決|立即更新|匯入|允許再次|寄送測試|結束所有|重設|解除鎖定|編輯|改用設定檔)/;

export function registerRbac({ test, newPage, login, waitTitle, URL0, PW }) {
  for (const role of ['admin', 'operator', 'auditor', 'viewer']) {
    test(`權限整體檢查（${role}）：導覽、逐頁可見性、載入時沒有 403、寫入按鈕、直接呼叫寫入 API`, async () => {
      const { ctx, page } = await newPage();
      await login(page, role, PW, '/account');
      await waitTitle(page, '個人設定');

      // 側邊導覽（項目與區塊順序）
      const labels = await page.locator('.ds-sidenav__link .ds-sidenav__label').allTextContents();
      assert.deepEqual(labels, NAV[role], `${role} 的導覽：${labels.join('、')}`);
      const heads = await page.locator('.ds-sidenav__heading').allTextContents();
      assert.deepEqual(heads, SECTIONS[role], `${role} 的導覽區塊：${heads.join('、')}`);

      // 載入期間的 API 403
      let denied = [];
      page.on('response', (r) => {
        const u = new URL(r.url());
        if (u.pathname.startsWith('/api/') && r.status() === 403) denied.push(`${r.request().method()} ${u.pathname}${u.search}`);
      });

      for (const [path, title, roles] of PAGES) {
        denied = [];
        await page.goto(`${URL0}${path}`);
        await page.locator('#page-title').first().waitFor({ timeout: 15000 });
        await page.waitForLoadState('networkidle').catch(() => {});
        await page.waitForTimeout(300);
        const heading = (await page.locator('#page-title').first().textContent()).trim();
        const forbidden = heading === '沒有權限檢視這個畫面';
        if (roles.includes(role)) {
          assert.ok(!forbidden, `${role} 應可檢視 ${path}`);
          if (title) assert.ok(heading.includes(title), `${role} ${path} 的標題：${heading}`);
          assert.deepEqual(denied, [], `${role} 在 ${path} 載入時有 API 回 403：${denied.join('、')}`);
          if ((role === 'auditor' || role === 'viewer') && path !== '/account') {
            const names = await page
              .locator('main button:visible:not([aria-disabled="true"]):not([disabled]), main a.ds-button:visible')
              .evaluateAll((els) => els.map((e) => (e.getAttribute('aria-label') || e.textContent || '').trim()));
            const writes = names.filter((n) => WRITE_VERBS.test(n));
            assert.deepEqual(writes, [], `${role} 在 ${path} 看得到寫入按鈕：${writes.join('、')}`);
          }
        } else {
          assert.ok(forbidden, `${role} 不應可檢視 ${path}（標題：${heading}）`);
        }
      }

      // 直接呼叫寫入 API
      const results = await page.evaluate(async (writes) => {
        const me = (await (await fetch('/api/v1/me', { credentials: 'same-origin' })).json()).data;
        const perms = new Set(me.permissions || []);
        const out = [];
        for (const [method, path, perm] of writes) {
          if (perms.has(perm)) continue;
          const r = await fetch(path, {
            method,
            credentials: 'same-origin',
            headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': me.csrf_token },
            body: method === 'DELETE' ? undefined : '{}',
          });
          let body = null;
          try {
            body = await r.json();
          } catch {
            body = null;
          }
          out.push({ method, path, perm, status: r.status, permission: body && body.error && body.error.details ? body.error.details.permission : body && body.details ? body.details.permission : null });
        }
        // CSRF：工作階段的寫入不帶 X-CSRF-Token
        const c = await fetch('/api/v1/me/sessions/revoke-others', { method: 'POST', credentials: 'same-origin', headers: { 'Content-Type': 'application/json' }, body: '{}' });
        let csrfCode = null;
        try {
          csrfCode = (await c.json()).error.code;
        } catch {
          csrfCode = null;
        }
        return { results: out, csrfStatus: c.status, csrfCode, permissions: [...perms] };
      }, WRITES);
      // details.permission 可能是先檢查的讀取權限（例如唯讀呼叫註冊的寫入端點時為 enroll:read）：必須是角色確實沒有的權限
      const owned = new Set(results.permissions);
      const bad = results.results.filter((r) => r.status !== 403 || !r.permission || owned.has(r.permission));
      assert.deepEqual(bad, [], `${role} 直接呼叫寫入 API 沒有被 403 拒絕：${JSON.stringify(bad)}`);
      if (role !== 'admin') assert.ok(results.results.length > 0, `${role} 沒有檢查到任何寫入端點`);
      assert.equal(results.csrfStatus, 403, `沒有 X-CSRF-Token 的寫入應回 403（${results.csrfCode}）`);
      assert.ok(results.permissions.length > 0, '讀不到 /api/v1/me 的權限');
      console.log(`     ${role}：${PAGES.length} 頁、${results.results.length} 個寫入端點 403、權限 ${results.permissions.length} 項`);
      await ctx.close();
    });
  }
}
