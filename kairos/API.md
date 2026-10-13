# Kairos 主控台 API（v1）

本文件是 Kairos 主控台與外部整合使用的查詢與管理 API 的規格（商業審計 F1；設計包 `kairos/screens.md` §24 #1、#3、#4、#5、#19 的後端部分）。0.4.0〔草稿〕起提供第一版：**唯讀**的主機清冊、單台詳情、指標趨勢、總覽 KPI、系統健康與即時變更推送（SSE），以 API token 驗證。使用者、角色與工作階段（#10、F6）屬於之後的版本，本版預留了接入的位置（第 4.4 節）。

- 設定見第 2 節，驗證見第 4 節，端點一覽見第 5 節。
- 實作：`kairos/src/api/`（HTTP 伺服器、驗證、各端點）、`kairos/src/token_cli.rs`（`kairos api-token`）。
- 量測（1 萬台主機的列表、30 天趨勢、與 report 寫入同時進行時的 `ack` 延遲）見第 9 節。

## 1. 總則

| 項目 | 規則 |
|---|---|
| 傳輸 | 只有 HTTPS（TLS 1.2／1.3，rustls＋ring，與 agent 連線埠相同的加密後端），HTTP/1.1。不提供明文 HTTP |
| 連接埠 | `console.listen`，預設 `127.0.0.1:9443`（設計包 `ia.md` §3.2：初始化期間只綁 loopback；首次啟動精靈完成後建議改為 `0.0.0.0:9443`，本版沒有精靈，由管理者手動修改）。與 agent 連線埠（`server.listen`）分開，防火牆只對管理網段開放 |
| 版本 | 路徑前綴 `/api/v1`。同一個大版本內只新增欄位與端點，不刪除、不改型別；讀取端應忽略不認得的欄位 |
| 格式 | 請求與回應都是 JSON（UTF-8），`Content-Type: application/json; charset=utf-8`；SSE 為 `text/event-stream` |
| 時間 | RFC 3339、**UTC**、`Z` 結尾、毫秒，例：`2026-10-13T06:03:27.123Z`（與 `hello_ack` 的 `server_time` 相同，設計包 §24 #23）。持續時間以數字表示並在欄位名稱帶單位（`_ms`、`_secs`），容量為 `_bytes` |
| 伺服器時間 | **每個回應**（含錯誤）的最外層都有 `server_time`：Kairos 產生回應時的系統時間，讓主控台計算相對時間（「3 分鐘前」）時不依賴瀏覽器的時鐘 |
| 方法 | 本版只有 `GET`（`HEAD` 視同 `GET` 但不回本文）。其他方法回 405（`Allow: GET, HEAD`） |
| 快取 | 所有回應 `Cache-Control: no-store` |
| CORS | **預設關閉**：不回任何 `Access-Control-*` 標頭，瀏覽器的跨來源請求一律失敗。主控台日後與 API 同源提供，不需要 CORS |
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
| 401 | `unauthorized` | 〔待分配〕 | 沒有 `Authorization`、格式錯誤、token 不存在、已撤銷或已過期（對外不區分），附 `WWW-Authenticate: Bearer realm="kairos"` |
| 403 | `forbidden` | 〔待分配〕 | token 的範圍不包含這個操作（本版的端點都只需要 `read`，保留給之後的寫入操作） |
| 404 | `not_found` | — | 沒有這個路徑 |
| 404 | `host_not_found` | — | 清冊中沒有這個 `agent_id`（設計包 S05「主機不存在 → 404 頁」） |
| 404 | `metric_not_found` | — | 這台主機在查詢範圍內沒有這個指標 |
| 405 | `method_not_allowed` | — | 不是 `GET`／`HEAD` |
| 413 | `payload_too_large` | — | 請求帶有本文（本版的端點都不接受本文） |
| 429 | `rate_limited` | — | 同一來源驗證失敗太多次（第 4.3 節），附 `Retry-After`（秒） |
| 503 | `too_many_streams` | — | SSE 的同時連線數已達 `console.max_streams`，附 `Retry-After` |
| 503 | `storage_unavailable` | `KRS-E4003` | 資料庫查詢失敗（例如磁碟 I/O 錯誤、資料庫被刪除）；主控台顯示「無法載入○○」 |
| 503 | `shutting_down` | — | Kairos 正在關機 |
| 500 | `internal` | `KRS-E4003` | 未預期的錯誤（日誌中有細節；回應不含內部資訊） |

