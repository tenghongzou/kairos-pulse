# Kairos 主控台 API（v1）

本文件是 Kairos 主控台與外部整合使用的查詢與管理 API 的規格（商業審計 F1；設計包 `kairos/screens.md` §24 #1、#3、#4、#5、#19 的後端部分）。0.4.0〔草稿〕起提供第一版：**唯讀**的主機清冊、單台詳情、指標趨勢、總覽 KPI、系統健康與即時變更推送（SSE），以 API token 驗證。0.6.0〔草稿〕起另有使用者、角色（RBAC）、主控台的工作階段與稽核雜湊鏈（設計包 #10、#11、D25；審計 F6），見第 12 節。

- 設定見第 2 節，驗證見第 4 節，端點一覽見第 5 節；0.5.0 的註冊與 PKI 管理端點見第 11 節，0.6.0〔草稿〕的使用者、角色與稽核見第 12 節，告警（事件、規則、靜音、通知管道）見第 13 節。
- 實作：`kairos/src/api/`（HTTP 伺服器、驗證、各端點）、`kairos/src/token_cli.rs`（`kairos api-token`）。
- 量測（1 萬台主機的列表、30 天趨勢、與 report 寫入同時進行時的 `ack` 延遲）見第 9 節：列表 p99 約 10 ms、30 天趨勢 p99 約 8～13 ms（目標 200／300 ms）。

## 1. 總則

| 項目 | 規則 |
|---|---|
| 傳輸 | 只有 HTTPS（TLS 1.2／1.3，rustls＋ring，與 agent 連線埠相同的加密後端），HTTP/1.1。不提供明文 HTTP |
| 連接埠 | `console.listen`，預設 `127.0.0.1:9443`（設計包 `ia.md` §3.2：初始化期間只綁 loopback；首次啟動精靈完成後建議改為 `0.0.0.0:9443`，本版沒有精靈，由管理者手動修改）。與 agent 連線埠（`server.listen`）分開，防火牆只對管理網段開放 |
| 版本 | 路徑前綴 `/api/v1`。同一個大版本內只新增欄位與端點，不刪除、不改型別；讀取端應忽略不認得的欄位 |
| 格式 | 請求與回應都是 JSON（UTF-8），`Content-Type: application/json; charset=utf-8`；SSE 為 `text/event-stream` |
| 時間 | RFC 3339、**UTC**、`Z` 結尾、毫秒，例：`2026-10-13T06:03:27.123Z`（與 `hello_ack` 的 `server_time` 相同，設計包 §24 #23）。持續時間以數字表示並在欄位名稱帶單位（`_ms`、`_secs`），容量為 `_bytes` |
| 伺服器時間 | **每個回應**（含錯誤）的最外層都有 `server_time`：Kairos 產生回應時的系統時間，讓主控台計算相對時間（「3 分鐘前」）時不依賴瀏覽器的時鐘 |
| 方法 | 查詢端點只有 `GET`（`HEAD` 視同 `GET` 但不回本文），其他方法回 405（`Allow: GET, HEAD`）。0.5.0 起註冊與 PKI 的管理端點另有 `POST`、`PUT`（第 11 節） |
| 快取 | 所有回應 `Cache-Control: no-store` |
| CORS | **預設關閉**：不回任何 `Access-Control-*` 標頭，瀏覽器的跨來源請求一律失敗。網頁介面與 API 同源提供（0.7.0〔草稿〕，第 14 節），不需要 CORS |
| 字元 | 主機名稱等由 agent 提供的字串原樣輸出（JSON 跳脫），顯示時由主控台處理（Pulse 已把不能顯示的字元換成 `?`） |

### 1.1 成功回應

```json
{
  "server_time": "2026-10-13T06:03:27.123Z",
  "data": { }
}
```

列表另有 `page`（第 3 節）與 `meta`。

### 1.2 錯誤回應

```json
{
  "server_time": "2026-10-13T06:03:27.123Z",
  "error": {
    "code": "invalid_parameter",
    "product_code": null,
    "message": "status 的值 \"down\" 不是主機狀態 token（critical、offline、warning、unknown、pending、online）",
    "details": { "parameter": "status" }
  }
}
```

- `code`：snake_case 的機器代碼，**程式只依它判斷**（與 8444 註冊端點的 `{"error":{"code","message"}}` 同一種寫法，設計包 `decisions/enroll-rate-limit.md` §5.1）。
- `product_code`：對應的產品錯誤代碼（`copywriting.md` §3.3），主控台據此顯示三段式說明；沒有對應的代碼時為 `null`。
- `message`：給開發者看的說明（繁體中文），不保證固定，不要解析。
- `details`：選用，依代碼而定。

| HTTP | `code` | `product_code` | 何時 |
|---|---|---|---|
| 400 | `invalid_parameter` | — | 查詢參數不合法（`details.parameter` 寫出是哪一個） |
| 400 | `invalid_cursor` | — | `cursor` 無法解析、或與目前的排序不符（改變排序後要從第一頁開始） |
| 401 | `unauthorized` | `KRS-E6010` | 沒有 `Authorization`（或工作階段）、格式錯誤、token 不存在、已撤銷或已過期（對外不區分），附 `WWW-Authenticate: Bearer realm="kairos"`；工作階段已結束或逾時時 `details.reason = "session_expired"`（第 12.3 節） |
| 401 | `invalid_credentials` | `KRS-E6001` | 登入的帳號或密碼不正確（不區分哪一個；第 12.2 節） |
| 401 | `invalid_mfa_code` | `KRS-E6012` | TOTP 或復原碼不正確或已使用過 |
| 401 | `mfa_challenge_expired` | — | 登入第二步的挑戰逾時或無效，重新輸入帳號密碼 |
| 403 | `forbidden` | `KRS-E6104` | 權限不足：`details.permission` 為所需的權限、`details.required` 為擁有它的最低角色（0.4／0.5 的 token 為範圍；第 11.1、12.1 節） |
| 403 | `account_locked` | `KRS-E6003` | 連續登入失敗而鎖定，附 `Retry-After` 與 `details.locked_until` |
| 403 | `account_disabled` | `KRS-E6009` | 目錄服務回報帳號已停用或鎖定 |
| 403 | `csrf_failed` | `KRS-E6105` | 以工作階段送出的寫入請求缺少或帶錯 `X-CSRF-Token`，或來自其他網站（第 12.3 節） |
| 403 | `password_change_required` | `KRS-E6013` | 第一次登入必須先變更密碼 |
| 409 | `last_admin`、`cannot_target_self`、`target_is_self`、`user_exists`、`directory_account` | — | 使用者管理的防呆（第 12.5 節） |
| 503 | `directory_unavailable` | `KRS-E6002`／`E6005`／`E6006`／`E6007` | 無法以目錄服務登入（`details.reason`） |
| 404 | `not_found` | — | 沒有這個路徑 |
| 404 | `host_not_found` | — | 清冊中沒有這個 `agent_id`（設計包 S05「主機不存在 → 404 頁」） |
| 404 | `metric_not_found` | — | 這台主機在查詢範圍內沒有這個指標 |
| 405 | `method_not_allowed` | — | 不是 `GET`／`HEAD` |
| 413 | `payload_too_large` | — | 請求帶有本文（本版的端點都不接受本文） |
| 429 | `rate_limited` | `KRS-W6011` | 同一來源驗證失敗太多次（第 4.3 節），附 `Retry-After`（秒）。第一次限制這個來源時 Kairos 記一則 `WARN`（`警告 KRS-W6011：…`） |
| 503 | `too_many_streams` | — | SSE 的同時連線數已達 `console.max_streams`，附 `Retry-After` |
| 503 | `storage_unavailable` | `KRS-E4003` | 資料庫查詢失敗（例如磁碟 I/O 錯誤、資料庫被刪除）；主控台顯示「無法載入○○」 |
| 503 | `shutting_down` | — | Kairos 正在關機 |
| 500 | `internal` | `KRS-E4003` | 未預期的錯誤（日誌中有細節；回應不含內部資訊） |

產品代碼依設計包 `copywriting.md` §3.4「主控台 API」（`decisions/INTEGRATION.md` §26）：驗證失敗與限速屬於子類別 60（登入與驗證，`KRS-E6010`、`KRS-W6011`），權限不足屬於 61（`KRS-E6104`）；登入畫面的帳號密碼錯誤是另一個代碼 `KRS-E6001`。表中「—」的代碼為用法錯誤或暫時狀態，主控台就地處理，`product_code` 為 `null`。

## 2. 設定（`[console]`）

```toml
[console]
enabled = true              # false：不監聽主控台連接埠
listen = "127.0.0.1:9443"   # 初始化期間只綁 loopback；開放給管理網段時改成 0.0.0.0:9443 並以防火牆限制來源
cert_path = ""              # 空字串：沿用 server.cert_path（同一張伺服器憑證）
key_path = ""               # 空字串：沿用 server.key_path；兩者必須同時設定或同時留空
max_connections = 64        # 主控台的同時連線上限（含 SSE），1..=4096
max_streams = 16            # SSE（/api/v1/events）的同時連線上限，1..=max_connections
```

- **連接埠不可重複**：`server.listen` 與 `console.listen` 的 `IP:port` 相同（或其中一個是 `0.0.0.0`／`::` 而連接埠相同）時拒絕啟動，結束代碼 3，`KRS-E0202`「設定互相矛盾」（設計包 §24 #16 (a)；`enroll.listen` 實作註冊端點時加入同一個檢查）。連接埠被其他程式佔用時與 agent 連線埠相同，結束代碼 11（`KRS-E2001`）。
- **憑證**：預設與 agent 連線埠共用 `server.cert_path`／`key_path`（主控台只做伺服器端 TLS，不要求用戶端憑證）。要讓瀏覽器信任時，建議另以 `console.cert_path`／`key_path` 設定由組織 CA 簽發、SAN 為主控台網址的憑證。兩種情況都依 `server.reload_interval_secs` 熱重載（換憑證不需重新啟動）。
- **儲存後端**：主控台需要 SQLite（`storage.backend = "sqlite"`，預設）；`backend = "jsonl"` 時不啟動主控台並記 `WARN`（API token 存在 SQLite 中）。
- **檔案描述元**：主控台的連線計入啟動時的 `LimitNOFILE` 檢查（設計包 §24 #18 (c)、D21）：需要量 ＝ `server.max_connections` ＋ `console.max_connections` ＋ 預留 64，建議值 ＝ max(4096, 不小於需要量的 2 的冪次)。預設 1,024 ＋ 64 ＋ 64 ＝ 1,152，發佈包 unit 的 `LimitNOFILE=8192` 足夠。系統健康（第 5.6 節）回報同一個算式的結果。
- 只改 `[console]` 時需要重新啟動 Kairos（憑證檔的內容除外）。
- 啟動日誌：`主控台 API 已啟動，監聽 https://127.0.0.1:9443/api/v1/（連線上限 64，SSE 上限 16，憑證 …）`。
- 主控台在自己的 tokio runtime（2 個工作執行緒、最多 8 個 blocking 執行緒）上執行，與 agent 連線、report 的 writer 分開排程（第 9 節）。

## 3. 列表與游標分頁

```
GET /api/v1/hosts?status=offline&sort=-last_seen&limit=100
GET /api/v1/hosts?status=offline&sort=-last_seen&limit=100&cursor=eyJ2IjoxLC...
```

```json
{
  "server_time": "…",
  "data": [ { …一台主機… } ],
  "page": { "limit": 100, "total": 312, "next_cursor": "eyJ2IjoxLC…" },
  "meta": { … }
}
```

- `limit`：1～1000，預設 100（設計包 S04「每次向伺服器取 100 筆」）。
- `page.total`：符合篩選條件的總數（不受 `limit` 與 `cursor` 影響），即「顯示 10 / 1,200 台」的分母。
- `page.next_cursor`：下一頁的游標；最後一頁為 `null`。游標是不透明的字串（不要解析或自行組合），帶有排序方式與上一頁最後一列的排序鍵（keyset pagination）：翻頁期間有主機新增或狀態改變時，不會重複或跳過其他主機（狀態改變的那一台可能出現在前一頁或下一頁）。
- 改變篩選條件時沿用游標仍可運作（以排序鍵定位）；改變**排序**時回 400 `invalid_cursor`，請從第一頁開始。
- 不提供頁碼（設計包 `ia.md` §3.2：2 萬台時跳頁不一致）。

## 4. 驗證：API token

**API token** 給整合（Grafana、腳本）與自動化使用；主控台的網頁介面以帳號登入、使用工作階段的 cookie（0.6.0〔草稿〕，第 12.2、12.3 節）。兩者得到同樣的身分（`Principal`）與權限（第 12.1 節）。

### 4.1 token

```
Authorization: Bearer kat_3f9c2a71b4e05d18_Qm9yZW1hbi1pcy1ub3QtYS1yZWFsLXRva2VuLWV4YW1wbGU
```

- 格式：`kat_` ＋ 16 個十六進位字元的 **token_id**（公開的識別碼，用於列表、撤銷與日誌）＋ `_` ＋ 43 個 base64url 字元的**秘密**（32 bytes 隨機值，以作業系統的 CSPRNG 產生）。`kat_` 前綴讓 secret 掃描工具容易辨識。
- 伺服器只保存秘密的 **SHA-256 雜湊**（`api_tokens.secret_hash`）。秘密有 256 位元的亂度，不需要慢速雜湊（bcrypt／Argon2 是為了低亂度的密碼）。比對時先以 token_id 查詢，再以**常數時間**比較雜湊；token_id 不存在時仍做一次同樣的比較，回應時間不洩漏 token_id 是否存在。
- token 只在建立時顯示一次，之後無法取回；遺失時撤銷後重新建立。
- **權限**：0.6 起 token 屬於某個**角色**（`--role`，預設 `viewer`）或某個**使用者**（`--user`，權限＝使用者目前的權限）；0.4／0.5 建立的 token 保留原本的範圍 `read`／`operate`／`admin`，權限凍結在升級前（第 12.1 節）。權限不足的請求回 403。
- **到期**：建立時指定，預設 90 天、上限 3,650 天；不提供不過期的 token（設計包 `cli-style.md` §11.6：外洩後只能靠人工撤銷）。需要長期使用的整合以 `--expires-in 3650d` 建立，`kairos api-token list` 出現 14 天內到期的提示時輪替。到期後回 401（`KRS-E6010`）。
- **最後使用**：成功驗證時在記憶體中記下時間與來源 IP，每 60 秒以一個短交易寫回資料庫（`last_used_at`、`last_used_ip`；`kairos api-token list` 顯示）。
- 回應、日誌與 `kairos api-token list` 都不會出現秘密或雜湊；日誌只記 token_id 與名稱。

### 4.2 `kairos api-token`

```
kairos api-token create --name <名稱> [--role <角色> | --user <帳號> [--role <角色>]] [--expires-in <時間> | --expires-at <日期或時間>] [--json] [--config <檔案>]
kairos api-token list [--all] [--json] [--config <檔案>]
kairos api-token revoke <token_id> [--reason <原因>] [--yes] [--json] [--config <檔案>]
```

輸出依設計包 `design-system/cli-style.md` §11.6（結果在 stdout、過程與錯誤在 stderr、`--json` 為純 ASCII 的單一文件）。給人看的時間用**本機時區**並標示時區（`copywriting.md` §5.1），`--json` 一律 UTC：

```console
$ sudo -u kairos /opt/kairos/kairos api-token create --name grafana --expires-in 180d --config /opt/kairos/kairos.toml
kat_3f9c2a71b4e05d18_Qm9yZW1hbi1pcy1ub3QtYS1yZWFsLXRva2VuLWV4YW1wbGU
✓ 已建立 API token「grafana」（token_id 3f9c2a71b4e05d18，範圍 read，到期 2027-04-11 14:03 UTC+8）
  這是唯一一次顯示 token，請立即保存到密碼管理工具；遺失時撤銷後重新建立。
  使用方式：Authorization: Bearer <token>（https://<主機>:9443/api/v1/）

$ sudo -u kairos /opt/kairos/kairos api-token list --config /opt/kairos/kairos.toml
TOKEN_ID          名稱     範圍  狀態  建立（UTC+8）           到期（UTC+8）       最後使用
3f9c2a71b4e05d18  grafana  read  有效  2026-10-13 14:03 ops    2027-04-11 14:03    3 分鐘前（10.20.5.31）
a07e1c5590d2b3f4  report   read  有效  2026-07-20 09:12 ops    2026-10-18 09:12 ⚠  2 天前（10.20.5.40）

共 2 個（有效 2）
⚠ 1 個 API token 在 14 天內到期：到期前以 kairos api-token create 建立新的 token、更新整合端，再撤銷舊的。
```

- stdout 只有 token 一行（`TOKEN=$(kairos api-token create …)`），其他說明寫 stderr。
- `--expires-in`：`90m`、`12h`、`30d`、`2w`（與 `kairos hosts list --offline-for` 相同的寫法），上限 3650 天；`--expires-at`：`2027-01-01`（UTC 當天 00:00）或 RFC 3339。`--no-expiry`（0.4 草稿）已移除：指定時以結束代碼 2 拒絕並提示 `--expires-in 3650d`。
- `--name`：1～64 個字元，不可含控制字元；不必唯一（以 token_id 區分）。
- `list`：預設只列出有效的 token；`--all` 另列已撤銷與已過期的。欄位：token_id、名稱、範圍、狀態（有效／已撤銷／已過期）、建立時間與建立者、到期、最後使用（7 天內為相對時間，7 天以上改為日期時間，copywriting.md §5.3；附 IP）。時間為本機時區，只在欄位標題標示；剩餘 14 天以內的有效 token 在到期時間後加 `⚠`（`--ascii` 或非 UTF-8 終端機為 `[WARN]`），並在 stderr 提示輪替。
- `revoke`：危險操作 L1（`cli-style.md` §5.4）：互動模式詢問 `是否繼續？[y/N]`，非互動需 `--yes`（否則結束代碼 2）。撤銷立即生效（每個請求都查詢資料庫）。重複撤銷已撤銷的 token 不算錯誤。
- 結束代碼：0 完成、1 資料庫錯誤、2 用法錯誤、3 設定錯誤、10 權限不足（以 root 執行而資料目錄屬於服務帳號時拒絕，與 `kairos reports import` 相同）、11 資料庫被 `kairos reports import` 獨占中。
- 以服務帳號執行（`sudo -u kairos`），Kairos 執行中也可以（只寫一個短交易）；資料庫不存在時建立（並升級 schema）。

