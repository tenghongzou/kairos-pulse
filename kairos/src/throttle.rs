//! 日誌節流：可能被掃描或攻擊大量觸發的日誌（握手失敗、連線錯誤、協定違規、上線／離線等），
//! 每個時間窗最多記錄一次，並在下一次記錄時附上期間略過的則數。
//!
//! 節流的 key 是「類別」或「類別 + 來源」：
//! - 只以類別為 key：同一類別全域共用一個時間窗（例如握手失敗、accept 失敗）。
//! - 以類別 + 來源 IP 為 key：攻擊來源只會壓掉自己的訊息，不會掩蓋其他來源（例如撤銷、
//!   auth_failed，以及未啟用 mTLS 時的上線、離線）。來源以 [`crate::per_ip::source_key`] 正規化
//!   （IPv6 以 /64 合併；R2-K11）。
//!   為了避免大量來源仍能洗版，同一類別在一個時間窗內最多記錄 [`SOURCE_BUDGET_PER_WINDOW`] 則；
//!   因總量上限而未記錄的則數會在該類別下一次記錄時附註，不會無聲消失。
//!   每個類別各自最多追蹤 [`MAX_KEYS_PER_CATEGORY`] 個來源，超過時該類別退回以類別為 key，
//!   不影響其他類別。
//!   事件帶有 agent_id 時（未啟用 mTLS 的上線、離線等），被略過的 agent_id 會在該來源下一次
//!   記錄時列出（最多列 [`SKIPPED_AGENTS_LISTED`] 個），同一 NAT 後的多個 agent 不會無從得知。
//! - 以類別 + agent_id 為 key（F1）：啟用 mTLS 時，已通過憑證驗證的 agent 事件（上線、重新上線、
//!   離線、被取代、取代舊連線）以 agent_id 節流，同一來源 IP 的多個 agent 各自記錄。
//!   agent_id 與客戶端憑證綁定，數量受已簽發的憑證限制，因此不套用每類別的總量上限；
//!   同一 agent 每個時間窗最多記錄 [`AGENT_EVENTS_PER_WINDOW`] 則（互踢、短暫重連仍逐筆記錄，
//!   持有有效憑證者快速重連洗版時才略過並在下一則附註）。每個類別最多追蹤
//!   [`MAX_AGENT_KEYS_PER_CATEGORY`] 個 agent，超過時退回以類別為 key。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::per_ip::source_key;

/// 預設時間窗：同一個 key 每 10 秒最多記一次
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

/// 以「類別 + 來源 IP」為 key 時，同一類別在一個時間窗內最多記錄的則數（所有來源合計）
pub const SOURCE_BUDGET_PER_WINDOW: u32 = 20;

/// 每個類別同時追蹤的來源 IP 數上限，避免以大量來源 IP 灌爆節流表本身
pub const MAX_KEYS_PER_CATEGORY: usize = 1024;

/// 每個類別同時追蹤的（已通過 mTLS 驗證的）agent_id 數上限。
/// 不小於 `server.max_connections` 的上限（65535），同時在線的 agent 都能各自記錄
pub const MAX_AGENT_KEYS_PER_CATEGORY: usize = 65536;

/// 以 agent_id 為 key 時，同一 agent、同一類別在一個時間窗內最多記錄的則數。
/// 同一 agent_id 的兩個實例互踢（各自退避重連，約每 1～2 秒一次）不會超過
pub const AGENT_EVENTS_PER_WINDOW: u32 = 10;

/// 「另有 N 則略過」附註中最多列出的 agent_id 數
pub const SKIPPED_AGENTS_LISTED: usize = 5;

/// 每個來源最多記住的被略過 agent_id 數（相異）；超過時附註改為「至少」
pub const MAX_TRACKED_SKIPPED_AGENTS: usize = 32;

