//! 資料輸出：按 UTC 日期分檔的 `reports-YYYY-MM-DD.jsonl`（單一 writer task）與 `agents.json` 快照。
//!
//! report 的寫入結果會回覆給送出它的連線：writer 把同一天的一段紀錄一次寫入並 flush 後，
//! 才逐筆透過 oneshot 回報成功或失敗，連線收到成功才回 `ack`。
//!
//! `storage_unavailable` 的語意是「確定未保存」，因此：
//! - 連線等待逾時後，逾時的要求絕不能再被寫入：每筆要求附有 [`WriteTicket`]，writer 編碼前先
//!   「認領」，連線逾時時先「取消」，兩者以 compare-and-swap 互斥，只有一方會成功；
//! - 寫出中途失敗時，只有確定一個 byte 都沒寫入檔案的要求才回 [`WriteStatus::Failed`]，
//!   可能已（部分）寫入的要求回 [`WriteStatus::Unknown`]（連線不回應並斷線）；
//! - 當天的檔案達到 `storage.max_daily_bytes` 時，放不下的要求不寫入，回 [`WriteStatus::Failed`]
//!   （R2-K3）。

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use futures_util::FutureExt;
use log::{debug, error, info, warn};
use serde::{Serialize, Serializer};
use std::io::{Error, ErrorKind, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{MissedTickBehavior, interval};

use crate::registry::RegistrySnapshot;
use crate::retention::{
    DateGuard, DatedFiles, GUARD_STATE_FILE, is_tmp_name_for, write_atomically,
};
use crate::throttle::{Category, LogThrottle};
use crate::throttled;

/// report 佇列容量
pub const REPORT_QUEUE_CAPACITY: usize = 1024;

/// writer task 單次最多取出的筆數
const REPORT_BATCH_LIMIT: usize = 256;

/// 除了換日之外，writer 定期執行保留清除的間隔
pub const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// 舊版（按日分檔前）的單一檔名；只用於啟動時提示，不再寫入也不會被自動刪除
pub const LEGACY_REPORTS_FILE: &str = "reports.jsonl";
pub const AGENTS_FILE: &str = "agents.json";

/// report 檔：`reports-YYYY-MM-DD.jsonl`（UTC 日期）
pub const REPORT_FILES: DatedFiles = DatedFiles {
    prefix: "reports-",
    suffix: ".jsonl",
    label: " report 檔",
};

/// 取得目前時間的函式（正式程式為 `Utc::now`，測試可注入固定時間）
pub type Clock = fn() -> DateTime<Utc>;

/// 達到每日大小上限時回給連線的原因（連線回 `storage_unavailable`）
const DAILY_LIMIT_REACHED: &str = "已達當天 report 檔的大小上限（storage.max_daily_bytes）";

/// RFC 3339 UTC 毫秒格式，例如 `2026-10-08T03:15:45.210Z`
pub fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn serialize_time<S: Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&fmt_time(*t))
}

/// reports-YYYY-MM-DD.jsonl 的一行（欄位順序即輸出順序）
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportRecord {
    /// 伺服器收到的時間；其 UTC 日期決定寫入哪個檔案
    #[serde(serialize_with = "serialize_time")]
    pub recv_at: DateTime<Utc>,
    pub peer: String,
    pub agent_id: String,
    pub hostname: String,
    pub seq: u64,
    pub ts: String,
    pub cpu_usage: f32,
    pub total_memory: u64,
    pub used_memory: u64,
}

const TICKET_PENDING: u8 = 0;
const TICKET_CLAIMED: u8 = 1;
const TICKET_CANCELLED: u8 = 2;

/// 一筆寫入要求在 writer 與連線之間的交接狀態：等待中 → 已認領（writer 將寫入）或已取消（連線已放棄）
#[derive(Debug, Clone, Default)]
pub struct WriteTicket(Arc<AtomicU8>);

impl WriteTicket {
    fn transition(&self, to: u8) -> bool {
        self.0
            .compare_exchange(TICKET_PENDING, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// writer 編碼前呼叫：成功表示可以寫入；回傳 `false` 表示連線已取消，不得寫入
    pub fn claim(&self) -> bool {
        self.transition(TICKET_CLAIMED)
    }

    /// 連線放棄等待時呼叫：成功表示 writer 保證不會寫入這筆；
    /// 回傳 `false` 表示 writer 已認領（可能已寫入或正在寫入）
    pub fn cancel(&self) -> bool {
        self.transition(TICKET_CANCELLED)
    }

    /// writer 是否已認領
    pub fn is_claimed(&self) -> bool {
        self.0.load(Ordering::Acquire) == TICKET_CLAIMED
    }
}

/// 送往 writer 的一筆寫入要求
#[derive(Debug)]
pub struct WriteRequest {
    pub record: ReportRecord,
    /// 寫入（並 flush）完成或失敗後回覆；對方已不在時忽略
    pub reply: oneshot::Sender<WriteStatus>,
    pub ticket: WriteTicket,
}

/// 已放入佇列、等待寫入結果的要求
#[derive(Debug)]
pub struct PendingWrite {
    result: oneshot::Receiver<WriteStatus>,
    ticket: WriteTicket,
}

/// 等待寫入的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteStatus {
    /// 已寫入並 flush
    Written,
    /// 確定沒有寫入任何一個 byte（內含原因）
    Failed(String),
    /// 可能已（部分）寫入：寫出中途失敗，或 writer 已認領卻未回覆就結束
    Unknown(String),
}

impl PendingWrite {
    /// 等待寫入結果。可安全地被逾時中斷，之後可再次呼叫。
    pub async fn wait(&mut self) -> WriteStatus {
        match (&mut self.result).await {
            Ok(status) => status,
            Err(_) if self.ticket.is_claimed() => {
                WriteStatus::Unknown("report 寫入工作未回覆即結束".to_string())
            }
            Err(_) => WriteStatus::Failed("report 寫入工作未處理即結束".to_string()),
        }
    }

    /// 放棄等待；見 [`WriteTicket::cancel`]
    pub fn cancel(&self) -> bool {
        self.ticket.cancel()
    }
}

/// 資料目錄中的檔案路徑
#[derive(Debug, Clone)]
pub struct DataPaths {
    pub data_dir: PathBuf,
    pub agents: PathBuf,
}

impl DataPaths {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            agents: data_dir.join(AGENTS_FILE),
        }
    }
}

/// 將一批要求依 recv_at 的 UTC 日期切成連續的段落（保持原順序），跨日的批次會被正確拆開
pub fn split_by_date(
    requests: impl IntoIterator<Item = WriteRequest>,
) -> Vec<(NaiveDate, Vec<WriteRequest>)> {
    let mut groups: Vec<(NaiveDate, Vec<WriteRequest>)> = Vec::new();
    for request in requests {
        let date = request.record.recv_at.date_naive();
        match groups.last_mut() {
            Some((d, group)) if *d == date => group.push(request),
            _ => groups.push((date, vec![request])),
        }
    }
    groups
}

/// 建立資料目錄（啟動時呼叫，失敗即中止啟動），並刪除先前異常結束時留下的暫存檔
pub async fn prepare_data_dir(data_dir: &Path) -> Result<DataPaths, Error> {
    fs::create_dir_all(data_dir).await.map_err(|e| {
        Error::new(
            e.kind(),
            format!("無法建立資料目錄 {}: {}", data_dir.display(), e),
        )
    })?;
    if let Ok(mut entries) = fs::read_dir(data_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if (is_tmp_name_for(name, AGENTS_FILE) || is_tmp_name_for(name, GUARD_STATE_FILE))
                && entry.file_type().await.is_ok_and(|t| t.is_file())
            {
                let _ = fs::remove_file(entry.path()).await;
            }
        }
    }
    Ok(DataPaths::new(data_dir))
}

