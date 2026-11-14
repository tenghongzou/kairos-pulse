//! 推送的排程與升級時段（REMOTE.md 第 4.7.1、4.7.5 節；0.10）。
//!
//! `schedule` = `{starts_at?, ends_at?, windows[], timezone}`：
//!
//! - `starts_at`：在這之前推送為 `scheduled`，不提議任何主機。
//! - `ends_at`：到時推送結束（尚未開始替換的主機標為 `expired`／`rollout_ended`）。
//! - `windows[]`：每週的時段（`days`：`mon`～`sun`，省略或空陣列＝每天；`start`、`end`：`HH:MM`，
//!   `end` 小於 `start` 時跨午夜，時段屬於開始的那一天）。只在時段內提議主機，提議的 `apply_window`
//!   到時段結束為止。沒有時段＝隨時。
//! - `timezone`：時段的時區。IANA 時區名稱（`Asia/Taipei`、`Europe/Berlin`；時區資料庫由 `chrono-tz`
//!   編進執行檔）或**固定位移**（`+08:00`，與 `alerts.timezone` 相同的寫法；另接受 `UTC`、`Z`）。
//!
//! 日光節約時間的規則（REMOTE.md 第 4.7.5 節「時段的時區與日光節約時間」）：時段屬於它開始的那個當地
//! 日期，兩端各自換算成絕對時間（長度是牆上時鐘的長度）；不存在的時刻換算為切換的那一刻（只會延後）；
//! 重複的時刻，開始取第二次、結束取開始之後的第一次；換算後結束沒有晚於開始時，時段從換算後的開始起
//! 維持牆上時鐘的長度（不會變成零長度）。
//!
//! 這裡只有純函式（以毫秒時間計算），方便以假的時鐘測試。

use chrono::{
    DateTime, Datelike, Days, FixedOffset, LocalResult, NaiveDateTime, NaiveTime, SecondsFormat,
    TimeZone, Utc,
};
use serde_json::{Value, json};

/// 時段的數量上限
pub const MAX_WINDOWS: usize = 16;
/// 一天的分鐘數
const DAY_MIN: i64 = 1440;
/// `timezone` 的長度上限（bytes；時區資料庫最長的名稱是 32）
const MAX_TIMEZONE_BYTES: usize = 64;
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

/// 時段的時區：固定位移，或 IANA 時區（有日光節約時間的規則）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Fixed(FixedOffset),
    Named(chrono_tz::Tz),
}

impl Zone {
    /// `schedule.timezone` → (保存的寫法, 時區)。固定位移照 0.10.0 的寫法（[`parse_offset_secs`]）；其他
    /// 必須是時區資料庫中的名稱（大小寫完全相同，含連結的舊名），不認得時 `None`
    pub fn parse(s: &str) -> Option<(String, Zone)> {
        let s = s.trim();
        if let Some(secs) = parse_offset_secs(s) {
            return Some((s.to_string(), Zone::Fixed(FixedOffset::east_opt(secs)?)));
        }
        if s.is_empty() || s.len() > MAX_TIMEZONE_BYTES {
            return None;
        }
        let tz: chrono_tz::Tz = s.parse().ok()?;
        Some((tz.name().to_string(), Zone::Named(tz)))
    }

    /// `ms` 的當地時間（牆上時鐘）
    fn local(&self, ms: i64) -> Option<NaiveDateTime> {
        let utc = DateTime::<Utc>::from_timestamp_millis(ms)?;
        Some(match self {
            Zone::Fixed(z) => utc.with_timezone(z).naive_local(),
            Zone::Named(z) => utc.with_timezone(z).naive_local(),
        })
    }

    /// 當地時間 → 絕對時間（毫秒）：唯一、重複（較早、較晚）或不存在
    fn resolve(&self, local: &NaiveDateTime) -> LocalResult<i64> {
        match self {
            Zone::Fixed(z) => z.from_local_datetime(local).map(|t| t.timestamp_millis()),
            Zone::Named(z) => z.from_local_datetime(local).map(|t| t.timestamp_millis()),
        }
    }

