# Kairos 資料儲存（SQLite）

本文件說明 Kairos 0.3 起的 report 儲存、主機清冊、彙總、保留期限與容量上限（商業審計 A1、A6、A9），以及與 at-least-once（A5，`sprint2/durable`）的去重契約。設定的完整清單見 [README.md](README.md)「設定」。

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
- **後端選擇**（`storage.backend`）：`sqlite`（預設）或 `jsonl`。`jsonl` 是舊版的按日 JSONL writer（`storage.rs`），保留給相容與回復使用，沒有主機清冊、彙總與去重。
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

## 3. Schema（v1）

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
    ts         INTEGER NOT NULL,           -- 伺服器收到的時間（JSONL 的 recv_at）
    seq        INTEGER NOT NULL,           -- 協定的 seq（每條連線從 1 起）
    agent_ts,                              -- client 的 ts：RFC 3339 毫秒 UTC 時存成 INTEGER，否則原樣存 TEXT
    format     INTEGER NOT NULL,           -- 1：協定 v1 的三欄位；2：通用指標
    body       BLOB NOT NULL,              -- 緊湊編碼，見 3.1
    boot_ref   INTEGER,                    -- 去重鍵（durable），沒有時為 NULL
    report_seq INTEGER
);
CREATE INDEX reports_host_ts ON reports (host_id, ts);
CREATE UNIQUE INDEX reports_dedup ON reports (boot_ref, report_seq) WHERE boot_ref IS NOT NULL;

