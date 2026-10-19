//! 註冊服務的資料庫連線：一條寫入連線（交易依序進行）與數條唯讀連線（輪詢、token 查詢）。
//!
//! - 只經由 SQLite 開啟既有的資料庫（[`crate::store::open_write`]、[`crate::store::open_read`]），不以 `std::fs`
//!   開關資料庫檔（同一行程內會放掉 report writer 的 fcntl 鎖，見 `kairos/API.md` 第 7 節）。
//! - 寫入的 busy 等待上限 [`WRITE_BUSY`]：report 的 writer 每批只持有寫入鎖數毫秒，2 秒足以排到；
//!   等不到時回 `storage_unavailable`（503），Pulse 依退避重試。
//! - 所有操作都在 blocking 執行緒上進行（`spawn_blocking`），不佔用 async worker。

use log::warn;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::store::db::{DbHandle, Store};
use crate::store::sqlite::file_identity;

/// 寫入連線的 busy 等待上限
pub const WRITE_BUSY: Duration = Duration::from_secs(2);
/// 同時進行的唯讀查詢上限
pub const MAX_READERS: usize = 4;
/// 兩個寫入交易之間至少相隔的時間：report 的 writer 等待寫入鎖時（SQLite 的 busy handler 每隔數毫秒重試），
/// 註冊服務連續的寫入不會一直搶先取得鎖，`ack` 不會被拖慢（量測見 `kairos/PKI.md` 第 12 節）。
/// 註冊服務的寫入因此最多每秒約 500 個交易，遠高於 L8 的預設（每秒 50 筆申請）
pub const WRITE_SPACING: Duration = Duration::from_millis(2);

/// 資料庫無法使用（回 503 `storage_unavailable`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbError(pub String);

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 測試輔助：測試以 SQL（[`crate::store::db::Store::test_sqlite`]）準備資料時的錯誤
#[cfg(test)]
impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError(format!("sqlite: {e}"))
    }
}

impl From<crate::store::db::StoreError> for DbError {
    /// 與 rusqlite 的錯誤相同：訊息以 `sqlite:` 開頭（寫入失敗時重新開啟連線）
    fn from(e: crate::store::db::StoreError) -> Self {
        DbError(format!("sqlite: {e}"))
    }
}

/// 寫入連線與開啟時的檔案身分
type WriterSlot = Option<(DbHandle, Option<(u64, u64)>)>;

struct Reader {
    conn: DbHandle,
    file: Option<(u64, u64)>,
}

/// 寫入連線與唯讀連線池
pub struct EnrollDb {
    path: PathBuf,
    writer: Mutex<WriterSlot>,
    readers: Mutex<Vec<Reader>>,
    read_permits: Arc<Semaphore>,
}

impl EnrollDb {
    pub fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            writer: Mutex::new(None),
            readers: Mutex::new(Vec::new()),
            read_permits: Arc::new(Semaphore::new(MAX_READERS)),
        })
    }

    /// 以寫入連線執行 `f`（目前的執行緒；呼叫端必須已在 blocking 執行緒上）。失敗時關閉連線，下次重新開啟
    pub fn write_blocking<R>(
        &self,
        f: impl FnOnce(&dyn Store) -> Result<R, DbError>,
    ) -> Result<R, DbError> {
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let current = file_identity(&self.path);
        if guard.as_ref().is_some_and(|(_, file)| *file != current) {
            // 資料庫檔被換掉（writer 建立了新的資料庫）：重新開啟
            *guard = None;
        }
        if guard.is_none() {
            let conn = crate::store::open_write(&self.path, WRITE_BUSY)
                .map_err(|e| DbError(e.to_string()))?;
            *guard = Some((conn, current));
        }
        let (conn, _) = guard
            .as_mut()
            .ok_or_else(|| DbError("資料庫未開啟".into()))?;
        let result = f(conn.as_ref());
        if let Err(e) = &result
            && e.0.starts_with("sqlite:")
        {
            *guard = None;
        }
        // 持有寫入的 mutex 等待：下一個註冊服務的交易至少在這之後才開始
        std::thread::sleep(WRITE_SPACING);
        result
    }

    /// 以唯讀連線執行 `f`（目前的執行緒）
    pub fn read_blocking<R>(
        &self,
        f: impl FnOnce(&dyn Store) -> Result<R, DbError>,
    ) -> Result<R, DbError> {
        let current = file_identity(&self.path);
        let reader = {
            let mut idle = self.readers.lock().unwrap_or_else(|e| e.into_inner());
            let mut found = None;
            while let Some(r) = idle.pop() {
                if r.file == current {
                    found = Some(r);
                    break;
                }
            }
            found
        };
        let reader = match reader {
            Some(r) => r,
            None => {
                let conn =
                    crate::store::open_read(&self.path).map_err(|e| DbError(e.to_string()))?;
                Reader {
                    conn,
                    file: current,
                }
            }
        };
        let result = f(reader.conn.as_ref());
        let mut idle = self.readers.lock().unwrap_or_else(|e| e.into_inner());
        if idle.len() < MAX_READERS {
            idle.push(reader);
        }
        result
    }

    /// [`EnrollDb::write_blocking`] 的非同步版本
    pub async fn write<R, F>(self: &Arc<Self>, f: F) -> Result<R, DbError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Store) -> Result<R, DbError> + Send + 'static,
    {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.write_blocking(f))
            .await
            .unwrap_or_else(|e| {
                warn!("註冊服務的資料庫寫入異常結束: {e}");
                Err(DbError(format!("寫入異常結束: {e}")))
            })
    }

    /// [`EnrollDb::read_blocking`] 的非同步版本（同時最多 [`MAX_READERS`] 條）
    pub async fn read<R, F>(self: &Arc<Self>, f: F) -> Result<R, DbError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Store) -> Result<R, DbError> + Send + 'static,
    {
        let _permit = self
            .read_permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| DbError("查詢名額已關閉".into()))?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.read_blocking(f))
            .await
            .unwrap_or_else(|e| {
                warn!("註冊服務的資料庫查詢異常結束: {e}");
                Err(DbError(format!("查詢異常結束: {e}")))
            })
    }
}
