//! 本機緩衝（審計 A5）：協商了 `durable` 時，report 先寫進磁碟上的緩衝，收到 Kairos 的 `ack`
//! 才刪除；斷線、Kairos 維護或寫入結果不明時，資料留在緩衝中，重連後依序補送。
//!
//! # 檔案
//!
//! `buffer.dir`（預設 `buffer/`）下：
//!
//! - `spool-<16 位十六進位區段編號>.dat`：區段檔，只附加寫入。開頭 8 bytes 為 [`SEGMENT_MAGIC`]，
//!   之後是一筆筆記錄。區段大小上限為 `max_bytes / 16`（夾在 64 KiB～4 MiB），寫滿就換下一個；
//!   一個區段中的記錄都已送達（或被丟棄）後整個刪除。
//! - `spool.ack`：第一筆尚未送達的記錄位置（`{"segment":N,"offset":M}`），之前的都已送達。
//!   以「暫存檔＋改名」更新，不 fsync（見下方的斷電取捨）。
//!
//! 每筆記錄：
//!
//! ```text
//! 0..4    payload 長度（u32 LE，1～MAX_PAYLOAD_BYTES）
//! 4..8    CRC-32（IEEE，u32 LE），涵蓋 8.. 到記錄結尾（report_seq、取樣時間與 payload）
//! 8..16   report_seq（u64 LE）
//! 16..24  取樣時間（Unix 毫秒，i64 LE；檢查 buffer.max_age 用）
//! 24..    payload：JSON {"boot_id","report_seq","sample"}
//! ```
//!
//! # 斷電與效能的取捨
//!
//! 寫入不逐筆 fsync：資料先交給作業系統，最多每 [`SYNC_INTERVAL`] fsync 一次（另外在換區段與
//! 正常結束時）。斷電時最多遺失最近約 10 秒的取樣（預設每 5 秒一筆時約 2 筆），換得每筆取樣只有
//! 一次 `write`；不完整或內容損毀的尾端記錄以長度與 CRC 偵測，啟動時截掉並記 WARN。
//! `spool.ack` 最多每 [`ACK_PERSIST_INTERVAL`] 寫一次、不 fsync：斷電或當機後它可能停在較舊的
//! 位置，已送達的幾筆會再送一次，由 Kairos 去重（寧可重送，不可遺失）；檔案遺失或損毀時從最舊的
//! 區段開頭重送。
//!
//! # 容量
//!
//! 待送的記錄超過 `buffer.max_mb` 或 `buffer.max_records` 時丟掉最舊的（計數並節流記 WARN）；
//! 取樣時間超過 `buffer.max_age_hours` 的記錄在送出前丟掉。磁碟用量最多約
//! `max_mb` 加上兩個區段（目前正在寫的與部分已送達的最舊區段）。
//!
//! # 檔案權限
//!
//! Unix：目錄以 0750、檔案以 0640 建立（S3 的規則，不依賴 umask）；既有目錄允許其他使用者存取時
//! 記 WARN。Windows：檔案繼承目錄的 ACL；目錄允許一般使用者存取時記 WARN
//! （[`crate::file_perms::dir_permission_warning`]）。
//!
//! 同一個緩衝目錄只能由一個 Pulse 使用（不加鎖；兩個程序共用會互相覆寫）。

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc};

use crate::codes;
use crate::local_status;
use crate::log_throttle::{LogThrottle, suppressed_note};
use crate::system_info::Sample;

/// 區段檔開頭的格式標記（格式改變時換版本號）
pub const SEGMENT_MAGIC: &[u8; 8] = b"PSPOOL01";
/// 記錄標頭的長度
pub const HEADER_BYTES: usize = 24;
/// 單筆 payload 的上限（report 單行上限 64 KiB 加上 JSON 的跳脫與欄位名稱，留足餘裕）
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
/// 區段大小的下限與上限（實際為 `max_bytes / 16` 夾在這之間）
const MIN_SEGMENT_BYTES: u64 = 64 * 1024;
const MAX_SEGMENT_BYTES: u64 = 4 * 1024 * 1024;
/// 最多每隔這麼久 fsync 一次目前的區段
pub const SYNC_INTERVAL: Duration = Duration::from_secs(10);
/// 最多每隔這麼久更新一次 `spool.ack`
pub const ACK_PERSIST_INTERVAL: Duration = Duration::from_secs(5);
/// 送達位置檔
const ACK_FILE: &str = "spool.ack";
const ACK_TMP_FILE: &str = "spool.ack.tmp";
const SEGMENT_PREFIX: &str = "spool-";
const SEGMENT_SUFFIX: &str = ".dat";

/// Unix 上建立的目錄與檔案權限（S3）
#[cfg_attr(not(unix), allow(dead_code))]
const DIR_MODE: u32 = 0o750;
#[cfg_attr(not(unix), allow(dead_code))]
const FILE_MODE: u32 = 0o640;

/// 緩衝的設定（來自 `[buffer]`）
#[derive(Debug, Clone)]
pub struct SpoolSettings {
    pub dir: PathBuf,
    /// 待送記錄的總大小上限（bytes，含記錄標頭）
    pub max_bytes: u64,
    /// 待送記錄的筆數上限
    pub max_records: usize,
    /// 取樣時間超過這麼久的記錄不送出、直接丟棄
    pub max_age: Duration,
}

/// 緩衝中一筆記錄的內容（payload）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpoolRecord {
    pub boot_id: String,
    pub report_seq: u64,
    pub sample: Sample,
}

/// 序列化用（不複製取樣）
#[derive(Serialize)]
struct SpoolRecordRef<'a> {
    boot_id: &'a str,
    report_seq: u64,
    sample: &'a Sample,
}

/// 待送記錄的索引（記憶體中，每筆約 48 bytes）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    /// 本程序內單調遞增的編號（送出游標與回應對應用）
    id: u64,
    segment: u64,
    offset: u64,
    /// 含標頭的長度
    len: u64,
    report_seq: u64,
    sampled_at_ms: i64,
}

/// 對外的記錄摘要
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryInfo {
    pub id: u64,
    pub report_seq: u64,
}

/// [`Spool::append`] 的結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appended {
    pub report_seq: u64,
    /// 因容量上限丟掉的最舊記錄數
    pub dropped: u64,
}