**稽核**：建立與撤銷都寫入 `api_tokens`（建立者與撤銷者為執行命令的作業系統帳號：`SUDO_USER`、`USER`、`LOGNAME` 依序取第一個有值的，另記 uid；撤銷原因），並在同一個交易中寫入稽核雜湊鏈（0.6，第 12.8 節）；執行中的 Kairos 另在 **10 秒內**寫入 Kairos 的日誌（`INFO`「API token 已建立／已撤銷：token_id …、名稱 …、範圍 …、到期 …、建立者 …」；Kairos 未執行時在下次啟動時寫入）。

### 4.3 失敗的速率限制

- 同一來源（IPv4 位址；IPv6 以 /64 合併，與 agent 連線埠的新連線速率相同）**驗證失敗**（含登入失敗，第 12.2 節）時扣額度。0.7.0 起**沒有出示任何憑據**（沒有 `Authorization`、也沒有工作階段 cookie，例如未登入的瀏覽器開啟主控台時讀取 `/api/v1/me`）回 401 但**不扣額度**：沒有猜測任何秘密，否則同一個 NAT 後的使用者重新整理登入頁幾次就會被限速；出示了錯誤或失效的 token、工作階段照常扣。額度：可連續 10 次，之後每 6 秒恢復 1 次（每分鐘 10 次）。額度用完時該來源的**所有請求**回 429 `rate_limited`（`Retry-After` 為恢復 1 次所需的秒數），不再檢查 token，直到額度恢復。成功的請求不扣額度。
- 每個來源第一次被限制時記一行 `WARN`（`警告 KRS-W6011：主控台 API：來自 … 的請求驗證失敗太多次…`；同一來源恢復額度前不再記），不記錄 token 內容。
- 記憶體中最多追蹤 10,000 個來源，超過時清掉最久沒有失敗的來源。

### 4.4 工作階段與 RBAC（0.6.0〔草稿〕，已實作）

- 每個請求先經過 `api::auth::Authenticator::authenticate`（`Authorization` 優先，其次是工作階段的 cookie），得到 `Principal`（`ApiToken` 或 `Session`）與它的權限（`Grants`：全站的一組，加上每個主機群組的一組）。
- 端點以 `principal.require(Permission::…)` 檢查權限（集中定義在 `kairos/src/rbac.rs`）；主機的讀取另以 `principal.host_access()` 篩選群組範圍（列表、詳情、趨勢、總覽、SSE）。
- `GET /api/v1/me` 回傳目前的身分與 `permissions`，主控台以它決定顯示哪些功能（權限差異）。
- 詳見第 12 節。

## 5. 端點

| 方法與路徑 | 用途（設計包畫面） | 說明 |
|---|---|---|
| `GET /api/v1/me` | 全部 | 目前的身分與權限（第 5.0、12.4 節） |
| `/api/v1/auth/…`、`/api/v1/me/…`、`/api/v1/users…`、`/api/v1/roles`、`/api/v1/directory…`、`/api/v1/api-tokens…`、`/api/v1/audit…` | S01、S16、S17、S18、S21、S23 | 登入、工作階段、使用者與角色、目錄服務、API token、稽核日誌（第 12 節） |
| `GET /api/v1/hosts` | S04 主機清單、全域搜尋、S03 連結 | 清冊列表：篩選、排序、游標分頁、總數（第 5.1 節） |
| `GET /api/v1/hosts/{agent_id}` | S05 主機詳情 | 單台詳情（第 5.2 節） |
| `GET /api/v1/hosts/{agent_id}/metrics` | S05 趨勢分頁 | 這台主機目前回報的指標（名稱、單位、label 組合） |
| `GET /api/v1/hosts/facets` | S04 篩選列、S03 群組健康 | 範圍內主機的 OS、agent 版本、標籤、群組、離線原因與各自的台數（0.7.0，第 5.7 節） |
| `GET /api/v1/hosts/{agent_id}/connections` | S05「連線與憑證」 | 這台主機在 Kairos 這次啟動後的上線與離線紀錄（記憶體中；0.7.0，第 5.8 節） |
| `GET /api/v1/hosts/{agent_id}/metrics/{name}` | S05 趨勢、S03 sparkline | 指標趨勢（第 5.3 節） |
| `GET /api/v1/overview` | S03 總覽 | 各狀態台數、上線數 24 小時、離線最久、資源使用前 10（第 5.4 節） |
| `GET /api/v1/system/health` | S20 系統健康、S03 健康條 | 檢查項目與數值（第 5.5 節） |
| `GET /api/v1/events` | S04「有 12 台狀態變化」、S03 自動更新、S06 | 即時變更推送（SSE，第 6 節；告警的 `alert.*` 見第 13.6 節） |
| `/api/v1/alerts…`、`/alert-rules…`、`/silences…`、`/channels…` | S06～S11 | 告警（第 13 節） |

### 5.0 `GET /api/v1/me`

```json
{ "server_time": "…", "data": {
  "type": "api_token", "token_id": "3f9c2a71b4e05d18", "name": "grafana",
  "scopes": [], "role": "viewer", "owner": null, "expires_at": "2027-04-11T06:03:27.000Z",
  "permissions": ["hosts:read", "health:read"], "group_permissions": {}
} }
```

`scopes` 只在 0.4／0.5 建立的 token 有值（`["read"]` 等）。工作階段的回應見第 12.4 節。

### 5.1 `GET /api/v1/hosts`

**主機狀態**依設計包 `ia.md` §5.1 的 token。本版能判定的是 `online`、`offline`、`unknown`；`critical`、`warning`（需要告警，F3）與 `pending`（需要註冊流程，F5）尚無資料，篩選時不會符合任何主機，計數為 `null`（不是 0，設計包 S03「無法取得時不可顯示 0」）。

| 狀態 | 判定 |
|---|---|
| `online` | 目前有連線、通過 hello，且 `last_seen` 在離線門檻（`offline_after_ms`）內 |
| `offline` | 曾經上線，現在沒有連線或超過離線門檻 |
| `unknown` | 清冊中有、但從未連線到這個 Kairos（由 `kairos reports import` 匯入的主機，`status_detail.never_connected: true`）；或在**狀態暖機期**（Kairos 啟動後 2 分鐘）內，最後一次訊息在這次啟動之前、這次啟動後還沒連上的主機（`status_detail.starting: true`；設計包 `ia.md` §5.1）。2 分鐘後仍未連上改為 `offline`；這次啟動後已連上又中斷的主機照常為 `offline`。狀態暖機期只影響狀態顯示，與告警的暖機期無關 |

**查詢參數**（同一參數多個值以逗號分隔，取「或」；不同參數取「且」，與設計包 D23 的篩選規則相同）：

| 參數 | 例 | 說明 |
|---|---|---|
| `status` | `offline,unknown` | 主機狀態 token（六種之一，否則 400） |
| `never_connected` | `true` | 從未連線（`unknown` 的一種） |
| `offline_for` | `3d` | 失聯（不是 `online`）且 `last_seen` 至少在這麼久之前（`90m`、`12h`、`3d`、`2w`；設計包「失聯超過 N」、`kairos hosts list --offline-for`） |
| `offline_reason` | `certificate_rejected` | 最後中斷原因（`idle_timeout`、`agent_closed`、`server_shutdown`、`certificate_rejected`、`write_unknown`；第 5.2 節）。0.7 的過渡期另接受舊名稱 `timeout`（＝`idle_timeout`）、`connection_closed`（＝`agent_closed`），0.8 移除 |
| `tag` | `env:prod,line3` | 標籤（完全相同，不分大小寫） |
| `os` | `Ubuntu,Windows` | `os.name`（完全相同，不分大小寫） |
| `version` | `0.3.0,0.2.0` | `agent_version`（完全相同） |
| `durable` | `true` | 最近一次連線是否協商了補送（`durable`）；Kairos 重新啟動後尚未連線的主機不知道，兩種值都不符合 |
| `subnet` | `10.30.3.0/24,2001:db8::/48` | 最後已知 IP 在網段內（CIDR；只寫位址表示單一位址；主機位元必須為 0，否則 400 並建議正確的寫法） |
| `q` | `hmi-line3`、`6f1c0e4a`、`10.30.3.27`、`10.30.3.0/24` | 搜尋（設計包 S04）：是 IP 時比對最後已知 IP；是 CIDR 時同 `subnet`；其他比對主機名稱與顯示名稱（片段、不分大小寫）與 `agent_id` 前綴（4 個字元以上） |
| `sort` | `-last_seen` | 排序欄位，前面加 `-` 為遞減；見下表。預設 `status` |
| `limit`、`cursor` | | 第 3 節 |

| `sort` | 排序方式（相同時一律再以 `agent_id` 排序，結果穩定） |
|---|---|
| `status`（預設） | 依嚴重度（`critical` → `offline` → `warning` → `unknown` → `pending` → `online`，`ia.md` §5.1），同一狀態中離線最久的在前（設計包 S04「預設排序依嚴重度再依持續時間」） |
| `hostname` | 顯示名稱（沒有時為主機名稱），不分大小寫 |
| `last_seen`、`first_seen` | 時間 |
| `ip` | 最後已知 IP（數值順序，IPv4 在 IPv6 前；沒有 IP 的在最後） |
| `agent_version` | 版本（依 SemVer 的數字比較，無法解析的以字串比較） |
| `cpu`、`memory` | 最近一次 report 的 CPU 使用率、記憶體使用比例（沒有 report 的在最後） |

**一台主機**（`data[]` 的元素；詳情另有第 5.2 節的欄位）：

```json
{
  "agent_id": "6f1c0e4a-1b2c-4d3e-9f00-112233445566",
  "hostname": "hmi-line3-07",
  "display_name": null,
  "status": "offline",
  "status_detail": { "offline_reason": "idle_timeout", "offline_reason_legacy": "timeout", "never_connected": false, "starting": false, "long_offline": false },
  "ip": "10.30.3.27",
  "agent_version": "0.3.1",
  "proto": 2,
  "os": { "name": "Windows", "version": "10 IoT LTSC", "kernel": null, "arch": "x86_64" },
  "tags": ["HMI", "Line3"],
  "first_seen": "2026-09-01T01:00:00.000Z",
  "last_seen": "2026-10-13T05:49:05.000Z",
  "offline_for_secs": 840,
  "offline_after_ms": 75000,
  "report_interval_ms": 5000,
  "identity_verified": true,
  "durable": true,
  "replaying": false,
  "last_report": { "ts": "2026-10-13T05:49:01.000Z", "cpu_usage": 99.1, "memory_used_bytes": 8321499136, "memory_total_bytes": 8589934592 },
  "source": "agent"
}
```

- `ip`：最後已知的來源位址（不含連接埠；NAT 後多台主機可能相同）。
- `offline_for_secs`：不是 `online` 時為 `server_time − last_seen`，否則 `null`。`long_offline`：超過 24 小時（設計包 `ia.md` §5.2「長期離線」）。
- `durable`、`replaying`：最近一次連線是否協商了補送、目前是否正在補送（最近 60 秒內收到取樣時間比收到時間早 60 秒以上的 durable report）；Kairos 重新啟動後尚未連線的主機為 `null`。
- `last_report`：最近一次 report 的摘要（CPU、記憶體）；沒有時為 `null`。
- `source`：`agent`（由連線建立）或 `import`（由 `kairos reports import` 建立）。

**`meta`**：

```json
{
  "status_counts": { "critical": null, "offline": 12, "warning": null, "unknown": 0, "pending": null, "online": 1169 },
  "unsupported_status": ["critical", "warning", "pending"],
  "last_event_id": "m3k2x9-1842",
  "data_as_of": "2026-10-13T06:03:26.900Z"
}
```

- `status_counts`：套用**狀態以外**的篩選後各狀態的台數（主機清單的狀態 chip，設計包 S04）。
- `last_event_id`：產生這份列表時最新的 SSE 事件編號（第 6 節）：先開 SSE、再取列表，之後只套用編號較新的事件。
- `data_as_of`：列表所依據的狀態表快照時間（最多約 1 秒前，第 9 節）。

### 5.2 `GET /api/v1/hosts/{agent_id}`

`agent_id` 不分大小寫（回應一律小寫）。除了第 5.1 節的欄位，另有：

```json
{
  "host_fingerprint": "sha256:9f2c…",
  "peer": "10.30.3.27:51544",
  "connected_at": "2026-10-13T01:00:02.000Z",
  "connection": {
    "online": false, "durable": true, "replaying": false,
    "last_backfill_at": "2026-10-13T01:19:44.000Z", "last_backfill_lag_ms": 3601000
  },
  "last_report": { "recv_at": "…", "ts": "…", "seq": 18422, "cpu_usage": 99.1, "memory_used_bytes": 8321499136, "memory_total_bytes": 8589934592, "metrics": 17 },
  "certificate_rejection": {
    "at": "2026-10-13T05:50:11.000Z", "product_code": "KRS-W1105", "protocol_code": "certificate_rejected",
    "reason": "revoked", "serial": "1a2f", "peer": "10.30.3.27", "count": 4
  }
}
```

- `connection`：目前或最近一次連線的投遞資訊（Kairos 重新啟動後尚未連線時為 `null`）。`last_backfill_*`：最近一次收到補送資料的時間與當時的落後量（收到時間 − 取樣時間）。
- `certificate_rejection`：**握手時被拒的憑證歸屬**（設計包 §24 #3、`ia.md` §5.2「憑證被拒」）：最近一次以這台主機的憑證（SAN 為 `agent_id`）連線、在 TLS 握手時被拒的時間、原因、序號與來源位址，`count` 為累計次數。`reason`：`revoked`（`KRS-W1105`）、`expired`（`KRS-W1108`）、`revocation_status_unavailable`（`KRS-E1203`，伺服器端的 CRL 問題）。沒有時為 `null`。之後成功上線的主機仍保留紀錄，但不再算作「憑證被拒」的離線原因（`offline_reason` 只在被拒時間晚於 `last_seen` 時為 `certificate_rejected`）。
  - **只歸屬由信任的 CA 簽發的憑證**：已撤銷、無法判定撤銷狀態（兩者都在憑證鏈驗證之後才檢查），以及已過期但以效期的最後一秒重新驗證可以通過的。自簽或不受信任的 CA 簽發的憑證（`KRS-W1109`）**不歸屬**：SAN 由出示者自己決定，否則任何人都能讓別台主機顯示「憑證被拒」，或以大量假的 `agent_id` 填滿紀錄。
  - 記錄在記憶體中（最多 1 萬台），每 10 秒寫入主機清冊（`hosts.cert_rejected_at`、`cert_rejection`），重新啟動後仍在。清冊中沒有的 `agent_id`（例如已清除的主機）只留在記憶體中。
  - 連線中被踢掉（熱重載後重新驗證失敗，例如撤銷後 1 個重載週期內）的離線原因同樣是 `certificate_rejected`。
- `offline_reason`（第 5.1 節）：`idle_timeout`（超過離線門檻沒有合法訊息）、`agent_closed`（連線關閉：agent 關閉、網路中斷或伺服器關閉連線）、`server_shutdown`（Kairos 關機）、`certificate_rejected`（見上）、`write_unknown`（0.6.0：report 寫入結果未知，Kairos 關閉了連線）。**0.7〔草稿〕起與 Webhook 的 `offline_reason`、`/metrics` 的 `kairos_agent_disconnects_total{reason}` 是同一組名稱**（[`ALERTS.md`](ALERTS.md) 第 2 節；設計包 §28.5 #4）。
  - **過渡期（0.7，破壞性變更）**：0.4～0.6 的名稱是 `timeout`、`connection_closed`。0.7 的回應在 `status_detail` 另帶 `offline_reason_legacy`（舊名稱；其餘三個值兩者相同），SSE 的 `host.offline` 另帶 `reason_legacy`，總覽的離線清單另帶 `offline_reason_legacy`；查詢參數 `offline_reason` 新舊名稱都接受。依舊名稱判斷的整合請改讀 `offline_reason`（新名稱）；`*_legacy` 欄位與舊的查詢值在 0.8 移除。
  - 只有 Kairos 這次啟動後發生的才知道，其餘為 `null`。設計包 §24 #2 的完整分類（`auth_failed`、`superseded`、寫入結果未知等）與每台主機的連線紀錄屬於之後的版本。
- `certificate`（0.7.0）：這台主機最新一張由 Kairos 簽發的憑證的摘要 `{"state", "not_after", "revoked_at"}`，`state` 為 `valid`、`expiring`（30 天內到期）、`expired`、`revoked`；沒有簽發紀錄（手動配置的憑證）時為 `null`。只需要 `hosts:read`（設計包 S05：唯讀角色也看得到「有效，至 2028-01-12」）；序號、指紋與簽發方式需要 `pki:read`，見 `GET /api/v1/pki/certificates?agent_id=`（第 11.5 節）。
- 404 `host_not_found`：清冊中沒有、狀態表中也沒有。

`GET /api/v1/hosts/{agent_id}/metrics` 回傳這台主機最近一次 report 中的指標（v1 的三欄位 report 對應到 `system.cpu.usage`、`system.memory.total`、`system.memory.used`）：

```json
{ "server_time": "…", "data": { "agent_id": "…", "as_of": "…", "metrics": [
  { "name": "system.cpu.usage", "unit": "%", "labels": {} },
  { "name": "system.filesystem.available", "unit": "By", "labels": { "fstype": "ext4", "mountpoint": "/" } }
] } }
```

### 5.3 `GET /api/v1/hosts/{agent_id}/metrics/{name}`

```
GET /api/v1/hosts/6f1c…/metrics/system.cpu.usage?range=24h
GET /api/v1/hosts/6f1c…/metrics/system.filesystem.available?from=2026-10-01T00:00:00Z&to=2026-10-13T00:00:00Z&resolution=1h&label.mountpoint=/
```

| 參數 | 說明 |
|---|---|
| `range` | 從現在往前的長度（`1h`、`24h`、`7d`、`30d`），與 `from`／`to` 擇一；都沒有時為 `1h` |
| `from`、`to` | RFC 3339（含時區）；`to` 省略時為現在。`from` 必須早於 `to` |
| `resolution` | `auto`（預設）、`raw`、`5m`、`1h` |
| `max_points` | 每條序列最多幾個點，10～5000，預設 1000 |
| `label.<key>` | 只取 label 相符的序列（可重複，例：`label.mountpoint=/`） |

