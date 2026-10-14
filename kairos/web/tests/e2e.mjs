// 主控台網頁介面的端對端測試（由 tests/run.sh 啟動 Kairos 後執行）。
// 使用 playwright（瀏覽器自動化）與 axe-core（無障礙檢查）；不需要 @playwright/test，測試以簡單的
// 執行器依序執行，任何一項失敗時結束代碼 1。

import { createRequire } from 'node:module';
import { createHmac } from 'node:crypto';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import assert from 'node:assert/strict';
import { FakeAgent } from './fake-agent.mjs';

const PW_DIR = process.env.PW_DIR;
const require = createRequire(join(PW_DIR, 'node_modules', 'noop.js'));
const { chromium } = require('playwright');
const AXE = readFileSync(require.resolve('axe-core/axe.min.js'), 'utf8');
let jsQR = null;
try {
  jsQR = require('jsqr');
} catch {
  jsQR = null;
}

const URL0 = process.env.KAIROS_URL;
const PW = process.env.PW;
const OUT = process.env.OUT_DIR;
mkdirSync(OUT, { recursive: true });

// ---------------------------------------------------------------------------
// 測試資料：假 agent（含主機名稱帶 HTML 的主機，XSS 測試）
const XSS_NAME = '<script>window.__xss=1</script><img src=x onerror="window.__xss=2">';
const AGENTS = [
  { agentId: '11111111-1111-4111-8111-000000000001', hostname: 'web-01', tags: ['env:prod', 'web'] },
  { agentId: '11111111-1111-4111-8111-000000000002', hostname: 'db-02', tags: ['env:prod', 'db'], os: { name: 'Windows', version: '10 IoT LTSC', arch: 'x86_64' } },
  { agentId: '11111111-1111-4111-8111-000000000003', hostname: XSS_NAME, tags: ['lab'] },
  { agentId: '11111111-1111-4111-8111-000000000004', hostname: 'hmi-line3-07', tags: ['HMI', 'Line3'], os: { name: 'Windows', version: '10 IoT LTSC', arch: 'x86_64' } },
];
const agents = [];

// ---------------------------------------------------------------------------
// 小工具
function base32Decode(s) {
  const alpha = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
  let bits = 0;
  let val = 0;
  const out = [];
  for (const c of s.replace(/[\s=]/g, '').toUpperCase()) {
    val = (val << 5) | alpha.indexOf(c);
    bits += 5;
    if (bits >= 8) {
      out.push((val >>> (bits - 8)) & 0xff);
      bits -= 8;
    }
  }
  return Buffer.from(out);
}

function totp(secret, step = Math.floor(Date.now() / 30000)) {
  const msg = Buffer.alloc(8);
  msg.writeBigUInt64BE(BigInt(step));
  const mac = createHmac('sha1', base32Decode(secret)).update(msg).digest();
  const off = mac[mac.length - 1] & 0xf;
  const code = ((mac.readUInt32BE(off) & 0x7fffffff) % 1000000).toString().padStart(6, '0');
  return code;
}

let browser;
const consoleErrors = [];
const openPages = [];

async function newPage({ bypassCSP = false, viewport = { width: 1440, height: 900 }, colorScheme = 'light', expectCspErrors = false } = {}) {
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true, bypassCSP, viewport, colorScheme, locale: 'zh-TW', timezoneId: 'Asia/Taipei' });
  const page = await ctx.newPage();
  openPages.push(page);
  page.on('console', (m) => {
    if (m.type() !== 'error') return;
    const text = m.text();
    // 401／403／404 的 fetch 會由瀏覽器記一行「Failed to load resource」：屬於預期的 API 回應
    if (/Failed to load resource/.test(text)) return;
    // CSP 測試刻意觸發的違規
    if (expectCspErrors && /Content Security Policy|Trusted|unsafe-eval/i.test(text)) return;
    consoleErrors.push(`${page.url()}：${text}`);
  });
  page.on('pageerror', (e) => consoleErrors.push(`${page.url()}：pageerror ${e.message}`));
  return { ctx, page };
}

async function login(page, user, password = PW, next = '/') {
  await page.goto(`${URL0}/login?next=${encodeURIComponent(next)}`);
  await page.getByLabel('帳號').fill(user);
  await page.locator('input[name="password"]').fill(password);
  await page.getByRole('button', { name: '登入', exact: true }).click();
}

