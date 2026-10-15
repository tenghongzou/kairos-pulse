// 權限的整體檢查（三組畫面合併後）：每個內建角色登入後逐頁比對設計包 ia.md §4.2 的權限矩陣，
// 並直接呼叫寫入 API 確認後端的 RBAC 與畫面一致。
//
// - 側邊導覽：每個角色看到的項目與順序（ia.md §3 的區塊順序）。
// - 每一頁：該角色應該看得到的頁面不顯示 403，且載入期間沒有任何 API 回 403（畫面不呼叫自己沒有權限的端點；
//   有的話代表畫面的權限判斷比後端寬）；不該看到的頁面顯示 403。
// - 寫入按鈕的自動掃描（每個角色、每一頁，含清單第一筆的詳情頁）：主要內容中看得到、可以按的寫入按鈕（名稱以寫入的
//   動詞開頭）一律要能對應到 BUTTON_PERMS 的一列，且角色有該列的權限（ia.md §4.2 → API.md 第 12.1 節的權限名稱）。
//   對應不到的按鈕視為失敗（新增按鈕時要一併分類）。部分權限以 aria-disabled 顯示並說明原因的按鈕不算（screens.md §0.2）。
//   EXPECTED：有權限的角色一定看得到的主要寫入按鈕（反向檢查：不能因為權限判斷寫錯而對有權限的人隱藏）。
//   稽核員的「匯出」「驗證完整性」屬於讀取；個人設定是自己的帳號，不列入。
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
  ['/system/settings/security', '系統設定', ['admin', 'operator', 'auditor']],
  ['/system/settings/integrations', '系統設定', ['admin', 'operator', 'auditor']],
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

// 清單頁 → 第一筆的詳情頁（也掃描寫入按鈕）：[清單路徑, 詳情連結的選擇器]
const DETAILS = [
  ['/alerts/events', 'tbody tr a[href^="/alerts/events/"]'],
  ['/alerts/rules', 'tbody tr a[href^="/alerts/rules/"]'],
  ['/alerts/channels', 'tbody tr a[href^="/alerts/channels/"]'],
  ['/enrollment/tokens', 'tbody tr a[href^="/enrollment/tokens/"]'],
  ['/admin/users', 'tbody tr a[href^="/admin/users/"]'],
];

// 寫入按鈕的名稱（開頭的動詞；ds-button、連結樣式的按鈕、開關）
const WRITE_VERBS = /^(新增|建立|核准|拒絕|撤銷|停用|啟用|刪除|儲存|套用|確認|指派|解決|暫停通知|立即更新|匯入|匯出 CSR|允許再次|寄送測試|結束所有|重設|解除鎖定|編輯|改用|複製$|移除|更換|臨時靜音|排定維護|提前結束|取消排程|以帳號測試)/;

