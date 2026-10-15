# Kairos 主控台網頁介面（`kairos/web/`）

0.7.0〔草稿〕起 Kairos 在主控台的監聽（`console.listen`，預設 `https://127.0.0.1:9443/`）上提供網頁介面（審計 F2）。本文件是給之後擴充告警（S06～S11）、註冊與 PKI（S12～S15）、管理（S16～S18）、系統（S20～S22）畫面的工程師的架構與慣例說明。設計以設計包（`design/`）為準：`kairos/screens.md`（畫面）、`kairos/ia.md`（導覽、權限 §4.2、狀態詞彙 §5）、`design-system/`（token、元件、文案）、`decisions/INTEGRATION.md` §28.6（沒有後端時隱藏或標示）。

## 1. 原則

| 原則 | 做法 |
|---|---|
| 發佈流程不引入 npm | 原生 ES modules＋vanilla JS，沒有建置步驟、沒有第三方套件；`kairos/build.rs` 在編譯時以 `include_bytes!` 把 `web/index.html` 與 `web/assets/**` 嵌入執行檔（`src/api/web.rs`）。清單依路徑排序、內容只取決於檔案本身，發佈包的可重現性不受影響。`web/.gitattributes` 讓這些檔案一律以 LF 取出（Windows 的 `core.autocrlf` 不影響嵌入的內容） |
| 外部字串一律是文字 | 所有 DOM 以 `core/dom.js` 的 `h()`／`s()` 建立，字串一律成為 Text 節點；**不使用 `innerHTML`、`insertAdjacentHTML`、`document.write`**。CSP 另以 Trusted Types 讓瀏覽器拒絕這些入口，誤用時在開發階段就會丟出例外 |
| 嚴格 CSP | `script-src 'self'`、`style-src 'self'`（不允許 inline script、`eval`、`style="…"` 屬性與 `<style>`）、`connect-src 'self'`、`frame-ancestors 'none'`、`require-trusted-types-for 'script'`。程式設定樣式只用 CSSOM（`el.style.setProperty`），`h()` 的 `style` 參數也是如此 |
| 不在瀏覽器保存秘密 | 工作階段只在 HttpOnly cookie（`__Host-kairos_session`）；CSRF token 只在記憶體（`core/api.js`），重新整理後由 `GET /api/v1/me` 取回。`localStorage` 只放 `kairos.*` 的顯示偏好（主題、時區…），以 try/catch 存取 |
| 權限只依 API | 選單、按鈕、分頁一律以 `/api/v1/me` 的 `permissions`（全站）與 `group_permissions`（群組範圍）決定；無權限的動作**隱藏**（screens.md §0.2）。伺服器仍會檢查（403），畫面只是不顯示 |
| 沒有後端的功能不顯示 | 依 `decisions/INTEGRATION.md` §28.6：不顯示停用中的假功能；只有部分可用時在畫面上說明 |
| 設計 token 唯一來源 | `assets/tokens.css` 是設計包 `design-system/tokens.css` 的逐位元組複本（第 6 節）；`assets/app.css` 只引用 `--ds-*` token |

## 2. 檔案

