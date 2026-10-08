//! 重連退避：指數加倍、上限、±20% 抖動。
//!
//! 不引入 rand：亂數來源為 `RandomState`（每次建立都有不同的隨機金鑰）雜湊目前時間。
//! 所有運算皆為飽和 / checked 運算，任何輸入（含極大值）都不會 panic。

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 抖動比例（±20%）。
const JITTER_RATIO: f64 = 0.2;

#[derive(Debug)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    /// 下一次的基準延遲（未加抖動），恆為 `min(initial × 2^n, max)`
    current: Duration,
    /// 連續失敗次數
    failures: u32,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        let max = max.max(initial);
        Self {
            initial,
            max,
            current: initial,
            failures: 0,
        }
    }

    /// 記錄一次失敗，回傳本次應等待的延遲（已加抖動）。
    pub fn next_delay(&mut self) -> Duration {
        self.next_delay_with(random_unit(), random_unit())
    }

    /// 以指定的 `[0, 1)` 亂數計算延遲（供測試使用）。
    ///
    /// `u1` 決定 ±20% 抖動；若抖動後超過上限，改用 `u2` 在 `[0.8 × max, max]` 間均勻取值，
    /// 避免全部截在 `max` 而讓多個 agent 同步重連。
    fn next_delay_with(&mut self, u1: f64, u2: f64) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let base = self.current.min(self.max);
        self.current = self.current.saturating_mul(2).min(self.max);

        let jittered = scale(
            base,
            1.0 - JITTER_RATIO + 2.0 * JITTER_RATIO * unit(u1),
            self.max,
        );
        if jittered <= self.max {
            jittered
        } else {
            scale(
                self.max,
                1.0 - JITTER_RATIO + JITTER_RATIO * unit(u2),
                self.max,
            )
        }
    }

    /// 重置退避（連線穩定維持一段時間後呼叫）。
    pub fn reset(&mut self) {
        self.current = self.initial;
        self.failures = 0;
    }

    /// 目前的連續失敗次數。
    pub fn failures(&self) -> u32 {
        self.failures
    }
}

/// 把亂數限制在 `[0, 1]`（NaN 視為 0）。
fn unit(u: f64) -> f64 {
    if u.is_nan() { 0.0 } else { u.clamp(0.0, 1.0) }
}

/// `d × factor`；結果無法以 `Duration` 表示時回傳 `overflow`（不 panic）。
fn scale(d: Duration, factor: f64, overflow: Duration) -> Duration {
    Duration::try_from_secs_f64(d.as_secs_f64() * factor).unwrap_or(overflow)
}

/// 產生 `[0, 1)` 的亂數（非密碼學用途，只用於抖動）。
fn random_unit() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let hash = RandomState::new().hash_one(nanos);
    // 取高 53 位元轉成 [0, 1) 的 f64
    (hash >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    fn approx_secs(d: Duration) -> u64 {
        d.as_secs_f64().round() as u64
    }

    #[test]
    fn doubles_until_cap() {
        let mut b = Backoff::new(S(2), S(60));
        // u1 = 0.5 → 係數 1.0，無抖動
        let delays: Vec<u64> = (0..8)
            .map(|_| approx_secs(b.next_delay_with(0.5, 0.5)))
            .collect();
        assert_eq!(delays, vec![2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(b.failures(), 8);
    }

    #[test]
    fn reset_restarts_from_initial() {
        let mut b = Backoff::new(S(2), S(60));
        for _ in 0..5 {
            b.next_delay_with(0.5, 0.5);
        }
        b.reset();
        assert_eq!(b.failures(), 0);
        assert_eq!(approx_secs(b.next_delay_with(0.5, 0.5)), 2);
    }

    #[test]
    fn jitter_bounds_below_cap() {
        let mut b = Backoff::new(S(10), S(60));
        assert_eq!(b.next_delay_with(0.0, 0.0), Duration::from_millis(8000));
        let mut b = Backoff::new(S(10), S(60));
        let hi = b.next_delay_with(0.999_999, 0.0);
        assert!(hi > Duration::from_millis(11_999) && hi < S(12), "{hi:?}");
    }

    #[test]
    fn over_cap_redistributes_uniformly() {
        // 達到上限後：抖動超過 max 時改在 [0.8 × max, max] 取值，不會全部截在 max
        let mut b = Backoff::new(S(60), S(60));
        assert_eq!(b.next_delay_with(0.999, 0.0), S(48));
        assert_eq!(b.next_delay_with(0.999, 0.5), S(54));
        let top = b.next_delay_with(0.999, 0.999_999);
        assert!(
            top <= S(60) && top > Duration::from_millis(59_990),
            "{top:?}"
        );
        // 抖動未超過上限時照常往下抖
        assert_eq!(b.next_delay_with(0.0, 0.9), S(48));
    }

    #[test]
    fn random_delays_within_range() {
        let mut b = Backoff::new(S(2), S(60));
        for i in 0..50u32 {
            let base = S(2).saturating_mul(1 << i.min(5)).min(S(60));
            let d = b.next_delay();
            let lo = base.mul_f64(0.8);
            let hi = base.mul_f64(1.2).min(S(60));
            assert!(d >= lo && d <= hi, "第 {i} 次：{d:?} 不在 [{lo:?}, {hi:?}]");
        }
    }

    #[test]
    fn distribution_at_cap_spans_range() {
        // 在上限處取樣 2000 次：全部落在 [48, 60]，且不會集中在 60
        let mut b = Backoff::new(S(60), S(60));
        let samples: Vec<Duration> = (0..2000).map(|_| b.next_delay()).collect();
        assert!(samples.iter().all(|d| *d >= S(48) && *d <= S(60)));
        let at_max = samples.iter().filter(|d| **d == S(60)).count();
        assert!(at_max < 20, "有 {at_max} 筆剛好等於上限");
        // 前後兩半區間都有樣本
        assert!(samples.iter().any(|d| *d < S(54)));
        assert!(samples.iter().any(|d| *d > S(54)));
    }

    #[test]
    fn extreme_values_never_panic() {
        for (initial, max) in [
            (Duration::MAX, Duration::MAX),
            (S(u64::MAX), S(u64::MAX)),
            (S(1), Duration::MAX),
            (Duration::ZERO, Duration::ZERO),
            (Duration::MAX, S(1)),
        ] {
            let mut b = Backoff::new(initial, max);
            for _ in 0..200 {
                let d = b.next_delay();
                assert!(d <= max.max(initial));
                b.next_delay_with(1.0, 1.0);
                b.next_delay_with(f64::NAN, f64::INFINITY);
            }
            assert_eq!(b.failures(), 600);
        }
        // 失敗計數飽和而不溢位
        let mut b = Backoff::new(S(1), S(2));
        b.failures = u32::MAX;
        b.next_delay();
        assert_eq!(b.failures(), u32::MAX);
    }

    #[test]
    fn random_unit_in_range_and_varies() {
        let samples: Vec<f64> = (0..100).map(|_| random_unit()).collect();
        assert!(samples.iter().all(|u| (0.0..1.0).contains(u)));
        let first = samples[0];
        assert!(samples.iter().any(|u| *u != first));
    }
}
