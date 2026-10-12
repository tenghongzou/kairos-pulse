//! 被拒連線的計數（設計包 D15、`kairos/screens.md` §24 #17 (b)）：供日後 S20「連線」與 `/metrics` 使用。
//!
//! 分兩類保存在記憶體（重新啟動後歸零）：
//! - **因每 IP 上限被拒**（`KRS-W2201`）：依來源位址計數（[`super::address_key`]），附當時的上限與是否在
//!   `server.per_ip_allowlist` 內，讓畫面能寫出要調的設定。新連線速率超過上限另外依來源（IPv6 /64）計數。
//! - **因全域上限被拒**（`KRS-W2204`）：`server.max_connections` 名額用盡而暫停接受新連線的次數與累計時間，
//!   以及「握手中」上限（`server.max_connections` 的一半）拒絕的次數。
//!
//! 計數都是啟動以來的累計值；「過去 1 小時」等時間窗由呼叫端以兩次 [`ConnectionStats::snapshot`] 相減取得。
//! 依來源的表最多 [`MAX_TRACKED_SOURCES`] 筆，之後的新來源只計入 `untracked`（總數仍正確），
//! 以大量位址觸發拒絕也不會無限制佔用記憶體。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 依來源計數的表最多保留的來源數
pub const MAX_TRACKED_SOURCES: usize = 1024;

/// 一個來源被拒的紀錄
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRejections {
    pub source: IpAddr,
    /// 被拒的連線數
    pub rejected: u64,
    pub first_at: Instant,
    pub last_at: Instant,
    /// 最近一次被拒時的上限（同時連線數或可連續的新連線數）
    pub limit: usize,
    /// 最近一次被拒時是否在 `server.per_ip_allowlist` 內
    pub allowlisted: bool,
}

/// 依來源計數的被拒連線
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BySource {
    /// 總數（含 `untracked`）
    pub total: u64,
    /// 依被拒次數由多到少排列
    pub sources: Vec<SourceRejections>,
    /// 來源表已滿後才出現的來源被拒的次數
    pub untracked: u64,
}

/// 因全域上限被拒（`KRS-W2204`）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlobalRejections {
    /// `server.max_connections` 名額用盡、暫停接受新連線的次數
    pub pauses: u64,
    /// 暫停接受新連線的累計時間（含目前仍在暫停中的部分）
    pub paused: Duration,
    /// 目前是否暫停中
    pub paused_now: bool,
    /// 「握手中」連線數已達上限而在握手前關閉的連線數
    pub handshake_limit_rejected: u64,
}

/// 被拒連線的快照
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RejectionSnapshot {
    /// 因每 IP 同時連線（mTLS 時為握手中連線）上限被拒，依位址
    pub per_ip: BySource,
    /// 因新連線速率上限被拒，依來源（IPv6 /64）
    pub per_source_rate: BySource,
    pub global: GlobalRejections,
}

#[derive(Debug, Default)]
struct Table {
    total: u64,
    sources: HashMap<IpAddr, SourceRejections>,
    untracked: u64,
}

impl Table {
    fn record(&mut self, source: IpAddr, limit: usize, allowlisted: bool, now: Instant) {
        self.total += 1;
        if let Some(r) = self.sources.get_mut(&source) {
            r.rejected += 1;
            r.last_at = now;
            r.limit = limit;
            r.allowlisted = allowlisted;
        } else if self.sources.len() < MAX_TRACKED_SOURCES {
            self.sources.insert(
                source,
                SourceRejections {
                    source,
                    rejected: 1,
                    first_at: now,
                    last_at: now,
                    limit,
                    allowlisted,
                },
            );
        } else {
            self.untracked += 1;
        }
    }

