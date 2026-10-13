//! `kairos reports import`：把 0.2.x 以前 Kairos 寫的 JSONL（`reports-YYYY-MM-DD.jsonl`）匯入 SQLite。
//!
//! 設計（`kairos/STORAGE.md`「從 JSONL 升級」）：
//!
//! - **獨占**：寫入時以 [`open_exclusive`] 開啟資料庫。Kairos 執行中時拒絕（[`ErrorKind::ResourceBusy`]），
//!   匯入期間 Kairos 也無法啟動，因此只有一個寫入者，不影響服務的 `ack` 延遲，也不會與服務記憶體中的
//!   彙總互相重複計算。`--dry-run` 只以唯讀方式開啟（Kairos 執行中也可以）。
//! - **取樣時間**：與 Kairos 當初寫入 SQLite 時相同（[`sample_time_ms`]）：durable 的行（帶 `boot_id`）
//!   為 Pulse 的 `ts`，其他為 `recv_at`。
//! - **去重鍵**：每一行以「主機＋取樣時間＋內容雜湊」（[`content_hash`]，涵蓋 `recv_at`、`seq`、`ts`、
//!   去重鍵與指標）記在 `import_keys`，同一行重複匯入（重新執行、`.jsonl` 與它的複本）只算一次。另外：
//!   durable 的行以 Kairos 的唯一索引（主機＋`boot_id`＋`report_seq`）去重；非 durable 的行若資料庫中
//!   已有完全相同的原始 report（例如 0.3 的 JSONL 鏡像），也視為重複。
//! - **保留期限**：取樣時間在原始資料保留期限內的寫入原始資料並併入彙總；超過原始資料保留期限、但仍在
//!   彙總保留期限內的只併入彙總（5 分鐘桶已過期時只併入 1 小時）；全部超過的略過並計數。
//! - **彙總**：取樣時間早於主機 `rollup_until` 的直接在同一個交易中累加進彙總表（與 Kairos 的遲到資料
//!   相同）；其他的先在記憶體中合併，每個檔案結束（或記憶體中的列太多）時寫出並推進 `rollup_until`。
//!   開始前先把資料庫中尚未寫出彙總的原始資料讀進記憶體（與 Kairos 啟動時相同），寫出時一併計入，
//!   不漏算、不重複算。匯入中斷時，已提交的原始資料由下一次 Kairos 啟動（或下一次匯入）補回彙總。
//! - **分批**：每 [`BATCH_LINES`] 行一個交易；中斷後重新執行，已提交的部分以去重鍵略過。
//! - **主機**：清冊中沒有的主機以 `source = 'import'`、離線狀態建立；既有的主機只把 `first_seen` 往前推，
//!   `last_seen`、`hostname`、`peer` 只在匯入的資料比清冊中的新時才更新，不覆蓋較新的資訊。

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, Error, ErrorKind};
use std::path::Path;
use std::time::Duration;

use super::codec::{self, AgentTs, BYTES_PER_MB, FORMAT_LEGACY, FORMAT_METRICS, canonical_labels};
use super::rollup::{Agg, BUCKET_1H_SECS, BUCKET_5M_SECS, RollupRow, aggregate, bucket_start};
use super::sample_time_ms;
use super::sqlite::{
    Caches, HOST_SOURCE_IMPORT, SqliteOptions, db_error, merge_rollup_rows, merge_rollup_rows_from,
    open_exclusive, open_read_only,
};
use crate::protocol::Metric;
use crate::storage::{DedupKey, RecordBody, ReportRecord};

/// 每個交易最多處理的行數
pub const BATCH_LINES: usize = 2_000;
/// 記憶體中尚未寫出的彙總最多這麼多列（約 80 bytes／列），超過時先寫出
const MEM_ROLLUP_LIMIT: usize = 400_000;
/// 寫出記憶體中的彙總時，每個交易最多約這麼多列（只在主機邊界切開）
const FLUSH_CHUNK_ROWS: usize = 5_000;
/// 每個檔案最多記住幾個壞行的行號與原因
pub const MAX_BAD_SAMPLES: usize = 5;
/// 一行最長的位元組數；Kairos 寫出的一行不超過協定的單行上限（64 KiB）加上伺服器的欄位
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// 開啟資料庫時等待其他連線（例如剛好在執行的 `kairos hosts list`）的時間
pub const OPEN_BUSY: Duration = Duration::from_secs(2);

/// 匯入的計數（一個檔案或全部）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// 讀取的行數（不含空行）
    pub lines_read: u64,
    /// 寫入原始資料（並併入彙總）的筆數
    pub imported_raw: u64,
    /// 超過原始資料保留期限、只併入彙總的筆數
    pub imported_rollup_only: u64,
    /// 已匯入過、或資料庫中已有相同的 report，略過
    pub duplicates: u64,
    /// 不是合法的 JSON、欄位缺漏或不合法，略過
    pub bad_lines: u64,
    /// 取樣時間超過所有保留期限，略過
    pub expired: u64,
    /// 取樣時間不在 `--since`／`--until` 的範圍內，略過
    pub out_of_range: u64,
    /// 主機清冊（`storage.max_hosts`）或指標序列數已達上限，略過
    pub rejected: u64,
}

impl Counts {
    pub fn add(&mut self, o: &Counts) {
        self.lines_read += o.lines_read;
        self.imported_raw += o.imported_raw;
        self.imported_rollup_only += o.imported_rollup_only;
        self.duplicates += o.duplicates;
        self.bad_lines += o.bad_lines;
        self.expired += o.expired;
        self.out_of_range += o.out_of_range;
        self.rejected += o.rejected;
    }

