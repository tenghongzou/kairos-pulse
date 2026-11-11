//! `kairos db migrate`：SQLite 與 PostgreSQL 互搬（`kairos/HA.md` 第 5.6 節）。
//!
//! - **來源**是設定中目前的後端，**目的**由 `--to` 指定；目的必須是空的（PostgreSQL：schema 不存在或沒有
//!   資料表；SQLite：檔案不存在）。不做合併。
//! - **一致性**：來源是 SQLite 時以獨占鎖開啟（Kairos 執行中時 `KRS-E4304`）；來源是 PostgreSQL 時先取得
//!   「服務執行中」的互斥鎖（任何節點的 Kairos 正在使用時 `KRS-E4304`），再以 `REPEATABLE READ` 的快照讀取。
//!   目的是 PostgreSQL 時同樣持有互斥鎖，搬移期間 Kairos 無法啟動。
//! - **方式**：依 [`schema::TABLES`] 的相依順序逐表複製，保留所有主鍵值，每張表一個交易；複製後以
//!   `setval` 對齊 PostgreSQL 的 IDENTITY 序列。欄位依名稱對應（兩邊的欄位相同，只有 `reports.agent_ts`
//!   在 PostgreSQL 拆成 `agent_ts_ms`／`agent_ts_text`）。
//! - **驗證**（不可略過）：每張表的列數、`reports` 的 `max(id)` 與抽樣 1,000 列的內容、目的端完整的稽核鏈
//!   驗證（結果必須與來源相同）。任何一項不一致都回錯誤（`KRS-E4404`）。
//! - **未完成的目的端不可使用**：PostgreSQL 在驗證通過後才寫入 `meta.schema_version`（之前只有
//!   `migration_in_progress`，Kairos 拒絕啟動）；SQLite 先寫到 `<路徑>.partial`，驗證通過後才改名。
//!
//! 規格寫的是 `COPY … FROM STDIN (FORMAT binary)`；這裡以一批多列的 `INSERT … SELECT FROM unnest(…)` 寫入
//! （每批 [`BATCH_ROWS`] 列），不需要自己實作 COPY 的二進位格式，量測見 `kairos/STORAGE.md` 第 18 節。

use rusqlite::types::{Value as SqlValue, ValueRef};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::db::TxMode;
use super::postgres::conn::{LockKind, PgConn, PgError, Value, array};
use super::postgres::pool::{Access, PgPool};
use super::postgres::{self, SchemaState, schema};
use super::sqlite::{self, SCHEMA_VERSION, SqliteOptions};
use crate::audit::{AuditKey, VerifyReport};

/// 每批寫入的列數
pub const BATCH_ROWS: usize = 2_000;
/// 每批最多的位元組數（估計值；`reports.body` 很大時提早送出）
const BATCH_BYTES: usize = 8 * 1024 * 1024;
/// `reports` 抽樣比對的列數
pub const SAMPLE_ROWS: i64 = 1_000;
/// 等待獨占鎖的時間
const EXCLUSIVE_WAIT: Duration = Duration::from_secs(2);

/// 資料庫正在使用中（與 `kairos reports import` 相同的代碼）
pub const CODE_IN_USE: &str = "KRS-E4304";
/// 搬移失敗或驗證不一致
pub const CODE_FAILED: &str = postgres::CODE_MIGRATE_FAILED;

/// 搬移的選項
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// 只複製取樣時間在這之後（Unix 毫秒）的原始 report 與行程快照；其他資料表一律全部複製
    pub reports_since_ms: Option<i64>,
    /// 只檢查與計數，不寫入
    pub dry_run: bool,
    /// 目的端有上次未完成的搬移時先清空（PostgreSQL：刪除 schema；SQLite：刪除 `.partial`）
    pub restart: bool,
}

/// 一張資料表的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableResult {
    pub name: &'static str,
    /// 來源的列數（套用 `reports_since` 之後）
    pub source_rows: u64,
    /// 寫入目的端的列數（試執行時為 0）
    pub copied_rows: u64,
}

/// 搬移的結果
#[derive(Debug, Clone)]
pub struct Summary {
    pub tables: Vec<TableResult>,
    pub total_rows: u64,
    /// `reports` 抽樣比對的列數
    pub sampled: u64,
    /// 稽核鏈驗證檢查的筆數與是否完整（來源與目的相同才會到這裡）
    pub audit_checked: u64,
    pub audit_intact: bool,
    pub elapsed: Duration,
    pub dry_run: bool,
}

