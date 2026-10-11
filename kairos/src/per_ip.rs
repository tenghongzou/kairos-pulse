//! 每個來源的連線限制：同時連線數上限與新連線速率上限。
//!
//! 只有全域上限時，單一來源可以用慢速握手佔滿所有名額；這裡在 accept 後立即檢查，
//! 超過上限的連線直接關閉。名額在連線結束（[`PerIpPermit`] 被釋放）時歸還。
//!
//! 兩種限制的計算單位不同：
//! - **同時連線數**（`server.max_connections_per_ip`）以單一位址計算（[`address_key`]：
//!   IPv4-mapped IPv6 視為 IPv4）。企業 VLAN、雲端子網中多台主機常共用同一個 IPv6 /64，
//!   以 /64 合併會誤擋正常的 agent。
//! - **新連線速率**以來源 [`source_key`] 計算：IPv6 以 /64 合併（一般使用者或主機通常分配到整個
//!   /64，可任意使用其中的位址；R2-K2）。report 的跨連線限速與日誌節流也以 /64 為來源。
//!
//! 以大量 IPv6 位址進行慢速握手、佔滿全域名額的攻擊，由 accept 迴圈的「握手中上限」
//! （`max_connections` 的一半，見 `main::handshake_limit`）負責防護；這裡的同時連線上限只防止
//! 單一位址佔用過多名額。
//!
//! 檢查順序：同時連線數 → 呼叫端的閘門（握手中上限）→ 新連線速率。被前兩者拒絕的連線不扣速率額度。
//! 計數表的筆數不超過同時連線數（每筆至少對應一條連線）；速率表超過上限時清掉已補滿的項目。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::rate::TokenBucket;

/// 每個來源每秒可建立的新連線數（R2-K12）。Pulse 斷線後以退避重連，正常遠低於此值
pub const NEW_CONNECTIONS_PER_SEC: f64 = 10.0;

/// 每個來源可連續建立的新連線數下限；實際為 `max(此值, 2 × max_connections_per_ip)`，
/// 讓同一個 NAT 後的 agent 在伺服器重新啟動後能同時重連
pub const MIN_NEW_CONNECTION_BURST: f64 = 32.0;

/// 速率表超過此筆數時，清掉已補滿（閒置）的項目
const RATE_PRUNE_THRESHOLD: usize = 4096;

/// 同時連線數的 key：單一位址；IPv4-mapped IPv6 位址（`::ffff:a.b.c.d`，雙堆疊 socket 上的
/// IPv4 連線）視為 IPv4 位址
pub fn address_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

/// 來源的正規化 key（新連線速率、report 限速、日誌節流）：同 [`address_key`]，
/// 但 IPv6 位址只保留前 64 bits（同一個 /64 視為同一個來源）
pub fn source_key(ip: IpAddr) -> IpAddr {
    match address_key(ip) {
        IpAddr::V6(v6) => {
            let prefix = u128::from(v6) & !((1u128 << 64) - 1);
            IpAddr::V6(Ipv6Addr::from(prefix))
        }
        v4 => v4,
    }
}

/// 拒絕新連線的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// 該位址的同時連線數已達上限
    Concurrency,
    /// 呼叫端的閘門拒絕（握手中的連線數已達上限）
    Gate,
    /// 該來源建立新連線的速率超過上限
    Rate,
}

#[derive(Debug)]
pub struct PerIpLimiter {
    max_per_ip: usize,
    counts: Mutex<HashMap<IpAddr, usize>>,
    rates: Mutex<HashMap<IpAddr, TokenBucket>>,
    new_connection_burst: f64,
}

/// 一條連線佔用的名額；釋放時歸還
#[derive(Debug)]
pub struct PerIpPermit {
    limiter: Arc<PerIpLimiter>,
    key: IpAddr,
}

impl PerIpLimiter {
    pub fn new(max_per_ip: usize) -> Arc<Self> {
        let max_per_ip = max_per_ip.max(1);
        Arc::new(Self {
            max_per_ip,
            counts: Mutex::new(HashMap::new()),
            rates: Mutex::new(HashMap::new()),
            new_connection_burst: MIN_NEW_CONNECTION_BURST.max(2.0 * max_per_ip as f64),
        })
    }