- **解析度（`auto`）**：依序選第一個同時滿足兩個條件的：(1) `from` 在該層的保留期限內（`storage.raw_retention_days`、`rollup_5m_retention_days`、`rollup_1h_retention_days`，與保留清除相同的「今天與前 N 天」）；(2) 預估點數不超過 `max_points`（原始資料以這台主機宣告的 report 間隔估算，沒有宣告時 5 秒）。都不滿足時用 1 小時彙總。預設下：1 小時 → 原始資料（720 點）、24 小時 → 5 分鐘（288 點）、7 天 → 1 小時（168 點）、30 天 → 1 小時（720 點）。設計包 S05「解析度依範圍」。
- **點數上限**：結果超過 `max_points` 時，把相鄰的點依時間合併成較大的桶（min 取最小、max 取最大、avg 以 count 加權、count 相加），`bucket_secs` 為合併後的桶長。指定 `resolution=raw` 查 7 天也不會回傳 12 萬個點。
- **每個點**：`{ "t": 桶的開始時間, "min", "max", "avg", "count" }`。原始資料（未合併）時 `min = max = avg` 為該筆的值、`count = 1`。資料空缺（離線、`storage_unavailable`）**不補點**：主控台以斷線呈現，不以 0 填補（設計包 S05）。
- **有 label 的序列分開**：例如每個掛載點一條；`series[]` 依 label 排序，最多 64 條（超過時 `truncated: true`）。
- **最新的桶**：5 分鐘／1 小時彙總在桶結束約 15 秒後才寫入資料庫（STORAGE.md 第 7 節）；尚未寫入的部分（主機的 `rollup_until` 之後）由原始資料即時計算，趨勢的最右端不會缺。
- **時間**：durable 的資料以 Pulse 的取樣時間（`ts`）排列，補送的資料會補進過去的空缺（STORAGE.md 第 5.2 節）。
- 超過所有保留期限的範圍：`from` 調到 1 小時彙總保留的起點，`notes` 寫明（設計包 S05「只保留最近 400 天的 1 小時彙總」）。指定的解析度在查詢範圍的一部分沒有資料（超過該層的保留期限）時，`notes` 寫出該層的保留天數。範圍最長 3,660 天。
- 404 `metric_not_found`：Kairos 從未收過這個名稱的指標（任何主機）。主機有這個指標、但查詢範圍內沒有資料時回 200、`points` 為空陣列；剛上線、第一筆 report 尚未寫入的主機回 200、`series` 為空陣列。

```json
{
  "server_time": "…",
  "data": {
    "agent_id": "6f1c…", "name": "system.cpu.usage",
    "from": "2026-10-12T06:03:27.123Z", "to": "2026-10-13T06:03:27.123Z",
    "resolution": "5m", "requested_resolution": "auto", "bucket_secs": 300,
    "series": [ { "unit": "%", "labels": {}, "points": [ { "t": "2026-10-12T06:05:00.000Z", "min": 3.1, "max": 12.0, "avg": 5.2, "count": 60 } ] } ],
    "truncated": false,
    "retention_days": { "raw": 7, "rollup_5m": 31, "rollup_1h": 400 },
    "notes": []
  }
}
```

### 5.4 `GET /api/v1/overview`

設計包 S03 的資料（`?group=` 在有群組模型後加入）：

```json
{
  "server_time": "…",
  "data": {
    "status_counts": { "critical": null, "offline": 12, "warning": null, "unknown": 0, "pending": null, "online": 1169 },
    "total": 1181,
    "never_connected": 0,
    "long_offline": 3,
    "change_1h": { "online": -9, "offline": 9 },
    "online_series": { "interval_secs": 60, "points": [ { "t": "…", "online": 1169, "total": 1181 } ] },
    "delivery": { "online": 1169, "durable": 1150, "replaying": 0 },
    "longest_offline": [ { "agent_id": "…", "hostname": "lab-pc-17", "display_name": null, "last_seen": "…", "offline_for_secs": 259200, "offline_reason": null } ],
    "top_cpu": [ { "agent_id": "…", "hostname": "his-ap-01", "display_name": null, "cpu_usage": 97.0, "memory_used_ratio": 0.82 } ],
    "top_memory": [ … ],
    "health": { "status": "warning", "items": [ { "id": "database", "status": "warning", "product_code": "KRS-W4202", "summary": "…" } ] }
  }
}
```

- `total`：清冊中的主機數（以清冊為準，重新啟動後不會變少；設計包 S03）。
- `online_series`：每分鐘的上線數（設計包 §24 #5「伺服器每分鐘彙總一次上線數，不從 report 推算」），最近 24 小時；保存在記憶體中，**Kairos 重新啟動後從頭累積**。`change_1h`：與 1 小時前的差（還沒有 1 小時前的資料時為 `null`）。
- `longest_offline`：離線最久的 5 台；`top_cpu`、`top_memory`：線上主機中使用率最高的 10 台（最近一次 report）。
- `health`：第 5.5 節的整體狀態與非正常的項目（總覽的健康條）。

### 5.5 `GET /api/v1/system/health`

設計包 S20：每個檢查項目一列，`status` 為 `ok`／`warning`／`critical`，整體取最嚴重者。

```json
{
  "server_time": "…",
  "data": {
    "status": "warning",
    "items": [
      { "id": "database", "status": "warning", "product_code": "KRS-W4202", "summary": "資料庫已達容量上限，正在提早刪除最舊的資料", "values": { … } },
      …
    ],
    "service": { "version": "0.5.0", "commit": "4223101", "started_at": "…", "uptime_secs": 1036800, "config_file": "/opt/kairos/kairos.toml" }
  }
}
```

| `id` | 內容（`values`） | 警告／嚴重 |
|---|---|---|
| `database` | `used_bytes`（＝(`page_count` − `freelist_count`) × `page_size`，不含 WAL）、`file_bytes`、`wal_bytes`、`max_total_bytes`、`usage_ratio`、`estimate`（`hosts`、`bytes`：以**目前回報中的主機**與各自宣告的 report 間隔估算保留期滿的用量，設計包 §24 #19、§25.5 #4）、`capacity`（`active`：正在刪除；`last`：最近一次容量刪除的開始與結束時間、刪除的筆數） | 警告：預估超過上限、正在提早刪除或最近 24 小時內刪除過（`KRS-W4202`）。不會是嚴重（不影響新的回報） |
| `retention` | 三層保留天數、最近一次保留清除的時間與刪除筆數、日期保護是否暫停清除 | 警告：日期保護暫停中（`KRS-W4201`） |
| `report_writes` | 過去 1 小時與啟動以來：`written`（成功）、`duplicate`（重複已去除）、`storage_unavailable`（確定未保存）、`unknown`（寫入結果未知，連線被關閉）、`rate_limited`（report 速率超過上限）、`success_ratio` | 警告：過去 1 小時 `storage_unavailable` 或 `unknown` > 0（`KRS-E4101`）；嚴重：最近 5 分鐘內持續發生 |
| `delivery` | 線上主機數、其中協商了補送的台數、補送中的台數（S20「支援補送 831 / 843 台 · 補送中 0 台」） | — |
| `connections` | 目前連線數、`server.max_connections`、使用率、狀態表筆數與 `limits.max_agents`、啟動以來因每 IP 上限／全域上限被拒的累計 | 警告：使用率 ≥ 90%（`KRS-W2204`）；嚴重：≥ 98%（D19） |
| `tls` | 伺服器憑證到期、各 CRL 的下次更新時間與剩餘天數、mTLS 是否啟用 | 伺服器憑證 ≤ 30 天警告、≤ 7 天嚴重（`KRS-W1001`）；CRL ≤ 7 天警告（`KRS-W1201`）、≤ 1 天或已過期嚴重（`KRS-E1203`） |
| `listeners` | agent 連線埠與主控台的實際監聽位址 | — |
| `file_descriptors` | `RLIMIT_NOFILE` 的 soft limit（只在 Linux）、需要量、建議值（第 2 節） | 警告：soft limit 低於需要量 |
| `auth` | 是否啟用 mTLS、`auth.allow_unauthenticated` | 嚴重：未啟用 mTLS 而監聽對外位址（`KRS-W0302`） |

〔之後〕CRL 自動更新（#6）、註冊端點（#16）、授權（#13）、告警引擎（#9）、時鐘（NTP）在對應功能實作時加入。

### 5.6 `KRS-W4202`：容量上限刪除最舊的資料

資料庫用量超過 `storage.max_total_bytes` 時，Kairos 依序刪除最舊的原始 report、5 分鐘彙總、1 小時彙總，刪到上限的 95% 以下（STORAGE.md 第 8.2 節）。0.4 起：

- **日誌**：開始時 `WARN`「警告 KRS-W4202：資料庫已達容量上限，正在提早刪除最舊的資料（已使用 … bytes，上限 storage.max_total_bytes = … bytes）…」；結束時 `INFO`「KRS-W4202 結束：…這一輪刪除原始 report N 筆、5 分鐘彙總 M 列、1 小時彙總 L 列」，文字依設計包 `copywriting.md` §4.2 範例 68。沒有東西可刪時另記 `WARN`（節流）。
- **事件紀錄**：開始與結束寫入資料庫 `meta.capacity_event`（JSON：`started_at`、`ended_at`、`used_bytes`、`limit_bytes`、各層刪除的筆數），重新啟動後仍可在系統健康查到最近一次。
- **系統健康**：`database` 項目為警告並帶 `KRS-W4202`（第 5.5 節）。0.6.0 起另有告警事件（系統規則 `sys.db_capacity`，第 13 節）。

### 5.7 `GET /api/v1/hosts/facets`（0.7.0）

主機清單篩選列的選項與台數（設計包 S04 的「離線原因／群組／標籤／OS／agent 版本」下拉選單、S03「群組健康」）。只計算身分**範圍內**的主機（群組範圍的身分只看到自己的群組，第 12.1 節），不受其他篩選條件影響；由列表的記憶體索引回答（第 9 節），不查詢資料庫。權限 `hosts:read`；不接受查詢參數。

```json
{ "server_time": "…", "data": {
  "total": 1181,
  "os": { "values": [ { "value": "Windows", "count": 820 }, { "value": "Ubuntu", "count": 361 } ], "truncated": false },
  "version": { "values": [ { "value": "0.6.0", "count": 1100 } ], "truncated": false },
  "tag": { "values": [ { "value": "HMI", "count": 300 } ], "truncated": false },
  "group": { "values": [ { "value": "B廠產線", "count": 300, "online": 290 } ], "truncated": false },
  "no_group": { "count": 12, "online": 12 },
  "offline_reason": { "values": [ { "value": "timeout", "count": 9 } ], "truncated": false },
  "data_as_of": "…"
} }
```

- 每一種依台數由多到少、再依值排序，最多 200 個（超過時 `truncated: true`）。值與列表的篩選參數相同（`os`、`version`、`tag`、`group`、`offline_reason`），可直接帶入 `GET /api/v1/hosts`。
- `group[].online`：該群組線上的台數；`no_group`：沒有群組的主機。`offline_reason` 只計不在線上的主機（第 5.2 節的代碼）。
- 路徑中的 `facets` 不會與 `agent_id`（UUID）衝突。

### 5.8 `GET /api/v1/hosts/{agent_id}/connections`（0.7.0）

這台主機最近的上線與離線（設計包 S05「連線與憑證」的連線紀錄、斷線紀錄的最小版本）。權限 `hosts:read`；範圍外或不存在的主機回 404 `host_not_found`。

```json
{ "server_time": "…", "data": {
  "agent_id": "6f1c…", "since": "2026-10-15T00:00:02.000Z", "persisted": false, "max_entries": 50,
  "entries": [
    { "at": "2026-10-15T05:49:05.000Z", "event": "offline", "reason": "idle_timeout", "ip": "10.30.3.27" },
    { "at": "2026-10-15T01:00:02.000Z", "event": "online", "reason": null, "ip": "10.30.3.27" }
  ]
} }
```

- 由狀態表的變化累積（與 SSE 的 `host.*` 同一個來源），新到舊，每台主機最多 50 筆、最多 2 萬台（超過時捨棄最久沒有變化的主機）。**只在記憶體中**：`since` 為 Kairos 這次啟動的時間，重新啟動後從頭記錄（`persisted: false`）。
- `reason` 為第 5.2 節的 `offline_reason`（新名稱；這個端點 0.7 才新增，不帶 `*_legacy`）。每次連線的建立時間、來源連接埠、持續時間與協定錯誤碼的完整、持久的連線紀錄（設計包 §24 #2）屬於之後的版本。

## 6. 即時變更推送（SSE）

```
GET /api/v1/events
Accept: text/event-stream
Authorization: Bearer …
Last-Event-ID: m3k2x9-1842        （重連時，瀏覽器的 EventSource 會自動帶上）
```

```text
retry: 5000

event: ready
data: {"server_time":"…","last_event_id":"m3k2x9-1842","resumed":true}

id: m3k2x9-1843
event: host.offline
data: {"agent_id":"6f1c…","hostname":"hmi-line3-07","display_name":null,"status":"offline","reason":"idle_timeout","reason_legacy":"timeout","at":"…","ip":"10.30.3.27"}

: heartbeat 2026-10-13T06:03:42.000Z

```

| 事件 | 何時 | `data` |
|---|---|---|
| `ready` | 連線建立時（第一則） | `server_time`、`last_event_id`（目前最新的編號）、`resumed`（`Last-Event-ID` 是否成功接續） |
| `host.added` | 清冊中沒有的主機第一次上線（新主機） | 同下，`status` 為 `online` |
| `host.online` | 上線、重新上線、離線後恢復 | `agent_id`、`hostname`、`display_name`、`status`、`at`、`ip` |
| `host.offline` | 離線 | 同上，另有 `reason`（第 5.2 節的 `offline_reason`） |
| `alert.created`、`alert.updated`、`alert.resolved` | 告警事件的變化（0.6.0，第 13.6 節；只送給有 `alerts:read` 的身分） | 事件的摘要 |
| `resync` | 要求重新抓取列表（見下） | `reason`：`restarted`（Kairos 重新啟動過）、`too_old`（`Last-Event-ID` 已不在保留範圍）、`overflow`（連線太慢、事件來不及送出） |
| 心跳 | 每 15 秒一行註解（`: heartbeat <時間>`） | 讓代理伺服器與用戶端偵測斷線；不是事件，不改變 `Last-Event-ID` |

- **事件編號**：`<epoch>-<序號>`。epoch 是這次啟動的識別碼（啟動時間的 base36），序號從 1 起遞增。
- **重連的語意**：Kairos 在記憶體中保留最近 **4,096 則**事件。帶 `Last-Event-ID` 重連時，若 epoch 相同且之後的事件都還在保留範圍內，先補送遺漏的事件（`ready.resumed: true`），之後繼續即時推送；否則先送 `resync`（`resumed: false`），用戶端應**重新抓取列表**（`GET /api/v1/hosts`，以回應的 `meta.last_event_id` 作為新的起點），再繼續套用之後的事件。Kairos 重新啟動後一律 `resync`。
- **建議用法**：先開 SSE 收到 `ready`，再取列表；之後只套用編號大於列表 `meta.last_event_id` 的事件（同一台主機的事件依編號順序套用即可，狀態以最後一則為準）。
- **事件只是提示**：狀態可能在事件之後又改變，需要精確值時以列表或詳情為準；設計包 S04「有 12 台狀態變化 · 更新列表」即依事件計數、由使用者決定何時重新抓取。
- **連線數上限**：`console.max_streams`（預設 16）；超過時回 503 `too_many_streams`（`Retry-After: 10`）。每條 SSE 也算在 `console.max_connections` 內。用戶端送出的速度跟不上（伺服器端緩衝超過 256 則）時，送出 `resync` 後從最新的事件繼續。
- 驗證與其他端點相同（`Authorization` 標頭；瀏覽器的 `EventSource` 不能帶標頭，主控台改用工作階段 cookie 後即可使用，本版以 fetch 讀取串流）。token 被撤銷或過期後，已建立的串流在下一次心跳時關閉。`Last-Event-ID` 也可以查詢參數 `last_event_id` 帶入。
- Kairos 關機時串流直接關閉，用戶端依 `retry` 重連。

## 7. 安全

- TLS 設定沿用 agent 連線埠：rustls（ring），TLS 1.2／1.3、rustls 的預設密碼套件；ALPN 只有 `http/1.1`。主控台不要求用戶端憑證。
- **大小與逾時上限**：請求行與標頭合計最多 16 KiB（超過時回 431 或中斷連線）；不接受請求本文（`Content-Length` > 0 或 chunked 回 413）；TLS 握手 10 秒、讀取請求標頭 10 秒（含 keep-alive 閒置）、每個請求處理 30 秒（逾時回 503，SSE 除外）。查詢結果的大小由 `limit`（1,000）、`max_points`（5,000）與序列數（64）限制。
- **連線上限**：`console.max_connections`（預設 64）；滿時暫停接受新連線（在 TCP 層排隊），與 agent 連線埠的名額分開，主控台的負載不影響 agent 的連線。
- **讀取不影響寫入**：查詢使用獨立的唯讀連線（WAL 讀者，不擋住 report 的寫入，最多 4 條同時查詢）；主機列表由記憶體中的索引回答；主控台在自己的 tokio runtime 上執行（第 9 節）。
- **主控台的寫入**：token 的最後使用時間（每 60 秒）、稽核旗標與被拒的憑證（每 10 秒）由背景工作以短交易寫入，busy 時最多等 250 ms。這條連線只經由 SQLite 開啟既有的資料庫：在 Kairos 行程內以 `std::fs` 開關資料庫檔會放掉 writer 的 fcntl 鎖（POSIX 的鎖以行程為單位），`kairos reports import` 就無法偵測到 Kairos 正在執行（e2e 測試 `import.rs` 涵蓋）。
- **回應內容**：不含私鑰、token 秘密或雜湊、憑證以外的設定內容；錯誤回應不含堆疊或 SQL。
- **檔案權限**：沿用 0640／0750（`api_tokens` 在 `kairos.db` 中，與 report 相同的權限）。
- **CORS**：關閉（第 1 節）。
- **稽核**：token 的建立與撤銷寫入資料庫與日誌（第 4.2 節）；每個 API 請求以 `DEBUG` 記錄（方法、路徑、狀態碼、耗時、token_id），不記錄查詢參數以外的內容。

## 8. 框架選型

**hyper 1.x（只啟用 `server`、`http1`）＋ hyper-util（`tokio`）**，自行處理路由與 JSON：

- 與既有的 tokio、tokio-rustls（ring）直接整合：TLS 握手沿用 `tokio_rustls::TlsAcceptor`，hyper 只處理 HTTP/1.1。
- 依賴少：hyper 本身與 `http`、`httparse`、`httpdate`、`want`、`atomic-waker` 等小型 crate；沒有 tower、matchit 等中介層框架。路由只有 8 條，手寫比例行框架更容易稽核。授權皆為 MIT／Apache-2.0，`cargo deny` 的允許清單不需變更。
- 較完整的框架（axum、actix-web）提供的中介層、抽取器、websocket 等本版用不到；日後端點多、需要 OpenAPI 產生時再評估。
- 不啟用 HTTP/2：主控台與 API 的請求量不需要多工，少一個需要防護（HPACK、stream 數量）的攻擊面。

