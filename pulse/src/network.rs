use serde::Serialize;
use std::io::Result;
use tokio_native_tls::TlsConnector;
use tokio::net::TcpStream;
use tokio::io::{AsyncWriteExt, AsyncReadExt};
use native_tls::TlsConnector as NativeTlsConnector;
use tokio::time::{sleep, Duration};
use log::{info, error};
use std::io;

#[derive(Serialize)]
struct SystemInfo {
    cpu_usage: f32,
    total_memory: u64,
    used_memory: u64,
    hostname: String,
}

pub async fn send_system_info_periodically() {
    loop {
        if let Err(e) = send_system_info_with_retry(5).await {
            error!("重試失敗，放棄本次發送: {}", e);
        }
        sleep(Duration::from_secs(10)).await; // 等待後繼續下一次週期性發送
    }
}

async fn send_system_info_with_retry(max_retries: usize) -> Result<()> {
    let mut retries = 0;
    let mut delay = 2; // 初始重試延遲（秒）

    while retries < max_retries {
        match send_system_info().await {
            Ok(_) => {
                info!("資料成功發送");
                return Ok(()); // 發送成功，退出重試
            }
            Err(e) => {
                retries += 1;
                error!(
                    "連線失敗 (第 {}/{}) 次嘗試: {}，將在 {} 秒後重試",
                    retries, max_retries, e, delay
                );
                sleep(Duration::from_secs(delay)).await;
                delay *= 2; // 指數回退策略：延長重試間隔
            }
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::Other, "重試次數已達上限"))
}

async fn send_system_info() -> Result<()> {
    let server_address = "127.0.0.1:8080"; // 使用伺服器的域名與端口

    // 建立不驗證憑證的 TLS Connector
    let mut builder = native_tls::TlsConnector::builder();
    builder.danger_accept_invalid_certs(true); // 繞過憑證驗證
    let native_connector = builder.build().unwrap();
    let connector = TlsConnector::from(native_connector);
    // 正常
    // let connector = TlsConnector::from(NativeTlsConnector::new().unwrap());

    let tcp_stream = TcpStream::connect(server_address).await?;
    let mut tls_stream = connector.connect("127.0.0.1", tcp_stream)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    info!("已建立 TLS 加密連線");

    // 模擬系統資訊（可改為從 `system_info` 模組獲取真實數據）
    let system_info = SystemInfo {
        cpu_usage: 12.5,
        total_memory: 8192,
        used_memory: 2048,
        hostname: "localhost".to_string(),
    };

    // 序列化為 JSON
    let json_data = serde_json::to_string(&system_info)?;
    info!("發送的資料: {}", json_data);

    // 發送資料
    tls_stream.write_all(json_data.as_bytes()).await?;
    tls_stream.flush().await?;
    info!("資料已成功發送");

    // 接收伺服器回應
    let mut buffer = vec![0; 1024];
    let n = tls_stream.read(&mut buffer).await?;
    info!("伺服器回應: {}", String::from_utf8_lossy(&buffer[..n]));

    Ok(())
}
