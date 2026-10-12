//! report 的儲存後端（審計 A1、A6、A9）。
//!
//! 連線只透過 [`ReportSink`](crate::storage::ReportSink) 送出 report、等待結果，不知道後端是什麼。
//! 後端實作 [`ReportStore`]（同步 API，一批一個交易），由 [`run_store_writer`] 這個單一 writer task
//! 驅動：從佇列取出目前所有的要求（最多 [`BATCH_LIMIT`] 筆）組成一批（group commit），在 blocking
//! 執行緒上寫入，commit 成功後才逐筆回覆，連線收到成功才回 `ack`。寫入期間新進的要求在佇列中
//! 累積，成為下一批，因此不需要刻意延遲來湊批次。
//!
//! 目前的實作是 SQLite（[`sqlite::SqliteStore`]）；舊的 JSONL 後端（`storage.backend = "jsonl"`）
//! 仍是 [`crate::storage`] 中原本的 async writer，也用來輸出 JSONL 鏡像。日後改用時序資料庫時，
//! 實作 [`ReportStore`] 即可，連線端不需修改。架構、schema 與設定見 `kairos/STORAGE.md`。

pub mod codec;
pub mod rollup;
pub mod sqlite;

use log::{error, info};
use std::future::Future;
use std::io::Error;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::time::{MissedTickBehavior, interval};

use crate::config::{AppConfig, StorageBackend};
use crate::registry::{RegistrySnapshot, RestoredAgent};
use crate::storage::{
    Clock, REPORT_QUEUE_CAPACITY, RETENTION_SWEEP_INTERVAL, ReportRecord, ReportSink, WriteRequest,
    WriteStatus, report_channel, start_report_writer,
};
use crate::throttle::{Category, LogThrottle};
use crate::throttled;

/// writer 一批最多取出的筆數（一個交易）
pub const BATCH_LIMIT: usize = 1024;

/// 維護工作（彙總寫出、保留期限、容量上限）的最短間隔：每次最多做一步（一個短交易）
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(100);

/// 主機清冊與狀態表同步的間隔
pub const INVENTORY_SYNC_INTERVAL: Duration = Duration::from_secs(30);

/// report 儲存後端：同步 API，只在 writer 的 blocking 執行緒上呼叫
pub trait ReportStore: Send + 'static {
    /// 在一個交易中寫入一批 report（呼叫端已認領每一筆），回傳每筆的結果（順序與輸入相同）：
    /// - [`WriteStatus::Written`]：已提交；[`WriteStatus::Duplicate`]：與已保存的一筆重複，未再寫入；
    /// - [`WriteStatus::Failed`]：確定未寫入（交易回復、或這一筆被拒，例如主機清冊已滿）；
    /// - [`WriteStatus::Unknown`]：提交時失敗、無法確定是否已寫入（連線不回應並關閉）。
    fn write_batch(&mut self, records: &[ReportRecord]) -> Vec<WriteStatus>;

    /// 把狀態表的快照同步到主機清冊（只寫入有變化的主機）
    fn sync_inventory(&mut self, snapshot: &RegistrySnapshot);

    /// 做一步維護工作（一個短交易）；回傳是否還有工作
    fn maintain(&mut self) -> bool;

    /// 結束前呼叫（關閉資料庫）
    fn close(&mut self);
}

/// 狀態表快照的發送端：monitor 每次寫 `agents.json` 時更新，writer 每 [`INVENTORY_SYNC_INTERVAL`]
/// 與結束前取最新的一份同步到主機清冊
pub type InventoryFeed = Arc<watch::Sender<Option<Arc<RegistrySnapshot>>>>;

/// 在 blocking 執行緒上對 `store` 執行 `f`，再把 store 交還（不佔用 async worker）。
/// `f` panic 時在這裡重新 panic，由 supervise 觸發關機；已認領而尚未回覆的要求，
/// 連線端會得到 `Unknown`（不回應並關閉連線）
async fn on_blocking<S, R, F>(store: S, f: F) -> (S, R)
where
    S: ReportStore,
    R: Send + 'static,
    F: FnOnce(&mut S) -> R + Send + 'static,
{
    let result = tokio::task::spawn_blocking(move || {
        let mut store = store;
        let r = f(&mut store);
        (store, r)
    })
    .await;
    match result {
        Ok(v) => v,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => panic!("儲存工作的 blocking 執行緒被取消: {e}"),
    }
}

