//! 每個來源的連線限制：同時連線數上限與新連線速率上限，以及 mTLS 模式下依憑證身分計數（審計 A7、K-S2）。
//!
//! 只有全域上限時，單一來源可以用慢速握手佔滿所有名額；這裡在 accept 後立即檢查，
//! 超過上限的連線直接關閉（不進行 TLS 握手）。
//!
//! # 兩階段計數
//!
//! 一條連線依序經過兩個階段（[`ConnectionSlot`]）：
//!
//! 1. **握手前與握手中**（accept 到 hello 通過，含 TLS 握手與等待 hello）：一律依來源位址計數，
//!    上限為 `server.max_connections_per_ip`；來源在 `server.per_ip_allowlist` 內時改用
//!    `min(server.max_connections_per_allowlisted_ip, server.max_connections)`。另受 accept 迴圈的
//!    全域「握手中」上限（`server.max_connections` 的一半）限制。這一段防止慢速握手佔滿名額（K-S2）。
//! 2. **hello 通過後**：
//!    - **啟用 mTLS**：agent_id 已與客戶端憑證綁定，改依憑證身分（正規化後的 agent_id）計數，
//!      歸還來源位址的名額。同一個 NAT 後的 agent 數量因此不受每 IP 上限限制（A7）；
//!      同一身分的新連線依既有的 `superseded` 行為取代舊連線，每個身分在穩定狀態只有一條連線。
//!    - **未啟用 mTLS**：agent_id 可任意宣告，繼續依來源位址計數（每 IP 上限涵蓋連線的整個生命週期）。
//!
//! # 計算單位
//!
//! - **同時連線數**以單一位址計算（[`address_key`]：IPv4-mapped IPv6 視為 IPv4）。企業 VLAN、雲端子網中
//!   多台主機常共用同一個 IPv6 /64，以 /64 合併會誤擋正常的 agent。
//! - **新連線速率**以來源 [`source_key`] 計算：IPv6 以 /64 合併（一般使用者或主機通常分配到整個
//!   /64，可任意使用其中的位址；R2-K2）。report 的跨連線限速與日誌節流也以 /64 為來源。
//!   可連續的條數為 `max(32, 2 × 該來源的每 IP 上限)`；來源（/64）與允許清單有交集時以允許清單的上限計算。
//!
//! 以大量 IPv6 位址進行慢速握手、佔滿全域名額的攻擊，由 accept 迴圈的「握手中上限」
//! （`max_connections` 的一半，見 `main::handshake_limit`）負責防護；這裡的同時連線上限只防止
//! 單一位址佔用過多名額。
//!
//! 檢查順序：同時連線數 → 呼叫端的閘門（握手中上限）→ 新連線速率。被前兩者拒絕的連線不扣速率額度。
//! 被拒的連線依原因計數（[`stats`]）。計數表的筆數不超過同時連線數（每筆至少對應一條連線）；
//! 速率表超過上限時清掉已補滿的項目。

pub mod cidr;
pub mod stats;

use std::collections::HashMap;
use std::io::Error;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use tokio::sync::OwnedSemaphorePermit;

use crate::config::AppConfig;
use crate::rate::TokenBucket;

pub use cidr::Allowlist;
pub use stats::{ConnectionStats, RejectionSnapshot};

/// 每個來源每秒可建立的新連線數（R2-K12）。Pulse 斷線後以退避重連，正常遠低於此值
pub const NEW_CONNECTIONS_PER_SEC: f64 = 10.0;

/// 每個來源可連續建立的新連線數下限；實際為 `max(此值, 2 × 該來源的每 IP 上限)`，
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

/// 每 IP 上限的設定（啟動時由設定推導，執行期間不變）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerIpPolicy {
    /// `server.max_connections_per_ip` 的實際值
    pub max_per_ip: usize,
    /// `server.per_ip_allowlist`
    pub allowlist: Arc<Allowlist>,
    /// 允許清單內每個位址的實際上限：`min(server.max_connections_per_allowlisted_ip, server.max_connections)`
    pub max_per_allowlisted_ip: usize,
    /// 是否啟用 mTLS（決定 hello 通過後是否改依憑證身分計數；只影響說明與快照，
    /// 實際依每條連線 hello 時是否以憑證驗證判斷）
    pub mtls: bool,
}

