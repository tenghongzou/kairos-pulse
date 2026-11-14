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
/// bucket 表的鍵數上限：達到後清不出空位時，新的鍵一律被拒
pub const MAX_KEYS: usize = 262_144;
/// bucket 表已滿時兩次清理的最短間隔
pub const FULL_PRUNE_INTERVAL: Duration = Duration::from_secs(1);
/// L7 追蹤的來源數上限（計數表與封鎖表各自）
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

#[derive(Debug)]
struct KeyedState<K> {
    map: HashMap<K, Gcra>,
    /// 筆數達到這個值時才清理（每次清理後設為剩餘筆數的 2 倍）：清理的成本攤在之後的新鍵上，
    /// 不會每個新鍵都在鎖內掃描整張表
    prune_at: usize,
    last_prune: Option<Instant>,
}

impl<K: Eq + Hash> KeyedState<K> {
    fn prune(&mut self, now: Instant) {
        self.map.retain(|_, b| !b.is_full(now));
        self.last_prune = Some(now);
        self.prune_at = (self.map.len() * 2).clamp(PRUNE_THRESHOLD + 1, MAX_KEYS);
    }

    /// 是否還放得下一個新的鍵（必要時清理）
    fn make_room(&mut self, now: Instant) -> bool {
        if self.map.len() < self.prune_at {
            return true;
        }
        // 已達上限且剛清理過：不再掃描，直接拒絕
        if self.map.len() >= MAX_KEYS
            && self
                .last_prune
                .is_some_and(|t| now.saturating_duration_since(t) < FULL_PRUNE_INTERVAL)
        {
            return false;
        }
        self.prune(now);
        self.map.len() < MAX_KEYS
    }
}

/// 依鍵分開的 bucket 表。最多 [`MAX_KEYS`] 個鍵；滿的時候新的鍵一律被拒（回 `max_wait`），既有的鍵不受影響
#[derive(Debug)]
pub struct Keyed<K: Eq + Hash + Clone> {
    state: Mutex<KeyedState<K>>,
}

impl<K: Eq + Hash + Clone> Default for Keyed<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash + Clone> Keyed<K> {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(KeyedState {
                map: HashMap::new(),
                prune_at: PRUNE_THRESHOLD + 1,
                last_prune: None,
            }),
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
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.map.contains_key(key) && !st.make_room(now) {
            return Err(max_wait);
        }
        let b = st
            .map
            .entry(key.clone())
            .or_insert_with(|| Gcra::new(capacity, rate, now));
        b.retune(capacity, rate);
        b.take(now, max_wait)
    }

    /// 清掉已補滿的項目
    pub fn prune(&self, now: Instant) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prune(now);
    }

    /// 目前的鍵數
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
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

/// 一個來源在時間窗內的相異無效值。筆數不會超過政策的上限：達到上限就封鎖並清空，封鎖期間不再記錄
#[derive(Debug, Default)]
struct SourceValues {
    /// 無效值（SHA-256）→ 最後一次出現的時間
    values: HashMap<[u8; 32], i64>,
    /// 這個來源最後一次被記錄的時間（淘汰時不必掃描每個值）
    last_ms: i64,
}

#[derive(Debug, Default)]
struct InvalidState {
    seen: HashMap<(IpAddr, InvalidClass), SourceValues>,
    blocks: HashMap<(IpAddr, InvalidClass), Block>,
}

/// 表滿時一次淘汰的比例（1/8）：之後的 `上限 ÷ 8` 個新來源不必再掃描整張表
const EVICT_DIVISOR: usize = 8;

/// 從 `map` 淘汰 `key_of` 最小的 1/8（至少 1 筆）
fn evict_oldest<K: Eq + Hash, V>(map: &mut HashMap<K, V>, key_of: impl Fn(&V) -> i64) {
    let mut keys: Vec<i64> = map.values().map(&key_of).collect();
    if keys.is_empty() {
        return;
    }
    let n = (keys.len() / EVICT_DIVISOR).max(1);
    let (_, cutoff, _) = keys.select_nth_unstable(n - 1);
    let cutoff = *cutoff;
    let mut left = n;
    map.retain(|_, v| {
        if left > 0 && key_of(v) <= cutoff {
            left -= 1;
            false
        } else {
            true
        }
    });
}

impl InvalidState {
    /// 計數表已滿、要記錄新的來源：先清掉時間窗外的來源，仍然滿時淘汰最久沒有出現的一批
    fn make_room_for_source(&mut self, now_ms: i64, window_ms: i64) {
        self.seen.retain(|_, s| now_ms - s.last_ms < window_ms);
        if self.seen.len() >= MAX_TRACKED_SOURCES {
            evict_oldest(&mut self.seen, |s| s.last_ms);
        }
    }

