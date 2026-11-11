//! 兩個後端（SQLite、PostgreSQL）共用的 store 測試（`kairos/HA.md` 第 5.2、23 節）。
//!
//! 每個測試本體是 `fn(&TestDb) -> String`：對一個全新的資料庫操作並檢查結果，回傳一份「過程紀錄」
//! （各步驟結果的文字）。[`backend_tests!`] 為每個本體產生兩個測試：
//!
//! - `…::sqlite::<名稱>`：在暫存目錄的 SQLite 上執行；
//! - `…::postgres::<名稱>`：在 PostgreSQL 上執行（每個測試一個自己的 schema，結束時刪除），**並且**在
//!   SQLite 上再執行一次，兩份過程紀錄必須完全相同——PostgreSQL 的行為以 SQLite 為準。
//!
//! PostgreSQL 的連線由環境變數指定：`KAIROS_TEST_PG_URL=postgres://user:password@host:port/dbname`。
//! 未設定時 PostgreSQL 的測試印出「略過」的原因後返回（不是通過：`cargo test` 仍顯示 ok，原因在輸出中）；
//! 設定了 `KAIROS_TEST_PG_REQUIRED=1`（CI）時，未設定或連不上都視為失敗。

use chrono::{DateTime, Utc};
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::db::DbHandle;
use super::postgres::conn::{PgConn, Value};
use super::postgres::pool::{Access, PgPool};
use super::sqlite::{DB_FILE, FailPoint, SqliteOptions};
use super::{ReportStore, postgres, sqlite};
use crate::config::{PostgresSettings, SqliteSynchronous};
use crate::registry::{RegistrySnapshot, RestoredAgent};
use crate::storage::Clock;

mod cluster;
mod domain;
mod domain2;
mod migrate;
mod pg_only;
mod report;

/// 環境變數：PostgreSQL 測試資料庫的連線
pub(crate) const ENV_URL: &str = "KAIROS_TEST_PG_URL";
/// 環境變數：設為 `1` 時 PostgreSQL 的測試不可略過
pub(crate) const ENV_REQUIRED: &str = "KAIROS_TEST_PG_REQUIRED";

static SEQ: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static NOW_MS: Cell<i64> = const { Cell::new(0) };
}

/// 測試用時鐘：每個執行緒（即每個測試）各自的時間，以 [`set_now`] 設定
pub(crate) fn test_clock() -> DateTime<Utc> {
    DateTime::from_timestamp_millis(NOW_MS.with(Cell::get)).unwrap_or_default()
}

pub(crate) fn set_now(t: DateTime<Utc>) {
    NOW_MS.with(|c| c.set(t.timestamp_millis()));
}

/// 測試會用到的 report store 內部狀態（兩個後端各自實作：`sqlite_tests.rs`、`postgres/report.rs`）
pub(crate) trait TestStore: ReportStore {
    fn set_fail(&mut self, f: FailPoint);
    /// 記憶體中尚未寫出的彙總桶數
    fn rollup_len(&self) -> usize;
    fn rollup_clear(&mut self);
    /// 取出所有記憶體中的彙總，回傳樣本數的總和
    fn rollup_take_count(&mut self) -> i64;
    /// 一小時內不檢查容量
    fn defer_capacity(&mut self);
    /// 重設容量檢查的狀態（立即重新檢查）
    fn reset_capacity(&mut self);
    /// 立即重新開始一輪保留清除
    fn reset_retention(&mut self);
    fn set_max_total_bytes(&mut self, n: u64);
    fn set_max_hosts(&mut self, n: usize);
    fn used(&mut self) -> u64;
    /// 同步清冊並回傳寫入的列數
    fn sync_now(&mut self, snapshot: &RegistrySnapshot) -> usize;
}

