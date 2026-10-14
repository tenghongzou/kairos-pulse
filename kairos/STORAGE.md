# Kairos 資料儲存（SQLite）

本文件說明 Kairos 0.3 起的 report 儲存、主機清冊、彙總、保留期限與容量上限（商業審計 A1、A6、A9），at-least-once（A5，協定 feature `durable`）的去重與補送資料的處理，以及匯入 0.2.x 的 JSONL（`kairos reports import`，第 13.1 節）。設定的完整清單見 [README.md](README.md)「設定」。

## 1. 架構

```text
連線（connection.rs）
  │ ReportSink::submit(ReportRecord)        ← 佇列 1024 筆；WriteTicket 讓逾時的要求可以取消
  ▼
writer task（store::run_store_writer，單一）
  │ 取出佇列中目前所有的要求（最多 1024 筆）→ 一批 = 一個交易（group commit）
  │ spawn_blocking：ReportStore::write_batch（不佔用 tokio 的 async worker）
  │ commit 成功後才逐筆回覆 → 連線回 ack
  │ （可選）把寫入成功的 report 轉給 JSONL 鏡像（盡力而為，不影響 ack）
  │ 每 100 ms 做一步維護：彙總寫出、容量上限、保留期限（每步一個短交易）
  │ 每 30 秒：把狀態表的快照同步到主機清冊
  ▼
SqliteStore（store/sqlite.rs）→ <storage.data_dir>/kairos.db（WAL）
                                 背景 checkpoint 執行緒（另一條連線）
```

- **抽象介面**：連線只認得 `ReportSink`（送出、等待結果、逾時取消），不知道後端。後端實作 `trait ReportStore`（`write_batch`、`sync_inventory`、`maintain`、`close`，同步 API、一批一個交易）。日後換成時序資料庫時實作這個 trait 即可，連線與協定不需修改。
- **後端選擇**（`storage.backend`）：`sqlite`（預設）或 `jsonl`。`jsonl` 是舊版的按日 JSONL writer（`storage.rs`），保留給相容與回復使用，沒有主機清冊與彙總；durable 的去重只在記憶體中、範圍有限（第 5.1 節）。
- **其他連線**（0.4 起，主控台 API，`API.md`）：唯讀連線池（WAL 讀者，最多 4 條同時查詢，不擋住寫入）；以及一條低頻寫入的連線（token 的最後使用時間每 60 秒、token 稽核旗標與被拒的憑證每 10 秒，各一個短交易，busy 最多等 250 ms）。report、彙總、清冊同步仍只由 writer 寫入；主控台只寫 `api_tokens` 與 `hosts.cert_rejected_at`／`cert_rejection` 這兩個 writer 不碰的欄位。`kairos api-token` 也是短交易的寫入者（可在 Kairos 執行中使用）。主控台的連線只經由 SQLite 開啟（不以 `std::fs` 開關資料庫檔），否則會放掉同一個行程中 writer 的 fcntl 鎖（第 13.1 節的獨占鎖偵測因此失效）。
- **JSONL 鏡像**（`storage.jsonl_mirror = true`，只在 `sqlite` 時有效）：寫入 SQLite 成功（`Written`）的 report 另外交給 JSONL writer，格式、檔名、`storage.retention_days` 與 `storage.max_daily_bytes` 的規則與 `backend = "jsonl"` 完全相同（`retention.rs` 的保留清除與時鐘跳躍保護照常運作）。鏡像是盡力而為：鏡像的佇列已滿或寫入失敗時只記 `WARN`／`ERROR`，**不影響 `ack`**；重複的 report（`Duplicate`）不寫入鏡像。

### 1.1 `ack` 的語意（PROTOCOL.md「流程」第 2 點）

| writer 的結果 | 連線的回應 | 何時 |
|---|---|---|
| `Written` | `ack` | 交易已 commit |
| `Duplicate` | `ack` | 與已保存的一筆重複（同一主機、同一個去重鍵），沒有再寫入，見第 5 節 |
| `Failed` | `storage_unavailable`（確定未保存） | commit 之前失敗（交易回復）；commit 時回報磁碟已滿、`BUSY`／`LOCKED`、唯讀（交易回復）；資料庫無法開啟；這一筆被拒（主機清冊或序列數已達上限） |
| `Unknown` | 不回應、關閉連線 | commit 時發生其他錯誤（例如 I/O 錯誤），無法確定是否已寫入；連線重新開啟、快取重新載入 |

`storage.sqlite_synchronous = "normal"`（預設）時，WAL 模式下 commit 只把資料交給作業系統、不 fsync：Kairos 當掉不會遺失已 `ack` 的資料，作業系統當機或斷電可能遺失最後幾個交易。這與 JSONL 的「write + flush」相同，符合 PROTOCOL.md 的「已交給作業系統，但不保證已同步到磁碟」。需要斷電也不遺失時設為 `full`（每次 commit 都 fsync WAL，延遲較高）。

## 2. 檔案

| 檔案 | 說明 |
|---|---|
| `data/kairos.db` | SQLite 資料庫（原始 report、彙總、主機清冊）。Unix 上 `0640`，第一次啟動時建立；既有的檔案開啟時收緊到最多 `0640` |
| `data/kairos.db-wal`、`kairos.db-shm` | WAL 與共用記憶體檔，SQLite 建立時沿用資料庫檔的權限（`0640`）。Kairos 正常關機時刪除。**備份或搬移時不可只複製 `kairos.db`**（見第 9 節） |
| `data/agents.json` | 狀態表的即時快照（每 5 秒），與 0.2 相同；重新啟動後含最近 24 小時內出現過的主機（見第 6 節） |
| `data/reports-YYYY-MM-DD.jsonl`、`.retention-guard.json` | 只在 `backend = "jsonl"` 或 `jsonl_mirror = true` 時寫入 |

資料目錄所在的檔案系統必須支援共享記憶體與 POSIX 檔案鎖（本機磁碟；**不要放在 NFS／SMB 等網路磁碟**）。無法啟用 WAL 時 Kairos 拒絕啟動。

Kairos 執行中把 `kairos.db` 刪除或替換時（Unix 上以裝置與 inode 比對，與 JSONL 的 `needs_reopen` 相同），下一批寫入前記 `WARN` 並建立新的資料庫，不會寫進已刪除的檔案後回 `ack`；記憶體中尚未寫出的彙總一併捨棄。

## 3. Schema（v3）

`PRAGMA application_id = 0x4B525331`（「KRS1」），`PRAGMA user_version` 為 schema 版本。時間一律為 Unix 毫秒（彙總的 `bucket` 為 Unix 秒），UTC。

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value) WITHOUT ROWID;
-- schema_upgraded_by：最後一次升級 schema 的 Kairos 版本；retention_guard：日期保護的狀態（JSON）

CREATE TABLE hosts (                       -- 主機清冊（A6），每個 agent_id 一列
    host_id            INTEGER PRIMARY KEY,
    agent_id           TEXT NOT NULL UNIQUE, -- 小寫；啟用 mTLS 時與客戶端憑證的 SAN 綁定
    hostname           TEXT NOT NULL,
    agent_version      TEXT,
    proto              INTEGER,             -- 最近一次協商出的協定版本
    host_fingerprint   TEXT,                -- 以下為 v2 hello 的主機資訊（最後已知的值）
    display_name       TEXT,
    tags               TEXT,                -- JSON 陣列
    os                 TEXT,                -- JSON 物件
    report_interval_ms INTEGER,
    peer               TEXT,                -- 最後的來源 ip:port
    identity_verified  INTEGER NOT NULL DEFAULT 0, -- 1：agent_id 以 mTLS 客戶端憑證驗證
    online             INTEGER NOT NULL DEFAULT 0,
    first_seen         INTEGER NOT NULL,
    connected_at       INTEGER,
    last_seen          INTEGER NOT NULL,
    offline_after_ms   INTEGER,
    last_report        TEXT,                -- agents.json 的 last_report（JSON）
    rollup_until       INTEGER NOT NULL DEFAULT 0 -- 已寫出彙總的時間，見第 7 節
);
CREATE INDEX hosts_last_seen ON hosts (last_seen);

CREATE TABLE series (                      -- 指標序列：名稱＋單位＋label 的組合，各存一次
    series_id INTEGER PRIMARY KEY,
    name      TEXT NOT NULL,
    unit      TEXT NOT NULL,
    labels    TEXT NOT NULL,               -- 沒有 label 時為空字串，否則為依名稱排序的 JSON 物件
    UNIQUE (name, unit, labels)
);

CREATE TABLE boots (                       -- 去重鍵的 boot_id（每台主機每個 boot 一列）
    boot_ref INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id  INTEGER NOT NULL,
    boot_id  TEXT NOT NULL,
    UNIQUE (host_id, boot_id)
);