/// 建立檔案所在的目錄（資料目錄在執行中被刪除時重建）
async fn ensure_parent_dir(path: &Path) -> Result<(), Error> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => fs::create_dir_all(dir).await.map_err(|e| {
            Error::new(
                e.kind(),
                format!("無法建立資料目錄 {}: {}", dir.display(), e),
            )
        }),
        _ => Ok(()),
    }
}

/// 寫入前檢查目前的 report 檔路徑是否仍指向開著的檔案：檔案（或整個資料目錄）已被刪除、
/// 被換掉時必須重新開檔，否則資料會寫進已刪除的檔案而遺失，連線卻收到 `ack`。
///
/// `path_meta` 為以路徑查詢（`fs::metadata`）的結果，`open_meta` 為開著的檔案控制代碼的查詢結果。
/// 下列情況都要重新開檔（K-C3 與其延伸）：
/// - 以路徑查詢失敗，不論原因：`NotFound`（已刪除）、`PermissionDenied`（Windows 上被刪除、但仍有
///   控制代碼開著的檔案處於 delete-pending 狀態，查詢時回 `ERROR_ACCESS_DENIED`；FAT 與網路磁碟亦同）、
///   `NotADirectory`（Linux 上資料目錄被換成同名檔案時回 `ENOTDIR`），以及其他無法確認檔案仍在的錯誤。
///   無法確認時一律重開：重開成功就寫進正確的檔案；重開失敗則這批確定未寫入、回 `storage_unavailable`，
///   不會在無法確認的情況下回 `ack`；
/// - 路徑指向的不是一般檔案（例如被換成同名目錄）；
/// - 無法查詢開著的控制代碼；
/// - Unix 上路徑指向的檔案與開著的不是同一個（裝置與 inode 不同；例如檔案被刪除後又有同名的新檔案）。
///
/// 重開前會先關閉舊的控制代碼，讓刪除完成；若真的是權限問題，重開會失敗並回報錯誤。
pub fn needs_reopen(
    path_meta: &Result<std::fs::Metadata, Error>,
    open_meta: &Result<std::fs::Metadata, Error>,
) -> bool {
    match (path_meta, open_meta) {
        (Ok(path), Ok(open)) => !path.is_file() || !same_file(path, open),
        _ => true,
    }
}

/// 兩份 metadata 是否屬於同一個檔案。Unix 比較裝置與 inode；其他平台無法以穩定的 API 比較，
/// 視為相同（Windows 上被刪除的檔案在控制代碼關閉前，路徑查詢會回 `PermissionDenied`，已由上面處理）
#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    true
}

/// 檔案非空且最後一個 byte 不是 `\n` 時（先前寫到一半失敗或程式中斷），需先補上換行，
/// 避免下一筆紀錄接在殘缺的行後面
pub fn needs_newline_repair(file_len: u64, last_byte: Option<u8>) -> bool {
    file_len > 0 && last_byte != Some(b'\n')
}

/// 以附加模式開啟 report 檔（必要時先建立資料目錄）；若結尾是殘缺的行，先補上 `\n`
async fn open_reports_file(path: &Path) -> Result<BufWriter<File>, Error> {
    ensure_parent_dir(path).await?;
    let with_path = |e: Error| Error::new(e.kind(), format!("無法開啟 {}: {}", path.display(), e));
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .await
        .map_err(with_path)?;

    let len = file.metadata().await.map_err(with_path)?.len();
    let last_byte = if len > 0 {
        file.seek(SeekFrom::End(-1)).await.map_err(with_path)?;
        let mut b = [0u8; 1];
        file.read_exact(&mut b).await.map_err(with_path)?;
        Some(b[0])
    } else {
        None
    };
    if needs_newline_repair(len, last_byte) {
        warn!("{} 結尾有不完整的行，補上換行後繼續寫入", path.display());
        // 附加模式下寫入一律接在檔尾
        file.write_all(b"\n").await.map_err(with_path)?;
        file.flush().await.map_err(with_path)?;
    }
    Ok(BufWriter::new(file))
}

/// 將一段紀錄編碼成完整的 JSON Lines（每筆以 `\n` 結尾）。
/// 回傳每筆在 `buf` 中的位置；單筆序列化失敗時為 `None`（只略過該筆，不會留下半行）。
pub fn encode_batch<'a>(
    records: impl Iterator<Item = &'a ReportRecord>,
    buf: &mut Vec<u8>,
) -> Vec<Option<Range<usize>>> {
    records
        .map(|record| {
            let start = buf.len();
            match serde_json::to_writer(&mut *buf, record) {
                Ok(()) => {
                    buf.push(b'\n');
                    Some(start..buf.len())
                }
                Err(e) => {
                    buf.truncate(start);
                    error!(
                        "無法序列化 report（agent {} seq={}）: {}",
                        record.agent_id, record.seq, e
                    );
                    None
                }
            }
        })
        .collect()
}

const SERIALIZE_FAILED: &str = "無法序列化這筆 report";

/// 寫出成功時每筆的結果
fn results_after_success(encoded: &[Option<Range<usize>>]) -> Vec<WriteStatus> {
    encoded
        .iter()
        .map(|range| match range {
            Some(_) => WriteStatus::Written,
            None => WriteStatus::Failed(SERIALIZE_FAILED.to_string()),
        })
        .collect()
}

/// 寫出中途失敗時每筆的結果。`written` 為這一批確定已進入檔案的 bytes 數（`None` 表示無法得知）：
/// 範圍完全落在已寫入的 bytes 之後的紀錄確定沒有寫入（[`WriteStatus::Failed`]）；
/// 其餘可能已完整或部分寫入（[`WriteStatus::Unknown`]）。無法得知寫入多少時一律 `Unknown`。
pub fn results_after_failure(
    encoded: &[Option<Range<usize>>],
    written: Option<u64>,
    reason: &str,
) -> Vec<WriteStatus> {
    encoded
        .iter()
        .map(|range| match (range, written) {
            (None, _) => WriteStatus::Failed(SERIALIZE_FAILED.to_string()),
            (Some(r), Some(written)) if r.start as u64 >= written => {
                WriteStatus::Failed(reason.to_string())
            }
            (Some(_), _) => WriteStatus::Unknown(reason.to_string()),
        })
        .collect()
}

/// 送往 writer task 的 handle
#[derive(Debug, Clone)]
pub struct ReportSink {
    tx: mpsc::Sender<WriteRequest>,
}

/// 建立 report 佇列（容量 `capacity`）
pub fn report_channel(capacity: usize) -> (ReportSink, mpsc::Receiver<WriteRequest>) {
    let (tx, rx) = mpsc::channel(capacity);
    (ReportSink { tx }, rx)
}

impl ReportSink {
    /// 佇列中尚未被 writer 取出的筆數
    pub fn pending(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }

    /// 放入佇列（佇列滿時等待），回傳可等待結果的 [`PendingWrite`]。
    /// 佇列已關閉（writer 已停止）時回傳 `Err`。在放入前被逾時中斷時，這筆不會進入佇列。
    pub async fn submit(&self, record: ReportRecord) -> Result<PendingWrite, String> {
        let (reply, result) = oneshot::channel();
        let ticket = WriteTicket::default();
        self.tx
            .send(WriteRequest {
                record,
                reply,
                ticket: ticket.clone(),
            })
            .await
            .map_err(|_| "report 寫入工作已停止".to_string())?;
        Ok(PendingWrite { result, ticket })
    }

    /// 送出一筆並等待寫入結果（寫入並 flush 後才回傳 `Ok`）
    #[cfg(test)]
    pub async fn write(&self, record: ReportRecord) -> Result<(), String> {
        match self.submit(record).await?.wait().await {
            WriteStatus::Written => Ok(()),
            WriteStatus::Failed(e) | WriteStatus::Unknown(e) => Err(e),
        }
    }
}

