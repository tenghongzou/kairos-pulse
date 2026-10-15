// 指標擴充（F4；設計包 decisions/INTEGRATION.md §30、kairos/screens.md S03／S04／S05／S09）的端對端測試；由
// e2e.mjs 註冊。web-01 是模擬 Pulse 0.8.0 的假 agent（新指標、行程排行、資產），db-02 是 0.6.0 的 Windows（舊版）。
//
// 涵蓋：目前數值（置換空間、負載、磁碟＋展開、運作時間、行程）、趨勢的六個指標群組（小倍數、裝置選單、參考線、
// 表格檢視、鍵盤逐點）、行程分頁（兩張表、aria-sort、命令列的敏感資訊提示、歷史瀏覽、超出保留期限）、
// 資產資訊分頁（序號、磁碟、網路介面、變更歷史）、舊版 Pulse 與 Windows 的空狀態、S04 的磁碟欄、作業系統篩選與
// 欄位設定、S03 的磁碟排行、S09 的新指標與範圍、S08 的範本；每個畫面 axe 0 違規（含深色）。

import assert from 'node:assert/strict';

const WEB = '11111111-1111-4111-8111-000000000001';
const DB = '11111111-1111-4111-8111-000000000002';

export function registerMetrics({ test, newPage, login, waitTitle, axe, shot, URL0, PW }) {
  test('F4 S05 概覽：目前數值（置換空間、負載、磁碟＋展開、運作時間、行程）', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'viewer', PW, `/hosts/${WEB}`);
    await waitTitle(page, 'web-01');
    const values = page.locator('section', { has: page.getByRole('heading', { name: '目前數值' }) });
    await values.getByText('置換空間').waitFor();
    await values.getByText(/負載（1／5／15 分鐘）/).waitFor();
    await values.getByText(/1\.25／0\.98／0\.75/).waitFor();
    await values.getByText(/每核心 0\.12，8 個邏輯處理器/).waitFor();
    // 使用率最高的掛載點：/var/log（將滿）＋「＋3 個掛載點」展開
    await values.locator('.k-top-mount', { hasText: /\/var\/log 9\d\.\d%/ }).waitFor();
    await values.getByText('將滿').first().waitFor();
    const more = values.getByRole('button', { name: '＋3 個掛載點' });
    assert.equal(await more.getAttribute('aria-expanded'), 'false');
    await more.click();
    const less = values.getByRole('button', { name: '收合掛載點清單' });
    assert.equal(await less.getAttribute('aria-expanded'), 'true');
    await values.locator('.k-mount-list li', { hasText: '/data' }).waitFor();
    await values.getByText(/開機於 /).waitFor();
    await values.getByText('214 個').waitFor();
    await axe(page, 'F4 主機詳情：概覽（0.8）');
    await shot(page, 'f4-host-overview');
    // 行程排行的連結
    await values.getByRole('link', { name: '查看行程排行' }).click();
    await page.waitForURL(/\/processes$/);
    await ctx.close();
  });

  test('F4 S05 趨勢：六個指標群組、小倍數、裝置選單、參考線、表格與鍵盤', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'viewer', PW, `/hosts/${WEB}/metrics`);
    await waitTitle(page, 'web-01');
    const groups = page.getByRole('group', { name: '指標群組' });
    await groups.getByRole('button', { name: 'CPU 與記憶體' }).waitFor();
    assert.equal(await groups.getByRole('button', { name: 'CPU 與記憶體' }).getAttribute('aria-pressed'), 'true');
    await page.locator('svg.k-chart').first().waitFor({ timeout: 15000 });
    // 磁碟使用率：4 個掛載點 → 小倍數
    await groups.getByRole('button', { name: '磁碟使用率' }).click();
    await page.waitForURL(/metric=fs/);
    await page.locator('.k-multiples .k-multiple').nth(3).waitFor({ timeout: 15000 });
    assert.equal(await page.locator('.k-multiples .k-multiple').count(), 4);
    await axe(page, 'F4 趨勢：磁碟使用率');
    // 磁碟讀寫：裝置選單（裝置（掛載點））與忙碌時間比例
    await groups.getByRole('button', { name: '磁碟讀寫' }).click();
    await page.waitForURL(/metric=io/);
    const dev = page.getByLabel('裝置');
    await dev.waitFor({ timeout: 15000 });
    assert.ok((await dev.locator('option').allTextContents()).includes('vda1（/）'));
    await page.getByRole('heading', { name: /忙碌時間比例/ }).waitFor({ timeout: 15000 });
    await page.locator('.k-legend', { hasText: '讀取' }).first().waitFor();
    await page.getByText(/單位：MB\/s（十進位/).waitFor();
    await dev.selectOption('*');
    await page.waitForURL(/dev=\*|dev=%2A/);
    await page.locator('.k-multiples .k-multiple').nth(1).waitFor({ timeout: 15000 });
    await axe(page, 'F4 趨勢：磁碟讀寫（小倍數）');
    // 網路：接收／傳送與錯誤數
    await groups.getByRole('button', { name: '網路', exact: true }).click();
    await page.locator('.k-legend', { hasText: '接收' }).first().waitFor({ timeout: 15000 });
    await page.getByText(/eth0：接收錯誤 0 個、傳送錯誤 0 個/).waitFor({ timeout: 15000 });
    // 鍵盤：聚焦圖後以方向鍵逐點檢視，數值寫在 aria-live 區
    const svg = page.locator('svg.k-chart[tabindex="0"]').first();
    await svg.focus();
    await page.keyboard.press('End');
    await page.locator('p[aria-live="polite"]', { hasText: /接收 .*bps/ }).first().waitFor();
    // 表格檢視
    await page.getByText('以表格檢視資料').first().click();
    await page.locator('table.k-mini-table th', { hasText: '傳送' }).first().waitFor();
    await axe(page, 'F4 趨勢：網路');
    await shot(page, 'f4-host-net');
    // 負載：參考線「邏輯處理器 8 個」
    await groups.getByRole('button', { name: '負載' }).click();
    await page.locator('.k-legend', { hasText: '邏輯處理器 8 個' }).waitFor({ timeout: 15000 });
    await page.locator('.k-legend', { hasText: '15 分鐘' }).waitFor();
    // 置換空間
    await groups.getByRole('button', { name: '置換空間' }).click();
    await page.waitForURL(/metric=swap/);
    await page.locator('.k-legend', { hasText: '使用率' }).waitFor({ timeout: 15000 });
    await axe(page, 'F4 趨勢：置換空間');
    await ctx.close();
  });

  test('F4 S05 行程：兩張表、排序依據、命令列提示、歷史瀏覽與保留期限', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'viewer', PW, `/hosts/${WEB}/processes`);
    await waitTitle(page, 'web-01');
    await page.locator('table caption', { hasText: /依 CPU（前 2）/ }).waitFor({ timeout: 15000 });
    await page.locator('table caption', { hasText: /依記憶體（前 2）/ }).waitFor();
    assert.equal(await page.locator('th[aria-sort="descending"]').count(), 2);
    await page.getByText('Pulse 回報的命令列可能含敏感資訊').waitFor();
    // 命令列超過 48 字：截斷＋展開
    const cmd = page.locator('details.k-cmd').first();
    await cmd.locator('summary').click();
    await cmd.getByText(/config_file=\/etc\/postgresql\.conf/).waitFor();
    await page.getByText(/取樣時間（UTC\+8）/).waitFor();
    await page.getByText('（最新，每 60 秒一筆）').waitFor();
    assert.equal(await page.getByRole('button', { name: '較晚一筆' }).getAttribute('aria-disabled'), 'true');
    await axe(page, 'F4 主機詳情：行程');
    await shot(page, 'f4-host-processes');
    // 較早一筆
    await page.getByRole('button', { name: '較早一筆' }).click();
    await page.waitForURL(/at=/);
    await page.getByText(/之前最近的一筆/).waitFor();
    assert.equal(await page.evaluate(() => document.activeElement.textContent), '較早一筆');
    // 超出保留期限
    await page.getByLabel(/跳到時間/).fill('2020-01-01T08:00');
    await page.getByRole('button', { name: '前往' }).click();
    await page.getByText('這個時間沒有行程快照').waitFor();
    await page.getByRole('button', { name: '回到最新' }).click();
    await page.getByText('（最新，每 60 秒一筆）').waitFor();
    await ctx.close();
  });

  test('F4 S05 資產資訊：作業系統與硬體、序號、磁碟、網路介面、變更歷史', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'viewer', PW, `/hosts/${WEB}/inventory`);
    await waitTitle(page, 'web-01');
    await page.getByText('Linux (Ubuntu 24.04)').waitFor({ timeout: 15000 });
    await page.getByText('AMD EPYC 7763 64-Core Processor').waitFor();
    await page.getByText('4 個實體核心、8 個邏輯處理器').waitFor();
    await page.getByText('8 GiB（置換空間 2 GiB）').waitFor();
    await page.getByText('E2E-SERIAL-01').waitFor();
    await page.locator('table caption', { hasText: '磁碟' }).waitFor();
    await page.locator('td', { hasText: 'vdb1' }).waitFor();
    await page.locator('td', { hasText: '52:54:00:12:34:56' }).waitFor();
    await page.getByText(/首次收到資產資訊/).waitFor();
    await axe(page, 'F4 主機詳情：資產資訊');
    await shot(page, 'f4-host-inventory');
    // 主機清單不顯示序號
    await page.goto(`${URL0}/hosts`);
    await page.locator('tbody tr[data-key]').first().waitFor();
    assert.equal(await page.getByText('E2E-SERIAL-01').count(), 0);
    await ctx.close();
  });

  test('F4 舊版 Pulse 與 Windows：空狀態文案', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'viewer', PW, `/hosts/${DB}`);
    await waitTitle(page, 'db-02');
    const values = page.locator('section', { has: page.getByRole('heading', { name: '目前數值' }) });
    await values.getByText('— Windows 不提供負載').waitFor();
    await values.getByText('置換空間（分頁檔）').waitFor();
    await values.getByText('— 需要 Pulse 0.8.0 以上').first().waitFor();
    await page.getByRole('link', { name: '行程', exact: true }).click();
    await page.getByText('這台主機的 Pulse 版本不回報此資訊').waitFor();
    await page.getByText(/需要 Pulse 0\.8\.0 以上（目前 0\.6\.0）/).waitFor();
    await axe(page, 'F4 舊版 Pulse：行程');
    await page.getByRole('link', { name: '資產資訊' }).click();
    await page.getByText('（來自連線時的 hello）').waitFor();
    await page.getByText('這台主機的 Pulse 版本不回報此資訊').waitFor();
    await axe(page, 'F4 舊版 Pulse：資產資訊');
    await page.goto(`${URL0}/hosts/${DB}/metrics?metric=load`);
    await page.getByText('Windows 不提供負載').waitFor({ timeout: 15000 });
    await page.goto(`${URL0}/hosts/${DB}/metrics?metric=io`);
    await page.getByText('這台主機的 Pulse 版本不回報此資訊').waitFor({ timeout: 15000 });
    await axe(page, 'F4 舊版 Pulse：磁碟讀寫');
    await ctx.close();
  });

  test('F4 S04：磁碟欄、作業系統篩選、欄位設定；S03：磁碟排行', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'viewer', PW, '/hosts');
    await waitTitle(page, '主機');
    const row = page.locator(`tr[data-key="${WEB}"]`);
    await row.waitFor();
    await row.getByText(/\/var\/log 9\d\.\d%/).waitFor();
    await row.getByText('將滿').waitFor();
    // 依磁碟排序
    await page.getByRole('button', { name: /^磁碟/ }).click();
    await page.waitForURL(/sort=-?disk/);
    // 作業系統（家族）篩選
    const os = page.getByLabel('作業系統');
    await os.selectOption('windows');
    await page.waitForURL(/os_family=windows/);
    await page.locator(`tr[data-key="${DB}"]`).waitFor();
    assert.equal(await page.locator(`tr[data-key="${WEB}"]`).count(), 0);
    await page.getByRole('button', { name: '清除篩選' }).first().click();
    await page.locator(`tr[data-key="${WEB}"]`).waitFor();
    // 欄位設定：開啟「運作時間」「邏輯處理器」
    await page.getByRole('button', { name: '欄位設定' }).click();
    const dlg = page.getByRole('dialog', { name: '欄位設定' });
    await dlg.getByLabel('運作時間').check();
    await dlg.getByLabel('邏輯處理器').check();
    await axe(page, 'F4 主機清單：欄位設定');
    await dlg.getByRole('button', { name: '套用' }).click();
    await page.getByRole('columnheader', { name: /運作時間/ }).waitFor();
    await page.locator(`tr[data-key="${WEB}"] td`, { hasText: /^8$/ }).waitFor();
    const stored = await page.evaluate(() => localStorage.getItem('kairos.hostColumns'));
    assert.ok(stored && stored.length < 40, stored);
    await axe(page, 'F4 主機清單');
    await shot(page, 'f4-hosts');
    // S03：資源使用前 10 的「磁碟」
    await page.goto(`${URL0}/`);
    await waitTitle(page, '總覽');
    await page.getByRole('group', { name: '切換資源' }).getByRole('button', { name: '磁碟' }).click();
    await page.getByRole('columnheader', { name: '掛載點 使用率' }).waitFor();
    await page.locator('td', { hasText: /\/var\/log 9\d\.\d%/ }).first().waitFor();
    await axe(page, 'F4 總覽：磁碟排行');
    await ctx.close();
  });

  test('F4 S09／S08：新指標、單位、範圍與預覽；範本', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/alerts/rules/new');
    await waitTitle(page, '新增告警規則');
    const metric = page.locator('select[name="metric"]');
    await metric.waitFor();
    assert.deepEqual(await metric.locator('option').allTextContents(), ['CPU 使用率', '記憶體使用率', '磁碟使用率', '置換空間使用率', '每核心負載', '網路接收流量', '網路傳送流量']);
    await metric.selectOption('disk');
    const scope = page.locator('select[name="scope_mode"]');
    await scope.waitFor();
    await scope.selectOption('some');
    await page.getByLabel(/指定的掛載點/).fill('/var/log\n/data*');
    await page.getByText(/以過去 7 天的資料試算/).waitFor({ timeout: 20000 });
    await axe(page, 'F4 告警規則：磁碟');
    await metric.selectOption('network_rx');
    await page.locator('.ds-sentence', { hasText: 'Mbps' }).first().waitFor();
    await page.locator('select[name="scope_mode"]').waitFor();
    assert.equal(await page.locator('select[name="scope_mode"] option', { hasText: '所有網路介面' }).count(), 1);
    await metric.selectOption('load');
    await page.locator('.ds-sentence', { hasText: '倍' }).first().waitFor();
    await page.getByText(/^其中 \d+ 台沒有回報這個指標/).first().waitFor({ timeout: 20000 });
    await axe(page, 'F4 告警規則：負載');
    // 範本
    await page.goto(`${URL0}/alerts/rules`);
    await waitTitle(page, '告警規則');
    await page.getByRole('button', { name: '從範本新增' }).click();
    await page.getByRole('menuitem', { name: /磁碟使用率 > 95%/ }).click();
    await page.waitForURL(/template=disk95/);
    await page.locator('select[name="metric"]').waitFor();
    assert.equal(await page.locator('select[name="metric"]').inputValue(), 'disk');
    assert.equal(await page.locator('input[name="threshold"]').inputValue(), '95');
    await ctx.close();
  });

  test('F4 深色主題：趨勢、行程、資產資訊 axe 0 違規', async () => {
    const { ctx, page } = await newPage({ colorScheme: 'dark' });
    await login(page, 'viewer', PW, `/hosts/${WEB}/metrics?metric=io`);
    await waitTitle(page, 'web-01');
    await page.locator('.k-legend', { hasText: '讀取' }).first().waitFor({ timeout: 15000 });
    await axe(page, 'F4 趨勢（深色）');
    await shot(page, 'f4-host-io-dark');
    await page.goto(`${URL0}/hosts/${WEB}/processes`);
    await page.locator('table caption', { hasText: /依 CPU/ }).waitFor({ timeout: 15000 });
    await axe(page, 'F4 行程（深色）');
    await page.goto(`${URL0}/hosts/${WEB}/inventory`);
    await page.getByText('E2E-SERIAL-01').waitFor({ timeout: 15000 });
    await axe(page, 'F4 資產資訊（深色）');
    await page.goto(`${URL0}/hosts/${WEB}`);
    await page.getByText('214 個').waitFor({ timeout: 15000 });
    await axe(page, 'F4 概覽（深色）');
    await ctx.close();
  });
}
