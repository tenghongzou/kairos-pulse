// 告警畫面（S06～S11）的端對端測試；由 e2e.mjs 註冊（共用瀏覽器、登入、axe 等工具）。
//
// 流程：規則編輯（S09，主機離線、寬限 0）→ 假 agent 中斷 → 事件經 SSE 出現在清單（S06）→ 維運者確認 → 詳情（S07）註解、解決；
// 靜音（S10）顯示符合台數、超過上限被伺服器拒絕；Webhook 管道（S11）以本機接收器測試通知、機密遮蔽、429 限速；
// 唯讀角色看不到寫入按鈕、直接呼叫 API 回 403；XSS（主機名稱含 <script> 的事件標題）；亮暗主題的 axe。

import http from 'node:http';
import assert from 'node:assert/strict';

const TAG = 'alert-e2e';
const RULE = 'E2E 主機離線（寬限 0）';
const XSS_HOST = '<script>window.__xss=3</script><img src=x onerror="window.__xss=4">alert-x';

export function register({ test, newPage, login, waitTitle, axe, shot, FakeAgent, URL0, PW }) {
  const port = Number(process.env.AGENT_PORT);
  const state = { agents: [] };

  async function connect(agentId, hostname) {
    const a = new FakeAgent({ port, agentId, hostname, tags: [TAG], backfillMinutes: 0 });
    await a.connect();
    state.agents.push(a);
    return a;
  }

  /** 以目前登入的工作階段呼叫 API（帶 CSRF） */
  async function apiCall(page, method, path, body) {
    return page.evaluate(
      async ({ method, path, body }) => {
        const me = await (await fetch('/api/v1/me')).json();
        const r = await fetch(path, {
          method,
          headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': me.data.csrf_token },
          body: body ? JSON.stringify(body) : undefined,
        });
        let json = null;
        try {
          json = await r.json();
        } catch {
          json = null;
        }
        return { status: r.status, json };
      },
      { method, path, body },
    );
  }

  /** 確保有「標籤 alert-e2e、寬限 0」的主機離線規則（單獨執行某個測試時） */
  async function ensureRule(page) {
    const r = await apiCall(page, 'GET', '/api/v1/alert-rules');
    if (r.json.data.some((x) => x.name === RULE)) return;
    const c = await apiCall(page, 'POST', '/api/v1/alert-rules', { kind: 'host_offline', name: RULE, severity: 'critical', params: { grace_secs: 0 }, scope: { tags: [TAG] } });
    assert.equal(c.status, 201, JSON.stringify(c.json));
  }

  /** 等事件的列出現：清單原本是空的會自動重新載入；否則按「更新列表」 */
  async function waitEventRow(page, text, timeout = 40000) {
    const row = page.locator('tbody tr', { hasText: text }).first();
    const refresh = page.getByRole('button', { name: '更新列表' });
    const deadline = Date.now() + timeout;
    let reloadAt = Date.now() + 20000;
    while (Date.now() < deadline) {
      if (await row.isVisible().catch(() => false)) {
        console.log(`     （事件出現：${Math.round((Date.now() - (deadline - timeout)) / 100) / 10} 秒${reloadAt === Infinity ? '，重新載入後' : ''}）`);
        return row;
      }
      if (await refresh.isVisible().catch(() => false)) await refresh.click();
      // SSE 尚未連上時事件可能已經送出：20 秒後重新載入一次
      if (Date.now() > reloadAt) {
        reloadAt = Infinity;
        await page.reload();
      }
      await page.waitForTimeout(500);
    }
    throw new Error(`事件「${text}」沒有出現在清單`);
  }

  function localInput(ms) {
    const p = Object.fromEntries(new Intl.DateTimeFormat('en-US', { timeZone: 'Asia/Taipei', year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }).formatToParts(new Date(ms)).map((x) => [x.type, x.value]));
    return `${p.year}-${p.month}-${p.day}T${p.hour}:${p.minute}`;
  }

  test('告警規則（S09）：建立主機離線規則、離線類不提供歷史試算、恢復門檻就地檢查', async () => {
    await connect('22222222-2222-4222-8222-000000000001', 'alert-e2e-01');
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/alerts/rules');
    await waitTitle(page, '告警規則');
    await page.getByRole('link', { name: '主機離線', exact: true }).first().waitFor();
    await axe(page, '告警規則');
    await shot(page, 'alert-rules-light');
    await page.getByRole('link', { name: '新增規則' }).first().click();
    await waitTitle(page, '新增告警規則');
    // 指標門檻：恢復門檻在觸發門檻的另一側 → 就地錯誤
    await page.getByLabel('觸發門檻（%）').fill('80');
    await page.getByLabel('恢復門檻（%）').fill('90');
    await page.getByLabel(/^名稱/).fill('E2E CPU');
    await page.getByRole('button', { name: '建立規則' }).click();
    await page.getByText('恢復門檻不可高於觸發門檻', { exact: false }).first().waitFor();
    await axe(page, '告警規則編輯');
    // 未儲存的變更：站內換頁先詢問（router 的 setLeaveGuard）；取消後留在原頁、內容不變
    await page.locator('.ds-sidenav__link', { hasText: '告警事件' }).click();
    await page.getByRole('dialog').getByText('要放棄變更嗎？').waitFor();
    await axe(page, '未儲存變更的詢問');
    await page.getByRole('dialog').getByRole('button', { name: '取消' }).click();
    await page.getByRole('dialog').waitFor({ state: 'hidden' });
    assert.equal(new URL(page.url()).pathname, '/alerts/rules/new');
    assert.equal(await page.getByLabel(/^名稱/).inputValue(), 'E2E CPU');
    // 改為主機離線、寬限 0 分鐘、範圍為標籤 alert-e2e
    await page.getByRole('radio', { name: '主機離線', exact: true }).check();
    await page.getByText('主機離線類規則目前無法以歷史資料試算').waitFor({ timeout: 15000 });
    await page.getByLabel('離線持續（分鐘）後通知').fill('0');
    await page.getByLabel('標籤').fill(TAG);
    await page.getByText(/目前符合 [1-9]\d* 台主機/).waitFor({ timeout: 15000 });
    await page.getByRole('radio', { name: /^嚴重/ }).check();
    await page.getByLabel(/^名稱/).fill(RULE);
    await shot(page, 'alert-rule-edit-light');
    await page.getByRole('button', { name: '建立規則' }).click();
    await waitTitle(page, '告警規則');
    await page.getByRole('link', { name: RULE }).waitFor();
    // 停用系統預設規則需要原因（L1）：取消
    await page.getByRole('switch', { name: '啟用規則 主機長期失聯' }).click();
    await page.getByRole('dialog').getByText('停用必須填寫原因').waitFor();
    await page.getByRole('dialog').getByRole('button', { name: '停用規則' }).click();
    await page.getByText('請填寫停用的原因。').waitFor();
    await page.getByRole('dialog').getByRole('button', { name: '取消' }).click();
    assert.equal(await page.getByRole('switch', { name: '啟用規則 主機長期失聯' }).getAttribute('aria-checked'), 'true');
    // 0.10.1（H1）：「雙人覆核已關閉」不可停用——沒有開關，只有狀態；API 回 409。不可靜音——靜音表單不能勾選，API 回 400
    const lockedRow = page.locator('tr[data-key="sys.rollout_approval_disabled"]');
    await lockedRow.getByText('啟用（不可停用）').waitFor();
    assert.equal(await lockedRow.getByRole('switch').count(), 0, '不可停用的規則沒有開關');
    await lockedRow.getByText('不可停用、不可靜音').waitFor();
    await page.locator('tr[data-key="sys.unauthenticated"]').getByText('不可靜音', { exact: true }).waitFor();
    assert.equal(await page.locator('tr[data-key="sys.unauthenticated"]').getByRole('switch').count(), 1, '未認證連線的規則仍可停用');
    await axe(page, '告警規則（不可停用的系統規則）');
    const denied = await apiCall(page, 'POST', '/api/v1/alert-rules/sys.rollout_approval_disabled/disable', { reason: 'e2e' });
    assert.equal(denied.status, 409);
    assert.equal(denied.json.error.code, 'rule_always_enabled');
    const noSilence = await apiCall(page, 'POST', '/api/v1/silences', { kind: 'silence', name: 'e2e 預先靜音', reason: 'e2e', rules: ['sys.rollout_approval_disabled'], duration_secs: 600 });
    assert.equal(noSilence.status, 400);
    assert.equal(noSilence.json.error.code, 'rule_unsilenceable');
    await page.goto(`${URL0}/alerts/silences/new?type=silence&rules=sys.rollout_approval_disabled,sys.host_long_offline`);
    const box = page.getByRole('checkbox', { name: /雙人覆核已關閉/ });
    await box.waitFor();
    assert.equal(await box.isDisabled(), true, '不可靜音的規則不能勾選');
    assert.equal(await box.isChecked(), false, '網址帶進來的預選也拿掉');
    assert.equal(await page.getByRole('checkbox', { name: /主機長期失聯/ }).isChecked(), true);
    await page.getByText('（不可靜音）').first().waitFor();
    await axe(page, '新增靜音（不可靜音的規則）');
    await ctx.close();
  });

  test('告警事件：Pulse 停止 → 事件經 SSE 出現在清單 → 維運者確認 → 詳情註解、解決', async () => {
    const agent = state.agents.find((a) => a.hostname === 'alert-e2e-01') || (await connect('22222222-2222-4222-8222-000000000001', 'alert-e2e-01'));
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/alerts/rules');
    await waitTitle(page, '告警規則');
    await ensureRule(page);
    await ctx.close();
    const o = await newPage();
    const p = o.page;
    await login(p, 'operator', PW, '/alerts/events');
    await waitTitle(p, '告警事件');
    await p.locator('tbody tr, .ds-empty').first().waitFor();
    agent.close();
    const row = await waitEventRow(p, '主機離線：alert-e2e-01');
    await row.locator('[data-status="critical"]').waitFor();
    // 側邊導覽的計數泡泡
    await p.locator('.ds-sidenav__link', { hasText: '告警事件' }).locator('.ds-sidenav__count').waitFor({ timeout: 15000 });
    // 分頁標題的未確認數與計數泡泡相同（ia.md §7：「(3) Kairos｜告警事件」）
    await p.waitForFunction(() => {
      const b = document.querySelector('.ds-sidenav__count');
      return b && document.title === `(${b.textContent.trim()}) Kairos｜告警事件`;
    }, null, { timeout: 15000 });
    await axe(p, '告警事件');
    await shot(p, 'alert-events-light');
    // 確認（行內按鈕 → 預計處理時間）
    await row.getByRole('button', { name: /^確認事件 主機離線：alert-e2e-01/ }).click();
    await p.getByRole('dialog').getByText('預計處理時間').waitFor();
    await axe(p, '告警事件：確認');
    await p.getByRole('dialog').getByRole('button', { name: '確認事件', exact: true }).click();
    await p.getByText(/^已確認事件 E-/).first().waitFor();
    await p.locator('tbody tr', { hasText: '主機離線：alert-e2e-01' }).first().getByText('已確認').waitFor();
    // 詳情
    await p.getByRole('link', { name: '主機離線：alert-e2e-01' }).first().click();
    await waitTitle(p, '主機離線：alert-e2e-01');
    await p.locator('#ev-timeline').getByText('已確認').first().waitFor();
    await p.getByText('負責人：operator').first().waitFor();
    await p.getByLabel('新增註解').fill('已聯絡廠務');
    await p.getByRole('button', { name: '送出註解' }).click();
    await p.locator('#ev-timeline').getByText('已聯絡廠務').waitFor();
    await axe(p, '告警事件詳情');
    await shot(p, 'alert-event-light');
    await p.getByRole('button', { name: '解決…' }).click();
    await p.getByRole('dialog').getByRole('button', { name: '解決事件' }).click();
    await p.getByText('請選擇根因類別。').waitFor();
    await p.getByRole('dialog').getByLabel(/根因類別/).selectOption('network');
    await p.getByRole('dialog').getByRole('button', { name: '解決事件' }).click();
    await p.getByText('事件已解決', { exact: true }).waitFor();
    await p.getByText(/根因：網路/).first().waitFor();
    // 已解決：只剩註解
    assert.equal(await p.getByRole('button', { name: '解決…' }).count(), 0);
    await p.getByLabel('新增註解').waitFor();
    await o.ctx.close();
  });

  test('XSS：主機名稱含 <script> 的事件標題在清單、詳情、全域搜尋都以文字顯示', async () => {
    const a = await connect('22222222-2222-4222-8222-000000000003', XSS_HOST);
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/alerts/events');
    await waitTitle(page, '告警事件');
    await ensureRule(page);
    await page.locator('tbody tr, .ds-empty').first().waitFor();
    a.close();
    const row = await waitEventRow(page, '<script>window.__xss=3');
    const link = row.locator('a.k-event-link');
    assert.equal(await link.textContent(), `主機離線：${XSS_HOST}`);
    await link.click();
    await page.locator('#page-title', { hasText: '<script>' }).waitFor();
    assert.equal(await page.locator('#page-title').textContent(), `主機離線：${XSS_HOST}`);
    await page.keyboard.press('Control+k');
    await page.getByRole('combobox').fill('<img');
    await page.getByRole('option', { name: /<img src=x/ }).first().waitFor();
    const flags = await page.evaluate(() => ({ xss: window.__xss, imgs: document.querySelectorAll('img[src="x"]').length }));
    assert.deepEqual(flags, { xss: undefined, imgs: 0 });
    await ctx.close();
  });

  test('靜音（S10）：顯示符合台數、超過上限被伺服器拒絕、建立 1 小時的靜音', async () => {
    if (!state.agents.some((a) => a.hostname === 'alert-e2e-02')) await connect('22222222-2222-4222-8222-000000000002', 'alert-e2e-02');
    const { ctx, page } = await newPage();
    await login(page, 'operator', PW, '/alerts/silences');
    await waitTitle(page, '靜音與維護時段');
    await page.locator('.ds-empty, tbody tr').first().waitFor();
    await axe(page, '靜音與維護時段');
    await page.getByRole('button', { name: '臨時靜音' }).click();
    const dlg = page.getByRole('dialog');
    await dlg.getByLabel(/^名稱/).fill('E2E 備份作業');
    await dlg.getByLabel('標籤').fill(TAG);
    const matching = dlg.locator('[data-matching]');
    await matching.waitFor({ timeout: 15000 });
    assert.ok(Number(await matching.getAttribute('data-matching')) >= 1, '應有符合的主機');
    await dlg.getByText(/目前符合 \d+ 台主機/).waitFor();
    await dlg.getByText(/臨時靜音最長 4 小時/).waitFor();
    // 「到明天 08:00」超過 4 小時時停用（aria-disabled）
    await axe(page, '靜音：新增');
    await shot(page, 'alert-silence-form-light');
    // 自訂結束時間：8 天後 → 伺服器拒絕（silence_too_long）
    await dlg.getByRole('radio', { name: '自訂結束時間' }).check();
    await dlg.getByLabel(/^結束（/).fill(localInput(Date.now() + 8 * 86400000));
    await dlg.getByLabel(/^原因/).fill('E2E：測試時長上限');
    await page.locator('dialog[open] .ds-dialog__foot').getByRole('button', { name: '建立' }).click();
    // 包含緊急等級的規則（不選＝所有規則）→ L1 確認
    const confirmDlg = page.locator('dialog[open]').last();
    await confirmDlg.getByText('此靜音包含緊急等級事件', { exact: false }).waitFor();
    await confirmDlg.getByRole('button', { name: '建立' }).click();
    await dlg.getByText(/超過上限：最長 4 小時/).waitFor();
    // 改為 1 小時
    await dlg.getByRole('radio', { name: '1 小時' }).check();
    await page.locator('dialog[open] .ds-dialog__foot').getByRole('button', { name: '建立' }).click();
    await page.locator('dialog[open]').last().getByRole('button', { name: '建立' }).click();
    await page.getByText('已建立「E2E 備份作業」').waitFor();
    await page.locator('tbody tr', { hasText: 'E2E 備份作業' }).waitFor();
    await page.getByText(/目前有 \d+ 個靜音或維護時段生效/).waitFor();
    await shot(page, 'alert-silences-light');
    await ctx.close();
  });

  test('通知管道（S11）：建立 Webhook、以本機接收器測試通知、機密遮蔽、測試通知限速（429）', async () => {
    const got = [];
    const server = http.createServer((req, res) => {
      let body = '';
      req.on('data', (c) => (body += c));
      req.on('end', () => {
        got.push({ url: req.url, headers: req.headers, body });
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end('{"ok":true}');
      });
    });
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const hook = `http://127.0.0.1:${server.address().port}/hook/secret-token-123`;
    try {
      const { ctx, page } = await newPage();
      await login(page, 'admin', PW, '/alerts/channels');
      await waitTitle(page, '通知管道');
      await page.locator('.ds-empty, tbody tr').first().waitFor();
      await axe(page, '通知管道');
      await page.getByRole('button', { name: '新增管道' }).click();
      await page.getByRole('menuitem', { name: '新增 Webhook 管道' }).click();
      await waitTitle(page, '新增 Webhook 管道');
      await page.getByLabel(/^名稱/).fill('E2E Webhook');
      await page.getByLabel('Webhook 網址').fill(hook);
      // http 未允許：測試時欄位就地錯誤
      await page.getByRole('button', { name: '寄送測試通知' }).click();
      await page.getByText(/必須是 https/).first().waitFor();
      await page.getByLabel('允許未加密的 HTTP').check();
      await page.getByRole('button', { name: '寄送測試通知' }).click();
      await page.getByText('已送達').waitFor({ timeout: 20000 });
      assert.equal(got.length, 1, '接收器應收到 1 則測試通知');
      assert.match(got[0].body, /kairos/);
      await axe(page, '通知管道：新增 Webhook');
      await page.getByRole('button', { name: '建立管道' }).click();
      await page.getByText('已建立通知管道「E2E Webhook」').waitFor();
      await waitTitle(page, 'E2E Webhook');
      // 機密只顯示遮蔽值（畫面與 API 回應都沒有路徑中的 token）
      const shown = await page.locator('input[readonly]').first().inputValue();
      assert.ok(!shown.includes('secret-token-123'), `畫面顯示了機密：${shown}`);
      const id = page.url().split('/').pop();
      const api = await page.evaluate(async (id) => (await fetch(`/api/v1/channels/${id}`)).text(), id);
      assert.ok(!api.includes('secret-token-123'), 'API 回應含機密');
      await page.getByRole('button', { name: '更換Webhook 網址' }).waitFor();
      await axe(page, '通知管道：設定與狀態');
      await shot(page, 'alert-channel-light');
      // 已儲存的管道：60 秒內 3 次，第 4 次 429
      for (let i = 0; i < 3; i += 1) {
        await page.getByRole('button', { name: '寄送測試通知' }).click();
        await page.locator('.k-test-out').getByText('已送達').waitFor({ timeout: 20000 });
        await page.locator('#ch-test:not([aria-busy])').waitFor();
      }
      await page.getByRole('button', { name: '寄送測試通知' }).click();
      // 按鈕下方就地顯示 KRS-W5111 與倒數；上一次的結果保留；倒數期間 aria-disabled（screens.md S11、範例 93）
      await page.getByText('測試通知太頻繁，請稍後再試').waitFor({ timeout: 20000 });
      await page.getByText(/同一個管道 60 秒內最多 3 次/).waitFor();
      await page.locator('.k-test-limit').getByText('KRS-W5111').waitFor();
      await page.locator('.k-test-out').getByText('已送達').waitFor();
      const testBtn = page.locator('#ch-test');
      assert.equal(await testBtn.getAttribute('aria-disabled'), 'true');
      const describedBy = await testBtn.getAttribute('aria-describedby');
      assert.ok(describedBy && (await page.locator(`#${describedBy}`).count()) === 1, '按鈕要以 aria-describedby 指向限速說明');
      const secs = async () => Number(await page.locator('.k-test-limit .k-countdown').textContent());
      const s1 = await secs();
      await page.waitForTimeout(2100);
      assert.ok((await secs()) < s1, '剩餘秒數應每秒倒數');
      // Playwright 把 aria-disabled 視為不可點選：強制點選，模擬使用者仍按得到（不送出，只朗讀原因）
      await testBtn.click({ force: true });
      await page.waitForTimeout(500);
      assert.equal(got.length, 4, '限速的請求與倒數期間的點選都不應送出');
      await axe(page, '通知管道：測試通知限速');
      // 維運者：只能看與寄送測試（沒有新增、儲存、刪除）
      const o = await newPage();
      await login(o.page, 'operator', PW, `/alerts/channels/${id}`);
      await waitTitle(o.page, 'E2E Webhook');
      assert.equal(await o.page.getByRole('button', { name: '儲存' }).count(), 0);
      assert.equal(await o.page.getByRole('button', { name: '刪除管道…' }).count(), 0);
      await o.page.getByRole('button', { name: '寄送測試通知' }).waitFor();
      await o.ctx.close();
      await ctx.close();
    } finally {
      server.close();
    }
  });

  test('告警權限：唯讀看不到寫入按鈕、規則與管道 403、直接呼叫寫入 API 回 403', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'viewer', PW, '/alerts/events?view=all');
    await waitTitle(page, '告警事件');
    await page.locator('tbody tr, .ds-empty').first().waitFor();
    const labels = await page.locator('.ds-sidenav__link .ds-sidenav__label').allTextContents();
    assert.ok(labels.includes('告警事件') && labels.includes('靜音與維護'), labels.join('、'));
    assert.ok(!labels.includes('告警規則') && !labels.includes('通知管道'), labels.join('、'));
    assert.equal(await page.getByRole('button', { name: /^確認事件/ }).count(), 0);
    assert.equal(await page.locator('tbody input[type="checkbox"]').count(), 0);
    const first = page.locator('a.k-event-link').first();
    if (await first.count()) {
      await first.click();
      await page.locator('#ev-timeline, .k-timeline, .ds-card').first().waitFor();
      assert.equal(await page.getByRole('button', { name: /確認並指派給我|解決…|指派給…/ }).count(), 0);
      assert.equal(await page.getByLabel('新增註解').count(), 0);
    }
    await page.goto(`${URL0}/alerts/rules`);
    await page.locator('#page-title', { hasText: '沒有權限檢視這個畫面' }).waitFor();
    await page.goto(`${URL0}/alerts/channels`);
    await page.locator('#page-title', { hasText: '沒有權限檢視這個畫面' }).waitFor();
    await page.goto(`${URL0}/alerts/silences`);
    await waitTitle(page, '靜音與維護時段');
    assert.equal(await page.getByRole('button', { name: '臨時靜音' }).count(), 0);
    await page.getByText('唯讀', { exact: true }).waitFor();
    await axe(page, '靜音與維護時段（唯讀）');
    for (const [method, path, body] of [
      ['POST', '/api/v1/alerts/ack', { event_ids: ['E-00000000-0001'] }],
      ['POST', '/api/v1/silences', { name: 'x', reason: 'x', duration_secs: 60 }],
      ['GET', '/api/v1/alert-rules'],
      ['GET', '/api/v1/channels'],
    ]) {
      const r = await apiCall(page, method, path, body);
      assert.equal(r.status, 403, `${method} ${path} → ${r.status}`);
      assert.equal(r.json.error.code, 'forbidden');
    }
    await ctx.close();
    // 稽核員：規則唯讀（沒有新增、開關是文字）
    const a = await newPage();
    await login(a.page, 'auditor', PW, '/alerts/rules');
    await waitTitle(a.page, '告警規則');
    await a.page.getByRole('link', { name: '主機離線', exact: true }).first().waitFor();
    assert.equal(await a.page.getByRole('link', { name: '新增規則' }).count(), 0);
    assert.equal(await a.page.getByRole('switch').count(), 0);
    await a.ctx.close();
  });

  test('告警畫面（深色主題）：axe 0 違規', async () => {
    const { ctx, page } = await newPage({ colorScheme: 'dark' });
    await login(page, 'admin', PW, '/alerts/events?view=all');
    await waitTitle(page, '告警事件');
    await page.locator('tbody tr, .ds-empty').first().waitFor();
    await axe(page, '告警事件（深色）');
    await shot(page, 'alert-events-dark');
    const link = page.locator('a.k-event-link').first();
    if (await link.count()) {
      await link.click();
      await page.locator('#ev-timeline, .ds-card').first().waitFor();
      await axe(page, '告警事件詳情（深色）');
    }
    for (const [path, title, label] of [
      ['/alerts/rules', '告警規則', '告警規則（深色）'],
      ['/alerts/rules/new', '新增告警規則', '告警規則編輯（深色）'],
      ['/alerts/silences', '靜音與維護時段', '靜音與維護時段（深色）'],
      ['/alerts/channels', '通知管道', '通知管道（深色）'],
      ['/', '總覽', '總覽：告警（深色）'],
    ]) {
      await page.goto(`${URL0}${path}`);
      await waitTitle(page, title);
      await page.waitForTimeout(800);
      await axe(page, label);
    }
    // 窄螢幕：事件清單無水平溢出（表格在捲動容器內）
    const m = await newPage({ viewport: { width: 375, height: 800 } });
    await login(m.page, 'operator', PW, '/alerts/events?view=all');
    await waitTitle(m.page, '告警事件');
    const px = await m.page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    assert.ok(px <= 0, `窄螢幕水平溢出 ${px}px`);
    await m.ctx.close();
    await ctx.close();
  });

  return {
    cleanup() {
      for (const a of state.agents) a.close();
    },
  };
}