    /// 匯入（原始資料＋只進彙總）的筆數
    pub fn imported(&self) -> u64 {
        self.imported_raw + self.imported_rollup_only
    }

    fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Raw => self.imported_raw += 1,
            Outcome::RollupOnly => self.imported_rollup_only += 1,
            Outcome::Duplicate => self.duplicates += 1,
            Outcome::Rejected => self.rejected += 1,
        }
    }
}

/// 一個壞行
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BadLine {
    /// 行號（1 起算，含空行）
    pub line: u64,
    pub reason: String,
}

/// 一個檔案的結果
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileReport {
    pub counts: Counts,
    /// 前 [`MAX_BAD_SAMPLES`] 個壞行
    pub bad_samples: Vec<BadLine>,
    /// 最後一行沒有換行（檔案可能仍在寫入），已略過並計入壞行
    pub incomplete_tail: bool,
}

/// 匯入的設定
#[derive(Debug, Clone)]
pub struct ImportSettings {
    /// 試執行：唯讀，只計算會匯入的筆數
    pub dry_run: bool,
    /// 只匯入取樣時間在 [since, until) 的資料（Unix 毫秒）
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    /// 計算保留期限的「現在」（系統時鐘）
    pub now: DateTime<Utc>,
}

/// 全部結束後的資訊
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Finished {
    /// 新建立（試執行：將建立）的主機數
    pub new_hosts: u64,
    /// 資料庫使用中的大小（試執行為 `None`）
    pub used_bytes: Option<u64>,
    /// `storage.max_total_bytes`（0 = 不限制）
    pub max_total_bytes: u64,
}

impl Finished {
    /// 資料庫使用中的大小超過 `storage.max_total_bytes`：Kairos 啟動後會依寫入順序刪除最舊的原始
    /// report，而匯入的資料是最後寫入的，會比較晚被刪（先刪到較新的即時資料）
    pub fn over_capacity(&self) -> bool {
        self.max_total_bytes > 0 && self.used_bytes.is_some_and(|u| u > self.max_total_bytes)
    }
}

/// [`Importer::import_reader`] 的錯誤
#[derive(Debug)]
pub enum ImportError {
    /// 讀取來源檔失敗：這個檔案之前的批次已提交，其餘略過（可以繼續匯入其他檔案）
    Read(Error),
    /// 資料庫錯誤：中止匯入（已提交的批次保留，重新執行時以去重鍵略過）
    Db(Error),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Read(e) | ImportError::Db(e) => e.fmt(f),
        }
    }
}

/// 一行的處理結果（寫入或試執行）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Raw,
    RollupOnly,
    Duplicate,
    Rejected,
}

/// 依保留期限的分類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Raw,
    RollupOnly,
}

/// 保留期限的起點（與 Kairos 的保留清除相同：保留「今天與前 N 天」，UTC；`None` 為不依時間刪除）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cutoffs {
    /// 原始資料：取樣時間（毫秒）不早於此值
    raw_from_ms: Option<i64>,
    /// 5 分鐘、1 小時彙總：桶（秒）不早於此值
    m5_from: Option<i64>,
    h1_from: Option<i64>,
}

impl Cutoffs {
    fn new(opts: &SqliteOptions, today: NaiveDate) -> Self {
        let from = |days: u32| -> Option<i64> {
            if days == 0 {
                return None;
            }
            let d = today.checked_sub_days(chrono::Days::new(u64::from(days)))?;
            Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
        };
        Self {
            raw_from_ms: from(opts.raw_retention_days).map(|s| s * 1000),
            m5_from: from(opts.rollup_5m_retention_days),
            h1_from: from(opts.rollup_1h_retention_days),
        }
    }

    /// `None`：超過所有保留期限
    fn classify(&self, ts_ms: i64) -> Option<Class> {
        if self.raw_from_ms.is_none_or(|f| ts_ms >= f) {
            return Some(Class::Raw);
        }
        let m5 = self
            .m5_from
            .is_none_or(|f| bucket_start(ts_ms, BUCKET_5M_SECS) >= f);
        let h1 = self
            .h1_from
            .is_none_or(|f| bucket_start(ts_ms, BUCKET_1H_SECS) >= f);
        (m5 || h1).then_some(Class::RollupOnly)
    }