/// 一個位址適用的同時連線上限
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max: usize,
    /// 是否因在 `server.per_ip_allowlist` 內而使用允許清單的上限
    pub allowlisted: bool,
}

impl PerIpPolicy {
    /// 沒有允許清單、未啟用 mTLS 的設定
    #[cfg(test)]
    pub fn simple(max_per_ip: usize) -> Self {
        Self {
            max_per_ip: max_per_ip.max(1),
            allowlist: Arc::new(Allowlist::default()),
            max_per_allowlisted_ip: max_per_ip.max(1),
            mtls: false,
        }
    }

    /// 由（已通過 [`AppConfig::validate`] 的）設定推導
    pub fn from_config(config: &AppConfig) -> Result<Self, Error> {
        Ok(Self {
            max_per_ip: config.max_connections_per_ip().max(1),
            allowlist: Arc::new(config.per_ip_allowlist()?),
            max_per_allowlisted_ip: config.max_connections_per_allowlisted_ip().max(1),
            mtls: config.auth.client_ca_path().is_some(),
        })
    }

    /// `ip` 適用的同時連線上限
    pub fn limit_for(&self, ip: IpAddr) -> Limit {
        if self.allowlist.contains(ip) {
            Limit {
                max: self.max_per_allowlisted_ip,
                allowlisted: true,
            }
        } else {
            Limit {
                max: self.max_per_ip,
                allowlisted: false,
            }
        }
    }

    /// 來源（[`source_key`]）可連續建立的新連線數，與是否套用允許清單
    pub fn new_connection_burst(&self, source: IpAddr) -> (f64, bool) {
        let allowlisted = self.allowlist.matches_source(source);
        let max = if allowlisted {
            self.max_per_allowlisted_ip
        } else {
            self.max_per_ip
        };
        (MIN_NEW_CONNECTION_BURST.max(2.0 * max as f64), allowlisted)
    }

    /// 未啟用 mTLS 時以來源 IP 計算的 report 額度倍數（來源與允許清單有交集時用允許清單的上限）
    pub fn report_scale(&self, source: IpAddr) -> usize {
        if self.allowlist.matches_source(source) {
            self.max_per_allowlisted_ip
        } else {
            self.max_per_ip
        }
    }

    /// 啟動日誌用的說明：兩階段的計數方式與實際上限
    pub fn describe(&self) -> String {
        let mut s = if self.mtls {
            format!(
                "每 IP 上限：已啟用 mTLS，只計算握手中（尚未通過 hello）的連線，每個位址 {} 條（server.max_connections_per_ip）；通過 hello 後改依憑證身分（agent_id）計數，同一身分的新連線取代舊連線",
                self.max_per_ip
            )
        } else {
            format!(
                "每 IP 上限：未啟用 mTLS，計算全部連線（含握手中），每個位址 {} 條（server.max_connections_per_ip）",
                self.max_per_ip
            )
        };
        if self.allowlist.is_empty() {
            s.push_str("；網段允許清單（server.per_ip_allowlist）是空的");
        } else {
            s.push_str(&format!(
                "；網段允許清單（server.per_ip_allowlist）{} 個網段（{}）內每個位址 {} 條",
                self.allowlist.entries().len(),
                self.allowlist,
                self.max_per_allowlisted_ip
            ));
        }
        s
    }
}

/// 拒絕新連線的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// 該位址的同時連線數（mTLS 時為握手中的連線數）已達上限
    Concurrency(Limit),
    /// 呼叫端的閘門拒絕（握手中的連線數已達上限）
    Gate,
    /// 該來源建立新連線的速率超過上限
    Rate,
}

/// 目前連線計數的快照
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LimiterSnapshot {
    /// 依來源位址計數中的位址數
    pub counted_addresses: usize,
    /// 依來源位址計數中的連線數（mTLS 時只有握手中的連線；未啟用 mTLS 時為全部連線）
    pub counted_connections: usize,
    /// 已改依憑證身分計數的身分數（mTLS）
    pub identities: usize,
    /// 已改依憑證身分計數的連線數（同一身分取代期間短暫可能多於身分數）
    pub identity_connections: usize,
    pub rejections: RejectionSnapshot,
}

