use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::pki_types::pem::{self, PemObject};
use std::future::Future;
use std::io::{Error, ErrorKind};
use std::sync::Arc;
use std::time::Duration;

// 握手與讀寫的逾時時間，避免閒置連線永久佔用任務
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<(), Error> {
    // 載入 TLS 設定
    let tls_config = load_tls_config("cert.pem", "key.pem")?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("TLS 伺服器已啟動，等待連線...");

    loop {
        let (socket, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                // 接受連線失敗（例如檔案描述符用盡）不應讓整個伺服器結束，稍候再重試
                eprintln!("接受連線失敗: {}", e);
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        println!("接收到來自 {} 的連線", addr);

        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(acceptor, socket).await {
                eprintln!("處理來自 {} 的連線時發生錯誤: {}", addr, e);
            }
        });
    }
}

async fn handle_connection(acceptor: TlsAcceptor, socket: TcpStream) -> Result<(), Error> {
    let mut tls_stream = with_timeout(HANDSHAKE_TIMEOUT, "TLS 握手", acceptor.accept(socket)).await?;

    let mut buffer = vec![0; 1024];
    let n = with_timeout(IO_TIMEOUT, "讀取資料", tls_stream.read(&mut buffer)).await?;
    if n == 0 {
        // 客戶端未送出任何資料就關閉連線
        return Ok(());
    }
    println!("收到資料: {}", String::from_utf8_lossy(&buffer[..n]));

    // 回應確認訊息
    let response = b"data received over TLS";
    with_timeout(IO_TIMEOUT, "發送回應", tls_stream.write_all(response)).await?;
    println!("回應已發送");

    // 送出 close_notify，讓客戶端知道連線是正常結束而非被截斷
    with_timeout(IO_TIMEOUT, "關閉連線", tls_stream.shutdown()).await?;

    Ok(())
}

// 為非同步 I/O 加上逾時，逾時時轉換為 std::io::Error
async fn with_timeout<T>(
    duration: Duration,
    action: &str,
    fut: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    timeout(duration, fut)
        .await
        .map_err(|_| Error::new(ErrorKind::TimedOut, format!("{}逾時", action)))?
}

// 載入證書與私鑰檔案
fn load_tls_config(cert_path: &str, key_path: &str) -> Result<ServerConfig, Error> {
    let certs = CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| pem_error(cert_path, "證書", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| pem_error(cert_path, "證書", e))?;
    if certs.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, format!("{} 中找不到證書", cert_path)));
    }

    // 讀取第一把私鑰，接受 PKCS#8、PKCS#1（RSA）與 SEC1（EC）格式
    let key = PrivateKeyDer::from_pem_file(key_path).map_err(|e| pem_error(key_path, "私鑰", e))?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::new(ErrorKind::InvalidData, e))?; // 手動將錯誤轉換為 std::io::Error

    Ok(config)
}

// 將 PEM 解析錯誤轉換為 std::io::Error，錯誤訊息附上路徑以便判斷是哪個檔案出問題
fn pem_error(path: &str, item: &str, e: pem::Error) -> Error {
    match e {
        pem::Error::Io(e) => Error::new(e.kind(), format!("無法開啟 {}: {}", path, e)),
        pem::Error::NoItemsFound => Error::new(ErrorKind::InvalidInput, format!("{} 中找不到{}", path, item)),
        e => Error::new(ErrorKind::InvalidData, format!("{} 中的{}格式錯誤: {}", path, item, e)),
    }
}
