use std::io;
use std::io::Result;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio_native_tls::TlsConnector;

use crate::system_info::get_real_system_info;

/// TLS 連線所需的參數，集中傳遞避免大量獨立參數。
#[derive(Clone)]
struct TlsSettings {
    /// 連線位址（`host:port`）
    server: String,
    /// TLS SNI / 憑證驗證用的主機名稱
    domain: String,
    /// 是否略過憑證驗證（僅供開發）
    accept_invalid_certs: bool,
}

#[allow(clippy::too_many_arguments)]
pub async fn send_system_info_periodically(
    server_address: String,
    server_port: u16,
    domain: String,
    accept_invalid_certs: bool,
    interval: u64,
    max_retries: usize,
    initial_delay: u64,
) {
    let settings = Arc::new(TlsSettings {
        server: format!("{}:{}", server_address, server_port),
        domain,
        accept_invalid_certs,
    });
    // 限制同時進行的回報任務數量，避免連線在高延遲時無限堆積
    let semaphore = Arc::new(Semaphore::new(100));

    loop {
        let settings = Arc::clone(&settings);
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        tokio::spawn(async move {
            if let Err(e) = send_system_info_with_retry(&settings, max_retries, initial_delay).await
            {
                log::error!("回報系統資訊失敗: {}", e);
            }
            drop(permit);
        });

        // 依設定的間隔啟動下一次回報
        tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
    }
}

async fn send_system_info_with_retry(
    settings: &TlsSettings,
    max_retries: usize,
    initial_delay: u64,
) -> Result<()> {
    let mut retries = 0;
    let mut delay = initial_delay;

    while retries < max_retries {
        match send_system_info(settings).await {
            Ok(_) => {
                log::info!("資料成功發送");
                return Ok(());
            }
            Err(e) => {
                retries += 1;
                log::error!(
                    "連線失敗 (第 {}/{}) 次嘗試: {}，將在 {} 秒後重試",
                    retries,
                    max_retries,
                    e,
                    delay
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
                delay = delay.saturating_mul(2);
            }
        }
    }
    Err(io::Error::other("重試次數已達上限"))
}

async fn send_system_info(settings: &TlsSettings) -> Result<()> {
    // 建立 TLS Connector
    let mut builder = native_tls::TlsConnector::builder();
    if settings.accept_invalid_certs {
        // ⚠️ 僅供本機自簽憑證的開發測試，正式環境不應開啟
        log::warn!("已停用 TLS 憑證驗證（accept_invalid_certs = true），請勿用於正式環境");
        builder.danger_accept_invalid_certs(true);
    }
    let native_connector = builder
        .build()
        .map_err(io::Error::other)?;
    let connector = TlsConnector::from(native_connector);

    let tcp_stream = TcpStream::connect(&settings.server).await?;
    // 使用設定的 domain 作為 SNI / 憑證驗證主機名稱
    let mut tls_stream = connector
        .connect(&settings.domain, tcp_stream)
        .await
        .map_err(io::Error::other)?;
    log::info!("已建立 TLS 加密連線");

    // 獲取真實系統資訊
    let system_info = get_real_system_info().await;

    // 序列化為 JSON
    let json_data = serde_json::to_string(&system_info)?;
    log::info!("發送的資料: {}", json_data);

    // 發送資料
    tls_stream.write_all(json_data.as_bytes()).await?;
    tls_stream.flush().await?;
    log::info!("資料已成功發送");

    // 接收伺服器回應
    let mut buffer = vec![0; 1024];
    let n = tls_stream.read(&mut buffer).await?;
    log::info!("伺服器回應: {}", String::from_utf8_lossy(&buffer[..n]));

    Ok(())
}