/// 統計（自程序啟動起）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpoolStats {
    /// 待送筆數
    pub pending: usize,
    /// 待送記錄的總大小（bytes）
    pub pending_bytes: u64,
    /// 區段檔的總大小（bytes）
    pub disk_bytes: u64,
    /// 容量已滿而丟掉的最舊記錄數
    pub dropped_full: u64,
    /// 超過 max_age 而丟掉的記錄數
    pub dropped_expired: u64,
    /// Kairos 拒收（invalid_report、bad_request）而丟掉的記錄數
    pub dropped_rejected: u64,
    /// 損毀而略過的記錄數（啟動時的尾端截斷不計筆數，只記日誌）
    pub corrupted: u64,
    /// 因容量已滿而丟掉的記錄，取樣時間的範圍（Unix 毫秒，最舊～最新）
    pub dropped_full_from_ms: Option<i64>,
    pub dropped_full_until_ms: Option<i64>,
    /// 最近一次因容量已滿而丟棄的時間（Unix 毫秒）
    pub last_dropped_full_at_ms: Option<i64>,
    /// 因超過 max_age 而丟掉的記錄，取樣時間的範圍
    pub dropped_expired_from_ms: Option<i64>,
    pub dropped_expired_until_ms: Option<i64>,
    pub last_dropped_expired_at_ms: Option<i64>,
    /// 無法寫入緩衝而遺失的取樣數（`PLS-W4103`）
    pub write_failures: u64,
    pub last_write_failure_at_ms: Option<i64>,
    /// 最近一次寫入失敗的原因
    pub last_write_error: Option<String>,
}

impl SpoolStats {
    /// 把一批丟掉的記錄的取樣時間範圍併入目前累計的範圍
    fn widen(from: &mut Option<i64>, until: &mut Option<i64>, oldest: i64, newest: i64) {
        *from = Some(from.map_or(oldest, |f| f.min(oldest)));
        *until = Some(until.map_or(newest, |u| u.max(newest)));
    }
}

/// 送達位置
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct AckPos {
    segment: u64,
    offset: u64,
}

/// 磁碟上的環狀緩衝。所有方法都是同步 I/O（每次只有一筆、幾 KB），呼叫端以 [`SharedSpool`] 共用
#[derive(Debug)]
pub struct Spool {
    settings: SpoolSettings,
    segment_bytes: u64,
    boot_id: String,
    next_report_seq: u64,
    next_id: u64,
    entries: VecDeque<Entry>,
    pending_bytes: u64,
    /// 區段編號 → 檔案大小
    segments: BTreeMap<u64, u64>,
    current: u64,
    writer: File,
    /// 讀取用的檔案（區段編號與控制代碼）
    reader: Option<(u64, File)>,
    unsynced: bool,
    last_sync: Instant,
    ack_dirty: bool,
    last_ack_persist: Instant,
    stats: SpoolStats,
}

/// CRC-32（IEEE 802.3，反射多項式 0xEDB88320）的查表
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// CRC-32（IEEE），不另外引入 crate
pub fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |c, &b| {
        CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8)
    })
}

fn segment_name(segment: u64) -> String {
    format!("{SEGMENT_PREFIX}{segment:016x}{SEGMENT_SUFFIX}")
}

fn parse_segment_name(name: &str) -> Option<u64> {
    let hex = name
        .strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(SEGMENT_SUFFIX)?;
    (hex.len() == 16)
        .then(|| u64::from_str_radix(hex, 16).ok())
        .flatten()
}

/// 取樣時間（Unix 毫秒）；`ts` 無法解析時用現在
fn sampled_at_ms(ts: &str) -> i64 {
    DateTime::parse_from_rfc3339(ts)
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|_| Utc::now().timestamp_millis())
}

/// 編碼一筆記錄（標頭＋payload）
fn encode_record(report_seq: u64, sampled_at_ms: i64, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_BYTES + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&[0; 4]);
    buf.extend_from_slice(&report_seq.to_le_bytes());
    buf.extend_from_slice(&sampled_at_ms.to_le_bytes());
    buf.extend_from_slice(payload);
    let crc = crc32(&buf[8..]);
    buf[4..8].copy_from_slice(&crc.to_le_bytes());
    buf
}

/// 解析 `data[pos..]` 的一筆記錄標頭並驗證 CRC：回傳 (記錄長度, report_seq, 取樣時間)；
/// 不完整或損毀時為 `None`
fn decode_record(data: &[u8], pos: usize) -> Option<(usize, u64, i64)> {
    let header = data.get(pos..pos.checked_add(HEADER_BYTES)?)?;
    let len = u32::from_le_bytes(header[0..4].try_into().ok()?) as usize;
    if len == 0 || len > MAX_PAYLOAD_BYTES {
        return None;
    }
    let crc = u32::from_le_bytes(header[4..8].try_into().ok()?);
    let end = pos.checked_add(HEADER_BYTES + len)?;
    let body = data.get(pos + 8..end)?;
    if crc32(body) != crc {
        return None;
    }
    let report_seq = u64::from_le_bytes(header[8..16].try_into().ok()?);
    let sampled_at = i64::from_le_bytes(header[16..24].try_into().ok()?);
    Some((HEADER_BYTES + len, report_seq, sampled_at))
}

fn create_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DIR_MODE);
    }
    builder.create(dir)
}

fn file_options() -> OpenOptions {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(FILE_MODE);
    }
    options
}

fn with_path(path: &Path) -> impl Fn(io::Error) -> io::Error + '_ {
    move |e| io::Error::new(e.kind(), format!("{}：{e}", path.display()))
}

impl Spool {
    /// 開啟（必要時建立）緩衝目錄，載入尚未送達的記錄。`boot_id` 每次程序啟動時產生新的
    /// UUID v4，`report_seq` 從 1 開始；先前程序留下的記錄沿用各自的 `boot_id`／`report_seq`。
    pub fn open(settings: SpoolSettings) -> io::Result<Self> {
        let dir = settings.dir.clone();
        create_dir(&dir).map_err(with_path(&dir))?;
        if let Some(warning) = crate::file_perms::dir_permission_warning(&dir, "本機緩衝目錄")
        {
            log::warn!("{warning}");
        }
        let segment_bytes = (settings.max_bytes / 16).clamp(MIN_SEGMENT_BYTES, MAX_SEGMENT_BYTES);
        let ack = read_ack(&dir);
        let _ = fs::remove_file(dir.join(ACK_TMP_FILE));

        let mut found: Vec<u64> = fs::read_dir(&dir)
            .map_err(with_path(&dir))?
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| parse_segment_name(e.file_name().to_str()?))
            .collect();
        found.sort_unstable();

