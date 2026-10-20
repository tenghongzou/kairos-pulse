// 指標轉出（商業審計 F7：Prometheus exporter、OTLP；kairos/EXPORT.md）在 S21「整合」的端對端測試；由 e2e.mjs 註冊。
//
// 涵蓋：管理員啟用 Prometheus exporter（L1 確認、來源變為主控台）→ 以工作階段抓取 /api/v1/export/prometheus
// （假 agent web-01 的 system_cpu_usage_percent）；設定 OTLP 端點與標頭（只顯示遮蔽值）→ 測試連線 → 假的 collector
// （本檔的 HTTP 伺服器）收到帶標頭的 protobuf；停用 OTLP；稽核員唯讀；每個狀態 axe 0 違規（含深色）。

import assert from 'node:assert/strict';
import { createServer } from 'node:http';

const WEB = '11111111-1111-4111-8111-000000000001';
const SECRET = 'Bearer e2e-otlp-7f3a91';

function fakeCollector() {
  const requests = [];
  const server = createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      requests.push({ url: req.url, headers: req.headers, body: Buffer.concat(chunks) });
      res.writeHead(200, { 'Content-Type': 'application/x-protobuf', 'Content-Length': 0 });
      res.end();
    });
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({ server, requests, port: server.address().port }));
  });
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

