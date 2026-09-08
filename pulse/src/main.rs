mod heartbeat;
mod system_info;
mod network;
mod config_manager;

use config_manager::AppConfig;
use log::{debug, info};
use std::fs::create_dir_all;

fn setup_logger(log_level: &str) -> Result<(), fern::InitError> {
    // 確保日誌目錄存在
    create_dir_all("logs")?;

    let log_level = match log_level {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    };

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} [{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                message
            ))
        })
        .level(log_level)
        .chain(std::io::stdout()) // 日誌輸出到終端
        .chain(fern::log_file("logs/pulse.log")?) // 日誌輸出到檔案
        .apply()?;

    Ok(())
}

#[tokio::main]
async fn main() {
    // 讀取編譯時嵌入的配置檔案
    let config = AppConfig::from_embedded().expect("無法解析嵌入的設定檔 config.toml");

    // 初始化日誌
    setup_logger(&config.log.level).expect("無法初始化日誌");

    info!("Pulse 地端程式啟動...");
    debug!("使用的配置: {:?}", config);

    // 收集並顯示系統資訊
    system_info::get_system_info();

    // 啟動心跳功能
    let heartbeat_interval = config.heartbeat.interval;
    tokio::spawn(async move {
        heartbeat::send_heartbeat(heartbeat_interval).await;
    });

    // 啟動週期性發送系統資訊
    tokio::spawn(async move {
        network::send_system_info_periodically(
            config.server.address,
            config.server.port,
            config.server.domain,
            config.server.accept_invalid_certs,
            config.report.interval,
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
