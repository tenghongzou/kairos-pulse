//! 由 report 算出的衍生序列（`crates/kairos-proto/METRICS.md` 第 9.1、9.2 節）：
//!
//! - **速率**（`series.derived = 1`）：標準計數器（`MetricKind::Counter`）與同一台主機、同一序列的上一筆
//!   以 [`counter_rate`] 相減，每秒速率；`unit` 為 `<原單位>/s`。重置、`system.uptime` 變小、時間沒有前進
//!   時沒有速率點（缺口，不補 0）。晚到的資料（`ts` 不晚於上一筆）不產生速率點、也不更新「上一筆」。
//! - **推算的使用率**（`series.derived = 2`）：同一筆 report 中某個掛載點有 `system.filesystem.total` 與
//!   `available`、但沒有 `system.filesystem.usage` 時（0.2.0～0.7.0 的 Pulse），以
//!   `(total − available) ÷ total × 100` 推算（保留給 root 的空間算成已使用，比 `df` 略高）。
//!
//! v8 起計數器的原始累計值只存在原始資料，不再彙總；彙總只有速率序列。這裡只做純計算，寫入與「上一筆」的
//! 保存由呼叫端（SQLite 的寫入、重新啟動後的補回、主控台 API 的原始資料查詢）負責。

use kairos_proto::{
    CounterSample, METRIC_FILESYSTEM_AVAILABLE, METRIC_FILESYSTEM_TOTAL, METRIC_FILESYSTEM_USAGE,
    METRIC_UPTIME, MetricKind, counter_rate, standard_metric,
};
use serde_json::Number;
use std::collections::HashMap;

/// `series.derived`：原始序列
pub const DERIVED_NONE: i64 = 0;
/// `series.derived`：由計數器算出的每秒速率
pub const DERIVED_RATE: i64 = 1;
/// `series.derived`：推算值（舊版 Pulse 的磁碟使用率）
pub const DERIVED_ESTIMATED: i64 = 2;

/// 衍生序列在 API 中的名稱（`derived` 欄位）
pub fn derived_name(derived: i64) -> Option<&'static str> {
    match derived {
        DERIVED_RATE => Some("rate"),
        DERIVED_ESTIMATED => Some("estimated"),
        _ => None,
    }
}

/// 速率序列的單位
pub fn rate_unit(unit: &str) -> String {
    format!("{unit}/s")
}

/// 序列在衍生計算中的角色
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 標準計數器（名稱與單位都相符）
    Counter,
    /// `system.uptime`
    Uptime,
    FsTotal,
    FsAvailable,
    FsUsage,
    Other,
}

/// 依名稱、單位判斷角色；衍生序列一律 [`Role::Other`]
pub fn role_of(name: &str, unit: &str, derived: i64) -> Role {
    if derived != DERIVED_NONE {
        return Role::Other;
    }
    let Some(std) = standard_metric(name).filter(|s| s.unit == unit) else {
        return Role::Other;
    };
    if std.kind == MetricKind::Counter {
        return Role::Counter;
    }
    match name {
        METRIC_UPTIME => Role::Uptime,
        METRIC_FILESYSTEM_TOTAL => Role::FsTotal,
        METRIC_FILESYSTEM_AVAILABLE => Role::FsAvailable,
        METRIC_FILESYSTEM_USAGE => Role::FsUsage,
        _ => Role::Other,
    }
}

/// report 中的一個指標（已取得序列編號）
#[derive(Debug, Clone, Copy)]
pub struct Item<'a> {
    pub id: i64,
    pub name: &'a str,
    pub unit: &'a str,
    /// 正規化後的 label（`canonical_labels`）
    pub labels: &'a str,
    pub role: Role,
    pub value: &'a Number,
}

/// 一個衍生的點
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    /// 衍生序列的名稱（與來源相同）
    pub name: String,
    /// 衍生序列的單位
    pub unit: String,
    pub labels: String,
    pub derived: i64,
    pub value: f64,
}

/// 推算的使用率：`(total − available) ÷ total × 100`，四捨五入到小數 2 位、夾在 0～100；total 為 0 時 `None`
pub fn estimated_usage(total: f64, available: f64) -> Option<f64> {
    if total.is_nan() || total <= 0.0 || !available.is_finite() {
        return None;
    }
    let v = ((total - available) / total * 100.0).clamp(0.0, 100.0);
    Some((v * 100.0).round() / 100.0)
}

/// 一台主機各計數器序列的上一筆（`series_id → CounterSample`）
pub type CounterState = HashMap<i64, CounterSample>;

