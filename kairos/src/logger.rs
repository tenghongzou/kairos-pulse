//! 日誌：輸出到終端與 `logs/kairos-YYYY-MM-DD.log`（依 UTC 日期分檔），並依保留天數清除舊檔。

use chrono::{DateTime, NaiveDate, Utc};
use log::{LevelFilter, info};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Error, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{MissedTickBehavior, interval};

use crate::fsperm;
use crate::retention::{
    DateGuard, DatedFiles, GUARD_STATE_FILE, LOG_GUARD_STATE_FILE, is_tmp_name_for,
};
use crate::shutdown::Shutdown;

pub const LOG_DIR: &str = "logs";

/// 日誌檔：`logs/kairos-YYYY-MM-DD.log`（UTC 日期，與 reports 一致）
pub const LOG_FILES: DatedFiles = DatedFiles {
    prefix: "kairos-",
    suffix: ".log",
    label: "日誌檔",
};

/// 舊版（按日分檔前）的單一日誌檔
pub const LEGACY_LOG_FILE: &str = "kairos.log";

/// 定期清除舊日誌的間隔
pub const LOG_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// 以附加模式開啟日誌檔。Unix 上新檔案為 0640，既有的檔案收緊到最多 0640（S3）。
/// 會在日誌的寫入路徑上呼叫，因此不可以記錄日誌（會重入 logger）；收緊失敗時忽略
fn open_log_file(path: &Path) -> std::io::Result<File> {
    let file =
        fsperm::private_std_options(OpenOptions::new().create(true).append(true)).open(path)?;
    let _ = fsperm::restrict_open_file(&file);
    Ok(file)
}

/// 依 UTC 日期分檔的日誌輸出（取代 fern 的 `DateBased`，以便在 Unix 上指定新檔案的權限）。
///
/// fern 每筆日誌先寫入（可能分成多次 `write`）再呼叫一次 `flush`；只在一筆日誌的第一次 `write`
/// 檢查日期，同一筆日誌不會被拆到兩個檔案。換日時關閉舊檔、開啟（必要時建立）新日期的檔案。
pub struct DailyLogFile {
    dir: PathBuf,
    clock: fn() -> DateTime<Utc>,
    current: Option<(NaiveDate, BufWriter<File>)>,
    /// 下一次 `write` 是一筆新日誌的開頭
    record_start: bool,
}

impl DailyLogFile {
    pub fn new(dir: PathBuf, clock: fn() -> DateTime<Utc>) -> Self {
        Self {
            dir,
            clock,
            current: None,
            record_start: true,
        }
    }

    fn file(&mut self) -> std::io::Result<&mut BufWriter<File>> {
        if self.record_start || self.current.is_none() {
            let today = (self.clock)().date_naive();
            if self.current.as_ref().map(|(d, _)| *d) != Some(today) {
                if let Some((_, mut old)) = self.current.take() {
                    let _ = old.flush();
                }
                let file = open_log_file(&self.dir.join(LOG_FILES.file_name(today)))?;
                self.current = Some((today, BufWriter::new(file)));
            }
            self.record_start = false;
        }
        self.current
            .as_mut()
            .map(|(_, f)| f)
            .ok_or_else(|| Error::other("日誌檔未開啟"))
    }
}

impl Write for DailyLogFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file()?.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.record_start = true;
        match self.current.as_mut() {
            Some((_, f)) => f.flush(),
            None => Ok(()),
        }
    }
}

/// 初始化日誌。檔名依 UTC 日期，換日時自動切換到新檔；每筆日誌寫入後即 flush。
/// Unix 上日誌目錄（新建立時）為 0750、日誌檔最多 0640（S3）。
///
/// 回傳建立今天的日誌檔**之前**既有最新日誌檔的日期，作為日誌保留清除的日期基準（見 [`run_cleanup`]）。
pub fn setup(level: LevelFilter) -> Result<Option<NaiveDate>, Error> {
    fsperm::create_dir_all(Path::new(LOG_DIR))
        .map_err(|e| Error::new(e.kind(), format!("無法建立日誌目錄 {}: {}", LOG_DIR, e)))?;
    let newest_before_start = LOG_FILES.newest(Path::new(LOG_DIR), Utc::now().date_naive());

    // 日誌檔在第一次寫入時才開啟；先開一次今天的檔案，讓權限等問題在啟動時就被發現
    let today = Path::new(LOG_DIR).join(LOG_FILES.file_name(Utc::now().date_naive()));
    open_log_file(&today).map_err(|e| {
        Error::new(
            e.kind(),
            format!("無法開啟日誌檔 {}: {}", today.display(), e),
        )
    })?;

    let file = fern::Output::writer(
        Box::new(DailyLogFile::new(PathBuf::from(LOG_DIR), Utc::now)),
        "\n",
    );

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} [{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                message
            ))
        })
        .level(level)
        .chain(std::io::stdout()) // 日誌輸出到終端
        .chain(file) // 日誌輸出到按日分檔的檔案
        .apply()
        .map_err(|e| Error::other(format!("無法初始化日誌: {}", e)))?;
    Ok(newest_before_start)
}

/// 啟動時清除一次 `dir`（正式程式為 [`LOG_DIR`]）中的舊日誌、提示舊版日誌檔，之後每小時清除一次，
/// 直到關機。`newest_before_start` 為啟動前既有最新日誌檔的日期（[`setup`] 的回傳值）。
pub async fn run_cleanup(
    dir: PathBuf,
    retention_days: u32,
    newest_before_start: Option<NaiveDate>,
    shutdown: Arc<Shutdown>,
) {
    run_cleanup_every(
        dir,
        retention_days,
        newest_before_start,
        shutdown,
        LOG_CLEANUP_INTERVAL,
    )
    .await
}