```
web/
├── index.html              外殼：tokens.css、app.css、theme-init.js、main.js；圖示 sprite（設計包原型的圖示）
├── assets/
│   ├── tokens.css          設計包的複本（不要手改）
│   ├── app.css             元件與版面（來源：設計包原型 app.css，移除原型專用部分）
│   ├── theme-init.js       第一次繪製前套用主題（一般 script，CSP 不允許 inline）
│   ├── favicon.svg
│   └── js/
│       ├── main.js         路由表、登入狀態、外框與畫面的掛載、閒置逾時
│       ├── core/
│       │   ├── dom.js      h()／s()／icon()／mount()：DOM 建構的唯一入口
│       │   ├── api.js      API 用戶端（CSRF、錯誤、401／403、連線狀態）
│       │   ├── session.js  /api/v1/me、can()／canAny()、權限改變的通知
│       │   ├── router.js   History API 路由、站內連結攔截、safeNext()
│       │   ├── sse.js      共用的 EventSource、重連、事件編號
│       │   ├── format.js   時間（顯示時區、伺服器時間校正、相對時間）、數字、容量
│       │   ├── prefs.js    只存在這個瀏覽器的偏好
│       │   ├── strings.js  所有介面文字（繁中）、狀態與角色標籤
│       │   ├── errors.js   錯誤 → 三段式（原因、處理方式、代碼）
│       │   └── error-codes.js  KRS- 代碼與三段式（由設計包產生，不要手改）
│       ├── ui/             共用元件：components（StatusBadge、KpiTile、Card、EmptyState、ErrorState、
│       │                   InlineAlert、欄位…）、table、dialog、toast、menu、chart、search、shell、nav、qr
│       └── screens/        畫面：login（S01）、overview（S03）、hosts（S04）、host-detail（S05）、
│                           account（S23）、errors（403／404）、host-common（主機的共用呈現）
│                           註冊與憑證：enroll-tokens（S12）、enroll-pending（S13）、pki-certs（S14）、pki-ca（S15）、
│                           pki-common（動作對話框 L1／L2、模式與簽發方式的提示、標籤）；文字在 core/strings-pki.js
├── tests/                  Playwright＋axe 的端對端測試（不嵌入、不在 cargo test 中；第 7 節）
└── tools/sync-design.mjs   從設計包複製 tokens.css、產生 error-codes.js（開發時執行）
```

`tests/`、`tools/`、本檔案不嵌入執行檔（只嵌入 `index.html` 與 `assets/`）。

## 3. 伺服器端（`kairos/src/api/web.rs`）

| 路徑 | 回應 |
|---|---|
| `/api/…` | API（`kairos/API.md`），不經過網頁介面 |
| `/healthz`、`/readyz`、`/metrics` | 健康檢查（`observability.on_console = false` 時為 JSON 的 404） |
| `/assets/<檔案>` | 嵌入的檔案；`ETag`（內容的 SHA-256）＋`Cache-Control: no-cache`（每次以 `If-None-Match` 確認，換版立即生效，不需要在檔名加雜湊）；不存在 404 |
| 最後一段含 `.` 的其他路徑 | 404（`/favicon.ico` 等不是頁面） |
| 其他路徑 | `index.html`（`Cache-Control: no-store`），前端依網址決定畫面；不存在的畫面由前端顯示 404 頁。第一段不在 `web.rs` 的 `SPA_ROOTS`（設計包 `ia.md` §3 的頂層路由）時狀態碼為 404（本文相同），新增頂層路由時要一併加入 |

只接受 `GET`／`HEAD`。主控台的**每個**回應（含 API）都有 `Content-Security-Policy`（HTML 為完整的政策，其他為 `default-src 'none'; frame-ancestors 'none'`）、`Strict-Transport-Security`（主控台只有 HTTPS）、`X-Content-Type-Options: nosniff`、`X-Frame-Options: DENY`、`Referrer-Policy: no-referrer`、`Cross-Origin-Opener-Policy`、`Cross-Origin-Resource-Policy`、`Permissions-Policy`。網頁介面不需要登入即可取得（登入頁），資料一律經由需要驗證的 API。

## 4. 前端架構

### 4.1 路由與畫面

- 路由表在 `main.js` 的 `ROUTES`：`{ path: '/hosts/:id/:tab?', load: () => import(...), title, perm | permAny | session | public }`。
- 流程：未登入 → `/login?next=<目前網址>`；必須先變更密碼 → 登入頁的變更密碼步驟；權限不足 → 外框內的 403 頁（網址不變）；不存在 → 404 頁。
- 畫面模組匯出 `render(ctx)`，回傳 `{ dispose() }`（可省略）。`ctx`：`root`（內容區）、`params`、`query`（`URLSearchParams`）、`app`（`logout()`、`notFound()`）、`setTitle(text)`（分頁標題「Kairos｜…」）、`focusTitle()`。
- 換頁時焦點移到頁面標題（`#page-title`，`tabindex="-1"`）；第一次繪製時不移動焦點（第一個 Tab 是「跳到主要內容」）。
- 清單的篩選、排序寫回網址（`router.replaceQuery`），可分享、可加書籤。

### 4.2 API 用戶端（`core/api.js`）

