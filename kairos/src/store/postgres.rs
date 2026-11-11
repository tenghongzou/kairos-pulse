//! PostgreSQL 後端（`storage.backend = "postgres"`；`kairos/HA.md` 第 4、5、7 節，L1b）。
//!
//! - [`conn`]：連線與交易（[`conn::PgConn`]、[`conn::PgTx`]），介面與 rusqlite 相同，實作 [`Store`](super::db::Store)。
//! - [`pool`]：連線池、TLS、伺服器要求的檢查，以及「這個行程使用的後端」（[`pool::active`]）。
//! - [`schema`]：與 SQLite schema v9 對等的建表 SQL；[`ensure_schema`] 在空的 schema 上建立它。
//! - 各領域的實作在 `store/postgres/<領域>.rs`，逐函式對照 `store/sqlite/<領域>.rs`。
//! - [`report`]：report 的寫入路徑（[`report::PgStore`]，[`super::ReportStore`]）。
//!
//! 與 SQLite 後端的差別（細節見 `kairos/STORAGE.md` 第 18 節）：
//!
//! - 序列化的寫入交易（[`TxMode::Immediate`](super::db::TxMode)）以交易層級的 advisory lock 排成一列，
//!   跨行程、跨節點都成立；report 的寫入不經過這個鎖。
//! - 稽核雜湊鏈另有自己的 advisory lock，`id` 在持鎖期間指定（連續、沒有空號）。
//! - 沒有資料庫檔：`kairos db backup`、`kairos reports import` 不支援（以 `pg_dump` 與 `kairos db migrate` 取代）。

pub(crate) mod admin;
pub(crate) mod alerts;
pub(crate) mod api;
pub(crate) mod audit;
pub mod conn;
pub(crate) mod enroll;
pub(crate) mod hosts;
pub(crate) mod pki;
pub mod pool;
pub(crate) mod purge;
pub(crate) mod remote;
pub mod report;
pub mod schema;
pub(crate) mod tokens;
pub(crate) mod users;

use log::{info, warn};
use std::io::{Error, ErrorKind};
use std::sync::Arc;
use std::time::Duration;

use self::conn::{LockKind, OptionalExtension, PgConn, PgError, PgResult};
use self::pool::{Access, PgPool, ServerInfo};
use super::db::TxMode;
use super::sqlite::SCHEMA_VERSION;

/// `meta.application` 的值：辨識這個 schema 是 Kairos 建立的（對應 SQLite 的 `PRAGMA application_id`）
pub const APPLICATION: &str = "KRS1";
/// `meta` 的鍵
pub const META_APPLICATION: &str = "application";
pub const META_SCHEMA_VERSION: &str = "schema_version";
/// `kairos db migrate` 正在（或中斷前正在）把資料搬進這個 schema：搬移並驗證完成後才寫入
/// `schema_version`，在那之前 Kairos 拒絕使用（`kairos/HA.md` 第 5.6 節）
pub const META_MIGRATION: &str = "migration_in_progress";

/// 資料搬移失敗或未完成，目的端不可使用
pub const CODE_MIGRATE_FAILED: &str = "KRS-E4404";
/// 這個命令不支援 PostgreSQL 後端（`kairos db backup`、`kairos reports import`）。**暫定代碼**：
/// `kairos/HA.md` 第 19 節沒有分配，待設計包正式分配
pub const CODE_PG_UNSUPPORTED: &str = "KRS-E4406";

fn pg_error(context: &str, e: PgError) -> Error {
    let kind = match e.kind() {
        super::db::StoreErrorKind::Busy => ErrorKind::ResourceBusy,
        super::db::StoreErrorKind::Full => ErrorKind::StorageFull,
        _ if e.sqlstate() == Some("42501") => ErrorKind::PermissionDenied,
        _ => ErrorKind::Other,
    };
    Error::new(kind, format!("{context}: {e}"))
}

/// schema 的狀態
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaState {
    /// schema 不存在或沒有任何資料表
    Empty,
    /// Kairos 的 schema，版本為 n
    Version(i32),
    /// `kairos db migrate` 尚未完成
    Migrating,
    /// 有資料表但不是 Kairos 的（或 `meta` 的內容不完整）
    Foreign(String),
}