/// 啟動 report 寫入工作，回傳 (handle, writer task)。
///
/// 啟動時清除過期檔案、提示舊版 reports.jsonl，並開啟今天（UTC）的檔案；
/// 開檔失敗即回錯誤，讓啟動階段就發現問題。`max_daily_bytes` 為每個日期檔案的大小上限（0 = 不限制）。
pub async fn start_report_writer(
    data_dir: PathBuf,
    retention_days: u32,
    max_daily_bytes: u64,
    clock: Clock,
    cleanup_every: Duration,
) -> Result<(ReportSink, impl std::future::Future<Output = ()>), Error> {
    let today = clock().date_naive();
    let legacy = data_dir.join(LEGACY_REPORTS_FILE);
    if fs::metadata(&legacy).await.is_ok() {
        info!(
            "發現舊版的 {}：reports 已改為按 UTC 日期分檔（reports-YYYY-MM-DD.jsonl），舊檔不會再寫入，也不會被自動刪除",
            legacy.display()
        );
    }
    // 時鐘跳躍保護的狀態以資料目錄中的狀態檔為準；沒有（或損毀）時以既有最新的 report 檔日期為基準
    // （不晚於今天，晚於今天的檔案不列入），啟動時的時鐘跳躍也能偵測到
    let baseline = REPORT_FILES.guard_baseline(REPORT_FILES.newest(&data_dir, today), today);
    let guard = DateGuard::load_or_new(
        REPORT_FILES.label,
        data_dir.join(GUARD_STATE_FILE),
        None,
        baseline,
        Instant::now(),
        clock(),
    );
    let mut state = WriterState::new(data_dir, retention_days, max_daily_bytes, clock, guard);
    state.cleanup_now().await;

    state.current = Some(OpenFile::open(&state.data_dir, today).await?);
    let (sink, rx) = report_channel(REPORT_QUEUE_CAPACITY);
    Ok((sink, run_report_writer(state, rx, cleanup_every)))
}

/// 目前開啟的 report 檔
struct OpenFile {
    date: NaiveDate,
    writer: BufWriter<File>,
    /// 檔案目前的大小（開檔時讀取，之後加上成功寫入的 bytes 數）
    len: u64,
}

impl OpenFile {
    async fn open(data_dir: &Path, date: NaiveDate) -> Result<Self, Error> {
        let path = data_dir.join(REPORT_FILES.file_name(date));
        let writer = open_reports_file(&path).await?;
        let len = file_len(&writer)
            .await
            .ok_or_else(|| Error::other(format!("無法取得 {} 的大小", path.display())))?;
        Ok(Self { date, writer, len })
    }
}

struct WriterState {
    data_dir: PathBuf,
    retention_days: u32,
    /// 每個日期檔案的大小上限（0 = 不限制）
    max_daily_bytes: u64,
    clock: Clock,
    /// 目前開啟的檔案
    current: Option<OpenFile>,
    /// 最近一次執行清除時的「今天」；寫入的紀錄切換到更新的日期時再清除一次
    last_cleanup: NaiveDate,
    /// 清除前的日期合理性檢查（時鐘往前跳時不清除）
    guard: DateGuard,
    /// 開檔／寫入失敗與達到大小上限的日誌節流（磁碟滿時每批都會失敗；R2-K12）
    throttle: LogThrottle,
    /// 測試用：下一次寫出只寫入這麼多 bytes 就回報失敗（模擬寫到一半磁碟滿）
    #[cfg(test)]
    fail_after: Option<usize>,
}

impl WriterState {
    fn new(
        data_dir: PathBuf,
        retention_days: u32,
        max_daily_bytes: u64,
        clock: Clock,
        guard: DateGuard,
    ) -> Self {
        Self {
            data_dir,
            retention_days,
            max_daily_bytes,
            clock,
            current: None,
            last_cleanup: clock().date_naive(),
            guard,
            throttle: LogThrottle::default(),
            #[cfg(test)]
            fail_after: None,
        }
    }

    /// 寫出一段資料並 flush（測試可注入寫到一半失敗）
    async fn write_out(&mut self, data: &[u8]) -> Result<(), Error> {
        let Some(file) = self.current.as_mut() else {
            return Err(Error::other("report 檔未開啟"));
        };
        #[cfg(test)]
        if let Some(n) = self.fail_after.take() {
            write_all_and_flush(&mut file.writer, &data[..n.min(data.len())]).await?;
            return Err(Error::new(ErrorKind::StorageFull, "測試注入的寫入失敗"));
        }
        write_all_and_flush(&mut file.writer, data).await
    }

    /// 將同一天的紀錄整段寫入該日的檔案（一次 write_all + flush），再逐筆回覆結果。
    /// 編碼前先認領每一筆；連線已因逾時取消（已回 `storage_unavailable`）的要求直接略過。
    async fn write_day(&mut self, date: NaiveDate, requests: Vec<WriteRequest>, buf: &mut Vec<u8>) {
        let total = requests.len();
        let (records, replies): (Vec<_>, Vec<_>) = requests
            .into_iter()
            .filter(|r| r.ticket.claim())
            .map(|r| (r.record, r.reply))
            .unzip();
        if records.len() < total {
            debug!(
                "略過 {} 筆連線已逾時放棄的 report（已回覆 storage_unavailable）",
                total - records.len()
            );
        }
        if records.is_empty() {
            return;
        }
        let results = self.write_records(date, &records, buf).await;
        for (reply, result) in replies.into_iter().zip(results) {
            // 連線已關閉（例如關機時）就收不到，忽略
            let _ = reply.send(result);
        }
    }

    /// 寫入一段同一天的紀錄，回傳每筆的結果
    async fn write_records(
        &mut self,
        date: NaiveDate,
        records: &[ReportRecord],
        buf: &mut Vec<u8>,
    ) -> Vec<WriteStatus> {
        let not_written = |reason: String| vec![WriteStatus::Failed(reason); records.len()];
        let path = self.data_dir.join(REPORT_FILES.file_name(date));

        if self.current.as_ref().map(|f| f.date) != Some(date) {
            // 換日（或跨日批次中的下一段）：關閉舊檔，必要時清除過期檔案
            if let Some(mut old) = self.current.take()
                && let Err(e) = old.writer.flush().await
            {
                error!("寫入 {} 失敗: {}", REPORT_FILES.file_name(old.date), e);
            }
            info!("開始寫入 {}", path.display());
            // 換日時清除一次；刪除與否以 writer 的時鐘（經合理性檢查）為準，不以單筆紀錄的日期決定
            if date > self.last_cleanup {
                self.cleanup_now().await;
            }
        } else if let Some(file) = self.current.as_ref()
            && needs_reopen(
                &fs::metadata(&path).await,
                &file.writer.get_ref().metadata().await,
            )
        {
            // 檔案或資料目錄在執行中被刪除：放棄舊的檔案控制代碼，重建後再寫
            warn!(
                "{} 已不存在、正被刪除或已被替換（檔案或資料目錄可能被刪除或換掉），重新開檔",
                path.display()
            );
            self.current = None;
        }

        if self.current.is_none() {
            match OpenFile::open(&self.data_dir, date).await {
                Ok(f) => self.current = Some(f),
                Err(e) => {
                    // 開檔失敗：這一批一個 byte 都還沒寫。磁碟滿或權限錯誤時每批都會失敗，節流記錄
                    throttled!(
                        self.throttle,
                        Category::StorageWrite,
                        error,
                        "{}，{} 筆 report 未寫入",
                        e,
                        records.len()
                    );
                    return not_written(e.to_string());
                }
            }
        }
        let Some(file) = self.current.as_ref() else {
            return not_written("report 檔未開啟".to_string());
        };

        // 整段先組成完整的行，再一次寫出並 flush。
        // 每批結束時緩衝區都是空的（成功時已 flush，失敗時丟棄 writer），檔案長度的變化即為這批寫入的量
        buf.clear();
        let encoded = encode_batch(records.iter(), buf);
        // 每日大小上限：只寫入放得下的前段，其餘確定未寫入
        let cutoff = daily_limit_cutoff(&encoded, buf.len(), file.len, self.max_daily_bytes);
        if cutoff < buf.len() {
            throttled!(
                self.throttle,
                Category::StorageWrite,
                error,
                "{} 已達每日大小上限 {} bytes（storage.max_daily_bytes），本批 {} 筆中有 {} 筆未寫入",
                path.display(),
                self.max_daily_bytes,
                records.len(),
                encoded
                    .iter()
                    .flatten()
                    .filter(|r| r.start >= cutoff)
                    .count()
            );
        }
        let len_before = file_len(&file.writer).await;
        let result = if cutoff == 0 {
            Ok(())
        } else {
            self.write_out(&buf[..cutoff]).await
        };
        let mut results = match result {
            Ok(()) => {
                if let Some(file) = self.current.as_mut() {
                    file.len += cutoff as u64;
                }
                if cutoff > 0 {
                    debug!(
                        "已寫入 {} 筆 report 至 {}",
                        encoded
                            .iter()
                            .flatten()
                            .filter(|r| r.start < cutoff)
                            .count(),
                        path.display()
                    );
                }
                results_after_success(&encoded)
            }
            Err(e) => {
                // 中途失敗時可能已寫入一部分：以檔案長度的變化判斷哪些紀錄確定沒寫入
                let len_after = match self.current.as_ref() {
                    Some(f) => file_len(&f.writer).await,
                    None => None,
                };
                let written = match (len_before, len_after) {
                    (Some(before), Some(after)) => after.checked_sub(before),
                    _ => None,
                };
                let results = results_after_failure(
                    &encoded,
                    written,
                    &format!("寫入 {} 失敗: {}", path.display(), e),
                );
                let unknown = results
                    .iter()
                    .filter(|r| matches!(r, WriteStatus::Unknown(_)))
                    .count();
                throttled!(
                    self.throttle,
                    Category::StorageWrite,
                    error,
                    "寫入 {} 失敗（本批 {} 筆，其中 {} 筆可能已部分寫入、結果未知）: {}",
                    path.display(),
                    records.len(),
                    unknown,
                    e
                );
                // 丟棄目前的 writer，下一批重新開檔（開檔時會補換行並重新讀取大小）
                self.current = None;
                results
            }
        };
        // 超過上限而沒有寫出的紀錄：確定未寫入
        for (result, range) in results.iter_mut().zip(&encoded) {
            if let Some(r) = range
                && r.start >= cutoff
            {
                *result = WriteStatus::Failed(DAILY_LIMIT_REACHED.to_string());
            }
        }
        results
    }

