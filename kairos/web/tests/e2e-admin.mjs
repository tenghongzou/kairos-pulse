// 管理與系統畫面（S16～S18、S20、S21）的端對端測試；由 e2e.mjs 以 registerAdmin(...) 註冊。
//
// 測試資料（run.sh）：admin、operator、auditor、viewer 帳號；kairos.toml 的 [enroll] auto_pause_after_hours = 36
// （設定檔的值）與環境變數 KAIROS__ENROLL__AUTO_PAUSE_ENABLED=true（環境變數鎖定的設定）。

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

export function registerAdmin({ test, newPage, login, waitTitle, axe, shot, URL0, PW, XSS_NAME }) {
  /** 以目前的工作階段呼叫 API（帶 CSRF；CSRF token 由 /api/v1/me 取得，只在這個函式內） */
  async function apiCall(page, method, path, body) {
    return page.evaluate(
      async ([m, p, b]) => {
        const me = await (await fetch('/api/v1/me')).json();
        const r = await fetch(p, {
          method: m,
          headers: m === 'GET' ? {} : { 'Content-Type': 'application/json', 'X-CSRF-Token': me.data.csrf_token },
          body: b ? JSON.stringify(b) : undefined,
        });
        let j = null;
        try {
          j = await r.json();
        } catch {
          j = null;
        }
        return { status: r.status, body: j };
      },
      [method, path, body],
    );
  }

  async function storageDump(page) {
    return page.evaluate(() => {
      const out = [];
      for (const s of [localStorage, sessionStorage]) for (let i = 0; i < s.length; i += 1) out.push(`${s.key(i)}=${s.getItem(s.key(i))}`);
      return out.join('\n');
    });
  }

  async function navLabels(page) {
    return page.locator('.ds-sidenav__link .ds-sidenav__label').allTextContents();
  }

  let alicePw = null;

  test('S16：管理員建立使用者 → 一次性密碼只顯示一次 → 該使用者登入被要求改密碼', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/admin/users');
    await waitTitle(page, '使用者與角色');
    await page.locator('tbody tr[data-key="admin"]').waitFor();
    // 「從目錄服務加入」與權限清冊 CSV 隱藏
    assert.equal(await page.getByRole('button', { name: /從目錄服務加入|匯出/ }).count(), 0);
    await axe(page, '使用者清單');
    await shot(page, 'admin-users-light');
    await page.getByRole('button', { name: '新增使用者…' }).click();
    const dlg = page.getByRole('dialog');
    await dlg.getByLabel(/^帳號/).fill('alice.w');
    await dlg.getByLabel('名稱', { exact: true }).fill('王愛麗');
    await axe(page, '新增使用者');
    await dlg.getByRole('button', { name: '建立使用者' }).click();
    const secretBox = page.locator('dialog input[data-secret]');
    await secretBox.waitFor();
    const pw = await secretBox.inputValue();
    assert.match(pw, /^\S{20,}$/, `一次性密碼 ${pw.length} 字元`);
    await page.getByText('這是唯一一次顯示').waitFor();
    await axe(page, '一次性密碼');
    // 沒有勾選「我已記下」不能關閉
    await page.keyboard.press('Escape');
    assert.equal(await page.locator('dialog[open]').count(), 1);
    await page.getByRole('dialog').getByRole('button', { name: '關閉', exact: true }).last().click({ force: true });
    assert.equal(await page.locator('dialog[open]').count(), 1);
    await page.getByLabel(/我已記下/).check();
    await page.getByRole('dialog').getByRole('button', { name: '關閉', exact: true }).last().click();
    await page.locator('dialog[open]').waitFor({ state: 'detached' });
    await waitTitle(page, '王愛麗');
    // 關閉後畫面、網址、瀏覽器儲存都沒有這個密碼
    const html = await page.evaluate(() => document.documentElement.outerHTML + Array.from(document.querySelectorAll('input')).map((i) => i.value).join(' '));
    assert.ok(!html.includes(pw), '一次性密碼不應留在畫面上');
    assert.ok(!page.url().includes(pw));
    assert.ok(!(await storageDump(page)).includes(pw), '一次性密碼不應存在瀏覽器儲存');
    await page.getByText('下次登入須變更密碼').first().waitFor();
    await axe(page, '使用者詳情');
    await shot(page, 'admin-user-light');
    alicePw = pw;
    // 對方第一次登入：必須變更密碼
    const b = await newPage();
    await login(b.page, 'alice.w', pw, '/');
    await b.page.locator('#page-title', { hasText: '變更密碼' }).waitFor();
    await b.page.getByLabel(/目前的密碼/).fill(pw);
    await b.page.getByLabel(/^新密碼/).fill('Amber-Orchid-Falcon-73');
    await b.page.getByLabel(/確認新密碼/).fill('Amber-Orchid-Falcon-73');
    await b.page.getByRole('button', { name: '變更密碼' }).click();
    await waitTitle(b.page, '總覽');
    // 角色變更（L1，顯示權限差異）
    await page.getByRole('button', { name: '編輯角色…' }).click();
    const rd = page.getByRole('dialog');
    await rd.getByRole('combobox', { name: /^角色/ }).selectOption('auditor');
    await rd.getByRole('button', { name: '加入指派' }).click();
    await rd.getByText('稽核員（全站）').waitFor();
    await rd.getByRole('button', { name: '下一步' }).click();
    const cd = page.getByRole('dialog');
    await cd.getByText('新增的權限').waitFor();
    await cd.getByText(/users:read/).first().waitFor();
    await axe(page, '角色變更確認');
    await cd.getByRole('button', { name: '變更角色' }).click();
    await page.getByText('已變更角色').waitFor();
    await page.locator('ul li', { hasText: '稽核員（全站）' }).first().waitFor();
    // 結束對方所有的工作階段（L1，D25）→ 對方下一個請求回登入頁
    await page.getByRole('button', { name: '結束所有工作階段…' }).click();
    await page.getByRole('dialog').getByText('帳號不會被停用').waitFor();
    await page.getByRole('dialog').getByRole('button', { name: '結束所有工作階段' }).click();
    await page.getByText('已結束 王愛麗 的所有工作階段').waitFor();
    await b.page.getByRole('link', { name: '主機', exact: true }).click();
    await b.page.waitForURL(/reason=session_expired/);
    await b.ctx.close();
    // 停用（L1）→ 啟用
    await page.getByRole('button', { name: '停用帳號…' }).click();
    await page.getByRole('dialog').getByRole('button', { name: '停用帳號', exact: true }).click();
    await page.getByText(/已停用 alice\.w/).waitFor();
    await page.locator('[data-status="offline"]', { hasText: '停用' }).first().waitFor();
    await page.getByRole('button', { name: '啟用帳號' }).click();
    await page.getByText('已啟用 alice.w').waitFor();
    // 自己：不能停用；結束其他工作階段（保留目前這個）
    await page.goto(`${URL0}/admin/users/admin`);
    await waitTitle(page, 'admin');
    const self = page.getByRole('button', { name: '停用帳號…' });
    assert.equal(await self.getAttribute('aria-disabled'), 'true');
    await page.getByText('不能停用或刪除自己的帳號。').waitFor();
    assert.equal(await page.getByRole('button', { name: /刪除使用者/ }).count(), 0);
    // 一次性密碼沒有進入 Kairos 的日誌
    if (process.env.KAIROS_LOG) assert.ok(!readFileSync(process.env.KAIROS_LOG, 'utf8').includes(pw), '一次性密碼不應出現在 Kairos 的日誌');
    await ctx.close();
  });

  test('S16：角色與權限分頁（權限矩陣）', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/admin/roles');
    await waitTitle(page, '使用者與角色');
    await page.locator('table.k-matrix tbody tr').first().waitFor();
    assert.ok((await page.locator('table.k-matrix tbody tr').count()) >= 30);
    await axe(page, '角色與權限');
    await ctx.close();
  });

  test('S21：API token 建立只顯示一次、清單只有前 4 碼、撤銷', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/system/settings/integrations');
    await waitTitle(page, '系統設定');
    await page.getByRole('button', { name: '建立 API token…' }).waitFor();
    await axe(page, '系統設定：整合');
    await page.getByRole('button', { name: '建立 API token…' }).click();
    const dlg = page.getByRole('dialog');
    await dlg.getByLabel(/^名稱/).fill('grafana-e2e');
    await dlg.getByRole('button', { name: '建立 token' }).click();
    const box = page.locator('dialog input[data-secret]');
    await box.waitFor();
    const token = await box.inputValue();
    assert.match(token, /^kat_[0-9a-f]{16}_[A-Za-z0-9_-]{43}$/);
    const hint = `kat_${token.slice(4, 8)}…`;
    await axe(page, 'API token：只顯示一次');
    await page.keyboard.press('Escape');
    assert.equal(await page.locator('dialog[open]').count(), 1);
    await page.getByLabel('我已保存這個 token').check();
    await page.getByRole('dialog').getByRole('button', { name: '關閉', exact: true }).last().click();
    await page.locator('dialog[open]').waitFor({ state: 'detached' });
    const row = page.locator(`tr[data-token="${hint}"]`);
    await row.waitFor();
    assert.ok((await row.innerText()).includes('grafana-e2e'));
    // 畫面、網址、瀏覽器儲存都沒有完整的 token（也沒有完整的 token_id）
    const html = await page.evaluate(() => document.documentElement.outerHTML + Array.from(document.querySelectorAll('input')).map((i) => i.value).join(' '));
    assert.ok(!html.includes(token.slice(4)), 'token 不應留在畫面上');
    assert.ok(!(await storageDump(page)).includes(token.slice(4)));
    // token 可以使用（預設唯讀）
    const use = (tk) => page.evaluate(async (tk2) => (await fetch('/api/v1/me', { headers: { Authorization: `Bearer ${tk2}` } })).status, tk);
    assert.equal(await use(token), 200);
    // 撤銷（L1）
    await row.getByRole('button', { name: /撤銷/ }).click();
    await page.getByRole('dialog').getByText('立即失效').waitFor();
    await page.getByRole('dialog').getByRole('button', { name: '撤銷 token' }).click();
    await page.getByText('已撤銷 API token「grafana-e2e」').waitFor();
    await row.waitFor({ state: 'detached' });
    assert.equal(await use(token), 401);
    await page.getByLabel('顯示已撤銷與已過期').check();
    await page.locator(`tr[data-token="${hint}"]`, { hasText: '已撤銷' }).waitFor();
    if (process.env.KAIROS_LOG) assert.ok(!readFileSync(process.env.KAIROS_LOG, 'utf8').includes(token.slice(21)), 'token 的秘密不應出現在 Kairos 的日誌');
    await ctx.close();
  });

  test('S18：稽核紀錄查詢、詳情、完整性驗證（HMAC 邊界）、JSONL 匯出', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'auditor', PW, '/audit?category=user');
    await waitTitle(page, '稽核日誌');
    await page.locator('tbody tr[data-key]').first().waitFor();
    // 依目標篩選（從使用者詳情的「查看稽核紀錄」帶入）
    await page.goto(`${URL0}/audit?target=alice.w`);
    const created = page.locator('tbody tr[data-key]', { hasText: 'user.create' });
    await created.first().waitFor();
    await axe(page, '稽核日誌');
    await shot(page, 'admin-audit-light');
    await created.first().getByRole('button', { name: /詳情/ }).click();
    const d = page.getByRole('dialog');
    await d.getByText('前一筆的雜湊').waitFor();
    assert.match(await d.innerText(), /[0-9a-f]{64}/);
    await axe(page, '稽核紀錄詳情');
    await page.keyboard.press('Escape');
    await page.locator('dialog[open]').waitFor({ state: 'detached' });
    // 驗證完整性
    await page.getByRole('button', { name: '驗證完整性' }).click();
    await page.getByText(/雜湊鏈完整/).waitFor();
    const txt = await page.locator('.ds-alert--success').innerText();
    assert.match(txt, /已檢查 [\d,]+ 筆/);
    // run.sh 先以 CLI 建立帳號（服務啟動前、沒有 HMAC），之後的紀錄有 HMAC：結果寫出邊界
    assert.match(txt, /HMAC 已驗證 [\d,]+ 筆（自第 \d+ 筆起）；之前的 [1-9][\d,]* 筆/);
    // JSONL 匯出：最後一行是鏈的起訖
    const href = await page.getByRole('link', { name: '匯出 JSONL（可離線驗證）' }).getAttribute('href');
    assert.match(href, /^\/api\/v1\/audit\/export/);
    const lines = (await page.evaluate(async (u) => (await fetch(u)).text(), href)).trim().split('\n');
    const last = JSON.parse(lines[lines.length - 1]);
    assert.equal(last.type, 'chain');
    assert.ok(last.count >= 1 && /^[0-9a-f]{64}$/.test(last.last_hash));
    assert.equal(JSON.parse(lines[0]).type, 'event');
    await ctx.close();
    // 維運者：只看得到自己的操作，沒有驗證與匯出
    const o = await newPage();
    await login(o.page, 'operator', PW, '/audit');
    await waitTitle(o.page, '稽核日誌');
    await o.page.getByText('只顯示你自己的操作').waitFor();
    assert.equal(await o.page.getByRole('button', { name: '驗證完整性' }).count(), 0);
    assert.equal(await o.page.getByRole('link', { name: /匯出/ }).count(), 0);
    await o.ctx.close();
  });

  test('S21：修改設定 → 來源變為主控台 → 改用設定檔的值', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/system/settings/security');
    await waitTitle(page, '系統設定');
    const hours = page.getByLabel('自動暫停前的時數');
    await hours.waitFor();
    assert.equal(await hours.inputValue(), '36');
    await page.getByText('來自 kairos.toml。').first().waitFor();
    await axe(page, '系統設定：連線與安全');
    await shot(page, 'admin-settings-light');
    // 改為開放、自動暫停時數 48（L1）
    await page.getByRole('radio', { name: /^開放/ }).check();
    await hours.fill('48');
    await page.getByText('尚未套用').waitFor();
    await page.getByRole('button', { name: '套用模式…' }).click();
    const d = page.getByRole('dialog');
    await d.getByText('自動暫停的時數：36 → 48 小時').waitFor();
    await d.getByRole('button', { name: '改為開放' }).click();
    await page.getByText('註冊端點已改為「開放」').waitFor();
    await page.getByText(/設定檔中為 36，目前不生效/).waitFor();
    await page.getByText(/此設定由主控台管理，優先於設定檔（user:admin/).first().waitFor();
    assert.equal(await page.getByLabel('自動暫停前的時數').inputValue(), '48');
    // 改用設定檔的值（L1）＝刪除資料庫的值
    await page.getByRole('button', { name: '改用設定檔的值：enroll.auto_pause_after_hours' }).click();
    await page.getByRole('dialog').getByText('改為：36（設定檔）').waitFor();
    await page.getByRole('dialog').getByRole('button', { name: '改用設定檔的值', exact: true }).click();
    await page.getByText('已改用設定檔的值').waitFor();
    await page.locator('input[name="auto_pause_after_hours"]').waitFor();
    await assert.doesNotReject(async () => {
      for (let i = 0; i < 50 && (await page.getByLabel('自動暫停前的時數').inputValue()) !== '36'; i += 1) await page.waitForTimeout(100);
    });
    assert.equal(await page.getByLabel('自動暫停前的時數').inputValue(), '36');
    // 模式也恢復內建預設（關閉）
    await page.getByRole('button', { name: '改用內建預設值：enroll.mode' }).click();
    await page.getByRole('dialog').getByRole('button', { name: '改用內建預設值', exact: true }).click();
    await page.getByText('已改用內建預設值').waitFor();
    await page.getByRole('radio', { name: /^關閉/ }).and(page.locator(':checked')).waitFor();
    const s = await apiCall(page, 'GET', '/api/v1/settings');
    const get = (k) => s.body.data.settings.find((x) => x.key === k);
    assert.deepEqual([get('enroll.auto_pause_after_hours').source, get('enroll.auto_pause_after_hours').value], ['file', 36]);
    assert.equal(get('enroll.mode').source, 'default');
    // 稽核：改用設定檔的值有紀錄
    const a = await apiCall(page, 'GET', '/api/v1/audit?action=setting.reset');
    assert.ok(a.body.data.length >= 2);
    await ctx.close();
  });

  test('S21：環境變數鎖定的設定唯讀（畫面與 API 409）', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/system/settings/security');
    await waitTitle(page, '系統設定');
    const auto = page.getByLabel(/自動暫停新註冊/);
    await auto.waitFor();
    assert.equal(await auto.isDisabled(), true);
    await page.getByText('由環境變數 KAIROS__ENROLL__AUTO_PAUSE_ENABLED 設定，無法在此修改。').waitFor();
    assert.equal(await page.getByRole('button', { name: /enroll\.auto_pause_enabled/ }).count(), 0);
    const r1 = await apiCall(page, 'POST', '/api/v1/settings/reset', { keys: ['enroll.auto_pause_enabled'] });
    assert.equal(r1.status, 409);
    assert.equal(r1.body.error.code, 'locked_by_environment');
    const r2 = await apiCall(page, 'PUT', '/api/v1/enroll/mode', { auto_pause_enabled: false });
    assert.equal(r2.status, 409);
    assert.equal(r2.body.error.details.variable, 'KAIROS__ENROLL__AUTO_PAUSE_ENABLED');
    await ctx.close();
  });

  test('權限：稽核員看得到稽核與使用者但沒有寫入按鈕；唯讀看不到管理選單、直接呼叫 403；維運者只有「連線與安全」', async () => {
    const a = await newPage();
    await login(a.page, 'auditor', PW, '/admin/users');
    await waitTitle(a.page, '使用者與角色');
    const labels = await navLabels(a.page);
    for (const l of ['稽核日誌', '使用者與角色', '目錄服務', '系統健康', '系統設定']) assert.ok(labels.includes(l), `稽核員的選單應有 ${l}：${labels}`);
    await a.page.locator('tbody tr[data-key="admin"]').waitFor();
    assert.equal(await a.page.getByRole('button', { name: '新增使用者…' }).count(), 0);
    await a.page.goto(`${URL0}/admin/users/viewer`);
    await waitTitle(a.page, 'viewer');
    for (const name of ['編輯角色…', '停用帳號…', '重設密碼…', '結束所有工作階段…', '編輯資料…']) assert.equal(await a.page.getByRole('button', { name }).count(), 0, name);
    await a.page.goto(`${URL0}/system/settings/integrations`);
    await a.page.getByText('唯讀：需要管理員權限才能修改').waitFor();
    await a.page.locator('#api-tokens, .ds-empty').first().waitFor();
    assert.equal(await a.page.getByRole('button', { name: '建立 API token…' }).count(), 0);
    assert.equal(await a.page.getByRole('button', { name: /撤銷/ }).count(), 0);
    const w = await apiCall(a.page, 'POST', '/api/v1/users', { username: 'nope' });
    assert.equal(w.status, 403);
    await a.ctx.close();
    // 唯讀
    const v = await newPage();
    await login(v.page, 'viewer');
    await waitTitle(v.page, '總覽');
    assert.deepEqual(await navLabels(v.page), ['總覽', '主機', '系統健康']);
    for (const p of ['/admin/users', '/admin/directory', '/audit', '/system/settings']) {
      await v.page.goto(`${URL0}${p}`);
      await v.page.locator('#page-title', { hasText: '沒有權限檢視這個畫面' }).waitFor();
    }
    for (const p of ['/api/v1/users', '/api/v1/audit', '/api/v1/api-tokens', '/api/v1/settings', '/api/v1/directory']) {
      const r = await apiCall(v.page, 'GET', p);
      assert.equal(r.status, 403, p);
      assert.equal(r.body.error.code, 'forbidden');
    }
    // 系統健康：只看整體與各項狀態，不看數值
    await v.page.goto(`${URL0}/system/health`);
    await waitTitle(v.page, '系統健康');
    await v.page.locator('[data-item="database"]').waitFor();
    await v.page.getByText('數值細節需要系統設定的檢視權限').waitFor();
    await axe(v.page, '系統健康（唯讀）');
    await v.ctx.close();
    // 維運者：設定只有「連線與安全」、唯讀
    const o = await newPage();
    await login(o.page, 'operator', PW, '/system/settings');
    await waitTitle(o.page, '系統設定');
    assert.deepEqual(await o.page.locator('.ds-tabs a').allTextContents(), ['連線與安全']);
    await o.page.getByText('唯讀：需要管理員權限才能修改').waitFor();
    await o.page.getByLabel('自動暫停前的時數').waitFor();
    assert.equal(await o.page.getByRole('button', { name: '套用模式…' }).count(), 0);
    await o.page.goto(`${URL0}/system/settings/integrations`);
    await o.page.getByText('這個分頁只對管理員與稽核員開放。').waitFor();
    await o.ctx.close();
  });

  test('XSS：使用者的顯示名稱含 <script> 以文字顯示', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/admin/users');
    await waitTitle(page, '使用者與角色');
    const r = await apiCall(page, 'POST', '/api/v1/users', { username: 'xss.user', display_name: XSS_NAME, roles: ['viewer'] });
    assert.equal(r.status, 201);
    await page.goto(`${URL0}/admin/users?q=xss`);
    const link = page.locator('a.k-host-name', { hasText: '<script>' });
    await link.waitFor();
    assert.equal(await link.textContent(), XSS_NAME);
    await link.click();
    await page.locator('#page-title', { hasText: '<script>' }).waitFor();
    assert.equal(await page.locator('#page-title').textContent(), XSS_NAME);
    // 稽核紀錄的細節（JSON）也以文字顯示
    await page.goto(`${URL0}/audit?target=xss.user`);
    await page.locator('tbody tr[data-key]', { hasText: 'user.create' }).first().getByRole('button', { name: /詳情/ }).click();
    await page.getByRole('dialog').locator('pre', { hasText: '<script>' }).waitFor();
    const flags = await page.evaluate(() => ({ xss: window.__xss, imgs: document.querySelectorAll('img[src="x"]').length, scripts: Array.from(document.scripts).filter((s) => s.textContent.includes('__xss')).length }));
    assert.deepEqual(flags, { xss: undefined, imgs: 0, scripts: 0 });
    await ctx.close();
  });

  test('S17 目錄服務與 S20 系統健康：唯讀連線設定、群組對應、診斷包指令', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/admin/directory');
    await waitTitle(page, '目錄服務');
    await page.getByText(/此設定在 kairos\.toml 的 \[ldap\]/).waitFor();
    // 區塊 3「同步」隱藏、連線設定沒有編輯按鈕
    assert.equal(await page.getByRole('heading', { name: '同步' }).count(), 0);
    assert.equal(await page.getByRole('button', { name: /上傳|更換|備援/ }).count(), 0);
    await page.getByRole('button', { name: '新增對應…' }).click();
    await page.getByRole('dialog').getByLabel(/AD 群組/).fill('CN=IT-Ops,OU=Groups,DC=example,DC=gov,DC=tw');
    await page.getByRole('dialog').getByLabel('角色').selectOption('operator');
    await page.getByRole('dialog').getByRole('button', { name: '下一步' }).click();
    await page.getByRole('dialog').getByRole('button', { name: '新增對應' }).click();
    await page.getByText('已更新群組對應').waitFor();
    const row = page.locator('tr', { hasText: 'cn=it-ops' });
    await row.waitFor();
    await axe(page, '目錄服務');
    await row.getByRole('button', { name: /移除/ }).click();
    await page.getByRole('dialog').getByRole('button', { name: '移除對應' }).click();
    await row.waitFor({ state: 'detached' });
    // 系統健康
    await page.goto(`${URL0}/system/health`);
    await waitTitle(page, '系統健康');
    for (const id of ['database', 'connections', 'tls', 'alerts', 'metrics']) await page.locator(`[data-item="${id}"]`).waitFor();
    assert.equal(await page.getByRole('button', { name: '下載診斷包' }).count(), 0);
    assert.equal(await page.locator('[data-item="directory"]').count(), 0);
    await page.getByText('如何產生診斷包').click();
    await page.getByText(/kairos diag --config/).waitFor();
    await axe(page, '系統健康');
    await shot(page, 'admin-health-light');
    // 頂列的系統狀態燈連到這裡
    assert.equal(await page.locator('a.k-status-light').getAttribute('href'), '/system/health');
    await ctx.close();
  });

  test('管理畫面的深色主題：axe 0 違規', async () => {
    const { ctx, page } = await newPage({ colorScheme: 'dark' });
    await login(page, 'admin', PW, '/admin/users');
    await waitTitle(page, '使用者與角色');
    await page.locator('tbody tr[data-key]').first().waitFor();
    await axe(page, '使用者清單（深色）');
    await page.goto(`${URL0}/admin/users/alice.w`);
    await waitTitle(page, '王愛麗');
    await axe(page, '使用者詳情（深色）');
    await page.goto(`${URL0}/admin/roles`);
    await page.locator('table.k-matrix').waitFor();
    await axe(page, '角色與權限（深色）');
    await page.goto(`${URL0}/audit`);
    await page.locator('tbody tr[data-key]').first().waitFor();
    await axe(page, '稽核日誌（深色）');
    await shot(page, 'admin-audit-dark');
    await page.goto(`${URL0}/system/health`);
    await page.locator('[data-item="database"]').waitFor();
    await axe(page, '系統健康（深色）');
    await shot(page, 'admin-health-dark');
    for (const tab of ['general', 'security', 'storage', 'integrations']) {
      await page.goto(`${URL0}/system/settings/${tab}`);
      await page.locator('.k-settings-table, #api-tokens').first().waitFor();
      await axe(page, `系統設定：${tab}（深色）`);
    }
    await shot(page, 'admin-settings-dark');
    await page.goto(`${URL0}/admin/directory`);
    await waitTitle(page, '目錄服務');
    await axe(page, '目錄服務（深色）');
    assert.ok(alicePw === null || typeof alicePw === 'string');
    await ctx.close();
  });
}
