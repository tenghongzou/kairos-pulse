//! 日誌檔按日分檔（`logs/pulse-YYYY-MM-DD.log`）與保留清除。
//!
//! 檔名日期一律為 **UTC**（與 Kairos 相同），每行開頭的時間戳記則為本機時間。
//! 清除只刪除檔名**完全符合** `pulse-YYYY-MM-DD.log`（且為合法日期）的一般檔案，
//! 其他檔案一律不碰。
//!
//! ## 時鐘跳躍保護（[`DateGuard`]，與 Kairos 共用 `date-guard` crate 的實作）
//!
//! 系統時鐘被誤校到未來時，以「今天」計算的保留期限會把所有日誌檔判定為過期而刪光。
//! 因此清除前先檢查日期是否合理：
//!
//! - 接受的日期（accepted）只能隨實際經過的時間推進：允許的最晚日期 = 基準日期 + 自基準點以來
//!   經過的時間換算天數（無條件進位）+ 1 天。經過時間在同一次執行內用單調時鐘；跨重新啟動時為
//!   「存檔時已經過的時間 + min(max(0, 現在 − 存檔時間), 24 小時)」（狀態每次檢查都存檔）。
//!   基準點只在啟動時沒有狀態、接受跳躍後的日期、回撥後降回 accepted 時設定。
//! - 超過允許範圍 → 記 WARN 並暫停清除（跳躍狀態）；新的日期持續 24 小時後才接受並恢復清除。
//!   跳躍暫停跨重新啟動時延續「存檔時已累積的暫停時間 + min(停機時間, 1 小時)」；
//!   回撥計時延續 min(max(0, 現在 − 起點), 24 小時)，牆鐘差為負時起點改寫為現在。
//!   暫停期間日期又比「跳躍時看到的日期」再往前超過 2 天 → 重新起算 24 小時。
//! - 日期回撥（今天早於 accepted）時照常以較早的日期清除（只會刪得更少）；
//!   持續 24 小時後把 accepted 降回今天，之後再往前跳仍會被偵測。
//! - 狀態存在日誌目錄的 `.retention-guard.json`（原子寫入），重新啟動後延續；
//!   狀態檔不存在或損毀時，以「不晚於今天的最新日誌檔日期」為基準。`retention_days = 0` 時不檢查。

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{DateTime, NaiveDate, Utc};
use date_guard::{DatedName, GuardEvent, GuardState, LegacyState, should_delete};

/// 日誌目錄（相對於目前工作目錄）。
pub const LOG_DIR: &str = "logs";
const PREFIX: &str = "pulse-";
const SUFFIX: &str = ".log";
/// 日誌檔名規則（與 Kairos 共用的實作）
const LOG_NAMES: DatedName = DatedName {
    prefix: PREFIX,
    suffix: SUFFIX,
};

/// `log.format = "json"` 的日誌檔：`logs/pulse-YYYY-MM-DD.jsonl`（兩種格式不混在同一個檔案）
const LOG_NAMES_JSON: DatedName = DatedName {
    prefix: PREFIX,
    suffix: ".jsonl",
};

/// 以附加模式開啟日誌檔
fn open_append(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().create(true).append(true).open(path)
}

/// 按日分檔的日誌輸出（UTC 日期；副檔名依格式：`.log` 或 `.jsonl`）
pub fn daily_file(format: kp_obs::logfmt::LogFormat) -> kp_obs::logfmt::DailyFile {
    kp_obs::logfmt::DailyFile::new(
        PathBuf::from(LOG_DIR),
        PREFIX,
        format.file_suffix(),
        open_append,
        Utc::now,
    )
}

/// 已初始化的日誌輸出（`/metrics` 的 syslog 計數、結束前送完 syslog 佇列、JSON 的 agent_id）
static SINK: std::sync::OnceLock<std::sync::Arc<kp_obs::logfmt::LogSink>> =
    std::sync::OnceLock::new();

/// 記下已初始化的日誌輸出
pub fn set_sink(sink: std::sync::Arc<kp_obs::logfmt::LogSink>) {
    let _ = SINK.set(sink);
}

/// 已初始化的日誌輸出（初始化日誌之前為 `None`）
pub fn sink() -> Option<&'static std::sync::Arc<kp_obs::logfmt::LogSink>> {
    SINK.get()
}

/// 結束前：寫出緩衝並等待 syslog 佇列送完（最多 2 秒）
pub fn flush_all() {
    log::logger().flush();
    if let Some(s) = sink() {
        s.flush();
        if let Some(syslog) = &s.syslog {
            syslog.flush(std::time::Duration::from_secs(2));
        }
    }
}

/// [`DateGuard`] 狀態檔的檔名（放在日誌目錄中）；不符合日誌檔名，不會被清除。
pub const GUARD_STATE_FILE: &str = ".retention-guard.json";

/// 偵測到時鐘跳躍（或日期回撥）後，新的日期持續這麼久才接受（規則見 [`date_guard::DateGuard`]）。
pub use date_guard::JUMP_ACCEPT_AFTER;

