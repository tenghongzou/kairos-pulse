//! 重送與輪詢的時間規則（ENROLL.md 第 6.3、6.4、7.3 節；設計包 `enrollment-ux.md` §3.4、§8、§9）。
//!
//! 全部是純函式，亂數由呼叫端傳入（`[0, 1)`），單元測試可以固定。
//!
//! | 情況 | 規則 |
//! |---|---|
//! | 429／503（新註冊、輪詢） | `Retry-After × U(1.0, 1.2) + U(0, 2 秒)`，**絕不早於**伺服器指定的時間 |
//! | 網路錯誤（新註冊） | 2 秒起倍增、上限 5 分鐘、±20% |
//! | 輪詢 | 第一次依 `poll_after_ms`，之後每次 ×1.5、上限 5 分鐘、±20%，且不早於 Kairos 最近一次給的 `poll_after_ms` |
//! | `clock_skew` | 每 5 分鐘 ±20%（偵測到本機時間被調整時立即） |
//! | 過期續簽 | 一般錯誤 2 → 60 分鐘 ±20%；429／503 依 `Retry-After` 且不少於目前退避值、不重設退避；`grace_exceeded` 每 6 小時 ±20% |
//! | 8443 正常輪替失敗 | 1 → 12 小時 ±20% |
//! | `pending_queue_full` | 依 `Retry-After`（伺服器給 3600 秒），服務每小時重試 |

use std::time::Duration;

/// 抖動比例
pub const JITTER: f64 = 0.2;
/// 新註冊的網路錯誤退避
pub const NET_BACKOFF_INITIAL: Duration = Duration::from_secs(2);
pub const NET_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// 輪詢間隔的上限（Kairos 給的 `poll_after_ms` 較大時以它為準）
pub const POLL_MAX: Duration = Duration::from_secs(300);
/// 輪詢間隔的倍數
pub const POLL_FACTOR: f64 = 1.5;
/// `clock_skew` 的重送間隔
pub const CLOCK_RETRY: Duration = Duration::from_secs(300);
/// 過期續簽一般錯誤的退避
pub const GRACE_BACKOFF_INITIAL: Duration = Duration::from_secs(120);
pub const GRACE_BACKOFF_MAX: Duration = Duration::from_secs(3600);
/// `grace_exceeded` 的重試間隔
pub const GRACE_EXCEEDED_RETRY: Duration = Duration::from_secs(6 * 3600);
/// 服務啟動或剛偵測到過期時，過期續簽前的隨機延遲上限
pub const GRACE_START_SPREAD: Duration = Duration::from_secs(120);
/// 8443 正常輪替失敗的退避
pub const RENEW_BACKOFF_INITIAL: Duration = Duration::from_secs(3600);
pub const RENEW_BACKOFF_MAX: Duration = Duration::from_secs(12 * 3600);
/// `hello_ack.renew_now` 之後的隨機延遲上限
pub const RENEW_NOW_SPREAD: Duration = Duration::from_secs(6 * 3600);

/// `[0, 1]`（NaN 視為 0）
fn unit(u: f64) -> f64 {
    if u.is_nan() { 0.0 } else { u.clamp(0.0, 1.0) }
}

fn scale(d: Duration, factor: f64) -> Duration {
    Duration::try_from_secs_f64(d.as_secs_f64() * factor).unwrap_or(Duration::MAX)
}

/// `d × (1 ± 20%)`
pub fn jitter(d: Duration, u: f64) -> Duration {
    scale(d, 1.0 - JITTER + 2.0 * JITTER * unit(u))
}

/// 429／503 的等待：`retry_after × U(1.0, 1.2) + U(0, 2 秒)`，一定不小於 `retry_after`
pub fn retry_after_wait(retry_after: Duration, u1: f64, u2: f64) -> Duration {
    let base = scale(retry_after, 1.0 + 0.2 * unit(u1));
    base.max(retry_after)
        .saturating_add(scale(Duration::from_secs(2), unit(u2)))
}

