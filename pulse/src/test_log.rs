//! 測試用 logger：記錄「同一執行緒」產生的日誌，供測試斷言日誌等級與內容。
//!
//! `#[tokio::test]` 預設為 current_thread runtime，所有 task 都在測試執行緒上執行，
//! 因此 async 測試也能用 [`capture`] 包住 `block_on`。

use std::cell::RefCell;
use std::sync::Once;

use log::{Level, LevelFilter, Log, Metadata, Record};

thread_local! {
    static CAPTURED: RefCell<Option<Vec<(Level, String)>>> = const { RefCell::new(None) };
}

struct CaptureLogger;

impl Log for CaptureLogger {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let msg = record.args().to_string();
        CAPTURED.with(|c| {
            if let Some(logs) = c.borrow_mut().as_mut() {
                logs.push((record.level(), msg));
            }
        });
    }

    fn flush(&self) {}
}

static LOGGER: CaptureLogger = CaptureLogger;
static INIT: Once = Once::new();

/// 執行 `f`，回傳其結果與期間本執行緒記錄的所有日誌（等級、訊息）。
pub fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<(Level, String)>) {
    INIT.call_once(|| {
        // 測試行程中只有這一個 logger；設定失敗代表已設定過，不影響
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(LevelFilter::Trace);
    });
    CAPTURED.with(|c| *c.borrow_mut() = Some(Vec::new()));
    let out = f();
    let logs = CAPTURED.with(|c| c.borrow_mut().take().unwrap_or_default());
    (out, logs)
}