    /// 取樣時間早於此值（毫秒）的資料已超過所有保留期限：對應的去重鍵可以刪除
    fn expired_before_ms(&self) -> Option<i64> {
        let raw = self.raw_from_ms?;
        let m5 = self.m5_from? * 1000;
        let h1 = self.h1_from? * 1000;
        Some(raw.min(m5).min(h1))
    }
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

/// JSONL 的一行（`schema` 1／2，0.1.0 的行沒有 `schema`；durable 的行另有 `boot_id`、`report_seq`）
#[derive(Debug, Deserialize)]
struct JsonLine {
    schema: Option<u32>,
    recv_at: String,
    peer: Option<String>,
    agent_id: String,
    hostname: String,
    seq: u64,
    ts: String,
    boot_id: Option<String>,
    report_seq: Option<u64>,
    cpu_usage: Option<f64>,
    total_memory: Option<u64>,
    used_memory: Option<u64>,
    metrics: Option<Vec<Metric>>,
}

/// 解析並驗證一行（不含換行）。規則與 Kairos 收到 report 時相同；`hostname`、`peer` 中不能顯示的字元
/// 改成 `?`（較舊的 Kairos 對主機名稱的限制較寬鬆）
pub fn parse_line(line: &[u8]) -> Result<ReportRecord, String> {
    let line: JsonLine = serde_json::from_slice(line).map_err(|e| match e.classify() {
        serde_json::error::Category::Data => format!("欄位缺漏或型別錯誤（{e}）"),
        _ => "不是合法的 JSON".to_string(),
    })?;
    let schema = line.schema.unwrap_or(1);
    let recv_at = DateTime::parse_from_rfc3339(&line.recv_at)
        .map_err(|_| "recv_at 不是 RFC 3339 時間".to_string())?
        .with_timezone(&Utc);
    if !kairos_proto::is_uuid_text(&line.agent_id) {
        return Err("agent_id 不是 UUID".to_string());
    }
    let agent_id = kairos_proto::normalize_agent_id(&line.agent_id);
    let hostname =
        kairos_proto::sanitize_display_text(&line.hostname, kairos_proto::MAX_HOSTNAME_BYTES)
            .ok_or("hostname 是空的")?;
    let peer = line
        .peer
        .as_deref()
        .and_then(|p| kairos_proto::sanitize_display_text(p, 128))
        .unwrap_or_default();
    if line.ts.len() > kairos_proto::MAX_TS_BYTES
        || line.ts.chars().any(kairos_proto::is_forbidden_display_char)
    {
        return Err("ts 過長或含有控制字元".to_string());
    }
    let body = match schema {
        1 => {
            let (Some(cpu), Some(total), Some(used)) =
                (line.cpu_usage, line.total_memory, line.used_memory)
            else {
                return Err("schema 1 缺少 cpu_usage、total_memory 或 used_memory".to_string());
            };
            let cpu = kairos_proto::server_cpu_usage(cpu);
            kairos_proto::validate_report(cpu, total, used)?;
            RecordBody::Legacy {
                cpu_usage: cpu,
                total_memory: total,
                used_memory: used,
            }
        }
        2 => {
            let metrics = line.metrics.ok_or("schema 2 缺少 metrics")?;
            kairos_proto::validate_metrics(&metrics)?;
            RecordBody::Metrics { metrics }
        }
        other => return Err(format!("不支援的 schema {other}")),
    };
    let dedup = match (line.boot_id, line.report_seq) {
        (None, None) => None,
        (Some(boot_id), Some(report_seq)) => {
            let seq = kairos_proto::DurableSeq {
                boot_id,
                report_seq,
            };
            kairos_proto::validate_durable_seq(&seq)?;
            Some(DedupKey {
                boot_id: seq.boot_id.to_ascii_lowercase(),
                report_seq,
            })
        }
        _ => return Err("boot_id 與 report_seq 只有其中一個".to_string()),
    };
    Ok(
        ReportRecord::new(recv_at, peer, agent_id, hostname, line.seq, line.ts, body)
            .with_dedup(dedup),
    )
}

/// 去重鍵的內容雜湊：FNV-1a 64 位元再以 splitmix64 混合。保存在資料庫中，因此必須跨版本、跨平台
/// 穩定（不使用 `std` 的 `DefaultHasher`）；欄位以長度或固定寬度分隔，不同的內容不會拼成相同的位元組
struct StableHasher(u64);

impl StableHasher {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= u64::from(x);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    fn str(&mut self, s: &str) {
        self.u64(s.len() as u64);
        self.bytes(s.as_bytes());
    }

    fn finish(self) -> i64 {
        let mut z = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) as i64
    }
}

/// 一筆 report 的內容雜湊（不含主機與取樣時間，兩者另外放在去重鍵中）
pub fn content_hash(r: &ReportRecord) -> i64 {
    let mut h = StableHasher::new();
    h.str("kairos-import-v1");
    h.u64(r.recv_at.timestamp_millis() as u64);
    h.u64(r.seq);
    h.str(&r.ts);
    match &r.dedup {
        None => h.u64(0),
        Some(d) => {
            h.u64(1);
            h.str(&d.boot_id);
            h.u64(d.report_seq);
        }
    }
    match &r.body {
        RecordBody::Legacy {
            cpu_usage,
            total_memory,
            used_memory,
        } => {
            h.u64(1);
            h.u64(u64::from(cpu_usage.to_bits()));
            h.u64(*total_memory);
            h.u64(*used_memory);
        }
        RecordBody::Metrics { metrics } => {
            h.u64(2);
            h.u64(metrics.len() as u64);
            for m in metrics {
                h.str(&m.name);
                h.str(&m.unit);
                h.str(&canonical_labels(&m.labels));
                h.str(&m.value.to_string());
            }
        }
    }
    h.finish()
}

/// 讀取一行（不含換行），最多 `max` bytes；更長的行讀到換行為止丟棄。回傳 `None` 表示檔案結束，
/// 否則為（是否過長, 是否以換行結束）
fn read_line_capped(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<(bool, bool)>> {
    buf.clear();
    let mut too_long = false;
    let mut any = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(any.then_some((too_long, false)));
        }
        any = true;
        let (chunk, newline) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (&available[..i], Some(i + 1)),
            None => (available, None),
        };
        if !too_long {
            if buf.len() + chunk.len() > max {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        let used = newline.unwrap_or(available.len());
        reader.consume(used);
        if newline.is_some() {
            return Ok(Some((too_long, true)));
        }
    }
}

// ---------------------------------------------------------------------------
// 匯入
// ---------------------------------------------------------------------------

