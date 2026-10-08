//! 日誌節流：可能被掃描或攻擊大量觸發的日誌（握手失敗、連線錯誤、協定違規、上線／離線等），
//! 每個時間窗最多記錄一次，並在下一次記錄時附上期間略過的則數。
//!
//! 節流的 key 是「類別」或「類別 + 來源 IP」：
//! - 只以類別為 key：同一類別全域共用一個時間窗（例如握手失敗、accept 失敗）。
//! - 以類別 + 來源 IP 為 key：攻擊來源只會壓掉自己的訊息，不會掩蓋其他來源（例如上線、離線、
//!   撤銷）。為了避免大量來源仍能洗版，同一類別在一個時間窗內最多記錄
//!   [`SOURCE_BUDGET_PER_WINDOW`] 則；key 數量也有上限 [`MAX_KEYS`]，超過時退回以類別為 key。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 預設時間窗：同一個 key 每 10 秒最多記一次
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

/// 以「類別 + 來源」為 key 時，同一類別在一個時間窗內最多記錄的則數（所有來源合計）
pub const SOURCE_BUDGET_PER_WINDOW: u32 = 20;

/// 同時追蹤的 key 數量上限，避免以大量來源 IP 灌爆節流表本身
pub const MAX_KEYS: usize = 4096;

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
    /// 同一來源 IP 超過連線上限
    PerIpLimit,
    /// agent 上線（註冊、取代舊連線）
    AgentOnline,
    /// agent 離線（連線關閉、被取代）
    AgentOffline,
    /// 宣告的 heartbeat_interval_ms 超出範圍
    HeartbeatClamped,
    /// 內容無效的 report（invalid_report）
    InvalidReport,
    /// report 送得太快，被延後處理
    ReportRate,
    /// 狀態表已達上限（淘汰離線 agent 或拒絕新 agent）
    RegistryFull,
    /// report 寫入逾時且結果未知，關閉連線
    WriteUnknown,
}

/// 節流的 key：類別，或類別 + 來源 IP
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub category: Category,
    pub source: Option<IpAddr>,
}

impl From<Category> for Key {
    fn from(category: Category) -> Self {
        Self {
            category,
            source: None,
        }
    }
}

impl From<(Category, IpAddr)> for Key {
    fn from((category, ip): (Category, IpAddr)) -> Self {
        Self {
            category,
            source: Some(ip),
        }
    }
}

/// 是否應記錄這一則
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 記錄；`suppressed` 為上次記錄後略過的則數
    Log { suppressed: u64 },
    /// 略過
    Suppress,
}

impl Decision {
    /// 附加在日誌訊息後的說明（沒有略過時為空字串）
    pub fn suffix(&self) -> String {
        match self {
            Decision::Log { suppressed } if *suppressed > 0 => {
                format!("（上次記錄後另有 {} 則略過）", suppressed)
            }
            _ => String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    last_logged: Instant,
    suppressed: u64,
}

/// 以「類別 + 來源」為 key 時，同一類別在目前時間窗內已記錄的則數
#[derive(Debug, Clone, Copy)]
struct Budget {
    window_start: Instant,
    used: u32,
}

#[derive(Debug, Default)]
struct State {
    entries: HashMap<Key, Entry>,
    budgets: HashMap<Category, Budget>,
}

#[derive(Debug)]
pub struct LogThrottle {
    interval: Duration,
    state: Mutex<State>,
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
            state: Mutex::new(State::default()),
        }
    }

