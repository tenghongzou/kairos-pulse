//! durable report 的去重（審計 A5）：以 (agent_id, `boot_id`, `report_seq`) 判斷一筆 report 是否
//! 已寫入資料檔，重送的那筆回 `ack` 但不再寫入（PROTOCOL.md「durable：at-least-once 投遞」）。
//!
//! 這是 JSONL 儲存的暫行實作，介面刻意只有「寫入時多一個 `Option<DedupKey>`」：
//! 改用資料庫（A1）時，以 (agent_id, boot_id, report_seq) 的唯一索引取代本模組，
//! [`crate::storage::ReportSink::submit`] 的參數與 [`crate::storage::WriteStatus::Duplicate`] 不變。
//!
//! # 資料結構
//!
//! 每個 (agent_id, boot_id) 一個 256 位元的滑動視窗：記錄已寫入的最大 `report_seq`（`high`），
//! 以及 `high - 255 ..= high` 中哪些已寫入。client 依序送出、在途不超過
//! [`MAX_DURABLE_IN_FLIGHT`](kairos_proto::MAX_DURABLE_IN_FLIGHT) 筆時，需要重送的資料一定落在
//! 視窗內；比視窗更舊的重送一律視為已寫入（協定的明文規則，見 [`DURABLE_DEDUP_WINDOW`]）。
//! 每個視窗約 100 bytes（含兩個字串與雜湊表的額外負擔），1 萬台 agent、每台幾個 boot_id 約數 MB。
//!
//! # 限制（JSONL 版本）
//!
//! - 只在記憶體中。啟動時由 writer 從最新的兩個資料檔尾端（合計最多 [`REBUILD_MAX_BYTES`]）重建
//!   （[`DedupIndex::rebuild_from_lines`]），超出這個範圍、重新啟動前已寫入但 `ack` 沒送到的資料，
//!   重送時會再寫一次。
//! - [`IDLE_EVICTION`] 沒有新資料的 (agent_id, boot_id) 會被清除；總數超過 [`MAX_ENTRIES`] 時
//!   清掉最久沒有更新的八分之一。被清除後的重送可能重複寫入。
//! - 寫出中途失敗、結果不明（[`crate::storage::WriteStatus::Unknown`]）的那筆不列入，重送時會再寫。
//!
//! 以上都只會造成重複，不會遺失資料（at-least-once）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kairos_proto::DURABLE_DEDUP_WINDOW;
use serde::{Deserialize, Serialize};

/// 去重鍵：協定的 `boot_id`（已轉小寫）與 `report_seq`。主機身分（agent_id）另外傳入。
///
/// 也是資料檔中這兩個欄位的序列化格式（JSONL 行的 `boot_id`、`report_seq`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DedupKey {
    pub boot_id: String,
    pub report_seq: u64,
}

/// 視窗的位元數（= [`DURABLE_DEDUP_WINDOW`]）
const WINDOW_BITS: u64 = DURABLE_DEDUP_WINDOW;
const WORDS: usize = (WINDOW_BITS / 64) as usize;
const _: () = assert!(WINDOW_BITS.is_multiple_of(64) && WORDS > 0);

/// 最久多久沒有新資料的 (agent_id, boot_id) 會被清除
pub const IDLE_EVICTION: Duration = Duration::from_secs(48 * 3600);

/// 記錄的 (agent_id, boot_id) 總數上限
pub const MAX_ENTRIES: usize = 200_000;

/// 啟動時重建去重紀錄最多讀取的資料檔 bytes 數（最新的兩個檔案尾端合計）
pub const REBUILD_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// 一個 (agent_id, boot_id) 的滑動視窗
#[derive(Debug, Clone)]
struct Window {
    /// 已寫入的最大 report_seq（至少為 1）
    high: u64,
    /// 第 i 位元：`high - i` 是否已寫入
    bits: [u64; WORDS],
    /// 最近一次寫入的時間（清除閒置紀錄用）
    touched: Instant,
}

impl Window {
    fn new(seq: u64, now: Instant) -> Self {
        let mut bits = [0; WORDS];
        bits[0] = 1;
        Self {
            high: seq,
            bits,
            touched: now,
        }
    }

    fn bit(&self, i: u64) -> bool {
        self.bits[(i / 64) as usize] & (1 << (i % 64)) != 0
    }

    fn set(&mut self, i: u64) {
        self.bits[(i / 64) as usize] |= 1 << (i % 64);
    }

    /// 整個視窗往「較舊」的方向移 `d` 位（`high` 增加 `d`）
    fn shift(&mut self, d: u64) {
        if d >= WINDOW_BITS {
            self.bits = [0; WORDS];
            return;
        }
        let words = (d / 64) as usize;
        let bits = (d % 64) as u32;
        for w in (0..WORDS).rev() {
            let value = if w >= words {
                let src = w - words;
                let mut v = self.bits[src] << bits;
                if bits > 0 && src > 0 {
                    v |= self.bits[src - 1] >> (64 - bits);
                }
                v
            } else {
                0
            };
            self.bits[w] = value;
        }
    }

