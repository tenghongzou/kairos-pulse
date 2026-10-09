//! 日誌節流：可能被掃描或攻擊大量觸發的日誌（握手失敗、連線錯誤、協定違規、上線／離線等），
//! 每個時間窗最多記錄一次，並在下一次記錄時附上期間略過的則數。
//!
//! 節流的 key 是「類別」或「類別 + 來源」：
//! - 只以類別為 key：同一類別全域共用一個時間窗（例如握手失敗、accept 失敗）。
//! - 以類別 + 來源為 key：攻擊來源只會壓掉自己的訊息，不會掩蓋其他來源（例如上線、離線、
//!   撤銷）。來源以 [`crate::per_ip::source_key`] 正規化（IPv6 以 /64 合併；R2-K11）。
//!   為了避免大量來源仍能洗版，同一類別在一個時間窗內最多記錄 [`SOURCE_BUDGET_PER_WINDOW`] 則；
//!   因總量上限而未記錄的則數會在該類別下一次記錄時附註，不會無聲消失。
//!   每個類別各自最多追蹤 [`MAX_KEYS_PER_CATEGORY`] 個來源，超過時該類別退回以類別為 key，
//!   不影響其他類別。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::per_ip::source_key;

/// 預設時間窗：同一個 key 每 10 秒最多記一次
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

/// 以「類別 + 來源」為 key 時，同一類別在一個時間窗內最多記錄的則數（所有來源合計）
pub const SOURCE_BUDGET_PER_WINDOW: u32 = 20;

/// 每個類別同時追蹤的來源數上限，避免以大量來源 IP 灌爆節流表本身
pub const MAX_KEYS_PER_CATEGORY: usize = 1024;

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

/// 節流的 key：類別，或類別 + 來源
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
            source: Some(source_key(ip)),
        }
    }
}

/// 是否應記錄這一則
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 記錄；`suppressed` 為同一 key 上次記錄後略過的則數，
    /// `over_budget` 為先前同一類別因總量上限而未記錄的則數（所有來源合計）
    Log { suppressed: u64, over_budget: u64 },
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
        } = *self
        {
            if suppressed > 0 {
                out.push_str(&format!("（上次記錄後另有 {} 則略過）", suppressed));
            }
            if over_budget > 0 {
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
struct CategoryState {
    /// key 為來源；`None` 為以類別為 key
    entries: HashMap<Option<IpAddr>, Entry>,
    budget: Option<Budget>,
    /// 因總量上限而未記錄、尚未在日誌中附註的則數
    over_budget: u64,
}

#[derive(Debug)]
pub struct LogThrottle {
    interval: Duration,
    state: Mutex<HashMap<Category, CategoryState>>,
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
        }
    }

    /// 以注入的時間判斷是否記錄（單元測試用；正式程式用 [`LogThrottle::check`]）
    pub fn check_at(&self, key: impl Into<Key>, now: Instant) -> Decision {
        let key = key.into();
        // 持鎖期間只有簡單的欄位更新，被污染也沿用內部資料
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let cat = guard.entry(key.category).or_default();
        let interval = self.interval;
        let mut source = key.source;

        if source.is_some()
            && !cat.entries.contains_key(&source)
            && cat.entries.len() >= MAX_KEYS_PER_CATEGORY
        {
            // 先清掉時間窗已過的 key；仍然太多就退回以類別為 key（只影響這個類別）
            cat.entries
                .retain(|_, e| now.saturating_duration_since(e.last_logged) < interval);
            if cat.entries.len() >= MAX_KEYS_PER_CATEGORY {
                source = None;
            }
        }

        let within_window = cat
            .entries
            .get(&source)
            .is_some_and(|e| now.saturating_duration_since(e.last_logged) < interval);
        if within_window {
            if let Some(entry) = cat.entries.get_mut(&source) {
                entry.suppressed += 1;
            }
            return Decision::Suppress;
        }

        // 依來源分開的 key：同一類別所有來源合計也有上限
        if source.is_some() {
            let budget = cat.budget.get_or_insert(Budget {
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
                cat.over_budget += 1;
                // 也記在該來源的略過則數裡，等它下一次記錄時一併回報
                cat.entries
                    .entry(source)
                    .and_modify(|e| e.suppressed += 1)
                    .or_insert(Entry {
                        last_logged: now.checked_sub(interval).unwrap_or(now),
                        suppressed: 1,
                    });
                return Decision::Suppress;
            }
            budget.used += 1;
        }

        let suppressed = cat.entries.get(&source).map_or(0, |e| e.suppressed);
        cat.entries.insert(
            source,
            Entry {
                last_logged: now,
                suppressed: 0,
            },
        );
        Decision::Log {
            suppressed,
            over_budget: std::mem::take(&mut cat.over_budget),
        }
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

    fn log(suppressed: u64) -> Decision {
        Decision::Log {
            suppressed,
            over_budget: 0,
        }
    }

    fn entries(t: &LogThrottle, c: Category) -> usize {
        t.state
            .lock()
            .unwrap()
            .get(&c)
            .map_or(0, |s| s.entries.len())
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
                over_budget: 80
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
    }
}