    /// 以注入的時間判斷是否記錄（單元測試用；正式程式用 [`LogThrottle::check`]）
    pub fn check_at(&self, key: impl Into<Key>, now: Instant) -> Decision {
        let mut key = key.into();
        // 持鎖期間只有簡單的欄位更新，被污染也沿用內部資料
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *guard;
        let interval = self.interval;

        if !state.entries.contains_key(&key) && state.entries.len() >= MAX_KEYS {
            // 先清掉時間窗已過的 key；仍然太多就退回以類別為 key
            state
                .entries
                .retain(|_, e| now.saturating_duration_since(e.last_logged) < interval);
            if state.entries.len() >= MAX_KEYS {
                key.source = None;
            }
        }

        let within_window = state
            .entries
            .get(&key)
            .is_some_and(|e| now.saturating_duration_since(e.last_logged) < interval);
        if within_window {
            if let Some(entry) = state.entries.get_mut(&key) {
                entry.suppressed += 1;
            }
            return Decision::Suppress;
        }

        // 依來源分開的 key：同一類別所有來源合計也有上限
        if key.source.is_some() {
            let budget = state.budgets.entry(key.category).or_insert(Budget {
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
                // 記在該來源的略過則數裡，等它下一次記錄時一併回報
                state
                    .entries
                    .entry(key)
                    .and_modify(|e| e.suppressed += 1)
                    .or_insert(Entry {
                        last_logged: now.checked_sub(interval).unwrap_or(now),
                        suppressed: 1,
                    });
                return Decision::Suppress;
            }
            budget.used += 1;
        }

        let suppressed = state.entries.get(&key).map_or(0, |e| e.suppressed);
        state.entries.insert(
            key,
            Entry {
                last_logged: now,
                suppressed: 0,
            },
        );
        Decision::Log { suppressed }
    }

    pub fn check(&self, key: impl Into<Key>) -> Decision {
        self.check_at(key, Instant::now())
    }
}

/// 依節流結果以指定等級記錄；略過時不輸出。
/// key 可以是 [`Category`] 或 `(Category, IpAddr)`。
#[macro_export]
macro_rules! throttled {
    ($throttle:expr, $key:expr, $level:ident, $($arg:tt)+) => {{
        let decision = $throttle.check($key);
        if let $crate::throttle::Decision::Log { .. } = decision {
            log::$level!("{}{}", format_args!($($arg)+), decision.suffix());
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

    #[test]
    fn first_event_is_logged_then_suppressed_within_window() {
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        assert_eq!(
            t.check_at(Category::Handshake, t0),
            Decision::Log { suppressed: 0 }
        );
        for ms in [1, 100, 5_000, 9_999] {
            assert_eq!(
                t.check_at(Category::Handshake, t0 + Duration::from_millis(ms)),
                Decision::Suppress
            );
        }
        // 時間窗結束後記錄，並回報略過的則數
        let d = t.check_at(Category::Handshake, t0 + Duration::from_secs(10));
        assert_eq!(d, Decision::Log { suppressed: 4 });
        assert_eq!(d.suffix(), "（上次記錄後另有 4 則略過）");
        // 計數歸零，時間窗從這次記錄重新起算
        assert_eq!(
            t.check_at(Category::Handshake, t0 + Duration::from_secs(15)),
            Decision::Suppress
        );
        assert_eq!(
            t.check_at(Category::Handshake, t0 + Duration::from_secs(20)),
            Decision::Log { suppressed: 1 }
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
                    Decision::Log { suppressed } => Some(suppressed),
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
            Decision::Log { suppressed: 0 }
        );
        // 攻擊來源下一次記錄時回報略過的則數
        assert_eq!(
            t.check_at(attacker, t0 + Duration::from_secs(10)),
            Decision::Log { suppressed: 999 }
        );
    }

    #[test]
    fn many_sources_share_a_category_budget() {
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
        // 被預算擋下的來源在下一個時間窗記錄時回報略過的則數
        assert_eq!(
            t.check_at(
                (Category::AgentOffline, ip(99)),
                t0 + Duration::from_secs(10)
            ),
            Decision::Log { suppressed: 1 }
        );
        // 不同類別的預算互不影響
        assert!(matches!(
            t.check_at((Category::AgentOnline, ip(5)), t0),
            Decision::Log { .. }
        ));
    }

    #[test]
    fn key_table_is_bounded() {
        let t = LogThrottle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        for i in 0..(MAX_KEYS as u32 + 500) {
            t.check_at((Category::PerIpLimit, ip(i)), t0);
        }
        let len = t.state.lock().unwrap().entries.len();
        assert!(len <= MAX_KEYS + 1, "{len}");
        // 時間窗過後再灌入大量新來源：過期的 key 被清掉，表的大小仍有上限
        let t1 = t0 + Duration::from_secs(11);
        for i in 0..(MAX_KEYS as u32 * 2) {
            t.check_at((Category::PerIpLimit, ip(1_000_000 + i)), t1);
        }
        let len = t.state.lock().unwrap().entries.len();
        assert!(len <= MAX_KEYS + 1, "{len}");
    }

    #[test]
    fn no_suffix_without_suppression() {
        assert_eq!(Decision::Log { suppressed: 0 }.suffix(), "");
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
    }
}