/// 搬移的錯誤
#[derive(Debug)]
pub struct MigrateError {
    pub code: &'static str,
    pub message: String,
    /// 設定或用法的問題（結束代碼 3）而不是執行失敗
    pub config: bool,
}

impl MigrateError {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            code: CODE_FAILED,
            message: message.into(),
            config: false,
        }
    }

    fn in_use(message: impl Into<String>) -> Self {
        Self {
            code: CODE_IN_USE,
            message: message.into(),
            config: false,
        }
    }
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<PgError> for MigrateError {
    fn from(e: PgError) -> Self {
        Self::failed(format!("PostgreSQL: {e}"))
    }
}

impl From<rusqlite::Error> for MigrateError {
    fn from(e: rusqlite::Error) -> Self {
        Self::failed(format!("SQLite: {e}"))
    }
}

type Result<T> = std::result::Result<T, MigrateError>;

/// 一個欄位在兩邊的對應
#[derive(Debug, Clone)]
struct Column {
    /// PostgreSQL 的欄位名稱
    name: String,
    /// PostgreSQL 陣列轉型用的型別（`bigint`、`text`、`bytea`、`double precision`）
    pg_type: String,
}

/// 一張資料表的欄位（PostgreSQL 的順序）
fn pg_columns(conn: &PgConn, table: &str) -> Result<Vec<Column>> {
    let rows = conn.query_rows(
        "SELECT column_name::text, data_type::text FROM information_schema.columns
         WHERE table_schema = ?1 AND table_name = ?2 ORDER BY ordinal_position",
        [conn.pool().schema(), table],
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let name: String = r.get(0)?;
        let pg_type: String = r.get(1)?;
        if !matches!(
            pg_type.as_str(),
            "bigint" | "text" | "bytea" | "double precision"
        ) {
            return Err(MigrateError::failed(format!(
                "PostgreSQL 的 {table}.{name} 是未預期的型別 {pg_type}"
            )));
        }
        out.push(Column { name, pg_type });
    }
    if out.is_empty() {
        return Err(MigrateError::failed(format!(
            "PostgreSQL 的 schema 中沒有資料表 {table}"
        )));
    }
    Ok(out)
}

/// SQLite 一張資料表的欄位名稱
fn sqlite_columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if names.is_empty() {
        return Err(MigrateError::failed(format!(
            "SQLite 資料庫中沒有資料表 {table}"
        )));
    }
    Ok(names)
}

/// PostgreSQL 的欄位在 SQLite 中的來源：同名的欄位，或拆開的欄位（[`schema::SPLIT_COLUMNS`]）的原欄位
#[derive(Debug, Clone, PartialEq, Eq)]
enum SqliteSource {
    Same(String),
    /// `reports.agent_ts`（無型別）中的整數 → `agent_ts_ms`
    SplitInteger(String),
    /// `reports.agent_ts` 中的文字 → `agent_ts_text`
    SplitText(String),
}

impl SqliteSource {
    fn column(&self) -> &str {
        match self {
            SqliteSource::Same(c) | SqliteSource::SplitInteger(c) | SqliteSource::SplitText(c) => c,
        }
    }
}

/// 兩邊欄位的對應；欄位不一致（schema 漂移）時回錯誤，不猜
fn column_mapping(table: &str, pg: &[Column], sqlite: &[String]) -> Result<Vec<SqliteSource>> {
    let split = schema::SPLIT_COLUMNS.iter().find(|(t, _, _)| *t == table);
    let mut used: Vec<&str> = Vec::new();
    let mut out = Vec::with_capacity(pg.len());
    for col in pg {
        let source = match split {
            Some((_, from, parts)) if parts.contains(&col.name.as_str()) => {
                used.push(from);
                if col.pg_type == "bigint" {
                    SqliteSource::SplitInteger((*from).to_string())
                } else {
                    SqliteSource::SplitText((*from).to_string())
                }
            }
            _ => {
                if !sqlite.iter().any(|s| s == &col.name) {
                    return Err(MigrateError::failed(format!(
                        "SQLite 的 {table} 沒有欄位 {}（兩邊的 schema 不一致）",
                        col.name
                    )));
                }
                used.push(col.name.as_str());
                SqliteSource::Same(col.name.clone())
            }
        };
        out.push(source);
    }
    if let Some(extra) = sqlite.iter().find(|s| !used.contains(&s.as_str())) {
        return Err(MigrateError::failed(format!(
            "PostgreSQL 的 {table} 沒有欄位 {extra}（兩邊的 schema 不一致）"
        )));
    }
    Ok(out)
}

