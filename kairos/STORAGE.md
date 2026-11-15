# Kairos 資料儲存（SQLite）

> 0.10.0〔草稿〕起另有 **PostgreSQL 後端**（`storage.backend = "postgres"`）：功能與本文件描述的 SQLite 後端相同，設定、schema 的對應、並行控制、搬移（`kairos db migrate`）與兩者的差異集中在第 19 節。除了第 19 節，本文件描述的都是預設的 SQLite 後端。

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
| `data/alerts-secrets.key`、`data/console.key`、`data/audit-hmac.key`、`data/privacy.key` | 金鑰檔（各 32 bytes，0600）：通知管道機密、CSRF／TOTP、稽核鏈的 HMAC（0.7）、主機名稱等的假名化（0.9，只在 `privacy.hostname`／`process_user = "pseudonymize"` 時建立；PRIVACY.md 第 3 節）。不在資料庫中，資料庫備份不含；備份與還原見第 9 節 |

資料目錄所在的檔案系統必須支援共享記憶體與 POSIX 檔案鎖（本機磁碟；**不要放在 NFS／SMB 等網路磁碟**）。無法啟用 WAL 時 Kairos 拒絕啟動。

Kairos 執行中把 `kairos.db` 刪除或替換時（Unix 上以裝置與 inode 比對，與 JSONL 的 `needs_reopen` 相同），下一批寫入前記 `WARN` 並建立新的資料庫，不會寫進已刪除的檔案後回 `ack`；記憶體中尚未寫出的彙總一併捨棄。

## 3. Schema（v5）

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

0.10.0 起 `meta` 另有 `remote.rollout_approval_observed`（JSON；兩個後端相同，**不需要 schema 變更**，schema 仍是 v11）：遠端管理服務上一次啟動時觀察到的 `[remote] rollout_approval`——`value`、`source`（`file`／`environment`／`default`）、`observed_at`、`node`、`changed_at`、`changed_from`、`disabled_at`。啟動時與目前的設定比對，改變時在同一個寫入交易中寫稽核 `settings.rollout_approval.*` 並更新這個鍵（REMOTE.md 第 5.4 節、`remote/policy.rs`）；隨 `kairos db backup` 與 `kairos db migrate` 一起搬移。升級推送的送審摘要（`submission`、`digest`）不落地，每次由 `rollouts` 與 `rollout_hosts` 計算。

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
- v4（0.5.0，衝刺 4）：註冊通道與 PKI 的資料表 `pki_cas`、`enroll_tokens`、`enroll_requests`、`pki_certificates`、`pki_crls`、`enroll_replay`、`enroll_blocks`、`enroll_relaxations`、`settings`、`audit_events`，以及 `hosts` 的 `lifecycle`、`decommissioned_at`、`decommissioned_by`、`current_serial`、`grace_override_until`、`expired_seen_at`、`last_grace_attempt` 與索引 `hosts_hostname`。完整的 SQL 與保存期限見 [`PKI.md`](PKI.md) 第 7、13 節（`store/sqlite.rs` 的 `SCHEMA_V4`）。升級到 v4 之後 0.4 的 Kairos 無法開啟；`kairos enroll-token`、`kairos enroll`、`kairos pki`、`kairos config` 開啟資料庫時同樣會升級 schema。
- v5（0.6.0，衝刺 5）：告警引擎的資料表 `alert_rules`（規則）、`alert_events`（事件，含彙總的 `parent_id`、處理狀態與通知進度 `notify_state`）、`alert_timeline`（處理紀錄）、`alert_silences`（靜音與維護時段）、`alert_channels`（通知管道；`secrets` 為 AES-256-GCM 密文）、`alert_deliveries`（發送紀錄）、`alert_actions`（CLI 交給執行中的 Kairos 處理的動作）與索引；`meta` 另有 `alerts_config_rev`（規則、靜音、管道的版本）與 `alerts_heartbeat`（停機期間的判斷）。完整的 SQL 見 `store/sqlite.rs` 的 `SCHEMA_V5`，欄位說明見 [`ALERTS.md`](ALERTS.md)。升級到 v5 之後 0.5 的 Kairos 無法開啟；`kairos alerts`、`kairos silences`、`kairos alert-rules`、`kairos alert-channels` 開啟資料庫時同樣會升級 schema。
- v6（0.6.0，衝刺 5）：使用者、角色指派、工作階段與 TOTP 復原碼（`users`、`role_assignments`、`sessions`、`user_recovery_codes`），`api_tokens` 的 `owner_user_id`、`role`，`audit_events` 的雜湊鏈欄位 `prev_hash`、`hash`（索引 `audit_events_actor`），`hosts.host_group`（角色指派的範圍）。v6 接在 v5（告警）之後，不依賴 v5 的物件；0.5.0（v4）的資料庫在第一次啟動時依序升級 v5 → v6（各自一個交易），既有的 API token 權限不變（凍結在 0.5，沒有告警與使用者等新權限）。完整的說明見 [`API.md`](API.md) 第 12.10 節。升級後 0.5 的 Kairos 無法開啟；`kairos users`、`kairos audit` 開啟資料庫時同樣會升級 schema。密碼只存 Argon2id 雜湊、工作階段只存 SHA-256、TOTP 秘密以 `data/console.key` 加密（金鑰不在資料庫中，`kairos db backup` 不含它）。
- v7（0.7.0，衝刺 6）：`audit_events.hmac`（稽核紀錄的 HMAC，[`API.md`](API.md) 第 12.8 節）。既有的紀錄為 NULL（只有雜湊鏈）；服務以 0.7 啟動、建立 `data/audit-hmac.key` 之後寫入的第一筆起才有 HMAC，邊界記在 `meta.audit_hmac`（`since_id`、`key_id`），`meta.audit_chain_head` 另帶 `mac`。升級後 0.6 的 Kairos 無法開啟（降版從備份還原）；任何開啟資料庫的 CLI 同樣會升級 schema。
- v8（0.8.0，衝刺 7，審計 F4）：指標擴充（第 15 節）。`series` 加上 `derived` 並把唯一鍵改為 (name, unit, labels, derived)（SQLite 無法修改唯一鍵，遷移時重建資料表，`series_id` 不變）、索引 `series_name`；`host_series`（由既有的彙總補上）；`hosts.boot_time`、`hosts.series_limited_at`；`host_inventory`、`inventory_changes`、`process_snapshots`。升級後 0.7 的 Kairos 無法開啟（降版從備份還原）；任何開啟資料庫的 CLI 同樣會升級 schema。
- v9（0.9.0，衝刺 9，商業審計 F8）：集中設定與 Pulse 遠端升級（第 18 節）。`config_profiles`、`config_profile_revisions`、`config_assignments`、`host_config`、`releases`、`release_artifacts`、`rollouts`、`rollout_hosts` 與索引；`hosts.install_method`、`install_upgrade`、`install_config`；`meta.config_rev`。SQL 在 `store/sqlite/remote.rs` 的 `SCHEMA_V9`（新的資料表依 L1a 的做法放在領域的檔案中，`MIGRATIONS` 仍在 `store/sqlite.rs`）。升級後 0.8 的 Kairos 無法開啟（降版從備份還原）；任何開啟資料庫的 CLI 同樣會升級 schema。
- v10（0.10.0，衝刺 10，HA L2）：叢集協調的資料表（第 20 節）。`cluster_nodes`、`cluster_leases`、`cluster_revs`、`host_presence`、`cluster_events`、`alert_event_seq`、`login_challenges`、`enroll_nonces`、`auth_failures` 與索引；`enroll_requests.issuing_node`、`issuing_at`；`alert_deliveries.claim_epoch`。SQL 在 `store/sqlite/cluster.rs` 的 `SCHEMA_V10`。叢集模式只支援 PostgreSQL，SQLite 上這些資料表保持空白（兩個後端的資料表一致，`kairos db migrate` 才能一對一複製）。0.10.0 沒有停在 v10 的發佈版本：與 v11 一起套用。
- v11（0.10.0，衝刺 10，商業審計 F8）：升級推送的雙人覆核（REMOTE.md 第 5.4 節）。`rollouts` 新增 `submitted_at`、`submitted_by`、`reject_reason`（`approved_at`／`approved_by` v9 就有，0.10.0 起使用）。只新增三個可為 NULL 的欄位；SQLite 的 SQL 在 `store/sqlite/remote.rs` 的 `SCHEMA_V11_ROLLOUT_APPROVAL`，PostgreSQL 在 `store/postgres/schema.rs` 的 `SCHEMA_V11`（`UPGRADES` 的第二步；全新的 schema 建立 v9 後依序套用 v10、v11）。0.9 的資料庫（v9）第一次以 0.10.0 啟動時在一個交易中依序套用 v10、v11；升級後 0.9 的 Kairos 無法開啟（降版從備份還原）。
- v12（0.11.0 起）：hello 的 `install.external_url`（REMOTE.md 第 2.1 節）。`hosts` 新增 `install_external_url`（TEXT，可為 NULL：`none`／`allowlist`；NULL＝Pulse 沒有宣告）。只新增一個可為 NULL 的欄位，不修改既有的資料；SQLite 的 SQL 在 `store/sqlite/remote.rs` 的 `SCHEMA_V12_INSTALL_EXTERNAL_URL`，PostgreSQL 在 `store/postgres/schema.rs` 的 `SCHEMA_V12`（`UPGRADES` 的第三步）。與其他 `install_*` 欄位不同，清冊同步時**不以 `coalesce` 保留舊值**：hello 帶 `install.upgrade` 時整個取代（沒有宣告就寫 NULL——Pulse 降回舊版後是「未知」），沒有主機資訊的同步才保留。v11 的資料庫第一次以新版啟動時自動升級；升級後 0.10.1 以前的 Kairos 無法開啟（降版從備份還原），叢集的升級步驟照「schema 改變的版本」（INSTALL.md）。
- v13（0.12.0 起）：hello 的 `install.rollback`（REMOTE.md 第 2.1 節；ADR-0003 階段 2）。`hosts` 新增 `install_rollback`（TEXT，可為 NULL：`archived`／`binary_only`／`none`／`degraded`；NULL＝Pulse 沒有宣告）。只新增一個可為 NULL 的欄位，不修改既有的資料；SQLite 的 SQL 在 `store/sqlite/remote.rs` 的 `SCHEMA_V13_INSTALL_ROLLBACK`，PostgreSQL 在 `store/postgres/schema.rs` 的 `SCHEMA_V13`（`UPGRADES` 的第四步）。寫入規則與 `install_external_url` 相同：hello 帶 `install.upgrade` 時整個取代（沒有宣告就寫 NULL），沒有主機資訊的同步才保留。v12 的資料庫第一次以新版啟動時自動升級；升級後 0.11.0 以前的 Kairos 無法開啟（降版從備份還原），叢集的升級步驟照「schema 改變的版本」（INSTALL.md）。
- 資料庫中**沒有任何私鑰**：簽發 CA 的私鑰在 `data/pki/`（0600，目錄 0700），`kairos db backup` 不含它，需另外（加密）備份；註冊 token 與 `poll_secret` 只存 SHA-256。

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
- **上限**：`storage.max_hosts`（預設 `max(100000, limits.max_agents)`）。清冊已滿時新主機的 report 回 `storage_unavailable`，同步時不新增（記 `WARN`）。清冊不會自動刪除主機；0.9 起以「刪除主機資料」刪除一台主機的所有資料（第 16 節）。
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
- **哪些值**：每筆 report 的每個指標（數值以 f64 計）。v1 的三個欄位對應到標準序列：`cpu_usage` → `system.cpu.usage`（`%`），`total_memory`／`used_memory`（MB）× 1048576 → `system.memory.total`／`used`（`By`），與 v2 的指標合併成同一條趨勢。**計數器**（網路收發、磁碟 I/O 等，METRICS.md 第 3 節）：v8 起只彙總 Kairos 算出的每秒速率序列（第 15.1 節），累計值本身不彙總；v7 以前寫入的累計值彙總保留到保留期限自然刪除。缺少使用率的掛載點另彙總推算的使用率（第 15.2 節）。
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
| 行程快照（`process_snapshots`，0.8.0） | 同原始 report | 7 |
| 資產變更歷史（`inventory_changes`，0.8.0） | 同 1 小時彙總（另有每台主機 1,000 筆的上限） | 400 |

