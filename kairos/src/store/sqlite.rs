//! SQLite 後端（審計 A1、A6、A9）：`<data_dir>/kairos.db`。
//!
//! - 原始 report 每筆一列（`reports`），指標以 [`codec`](super::codec) 的緊湊格式存放，
//!   主機與序列（指標名稱＋單位＋label）各自只存一次（`hosts`、`series`）。
//! - 主機清冊（`hosts`）：連線狀態每 [`super::INVENTORY_SYNC_INTERVAL`] 從狀態表同步，
//!   啟動時載回（[`SqliteStore::open`] 回傳最近 24 小時出現過的主機，交給 `Registry::restore`）。
//! - 彙總（`rollup_5m`、`rollup_1h`，見 [`rollup`](super::rollup)）、保留期限與總容量上限的刪除，
//!   都由 [`SqliteStore::maintain`] 分批進行，每次一個短交易，不會長時間擋住 report 的寫入。
//!
//! 所有方法都是同步（blocking）的，只在 writer 的 blocking 執行緒上呼叫（見 [`super::run_store_writer`]）。
//! schema、設定與容量估算見 `kairos/STORAGE.md`。

use chrono::{DateTime, NaiveDate, Utc};
use log::{debug, info, warn};
use rusqlite::types::Value as SqlValue;
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::codec::{self, AgentTs, BYTES_PER_MB, FORMAT_LEGACY, FORMAT_METRICS, canonical_labels};
use super::rollup::{BUCKET_1H_SECS, Rollup, RollupRow, aggregate, bucket_start};
use super::{ReportStore, sample_time_ms};
use crate::config::{AppConfig, SqliteSynchronous};
use crate::fsperm;
use crate::protocol::{HostInfo, METRIC_CPU_USAGE, METRIC_MEMORY_TOTAL, METRIC_MEMORY_USED};
use crate::registry::{
    AgentSnapshot, LastReport, OFFLINE_RETENTION, RegistrySnapshot, RestoredAgent,
};
use crate::retention::log_guard_event;
use crate::storage::{Clock, RecordBody, ReportRecord, WriteStatus};
use crate::throttle::{Category, LogThrottle};
use crate::throttled;

/// 資料庫檔名（放在 `storage.data_dir`）
pub const DB_FILE: &str = "kairos.db";

/// `PRAGMA application_id`：「KRS1」，用來辨識這是 Kairos 的資料庫
pub const APPLICATION_ID: i32 = 0x4B52_5331;

/// 一次 schema 變更：`version` 從 1 起連續遞增，`sql` 把前一版升級到這一版
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: i32,
    pub sql: &'static str,
}

/// schema v1（`kairos/STORAGE.md`「schema」）
const SCHEMA_V1: &str = "
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value
) WITHOUT ROWID;

CREATE TABLE hosts (
    host_id            INTEGER PRIMARY KEY,
    agent_id           TEXT NOT NULL UNIQUE,
    hostname           TEXT NOT NULL,
    agent_version      TEXT,
    proto              INTEGER,
    host_fingerprint   TEXT,
    display_name       TEXT,
    tags               TEXT,
    os                 TEXT,
    report_interval_ms INTEGER,
    peer               TEXT,
    identity_verified  INTEGER NOT NULL DEFAULT 0,
    online             INTEGER NOT NULL DEFAULT 0,
    first_seen         INTEGER NOT NULL,
    connected_at       INTEGER,
    last_seen          INTEGER NOT NULL,
    offline_after_ms   INTEGER,
    last_report        TEXT,
    rollup_until       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX hosts_last_seen ON hosts (last_seen);

CREATE TABLE series (
    series_id INTEGER PRIMARY KEY,
    name      TEXT NOT NULL,
    unit      TEXT NOT NULL,
    labels    TEXT NOT NULL,
    UNIQUE (name, unit, labels)
);

CREATE TABLE boots (
    boot_ref INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id  INTEGER NOT NULL,
    boot_id  TEXT NOT NULL,
    UNIQUE (host_id, boot_id)
);

CREATE TABLE reports (
    id         INTEGER PRIMARY KEY,
    host_id    INTEGER NOT NULL,
    ts         INTEGER NOT NULL,
    seq        INTEGER NOT NULL,
    agent_ts,
    format     INTEGER NOT NULL,
    body       BLOB NOT NULL,
    boot_ref   INTEGER,
    report_seq INTEGER,
    recv_lag   INTEGER
);
CREATE INDEX reports_host_ts ON reports (host_id, ts);
CREATE UNIQUE INDEX reports_dedup ON reports (boot_ref, report_seq) WHERE boot_ref IS NOT NULL;

CREATE TABLE rollup_5m (
    host_id   INTEGER NOT NULL,
    bucket    INTEGER NOT NULL,
    series_id INTEGER NOT NULL,
    vmin      REAL NOT NULL,
    vmax      REAL NOT NULL,
    vsum      REAL NOT NULL,
    cnt       INTEGER NOT NULL,
    PRIMARY KEY (host_id, bucket, series_id)
) WITHOUT ROWID;

CREATE TABLE rollup_1h (
    host_id   INTEGER NOT NULL,
    bucket    INTEGER NOT NULL,
    series_id INTEGER NOT NULL,
    vmin      REAL NOT NULL,
    vmax      REAL NOT NULL,
    vsum      REAL NOT NULL,
    cnt       INTEGER NOT NULL,
    PRIMARY KEY (host_id, bucket, series_id)
) WITHOUT ROWID;
";

/// schema v2：`kairos reports import`（匯入 0.2.x 以前的 JSONL）的去重鍵，以及主機的來源
/// （`kairos/STORAGE.md`「從 JSONL 升級」）
const SCHEMA_V2: &str = "
ALTER TABLE hosts ADD COLUMN source TEXT;

CREATE TABLE import_keys (
    host_id INTEGER NOT NULL,
    ts      INTEGER NOT NULL,
    hash    INTEGER NOT NULL,
    PRIMARY KEY (host_id, ts, hash)
) WITHOUT ROWID;
";

/// schema v3（0.4）：主控台 API 的 token（`kairos/API.md` 第 4 節），以及握手時被拒的客戶端憑證
/// 歸屬到主機（設計包 §24 #3）
const SCHEMA_V3: &str = "
CREATE TABLE api_tokens (
    token_id      TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    secret_hash   BLOB NOT NULL,
    scope         TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    created_by    TEXT,
    expires_at    INTEGER,
    revoked_at    INTEGER,
    revoked_by    TEXT,
    revoke_reason TEXT,
    last_used_at  INTEGER,
    last_used_ip  TEXT,
    logged        INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

ALTER TABLE hosts ADD COLUMN cert_rejected_at INTEGER;
ALTER TABLE hosts ADD COLUMN cert_rejection TEXT;
";

/// schema v4（0.5）：註冊通道（8444）與 PKI（`kairos/PKI.md` 第 7 節、`crates/kairos-proto/ENROLL.md`）。
/// 與 PKI.md 的差異：`enroll_requests` 另有 `cert_lifetime_days`（核准時指定或 token 的有效天數）、
/// `issue_error`（最近一次簽發失敗的原因）與 `(source_ip, state)` 索引（動態 `poll_after_ms` 依同一來源的
/// 待核准筆數計算）；`hosts` 另有 `decommissioned_by`（除役來源）
const SCHEMA_V4: &str = "
CREATE TABLE pki_cas (
    ca_id        INTEGER PRIMARY KEY,
    subject      TEXT NOT NULL,
    spki_sha256  BLOB NOT NULL UNIQUE,
    cert_der     BLOB,
    not_before   INTEGER,
    not_after    INTEGER,
    key_file     TEXT,
    state        TEXT NOT NULL,
    next_crl_number INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL,
    retired_at   INTEGER
);

CREATE TABLE enroll_tokens (
    token_id       TEXT PRIMARY KEY,
    secret_hash    BLOB NOT NULL UNIQUE,
    hint           TEXT NOT NULL,
    name           TEXT NOT NULL,
    group_name     TEXT NOT NULL,
    tags           TEXT,
    approval       TEXT NOT NULL,
    max_uses       INTEGER NOT NULL,
    used           INTEGER NOT NULL DEFAULT 0,
    expires_at     INTEGER NOT NULL,
    allowed_cidrs  TEXT,
    batch_mode     INTEGER NOT NULL DEFAULT 0,
    allowed_agent_ids TEXT,
    cert_lifetime_days INTEGER,
    created_at     INTEGER NOT NULL,
    created_by     TEXT NOT NULL,
    disabled_at    INTEGER,
    disabled_by    TEXT,
    disable_reason TEXT,
    last_used_at   INTEGER
) WITHOUT ROWID;

CREATE TABLE enroll_requests (
    request_id     TEXT PRIMARY KEY,
    kind           TEXT NOT NULL,
    agent_id       TEXT NOT NULL,
    request_code   TEXT NOT NULL,
    state          TEXT NOT NULL,
    csr_der        BLOB NOT NULL,
    csr_sha256     BLOB NOT NULL,
    spki_sha256    BLOB NOT NULL,
    poll_secret_hash BLOB,
    token_id       TEXT,
    expired_serial TEXT,
    previous_serial TEXT,
    source_ip      TEXT NOT NULL,
    hostname       TEXT NOT NULL,
    display_name   TEXT,
    tags           TEXT,
    os             TEXT,
    host_fingerprint TEXT,
    pulse_version  TEXT,
    server_verification TEXT,
    flags          TEXT,
    cert_lifetime_days INTEGER,
    created_at     INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL,
    decided_at     INTEGER,
    decided_by     TEXT,
    reject_reason_code TEXT,
    reject_reason  TEXT,
    issued_serial  TEXT,
    issue_error    TEXT,
    delivered_at   INTEGER,
    withdrawn_at   INTEGER,
    withdraw_reason TEXT,
    withdraw_initiated_by TEXT,
    withdraw_ip    TEXT,
    closed_at      INTEGER
) WITHOUT ROWID;
CREATE UNIQUE INDEX enroll_requests_csr ON enroll_requests (agent_id, csr_sha256);
CREATE INDEX enroll_requests_state ON enroll_requests (state, created_at);
CREATE INDEX enroll_requests_agent ON enroll_requests (agent_id);
CREATE INDEX enroll_requests_code ON enroll_requests (request_code);
CREATE INDEX enroll_requests_token ON enroll_requests (token_id);
CREATE INDEX enroll_requests_source ON enroll_requests (source_ip, state);
CREATE INDEX enroll_requests_source_time ON enroll_requests (source_ip, created_at);

CREATE TABLE pki_certificates (
    serial         TEXT NOT NULL UNIQUE,
    ca_id          INTEGER,
    agent_id       TEXT NOT NULL,
    spki_sha256    BLOB NOT NULL,
    not_before     INTEGER NOT NULL,
    not_after      INTEGER NOT NULL,
    cert_der       BLOB NOT NULL,
    issued_at      INTEGER NOT NULL,
    issued_via     TEXT NOT NULL,
    request_id     TEXT,
    approved_by    TEXT,
    first_used_at  INTEGER,
    revoked_at     INTEGER,
    revoke_effective_at INTEGER,
    revocation_reason TEXT,
    revoked_by     TEXT,
    revoke_comment TEXT,
    crl_number     INTEGER
);
CREATE INDEX pki_certificates_agent ON pki_certificates (agent_id, issued_at);
CREATE INDEX pki_certificates_not_after ON pki_certificates (not_after);
CREATE INDEX pki_certificates_pending_crl ON pki_certificates (ca_id, revoke_effective_at)
    WHERE revoked_at IS NOT NULL;

CREATE TABLE pki_crls (
    ca_id          INTEGER NOT NULL,
    crl_number     INTEGER NOT NULL,
    this_update    INTEGER NOT NULL,
    next_update    INTEGER NOT NULL,
    revoked_count  INTEGER NOT NULL,
    der            BLOB NOT NULL,
    source         TEXT NOT NULL,
    created_by     TEXT,
    PRIMARY KEY (ca_id, crl_number)
) WITHOUT ROWID;

CREATE TABLE enroll_replay (
    key            BLOB PRIMARY KEY,
    expires_at     INTEGER NOT NULL,
    agent_id       TEXT NOT NULL,
    first_seen_at  INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX enroll_replay_expires ON enroll_replay (expires_at);

CREATE TABLE enroll_blocks (
    source         TEXT NOT NULL,
    class          TEXT NOT NULL,
    blocked_until  INTEGER NOT NULL,
    offenses       INTEGER NOT NULL,
    last_offense_at INTEGER NOT NULL,
    PRIMARY KEY (source, class)
) WITHOUT ROWID;

CREATE TABLE enroll_relaxations (
    id             INTEGER PRIMARY KEY,
    cidr           TEXT NOT NULL,
    expires_at     INTEGER NOT NULL,
    created_at     INTEGER NOT NULL,
    created_by     TEXT NOT NULL,
    reason         TEXT NOT NULL
);

CREATE TABLE settings (
    key            TEXT PRIMARY KEY,
    value          TEXT NOT NULL,
    updated_at     INTEGER NOT NULL,
    updated_by     TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE audit_events (
    id             INTEGER PRIMARY KEY,
    at             INTEGER NOT NULL,
    actor          TEXT NOT NULL,
    actor_ip       TEXT,
    category       TEXT NOT NULL,
    action         TEXT NOT NULL,
    object         TEXT,
    result         TEXT NOT NULL,
    code           TEXT,
    details        TEXT
);
CREATE INDEX audit_events_at ON audit_events (at);
CREATE INDEX audit_events_object ON audit_events (object);

ALTER TABLE hosts ADD COLUMN lifecycle TEXT NOT NULL DEFAULT 'active';
ALTER TABLE hosts ADD COLUMN decommissioned_at INTEGER;
ALTER TABLE hosts ADD COLUMN decommissioned_by TEXT;
ALTER TABLE hosts ADD COLUMN current_serial TEXT;
ALTER TABLE hosts ADD COLUMN grace_override_until INTEGER;
ALTER TABLE hosts ADD COLUMN expired_seen_at INTEGER;
ALTER TABLE hosts ADD COLUMN last_grace_attempt TEXT;
CREATE INDEX hosts_hostname ON hosts (hostname);
";

/// schema v6（0.6）：使用者、角色指派、工作階段、TOTP 復原碼（審計 F6；`kairos/API.md` 第 12 節）、
/// 稽核事件的雜湊鏈（`prev_hash`、`hash`，[`crate::audit`]）、API token 的擁有者與角色、主機群組
/// （角色指派的範圍）。
///
/// **與 v5 的關係**：v5 保留給告警（另一條開發線）。這一版只建立新的資料表，並在 v4 既有的資料表
/// （`api_tokens`、`audit_events`、`hosts`）加欄位，不依賴 v5 的任何物件：可以直接接在 v4 之後
/// （本分支），也可以接在 v5 之後（整合後）。`migrate` 依版本號套用，版本號不必連續。
///
/// 既有的 API token：`role`、`owner_user_id` 為 NULL，維持升級前的範圍（`scope`）與權限（[`crate::rbac::LegacyScope`]）。
/// 既有的稽核事件：沒有雜湊，第一次寫入或驗證時依 id 順序補上（[`crate::audit::seal_legacy`]）。
const SCHEMA_V6: &str = "
CREATE TABLE users (
    user_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    username       TEXT NOT NULL COLLATE NOCASE UNIQUE,
    display_name   TEXT,
    email          TEXT,
    source         TEXT NOT NULL DEFAULT 'local',
    ldap_dn        TEXT,
    password_hash  TEXT,
    password_changed_at INTEGER,
    must_change_password INTEGER NOT NULL DEFAULT 0,
    totp_secret    BLOB,
    totp_pending   BLOB,
    totp_enabled_at INTEGER,
    totp_last_step INTEGER,
    state          TEXT NOT NULL DEFAULT 'active',
    failed_logins  INTEGER NOT NULL DEFAULT 0,
    locked_until   INTEGER,
    last_login_at  INTEGER,
    last_login_ip  TEXT,
    created_at     INTEGER NOT NULL,
    created_by     TEXT NOT NULL,
    updated_at     INTEGER NOT NULL,
    disabled_at    INTEGER,
    disabled_by    TEXT,
    disable_reason TEXT
);

CREATE TABLE user_recovery_codes (
    user_id        INTEGER NOT NULL,
    code_hash      BLOB NOT NULL,
    used_at        INTEGER,
    PRIMARY KEY (user_id, code_hash)
) WITHOUT ROWID;

CREATE TABLE role_assignments (
    assignment_id  INTEGER PRIMARY KEY,
    subject_type   TEXT NOT NULL,
    subject        TEXT NOT NULL,
    role           TEXT NOT NULL,
    scope_type     TEXT NOT NULL DEFAULT 'global',
    scope_value    TEXT NOT NULL DEFAULT '',
    created_at     INTEGER NOT NULL,
    created_by     TEXT NOT NULL,
    UNIQUE (subject_type, subject, role, scope_type, scope_value)
);
CREATE INDEX role_assignments_subject ON role_assignments (subject_type, subject);

CREATE TABLE sessions (
    session_hash   BLOB PRIMARY KEY,
    public_id      TEXT NOT NULL UNIQUE,
    user_id        INTEGER NOT NULL,
    method         TEXT NOT NULL,
    ldap_groups    TEXT,
    created_at     INTEGER NOT NULL,
    last_seen_at   INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL,
    idle_timeout_ms INTEGER NOT NULL,
    ip             TEXT,
    user_agent     TEXT,
    ended_at       INTEGER,
    end_reason     TEXT,
    ended_by       TEXT
) WITHOUT ROWID;
CREATE INDEX sessions_user ON sessions (user_id, ended_at);

ALTER TABLE api_tokens ADD COLUMN owner_user_id INTEGER;
ALTER TABLE api_tokens ADD COLUMN role TEXT;

ALTER TABLE audit_events ADD COLUMN prev_hash TEXT;
ALTER TABLE audit_events ADD COLUMN hash TEXT;
CREATE INDEX audit_events_actor ON audit_events (actor, id);

ALTER TABLE hosts ADD COLUMN host_group TEXT;
CREATE INDEX hosts_group ON hosts (host_group);
";

/// 依序套用的 schema 變更；最後一項的版本即目前的 schema 版本
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: SCHEMA_V1,
    },
    Migration {
        version: 2,
        sql: SCHEMA_V2,
    },
    Migration {
        version: 3,
        sql: SCHEMA_V3,
    },
    Migration {
        version: 4,
        sql: SCHEMA_V4,
    },
    // v5：告警（另一條開發線，整合時插在這裡）
    Migration {
        version: 6,
        sql: SCHEMA_V6,
    },
];

/// 目前的 schema 版本（`PRAGMA user_version`）
pub const SCHEMA_VERSION: i32 = 6;

/// `meta` 的鍵：最近一次容量上限刪除（`KRS-W4202`）的事件（JSON，見 [`CapacityEvent`]）
pub const META_CAPACITY_EVENT: &str = "capacity_event";
/// `meta` 的鍵：最近一次保留清除（JSON，見 [`RetentionRun`]）
pub const META_RETENTION_LAST: &str = "retention_last";

/// 設計包 `copywriting.md` §3.3：容量已滿，提早刪除最舊的資料
pub const CODE_CAPACITY: &str = "KRS-W4202";

/// 容量上限刪除的事件（`meta.capacity_event`；主控台的系統健康，`kairos/API.md` 第 5.6 節）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CapacityEvent {
    /// 開始刪除的時間（Unix 毫秒）
    pub started_at: i64,
    /// 停止刪除的時間；仍在刪除時為 `None`
    pub ended_at: Option<i64>,
    /// 開始時（或結束時）的使用量
    pub used_bytes: u64,
    pub limit_bytes: u64,
    /// 刪除的列數：原始 report、5 分鐘彙總、1 小時彙總
    pub deleted_raw: u64,
    pub deleted_rollup_5m: u64,
    pub deleted_rollup_1h: u64,
    /// 已沒有東西可刪而停止（仍超過上限）
    #[serde(default)]
    pub exhausted: bool,
}

/// 最近一次保留清除（`meta.retention_last`）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetentionRun {
    /// 這一輪結束的時間（Unix 毫秒）
    pub finished_at: i64,
    pub deleted_raw: u64,
    pub deleted_rollup_5m: u64,
    pub deleted_rollup_1h: u64,
}