impl LimiterSnapshot {
    /// 日誌用的摘要（關機時記錄一次）：兩類被拒連線的累計值
    pub fn summary(&self) -> String {
        let r = &self.rejections;
        let untracked = if r.per_ip.untracked > 0 {
            format!("，另有 {} 條來自未個別記錄的位址", r.per_ip.untracked)
        } else {
            String::new()
        };
        format!(
            "啟動以來被拒的連線：因每 IP 上限 {} 條（{} 個來源位址{}），因新連線速率 {} 條；因全域上限暫停接受新連線 {} 次、累計 {:.1} 秒，握手中上限拒絕 {} 條",
            r.per_ip.total,
            r.per_ip.sources.len(),
            untracked,
            r.per_source_rate.total,
            r.global.pauses,
            r.global.paused.as_secs_f64(),
            r.global.handshake_limit_rejected
        )
    }
}

#[derive(Debug)]
pub struct PerIpLimiter {
    policy: PerIpPolicy,
    counts: Mutex<HashMap<IpAddr, usize>>,
    rates: Mutex<HashMap<IpAddr, TokenBucket>>,
    identities: Mutex<HashMap<String, usize>>,
    stats: ConnectionStats,
}

/// 依來源位址計數的名額；釋放時歸還
#[derive(Debug)]
pub struct PerIpPermit {
    limiter: Arc<PerIpLimiter>,
    key: IpAddr,
}

/// 依憑證身分計數的名額（mTLS，hello 通過後）；釋放時歸還
#[derive(Debug)]
pub struct IdentityPermit {
    limiter: Arc<PerIpLimiter>,
    agent_id: String,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // 持鎖期間只有簡單的計數更新，被污染也沿用內部資料
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PerIpLimiter {
    /// 沒有允許清單、未啟用 mTLS 的限制器
    #[cfg(test)]
    pub fn new(max_per_ip: usize) -> Arc<Self> {
        Self::with_policy(PerIpPolicy::simple(max_per_ip))
    }

    pub fn with_policy(policy: PerIpPolicy) -> Arc<Self> {
        Arc::new(Self {
            policy,
            counts: Mutex::new(HashMap::new()),
            rates: Mutex::new(HashMap::new()),
            identities: Mutex::new(HashMap::new()),
            stats: ConnectionStats::default(),
        })
    }

    pub fn policy(&self) -> &PerIpPolicy {
        &self.policy
    }

    /// 被拒連線的計數器（accept 迴圈另以它記錄全域上限的暫停時間）
    pub fn stats(&self) -> &ConnectionStats {
        &self.stats
    }

    /// 新連線速率檢查：取得一個 token（不足時不扣除）
    fn admit_rate(&self, key: IpAddr, now: Instant) -> bool {
        let mut rates = lock(&self.rates);
        if !rates.contains_key(&key) && rates.len() >= RATE_PRUNE_THRESHOLD {
            rates.retain(|_, b| !b.is_full(now));
        }
        rates
            .entry(key)
            .or_insert_with(|| {
                let (burst, _) = self.policy.new_connection_burst(key);
                TokenBucket::new(burst, NEW_CONNECTIONS_PER_SEC, now)
            })
            .try_take(now)
    }

    /// 取得一個名額：依序檢查同時連線數、`gate`（呼叫端的握手中上限）、新連線速率；
    /// 任一項拒絕時計數並回傳原因。`gate` 只在同時連線數通過後呼叫；被拒絕的連線不扣速率額度。
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
        let limit = self.policy.limit_for(key);
        let reject_concurrency = || {
            self.stats
                .record_per_ip(key, limit.max, limit.allowlisted, now);
            Err(Rejected::Concurrency(limit))
        };
        if lock(&self.counts)
            .get(&key)
            .is_some_and(|c| *c >= limit.max)
        {
            return reject_concurrency();
        }
        if !gate() {
            self.stats.record_handshake_limit();
            return Err(Rejected::Gate);
        }
        let source = source_key(ip);
        if !self.admit_rate(source, now) {
            let (burst, allowlisted) = self.policy.new_connection_burst(source);
            self.stats
                .record_rate(source, burst as usize, allowlisted, now);
            return Err(Rejected::Rate);
        }
        let mut counts = lock(&self.counts);
        let count = counts.entry(key).or_insert(0);
        if *count >= limit.max {
            drop(counts);
            return reject_concurrency();
        }
        *count += 1;
        Ok(PerIpPermit {
            limiter: self.clone(),
            key,
        })
    }

