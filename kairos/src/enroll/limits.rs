//! 8444 的速率限制（`crates/kairos-proto/ENROLL.md` 第 11 節、設計包 `decisions/enroll-rate-limit.md`）。
//!
//! - [`Gcra`]：token bucket，額度用完時依排隊位置計算 `Retry-After`（`enroll-rate-limit.md` §5.1：
//!   每回一次 429 就把 `next_free_at` 往後推 1 個間隔，被拒的請求分散到後面的時段，不會同時回來再撞一次）。
//! - [`Keyed`]：依鍵（來源、token、`agent_id`、申請）分開的 bucket 表，超過 4,096 筆時清掉已補滿的項目。
//! - [`InvalidTracker`]：L7「相異無效值」計數與封鎖（新註冊類、續簽類分開）。
//! - [`Slots`]：同時數的名額（長輪詢、每個位址的同時連線）。
//!
//! 規則與 8443 的 `per_ip.rs` 相同：被前面某層拒絕的請求不扣後面各層的額度——呼叫端依判定順序逐層
//! [`Gcra::take`]，某層拒絕就停止，不會去扣後面的。

use std::collections::HashMap;
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// bucket 表超過此筆數時清掉已補滿的項目
pub const PRUNE_THRESHOLD: usize = 4096;
/// L7 追蹤的來源數上限（封鎖表）
pub const MAX_TRACKED_SOURCES: usize = 65_536;

/// token bucket＋排隊位置（GCRA）
#[derive(Debug, Clone)]
pub struct Gcra {
    capacity: f64,
    rate: f64,
    tokens: f64,
    last: Instant,
    /// 下一個排隊位置
    next_free: Instant,
}

impl Gcra {
    /// 初始為滿的 bucket：可連續 `capacity` 次，每秒補充 `rate`
    pub fn new(capacity: f64, rate: f64, now: Instant) -> Self {
        Self {
            capacity: capacity.max(1.0),
            rate: rate.max(1e-9),
            tokens: capacity.max(1.0),
            last: now,
            next_free: now,
        }
    }

    /// 調整容量與速率（設定改變、批次部署來源的倍數）；目前的 token 不超過新容量
    pub fn retune(&mut self, capacity: f64, rate: f64) {
        self.capacity = capacity.max(1.0);
        self.rate = rate.max(1e-9);
        self.tokens = self.tokens.min(self.capacity);
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = self.last.max(now);
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
    }

    /// 取一次額度。用完時回傳應等待的時間（依排隊位置，不超過 `max_wait`；超過時回 `max_wait` 且
    /// 不推進排隊位置）
    pub fn take(&mut self, now: Instant, max_wait: Duration) -> Result<(), Duration> {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(());
        }
        let deficit = Duration::from_secs_f64((1.0 - self.tokens) / self.rate);
        let slot = (now + deficit).max(self.next_free);
        let wait = slot.saturating_duration_since(now);
        if wait > max_wait {
            return Err(max_wait);
        }
        self.next_free = slot + Duration::from_secs_f64(1.0 / self.rate);
        Err(wait.max(Duration::from_millis(1)))
    }

    /// 不扣額度，只看是否還有額度
    pub fn has_token(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= 1.0
    }

    /// 補充後是否已滿（閒置，可以清掉）
    pub fn is_full(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= self.capacity
    }
}

/// `Retry-After`（整數秒，無條件進位，至少 1）
pub fn retry_after_secs(wait: Duration) -> u64 {
    (wait.as_millis().div_ceil(1000) as u64).max(1)
}

/// 依鍵分開的 bucket 表
#[derive(Debug, Default)]
pub struct Keyed<K: Eq + Hash + Clone> {
    map: Mutex<HashMap<K, Gcra>>,
}

impl<K: Eq + Hash + Clone> Keyed<K> {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// 以 `capacity`／`rate` 取 `key` 的一次額度（容量與速率可每次不同，例如批次部署來源）
    pub fn take(
        &self,
        key: &K,
        capacity: f64,
        rate: f64,
        now: Instant,
        max_wait: Duration,
    ) -> Result<(), Duration> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > PRUNE_THRESHOLD && !map.contains_key(key) {
            map.retain(|_, b| !b.is_full(now));
        }
        let b = map
            .entry(key.clone())
            .or_insert_with(|| Gcra::new(capacity, rate, now));
        b.retune(capacity, rate);
        b.take(now, max_wait)
    }

    /// 筆數（測試、系統健康）
    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 清掉已補滿的項目
    pub fn prune(&self, now: Instant) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, b| !b.is_full(now));
    }
}