/// 一行解析後、等待寫入的資料
struct Parsed {
    record: ReportRecord,
    ts_ms: i64,
    hash: i64,
    class: Class,
}

/// 交易成功後才加入記憶體彙總的值：(host_id, series_id, ts_ms, 值)
type Sample = (i64, i64, i64, f64);

/// 一批中每台主機看到的資料（交易結束前更新清冊）
struct HostSeen {
    first: i64,
    last: i64,
    hostname: String,
    peer: String,
}

/// 寫入模式的狀態
struct Writer {
    conn: Connection,
    caches: Caches,
    /// agent_id → host_id
    hosts: HashMap<String, i64>,
    /// host_id → rollup_until（毫秒）
    until: HashMap<i64, i64>,
    /// 記憶體中尚未寫出的彙總：(host_id, 5 分鐘桶的秒, series_id)
    mem: BTreeMap<(i64, i64, i64), Agg>,
    buf: Vec<u8>,
}

/// 試執行（唯讀）的狀態
struct DryRun {
    conn: Option<Connection>,
    /// 資料庫有 `import_keys`（schema v2 以上）
    has_keys: bool,
    /// agent_id → 資料庫中的 host_id（`None`：清冊中沒有，將建立）
    hosts: HashMap<String, Option<i64>>,
    existing_hosts: usize,
    /// 序列 → series_id（`None`：資料庫中沒有）
    series: HashMap<(String, String, String), Option<i64>>,
    /// 將建立的主機（清冊中沒有）
    new: HashSet<String>,
    /// 這次執行中已看過的行：(agent_id 的雜湊, 取樣時間, 內容雜湊)
    seen: HashSet<(i64, i64, i64)>,
}

enum Mode {
    Write(Box<Writer>),
    DryRun(Box<DryRun>),
}

/// 匯入器：[`Importer::open`] → 每個檔案 [`Importer::import_reader`] → [`Importer::finish`]
pub struct Importer {
    mode: Mode,
    settings: ImportSettings,
    cutoffs: Cutoffs,
    max_hosts: usize,
    max_total_bytes: u64,
    new_hosts: u64,
    batch: Vec<Parsed>,
}

impl Importer {
    /// 開啟資料庫。寫入時以獨占方式開啟（必要時建立資料庫、升級 schema），Kairos 執行中回
    /// [`ErrorKind::ResourceBusy`]；試執行以唯讀方式開啟（資料庫不存在時視為空的）。
    ///
    /// 寫入時另外：刪除已超過所有保留期限的去重鍵，並把尚未寫出彙總的原始資料讀進記憶體
    pub fn open(opts: &SqliteOptions, settings: ImportSettings) -> Result<Self, Error> {
        let cutoffs = Cutoffs::new(opts, settings.now.date_naive());
        let mode = if settings.dry_run {
            Mode::DryRun(Box::new(DryRun::open(&opts.path)?))
        } else {
            Mode::Write(Box::new(Writer::open(opts, &cutoffs)?))
        };
        Ok(Self {
            mode,
            settings,
            cutoffs,
            max_hosts: opts.max_hosts,
            max_total_bytes: opts.max_total_bytes,
            new_hosts: 0,
            batch: Vec::with_capacity(BATCH_LINES),
        })
    }

    /// 匯入一個 JSONL 檔（或任何 `BufRead`）。`progress` 在每一批之後以目前的計數呼叫。
    /// 回傳 `Err` 時已提交的批次保留，重新執行時以去重鍵略過
    pub fn import_reader(
        &mut self,
        mut reader: impl BufRead,
        progress: &mut dyn FnMut(&Counts),
    ) -> Result<FileReport, ImportError> {
        let mut report = FileReport::default();
        let mut line = Vec::with_capacity(4096);
        let mut line_no: u64 = 0;
        loop {
            let (too_long, newline) = match read_line_capped(&mut reader, &mut line, MAX_LINE_BYTES)
            {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => {
                    // 尚未提交的這一批不寫入（下一個檔案不能帶著它）
                    self.batch.clear();
                    return Err(ImportError::Read(e));
                }
            };
            line_no += 1;
            let text = line.strip_suffix(b"\r").unwrap_or(&line);
            if !too_long && text.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            report.counts.lines_read += 1;
            let parsed = if too_long {
                Err(format!("超過 {MAX_LINE_BYTES} bytes"))
            } else if !newline {
                report.incomplete_tail = true;
                Err("最後一行不完整（沒有換行，檔案可能仍在寫入）".to_string())
            } else {
                parse_line(text)
            };
            let record = match parsed {
                Ok(r) => r,
                Err(reason) => {
                    report.counts.bad_lines += 1;
                    if report.bad_samples.len() < MAX_BAD_SAMPLES {
                        report.bad_samples.push(BadLine {
                            line: line_no,
                            reason,
                        });
                    }
                    continue;
                }
            };
            let ts_ms = sample_time_ms(&record);
            if self.settings.since_ms.is_some_and(|s| ts_ms < s)
                || self.settings.until_ms.is_some_and(|u| ts_ms >= u)
            {
                report.counts.out_of_range += 1;
                continue;
            }
            let Some(class) = self.cutoffs.classify(ts_ms) else {
                report.counts.expired += 1;
                continue;
            };
            let hash = content_hash(&record);
            self.batch.push(Parsed {
                record,
                ts_ms,
                hash,
                class,
            });
            if self.batch.len() >= BATCH_LINES {
                self.run_batch(&mut report.counts)
                    .map_err(ImportError::Db)?;
                progress(&report.counts);
            }
        }
        self.run_batch(&mut report.counts)
            .map_err(ImportError::Db)?;
        if let Mode::Write(w) = &mut self.mode {
            // 每個檔案結束時寫出記憶體中的彙總（推進 rollup_until），下一個檔案（之後的日期）照常累積
            w.flush_mem()
                .map_err(|e| ImportError::Db(db_error("無法寫出彙總", e)))?;
        }
        progress(&report.counts);
        Ok(report)
    }