CREATE TABLE reports (                     -- 原始 report，每筆一列（A1）
    id         INTEGER PRIMARY KEY,
    host_id    INTEGER NOT NULL,
    ts         INTEGER NOT NULL,           -- 取樣時間（第 5.2 節）：durable report 為 client 的 ts，其他為伺服器收到的時間
    seq        INTEGER NOT NULL,           -- 協定的 seq（每條連線從 1 起）
    agent_ts,                              -- client 的 ts：RFC 3339 毫秒 UTC 時存成 INTEGER，否則原樣存 TEXT
    format     INTEGER NOT NULL,           -- 1：協定 v1 的三欄位；2：通用指標
    body       BLOB NOT NULL,              -- 緊湊編碼，見 3.1
    boot_ref   INTEGER,                    -- 去重鍵（durable），沒有時為 NULL
    report_seq INTEGER,
    recv_lag   INTEGER                     -- 收到的時間 − ts（毫秒）；相同時（即時資料）為 NULL。JSONL 的 recv_at = ts + recv_lag
);
CREATE INDEX reports_host_ts ON reports (host_id, ts);
CREATE UNIQUE INDEX reports_dedup ON reports (boot_ref, report_seq) WHERE boot_ref IS NOT NULL;

CREATE TABLE rollup_5m (                   -- 5 分鐘彙總（A9）
    host_id INTEGER NOT NULL, bucket INTEGER NOT NULL, series_id INTEGER NOT NULL,
    vmin REAL NOT NULL, vmax REAL NOT NULL, vsum REAL NOT NULL, cnt INTEGER NOT NULL,
    PRIMARY KEY (host_id, bucket, series_id)
) WITHOUT ROWID;
CREATE TABLE rollup_1h ( ...同上... ) WITHOUT ROWID;   -- 1 小時彙總

-- v2：kairos reports import（第 13.1 節）
ALTER TABLE hosts ADD COLUMN source TEXT;  -- 'import'：由 kairos reports import 建立；由連線建立的為 NULL
CREATE TABLE import_keys (                 -- 匯入的去重鍵：主機＋取樣時間＋內容雜湊
    host_id INTEGER NOT NULL,
    ts      INTEGER NOT NULL,              -- 取樣時間（與 reports.ts 相同的規則）
    hash    INTEGER NOT NULL,              -- 內容雜湊（64 位元）
    PRIMARY KEY (host_id, ts, hash)
) WITHOUT ROWID;

-- v3：主控台 API（API.md）
CREATE TABLE api_tokens (                  -- API token（kairos api-token；秘密只存 SHA-256）
    token_id      TEXT PRIMARY KEY,        -- 公開的識別碼（16 個十六進位字元）
    name          TEXT NOT NULL,
    secret_hash   BLOB NOT NULL,           -- SHA-256(秘密)
    scope         TEXT NOT NULL,           -- 本版只有 'read'
    created_at    INTEGER NOT NULL,
    created_by    TEXT,                    -- 執行 kairos api-token 的作業系統帳號
    expires_at    INTEGER,                 -- NULL：不過期
    revoked_at    INTEGER,
    revoked_by    TEXT,
    revoke_reason TEXT,
    last_used_at  INTEGER,                 -- 主控台每 60 秒寫回
    last_used_ip  TEXT,
    logged        INTEGER NOT NULL DEFAULT 0  -- 1：建立已寫入 Kairos 日誌；2：撤銷已寫入
) WITHOUT ROWID;
ALTER TABLE hosts ADD COLUMN cert_rejected_at INTEGER;  -- 握手時被拒的憑證（設計包 §24 #3）
ALTER TABLE hosts ADD COLUMN cert_rejection TEXT;       -- JSON：reason、serial、peer、count
```

`meta` 另有 `capacity_event`（最近一次容量上限刪除，第 8.2 節）與 `retention_last`（最近一次保留清除的結果），兩者由 writer 寫入、主控台的系統健康讀取。

- 不使用 `STRICT` 表與外鍵，讓較舊的 `sqlite3` 命令列工具（例如 RHEL 8 的 3.26）也能開啟備份檔檢查。
- `seq`、`report_seq` 是 u64，以 i64 的位元形式保存（超過 2^63 時讀回要轉回 u64）。
- 每筆 report 不重複保存 `peer`、`hostname`、`agent_id`（JSONL 每行約 250 bytes 的重複字串）；`kairos reports export` 以清冊中最後的值補上。

### 3.1 report 內容的編碼（`reports.body`）

```text
format 1：cpu_usage f32 LE（4 bytes）| varint total_memory | varint used_memory
format 2：varint 個數 | 每個指標：varint series_id | tag（1 byte）| 值
          tag 0：非負整數（varint）；tag 1：負整數 v，存 varint(-v - 1)；tag 2：f64 LE（8 bytes）