```js
const rows = await api(`/api/v1/hosts${qs({ status: ['offline'], sort: '-last_seen' })}`);   // 回傳 data
const res = await api('/api/v1/hosts', { raw: true });       // 整個回應（page、meta）
await api('/api/v1/me/password', { method: 'POST', body: { ... } });  // 自動帶 X-CSRF-Token
```

- 錯誤一律丟出 `ApiError`（`status`、`code`、`productCode`、`details`、`retryAfter`）。
- 401 `unauthorized`：全域處理，帶 `next` 與 `reason=session_expired` 回登入頁（設計包 D39）。
- 403 `password_change_required`：回登入頁的變更密碼步驟。403 `forbidden`：由畫面顯示（`errorState` 會寫出需要的權限與角色）。
- 連不上伺服器：`code = "network"`，頁首顯示「與 Kairos 伺服器的連線中斷…」橫幅（screens.md §0.2「資料過時」），資料時間改為警告樣式。
- 每個回應的 `server_time` 校正時鐘：相對時間以伺服器時間計算。

### 4.3 錯誤呈現（`core/errors.js`、`ui/components.js`）

- `errorState(err, { title, onRetry })`：區塊級錯誤，三段式（`copywriting.md` §4）。`product_code` 在 `error-codes.js` 有範例時用設計包的原因與處理方式，`title` 為呼叫端的情境標題（「無法載入主機清單」）。
- `errorAlert(err)`：表單送出失敗。欄位格式錯誤（`invalid_parameter` 的 `details.parameter`）顯示在欄位下，不附代碼。
- `short(err)`：Toast 用的一行。
- 新的機器代碼需要就地說明時，在 `strings.js` 加 `err.<code>.title`／`.reason`／`.fix`。

### 4.4 權限

```js
session.can('alerts:write')       // 全站範圍
session.canAny('hosts:read')      // 全站或任一群組範圍（主機的讀取）
session.scopedGroups()            // 只有群組範圍時的群組（「範圍：B廠產線」）
```

權限可能在使用中改變：回到分頁時、SSE 斷線重連時重新讀取 `/api/v1/me`；改變時外框重畫，目前的畫面若因此不允許會重新判斷（403）。

### 4.5 即時更新（`core/sse.js`）

- 整個主控台共用一條 `EventSource`（`console.max_streams` 預設 16）；有訂閱者時開啟。
- `sse.subscribe(fn)` 收到 `{ type, id, data }`；畫面在 `dispose()` 取消訂閱。
- 依 API.md 第 6 節的建議：列表記下 `meta.last_event_id`，只套用編號較新的事件（`sse.isNewer`）；重新抓取的列表可能比事件舊（索引最多 1 秒），主機清單保留最近的事件並在重新抓取後再套用一次。
- 伺服器拒絕或關閉串流時（401、503、權限改變）以退避重連，先確認 `/api/v1/me`。

### 4.6 表格（`ui/table.js`）

伺服器端篩選、排序、游標分頁；原生 `<table>`＋「載入更多（還有 N 筆）」。這是設計系統允許的退回做法（`design-system/README.md` §6.3），取代虛擬捲動與 `role="grid"`：原生表格的語意讓螢幕報讀器逐欄朗讀，鍵盤以 Tab 走過排序按鈕（`aria-sort`）與每列的主機名稱連結。即時更新時只更新該列（`updateRow`），不重排。

### 4.7 全域搜尋（`ui/search.js`）

Ctrl+K 或 `/`。以搜尋提供者擴充：

```js
registerProvider({
  id: 'alerts', group: () => t('search.group.alerts'), allowed: () => session.can('alerts:read'),
  async search(q, signal) { /* → [{ text, sub, href, badge }] */ },
});
```

### 4.8 文字與格式

- 所有介面文字在 `core/strings.js`（`t('hosts.title')`、具名參數 `{n}`），缺少 key 時在主控台記錯誤（端對端測試會失敗）。用語依 `copywriting.md` §2.2：主機、agent、註冊 token、撤銷、工作階段、兩步驟驗證（驗證碼、復原碼）…；狀態標籤不得改寫。
- 時間依 `copywriting.md` §5：顯示時區為個人偏好（預設 `Asia/Taipei`，不自動採用瀏覽器時區），表格欄位標題寫出 `UTC+8`；`fmt.timeEl()` 預設相對時間、`title` 為絕對時間與 UTC，每 30 秒更新一次。
- 數字依 §6：`fmt.num`（千分位）、`fmt.pct`（小數 1 位）、`fmt.bytes`（十進位 SI）、`fmt.duration`（最多兩個單位）。

