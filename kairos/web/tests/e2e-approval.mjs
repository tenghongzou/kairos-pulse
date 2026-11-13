// 升級推送的雙人覆核、版本庫檔案的完整性、政策的可見性（ADR-0002 切片 S1～S3）在主控台的端對端測試；由 e2e.mjs 註冊。
//
// 對第三個 Kairos 執行（approval-env.sh）：`[remote] rollout_approval = "required"`、三位管理員（alice、bob、carol）、
// 維運者 oscar、稽核員 audrey、唯讀 vera；假 agent appr-01（Pulse 0.9.0）與 appr-02（0.9.5，降版用）。與主要的
// Kairos 一樣沒有 mTLS，資格只依 hello。除了註明「攔截」的地方，都是真的 API 與真的狀態轉移。
//
// - E1 雙人覆核：送出（對話框寫明誰可以核准）→ 建立者的詳情頁（不能核准、送審摘要、digest、覆核紀錄、匯出 CSV、
//   泡泡不計）→ 另一位管理員的泡泡（可存取名稱寫出組成）與「待核准」檢視 → 直接打 API 不帶確認字串或 digest 被拒
//   （這條線要補的洞）→ L2 對話框內嵌送審摘要、字串不符時按鈕停用、送出的請求帶 confirm 與 digest → 開始。
//   三人邊界（甲建立、乙送出：甲、乙都不能核准，丙可以）；摘要過期（409 submission_changed）時重新載入；
//   退回與原因；降版的 L3 字串「核准降版 N 台」與原因必填。
// - E12 XSS：推送名稱與退回原因含 HTML 時在清單、詳情、送審摘要、覆核紀錄都以文字顯示。
// - R2～R4：維運者與稽核員的「待核准」檢視與詳情唯讀、泡泡不計待核准；唯讀角色不查詢推送；S21 的「遠端管理」
//   卡片對每個角色都沒有可編輯的控制項。
// - E2 沒有可核准者（攔截 API）：建立表單的預覽與送出對話框、待核准的詳情頁與檢視、S20。
// - E3 檔案遺失（真的把版本庫的檔案移走）：S25 頁首的 KRS-W8417 三段式與「重新上傳…」、資格預覽的
//   artifact_missing、核准時 409 的就地說明、S20；補檔後核准；核准後檔案再遺失時詳情頁與清單的「等待檔案」。
// - E4 政策：S21「遠端管理」卡片（需要、來源、上一次改變）、S26 清單頁首；把設定改為 none 重新啟動 Kairos →
//   稽核 settings.rollout_approval.change、告警事件「雙人覆核已關閉」、S20／S21／S26 的顯示；既有的待核准推送
//   仍然要核准。
// - 每個新畫面與對話框 axe 0 違規（淺色與深色）。

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawn } from 'node:child_process';
import { existsSync, openSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import https from 'node:https';
import { join } from 'node:path';
import { testRelease, api, until } from './e2e-remote.mjs';

const BASE = process.env.KAIROS_APPROVAL_URL;
const A1 = '22222222-2222-4222-8222-000000000001';
const A2 = '22222222-2222-4222-8222-000000000002';
const VERSION = '0.9.1';
const XSS = '<img src=x onerror="window.__xss=7">';

function healthz() {
  return new Promise((resolve) => {
    const rq = https.get(`${BASE.replace('localhost', '127.0.0.1')}/healthz`, { rejectUnauthorized: false }, (res) => {
      res.resume();
      resolve(res.statusCode);
    });
    rq.on('error', () => resolve(0));
    rq.setTimeout(2000, () => {
      rq.destroy();
      resolve(0);
    });
  });
}

/** 把 rollout_approval 改為 value 後重新啟動第三個 Kairos（設定只能在設定檔修改、需要重新啟動） */
async function restartKairos(value, from = 'required') {
  const pidFile = process.env.APPROVAL_PID_FILE;
  const cfgFile = process.env.APPROVAL_CFG;
  process.kill(Number(readFileSync(pidFile, 'utf8').trim()), 'SIGTERM');
  await until(async () => (await healthz()) === 0, 30000, 'Kairos 停止');
  const cfg = readFileSync(cfgFile, 'utf8');
  assert.ok(cfg.includes(`rollout_approval = "${from}"`), `設定檔目前應該是 ${from}`);
  writeFileSync(cfgFile, cfg.replace(`rollout_approval = "${from}"`, `rollout_approval = "${value}"`));
  const out = openSync(process.env.APPROVAL_LOG, 'a');
  const child = spawn(process.env.KAIROS_BIN, ['--config', cfgFile], { detached: true, stdio: ['ignore', out, out] });
  child.unref();
  writeFileSync(pidFile, String(child.pid));
  await until(async () => (await healthz()) === 200, 30000, 'Kairos 重新啟動');
}

/** 以工作階段上傳發佈檔（PUT application/octet-stream） */
async function putFile(page, path, buffer) {
  return page.evaluate(
    async ([p, b64]) => {
      const me = (await (await fetch('/api/v1/me')).json()).data;
      const bin = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
      const r = await fetch(p, { method: 'PUT', headers: { 'Content-Type': 'application/octet-stream', 'X-CSRF-Token': me.csrf_token }, body: bin });
      return r.status;
    },
    [path, buffer.toString('base64')],
  );
}

export function registerApproval({ test, newPage, waitTitle, axe, shot, FakeAgent, PW }) {
  const rel = testRelease();
  const tarFile = rel.good.find((f) => f.name === rel.tar);
  // 版本庫的檔案以內容的 SHA-256 命名（<data_dir>/releases/<sha256>）
  const stored = join(process.env.APPROVAL_DIR || '', 'data', 'releases', createHash('sha256').update(tarFile.buffer).digest('hex'));
  let a1 = null;
  let a2 = null;
  let heldId = null; // 留給「檔案遺失」測試的待核准推送（alice 建立並送出、對象 appr-01）

  async function loginAt(page, user, next = '/') {
    await page.goto(`${BASE}/login?next=${encodeURIComponent(next)}`);
    await page.getByLabel('帳號').fill(user);
    await page.locator('input[name="password"]').fill(PW);
    await page.getByRole('button', { name: '登入', exact: true }).click();
  }

  async function session(user, next = '/account', title = '個人設定') {
    const s = await newPage();
    await loginAt(s.page, user, next);
    await waitTitle(s.page, title);
    return s;
  }

  const spec = (name, agent, extra = {}) => ({ name, target_version: VERSION, selector: { agent_ids: [agent] }, waves: [{ size: 'rest' }], ...extra });

  /** 以 API 建立並送出（確認字串取自草稿的預覽）；回傳推送的 id */
  async function createAndSubmit(creator, submitter, body, reason) {
    const c = await api(creator, 'POST', '/api/v1/rollouts', body);
    assert.equal(c.status, 201, JSON.stringify(c.json));
    const id = c.json.data.id;
    const d = await api(submitter, 'GET', `/api/v1/rollouts/${id}`);
    const s = await api(submitter, 'POST', `/api/v1/rollouts/${id}/submit`, { confirm: d.json.data.preview.confirm_text, reason });
    assert.equal(s.status, 200, JSON.stringify(s.json));
    assert.equal(s.json.data.state, 'pending_approval');
    return id;
  }

  const stateOf = async (page, id) => (await api(page, 'GET', `/api/v1/rollouts/${id}`)).json.data;
  /** 測試結束時釋放主機：取消推送。核准後的推送可能已經結束（沒有 mTLS 的測試環境不協商 upgrade，連線中的主機在
   *  第一輪就被標為尚未就緒，推送隨即完成）——已結束的不必取消 */
  const cancel = async (page, id) => {
    const r = await api(page, 'POST', `/api/v1/rollouts/${id}/cancel`, { reason: 'E2E：測試結束' });
    if (r.status === 409 && r.json.error.code === 'invalid_state' && ['completed', 'cancelled'].includes(r.json.error.details.state)) return;
    assert.equal(r.status, 200, JSON.stringify(r.json));
  };
  const bubble = (page) => page.locator('.ds-sidenav__link', { hasText: '升級推送' }).locator('.ds-sidenav__count');
  const dark = async (page, label) => {
    await page.emulateMedia({ colorScheme: 'dark' });
    await axe(page, `${label}（深色）`);
    await page.emulateMedia({ colorScheme: 'light' });
  };

  /** 核准的 L2／L3 對話框：輸入確認字串（與原因）後送出；回傳對話框 */
  async function openApprove(page) {
    await page.getByRole('button', { name: '核准…' }).click();
    const dlg = page.getByRole('dialog');
    await dlg.locator('#approve-summary').waitFor();
    return dlg;
  }

  test('雙人覆核 E1：送出 → 建立者不能核准 → 另一位管理員的泡泡與待核准檢視 → 直接打 API 被拒 → L2 核准', async () => {
    assert.ok(BASE, '需要 KAIROS_APPROVAL_URL（approval-env.sh）');
    const agent = (agentId, hostname, agentVersion) => new FakeAgent({ port: Number(process.env.APPROVAL_AGENT_PORT), agentId, hostname, tags: ['env:lab'], backfillMinutes: 8, agentVersion, install: { method: 'tar', upgrade: 'ready', config: 'enabled' } });
    a1 = agent(A1, 'appr-01', '0.9.0');
    a2 = agent(A2, 'appr-02', '0.9.5');
    await a1.connect();
    await a2.connect();
    const al = await session('alice');
    for (const id of [A1, A2]) {
      await until(async () => {
        const r = await api(al.page, 'GET', `/api/v1/hosts/${id}/upgrade`);
        return r.status === 200 && r.json.data.install && r.json.data.install.upgrade === 'ready';
      }, 30000, `${id} 的升級資格`);
    }
    // 匯入版本並上傳 tar（以 API；匯入的畫面在 e2e-remote.mjs 測試）
    const text = (name) => rel.good.find((f) => f.name === name).buffer.toString('utf8');
    const manifest = rel.good.find((f) => /-release\.json$/.test(f.name));
    const imp = await api(al.page, 'POST', '/api/v1/releases', { sha256sums: text('SHA256SUMS'), signature: text('SHA256SUMS.sig'), manifest: manifest.buffer.toString('utf8') });
    assert.ok(imp.status === 200 || imp.status === 201, JSON.stringify(imp.json));
    const put = await putFile(al.page, `/api/v1/releases/${VERSION}/artifacts/${encodeURIComponent(rel.tar)}`, tarFile.buffer);
    assert.ok(put === 200 || put === 201, `上傳發佈檔：${put}`);
    assert.ok(existsSync(stored), `版本庫的檔案：${stored}`);

    // 建立並送出：對話框寫明要由誰核准（三人邊界）；有另外兩位管理員，沒有「沒有可核准者」的警告
    await al.page.goto(`${BASE}/pulse/rollouts/new`);
    await waitTitle(al.page, '建立推送');
    await al.page.getByLabel(/^名稱/).fill('E2E 覆核');
    const preview = al.page.locator('#rollout-preview');
    await preview.getByText('可升級 1 台').waitFor({ timeout: 15000 });
    await preview.locator('li[data-reason="downgrade_not_allowed"]').waitFor();
    assert.equal(await al.page.locator('#rollout-preview-no-approvers').count(), 0);
    await al.page.getByRole('button', { name: '送出推送…' }).click();
    const sd = al.page.getByRole('dialog');
    await sd.getByText('這台 Kairos 啟用了雙人覆核').waitFor();
    await sd.locator('#submit-who-approves').getByText(/建立推送的人與送出推送的人都不能核准/).waitFor();
    assert.equal(await sd.locator('#submit-no-approvers').count(), 0);
    await axe(al.page, '雙人覆核 送出確認（需要核准）');
    await dark(al.page, '雙人覆核 送出確認（需要核准）');
    await sd.getByLabel(/輸入「升級 1 台」以確認/).fill('升級 1 台');
    await sd.getByRole('button', { name: '升級 1 台', exact: true }).click();
    await al.page.getByText('已送出推送「E2E 覆核」，等待核准').waitFor();
    await waitTitle(al.page, 'E2E 覆核');
    const id = new URL(al.page.url()).pathname.split('/').pop();
    assert.match(id, /^r-\d{8}-\d{4}$/);

    // 建立並送出的人：不能核准（沒有「核准…」「退回…」），看到誰可以核准、送審摘要、digest 與覆核紀錄
    const pending = al.page.locator('.ds-alert', { hasText: '待核准' });
    await pending.getByText(/已在 .* 送出（已等待 .*），等待另一位管理員核准/).waitFor();
    await pending.getByText('你建立或送出了這個推送，不能自己核准（KRS-E6106）。').waitFor();
    await pending.getByText(/需要由 user:alice（建立並送出的人）以外、有核准權限的管理員（以帳號登入）核准/).waitFor();
    assert.equal(await al.page.getByRole('button', { name: /^(核准…|退回…)$/ }).count(), 0, '建立並送出的人沒有核准、退回');
    assert.equal(await al.page.getByRole('button', { name: '取消推送…' }).count(), 1);
    assert.equal(await al.page.getByRole('button', { name: '複製連結' }).count(), 1);
    const subCard = al.page.locator('section[aria-labelledby="rollout-submission"]');
    await subCard.getByText('可升級 1 台').waitFor();
    await subCard.getByText('共 2 批：1 → 0 台').waitFor();
    const digest = (await al.page.locator('#rollout-digest').textContent()).trim();
    assert.match(digest, /^[0-9a-f]{64}$/);
    await al.page.locator('section[aria-labelledby="rollout-review"]').getByText(/user:alice · /).first().waitFor();
    await axe(al.page, '雙人覆核 待核准的詳情（送出者）');
    await dark(al.page, '雙人覆核 待核准的詳情（送出者）');
    await shot(al.page, 'approval-detail-owner');
    // 完整的主機清單（CSV）
    const [download] = await Promise.all([al.page.waitForEvent('download'), al.page.getByRole('button', { name: '匯出完整清單（CSV）' }).click()]);
    const csv = readFileSync(await download.path(), 'utf8');
    assert.match(csv, /agent_id,hostname,wave,state,reason,from_version/);
    assert.ok(csv.includes(A1), 'CSV 有已固定的主機');
    assert.equal(download.suggestedFilename(), `${id}-hosts.csv`);
    // 泡泡不計自己不能核准的；直接打 API 也不能核准自己的推送（403 self_approval，KRS-E6106）
    const mine = await api(al.page, 'GET', '/api/v1/rollouts');
    assert.deepEqual([mine.json.meta.pending_approval, mine.json.meta.pending_approval_actionable], [1, 0]);
    await al.page.waitForTimeout(1500);
    assert.equal(await bubble(al.page).count(), 0, '送出者的泡泡不計待核准');
    const self = await api(al.page, 'POST', `/api/v1/rollouts/${id}/approve`, { confirm: '核准 1 台', digest });
    assert.deepEqual([self.status, self.json.error.code, self.json.error.product_code], [403, 'self_approval', 'KRS-E6106']);

    // 另一位管理員：泡泡 1（可存取名稱寫出組成）、「待核准」檢視
    const bo = await session('bob', '/pulse/rollouts', '升級推送');
    await bubble(bo.page).waitFor({ timeout: 15000 });
    assert.equal((await bubble(bo.page).textContent()).trim(), '1');
    assert.match(await bo.page.locator('.ds-sidenav__link', { hasText: '升級推送' }).textContent(), /1 項需要處理：1 個等你核准/);
    const views = bo.page.getByRole('navigation', { name: '推送清單的檢視' });
    await views.getByRole('link', { name: '待核准（1，1 個等你核准）' }).click();
    const row = bo.page.locator(`#rollouts-pending tr[data-rollout="${id}"]`);
    await row.waitFor();
    assert.equal(new URL(bo.page.url()).searchParams.get('view'), 'pending_approval');
    assert.equal(await views.getByRole('link', { name: /^待核准/ }).getAttribute('aria-current'), 'page');
    const cells = await row.locator('td').allTextContents();
    assert.match(cells[2], /1 台/);
    assert.match(cells[3], /user:alice/);
    assert.match(cells[6], /^可以/);
    await axe(bo.page, '雙人覆核 待核准檢視');
    await dark(bo.page, '雙人覆核 待核准檢視');
    await shot(bo.page, 'approval-pending-view');

    // 這條線要補的洞：直接呼叫 API 核准——沒有伺服器算出的確認字串與 digest 一律被拒，狀態不變
    for (const [body, status, code] of [
      [{}, 400, 'confirmation_required'],
      [{ confirm: '核准 2 台', digest }, 400, 'confirmation_required'],
      [{ confirm: '核准 1 台' }, 400, 'invalid_parameter'],
      [{ confirm: '核准 1 台', digest: '0'.repeat(64) }, 409, 'submission_changed'],
    ]) {
      const r = await api(bo.page, 'POST', `/api/v1/rollouts/${id}/approve`, body);
      assert.deepEqual([r.status, r.json.error.code], [status, code], JSON.stringify(body));
    }
    assert.equal((await stateOf(bo.page, id)).state, 'pending_approval');

    // 詳情：可以核准的人看到「核准…」「退回…」；L2 對話框內嵌送審摘要，字串不符時按鈕停用
    await row.getByRole('link', { name: 'E2E 覆核' }).click();
    await waitTitle(bo.page, 'E2E 覆核');
    await bo.page.locator('.ds-alert', { hasText: '待核准' }).getByText(/user:alice 於 .* 送出（已等待 .*），等待核准/).waitFor();
    assert.equal(await bo.page.getByRole('button', { name: '退回…' }).count(), 1);
    await axe(bo.page, '雙人覆核 待核准的詳情（核准者）');
    const dlg = await openApprove(bo.page);
    await dlg.getByText('核准推送「E2E 覆核」？').waitFor();
    await dlg.locator('#approve-summary').getByText('可升級 1 台').waitFor();
    await dlg.locator('#approve-summary').getByText('核准後會立即開始升級。').waitFor();
    const ok = dlg.getByRole('button', { name: '核准 1 台', exact: true });
    assert.ok(await ok.isDisabled(), '輸入確認字串前不能核准');
    const typed = dlg.getByLabel(/輸入「核准 1 台」以確認/);
    await typed.fill('核准 2 台');
    assert.ok(await ok.isDisabled(), '確認字串不符時不能核准');
    await axe(bo.page, '雙人覆核 核准對話框（L2）');
    await dark(bo.page, '雙人覆核 核准對話框（L2）');
    await shot(bo.page, 'approval-dialog');
    await typed.fill('核准 1 台');
    const [rq, rs] = await Promise.all([
      bo.page.waitForRequest((r) => r.method() === 'POST' && r.url().endsWith(`/rollouts/${id}/approve`)),
      bo.page.waitForResponse((r) => r.request().method() === 'POST' && r.url().endsWith(`/rollouts/${id}/approve`)),
      ok.click(),
    ]);
    assert.deepEqual(rq.postDataJSON(), { confirm: '核准 1 台', digest }, '核准的請求帶伺服器算出的確認字串與 digest');
    const approved = (await rs.json()).data;
    assert.deepEqual([rs.status(), approved.state, approved.approved_by], [200, 'running', 'user:bob']);
    await bo.page.getByText('已核准推送').first().waitFor();
    assert.equal((await stateOf(bo.page, id)).submission, undefined, '核准之後不再有 submission');
    await bo.page.locator('section[aria-labelledby="rollout-review"]').getByText(/user:bob · /).waitFor();
    await cancel(bo.page, id);
    await al.ctx.close();
    await bo.ctx.close();
  });

  test('雙人覆核 E1／E12：三人邊界（甲建立、乙送出）→ 摘要過期時重新載入 → 退回與原因；名稱與原因的 HTML 以文字顯示', async () => {
    const al = await session('alice');
    const bo = await session('bob');
    const name = `E2E 邊界 ${XSS}`;
    const id = await createAndSubmit(al.page, bo.page, spec(name, A1));
    // 甲（建立者）、乙（送出者）都不能核准；畫面寫出需要兩人以外的管理員
    for (const s of [al, bo]) {
      await s.page.goto(`${BASE}/pulse/rollouts/${id}`);
      await waitTitle(s.page, 'E2E 邊界');
      const box = s.page.locator('.ds-alert', { hasText: '待核准' });
      await box.getByText('你建立或送出了這個推送，不能自己核准（KRS-E6106）。').waitFor();
      await box.getByText(/需要由 user:alice（建立者）與 user:bob（送出者）以外、有核准權限的管理員（以帳號登入）核准/).waitFor();
      assert.equal(await s.page.getByRole('button', { name: /^(核准…|退回…)$/ }).count(), 0);
      const sub = (await stateOf(s.page, id)).submission;
      const r = await api(s.page, 'POST', `/api/v1/rollouts/${id}/approve`, { confirm: sub.confirm_text, digest: sub.digest });
      assert.deepEqual([r.status, r.json.error.code], [403, 'self_approval']);
      assert.equal(sub.approvers_available, 1, '只剩第三位管理員可以核准');
    }
    assert.equal(await al.page.locator('#page-title').textContent(), name);
    await axe(al.page, '雙人覆核 三人邊界（建立者）');

    // 丙：可以核准。開啟對話框之後推送被修改並重新送出 → 409 submission_changed：關閉對話框、重新載入摘要
    const ca = await session('carol', `/pulse/rollouts/${id}`, 'E2E 邊界');
    const digest1 = (await ca.page.locator('#rollout-digest').textContent()).trim();
    const dlg = await openApprove(ca.page);
    const name2 = `E2E 邊界二 ${XSS}`;
    const put = await api(al.page, 'PUT', `/api/v1/rollouts/${id}`, spec(name2, A1));
    assert.equal(put.status, 200, JSON.stringify(put.json));
    assert.equal(put.json.data.state, 'draft', '修改待核准的推送會退回草稿');
    const again = await api(bo.page, 'POST', `/api/v1/rollouts/${id}/submit`, { confirm: '升級 1 台' });
    assert.equal(again.status, 200, JSON.stringify(again.json));
    await dlg.getByLabel(/輸入「核准 1 台」以確認/).fill('核准 1 台');
    const [resp] = await Promise.all([ca.page.waitForResponse((r) => r.request().method() === 'POST' && r.url().endsWith(`/rollouts/${id}/approve`)), dlg.getByRole('button', { name: '核准 1 台', exact: true }).click()]);
    assert.equal(resp.status(), 409);
    assert.equal((await resp.json()).error.code, 'submission_changed');
    await ca.page.getByText('這個推送在你檢視之後被修改或重新送出').first().waitFor();
    await dlg.waitFor({ state: 'detached' });
    await until(async () => {
      const d = (await ca.page.locator('#rollout-digest').textContent()).trim();
      return d !== digest1 && /^[0-9a-f]{64}$/.test(d);
    }, 15000, '重新載入的 digest');
    assert.equal((await stateOf(ca.page, id)).state, 'pending_approval');

    // 退回：原因必填；建立者在草稿看到原因
    const reason = `E2E：請改批次 ${XSS}`;
    await ca.page.getByRole('button', { name: '退回…' }).click();
    const rd = ca.page.getByRole('dialog');
    await rd.getByRole('button', { name: '退回推送' }).click();
    await rd.getByText('請填寫原因').waitFor();
    await axe(ca.page, '雙人覆核 退回對話框');
    await rd.getByLabel(/原因/).fill(reason);
    await rd.getByRole('button', { name: '退回推送' }).click();
    await ca.page.getByText('已退回推送').first().waitFor();
    await until(async () => (await ca.page.getByRole('button', { name: /^(核准…|退回…)$/ }).count()) === 0, 15000, '退回後沒有核准、退回');
    assert.equal((await stateOf(ca.page, id)).state, 'draft');
    await al.page.goto(`${BASE}/pulse/rollouts/${id}`);
    await waitTitle(al.page, 'E2E 邊界二');
    await al.page.locator('.ds-alert', { hasText: '已退回' }).getByText(reason).waitFor();
    await al.page.locator('section[aria-labelledby="rollout-review"]').getByText(reason).waitFor();
    assert.equal(await al.page.locator('#page-title').textContent(), name2);
    await axe(al.page, '雙人覆核 被退回的草稿');
    // 清單與搜尋不到注入：名稱是文字
    await al.page.goto(`${BASE}/pulse/rollouts`);
    await waitTitle(al.page, '升級推送');
    assert.equal(await al.page.locator(`tr[data-rollout="${id}"] a`).first().textContent(), name2);
    for (const p of [al.page, ca.page]) {
      const flags = await p.evaluate(() => ({ xss: window.__xss, imgs: document.querySelectorAll('img[src="x"]').length }));
      assert.deepEqual(flags, { xss: undefined, imgs: 0 });
    }
    await cancel(al.page, id);
    await al.ctx.close();
    await bo.ctx.close();
    await ca.ctx.close();
  });

  test('雙人覆核 E1：降版的核准是 L3 字串「核准降版 N 台」，原因必填', async () => {
    const al = await session('alice');
    const id = await createAndSubmit(al.page, al.page, spec('E2E 降版', A2, { options: { allow_downgrade: true } }), 'E2E：回到 0.9.1');
    const sub = (await stateOf(al.page, id)).submission;
    assert.deepEqual([sub.confirm_text, sub.confirm_level, sub.downgrade], ['核准降版 1 台', 'L3', 1]);
    assert.ok(sub.warnings.includes('downgrade'));
    const ca = await session('carol', `/pulse/rollouts/${id}`, 'E2E 降版');
    // 用非降版的字串直接打 API 被拒；降版而沒有原因也被拒
    const wrong = await api(ca.page, 'POST', `/api/v1/rollouts/${id}/approve`, { confirm: '核准 1 台', digest: sub.digest });
    assert.deepEqual([wrong.status, wrong.json.error.code, wrong.json.error.details.confirm_text], [400, 'confirmation_required', '核准降版 1 台']);
    const noReason = await api(ca.page, 'POST', `/api/v1/rollouts/${id}/approve`, { confirm: '核准降版 1 台', digest: sub.digest });
    assert.deepEqual([noReason.status, noReason.json.error.code], [400, 'confirmation_required']);
    const dlg = await openApprove(ca.page);
    await dlg.locator('#approve-summary').getByText('其中降版 1 台').waitFor();
    await dlg.locator('#approve-summary li[data-warning="downgrade"]').waitFor();
    await dlg.getByLabel(/輸入「核准降版 1 台」以確認/).fill('核准降版 1 台');
    const ok = dlg.getByRole('button', { name: '核准降版 1 台', exact: true });
    await ok.click();
    await dlg.getByText('請填寫原因').waitFor();
    await axe(ca.page, '雙人覆核 核准對話框（降版，L3）');
    await dark(ca.page, '雙人覆核 核准對話框（降版，L3）');
    await dlg.getByLabel(/原因/).fill('E2E：已確認現場允許降版');
    const [rq] = await Promise.all([ca.page.waitForRequest((r) => r.method() === 'POST' && r.url().endsWith(`/rollouts/${id}/approve`)), ok.click()]);
    assert.deepEqual(rq.postDataJSON(), { confirm: '核准降版 1 台', digest: sub.digest, reason: 'E2E：已確認現場允許降版' });
    await ca.page.getByText('已核准推送').first().waitFor();
    assert.equal((await stateOf(ca.page, id)).approved_by, 'user:carol');
    await cancel(al.page, id);
    await al.ctx.close();
    await ca.ctx.close();
  });

  test('雙人覆核 E2／R2～R3：維運者與稽核員唯讀、泡泡不計；唯讀不查詢推送；沒有可核准者的警告（攔截）', async () => {
    const al = await session('alice');
    heldId = await createAndSubmit(al.page, al.page, spec('E2E 檔案', A1));
    // 維運者、稽核員：看得到「待核准」檢視與送審摘要，沒有核准、退回、取消；泡泡不計待核准
    for (const user of ['oscar', 'audrey']) {
      const s = await session(user, '/pulse/rollouts?view=pending_approval', '升級推送');
      const row = s.page.locator(`#rollouts-pending tr[data-rollout="${heldId}"]`);
      await row.getByText('不行（沒有核准權限）').waitFor();
      await s.page.getByRole('navigation', { name: '推送清單的檢視' }).getByRole('link', { name: '待核准（1）', exact: true }).waitFor();
      const meta = (await api(s.page, 'GET', '/api/v1/rollouts')).json.meta;
      assert.deepEqual([meta.pending_approval, meta.pending_approval_actionable], [1, 0], user);
      await s.page.waitForTimeout(1500);
      assert.equal(await bubble(s.page).count(), 0, `${user} 的泡泡不計待核准`);
      assert.equal(await s.page.getByRole('link', { name: '建立推送' }).count(), 0);
      await axe(s.page, `雙人覆核 待核准檢視（${user}）`);
      await row.getByRole('link', { name: 'E2E 檔案' }).click();
      await waitTitle(s.page, 'E2E 檔案');
      await s.page.locator('section[aria-labelledby="rollout-submission"]').getByText('可升級 1 台').waitFor();
      assert.equal(await s.page.getByRole('button', { name: /^(核准…|退回…|取消推送…|繼續…)$/ }).count(), 0, `${user} 沒有寫入按鈕`);
      const r = await api(s.page, 'POST', `/api/v1/rollouts/${heldId}/approve`, { confirm: '核准 1 台', digest: 'x' });
      assert.deepEqual([r.status, r.json.error.code], [403, 'forbidden'], user);
      await axe(s.page, `雙人覆核 待核准的詳情（${user}）`);
      await s.ctx.close();
    }
    // 唯讀（沒有 rollouts:read）：看不到「升級推送」，也不送出查詢
    const ve = await newPage();
    const asked = [];
    ve.page.on('request', (rq) => {
      if (new URL(rq.url()).pathname.startsWith('/api/v1/rollouts')) asked.push(rq.url());
    });
    await loginAt(ve.page, 'vera', '/system/health');
    await waitTitle(ve.page, '系統健康');
    await ve.page.locator('[data-item="database"]').waitFor();
    await ve.page.waitForTimeout(1500);
    assert.equal(await ve.page.locator('.ds-sidenav__link', { hasText: '升級推送' }).count(), 0);
    assert.equal(await ve.page.locator('[data-item="remote"]').count(), 0);
    assert.deepEqual(asked, []);
    await ve.ctx.close();

    // 沒有可核准者（攔截 API：approvers_available = 0）：建立表單的預覽與送出對話框
    const noOne = (sub) => ({ ...sub, approvers_available: 0, warnings: [...(sub.warnings || []), 'no_approvers'] });
    await al.page.route('**/api/v1/rollouts/preview', async (route) => {
      const r = await route.fetch();
      const body = await r.json();
      if (body.data) Object.assign(body.data, { approvers_available: 0, warnings: [...(body.data.warnings || []), 'no_approvers'] });
      return route.fulfill({ response: r, json: body });
    });
    await al.page.goto(`${BASE}/pulse/rollouts/new`);
    await waitTitle(al.page, '建立推送');
    await al.page.getByLabel(/^名稱/).fill('E2E 沒有人');
    // appr-01 已在待核准的推送中：對象指定 appr-02（降版）
    await al.page.getByLabel('指定群組、標籤或主機（聯集）').check();
    await al.page.locator('textarea[name="agent_ids"]').fill(A2);
    await al.page.locator('input[name="allow_downgrade"]').check();
    const preview = al.page.locator('#rollout-preview');
    await preview.getByText('可升級 1 台').waitFor({ timeout: 15000 });
    await al.page.locator('#rollout-preview-no-approvers').getByText('目前沒有其他可以核准的管理員').waitFor();
    await axe(al.page, '雙人覆核 建立推送（沒有可核准者）');
    await al.page.getByRole('button', { name: '送出推送…' }).click();
    const sd = al.page.getByRole('dialog');
    await sd.locator('#submit-no-approvers').getByText(/送出後會一直停在待核准/).waitFor();
    await axe(al.page, '雙人覆核 送出確認（沒有可核准者）');
    await sd.getByRole('button', { name: '取消' }).click();
    await sd.waitFor({ state: 'detached' });
    await al.ctx.close();

    // 待核准的詳情頁、「待核准」檢視與 S20（攔截）
    const ca = await newPage();
    await ca.page.route(`**/api/v1/rollouts/${heldId}`, async (route) => {
      const r = await route.fetch();
      const body = await r.json();
      if (body.data && body.data.submission) body.data.submission = noOne(body.data.submission);
      return route.fulfill({ response: r, json: body });
    });
    await ca.page.route('**/api/v1/rollouts?*', async (route) => {
      const r = await route.fetch();
      const body = await r.json();
      for (const x of body.data || []) if (x.submission) x.submission = noOne(x.submission);
      return route.fulfill({ response: r, json: body });
    });
    await ca.page.route('**/api/v1/system/health', async (route) => {
      const r = await route.fetch();
      const body = await r.json();
      body.data.items = (body.data.items || []).map((i) =>
        i.id === 'remote' ? { ...i, status: 'warning', product_code: null, summary: '1 個待核准的推送目前沒有可以核准的管理員（建立者與送出者不能核准）', values: { ...i.values, allowed_signers_configured: true, pending_approval: 1, pending_approval_no_approvers: 1 } } : i,
      );
      return route.fulfill({ response: r, json: body });
    });
    await loginAt(ca.page, 'carol', `/pulse/rollouts/${heldId}`);
    await waitTitle(ca.page, 'E2E 檔案');
    await ca.page.locator('.ds-alert', { hasText: '目前沒有可以核准這個推送的管理員' }).waitFor();
    await ca.page.locator('section[aria-labelledby="rollout-submission"] li[data-warning="no_approvers"]').waitFor();
    await axe(ca.page, '雙人覆核 待核准的詳情（沒有可核准者）');
    await ca.page.goto(`${BASE}/pulse/rollouts?view=pending_approval`);
    await waitTitle(ca.page, '升級推送');
    await ca.page.locator(`#rollouts-pending tr[data-rollout="${heldId}"]`).getByText('目前沒有可以核准的管理員').waitFor();
    await axe(ca.page, '雙人覆核 待核准檢視（沒有可核准者）');
    await ca.page.goto(`${BASE}/system/health`);
    await waitTitle(ca.page, '系統健康');
    const remote = ca.page.locator('[data-item="remote"]');
    await remote.getByText(/1 個待核准的推送目前沒有可以核准的管理員/).waitFor();
    assert.equal(await remote.getByRole('link', { name: '查看待核准的推送' }).getAttribute('href'), '/pulse/rollouts?view=pending_approval');
    await axe(ca.page, '雙人覆核 S20 沒有可核准者');
    await ca.ctx.close();
  });

  test('檔案完整性 E3：版本庫的檔案遺失 → S25 三段式與重新上傳、資格預覽、核准 409、S20 → 補檔後核准 → 等待檔案', async () => {
    assert.ok(heldId, '需要前一個測試留下的待核准推送');
    const al = await session('alice');
    const ca = await session('carol');
    // 送出之後、核准之前檔案不見了（例如還原備份、叢集換手）
    renameSync(stored, `${stored}.bak`);
    try {
      // S25：頁首的三段式（KRS-W8417）與「重新上傳…」
      await al.page.goto(`${BASE}/pulse/releases`);
      await waitTitle(al.page, 'Pulse 版本庫');
      const head = al.page.locator('#release-files-missing');
      await head.getByText(`Pulse 版本庫的檔案遺失：${VERSION} 的 ${rel.tar}`).waitFor();
      await head.getByText('KRS-W8417').waitFor();
      await head.getByText('原因', { exact: true }).waitFor();
      await head.getByText('處理方式', { exact: true }).waitFor();
      await head.locator(`li[data-file="${rel.tar}"]`).getByText(/檔案遺失/).waitFor();
      await axe(al.page, '檔案完整性 S25 頁首（檔案遺失）');
      await dark(al.page, '檔案完整性 S25 頁首（檔案遺失）');
      await shot(al.page, 'approval-release-missing');
      // 稽核員：看得到三段式，沒有「重新上傳…」
      const au = await session('audrey', '/pulse/releases', 'Pulse 版本庫');
      await au.page.locator('#release-files-missing').getByText('KRS-W8417').waitFor();
      assert.equal(await au.page.getByRole('button', { name: /^重新上傳/ }).count(), 0);
      await au.ctx.close();

      // 資格預覽：需要這個檔案的主機是「版本庫的檔案遺失」，不是可升級
      const pv = await api(al.page, 'POST', '/api/v1/rollouts/preview', { name: 'x', target_version: VERSION, selector: { all: true }, waves: [{ size: 'rest' }] });
      assert.equal(pv.status, 200, JSON.stringify(pv.json));
      // appr-01 與 appr-02 都是 tar／x86_64：兩台都需要這個檔案
      assert.deepEqual([pv.json.data.eligible, pv.json.data.skipped.artifact_missing], [0, 2]);
      assert.ok(pv.json.data.warnings.includes('artifact_problem'));
      await al.page.goto(`${BASE}/pulse/rollouts/new`);
      await waitTitle(al.page, '建立推送');
      await al.page.getByLabel(/^名稱/).fill('E2E 遺失');
      const preview = al.page.locator('#rollout-preview');
      await preview.locator('li[data-reason="artifact_missing"]').getByText(/版本庫的檔案遺失或大小不符/).waitFor({ timeout: 15000 });
      await preview.getByText('可升級 0 台').waitFor();
      const files = al.page.locator('#rollout-preview-files');
      await files.getByText(`目標版本 ${VERSION} 有 1 個發佈檔遺失或大小不符`).waitFor();
      await files.getByText('KRS-W8417').waitFor();
      assert.equal(await files.getByRole('link', { name: `前往 Pulse ${VERSION} 的版本詳情` }).getAttribute('href'), `/pulse/releases/${VERSION}`);
      assert.equal(await al.page.getByRole('button', { name: '送出推送…' }).getAttribute('aria-disabled'), 'true');
      await axe(al.page, '檔案完整性 建立推送（檔案遺失）');

      // 核准：伺服器在核准的交易中重新檢查檔案 → 409 artifact_missing，就地說明，狀態不變
      await ca.page.goto(`${BASE}/pulse/rollouts/${heldId}`);
      await waitTitle(ca.page, 'E2E 檔案');
      const sub = ca.page.locator('section[aria-labelledby="rollout-submission"]');
      await sub.locator('li[data-warning="artifact_problem"]').waitFor();
      await sub.getByText(`Pulse ${VERSION} 的 ${rel.tar}（檔案遺失，1 台主機需要）`).waitFor();
      await axe(ca.page, '檔案完整性 待核准的詳情（檔案遺失）');
      const dlg = await openApprove(ca.page);
      await dlg.getByLabel(/輸入「核准 1 台」以確認/).fill('核准 1 台');
      const [resp] = await Promise.all([ca.page.waitForResponse((r) => r.request().method() === 'POST' && r.url().endsWith(`/rollouts/${heldId}/approve`)), dlg.getByRole('button', { name: '核准 1 台', exact: true }).click()]);
      assert.equal(resp.status(), 409);
      const err = (await resp.json()).error;
      assert.deepEqual([err.code, err.product_code, err.details.files[0].filename], ['artifact_missing', 'KRS-W8417', rel.tar]);
      await dlg.getByText('版本庫的檔案遺失，無法核准').waitFor();
      await dlg.getByText('KRS-W8417').first().waitFor();
      await axe(ca.page, '檔案完整性 核准對話框（409 artifact_missing）');
      await dlg.getByRole('button', { name: '取消' }).click();
      await dlg.waitFor({ state: 'detached' });
      assert.equal((await stateOf(ca.page, heldId)).state, 'pending_approval');

      // S20「遠端管理」：警告、代碼、前往版本庫
      await ca.page.goto(`${BASE}/system/health`);
      await waitTitle(ca.page, '系統健康');
      const remote = ca.page.locator('[data-item="remote"]');
      await remote.getByText(/版本庫的檔案遺失/).waitFor();
      await remote.getByText('KRS-W8417').waitFor();
      assert.equal(await remote.getAttribute('data-status'), 'warning');
      await axe(ca.page, '檔案完整性 S20（檔案遺失）');

      // 重新上傳：頁首的按鈕直接開啟這個版本的上傳，補上後三段式消失
      await al.page.goto(`${BASE}/pulse/releases`);
      await waitTitle(al.page, 'Pulse 版本庫');
      await al.page.locator('#release-files-missing').getByRole('button', { name: `重新上傳 ${VERSION}…` }).click();
      const up = al.page.getByRole('dialog');
      await up.getByText(`上傳 Pulse ${VERSION} 的發佈檔`).waitFor();
      await up.getByLabel('檔案').setInputFiles([tarFile]);
      await up.getByRole('button', { name: '上傳發佈檔', exact: true }).click();
      await up.locator(`li[data-file="${rel.tar}"]`).getByText('✓ 雜湊相符').waitFor({ timeout: 20000 });
      await up.getByRole('button', { name: '完成' }).click();
      await up.waitFor({ state: 'detached' });
      await until(async () => (await al.page.locator('#release-files-missing').count()) === 0, 15000, '三段式消失');
      assert.ok(existsSync(stored), '檔案已補回');
    } finally {
      if (!existsSync(stored) && existsSync(`${stored}.bak`)) renameSync(`${stored}.bak`, stored);
    }

    // 補檔後核准成功。appr-01 先離線：沒有 mTLS 的測試環境不協商 upgrade，連線中的主機會在第一輪被標為
    // 「尚未就緒」；離線（寬限期內）的主機留在未開始，才看得到「等待檔案」
    a1.close();
    await ca.page.goto(`${BASE}/pulse/rollouts/${heldId}`);
    await waitTitle(ca.page, 'E2E 檔案');
    const dlg = await openApprove(ca.page);
    await dlg.getByLabel(/輸入「核准 1 台」以確認/).fill('核准 1 台');
    await dlg.getByRole('button', { name: '核准 1 台', exact: true }).click();
    await ca.page.getByText('已核准推送').first().waitFor();
    assert.equal((await stateOf(ca.page, heldId)).state, 'running');

    // 核准之後檔案又不見：不提議、主機留在未開始、不計入失敗；詳情頁與清單寫「版本庫的檔案遺失，N 台主機等待中」
    const keep = `${stored}.again`;
    renameSync(stored, keep);
    try {
      await until(async () => (await stateOf(ca.page, heldId)).next_action.code === 'waiting_artifact', 45000, 'next_action waiting_artifact');
      const st = await stateOf(ca.page, heldId);
      assert.deepEqual([st.state, st.next_action.hosts, st.progress.failed || 0], ['running', 1, 0]);
      await ca.page.reload();
      await waitTitle(ca.page, 'E2E 檔案');
      const wait = ca.page.locator('.ds-alert', { hasText: '版本庫的檔案遺失，1 台主機等待中' });
      await wait.getByText('KRS-W8417').waitFor();
      await wait.getByText(/不計入失敗/).waitFor();
      assert.equal(await wait.getByRole('link', { name: '前往 Pulse 版本庫' }).getAttribute('href'), `/pulse/releases/${VERSION}`);
      await axe(ca.page, '檔案完整性 推送詳情（等待檔案）');
      await dark(ca.page, '檔案完整性 推送詳情（等待檔案）');
      await ca.page.goto(`${BASE}/pulse/rollouts`);
      await waitTitle(ca.page, '升級推送');
      await ca.page.locator(`tr[data-rollout="${heldId}"]`).getByText('版本庫的檔案遺失，1 台主機等待中').waitFor();
      await axe(ca.page, '檔案完整性 推送清單（等待檔案）');
    } finally {
      renameSync(keep, stored);
    }
    // 補回後下一輪自動繼續（不再等待檔案）
    await until(async () => (await stateOf(ca.page, heldId)).next_action.code !== 'waiting_artifact', 45000, '補檔後繼續');
    await cancel(al.page, heldId);
    heldId = null;
    await al.ctx.close();
    await ca.ctx.close();
  });

  test('政策 E4／R4：S21「遠端管理」唯讀 → 改為 none 重新啟動 → 稽核、告警事件與 S20／S21／S26 的「雙人覆核已關閉」；既有的待核准仍要核准', async () => {
    const al = await session('alice');
    // 重新啟動之前送出的推送（降版，對象 appr-02）：政策關閉之後仍然要核准
    const id = await createAndSubmit(al.page, al.page, spec('E2E 政策', A2, { options: { allow_downgrade: true } }), 'E2E：政策測試');

    // S21「連線與安全」的「遠端管理」卡片：需要、來源、上一次改變（none → required，啟動時與資料庫的前值比對）
    for (const user of ['alice', 'oscar', 'audrey']) {
      const s = user === 'alice' ? al : await session(user);
      await s.page.goto(`${BASE}/system/settings/security`);
      await waitTitle(s.page, '系統設定');
      const cardEl = s.page.locator('section[aria-labelledby="settings-remote"]');
      const box = cardEl.locator('#rollout-approval-setting');
      await box.waitFor();
      assert.deepEqual([await box.getAttribute('data-value'), await box.getAttribute('data-source')], ['required', 'file'], user);
      await box.getByText('需要', { exact: true }).waitFor();
      await cardEl.locator('#rollout-approval-changed').getByText(/上一次改變：.*由「不需要」改為「需要」/).waitFor();
      await cardEl.getByText(/只能在 kairos\.toml（或環境變數）修改，需要重新啟動 Kairos；主控台不能修改/).waitFor();
      await cardEl.locator('tr[data-key="remote.rollout_approval"]').waitFor();
      // 沒有任何角色看得到可編輯的控制項
      assert.equal(await cardEl.locator('input, select, textarea, button').count(), 0, `${user} 的遠端管理卡片不應有控制項`);
      if (user === 'alice') {
        await axe(s.page, '政策 S21 遠端管理卡片');
        await dark(s.page, '政策 S21 遠端管理卡片');
        await shot(s.page, 'approval-settings');
      } else {
        await axe(s.page, `政策 S21 遠端管理卡片（${user}）`);
        await s.ctx.close();
      }
    }
    // S26 清單頁首：30 天內的改變（啟用是資訊）
    await al.page.goto(`${BASE}/pulse/rollouts`);
    await waitTitle(al.page, '升級推送');
    await al.page.locator('#rollout-approval-change[data-to="required"]').getByText(/雙人覆核已在 .* 啟用（設定檔）/).waitFor();
    await axe(al.page, '政策 S26 清單（雙人覆核已啟用）');
    await al.ctx.close();

    // 有主機權限的人把設定改為 none 並重新啟動：Kairos 在遠端管理開始接受請求之前寫入稽核，並開告警事件
    if (a1) a1.close();
    if (a2) a2.close();
    await restartKairos('none');

    const ca = await session('carol', '/pulse/rollouts', '升級推送');
    const off = ca.page.locator('#rollout-approval-change[data-to="none"]');
    await off.getByText(/雙人覆核已在 .* 關閉（設定檔）/).waitFor();
    await off.getByText(/已在待核准的推送仍然要核准或退回/).waitFor();
    const meta = (await api(ca.page, 'GET', '/api/v1/rollouts')).json.meta;
    assert.deepEqual([meta.rollout_approval, meta.rollout_approval_change.from, meta.rollout_approval_change.to, meta.pending_approval_actionable], ['none', 'required', 'none', 1]);
    await axe(ca.page, '政策 S26 清單（雙人覆核已關閉）');
    await dark(ca.page, '政策 S26 清單（雙人覆核已關閉）');
    await shot(ca.page, 'approval-disabled-list');
    // 稽核：system 寫入的 settings.rollout_approval.change（required → none）
    const audit = await api(ca.page, 'GET', '/api/v1/audit?action=settings.rollout_approval.change');
    assert.equal(audit.status, 200, JSON.stringify(audit.json));
    const changes = JSON.stringify(audit.json.data);
    assert.ok(audit.json.data.length >= 2, changes);
    assert.match(changes, /"from":"required"/);
    assert.match(changes, /"to":"none"/);
    // S20：警告、關閉的時間與來源、前往設定
    await ca.page.goto(`${BASE}/system/health`);
    await waitTitle(ca.page, '系統健康');
    const remote = ca.page.locator('[data-item="remote"]');
    await remote.getByText(/雙人覆核已關閉（remote\.rollout_approval 由 required 改為 none）/).waitFor();
    assert.equal(await remote.getAttribute('data-status'), 'warning');
    await remote.locator('#health-approval-off').getByText(/雙人覆核已在 .* 關閉（設定檔）/).waitFor();
    assert.equal(await remote.getByRole('link', { name: '查看雙人覆核的設定' }).getAttribute('href'), '/system/settings/security');
    await axe(ca.page, '政策 S20（雙人覆核已關閉）');
    await dark(ca.page, '政策 S20（雙人覆核已關閉）');
    // S21：不需要、上一次改變
    await ca.page.goto(`${BASE}/system/settings/security`);
    await waitTitle(ca.page, '系統設定');
    const box = ca.page.locator('#rollout-approval-setting');
    await box.waitFor();
    assert.equal(await box.getAttribute('data-value'), 'none');
    await box.getByText('不需要', { exact: true }).waitFor();
    await ca.page.locator('#rollout-approval-changed').getByText(/上一次改變：.*由「需要」改為「不需要」/).waitFor();
    await axe(ca.page, '政策 S21（雙人覆核已關閉）');
    // 告警事件「雙人覆核已關閉」（系統規則，警告）
    await until(
      async () => {
        await ca.page.goto(`${BASE}/alerts/events?view=all`);
        await waitTitle(ca.page, '告警事件');
        return ca.page
          .getByText(/雙人覆核已關閉/)
          .first()
          .waitFor({ timeout: 8000 })
          .then(() => true)
          .catch(() => false);
      },
      120000,
      '告警事件「雙人覆核已關閉」',
    );
    await axe(ca.page, '政策 告警事件（雙人覆核已關閉）');

    // 關閉之後：新的推送不需要核准；重新啟動之前已經待核准的推送仍然要核准（核准只看推送的狀態）
    const pv = await api(ca.page, 'POST', '/api/v1/rollouts/preview', spec('x', A1));
    assert.equal(pv.json.data.approval_required, false);
    assert.equal((await stateOf(ca.page, id)).state, 'pending_approval');
    await ca.page.goto(`${BASE}/pulse/rollouts/${id}`);
    await waitTitle(ca.page, 'E2E 政策');
    const dlg = await openApprove(ca.page);
    await dlg.getByLabel(/輸入「核准降版 1 台」以確認/).fill('核准降版 1 台');
    await dlg.getByLabel(/原因/).fill('E2E：政策關閉後核准既有的推送');
    await dlg.getByRole('button', { name: '核准降版 1 台', exact: true }).click();
    await ca.page.getByText('已核准推送').first().waitFor();
    assert.equal((await stateOf(ca.page, id)).approved_by, 'user:carol');
    // 0.10.1（H2）：核准過的推送 approval = approved；稽核的 rollout.start 記下開始時生效的設定（已是 none）與核准者
    assert.equal((await stateOf(ca.page, id)).approval, 'approved');
    const started = (await api(ca.page, 'GET', '/api/v1/audit?action=rollout.start')).json.data.find((e) => e.object === id);
    assert.ok(started, '核准後開始的稽核');
    assert.deepEqual([started.details.rollout_approval, started.details.approval, started.details.approved_by], ['none', 'approved', 'user:carol']);
    assert.equal(await ca.page.locator('#rollout-unapproved').count(), 0, '核准過的推送沒有「未經核准」的說明');
    await cancel(ca.page, id);

    // 關閉之後送出的推送：不必核准就排程（approval = not_required），清單與詳情標示「未經核准」，稽核記下送出時的設定。
    // 這個 Kairos 目前的設定是 none，所以是中性的標籤與資訊
    const startsAt = new Date(Date.now() + 6 * 3600 * 1000).toISOString();
    const c2 = await api(ca.page, 'POST', '/api/v1/rollouts', spec('E2E 未經核准', A1, { schedule: { starts_at: startsAt } }));
    assert.equal(c2.status, 201, JSON.stringify(c2.json));
    const id2 = c2.json.data.id;
    const d2 = await api(ca.page, 'GET', `/api/v1/rollouts/${id2}`);
    const s2 = await api(ca.page, 'POST', `/api/v1/rollouts/${id2}/submit`, { confirm: d2.json.data.preview.confirm_text });
    assert.equal(s2.status, 200, JSON.stringify(s2.json));
    assert.deepEqual([s2.json.data.state, s2.json.data.approval, s2.json.data.approved_by], ['scheduled', 'not_required', null]);
    const submitted = (await api(ca.page, 'GET', '/api/v1/audit?action=rollout.submit')).json.data.find((e) => e.object === id2);
    assert.equal(submitted.details.rollout_approval, 'none');
    await ca.page.goto(`${BASE}/pulse/rollouts`);
    await waitTitle(ca.page, '升級推送');
    const row2 = ca.page.locator(`tr[data-rollout="${id2}"]`);
    const badge2 = row2.locator('[data-approval="not_required"]');
    await badge2.waitFor();
    assert.equal((await badge2.textContent()).trim(), '未經核准');
    assert.match(await badge2.getAttribute('class'), /ds-status-badge--neutral/);
    assert.equal(await ca.page.locator(`tr[data-rollout="${id}"] [data-approval]`).count(), 0, '核准過的推送沒有標記');
    await axe(ca.page, '政策 S26 清單（未經核准的推送）');
    await dark(ca.page, '政策 S26 清單（未經核准的推送）');
    await ca.page.goto(`${BASE}/pulse/rollouts/${id2}`);
    await waitTitle(ca.page, 'E2E 未經核准');
    const note = ca.page.locator('#rollout-unapproved');
    await note.getByText('這個推送沒有經過另一位管理員核准，到排程的時間就會開始').waitFor();
    assert.equal(await note.getAttribute('data-emphasis'), 'info');
    await ca.page.locator('#rollout-approval[data-approval="not_required"]').waitFor();
    await axe(ca.page, '政策 S26 詳情（未經核准的推送）');
    await dark(ca.page, '政策 S26 詳情（未經核准的推送）');
    await shot(ca.page, 'approval-unapproved-detail');

    // 改回 required 重新啟動：覆核關閉期間送出的推送仍然標示，而且是警告（目前的設定要求覆核）
    await restartKairos('required', 'none');
    const cb = await session('carol', '/pulse/rollouts', '升級推送');
    const warnBadge = cb.page.locator(`tr[data-rollout="${id2}"] [data-approval="not_required"]`);
    await warnBadge.waitFor();
    assert.equal((await warnBadge.textContent()).trim(), '未經核准');
    assert.match(await warnBadge.getAttribute('class'), /ds-status-badge--warning/);
    await axe(cb.page, '政策 S26 清單（未經核准的推送，目前要求覆核）');
    await dark(cb.page, '政策 S26 清單（未經核准的推送，目前要求覆核）');
    await cb.page.goto(`${BASE}/pulse/rollouts/${id2}`);
    await waitTitle(cb.page, 'E2E 未經核准');
    const warnNote = cb.page.locator('#rollout-unapproved');
    await warnNote.getByText(/目前的設定要求雙人覆核：這個推送是在覆核關閉的期間送出的/).waitFor();
    assert.equal(await warnNote.getAttribute('data-emphasis'), 'warning');
    await axe(cb.page, '政策 S26 詳情（未經核准的推送，目前要求覆核）');
    await dark(cb.page, '政策 S26 詳情（未經核准的推送，目前要求覆核）');
    await shot(cb.page, 'approval-unapproved-detail-required');
    await cancel(cb.page, id2);
    await cb.ctx.close();
    await ca.ctx.close();
  });
}
