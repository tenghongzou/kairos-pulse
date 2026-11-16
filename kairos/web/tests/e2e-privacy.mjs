// 個資保護（商業審計 S6）的端對端測試；由 e2e.mjs 以 registerPrivacy(...) 註冊在最後（會刪除主機）。
//
// - S05「更多」：唯讀、維運者看不到；管理員匯出個資（下載 JSON）、刪除主機資料（仍連線中時說明並停止；
//   離線後 L2：輸入主機名稱才能按，axe 0 違規）→ 回到主機清單、主機已不存在。
// - S05 假名化的主機（主機 API 的 pseudonymized）：頁首的說明；一般主機名稱沒有。
// - S12（第二個 Kairos，pki-env.sh）：設定了 enroll.download_dir 且檔案齊全時，建立 token 後顯示一行安裝指令。
// - S14（第二個 Kairos）：匯入手動簽發的憑證（根 CA 直接簽發，pki-env.sh 的 manual.pem）→ 結果留在對話框內
//   （再匯入同一張為「先前已匯入」、伺服器憑證全部未通過為 InlineAlert 與 KRS-E1305 三段式）→ 清單出現這台主機。

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const PKI_BASE = process.env.KAIROS_PKI_URL;
const PKI_DIR = process.env.PKI_DIR;
const MANUAL_ID = process.env.MANUAL_ID;