`unauthorized`／`forbidden` 尚沒有產品代碼：`copywriting.md` §3.3 的 6001（帳號或密碼不正確）是給登入畫面的，不適用於 API token；需要設計分配（第 10 節）。

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

## 4. 驗證（暫行：API token）

本版只有 **API token**：給主控台開發、整合（Grafana、腳本）與自動化使用。完整的使用者、角色與工作階段（設計包 §24 #10、D25；審計 F6）在之後的版本加入。

### 4.1 token

```
Authorization: Bearer kat_3f9c2a71b4e05d18_Qm9yZW1hbi1pcy1ub3QtYS1yZWFsLXRva2VuLWV4YW1wbGU
```

- 格式：`kat_` ＋ 16 個十六進位字元的 **token_id**（公開的識別碼，用於列表、撤銷與日誌）＋ `_` ＋ 43 個 base64url 字元的**秘密**（32 bytes 隨機值，以作業系統的 CSPRNG 產生）。`kat_` 前綴讓 secret 掃描工具容易辨識。
- 伺服器只保存秘密的 **SHA-256 雜湊**（`api_tokens.secret_hash`）。秘密有 256 位元的亂度，不需要慢速雜湊（bcrypt／Argon2 是為了低亂度的密碼）。比對時先以 token_id 查詢，再以**常數時間**比較雜湊；token_id 不存在時仍做一次同樣的比較，回應時間不洩漏 token_id 是否存在。
- token 只在建立時顯示一次，之後無法取回；遺失時撤銷後重新建立。
- **範圍**（`scope`）：本版只有 `read`（唯讀：所有 `GET` 端點）。之後的寫入端點會要求 `write` 等範圍，`read` 的 token 回 403。
- **到期**：建立時指定，預設 90 天；`--no-expiry` 建立不過期的 token（不建議）。到期後回 401。
- **最後使用**：成功驗證時在記憶體中記下時間與來源 IP，每 60 秒以一個短交易寫回資料庫（`last_used_at`、`last_used_ip`；`kairos api-token list` 顯示）。
- 回應、日誌與 `kairos api-token list` 都不會出現秘密或雜湊；日誌只記 token_id 與名稱。

### 4.2 `kairos api-token`

```
kairos api-token create --name <名稱> [--expires-in <時間> | --expires-at <日期或時間> | --no-expiry] [--scope read] [--json] [--config <檔案>]
kairos api-token list [--all] [--json] [--config <檔案>]
kairos api-token revoke <token_id> [--reason <原因>] [--yes] [--json] [--config <檔案>]
```

輸出依設計包 `design-system/cli-style.md`（結果在 stdout、過程與錯誤在 stderr、`--json` 為純 ASCII 的單一文件）：

```console
$ sudo -u kairos /opt/kairos/kairos api-token create --name grafana --expires-in 180d --config /opt/kairos/kairos.toml
kat_3f9c2a71b4e05d18_Qm9yZW1hbi1pcy1ub3QtYS1yZWFsLXRva2VuLWV4YW1wbGU
✓ 已建立 API token「grafana」（token_id 3f9c2a71b4e05d18，範圍 read，到期 2027-04-11 06:03 UTC）
  這是唯一一次顯示 token，請立即保存到密碼管理工具；遺失時撤銷後重新建立。
  使用方式：Authorization: Bearer <token>（主控台 https://<主機>:9443/api/v1/）
```

- stdout 只有 token 一行（`TOKEN=$(kairos api-token create …)`），其他說明寫 stderr。
- `--expires-in`：`90m`、`12h`、`30d`、`2w`（與 `kairos hosts list --offline-for` 相同的寫法），上限 3650 天；`--expires-at`：`2027-01-01`（UTC 當天 00:00）或 RFC 3339。
- `--name`：1～64 個字元，不可含控制字元；不必唯一（以 token_id 區分）。
- `list`：預設只列出有效的 token；`--all` 另列已撤銷與已過期的。欄位：token_id、名稱、範圍、狀態（有效／已撤銷／已過期）、建立時間與建立者、到期、最後使用（時間與 IP）。
- `revoke`：危險操作 L1（`cli-style.md` §5.4）：互動模式詢問 `是否繼續？[y/N]`，非互動需 `--yes`（否則結束代碼 2）。撤銷立即生效（每個請求都查詢資料庫）。重複撤銷已撤銷的 token 不算錯誤。
- 結束代碼：0 完成、1 資料庫錯誤、2 用法錯誤、3 設定錯誤、10 權限不足（以 root 執行而資料目錄屬於服務帳號時拒絕，與 `kairos reports import` 相同）、11 資料庫被 `kairos reports import` 獨占中。
- 以服務帳號執行（`sudo -u kairos`），Kairos 執行中也可以（只寫一個短交易）；資料庫不存在時建立（並升級 schema）。

