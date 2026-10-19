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

pub mod assets;
pub mod codec;
pub mod db;
pub mod derive;
pub mod import;
pub mod purge;
pub mod rollup;
pub mod sqlite;

pub use db::prelude;

use log::{error, info};
use std::future::Future;
use std::io::Error;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{MissedTickBehavior, interval};

use crate::config::{AppConfig, StorageBackend};
use crate::registry::{RegistrySnapshot, RestoredAgent};
use crate::storage::{
    Clock, REPORT_QUEUE_CAPACITY, RETENTION_SWEEP_INTERVAL, ReportRecord, ReportSink, WriteRequest,
    WriteStatus, report_channel, start_mirror_writer, start_report_writer,
};
use crate::throttle::{Category, LogThrottle};
use crate::throttled;

/// writer 一批最多取出的筆數（一個交易）
pub const BATCH_LIMIT: usize = 1024;

/// 維護工作（彙總寫出、保留期限、容量上限）的最短間隔：每次最多做一步（一個短交易）
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(100);

/// 主機清冊與狀態表同步的間隔
pub const INVENTORY_SYNC_INTERVAL: Duration = Duration::from_secs(30);

/// durable report 的取樣時間（`ts`）最多可以早於收到的時間多久：Pulse `buffer.max_age_hours` 的上限
/// （1 年）再加 2 天餘裕。更早的視為 client 時鐘錯誤，改以收到的時間保存與彙總
pub const MAX_SAMPLE_AGE: Duration = Duration::from_secs(367 * 86_400);

/// durable report 的取樣時間最多可以晚於收到的時間多久（client 時鐘偏快）；更晚的改用收到的時間
pub const MAX_SAMPLE_AHEAD: Duration = Duration::from_secs(5 * 60);

/// report 的取樣時間（Unix 毫秒）：SQLite 的 `reports.ts`，決定彙總的桶、保留期限與匯出的範圍。
///
/// - durable report（帶去重鍵，可能是補送的歷史資料）：client 的 `ts`，須為 RFC 3339 且落在收到時間的
///   [−[`MAX_SAMPLE_AGE`], +[`MAX_SAMPLE_AHEAD`]] 之內；
/// - 其他 report、或 `ts` 無法解析、超出範圍：收到的時間（與 0.2 的 JSONL 相同，不受 client 時鐘影響）。
///
/// client 原本的 `ts` 字串另外原樣保存（`reports.agent_ts`），收到的時間由 `recv_lag` 還原
pub fn sample_time_ms(record: &ReportRecord) -> i64 {
    let recv = record.recv_at.timestamp_millis();
    if record.dedup.is_none() {
        return recv;
    }
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&record.ts) else {
        return recv;
    };
    let ts = ts.timestamp_millis();
    let age = i64::try_from(MAX_SAMPLE_AGE.as_millis()).unwrap_or(i64::MAX);
    let ahead = i64::try_from(MAX_SAMPLE_AHEAD.as_millis()).unwrap_or(i64::MAX);
    if ts >= recv.saturating_sub(age) && ts <= recv.saturating_add(ahead) {
        ts
    } else {
        recv
    }
}

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

    /// 保存一台主機的資產（`inventory` 訊息；METRICS.md 第 6、9.3 節）：與上一次合併、算出摘要，
    /// 內容改變時寫入變更歷史。一個交易；失敗時回傳原因（確定未保存或結果不明，連線都回
    /// `storage_unavailable`，Pulse 稍後重送）
    fn write_assets(&mut self, write: &AssetWrite) -> Result<AssetOutcome, String> {
        let _ = write;
        Err("這個儲存後端不保存主機資產".to_string())
    }

    /// 刪除一台主機的資料：做一步（一個短交易），回傳是否完成（[`purge`]；`kairos/PRIVACY.md` 第 4 節）
    fn purge_step(&mut self, job: &mut purge::PurgeJob) -> Result<bool, String> {
        let _ = job;
        Err("這個儲存後端不支援刪除主機資料".to_string())
    }
}

/// 資產寫入佇列的容量（`inventory` 每條連線最多每 60 秒一則）
pub const ASSET_QUEUE_CAPACITY: usize = 1024;