    /// 不存在的當地時間（被跳過的區間）→ 切換的那一刻：時鐘第一次顯示不早於 `local` 的時刻。位移都在
    /// ±16 小時內，以二分法在前後 16 小時之間找（秒）
    fn after_gap(&self, local: &NaiveDateTime) -> i64 {
        let guess = local.and_utc().timestamp();
        let (mut lo, mut hi) = (guess - 16 * 3600, guess + 16 * 3600);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if self.local(mid * 1000).is_some_and(|l| l >= *local) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        hi * 1000
    }

    /// `ms` 的當地時間（RFC 3339，含當時的位移；例如 `2026-03-29T03:00:00+02:00`）
    pub fn rfc3339(&self, ms: i64) -> Option<String> {
        let utc = DateTime::<Utc>::from_timestamp_millis(ms)?;
        Some(match self {
            Zone::Fixed(z) => utc
                .with_timezone(z)
                .to_rfc3339_opts(SecondsFormat::Secs, false),
            Zone::Named(z) => utc
                .with_timezone(z)
                .to_rfc3339_opts(SecondsFormat::Secs, false),
        })
    }

    /// `ms` 當時的位移（`+08:00`）
    fn offset_at(&self, ms: i64) -> Option<String> {
        self.rfc3339(ms)
            .and_then(|s| s.get(s.len().checked_sub(6)?..).map(str::to_string))
    }
}

/// 某一次時段受日光節約時間影響的方式（API 的 `upcoming_windows[].dst`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DstNote {
    /// 跳過（撥快）：端點落在不存在的時刻，或實際長度比牆上時鐘的長度短
    Gap,
    /// 重複（撥慢）：端點落在重複的時刻，或實際長度比牆上時鐘的長度長
    Fold,
}

impl DstNote {
    fn as_str(self) -> &'static str {
        match self {
            DstNote::Gap => "gap",
            DstNote::Fold => "fold",
        }
    }
}

/// 時段的某一次（絕對時間；相連或重疊的已合併）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Occurrence {
    pub start: i64,
    pub end: i64,
    pub dst: Option<DstNote>,
}