        let mut stats = SpoolStats::default();
        let mut entries = VecDeque::new();
        let mut segments = BTreeMap::new();
        let mut next_id = 0u64;
        let last = found.last().copied();
        for &segment in &found {
            let path = dir.join(segment_name(segment));
            if ack.is_some_and(|a| segment < a.segment) {
                // 已全部送達：上次來不及刪除
                remove_segment(&path);
                continue;
            }
            let data = fs::read(&path).map_err(with_path(&path))?;
            if data.len() < SEGMENT_MAGIC.len() || &data[..SEGMENT_MAGIC.len()] != SEGMENT_MAGIC {
                if Some(segment) == last
                    && data.len() < SEGMENT_MAGIC.len()
                    && SEGMENT_MAGIC.starts_with(&data)
                {
                    // 剛建立、標記還沒寫完就斷電：當成空的區段重寫
                    rewrite_magic(&path)?;
                    segments.insert(segment, SEGMENT_MAGIC.len() as u64);
                } else {
                    log::warn!(
                        "本機緩衝 {} 不是可辨識的區段檔（格式標記不符），已刪除",
                        path.display()
                    );
                    stats.corrupted = stats.corrupted.saturating_add(1);
                    remove_segment(&path);
                }
                continue;
            }
            let skip_before = match ack {
                Some(a) if a.segment == segment => a.offset,
                _ => 0,
            };
            let mut pos = SEGMENT_MAGIC.len();
            while pos < data.len() {
                let Some((len, report_seq, sampled_at_ms)) = decode_record(&data, pos) else {
                    break;
                };
                if pos as u64 >= skip_before {
                    entries.push_back(Entry {
                        id: next_id,
                        segment,
                        offset: pos as u64,
                        len: len as u64,
                        report_seq,
                        sampled_at_ms,
                    });
                    next_id += 1;
                }
                pos += len;
            }
            if pos < data.len() {
                let rest = data.len() - pos;
                if Some(segment) == last {
                    log::warn!(
                        "本機緩衝 {} 的尾端有 {rest} bytes 不完整或損毀的記錄（可能是寫入時斷電），已截掉",
                        path.display()
                    );
                    let file = file_options()
                        .write(true)
                        .open(&path)
                        .map_err(with_path(&path))?;
                    file.set_len(pos as u64).map_err(with_path(&path))?;
                    file.sync_all().map_err(with_path(&path))?;
                } else {
                    log::warn!(
                        "本機緩衝 {} 在第 {pos} bytes 之後有 {rest} bytes 損毀，這部分的記錄無法讀取、已略過",
                        path.display()
                    );
                    stats.corrupted = stats.corrupted.saturating_add(1);
                }
            }
            segments.insert(segment, pos as u64);
        }

        // 目前寫入的區段：最後一個區段還沒滿就接著寫，否則開新的
        let (current, writer) = match segments.last_key_value() {
            Some((&segment, &size)) if size < segment_bytes => {
                let path = dir.join(segment_name(segment));
                let file = file_options()
                    .append(true)
                    .open(&path)
                    .map_err(with_path(&path))?;
                (segment, file)
            }
            other => {
                let segment = other
                    .map(|(&s, _)| s + 1)
                    .unwrap_or(0)
                    .max(ack.map_or(0, |a| a.segment));
                let file = create_segment(&dir, segment)?;
                segments.insert(segment, SEGMENT_MAGIC.len() as u64);
                (segment, file)
            }
        };