async function waitTitle(page, text) {
  await page.locator('#page-title', { hasText: text }).waitFor({ timeout: 15000 });
}

const axeResults = [];
async function axe(page, label) {
  await page.evaluate(AXE);
  const res = await page.evaluate(async () => {
    // eslint-disable-next-line no-undef
    const r = await axe.run(document, { resultTypes: ['violations'] });
    return r.violations.map((v) => ({ id: v.id, impact: v.impact, help: v.help, nodes: v.nodes.map((n) => n.target.join(' ')).slice(0, 5) }));
  });
  axeResults.push({ label, violations: res });
  if (res.length) throw new Error(`axe（${label}）：${JSON.stringify(res, null, 1)}`);
}

async function shot(page, name) {
  // 桌機只拍可視範圍（整頁截圖時 Chromium 會暫時放大視窗，sticky 的側邊導覽會被拍成過渡中的寬度）
  const narrow = page.viewportSize().width < 769;
  await page.screenshot({ path: join(OUT, `${name}.png`), fullPage: narrow });
}

// ---------------------------------------------------------------------------
const tests = [];
const test = (name, fn) => tests.push({ name, fn });

test('安全標頭與 CSP：拒絕 inline script、Trusted Types 擋下 innerHTML', async () => {
  const { ctx, page } = await newPage({ expectCspErrors: true });
  const resp = await page.goto(`${URL0}/login`);
  const hd = resp.headers();
  assert.match(hd['content-security-policy'], /script-src 'self'/);
  assert.doesNotMatch(hd['content-security-policy'], /unsafe-inline|unsafe-eval/);
  assert.match(hd['content-security-policy'], /frame-ancestors 'none'/);
  assert.equal(hd['x-frame-options'], 'DENY');
  assert.equal(hd['x-content-type-options'], 'nosniff');
  assert.match(hd['strict-transport-security'], /max-age=/);
  await page.locator('#page-title').waitFor();
  const r = await page.evaluate(async () => {
    const violations = [];
    document.addEventListener('securitypolicyviolation', (e) => violations.push(e.violatedDirective));
    // 1. 以 DOM 加入 inline script（不經過 Trusted Types 的字串入口）
    const s = document.createElement('script');
    s.appendChild(document.createTextNode('window.__inline = 1'));
    document.body.appendChild(s);
    // 2. innerHTML：Trusted Types 拒絕
    // 2. 行內事件處理（onclick 屬性）：script-src 沒有 unsafe-inline
    let handlerBlocked = false;
    try {
      const b = document.createElement('button');
      b.setAttribute('onclick', 'window.__inline = 4');
      document.body.appendChild(b);
      b.click();
    } catch {
      handlerBlocked = true;
    }
    let ttBlocked = false;
    try {
      document.createElement('div').innerHTML = '<img src=x onerror="window.__inline=2">';
    } catch {
      ttBlocked = true;
    }
    await new Promise((res) => setTimeout(res, 200));
    return { inline: window.__inline, ttBlocked, handlerBlocked, violations };
  });
  assert.equal(r.inline, undefined, 'inline script 不應執行');
  assert.ok(r.ttBlocked, 'innerHTML 應被 Trusted Types 擋下');
  assert.ok(r.handlerBlocked || r.violations.some((v) => v.startsWith('script-src')), `行內事件處理應被擋下：${r.violations}`);
  assert.ok(r.violations.includes('require-trusted-types-for'), `inline script 應被 Trusted Types 擋下：${r.violations}`);
  await ctx.close();
});

