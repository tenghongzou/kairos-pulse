//! 資料輸出：按 UTC 日期分檔的 `reports-YYYY-MM-DD.jsonl`（單一 writer task）與 `agents.json` 快照。
//!
//! report 的寫入結果會回覆給送出它的連線：writer 把同一天的一段紀錄一次寫入並 flush 後，
//! 才逐筆透過 oneshot 回報成功或失敗，連線收到成功才回 `ack`。

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use log::{debug, error, info, warn};
use serde::{Serialize, Serializer};
use std::io::{Error, ErrorKind, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{MissedTickBehavior, interval};

use crate::registry::RegistrySnapshot;
use crate::retention::DatedFiles;

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

/// 寫入結果：`Err` 內含失敗原因
pub type WriteResult = Result<(), String>;

/// 送往 writer 的一筆寫入要求
#[derive(Debug)]
pub struct WriteRequest {
    pub record: ReportRecord,
    /// 寫入（並 flush）完成或失敗後回覆；對方已不在時忽略
    pub reply: oneshot::Sender<WriteResult>,
}

/// 資料目錄中的檔案路徑
#[derive(Debug, Clone)]
pub struct DataPaths {
    pub data_dir: PathBuf,
    pub agents: PathBuf,
    pub agents_tmp: PathBuf,
}

impl DataPaths {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            agents: data_dir.join(AGENTS_FILE),
            agents_tmp: data_dir.join(format!("{}.tmp", AGENTS_FILE)),
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

/// 建立資料目錄（啟動時呼叫，失敗即中止啟動）
pub async fn prepare_data_dir(data_dir: &Path) -> Result<DataPaths, Error> {
    fs::create_dir_all(data_dir).await.map_err(|e| {
        Error::new(
            e.kind(),
            format!("無法建立資料目錄 {}: {}", data_dir.display(), e),
        )
    })?;
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

/// 寫入前以 `fs::metadata` 檢查目前的 report 檔：檔案（或整個資料目錄）已被刪除時必須重新開檔，
/// 否則資料會寫進已刪除的檔案而遺失
pub fn needs_reopen(metadata: &Result<std::fs::Metadata, Error>) -> bool {
    matches!(metadata, Err(e) if e.kind() == ErrorKind::NotFound)
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
/// 回傳每筆是否成功編碼；單筆序列化失敗時只略過該筆，不會留下半行。
pub fn encode_batch<'a>(
    records: impl Iterator<Item = &'a ReportRecord>,
    buf: &mut Vec<u8>,
) -> Vec<bool> {
    records
        .map(|record| {
            let start = buf.len();
            match serde_json::to_writer(&mut *buf, record) {
                Ok(()) => {
                    buf.push(b'\n');
                    true
                }
                Err(e) => {
                    buf.truncate(start);
                    error!(
                        "無法序列化 report（agent {} seq={}）: {}",
                        record.agent_id, record.seq, e
                    );
                    false
                }
            }
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

    /// 送出一筆並等待寫入結果（寫入並 flush 後才回傳 `Ok`）。
    /// 佇列已關閉（writer 已停止）或 writer 未回覆即結束時回傳 `Err`。逾時由呼叫端控制。
    pub async fn write(&self, record: ReportRecord) -> WriteResult {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(WriteRequest { record, reply })
            .await
            .map_err(|_| "report 寫入工作已停止".to_string())?;
        result
            .await
            .map_err(|_| "report 寫入工作未回覆即結束".to_string())?
    }
}

/// 啟動 report 寫入工作，回傳 (handle, writer task)。
///
/// 啟動時清除過期檔案、提示舊版 reports.jsonl，並開啟今天（UTC）的檔案；
/// 開檔失敗即回錯誤，讓啟動階段就發現問題。
pub async fn start_report_writer(
    data_dir: PathBuf,
    retention_days: u32,
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
    REPORT_FILES.cleanup(&data_dir, today, retention_days).await;

    let writer = open_reports_file(&data_dir.join(REPORT_FILES.file_name(today))).await?;
    let (sink, rx) = report_channel(REPORT_QUEUE_CAPACITY);
    let state = WriterState {
        data_dir,
        retention_days,
        clock,
        current: Some((today, writer)),
        last_cleanup: today,
    };
    Ok((sink, run_report_writer(state, rx, cleanup_every)))
}

struct WriterState {
    data_dir: PathBuf,
    retention_days: u32,
    clock: Clock,
    /// 目前開啟的檔案及其日期
    current: Option<(NaiveDate, BufWriter<File>)>,
    /// 最近一次執行清除時的「今天」；切換到更新的日期時再清除一次
    last_cleanup: NaiveDate,
}

impl WriterState {
    /// 將同一天的紀錄整段寫入該日的檔案（一次 write_all + flush），再逐筆回覆結果
    async fn write_day(&mut self, date: NaiveDate, requests: Vec<WriteRequest>, buf: &mut Vec<u8>) {
        let (records, replies): (Vec<_>, Vec<_>) =
            requests.into_iter().map(|r| (r.record, r.reply)).unzip();
        let results = match self.write_records(date, &records, buf).await {
            Ok(encoded) => encoded
                .into_iter()
                .map(|ok| {
                    if ok {
                        Ok(())
                    } else {
                        Err("無法序列化這筆 report".to_string())
                    }
                })
                .collect(),
            Err(e) => vec![Err(e); records.len()],
        };
        for (reply, result) in replies.into_iter().zip(results) {
            // 連線已關閉（例如關機時）就收不到，忽略
            let _ = reply.send(result);
        }
    }

    async fn write_records(
        &mut self,
        date: NaiveDate,
        records: &[ReportRecord],
        buf: &mut Vec<u8>,
    ) -> Result<Vec<bool>, String> {
        let path = self.data_dir.join(REPORT_FILES.file_name(date));

        if self.current.as_ref().map(|(d, _)| *d) != Some(date) {
            // 換日（或跨日批次中的下一段）：關閉舊檔，必要時清除過期檔案
            if let Some((old_date, mut old)) = self.current.take()
                && let Err(e) = old.flush().await
            {
                error!("寫入 {} 失敗: {}", REPORT_FILES.file_name(old_date), e);
            }
            info!("開始寫入 {}", path.display());
            if date > self.last_cleanup {
                self.last_cleanup = date;
                REPORT_FILES
                    .cleanup(&self.data_dir, date, self.retention_days)
                    .await;
            }
        } else if needs_reopen(&fs::metadata(&path).await) {
            // 檔案或資料目錄在執行中被刪除：放棄舊的檔案控制代碼，重建後再寫
            warn!(
                "{} 已不存在（檔案或資料目錄可能被刪除），重新建立",
                path.display()
            );
            self.current = None;
        }

        if self.current.is_none() {
            match open_reports_file(&path).await {
                Ok(w) => self.current = Some((date, w)),
                Err(e) => {
                    error!("{}，{} 筆 report 未寫入", e, records.len());
                    return Err(e.to_string());
                }
            }
        }
        let Some((_, w)) = self.current.as_mut() else {
            return Err("report 檔未開啟".to_string());
        };

        // 整段先組成完整的行，再一次寫出並 flush
        buf.clear();
        let encoded = encode_batch(records.iter(), buf);
        match write_all_and_flush(w, buf).await {
            Ok(()) => {
                debug!(
                    "已寫入 {} 筆 report 至 {}",
                    encoded.iter().filter(|ok| **ok).count(),
                    path.display()
                );
                Ok(encoded)
            }
            Err(e) => {
                error!(
                    "寫入 {} 失敗（本批 {} 筆）: {}",
                    path.display(),
                    records.len(),
                    e
                );
                // 丟棄目前的 writer，下一批重新開檔（開檔時會補換行）
                self.current = None;
                Err(format!("寫入 {} 失敗: {}", path.display(), e))
            }
        }
    }

    /// 定期保留清除（不依賴是否有 report 進來）
    async fn periodic_cleanup(&mut self) {
        let today = (self.clock)().date_naive();
        REPORT_FILES
            .cleanup(&self.data_dir, today, self.retention_days)
            .await;
        if today > self.last_cleanup {
            self.last_cleanup = today;
        }
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
            }
            _ = cleanup.tick() => state.periodic_cleanup().await,
        }
    }

    if let Some((date, mut w)) = state.current.take()
        && let Err(e) = w.flush().await
    {
        error!("寫入 {} 失敗: {}", REPORT_FILES.file_name(date), e);
    }
    debug!("report 寫入工作結束");
}

async fn write_all_and_flush(w: &mut BufWriter<File>, data: &[u8]) -> Result<(), Error> {
    w.write_all(data).await?;
    w.flush().await
}

/// 將快照寫入 agents.json：先寫 `.tmp` 再 rename，避免讀取端看到寫到一半的檔案
pub async fn write_agents_snapshot(
    paths: &DataPaths,
    snapshot: &RegistrySnapshot,
) -> Result<(), Error> {
    let mut json =
        serde_json::to_vec_pretty(snapshot).map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
    json.push(b'\n');
    // 資料目錄在執行中被刪除時重建
    ensure_parent_dir(&paths.agents_tmp).await?;
    fs::write(&paths.agents_tmp, &json).await.map_err(|e| {
        Error::new(
            e.kind(),
            format!("無法寫入 {}: {}", paths.agents_tmp.display(), e),
        )
    })?;
    fs::rename(&paths.agents_tmp, &paths.agents)
        .await
        .map_err(|e| {
            Error::new(
                e.kind(),
                format!(
                    "無法將 {} 更名為 {}: {}",
                    paths.agents_tmp.display(),
                    paths.agents.display(),
                    e
                ),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::{Value, json};

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

    fn request(record: ReportRecord) -> (WriteRequest, oneshot::Receiver<WriteResult>) {
        let (reply, rx) = oneshot::channel();
        (WriteRequest { record, reply }, rx)
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
        let (sink, task) = start_report_writer(dir.clone(), 30, noon, HOUR)
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
        let (sink, task) = start_report_writer(dir.clone(), 30, noon, HOUR)
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
        let dir = temp_dir("crossday");
        fs::create_dir_all(&dir).await.unwrap();
        // 31 天前的檔案：切換到 10-10 時會依新的「今天」被清除
        fs::write(dir.join("reports-2026-09-09.jsonl"), b"old\n")
            .await
            .unwrap();
        fs::write(dir.join(LEGACY_REPORTS_FILE), b"legacy\n")
            .await
            .unwrap();

        let (sink, task) = start_report_writer(dir.clone(), 30, before_midnight, HOUR)
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
        let (sink, task) = start_report_writer(dir.clone(), 30, noon, Duration::from_millis(50))
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
    fn reopen_only_when_file_is_gone() {
        assert!(needs_reopen(&Err(Error::new(ErrorKind::NotFound, "gone"))));
        assert!(!needs_reopen(&Err(Error::new(
            ErrorKind::PermissionDenied,
            "denied"
        ))));
        let meta = std::fs::metadata(std::env::current_exe().unwrap());
        assert!(meta.is_ok());
        assert!(!needs_reopen(&meta));
    }

    #[tokio::test]
    async fn writer_recreates_deleted_data_dir() {
        let dir = temp_dir("rm");
        let path = dir.join(REPORT_FILES.file_name(noon().date_naive()));
        let (sink, task) = start_report_writer(dir.clone(), 30, noon, HOUR)
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
        let (sink, task) = start_report_writer(dir.clone(), 30, noon, HOUR)
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
        assert_eq!(encode_batch(records.iter(), &mut buf), [true, true, true]);
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
        assert_eq!(p.agents_tmp, Path::new("data").join("agents.json.tmp"));
    }
}
