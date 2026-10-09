//! 日誌節流：同一類別在一段時間內最多記錄一次，被略過的次數在下次記錄時一併輸出。
//!
//! 用於「伺服器送了怪東西」這類可能被大量觸發的訊息，避免惡意或故障的伺服器
//! 在短時間內灌爆日誌檔。時間由呼叫端以參數傳入，方便單元測試。
//!
//! 時間使用 `tokio::time::Instant`：在 tokio 的暫停時鐘（`tokio::time::pause`）下也能測試
//! 長視窗（例如 10 分鐘）的節流；沒有 runtime 時行為與 `std::time::Instant` 相同。

use std::time::Duration;

use tokio::time::Instant;

/// 預設節流視窗。
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(10);

/// 單一類別的節流狀態。
#[derive(Debug, Clone)]
pub struct LogThrottle {
    window: Duration,
    /// 上次實際記錄的時間
    last_logged: Option<Instant>,
    /// 自上次記錄以來被略過的次數
    suppressed: u64,
}

impl LogThrottle {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            last_logged: None,
            suppressed: 0,
        }
    }

    /// 判斷此刻是否應記錄。
    ///
    /// 應記錄時回傳 `Some(n)`，`n` 為上次記錄之後被略過的次數（並歸零）；
    /// 不應記錄時回傳 `None` 並累加略過次數。
    pub fn check(&mut self, now: Instant) -> Option<u64> {
        let due = match self.last_logged {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.window,
        };
        if due {
            self.last_logged = Some(now);
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }
}

impl Default for LogThrottle {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW)
    }
}

/// 日誌訊息尾端的「另有 N 則被略過」說明；`n == 0` 時為空字串。
///
/// 被略過的次數是「自上次實際記錄以來」累計的，距上次記錄可能遠超過一個視窗，
/// 因此不寫成「N 秒內」。
pub fn suppressed_note(n: u64, window: Duration) -> String {
    if n == 0 {
        String::new()
    } else {
        format!(
            "（自上次記錄以來另有 {n} 則同類訊息被略過；同類訊息每 {} 秒最多記錄一則）",
            window.as_secs()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = Duration::from_secs(10);

    #[test]
    fn first_call_logs() {
        let mut t = LogThrottle::new(W);
        assert_eq!(t.check(Instant::now()), Some(0));
    }

    #[test]
    fn suppresses_within_window_and_reports_count() {
        let mut t = LogThrottle::new(W);
        let t0 = Instant::now();
        assert_eq!(t.check(t0), Some(0));
        for ms in 1..=500 {
            assert_eq!(t.check(t0 + Duration::from_millis(ms)), None);
        }
        // 視窗內最後一刻仍被略過
        assert_eq!(t.check(t0 + W - Duration::from_millis(1)), None);
        // 視窗到期：記錄並帶出略過次數，計數歸零
        assert_eq!(t.check(t0 + W), Some(501));
        assert_eq!(t.check(t0 + W + Duration::from_secs(1)), None);
        assert_eq!(t.check(t0 + W * 2), Some(1));
    }

    #[test]
    fn sparse_events_are_never_suppressed() {
        let mut t = LogThrottle::new(W);
        let t0 = Instant::now();
        for i in 0..5 {
            assert_eq!(t.check(t0 + W * i), Some(0));
        }
    }

    #[test]
    fn clock_going_backwards_does_not_panic() {
        let mut t = LogThrottle::new(W);
        let t0 = Instant::now() + Duration::from_secs(100);
        assert_eq!(t.check(t0), Some(0));
        assert_eq!(t.check(t0 - Duration::from_secs(50)), None);
    }

    #[test]
    fn note_text() {
        assert_eq!(suppressed_note(0, W), "");
        assert_eq!(
            suppressed_note(42, W),
            "（自上次記錄以來另有 42 則同類訊息被略過；同類訊息每 10 秒最多記錄一則）"
        );
    }
}
