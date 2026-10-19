//! 維運工作（個人資料匯出與刪除前的盤點、診斷包、健康檢查）的資料庫操作（`AdminStore`）。
//! SQLite 的實作在 `store/sqlite/admin.rs`。

#[allow(unused_imports)]
use super::db::StoreResult;

/// 維運工作的資料庫操作
pub trait AdminStore {
    /// 一台主機的個資（GDPR 第 15 條的存取請求、個資法第 3 條的查詢；`kairos.host.personal-data/v1`）。
    /// 任何資料表都沒有這台主機時為 `None`。
    ///
    /// 包含：清冊、資產與變更歷史、最新的行程快照、report 與彙總的筆數與時間範圍、告警事件與處理紀錄、
    /// 註冊申請（不含 CSR 與輪詢憑據的雜湊）、憑證的簽發紀錄（不含 DER）、與這台主機有關的稽核紀錄。
    /// SQLite 的實作以 `SELECT *` 輸出各表的欄位（BLOB 不輸出、JSON 文字展開、時間轉成 RFC 3339）
    fn personal_data(
        &self,
        agent_id: &str,
        settings: &crate::privacy::PrivacySettings,
        now_ms: i64,
    ) -> StoreResult<Option<serde_json::Value>>;

    /// 診斷包的資料庫摘要：(schema 版本, 主機數, 線上主機數)；各項讀不到時為 `None`
    fn diag_counts(&self) -> StoreResult<(Option<i64>, Option<i64>, Option<i64>)>;

    /// 資料庫檔所在的目錄（SQLite；記憶體中的資料庫為 `None`）。金鑰檔與資料庫放在同一個資料目錄時用來找金鑰
    fn database_dir(&self) -> StoreResult<Option<std::path::PathBuf>>;
}
