# 變更紀錄

本檔案記錄 Kairos、Pulse 與共用 crate 值得注意的變更，格式依 [Keep a Changelog](https://keepachangelog.com/zh-TW/1.1.0/)。

Kairos 與 Pulse 自 0.2.0 起各自採用 [SemVer](https://semver.org/lang/zh-TW/)：`--version` 印出 `<名稱> <版本> (<commit>)`，發佈包為 `<名稱>-<版本>-<commit>-linux-x86_64.tar.gz`。1.0 之前，破壞性變更升 MINOR（0.2 → 0.3），其餘升 PATCH。0.2.0 之前的建置一律顯示 `0.1.0 (<commit>)`，只能以 commit 區分。版本規則、打 tag 與發佈流程見 [RELEASING.md](RELEASING.md)（只在原始碼 repo 中）。

各項後的代號（例如 S1、A11）對應商業審計的項目編號。

## 版本相容矩陣〔草稿〕

Kairos 與 Pulse 混用版本時的相容性。「0.1.0」泛指 0.2.0 之前以 commit 識別的所有建置，各 commit 之間的細節見各產品 README 的「升級注意事項（與 … 混用版本）」。

**協定版本**由每條連線的 hello 協商（`crates/kairos-proto/PROTOCOL.md`「版本協商」），與產品版本號無關：

| 產品版本 | 支援的協定 | hello／hello_ack |
|---|---|---|
| Kairos 0.1.0 | v1 | 只檢查 hello 的 `proto == 1`，忽略不認得的欄位 |
| Kairos 0.2.0 | v1～v2 | 取 client 宣告的範圍與 1～2 中最高的共同版本；v1 時的 `hello_ack`、report 與錯誤處理與 0.1.0 相同 |
| Pulse 0.1.0 | v1 | `proto: 1` |
| Pulse 0.2.0 | v1～v2 | `proto: 1`（讓 0.1.0 的 Kairos 接受）加上 `proto_min: 1`、`proto_max: 2`、`features: ["metrics"]` 與主機資訊；依 `hello_ack` 的 `proto` 與 `features` 決定 report 格式 |

| Kairos ＼ Pulse | Pulse 0.1.0（協定 v1） | Pulse 0.2.0（協定 v1～v2） |
|---|---|---|
| **Kairos 0.1.0**（協定 v1） | 協定 v1，依舊版說明 | **相容，協定 v1**：Kairos 忽略 v2 欄位、回 `"proto":1`，Pulse 改送三欄位的 report；JSONL 維持舊格式（沒有 `schema`），沒有主機資訊。注意：`572b955` 之前的 Kairos 把 `timeouts.write_secs` 設得大於約 8 秒時，寫檔緩慢期間 Pulse 可能誤判斷線；Pulse 的預設心跳改為 15 秒，離線判定變為約 75 秒（破壞性變更第 3 項） |
| **Kairos 0.2.0**（協定 v1～v2） | **有條件相容，協定 v1**：`hello_ack`、report、錯誤處理與 0.1.0 的 Kairos 相同；JSONL 為 `schema` 1（欄位與舊版相同，只多 `schema`），`agents.json` 的 `proto` 為 1、沒有主機資訊。限制：`44dcee5` 之前的 Pulse 在主機名稱含格式字元（Cf）時、`44dcee5` 在主機名稱含 U+034F、Hangul filler、變體選擇字元或私用區字元時**無法註冊**；舊版 Pulse 的讀取閒置計算方式在心跳較大時可能誤斷線 | **相容（建議組合），協定 v2**：report 為通用指標（JSONL `schema` 2），hello 帶主機資訊（寫入 `agents.json`），`hello_ack` 帶 `server_time`（Pulse 檢查時鐘偏差） |

- 同一個 Kairos 0.2.0 可以同時服務 v1 與 v2 的 Pulse：每個 agent 依自己的連線協商，同一個 `reports-*.jsonl` 中 `schema` 1 與 2 的行並存（讀取工具須依 `schema` 分流，見破壞性變更第 6 項）。
- 0.3.0〔草稿〕的協定版本與 0.2.0 相同（v1～v2），另以 feature `durable` 協商 at-least-once 投遞：Kairos 0.3.0 與 Pulse 0.3.0（啟用本機緩衝，預設）都宣告時才啟用，其他組合與上表的 0.2.0 相同（at-most-once）。Kairos 0.3.0 另改為 SQLite 儲存，讀取資料的工具受影響。各組合見 [0.3.0] 的「相容性」。
- 0.1.0 與 0.2.0 的任何組合都有共同的協定版本，不會出現 `unsupported_version`；它只會在日後某一端不再支援 v1（例如只支援 v3 以上）時出現。
- **升級順序**：協定上任何順序都可以，混用期間自動以 v1 通訊，不必同時升級。仍**建議先升級 Pulse、再升級 Kairos**，原因是上表「Kairos 0.2.0 × Pulse 0.1.0」的限制（舊版 Pulse 的主機名稱與讀取閒置問題）只在先升級 Kairos 時出現；反過來的組合只有第 3 項的離線判定時間改變。資料格式則在 Kairos 升級的那一刻改變（JSONL 開始有 `schema`），與 Pulse 的升級順序無關。從 0.2.0 升級到 0.3.0 時沒有這些限制，**建議先升級 Kairos、再升級 Pulse**（Pulse 升級後的第一次斷線就能補送），步驟見 [0.3.0]。
- 驗證方式：`kairos-proto` 以凍結的 v1 型別定義驗證舊端忽略 v2 欄位；Kairos 整合測試以 v1／v2 client 涵蓋各種協商結果；Pulse 單元測試以記憶體內的 v1／v2 伺服器驗證 report 格式的選擇；端對端測試（`tests/e2e`）以實際的執行檔驗證 0.2.0 × 0.2.0（v2），並以送出 0.1.0 Pulse 訊息的假 client 驗證 0.2.0 的 Kairos × 0.1.0 的 Pulse（v1）。沒有以 0.1.0 的執行檔實測。

## 支援期限〔草稿〕

> 以下為待使用者決定的草稿，數字皆為建議值，尚未對外承諾。

| 項目 | 建議 |
|---|---|
| 支援的版本線 | 最新的 MINOR 版本線（目前 0.2.x）提供錯誤與安全性修正（PATCH） |
| 前一個版本線 | 新的 MINOR 版本發佈後〔待定：建議 6 個月〕內只提供安全性修正，之後終止支援 |
| PATCH 版本 | 同一 MINOR 版本線內不含破壞性變更，可直接升級 |
| Kairos／Pulse 混用 | 保證相鄰兩個 MINOR 版本線可以混用（例如 Kairos 0.3 搭配 Pulse 0.2），以便分批升級〔待定〕 |
| 終止支援通知 | 至少提前〔待定：建議 3 個月〕在本檔案與發佈公告中通知 |
| 平台 | Linux x86_64（musl 靜態連結）、systemd 247 以上；其他平台目前不提供發佈包 |
| 1.0 之後 | 另訂長期支援（LTS）政策〔待定〕 |

## [Unreleased]

（0.4.0 之後的變更記在這裡。）

### 新增（衝刺 4：註冊通道 8444 的規格，尚未實作）

- `crates/kairos-proto/ENROLL.md`：Pulse ↔ Kairos 註冊通道（8444 HTTPS）協定 v1 的唯一規格——端點 `/v1/enroll/server-info`、`token-check`、`requests`（新註冊）、`requests/{request_id}`（輪詢與取回憑證，含長輪詢）、`renewals`（過期寬限期續簽）、撤回的兩段式 challenge／withdraw；錯誤碼與 `KRS-`／`PLS-` 對照、時鐘檢查、三段模式、多層速率限制、同一個 CSR 只簽發一張憑證（設計包 `INTEGRATION.md` §23.9 定案）、8443 的 `renew`／`deregister` 草稿、安全審查項目的處置、實作分工與端對端測試情境。協定不變：0.4 的 Kairos 與 Pulse 不受影響。
- `kairos-proto` 新增 `enroll` 模組：註冊通道的請求與回應型別、欄位驗證、路徑與模式白名單、註冊 token（`KRT1-…`，GF(32) 檢查碼）、安裝字串、CA 指紋、申請編號、簽章內容的正規化與 ECDSA P-256 驗證；單元測試直接解析 ENROLL.md 的錯誤碼表、範例與測試向量。`kairos-proto` 因此依賴 `ring`、`chrono`（兩者原本就在依賴樹中）；測試另用 `rcgen`、`x509-parser`（MIT OR Apache-2.0）。
- `kairos/PKI.md`：自動簽發的 CA 架構（根 CA 離線、Kairos 線上持有代理簽發 CA）、用戶端憑證格式、簽發 CA 的輪替、Kairos 自行產生 CRL、離線簽發模式（CSR 佇列匯出與匯入）、資料庫 schema v4。標出待使用者確認的決定。
- `kairos/API.md` 第 11 節：註冊 token、申請（含批次核准）、註冊端點模式、憑證與 CRL、離線簽發佇列的管理端點；API token 範圍新增 `operate`、`admin`；CLI `kairos enroll-token`、`kairos enroll`、`kairos pki`。

## [0.4.0] - 〔草稿：發佈日期待定〕

〔草稿〕**Kairos 0.4.0、Pulse 0.4.0**：商業審計路線圖「階段 3 MVP 產品面」。Kairos 新增 F1（查詢與管理 API）第一版：HTTPS 的主控台 API（`/api/v1`，唯讀）與 API token，是主控台畫面的前提；Pulse 新增 `pulse status`、所有人可讀的本機狀態檔與日誌的事件代碼。**協定不變**（仍為 v1～v2＋`durable`），兩個產品可以分批、以任何順序升級。`kairos/Cargo.toml`、`pulse/Cargo.toml` 的版本已改為 0.4.0；發佈時依 RELEASING.md 填上日期並移除〔草稿〕標記。API 的規格、端點、驗證、即時推送與量測見 `kairos/API.md`（Kairos 發佈包內的 `API.md`）；Pulse 的錯誤代碼對照見 `pulse/ERROR-CODES.md`（Pulse 發佈包內的 `ERROR-CODES.md`）。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **預設監聽主控台 API `127.0.0.1:9443`**（`[console]`，HTTPS；預設啟用、只綁 loopback） | 9443 已被其他程式使用的主機；與 `server.listen` 使用同一個位址與連接埠的設定 | 升級前 `ss -ltnp \| grep :9443` 確認沒有被使用。被佔用時 Kairos 以結束代碼 11 無法啟動（systemd 每 5 秒重試）：在 `kairos.toml` 加上 `[console]` 的 `listen`（換連接埠）或 `enabled = false`。與 `server.listen` 相同時以 `KRS-E0202` 拒絕啟動（結束代碼 3） |
| 2 | Kairos | **資料庫 schema 升級到 v3**（`api_tokens`、`hosts.cert_rejected_at`／`cert_rejection`） | 所有 SQLite 部署 | 升級前 `kairos db backup`（Kairos INSTALL 第 11 節）；升級後 0.3 無法開啟資料庫（降版從備份還原） |
| 3 | Kairos | **檔案描述元的需要量多了主控台的連線上限**：`server.max_connections` ＋ `console.max_connections`（預設 64）＋ 64（設計包 D21） | 自訂 `LimitNOFILE` 剛好等於舊算式的部署 | 啟動時不足會記 `WARN` 並附建議值；發佈包 unit 的 `LimitNOFILE=8192` 足夠 |
| 4 | Pulse | **本機狀態檔改到 `/var/lib/pulse/status.json`，需要新版 `pulse.service`**（`StateDirectory=pulse`、`StateDirectoryMode=0755`；多實例 `StateDirectory=pulse/N`） | 以發佈包的 systemd unit 執行的部署（`ProtectSystem=strict` 下 `/var/lib/pulse` 不可寫）；要讓一般帳號執行 `pulse status` 的主機 | 換上新版 unit 並 `systemctl daemon-reload`（多實例依 Pulse INSTALL 第 9 節重新產生）；`chmod 751 /opt/pulse`（多實例的 `/opt/pulse-N` 也是）。沒有換 unit 時**服務照常啟動與監控**：狀態檔改寫在 `/opt/pulse/logs/status.json` 並記一次 `WARN`（`status.write_failed PLS-E8204`），`pulse status` 只有 root 能用，「設定」列為警告（結束代碼 8） |
| 5 | Pulse | **日誌訊息開頭加上事件與代碼**（例如 `buffer.full PLS-W4202 …`、`conn.registered - …`），`clock.skew` 超過 5 分鐘改為 `ERROR`；啟動失敗的 stderr 改為 `錯誤 PLS-E0101：…` | 以行首比對 Pulse 日誌文字的告警規則或腳本（例如比對 `[ERROR] 連線中斷`） | 升級前把規則改為比對事件或代碼（`conn.lost`、`PLS-E2002` 等，`pulse/ERROR-CODES.md`），或改用包含比對 |

**升級步驟**（依序；協定不變，也可以先升級 Pulse，Kairos 與 Pulse 0.3／0.4 可以任意混用）：

1. **Kairos 升級前**：`kairos db backup` 備份資料庫（第 2 項），確認 9443 沒有被使用（第 1 項）。
2. **升級 Kairos**：換上新版執行檔並重新啟動（Kairos INSTALL 第 11 節，unit 不需修改）→ 確認日誌有「主控台 API 已啟動，監聽 https://127.0.0.1:9443/api/v1/」。
3. **確認主控台 API**：以 `kairos api-token create` 建立 token（預設 90 天後到期）並依 Kairos INSTALL 第 9.3 節確認；需要從其他主機使用時把 `console.listen` 改為對外位址，並以防火牆**只允許管理網段**。
4. **Pulse 升級前**：依第 5 項調整比對 Pulse 日誌文字的告警規則。
5. **每台 Pulse 主機：換上新版 `pulse.service`**（第 4 項；多實例時對每個實例重新產生）並 `systemctl daemon-reload`；`chmod 751 /opt/pulse`。
6. **升級 Pulse**：換上新版執行檔並重新啟動（Pulse INSTALL 第 10 節），以 `/opt/pulse/pulse status` 確認（結束代碼 0；「設定」列沒有 `PLS-E8204`）。

### Kairos

#### 新增

- **主控台 API**（F1；設計包 `screens.md` §24 #1、#4、#5、#19）：HTTPS（rustls／ring，HTTP/1.1，hyper 1），版本前綴 `/api/v1`，JSON，時間為 RFC 3339 UTC 毫秒，每個回應帶 `server_time`，錯誤為 `{"error":{"code","product_code","message"}}`（`product_code` 對照 `copywriting.md` 的 `KRS-` 代碼），游標分頁。端點：
  - `GET /api/v1/hosts`：主機清冊列表，伺服器端篩選（狀態、從未連線、失聯超過 N、離線原因、tag、OS、版本、是否支援補送、CIDR、主機名稱／顯示名稱／IP／agent_id 搜尋）、排序（嚴重度、名稱、最後上線、IP、版本、CPU、記憶體）、游標分頁、總數與各狀態台數；
  - `GET /api/v1/hosts/{agent_id}`：詳情（清冊欄位、主機指紋、OS、標籤、目前連線、最近一次 report、是否協商補送與補送中、握手時被拒的憑證）；
  - `GET /api/v1/hosts/{agent_id}/metrics[/{name}]`：目前回報的指標、趨勢（依範圍自動選原始資料／5 分鐘／1 小時彙總，也可指定；min／max／avg／count；有 label 的序列分開；點數上限；尚未寫出的彙總由原始資料計算）；
  - `GET /api/v1/overview`：各狀態台數、每分鐘上線數（24 小時）、與 1 小時前的差、離線最久、資源使用前 10、健康摘要；
  - `GET /api/v1/system/health`：資料庫用量與容量上限、以目前回報中的主機與宣告的間隔估算保留期滿的用量、最近一次容量刪除、保留設定與最近一次清除、report 寫入統計（成功、`storage_unavailable`、結果不明、重複、速率限制）、支援補送／補送中的台數、連線使用率、TLS 到期、監聽位址、檔案描述元、驗證模式；
  - `GET /api/v1/events`：即時變更推送（SSE）：`host.added`、`host.online`、`host.offline`（附原因），每 15 秒心跳，`Last-Event-ID` 在保留範圍（4,096 則）內補送、否則 `resync`，同時連線數上限 `console.max_streams`；
  - `GET /api/v1/me`：目前的身分。
- **API 錯誤的產品代碼**（設計包 `copywriting.md` §3.4、`decisions/INTEGRATION.md` §26）：401 `unauthorized` → `KRS-E6010`、429 `rate_limited` → `KRS-W6011`、403 `forbidden` → `KRS-E6104`；`storage_unavailable`／`internal` → `KRS-E4003`；用法錯誤與暫時狀態為 `null`。每個來源第一次被限速時的 `WARN` 日誌為「警告 KRS-W6011：…」。
- **API token**（暫行驗證，F6 之前）：`kairos api-token create|list|revoke`（設計包 `cli-style.md` §11.6；token 只在建立時顯示一次、資料庫只存 SHA-256、唯讀範圍、撤銷立即生效）。有效期限**預設 90 天、上限 3,650 天**（`--expires-in`／`--expires-at`），**不提供不過期的 token**（需要長期使用時 `--expires-in 3650d`）。給人看的時間用本機時區並標示時區（`--json` 為 UTC）；`list` 對 14 天內到期的有效 token 加 `⚠`（ASCII `[WARN]`）並提示輪替。`Authorization: Bearer`；常數時間比對；**驗證失敗依來源 IP 速率限制**（IPv6 以 /64 合併；可連續 10 次，之後每 6 秒恢復 1 次，額度用完回 429）；最後使用的時間與來源每 60 秒寫回；建立與撤銷由執行中的 Kairos 寫入日誌。
- **狀態暖機期**（設計包 `ia.md` §5.1）：Kairos 啟動後 2 分鐘內，最後一次訊息在這次啟動之前、還沒連上的主機為 `unknown`（`status_detail.starting`），之後仍未連上改為 `offline`。
- **設定 `[console]`**：`enabled`（預設 `true`）、`listen`（預設 `127.0.0.1:9443`）、`cert_path`／`key_path`（預設沿用伺服器憑證，熱重載）、`max_connections`（64）、`max_streams`（16）。
- **握手時被拒的憑證歸屬到主機**（設計包 §24 #3）：已撤銷、已過期（或無法判定撤銷狀態）的客戶端憑證以 SAN 歸屬到 `agent_id`，記錄原因（`KRS-W1105`／`W1108`／`E1203`）、序號與來源位址，寫入主機清冊；主機詳情與列表的離線原因顯示「憑證被拒」。不受信任的 CA 簽發的憑證不歸屬（SAN 可偽造）。
- **`KRS-W4202`**（容量上限刪除最舊的資料；設計包 §25.5 #3）：日誌改用代碼與 `copywriting.md` 的文案，開始與結束寫入 `meta.capacity_event`，系統健康顯示最近一次容量刪除。
- 量測（`kairos/src/api/bench.rs`，kp-build 4 vCPU）：1 萬台主機的列表 p99 約 10 ms（每次重建索引的最壞情況約 40 ms）、30 天趨勢 p99 約 8～13 ms；每秒 2,000 筆 report 同時以主控台一般用量查詢時 `ack` p99 約 1 ms（只有寫入時約 2 ms），連續查詢的壓力下約 6 ms（API.md 第 9 節）。

#### 變更

- 啟動時的檔案描述元檢查（`LimitNOFILE`）的需要量含主控台的連線上限（上方第 3 項）。
- 狀態表記錄最近一次的離線原因（逾時、連線關閉、Kairos 關機、連線中憑證被拒）、是否協商了 `durable` 與最近一次收到補送資料的時間，供 API 使用；`agents.json` 的格式不變。
- 主控台在自己的 tokio runtime 上執行，查詢不影響 agent 連線與 report 寫入的延遲。
- `kairos hosts list`、`reports export／import`、`db backup` 不變；`kairos reports import` 仍需要停止 Kairos（主控台的寫入連線不會放掉跨行程的檔案鎖，e2e `import.rs` 涵蓋）。
- 資料庫在執行中被替換、writer 重新開啟時，不再以 `std::fs` 開關既有的資料庫檔與 `-wal`／`-shm`（只以路徑收緊權限）：同一個行程內關閉這些檔的 fd 會放掉其他連線（主控台的唯讀連線池、背景 checkpoint）的 fcntl 鎖，`kairos reports import` 可能誤判 Kairos 未執行。
- `storage.backend = "jsonl"` 時不啟動主控台（記 `WARN`）。

### Pulse

#### 新增

- **`pulse status`**（設計包 `pulse/cli-spec.md` §4）：讀取本機狀態檔與服務管理員（systemd、Windows 服務控制管理員），顯示服務、連線、回報、身分、設定、時鐘六列與本機狀態（線上／警告／離線／嚴重／未知），問題另以三段式說明（標題、原因、處理方式與代碼）寫到 stderr。`--brief`（單行）、`--json`（`pulse.status/v1`，純 ASCII）、`--watch [秒]`、`--exit-codes=nagios`、`--config`、`--ascii`、`--color`。不需要連線；結束代碼 0 線上、8 警告、9 離線／嚴重／未知、2 用法錯誤、10 無法讀取狀態檔。「回報」列寫出補送資訊：待補送筆數、最舊一筆的取樣時間、預計補完時間、最近 24 小時內因緩衝已滿或超過保存時間而丟棄的筆數與取樣時間範圍（`PLS-W4202`／`PLS-W4203`）、無法寫入緩衝（`PLS-W4103`）、緩衝無法使用（`PLS-E4004`）、Kairos 暫時無法保存（`PLS-W4101`）。README「本機狀態（pulse status）」、INSTALL 第 8.2 節。
- **本機狀態檔**（`cli-spec.md` §4.1）：Linux `/var/lib/pulse/status.json`（`pulse.service` 新增 `StateDirectory=pulse`；多實例 `StateDirectory=pulse/N` 時依 `STATE_DIRECTORY` 寫 `/var/lib/pulse/N/`），Windows `%ProgramData%\Pulse\status.json`；`[status] file` 可改。**所有人可讀**（Unix 0644、目錄 0755，不受 unit 的 `UMask=0077` 影響；Windows 授予 Users 讀取），`pulse status` 不需要 root。執行中每 10 秒、狀態變化時另外以原子方式改寫（暫存檔＋rename，不 fsync）。內容不含秘密：沒有私鑰、token；未啟用 mTLS 時不含 agent_id（等同身分憑據）。以單調時鐘判斷是否過時（超過 60 秒為「未知」，`PLS-W8004`），正常結束前寫入結束代碼。
- **`pulse explain <代碼>`**：印出錯誤代碼的標題；發佈包新增 `ERROR-CODES.md`（代碼 → 程式位置 → 觸發條件的對照表，與尚未實作的代碼）。
- 子命令 `pulse run`（等同不帶參數）；`pulse status --instance <N>`（多實例）。

#### 變更

- **日誌加上固定事件與錯誤代碼**（上方第 5 項；設計包 `pulse/log-ux.md` §4、`decisions/INTEGRATION.md` §25.5）：有對應事件的行，訊息開頭為「`<event> <code> `」，例如 `buffer.full PLS-W4202`、`auth.rejected PLS-E1105`、`conn.registered -`。本機緩衝的 5010～5015（`buffer.opened`、`buffer.unavailable` `PLS-E4004`、`buffer.write_failed` `PLS-W4103`、`buffer.full` `PLS-W4202`、`buffer.expired` `PLS-W4203`、`buffer.replay_done`），連線中斷依原因分為 `conn.lost`（`PLS-E2002`～`E2005`、`E2201`）、`tls.server_cert_rejected`（`PLS-E1401`～`E1405`）、`auth.*`、`conn.superseded`、`proto.incompatible`、`server.full`；新增 `conn.restored`（恢復時記中斷時間、重試次數、待補送與丟棄筆數）與 `conn.outage`（中斷滿 30 分鐘時一次 `ERROR`，同時寫 stderr）。
- **`clock.skew`**（事件 5000，`log-ux.md` §4.5）：30 秒～5 分鐘記 `WARN`（`PLS-W8301`），**超過 5 分鐘改記 `ERROR`**（`PLS-E8302`）並寫 stderr；只在等級改變或偏差再變化超過 1 分鐘時記錄（重連時不重複），回到 30 秒以內記一則 `INFO`（`clock.skew -`）。
- **`status.write_failed`**（事件 5020，`PLS-E8204`）：執行中無法寫入狀態檔時第一次記 `WARN`，連續失敗滿 60 秒（`pulse status` 開始顯示「未知」）記 `ERROR` 並寫 stderr，之後每 10 分鐘一則 `ERROR` 摘要、其間 `DEBUG`；恢復寫入記 `status.write_restored`（事件 5021，`INFO`）。
- **`pulse status` 遇到過時的狀態檔**（`cli-spec.md` §4.8）：連線、回報、身分、時鐘列以最後已知內容照常判定，並標示「（最後已知，N 分鐘前）」，沒有內容時略過（`skipped`）；本機狀態為「未知」（結束代碼 9）。沒有權限讀取狀態檔時結束代碼 10，`--exit-codes=nagios` 為 3（UNKNOWN）。
- 啟動失敗的 stderr 訊息帶代碼：`錯誤 PLS-E0101：無法載入設定：…`（範圍錯誤為 `PLS-E0201`）、`錯誤 PLS-E4002：無法初始化日誌：…`；`agent.fatal` 帶 `PLS-E3103`／`PLS-E3104`（`agent.id`）、`PLS-E1102`（SAN 與 agent_id 不符）。命令列錯誤帶 `PLS-E0001`～`E0003`。
- 無法開啟本機緩衝時，systemd 的 `Status:` 在「已註冊：…」後加上「；無法開啟本機緩衝（PLS-E4004，斷線期間的資料不會補送）」。
- `--help` 列出 `status`、`explain`。

#### 與設計包的差異與待辦

見 `pulse/ERROR-CODES.md` §4、§5：尚未實作 Windows 事件記錄（`clock.skew`、`status.write_failed` 的 `ERROR` 目前只寫日誌與 stderr）、服務描述、`log-ux.md` 的完整格式（RFC 3339 時間、JSON）與重試摘要、`--all`、`--json-lines`，以及 53 個與註冊、安裝、自動更新憑證相關的代碼。Windows 的 MSI（含 SCCM 偵測指令碼依賴的 `install-result.json` 與執行檔的版本資源）尚未實作，在那之前不要以 `Detect-PulseInstalled.ps1` 作為必要部署的偵測規則（設計包 `sccm-deployment.md` §3、`decisions/INTEGRATION.md` §26.5 #8）。

### 相容性

- 協定不變：Kairos 0.4 與 Pulse 0.2／0.3／0.4 的組合，以及 Pulse 0.4 與 Kairos 0.2／0.3 的組合，都與對應的 0.3 組合相同（見「版本相容矩陣」與 0.3.0 的「相容性（A5）」）。
- `pulse status` 只讀本機狀態檔，與連上的 Kairos 版本無關。

### 已知限制

- API 為唯讀；沒有使用者、角色、工作階段（F6）、告警（F3）與網頁介面（F2）。主機狀態只有 `online`、`offline`、`unknown`，`critical`／`warning`／`pending` 的計數為 `null`。其他限制、延後項目與待決定事項見 `kairos/API.md` 第 10 節。

### 共用

- 端對端測試（`tests/e2e`）新增 `api.rs`（以實際的 `kairos api-token create` 建立 token，經主控台 API 查主機與指標，SSE 收到上線／離線事件，撤銷的憑證歸屬到主機）與 `status.rs`（`pulse status` 隨 Kairos 停機、恢復與 Pulse 結束的變化）。每個 Kairos 另取一個隨機埠給主控台；子行程不繼承 `PULSE_SYMBOLS`、`NO_COLOR`、`CLICOLOR_FORCE`，狀態檔寫在各測試自己的暫存目錄。

### 發佈與建置

- 版本：`kairos`、`pulse` 改為 0.4.0；發佈包為 `kairos-0.4.0-<commit>-linux-x86_64.tar.gz`、`pulse-0.4.0-<commit>-linux-x86_64.tar.gz`。
- Kairos 發佈包另附 `API.md`；Pulse 發佈包另附 `ERROR-CODES.md`，`pulse.service` 新增 `StateDirectory=pulse`、`StateDirectoryMode=0755`。
- 新增依賴 `hyper`（只啟用 `server`、`http1`）、`hyper-util`（`tokio`）及其依賴 `http`、`http-body`、`httparse`、`httpdate`、`atomic-waker`（皆為 MIT／Apache-2.0）；`ring`、`subtle` 改為 Kairos 的直接依賴（原已在依賴樹中）。`deny.toml`、`packaging/about.toml` 的授權允許清單不需變更。

## [0.3.0] - 〔草稿：發佈日期待定〕

〔草稿〕**Kairos 0.3.0、Pulse 0.3.0**：商業審計路線圖「階段 2 資料層」中的 A1（資料庫）、A5（at-least-once 投遞）、A6（持久化主機清冊）、A9（容量）。Kairos 的 report 預設改寫 SQLite，另有主機清冊、5 分鐘／1 小時彙總、分層保留與總容量上限；Pulse 新增本機環狀緩衝，Kairos 停機或網路中斷期間的取樣在恢復後補送，雙方以協定 feature `durable` 協商，Kairos 以唯一索引去重。`kairos/Cargo.toml`、`pulse/Cargo.toml` 的版本已改為 0.3.0；發佈時依 RELEASING.md 填上日期並移除〔草稿〕標記。架構、schema、設定、容量估算、量測與備份見 `kairos/STORAGE.md`（發佈包內的 `STORAGE.md`）；協定見 `crates/kairos-proto/PROTOCOL.md`「durable：at-least-once 投遞」。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **report 預設寫入 SQLite（`data/kairos.db`），不再寫 `reports-YYYY-MM-DD.jsonl`**（A1） | 直接讀取 `data/reports-*.jsonl` 的工具（匯入、報表、監控腳本），以及複製資料目錄的備份腳本 | 升級前擇一：讀取工具改用 `kairos reports export --since <時間> [--agent <agent_id>]`（每行格式與 JSONL 相同，`peer`、`hostname` 為清冊中最後的值）；或在 `[storage]` 加上 `jsonl_mirror = true` 同時輸出 JSONL（盡力而為、不影響 `ack`，磁碟用量約多 6 倍）；或暫時設定 `backend = "jsonl"`（與 0.2 相同，沒有主機清冊與彙總）。備份改用 `kairos db backup <檔案>`（**執行中不可直接複製 `kairos.db`**，INSTALL 第 10.2 節）。既有的 JSONL 檔不會自動匯入資料庫、不會被修改；需要舊資料時以 `kairos reports import` 匯入（需要停止 Kairos，見下方升級步驟 2a）。不開鏡像時 JSONL 的保留清除不再執行，舊檔請在匯入後自行搬移或刪除 |
| 2 | Kairos | **資料目錄必須在本機磁碟** | `storage.data_dir` 放在 NFS、SMB 等網路磁碟的部署 | SQLite 的 WAL 需要共享記憶體與檔案鎖；無法啟用 WAL 時拒絕啟動（`無法啟用 WAL`）。改到本機磁碟 |
| 3 | Kairos | **總容量上限預設 64 GiB，超過時刪除最舊的資料**（A9） | 主機多（約 1,300 台以上）或保留天數調長的部署 | 依 STORAGE.md「容量估算」設定 `storage.max_total_bytes`（0 = 不限制）與 `raw_retention_days`／`rollup_5m_retention_days`／`rollup_1h_retention_days`（預設 7／31／400 天）。上限小於以 `server.max_connections` 台估算的容量時啟動記 `WARN`。縮短 `raw_retention_days` 時一併檢查第 6 項的 `buffer.max_age_hours` |
| 4 | Kairos | **`agents.json` 在重新啟動後保留最近 24 小時內出現過的主機**（A6，行為改變） | 依「重新啟動後 `agents.json` 為空」或 `total` 計數的工具 | 重新啟動後這些主機以 `online: false`、`conn_id: 0` 出現，24 小時後照常移除；需要所有主機（含更早的）時用 `kairos hosts list --json` |
| 5 | Kairos | **補送的資料：`ts` 可能比 `recv_at` 早數小時到數天**（A5）。SQLite 中依 `ts`（Pulse 的取樣時間）保存、彙總與套用保留期限；匯出與 JSONL 另有選用欄位 `boot_id`、`report_seq`（`schema` 不變），JSONL 仍寫入收到當天的檔案 | 依 `recv_at`、檔案日期或匯出順序當作取樣時間的讀取工具 | 時序分析改用 `ts`；忽略未知欄位的工具不受其他影響。`kairos reports export --since` 依取樣時間篩選（STORAGE.md 第 5.2 節） |
| 6 | Pulse | **預設啟用本機緩衝，需要可寫的 `buffer/` 目錄**（A5） | 以發佈包的 systemd unit（`ProtectSystem=strict`，只有 `logs/` 可寫）執行的部署 | 升級前 `install -d -o pulse -g pulse -m 750 /opt/pulse/buffer`，並換上新版 `pulse.service`（`ReadWritePaths=/opt/pulse/logs -/opt/pulse/buffer`；多實例時對每個實例重做，Pulse INSTALL.md 第 9、10 節）。沒有做時 Pulse 照常監控，但記 `ERROR`「無法開啟本機緩衝」並改為 at-most-once。緩衝保留 `buffer.max_age_hours`（預設 168 小時）內的資料，建議不超過 Kairos `raw_retention_days` × 24（STORAGE.md 第 5.4 節） |
| 7 | Pulse | **磁碟用量增加**：緩衝預設最多約 72 MiB（`buffer.max_mb` 64 MiB 加兩個區段）（A5） | 磁碟空間很小的主機 | 在 `pulse.toml` 調小 `[buffer] max_mb`／`max_records`，或 `enabled = false`（與 0.2.0 相同） |
| 8 | 兩者 | **恢復連線後的寫入量暫時增加**：每台 Pulse 補送時約每秒 0.83 筆（`buffer.replay_interval_ms` 預設 1200；平常每 5 秒 1 筆），1 萬台同時恢復約每秒 8,300 筆（平常約 2,000 筆）（A5） | 大量主機同時恢復連線的部署（例如 Kairos 維護後） | kp-build（4 vCPU）上量測：每秒 8,000 筆持續 5 分鐘 p99 0.12 秒、沒有 `storage_unavailable`；每秒 16,000 筆持續 5 分鐘時約 3% 回 `storage_unavailable`（Pulse 退避後重送，不遺失、不重複），最長回應 5.05 秒（8 秒預算內）（STORAGE.md「量測」）。預設值即依此訂在約每秒 8,300 筆；主機超過 1 萬台時再調大 `buffer.replay_interval_ms`（例如 2000）。以 `backend = "jsonl"` 或鏡像寫 JSONL 時，補送寫入收到當天的檔案，`storage.max_daily_bytes` 可能較快達到（之後回 `storage_unavailable`，資料留在 Pulse，換日後繼續補送） |

**升級步驟**（依序；Kairos 先升級，Pulse 升級後的第一次斷線就能補送）：

1. **Kairos 升級前**：依第 1 項處理讀取 JSONL 的工具與備份腳本，依第 5 項檢查以 `recv_at` 當取樣時間的工具；確認資料目錄在本機磁碟並有足夠空間（第 2、3 項）。
2. **升級 Kairos**：換上新版執行檔並重新啟動（Kairos INSTALL 第 11 節，unit 不需修改）。之後每次升級 Kairos 前先以 `kairos db backup` 備份：新版可能升級 schema，升級後的資料庫無法給舊版使用。
   - 2a.（選用）**匯入 0.2 的 JSONL**：`kairos reports import --dry-run` 估算筆數後，停止 Kairos、`sudo -u kairos /opt/kairos/kairos reports import /opt/kairos/data --config /opt/kairos/kairos.toml`、再啟動（約每秒 3～4 萬行）。停機要短時，可在換上新版之前、0.2 仍在執行時，先以新版的執行檔匯入前幾天的檔案，停機時只匯入當天的（STORAGE.md「匯入舊的 JSONL」）。
3. **確認 Kairos**：以 `sudo -u kairos /opt/kairos/kairos hosts list --config /opt/kairos/kairos.toml` 確認既有的 Pulse（0.2.0，at-most-once）重新連上後出現在清冊中。
4. **每台 Pulse 主機：建立緩衝目錄**：`install -d -o pulse -g pulse -m 750 /opt/pulse/buffer`（第 6 項；多實例時每個實例各一個）。
5. **換上新版 `pulse.service`** 並 `systemctl daemon-reload`（`ReadWritePaths` 加上 `-/opt/pulse/buffer`）。
6. **升級 Pulse**：換上新版執行檔並重新啟動（Pulse INSTALL 第 10 節）。啟動日誌應有「本機緩衝：buffer（待送 0 筆…）」，Kairos 的上線日誌註明「durable：report 可補送、去重」。
7. 需要時調整 Pulse 的 `[buffer]`（第 7、8 項）與 Kairos 的保留天數，讓 `buffer.max_age_hours ≤ 24 × raw_retention_days`（第 6 項）。

先升級 Pulse 也可以：連上 0.2.0 的 Kairos 時不啟用 durable（連線期間 at-most-once），斷線期間的積壓保留在 Pulse 的緩衝中，Kairos 升級後才補送（只是會先佔用緩衝）。

### 協定（kairos-proto）

- 新增 feature `durable`（v2 的 `features` 協商，不提升協定版本）：雙方都宣告時 report 為 at-least-once，report 帶 `boot_id`（UUID，Pulse 每次程序啟動時產生）與 `report_seq`（同一 `boot_id` 內從 1 遞增）；缺漏或型別錯誤回 `bad_request`。任一端沒有宣告時維持 at-most-once。PROTOCOL.md 新增「durable：at-least-once 投遞」與測試向量，並更新相容矩陣、寫入結果不明時的重送語意與 8 秒回應預算的說明（A5）。
- `DurableSeq`、`FEATURE_DURABLE`、`Session.durable`、`validate_durable_seq`、`DURABLE_DEDUP_WINDOW`（256）、`MAX_DURABLE_IN_FLIGHT`（8）。
- `PROTOCOL.md`：`ack` 的「已寫入」涵蓋資料庫的 commit；狀態表與 `agents.json` 在重新啟動後的行為依主機清冊更新；伺服器的去重範圍分別說明 SQLite（原始資料保存期間）與 JSONL（記憶體視窗）。

### Kairos

#### 新增

- **SQLite 儲存**（A1，`storage.backend = "sqlite"`，預設）：`data/kairos.db`（WAL）。原始 report 每筆一列，指標以緊湊的二進位格式保存，名稱、單位與 label 的組合（序列）各存一次，不再每筆重複 `peer`、`hostname`、`agent_id`；以 `(host_id, ts)` 索引查詢某台主機的時間範圍。Pulse 0.2.0 的典型 report（17 個指標）每筆約 240 bytes（含索引），JSONL 約 1.5 KB。schema 以 `PRAGMA user_version` 管理版本，啟動時依序遷移（每版一個交易）；資料庫比程式新時、或不是 Kairos 的資料庫時拒絕啟動。
- **寫入介面抽象化**：連線只認得 `ReportSink`；後端實作 `trait ReportStore`（`store/mod.rs`），日後可換成時序資料庫。單一 writer 把佇列中目前所有的要求（最多 1024 筆）寫成一個交易（group commit），在 blocking 執行緒上執行，commit 成功後才回 `ack`；commit 前失敗、磁碟已滿等確定未寫入時回 `storage_unavailable`，commit 結果不明時維持「不回應、關閉連線」。checkpoint 由背景執行緒進行（必要時由 writer 在兩批之間補做），不在 commit 時擋住寫入。資料庫在執行中被刪除或替換時重新建立，不會寫進已刪除的檔案後回 `ack`。量測（kp-build，每秒 2000 筆、1 萬台主機）：p50 0.55 ms、p99 1.4 ms（STORAGE.md「量測」）。
- **durable（at-least-once）與去重**（A5）：協商 `durable` 時，連線以 `ReportRecord::with_dedup(Some(DedupKey { boot_id, report_seq }))` 把去重鍵交給 writer（`ReportSink::submit(record)` 不變）。SQLite 以唯一索引保證同一主機、同一個 `(boot_id, report_seq)` 只保存一次，範圍為原始資料保存的整段期間；重複的回 `WriteStatus::Duplicate`，連線照常回 `ack`、不重寫、不加入彙總、不寫入鏡像、不更新 `last_report`。commit 結果不明而斷線後的重送也由唯一索引去重，記憶體中的彙總由原始資料重建，那一筆只算一次。`backend = "jsonl"` 時以記憶體中的視窗去重（每個 boot 256 筆，啟動時由最新兩個 report 檔尾端重建，48 小時閒置清除）；JSONL 鏡像不另外去重。上線日誌註明「durable：report 可補送、去重」。
- **補送的歷史資料**（A5）：一律接受，`ts` 原樣保存。SQLite 的 `reports.ts` 為取樣時間（durable 時為 Pulse 的 `ts`，須在收到時間的 −367 天～＋5 分鐘內，否則用收到的時間；非 durable 為收到的時間），另存 `recv_lag` 還原收到的時間。彙總依取樣時間分桶；取樣時間落在已寫出的桶時，在同一個交易中累加進 `rollup_5m`／`rollup_1h`（不覆蓋、不遺漏、重新啟動後不重複）；補送中的桶等補完（15 秒沒有新值）才寫出。`agents.json` 的 `last_report` 不會被較舊的補送資料蓋過。
- **主機清冊**（A6）：所有出現過的主機保存在 `hosts` 表（`agent_id`、hello 的主機資訊含 `host_fingerprint`、最後的來源位址、是否以 mTLS 驗證身分、首次出現、最後訊息、最後一筆 report），每 30 秒從狀態表同步（每個交易最多 500 列，其餘在之後的維護步驟寫入；1 萬台時每個交易約 17 ms）、關機時寫入最後狀態。啟動時把上次異常結束時仍標為線上的主機改為離線，並把最近 24 小時內出現過的主機以離線狀態放回狀態表（「重新上線」與主機指紋的比對延續到重新啟動之後）。新設定 `storage.max_hosts`（預設 `max(100000, limits.max_agents)`）。
- **`kairos hosts list [--offline-for <時間>] [--wide] [--no-header] [--json] [--ascii]`**：查詢主機清冊（線上／離線／未知、主機名稱、`agent_id`、IP、最後訊息），回答「哪些主機已失聯 N 天」；`--json` 為純 ASCII（`schema` `kairos.hosts.list/v1`），錯誤為 `KRS-E4003`。輸出依設計包 `cli-style.md`。
- **`kairos reports export [--since <時間>] [--agent <agent_id>]`**：以 JSON Lines 匯出 SQLite 中的原始 report，格式與 JSONL 檔相同（`schema` 1／2，durable 的另有 `boot_id`、`report_seq`；`--since` 依取樣時間）。
- **`kairos db backup <檔案>`**：線上備份（`VACUUM INTO`），Kairos 執行中也可以執行，不覆寫既有檔案，備份檔 `0640`。
- **`kairos reports import [--dry-run] [--since <時間>] [--until <時間>] [--json] <檔案或目錄>...`**：把 0.2.x 以前的 `reports-YYYY-MM-DD.jsonl`（0.1.0 沒有 `schema` 的行、`schema` 1／2、durable 的 `boot_id`／`report_seq`）匯入 SQLite（STORAGE.md「匯入舊的 JSONL」）。
  - 取樣時間與 Kairos 寫入時的規則相同（durable 的行用 `ts`，其他用 `recv_at`），`reports export` 輸出的行與原本的相同。
  - **冪等**：以「主機＋取樣時間＋內容雜湊」為去重鍵（新的 `import_keys` 表），重複執行、中斷後重新執行、同一行出現在兩個檔案都只匯入一次；durable 的行另以原本的唯一索引、非 durable 的行以「資料庫中已有完全相同的原始 report」（例如 JSONL 鏡像）判斷重複。
  - **保留期限**：原始資料保留期限內的寫入原始資料並併入彙總；超過的**只併入彙總**（5 分鐘彙總也過期時只進 1 小時）；超過所有彙總保留期限的略過。彙總與 Kairos 的遲到資料相同的方式合併，並納入 Kairos 停止時尚未寫出的彙總，Kairos 再啟動時不重複計算。
  - **主機**：清冊中沒有的主機以離線、`hosts.source = 'import'` 建立；既有的主機只在匯入的資料較新時更新 `last_seen`、`hostname`、`peer`。
  - **需要停止 Kairos**：以 SQLite 的獨占鎖開啟資料庫，Kairos 執行中時以結束代碼 11（`KRS-E4304`「資料庫正在使用中」）拒絕，匯入期間 Kairos 也無法啟動；不與服務同時寫入（服務記憶體中的彙總與 8 秒回應預算）。`--dry-run` 以唯讀方式開啟，執行中也可以。
  - 來源檔只讀取；目錄只取 `reports-*.jsonl` 與 `reports.jsonl`，不進入子目錄、不跟隨符號連結（參數本身是符號連結時也拒絕），壓縮檔列出提示先解壓縮；Linux 上以 root 執行而資料目錄屬於其他帳號時拒絕（結束代碼 10）。
  - 壞行（不是 JSON、欄位缺漏、不合法、沒有換行的最後一行）略過並計數，每個檔案列出前 5 個的行號與原因；摘要列出讀取、匯入（原始資料／只進彙總）、重複略過、壞行、超出保留期、不在範圍、無法匯入與新增主機。結束代碼：0 完成（含有壞行）、1 有來源無法讀取或匯入失敗、2 用法、3 設定、10 權限、11 資料庫使用中。`--json` 為純 ASCII（`schema` `kairos.reports.import/v1`）。
  - kp-build 上 100 萬行（984 MB）約 25 秒（每秒約 4 萬行；1 萬台主機時約 3.4 萬行、只進彙總時約 2.1 萬行），再匯入（全部重複）約 15 秒；去重鍵每行約 23 bytes（STORAGE.md「量測」）。
- schema v2：`hosts.source` 與 `import_keys`（遷移在啟動或匯入時自動套用；以 0.3.0 開發中的建置建立的 v1 資料庫照常升級）。
- **彙總**（A9）：每個指標的 5 分鐘與 1 小時 min／max／sum／count（`rollup_5m`、`rollup_1h`）；v1 的三個欄位對應到標準指標（記憶體換算成 bytes），與 v2 合併成同一條趨勢。重新啟動時從原始資料補回尚未寫出的部分，不漏算、不重複算。
- **分層保留與總容量上限**（A9）：`storage.raw_retention_days`（預設 7）、`storage.rollup_5m_retention_days`（31）、`storage.rollup_1h_retention_days`（400），依取樣時間；`storage.max_total_bytes`（預設 64 GiB）超過時依序刪除最舊的原始 report、5 分鐘彙總、1 小時彙總，直到低於上限的 95%。刪除都分批進行（每步最多 5000 列、一個短交易），不會長時間鎖住資料庫；依時間的刪除沿用 DateGuard 的時鐘跳躍保護（狀態存在資料庫中）。
- 新設定 `storage.backend`（`sqlite`／`jsonl`）、`storage.jsonl_mirror`（`sqlite` 時另輸出 JSONL 鏡像，沿用 `retention_days`、`max_daily_bytes` 與保留清除）、`storage.sqlite_synchronous`（`normal`／`full`）。範例設定與 README 的設定表已更新。
- 啟動時：`storage.max_total_bytes` 小於以 `server.max_connections` 台主機估算的容量時記 `WARN`；記錄資料庫的 schema 版本、主機與序列數與載回的主機數。

#### 變更

- **（破壞性）** report 預設寫入 SQLite（見上表第 1 項）。`storage.retention_days`、`storage.max_daily_bytes` 只用於 JSONL（`backend = "jsonl"` 或鏡像）；鏡像達到 `max_daily_bytes` 時只是不寫鏡像，不回 `storage_unavailable`。`storage.max_daily_bytes` 的啟動警告只在寫 JSONL 時出現。
- **（行為改變）** `agents.json` 的處置：保留，格式不變，仍是狀態表的即時快照（每 5 秒），Kairos 不讀回這個檔案；完整的主機清冊改由 SQLite 保存（`kairos hosts list`）。重新啟動後 `agents.json` 含最近 24 小時內出現過的主機（見上表第 4 項）；`backend = "jsonl"` 時與 0.2 相同。
- JSONL 的保留清除與時鐘跳躍保護不變，對 `backend = "jsonl"` 與鏡像都有效。
- 新的日誌類別「SQLite 維護工作」（彙總寫出、保留期限與容量上限的刪除、清冊同步）的 `WARN` 依類別節流。

#### 文件

- 新增 `kairos/STORAGE.md`（發佈包內附）：架構、`ack` 語意、schema 與遷移、批次策略、去重、取樣時間與遲到資料、保留期限與 Pulse 緩衝的關係、主機清冊、彙總、保留與容量上限、設定、容量估算公式與表、吞吐量量測（含補送尖峰與匯入速度）、查詢範例、備份與還原、從 JSONL 升級（含 `kairos reports import` 的設計：去重鍵、保留期限、與服務並行、縮短停機的作法）。
- README：設定表、資料輸出、檔案權限、專案結構、依賴與目前限制依 SQLite 與 durable 更新；INSTALL：資料目錄須為本機磁碟、確認清單加上 `kairos hosts list`、備份改用 `kairos db backup`、升級前備份與回滾、疑難排解。

### Pulse

- 新增本機環狀緩衝 `[buffer]`（預設啟用）：取樣先寫進 `buffer/`，Kairos 確認後才刪除；重新連上後先依序補送積壓再送新的取樣，最多 4 筆在途、兩筆間隔至少 `replay_interval_ms`（預設 1200 ms，實際不超過 `report.interval` 的 60%，讓積壓一定補得完）；`storage_unavailable` 與結果不明時退避後重送，`invalid_report`／`bad_request` 丟棄（A5）。
- 緩衝滿時丟最舊並計數記 `WARN`；超過 `max_age_hours`（預設 7 天）的不送出；每筆記錄有 CRC，啟動時截掉斷電造成的不完整尾端；不逐筆 fsync（最多每 10 秒）。Unix 上目錄 0750、檔案 0640，目錄權限過寬時記 `WARN`（Windows 檢查 ACL）（A5）。
- 連上不支援 `durable` 的 Kairos 時，連線期間維持 at-most-once，緩衝中的積壓保留不送（A5）。
- `pulse.service` 允許寫入 `buffer/`（`ReadWritePaths=/opt/pulse/logs -/opt/pulse/buffer`）；INSTALL 補上緩衝目錄的建立與升級步驟。

### 相容性（A5）

| Kairos ＼ Pulse | Pulse 0.2.0 | Pulse 0.3.0（`buffer.enabled = true`） | Pulse 0.3.0（`buffer.enabled = false`） |
|---|---|---|---|
| Kairos 0.2.0 | 與 0.2.0 相同 | v2，不啟用 durable：連線期間 at-most-once；斷線期間的積壓保留在 Pulse，連上 0.3.0 的 Kairos 後才補送 | 與 0.2.0 相同 |
| Kairos 0.3.0 | v2，不啟用 durable（與 0.2.0 相同；Pulse 不宣告 durable）；資料寫入 SQLite | **v2＋durable：at-least-once** | 與 0.2.0 相同；資料寫入 SQLite |

協定 v1（0.1.0 的任一端）時不使用 features，維持 at-most-once。

### 容量估算（A5，預設值）

- 每台 Pulse 補送速率 1 ÷ 1.2 秒 ≈ 0.83 筆／秒（Kairos 的 report 速率限制為每秒 2 則、可連續 10 則，新連線先給 2 則；兩筆至少間隔 1.2 秒時 token 在下一筆之前已補回 2.4 則，連新連線的 2 則也不會用完，因此不會觸發限流或 `storage_unavailable`；設定的下限 500 ms 時恰為每秒 2 則，同樣不觸發），扣掉同時產生的新取樣（每 5 秒 1 筆）後積壓以約 0.63 筆／秒減少：斷線 1 小時（720 筆）約 19 分鐘補完，24 小時（17,280 筆）約 7.6 小時補完。
- 1 萬台同時恢復連線：Pulse 的重連隨機延遲（0～5 秒）與補送前的隨機延遲（0～5 秒）把開始時間攤開後，Kairos 的 report 寫入約 8,300 筆／秒（平常約 2,000 筆／秒，約 4.2 倍），持續到各台補完。SQLite 在 kp-build（4 vCPU）上：每秒 8,000 筆持續 5 分鐘 p99 0.12 秒、沒有 `storage_unavailable`；每秒 16,000 筆在 60 秒內 p99 0.1 秒，持續 5 分鐘時資料庫變大、寫入跟不上，約 3% 的 report 在 5 秒後回 `storage_unavailable`，最長回應 5.05 秒，沒有結果不明，8 秒回應預算不受影響（STORAGE.md「量測」）。伺服器端不會無限積壓：每條連線最多 4 筆在途、Kairos 逐則處理；寫入跟不上時寫入佇列（1024 筆）滿、連線等待逾時回 `storage_unavailable`，Pulse 退避（2 秒加倍到 60 秒）後重送，整體自動放慢。預設值 1200 ms 的尖峰（約 8,300 筆／秒）接近每秒 8,000 筆、沒有 `storage_unavailable` 的量測範圍。需要再壓低尖峰時調大 `buffer.replay_interval_ms`（例如 2000 ms → 約 5,000 筆／秒，24 小時的積壓約 16 小時補完）；主機少、希望補得快時可調小（500 ms → 每台約 2 筆／秒，24 小時的積壓約 2.7 小時補完）。
- 補送的資料不增加長期的容量（補的是斷線期間本來就該有的資料）；寫 JSONL 時寫入收到當天的檔案，`storage.max_daily_bytes` 可能比平常更快達到（見上表第 8 項）。

### 共用

- 端對端測試（`tests/e2e`）改以預設的 SQLite 執行：report 以實際的 `kairos reports export` 從資料庫取出檢查，另檢查 JSONL 鏡像與資料庫一致、關機後 `kairos hosts list --json` 中主機為離線且帶主機資訊、重新啟動時從清冊載回主機。新增 durable 的端對端測試：Kairos 停機期間的取樣補送後不缺號、不重複；結果不明的重送與 Kairos 重新啟動後的重送都不重複。新增 `kairos reports import` 的端對端測試：Kairos 執行中拒絕（結束代碼 11，跨行程的檔案鎖）、停止後匯入與重複匯入、重新啟動後匯出與清冊。

### 發佈與建置

- 版本：`kairos`、`pulse` 改為 0.3.0；發佈包為 `kairos-0.3.0-<commit>-linux-x86_64.tar.gz`、`pulse-0.3.0-<commit>-linux-x86_64.tar.gz`。
- 新增依賴 `rusqlite`（MIT，只啟用 `bundled`：編譯內附的 SQLite 原始碼，不依賴系統的 libsqlite3，musl 靜態連結與 Windows 都不需額外安裝）及其依賴 `libsqlite3-sys`（MIT）、`fallible-iterator`、`fallible-streaming-iterator`（MIT／Apache-2.0）、`pkg-config`、`vcpkg`（建置用，MIT／Apache-2.0）。SQLite 本身是 public domain（不需授權條款），`THIRD-PARTY-LICENSES` 另加註說明。`deny.toml`、`packaging/about.toml` 的授權允許清單不需變更（沒有新的授權種類；未啟用 rusqlite 的 `cache` feature，避免引入 Zlib 授權的 `foldhash`）。
- Kairos 發佈包新增 `STORAGE.md`；Pulse 發佈包的 `pulse.service` 允許寫入 `buffer/`。

## [0.2.0] - 〔草稿：發佈日期待定〕

〔草稿〕**Kairos 0.2.0**、**Pulse 0.2.0**（兩者同時發佈）。`Cargo.toml` 已是 0.2.0，尚未打 tag；發佈時依 RELEASING.md 填入日期並移除〔草稿〕標記。共用 crate（`kairos-proto`、`date-guard`）是內部 crate，維持 0.1.0、不單獨發佈。協定新增 v2，與 v1 自動協商（見上方「版本相容矩陣」）。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **未啟用 mTLS 的對外監聽改為拒絕啟動**（S1） | `server.listen` 為非 loopback 位址（`0.0.0.0`、`::`、對外 IP，或解析成這些位址的主機名稱）而沒有設定 `auth.client_ca` 的部署 | 升級**前**依 `kairos/README.md`「啟用 mTLS」設定 `auth.client_ca`（建議同時設定 `auth.crl`）。只有隔離的測試環境才在 `kairos.toml` 的 `[auth]` 加上 `allow_unauthenticated = true`（或 `KAIROS__AUTH__ALLOW_UNAUTHENTICATED=true`）。只監聽 `127.0.0.1`／`::1` 的設定不受影響 |
| 2 | Pulse | **設定 `ca_cert` 時預設不再信任作業系統信任庫**（S5） | 設定了 `server.ca_cert`、但 Kairos 的伺服器憑證其實由公開 CA 簽發，又沒有明確設定 `server.use_system_roots` 的部署 | 依 INSTALL 部署（私有 CA）者不受影響。其餘請在 `pulse.toml` 的 `[server]` 加上 `use_system_roots = true`。升級後啟動日誌的「伺服器憑證信任來源：…」一行會寫出實際採用的信任來源；設定錯誤時會以「伺服器憑證不受信任」無法連線 |
| 3 | Pulse（影響 Kairos 的離線判定） | **心跳預設由 1 秒改為 15 秒**（A11） | 未自行設定 `heartbeat.interval` 的 Pulse | Kairos 的離線判定 `max(5 × 心跳, 5 秒)` 由 **5 秒變成 75 秒**：主機斷線後約 75 秒才會在 Kairos 顯示為離線；Pulse 的讀取閒置逾時由 11 秒變成 30 秒。依賴快速離線判定的監控或告警請調整門檻，或在 `pulse.toml` 設定 `[heartbeat] interval = 1`（1～60 秒）恢復舊行為。Kairos 不需修改 |
| 4 | Kairos | **既有資料檔與日誌檔的權限被收緊到最多 0640**（S3，Unix） | 以 Kairos 服務帳號以外、又不在其群組中的帳號讀取 `data/reports-*.jsonl`、`agents.json`、`logs/kairos-*.log` 的工具（例如日誌收集代理、備份腳本） | Kairos 開啟既有的 report 檔與日誌檔時會移除 group 寫入與 other 的所有權限（`0644` → `0640`，`0600` 維持不變），`agents.json` 與 `.retention-guard*.json` 每次重寫都是 `0640`。需要讀取的帳號請加入 Kairos 的群組（例如 `usermod -aG kairos <帳號>`）或以 ACL（`setfacl`）授權。既有目錄的權限不會被改變，但其他使用者可存取時（例如 `0755`）啟動會記 `WARN` 並建議 `chmod 750` |
| 5 | Kairos、Pulse | **啟動失敗的結束代碼依原因區分**（用法錯誤 2、設定錯誤 3、憑證錯誤 4，Kairos 另有連接埠衝突 11），systemd 不再重試 2、3、4 | 以結束代碼判斷啟動失敗的腳本或監控；沿用舊版 unit 檔的部署 | 依設計包 `design/design-system/cli-style.md` §8 的結束代碼表（不使用 sysexits）：用法錯誤 **2**（Kairos 原為 1，Pulse 不變）；設定錯誤 **3**（原為 1：設定檔無法載入或驗證不通過；Kairos 另含第 1 項的拒絕啟動，Pulse 另含無效的 `server.domain`）；憑證錯誤 **4**（原為 1：Kairos 的伺服器憑證、私鑰、client CA、CRL，Pulse 的 `ca_cert`、客戶端憑證、私鑰、SAN 不符與 `agent.id`）；Kairos 連接埠已被佔用 **11**（原為 1，仍會重試）。對照表見根目錄 README「結束代碼」。新版 `kairos.service`、`pulse.service` 加上 `RestartPreventExitStatus=2 3 4`：服務停在 `failed`（`status=2`／`3`／`4`），修正後執行 `systemctl restart`。升級時請以發佈包的 unit 覆寫 `/etc/systemd/system/` 下的檔案（自訂內容放在 `systemctl edit` 的 drop-in）並 `systemctl daemon-reload`；Pulse 多實例時依 `pulse/packaging/INSTALL.md` 第 9 節重新產生每個 `pulse-N.service`。只判斷「非 0」的腳本不受影響 |
| 6 | Kairos（影響讀取資料檔的工具） | **JSONL 每行新增 `schema`；新版 Pulse 連新版 Kairos 時 report 改為通用指標（`schema` 2）**（A3、A4、O6） | 直接讀取 `data/reports-*.jsonl` 或 `agents.json` 的工具（匯入、報表、監控腳本） | 每一行最前面多了 `schema`（`1` 為原本的 `cpu_usage`／`total_memory`／`used_memory`，`2` 為 `metrics` 陣列，記憶體等改以 bytes 表示）。兩端都升級後新資料為 `schema` 2，混用期間同一個檔案中兩種格式並存：升級 Kairos **前**請讓讀取工具依 `schema` 分流（沒有 `schema` 的舊資料視同 1，不認得的 `schema` 略過）。`agents.json` 每個 agent 多了 `proto` 與 v2 的主機資訊，`last_report` 的三個值在 v2 由標準指標換算（缺少時省略），另有 `metrics` 指標數。格式說明見 `kairos/README.md`「資料輸出」 |
| 7 | Kairos | **連線被拒與全域上限的日誌文字改寫**（A7） | 以日誌全文或固定字串比對來告警的監控（例如 grep「已達連線上限」） | 每 IP 上限：「來自 <位址> 的同時連線數已達上限 N（server.max_connections_per_ip），拒絕新連線」→「來自 <位址> 的同時連線數／握手中（尚未通過 hello）的連線數已達上限 N（<適用的設定>），在 TLS 握手前拒絕新連線」，並附允許清單的提示；全域上限：「已達連線上限 N，暫停接受新連線」→「已達同時連線上限 N（server.max_connections），暫停接受新連線；…」，恢復時另附累計暫停次數與時間；握手中上限的訊息加上「（server.max_connections 的一半）」與「在 TLS 握手前」。請改以「已達同時連線上限」「已達上限」等新字串或日誌等級比對。另外 agent 上線的 `INFO` 日誌結尾多了「，協定 vN」（比對前綴者不受影響） |
| 8 | Kairos、Pulse | **發佈包檔名加入版本，輸出位置與 `SHA256SUMS` 改變**（B3、D2） | 依檔名下載、解壓或比對雜湊的自動化腳本 | 檔名 `kairos-<commit>-linux-x86_64.tar.gz` → `kairos-<版本>-<commit>-linux-x86_64.tar.gz`（Pulse 相同），解壓後的目錄名稱也隨之改變；`--version` 由 `0.1.0 (<commit>)` 變為 `0.2.0 (<commit>)`。發佈處的 `SHA256SUMS` 改為兩個產品共用的一份並附 `SHA256SUMS.sig`，驗證步驟改為先以 `ssh-keygen -Y verify` 驗簽章，再 `sha256sum -c --ignore-missing`（INSTALL 第 2 節） |

**建議的升級順序**（協定 v2 以版本協商相容 v1，Kairos 與 Pulse 不必同時升級，以下順序是建議而非必要，理由見上方「版本相容矩陣」）：

1. 檢查 Kairos 是否符合第 1 項（`server.listen` 非 loopback 卻沒有 `auth.client_ca`），先啟用 mTLS。
2. 檢查第 4 項：有其他帳號讀取 Kairos 資料檔或日誌時，先把它加入 Kairos 的群組；第 6 項：讀取 JSONL 的工具先改為依 `schema` 分流；第 7 項：更新比對日誌文字的監控；第 8 項：更新下載與驗證發佈包的腳本。
3. 升級 Pulse（第 2、3 項；換上新版 `pulse.service`），確認 `systemctl status pulse` 的 `Status:` 為「已註冊：…」、日誌的信任來源正確。連舊版 Kairos 時註冊日誌為「協定 v1」。
4. 升級 Kairos（換上新版 `kairos.service`）。之後 Pulse 重連時註冊日誌變為「協定 v2」，JSONL 開始出現 `schema` 2 的行；尚未升級的 Pulse 繼續以 v1 運作。
5. 調整依賴離線判定時間（第 3 項）與結束代碼（第 5 項）的監控與腳本。

另有一項不需修改設定的行為改變：**啟用 mTLS 時 `server.max_connections_per_ip` 只計算尚未通過 hello 的連線**，通過後改依憑證身分計數（A7，見 Kairos「變更」）；同一個 NAT 後的 agent 不再從第 33 台起被斷線。

兩個產品之間的協定新增 **v2**，並以版本協商相容 v1（見「共用」與上方「版本相容矩陣」）：新版與舊版混用時自動以 v1 通訊、線上格式與舊版相同；兩端都是新版時才使用 v2（通用指標、主機資訊、`server_time`）。新舊版本混用的其他限制見各產品 README 的「升級注意事項（與 … 混用版本）」。

### Kairos

#### 新增

- `auth.allow_unauthenticated`（預設 `false`）：明確允許未啟用 mTLS 時監聽非 loopback 位址，只用於隔離的測試環境；允許時記 `WARN`，啟用 mTLS 時不生效（S1）。
- 伺服器憑證與 CRL 的到期預警（O1）：啟動時、熱重載套用新設定時與之後每個重載週期（停用熱重載時每小時）讀取伺服器憑證的 `notAfter` 與每份 CRL 的 `nextUpdate`，預警期間內記 `WARN`、已過期記 `ERROR`、恢復時記 `INFO`；每個檔案各自節流（狀態改變時立即記錄，其餘每 24 小時最多一次）。新設定 `server.cert_expiry_warning_days`（預設 30）、`auth.crl_expiry_warning_days`（預設 7），範圍 0～365，0 為不預警。CRL 過期時仍由 rustls 拒絕所有客戶端憑證（fail closed），行為不變。
- 發佈包新增 `kairos-crl.service` 與每週執行的 `kairos-crl.timer` 範例：產生 CRL → 以 CA 驗簽並確認 `nextUpdate` 在未來 → 原子改名 → 部署，任一步失敗即停止（INSTALL 第 10.3 節）。
- Unix 上伺服器私鑰（`server.key_path`）權限過寬時，啟動與熱重載換了私鑰時記 `WARN` 並附 `chown root:kairos`／`chmod 640` 的建議，不拒絕啟動（S8）。
- `kairos.service` 新增 `RestartPreventExitStatus=2 3 4`（用法、設定、憑證錯誤不重試）。
- 每 IP 上限的網段允許清單（A7 過渡方案，設計包 D14、D15、D22）：新設定 `server.per_ip_allowlist`（CIDR 清單，IPv4／IPv6，只寫位址表示單一位址；環境變數以逗號分隔）與 `server.max_connections_per_allowlisted_ip`（預設 4096，範圍 1～65535）。清單內的位址改用實際上限 min(`server.max_connections_per_allowlisted_ip`, `server.max_connections`)，新連線可連續的條數隨之為 `max(32, 2 × 實際上限)`，未啟用 mTLS 時以來源 IP 計算的 report 額度也以此為倍數。網段格式錯誤（含主機位元不為 0）時拒絕啟動並寫出第幾項與原因；清單非空且前者大於 `server.max_connections` 時啟動記一次 `WARN`（設計包 `KRS-W0203` 的情境），清單為空時不檢查、不警告。
- 被拒連線的計數（供日後 S20 與 `/metrics`）：在記憶體中分「因每 IP 上限」（依來源位址，附上限與是否在允許清單內；新連線速率另依來源計數）與「因全域上限」（暫停接受新連線的次數與累計時間、握手中上限拒絕的次數）累計，提供內部的快照 API；恢復接受新連線的日誌附上累計暫停時間，關機時記一行摘要。
- 啟動時以 `INFO` 記錄每 IP 上限的計數方式（是否只計算握手中的連線）、實際上限與允許清單。
- 協定 v2 與版本協商（A3）：依 hello 的 `proto`／`proto_min`／`proto_max` 協商（支援 1～2），`hello_ack` 回協商結果；v2 另回 `features`（`["metrics"]`）與 `server_time`（RFC 3339 UTC 毫秒，設計包 §12 #4）。舊版 Pulse 協商為 v1，`hello_ack`、report 格式與錯誤處理與舊版完全相同；協商為 v1 時 hello 中的 v2 主機資訊一律忽略（不檢查型別），與舊版 Kairos 相同。
- 通用指標（A4）：v2 且雙方都宣告 `metrics` 時，report 為 `metrics: [{name, value, unit, labels}]`，驗證名稱、單位、label、數量（1～256）、重複與標準指標的單位與範圍（違反時 `invalid_report`），原樣保存到 JSONL（`schema` 2，數字保留原本的表示）。
- v2 hello 的主機資訊（設計包 `enrollment-ux.md` §12 #5、`screens.md` §24 #15）：`host_fingerprint`、`display_name`、`tags`、`report_interval_ms`、`os` 依 PROTOCOL.md 的限制驗證（違反時 `bad_request`），寫入 `agents.json`；每個 agent 另記錄協商出的 `proto`。
- 同一個 `agent_id` 的主機指紋與狀態表中記錄的不同時記 `WARN`「主機指紋與上次不同」（每個 agent 節流），用於發現由映像檔複製、共用 `agent.id` 的主機。

#### 變更

- **（破壞性）** 未啟用 mTLS 又監聽非 loopback 位址時拒絕啟動（S1，見上表第 1 項）。
- **（破壞性）** Unix 上 report JSONL、`agents.json`、`.retention-guard*.json` 與日誌檔以 `0640` 建立，既有的 report 檔與日誌檔開啟時收緊到最多 `0640`；自行建立的資料目錄與日誌目錄（含執行中被刪除後重建）為 `0750`；既有目錄對其他使用者開放時記 `WARN`（S3，見上表第 4 項）。`agents.json` 與日期保護狀態檔的暫存檔建立時就是 `0640`，沒有暫存期間權限過寬的空窗。Windows 沒有對應的 mode，README「檔案權限」說明以 `icacls` 設定目錄 ACL。
- **（破壞性）** 啟動失敗的結束代碼依原因區分（見上表第 5 項）：命令列參數錯誤 1 → 2、設定錯誤 1 → 3、伺服器憑證／私鑰／client CA／CRL 載入失敗 1 → 4、`server.listen` 的連接埠已被佔用 1 → 11。背景工作異常、看門狗強制結束與其他啟動失敗（例如資料目錄、日誌目錄無法建立）仍為 1，第二次訊號強制結束仍為 130。
- 日誌依 UTC 日期分檔改由 Kairos 自行實作（`DailyLogFile`，取代 fern 的 `date-based`，以便指定新檔案的權限），檔名與保留規則不變；只在一筆日誌的開頭檢查日期，同一行不會被拆到兩個檔案。
- 協定型別、驗證與日期保護改用共用 crate（見「共用」），線上行為與日誌內容不變；`cpu_usage` 在線上統一為 f64、保存仍為 f32，接受範圍與 JSONL 輸出不變（A14）。
- **（行為改變）** 啟用 mTLS 時改依憑證身分計數（A7 根本解，設計包 D13）：`server.max_connections_per_ip` 只計算握手前、握手中與等待 hello 的連線（慢速握手防護，K-S2 仍有效）；hello 通過、`agent_id` 與憑證綁定後歸還來源位址的名額，改依 `agent_id` 計數，同一身分的多條連線依既有的 `superseded` 行為處理。同一個 NAT 後的 agent 數只受 `server.max_connections` 限制。設定鍵與預設值不變；為了 NAT 而調高 `server.max_connections_per_ip` 的 mTLS 部署可改回預設。未啟用 mTLS 時行為不變（每 IP 上限涵蓋整個連線）。
- **（破壞性）** JSONL 每行新增 `schema`（O6）：`1` 為三欄位的 report，`2` 為通用指標；新版 Pulse 連線時寫成 `schema` 2（見上表第 6 項）。
- **（破壞性）** 連線被拒的日誌改寫（見上表第 7 項）：每 IP 上限寫出計算的是「同時連線數」還是「握手中的連線數」、適用的設定（`server.max_connections_per_ip` 或允許清單），並提示可加入 `server.per_ip_allowlist`；全域上限與握手中上限的日誌寫出對應的設定（`server.max_connections`）。依賴舊日誌全文比對的監控請更新。
- `unsupported_version` 的 `message` 改為伺服器支援的範圍 `supported proto: 1-2`，並另帶 `proto_min`、`proto_max`（舊版 Pulse 忽略多出的欄位）；只送 `proto: 2`（沒有範圍）的 client 改以 v2 接受（舊版回 `unsupported_version`）。
- agent 上線的 `INFO` 日誌結尾加上「，協定 vN」；版本不受支援的 `WARN` 附上 client 宣告的範圍與本伺服器支援的範圍。

#### 修正

- Linux 上資料目錄被換成同名檔案等情況下，report 檔路徑查詢失敗（例如 `NotADirectory`）時沒有重新開檔，寫進已刪除的檔案卻仍回 `ack`。現在以路徑查詢失敗時一律重開（重開失敗則回 `storage_unavailable`），Unix 上另比對裝置與 inode，偵測檔案被替換。
- 命令列參數含非 UTF-8 字元時 panic（Q5）：改用 `args_os`，`--config <path>` 可接受非 UTF-8 路徑；其他無法辨識的參數印出用法後結束。

#### 文件

- README 不再寫死測試數量；修正 `agents.json` 暫存檔名、快速開始的啟動輸出、CI 的 `cargo deny check` 等與程式不一致的說明（Q8）。
- 心跳、離線判定的範例數值依 Pulse 的新預設（15 秒、75000 ms）更新；私有 CA 一節說明新版 Pulse 設定 `ca_cert` 時已預設只信任私有 CA。
- INSTALL 的權限總覽補上 Kairos 產生的檔案、私鑰權限的理由與確認清單，新增 CRL 定期更新（10.3）與 unit 的 `RestartPreventExitStatus`。

### Pulse

#### 新增

- 敏感檔案的權限檢查（S4）：`server.client_key` 與 `agent.id` 權限過寬時記 `WARN`（不阻止啟動、不自動修改）。Unix：其他使用者有任何權限、群組有寫入或執行權限時警告，群組可讀只在擁有者為 root 時允許（`root:pulse 640`）。Windows：讀取 DACL，Everyone、Authenticated Users、BUILTIN\Users 等一般使用者群組有讀取或寫入的允許項目（或 NULL DACL）時警告；不計算拒絕項目與有效權限（可能多報）。`--print-agent-id` 的 stderr 警告也涵蓋 Windows。
- 啟動時與斷線後第一次重連加上 0～min(心跳, 5 秒) 的隨機延遲，Kairos 重新啟動後大量 agent 不會在同一秒內重連（A11）。啟動延遲期間收到結束要求立即結束。
- 系統資訊取樣的逾時（Q4）：每次刷新逾時為 clamp(`report.interval`, 2 秒, 10 秒)，卡住時記 `WARN` 並略過該次 report（heartbeat 照常），期間不再發起新的刷新，每 10 分鐘提醒一次；恢復時記 `INFO` 並重建 CPU 基準。
- 啟動後立即以 sd_notify 送出 `Status:`「連線中」（O2），`systemctl status pulse` 在第一次註冊前也有狀態行。
- `pulse.service` 新增 `RestartPreventExitStatus=2 3 4`（用法、設定、憑證錯誤不重試）。
- 日誌初始化之後才發生、使 Pulse 以非 0 結束的錯誤（例如憑證錯誤），除了寫日誌也另寫一行 `[ERROR] …` 到 stderr；`pulse.service` 的 stdout 不進 journal，`journalctl -u pulse` 因此看得到服務停在 `failed` 的原因。
- 協定 v2 與版本協商（A3）：hello 送 `"proto":1`（舊版 Kairos 只接受 1）加上 `proto_min: 1`、`proto_max: 2`、`features: ["metrics"]`，依 `hello_ack` 的 `proto` 與 `features` 選擇 report 格式；連舊版 Kairos 時照舊送三欄位。註冊成功的日誌加上「協定 vN」。
- 通用指標（A4）：Kairos 支援時，report 改送 `system.cpu.usage`、`system.memory.total`／`used`（bytes），以及原本只在啟動時印在本機的磁碟容量（`system.filesystem.total`／`available`）、網路累計流量（`system.network.received`／`transmitted`）與元件溫度（`system.temperature`）。篩選規則同啟動時的 INFO 清單，各類最多 32 個；整行超過 65536 bytes 時捨去磁碟／網路／溫度指標。
- v2 hello 的主機資訊：`host_fingerprint`（Linux `/etc/machine-id`、Windows `MachineGuid` 加上用途字串後的 SHA-256，不送原始值；使用 rustls 已內建的 ring，不新增依賴）、`report_interval_ms`、`os`（sysinfo 的作業系統名稱、版本、核心版本、CPU 架構），以及新設定 `agent.display_name`、`agent.tags`（預設不送；不符合 Kairos 的限制時以結束代碼 3 拒絕啟動）。
- v2 的 `hello_ack` 帶 `server_time`：與本機時鐘相差超過 5 分鐘時記 `WARN`（設計包 PF03）。

#### 變更

- **（破壞性）** `server.use_system_roots` 預設改為依 `ca_cert` 決定：設定了 `ca_cert` 時只信任 `ca_cert`，未設定時信任作業系統信任庫；明確設定時以設定為準（S5，見上表第 2 項）。啟動時以 `INFO` 記錄實際採用的信任來源。
- **（破壞性）** `heartbeat.interval` 預設由 1 秒改為 15 秒（A11，見上表第 3 項）。
- **（破壞性）** 設定錯誤（`pulse.toml`／環境變數的語法、型別、範圍錯誤，`log.level` 無效，`use_system_roots = false` 卻未設定 `ca_cert`）的結束代碼由 1 改為 3；憑證錯誤（`ca_cert`、客戶端憑證或私鑰無法讀取、格式錯誤、不成對，SAN 與 agent_id 不符，`agent.id` 無法讀取、內容不合法或無法建立）由 1 改為 4（依設計包 `design/pulse/cli-spec.md` §1.3，見上表第 5 項）。用法錯誤仍為 2，日誌目錄無法建立等其他啟動失敗與內部錯誤仍為 1。
- `server.domain` 不是有效的 DNS 名稱或 IP 位址時，在載入設定時就以設定錯誤（3）結束，而不是等到建立 TLS 設定時才失敗。
- 第一筆 report 的 CPU 使用率改為涵蓋一個完整的 `report.interval`（Q3）：啟動時先建立基準，第一次取樣在一個 `report.interval` 之後，因此啟動後要等一個 `report.interval` 才有第一筆 report。
- 連續失敗達 `retry.max_retries` 後的 `ERROR` 日誌每 10 分鐘最多一則，其餘改記 `WARN`，下一則 `ERROR` 附上次數；連線穩定 30 秒以上後重新起算（Q6）。
- 伺服器送來不認得的訊息類型時改記 `DEBUG`（獨立節流），壞掉的訊息仍記 `WARN`；兩者都照舊忽略（A14）。
- 協定型別、驗證與日誌日期保護改用共用 crate（見「共用」），線上行為與日誌內容不變。
- `hello_ack` 的 `proto` 不在 1～2 時才視為版本不符（`proto_mismatch`，原為不等於 1），提示文字改為支援的範圍。

#### 文件

- INSTALL：`client.key`、`agent.id` 改為 `root:pulse 640`（服務帳號唯讀），附舊安裝的遷移指令；舊配置 `pulse:pulse 600` 仍可運作、不會警告（S8）。
- README 不再寫死測試數量，並新增測試防止再寫回去（Q8）；核對 sd_notify 的 `Status:` 文字與 unit（O2）。
- README 的「本版的行為改變」併入本檔案。

### 共用

#### 新增

- `crates/kairos-proto`（`publish = false`）：協定常數、`ClientMsg`／`ServerMsg`／`ErrorCode`、訊息解析、hello／ts／report 欄位驗證、禁止字元表、UUID 格式與 `agent_id` 正規化，Kairos 與 Pulse 共用一份實作（A8）。
- `tests/e2e`（`kp-e2e`，`publish = false`）：端對端測試，以實際編譯出的 `kairos`、`pulse` 執行檔在 127.0.0.1 的隨機埠上以 mTLS 互連（Q1）：註冊（協商為協定 v2）、heartbeat、report 寫入 JSONL（`schema` 2）、CRL 熱重載撤銷、Kairos 重啟後 Pulse 自動重連、兩端優雅關機、協定 v1 相容（以送出舊版 Pulse 訊息的假 client 與新版 Pulse 同時連線），以及兩端啟動時的結束代碼。只用標準函式庫與 workspace 已使用的 `serde_json`、`rustls`，不連外部網路；Linux 與 Windows 都會執行，整套約 10 秒。說明見根目錄 README「端對端測試」。CI 先建置兩個執行檔，再以獨立步驟執行端對端測試。
- `crates/date-guard`（`publish = false`）：依日期命名檔案的保留清除規則與時鐘跳躍／回撥保護（DateGuard），Kairos 的 report 檔與日誌、Pulse 的日誌共用（A8）。狀態檔格式不變，Kairos 仍接受舊版格式。
- **協定 v2**（A3、A4）：`PROTOCOL.md` 新增「版本協商」（規則、features、相容矩陣、舊端忽略新欄位的行為）、v2 hello 欄位限制、「主機指紋」的計算方式與測試向量、「report（通用指標）」的格式、上限與標準指標表，「範例」改為 v1、v2、錯誤三個小節。`kairos-proto` 新增 `negotiate`（`client_proto_range`、`negotiate`、`Session`）、`metrics`（`Metric`、`validate_metrics`）、`HelloExt`／`HostInfo`／`ReportBody` 與對應的驗證；v2 欄位解析失敗不會讓整則訊息失敗，協商為 v1 時與 v1 實作的行為相同。測試從規格解析 v1／v2 向量、上限、標準指標表與主機指紋範例，並以凍結的 v1 型別定義驗證新舊混用。

#### 變更

- 協定規格只留一份：`crates/kairos-proto/PROTOCOL.md`。`kairos/PROTOCOL.md`、`pulse/PROTOCOL.md` 改為指向它的說明，發佈包內附的仍是完整規格。協定測試以 `include_str!` 直接從規格的「範例」節解析測試向量、比對「錯誤碼」表，取代兩個產品各自寫死的複本。
- 規格中 Pulse 讀取閒置的範例數值依新的預設心跳更新（15 秒時為 30 秒）。

### 發佈與建置

#### 新增

- **版本號**：Kairos、Pulse 改為 0.2.0 並採用 SemVer，`--version` 為 `<名稱> 0.2.0 (<commit>)`；共用 crate 維持 0.1.0（內部 crate，不單獨發佈）。版本規則、打 tag 與發佈流程見 `RELEASING.md`（B3）。
- **簽章**：發佈處提供 `SHA256SUMS.sig`（`ssh-keygen -Y sign`，namespace `kairos-pulse-release`）。INSTALL 第 2 節的驗證改為先以 `ssh-keygen -Y verify` 驗簽章、再 `sha256sum -c --ignore-missing`；發佈公鑰 `allowed_signers` 須從另一個管道取得並核對指紋（D2）。
- **第三方授權聲明**：發佈包新增 `THIRD-PARTY-LICENSES`（cargo-about 依 `Cargo.lock` 產生，含各依賴的授權全文），INSTALL 的安裝與升級步驟一併把它與 `CHANGELOG.md` 複製到 `/opt/<產品>/`（B2）。
- **SBOM**：發佈包新增 `<名稱>.cdx.json`（CycloneDX 1.5，cargo-cyclonedx 產生），發佈處另附一份 `<名稱>-<版本>-<commit>-linux-x86_64.cdx.json`（D4）。
- `packaging/release.sh`：檢查工作目錄乾淨、執行 fmt／clippy／test（與 CI 相同，端對端測試分開執行）／deny 後打包兩個產品，提供私鑰時簽章；`packaging/sign-release.sh`：簽章與驗證；`packaging/check-reproducible.sh`：以兩個全新的建置環境各建置一次並比對雜湊（D3、D4）。
- CI 新增 `package` job：Linux 與 Windows 的 fmt、clippy、test 與 cargo deny 都通過後才打包，產出兩個發佈包、SBOM 與合併版 `SHA256SUMS`（artifact，不簽章）；push 到 `main` 與手動觸發時另跑可重現性檢查（D3、D4）。

#### 變更

- **（破壞性）** **發佈包檔名加入版本**：`kairos-<commit>-linux-x86_64.tar.gz` → `kairos-<版本>-<commit>-linux-x86_64.tar.gz`（Pulse 相同），依檔名下載或解壓的自動化腳本需要調整（見上表第 8 項）。
- 兩個產品的打包腳本都輸出到 repo 根目錄的 `dist/`，`SHA256SUMS` 改為兩個產品共用的合併清單（修正每次打包都覆寫成只剩單一套件的問題，D2）。
- 建置 image 以 `@sha256:` digest 固定，唯一需要 `apk add` 的 GNU tar 鎖定版本（D4）。
- 建置來源改用 `git archive` 匯出的 commit 內容，不再掛載工作目錄：Windows（`core.autocrlf=true`）上打出的發佈包中文件與範例設定原本是 CRLF，Pulse 嵌入的預設設定也因此與 Linux 上建置的不同；現在與主機的行尾設定無關。