/// 寫入 `meta`（失敗只影響系統健康的顯示，不影響寫入）
fn put_meta(conn: &Connection, key: &str, value: &impl serde::Serialize) -> rusqlite::Result<()> {
    let text = serde_json::to_string(value).unwrap_or_default();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, text],
    )
    .map(|_| ())
}

/// `hosts.source`：由 `kairos reports import` 建立的主機（由連線建立的為 NULL）
pub const HOST_SOURCE_IMPORT: &str = "import";

/// 序列（指標名稱＋單位＋label 組合）的數量上限。未啟用 mTLS 時任何人都能送出任意 label，
/// 以此限制 `series` 表與記憶體快取；超過時含新序列的 report 回 `storage_unavailable`
pub const MAX_SERIES: usize = 1_000_000;

/// 每次維護步驟最多刪除的列數（每步一個交易）
const DELETE_BATCH_ROWS: i64 = 5_000;
/// 每次寫出彙總最多約這麼多列（只在主機邊界切開）
const ROLLUP_CHUNK_ROWS: usize = 5_000;
/// 背景 checkpoint 的間隔（寫入的連線不自動 checkpoint，見 [`Checkpointer`]）
const CHECKPOINT_INTERVAL: Duration = Duration::from_millis(500);
/// WAL 中的頁數超過這個值（約 16 MiB）時，由寫入的連線在兩批之間補做一次 checkpoint，
/// 讓下一個交易從 WAL 開頭重新寫（見 [`Checkpointer`]）
const WRITER_CHECKPOINT_FRAMES: i64 = 4096;
/// 背景 checkpoint 無法啟動時，WAL 檔超過這個大小就由寫入的連線 checkpoint
const WAL_FALLBACK_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;
/// 檢查總容量的間隔（超過上限、正在刪除時每一步都檢查）
const CAPACITY_CHECK_INTERVAL: Duration = Duration::from_secs(10);
/// 依保留期限刪除的間隔（啟動後立即執行一次）
const RETENTION_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// 超過容量上限後刪到上限的這個比例以下才停止，避免在上限附近每幾秒刪一次
const CAPACITY_LOW_WATERMARK_PERCENT: u64 = 95;
/// 主機清冊的 `last_seen` 最多這麼久寫入一次（其他欄位改變時立即寫入）
const LAST_SEEN_WRITE_INTERVAL_MS: i64 = 60_000;
/// 主機清冊同步每個交易最多寫入的列數：重新啟動後第一次同步約有主機數那麼多列（1 萬台時 1 萬列），
/// 分成多個短交易，在 report 的批次之間逐步寫入，不會一次擋住寫入（量測見 STORAGE.md「量測」）
pub(crate) const INVENTORY_CHUNK_ROWS: usize = 500;
/// 日期保護（DateGuard）狀態在 `meta` 表中的鍵
const META_RETENTION_GUARD: &str = "retention_guard";
/// 日誌中稱呼 SQLite 中的資料的名稱（DateGuard 的事件訊息）
const GUARD_LABEL: &str = " SQLite 資料";

const HOSTS_FULL: &str = "主機清冊已達上限（storage.max_hosts），不接受新主機的 report";
const SERIES_FULL: &str = "指標序列數已達上限，不接受含新序列的 report";

/// [`SqliteStore`] 的設定（取自 `[storage]`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteOptions {
    pub path: PathBuf,
    pub synchronous: SqliteSynchronous,
    pub raw_retention_days: u32,
    pub rollup_5m_retention_days: u32,
    pub rollup_1h_retention_days: u32,
    pub max_total_bytes: u64,
    pub max_hosts: usize,
    /// 伺服器是否啟用 mTLS（agent_id 與客戶端憑證綁定），寫入 `hosts.identity_verified`
    pub mtls: bool,
}

impl SqliteOptions {
    pub fn from_config(config: &AppConfig, data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(DB_FILE),
            synchronous: config.storage.sqlite_synchronous,
            raw_retention_days: config.storage.raw_retention_days,
            rollup_5m_retention_days: config.storage.rollup_5m_retention_days,
            rollup_1h_retention_days: config.storage.rollup_1h_retention_days,
            max_total_bytes: config.storage.max_total_bytes,
            max_hosts: config.max_hosts(),
            mtls: config.auth.client_ca_path().is_some(),
        }
    }
}

pub(crate) fn db_error(context: &str, e: rusqlite::Error) -> Error {
    let kind = match e.sqlite_error_code() {
        Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly) => ErrorKind::PermissionDenied,
        Some(ErrorCode::CannotOpen) => ErrorKind::NotFound,
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => ErrorKind::ResourceBusy,
        _ => ErrorKind::Other,
    };
    Error::new(kind, format!("{context}: {e}"))
}

/// `-wal`、`-shm` 檔的路徑
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// 檔案身分：Unix 為（裝置, inode），其他平台只看是否存在（Windows 上開著的資料庫無法被刪除）
pub(crate) fn file_identity(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    Some((0, 0))
}