### 4.9 主題與偏好

主題（跟隨系統／淺色／深色）與減少動態效果寫在 `<html data-theme|data-motion>`，`theme-init.js` 在第一次繪製前套用。這個版本沒有伺服器端的偏好儲存（§28.6），畫面上標示「只套用在這個瀏覽器」。

### 4.10 閒置逾時

總覽與清單會定期更新，伺服器把這些請求也當成活動；瀏覽器端另外追蹤使用者的操作，超過工作階段的閒置時間（`/api/v1/me` 的 `session.idle_timeout_secs`）沒有操作就登出並回登入頁（「工作階段已結束」）。

## 5. 新增畫面的步驟

1. `screens/<名稱>.js`：匯出 `render(ctx)`；以 `pageHead()` 開頭（`#page-title`），區塊用 `card()`；載入中用骨架（`kpiSkeleton`、`skeletonBlock`、`table.setLoading()`，**載入時不可顯示 0**），空狀態用 `emptyState()`（區分「真的沒有資料」與「篩選後沒有結果」），錯誤用 `errorState()`（區塊級，不拖垮整頁）。
2. `main.js` 的 `ROUTES` 加一項，標明 `perm`／`permAny`（依 `ia.md` §4.2 與 API.md 第 12.1 節的權限名稱）。
3. `ui/nav.js` 加側邊導覽項目（同樣的權限）。
4. 文字加到 `core/strings.js`。
5. 寫入操作：按鈕依權限隱藏（部分權限時以 `aria-disabled` 並說明原因）；L1 用 `confirm()`，L2／L3 用 `openDialog()` 加輸入確認（`screens.md` §0.3）；成功以 `toast()`。
6. `tests/e2e.mjs` 加流程測試與 `axe()`（淺色、深色）。

## 6. 設計 token 與錯誤代碼的同步

- `assets/tokens.css` 必須與設計包 `design-system/tokens.css` 相同。`cargo test -p kairos api::web::tests::tokens_css_matches_the_design_package` 檢查複本的 SHA-256 與 `web/tokens.css.sha256` 記錄的相同，並在找得到設計包時（環境變數 `KAIROS_DESIGN_DIR`，或 `kairos/` 往上各層的 `design/`）逐位元組比對（換行一律 LF）。
- 設計包更新後：`node kairos/web/tools/sync-design.mjs [設計包目錄]`（只用 Node.js 內建模組），會重新複製 `tokens.css`、更新 `tokens.css.sha256`、重新產生 `assets/js/core/error-codes.js`（設計包 `prototype/kairos/error-codes.js` 中 KRS- 的代碼與三段式）。

## 7. 測試

| 項目 | 方式 |
|---|---|
| 伺服器端（靜態檔、路由、安全標頭、tokens.css 一致、index.html 沒有 inline script／style） | `cargo test -p kairos api::web`、`api::tests::web_ui_is_served_with_security_headers` |
| 流程、CSP、XSS、RBAC、鍵盤、axe、截圖 | `bash kairos/web/tests/run.sh`（Linux；需要 Node.js、`playwright`、`axe-core`，選用 `jsqr`＋`pngjs` 解碼 QR code）。對真的 Kairos（暫存的資料庫、測試憑證、`kairos users create` 建立的帳號）執行，假 agent（`tests/fake-agent.mjs`）以 NDJSON 協定上線與回報 |

`run.sh` 的環境變數：`KAIROS_BIN`、`NODE`、`PW_DIR`（含 `node_modules/playwright`、`axe-core` 的目錄，預設 `~/pw`）、`OUT_DIR`（截圖與 `results.json`）、`ONLY`（只跑名稱含這個字串的測試）、`KEEP=1`（保留暫存目錄）。目前的測試：