/// 讀取 schema 的狀態（不修改任何東西）
pub fn schema_state(conn: &PgConn) -> PgResult<SchemaState> {
    let schema = conn.pool().schema().to_string();
    let tables: i64 = conn.query_row(
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = ?1 AND c.relkind IN ('r', 'p', 'v', 'm')",
        [schema.as_str()],
        |r| r.get(0),
    )?;
    if tables == 0 {
        return Ok(SchemaState::Empty);
    }
    let has_meta: bool = conn.query_row(
        "SELECT to_regclass(?1) IS NOT NULL",
        [format!("{schema}.meta")],
        |r| r.get(0),
    )?;
    if !has_meta {
        return Ok(SchemaState::Foreign(format!(
            "schema {schema} 中有 {tables} 個資料表，但沒有 Kairos 的 meta 表"
        )));
    }
    let get = |key: &str| -> PgResult<Option<String>> {
        conn.query_row(
            &format!("SELECT value FROM {schema}.meta WHERE key = ?1"),
            [key],
            |r| r.get(0),
        )
        .optional()
    };
    if get(META_APPLICATION)?.as_deref() != Some(APPLICATION) {
        return Ok(SchemaState::Foreign(format!(
            "schema {schema} 的 meta 表沒有 Kairos 的標記（application = {APPLICATION}）"
        )));
    }
    match get(META_SCHEMA_VERSION)? {
        Some(v) => match v.trim().parse::<i32>() {
            Ok(n) if n > 0 => Ok(SchemaState::Version(n)),
            _ => Ok(SchemaState::Foreign(format!(
                "schema {schema} 的 schema_version「{v}」不合法"
            ))),
        },
        None if get(META_MIGRATION)?.is_some() => Ok(SchemaState::Migrating),
        None => Ok(SchemaState::Foreign(format!(
            "schema {schema} 的 meta 表沒有 schema_version"
        ))),
    }
}

fn newer_schema(found: i32) -> Error {
    Error::new(
        ErrorKind::InvalidData,
        format!(
            "{}：資料庫的 schema 版本 {found} 比本版 Kairos 支援的 {SCHEMA_VERSION} 新（叢集中已有較新版本的 Kairos 升級了資料庫）；請把這個節點升級到相同的版本",
            pool::CODE_PG_SCHEMA_NEWER
        ),
    )
}

/// 把 [`SchemaState`] 轉成「可以使用」或錯誤（`Empty` 由呼叫端決定要不要建立）
fn usable(state: &SchemaState, target: &str) -> Result<bool, Error> {
    match state {
        SchemaState::Version(n) if *n == SCHEMA_VERSION => Ok(true),
        SchemaState::Version(n) if *n > SCHEMA_VERSION => Err(newer_schema(*n)),
        SchemaState::Version(n) => Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "{}：{target} 的 schema 版本 {n} 比本版 Kairos 需要的 {SCHEMA_VERSION} 舊，而且沒有對應的升級步驟（PostgreSQL 後端從 v{SCHEMA_VERSION} 開始）",
                pool::CODE_PG_REQUIREMENTS
            ),
        )),
        SchemaState::Migrating => Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "{CODE_MIGRATE_FAILED}：{target} 的資料搬移（kairos db migrate）尚未完成或驗證失敗，不可使用；請以 kairos db migrate --restart 重新執行"
            ),
        )),
        SchemaState::Foreign(why) => Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "{}：{why}；storage.postgres.schema 必須是 Kairos 專用的（空的，或由 Kairos 建立）",
                pool::CODE_PG_REQUIREMENTS
            ),
        )),
        SchemaState::Empty => Ok(false),
    }
}

/// 在交易中建立 schema v9 的所有資料表（不寫 `schema_version`；`kairos db migrate` 在搬移完成後才寫）
pub(crate) fn create_tables(conn: &PgConn) -> PgResult<()> {
    let schema = conn.pool().schema().to_string();
    conn.execute_batch(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))?;
    conn.execute_batch(schema::SCHEMA_V9)?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [META_APPLICATION, APPLICATION],
    )?;
    Ok(())
}

/// 寫入 schema 版本（建立完成、或 `kairos db migrate` 驗證通過之後）
pub(crate) fn stamp_version(conn: &PgConn) -> PgResult<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2), ('schema_upgraded_by', ?3)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        conn::params![
            META_SCHEMA_VERSION,
            SCHEMA_VERSION.to_string(),
            crate::VERSION
        ],
    )?;
    conn.execute("DELETE FROM meta WHERE key = ?1", [META_MIGRATION])?;
    Ok(())
}