test('登入 → 總覽 → 主機清單（篩選、排序、CIDR）→ 主機詳情 → 登出', async () => {
  const { ctx, page } = await newPage();
  await page.goto(`${URL0}/hosts`);
  // 未登入：帶 next 回登入頁
  await page.waitForURL(/\/login\?next=%2Fhosts/);
  await page.getByText('登入後將前往：主機').waitFor();
  await login(page, 'admin', PW, '/');
  await waitTitle(page, '總覽');
  // 六個狀態磚：線上至少 3 台；嚴重、警告、待核准為「—」（不顯示 0）
  await page.locator('[data-kpi="online"]').waitFor();
  const online = Number((await page.locator('[data-kpi="online"]').innerText()).replace(/[^\d]/g, ''));
  assert.ok(online >= 3, `線上 ${online}`);
  assert.equal((await page.locator('[data-kpi="critical"]').innerText()).trim(), '—');
  await axe(page, '總覽');
  const nav = await page.evaluate(() => {
    const n = document.querySelector('.ds-sidenav');
    const cs = getComputedStyle(n);
    return { w: n.getBoundingClientRect().width, width: cs.width, flex: cs.flex, trans: cs.transition, cls: document.documentElement.className };
  });
  assert.ok(nav.w >= 200, `側邊導覽寬度 ${JSON.stringify(nav)}`);
  await shot(page, 'overview-light');
  // 線上磚 → 主機清單（帶狀態篩選）
  await page.locator('a.ds-kpi', { has: page.locator('[data-kpi="online"]') }).click();
  await waitTitle(page, '主機');
  await page.waitForURL(/status=online/);
  await page.locator('tbody tr[data-key]').first().waitFor();
  const pressed = await page.locator('.ds-filterbar__chip[data-status="online"]').getAttribute('aria-pressed');
  assert.equal(pressed, 'true');
  // 清除篩選後依主機名稱排序
  await page.getByRole('button', { name: '清除篩選' }).first().click();
  await page.waitForURL((u) => !u.search.includes('status='));
  await page.getByRole('button', { name: /^主機名稱/ }).click();
  await page.waitForURL(/sort=hostname/);
  await page.locator('th[aria-sort="ascending"]').waitFor();
  // CIDR 搜尋：127.0.0.0/8 符合所有 agent；主機位元不為 0 時顯示建議
  const search = page.getByLabel('搜尋', { exact: true });
  await search.fill('127.0.0.0/8');
  await search.press('Enter');
  await page.waitForURL(/q=127\.0\.0\.0%2F8/);
  await page.locator('tbody tr[data-key]').first().waitFor();
  await search.fill('127.0.0.1/8');
  await search.press('Enter');
  await page.locator('.ds-field__error:not([hidden])').waitFor();
  await page.getByText('搜尋條件的格式不正確').waitFor();
  await search.fill('web-01');
  await search.press('Enter');
  await page.locator('tbody tr[data-key]').first().waitFor();
  assert.equal(await page.locator('tbody tr[data-key]').count(), 1);
  await axe(page, '主機清單');
  await shot(page, 'hosts-light');
  // 主機詳情
  await page.getByRole('link', { name: 'web-01' }).click();
  await waitTitle(page, 'web-01');
  await page.getByText('狀態說明').waitFor();
  await page.getByText('最近 24 小時（依回報資料）').waitFor();
  await axe(page, '主機詳情：概覽');
  await shot(page, 'host-overview-light');
  await page.getByRole('link', { name: '趨勢', exact: true }).click();
  await page.waitForURL(/\/metrics/);
  await page.locator('svg.k-chart').first().waitFor({ timeout: 15000 });
  await page.getByRole('button', { name: '1 小時' }).click();
  await page.locator('svg.k-chart, .ds-empty').first().waitFor();
  await axe(page, '主機詳情：趨勢');
  await shot(page, 'host-metrics-light');
  await page.getByRole('link', { name: '連線與憑證' }).click();
  await page.getByRole('heading', { name: '最近的上線與離線' }).waitFor();
  await page.locator('table.k-mini-table td', { hasText: '上線' }).first().waitFor();
  await axe(page, '主機詳情：連線與憑證');
  // 不存在的主機
  await page.goto(`${URL0}/hosts/99999999-9999-4999-8999-999999999999`);
  await page.getByText('找不到這台主機').waitFor();
  // 登出
  await page.getByRole('button', { name: /使用者選單/ }).click();
  await page.getByRole('menuitem', { name: '登出' }).click();
  await page.waitForURL(/\/login\?reason=logged_out/);
  await page.getByText('已登出').waitFor();
  // 登出後回上一頁也不能看到資料
  await page.goto(`${URL0}/`);
  await page.waitForURL(/\/login/);
  await ctx.close();
});

