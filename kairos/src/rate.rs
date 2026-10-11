//! 速率限制：token bucket，以及跨連線的 report 限速（R2-K3）。
//!
//! report 限速有兩層：
//! - **來源**（跨連線共用、斷線後保留）：啟用 mTLS 時以 agent_id 為來源（憑證綁定，無法偽造）；
//!   未啟用 mTLS 時 agent_id 可任意宣告，改以來源 IP（IPv6 以 /64 合併）為來源，容量與速率乘上
//!   `server.max_connections_per_ip`，讓同一個 NAT 後的多個 agent 不會互相拖累。
//! - **連線**（未啟用 mTLS 時）：限制單一連線不得獨佔來源 IP 的額度。
//!
//! 新出現的來源與新連線都只給 [`REPORT_INITIAL_TOKENS`]（不給滿），重新連線無法取得新的額度。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// report 限速：每個來源（或連線）的 token bucket 容量（可連續送出的筆數）
pub const REPORT_BURST: f64 = 10.0;

/// report 限速：每秒補充的 token 數。Pulse 的 `report.interval` 最短 1 秒，正常不會觸及。
pub const REPORT_RATE_PER_SEC: f64 = 2.0;

/// 新出現的來源（新的 agent_id、新的來源 IP）與新連線一開始可用的 token（不給滿）
pub const REPORT_INITIAL_TOKENS: f64 = 2.0;

/// 單則 report 因限速而延後處理的上限（= 1 / [`REPORT_RATE_PER_SEC`]）。
/// 需要等待更久時（同一來源有多條連線同時送出）不延後，直接回 `storage_unavailable`（確定未保存）。
pub const MAX_REPORT_PACING_DELAY: Duration = Duration::from_millis(500);

/// 浮點誤差的容忍量：剛好等待 [`MAX_REPORT_PACING_DELAY`] 的情況不應因捨入而被拒絕
const PACING_TOLERANCE: Duration = Duration::from_millis(1);

/// 來源表超過此筆數時，清掉已補滿（閒置）的 bucket；補滿的 bucket 與重新建立的（只有
/// [`REPORT_INITIAL_TOKENS`]）相比只會更寬鬆，因此清掉不會讓來源取得更多額度
const PRUNE_THRESHOLD: usize = 4096;

// 封頂值不得小於正常的限速間隔，否則限速會失效
const _: () = assert!(MAX_REPORT_PACING_DELAY.as_millis() * (REPORT_RATE_PER_SEC as u128) >= 1000);

/// token bucket。token 不足時回傳應等待的時間；[`TokenBucket::acquire`] 會預先扣除（允許暫時為負），
/// 等待後即可處理，持續超量時的處理速率恰為補充速率。
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// 初始為滿的 bucket
    pub fn new(capacity: f64, refill_per_sec: f64, now: Instant) -> Self {
        Self::with_tokens(capacity, refill_per_sec, capacity, now)
    }

    /// 初始有 `tokens` 個 token（不超過容量）的 bucket
    pub fn with_tokens(capacity: f64, refill_per_sec: f64, tokens: f64, now: Instant) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: tokens.min(capacity),
            last: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = self.last.max(now);
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
    }

    /// 補充後，取一個 token 前需要等待的時間（不扣除）
    pub fn wait_needed(&mut self, now: Instant) -> Duration {
        self.refill(now);
        if self.tokens >= 1.0 {
            Duration::ZERO
        } else {
            Duration::try_from_secs_f64((1.0 - self.tokens) / self.refill_per_sec)
                .unwrap_or(Duration::MAX)
        }
    }

    /// 扣除一個 token（可為負）
    pub fn consume(&mut self) {
        self.tokens -= 1.0;
    }

    /// 取一個 token，回傳處理前應等待的時間（不需等待時為 0）
    pub fn acquire(&mut self, now: Instant) -> Duration {
        let wait = self.wait_needed(now);
        self.consume();
        wait
    }

    /// 只在不需等待時取用；回傳是否取得
    pub fn try_take(&mut self, now: Instant) -> bool {
        if self.wait_needed(now).is_zero() {
            self.consume();
            true
        } else {
            false
        }
    }

    /// 補充後是否已滿（閒置）
    pub fn is_full(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= self.capacity
    }
}

/// report 限速的來源
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReportSource {
    /// 啟用 mTLS 時：與憑證綁定的 agent_id
    Agent(String),
    /// 未啟用 mTLS 時：來源 IP（已以 [`crate::per_ip::source_key`] 正規化）
    Ip(IpAddr),
}

