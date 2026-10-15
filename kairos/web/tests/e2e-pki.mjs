// 註冊與憑證（S12～S15）的端對端測試：對 run.sh 啟動的第二個 Kairos（pki-env.sh：mTLS、註冊端點、線上簽發）。
// 由 e2e.mjs 註冊到同一個執行器（共用 newPage、axe、截圖與主控台錯誤檢查）。
//
// 流程：建立 token（只顯示一次、清單只見前 4 碼、不進 storage／網址／日誌）→ 以實際的 pulse 執行檔送出註冊申請 →
// 佇列 → 核准 → 憑證清單；測試用 HTTPS 用戶端送出主機名稱含 <script> 的申請（XSS）→ 拒絕（L2）；
// 撤銷憑證（L2 確認對話框）→ CRL 新版本；唯讀／稽核員看不到寫入按鈕、直接呼叫 403；深色主題 axe。

import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomUUID, randomBytes } from 'node:crypto';
import https from 'node:https';
import assert from 'node:assert/strict';

const BASE = process.env.KAIROS_PKI_URL;
const PW = process.env.PW;
const ENROLL_PORT = Number(process.env.PKI_ENROLL_PORT);
const PULSE_BIN = process.env.PULSE_BIN;
const PKI_LOG = process.env.PKI_LOG;
const XSS_NAME = '<script>window.__xss=1</script><img src=x onerror="window.__xss=2">';

// 測試之間共用（只在記憶體）
const S = { token: null, install: null, tokenName: 'B廠 2026Q4 部署', pulseAgent: null, pulseHost: null };

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const b64url = (buf) => Buffer.from(buf).toString('base64').replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');

async function login(page, user, next = '/') {
  await page.goto(`${BASE}/login?next=${encodeURIComponent(next)}`);
  await page.getByLabel('帳號').fill(user);
  await page.locator('input[name="password"]').fill(PW);
  await page.getByRole('button', { name: '登入', exact: true }).click();
}

async function title(page, text) {
  await page.locator('#page-title', { hasText: text }).waitFor({ timeout: 15000 });
}

/** 以頁面的工作階段（cookie＋CSRF）直接呼叫 API，回傳 HTTP 狀態碼 */
async function direct(page, method, path, body) {
  return page.evaluate(
    async ({ method, path, body }) => {
      const me = await (await fetch('/api/v1/me')).json();
      const r = await fetch(path, {
        method,
        headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': me.data.csrf_token },
        body: method === 'GET' ? undefined : JSON.stringify(body || {}),
      });
      return r.status;
    },
    { method, path, body },
  );
}

/** 8444 的請求（測試用 HTTPS 用戶端；伺服器憑證由測試根 CA 簽發，這裡不驗證） */
function enrollPost(path, body) {
  return new Promise((resolve, reject) => {
    const data = JSON.stringify(body);
    const req = https.request(
      { host: '127.0.0.1', port: ENROLL_PORT, path, method: 'POST', servername: 'localhost', rejectUnauthorized: false, headers: { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(data) } },
      (res) => {
        let buf = '';
        res.on('data', (c) => (buf += c));
        res.on('end', () => {
          try {
            resolve({ status: res.statusCode, body: JSON.parse(buf) });
          } catch {
            resolve({ status: res.statusCode, body: buf });
          }
        });
      },
    );
    req.on('error', reject);
    req.end(data);
  });
}

/** P-256 金鑰與 CSR（SAN＝DNS:<agent_id>；以 openssl 產生） */
function makeCsr(agentId) {
  const dir = mkdtempSync(join(tmpdir(), 'kairos-csr-'));
  execFileSync('openssl', ['genpkey', '-algorithm', 'EC', '-pkeyopt', 'ec_paramgen_curve:P-256', '-out', join(dir, 'k.pem')], { stdio: 'ignore' });
  execFileSync('openssl', ['req', '-new', '-key', join(dir, 'k.pem'), '-subj', `/CN=${agentId}`, '-addext', `subjectAltName=DNS:${agentId}`, '-outform', 'DER', '-out', join(dir, 'csr.der')], { stdio: 'ignore' });
  return readFileSync(join(dir, 'csr.der'));
}

/** 等到清單出現符合的列（佇列是載入時的快照：重新整理） */
async function waitRow(page, url, text, timeout = 30000) {
  const end = Date.now() + timeout;
  for (;;) {
    await page.goto(url);
    const row = page.locator('tbody tr[data-key]', { hasText: text });
    try {
      await row.first().waitFor({ timeout: 3000 });
      return row.first();
    } catch (e) {
      if (Date.now() > end) throw e;
    }
  }
}