test('不在 localStorage 放 token；工作階段 cookie 為 HttpOnly', async () => {
  const { ctx, page } = await newPage();
  await login(page, 'viewer');
  await waitTitle(page, '總覽');
  const store = await page.evaluate(() => {
    const out = {};
    for (const s of [localStorage, sessionStorage]) for (let i = 0; i < s.length; i += 1) out[s.key(i)] = s.getItem(s.key(i));
    return { out, cookie: document.cookie };
  });
  for (const [k, v] of Object.entries(store.out)) {
    assert.ok(k.startsWith('kairos.'), `未預期的 storage key ${k}`);
    assert.ok(String(v).length < 40, `storage 的值太長（可能是秘密）：${k}`);
  }
  assert.ok(!store.cookie.includes('kairos_session'), 'session cookie 不可被 JavaScript 讀取');
  const cookies = await ctx.cookies();
  const c = cookies.find((x) => x.name === '__Host-kairos_session');
  assert.ok(c && c.httpOnly && c.secure && c.sameSite === 'Strict', JSON.stringify(cookies));
  await ctx.close();
});

test('XSS：含 <script> 的主機名稱以文字顯示', async () => {
  const { ctx, page } = await newPage();
  await login(page, 'admin', PW, '/hosts?q=lab');
  await waitTitle(page, '主機');
  await page.goto(`${URL0}/hosts?q=${encodeURIComponent('<script>')}`);
  const link = page.locator('a.k-host-name', { hasText: '<script>' });
  await link.waitFor();
  assert.equal(await link.textContent(), XSS_NAME);
  await link.click();
  await page.locator('#page-title', { hasText: '<script>' }).waitFor();
  assert.equal(await page.locator('#page-title').textContent(), XSS_NAME);
  // 全域搜尋
  await page.keyboard.press('Control+k');
  await page.getByRole('combobox').fill('<img');
  await page.getByRole('option', { name: /<img src=x/ }).first().waitFor();
  const flags = await page.evaluate(() => ({ xss: window.__xss, imgs: document.querySelectorAll('img[src="x"]').length, scripts: Array.from(document.scripts).filter((s) => s.textContent.includes('__xss')).length }));
  assert.deepEqual(flags, { xss: undefined, imgs: 0, scripts: 0 });
  await ctx.close();
});

test('即時更新：主機離線後清單的狀態欄就地更新並提示更新列表', async () => {
  const { ctx, page } = await newPage();
  await login(page, 'operator', PW, '/hosts');
  await waitTitle(page, '主機');
  const row = page.locator('tr[data-key="11111111-1111-4111-8111-000000000004"]');
  await row.locator('[data-status="online"]').waitFor();
  agents[3].close();
  await row.locator('[data-status="offline"]').waitFor({ timeout: 20000 });
  await page.getByText(/有 \d+ 台狀態變化/).waitFor();
  await page.getByRole('button', { name: '更新列表' }).click();
  await page.locator('.ds-filterbar__chip[data-status="offline"] .k-n', { hasText: /^[1-9]/ }).waitFor();
  // 連線紀錄有這次的離線
  await page.goto(`${URL0}/hosts/11111111-1111-4111-8111-000000000004/connection`);
  await page.locator('td', { hasText: '連線關閉' }).first().waitFor();
  await ctx.close();
});

test('鍵盤：跳到主要內容、Ctrl+K 搜尋、↓ Enter 開啟主機', async () => {
  const { ctx, page } = await newPage();
  await login(page, 'viewer', PW, '/hosts');
  await waitTitle(page, '主機');
  // 重新載入：第一次繪製時焦點在頁面開頭，第一個 Tab 是「跳到主要內容」
  await page.reload();
  await waitTitle(page, '主機');
  await page.keyboard.press('Tab');
  const skip = await page.evaluate(() => document.activeElement.textContent);
  assert.equal(skip, '跳到主要內容');
  await page.keyboard.press('Enter');
  assert.equal(await page.evaluate(() => document.activeElement.id), 'page-title');
  await page.keyboard.press('Control+k');
  await page.keyboard.type('db-0');
  await page.getByRole('option', { name: /db-02/ }).waitFor();
  await axe(page, '全域搜尋');
  await page.keyboard.press('Enter');
  await waitTitle(page, 'db-02');
  // 使用者選單以鍵盤操作
  await page.getByRole('button', { name: /使用者選單/ }).focus();
  await page.keyboard.press('Enter');
  assert.equal(await page.evaluate(() => document.activeElement.getAttribute('role')), 'menuitem');
  await page.keyboard.press('Escape');
  assert.match(await page.evaluate(() => document.activeElement.getAttribute('aria-label') || ''), /使用者選單/);
  await ctx.close();
});