/// L7 的兩個類別（分開計數、分開封鎖）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InvalidClass {
    /// 新註冊類：不存在／停用的 token、錯誤的 `poll_secret`
    Enroll,
    /// 續簽類：`renew_identity_mismatch`、`replayed_request`、`withdraw_signature_invalid`
    Renew,
}

impl InvalidClass {
    pub fn as_str(self) -> &'static str {
        match self {
            InvalidClass::Enroll => "enroll",
            InvalidClass::Renew => "renew",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "enroll" => Some(InvalidClass::Enroll),
            "renew" => Some(InvalidClass::Renew),
            _ => None,
        }
    }
}

/// L7 的參數
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidPolicy {
    pub enroll_limit: usize,
    pub renew_limit: usize,
    pub window_ms: i64,
    pub block_initial_ms: i64,
    pub block_max_ms: i64,
}

/// 一個來源的封鎖狀態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub until_ms: i64,
    pub offenses: i64,
    pub last_offense_ms: i64,
}

/// 封鎖的再犯紀錄在 24 小時沒有再犯後歸零
pub const OFFENSE_RESET_MS: i64 = 86_400_000;

#[derive(Debug, Default)]
struct InvalidState {
    /// (來源, 類別) → 相異無效值（SHA-256）→ 最後一次出現的時間
    seen: HashMap<(IpAddr, InvalidClass), HashMap<[u8; 32], i64>>,
    blocks: HashMap<(IpAddr, InvalidClass), Block>,
}

/// L7：每個來源在時間窗內嘗試過的**相異**無效值。同一個無效值重複出現（NAT 後的 300 台主機帶著同一個
/// 舊 token）只算 1 個；達到上限時封鎖該來源的這一類請求，再犯時封鎖時間加倍、上限 `block_max`
#[derive(Debug)]
pub struct InvalidTracker {
    policy: Mutex<InvalidPolicy>,
    state: Mutex<InvalidState>,
}

impl InvalidTracker {
    pub fn new(policy: InvalidPolicy) -> Self {
        Self {
            policy: Mutex::new(policy),
            state: Mutex::new(InvalidState::default()),
        }
    }

    pub fn set_policy(&self, policy: InvalidPolicy) {
        *self.policy.lock().unwrap_or_else(|e| e.into_inner()) = policy;
    }

    /// 載入資料庫中的封鎖（重新啟動後仍有效）
    pub fn restore(&self, source: IpAddr, class: InvalidClass, block: Block) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.blocks.insert((source, class), block);
    }

    /// 封鎖中時回傳剩餘的毫秒數
    pub fn blocked(&self, source: IpAddr, class: InvalidClass, now_ms: i64) -> Option<i64> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.blocks
            .get(&(source, class))
            .filter(|b| b.until_ms > now_ms)
            .map(|b| b.until_ms - now_ms)
    }

    /// 記錄一個無效值；達到上限而開始封鎖時回傳新的封鎖（呼叫端寫入資料庫並記 `KRS-W3005`）
    pub fn record(
        &self,
        source: IpAddr,
        class: InvalidClass,
        value: [u8; 32],
        now_ms: i64,
    ) -> Option<Block> {
        let policy = *self.policy.lock().unwrap_or_else(|e| e.into_inner());
        let limit = match class {
            InvalidClass::Enroll => policy.enroll_limit,
            InvalidClass::Renew => policy.renew_limit,
        };
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.seen.len() >= MAX_TRACKED_SOURCES && !st.seen.contains_key(&(source, class)) {
            // 清掉時間窗外的值；仍然太多時清掉最舊的來源
            st.seen.retain(|_, m| {
                m.retain(|_, t| now_ms - *t < policy.window_ms);
                !m.is_empty()
            });
            if st.seen.len() >= MAX_TRACKED_SOURCES
                && let Some(oldest) = st
                    .seen
                    .iter()
                    .min_by_key(|(_, m)| m.values().max().copied().unwrap_or(0))
                    .map(|(k, _)| *k)
            {
                st.seen.remove(&oldest);
            }
        }
        let distinct = {
            let values = st.seen.entry((source, class)).or_default();
            values.retain(|_, t| now_ms - *t < policy.window_ms);
            values.insert(value, now_ms);
            values.len()
        };
        if distinct < limit {
            return None;
        }
        if st
            .blocks
            .get(&(source, class))
            .is_some_and(|b| b.until_ms > now_ms)
        {
            return None;
        }
        let prev = st.blocks.get(&(source, class)).copied();
        let offenses = match prev {
            Some(b) if now_ms - b.last_offense_ms < OFFENSE_RESET_MS => b.offenses + 1,
            _ => 0,
        };
        let duration = policy
            .block_initial_ms
            .saturating_mul(1i64 << offenses.min(20))
            .min(policy.block_max_ms);
        let block = Block {
            until_ms: now_ms + duration,
            offenses,
            last_offense_ms: now_ms,
        };
        st.blocks.insert((source, class), block);
        // 封鎖後重新計數
        st.seen.remove(&(source, class));
        if st.blocks.len() > MAX_TRACKED_SOURCES {
            st.blocks.retain(|_, b| {
                b.until_ms > now_ms || now_ms - b.last_offense_ms < OFFENSE_RESET_MS
            });
        }
        Some(block)
    }

    /// 解除封鎖
    pub fn unblock(&self, source: IpAddr, class: InvalidClass) -> bool {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.blocks.remove(&(source, class)).is_some()
    }

    /// 封鎖中的來源數（系統健康）
    pub fn blocked_count(&self, now_ms: i64) -> usize {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.blocks.values().filter(|b| b.until_ms > now_ms).count()
    }
}

