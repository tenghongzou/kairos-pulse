//! 每個來源 IP 的同時連線數上限。
//!
//! 只有全域上限時，單一來源可以用慢速握手佔滿所有名額；這裡在 accept 後立即檢查，
//! 超過上限的連線直接關閉。名額在連線結束（[`PerIpPermit`] 被釋放）時歸還。
//! 計數表的筆數不超過同時連線數（每筆至少對應一條連線），不需另外設上限。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug)]
pub struct PerIpLimiter {
    max_per_ip: usize,
    counts: Mutex<HashMap<IpAddr, usize>>,
}

/// 一條連線佔用的名額；釋放時歸還
#[derive(Debug)]
pub struct PerIpPermit {
    limiter: Arc<PerIpLimiter>,
    ip: IpAddr,
}

/// IPv4-mapped IPv6 位址（`::ffff:a.b.c.d`，雙堆疊 socket 上的 IPv4 連線）視為同一個 IPv4 位址
pub fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

impl PerIpLimiter {
    pub fn new(max_per_ip: usize) -> Arc<Self> {
        Arc::new(Self {
            max_per_ip: max_per_ip.max(1),
            counts: Mutex::new(HashMap::new()),
        })
    }

    pub fn max_per_ip(&self) -> usize {
        self.max_per_ip
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<IpAddr, usize>> {
        // 持鎖期間只有簡單的計數更新，被污染也沿用內部資料
        self.counts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 取得一個名額；該 IP 已達上限時回傳 `None`
    pub fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<PerIpPermit> {
        let ip = normalize_ip(ip);
        let mut counts = self.lock();
        let count = counts.entry(ip).or_insert(0);
        if *count >= self.max_per_ip {
            return None;
        }
        *count += 1;
        Some(PerIpPermit {
            limiter: self.clone(),
            ip,
        })
    }

    /// 目前追蹤的 IP 數
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.lock().len()
    }
}

impl Drop for PerIpPermit {
    fn drop(&mut self) {
        let mut counts = self.limiter.lock();
        if let Some(count) = counts.get_mut(&self.ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const A: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    const B: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));

    #[test]
    fn limit_is_per_ip_and_released_on_drop() {
        let l = PerIpLimiter::new(2);
        let a1 = l.try_acquire(A).unwrap();
        let a2 = l.try_acquire(A).unwrap();
        // K-S2：同一 IP 第 3 條被拒，其他 IP 不受影響
        assert!(l.try_acquire(A).is_none());
        let b1 = l.try_acquire(B).unwrap();
        assert_eq!(l.tracked(), 2);

        drop(a1);
        let a3 = l.try_acquire(A).unwrap();
        assert!(l.try_acquire(A).is_none());

        drop((a2, a3, b1));
        // 歸零的 IP 會從計數表移除
        assert_eq!(l.tracked(), 0);
    }

    #[test]
    fn ipv4_mapped_ipv6_counts_as_ipv4() {
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped());
        assert_eq!(normalize_ip(mapped), A);
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(normalize_ip(v6), v6);

        let l = PerIpLimiter::new(1);
        let _p = l.try_acquire(A).unwrap();
        assert!(l.try_acquire(mapped).is_none());
    }

    #[test]
    fn zero_limit_is_treated_as_one() {
        let l = PerIpLimiter::new(0);
        assert_eq!(l.max_per_ip(), 1);
        let _p = l.try_acquire(A).unwrap();
        assert!(l.try_acquire(A).is_none());
    }
}
