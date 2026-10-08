//! 依日期命名的檔案（`<prefix>YYYY-MM-DD<suffix>`）的檔名規則與保留清除。
//!
//! reports（`reports-YYYY-MM-DD.jsonl`）與日誌（`kairos-YYYY-MM-DD.log`）共用；日期一律為 UTC。

use chrono::{Days, NaiveDate};
use log::{info, warn};
use std::path::Path;
use tokio::fs;

/// 一類依日期命名的檔案
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatedFiles {
    pub prefix: &'static str,
    pub suffix: &'static str,
    /// 日誌中稱呼這類檔案的名稱
    pub label: &'static str,
}

impl DatedFiles {
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

    /// 刪除 `dir` 中過期的檔案，回傳刪除數。檔名不完全符合的檔案一律不碰；刪除失敗只記 warn。
    pub async fn cleanup(&self, dir: &Path, today: NaiveDate, retention_days: u32) -> usize {
        if retention_days == 0 {
            return 0;
        }
        let mut entries = match fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!(
                    "無法列出 {} 以清除過期的{}: {}",
                    dir.display(),
                    self.label,
                    e
                );
                return 0;
            }
        };
        let mut removed = 0;
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => {
                    warn!("列出 {} 時發生錯誤: {}", dir.display(), e);
                    break;
                }
            };
            let name = entry.file_name();
            let Some(date) = name.to_str().and_then(|n| self.parse(n)) else {
                continue;
            };
            if !should_delete(date, today, retention_days)
                || !entry.file_type().await.is_ok_and(|t| t.is_file())
            {
                continue;
            }
            let path = entry.path();
            match fs::remove_file(&path).await {
                Ok(()) => {
                    removed += 1;
                    info!(
                        "已刪除過期的{} {}（保留 {} 天）",
                        self.label,
                        path.display(),
                        retention_days
                    );
                }
                Err(e) => warn!("無法刪除過期的{} {}: {}", self.label, path.display(), e),
            }
        }
        removed
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const REPORTS: DatedFiles = DatedFiles {
        prefix: "reports-",
        suffix: ".jsonl",
        label: "report 檔",
    };
    const LOGS: DatedFiles = DatedFiles {
        prefix: "kairos-",
        suffix: ".log",
        label: "日誌檔",
    };

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kairos-retention-{}-{}-{}",
            tag,
            std::process::id(),
            crate::registry::next_conn_id()
        ))
    }

    #[test]
    fn file_names() {
        assert_eq!(
            REPORTS.file_name(date(2026, 10, 9)),
            "reports-2026-10-09.jsonl"
        );
        assert_eq!(
            REPORTS.file_name(date(999, 1, 2)),
            "reports-0999-01-02.jsonl"
        );
        assert_eq!(LOGS.file_name(date(2026, 1, 2)), "kairos-2026-01-02.log");
    }

    #[test]
    fn parsing_is_strict() {
        assert_eq!(
            REPORTS.parse("reports-2026-10-09.jsonl"),
            Some(date(2026, 10, 9))
        );
        assert_eq!(
            REPORTS.parse("reports-2024-02-29.jsonl"),
            Some(date(2024, 2, 29))
        );
        assert_eq!(LOGS.parse("kairos-2026-10-09.log"), Some(date(2026, 10, 9)));
        let d = date(2026, 12, 31);
        assert_eq!(REPORTS.parse(&REPORTS.file_name(d)), Some(d));
        assert_eq!(LOGS.parse(&LOGS.file_name(d)), Some(d));

        for name in [
            "reports.jsonl",
            "reports-2026-10-09.jsonl.bak",
            "reports-2026-10-09.json",
            "reports-2026-10-09.JSONL",
            "Reports-2026-10-09.jsonl",
            "xreports-2026-10-09.jsonl",
            "reports-2026-1-09.jsonl",
            "reports-2026-10-9.jsonl",
            "reports-20261009.jsonl",
            "reports-2026_10_09.jsonl",
            "reports-2026-13-01.jsonl",
            "reports-2026-02-30.jsonl",
            "reports-2025-02-29.jsonl",
            "reports- 2026-10-09.jsonl",
            "reports-+026-10-09.jsonl",
            "reports-2026-10-09 .jsonl",
            "reports-２026-10-09.jsonl",
            "reports-.jsonl",
            "kairos-2026-10-09.log", // 另一類檔案
            "agents.json",
            "",
        ] {
            assert_eq!(REPORTS.parse(name), None, "不應被解析: {name:?}");
        }
        for name in [
            "kairos.log",
            "kairos-2026-10-09.log.1",
            "kairos-2026-10-09.txt",
            "kairos-2026-10-9.log",
            "Kairos-2026-10-09.log",
            "reports-2026-10-09.jsonl",
        ] {
            assert_eq!(LOGS.parse(name), None, "不應被解析: {name:?}");
        }
    }

    #[test]
    fn retention_boundaries() {
        let today = date(2026, 10, 9);
        assert!(!should_delete(today, today, 30));
        assert!(!should_delete(date(2026, 9, 9), today, 30)); // 剛好 30 天前 → 保留
        assert!(should_delete(date(2026, 9, 8), today, 30)); // 31 天前 → 刪除
        assert!(should_delete(date(2000, 1, 1), today, 30));
        assert!(!should_delete(date(2026, 10, 10), today, 30)); // 未來（時鐘回撥）
        assert!(!should_delete(date(2000, 1, 1), today, 0)); // 0 = 永久保留
        assert!(!should_delete(date(2026, 10, 8), today, 1));
        assert!(should_delete(date(2026, 10, 7), today, 1));
        assert!(!should_delete(date(2025, 12, 31), date(2026, 1, 30), 30)); // 跨年
        assert!(should_delete(date(2025, 12, 30), date(2026, 1, 30), 30));
        // 3650 天：期間含 2020、2024 兩個閏日
        assert!(!should_delete(date(2016, 10, 11), today, 3650));
        assert!(should_delete(date(2016, 10, 10), today, 3650));
        // 日誌預設 14 天
        assert!(!should_delete(date(2026, 9, 25), today, 14));
        assert!(should_delete(date(2026, 9, 24), today, 14));
    }

    #[tokio::test]
    async fn cleanup_deletes_only_expired_well_formed_files() {
        let dir = temp_dir("cleanup");
        fs::create_dir_all(&dir).await.unwrap();
        let names = [
            "kairos-2026-10-09.log",
            "kairos-2026-09-25.log", // 剛好 14 天前 → 保留
            "kairos-2026-09-24.log", // 15 天前 → 刪除
            "kairos-2020-01-01.log", // 刪除
            "kairos.log",            // 舊版 → 不碰
            "kairos-2020-01-01.log.bak",
            "reports-2020-01-01.jsonl", // 另一類 → 不碰
            "notes.txt",
        ];
        for n in names {
            fs::write(dir.join(n), b"x\n").await.unwrap();
        }
        fs::create_dir(dir.join("kairos-2019-01-01.log"))
            .await
            .unwrap();

        assert_eq!(LOGS.cleanup(&dir, date(2026, 10, 9), 14).await, 2);
        for n in names {
            let exists = fs::metadata(dir.join(n)).await.is_ok();
            let deleted = n == "kairos-2026-09-24.log" || n == "kairos-2020-01-01.log";
            assert_eq!(exists, !deleted, "{n}");
        }
        assert!(
            fs::metadata(dir.join("kairos-2019-01-01.log"))
                .await
                .is_ok()
        );
        assert_eq!(LOGS.cleanup(&dir, date(2030, 1, 1), 0).await, 0);
        assert_eq!(
            LOGS.cleanup(&dir.join("missing"), date(2030, 1, 1), 1)
                .await,
            0
        );
        fs::remove_dir_all(&dir).await.unwrap();
    }
}