**稽核**：建立與撤銷都寫入 `api_tokens`（建立者與撤銷者為執行命令的作業系統帳號：`SUDO_USER`、`USER`、`LOGNAME` 依序取第一個有值的，另記 uid；撤銷原因），並由執行中的 Kairos 在 **10 秒內**寫入 Kairos 的日誌（`INFO`「API token 已建立／已撤銷：token_id …、名稱 …、範圍 …、到期 …、建立者 …」；Kairos 未執行時在下次啟動時寫入）。完整、不可竄改的稽核雜湊鏈（設計包 §24 #11）屬於之後的版本。

### 4.3 失敗的速率限制

- 同一來源（IPv4 位址；IPv6 以 /64 合併，與 agent 連線埠的新連線速率相同）**驗證失敗**時扣額度：可連續 10 次，之後每 6 秒恢復 1 次（每分鐘 10 次）。額度用完時該來源的**所有請求**回 429 `rate_limited`（`Retry-After` 為恢復 1 次所需的秒數），不再檢查 token，直到額度恢復。成功的請求不扣額度。
- 每個來源第一次被限制時記一行 `WARN`（節流），不記錄 token 內容。
- 記憶體中最多追蹤 10,000 個來源，超過時清掉最久沒有失敗的來源。

### 4.4 之後接入工作階段與 RBAC 的位置

- 每個請求先經過 `api::auth::authenticate`，得到 `Principal`（目前只有 `Principal::ApiToken { token_id, name, scope }`）。之後加入 `Principal::Session { user, roles, scopes }`（cookie＋CSRF token，D25 的「結束某使用者全部工作階段」以 `sessions` 表處理），端點不需修改。
- 端點以 `principal.require(Permission::HostsRead)` 檢查權限；目前 `read` 範圍包含所有讀取權限，日後角色（管理員、維運者、稽核員、唯讀，`ia.md` §4.1）對應到一組 `Permission`。
- 群組範圍（`ia.md` §4.3）：主機清冊的篩選在 `HostIndex::query` 以 `principal.host_filter()` 加上一層條件，總數與 KPI 也只算範圍內的主機。本版沒有群組，`host_filter()` 一律為全部。
- `GET /api/v1/me` 回傳目前的身分，主控台以它決定顯示哪些功能（權限差異）。

## 5. 端點

| 方法與路徑 | 用途（設計包畫面） | 說明 |
|---|---|---|
| `GET /api/v1/me` | 全部 | 目前的身分與範圍（第 5.0 節） |
| `GET /api/v1/hosts` | S04 主機清單、全域搜尋、S03 連結 | 清冊列表：篩選、排序、游標分頁、總數（第 5.1 節） |
| `GET /api/v1/hosts/{agent_id}` | S05 主機詳情 | 單台詳情（第 5.2 節） |
| `GET /api/v1/hosts/{agent_id}/metrics` | S05 趨勢分頁 | 這台主機目前回報的指標（名稱、單位、label 組合） |
| `GET /api/v1/hosts/{agent_id}/metrics/{name}` | S05 趨勢、S03 sparkline | 指標趨勢（第 5.3 節） |
| `GET /api/v1/overview` | S03 總覽 | 各狀態台數、上線數 24 小時、離線最久、資源使用前 10（第 5.4 節） |
| `GET /api/v1/system/health` | S20 系統健康、S03 健康條 | 檢查項目與數值（第 5.5 節） |
| `GET /api/v1/events` | S04「有 12 台狀態變化」、S03 自動更新 | 即時變更推送（SSE，第 6 節） |

### 5.0 `GET /api/v1/me`