    /// 改依憑證身分計數（`agent_id` 為正規化後的值）
    fn acquire_identity(self: &Arc<Self>, agent_id: &str) -> IdentityPermit {
        *lock(&self.identities)
            .entry(agent_id.to_string())
            .or_insert(0) += 1;
        IdentityPermit {
            limiter: self.clone(),
            agent_id: agent_id.to_string(),
        }
    }

    pub fn snapshot(&self, now: Instant) -> LimiterSnapshot {
        let (counted_addresses, counted_connections) = {
            let counts = lock(&self.counts);
            (counts.len(), counts.values().sum())
        };
        let (identities, identity_connections) = {
            let ids = lock(&self.identities);
            (ids.len(), ids.values().sum())
        };
        LimiterSnapshot {
            counted_addresses,
            counted_connections,
            identities,
            identity_connections,
            rejections: self.stats.snapshot(now),
        }
    }

    /// `ip` 目前依來源位址計數的連線數
    #[cfg(test)]
    pub fn counted(&self, ip: IpAddr) -> usize {
        lock(&self.counts)
            .get(&address_key(ip))
            .copied()
            .unwrap_or(0)
    }
}

impl Drop for PerIpPermit {
    fn drop(&mut self) {
        let mut counts = lock(&self.limiter.counts);
        if let Some(count) = counts.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.key);
            }
        }
    }
}

impl Drop for IdentityPermit {
    fn drop(&mut self) {
        let mut ids = lock(&self.limiter.identities);
        if let Some(count) = ids.get_mut(&self.agent_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                ids.remove(&self.agent_id);
            }
        }
    }
}

/// 一條連線佔用的名額，依兩階段釋放（見模組說明）：
/// - accept 時：來源位址的名額（[`PerIpPermit`]）與全域「握手中」的名額；
/// - hello 通過（[`ConnectionSlot::hello_passed`]）：歸還握手中的名額；以 mTLS 驗證身分時另歸還
///   來源位址的名額，改依憑證身分計數。
///
/// 連線結束（被釋放）時歸還仍持有的名額。
#[derive(Debug)]
pub struct ConnectionSlot {
    ip: Option<PerIpPermit>,
    handshake: Option<OwnedSemaphorePermit>,
    identity: Option<IdentityPermit>,
}

/// 連線目前的計數方式
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 握手前、握手中或等待 hello：依來源位址計數，並佔用握手中的名額
    PreAuth,
    /// hello 通過、未以 mTLS 驗證：依來源位址計數
    PerIp,
    /// hello 通過、以 mTLS 驗證：依憑證身分計數
    Identity,
}

impl ConnectionSlot {
    pub fn new(ip: PerIpPermit, handshake: Option<OwnedSemaphorePermit>) -> Self {
        Self {
            ip: Some(ip),
            handshake,
            identity: None,
        }
    }

    /// hello 通過時呼叫：歸還「握手中」的名額。`authenticated_agent_id` 為以 mTLS 驗證、
    /// 與憑證綁定的（正規化）agent_id 時，改依憑證身分計數並歸還來源位址的名額；
    /// 為 `None`（未啟用 mTLS）時繼續依來源位址計數
    pub fn hello_passed(&mut self, authenticated_agent_id: Option<&str>) {
        self.handshake = None;
        if let Some(agent_id) = authenticated_agent_id
            && self.identity.is_none()
            && let Some(ip) = self.ip.take()
        {
            self.identity = Some(ip.limiter.acquire_identity(agent_id));
        }
    }

