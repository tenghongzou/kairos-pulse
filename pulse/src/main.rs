mod heartbeat;
mod system_info;
mod network;

#[tokio::main]
async fn main() {
    // 初始化日誌
    env_logger::init();
    log::info!("Pulse 地端程式啟動...");

    // 收集並顯示系統資訊
    system_info::get_system_info();

    // 啟動心跳功能
    tokio::spawn(async {
        heartbeat::send_heartbeat().await;
    });

    // 啟動週期性發送系統資訊
    tokio::spawn(async {
        network::send_system_info_periodically().await;
    });

    // 保持主程式運行
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
    }
}