/// 資料庫與 `-wal`、`-shm` 檔：先以 0640 建立資料庫檔（SQLite 建立 `-wal`、`-shm` 時沿用
/// 資料庫檔的權限），既有的檔案收緊到最多 0640（S3；Unix）。
///
/// 已存在的檔案**只以路徑**收緊權限，不以 `std::fs` 開啟：POSIX 的 fcntl 鎖以行程為單位，同一個
/// 行程內關閉任何一個指向資料庫檔（或 `-shm`）的 fd，都會放掉 SQLite 連線在這個檔上的鎖。writer 在
/// 資料庫被替換後重新開啟時，主控台的唯讀連線可能已經開著新檔；若在這裡開關它，`kairos reports import`
/// 就偵測不到 Kairos 正在執行（`kairos/API.md` 第 7 節）。
fn prepare_files(path: &Path) -> Result<(), Error> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        fsperm::create_dir_all(dir).map_err(|e| {
            Error::new(
                e.kind(),
                format!("無法建立資料目錄 {}: {}", dir.display(), e),
            )
        })?;
    }
    // 不存在時才建立（新檔不會有其他連線開著）；已存在（含建立時的競爭）時不開啟
    match fsperm::private_std_options(std::fs::OpenOptions::new().write(true).create_new(true))
        .open(path)
    {
        Ok(file) => {
            if let Err(e) = fsperm::restrict_open_file(&file) {
                warn!("無法將 {} 的權限收緊為 0640: {}", path.display(), e);
            }
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(Error::new(
                e.kind(),
                format!("無法開啟 {}: {}", path.display(), e),
            ));
        }
    }
    for p in [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ] {
        if let Err(e) = fsperm::restrict_path(&p) {
            warn!("無法將 {} 的權限收緊為 0640: {}", p.display(), e);
        }
    }
    Ok(())
}

/// `kairos reports import` 開啟資料庫：與 [`open_connection`] 相同（必要時建立、升級 schema），但以
/// `locking_mode = EXCLUSIVE` 取得資料庫檔的獨占鎖並保留到關閉。
///
/// WAL 模式下每條開啟中的連線都持有資料庫檔的 SHARED 鎖（SQLite 關閉 WAL 時就是以能否取得
/// EXCLUSIVE 鎖判斷自己是不是最後一條連線），因此 Kairos 執行中（writer 與背景 checkpoint 的連線）
/// 這裡會在 `busy` 之後回 [`ErrorKind::ResourceBusy`]；取得之後，匯入期間 Kairos 也無法啟動、
/// `hosts list` 等唯讀查詢會等待後失敗，不會有第二個寫入者。exclusive 模式下 WAL 不使用共享記憶體
/// （`-shm`），關閉時 checkpoint 並刪除 `-wal`。
pub(super) fn open_exclusive(opts: &SqliteOptions, busy: Duration) -> Result<Connection, Error> {
    prepare_files(&opts.path)?;
    let path = opts.path.display().to_string();
    let conn = Connection::open_with_flags(
        &opts.path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| db_error(&format!("無法開啟 {path}"), e))?;
    let ctx = |what: &str| format!("{path}：{what}");
    conn.busy_timeout(busy)
        .map_err(|e| db_error(&ctx("無法設定 busy_timeout"), e))?;
    // 必須在第一次讀取之前設定，WAL 才不使用共享記憶體、改以資料庫檔的 EXCLUSIVE 鎖
    conn.execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE; COMMIT;")
        .map_err(|e| match e.sqlite_error_code() {
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => Error::new(
                ErrorKind::ResourceBusy,
                ctx("資料庫正在使用中（Kairos 執行中，或另一個 kairos 子命令正在使用）"),
            ),
            _ => db_error(&ctx("無法取得資料庫的獨占鎖"), e),
        })?;
    finish_open(conn, opts, &path)
}

/// 管理用的讀寫連線（`kairos api-token`、主控台 API 的低頻寫入）：與 writer 相同的開啟方式
/// （必要時建立資料庫並升級 schema），busy 時最多等待 `busy`
pub(crate) fn open_admin(opts: &SqliteOptions, busy: Duration) -> Result<Connection, Error> {
    let conn = open_connection(opts)?;
    conn.busy_timeout(busy)
        .map_err(|e| db_error("無法設定 busy_timeout", e))?;
    Ok(conn)
}

/// 開啟（必要時建立）資料庫、設定 PRAGMA 並升級 schema
fn open_connection(opts: &SqliteOptions) -> Result<Connection, Error> {
    prepare_files(&opts.path)?;
    let path = opts.path.display().to_string();
    let conn = Connection::open_with_flags(
        &opts.path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| db_error(&format!("無法開啟 {path}"), e))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| db_error(&format!("{path}：無法設定 busy_timeout"), e))?;
    finish_open(conn, opts, &path)
}

/// [`open_connection`]、[`open_exclusive`] 共用：檢查 application_id、啟用 WAL、PRAGMA、升級 schema
fn finish_open(
    mut conn: Connection,
    opts: &SqliteOptions,
    path: &str,
) -> Result<Connection, Error> {
    let ctx = |what: &str| format!("{path}：{what}");
    check_application_id(&conn).map_err(|e| Error::new(e.kind(), format!("{path}：{e}")))?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .map_err(|e| db_error(&ctx("無法啟用 WAL"), e))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Error::other(ctx(&format!(
            "無法啟用 WAL（journal_mode 為 {mode}），資料目錄所在的檔案系統可能不支援共享記憶體（例如網路磁碟）"
        ))));
    }
    let synchronous = match opts.synchronous {
        SqliteSynchronous::Normal => "NORMAL",
        SqliteSynchronous::Full => "FULL",
    };
    // wal_autocheckpoint = 0：checkpoint（含 fsync）由背景執行緒做，不在 commit 時擋住寫入
    conn.execute_batch(&format!(
        "PRAGMA synchronous = {synchronous};
         PRAGMA cache_size = -65536;
         PRAGMA temp_store = MEMORY;
         PRAGMA journal_size_limit = 67108864;
         PRAGMA wal_autocheckpoint = 0;"
    ))
    .map_err(|e| db_error(&ctx("無法設定 PRAGMA"), e))?;
    migrate(&mut conn, MIGRATIONS).map_err(|e| Error::new(e.kind(), format!("{path}：{e}")))?;
    Ok(conn)
}

/// 不是空的資料庫、又不是 Kairos 建立的時候拒絕使用（例如 `storage.data_dir` 設錯）
fn check_application_id(conn: &Connection) -> Result<(), Error> {
    let id: i32 = conn
        .query_row("PRAGMA application_id", [], |r| r.get(0))
        .map_err(|e| db_error("無法讀取 application_id", e))?;
    if id == APPLICATION_ID {
        return Ok(());
    }
    let tables: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
        .map_err(|e| db_error("無法讀取 schema", e))?;
    if id == 0 && tables == 0 {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::InvalidData,
        format!("不是 Kairos 的資料庫（application_id = {id:#x}），請確認 storage.data_dir"),
    ))
}

/// 依 `PRAGMA user_version` 套用尚未套用的 schema 變更，每一版一個交易（失敗時整版不生效）。
/// 資料庫的版本比 `migrations` 新時（降版執行）回錯誤，不做任何變更。回傳升級後的版本
pub fn migrate(conn: &mut Connection, migrations: &[Migration]) -> Result<i32, Error> {
    let current: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| db_error("無法讀取 schema 版本", e))?;
    let latest = migrations.last().map_or(0, |m| m.version);
    if current > latest {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "資料庫的 schema 版本 {current} 比本版 Kairos 支援的 {latest} 新（由較新版本的 Kairos 建立）；請改用較新版本，或從升級前的備份還原"
            ),
        ));
    }
    for m in migrations.iter().filter(|m| m.version > current) {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| db_error("無法開始交易", e))?;
        tx.execute_batch(m.sql)
            .map_err(|e| db_error(&format!("schema 升級到 v{} 失敗", m.version), e))?;
        tx.pragma_update(None, "user_version", m.version)
            .and_then(|_| tx.pragma_update(None, "application_id", APPLICATION_ID))
            .and_then(|_| {
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_upgraded_by', ?1)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    [crate::VERSION],
                )
            })
            .map_err(|e| db_error(&format!("schema 升級到 v{} 失敗", m.version), e))?;
        tx.commit()
            .map_err(|e| db_error(&format!("schema 升級到 v{} 失敗", m.version), e))?;
        if current == 0 && m.version == migrations[0].version {
            info!("已建立資料庫 schema v{}", m.version);
        } else {
            info!("資料庫 schema 已升級到 v{}", m.version);
        }
    }
    Ok(latest.max(current))
}

/// 背景 checkpoint：另一條連線每 [`CHECKPOINT_INTERVAL`] 執行一次 `wal_checkpoint(PASSIVE)`，
/// 把 WAL 寫回資料庫檔（含 fsync）。PASSIVE 不等待、不擋住寫入；寫入的連線因此不必在 commit 時
/// 自動 checkpoint（在 kp-build 上，自動 checkpoint 讓 p99 的 ack 延遲多出數百毫秒）。
///
/// 但持續寫入時，背景 checkpoint 永遠追不上最新的一頁，WAL 不會從頭重新寫而一直變大；因此背景
/// 執行緒回報 WAL 的頁數（`frames`），超過 [`WRITER_CHECKPOINT_FRAMES`] 時由寫入的連線在兩批之間
/// 補做一次 PASSIVE checkpoint：大部分的頁已由背景寫回並同步，補做的只有最近的少數頁，停頓很短，
/// 完成後下一個交易從 WAL 開頭重新寫（WAL 檔再由 `journal_size_limit` 截到 64 MiB 以內）
struct Checkpointer {
    stop: Arc<(Mutex<bool>, Condvar)>,
    /// 最近一次背景 checkpoint 時 WAL 中的頁數
    frames: Arc<AtomicI64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Checkpointer {
    fn start(path: &Path) -> Option<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| {
            warn!(
                "無法開啟 {} 的背景 checkpoint 連線（改由寫入的連線處理）: {}",
                path.display(),
                e
            )
        })
        .ok()?;
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let flag = stop.clone();
        let frames = Arc::new(AtomicI64::new(0));
        let wal_frames = frames.clone();
        let handle = std::thread::Builder::new()
            .name("kairos-checkpoint".into())
            .spawn(move || {
                let (lock, cvar) = &*flag;
                let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
                while !*stopped {
                    let (guard, _) = cvar
                        .wait_timeout(stopped, CHECKPOINT_INTERVAL)
                        .unwrap_or_else(|e| e.into_inner());
                    stopped = guard;
                    if *stopped {
                        break;
                    }
                    // 回傳 (busy, WAL 頁數, 已寫回的頁數)；失敗只影響這一次
                    match conn
                        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get::<_, i64>(1))
                    {
                        Ok(n) => wal_frames.store(n, Ordering::Relaxed),
                        Err(e) => debug!("背景 checkpoint 失敗: {e}"),
                    }
                }
            })
            .map_err(|e| warn!("無法啟動背景 checkpoint 執行緒（改由寫入的連線處理）: {e}"))
            .ok()?;
        Some(Self {
            stop,
            frames,
            handle: Some(handle),
        })
    }
}