test('兩步驟驗證：啟用（QR code、復原碼）、錯誤的驗證碼、以驗證碼登入', async () => {
  const { ctx, page } = await newPage();
  await login(page, 'mfauser', PW, '/account');
  await waitTitle(page, '個人設定');
  await axe(page, '個人設定');
  await shot(page, 'account-light');
  await page.getByRole('button', { name: '啟用兩步驟驗證…' }).click();
  await page.getByRole('dialog').getByLabel(/目前的密碼/).fill(PW);
  await page.getByRole('button', { name: '下一步' }).click();
  const pre = page.locator('dialog pre.ds-code-block');
  await pre.waitFor();
  const secret = (await pre.evaluate((el) => el.firstChild.textContent)).replace(/\s/g, '');
  assert.match(secret, /^[A-Z2-7]{16,}$/);
  // QR code 的內容是 otpauth URI（有 jsQR 時解碼確認）
  const svg = page.locator('dialog svg.k-qr');
  await svg.waitFor();
  if (jsQR) {
    const png = await svg.screenshot();
    const { PNG } = require('pngjs');
    const img = PNG.sync.read(png);
    const decoded = jsQR(new Uint8ClampedArray(img.data), img.width, img.height);
    assert.ok(decoded, 'QR code 應可解碼');
    assert.match(decoded.data, new RegExp(`^otpauth://totp/.*secret=${secret}`));
  }
  await axe(page, '兩步驟驗證：QR code');
  const step = Math.floor(Date.now() / 30000);
  await page.getByRole('dialog').getByLabel(/驗證碼/).fill(totp(secret, step));
  await page.getByRole('button', { name: '下一步' }).click();
  await page.getByText('只會顯示這一次').waitFor();
  const codes = await page.locator('.k-recovery-codes li').allTextContents();
  assert.equal(codes.length, 10);
  // 沒有勾選前不能關閉
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('dialog[open]').count(), 1);
  await page.getByLabel('我已保存復原碼').check();
  await page.getByRole('button', { name: '關閉', exact: true }).last().click();
  await page.locator('dialog[open]').waitFor({ state: 'detached' });
  await page.getByText('已啟用', { exact: true }).waitFor();
  // 登出後重新登入：第二步
  await page.getByRole('button', { name: /使用者選單/ }).click();
  await page.getByRole('menuitem', { name: '登出' }).click();
  await page.waitForURL(/\/login/);
  await login(page, 'mfauser');
  await page.locator('#page-title', { hasText: '兩步驟驗證' }).waitFor();
  await axe(page, '登入：兩步驟驗證');
  await page.getByLabel('驗證碼').fill('000000');
  await page.getByRole('button', { name: '驗證', exact: true }).click();
  await page.getByText('KRS-E6012').waitFor();
  // 以復原碼登入（同一個時間步的驗證碼不能重複使用）
  await page.getByRole('button', { name: '改用復原碼' }).click();
  await page.getByLabel('復原碼').fill(codes[0]);
  await page.getByRole('button', { name: '驗證', exact: true }).click();
  await waitTitle(page, '總覽');
  await ctx.close();
});

test('第一次登入必須變更密碼', async () => {
  const { ctx, page } = await newPage();
  await login(page, 'newbie', process.env.NEWBIE_PW, '/hosts');
  await page.locator('#page-title', { hasText: '變更密碼' }).waitFor();
  await axe(page, '登入：變更密碼');
  // 直接前往其他頁面也會回到這裡
  await page.goto(`${URL0}/hosts`);
  await page.locator('#page-title', { hasText: '變更密碼' }).waitFor();
  await page.getByLabel(/目前的密碼/).fill(process.env.NEWBIE_PW);
  await page.getByLabel(/^新密碼/).fill('password');
  await page.getByLabel(/確認新密碼/).fill('password');
  await page.getByRole('button', { name: '變更密碼' }).click();
  await page.locator('.ds-field__error:not([hidden])').waitFor();
  await page.getByLabel(/^新密碼/).fill('Tidal-Lantern-Quartz-58');
  await page.getByLabel(/確認新密碼/).fill('Tidal-Lantern-Quartz-58');
  await page.getByRole('button', { name: '變更密碼' }).click();
  await waitTitle(page, '主機');
  await ctx.close();
});