    /// 以目前時鐘的日期執行保留清除；日期比先前晚超過 2 天（時鐘可能被誤校）時跳過
    async fn cleanup_now(&mut self) {
        // 永久保留時不清除，也不需要檢查日期
        if self.retention_days == 0 {
            return;
        }
        let now_wall = (self.clock)();
        let Some(date) = self
            .guard
            .check(now_wall.date_naive(), Instant::now(), now_wall)
            .await
        else {
            return;
        };
        REPORT_FILES
            .cleanup(&self.data_dir, date, self.retention_days)
            .await;
        self.last_cleanup = self.last_cleanup.max(date);
    }
}

async fn run_report_writer(
    mut state: WriterState,
    mut rx: mpsc::Receiver<WriteRequest>,
    cleanup_every: Duration,
) {
    let mut batch = Vec::with_capacity(REPORT_BATCH_LIMIT);
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut cleanup = interval(cleanup_every);
    cleanup.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 第一次 tick 立即完成；啟動時已清除過，跳過
    cleanup.tick().await;

    loop {
        tokio::select! {
            // 優先處理資料，避免清除延遲回覆
            biased;
            n = rx.recv_many(&mut batch, REPORT_BATCH_LIMIT) => {
                // 所有 sender 都被釋放時，recv_many 會先取完佇列中剩餘的要求，之後回傳 0
                if n == 0 {
                    break;
                }
                for (date, requests) in split_by_date(batch.drain(..)) {
                    state.write_day(date, requests, &mut buf).await;
                }
                // 高負載時佇列一直有資料，上面的分支永遠先被選中：每批處理完也檢查是否該清除，
                // 避免定期清除被餓死（R2-K19）
                if cleanup.tick().now_or_never().is_some() {
                    state.cleanup_now().await;
                }
            }
            _ = cleanup.tick() => state.cleanup_now().await,
        }
    }

    if let Some(mut file) = state.current.take()
        && let Err(e) = file.writer.flush().await
    {
        error!("寫入 {} 失敗: {}", REPORT_FILES.file_name(file.date), e);
    }
    debug!("report 寫入工作結束");
}

/// 目前檔案的長度；查詢失敗時為 `None`
async fn file_len(w: &BufWriter<File>) -> Option<u64> {
    w.get_ref().metadata().await.ok().map(|m| m.len())
}

async fn write_all_and_flush(w: &mut BufWriter<File>, data: &[u8]) -> Result<(), Error> {
    w.write_all(data).await?;
    w.flush().await
}

/// 每日大小上限：`buf`（長度 `buf_len`，各筆位置為 `encoded`）中可寫入的前段長度。
/// 檔案目前為 `file_len` bytes；`max` 為 0 時不限制。只寫入完整的紀錄（第一筆放不下的紀錄起全部不寫）。
pub fn daily_limit_cutoff(
    encoded: &[Option<Range<usize>>],
    buf_len: usize,
    file_len: u64,
    max: u64,
) -> usize {
    if max == 0 {
        return buf_len;
    }
    let remaining = max.saturating_sub(file_len);
    encoded
        .iter()
        .flatten()
        .find(|r| r.end as u64 > remaining)
        .map_or(buf_len, |r| r.start)
}