    fn snapshot(&self) -> BySource {
        let mut sources: Vec<SourceRejections> = self.sources.values().copied().collect();
        sources.sort_by(|a, b| {
            b.rejected
                .cmp(&a.rejected)
                .then_with(|| a.source.cmp(&b.source))
        });
        BySource {
            total: self.total,
            sources,
            untracked: self.untracked,
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    per_ip: Table,
    rate: Table,
    pauses: u64,
    paused: Duration,
    paused_since: Option<Instant>,
    handshake_limit_rejected: u64,
}

/// 被拒連線的計數器（跨執行緒共用）
#[derive(Debug, Default)]
pub struct ConnectionStats {
    inner: Mutex<Inner>,
}

impl ConnectionStats {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // 持鎖期間只有簡單的計數更新，被污染也沿用內部資料
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn record_per_ip(
        &self,
        address: IpAddr,
        limit: usize,
        allowlisted: bool,
        now: Instant,
    ) {
        self.lock().per_ip.record(address, limit, allowlisted, now);
    }

    pub(super) fn record_rate(
        &self,
        source: IpAddr,
        burst: usize,
        allowlisted: bool,
        now: Instant,
    ) {
        self.lock().rate.record(source, burst, allowlisted, now);
    }

    pub(super) fn record_handshake_limit(&self) {
        self.lock().handshake_limit_rejected += 1;
    }

    /// `server.max_connections` 名額用盡、開始暫停接受新連線；已在暫停中時不重複計算
    pub fn pause_begin(&self, now: Instant) {
        let mut inner = self.lock();
        if inner.paused_since.is_none() {
            inner.paused_since = Some(now);
            inner.pauses += 1;
        }
    }

    /// 取得名額、恢復接受新連線；回傳這次暫停的時間（沒有在暫停中時為 `None`）
    pub fn pause_end(&self, now: Instant) -> Option<Duration> {
        let mut inner = self.lock();
        let since = inner.paused_since.take()?;
        let paused = now.saturating_duration_since(since);
        inner.paused += paused;
        Some(paused)
    }

    pub fn snapshot(&self, now: Instant) -> RejectionSnapshot {
        let inner = self.lock();
        let ongoing = inner
            .paused_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        RejectionSnapshot {
            per_ip: inner.per_ip.snapshot(),
            per_source_rate: inner.rate.snapshot(),
            global: GlobalRejections {
                pauses: inner.pauses,
                paused: inner.paused + ongoing,
                paused_now: inner.paused_since.is_some(),
                handshake_limit_rejected: inner.handshake_limit_rejected,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(0xc000_0200 + n))
    }

    #[test]
    fn per_source_counts_are_sorted_and_bounded() {
        let stats = ConnectionStats::default();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        stats.record_per_ip(ip(1), 32, false, t0);
        stats.record_per_ip(ip(2), 32, false, t0);
        stats.record_per_ip(ip(2), 1024, true, t1);
        let s = stats.snapshot(t1);
        assert_eq!(s.per_ip.total, 3);
        assert_eq!(s.per_ip.untracked, 0);
        assert_eq!(s.per_ip.sources.len(), 2);
        let top = s.per_ip.sources[0];
        assert_eq!((top.source, top.rejected), (ip(2), 2));
        // 保留第一次與最近一次的時間，以及最近一次的上限
        assert_eq!((top.first_at, top.last_at), (t0, t1));
        assert_eq!((top.limit, top.allowlisted), (1024, true));
        assert_eq!(s.per_source_rate, BySource::default());

        // 來源表已滿後，新來源只計入 untracked，既有來源照常累計
        for n in 0..(MAX_TRACKED_SOURCES as u32 + 10) {
            stats.record_per_ip(ip(100 + n), 32, false, t1);
        }
        stats.record_per_ip(ip(2), 32, false, t1);
        let s = stats.snapshot(t1);
        assert_eq!(s.per_ip.sources.len(), MAX_TRACKED_SOURCES);
        assert_eq!(s.per_ip.untracked, 12);
        assert_eq!(s.per_ip.total, 3 + MAX_TRACKED_SOURCES as u64 + 10 + 1);
        assert_eq!(s.per_ip.sources[0].rejected, 3);
    }

    #[test]
    fn global_pause_time_accumulates_including_the_ongoing_pause() {
        let stats = ConnectionStats::default();
        let t0 = Instant::now();
        let s = |secs| t0 + Duration::from_secs(secs);
        assert_eq!(stats.pause_end(t0), None);
        stats.pause_begin(s(0));
        // 暫停中重複呼叫不重複計算
        stats.pause_begin(s(1));
        let snap = stats.snapshot(s(2));
        assert_eq!(snap.global.pauses, 1);
        assert!(snap.global.paused_now);
        assert_eq!(snap.global.paused, Duration::from_secs(2));
        assert_eq!(stats.pause_end(s(3)), Some(Duration::from_secs(3)));
        stats.pause_begin(s(10));
        assert_eq!(stats.pause_end(s(15)), Some(Duration::from_secs(5)));
        stats.record_handshake_limit();
        let snap = stats.snapshot(s(20));
        assert_eq!(
            snap.global,
            GlobalRejections {
                pauses: 2,
                paused: Duration::from_secs(8),
                paused_now: false,
                handshake_limit_rejected: 1,
            }
        );
    }
}