CREATE TABLE rollup_5m (                   -- 5 分鐘彙總（A9）
    host_id INTEGER NOT NULL, bucket INTEGER NOT NULL, series_id INTEGER NOT NULL,
    vmin REAL NOT NULL, vmax REAL NOT NULL, vsum REAL NOT NULL, cnt INTEGER NOT NULL,
    PRIMARY KEY (host_id, bucket, series_id)
) WITHOUT ROWID;
CREATE TABLE rollup_1h ( ...同上... ) WITHOUT ROWID;   -- 1 小時彙總
```

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

## 4. 寫入路徑與批次策略

- **group commit**：writer 每次取出佇列中目前所有的要求（最多 1024 筆）放進一個交易（`BEGIN IMMEDIATE`）。commit 期間新進的要求在佇列中累積，成為下一批；不刻意延遲湊批次，因此低負載時延遲最低、高負載時自動變成大批次。
- **不阻塞 tokio**：每批寫入與每步維護都在 `spawn_blocking` 執行；store 在 async task 與 blocking 執行緒之間移交，同一時間只有一個執行緒使用連線。
- **逾時與取消**：與 JSONL 相同（`WriteTicket`）。連線等待超過 `timeouts.write_secs` 時，writer 尚未認領的要求被取消（保證不寫入，回 `storage_unavailable`）；已認領的再等最多 1 秒，仍沒有結果則不回應並關閉連線。PROTOCOL.md 的「回應延遲最多 8 秒」預算不變。
- **checkpoint**：寫入的連線關閉自動 checkpoint（`wal_autocheckpoint = 0`），改由背景執行緒以另一條連線每 0.5 秒執行 `wal_checkpoint(PASSIVE)`（把 WAL 寫回資料庫並 fsync，不擋住寫入）。持續寫入時背景 checkpoint 追不上最新的頁，WAL 不會從頭重寫，因此 WAL 超過約 4096 頁（16 MiB）時，由寫入的連線在兩批之間補做一次 PASSIVE checkpoint（大部分的頁已寫回並同步，停頓很短），之後的交易從 WAL 開頭重寫；WAL 檔由 `journal_size_limit` 截到 64 MiB 以內。量測：改用背景 checkpoint 前 p99 約 334 ms，之後約 1.5 ms（第 11 節）。
- **快取**：主機（`agent_id` → `host_id`）、序列與每台主機最近的 boot 存在記憶體；交易失敗時整個清空、下一批前重新載入（失敗交易中建立的編號已不存在）。

## 5. 去重（與 `sprint2/durable` 的契約）

at-least-once（A5）時 Pulse 會重送 report。Pulse 與 Kairos 協商到 feature `durable` 時，report 另帶 `boot_id`（字串，UUID，Pulse 每次啟動產生）與 `report_seq`（u64，同一個 `boot_id` 內單調遞增），重送時兩者不變。

**本分支提供的 API**（`kairos/src/storage.rs`）：

```rust
pub struct DedupKey { pub boot_id: String, pub report_seq: u64 }
impl ReportRecord { pub fn with_dedup(self, dedup: Option<DedupKey>) -> Self }
pub enum WriteStatus { Written, Duplicate, Failed(String), Unknown(String) }
```

- 連線端（`connection.rs` 的 `handle_registered`）在建立 `ReportRecord` 後，若連線協商到 `durable` 且 report 帶有兩個欄位，以 `.with_dedup(Some(DedupKey { boot_id, report_seq }))` 附上；沒有時維持 `None`（行為與目前相同）。這段接線由 durable 分支處理，本分支沒有修改協定解析。`store_report` 與 writer 不需再改。
- SQLite 以唯一索引保證同一主機、同一個 `(boot_id, report_seq)` 只保存一次（`boots` 把 `(host_id, boot_id)` 對應到 `boot_ref`，`reports_dedup` 為 `(boot_ref, report_seq)` 的部分唯一索引）。重複的那筆（含同一批中重複）回 `WriteStatus::Duplicate`：連線照常回 `ack`，**不重寫**、不更新 `agents.json` 的 `last_report`（那是較舊的資料）、不加入彙總、不寫入 JSONL 鏡像。
- 不同主機使用相同的 `boot_id` 與 `report_seq` 不衝突；同一主機換了 `boot_id` 後從頭編號也不衝突；舊 boot 的重送在換 boot 後仍能辨識。
- 去重的範圍是**原始資料仍保存的期間**（`storage.raw_retention_days`、容量上限）：原始 report 被刪除後，同一個去重鍵再送來會被當成新的一筆。保留清除會順便刪除已沒有任何 report 引用的 `boots` 列。
- `backend = "jsonl"` 不去重（JSONL 只把 `boot_id`、`report_seq` 寫在 `ts` 之後，讀取端可自行去重）。
- 測試：`store::sqlite::tests::duplicate_dedup_keys_are_acknowledged_but_not_stored_twice`、`store::tests::sqlite_storage_acks_after_commit_mirrors_to_jsonl_and_persists_the_inventory`、`connection::tests::duplicate_report_is_acked_without_updating_last_report`。

## 6. 主機清冊（A6）

- 第一次收到某台主機的 report 時在 `hosts` 新增一列（`agent_id`、`hostname`、`peer`）。之後每 30 秒，writer 把 monitor 最新的狀態表快照（與 `agents.json` 同一份）同步到 `hosts`：上線、離線、hello 的資訊改變時立即寫入；`last_seen` 與 `last_report` 只在 `last_seen` 前進 60 秒以上時寫入（線上的主機每分鐘最多寫一次）。關機時最後一份快照（所有主機已離線）在 writer 結束前寫入。
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
- **怎麼算**：commit 成功後才把值加進記憶體中尚未寫出的 5 分鐘桶。桶結束 15 秒後（等待排隊中的 report）分批寫出（每步最多約 5000 列，只在主機的邊界切開），同一個交易中合併進 1 小時彙總（`ON CONFLICT DO UPDATE` 合併 min／max／sum／count），並把這些主機的 `hosts.rollup_until` 推進到桶的結束時間。
- **重新啟動不漏算、不重複算**：啟動時，各主機從 `rollup_until` 起重新讀取原始資料補回記憶體中的桶；已寫出的桶不會再算一次（`rollup_until` 與寫出在同一個交易）。
- **已知的近似**：時鐘回撥等原因造成 report 的時間落在已寫出的桶時，仍會合併進該桶；若在寫出前 Kairos 當掉，這類遲到的值不會補回（原始資料仍在）。1 小時桶只在其中的 5 分鐘桶寫出時更新。

**記憶體**：每台主機每個序列每個尚未寫出的桶約 80 bytes；1 萬台 × 20 個序列 × 2 個桶約 32 MB。

## 8. 保留期限與總容量上限（A9）

### 8.1 依時間

| 資料 | 設定 | 預設 |
|---|---|---|
| 原始 report | `storage.raw_retention_days` | 7（今天與前 7 天，UTC） |
| 5 分鐘彙總 | `storage.rollup_5m_retention_days` | 31 |
| 1 小時彙總 | `storage.rollup_1h_retention_days` | 400 |

0 表示不依時間刪除。啟動後立即執行一次，之後每小時一輪；每一步刪除最多 5000 列（一個交易，每台主機以 `(host_id, ts)` 索引或主鍵範圍刪除），步與步之間讓 report 先寫，不會長時間鎖住資料庫。一輪結束時刪除已沒有任何 report 引用的 `boots` 列，並記一行 `INFO`（刪除的筆數）。

刪除前同樣檢查系統日期是否合理（與 JSONL、日誌共用 `crates/date-guard` 的 DateGuard 規則：時鐘被誤校到未來時暫停刪除 24 小時）；狀態存在資料庫的 `meta.retention_guard`，跨重新啟動延續。

### 8.2 總容量上限

`storage.max_total_bytes`（預設 64 GiB，0 = 不限制，否則至少 64 MiB）。每 10 秒檢查一次資料庫的使用量 =（`page_count` − `freelist_count`）× `page_size`；超過時依序刪除，每步一個交易：

1. 最舊的原始 report（依寫入順序，每步 5000 筆）；
2. 原始 report 已刪光時，5 分鐘彙總最舊的一天；
3. 再來是 1 小時彙總最舊的一天。

刪到上限的 95% 以下才停止（避免在上限附近每幾秒刪一次），開始與結束各記一行（結束時附刪除的筆數）。主機清冊、序列與 `meta` 不會被刪除；沒有東西可刪時記 `WARN`。

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

## 11. 量測

`kairos/src/store/tests.rs` 的 `sqlite_throughput`（`#[ignore]`）：以實際的 writer 與 SQLite，每 10 ms 送出一批，每筆從放入佇列到收到結果計時。