async fn run_cleanup_every(
    dir: PathBuf,
    retention_days: u32,
    newest_before_start: Option<NaiveDate>,
    shutdown: Arc<Shutdown>,
    every: Duration,
) {
    let dir = dir.as_path();
    remove_stale_guard_tmp_files(dir).await;
    let legacy = dir.join(LEGACY_LOG_FILE);
    if tokio::fs::metadata(&legacy).await.is_ok() {
        info!(
            "發現舊版的 {}：日誌已改為按 UTC 日期分檔（kairos-YYYY-MM-DD.log），舊檔不會再寫入，也不會被自動刪除",
            legacy.display()
        );
    }

    let mut ticker = interval(every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 時鐘被誤校到未來時不清除（見 DateGuard）。狀態以 logs/ 中的狀態檔為準（檔名與 report 的不同，
    // 兩個目錄相同時也不會共用；舊版的檔名只在新檔不存在時讀取）；都沒有（或損毀）時
    // 比照 report，以啟動前最新的日誌檔日期為基準（不晚於今天）
    let baseline = LOG_FILES.guard_baseline(newest_before_start, Utc::now().date_naive());
    let mut guard = DateGuard::load_or_new(
        LOG_FILES.label,
        dir.join(LOG_GUARD_STATE_FILE),
        Some(&dir.join(GUARD_STATE_FILE)),
        baseline,
        Instant::now(),
        Utc::now(),
    );
    loop {
        // 第一次 tick 立即完成：啟動時先清除一次
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        if retention_days == 0 {
            continue;
        }
        let now_wall = Utc::now();
        if let Some(today) = guard
            .check(now_wall.date_naive(), Instant::now(), now_wall)
            .await
        {
            LOG_FILES.cleanup(dir, today, retention_days).await;
        }
    }
}

/// 刪除先前異常結束時留下的狀態檔暫存檔（`.retention-guard-logs.json.<隨機>.tmp`，
/// 以及舊版檔名的暫存檔）
async fn remove_stale_guard_tmp_files(dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if (is_tmp_name_for(name, LOG_GUARD_STATE_FILE) || is_tmp_name_for(name, GUARD_STATE_FILE))
            && entry.file_type().await.is_ok_and(|t| t.is_file())
        {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicI64, Ordering};

    static LOG_CLOCK: AtomicI64 = AtomicI64::new(0);

    fn log_clock() -> DateTime<Utc> {
        Utc.timestamp_opt(LOG_CLOCK.load(Ordering::SeqCst), 0)
            .unwrap()
    }

    #[test]
    fn daily_log_file_switches_files_only_between_records() {
        let dir = std::env::temp_dir().join(format!(
            "kairos-logfile-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        fsperm::create_dir_all(&dir).unwrap();
        let day1 = Utc.with_ymd_and_hms(2026, 10, 9, 23, 59, 59).unwrap();
        let day2 = Utc.with_ymd_and_hms(2026, 10, 10, 0, 0, 1).unwrap();
        LOG_CLOCK.store(day1.timestamp(), Ordering::SeqCst);
        let mut w = DailyLogFile::new(dir.clone(), log_clock);
        // 一筆日誌分成多次 write：中途跨日也留在同一個檔案
        w.write_all(b"a1 ").unwrap();
        LOG_CLOCK.store(day2.timestamp(), Ordering::SeqCst);
        w.write_all(b"a2\n").unwrap();
        w.flush().unwrap();
        // 下一筆寫進新日期的檔案
        w.write_all(b"b\n").unwrap();
        w.flush().unwrap();
        let read = |d: DateTime<Utc>| {
            std::fs::read_to_string(dir.join(LOG_FILES.file_name(d.date_naive()))).unwrap()
        };
        assert_eq!(read(day1), "a1 a2\n");
        assert_eq!(read(day2), "b\n");
        #[cfg(unix)]
        {
            // S3：新的日誌檔為 0640
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join(LOG_FILES.file_name(day2.date_naive()));
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o640);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn run_cleanup_deletes_old_logs_and_stops_on_shutdown() {
        // R2-T1：logger::run_cleanup 的啟動清除、狀態檔與關機結束
        let dir = std::env::temp_dir().join(format!(
            "kairos-logger-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let today = Utc::now().date_naive();
        let yesterday = today.pred_opt().unwrap();
        let old = dir.join(LOG_FILES.file_name(today - chrono::Days::new(30)));
        let kept = dir.join(LOG_FILES.file_name(yesterday));
        let legacy = dir.join(LEGACY_LOG_FILE);
        for p in [&old, &kept, &legacy] {
            std::fs::write(p, b"x\n").unwrap();
        }
        // 先前異常結束留下的狀態檔暫存檔
        let stale = dir.join(".retention-guard-logs.json.0123456789abcdef.tmp");
        std::fs::write(&stale, b"x").unwrap();
        let shutdown = Arc::new(Shutdown::new());
        let task = tokio::spawn(run_cleanup_every(
            dir.clone(),
            14,
            Some(yesterday),
            shutdown.clone(),
            Duration::from_millis(20),
        ));
        let mut deleted = false;
        for _ in 0..250 {
            if !old.exists() {
                deleted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(deleted, "過期日誌應在啟動時刪除");
        assert!(kept.exists());
        assert!(legacy.exists(), "舊版日誌檔不碰");
        // 日誌使用自己的狀態檔名
        assert!(dir.join(LOG_GUARD_STATE_FILE).exists());
        assert!(!stale.exists(), "啟動時應刪除殘留的暫存檔");
        assert!(!dir.join(GUARD_STATE_FILE).exists());

        shutdown.trigger();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("關機後應結束")
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