export function registerPrivacy({ test, newPage, login, waitTitle, axe, shot, FakeAgent, URL0, PW }) {
  const PURGE_ID = '11111111-1111-4111-8111-0000000000a9';
  const ONLINE_ID = '11111111-1111-4111-8111-0000000000aa';

  async function connectAgent(agentId, hostname) {
    const ag = new FakeAgent({ port: Number(process.env.AGENT_PORT), agentId, hostname, backfillMinutes: 8 });
    await ag.connect();
    const deadline = Date.now() + 20000;
    while (ag.acked < ag.reportSeq && Date.now() < deadline) await new Promise((r) => setTimeout(r, 200));
    return ag;
  }

  test('S05 刪除主機資料（L2）與匯出個資：只有管理員；仍連線中時不能刪除', async () => {
    const offline = await connectAgent(PURGE_ID, 'privacy-laptop');
    offline.close();
    const online = await connectAgent(ONLINE_ID, 'privacy-online');
    try {
      // 唯讀、維運者：沒有「更多」
      for (const user of ['viewer', 'operator']) {
        const { ctx, page } = await newPage();
        await login(page, user, PW, `/hosts/${PURGE_ID}`);
        await waitTitle(page, 'privacy-laptop');
        assert.equal(await page.locator('#host-more').count(), 0, `${user} 不應看到「更多」`);
        await ctx.close();
      }
      const { ctx, page } = await newPage();
      // 仍連線中：說明並停止
      await login(page, 'admin', PW, `/hosts/${ONLINE_ID}`);
      await waitTitle(page, 'privacy-online');
      await page.locator('#host-more').click();
      await page.getByRole('menuitem', { name: '刪除主機資料…' }).click();
      const blocked = page.locator('dialog[open]');
      await blocked.getByText('這台主機目前仍連線中').waitFor();
      assert.equal(await blocked.getByRole('button', { name: '刪除主機資料' }).count(), 0);
      // 有撤銷權限（管理員）：附「撤銷憑證…」捷徑（到憑證清單篩選這台主機）
      assert.match(await blocked.getByRole('link', { name: '撤銷憑證…' }).getAttribute('href'), new RegExp(`^/certificates\\?q=${ONLINE_ID}$`));
      await axe(page, 'S05 刪除主機資料（仍連線中）');
      await blocked.getByRole('button', { name: '關閉' }).last().click();

      // 離線的主機：匯出個資（下載 JSON）
      await page.goto(`${URL0}/hosts/${PURGE_ID}`);
      await waitTitle(page, 'privacy-laptop');
      await page.locator('#host-more').click();
      const [download] = await Promise.all([page.waitForEvent('download'), page.getByRole('menuitem', { name: '匯出個資（JSON）' }).click()]);
      assert.equal(download.suggestedFilename(), `kairos-host-${PURGE_ID}-personal-data.json`);
      const doc = JSON.parse(readFileSync(await download.path(), 'utf8'));
      assert.equal(doc.schema, 'kairos.host.personal-data/v1');
      assert.equal(doc.host.hostname, 'privacy-laptop');
      await page.getByText('已匯出這台主機的個資').waitFor();

      // 刪除：影響預覽 → L2（輸入主機名稱才能按）
      await page.locator('#host-more').click();
      await page.getByRole('menuitem', { name: '刪除主機資料…' }).click();
      const dlg = page.locator('dialog[open]');
      await dlg.getByText('刪除後無法復原。').waitFor();
      await dlg.getByText('稽核日誌不刪除', { exact: false }).waitFor();
      // 每一類都列出筆數（0 筆的列照常顯示）
      assert.equal(await dlg.getByText(/^[\d,]+ 筆$/).count(), 7);
      const ok = dlg.getByRole('button', { name: '刪除主機資料', exact: true });
      assert.equal(await ok.isDisabled(), true);
      await dlg.locator('input[name="confirm"]').fill('privacy');
      assert.equal(await ok.isDisabled(), true);
      await dlg.locator('input[name="confirm"]').fill('privacy-laptop');
      assert.equal(await ok.isDisabled(), false);
      await dlg.locator('textarea[name="reason"]').fill('e2e：員工離職');
      await axe(page, 'S05 刪除主機資料（L2）');
      await shot(page, 'privacy-purge');
      await ok.click();
      await page.waitForURL(/\/hosts$/);
      await page.getByText('已刪除 privacy-laptop 的資料').waitFor();
      const status = await page.evaluate(async (id) => (await fetch(`/api/v1/hosts/${id}`)).status, PURGE_ID);
      assert.equal(status, 404);
      await ctx.close();
    } finally {
      online.close();
    }
  });

  test('S05 假名化的主機：依主機 API 的 pseudonymized 顯示說明', async () => {
    const PSEUDO_ID = '11111111-1111-4111-8111-0000000000ab';
    const PSEUDO = 'host-k3m7x2q7v4ab';
    const ag = await connectAgent(PSEUDO_ID, PSEUDO);
    try {
      const { ctx, page } = await newPage();
      await login(page, 'viewer', PW, `/hosts/${PSEUDO_ID}`);
      await waitTitle(page, PSEUDO);
      const api = (id) => page.evaluate(async (x) => (await (await fetch(`/api/v1/hosts/${x}`)).json()).data, id);
      assert.equal((await api(PSEUDO_ID)).pseudonymized, true);
      await page.getByText('主機名稱已假名化（無法還原）', { exact: false }).waitFor();
      await axe(page, 'S05 假名化的主機');
      // 一般的主機名稱：沒有說明
      await page.goto(`${URL0}/hosts/${ONLINE_ID}`);
      await waitTitle(page, 'privacy-online');
      assert.equal((await api(ONLINE_ID)).pseudonymized, false);
      assert.equal(await page.getByText('主機名稱已假名化', { exact: false }).count(), 0);
      await ctx.close();
    } finally {
      ag.close();
    }
  });

  test('S12 一行安裝指令（下載端點）與 S14 匯入手動簽發的憑證', async () => {
    assert.ok(PKI_BASE && PKI_DIR && MANUAL_ID, '需要第二個 Kairos（pki-env.sh）');
    // 下載端點的檔案（同一個發佈版本的安裝腳本、SHA256SUMS 與簽章）
    const dl = join(PKI_DIR, 'dl');
    mkdirSync(dl, { recursive: true });
    const script = '#!/bin/sh\n';
    const scriptSha = createHash('sha256').update(script).digest('hex');
    writeFileSync(join(dl, 'pulse-install.sh'), script);
    writeFileSync(join(dl, 'SHA256SUMS'), `${scriptSha}  pulse-install.sh\n`);
    writeFileSync(join(dl, 'SHA256SUMS.sig'), '-----BEGIN SSH SIGNATURE-----\n');
    const { ctx, page } = await newPage();
    await page.goto(`${PKI_BASE}/login?next=${encodeURIComponent('/enrollment/tokens')}`);
    await page.getByLabel('帳號').fill('admin');
    await page.locator('input[name="password"]').fill(PW);
    await page.getByRole('button', { name: '登入', exact: true }).click();
    await page.locator('#page-title', { hasText: '註冊 token' }).waitFor({ timeout: 15000 });
    await page.getByRole('button', { name: '建立 token' }).first().click();
    const dlg = page.locator('dialog[open]');
    await dlg.locator('input[name="name"]').fill('一行指令');
    await dlg.locator('input[name="group"]').fill('B廠產線');
    await dlg.locator('input[name="max_uses"]').fill('5');
    await dlg.getByRole('button', { name: '建立 token' }).click();
    const one = page.locator('#tk-oneliner code');
    await one.waitFor();
    const cmd = await one.innerText();
    assert.match(cmd, /curl -fsSLO --cacert kairos-ca\.pem https:\/\/127\.0\.0\.1:\d+\/agent\/pulse-install\.sh/);
    assert.match(cmd, /--base-url https:\/\/127\.0\.0\.1:\d+\/agent --ca-file kairos-ca\.pem/);
    // 安全稽核 C3：下載後先比對腳本的 SHA-256，相符才以 root 執行（比對在 sudo 之前）
    const check = `echo '${scriptSha}  pulse-install.sh' | sha256sum -c - && sudo `;
    assert.ok(cmd.includes(check), cmd);
    assert.ok(cmd.indexOf('pulse-install.sh &&') < cmd.indexOf(check), cmd);
    await page.getByText('指令會先比對安裝腳本的 SHA-256', { exact: false }).waitFor();
    // 安全稽核 C3 加強：測試用的 Kairos 沒有設定 [remote] allowed_signers——指令照常顯示，管理員另外看到
    // 「沒有驗證下載目錄的簽章」的警語（API 的 downloads.warning）
    assert.match(await page.locator('#tk-one-sig').innerText(), /沒有設定 \[remote\] allowed_signers/);
    assert.match(cmd, /PULSE_INSTALL_STRING='pulse-enroll:v1:/);
    await page.getByText('已先以安裝套件（MSI、deb、rpm）安裝 Pulse 時，改為執行：').waitFor();
    await axe(page, 'S12 建立完成（一行安裝指令）');
    await page.getByRole('button', { name: '完成' }).click();

    // S14：匯入手動簽發的憑證
    await page.goto(`${PKI_BASE}/certificates`);
    await page.locator('#page-title', { hasText: '憑證' }).waitFor({ timeout: 15000 });
    await page.locator('#cert-import').click();
    const imp = page.locator('dialog[open]');
    await imp.getByRole('button', { name: '匯入', exact: true }).click();
    await imp.getByText('請貼上至少一張 PEM 憑證').waitFor();
    // 底部固定說明：外部 CA 簽發的憑證撤銷後，下一次重新連線時才被拒絕
    await imp.getByText('在這裡撤銷後，已連線中的主機在下一次重新連線時才會被拒絕', { exact: false }).waitFor();
    await imp.locator('textarea[name="pem"]').fill(readFileSync(join(PKI_DIR, 'manual.pem'), 'utf8'));
    await axe(page, 'S14 匯入憑證');
    await imp.getByRole('button', { name: '匯入', exact: true }).click();
    // 結果留在對話框內（設計包 S14「0.9 規格」）：摘要為 role="status"，全部通過時清空欄位
    await imp.getByRole('status').filter({ hasText: '匯入 1 張、先前已匯入 0 張、未通過 0 張' }).waitFor();
    assert.equal(await imp.locator('textarea[name="pem"]').inputValue(), '');
    // 再匯入同一張：先前已匯入
    await imp.locator('textarea[name="pem"]').fill(readFileSync(join(PKI_DIR, 'manual.pem'), 'utf8'));
    await imp.getByRole('button', { name: '匯入', exact: true }).click();
    await imp.getByRole('status').filter({ hasText: '匯入 0 張、先前已匯入 1 張、未通過 0 張' }).waitFor();
    // 全部未通過（伺服器憑證：SAN 不是 agent_id）：InlineAlert「無法匯入」、逐張列出並可展開 KRS-E1305
    await imp.locator('textarea[name="pem"]').fill(readFileSync(join(PKI_DIR, 'server.pem'), 'utf8'));
    await imp.getByRole('button', { name: '匯入', exact: true }).click();
    const failed = imp.locator('.ds-alert--error', { hasText: '無法匯入' });
    await failed.getByText('匯入 0 張、先前已匯入 0 張、未通過 1 張').waitFor();
    await failed.getByText(/：未通過（KRS-E1305）：/).waitFor();
    await failed.locator('details summary').first().click();
    await failed.getByText('無法匯入憑證：', { exact: false }).first().waitFor();
    await axe(page, 'S14 匯入憑證（結果）');
    await imp.getByRole('button', { name: '關閉', exact: true }).last().click();
    await page.goto(`${PKI_BASE}/certificates?q=${MANUAL_ID.slice(0, 8)}`);
    await page.locator('tbody tr[data-key]', { hasText: MANUAL_ID.slice(0, 8) }).first().waitFor();
    await ctx.close();
  });
}