```

varint 為 LEB128。數值保留收到的 JSON 數字種類，`kairos reports export` 輸出的數字與收到的相同（整數不會變成小數）。實作見 `kairos/src/store/codec.rs`。

### 3.2 Schema 版本與遷移

- 啟動時依 `PRAGMA user_version` 依序套用尚未套用的遷移（`store/sqlite.rs` 的 `MIGRATIONS`），每一版一個交易：失敗時該版完全不生效，Kairos 不啟動。
- 資料庫的版本比程式新（降版執行）時拒絕啟動，不做任何變更；請改用較新的版本，或從升級前的備份還原。
- 檔案存在但 `application_id` 不是 Kairos、又有其他資料表時拒絕使用（例如 `storage.data_dir` 指錯）。
- 新增欄位或資料表時加一個遷移，不修改既有遷移的內容。**升級 Kairos 前先備份**（第 9 節）：遷移後的資料庫無法給舊版使用。
- 目前的版本：v1（資料表、索引）、v2（`hosts.source` 與 `import_keys`，匯入工具使用；0.3.0 的開發期間加入，以 v1 建立的測試版資料庫在啟動時自動升級）、v3（0.4：`api_tokens`、`hosts.cert_rejected_at`／`cert_rejection`，主控台 API 使用）。`kairos reports import`、`kairos api-token` 開啟資料庫時同樣會升級 schema；升級到 v3 之後 0.3 的 Kairos 無法開啟（降版請從升級前的備份還原）。
- 規劃中：v4（0.5.0〔草稿〕，衝刺 4）新增註冊通道與 PKI 的資料表（`enroll_tokens`、`enroll_requests`、`pki_certificates`、`pki_cas`、`pki_crls`、`enroll_replay`、`settings`、`audit_events` 等）與 `hosts` 的生命週期欄位，規格見 [`PKI.md`](PKI.md) 第 7 節；實作後併入第 3 節。

## 4. 寫入路徑與批次策略

- **group commit**：writer 每次取出佇列中目前所有的要求（最多 1024 筆）放進一個交易（`BEGIN IMMEDIATE`）。commit 期間新進的要求在佇列中累積，成為下一批；不刻意延遲湊批次，因此低負載時延遲最低、高負載時自動變成大批次。
- **不阻塞 tokio**：每批寫入與每步維護都在 `spawn_blocking` 執行；store 在 async task 與 blocking 執行緒之間移交，同一時間只有一個執行緒使用連線。
- **逾時與取消**：與 JSONL 相同（`WriteTicket`）。連線等待超過 `timeouts.write_secs` 時，writer 尚未認領的要求被取消（保證不寫入，回 `storage_unavailable`）；已認領的再等最多 1 秒，仍沒有結果則不回應並關閉連線。PROTOCOL.md 的「回應延遲最多 8 秒」預算不變。
- **checkpoint**：寫入的連線關閉自動 checkpoint（`wal_autocheckpoint = 0`），改由背景執行緒以另一條連線每 0.5 秒執行 `wal_checkpoint(PASSIVE)`（把 WAL 寫回資料庫並 fsync，不擋住寫入）。持續寫入時背景 checkpoint 追不上最新的頁，WAL 不會從頭重寫，因此 WAL 超過約 4096 頁（16 MiB）時，由寫入的連線在兩批之間補做一次 PASSIVE checkpoint（大部分的頁已寫回並同步，停頓很短），之後的交易從 WAL 開頭重寫；WAL 檔由 `journal_size_limit` 截到 64 MiB 以內。量測：改用背景 checkpoint 前 p99 約 334 ms，之後約 1.5 ms（第 11 節）。
- **快取**：主機（`agent_id` → `host_id`）、序列與每台主機最近的 boot 存在記憶體；交易失敗時整個清空、下一批前重新載入（失敗交易中建立的編號已不存在）。

## 5. 去重與補送的歷史資料（A5）

at-least-once（A5）時 Pulse 會重送 report。Pulse 與 Kairos 協商到 feature `durable` 時，report 另帶 `boot_id`（字串，UUID，Pulse 每次啟動產生）與 `report_seq`（u64，同一個 `boot_id` 內單調遞增），重送時兩者與 `ts`、內容都不變（PROTOCOL.md「durable：at-least-once 投遞」）。Pulse 斷線期間的取樣存在本機緩衝，重新連上後依序補送，因此 `ts` 可能比收到的時間早數小時到數天（Pulse 的 `buffer.max_age_hours`，預設 168 小時）。

### 5.1 去重

連線端與後端之間只有一套介面（`kairos/src/storage.rs`）：

```rust
pub struct DedupKey { pub boot_id: String, pub report_seq: u64 }
impl ReportRecord { pub fn with_dedup(self, dedup: Option<DedupKey>) -> Self }
impl ReportSink { pub async fn submit(&self, record: ReportRecord) -> Result<PendingWrite, String> }
pub enum WriteStatus { Written, Duplicate, Failed(String), Unknown(String) }
```

- 連線（`connection.rs`）協商到 `durable` 時，以 `.with_dedup(Some(DedupKey { boot_id, report_seq }))` 附上（`boot_id` 已轉小寫；欄位缺漏或不合法時回 `bad_request`，不會走到 writer）；沒有協商時為 `None`，行為與 0.2 相同。
- 後端回 `Duplicate` 時連線照常回 `ack`，**不重寫**、不更新 `agents.json` 的 `last_report`（那是較舊的資料）；`Written` 的 durable report 也只在 `ts` 不早於目前的 `last_report` 時更新它（補送的舊資料不蓋過較新的狀態）。

**SQLite（預設）**：唯一索引保證同一主機、同一個 `(boot_id, report_seq)` 只保存一次（`boots` 把 `(host_id, boot_id)` 對應到 `boot_ref`，`reports_dedup` 為 `(boot_ref, report_seq)` 的部分唯一索引）。

- 重複的那筆（含同一批中重複）回 `Duplicate`：不寫入、**不加入彙總**、不寫入 JSONL 鏡像。
- 不同主機使用相同的 `boot_id` 與 `report_seq` 不衝突；同一主機換了 `boot_id` 後從頭編號也不衝突；舊 boot 的重送在換 boot 後仍能辨識。
- **commit 結果不明 → 斷線 → Pulse 重送**：連線不回應並關閉（`Unknown`），Pulse 重連後重送。若那個交易其實已生效，重送的那筆被唯一索引判定為 `Duplicate`，不會重複；若沒有生效，重送時正常寫入。結果不明時記憶體中的彙總沒有加入那一批的值（不知道是否已寫入），writer 在下一次寫入前從原始資料重建記憶體中的彙總（與啟動時相同，第 7 節），因此那一筆的值不論哪種情況都只算一次。「寫入逾時、但 writer 其實已寫完」的情況相同。
- 去重的範圍是**原始資料仍保存的期間**：原始 report 被保留期限或容量上限刪除後，同一個去重鍵再送來會被當成新的一筆（與 Pulse 設定的關係見第 5.4 節）。保留清除會順便刪除已沒有任何 report 引用的 `boots` 列。
- 測試：`store::sqlite::tests::duplicate_dedup_keys_are_acknowledged_but_not_stored_twice`、`after_an_unknown_commit_the_resent_report_is_rolled_up_exactly_once`、`store::tests::sqlite_storage_acks_after_commit_mirrors_to_jsonl_and_persists_the_inventory`、`connection::tests::duplicate_report_is_acked_without_updating_last_report`、端對端 `tests/e2e/tests/durable.rs`（Kairos 停機補送不缺號不重複、結果不明的重送與重新啟動後的重送都不重複）。

**`backend = "jsonl"`**：JSONL writer 以記憶體中的 `DedupIndex`（`kairos/src/dedup.rs`）去重，範圍有限：

- 每個 (agent_id, `boot_id`) 一個 256 筆的滑動視窗；比「已寫入的最大 `report_seq` − 255」更舊的重送視為已寫入（協定的明文規則；Pulse 依序送出、在途不超過 4 筆，需要重送的一定在視窗內）。
- 只在記憶體中：啟動時從最新兩個 `reports-*.jsonl` 的尾端（合計最多 64 MiB）重建；48 小時沒有新資料的 (agent_id, `boot_id`) 清除，總數超過 20 萬時清掉最久沒更新的八分之一。超出範圍的重送會重複寫入。
- 寫出中途失敗、結果不明（`Unknown`）的那筆不列入，重送時再寫一次（可能多一行不完整或重複的紀錄）：寧可重複，不可遺失。
- JSONL 鏡像（`backend = "sqlite"` 且 `jsonl_mirror = true`）不建立 `DedupIndex`：送進鏡像的都是 SQLite 已寫入（已去重）的 report。
- 兩種後端都把 `boot_id`、`report_seq` 寫在 JSONL 該行的 `ts` 之後（`kairos reports export` 亦同）。

### 5.2 時間：取樣時間與收到時間

SQLite 的 `reports.ts` 是**取樣時間**，彙總的桶、依時間的保留期限與 `kairos reports export --since` 都以它為準：

| report | `reports.ts` |
|---|---|
| 協商了 `durable`，`ts` 為 RFC 3339 且在收到時間的 −367 天～＋5 分鐘之內 | client 的 `ts`（Pulse 的取樣時間） |
| 協商了 `durable`，但 `ts` 無法解析或超出範圍（client 時鐘錯誤） | 收到的時間 |
| 沒有協商 `durable`（0.2 的 Pulse、協定 v1、停用緩衝） | 收到的時間（與 0.2 的 JSONL 相同，不受 client 時鐘影響） |

- −367 天 = Pulse `buffer.max_age_hours` 的上限（1 年）加 2 天餘裕；＋5 分鐘容忍 client 時鐘稍快（Pulse 以 `hello_ack` 的 `server_time` 檢查偏差並記 `WARN`）。
- client 原本的 `ts` 字串另外原樣保存在 `agent_ts`，收到的時間為 `ts + recv_lag`；`kairos reports export` 輸出的 `ts`、`recv_at` 與 JSONL 相同。
- 協商了 `durable` 的連線上，即時的取樣也用 Pulse 的時間：Pulse 時鐘偏差 ±5 分鐘以內時，趨勢會跟著偏移同樣的量（與補送的資料一致，不會在補送與即時的交界處錯位）。
- `hosts.last_seen`、`first_seen` 仍是伺服器的時間；JSONL（後端或鏡像）仍以 `recv_at` 的日期決定寫入哪個檔案與保留期限。

### 5.3 遲到的資料與已寫出的彙總

補送的資料、client 時鐘偏差、或結果不明後的重送，都可能讓一筆 report 的取樣時間落在**已經寫出的 5 分鐘／1 小時桶**中。處理方式（`store/rollup.rs`、`store/sqlite.rs`）：

- 每台主機的 `hosts.rollup_until` 是「已寫出的桶」的結束時間；記憶體中尚未寫出的桶一定都在它之後（每台主機的桶依時間順序寫出）。
- 取樣時間 **≥ `rollup_until`**：commit 後加進記憶體中的桶，照常寫出。
- 取樣時間 **< `rollup_until`**（遲到）：在寫入原始資料的**同一個交易**中，依桶先合併，再以 `INSERT … ON CONFLICT DO UPDATE` 累加進 `rollup_5m` 與 `rollup_1h`（`vmin = min(…)`、`vmax = max(…)`、`vsum = vsum + …`、`cnt = cnt + …`），**不覆蓋、不遺漏**；桶不存在時新增。交易回復時這些合併一併回復；重複的那筆不合併。
- 重新啟動時只從 `rollup_until` 起重新讀取原始資料補回記憶體中的桶，已直接合併的遲到資料不會再算一次。
- **補送中的桶延後寫出**：記憶體中的桶要「已結束 15 秒」且「該主機在這個桶已 15 秒沒有新值」才寫出。補送 3 天前的資料時桶早已結束，但補送進行中一直有新值，等補到下一個桶後才一次寫出整個桶；某台主機有一個桶還不能寫出時，它之後的桶也先不寫（才能推進 `rollup_until`），其他主機不受影響。這避免同一個桶被拆成很多次寫出、之後每筆都變成遲到資料逐筆 upsert。
- 測試：`late_samples_are_merged_into_flushed_rollups_by_sample_time`、`a_backfilled_bucket_is_flushed_once_after_it_is_complete`、`rollup::tests::a_bucket_being_backfilled_waits_and_blocks_only_its_own_host`。

### 5.4 保留期限、Pulse 的 `buffer.max_age_hours` 與去重範圍

三者都以取樣時間計算，關係如下（預設值：原始資料 `storage.raw_retention_days = 7`、Pulse `buffer.max_age_hours = 168`）：

- **原始資料**保留「今天與前 N 天」（UTC，`N = raw_retention_days`）：一筆取樣在它的 UTC 日期早於「今天 − N」時才被刪除，此時它的年齡一定**超過 24 × N 小時**。
- **Pulse** 在送出（含重送）前丟棄取樣時間超過 `buffer.max_age_hours` 的資料。
- 因此只要 **`buffer.max_age_hours ≤ 24 × raw_retention_days`**（預設 168 ≤ 168），Pulse 還可能送出的資料在 Kairos 都還保存著，去重一定有效，補送與重送都不會重複。
- 不滿足時（例如 1 萬台主機依第 10 節把原始資料縮短為 2 天，Pulse 仍保留 7 天）：比原始保留期限更舊的補送資料照常接受、寫入原始資料並併入彙總（5 分鐘／1 小時彙總依各自的保留期限保存，這些補送仍有價值），原始資料在下一輪保留清除（每小時）被刪除。唯一的影響是：若同一筆在原始資料被刪除**之後**又被重送（只會發生在它的 `ack` 遺失時；Pulse 收到 `ack` 就刪除），它會被當成新的一筆再寫入一次，彙總中多算一次。不會遺失資料，也沒有其他副作用。要完全避免，請把各台 Pulse 的 `buffer.max_age_hours` 設為 `24 × raw_retention_days` 以下。
- `raw_retention_days = 0`（不依時間刪除）時沒有這個限制。
- **總容量上限**（第 8.2 節）依寫入順序刪除最舊的原始 report，不看取樣時間；剛補送進來的舊資料會比較晚被刪。被容量上限刪除後的重送同上（當成新的一筆）。
- 彙總的保留期限依桶的時間：比 `rollup_5m_retention_days` 更舊的補送資料會先合併進 5 分鐘桶，在下一輪保留清除時刪除（1 小時桶保留較久）。
- 保留清除的「今天」是伺服器的時鐘（經 DateGuard 檢查），不受 client 的 `ts` 影響；client 時鐘錯誤的 `ts` 已在第 5.2 節改用收到的時間，不會讓資料一寫入就被刪除或永遠不刪。
- `backend = "jsonl"`：檔案與保留期限依 `recv_at`（收到的日期）；去重範圍為記憶體中的視窗（第 5.1 節），與保留期限無關。

## 6. 主機清冊（A6）

- 第一次收到某台主機的 report 時在 `hosts` 新增一列（`agent_id`、`hostname`、`peer`）。之後每 30 秒，writer 把 monitor 最新的狀態表快照（與 `agents.json` 同一份）同步到 `hosts`：上線、離線、hello 的資訊改變時立即寫入；`last_seen` 與 `last_report` 只在 `last_seen` 前進 60 秒以上時寫入（線上的主機每分鐘最多寫一次）。關機時最後一份快照（所有主機已離線）在 writer 結束前全部寫入。
- **分批寫入**：有變化的主機每個交易最多寫 500 列，其餘在之後的維護步驟（每 100 ms 一步，與 report 的批次交錯）逐批寫入；下一份快照到來時取代尚未寫完的部分（新的快照包含最新的狀態）。重新啟動後的第一次同步約有主機數那麼多列（1 萬台時 1 萬列），在 kp-build 上單一交易約 69 ms，分批後每個交易最多約 17 ms（第 11 節）。
- v1 的 hello 沒有主機資訊：`host_fingerprint`、`display_name`、`tags`、`os`、`report_interval_ms` 保留最後已知的值，不會被清空。
- `identity_verified`：伺服器啟用 mTLS（`auth.client_ca`）時為 1，表示 `agent_id` 與客戶端憑證的 SAN 綁定；未啟用時 `agent_id` 是自行宣告的。
- **啟動時載回**：上次異常結束時仍標為線上的主機改為離線（保留 `last_seen`）；`last_seen` 在 24 小時內的主機以「離線」狀態放回狀態表（`Registry::restore`，最多 `limits.max_agents` 台，最近出現的優先）。因此重新啟動後 `agents.json` 不再是空的，之後的 hello 記為「重新上線」，「主機指紋與上次不同」的偵測也延續到重新啟動之後。更早的主機只在清冊中。
- **上限**：`storage.max_hosts`（預設 `max(100000, limits.max_agents)`）。清冊已滿時新主機的 report 回 `storage_unavailable`，同步時不新增（記 `WARN`）。清冊不會自動刪除主機。
- **`agents.json` 的處置**：保留，格式不變，仍是「目前狀態表」的即時快照；它不再是唯一的狀態來源，只是不再「重新啟動就清空」。需要完整清冊或歷史時用 `kairos hosts list` 或查詢資料庫。

### 6.1 `kairos hosts list`

```console
$ sudo -u kairos kairos hosts list --config /opt/kairos/kairos.toml --offline-for 3d
狀態         主機名稱  AGENT_ID  IP          最後訊息
[DOWN] 離線  db-02     0a9b8c7d  10.0.3.12   4 天 2 小時前

