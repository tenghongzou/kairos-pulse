//! 時間：`sent_at` 的解析、`server_time` 的格式與時鐘偏差的判定（ENROLL.md「時鐘檢查」；
//! 設計包 `kairos/screens.md` §24 #23、D27）。
//!
//! 本 crate 不讀系統時間：「現在」一律由呼叫端以 Unix 毫秒傳入（Kairos 用收到請求標頭時的時間）。

use chrono::{DateTime, Utc};
use std::fmt;

/// 時鐘偏差的門檻（毫秒）：`|收到時間 − sent_at|` **超過** 5 分鐘時回 `clock_skew`（恰好 5 分鐘受理）
pub const MAX_CLOCK_SKEW_MS: i64 = 300_000;

/// `sent_at` 的長度上限（bytes）
pub const MAX_SENT_AT_BYTES: usize = 64;

/// 時間格式錯誤
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeFormatError;

impl fmt::Display for TimeFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("must be an RFC 3339 timestamp with time zone, e.g. 2026-10-11T02:13:05.000Z")
    }
}

impl std::error::Error for TimeFormatError {}

/// 解析 RFC 3339 時間（含時區；日期與時間之間必須是大寫 `T`，最長 64 bytes），回傳 Unix 毫秒
pub fn parse_rfc3339_ms(s: &str) -> Result<i64, TimeFormatError> {
    if s.len() > MAX_SENT_AT_BYTES || !s.is_ascii() || s.as_bytes().get(10) != Some(&b'T') {
        return Err(TimeFormatError);
    }
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.timestamp_millis())
        .map_err(|_| TimeFormatError)
}

/// 協定上的時間格式：RFC 3339、UTC、`Z` 結尾、毫秒，例 `2026-10-11T18:13:05.123Z`
/// （與 8443 的 `hello_ack.server_time`、主控台 API 相同）
pub fn format_rfc3339_ms(unix_ms: i64) -> String {
    match DateTime::<Utc>::from_timestamp_millis(unix_ms) {
        Some(t) => t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        None => "1970-01-01T00:00:00.000Z".to_string(),
    }
}

/// 時鐘偏差（毫秒）：`sent_at − 伺服器收到請求的時間`。負值＝主機時鐘較慢
/// （與 Pulse `enrollment.json` 的 `offset_ms` 同號）
pub fn clock_offset_ms(sent_at_ms: i64, received_ms: i64) -> i64 {
    sent_at_ms.saturating_sub(received_ms)
}

/// 偏差是否超過門檻（[`MAX_CLOCK_SKEW_MS`]，恰好等於時受理）
pub fn exceeds_clock_skew(offset_ms: i64) -> bool {
    offset_ms.unsigned_abs() > MAX_CLOCK_SKEW_MS as u64
}
