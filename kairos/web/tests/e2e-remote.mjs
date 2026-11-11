// 遠端管理（商業審計 F8 MVP：kairos/API.md 第 17 節）在主控台的端對端測試；由 e2e.mjs 註冊。
//
// 涵蓋（設計包 screens.md S24～S26、S04、S05「0.9 規格」）：
// - 假 agent pulse-09（Pulse 0.9.0、hello 的 install：tar、就緒、集中設定啟用）。測試用的 Kairos 沒有 mTLS，
//   所以不協商 config／upgrade；資格與設定狀態只依 hello 的版本與 install。
// - S25：空狀態、信任錨未設定的警告與「建議匯入部署中的版本」；匯入對話框——簽章不符（KRS-E8413，對話框不關閉）→
//   正確的發佈（本檔以 Node.js 的 Ed25519 產生與 ssh-keygen -Y sign 相同格式的 SHA256SUMS.sig）→ 逐檔上傳
//   （✓ 雜湊相符、不在 SHA256SUMS 中的檔案就地錯誤）→ 清單與詳情（已上傳／待上傳）；推送使用中時刪除為 aria-disabled，
//   取消推送後以 L2（輸入版本號）刪除。
// - S26：建立推送的單頁表單與資格預覽（資格依連線中主機的 hello，不等清冊同步；可升級 1 台、Pulse 太舊的主機分列）
//   → 無法自動回滾的 L2 字串（攔截預覽）→ L2 輸入「升級 1 台」送出 → 詳情；維運者只有「暫停…」（L1、原因必填；
//   主控台暫停不附 KRS-W8414）→ 計數泡泡；自動暫停的標題與 KRS-W8414、計數泡泡依清單的 meta（攔截 API）→
//   管理員「繼續…」→「取消推送…」。
// - S20「遠端管理」列（信任錨未設定的警告與捷徑；0.10 的「版本庫的檔案遺失」；唯讀不顯示）。
// - S24（維運者，config:write）：新增設定檔（依 schema 產生的欄位、範圍錯誤、隱私敏感的選項）→ 指派全站（影響預覽）→
//   修改產生第 2 版 → 版本歷史 → 回復到第 1 版 → 刪除被指派擋下 → S05「設定」分頁 → 解除指派 → 刪除；稽核員唯讀。
// - S04 的「設定狀態」「遠端升級」篩選；S05 的「升級」卡片（攔截 API 模擬回滾失敗與「標記為已處理…」）。
// - 功能關閉（攔截 API 回 503 remote_unavailable）：S24～S26 整頁 EmptyState，「Pulse 管理」區塊仍顯示。
// - 每個畫面 axe 0 違規（含深色）。

import assert from 'node:assert/strict';
import { createHash, createPrivateKey, createPublicKey, sign as edSign } from 'node:crypto';

const P09 = '11111111-1111-4111-8111-000000000009';
const WEB = '11111111-1111-4111-8111-000000000001';
const VERSION = '0.9.1';
const COMMIT = 'abcdef1';

// ---- 測試用的發佈（與 kairos/src/remote/test_sign.rs 相同的 SSHSIG 格式） ----
function sshString(buf) {
  const b = Buffer.isBuffer(buf) ? buf : Buffer.from(buf);
  const len = Buffer.alloc(4);
  len.writeUInt32BE(b.length);
  return Buffer.concat([len, b]);
}

function signer(seedByte) {
  const seed = Buffer.alloc(32, seedByte);
  const key = createPrivateKey({ key: Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), seed]), format: 'der', type: 'pkcs8' });
  const pub = createPublicKey(key).export({ format: 'der', type: 'spki' }).subarray(-32);
  const keyBlob = Buffer.concat([sshString('ssh-ed25519'), sshString(pub)]);
  return (message) => {
    const ns = 'kairos-pulse-release';
    const signed = Buffer.concat([Buffer.from('SSHSIG'), sshString(ns), sshString(''), sshString('sha512'), sshString(createHash('sha512').update(message).digest())]);
    const sig = edSign(null, signed, key);
    const sigBlob = Buffer.concat([sshString('ssh-ed25519'), sshString(sig)]);
    const ver = Buffer.alloc(4);
    ver.writeUInt32BE(1);
    const blob = Buffer.concat([Buffer.from('SSHSIG'), ver, sshString(keyBlob), sshString(ns), sshString(''), sshString('sha512'), sshString(sigBlob)]);
    const b64 = blob.toString('base64').match(/.{1,70}/g).join('\n');
    return `-----BEGIN SSH SIGNATURE-----\n${b64}\n-----END SSH SIGNATURE-----\n`;
  };
}

function testRelease() {
  const manifestName = `pulse-${VERSION}-${COMMIT}-release.json`;
  const manifest = `{"schema":"kairos-pulse.release/v1","product":"pulse","version":"${VERSION}","commit":"${COMMIT}","released_at":"2026-11-18","min_kairos":"0.5.0","min_upgrade_from":"0.9.0","remote_features":["config","upgrade"],"breaking":false}\n`;
  const tar = `pulse-${VERSION}-${COMMIT}-linux-x86_64.tar.gz`;
  const deb = `pulse-${VERSION}-${COMMIT}-linux-x86_64.deb`;
  const tarData = Buffer.alloc(40000, 'x');
  tarData.write(`${tar}\n`);
  const debData = Buffer.alloc(40000, 'y');
  const sha = (b) => createHash('sha256').update(b).digest('hex');
  // deb 列在 SHA256SUMS 但不上傳（詳情顯示「待上傳」）
  const sums = `${sha(tarData)}  ${tar}\n${sha(debData)}  ${deb}\n${sha(Buffer.from(manifest))}  ${manifestName}\n`;
  const sig = signer(7)(Buffer.from(sums));
  const file = (name, buffer) => ({ name, mimeType: 'application/octet-stream', buffer: Buffer.isBuffer(buffer) ? buffer : Buffer.from(buffer) });
  return {
    good: [file('SHA256SUMS', sums), file('SHA256SUMS.sig', sig), file(manifestName, manifest), file(tar, tarData), file(`pulse-${VERSION}-${COMMIT}-linux-aarch64.tar.gz`, Buffer.alloc(100, 'z'))],
    // 簽章的內容與 SHA256SUMS 不符（另一份內容的簽章）
    bad: [file('SHA256SUMS', sums), file('SHA256SUMS.sig', signer(7)(Buffer.from('tampered\n'))), file(manifestName, manifest)],
    tar,
    deb,
  };
}