impl Drop for Checkpointer {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.stop;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cvar.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 主機在快取中的資料
#[derive(Debug, Clone)]
struct HostEntry {
    host_id: i64,
    /// 已寫出彙總的時間（Unix 毫秒）：重新啟動時從這裡重新讀取原始資料補回彙總
    rollup_until: i64,
    /// 上次同步到清冊的內容（雜湊、last_seen）；尚未同步時為 `None`
    synced: Option<(u64, i64)>,
}

/// 主機、序列與 boot 的快取。交易失敗時整個清空、下一次寫入前從資料庫重新載入
/// （失敗的交易中新建立的編號已不存在）
#[derive(Debug, Default)]
pub(super) struct Caches {
    loaded: bool,
    hosts: HashMap<String, HostEntry>,
    /// `name \x1f unit \x1f labels` → series_id
    series: HashMap<String, i64>,
    /// 每台主機最近一次使用的 boot：(boot_id, boot_ref)
    boots: HashMap<i64, (String, i64)>,
    /// v1 三欄位對應的標準序列：cpu、memory.total、memory.used
    legacy: Option<[i64; 3]>,
    key: String,
}

impl Caches {
    pub(super) fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut caches = Caches::default();
        let mut stmt = conn.prepare("SELECT agent_id, host_id, rollup_until FROM hosts")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                HostEntry {
                    host_id: r.get(1)?,
                    rollup_until: r.get(2)?,
                    synced: None,
                },
            ))
        })?;
        for row in rows {
            let (agent_id, entry) = row?;
            caches.hosts.insert(agent_id, entry);
        }
        let mut stmt = conn.prepare("SELECT series_id, name, unit, labels FROM series")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                series_key(
                    &r.get::<_, String>(1)?,
                    &r.get::<_, String>(2)?,
                    &r.get::<_, String>(3)?,
                ),
            ))
        })?;
        for row in rows {
            let (id, key) = row?;
            caches.series.insert(key, id);
        }
        caches.loaded = true;
        Ok(caches)
    }

    /// report 的主機編號；第一次出現的主機在清冊中新增一列。清冊已滿時為 `None`
    fn host_id(
        &mut self,
        tx: &Transaction,
        record: &ReportRecord,
        ts_ms: i64,
        opts: &SqliteOptions,
    ) -> rusqlite::Result<Option<i64>> {
        if let Some(e) = self.hosts.get(record.agent_id.as_str()) {
            return Ok(Some(e.host_id));
        }
        if self.hosts.len() >= opts.max_hosts {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO hosts (agent_id, hostname, peer, identity_verified, online, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5) ON CONFLICT (agent_id) DO NOTHING",
            params![record.agent_id, record.hostname, record.peer, opts.mtls, ts_ms],
        )?;
        let (host_id, rollup_until) = tx.query_row(
            "SELECT host_id, rollup_until FROM hosts WHERE agent_id = ?1",
            [&record.agent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        self.hosts.insert(
            record.agent_id.clone(),
            HostEntry {
                host_id,
                rollup_until,
                synced: None,
            },
        );
        Ok(Some(host_id))
    }

    /// 序列編號；第一次出現時新增。數量已達 [`MAX_SERIES`] 時為 `None`
    pub(super) fn series_id(
        &mut self,
        tx: &Transaction,
        name: &str,
        unit: &str,
        labels: &str,
    ) -> rusqlite::Result<Option<i64>> {
        self.key.clear();
        push_series_key(&mut self.key, name, unit, labels);
        if let Some(&id) = self.series.get(self.key.as_str()) {
            return Ok(Some(id));
        }
        if self.series.len() >= MAX_SERIES {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO series (name, unit, labels) VALUES (?1, ?2, ?3)
             ON CONFLICT (name, unit, labels) DO NOTHING",
            params![name, unit, labels],
        )?;
        let id: i64 = tx.query_row(
            "SELECT series_id FROM series WHERE name = ?1 AND unit = ?2 AND labels = ?3",
            params![name, unit, labels],
            |r| r.get(0),
        )?;
        self.series.insert(self.key.clone(), id);
        Ok(Some(id))
    }

    /// v1 的三個欄位在彙總中對應的標準序列（記憶體換算成 bytes，與 v2 的指標合併）
    pub(super) fn legacy_series(&mut self, tx: &Transaction) -> rusqlite::Result<Option<[i64; 3]>> {
        if let Some(ids) = self.legacy {
            return Ok(Some(ids));
        }
        let mut ids = [0; 3];
        for (slot, (name, unit)) in ids.iter_mut().zip([
            (METRIC_CPU_USAGE, "%"),
            (METRIC_MEMORY_TOTAL, "By"),
            (METRIC_MEMORY_USED, "By"),
        ]) {
            match self.series_id(tx, name, unit, "")? {
                Some(id) => *slot = id,
                None => return Ok(None),
            }
        }
        self.legacy = Some(ids);
        Ok(Some(ids))
    }

    /// 去重鍵的 boot 編號（每台主機每個 boot_id 一列）
    pub(super) fn boot_ref(
        &mut self,
        tx: &Transaction,
        host_id: i64,
        boot_id: &str,
    ) -> rusqlite::Result<i64> {
        if let Some((cached, boot_ref)) = self.boots.get(&host_id)
            && cached == boot_id
        {
            return Ok(*boot_ref);
        }
        tx.execute(
            "INSERT INTO boots (host_id, boot_id) VALUES (?1, ?2) ON CONFLICT (host_id, boot_id) DO NOTHING",
            params![host_id, boot_id],
        )?;
        let boot_ref: i64 = tx.query_row(
            "SELECT boot_ref FROM boots WHERE host_id = ?1 AND boot_id = ?2",
            params![host_id, boot_id],
            |r| r.get(0),
        )?;
        self.boots.insert(host_id, (boot_id.to_string(), boot_ref));
        Ok(boot_ref)
    }
}

fn push_series_key(key: &mut String, name: &str, unit: &str, labels: &str) {
    key.push_str(name);
    key.push('\u{1f}');
    key.push_str(unit);
    key.push('\u{1f}');
    key.push_str(labels);
}

fn series_key(name: &str, unit: &str, labels: &str) -> String {
    let mut key = String::with_capacity(name.len() + unit.len() + labels.len() + 2);
    push_series_key(&mut key, name, unit, labels);
    key
}

/// 一批寫入中、交易成功後才加入彙總的值：(host_id, series_id, ts_ms, 值)
type Sample = (i64, i64, i64, f64);

/// 寫入一批時的失敗：交易提交前（確定沒有寫入）或提交時（可能已寫入）
enum BatchError {
    BeforeCommit(rusqlite::Error),
    Commit(rusqlite::Error),
}

/// 依保留期限刪除的進度（每小時一輪，每一步一個交易）
#[derive(Debug)]
struct RetentionJob {
    stage: RetentionStage,
    hosts: Vec<i64>,
    cursor: usize,
    /// 刪除 `ts`（毫秒）早於此值的原始 report
    raw_before_ms: Option<i64>,
    /// 刪除 `bucket`（秒）早於此值的彙總
    rollup_5m_before: Option<i64>,
    rollup_1h_before: Option<i64>,
    deleted: [u64; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetentionStage {
    Raw,
    Rollup5m,
    Rollup1h,
    Boots,
}

/// 總容量上限的狀態
#[derive(Debug, Default)]
struct Capacity {
    /// 已超過上限、正在刪除
    active: bool,
    next_check: Option<Instant>,
    /// 這一輪刪除的列數：原始、5 分鐘、1 小時
    deleted: [u64; 3],
    /// 這一輪開始的時間（Unix 毫秒）
    started_at: i64,
}

/// 測試用：讓下一次寫入在提交前或提交時失敗（正式程式不會設定）
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailPoint {
    /// 插入後、提交前失敗（交易回復，確定未寫入）
    BeforeCommit,
    /// 提交成功，但回報 I/O 錯誤（模擬結果未知）
    CommitUnknown,
    /// 提交時磁碟已滿（交易回復，確定未寫入）
    CommitDiskFull,
}

/// SQLite 後端
pub struct SqliteStore {
    opts: SqliteOptions,
    clock: Clock,
    conn: Option<Connection>,
    file_id: Option<(u64, u64)>,
    caches: Caches,
    rollup: Rollup,
    guard: Option<date_guard::DateGuard>,
    retention: Option<RetentionJob>,
    next_retention: Option<Instant>,
    capacity: Capacity,
    checkpointer: Option<Checkpointer>,
    /// 背景 checkpoint 連線開啟的檔案（資料庫被換掉時重新開啟）
    checkpoint_file: Option<(u64, u64)>,
    /// 下一次檢查 WAL 大小的時間（背景 checkpoint 失效時的保險）
    next_wal_check: Option<Instant>,
    throttle: LogThrottle,
    buf: Vec<u8>,
    /// 提交結果不明之後：記憶體中的彙總可能少了已提交的那一批，下一次使用前改由原始資料重建
    rollup_stale: bool,
    /// 主機清冊中尚未寫入的列（最新一份快照中有變化的主機），每次維護步驟寫入一批
    pending_inventory: VecDeque<InventoryRow>,
    /// 測試用的失敗注入
    pub(crate) fail_next: Option<FailPoint>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("path", &self.opts.path)
            .field("open", &self.conn.is_some())
            .field("hosts", &self.caches.hosts.len())
            .field("rollup_pending", &self.rollup.len())
            .finish()
    }
}

impl SqliteStore {
    /// 開啟資料庫（必要時建立並升級 schema），回傳 store 與要放回狀態表的主機
    /// （最近 24 小時出現過的，見 `Registry::restore`）。失敗時 Kairos 不啟動。
    ///
    /// 也會：把上次異常結束時仍標為線上的主機改為離線、從原始資料補回尚未寫出的彙總、
    /// 讀回日期保護的狀態。
    pub fn open(opts: SqliteOptions, clock: Clock) -> Result<(Self, Vec<RestoredAgent>), Error> {
        let mut store = Self {
            opts,
            clock,
            conn: None,
            file_id: None,
            caches: Caches::default(),
            rollup: Rollup::default(),
            guard: None,
            retention: None,
            next_retention: None,
            capacity: Capacity::default(),
            checkpointer: None,
            checkpoint_file: None,
            next_wal_check: None,
            throttle: LogThrottle::default(),
            buf: Vec::with_capacity(4096),
            rollup_stale: false,
            pending_inventory: VecDeque::new(),
            fail_next: None,
        };
        store.ensure_ready().map_err(Error::other)?;
        let now = (store.clock)();
        let path = store.opts.path.display().to_string();
        let conn = store
            .conn
            .as_ref()
            .ok_or_else(|| Error::other("資料庫未開啟"))?;
        let stale = conn
            .execute("UPDATE hosts SET online = 0 WHERE online = 1", [])
            .map_err(|e| db_error(&format!("{path}：無法更新主機清冊"), e))?;
        if stale > 0 {
            info!(
                "主機清冊中有 {stale} 台主機在上次結束時仍標為線上（Kairos 未正常關機），改為離線"
            );
        }
        let since = now - chrono::Duration::from_std(OFFLINE_RETENTION).unwrap_or_default();
        let restored = load_restored(conn, since.timestamp_millis())
            .map_err(|e| db_error(&format!("{path}：無法讀取主機清冊"), e))?;
        let recovered = store
            .recover_rollup()
            .map_err(|e| db_error(&format!("{path}：無法從原始資料補回彙總"), e))?;
        store.guard = Some(store.load_guard(now));
        info!(
            "SQLite 資料庫 {}（schema v{}）：主機清冊 {} 台、指標序列 {} 個{}",
            path,
            SCHEMA_VERSION,
            store.caches.hosts.len(),
            store.caches.series.len(),
            if recovered > 0 {
                format!("；已從原始資料補回 {recovered} 筆 report 的彙總")
            } else {
                String::new()
            }
        );
        Ok((store, restored))
    }

    /// 確認連線可用：檔案被刪除或替換時重新開啟（建立新的資料庫，與 JSONL 的 `needs_reopen` 相同，
    /// 避免寫進已刪除的檔案卻回 `ack`），快取未載入時載入。失敗時回傳原因（這一批確定未寫入）
    fn ensure_ready(&mut self) -> Result<(), String> {
        if self.file_id.is_some() && file_identity(&self.opts.path) != self.file_id {
            if self.conn.is_some() {
                warn!(
                    "{} 已被刪除或替換（資料庫或資料目錄可能被刪除或換掉），重新開啟並建立新的資料庫",
                    self.opts.path.display()
                );
            }
            // 新的資料庫：記憶體中的彙總與快取的編號都不再有效
            self.conn = None;
            self.file_id = None;
            self.rollup.clear();
            self.caches = Caches::default();
        }
        if self.conn.is_none() {
            let conn = open_connection(&self.opts).map_err(|e| e.to_string())?;
            self.file_id = file_identity(&self.opts.path);
            self.conn = Some(conn);
            self.caches.loaded = false;
            // 新的資料庫檔（或第一次開啟）：背景 checkpoint 改用新的連線
            if self.checkpointer.is_none() || self.checkpoint_file != self.file_id {
                self.checkpointer = None;
                self.checkpointer = Checkpointer::start(&self.opts.path);
                self.checkpoint_file = self.file_id;
            }
        }
        if !self.caches.loaded {
            let conn = self.conn.as_ref().ok_or("資料庫未開啟")?;
            match Caches::load(conn) {
                Ok(c) => self.caches = c,
                Err(e) => {
                    self.conn = None;
                    return Err(format!(
                        "無法讀取 {} 的主機與序列: {}",
                        self.opts.path.display(),
                        e
                    ));
                }
            }
        }
        if self.rollup_stale {
            // 提交結果不明：那一批可能已寫入，記憶體中的彙總卻沒有它的值（重送時是 Duplicate，
            // 不會再加入）。以原始資料重建記憶體中的桶（與啟動時相同），之後才繼續寫入
            self.rollup.clear();
            match self.recover_rollup() {
                Ok(n) => {
                    self.rollup_stale = false;
                    info!("提交結果不明之後，已從原始資料重建尚未寫出的彙總（{n} 筆 report）");
                }
                Err(e) => {
                    self.invalidate(is_connection_fatal(&e));
                    return Err(format!(
                        "無法從 {} 的原始資料重建彙總: {}",
                        self.opts.path.display(),
                        e
                    ));
                }
            }
        }
        Ok(())
    }

    /// 交易失敗後：清空快取（失敗交易中新建立的編號已不存在），下一次寫入前重新載入；
    /// `reopen` 時另關閉連線
    fn invalidate(&mut self, reopen: bool) {
        self.caches = Caches::default();
        if reopen {
            self.conn = None;
        }
    }

    /// 在一個交易中寫入一批。取樣時間不早於主機 `rollup_until` 的數值放進 `samples`（提交後才加入
    /// 記憶體中的桶）；早於的（遲到資料：對應的桶已寫出）在同一個交易中直接合併進彙總表
    fn insert_batch(
        &mut self,
        records: &[ReportRecord],
        statuses: &mut [WriteStatus],
        samples: &mut Vec<Sample>,
    ) -> Result<(), BatchError> {
        let Self {
            conn,
            caches,
            opts,
            buf,
            fail_next,
            ..
        } = self;
        let conn = conn
            .as_mut()
            .ok_or(BatchError::BeforeCommit(rusqlite::Error::InvalidQuery))?;
        let before = BatchError::BeforeCommit;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(before)?;
        let mut late: Vec<Sample> = Vec::new();
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO reports (host_id, ts, seq, agent_ts, format, body, boot_ref, report_seq, recv_lag)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT DO NOTHING",
                )
                .map_err(before)?;
            let mut record_samples: Vec<Sample> = Vec::new();
            'records: for (i, r) in records.iter().enumerate() {
                let recv_ms = r.recv_at.timestamp_millis();
                // 取樣時間：durable report 為 client 的 ts（補送的歷史資料），否則為收到的時間
                let ts = sample_time_ms(r);
                let Some(host_id) = caches.host_id(&tx, r, recv_ms, opts).map_err(before)? else {
                    statuses[i] = WriteStatus::Failed(HOSTS_FULL.to_string());
                    continue;
                };
                record_samples.clear();
                buf.clear();
                let format = match &r.body {
                    RecordBody::Legacy {
                        cpu_usage,
                        total_memory,
                        used_memory,
                    } => {
                        let Some([cpu, total, used]) = caches.legacy_series(&tx).map_err(before)?
                        else {
                            statuses[i] = WriteStatus::Failed(SERIES_FULL.to_string());
                            continue;
                        };
                        codec::encode_legacy(buf, *cpu_usage, *total_memory, *used_memory);
                        record_samples.extend([
                            (host_id, cpu, ts, f64::from(*cpu_usage)),
                            (host_id, total, ts, (*total_memory * BYTES_PER_MB) as f64),
                            (host_id, used, ts, (*used_memory * BYTES_PER_MB) as f64),
                        ]);
                        FORMAT_LEGACY
                    }
                    RecordBody::Metrics { metrics } => {
                        let mut ids = Vec::with_capacity(metrics.len());
                        for m in metrics {
                            let labels = canonical_labels(&m.labels);
                            match caches
                                .series_id(&tx, &m.name, &m.unit, &labels)
                                .map_err(before)?
                            {
                                Some(id) => ids.push(id),
                                None => {
                                    statuses[i] = WriteStatus::Failed(SERIES_FULL.to_string());
                                    continue 'records;
                                }
                            }
                        }
                        codec::encode_metrics(
                            buf,
                            ids.iter().copied().zip(metrics.iter().map(|m| &m.value)),
                        );
                        record_samples.extend(
                            ids.iter()
                                .zip(metrics)
                                .map(|(&id, m)| (host_id, id, ts, m.value_f64())),
                        );
                        FORMAT_METRICS
                    }
                };
                let boot_ref = match &r.dedup {
                    Some(d) => Some(caches.boot_ref(&tx, host_id, &d.boot_id).map_err(before)?),
                    None => None,
                };
                let agent_ts = match AgentTs::parse(&r.ts) {
                    AgentTs::Millis(ms) => SqlValue::Integer(ms),
                    AgentTs::Text(t) => SqlValue::Text(t),
                };
                let changed = insert
                    .execute(params![
                        host_id,
                        ts,
                        r.seq as i64,
                        agent_ts,
                        format,
                        &buf[..],
                        boot_ref,
                        r.dedup.as_ref().map(|d| d.report_seq as i64),
                        (recv_ms != ts).then_some(recv_ms - ts),
                    ])
                    .map_err(before)?;
                if changed == 0 {
                    // 重複（已保存過）：不寫入、不加入彙總
                    statuses[i] = WriteStatus::Duplicate;
                    continue;
                }
                let rollup_until = caches
                    .hosts
                    .get(r.agent_id.as_str())
                    .map_or(0, |e| e.rollup_until);
                if ts < rollup_until {
                    late.extend_from_slice(&record_samples);
                } else {
                    samples.extend_from_slice(&record_samples);
                }
            }
        }
        if !late.is_empty() {
            merge_rollup_rows(&tx, &aggregate(late.drain(..))).map_err(before)?;
        }
        // 失敗注入（測試）：回傳錯誤時 tx 被釋放，交易回復
        let injected = |code| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("測試注入的失敗".to_string()),
            )
        };
        match fail_next.take() {
            None => {}
            Some(FailPoint::BeforeCommit) => {
                return Err(BatchError::BeforeCommit(injected(
                    rusqlite::ffi::SQLITE_IOERR,
                )));
            }
            Some(FailPoint::CommitDiskFull) => {
                return Err(BatchError::Commit(injected(rusqlite::ffi::SQLITE_FULL)));
            }
            Some(FailPoint::CommitUnknown) => {
                tx.commit().map_err(BatchError::Commit)?;
                return Err(BatchError::Commit(injected(rusqlite::ffi::SQLITE_IOERR)));
            }
        }
        tx.commit().map_err(BatchError::Commit)
    }

    /// 寫入一批（一個交易）。回傳每筆的結果
    fn write(&mut self, records: &[ReportRecord]) -> Vec<WriteStatus> {
        if records.is_empty() {
            return Vec::new();
        }
        if let Err(reason) = self.ensure_ready() {
            throttled!(
                self.throttle,
                Category::StorageWrite,
                error,
                "{}，{} 筆 report 未寫入",
                reason,
                records.len()
            );
            return vec![WriteStatus::Failed(reason); records.len()];
        }
        let mut statuses = vec![WriteStatus::Written; records.len()];
        let mut samples = Vec::new();
        let result = self.insert_batch(records, &mut statuses, &mut samples);
        match result {
            Ok(()) => {
                let now_ms = (self.clock)().timestamp_millis();
                for (host_id, series_id, ts, value) in samples {
                    self.rollup.add(host_id, series_id, ts, value, now_ms);
                }
                debug!(
                    "已寫入 {} 筆 report 至 {}",
                    statuses
                        .iter()
                        .filter(|s| **s == WriteStatus::Written)
                        .count(),
                    self.opts.path.display()
                );
                statuses
            }
            Err(BatchError::BeforeCommit(e)) => {
                // 交易已回復：確定沒有寫入
                let reason = format!("寫入 {} 失敗: {}", self.opts.path.display(), e);
                throttled!(
                    self.throttle,
                    Category::StorageWrite,
                    error,
                    "{}，本批 {} 筆 report 未寫入",
                    reason,
                    records.len()
                );
                self.invalidate(is_connection_fatal(&e));
                vec![WriteStatus::Failed(reason); records.len()]
            }
            Err(BatchError::Commit(e)) => {
                let reason = format!("寫入 {} 時提交失敗: {}", self.opts.path.display(), e);
                let definitely_not_written = matches!(
                    e.sqlite_error_code(),
                    Some(
                        ErrorCode::DiskFull
                            | ErrorCode::DatabaseBusy
                            | ErrorCode::DatabaseLocked
                            | ErrorCode::ReadOnly
                    )
                );
                if definitely_not_written {
                    throttled!(
                        self.throttle,
                        Category::StorageWrite,
                        error,
                        "{}，本批 {} 筆 report 未寫入",
                        reason,
                        records.len()
                    );
                    self.invalidate(false);
                    vec![WriteStatus::Failed(reason); records.len()]
                } else {
                    throttled!(
                        self.throttle,
                        Category::StorageWrite,
                        error,
                        "{}，本批 {} 筆 report 是否已保存無法確定（連線將不回應並關閉）",
                        reason,
                        records.len()
                    );
                    // 無法確定交易是否生效：重新開啟連線並重新載入快取，記憶體中的彙總改由原始資料重建
                    self.invalidate(true);
                    self.rollup_stale = true;
                    vec![WriteStatus::Unknown(reason); records.len()]
                }
            }
        }
    }

    /// 讀回日期保護的狀態；沒有時以最新一筆 report 的日期（不晚於今天）為基準
    fn load_guard(&self, now: DateTime<Utc>) -> date_guard::DateGuard {
        let today = now.date_naive();
        let Some(conn) = self.conn.as_ref() else {
            return date_guard::DateGuard::new(today, Instant::now(), now);
        };
        let saved: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [META_RETENTION_GUARD],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if let Some(saved) = saved {
            match date_guard::DateGuard::from_state(
                saved.as_bytes(),
                date_guard::LegacyState::Accept,
                Instant::now(),
                now,
            ) {
                Some(g) => return g,
                None => warn!("資料庫中的日期保護狀態無效，改以最新一筆 report 的日期為基準"),
            }
        }
        let newest: Option<i64> = conn
            .query_row(
                "SELECT ts + coalesce(recv_lag, 0) FROM reports ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        let newest = newest
            .and_then(DateTime::from_timestamp_millis)
            .map(|t| t.date_naive());
        date_guard::DateGuard::new(
            date_guard::guard_baseline(newest, today),
            Instant::now(),
            now,
        )
    }

    /// 從各主機的 `rollup_until` 起（取樣時間）重新讀取原始資料，補回記憶體中尚未寫出的彙總。
    /// 早於 `rollup_until` 的資料已在寫出的桶中（含遲到資料的直接合併），不再讀取。回傳讀取的筆數
    fn recover_rollup(&mut self) -> rusqlite::Result<u64> {
        let now_ms = (self.clock)().timestamp_millis();
        let Self {
            conn,
            caches,
            rollup,
            ..
        } = self;
        let Some(conn) = conn.as_mut() else {
            return Ok(0);
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut count = 0;
        {
            let mut stmt =
                tx.prepare("SELECT ts, format, body FROM reports WHERE host_id = ?1 AND ts >= ?2")?;
            let hosts: Vec<(i64, i64)> = caches
                .hosts
                .values()
                .map(|e| (e.host_id, e.rollup_until))
                .collect();
            for (host_id, until) in hosts {
                let rows = stmt.query_map(params![host_id, until], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                })?;
                for row in rows {
                    let (ts, format, body) = row?;
                    count += 1;
                    match format {
                        FORMAT_LEGACY => {
                            if let (Some((cpu, total, used)), Some([c, t, u])) =
                                (codec::decode_legacy(&body), caches.legacy_series(&tx)?)
                            {
                                rollup.add(host_id, c, ts, f64::from(cpu), now_ms);
                                rollup.add(host_id, t, ts, (total * BYTES_PER_MB) as f64, now_ms);
                                rollup.add(host_id, u, ts, (used * BYTES_PER_MB) as f64, now_ms);
                            }
                        }
                        FORMAT_METRICS => {
                            for (series_id, value) in
                                codec::decode_metrics(&body).unwrap_or_default()
                            {
                                rollup.add(
                                    host_id,
                                    series_id,
                                    ts,
                                    value.as_f64().unwrap_or(f64::NAN),
                                    now_ms,
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        tx.commit()?;
        Ok(count)
    }

    /// 寫出已結束的 5 分鐘桶（一個交易，最多約 [`ROLLUP_CHUNK_ROWS`] 列），同時合併進 1 小時彙總，
    /// 並推進這些主機的 `rollup_until`
    fn flush_rollup(&mut self, now_ms: i64) -> rusqlite::Result<usize> {
        let rows = self.rollup.take_ready(now_ms, ROLLUP_CHUNK_ROWS);
        if rows.is_empty() {
            return Ok(0);
        }
        let Some(conn) = self.conn.as_mut() else {
            self.rollup.put_back(rows);
            return Ok(0);
        };
        let mut until: HashMap<i64, i64> = HashMap::new();
        for row in &rows {
            let end = (row.bucket + super::rollup::BUCKET_5M_SECS) * 1000;
            let e = until.entry(row.host_id).or_insert(end);
            *e = (*e).max(end);
        }
        let result = (|| -> rusqlite::Result<()> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            merge_rollup_rows(&tx, &rows)?;
            {
                let mut upd = tx.prepare(
                    "UPDATE hosts SET rollup_until = max(rollup_until, ?2) WHERE host_id = ?1",
                )?;
                for (host_id, end) in &until {
                    upd.execute(params![host_id, end])?;
                }
            }
            tx.commit()
        })();
        match result {
            Ok(()) => {
                for entry in self.caches.hosts.values_mut() {
                    if let Some(end) = until.get(&entry.host_id) {
                        entry.rollup_until = entry.rollup_until.max(*end);
                    }
                }
                Ok(rows.len())
            }
            Err(e) => {
                self.rollup.put_back(rows);
                Err(e)
            }
        }
    }

    /// 資料庫使用中的大小：使用中的頁（不含可重複使用的空閒頁）× 頁大小。
    /// 不含 WAL：WAL 在 checkpoint 後由 `journal_size_limit` 截到 64 MiB 以內，且刪除資料本身也會
    /// 寫入 WAL，若計入會在上限較小時把資料全部刪光（見 STORAGE.md「總容量上限」）
    fn used_bytes(&self) -> rusqlite::Result<u64> {
        let Some(conn) = self.conn.as_ref() else {
            return Ok(0);
        };
        let pragma =
            |name: &str| conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0));
        let pages = pragma("page_count")? - pragma("freelist_count")?;
        Ok(u64::try_from(pages.max(0) * pragma("page_size")?).unwrap_or(0))
    }

    /// 總容量上限：超過時刪除一批最舊的資料。回傳是否還要繼續刪
    fn capacity_step(&mut self) -> rusqlite::Result<bool> {
        let cap = self.opts.max_total_bytes;
        if cap == 0 {
            return Ok(false);
        }
        let now = Instant::now();
        if !self.capacity.active && self.capacity.next_check.is_some_and(|t| now < t) {
            return Ok(false);
        }
        self.capacity.next_check = Some(now + CAPACITY_CHECK_INTERVAL);
        let used = self.used_bytes()?;
        if !self.capacity.active {
            if used <= cap {
                return Ok(false);
            }
            self.capacity.active = true;
            self.capacity.deleted = [0; 3];
            self.capacity.started_at = (self.clock)().timestamp_millis();
            // 設計包 copywriting.md §4.2 範例 68
            warn!(
                "警告 {CODE_CAPACITY}：資料庫已達容量上限，正在提早刪除最舊的資料：已使用 {} bytes，超過 storage.max_total_bytes（{} bytes）；依序刪除最舊的原始 report、5 分鐘彙總、1 小時彙總，直到低於上限的 {}%。新的回報照常保存，實際保留期會短於設定（處理方式：依 STORAGE.md「容量估算」調高 storage.max_total_bytes 或縮短保留天數後重新啟動 Kairos）",
                used, cap, CAPACITY_LOW_WATERMARK_PERCENT
            );
            self.save_capacity_event(used, None, false);
        } else if used <= cap / 100 * CAPACITY_LOW_WATERMARK_PERCENT {
            self.capacity.active = false;
            let [raw, m5, h1] = self.capacity.deleted;
            info!(
                "{CODE_CAPACITY} 結束：SQLite 資料庫已降到 {used} bytes（上限 {cap} bytes），停止刪除；這一輪刪除原始 report {raw} 筆、5 分鐘彙總 {m5} 列、1 小時彙總 {h1} 列"
            );
            self.save_capacity_event(used, Some((self.clock)().timestamp_millis()), false);
            return Ok(false);
        }
        let Some(conn) = self.conn.as_mut() else {
            return Ok(false);
        };
        let raw = conn.execute(
            "DELETE FROM reports WHERE id IN (SELECT id FROM reports ORDER BY id LIMIT ?1)",
            [DELETE_BATCH_ROWS],
        )?;
        if raw > 0 {
            self.capacity.deleted[0] += raw as u64;
            return Ok(true);
        }
        for (i, table) in [(1, "rollup_5m"), (2, "rollup_1h")] {
            let n = delete_oldest_rollup_day(conn, table)?;
            if n > 0 {
                self.capacity.deleted[i] += n;
                return Ok(true);
            }
        }
        self.capacity.active = false;
        throttled!(
            self.throttle,
            Category::StorageMaintenance,
            warn,
            "警告 {}：SQLite 資料庫使用 {} bytes，超過 storage.max_total_bytes（{} bytes），但已沒有原始 report 或彙總可刪除（主機清冊與序列不會自動刪除）；請調高上限",
            CODE_CAPACITY,
            used,
            cap
        );
        self.save_capacity_event(used, Some((self.clock)().timestamp_millis()), true);
        Ok(false)
    }

    /// 把這一輪容量上限刪除的狀態寫入 `meta.capacity_event`（系統健康的「最近一次容量刪除」）
    fn save_capacity_event(&mut self, used: u64, ended_at: Option<i64>, exhausted: bool) {
        let [raw, m5, h1] = self.capacity.deleted;
        let event = CapacityEvent {
            started_at: self.capacity.started_at,
            ended_at,
            used_bytes: used,
            limit_bytes: self.opts.max_total_bytes,
            deleted_raw: raw,
            deleted_rollup_5m: m5,
            deleted_rollup_1h: h1,
            exhausted,
        };
        if let Some(conn) = self.conn.as_ref()
            && let Err(e) = put_meta(conn, META_CAPACITY_EVENT, &event)
        {
            debug!("無法記錄容量刪除的事件: {e}");
        }
    }

    /// 開始新一輪的保留期限刪除；日期保護判斷時鐘可能被誤校時不刪除
    fn start_retention(&mut self, now: DateTime<Utc>) -> Option<RetentionJob> {
        let opts = &self.opts;
        if opts.raw_retention_days == 0
            && opts.rollup_5m_retention_days == 0
            && opts.rollup_1h_retention_days == 0
        {
            return None;
        }
        let guard = self.guard.as_mut()?;
        let decision = guard.decide(now.date_naive(), Instant::now(), now);
        for event in &decision.events {
            log_guard_event(GUARD_LABEL, event);
        }
        let state = guard.state(Instant::now(), now);
        if let (Some(conn), Ok(bytes)) = (self.conn.as_ref(), state.to_file_bytes())
            && let Err(e) = conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![META_RETENTION_GUARD, String::from_utf8_lossy(&bytes)],
            )
        {
            throttled!(
                self.throttle,
                Category::StorageMaintenance,
                warn,
                "無法保存日期保護的狀態（重新啟動後的時鐘跳躍保護可能失效）: {}",
                e
            );
        }
        let date = decision.cleanup_date?;
        let before = |days: u32| -> Option<i64> {
            if days == 0 {
                return None;
            }
            let keep_from: NaiveDate = date.checked_sub_days(chrono::Days::new(u64::from(days)))?;
            Some(keep_from.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
        };
        let mut hosts: Vec<i64> = self.caches.hosts.values().map(|e| e.host_id).collect();
        hosts.sort_unstable();
        Some(RetentionJob {
            stage: RetentionStage::Raw,
            hosts,
            cursor: 0,
            raw_before_ms: before(opts.raw_retention_days).map(|s| s * 1000),
            rollup_5m_before: before(opts.rollup_5m_retention_days),
            rollup_1h_before: before(opts.rollup_1h_retention_days),
            deleted: [0; 4],
        })
    }

    /// 保留期限：每小時一輪，每一步刪除最多 [`DELETE_BATCH_ROWS`] 列。回傳這一輪是否還沒做完
    fn retention_step(&mut self, now: DateTime<Utc>) -> rusqlite::Result<bool> {
        if self.retention.is_none() {
            let due = self.next_retention.is_none_or(|t| Instant::now() >= t);
            if !due {
                return Ok(false);
            }
            self.next_retention = Some(Instant::now() + RETENTION_INTERVAL);
            self.retention = self.start_retention(now);
            if self.retention.is_none() {
                return Ok(false);
            }
        }
        let Some(conn) = self.conn.as_mut() else {
            return Ok(false);
        };
        let Some(job) = self.retention.as_mut() else {
            return Ok(false);
        };
        let mut budget = DELETE_BATCH_ROWS;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        while budget > 0 {
            let (sql, before) = match job.stage {
                RetentionStage::Raw => (
                    "DELETE FROM reports WHERE id IN
                     (SELECT id FROM reports WHERE host_id = ?1 AND ts < ?2 LIMIT ?3)",
                    job.raw_before_ms,
                ),
                RetentionStage::Rollup5m => (
                    "DELETE FROM rollup_5m WHERE (host_id, bucket, series_id) IN
                     (SELECT host_id, bucket, series_id FROM rollup_5m WHERE host_id = ?1 AND bucket < ?2 LIMIT ?3)",
                    job.rollup_5m_before,
                ),
                RetentionStage::Rollup1h => (
                    "DELETE FROM rollup_1h WHERE (host_id, bucket, series_id) IN
                     (SELECT host_id, bucket, series_id FROM rollup_1h WHERE host_id = ?1 AND bucket < ?2 LIMIT ?3)",
                    job.rollup_1h_before,
                ),
                RetentionStage::Boots => {
                    // 已沒有任何 report 引用的 boot（去重鍵的 boot_id 對照）
                    let n = tx.execute(
                        "DELETE FROM boots WHERE boot_ref IN
                         (SELECT boot_ref FROM boots b WHERE NOT EXISTS
                            (SELECT 1 FROM reports r WHERE r.boot_ref = b.boot_ref) LIMIT ?1)",
                        [budget],
                    )? as i64;
                    job.deleted[3] += n as u64;
                    if n < budget {
                        break;
                    }
                    budget -= n;
                    continue;
                }
            };
            let next_stage = |s| match s {
                RetentionStage::Raw => RetentionStage::Rollup5m,
                RetentionStage::Rollup5m => RetentionStage::Rollup1h,
                _ => RetentionStage::Boots,
            };
            let (Some(before), Some(&host_id)) = (before, job.hosts.get(job.cursor)) else {
                job.stage = next_stage(job.stage);
                job.cursor = 0;
                continue;
            };
            let limit = budget;
            let n = tx.execute(sql, params![host_id, before, limit])? as i64;
            let slot = match job.stage {
                RetentionStage::Raw => 0,
                RetentionStage::Rollup5m => 1,
                _ => 2,
            };
            job.deleted[slot] += n as u64;
            budget -= n;
            if n < limit {
                // 這台主機已刪完；刪滿額度時下一步從同一台繼續
                job.cursor += 1;
            }
        }
        tx.commit()?;
        let done = job.stage == RetentionStage::Boots && budget > 0;
        if done {
            let [raw, m5, h1, boots] = job.deleted;
            if raw + m5 + h1 + boots > 0 {
                info!(
                    "已依保留期限刪除 SQLite 中過期的資料：原始 report {raw} 筆、5 分鐘彙總 {m5} 列、1 小時彙總 {h1} 列（storage.raw_retention_days、rollup_5m_retention_days、rollup_1h_retention_days）"
                );
            }
            let run = RetentionRun {
                finished_at: now.timestamp_millis(),
                deleted_raw: raw,
                deleted_rollup_5m: m5,
                deleted_rollup_1h: h1,
            };
            if let Err(e) = put_meta(conn, META_RETENTION_LAST, &run) {
                debug!("無法記錄保留清除的結果: {e}");
            }
            self.retention = None;
            // boot 對照表可能刪除了快取中的項目
            self.caches.boots.clear();
        }
        Ok(!done)
    }

    /// WAL 頁數超過 [`WRITER_CHECKPOINT_FRAMES`] 時由寫入的連線補做 checkpoint（見 [`Checkpointer`]）；
    /// 背景 checkpoint 無法啟動時，改為每秒檢查 WAL 檔的大小
    fn writer_checkpoint(&mut self) {
        let due = match &self.checkpointer {
            Some(c) => c.frames.swap(0, Ordering::Relaxed) > WRITER_CHECKPOINT_FRAMES,
            None => {
                let now = Instant::now();
                if self.next_wal_check.is_some_and(|t| now < t) {
                    return;
                }
                self.next_wal_check = Some(now + Duration::from_secs(1));
                std::fs::metadata(sidecar(&self.opts.path, "-wal"))
                    .is_ok_and(|m| m.len() > WAL_FALLBACK_CHECKPOINT_BYTES)
            }
        };
        if due
            && let Some(conn) = self.conn.as_ref()
            && let Err(e) =
                conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get::<_, i64>(0))
        {
            debug!("checkpoint 失敗: {e}");
        }
    }

    /// 一步維護工作：主機清冊的下一批 → 寫出彙總 → 容量上限 → 保留期限。回傳是否還有工作
    /// （呼叫端稍後再呼叫）
    fn maintain_step(&mut self) -> rusqlite::Result<bool> {
        self.writer_checkpoint();
        if !self.pending_inventory.is_empty() {
            self.sync_chunk()?;
            return Ok(true);
        }
        let now = (self.clock)();
        // 最舊的桶已結束但仍在補送（或所屬主機較早的桶還在補）時，這一步可能沒有可寫出的列，
        // 照常繼續做容量上限與保留期限
        if self.rollup.has_ready(now.timestamp_millis())
            && self.flush_rollup(now.timestamp_millis())? > 0
        {
            return Ok(true);
        }
        if self.capacity_step()? {
            return Ok(true);
        }
        self.retention_step(now)
    }

    /// 主機清冊同步：狀態表中有變化的主機寫入 `hosts`。`last_seen` 與 `last_report` 只在 `last_seen`
    /// 相差 60 秒以上時才寫（線上的主機每分鐘最多寫一次），其他欄位（上線、離線、hello 的資訊）
    /// 改變時立即寫入。
    ///
    /// 有變化的主機放進待寫入的佇列（取代上一份快照尚未寫完的部分：新的快照包含最新的狀態），
    /// 這裡先寫入一批（[`INVENTORY_CHUNK_ROWS`] 列、一個交易），其餘由之後的維護步驟逐批寫入。
    /// 回傳這次寫入的列數
    fn sync(&mut self, snapshot: &RegistrySnapshot) -> rusqlite::Result<usize> {
        let mut rows = VecDeque::new();
        let mut new_hosts = 0;
        for agent in &snapshot.agents {
            let row = InventoryRow::of(agent, self.opts.mtls);
            let entry = self.caches.hosts.get(agent.agent_id.as_str());
            let unchanged = entry.and_then(|e| e.synced).is_some_and(|(hash, seen)| {
                hash == row.hash && row.last_seen - seen < LAST_SEEN_WRITE_INTERVAL_MS
            });
            if unchanged {
                continue;
            }
            if entry.is_none() && self.caches.hosts.len() + new_hosts >= self.opts.max_hosts {
                throttled!(
                    self.throttle,
                    Category::StorageMaintenance,
                    warn,
                    "主機清冊已達上限 {}（storage.max_hosts），agent {} 不寫入清冊",
                    self.opts.max_hosts,
                    agent.agent_id
                );
                continue;
            }
            if entry.is_none() {
                new_hosts += 1;
            }
            rows.push_back(row);
        }
        self.pending_inventory = rows;
        self.sync_chunk()
    }

    /// 寫入待寫入清冊的下一批（一個交易）；失敗時放回，下一次再試。回傳寫入的列數
    fn sync_chunk(&mut self) -> rusqlite::Result<usize> {
        if self.pending_inventory.is_empty() {
            return Ok(0);
        }
        let n = self.pending_inventory.len().min(INVENTORY_CHUNK_ROWS);
        let rows: Vec<InventoryRow> = self.pending_inventory.drain(..n).collect();
        match self.write_inventory(&rows) {
            Ok(()) => Ok(rows.len()),
            Err(e) => {
                for row in rows.into_iter().rev() {
                    self.pending_inventory.push_front(row);
                }
                Err(e)
            }
        }
    }

    /// 在一個交易中把 `rows` 寫入清冊，成功後更新快取
    fn write_inventory(&mut self, rows: &[InventoryRow]) -> rusqlite::Result<()> {
        let Some(conn) = self.conn.as_mut() else {
            return Ok(());
        };
        let mut written = Vec::with_capacity(rows.len());
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO hosts (agent_id, hostname, agent_version, proto, host_fingerprint,
                    display_name, tags, os, report_interval_ms, peer, identity_verified, online,
                    first_seen, connected_at, last_seen, offline_after_ms, last_report)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                 ON CONFLICT (agent_id) DO UPDATE SET
                    hostname = excluded.hostname,
                    agent_version = excluded.agent_version,
                    proto = excluded.proto,
                    host_fingerprint = coalesce(excluded.host_fingerprint, hosts.host_fingerprint),
                    display_name = coalesce(excluded.display_name, hosts.display_name),
                    tags = coalesce(excluded.tags, hosts.tags),
                    os = coalesce(excluded.os, hosts.os),
                    report_interval_ms = coalesce(excluded.report_interval_ms, hosts.report_interval_ms),
                    peer = excluded.peer,
                    identity_verified = excluded.identity_verified,
                    online = excluded.online,
                    connected_at = excluded.connected_at,
                    last_seen = max(hosts.last_seen, excluded.last_seen),
                    offline_after_ms = excluded.offline_after_ms,
                    last_report = coalesce(excluded.last_report, hosts.last_report)
                 RETURNING host_id, rollup_until",
            )?;
            for row in rows {
                let (host_id, rollup_until): (i64, i64) = stmt.query_row(
                    params![
                        row.agent_id,
                        row.hostname,
                        row.agent_version,
                        row.proto,
                        row.host_fingerprint,
                        row.display_name,
                        row.tags,
                        row.os,
                        row.report_interval_ms,
                        row.peer,
                        row.identity_verified,
                        row.online,
                        row.connected_at.unwrap_or(row.last_seen),
                        row.connected_at,
                        row.last_seen,
                        row.offline_after_ms,
                        row.last_report,
                    ],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                written.push((host_id, rollup_until));
            }
        }
        tx.commit()?;
        for (row, (host_id, rollup_until)) in rows.iter().zip(written) {
            let entry = self
                .caches
                .hosts
                .entry(row.agent_id.clone())
                .or_insert(HostEntry {
                    host_id,
                    rollup_until,
                    synced: None,
                });
            entry.synced = Some((row.hash, row.last_seen));
        }
        Ok(())
    }
}

/// 把 5 分鐘桶的列合併進 `rollup_5m` 與 `rollup_1h`（已存在的列累加 min／max／sum／count，
/// 不覆蓋）。寫出記憶體中的桶、以及遲到資料直接合併進已寫出的桶時共用
pub(super) fn merge_rollup_rows(tx: &Transaction, rows: &[RollupRow]) -> rusqlite::Result<()> {
    merge_rollup_rows_from(tx, rows, None)
}

/// [`merge_rollup_rows`]，但 `bucket`（秒）早於 `rollup_5m_from` 的列只合併進 `rollup_1h`
/// （`kairos reports import` 匯入超過 5 分鐘彙總保留期限的資料時，不寫入馬上就會被刪除的 5 分鐘桶）
pub(super) fn merge_rollup_rows_from(
    tx: &Transaction,
    rows: &[RollupRow],
    rollup_5m_from: Option<i64>,
) -> rusqlite::Result<()> {
    let merge = |table: &str| {
        format!(
            "INSERT INTO {table} (host_id, bucket, series_id, vmin, vmax, vsum, cnt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (host_id, bucket, series_id) DO UPDATE SET
               vmin = min(vmin, excluded.vmin), vmax = max(vmax, excluded.vmax),
               vsum = vsum + excluded.vsum, cnt = cnt + excluded.cnt"
        )
    };
    let mut m5 = tx.prepare(&merge("rollup_5m"))?;
    let mut h1 = tx.prepare(&merge("rollup_1h"))?;
    for row in rows {
        let a = &row.agg;
        let count = i64::try_from(a.count).unwrap_or(i64::MAX);
        if rollup_5m_from.is_none_or(|from| row.bucket >= from) {
            m5.execute(params![
                row.host_id,
                row.bucket,
                row.series_id,
                a.min,
                a.max,
                a.sum,
                count
            ])?;
        }
        let hour = bucket_start(row.bucket * 1000, BUCKET_1H_SECS);
        h1.execute(params![
            row.host_id,
            hour,
            row.series_id,
            a.min,
            a.max,
            a.sum,
            count
        ])?;
    }
    Ok(())
}

/// 交易中的錯誤是否表示連線本身已不可用（需要重新開啟）
fn is_connection_fatal(e: &rusqlite::Error) -> bool {
    !matches!(
        e.sqlite_error_code(),
        Some(
            ErrorCode::DiskFull
                | ErrorCode::DatabaseBusy
                | ErrorCode::DatabaseLocked
                | ErrorCode::ConstraintViolation
                | ErrorCode::TooBig
        )
    )
}

/// 刪除某個彙總表中最舊一天的資料（每台主機各自以主鍵範圍刪除），回傳刪除的列數
fn delete_oldest_rollup_day(conn: &mut Connection, table: &str) -> rusqlite::Result<u64> {
    let oldest: Option<i64> = conn.query_row(
        &format!(
            "SELECT min(b) FROM (SELECT (SELECT min(bucket) FROM {table} WHERE host_id = h.host_id) AS b FROM hosts h)"
        ),
        [],
        |r| r.get(0),
    )?;
    let Some(oldest) = oldest else {
        return Ok(0);
    };
    let before = bucket_start(oldest * 1000, 86_400) + 86_400;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let n = tx.execute(
        &format!(
            "DELETE FROM {table} WHERE (host_id, bucket, series_id) IN
             (SELECT host_id, bucket, series_id FROM {table} WHERE host_id IN (SELECT host_id FROM hosts) AND bucket < ?1 LIMIT ?2)"
        ),
        params![before, DELETE_BATCH_ROWS],
    )?;
    tx.commit()?;
    Ok(n as u64)
}

/// 要寫入清冊的一列（由狀態表的快照轉換）
#[derive(Debug)]
struct InventoryRow {
    agent_id: String,
    hostname: String,
    agent_version: String,
    proto: u32,
    host_fingerprint: Option<String>,
    display_name: Option<String>,
    tags: Option<String>,
    os: Option<String>,
    report_interval_ms: Option<i64>,
    peer: String,
    identity_verified: bool,
    online: bool,
    connected_at: Option<i64>,
    last_seen: i64,
    offline_after_ms: i64,
    last_report: Option<String>,
    /// 除了 last_seen 與 last_report（隨每次 heartbeat、report 改變）之外所有欄位的雜湊
    hash: u64,
}

fn parse_ms(t: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(t)
        .ok()
        .map(|t| t.timestamp_millis())
}

impl InventoryRow {
    fn of(a: &AgentSnapshot, mtls: bool) -> Self {
        let host = a.host.as_ref();
        let json = |v: Option<String>| v.filter(|s| !s.is_empty() && s != "null");
        let mut row = Self {
            agent_id: a.agent_id.clone(),
            hostname: a.hostname.clone(),
            agent_version: a.agent_version.clone(),
            proto: a.proto,
            host_fingerprint: host.and_then(|h| h.host_fingerprint.clone()),
            display_name: host.and_then(|h| h.display_name.clone()),
            tags: json(
                host.and_then(|h| h.tags.as_ref())
                    .and_then(|t| serde_json::to_string(t).ok()),
            ),
            os: json(
                host.and_then(|h| h.os.as_ref())
                    .and_then(|o| serde_json::to_string(o).ok()),
            ),
            report_interval_ms: host
                .and_then(|h| h.report_interval_ms)
                .and_then(|v| i64::try_from(v).ok()),
            peer: a.peer.clone(),
            identity_verified: mtls,
            online: a.online,
            connected_at: parse_ms(&a.connected_at),
            last_seen: parse_ms(&a.last_seen).unwrap_or(0),
            offline_after_ms: i64::try_from(a.offline_after_ms).unwrap_or(i64::MAX),
            last_report: a
                .last_report
                .as_ref()
                .and_then(|r| serde_json::to_string(r).ok()),
            hash: 0,
        };
        let mut h = DefaultHasher::new();
        (
            &row.hostname,
            &row.agent_version,
            row.proto,
            &row.host_fingerprint,
            &row.display_name,
            &row.tags,
            &row.os,
            row.report_interval_ms,
            &row.peer,
            row.identity_verified,
            row.online,
            row.connected_at,
        )
            .hash(&mut h);
        row.offline_after_ms.hash(&mut h);
        row.hash = h.finish();
        row
    }
}

/// 清冊中 `last_seen >= since_ms` 的主機，轉成狀態表的格式
fn load_restored(conn: &Connection, since_ms: i64) -> rusqlite::Result<Vec<RestoredAgent>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, hostname, agent_version, proto, host_fingerprint, display_name, tags, os,
                report_interval_ms, peer, connected_at, last_seen, offline_after_ms, last_report
         FROM hosts WHERE last_seen >= ?1",
    )?;
    let rows = stmt.query_map([since_ms], |r| {
        let mut host = serde_json::Map::new();
        let mut put = |key: &str, v: Option<serde_json::Value>| {
            if let Some(v) = v {
                host.insert(key.to_string(), v);
            }
        };
        put(
            "host_fingerprint",
            r.get::<_, Option<String>>(4)?.map(Into::into),
        );
        put(
            "display_name",
            r.get::<_, Option<String>>(5)?.map(Into::into),
        );
        let parse = |s: Option<String>| s.and_then(|s| serde_json::from_str(&s).ok());
        put("tags", parse(r.get(6)?));
        put("os", parse(r.get(7)?));
        put(
            "report_interval_ms",
            r.get::<_, Option<i64>>(8)?.map(Into::into),
        );
        let host: Option<HostInfo> = (!host.is_empty())
            .then(|| serde_json::from_value(serde_json::Value::Object(host)).ok())
            .flatten();
        let time = |ms: Option<i64>| ms.and_then(DateTime::from_timestamp_millis);
        let last_seen = time(r.get(11)?).unwrap_or_default();
        Ok(RestoredAgent {
            agent_id: r.get(0)?,
            hostname: r.get(1)?,
            agent_version: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            proto: r
                .get::<_, Option<i64>>(3)?
                .and_then(|p| u32::try_from(p).ok())
                .unwrap_or(1),
            host,
            peer: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
            connected_at: time(r.get(10)?).unwrap_or(last_seen),
            last_seen,
            offline_after: Duration::from_millis(
                r.get::<_, Option<i64>>(12)?
                    .and_then(|v| u64::try_from(v).ok())
                    .unwrap_or(kairos_proto::MIN_OFFLINE_AFTER_MS),
            ),
            last_report: r
                .get::<_, Option<String>>(13)?
                .and_then(|s| serde_json::from_str::<LastReport>(&s).ok()),
        })
    })?;
    rows.collect()
}