```json
{ "server_time": "…", "data": {
  "type": "api_token", "token_id": "3f9c2a71b4e05d18", "name": "grafana",
  "scopes": ["read"], "expires_at": "2027-04-11T06:03:27.000Z"
} }
```

### 5.1 `GET /api/v1/hosts`

**主機狀態**依設計包 `ia.md` §5.1 的 token。本版能判定的是 `online`、`offline`、`unknown`；`critical`、`warning`（需要告警，F3）與 `pending`（需要註冊流程，F5）尚無資料，篩選時不會符合任何主機，計數為 `null`（不是 0，設計包 S03「無法取得時不可顯示 0」）。

| 狀態 | 判定 |
|---|---|
| `online` | 目前有連線、通過 hello，且 `last_seen` 在離線門檻（`offline_after_ms`）內 |
| `offline` | 曾經上線，現在沒有連線或超過離線門檻 |
| `unknown` | 清冊中有、但從未連線到這個 Kairos（由 `kairos reports import` 匯入的主機，`status_detail.never_connected: true`）；或 Kairos 啟動後 2 分鐘內、這台主機在啟動前已離線且尚未連上（`status_detail.starting: true`，設計包「Kairos 剛重新啟動且暖機期內」） |

**查詢參數**（同一參數多個值以逗號分隔，取「或」；不同參數取「且」，與設計包 D23 的篩選規則相同）：

| 參數 | 例 | 說明 |
|---|---|---|
| `status` | `offline,unknown` | 主機狀態 token（六種之一，否則 400） |
| `never_connected` | `true` | 從未連線（`unknown` 的一種） |
| `offline_for` | `3d` | 失聯（不是 `online`）且 `last_seen` 至少在這麼久之前（`90m`、`12h`、`3d`、`2w`；設計包「失聯超過 N」、`kairos hosts list --offline-for`） |
| `offline_reason` | `certificate_rejected` | 最後中斷原因（`timeout`、`connection_closed`、`server_shutdown`、`certificate_rejected`；第 5.2 節） |
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
  "status_detail": { "offline_reason": "timeout", "never_connected": false, "starting": false, "long_offline": false },
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
  },
  "clock_offset_ms": null
}
```

- `connection`：目前或最近一次連線的投遞資訊（Kairos 重新啟動後尚未連線時為 `null`）。`last_backfill_*`：最近一次收到補送資料的時間與當時的落後量（收到時間 − 取樣時間）。
- `certificate_rejection`：**握手時被拒的憑證歸屬**（設計包 §24 #3、`ia.md` §5.2「憑證被拒」）：最近一次以這台主機的憑證（SAN 為 `agent_id`）連線、在 TLS 握手時被拒的時間、原因與序號，`count` 為同一原因的累計次數。`reason`：`revoked`（`KRS-W1105`）、`expired`（`KRS-W1108`）、`unknown_issuer`（`KRS-W1109`）、`revocation_status_unavailable`（`KRS-E1203`，伺服器端的 CRL 問題）、`other`（`KRS-W1105`）。沒有時為 `null`。之後成功上線的主機仍保留紀錄，但不再算作「憑證被拒」的離線原因（`offline_reason` 只在被拒時間晚於 `last_seen` 時為 `certificate_rejected`）。
- `offline_reason`（第 5.1 節）：`timeout`（超過離線門檻沒有合法訊息）、`connection_closed`（連線關閉：agent 關閉、網路中斷或伺服器關閉連線）、`server_shutdown`（Kairos 關機）、`certificate_rejected`（見上）。只有 Kairos 這次啟動後發生的才知道，其餘為 `null`。設計包 §24 #2 的完整分類（`auth_failed`、`superseded`、寫入結果未知等）與每台主機的連線紀錄屬於之後的版本。
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
- 超過所有保留期限的範圍：`from` 調到 1 小時彙總保留的起點，`notes` 寫明（設計包 S05「只保留最近 400 天的 1 小時彙總」）。範圍最長 3,660 天。

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
    "service": { "version": "0.4.0", "commit": "4223101", "started_at": "…", "uptime_secs": 1036800, "config_file": "/opt/kairos/kairos.toml" }
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
- **系統健康**：`database` 項目為警告並帶 `KRS-W4202`（第 5.5 節）。告警事件（S06）需要告警引擎（F3），屬於之後的版本。

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
data: {"agent_id":"6f1c…","hostname":"hmi-line3-07","display_name":null,"status":"offline","reason":"timeout","at":"…","ip":"10.30.3.27"}

: heartbeat 2026-10-13T06:03:42.000Z

```

