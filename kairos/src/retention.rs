//! 依日期命名的檔案（`<prefix>YYYY-MM-DD<suffix>`）的檔名規則與保留清除，以及清除前的日期合理性檢查。
//!
//! reports（`reports-YYYY-MM-DD.jsonl`）與日誌（`kairos-YYYY-MM-DD.log`）共用；日期一律為 UTC。

use chrono::{DateTime, NaiveDate, Utc};
use date_guard::{DatedName, GuardEvent, LegacyState};
use log::{info, warn};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{Error, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::fs;

// 日期合理性檢查的規則與狀態在 Kairos 與 Pulse 共用的 date-guard crate；
// 這裡只負責狀態檔的讀寫（含舊檔名的沿用）與日誌措辭。
pub use date_guard::{JUMP_ACCEPT_AFTER, ROLLBACK_ACCEPT_AFTER, should_delete};

/// report 的 [`DateGuard`] 狀態檔（放在 `storage.data_dir`）。
/// 檔名不符合任何 `<prefix>YYYY-MM-DD<suffix>`，不會被清除。
pub const GUARD_STATE_FILE: &str = ".retention-guard.json";

/// 日誌的 [`DateGuard`] 狀態檔（放在 `logs/`）。與 report 的檔名不同，
/// `storage.data_dir` 設為日誌目錄時兩者也不會共用狀態（R2-K18）。
/// 舊版放在 `logs/` 的 [`GUARD_STATE_FILE`] 只在新檔不存在時讀取一次。
pub const LOG_GUARD_STATE_FILE: &str = ".retention-guard-logs.json";

/// 保留清除前的日期合理性檢查（R2-K4～K7）。規則見 [`date_guard::DateGuard`]（Pulse 的日誌清除
/// 使用同一份實作）；本型別加上狀態檔的保存（每次檢查都在 blocking 執行緒原子寫入）與日誌。
/// 接受舊版（只有 `accepted` 與 `jump_since_unix`）的狀態檔格式。
#[derive(Debug)]
pub struct DateGuard {
    /// 日誌中稱呼這類檔案的名稱
    label: &'static str,
    core: date_guard::DateGuard,
    /// 狀態檔路徑；`None` 表示不保存
    state_path: Option<PathBuf>,
    /// 上次寫入狀態檔是否失敗（只在狀態改變時記錄，避免洗版）
    save_failed: bool,
}

impl DateGuard {
    /// 不保存狀態的 DateGuard，以 `baseline` 與現在為基準點
    pub fn new(
        label: &'static str,
        baseline: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Self {
        Self {
            label,
            core: date_guard::DateGuard::new(baseline, now, now_wall),
            state_path: None,
            save_failed: false,
        }
    }

    /// 從狀態檔載入（不存在時改讀 `legacy_path`）；都不存在或損毀時以 `fallback` 與現在為基準點
    /// （並在第一次檢查時寫出狀態檔）。同步讀檔，只在啟動時呼叫一次。
    pub fn load_or_new(
        label: &'static str,
        state_path: PathBuf,
        legacy_path: Option<&Path>,
        fallback: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Self {
        let mut guard = Self::new(label, fallback, now, now_wall);
        let (bytes, from) = match std::fs::read(&state_path) {
            Ok(bytes) => (Some(bytes), state_path.as_path()),
            Err(_) => match legacy_path.map(|p| (std::fs::read(p), p)) {
                Some((Ok(bytes), p)) => (Some(bytes), p),
                _ => (None, state_path.as_path()),
            },
        };
        if let Some(bytes) = bytes {
            match date_guard::DateGuard::from_state(&bytes, LegacyState::Accept, now, now_wall) {
                Some(core) => guard.core = core,
                None => warn!(
                    "{} 內容無效，改以 {} 作為清除過期的{}的日期基準",
                    from.display(),
                    fallback,
                    label
                ),
            }
        }
        guard.state_path = Some(state_path);
        guard
    }

    /// 以時鐘的「今天」判斷能否清除；可以時回傳用來清除的日期。
    /// 狀態改變時在 blocking 執行緒寫入狀態檔（含 fsync，不佔用 async worker；R2-K18）。
    /// 每次檢查都存檔（正常為每小時一次），讓狀態檔中基準點的經過時間保持最新。
    pub async fn check(
        &mut self,
        today: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Option<NaiveDate> {
        let decision = self.core.decide(today, now, now_wall);
        for event in &decision.events {
            self.log_event(event);
        }
        if self.state_path.is_some() {
            self.save(now, now_wall).await;
        }
        decision.cleanup_date
    }

    /// 記錄狀態變化（措辭與 A8 抽出共用 crate 之前相同）
    fn log_event(&self, event: &GuardEvent) {
        let hours = |d: Duration| d.as_secs() / 3600;
        match *event {
            GuardEvent::JumpCleared { today, .. } => {
                info!("系統日期已回到 {}，恢復清除過期的{}", today, self.label)
            }
            GuardEvent::RollbackStarted { today, accepted } => info!(
                "系統日期 {} 早於先前的 {}（時鐘回撥）；以較早的日期清除過期的{}，持續 {} 小時後改以此日期為基準",
                today,
                accepted,
                self.label,
                hours(ROLLBACK_ACCEPT_AFTER)
            ),
            GuardEvent::RollbackAccepted { today, .. } => info!(
                "系統日期回撥至 {} 已持續 {} 小時，改以此日期作為清除過期的{}的基準",
                today,
                hours(ROLLBACK_ACCEPT_AFTER),
                self.label
            ),
            // Kairos 不記錄回撥計時的取消（Pulse 記 debug）
            GuardEvent::RollbackCleared { .. } => {}
            GuardEvent::JumpStarted {
                today,
                allowed_max,
                accepted,
            } => warn!(
                "系統日期 {} 晚於依經過時間推算的最晚日期 {}（先前為 {}），可能是時鐘被誤校；暫停清除過期的{}以免誤刪。若日期正確，{} 小時後會恢復清除",
                today,
                allowed_max,
                accepted,
                self.label,
                hours(JUMP_ACCEPT_AFTER)
            ),
            GuardEvent::JumpRestarted { today, previous } => warn!(
                "暫停清除期間系統日期再往前跳到 {}（偵測到跳躍時為 {}），重新起算 {} 小時",
                today,
                previous,
                hours(JUMP_ACCEPT_AFTER)
            ),
            GuardEvent::JumpAccepted { today } => warn!(
                "系統日期 {} 已持續 {} 小時，接受為正確日期並恢復清除過期的{}",
                today,
                hours(JUMP_ACCEPT_AFTER),
                self.label
            ),
        }
    }

    /// 原子寫入狀態檔（暫存檔同步到磁碟後 rename）
    async fn save(&mut self, now: Instant, now_wall: DateTime<Utc>) {
        let Some(path) = self.state_path.clone() else {
            return;
        };
        let state = self.core.state(now, now_wall);
        let target = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let json = state.to_file_bytes().map_err(Error::other)?;
            if let Some(dir) = target.parent()
                && !dir.as_os_str().is_empty()
            {
                crate::fsperm::create_dir_all(dir)?;
            }
            write_atomically(&target, &[&json])
        })
        .await
        .unwrap_or_else(|e| Err(Error::other(e)));
        match result {
            Ok(()) => {
                if self.save_failed {
                    info!("{} 已恢復寫入", path.display());
                }
                self.save_failed = false;
            }
            Err(e) => {
                if !self.save_failed {
                    warn!(
                        "無法寫入 {}（重新啟動後的時鐘跳躍保護可能失效）: {}",
                        path.display(),
                        e
                    );
                }
                self.save_failed = true;
            }
        }
    }
}

/// 暫存檔名：`<檔名>.<隨機 16 位十六進位>.tmp`，放在同一個目錄（rename 才是原子的）
pub fn unique_tmp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{}.{:016x}.tmp", name, h.finish()))
}

/// 是否為 [`unique_tmp_path`] 為 `file_name` 產生的暫存檔名（或舊版固定的 `<檔名>.tmp`）
pub fn is_tmp_name_for(candidate: &str, file_name: &str) -> bool {
    let Some(rest) = candidate
        .strip_prefix(file_name)
        .and_then(|r| r.strip_prefix('.'))
    else {
        return false;
    };
    if rest == "tmp" {
        return true;
    }
    rest.strip_suffix(".tmp")
        .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// 原子寫入：以 `create_new` 建立隨機檔名的暫存檔（不會跟隨預先放置的符號連結或覆寫既有檔案；
/// R2-K9）、寫入並同步到磁碟後 rename 成 `path`。失敗時刪除暫存檔。同步 I/O，請在 blocking 執行緒呼叫。
///
/// Unix 上暫存檔以 mode 0640 建立（[`crate::fsperm::FILE_MODE`]，S3；實際權限再受 umask 限制），
/// rename 後的目標檔（`agents.json`、`.retention-guard*.json`）從一開始就不會比 0640 寬，
/// 不需事後收緊，也沒有暫存期間權限過寬的空窗。
pub fn write_atomically(path: &Path, parts: &[&[u8]]) -> std::io::Result<()> {
    let tmp = unique_tmp_path(path);
    let written = (|| {
        let mut file = crate::fsperm::private_std_options(
            std::fs::OpenOptions::new().write(true).create_new(true),
        )
        .open(&tmp)?;
        for part in parts {
            file.write_all(part)?;
        }
        file.sync_all()
    })()
    .map_err(|e| Error::new(e.kind(), format!("無法寫入 {}: {}", tmp.display(), e)));
    let result = written.and_then(|()| {
        std::fs::rename(&tmp, path).map_err(|e| {
            Error::new(
                e.kind(),
                format!("無法將 {} 更名為 {}: {}", tmp.display(), path.display(), e),
            )
        })
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 一類依日期命名的檔案
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatedFiles {
    pub prefix: &'static str,
    pub suffix: &'static str,
    /// 日誌中稱呼這類檔案的名稱
    pub label: &'static str,
}

impl DatedFiles {
    /// 檔名規則（與 Pulse 共用的實作，見 [`date_guard::DatedName`]）
    fn name(&self) -> DatedName {
        DatedName {
            prefix: self.prefix,
            suffix: self.suffix,
        }
    }

    /// 某日期的檔名
    pub fn file_name(&self, date: NaiveDate) -> String {
        self.name().file_name(date)
    }

    /// 只有**完全符合** `<prefix>YYYY-MM-DD<suffix>`（且為合法日期）才回傳日期
    pub fn parse(&self, name: &str) -> Option<NaiveDate> {
        self.name().parse(name)
    }

    /// `dir` 中檔名合法、日期不晚於 `not_after` 的檔案裡最新的日期（沒有時為 `None`）。
    /// 晚於 `not_after` 的檔案（時鐘誤校到未來時建立的）不列入。
    /// 同步讀取目錄，只在啟動時呼叫一次（作為 [`DateGuard`] 沒有狀態檔時的基準）。
    pub fn newest(&self, dir: &Path, not_after: NaiveDate) -> Option<NaiveDate> {
        self.name().newest(dir, not_after)
    }

    /// 啟動時 [`DateGuard`] 的基準：既有最新檔案的日期，但不晚於今天
    /// （檔案日期在未來時以今天為準，避免之後的時鐘跳躍被當成正常）；沒有檔案時為今天
    pub fn guard_baseline(&self, newest: Option<NaiveDate>, today: NaiveDate) -> NaiveDate {
        date_guard::guard_baseline(newest, today)
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Days;
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

    const H: Duration = Duration::from_secs(3600);

    fn wall(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn plus_days(d: NaiveDate, n: u64) -> NaiveDate {
        d.checked_add_days(Days::new(n)).unwrap()
    }

    /// 不保存狀態、以 2026-10-09 12:00 為基準點的 DateGuard
    fn guard(t0: Instant) -> DateGuard {
        DateGuard::new("report 檔", date(2026, 10, 9), t0, wall(2026, 10, 9, 12))
    }

    #[tokio::test]
    async fn date_guard_allows_normal_progress() {
        // 每小時檢查一次、連續 40 天，日期與實際經過的時間一致：每次都可清除
        let t0 = Instant::now();
        let mut g = guard(t0);
        let base = date(2026, 10, 9);
        for h in 0..(40 * 24u64) {
            let today = plus_days(base, (h + 12) / 24);
            let now = t0 + H * h as u32;
            assert_eq!(
                g.check(today, now, Utc::now()).await,
                Some(today),
                "第 {h} 小時"
            );
        }
        assert_eq!(g.core.accepted(), plus_days(base, (40 * 24 - 1 + 12) / 24));
        // 不超過 ceil(經過時間) + 1 天的跳動也接受（例如換日前後的檢查、時區設定變動）
        let mut g = guard(t0);
        assert_eq!(
            g.check(date(2026, 10, 10), t0, Utc::now()).await,
            Some(date(2026, 10, 10))
        );
        assert_eq!(
            g.check(date(2026, 10, 11), t0 + H, Utc::now()).await,
            Some(date(2026, 10, 11))
        );
        // 日期上限不會溢位
        let mut g = DateGuard::new("x", NaiveDate::MAX, t0, Utc::now());
        assert_eq!(
            g.check(NaiveDate::MAX, t0, Utc::now()).await,
            Some(NaiveDate::MAX)
        );
    }

    #[tokio::test]
    async fn gradual_forward_steps_cannot_outrun_elapsed_time() {
        // R2-K4：每小時把時鐘往前推 2 天（每次都在舊版的 2 天門檻內），15 小時後原本可推進 30 天
        let t0 = Instant::now();
        let mut g = guard(t0);
        let base = date(2026, 10, 9);
        let mut latest_cleanup = base;
        for h in 1..=15u32 {
            let today = plus_days(base, 2 * u64::from(h));
            if let Some(d) = g.check(today, t0 + H * h, Utc::now()).await {
                latest_cleanup = latest_cleanup.max(d);
            }
        }
        // 推進量不超過 ceil(經過時間) + 1 天
        assert!(latest_cleanup <= plus_days(base, 2), "{latest_cleanup}");
        assert!(g.core.accepted() <= plus_days(base, 2));
        // 持續推進時暫停一直重新起算，即使超過 24 小時也不接受
        for h in 16..=60u32 {
            let today = plus_days(base, 2 * u64::from(h));
            assert_eq!(g.check(today, t0 + H * h, Utc::now()).await, None, "{h}");
        }
        // 日期停止變動後持續 24 小時才接受
        // （第 60 小時剛重新起算過；停在比它晚 10 天的日期，第 61 小時再重新起算一次）
        let stopped = plus_days(base, 130);
        assert_eq!(g.check(stopped, t0 + H * 61, Utc::now()).await, None);
        assert_eq!(
            g.check(stopped, t0 + H * 61 + JUMP_ACCEPT_AFTER - H, Utc::now())
                .await,
            None
        );
        assert_eq!(
            g.check(stopped, t0 + H * 61 + JUMP_ACCEPT_AFTER, Utc::now())
                .await,
            Some(stopped)
        );
        // 接受後以新的日期為基準點：之後的正常推進照常接受
        let next = plus_days(stopped, 1);
        assert_eq!(
            g.check(next, t0 + H * 61 + JUMP_ACCEPT_AFTER + H, Utc::now())
                .await,
            Some(next)
        );
    }

    #[tokio::test]
    async fn date_guard_skips_forward_jump_until_it_persists() {
        let t0 = Instant::now();
        let mut g = guard(t0);
        // K-C2：誤校到未來 → 暫停清除
        assert_eq!(g.check(date(2027, 10, 9), t0, Utc::now()).await, None);
        assert_eq!(g.check(date(2027, 10, 9), t0 + H, Utc::now()).await, None);
        // 跳躍日期往回（但仍在合理範圍外）不會重新起算
        assert_eq!(
            g.check(date(2027, 10, 8), t0 + H * 2, Utc::now()).await,
            None
        );
        // 時鐘修正回來 → 恢復，且之後再跳仍會被擋
        assert_eq!(
            g.check(date(2026, 10, 9), t0 + H * 3, Utc::now()).await,
            Some(date(2026, 10, 9))
        );
        let t1 = t0 + H * 4;
        assert_eq!(g.check(date(2026, 10, 13), t1, Utc::now()).await, None);
        // 跳躍時的日期 + 2 天以內的變動繼續計時
        assert_eq!(g.check(date(2026, 10, 15), t1 + H, Utc::now()).await, None);
        // 新的日期持續 24 小時 → 接受
        assert_eq!(
            g.check(date(2026, 10, 15), t1 + JUMP_ACCEPT_AFTER, Utc::now())
                .await,
            Some(date(2026, 10, 15))
        );
        assert_eq!(
            g.check(date(2026, 10, 16), t1 + JUMP_ACCEPT_AFTER, Utc::now())
                .await,
            Some(date(2026, 10, 16))
        );
    }

    #[tokio::test]
    async fn rollback_lowers_accepted_date_after_24_hours() {
        // R2-K6：誤校日期被接受後時鐘改回，24 小時後把 accepted 降回目前日期，保護重新生效
        let t0 = Instant::now();
        let mut g = guard(t0);
        let future = date(2027, 10, 9);
        assert_eq!(g.check(future, t0, Utc::now()).await, None);
        assert_eq!(
            g.check(future, t0 + JUMP_ACCEPT_AFTER, Utc::now()).await,
            Some(future)
        );
        // 時鐘改回：以較早的日期清除（只會刪得更少）
        let t1 = t0 + JUMP_ACCEPT_AFTER + H;
        let real = date(2026, 10, 10);
        assert_eq!(g.check(real, t1, Utc::now()).await, Some(real));
        assert_eq!(g.core.accepted(), future);
        // 24 小時內回到未來日期：視為未變，照常
        assert_eq!(g.check(future, t1 + H, Utc::now()).await, Some(future));
        // 回撥計時重新開始，持續 24 小時後降回
        let t2 = t1 + H * 2;
        assert_eq!(g.check(real, t2, Utc::now()).await, Some(real));
        assert_eq!(
            g.check(real, t2 + ROLLBACK_ACCEPT_AFTER - H, Utc::now())
                .await,
            Some(real)
        );
        assert_eq!(g.core.accepted(), future);
        let t3 = t2 + ROLLBACK_ACCEPT_AFTER;
        let real = date(2026, 10, 11);
        assert_eq!(g.check(real, t3, Utc::now()).await, Some(real));
        assert_eq!(g.core.accepted(), real);
        // 之後再誤校到同一個未來日期 → 再次暫停
        assert_eq!(g.check(future, t3 + H, Utc::now()).await, None);
    }

    fn state_text(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn date_guard_state_survives_restart_during_clock_jump() {
        // K-C2：時鐘誤校到未來期間重新啟動，新實例從狀態檔讀回狀態，不以未來日期為基準清除
        let dir = temp_dir("guard-state");
        let path = dir.join(GUARD_STATE_FILE);
        let t0 = Instant::now();
        let load = |fallback, now, now_wall| {
            DateGuard::load_or_new("report 檔", path.clone(), None, fallback, now, now_wall)
        };

        let mut g = load(date(2026, 10, 8), t0, wall(2026, 10, 9, 12));
        assert_eq!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 12)).await,
            Some(date(2026, 10, 9))
        );
        // 跳到一年後
        assert_eq!(
            g.check(date(2027, 10, 9), t0, wall(2027, 10, 9, 12)).await,
            None
        );
        drop(g);
        assert_eq!(state_text(&path)["jump_date"], "2027-10-09");

        // 重新啟動：即使沒有狀態檔時的基準會是未來日期，仍以狀態檔為準
        let t1 = Instant::now();
        let mut g = load(date(2027, 10, 9), t1, wall(2027, 10, 9, 13));
        assert_eq!(
            g.check(date(2027, 10, 9), t1, wall(2027, 10, 9, 13)).await,
            None
        );
        drop(g);

        // 隔天重新啟動：停機時間最多算 1 小時，暫停仍持續；執行中再累積到 24 小時才接受
        let t2 = Instant::now();
        let mut g = load(date(2020, 1, 1), t2, wall(2027, 10, 10, 12));
        assert_eq!(
            g.check(date(2027, 10, 10), t2, wall(2027, 10, 10, 12))
                .await,
            None
        );
        // 已累積：重新啟動前 1 小時 + 本次停機 1 小時 = 2 小時；再 22 小時達到 24 小時
        assert_eq!(
            g.check(date(2027, 10, 10), t2 + H * 22 - H, wall(2027, 10, 11, 9))
                .await,
            None
        );
        assert_eq!(
            g.check(date(2027, 10, 11), t2 + H * 22, wall(2027, 10, 11, 10))
                .await,
            Some(date(2027, 10, 11))
        );
        drop(g);
        let state = state_text(&path);
        assert_eq!(state["accepted"], "2027-10-11");
        assert_eq!(state["anchor_date"], "2027-10-11");
        assert!(state["jump_since_unix"].is_null(), "{state}");
        assert!(state["jump_elapsed_secs"].is_null(), "{state}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn restart_with_a_further_jump_restarts_the_pause() {
        // R2-K5：暫停中重新啟動、且時鐘又往前跳超過 2 天：重新起算，而不是以牆鐘差立即接受
        let dir = temp_dir("guard-further");
        let path = dir.join(GUARD_STATE_FILE);
        let load = |now, now_wall| {
            DateGuard::load_or_new(
                "report 檔",
                path.clone(),
                None,
                date(2026, 10, 9),
                now,
                now_wall,
            )
        };
        let t0 = Instant::now();
        let mut g = load(t0, wall(2026, 10, 9, 12));
        assert_eq!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 12)).await,
            Some(date(2026, 10, 9))
        );
        // 跳到 2026-12-01 → 暫停
        assert_eq!(
            g.check(date(2026, 12, 1), t0, wall(2026, 12, 1, 0)).await,
            None
        );
        drop(g);

        // 重新啟動時時鐘又跳到一年後（牆鐘差約一年）：舊版會立即接受並清除
        let t1 = Instant::now();
        let mut g = load(t1, wall(2027, 12, 1, 0));
        assert_eq!(
            g.check(date(2027, 12, 1), t1, wall(2027, 12, 1, 0)).await,
            None
        );
        drop(g);
        assert_eq!(state_text(&path)["jump_date"], "2027-12-01");

        // 在新的日期停留：每次重新啟動停機時間最多算 1 小時，執行中累積到 24 小時後接受
        let t2 = Instant::now();
        let mut g = load(t2, wall(2027, 12, 1, 23));
        assert_eq!(
            g.check(date(2027, 12, 1), t2, wall(2027, 12, 1, 23)).await,
            None
        );
        drop(g);
        assert_eq!(state_text(&path)["jump_elapsed_secs"], 3600);
        let t3 = Instant::now();
        let mut g = load(t3, wall(2027, 12, 2, 0));
        assert_eq!(
            g.check(date(2027, 12, 2), t3, wall(2027, 12, 2, 0)).await,
            None
        );
        assert_eq!(
            g.check(date(2027, 12, 2), t3 + H * 22, wall(2027, 12, 2, 22))
                .await,
            Some(date(2027, 12, 2))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn adjusting_the_clock_while_stopped_does_not_end_the_pause() {
        // R2-K5：暫停中停機，停機期間把時鐘調快 2 天以內（不會重新起算）再啟動。
        // 若以牆鐘差延續（上限 24 小時 = 接受門檻），重新啟動後會立刻接受並清除
        let dir = temp_dir("guard-stopped-adjust");
        let path = dir.join(GUARD_STATE_FILE);
        let load = |now, now_wall| {
            DateGuard::load_or_new(
                "report 檔",
                path.clone(),
                None,
                date(2026, 10, 9),
                now,
                now_wall,
            )
        };
        let t0 = Instant::now();
        let mut g = load(t0, wall(2026, 10, 9, 12));
        assert!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 12))
                .await
                .is_some()
        );
        // 時鐘跳到 2027-01-01 → 暫停（執行中只累積 10 分鐘）
        assert_eq!(
            g.check(date(2027, 1, 1), t0, wall(2027, 1, 1, 0)).await,
            None
        );
        let ten_min = Duration::from_secs(600);
        assert_eq!(
            g.check(
                date(2027, 1, 1),
                t0 + ten_min,
                wall(2027, 1, 1, 0) + ten_min
            )
            .await,
            None
        );
        drop(g);
        // 停機期間把時鐘調快 2 天再啟動（牆鐘差約 2 天）
        for h in [0u32, 1, 2] {
            let t1 = Instant::now();
            let mut g = load(t1, wall(2027, 1, 3, h));
            assert_eq!(
                g.check(date(2027, 1, 3), t1, wall(2027, 1, 3, h)).await,
                None,
                "第 {h} 次重新啟動不應接受"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn future_start_times_in_state_are_rewritten_on_load() {
        // R2-K7：狀態檔中的起點在未來（牆鐘往回撥）時改寫為現在並存檔，
        // 否則每次重新啟動計時都歸零，每日排程重啟會讓清除無限期暫停
        let dir = temp_dir("guard-future-since");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(GUARD_STATE_FILE);
        let now_wall = wall(2027, 1, 1, 0);
        let future = (now_wall + chrono::Duration::days(10)).timestamp();
        std::fs::write(
            &path,
            format!(
                r#"{{"accepted":"2026-10-09","anchor_date":"2026-10-09","anchor_elapsed_secs":0,
                    "saved_unix":{future},"jump_since_unix":{future},"jump_date":"2027-01-01",
                    "jump_elapsed_secs":0,"rollback_since_unix":null}}"#
            ),
        )
        .unwrap();
        let load = |now, now_wall| {
            DateGuard::load_or_new(
                "report 檔",
                path.clone(),
                None,
                date(2026, 10, 9),
                now,
                now_wall,
            )
        };
        let t0 = Instant::now();
        let mut g = load(t0, now_wall);
        assert_eq!(g.check(date(2027, 1, 1), t0, now_wall).await, None);
        drop(g);
        let state = state_text(&path);
        assert_eq!(state["jump_since_unix"], now_wall.timestamp());
        assert_eq!(state["saved_unix"], now_wall.timestamp());

        // 之後的重新啟動從改寫後的存檔時間延續（停機最多算 1 小時），執行中累積到 24 小時後接受
        let t1 = Instant::now();
        let later = now_wall + chrono::Duration::hours(24);
        let mut g = load(t1, later);
        assert_eq!(g.check(date(2027, 1, 2), t1, later).await, None);
        assert_eq!(
            g.check(date(2027, 1, 2), t1 + H * 23, later).await,
            Some(date(2027, 1, 2))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn anchor_and_rollback_persist_across_restart() {
        let dir = temp_dir("guard-anchor");
        let path = dir.join(GUARD_STATE_FILE);
        let load = |now, now_wall| {
            DateGuard::load_or_new(
                "report 檔",
                path.clone(),
                None,
                date(2026, 10, 9),
                now,
                now_wall,
            )
        };
        let t0 = Instant::now();
        let mut g = load(t0, wall(2026, 10, 9, 12));
        assert!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 12))
                .await
                .is_some()
        );
        drop(g);
        // 停機 1 天後重新啟動：以牆鐘差計算經過時間，正常接受
        let t1 = Instant::now();
        let mut g = load(t1, wall(2026, 10, 10, 12));
        assert_eq!(
            g.check(date(2026, 10, 10), t1, wall(2026, 10, 10, 12))
                .await,
            Some(date(2026, 10, 10))
        );
        drop(g);
        // 再停機 3 天：延續量上限 24 小時 → 視為跳躍，暫停 24 小時（第一輪起即有的行為）
        let t1 = Instant::now();
        let mut g = load(t1, wall(2026, 10, 13, 12));
        assert_eq!(
            g.check(date(2026, 10, 13), t1, wall(2026, 10, 13, 12))
                .await,
            None
        );
        assert_eq!(
            g.check(
                date(2026, 10, 14),
                t1 + JUMP_ACCEPT_AFTER,
                wall(2026, 10, 14, 12)
            )
            .await,
            Some(date(2026, 10, 14))
        );
        let t1 = t1 + JUMP_ACCEPT_AFTER;
        // 時鐘回撥：回撥計時寫入狀態檔
        assert_eq!(
            g.check(date(2026, 1, 1), t1 + H, wall(2026, 1, 1, 0)).await,
            Some(date(2026, 1, 1))
        );
        drop(g);
        assert!(state_text(&path)["rollback_since_unix"].is_i64());
        // 重新啟動後延續回撥計時：牆鐘差達 24 小時 → 降回
        let t2 = Instant::now();
        let mut g = load(t2, wall(2026, 1, 2, 0));
        assert_eq!(
            g.check(date(2026, 1, 2), t2, wall(2026, 1, 2, 0)).await,
            Some(date(2026, 1, 2))
        );
        assert_eq!(g.core.accepted(), date(2026, 1, 2));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn missing_or_corrupt_state_falls_back_to_baseline() {
        let dir = temp_dir("guard-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(GUARD_STATE_FILE);
        let t0 = Instant::now();
        let load = || {
            DateGuard::load_or_new(
                "x",
                path.clone(),
                None,
                date(2026, 10, 8),
                t0,
                wall(2026, 10, 9, 0),
            )
        };
        // 不存在：以基準為準，第一次檢查時寫出狀態檔
        let mut g = load();
        assert_eq!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 0)).await,
            Some(date(2026, 10, 9))
        );
        assert!(path.exists());
        for bad in [
            "not json",
            r#"{"accepted":"2026-13-01","jump_since_unix":null}"#,
            r#"{"accepted":"2026-10-08","anchor_date":"x","anchor_elapsed_secs":1,"saved_unix":1}"#,
            r#"{"accepted":"2026-10-08","anchor_date":"2026-10-08"}"#,
            "{}",
        ] {
            std::fs::write(&path, bad).unwrap();
            let mut g = load();
            // 退回基準：跳到未來仍會被擋
            assert_eq!(
                g.check(date(2027, 1, 1), t0, wall(2027, 1, 1, 0)).await,
                None,
                "{bad}"
            );
            assert_eq!(
                g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 0)).await,
                Some(date(2026, 10, 9))
            );
        }
        // 狀態檔已改寫為有效內容
        assert_eq!(state_text(&path)["accepted"], "2026-10-09");

        // 舊版格式（只有 accepted 與 jump_since_unix）仍可讀取，並補上新欄位
        std::fs::write(&path, r#"{"accepted":"2026-10-09","jump_since_unix":null}"#).unwrap();
        let mut g = load();
        assert_eq!(g.core.accepted(), date(2026, 10, 9));
        assert_eq!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 0)).await,
            Some(date(2026, 10, 9))
        );
        assert_eq!(state_text(&path)["anchor_date"], "2026-10-09");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn legacy_state_file_is_read_when_the_new_one_is_missing() {
        // R2-K18：日誌改用自己的狀態檔名；升級時沿用舊檔（不修改舊檔）
        let dir = temp_dir("guard-legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join(GUARD_STATE_FILE);
        let path = dir.join(LOG_GUARD_STATE_FILE);
        let old = r#"{"accepted":"2026-10-09","jump_since_unix":null}"#;
        std::fs::write(&legacy, old).unwrap();
        let t0 = Instant::now();
        let mut g = DateGuard::load_or_new(
            "日誌檔",
            path.clone(),
            Some(&legacy),
            date(2020, 1, 1),
            t0,
            wall(2026, 10, 9, 0),
        );
        // 以舊檔的日期為準（不是 fallback 的 2020-01-01）：跳到一年後被擋
        assert_eq!(
            g.check(date(2027, 10, 9), t0, wall(2027, 10, 9, 0)).await,
            None
        );
        assert_eq!(state_text(&path)["accepted"], "2026-10-09");
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), old);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_write_uses_unique_temp_files() {
        // R2-K9：暫存檔為隨機檔名並以 create_new 建立；預先放置的固定檔名不會被使用或覆寫
        let dir = temp_dir("atomic");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("agents.json");
        let decoy = dir.join("agents.json.tmp");
        std::fs::write(&decoy, b"decoy").unwrap();
        write_atomically(&target, &[b"{\"a\":1}", b"\n"]).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"{\"a\":1}\n");
        assert_eq!(std::fs::read(&decoy).unwrap(), b"decoy");
        // 寫完後只剩目標檔與誘餌檔
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");

        let a = unique_tmp_path(&target);
        let b = unique_tmp_path(&target);
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_str().unwrap();
        assert!(is_tmp_name_for(name, "agents.json"), "{name}");
        assert!(is_tmp_name_for("agents.json.tmp", "agents.json"));
        assert!(!is_tmp_name_for("agents.json", "agents.json"));
        assert!(!is_tmp_name_for("agents.json.xyz.tmp", "agents.json"));
        assert!(!is_tmp_name_for(
            "other.json.0123456789abcdef.tmp",
            "agents.json"
        ));

        // 目標目錄不存在時回錯誤，不留下暫存檔
        assert!(write_atomically(&dir.join("missing").join("x.json"), &[b"x"]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_files_with_at_most_0640() {
        // S3：暫存檔以 0640 建立，rename 後的檔案一開始就不比 0640 寬；
        // 取代既有的寬鬆檔案時也不會沿用舊權限
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("atomic-mode");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("agents.json");
        std::fs::write(&target, b"old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o666)).unwrap();
        write_atomically(&target, &[b"{}"]).unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & !crate::fsperm::FILE_MODE, 0, "mode {mode:o}");
        // 擁有者仍可讀寫
        assert_eq!(mode & 0o600, 0o600, "mode {mode:o}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn guard_state_file_is_created_with_at_most_0640() {
        // S3：.retention-guard*.json 由 write_atomically 產生，同樣不比 0640 寬；
        // 狀態檔所在目錄不存在時以 0750 建立
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("guard-mode");
        let state_dir = dir.join("data");
        let path = state_dir.join(GUARD_STATE_FILE);
        let t0 = Instant::now();
        let now_wall = wall(2026, 10, 9, 12);
        let mut g = DateGuard::load_or_new(
            "report 檔",
            path.clone(),
            None,
            date(2026, 10, 9),
            t0,
            now_wall,
        );
        g.check(date(2026, 10, 9), t0, now_wall).await;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & !crate::fsperm::FILE_MODE, 0, "mode {mode:o}");
        let dir_mode = std::fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode & !crate::fsperm::DIR_MODE, 0, "mode {dir_mode:o}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn newest_file_date() {
        let dir = temp_dir("newest");
        let today = date(2026, 10, 9);
        assert_eq!(REPORTS.newest(&dir, today), None);
        fs::create_dir_all(&dir).await.unwrap();
        assert_eq!(REPORTS.newest(&dir, today), None);
        for n in [
            "reports-2026-10-01.jsonl",
            "reports-2026-10-08.jsonl",
            "reports-2027-01-01.jsonl", // 晚於今天：不列入
            "reports-2030-13-01.jsonl",
            "kairos-2031-01-01.log",
        ] {
            fs::write(dir.join(n), b"x\n").await.unwrap();
        }
        assert_eq!(REPORTS.newest(&dir, today), Some(date(2026, 10, 8)));
        assert_eq!(
            REPORTS.newest(&dir, date(2027, 6, 1)),
            Some(date(2027, 1, 1))
        );
        // 基準不晚於今天
        assert_eq!(
            REPORTS.guard_baseline(Some(date(2026, 10, 8)), today),
            date(2026, 10, 8)
        );
        assert_eq!(REPORTS.guard_baseline(Some(date(2030, 1, 1)), today), today);
        assert_eq!(REPORTS.guard_baseline(None, today), today);
        fs::remove_dir_all(&dir).await.unwrap();
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
