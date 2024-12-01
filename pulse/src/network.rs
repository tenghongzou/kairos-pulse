use std::io::Result;
use tokio_native_tls::TlsConnector;
use tokio::net::TcpStream;
use tokio::io::{AsyncWriteExt, AsyncReadExt};
use std::sync::Arc;
use tokio::sync::Semaphore;
use log::info;
use std::io;

use crate::system_info::{get_real_system_info};

pub async fn send_system_info_periodically(
    server_address: String,
    server_port: u16,
    max_retries: usize,
    initial_delay: u64,
) {
    let server = Arc::new(format!("{}:{}", server_address, server_port));
    let semaphore = Arc::new(Semaphore::new(100));

    loop {
        let server_owned = Arc::clone(&server);
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        tokio::spawn(async move {
            if let Err(e) = send_system_info_with_retry(&server_owned, max_retries, initial_delay).await {
                eprintln!("Error: {}", e);
            }
            drop(permit);
        });

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await; // 每秒啟動一次任務
    }
}

async fn send_system_info_with_retry(
    server: &str,
    max_retries: usize,
    initial_delay: u64,
) -> Result<()> {
    let mut retries = 0;
    let mut delay = initial_delay;

    while retries < max_retries {
        match send_system_info(server).await {
            Ok(_) => {
                log::info!("資料成功發送");
                return Ok(());
            }
            Err(e) => {
                retries += 1;
                log::error!(
                    "連線失敗 (第 {}/{}) 次嘗試: {}，將在 {} 秒後重試",
                    retries, max_retries, e, delay
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
                delay *= 2;
            }
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::Other, "重試次數已達上限"))
}

async fn send_system_info(server: &str) -> Result<()> {
    let server_address = server; // 使用伺服器的域名與端口

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

    // 獲取真實系統資訊
    let system_info = get_real_system_info();

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