共 1 台（離線 1）
```

以唯讀方式開啟資料庫，Kairos 執行中也可以查詢。選項：`--offline-for <時間>`（`90m`、`12h`、`3d`、`2w`，只列出失聯至少這麼久的主機）、`--wide`、`--no-header`、`--json`（純 ASCII，`schema` 為 `kairos.hosts.list/v1`，`data.hosts[]` 含 `status`、`last_seen`、`silent_secs` 等）、`--ascii`。狀態為「線上」「離線」與「未知」（清冊記錄為線上，但已超過離線門檻 + 約 2 分鐘沒有更新，例如 Kairos 未正常關機）。輸出依 `design/design-system/cli-style.md`（不輸出顏色；符號依 §4.2 決定 Unicode 或 ASCII）。結束代碼：0 正常、1 無法讀取資料庫、2 用法錯誤、3 設定錯誤、10 權限不足。

## 7. 彙總（A9）

- **兩層**：5 分鐘（`rollup_5m`）與 1 小時（`rollup_1h`），每列為一台主機、一個序列、一個時間桶的 `vmin`、`vmax`、`vsum`、`cnt`（平均 = `vsum / cnt`）。
- **哪些值**：每筆 report 的每個指標（數值以 f64 計）。v1 的三個欄位對應到標準序列：`cpu_usage` → `system.cpu.usage`（`%`），`total_memory`／`used_memory`（MB）× 1048576 → `system.memory.total`／`used`（`By`），與 v2 的指標合併成同一條趨勢。網路流量是累計計數器，彙總的是計數器本身的值（速率由使用端以前後相減計算）。
- **怎麼算**：依取樣時間（第 5.2 節）分桶。commit 成功後才把值加進記憶體中尚未寫出的 5 分鐘桶；桶結束 15 秒、且該主機在這個桶 15 秒沒有新值之後（等待排隊中的 report、補送中的桶補完）分批寫出（每步最多約 5000 列，只在主機的邊界切開，每台主機依時間順序），同一個交易中合併進 1 小時彙總（`ON CONFLICT DO UPDATE` 合併 min／max／sum／count），並把這些主機的 `hosts.rollup_until` 推進到桶的結束時間。
- **遲到的資料**（取樣時間早於 `rollup_until`）在寫入原始資料的同一個交易中直接累加進已寫出的桶（第 5.3 節）。
- **重新啟動不漏算、不重複算**：啟動時，各主機從 `rollup_until` 起重新讀取原始資料補回記憶體中的桶；已寫出的桶與直接合併的遲到資料不會再算一次（`rollup_until` 與寫出在同一個交易）。commit 結果不明之後也以同樣的方式重建（第 5.1 節）。
- **已知的近似**：1 小時桶只在其中的 5 分鐘桶寫出（或遲到資料合併）時更新。取樣時間早於原始資料保留期限的補送資料，若在它的桶寫出前（最多約 15 秒）原始資料已被保留清除刪除、Kairos 又剛好當掉，這些值不會補回彙總。

**記憶體**：每台主機每個序列每個尚未寫出的桶約 80 bytes；1 萬台 × 20 個序列 × 2 個桶約 32 MB。

## 8. 保留期限與總容量上限（A9）

### 8.1 依時間

| 資料 | 設定 | 預設 |
|---|---|---|
| 原始 report | `storage.raw_retention_days` | 7（今天與前 7 天，UTC） |
| 5 分鐘彙總 | `storage.rollup_5m_retention_days` | 31 |
| 1 小時彙總 | `storage.rollup_1h_retention_days` | 400 |

依取樣時間（`reports.ts`、彙總的桶）；與 Pulse 補送的關係見第 5.4 節。0 表示不依時間刪除。啟動後立即執行一次，之後每小時一輪；每一步刪除最多 5000 列（一個交易，每台主機以 `(host_id, ts)` 索引或主鍵範圍刪除），步與步之間讓 report 先寫，不會長時間鎖住資料庫。一輪結束時刪除已沒有任何 report 引用的 `boots` 列，並記一行 `INFO`（刪除的筆數）。

刪除前同樣檢查系統日期是否合理（與 JSONL、日誌共用 `crates/date-guard` 的 DateGuard 規則：時鐘被誤校到未來時暫停刪除 24 小時）；狀態存在資料庫的 `meta.retention_guard`，跨重新啟動延續。

### 8.2 總容量上限

`storage.max_total_bytes`（預設 64 GiB，0 = 不限制，否則至少 64 MiB）。每 10 秒檢查一次資料庫的使用量 =（`page_count` − `freelist_count`）× `page_size`；超過時依序刪除，每步一個交易：

1. 最舊的原始 report（依寫入順序，每步 5000 筆）；
2. 原始 report 已刪光時，5 分鐘彙總最舊的一天；
3. 再來是 1 小時彙總最舊的一天。

刪到上限的 95% 以下才停止（避免在上限附近每幾秒刪一次）。主機清冊、序列、`meta` 與 `api_tokens` 不會被刪除。0.4 起以設計包的代碼 `KRS-W4202`（容量已滿，提早刪除最舊的資料；`copywriting.md` §4.2 範例 68）記錄：

- 開始：`WARN`「警告 KRS-W4202：資料庫已達容量上限，正在提早刪除最舊的資料：已使用 … bytes，超過 storage.max_total_bytes（… bytes）…」；
- 結束：`INFO`「KRS-W4202 結束：SQLite 資料庫已降到 … bytes（上限 … bytes），停止刪除；這一輪刪除原始 report N 筆、5 分鐘彙總 M 列、1 小時彙總 L 列」；
- 沒有東西可刪：`WARN`（節流）「警告 KRS-W4202：…已沒有原始 report 或彙總可刪除…；請調高上限」；
- 開始與結束都寫入 `meta.capacity_event`（JSON：`started_at`、`ended_at`、`used_bytes`、`limit_bytes`、各層刪除的列數、`exhausted`），主控台的系統健康（`/api/v1/system/health` 的 `database`）以它顯示「最近一次容量刪除」，正在刪除或最近 24 小時內刪除過時為警告。

注意：

- 刪除後的空間由 SQLite 重複使用，**檔案不會變小**；檔案大小大約維持在曾經到達的最大值。需要縮小檔案時，停止 Kairos 後以 `sqlite3 kairos.db 'VACUUM'` 整理（需要約與資料庫相同的暫存空間），或以 `kairos db backup` 產生精簡的備份再換回（第 9 節）。
- 使用量**不含 WAL**：WAL 由 checkpoint 控制在數十 MiB 內，且刪除資料本身也會寫入 WAL，若計入會在上限較小時把資料全部刪光。規劃磁碟時請在上限之外預留至少 128 MiB（WAL、`-shm` 與日誌）。
- 啟動時若上限小於第 10 節的估算量（以 `server.max_connections` 台主機估算）記一次 `WARN`。

## 9. 備份與還原

**線上備份**（Kairos 執行中也可以，不會擋住寫入）：

```bash
sudo -u kairos /opt/kairos/kairos db backup /var/backups/kairos/kairos-$(date +%F).db \
    --config /opt/kairos/kairos.toml
