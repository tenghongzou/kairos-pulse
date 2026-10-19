//! 主控台 API 與 CLI 的跨領域查詢（`kairos/HA.md` 第 5.2 節的 `ApiQueries`）：只讀、把數張表 JOIN 起來
//! 給畫面使用的查詢。SQLite 的實作在 `store/sqlite/api.rs`。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

#[allow(unused_imports)]
use super::db::StoreResult;
use crate::api::hosts::DbHost;

/// 主控台畫面與 CLI 的跨領域查詢
pub trait ApiQueries {
    /// 憑證所屬主機的名稱與群組（主控台 S14 的「主機」「群組」欄）：序號 → (主機名稱, 群組)。主機清冊沒有這台時
    /// 用申請的主機名稱；`serial` 為 `None` 時列出所有憑證
    fn cert_hosts(
        &self,
        serial: Option<&str>,
    ) -> StoreResult<std::collections::HashMap<String, (Option<String>, Option<String>)>>;

    /// 清冊中所有主機（主機清單 S04）
    fn load_hosts(&self) -> StoreResult<Vec<DbHost>>;

    /// 清冊中的一台主機（主機詳情）
    fn load_host(&self, agent_id: &str) -> StoreResult<Option<DbHost>>;

    /// 清冊中最大的 host_id（沒有主機時 0；新主機寫入清冊時改變）
    fn max_host_id(&self) -> StoreResult<i64>;

    /// 這台主機最新（依簽發時間）一張憑證：(到期, 撤銷時間)
    fn latest_cert_of(&self, agent_id: &str) -> StoreResult<Option<(i64, Option<i64>)>>;
}