    fn contains(&self, seq: u64) -> bool {
        match self.high.checked_sub(seq) {
            None => false, // 比已寫入的都新
            Some(d) if d < WINDOW_BITS => self.bit(d),
            Some(_) => true, // 比視窗更舊：協定規定視為已寫入
        }
    }

    fn insert(&mut self, seq: u64, now: Instant) {
        self.touched = now;
        if seq > self.high {
            self.shift(seq - self.high);
            self.high = seq;
            self.set(0);
        } else if self.high - seq < WINDOW_BITS {
            self.set(self.high - seq);
        }
    }
}

/// 已寫入的 durable report 的去重紀錄（由 report writer 單獨持有，不需要鎖）
#[derive(Debug, Default)]
pub struct DedupIndex {
    /// agent_id → boot_id → 視窗
    agents: HashMap<String, HashMap<String, Window>>,
    /// 視窗總數
    entries: usize,
}

impl DedupIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// 記錄的 (agent_id, boot_id) 數
    pub fn len(&self) -> usize {
        self.entries
    }

    /// 這筆是否已寫入（重送時回 `ack`、不再寫入）
    pub fn contains(&self, agent_id: &str, key: &DedupKey) -> bool {
        self.agents
            .get(agent_id)
            .and_then(|boots| boots.get(&key.boot_id))
            .is_some_and(|w| w.contains(key.report_seq))
    }

    /// 記錄這筆已寫入
    pub fn insert(&mut self, agent_id: &str, key: &DedupKey, now: Instant) {
        if let Some(window) = self
            .agents
            .get_mut(agent_id)
            .and_then(|boots| boots.get_mut(&key.boot_id))
        {
            window.insert(key.report_seq, now);
            return;
        }
        if self.entries >= MAX_ENTRIES {
            self.evict_oldest(MAX_ENTRIES / 8);
        }
        self.agents
            .entry(agent_id.to_string())
            .or_default()
            .insert(key.boot_id.clone(), Window::new(key.report_seq, now));
        self.entries += 1;
    }

    /// 清除超過 `idle` 沒有新資料的紀錄；回傳清除的數量
    pub fn evict_idle(&mut self, now: Instant, idle: Duration) -> usize {
        self.retain(|w| now.saturating_duration_since(w.touched) < idle)
    }

    /// 清除最久沒有更新的 `n` 筆（總數超過上限時）
    fn evict_oldest(&mut self, n: usize) {
        let mut touched: Vec<Instant> = self
            .agents
            .values()
            .flat_map(|boots| boots.values().map(|w| w.touched))
            .collect();
        if touched.is_empty() || n == 0 {
            return;
        }
        let k = n.min(touched.len()) - 1;
        let (_, cutoff, _) = touched.select_nth_unstable(k);
        let cutoff = *cutoff;
        let removed = self.retain(|w| w.touched > cutoff);
        log::warn!(
            "durable report 的去重紀錄達到上限 {MAX_ENTRIES} 筆，已清除最久沒有更新的 {removed} 筆\
             （之後這些主機重送舊資料時可能寫出重複的紀錄）"
        );
    }

    fn retain(&mut self, mut keep: impl FnMut(&Window) -> bool) -> usize {
        let before = self.entries;
        self.agents.retain(|_, boots| {
            boots.retain(|_, w| keep(w));
            !boots.is_empty()
        });
        self.entries = self.agents.values().map(HashMap::len).sum();
        before - self.entries
    }

    /// 以資料檔中的 JSONL 行重建（啟動時；見 [`crate::storage::start_report_writer`]）。
    /// 不是 JSON、沒有 `boot_id`／`report_seq` 的行（非 durable 的 report、尾端不完整的行）略過。
    /// 回傳加入的行數
    pub fn rebuild_from_lines<'a>(
        &mut self,
        lines: impl Iterator<Item = &'a [u8]>,
        now: Instant,
    ) -> usize {
        #[derive(Deserialize)]
        struct Line {
            agent_id: String,
            boot_id: String,
            report_seq: u64,
        }
        let mut n = 0;
        for line in lines {
            // 大多數行（非 durable）不含 boot_id：先以 bytes 比對，略過不必要的 JSON 解析
            if !contains_bytes(line, b"\"boot_id\"") {
                continue;
            }
            let Ok(line) = serde_json::from_slice::<Line>(line) else {
                continue;
            };
            self.insert(
                &line.agent_id,
                &DedupKey {
                    boot_id: line.boot_id,
                    report_seq: line.report_seq,
                },
                now,
            );
            n += 1;
        }
        n
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
    const B: &str = "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e";

    fn key(boot: &str, seq: u64) -> DedupKey {
        DedupKey {
            boot_id: boot.to_string(),
            report_seq: seq,
        }
    }

    #[test]
    fn written_keys_are_duplicates_and_others_are_not() {
        let now = Instant::now();
        let mut idx = DedupIndex::new();
        assert!(!idx.contains(A, &key(B, 1)));
        idx.insert(A, &key(B, 1), now);
        idx.insert(A, &key(B, 2), now);
        assert!(idx.contains(A, &key(B, 1)));
        assert!(idx.contains(A, &key(B, 2)));
        assert!(!idx.contains(A, &key(B, 3)));
        // 不同 agent、不同 boot_id 互不影響
        assert!(!idx.contains("other", &key(B, 1)));
        assert!(!idx.contains(A, &key(A, 1)));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn a_gap_left_by_a_failed_write_is_not_a_duplicate() {
        // 第 10 筆 storage_unavailable、11～13 筆（在途）已寫入：之後重送 10 時必須寫入
        let now = Instant::now();
        let mut idx = DedupIndex::new();
        for seq in [1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 12, 13] {
            idx.insert(A, &key(B, seq), now);
        }
        assert!(!idx.contains(A, &key(B, 10)));
        assert!(idx.contains(A, &key(B, 11)));
        idx.insert(A, &key(B, 10), now);
        assert!(idx.contains(A, &key(B, 10)));
    }

    #[test]
    fn window_covers_256_and_older_is_treated_as_written() {
        let now = Instant::now();
        let mut idx = DedupIndex::new();
        idx.insert(A, &key(B, 1000), now);
        // high = 1000：745..=1000 在視窗內，未寫入的不算重複
        for seq in [745, 800, 999] {
            assert!(!idx.contains(A, &key(B, seq)), "{seq}");
        }
        // 744 以前比視窗更舊：視為已寫入
        assert!(idx.contains(A, &key(B, 744)));
        assert!(idx.contains(A, &key(B, 1)));
        // 逐位元移動與跨 64 位元字組的移動都正確
        let mut w = Window::new(1, now);
        for seq in 2..=300 {
            if seq % 3 != 0 {
                w.insert(seq, now);
            }
        }
        for seq in 45..=300 {
            assert_eq!(w.contains(seq), seq % 3 != 0, "{seq}");
        }
        w.insert(300 + 64, now);
        assert!(w.contains(364) && w.contains(299) && !w.contains(300) && !w.contains(363));
        w.insert(364 + 200, now);
        assert!(w.contains(564) && w.contains(364) && !w.contains(365));
        assert!(w.contains(308)); // 564 - 256：比視窗更舊
        w.insert(10_000, now);
        assert!(w.contains(10_000) && !w.contains(9_999) && w.contains(9_744));
    }

    #[test]
    fn idle_and_overflowing_entries_are_evicted() {
        let t0 = Instant::now();
        let mut idx = DedupIndex::new();
        idx.insert(A, &key(B, 1), t0);
        idx.insert(A, &key(A, 1), t0 + Duration::from_secs(3600));
        assert_eq!(idx.evict_idle(t0 + IDLE_EVICTION, IDLE_EVICTION), 1);
        assert!(!idx.contains(A, &key(B, 1)));
        assert!(idx.contains(A, &key(A, 1)));
        assert_eq!(idx.len(), 1);

        // 上限：清掉最久沒更新的八分之一
        let mut idx = DedupIndex::new();
        for i in 0..MAX_ENTRIES {
            idx.insert(
                &format!("agent-{i}"),
                &key(B, 1),
                t0 + Duration::from_millis(i as u64),
            );
        }
        assert_eq!(idx.len(), MAX_ENTRIES);
        idx.insert("new", &key(B, 1), t0 + Duration::from_secs(3600));
        assert_eq!(idx.len(), MAX_ENTRIES - MAX_ENTRIES / 8 + 1);
        assert!(!idx.contains("agent-0", &key(B, 1)));
        assert!(idx.contains(&format!("agent-{}", MAX_ENTRIES - 1), &key(B, 1)));
        assert!(idx.contains("new", &key(B, 1)));
    }

    #[test]
    fn rebuild_reads_durable_lines_and_skips_the_rest() {
        let lines = [
            format!(
                r#"{{"schema":2,"agent_id":"{A}","seq":3,"boot_id":"{B}","report_seq":7,"metrics":[]}}"#
            ),
            format!(r#"{{"schema":2,"agent_id":"{A}","seq":4,"metrics":[]}}"#), // 非 durable
            format!(r#"{{"schema":2,"agent_id":"{A}","boot_id":"{B}","report_seq":8"#), // 不完整
            "garbage \"boot_id\"".to_string(),
            String::new(),
        ];
        let mut idx = DedupIndex::new();
        let n = idx.rebuild_from_lines(lines.iter().map(|l| l.as_bytes()), Instant::now());
        assert_eq!(n, 1);
        assert!(idx.contains(A, &key(B, 7)));
        assert!(!idx.contains(A, &key(B, 8)));
    }
}