/// 同時數的名額表（每個鍵最多 `per_key`、全部合計最多 `total`）。取得的名額在 [`SlotGuard`] 釋放時歸還
#[derive(Debug)]
pub struct Slots<K: Eq + Hash + Clone> {
    per_key: Mutex<HashMap<K, usize>>,
    total: AtomicUsize,
}

impl<K: Eq + Hash + Clone> Default for Slots<K> {
    fn default() -> Self {
        Self {
            per_key: Mutex::new(HashMap::new()),
            total: AtomicUsize::new(0),
        }
    }
}

/// 一個名額；釋放時歸還
pub struct SlotGuard<K: Eq + Hash + Clone> {
    slots: Arc<Slots<K>>,
    key: K,
}

impl<K: Eq + Hash + Clone> Drop for SlotGuard<K> {
    fn drop(&mut self) {
        self.slots.release(&self.key);
    }
}

impl<K: Eq + Hash + Clone> Slots<K> {
    /// 取得一個名額；已達任一上限時 `None`
    pub fn try_acquire(
        self: &Arc<Self>,
        key: K,
        per_key: usize,
        total: usize,
    ) -> Option<SlotGuard<K>> {
        let mut map = self.per_key.lock().unwrap_or_else(|e| e.into_inner());
        let used = map.get(&key).copied().unwrap_or(0);
        if used >= per_key || self.total.load(Ordering::Relaxed) >= total {
            return None;
        }
        map.insert(key.clone(), used + 1);
        self.total.fetch_add(1, Ordering::Relaxed);
        Some(SlotGuard {
            slots: self.clone(),
            key,
        })
    }

    fn release(&self, key: &K) {
        let mut map = self.per_key.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(key) {
            *n -= 1;
            if *n == 0 {
                map.remove(key);
            }
        }
        self.total.fetch_sub(1, Ordering::Relaxed);
    }

