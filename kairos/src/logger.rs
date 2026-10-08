//! 日誌：輸出到終端與 `logs/kairos-YYYY-MM-DD.log`（依 UTC 日期分檔），並依保留天數清除舊檔。

use chrono::Utc;
use log::{LevelFilter, info};
use std::fs::{OpenOptions, create_dir_all};
use std::io::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{MissedTickBehavior, interval};

use crate::retention::DatedFiles;
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
pub fn setup(level: LevelFilter) -> Result<(), Error> {
    create_dir_all(LOG_DIR)
        .map_err(|e| Error::new(e.kind(), format!("無法建立日誌目錄 {}: {}", LOG_DIR, e)))?;

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
        .map_err(|e| Error::other(format!("無法初始化日誌: {}", e)))
}

/// 啟動時清除一次舊日誌、提示舊版日誌檔，之後每小時清除一次，直到關機
pub async fn run_cleanup(retention_days: u32, shutdown: Arc<Shutdown>) {
    let dir = Path::new(LOG_DIR);
    let legacy = dir.join(LEGACY_LOG_FILE);
    if tokio::fs::metadata(&legacy).await.is_ok() {
        info!(
            "發現舊版的 {}：日誌已改為按 UTC 日期分檔（kairos-YYYY-MM-DD.log），舊檔不會再寫入，也不會被自動刪除",
            legacy.display()
        );
    }

    let mut ticker = interval(LOG_CLEANUP_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        // 第一次 tick 立即完成：啟動時先清除一次
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        LOG_FILES
            .cleanup(dir, Utc::now().date_naive(), retention_days)
            .await;
    }
}