/// 將快照寫入 agents.json：以隨機檔名建立暫存檔（`create_new`，不跟隨預先放置的符號連結；R2-K9）、
/// 同步到磁碟（fsync）再 rename，避免讀取端看到寫到一半的檔案，也避免當機後 rename 已生效、
/// 內容卻是空的。寫檔在 blocking 執行緒進行。
pub async fn write_agents_snapshot(
    paths: &DataPaths,
    snapshot: &RegistrySnapshot,
) -> Result<(), Error> {
    let mut json =
        serde_json::to_vec_pretty(snapshot).map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
    json.push(b'\n');
    // 資料目錄在執行中被刪除時重建
    ensure_parent_dir(&paths.agents).await?;
    let path = paths.agents.clone();
    tokio::task::spawn_blocking(move || write_atomically(&path, &[&json]))
        .await
        .map_err(Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::{Value, json};
    use std::sync::atomic::AtomicI64;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap()
    }

    /// 測試用固定時鐘：2026-10-09 12:00:00 UTC
    fn noon() -> DateTime<Utc> {
        at(2026, 10, 9, 12, 0, 0)
    }

    /// 測試用固定時鐘：2026-10-09 23:59:00 UTC
    fn before_midnight() -> DateTime<Utc> {
        at(2026, 10, 9, 23, 59, 0)
    }

    const HOUR: Duration = RETENTION_SWEEP_INTERVAL;

    fn record_at(seq: u64, recv_at: DateTime<Utc>) -> ReportRecord {
        ReportRecord {
            recv_at,
            peer: "p".into(),
            agent_id: "a".into(),
            hostname: "h".into(),
            seq,
            ts: "t".into(),
            cpu_usage: 1.0,
            total_memory: 2,
            used_memory: 1,
        }
    }

    fn record(seq: u64) -> ReportRecord {
        record_at(seq, noon())
    }

    fn request(record: ReportRecord) -> (WriteRequest, oneshot::Receiver<WriteStatus>) {
        let (reply, rx) = oneshot::channel();
        (
            WriteRequest {
                record,
                reply,
                ticket: WriteTicket::default(),
            },
            rx,
        )
    }

    /// 可由測試調整的時鐘：每個測試各用一個 static，避免平行執行時互相干擾
    macro_rules! settable_clock {
        ($name:ident, $cell:ident) => {
            static $cell: AtomicI64 = AtomicI64::new(0);
            fn $name() -> DateTime<Utc> {
                DateTime::from_timestamp($cell.load(Ordering::SeqCst), 0).unwrap()
            }
        };
    }

    fn set_clock(cell: &AtomicI64, t: DateTime<Utc>) {
        cell.store(t.timestamp(), Ordering::SeqCst);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kairos-storage-{}-{}-{}",
            tag,
            std::process::id(),
            crate::registry::next_conn_id()
        ))
    }

    async fn seqs_in(path: &Path) -> Vec<u64> {
        fs::read_to_string(path)
            .await
            .unwrap()
            .lines()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn time_format_is_rfc3339_millis_utc() {
        let t = at(2026, 10, 8, 3, 15, 45) + chrono::Duration::milliseconds(210);
        assert_eq!(fmt_time(t), "2026-10-08T03:15:45.210Z");
    }

    #[test]
    fn report_record_field_order_and_values() {
        let r = ReportRecord {
            recv_at: at(2026, 10, 8, 3, 15, 45) + chrono::Duration::milliseconds(300),
            peer: "127.0.0.1:50000".into(),
            agent_id: "id".into(),
            hostname: "web-01".into(),
            seq: 6,
            ts: "2026-10-08T03:15:45.210Z".into(),
            cpu_usage: 17.84,
            total_memory: 49152,
            used_memory: 37034,
        };
        let line = serde_json::to_string(&r).unwrap();
        assert!(line.starts_with(r#"{"recv_at":"2026-10-08T03:15:45.300Z""#));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v,
            json!({"recv_at":"2026-10-08T03:15:45.300Z","peer":"127.0.0.1:50000","agent_id":"id",
                   "hostname":"web-01","seq":6,"ts":"2026-10-08T03:15:45.210Z","cpu_usage":17.84,
                   "total_memory":49152,"used_memory":37034})
        );
    }

    #[test]
    fn report_file_names() {
        assert_eq!(
            REPORT_FILES.file_name(date(2026, 10, 9)),
            "reports-2026-10-09.jsonl"
        );
        assert_eq!(REPORT_FILES.parse(LEGACY_REPORTS_FILE), None);
    }

    #[test]
    fn batch_split_across_midnight() {
        let groups = split_by_date(vec![
            request(record_at(1, at(2026, 10, 9, 23, 59, 58))).0,
            request(record_at(
                2,
                at(2026, 10, 9, 23, 59, 59) + chrono::Duration::milliseconds(999),
            ))
            .0,
            request(record_at(3, at(2026, 10, 10, 0, 0, 0))).0,
            request(record_at(4, at(2026, 10, 10, 0, 0, 1))).0,
        ]);
        let summary: Vec<(NaiveDate, Vec<u64>)> = groups
            .iter()
            .map(|(d, g)| (*d, g.iter().map(|r| r.record.seq).collect()))
            .collect();
        assert_eq!(
            summary,
            [
                (date(2026, 10, 9), vec![1, 2]),
                (date(2026, 10, 10), vec![3, 4])
            ]
        );

        // 時鐘回撥造成日期來回時，保持順序拆成多段
        let groups = split_by_date(vec![
            request(record_at(1, at(2026, 10, 10, 0, 0, 1))).0,
            request(record_at(2, at(2026, 10, 9, 23, 59, 59))).0,
            request(record_at(3, at(2026, 10, 10, 0, 0, 2))).0,
        ]);
        assert_eq!(
            groups
                .iter()
                .map(|(d, g)| (*d, g.len()))
                .collect::<Vec<_>>(),
            [
                (date(2026, 10, 10), 1),
                (date(2026, 10, 9), 1),
                (date(2026, 10, 10), 1)
            ]
        );
        assert!(split_by_date(Vec::new()).is_empty());
        // UTC 23:00 = 台灣時間隔天 07:00，仍歸入 UTC 當天
        assert_eq!(
            split_by_date(vec![request(record_at(1, at(2026, 10, 9, 23, 0, 0))).0])[0].0,
            date(2026, 10, 9)
        );
    }

    #[tokio::test]
    async fn write_replies_ok_only_after_data_is_in_the_file() {
        let dir = temp_dir("ack");
        let (sink, task) = start_report_writer(dir.clone(), 30, 0, noon, HOUR)
            .await
            .unwrap();
        let handle = tokio::spawn(task);
        let path = dir.join(REPORT_FILES.file_name(noon().date_naive()));

        for seq in 1..=3 {
            assert_eq!(sink.write(record(seq)).await, Ok(()));
            // 回覆成功時資料必定已在檔案中
            assert_eq!(seqs_in(&path).await.last(), Some(&seq));
        }
        drop(sink);
        handle.await.unwrap();
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn write_failure_is_reported_to_the_sender() {
        let dir = temp_dir("fail");
        let (sink, task) = start_report_writer(dir.clone(), 30, 0, noon, HOUR)
            .await
            .unwrap();
        let handle = tokio::spawn(task);
        assert_eq!(sink.write(record(1)).await, Ok(()));

        // 把資料目錄換成同名的「檔案」：無法重建目錄，寫入必定失敗
        fs::remove_dir_all(&dir).await.unwrap();
        fs::write(&dir, b"not a directory").await.unwrap();
        let err = sink.write(record(2)).await.unwrap_err();
        assert!(err.contains("無法建立資料目錄"), "{err}");

        // 恢復後可以繼續寫
        fs::remove_file(&dir).await.unwrap();
        assert_eq!(sink.write(record(3)).await, Ok(()));
        let path = dir.join(REPORT_FILES.file_name(noon().date_naive()));
        assert_eq!(seqs_in(&path).await, [3]);

        drop(sink);
        handle.await.unwrap();
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn write_reports_error_when_writer_is_gone() {
        let (sink, rx) = report_channel(4);
        drop(rx);
        assert!(sink.write(record(1)).await.is_err());

        // writer 取出要求後未回覆就結束
        let (sink, mut rx) = report_channel(4);
        let waiter = tokio::spawn(async move { sink.write(record(1)).await });
        let request = rx.recv().await.unwrap();
        drop(request);
        assert!(waiter.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn writer_splits_cross_day_batch_and_replies_each() {
        settable_clock!(clock, NOW);
        set_clock(&NOW, before_midnight());
        let dir = temp_dir("crossday");
        fs::create_dir_all(&dir).await.unwrap();
        // 31 天前的檔案：切換到 10-10 時會依新的「今天」被清除
        fs::write(dir.join("reports-2026-09-09.jsonl"), b"old\n")
            .await
            .unwrap();
        fs::write(dir.join(LEGACY_REPORTS_FILE), b"legacy\n")
            .await
            .unwrap();
        // 前一天的檔案：作為啟動時的日期基準（見 DateGuard）
        fs::write(dir.join("reports-2026-10-08.jsonl"), b"{\"seq\":0}\n")
            .await
            .unwrap();

        let (sink, task) = start_report_writer(dir.clone(), 30, 0, clock, HOUR)
            .await
            .unwrap();
        // 啟動時（今天 10-09）09-09 剛好 30 天前 → 保留
        assert!(
            fs::metadata(dir.join("reports-2026-09-09.jsonl"))
                .await
                .is_ok()
        );

        // writer 啟動前先排入跨日的一批，確保同一批被拆開
        let mut waiters = Vec::new();
        for (seq, t) in [
            (1, at(2026, 10, 9, 23, 59, 59)),
            (2, at(2026, 10, 10, 0, 0, 0)),
            (3, at(2026, 10, 10, 0, 0, 5)),
        ] {
            let s = sink.clone();
            waiters.push(tokio::spawn(
                async move { s.write(record_at(seq, t)).await },
            ));
        }
        while sink.pending() < 3 {
            tokio::task::yield_now().await;
        }
        // 時鐘也跨過午夜
        set_clock(&NOW, at(2026, 10, 10, 0, 0, 6));
        drop(sink);
        task.await;
        for w in waiters {
            assert_eq!(w.await.unwrap(), Ok(()));
        }

        assert_eq!(seqs_in(&dir.join("reports-2026-10-09.jsonl")).await, [1]);
        assert_eq!(seqs_in(&dir.join("reports-2026-10-10.jsonl")).await, [2, 3]);
        // 換日後 09-09 變成 31 天前 → 刪除；舊版檔案不碰
        assert!(
            fs::metadata(dir.join("reports-2026-09-09.jsonl"))
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(dir.join(LEGACY_REPORTS_FILE))
                .await
                .unwrap(),
            "legacy\n"
        );
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn writer_cleans_up_periodically_without_reports() {
        let dir = temp_dir("periodic");
        let (sink, task) = start_report_writer(dir.clone(), 30, 0, noon, Duration::from_millis(50))
            .await
            .unwrap();
        let handle = tokio::spawn(task);
        // 啟動後才出現的過期檔案：沒有任何 report 進來，也要在下一次定期清除時刪掉
        let old = dir.join("reports-2026-01-01.jsonl");
        let kept = dir.join("reports-2026-09-09.jsonl");
        fs::write(&old, b"x\n").await.unwrap();
        fs::write(&kept, b"x\n").await.unwrap();
        let mut deleted = false;
        for _ in 0..100 {
            if fs::metadata(&old).await.is_err() {
                deleted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(deleted, "過期檔案應被定期清除");
        assert!(fs::metadata(&kept).await.is_ok());
        drop(sink);
        handle.await.unwrap();
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[test]
    fn reopen_when_path_no_longer_refers_to_the_open_file() {
        let exe = std::env::current_exe().unwrap();
        let open = std::fs::File::open(&exe).unwrap().metadata();
        let err = |kind| Err(Error::new(kind, "x"));
        // 以路徑查詢失敗：不論原因都重開（Linux 的 NotADirectory、Windows delete-pending 的 PermissionDenied）
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::PermissionDenied,
            ErrorKind::NotADirectory,
            ErrorKind::TimedOut,
        ] {
            assert!(needs_reopen(&err(kind), &open), "{kind:?}");
        }
        // 無法查詢開著的控制代碼
        assert!(needs_reopen(
            &std::fs::metadata(&exe),
            &err(ErrorKind::Other)
        ));
        // 路徑指向目錄
        let dir = std::fs::metadata(exe.parent().unwrap());
        assert!(needs_reopen(&dir, &open));
        // 同一個檔案：不重開
        assert!(!needs_reopen(&std::fs::metadata(&exe), &open));
    }

    #[cfg(unix)]
    #[test]
    fn reopen_when_path_refers_to_a_different_file() {
        // Unix：同名的新檔案（不同 inode）不是開著的那一個
        let dir = std::env::temp_dir().join(format!(
            "kairos-storage-inode-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a");
        std::fs::write(&path, b"old").unwrap();
        let open = std::fs::File::open(&path).unwrap();
        assert!(!needs_reopen(&std::fs::metadata(&path), &open.metadata()));
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"new").unwrap();
        assert!(needs_reopen(&std::fs::metadata(&path), &open.metadata()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writer_reopens_when_report_file_is_replaced() {
        // Unix：report 檔被刪除後立即出現同名的新檔案時，寫進新檔案而不是已刪除的那一個
        let dir = temp_dir("replaced");
        let path = dir.join(REPORT_FILES.file_name(noon().date_naive()));
        let (sink, task) = start_report_writer(dir.clone(), 30, 0, noon, HOUR)
            .await
            .unwrap();
        let handle = tokio::spawn(task);
        assert_eq!(sink.write(record(1)).await, Ok(()));
        fs::remove_file(&path).await.unwrap();
        fs::write(&path, b"").await.unwrap();
        assert_eq!(sink.write(record(2)).await, Ok(()));
        assert_eq!(seqs_in(&path).await, [2]);
        drop(sink);
        handle.await.unwrap();
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn writer_recreates_deleted_data_dir() {
        let dir = temp_dir("rm");
        let path = dir.join(REPORT_FILES.file_name(noon().date_naive()));
        let (sink, task) = start_report_writer(dir.clone(), 30, 0, noon, HOUR)
            .await
            .unwrap();
        let handle = tokio::spawn(task);

        assert_eq!(sink.write(record(1)).await, Ok(()));
        // 刪除整個資料目錄後繼續寫
        fs::remove_dir_all(&dir).await.unwrap();
        assert_eq!(sink.write(record(2)).await, Ok(()));
        assert_eq!(sink.write(record(3)).await, Ok(()));
        drop(sink);
        handle.await.unwrap();

        assert_eq!(seqs_in(&path).await, [2, 3]);
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn writer_drains_queue_and_replies_after_senders_dropped() {
        let dir = temp_dir("drain");
        let (sink, task) = start_report_writer(dir.clone(), 30, 0, noon, HOUR)
            .await
            .unwrap();
        // writer 尚未啟動時先塞 500 筆，模擬關機時佇列中仍有資料
        let mut waiters = Vec::new();
        for i in 0..500 {
            let s = sink.clone();
            waiters.push(tokio::spawn(async move { s.write(record(i)).await }));
        }
        while sink.pending() < 500 {
            tokio::task::yield_now().await;
        }
        drop(sink);
        task.await;
        for w in waiters {
            assert_eq!(w.await.unwrap(), Ok(()));
        }
        let path = dir.join(REPORT_FILES.file_name(noon().date_naive()));
        assert_eq!(seqs_in(&path).await.len(), 500);
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_request_is_never_written() {
        // K-C1：連線逾時取消（已回 storage_unavailable）的要求，writer 不得寫入
        let dir = temp_dir("cancel");
        let (sink, task) = start_report_writer(dir.clone(), 30, 0, noon, HOUR)
            .await
            .unwrap();
        // writer 尚未啟動：三筆都在佇列中
        let mut p1 = sink.submit(record(1)).await.unwrap();
        let p2 = sink.submit(record(2)).await.unwrap();
        let mut p3 = sink.submit(record(3)).await.unwrap();
        assert!(p2.cancel(), "writer 尚未認領，取消必定成功");
        let handle = tokio::spawn(task);

        assert_eq!(p1.wait().await, WriteStatus::Written);
        assert_eq!(p3.wait().await, WriteStatus::Written);
        let path = dir.join(REPORT_FILES.file_name(noon().date_naive()));
        assert_eq!(seqs_in(&path).await, [1, 3]);
        // 已認領的要求不能再取消
        assert!(!p1.cancel());
        drop((p1, p2, p3));

        drop(sink);
        handle.await.unwrap();
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[test]
    fn partial_write_failure_reports_unknown_for_possibly_written_records() {
        // K-C1：寫出中途失敗時，只有確定一個 byte 都沒寫入的紀錄才是 Failed（storage_unavailable）
        let encoded = vec![Some(0..10), None, Some(10..20), Some(20..30)];
        let failed = |s: &WriteStatus| matches!(s, WriteStatus::Failed(_));
        let unknown = |s: &WriteStatus| matches!(s, WriteStatus::Unknown(_));

        // 一個 byte 都沒寫入 → 全部 Failed
        let r = results_after_failure(&encoded, Some(0), "x");
        assert!(r.iter().all(failed), "{r:?}");
        // 寫到第 15 byte：第一筆完整、第三筆部分寫入 → Unknown；第四筆確定沒寫 → Failed
        let r = results_after_failure(&encoded, Some(15), "x");
        assert!(
            unknown(&r[0]) && failed(&r[1]) && unknown(&r[2]) && failed(&r[3]),
            "{r:?}"
        );
        // 剛好寫到紀錄邊界：下一筆確定沒寫
        let r = results_after_failure(&encoded, Some(20), "x");
        assert!(unknown(&r[2]) && failed(&r[3]), "{r:?}");
        // 全部寫入但 flush 回報失敗 → 都是 Unknown（序列化失敗的除外）
        let r = results_after_failure(&encoded, Some(30), "x");
        assert!(
            unknown(&r[0]) && failed(&r[1]) && unknown(&r[2]) && unknown(&r[3]),
            "{r:?}"
        );
        // 無法得知寫入多少 → 一律 Unknown
        let r = results_after_failure(&encoded, None, "x");
        assert!(
            unknown(&r[0]) && failed(&r[1]) && unknown(&r[2]) && unknown(&r[3]),
            "{r:?}"
        );

        // 成功時：序列化失敗的那筆仍是 Failed
        let r = results_after_success(&encoded);
        assert_eq!(r[0], WriteStatus::Written);
        assert!(failed(&r[1]));
    }

    #[test]
    fn ticket_claim_and_cancel_are_exclusive() {
        let t = WriteTicket::default();
        assert!(!t.is_claimed());
        assert!(t.claim());
        assert!(t.is_claimed());
        assert!(!t.cancel());
        assert!(!t.claim());

        let t = WriteTicket::default();
        assert!(t.cancel());
        assert!(!t.claim());
        assert!(!t.is_claimed());
    }

    #[tokio::test]
    async fn dropped_request_reports_failed_or_unknown() {
        // writer 未認領就丟棄 → 確定沒有寫入
        let (sink, mut rx) = report_channel(4);
        let mut p = sink.submit(record(1)).await.unwrap();
        drop(rx.recv().await.unwrap());
        assert!(matches!(p.wait().await, WriteStatus::Failed(_)));

        // writer 認領後未回覆就結束 → 無法確定
        let mut p = sink.submit(record(2)).await.unwrap();
        let req = rx.recv().await.unwrap();
        assert!(req.ticket.claim());
        drop(req);
        assert!(matches!(p.wait().await, WriteStatus::Unknown(_)));
    }

    #[tokio::test]
    async fn forward_clock_jump_does_not_delete_reports() {
        // K-C2：時鐘被誤校到一年後，不得把所有 report 檔判定為過期而刪光
        settable_clock!(clock, NOW);
        set_clock(&NOW, noon());
        let dir = temp_dir("jump");
        fs::create_dir_all(&dir).await.unwrap();
        let old = dir.join("reports-2026-09-01.jsonl"); // 38 天前：正常時就該刪
        let recent = dir.join("reports-2026-10-08.jsonl");
        fs::write(&old, b"x\n").await.unwrap();
        fs::write(&recent, b"x\n").await.unwrap();

        let (sink, task) =
            start_report_writer(dir.clone(), 30, 0, clock, Duration::from_millis(20))
                .await
                .unwrap();
        assert!(
            fs::metadata(&old).await.is_err(),
            "正常日期下過期檔案照常刪除"
        );
        let handle = tokio::spawn(task);

        set_clock(&NOW, at(2027, 10, 9, 12, 0, 0));
        // 經過多次定期清除，以及一筆日期在未來的紀錄（換日）
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            sink.write(record_at(1, at(2030, 1, 1, 0, 0, 0))).await,
            Ok(())
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(fs::metadata(&recent).await.is_ok(), "時鐘跳躍後不得刪除");
        assert!(
            fs::metadata(dir.join(REPORT_FILES.file_name(noon().date_naive())))
                .await
                .is_ok()
        );

        drop(sink);
        handle.await.unwrap();
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn restart_during_clock_jump_does_not_delete_reports() {
        // K-C2：時鐘誤校到未來期間已建立未來日期的 report 檔，此時重新啟動，
        // 新的 writer 從狀態檔讀回時鐘跳躍的狀態，不以未來日期為基準清除
        settable_clock!(clock, NOW);
        set_clock(&NOW, noon());
        let dir = temp_dir("restart-jump");
        fs::create_dir_all(&dir).await.unwrap();
        let recent = dir.join("reports-2026-10-08.jsonl");
        fs::write(&recent, b"x\n").await.unwrap();

        let (sink, task) =
            start_report_writer(dir.clone(), 30, 0, clock, Duration::from_millis(20))
                .await
                .unwrap();
        let handle = tokio::spawn(task);
        // 跳到一年後：定期清除偵測到跳躍；未來日期的紀錄建立未來日期的檔案
        set_clock(&NOW, at(2027, 10, 9, 12, 0, 0));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            sink.write(record_at(1, at(2027, 10, 9, 12, 0, 0))).await,
            Ok(())
        );
        drop(sink);
        handle.await.unwrap();
        assert!(
            fs::metadata(dir.join("reports-2027-10-09.jsonl"))
                .await
                .is_ok()
        );
        assert!(fs::metadata(dir.join(GUARD_STATE_FILE)).await.is_ok());

        // 重新啟動（時鐘仍在未來）：沒有狀態檔時基準會是 2027-10-09 而立即清除
        let (sink, task) = start_report_writer(dir.clone(), 30, 0, clock, HOUR)
            .await
            .unwrap();
        assert!(fs::metadata(&recent).await.is_ok(), "重新啟動後不得清除");
        assert!(
            fs::metadata(dir.join(REPORT_FILES.file_name(noon().date_naive())))
                .await
                .is_ok()
        );
        drop(sink);
        task.await;

        // 對照組：刪掉狀態檔後重新啟動，以未來日期為基準就會清除（證明上面的保護來自狀態檔）
        fs::remove_file(dir.join(GUARD_STATE_FILE)).await.unwrap();
        let (sink, task) = start_report_writer(dir.clone(), 30, 0, clock, HOUR)
            .await
            .unwrap();
        assert!(fs::metadata(&recent).await.is_err());
        drop(sink);
        task.await;
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn future_record_date_alone_does_not_trigger_deletion() {
        // 不以單筆紀錄的日期決定刪除：時鐘正常、紀錄日期在未來
        let dir = temp_dir("futurerec");
        fs::create_dir_all(&dir).await.unwrap();
        let recent = dir.join("reports-2026-10-01.jsonl");
        fs::write(&recent, b"x\n").await.unwrap();
        let (sink, task) = start_report_writer(dir.clone(), 30, 0, noon, HOUR)
            .await
            .unwrap();
        let handle = tokio::spawn(task);
        assert_eq!(
            sink.write(record_at(1, at(2030, 1, 1, 0, 0, 0))).await,
            Ok(())
        );
        assert!(fs::metadata(&recent).await.is_ok());
        assert_eq!(seqs_in(&dir.join("reports-2030-01-01.jsonl")).await, [1]);
        drop(sink);
        handle.await.unwrap();
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn startup_after_long_gap_defers_cleanup() {
        // 既有最新的檔案是 10 天前：啟動時視為日期跳躍，暫不清除
        let dir = temp_dir("gap");
        fs::create_dir_all(&dir).await.unwrap();
        let ancient = dir.join("reports-2025-01-01.jsonl");
        let newest = dir.join("reports-2026-09-29.jsonl");
        fs::write(&ancient, b"x\n").await.unwrap();
        fs::write(&newest, b"x\n").await.unwrap();
        let (sink, task) = start_report_writer(dir.clone(), 30, 0, noon, HOUR)
            .await
            .unwrap();
        assert!(fs::metadata(&ancient).await.is_ok());
        drop(sink);
        task.await;
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn agents_snapshot_is_written_atomically() {
        let dir = temp_dir("snapshot");
        let paths = DataPaths::new(&dir);
        let registry = crate::registry::Registry::new();
        write_agents_snapshot(&paths, &registry.snapshot(noon()))
            .await
            .unwrap();
        let v: Value =
            serde_json::from_str(&fs::read_to_string(&paths.agents).await.unwrap()).unwrap();
        assert_eq!(v["total"], json!(0));
        // 暫存檔已更名，沒有殘留
        let mut names = Vec::new();
        let mut entries = fs::read_dir(&dir).await.unwrap();
        while let Some(e) = entries.next_entry().await.unwrap() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, [AGENTS_FILE]);
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn stale_temp_files_are_removed_at_startup() {
        let dir = temp_dir("stale-tmp");
        fs::create_dir_all(&dir).await.unwrap();
        for n in [
            "agents.json.tmp",
            "agents.json.0123456789abcdef.tmp",
            ".retention-guard.json.fedcba9876543210.tmp",
        ] {
            fs::write(dir.join(n), b"x").await.unwrap();
        }
        let keep = dir.join("agents.json.backup");
        fs::write(&keep, b"x").await.unwrap();
        prepare_data_dir(&dir).await.unwrap();
        let mut names = Vec::new();
        let mut entries = fs::read_dir(&dir).await.unwrap();
        while let Some(e) = entries.next_entry().await.unwrap() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, ["agents.json.backup"]);
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[test]
    fn daily_limit_cutoff_keeps_whole_records() {
        let encoded = vec![Some(0..10), None, Some(10..20), Some(20..30)];
        // 不限制
        assert_eq!(daily_limit_cutoff(&encoded, 30, 1_000_000, 0), 30);
        // 剩 25 bytes：前兩筆放得下，第三筆放不下 → 從第三筆的開頭截斷
        assert_eq!(daily_limit_cutoff(&encoded, 30, 75, 100), 20);
        // 剛好放得下
        assert_eq!(daily_limit_cutoff(&encoded, 30, 70, 100), 30);
        // 已滿或已超過
        assert_eq!(daily_limit_cutoff(&encoded, 30, 100, 100), 0);
        assert_eq!(daily_limit_cutoff(&encoded, 30, 200, 100), 0);
    }

    #[tokio::test]
    async fn daily_size_limit_rejects_records_that_do_not_fit() {
        // R2-K3：達到每日大小上限後回 Failed（storage_unavailable，確定未寫入），不寫入檔案
        let dir = temp_dir("daily-limit");
        let line_len = {
            let mut buf = Vec::new();
            encode_batch(std::iter::once(&record(1)), &mut buf);
            buf.len() as u64
        };
        // 上限可放 2.5 筆
        let (sink, task) = start_report_writer(dir.clone(), 30, line_len * 5 / 2, noon, HOUR)
            .await
            .unwrap();
        let handle = tokio::spawn(task);
        assert_eq!(sink.write(record(1)).await, Ok(()));
        assert_eq!(sink.write(record(2)).await, Ok(()));
        let err = sink.write(record(3)).await.unwrap_err();
        assert!(err.contains("max_daily_bytes"), "{err}");
        let path = dir.join(REPORT_FILES.file_name(noon().date_naive()));
        assert_eq!(seqs_in(&path).await, [1, 2]);
        drop(sink);
        handle.await.unwrap();

        // 重新啟動後以既有檔案大小計算，仍然拒絕
        let (sink, task) = start_report_writer(dir.clone(), 30, line_len * 5 / 2, noon, HOUR)
            .await
            .unwrap();
        let handle = tokio::spawn(task);
        assert!(sink.write(record(4)).await.is_err());

        drop(sink);
        handle.await.unwrap();
        assert_eq!(seqs_in(&path).await, [1, 2]);
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn partial_write_failure_replies_by_file_length_and_repairs_the_line() {
        // R2-T1：寫出中途失敗的整條路徑——以檔案長度的變化判定每筆的結果，下一批重新開檔並補換行
        let dir = temp_dir("partial");
        fs::create_dir_all(&dir).await.unwrap();
        let guard = DateGuard::new("report 檔", noon().date_naive(), Instant::now(), noon());
        let mut state = WriterState::new(dir.clone(), 30, 0, noon, guard);
        let date = noon().date_naive();
        let mut buf = Vec::new();
        let line_len = {
            let mut b = Vec::new();
            encode_batch(std::iter::once(&record(1)), &mut b);
            b.len()
        };

        // 第一批：3 筆，只寫入第一筆加上第二筆的前 5 bytes 就失敗
        state.fail_after = Some(line_len + 5);
        let mut replies = Vec::new();
        let mut requests = Vec::new();
        for seq in 1..=3 {
            let (req, rx) = request(record(seq));
            requests.push(req);
            replies.push(rx);
        }
        state.write_day(date, requests, &mut buf).await;
        let results: Vec<WriteStatus> = futures_util::future::join_all(replies)
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        // 第一筆完整寫入、第二筆部分寫入 → 結果未知（連線不回應並斷線）；第三筆確定沒寫 → Failed
        assert!(matches!(results[0], WriteStatus::Unknown(_)), "{results:?}");
        assert!(matches!(results[1], WriteStatus::Unknown(_)), "{results:?}");
        assert!(matches!(results[2], WriteStatus::Failed(_)), "{results:?}");
        assert!(state.current.is_none(), "失敗後丟棄 writer");

        // 第二批：重新開檔、先補換行再寫入
        let (req, rx) = request(record(4));
        state.write_day(date, vec![req], &mut buf).await;
        assert_eq!(rx.await.unwrap(), WriteStatus::Written);
        let path = dir.join(REPORT_FILES.file_name(date));
        let text = fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(lines[1].len(), 5);
        let last: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(last["seq"], 4);
        // 檔案大小的追蹤包含補上的換行
        assert_eq!(
            state.current.as_ref().unwrap().len,
            fs::metadata(&path).await.unwrap().len()
        );
        drop(state);
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[test]
    fn newline_repair_logic() {
        assert!(!needs_newline_repair(0, None));
        assert!(!needs_newline_repair(10, Some(b'\n')));
        assert!(needs_newline_repair(10, Some(b'}')));
        assert!(needs_newline_repair(1, Some(b'{')));
        // 長度非 0 卻讀不到最後一個 byte：保守起見補換行
        assert!(needs_newline_repair(5, None));
    }

    #[test]
    fn encode_batch_produces_complete_lines() {
        let mut buf = Vec::new();
        let records = [record(1), record(2), record(3)];
        let encoded = encode_batch(records.iter(), &mut buf);
        assert!(encoded.iter().all(Option::is_some));
        // 範圍首尾相接、涵蓋整個緩衝區
        assert_eq!(encoded[0].as_ref().unwrap().start, 0);
        assert_eq!(
            encoded[0].as_ref().unwrap().end,
            encoded[1].as_ref().unwrap().start
        );
        assert_eq!(encoded[2].as_ref().unwrap().end, buf.len());
        let text = String::from_utf8(buf).unwrap();
        assert!(text.ends_with('\n'));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        for (i, line) in lines.iter().enumerate() {
            let v: Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["seq"], json!(i as u64 + 1));
        }
    }

    #[tokio::test]
    async fn reopen_repairs_truncated_last_line_per_file() {
        let dir = temp_dir("repair");
        fs::create_dir_all(&dir).await.unwrap();
        let day1 = dir.join(REPORT_FILES.file_name(date(2026, 10, 9)));
        fs::write(&day1, b"{\"seq\":1}\n{\"seq\":2,\"cpu")
            .await
            .unwrap();
        let mut w = open_reports_file(&day1).await.unwrap();
        let mut buf = Vec::new();
        encode_batch(std::iter::once(&record(3)), &mut buf);
        write_all_and_flush(&mut w, &buf).await.unwrap();
        drop(w);

        let text = fs::read_to_string(&day1).await.unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], "{\"seq\":2,\"cpu");
        assert_eq!(
            serde_json::from_str::<Value>(lines[2]).unwrap()["seq"],
            json!(3)
        );

        // 結尾完整時不重複補換行
        let w = open_reports_file(&day1).await.unwrap();
        drop(w);
        assert_eq!(fs::read_to_string(&day1).await.unwrap(), text);

        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[test]
    fn data_paths_layout() {
        let p = DataPaths::new(Path::new("data"));
        assert_eq!(p.data_dir, Path::new("data"));
        assert_eq!(p.agents, Path::new("data").join("agents.json"));
    }
}
