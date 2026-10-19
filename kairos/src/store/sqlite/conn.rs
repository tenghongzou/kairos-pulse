//! SQLite 後端的 [`Store`]／[`Tx`]：`rusqlite::Connection`（自動提交）與 `rusqlite::Transaction`
//! 實作所有領域 trait。各領域的方法只是轉呼叫 `store/sqlite/<領域>.rs` 中原本的函式（SQL 原封不動），
//! 錯誤以 [`From<rusqlite::Error>`] 轉成 [`StoreError`]（訊息不變）。

use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior};

use crate::store::db::{Store, StoreError, StoreErrorKind, StoreResult, Tx, TxMode};

/// 可以取得底層 `rusqlite::Connection` 的型別（連線本身與交易）。領域 trait 以
/// `impl<C: AsConn> XxxStore for C` 一次實作給兩者
pub trait AsConn {
    fn conn(&self) -> &Connection;
}

impl AsConn for Connection {
    fn conn(&self) -> &Connection {
        self
    }
}

impl AsConn for Transaction<'_> {
    fn conn(&self) -> &Connection {
        self
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        let kind = match &e {
            rusqlite::Error::QueryReturnedNoRows => StoreErrorKind::NotFound,
            _ => match e.sqlite_error_code() {
                Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => StoreErrorKind::Busy,
                Some(ErrorCode::ConstraintViolation) => StoreErrorKind::Constraint,
                Some(ErrorCode::DiskFull) => StoreErrorKind::Full,
                _ => StoreErrorKind::Other,
            },
        };
        StoreError::new(kind, e)
    }
}

impl StoreError {
    /// SQLite 後端原本的錯誤（其他後端或不是由 rusqlite 產生的錯誤為 `None`）
    pub(crate) fn as_sqlite(&self) -> Option<&rusqlite::Error> {
        self.source_ref().downcast_ref::<rusqlite::Error>()
    }
}

/// 開始交易：與 `Connection::transaction_with_behavior` 送出的 SQL 相同（`BEGIN DEFERRED`／`BEGIN IMMEDIATE`），
/// 只是不需要 `&mut`（巢狀時由 SQLite 回錯誤）
pub(crate) fn begin(conn: &Connection, mode: TxMode) -> rusqlite::Result<Transaction<'_>> {
    let behavior = match mode {
        TxMode::Deferred => TransactionBehavior::Deferred,
        TxMode::Immediate => TransactionBehavior::Immediate,
    };
    Transaction::new_unchecked(conn, behavior)
}

impl<C: AsConn> Store for C {
    fn begin(&self, mode: TxMode) -> StoreResult<Box<dyn Tx + '_>> {
        Ok(Box::new(begin(self.conn(), mode)?))
    }

    fn is_autocommit(&self) -> bool {
        self.conn().is_autocommit()
    }

    #[cfg(test)]
    fn test_sqlite(&self) -> &Connection {
        self.conn()
    }
}

impl Tx for Transaction<'_> {
    fn commit(self: Box<Self>) -> StoreResult<()> {
        Ok(Transaction::commit(*self)?)
    }
}

/// 把領域 trait 的方法轉呼叫到 SQLite 實作的函式：`fn 名稱(&self, 參數: 型別, …) -> 回傳 => 函式;`。
/// 函式的第一個參數是 `&Connection`，回傳 `rusqlite::Result<回傳>`
macro_rules! forward {
    ($( $(#[$m:meta])* fn $name:ident(&self $(, $arg:ident : $ty:ty)* $(,)?) -> $ret:ty => $path:path; )*) => {
        $(
            $(#[$m])*
            fn $name(&self $(, $arg: $ty)*) -> $crate::store::db::StoreResult<$ret> {
                Ok($path($crate::store::sqlite::conn::AsConn::conn(self) $(, $arg)*)?)
            }
        )*
    };
}
pub(crate) use forward;
