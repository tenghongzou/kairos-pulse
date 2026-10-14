#!/usr/bin/env node
// 從設計包複製主控台需要的檔案（開發時執行；不在發佈流程中，也不嵌入執行檔）：
//
//   node kairos/web/tools/sync-design.mjs [設計包目錄]
//
// 設計包目錄預設為 repo 的上一層的 design/（F:\sync-project\design；kp-build 為 ~/design）。
// 1. design-system/tokens.css → kairos/web/assets/tokens.css（逐位元組複製，換行改為 LF），
//    並把 SHA-256 寫入 kairos/web/tokens.css.sha256（cargo test 的 tokens_css_matches_the_design_package 檢查）。
// 2. prototype/kairos/error-codes.js（由 copywriting.md §3.3、§4.2 產生）→ kairos/web/assets/js/core/error-codes.js：
//    只取 Kairos（KRS-）的代碼與三段式範例，轉成 ES module。
// 只使用 Node.js 內建模組，不需要 npm。

import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync, existsSync, mkdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const here = dirname(fileURLToPath(import.meta.url));
const web = resolve(here, '..');
const design = resolve(process.argv[2] || join(web, '..', '..', '..', 'design'));
if (!existsSync(join(design, 'design-system', 'tokens.css'))) {
  console.error(`找不到設計包：${design}/design-system/tokens.css`);
  process.exit(2);
}

// 1. tokens.css
const tokens = readFileSync(join(design, 'design-system', 'tokens.css'), 'utf8').replace(/\r\n/g, '\n');
writeFileSync(join(web, 'assets', 'tokens.css'), tokens);
const sha = createHash('sha256').update(tokens, 'utf8').digest('hex');
const version = (tokens.match(/版本：([^\n（]+)/) || [, '?'])[1].trim();
writeFileSync(
  join(web, 'tokens.css.sha256'),
  `${sha}  design/design-system/tokens.css（設計包 ${version}，${new Date().toISOString().slice(0, 10)} 複製；換行一律 LF）\n`,
);
console.log(`tokens.css：${sha}`);

// 2. error-codes.js（原型以一般 script 設定 window.KairosErrorCodes）
const src = readFileSync(join(design, 'prototype', 'kairos', 'error-codes.js'), 'utf8');
const sandbox = { window: {} };
vm.runInNewContext(src, sandbox);
const data = sandbox.window.KairosErrorCodes;
const out = {};
for (const c of data.codes) {
  if (c.product !== 'KRS') continue;
  const e = c.example || null;
  out[c.code] = {
    level: c.level,
    text: c.text,
    title: e ? e.title : null,
    reason: e ? e.reason : null,
    fix: e ? e.fix.trim() : null,
  };
}
mkdirSync(join(web, 'assets', 'js', 'core'), { recursive: true });
const header = `// 自動產生，請勿手動修改：node kairos/web/tools/sync-design.mjs
// 來源：設計包 prototype/kairos/error-codes.js（copywriting.md §3.3 錯誤代碼總表、§4.2 三段式範例；產生於 ${data.generated}）
// 只含 Kairos（KRS-）的 ${Object.keys(out).length} 個代碼；三段式為 title（發生什麼）、reason（原因）、fix（處理方式）
`;
writeFileSync(
  join(web, 'assets', 'js', 'core', 'error-codes.js'),
  `${header}export const ERROR_CODES = ${JSON.stringify(out, null, 1)};\n`,
);
console.log(`error-codes.js：${Object.keys(out).length} 個 KRS 代碼`);