async function api(page, method, path, body) {
  return page.evaluate(
    async ([m, p, b]) => {
      const me = (await (await fetch('/api/v1/me')).json()).data;
      const r = await fetch(p, { method: m, headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': me.csrf_token }, body: b == null ? undefined : JSON.stringify(b) });
      let json = null;
      try {
        json = await r.json();
      } catch {
        json = null;
      }
      return { status: r.status, json };
    },
    [method, path, body],
  );
}

async function until(fn, ms = 20000, what = '條件') {
  const t0 = Date.now();
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() - t0 > ms) throw new Error(`等待逾時：${what}`);
    await new Promise((r) => setTimeout(r, 300));
  }
}

export function registerRemote({ test, newPage, login, waitTitle, axe, shot, FakeAgent, URL0, PW }) {
  let agent = null;
  let rolloutId = null;
  let profileId = null;

  test('F8 S25 Pulse 版本庫：信任錨警告 → 匯入（簽章不符 KRS-E8413）→ 匯入並上傳 → 詳情', async () => {
    agent = new FakeAgent({
      port: Number(process.env.AGENT_PORT),
      agentId: P09,
      hostname: 'pulse-09',
      tags: ['env:lab', 'f8'],
      backfillMinutes: 8,
      agentVersion: '0.9.0',
      install: { method: 'tar', upgrade: 'ready', config: 'enabled' },
    });
    await agent.connect();
    const rel = testRelease();
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/account');
    await waitTitle(page, '個人設定');
    // 主機清單的索引（facets）看得到 pulse-09 的版本後才開啟版本庫（「建議匯入部署中的版本」依它計算）
    await until(async () => {
      const r = await api(page, 'GET', '/api/v1/hosts/facets');
      return r.status === 200 && (r.json.data.version.values || []).some((v) => v.value === '0.9.0');
    }, 30000, 'facets 的 0.9.0');
    await page.goto(`${URL0}/pulse/releases`);
    await waitTitle(page, 'Pulse 版本庫');
    await page.getByText('還沒有匯入任何 Pulse 版本').waitFor();
    // 測試用的 Kairos 沒有設定 allowed_signers：警告；pulse-09 執行 0.9.0、不在版本庫：建議匯入
    await page.getByText(/信任錨未設定/).waitFor();
    await page.getByText(/建議也匯入目前部署中的版本 0\.9\.0：\d+ 台主機/).waitFor();
    // 側邊導覽的「Pulse 管理」區塊（ia.md §3）
    assert.deepEqual(await page.locator('.ds-sidenav__section', { hasText: 'Pulse 管理' }).locator('.ds-sidenav__label').allTextContents(), ['集中設定', '升級推送', 'Pulse 版本庫']);
    await axe(page, 'F8 S25 版本庫（空）');
    // 簽章不符：三段式，對話框不關閉
    await page.getByRole('button', { name: '匯入版本…' }).first().click();
    const dlg = page.getByRole('dialog');
    await dlg.getByLabel('檔案').setInputFiles(rel.bad);
    await dlg.getByText('已選擇 3 個檔案').waitFor();
    await axe(page, 'F8 S25 匯入對話框');
    await dlg.getByRole('button', { name: '驗證並匯入' }).click();
    await dlg.getByText('KRS-E8413').waitFor();
    await axe(page, 'F8 S25 匯入失敗');
    // 正確的發佈：驗證 → 逐檔上傳（不在 SHA256SUMS 的檔案就地錯誤）
    await dlg.getByLabel('檔案').setInputFiles(rel.good);
    await dlg.getByRole('button', { name: '驗證並匯入' }).click();
    await dlg.getByText(`2. 簽章驗證通過：Pulse ${VERSION}（commit ${COMMIT}）`).waitFor();
    await dlg.locator(`li[data-file="${rel.tar}"]`).getByText('✓ 雜湊相符').waitFor({ timeout: 20000 });
    await dlg.locator('li[data-file$="aarch64.tar.gz"]').getByText('這個檔案不在 SHA256SUMS 中').waitFor();
    await dlg.getByText(/3\. 完成：已上傳 1 個發佈檔，1 個沒有上傳/).waitFor();
    await axe(page, 'F8 S25 匯入完成');
    await dlg.getByRole('button', { name: '完成' }).click();
    const row = page.locator(`tr[data-version="${VERSION}"]`);
    await row.waitFor();
    await row.getByText(/linux x86_64：.*deb（未上傳）/).waitFor();
    assert.match(await row.innerText(), /tar\.gz(?!（未上傳）)/);
    await axe(page, 'F8 S25 版本庫');
    await shot(page, 'f8-releases');
    // 詳情
    await row.getByRole('link', { name: VERSION }).click();
    await waitTitle(page, `Pulse ${VERSION}`);
    await page.locator(`tr[data-file="${rel.tar}"]`).getByText('已上傳').waitFor();
    await page.locator(`tr[data-file="${rel.deb}"]`).getByText('待上傳').waitFor();
    await page.getByText('0.9.0', { exact: true }).first().waitFor();
    await axe(page, 'F8 S25 版本詳情');
    await page.emulateMedia({ colorScheme: 'dark' });
    await axe(page, 'F8 S25 版本詳情（深色）');
    await page.emulateMedia({ colorScheme: 'light' });
    // S20「遠端管理」列：版本庫有發佈版本、信任錨未設定 → 警告，附前往版本庫的捷徑
    await page.goto(`${URL0}/system/health`);
    await waitTitle(page, '系統健康');
    const remoteRow = page.locator('[data-item="remote"]');
    await remoteRow.getByText('遠端管理', { exact: true }).waitFor();
    assert.equal(await remoteRow.getAttribute('data-status'), 'warning');
    await remoteRow.getByText(/信任錨未設定：匯入的發佈版本無法驗證簽章/).waitFor();
    await remoteRow.getByRole('link', { name: '前往 Pulse 版本庫' }).waitFor();
    await axe(page, 'F8 S20 遠端管理列');
    // 版本庫的檔案遺失（0.10；以攔截的健康 API 模擬還原備份之後的狀態）：警告、代碼、前往版本庫的捷徑
    await ctx.route('**/api/v1/system/health', async (route) => {
      const real = await (await route.fetch()).json();
      const items = (real.data.items || []).map((i) =>
        i.id === 'remote'
          ? {
              ...i,
              status: 'warning',
              product_code: 'KRS-W8417',
              summary: `版本庫的檔案遺失：${VERSION} 的 ${rel.tar}（3 台的回滾需要）；請重新匯入這個版本的發佈檔`,
              values: { ...i.values, allowed_signers_configured: true, release_files_missing: 1, missing_files: [{ version: VERSION, filename: rel.tar, problem: 'missing', rollback_target_for: 3 }] },
            }
          : i,
      );
      await route.fulfill({ json: { ...real, data: { ...real.data, items } } });
    });
    await page.reload();
    await waitTitle(page, '系統健康');
    const missingRow = page.locator('[data-item="remote"]');
    await missingRow.getByText(/版本庫的檔案遺失：.*3 台的回滾需要/).waitFor();
    assert.equal(await missingRow.getAttribute('data-status'), 'warning');
    await missingRow.getByText('KRS-W8417').waitFor();
    assert.equal(await missingRow.getByRole('link', { name: '前往 Pulse 版本庫' }).getAttribute('href'), '/pulse/releases');
    await axe(page, 'F8 S20 版本庫的檔案遺失');
    await ctx.unroute('**/api/v1/system/health');
    await ctx.close();
    // 唯讀（沒有 rollouts:read）：不顯示「遠端管理」列
    const v = await newPage();
    await login(v.page, 'viewer', PW, '/system/health');
    await waitTitle(v.page, '系統健康');
    await v.page.locator('[data-item="database"]').waitFor();
    assert.equal(await v.page.locator('[data-item="remote"]').count(), 0);
    await v.ctx.close();
  });

  test('F8 S26 升級推送：資格預覽 → L2 送出 → 維運者暫停（計數泡泡）→ 管理員繼續、取消 → 刪除版本（L2）', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/pulse/rollouts');
    await waitTitle(page, '升級推送');
    await page.getByText('還沒有升級推送').waitFor();
    // 推送的資格以連線中主機的 hello 為準（API.md 第 17.3 節）：不必等清冊每 30 秒的同步
    const upg = await api(page, 'GET', `/api/v1/hosts/${P09}/upgrade`);
    assert.equal(upg.status, 200);
    assert.equal(upg.json.data.install.upgrade, 'ready');
    await axe(page, 'F8 S26 推送（空）');
    // 0.10 排程：starts_at 在未來的推送送出後為「已排程」，可以取消（以 API 建立，畫面檢查清單與詳情）
    const startsAt = new Date(Date.now() + 6 * 3600 * 1000).toISOString();
    const sch = await api(page, 'POST', '/api/v1/rollouts', {
      name: 'E2E 排程',
      target_version: VERSION,
      selector: { all: true },
      waves: [{ size: 1, manual_continue: true }, { size: '50%', soak_minutes: 5 }, { size: 'rest' }],
      schedule: { starts_at: startsAt, windows: [{ days: ['sat', 'sun'], start: '23:00', end: '03:00' }] },
    });
    assert.equal(sch.status, 201, JSON.stringify(sch.json));
    const schId = sch.json.data.id;
    assert.equal(sch.json.data.schedule.windows[0].end, '03:00');
    assert.ok(sch.json.data.schedule.timezone, '有時段時填入預設時區');
    const schSubmit = await api(page, 'POST', `/api/v1/rollouts/${schId}/submit`, { confirm: '升級 1 台' });
    assert.equal(schSubmit.status, 200, JSON.stringify(schSubmit.json));
    assert.equal(schSubmit.json.data.state, 'scheduled');
    assert.equal(schSubmit.json.data.next_action.code, 'scheduled');
    await page.reload();
    await waitTitle(page, '升級推送');
    const schRow = page.locator(`tr[data-rollout="${schId}"]`);
    await schRow.getByText('已排程').first().waitFor();
    await schRow.getByText(/已排程，.* 開始/).waitFor();
    await axe(page, 'F8 S26 推送清單（已排程）');
    await page.goto(`${URL0}/pulse/rollouts/${schId}`);
    await waitTitle(page, 'E2E 排程');
    await page.getByText(/已排程：.* 開始升級。對象已在送出時固定/).waitFor();
    await page.locator('#rollout-schedule').getByText(/維護時段：週六、週日 23:00～03:00（時區 /).waitFor();
    // 三批：先行批次、第 2 批、其餘
    await page.locator('#rollout-waves').getByText('第 2 批').waitFor();
    assert.equal(await page.getByRole('button', { name: '取消推送…' }).count(), 1, '已排程的推送可以取消');
    await axe(page, 'F8 S26 推送詳情（已排程、三批）');
    const schCancel = await api(page, 'POST', `/api/v1/rollouts/${schId}/cancel`, { reason: 'E2E：排程測試結束' });
    assert.equal(schCancel.status, 200, JSON.stringify(schCancel.json));
    await page.goto(`${URL0}/pulse/rollouts`);
    await waitTitle(page, '升級推送');
    await page.getByRole('link', { name: '建立推送' }).first().click();
    await waitTitle(page, '建立推送');
    // 雙人覆核的控制項不出現
    assert.equal(await page.getByText(/核准者|雙人覆核/).count(), 0);
    await page.getByLabel(/^名稱/).fill('E2E 0.9.1');
    const preview = page.locator('#rollout-preview');
    await preview.getByText('可升級 1 台').waitFor({ timeout: 15000 });
    // 0.6／0.8 的假 agent：Pulse 太舊（0.8 → 0.9 必須手動升級一次）
    await preview.locator('li[data-reason="unsupported_version"]').waitFor();
    await preview.getByText('先行批次 1 台、其餘 0 台').waitFor();
    // 0.10 自訂批次：套用建議的四批範本 → 預覽列出各批台數 → 移除中間的批次回到兩批
    await page.getByRole('button', { name: /套用建議的四批範本/ }).click();
    await preview.getByText('分 4 批：1 → 0 → 0 → 0 台').waitFor({ timeout: 15000 });
    await page.locator('#rollout-mid-waves fieldset[data-wave="2"]').waitFor();
    // 0.10 維護時段：新增一個時段，時區說明來自預覽的 default_timezone
    await page.getByRole('button', { name: '新增時段' }).click();
    await page.locator('#rollout-windows fieldset[data-window="0"]').waitFor();
    await page.locator('#rollout-window-tz').getByText(/時段以 Kairos 的時區 [+-]\d\d:\d\d 計算/).waitFor({ timeout: 15000 });
    await axe(page, 'F8 S26 建立推送（四批＋維護時段）');
    await page.getByRole('button', { name: '移除時段 1' }).click();
    await page.getByRole('button', { name: '移除第 2 批' }).click();
    await page.getByRole('button', { name: '移除第 2 批' }).click();
    assert.equal(await page.locator('#rollout-mid-waves fieldset').count(), 0);
    await preview.getByText('先行批次 1 台、其餘 0 台').waitFor({ timeout: 15000 });
    await axe(page, 'F8 S26 建立推送');
    await shot(page, 'f8-rollout-new');
    // 數字欄位的範圍
    const soak = page.getByLabel('觀察時間（分鐘）');
    await soak.fill('99999');
    await page.getByRole('button', { name: '送出推送…' }).click();
    await page.getByText('請輸入 0～10,080 的整數').waitFor();
    await soak.fill('60');
    // 允許無法自動回滾的主機時的 L2 字串（設計包 S26）：攔截一次預覽模擬 1 台無法自動回滾，對話框照字串顯示後取消
    await page.route(
      '**/api/v1/rollouts/preview',
      async (route) => {
        const r = await route.fetch();
        const body = await r.json();
        body.data.no_rollback = 1;
        body.data.confirm_text = `升級 ${body.data.eligible} 台（無法自動回滾 1 台）`;
        return route.fulfill({ response: r, json: body });
      },
      { times: 1 },
    );
    await page.getByRole('button', { name: '送出推送…' }).click();
    const nrb = page.getByRole('dialog');
    await nrb.getByText('輸入「升級 1 台（無法自動回滾 1 台）」以確認').waitFor();
    await nrb.getByText('其中 1 台無法自動回滾，升級失敗時需要到現場處理。').waitFor();
    assert.ok(await nrb.getByRole('button', { name: '升級 1 台（無法自動回滾 1 台）' }).isDisabled());
    await axe(page, 'F8 S26 送出確認（無法自動回滾，L2）');
    await nrb.getByRole('button', { name: '取消' }).click();
    await nrb.waitFor({ state: 'detached' });
    await page.getByRole('button', { name: '送出推送…' }).click();
    const dlg = page.getByRole('dialog');
    await dlg.getByText('輸入「升級 1 台」以確認').waitFor();
    const go = dlg.getByRole('button', { name: '升級 1 台' });
    assert.ok(await go.isDisabled(), '輸入確認字串前不能送出');
    await axe(page, 'F8 S26 送出確認（L2）');
    await dlg.getByLabel(/輸入「升級 1 台」以確認/).fill('升級 1 台');
    await go.click();
    await page.getByText('已送出推送「E2E 0.9.1」，開始升級').waitFor();
    await waitTitle(page, 'E2E 0.9.1');
    rolloutId = new URL(page.url()).pathname.split('/').pop();
    assert.match(rolloutId, /^r-\d{8}-\d{4}$/);
    await page.locator('#rollout-waves').waitFor();
    await page.locator(`tbody tr[data-key="${P09}"]`).waitFor({ timeout: 15000 });
    await axe(page, 'F8 S26 推送詳情');
    await shot(page, 'f8-rollout-detail');
    // 版本被推送使用：刪除為 aria-disabled 並說明
    await page.goto(`${URL0}/pulse/releases/${VERSION}`);
    await waitTitle(page, `Pulse ${VERSION}`);
    await page.getByText('1 個未結束的推送使用這個版本').waitFor();
    assert.equal(await page.getByRole('button', { name: '刪除版本…' }).getAttribute('aria-disabled'), 'true');
    await ctx.close();

    // 維運者：只能暫停（緊急煞車）
    const o = await newPage();
    await login(o.page, 'operator', PW, `/pulse/rollouts/${rolloutId}`);
    await waitTitle(o.page, 'E2E 0.9.1');
    await o.page.locator('#rollout-waves').waitFor();
    assert.equal(await o.page.getByRole('button', { name: /^(繼續…|取消推送…)$/ }).count(), 0, '維運者不能繼續或取消');
    await o.page.getByRole('button', { name: '暫停…' }).click();
    const pd = o.page.getByRole('dialog');
    await pd.getByRole('button', { name: '暫停推送' }).click();
    await pd.getByText('請填寫原因').waitFor();
    await pd.getByLabel(/原因/).fill('E2E：現場回報異常，先停');
    await pd.getByRole('button', { name: '暫停推送' }).click();
    await o.page.getByText('已暫停推送').first().waitFor();
    await o.page.getByText('已暫停：E2E：現場回報異常，先停').waitFor();
    // 主控台暫停（pause_cause: manual）不附 KRS-W8414
    assert.equal(await o.page.locator('.ds-alert', { hasText: '已暫停：E2E：現場回報異常，先停' }).getByText('KRS-W8414').count(), 0);
    // 計數泡泡（已暫停的推送）
    const badge = o.page.locator('.ds-sidenav__link', { hasText: '升級推送' }).locator('.ds-sidenav__count');
    await badge.waitFor({ timeout: 15000 });
    assert.equal((await badge.textContent()).trim(), '1');
    await axe(o.page, 'F8 S26 已暫停（維運者）');
    // 清單：已暫停的下一步
    await o.page.goto(`${URL0}/pulse/rollouts?state=paused`);
    await waitTitle(o.page, '升級推送');
    await o.page.locator(`tr[data-rollout="${rolloutId}"]`).getByText('已暫停：E2E：現場回報異常，先停').waitFor();
    assert.equal(await o.page.getByRole('link', { name: '建立推送' }).count(), 0, '維運者沒有建立推送');
    await axe(o.page, 'F8 S26 推送清單（維運者）');
    await o.ctx.close();

    // 自動暫停（攔截詳情：pause_cause = failure_ratio）：「已自動暫停：…」並附 KRS-W8414；
    // 計數泡泡以清單的 meta 計算（攔截：已暫停 1＋回滾失敗未處理 2 → 3）
    const ap = await newPage();
    await ap.page.route(`**/api/v1/rollouts/${rolloutId}`, async (route) => {
      const r = await route.fetch();
      const body = await r.json();
      body.data.pause_cause = 'failure_ratio';
      body.data.pause_reason = '第 1 批的失敗比例 100%（1／1）超過上限 0%';
      return route.fulfill({ response: r, json: body });
    });
    await ap.page.route('**/api/v1/rollouts', async (route) => {
      if (route.request().method() !== 'GET') return route.continue();
      const r = await route.fetch();
      const body = await r.json();
      body.meta = { ...body.meta, rollback_failed_unacknowledged: 2 };
      return route.fulfill({ response: r, json: body });
    });
    await login(ap.page, 'admin', PW, `/pulse/rollouts/${rolloutId}`);
    await waitTitle(ap.page, 'E2E 0.9.1');
    const autoAlert = ap.page.locator('.ds-alert', { hasText: '已自動暫停：第 1 批的失敗比例 100%（1／1）超過上限 0%' });
    await autoAlert.getByText('KRS-W8414').waitFor();
    const apBadge = ap.page.locator('.ds-sidenav__link', { hasText: '升級推送' }).locator('.ds-sidenav__count');
    await until(async () => (await apBadge.count()) && (await apBadge.textContent()).trim() === '3', 15000, '計數泡泡 3');
    await axe(ap.page, 'F8 S26 已自動暫停');
    await ap.ctx.close();

    // 即時更新（0.10；REMOTE.md 第 8.4 節）：推送詳情以 ?rollout_id= 訂閱 SSE，收到 rollout.host 就地更新那一列，
    // 之後的 rollout.progress 只重新載入頁首與批次、不重新抓取主機表（攔截 SSE：等主機表出現後才送出事件）
    const lv = await newPage();
    let rowReady = null;
    const rowShown = new Promise((resolve) => (rowReady = resolve));
    const watched = [];
    await lv.page.route('**/api/v1/events**', async (route) => {
      const u = new URL(route.request().url());
      if (u.searchParams.get('rollout_id') !== rolloutId) return route.continue();
      watched.push(u.search);
      await rowShown;
      const host = { rollout_id: rolloutId, agent_id: P09, hostname: 'pulse-09', upgrade_id: 'x', wave: 0, ordinal: 0, state: 'accepted', reason: null, from_version: '0.9.0', to_version: VERSION, attempts: 1, progress: { bytes: 18000, total: 40000 }, code: 'PLS-W8404', detail: '', offered_at: null, applying_at: null, finished_at: null, updated_at: new Date().toISOString(), acknowledged_at: null, acknowledged_by: null };
      const progress = { rollout_id: rolloutId, state: 'paused', current_wave: 0, progress: { accepted: 1 }, pause_cause: 'manual', next_action: { code: 'paused', reason: 'x', cause: 'manual' } };
      const body = `retry: 600000\n\nevent: ready\ndata: {"resumed":false}\n\nevent: rollout.host\ndata: ${JSON.stringify(host)}\n\nevent: rollout.progress\ndata: ${JSON.stringify(progress)}\n\n`;
      return route.fulfill({ status: 200, contentType: 'text/event-stream', body });
    });
    let hostLists = 0;
    lv.page.on('request', (rq) => {
      if (rq.url().includes(`/api/v1/rollouts/${rolloutId}/hosts?`)) hostLists += 1;
    });
    await login(lv.page, 'admin', PW, `/pulse/rollouts/${rolloutId}`);
    await waitTitle(lv.page, 'E2E 0.9.1');
    const liveRow = lv.page.locator(`tbody tr[data-key="${P09}"]`);
    await liveRow.waitFor({ timeout: 15000 });
    assert.equal(await liveRow.getByText('PLS-W8404').count(), 0);
    const before = hostLists;
    rowReady();
    await liveRow.getByText('PLS-W8404').waitFor({ timeout: 15000 });
    await liveRow.getByText('已接受').waitFor();
    assert.match(await liveRow.innerText(), /45/, '進度 18000／40000');
    assert.ok(watched.length >= 1 && watched[0].includes(`rollout_id=${rolloutId}`), 'SSE 以 rollout_id 訂閱');
    // rollout.progress 之後 2 秒重新載入頁首與批次；主機表不重新抓取（那一列仍是事件的內容）
    await lv.page.waitForTimeout(3500);
    assert.equal(hostLists, before, '收到 rollout.host 後不重新抓取主機表');
    assert.equal(await liveRow.getByText('PLS-W8404').count(), 1);
    await axe(lv.page, 'F8 S26 推送詳情（即時更新後）');
    await lv.ctx.close();

    // 管理員：繼續 → 取消
    const a = await newPage();
    await login(a.page, 'admin', PW, `/pulse/rollouts/${rolloutId}`);
    await waitTitle(a.page, 'E2E 0.9.1');
    await a.page.getByRole('button', { name: '繼續…' }).first().click();
    await a.page.getByRole('dialog').getByRole('button', { name: '繼續推送' }).click();
    await a.page.getByText('已繼續推送').waitFor();
    await a.page.getByRole('button', { name: '取消推送…' }).waitFor();
    await a.page.getByRole('button', { name: '取消推送…' }).click();
    const cd = a.page.getByRole('dialog');
    await cd.getByLabel(/原因/).fill('E2E 結束');
    await cd.getByRole('button', { name: '取消推送' }).click();
    await a.page.getByText('已取消推送').first().waitFor();
    await a.page.getByText('取消原因：E2E 結束').waitFor();
    // S05 的「升級」卡片：最近的升級連到這個推送
    await a.page.goto(`${URL0}/hosts/${P09}`);
    await waitTitle(a.page, 'pulse-09');
    const up = a.page.locator('section', { has: a.page.getByRole('heading', { name: '升級', exact: true }) });
    await up.getByText('tar', { exact: true }).waitFor();
    await up.getByText('就緒').waitFor();
    await up.getByRole('link', { name: rolloutId }).waitFor();
    await axe(a.page, 'F8 S05 升級卡片');
    // 刪除版本（L2：輸入版本號）
    await a.page.goto(`${URL0}/pulse/releases/${VERSION}`);
    await waitTitle(a.page, `Pulse ${VERSION}`);
    await a.page.getByRole('button', { name: '刪除版本…' }).click();
    const dd = a.page.getByRole('dialog');
    const del = dd.getByRole('button', { name: '刪除版本' });
    assert.ok(await del.isDisabled());
    await axe(a.page, 'F8 S25 刪除版本（L2）');
    await dd.getByLabel(/輸入/).fill(VERSION);
    await del.click();
    await a.page.getByText(`已刪除 Pulse ${VERSION}`).waitFor();
    await waitTitle(a.page, 'Pulse 版本庫');
    await a.ctx.close();
  });

  test('F8 S24 集中設定（維運者）：新增設定檔 → 指派全站 → 第 2 版 → 回復 → S05 設定分頁 → 解除指派、刪除', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'operator', PW, '/pulse/config');
    await waitTitle(page, '集中設定');
    await page.getByText('還沒有設定檔').waitFor();
    await page.locator('#config-sync').waitFor();
    await axe(page, 'F8 S24 集中設定（空）');
    await page.getByRole('link', { name: '新增設定檔' }).first().click();
    await waitTitle(page, '新增設定檔');
    await page.getByLabel(/^名稱/).fill('E2E 一般');
    await page.getByLabel('說明', { exact: true }).fill('E2E 的回報間隔');
    const add = async (key) => {
      await page.getByLabel('新增設定', { exact: true }).selectOption(key);
      await page.getByRole('button', { name: '加入' }).click();
    };
    await add('report.interval');
    const interval = page.getByLabel('report.interval', { exact: true });
    await interval.fill('0');
    await add('log.level');
    await page.getByLabel('log.level', { exact: true }).selectOption('debug');
    await add('processes.command');
    // 隱私敏感：只列出更保守的值（full 只能在本機設定）
    assert.deepEqual(await page.getByLabel('processes.command', { exact: true }).locator('option').allTextContents(), ['none', 'executable']);
    await page.locator('li[data-key="processes.command"]').getByText(/隱私敏感：只能下發更保守的值/).waitFor();
    await page.getByText('新的設定檔還沒有指派').waitFor();
    await page.getByRole('button', { name: '儲存', exact: true }).click();
    await page.getByText('請輸入 1～3,600 的整數').waitFor();
    await interval.fill('10');
    await axe(page, 'F8 S24 新增設定檔');
    await page.getByRole('button', { name: '儲存', exact: true }).click();
    await page.getByText('已新增設定檔「E2E 一般」').waitFor();
    await waitTitle(page, 'E2E 一般');
    await page.getByText('第 1 版').first().waitFor();
    profileId = new URL(page.url()).pathname.split('/').pop();
    // 指派全站（影響預覽）
    await page.goto(`${URL0}/pulse/config/assignments`);
    await waitTitle(page, '集中設定');
    await page.getByRole('button', { name: '新增指派' }).first().click();
    const dlg = page.getByRole('dialog');
    await dlg.getByText(/^影響預覽：/).waitFor({ timeout: 15000 });
    await axe(page, 'F8 S24 新增指派');
    await dlg.getByRole('button', { name: '指派', exact: true }).click();
    await page.getByText('已指派「E2E 一般」給全站').waitFor();
    await page.locator('#config-assignments').getByText('全站').waitFor();
    await axe(page, 'F8 S24 指派');
    // S04：pulse-09 有期望設定、尚未回報（沒有 mTLS，不協商 config）。主機清單的索引與資料庫的設定狀態定期同步
    await until(async () => {
      const r = await api(page, 'GET', '/api/v1/hosts?config_state=never_reported');
      return r.status === 200 && r.json.data.some((x) => x.agent_id === P09);
    }, 45000, '主機清單的設定狀態');
    await page.goto(`${URL0}/hosts?config_state=never_reported`);
    await waitTitle(page, '主機');
    await page.getByText('設定狀態＝尚未回報').waitFor();
    await page.locator(`tbody tr[data-key="${P09}"]`).waitFor();
    assert.equal(await page.locator(`tbody tr[data-key="${WEB}"]`).count(), 0);
    await page.getByLabel('遠端升級').selectOption('ready');
    await page.getByText('遠端升級＝就緒').waitFor();
    await page.locator(`tbody tr[data-key="${P09}"]`).waitFor();
    await axe(page, 'F8 S04 設定狀態與遠端升級篩選');
    // 修改 → 第 2 版（影響預覽），版本歷史
    await page.goto(`${URL0}/pulse/config/${profileId}`);
    await waitTitle(page, 'E2E 一般');
    await page.getByLabel('report.interval', { exact: true }).fill('20');
    await page.locator('#config-impact').getByText(/^影響預覽：/).waitFor();
    await page.getByLabel('變更原因（選填）').fill('E2E 調整');
    await page.getByRole('button', { name: '儲存', exact: true }).click();
    await page.getByText('已儲存設定檔「E2E 一般」（第 2 版）').waitFor();
    const rev2 = page.locator('li[data-revision="2"]');
    await rev2.getByText('10 → 20').waitFor();
    await rev2.getByText('原因：E2E 調整').waitFor();
    // 有指派：刪除為 aria-disabled 並說明
    await page.getByText('請先解除 1 個指派').waitFor();
    assert.equal(await page.getByRole('button', { name: '刪除設定檔…' }).getAttribute('aria-disabled'), 'true');
    await axe(page, 'F8 S24 設定檔編輯');
    await shot(page, 'f8-config-edit');
    // 回復到第 1 版（L1）
    await page.getByRole('button', { name: '回復到這一版…（第 1 版）' }).click();
    await page.getByRole('dialog').getByRole('button', { name: '回復', exact: true }).click();
    await page.getByText('已回復到第 1 版的內容（第 3 版）').waitFor();
    assert.equal(await page.getByLabel('report.interval', { exact: true }).inputValue(), '10');
    // 未儲存的變更：離開時詢問
    await page.getByLabel('report.interval', { exact: true }).fill('30');
    await page.locator('.ds-sidenav__link', { hasText: '升級推送' }).click();
    await page.getByRole('dialog').getByText('要放棄變更嗎？').waitFor();
    await page.getByRole('dialog').getByRole('button', { name: '取消' }).click();
    await waitTitle(page, 'E2E 一般');
    await page.getByLabel('report.interval', { exact: true }).fill('10');
    // S05「設定」分頁：期望設定與來源
    await page.goto(`${URL0}/hosts/${P09}/config`);
    await waitTitle(page, 'pulse-09');
    await page.getByText(/尚未回報：主機還沒有回報第 \d+ 版的套用結果/).waitFor();
    const keyRow = page.locator('#host-config tr[data-key="report.interval"]');
    await keyRow.getByText('設定檔：E2E 一般').waitFor();
    await axe(page, 'F8 S05 設定分頁');
    // 解除指派（L1）→ 刪除設定檔（L1）
    await page.goto(`${URL0}/pulse/config/assignments`);
    await page.getByRole('button', { name: /^解除指派：「E2E 一般」對全站/ }).click();
    await page.getByRole('dialog').getByRole('button', { name: '解除指派' }).click();
    await page.getByText('已解除指派').waitFor();
    await page.goto(`${URL0}/pulse/config/${profileId}`);
    await waitTitle(page, 'E2E 一般');
    await page.getByRole('button', { name: '刪除設定檔…' }).click();
    await page.getByRole('dialog').getByRole('button', { name: '刪除設定檔' }).click();
    await page.getByText('已刪除設定檔「E2E 一般」').waitFor();
    await waitTitle(page, '集中設定');
    await ctx.close();
  });

  test('F8 權限與功能關閉：稽核員唯讀、唯讀看不到 Pulse 管理；503 remote_unavailable 的空狀態', async () => {
    // 稽核員：唯讀
    const { ctx, page } = await newPage();
    const created = await (async () => {
      const a = await newPage();
      await login(a.page, 'admin', PW, '/pulse/config');
      await waitTitle(a.page, '集中設定');
      const r = await api(a.page, 'POST', '/api/v1/config/profiles', { name: 'E2E 稽核', settings: { log: { level: 'info' } } });
      await a.ctx.close();
      return r.json.data;
    })();
    await login(page, 'auditor', PW, `/pulse/config/${created.id}`);
    await waitTitle(page, 'E2E 稽核');
    await page.getByText('你可以檢視設定檔；修改需要「修改集中設定」').waitFor();
    assert.ok(await page.getByLabel('log.level', { exact: true }).isDisabled());
    assert.equal(await page.getByRole('button', { name: /^(儲存|刪除設定檔…)$/ }).count(), 0);
    await axe(page, 'F8 S24 稽核員');
    await page.goto(`${URL0}/pulse/config`);
    await waitTitle(page, '集中設定');
    assert.equal(await page.getByRole('link', { name: '新增設定檔' }).count(), 0);
    const w = await api(page, 'DELETE', `/api/v1/config/profiles/${created.id}`, {});
    assert.equal(w.status, 403);
    // 全域搜尋（0.10）：推送（名稱、編號、目標版本）與設定檔（名稱、說明）；稽核員有 rollouts:read、config:read
    await page.keyboard.press('Control+k');
    await page.keyboard.type('E2E');
    const found = page.getByRole('listbox', { name: '搜尋結果' });
    await found.getByRole('group', { name: '升級推送' }).getByRole('option', { name: /E2E 0\.9\.1/ }).waitFor();
    await found.getByRole('group', { name: '設定檔' }).getByRole('option', { name: /E2E 稽核/ }).waitFor();
    await axe(page, '全域搜尋（推送與設定檔）');
    await found.getByRole('option', { name: /E2E 稽核/ }).click();
    await waitTitle(page, 'E2E 稽核');
    assert.equal(new URL(page.url()).pathname, `/pulse/config/${created.id}`);
    await page.keyboard.press('Control+k');
    await page.keyboard.type(rolloutId);
    await page.getByRole('option', { name: /E2E 0\.9\.1/ }).click();
    await waitTitle(page, 'E2E 0.9.1');
    assert.equal(new URL(page.url()).pathname, `/pulse/rollouts/${rolloutId}`);
    // 唯讀角色（沒有 rollouts:read、config:read）：不查詢這兩個清單，也沒有結果
    const vs = await newPage();
    const asked = [];
    vs.page.on('request', (rq) => {
      const p = new URL(rq.url()).pathname;
      if (p === '/api/v1/rollouts' || p === '/api/v1/config/profiles') asked.push(p);
    });
    await login(vs.page, 'viewer', PW, '/hosts');
    await waitTitle(vs.page, '主機');
    await vs.page.keyboard.press('Control+k');
    await vs.page.keyboard.type('E2E 0.9');
    await vs.page.getByText('沒有符合「E2E 0.9」的結果。').waitFor();
    assert.deepEqual(asked, []);
    await vs.page.keyboard.press('Escape');
    // S04 agent 版本欄的 ⚠（0.10）：低於建議的最低版本（攔截主機清單：建議 0.9.5，全部標為低於）；只需要 hosts:read
    await vs.page.route('**/api/v1/hosts?**', async (route) => {
      const r = await route.fetch();
      const body = await r.json();
      if (!body.meta || !Array.isArray(body.data)) return route.fulfill({ response: r });
      assert.ok('recommended_min_agent_version' in body.meta, 'meta.recommended_min_agent_version');
      assert.ok(body.data.every((x) => typeof x.agent_outdated === 'boolean'), 'agent_outdated');
      body.meta.recommended_min_agent_version = '0.9.5';
      for (const x of body.data) x.agent_outdated = x.agent_id === P09;
      return route.fulfill({ response: r, json: body });
    });
    await vs.page.goto(`${URL0}/hosts?q=pulse-09`);
    await waitTitle(vs.page, '主機');
    const flagged = vs.page.locator(`tbody tr[data-key="${P09}"] .k-version-flag`);
    await flagged.waitFor({ timeout: 15000 });
    assert.match(await flagged.getAttribute('title'), /低於建議的最低版本 0\.9\.5/);
    assert.match(await flagged.innerText(), /低於建議的最低版本 0\.9\.5/, '螢幕報讀的文字');
    assert.equal(await vs.page.locator('.k-version-flag').count(), 1, '其他主機不標示');
    await axe(vs.page, 'S04 低於建議版本');
    await vs.ctx.close();
    // S05 的「升級」卡片：回滾失敗（攔截 API）→ 稽核員沒有「標記為已處理…」
    await page.route(`**/api/v1/hosts/${P09}/upgrade`, (route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          server_time: new Date().toISOString(),
          data: { agent_id: P09, agent_version: '0.9.0', install: { method: 'tar', upgrade: 'ready', config: 'enabled' }, rollback_failed: true, upgrades: [{ rollout_id: 'r-20261019-0099', agent_id: P09, hostname: 'pulse-09', state: 'rollback_failed', from_version: '0.9.0', to_version: '0.9.1', code: 'PLS-E8408', updated_at: new Date().toISOString(), finished_at: new Date().toISOString(), acknowledged_at: null }] },
        }),
      }),
    );
    await page.goto(`${URL0}/hosts/${P09}`);
    await waitTitle(page, 'pulse-09');
    await page.getByText('升級回滾失敗，需要人工處理').waitFor();
    assert.equal(await page.getByRole('button', { name: '標記為已處理…' }).count(), 0);
    await ctx.close();
    // 管理員：標記為已處理（L1，原因必填；攔截 acknowledge）
    const a = await newPage();
    await login(a.page, 'admin', PW, '/account');
    await waitTitle(a.page, '個人設定');
    let acked = null;
    await a.page.route(`**/api/v1/hosts/${P09}/upgrade`, (route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          server_time: new Date().toISOString(),
          data: { agent_id: P09, agent_version: '0.9.0', install: { method: 'tar', upgrade: 'ready', config: 'enabled' }, rollback_failed: !acked, upgrades: [{ rollout_id: 'r-20261019-0099', agent_id: P09, hostname: 'pulse-09', state: 'rollback_failed', from_version: '0.9.0', to_version: '0.9.1', code: 'PLS-E8408', updated_at: new Date().toISOString(), finished_at: new Date().toISOString(), acknowledged_at: acked ? new Date().toISOString() : null }] },
        }),
      }),
    );
    await a.page.route('**/api/v1/rollouts/r-20261019-0099/hosts/*/acknowledge', (route) => {
      acked = JSON.parse(route.request().postData() || '{}');
      return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ server_time: new Date().toISOString(), data: { ok: true } }) });
    });
    await a.page.goto(`${URL0}/hosts/${P09}`);
    await waitTitle(a.page, 'pulse-09');
    await a.page.getByText('升級回滾失敗，需要人工處理').waitFor();
    await axe(a.page, 'F8 S05 升級回滾失敗');
    await a.page.getByRole('button', { name: '標記為已處理…' }).click();
    const dlg = a.page.getByRole('dialog');
    await dlg.getByRole('button', { name: '標記為已處理' }).click();
    await dlg.getByText('請填寫原因').waitFor();
    await dlg.getByLabel(/原因/).fill('E2E：現場重新安裝 Pulse 0.9.0');
    await dlg.getByRole('button', { name: '標記為已處理' }).click();
    await a.page.getByText('已標記為已處理').waitFor();
    assert.equal(acked && acked.note, 'E2E：現場重新安裝 Pulse 0.9.0');
    await a.page.getByText('升級回滾失敗，需要人工處理').waitFor({ state: 'detached' });
    await a.page.unroute(`**/api/v1/hosts/${P09}/upgrade`);
    await api(a.page, 'DELETE', `/api/v1/config/profiles/${created.id}`, {});
    // 功能關閉（503 remote_unavailable）：整頁 EmptyState，「Pulse 管理」區塊仍顯示
    const off = (route) => route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ server_time: new Date().toISOString(), error: { code: 'remote_unavailable', message: '遠端管理無法使用：集中設定已停用（remote.config_enabled = false）' } }) });
    await a.page.route('**/api/v1/config/**', off);
    await a.page.route('**/api/v1/rollouts**', off);
    await a.page.route('**/api/v1/releases**', off);
    for (const [path, title, text] of [
      ['/pulse/config', '集中設定', '集中設定未啟用'],
      ['/pulse/rollouts', '升級推送', '遠端升級未啟用'],
      ['/pulse/releases', 'Pulse 版本庫', '遠端升級未啟用'],
    ]) {
      await a.page.goto(`${URL0}${path}`);
      await waitTitle(a.page, title);
      await a.page.locator('.ds-empty__title', { hasText: text }).waitFor();
      assert.equal(await a.page.locator('.ds-sidenav__heading', { hasText: 'Pulse 管理' }).count(), 1);
    }
    assert.equal(await a.page.getByRole('link', { name: '建立推送' }).count(), 0);
    await axe(a.page, 'F8 功能關閉');
    await a.page.emulateMedia({ colorScheme: 'dark' });
    await axe(a.page, 'F8 功能關閉（深色）');
    await a.ctx.close();
    // 唯讀：看不到 Pulse 管理，直接存取 403；S04 沒有設定狀態與遠端升級篩選
    const v = await newPage();
    await login(v.page, 'viewer', PW, '/hosts');
    await waitTitle(v.page, '主機');
    assert.equal(await v.page.locator('.ds-sidenav__heading', { hasText: 'Pulse 管理' }).count(), 0);
    assert.equal(await v.page.getByLabel('設定狀態').count(), 0);
    assert.equal(await v.page.getByLabel('遠端升級').count(), 0);
    await v.page.goto(`${URL0}/pulse/rollouts`);
    await v.page.locator('#page-title', { hasText: '沒有權限檢視這個畫面' }).waitFor();
    await v.page.goto(`${URL0}/hosts/${P09}/config`);
    await v.page.locator('#page-title', { hasText: '沒有權限檢視這個畫面' }).waitFor();
    // 主機詳情：「設定」分頁不顯示；升級卡片只有摘要（沒有 rollouts:read 時不呼叫升級紀錄）
    await v.page.goto(`${URL0}/hosts/${P09}`);
    await waitTitle(v.page, 'pulse-09');
    assert.equal(await v.page.getByRole('link', { name: '設定', exact: true }).count(), 0);
    await v.page.locator('section', { has: v.page.getByRole('heading', { name: '升級', exact: true }) }).getByText('就緒').waitFor();
    await v.ctx.close();
    if (agent) agent.close();
    agent = null;
    await until(async () => true);
  });
}