/// 會被節流的日誌類別
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    /// TLS 握手失敗（無客戶端憑證、未知 CA、過期、逾時、非 TLS 流量等）
    Handshake,
    /// 握手時客戶端憑證已被撤銷（依來源分開節流，不會被一般握手失敗掩蓋）
    CertificateRevoked,
    /// 握手時因 CRL 過期或無法判定撤銷狀態而失敗：多半是伺服器端的 CRL 需要更新
    RevocationStatus,
    /// 握手後違反協定（hello_required、unsupported_version、too_long、連續無效訊息）
    ProtocolViolation,
    /// 握手後未在時限內送出 hello
    HelloTimeout,
    /// hello 的 agent_id 與客戶端憑證不符，或憑證在 hello 時已不再有效
    AuthFailed,
    /// 其他連線層級錯誤
    ConnectionError,
    /// accept 失敗（例如檔案描述符用盡）
    Accept,
    /// 同一來源超過同時連線上限
    PerIpLimit,
    /// 同一來源建立新連線的速率超過上限
    ConnectionRate,
    /// 握手中（尚未完成 hello）的連線數已達上限
    HandshakeLimit,
    /// agent 上線（註冊、取代舊連線）
    AgentOnline,
    /// agent 離線（連線關閉、被取代）
    AgentOffline,
    /// 宣告的 heartbeat_interval_ms 超出範圍
    HeartbeatClamped,
    /// 內容無效的 report（invalid_report）
    InvalidReport,
    /// report 送得太快，被延後處理或拒絕
    ReportRate,
    /// 單一連線送出訊息的速率超過上限，被延後處理
    MessageRate,
    /// 狀態表已達上限且沒有可淘汰的離線 agent，拒絕新 agent
    RegistryFull,
    /// 狀態表已達上限，淘汰離線最久的 agent
    RegistryEvicted,
    /// report 寫入逾時且結果未知，關閉連線
    WriteUnknown,
    /// report 檔開檔或寫入失敗、達到每日大小上限
    StorageWrite,
}

/// 節流的來源
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Source {
    /// 來源 IP（已以 [`source_key`] 正規化）
    Ip(IpAddr),
    /// 已通過 mTLS 驗證、與客戶端憑證綁定的 agent_id
    Agent(String),
}

/// 節流的 key：類別，或類別 + 來源
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub category: Category,
    pub source: Option<Source>,
    /// 事件所屬的 agent_id：不影響節流，只用於在「另有 N 則略過」附註中列出被略過的 agent
    pub agent: Option<String>,
}

impl Key {
    /// 已註冊 agent 的事件（上線、重新上線、離線、被取代、取代舊連線；F1）。
    /// `authenticated`（已以 mTLS 驗證 agent_id）時以 agent_id 節流，同一來源 IP 的多個 agent
    /// 各自記錄；否則 agent_id 可任意宣告，仍以來源 IP 節流防止灌爆，並記下被略過的 agent_id。
    pub fn agent_event(
        category: Category,
        ip: IpAddr,
        agent_id: &str,
        authenticated: bool,
    ) -> Self {
        if authenticated {
            Self {
                category,
                source: Some(Source::Agent(agent_id.to_string())),
                agent: None,
            }
        } else {
            Self {
                category,
                source: Some(Source::Ip(source_key(ip))),
                agent: Some(agent_id.to_string()),
            }
        }
    }
}

impl From<Category> for Key {
    fn from(category: Category) -> Self {
        Self {
            category,
            source: None,
            agent: None,
        }
    }
}

impl From<(Category, IpAddr)> for Key {
    fn from((category, ip): (Category, IpAddr)) -> Self {
        Self {
            category,
            source: Some(Source::Ip(source_key(ip))),
            agent: None,
        }
    }
}

/// 同一個 key 上次記錄後被略過的 agent_id（相異，依出現順序）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkippedAgents {
    ids: Vec<String>,
    /// 相異的 agent_id 超過 [`MAX_TRACKED_SKIPPED_AGENTS`]，有些沒有記下
    overflow: bool,
}