        let pending_bytes = entries.iter().map(|e| e.len).sum();
        let now = Instant::now();
        let mut spool = Self {
            settings,
            segment_bytes,
            boot_id: uuid::Uuid::new_v4().to_string(),
            next_report_seq: 1,
            next_id,
            entries,
            pending_bytes,
            segments,
            current,
            writer,
            reader: None,
            unsynced: false,
            last_sync: now,
            ack_dirty: true,
            last_ack_persist: now,
            stats,
        };
        // 設定的上限變小時，先丟掉超出的最舊記錄
        let dropped = spool.enforce_capacity(0);
        if dropped > 0 {
            log::warn!(
                "{}本機緩衝的待送資料超過容量上限（buffer.max_mb／buffer.max_records），已丟掉最舊的 {dropped} 筆",
                codes::tag(codes::BUFFER_FULL, Some(codes::W_BUFFER_FULL))
            );
        }
        spool.collect_segments();
        spool.persist_ack()?;
        Ok(spool)
    }

    /// 本次程序的 `boot_id`
    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// 待送筆數
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn stats(&self) -> SpoolStats {
        SpoolStats {
            pending: self.entries.len(),
            pending_bytes: self.pending_bytes,
            disk_bytes: self.segments.values().sum(),
            ..self.stats.clone()
        }
    }

    /// 取樣時間不早於 `since_ms`（Unix 毫秒）的待送筆數（`window_1h.buffered`）
    pub fn pending_sampled_since(&self, since_ms: i64) -> usize {
        // 記錄依取樣順序排列：從最新的往回數
        self.entries
            .iter()
            .rev()
            .take_while(|e| e.sampled_at_ms >= since_ms)
            .count()
    }

    /// 無法寫入一筆取樣（`PLS-W4103`）：計數並記下原因
    pub fn record_write_failure(&mut self, error: &str) {
        self.stats.write_failures = self.stats.write_failures.saturating_add(1);
        self.stats.last_write_failure_at_ms = Some(Utc::now().timestamp_millis());
        self.stats.last_write_error = Some(error.chars().take(400).collect());
    }

    /// 寫入一筆新的取樣（指派本程序的 `boot_id` 與下一個 `report_seq`）。超過容量時先丟掉最舊的記錄。
    pub fn append(&mut self, sample: &Sample) -> io::Result<Appended> {
        let report_seq = self.next_report_seq;
        self.next_report_seq = self.next_report_seq.saturating_add(1);
        let payload = serde_json::to_vec(&SpoolRecordRef {
            boot_id: &self.boot_id,
            report_seq,
            sample,
        })
        .map_err(io::Error::other)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("取樣資料過大（{} bytes），無法寫入本機緩衝", payload.len()),
            ));
        }
        let record = encode_record(report_seq, sampled_at_ms(&sample.ts), &payload);
        let len = record.len() as u64;
        let dropped = self.enforce_capacity(len);

        let size = self.segments.get(&self.current).copied().unwrap_or(0);
        if size + len > self.segment_bytes && size > SEGMENT_MAGIC.len() as u64 {
            self.roll()?;
        }
        let size = self.segments.get(&self.current).copied().unwrap_or(0);
        if let Err(e) = self.writer.write_all(&record) {
            // 寫到一半失敗：截回原本的長度，避免留下半筆記錄（截不回去時，下次啟動會截掉）
            let _ = self.writer.set_len(size);
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "{}：{e}",
                    self.settings.dir.join(segment_name(self.current)).display()
                ),
            ));
        }
        self.entries.push_back(Entry {
            id: self.next_id,
            segment: self.current,
            offset: size,
            len,
            report_seq,
            sampled_at_ms: sampled_at_ms(&sample.ts),
        });
        self.next_id += 1;
        self.pending_bytes += len;
        self.segments.insert(self.current, size + len);
        self.unsynced = true;
        if dropped > 0 {
            self.collect_segments();
        }
        Ok(Appended {
            report_seq,
            dropped,
        })
    }

    /// 放入 `incoming` bytes 的新記錄前，丟掉超出上限的最舊記錄；回傳丟掉的筆數
    fn enforce_capacity(&mut self, incoming: u64) -> u64 {
        let extra = u64::from(incoming > 0);
        let mut dropped = 0;
        let mut range: Option<(i64, i64)> = None;
        while let Some(front) = self.entries.front().copied() {
            let over_records = self.entries.len() as u64 + extra > self.settings.max_records as u64;
            let over_bytes = self.pending_bytes + incoming > self.settings.max_bytes;
            if !(over_records || over_bytes) {
                break;
            }
            self.entries.pop_front();
            self.pending_bytes -= front.len;
            self.ack_dirty = true;
            dropped += 1;
            let t = front.sampled_at_ms;
            range = Some(range.map_or((t, t), |(a, b)| (a.min(t), b.max(t))));
        }
        self.stats.dropped_full = self.stats.dropped_full.saturating_add(dropped);
        if let Some((oldest, newest)) = range {
            let s = &mut self.stats;
            SpoolStats::widen(
                &mut s.dropped_full_from_ms,
                &mut s.dropped_full_until_ms,
                oldest,
                newest,
            );
            s.last_dropped_full_at_ms = Some(Utc::now().timestamp_millis());
        }
        dropped
    }

    /// 換到新的區段（舊的先 fsync）
    fn roll(&mut self) -> io::Result<()> {
        let path = self.settings.dir.join(segment_name(self.current));
        self.writer.sync_data().map_err(with_path(&path))?;
        let next = self.current + 1;
        self.writer = create_segment(&self.settings.dir, next)?;
        self.segments.insert(next, SEGMENT_MAGIC.len() as u64);
        self.current = next;
        self.unsynced = false;
        self.last_sync = Instant::now();
        Ok(())
    }

    /// 編號 ≥ `id` 的第一筆待送記錄
    pub fn first_at_or_after(&self, id: u64) -> Option<EntryInfo> {
        let i = self.entries.partition_point(|e| e.id < id);
        self.entries.get(i).map(|e| EntryInfo {
            id: e.id,
            report_seq: e.report_seq,
        })
    }

    /// 最舊的待送記錄的取樣時間
    pub fn oldest_sampled_at(&self) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp_millis(self.entries.front()?.sampled_at_ms)
    }

    /// 第一筆（最舊的）待送記錄
    #[cfg(test)]
    pub fn front(&self) -> Option<EntryInfo> {
        self.first_at_or_after(0)
    }

    fn position(&self, id: u64) -> Option<usize> {
        let i = self.entries.partition_point(|e| e.id < id);
        (self.entries.get(i)?.id == id).then_some(i)
    }

    /// 讀出一筆記錄。記錄已不在緩衝中時回傳 `Ok(None)`；內容損毀時為 `InvalidData`
    /// （呼叫端以 [`Spool::discard_corrupted`] 丟掉）
    pub fn read(&mut self, id: u64) -> io::Result<Option<SpoolRecord>> {
        let Some(i) = self.position(id) else {
            return Ok(None);
        };
        let entry = self.entries[i];
        let path = self.settings.dir.join(segment_name(entry.segment));
        if self.reader.as_ref().map(|(s, _)| *s) != Some(entry.segment) {
            self.reader = None;
            let file = File::open(&path).map_err(with_path(&path))?;
            self.reader = Some((entry.segment, file));
        }
        let Some((_, file)) = self.reader.as_mut() else {
            return Ok(None);
        };
        let mut data = vec![0u8; entry.len as usize];
        file.seek(SeekFrom::Start(entry.offset))
            .and_then(|_| file.read_exact(&mut data))
            .map_err(with_path(&path))?;
        let corrupted = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} 第 {} bytes 的記錄（report_seq={}）已損毀",
                    path.display(),
                    entry.offset,
                    entry.report_seq
                ),
            )
        };
        if decode_record(&data, 0).map(|(len, ..)| len as u64) != Some(entry.len) {
            return Err(corrupted());
        }
        let record: SpoolRecord =
            serde_json::from_slice(&data[HEADER_BYTES..]).map_err(|_| corrupted())?;
        Ok(Some(record))
    }

    /// 已送達（收到 ack）：從緩衝移除。回傳是否找到
    pub fn remove(&mut self, id: u64) -> bool {
        let Some(i) = self.position(id) else {
            return false;
        };
        if let Some(entry) = self.entries.remove(i) {
            self.pending_bytes -= entry.len;
        }
        if i == 0 {
            self.ack_dirty = true;
            self.collect_segments();
        }
        true
    }

    /// Kairos 拒收（invalid_report、bad_request）的記錄：丟掉並計數
    pub fn discard_rejected(&mut self, id: u64) -> bool {
        let found = self.remove(id);
        if found {
            self.stats.dropped_rejected = self.stats.dropped_rejected.saturating_add(1);
        }
        found
    }

    /// 內容損毀、無法讀取的記錄：丟掉並計數
    pub fn discard_corrupted(&mut self, id: u64) -> bool {
        let found = self.remove(id);
        if found {
            self.stats.corrupted = self.stats.corrupted.saturating_add(1);
        }
        found
    }

    /// 丟掉取樣時間早於 `now - max_age` 的最舊記錄（只看前端：記錄依取樣順序排列）；回傳丟掉的筆數
    pub fn drop_expired(&mut self, now: DateTime<Utc>) -> u64 {
        let max_age_ms = i64::try_from(self.settings.max_age.as_millis()).unwrap_or(i64::MAX);
        let cutoff = now.timestamp_millis().saturating_sub(max_age_ms);
        let mut dropped = 0;
        let mut range: Option<(i64, i64)> = None;
        while let Some(front) = self.entries.front().copied() {
            if front.sampled_at_ms >= cutoff {
                break;
            }
            self.entries.pop_front();
            self.pending_bytes -= front.len;
            dropped += 1;
            let t = front.sampled_at_ms;
            range = Some(range.map_or((t, t), |(a, b)| (a.min(t), b.max(t))));
        }
        if let Some((oldest, newest)) = range {
            let s = &mut self.stats;
            SpoolStats::widen(
                &mut s.dropped_expired_from_ms,
                &mut s.dropped_expired_until_ms,
                oldest,
                newest,
            );
            s.last_dropped_expired_at_ms = Some(now.timestamp_millis());
        }
        if dropped > 0 {
            self.stats.dropped_expired = self.stats.dropped_expired.saturating_add(dropped);
            self.ack_dirty = true;
            self.collect_segments();
        }
        dropped
    }

    /// 刪除已沒有待送記錄的區段（目前寫入的除外）
    fn collect_segments(&mut self) {
        let keep_from = self.entries.front().map_or(self.current, |e| e.segment);
        let old: Vec<u64> = self
            .segments
            .range(..keep_from)
            .map(|(&s, _)| s)
            .filter(|&s| s != self.current)
            .collect();
        for segment in old {
            if self.reader.as_ref().is_some_and(|(s, _)| *s == segment) {
                self.reader = None;
            }
            remove_segment(&self.settings.dir.join(segment_name(segment)));
            self.segments.remove(&segment);
        }
    }

    /// 定期呼叫：距上次 fsync 超過 [`SYNC_INTERVAL`] 時 fsync，距上次更新 `spool.ack` 超過
    /// [`ACK_PERSIST_INTERVAL`] 時更新
    pub fn maintain(&mut self, now: Instant) -> io::Result<()> {
        if self.unsynced && now.saturating_duration_since(self.last_sync) >= SYNC_INTERVAL {
            self.sync()?;
        }
        if self.ack_dirty
            && now.saturating_duration_since(self.last_ack_persist) >= ACK_PERSIST_INTERVAL
        {
            self.persist_ack()?;
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let path = self.settings.dir.join(segment_name(self.current));
        self.writer.sync_data().map_err(with_path(&path))?;
        self.unsynced = false;
        self.last_sync = Instant::now();
        Ok(())
    }

    /// 正常結束時呼叫：fsync 並更新 `spool.ack`
    pub fn close(&mut self) -> io::Result<()> {
        if self.unsynced {
            self.sync()?;
        }
        self.persist_ack()
    }

    fn ack_pos(&self) -> AckPos {
        match self.entries.front() {
            Some(e) => AckPos {
                segment: e.segment,
                offset: e.offset,
            },
            None => AckPos {
                segment: self.current,
                offset: self.segments.get(&self.current).copied().unwrap_or(0),
            },
        }
    }

    fn persist_ack(&mut self) -> io::Result<()> {
        let tmp = self.settings.dir.join(ACK_TMP_FILE);
        let path = self.settings.dir.join(ACK_FILE);
        let json = serde_json::to_vec(&self.ack_pos()).map_err(io::Error::other)?;
        let mut file = file_options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(with_path(&tmp))?;
        file.write_all(&json).map_err(with_path(&tmp))?;
        drop(file);
        fs::rename(&tmp, &path).map_err(with_path(&path))?;
        self.ack_dirty = false;
        self.last_ack_persist = Instant::now();
        Ok(())
    }
}

