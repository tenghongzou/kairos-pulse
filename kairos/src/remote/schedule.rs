//! 推送的排程與升級時段（REMOTE.md 第 4.7.1、4.7.5 節；0.10）。
//!
//! `schedule` = `{starts_at?, ends_at?, windows[], timezone}`：
//!
//! - `starts_at`：在這之前推送為 `scheduled`，不提議任何主機。
//! - `ends_at`：到時推送結束（尚未開始替換的主機標為 `expired`／`rollout_ended`）。
//! - `windows[]`：每週的時段（`days`：`mon`～`sun`，省略或空陣列＝每天；`start`、`end`：`HH:MM`，
//!   `end` 小於 `start` 時跨午夜，時段屬於開始的那一天）。只在時段內提議主機，提議的 `apply_window`
//!   到時段結束為止。沒有時段＝隨時。
//! - `timezone`：時段的時區，**固定位移**（`+08:00`，與 `alerts.timezone` 相同的寫法；另接受 `UTC`、`Z`）。
//!
//! 這裡只有純函式（以毫秒時間計算），方便以假的時鐘測試。

use serde_json::{Value, json};

/// 時段的數量上限
pub const MAX_WINDOWS: usize = 16;
/// 一天的分鐘數
const DAY_MIN: i64 = 1440;
const DAY_MS: i64 = 86_400_000;
/// `days` 的名稱（索引 0＝週一）
pub const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// 每週的一個時段
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSpec {
    /// 開始的日子（位元 0＝週一 … 位元 6＝週日）；`0x7f`＝每天
    pub days: u8,
    /// 開始（當地時間，從午夜起的分鐘）
    pub start_min: u16,
    /// 結束（小於 `start_min` 時跨午夜）
    pub end_min: u16,
}

impl WindowSpec {
    fn duration_ms(&self) -> i64 {
        let d = (i64::from(self.end_min) - i64::from(self.start_min)).rem_euclid(DAY_MIN);
        d * 60_000
    }

    fn json(&self) -> Value {
        let days: Vec<&str> = (0..7)
            .filter(|i| self.days & (1 << i) != 0)
            .map(|i| DAY_NAMES[i])
            .collect();
        json!({"days": days, "start": hhmm(self.start_min), "end": hhmm(self.end_min)})
    }
}

fn hhmm(m: u16) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

fn parse_hhmm(v: Option<&Value>) -> Option<u16> {
    let s = v?.as_str()?;
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// 固定位移的時區（`+08:00`、`-05:30`、`UTC`、`Z`）→ 秒
pub fn parse_offset_secs(s: &str) -> Option<i32> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("utc") || s.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 14 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

/// 推送的排程
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScheduleSpec {
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub windows: Vec<WindowSpec>,
    /// 固定位移（文字）；`None`＝建立時以 Kairos 的 `alerts.timezone` 填入
    pub timezone: Option<String>,
}

/// 目前是否在時段內
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    /// 可以提議；`until` 為這個時段（含相連的時段）結束的時間，沒有時段時 `None`
    Open { until: Option<i64> },
    /// 時段外；`next` 為下一個時段開始的時間
    Closed { next: Option<i64> },
}

impl WindowState {
    pub fn is_open(&self) -> bool {
        matches!(self, WindowState::Open { .. })
    }
}

fn parse_ts(v: Option<&Value>, field: &str) -> Result<Option<i64>, (String, String)> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|t| Some(t.timestamp_millis()))
            .map_err(|_| {
                (
                    format!("schedule.{field}"),
                    "必須是 RFC 3339 的時間，例如 2026-12-01T01:00:00+08:00".to_string(),
                )
            }),
        Some(_) => Err((
            format!("schedule.{field}"),
            "必須是 RFC 3339 的時間字串".to_string(),
        )),
    }
}

impl ScheduleSpec {
    /// 沒有任何排程（立即開始、隨時可以提議）
    pub fn is_empty(&self) -> bool {
        self.starts_at.is_none() && self.ends_at.is_none() && self.windows.is_empty()
    }