// 寫入按鈕 → 需要的權限：[頁面路徑的正規式, 按鈕名稱的正規式, 權限]（ia.md §4.2；權限名稱見 API.md 第 12.1 節）
const BUTTON_PERMS = [
  // S03 進行中事件的行內確認、S06／S07 事件
  [/^\/($|alerts\/events)/, /^(確認|指派|解決|暫停通知)/, 'alerts:write'],
  [/^\/alerts\/events/, /^建立靜音/, 'silences:write'],
  // S08／S09 規則（系統預設規則的條件只有 alert-rules:admin，維運者看到停用的欄位）
  [/^\/alerts\/rules/, /^(新增規則|建立規則|儲存規則|複製$|刪除|啟用規則|套用建議|移除主機)/, 'alert-rules:write'],
  // S10 靜音與維護
  [/^\/alerts\/silences/, /^(臨時靜音|排定維護時段|建立靜音|提前結束|取消排程)/, 'silences:write'],
  // S11 通知管道：維運者只有寄送測試（channels:test）
  [/^\/alerts\/channels\/[^n]/, /^寄送測試通知/, 'channels:test'],
  [/^\/alerts\/channels$/, /^寄送測試通知/, 'channels:test'],
  [/^\/alerts\/channels/, /^(新增.*管道|建立管道|儲存|刪除管道|更換)/, 'channels:write'],
  // S12／S13 註冊
  [/^\/enrollment\/tokens/, /^(建立.*token|停用 token)/, 'enroll.tokens:write'],
  [/^\/enrollment\/pending/, /^(核准|拒絕)/, 'enroll.requests:decide'],
  // S14／S15 憑證與 CA
  [/^\/certificates/, /^撤銷/, 'pki.certificates:revoke'],
  [/^\/certificates\/ca/, /^立即更新 CRL/, 'pki.certificates:revoke'],
  [/^\/certificates/, /^允許再次自動續簽/, 'pki.grace:override'],
  [/^\/certificates/, /^建立限定這些主機的 token/, 'enroll.tokens:write'],
  [/^\/certificates/, /^(匯出 CSR 佇列|匯入簽發結果)/, 'pki.queue:manage'],
  // S16 使用者與角色
  [/^\/admin\/(users|roles)/, /^(新增使用者|建立使用者|編輯|停用帳號|啟用帳號|解除鎖定|重設|刪除使用者|移除)/, 'users:write'],
  [/^\/admin\/users/, /^結束所有工作階段/, 'sessions:revoke'],
  [/^\/admin\/users\//, /^(建立 API token|撤銷)/, 'api_tokens:write'],
  // S17 目錄服務
  [/^\/admin\/directory/, /^(新增對應|移除|以帳號測試登入)/, 'directory:write'],
  // S21 系統設定
  [/^\/system\/settings/, /^套用模式/, 'enroll.mode:write'],
  [/^\/system\/settings/, /^改用(設定檔的值|內建預設值)：enroll\./, 'enroll.mode:write'],
  [/^\/system\/settings/, /^改用(設定檔的值|內建預設值)：pki\./, 'pki.policy:write'],
  [/^\/system\/settings/, /^(建立 API token|撤銷)/, 'api_tokens:write'],
];

// 有權限的角色一定看得到的寫入按鈕：[頁面, 按鈕名稱的正規式, 權限]
const EXPECTED = [
  ['/alerts/rules', /^新增規則/, 'alert-rules:write'],
  ['/alerts/silences', /^臨時靜音/, 'silences:write'],
  ['/alerts/channels', /^新增管道/, 'channels:write'],
  ['/enrollment/tokens', /^建立.*token/, 'enroll.tokens:write'],
  ['/admin/users', /^新增使用者/, 'users:write'],
  ['/system/settings/security', /^套用模式/, 'enroll.mode:write'],
  ['/system/settings/integrations', /^建立 API token/, 'api_tokens:write'],
];

/** 主要內容中看得到的按鈕名稱（含 role="switch" 的開關）；enabled＝可以按的（aria-disabled／disabled 的不算） */
async function visibleButtons(page, { enabled = true } = {}) {
  const sel = enabled
    ? 'main button:visible:not([aria-disabled="true"]):not([disabled]), main a.ds-button:visible:not([aria-disabled="true"])'
    : 'main button:visible, main a.ds-button:visible';
  return page.locator(sel).evaluateAll((els) => els.map((e) => (e.getAttribute('aria-label') || e.textContent || '').replace(/\s+/g, ' ').trim()));
}

/** 這一頁的寫入按鈕與權限的比對：回傳 [未分類, 沒有權限卻看得到] */
function checkButtons(path, names, perms) {
  const unclassified = [];
  const violations = [];
  for (const n of names.filter((x) => WRITE_VERBS.test(x))) {
    const rule = BUTTON_PERMS.find(([p, b]) => p.test(path) && b.test(n));
    if (!rule) unclassified.push(n);
    else if (!perms.has(rule[2])) violations.push(`${n}（需要 ${rule[2]}）`);
  }
  return [unclassified, violations];
}

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

      const perms = new Set((await page.evaluate(async () => (await (await fetch('/api/v1/me')).json()).data.permissions)) || []);
      let scanned = 0;
      const scan = async (path) => {
        const names = await visibleButtons(page);
        const [unclassified, violations] = checkButtons(path, names, perms);
        assert.deepEqual(unclassified, [], `${role} 在 ${path} 有沒有分類的寫入按鈕（加到 e2e-rbac.mjs 的 BUTTON_PERMS）：${unclassified.join('、')}`);
        assert.deepEqual(violations, [], `${role} 在 ${path} 看得到沒有權限的寫入按鈕：${violations.join('、')}`);
        // 反向檢查含 aria-disabled 的按鈕（例：沒有變更時「套用模式…」為 aria-disabled，但仍代表有這個能力）
        const all = await visibleButtons(page, { enabled: false });
        for (const [p, b, perm] of EXPECTED) {
          if (p !== path) continue;
          const has = all.some((n) => b.test(n));
          assert.equal(has, perms.has(perm), `${role} 在 ${path}：「${b.source}」${perms.has(perm) ? '應該看得到' : '不應看到'}（${perm}）`);
        }
        scanned += 1;
      };

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
          if (path !== '/account') await scan(path);
          // 清單第一筆的詳情頁
          const detail = DETAILS.find(([p]) => p === path);
          const link = detail ? page.locator(detail[1]).first() : null;
          if (link && (await link.count())) {
            const href = await link.getAttribute('href');
            denied = [];
            await page.goto(`${URL0}${href}`);
            await page.locator('#page-title').first().waitFor({ timeout: 15000 });
            await page.waitForLoadState('networkidle').catch(() => {});
            await page.waitForTimeout(300);
            assert.notEqual((await page.locator('#page-title').first().textContent()).trim(), '沒有權限檢視這個畫面', `${role} 應可檢視 ${href}`);
            assert.deepEqual(denied, [], `${role} 在 ${href} 載入時有 API 回 403：${denied.join('、')}`);
            await scan(new URL(href, URL0).pathname);
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
      console.log(`     ${role}：${PAGES.length} 頁（掃描寫入按鈕 ${scanned} 頁）、${results.results.length} 個寫入端點 403、權限 ${results.permissions.length} 項`);
      await ctx.close();
    });
  }
}