test('權限：沒有角色的帳號看不到監控選單，直接存取回 403；系統狀態燈依權限', async () => {
  const { ctx, page } = await newPage();
  await login(page, 'noroles');
  await page.locator('#page-title', { hasText: '沒有權限檢視這個畫面' }).waitFor();
  assert.equal(await page.locator('.ds-sidenav__link').count(), 0);
  assert.equal(await page.locator('.k-status-light:not([hidden])').count(), 0);
  await axe(page, '403');
  await page.goto(`${URL0}/hosts/11111111-1111-4111-8111-000000000001`);
  await page.locator('#page-title', { hasText: '沒有權限檢視這個畫面' }).waitFor();
  // 個人設定仍可使用
  await page.goto(`${URL0}/account`);
  await waitTitle(page, '個人設定');
  // 不存在的頁面
  await page.goto(`${URL0}/nope`);
  await page.locator('#page-title', { hasText: '找不到這個頁面' }).waitFor();
  await axe(page, '404');
  await ctx.close();
  // 唯讀：有主機與系統狀態燈
  const v = await newPage();
  await login(v.page, 'viewer');
  await waitTitle(v.page, '總覽');
  assert.deepEqual(await v.page.locator('.ds-sidenav__link .ds-sidenav__label').allTextContents(), ['總覽', '主機']);
  await v.page.locator('.k-status-light:not([hidden])').waitFor();
  await v.ctx.close();
});

test('工作階段被結束：任何頁面帶 next 回登入頁並說明', async () => {
  const a = await newPage();
  const b = await newPage();
  await login(a.page, 'sessuser', PW, '/hosts');
  await waitTitle(a.page, '主機');
  await login(b.page, 'sessuser', PW, '/account');
  await waitTitle(b.page, '個人設定');
  await b.page.getByRole('button', { name: '結束其他所有工作階段…' }).click();
  await b.page.getByRole('dialog').getByText('目前這個工作階段會保留。').waitFor();
  await b.page.getByRole('dialog').getByRole('button', { name: '結束其他所有工作階段' }).click();
  await b.page.getByText(/已結束其他所有工作階段（1 個）/).waitFor();
  // A 的下一個請求得到 401 → 登入頁
  await a.page.getByRole('link', { name: '總覽' }).click();
  await a.page.waitForURL(/reason=session_expired/);
  await a.page.getByText('工作階段已結束，請重新登入').waitFor();
  await axe(a.page, '登入：工作階段已結束');
  await a.ctx.close();
  await b.ctx.close();
});

