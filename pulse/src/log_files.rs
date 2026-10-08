//! 日誌檔按日分檔（`logs/pulse-YYYY-MM-DD.log`）與保留清除。
//!
//! 檔名日期一律為 **UTC**（與 Kairos 相同），每行開頭的時間戳記則為本機時間。
//! 清除只刪除檔名**完全符合** `pulse-YYYY-MM-DD.log`（且為合法日期）的一般檔案，
//! 其他檔案一律不碰。

use std::fs;
use std::path::Path;

use chrono::{Days, NaiveDate, Utc};

/// 日誌目錄（相對於目前工作目錄）。
pub const LOG_DIR: &str = "logs";
const PREFIX: &str = "pulse-";
const SUFFIX: &str = ".log";

/// fern 的按日分檔輸出：`logs/pulse-YYYY-MM-DD.log`，以 UTC 日期換檔。
pub fn daily_output() -> fern::DateBased {
    fern::DateBased::new(format!("{LOG_DIR}/{PREFIX}"), format!("%Y-%m-%d{SUFFIX}")).utc_time()
}

/// 某日期的日誌檔名。
#[cfg(test)]
fn file_name(date: NaiveDate) -> String {
    format!("{PREFIX}{}{SUFFIX}", date.format("%Y-%m-%d"))
}

/// 只有完全符合 `pulse-YYYY-MM-DD.log`（且為合法日期）才回傳日期。
fn parse(name: &str) -> Option<NaiveDate> {
    let date = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    let bytes = date.as_bytes();
    let well_formed = bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, &b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    if !well_formed {
        return None;
    }
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// 保留判斷：今天與前 `retention_days` 天保留，更早的刪除；`retention_days == 0` 表示永久保留。
fn should_delete(file_date: NaiveDate, today: NaiveDate, retention_days: u32) -> bool {
    if retention_days == 0 {
        return false;
    }
    today
        .checked_sub_days(Days::new(u64::from(retention_days)))
        .is_some_and(|oldest_kept| file_date < oldest_kept)
}

/// 以目前 UTC 日期清除 `LOG_DIR` 中的過期日誌檔，回傳刪除數。
///
/// 阻塞的檔案 I/O；在 async 環境中請用 `spawn_blocking` 呼叫。
pub fn cleanup_now(retention_days: u32) -> usize {
    cleanup(Path::new(LOG_DIR), Utc::now().date_naive(), retention_days)
}

/// 刪除 `dir` 中過期的日誌檔，回傳刪除數。刪除失敗只記 warn。
fn cleanup(dir: &Path, today: NaiveDate, retention_days: u32) -> usize {
    if retention_days == 0 {
        return 0;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!("無法列出 {} 以清除過期的日誌檔：{e}", dir.display());
            return 0;
        }
    };
    let mut removed = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                log::warn!("列出 {} 時發生錯誤：{e}", dir.display());
                break;
            }
        };
        let name = entry.file_name();
        let Some(date) = name.to_str().and_then(parse) else {
            continue;
        };
        if !should_delete(date, today, retention_days)
            || !entry.file_type().is_ok_and(|t| t.is_file())
        {
            continue;
        }
        let path = entry.path();
        match fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                log::info!(
                    "已刪除過期的日誌檔 {}（保留 {retention_days} 天）",
                    path.display()
                );
            }
            Err(e) => log::warn!("無法刪除過期的日誌檔 {}：{e}", path.display()),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn file_name_round_trip() {
        let date = d("2026-10-09");
        assert_eq!(file_name(date), "pulse-2026-10-09.log");
        assert_eq!(parse(&file_name(date)), Some(date));
    }

    #[test]
    fn only_exact_names_are_recognized() {
        for name in [
            "pulse.log",
            "pulse-2026-10-9.log",
            "pulse-2026-10-09.log.bak",
            "pulse-2026-10-09.txt",
            "xpulse-2026-10-09.log",
            "pulse-2026-13-01.log",
            "pulse-2026-02-30.log",
            "pulse-2026_10_09.log",
            "pulse-+026-10-09.log",
            "kairos-2026-10-09.log",
        ] {
            assert_eq!(parse(name), None, "{name}");
        }
    }

    #[test]
    fn retention_boundaries() {
        let today = d("2026-10-09");
        // 保留 14 天：今天與前 14 天（09-25 起）保留
        assert!(!should_delete(d("2026-10-09"), today, 14));
        assert!(!should_delete(d("2026-09-25"), today, 14));
        assert!(should_delete(d("2026-09-24"), today, 14));
        // 0 = 永久保留
        assert!(!should_delete(d("2000-01-01"), today, 0));
        // 上限 3650 天
        assert!(!should_delete(d("2016-10-11"), today, 3650));
        assert!(should_delete(d("2016-10-10"), today, 3650));
        // 未來日期的檔案不刪
        assert!(!should_delete(d("2027-01-01"), today, 1));
    }

    #[test]
    fn cleanup_removes_only_expired_matching_files() {
        let dir: PathBuf = std::env::temp_dir()
            .join("pulse-log-files-tests")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "pulse-2026-10-09.log",
            "pulse-2026-09-25.log",
            "pulse-2026-09-24.log",
            "pulse-2020-01-01.log",
            "pulse.log",
            "notes-2020-01-01.log",
            "pulse-2020-01-01.log.bak",
        ] {
            fs::write(dir.join(name), "x").unwrap();
        }
        // 符合檔名的目錄不刪
        fs::create_dir(dir.join("pulse-2019-01-01.log")).unwrap();

        assert_eq!(cleanup(&dir, d("2026-10-09"), 14), 2);
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "notes-2020-01-01.log",
                "pulse-2019-01-01.log",
                "pulse-2020-01-01.log.bak",
                "pulse-2026-09-25.log",
                "pulse-2026-10-09.log",
                "pulse.log",
            ]
        );
        // retention_days = 0 不刪任何檔案
        assert_eq!(cleanup(&dir, d("2030-01-01"), 0), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cleanup_missing_dir_is_not_fatal() {
        assert_eq!(
            cleanup(Path::new("no-such-log-dir-xyz"), d("2026-10-09"), 14),
            0
        );
    }
}
