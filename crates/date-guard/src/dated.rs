//! 依日期命名的檔名（`<prefix>YYYY-MM-DD<suffix>`）與保留期限的判斷。

use chrono::{Days, NaiveDate};
use std::path::Path;

/// 一類依日期命名的檔案的檔名規則
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatedName {
    pub prefix: &'static str,
    pub suffix: &'static str,
}

impl DatedName {
    /// 某日期的檔名
    pub fn file_name(&self, date: NaiveDate) -> String {
        format!("{}{}{}", self.prefix, date.format("%Y-%m-%d"), self.suffix)
    }

    /// 只有**完全符合** `<prefix>YYYY-MM-DD<suffix>`（且為合法日期）才回傳日期
    pub fn parse(&self, name: &str) -> Option<NaiveDate> {
        let date = name.strip_prefix(self.prefix)?.strip_suffix(self.suffix)?;
        let bytes = date.as_bytes();
        let well_formed = bytes.len() == 10
            && bytes.iter().enumerate().all(|(i, &b)| {
                if i == 4 || i == 7 {
                    b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            });
        if !well_formed {
            return None;
        }
        NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
    }

    /// `dir` 中檔名合法、日期不晚於 `not_after` 的檔案裡最新的日期（目錄不存在或沒有時為 `None`）。
    /// 晚於 `not_after` 的檔案（時鐘誤校到未來時建立的）不列入。
    /// 同步讀取目錄，只在啟動時呼叫一次（作為 [`DateGuard`](crate::DateGuard) 沒有狀態檔時的基準）。
    pub fn newest(&self, dir: &Path, not_after: NaiveDate) -> Option<NaiveDate> {
        std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().and_then(|n| self.parse(n)))
            .filter(|date| *date <= not_after)
            .max()
    }
}

/// 啟動時 [`DateGuard`](crate::DateGuard) 的基準：既有最新檔案的日期，但不晚於今天
/// （檔案日期在未來時以今天為準，避免之後的時鐘跳躍被當成正常）；沒有檔案時為今天
pub fn guard_baseline(newest: Option<NaiveDate>, today: NaiveDate) -> NaiveDate {
    newest.map_or(today, |newest| newest.min(today))
}

/// 保留判斷：今天與前 `retention_days` 天保留，更早的刪除；`retention_days == 0` 表示永久保留
pub fn should_delete(file_date: NaiveDate, today: NaiveDate, retention_days: u32) -> bool {
    if retention_days == 0 {
        return false;
    }
    today
        .checked_sub_days(Days::new(u64::from(retention_days)))
        .is_some_and(|oldest_kept| file_date < oldest_kept)
}