/// 只複製最近資料的資料表與它的時間欄位（Unix 毫秒）
fn since_column(table: &str) -> Option<&'static str> {
    match table {
        "reports" | "process_snapshots" => Some("ts"),
        _ => None,
    }
}

fn where_since(table: &str, since: Option<i64>) -> String {
    match (since_column(table), since) {
        (Some(col), Some(ms)) => format!(" WHERE {col} >= {ms}"),
        _ => String::new(),
    }
}

fn from_sqlite(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Integer(i),
        ValueRef::Real(f) => Value::Real(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}

fn to_sqlite(v: Value) -> SqlValue {
    match v {
        Value::Null | Value::Array(_) => SqlValue::Null,
        Value::Integer(i) => SqlValue::Integer(i),
        Value::Real(f) => SqlValue::Real(f),
        Value::Text(t) => SqlValue::Text(t),
        Value::Blob(b) => SqlValue::Blob(b),
    }
}

fn value_size(v: &Value) -> usize {
    match v {
        Value::Text(t) => t.len() + 8,
        Value::Blob(b) => b.len() + 8,
        _ => 8,
    }
}

/// 把一列 SQLite 的值轉成 PostgreSQL 欄位順序的值
fn sqlite_row(
    row: &rusqlite::Row<'_>,
    mapping: &[SqliteSource],
    positions: &[usize],
) -> rusqlite::Result<Vec<Value>> {
    let mut out = Vec::with_capacity(mapping.len());
    for (source, pos) in mapping.iter().zip(positions) {
        let v = from_sqlite(row.get_ref(*pos)?);
        out.push(match source {
            SqliteSource::Same(_) => v,
            SqliteSource::SplitInteger(_) => match v {
                Value::Integer(_) => v,
                _ => Value::Null,
            },
            SqliteSource::SplitText(_) => match v {
                Value::Text(_) => v,
                // 整數歸 agent_ts_ms；其他型別（不應該出現）以文字保存，不丟掉
                Value::Null | Value::Integer(_) => Value::Null,
                Value::Real(f) => Value::Text(f.to_string()),
                Value::Blob(b) => Value::Text(String::from_utf8_lossy(&b).into_owned()),
                Value::Array(_) => Value::Null,
            },
        });
    }
    Ok(out)
}

/// 寫入 PostgreSQL 的一批：每個欄位一個陣列參數
fn pg_insert_batch(
    conn: &PgConn,
    table: &str,
    cols: &[Column],
    rows: &mut Vec<Vec<Value>>,
) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let n = rows.len();
    let mut arrays: Vec<Vec<Value>> = cols.iter().map(|_| Vec::with_capacity(n)).collect();
    for row in rows.drain(..) {
        for (slot, v) in arrays.iter_mut().zip(row) {
            slot.push(v);
        }
    }
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    let casts: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!("?{}::{}[]", i + 1, c.pg_type))
        .collect();
    let sql = format!(
        "INSERT INTO {table} ({}) SELECT * FROM unnest({})",
        names.join(", "),
        casts.join(", ")
    );
    let params: Vec<Value> = arrays.into_iter().map(array).collect();
    let written = conn.execute(&sql, params)?;
    Ok(written as u64)
}

fn count_sqlite(conn: &rusqlite::Connection, table: &str, since: Option<i64>) -> Result<u64> {
    let n: i64 = conn.query_row(
        &format!("SELECT count(*) FROM {table}{}", where_since(table, since)),
        [],
        |r| r.get(0),
    )?;
    Ok(u64::try_from(n).unwrap_or(0))
}

fn count_pg(conn: &PgConn, table: &str, since: Option<i64>) -> Result<u64> {
    Ok(conn.query_row(
        &format!("SELECT count(*) FROM {table}{}", where_since(table, since)),
        (),
        |r| r.get(0),
    )?)
}

/// 來源與目的的稽核鏈驗證結果必須相同；回傳（檢查的筆數, 鏈是否完整）
fn compare_audit(source: &VerifyReport, dest: &VerifyReport) -> Result<(u64, bool)> {
    if source != dest {
        return Err(MigrateError::failed(format!(
            "目的端的稽核鏈驗證結果與來源不同（來源：檢查 {} 筆、最後一筆 {:?}、斷點 {:?}、HMAC {}；目的：檢查 {} 筆、最後一筆 {:?}、斷點 {:?}、HMAC {}）",
            source.checked,
            source.last_id,
            source.broken,
            source.hmac.status,
            dest.checked,
            dest.last_id,
            dest.broken,
            dest.hmac.status
        )));
    }
    Ok((dest.checked, dest.broken.is_none()))
}