    /// 解析 API 本文的 `schedule`（`null` 或省略＝沒有排程）
    pub fn parse(v: Option<&Value>) -> Result<ScheduleSpec, (String, String)> {
        let m = match v {
            None | Some(Value::Null) => return Ok(ScheduleSpec::default()),
            Some(Value::Object(m)) => m,
            Some(_) => return Err(("schedule".into(), "schedule 必須是物件".into())),
        };
        for k in m.keys() {
            if !matches!(k.as_str(), "starts_at" | "ends_at" | "windows" | "timezone") {
                return Err((format!("schedule.{k}"), format!("不認得的欄位 {k}")));
            }
        }
        let starts_at = parse_ts(m.get("starts_at"), "starts_at")?;
        let ends_at = parse_ts(m.get("ends_at"), "ends_at")?;
        if let (Some(s), Some(e)) = (starts_at, ends_at)
            && e <= s
        {
            return Err((
                "schedule.ends_at".into(),
                "ends_at 必須晚於 starts_at".into(),
            ));
        }
        let timezone = match m.get("timezone") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if parse_offset_secs(s).is_some() => Some(s.trim().to_string()),
            Some(_) => {
                return Err((
                    "schedule.timezone".into(),
                    "timezone 必須是固定位移，例如 +08:00（不支援時區名稱）".into(),
                ));
            }
        };
        let windows = match m.get("windows") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(list)) if list.len() <= MAX_WINDOWS => list
                .iter()
                .enumerate()
                .map(|(i, w)| parse_window(w).map_err(|e| (format!("schedule.windows[{i}]"), e)))
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err((
                    "schedule.windows".into(),
                    format!("windows 必須是陣列，最多 {MAX_WINDOWS} 個時段"),
                ));
            }
        };
        Ok(ScheduleSpec {
            starts_at,
            ends_at,
            windows,
            timezone,
        })
    }

    /// 資料表中的排程（`{}` 或無法解析＝沒有排程）
    pub fn from_db(text: &str) -> ScheduleSpec {
        serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|v| ScheduleSpec::parse(Some(&v)).ok())
            .unwrap_or_default()
    }

    /// API 與資料表的 JSON（沒有排程時為 `{}`）
    pub fn json(&self) -> Value {
        if self.is_empty() {
            return json!({});
        }
        json!({
            "starts_at": self.starts_at.map(crate::api::http::ts_ms),
            "ends_at": self.ends_at.map(crate::api::http::ts_ms),
            "windows": self.windows.iter().map(WindowSpec::json).collect::<Vec<_>>(),
            "timezone": self.timezone,
        })
    }

    fn offset_ms(&self) -> i64 {
        i64::from(
            self.timezone
                .as_deref()
                .and_then(parse_offset_secs)
                .unwrap_or(8 * 3600),
        ) * 1000
    }

    /// 包含 `t` 的時段中最晚的結束時間
    fn end_of_window_at(&self, t: i64) -> Option<i64> {
        let off = self.offset_ms();
        let day = (t + off).div_euclid(DAY_MS);
        let mut best = None;
        for w in &self.windows {
            for d in [day - 1, day] {
                if w.days & (1 << weekday(d)) == 0 {
                    continue;
                }
                let start = d * DAY_MS + i64::from(w.start_min) * 60_000 - off;
                let end = start + w.duration_ms();
                if start <= t && t < end {
                    best = best.max(Some(end));
                }
            }
        }
        best
    }

    /// `now_ms` 是否在時段內（不看 `starts_at`；`ends_at` 之後一律時段外，時段的結束不超過 `ends_at`）
    pub fn window_at(&self, now_ms: i64) -> WindowState {
        if self.ends_at.is_some_and(|e| now_ms >= e) {
            return WindowState::Closed { next: None };
        }
        if self.windows.is_empty() {
            return WindowState::Open {
                until: self.ends_at,
            };
        }
        if let Some(mut until) = self.end_of_window_at(now_ms) {
            // 相連或重疊的時段視為同一段（最多跨一週）
            for _ in 0..(7 * MAX_WINDOWS) {
                match self.end_of_window_at(until) {
                    Some(next) if next > until => until = next,
                    _ => break,
                }
            }
            let until = self.ends_at.map_or(until, |e| e.min(until));
            return WindowState::Open { until: Some(until) };
        }
        let off = self.offset_ms();
        let day = (now_ms + off).div_euclid(DAY_MS);
        let mut next: Option<i64> = None;
        for w in &self.windows {
            for d in day..=day + 7 {
                if w.days & (1 << weekday(d)) == 0 {
                    continue;
                }
                let start = d * DAY_MS + i64::from(w.start_min) * 60_000 - off;
                if start > now_ms {
                    next = Some(next.map_or(start, |n| n.min(start)));
                    break;
                }
            }
        }
        WindowState::Closed {
            next: next.filter(|n| self.ends_at.is_none_or(|e| *n < e)),
        }
    }

    /// 在 `offered_ms` 提議的主機的 `apply_window`：`(not_before, not_after)`。沒有時段時為提議後
    /// `default_ms`（24 小時），有時段時到該時段結束為止；都不超過 `ends_at`。提議的時間不在任何時段內
    /// （或已過 `ends_at`）時 `None`
    pub fn apply_window(&self, offered_ms: i64, default_ms: i64) -> Option<(i64, i64)> {
        match self.window_at(offered_ms) {
            WindowState::Open { until: Some(u) } if !self.windows.is_empty() => {
                Some((offered_ms, u))
            }
            WindowState::Open { .. } => {
                let end = offered_ms + default_ms;
                Some((offered_ms, self.ends_at.map_or(end, |e| e.min(end))))
            }
            WindowState::Closed { .. } => None,
        }
    }
}