/// 確認 schema 可以使用：空的 schema 上建立 v9（一個交易；以 advisory lock 確保同時啟動的節點只有
/// 一個執行，其他等待後重新檢查）。版本較新（`KRS-E4405`）、搬移未完成（`KRS-E4404`）或不是 Kairos 的
/// schema（`KRS-E4402`）時回錯誤，不做任何變更。回傳 schema 版本
pub fn ensure_schema(conn: &PgConn) -> Result<i32, Error> {
    let target = conn.pool().settings().display_target();
    let state = schema_state(conn)
        .map_err(|e| pg_error(&format!("{target}：無法讀取 schema 的狀態"), e))?;
    if usable(&state, &target)? {
        return Ok(SCHEMA_VERSION);
    }
    let create = || -> PgResult<Result<(), Error>> {
        let tx = conn.begin_tx(TxMode::Deferred)?;
        tx.lock_in_tx(LockKind::Migrate)?;
        // 等待期間可能已由另一個節點建立
        let state = schema_state(&tx)?;
        match usable(&state, &target) {
            Ok(true) => return Ok(Ok(())),
            Ok(false) => {}
            Err(e) => return Ok(Err(e)),
        }
        create_tables(&tx)?;
        stamp_version(&tx)?;
        tx.commit()?;
        info!("已在 {target} 建立資料庫 schema v{SCHEMA_VERSION}");
        Ok(Ok(()))
    };
    let result = create();
    // 建表之前準備的敘述（查 meta）在 schema 建立後仍然有效；保險起見清掉快取
    conn.clear_statements();
    match result {
        Ok(r) => r.map(|()| SCHEMA_VERSION),
        Err(e) => {
            let hint = if e.sqlstate() == Some("42501") {
                format!(
                    "（{}：帳號需要在資料庫中建立 schema，或對既有的 schema {} 有 CREATE 與 USAGE 權限）",
                    pool::CODE_PG_REQUIREMENTS,
                    conn.pool().schema()
                )
            } else {
                String::new()
            };
            Err(pg_error(&format!("{target}：建立 schema 失敗{hint}"), e))
        }
    }
}

/// 啟動時與 `--check-config` 的檢查（`kairos/HA.md` 第 4.3 節）：連線、版本、編碼、是否為主庫。
/// 不符合時回錯誤（訊息含 `KRS-E4401`／`KRS-E4402`）；同步複寫、連線數、時差只回警告訊息
pub fn check_server(pool: &Arc<PgPool>) -> Result<(ServerInfo, Vec<String>), Error> {
    let conn = pool
        .get(Access::ReadOnly, pool::DEFAULT_BUSY)
        .map_err(|e| Error::new(e.kind(), format!("{}：{e}", pool::CODE_PG_UNREACHABLE)))?;
    let info = pool::server_info(&conn).map_err(|e| {
        pg_error(
            &format!("{}：無法讀取 PostgreSQL 的狀態", pool::CODE_PG_UNREACHABLE),
            e,
        )
    })?;
    if let Some(why) = info.unmet_requirement() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("{}：{why}", pool::CODE_PG_REQUIREMENTS),
        ));
    }
    pool.note_server(&info);
    Ok((info.clone(), server_warnings(pool, &info)))
}

/// 不影響啟動的警告（`KRS-W4403`、`KRS-W8305`、連線數、未加密）
pub fn server_warnings(pool: &PgPool, info: &ServerInfo) -> Vec<String> {
    let s = pool.settings();
    let mut out = Vec::new();
    if !info.synchronous_replication() {
        out.push(format!(
            "{}：PostgreSQL 沒有同步複寫（synchronous_commit = {}，synchronous_standby_names {}）：主庫故障時可能遺失最後幾個已確認（ack）的交易。設定同步備庫，或接受這個風險",
            pool::CODE_PG_NO_SYNC_REPLICATION,
            info.synchronous_commit,
            if info.synchronous_standby_names.trim().is_empty() { "是空的" } else { "已設定" },
        ));
    }
    if info.clock_skew_ms.abs() > pool::CLOCK_SKEW_WARN_MS {
        out.push(format!(
            "{}：這台主機的時鐘與資料庫的時鐘相差 {} 毫秒（超過 2 秒）；請設定 NTP",
            pool::CODE_PG_CLOCK_SKEW,
            info.clock_skew_ms
        ));
    }
    // 池的連線之外，writer、搬移等另有專用連線（HA.md 第 4.3 節：pool_size ＋ 4）
    let needed = i64::try_from(s.pool_size)
        .unwrap_or(i64::MAX)
        .saturating_add(4);
    if info.max_connections < needed {
        out.push(format!(
            "PostgreSQL 的 max_connections = {} 小於這個 Kairos 需要的 {needed}（storage.postgres.pool_size {} ＋ 4）；連線可能被拒絕",
            info.max_connections, s.pool_size
        ));
    }
    match s.effective_sslmode() {
        crate::config::PgSslMode::Disable if !s.is_local() => out.push(format!(
            "與 PostgreSQL（{}）的連線沒有加密（storage.postgres.sslmode = \"disable\"）：資料與密碼以明文經過網路",
            s.host
        )),
        crate::config::PgSslMode::Require if !s.is_local() => out.push(format!(
            "與 PostgreSQL（{}）的連線有加密但不驗證伺服器憑證（storage.postgres.sslmode = \"require\"）：無法防止中間人；建議改用 \"verify-full\" 並設定 storage.postgres.ca_file",
            s.host
        )),
        _ => {}
    }
    out
}

