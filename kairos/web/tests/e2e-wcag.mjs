// 發佈前的兩個 WCAG A 級修正（設計包 decisions/INTEGRATION.md §29.5 #6、#7）：
// - 2.1.4 單一字元快速鍵：告警事件的 j／k 只在焦點位於事件表格內時生效。
// - 2.2.1 可調整的時間限制：閒置逾時前 2 分鐘以對話框提醒，「繼續使用」延長；逾時才登出。
//   以 Playwright 的 page.clock 快轉瀏覽器時間（伺服器的工作階段不受影響）。

import assert from 'node:assert/strict';

export function registerWcag({ test, newPage, login, waitTitle, axe, URL0, PW }) {
  test('WCAG 2.1.4：j／k 只在焦點位於告警事件表格內時生效', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'operator', PW, '/alerts/events?view=all');
    await waitTitle(page, '告警事件');
    const links = page.locator('tbody .k-event-link');
    await links.first().waitFor({ timeout: 15000 });
    assert.ok((await links.count()) >= 2, '需要至少兩筆事件');
    const activeIsLink = () => page.evaluate(() => Boolean(document.activeElement && document.activeElement.classList.contains('k-event-link')));
    // 焦點在頁面標題（表格外）：j 不攔截、焦點不移動
    await page.locator('#page-title').focus();
    await page.keyboard.press('j');
    assert.equal(await activeIsLink(), false, '焦點在表格外時 j 不應移動焦點');
    // 搜尋欄：j 是一般文字
    const search = page.getByRole('searchbox').first();
    if (await search.count()) {
      await search.fill('');
      await search.press('j');
      assert.equal(await search.inputValue(), 'j');
      await search.fill('');
      await page.locator('tbody .k-event-link').first().waitFor();
    }
    // 焦點在表格內：j／k 在事件之間移動
    await links.first().focus();
    await page.keyboard.press('j');
    assert.equal(await page.evaluate(() => [...document.querySelectorAll('tbody .k-event-link')].indexOf(document.activeElement)), 1);
    await page.keyboard.press('k');
    assert.equal(await page.evaluate(() => [...document.querySelectorAll('tbody .k-event-link')].indexOf(document.activeElement)), 0);
    await page.getByText('焦點在事件表格內時').first().waitFor();
    await ctx.close();
  });

  test('WCAG 2.2.1：閒置逾時前 2 分鐘提醒，「繼續使用」延長、逾時才登出', async () => {
    const { ctx, page } = await newPage();
    await page.clock.install();
    await login(page, 'auditor', PW, '/');
    await waitTitle(page, '總覽');
    const idleSecs = await page.evaluate(async () => (await (await fetch('/api/v1/me')).json()).data.session.idle_timeout_secs || 1800);
    const dlg = page.getByRole('alertdialog', { name: '即將因閒置而登出' });

    // 到期前 2 分鐘內：出現提醒，焦點在「繼續使用」，有 aria-live 的剩餘時間
    await page.clock.fastForward((idleSecs - 115) * 1000);
    await dlg.waitFor({ timeout: 15000 });
    assert.equal(await page.evaluate(() => document.activeElement && document.activeElement.textContent.trim()), '繼續使用');
    assert.match(await dlg.locator('[aria-live]').textContent(), /剩餘 \d+:\d\d/);
    await axe(page, '閒置提醒');
    // 在對話框內按 Tab 不算「繼續使用」；Enter 選「繼續使用」→ 關閉、留在原頁
    await page.keyboard.press('Tab');
    await page.keyboard.press('Shift+Tab');
    await page.keyboard.press('Enter');
    await dlg.waitFor({ state: 'hidden' });
    await page.clock.fastForward(10 * 1000);
    assert.equal(new URL(page.url()).pathname, '/');
    assert.equal(await dlg.count(), 0);

    // 延長後重新計時：再次接近到期才提醒；不理會則逾時登出
    await page.clock.fastForward((idleSecs - 125) * 1000);
    await dlg.waitFor({ timeout: 15000 });
    await page.clock.fastForward(130 * 1000);
    await page.waitForURL(/\/login/, { timeout: 15000 });
    assert.match(page.url(), /reason=session_expired/);
    await ctx.close();
  });
}
