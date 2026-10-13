//! 人看的輸出中的時間、持續時間與數字（`design-system/copywriting.md` §5、§6；寫法以
//! `pulse/cli-spec.md` §4 的範例為準）。

use std::time::Duration;

use chrono::{DateTime, FixedOffset, Local, Offset as _, Utc};

/// 顯示用的時區：正式執行時為本機時區（依各時間點的位移，跨夏令時間也正確），測試時固定
#[derive(Debug, Clone, Copy)]
pub enum Zone {
    Local,
    /// 固定位移（測試用）
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(FixedOffset),
}

impl Zone {
    pub fn offset_at(self, t: DateTime<Utc>) -> FixedOffset {
        match self {
            Zone::Local => t.with_timezone(&Local).offset().fix(),
            Zone::Fixed(o) => o,
        }
    }

    pub fn local(self, t: DateTime<Utc>) -> DateTime<FixedOffset> {
        t.with_timezone(&self.offset_at(t))
    }
}

/// `UTC+8`、`UTC-5`、`UTC+5:30`、`UTC+0`
pub fn tz_label(offset: FixedOffset) -> String {
    let secs = offset.local_minus_utc();
    let sign = if secs < 0 { '-' } else { '+' };
    let abs = secs.unsigned_abs();
    let (h, m) = (abs / 3600, abs % 3600 / 60);
    if m == 0 {
        format!("UTC{sign}{h}")
    } else {
        format!("UTC{sign}{h}:{m:02}")
    }
}

/// `2026-10-09 14:22:10 UTC+8`
pub fn date_time(t: DateTime<Utc>, zone: Zone) -> String {
    let l = zone.local(t);
    format!(
        "{} {}",
        l.format("%Y-%m-%d %H:%M:%S"),
        tz_label(*l.offset())
    )
}

/// `14:22:33 UTC+8`
pub fn time_tz(t: DateTime<Utc>, zone: Zone) -> String {
    let l = zone.local(t);
    format!("{} {}", l.format("%H:%M:%S"), tz_label(*l.offset()))
}

/// `14:01:30`（不帶時區；同一段文字已標示過時區時使用）
pub fn time_only(t: DateTime<Utc>, zone: Zone) -> String {
    zone.local(t).format("%H:%M:%S").to_string()
}

/// 24 小時內只寫時刻，更早寫日期（`cli-spec.md` §4.2「時間」）
pub fn moment(t: DateTime<Utc>, now: DateTime<Utc>, zone: Zone) -> String {
    if (now - t).abs() < chrono::Duration::hours(24) {
        time_tz(t, zone)
    } else {
        date_time(t, zone)
    }
}

/// 取樣時間的範圍：`2026-10-06 12:00～17:06`（同一天）或 `2026-10-06 12:00～10-07 03:10`
pub fn sample_range(from: DateTime<Utc>, until: DateTime<Utc>, zone: Zone) -> String {
    let (a, b) = (zone.local(from), zone.local(until));
    let second = if a.date_naive() == b.date_naive() {
        b.format("%H:%M").to_string()
    } else if a.format("%Y").to_string() == b.format("%Y").to_string() {
        b.format("%m-%d %H:%M").to_string()
    } else {
        b.format("%Y-%m-%d %H:%M").to_string()
    };
    format!("{}～{second}", a.format("%Y-%m-%d %H:%M"))
}

/// 持續時間（最多兩個單位，第二個單位為 0 時省略）：`45 秒`、`12 分 30 秒`、`41 分`、
/// `2 小時 13 分`、`2 小時`、`3 天 4 小時`
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    let (days, hours, mins, secs) = (s / 86_400, s / 3600 % 24, s / 60 % 60, s % 60);
    let two = |a: u64, ua: &str, b: u64, ub: &str| {
        if b == 0 {
            format!("{a} {ua}")
        } else {
            format!("{a} {ua} {b} {ub}")
        }
    };
    if s < 60 {
        format!("{s} 秒")
    } else if s < 3600 {
        two(mins, "分", secs, "秒")
    } else if s < 86_400 {
        two(s / 3600, "小時", mins, "分")
    } else {
        two(days, "天", hours, "小時")
    }
}

/// 相對時間（過去）：`3 秒前`、`12 分 34 秒前`、`5 分鐘前`、`2 小時 1 分前`、`2 天 21 小時前`
pub fn ago(d: Duration) -> String {
    format!("{}前", minutes_word(duration(d)))
}