/// 抽樣的 id：在 [min, max] 之間平均取 [`SAMPLE_ROWS`] 個起點（每個起點比對「id ≥ 起點的第一列」）
fn sample_points(min: i64, max: i64) -> Vec<i64> {
    if max < min {
        return Vec::new();
    }
    let span = max - min;
    let n = SAMPLE_ROWS.min(span + 1);
    (0..n)
        .map(|i| min + (i128::from(span) * i128::from(i) / i128::from(n.max(1))) as i64)
        .collect()
}

/// `reports` 一列在兩邊共同的表示（PostgreSQL 的欄位順序）
fn sqlite_report_at(
    conn: &rusqlite::Connection,
    mapping: &[SqliteSource],
    from_id: i64,
    since: Option<i64>,
) -> Result<Option<Vec<Value>>> {
    let cols: Vec<&str> = mapping.iter().map(SqliteSource::column).collect();
    let positions: Vec<usize> = (0..cols.len()).collect();
    let filter = since.map_or(String::new(), |ms| format!(" AND ts >= {ms}"));
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM reports WHERE id >= ?1{filter} ORDER BY id LIMIT 1",
        cols.join(", ")
    ))?;
    let mut rows = stmt.query([from_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(sqlite_row(row, mapping, &positions)?)),
        None => Ok(None),
    }
}

fn pg_report_at(
    conn: &PgConn,
    cols: &[Column],
    from_id: i64,
    since: Option<i64>,
) -> Result<Option<Vec<Value>>> {
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    let filter = since.map_or(String::new(), |ms| format!(" AND ts >= {ms}"));
    let rows = conn.query_rows(
        &format!(
            "SELECT {} FROM reports WHERE id >= ?1{filter} ORDER BY id LIMIT 1",
            names.join(", ")
        ),
        [from_id],
    )?;
    match rows.first() {
        Some(row) => {
            let mut out = Vec::with_capacity(cols.len());
            for i in 0..cols.len() {
                out.push(row.get::<_, Value>(i)?);
            }
            Ok(Some(out))
        }
        None => Ok(None),
    }
}