/// 解析 `postgres://user:password@host:port/dbname`
pub(crate) fn parse_url(url: &str) -> Result<(PostgresSettings, String), String> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or("必須以 postgres:// 開頭")?;
    let (auth, rest) = rest.split_once('@').ok_or("缺少 user:password@")?;
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (addr, dbname) = rest.split_once('/').ok_or("缺少 /dbname")?;
    let (host, port) = match addr.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "port 不合法")?),
        None => (addr, 5432),
    };
    Ok((
        PostgresSettings {
            host: host.to_string(),
            port,
            dbname: dbname.split('?').next().unwrap_or(dbname).to_string(),
            user: user.to_string(),
            pool_size: 4,
            connect_timeout_secs: 5,
            ..PostgresSettings::default()
        },
        password.to_string(),
    ))
}

fn required() -> bool {
    std::env::var(ENV_REQUIRED).is_ok_and(|v| v == "1")
}

/// 一個測試用的資料庫
pub(crate) struct TestDb {
    pub dir: PathBuf,
    pool: Option<Arc<PgPool>>,
}

impl TestDb {
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kairos-bt-{}-{}-{}",
            tag,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn sqlite(tag: &str) -> Self {
        Self {
            dir: Self::temp_dir(tag),
            pool: None,
        }
    }

    /// PostgreSQL 的測試資料庫；不可用時印出原因並回 `None`（[`ENV_REQUIRED`] 時 panic）
    pub fn postgres(tag: &str) -> Option<Self> {
        let Ok(url) = std::env::var(ENV_URL) else {
            if required() {
                panic!("{ENV_REQUIRED}=1 但未設定 {ENV_URL}");
            }
            eprintln!("略過：未設定 {ENV_URL}（PostgreSQL 後端的測試需要一個可用的資料庫）");
            return None;
        };
        let (mut settings, password) =
            parse_url(&url).unwrap_or_else(|e| panic!("{ENV_URL} 不合法：{e}"));
        let dir = Self::temp_dir(tag);
        if !password.is_empty() {
            let file = dir.join("pg.password");
            std::fs::write(&file, &password).unwrap();
            settings.password_file = file.display().to_string();
        }
        settings.schema = format!(
            "kt_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let pool = PgPool::with_data_dir(settings, Some(dir.clone()));
        match pool.get(Access::ReadWrite, Duration::from_secs(5)) {
            Ok(_) => Some(Self {
                dir,
                pool: Some(pool),
            }),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                if required() {
                    panic!("{ENV_REQUIRED}=1 但無法連線到 {ENV_URL}：{e}");
                }
                eprintln!("略過：無法連線到 {ENV_URL} 指定的 PostgreSQL（{e}）");
                None
            }
        }
    }

    pub fn is_postgres(&self) -> bool {
        self.pool.is_some()
    }

    pub fn pool(&self) -> Arc<PgPool> {
        self.pool
            .clone()
            .expect("只有 PostgreSQL 的測試資料庫有連線池")
    }

    pub fn opts(&self) -> SqliteOptions {
        SqliteOptions {
            path: self.dir.join(DB_FILE),
            synchronous: SqliteSynchronous::Normal,
            raw_retention_days: 7,
            rollup_5m_retention_days: 31,
            rollup_1h_retention_days: 400,
            max_total_bytes: 0,
            max_hosts: 1000,
            max_series_per_host: crate::config::DEFAULT_MAX_SERIES_PER_HOST,
            mtls: true,
        }
    }

    /// 讀寫連線（必要時建立 schema）
    pub fn handle(&self) -> DbHandle {
        self.handle_busy(Duration::from_secs(5))
    }

    pub fn handle_busy(&self, busy: Duration) -> DbHandle {
        match &self.pool {
            Some(pool) => Box::new(postgres::open_admin(pool, busy).unwrap()),
            None => Box::new(sqlite::open_admin(&self.opts(), busy).unwrap()),
        }
    }