/// 指數退避（未加抖動的目前值、下一次的值）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exp {
    pub current: Duration,
    pub initial: Duration,
    pub max: Duration,
}

impl Exp {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            current: initial,
            initial,
            max,
        }
    }

    /// 從保存的目前值恢復（服務重新啟動後沿用）
    pub fn resume(initial: Duration, max: Duration, current_ms: Option<u64>) -> Self {
        let current = current_ms
            .map(Duration::from_millis)
            .unwrap_or(initial)
            .clamp(initial, max);
        Self {
            current,
            initial,
            max,
        }
    }

    /// 這次的等待（加抖動，不超過上限）並倍增
    pub fn next(&mut self, u: f64) -> Duration {
        let wait = jitter(self.current, u).min(self.max);
        self.current = self.current.saturating_mul(2).min(self.max);
        wait
    }

    pub fn reset(&mut self) {
        self.current = self.initial;
    }

    pub fn current_ms(&self) -> u64 {
        u64::try_from(self.current.as_millis()).unwrap_or(u64::MAX)
    }
}

/// 輪詢間隔（設計包 §9、ENROLL.md 第 6.4 節）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Poller {
    /// 下一次的基準間隔（未加抖動）
    pub interval: Duration,
    /// Kairos 最近一次給的 `poll_after_ms`（不得早於它）
    pub floor: Duration,
}

impl Poller {
    /// 收到第一個 `pending_approval`（含 `poll_after_ms`）時建立
    pub fn new(poll_after: Duration) -> Self {
        Self {
            interval: poll_after,
            floor: poll_after,
        }
    }

    /// 從 `enrollment.json` 恢復
    pub fn resume(interval_ms: Option<u64>, floor_ms: Option<u64>) -> Self {
        let floor = Duration::from_millis(floor_ms.unwrap_or(30_000));
        Self {
            interval: interval_ms.map(Duration::from_millis).unwrap_or(floor),
            floor,
        }
    }

    /// 收到輪詢回應（`pending_approval`）後的等待時間；`poll_after` 為這次回應的值
    pub fn next(&mut self, poll_after: Option<Duration>, u: f64) -> Duration {
        if let Some(p) = poll_after {
            self.floor = p;
        }
        let wait = jitter(self.interval, u).max(self.floor);
        self.interval = scale(self.interval, POLL_FACTOR)
            .min(POLL_MAX)
            .max(self.floor.min(POLL_MAX));
        wait
    }
}

/// 下一次過期續簽在 429／503 時的等待：依 `Retry-After`（加抖動）且不少於目前的退避值；
/// 不重設、也不推進退避（設計包 §8）
pub fn grace_rate_limited_wait(retry_after: Duration, backoff: &Exp, u1: f64, u2: f64) -> Duration {
    retry_after_wait(retry_after, u1, u2).max(backoff.current)
}

/// 本機時間是否被調整：牆鐘與單調時鐘的差距變動超過 1 秒（設計包 §5.12）
pub fn clock_adjusted(wall_delta_ms: i64, mono_delta_ms: i64) -> bool {
    (wall_delta_ms - mono_delta_ms).abs() > 1_000
}