依取樣時間（`reports.ts`、彙總的桶）；與 Pulse 補送的關係見第 5.4 節。0 表示不依時間刪除。啟動後立即執行一次，之後每小時一輪；每一步刪除最多 5000 列（一個交易，每台主機以 `(host_id, ts)` 索引或主鍵範圍刪除），步與步之間讓 report 先寫，不會長時間鎖住資料庫。一輪結束時刪除已沒有任何 report 引用的 `boots` 列，並記一行 `INFO`（刪除的筆數）。

刪除前同樣檢查系統日期是否合理（與 JSONL、日誌共用 `crates/date-guard` 的 DateGuard 規則：時鐘被誤校到未來時暫停刪除 24 小時）；狀態存在資料庫的 `meta.retention_guard`，跨重新啟動延續。

### 8.2 總容量上限

`storage.max_total_bytes`（預設 64 GiB，0 = 不限制，否則至少 64 MiB）。每 10 秒檢查一次資料庫的使用量 =（`page_count` − `freelist_count`）× `page_size`；超過時依序刪除，每步一個交易：

1. 最舊的原始 report（依寫入順序，每步 5000 筆）；同一步刪除取樣時間早於剩下最舊一筆原始 report 的行程快照（與原始資料同一層，0.8.0）；
2. 原始 report 已刪光時，5 分鐘彙總最舊的一天；
3. 再來是 1 小時彙總最舊的一天。

刪到上限的 95% 以下才停止（避免在上限附近每幾秒刪一次）。主機清冊、序列、`meta` 與 `api_tokens` 不會被刪除。0.4 起以設計包的代碼 `KRS-W4202`（容量已滿，提早刪除最舊的資料；`copywriting.md` §4.2 範例 68）記錄：

- 開始：`WARN`「警告 KRS-W4202：資料庫已達容量上限，正在提早刪除最舊的資料：已使用 … bytes，超過 storage.max_total_bytes（… bytes）…」；
- 結束：`INFO`「KRS-W4202 結束：SQLite 資料庫已降到 … bytes（上限 … bytes），停止刪除；這一輪刪除原始 report N 筆、5 分鐘彙總 M 列、1 小時彙總 L 列」；
- 沒有東西可刪：`WARN`（節流）「警告 KRS-W4202：…已沒有原始 report 或彙總可刪除…；請調高上限」；
- 開始與結束都寫入 `meta.capacity_event`（JSON：`started_at`、`ended_at`、`used_bytes`、`limit_bytes`、各層刪除的列數、`exhausted`），主控台的系統健康（`/api/v1/system/health` 的 `database`）以它顯示「最近一次容量刪除」，正在刪除或最近 24 小時內刪除過時為警告。

### 8.3 告警歷史

告警的資料表不受上面的容量刪除影響（它們的大小另有上限），由告警引擎每小時清除一次，每步最多 2,000 個事件：

| 資料 | 設定 | 預設 |
|---|---|---|
| 已解決的事件（與其處理紀錄、發送紀錄） | `alerts.retention_days` | 400 天（依結案時間） |
| 事件總數上限（超過時刪除最舊的已解決事件） | `alerts.max_events` | 100,000 |
| 沒有事件的發送紀錄（測試通知、靜音結束的彙整等） | — | 30 天（不超過 `alerts.retention_days`） |
| 已結束的靜音、處理過的 CLI 動作 | `alerts.retention_days`、7 天 | |

未解決的事件不會被刪除。估算：一個事件約 1～2 KB（含處理紀錄與發送紀錄），10 萬個事件約 100～200 MB；請在 `storage.max_total_bytes` 之外預留這個空間（容量刪除只刪 report 與彙總）。

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

**金鑰檔**：`data/alerts-secrets.key`（0.6.0 起；通知管道機密的 AES-256-GCM 金鑰）、`data/console.key`（0.6.0 起；CSRF 與 TOTP 秘密的加密金鑰）、`data/audit-hmac.key`（0.7 起；稽核鏈的 HMAC 金鑰）、`data/privacy.key`（0.9 起；假名化的 HMAC 金鑰，遺失後新收到的資料改用新的假名）**不在資料庫備份中**，資料庫只有密文與 HMAC。還原時金鑰與 `kairos.db` 要是同一次備份的組合。

**金鑰檔的加密備份**（0.7；預設不含，需要另外指定 `--include-keys`）：

```bash
sudo -u kairos /opt/kairos/kairos db backup /var/backups/kairos/kairos-$(date +%F).db \
    --include-keys --password-stdin --config /opt/kairos/kairos.toml < /root/kairos-backup.pass
# kairos-YYYY-MM-DD.db.keys（0600）
```

- 格式 `kairos.keys-backup/v1`（一份 JSON）：Argon2id（m=64 MiB、t=3、p=1、16 bytes 隨機鹽）由備份密碼衍生 32 bytes 金鑰，AES-256-GCM（12 bytes 隨機 nonce）加密；`format`、`kdf`（含鹽）、`cipher` 是附加資料（AAD），被修改即無法解開。只用既有的依賴（`argon2`、`ring`），沒有引入 age 等新 crate。
- 密碼只從 `--password-file <檔案>`（第一行）或 `--password-stdin` 讀取，至少 12 個字元；不接受命令列上的密碼。密碼、衍生的金鑰與解開後的金鑰在記憶體中用完即清除（`zeroize`），不寫入暫存檔（加密後才寫出）。
- 先寫 `.keys` 再備份資料庫；任何一步失敗時不留下另一個檔案。資料目錄中不存在的金鑰檔略過並在輸出中列出。

**還原金鑰檔**（Kairos 停止時）：

```bash
systemctl stop kairos
install -o kairos -g kairos -m 640 /var/backups/kairos/kairos-2026-10-12.db /opt/kairos/data/kairos.db
rm -f /opt/kairos/data/kairos.db-wal /opt/kairos/data/kairos.db-shm
sudo -u kairos /opt/kairos/kairos db restore-keys /var/backups/kairos/kairos-2026-10-12.db.keys \
    --password-stdin --config /opt/kairos/kairos.toml < /root/kairos-backup.pass
systemctl start kairos
```

- 寫回 `storage.data_dir`：先寫同目錄的暫存檔（0600）再改名；以 root 執行時擁有者改為資料目錄的擁有者。先檢查全部再寫入，不會只還原一半。
- 與既有檔案相同時不變；不同時拒絕（資料庫可能是用既有的金鑰加密的），確認後加 `--overwrite`，舊檔改名為 `<名稱>.replaced-<時間>` 保留（之後自行刪除）。
- 密碼錯誤與檔案被修改無法區分，都以`KRS-E4307` 拒絕，不寫入任何檔案。
- 不使用 `--include-keys` 的部署仍可自行加密保存三個檔案（`tar -C /opt/kairos/data -cf - alerts-secrets.key console.key audit-hmac.key | gpg -c > keys-$(date +%F).tar.gpg`），還原後 `chown kairos:kairos`、`chmod 600`。

少了 `alerts-secrets.key` 時 Kairos 會建立新的金鑰，既有管道的機密無法解開：管道的狀態為 `secrets_unavailable`（`KRS-E5110`；不發送，開「通知管道失敗」事件），還原舊金鑰（`--overwrite`）並重新啟動，或重新輸入機密。少了 `console.key` 時同樣建立新的金鑰，已綁定的 TOTP 需重設（`kairos users reset-mfa`）。少了 `audit-hmac.key` 時 Kairos 建立新的金鑰，但不會以它接續稽核鏈：`kairos audit verify` 與自我驗證以 `hmac_key_unavailable`／`hmac_key_mismatch`（`KRS-E6201`）失敗，直到還原原本的金鑰。

**PKI 的檔案**（0.5.0 起，線上簽發時）：`data/pki/issuing-<ca_id>.key` 是簽發 CA 的私鑰，**不在資料庫備份中**；遺失時舊簽發 CA 簽發的憑證仍有效，但無法再為它產生 CRL（處置見 `PKI.md` 第 4 節「私鑰遺失」）。以加密的方式另外備份 `data/pki/`（例如 `tar -C /opt/kairos/data -cf - pki | gpg -c > pki-$(date +%F).tar.gpg`），還原時保持擁有者為 `kairos`、權限 0700／0600；`issuing-cas.pem` 與 `crl/` 由 Kairos 依資料庫重新寫出，不需備份。

**JSONL 匯出**（給原本讀取 `reports-*.jsonl` 的工具）：`kairos reports export --since 1d [--agent <agent_id>] > reports.jsonl`，每行的格式與 JSONL 檔相同（`schema` 1／2；`peer`、`hostname` 為清冊中最後的值）。未指定 `--agent` 時會掃描整個 `reports` 表。

## 10. 容量估算

