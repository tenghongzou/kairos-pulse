//! 日誌：輸出到終端與 `logs/kairos-YYYY-MM-DD.log`（依 UTC 日期分檔），並依保留天數清除舊檔。

use chrono::{NaiveDate, Utc};
use log::{LevelFilter, info};
use std::fs::{OpenOptions, create_dir_all};
use std::io::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{MissedTickBehavior, interval};

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

/// 初始化日誌。檔名依 UTC 日期，換日時由 fern 自動切換到新檔；每筆日誌寫入後即 flush。
///
/// 回傳建立今天的日誌檔**之前**既有最新日誌檔的日期，作為日誌保留清除的日期基準（見 [`run_cleanup`]）。
pub fn setup(level: LevelFilter) -> Result<Option<NaiveDate>, Error> {
    create_dir_all(LOG_DIR)
        .map_err(|e| Error::new(e.kind(), format!("無法建立日誌目錄 {}: {}", LOG_DIR, e)))?;
    let newest_before_start = LOG_FILES.newest(Path::new(LOG_DIR), Utc::now().date_naive());

    // fern 在第一次寫入時才開檔；先開一次今天的檔案，讓權限等問題在啟動時就被發現
    let today = Path::new(LOG_DIR).join(LOG_FILES.file_name(Utc::now().date_naive()));
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&today)
        .map_err(|e| {
            Error::new(
                e.kind(),
                format!("無法開啟日誌檔 {}: {}", today.display(), e),
            )
        })?;

    let file = fern::DateBased::new(
        Path::new(LOG_DIR).join(LOG_FILES.prefix),
        format!("%Y-%m-%d{}", LOG_FILES.suffix),
    )
    .utc_time();

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
