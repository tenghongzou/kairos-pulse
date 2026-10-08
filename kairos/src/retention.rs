//! 依日期命名的檔案（`<prefix>YYYY-MM-DD<suffix>`）的檔名規則與保留清除。
//!
//! reports（`reports-YYYY-MM-DD.jsonl`）與日誌（`kairos-YYYY-MM-DD.log`）共用；日期一律為 UTC。

use chrono::{DateTime, Days, NaiveDate, Utc};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::fs;

/// 用來清除檔案的「今天」比上次接受的日期晚超過這麼多天時，視為系統時鐘往前跳
pub const MAX_FORWARD_JUMP_DAYS: u64 = 2;

/// 偵測到時鐘往前跳後，新的日期持續這麼久才接受並恢復清除
pub const JUMP_ACCEPT_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// [`DateGuard`] 狀態檔的檔名（放在被清除的目錄中：`data/` 與 `logs/` 各一份）。
/// 檔名不符合任何 `<prefix>YYYY-MM-DD<suffix>`，不會被清除。
pub const GUARD_STATE_FILE: &str = ".retention-guard.json";

/// 狀態檔內容
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GuardState {
    /// 最近一次接受的日期（YYYY-MM-DD）
    accepted: String,
    /// 偵測到時鐘跳躍的牆鐘時間（Unix 秒）；沒有跳躍時為 `null`
    jump_since_unix: Option<i64>,
}

/// 偵測中的時鐘跳躍
#[derive(Debug, Clone, Copy)]
struct Jump {
    /// 本次執行中開始計時的時間（單調時鐘）
    since: Instant,
    /// 先前的執行已經過的時間（從狀態檔讀回時以牆鐘差計算）
    carried: Duration,
    /// 偵測到跳躍的牆鐘時間（寫入狀態檔用）
    since_wall: DateTime<Utc>,
}

impl Jump {
    fn elapsed(&self, now: Instant) -> Duration {
        self.carried
            .saturating_add(now.saturating_duration_since(self.since))
    }
}

/// 保留清除前的日期合理性檢查。
///
/// 系統時鐘被誤校到未來時，以「今天」計算的保留期限會把所有檔案判定為過期而刪光。
/// 這裡記住最近一次接受的日期，新的日期比它晚超過 [`MAX_FORWARD_JUMP_DAYS`] 天時記 WARN 並
/// 暫停清除；新的日期持續 [`JUMP_ACCEPT_AFTER`] 後（例如 VM 暫停數天後恢復，時間其實是正確的）
/// 才接受並恢復清除。時鐘往回撥不受限制：以較早的日期清除只會刪得更少。
///
/// 狀態（已接受的日期、跳躍開始的時間）會原子寫入狀態檔 [`GUARD_STATE_FILE`]，重新啟動時讀回：
/// 時鐘誤校期間已建立的未來日期檔案，不會讓新的實例以未來日期為基準而立即清除。
/// 跳躍的持續時間在同一次執行內以單調時鐘計算，跨越重新啟動時以牆鐘差延續。
#[derive(Debug)]
pub struct DateGuard {
    /// 日誌中稱呼這類檔案的名稱
    label: &'static str,
    /// 最近一次接受的日期
    accepted: NaiveDate,
    /// 偵測中的跳躍
    jump: Option<Jump>,
    /// 狀態檔路徑；`None` 表示不保存
    state_path: Option<PathBuf>,
    /// 尚未寫入狀態檔的變更
    dirty: bool,
    /// 上次寫入狀態檔是否失敗（只在狀態改變時記錄，避免洗版）
    save_failed: bool,
}

impl DateGuard {
    /// 不保存狀態的 DateGuard
    pub fn new(label: &'static str, baseline: NaiveDate) -> Self {
        Self {
            label,
            accepted: baseline,
            jump: None,
            state_path: None,
            dirty: false,
            save_failed: false,
        }
    }