/// 一則 `inventory` 的寫入要求（已通過 `validate_inventory`）
#[derive(Debug, Clone)]
pub struct AssetWrite {
    pub agent_id: String,
    pub hostname: String,
    pub peer: String,
    /// 收集資產的時間（`inventory` 的 `ts`，Unix 毫秒；不合理時為收到的時間）
    pub collected_ms: i64,
    /// 收到的時間（Unix 毫秒）
    pub received_ms: i64,
    pub inventory: kairos_proto::Inventory,
}

/// 資產寫入的結果
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetOutcome {
    /// 這台主機第一次收到資產
    pub first: bool,
    /// 合併後的摘要與上一次不同
    pub changed: bool,
    /// 寫入變更歷史的筆數（0 或 1；第一次收到、或差異只有 IPv6 暫時位址時為 0）
    pub history: usize,
    /// 合併後的 CPU 邏輯處理器數（告警的每核心負載）
    pub logical_cores: Option<u32>,
    /// 合併後的作業系統家族（`linux`／`windows`）
    pub os_family: Option<String>,
}

/// 送往 writer 的一則資產寫入
#[derive(Debug)]
pub struct AssetRequest {
    pub write: AssetWrite,
    pub reply: oneshot::Sender<Result<AssetOutcome, String>>,
}

/// 送往 writer 的一則刪除主機資料的要求（主控台 API；[`purge`]）
#[derive(Debug)]
pub struct PurgeRequest {
    pub job: purge::PurgeJob,
    pub reply: oneshot::Sender<Result<purge::PurgeCounts, String>>,
}

/// writer 除了 report 之外處理的工作（與 report 交錯，report 優先）
#[derive(Debug)]
pub enum WriterJob {
    Asset(Box<AssetRequest>),
    Purge(Box<PurgeRequest>),
}

/// 資產寫入佇列的送出端（只有 SQLite 後端有；JSONL 時 Kairos 不宣告 `inventory`）
#[derive(Debug, Clone)]
pub struct AssetSink {
    tx: mpsc::Sender<WriterJob>,
}

/// 刪除主機資料的送出端（與資產共用 writer 的佇列；只有 SQLite 後端有）
#[derive(Debug, Clone)]
pub struct PurgeSink {
    tx: mpsc::Sender<WriterJob>,
}

/// 建立資產寫入佇列（刪除主機資料也經由同一個佇列：[`AssetSink::purge_sink`]）
pub fn asset_channel(capacity: usize) -> (AssetSink, mpsc::Receiver<WriterJob>) {
    let (tx, rx) = mpsc::channel(capacity);
    (AssetSink { tx }, rx)
}

impl PurgeSink {
    /// 交給 writer 逐步刪除並等待完成（最多 `limit`）。逾時時 writer 仍會做完（重新查詢即可確認）
    pub async fn purge(
        &self,
        job: purge::PurgeJob,
        limit: Duration,
    ) -> Result<purge::PurgeCounts, String> {
        let (reply, result) = oneshot::channel();
        let work = async {
            self.tx
                .send(WriterJob::Purge(Box::new(PurgeRequest { job, reply })))
                .await
                .map_err(|_| "寫入工作已停止".to_string())?;
            result
                .await
                .map_err(|_| "寫入工作未回覆即結束（可能正在關機；重新執行可接續）".to_string())?
        };
        match tokio::time::timeout(limit, work).await {
            Ok(r) => r,
            Err(_) => Err(format!(
                "刪除未在 {} 秒內完成（仍在背景進行）",
                limit.as_secs()
            )),
        }
    }
}

impl AssetSink {
    /// 共用同一個 writer 佇列的刪除主機資料送出端
    pub fn purge_sink(&self) -> PurgeSink {
        PurgeSink {
            tx: self.tx.clone(),
        }
    }

    /// 放入佇列並等待結果，整段最多 `limit`。逾時或 writer 已停止時回傳原因（連線回
    /// `storage_unavailable`）：資產是狀態而非時序，重送不會造成重複
    pub async fn write(&self, write: AssetWrite, limit: Duration) -> Result<AssetOutcome, String> {
        let (reply, result) = oneshot::channel();
        let job = async {
            self.tx
                .send(WriterJob::Asset(Box::new(AssetRequest { write, reply })))
                .await
                .map_err(|_| "資產的寫入工作已停止".to_string())?;
            result
                .await
                .map_err(|_| "資產的寫入工作未回覆即結束".to_string())?
        };
        match tokio::time::timeout(limit, job).await {
            Ok(r) => r,
            Err(_) => Err(format!("資產的寫入未在 {} 秒內完成", limit.as_secs_f64())),
        }
    }
}

