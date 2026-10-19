//! 儲存層的後端中立介面（`kairos/HA.md` 第 5.2 節，L1a）。
//!
//! 呼叫端（API、CLI、告警、註冊、PKI、使用者、稽核、匯出、個資刪除、匯入）只認得這裡的型別與各領域的
//! store trait，不直接使用 rusqlite：
//!
//! - **領域 store**（trait，以「一個交易能完成的業務操作」或一個查詢為單位）：
//!   [`SettingsStore`]、[`AuditLog`]、[`UserStore`]、[`PkiStore`]、[`EnrollStore`]、[`AlertStore`]、
//!   [`HostStore`]、[`ApiQueries`]、[`AdminStore`]。trait 定義在各領域的模組（例如 `alerts/store.rs`），
//!   這裡只組合成 [`Store`]。report 的寫入路徑另有 [`super::ReportStore`]（單一 writer），不在這裡。
//! - **[`Store`]**：一條連線或一個交易能做的所有領域操作，加上開始交易（[`Store::begin`]）。
//!   呼叫端拿到的是 `&dyn Store`（連線池的 closure 參數、CLI 開啟的 [`DbHandle`]）。
//! - **[`Tx`]**：交易中的 [`Store`]；[`Tx::commit`] 提交，沒有提交就被 drop 時回復（與 rusqlite 的
//!   `Transaction` 相同）。交易的種類以 [`TxMode`] 指定：需要序列化的業務操作（`kairos/HA.md` 第 5.4 節
//!   D1～D7）一律在 [`TxMode::Immediate`] 的交易中進行，SQLite 以 `BEGIN IMMEDIATE` 取得全域寫入鎖；
//!   之後的 PostgreSQL 後端（L1b）改以條件式 UPDATE、`ON CONFLICT … RETURNING` 或列鎖達成相同的語意，
//!   呼叫端不需修改。
//! - **錯誤**：[`StoreError`]。顯示的文字與後端原本的錯誤相同（SQLite 為 rusqlite 的訊息），所以
//!   日誌、API 的錯誤訊息與 CLI 的輸出都不變。
//!
//! 目前只有 SQLite 的實作（[`super::sqlite`]：`rusqlite::Connection` 與 `rusqlite::Transaction`
//! 實作所有領域 trait，SQL 原封不動放在 `store/sqlite/*.rs`）。

use std::fmt;

pub use crate::audit::AuditLog;
pub use crate::pki::store::PkiStore;
pub use crate::users::store::UserStore;

/// 呼叫端要匯入的 trait（`use crate::store::prelude::*;`）：方法呼叫語法需要 trait 在作用域中
#[allow(unused_imports)]
pub mod prelude {
    pub use super::{
        AuditLog, DbHandle, PkiStore, Store, StoreError, StoreResult, Tx, TxMode, UserStore,
    };
}

/// 錯誤的分類（呼叫端需要依種類處理時使用；多數呼叫端只記錄並回 `storage_unavailable`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreErrorKind {
    /// 資料庫忙碌（SQLite 的 `SQLITE_BUSY`／`SQLITE_LOCKED`；PostgreSQL 的鎖等待逾時）
    Busy,
    /// 違反唯一鍵或其他限制
    Constraint,
    /// 查詢應回傳一列但沒有
    NotFound,
    /// 磁碟已滿
    Full,
    /// 其他（I/O、損毀、型別轉換等）
    Other,
}

/// 儲存層的錯誤。`Display` 與後端原本的錯誤相同
pub struct StoreError {
    kind: StoreErrorKind,
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl StoreError {
    pub fn new(
        kind: StoreErrorKind,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self {
            kind,
            source: source.into(),
        }
    }

    pub fn kind(&self) -> StoreErrorKind {
        self.kind
    }

    /// 後端原本的錯誤（SQLite 後端為 `rusqlite::Error`；只給 `store` 模組內部使用）
    pub(crate) fn source_ref(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self.source.as_ref()
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.source, f)
    }
}

impl fmt::Debug for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.source, f)
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

/// 交易的種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxMode {
    /// 一般的交易（SQLite `BEGIN`／`BEGIN DEFERRED`）：唯讀的一致快照，或不需要事先序列化的寫入
    Deferred,
    /// 序列化的寫入交易（SQLite `BEGIN IMMEDIATE`：一開始就取得寫入鎖）。`kairos/HA.md` 第 5.4 節的
    /// D1～D7 都在這種交易中進行
    Immediate,
}

/// 一條連線或一個交易能做的所有領域操作
pub trait Store: AuditLog + UserStore + PkiStore {
    /// 開始一個交易。連線上已有交易時失敗（與 SQLite 的巢狀 `BEGIN` 相同）
    fn begin(&self, mode: TxMode) -> StoreResult<Box<dyn Tx + '_>>;

    /// 目前不在交易中（自動提交模式）
    fn is_autocommit(&self) -> bool;
}

/// 交易中的 [`Store`]。沒有 [`Tx::commit`] 就被 drop 時回復
pub trait Tx: Store {
    /// 提交交易
    fn commit(self: Box<Self>) -> StoreResult<()>;
}

/// CLI 與一次性工作開啟的一條連線（[`super::open_admin`]、[`super::open_read`]）
pub type DbHandle = Box<dyn Store + Send>;