    pub fn max_per_ip(&self) -> usize {
        self.max_per_ip
    }

    fn counts(&self) -> MutexGuard<'_, HashMap<IpAddr, usize>> {
        // 持鎖期間只有簡單的計數更新，被污染也沿用內部資料
        self.counts.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn rates(&self) -> MutexGuard<'_, HashMap<IpAddr, TokenBucket>> {
        self.rates.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 新連線速率檢查：取得一個 token（不足時不扣除）
    fn admit_rate(&self, key: IpAddr, now: Instant) -> bool {
        let mut rates = self.rates();
        if !rates.contains_key(&key) && rates.len() >= RATE_PRUNE_THRESHOLD {
            rates.retain(|_, b| !b.is_full(now));
        }
        rates
            .entry(key)
            .or_insert_with(|| {
                TokenBucket::new(self.new_connection_burst, NEW_CONNECTIONS_PER_SEC, now)
            })
            .try_take(now)
    }

    /// 取得一個名額：依序檢查同時連線數、`gate`（呼叫端的握手中上限）、新連線速率；
    /// 任一項拒絕時回傳原因。`gate` 只在同時連線數通過後呼叫；被拒絕的連線不扣速率額度。
    pub fn try_acquire(
        self: &Arc<Self>,
        ip: IpAddr,
        gate: impl FnOnce() -> bool,
    ) -> Result<PerIpPermit, Rejected> {
        self.try_acquire_at(ip, Instant::now(), gate)
    }

    pub fn try_acquire_at(
        self: &Arc<Self>,
        ip: IpAddr,
        now: Instant,
        gate: impl FnOnce() -> bool,
    ) -> Result<PerIpPermit, Rejected> {
        let key = address_key(ip);
        if self
            .counts()
            .get(&key)
            .is_some_and(|c| *c >= self.max_per_ip)
        {
            return Err(Rejected::Concurrency);
        }
        if !gate() {
            return Err(Rejected::Gate);
        }
        if !self.admit_rate(source_key(ip), now) {
            return Err(Rejected::Rate);
        }
        let mut counts = self.counts();
        let count = counts.entry(key).or_insert(0);
        if *count >= self.max_per_ip {
            return Err(Rejected::Concurrency);
        }
        *count += 1;
        Ok(PerIpPermit {
            limiter: self.clone(),
            key,
        })
    }

    /// 目前追蹤的來源數
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.counts().len()
    }
}