/// 1970-01-01 起第 `day` 天是星期幾（0＝週一）；1970-01-01 是週四
fn weekday(day: i64) -> u32 {
    (day + 3).rem_euclid(7) as u32
}

fn parse_window(v: &Value) -> Result<WindowSpec, String> {
    let Value::Object(m) = v else {
        return Err("每個時段必須是物件".to_string());
    };
    for k in m.keys() {
        if !matches!(k.as_str(), "days" | "start" | "end") {
            return Err(format!("不認得的欄位 {k}"));
        }
    }
    let days = match m.get("days") {
        None | Some(Value::Null) => 0x7f,
        Some(Value::Array(list)) if list.is_empty() => 0x7f,
        Some(Value::Array(list)) => {
            let mut bits = 0u8;
            for d in list {
                let i = d
                    .as_str()
                    .and_then(|s| DAY_NAMES.iter().position(|n| *n == s))
                    .ok_or("days 的項目必須是 mon、tue、wed、thu、fri、sat、sun")?;
                bits |= 1 << i;
            }
            bits
        }
        Some(_) => return Err("days 必須是陣列（mon～sun；省略＝每天）".to_string()),
    };
    let start_min = parse_hhmm(m.get("start")).ok_or("start 必須是 HH:MM（00:00～23:59）")?;
    let end_min = parse_hhmm(m.get("end")).ok_or("end 必須是 HH:MM（00:00～23:59）")?;
    if start_min == end_min {
        return Err("start 與 end 不可相同".to_string());
    }
    Ok(WindowSpec {
        days,
        start_min,
        end_min,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .timestamp_millis()
    }

    fn sched(v: Value) -> ScheduleSpec {
        ScheduleSpec::parse(Some(&v)).unwrap()
    }

    #[test]
    fn empty_schedule_is_always_open_and_round_trips() {
        let s = ScheduleSpec::parse(None).unwrap();
        assert!(s.is_empty());
        assert_eq!(s.json(), json!({}));
        assert_eq!(ScheduleSpec::from_db("{}"), s);
        assert_eq!(ScheduleSpec::from_db("not json"), s);
        assert_eq!(s.window_at(0), WindowState::Open { until: None });
        assert_eq!(s.apply_window(1000, 500), Some((1000, 1500)));
    }

    #[test]
    fn parse_validates_fields() {
        for (v, field) in [
            (json!([]), "schedule"),
            (json!({"x": 1}), "schedule.x"),
            (json!({"starts_at": "tomorrow"}), "schedule.starts_at"),
            (json!({"ends_at": 5}), "schedule.ends_at"),
            (
                json!({"starts_at": "2026-12-02T00:00:00Z", "ends_at": "2026-12-01T00:00:00Z"}),
                "schedule.ends_at",
            ),
            (json!({"timezone": "Asia/Taipei"}), "schedule.timezone"),
            (json!({"windows": {}}), "schedule.windows"),
            (
                json!({"windows": [{"start": "1:00", "end": "05:00"}]}),
                "schedule.windows[0]",
            ),
            (
                json!({"windows": [{"start": "01:00", "end": "24:00"}]}),
                "schedule.windows[0]",
            ),
            (
                json!({"windows": [{"start": "01:00", "end": "01:00"}]}),
                "schedule.windows[0]",
            ),
            (
                json!({"windows": [{"days": ["funday"], "start": "01:00", "end": "02:00"}]}),
                "schedule.windows[0]",
            ),
            (
                json!({"windows": [{"start": "01:00", "end": "02:00", "x": 1}]}),
                "schedule.windows[0]",
            ),
            (
                json!({"windows": vec![json!({"start": "01:00", "end": "02:00"}); MAX_WINDOWS + 1]}),
                "schedule.windows",
            ),
        ] {
            let e = ScheduleSpec::parse(Some(&v)).unwrap_err();
            assert_eq!(e.0, field, "{v}");
        }
        let s = sched(json!({
            "starts_at": "2026-12-01T01:00:00+08:00",
            "ends_at": "2026-12-08T01:00:00+08:00",
            "windows": [{"days": ["sat", "sun"], "start": "01:00", "end": "05:00"}, {"start": "23:00", "end": "02:00"}],
            "timezone": "+08:00",
        }));
        assert_eq!(s.windows[0].days, 0b110_0000);
        assert_eq!(s.windows[1].days, 0x7f);
        // JSON 來回一致
        assert_eq!(ScheduleSpec::from_db(&s.json().to_string()), s);
        assert_eq!(s.json()["windows"][0]["days"], json!(["sat", "sun"]));
        assert_eq!(s.json()["windows"][1]["end"], "02:00");
    }

    #[test]
    fn offsets() {
        assert_eq!(parse_offset_secs("+08:00"), Some(28_800));
        assert_eq!(parse_offset_secs("-05:30"), Some(-19_800));
        assert_eq!(parse_offset_secs("UTC"), Some(0));
        assert_eq!(parse_offset_secs("Z"), Some(0));
        assert_eq!(parse_offset_secs("+8"), None);
        assert_eq!(parse_offset_secs("+15:00"), None);
        assert_eq!(parse_offset_secs("Asia/Taipei"), None);
    }

    #[test]
    fn weekday_of_known_dates() {
        // 2026-11-12 是週四
        let day = ms("2026-11-12T00:00:00Z") / DAY_MS;
        assert_eq!(weekday(day), 3);
        assert_eq!(weekday(day + 3), 6); // 週日
        assert_eq!(weekday(0), 3); // 1970-01-01 週四
    }

    #[test]
    fn daily_window_in_local_time() {
        // 每天 01:00～05:00（+08:00）
        let s =
            sched(json!({"windows": [{"start": "01:00", "end": "05:00"}], "timezone": "+08:00"}));
        assert_eq!(
            s.window_at(ms("2026-11-12T00:59:59+08:00")),
            WindowState::Closed {
                next: Some(ms("2026-11-12T01:00:00+08:00"))
            }
        );
        assert_eq!(
            s.window_at(ms("2026-11-12T01:00:00+08:00")),
            WindowState::Open {
                until: Some(ms("2026-11-12T05:00:00+08:00"))
            }
        );
        assert_eq!(
            s.window_at(ms("2026-11-12T04:59:59+08:00")),
            WindowState::Open {
                until: Some(ms("2026-11-12T05:00:00+08:00"))
            }
        );
        // 結束的那一刻已在時段外；下一個時段是隔天
        assert_eq!(
            s.window_at(ms("2026-11-12T05:00:00+08:00")),
            WindowState::Closed {
                next: Some(ms("2026-11-13T01:00:00+08:00"))
            }
        );
        // 同一個時刻以 UTC 表示也一樣（時段以 timezone 的當地時間計）
        assert!(s.window_at(ms("2026-11-11T18:30:00Z")).is_open());
        // 另一個位移：同樣的 01:00～05:00 在 -05:00 是不同的時刻
        let ny =
            sched(json!({"windows": [{"start": "01:00", "end": "05:00"}], "timezone": "-05:00"}));
        assert!(!ny.window_at(ms("2026-11-11T18:30:00Z")).is_open());
        assert!(ny.window_at(ms("2026-11-12T07:00:00Z")).is_open());
    }

    #[test]
    fn window_crossing_midnight_belongs_to_its_start_day() {
        // 週五 23:00～週六 03:00
        let s = sched(
            json!({"windows": [{"days": ["fri"], "start": "23:00", "end": "03:00"}], "timezone": "+08:00"}),
        );
        // 2026-11-13 是週五
        assert!(!s.window_at(ms("2026-11-13T22:59:00+08:00")).is_open());
        assert_eq!(
            s.window_at(ms("2026-11-13T23:30:00+08:00")),
            WindowState::Open {
                until: Some(ms("2026-11-14T03:00:00+08:00"))
            }
        );
        assert_eq!(
            s.window_at(ms("2026-11-14T02:59:00+08:00")),
            WindowState::Open {
                until: Some(ms("2026-11-14T03:00:00+08:00"))
            }
        );
        // 週六 23:30 不在時段內（時段屬於週五）；下一個是下週五
        assert_eq!(
            s.window_at(ms("2026-11-14T23:30:00+08:00")),
            WindowState::Closed {
                next: Some(ms("2026-11-20T23:00:00+08:00"))
            }
        );
        // 週五凌晨 01:00 不在時段內（那是週四開始的時段才會涵蓋的時間）
        assert!(!s.window_at(ms("2026-11-13T01:00:00+08:00")).is_open());
    }

    #[test]
    fn adjacent_windows_merge_and_next_picks_the_earliest() {
        let s = sched(json!({"windows": [
            {"start": "01:00", "end": "03:00"},
            {"start": "03:00", "end": "04:00"},
            {"days": ["sun"], "start": "12:00", "end": "13:00"},
        ], "timezone": "+08:00"}));
        assert_eq!(
            s.window_at(ms("2026-11-12T01:30:00+08:00")),
            WindowState::Open {
                until: Some(ms("2026-11-12T04:00:00+08:00"))
            }
        );
        // 週六 05:00：下一個是週日 01:00（不是週日 12:00）
        assert_eq!(
            s.window_at(ms("2026-11-14T05:00:00+08:00")),
            WindowState::Closed {
                next: Some(ms("2026-11-15T01:00:00+08:00"))
            }
        );
        // 週日 05:00：下一個是當天 12:00
        assert_eq!(
            s.window_at(ms("2026-11-15T05:00:00+08:00")),
            WindowState::Closed {
                next: Some(ms("2026-11-15T12:00:00+08:00"))
            }
        );
    }

    #[test]
    fn ends_at_caps_the_window_and_closes_afterwards() {
        let s = sched(json!({
            "ends_at": "2026-11-12T03:00:00+08:00",
            "windows": [{"start": "01:00", "end": "05:00"}],
            "timezone": "+08:00",
        }));
        assert_eq!(
            s.window_at(ms("2026-11-12T02:00:00+08:00")),
            WindowState::Open {
                until: Some(ms("2026-11-12T03:00:00+08:00"))
            }
        );
        assert_eq!(
            s.window_at(ms("2026-11-12T03:00:00+08:00")),
            WindowState::Closed { next: None }
        );
        // 時段外：下一個時段在 ends_at 之後就沒有下一個
        assert_eq!(
            s.window_at(ms("2026-11-11T12:00:00+08:00")),
            WindowState::Closed {
                next: Some(ms("2026-11-12T01:00:00+08:00"))
            }
        );
        let only_end = sched(json!({"ends_at": "2026-11-12T03:00:00+08:00"}));
        assert_eq!(
            only_end.window_at(ms("2026-11-12T02:00:00+08:00")),
            WindowState::Open {
                until: Some(ms("2026-11-12T03:00:00+08:00"))
            }
        );
    }

    #[test]
    fn apply_window_follows_the_window_of_the_offer() {
        let day = 24 * 3600 * 1000;
        let s =
            sched(json!({"windows": [{"start": "01:00", "end": "05:00"}], "timezone": "+08:00"}));
        let at = ms("2026-11-12T02:00:00+08:00");
        assert_eq!(
            s.apply_window(at, day),
            Some((at, ms("2026-11-12T05:00:00+08:00")))
        );
        // 提議的時間不在時段內
        assert_eq!(s.apply_window(ms("2026-11-12T06:00:00+08:00"), day), None);
        // 沒有時段：24 小時，但不超過 ends_at
        let e = sched(json!({"ends_at": "2026-11-12T03:00:00+08:00"}));
        assert_eq!(
            e.apply_window(at, day),
            Some((at, ms("2026-11-12T03:00:00+08:00")))
        );
        assert_eq!(e.apply_window(ms("2026-11-12T03:00:00+08:00"), day), None);
    }

    #[test]
    fn missing_timezone_defaults_to_taipei() {
        let s = sched(json!({"windows": [{"start": "01:00", "end": "05:00"}]}));
        assert!(s.window_at(ms("2026-11-12T02:00:00+08:00")).is_open());
    }
}
