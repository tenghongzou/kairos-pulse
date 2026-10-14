//! report 寫入的統計：成功、重複、`storage_unavailable`、結果不明、速率限制（設計包 S20「report 寫入」、
//! `decisions/INTEGRATION.md` §25.5 #3；主控台 API 的系統健康，`kairos/API.md` 第 5.5 節）。
//!
//! 每筆 report 的結果由連線（`connection::store_report`）記錄一次：啟動以來的累計（原子計數）與
//! 最近 [`WINDOW_MINUTES`] 分鐘、每分鐘一格的環狀計數（過去 1 小時、最近 5 分鐘是否持續發生）。

use serde::Serialize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 保留幾分鐘的每分鐘計數
pub const WINDOW_MINUTES: usize = 60;

/// 一筆 report 的結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// 已寫入（`ack`）
    Written,
    /// 與已保存的一筆重複，未再寫入（`ack`，durable 的重送）
    Duplicate,
    /// 確定未保存（`storage_unavailable`：寫入失敗、逾時取消、清冊已滿）
    Unavailable,
    /// 寫入結果不明（不回應並關閉連線）
    Unknown,
    /// report 速率超過上限（`storage_unavailable`：`report rate limit exceeded`，`KRS-W2203`）
    RateLimited,
}

const KINDS: usize = 5;

impl WriteOutcome {
    fn index(self) -> usize {
        match self {
            WriteOutcome::Written => 0,
            WriteOutcome::Duplicate => 1,
            WriteOutcome::Unavailable => 2,
            WriteOutcome::Unknown => 3,
            WriteOutcome::RateLimited => 4,
        }
    }
}

/// 各種結果的筆數
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct WriteCounts {
    pub written: u64,
    pub duplicate: u64,
    pub storage_unavailable: u64,
    pub unknown: u64,
    pub rate_limited: u64,
}

impl WriteCounts {
    fn from_array(a: [u64; KINDS]) -> Self {
        Self {
            written: a[0],
            duplicate: a[1],
            storage_unavailable: a[2],
            unknown: a[3],
            rate_limited: a[4],
        }
    }

    /// 成功（寫入或重複）佔「進入寫入流程」的比例；沒有任何 report 時為 `None`。
    /// 速率限制不計入（那是 agent 送太快，不是儲存的問題）
    pub fn success_ratio(&self) -> Option<f64> {
        let ok = self.written + self.duplicate;
        let all = ok + self.storage_unavailable + self.unknown;
        (all > 0).then(|| ok as f64 / all as f64)
    }

    /// 是否有寫入失敗（`storage_unavailable` 或結果不明）
    pub fn has_failures(&self) -> bool {
        self.storage_unavailable + self.unknown > 0
    }
}

/// 每分鐘一格：(分鐘編號, 各種結果的筆數)
#[derive(Debug)]
struct Ring {
    slots: [(u64, [u64; KINDS]); WINDOW_MINUTES],
}

/// report 寫入的統計（[`crate::storage::ReportSink`] 共用一份）
#[derive(Debug)]
pub struct WriteStats {
    started: Instant,
    totals: [AtomicU64; KINDS],
    ring: Mutex<Ring>,
}

impl Default for WriteStats {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteStats {
    pub fn new() -> Self {
        Self::starting_at(Instant::now())
    }

    fn starting_at(started: Instant) -> Self {
        Self {
            started,
            totals: Default::default(),
            // 分鐘編號從 1 起，0 表示空格
            ring: Mutex::new(Ring {
                slots: [(0, [0; KINDS]); WINDOW_MINUTES],
            }),
        }
    }

    /// `now` 是啟動後的第幾分鐘（從 1 起）
    fn minute(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started).as_secs() / 60 + 1
    }

    pub fn record(&self, outcome: WriteOutcome) {
        self.record_at(outcome, Instant::now());
    }

    pub fn record_at(&self, outcome: WriteOutcome, now: Instant) {
        let i = outcome.index();
        self.totals[i].fetch_add(1, Ordering::Relaxed);
        let minute = self.minute(now);
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let slot = &mut ring.slots[(minute % WINDOW_MINUTES as u64) as usize];
        if slot.0 != minute {
            *slot = (minute, [0; KINDS]);
        }
        slot.1[i] += 1;
    }