export function pkiTests({ test, newPage, axe, shot }) {
  test('S12 註冊 token：建立 → 只顯示一次 → 清單只見前 4 碼（不進 storage、網址、日誌）', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'admin', '/enrollment/tokens');
    await title(page, '註冊 token');
    await page.getByText('還沒有註冊 token').waitFor();
    // 側邊導覽：註冊與憑證
    assert.deepEqual(await page.locator('.ds-sidenav__section', { hasText: '註冊與憑證' }).locator('.ds-sidenav__label').allTextContents(), ['待核准', '註冊 token', '憑證', 'CA 與撤銷清單']);
    await axe(page, 'S12 註冊 token（空）');
    await page.getByRole('button', { name: '建立 token' }).first().click();
    const dlg = page.locator('dialog[open]');
    await dlg.locator('input[name="name"]').fill(S.tokenName);
    await dlg.locator('input[name="group"]').fill('B廠產線');
    // 必填檢查：使用次數上限
    await dlg.getByRole('button', { name: '建立 token' }).click();
    await dlg.getByText('請輸入 1～100,000 的整數').waitFor();
    await dlg.locator('input[name="max_uses"]').fill('10');
    await axe(page, 'S12 建立 token 表單');
    await dlg.getByRole('button', { name: '建立 token' }).click();
    // 建立完成：SecretField 預設遮蔽，「顯示」後才看得到
    const secret = page.locator('#tk-secret');
    await secret.waitFor();
    assert.equal(await secret.getAttribute('type'), 'password');
    await page.getByRole('button', { name: '顯示' }).click();
    assert.equal(await secret.getAttribute('type'), 'text');
    S.token = await secret.inputValue();
    assert.match(S.token, /^KRT1-[0-9A-Z]{4}-/);
    S.install = (await page.locator('#tk-install code').innerText()).trim();
    assert.match(S.install, /^pulse-enroll:v1:/);
    await page.getByText('token 只會顯示這一次，請現在複製').waitFor();
    // 下載端點未提供：不顯示一行指令與下載，改為 pulse enroll --install-string
    await page.getByText('這個版本的註冊端點不提供安裝腳本與套件下載').waitFor();
    assert.equal(await page.locator('dialog[open]').getByText('下載指令檔').count(), 0);
    await page.locator('dialog[open] pre', { hasText: 'pulse enroll --install-string' }).waitFor();
    await axe(page, 'S12 建立完成（只顯示一次）');
    await shot(page, 'pki-token-created');
    const leak = async () =>
      page.evaluate((tok) => {
        const st = [];
        for (const s of [localStorage, sessionStorage]) for (let i = 0; i < s.length; i += 1) st.push(s.key(i), s.getItem(s.key(i)));
        return { storage: st.join('\n').includes(tok), url: location.href.includes(tok), title: document.title.includes(tok) };
      }, S.token);
    assert.deepEqual(await leak(), { storage: false, url: false, title: false });
    await page.getByRole('button', { name: '完成' }).click();
    // 清單：只有前 4 碼（KRT1-XXXX…），DOM 中不再有完整的 token
    const row = page.locator('tbody tr[data-key]', { hasText: S.tokenName });
    await row.waitFor();
    const hint = `${S.token.slice(0, 9)}…`;
    await row.getByText(hint, { exact: true }).waitFor();
    const inDom = await page.evaluate((tok) => document.documentElement.outerHTML.includes(tok), S.token);
    assert.equal(inDom, false, '關閉後 DOM 不應再有完整的 token');
    assert.deepEqual(await leak(), { storage: false, url: false, title: false });
    await page.reload();
    await row.waitFor();
    assert.equal(await page.evaluate((tok) => document.documentElement.outerHTML.includes(tok), S.token), false);
    // 伺服器日誌也沒有 token 全文
    assert.equal(readFileSync(PKI_LOG, 'utf8').includes(S.token), false, 'Kairos 的日誌不應有 token 全文');
    await axe(page, 'S12 註冊 token 清單');
    // 詳情：部署進度漏斗
    await row.getByRole('link', { name: S.tokenName }).click();
    await title(page, S.tokenName);
    await page.locator('.k-funnel [data-step="used"]').waitFor();
    await axe(page, 'S12 token 詳情');
    await ctx.close();
  });

  test('S13 待核准：實際的 pulse 送出註冊申請 → 佇列 → 核准 → 憑證清單', async () => {
    assert.ok(S.install, '需要上一個測試建立的 token');
    const host = mkdtempSync(join(tmpdir(), 'pulse-web-'));
    writeFileSync(join(host, 'pulse.toml'), '[retry]\ninitial_delay = 1\nmax_delay = 2\n\n[status]\nfile = "state/status.json"\n\n[log]\nlevel = "debug"\n');
    const child = spawn(PULSE_BIN, ['enroll', '--install-string', S.install, '--wait', '120', '--non-interactive', '--json'], { cwd: host, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let err = '';
    child.stdout.on('data', (c) => (out += c));
    child.stderr.on('data', (c) => (err += c));
    const exited = new Promise((resolve) => child.on('exit', (code) => resolve(code)));
    try {
      for (let i = 0; i < 100 && !existsSync(join(host, 'agent.id')); i += 1) await sleep(200);
      S.pulseAgent = readFileSync(join(host, 'agent.id'), 'utf8').trim();
      const { ctx, page } = await newPage();
      await login(page, 'operator', '/enrollment/pending');
      await title(page, '待核准');
      const prefix = S.pulseAgent.slice(0, 8);
      const row = await waitRow(page, `${BASE}/enrollment/pending?q=${prefix}`, prefix);
      S.pulseHost = (await row.locator('button[data-open]').innerText()).trim();
      // 詳情：申請編號大字、可複製
      await row.locator('button[data-open]').click();
      const dlg = page.locator('dialog[open]');
      await dlg.locator('.k-req-code').waitFor();
      assert.match(await dlg.locator('.k-req-code').innerText(), /^[0-9A-Z]{4}-[0-9A-Z]{4}$/);
      await axe(page, 'S13 申請詳情');
      await dlg.getByRole('button', { name: '關閉' }).last().click();
      await axe(page, 'S13 待核准');
      await shot(page, 'pki-pending');
      // 勾選 → 批次核准（L1）
      await row.locator('input[type="checkbox"]').check();
      await page.getByRole('button', { name: /^核准 1 筆/ }).click();
      await page.locator('dialog[open]').getByRole('button', { name: '核准', exact: true }).click();
      await page.locator('.ds-table__summary[role="status"]', { hasText: '已核准 1 筆註冊申請' }).waitFor();
      // pulse 取得憑證後結束（0）
      const code = await Promise.race([exited, sleep(60000).then(() => 'timeout')]);
      assert.equal(code, 0, `pulse enroll 結束代碼 ${code}\n${out}\n${err.slice(-2000)}`);
      // 憑證清單出現（有效）
      await page.goto(`${BASE}/certificates?q=${prefix}`);
      await title(page, '憑證');
      const crow = page.locator('tbody tr[data-key]', { hasText: S.pulseHost });
      await crow.waitFor();
      await crow.getByText('有效', { exact: true }).waitFor();
      await axe(page, 'S14 憑證');
      await shot(page, 'pki-certificates');
      await ctx.close();
    } finally {
      if (child.exitCode == null) child.kill();
    }
  });

  test('S13 XSS：主機名稱含 <script> 的申請以文字顯示 → 拒絕（L2 輸入確認）', async () => {
    assert.ok(S.token, '需要建立的 token');
    const agentId = randomUUID();
    const r = await enrollPost('/v1/enroll/requests', {
      token: S.token,
      agent_id: agentId,
      csr: b64url(makeCsr(agentId)),
      poll_secret: b64url(randomBytes(32)),
      sent_at: new Date().toISOString(),
      hostname: XSS_NAME,
      pulse_version: '0.7.0',
      server_verification: 'tofu',
      host_fingerprint: `sha256:${'ab'.repeat(32)}`,
    });
    assert.equal(r.status, 200, JSON.stringify(r.body));
    assert.equal(r.body.state, 'pending_approval');
    const { ctx, page } = await newPage();
    await login(page, 'admin', '/enrollment/pending');
    await title(page, '待核准');
    const prefix = agentId.slice(0, 8);
    const row = await waitRow(page, `${BASE}/enrollment/pending?q=${prefix}`, prefix);
    assert.equal((await row.locator('button[data-open]').innerText()).trim(), XSS_NAME);
    // 側邊導覽「待核准」的計數泡泡（ia.md §7）：與 API 的待核准數相同，數字在連結的可存取名稱中
    const pendingTotal = () => page.evaluate(async () => (await (await fetch('/api/v1/enroll/requests?view=pending&state=pending_approval&limit=1')).json()).page.total);
    const navPending = page.locator('.ds-sidenav__link', { hasText: '待核准' });
    const before = await pendingTotal();
    assert.ok(before >= 1, `待核准數 ${before}`);
    await navPending.locator('[data-count="pending"]', { hasText: String(before) }).waitFor();
    assert.match(await navPending.textContent(), new RegExp(`，${before} 筆待核准`));
    // 未驗證伺服器身分（TOFU）的旗標
    await row.getByText('未驗證伺服器身分').waitFor();
    await row.locator('button[data-open]').click();
    await page.locator('dialog[open]').getByText(XSS_NAME, { exact: true }).first().waitFor();
    await page.locator('dialog[open]').getByRole('button', { name: '關閉' }).last().click();
    assert.equal(await page.evaluate(() => window.__xss), undefined);
    // 拒絕（L2）：輸入主機名稱前確認按鈕停用；原因必填
    await row.locator('input[type="checkbox"]').check();
    await page.getByRole('button', { name: '拒絕…' }).click();
    const dlg = page.locator('dialog[open]');
    const ok = dlg.getByRole('button', { name: '拒絕', exact: true });
    assert.equal(await ok.isDisabled(), true);
    await dlg.locator('input[id^="phrase"]').fill(XSS_NAME);
    assert.equal(await ok.isDisabled(), false);
    await axe(page, 'S13 拒絕（L2）');
    await ok.click();
    await dlg.getByText('請填寫拒絕的說明').waitFor();
    await dlg.locator('textarea[name="reason"]').fill('不明主機（e2e）');
    await ok.click();
    await page.locator('.ds-table__summary[role="status"]', { hasText: '已拒絕 1 筆註冊申請' }).waitFor();
    // 拒絕後計數泡泡立即更新（少 1；為 0 時不顯示）
    if (before > 1) await navPending.locator('[data-count="pending"]', { hasText: String(before - 1) }).waitFor();
    else await navPending.locator('[data-count="pending"]').waitFor({ state: 'detached' });
    // 最近結束：已拒絕
    await page.goto(`${BASE}/enrollment/pending?view=closed&q=${prefix}`);
    const closed = page.locator('tbody tr[data-key]', { hasText: XSS_NAME });
    await closed.first().waitFor();
    await closed.first().getByText('已拒絕').waitFor();
    assert.equal(await page.evaluate(() => window.__xss), undefined);
    await axe(page, 'S13 最近結束');
    await ctx.close();
  });

  test('S14 撤銷憑證（L2 確認對話框）→ S15 CRL 新版本', async () => {
    assert.ok(S.pulseHost, '需要 pulse 註冊的主機');
    const { ctx, page } = await newPage();
    await login(page, 'admin', '/certificates/ca');
    await title(page, 'CA 與撤銷清單');
    const before = Number((await page.locator('.ds-card dd .mono', { hasText: /^#\d+$/ }).first().innerText()).slice(1));
    await page.goto(`${BASE}/certificates?q=${S.pulseAgent.slice(0, 8)}`);
    const row = page.locator('tbody tr[data-key]', { hasText: S.pulseHost });
    await row.waitFor();
    await row.getByRole('button', { name: `更多動作：${S.pulseHost}` }).click();
    await page.getByRole('menuitem', { name: '撤銷憑證…' }).click();
    const dlg = page.locator('dialog[open]');
    await dlg.getByText('撤銷後這些主機會在數秒內被中斷連線').waitFor();
    const ok = dlg.getByRole('button', { name: '撤銷憑證', exact: true });
    assert.equal(await ok.isDisabled(), true);
    await dlg.locator('select[name="reason"]').selectOption('cessationOfOperation');
    await dlg.locator('textarea[name="comment"]').fill('e2e 除役');
    await dlg.locator('input[id^="phrase"]').fill(S.pulseHost);
    await axe(page, 'S14 撤銷（L2）');
    await ok.click();
    await page.locator('.ds-table__summary[role="status"]', { hasText: '已撤銷 1 張憑證' }).waitFor();
    await page.goto(`${BASE}/certificates?state=revoked`);
    await page.locator('tbody tr[data-key]', { hasText: S.pulseHost }).getByText('已撤銷').waitFor();
    // 詳情（/certificates/:serial）：撤銷原因
    await page.locator('tbody tr[data-key]', { hasText: S.pulseHost }).locator('button[data-open]').click();
    await page.waitForURL(/\/certificates\/[0-9A-F]+$/);
    await page.locator('dialog[open]').getByText('除役（cessationOfOperation）').waitFor();
    await axe(page, 'S14 憑證詳情');
    await page.locator('dialog[open]').getByRole('button', { name: '關閉' }).last().click();
    await page.waitForURL(/\/certificates\?state=revoked$/);
    // S15：CRL 版本增加、撤銷 1 筆
    await page.goto(`${BASE}/certificates/ca`);
    await title(page, 'CA 與撤銷清單');
    // 撤銷後數秒內產生新版（多筆撤銷在 2 秒內合併）
    let after = before;
    for (let i = 0; i < 20 && after <= before; i += 1) {
      await page.goto(`${BASE}/certificates/ca`);
      await title(page, 'CA 與撤銷清單');
      after = Number((await page.locator('.ds-card dd .mono', { hasText: /^#\d+$/ }).first().innerText()).slice(1));
      if (after <= before) await sleep(500);
    }
    assert.ok(after > before, `CRL 版本 ${before} → ${after}`);
    await page.getByText('1 筆', { exact: true }).first().waitFor();
    await page.getByRole('button', { name: '立即更新 CRL' }).click();
    await page.locator('dialog[open]').getByRole('button', { name: '立即更新 CRL' }).click();
    await page.getByText(/已產生新的 CRL/).waitFor();
    await axe(page, 'S15 CA 與撤銷清單');
    await shot(page, 'pki-ca');
    await ctx.close();
  });

  test('S12～S15 權限：唯讀看不到、稽核員看不到寫入按鈕、直接呼叫 403；維運者不能建立自動核准', async () => {
    // 唯讀：沒有「註冊與憑證」，直接存取 403
    {
      const { ctx, page } = await newPage();
      await login(page, 'viewer', '/');
      await title(page, '總覽');
      assert.equal(await page.locator('.ds-sidenav__heading', { hasText: '註冊與憑證' }).count(), 0);
      for (const p of ['/enrollment/tokens', '/enrollment/pending', '/certificates', '/certificates/ca']) {
        await page.goto(`${BASE}${p}`);
        await title(page, '沒有權限檢視這個畫面');
      }
      assert.equal(await direct(page, 'GET', '/api/v1/enroll/tokens'), 403);
      assert.equal(await direct(page, 'GET', '/api/v1/pki/certificates'), 403);
      await ctx.close();
    }
    // 稽核員：唯讀檢視
    {
      const { ctx, page } = await newPage();
      await login(page, 'auditor', '/enrollment/tokens');
      await title(page, '註冊 token');
      await page.locator('tbody tr[data-key]').first().waitFor();
      assert.equal(await page.getByRole('button', { name: '建立 token' }).count(), 0);
      await page.locator('tbody tr[data-key]').first().getByRole('button', { name: /^更多動作/ }).click();
      assert.equal(await page.getByRole('menuitem', { name: '停用 token…' }).count(), 0);
      await page.keyboard.press('Escape');
      await page.goto(`${BASE}/enrollment/pending?view=closed`);
      await page.locator('tbody tr[data-key]').first().waitFor();
      assert.equal(await page.locator('main input[type="checkbox"]').count(), 0);
      await page.goto(`${BASE}/certificates`);
      await page.locator('tbody tr[data-key]').first().waitFor();
      assert.equal(await page.locator('main input[type="checkbox"]').count(), 0);
      await page.locator('tbody tr[data-key]').first().getByRole('button', { name: /^更多動作/ }).click();
      assert.equal(await page.getByRole('menuitem', { name: '撤銷憑證…' }).count(), 0);
      await page.keyboard.press('Escape');
      await page.goto(`${BASE}/certificates/ca`);
      await title(page, 'CA 與撤銷清單');
      await page.locator('.ds-card').first().waitFor();
      assert.equal(await page.getByRole('button', { name: '立即更新 CRL' }).count(), 0);
      await axe(page, 'S15（稽核員）');
      const body = { name: 'x', group: 'g', max_uses: 1, approval: 'manual' };
      assert.equal(await direct(page, 'POST', '/api/v1/enroll/tokens', body), 403);
      assert.equal(await direct(page, 'POST', '/api/v1/enroll/requests/approve', { request_ids: ['x'] }), 403);
      assert.equal(await direct(page, 'POST', '/api/v1/enroll/requests/reject', { request_ids: ['x'], expected_count: 1, reason: 'x' }), 403);
      assert.equal(await direct(page, 'POST', '/api/v1/pki/certificates/revoke', { serials: ['00'], expected_count: 1, reason: 'unspecified', comment: 'x' }), 403);
      assert.equal(await direct(page, 'POST', '/api/v1/pki/crl/regenerate', {}), 403);
      await ctx.close();
    }
    // 維運者：自動核准的選項停用並說明；建立自動核准直接呼叫 403
    {
      const { ctx, page } = await newPage();
      await login(page, 'operator', '/enrollment/tokens');
      await title(page, '註冊 token');
      await page.getByRole('button', { name: '建立 token' }).first().click();
      const auto = page.locator('dialog[open] input[name="tk-ap"][value="auto"]');
      assert.equal(await auto.isDisabled(), true);
      await page.locator('dialog[open]').getByText('只有管理員可以建立自動核准的 token。').waitFor();
      await axe(page, 'S12 建立 token（維運者）');
      await page.locator('dialog[open]').getByRole('button', { name: '取消' }).click();
      assert.equal(await direct(page, 'POST', '/api/v1/enroll/tokens', { name: 'x', group: 'g', max_uses: 1, approval: 'auto', allowed_cidrs: ['10.0.0.0/8'] }), 403);
      await page.goto(`${BASE}/certificates/ca`);
      await page.getByRole('button', { name: '立即更新 CRL' }).waitFor();
      await ctx.close();
    }
  });

  test('S12～S15 深色主題：axe 0 違規；窄螢幕沒有水平溢出', async () => {
    const { ctx, page } = await newPage({ colorScheme: 'dark' });
    await login(page, 'admin', '/enrollment/tokens');
    await title(page, '註冊 token');
    await page.locator('tbody tr[data-key]').first().waitFor();
    await axe(page, 'S12 註冊 token（深色）');
    await page.locator('tbody tr[data-key] a').first().click();
    await page.locator('.k-funnel').waitFor();
    await axe(page, 'S12 token 詳情（深色）');
    for (const [p, name, wait] of [
      ['/enrollment/pending', 'S13 待核准（深色）', '.ds-empty, tbody tr[data-key]'],
      ['/enrollment/pending?view=closed', 'S13 最近結束（深色）', 'tbody tr[data-key]'],
      ['/certificates', 'S14 憑證（深色）', 'tbody tr[data-key]'],
      ['/certificates?state=expired_beyond_grace', 'S14 需重新註冊（深色）', '.ds-empty'],
      ['/certificates?state=pending_issuance', 'S14 待簽發（深色）', '.ds-empty'],
      ['/certificates/ca', 'S15 CA 與撤銷清單（深色）', '.ds-card'],
    ]) {
      await page.goto(`${BASE}${p}`);
      await page.locator(wait).first().waitFor();
      await page.locator('[aria-busy="true"]').first().waitFor({ state: 'detached', timeout: 10000 }).catch(() => {});
      await axe(page, name);
    }
    await shot(page, 'pki-ca-dark');
    await ctx.close();
    const narrow = await newPage({ viewport: { width: 375, height: 800 } });
    await login(narrow.page, 'admin', '/certificates');
    await title(narrow.page, '憑證');
    await narrow.page.locator('tbody tr[data-key]').first().waitFor();
    for (const p of ['/certificates', '/enrollment/tokens', '/enrollment/pending?view=closed', '/certificates/ca']) {
      await narrow.page.goto(`${BASE}${p}`);
      await narrow.page.locator('#page-title').waitFor();
      await narrow.page.waitForTimeout(500);
      const over = await narrow.page.evaluate(() => {
        const w = document.documentElement.clientWidth;
        return Array.from(document.querySelectorAll('main *'))
          .filter((el) => el.getBoundingClientRect().right > w + 1 && !el.closest('.k-scroll-x, .ds-table-wrap, .ds-sidenav, .ds-filterbar__row'))
          .slice(0, 3)
          .map((el) => `${el.tagName}.${el.className}`);
      });
      assert.deepEqual(over, [], `${p} 水平溢出`);
    }
    await shot(narrow.page, 'pki-certificates-narrow');
    await narrow.ctx.close();
  });
}
