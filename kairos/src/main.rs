use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{ServerConfig, Certificate, PrivateKey};
use std::fs::File;
use std::io::{BufReader, Error};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // 載入 TLS 設定
    let tls_config = load_tls_config("cert.pem", "key.pem")?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("TLS 伺服器已啟動，等待連線...");

    loop {
        let (socket, addr) = listener.accept().await?;
        println!("接收到來自 {} 的連線", addr);

        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Ok(mut tls_stream) = acceptor.accept(socket).await {
                let mut buffer = vec![0; 1024];
                if let Ok(n) = tls_stream.read(&mut buffer).await {
                    println!("收到資料: {}", String::from_utf8_lossy(&buffer[..n]));

                    // 回應確認訊息
                    let response = b"data received over TLS";
                    if tls_stream.write_all(response).await.is_ok() {
                        println!("回應已發送");
                    }
                }
            }
        });
    }
}

// 載入證書與私鑰檔案
fn load_tls_config(cert_path: &str, key_path: &str) -> Result<ServerConfig, Error> {
    let cert_file = &mut BufReader::new(File::open(cert_path)?);
    let key_file = &mut BufReader::new(File::open(key_path)?);

    let certs = rustls_pemfile::certs(cert_file)?
        .into_iter()
        .map(Certificate)
        .collect();

    let mut keys = rustls_pemfile::pkcs8_private_keys(key_file)?;
    if keys.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "No private key found"));
    }

    let key = PrivateKey(keys.remove(0));

    let config = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?; // 手動將錯誤轉換為 std::io::Error

    Ok(config)
}

