//! `[alerts]`：告警引擎（`kairos/ALERTS.md` 第 3 節）。

use chrono::FixedOffset;
use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind};

/// 告警暖機期的預設（秒）：大於 Pulse 的 `retry.max_delay` 加抖動（設計包 `alerts-ux.md` §3.3 B）
pub const DEFAULT_WARMUP_SECS: u64 = 600;
/// 告警歷史的預設保留天數（與 1 小時彙總的保留天數相同，讓年度的處理紀錄可查）
pub const DEFAULT_ALERT_RETENTION_DAYS: u32 = 400;
/// 告警事件的筆數上限（超過時刪除最舊的已解決事件與其處理紀錄、發送紀錄）
pub const DEFAULT_MAX_EVENTS: u64 = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct AlertSettings {
    /// `false`：不啟動告警引擎（主控台的告警端點回 503 `alerts_unavailable`）
    pub enabled: bool,
    /// 通知中的連結使用的主控台網址（例如 `https://monitor.example.com`）；空字串時通知不附連結
    pub console_url: String,
    /// 通知內容、事件編號的日期使用的時區：固定位移 `+08:00`（Asia/Taipei 沒有夏令時間）
    pub timezone: String,
    /// Kairos 啟動後的告警暖機期（秒），60～3600
    pub warmup_secs: u64,
    /// 已解決事件的保留天數，1～3660
    pub retention_days: u32,
    /// 事件筆數上限，1,000～10,000,000
    pub max_events: u64,
    /// 網段彙總的遮罩：IPv4 /8～/32、IPv6 /32～/128（設計包 `alerts-ux.md` §3.3 C）
    pub subnet_prefix_v4: u8,
    pub subnet_prefix_v6: u8,
}

impl Default for AlertSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            console_url: String::new(),
            timezone: "+08:00".to_string(),
            warmup_secs: DEFAULT_WARMUP_SECS,
            retention_days: DEFAULT_ALERT_RETENTION_DAYS,
            max_events: DEFAULT_MAX_EVENTS,
            subnet_prefix_v4: 24,
            subnet_prefix_v6: 64,
        }
    }
}

/// `+08:00`、`-03:30`、`UTC`、`Z` → 固定位移
pub fn parse_offset(text: &str) -> Option<FixedOffset> {
    let t = text.trim();
    if t.eq_ignore_ascii_case("utc") || t == "Z" {
        return FixedOffset::east_opt(0);
    }
    let (sign, rest) = match t.as_bytes().first()? {
        b'+' => (1, &t[1..]),
        b'-' => (-1, &t[1..]),
        _ => return None,
    };
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h, m),
        None => (rest, "0"),
    };
    let h: i32 = h.parse().ok()?;
    let m: i32 = m.parse().ok()?;
    if !(0..=14).contains(&h) || !(0..60).contains(&m) {
        return None;
    }
    FixedOffset::east_opt(sign * (h * 3600 + m * 60))
}

impl AlertSettings {
    pub fn offset(&self) -> FixedOffset {
        parse_offset(&self.timezone).unwrap_or_else(|| FixedOffset::east_opt(8 * 3600).unwrap())
    }

    pub fn validate(&self) -> Result<(), Error> {
        let invalid = |msg: String| Err(Error::new(ErrorKind::InvalidInput, msg));
        if parse_offset(&self.timezone).is_none() {
            return invalid(format!(
                "alerts.timezone 的值 \"{}\" 無效：請寫固定位移，例如 \"+08:00\"（不支援時區名稱）",
                self.timezone
            ));
        }
        if !(60..=3600).contains(&self.warmup_secs) {
            return invalid(format!(
                "alerts.warmup_secs 必須介於 60..=3600（目前為 {}）",
                self.warmup_secs
            ));
        }
        if !(1..=3660).contains(&self.retention_days) {
            return invalid(format!(
                "alerts.retention_days 必須介於 1..=3660（目前為 {}）",
                self.retention_days
            ));
        }
        if !(1_000..=10_000_000).contains(&self.max_events) {
            return invalid(format!(
                "alerts.max_events 必須介於 1000..=10000000（目前為 {}）",
                self.max_events
            ));
        }
        if !(8..=32).contains(&self.subnet_prefix_v4) {
            return invalid(format!(
                "alerts.subnet_prefix_v4 必須介於 8..=32（目前為 {}）",
                self.subnet_prefix_v4
            ));
        }
        if !(32..=128).contains(&self.subnet_prefix_v6) {
            return invalid(format!(
                "alerts.subnet_prefix_v6 必須介於 32..=128（目前為 {}）",
                self.subnet_prefix_v6
            ));
        }
        let url = self.console_url.trim();
        if !url.is_empty() && !(url.starts_with("https://") || url.starts_with("http://")) {
            return invalid(format!(
                "alerts.console_url 的值 \"{url}\" 無效：請寫完整網址，例如 https://monitor.example.com"
            ));
        }
        Ok(())
    }

    /// 通知中的事件連結（`console_url` 為空時為 `None`）
    pub fn event_link(&self, event_id: &str) -> Option<String> {
        let base = self.console_url.trim().trim_end_matches('/');
        (!base.is_empty()).then(|| format!("{base}/alerts/events/{event_id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_and_validation() {
        assert_eq!(parse_offset("+08:00").unwrap().local_minus_utc(), 8 * 3600);
        assert_eq!(parse_offset("-03:30").unwrap().local_minus_utc(), -12_600);
        assert_eq!(parse_offset("UTC").unwrap().local_minus_utc(), 0);
        assert_eq!(parse_offset("+8").unwrap().local_minus_utc(), 8 * 3600);
        for bad in ["Asia/Taipei", "+15:00", "08:00", "+08:61", ""] {
            assert!(parse_offset(bad).is_none(), "{bad}");
        }
        let ok = AlertSettings::default();
        assert!(ok.validate().is_ok());
        assert_eq!(ok.event_link("E-1"), None);
        let linked = AlertSettings {
            console_url: "https://m.example/".into(),
            ..AlertSettings::default()
        };
        assert_eq!(
            linked.event_link("E-20261009-0042").as_deref(),
            Some("https://m.example/alerts/events/E-20261009-0042")
        );
        for bad in [
            AlertSettings {
                warmup_secs: 30,
                ..AlertSettings::default()
            },
            AlertSettings {
                timezone: "Asia/Taipei".into(),
                ..AlertSettings::default()
            },
            AlertSettings {
                subnet_prefix_v4: 33,
                ..AlertSettings::default()
            },
            AlertSettings {
                console_url: "monitor".into(),
                ..AlertSettings::default()
            },
            AlertSettings {
                max_events: 10,
                ..AlertSettings::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