## 9. 效能與量測

**設計**

- 主機列表不在每個請求都查詢資料庫：記憶體中的索引（`api/hosts.rs` 的 `Snapshot`）＝清冊（`hosts`，每 15 秒讀一次；有新主機寫入時——最大的 `host_id` 改變——立即重新讀取）＋狀態表的即時狀態（`Registry::live_view`，持鎖期間只複製參考計數），最多 1 秒重建一次。篩選、排序、分頁在記憶體中進行。
- 趨勢查詢走主鍵／索引的範圍掃描：原始資料 `reports_host_ts (host_id, ts)`、彙總 `(host_id, bucket, series_id)`，都只讀這台主機、這段時間的列；同一個讀取交易中讀 `rollup_until`、彙總與原始資料（一致的快照）。
- 讀取使用唯讀連線池（WAL 讀者，不擋住 writer；最多 4 條同時查詢）；主控台在自己的 tokio runtime（2 個工作執行緒）上執行，不與 agent 連線、report 的 writer 共用工作執行緒。

**量測**：`kairos/src/api/bench.rs`（`#[ignore]`，以實際的 HTTPS 伺服器與用戶端，每個請求一條新的 TLS 連線，含握手與驗證）。

```bash
cargo test -p kairos --release -- --ignored --nocapture api_bench_queries
cargo test -p kairos --release -- --ignored --nocapture api_bench_acks_under_load
```

kp-build（PVE VM，4 vCPU、約 4 GB RAM、本機虛擬磁碟，Ubuntu）2026-10-13。資料：1 萬台主機（一半線上在狀態表、一半只在清冊中）；第 0 台有最近 1 小時的原始資料（每 5 秒一筆、17 個指標）、31 天的 5 分鐘彙總與 1 小時彙總；另外 300 台各有 30 天的 1 小時彙總（共約 380 萬列，資料庫 111 MB）。每種查詢連續 300 次：

| 查詢 | p50 | p99 | 最大 |
|---|---|---|---|
| 列表，預設排序（嚴重度＋離線持續），每頁 100 | 4.8 ms | 9.9 ms | 17 ms |
| `status=offline&sort=-last_seen` | 1.2 ms | 1.4 ms | 1.6 ms |
| CIDR 搜尋 `q=10.12.0.0/16&sort=ip` | 2.8 ms | 3.7 ms | 15 ms |
| `tag`＋`os`＋`version`，`sort=hostname` | 4.7 ms | 10.6 ms | 18 ms |
| `offline_for=1h&durable=false&q=host-0` | 0.9 ms | 1.1 ms | 1.3 ms |
| 每頁 1,000 筆 | 18.8 ms | 30.2 ms | 31 ms |
| 游標分頁走完 1 萬台（每頁 1,000，10 頁） | 25.7 ms | 40.3 ms | 40 ms |
| **每個請求都重建索引**（讀清冊 1 萬列＋合併；最壞情況） | 33.4 ms | 39.8 ms | 44 ms |
| 總覽 | 4.7 ms | 10.3 ms | 15 ms |
| 系統健康 | 2.2 ms | 2.8 ms | 4.3 ms |
| 趨勢 30 天（自動 → 1 小時，720 點） | 6.0 ms | 7.9 ms | 12 ms |
| 趨勢 30 天、有 label 的 3 條序列 | 11.7 ms | 13.1 ms | 15 ms |
| 趨勢 7 天、5 分鐘（2,016 點合併到 1,000 點以內） | 9.9 ms | 12.0 ms | 16 ms |
| 趨勢 24 小時（自動 → 5 分鐘＋尚未寫出的部分由原始資料計算） | 3.1 ms | 3.7 ms | 4.0 ms |
| 趨勢 1 小時（原始資料，720 點） | 4.2 ms | 4.7 ms | 5.0 ms |
| 參考：`/api/v1/me`（TLS 握手＋驗證） | 0.8 ms | 1.3 ms | 1.9 ms |

目標「1 萬台時列表 p99 < 200 ms、30 天趨勢 < 300 ms」：列表最壞情況（每次重建索引）p99 約 40 ms，30 天趨勢 p99 約 8～13 ms。

**查詢與 report 寫入同時進行**：1 萬台輪流、每秒 2,000 筆 report（相當於 1 萬台 × 每 5 秒一筆），每個階段 30 秒（6 萬筆），量測每筆從放入佇列到收到結果（即連線回 `ack`）的延遲。查詢用戶端在另一個 runtime 上執行（模擬外部的瀏覽器或整合；與伺服器共用 4 個 vCPU），混合列表（含每 7 個請求重建一次索引）、每頁 1,000 筆、30 天趨勢、7 天 5 分鐘的 3 條序列、系統健康與總覽：

| 階段 | ack p50 | ack p99 | ack 最大 | 查詢 |
|---|---|---|---|---|
| 只有寫入 | 0.55 ms | 2.3 ms | 134 ms（第一次彙總寫出） | — |
| 4 個用戶端各每秒 1 個請求（接近實際的主控台） | 0.55 ms | 1.0 ms | 8.7 ms | 4 req/s，p99 47 ms |
| 4 個用戶端連續查詢（壓力） | 0.62 ms | 5.7 ms | 18 ms | 140 req/s，p99 83 ms |

- 一般的主控台用量對 `ack` 延遲沒有可量測的影響；連續查詢的壓力下 p99 由約 2 ms 增加到約 6 ms（CPU 競爭），仍遠低於 `timeouts.write_secs`（5 秒）與協定的 8 秒回應預算，沒有 `storage_unavailable`。
- 主控台改用自己的 runtime 之前（與 agent 連線共用工作執行緒）同一個壓力測試的 `ack` p99 為 10.6 ms。
- 主控台的連線數（`console.max_connections`）與同時查詢數（4）有上限，查詢再多也只會排隊，不會無限制地搶走 writer 的 CPU。

## 10. 限制、延後項目與待決定

**本版的限制**

- 只有唯讀端點；主機的顯示名稱、群組、標籤、除役、撤銷等寫入操作屬於之後的版本（需要 RBAC 與稽核雜湊鏈）。
- 使用者、角色、工作階段、LDAP 與稽核雜湊鏈：0.6.0〔草稿〕，第 12 節（延後項目見第 12.11 節）。
- 主機狀態只有 `online`、`offline`、`unknown`；`critical`、`warning`（告警，F3）、`pending`（註冊，F5）的計數為 `null`。
- 離線原因只有 Kairos 這次啟動後發生的（`idle_timeout`、`agent_closed`、`server_shutdown`、`certificate_rejected`、`write_unknown`；0.7 起與 Webhook、`/metrics` 相同，舊名稱見第 5.2 節），不寫入清冊，重新啟動後為 `null`；設計包 #2 的完整分類與每台主機的連線紀錄屬於之後的版本。
- 每分鐘的上線數（總覽的 24 小時 sparkline、「比 1 小時前」）只在記憶體中，重新啟動後從頭累積。
- SSE 的事件只保留在記憶體中（4,096 則）；重新啟動後一律要求重新抓取。
- 系統健康的「被拒的連線」是啟動以來的累計，不是設計包 S20 的「過去 1 小時」。
- 主控台的設定（`[console]`）需要重新啟動才生效（憑證檔的內容會熱重載）。
- 網頁介面：0.7.0〔草稿〕起由主控台的監聽提供（第 14 節）；0.6 以前只有 API。

**延後項目**

| 項目 | 原因 |
|---|---|
| 首次啟動精靈（設計包 S02、#14） | 本衝刺範圍外；`console.listen` 預設只綁 127.0.0.1:9443，管理者手動改為對外位址 |
| `KRS-W4202` 的告警事件（S06） | 0.6.0 已加入（告警引擎的系統規則 `sys.db_capacity`，第 13 節） |
| 啟動時的容量警告改用宣告的 report 間隔（§24 #19、§25.5 #4） | 系統健康已以目前回報中的主機與各自宣告的間隔估算（`database.values.estimate`）；啟動時的 `WARN` 仍以 `server.max_connections` 的最壞情況估算，與設計包的精靈、S21 一起調整 |
| 握手時被拒、但 SAN 不可信（自簽、不受信任的 CA）的連線 | 只記日誌（`KRS-W1109`），不歸屬到主機（第 5.2 節）；設計包「被拒連線嘗試」以來源 IP 呈現時再加入 |
| OpenAPI 文件 | 本文件為規格；端點穩定後再產生 OpenAPI 3.1 |

**已決定（使用者已核准，衝刺 3 整合）**

- **預設啟用主控台，只綁 `127.0.0.1:9443`**：0.4 升級後預設監聽 loopback；9443 被其他程式佔用時 Kairos 以結束代碼 11 無法啟動（systemd 每 5 秒重試），以 `[console]` 的 `listen` 換連接埠或 `enabled = false` 停用（CHANGELOG 破壞性變更第 1 項、INSTALL 第 9.3 節）。開放給管理網段由管理者手動修改，並以防火牆只允許管理網段。
- **token 的有效期限預設 90 天、上限 3,650 天**（第 4.1、4.2 節），不提供不過期的 token（`--no-expiry` 已移除，設計包 §26.4 #3）。
- **驗證失敗依來源 IP 速率限制**（第 4.3 節；IPv6 以 /64 合併）：同一個 NAT 後的合法用戶端可能被同一來源的錯誤 token 連帶限制（最多約 1 分鐘），接受這個取捨。
- **錯誤代碼**（設計包 `copywriting.md` §3.4、`decisions/INTEGRATION.md` §26）：401 `KRS-E6010`、429 `KRS-W6011`、403 `KRS-E6104`（第 1.2 節）。
- **狀態暖機期 2 分鐘**（設計包 `ia.md` §5.1，第 5.1 節）。

**需要使用者或設計決定**

1. **連接埠被佔用的代碼**：設計包 §26.1 #2 核准主控台埠被佔用時為 `KRS-E2001`（結束代碼 11），但目前 agent 連接埠與主控台連接埠的「無法監聽…」訊息都還沒有帶代碼，待補。
2. **主機詳情是否顯示「支援補送」**（`decisions/INTEGRATION.md` §25.8 #6）：API 已提供 `durable`、`replaying`、`connection.last_backfill_*`，畫面是否呈現由設計決定。

## 11. 衝刺 4：註冊與 PKI 的管理端點（草稿）

0.5.0〔草稿〕，**已實作**（`kairos/src/api/enroll.rs`、`kairos/src/enroll_cli.rs`；與本節不同之處見第 11.8、11.9 節）。8444 的協定見 [`crates/kairos-proto/ENROLL.md`](../crates/kairos-proto/ENROLL.md)，資料表與簽發流程見 [`PKI.md`](PKI.md)。本節的端點是主控台 S12（註冊 token）、S13（待核准）、S14（憑證）、S15（CA 與 CRL）、S21（註冊端點模式）的後端（設計包 §24 #6、#7、#16 (d)）。實作時同步更新第 1 節（方法）、第 4 節（範圍）、第 5 節（端點一覽）與第 10 節。

### 11.1 寫入權限：範圍與 `Permission`

本版第一次出現寫入端點，延伸第 4.4 節的 `Principal`／`Permission`（0.6 起權限集中定義在 `kairos/src/rbac.rs`，名稱與角色的對應見第 12.1 節；下表的範圍只適用於 0.4／0.5 建立的 token）：

| API token 的 `scope` | 包含 | 對應的角色（設計包 `ia.md` §4.1） |
|---|---|---|
| `read`（既有） | 所有 `GET` | 稽核員、唯讀 |
| `operate`（新） | `read`＋日常的註冊與憑證操作 | 維運者 |
| `admin`（新） | `operate`＋安全影響等同簽發新憑證或改變對外服務的操作 | 管理員 |

範圍是階層式的（`admin` ⊃ `operate` ⊃ `read`），`api_tokens.scope` 保存其中一個值；`kairos api-token create --scope operate|admin`（預設仍是 `read`）。`Permission` 新增下列變體，端點以 `principal.require(...)` 檢查，日後的使用者角色對應到同一組權限：

| `Permission` | `operate` | `admin` | 用於 |
|---|---|---|---|
| `EnrollTokenWrite` | ✓（只能建立需核准的 token） | ✓ | 建立、停用 token |
| `EnrollTokenAuto` | | ✓ | 建立自動核准的 token；自動核准 token 的批次部署模式 |
| `EnrollDecide` | ✓ | ✓ | 核准、拒絕申請（設計包 S13） |
| `EnrollLicenseOverflow` | | ✓ | 核准後會超過授權節點數（S13「超出授權時僅管理員」） |
| `EnrollModeWrite` | | ✓ | 切換註冊端點模式、自動暫停（S21，D16：維運者唯讀） |
| `CertRevoke` | ✓ | ✓ | 撤銷憑證（S14）、立即重新產生 CRL |
| `CertPolicyWrite`、`GraceOverride` | | ✓ | 憑證政策、「允許再次自動續簽」（D12） |
| `PkiQueue` | ✓ | ✓ | 離線模式的 CSR 佇列匯出與匯入、上傳 CRL（S15：維運者在管理員允許時） |
| `PkiCaWrite` | | ✓ | 簽發 CA 的建立、安裝、啟用、退役（L3） |

- 權限不足回 403 `forbidden`（`KRS-E6104`），`details.required` 為所需的範圍（0.6 起另有 `details.permission`）。寫入端點的權限在註冊服務是否啟動之前檢查（沒有權限時一律 403）。
- **寫入請求**：方法 `POST`、`PUT`；本文為 JSON（`Content-Type: application/json`，其他 415），上限 1 MiB（CSR 佇列匯入 32 MiB）；回應與錯誤格式同第 1 節。主控台日後以 cookie 工作階段呼叫時另需 CSRF token（第 4.4 節）。
- **稽核**：每個寫入都寫入 `audit_events`（`PKI.md` 第 7 節；0.6 起接上雜湊鏈，第 12.8 節）：執行者（`token:<token_id>` 或 `user:<帳號>`）、來源 IP、對象、原因、結果；被拒絕（403）的寫入請求以 `access.denied` 記錄。
- **危險操作的確認**（設計包 `screens.md` §0.3、`cli-style.md` §5.4 的 API 版）：L1 不需額外欄位；L2 必須帶 `reason`（撤銷、拒絕為必填）與 `expected_count`（等於這次處理的筆數，不符時 409 `count_mismatch`）；L3 必須帶 `confirm`（固定字串，例如 `"重建 CA"`）。影響範圍可能在送出前改變的操作（關閉註冊端點、縮短寬限期）另需第 11.4 節的 `impact_token`。

新增的錯誤碼（`product_code` 依 `copywriting.md` §3.3 既有代碼；「—」為就地處理）：

| HTTP | `code` | `product_code` | 何時 |
|---|---|---|---|
| 404 | `token_not_found`、`request_not_found`、`certificate_not_found`、`ca_not_found` | — | 對象不存在 |
| 409 | `count_mismatch` | — | L2 的 `expected_count` 與實際筆數不同 |
| 409 | `impact_changed` | — | 影響數字在確認後改變（第 11.4 節），附新的數字與 `impact_token` |
| 409 | `locked_by_environment` | — | 設定被環境變數覆寫，無法從主控台修改（`details.variable`） |
| 409 | `invalid_state` | — | 對象的狀態不允許這個操作，`details.state` |
| 400 | `confirmation_required` | — | L2／L3 缺少 `reason`、`expected_count` 或 `confirm` |
| 403 | `license_exhausted` | `KRS-E7101` | 核准後會超過授權節點數，且沒有 `EnrollLicenseOverflow` |
| 503 | `issuance_unavailable` | `KRS-E1302`／`KRS-E1303` | 線上簽發不可用：離線簽發模式為 `KRS-E1302`，線上模式但簽發 CA 私鑰無法使用為 `KRS-E1303`（設計包 INTEGRATION §27.3）；核准仍會記錄，申請停在「已核准，待簽發」 |

### 11.2 註冊 token（S12）

| 方法與路徑 | 權限 | 說明 |
|---|---|---|
| `GET /api/v1/enroll/tokens?state=active` | `read` | 列表。`state`：`active`（預設）、`expired`、`disabled`、`all`；游標分頁同第 3 節。每列含部署進度（下方的漏斗） |
| `POST /api/v1/enroll/tokens` | `EnrollTokenWrite`（自動核准另需 `EnrollTokenAuto`） | 建立（不需確認）。回 201，**只有這一次**回傳 `token` 的完整值 |
| `GET /api/v1/enroll/tokens/{token_id}` | `read` | 詳情與部署進度 |
| `POST /api/v1/enroll/tokens/{token_id}/disable` | `EnrollTokenWrite` | 停用（L1，`reason` 選填）；已停用時回 200（冪等）。已簽發的憑證不受影響 |

建立的請求本文：

| 欄位 | 必填 | 規則（設計包 S12） |
|---|---|---|
| `name` | 是 | 1～64 字元，字元限制同 `hostname` |
| `group` | 是 | 1～64 字元 |
| `tags` | 否 | 預設標籤，限制同 hello 的 `tags` |
| `expires_in_secs`／`expires_at` | 否 | 擇一；預設 7 天，最長 90 天 |
| `max_uses` | 是 | 1～100,000（沒有「無限」） |
| `approval` | 是 | `manual`／`auto` |
| `allowed_cidrs` | 自動核准或批次部署模式時必填 | CIDR 陣列（主機位元必須為 0，同第 5.1 節的 `subnet`） |
| `batch_mode` | 否 | 布林；需要 `allowed_cidrs`；自動核准的 token 需要 `EnrollTokenAuto` |
| `allowed_agent_ids` | 否 | 限定主機（UUID 陣列，最多 10,000）；有它時可不設 `allowed_cidrs` |
| `cert_lifetime_days` | 否 | 90～825；預設沿用憑證政策 |

回應 201 的 `data`：`token_id`、`token`（`KRT1-…`，只出現這一次）、`hint`、`install_string`（ENROLL.md 第 14.7 節；註冊端點與推導值不同時含 `e`）、`ca_pin`、`ca_pin_display`（兩行）、`agent_endpoint`、`enroll_endpoint`、`enroll_mode`（目前的模式，主控台據此顯示 S12 的提示並停用「複製」）、`auto_pause_cleared`（這次建立解除了自動暫停）、其餘欄位同列表。

**部署進度**（`progress`）：`queued`（以這個 token 送出、目前被 429 延後的主機數，記憶體中的計數）、`clock_rejected`（最近 15 分鐘被 `clock_skew` 拒絕且之後沒有受理，設計包 §24 #23 (g)）、`used`、`pending`、`rejected`、`withdrawn`、`issued`、`online`（已簽發且曾連上 8443）、`issued_not_online`。