    #[cfg(test)]
    pub fn phase(&self) -> Phase {
        if self.identity.is_some() {
            Phase::Identity
        } else if self.handshake.is_some() {
            Phase::PreAuth
        } else {
            Phase::PerIp
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
            Rejected::Concurrency(Limit {
                max: 2,
                allowlisted: false
            })
        );
        let b1 = l.try_acquire(B, || true).unwrap();
        assert_eq!(l.snapshot(Instant::now()).counted_addresses, 2);

        drop(a1);
        let a3 = l.try_acquire(A, || true).unwrap();
        assert!(l.try_acquire(A, || true).is_err());

        drop((a2, a3, b1));
        // 歸零的 IP 會從計數表移除
        assert_eq!(l.snapshot(Instant::now()).counted_addresses, 0);
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
            Rejected::Concurrency(Limit {
                max: 1,
                allowlisted: false
            })
        );
        assert_eq!(l.snapshot(Instant::now()).counted_addresses, 2);

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
            Rejected::Concurrency(Limit {
                max: 1,
                allowlisted: false
            })
        );
        assert!(!called);
    }

    #[test]
    fn zero_limit_is_treated_as_one() {
        let l = PerIpLimiter::new(0);
        assert_eq!(l.policy().max_per_ip, 1);
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

    fn policy(
        allowlist: &[&str],
        max_per_ip: usize,
        allowlisted: usize,
        mtls: bool,
    ) -> PerIpPolicy {
        PerIpPolicy {
            max_per_ip,
            allowlist: Arc::new(Allowlist::parse(allowlist).unwrap()),
            max_per_allowlisted_ip: allowlisted,
            mtls,
        }
    }

    #[test]
    fn allowlisted_addresses_use_the_allowlisted_limit() {
        // A7 過渡方案：允許清單內的每個位址改用允許清單的上限（含 IPv4-mapped IPv6）
        let l = PerIpLimiter::with_policy(policy(&["192.0.2.0/24", "2001:db8::/32"], 1, 3, false));
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped());
        let held: Vec<_> = (0..3)
            .map(|i| {
                let ip = if i == 0 { mapped } else { A };
                l.try_acquire(ip, || true).unwrap()
            })
            .collect();
        assert_eq!(
            l.try_acquire(A, || true).unwrap_err(),
            Rejected::Concurrency(Limit {
                max: 3,
                allowlisted: true
            })
        );
        // 清單外的位址維持 server.max_connections_per_ip
        let outside = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1));
        let _o = l.try_acquire(outside, || true).unwrap();
        assert_eq!(
            l.try_acquire(outside, || true).unwrap_err(),
            Rejected::Concurrency(Limit {
                max: 1,
                allowlisted: false
            })
        );
        let _v6 = l.try_acquire(v6("2001:db8::1"), || true).unwrap();
        let _v6b = l.try_acquire(v6("2001:db8::1"), || true).unwrap();
        assert_eq!(l.counted(A), 3);
        drop(held);
        assert_eq!(l.counted(A), 0);

        // 被拒的連線依位址計數，附上限與是否在清單內
        let s = l.snapshot(Instant::now()).rejections;
        assert_eq!(s.per_ip.total, 2);
        let a = s.per_ip.sources.iter().find(|r| r.source == A).unwrap();
        assert_eq!((a.rejected, a.limit, a.allowlisted), (1, 3, true));
        let o = s
            .per_ip
            .sources
            .iter()
            .find(|r| r.source == outside)
            .unwrap();
        assert_eq!((o.rejected, o.limit, o.allowlisted), (1, 1, false));
    }

    #[test]
    fn allowlisted_sources_get_a_larger_new_connection_burst() {
        // 新連線的連續上限隨之為 max(32, 2 × 實際上限)（screens.md §24 #17 (a)）
        let p = policy(&["192.0.2.0/24", "2001:db8:1:2::10/128"], 32, 100, false);
        assert_eq!(p.new_connection_burst(A), (200.0, true));
        assert_eq!(
            p.new_connection_burst(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1))),
            (64.0, false)
        );
        // IPv6 以 /64 為來源：與允許清單有交集即套用
        assert_eq!(p.new_connection_burst(v6("2001:db8:1:2::")), (200.0, true));
        assert!(!p.new_connection_burst(v6("2001:db8:1:3::")).1);
        assert_eq!(p.report_scale(A), 100);
        assert_eq!(p.report_scale(v6("2001:db8:1:3::")), 32);

        let l = PerIpLimiter::with_policy(p);
        let t0 = Instant::now();
        for _ in 0..200 {
            drop(l.try_acquire_at(A, t0, || true).unwrap());
        }
        assert_eq!(
            l.try_acquire_at(A, t0, || true).unwrap_err(),
            Rejected::Rate
        );
        let s = l.snapshot(t0).rejections.per_source_rate;
        assert_eq!(s.total, 1);
        assert_eq!((s.sources[0].limit, s.sources[0].allowlisted), (200, true));
    }

    #[tokio::test]
    async fn mtls_identity_replaces_per_ip_counting_after_hello() {
        // A7 根本解：mTLS 下 hello 通過後歸還來源位址的名額，改依憑證身分計數
        let l = PerIpLimiter::with_policy(policy(&[], 2, 2, true));
        let handshakes = Arc::new(tokio::sync::Semaphore::new(10));
        let mut slots = Vec::new();
        let t0 = Instant::now();
        for i in 0..40 {
            // 每秒一條，不觸及新連線速率上限
            let permit = l
                .try_acquire_at(A, t0 + Duration::from_secs(i), || true)
                .unwrap();
            let mut slot = ConnectionSlot::new(
                permit,
                Some(handshakes.clone().try_acquire_owned().unwrap()),
            );
            assert_eq!(slot.phase(), Phase::PreAuth);
            slot.hello_passed(Some(&format!("agent-{i}")));
            assert_eq!(slot.phase(), Phase::Identity);
            slots.push(slot);
        }
        // 同一位址 40 條已驗證的連線：不佔每 IP 名額，也不佔握手中名額
        assert_eq!(l.counted(A), 0);
        assert_eq!(handshakes.available_permits(), 10);
        let snap = l.snapshot(Instant::now());
        assert_eq!((snap.identities, snap.identity_connections), (40, 40));

        // 握手前仍受每 IP 上限保護（慢速握手，K-S2）
        let p1 = ConnectionSlot::new(l.try_acquire(A, || true).unwrap(), None);
        let p2 = ConnectionSlot::new(l.try_acquire(A, || true).unwrap(), None);
        assert!(matches!(
            l.try_acquire(A, || true),
            Err(Rejected::Concurrency(Limit { max: 2, .. }))
        ));
        drop((p1, p2));

        // 同一身分的第二條連線（取代期間）計入同一個身分
        let mut again = ConnectionSlot::new(l.try_acquire(A, || true).unwrap(), None);
        again.hello_passed(Some("agent-0"));
        let snap = l.snapshot(Instant::now());
        assert_eq!((snap.identities, snap.identity_connections), (40, 41));
        drop(slots);
        drop(again);
        let snap = l.snapshot(Instant::now());
        assert_eq!((snap.identities, snap.identity_connections), (0, 0));
        assert_eq!(snap.counted_connections, 0);
    }

    #[test]
    fn without_mtls_registered_connections_keep_counting_per_ip() {
        let l = PerIpLimiter::with_policy(policy(&[], 2, 2, false));
        let mut a = ConnectionSlot::new(l.try_acquire(A, || true).unwrap(), None);
        a.hello_passed(None);
        assert_eq!(a.phase(), Phase::PerIp);
        let _b = l.try_acquire(A, || true).unwrap();
        assert!(matches!(
            l.try_acquire(A, || true),
            Err(Rejected::Concurrency(_))
        ));
        assert_eq!(l.snapshot(Instant::now()).identities, 0);
        drop(a);
        assert!(l.try_acquire(A, || true).is_ok());
    }

    #[test]
    fn gate_rejections_are_counted_as_global() {
        let l = PerIpLimiter::new(4);
        for _ in 0..3 {
            assert_eq!(l.try_acquire(A, || false).unwrap_err(), Rejected::Gate);
        }
        let s = l.snapshot(Instant::now()).rejections;
        assert_eq!(s.global.handshake_limit_rejected, 3);
        assert_eq!(s.per_ip.total, 0);
        assert_eq!(s.per_source_rate.total, 0);
    }

    #[test]
    fn snapshot_summary_lists_both_rejection_classes() {
        let l = PerIpLimiter::new(1);
        let _a = l.try_acquire(A, || true).unwrap();
        assert!(l.try_acquire(A, || true).is_err());
        assert!(l.try_acquire(B, || false).is_err());
        let now = Instant::now();
        l.stats().pause_begin(now);
        l.stats().pause_end(now + Duration::from_millis(1500));
        assert_eq!(
            l.snapshot(now).summary(),
            "啟動以來被拒的連線：因每 IP 上限 1 條（1 個來源位址），因新連線速率 0 條；因全域上限暫停接受新連線 1 次、累計 1.5 秒，握手中上限拒絕 1 條"
        );
    }

    #[test]
    fn policy_describes_both_phases() {
        let d = policy(&["10.0.0.0/8"], 32, 1024, true).describe();
        assert!(d.contains("只計算握手中"), "{d}");
        assert!(d.contains("憑證身分"), "{d}");
        assert!(d.contains("10.0.0.0/8"), "{d}");
        assert!(d.contains("1024 條"), "{d}");
        let d = policy(&[], 32, 1024, false).describe();
        assert!(d.contains("計算全部連線"), "{d}");
        assert!(d.contains("是空的"), "{d}");
    }
}
