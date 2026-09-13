use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{ServerConfig, Certificate, PrivateKey};
use rustls_pemfile::Item;
use std::fs::File;
use std::future::Future;
use std::io::{BufRead, BufReader, Error, ErrorKind};
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
    let cert_file = &mut BufReader::new(open_file(cert_path)?);
    let key_file = &mut BufReader::new(open_file(key_path)?);

    let certs: Vec<Certificate> = rustls_pemfile::certs(cert_file)?
        .into_iter()
        .map(Certificate)
        .collect();
    if certs.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, format!("{} 中找不到證書", cert_path)));
    }

    let key = read_private_key(key_file)?.ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput, format!("{} 中找不到私鑰", key_path))
    })?;

    let config = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::new(ErrorKind::InvalidData, e))?; // 手動將錯誤轉換為 std::io::Error

    Ok(config)
}

// 開啟檔案，錯誤訊息附上路徑以便判斷是哪個檔案出問題
fn open_file(path: &str) -> Result<File, Error> {
    File::open(path).map_err(|e| Error::new(e.kind(), format!("無法開啟 {}: {}", path, e)))
}

// 讀取第一把私鑰，接受 PKCS#8、PKCS#1（RSA）與 SEC1（EC）格式
fn read_private_key(reader: &mut dyn BufRead) -> Result<Option<PrivateKey>, Error> {
    while let Some(item) = rustls_pemfile::read_one(reader)? {
        match item {
            Item::PKCS8Key(key) | Item::RSAKey(key) | Item::ECKey(key) => return Ok(Some(PrivateKey(key))),
            _ => {}
        }
    }
    Ok(None)
}