### 11.3 申請（S13）

| 方法與路徑 | 權限 | 說明 |
|---|---|---|
| `GET /api/v1/enroll/requests` | `read` | 列表。`view`：`pending`（預設）／`closed`（最近結束，30 天）；篩選 `token`、`kind`（`enroll`、`grace_renewal`、`renewal`）、`flag`（`none`、`any` 或旗標名稱）、`subnet`、`q`（申請編號〔含不含連字號、不分大小寫〕、主機名稱、IP、`agent_id` 前綴）；排序 `created_at`（預設）、`hostname`、`source_ip`；游標分頁 |
| `GET /api/v1/enroll/requests/{request_id}` | `read` | 詳情：申請編號、公鑰指紋前 8 碼、CSR 摘要、主機資訊、來源 IP 歷史、旗標說明、撤回資訊 |
| `POST /api/v1/enroll/requests/approve` | `EnrollDecide` | 核准（L1，含批次）：`request_ids`（1～1,000）、`cert_lifetime_days`（選填） |
| `POST /api/v1/enroll/requests/reject` | `EnrollDecide` | 拒絕（L2）：`request_ids`（1～1,000）、`expected_count`、`reason_code`（`unknown_host`／`source_mismatch`／`duplicate`／`other`）、`reason`（必填，顯示給 agent 端，`PLS-E3302`） |

- 異常旗標（`flags`）：`source_outside_cidrs`、`duplicate_hostname`、`agent_id_exists`、`burst_from_ip`、`license_overflow`、`unverified_server`（TOFU）、`reenroll_after_revocation`、`fingerprint_mismatch`（設計包 S13）。「全選無異常」由主控台以 `flag=none` 取得 id 後分批送出。
- 批次的回應不是全有全無：`data.results[]` 逐筆回 `request_id` 與 `result`——`issued`（線上簽發完成，附 `serial`）、`approved`（已核准、待簽發：離線模式或 `issuance_unavailable`）、`already_decided`（附目前的 `state`）、`withdrawn`（核准途中已被申請者撤回，設計包 S13「不算失敗」）、`not_found`、`license_exhausted`、`failed`（附 `code`）；`data.summary` 為各結果的筆數。HTTP 一律 200（只要請求本身合法）。
- 核准與拒絕在送出時重新檢查狀態，與 Pulse 的撤回競爭時以資料庫交易的先後為準。

### 11.4 註冊端點模式（S21）

| 方法與路徑 | 權限 | 說明 |
|---|---|---|
| `GET /api/v1/enroll/mode` | `read` | `mode`、`state`（`open`／`open_paused`／`renew_only`／`closed`）、`auto_pause_enabled`、`auto_pause_after_hours`、`paused_since`、`listening`（8444 實際是否在監聽與原因）、每個欄位的來源（`environment`／`console`／`file`／`default`） |
| `GET /api/v1/enroll/mode/impact?mode=closed` | `read` | 切換到指定模式的影響數字（設計包 `enroll-pause.md` §4.3）：`N`（寬限期內）、`M`（30 天內進入寬限期且離線或輪替失敗）、`L`（長期離線，參考）、`R`（待核准）、`T`（有效 token）；`level`（`L1`／`L2`）、`confirm_text`（L2 時為「停止續簽 {N＋M} 台」）與 `impact_token`（數字的雜湊，5 分鐘有效） |
| `PUT /api/v1/enroll/mode` | `EnrollModeWrite` | 本文 `mode`、`auto_pause_enabled`、`auto_pause_after_hours`（1～168）、`reason`、`impact_token`（切換到 `closed` 時必填）。伺服器重新計算影響數字，與 `impact_token` 不符時 409 `impact_changed`（附新的數字，設計包 #16 (d)）；L2 時 `reason` 必填。成功後立即生效（ENROLL.md 第 10 節），寫入 `settings` 與稽核 |

### 11.5 憑證、CA 與 CRL（S14、S15）

| 方法與路徑 | 權限 | 說明 |
|---|---|---|
| `GET /api/v1/pki/certificates` | `read` | 列表：`state`（`valid`、`expiring`、`revoked`、`revocation_pending`、`expired_in_grace`、`expired_beyond_grace`）、`expires_within`、`agent_id`、`q`（序號、主機名稱、`agent_id` 前綴）；每筆含 `hostname`、`group`（第 11.8 節）；游標分頁 |
| `GET /api/v1/pki/certificates/{serial}` | `read` | 詳情（不含私鑰；`cert_pem` 可下載） |
| `POST /api/v1/pki/certificates/revoke` | `CertRevoke` | 撤銷（L2）：`serials`（1～1,000）、`expected_count`、`reason`（`keyCompromise`／`cessationOfOperation`／`superseded`／`unspecified`）、`comment`（必填）。回各筆結果與預計收錄的 CRL 版本；線上模式數秒內生效，離線模式為「撤銷待生效」 |
| `PUT /api/v1/pki/policy` | `CertPolicyWrite` | 憑證政策（S14）：`cert_lifetime_days`、`renew_before_days`、`renew_requires_approval`、`expired_renew_grace_days`、`revoke_on_self_decommission`；縮短寬限期使 K＞0 台立即無法續簽時為 L2（`impact_token`，同第 11.4 節） |
| `POST /api/v1/pki/grace-overrides` | `GraceOverride` | 允許再次自動續簽（L1，原因必填）：`agent_ids`、`until`（最長 90 天）、`reason`、`also_set_renew_only`（布林，模式為關閉時一併改為僅續簽） |
| `GET /api/v1/pki/ca` | `read` | 信任錨與簽發 CA（狀態、到期、私鑰位置、簽發張數、還在使用舊 CA 的主機數） |
| `GET /api/v1/pki/crl` | `read` | 各 CA 目前的 CRL（版本、`this_update`、`next_update`、撤銷筆數、載入時間）與版本歷史 |
| `POST /api/v1/pki/crl/regenerate` | `CertRevoke` | 立即重新產生 CRL（線上模式；S15「立即更新 CRL」） |
| `POST /api/v1/pki/crl` | `PkiQueue` | 上傳 CRL（DER 或 PEM，base64）：S15 的四項檢查，第 1～3 項不過 400（`KRS-E1206`）；撤銷筆數較少時需 `confirm`（L3） |
| `GET /api/v1/pki/signing-queue` | `read` | 已核准、待簽發的申請（離線模式） |
| `GET /api/v1/pki/signing-queue/export` | `PkiQueue` | `kairos-csr-queue/v1` 檔案（`PKI.md` 第 6 節） |
| `POST /api/v1/pki/signing-queue/import` | `PkiQueue` | 匯入 `kairos-signed-certs/v1`；逐張結果（`issued`／`rejected` 與原因） |
| `GET /api/v1/pki/revocations/export` | `PkiQueue` | `kairos-revocations/v1`（離線產生 CRL 用） |

簽發 CA 的建立、安裝、啟用與退役（L3）本衝刺只提供 CLI（第 11.6 節），API 之後再加。`GET /api/v1/pki/ca` 的每個簽發 CA 另有 `expiry_warning`（剩餘天數低於門檻時為 `KRS-W1304`），頂層 `expiry_warning_days` 為門檻；`signing-queue/import` 不通過的項目附 `product_code: KRS-E1305`。

### 11.6 CLI

風格依設計包 `cli-style.md`（結果在 stdout、過程在 stderr、`--json` 為純 ASCII 的單一文件、結束代碼 §8、危險操作 §5.4）。與 `kairos api-token` 相同：以服務帳號執行（`sudo -u kairos`），Kairos 執行中也可以；直接讀寫資料庫，執行中的 Kairos 在 2 秒內察覺（核准的申請由常駐行程簽發，`PKI.md` 第 10 節）。稽核的執行者為作業系統帳號（`SUDO_USER`、`USER`、`LOGNAME` 依序取第一個有值的）。

```
kairos enroll-token create --name <名稱> --group <群組> --max-uses <N> [--approval manual|auto]
                           [--expires-in 7d | --expires-at <日期>] [--tag <標籤>]... [--allow-cidr <CIDR>]...
                           [--batch] [--allow-agent-id <agent_id>]... [--cert-days <N>] [--json]
kairos enroll-token list [--all | --state active|expired|disabled] [--json]
kairos enroll-token show <token_id> [--json]
kairos enroll-token disable <token_id> [--reason <原因>] [--yes] [--json]

kairos enroll list [--closed] [--token <token_id>] [--flag none|any|<旗標>] [--json]
kairos enroll show <request_id | 申請編號> [--json]
kairos enroll approve <request_id | 申請編號>... [--all-clean --token <token_id>] [--yes] [--json]
kairos enroll reject <request_id | 申請編號>... --reason <原因> [--reason-code <代碼>] [--yes --expect-count <N>] [--json]
kairos enroll mode [open | renew_only | closed] [--auto-pause on|off] [--auto-pause-after-hours <N>]
                   [--reason <原因>] [--yes] [--expect-count <N>] [--json]

kairos pki certificates list [--state <狀態>] [--expires-within <時間>] [--json]
kairos pki certificates import <目錄>
kairos pki revoke <主機名稱 | agent_id | 序號>... --reason <原因> [--yes --expect-count <N>] [--json]
kairos pki gencrl [--install-timer]
kairos pki issuing-ca new | install <cert.pem> | activate <ca_id> | retire <ca_id> [--compromised] [--confirm <字串>]
kairos pki queue export [--out <檔案>] | import <檔案> | import --pem-dir <目錄>
kairos pki revocations export [--out <檔案>]
kairos pki crl upload <檔案> [--confirm <字串>]

# 在離線的 CA 主機上（不讀 Kairos 的設定與資料庫）：
kairos pki sign-bundle --in <檔案> --ca-cert <檔案> --ca-key <檔案> --out <檔案>
kairos pki gencrl --revocations <檔案> --ca-cert <檔案> --ca-key <檔案> --out <檔案>
```

| 命令 | 危險等級 | 輸出與規則 |
|---|---|---|
| `enroll-token create` | — | **stdout 只有 token 一行**（`TOKEN=$(kairos enroll-token create …)`，與 `api-token create` 相同）；stderr 寫出名稱、`token_id`、核准方式、到期（本機時區）、CA 指紋（兩行顯示格式）、安裝字串與「token 只會顯示這一次，請以安全管道傳遞」。`--json`（`kairos.enroll-token.create/v1`）的 `data.token`、`data.install_string` 是唯一帶秘密的欄位。自動核准需要 `--allow-cidr`（或 `--allow-agent-id`） |
| `enroll-token list`／`show` | — | 表格：`TOKEN_ID`、名稱、群組、核准方式、已用／上限、有效至（本機時區）、部署進度；token 以 `hint` 顯示（`KRT1-GE8Q…DP97`） |
| `enroll-token disable` | L1 | `是否繼續？[y/N]`；非互動需 `--yes`。重複停用不是錯誤 |
| `enroll list`／`show` | — | 申請編號（`1KGJ-N9XN`）、主機名稱、`agent_id` 前 8 碼、來源 IP、OS、token、送出（相對時間）、異常旗標；`show` 另列公鑰指紋前 8 碼與 CSR 摘要 |
| `enroll approve` | L1 | 列出將核准的筆數與（3 筆以內時）申請編號後詢問；非互動需 `--yes`。`--all-clean --token <id>`＝該 token 所有沒有異常旗標的待核准申請（設計包 S13「全選只選無異常」）。逐筆結果同第 11.3 節；有任一筆 `failed` 時結束代碼 1 |
| `enroll reject` | L2 | 互動時輸入「拒絕 N 筆」（單筆時輸入主機名稱）；非互動需 `--yes` 與 `--expect-count <N>`。`--reason` 必填 |
| `enroll mode` | L1／L2 | 不帶模式時只顯示目前狀態與來源。切換到 `closed` 且 N＋M＞0 時為 L2：互動輸入「停止續簽 {N＋M} 台」，非互動需 `--yes --expect-count <N＋M>` 與 `--reason`；數字在確認後改變時中止並重新顯示（同 `impact_changed`）。被環境變數覆寫時結束代碼 3 並寫出變數名稱 |
| `pki revoke` | L2 | 沿用 `cli-style.md` §5.4 的範例 |
| `pki issuing-ca retire`、`pki crl upload`（筆數較少） | L3 | 非互動需 `--confirm "重建 CA"`／`--confirm "上傳較舊的 CRL"`，`--yes` 不足以略過 |
| `pki sign-bundle`、`pki gencrl --revocations` | — | 在 CA 主機執行；私鑰檔權限過寬時拒絕（結束代碼 4）；輸出檔已存在時不覆寫（結束代碼 11） |

- **結束代碼**（`cli-style.md` §8）：0 完成；1 資料庫錯誤或批次中有失敗；2 用法錯誤（含非互動缺少 `--yes`／`--expect-count`／`--confirm`）；3 設定錯誤（含被環境變數覆寫）；4 憑證錯誤（CA 憑證或私鑰有問題、匯入的憑證檢查不通過）；6 被拒絕（對象的狀態不允許，例如核准已結束的申請）；10 權限不足；11 衝突（資料庫被 `kairos reports import` 獨占、輸出檔已存在）。
- **`--json` 的 `schema`**：`kairos.enroll-token.<動作>/v1`、`kairos.enroll.<動作>/v1`、`kairos.pki.<動作>/v1`。
- 申請編號的輸入規則同 ENROLL.md 第 14.3 節（不分大小寫、連字號可省略）；同一個編號對到多筆待核准申請時（40 位元不保證唯一）列出候選並以結束代碼 2 要求改用 `request_id`。

### 11.7 設定的來源與改用設定檔的值（S21；已決定（2026-10-14））

| 方法與路徑 | 權限 | 說明 |
|---|---|---|
| `GET /api/v1/settings` | `read` | 每個設定鍵的 `value`、`source`（`env`／`console`／`file`／`default`）、`env_var`、`updated_by`、`updated_at`（主控台修改時）、`overridden[]`（較低優先層中與生效值不同的值：`source`、`value`）、`console_editable`、`pending_restart`（本版一律 `null`）。與 `kairos config show --effective --json` 相同 |
| `POST /api/v1/settings/reset` | `enroll.*` 需 `EnrollModeWrite`，`pki.policy.*` 需 `CertPolicyWrite` | 本文 `keys`（設定鍵陣列）：「改用設定檔的值」＝刪除 `settings` 的鍵（L1，寫入稽核）；被環境變數覆寫時 409 `locked_by_environment`。回 `results[]`（`key`、`was_console`） |

- 優先順序：環境變數 ＞ 主控台（資料庫 `settings`）＞ `kairos.toml` ＞ 內建預設。可從主控台修改的鍵：`enroll.mode`、`enroll.auto_pause_enabled`、`enroll.auto_pause_after_hours`、`pki.policy.*`。
- `GET /api/v1/enroll/mode` 另帶 `downloads`（`available`、`reason`、`alternative`）：本版下載端點回 404，主控台據此隱藏一行指令與下載連結（設計包 INTEGRATION §27.4 #3）；token 建立回應的 `downloads`、token 列表的 `meta.downloads` 相同。同一個回應另有 `stats`（啟動以來的請求、429、封鎖、時鐘被拒、握手前關閉、簽發的計數；S20 的資料來源，系統健康的整合之後再做）。

### 11.8 實作與本節的差異

- 列表（token、申請、憑證）以 `limit`（1～1,000，預設 100）回傳前 N 筆與 `page.total`，`page.next_cursor` 一律為 `null`；游標分頁之後加入。
- 未實作：`POST /api/v1/pki/crl`（上傳 CRL）、`GET /api/v1/pki/revocations/export`（離線模式的 CRL 工具，PKI.md 第 13 節）。
- token 的 JSON 有 `hint` 與 `token_hint`（相同的值，只有前 4 碼 `KRT1-XXXX…`）；建立時另有完整的 `token`（只出現這一次）、`install_string`、`ca_pin`、`ca_pin_display`、`agent_endpoint`、`enroll_endpoint`、`enroll_mode`、`auto_pause_cleared`、`downloads`。
- 憑證（列表與 `GET /api/v1/pki/certificates/{serial}`）另有 `hostname`（主機清冊的主機名稱；主機還沒連上 agent 連接埠時為申請的主機名稱）與 `group`（主機群組，沒有時 `null`），主控台 S14 的「主機」「群組」欄（0.7.0〔草稿〕）。`q` 比對序號（片段，不分大小寫）、`agent_id` 前綴與主機名稱（片段，不分大小寫）。
- 拒絕、撤銷的 L2 確認以 `expected_count` 進行；切換到 `closed` 一律需要 `impact_token`（L2 時另需 `reason`）。

### 11.9 CLI 與第 11.6 節的差異

- `kairos config show --effective [<設定鍵或前綴>...] [--source env|console|file|default] [--json]`（設計包 `cli-style.md` §11.7）與 `kairos config reset <設定鍵>... [--yes]`。
- `kairos pki issuing-ca list`；`issuing-ca new` 預設把 CSR 寫在 `data/pki/issuing-<ca_id>.csr`（`--out` 可另外指定）；第一個簽發 CA 在 `install` 時自動啟用。
- 未實作：`kairos pki certificates import`、`kairos pki gencrl`、`kairos pki revocations export`、`kairos pki crl upload`。

## 12. 使用者、角色、工作階段與稽核（0.6.0〔草稿〕）

衝刺 5，**已實作**（審計 F6、S7 的雜湊鏈部分；設計包 `kairos/screens.md` S01、S16、S17、S18、S23、§24 #10、#11、`README.md` D25）。程式：`kairos/src/rbac.rs`（權限的集中定義）、`kairos/src/users/`（密碼、TOTP、主控台金鑰、資料表、LDAP）、`kairos/src/audit.rs`（雜湊鏈）、`kairos/src/api/{auth,account,users,audit_log}.rs`、`kairos/src/users_cli.rs`、`kairos/src/audit_cli.rs`。資料庫為 **schema v6**（第 12.10 節）。

### 12.1 權限、角色與範圍

權限（`Permission`）集中定義在 `kairos/src/rbac.rs`：每個端點以 `principal.require(Permission::…)` 檢查，權限名稱（`<資源>:<動作>`）是對外的穩定識別字（`/api/v1/me` 的 `permissions`、403 的 `details.permission`）。新功能（例如告警的 `alerts:read`、`alerts:write`）只要在 `Permission` 加一個變體與名稱、加進 `Permission::ALL`，並在 `Role::permissions` 決定哪些角色擁有它。