```

以 SQLite 的 `VACUUM INTO` 寫出一致的快照（不含空閒頁，通常比原檔小），備份檔已存在時不覆寫，Unix 上權限為 `0640`。有安裝 `sqlite3` 命令列工具時，也可以用 `sqlite3 /opt/kairos/data/kairos.db ".backup '/var/backups/kairos/kairos.db'"`。**不要在 Kairos 執行中直接複製 `kairos.db`**：最新的交易可能還在 `kairos.db-wal` 中，只複製主檔會得到不完整、甚至損毀的副本。

**離線備份**：`systemctl stop kairos` 後複製整個資料目錄（正常關機後 `-wal`、`-shm` 已被刪除，只有 `kairos.db`）。

**還原**：

```bash
systemctl stop kairos
rm -f /opt/kairos/data/kairos.db-wal /opt/kairos/data/kairos.db-shm
install -o kairos -g kairos -m 640 /var/backups/kairos/kairos-2026-10-12.db /opt/kairos/data/kairos.db
systemctl start kairos
```

備份檔是完整的 Kairos 資料庫，可以直接啟動；備份之後收到的 report 不在其中。`agents.json` 不需要備份（啟動時由清冊重建）。

**JSONL 匯出**（給原本讀取 `reports-*.jsonl` 的工具）：`kairos reports export --since 1d [--agent <agent_id>] > reports.jsonl`，每行的格式與 JSONL 檔相同（`schema` 1／2；`peer`、`hostname` 為清冊中最後的值）。未指定 `--agent` 時會掃描整個 `reports` 表。

## 10. 容量估算

以每台主機每 5 秒一筆 report、約 17 個指標（Pulse 0.2.0 的典型值：CPU、記憶體 2、3 個掛載點的容量 6、2 張網卡的流量 4、溫度 4）、約 20 個序列估算（`AppConfig::estimated_sqlite_bytes`）：

```text
每台主機每天：
  原始   = 17280 筆 × 240 bytes            ≈ 4.15 MB
  5 分鐘 = 288 × 20 序列 × 50 bytes          ≈ 0.29 MB
  1 小時 = 24 × 20 序列 × 50 bytes           ≈ 0.024 MB
總量 = 主機數 × [原始 × (raw_retention_days + 1) + 5 分鐘 × (rollup_5m_retention_days + 1)
                 + 1 小時 × (rollup_1h_retention_days + 1)]
```

| 主機數 | 原始 7 天 | 5 分鐘 31 天 | 1 小時 400 天 | 合計（預設保留） |
|---|---|---|---|---|
| 100 | 3.3 GB | 0.9 GB | 1.0 GB | 約 5.2 GB |
| 1,000 | 33 GB | 9.2 GB | 9.6 GB | 約 52 GB |
| 10,000 | 332 GB | 92 GB | 96 GB | 約 520 GB |

- 240 bytes／筆來自第 11 節的量測（含 `reports_host_ts` 索引與主機、序列等；資料表與索引本身約 203 bytes）；指標數不同時大致等比例調整（每個指標約 7～10 bytes）。0.2 的 JSONL 同一筆約 1.5 KB（`schema` 2，17 個指標），約為 SQLite 的 6 倍。
- 預設上限 64 GiB 約可容納 1,300 台主機的預設保留期間。1 萬台時請依磁碟調高 `storage.max_total_bytes`，或縮短保留天數，例如原始 2 天、5 分鐘 14 天、1 小時 180 天：約 124 + 43 + 43 ≈ 210 GB。超過上限時依第 8.2 節提早刪除最舊的資料（原始資料先刪）。
- 主機清冊每台約 1 KB，1 萬台約 10 MB，可忽略。
- 以 `kairos reports import` 匯入的資料每行另有約 23 bytes 的去重鍵（`import_keys`，第 11.3 節），超過所有保留期限時在下一次匯入時刪除。
- **補送**（A5）不增加長期的容量（補的是斷線期間本來就該有的資料），但恢復連線時寫入速率暫時升高：每台 Pulse 補送時約每秒 0.83 筆（`buffer.replay_interval_ms` 預設 1200），1 萬台同時恢復約每秒 8,300 筆（平常約 2,000 筆）。第 11 節量測了每秒 8,000～16,000 筆。原始資料保留天數縮短時，請一併檢查第 5.4 節的 `buffer.max_age_hours`。

## 11. 量測

`kairos/src/store/tests.rs` 的 `sqlite_throughput`（`#[ignore]`）：以實際的 writer 與 SQLite，每 10 ms 送出一批，每筆從放入佇列到收到結果計時。

```bash
cargo test -p kairos --release -- --ignored --nocapture sqlite_throughput
# 可調：KP_BENCH_RATE（筆／秒，預設 2000）、KP_BENCH_SECS（預設 30）、KP_BENCH_HOSTS（預設 10000）、
#       KP_BENCH_SYNC=full、KP_BENCH_DIR（保留資料庫供檢查）
# 補送尖峰：KP_BENCH_DURABLE=1（帶去重鍵、取樣時間從 KP_BENCH_BACKFILL_HOURS=72 小時前起）、
#       KP_BENCH_DUP_PERCENT=2（2% 另外重送一次）、KP_BENCH_INVENTORY=1（量測期間第一次清冊同步 1 萬台）、
#       KP_BENCH_TIMEOUT=1（與連線相同：等 5 秒，未認領的取消 = storage_unavailable，已認領再等 1 秒）
cargo test -p kairos --release -- --ignored --nocapture inventory_sync_timing     # 清冊第一次同步
cargo test -p kairos --release -- --ignored --nocapture sqlite_backfill_profile   # 寫入／維護耗時隨資料量的變化
```

kp-build（PVE VM，4 vCPU、3 GB RAM、本機虛擬磁碟，Ubuntu）2026-10-12 的結果，1 萬台主機輪流、每秒 2000 筆（相當於 1 萬台 × 每 5 秒一筆）、每筆 17 個指標：

