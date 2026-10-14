//! 「寄送測試通知」（`POST /api/v1/channels/{id}/test`、`POST /api/v1/channels/test`）的限速：
//! 每個管道、每個使用者（執行者）各一個滑動視窗。測試通知會真的送到值班群組或信箱，連續按下
//! 會打擾收件者，也可能讓外部服務（LINE、Teams、SMTP）把 Kairos 判為濫用而封鎖。
//!
//! | 對象 | 上限 |
//! |---|---|
//! | 同一個管道（不分使用者） | [`PER_CHANNEL`]：60 秒內 3 次 |
//! | 同一個使用者（不分管道，含未儲存的管道） | [`PER_ACTOR`]：10 分鐘內 10 次 |
//!
//! 超過時回 429 `rate_limited`（〔暫定〕[`CODE_TEST_RATE_LIMITED`]）與 `Retry-After`，不送出、
//! 不寫發送紀錄。只在記憶體中計數（重新啟動後歸零）。

use std::collections::{HashMap, VecDeque};

/// 〔暫定〕測試通知太頻繁（子類別 51「通知管道」；正式號碼待設計包分配）
pub const CODE_TEST_RATE_LIMITED: &str = "KRS-W5111";
/// 同一個管道：(次數, 視窗毫秒)
pub const PER_CHANNEL: (usize, i64) = (3, 60_000);
/// 同一個使用者：(次數, 視窗毫秒)
pub const PER_ACTOR: (usize, i64) = (10, 600_000);
/// 記住的對象上限（超過時先清掉視窗外的紀錄；仍超過時不再新增對象，但照常限制已知的對象）
const MAX_KEYS: usize = 10_000;

/// 被限制的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limited {
    /// `channel` 或 `user`
    pub scope: &'static str,
    pub limit: usize,
    pub window_secs: i64,
    /// 幾秒後可以再試（至少 1）
    pub retry_after_secs: u64,
}

#[derive(Debug, Default)]
pub struct TestLimiter {
    by_channel: HashMap<String, VecDeque<i64>>,
    by_actor: HashMap<String, VecDeque<i64>>,
}

fn prune(q: &mut VecDeque<i64>, now: i64, window: i64) {
    while q.front().is_some_and(|t| now - t >= window) {
        q.pop_front();
    }
}

fn blocked(
    map: &mut HashMap<String, VecDeque<i64>>,
    key: &str,
    (limit, window): (usize, i64),
    now: i64,
    scope: &'static str,
) -> Option<Limited> {
    let q = map.get_mut(key)?;
    prune(q, now, window);
    if q.len() < limit {
        return None;
    }
    let oldest = *q.front()?;
    let wait_ms = (oldest + window - now).max(1);
    Some(Limited {
        scope,
        limit,
        window_secs: window / 1000,
        retry_after_secs: (wait_ms as u64).div_ceil(1000).max(1),
    })
}

fn record(map: &mut HashMap<String, VecDeque<i64>>, key: &str, window: i64, now: i64) {
    if !map.contains_key(key) && map.len() >= MAX_KEYS {
        map.retain(|_, q| {
            prune(q, now, window);
            !q.is_empty()
        });
        if map.len() >= MAX_KEYS {
            return;
        }
    }
    map.entry(key.to_string()).or_default().push_back(now);
}

impl TestLimiter {
    /// 檢查並記錄一次測試；`channel` 為 `None` 時（未儲存的管道）只看使用者的上限
    pub fn check(&mut self, channel: Option<&str>, actor: &str, now: i64) -> Result<(), Limited> {
        if let Some(c) = channel
            && let Some(l) = blocked(&mut self.by_channel, c, PER_CHANNEL, now, "channel")
        {
            return Err(l);
        }
        if let Some(l) = blocked(&mut self.by_actor, actor, PER_ACTOR, now, "user") {
            return Err(l);
        }
        if let Some(c) = channel {
            record(&mut self.by_channel, c, PER_CHANNEL.1, now);
        }
        record(&mut self.by_actor, actor, PER_ACTOR.1, now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_channel_allows_three_tests_per_minute() {
        let mut l = TestLimiter::default();
        for i in 0..3 {
            assert!(l.check(Some("c-1"), "user:alice", i * 1000).is_ok());
        }
        let e = l.check(Some("c-1"), "user:bob", 5_000).unwrap_err();
        assert_eq!(e.scope, "channel");
        // 最舊的一次在 0 ms：60 秒後才空出名額
        assert_eq!(e.retry_after_secs, 55);
        // 其他管道不受影響
        assert!(l.check(Some("c-2"), "user:bob", 5_000).is_ok());
        // 視窗過了就恢復
        assert!(l.check(Some("c-1"), "user:bob", 60_000).is_ok());
    }

    #[test]
    fn each_user_allows_ten_tests_per_ten_minutes_across_channels() {
        let mut l = TestLimiter::default();
        for i in 0..10 {
            let ch = format!("c-{i}");
            assert!(l.check(Some(&ch), "user:alice", i * 1000).is_ok());
        }
        // 未儲存的管道也算
        let e = l.check(None, "user:alice", 20_000).unwrap_err();
        assert_eq!((e.scope, e.limit, e.window_secs), ("user", 10, 600));
        assert_eq!(e.retry_after_secs, 580);
        // 被拒的那次不計入：另一位使用者照常
        assert!(l.check(None, "user:bob", 20_000).is_ok());
        assert!(l.check(Some("c-99"), "user:alice", 600_000).is_ok());
    }

    #[test]
    fn rejected_attempts_do_not_extend_the_window() {
        let mut l = TestLimiter::default();
        for _ in 0..3 {
            l.check(Some("c"), "a", 0).unwrap();
        }
        for t in [1_000, 2_000, 30_000] {
            assert!(l.check(Some("c"), "a", t).is_err());
        }
        assert!(l.check(Some("c"), "a", 60_000).is_ok());
    }
}