> **0.8.0 起**（METRICS.md 第 9.6 節；D1）：Pulse 0.8.0 在一般伺服器（2 個掛載點、2 個磁碟 I/O 裝置、2 個介面）約 41 個指標，每筆約 530 bytes、彙總約 47 個序列（計數器只彙總速率），另有行程快照每台每天約 1.4 MB：
>
> ```text
> 每台主機每天：原始 17280 × 530 ≈ 9.2 MB；行程快照 1440 × 1 KB ≈ 1.4 MB；
>               5 分鐘 288 × 47 × 50 ≈ 0.68 MB；1 小時 24 × 47 × 50 ≈ 0.056 MB
> 預設保留期限下每台 ≈ 9.2 × 8 + 1.4 × 8 + 0.68 × 32 + 0.056 × 401 ≈ 129 MB
> ```
>
> 預設上限 64 GiB 約可完整保留 532 台（0.7.0 約 1,320 台）；`server.max_connections` 預設 1024 時啟動會記錄容量不足的警告（`AppConfig::total_limit_below_estimate`）。D1 的建議是不改預設的上限，由管理者依規模調整。下面的舊估算是 0.2.0～0.7.0 的 Pulse（17 個指標）。

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

`kairos/src/store/tests.rs` 的 `store_throughput`（`#[ignore]`；0.9 以前名為 `sqlite_throughput`）：以實際的 writer 與 SQLite，每 10 ms 送出一批，每筆從放入佇列到收到結果計時。

```bash
cargo test -p kairos --release -- --ignored --nocapture store_throughput
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

## 15. 指標擴充（schema v8；0.8.0）

規格：[`crates/kairos-proto/METRICS.md`](../crates/kairos-proto/METRICS.md) 第 9 節。實作：`store/sqlite.rs`（寫入與補回）、`store/derive.rs`（速率與推算的純計算）、`store/assets.rs`（資產合併與變更歷史）、`store/sqlite_assets.rs`。

### 15.1 計數器的速率序列

- 標準計數器（名稱與單位都相符）寫入時，以 `kairos_proto::counter_rate` 與同一台主機、同一序列的上一筆相減，另存為衍生序列：`name`、`labels` 相同，`unit` 為 `<原單位>/s`，`derived = 1`。速率點的時間為後一筆的取樣時間，**只進彙總**（原始資料只有累計值；主控台 API 查原始解析度時以相鄰兩筆即時計算）。
- 重置（值變小）、`system.uptime` 變小（重新開機過）、時間沒有前進時沒有速率點（缺口，不補 0）。晚到的資料（取樣時間不晚於上一筆，例如 Pulse 重新啟動、舊的 `boot_id` 還有積壓）不產生速率點，也不更新上一筆。重複的 durable report（去重）不影響上一筆。
- 上一筆放在記憶體（每台主機、每個計數器序列）。Kairos 重新啟動後：補回彙總時（第 7 節）從 `rollup_until` 之前最近的一筆開始依時間重算；之後第一筆 report 以原始資料中時間最近且早於它的一筆作為上一筆（每台主機只查一次）。交易失敗時與其他快取一起清除、重新載入。
- `kairos reports import`（0.2 的 JSONL）不計算速率（匯入的累計值照舊彙總；原始解析度的查詢仍可算出速率）。

### 15.2 推算的使用率

同一筆 report 中某個掛載點（同一組 label）有 `system.filesystem.total` 與 `available`、但沒有 `system.filesystem.usage` 時（Pulse 0.2.0～0.7.0），以 `(total − available) ÷ total × 100`（小數 2 位、夾在 0～100）存成 `system.filesystem.usage`、`%`、`derived = 2` 的序列（只進彙總）。保留給 root 的空間算成已使用，比 `df` 略高；主控台標示「推算」。

### 15.3 每台主機的序列數上限

`host_series` 記錄每台主機用過的序列（含衍生序列；主控台 API 以它只列出這台主機的序列）。數量達到 `storage.max_series_per_host`（預設 2048，64～65,536）時，新序列的指標**不保存**，同一筆的其他指標照存並回 `ack`，記 `WARN`「警告 KRS-W4104：主機 … 的指標序列數已達上限 …」（每台主機每小時最多一次），並記下 `hosts.series_limited_at`（主控台 S05 的提示）。原因多半是介面或掛載點名稱不斷變化（容器網路）；全域上限 1,000,000（`SERIES_FULL`）不變。`host_series` 不會自動清除（之後的版本再考慮依彙總的保留期限清除）。

### 15.4 資產與變更歷史

- `host_inventory`（每台主機一列）：合併後的資產（`body`，JSON，不含 `boot_time`）、摘要 `digest`（`kairos_proto::inventory_digest`）、`collected_at`（`inventory` 的 `ts`；無法解析或與收到時間相差太多時為收到時間）、`received_at`、`changed_at`（摘要最後一次改變），以及主機清單篩選用的冗餘欄位。`hosts.boot_time`（Unix 秒）隨每則 `inventory` 更新，不算資產變更。
- 合併：有出現的 `os`、`cpu`、`memory`、`system` 逐欄覆蓋，省略的欄位保留舊值；`disks`、`network_interfaces` 有出現時整個取代。
- 變更歷史（`inventory_changes`）：摘要改變時寫入逐欄的差異（第一次收到不寫）。**IPv6 暫時位址**：介面位址的比較中，IPv6 的非 link-local 位址只比較所在的 /64 前綴，同一個前綴內的隱私位址輪換不寫入（Pulse 仍會因摘要改變而重送，目前的資產保存實際的位址）。每台主機最多 1,000 筆（超過時刪最舊的），保留期限見第 8.1 節。
- `inventory` 在 writer 上與 report 依序寫入（一則一個交易），在第一筆 report 之前到達時同樣建立主機的清冊列。

### 15.5 行程快照

`process_snapshots`（主鍵 host_id＋ts，WITHOUT ROWID；索引 `process_snapshots_ts`）：每筆帶 `processes` 的 report 一列（`ProcessList` 的 JSON），與 report 同一個交易；去重判定為重複的 report 不寫。保留期限與容量刪除與原始資料同一層（第 8 節）。主控台 API 由資料庫讀取最新一筆（規格提到的「registry 中的最新一筆」沒有實作：查詢走主鍵、成本很低）。

### 15.6 JSONL 後端

`storage.backend = "jsonl"` 不宣告 `inventory`、`processes`；新的標準指標照常寫入 `schema` 2 的行。JSONL 鏡像（`storage.jsonl_mirror`）也不輸出 `processes` 與資產（`ReportRecord::processes` 不序列化）。

## 14. 限制與後續

- 單一寫入者：同一個資料目錄只能有一個 Kairos（A2 的 HA 需要外部資料庫：0.10.0〔草稿〕的 PostgreSQL 後端與叢集模式——主動／備援——見第 19、20 節；規格見 [`HA.md`](HA.md)，多主動連線層尚未實作）。
- 主控台 API（0.4，`API.md`）以唯讀連線查詢這個 schema；趨勢也可以 SQL 或 `kairos reports export` 取得。
- `kairos reports export` 不做全域時間範圍的索引（未指定 `--agent` 時掃描整個 `reports` 表）。
- 彙總的值以 f64 計算，超過 2^53 的整數會失去精度（原始資料保留原值）。
- 清冊不會自動刪除主機（除役流程屬 F 系列）；刪除一台主機的資料見第 16 節。
- durable 連線的取樣時間以 Pulse 的時鐘為準（第 5.2 節）；Pulse 時鐘偏差超過 5 分鐘（偏快）或 367 天（偏慢）時改用收到的時間。
- 原始資料保留天數小於 Pulse 的 `buffer.max_age_hours` 時，原始資料被刪除之後的重送會再寫入一次（第 5.4 節）。
- `kairos reports import` 需要停止 Kairos（獨占資料庫，第 13.1 節）；不支援壓縮過的 JSONL，也不讀取標準輸入。

## 16. 刪除主機資料（0.9.0〔草稿〕；商業審計 S6）

規格與取捨見 [`PRIVACY.md`](PRIVACY.md) 第 4 節；實作 `store/purge.rs`（計數、逐步刪除）、`store/sqlite_purge.rs`（writer 的快取）。不需要 schema 變更（0.9.0 的 v9 另有集中設定與推送的紀錄，最後一步一併刪除，見第 18 節）。

- **由 writer 執行**：主控台 API 把刪除交給 report 的 writer（與資產共用的佇列，`store::PurgeSink`），writer 在 report 的批次之間每一輪做一步（每一步最多 5,000 列、一個 `BEGIN IMMEDIATE` 交易）。順序：原始 report → 行程快照 → 5 分鐘彙總 → 1 小時彙總 → 資產變更歷史；最後一步在同一個交易中刪除資產、`host_series`、`boots`、`import_keys`、告警事件（含處理紀錄、發送紀錄、CLI 動作）、註冊申請、`enroll_replay`、憑證紀錄、`hosts`，並寫入稽核 `host.purge`。中斷時清冊仍在，重新執行即可接續。
- **快取**：第一步清除這台主機在 writer 中的快取（`agent_id` → `host_id`、序列、計數器的上一筆、boot）與記憶體中尚未寫出的彙總；刪除開始後 10 分鐘內這台主機的 report、資產與清冊同步一律不寫入（`WriteStatus::Failed`，連線回 `storage_unavailable`），避免狀態表的快照或在途的資料把清冊建立回來。
- **憑證**：已撤銷、尚未過期的憑證紀錄保留（CRL 需要），尚未撤銷的有效憑證也保留（呼叫端先撤銷）；清冊中已沒有的主機的憑證在過期 1 天後由註冊服務每小時的清除刪除（`purge::delete_orphan_certificates`）。
- **`kairos hosts purge`**：以 `locking_mode = EXCLUSIVE` 開啟資料庫（與第 13.1 節的匯入相同），Kairos 執行中時 `KRS-E4304`（結束代碼 11）；`--dry-run` 以唯讀連線計數。
- **不涵蓋**：JSONL 鏡像（第 1 節）、備份（第 9 節）、日誌。
- 測試：`store::purge::tests`（兩台主機只刪一台、計數與試執行相同、憑證的保留、稽核與完整性驗證、刪除後的寫入被拒、少量額度的多步刪除）、`api::privacy_tests`、`privacy_cli::tests`、端對端 `tests/e2e/tests/privacy.rs`。

## 17. 儲存層的結構（0.9.0〔草稿〕；HA L1a）

0.9.0 把「各模組直接用 rusqlite」改為領域 store 介面（[`HA.md`](HA.md) 第 5 節的 L1a），為之後的 PostgreSQL 後端（L1b，0.10.0）準備。**純重構**：schema（L1a 本身沒有改變 schema；之後 F8 的 v9 依同樣的結構加入 `RemoteStore`，第 18 節）、SQL、交易邊界、錯誤訊息與代碼、API 回應、CLI 輸出都沒有改變；report 的寫入路徑（第 1、4 節）完全沒有動。

```text
呼叫端（api/、enroll/、alerts/、pki/、*_cli.rs、audit_monitor、probe、diag、export）
  │ &dyn Store（連線池的 closure）或 DbHandle（CLI）；只呼叫領域方法，不寫 SQL
  ▼