impl ReportStore for SqliteStore {
    fn write_batch(&mut self, records: &[ReportRecord]) -> Vec<WriteStatus> {
        self.write(records)
    }

    fn sync_inventory(&mut self, snapshot: &RegistrySnapshot) {
        if let Err(reason) = self.ensure_ready() {
            throttled!(
                self.throttle,
                Category::StorageMaintenance,
                warn,
                "{}，主機清冊未同步",
                reason
            );
            return;
        }
        match self.sync(snapshot) {
            Ok(n) => debug!(
                "主機清冊已同步 {} 台主機（尚有 {} 台在之後的維護步驟寫入）",
                n,
                self.pending_inventory.len()
            ),
            Err(e) => {
                throttled!(
                    self.throttle,
                    Category::StorageMaintenance,
                    warn,
                    "主機清冊同步失敗（下次同步時重試）: {}",
                    e
                );
                self.invalidate(is_connection_fatal(&e));
            }
        }
    }

    fn maintain(&mut self) -> bool {
        if self.ensure_ready().is_err() {
            return false;
        }
        match self.maintain_step() {
            Ok(more) => more,
            Err(e) => {
                throttled!(
                    self.throttle,
                    Category::StorageMaintenance,
                    warn,
                    "SQLite 維護工作（彙總寫出、保留期限或容量上限的刪除）失敗，稍後重試: {}",
                    e
                );
                self.invalidate(is_connection_fatal(&e));
                false
            }
        }
    }