    /// 啟動以來的累計
    pub fn totals(&self) -> WriteCounts {
        let mut a = [0; KINDS];
        for (v, t) in a.iter_mut().zip(&self.totals) {
            *v = t.load(Ordering::Relaxed);
        }
        WriteCounts::from_array(a)
    }

    /// 最近 `minutes` 分鐘（含目前這一分鐘）每分鐘的計數，由舊到新；沒有資料的分鐘為 0
    pub fn per_minute(&self, now: Instant, minutes: usize) -> Vec<WriteCounts> {
        let minutes = minutes.clamp(1, WINDOW_MINUTES) as u64;
        let current = self.minute(now);
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        (0..minutes)
            .rev()
            .map(|back| {
                let Some(m) = current.checked_sub(back).filter(|m| *m > 0) else {
                    return WriteCounts::default();
                };
                let slot = &ring.slots[(m % WINDOW_MINUTES as u64) as usize];
                if slot.0 == m {
                    WriteCounts::from_array(slot.1)
                } else {
                    WriteCounts::default()
                }
            })
            .collect()
    }

    /// 最近 `minutes` 分鐘的合計
    pub fn window(&self, now: Instant, minutes: usize) -> WriteCounts {
        let mut sum = [0u64; KINDS];
        for c in self.per_minute(now, minutes) {
            for (s, v) in sum.iter_mut().zip([
                c.written,
                c.duplicate,
                c.storage_unavailable,
                c.unknown,
                c.rate_limited,
            ]) {
                *s += v;
            }
        }
        WriteCounts::from_array(sum)
    }

    /// 啟動多久了（不足 `minutes` 分鐘時「持續發生」的判斷不成立）
    pub fn uptime(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_per_minute_and_totals() {
        let t0 = Instant::now();
        let s = WriteStats::starting_at(t0);
        s.record_at(WriteOutcome::Written, t0);
        s.record_at(WriteOutcome::Written, t0 + Duration::from_secs(30));
        s.record_at(WriteOutcome::Duplicate, t0 + Duration::from_secs(61));
        s.record_at(
            WriteOutcome::Unavailable,
            t0 + Duration::from_secs(3 * 60 + 1),
        );
        s.record_at(
            WriteOutcome::RateLimited,
            t0 + Duration::from_secs(3 * 60 + 2),
        );
        s.record_at(WriteOutcome::Unknown, t0 + Duration::from_secs(3 * 60 + 3));
        let now = t0 + Duration::from_secs(3 * 60 + 10);
        assert_eq!(
            s.totals(),
            WriteCounts {
                written: 2,
                duplicate: 1,
                storage_unavailable: 1,
                unknown: 1,
                rate_limited: 1
            }
        );
        let minutes = s.per_minute(now, 5);
        assert_eq!(minutes.len(), 5);
        // 由舊到新：啟動前（不存在）、第 1 分鐘、第 2 分鐘、第 3 分鐘、第 4 分鐘
        assert_eq!(minutes[0], WriteCounts::default());
        assert_eq!(minutes[1].written, 2);
        assert_eq!(minutes[2].duplicate, 1);
        assert_eq!(minutes[3], WriteCounts::default());
        assert_eq!(minutes[4].storage_unavailable, 1);
        assert_eq!(minutes[4].rate_limited, 1);
        let w = s.window(now, 60);
        assert_eq!(w, s.totals());
        assert!(w.has_failures());
        // 成功 3 筆／進入寫入流程 5 筆（速率限制不算）
        assert_eq!(w.success_ratio(), Some(0.6));
        assert_eq!(WriteCounts::default().success_ratio(), None);
        // 1 小時後舊的格子被新的分鐘取代
        let later = t0 + Duration::from_secs(61 * 60);
        s.record_at(WriteOutcome::Written, later);
        let w = s.window(later, 60);
        assert_eq!(w.written, 1);
        assert_eq!(w.duplicate, 0);
        assert_eq!(s.totals().written, 3);
    }
}