    fn run_batch(&mut self, counts: &mut Counts) -> Result<(), Error> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.batch);
        let outcomes = match &mut self.mode {
            Mode::Write(w) => w
                .write_batch(&batch, &self.cutoffs, self.max_hosts, &mut self.new_hosts)
                .map_err(|e| db_error("寫入失敗", e))?,
            Mode::DryRun(d) => d
                .classify_batch(&batch, self.max_hosts, &mut self.new_hosts)
                .map_err(|e| db_error("無法讀取資料庫", e))?,
        };
        for o in outcomes {
            counts.record(o);
        }
        self.batch = batch;
        self.batch.clear();
        if let Mode::Write(w) = &mut self.mode
            && w.mem.len() > MEM_ROLLUP_LIMIT
        {
            w.flush_mem().map_err(|e| db_error("無法寫出彙總", e))?;
        }
        Ok(())
    }

    /// 寫出剩下的彙總並關閉資料庫（關閉時 checkpoint、刪除 `-wal`，Kairos 之後可以開啟）
    pub fn finish(self) -> Result<Finished, Error> {
        let used_bytes = match self.mode {
            Mode::Write(w) => {
                let mut w = *w;
                w.flush_mem().map_err(|e| db_error("無法寫出彙總", e))?;
                let used = w.used_bytes().ok();
                w.conn
                    .close()
                    .map_err(|(_, e)| db_error("無法關閉資料庫", e))?;
                used
            }
            Mode::DryRun(_) => None,
        };
        Ok(Finished {
            new_hosts: self.new_hosts,
            used_bytes,
            max_total_bytes: self.max_total_bytes,
        })
    }
}

/// 把 report 的內容編成 `reports.body`，並取得（必要時建立）序列。序列數已達上限時為 `None`
fn encode_body(
    caches: &mut Caches,
    tx: &Transaction,
    host_id: i64,
    p: &Parsed,
    buf: &mut Vec<u8>,
    samples: &mut Vec<Sample>,
) -> rusqlite::Result<Option<i64>> {
    buf.clear();
    samples.clear();
    let ts = p.ts_ms;
    match &p.record.body {
        RecordBody::Legacy {
            cpu_usage,
            total_memory,
            used_memory,
        } => {
            let Some([cpu, total, used]) = caches.legacy_series(tx)? else {
                return Ok(None);
            };
            codec::encode_legacy(buf, *cpu_usage, *total_memory, *used_memory);
            samples.extend([
                (host_id, cpu, ts, f64::from(*cpu_usage)),
                (host_id, total, ts, (*total_memory * BYTES_PER_MB) as f64),
                (host_id, used, ts, (*used_memory * BYTES_PER_MB) as f64),
            ]);
            Ok(Some(FORMAT_LEGACY))
        }
        RecordBody::Metrics { metrics } => {
            let mut ids = Vec::with_capacity(metrics.len());
            for m in metrics {
                let labels = canonical_labels(&m.labels);
                match caches.series_id(tx, &m.name, &m.unit, &labels)? {
                    Some(id) => ids.push(id),
                    None => return Ok(None),
                }
            }
            codec::encode_metrics(
                buf,
                ids.iter().copied().zip(metrics.iter().map(|m| &m.value)),
            );
            samples.extend(
                ids.iter()
                    .zip(metrics)
                    .map(|(&id, m)| (host_id, id, ts, m.value_f64())),
            );
            Ok(Some(FORMAT_METRICS))
        }
    }
}