| 事件 | 何時 | `data` |
|---|---|---|
| `ready` | 連線建立時（第一則） | `server_time`、`last_event_id`（目前最新的編號）、`resumed`（`Last-Event-ID` 是否成功接續） |
| `host.added` | 清冊中沒有的主機第一次上線（新主機） | 同下，`status` 為 `online` |
| `host.online` | 上線、重新上線、離線後恢復 | `agent_id`、`hostname`、`display_name`、`status`、`at`、`ip` |
| `host.offline` | 離線 | 同上，另有 `reason`（第 5.2 節的 `offline_reason`） |
| `resync` | 要求重新抓取列表（見下） | `reason`：`restarted`（Kairos 重新啟動過）、`too_old`（`Last-Event-ID` 已不在保留範圍）、`overflow`（連線太慢、事件來不及送出） |
| 心跳 | 每 15 秒一行註解（`: heartbeat <時間>`） | 讓代理伺服器與用戶端偵測斷線；不是事件，不改變 `Last-Event-ID` |

- **事件編號**：`<epoch>-<序號>`。epoch 是這次啟動的識別碼（啟動時間的 base36），序號從 1 起遞增。
- **重連的語意**：Kairos 在記憶體中保留最近 **4,096 則**事件。帶 `Last-Event-ID` 重連時，若 epoch 相同且之後的事件都還在保留範圍內，先補送遺漏的事件（`ready.resumed: true`），之後繼續即時推送；否則先送 `resync`（`resumed: false`），用戶端應**重新抓取列表**（`GET /api/v1/hosts`，以回應的 `meta.last_event_id` 作為新的起點），再繼續套用之後的事件。Kairos 重新啟動後一律 `resync`。
- **建議用法**：先開 SSE 收到 `ready`，再取列表；之後只套用編號大於列表 `meta.last_event_id` 的事件（同一台主機的事件依編號順序套用即可，狀態以最後一則為準）。
- **事件只是提示**：狀態可能在事件之後又改變，需要精確值時以列表或詳情為準；設計包 S04「有 12 台狀態變化 · 更新列表」即依事件計數、由使用者決定何時重新抓取。
- **連線數上限**：`console.max_streams`（預設 16）；超過時回 503 `too_many_streams`（`Retry-After: 10`）。每條 SSE 也算在 `console.max_connections` 內。用戶端送出的速度跟不上（伺服器端緩衝超過 256 則）時，送出 `resync` 後從最新的事件繼續。
- 驗證與其他端點相同（`Authorization` 標頭；瀏覽器的 `EventSource` 不能帶標頭，主控台改用工作階段 cookie 後即可使用，本版以 fetch 讀取串流）。token 被撤銷或過期後，已建立的串流在下一次心跳時關閉。
- Kairos 關機時串流直接關閉，用戶端依 `retry` 重連。

## 7. 安全

- TLS 設定沿用 agent 連線埠：rustls（ring），TLS 1.2／1.3、rustls 的預設密碼套件；ALPN 只有 `http/1.1`。主控台不要求用戶端憑證。
- **大小與逾時上限**：請求行與標頭合計最多 16 KiB（超過時中斷連線）；不接受請求本文（`Content-Length` > 0 或 chunked 回 413）；TLS 握手 10 秒、讀取請求標頭 10 秒（含 keep-alive 閒置）、每個請求處理 30 秒（逾時回 503，SSE 除外）。查詢結果的大小由 `limit`（1,000）、`max_points`（5,000）與序列數（64）限制。
- **連線上限**：`console.max_connections`（預設 64）；滿時暫停接受新連線（在 TCP 層排隊），與 agent 連線埠的名額分開，主控台的負載不影響 agent 的連線。
- **讀取不影響寫入**：查詢使用獨立的唯讀連線（WAL 讀者，不擋住 report 的寫入，最多 4 條同時查詢）；主機列表由記憶體中的索引回答（第 9 節）。
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

〔量測完成後補上：1 萬台主機的列表（含篩選、CIDR、排序、分頁）p99、30 天趨勢、與 report 寫入同時進行時的 `ack` p99。〕

## 10. 限制、延後項目與待決定

〔實作完成後補上。〕