/// 驗證 `reports`：`max(id)` 相同，抽樣的列內容相同。回傳比對的列數
fn verify_reports(
    sqlite: &rusqlite::Connection,
    pg: &PgConn,
    cols: &[Column],
    mapping: &[SqliteSource],
    since: Option<i64>,
) -> Result<u64> {
    let filter = where_since("reports", since);
    let (s_min, s_max): (Option<i64>, Option<i64>) = sqlite.query_row(
        &format!("SELECT min(id), max(id) FROM reports{filter}"),
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (p_min, p_max): (Option<i64>, Option<i64>) = pg.query_row(
        &format!("SELECT min(id), max(id) FROM reports{filter}"),
        (),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if (s_min, s_max) != (p_min, p_max) {
        return Err(MigrateError::failed(format!(
            "reports 的 id 範圍不一致（SQLite {s_min:?}..{s_max:?}，PostgreSQL {p_min:?}..{p_max:?}）"
        )));
    }
    let (Some(min), Some(max)) = (s_min, s_max) else {
        return Ok(0);
    };
    let mut compared = 0;
    for point in sample_points(min, max) {
        let a = sqlite_report_at(sqlite, mapping, point, since)?;
        let b = pg_report_at(pg, cols, point, since)?;
        if a != b {
            return Err(MigrateError::failed(format!(
                "reports 抽樣比對不一致（id ≥ {point} 的第一列在兩邊的內容不同）"
            )));
        }
        compared += 1;
    }
    Ok(compared)
}

fn schema_mismatch(found: i32, what: &str) -> MigrateError {
    MigrateError {
        code: CODE_FAILED,
        message: format!(
            "{what} 的 schema 版本是 {found}，這個版本的 Kairos 只能搬移 v{SCHEMA_VERSION}；請先以相同版本的 Kairos 啟動一次完成升級"
        ),
        config: true,
    }
}

fn sqlite_version(conn: &rusqlite::Connection) -> Result<i32> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// 取得 PostgreSQL 的獨占連線（不建立 schema）
fn pg_exclusive(pool: &Arc<PgPool>) -> Result<PgConn> {
    let conn = pool
        .get(Access::ReadWrite, EXCLUSIVE_WAIT)
        .map_err(|e| MigrateError {
            code: postgres::pool::CODE_PG_UNREACHABLE,
            message: e.to_string(),
            config: false,
        })?;
    let deadline = Instant::now() + EXCLUSIVE_WAIT;
    loop {
        if conn.try_session_lock(LockKind::Service)? {
            return Ok(conn);
        }
        if Instant::now() >= deadline {
            return Err(MigrateError::in_use(format!(
                "{} 正在使用中：有 Kairos 節點正在執行，或另一個 kairos 子命令正在使用。請停止所有節點後再執行",
                pool.settings().display_target()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn open_sqlite_source(opts: &SqliteOptions) -> Result<rusqlite::Connection> {
    if !opts.path.is_file() {
        return Err(MigrateError {
            code: CODE_FAILED,
            message: format!(
                "找不到 SQLite 資料庫 {}（沒有資料可以搬移）",
                opts.path.display()
            ),
            config: true,
        });
    }
    sqlite::open_exclusive(opts, EXCLUSIVE_WAIT).map_err(|e| {
        if e.kind() == ErrorKind::ResourceBusy {
            MigrateError::in_use(format!(
                "{} 正在使用中：Kairos 執行中，或另一個 kairos 子命令正在使用。請停止 Kairos 後再執行",
                opts.path.display()
            ))
        } else {
            MigrateError::failed(e.to_string())
        }
    })
}

/// SQLite → PostgreSQL。`key`：稽核 HMAC 的金鑰（資料目錄的 `audit-hmac.key`；沒有時只驗證雜湊鏈）。
/// `progress` 在每張表完成時呼叫
pub fn sqlite_to_postgres(
    source: &SqliteOptions,
    pool: &Arc<PgPool>,
    opts: &Options,
    key: Option<&AuditKey>,
    progress: &mut dyn FnMut(&TableResult),
) -> Result<Summary> {
    let started = Instant::now();
    let src = open_sqlite_source(source)?;
    let version = sqlite_version(&src)?;
    if version != SCHEMA_VERSION {
        return Err(schema_mismatch(version, "SQLite 資料庫"));
    }
    let dest = pg_exclusive(pool)?;
    let target = pool.settings().display_target();
    match postgres::schema_state(&dest)? {
        SchemaState::Empty => {}
        SchemaState::Migrating if opts.restart && !opts.dry_run => {
            dest.execute_batch(&format!("DROP SCHEMA {} CASCADE", pool.schema()))?;
            dest.clear_statements();
        }
        SchemaState::Migrating if opts.dry_run => {}
        SchemaState::Migrating => {
            return Err(MigrateError::failed(format!(
                "{target} 有上次未完成的搬移；加上 --restart 會先清空目的端再重新執行"
            )));
        }
        SchemaState::Version(_) => {
            return Err(MigrateError::failed(format!(
                "{target} 已經有 Kairos 的資料；目的端必須是空的（不做合併）。確認 storage.postgres 指向正確的資料庫與 schema"
            )));
        }
        SchemaState::Foreign(why) => {
            return Err(MigrateError::failed(format!(
                "{why}；目的端必須是空的 schema"
            )));
        }
    }

    let since = opts.reports_since_ms;
    let mut tables = Vec::new();
    if opts.dry_run {
        for table in schema::TABLES {
            let result = TableResult {
                name: table,
                source_rows: count_sqlite(&src, table, since)?,
                copied_rows: 0,
            };
            progress(&result);
            tables.push(result);
        }
        let source_audit = sqlite::audit::verify_with_key(&src, key)?;
        return Ok(Summary {
            total_rows: tables.iter().map(|t| t.source_rows).sum(),
            tables,
            sampled: 0,
            audit_checked: source_audit.checked,
            audit_intact: source_audit.broken.is_none(),
            elapsed: started.elapsed(),
            dry_run: true,
        });
    }

    // 建表並標記「搬移中」（一個交易；之後每張表一個交易）
    {
        let tx = dest.begin_tx(TxMode::Deferred)?;
        postgres::create_tables(&tx)?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [postgres::META_MIGRATION, crate::VERSION],
        )?;
        tx.commit()?;
        dest.clear_statements();
    }

    let mut report_mapping = None;
    for table in schema::TABLES {
        let cols = pg_columns(&dest, table)?;
        let mapping = column_mapping(table, &cols, &sqlite_columns(&src, table)?)?;
        // 同一個 SQLite 欄位可能對應兩個 PostgreSQL 欄位（拆開的欄位）：SELECT 中各取一次
        let select: Vec<&str> = mapping.iter().map(SqliteSource::column).collect();
        let positions: Vec<usize> = (0..select.len()).collect();
        let tx = dest.begin_tx(TxMode::Deferred)?;
        let mut copied = 0_u64;
        let mut source_rows = 0_u64;
        {
            let mut stmt = src.prepare(&format!(
                "SELECT {} FROM {table}{}",
                select.join(", "),
                where_since(table, since)
            ))?;
            let mut rows = stmt.query([])?;
            let mut batch: Vec<Vec<Value>> = Vec::with_capacity(BATCH_ROWS);
            let mut bytes = 0_usize;
            while let Some(row) = rows.next()? {
                let mut values = sqlite_row(row, &mapping, &positions)?;
                // meta 的鍵由目的端自己管理：建表時寫入的 application 與搬移標記不被來源覆蓋
                if *table == "meta" {
                    let key_text = match values.first() {
                        Some(Value::Text(k)) => k.as_str(),
                        _ => "",
                    };
                    if key_text == postgres::META_APPLICATION
                        || key_text == postgres::META_SCHEMA_VERSION
                        || key_text == postgres::META_MIGRATION
                    {
                        continue;
                    }
                    // SQLite 的 meta.value 沒有型別：一律以文字保存
                    if let Some(v) = values.get_mut(1) {
                        *v = match std::mem::replace(v, Value::Null) {
                            Value::Integer(i) => Value::Text(i.to_string()),
                            Value::Real(f) => Value::Text(f.to_string()),
                            other => other,
                        };
                    }
                }
                source_rows += 1;
                bytes += values.iter().map(value_size).sum::<usize>();
                batch.push(values);
                if batch.len() >= BATCH_ROWS || bytes >= BATCH_BYTES {
                    copied += pg_insert_batch(&tx, table, &cols, &mut batch)?;
                    bytes = 0;
                }
            }
            copied += pg_insert_batch(&tx, table, &cols, &mut batch)?;
        }
        tx.commit()?;
        if *table == "reports" {
            report_mapping = Some((cols, mapping));
        }
        let result = TableResult {
            name: table,
            source_rows,
            copied_rows: copied,
        };
        progress(&result);
        tables.push(result);
    }

    // IDENTITY 序列對齊到目前的最大值（空的資料表維持從 1 開始）
    for (table, column) in schema::IDENTITY_COLUMNS {
        dest.execute_batch(&format!(
            "SELECT setval(pg_get_serial_sequence('{schema}.{table}', '{column}'),
                           GREATEST((SELECT max({column}) FROM {table}), 1),
                           (SELECT max({column}) FROM {table}) IS NOT NULL)",
            schema = pool.schema()
        ))?;
    }

    // 驗證 1：列數
    for t in &tables {
        // meta 另外比較：目的端有自己的鍵
        let expected = t.source_rows;
        let mut found = count_pg(&dest, t.name, since)?;
        if t.name == "meta" {
            let own: u64 = dest.query_row(
                "SELECT count(*) FROM meta WHERE key IN (?1, ?2, ?3)",
                [
                    postgres::META_APPLICATION,
                    postgres::META_SCHEMA_VERSION,
                    postgres::META_MIGRATION,
                ],
                |r| r.get(0),
            )?;
            found = found.saturating_sub(own);
        }
        if found != expected || t.copied_rows != expected {
            return Err(MigrateError::failed(format!(
                "{} 的列數不一致（來源 {expected} 列，寫入 {} 列，目的端 {found} 列）；目的端保留供檢查，不可使用",
                t.name, t.copied_rows
            )));
        }
    }
    // 驗證 2：reports 的 max(id) 與抽樣
    let (cols, mapping) = report_mapping
        .ok_or_else(|| MigrateError::failed("schema 中沒有 reports 資料表".to_string()))?;
    let sampled = verify_reports(&src, &dest, &cols, &mapping, since)?;
    // 驗證 3：稽核鏈
    let source_audit = sqlite::audit::verify_with_key(&src, key)?;
    let dest_audit = postgres::audit::verify_with_key(&dest, key)?;
    let (audit_checked, audit_intact) = compare_audit(&source_audit, &dest_audit)?;

    postgres::stamp_version(&dest)?;
    Ok(Summary {
        total_rows: tables.iter().map(|t| t.copied_rows).sum(),
        tables,
        sampled,
        audit_checked,
        audit_intact,
        elapsed: started.elapsed(),
        dry_run: false,
    })
}

/// 未完成的 SQLite 目的檔
pub fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".partial");
    PathBuf::from(s)
}

fn remove_sqlite_files(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut s = path.as_os_str().to_os_string();
        s.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(s));
    }
}

/// PostgreSQL → SQLite（`dest`：新的 SQLite 檔，不可已存在）
pub fn postgres_to_sqlite(
    pool: &Arc<PgPool>,
    dest: &Path,
    template: &SqliteOptions,
    opts: &Options,
    key: Option<&AuditKey>,
    progress: &mut dyn FnMut(&TableResult),
) -> Result<Summary> {
    let started = Instant::now();
    if dest.exists() {
        return Err(MigrateError {
            code: CODE_FAILED,
            message: format!(
                "{} 已經存在；目的端必須是不存在的檔案（不做合併、不覆寫）",
                dest.display()
            ),
            config: true,
        });
    }
    let partial = partial_path(dest);
    if partial.exists() {
        if opts.dry_run {
            // 試執行不動目的端
        } else if opts.restart {
            remove_sqlite_files(&partial);
        } else {
            return Err(MigrateError::failed(format!(
                "{} 是上次未完成的搬移；加上 --restart 會先刪除它再重新執行",
                partial.display()
            )));
        }
    }
    let src = pg_exclusive(pool)?;
    match postgres::schema_state(&src)? {
        SchemaState::Version(n) if n == SCHEMA_VERSION => {}
        SchemaState::Version(n) => return Err(schema_mismatch(n, "PostgreSQL 資料庫")),
        other => {
            return Err(MigrateError {
                code: CODE_FAILED,
                message: format!(
                    "{} 沒有可以搬移的 Kairos 資料（{other:?}）",
                    pool.settings().display_target()
                ),
                config: true,
            });
        }
    }
    // 同一個快照中讀完所有資料表
    let snapshot = src.begin_snapshot()?;
    let since = opts.reports_since_ms;
    let mut tables = Vec::new();
    if opts.dry_run {
        for table in schema::TABLES {
            let result = TableResult {
                name: table,
                source_rows: count_pg(&snapshot, table, since)?,
                copied_rows: 0,
            };
            progress(&result);
            tables.push(result);
        }
        let source_audit = postgres::audit::verify_with_key(&snapshot, key)?;
        return Ok(Summary {
            total_rows: tables.iter().map(|t| t.source_rows).sum(),
            tables,
            sampled: 0,
            audit_checked: source_audit.checked,
            audit_intact: source_audit.broken.is_none(),
            elapsed: started.elapsed(),
            dry_run: true,
        });
    }

    let out_opts = SqliteOptions {
        path: partial.clone(),
        ..template.clone()
    };
    let mut out = sqlite::open_exclusive(&out_opts, EXCLUSIVE_WAIT)
        .map_err(|e| MigrateError::failed(format!("無法建立 {}: {e}", partial.display())))?;
    let result = (|| -> Result<Summary> {
        let mut report_mapping = None;
        for table in schema::TABLES {
            let cols = pg_columns(&snapshot, table)?;
            let mapping = column_mapping(table, &cols, &sqlite_columns(&out, table)?)?;
            // SQLite 的欄位：拆開的欄位合併回一個（兩個 PostgreSQL 欄位最多一個非 NULL）
            let mut sqlite_cols: Vec<&str> = Vec::new();
            let mut slot_of: Vec<usize> = Vec::with_capacity(mapping.len());
            for m in &mapping {
                let name = m.column();
                let slot = match sqlite_cols.iter().position(|c| *c == name) {
                    Some(i) => i,
                    None => {
                        sqlite_cols.push(name);
                        sqlite_cols.len() - 1
                    }
                };
                slot_of.push(slot);
            }
            let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
            let insert = format!(
                "INSERT INTO {table} ({}) VALUES ({})",
                sqlite_cols.join(", "),
                vec!["?"; sqlite_cols.len()].join(", ")
            );
            let tx = out.transaction()?;
            if *table == "meta" {
                // 新建的 SQLite 資料庫已有自己的 meta（schema_upgraded_by）：以來源的內容為準
                tx.execute("DELETE FROM meta", [])?;
            }
            let is_meta = *table == "meta";
            let mut copied = 0_u64;
            let mut source_rows = 0_u64;
            {
                let mut stmt = tx.prepare(&insert)?;
                let mut failure: Option<rusqlite::Error> = None;
                snapshot.query_each(
                    &format!(
                        "SELECT {} FROM {table}{}",
                        names.join(", "),
                        where_since(table, since)
                    ),
                    (),
                    |row| {
                        let mut values: Vec<SqlValue> = vec![SqlValue::Null; sqlite_cols.len()];
                        for (i, slot) in slot_of.iter().enumerate() {
                            let v: Value = row.get(i)?;
                            // PostgreSQL 後端自己的 meta 鍵（SQLite 以 PRAGMA 保存）不複製
                            if is_meta
                                && i == 0
                                && matches!(&v, Value::Text(k) if k == postgres::META_APPLICATION
                                    || k == postgres::META_SCHEMA_VERSION
                                    || k == postgres::META_MIGRATION)
                            {
                                return Ok(true);
                            }
                            if !matches!(v, Value::Null) {
                                values[*slot] = to_sqlite(v);
                            }
                        }
                        source_rows += 1;
                        match stmt.execute(rusqlite::params_from_iter(values)) {
                            Ok(n) => {
                                copied += n as u64;
                                Ok(true)
                            }
                            Err(e) => {
                                failure = Some(e);
                                Ok(false)
                            }
                        }
                    },
                )?;
                if let Some(e) = failure {
                    return Err(e.into());
                }
            }
            tx.commit()?;
            if *table == "reports" {
                report_mapping = Some((cols, mapping));
            }
            let result = TableResult {
                name: table,
                source_rows,
                copied_rows: copied,
            };
            progress(&result);
            tables.push(result);
        }
        // 驗證 1：列數
        for t in &tables {
            let found = count_sqlite(&out, t.name, since)?;
            if found != t.source_rows || t.copied_rows != t.source_rows {
                return Err(MigrateError::failed(format!(
                    "{} 的列數不一致（來源 {} 列，寫入 {} 列，目的端 {found} 列）",
                    t.name, t.source_rows, t.copied_rows
                )));
            }
        }
        let (cols, mapping) = report_mapping
            .ok_or_else(|| MigrateError::failed("schema 中沒有 reports 資料表".to_string()))?;
        let sampled = verify_reports(&out, &snapshot, &cols, &mapping, since)?;
        let source_audit = postgres::audit::verify_with_key(&snapshot, key)?;
        let dest_audit = sqlite::audit::verify_with_key(&out, key)?;
        let (audit_checked, audit_intact) = compare_audit(&source_audit, &dest_audit)?;
        Ok(Summary {
            total_rows: tables.iter().map(|t| t.copied_rows).sum(),
            tables: std::mem::take(&mut tables),
            sampled,
            audit_checked,
            audit_intact,
            elapsed: started.elapsed(),
            dry_run: false,
        })
    })();
    // 關閉（exclusive 模式下關閉時 checkpoint 並刪除 -wal）後才改名
    let closed = out.close().map_err(|(_, e)| MigrateError::from(e));
    let summary = result?;
    closed?;
    std::fs::rename(&partial, dest).map_err(|e| {
        MigrateError::failed(format!(
            "無法把 {} 改名為 {}: {e}",
            partial.display(),
            dest.display()
        ))
    })?;
    Ok(Summary {
        elapsed: started.elapsed(),
        ..summary
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_points_cover_the_range() {
        assert!(sample_points(5, 4).is_empty());
        assert_eq!(sample_points(7, 7), vec![7]);
        assert_eq!(sample_points(1, 3), vec![1, 2, 3]);
        let p = sample_points(1, 1_000_000);
        assert_eq!(p.len(), 1000);
        assert_eq!(p[0], 1);
        assert!(p.windows(2).all(|w| w[0] < w[1]));
        assert!(*p.last().unwrap() <= 1_000_000);
    }

    #[test]
    fn split_columns_map_by_type() {
        let pg = vec![
            Column {
                name: "id".into(),
                pg_type: "bigint".into(),
            },
            Column {
                name: "agent_ts_ms".into(),
                pg_type: "bigint".into(),
            },
            Column {
                name: "agent_ts_text".into(),
                pg_type: "text".into(),
            },
        ];
        let sqlite = vec!["id".to_string(), "agent_ts".to_string()];
        let m = column_mapping("reports", &pg, &sqlite).unwrap();
        assert_eq!(
            m,
            vec![
                SqliteSource::Same("id".into()),
                SqliteSource::SplitInteger("agent_ts".into()),
                SqliteSource::SplitText("agent_ts".into()),
            ]
        );
        // 欄位不一致時不猜
        let e = column_mapping("hosts", &pg, &sqlite).unwrap_err();
        assert!(e.message.contains("agent_ts_ms"), "{}", e.message);
        let more = vec![
            "id".to_string(),
            "agent_ts".to_string(),
            "extra".to_string(),
        ];
        let e = column_mapping("reports", &pg, &more).unwrap_err();
        assert!(e.message.contains("extra"), "{}", e.message);
    }

    #[test]
    fn since_only_applies_to_raw_tables() {
        assert_eq!(where_since("reports", Some(5)), " WHERE ts >= 5");
        assert_eq!(where_since("process_snapshots", Some(5)), " WHERE ts >= 5");
        assert_eq!(where_since("hosts", Some(5)), "");
        assert_eq!(where_since("reports", None), "");
    }
}