/// 相對時間（未來）：`52 秒後`、`52 分鐘後`、`4 分鐘後`
pub fn later(d: Duration) -> String {
    format!("{}後", minutes_word(duration(d)))
}

/// 只有分鐘一個單位時寫「分鐘」（`copywriting.md` §5.3：「3 分鐘前」「5 分鐘後重試」）
fn minutes_word(s: String) -> String {
    match s.strip_suffix(" 分") {
        Some(n) if !n.contains(' ') => format!("{n} 分鐘"),
        _ => s,
    }
}

/// 預計補完時間：四捨五入到分鐘，`約 21 小時 54 分`、`約 19 分鐘`、`不到 1 分鐘`
pub fn eta(d: Duration) -> String {
    let mins = round_minutes(d).as_secs() / 60;
    if mins == 0 {
        "不到 1 分鐘".to_string()
    } else if mins < 60 {
        format!("約 {mins} 分鐘")
    } else if mins.is_multiple_of(60) {
        format!("約 {} 小時", mins / 60)
    } else {
        format!("約 {} 小時 {} 分", mins / 60, mins % 60)
    }
}

/// 四捨五入到分鐘（預計補完時間）
pub fn round_minutes(d: Duration) -> Duration {
    Duration::from_secs((d.as_secs() + 30) / 60 * 60)
}

/// `--brief` 的緊湊持續時間：`12m30s`、`21h54m`、`52m`、`78d`、`2h13m`
pub fn compact(d: Duration) -> String {
    let s = d.as_secs();
    let (days, hours, mins, secs) = (s / 86_400, s / 3600 % 24, s / 60 % 60, s % 60);
    let two = |a: u64, ua: &str, b: u64, ub: &str| {
        if b == 0 {
            format!("{a}{ua}")
        } else {
            format!("{a}{ua}{b}{ub}")
        }
    };
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        two(mins, "m", secs, "s")
    } else if s < 86_400 {
        two(s / 3600, "h", mins, "m")
    } else {
        two(days, "d", hours, "h")
    }
}

/// 千分位：`49,924`
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 百分比：小數 1 位，0 與 100 不帶小數（`0.3%`、`99.8%`、`100%`）
pub fn percent(ratio: f64) -> String {
    let p = (ratio * 1000.0).round() / 10.0;
    if p <= 0.0 {
        "0%".to_string()
    } else if p >= 100.0 {
        "100%".to_string()
    } else {
        format!("{p:.1}%")
    }
}

/// 容量上限：整數 MiB 寫 `64 MiB`，否則以 bytes 的十進位寫法
pub fn mib(bytes: u64) -> String {
    if bytes.is_multiple_of(1024 * 1024) {
        format!("{} MiB", bytes / (1024 * 1024))
    } else {
        format!("{} bytes", thousands(bytes))
    }
}

/// 保存時間：整天寫 `7 天`，否則 `36 小時`
pub fn hours(h: u64) -> String {
    if h > 0 && h.is_multiple_of(24) {
        format!("{} 天", h / 24)
    } else {
        format!("{h} 小時")
    }
}

