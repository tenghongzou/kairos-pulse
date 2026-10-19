//! 主機清冊、report 與彙總的讀取、資產、資料庫本身（容量、備份）的領域介面（`kairos/HA.md` 第 5.2 節的
//! `HostStore`）。report 的寫入仍由 [`super::ReportStore`] 的單一 writer 進行，這裡只有讀取與管理用的操作。
//! SQLite 的實作在 `store/sqlite/hosts.rs`。

#[allow(unused_imports)]
use super::db::StoreResult;

/// 主機清冊、report、彙總、資產與資料庫管理
pub trait HostStore {
    /// 資料庫的頁大小與使用中的頁數（`page_count - freelist_count`）
    fn db_page_stats(&self) -> StoreResult<(i64, i64)>;

    /// 主機的 host_id（清冊沒有這台時為 `None`）
    fn host_id_of(&self, agent_id: &str) -> StoreResult<Option<i64>>;

    /// 指標趨勢用的主機資訊：(host_id, rollup_until, report_interval_ms)
    fn trend_host(&self, agent_id: &str) -> StoreResult<Option<(i64, i64, Option<i64>)>>;

    /// 這台主機這個名稱的序列：(series_id, unit, labels, derived)。v8 起以 `host_series` 只列出這台主機
    /// 用過的序列；沒有對應時（例如 v7 升級時只出現在原始資料、尚未彙總的序列）改列出這個名稱的所有序列
    fn host_series(&self, host_id: i64, name: &str)
    -> StoreResult<Vec<(i64, String, String, i64)>>;

    /// 標準序列（沒有 label、不是推算值）的 series_id
    fn standard_series_id(&self, name: &str, unit: &str) -> StoreResult<Option<i64>>;

    /// 開機時間（`unit = 's'`、沒有 label）序列的 series_id
    fn uptime_series_id(&self, name: &str) -> StoreResult<Option<i64>>;

    /// 序列的 (name, unit, labels, derived)
    fn series_info(&self, series_id: i64) -> StoreResult<Option<(String, String, String, i64)>>;

    /// 主機在 `before_ms` 之前最近一筆 report 的取樣時間
    fn report_ts_before(&self, host_id: i64, before_ms: i64) -> StoreResult<Option<i64>>;

    /// 主機最新的一筆 report：(取樣時間, 格式, 內容)
    fn latest_report(&self, host_id: i64) -> StoreResult<Option<(i64, i64, Vec<u8>)>>;

    /// 主機 [from, to) 的原始 report，依取樣時間排序：對每一筆內容是 BLOB 的呼叫 `f(取樣時間, 格式, 內容)`
    fn for_each_report(
        &self,
        host_id: i64,
        from_ms: i64,
        to_ms: i64,
        f: &mut dyn FnMut(i64, i64, &[u8]),
    ) -> StoreResult<()>;

    /// 彙總表（`rollup_5m` 或 `rollup_1h`）中主機 [from_bucket, to_bucket) 的每一列：
    /// `f(bucket, series_id, 彙總)`；`f` 只在 `wanted(series_id)` 時才讀取其餘欄位
    fn for_each_rollup(
        &self,
        table: &str,
        host_id: i64,
        from_bucket: i64,
        to_bucket: i64,
        wanted: &dyn Fn(i64) -> bool,
        f: &mut dyn FnMut(i64, i64, crate::store::rollup::Agg),
    ) -> StoreResult<()>;

    /// 主機最近（`bucket >= since_bucket`）1 小時彙總中出現過的序列
    fn recent_rollup_series(&self, host_id: i64, since_bucket: i64) -> StoreResult<Vec<i64>>;

    /// 主機的群組：清冊沒有這台時 `None`，有這台但沒有群組時 `Some(None)`
    fn host_group_of(&self, agent_id: &str) -> StoreResult<Option<Option<String>>>;

    /// 清冊中的主機編號、hello 的 `os`、開機時間
    fn host_basics(
        &self,
        agent_id: &str,
    ) -> StoreResult<Option<(i64, Option<String>, Option<i64>)>>;

    /// 主機目前的資產：(摘要雜湊, 內容, 收集時間, 收到時間, 變更時間)
    fn host_inventory(&self, host_id: i64) -> StoreResult<Option<(String, String, i64, i64, i64)>>;

    /// 資產的變更歷史：id 小於 `before` 的最新 `limit` 筆 (id, 變更時間, 變更內容)
    fn inventory_changes(
        &self,
        host_id: i64,
        before: i64,
        limit: i64,
    ) -> StoreResult<Vec<(i64, i64, String)>>;

    /// 不晚於 `at_ms` 的最新一筆行程快照：(時間, 內容)
    fn process_snapshot_at(&self, host_id: i64, at_ms: i64) -> StoreResult<Option<(i64, Vec<u8>)>>;

    /// 清冊中所有主機的 agent_id
    fn all_agent_ids(&self) -> StoreResult<Vec<String>>;

    /// 把被拒的憑證累加到主機清冊（一個交易）。每列 (agent_id, 時間, 原因, 序號, 來源 IP, 次數增量)；
    /// 只在清冊沒有更新的紀錄時寫入。回傳每列變更的筆數（0：清冊沒有這台主機或已有較新的紀錄）
    fn store_cert_rejections(
        &self,
        rows: &[(String, i64, String, Option<String>, Option<String>, i64)],
    ) -> StoreResult<Vec<usize>>;

    /// 主機的名稱：(hostname, display_name)
    fn host_names(&self, agent_id: &str) -> StoreResult<Option<(String, Option<String>)>>;

    /// 刪除主機資料的影響（試執行；`kairos/PRIVACY.md` 第 4 節）
    fn purge_plan(&self, agent_id: &str, now_ms: i64) -> StoreResult<super::purge::PurgeCounts>;

    /// 主機清冊（`kairos hosts list`）
    fn list_hosts(&self) -> StoreResult<Vec<crate::store::sqlite::HostRow>>;

    /// 匯出 report（`kairos reports export`）：`since_ms` 之後（可限定一台主機）依時間逐筆交給 `out`；回傳筆數
    fn export_reports(
        &self,
        since_ms: i64,
        agent_id: Option<&str>,
        out: &mut dyn FnMut(&crate::storage::ReportRecord) -> Result<(), std::io::Error>,
    ) -> StoreResult<u64>;

    /// 線上備份到 `dest`（`kairos db backup`；新檔，權限 0640）
    fn backup_to(&self, dest: &std::path::Path) -> StoreResult<()>;

    /// 刪除主機資料：一步一步執行到完成（`kairos hosts purge`，Kairos 停止時以獨占連線執行）
    fn purge_run_to_end(
        &self,
        job: &mut crate::store::purge::PurgeJob,
        now_ms: i64,
    ) -> StoreResult<()>;

    /// 指標轉出（OTLP）resource 的主機屬性：(agent_id, 顯示名稱, 群組, tags 的 JSON, hello 的 os, 資產的作業系統家族)
    fn host_meta_rows(
        &self,
    ) -> StoreResult<
        Vec<(
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )>,
    >;
}