| 情境 | 筆數 | p50 | p99 | p99.9 | 最大 | 資料庫 | 每筆 |
|---|---|---|---|---|---|---|---|
| 30 秒（`synchronous = normal`） | 60,000 | 0.55 ms | 1.5 ms | 9.1 ms | 18 ms | 14.2 MB（WAL 結束時 19.7 MB） | 237 bytes |
| 改用背景 checkpoint 之前（參考） | 60,000 | 0.55 ms | 334 ms | 460 ms | 472 ms | 14.1 MB | 235 bytes |
| 7 分鐘（含一次 1 萬台 × 17 序列的 5 分鐘彙總寫出） | 840,000 | 0.57 ms | 1.8 ms | 12 ms | 44 ms | 190 MB（WAL 結束時 21 MB） | 226 bytes |

- 每筆大小：`reports` 資料表與 `reports_host_ts` 索引約 203～206 bytes，其餘為主機、序列與彙總（7 分鐘的量測中 34 萬列彙總約 16.6 MB，每列約 49 bytes，與第 10 節的估算相符）。
- 所有 report 都在 50 ms 內收到結果，遠低於 `timeouts.write_secs`（預設 5 秒），PROTOCOL.md 的 8 秒回應預算不受影響。
- 上表的量測沒有包含主機清冊的同步；清冊與補送尖峰見下面兩小節。

### 11.1 1 萬台同時恢復連線的補送尖峰（A5）

情境：Kairos 停機（或網路中斷）後 1 萬台 Pulse 同時恢復，各自補送積壓。Pulse 0.3.0 預設每 1.2 秒一筆（`buffer.replay_interval_ms = 1200`，約每秒 8,300 筆）；量測時（當時預設 600 ms）以每秒 16,700 筆為上限，因此量了每秒 8,000 與 16,000 筆兩種。量測以 `KP_BENCH_DURABLE=1 KP_BENCH_DUP_PERCENT=2 KP_BENCH_INVENTORY=1`：每筆帶去重鍵、取樣時間為 72 小時前起的歷史資料（彙總的桶都是已結束的舊桶，走第 5.3 節的補送路徑），2% 的筆數重送一次（應為 `Duplicate`），並在第 30 秒做一次 1 萬台的清冊同步。kp-build 同上，2026-10-12：

| 速率 × 時間 | 等待方式 | 結果 | p50 | p99 | 最大 | `storage_unavailable` | 結果不明 |
|---|---|---|---|---|---|---|---|
| 8,000／秒 × 60 秒 | 不逾時 | 48 萬筆全部寫入，重送 9,600 筆皆為 `Duplicate` | 1.4 ms | 43 ms | 94 ms | 0 | 0 |
| 16,000／秒 × 60 秒 | 不逾時 | 96 萬筆全部寫入，重送 19,200 筆皆為 `Duplicate` | 2.8 ms | 103 ms | 143 ms | 0 | 0 |
| 8,000／秒 × 5 分鐘 | 與連線相同 | 240 萬筆全部寫入，重送 48,000 筆皆為 `Duplicate` | 2.1 ms | 124 ms | 226 ms | 0 | 0 |
| 16,000／秒 × 5 分鐘 | 與連線相同 | 480 萬筆中 465 萬筆寫入 | 188 ms | 5.0 s | 5.05 s | 149,923（3.1%） | 0 |
| 16,000／秒 × 5 分鐘（參考） | 不逾時（不取消） | 寫入跟不上後佇列外的等待無限累積 | 167 ms | 12.5 s | 14.1 s | （連線會在 5 秒時回） | — |

- **8 秒回應預算不受影響**：寫入跟不上時，連線等待 `timeouts.write_secs`（5 秒）後取消尚未被認領的要求並回 `storage_unavailable`（確定未保存），所以最長回應約 5.05 秒；已被認領的都在 1 秒寬限內有結果，沒有任何一筆變成「結果不明」。被取消的要求 writer 不會再處理，佇列不會無限累積（上表最後一列是不取消時的對照）。
- **每秒 16,000 筆在 60 秒內跟得上，持續 5 分鐘時跟不上**：kp-build 上持續寫入的能力隨資料庫變大而下降（`sqlite_backfill_profile`：每批 1000 筆的寫入從 9.7 ms 增加到 17 ms、每步維護從 12 ms 增加到 27 ms，資料庫從 0.1 GB 增加到 0.5 GB）。原因是 `reports_host_ts` 與 `reports_dedup` 兩個索引、以及彙總表的主鍵都依主機分散，1 萬台主機輪流寫入時每個交易會碰到上千個分散的頁；補送時彙總的桶寫出量也是即時資料的約 8 倍。加大 SQLite 的 cache（256 MB）沒有改善；提高 writer 補做 checkpoint 的門檻（4096 → 16384 頁）約改善 20%，但最大延遲變大，未採用。
- **實際的行為**：Pulse 收到 `storage_unavailable` 後退避（2 秒加倍到 60 秒）再從最舊的未確認資料重送，整體速率自動降到 Kairos 寫得進去的程度，資料留在 Pulse 的緩衝中、不會遺失，重送由唯一索引去重。上表是固定速率的負載（不退避），比實際更嚴苛。
- **預設值**：依上表，Pulse 的 `buffer.replay_interval_ms` 預設為 **1200**：1 萬台同時恢復約每秒 8,300 筆，接近上表中每秒 8,000 筆持續 5 分鐘、沒有 `storage_unavailable` 的範圍（p99 0.12 秒、最大 0.23 秒）。每台補送約每秒 0.83 筆，扣掉同時產生的新取樣（每 5 秒 1 筆）後積壓以約每秒 0.63 筆減少：斷線 1 小時（720 筆）約 19 分鐘補完，24 小時（17,280 筆）約 7.6 小時補完（600 ms 時分別約 8 分鐘、3.3 小時）。主機更多（例如 2 萬台）時再調大（2000 → 1 萬台約每秒 5,000 筆，24 小時的積壓約 16 小時補完），或使用更快的磁碟與更多 CPU；主機少、希望補得快時可調小到 500（下限，Kairos 的 report 速率限制每秒 2 則）。Pulse 實際使用的間隔不超過 `report.interval` 的 60%（`report.interval = 1` 時為 600 ms），否則新的取樣比送出的快、積壓補不完。

### 11.2 主機清冊的第一次同步

重新啟動後的第一次同步約寫入主機數那麼多列。`inventory_sync_timing`（1 萬台，kp-build，3 次）：

| 方式 | 交易數 | 最長的一個交易 | 合計 |
|---|---|---|---|
| 一個交易（改成分批之前） | 1 | 68～69 ms | 68～69 ms |
| 每個交易最多 500 列（目前） | 20 | 17 ms | 83～84 ms |

分批之後，同步在 report 的批次之間逐步進行（每 100 ms 一步），report 的寫入最多被擋住約 17 ms；上一小節的補送量測都包含第 30 秒的這次同步（例如每秒 8,000 筆 60 秒的最大延遲 94 ms）。

### 11.3 匯入舊的 JSONL（`kairos reports import`）

`kairos/src/store/import_tests.rs` 的 `import_throughput`（`#[ignore]`）：產生 100 萬行的 JSONL（一半為 `schema` 2、17 個指標，Pulse 0.2.0 的典型值；一半為 `schema` 1；每台每 5 秒一筆），依序以試執行、匯入、再匯入一次（全部重複）處理同一個檔案。

```bash
cargo test -p kairos --release -- --ignored --nocapture import_throughput
# 可調：KP_IMPORT_LINES（預設 1000000）、KP_IMPORT_HOSTS（預設 200）、
#       KP_IMPORT_DAYS_AGO（資料的結束時間，預設 1 天前；超過原始資料保留期限時走「只進彙總」）、KP_IMPORT_DIR
```

kp-build（同上，4 vCPU、本機虛擬磁碟）2026-10-13：

| 情境 | JSONL | 試執行 | 匯入 | 再匯入（全部重複） | 資料庫 |
|---|---|---|---|---|---|
| 200 台、原始資料（約 7 小時） | 100 萬行，984 MB | 12.2 秒（約 8.2 萬行／秒） | 24.4～25.9 秒（約 3.9～4.1 萬行／秒） | 14.4～14.9 秒（約 6.9 萬行／秒） | 143 MB（`VACUUM` 後 137 MB，其中 `import_keys` 22 MB，每行 22.5 bytes） |
| 1 萬台、原始資料 | 100 萬行，968 MB | 12.2 秒 | 29.3 秒（約 3.4 萬行／秒） | 14.6 秒 | 145 MB（`import_keys` 23 MB） |
| 1 萬台、只進彙總（10 天前） | 100 萬行，968 MB | 12.6 秒 | 47.4 秒（約 2.1 萬行／秒） | 14.8 秒 | 36 MB（`import_keys` 23 MB） |