/// 取出下一則資產寫入或刪除要求；沒有佇列（或已關閉）時永遠等待
async fn next_asset(rx: &mut Option<mpsc::Receiver<WriterJob>>) -> Option<WriterJob> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
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
#[cfg(test)]
pub async fn run_store_writer<S: ReportStore>(
    store: S,
    rx: mpsc::Receiver<WriteRequest>,
    inventory: watch::Receiver<Option<Arc<RegistrySnapshot>>>,
    mirror: Option<ReportSink>,
) {
    run_store_writer_with_assets(store, rx, None, inventory, mirror).await;
}

/// [`run_store_writer`]，另處理資產（`inventory`）的寫入佇列：資產與 report 在同一個 writer 上依序寫入
/// （SQLite 只有一個寫入者），report 優先
pub async fn run_store_writer_with_assets<S: ReportStore>(
    store: S,
    mut rx: mpsc::Receiver<WriteRequest>,
    mut assets: Option<mpsc::Receiver<WriterJob>>,
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
    // 刪除主機資料的要求：依序處理，每一輪做一步（與 report 的批次、維護工作交錯）
    let mut purges: std::collections::VecDeque<PurgeRequest> = std::collections::VecDeque::new();

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
            req = next_asset(&mut assets) => match req {
                Some(WriterJob::Asset(req)) => {
                    let AssetRequest { write, reply } = *req;
                    let (s, result) = on_blocking(store, move |s| s.write_assets(&write)).await;
                    store = s;
                    let _ = reply.send(result);
                }
                Some(WriterJob::Purge(req)) => purges.push_back(*req),
                // 所有送出端都已釋放：不再等待這個佇列
                None => assets = None,
            },
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

        if let Some(mut req) = purges.pop_front() {
            let (s, (req_back, result)) = on_blocking(store, move |s| {
                let r = s.purge_step(&mut req.job);
                (req, r)
            })
            .await;
            store = s;
            req = req_back;
            match result {
                Ok(false) => purges.push_front(req),
                Ok(true) => {
                    let _ = req.reply.send(Ok(req.job.counts.clone()));
                }
                Err(e) => {
                    let _ = req.reply.send(Err(e));
                }
            }
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
    /// 資產的寫入佇列（SQLite 時才有；有值時 Kairos 宣告 `inventory`、`processes`，METRICS.md 第 7.1 節）
    pub assets: Option<AssetSink>,
    /// 刪除主機資料（SQLite 時才有；主控台 API 的 `POST /api/v1/hosts/{agent_id}/purge`）
    pub purger: Option<PurgeSink>,
}

/// 依 `storage.backend` 啟動 report 的儲存。開啟失敗（資料目錄無法寫入、資料庫損毀或版本較新）
/// 時回錯誤，Kairos 不啟動
pub async fn start(config: &AppConfig, data_dir: &Path, clock: Clock) -> Result<Storage, Error> {
    let retention_days = config.storage.retention_days;
    let max_daily_bytes = config.storage.max_daily_bytes;
    match config.storage.backend {
        StorageBackend::Jsonl => {
            info!("report 寫入 JSONL（storage.backend = \"jsonl\"）；沒有主機清冊與彙總");
            let (reports, task) = start_report_writer(
                data_dir.to_path_buf(),
                retention_days,
                max_daily_bytes,
                clock,
                RETENTION_SWEEP_INTERVAL,
            )
            .await?;
            Ok(Storage {
                reports,
                writer: Box::pin(task),
                inventory: None,
                restored: Vec::new(),
                assets: None,
                purger: None,
            })
        }
        StorageBackend::Sqlite => {
            let mirror = if config.storage.jsonl_mirror {
                let (sink, task) = start_mirror_writer(
                    data_dir.to_path_buf(),
                    retention_days,
                    max_daily_bytes,
                    clock,
                    RETENTION_SWEEP_INTERVAL,
                )
                .await?;
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
            let (assets, assets_rx) = asset_channel(ASSET_QUEUE_CAPACITY);
            let (feed, inventory_rx) = watch::channel(None);
            let (mirror_sink, mirror_task) = match mirror {
                Some((sink, task)) => (Some(sink), Some(task)),
                None => (None, None),
            };
            let writer = async move {
                run_store_writer_with_assets(store, rx, Some(assets_rx), inventory_rx, mirror_sink)
                    .await;
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
                purger: Some(assets.purge_sink()),
                assets: Some(assets),
            })
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
