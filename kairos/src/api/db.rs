//! 主控台 API 的資料庫連線：唯讀連線池（WAL 讀者，不擋住 report 的寫入）與低頻寫入用的連線。
//!
//! - 查詢一律在 blocking 執行緒上執行；同時最多 [`MAX_READERS`] 條，讓大量的 API 請求不會搶走
//!   writer 的 CPU（`kairos/API.md` 第 7、9 節）。
//! - 資料庫檔被換掉（writer 建立了新的資料庫）時，連線在下一次取用時重新開啟。
//! - 寫入（token 的最後使用時間、稽核旗標、被拒的憑證）只由背景工作以短交易進行，
//!   busy 時最多等待 [`WRITE_BUSY`]，不會讓 report 的 writer 等待太久。

use log::warn;
use rusqlite::Connection;
use std::io::Error;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

use super::http::ApiError;
use crate::store::db::StoreResult;
use crate::store::sqlite::{
    SqliteOptions, file_identity, open_existing_read_write, open_read_only,
};

/// 同時進行的查詢上限
pub const MAX_READERS: usize = 4;

/// 寫入連線的 busy 等待上限
pub const WRITE_BUSY: Duration = Duration::from_millis(250);

/// 一條唯讀連線與它開啟的檔案身分
struct Reader {
    conn: Connection,
    file: Option<(u64, u64)>,
}

/// 唯讀連線池
pub struct ReadPool {
    path: PathBuf,
    idle: Mutex<Vec<Reader>>,
    permits: Arc<Semaphore>,
}

impl ReadPool {
    pub fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            idle: Mutex::new(Vec::new()),
            permits: Arc::new(Semaphore::new(MAX_READERS)),
        })
    }

    fn take(&self) -> Result<Reader, Error> {
        let current = file_identity(&self.path);
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(r) = idle.pop() {
            if r.file == current {
                return Ok(r);
            }
        }
        drop(idle);
        let conn = open_read_only(&self.path)?;
        conn.execute_batch("PRAGMA query_only = 1; PRAGMA cache_size = -16384;")
            .map_err(|e| crate::store::sqlite::db_error("無法設定唯讀連線", e))?;
        Ok(Reader {
            conn,
            file: current,
        })
    }

    fn put_back(&self, reader: Reader) {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        if idle.len() < MAX_READERS {
            idle.push(reader);
        }
    }

    /// 以一條唯讀連線執行 `f`（blocking 執行緒）。`what` 是失敗時錯誤訊息中的物件（「主機清單」）
    pub async fn read<R, F>(self: &Arc<Self>, what: &'static str, f: F) -> Result<R, ApiError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, ApiError> + Send + 'static,
    {
        let _permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ApiError::internal())?;
        let pool = self.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let reader = match pool.take() {
                Ok(r) => r,
                Err(e) => {
                    warn!("主控台 API 無法開啟資料庫（{what}）: {e}");
                    return Err(ApiError::storage(what));
                }
            };
            let result = f(&reader.conn);
            pool.put_back(reader);
            result
        })
        .await;
        match joined {
            Ok(result) => result,
            Err(e) => {
                warn!("主控台 API 的查詢異常結束（{what}）: {e}");
                Err(ApiError::internal())
            }
        }
    }
}

/// 查詢失敗時記錄並轉成 `storage_unavailable`（KRS-E4003）
pub fn query_failed<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> ApiError {
    move |e| {
        warn!("主控台 API 查詢失敗（{what}）: {e}");
        ApiError::storage(what)
    }
}

/// 低頻寫入用的連線（只由背景工作使用）。只經由 SQLite 開啟既有的資料庫
/// （[`open_existing_read_write`]：在 Kairos 行程內以 `std::fs` 開關資料庫檔會放掉 writer 的鎖）
pub struct WriteConn {
    opts: SqliteOptions,
    busy: Duration,
    conn: Mutex<Option<Connection>>,
}

/// 使用者管理、登入、稽核等互動寫入的 busy 等待上限（比背景工作長：使用者正在等結果）
pub const ADMIN_WRITE_BUSY: Duration = Duration::from_secs(2);

impl WriteConn {
    pub fn new(opts: SqliteOptions) -> Arc<Self> {
        Self::with_busy(opts, WRITE_BUSY)
    }

    pub fn with_busy(opts: SqliteOptions, busy: Duration) -> Arc<Self> {
        Arc::new(Self {
            opts,
            busy,
            conn: Mutex::new(None),
        })
    }

    /// 在 blocking 執行緒上以寫入連線執行 `f`；失敗時關閉連線，下次重新開啟
    pub async fn write<R, F>(self: &Arc<Self>, f: F) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> StoreResult<R> + Send + 'static,
    {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = this.conn.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                *guard = Some(
                    open_existing_read_write(&this.opts.path, this.busy)
                        .map_err(|e| e.to_string())?,
                );
            }
            let conn = guard.as_ref().ok_or("資料庫未開啟")?;
            match f(conn) {
                Ok(r) => Ok(r),
                Err(e) => {
                    *guard = None;
                    Err(e.to_string())
                }
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }
}