    fn close(&mut self) {
        // 清冊中尚未寫入的列全部寫完（關機時的快照：所有主機已離線）
        while !self.pending_inventory.is_empty() {
            if let Err(e) = self.sync_chunk() {
                warn!(
                    "關機前寫入主機清冊失敗，{} 台主機的最新狀態未寫入: {}",
                    self.pending_inventory.len(),
                    e
                );
                break;
            }
        }
        // 剩下的彙總留在原始資料中，下次啟動時補回；關閉連線讓 SQLite 完成 checkpoint 並刪除 -wal、-shm
        self.checkpointer = None;
        if let Some(conn) = self.conn.take()
            && let Err((_, e)) = conn.close()
        {
            warn!("關閉 {} 失敗: {}", self.opts.path.display(), e);
        }
    }
}

// ---- 唯讀查詢（`kairos hosts list`）----

/// 主機清冊的一列（唯讀查詢用）
#[derive(Debug, Clone, PartialEq)]
pub struct HostRow {
    pub agent_id: String,
    pub hostname: String,
    pub display_name: Option<String>,
    pub agent_version: Option<String>,
    pub proto: Option<i64>,
    pub host_fingerprint: Option<String>,
    pub peer: Option<String>,
    pub identity_verified: bool,
    /// 上次同步時是否線上（Kairos 未執行時一律為離線）
    pub online: bool,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub offline_after_ms: Option<i64>,
}