test('深色主題與窄螢幕：axe 0 違規、截圖', async () => {
  const { ctx, page } = await newPage({ colorScheme: 'dark' });
  await login(page, 'admin');
  await waitTitle(page, '總覽');
  assert.equal(await page.evaluate(() => getComputedStyle(document.body).backgroundColor !== 'rgb(255, 255, 255)'), true);
  await axe(page, '總覽（深色）');
  await shot(page, 'overview-dark');
  await page.goto(`${URL0}/hosts`);
  await page.locator('tbody tr[data-key]').first().waitFor();
  await axe(page, '主機清單（深色）');
  await shot(page, 'hosts-dark');
  await page.goto(`${URL0}/hosts/11111111-1111-4111-8111-000000000001/metrics`);
  await page.locator('svg.k-chart').first().waitFor({ timeout: 15000 });
  await axe(page, '趨勢（深色）');
  await shot(page, 'host-metrics-dark');
  await page.goto(`${URL0}/account`);
  await waitTitle(page, '個人設定');
  // 個人設定切換為淺色：只存在這個瀏覽器
  await page.getByLabel('淺色').check();
  assert.equal(await page.evaluate(() => document.documentElement.getAttribute('data-theme')), 'light');
  assert.equal(await page.evaluate(() => localStorage.getItem('kairos.theme')), 'light');
  await ctx.close();
  const m = await newPage({ viewport: { width: 375, height: 800 } });
  await login(m.page, 'viewer');
  await waitTitle(m.page, '總覽');
  const overflow = await m.page.evaluate(() => {
    const w = document.documentElement.clientWidth;
    const wide = Array.from(document.querySelectorAll('body *'))
      .filter((el) => el.getBoundingClientRect().right > w + 1 && !el.closest('.k-scroll-x, .ds-table-wrap, .ds-sidenav'))
      .slice(0, 5)
      .map((el) => `${el.tagName.toLowerCase()}.${el.className && el.className.baseVal == null ? el.className : ''}`);
    return { px: document.documentElement.scrollWidth - w, wide };
  });
  assert.ok(overflow.px <= 0, `窄螢幕水平溢出 ${overflow.px}px：${overflow.wide.join('、')}`);
  await m.page.getByRole('button', { name: '開啟導覽選單' }).click();
  await m.page.getByRole('link', { name: '主機', exact: true }).click();
  await waitTitle(m.page, '主機');
  await axe(m.page, '主機清單（窄螢幕）');
  await shot(m.page, 'hosts-mobile');
  await m.ctx.close();
});

test('帳號鎖定的訊息（KRS-E6003）與帳號密碼錯誤（KRS-E6001）', async () => {
  const { ctx, page } = await newPage();
  await login(page, 'lockme', 'wrong-password-1');
  await page.getByText('KRS-E6001').waitFor();
  await axe(page, '登入：錯誤');
  for (let i = 0; i < 3; i += 1) {
    await page.locator('input[name="password"]').fill('wrong-password-x');
    await page.getByRole('button', { name: '登入', exact: true }).click();
    await page.waitForTimeout(300);
  }
  await page.getByText('KRS-E6003').waitFor();
  await page.getByText(/帳號暫時鎖定/).waitFor();
  await ctx.close();
});


// ---------------------------------------------------------------------------
async function main() {
  for (const a of AGENTS) {
    const ag = new FakeAgent({ port: Number(process.env.AGENT_PORT), ...a });
    await ag.connect();
    agents.push(ag);
  }
  // 等補送的資料都寫入（每筆都收到 ack），最多 60 秒
  const deadline = Date.now() + 60000;
  while (agents.some((a) => a.acked < a.reportSeq) && Date.now() < deadline) await new Promise((r) => setTimeout(r, 500));
  await new Promise((r) => setTimeout(r, 1500));
  browser = await chromium.launch();
  const only = process.env.ONLY;
  let failed = 0;
  const results = [];
  for (const tc of tests) {
    if (only && !tc.name.includes(only)) continue;
    const t0 = Date.now();
    openPages.length = 0;
    try {
      await tc.fn();
      results.push({ name: tc.name, ok: true, ms: Date.now() - t0 });
      console.log(`ok   ${tc.name}（${Date.now() - t0} ms）`);
    } catch (e) {
      failed += 1;
      for (const [i, p] of openPages.entries()) {
        if (!p.isClosed()) await p.screenshot({ path: join(OUT, `fail-${results.length}-${i}.png`) }).catch(() => {});
      }
      results.push({ name: tc.name, ok: false, error: String(e && e.stack) });
      console.log(`FAIL ${tc.name}\n     ${String(e && e.stack).split('\n').slice(0, 12).join('\n     ')}`);
    }
  }
  await browser.close();
  for (const a of agents) a.close();
  const axePages = axeResults.length;
  const axeViolations = axeResults.reduce((n, r) => n + r.violations.length, 0);
  console.log(`axe：${axePages} 個畫面，${axeViolations} 個違規`);
  if (consoleErrors.length) {
    console.log(`瀏覽器主控台錯誤 ${consoleErrors.length} 則：\n  ${consoleErrors.join('\n  ')}`);
  }
  writeFileSync(join(OUT, 'results.json'), JSON.stringify({ results, axe: axeResults, consoleErrors }, null, 1));
  const total = results.length;
  console.log(`${total - failed} / ${total} 通過`);
  process.exit(failed || consoleErrors.length ? 1 : 0);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