/// 時鐘偏差的大小：30 秒以內 `0.2 秒`；以上用持續時間的寫法
pub fn offset_amount(ms: i64) -> String {
    let abs = ms.unsigned_abs();
    if abs < 60_000 {
        let tenths = (abs + 50) / 100;
        if tenths.is_multiple_of(10) {
            format!("{} 秒", tenths / 10)
        } else {
            format!("{}.{} 秒", tenths / 10, tenths % 10)
        }
    } else {
        duration(Duration::from_millis(abs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn taipei() -> Zone {
        Zone::Fixed(FixedOffset::east_opt(8 * 3600).unwrap())
    }

    #[test]
    fn durations_follow_the_spec_examples() {
        let d = |s: u64| duration(Duration::from_secs(s));
        assert_eq!(d(3), "3 秒");
        assert_eq!(d(750), "12 分 30 秒");
        assert_eq!(d(41 * 60), "41 分");
        assert_eq!(d(2 * 3600 + 13 * 60 + 5), "2 小時 13 分");
        assert_eq!(d(2 * 3600), "2 小時");
        assert_eq!(d(3 * 86_400 + 4 * 3600 + 59), "3 天 4 小時");
        assert_eq!(d(12 * 86_400 + 4 * 3600), "12 天 4 小時");
        assert_eq!(d(122), "2 分 2 秒");
        let a = |s: u64| ago(Duration::from_secs(s));
        assert_eq!(a(3), "3 秒前");
        assert_eq!(a(754), "12 分 34 秒前");
        assert_eq!(a(5 * 60), "5 分鐘前");
        assert_eq!(a(2 * 3600 + 60), "2 小時 1 分前");
        assert_eq!(a(2 * 86_400 + 21 * 3600), "2 天 21 小時前");
        let l = |s: u64| later(Duration::from_secs(s));
        assert_eq!(l(52), "52 秒後");
        assert_eq!(l(52 * 60), "52 分鐘後");
        assert_eq!(l(32), "32 秒後");
    }

    #[test]
    fn eta_and_compact() {
        // cli-spec §4.8 的 replay_eta_ms 78827000 對應 §4.5a 的「約 21 小時 54 分」
        assert_eq!(eta(Duration::from_millis(78_827_000)), "約 21 小時 54 分");
        assert_eq!(
            eta(Duration::from_secs(21 * 3600 + 54 * 60 + 10)),
            "約 21 小時 54 分"
        );
        assert_eq!(eta(Duration::from_secs(19 * 60)), "約 19 分鐘");
        assert_eq!(eta(Duration::from_secs(29)), "不到 1 分鐘");
        assert_eq!(compact(Duration::from_secs(750)), "12m30s");
        assert_eq!(compact(Duration::from_secs(21 * 3600 + 54 * 60)), "21h54m");
        assert_eq!(compact(Duration::from_secs(52 * 60)), "52m");
        assert_eq!(compact(Duration::from_secs(78 * 86_400)), "78d");
        assert_eq!(compact(Duration::from_secs(42)), "42s");
    }

    #[test]
    fn times_carry_the_zone_label() {
        let t = utc("2026-10-09T06:22:10Z");
        assert_eq!(date_time(t, taipei()), "2026-10-09 14:22:10 UTC+8");
        assert_eq!(time_tz(t, taipei()), "14:22:10 UTC+8");
        assert_eq!(time_only(t, taipei()), "14:22:10");
        let now = utc("2026-10-09T06:35:02Z");
        assert_eq!(
            moment(utc("2026-10-09T06:22:33Z"), now, taipei()),
            "14:22:33 UTC+8"
        );
        assert_eq!(
            moment(utc("2026-10-06T09:14:40Z"), now, taipei()),
            "2026-10-06 17:14:40 UTC+8"
        );
        assert_eq!(
            sample_range(
                utc("2026-10-06T04:00:00Z"),
                utc("2026-10-06T09:06:20Z"),
                taipei()
            ),
            "2026-10-06 12:00～17:06"
        );
        assert_eq!(
            sample_range(
                utc("2026-10-06T04:00:00Z"),
                utc("2026-10-07T09:06:20Z"),
                taipei()
            ),
            "2026-10-06 12:00～10-07 17:06"
        );
        assert_eq!(tz_label(FixedOffset::east_opt(0).unwrap()), "UTC+0");
        assert_eq!(tz_label(FixedOffset::west_opt(5 * 3600).unwrap()), "UTC-5");
        assert_eq!(tz_label(FixedOffset::east_opt(19_800).unwrap()), "UTC+5:30");
    }

    #[test]
    fn numbers() {
        assert_eq!(thousands(149), "149");
        assert_eq!(thousands(3676), "3,676");
        assert_eq!(thousands(49_924), "49,924");
        assert_eq!(thousands(1_000_000), "1,000,000");
        assert_eq!(percent(0.003), "0.3%");
        assert_eq!(percent(0.998), "99.8%");
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(1.0), "100%");
        assert_eq!(mib(67_108_864), "64 MiB");
        assert_eq!(hours(168), "7 天");
        assert_eq!(hours(36), "36 小時");
        assert_eq!(offset_amount(210), "0.2 秒");
        assert_eq!(offset_amount(-300), "0.3 秒");
        assert_eq!(offset_amount(45_000), "45 秒");
        assert_eq!(offset_amount(-462_000), "7 分 42 秒");
        assert_eq!(offset_amount(-(2 * 3600 + 13 * 60) * 1000), "2 小時 13 分");
    }
}