內建角色依設計包 `ia.md` §4.1、§4.2（自訂角色屬於 v1.0 之後）：

| 權限 | 管理員 `admin` | 維運者 `operator` | 稽核員 `auditor` | 唯讀 `viewer` | 用於 |
|---|:-:|:-:|:-:|:-:|---|
| `hosts:read` | ✓ | ✓ | ✓ | ✓ | 主機、趨勢、總覽、SSE |
| `health:read` | ✓ | ✓ | ✓ | ✓ | 系統健康（S20） |
| `enroll:read` | ✓ | ✓ | ✓ | | 註冊 token、申請、模式（S12、S13） |
| `enroll.tokens:write` | ✓ | ✓ | | | 建立需核准的 token、停用 |
| `enroll.tokens:auto_approve` | ✓ | | | | 自動核准 token |
| `enroll.requests:decide` | ✓ | ✓ | | | 核准、拒絕申請 |
| `enroll.license:overflow` | ✓ | | | | 超出授權節點數時核准 |
| `enroll.mode:write` | ✓ | | | | 註冊端點模式 |
| `pki:read` | ✓ | ✓ | ✓ | | 憑證、CA、CRL（S14、S15） |
| `pki.certificates:revoke` | ✓ | ✓ | | | 撤銷、立即重新產生 CRL |
| `pki.policy:write` | ✓ | | | | 憑證政策 |
| `pki.grace:override` | ✓ | | | | 允許再次自動續簽（D12） |
| `pki.queue:manage` | ✓ | ✓ | | | 離線簽發的 CSR 佇列 |
| `pki.ca:write` | ✓ | | | | 簽發 CA（本版只有 CLI） |
| `settings:read` | ✓ | ✓ | ✓ | | 設定與來源（S21；維運者唯讀，D16） |
| `users:read` | ✓ | | ✓ | | 使用者、角色指派、工作階段清單（S16） |
| `users:write` | ✓ | | | | 建立、停用、刪除、角色、重設密碼與 MFA |
| `sessions:revoke` | ✓ | | | | 結束他人的所有工作階段（D25） |
| `api_tokens:read` | ✓ | | ✓ | | API token 清單（S21「整合」） |
| `api_tokens:write` | ✓ | | | | 建立、撤銷 API token |
| `directory:read` | ✓ | | ✓ | | 目錄服務設定（bind 密碼遮蔽）與群組對應（S17） |
| `directory:write` | ✓ | | | | 群組對應、以帳號測試登入 |
| `audit:read` | ✓ | | ✓ | | 所有人的稽核紀錄、驗證完整性（S18） |
| `audit:read_own` | ✓ | ✓ | | | 只看自己的操作（維運者 `actor=me` 固定） |
| `audit:export` | ✓ | | ✓ | | 匯出稽核紀錄 |
| `alerts:read` | ✓ | ✓ | ✓ | ✓ | 告警事件、SSE 的 `alert.*`（S06、S07；第 13 節） |
| `alerts:write` | ✓ | ✓ | | | 確認、指派、暫停通知、解決、註解 |
| `alert-rules:read` | ✓ | ✓ | ✓ | | 告警規則與試算（S08） |
| `alert-rules:write` | ✓ | ✓ | | | 自訂規則、啟停、系統規則的通知設定（S08、S09） |
| `alert-rules:admin` | ✓ | | | | 系統預設規則的條件（S09） |
| `silences:read` | ✓ | ✓ | ✓ | ✓ | 靜音與維護時段（S10） |
| `silences:write` | ✓ | ✓ | | | 建立、修改、提前結束靜音與維護時段 |
| `channels:read` | ✓ | ✓ | | | 通知管道的清單與狀態（S11；機密只有遮蔽值） |
| `channels:test` | ✓ | ✓ | | | 寄送測試通知 |
| `channels:write` | ✓ | | | | 新增、修改、刪除通知管道（含機密） |

個人設定（`/api/v1/me/…`：自己的工作階段、密碼、TOTP）不需要權限，只需要以帳號登入的工作階段。

**範圍**（`ia.md` §4.3）：每筆角色指派是（主體, 角色, 範圍）。主體為本機或目錄服務的使用者、或目錄服務的群組；範圍為 `global`（全站）或 `group:<主機群組>`。群組範圍的指派**只授予該群組主機的讀取**：主機列表、詳情、趨勢、總覽與 SSE 只看得到 `hosts.host_group` 相符（不分大小寫）的主機，範圍外的主機回 404（與不存在相同）；註冊、PKI、設定、使用者、稽核等系統層級的操作一律需要全站範圍的指派（設計包「系統層級頁面只對全站範圍的角色開放」；群組範圍的管理員能在群組內做什麼，等主機的寫入操作出現時再擴充，見第 12.11 節）。主機的群組在簽發時取自註冊 token 的 `group`（已有群組的主機不覆寫）；主機列表帶 `group` 欄位，`GET /api/v1/hosts?group=` 可篩選。租戶範圍（F12）保留、不接受。

**API token** 的權限有三種來源（`kairos api-token create`、`POST /api/v1/api-tokens`）：

| 種類 | 權限 | 建立方式 |
|---|---|---|
| 角色 | 該角色的全站權限 | `--role viewer`（**0.6 起的預設**）、`--role operator` … |
| 使用者 | 擁有者**目前**的權限（個人指派；不含目錄服務群組的對應）；另指定角色時取交集。擁有者停用或刪除時 token 立即失效 | `--user chen.y [--role viewer]` |
| 舊範圍（0.4／0.5） | **凍結在升級前**：`read`＝所有讀取（主機、總覽、健康、事件、註冊、PKI、設定），`operate`＝`read`＋需核准的 token、核准拒絕、撤銷、CSR 佇列，`admin`＝`operate`＋自動核准 token、模式、政策、寬限期覆寫、簽發 CA。升級後新增的權限（使用者、稽核、目錄服務、API token 管理、告警…）**不會**給它們 | 升級前建立的 token；`--scope`（已棄用） |

403 的 `details`：`permission`（所需的權限）與 `required`（擁有它的最低角色；舊範圍的 token 為 0.5 的範圍名稱，與 0.5 相同）。

### 12.2 登入（S01）

| 方法與路徑 | 說明 |
|---|---|
| `GET /api/v1/auth/methods` | 不需驗證：`local`、`ldap`（是否啟用目錄服務登入）、`mfa`（`totp`） |
| `POST /api/v1/auth/login` | 本文 `username`、`password`、`method`（`local`，預設；或 `ldap`）。成功時 `Set-Cookie` 與 `data.user`、`data.csrf_token`、`data.session`、`data.must_change_password`；帳號啟用 TOTP 時改回 `{"mfa_required": true, "mfa_token", "mfa_expires_at"}`（沒有 cookie） |
| `POST /api/v1/auth/login/mfa` | 第二步：`mfa_token` 與 `code`（6 位數 TOTP，或 10 組復原碼之一 `XXXXX-XXXXX`，不分大小寫、可省略連字號）。挑戰 5 分鐘有效、最多嘗試 5 次 |
| `POST /api/v1/auth/logout` | 結束目前的工作階段並清除 cookie（需要 CSRF） |

- **密碼**：Argon2id（PHC 字串；預設 m=19 MiB、t=2、p=1，`[accounts]` 可調）。驗證以雜湊中記錄的參數計算，參數調整後舊雜湊仍可驗證，下次登入成功時以新參數重新雜湊。帳號不存在、或是目錄服務帳號時仍以同樣的參數驗證一次假的雜湊，回應時間不透露帳號是否存在。同時進行的雜湊最多 2 個（每個約 19 MiB 記憶體）。
- **不透露帳號是否存在**：帳號不存在、密碼錯誤、帳號停用、目錄服務帳號以本機方式登入，一律 401 `invalid_credentials`（`KRS-E6001`）。
- **鎖定**（`KRS-E6003`）：連續失敗 `accounts.lockout_threshold`（預設 5）次後鎖定 `accounts.lockout_minutes`（預設 15）分鐘，回 403 `account_locked`，`details.locked_until`、`Retry-After`；鎖定期間正確的密碼也一樣。**不存在的帳號同樣會「鎖定」**（記憶體中計數，最多 1 萬個名稱），回應與存在的帳號相同。TOTP 第二步、變更密碼與綁定 TOTP 時輸入錯誤的目前密碼，都計入同一個計數。管理員以 `kairos users unlock` 或啟用帳號解除。
- **依來源限速**（`KRS-W6011`）：登入失敗與 API token 驗證失敗共用同一個依來源 IP 的額度（第 4.3 節：可連續 10 次，之後每 6 秒 1 次）；用完時登入也回 429。每次登入（與第二步）**先扣**一次額度、成功或與密碼無關的錯誤（本文格式、目錄服務無法使用）時退回：同時送出的大量嘗試不會在扣減之前都通過檢查。帳號的鎖定在建立工作階段的交易中再確認一次（等待雜湊期間被鎖定、停用的帳號不會登入成功）。同時等待密碼雜湊的請求超過 32 個時回 503 `busy`（`Retry-After: 1`）。放在反向代理之後時，所有使用者共用代理的來源位址與額度。
- **首次登入必須變更密碼**：管理員建立或重設密碼（含系統產生的一次性密碼）的帳號，登入後只能呼叫 `GET /api/v1/me`、`POST /api/v1/me/password`、登出，其他端點回 403 `password_change_required`（`KRS-E6013`）。
- **密碼政策**：至少 `accounts.password_min_length`（預設 12，最少 8）個字元、最多 256、不含控制字元、不包含帳號名稱、不是同一個字元重複、不是常見的密碼（例如 `password`、`P@ssw0rd2026`）。產生的一次性密碼為 4 組 5 個字元（約 116 位元）。
- **目錄服務**（`method: "ldap"`，第 12.7 節）：第一次登入時建立來源為 `ldap` 的使用者（沒有密碼）；同名的本機帳號存在時拒絕（不讓目錄服務接管本機帳號）。錯誤：密碼錯誤或查無使用者 401 `KRS-E6001`；AD 回報停用或鎖定 403 `account_disabled`（`KRS-E6009`）；連不上、憑證不受信任、服務帳號 bind 失敗、搜尋基底不存在 503 `directory_unavailable`（`product_code` 依原因為 `KRS-E6002`、`E6005`、`E6006`、`E6007`，`details.reason`；伺服器名稱等細節只寫日誌）。
- 每次成功、失敗、鎖定、登出都寫入稽核（第 12.8 節）。失敗的執行者一律為 `anonymous`、對象為輸入的帳號（不記錄密碼）；帳號不存在且輸入不符合帳號格式時（例如把密碼打進帳號欄）只記 SHA-256 的前 8 碼。

### 12.3 工作階段與 CSRF

- **cookie**：`__Host-kairos_session=<秘密>; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=<絕對逾時>`。秘密為 32 bytes CSPRNG 的 base64url，資料庫只保存 SHA-256（`sessions.session_hash`）；清單中以 16 個十六進位字元的公開 `session_id` 識別。每次登入都是新的工作階段（沒有 session fixation）。
- **逾時**：閒置 `accounts.session_idle_minutes`（預設 30）分鐘、絕對 `accounts.session_absolute_hours`（預設 12）小時。最後活動時間最多每 60 秒寫回一次（閒置的判斷誤差 1 分鐘以內）；SSE 不更新最後活動時間。
- **立即失效**：每個請求都查詢資料庫——工作階段被結束、逾時、使用者停用或刪除、目錄服務帳號而 `[ldap]` 已停用時回 401 `unauthorized`（`KRS-E6010`，`details.reason = "session_expired"`）；SSE 每次心跳（15 秒）重新確認身分並**重新計算權限與主機範圍**：失效或已沒有主機的讀取權限時關閉，降為群組範圍時只再推送範圍內的主機。
- **CSRF**：以工作階段驗證的 `POST`／`PUT`／`PATCH`／`DELETE` 必須帶 `X-CSRF-Token`（登入回應與 `GET /api/v1/me` 的 `csrf_token`；由 cookie 秘密以主控台金鑰 HMAC 衍生，常數時間比對），且 `Origin`（有帶時）必須等於 `https://<Host>`、`Sec-Fetch-Site` 不可是 `cross-site`；否則 403 `csrf_failed`（`KRS-E6105`）。經反向代理時代理必須保留原本的 `Host`。登入端點要求 `Content-Type: application/json`（跨站的表單無法送出），CORS 關閉。
- 同時帶 `Authorization` 與 cookie 時以 `Authorization`（API token）為準，不需要 CSRF token。
- **主控金鑰**：`<storage.data_dir>/console.key`（32 bytes，0600），第一次啟動主控台時建立；用於 CSRF token、TOTP 秘密的加密（AES-256-GCM，以使用者 id 為附加資料）與復原碼的 HMAC。無法建立或讀取時帳號登入停用（API token 照常），日誌有 `ERROR`。遺失時已綁定的 TOTP 無法驗證，以 `kairos users reset-mfa` 重設。

### 12.4 個人設定（S23；D25）

| 方法與路徑 | 說明 |
|---|---|
| `GET /api/v1/me` | 目前的身分：工作階段為 `type: "session"`、`user`、`roles`（含 `via_group`：來自哪個目錄服務群組）、`session`、`csrf_token`；API token 為 `type: "api_token"`、`token_id`、`name`、`scopes`（舊範圍）、`role`、`owner`。兩者都有 `permissions`（全站）與 `group_permissions` |
| `GET /api/v1/me/sessions` | 自己的有效工作階段（`current` 標出目前這個） |
| `POST /api/v1/me/sessions/revoke-others` | **結束其他所有工作階段、保留目前這個**（D25；S23 與 S16 對象是自己）。回 `ended`（N）、`kept_current: true` |
| `POST /api/v1/me/sessions/{session_id}/revoke` | 結束自己的一個工作階段（目前這個時一併清除 cookie） |
| `POST /api/v1/me/password` | `current_password`、`new_password`：變更自己的密碼（本機帳號）；其他工作階段結束、保留目前這個 |
| `POST /api/v1/me/totp/setup` | `password`：產生 TOTP 秘密（20 bytes），回 `secret`（base32）與 `otpauth_uri`（QR code 內容；SHA1、6 位數、30 秒）。已啟用時 409 |
| `POST /api/v1/me/totp/enable` | `code`：以驗證器顯示的碼確認後啟用，回 10 組 `recovery_codes`（**只在這次回應出現**） |
| `POST /api/v1/me/totp/disable` | `password` 與 `code`（目前的 TOTP 或一組復原碼）：停用並刪除復原碼；只知道密碼的人不能關掉兩步驟驗證 |

- 這些端點只接受工作階段（API token 回 400 `not_a_session`）。目錄服務帳號的密碼與兩步驟驗證由 AD 管理（409 `directory_account`）。
- TOTP 驗證接受前後各 1 個時間步，同一個時間步只能使用一次（重放回 401 `invalid_mfa_code`，`KRS-E6012`）；復原碼各只能用一次。

### 12.5 使用者與角色（S16）

| 方法與路徑 | 權限 | 說明 |
|---|---|---|
| `GET /api/v1/users` | `users:read` | 列表：`source`（`local`／`ldap`）、`status`（`active`／`disabled`／`locked`）、`role`、`q`；每列含 `roles`、`mfa`、`last_login_at`、`active_sessions` |
| `POST /api/v1/users` | `users:write` | `username`（1～64 字元：英數字與 `. _ - @`）、`display_name`、`email`、`roles`（`["admin", "operator@group:B廠產線"]`）、`password`（選填；沒有時產生一次性密碼，**只在回應的 `initial_password` 出現一次**）。一律在第一次登入時必須變更。201 |
| `GET /api/v1/users/{username}` | `users:read` | 詳情、有效權限、有效的工作階段、擁有的 API token |
| `PUT /api/v1/users/{username}` | `users:write` | `display_name`、`email` |
| `PUT /api/v1/users/{username}/roles` | `users:write` | 以 `roles` 取代全部指派（L1，`reason` 選填）；回應帶 `permissions_added`、`permissions_removed`（S16「顯示權限差異」） |
| `POST /api/v1/users/{username}/disable` | `users:write` | 停用（L1，`reason`）：所有工作階段立即結束，擁有的 API token 失效 |
| `POST /api/v1/users/{username}/enable` | `users:write` | 啟用（同時解除鎖定） |
| `POST /api/v1/users/{username}/delete` | `users:write` | 刪除（L2：`confirm` 必須等於帳號）：角色指派、TOTP 一併刪除，工作階段結束，擁有的 API token 撤銷；稽核紀錄保留 |
| `POST /api/v1/users/{username}/unlock` | `users:write` | 解除登入失敗的鎖定 |
| `POST /api/v1/users/{username}/password` | `users:write` | 重設密碼（本機帳號；`password` 或產生一次性的 `password`）：所有工作階段結束、下次登入必須變更 |
| `POST /api/v1/users/{username}/mfa/reset` | `users:write` | 重設 MFA（L1）：TOTP 與復原碼刪除 |
| `GET /api/v1/users/{username}/sessions` | `users:read` | 有效的工作階段 |
| `POST /api/v1/users/{username}/sessions/revoke-all` | `sessions:revoke` | **結束某使用者的全部工作階段**（D25，L1，`reason`）。對象是自己時回 409 `target_is_self`，請改用 `POST /api/v1/me/sessions/revoke-others` |
| `GET /api/v1/roles` | `users:read` | 四個內建角色的權限矩陣與成員數（S16「角色與權限」分頁），所有權限名稱 |

防呆（S16）：不能移除最後一位**啟用中、有全站管理員個人指派**的使用者的管理員角色、不能停用或刪除他（409 `last_admin`；目錄服務群組帶來的管理員不計入，因為無法預先確定群組成員）；不能停用或刪除自己（409 `cannot_target_self`；以自己擁有的 API token 操作也算自己）。`user_id` 不重複使用（`AUTOINCREMENT`）：刪除後建立的帳號不會接手舊帳號的工作階段紀錄。檢查與變更在同一個寫入交易中，兩位管理員同時操作不會一起被移除。

### 12.6 API token（S21「整合」）

| 方法與路徑 | 權限 | 說明 |
|---|---|---|
| `GET /api/v1/api-tokens?all=true` | `api_tokens:read` | 清單（預設只有有效的）：`grant`（`role:operator`、`user:chen.y∩viewer`、`scope:read`）、到期、最後使用；不含秘密與雜湊 |
| `POST /api/v1/api-tokens` | `api_tokens:write` | `name`、`role` 與／或 `owner`（帳號；至少一個）、`expires_in_secs` 或 `expires_at`（預設 90 天，最長 3,650 天，不提供不過期）。201，`token` 只出現這一次 |
| `POST /api/v1/api-tokens/{token_id}/revoke` | `api_tokens:write` | 撤銷（L1，`reason`）；重複撤銷不是錯誤（`already_revoked`） |