    /// 唯讀連線（資料庫必須已建立）
    pub fn read(&self) -> DbHandle {
        match &self.pool {
            Some(pool) => Box::new(postgres::open_read(pool).unwrap()),
            None => Box::new(sqlite::open_read_only(&self.opts().path).unwrap()),
        }
    }

    /// PostgreSQL 的讀寫連線（PostgreSQL 專屬的測試）
    pub fn pg(&self) -> PgConn {
        postgres::open_admin(&self.pool(), Duration::from_secs(5)).unwrap()
    }

    /// 開啟 report store
    pub fn store_with(
        &self,
        opts: SqliteOptions,
        clock: Clock,
    ) -> (Box<dyn TestStore>, Vec<RestoredAgent>) {
        match &self.pool {
            Some(pool) => {
                let (s, r) = postgres::report::PgStore::open(pool.clone(), opts, clock).unwrap();
                (Box::new(s), r)
            }
            None => {
                let (s, r) = sqlite::SqliteStore::open(opts, clock).unwrap();
                (Box::new(s), r)
            }
        }
    }

    pub fn store(&self, clock: Clock) -> (Box<dyn TestStore>, Vec<RestoredAgent>) {
        self.store_with(self.opts(), clock)
    }

    /// 執行查詢，回傳所有列（SQL 必須是兩個後端都合法的寫法；不帶參數）
    pub fn rows(&self, sql: &str) -> Vec<Vec<Value>> {
        match &self.pool {
            Some(_) => {
                let conn = self.pg();
                conn.query_rows(sql, ())
                    .unwrap_or_else(|e| panic!("{sql}: {e}"))
                    .iter()
                    .map(|r| {
                        (0..r.len())
                            .map(|i| r.get::<_, Value>(i).unwrap())
                            .collect()
                    })
                    .collect()
            }
            None => {
                let conn = sqlite::open_admin(&self.opts(), Duration::from_secs(5)).unwrap();
                let mut stmt = conn.prepare(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
                let n = stmt.column_count();
                stmt.query_map([], |r| {
                    (0..n)
                        .map(|i| {
                            Ok(match r.get_ref(i)? {
                                rusqlite::types::ValueRef::Null => Value::Null,
                                rusqlite::types::ValueRef::Integer(v) => Value::Integer(v),
                                rusqlite::types::ValueRef::Real(v) => Value::Real(v),
                                rusqlite::types::ValueRef::Text(t) => {
                                    Value::Text(String::from_utf8_lossy(t).into_owned())
                                }
                                rusqlite::types::ValueRef::Blob(b) => Value::Blob(b.to_vec()),
                            })
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .map(Result::unwrap)
                .collect()
            }
        }
    }

    /// 查詢一個整數
    pub fn int(&self, sql: &str) -> i64 {
        match self.rows(sql).first().and_then(|r| r.first()) {
            Some(Value::Integer(i)) => *i,
            Some(Value::Null) | None => panic!("{sql}: 沒有結果"),
            Some(other) => panic!("{sql}: 不是整數（{other:?}）"),
        }
    }

    /// 查詢一個浮點數
    pub fn real(&self, sql: &str) -> f64 {
        match self.rows(sql).first().and_then(|r| r.first()) {
            Some(Value::Real(f)) => *f,
            #[allow(clippy::cast_precision_loss)]
            Some(Value::Integer(i)) => *i as f64,
            other => panic!("{sql}: 不是數值（{other:?}）"),
        }
    }

    /// 查詢一個文字
    pub fn text(&self, sql: &str) -> Option<String> {
        match self.rows(sql).first().and_then(|r| r.first()) {
            Some(Value::Text(t)) => Some(t.clone()),
            Some(Value::Null) | None => None,
            Some(other) => panic!("{sql}: 不是文字（{other:?}）"),
        }
    }

    /// 執行一個不帶參數的敘述
    pub fn exec(&self, sql: &str) {
        match &self.pool {
            Some(_) => {
                self.pg()
                    .execute(sql, ())
                    .unwrap_or_else(|e| panic!("{sql}: {e}"));
            }
            None => {
                sqlite::open_admin(&self.opts(), Duration::from_secs(5))
                    .unwrap()
                    .execute(sql, [])
                    .unwrap_or_else(|e| panic!("{sql}: {e}"));
            }
        }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if let Some(pool) = &self.pool {
            if let Ok(conn) = pool.get(Access::ReadWrite, Duration::from_secs(5)) {
                let _ =
                    conn.execute_batch(&format!("DROP SCHEMA IF EXISTS {} CASCADE", pool.schema()));
            }
            pool.close_idle();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 為每個測試本體（`fn(&TestDb) -> String`）產生 `sqlite::<名稱>` 與 `postgres::<名稱>` 兩個測試
macro_rules! backend_tests {
    ($($name:ident),* $(,)?) => {
        mod sqlite {
            use $crate::store::backend_tests::TestDb;
            $(
                #[test]
                fn $name() {
                    let db = TestDb::sqlite(stringify!($name));
                    super::$name(&db);
                }
            )*
        }
        mod postgres {
            use $crate::store::backend_tests::TestDb;
            $(
                #[test]
                fn $name() {
                    let Some(db) = TestDb::postgres(stringify!($name)) else {
                        return;
                    };
                    let pg = super::$name(&db);
                    let reference = TestDb::sqlite(stringify!($name));
                    let lite = super::$name(&reference);
                    $crate::store::backend_tests::assert_same(&pg, &lite);
                }
            )*
        }
    };
}
pub(crate) use backend_tests;

/// 兩份過程紀錄必須相同；不同時指出第一個不同的步驟
pub(crate) fn assert_same(pg: &str, lite: &str) {
    if pg == lite {
        return;
    }
    let (mut a, mut b) = (pg.lines(), lite.lines());
    loop {
        match (a.next(), b.next()) {
            (Some(x), Some(y)) if x == y => {}
            (x, y) => {
                let x: Vec<char> = x.unwrap_or("（沒有這一步）").chars().collect();
                let y: Vec<char> = y.unwrap_or("（沒有這一步）").chars().collect();
                // 只顯示第一個不同之處的前後文
                let same = x.iter().zip(&y).take_while(|(a, b)| a == b).count();
                let from = same.saturating_sub(120);
                let part = |v: &[char]| v.iter().skip(from).take(420).collect::<String>();
                panic!(
                    "PostgreSQL 與 SQLite 的過程紀錄不同（步驟「{}」，第 {same} 個字元起）：
  PostgreSQL: …{}
  SQLite:     …{}",
                    x.iter().take_while(|c| **c != ':').collect::<String>(),
                    part(&x),
                    part(&y)
                )
            }
        }
    }
}

/// 過程紀錄：`log!(out, "名稱", 值)` 以 `{:?}` 記下一個步驟的結果
macro_rules! log {
    ($out:expr, $what:expr, $value:expr) => {{
        let v = $value;
        $out.push_str(&format!("{}: {:?}\n", $what, v));
        v
    }};
}
pub(crate) use log;

#[test]
fn url_is_parsed() {
    let (s, pw) = parse_url("postgres://kairos:secret@127.0.0.1:55414/kairos").unwrap();
    assert_eq!(
        (
            s.host.as_str(),
            s.port,
            s.dbname.as_str(),
            s.user.as_str(),
            pw.as_str()
        ),
        ("127.0.0.1", 55414, "kairos", "kairos", "secret")
    );
    let (s, pw) = parse_url("postgresql://u@db.example.com/x?sslmode=disable").unwrap();
    assert_eq!(
        (s.host.as_str(), s.port, s.dbname.as_str(), pw.as_str()),
        ("db.example.com", 5432, "x", "")
    );
    assert!(parse_url("mysql://x").is_err());
}
