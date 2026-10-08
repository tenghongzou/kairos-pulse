//! 日誌節流：可能被掃描或攻擊大量觸發的日誌（握手失敗、連線錯誤、協定違規等），
//! 依類別每個時間窗最多記錄一次，並在下一次記錄時附上期間略過的則數。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 預設時間窗：同一類別每 10 秒最多記一次
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

/// 會被節流的日誌類別
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    /// TLS 握手失敗（無客戶端憑證、未知 CA、憑證已撤銷或過期、逾時、非 TLS 流量等）
    Handshake,
    /// 握手後在 hello 前違反協定（hello_required、unsupported_version、too_long、連續 bad_request）
    ProtocolViolation,
    /// 握手後未在時限內送出 hello
    HelloTimeout,
    /// 其他連線層級錯誤
    ConnectionError,
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

#[derive(Debug)]
pub struct LogThrottle {
    interval: Duration,
    entries: Mutex<HashMap<Category, Entry>>,
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
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// 以注入的時間判斷是否記錄（單元測試用；正式程式用 [`LogThrottle::check`]）
    pub fn check_at(&self, category: Category, now: Instant) -> Decision {
        // 持鎖期間只有簡單的欄位更新，被污染也沿用內部資料
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match entries.get_mut(&category) {
            Some(entry) if now.saturating_duration_since(entry.last_logged) < self.interval => {
                entry.suppressed += 1;
                Decision::Suppress
            }
            Some(entry) => {
                let suppressed = entry.suppressed;
                *entry = Entry {
                    last_logged: now,
                    suppressed: 0,
                };
                Decision::Log { suppressed }
            }
            None => {
                entries.insert(
                    category,
                    Entry {
                        last_logged: now,
                        suppressed: 0,
                    },
                );
                Decision::Log { suppressed: 0 }
            }
        }
    }

    pub fn check(&self, category: Category) -> Decision {
        self.check_at(category, Instant::now())
    }
}

/// 依節流結果以指定等級記錄；略過時不輸出
#[macro_export]
macro_rules! throttled {
    ($throttle:expr, $category:expr, $level:ident, $($arg:tt)+) => {{
        let decision = $throttle.check($category);
        if let $crate::throttle::Decision::Log { .. } = decision {
            log::$level!("{}{}", format_args!($($arg)+), decision.suffix());
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