- 原始資料的每筆大小與第 10 節相同（約 120 bytes／筆的資料與索引，這裡半數是只有 3 個值的 `schema` 1），另加約 23 bytes 的去重鍵。
- 「只進彙總」比較慢：每一行的每個指標都在同一個交易中直接累加進 5 分鐘與 1 小時彙總（與去重鍵在同一個交易，中斷後重新執行不會重複計算），不像原始資料先在記憶體中合併。
- 估算：0.2 的 1,000 台、7 天的 JSONL 約 1.2 億行，以每秒約 4 萬行約需 50 分鐘。停機時間較長時，可以在升級前、0.2 仍在執行時先匯入前幾天的檔案（第 13.1 節「縮短停機」）。

## 12. 查詢範例

以 `sqlite3`（唯讀：`sqlite3 'file:/opt/kairos/data/kairos.db?mode=ro'`）或任何 SQLite 用戶端查詢。

```sql
-- 某台主機 30 天的 CPU 趨勢（1 小時彙總，走主鍵範圍）
SELECT datetime(r.bucket, 'unixepoch') AS hour, r.vmin, r.vmax, r.vsum / r.cnt AS avg
FROM rollup_1h r
JOIN hosts h USING (host_id) JOIN series s USING (series_id)
WHERE h.agent_id = '6f1c0e4a-1b2c-4d3e-9f00-112233445566'
  AND s.name = 'system.cpu.usage' AND s.labels = ''
  AND r.bucket >= strftime('%s', 'now', '-30 days')
ORDER BY r.bucket;

-- 失聯超過 3 天的主機（等同 kairos hosts list --offline-for 3d）
SELECT agent_id, hostname, datetime(last_seen / 1000, 'unixepoch') AS last_seen
FROM hosts WHERE last_seen < (strftime('%s', 'now', '-3 days') * 1000) ORDER BY last_seen;

-- 某台主機最近一天的原始 report 筆數（走 reports_host_ts 索引）
SELECT count(*) FROM reports
WHERE host_id = (SELECT host_id FROM hosts WHERE agent_id = '6f1c0e4a-1b2c-4d3e-9f00-112233445566')
  AND ts >= (strftime('%s', 'now', '-1 day') * 1000);
```

原始 report 的 `body` 是二進位編碼，請用 `kairos reports export` 取出。`store::sqlite::tests::host_trend_queries_use_the_indexes` 以 `EXPLAIN QUERY PLAN` 確認上述查詢走索引。

## 13. 從 JSONL（0.2）升級

- 升級後預設改寫 SQLite（`data/kairos.db`），**不再寫 `reports-YYYY-MM-DD.jsonl`**。直接讀 JSONL 的工具有三個選擇：設定 `storage.jsonl_mirror = true`（同時寫兩份，磁碟用量約增加 6 倍）、改用 `kairos reports export`、或暫時設定 `storage.backend = "jsonl"`（與 0.2 完全相同，但沒有主機清冊與彙總）。
- 既有的 `reports-*.jsonl` 不會自動匯入資料庫，也不會被修改；需要時以 `kairos reports import` 匯入（第 13.1 節）。`jsonl_mirror = false` 時 JSONL writer 不執行，**舊檔不會再被自動清除**，請在匯入、不需要後自行搬移或刪除（或暫時開啟鏡像，讓 `storage.retention_days` 清掉過期的檔案）。`data/.retention-guard.json` 只給 JSONL 使用。
- 主機清冊從空開始（或由匯入建立），主機在第一次送 report、或下一次清冊同步（30 秒內）時出現。
- 回到 0.2：0.2 不認得 `kairos.db`（忽略），照常寫 JSONL；之後再升級時資料庫中的資料仍在（期間的 report 只在 JSONL 中）。降版到 schema 較舊的 0.3.x 時會被拒絕（第 3.2 節）。
- systemd 的 unit 不需修改：資料庫在原本的資料目錄 `/opt/kairos/data`（`ReadWritePaths` 已包含）。
- **與 Pulse 0.3（durable）**：at-least-once 需要 Kairos 與 Pulse 都是 0.3（任一端是 0.2 時維持 at-most-once，見 CHANGELOG 的相容矩陣）。建議先升級 Kairos（CHANGELOG [0.3.0] 的升級步驟）。補送的資料以 Pulse 的取樣時間保存與彙總（第 5.2 節），從匯出或 JSONL 鏡像讀資料的工具請以 `ts` 作為取樣時間，`recv_at` 只是收到的時間。

### 13.1 匯入舊的 JSONL（`kairos reports import`）

0.2.x 以前寫的 `reports-YYYY-MM-DD.jsonl`（0.1.0 沒有 `schema` 的行、`schema` 1／2 的行、durable 時帶 `boot_id`／`report_seq` 的行；0.3 的 JSONL 鏡像與 `backend = "jsonl"` 的檔案格式相同，也可以）可以匯入資料庫，之後以 `kairos reports export`、彙總與 SQL 查詢使用。

```bash
# 試執行：只讀取，計算會匯入、重複、壞行與超出保留期的筆數（Kairos 執行中也可以）
sudo -u kairos /opt/kairos/kairos reports import /opt/kairos/data --dry-run --config /opt/kairos/kairos.toml
# 匯入：先停止 Kairos
sudo systemctl stop kairos
sudo -u kairos /opt/kairos/kairos reports import /opt/kairos/data --config /opt/kairos/kairos.toml
sudo systemctl start kairos
```

```console
[ OK ] reports-2026-10-03.jsonl：17,280 行，匯入 17,280
[WARN] reports-2026-10-04.jsonl：17,282 行，匯入 17,280、壞行 2
  壞行：第 3 行（不是合法的 JSON）、第 17282 行（最後一行不完整（沒有換行，檔案可能仍在寫入））
[ OK ] 已匯入 2 個檔案到 /opt/kairos/data/kairos.db
  讀取        34,562 行
  匯入        34,560 筆（原始資料 34,560、只進彙總 0）
  重複略過    0
  壞行        2
  超出保留期  0
  新增主機    1 台
[WARN] 警告 KRS-W4306：2 行不是合法的 report，已略過
  處理方式：上方列出各檔案前幾個壞行的行號與原因；確認是否為截斷或手動編輯過的檔案。修正後重新執行，已匯入的部分不會重複。
```

每個檔案的結果寫 stderr，摘要寫 stdout（設計包 `cli-style.md`）；`--json` 時 stdout 只有一份純 ASCII 的 JSON（`schema` 為 `kairos.reports.import/v1`：`data.totals`、`data.files[]`、`data.issues[]`），stderr 保持空白。選項見 `kairos reports import --help`。

- **來源**：參數為一或多個檔案或目錄。目錄只取其中的 `reports-YYYY-MM-DD.jsonl` 與舊版的 `reports.jsonl`，依日期排序，不進入子目錄。Kairos 的保留清除只刪檔、不壓縮，因此不支援壓縮檔：目錄中的 `reports-*.jsonl.gz` 等會被列出、提示先解壓縮（例如 `zcat reports-2026-09-01.jsonl.gz > /tmp/import/reports-2026-09-01.jsonl` 後匯入 `/tmp/import`）。
- **只讀、不跟隨符號連結**：來源檔以唯讀方式開啟，不修改、不刪除。比照 `kairos db backup` 不跟隨預先放置的符號連結：參數本身是符號連結、目錄中的符號連結、FIFO 與裝置檔一律不讀取（列在摘要中）；Unix 上另比對檢查與開啟時的裝置與 inode，中間被換掉時拒絕。
- **時間**：取樣時間與 Kairos 當初寫入 SQLite 時的規則相同（第 5.2 節）：durable 的行（帶 `boot_id`）用 `ts`（須在 `recv_at` 的 −367 天～＋5 分鐘之內），其他用 `recv_at`。`agent_ts`、`recv_lag` 照常保存，`kairos reports export` 輸出的行與原本的行相同（`peer`、`hostname` 為清冊中的值）。`--since`／`--until` 以取樣時間篩選（`2026-09-01`（UTC）、RFC 3339，或 `30d` 這類從現在往前的時間），範圍外的計入「不在範圍」。
- **去重鍵（冪等）**：每一行以「主機＋取樣時間＋內容雜湊」記在 `import_keys`。內容雜湊為 64 位元（FNV-1a 再以 splitmix64 混合，跨版本、跨平台固定），涵蓋 `recv_at`、`seq`、`ts`、`boot_id`／`report_seq` 與每個指標的名稱、單位、label、值，欄位順序與空白不影響。重複執行、中斷後重新執行、同一行出現在兩個檔案（例如複本）都只匯入一次，計入「重複略過」。另外兩種重複也會略過：durable 的行以 Kairos 原本的唯一索引（主機＋`boot_id`＋`report_seq`）判斷，與 Kairos 已保存（或 Pulse 之後補送）的同一筆不會重複；非 durable 的行若資料庫中已有完全相同的原始 report（同一主機、取樣時間、`seq`、格式與內容，例如開啟鏡像之後 JSONL 中的行），也視為重複。因此匯入整個資料目錄（含開啟鏡像之後的檔案）也不會重複。雜湊碰撞（同一主機、同一毫秒的兩筆不同 report 雜湊相同）的機率約為 2⁻⁶⁴，會被當成重複略過。
- **保留期限**：與 Kairos 的保留清除相同的日期計算（今天與前 N 天，UTC，以執行時的系統時鐘），依取樣時間：

  | 取樣時間 | 處理 |
  |---|---|
  | 在 `raw_retention_days` 之內 | 寫入原始資料，併入 5 分鐘與 1 小時彙總 |
  | 超過原始資料保留期限，仍在彙總的保留期限內 | **只併入彙總**（「只進彙總」），不寫入原始資料；超過 `rollup_5m_retention_days` 的只併入 1 小時彙總 |
  | 超過所有彙總的保留期限 | 略過（「超出保留期」） |

  選擇「只進彙總」而不是全部略過或全部寫入：寫入原始資料的舊資料會在下一輪保留清除（每小時）被刪除，白白佔用寫入時間；但它們的長期趨勢（1 小時彙總預設保留 400 天）仍有價值。只進彙總的去重鍵與彙總在同一個交易中寫入，重新匯入時不會重複計算。`raw_retention_days = 0` 等不依時間刪除的設定時一律寫入。