impl Drop for PerIpPermit {
    fn drop(&mut self) {
        let mut counts = self.limiter.counts();
        if let Some(count) = counts.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    const A: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    const B: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));

    fn v6(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn limit_is_per_ip_and_released_on_drop() {
        let l = PerIpLimiter::new(2);
        let a1 = l.try_acquire(A, || true).unwrap();
        let a2 = l.try_acquire(A, || true).unwrap();
        // K-S2：同一 IP 第 3 條被拒，其他 IP 不受影響
        assert_eq!(
            l.try_acquire(A, || true).unwrap_err(),
            Rejected::Concurrency
        );
        let b1 = l.try_acquire(B, || true).unwrap();
        assert_eq!(l.tracked(), 2);

        drop(a1);
        let a3 = l.try_acquire(A, || true).unwrap();
        assert!(l.try_acquire(A, || true).is_err());

        drop((a2, a3, b1));
        // 歸零的 IP 會從計數表移除
        assert_eq!(l.tracked(), 0);
    }

    #[test]
    fn ipv4_mapped_ipv6_counts_as_ipv4() {
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped());
        assert_eq!(source_key(mapped), A);
        assert_eq!(source_key(v6("::1")), v6("::"));

        let l = PerIpLimiter::new(1);
        let _p = l.try_acquire(A, || true).unwrap();
        assert!(l.try_acquire(mapped, || true).is_err());
    }

    #[test]
    fn ipv6_concurrency_is_per_address_but_rate_is_per_64() {
        assert_eq!(
            source_key(v6("2001:db8:1:2:aaaa:bbbb:cccc:dddd")),
            v6("2001:db8:1:2::")
        );
        assert_eq!(address_key(v6("2001:db8:1:2::9")), v6("2001:db8:1:2::9"));
        // 同一個 /64 內的不同位址各自有同時連線額度（企業 VLAN、雲端子網共用 /64）
        let l = PerIpLimiter::new(1);
        let _p1 = l.try_acquire(v6("2001:db8:1:2::1"), || true).unwrap();
        let _p2 = l.try_acquire(v6("2001:db8:1:2::2"), || true).unwrap();
        assert_eq!(
            l.try_acquire(v6("2001:db8:1:2::1"), || true).unwrap_err(),
            Rejected::Concurrency
        );
        assert_eq!(l.tracked(), 2);

        // 新連線速率仍以 /64 合併計算：用不同位址輪流連線也共用同一份額度
        let l = PerIpLimiter::new(1);
        let t0 = Instant::now();
        let burst = MIN_NEW_CONNECTION_BURST as u32;
        for i in 0..burst {
            let ip = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, i as u16 + 1));
            drop(l.try_acquire_at(ip, t0, || true).unwrap());
        }
        assert_eq!(
            l.try_acquire_at(v6("2001:db8:1:2::ffff"), t0, || true)
                .unwrap_err(),
            Rejected::Rate
        );
        // 相鄰的 /64 是另一個來源
        assert!(l.try_acquire_at(v6("2001:db8:1:3::1"), t0, || true).is_ok());
    }

    #[test]
    fn gate_is_checked_before_the_rate_and_rejections_do_not_consume_it() {
        // 握手中上限（閘門）拒絕的連線不扣新連線速率額度；同時連線數拒絕時不呼叫閘門
        let l = PerIpLimiter::new(1);
        let t0 = Instant::now();
        for _ in 0..1000 {
            assert_eq!(
                l.try_acquire_at(A, t0, || false).unwrap_err(),
                Rejected::Gate
            );
        }
        let burst = MIN_NEW_CONNECTION_BURST as usize;
        for _ in 0..burst {
            drop(l.try_acquire_at(A, t0, || true).unwrap());
        }
        assert_eq!(
            l.try_acquire_at(A, t0, || true).unwrap_err(),
            Rejected::Rate
        );
        let _held = l.try_acquire_at(B, t0, || true).unwrap();
        let mut called = false;
        assert_eq!(
            l.try_acquire_at(B, t0, || {
                called = true;
                true
            })
            .unwrap_err(),
            Rejected::Concurrency
        );
        assert!(!called);
    }

    #[test]
    fn zero_limit_is_treated_as_one() {
        let l = PerIpLimiter::new(0);
        assert_eq!(l.max_per_ip(), 1);
        let _p = l.try_acquire(A, || true).unwrap();
        assert!(l.try_acquire(A, || true).is_err());
    }

    #[test]
    fn new_connection_rate_is_limited_per_source() {
        // R2-K12：同一來源連續建立新連線（每條立即結束）超過 burst 後被拒，之後依速率恢復
        let l = PerIpLimiter::new(4);
        let t0 = Instant::now();
        let burst = MIN_NEW_CONNECTION_BURST as usize;
        for _ in 0..burst {
            drop(l.try_acquire_at(A, t0, || true).unwrap());
        }
        assert_eq!(
            l.try_acquire_at(A, t0, || true).unwrap_err(),
            Rejected::Rate
        );
        // 其他來源不受影響
        assert!(l.try_acquire_at(B, t0, || true).is_ok());
        // 0.1 秒補回 1 個
        assert!(
            l.try_acquire_at(A, t0 + Duration::from_millis(100), || true)
                .is_ok()
        );
        assert_eq!(
            l.try_acquire_at(A, t0 + Duration::from_millis(100), || true)
                .unwrap_err(),
            Rejected::Rate
        );

        // burst 至少為每 IP 上限的兩倍
        let l = PerIpLimiter::new(100);
        for _ in 0..200 {
            drop(l.try_acquire_at(A, t0, || true).unwrap());
        }
        assert!(l.try_acquire_at(A, t0, || true).is_err());
    }
}