fn read_ack(dir: &Path) -> Option<AckPos> {
    let text = fs::read(dir.join(ACK_FILE)).ok()?;
    match serde_json::from_slice(&text) {
        Ok(pos) => Some(pos),
        Err(_) => {
            log::warn!(
                "本機緩衝的送達位置檔 {} 已損毀，從最舊的記錄開始重送（Kairos 會去除重複）",
                dir.join(ACK_FILE).display()
            );
            None
        }
    }
}

fn create_segment(dir: &Path, segment: u64) -> io::Result<File> {
    let path = dir.join(segment_name(segment));
    let mut file = file_options()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .map_err(with_path(&path))?;
    file.write_all(SEGMENT_MAGIC).map_err(with_path(&path))?;
    drop(file);
    file_options()
        .append(true)
        .open(&path)
        .map_err(with_path(&path))
}

fn rewrite_magic(path: &Path) -> io::Result<()> {
    let mut file = file_options()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(with_path(path))?;
    file.write_all(SEGMENT_MAGIC).map_err(with_path(path))
}

fn remove_segment(path: &Path) {
    if let Err(e) = fs::remove_file(path)
        && e.kind() != io::ErrorKind::NotFound
    {
        log::warn!("無法刪除本機緩衝的區段檔 {}：{e}", path.display());
    }
}

// ---------------------------------------------------------------------------
// 收集 task 與連線 task 之間的共用狀態
// ---------------------------------------------------------------------------

/// 寫入失敗與容量已滿的 WARN 節流視窗
const SPOOL_LOG_WINDOW: Duration = Duration::from_secs(300);

