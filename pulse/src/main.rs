mod heartbeat;
mod system_info;
mod network;
mod config_manager;

use log::LevelFilter;
use config_manager::AppConfig;

#[tokio::main]
async fn main() {
    // 讀取配置檔案
    let config = AppConfig::from_file("config.toml").expect("無法讀取配置檔案");

    // 初始化日誌
    env_logger::Builder::new()
        .filter(None, match config.log.level.as_str() {
            "trace" => LevelFilter::Trace,
            "debug" => LevelFilter::Debug,
            "info" => LevelFilter::Info,
            "warn" => LevelFilter::Warn,
            "error" => LevelFilter::Error,
            _ => LevelFilter::Info,
        })
        .init();

    log::info!("Pulse 地端程式啟動...");
    log::debug!("使用的配置: {:?}", config);

    // 收集並顯示系統資訊
    system_info::get_system_info();

    // 啟動心跳功能
    tokio::spawn(async move {
        heartbeat::send_heartbeat(config.heartbeat.interval).await;
    });

    // 啟動週期性發送系統資訊
    tokio::spawn(async move {
        network::send_system_info_periodically(
            config.server.address,
            config.server.port,
            config.retry.max_retries,
            config.retry.initial_delay,
        )
            .await;
    });

    // 保持主程式運行
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
    }
}