1. 安全標頭與 CSP：inline script、行內事件處理、`innerHTML` 都被擋下（CSP／Trusted Types）。
2. 登入 → 總覽（KPI；沒有判定的狀態顯示「—」）→ 主機清單（狀態 chip、排序、CIDR 搜尋與錯誤的 CIDR、主機名稱搜尋）→ 主機詳情（概覽、趨勢、連線與憑證、不存在的主機）→ 登出。
3. 不在 localStorage／sessionStorage 放秘密；工作階段 cookie 為 HttpOnly、Secure、SameSite=Strict。
4. XSS：主機名稱為 `<script>…</script><img onerror>` 的主機在清單、詳情、全域搜尋都以文字顯示。
5. 即時更新：主機離線後清單的狀態欄就地更新、「有 N 台狀態變化 · 更新列表」、連線紀錄。
6. 鍵盤：跳到主要內容、Ctrl+K 搜尋與 ↓ Enter、使用者選單（Enter、Esc 回到按鈕）。
7. 兩步驟驗證：啟用（QR code 可解碼為 otpauth URI、驗證碼、10 組復原碼、未勾選「我已保存」不能關閉）、錯誤的驗證碼（`KRS-E6012`）、以復原碼登入。
8. 第一次登入必須變更密碼（密碼政策錯誤顯示在欄位下）。
9. 權限：沒有角色的帳號看不到監控選單、直接存取 403、沒有系統狀態燈；唯讀看得到總覽與主機；404 頁。
10. 工作階段被其他裝置結束：下一個請求帶 next 回登入頁並說明。
11. 深色主題、窄螢幕（375px 無水平溢出、漢堡選單）、個人設定切換主題。
12. 帳號密碼錯誤（`KRS-E6001`）與鎖定（`KRS-E6003`）。
13. 註冊與憑證（`e2e-pki.mjs`，第二個 Kairos：`pki-env.sh` 以 openssl 建立測試 PKI，mTLS、註冊端點、線上簽發）：建立 token（只顯示一次、清單只見前 4 碼、不進 storage／網址／日誌）。
14. 以實際的 `pulse enroll --install-string` 送出註冊申請 → 待核准 → 核准 → `pulse` 取得憑證 → 憑證清單。
15. 測試用 HTTPS 用戶端送出主機名稱含 `<script>` 的申請（XSS）→ 以 L2（輸入主機名稱、原因必填）拒絕 → 最近結束。
16. 撤銷憑證（L2 確認對話框）→ 詳情（`/certificates/:serial`）→ CRL 新版本、立即更新 CRL。
17. 權限：唯讀看不到「註冊與憑證」且直接存取 403；稽核員沒有寫入按鈕、直接呼叫寫入端點 403；維運者的自動核准選項停用。
18. 深色主題的 axe 與 375px 無水平溢出。

每個主要畫面在淺色與深色主題各跑一次 axe（目標 0 違規），截圖在 `OUT_DIR`。

## 8. 0.7.0 的範圍與隱藏的元素

