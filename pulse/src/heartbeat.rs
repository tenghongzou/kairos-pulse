use std::time::Duration;
use tokio::time;

pub async fn send_heartbeat() {
    loop {
        println!("💓 Heartbeat sent at {:?}", chrono::Utc::now());
        time::sleep(Duration::from_secs(1)).await; // 每 1 秒執行一次
    }
}