/// 收集 task（寫入）與連線 task（讀取、刪除）共用的緩衝
#[derive(Debug)]
pub struct SharedSpool {
    spool: Mutex<Spool>,
    /// 寫入新記錄時通知連線 task
    pub notify: Notify,
    /// 目前連線的 Kairos 沒有協商 durable：新的取樣照舊 at-most-once 直接送出，不寫入緩衝
    live: AtomicBool,
}

impl SharedSpool {
    pub fn new(spool: Spool) -> Arc<Self> {
        Arc::new(Self {
            spool: Mutex::new(spool),
            notify: Notify::new(),
            live: AtomicBool::new(false),
        })
    }

    /// 鎖定緩衝（只做同步 I/O，不得跨越 await）
    pub fn lock(&self) -> MutexGuard<'_, Spool> {
        // 持有鎖的一方 panic 時資料結構仍一致（每個方法結束前都維持不變量），繼續使用
        self.spool.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 連上沒有協商 durable 的 Kairos 時設為 `true`，連線結束時設回 `false`（見 [`LiveGuard`]）
    pub fn set_live(self: &Arc<Self>, live: bool) -> LiveGuard {
        self.live.store(live, Ordering::SeqCst);
        LiveGuard(self.clone())
    }

    pub(crate) fn is_live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }
}

/// 離開時把 live 設回 `false`（之後的取樣寫入緩衝）
#[derive(Debug)]
pub struct LiveGuard(Arc<SharedSpool>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.live.store(false, Ordering::SeqCst);
    }
}