/// 以唯讀方式開啟資料庫（不建立檔案、不升級 schema）；Kairos 執行中也可以開啟（WAL）
pub fn open_read_only(path: &Path) -> Result<Connection, Error> {
    open_existing(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        Duration::from_secs(5),
    )
}

/// 在 Kairos 行程內開啟既有資料庫的讀寫連線（主控台 API 的低頻寫入）：只經由 SQLite 開啟，
/// 不建立檔案、不升級 schema，也不以 `std::fs` 開關資料庫檔。
///
/// 不可以在 Kairos 行程內使用 [`open_admin`]：它的 `prepare_files` 以 `std::fs` 開啟再關閉資料庫檔，
/// POSIX 的 fcntl 鎖以行程為單位，關閉任何一個檔案描述元就會放掉這個行程對該檔案的所有鎖
/// （包括 writer 連線的 SHARED 鎖），`kairos reports import` 會因此誤判 Kairos 沒有在執行
pub fn open_existing_read_write(path: &Path, busy: Duration) -> Result<Connection, Error> {
    let conn = open_existing(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        busy,
    )?;
    // checkpoint 由 Kairos 的背景執行緒處理，這條連線 commit 時不 checkpoint
    conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA wal_autocheckpoint = 0;")
        .map_err(|e| db_error("無法設定 PRAGMA", e))?;
    Ok(conn)
}