/// 記錄 [`check_server`] 的警告
pub fn log_warnings(warnings: &[String]) {
    for w in warnings {
        warn!("{w}");
    }
}

/// 伺服器警告的記錄間隔（`kairos/HA.md` 第 4.3 節：啟動時與每小時一次）
pub const SERVER_WARNING_INTERVAL: Duration = Duration::from_secs(3600);

/// 健康檢查時呼叫：更新伺服器的狀態（[`PgPool::last_server`]），並依 [`SERVER_WARNING_INTERVAL`] 記錄警告
pub fn refresh_server(conn: &PgConn) {
    let pool = conn.pool();
    match pool::server_info(conn) {
        Ok(info) => {
            pool.note_server(&info);
            let warnings = server_warnings(pool, &info);
            if !warnings.is_empty() && pool.warning_due(SERVER_WARNING_INTERVAL) {
                log_warnings(&warnings);
            }
        }
        Err(e) => log::debug!("無法讀取 PostgreSQL 的狀態: {e}"),
    }
}

/// 唯讀連線（[`super::open_read`] 的 PostgreSQL 版）
pub fn open_read(pool: &Arc<PgPool>) -> Result<PgConn, Error> {
    pool.get(Access::ReadOnly, pool::DEFAULT_BUSY)
}

/// 既有 schema 的讀寫連線（[`super::open_write`] 的 PostgreSQL 版；不建立、不升級 schema）
pub fn open_write(pool: &Arc<PgPool>, busy: Duration) -> Result<PgConn, Error> {
    pool.get(Access::ReadWrite, busy)
}

/// 讀寫連線，必要時建立 schema（[`super::open_admin`] 的 PostgreSQL 版）
pub fn open_admin(pool: &Arc<PgPool>, busy: Duration) -> Result<PgConn, Error> {
    let conn = pool.get(Access::ReadWrite, busy)?;
    ensure_schema(&conn)?;
    Ok(conn)
}

/// 獨占的讀寫連線（[`super::open_exclusive`] 的 PostgreSQL 版）：取得「服務執行中」的互斥鎖並保留到連線
/// 關閉；任何節點的 Kairos（writer 持有共享鎖）或另一個需要獨占的 CLI 正在使用時，等待 `busy` 後回
/// [`ErrorKind::ResourceBusy`]。必要時建立 schema
pub fn open_exclusive(pool: &Arc<PgPool>, busy: Duration) -> Result<PgConn, Error> {
    let conn = pool.get(Access::ReadWrite, busy)?;
    let deadline = std::time::Instant::now() + busy;
    loop {
        let got = conn
            .try_session_lock(LockKind::Service)
            .map_err(|e| pg_error("無法取得資料庫的獨占鎖", e))?;
        if got {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::ResourceBusy,
                format!(
                    "{}：資料庫正在使用中（有 Kairos 節點正在執行，或另一個 kairos 子命令正在使用）",
                    pool.settings().display_target()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    ensure_schema(&conn)?;
    Ok(conn)
}

/// 健康檢查：資料庫可以寫入（連線是讀寫的主庫、schema 可用）。不取得任何鎖
pub fn probe_writable(conn: &PgConn) -> Result<(), String> {
    let (recovery, read_only): (bool, String) = conn
        .query_row(
            "SELECT pg_is_in_recovery(), current_setting('transaction_read_only')",
            (),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| e.to_string())?;
    if recovery {
        return Err("連線到的是唯讀的備庫".to_string());
    }
    if read_only == "on" {
        return Err("資料庫目前是唯讀的（default_transaction_read_only）".to_string());
    }
    match schema_state(conn).map_err(|e| e.to_string())? {
        SchemaState::Version(n) if n == SCHEMA_VERSION => Ok(()),
        other => Err(format!("schema 不可使用（{other:?}）")),
    }
}