export function registerExport({ test, newPage, login, waitTitle, axe, shot, URL0, PW }) {
  test('F7 S21 整合：啟用 Prometheus exporter → 以工作階段抓取指標', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'admin', PW, '/system/settings/integrations');
    await waitTitle(page, '系統設定');
    const card = page.locator('section', { has: page.getByRole('heading', { name: '指標轉出', exact: true }) });
    await card.getByRole('heading', { name: /Prometheus exporter/ }).waitFor();
    // 兩者都停用：只有說明與兩個開關（設計包 S21「0.9 規格」），不顯示端點、統計與其他欄位
    await card.getByLabel('提供 Prometheus 端點').waitFor();
    await card.getByLabel('推送到 OpenTelemetry collector').waitFor();
    assert.equal(await card.getByText('GET /api/v1/export/prometheus').isVisible(), false, '兩者都停用時不顯示端點');
    assert.equal(await card.getByLabel('過期門檻（秒）').isVisible(), false, '兩者都停用時不顯示其他欄位');
    await axe(page, 'F7 系統設定：指標轉出（未啟用）');
    // 未啟用：404 export_disabled
    const disabled = await page.evaluate(async () => {
      const r = await fetch('/api/v1/export/prometheus');
      return { status: r.status, body: await r.json() };
    });
    assert.equal(disabled.status, 404);
    assert.equal(disabled.body.error.code, 'export_disabled');
    // 沒有變更時「儲存變更…」不可用
    const save = card.getByRole('button', { name: '儲存變更…' });
    assert.equal(await save.getAttribute('aria-disabled'), 'true');
    await card.getByLabel('提供 Prometheus 端點').check();
    await card.getByText('尚未儲存').waitFor();
    // 勾選開關後才顯示端點與其他欄位
    await card.getByText('GET /api/v1/export/prometheus').waitFor();
    // 不合法的過期門檻：欄位錯誤，不送出
    const stale = card.getByLabel('過期門檻（秒）');
    await stale.fill('5');
    await save.click();
    await card.getByText('請輸入 30～86400 的整數').waitFor();
    await stale.fill('120');
    await save.click();
    const dlg = page.getByRole('dialog');
    await dlg.getByText(/^export\.prometheus\.enabled：.+ → .+$/).waitFor();
    await dlg.getByText(/^export\.prometheus\.stale_after_secs：300 → 120$/).waitFor();
    await axe(page, 'F7 指標轉出：儲存確認');
    await dlg.getByRole('button', { name: '儲存', exact: true }).click();
    await page.getByText('已儲存指標轉出的設定').waitFor();
    await card.getByRole('heading', { name: /Prometheus exporter.*已啟用/ }).waitFor();
    await card.getByText(/主控台的值（user:admin/).first().waitFor();
    // 抓取：web-01（每 5 秒一筆 report）出現在輸出中
    const text = await until(
      () =>
        page.evaluate(async (id) => {
          const r = await fetch('/api/v1/export/prometheus');
          const t = await r.text();
          return r.status === 200 && t.includes(`system_cpu_usage_percent{agent_id="${id}"`) ? t : null;
        }, WEB),
      20000,
      'web-01 的指標',
    );
    assert.match(text, /# TYPE kairos_host_up gauge/);
    assert.match(text, new RegExp(`kairos_host_up\\{agent_id="${WEB}",hostname="web-01"[^}]*\\} 1`));
    assert.match(text, /system_network_received_bytes_total\{/);
    assert.doesNotMatch(text, /kairos_process_/, '行程排行預設不輸出');
    // OpenMetrics
    const om = await page.evaluate(async () => {
      const r = await fetch('/api/v1/export/prometheus', { headers: { Accept: 'application/openmetrics-text; version=1.0.0' } });
      return { type: r.headers.get('content-type'), text: await r.text() };
    });
    assert.match(om.type, /application\/openmetrics-text/);
    assert.ok(om.text.endsWith('# EOF\n'));
    // 狀態：最近一次抓取
    await page.reload();
    await waitTitle(page, '系統設定');
    await card.getByText(/台主機、\d+ 個序列/).waitFor();
    await axe(page, 'F7 指標轉出：已啟用');
    await card.getByRole('heading', { name: /Prometheus exporter/ }).scrollIntoViewIfNeeded();
    await shot(page, 'f7-export-prometheus');
    await ctx.close();
  });

  test('F7 S21 整合：OTLP 端點與標頭 → 測試連線 → collector 收到 protobuf → 停用', async () => {
    const col = await fakeCollector();
    const { ctx, page } = await newPage();
    try {
      await login(page, 'admin', PW, '/system/settings/integrations');
      await waitTitle(page, '系統設定');
      const card = page.locator('section', { has: page.getByRole('heading', { name: '指標轉出', exact: true }) });
      await card.getByRole('heading', { name: /OTLP 推送/ }).waitFor();
      await card.getByLabel('推送到 OpenTelemetry collector').check();
      // 測試連線在沒有已儲存的端點時不可用，按鈕旁寫出原因（可見文字）
      assert.ok(await card.getByRole('button', { name: '測試連線' }).isDisabled());
      await card.getByText('先儲存端點再測試').waitFor();
      const save = card.getByRole('button', { name: '儲存變更…' });
      // 啟用卻沒有端點：欄位錯誤
      await save.click();
      await card.getByText('啟用時必須填寫端點').waitFor();
      await card.getByLabel('OTLP/HTTP 端點').fill(`http://127.0.0.1:${col.port}/v1/metrics`);
      await card.getByLabel('推送間隔（秒）').fill('2');
      await card.getByLabel('更換標頭…').check();
      await card.getByLabel('標頭名稱 1').fill('Authorization');
      await card.getByLabel('標頭的值 1').fill(SECRET);
      await axe(page, 'F7 指標轉出：OTLP 編輯中');
      await save.click();
      const dlg = page.getByRole('dialog');
      await dlg.getByText('HTTP 標頭：更換為 Authorization').waitFor();
      assert.ok(!(await dlg.innerText()).includes(SECRET), '確認對話框不顯示標頭的值');
      await dlg.getByRole('button', { name: '儲存', exact: true }).click();
      await page.getByText('已儲存指標轉出的設定').waitFor();
      // 只顯示遮蔽值；畫面上沒有秘密
      await card.getByText('Authorization: ••••').waitFor();
      const html = await page.evaluate(() => document.documentElement.outerHTML + Array.from(document.querySelectorAll('input')).map((i) => i.value).join(' '));
      assert.ok(!html.includes('e2e-otlp-7f3a91'), '標頭的值不應留在畫面上');
      // 有未儲存的變更時：按鈕旁說明「測試的是已儲存的設定」；改回原值後消失
      const interval = card.getByLabel('推送間隔（秒）');
      await interval.fill('3');
      await card.getByText('測試的是已儲存的設定').waitFor();
      await interval.fill('2');
      await card.getByText('測試的是已儲存的設定').waitFor({ state: 'detached' });
      // 測試連線：結果就地顯示在按鈕下方
      await card.getByRole('button', { name: '測試連線' }).click();
      await card.getByText(/collector 已接受（HTTP 200，/).waitFor();
      // 推送：collector 收到帶標頭、非空的 protobuf（含 semconv 的名稱與 web-01 的 host.name）
      const req = await until(() => col.requests.find((r) => r.body.length > 0 && r.body.includes('web-01')), 20000, 'collector 收到 report');
      assert.equal(req.url, '/v1/metrics');
      assert.equal(req.headers['content-type'], 'application/x-protobuf');
      assert.equal(req.headers.authorization, SECRET);
      assert.ok(req.body.includes('system.cpu.utilization'));
      assert.ok(req.body.includes('host.name'));
      await page.reload();
      await waitTitle(page, '系統設定');
      await card.getByText(/正常|等待第一次推送/).first().waitFor();
      await axe(page, 'F7 指標轉出：OTLP 已啟用');
      await card.getByRole('heading', { name: /OTLP 推送/ }).scrollIntoViewIfNeeded();
      await shot(page, 'f7-export-otlp');
      // 停用（改用設定檔的值：內建預設為停用）
      await card.getByRole('button', { name: '改用設定檔的值：export.otlp.enabled' }).click();
      await dlg.getByText('export.otlp.enabled：改用設定檔的值').waitFor();
      await dlg.getByRole('button', { name: '儲存', exact: true }).click();
      await page.getByText('已儲存指標轉出的設定').waitFor();
      await card.getByRole('heading', { name: /OTLP 推送.*未啟用/ }).waitFor();
      // 深色主題
      await page.emulateMedia({ colorScheme: 'dark' });
      await axe(page, 'F7 指標轉出（深色）');
    } finally {
      col.server.close();
      await ctx.close();
    }
  });

  test('F7 S21 整合：稽核員唯讀（沒有儲存與測試按鈕）；唯讀角色抓取 Prometheus 端點', async () => {
    const { ctx, page } = await newPage();
    await login(page, 'auditor', PW, '/system/settings/integrations');
    await waitTitle(page, '系統設定');
    const card = page.locator('section', { has: page.getByRole('heading', { name: '指標轉出', exact: true }) });
    await card.getByText('你可以檢視設定與狀態；修改需要管理員（export:write）。').waitFor();
    assert.equal(await card.getByRole('button', { name: '儲存變更…' }).count(), 0);
    assert.equal(await card.getByRole('button', { name: '測試連線' }).count(), 0);
    assert.ok(await card.getByLabel('提供 Prometheus 端點').isDisabled());
    await axe(page, 'F7 指標轉出：稽核員');
    // 直接呼叫寫入 API：403 export:write
    const st = await page.evaluate(async () => (await fetch('/api/v1/export/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: '{}' })).status);
    assert.equal(st, 403);
    await ctx.close();
    // 唯讀（hosts:read）可以抓取
    const v = await newPage();
    await login(v.page, 'viewer', PW, `/hosts/${WEB}`);
    await waitTitle(v.page, 'web-01');
    const s = await v.page.evaluate(async () => (await fetch('/api/v1/export/prometheus')).status);
    assert.equal(s, 200);
    await v.ctx.close();
  });
}