/// [`ReportLimiter::acquire`] 的結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// 等待這段時間後處理（不需等待時為 0）
    Proceed(Duration),
    /// 需要等待超過 [`MAX_REPORT_PACING_DELAY`]：不處理，回 `storage_unavailable`
    Reject,
}

/// 跨連線共用的 report 限速表
#[derive(Debug)]
pub struct ReportLimiter {
    /// 以來源 IP 為來源時，容量與速率的倍數（`server.max_connections_per_ip`）
    ip_scale: f64,
    buckets: Mutex<HashMap<ReportSource, TokenBucket>>,
}

impl ReportLimiter {
    pub fn new(max_connections_per_ip: usize) -> Self {
        Self {
            ip_scale: max_connections_per_ip.max(1) as f64,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<ReportSource, TokenBucket>> {
        // 持鎖期間只有簡單的數值運算，被污染也沿用內部資料
        self.buckets.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 新連線的 bucket（未啟用 mTLS 時使用）：只給 [`REPORT_INITIAL_TOKENS`]
    pub fn connection_bucket(now: Instant) -> TokenBucket {
        TokenBucket::with_tokens(
            REPORT_BURST,
            REPORT_RATE_PER_SEC,
            REPORT_INITIAL_TOKENS,
            now,
        )
    }

    fn source_bucket(&self, source: &ReportSource, now: Instant) -> TokenBucket {
        let scale = match source {
            ReportSource::Agent(_) => 1.0,
            ReportSource::Ip(_) => self.ip_scale,
        };
        TokenBucket::with_tokens(
            REPORT_BURST * scale,
            REPORT_RATE_PER_SEC * scale,
            REPORT_INITIAL_TOKENS * scale,
            now,
        )
    }

    /// 為一則 report 取得額度：來源的 bucket（必要）與連線的 bucket（可選）都要有額度。
    /// 需要等待的時間超過 `max_wait` 時回 [`Pace::Reject`]，且不扣除任何 token。
    pub fn acquire(
        &self,
        source: ReportSource,
        connection: Option<&mut TokenBucket>,
        now: Instant,
        max_wait: Duration,
    ) -> Pace {
        let mut buckets = self.lock();
        if !buckets.contains_key(&source) && buckets.len() >= PRUNE_THRESHOLD {
            buckets.retain(|_, b| !b.is_full(now));
        }
        let bucket = match buckets.entry(source) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let b = self.source_bucket(e.key(), now);
                e.insert(b)
            }
        };
        let mut wait = bucket.wait_needed(now);
        let mut connection = connection;
        if let Some(c) = connection.as_deref_mut() {
            wait = wait.max(c.wait_needed(now));
        }
        if wait > max_wait.saturating_add(PACING_TOLERANCE) {
            return Pace::Reject;
        }
        bucket.consume();
        if let Some(c) = connection {
            c.consume();
        }
        Pace::Proceed(wait)
    }

    /// 目前追蹤的來源數
    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const IP: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    const MAX: Duration = MAX_REPORT_PACING_DELAY;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn token_bucket_allows_burst_then_paces() {
        // K-S6：可連續 10 則，之後每則間隔 0.5 秒
        let t0 = Instant::now();
        let mut b = TokenBucket::new(REPORT_BURST, REPORT_RATE_PER_SEC, t0);
        for _ in 0..10 {
            assert_eq!(b.acquire(t0), Duration::ZERO);
        }
        assert_eq!(b.acquire(t0), ms(500));
        // 等待後處理，持續超量時的處理速率為每秒 2 則
        assert_eq!(b.acquire(t0 + ms(500)), ms(500));
        // 正常速率（每秒 1 則）永遠不需等待
        let mut b = TokenBucket::new(REPORT_BURST, REPORT_RATE_PER_SEC, t0);
        for i in 0..100 {
            assert_eq!(b.acquire(t0 + Duration::from_secs(i)), Duration::ZERO);
        }
        // 閒置後補滿，但不超過容量
        let mut b = TokenBucket::new(REPORT_BURST, REPORT_RATE_PER_SEC, t0);
        let later = t0 + Duration::from_secs(3600);
        for _ in 0..10 {
            assert_eq!(b.acquire(later), Duration::ZERO);
        }
        assert!(b.acquire(later) > Duration::ZERO);
        // 時鐘倒退不 panic
        assert!(b.acquire(t0) > Duration::ZERO);
        // try_take 不會讓 token 變負
        let mut b = TokenBucket::with_tokens(2.0, 1.0, 1.0, t0);
        assert!(b.try_take(t0));
        assert!(!b.try_take(t0));
        assert!(b.try_take(t0 + Duration::from_secs(1)));
        assert!(!b.is_full(t0 + Duration::from_secs(1)));
        assert!(b.is_full(t0 + Duration::from_secs(3)));
    }

    #[test]
    fn source_bucket_is_shared_across_connections_and_not_full_when_new() {
        // R2-K3：重新連線不會取得新的（滿的）額度
        let t0 = Instant::now();
        let limiter = ReportLimiter::new(1);
        let agent = || ReportSource::Agent("a".into());
        // 新來源只有 2 個 token
        assert_eq!(
            limiter.acquire(agent(), None, t0, MAX),
            Pace::Proceed(ms(0))
        );
        assert_eq!(
            limiter.acquire(agent(), None, t0, MAX),
            Pace::Proceed(ms(0))
        );
        assert_eq!(
            limiter.acquire(agent(), None, t0, MAX),
            Pace::Proceed(ms(500))
        );
        // 「重新連線」後同一來源沿用同一個 bucket：仍需等待
        assert!(matches!(
            limiter.acquire(agent(), None, t0 + ms(500), MAX),
            Pace::Proceed(w) if w > Duration::ZERO
        ));
        // 其他來源不受影響
        assert_eq!(
            limiter.acquire(ReportSource::Agent("b".into()), None, t0, MAX),
            Pace::Proceed(ms(0))
        );
        assert_eq!(limiter.tracked(), 2);
    }

    #[test]
    fn concurrent_use_beyond_the_pacing_cap_is_rejected_without_consuming() {
        let t0 = Instant::now();
        let limiter = ReportLimiter::new(1);
        let ip = || ReportSource::Ip(IP);
        let mut c1 = ReportLimiter::connection_bucket(t0);
        let mut c2 = ReportLimiter::connection_bucket(t0);
        // 來源 IP 的 bucket（容量 10、初始 2）被兩條連線共用
        assert_eq!(
            limiter.acquire(ip(), Some(&mut c1), t0, MAX),
            Pace::Proceed(ms(0))
        );
        assert_eq!(
            limiter.acquire(ip(), Some(&mut c2), t0, MAX),
            Pace::Proceed(ms(0))
        );
        assert_eq!(
            limiter.acquire(ip(), Some(&mut c1), t0, MAX),
            Pace::Proceed(ms(500))
        );
        // 來源已欠 1 個 token：另一條連線需要等 1 秒 > 上限 → 拒絕，且不扣除
        assert_eq!(limiter.acquire(ip(), Some(&mut c2), t0, MAX), Pace::Reject);
        assert_eq!(limiter.acquire(ip(), Some(&mut c2), t0, MAX), Pace::Reject);
        // 被拒絕的兩次沒有扣除來源的 token（否則來源會欠 3 個，0.5 秒後仍需等 1.5 秒而被拒絕）
        assert_eq!(
            limiter.acquire(ip(), Some(&mut c2), t0 + ms(500), MAX),
            Pace::Proceed(ms(500))
        );
    }

    #[test]
    fn connection_bucket_limits_a_single_connection_within_a_shared_ip() {
        // 未啟用 mTLS：來源 IP 的額度乘上每 IP 連線數，但單一連線仍受自己的 bucket 限制
        let t0 = Instant::now();
        let limiter = ReportLimiter::new(32);
        let mut c = ReportLimiter::connection_bucket(t0);
        let ip = || ReportSource::Ip(IP);
        for _ in 0..2 {
            assert_eq!(
                limiter.acquire(ip(), Some(&mut c), t0, MAX),
                Pace::Proceed(ms(0))
            );
        }
        assert_eq!(
            limiter.acquire(ip(), Some(&mut c), t0, MAX),
            Pace::Proceed(ms(500))
        );
        // 同一 IP 的另一條新連線不受影響（來源額度足夠）
        let mut other = ReportLimiter::connection_bucket(t0);
        assert_eq!(
            limiter.acquire(ip(), Some(&mut other), t0, MAX),
            Pace::Proceed(ms(0))
        );
    }

    #[test]
    fn idle_sources_are_pruned_when_the_table_is_large() {
        let t0 = Instant::now();
        let limiter = ReportLimiter::new(1);
        for i in 0..PRUNE_THRESHOLD {
            limiter.acquire(ReportSource::Agent(i.to_string()), None, t0, MAX);
        }
        assert_eq!(limiter.tracked(), PRUNE_THRESHOLD);
        // 一小時後全部都已補滿：加入新來源時清掉
        let later = t0 + Duration::from_secs(3600);
        limiter.acquire(ReportSource::Agent("new".into()), None, later, MAX);
        assert_eq!(limiter.tracked(), 1);
    }
}