store/db.rs：trait Store ＝ SettingsStore ＋ AuditLog ＋ UserStore ＋ TokenStore ＋ PkiStore
             ＋ EnrollStore ＋ AlertStore ＋ HostStore ＋ ApiQueries ＋ AdminStore ＋ RemoteStore，
             加上 begin(TxMode)
             trait Tx：交易中的 Store（commit；沒有 commit 就 drop 時回復）
             StoreError（訊息與後端相同）、StoreErrorKind、in_write_tx
  ▼
store/sqlite/conn.rs：rusqlite::Connection 與 Transaction 實作 Store／Tx（AsConn＋blanket impl；
                      forward! 巨集把每個領域方法轉呼叫到下列檔案中原本的函式）
store/sqlite/{audit,users,tokens,pki,enroll,alerts,hosts,api,admin}.rs：原封不動搬來的 SQL
                      store/sqlite/remote.rs：F8 的資料表（schema v9）與 SQL
```

- **領域介面與位置**：

  | trait | 定義 | 內容 |
  |---|---|---|
  | `SettingsStore` | `settings.rs` | `settings` 表、`meta` 的鍵值 |
  | `AuditLog` | `audit.rs` | 稽核雜湊鏈的寫入、驗證、查詢、匯出、清除 |
  | `UserStore` | `users/store.rs` | 帳號、角色指派、工作階段、TOTP |
  | `TokenStore` | `api/tokens.rs` | `api_tokens` |
  | `PkiStore` | `pki/store.rs` | 簽發 CA、憑證、撤銷、CRL |
  | `EnrollStore` | `enroll/store.rs` | 註冊 token、申請、重放快取、封鎖、清冊中的註冊欄位 |
  | `AlertStore` | `alerts/store.rs` | 規則、事件、靜音、管道、發送紀錄、動作佇列、規則試算的查詢 |
  | `HostStore` | `store/hosts.rs` | 清冊與 report／彙總的讀取、資產、資料庫大小、備份、匯出、刪除主機資料 |
  | `ApiQueries` | `store/api.rs` | 主控台畫面的跨表查詢（主機清單與詳情、憑證所屬主機） |
  | `AdminStore` | `store/admin.rs` | 個資匯出、診斷包的摘要 |
  | `RemoteStore` | `remote/store.rs` | 集中設定、Pulse 版本庫、升級推送（0.9.0 F8，schema v9，第 18 節） |

  report 的寫入仍是 `ReportStore`（單一 writer，`SqliteStore`）。
- **交易**：`Store::begin(TxMode::Deferred)` 與 `TxMode::Immediate` 送出與原本相同的 `BEGIN DEFERRED`／`BEGIN IMMEDIATE`（以 `rusqlite::Transaction::new_unchecked` 實作，不需要 `&mut`）。原本依賴 SQLite 全域寫入鎖的操作（HA.md 第 2.2 節 D1～D9）都還在 `TxMode::Immediate` 的交易中，各 trait 的說明寫明了交易要求（例如 `EnrollStore` 的 D2～D4、`PkiStore` 的 D5、`AlertStore` 的 D6、D7、`AuditLog` 的 D1）。`store::db::in_write_tx` 取代原本 `audit::in_write_tx`（已在交易中時沿用，否則開 `BEGIN IMMEDIATE`）。
- **連線**：主控台的唯讀池與低頻寫入（`api/db.rs`）、註冊服務的連線（`enroll/db.rs`）改持有 `DbHandle`（`Box<dyn Store + Send>`），由 `store::open_read`、`open_console_read`、`open_write` 開啟（開啟方式與 PRAGMA 與原本相同，仍只經由 SQLite 開啟資料庫檔，第 1 節）。CLI 以 `store::open_admin`、`open_exclusive`、`open_read`、`open_for_diag`、`open_for_probe` 開啟。
- **錯誤**：`StoreError` 包著後端原本的錯誤，`Display` 不變，所以日誌、`storage_unavailable` 的訊息與 CLI 的輸出都與 0.8.0 相同；`io_kind()` 讓 `kairos reports export` 仍能把輸出被關閉（`BrokenPipe`）視為正常結束。
- **仍直接使用 rusqlite 的地方**（都在 `store/` 內，或只在測試中）：

  | 位置 | 原因 |
  |---|---|
  | `store/sqlite.rs`（`SqliteStore`、migration、開啟、`list_hosts`、`export_reports`、`backup_to`） | report 的寫入路徑與 schema 本身是 SQLite 後端；L1b 另寫 `postgres::*` |
  | `store/sqlite/*.rs` | SQLite 實作（搬來的 SQL） |
  | `store/import.rs` | `kairos reports import` 以獨占連線、`BEGIN IMMEDIATE` 分批寫入，與 writer 的內部結構（序列、彙總）綁在一起；叢集不支援 JSONL（HA.md 第 21 節 #17），L3 的 `kairos db migrate` 另寫 |
  | `store/purge.rs`、`store/sqlite_purge.rs` | 刪除主機資料的逐步刪除由 writer 執行，與 writer 的快取綁在一起；呼叫端經由 `HostStore::purge_plan`／`purge_run_to_end` 與 `PkiStore::revoke_active_certificates` 使用 |
  | 測試（`*_tests.rs`、`tests.rs`、`bench.rs`、各模組的 `mod tests`） | 以 SQL 準備資料或檢查結果；經由連線池時使用 `Store::test_sqlite()`（只在 `cfg(test)`） |

- **量測**：重構前後以第 11 節的 `sqlite_throughput` 比較（同一台 kp-build、同一天），見下表。

  kp-build 2026-10-19，重構前（`78c6abb`）與重構後依序各跑兩次（release；另一條建置線同時在這台機器上編譯，數字有雜訊）：

  | 情境 | 版本 | p50 | p99 | p99.9 | 最大 |
  |---|---|---|---|---|---|
  | 每秒 2,000 筆 × 30 秒 | 重構前 #1／#2 | 0.81／0.80 ms | 2.8／2.8 ms | 9.9／10.0 ms | 20.1／20.7 ms |
  | | 重構後 #1／#2 | 0.81／0.81 ms | 3.2／3.0 ms | 11.7／9.7 ms | 21.4／21.2 ms |
  | 補送 8,000 筆／秒 × 60 秒（durable、2% 重送、清冊同步） | 重構前 #1／#2 | 1.84／1.92 ms | 55.3／55.8 ms | 99.9／107.6 ms | 125.1／127.9 ms |
  | | 重構後 #1／#2 | 1.87／1.90 ms | 54.6／50.9 ms | 99.6／98.8 ms | 125.6／125.9 ms |

  兩種情境都全部寫入（重送的 9,600 筆皆為 `Duplicate`），沒有 `storage_unavailable` 與結果不明。差異在兩次量測之間的波動範圍內：report 的寫入路徑（`SqliteStore`）沒有經過新的介面，重構不影響它。

## 18. 集中設定與 Pulse 遠端升級（schema v9；0.9.0〔草稿〕；商業審計 F8）

規格見 [`crates/kairos-proto/REMOTE.md`](../crates/kairos-proto/REMOTE.md) 第 6 節（草案）；實作的 SQL 在 `store/sqlite/remote.rs` 的 `SCHEMA_V9`，領域介面 `RemoteStore`（`remote/store.rs`，已加入 `Store`），API 見 [`API.md`](API.md) 第 17 節。v9 只新增資料表與 `hosts` 的欄位，不修改既有的唯一鍵。

| 資料表 | 內容 | 主鍵 |
|---|---|---|
| `config_profiles` | 設定檔：`name`（不分大小寫唯一）、`description`、`settings`（兩層 JSON，正規化）、`revision`、建立與修改的時間與執行者 | `profile_id` |
| `config_profile_revisions` | 每一版的 `settings`、`description`、`changed_at`、`changed_by`、`reason`（全部保留） | (`profile_id`, `revision`) |
| `config_assignments` | 指派：`profile_id`、`selector_kind`（`global`／`group`／`tag`／`host`）、`selector_value`（group 小寫、host 為小寫的 agent_id）、`priority`、`expires_at`；(`selector_kind`, `selector_value`, `profile_id`) 唯一 | `assignment_id` |
| `host_config` | 每台主機：期望設定（`desired_rev`、`desired_hash`、`desired`、`sources`）、Pulse 最近一次 `config_status`（`applied_rev`、`applied_hash`、`state`、`effective`、`overridden`、`ignored`、`errors`、`code`、`reported_at`）、最近一次 hello 的 `tags`、`agent_version`、`install_config`；索引 `host_config_state` | `agent_id` |
| `releases` | Pulse 發佈版本：`version`（(`product`, `version`) 唯一，`product` 只能是 `pulse`）、`commit_id`、`source`（`hosted`；0.10.0 起另有 `external`＋`base_url`：外部 URL 的版本庫，發佈檔不保存，`release_artifacts.size_bytes` 大於 0 而 `stored = 0` 表示已登記）、`sha256sums`、`signature`、`signer_fingerprint`／`signer_principal`（匯入時以 `[remote] allowed_signers` 驗證；沒有設定時 NULL）、`manifest_name`、`manifest`、匯入的時間與執行者 | `release_id` |
| `release_artifacts` | 每個發佈檔：`filename`、`os`、`arch`、`format`、`sha256`、`size_bytes`、`stored`（檔案在 `<releases_dir>/<sha256>`）、`stored_at` | (`release_id`, `filename`) |
| `rollouts` | 推送：`rollout_id`（`r-YYYYMMDD-NNNN`）、`name`、`release_id`、`selector`／`waves`／`schedule`／`options`（JSON）、`state`、`current_wave`、`wave_started_at`、`soak_until`、`awaiting_continue`、`pause_reason`、`pause_cause`（自動暫停的原因，告警用）、`cancel_reason`、建立、開始、完成與更新的時間（`approved_*` 保留給 0.10 的雙人覆核）；索引 `rollouts_state` | `rollout_id` |
| `rollout_hosts` | 推送中的每台主機：`upgrade_id`（唯一）、`wave`（`skipped` 為 -1）、`ordinal`、`state`、`reason`、`from_version`、`to_version`、`attempts`、`bytes_done`／`bytes_total`、`code`、`detail`、`offered_at`、`applying_at`、`finished_at`、`updated_at`、`acknowledged_at`／`_by`；索引 `rollout_hosts_host`、`rollout_hosts_state` | (`rollout_id`, `host_id`) |

- `hosts` 新增 `install_method`、`install_upgrade`、`install_config`（hello 的 `install`，由清冊同步寫入，與 `os` 相同以 `coalesce` 保留上一次的值）。schema v12 另有 `install_external_url`（hello 的 `install.external_url`；每次帶 `install.upgrade` 的 hello 整個取代，不保留舊值，見前面「schema 版本」的 v12）。schema v13 另有 `install_rollback`（hello 的 `install.rollback`；寫入規則相同）。
- `meta.config_rev`：全域單調遞增的設定版本（任何設定檔、指派改變時加 1）。
- **與 REMOTE.md 第 6 節草案的差異**：`rollouts` 另有 `target_version`、`pause_cause`，`release_id` 不設外鍵（rusqlite 內附的 SQLite 預設開啟外鍵檢查，刪除版本後已結束的推送仍要能顯示目標）；`host_config` 以 `agent_id` 為鍵、另記最近一次 hello 的標籤與版本（主機可能在 report 寫入清冊之前就送出 `config_get`；清冊每 30 秒才同步一次，期望設定以 hello 的標籤計算）；主鍵欄位沿用既有的命名（`profile_id`、`assignment_id`、`release_id`）；`releases` 另有 `signer_principal`、`manifest_name`；`release_artifacts` 另有 `stored_at`；`rollout_hosts` 另有 `acknowledged_at`／`_by`（回滾失敗標記為已處理）。
- **寫入**：遠端管理有自己的資料庫連線（與註冊服務相同的做法：一條寫入連線、busy 等待 2 秒、兩個寫入交易之間至少間隔 2 毫秒，加上數條唯讀連線），每個操作一個 `BEGIN IMMEDIATE` 交易、稽核在同一個交易中。8443 上的 `config_status`、`upgrade_status` 各一個短交易；`config_get`、`upgrade_get`、`upgrade_chunk_get` 只讀（發佈檔的內容從檔案讀，不在資料庫中）。心跳的 `pending` 不查資料庫（記憶體中的 `desired_rev` 與提議中的 `upgrade_id`，啟動時載入）。
- **重新計算**：一次設定變更在同一個交易中對所有主機計算合併後的設定與雜湊，只寫入雜湊改變的主機（REMOTE.md 第 3.4.4 節）。
- **保存期限**：設定檔的版本全部保留；已完成或取消的推送在 `[remote] rollout_retention_days`（預設 400 天）後刪除（含主機表），每小時檢查一次。
- **刪除主機資料**（第 16 節）一併刪除 `host_config` 與 `rollout_hosts` 中這台主機的紀錄（`remote_management` 筆數）；個資匯出另有 `remote_management`（`host_config` 與推送中的紀錄）。以 `host:<agent_id>` 選擇器的指派不刪除（它是設定，不是主機的資料）。
- **備份**：`kairos db backup` 包含這些資料表，但**不含版本庫的發佈檔**（`<資料目錄>/releases/`，`[remote] releases_dir`）：可以從發佈處重新取得並以 `PUT …/artifacts/{filename}` 重新上傳。還原後 `stored = 1` 而檔案不存在的發佈檔，提議時 `upgrade_chunk_get` 回 `upgrade_unavailable`（日誌「版本庫的 … 無法讀取」），請重新上傳。
- 升級到 v9 之後 0.8 的 Kairos 無法開啟（降版從備份還原）；任何開啟資料庫的 CLI 同樣會升級 schema。

## 19. PostgreSQL 後端（schema v9 起；0.10.0〔草稿〕；HA L1b）

`storage.backend = "postgres"` 時，Kairos 把 SQLite 後端保存的所有資料（原始 report、彙總、主機清冊、資產、告警、使用者與工作階段、稽核雜湊鏈、註冊與 PKI、設定、指標轉出設定、遠端管理）改放在外部的 PostgreSQL。功能、API、CLI 與 `kairos.db` 相同；規格見 [`HA.md`](HA.md) 第 4、5、7、12 節。**SQLite 仍是預設值**，沒有設定 `backend = "postgres"` 的部署行為不變。本節只談儲存後端：沒有啟用叢集模式（`cluster.enabled = false`）時，**同一個資料庫與 schema 同時只能有一個 Kairos 使用**（第 19.7 節；啟動時檢查）。多個 Kairos 共用一個資料庫的叢集協調（租約、leader、fencing；HA.md 第 6 節）與它的資料表（schema v10）見第 20 節。

### 19.1 設定與需求

```toml
[storage]
backend = "postgres"

[storage.postgres]
host = "db.example.com"         # 或 Unix socket 的目錄（/var/run/postgresql）
port = 5432
dbname = "kairos"
user = "kairos"
password_file = "/etc/kairos/pg.password"   # 0600；不接受寫在設定檔中的密碼
schema = "kairos"
sslmode = "verify-full"         # 未設定時：loopback 與 Unix socket 為 disable，其他為 verify-full
ca_file = "/etc/kairos/pg-ca.pem"
pool_size = 16
connect_timeout_secs = 5
```

| 項目 | 要求 | 不符合時 |
|---|---|---|
| 版本 | PostgreSQL 14 以上；只用社群版的功能，不需要任何擴充 | 拒絕啟動，`KRS-E4402` |
| 編碼 | `UTF8` | 拒絕啟動，`KRS-E4402` |
| 角色 | 連到**主庫**（`pg_is_in_recovery()` 為 false） | 拒絕啟動，`KRS-E4402` |
| 權限 | 能在資料庫中建立 `schema`（或對既有的 schema 有 `CREATE`、`USAGE`）；schema 必須是 Kairos 專用的 | 拒絕啟動，`KRS-E4402` |
| 連不上 | — | 啟動時拒絕啟動（`KRS-E4401`，結束代碼 3）；執行中 report 回 `storage_unavailable`、`/readyz` 503，恢復後自動重新連線 |
| 同步複寫 | `synchronous_commit` 為 `on`／`remote_write`／`remote_apply` 且 `synchronous_standby_names` 非空 | 照常啟動；啟動時與之後每小時記一次 `KRS-W4403`（主庫故障時可能遺失最後幾個已 `ack` 的交易） |
| TLS | 非本機時預設 `verify-full`（`ca_file`，未設定時為作業系統的信任清單） | `require`（不驗證憑證）與非本機的 `disable` 照常啟動並記 `WARN` |
| 連線數 | 資料庫的 `max_connections` ≥ `pool_size` ＋ 4 | `WARN` |
| 時鐘 | 與資料庫的時差不超過 2 秒 | `KRS-W8305`（`WARN`） |
| 連線池中介 | PgBouncer 只支援 session pooling（交易層級與 session 層級的 advisory lock 需要固定的連線） | 偵測不到 |

- **密碼**只從 `password_file` 讀取，每次建立連線時重新讀（輪替密碼不必重新啟動）；不寫入日誌，`kairos diag` 與 `--check-config` 的輸出也沒有。
- **`kairos --check-config`** 會實際連線並回報版本、schema 的狀態與上表的警告（項目 `database`）；不建立也不修改任何東西。
- **資料目錄仍然需要**（`storage.data_dir`）：金鑰檔（`audit-hmac.key`、`alerts-secrets.key`、`console.key`、`privacy.key`）、簽發 CA 的私鑰與 PKI 快取、遠端升級的版本庫、`agents.json` 與 JSONL 鏡像都留在本機。備份與還原時金鑰與資料庫要是同一個時間點的組合（第 9 節）。
- **連線**：連線池（`pool_size`，預設 16）供主控台、註冊、告警、遠端管理與 CLI 使用，用完即歸還；report 的寫入另有一條專用連線（`statement_timeout` 為 `timeouts.write_secs`）。

### 19.2 schema

與 SQLite 的 schema 一對一（`store/postgres/schema.rs`；目前是 v13：`SCHEMA_V9` 加上 `UPGRADES` 的升級步驟 v10、v11、v12、v13）：資料表、欄位、索引名稱相同，第一次啟動（或任何會建立資料庫的 CLI）時在空的 schema 上以一個交易建立目前的版本。對應規則（HA.md 第 5.3 節）：

| SQLite | PostgreSQL |
|---|---|
| `INTEGER`（含 0/1 旗標） | `BIGINT` |
| `REAL`、`BLOB` | `DOUBLE PRECISION`、`BYTEA` |
| `TEXT` | `TEXT COLLATE "C"`（位元組順序，與 SQLite 的 BINARY 相同；排序與分頁的結果一致） |
| `INTEGER PRIMARY KEY`（rowid 別名） | `BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY` |
| `WITHOUT ROWID` | 刪除 |
| 無型別的 `meta.value` | `TEXT`（數值以文字保存） |
| 無型別的 `reports.agent_ts` | 拆成 `agent_ts_ms BIGINT` 與 `agent_ts_text TEXT`（最多一個非 NULL） |
| `COLLATE NOCASE`（`users.username`、`config_profiles.name`） | 唯一索引 `lower(欄位)`，比對一律 `lower()` |
| `PRAGMA user_version`、`application_id` | `meta` 的 `schema_version`、`application`（值 `KRS1`） |
| 外鍵（rusqlite 內附的 SQLite 預設開啟） | 相同的外鍵 |

- **HA.md 第 5.5 節列的叢集資料表與欄位**（`cluster_nodes`、`cluster_leases`、`cluster_revs`、`host_presence`、`cluster_events`、`alert_event_seq`、`login_challenges`、`enroll_nonces`、`auth_failures`、`enroll_requests.issuing_node`、`alert_deliveries.claim_epoch`）**不在 v9**：0.9.0 實際發佈的 v9 是遠端管理的資料表（第 18 節），叢集的資料表在 **v10** 新增（兩個後端同時，第 20 節）。
- schema 的版本比程式新時拒絕啟動（`KRS-E4405`）；不是 Kairos 建立的 schema（有資料表但沒有 `meta.application = KRS1`）拒絕使用（`KRS-E4402`）；`kairos db migrate` 尚未完成的 schema 拒絕使用（`KRS-E4404`）。
- PostgreSQL 後端從 v9 開始（`BASE_VERSION`），沒有 v1～v8 的升級步驟（舊資料以 `kairos db migrate` 從 SQLite 搬入）。v9 之後的版本有升級路徑：`UPGRADES` 列出每一版的 SQL，既有的 v9（或 0.10.0 開發期間的 v10）schema 在啟動（或 CLI 開啟資料庫）時自動升級到 v11（第 20.3 節）。

### 19.3 並行控制

PostgreSQL 的交易一律是 READ COMMITTED。SQLite 靠全域寫入鎖序列化的地方（HA.md 第 2.2 節 D1～D9）這樣處理：

- **序列化的寫入交易**（呼叫端以 `TxMode::Immediate` 開始的交易：註冊、簽發、告警、使用者、設定、遠端管理的每個業務操作）在 `BEGIN` 之後取得一個交易層級的 advisory lock（`pg_advisory_xact_lock`，鍵由「KRS1」與 schema 名稱導出），彼此排成一列，語意與 `BEGIN IMMEDIATE` 相同，跨行程、跨節點都成立。等待超過連線的 busy 時間（主控台背景寫入 250 ms、互動寫入與註冊 2 秒、CLI 5 秒）回暫時性錯誤（API 503、report `storage_unavailable`），與 SQLite 的 busy 相同。
  - 這比 HA.md 第 5.4 節逐項改寫成條件式 UPDATE 或列鎖保守：正確性不依賴每個操作各自的寫法，代價是這些交易不能並行（它們的量很小：管理操作、每秒最多數十筆註冊）。能寫成單一原子敘述的操作仍然改寫了（token 扣次數 D2、設定版本 D6、TOTP 重設等），作為沒有鎖時的保險。
- **稽核雜湊鏈**（HA.md 第 12 節）：寫入前取得另一個 advisory lock，`id` 在持鎖期間指定為 `max(id) + 1`，鎖持有到交易結束，所以鏈的順序等於 commit 的順序、`id` 連續沒有空號（回復的交易不佔用 `id`）。雜湊與 HMAC 的計算與 SQLite 完全相同，搬移後可以直接驗證。驗證在 `REPEATABLE READ` 的唯讀交易中進行。
  - 與 HA.md 第 12.1 節的差別：以 advisory lock 取代 `meta.audit_chain_head` 的列鎖（`SELECT … FOR UPDATE`）；鏈尾仍記在 `meta.audit_chain_head`，內容與 SQLite 相同。
- **report 的寫入**不經過上面的鎖：每批一個交易，`reports` 以一個多列的 `INSERT … ON CONFLICT (boot_ref, report_seq) WHERE boot_ref IS NOT NULL DO NOTHING RETURNING` 寫入並取回哪幾列真的寫入，其餘為 `Duplicate`；主機、序列與 boot 以 `ON CONFLICT DO NOTHING` 建立。
- **錯誤分類**（HA.md 第 7.1 節）：commit 之前的錯誤為 `Failed`；死結（`40P01`）與序列化失敗（`40001`）整批自動重試最多 3 次（`kairos_db_transaction_retries_total`）；commit 時連線中斷為 `Unknown`（不回應並關閉連線，Pulse 重送後由唯一索引去重）；磁碟已滿（`53100`）為 `Failed`。領域 store 的交易邊界在呼叫端，無法在儲存層自動重試整個交易：遇到 `40P01`／`40001`／鎖等待逾時回暫時性錯誤，由呼叫端（API 503、Pulse 重試）處理。
- **「服務執行中」的鎖**：Kairos 的寫入連線持有一個 session 層級的共享 advisory lock。需要獨占資料庫的 CLI（`kairos hosts purge` 的離線模式、`kairos db migrate`）取互斥鎖，取不到時回 `KRS-E4304`，與 SQLite 以資料庫檔的 EXCLUSIVE 鎖判斷相同。

### 19.4 彙總與計數器速率

與 SQLite 後端相同：單一 writer 的記憶體桶（第 7 節）、寫入時以記憶體中的上一筆計算速率（第 15.1 節），重新啟動後由原始資料補回。兩個後端對同一組輸入產生相同的 `rollup_5m`、`rollup_1h`（共用的測試確認）。

HA.md 第 7.3、7.4 節的「leader 從原始資料產生彙總」是為了多個節點同時寫入（階段 B）設計的；只有一個 writer 時記憶體桶的不變式成立，這一版沒有實作 leader 版（HA.md 第 20.3 節允許的順序），留給階段 B。

### 19.5 保留期限與容量上限

- **保留期限**：規則與第 8.1 節相同（每步 5,000 列一個交易）。
- **容量上限**（`storage.max_total_bytes`）：使用量是這個 schema 中所有資料表（含索引與 TOAST）的 `pg_total_relation_size` 加總，每 60 秒量一次，不含 WAL。PostgreSQL 刪除列之後檔案不會變小（空間由 autovacuum 回收後重複使用），照 SQLite「刪到量測值低於上限的 95%」的規則會把資料刪光，所以改為**估計制**：
  1. 使用量超過門檻時開始一輪刪除。門檻是 `max(上限, 上一輪結束時的使用量 ＋ 上限的 1%)`（上一輪的使用量記在 `meta.capacity_high_water`，重新啟動後不會重複刪除）。
  2. 這一輪要釋放的量是「使用量 − 上限的 95%」；以「資料表大小 ÷ 列數」估計每列的大小（先 `ANALYZE`，每列至少算 64 bytes），依序刪除最舊的原始 report、5 分鐘彙總、1 小時彙總，逐批扣抵，扣完就停。
  3. 一輪結束後在交易外對被刪的資料表執行 `VACUUM (ANALYZE)`，讓空間可以重複使用。
  - 限制：索引膨脹會讓它比必要更早刪除；`capacity_event.used_bytes` 與 `kairos_database_used_bytes` 是資料表檔案的大小，刪除後不會下降。大量刪除後可以在離峰時段執行 `VACUUM FULL` 或 `pg_repack` 真正縮小檔案。
- **日期保護**（DateGuard）與 `meta` 的 `capacity_event`、`retention_last` 與 SQLite 相同。

### 19.6 `kairos db migrate`：SQLite 與 PostgreSQL 互搬

```bash
sudo systemctl stop kairos
# SQLite → PostgreSQL：設定檔的 storage.backend 仍是 "sqlite"，[storage.postgres] 填目的資料庫
sudo -u kairos kairos db migrate --to postgres --config /opt/kairos/kairos.toml [--reports-since 2d] [--dry-run]
# 成功後把 storage.backend 改成 "postgres"，執行 kairos --check-config，再啟動

# PostgreSQL → SQLite（回到單機，或匯出成可攜的快照）：storage.backend 是 "postgres"
sudo -u kairos kairos db migrate --to sqlite:/var/backups/kairos/export.db --config /opt/kairos/kairos.toml
```

- **來源**是設定中目前的後端，**目的**由 `--to` 指定，必須是空的（PostgreSQL：schema 不存在或沒有資料表；SQLite：檔案不存在）。不做合併。來源不會被修改。
- **一致性**：來源或目的是 SQLite 時以獨占鎖開啟；是 PostgreSQL 時取得「服務執行中」的互斥鎖（第 19.3 節），PostgreSQL 來源另以 `REPEATABLE READ` 的快照讀取。Kairos 執行中時回 `KRS-E4304`。
- **方式**：依資料表的相依順序複製，保留所有主鍵值，每張表一個交易；複製後以 `setval` 對齊 PostgreSQL 的 IDENTITY 序列。寫入 PostgreSQL 時每批 2,000 列一個多列 `INSERT … SELECT FROM unnest(…)`（HA.md 第 5.6 節寫的是 `COPY … (FORMAT binary)`；多列 INSERT 不需要自己實作 COPY 的二進位格式，速度見第 19.8 節）。
- **`--reports-since <期間>`**：只複製取樣時間在範圍內的原始 report 與行程快照；彙總、清冊、告警、稽核等一律全部複製。
- **驗證**（不可略過）：每張表的列數；`reports` 的 id 範圍與抽樣 1,000 列的內容；稽核鏈在目的端的驗證結果（雜湊鏈與 HMAC，使用資料目錄的 `audit-hmac.key`）必須與來源**相同**。任何一項不一致以 `KRS-E4404` 結束（結束代碼 1）。
  - 來源的鏈原本就有斷點時，搬移不會因此失敗（目的端有相同的斷點），輸出會註明。
- **未完成的目的端不可使用**：PostgreSQL 在驗證通過後才寫入 `meta.schema_version`（之前只有 `meta.migration_in_progress`，Kairos 以 `KRS-E4404` 拒絕啟動）；SQLite 先寫到 `<檔案>.partial`，驗證通過後才改名。重新執行需要 `--restart`（先清空目的端）。
- `--dry-run` 只列出每張表的列數；`--json` 的 `schema` 為 `kairos.db.migrate/v1`。

### 19.7 與 SQLite 後端不同的地方

| 項目 | PostgreSQL 後端 |
|---|---|
| `kairos db backup` | 不支援（`KRS-E4406`〔暫定〕）：改用 `pg_dump`、pgBackRest 或 `pg_basebackup` 加 WAL 歸檔；需要可攜的 SQLite 快照時用 `kairos db migrate --to sqlite:<檔案>`。`db restore-keys` 不受影響（金鑰不在資料庫中，請另外備份資料目錄的金鑰檔） |
| `kairos reports import` | 不支援（`KRS-E4406`〔暫定〕）：先匯入到 SQLite，再 `kairos db migrate --to postgres` |
| `/metrics` | `kairos_database_file_bytes`、`kairos_database_wal_bytes` 為 0；`kairos_database_used_bytes` 是資料表與索引的大小。另有 `kairos_db_pool_connections{state}`、`kairos_db_pool_size`、`kairos_db_pool_errors_total{kind}`、`kairos_db_transaction_retries_total{reason}`、`kairos_db_clock_skew_seconds`、`kairos_db_synchronous_replication` |
| `/readyz` 的 `database` | 連線池能取得連線、連到的是可寫的主庫、schema 可用（5 秒快取） |
| 系統健康的 `database` 項目 | `values.backend` 為 `postgres`，另有 `values.postgres`（`target`、`sslmode`、`server_version`、`synchronous_replication`、`clock_skew_ms`、`pool`）；`file_bytes`、`wal_bytes` 為 0，`page_size` 是 PostgreSQL 的區塊大小 |
| 容量上限 | 估計制（第 19.5 節） |
| 資料庫檔被刪除或替換時自動重建 | 不適用；連線中斷時自動重新連線，期間 report 回 `storage_unavailable` |
| 同時使用同一個資料庫的 Kairos | 沒有啟用叢集模式時**只能有一個**：report 的彙總（記憶體桶）、告警引擎、保留清除與啟動時把主機標為離線都假設只有一個行程（HA.md 第 2 節）。啟動時以 advisory lock 檢查（`LockKind::Mode`，第 20.5 節）：第二個 Kairos 拒絕啟動（`KRS-E4402`，結束代碼 3）。多個 Kairos 共用資料庫要啟用叢集模式（`cluster.enabled = true`，第 20 節；任何時間只有持有 leader 租約的節點執行這些工作） |

### 19.8 量測

`store_throughput`（第 11 節；`KP_BENCH_BACKEND=postgres` 加 `KAIROS_TEST_PG_URL`）在 kp-build（4 vCPU、4 GB RAM、與另一條開發線共用）2026-11-11 的結果。PostgreSQL 14 跑在同一台機器的容器中（記憶體上限 512 MB、預設參數 `shared_buffers = 128MB`、`synchronous_commit = on`、沒有備庫，經 docker 的埠轉送連線）；這是**小規模、資源受限的對照**，不是 HA.md 第 7.6 節的目標環境，該節的目標（叢集每秒 2,000 筆時 p99 < 50 ms、每秒 8,000 筆的補送尖峰 5 分鐘沒有 `storage_unavailable`）**在這個環境沒有達到**（見下），需要在正式規格的硬體與獨立的資料庫主機上重新量測。

每筆 17 個指標；延遲是從放入佇列到收到結果。

| 情境 | 後端 | 筆數 | p50 | p99 | 最大 | 超過 5 秒的筆數 |
|---|---|---|---|---|---|---|
| 每秒 2,000 筆、100 台主機、30 秒 | PostgreSQL | 60,000 | 4.1 ms | 41 ms | 238 ms | 0 |
| 每秒 8,000 筆、100 台主機、30 秒 | PostgreSQL | 240,000 | 5.5 ms | 85 ms | 311 ms | 0 |
| 同上 | SQLite | 240,000 | 1.3 ms | 12 ms | 23 ms | 0 |
| 每秒 2,000 筆、**1 萬台全新的主機**、120 秒 | PostgreSQL | 240,000 | 4.4 ms | 6.1 秒 | 6.7 秒 | 8,172（3.4%） |
| 同上 | SQLite | 240,000 | 0.88 ms | 3.8 ms | 39 ms | 0 |
| 每秒 2,000 筆、1 萬台全新的主機、30 秒 | PostgreSQL | 60,000 | 452 ms | 7.8 秒 | 7.9 秒 | 14,596（24%） |
| 同上 | SQLite | 60,000 | 0.81 ms | 9.9 ms | 31 ms | 0 |

補送尖峰（第 11.1 節的情境：1 萬台、每筆帶去重鍵、取樣時間從 72 小時前起、2% 重送、同時第一次清冊同步；`KP_BENCH_TIMEOUT=1` 與連線相同地在 5 秒取消）60 秒：

| 速率 | 後端 | 寫入 | 重複 | `storage_unavailable`（5 秒內未開始寫入） | 逾時（已開始寫入、6 秒內沒有結果） |
|---|---|---|---|---|---|
| 每秒 8,000 筆 | SQLite（沒有模擬逾時） | 480,000 | 9,600 | 0（p99 48 ms、最大 143 ms） | 0 |
| 每秒 8,000 筆 | PostgreSQL | 355,650 | 6,990 | 126,836（26%） | 6,119 |
| 每秒 4,000 筆 | PostgreSQL | 217,578 | 4,296 | 22,869（9.5%） | 2,943 |

- **穩定狀態**（主機都已在清冊中）PostgreSQL 的延遲是 SQLite 的 3～7 倍，每秒 8,000 筆仍在 100 ms 內（p99），沒有逾時。
- **大量新主機同時出現**是瓶頸：每台主機第一次出現時要寫入它用到的序列（`host_series`，每台 18 列，帶外鍵檢查），慢查詢日誌顯示每批（約 1,000 台、1.8 萬列）這個敘述約 1 秒。1 萬台全新的主機在 5 秒內全部出現時，前 15 秒左右的 report 會等 5 秒以上（連線回 `storage_unavailable`，Pulse 0.3 以上重送，不遺失）；之後回到穩定狀態。主機已在清冊中時（一般的重新啟動、`kairos db migrate` 之後）沒有這個階段。實作時已把這裡的逐筆往返改成每個交易一個敘述（改之前同一個情境 30 秒內 p50 為 37 秒）；再進一步要調整 `host_series` 的寫法或外鍵，尚未做。
- **補送尖峰**在這個環境撐不住每秒 8,000 筆：除了上面的啟動階段，寫入一旦落後，記憶體中的桶因為「15 秒沒有新值」被寫出，之後同一個桶的資料走遲到路徑（逐批 upsert 彙總表），讓寫入更慢。SQLite 在同一台機器上沒有落後，所以沒有進入這個狀態。這是 HA.md 第 7.3 節提到「補送尖峰時 upsert 太多」的情況，需要在正式環境量測後決定是否調整寫出規則。
- **大小**：`reports`（含索引與 TOAST）每筆約 305～375 bytes，SQLite 約 204～215 bytes（PostgreSQL 的列標頭與索引較大）；容量估算（第 10 節）在 PostgreSQL 上請乘以約 1.6。
- **沒有量測的**：`kairos db migrate` 的速度（HA.md 第 5.6 節的時間估算）、同步備庫下的延遲、1 萬台以上的規模、長時間（含 5 分鐘彙總寫出與保留清除）的行為、PostgreSQL 17 的效能（只跑過功能測試）。

### 19.9 測試

- 儲存層的測試以同一個測試本體在兩個後端各跑一次（`store/backend_tests.rs`，測試名稱以 `_sqlite`、`_postgres` 結尾）。PostgreSQL 的測試由環境變數指定資料庫：

  ```bash
  docker run -d --name kairos-test-pg -e POSTGRES_USER=kairos -e POSTGRES_PASSWORD=kairos -e POSTGRES_DB=kairos \
    -p 127.0.0.1:55432:5432 postgres:14-alpine
  KAIROS_TEST_PG_URL=postgres://kairos:kairos@127.0.0.1:55432/kairos cargo test -p kairos store::
  KAIROS_TEST_PG_URL=… cargo test -p kp-e2e --test postgres      # 實際的 Kairos＋Pulse 跑在 PostgreSQL 上
  ```

  共用的測試本體 25 個（report 路徑 14 個、領域 store 11 個）：PostgreSQL 的版本除了在 PostgreSQL 上執行，還在 SQLite 上再跑一次，兩邊每個步驟的結果必須完全相同；另有 PostgreSQL 專屬的 9 個（連線池、唯讀連線、服務鎖、schema 檢查、8 條連線並行寫稽核鏈、並行的原子更新）與 `db migrate` 的 3 個。每個測試使用自己的 schema，結束時刪除。沒有設定 `KAIROS_TEST_PG_URL` 時這些測試**明確略過**（在 stderr 印出「略過：未設定 KAIROS_TEST_PG_URL…」），不算驗證過；設定 `KAIROS_TEST_PG_REQUIRED=1` 時沒有可用的資料庫就失敗（CI 的 Linux job 與 `postgres-compat` job 如此設定，分別以 PostgreSQL 14 與 17 執行）。

## 20. 叢集協調的資料表（schema v10；0.10.0；HA L2）

叢集模式（`[cluster] enabled = true`，階段 A：主動／備援）讓多個 Kairos 共用一個 PostgreSQL，以資料庫中的租約選出唯一的主動節點（leader）。規格見 [`HA.md`](HA.md) 第 5.5、6 節，實作與規格的差異見 HA.md 第 6.8 節；部署步驟見 INSTALL 附錄 H。SQL 在 `store/sqlite/cluster.rs` 與 `store/postgres/schema.rs` 的 `SCHEMA_V10`，領域介面 `ClusterStore`（`cluster/store.rs`，兩個後端都實作）。

v10 是**獨立的一步**：只新增資料表與三個可為 NULL 的欄位，不修改既有的資料與唯一鍵。（0.10.0 的 schema 版本是 **v11**：v11 是 `rollouts` 的雙人覆核欄位，見第 3 節；0.9 的資料庫升級時依序套用 v10、v11。）**叢集模式只支援 PostgreSQL**；SQLite 也建立同樣的資料表（兩個後端的資料表一致，`kairos db migrate` 才能一對一複製），單機時保持空白。

### 20.1 資料表

時間欄位都是 Unix 毫秒；`cluster_nodes` 與 `cluster_leases` 的時間一律取**資料庫的時鐘**（租約的判斷不受節點時鐘偏差影響）。型別依第 19.2 節的對應規則（下表寫 SQLite 的型別）。

| 資料表 | 內容 | 主鍵 | 階段 A |
|---|---|---|---|
| `cluster_nodes` | 每個節點一列：`hostname`、`version`、`schema_version`、`role`（`standby`／`leader`／`draining`／`stopped`＝正常關機／`lost`＝心跳逾時，由 leader 標記）、`config_hash`（需要一致的設定的雜湊，與 leader 不同時 `KRS-W8505`）、`keys_hash`（金鑰指紋的雜湊）、`advertise`（顯示用的位址）、`started_at`（這次啟動的時間，用來分辨同名節點的不同行程）、`heartbeat_at`（每 `cluster.heartbeat_secs` 秒更新）、`info`（JSON 摘要：`pid`、`connections`、`keys_ok`、`key_problems`、`schema`、`epoch`） | `node_id` | **使用** |
| `cluster_leases` | 租約：`holder`（持有者的 `node_id`）、`epoch`（每次取得加一，fencing 用）、`acquired_at`、`expires_at`。目前只有一列 `name = 'leader'` | `name` | **使用** |
| `cluster_revs` | 跨節點的變更版本（`name`、`rev`） | `name` | 空白（`alerts_config_rev` 等仍在 `meta`） |
| `host_presence` | agent 連線的歸屬與線上狀態：`conn_node`、`conn_epoch`、`online`、`connected_at`、`last_seen`、`offline_reason`、`peer`、`rev`；索引 `host_presence_rev`、`host_presence_node`（只含線上的列） | `host_id` | 空白（線上狀態仍在主動節點的記憶體與 `hosts`） |
| `cluster_events` | 跨節點事件：`at`、`node_id`、`kind`、`target`、`payload` | `id` | 空白 |
| `alert_event_seq` | 告警事件編號的每日序號（`day`、`seq`） | `day` | 空白 |
| `login_challenges` | MFA 挑戰：`user_id`、`created_at`、`expires_at`、`attempts` | `challenge_hash` | 空白（仍在主動節點的記憶體） |
| `enroll_nonces` | 撤回註冊申請的 nonce：`request_id`、`expires_at` | `nonce_hash` | 空白（同上） |
| `auth_failures` | 驗證失敗的速率限制：`tokens`、`updated_at` | `source` | 空白（同上） |

新增的三個欄位（都可為 NULL，階段 A **不寫入**）：

| 欄位 | 用途（階段 B） |
|---|---|
| `enroll_requests.issuing_node`、`enroll_requests.issuing_at` | 多個節點同時簽發時的搶占：哪個節點、何時開始簽發 |
| `alert_deliveries.claim_epoch` | 通知由哪個 leader epoch 認領（換手時不重複發送） |

- **階段 A 實際讀寫的只有 `cluster_nodes`、`cluster_leases`**，以及 `meta` 的兩個鍵（第 20.2 節）。其餘資料表與欄位依規格先建立、保持空白，階段 B（多主動）才寫入；先建立是為了之後不必再為它們升級一次 schema。
- 與 HA.md 第 5.5 節的差異：沒有建立 `enroll_requests_issued_csr`（既有的 `enroll_requests_csr` 已是 (`agent_id`, `csr_sha256`) 的唯一索引）與 `host_presence` 的兩個序列（`conn_epoch_seq`、`presence_rev_seq`；階段 B 再加）。規格把這些資料表列為 v9，實際是 v10（0.9.0 的 v9 已用於遠端管理）。
- **節點的紀錄不會自動刪除**：停止或失聯的節點留在 `cluster_nodes`（`stopped`／`lost`），以 `kairos cluster forget <node_id>` 刪除。
- 這些資料表都很小（每個節點一列、一列租約），不計入容量估算；`cluster_nodes` 與 `cluster_leases` 的寫入量是每個節點每 `heartbeat_secs` 秒一次心跳，加上 leader 每 `lease_ttl_secs / 3` 秒一次續約。

### 20.2 `meta` 的鍵

| 鍵 | 內容 |
|---|---|
| `cluster_id` | 叢集的 UUID（v4）。`kairos cluster init` 第一次執行時產生，之後不變；叢集模式下經 `hello_ack.cluster_id` 送給協商了 `cluster` 的 Pulse |
| `cluster_keys` | JSON `{"v":1,"keys":{"<檔名>":"<指紋>",…}}`：`alerts-secrets.key`、`console.key`、`audit-hmac.key`、`privacy.key` 的指紋（`HMAC-SHA256(金鑰, "kairos-key-fingerprint/v1\n" ‖ 檔名)` 的前 16 bytes，hex；不能用來還原金鑰）。`kairos cluster init` 寫入，每個節點啟動時與之後定期比對自己的金鑰，不符時不參與 leader 選舉（`KRS-E8504`） |

- 簽發 CA 私鑰的指紋不在 `cluster_keys`：節點以私鑰的公鑰雜湊與 `pki_cas.spki_sha256` 比對（`active`／`trusted` 的簽發 CA 都要有私鑰）。
- **資料庫中仍然沒有任何金鑰**（第 3.2 節）：金鑰檔在每個節點的資料目錄，以 `kairos cluster keys export`／`import` 分發與備份（格式 `kairos.keys-backup/v2`，含簽發 CA 私鑰）。
- 單機（沒有執行過 `kairos cluster init`）時這兩個鍵不存在。

### 20.3 升級與降版

- **SQLite**：`MIGRATIONS` 的第 10 步（`store/sqlite.rs`；SQL 在 `store/sqlite/cluster.rs`）與第 11 步（`rollouts` 的雙人覆核欄位，`store/sqlite/remote.rs`），與之前的版本相同——啟動或任何開啟資料庫的 CLI 依 `PRAGMA user_version` 套用，一個交易。
- **PostgreSQL**：`store/postgres/schema.rs` 的 `UPGRADES`（`(10, SCHEMA_V10)`、`(11, SCHEMA_V11)`）。`ensure_schema` 發現 schema 是 v9（`BASE_VERSION`）以上、低於程式的版本時，在一個交易中取得 advisory lock `LockKind::Migrate`、重新讀取版本（等待期間可能已由另一個節點升級）、依序套用比目前版本新的步驟，最後寫入 `meta.schema_version` 與 `meta.schema_upgraded_by`（執行升級的 Kairos 版本）。同時啟動的多個節點只有一個執行升級，其他等待後直接使用。空的 schema 則是建立 v9 之後接著套用同樣的步驟。
  - 觸發的時機：第一個以新版啟動的節點（叢集模式：協調執行緒在備援階段就執行），或任何以可寫方式開啟資料庫的 CLI（例如 `kairos cluster init`）。`kairos --check-config` 只讀取，不升級。
  - 低於 v9 的 PostgreSQL schema 不存在（後端從 v9 開始）；遇到時回 `KRS-E4402`。
- **降版**：升級到 0.10.0 的 schema（v11）之後，0.9 的 Kairos（以及 0.10.0 開發期間 schema v9、v10 的建置）無法開啟——SQLite 回「資料庫的 schema 版本 11 比本版 Kairos 支援的 9 新」，PostgreSQL 回 `KRS-E4405`，都不做任何變更。沒有降版的遷移：請從升級前的備份還原（SQLite：第 9 節；PostgreSQL：`pg_dump` 等的備份）。**升級前先備份**。
- **叢集中的版本升級**：schema 不變的版本可以滾動升級；schema 改變的版本要先停止所有節點，升級所有節點的執行檔，再啟動第一個節點（由它升級 schema）。版本較舊的節點看到較新的 schema 時拒絕啟動（`KRS-E4405`）。步驟見 INSTALL 第 H.5 節。

### 20.4 fencing：租約對寫入的意義

階段 A 沿用記憶體中的彙總桶、告警引擎與單一 writer（第 7 節、第 19.4 節），前提是「同一時間只有一個行程寫入」。這個前提由租約加上 fencing 保證：

- **只有 leader 的行程開啟儲存**。備援節點只有一條協調用的連線（心跳、嘗試取得租約），不啟動 writer、告警、註冊、主控台與保留清除，所以不寫入第 20.1 節以外的任何資料表。
- **leader 行程的每個寫入都先確認租約**。取得租約後連線池設定 fencing（`PgPool::set_fence`，持有者與 epoch）；之後：
  - 每個可寫的交易（`TxMode::Immediate` 與 `Deferred`，含 report 的批次寫入與維護）在 `BEGIN`（與寫入鎖）之後的第一個敘述確認租約；
  - 自動提交的單一 `INSERT`／`UPDATE`／`DELETE` 自動包成「`BEGIN`；確認租約；敘述；`COMMIT`」；
  - 唯讀連線、`REPEATABLE READ` 的唯讀快照與協調自己的連線不檢查。
- **檢查是一個敘述**：`SELECT 1 / count(*) FROM (SELECT 1 FROM cluster_leases WHERE name = 'leader' AND holder = … AND epoch = … AND expires_at > now FOR SHARE)`。租約列符合時取得它的 `FOR SHARE` 列鎖並持有到交易結束——其他節點接手的 `UPDATE` 會等這個交易結束，所以「確認之後、commit 之前租約被搶走」不會發生。不符合時除以零（SQLSTATE 22012），交易失敗，轉成 `KRS-W8502` 並立即以結束代碼 75 結束行程。不需要 PL/pgSQL 或任何擴充。
- **結果**：暫停（虛擬機凍結、長時間的 GC 或 I/O）超過租約時間後恢復的舊 leader，第一個寫入就失敗並結束，不會覆寫新 leader 的資料；在途的 report 沒有收到 `ack`，Pulse 重送後由唯一索引去重（第 5.1 節）。
- **成本**：每個寫入交易多一個以主鍵讀取一列的敘述（多一次往返）。HA.md 第 7.6 節的效能目標尚未在叢集模式下量測。
- 單機模式與所有 CLI 的行程沒有設定 fencing（空操作），行為與第 19 節相同。CLI 的寫入（`kairos enroll approve`、`kairos alerts ack` 等）靠第 19.3 節的寫入鎖與稽核鏈的鎖保持正確，與節點的角色無關。

### 20.5 啟動保護：`LockKind::Mode`

第 19.3 節的「服務執行中」鎖（`LockKind::Service`）之外，另有一個 session 層級的 advisory lock 表示「這個 schema 由誰服務」：

| 行程 | 取鎖的方式 | 連線 |
|---|---|---|
| 沒有啟用叢集模式的 Kairos（`ServiceMode::Solo`） | **互斥** | writer 的專用連線 |
| 叢集模式的節點，含備援（`ServiceMode::Cluster`） | **共享** | 協調連線與 writer 的連線 |
| CLI | 不取 | — |

- 同一個 schema 上，單機模式的 Kairos 同時只能有一個，也不能與叢集的節點並存；叢集的節點彼此可以並存（由 leader 租約決定誰服務）。
- 取不到時拒絕啟動：`KRS-E4402`（「已有另一個 Kairos 正在使用」或「正被一個沒有啟用叢集模式的 Kairos 使用」），結束代碼 3，systemd 不重試。
- 鎖隨連線結束自動釋放（行程當掉也是），不會留下需要手動清除的狀態。
- PgBouncer 的 transaction pooling 會讓這個鎖失效，與第 19.1 節相同只支援 session pooling。

### 20.6 `kairos db migrate` 與備份

- `kairos db migrate` **一併複製**第 20.1 節的資料表（`store/postgres/schema.rs` 的 `TABLES`；彼此沒有相依，`cluster_events.id` 的 IDENTITY 以 `setval` 對齊）與 `meta` 的 `cluster_id`、`cluster_keys`。來源與目的都必須是目前的 schema 版本（v11）；較舊的來源先以相同版本的 Kairos 開啟一次完成升級。
- 搬移需要停止**所有**節點（取得「服務執行中」的互斥鎖，否則 `KRS-E4304`）。搬到 SQLite（回到單機）之後，`cluster_nodes` 與 `cluster_leases` 留著原本的列但不再使用；把 `cluster.enabled` 改回 `false` 後啟動。
- PostgreSQL 的備份（`pg_dump` 等）自然包含這些資料表。還原到另一組主機時，舊節點的列以 `kairos cluster forget` 刪除；`cluster_leases` 中過期的租約不需要處理（到期後任何節點都能取得）。金鑰另以 `kairos cluster keys export` 備份，還原時與資料庫要是同一個時間點的組合（第 9 節）。

### 20.7 連不上資料庫時

| 模式 | 啟動時連不上 | 執行中中斷 |
|---|---|---|
| 單機（第 19 節） | 拒絕啟動，`KRS-E4401`，結束代碼 3（systemd 不重試） | 不結束：report 回 `storage_unavailable`、`/readyz` 503，恢復後自動重新連線 |
| 叢集的備援節點 | **不結束**：持續重試，`/readyz` 的 `cluster` 為 `lost`，每分鐘記一次 `KRS-E4401` | 同左 |
| 叢集的主動節點 | （還沒有取得租約，同備援） | 超過 `lease_ttl_secs − 5` 秒沒有續約成功就以結束代碼 75 結束（`KRS-W8502`），重新啟動為備援後持續重試 |

- 所有 PostgreSQL 連線都設定了 TCP keepalive（間隔 10 秒、3 次）與 `tcp_user_timeout = 30 秒`（Linux）；協調連線的 `statement_timeout`／`lock_timeout` 是自我降級時限的一半（最多 5 秒）。另有一條只看單調時鐘的看門狗執行緒，協調執行緒卡在沒有回應的資料庫呼叫上時也會結束行程。
- 協調連線另以 `LISTEN kairos_cluster` 接收「租約已交還」的通知（正常關機時），備援立即嘗試取得；通知遺失只會讓延遲回到 2 秒的輪詢。