/// [`open_read_only`]、[`open_existing_read_write`] 共用：開啟既有的 Kairos 資料庫並檢查版本
fn open_existing(path: &Path, flags: OpenFlags, busy: Duration) -> Result<Connection, Error> {
    if !path.is_file() {
        return Err(Error::new(
            ErrorKind::NotFound,
            format!("找不到資料庫 {}", path.display()),
        ));
    }
    let conn = Connection::open_with_flags(path, flags)
        .map_err(|e| db_error(&format!("無法開啟 {}", path.display()), e))?;
    conn.busy_timeout(busy)
        .map_err(|e| db_error("無法設定 busy_timeout", e))?;
    let id: i32 = conn
        .query_row("PRAGMA application_id", [], |r| r.get(0))
        .map_err(|e| db_error(&format!("無法讀取 {}", path.display()), e))?;
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| db_error(&format!("無法讀取 {}", path.display()), e))?;
    if id != APPLICATION_ID || version == 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("{} 不是 Kairos 的資料庫", path.display()),
        ));
    }
    if version > SCHEMA_VERSION {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "{} 的 schema 版本 {} 比本版 Kairos 支援的 {} 新，請改用較新版本的 kairos 查詢",
                path.display(),
                version,
                SCHEMA_VERSION
            ),
        ));
    }
    Ok(conn)
}

/// 清冊中所有主機（依 `last_seen` 由舊到新）
pub fn list_hosts(conn: &Connection) -> Result<Vec<HostRow>, Error> {
    let query = || -> rusqlite::Result<Vec<HostRow>> {
        let mut stmt = conn.prepare(
            "SELECT agent_id, hostname, display_name, agent_version, proto, host_fingerprint, peer,
                    identity_verified, online, first_seen, last_seen, offline_after_ms
             FROM hosts ORDER BY last_seen, agent_id",
        )?;
        let time = |ms: i64| DateTime::from_timestamp_millis(ms).unwrap_or_default();
        let rows = stmt.query_map([], |r| {
            Ok(HostRow {
                agent_id: r.get(0)?,
                hostname: r.get(1)?,
                display_name: r.get(2)?,
                agent_version: r.get(3)?,
                proto: r.get(4)?,
                host_fingerprint: r.get(5)?,
                peer: r.get(6)?,
                identity_verified: r.get(7)?,
                online: r.get(8)?,
                first_seen: time(r.get(9)?),
                last_seen: time(r.get(10)?),
                offline_after_ms: r.get(11)?,
            })
        })?;
        rows.collect()
    };
    query().map_err(|e| db_error("無法讀取主機清冊", e))
}

/// 匯出原始 report（`kairos reports export`）：取樣時間（`reports.ts`）不早於 `since_ms` 的 report，
/// 依寫入順序轉回 JSONL 的格式（[`ReportRecord`]，`recv_at` 為收到的時間 = `ts + recv_lag`）逐筆
/// 交給 `out`。SQLite 不逐筆保存 `peer` 與 `hostname`，以主機清冊中最後的值代替。回傳匯出的筆數
pub fn export_reports(
    conn: &Connection,
    since_ms: i64,
    agent_id: Option<&str>,
    mut out: impl FnMut(&ReportRecord) -> Result<(), Error>,
) -> Result<u64, Error> {
    let fail = |e| db_error("無法讀取 report", e);
    let mut series: HashMap<i64, (String, String, std::collections::BTreeMap<String, String>)> =
        HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT series_id, name, unit, labels FROM series")
            .map_err(fail)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(fail)?;
        for row in rows {
            let (id, name, unit, labels) = row.map_err(fail)?;
            let labels = if labels.is_empty() {
                Default::default()
            } else {
                serde_json::from_str(&labels).unwrap_or_default()
            };
            series.insert(id, (name, unit, labels));
        }
    }
    let mut stmt = conn
        .prepare(
            "SELECT r.ts, r.seq, r.agent_ts, r.format, r.body, b.boot_id, r.report_seq,
                    h.agent_id, h.hostname, h.peer, r.recv_lag
             FROM reports r JOIN hosts h USING (host_id) LEFT JOIN boots b ON b.boot_ref = r.boot_ref
             WHERE r.ts >= ?1 AND (?2 IS NULL OR h.agent_id = ?2)
             ORDER BY r.id",
        )
        .map_err(fail)?;
    let mut rows = stmt.query(params![since_ms, agent_id]).map_err(fail)?;
    let mut n = 0;
    while let Some(r) = rows.next().map_err(fail)? {
        let get = |e: rusqlite::Error| db_error("無法讀取 report", e);
        let ts: i64 = r.get(0).map_err(get)?;
        let agent_ts = match r.get::<_, SqlValue>(2).map_err(get)? {
            SqlValue::Integer(ms) => AgentTs::Millis(ms).to_text(),
            SqlValue::Text(t) => t,
            _ => String::new(),
        };
        let body: Vec<u8> = r.get(4).map_err(get)?;
        let corrupt = || {
            Error::new(
                ErrorKind::InvalidData,
                format!("report 內容損毀（ts = {ts}）"),
            )
        };
        let body = match r.get::<_, i64>(3).map_err(get)? {
            FORMAT_LEGACY => {
                let (cpu_usage, total_memory, used_memory) =
                    codec::decode_legacy(&body).ok_or_else(corrupt)?;
                RecordBody::Legacy {
                    cpu_usage,
                    total_memory,
                    used_memory,
                }
            }
            _ => {
                let mut metrics = Vec::new();
                for (id, value) in codec::decode_metrics(&body).ok_or_else(corrupt)? {
                    let (name, unit, labels) = series.get(&id).ok_or_else(corrupt)?;
                    metrics.push(crate::protocol::Metric {
                        name: name.clone(),
                        value,
                        unit: unit.clone(),
                        labels: labels.clone(),
                    });
                }
                RecordBody::Metrics { metrics }
            }
        };
        let dedup = match (
            r.get::<_, Option<String>>(5).map_err(get)?,
            r.get::<_, Option<i64>>(6).map_err(get)?,
        ) {
            (Some(boot_id), Some(seq)) => Some(crate::storage::DedupKey {
                boot_id,
                report_seq: seq as u64,
            }),
            _ => None,
        };
        let recv_lag: Option<i64> = r.get(10).map_err(get)?;
        let recv_at = ts.saturating_add(recv_lag.unwrap_or(0));
        let record = ReportRecord::new(
            DateTime::from_timestamp_millis(recv_at).unwrap_or_default(),
            r.get::<_, Option<String>>(9)
                .map_err(get)?
                .unwrap_or_default(),
            r.get(7).map_err(get)?,
            r.get(8).map_err(get)?,
            r.get::<_, i64>(1).map_err(get)? as u64,
            agent_ts,
            body,
        )
        .with_dedup(dedup);
        out(&record)?;
        n += 1;
    }
    Ok(n)
}

/// 線上備份（`kairos db backup`）：以 `VACUUM INTO` 把資料庫一致的快照寫到 `dest`（不含空閒頁，
/// 比原檔小）。Kairos 執行中也可以備份，不會擋住寫入（WAL）。
///
/// 先以 `create_new` 建立權限 0640（Unix）的空檔再寫入：`dest` 已存在時拒絕（不覆寫，也不跟隨預先
/// 放置的符號連結），備份內容在寫入期間不會因 umask 而讓其他使用者可讀。失敗時刪除這個檔案
pub fn backup_to(conn: &Connection, dest: &Path) -> Result<(), Error> {
    let target = dest.to_str().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("備份路徑含有無法辨識的字元: {}", dest.display()),
        )
    })?;
    let file =
        fsperm::private_std_options(std::fs::OpenOptions::new().write(true).create_new(true))
            .open(dest)
            .map_err(|e| match e.kind() {
                ErrorKind::AlreadyExists => Error::new(
                    ErrorKind::AlreadyExists,
                    format!("{} 已存在，不覆寫", dest.display()),
                ),
                kind => Error::new(kind, format!("無法建立 {}: {}", dest.display(), e)),
            })?;
    if let Err(e) = fsperm::restrict_open_file(&file) {
        warn!("無法將 {} 的權限收緊為 0640: {}", dest.display(), e);
    }
    drop(file);
    // VACUUM INTO 可以寫入既有的空檔（沿用它的權限）
    if let Err(e) = conn.execute("VACUUM INTO ?1", [target]) {
        let _ = std::fs::remove_file(dest);
        return Err(db_error(&format!("無法備份到 {}", dest.display()), e));
    }
    Ok(())
}

#[cfg(test)]
#[path = "sqlite_tests.rs"]
mod tests;