```bash
cargo test -p kairos --release -- --ignored --nocapture sqlite_throughput
# 可調：KP_BENCH_RATE（筆／秒，預設 2000）、KP_BENCH_SECS（預設 30）、KP_BENCH_HOSTS（預設 10000）、
#       KP_BENCH_SYNC=full、KP_BENCH_DIR（保留資料庫供檢查）
```

kp-build（PVE VM，4 vCPU、3 GB RAM、本機虛擬磁碟，Ubuntu）2026-10-12 的結果，1 萬台主機輪流、每秒 2000 筆（相當於 1 萬台 × 每 5 秒一筆）、每筆 17 個指標：

| 情境 | 筆數 | p50 | p99 | p99.9 | 最大 | 資料庫 | 每筆 |
|---|---|---|---|---|---|---|---|
| 30 秒（`synchronous = normal`） | 60,000 | 0.55 ms | 1.5 ms | 9.1 ms | 18 ms | 14.2 MB（WAL 結束時 19.7 MB） | 237 bytes |
| 改用背景 checkpoint 之前（參考） | 60,000 | 0.55 ms | 334 ms | 460 ms | 472 ms | 14.1 MB | 235 bytes |
| 7 分鐘（含一次 1 萬台 × 17 序列的 5 分鐘彙總寫出） | 840,000 | 0.57 ms | 1.8 ms | 12 ms | 44 ms | 190 MB（WAL 結束時 21 MB） | 226 bytes |

- 每筆大小：`reports` 資料表與 `reports_host_ts` 索引約 203～206 bytes，其餘為主機、序列與彙總（7 分鐘的量測中 34 萬列彙總約 16.6 MB，每列約 49 bytes，與第 10 節的估算相符）。
- 所有 report 都在 50 ms 內收到結果，遠低於 `timeouts.write_secs`（預設 5 秒），PROTOCOL.md 的 8 秒回應預算不受影響。
- 主機清冊的同步沒有包含在量測中（量測直接驅動 writer）：1 萬台主機時，重新啟動後第一次同步約寫入 1 萬列（一個交易），之後只寫有變化的主機（每台每分鐘最多一次）。

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
- 既有的 `reports-*.jsonl` 不會匯入資料庫，也不會被修改。`jsonl_mirror = false` 時 JSONL writer 不執行，**舊檔不會再被自動清除**，請在不需要後自行搬移或刪除（或暫時開啟鏡像，讓 `storage.retention_days` 清掉過期的檔案）。`data/.retention-guard.json` 只給 JSONL 使用。
- 主機清冊從空開始，主機在第一次送 report、或下一次清冊同步（30 秒內）時出現。
- 回到 0.2：0.2 不認得 `kairos.db`（忽略），照常寫 JSONL；之後再升級時資料庫中的資料仍在（期間的 report 只在 JSONL 中）。降版到 schema 較舊的 0.3.x 時會被拒絕（第 3.2 節）。
- systemd 的 unit 不需修改：資料庫在原本的資料目錄 `/opt/kairos/data`（`ReadWritePaths` 已包含）。

## 14. 限制與後續

- 單一寫入者：同一個資料目錄只能有一個 Kairos（A2 的 HA 需要外部資料庫）。
- 沒有查詢 API；趨勢以 SQL 或 `kairos reports export` 取得（F1 的 API 會建立在這個 schema 上）。
- `kairos reports export` 不做全域時間範圍的索引（未指定 `--agent` 時掃描整個 `reports` 表）。
- 彙總的值以 f64 計算，超過 2^53 的整數會失去精度（原始資料保留原值）。
- 清冊不會自動刪除主機（除役流程屬 F 系列）。