    /// 封鎖表超過上限：先清掉已結束且再犯紀錄已歸零的，仍然超過時淘汰最早結束的一批
    fn trim_blocks(&mut self, now_ms: i64) {
        self.blocks
            .retain(|_, b| b.until_ms > now_ms || now_ms - b.last_offense_ms < OFFENSE_RESET_MS);
        if self.blocks.len() > MAX_TRACKED_SOURCES {
            evict_oldest(&mut self.blocks, |b| b.until_ms);
        }
    }
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
        // 封鎖中：不再累積（封鎖結束後重新計數）。否則被封鎖的來源每送一個相異值就多佔一筆
        if st
            .blocks
            .get(&(source, class))
            .is_some_and(|b| b.until_ms > now_ms)
        {
            st.seen.remove(&(source, class));
            return None;
        }
        if st.seen.len() >= MAX_TRACKED_SOURCES && !st.seen.contains_key(&(source, class)) {
            st.make_room_for_source(now_ms, policy.window_ms);
        }
        let distinct = {
            let s = st.seen.entry((source, class)).or_default();
            s.values.retain(|_, t| now_ms - *t < policy.window_ms);
            s.values.insert(value, now_ms);
            s.last_ms = now_ms;
            s.values.len()
        };
        if distinct < limit {
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
            st.trim_blocks(now_ms);
        }
        Some(block)
    }

    /// (計數表的來源數, 其中相異無效值的總數, 封鎖表的筆數)
    #[cfg(test)]
    pub fn sizes(&self) -> (usize, usize, usize) {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            st.seen.len(),
            st.seen.values().map(|s| s.values.len()).sum(),
            st.blocks.len(),
        )
    }

    /// 解除封鎖
    #[cfg(test)]
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
    #[cfg(test)]
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

    // 安全稽核 C1：bucket 表有上限，滿的時候新的鍵被拒、既有的鍵照常，項目補滿後空位會回來
    #[test]
    fn keyed_table_is_bounded_and_rejects_new_keys_when_full() {
        let t0 = Instant::now();
        let k: Keyed<u32> = Keyed::new();
        // 每個鍵取 1 次、幾乎不補充：清理時沒有任何項目是滿的
        for i in 0..MAX_KEYS as u32 {
            assert!(k.take(&i, 2.0, 1e-9, t0, MAX).is_ok());
        }
        assert_eq!(k.len(), MAX_KEYS);
        for i in 0..1000u32 {
            assert_eq!(
                k.take(&(MAX_KEYS as u32 + i), 2.0, 1e-9, t0, MAX),
                Err(MAX),
                "表已滿：新的鍵被拒"
            );
        }
        assert_eq!(k.len(), MAX_KEYS, "被拒的鍵不佔位置");
        assert!(k.take(&0, 2.0, 1e-9, t0, MAX).is_ok(), "既有的鍵不受影響");
        // 補充得快的表：項目補滿後被清掉，新的鍵可以進來
        let k: Keyed<u32> = Keyed::new();
        for i in 0..(PRUNE_THRESHOLD as u32 * 4) {
            assert!(k.take(&i, 2.0, 1.0, t0, MAX).is_ok());
        }
        let later = t0 + Duration::from_secs(10);
        k.prune(later);
        assert_eq!(k.len(), 0, "已補滿的項目在清理時移除");
        assert!(k.take(&u32::MAX, 2.0, 1.0, later, MAX).is_ok());
    }

    // 安全稽核 C1：封鎖期間不再累積無效值
    #[test]
    fn l7_does_not_accumulate_values_while_blocked() {
        let t = InvalidTracker::new(policy());
        let ip: IpAddr = "203.0.113.20".parse().unwrap();
        for i in 0..20u8 {
            t.record(ip, InvalidClass::Enroll, [i; 32], 0);
        }
        assert!(t.blocked(ip, InvalidClass::Enroll, 1).is_some());
        for i in 0..10_000u32 {
            let mut v = [0u8; 32];
            v[..4].copy_from_slice(&i.to_be_bytes());
            v[31] = 0xff;
            assert!(t.record(ip, InvalidClass::Enroll, v, 1000).is_none());
        }
        assert_eq!(t.sizes(), (0, 0, 1), "封鎖中的來源不佔計數表");
        // 未被封鎖的來源：相異值不超過政策的上限（達到就封鎖並清空）
        let other: IpAddr = "203.0.113.21".parse().unwrap();
        for i in 0..19u8 {
            t.record(other, InvalidClass::Renew, [i; 32], 1000);
        }
        assert_eq!(t.sizes(), (1, 19, 1));
    }

    // 安全稽核 C1：來源很多時計數表與封鎖表都有上限
    #[test]
    fn l7_tables_are_bounded_across_many_sources() {
        let t = InvalidTracker::new(policy());
        let n = MAX_TRACKED_SOURCES as u32 + 20_000;
        for i in 0..n {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(0x0b00_0000 + i));
            t.record(ip, InvalidClass::Enroll, [1; 32], i64::from(i));
        }
        let (sources, values, _) = t.sizes();
        assert!(sources <= MAX_TRACKED_SOURCES, "{sources}");
        assert!(values <= MAX_TRACKED_SOURCES, "{values}");
        // 最近的來源還在（淘汰的是最久沒有出現的）
        let last = IpAddr::V4(std::net::Ipv4Addr::from(0x0b00_0000 + n - 1));
        for i in 0..19u8 {
            t.record(last, InvalidClass::Enroll, [i + 2; 32], i64::from(n));
        }
        assert!(t.blocked(last, InvalidClass::Enroll, i64::from(n)).is_some());

        // 每個來源一個無效值就封鎖：封鎖表不超過上限，最近的封鎖仍有效
        let t = InvalidTracker::new(InvalidPolicy {
            enroll_limit: 1,
            ..policy()
        });
        for i in 0..n {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(0x0b00_0000 + i));
            assert!(
                t.record(ip, InvalidClass::Enroll, [1; 32], i64::from(i))
                    .is_some()
            );
        }
        let (sources, _, blocks) = t.sizes();
        assert_eq!(sources, 0);
        assert!(blocks <= MAX_TRACKED_SOURCES, "{blocks}");
        assert!(t.blocked(last, InvalidClass::Enroll, i64::from(n)).is_some());
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