impl Writer {
    fn open(opts: &SqliteOptions, cutoffs: &Cutoffs) -> Result<Self, Error> {
        let conn = open_exclusive(opts, OPEN_BUSY)?;
        let path = opts.path.display().to_string();
        let fail = |what: &str, e| db_error(&format!("{path}：{what}"), e);
        let caches = Caches::load(&conn).map_err(|e| fail("無法讀取主機與序列", e))?;
        let mut w = Self {
            conn,
            caches,
            hosts: HashMap::new(),
            until: HashMap::new(),
            mem: BTreeMap::new(),
            buf: Vec::with_capacity(4096),
        };
        {
            let mut stmt = w
                .conn
                .prepare("SELECT agent_id, host_id, rollup_until FROM hosts")
                .map_err(|e| fail("無法讀取主機清冊", e))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|e| fail("無法讀取主機清冊", e))?;
            for row in rows {
                let (agent_id, host_id, until) = row.map_err(|e| fail("無法讀取主機清冊", e))?;
                w.hosts.insert(agent_id, host_id);
                w.until.insert(host_id, until);
            }
        }
        if let Some(before) = cutoffs.expired_before_ms() {
            w.purge_keys(before)
                .map_err(|e| fail("無法刪除過期的匯入去重鍵", e))?;
        }
        w.load_unflushed()
            .map_err(|e| fail("無法讀取尚未寫出彙總的原始資料", e))?;
        Ok(w)
    }

    /// 刪除取樣時間早於 `before_ms` 的去重鍵（對應的資料已超過所有保留期限，再匯入時也會被略過）
    fn purge_keys(&mut self, before_ms: i64) -> rusqlite::Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut del = tx.prepare("DELETE FROM import_keys WHERE host_id = ?1 AND ts < ?2")?;
            for host_id in self.until.keys() {
                del.execute(params![host_id, before_ms])?;
            }
        }
        tx.commit()
    }

    /// 與 Kairos 啟動時相同：各主機從 `rollup_until` 起的原始資料尚未寫出彙總，讀進記憶體，
    /// 之後與匯入的資料一起寫出（推進 `rollup_until` 時才不會漏算）
    fn load_unflushed(&mut self) -> rusqlite::Result<()> {
        let Self {
            conn,
            caches,
            until,
            mem,
            ..
        } = self;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut stmt =
                tx.prepare("SELECT ts, format, body FROM reports WHERE host_id = ?1 AND ts >= ?2")?;
            for (&host_id, &from) in until.iter() {
                let mut rows = stmt.query(params![host_id, from])?;
                while let Some(r) = rows.next()? {
                    let ts: i64 = r.get(0)?;
                    let body: Vec<u8> = r.get(2)?;
                    match r.get::<_, i64>(1)? {
                        FORMAT_LEGACY => {
                            if let (Some((cpu, total, used)), Some([c, t, u])) =
                                (codec::decode_legacy(&body), caches.legacy_series(&tx)?)
                            {
                                add_mem(mem, (host_id, c, ts, f64::from(cpu)));
                                add_mem(mem, (host_id, t, ts, (total * BYTES_PER_MB) as f64));
                                add_mem(mem, (host_id, u, ts, (used * BYTES_PER_MB) as f64));
                            }
                        }
                        FORMAT_METRICS => {
                            for (series_id, value) in
                                codec::decode_metrics(&body).unwrap_or_default()
                            {
                                add_mem(
                                    mem,
                                    (host_id, series_id, ts, value.as_f64().unwrap_or(f64::NAN)),
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        tx.commit()
    }

    /// 一批（一個交易）。回傳每一行的結果
    fn write_batch(
        &mut self,
        batch: &[Parsed],
        cutoffs: &Cutoffs,
        max_hosts: usize,
        new_hosts: &mut u64,
    ) -> rusqlite::Result<Vec<Outcome>> {
        let Self {
            conn,
            caches,
            hosts,
            until,
            mem,
            buf,
        } = self;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut outcomes = Vec::with_capacity(batch.len());
        let mut late: Vec<Sample> = Vec::new();
        let mut rollup_only: Vec<Sample> = Vec::new();
        let mut fresh: Vec<Sample> = Vec::new();
        let mut seen: HashMap<i64, HostSeen> = HashMap::new();
        let mut created = 0;
        {
            let mut create_host = tx.prepare(
                "INSERT INTO hosts (agent_id, hostname, peer, identity_verified, online, first_seen, last_seen, source)
                 VALUES (?1, ?2, ?3, 0, 0, ?4, ?4, ?5) RETURNING host_id",
            )?;
            let mut insert_key = tx.prepare(
                "INSERT INTO import_keys (host_id, ts, hash) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
            )?;
            let mut same_raw = tx.prepare(
                "SELECT 1 FROM reports
                 WHERE host_id = ?1 AND ts = ?2 AND seq = ?3 AND format = ?4 AND body = ?5 LIMIT 1",
            )?;
            let mut insert = tx.prepare(
                "INSERT INTO reports (host_id, ts, seq, agent_ts, format, body, boot_ref, report_seq, recv_lag)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT DO NOTHING",
            )?;
            let mut samples: Vec<Sample> = Vec::new();
            for p in batch {
                let r = &p.record;
                let recv_ms = r.recv_at.timestamp_millis();
                let host_id = match hosts.get(r.agent_id.as_str()) {
                    Some(&id) => id,
                    None => {
                        if hosts.len() >= max_hosts {
                            outcomes.push(Outcome::Rejected);
                            continue;
                        }
                        let id: i64 = create_host.query_row(
                            params![r.agent_id, r.hostname, r.peer, recv_ms, HOST_SOURCE_IMPORT],
                            |row| row.get(0),
                        )?;
                        hosts.insert(r.agent_id.clone(), id);
                        until.insert(id, 0);
                        created += 1;
                        id
                    }
                };
                let Some(format) = encode_body(caches, &tx, host_id, p, buf, &mut samples)? else {
                    outcomes.push(Outcome::Rejected);
                    continue;
                };
                if insert_key.execute(params![host_id, p.ts_ms, p.hash])? == 0 {
                    outcomes.push(Outcome::Duplicate);
                    continue;
                }
                match p.class {
                    Class::Raw => {
                        let boot_ref = match &r.dedup {
                            Some(d) => Some(caches.boot_ref(&tx, host_id, &d.boot_id)?),
                            None => {
                                if same_raw.exists(params![
                                    host_id,
                                    p.ts_ms,
                                    r.seq as i64,
                                    format,
                                    &buf[..]
                                ])? {
                                    outcomes.push(Outcome::Duplicate);
                                    continue;
                                }
                                None
                            }
                        };
                        let agent_ts = match AgentTs::parse(&r.ts) {
                            AgentTs::Millis(ms) => rusqlite::types::Value::Integer(ms),
                            AgentTs::Text(t) => rusqlite::types::Value::Text(t),
                        };
                        let changed = insert.execute(params![
                            host_id,
                            p.ts_ms,
                            r.seq as i64,
                            agent_ts,
                            format,
                            &buf[..],
                            boot_ref,
                            r.dedup.as_ref().map(|d| d.report_seq as i64),
                            (recv_ms != p.ts_ms).then_some(recv_ms - p.ts_ms),
                        ])?;
                        if changed == 0 {
                            // durable：與 Kairos 已保存的同一筆重複（唯一索引）
                            outcomes.push(Outcome::Duplicate);
                            continue;
                        }
                        if p.ts_ms < until.get(&host_id).copied().unwrap_or(0) {
                            late.extend_from_slice(&samples);
                        } else {
                            fresh.extend_from_slice(&samples);
                        }
                        outcomes.push(Outcome::Raw);
                    }
                    Class::RollupOnly => {
                        rollup_only.extend_from_slice(&samples);
                        outcomes.push(Outcome::RollupOnly);
                    }
                }
                seen.entry(host_id)
                    .and_modify(|s| {
                        s.first = s.first.min(recv_ms);
                        if recv_ms > s.last {
                            s.last = recv_ms;
                            s.hostname.clone_from(&r.hostname);
                            s.peer.clone_from(&r.peer);
                        }
                    })
                    .or_insert_with(|| HostSeen {
                        first: recv_ms,
                        last: recv_ms,
                        hostname: r.hostname.clone(),
                        peer: r.peer.clone(),
                    });
            }
        }
        if !late.is_empty() {
            merge_rollup_rows(&tx, &aggregate(late))?;
        }
        if !rollup_only.is_empty() {
            merge_rollup_rows_from(&tx, &aggregate(rollup_only), cutoffs.m5_from)?;
        }
        {
            // 不覆蓋較新的資訊：hostname／peer／last_seen 只在匯入的資料比清冊中的新時更新
            let mut upd = tx.prepare(
                "UPDATE hosts SET
                   first_seen = min(first_seen, ?2),
                   hostname = CASE WHEN ?3 > last_seen THEN ?4 ELSE hostname END,
                   peer = CASE WHEN ?3 > last_seen THEN ?5 ELSE peer END,
                   last_seen = max(last_seen, ?3)
                 WHERE host_id = ?1",
            )?;
            for (host_id, s) in &seen {
                upd.execute(params![host_id, s.first, s.last, s.hostname, s.peer])?;
            }
        }
        tx.commit()?;
        *new_hosts += created;
        for s in fresh {
            add_mem(mem, s);
        }
        Ok(outcomes)
    }

    /// 寫出記憶體中的彙總：每台主機的列與 `rollup_until` 的推進在同一個交易中，
    /// 每個交易最多約 [`FLUSH_CHUNK_ROWS`] 列（只在主機邊界切開）
    fn flush_mem(&mut self) -> rusqlite::Result<()> {
        if self.mem.is_empty() {
            return Ok(());
        }
        let mem = std::mem::take(&mut self.mem);
        let mut chunk: Vec<RollupRow> = Vec::new();
        let mut ends: Vec<(i64, i64)> = Vec::new();
        let mut current: Option<i64> = None;
        for ((host_id, bucket, series_id), agg) in mem {
            if current != Some(host_id) {
                if chunk.len() >= FLUSH_CHUNK_ROWS {
                    self.write_chunk(&chunk, &ends)?;
                    chunk.clear();
                    ends.clear();
                }
                current = Some(host_id);
                ends.push((host_id, 0));
            }
            let end = (bucket + BUCKET_5M_SECS) * 1000;
            if let Some(last) = ends.last_mut() {
                last.1 = last.1.max(end);
            }
            chunk.push(RollupRow {
                bucket,
                host_id,
                series_id,
                agg,
            });
        }
        if !chunk.is_empty() {
            self.write_chunk(&chunk, &ends)?;
        }
        Ok(())
    }

    fn write_chunk(&mut self, rows: &[RollupRow], ends: &[(i64, i64)]) -> rusqlite::Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        merge_rollup_rows(&tx, rows)?;
        {
            let mut upd = tx.prepare(
                "UPDATE hosts SET rollup_until = max(rollup_until, ?2) WHERE host_id = ?1",
            )?;
            for (host_id, end) in ends {
                upd.execute(params![host_id, end])?;
            }
        }
        tx.commit()?;
        for (host_id, end) in ends {
            let e = self.until.entry(*host_id).or_insert(0);
            *e = (*e).max(*end);
        }
        Ok(())
    }

    fn used_bytes(&self) -> rusqlite::Result<u64> {
        let pragma = |name: &str| {
            self.conn
                .query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0))
        };
        let pages = pragma("page_count")? - pragma("freelist_count")?;
        Ok(u64::try_from(pages.max(0) * pragma("page_size")?).unwrap_or(0))
    }
}

fn add_mem(mem: &mut BTreeMap<(i64, i64, i64), Agg>, (host_id, series_id, ts, v): Sample) {
    if !v.is_finite() {
        return;
    }
    mem.entry((host_id, bucket_start(ts, BUCKET_5M_SECS), series_id))
        .and_modify(|a| a.add(v))
        .or_insert_with(|| Agg::new(v));
}

impl DryRun {
    fn open(path: &Path) -> Result<Self, Error> {
        let conn = if path.exists() {
            Some(open_read_only(path)?)
        } else {
            None
        };
        let (has_keys, existing_hosts) = match &conn {
            Some(c) => {
                let has_keys: i64 = c
                    .query_row(
                        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'import_keys'",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|e| db_error("無法讀取資料庫", e))?;
                let hosts: i64 = c
                    .query_row("SELECT count(*) FROM hosts", [], |r| r.get(0))
                    .map_err(|e| db_error("無法讀取主機清冊", e))?;
                (has_keys > 0, usize::try_from(hosts).unwrap_or(0))
            }
            None => (false, 0),
        };
        Ok(Self {
            conn,
            has_keys,
            hosts: HashMap::new(),
            existing_hosts,
            series: HashMap::new(),
            new: HashSet::new(),
            seen: HashSet::new(),
        })
    }

    fn host_id(&mut self, agent_id: &str) -> rusqlite::Result<Option<i64>> {
        if let Some(h) = self.hosts.get(agent_id) {
            return Ok(*h);
        }
        let id = match &self.conn {
            Some(c) => c
                .query_row(
                    "SELECT host_id FROM hosts WHERE agent_id = ?1",
                    [agent_id],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
        self.hosts.insert(agent_id.to_string(), id);
        Ok(id)
    }

    fn series_id(
        &mut self,
        name: &str,
        unit: &str,
        labels: String,
    ) -> rusqlite::Result<Option<i64>> {
        let key = (name.to_string(), unit.to_string(), labels);
        if let Some(id) = self.series.get(&key) {
            return Ok(*id);
        }
        let id = match &self.conn {
            Some(c) => c
                .query_row(
                    "SELECT series_id FROM series WHERE name = ?1 AND unit = ?2 AND labels = ?3",
                    params![key.0, key.1, key.2],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
        self.series.insert(key, id);
        Ok(id)
    }

    /// 非 durable 的行在資料庫中是否已有完全相同的原始 report（序列不存在時一定沒有）
    fn same_raw_exists(&mut self, host_id: i64, p: &Parsed) -> rusqlite::Result<bool> {
        let mut buf = Vec::new();
        let format = match &p.record.body {
            RecordBody::Legacy {
                cpu_usage,
                total_memory,
                used_memory,
            } => {
                codec::encode_legacy(&mut buf, *cpu_usage, *total_memory, *used_memory);
                FORMAT_LEGACY
            }
            RecordBody::Metrics { metrics } => {
                let mut ids = Vec::with_capacity(metrics.len());
                for m in metrics {
                    match self.series_id(&m.name, &m.unit, canonical_labels(&m.labels))? {
                        Some(id) => ids.push(id),
                        None => return Ok(false),
                    }
                }
                codec::encode_metrics(
                    &mut buf,
                    ids.iter().copied().zip(metrics.iter().map(|m| &m.value)),
                );
                FORMAT_METRICS
            }
        };
        let Some(c) = &self.conn else {
            return Ok(false);
        };
        c.query_row(
            "SELECT 1 FROM reports
             WHERE host_id = ?1 AND ts = ?2 AND seq = ?3 AND format = ?4 AND body = ?5 LIMIT 1",
            params![host_id, p.ts_ms, p.record.seq as i64, format, buf],
            |_| Ok(()),
        )
        .optional()
        .map(|r| r.is_some())
    }

    /// 查詢是否有任何一列
    fn exists(&self, sql: &str, params: impl rusqlite::Params) -> rusqlite::Result<bool> {
        match &self.conn {
            Some(c) => c
                .query_row(sql, params, |_| Ok(()))
                .optional()
                .map(|r| r.is_some()),
            None => Ok(false),
        }
    }

    fn classify(
        &mut self,
        p: &Parsed,
        max_hosts: usize,
        new_hosts: &mut u64,
    ) -> rusqlite::Result<Outcome> {
        let imported = match p.class {
            Class::Raw => Outcome::Raw,
            Class::RollupOnly => Outcome::RollupOnly,
        };
        let r = &p.record;
        let host_id = match self.host_id(&r.agent_id)? {
            Some(id) => Some(id),
            None => {
                // 清冊中沒有：第一次看到時計入將建立的主機（超過上限的不建立）
                if !self.new.contains(&r.agent_id) {
                    if self.existing_hosts + self.new.len() >= max_hosts {
                        return Ok(Outcome::Rejected);
                    }
                    self.new.insert(r.agent_id.clone());
                    *new_hosts += 1;
                }
                None
            }
        };
        let agent = {
            let mut h = StableHasher::new();
            h.str(&r.agent_id);
            h.finish()
        };
        if !self.seen.insert((agent, p.ts_ms, p.hash)) {
            return Ok(Outcome::Duplicate);
        }
        let Some(host_id) = host_id else {
            return Ok(imported);
        };
        if self.has_keys
            && self.exists(
                "SELECT 1 FROM import_keys WHERE host_id = ?1 AND ts = ?2 AND hash = ?3",
                params![host_id, p.ts_ms, p.hash],
            )?
        {
            return Ok(Outcome::Duplicate);
        }
        if p.class == Class::Raw {
            let dup = match &r.dedup {
                Some(d) => self.exists(
                    "SELECT 1 FROM boots b JOIN reports r ON r.boot_ref = b.boot_ref
                     WHERE b.host_id = ?1 AND b.boot_id = ?2 AND r.report_seq = ?3",
                    params![host_id, d.boot_id, d.report_seq as i64],
                )?,
                None => self.same_raw_exists(host_id, p)?,
            };
            if dup {
                return Ok(Outcome::Duplicate);
            }
        }
        Ok(imported)
    }

    fn classify_batch(
        &mut self,
        batch: &[Parsed],
        max_hosts: usize,
        new_hosts: &mut u64,
    ) -> rusqlite::Result<Vec<Outcome>> {
        batch
            .iter()
            .map(|p| self.classify(p, max_hosts, new_hosts))
            .collect()
    }
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