/// 某日期的日誌檔名。
#[cfg(test)]
fn file_name(date: NaiveDate) -> String {
    LOG_NAMES.file_name(date)
}

/// 只有完全符合 `pulse-YYYY-MM-DD.log` 或 `.jsonl`（且為合法日期）才回傳日期。
pub fn parse(name: &str) -> Option<NaiveDate> {
    LOG_NAMES.parse(name).or_else(|| LOG_NAMES_JSON.parse(name))
}

/// `dir` 中檔名合法、日期不晚於 `not_after` 的日誌檔裡最新的日期（沒有時為 `None`）。
/// 晚於 `not_after` 的檔案（時鐘誤校到未來時建立的）不列入。
fn newest_log_date(dir: &Path, not_after: NaiveDate) -> Option<NaiveDate> {
    LOG_NAMES
        .newest(dir, not_after)
        .max(LOG_NAMES_JSON.newest(dir, not_after))
}

/// 日誌檔保留清除（含時鐘跳躍保護）。
///
/// 阻塞的檔案 I/O；在 async 環境中請用 `spawn_blocking` 呼叫 [`LogRetention::run`]。
#[derive(Debug)]
pub struct LogRetention {
    dir: PathBuf,
    retention_days: u32,
    /// `retention_days == 0` 時為 `None`（永久保留，不檢查日期）
    guard: Option<DateGuard>,
    /// 開啟時產生、尚未寫進日誌的訊息（開啟時日誌可能還沒初始化）
    pending_logs: Vec<(log::Level, String)>,
}

/// 是否為狀態檔的暫存檔名：`.retention-guard.json.<32 位十六進位>.tmp`（或固定的 `.retention-guard.json.tmp`）。
fn is_guard_tmp_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(GUARD_STATE_FILE)
        .and_then(|r| r.strip_prefix('.'))
    else {
        return false;
    };
    if rest == "tmp" {
        return true;
    }
    rest.strip_suffix(".tmp")
        .is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// 刪除 `dir` 中殘留的狀態檔暫存檔（寫入中途結束時留下的），回傳要記錄的訊息。
fn remove_stale_tmp_files(dir: &Path) -> Vec<(log::Level, String)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut logs = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_guard_tmp_name)
            || !entry.file_type().is_ok_and(|t| t.is_file())
        {
            continue;
        }
        let path = entry.path();
        logs.push(match fs::remove_file(&path) {
            Ok(()) => (
                log::Level::Info,
                format!("已刪除殘留的暫存檔 {}", path.display()),
            ),
            Err(e) => (
                log::Level::Warn,
                format!("無法刪除殘留的暫存檔 {}：{e}", path.display()),
            ),
        });
    }
    logs
}

impl LogRetention {
    /// 讀取狀態檔並決定日期基準。
    ///
    /// **必須在初始化日誌之前呼叫**：fern 一初始化就會建立今天的日誌檔，時鐘被誤校到未來時，
    /// 那個檔案不能被當成「最新日誌檔的日期」。同時刪除殘留的狀態檔暫存檔。
    /// 此函式不寫日誌，訊息會在第一次 [`run`](Self::run) 時輸出。
    pub fn open(dir: &Path, retention_days: u32) -> Self {
        Self::open_at(
            dir,
            retention_days,
            Utc::now().date_naive(),
            Instant::now(),
            Utc::now(),
        )
    }