/// 單一 writer task：取出一批 → 寫入（一個交易）→ 逐筆回覆 → JSONL 鏡像；
/// 期間每 [`MAINTENANCE_INTERVAL`] 做一步維護、每 [`INVENTORY_SYNC_INTERVAL`] 同步主機清冊。
/// 所有 sender 都被釋放後，取完佇列中剩餘的要求、做最後一次清冊同步並關閉資料庫才結束
pub async fn run_store_writer<S: ReportStore>(
    store: S,
    mut rx: mpsc::Receiver<WriteRequest>,
    inventory: watch::Receiver<Option<Arc<RegistrySnapshot>>>,
    mirror: Option<ReportSink>,
) {
    let mut store = store;
    let mut batch = Vec::with_capacity(BATCH_LIMIT);
    let mut tick = interval(MAINTENANCE_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_maintenance = Instant::now();
    let mut last_sync = Instant::now();
    let throttle = LogThrottle::default();

    loop {
        tokio::select! {
            // 優先處理資料，維護工作在每一批之後檢查是否到期
            biased;
            n = rx.recv_many(&mut batch, BATCH_LIMIT) => {
                // 所有 sender 都被釋放時，recv_many 會先取完佇列中剩餘的要求，之後回傳 0
                if n == 0 {
                    break;
                }
                // 連線已因逾時取消（已回 storage_unavailable）的要求不寫入
                let (records, replies): (Vec<_>, Vec<_>) = batch
                    .drain(..)
                    .filter(|r| r.ticket.claim())
                    .map(|r| (r.record, r.reply))
                    .unzip();
                if !records.is_empty() {
                    let (s, (results, records)) = on_blocking(store, move |s| {
                        let results = s.write_batch(&records);
                        (results, records)
                    })
                    .await;
                    store = s;
                    for ((reply, result), record) in replies.into_iter().zip(results).zip(records) {
                        let written = result == WriteStatus::Written;
                        // 連線已關閉（例如關機時）就收不到，忽略
                        let _ = reply.send(result);
                        if written
                            && let Some(mirror) = &mirror
                            && !mirror.try_forward(record)
                        {
                            throttled!(
                                throttle,
                                Category::StorageWrite,
                                warn,
                                "JSONL 鏡像的佇列已滿，略過已寫入 SQLite 的 report（鏡像為盡力而為，不影響 ack）"
                            );
                        }
                    }
                }
            }
            _ = tick.tick() => {}
        }

        if last_maintenance.elapsed() >= MAINTENANCE_INTERVAL {
            last_maintenance = Instant::now();
            let snapshot = if last_sync.elapsed() >= INVENTORY_SYNC_INTERVAL {
                last_sync = Instant::now();
                inventory.borrow().clone()
            } else {
                None
            };
            let (s, ()) = on_blocking(store, move |s| {
                if let Some(snapshot) = snapshot {
                    s.sync_inventory(&snapshot);
                }
                s.maintain();
            })
            .await;
            store = s;
        }
    }

    // 關機：最後一份快照（所有 agent 已標為離線）寫入清冊後關閉資料庫
    let snapshot = inventory.borrow().clone();
    let _ = on_blocking(store, move |s| {
        if let Some(snapshot) = snapshot {
            s.sync_inventory(&snapshot);
        }
        s.close();
    })
    .await;
    log::debug!("report 寫入工作結束");
}

/// 啟動後的儲存元件
pub struct Storage {
    /// 連線送出 report 的佇列
    pub reports: ReportSink,
    /// writer（含 JSONL 鏡像）；所有 [`ReportSink`] 都被釋放後，寫完剩餘的資料才結束
    pub writer: Pin<Box<dyn Future<Output = ()> + Send>>,
    /// 主機清冊的快照發送端（SQLite 時才有）
    pub inventory: Option<InventoryFeed>,
    /// 要放回狀態表的主機（SQLite 時才有）
    pub restored: Vec<RestoredAgent>,
}

/// 依 `storage.backend` 啟動 report 的儲存。開啟失敗（資料目錄無法寫入、資料庫損毀或版本較新）
/// 時回錯誤，Kairos 不啟動
pub async fn start(config: &AppConfig, data_dir: &Path, clock: Clock) -> Result<Storage, Error> {
    let jsonl = |dir: &Path| {
        start_report_writer(
            dir.to_path_buf(),
            config.storage.retention_days,
            config.storage.max_daily_bytes,
            clock,
            RETENTION_SWEEP_INTERVAL,
        )
    };
    match config.storage.backend {
        StorageBackend::Jsonl => {
            info!("report 寫入 JSONL（storage.backend = \"jsonl\"）；沒有主機清冊與彙總");
            let (reports, task) = jsonl(data_dir).await?;
            Ok(Storage {
                reports,
                writer: Box::pin(task),
                inventory: None,
                restored: Vec::new(),
            })
        }
        StorageBackend::Sqlite => {
            let mirror = if config.storage.jsonl_mirror {
                let (sink, task) = jsonl(data_dir).await?;
                info!("另外輸出 JSONL 鏡像（storage.jsonl_mirror = true）");
                Some((sink, tokio::spawn(task)))
            } else {
                None
            };
            let opts = sqlite::SqliteOptions::from_config(config, data_dir);
            let (store, restored) =
                tokio::task::spawn_blocking(move || sqlite::SqliteStore::open(opts, clock))
                    .await
                    .map_err(Error::other)??;
            let (reports, rx) = report_channel(REPORT_QUEUE_CAPACITY);
            let (feed, inventory_rx) = watch::channel(None);
            let (mirror_sink, mirror_task) = match mirror {
                Some((sink, task)) => (Some(sink), Some(task)),
                None => (None, None),
            };
            let writer = async move {
                run_store_writer(store, rx, inventory_rx, mirror_sink).await;
                // writer 結束時釋放了鏡像的 sender，鏡像寫完剩餘的資料後結束
                if let Some(task) = mirror_task
                    && let Err(e) = task.await
                {
                    error!("JSONL 鏡像的寫入工作異常結束: {e}");
                }
            };
            Ok(Storage {
                reports,
                writer: Box::pin(writer),
                inventory: Some(Arc::new(feed)),
                restored,
            })
        }
    }
}

#[cfg(test)]
mod tests;