/// 收集 task 的取樣分流：寫入緩衝並通知連線 task；連上沒有協商 durable 的 Kairos 時改送 `live`
/// （與停用緩衝時相同的 at-most-once 路徑）。另外每秒執行一次 [`Spool::maintain`]。
/// 取樣來源關閉時結束（結束前 fsync 並更新送達位置）。
pub async fn ingest(
    mut samples: mpsc::Receiver<Sample>,
    shared: Arc<SharedSpool>,
    live: mpsc::Sender<Sample>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut write_errors = LogThrottle::new(SPOOL_LOG_WINDOW);
    let mut full = LogThrottle::new(SPOOL_LOG_WINDOW);
    loop {
        tokio::select! {
            sample = samples.recv() => {
                let Some(sample) = sample else { break };
                if shared.is_live() {
                    if let Err(mpsc::error::TrySendError::Full(s)) = live.try_send(sample) {
                        local_status::record(local_status::State::dropped_disconnected);
                        log::debug!("回報佇列已滿（寫出中），丟棄此次取樣（ts={}）", s.ts);
                    }
                    continue;
                }
                let result = shared.lock().append(&sample);
                match result {
                    Ok(appended) => {
                        log::trace!(
                            "取樣已寫入本機緩衝（report_seq={}，ts={}）",
                            appended.report_seq,
                            sample.ts
                        );
                        if appended.dropped > 0 {
                            let stats = shared.lock().stats();
                            local_status::record(local_status::State::buffer_changed);
                            if let Some(n) = full.check(tokio::time::Instant::now()) {
                                log::warn!(
                                    "{}本機緩衝已滿（待送 {} 筆、{} bytes），丟掉最舊的資料；累計已丟掉 {} 筆\
                                     （可調大 buffer.max_mb／buffer.max_records）{}",
                                    codes::tag(codes::BUFFER_FULL, Some(codes::W_BUFFER_FULL)),
                                    stats.pending,
                                    stats.pending_bytes,
                                    stats.dropped_full,
                                    suppressed_note(n, SPOOL_LOG_WINDOW)
                                );
                            }
                        }
                        shared.notify.notify_one();
                    }
                    Err(e) => {
                        let total = {
                            let mut spool = shared.lock();
                            spool.record_write_failure(&e.to_string());
                            spool.stats().write_failures
                        };
                        local_status::record(local_status::State::buffer_changed);
                        if let Some(n) = write_errors.check(tokio::time::Instant::now()) {
                            log::warn!(
                                "{}無法寫入本機緩衝，此次取樣（ts={}）遺失（累計 {total} 筆）：{e}{}",
                                codes::tag(codes::BUFFER_WRITE_FAILED, Some(codes::W_BUFFER_WRITE)),
                                sample.ts,
                                suppressed_note(n, SPOOL_LOG_WINDOW)
                            );
                        }
                    }
                }
            }
            _ = tick.tick() => {
                if let Err(e) = shared.lock().maintain(Instant::now())
                    && let Some(n) = write_errors.check(tokio::time::Instant::now())
                {
                    log::warn!(
                        "{}無法同步本機緩衝（斷電時最近的取樣可能遺失）：{e}{}",
                        codes::tag(codes::BUFFER_WRITE_FAILED, Some(codes::W_BUFFER_WRITE)),
                        suppressed_note(n, SPOOL_LOG_WINDOW)
                    );
                }
            }
        }
    }
    if let Err(e) = shared.lock().close() {
        log::warn!("結束時無法同步本機緩衝：{e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kairos_proto::Metric;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::AtomicU64;
            static N: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "pulse-spool-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn settings(dir: &Path) -> SpoolSettings {
        SpoolSettings {
            dir: dir.to_path_buf(),
            max_bytes: 1024 * 1024,
            max_records: 1000,
            max_age: Duration::from_secs(7 * 24 * 3600),
        }
    }

    fn sample(ts: &str, cpu: f32) -> Sample {
        Sample {
            ts: ts.to_string(),
            cpu_usage: cpu,
            total_memory: 2,
            used_memory: 1,
            total_memory_bytes: 2048,
            used_memory_bytes: 1024,
            extra: vec![Metric::new("system.temperature", 40, "Cel")],
        }
    }

    fn now_sample(cpu: f32) -> Sample {
        sample(&crate::protocol::now_ts(), cpu)
    }

    fn cpus(spool: &mut Spool) -> Vec<f32> {
        let mut out = Vec::new();
        let mut id = 0;
        while let Some(e) = spool.first_at_or_after(id) {
            out.push(spool.read(e.id).unwrap().unwrap().sample.cpu_usage);
            id = e.id + 1;
        }
        out
    }

    fn segment_files(dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| parse_segment_name(n).is_some())
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn segment_names_round_trip() {
        assert_eq!(segment_name(26), "spool-000000000000001a.dat");
        assert_eq!(parse_segment_name("spool-000000000000001a.dat"), Some(26));
        for bad in [
            "spool-1a.dat",
            "spool-000000000000001a.tmp",
            "x",
            "spool-zzzzzzzzzzzzzzzz.dat",
        ] {
            assert_eq!(parse_segment_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn records_are_read_back_in_order_with_boot_id_and_increasing_report_seq() {
        let tmp = TempDir::new("order");
        let mut spool = Spool::open(settings(&tmp.0)).unwrap();
        let boot = spool.boot_id().to_string();
        assert!(kairos_proto::is_uuid_text(&boot));
        for i in 0..5 {
            let a = spool.append(&now_sample(i as f32)).unwrap();
            assert_eq!(
                a,
                Appended {
                    report_seq: i + 1,
                    dropped: 0
                }
            );
        }
        assert_eq!(spool.len(), 5);
        let front = spool.front().unwrap();
        let record = spool.read(front.id).unwrap().unwrap();
        assert_eq!(
            (record.boot_id.as_str(), record.report_seq),
            (boot.as_str(), 1)
        );
        assert_eq!(record.sample.extra.len(), 1);
        assert_eq!(cpus(&mut spool), vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        // ack 後移除；中間的也可以移除
        assert!(spool.remove(front.id));
        assert!(!spool.remove(front.id));
        let third = spool.first_at_or_after(front.id + 2).unwrap();
        assert!(spool.remove(third.id));
        assert_eq!(cpus(&mut spool), vec![1.0, 3.0, 4.0]);
        assert_eq!(spool.read(front.id).unwrap(), None);
    }

    #[test]
    fn reopen_keeps_unacked_records_and_starts_a_new_boot_id() {
        let tmp = TempDir::new("reopen");
        let (boot1, acked) = {
            let mut spool = Spool::open(settings(&tmp.0)).unwrap();
            for i in 0..4 {
                spool.append(&now_sample(i as f32)).unwrap();
            }
            let front = spool.front().unwrap();
            spool.remove(front.id);
            spool.close().unwrap();
            (spool.boot_id().to_string(), front.report_seq)
        };
        assert_eq!(acked, 1);
        let mut spool = Spool::open(settings(&tmp.0)).unwrap();
        assert_ne!(spool.boot_id(), boot1, "每次程序啟動產生新的 boot_id");
        assert_eq!(cpus(&mut spool), vec![1.0, 2.0, 3.0]);
        // 舊的記錄沿用原本的 boot_id 與 report_seq；新的從 1 開始
        let old = spool.read(spool.front().unwrap().id).unwrap().unwrap();
        assert_eq!((old.boot_id.as_str(), old.report_seq), (boot1.as_str(), 2));
        assert_eq!(spool.append(&now_sample(9.0)).unwrap().report_seq, 1);
        assert_eq!(cpus(&mut spool), vec![1.0, 2.0, 3.0, 9.0]);
    }

    #[test]
    fn missing_ack_file_replays_everything_still_on_disk() {
        let tmp = TempDir::new("noack");
        {
            let mut spool = Spool::open(settings(&tmp.0)).unwrap();
            for i in 0..3 {
                spool.append(&now_sample(i as f32)).unwrap();
            }
            let front = spool.front().unwrap();
            spool.remove(front.id);
            spool.close().unwrap();
        }
        fs::remove_file(tmp.0.join(ACK_FILE)).unwrap();
        let mut spool = Spool::open(settings(&tmp.0)).unwrap();
        // 送達位置遺失：已送達的那筆也會再送（Kairos 去重），不會遺失
        assert_eq!(cpus(&mut spool), vec![0.0, 1.0, 2.0]);
        // 損毀的送達位置檔也一樣
        fs::write(tmp.0.join(ACK_FILE), b"{garbage").unwrap();
        drop(spool);
        let mut spool = Spool::open(settings(&tmp.0)).unwrap();
        assert_eq!(cpus(&mut spool), vec![0.0, 1.0, 2.0]);
    }

    #[test]
    fn torn_tail_is_detected_and_truncated() {
        let tmp = TempDir::new("torn");
        {
            let mut spool = Spool::open(settings(&tmp.0)).unwrap();
            for i in 0..3 {
                spool.append(&now_sample(i as f32)).unwrap();
            }
            spool.close().unwrap();
        }
        let path = segment_files(&tmp.0).pop().unwrap();
        let full = fs::read(&path).unwrap();
        // 模擬斷電：最後一筆只寫了一半，後面又多了一段零（延遲配置的檔案尾端）
        let mut torn = full[..full.len() - 10].to_vec();
        torn.extend_from_slice(&[0u8; 40]);
        fs::write(&path, &torn).unwrap();
        let mut spool = Spool::open(settings(&tmp.0)).unwrap();
        assert_eq!(cpus(&mut spool), vec![0.0, 1.0]);
        let truncated = fs::metadata(&path).unwrap().len();
        assert!(truncated < full.len() as u64);
        // 截掉後可以接著寫，重新開啟也讀得到
        spool.append(&now_sample(7.0)).unwrap();
        spool.close().unwrap();
        drop(spool);
        let mut spool = Spool::open(settings(&tmp.0)).unwrap();
        assert_eq!(cpus(&mut spool), vec![0.0, 1.0, 7.0]);

        // 內容損毀（CRC 不符）也視為尾端損毀
        let path = segment_files(&tmp.0).pop().unwrap();
        let mut data = fs::read(&path).unwrap();
        let n = data.len();
        data[n - 3] ^= 0xff;
        fs::write(&path, &data).unwrap();
        drop(spool);
        let mut spool = Spool::open(settings(&tmp.0)).unwrap();
        assert_eq!(cpus(&mut spool), vec![0.0, 1.0]);
    }

    #[test]
    fn unrecognized_segment_is_removed() {
        let tmp = TempDir::new("magic");
        fs::create_dir_all(&tmp.0).unwrap();
        fs::write(tmp.0.join(segment_name(3)), b"something else entirely").unwrap();
        let spool = Spool::open(settings(&tmp.0)).unwrap();
        assert!(spool.is_empty());
        assert_eq!(spool.stats().corrupted, 1);
        assert!(!tmp.0.join(segment_name(3)).exists());
    }

    #[test]
    fn full_buffer_drops_the_oldest_and_counts() {
        let tmp = TempDir::new("full");
        let mut s = settings(&tmp.0);
        s.max_records = 3;
        let mut spool = Spool::open(s).unwrap();
        for i in 0..5 {
            let a = spool.append(&now_sample(i as f32)).unwrap();
            assert_eq!(a.dropped, u64::from(i >= 3));
        }
        assert_eq!(cpus(&mut spool), vec![2.0, 3.0, 4.0]);
        assert_eq!(spool.stats().dropped_full, 2);

        // bytes 上限
        let tmp = TempDir::new("full-bytes");
        let mut s = settings(&tmp.0);
        let one = {
            let probe = TempDir::new("probe");
            let mut p = Spool::open(settings(&probe.0)).unwrap();
            p.append(&now_sample(0.0)).unwrap();
            p.stats().pending_bytes
        };
        s.max_bytes = one * 2 + one / 2;
        let mut spool = Spool::open(s).unwrap();
        for i in 0..4 {
            spool.append(&now_sample(i as f32)).unwrap();
        }
        assert_eq!(cpus(&mut spool), vec![2.0, 3.0]);
        assert_eq!(spool.stats().dropped_full, 2);
    }

    #[test]
    fn segments_roll_and_fully_acked_segments_are_deleted() {
        let tmp = TempDir::new("roll");
        let mut s = settings(&tmp.0);
        // 區段大小 = max_bytes / 16 = 64 KiB（下限）
        s.max_bytes = MIN_SEGMENT_BYTES * 16;
        let mut spool = Spool::open(s).unwrap();
        let big = |i: u32| {
            let mut x = now_sample(i as f32);
            x.extra = (0..200)
                .map(|n| Metric::new(&format!("app.metric_{n}"), n, "1"))
                .collect();
            x
        };
        for i in 0..40 {
            spool.append(&big(i)).unwrap();
        }
        let files = segment_files(&tmp.0);
        assert!(files.len() >= 3, "{files:?}");
        let disk: u64 = files.iter().map(|p| fs::metadata(p).unwrap().len()).sum();
        assert_eq!(spool.stats().disk_bytes, disk);
        // 全部送達：只留下目前寫入的區段
        while let Some(e) = spool.front() {
            assert!(spool.remove(e.id));
        }
        assert_eq!(segment_files(&tmp.0).len(), 1);
        spool.close().unwrap();
        drop(spool);
        let spool = Spool::open(settings(&tmp.0)).unwrap();
        assert!(spool.is_empty());
    }

    #[test]
    fn expired_records_are_dropped_before_sending() {
        let tmp = TempDir::new("expire");
        let mut s = settings(&tmp.0);
        s.max_age = Duration::from_secs(3600);
        let mut spool = Spool::open(s).unwrap();
        spool
            .append(&sample("2026-10-01T00:00:00.000Z", 1.0))
            .unwrap();
        spool
            .append(&sample("2026-10-01T00:30:00.000Z", 2.0))
            .unwrap();
        spool
            .append(&sample("2026-10-01T01:10:00.000Z", 3.0))
            .unwrap();
        let now = DateTime::parse_from_rfc3339("2026-10-01T01:20:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(spool.drop_expired(now), 1);
        assert_eq!(spool.drop_expired(now), 0);
        assert_eq!(cpus(&mut spool), vec![2.0, 3.0]);
        assert_eq!(spool.stats().dropped_expired, 1);
    }

    #[test]
    fn rejected_and_corrupted_records_are_counted() {
        let tmp = TempDir::new("reject");
        let mut spool = Spool::open(settings(&tmp.0)).unwrap();
        for i in 0..3 {
            spool.append(&now_sample(i as f32)).unwrap();
        }
        let front = spool.front().unwrap();
        assert!(spool.discard_rejected(front.id));
        // 讀取時發現損毀
        let next = spool.front().unwrap();
        let path = segment_files(&tmp.0).pop().unwrap();
        let mut data = fs::read(&path).unwrap();
        let off = spool.entries[0].offset as usize + HEADER_BYTES + 3;
        data[off] ^= 0xff;
        fs::write(&path, &data).unwrap();
        spool.reader = None;
        let err = spool.read(next.id).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(spool.discard_corrupted(next.id));
        let stats = spool.stats();
        assert_eq!(
            (stats.dropped_rejected, stats.corrupted, stats.pending),
            (1, 1, 1)
        );
    }

    #[cfg(unix)]
    #[test]
    fn files_and_directory_have_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("perm");
        let mut spool = Spool::open(settings(&tmp.0)).unwrap();
        spool.append(&now_sample(1.0)).unwrap();
        spool.close().unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        // umask 只會再收緊，不會放寬
        assert_eq!(mode(&tmp.0) & !DIR_MODE, 0, "{:o}", mode(&tmp.0));
        for f in segment_files(&tmp.0)
            .into_iter()
            .chain([tmp.0.join(ACK_FILE)])
        {
            assert_eq!(mode(&f) & !FILE_MODE, 0, "{}: {:o}", f.display(), mode(&f));
        }
    }

    #[tokio::test]
    async fn ingest_writes_to_the_spool_or_forwards_live_samples() {
        let tmp = TempDir::new("ingest");
        let shared = SharedSpool::new(Spool::open(settings(&tmp.0)).unwrap());
        let (tx, rx) = mpsc::channel(4);
        let (live_tx, mut live_rx) = mpsc::channel(1);
        let task = tokio::spawn(ingest(rx, shared.clone(), live_tx));
        tx.send(now_sample(1.0)).await.unwrap();
        shared.notify.notified().await;
        assert_eq!(shared.lock().len(), 1);
        // 連上沒有協商 durable 的 Kairos：照舊直接送出，不寫入緩衝
        let guard = shared.set_live(true);
        tx.send(now_sample(2.0)).await.unwrap();
        assert_eq!(live_rx.recv().await.unwrap().cpu_usage, 2.0);
        assert_eq!(shared.lock().len(), 1);
        drop(guard);
        tx.send(now_sample(3.0)).await.unwrap();
        shared.notify.notified().await;
        assert_eq!(shared.lock().len(), 2);
        drop(tx);
        task.await.unwrap();
        // 結束時已寫入送達位置
        assert!(tmp.0.join(ACK_FILE).exists());
    }
}