- **彙總**：與 Kairos 處理遲到資料的方式相同（第 5.3 節）。取樣時間早於主機 `rollup_until` 的，在寫入原始資料的同一個交易中累加進已寫出的桶；其他的先在記憶體中合併，每個檔案結束（或記憶體中累積 40 萬列）時寫出並推進 `rollup_until`。開始前先把資料庫中尚未寫出彙總的原始資料（Kairos 停止時還在記憶體中的桶）讀進來一起寫出，因此 Kairos 再啟動時不會重複計算、也不會漏算。匯入中途被中斷時，已提交的原始資料由下一次 Kairos 啟動（或重新匯入）補回彙總。
- **主機**：清冊中沒有的主機以離線狀態新增，`hosts.source = 'import'`（由連線建立的為 NULL），`first_seen`／`last_seen` 為匯入資料中最早／最晚的 `recv_at`，`identity_verified = 0`；`kairos hosts list` 中為「離線」。既有的主機只把 `first_seen` 往前推，`last_seen`、`hostname`、`peer` 只在匯入的資料比清冊中的新時才更新，hello 的主機資訊與線上狀態不變（不覆蓋較新的資訊）。主機數達 `storage.max_hosts`、或指標序列數達上限時，這些行略過並計入「無法匯入」。
- **壞行**：不是合法的 JSON、欄位缺漏或型別錯誤、不支援的 `schema`、`agent_id` 不是 UUID、`boot_id` 與 `report_seq` 只有其中一個、內容不符合協定的限制（與 Kairos 收到 report 時的檢查相同）、超過 1 MiB 的行，以及沒有換行的最後一行（檔案可能仍在寫入，之後重新匯入即可補上）都略過，計入「壞行」，每個檔案列出前 5 個的行號與原因。`hostname`、`peer` 中不能顯示的字元改成 `?`（較舊的 Kairos 限制較寬鬆），不算壞行。空行不計。
- **與服務並行：先停止 Kairos（會偵測）**。匯入以 SQLite 的 `locking_mode = EXCLUSIVE` 開啟資料庫，整個過程持有資料庫檔的獨占鎖。Kairos 執行中時（它的連線一直持有共用鎖）取不到，匯入不開始，以結束代碼 11 結束（`KRS-E4304`「資料庫正在使用中」）；反過來，匯入期間 Kairos 也無法開啟資料庫而啟動失敗（systemd 依 `Restart=on-failure` 每 5 秒重試，匯入結束後就會啟動），`kairos hosts list`、`reports export`、`db backup` 等待約 5 秒後失敗。不採用「與服務同時寫入」的原因：
  1. Kairos 在記憶體中保存尚未寫出的彙總，並以 `rollup_until` 判斷遲到資料；另一個行程同時寫入原始資料、推進 `rollup_until` 時兩邊的狀態對不上，彙總可能重複或漏算。
  2. 服務的寫入只有 `timeouts.write_secs`（5 秒）的預算（PROTOCOL.md 的 8 秒回應預算）：大量匯入的交易會讓服務的 writer 等待鎖，尖峰時回 `storage_unavailable`。
  3. 匯入是升級時的一次性工作，速度約每秒 3～4 萬行（第 11.3 節），停機時間可以先以 `--dry-run` 估算（試執行約為匯入時間的一半）。Pulse 0.3 在停機期間把取樣存在本機緩衝，恢復後補送；0.2 的 Pulse 在停機期間的取樣會遺失，因此停機要短時請用下一點的作法。
- **縮短停機**：0.2 的 Kairos 不使用 `kairos.db`，因此可以在**升級前、0.2 仍在執行時**，以新版的執行檔先匯入已經寫完的舊檔，停機升級時只匯入當天的檔案：

  ```bash
  # 0.2 仍在執行：匯入今天（UTC）以前的資料（資料庫在這時建立；以服務帳號執行）
  sudo -u kairos ~/kairos-dist/pkg-new/kairos reports import /opt/kairos/data \
      --until $(date -u +%F) --config /opt/kairos/kairos.toml
  # 停止 0.2、換上 0.3（INSTALL 第 11 節），匯入當天的檔案後再啟動
  sudo systemctl stop kairos
  sudo -u kairos ~/kairos-dist/pkg-new/kairos reports import /opt/kairos/data/reports-$(date -u +%F).jsonl \
      --config /opt/kairos/kairos.toml
  ```

  兩次匯入之間跨過 UTC 午夜時，停機時改為匯入整個目錄即可：已匯入的行以去重鍵略過（只多花讀取的時間，約每秒 7 萬行）。
- **以服務帳號執行**：匯入會建立或寫入 `kairos.db`（與 `-wal`）。Linux 上以 root 執行、而資料目錄屬於其他帳號時拒絕（結束代碼 10），避免建立 Kairos 之後無法寫入的檔案；請以 `sudo -u kairos` 執行。
- **結束代碼**：0 完成（有壞行時仍為 0，摘要中有 `KRS-W4306` 警告）、1 有來源無法讀取（其餘照常匯入）或匯入失敗、2 用法錯誤、3 設定錯誤、10 權限不足、11 資料庫使用中。每 2000 行一個交易，中途失敗時已提交的部分保留，修正後重新執行同一個命令即可（已匯入的計入「重複略過」）。
- **容量**：總容量上限依寫入順序刪除原始 report（第 8.2 節），匯入的舊資料是最後寫入的，會比較晚被刪；匯入後超過 `storage.max_total_bytes` 時 Kairos 會先刪到較新的即時資料。匯入結束時若已超過上限，摘要中有 `KRS-W4202` 警告：請先調高上限或以 `--since` 縮小範圍。
- 測試：`store::import::tests`（`schema` 1／2／沒有 `schema`、壞行、重複匯入與同一檔案中的重複、Kairos 已保存的 durable 與鏡像中的行、durable 的取樣時間與去重鍵、保留期限、彙總與 Kairos 尚未寫出的彙總和遲到的檔案、既有主機的資訊、時間範圍與主機上限、試執行與實際的計數相同、獨占鎖、內容雜湊）、`import_cli::tests`（參數、目錄的掃描與符號連結、結束代碼）、端對端 `tests/e2e/tests/import.rs`（Kairos 執行中拒絕、停止後匯入、重新啟動後匯出與清冊）。

## 14. 限制與後續

- 單一寫入者：同一個資料目錄只能有一個 Kairos（A2 的 HA 需要外部資料庫）。
- 主控台 API（0.4，`API.md`）以唯讀連線查詢這個 schema；趨勢也可以 SQL 或 `kairos reports export` 取得。
- `kairos reports export` 不做全域時間範圍的索引（未指定 `--agent` 時掃描整個 `reports` 表）。
- 彙總的值以 f64 計算，超過 2^53 的整數會失去精度（原始資料保留原值）。
- 清冊不會自動刪除主機（除役流程屬 F 系列）。
- durable 連線的取樣時間以 Pulse 的時鐘為準（第 5.2 節）；Pulse 時鐘偏差超過 5 分鐘（偏快）或 367 天（偏慢）時改用收到的時間。
- 原始資料保留天數小於 Pulse 的 `buffer.max_age_hours` 時，原始資料被刪除之後的重送會再寫入一次（第 5.4 節）。
- `kairos reports import` 需要停止 Kairos（獨占資料庫，第 13.1 節）；不支援壓縮過的 JSONL，也不讀取標準輸入。