/// 過期續簽的 `sent_at`：同一個值不重複使用，本機時間沒有前進時以「最近一次＋1 毫秒」
pub fn next_grace_sent_at(now_ms: i64, last_ms: Option<i64>) -> i64 {
    match last_ms {
        Some(last) if now_ms <= last => last + 1,
        _ => now_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn retry_after_never_earlier_than_server() {
        for u1 in [0.0, 0.5, 0.999, 1.0, f64::NAN] {
            for u2 in [0.0, 0.5, 1.0] {
                let w = retry_after_wait(S(42), u1, u2);
                assert!(w >= S(42), "{w:?}");
                assert!(w <= S(42).mul_f64(1.2) + S(2), "{w:?}");
            }
        }
        assert_eq!(retry_after_wait(S(42), 0.0, 0.0), S(42));
        assert_eq!(retry_after_wait(S(10), 1.0, 1.0), S(14));
    }

    #[test]
    fn network_backoff_doubles_to_five_minutes() {
        let mut b = Exp::new(NET_BACKOFF_INITIAL, NET_BACKOFF_MAX);
        let waits: Vec<u64> = (0..10).map(|_| b.next(0.5).as_secs()).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
        b.reset();
        assert_eq!(b.next(0.5), S(2));
        // 抖動 ±20%，但不超過上限
        let mut b = Exp::new(S(100), S(300));
        assert_eq!(b.next(0.0), S(80));
        assert_eq!(b.next(1.0), S(240));
        assert_eq!(b.next(1.0), S(300));
    }

    #[test]
    fn grace_backoff_two_to_sixty_minutes() {
        let mut b = Exp::new(GRACE_BACKOFF_INITIAL, GRACE_BACKOFF_MAX);
        let mins: Vec<u64> = (0..7).map(|_| b.next(0.5).as_secs() / 60).collect();
        assert_eq!(mins, [2, 4, 8, 16, 32, 60, 60]);
        // 429 不重設、不推進，且不少於目前的退避值
        let before = b;
        assert_eq!(grace_rate_limited_wait(S(10), &b, 0.0, 0.0), S(3600));
        assert_eq!(b, before);
        let fresh = Exp::new(GRACE_BACKOFF_INITIAL, GRACE_BACKOFF_MAX);
        assert_eq!(grace_rate_limited_wait(S(600), &fresh, 0.0, 0.0), S(600));
        // 從 enrollment.json 恢復
        let resumed = Exp::resume(GRACE_BACKOFF_INITIAL, GRACE_BACKOFF_MAX, Some(480_000));
        assert_eq!(resumed.current, S(480));
        let clamped = Exp::resume(GRACE_BACKOFF_INITIAL, GRACE_BACKOFF_MAX, Some(1));
        assert_eq!(clamped.current, GRACE_BACKOFF_INITIAL);
    }

    #[test]
    fn polling_grows_by_half_and_respects_the_server_floor() {
        let mut p = Poller::new(S(30));
        let waits: Vec<u64> = (0..7).map(|_| p.next(None, 0.5).as_secs()).collect();
        assert_eq!(waits, [30, 45, 67, 101, 151, 227, 300]);
        // 抖動不早於 Kairos 最近一次給的值
        let mut p = Poller::new(S(30));
        assert_eq!(p.next(Some(S(30)), 0.0), S(30));
        // 伺服器調大 poll_after（同一來源大量待核准）：一律遵守，即使超過 5 分鐘
        let mut p = Poller::new(S(30));
        assert_eq!(p.next(Some(S(400)), 0.0), S(400));
        assert_eq!(p.next(Some(S(900)), 1.0), S(900));
        // 恢復
        let p = Poller::resume(Some(67_500), Some(30_000));
        assert_eq!(p.interval, Duration::from_millis(67_500));
    }

    #[test]
    fn clock_retry_and_adjustment_detection() {
        assert_eq!(jitter(CLOCK_RETRY, 0.0), S(240));
        assert_eq!(jitter(CLOCK_RETRY, 1.0), S(360));
        assert!(!clock_adjusted(10_000, 10_500));
        assert!(clock_adjusted(10_000 + 462_000, 10_000));
        assert!(clock_adjusted(0, 2_000));
    }

    #[test]
    fn grace_sent_at_is_never_reused() {
        assert_eq!(next_grace_sent_at(1_000, None), 1_000);
        assert_eq!(next_grace_sent_at(1_000, Some(999)), 1_000);
        assert_eq!(next_grace_sent_at(1_000, Some(1_000)), 1_001);
        // 本機時間被往回調整
        assert_eq!(next_grace_sent_at(500, Some(1_000)), 1_001);
    }
}