    /// 目前使用中的名額
    pub fn in_use(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: Duration = Duration::from_secs(600);

    #[test]
    fn gcra_spreads_retry_after_by_queue_position() {
        let t0 = Instant::now();
        // 每秒 10 次、可連續 600 次（L4）
        let mut b = Gcra::new(600.0, 10.0, t0);
        for _ in 0..600 {
            assert!(b.take(t0, MAX).is_ok());
        }
        // 第 601 個起依序排隊：0.1、0.2、0.3… 秒
        let waits: Vec<Duration> = (0..5).map(|_| b.take(t0, MAX).unwrap_err()).collect();
        for w in waits.windows(2) {
            assert!(w[1] > w[0], "{waits:?}");
        }
        assert_eq!(retry_after_secs(waits[0]), 1);
        // 等到排隊的時間後可以取得
        assert!(b.take(t0 + Duration::from_millis(150), MAX).is_ok());
    }

    #[test]
    fn gcra_caps_retry_after_and_does_not_advance_past_the_cap() {
        let t0 = Instant::now();
        // L6r：每小時 10 次
        let mut b = Gcra::new(10.0, 10.0 / 3600.0, t0);
        for _ in 0..10 {
            assert!(b.take(t0, MAX).is_ok());
        }
        // 第 11 次排在 6 分鐘後；第 12 次要等 12 分鐘，超過上限 10 分鐘：回上限
        assert_eq!(b.take(t0, MAX).unwrap_err(), Duration::from_secs(360));
        assert_eq!(b.take(t0, MAX).unwrap_err(), MAX, "超過上限時回上限");
        assert_eq!(b.take(t0, MAX).unwrap_err(), MAX);
        // 上限沒有推進排隊位置：6 分鐘後就可以
        assert!(b.take(t0 + Duration::from_secs(361), MAX).is_ok());
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(42_001)), 43);
    }

    #[test]
    fn rejected_layers_do_not_consume_later_layers() {
        // 呼叫端依序檢查：前一層拒絕就不呼叫下一層
        let t0 = Instant::now();
        let ip: Keyed<u8> = Keyed::new();
        let token: Keyed<u8> = Keyed::new();
        let mut granted = 0;
        for _ in 0..10 {
            if ip.take(&1, 3.0, 0.001, t0, MAX).is_ok()
                && token.take(&1, 100.0, 1.0, t0, MAX).is_ok()
            {
                granted += 1;
            }
        }
        assert_eq!(granted, 3);
        // token 層只被扣了 3 次
        for _ in 0..97 {
            assert!(token.take(&1, 100.0, 1e-9, t0, MAX).is_ok());
        }
        assert!(token.take(&1, 100.0, 1e-9, t0, MAX).is_err());
    }

    fn policy() -> InvalidPolicy {
        InvalidPolicy {
            enroll_limit: 20,
            renew_limit: 20,
            window_ms: 3_600_000,
            block_initial_ms: 900_000,
            block_max_ms: 86_400_000,
        }
    }

    #[test]
    fn l7_counts_only_distinct_values_and_doubles_the_block() {
        let t = InvalidTracker::new(policy());
        let ip: IpAddr = "203.0.113.10".parse().unwrap();
        // 同一個無效值重複 1,000 次（NAT 後的舊 token）不封鎖
        for _ in 0..1000 {
            assert!(t.record(ip, InvalidClass::Enroll, [1; 32], 0).is_none());
        }
        assert_eq!(t.blocked(ip, InvalidClass::Enroll, 0), None);
        // 第 20 個相異值（含先前的 1 個）時封鎖 15 分鐘；封鎖中不再產生新的封鎖
        let blocks: Vec<Block> = (0..20u8)
            .filter_map(|i| t.record(ip, InvalidClass::Enroll, [i + 2; 32], 1000))
            .collect();
        assert_eq!(blocks.len(), 1);
        let b = blocks[0];
        assert_eq!(b.until_ms, 1000 + 900_000);
        assert_eq!(t.blocked(ip, InvalidClass::Enroll, 2000), Some(899_000));
        // 續簽類不受影響
        assert_eq!(t.blocked(ip, InvalidClass::Renew, 2000), None);
        // 封鎖結束後再犯：30 分鐘
        let now = 1000 + 900_000 + 1;
        let again: Vec<Block> = (0..20u8)
            .filter_map(|i| t.record(ip, InvalidClass::Enroll, [i + 100; 32], now))
            .collect();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].until_ms, now + 1_800_000);
        // 時間窗外的值不算
        let t = InvalidTracker::new(policy());
        for i in 0..19u8 {
            t.record(ip, InvalidClass::Renew, [i; 32], 0);
        }
        assert!(
            t.record(ip, InvalidClass::Renew, [99; 32], 3_600_001)
                .is_none()
        );
    }

    #[test]
    fn l7_block_duration_is_capped_and_resets_after_a_quiet_day() {
        let t = InvalidTracker::new(policy());
        let ip: IpAddr = "198.51.100.7".parse().unwrap();
        t.restore(
            ip,
            InvalidClass::Enroll,
            Block {
                until_ms: 0,
                offenses: 10,
                last_offense_ms: 0,
            },
        );
        let mut b = None;
        for i in 0..20u8 {
            b = t.record(ip, InvalidClass::Enroll, [i; 32], 1000);
        }
        assert_eq!(b.unwrap().until_ms, 1000 + 86_400_000);
        let t = InvalidTracker::new(policy());
        t.restore(
            ip,
            InvalidClass::Enroll,
            Block {
                until_ms: 0,
                offenses: 3,
                last_offense_ms: 0,
            },
        );
        let now = OFFENSE_RESET_MS + 1;
        for i in 0..19u8 {
            t.record(ip, InvalidClass::Enroll, [i; 32], now);
        }
        let b = t.record(ip, InvalidClass::Enroll, [77; 32], now).unwrap();
        assert_eq!((b.offenses, b.until_ms), (0, now + 900_000));
        assert!(t.unblock(ip, InvalidClass::Enroll));
        assert_eq!(t.blocked(ip, InvalidClass::Enroll, now), None);
    }

    #[test]
    fn slots_limit_per_key_and_total() {
        let s: Arc<Slots<u8>> = Arc::new(Slots::default());
        let a1 = s.try_acquire(1, 2, 3).unwrap();
        let _a2 = s.try_acquire(1, 2, 3).unwrap();
        assert!(s.try_acquire(1, 2, 3).is_none());
        let _b = s.try_acquire(2, 2, 3).unwrap();
        assert!(s.try_acquire(3, 2, 3).is_none(), "全域上限");
        drop(a1);
        let _c = s.try_acquire(1, 2, 3).unwrap();
        assert_eq!(s.in_use(), 3);
    }
}