    /// 從狀態檔載入；狀態檔不存在或損毀時以 `fallback` 為基準（並在第一次檢查時寫出狀態檔）
    pub fn load_or_new(
        label: &'static str,
        state_path: PathBuf,
        fallback: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Self {
        let mut guard = Self::new(label, fallback);
        match std::fs::read(&state_path) {
            Ok(bytes) => match parse_state(&bytes, now, now_wall) {
                Some((accepted, jump)) => {
                    guard.accepted = accepted;
                    guard.jump = jump;
                }
                None => {
                    warn!(
                        "{} 內容無效，改以 {} 作為清除過期的{}的日期基準",
                        state_path.display(),
                        fallback,
                        label
                    );
                    guard.dirty = true;
                }
            },
            Err(_) => guard.dirty = true,
        }
        guard.state_path = Some(state_path);
        guard
    }

    /// 以時鐘的「今天」判斷能否清除；可以時回傳用來清除的日期。狀態改變時寫入狀態檔。
    pub fn check(
        &mut self,
        today: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Option<NaiveDate> {
        let result = self.decide(today, now, now_wall);
        if self.dirty {
            self.save();
        }
        result
    }

    fn decide(
        &mut self,
        today: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Option<NaiveDate> {
        let limit = self
            .accepted
            .checked_add_days(Days::new(MAX_FORWARD_JUMP_DAYS))
            .unwrap_or(NaiveDate::MAX);
        if today <= limit {
            if self.jump.take().is_some() {
                info!("系統日期已回到 {}，恢復清除過期的{}", today, self.label);
                self.dirty = true;
            }
            if today > self.accepted {
                self.accepted = today;
                self.dirty = true;
            }
            return Some(today);
        }
        match self.jump {
            None => {
                warn!(
                    "系統日期 {} 比先前的 {} 晚超過 {} 天，可能是時鐘被誤校；暫停清除過期的{}以免誤刪。若日期正確，{} 小時後會恢復清除",
                    today,
                    self.accepted,
                    MAX_FORWARD_JUMP_DAYS,
                    self.label,
                    JUMP_ACCEPT_AFTER.as_secs() / 3600
                );
                self.jump = Some(Jump {
                    since: now,
                    carried: Duration::ZERO,
                    since_wall: now_wall,
                });
                self.dirty = true;
                None
            }
            Some(jump) if jump.elapsed(now) >= JUMP_ACCEPT_AFTER => {
                warn!(
                    "系統日期 {} 已持續 {} 小時，接受為正確日期並恢復清除過期的{}",
                    today,
                    JUMP_ACCEPT_AFTER.as_secs() / 3600,
                    self.label
                );
                self.jump = None;
                self.accepted = today;
                self.dirty = true;
                Some(today)
            }
            Some(_) => None,
        }
    }

    fn state(&self) -> GuardState {
        GuardState {
            accepted: self.accepted.format("%Y-%m-%d").to_string(),
            jump_since_unix: self.jump.map(|j| j.since_wall.timestamp()),
        }
    }

    /// 原子寫入狀態檔（先寫暫存檔並同步到磁碟，再 rename）。檔案很小，且只在狀態改變時寫入
    /// （正常情況每天一次），因此直接同步寫入。
    fn save(&mut self) {
        let Some(path) = &self.state_path else {
            self.dirty = false;
            return;
        };
        match write_state_atomically(path, &self.state()) {
            Ok(()) => {
                if self.save_failed {
                    info!("{} 已恢復寫入", path.display());
                }
                self.save_failed = false;
                self.dirty = false;
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

/// 解析狀態檔；格式不符時回傳 `None`
fn parse_state(
    bytes: &[u8],
    now: Instant,
    now_wall: DateTime<Utc>,
) -> Option<(NaiveDate, Option<Jump>)> {
    let state: GuardState = serde_json::from_slice(bytes).ok()?;
    let accepted = NaiveDate::parse_from_str(&state.accepted, "%Y-%m-%d").ok()?;
    let jump = match state.jump_since_unix {
        None => None,
        Some(unix) => {
            let since_wall = DateTime::<Utc>::from_timestamp(unix, 0)?;
            // 牆鐘往回撥（since 在未來）時視為剛開始
            let carried = (now_wall - since_wall).to_std().unwrap_or(Duration::ZERO);
            Some(Jump {
                since: now,
                carried,
                since_wall,
            })
        }
    };
    Some((accepted, jump))
}

fn write_state_atomically(path: &Path, state: &GuardState) -> std::io::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&json)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
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

    /// `dir` 中檔名合法、日期不晚於 `not_after` 的檔案裡最新的日期（沒有時為 `None`）。
    /// 晚於 `not_after` 的檔案（時鐘誤校到未來時建立的）不列入。
    /// 同步讀取目錄，只在啟動時呼叫一次（作為 [`DateGuard`] 沒有狀態檔時的基準）。
    pub fn newest(&self, dir: &Path, not_after: NaiveDate) -> Option<NaiveDate> {
        std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().and_then(|n| self.parse(n)))
            .filter(|date| *date <= not_after)
            .max()
    }

    /// 啟動時 [`DateGuard`] 的基準：既有最新檔案的日期，但不晚於今天
    /// （檔案日期在未來時以今天為準，避免之後的時鐘跳躍被當成正常）；沒有檔案時為今天
    pub fn guard_baseline(&self, newest: Option<NaiveDate>, today: NaiveDate) -> NaiveDate {
        newest.map_or(today, |newest| newest.min(today))
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
    fn date_guard_allows_normal_progress_and_clock_going_back() {
        let t0 = Instant::now();
        let mut g = DateGuard::new("report 檔", date(2026, 10, 9));
        let check = |g: &mut DateGuard, d, t| g.check(d, t, Utc::now());
        assert_eq!(
            check(&mut g, date(2026, 10, 9), t0),
            Some(date(2026, 10, 9))
        );
        assert_eq!(
            check(&mut g, date(2026, 10, 10), t0),
            Some(date(2026, 10, 10))
        );
        // 剛好 2 天內仍接受
        assert_eq!(
            check(&mut g, date(2026, 10, 12), t0),
            Some(date(2026, 10, 12))
        );
        // 往回撥：以較早的日期清除（只會刪得更少），基準不後退
        assert_eq!(check(&mut g, date(2026, 1, 1), t0), Some(date(2026, 1, 1)));
        assert_eq!(
            check(&mut g, date(2026, 10, 14), t0),
            Some(date(2026, 10, 14))
        );
    }

    #[test]
    fn date_guard_skips_forward_jump_until_it_persists() {
        let t0 = Instant::now();
        let check = |g: &mut DateGuard, d, t| g.check(d, t, Utc::now());
        let mut g = DateGuard::new("report 檔", date(2026, 10, 9));
        // K-C2：誤校到未來（超過 2 天）→ 暫停清除
        assert_eq!(check(&mut g, date(2027, 10, 9), t0), None);
        assert_eq!(
            check(&mut g, date(2027, 10, 9), t0 + Duration::from_secs(3600)),
            None
        );
        // 時鐘修正回來 → 恢復，且之後再跳仍會被擋
        assert_eq!(
            check(&mut g, date(2026, 10, 9), t0 + Duration::from_secs(7200)),
            Some(date(2026, 10, 9))
        );
        let t1 = t0 + Duration::from_secs(10_000);
        assert_eq!(check(&mut g, date(2026, 10, 13), t1), None);
        // 新的日期持續 24 小時 → 接受
        assert_eq!(
            check(&mut g, date(2026, 10, 13), t1 + JUMP_ACCEPT_AFTER),
            Some(date(2026, 10, 13))
        );
        assert_eq!(
            check(&mut g, date(2026, 10, 14), t1 + JUMP_ACCEPT_AFTER),
            Some(date(2026, 10, 14))
        );
        // 日期上限不會溢位
        let mut g = DateGuard::new("x", NaiveDate::MAX);
        assert_eq!(check(&mut g, NaiveDate::MAX, t0), Some(NaiveDate::MAX));
    }

    fn wall(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[test]
    fn date_guard_state_survives_restart_during_clock_jump() {
        // K-C2：時鐘誤校到未來期間重新啟動，新實例從狀態檔讀回狀態，不以未來日期為基準清除
        let dir = temp_dir("guard-state");
        let path = dir.join(GUARD_STATE_FILE);
        let t0 = Instant::now();

        let mut g = DateGuard::load_or_new(
            "report 檔",
            path.clone(),
            date(2026, 10, 8),
            t0,
            wall(2026, 10, 9, 12),
        );
        assert_eq!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 12)),
            Some(date(2026, 10, 9))
        );
        // 跳到一年後
        assert_eq!(g.check(date(2027, 10, 9), t0, wall(2027, 10, 9, 12)), None);
        drop(g);

        // 重新啟動：即使沒有狀態檔時的基準會是未來日期（例如已建立 reports-2027-10-09.jsonl），仍以狀態檔為準
        let t1 = Instant::now();
        let mut g = DateGuard::load_or_new(
            "report 檔",
            path.clone(),
            date(2027, 10, 9),
            t1,
            wall(2027, 10, 9, 13),
        );
        assert_eq!(g.check(date(2027, 10, 9), t1, wall(2027, 10, 9, 13)), None);
        drop(g);

        // 跳躍已持續 24 小時（跨越重新啟動，以牆鐘差計算）→ 接受
        let t2 = Instant::now();
        let mut g = DateGuard::load_or_new(
            "report 檔",
            path.clone(),
            date(2020, 1, 1),
            t2,
            wall(2027, 10, 10, 12),
        );
        assert_eq!(
            g.check(date(2027, 10, 10), t2, wall(2027, 10, 10, 12)),
            Some(date(2027, 10, 10))
        );
        drop(g);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("2027-10-10"), "{text}");
        assert!(text.contains("\"jump_since_unix\": null"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_or_corrupt_state_falls_back_to_baseline() {
        let dir = temp_dir("guard-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(GUARD_STATE_FILE);
        let t0 = Instant::now();
        // 不存在：以基準為準，第一次檢查時寫出狀態檔
        let mut g = DateGuard::load_or_new(
            "x",
            path.clone(),
            date(2026, 10, 8),
            t0,
            wall(2026, 10, 9, 0),
        );
        assert_eq!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 0)),
            Some(date(2026, 10, 9))
        );
        assert!(path.exists());
        for bad in [
            "not json",
            r#"{"accepted":"2026-13-01","jump_since_unix":null}"#,
            "{}",
        ] {
            std::fs::write(&path, bad).unwrap();
            let mut g = DateGuard::load_or_new(
                "x",
                path.clone(),
                date(2026, 10, 8),
                t0,
                wall(2026, 10, 9, 0),
            );
            // 退回基準：跳到未來仍會被擋
            assert_eq!(
                g.check(date(2027, 1, 1), t0, wall(2027, 1, 1, 0)),
                None,
                "{bad}"
            );
            assert_eq!(
                g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 0)),
                Some(date(2026, 10, 9))
            );
        }
        // 狀態檔已改寫為有效內容
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("2026-10-09"), "{text}");
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