建立與撤銷（API 與 `kairos api-token` 都一樣）在同一個交易中寫入稽核雜湊鏈；執行中的 Kairos 另在 10 秒內寫入日誌（第 4.2 節）。

### 12.7 目錄服務（LDAP／AD；S17）

設定在 `kairos.toml` 的 `[ldap]`（第 12.10 節）；群組對應存在資料庫。

| 方法與路徑 | 權限 | 說明 |
|---|---|---|
| `GET /api/v1/directory` | `directory:read` | 設定（伺服器、加密方式、CA 檔、bind DN、搜尋基底與屬性；**bind 密碼一律顯示 `********`**）與群組對應 |
| `PUT /api/v1/directory/mappings` | `directory:write` | `mappings: [{"group_dn": "CN=IT-Ops,OU=Groups,…", "role": "operator@group:B廠產線"}]` 取代全部（L1）；DN 以小寫、去除 `,`／`=` 前後空白比對 |
| `POST /api/v1/directory/test` | `directory:write` | 「以帳號測試登入」：`username`、`password`，回這個帳號的 DN、群組與會得到的角色（`ok: true`），或錯誤的 `product_code`、`reason`、`detail`（給管理員看的細節）；不建立工作階段。失敗計入來源的失敗額度 |

- **驗證流程**：連線（LDAPS 或 `ldap://`＋StartTLS；**一律驗證伺服器憑證鏈與名稱**，只信任 `ldap.ca_file` 中的 CA，不使用系統的信任清單、沒有略過驗證的選項）→ 以服務帳號 bind（`ldap.bind_dn`＋`ldap.bind_password_file`；沒有設定時匿名）→ 在 `ldap.user_base` 以 `(&<user_filter>(<login_attribute>=<帳號>))` 搜尋（帳號依 RFC 4515 跳脫）→ 搜尋結果必須恰好一筆 → 以該 DN 與輸入的密碼 bind → 讀取 `ldap.group_attribute`（AD 為 `memberOf`）。**空密碼一律拒絕**（LDAP 以空密碼 bind 會成為匿名 bind 而回成功）。主要伺服器連不上（或憑證問題）時依序改試 `ldap.fallback_urls`。
- **未加密**：`ldap://` 沒有 StartTLS 時拒絕啟動，除非 `ldap.allow_plaintext = true`（設計包 S17：需要管理員明確確認；密碼會以明文經過網路）。
- **權限**：個人指派＋所屬群組的對應；群組在登入時讀取並保存在工作階段中（AD 的群組異動在下次登入或工作階段逾時後生效，見第 12.11 節）。
- 純 Rust 實作：`ldap3` 0.12（`tls-rustls-ring`，與其他連接埠共用 rustls 0.23＋ring），`cargo deny` 通過。

### 12.8 稽核日誌與雜湊鏈（S18）

**記錄的事件**（`category`／`action`）：

| 類別 | 動作 |
|---|---|
| `login` | `login.success`、`login.failure`（含原因、方式）、`account.lock`、`logout` |
| `user` | `user.create`／`update`／`disable`／`enable`／`delete`／`unlock`、`role.change`（新增、移除的指派與權限差異）、`password.change`／`reset`、`mfa.enable`／`disable`／`reset`、`session.revoke`／`revoke_others`／`revoke_all`、`directory.mappings.change`、`directory.test` |
| `api_token` | `token.create`、`token.revoke` |
| `settings` | `mode.change`、`policy.change`、`setting.reset`（0.5 起） |
| `enroll`、`pki`、`host` | 註冊申請、核准、拒絕、撤回、簽發、撤銷、CA、CRL、除役（0.5 起） |
| `access` | `access.denied`：被拒絕（403）的寫入請求（方法、路徑、所需的權限） |
| `audit` | `audit.query`（第一頁）、`audit.export`、`audit.verify`：稽核員與管理員自己的查詢與匯出也記錄 |
| `system` | `system.start`、`system.shutdown`（版本、設定檔） |

每筆有 `at`、`actor`（`user:<帳號>`、`token:<token_id>`、執行 CLI 的作業系統帳號、`system`、`anonymous`）、`actor_ip`、`object`、`result`（`ok`／`failed`／`denied`／`rejected`）、`code`（產品代碼）、`details`（原因、變更前後；**不含密碼、token、秘密**）。

**雜湊鏈**：

```text
hash = SHA-256( prev_hash ‖ "\n" ‖ JSON([id, at, actor, actor_ip, category, action, object, result, code, details]) )
```

- JSON 為 serde_json 的精簡格式（沒有空白，非 ASCII 原樣以 UTF-8 輸出），`details` 是資料庫中保存的**字串本身**（匯出的 `details_raw`），沒有值時為 `null`；`at` 為 Unix 毫秒（匯出的 `at_ms`）；雜湊為小寫十六進位。第一筆的 `prev_hash` 為 64 個 `0`。
- 寫入一律經由 `audit::append`：先 INSERT 取得寫入鎖再計算，Kairos、主控台與 CLI 同時寫入也不會分岔；與被記錄的變更在**同一個交易**中（例如建立使用者與它的稽核要嘛都寫入、要嘛都沒有）。
- 保留期限（`audit.retention_days`，預設 1,095 天＝3 年，獨立於 report 的保留天數）每小時清除最舊的一段，被清除的最後一筆的雜湊記在 `meta.audit_chain_anchor`，剩下的第一筆必須接在它之後；最新一筆記在 `meta.audit_chain_head`，只刪除最後幾筆也會被發現。
- 0.5 寫入的紀錄（沒有雜湊）在升級後**第一次**寫入或驗證時依 id 順序補上，並記在 `meta.audit_legacy_sealed`；之後每次寫入只計算它自己那一筆。繞過程式直接插入資料庫、沒有雜湊的紀錄不會被下一次寫入接進鏈中，驗證時以 `missing_hash` 指出。
- 驗證在同一個讀取交易（同一個快照）中進行，驗證期間的寫入不會造成假的斷點。
- 被拒絕的寫入請求（`access.denied`）同一個執行者每分鐘最多記 20 筆，超過的筆數併入下一筆的 `details.suppressed_since_last`（避免低權限的身分以大量請求灌爆稽核表）。
- **HMAC（0.7〔草稿〕，schema v7）**：雜湊鏈本身沒有秘密，能改寫資料庫檔的人可以重新計算整條鏈。0.7 起每筆另有 `hmac = HMAC-SHA256(K, "kairos-audit-record/v1\n" ‖ hash)`（小寫十六進位），`K = HMAC-SHA256(金鑰檔, "kairos-audit-hmac/v1")`；金鑰檔是資料目錄的 `audit-hmac.key`（32 bytes，0600），**與 `console.key` 分開**（交給外部稽核者驗證 HMAC 時不必同時交出 CSRF／TOTP 的金鑰；遺失或輪替 `console.key` 也不影響稽核鏈）。金鑰檔只由 Kairos 服務在啟動時建立，CLI 只讀取。`meta.audit_chain_head` 另帶 `mac`。
  - **升級的邊界**：服務第一次以 0.7 啟動、建立金鑰後寫入的第一筆記在 `meta.audit_hmac`（`since_id`、`key_id`）。之前的紀錄（0.6 以前，或服務啟動前由 CLI 寫入的）只有雜湊鏈；`kairos audit verify` 與 API 的 `hmac` 物件寫出邊界（`since_id`、`chain_only` 筆數、`verified` 筆數）。
  - 新的斷點原因：`missing_hmac`（起點之後沒有 HMAC）、`hmac_mismatch`（紀錄或雜湊被重新計算）、`hmac_boundary_mismatch`（起點被移動）、`hmac_key_unavailable`（金鑰檔遺失或無法讀取）、`hmac_key_mismatch`（金鑰檔與起點記錄的 `key_id` 不同；不會以新的金鑰「接續」簽下去）。都是 `KRS-E6201`。
  - **外送鏈尾**：每 `audit.anchor_log_minutes` 分鐘（預設 60，0 不寫）寫一行固定事件 `audit.chain_anchor`（`KRS-I6202`，INFO；`fields`：`last_id`、`last_hash`、`last_hmac`、`hmac_since`）。以 syslog 或日誌收集器送到其他主機後就是資料庫之外的副本；比對時取外送的某一行，確認資料庫中同一個 `last_id` 的 `hash` 相同，且之後的紀錄能從它接上。
  - **自我驗證**：每 `audit.verify_interval_hours` 小時（預設 24，第一次在啟動後 1 分鐘）重新計算整條鏈與 HMAC，寫一筆 `audit.verify`（執行者 `system`，`details.via = "self_check"`），並發布內部事件；失敗時記 `ERROR`（`audit.chain_broken`）且告警引擎開系統規則「稽核日誌完整性」（`sys.audit_integrity`，緊急，`KRS-E6201`），下一次驗證通過時恢復。
  - **仍然的限制**：同時能改寫資料庫並刪除 `meta.audit_hmac` 與所有 `hmac` 的人，可以把鏈「降級」成 0.7 之前的樣子；重播先前抄下的 head 也無法由資料庫本身發現。兩者都由外送的鏈尾雜湊在外部發現。

| 方法與路徑 | 權限 | 說明 |
|---|---|---|
| `GET /api/v1/audit` | `audit:read`；只有 `audit:read_own` 時固定為自己的操作（`meta.own_only: true`） | `actor`、`category`、`action`（完全相同，或以 `.` 結尾為前綴：`user.`）、`target`（對象片段，不分大小寫）、`result`、`from`／`to`（RFC 3339）、`limit`（1～1000，預設 100）、`cursor`（新到舊，`page.next_cursor`）。每筆含 `prev_hash`、`hash` |
| `GET /api/v1/audit/export` | `audit:export` | 同樣的篩選，`application/x-ndjson`：每行一筆（`type: "event"`，另有 `at_ms`、`details_raw`，可只靠該行重新計算雜湊；0.7 另有 `hmac`），最後一行 `type: "chain"`（`count`、`first_id`、`first_prev_hash`、`last_id`、`last_hash`；CLI 的匯出另有 `hmac_since`）。只以時間篩選時是連續的一段鏈，可整段驗證 |
| `POST /api/v1/audit/verify` | `audit:read` | 重新計算整條鏈與 HMAC：`ok`、`checked`、`first_id`、`last_id`、`last_hash`、`broken`（第一個斷點的 `id`、`reason`：`hash_mismatch` 內容被修改／`prev_mismatch` 前一筆被刪除或順序被改／`missing_hash`／`anchor_mismatch`／`head_mismatch` 最後幾筆被刪除／0.7 的 `missing_hmac`、`hmac_mismatch`、`hmac_boundary_mismatch`、`hmac_key_unavailable`、`hmac_key_mismatch`；`product_code: KRS-E6201`）、`hmac`（`status`：`not_started`／`verified`／`key_unavailable`／`key_mismatch`／`failed`，`since_id`、`key_id`、`verified`、`chain_only`）。斷裂時 Kairos 記 `ERROR`「錯誤 KRS-E6201：…」 |

沒有修改或刪除的端點（`PUT`／`DELETE` 回 405）；CLI 也沒有。

### 12.9 CLI

```
kairos users create <帳號> --role <角色>[@group:<群組>] [--role …] [--display-name <名稱>] [--email <Email>] [--password-stdin]
kairos users list [--all] | show <帳號> | sessions <帳號> | groups
kairos users grant <帳號> --role <角色> | revoke-role <帳號> --role <角色> [--yes]
kairos users disable <帳號> [--reason <原因>] [--yes] | enable <帳號> | unlock <帳號>
kairos users delete <帳號> [--yes]
kairos users reset-password <帳號> [--password-stdin] [--yes] | reset-mfa <帳號> [--yes]
kairos users end-sessions <帳號> [--reason <原因>] [--yes]
kairos users map-group <群組 DN> --role <角色> | unmap-group <群組 DN> --role <角色>
kairos audit list [--actor …] [--category …] [--action …] [--target …] [--result …] [--since 24h | --from … --to …] [--limit N]
kairos audit verify
kairos audit export [--out <檔案>] [--since … | --from … --to …]
kairos api-token create --name <名稱> [--role <角色> | --user <帳號> [--role <角色>]] [--expires-in …]
```

- **首位管理員**（沒有預設帳號與密碼）：`sudo -u kairos /opt/kairos/kairos users create admin --role admin --config /opt/kairos/kairos.toml`。產生的一次性密碼**只在 stdout 顯示這一次**（`PW=$(…)`），第一次登入時必須變更；要自行指定時以 `--password-stdin` 從管線或檔案提供（標準輸入是終端機時拒絕，避免密碼顯示在畫面上；自行指定的密碼不要求第一次登入變更）。Kairos 不需要停止。
- 危險操作（`cli-style.md` §5.4）：`disable`、`revoke-role`、`reset-password`、`reset-mfa`、`end-sessions` 為 L1（互動 `是否繼續？[y/N]`，非互動需要 `--yes`），`delete` 為 L2（互動時輸入帳號，非互動需要 `--yes`）。
- `kairos audit verify`：結束代碼 0 完整、9 斷裂（`KRS-E6201`，三段式寫出第一個斷點）；驗證前補上 0.5 的紀錄的雜湊。`kairos audit export` 不覆寫既有檔案（結束代碼 11），檔案權限 0640。
- 每個變更都寫入稽核雜湊鏈，執行者為作業系統帳號（`SUDO_USER`、`USER`…）。`--json` 的 `schema` 為 `kairos.users.<子命令>/v1`、`kairos.audit.<子命令>/v1`。
- 結束代碼（`cli-style.md` §8）：0 完成、1 資料庫錯誤或找不到、2 用法錯誤、3 設定錯誤、6 被拒絕（最後一位管理員、目錄服務帳號）、9 雜湊鏈斷裂、10 權限不足、11 衝突（帳號已存在、輸出檔已存在、資料庫使用中）。
- `kairos api-token create` 未指定 `--role`／`--user` 時為 `--role viewer`（0.5 的預設是 `read` 範圍，見 CHANGELOG [0.6.0] 破壞性變更）；`--scope` 仍可使用但已棄用（權限凍結在 0.5）。`list` 的「權限」欄顯示 `role:…`、`user:…`、`scope:…`。

### 12.10 設定與資料表

```toml
[accounts]
password_min_length = 12        # 8..=128
lockout_threshold = 5           # 連續失敗幾次鎖定，1..=100
lockout_minutes = 15            # 1..=1440
session_idle_minutes = 30       # 閒置逾時，5..=1440（不可大於絕對逾時）
session_absolute_hours = 12     # 絕對逾時，1..=168
argon2_memory_kib = 19456       # 8192..=1048576
argon2_iterations = 2           # 1..=10
argon2_parallelism = 1          # 1..=8
mfa_issuer = "Kairos"           # 驗證器 App 顯示的發行者

[ldap]
enabled = false
url = "ldaps://dc01.example.gov.tw:636"
fallback_urls = ["ldaps://dc02.example.gov.tw:636"]
starttls = false                # ldap:// 以 StartTLS 升級
allow_plaintext = false         # 允許未加密的 ldap://（不建議）
ca_file = "/opt/kairos/ad-ca.pem"     # 使用 TLS 時必填
bind_dn = "CN=svc-kairos,OU=Service,DC=example,DC=gov,DC=tw"
bind_password_file = "/opt/kairos/ldap-bind.secret"   # 只有服務帳號可讀（0600）
user_base = "OU=Staff,DC=example,DC=gov,DC=tw"
user_filter = "(&(objectCategory=person)(objectClass=user))"   # 預設；排除 AD 的電腦帳號
login_attribute = "sAMAccountName"
display_name_attribute = "displayName"
email_attribute = "mail"
group_attribute = "memberOf"
timeout_secs = 10               # 1..=60

[audit]
retention_days = 1095           # 稽核紀錄保留天數，90..=36500（獨立於 report）
```

所有設定都只在 `kairos.toml`（與環境變數）中；需要重新啟動才生效。秘密不寫在設定檔（bind 密碼在另一個檔案；`kairos config show --effective` 只顯示路徑）。

**schema v6**（`store/sqlite.rs` 的 `SCHEMA_V6`）：新增 `users`（帳號、來源、Argon2id 雜湊、加密的 TOTP 秘密、狀態、失敗次數與鎖定、最後登入）、`user_recovery_codes`（HMAC）、`role_assignments`（主體類型 `user`／`ldap_group`、角色、範圍）、`sessions`（`session_hash` 為主鍵、公開 id、方式、目錄服務群組、建立、最後活動、絕對逾時、閒置逾時、IP、User-Agent、結束時間與原因）；`api_tokens` 加 `owner_user_id`、`role`；`audit_events` 加 `prev_hash`、`hash` 與索引 `audit_events_actor`；`hosts` 加 `host_group` 與索引。v5 保留給告警（另一條開發線）：v6 只建立新資料表、並在 v4 既有的資料表加欄位，可以直接接在 v4 之後，也可以接在 v5 之後；`migrate` 依版本號套用，版本號不必連續。工作階段的紀錄在結束或逾時 30 天後清除。

### 12.11 延後項目與待決定

| 項目 | 現況與原因 |
|---|---|
| 目錄服務的定期同步（S17「每 15 分鐘重新確認帳號狀態與群組；AD 停用時立即結束工作階段」） | 群組在登入時讀取；AD 停用的帳號在下次登入時被拒，已登入的工作階段最長到絕對逾時（預設 12 小時）。需要背景以服務帳號查詢所有 LDAP 使用者，延後 |
| 「從目錄服務加入」（S16，搜尋 AD 後預先建立帳號） | 目錄服務帳號在第一次登入時建立；預先指派個人角色要等它登入過一次，或以群組對應 |
| 擁有者是目錄服務帳號的 API token | 只有個人指派的權限（token 沒有登入時的群組） |
| 群組範圍的寫入權限（群組管理員管理其群組內的主機、token、規則；`ia.md` §4.3） | 本版群組範圍只授予主機的讀取；告警（第 13 節）也只對全站範圍的角色開放（群組範圍的維運者看不到告警，整合時選擇的保守做法）。主機的寫入操作（標籤、除役…）與群組內的告警處理延後 |
| 主機群組的管理（S21「主機群組與標籤」） | 群組取自註冊 token；沒有修改群組的 API |
| 雙人覆核（four-eyes）、重新驗證（L3 的「重新輸入密碼」） | 本版沒有 L3 的使用者操作 |
| 稽核外送（syslog／SIEM）與外部錨定 | 另一條開發線 |
| 使用者權限清冊 CSV（S16 稽核員匯出） | `GET /api/v1/users` 的 JSON 已有相同內容 |
| 個人的 API token（S23「我的 API token」） | 本版由管理員以 `owner` 建立 |