    fn open_at(
        dir: &Path,
        retention_days: u32,
        today: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Self {
        let mut pending_logs = remove_stale_tmp_files(dir);
        let guard = (retention_days > 0).then(|| {
            let fallback = newest_log_date(dir, today).unwrap_or(today);
            let (guard, warning) =
                DateGuard::load_or_new(dir.join(GUARD_STATE_FILE), fallback, now, now_wall);
            pending_logs.extend(warning.map(|w| (log::Level::Warn, w)));
            guard
        });
        Self {
            dir: dir.to_path_buf(),
            retention_days,
            guard,
            pending_logs,
        }
    }

    /// 以目前時間檢查日期並清除過期的日誌檔，回傳刪除數。
    pub fn run(&mut self) -> usize {
        self.run_at(Utc::now().date_naive(), Instant::now(), Utc::now())
    }

    fn run_at(&mut self, today: NaiveDate, now: Instant, now_wall: DateTime<Utc>) -> usize {
        for (level, msg) in self.pending_logs.drain(..) {
            log::log!(level, "{msg}");
        }
        let Some(guard) = &mut self.guard else {
            return 0;
        };
        match guard.check(today, now, now_wall) {
            Some(date) => cleanup(&self.dir, date, self.retention_days),
            None => 0,
        }
    }
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

// ---- 時鐘跳躍保護 ----

/// 保留清除前的日期合理性檢查（規則見模組說明與 [`date_guard::DateGuard`]，與 Kairos 共用實作）。
/// 本型別加上狀態檔的保存（每次檢查都同步原子寫入）與日誌；不接受 Kairos 舊版的狀態檔格式。
#[derive(Debug)]
struct DateGuard {
    core: date_guard::DateGuard,
    state_path: PathBuf,
    /// 上次寫入狀態檔是否失敗（只在狀態改變時記錄，避免洗版）
    save_failed: bool,
}

impl DateGuard {
    /// 以 `baseline` 為基準建立（不讀狀態檔）；第一次檢查時寫出狀態檔。
    fn new(
        state_path: PathBuf,
        baseline: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Self {
        Self {
            core: date_guard::DateGuard::new(baseline, now, now_wall),
            state_path,
            save_failed: false,
        }
    }

    /// 從狀態檔載入；不存在或損毀時以 `fallback` 為基準。損毀時一併回傳要記錄的警告。
    fn load_or_new(
        state_path: PathBuf,
        fallback: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> (Self, Option<String>) {
        let bytes = match fs::read(&state_path) {
            Ok(bytes) => bytes,
            Err(_) => return (Self::new(state_path, fallback, now, now_wall), None),
        };
        match date_guard::DateGuard::from_state(&bytes, LegacyState::Reject, now, now_wall) {
            Some(core) => (
                Self {
                    core,
                    state_path,
                    save_failed: false,
                },
                None,
            ),
            None => {
                let warning = format!(
                    "{} 內容無效，改以 {fallback} 作為清除過期日誌檔的日期基準",
                    state_path.display()
                );
                (
                    Self::new(state_path, fallback, now, now_wall),
                    Some(warning),
                )
            }
        }
    }

    /// 以時鐘的「今天」判斷能否清除；可以時回傳用來清除的日期。每次檢查都寫入狀態檔
    /// （記錄存檔時間與自基準點已經過的時間，供重新啟動後延續）。
    fn check(
        &mut self,
        today: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Option<NaiveDate> {
        let decision = self.core.decide(today, now, now_wall);
        for event in &decision.events {
            log_event(event);
        }
        self.save(now, now_wall);
        decision.cleanup_date
    }

    /// 寫入狀態檔。失敗只在第一次記 warn，恢復時記 info。
    fn save(&mut self, now: Instant, now_wall: DateTime<Utc>) {
        match write_state_atomically(&self.state_path, &self.core.state(now, now_wall)) {
            Ok(()) => {
                if self.save_failed {
                    log::info!("{} 已恢復寫入", self.state_path.display());
                }
                self.save_failed = false;
            }
            Err(e) => {
                if !self.save_failed {
                    log::warn!(
                        "無法寫入 {}（重新啟動後的時鐘跳躍保護可能失效）：{e}",
                        self.state_path.display()
                    );
                }
                self.save_failed = true;
            }
        }
    }
}

/// 記錄日期檢查的狀態變化（措辭與 A8 抽出共用 crate 之前相同）。
fn log_event(event: &GuardEvent) {
    let hours = JUMP_ACCEPT_AFTER.as_secs() / 3600;
    match *event {
        // 因日期回撥而結束暫停時不另外記錄（回撥本身會記錄）
        GuardEvent::JumpCleared {
            during_rollback: true,
            ..
        } => {}
        GuardEvent::JumpCleared { today, .. } => {
            log::info!("系統日期 {today} 已回到合理範圍，恢復清除過期的日誌檔");
        }
        GuardEvent::RollbackStarted { today, accepted } => log::info!(
            "系統日期 {today} 早於先前接受的 {accepted}（時鐘往回撥）；若持續 {hours} 小時，將改以目前日期作為清除日誌檔的基準"
        ),
        GuardEvent::RollbackAccepted { today, previous } => log::info!(
            "系統日期 {today} 早於先前接受的 {previous} 已持續 {hours} 小時，改以 {today} 作為清除日誌檔的基準"
        ),
        GuardEvent::RollbackCleared { accepted } => {
            log::debug!("系統日期已回到 {accepted} 之後");
        }
        GuardEvent::JumpStarted {
            today, accepted, ..
        } => log::warn!(
            "{}系統日期 {today} 比先前的 {accepted} 前進得比實際經過的時間多，可能是時鐘被誤校；暫停清除過期的日誌檔以免誤刪。\
             若日期正確，{hours} 小時後會恢復清除",
            crate::codes::tag(
                crate::codes::LOG_RETENTION_PAUSED,
                Some(crate::codes::W_LOG_RETENTION_PAUSED)
            )
        ),
        GuardEvent::JumpRestarted { today, previous } => log::warn!(
            "{}暫停清除日誌檔期間，系統日期又從 {previous} 往前跳到 {today}，重新起算 {hours} 小時",
            crate::codes::tag(
                crate::codes::LOG_RETENTION_PAUSED,
                Some(crate::codes::W_LOG_RETENTION_PAUSED)
            )
        ),
        GuardEvent::JumpAccepted { today } => {
            log::warn!("系統日期 {today} 已持續 {hours} 小時，接受為正確日期並恢復清除過期的日誌檔")
        }
    }
}

/// 原子寫入狀態檔：同目錄的暫存檔以 `create_new` + 隨機檔名建立（不會跟隨預先放置的符號連結、
/// 也不會覆寫既有檔案），寫入並 `sync_all` 後再 rename 取代；失敗時刪除暫存檔。
fn write_state_atomically(path: &Path, state: &GuardState) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let mut json = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    json.push(b'\n');
    let tmp = dir.join(format!(
        "{GUARD_STATE_FILE}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const HOUR: Duration = Duration::from_secs(3600);
    const DAY: Duration = Duration::from_secs(24 * 3600);

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn wall(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("pulse-log-files-tests")
            .join(format!("{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut left: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        left
    }

    fn read_state(dir: &Path) -> GuardState {
        serde_json::from_slice(&fs::read(dir.join(GUARD_STATE_FILE)).unwrap()).unwrap()
    }

    /// 測試用時鐘：單調時間與牆鐘一起前進，牆鐘可另外調整（模擬誤校）。
    struct Clock {
        t0: Instant,
        mono: Duration,
        wall: DateTime<Utc>,
    }

    impl Clock {
        fn new(wall_start: &str) -> Self {
            Self {
                t0: Instant::now(),
                mono: Duration::ZERO,
                wall: wall(wall_start),
            }
        }

        fn advance(&mut self, d: Duration) {
            self.mono += d;
            self.wall += chrono::Duration::from_std(d).unwrap();
        }

        fn set_wall(&mut self, s: &str) {
            self.wall = wall(s);
        }

        fn now(&self) -> Instant {
            self.t0 + self.mono
        }

        fn today(&self) -> NaiveDate {
            self.wall.date_naive()
        }

        fn check(&self, g: &mut DateGuard) -> Option<NaiveDate> {
            g.check(self.today(), self.now(), self.wall)
        }
    }

    fn guard_at(dir: &Path, clock: &Clock) -> DateGuard {
        DateGuard::load_or_new(
            dir.join(GUARD_STATE_FILE),
            clock.today(),
            clock.now(),
            clock.wall,
        )
        .0
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
            GUARD_STATE_FILE,
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
        let dir = temp_dir("cleanup");
        for name in [
            "pulse-2026-10-09.log",
            "pulse-2026-09-25.log",
            "pulse-2026-09-24.log",
            "pulse-2020-01-01.log",
            "pulse.log",
            "notes-2020-01-01.log",
            "pulse-2020-01-01.log.bak",
            GUARD_STATE_FILE,
        ] {
            fs::write(dir.join(name), "x").unwrap();
        }
        // 符合檔名的目錄不刪
        fs::create_dir(dir.join("pulse-2019-01-01.log")).unwrap();

        assert_eq!(cleanup(&dir, d("2026-10-09"), 14), 2);
        assert_eq!(
            names(&dir),
            [
                GUARD_STATE_FILE,
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

    #[test]
    fn normal_day_changes_are_accepted() {
        let dir = temp_dir("normal");
        let mut c = Clock::new("2026-10-09T22:00:00Z");
        let mut g = guard_at(&dir, &c);
        assert_eq!(c.check(&mut g), Some(d("2026-10-09")));
        // 每小時檢查一次，連續 10 天
        for _ in 0..240 {
            c.advance(HOUR);
            assert_eq!(c.check(&mut g), Some(c.today()));
        }
        assert_eq!(g.core.accepted(), d("2026-10-19"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stepwise_forward_jumps_are_detected() {
        // R2-K4：每小時往前調 2 天（每次都不超過 2 天）也不能逐步推進 accepted
        let dir = temp_dir("stepwise");
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        assert_eq!(c.check(&mut g), Some(d("2026-10-09")));
        let mut accepted_dates = Vec::new();
        for _ in 0..15 {
            c.advance(HOUR);
            let w = c.wall + chrono::Duration::days(2);
            c.wall = w;
            if let Some(date) = c.check(&mut g) {
                accepted_dates.push(date);
            }
        }
        // 最多只有基準 + 1 小時（無條件進位 1 天）+ 1 天的寬容，其後全部暫停
        assert!(
            accepted_dates.iter().all(|&date| date <= d("2026-10-11")),
            "{accepted_dates:?}"
        );
        assert!(
            g.core.accepted() <= d("2026-10-11"),
            "{}",
            g.core.accepted()
        );
        assert!(g.core.jump_pending());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jump_is_accepted_after_24_hours() {
        let dir = temp_dir("jump");
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        c.set_wall("2026-11-08T10:00:00Z");
        assert_eq!(c.check(&mut g), None, "往前跳 30 天：暫停");
        let state = read_state(&dir);
        assert_eq!(state.jump_date.as_deref(), Some("2026-11-08"));
        c.advance(DAY - Duration::from_secs(1));
        assert_eq!(c.check(&mut g), None);
        c.advance(Duration::from_secs(1));
        assert_eq!(c.check(&mut g), Some(d("2026-11-09")));
        assert_eq!(g.core.accepted(), d("2026-11-09"));
        assert!(read_state(&dir).jump_since_unix.is_none());
        // 接受後重設基準：之後的正常推進照常接受
        c.advance(DAY);
        assert_eq!(c.check(&mut g), Some(d("2026-11-10")));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jump_timer_restarts_when_date_moves_further() {
        let dir = temp_dir("restart");
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        c.set_wall("2026-11-08T10:00:00Z");
        assert_eq!(c.check(&mut g), None);
        // 暫停中再往前 2 天以內：不重新起算
        c.advance(12 * HOUR);
        c.set_wall("2026-11-10T10:00:00Z");
        assert_eq!(c.check(&mut g), None);
        assert_eq!(g.core.jump_date().unwrap(), d("2026-11-08"));
        // 比跳躍時看到的日期再往前超過 2 天：重新起算 24 小時
        c.advance(HOUR);
        c.set_wall("2026-11-11T10:00:00Z");
        assert_eq!(c.check(&mut g), None);
        assert_eq!(g.core.jump_date().unwrap(), d("2026-11-11"));
        c.advance(23 * HOUR);
        assert_eq!(c.check(&mut g), None, "從重新起算時只過了 23 小時");
        c.advance(HOUR);
        assert_eq!(c.check(&mut g), Some(c.today()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn returning_to_a_plausible_date_ends_the_pause() {
        let dir = temp_dir("return");
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        c.set_wall("2026-11-08T10:00:00Z");
        assert_eq!(c.check(&mut g), None);
        c.advance(HOUR);
        c.set_wall("2026-10-09T11:00:00Z");
        assert_eq!(c.check(&mut g), Some(d("2026-10-09")));
        assert!(!g.core.jump_pending());
        assert_eq!(g.core.accepted(), d("2026-10-09"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rollback_lowers_accepted_after_24_hours() {
        // R2-K6：誤校日期被接受後時鐘改回，持續 24 小時後 accepted 降回，保護恢復
        let dir = temp_dir("rollback");
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        c.set_wall("2026-11-08T10:00:00Z");
        c.check(&mut g);
        c.advance(DAY);
        assert_eq!(c.check(&mut g), Some(d("2026-11-09")), "誤校的日期被接受");
        // 時鐘改回正確日期：照常以較早的日期清除
        c.set_wall("2026-10-10T10:00:00Z");
        assert_eq!(c.check(&mut g), Some(d("2026-10-10")));
        assert_eq!(g.core.accepted(), d("2026-11-09"));
        assert!(read_state(&dir).rollback_since_unix.is_some());
        c.advance(DAY - Duration::from_secs(1));
        c.check(&mut g);
        assert_eq!(g.core.accepted(), d("2026-11-09"));
        c.advance(Duration::from_secs(1));
        assert_eq!(c.check(&mut g), Some(d("2026-10-11")));
        assert_eq!(g.core.accepted(), d("2026-10-11"));
        assert!(read_state(&dir).rollback_since_unix.is_none());
        // 之後再往前跳到原本誤校的日期：仍會被偵測
        c.advance(HOUR);
        c.set_wall("2026-11-09T12:00:00Z");
        assert_eq!(c.check(&mut g), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn brief_rollback_is_forgotten() {
        let dir = temp_dir("brief");
        let mut c = Clock::new("2026-10-09T00:00:30Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        c.set_wall("2026-10-08T23:59:50Z");
        assert_eq!(c.check(&mut g), Some(d("2026-10-08")));
        assert!(g.core.rollback_pending());
        c.advance(Duration::from_secs(20));
        assert_eq!(c.check(&mut g), Some(d("2026-10-09")));
        assert!(!g.core.rollback_pending());
        assert_eq!(g.core.accepted(), d("2026-10-09"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn state_survives_restart_and_jump_timer_carries_over() {
        let dir = temp_dir("persist");
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        c.set_wall("2026-11-08T10:00:00Z");
        assert_eq!(c.check(&mut g), None);
        // 暫停中執行 20 小時（每次檢查都存下已累積的時間）
        c.advance(20 * HOUR);
        assert_eq!(c.check(&mut g), None);
        assert_eq!(read_state(&dir).jump_elapsed_secs, Some(20 * 3600));
        drop(g);
        // 30 分鐘後重新啟動：跳躍狀態延續（20 小時 + 停機 30 分鐘），不會以未來日期為基準立即清除
        c.advance(HOUR / 2);
        let mut g = guard_at(&dir, &c);
        assert_eq!(g.core.accepted(), d("2026-10-09"));
        assert_eq!(c.check(&mut g), None);
        c.advance(3 * HOUR);
        assert_eq!(c.check(&mut g), None, "累計 23.5 小時");
        c.advance(HOUR / 2);
        assert_eq!(c.check(&mut g), Some(d("2026-11-09")));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jump_downtime_counts_at_most_one_hour() {
        // 跳躍暫停中停機，把時鐘調快 1.9 天（不觸發重新起算）再啟動 → 仍暫停
        let dir = temp_dir("jump-downtime");
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        c.set_wall("2026-11-08T10:00:00Z");
        assert_eq!(c.check(&mut g), None);
        c.advance(2 * HOUR);
        assert_eq!(c.check(&mut g), None);
        drop(g);
        c.set_wall("2026-11-10T07:36:00Z"); // 約 1.9 天後，日期仍在 jump_date + 2 天內
        let mut g = guard_at(&dir, &c);
        assert_eq!(c.check(&mut g), None, "停機期間最多只算 1 小時");
        assert_eq!(read_state(&dir).jump_elapsed_secs, Some(3 * 3600));
        assert_eq!(read_state(&dir).jump_date.unwrap(), "2026-11-08");
        c.advance(20 * HOUR);
        assert_eq!(c.check(&mut g), None, "累計 23 小時，仍暫停");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restart_with_clock_moved_further_restarts_the_timer() {
        // R2-K5：重啟時時鐘再往前跳，不會因牆鐘差立即被接受
        let dir = temp_dir("restart-jump");
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        c.set_wall("2026-11-08T10:00:00Z");
        assert_eq!(c.check(&mut g), None);
        drop(g);
        c.set_wall("2026-12-08T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        assert_eq!(
            c.check(&mut g),
            None,
            "牆鐘差 30 天，但日期也比跳躍時看到的再晚 30 天"
        );
        assert_eq!(read_state(&dir).jump_date.unwrap(), "2026-12-08");
        c.advance(DAY - Duration::from_secs(1));
        assert_eq!(c.check(&mut g), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn carried_time_across_restart_is_capped() {
        let dir = temp_dir("cap");
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        drop(g);
        // 停機 10 天後重新啟動：跨重啟的經過時間上限 24 小時 → 允許到 +2 天，其餘暫停 24 小時
        c.advance(10 * DAY);
        let mut g = guard_at(&dir, &c);
        assert_eq!(c.check(&mut g), None);
        c.advance(DAY);
        assert_eq!(c.check(&mut g), Some(d("2026-10-20")));
        drop(g);
        // 停機 20 小時：日期前進 1 天，照常接受
        c.advance(20 * HOUR);
        let mut g = guard_at(&dir, &c);
        assert_eq!(c.check(&mut g), Some(d("2026-10-21")));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jump_start_in_the_future_is_rewritten_on_restart() {
        // R2-K7：跳躍起點在未來（牆鐘往回撥）時改寫為現在並存檔，排程重啟不會讓計時永遠歸零
        let dir = temp_dir("future-since");
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        c.set_wall("2026-11-08T10:00:00Z");
        assert_eq!(c.check(&mut g), None);
        drop(g);
        // 牆鐘往回撥到跳躍起點之前，但日期仍在允許範圍外
        let mut state = read_state(&dir);
        state.jump_since_unix = Some(wall("2026-11-08T12:00:00Z").timestamp());
        fs::write(
            dir.join(GUARD_STATE_FILE),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let mut g = guard_at(&dir, &c);
        assert_eq!(c.check(&mut g), None);
        let since = read_state(&dir).jump_since_unix.unwrap();
        assert_eq!(since, c.wall.timestamp());
        // 之後每次重新啟動都延續已累積的時間（排程重啟不會讓計時歸零）
        for _ in 0..4 {
            c.advance(6 * HOUR);
            let _ = c.check(&mut g);
            drop(g);
            g = guard_at(&dir, &c);
        }
        assert_eq!(c.check(&mut g), Some(c.today()), "累計 24 小時後接受");
        assert!(!g.core.jump_pending());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn first_start_with_log_two_days_old_does_not_pause() {
        // 沒有狀態檔、最新日誌檔是 2 天前：經過時間只有幾毫秒，以奈秒無條件進位為 1 天 → 不暫停
        let dir = temp_dir("first-start");
        fs::write(dir.join("pulse-2026-10-07.log"), "x").unwrap();
        fs::write(dir.join("pulse-2026-09-01.log"), "x").unwrap();
        let today = d("2026-10-09");
        let now = Instant::now();
        let w = wall("2026-10-09T10:00:00Z");
        let mut r = LogRetention::open_at(&dir, 14, today, now, w);
        assert_eq!(r.guard.as_ref().unwrap().core.accepted(), d("2026-10-07"));
        let later = now + Duration::from_millis(5);
        assert_eq!(r.run_at(today, later, w), 1, "照常清除 09-01");
        assert!(!r.guard.as_ref().unwrap().core.jump_pending());
        assert_eq!(r.guard.as_ref().unwrap().core.accepted(), today);
        // 最新日誌檔是 3 天前：超出範圍，暫停
        let dir3 = temp_dir("first-start-3");
        fs::write(dir3.join("pulse-2026-10-06.log"), "x").unwrap();
        let mut r = LogRetention::open_at(&dir3, 14, today, now, w);
        assert_eq!(r.run_at(today, later, w), 0);
        assert!(r.guard.as_ref().unwrap().core.jump_pending());
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(dir3).unwrap();
    }

    #[test]
    fn stale_state_tmp_files_are_removed_at_startup() {
        let dir = temp_dir("stale-tmp");
        let stale = [
            format!(
                "{GUARD_STATE_FILE}.{}.tmp",
                "0123456789abcdef0123456789abcdef"
            ),
            format!("{GUARD_STATE_FILE}.tmp"),
        ];
        let keep = [
            format!("{GUARD_STATE_FILE}.xyz.tmp"),
            format!("{GUARD_STATE_FILE}.{}.tmp", "0123"),
            "pulse-2026-10-09.log".to_string(),
            "other.json.tmp".to_string(),
        ];
        for name in stale.iter().chain(&keep) {
            fs::write(dir.join(name), "x").unwrap();
        }
        let r = LogRetention::open_at(
            &dir,
            0,
            d("2026-10-09"),
            Instant::now(),
            wall("2026-10-09T10:00:00Z"),
        );
        let mut expected: Vec<String> = keep.to_vec();
        expected.sort();
        assert_eq!(names(&dir), expected);
        assert_eq!(r.pending_logs.len(), 2, "{:?}", r.pending_logs);
        assert!(r.pending_logs.iter().all(|(l, _)| *l == log::Level::Info));
        // 實際寫入時產生的暫存檔名也會被辨識
        assert!(is_guard_tmp_name(&format!(
            "{GUARD_STATE_FILE}.{}.tmp",
            uuid::Uuid::new_v4().simple()
        )));
        assert!(!is_guard_tmp_name(GUARD_STATE_FILE));
        fs::remove_dir_all(dir).unwrap();
    }

    /// 以每天一次的檢查正常執行 `days` 天（基準點不動，accepted 持續推進）。
    fn run_daily(c: &mut Clock, g: &mut DateGuard, days: u32) {
        for _ in 0..days {
            c.advance(DAY);
            assert_eq!(c.check(g), Some(c.today()));
        }
    }

    #[test]
    fn jump_detected_before_restart_is_not_accepted_after_restart() {
        // 基準點可能好幾個月都不動；跨重啟若以「現在 − 基準點的牆鐘時間」計算經過時間，
        // 重啟前已偵測到的跳躍會在重啟後被當成經過時間而接受
        let dir = temp_dir("old-anchor-jump");
        let mut c = Clock::new("2026-01-01T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        assert_eq!(c.check(&mut g), Some(d("2026-01-01")));
        run_daily(&mut c, &mut g, 90);
        assert_eq!(
            g.core.anchor_date(),
            d("2026-01-01"),
            "正常推進不移動基準點"
        );
        // 時鐘往前跳 30 天：暫停
        c.set_wall("2026-05-31T10:00:00Z");
        assert_eq!(c.check(&mut g), None);
        let state = read_state(&dir);
        assert_eq!(state.anchor_date.as_deref(), Some("2026-01-01"));
        assert_eq!(state.anchor_elapsed_secs, Some(90 * 24 * 3600));
        drop(g);
        // 1 小時後重新啟動：仍在暫停中（現在 − 基準點牆鐘 ≈ 150 天，但不可據此接受）
        c.advance(HOUR);
        let mut g = guard_at(&dir, &c);
        assert_eq!(c.check(&mut g), None);
        c.advance(22 * HOUR);
        assert_eq!(c.check(&mut g), None, "暫停只經過 23 小時");
        c.advance(HOUR);
        assert_eq!(
            c.check(&mut g),
            Some(d("2026-06-01")),
            "暫停滿 24 小時才接受"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn long_running_anchor_carries_elapsed_time_across_restart() {
        // 存檔時已經過的時間會跨重啟延續：執行數個月後正常重新啟動，不會誤判為跳躍
        let dir = temp_dir("old-anchor-restart");
        let mut c = Clock::new("2026-01-01T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        run_daily(&mut c, &mut g, 90);
        drop(g);
        c.advance(HOUR);
        let mut g = guard_at(&dir, &c);
        assert_eq!(c.check(&mut g), Some(d("2026-04-01")));
        run_daily(&mut c, &mut g, 3);
        assert_eq!(g.core.anchor_date(), d("2026-01-01"));
        // 停機期間的牆鐘差上限 24 小時：停機 5 天後重啟 → 暫停
        drop(g);
        c.advance(5 * DAY);
        let mut g = guard_at(&dir, &c);
        assert_eq!(c.check(&mut g), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn state_is_saved_on_every_check() {
        let dir = temp_dir("save-every");
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut g = guard_at(&dir, &c);
        c.check(&mut g);
        let first = read_state(&dir);
        c.advance(HOUR);
        c.check(&mut g);
        let second = read_state(&dir);
        assert_eq!(second.accepted, first.accepted);
        assert_eq!(second.saved_unix, first.saved_unix.map(|t| t + 3600));
        assert_eq!(
            second.anchor_elapsed_secs,
            first.anchor_elapsed_secs.map(|t| t + 3600)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_or_corrupt_state_uses_newest_log_not_after_today() {
        let dir = temp_dir("baseline");
        for name in [
            "pulse-2026-10-01.log",
            "pulse-2026-10-05.log",
            // 時鐘誤校時建立的未來檔案不列入
            "pulse-2027-01-01.log",
        ] {
            fs::write(dir.join(name), "x").unwrap();
        }
        let today = d("2026-11-08");
        let now = Instant::now();
        let w = wall("2026-11-08T10:00:00Z");
        let mut r = LogRetention::open_at(&dir, 14, today, now, w);
        assert_eq!(r.guard.as_ref().unwrap().core.accepted(), d("2026-10-05"));
        assert!(r.pending_logs.is_empty());
        // 以最新日誌檔為基準：今天比它晚 34 天 → 暫停，不刪檔
        assert_eq!(r.run_at(today, now, w), 0);
        assert_eq!(names(&dir).len(), 4);
        // 損毀的狀態檔：同樣以最新日誌檔為基準，並在第一次執行時記 warn、改寫狀態檔
        fs::write(dir.join(GUARD_STATE_FILE), b"{not json").unwrap();
        let mut r = LogRetention::open_at(&dir, 14, today, now, w);
        assert_eq!(r.guard.as_ref().unwrap().core.accepted(), d("2026-10-05"));
        let (_, logs) = crate::test_log::capture(|| r.run_at(today, now, w));
        assert!(
            logs.iter()
                .any(|(l, m)| *l == log::Level::Warn && m.contains("內容無效")),
            "{logs:?}"
        );
        assert_eq!(read_state(&dir).accepted, "2026-10-05");
        // 沒有任何日誌檔：以今天為基準
        let empty = temp_dir("baseline-empty");
        let r = LogRetention::open_at(&empty, 14, today, now, w);
        assert_eq!(r.guard.as_ref().unwrap().core.accepted(), today);
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(empty).unwrap();
    }

    #[test]
    fn retention_with_clock_jump_does_not_delete_logs() {
        // R2-P5：端對端，時鐘往前跳時不刪檔，持續 24 小時後才清除
        let dir = temp_dir("e2e");
        for name in [
            "pulse-2026-09-20.log",
            "pulse-2026-10-01.log",
            "pulse-2026-10-09.log",
        ] {
            fs::write(dir.join(name), "x").unwrap();
        }
        let mut c = Clock::new("2026-10-09T10:00:00Z");
        let mut r = LogRetention::open_at(&dir, 14, c.today(), c.now(), c.wall);
        assert_eq!(r.run_at(c.today(), c.now(), c.wall), 1, "09-20 已過期");
        c.set_wall("2027-10-09T10:00:00Z");
        assert_eq!(r.run_at(c.today(), c.now(), c.wall), 0);
        c.advance(12 * HOUR);
        assert_eq!(r.run_at(c.today(), c.now(), c.wall), 0);
        assert_eq!(names(&dir).len(), 3, "{:?}", names(&dir));
        c.advance(12 * HOUR);
        assert_eq!(r.run_at(c.today(), c.now(), c.wall), 2);
        assert_eq!(names(&dir), [GUARD_STATE_FILE]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zero_retention_does_not_check_or_write_state() {
        let dir = temp_dir("zero");
        fs::write(dir.join("pulse-2000-01-01.log"), "x").unwrap();
        let mut r = LogRetention::open(&dir, 0);
        assert!(r.guard.is_none());
        assert_eq!(r.run(), 0);
        assert_eq!(names(&dir), ["pulse-2000-01-01.log"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn state_file_is_written_atomically_without_leftovers() {
        let dir = temp_dir("atomic");
        let state = GuardState {
            accepted: "2026-10-09".into(),
            anchor_date: Some("2026-10-09".into()),
            anchor_elapsed_secs: Some(0),
            saved_unix: Some(1),
            jump_since_unix: None,
            jump_date: None,
            jump_elapsed_secs: None,
            rollback_since_unix: None,
        };
        let path = dir.join(GUARD_STATE_FILE);
        write_state_atomically(&path, &state).unwrap();
        // 覆寫既有狀態檔
        let state2 = GuardState {
            accepted: "2026-10-10".into(),
            ..state
        };
        write_state_atomically(&path, &state2).unwrap();
        assert_eq!(read_state(&dir), state2);
        assert_eq!(names(&dir), [GUARD_STATE_FILE], "不得留下暫存檔");
        // 舊版固定的暫存檔名存在也不受影響（暫存檔名是隨機的）
        fs::write(dir.join(format!("{GUARD_STATE_FILE}.tmp")), "x").unwrap();
        write_state_atomically(&path, &state2).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(format!("{GUARD_STATE_FILE}.tmp"))).unwrap(),
            "x"
        );
        // 目錄不存在時會建立
        let nested = dir.join("sub").join(GUARD_STATE_FILE);
        write_state_atomically(&nested, &state2).unwrap();
        assert!(nested.is_file());
        fs::remove_dir_all(dir).unwrap();
    }
}