| 畫面 | 有 | 隱藏或標示（原因） |
|---|---|---|
| S01 登入 | 本機／目錄服務（依 `/api/v1/auth/methods`）、兩步驟驗證（驗證碼或復原碼、逾時回第一步）、必須先變更密碼、鎖定剩餘時間、依來源限速、工作階段已結束的說明、公開健康燈（`/healthz`，關閉時隱藏）、`next` 只接受站內路徑 | 站台名稱（沒有系統設定 API）；使用條款、離線說明文件連結 |
| S03 總覽 | 六個狀態 KpiTile（嚴重、警告、待核准的計數 API 為 `null`，顯示「—」）、1 小時變化、24 小時上線數 Sparkline、系統健康條、進行中事件（有 `alerts:read` 時；唯讀）、群組健康（有群組時）、離線最久、資源使用前 10 | 範圍選擇器 `?group=`、kiosk 模式、補充指標的憑證磚（S14 之後）、最近變更（S18 之後）、事件列的行內「確認」（S06／S07 之後） |
| S04 主機清單 | 搜尋（名稱、agent_id、IP、CIDR）、狀態 chip（只列有判定的狀態）、離線原因／群組／標籤／OS／agent 版本（`/hosts/facets`）、伺服器端排序、載入更多、即時更新 | 勾選與批次操作、匯出 CSV、新增主機、已儲存的檢視、欄位設定、憑證與生命週期篩選（沒有後端） |
| S05 主機詳情 | 概覽（狀態說明、建議下一步、目前數值、最近 1 小時、24 小時時間軸、時鐘偏差 `KRS-W8302`、從未連線）、趨勢（1 小時～30 天、平均＋最小／最大、空缺不補 0、表格檢視）、連線與憑證（連線資訊、憑證有效狀態；有 `pki:read` 時另有序號與指紋、被拒的憑證、這次啟動後的上線與離線紀錄） | 資產資訊（F4）、事件、稽核紀錄分頁；編輯顯示名稱、進入維護、除役、撤銷、刪除資料（主機的寫入操作） |
| S23 個人設定 | 個人資料與角色、主題、減少動態效果、時間顯示、時區（只在這個瀏覽器）、變更密碼、兩步驟驗證（啟用、停用）、我的工作階段（逐一結束、結束其他所有工作階段 L1） | 我的通知（§28.6）、我的 API token（一行說明）、語言、預設首頁、快速鍵開關 |
| S12 註冊 token | 清單（有效／已過期／已用完／已停用／全部）、部署進度（已上線／已使用）、建立（核准方式、限定來源網段、批次部署模式、限定主機、簽發憑證有效期）、建立完成只顯示一次（SecretField 預設遮蔽、CA 指紋、安裝字串、`pulse enroll --install-string` 指令）、停用（L1）、詳情的部署進度漏斗、註冊端點模式與簽發方式提示 | Linux 一行指令、MSI 下載與「下載指令檔」（下載端點未提供，INTEGRATION §27.4 #3）；「時鐘偏差暫不受理」與「已簽發未上線」的主機清單（只有計數）；「改為開放」（S21）；稽核紀錄連結（S18） |
| S13 待核准 | 待核准／最近結束、token／類型／異常旗標（無異常、有異常）篩選與搜尋、全選只選無異常、批次核准（L1）、拒絕（L2：單筆輸入主機名稱、批次「拒絕 N 筆」，原因必填）、結果訊息、申請詳情（申請編號大字、公鑰指紋、旗標說明）、已核准待簽發、撤回與他人處理時取消勾選、每 10 秒確認清單變化 | 授權用量（S22）、OS 與來源網段篩選（API 沒有）、排隊中的主機數與 ProgressBar、佇列已滿提示、側邊導覽的待核准計數 |
| S14 憑證 | 狀態 KpiTile（含 CRL 剩餘、寬限期內、需重新註冊）、狀態與到期篩選、搜尋（序號、主機名稱、agent_id）、撤銷（L2，單筆輸入主機名稱、批次「撤銷 N 台」）、詳情（`/certificates/:serial`、下載 PEM）、寬限期內與需重新註冊的說明、允許再次自動續簽（L1，限管理員；維運者停用並說明）、建立限定這些主機的 token、待簽發（匯出 CSR 佇列、匯入簽發結果） | 匯出憑證清冊、手動簽發 CSR、重新簽發（輪替）、除役、憑證政策的修改（寬限期縮短的影響數字沒有 API，頁首改以唯讀的寬限天數呈現）、群組與簽發方式篩選、「最後嘗試」欄、以 PEM 目錄匯入 |
| S15 CA 與撤銷清單 | 每個簽發 CA 的 CRL（版本、發布、nextUpdate、撤銷筆數、來源；過期 `KRS-E1203`、7 天內 `KRS-W1201`）、立即更新 CRL（L1，線上簽發時）、信任錨與簽發 CA（狀態、有效期、私鑰位置、簽發張數、`KRS-W1304`）、簽發方式、`KRS-E1303`、CRL 版本歷史 | 上傳 CRL、下載目前的 CRL、設定自動更新、CA 輪替精靈、改為離線／線上簽發、更換伺服器憑證（API 沒有；簽發 CA 的管理以 CLI，「技術細節」列出指令）；離線簽發的「匯出撤銷清單」 |
| 全域 | 頂列（全域搜尋、系統狀態燈、使用者選單）、側邊導覽（依權限，可收合；窄螢幕為漢堡選單）、連線中斷橫幅、403／404 頁、Toast | 租戶切換器、說明（?）、全域橫幅（CRL 到期等，S20 之後）、分頁標題的未確認數 |