impl SkippedAgents {
    fn add(&mut self, agent_id: &str) {
        if self.ids.iter().any(|id| id == agent_id) {
            return;
        }
        if self.ids.len() < MAX_TRACKED_SKIPPED_AGENTS {
            self.ids.push(agent_id.to_string());
        } else {
            self.overflow = true;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// 例如 `a、b、c、d、e 等其餘 2 個`；超過記錄上限時為 `等其餘至少 27 個`
    fn describe(&self) -> String {
        let listed = self.ids.len().min(SKIPPED_AGENTS_LISTED);
        let mut out = self.ids[..listed].join("、");
        let rest = self.ids.len() - listed;
        if self.overflow {
            out.push_str(&format!(" 等其餘至少 {} 個", rest + 1));
        } else if rest > 0 {
            out.push_str(&format!(" 等其餘 {} 個", rest));
        }
        out
    }
}

/// 是否應記錄這一則
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 記錄；`suppressed` 為同一 key 上次記錄後略過的則數，
    /// `over_budget` 為先前同一類別因總量上限而未記錄的則數（所有來源合計），
    /// `skipped` 為同一 key 上次記錄後被略過的 agent_id
    Log {
        suppressed: u64,
        over_budget: u64,
        skipped: SkippedAgents,
    },
    /// 略過
    Suppress,
}

impl Decision {
    /// 附加在日誌訊息後的說明（沒有略過時為空字串）
    pub fn suffix(&self) -> String {
        let mut out = String::new();
        if let Decision::Log {
            suppressed,
            over_budget,
            skipped,
        } = self
        {
            if *suppressed > 0 {
                if skipped.is_empty() {
                    out.push_str(&format!("（上次記錄後另有 {} 則略過）", suppressed));
                } else {
                    out.push_str(&format!(
                        "（上次記錄後另有 {} 則略過，涉及 agent：{}）",
                        suppressed,
                        skipped.describe()
                    ));
                }
            }
            if *over_budget > 0 {
                out.push_str(&format!(
                    "（先前此類訊息另有 {} 則因每 {} 秒 {} 則的總量上限未記錄）",
                    over_budget,
                    DEFAULT_INTERVAL.as_secs(),
                    SOURCE_BUDGET_PER_WINDOW
                ));
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
struct Entry {
    /// 上次記錄的時間；以 agent_id 為 key 時為目前時間窗的開始時間
    last_logged: Instant,
    suppressed: u64,
    skipped: SkippedAgents,
    /// 以 agent_id 為 key 時，目前時間窗內已記錄的則數
    logged_in_window: u32,
}

impl Entry {
    fn logged_at(now: Instant) -> Self {
        Self {
            last_logged: now,
            suppressed: 0,
            skipped: SkippedAgents::default(),
            logged_in_window: 1,
        }
    }

    /// 記下一則略過（`agent` 為該則所屬的 agent_id）
    fn suppress(&mut self, agent: Option<&str>) {
        self.suppressed += 1;
        if let Some(id) = agent {
            self.skipped.add(id);
        }
    }
}

/// 以「類別 + 來源 IP」為 key 時，同一類別在目前時間窗內已記錄的則數
#[derive(Debug, Clone, Copy)]
struct Budget {
    window_start: Instant,
    used: u32,
}

#[derive(Debug, Default)]
struct CategoryState {
    /// key 為來源 IP；`None` 為以類別為 key
    entries: HashMap<Option<IpAddr>, Entry>,
    /// key 為已通過 mTLS 驗證的 agent_id
    agents: HashMap<String, Entry>,
    budget: Option<Budget>,
    /// 因總量上限而未記錄、尚未在日誌中附註的則數
    over_budget: u64,
}

impl CategoryState {
    /// 以 agent_id 為 key。agent 表已滿（且清掉過期的 key 後仍滿）時回傳 `None`，由呼叫端退回以類別為 key
    fn check_agent(
        &mut self,
        agent_id: String,
        now: Instant,
        interval: Duration,
    ) -> Option<Decision> {
        if !self.agents.contains_key(&agent_id) && self.agents.len() >= MAX_AGENT_KEYS_PER_CATEGORY
        {
            self.agents
                .retain(|_, e| now.saturating_duration_since(e.last_logged) < interval);
            if self.agents.len() >= MAX_AGENT_KEYS_PER_CATEGORY {
                return None;
            }
        }
        if let Some(entry) = self.agents.get_mut(&agent_id)
            && now.saturating_duration_since(entry.last_logged) < interval
        {
            if entry.logged_in_window >= AGENT_EVENTS_PER_WINDOW {
                // 同一 agent 的訊息本身已指出 agent_id，不需另外列出
                entry.suppress(None);
                return Some(Decision::Suppress);
            }
            entry.logged_in_window += 1;
            return Some(Decision::Log {
                suppressed: std::mem::take(&mut entry.suppressed),
                over_budget: std::mem::take(&mut self.over_budget),
                skipped: SkippedAgents::default(),
            });
        }
        let suppressed = self
            .agents
            .insert(agent_id, Entry::logged_at(now))
            .map_or(0, |e| e.suppressed);
        Some(Decision::Log {
            suppressed,
            over_budget: std::mem::take(&mut self.over_budget),
            skipped: SkippedAgents::default(),
        })
    }

    /// 以類別或類別 + 來源 IP 為 key；`agent` 為事件所屬的 agent_id（略過時記下）
    fn check_source(
        &mut self,
        mut source: Option<IpAddr>,
        agent: Option<&str>,
        now: Instant,
        interval: Duration,
    ) -> Decision {
        if source.is_some()
            && !self.entries.contains_key(&source)
            && self.entries.len() >= MAX_KEYS_PER_CATEGORY
        {
            // 先清掉時間窗已過的 key；仍然太多就退回以類別為 key（只影響這個類別）
            self.entries
                .retain(|_, e| now.saturating_duration_since(e.last_logged) < interval);
            if self.entries.len() >= MAX_KEYS_PER_CATEGORY {
                source = None;
            }
        }

        if let Some(entry) = self.entries.get_mut(&source)
            && now.saturating_duration_since(entry.last_logged) < interval
        {
            entry.suppress(agent);
            return Decision::Suppress;
        }

        // 依來源分開的 key：同一類別所有來源合計也有上限
        if source.is_some() {
            let budget = self.budget.get_or_insert(Budget {
                window_start: now,
                used: 0,
            });
            if now.saturating_duration_since(budget.window_start) >= interval {
                *budget = Budget {
                    window_start: now,
                    used: 0,
                };
            }
            if budget.used >= SOURCE_BUDGET_PER_WINDOW {
                self.over_budget += 1;
                // 也記在該來源的略過則數裡，等它下一次記錄時一併回報
                self.entries
                    .entry(source)
                    .or_insert_with(|| Entry {
                        last_logged: now.checked_sub(interval).unwrap_or(now),
                        suppressed: 0,
                        skipped: SkippedAgents::default(),
                        logged_in_window: 0,
                    })
                    .suppress(agent);
                return Decision::Suppress;
            }
            budget.used += 1;
        }

        let previous = self.entries.insert(source, Entry::logged_at(now));
        let (suppressed, skipped) =
            previous.map_or((0, SkippedAgents::default()), |e| (e.suppressed, e.skipped));
        Decision::Log {
            suppressed,
            over_budget: std::mem::take(&mut self.over_budget),
            skipped,
        }
    }
}

#[derive(Debug)]
pub struct LogThrottle {
    interval: Duration,
    state: Mutex<HashMap<Category, CategoryState>>,
    /// 測試用：記下所有實際輸出的節流日誌（含附註）
    #[cfg(test)]
    emitted: Mutex<Vec<String>>,
}

impl Default for LogThrottle {
    fn default() -> Self {
        Self::new(DEFAULT_INTERVAL)
    }
}

impl LogThrottle {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            state: Mutex::new(HashMap::new()),
            #[cfg(test)]
            emitted: Mutex::new(Vec::new()),
        }
    }

    /// 以注入的時間判斷是否記錄（單元測試用；正式程式用 [`LogThrottle::check`]）
    pub fn check_at(&self, key: impl Into<Key>, now: Instant) -> Decision {
        let Key {
            category,
            source,
            agent,
        } = key.into();
        // 持鎖期間只有簡單的欄位更新，被污染也沿用內部資料
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let cat = guard.entry(category).or_default();
        let interval = self.interval;
        match source {
            Some(Source::Agent(agent_id)) => {
                // agent 表已滿：退回以類別為 key，並把 agent_id 記在略過清單
                let fallback = agent_id.clone();
                match cat.check_agent(agent_id, now, interval) {
                    Some(decision) => decision,
                    None => cat.check_source(None, Some(&fallback), now, interval),
                }
            }
            Some(Source::Ip(ip)) => cat.check_source(Some(ip), agent.as_deref(), now, interval),
            None => cat.check_source(None, agent.as_deref(), now, interval),
        }
    }

    pub fn check(&self, key: impl Into<Key>) -> Decision {
        self.check_at(key, Instant::now())
    }

    /// 由 [`throttled!`] 在實際輸出一則日誌時呼叫；正式程式不做任何事，測試時記下內容
    #[doc(hidden)]
    pub fn emitted(&self, _line: &str) {
        #[cfg(test)]
        self.emitted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(_line.to_string());
    }

    /// 測試用：取出目前為止輸出的節流日誌
    #[cfg(test)]
    pub fn take_emitted(&self) -> Vec<String> {
        std::mem::take(&mut *self.emitted.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// 依節流結果以指定等級記錄；略過時不輸出。
/// key 可以是 [`Category`]、`(Category, IpAddr)` 或 [`Key`]（例如 [`Key::agent_event`]）。
#[macro_export]
macro_rules! throttled {
    ($throttle:expr, $key:expr, $level:ident, $($arg:tt)+) => {{
        let throttle = &$throttle;
        let decision = throttle.check($key);
        if let $crate::throttle::Decision::Log { .. } = decision {
            let line = format!("{}{}", format_args!($($arg)+), decision.suffix());
            throttle.emitted(&line);
            log::$level!("{}", line);
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n))
    }

    fn log(suppressed: u64) -> Decision {
        Decision::Log {
            suppressed,
            over_budget: 0,
            skipped: SkippedAgents::default(),
        }
    }

    fn entries(t: &LogThrottle, c: Category) -> usize {
        t.state
            .lock()
            .unwrap()
            .get(&c)
            .map_or(0, |s| s.entries.len())
    }

    fn agent_id(n: u32) -> String {
        format!("00000000-0000-4000-8000-{:012x}", n)
    }

    #[test]
    fn first_event_is_logged_then_suppressed_within_window() {
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        assert_eq!(t.check_at(Category::Handshake, t0), log(0));
        for ms in [1, 100, 5_000, 9_999] {
            assert_eq!(
                t.check_at(Category::Handshake, t0 + Duration::from_millis(ms)),
                Decision::Suppress
            );
        }
        // 時間窗結束後記錄，並回報略過的則數
        let d = t.check_at(Category::Handshake, t0 + Duration::from_secs(10));
        assert_eq!(d, log(4));
        assert_eq!(d.suffix(), "（上次記錄後另有 4 則略過）");
        // 計數歸零，時間窗從這次記錄重新起算
        assert_eq!(
            t.check_at(Category::Handshake, t0 + Duration::from_secs(15)),
            Decision::Suppress
        );
        assert_eq!(
            t.check_at(Category::Handshake, t0 + Duration::from_secs(20)),
            log(1)
        );
    }

    #[test]
    fn categories_are_independent() {
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        assert!(matches!(
            t.check_at(Category::Handshake, t0),
            Decision::Log { .. }
        ));
        assert!(matches!(
            t.check_at(Category::ConnectionError, t0),
            Decision::Log { .. }
        ));
        assert_eq!(t.check_at(Category::Handshake, t0), Decision::Suppress);
        assert!(matches!(
            t.check_at(Category::HelloTimeout, t0),
            Decision::Log { .. }
        ));
        // K-S7：一般握手失敗洗版時，撤銷與 CRL 狀態仍各自記錄
        assert!(matches!(
            t.check_at(Category::RevocationStatus, t0),
            Decision::Log { .. }
        ));
        assert!(matches!(
            t.check_at((Category::CertificateRevoked, ip(1)), t0),
            Decision::Log { .. }
        ));
    }

    #[test]
    fn flood_produces_one_line_per_window() {
        // 30 秒內每 10 ms 一次握手失敗（3000 次）→ 只記錄 3 行
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        let logged: Vec<u64> = (0..3000)
            .filter_map(|i| {
                match t.check_at(Category::Handshake, t0 + Duration::from_millis(i * 10)) {
                    Decision::Log { suppressed, .. } => Some(suppressed),
                    Decision::Suppress => None,
                }
            })
            .collect();
        assert_eq!(logged, [0, 999, 999]);
    }

    #[test]
    fn flooding_source_does_not_mask_other_sources() {
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        let attacker = (Category::AgentOnline, ip(1));
        assert!(matches!(t.check_at(attacker, t0), Decision::Log { .. }));
        for ms in 1..1000 {
            assert_eq!(
                t.check_at(attacker, t0 + Duration::from_millis(ms)),
                Decision::Suppress
            );
        }
        // 其他來源的同類訊息照常記錄
        assert_eq!(
            t.check_at(
                (Category::AgentOnline, ip(2)),
                t0 + Duration::from_millis(500)
            ),
            log(0)
        );
        // 攻擊來源下一次記錄時回報略過的則數
        assert_eq!(t.check_at(attacker, t0 + Duration::from_secs(10)), log(999));
    }

    #[test]
    fn ipv6_sources_in_the_same_64_share_a_key() {
        // R2-K11：攻擊者無法以同一個 /64 內的大量位址繞過依來源的節流
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::ffff:1".parse().unwrap();
        let other: IpAddr = "2001:db8:0:1::1".parse().unwrap();
        assert_eq!(t.check_at((Category::AuthFailed, a), t0), log(0));
        assert_eq!(
            t.check_at((Category::AuthFailed, b), t0),
            Decision::Suppress
        );
        assert_eq!(t.check_at((Category::AuthFailed, other), t0), log(0));
        // 未驗證的 agent 事件同樣以 /64 合併
        let k = |addr, n| Key::agent_event(Category::AgentOnline, addr, &agent_id(n), false);
        assert_eq!(t.check_at(k(a, 1), t0), log(0));
        assert_eq!(t.check_at(k(b, 2), t0), Decision::Suppress);
    }

    #[test]
    fn many_sources_share_a_category_budget_and_overflow_is_reported() {
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        let logged = (0..100)
            .filter(|i| {
                matches!(
                    t.check_at((Category::AgentOffline, ip(*i)), t0),
                    Decision::Log { .. }
                )
            })
            .count();
        assert_eq!(logged, SOURCE_BUDGET_PER_WINDOW as usize);
        // R2-K11：因總量上限未記錄的 80 則，在該類別下一次記錄時附註（不論來源）
        let d = t.check_at(
            (Category::AgentOffline, ip(500)),
            t0 + Duration::from_secs(10),
        );
        assert_eq!(
            d,
            Decision::Log {
                suppressed: 0,
                over_budget: 80,
                skipped: SkippedAgents::default(),
            }
        );
        assert!(
            d.suffix().contains("80 則因每 10 秒 20 則的總量上限未記錄"),
            "{}",
            d.suffix()
        );
        // 附註只出現一次；被預算擋下的來源在下一個時間窗記錄時回報自己略過的則數
        assert_eq!(
            t.check_at(
                (Category::AgentOffline, ip(99)),
                t0 + Duration::from_secs(10)
            ),
            log(1)
        );
        // 不同類別的預算互不影響
        assert!(matches!(
            t.check_at((Category::AgentOnline, ip(5)), t0),
            Decision::Log { .. }
        ));
    }

    #[test]
    fn key_table_is_bounded_per_category() {
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        for i in 0..(MAX_KEYS_PER_CATEGORY as u32 + 500) {
            t.check_at((Category::PerIpLimit, ip(i)), t0);
        }
        assert!(entries(&t, Category::PerIpLimit) <= MAX_KEYS_PER_CATEGORY + 1);
        // R2-K11：一個類別的 key 已滿，不影響其他類別依來源分開記錄
        assert_eq!(t.check_at((Category::AuthFailed, ip(1)), t0), log(0));
        assert_eq!(t.check_at((Category::AuthFailed, ip(2)), t0), log(0));
        // 時間窗過後再灌入大量新來源：過期的 key 被清掉，表的大小仍有上限
        let t1 = t0 + Duration::from_secs(11);
        for i in 0..(MAX_KEYS_PER_CATEGORY as u32 * 2) {
            t.check_at((Category::PerIpLimit, ip(1_000_000 + i)), t1);
        }
        assert!(entries(&t, Category::PerIpLimit) <= MAX_KEYS_PER_CATEGORY + 1);
    }

    #[test]
    fn no_suffix_without_suppression() {
        assert_eq!(log(0).suffix(), "");
        assert_eq!(Decision::Suppress.suffix(), "");
    }

    #[test]
    fn clock_going_backwards_does_not_panic() {
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now() + Duration::from_secs(100);
        t.check_at(Category::Handshake, t0);
        assert_eq!(
            t.check_at(Category::Handshake, t0 - Duration::from_secs(50)),
            Decision::Suppress
        );
        t.check_at((Category::AgentOnline, ip(1)), t0);
        assert_eq!(
            t.check_at((Category::AgentOnline, ip(1)), t0 - Duration::from_secs(50)),
            Decision::Suppress
        );
        let k = Key::agent_event(Category::AgentOnline, ip(1), &agent_id(1), true);
        for _ in 0..AGENT_EVENTS_PER_WINDOW {
            t.check_at(k.clone(), t0);
        }
        assert_eq!(
            t.check_at(k, t0 - Duration::from_secs(50)),
            Decision::Suppress
        );
    }

    #[test]
    fn authenticated_agents_behind_one_ip_are_logged_individually() {
        // F1：啟用 mTLS 時，同一來源 IP（NAT）的多個 agent 各自記錄
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        for n in 0..3 {
            let k = Key::agent_event(Category::AgentOnline, ip(1), &agent_id(n), true);
            assert_eq!(t.check_at(k, t0), log(0), "agent {n}");
        }
        // 同一 agent 在時間窗內反覆重連（例如互踢）：最多記錄 AGENT_EVENTS_PER_WINDOW 則，
        // 超過的略過，下一次記錄時回報略過的則數（不列 agent_id）
        let flapping = Key::agent_event(Category::AgentOnline, ip(1), &agent_id(0), true);
        for i in 1..AGENT_EVENTS_PER_WINDOW {
            let at = t0 + Duration::from_millis(100 * u64::from(i));
            assert_eq!(t.check_at(flapping.clone(), at), log(0), "#{i}");
        }
        assert_eq!(
            t.check_at(flapping.clone(), t0 + Duration::from_secs(2)),
            Decision::Suppress
        );
        assert_eq!(
            t.check_at(flapping.clone(), t0 + Duration::from_secs(3)),
            Decision::Suppress
        );
        let d = t.check_at(flapping, t0 + Duration::from_secs(10));
        assert_eq!(d, log(2));
        assert_eq!(d.suffix(), "（上次記錄後另有 2 則略過）");
        // 以 agent_id 為 key 不影響同類別依來源 IP 的節流
        assert_eq!(t.check_at((Category::AgentOnline, ip(1)), t0), log(0));
    }

    #[test]
    fn authenticated_agents_are_not_limited_by_the_category_budget() {
        // F1：Kairos 重啟後大量已驗證的 agent 同時重新上線，每個都有紀錄
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        let n = SOURCE_BUDGET_PER_WINDOW * 5;
        for i in 0..n {
            let k = Key::agent_event(Category::AgentOffline, ip(1), &agent_id(i), true);
            assert_eq!(t.check_at(k, t0), log(0), "agent {i}");
        }
        // 未驗證的事件仍受總量上限限制
        let logged = (0..n)
            .filter(|i| {
                let k = Key::agent_event(Category::AgentOnline, ip(*i), &agent_id(*i), false);
                matches!(t.check_at(k, t0), Decision::Log { .. })
            })
            .count();
        assert_eq!(logged, SOURCE_BUDGET_PER_WINDOW as usize);
    }

    #[test]
    fn unauthenticated_agents_are_throttled_by_ip_and_listed_when_skipped() {
        // F1：未啟用 mTLS 時以來源 IP 節流，但附註列出被略過的 agent_id
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        let k = |n| Key::agent_event(Category::AgentOnline, ip(1), &agent_id(n), false);
        assert_eq!(t.check_at(k(0), t0), log(0));
        assert_eq!(t.check_at(k(1), t0), Decision::Suppress);
        assert_eq!(t.check_at(k(2), t0), Decision::Suppress);
        // 同一個 agent_id 重複略過只列一次
        assert_eq!(t.check_at(k(1), t0), Decision::Suppress);
        let d = t.check_at(k(3), t0 + Duration::from_secs(10));
        assert_eq!(
            d.suffix(),
            format!(
                "（上次記錄後另有 3 則略過，涉及 agent：{}、{}）",
                agent_id(1),
                agent_id(2)
            )
        );
        // 下一次記錄時清單已清空
        let d = t.check_at(k(4), t0 + Duration::from_secs(20));
        assert_eq!(d, log(0));
    }

    #[test]
    fn skipped_agent_list_is_truncated() {
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        let k = |n| Key::agent_event(Category::AgentOffline, ip(1), &agent_id(n), false);
        t.check_at(k(0), t0);
        for n in 1..=8 {
            assert_eq!(t.check_at(k(n), t0), Decision::Suppress);
        }
        let d = t.check_at(k(100), t0 + Duration::from_secs(10));
        let listed: Vec<String> = (1..=5).map(agent_id).collect();
        assert_eq!(
            d.suffix(),
            format!(
                "（上次記錄後另有 8 則略過，涉及 agent：{} 等其餘 3 個）",
                listed.join("、")
            )
        );

        // 相異的 agent_id 超過記錄上限：記憶體有上限，附註改為「至少」
        let total = MAX_TRACKED_SKIPPED_AGENTS as u32 + 10;
        for n in 1..=total {
            t.check_at(k(1000 + n), t0 + Duration::from_secs(11));
        }
        let d = t.check_at(k(0), t0 + Duration::from_secs(20));
        let suffix = d.suffix();
        assert!(
            suffix.starts_with(&format!(
                "（上次記錄後另有 {} 則略過，涉及 agent：{}、",
                total,
                agent_id(1001)
            )),
            "{suffix}"
        );
        assert!(
            suffix.ends_with(&format!(
                " 等其餘至少 {} 個）",
                MAX_TRACKED_SKIPPED_AGENTS - SKIPPED_AGENTS_LISTED + 1
            )),
            "{suffix}"
        );
    }

    #[test]
    fn agents_suppressed_by_the_budget_are_listed_later() {
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        for i in 0..SOURCE_BUDGET_PER_WINDOW {
            t.check_at((Category::AgentOnline, ip(i)), t0);
        }
        // 預算已用完：這個來源的事件被略過，agent_id 記在該來源
        let k = |n| Key::agent_event(Category::AgentOnline, ip(500), &agent_id(n), false);
        assert_eq!(t.check_at(k(7), t0), Decision::Suppress);
        let d = t.check_at(k(8), t0 + Duration::from_secs(10));
        assert!(
            d.suffix()
                .contains(&format!("另有 1 則略過，涉及 agent：{}）", agent_id(7))),
            "{}",
            d.suffix()
        );
    }

    #[test]
    fn full_agent_table_falls_back_to_the_category_key() {
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        for i in 0..MAX_AGENT_KEYS_PER_CATEGORY as u32 {
            let k = Key::agent_event(Category::AgentOnline, ip(1), &agent_id(i), true);
            assert!(matches!(t.check_at(k, t0), Decision::Log { .. }));
        }
        // 表已滿且都在時間窗內：新的 agent 退回以類別為 key（第一則記錄、之後略過並記下 agent_id）
        let k = |n| Key::agent_event(Category::AgentOnline, ip(1), &agent_id(n), true);
        assert_eq!(t.check_at(k(1 << 20), t0), log(0));
        assert_eq!(t.check_at(k((1 << 20) + 1), t0), Decision::Suppress);
        // 時間窗過後清掉過期的 key，新的 agent 恢復各自記錄
        let t1 = t0 + Duration::from_secs(10);
        assert_eq!(t.check_at(k((1 << 20) + 2), t1), log(0));
        assert_eq!(t.check_at(k((1 << 20) + 3), t1), log(0));
    }
}