/// 一筆 report（取樣時間 `ts_ms`）的衍生點；同時更新 `state`（晚到的資料不更新）。
/// 回傳的點依輸入順序：先是各計數器的速率，再是推算的使用率
pub fn derive(items: &[Item<'_>], ts_ms: i64, state: &mut CounterState) -> Vec<Point> {
    let uptime = items
        .iter()
        .find(|i| i.role == Role::Uptime && i.labels.is_empty())
        .and_then(|i| i.value.as_u64());
    let mut out = Vec::new();
    for i in items.iter().filter(|i| i.role == Role::Counter) {
        let Some(value) = i.value.as_u64() else {
            continue;
        };
        let cur = CounterSample {
            value,
            ts_ms,
            uptime_secs: uptime,
        };
        match state.get(&i.id) {
            // 晚到（或同一時間）：不產生速率、不更新上一筆
            Some(prev) if cur.ts_ms <= prev.ts_ms => continue,
            Some(prev) => {
                if let Some(rate) = counter_rate(*prev, cur) {
                    out.push(Point {
                        name: i.name.to_string(),
                        unit: rate_unit(i.unit),
                        labels: i.labels.to_string(),
                        derived: DERIVED_RATE,
                        value: rate,
                    });
                }
            }
            None => {}
        }
        state.insert(i.id, cur);
    }
    // 推算的使用率：每組 label（掛載點）有 total 與 available、沒有 usage 時
    let mut fs: Vec<(&str, Option<f64>, Option<f64>, bool)> = Vec::new();
    for i in items {
        let slot = match i.role {
            Role::FsTotal | Role::FsAvailable | Role::FsUsage => {
                match fs.iter().position(|f| f.0 == i.labels) {
                    Some(p) => p,
                    None => {
                        fs.push((i.labels, None, None, false));
                        fs.len() - 1
                    }
                }
            }
            _ => continue,
        };
        let v = i.value.as_f64();
        match i.role {
            Role::FsTotal => fs[slot].1 = v,
            Role::FsAvailable => fs[slot].2 = v,
            _ => fs[slot].3 = true,
        }
    }
    for (labels, total, avail, has_usage) in fs {
        if has_usage {
            continue;
        }
        if let (Some(t), Some(a)) = (total, avail)
            && let Some(v) = estimated_usage(t, a)
        {
            out.push(Point {
                name: METRIC_FILESYSTEM_USAGE.to_string(),
                unit: "%".to_string(),
                labels: labels.to_string(),
                derived: DERIVED_ESTIMATED,
                value: v,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: u64) -> Number {
        Number::from(v)
    }

    struct R {
        names: Vec<(i64, &'static str, &'static str, &'static str, Number)>,
    }

    impl R {
        fn items(&self) -> Vec<Item<'_>> {
            self.names
                .iter()
                .map(|(id, name, unit, labels, v)| Item {
                    id: *id,
                    name,
                    unit,
                    labels,
                    role: role_of(name, unit, 0),
                    value: v,
                })
                .collect()
        }
    }

    fn report(rx: u64, uptime: u64) -> R {
        R {
            names: vec![
                (1, "system.uptime", "s", "", n(uptime)),
                (
                    2,
                    "system.network.received",
                    "By",
                    "{\"device\":\"eth0\"}",
                    n(rx),
                ),
            ],
        }
    }

    #[test]
    fn roles_follow_the_standard_table() {
        assert_eq!(role_of("system.network.received", "By", 0), Role::Counter);
        assert_eq!(role_of("system.disk.io_time", "ms", 0), Role::Counter);
        // 單位不符、衍生序列、自訂指標都不是計數器
        assert_eq!(role_of("system.network.received", "KB", 0), Role::Other);
        assert_eq!(role_of("system.network.received", "By/s", 1), Role::Other);
        assert_eq!(role_of("app.requests", "1", 0), Role::Other);
        assert_eq!(role_of("system.uptime", "s", 0), Role::Uptime);
        assert_eq!(role_of("system.filesystem.usage", "%", 0), Role::FsUsage);
        assert_eq!(role_of("system.cpu.usage", "%", 0), Role::Other);
    }

    #[test]
    fn rates_reset_reboot_and_late_samples() {
        let mut state = CounterState::new();
        // 第一筆：沒有上一筆，沒有速率
        assert!(derive(&report(1000, 100).items(), 10_000, &mut state).is_empty());
        // 正常：5 秒 5000 bytes → 1000 B/s
        let p = derive(&report(6000, 105).items(), 15_000, &mut state);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].unit, "By/s");
        assert_eq!(p[0].derived, DERIVED_RATE);
        assert_eq!(p[0].value, 1000.0);
        // 晚到：不產生、不更新
        assert!(derive(&report(5000, 104).items(), 14_000, &mut state).is_empty());
        assert_eq!(state[&2].value, 6000);
        // 重置（值變小）：沒有速率，但更新上一筆
        assert!(derive(&report(10, 110).items(), 20_000, &mut state).is_empty());
        assert_eq!(state[&2].value, 10);
        // 重新開機（uptime 變小，即使值變大）：沒有速率
        assert!(derive(&report(9_000_000, 5).items(), 25_000, &mut state).is_empty());
        let p = derive(&report(9_000_500, 10).items(), 30_000, &mut state);
        assert_eq!(p[0].value, 100.0);
    }

    #[test]
    fn usage_is_estimated_only_when_missing() {
        let fs = |labels: &'static str| {
            vec![
                (10, "system.filesystem.total", "By", labels, n(1000)),
                (11, "system.filesystem.available", "By", labels, n(250)),
            ]
        };
        let mut r = R {
            names: fs("{\"mountpoint\":\"/\"}"),
        };
        let p = derive(&r.items(), 1, &mut CounterState::new());
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].name, "system.filesystem.usage");
        assert_eq!(p[0].derived, DERIVED_ESTIMATED);
        assert_eq!(p[0].value, 75.0);
        // 有 usage（0.8.0 的 Pulse）：不推算
        r.names.push((
            12,
            "system.filesystem.usage",
            "%",
            "{\"mountpoint\":\"/\"}",
            Number::from_f64(70.0).unwrap(),
        ));
        assert!(derive(&r.items(), 1, &mut CounterState::new()).is_empty());
        // 另一個掛載點沒有 usage：只推算那一個
        r.names.extend(fs("{\"mountpoint\":\"/data\"}"));
        let p = derive(&r.items(), 1, &mut CounterState::new());
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].labels, "{\"mountpoint\":\"/data\"}");
        assert_eq!(estimated_usage(0.0, 0.0), None);
        assert_eq!(estimated_usage(100.0, 150.0), Some(0.0));
        assert_eq!(estimated_usage(3.0, 1.0), Some(66.67));
    }
}