/// 推送的排程
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScheduleSpec {
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub windows: Vec<WindowSpec>,
    /// IANA 時區名稱或固定位移（文字）；`None`＝建立時以 Kairos 的 `alerts.timezone` 填入
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
            Some(v) => match v.as_str().and_then(Zone::parse) {
                Some((name, _)) => Some(name),
                None => {
                    return Err((
                        "schedule.timezone".into(),
                        "timezone 必須是 IANA 時區名稱（例如 Asia/Taipei、Europe/Berlin；大小寫須相同）或固定位移（例如 +08:00）".into(),
                    ));
                }
            },
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

    /// 時段的時區（沒有指定、或資料表中的值無法解析時 `+08:00`）
    pub fn zone(&self) -> Zone {
        self.timezone
            .as_deref()
            .and_then(Zone::parse)
            .map(|(_, z)| z)
            .unwrap_or_else(|| {
                Zone::Fixed(FixedOffset::east_opt(8 * 3600).expect("+08:00 是合法的位移"))
            })
    }

    /// `around_ms` 的當地日期前 2 天到後 8 天之間開始的每一次時段（絕對時間），依開始時間排序，相連或重疊的
    /// 合併為一段。範圍涵蓋「包含 `around_ms` 的時段」與「之後一週內的下一個時段」
    fn occurrences(&self, around_ms: i64) -> Vec<Occurrence> {
        let zone = self.zone();
        let Some(today) = zone.local(around_ms).map(|l| l.date()) else {
            return Vec::new();
        };
        let mut all = Vec::new();
        for offset in 0..=10u64 {
            let Some(date) = today
                .checked_sub_days(Days::new(2))
                .and_then(|d| d.checked_add_days(Days::new(offset)))
            else {
                continue;
            };
            let bit = 1u8 << date.weekday().num_days_from_monday();
            for w in self.windows.iter().filter(|w| w.days & bit != 0) {
                if let Some(o) = w.occurrence(&zone, date) {
                    all.push(o);
                }
            }
        }
        all.sort_by_key(|o| (o.start, o.end));
        let mut merged: Vec<Occurrence> = Vec::with_capacity(all.len());
        for o in all {
            match merged.last_mut() {
                Some(last) if o.start <= last.end => {
                    last.end = last.end.max(o.end);
                    last.dst = last.dst.or(o.dst);
                }
                _ => merged.push(o),
            }
        }
        merged
    }

    /// 接下來的 `n` 次時段（含目前所在的那一次；`starts_at` 之前結束的不算，`ends_at` 之後開始的不算，
    /// 結束不超過 `ends_at`）。沒有時段時是空的
    pub fn upcoming(&self, now_ms: i64, n: usize) -> Vec<Occurrence> {
        let from = self.starts_at.map_or(now_ms, |s| s.max(now_ms));
        self.occurrences(from)
            .into_iter()
            .filter(|o| o.end > from && self.ends_at.is_none_or(|e| o.start < e))
            .map(|mut o| {
                o.end = self.ends_at.map_or(o.end, |e| e.min(o.end));
                o
            })
            .take(n)
            .collect()
    }

    /// API 的 `upcoming_windows[]`：接下來幾次時段實際的開始與結束（`start`／`end` 為 UTC，`start_local`／
    /// `end_local` 為 `timezone` 的當地時間、含當時的位移）、實際長度（分鐘）與日光節約時間的影響
    pub fn upcoming_json(&self, now_ms: i64, n: usize) -> Value {
        let zone = self.zone();
        Value::Array(
            self.upcoming(now_ms, n)
                .iter()
                .map(|o| {
                    json!({
                        "start": crate::api::http::ts_ms(o.start),
                        "end": crate::api::http::ts_ms(o.end),
                        "start_local": zone.rfc3339(o.start),
                        "end_local": zone.rfc3339(o.end),
                        "minutes": (o.end - o.start) / 60_000,
                        "dst": o.dst.map(DstNote::as_str),
                    })
                })
                .collect(),
        )
    }

    /// API 的 `timezone`：時段實際使用的時區（`kind`：`iana`／`fixed`）、目前的位移與時區資料庫的版本
    pub fn timezone_json(&self, now_ms: i64) -> Value {
        let zone = self.zone();
        json!({
            "name": self.timezone,
            "kind": match zone { Zone::Fixed(_) => "fixed", Zone::Named(_) => "iana" },
            "offset": zone.offset_at(now_ms),
            "tzdb": chrono_tz::IANA_TZDB_VERSION,
        })
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
        // 相連或重疊的時段已合併為一段（最多到一週多之後）
        let occurrences = self.occurrences(now_ms);
        if let Some(o) = occurrences
            .iter()
            .find(|o| o.start <= now_ms && now_ms < o.end)
        {
            let until = self.ends_at.map_or(o.end, |e| e.min(o.end));
            return WindowState::Open { until: Some(until) };
        }
        let next = occurrences
            .iter()
            .map(|o| o.start)
            .find(|start| *start > now_ms);
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

impl WindowSpec {
    /// 當地日期 `date` 開始的這一次（REMOTE.md 第 4.7.5 節的規則 1～5）
    fn occurrence(&self, zone: &Zone, date: chrono::NaiveDate) -> Option<Occurrence> {
        let time = |m: u16| NaiveTime::from_hms_opt(u32::from(m / 60), u32::from(m % 60), 0);
        let start_local = date.and_time(time(self.start_min)?);
        let end_date = if self.end_min <= self.start_min {
            date.checked_add_days(Days::new(1))?
        } else {
            date
        };
        let end_local = end_date.and_time(time(self.end_min)?);
        let mut dst = None;
        // 開始：不存在 → 切換的那一刻（延後）；重複 → 第二次
        let start = match zone.resolve(&start_local) {
            LocalResult::Single(t) => t,
            LocalResult::Ambiguous(_, later) => {
                dst = Some(DstNote::Fold);
                later
            }
            LocalResult::None => {
                dst = Some(DstNote::Gap);
                zone.after_gap(&start_local)
            }
        };
        // 結束：不存在 → 切換的那一刻；重複 → 開始之後的第一次
        let mut end = match zone.resolve(&end_local) {
            LocalResult::Single(t) => t,
            LocalResult::Ambiguous(earlier, later) => {
                dst.get_or_insert(DstNote::Fold);
                if earlier > start { earlier } else { later }
            }
            LocalResult::None => {
                dst.get_or_insert(DstNote::Gap);
                zone.after_gap(&end_local)
            }
        };
        let nominal = self.duration_ms();
        // 不會變成零長度：從換算後的開始起維持牆上時鐘的長度
        if end <= start {
            end = start + nominal;
            dst.get_or_insert(DstNote::Gap);
        }
        // 端點沒有受影響、但切換落在時段中間：實際長度與牆上時鐘的長度不同
        if dst.is_none() && end - start != nominal {
            dst = Some(if end - start < nominal {
                DstNote::Gap
            } else {
                DstNote::Fold
            });
        }
        Some(Occurrence { start, end, dst })
    }
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
            (json!({"timezone": "Asia/Taipe"}), "schedule.timezone"),
            (json!({"timezone": 8}), "schedule.timezone"),
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

    // ---- IANA 時區與日光節約時間（REMOTE.md 第 4.7.5 節的規則；時間都是假的時鐘） ----

    /// 每天一個時段
    fn daily(tz: &str, start: &str, end: &str) -> ScheduleSpec {
        sched(json!({"windows": [{"start": start, "end": end}], "timezone": tz}))
    }

    fn open(until: &str) -> WindowState {
        WindowState::Open {
            until: Some(ms(until)),
        }
    }

    fn closed(next: &str) -> WindowState {
        WindowState::Closed {
            next: Some(ms(next)),
        }
    }

    /// `at` 所在或之後的第一次時段：(開始, 結束, dst)
    fn first(s: &ScheduleSpec, at: &str) -> (i64, i64, Option<DstNote>) {
        let o = s.upcoming(ms(at), 1)[0];
        (o.start, o.end, o.dst)
    }

    #[test]
    fn timezone_accepts_iana_names_and_fixed_offsets_and_rejects_the_rest() {
        for (given, stored) in [
            ("Asia/Taipei", "Asia/Taipei"),
            ("Europe/Berlin", "Europe/Berlin"),
            (
                "America/Argentina/Buenos_Aires",
                "America/Argentina/Buenos_Aires",
            ),
            // 連結的舊名、Etc 的名稱也是時區資料庫的名稱：原樣保存
            ("Asia/Calcutta", "Asia/Calcutta"),
            ("Etc/GMT-8", "Etc/GMT-8"),
            ("  Europe/Berlin ", "Europe/Berlin"),
            // 0.10.0 的固定位移照舊（已經存在資料庫的推送）
            ("+08:00", "+08:00"),
            ("-05:30", "-05:30"),
            ("UTC", "UTC"),
            ("Z", "Z"),
        ] {
            let s =
                sched(json!({"timezone": given, "windows": [{"start": "01:00", "end": "02:00"}]}));
            assert_eq!(s.timezone.as_deref(), Some(stored), "{given}");
            // 資料表的 JSON 來回一致
            assert_eq!(ScheduleSpec::from_db(&s.json().to_string()), s, "{given}");
        }
        for bad in [
            "",
            "Asia/Taipe",
            "asia/taipei",
            "EUROPE/BERLIN",
            "Mars/Olympus_Mons",
            "+8",
            "+08",
            "GMT+8",
            "Asia/Taipei;DROP",
            "../etc/passwd",
            &"A".repeat(200),
        ] {
            let e = ScheduleSpec::parse(Some(&json!({"timezone": bad}))).unwrap_err();
            assert_eq!(e.0, "schedule.timezone", "{bad}");
            assert!(e.1.contains("IANA"), "{}", e.1);
        }
        assert!(matches!(Zone::parse("UTC"), Some((_, Zone::Fixed(_)))));
        assert!(matches!(
            Zone::parse("Europe/Berlin"),
            Some((_, Zone::Named(_)))
        ));
    }

    #[test]
    fn iana_zone_without_dst_matches_the_fixed_offset() {
        // Asia/Taipei 沒有日光節約時間：與 +08:00 的每一個結果相同（固定位移的回歸）
        let windows = json!([
            {"days": ["sat", "sun"], "start": "23:00", "end": "03:00"},
            {"start": "01:00", "end": "05:00"},
            {"days": ["wed"], "start": "12:00", "end": "12:30"},
        ]);
        let named = sched(json!({"windows": windows, "timezone": "Asia/Taipei"}));
        let fixed = sched(json!({"windows": windows, "timezone": "+08:00"}));
        let mut t = ms("2026-03-20T00:00:00Z");
        while t < ms("2026-04-10T00:00:00Z") {
            assert_eq!(named.window_at(t), fixed.window_at(t), "{t}");
            assert_eq!(named.upcoming(t, 3), fixed.upcoming(t, 3), "{t}");
            t += 37 * 60_000;
        }
        assert!(fixed.upcoming(t, 3).iter().all(|o| o.dst.is_none()));
    }

    #[test]
    fn iana_zone_follows_the_local_clock_across_seasons() {
        // Europe/Berlin 每天 01:00～05:00：冬天是 +01:00、夏天是 +02:00
        let s = daily("Europe/Berlin", "01:00", "05:00");
        assert_eq!(
            s.window_at(ms("2026-11-12T00:00:00Z")),
            open("2026-11-12T04:00:00Z")
        );
        assert_eq!(
            s.window_at(ms("2026-06-30T22:59:59Z")),
            closed("2026-06-30T23:00:00Z")
        );
        assert_eq!(
            s.window_at(ms("2026-06-30T23:00:00Z")),
            open("2026-07-01T03:00:00Z")
        );
        assert_eq!(first(&s, "2026-07-01T00:00:00Z").2, None);
    }

    #[test]
    fn spring_forward_northern_hemisphere() {
        // Europe/Berlin 2026-03-29：02:00～03:00 不存在（01:00Z 從 +01:00 變成 +02:00）
        // 切換落在時段中間：牆上時鐘 01:00～05:00，實際 3 小時
        let s = daily("Europe/Berlin", "01:00", "05:00");
        assert_eq!(
            first(&s, "2026-03-29T00:30:00Z"),
            (
                ms("2026-03-29T00:00:00Z"),
                ms("2026-03-29T03:00:00Z"),
                Some(DstNote::Gap)
            )
        );
        // 開始落在不存在的時刻：延後到切換的那一刻（當地 03:00），不會提早
        let s = daily("Europe/Berlin", "02:30", "04:00");
        assert_eq!(
            s.window_at(ms("2026-03-29T00:59:59Z")),
            closed("2026-03-29T01:00:00Z")
        );
        assert_eq!(
            s.window_at(ms("2026-03-29T01:00:00Z")),
            open("2026-03-29T02:00:00Z")
        );
        // 前一天與後一天照常（02:30 CET、02:30 CEST）
        assert_eq!(
            s.window_at(ms("2026-03-28T01:29:59Z")),
            closed("2026-03-28T01:30:00Z")
        );
        assert_eq!(
            s.window_at(ms("2026-03-29T02:00:00Z")),
            closed("2026-03-30T00:30:00Z")
        );
        // 預覽看得到實際的開始（含位移）與 dst 標記
        let up = s.upcoming_json(ms("2026-03-28T12:00:00Z"), 2);
        assert_eq!(up[0]["start_local"], "2026-03-29T03:00:00+02:00");
        assert_eq!(up[0]["end_local"], "2026-03-29T04:00:00+02:00");
        assert_eq!(up[0]["minutes"], 60);
        assert_eq!(up[0]["dst"], "gap");
        assert_eq!(up[1]["start_local"], "2026-03-30T02:30:00+02:00");
        assert_eq!(up[1]["dst"], Value::Null);
        // 結束落在不存在的時刻：在切換的那一刻結束
        let s = daily("Europe/Berlin", "01:00", "02:30");
        assert_eq!(
            first(&s, "2026-03-29T00:10:00Z"),
            (
                ms("2026-03-29T00:00:00Z"),
                ms("2026-03-29T01:00:00Z"),
                Some(DstNote::Gap)
            )
        );
        // 整個時段落在不存在的區間：不會變成零長度，從當地 03:00 起維持 30 分鐘
        let s = daily("Europe/Berlin", "02:15", "02:45");
        assert_eq!(
            first(&s, "2026-03-29T00:10:00Z"),
            (
                ms("2026-03-29T01:00:00Z"),
                ms("2026-03-29T01:30:00Z"),
                Some(DstNote::Gap)
            )
        );
        assert!(s.window_at(ms("2026-03-29T01:10:00Z")).is_open());
        // 開始被延後到恰好等於結束：同樣維持牆上時鐘的長度
        let s = daily("Europe/Berlin", "02:30", "03:00");
        assert_eq!(
            first(&s, "2026-03-29T00:10:00Z"),
            (
                ms("2026-03-29T01:00:00Z"),
                ms("2026-03-29T01:30:00Z"),
                Some(DstNote::Gap)
            )
        );
        // America/New_York 2026-03-08：02:00～03:00 不存在（07:00Z）
        let s = daily("America/New_York", "02:00", "04:00");
        assert_eq!(
            first(&s, "2026-03-08T06:00:00Z"),
            (
                ms("2026-03-08T07:00:00Z"),
                ms("2026-03-08T08:00:00Z"),
                Some(DstNote::Gap)
            )
        );
    }

    #[test]
    fn fall_back_northern_hemisphere() {
        // Europe/Berlin 2026-10-25：02:00～03:00 出現兩次（第一次 +02:00＝00:00Z～01:00Z，第二次 +01:00）
        // 開始落在重複的時刻：取第二次（寧可晚開始）
        let s = daily("Europe/Berlin", "02:30", "04:00");
        assert_eq!(
            s.window_at(ms("2026-10-25T00:30:00Z")),
            closed("2026-10-25T01:30:00Z")
        );
        assert_eq!(
            s.window_at(ms("2026-10-25T01:30:00Z")),
            open("2026-10-25T03:00:00Z")
        );
        assert_eq!(first(&s, "2026-10-25T00:30:00Z").2, Some(DstNote::Fold));
        // 結束落在重複的時刻：開始之後第一次顯示 02:30 的時候（實際長度與平常相同）
        let s = daily("Europe/Berlin", "01:00", "02:30");
        assert_eq!(
            first(&s, "2026-10-24T23:30:00Z"),
            (
                ms("2026-10-24T23:00:00Z"),
                ms("2026-10-25T00:30:00Z"),
                Some(DstNote::Fold)
            )
        );
        // 兩端都重複：第二次 02:15 到第二次 02:45，不是零長度或負的
        let s = daily("Europe/Berlin", "02:15", "02:45");
        assert_eq!(
            first(&s, "2026-10-25T00:00:00Z"),
            (
                ms("2026-10-25T01:15:00Z"),
                ms("2026-10-25T01:45:00Z"),
                Some(DstNote::Fold)
            )
        );
        assert!(!s.window_at(ms("2026-10-25T00:30:00Z")).is_open());
        // 重複的區間完整落在時段中間：牆上時鐘 01:00～05:00，實際 5 小時
        let s = daily("Europe/Berlin", "01:00", "05:00");
        assert_eq!(
            first(&s, "2026-10-25T00:00:00Z"),
            (
                ms("2026-10-24T23:00:00Z"),
                ms("2026-10-25T04:00:00Z"),
                Some(DstNote::Fold)
            )
        );
        // America/New_York 2026-11-01：01:00～02:00 出現兩次（第一次 -04:00＝05:00Z～06:00Z）
        let s = daily("America/New_York", "01:30", "03:00");
        assert_eq!(
            s.window_at(ms("2026-11-01T05:30:00Z")),
            closed("2026-11-01T06:30:00Z")
        );
        assert_eq!(
            s.window_at(ms("2026-11-01T06:30:00Z")),
            open("2026-11-01T08:00:00Z")
        );
        let s = daily("America/New_York", "00:30", "01:30");
        assert_eq!(
            first(&s, "2026-11-01T05:00:00Z"),
            (
                ms("2026-11-01T04:30:00Z"),
                ms("2026-11-01T05:30:00Z"),
                Some(DstNote::Fold)
            )
        );
    }

    #[test]
    fn southern_hemisphere_transitions() {
        // Australia/Sydney：10 月撥快（2026-10-04 02:00 → 03:00，UTC 10-03 16:00）
        let s = daily("Australia/Sydney", "02:30", "04:00");
        assert_eq!(
            s.window_at(ms("2026-10-03T15:59:59Z")),
            closed("2026-10-03T16:00:00Z")
        );
        assert_eq!(
            s.window_at(ms("2026-10-03T16:00:00Z")),
            open("2026-10-03T17:00:00Z")
        );
        let up = s.upcoming_json(ms("2026-10-03T12:00:00Z"), 1);
        assert_eq!(up[0]["start_local"], "2026-10-04T03:00:00+11:00");
        assert_eq!(up[0]["dst"], "gap");
        // 4 月撥慢（2026-04-05 03:00 → 02:00，UTC 04-04 16:00）：02:00～03:00 出現兩次，開始取第二次
        assert_eq!(
            s.window_at(ms("2026-04-04T15:30:00Z")),
            closed("2026-04-04T16:30:00Z")
        );
        assert_eq!(
            s.window_at(ms("2026-04-04T16:30:00Z")),
            open("2026-04-04T18:00:00Z")
        );
        let up = s.upcoming_json(ms("2026-04-04T12:00:00Z"), 1);
        assert_eq!(up[0]["start_local"], "2026-04-05T02:30:00+10:00");
        assert_eq!(up[0]["dst"], "fold");
        // America/Santiago：午夜被跳過（2026-09-06 00:00 → 01:00，UTC 04:00）。00:00 開始的時段在當地
        // 01:00 開始，仍屬於那個日期（週日）
        let s = sched(json!({
            "windows": [{"days": ["sun"], "start": "00:00", "end": "02:00"}],
            "timezone": "America/Santiago",
        }));
        assert_eq!(
            first(&s, "2026-09-05T12:00:00Z"),
            (
                ms("2026-09-06T04:00:00Z"),
                ms("2026-09-06T05:00:00Z"),
                Some(DstNote::Gap)
            )
        );
        let up = s.upcoming_json(ms("2026-09-05T12:00:00Z"), 1);
        assert_eq!(up[0]["start_local"], "2026-09-06T01:00:00-03:00");
    }

    #[test]
    fn windows_crossing_midnight_on_transition_days() {
        // Europe/Berlin 週六 23:00～03:00：屬於開始的那一天（週六），兩端各自換算
        let s = sched(json!({
            "windows": [{"days": ["sat"], "start": "23:00", "end": "03:00"}],
            "timezone": "Europe/Berlin",
        }));
        // 2026-03-28（週六）開始：23:00 CET～03:00 CEST，實際 3 小時
        assert_eq!(
            first(&s, "2026-03-28T12:00:00Z"),
            (
                ms("2026-03-28T22:00:00Z"),
                ms("2026-03-29T01:00:00Z"),
                Some(DstNote::Gap)
            )
        );
        assert_eq!(
            s.window_at(ms("2026-03-29T00:30:00Z")),
            open("2026-03-29T01:00:00Z")
        );
        // 週日晚上 23:30 不在時段內（時段屬於週六）；下一次是下週六 23:00 CEST
        assert_eq!(
            s.window_at(ms("2026-03-29T21:30:00Z")),
            closed("2026-04-04T21:00:00Z")
        );
        // 2026-10-24（週六）開始：23:00 CEST～03:00 CET，實際 5 小時
        assert_eq!(
            first(&s, "2026-10-24T12:00:00Z"),
            (
                ms("2026-10-24T21:00:00Z"),
                ms("2026-10-25T02:00:00Z"),
                Some(DstNote::Fold)
            )
        );
        // 平常的週六：4 小時、沒有標記
        assert_eq!(
            first(&s, "2026-11-14T12:00:00Z"),
            (ms("2026-11-14T22:00:00Z"), ms("2026-11-15T02:00:00Z"), None)
        );
        // 南半球：Australia/Sydney 2026-04-04（週六）23:00 AEDT～隔天 03:00 AEST，實際 5 小時
        let s = sched(json!({
            "windows": [{"days": ["sat"], "start": "23:00", "end": "03:00"}],
            "timezone": "Australia/Sydney",
        }));
        assert_eq!(
            first(&s, "2026-04-04T00:00:00Z"),
            (
                ms("2026-04-04T12:00:00Z"),
                ms("2026-04-04T17:00:00Z"),
                Some(DstNote::Fold)
            )
        );
        // 提議的 apply_window 到換算後的時段結束為止
        let at = ms("2026-04-04T16:30:00Z");
        assert_eq!(
            s.apply_window(at, 86_400_000),
            Some((at, ms("2026-04-04T17:00:00Z")))
        );
    }

    #[test]
    fn upcoming_respects_starts_at_and_ends_at_and_reports_the_timezone() {
        let s = sched(json!({
            "starts_at": "2026-03-29T00:30:00Z",
            "ends_at": "2026-03-30T02:00:00Z",
            "windows": [{"start": "01:00", "end": "05:00"}],
            "timezone": "Europe/Berlin",
        }));
        // starts_at 之前結束的不算；結束不超過 ends_at（03-30 的時段 23:00Z～03:00Z 截到 02:00Z）
        let up = s.upcoming(ms("2026-03-27T00:00:00Z"), 5);
        assert_eq!(
            up.iter().map(|o| (o.start, o.end)).collect::<Vec<_>>(),
            vec![
                (ms("2026-03-29T00:00:00Z"), ms("2026-03-29T03:00:00Z")),
                (ms("2026-03-29T23:00:00Z"), ms("2026-03-30T02:00:00Z")),
            ]
        );
        let tz = s.timezone_json(ms("2026-03-29T02:00:00Z"));
        assert_eq!(tz["name"], "Europe/Berlin");
        assert_eq!(tz["kind"], "iana");
        assert_eq!(tz["offset"], "+02:00");
        assert_eq!(tz["tzdb"], chrono_tz::IANA_TZDB_VERSION);
        let fixed = daily("-05:30", "01:00", "02:00").timezone_json(0);
        assert_eq!(fixed["kind"], "fixed");
        assert_eq!(fixed["offset"], "-05:30");
        // 沒有時段：沒有 upcoming
        assert!(ScheduleSpec::default().upcoming(0, 3).is_empty());
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