## 13. 衝刺 5：告警（草稿）

0.6.0〔草稿〕，**已實作**（`kairos/src/api/alerts.rs`；引擎見 [`ALERTS.md`](ALERTS.md)）。設計包 S06～S11、§24 #9。需要告警引擎（`[alerts] enabled = true`、SQLite），沒有啟動時所有端點回 503 `alerts_unavailable`。

### 13.1 權限（集中標記）與角色

每個端點需要的 `Permission` 只在 `api/alerts.rs` 的 `route` 一處決定；權限與角色的對應在 `kairos/src/rbac.rs`（第 12.1 節，設計包 `ia.md` §4.2 的 S06～S11）。權限不足的 403 帶 `details.permission`（例 `alerts:write`）與 `details.required`（擁有它的最低角色）。驗證與 CSRF 和其他端點相同（第 12.3 節：工作階段的寫入需要 `X-CSRF-Token`）。

| `Permission`（名稱） | 管理員 | 維運者 | 稽核員 | 唯讀 | 端點 |
|---|---|---|---|---|---|
| `AlertsRead`（`alerts:read`） | ✓ | ✓ | ✓ | ✓ | `GET /alerts`、`/alerts/summary`、`/alerts/{event_id}`；SSE 的 `alert.*` |
| `AlertsWrite`（`alerts:write`） | ✓ | ✓ | — | — | `POST /alerts/ack`、`/alerts/{event_id}/ack`、`assign`、`snooze`、`resolve`、`comments` |
| `AlertRulesRead`（`alert-rules:read`） | ✓ | ✓ | ✓ | — | `GET /alert-rules`、`/alert-rules/{rule_id}`、`POST /alert-rules/preview` |
| `AlertRulesWrite`（`alert-rules:write`） | ✓ | ✓ | — | — | `POST /alert-rules`、`PUT /alert-rules/{rule_id}`、`enable`、`disable`、`DELETE /alert-rules/{rule_id}` |
| `AlertRulesAdmin`（`alert-rules:admin`） | ✓ | — | — | — | `PUT /alert-rules/{rule_id}` 修改**系統預設規則**的 `params`（S09：只有管理員） |
| `SilencesRead`（`silences:read`） | ✓ | ✓ | ✓ | ✓ | `GET /silences`、`/silences/{silence_id}`、`POST /silences/preview` |
| `SilencesWrite`（`silences:write`） | ✓ | ✓ | — | — | `POST /silences`、`PUT /silences/{silence_id}`、`POST /silences/{silence_id}/end` |
| `ChannelsRead`（`channels:read`） | ✓ | ✓ | — | — | `GET /channels`、`/channels/{channel_id}`（機密一律只有遮蔽值；S11：稽核員與唯讀看不到） |
| `ChannelsTest`（`channels:test`） | ✓ | ✓ | — | — | `POST /channels/{channel_id}/test` |
| `ChannelsWrite`（`channels:write`） | ✓ | — | — | — | `POST /channels`、`POST /channels/test`（未儲存的設定）、`PUT`、`DELETE /channels/{channel_id}` |

- 告警的權限**只由全站範圍的角色指派**授予：群組範圍（`operator@group:…`）的指派只授予該群組主機的讀取（第 12.1 節），看不到告警（最保守的做法；群組範圍的告警屬於延後項目，第 12.11 節）。
- **0.4／0.5 的 API token（`read`／`operate`／`admin` 範圍）升級後沒有告警的權限**（權限凍結在升級前，第 12.1 節）：需要告警的整合以 `kairos api-token create --role viewer|operator|admin` 建立新 token。它們的 SSE 也收不到 `alert.*`。
- SSE 的 `alert.*` 只送給全站範圍有 `alerts:read` 的身分，每次心跳（第 6 節）重新計算權限：撤銷、工作階段結束、停用或降權後不再推送。

寫入的本文為 JSON（上限 256 KiB）；每個寫入都寫入稽核雜湊鏈（`audit_events`，第 12.8 節；類別 `alerts`，例 `alert.ack`、`silence.create`、`channel.update`；執行者 `user:<帳號>` 或 `token:<token_id>`），**不記錄機密的值**（管道只記更換了哪些機密欄位）。危險操作：停用系統預設規則需要 `reason`（L1）；刪除管道為 L2（`confirm_name` 等於管道名稱，有規則使用時另需 `replacement_channel_id` 或 `accept_no_channel: true`）；大範圍與包含緊急等級的靜音由主控台以 `preview` 的 `confirm` 決定是否顯示 L1 確認（伺服器不要求額外欄位，與第 11.1 節的 L1 相同）。

新增的錯誤碼：

| HTTP | `code` | `product_code` | 何時 |
|---|---|---|---|
| 400 | `rule_invalid` | `KRS-E5001` | 規則無法儲存（`details.parameter`；例：恢復門檻在觸發門檻的另一側、緊急等級沒有升級對象、系統規則的嚴重度低於預設） |
| 400 | `silence_too_long` | — | 臨時靜音超過上限（`details.max_secs`、`requested_secs`、`system`、`emergency`；D26，伺服器端驗證） |
| 400 | `channel_invalid` | `KRS-E5110` | 管道的設定無效（`details.parameter`） |
| 400 | `confirmation_required` | — | 停用系統預設規則沒有 `reason`；刪除管道沒有確認（`details.rules` 為受影響的規則） |
| 404 | `event_not_found`、`rule_not_found`、`silence_not_found`、`channel_not_found` | — | 對象不存在 |
| 409 | `invalid_state` | — | 事件已解決（只能新增註解）、已結束的靜音不能修改 |
| 409 | `not_acknowledgeable` | — | 規劃提醒與紀錄不需要確認、暫停通知、解決（`alerts-ux.md` §2.3） |
| 409 | `builtin_rule` | — | 系統預設規則不可刪除 |
| 503 | `alerts_unavailable` | — | 告警引擎沒有啟動 |

### 13.2 事件（S06、S07）

```
GET /api/v1/alerts?view=active&severity=emergency,critical&limit=100
```

| 參數 | 說明 |
|---|---|
| `view` | `active`（預設：觸發中＋已確認，需要處理）、`needs_me`（未確認的觸發中，加上已恢復但未確認的緊急事件）、`recovered`（已恢復待結案）、`all`（含規劃提醒與紀錄、已解決） |
| `severity`、`state`、`category`、`rule` | 逗號分隔，同一參數取「或」、不同參數取「且」 |
| `agent_id`、`q`（事件編號、標題、對象）、`from`、`to`（RFC 3339，開始時間） | |
| `include_children` | 預設只列頂層（彙總事件的子事件只在詳情列出） |
| `limit`、`cursor` | 第 3 節；排序固定為嚴重度 → 未確認優先 → 開始時間由新到舊 |

每個事件：`event_id`、`rule_id`、`kind`、`category`、`severity`、`attention`（`action`／`advisory`／`record`）、`attention_text`、`state`、`acked`、`title`、`subject`、`agent_id`、`code`、`parent_id`、`silenced`、`in_maintenance`、`flapping`、`started_at`、`updated_at`、`recovered_at`、`resolved_at`、`acked_at`、`acked_by`、`assignee`、`eta_at`、`root_cause`、`root_cause_label`、`resolve_note`、`snooze_until`、`occurrences`、`reopen_count`、`details`（主機名稱、IP、網段、中斷原因、數值等）、`notifications`（`sent`、`last_sent_at`、`escalated`）。`meta.counts`：`active`、`active_by_severity`、`unacknowledged`、`badge`（SideNav 徽章：觸發中、需要處理、頂層、未靜音）、`needs_me`、`recovered`、`silenced`；`meta.engine`：`running`、`last_tick_at`、`last_tick_duration_ms`、`warmup`（`active`、`until`、`expected_hosts`、`missing_hosts`）、`bus_dropped`、`last_error`（S06「告警引擎未在執行」）。

`GET /api/v1/alerts/summary`：`counts`、`silences`（`active` 個數、`hosts` 以主機為範圍的靜音涵蓋的台數，S06「目前有 2 個靜音規則生效，影響 301 台主機」）、`engine`。

`GET /api/v1/alerts/{event_id}`：另有 `timeline`（處理紀錄：`fired`、`notified`、`acknowledged`、`assigned`、`snoozed`、`commented`、`recovered`、`reopened`、`escalated`、`absorbed`、`detached`、`flapping`、`stable`、`eta_expired`、`resolved`）、`children`（`total`、最多 100 筆）、`parent`（`banner`：「此事件由系統事件『…』造成，請先處理該事件」）、`deliveries`（通知發送狀態：管道、種類、`state`〔`pending`／`held`／`sent`／`failed`／`dropped`／`merged`〕、`attempts`、`response_code`、`error_code`、`error`）、`rule`、`actions`（目前可做的動作）、`link`。

| 方法與路徑 | 本文 | 說明 |
|---|---|---|
| `POST /alerts/{id}/ack` | `assign_to_me`、`eta_minutes`（1～1440） | 確認（重複確認不是錯誤） |
| `POST /alerts/ack` | `event_ids`（1～1,000） | 批次確認：`results[]`、`summary`（`acknowledged`、`skipped`〔規劃提醒與紀錄〕、`failed`）、`message`（「已略過 N 筆不需要確認的事件」） |
| `POST /alerts/{id}/assign` | `assignee` | 指派（不視為確認） |
| `POST /alerts/{id}/snooze` | `hours`（1～24） | 暫停通知（只停止這個事件的重送） |
| `POST /alerts/{id}/resolve` | `root_cause`（必填）、`note` | 解決；回 `detached_children`（脫離成為獨立事件的子事件數） |
| `POST /alerts/{id}/comments` | `text`（1～2000） | 新增註解（已解決的事件也可以） |

### 13.3 規則（S08、S09）

`GET /api/v1/alert-rules`（篩選 `kind`、`category`、`severity`、`enabled`）：每條規則另有 `max_severity`（依天數升級者為最高等級）、`attention`、`scope_text`、`notify_effective`（套用嚴重度預設後的重送、恢復、升級）、`fires_7d`、`noise_7d`（7 天內 10 分鐘內自行恢復且未確認的比例）、`noisy`（> 50% 且至少 5 次）。

`POST /api/v1/alert-rules`：`kind`（`host_offline`、`host_long_offline`、`report_stale`、`metric_threshold`、`identity_conflict`）、`name`、`severity`、`params`（`metric_threshold`：`metric`〔`cpu`／`memory`〕、`op`、`threshold`〔0～100〕、`for_secs`、`clear_threshold`、`clear_for_secs`）、`scope`（`tags`、`subnets`、`hosts`）、`channels`、`notify`（`repeat_interval_secs`、`repeat_max`、`notify_recovery`、`escalate_after_secs`、`escalate_channels`）、`description`、`enabled`。回 201。

`PUT /api/v1/alert-rules/{rule_id}`：自訂規則可以修改所有欄位（種類除外）；系統預設規則可以修改 `channels`、`notify`、`description`、`severity`（系統類不可低於預設），`params` 只能改預設中已有的數值且需要 `alert-rules:admin`。

`POST /api/v1/alert-rules/preview`（歷史試算，S09「以過去 7 天的資料試算」）：本文為 `rule_id` 或未儲存的 `kind`／`params`／`scope`，另有 `days`（1～31，預設 7）。回 `fires`、`hosts_involved`、`self_recovered_within_10m`、`noise_ratio`、`noisy`、`suggestion`（雜訊偏高時建議的持續時間與試算次數）、`per_day[]`、`examples[]`、`data_days`、`resolution_secs`（5 分鐘彙總；範圍超過 2,000 台時 1 小時）、`approximate: true`、`notes[]`。主機離線類規則回 `supported: false`（沒有連線歷史）。

### 13.4 靜音與維護時段（S10）

`POST /api/v1/silences/preview`：本文 `kind`、`scope`、`rules`、`from_event`。回 `matching_hosts`（使用中的主機）、`scope_text`（「範圍：標籤為 產線，且網段為 10.20.3.0/24 或 10.30.3.0/24」）、`rules_text`、`limit`（`max_secs`、`max_text`、`system`、`emergency`、`reason`：快捷時長下方的說明）、`confirm`（`required`、`reasons[]`）。

`POST /api/v1/silences`：`kind`（`silence`／`maintenance`）、`name`、`reason`（必填）、`scope`、`rules`（不選＝所有規則）、`starts_at`（預設現在）、`ends_at` 或 `duration_secs`、`notify_on_end`（預設 true）、`exclude_uptime`、`from_event`。超過上限回 400 `silence_too_long`。`PUT` 修改尚未結束的靜音（重新驗證上限）；`POST /silences/{id}/end`（`reason`）提前結束。`GET /api/v1/silences?status=active|upcoming|ended|all`：每筆另有 `status`、`scope_text`、`scope_short`、`matching_hosts`、`remaining_secs`。

### 13.5 通知管道（S11）

`POST /api/v1/channels`：`name`、`kind`（`email`／`webhook`）、`enabled`、`config`（不含機密）、`secrets`（機密）。

| `kind` | `config` | `secrets` |
|---|---|---|
| `email` | `host`、`port`、`security`（`starttls`／`tls`／`none`）、`username`、`from`、`to[]`（1～50）、`helo` | `password` |
| `webhook` | `format`（`kairos`／`slack`／`teams`／`line`）、`method`（`POST`／`PUT`）、`allow_http`、`line_to` | `url`、`headers`（物件）、`hmac_secret`、`line_token` |
| 共用 | `timeout_secs`（1～60，預設 10）、`timezone`、`show_ip`（`full`／`subnet`／`none`）、`min_severity`、`notify_ack`、`ca_file` | |

回應的 `secrets` 只有遮蔽值（`url` 為 `https://主機/…`，其餘 `••••`）。`PUT` 時沒有提供的機密欄位沿用原本的值、值為 `null` 時刪除。列表與詳情另有 `state`（`ok`／`failed`／`secrets_unavailable`／`disabled`；`secrets_unavailable`〔0.7〕：機密無法解開——資料目錄的 `alerts-secrets.key` 遺失或與資料庫不成對——`last_error_code` 為 `KRS-E5110`，不會嘗試發送，「通知管道失敗」系統規則以 `KRS-E5110` 開事件；還原金鑰檔並重新啟動，或重新輸入全部機密後恢復）、`consecutive_failures`、`failed_since`、`last_attempt_at`、`last_success_at`、`last_error_code`、`last_error`（已移除機密）、`rules_using`；詳情另有 `rules[]` 與最近 20 筆 `deliveries`。

`POST /api/v1/channels/{id}/test`（`severity`，預設 `emergency`）：以真實範本與假資料送出，回 `ok`、`product_code`、`message`、`response_code`、`elapsed_ms`，並記在發送紀錄。

**限速（0.7〔草稿〕）**：測試通知會真的送到值班群組或信箱。同一個管道 60 秒內最多 3 次（不分使用者）、同一個使用者 10 分鐘內最多 10 次（不分管道，含 `POST /api/v1/channels/test` 的未儲存管道）。超過時回 429 `rate_limited`（`KRS-W5111`）、`Retry-After`，`details`：`scope`（`channel`／`user`）、`limit`、`window_secs`；不送出、不寫發送紀錄。計數只在記憶體中（重新啟動後歸零）。

### 13.6 即時推送（SSE）

第 6 節的 `/api/v1/events` 另有 `alert.created`、`alert.updated`、`alert.resolved`（`data` 為事件的摘要，欄位同第 13.2 節但沒有 `details`），編號與重連規則相同。只有內容（數值、台數）改變時不推送，最多每 60 秒寫回一次。

## 14. 網頁介面與安全標頭（0.7.0〔草稿〕）

主控台的監聽同時提供網頁介面（審計 F2；實作 `kairos/src/api/web.rs`，前端與慣例見 [`web/README.md`](web/README.md)）。網頁的檔案在編譯時嵌入執行檔（`kairos/build.rs`，依路徑排序、內容只取決於檔案本身，不影響可重現性），不讀取檔案系統。

| 路徑 | 回應 |
|---|---|
| `/api/…` | 本文件的 API |
| `/healthz`、`/readyz`、`/metrics` | 健康檢查（README「健康檢查與 Prometheus」）；`observability.on_console = false` 時為 JSON 的 404 |
| `/assets/<檔案>` | 嵌入的檔案，`ETag`（內容的 SHA-256 前 16 bytes）、`Cache-Control: no-cache`，`If-None-Match` 相符時 304；不存在時 404 |
| 最後一段含 `.` 的其他路徑 | 404（純文字）；第一段是主控台的路由、`.` 在第二段以後的參數時例外（`/admin/users/alice.w` 回 `index.html`） |
| 其他路徑（`/`、`/login`、`/hosts/<agent_id>/metrics`…） | 單頁應用的 `index.html`，`Cache-Control: no-store`；畫面由瀏覽器端依網址決定（不需要驗證：資料一律經由需要驗證的 API）。第一段不是主控台的路由（`login`、`hosts`、`account`、`alerts`、`enrollment`、`certificates`、`audit`、`reports`、`admin`、`system`、`setup`、`error`、`docs`、`help`、`t`；設計包 `ia.md` §3）時本文同樣是 `index.html`（瀏覽器顯示 404 頁），但狀態碼為 **404**：例如 Pulse 的 `--enroll-server` 誤設為主控台的連接埠時，`/v1/enroll/server-info` 得到 404 而判定為不支援自動註冊（`PLS-E2106`） |

- 只接受 `GET`／`HEAD`（其他 405，`Allow: GET, HEAD`）。
- **每個回應**（API、健康檢查、網頁）都帶：`Strict-Transport-Security: max-age=31536000`（主控台只有 HTTPS；不加 `includeSubDomains`）、`X-Content-Type-Options: nosniff`、`X-Frame-Options: DENY`、`Referrer-Policy: no-referrer`、`Cross-Origin-Opener-Policy: same-origin`、`Cross-Origin-Resource-Policy: same-origin`、`Permissions-Policy`。
- **Content-Security-Policy**：網頁（`index.html`）為 `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; require-trusted-types-for 'script'; trusted-types 'none'`（不允許 inline script、`eval`、inline style；Trusted Types 讓瀏覽器拒絕 `innerHTML` 等 DOM XSS 的入口）；其他回應為 `default-src 'none'; frame-ancestors 'none'`。
- 網頁介面以工作階段的 cookie 驗證（第 12.3 節），寫入帶 `X-CSRF-Token`；不在瀏覽器的 storage 保存 token。
- 網頁的請求不計入 `kairos_api_requests_total`（只計 API）。
