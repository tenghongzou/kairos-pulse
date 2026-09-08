use std::time::Duration;
use tokio::time;

/// 週期性輸出本機存活訊號（liveness tick）。
///
/// 注意：目前僅寫入日誌，尚未透過網路送出；未來若要做真正的
/// 網路心跳，可在此改為呼叫 network 模組送出。
pub async fn send_heartbeat(interval: u64) {
    loop {
        log::debug!("💓 Heartbeat at {:?}", chrono::Utc::now());
        time::sleep(Duration::from_secs(interval)).await; // 每 interval 秒執行一次
    }
}
