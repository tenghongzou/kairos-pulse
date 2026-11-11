//! 從外部 URL 下載發佈檔（`artifact.source = "url"`；REMOTE.md 第 4.1.2 節，0.10.0）。
//!
//! - 只在 URL 符合**本機** `upgrade.url_allowlist` 的前綴時下載（[`url_allowed`]）；白名單預設為空，
//!   也就是預設只接受從 Kairos 下載。理由：不讓被入侵的 Kairos 把主機變成對任意位址（含內網）發出請求的工具。
//! - HTTPS（rustls，TLS 1.2 以上），信任 `upgrade.external_ca`（空字串時為作業系統信任庫）；**不跟隨任何
//!   重新導向**（規格只禁止跨主機的重新導向；一律不跟隨比較單純，也不會被導向白名單以外的位址）。
//! - 大小以提議的 `artifact.size` 為上限；斷線後以 `Range` 續傳；沒有 HTTP 代理設定。
//! - **信任不來自這條連線**：下載的檔案與從 Kairos 下載的一樣，要符合已由本機信任錨驗證簽章的
//!   `SHA256SUMS` 中的雜湊（下載前先驗證簽章、下載後比對 SHA-256，輔助程式再驗證一次），這裡只負責搬資料。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::rustls::pki_types::ServerName;

pub use kairos_proto::mgmt::url::{parse_https_url, url_allowed, url_names_file};

/// 建立連線（TCP＋TLS）的期限
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// 等待回應標頭、或兩次收到資料之間的期限
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 每收到這麼多 bytes 回報一次進度
const PROGRESS_EVERY: u64 = 1024 * 1024;

/// 要下載的一個檔案
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlJob {
    pub upgrade_id: String,
    pub filename: String,
    pub url: String,
    /// 提議的大小（上限，也是完成的條件）
    pub size: u64,
    /// 暫存檔（`<檔名>.part`）
    pub part: PathBuf,
}

/// 下載失敗的原因（`detail` 與日誌；不含秘密）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// URL 不合法或不在白名單（不重試）
    NotAllowed(String),
    /// 連線、TLS、HTTP 或寫檔失敗（稍後重試、續傳）
    Transient(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::NotAllowed(s) | FetchError::Transient(s) => f.write_str(s),
        }
    }
}

/// 外部 URL 的 TLS 設定：信任 `external_ca`（PEM，可含多張），空字串時為作業系統信任庫；不送用戶端憑證
/// （Pulse 的 mTLS 憑證只用於 Kairos）
pub fn client_config(external_ca: &str) -> Result<Arc<ClientConfig>, String> {
    let roots = crate::tls::load_roots_for(external_ca.trim(), "upgrade.external_ca")?;
    let cfg = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("TLS 設定失敗：{e}"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(cfg))
}

fn transient(what: &str, e: impl std::fmt::Display) -> FetchError {
    FetchError::Transient(format!("{what}：{e}"))
}

async fn timed<T>(
    d: Duration,
    what: &str,
    f: impl Future<Output = io::Result<T>>,
) -> Result<T, FetchError> {
    match tokio::time::timeout(d, f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(transient(what, e)),
        Err(_) => Err(FetchError::Transient(format!(
            "{what}：{} 秒內沒有回應",
            d.as_secs()
        ))),
    }
}

/// 下載 `job.url` 到 `job.part`（已有部分內容時以 `Range` 續傳），完成時暫存檔剛好 `job.size` bytes。
/// 雜湊由呼叫端檢查。`connect_to`：測試時改連到本機的位址（TLS 的主機名稱仍以 URL 為準）
pub async fn download(
    job: &UrlJob,
    allowlist: &[String],
    tls: Arc<ClientConfig>,
    connect_to: Option<std::net::SocketAddr>,
    on_progress: &(dyn Fn() + Send + Sync),
) -> Result<(), FetchError> {
    if !url_allowed(&job.url, allowlist) {
        return Err(FetchError::NotAllowed("url_not_allowed".to_string()));
    }
    let url = parse_https_url(&job.url).map_err(FetchError::NotAllowed)?;
    let mut have = tokio::fs::metadata(&job.part).await.map_or(0, |m| m.len());
    if have > job.size {
        // 暫存檔比提議的大小還大：不可信，從頭來
        let _ = tokio::fs::remove_file(&job.part).await;
        have = 0;
    }
    if have == job.size {
        return Ok(());
    }
    let server_name = ServerName::try_from(url.host.clone())
        .map_err(|_| FetchError::NotAllowed("主機名稱不能作為 TLS 的伺服器名稱".to_string()))?;
    let tcp = match connect_to {
        Some(a) => timed(CONNECT_TIMEOUT, "連線", TcpStream::connect(a)).await?,
        None => {
            timed(
                CONNECT_TIMEOUT,
                "連線",
                TcpStream::connect((url.host.as_str(), url.port)),
            )
            .await?
        }
    };
    let _ = tcp.set_nodelay(true);
    let stream = timed(
        CONNECT_TIMEOUT,
        "TLS 交握",
        TlsConnector::from(tls).connect(server_name, tcp),
    )
    .await?;
    let mut io = BufReader::new(stream);
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: pulse/{}\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
        url.path,
        url.authority(),
        env!("CARGO_PKG_VERSION")
    );
    if have > 0 {
        req.push_str(&format!("Range: bytes={have}-\r\n"));
    }
    req.push_str("\r\n");
    timed(IDLE_TIMEOUT, "送出請求", async {
        io.get_mut().write_all(req.as_bytes()).await?;
        io.get_mut().flush().await
    })
    .await?;
    let head = timed(
        IDLE_TIMEOUT,
        "讀取回應",
        crate::enroll::http::read_response(&mut io, true),
    )
    .await?;
    let content_length = head
        .header("content-length")
        .and_then(|v| v.trim().parse::<u64>().ok());
    if head
        .header("transfer-encoding")
        .is_some_and(|v| !v.trim().eq_ignore_ascii_case("identity"))
        || head
            .header("content-encoding")
            .is_some_and(|v| !v.trim().eq_ignore_ascii_case("identity"))
    {
        return Err(FetchError::Transient(
            "伺服器以 Transfer-Encoding／Content-Encoding 傳送，不支援（請直接提供檔案）"
                .to_string(),
        ));
    }
    let append = match head.status {
        200 => false,
        206 => {
            // 必須從要求的位置開始、到檔案結尾
            let ok = head.header("content-range").is_some_and(|v| {
                v.trim()
                    .strip_prefix("bytes ")
                    .and_then(|r| r.split_once('/'))
                    .and_then(|(range, total)| {
                        let (a, b) = range.split_once('-')?;
                        Some((
                            a.trim().parse::<u64>().ok()?,
                            b.trim().parse::<u64>().ok()?,
                            total.trim().parse::<u64>().ok()?,
                        ))
                    })
                    .is_some_and(|(a, b, total)| a == have && total == job.size && b + 1 == total)
            });
            if !ok || have == 0 {
                let _ = tokio::fs::remove_file(&job.part).await;
                return Err(FetchError::Transient(
                    "續傳的 Content-Range 與預期不符，從頭重新下載".to_string(),
                ));
            }
            true
        }
        416 => {
            let _ = tokio::fs::remove_file(&job.part).await;
            return Err(FetchError::Transient(
                "伺服器拒絕續傳的範圍（416），從頭重新下載".to_string(),
            ));
        }
        s @ 300..=399 => {
            return Err(FetchError::Transient(format!(
                "伺服器回應重新導向（HTTP {s}）；不跟隨重新導向，請把 base_url 設為最終的位址"
            )));
        }
        s => return Err(FetchError::Transient(format!("HTTP {s}"))),
    };
    let start = if append { have } else { 0 };
    let expect = job.size - start;
    if content_length.is_some_and(|n| n != expect) {
        return Err(FetchError::Transient(format!(
            "檔案大小與提議不符（Content-Length {}，預期 {expect}）",
            content_length.unwrap_or(0)
        )));
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&job.part)
        .await
        .map_err(|e| transient("無法開啟暫存檔", e))?;
    let mut written = 0u64;
    let mut since_progress = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    while written < expect {
        let want = usize::try_from((expect - written).min(buf.len() as u64)).unwrap_or(buf.len());
        let n = timed(IDLE_TIMEOUT, "下載", io.read(&mut buf[..want])).await?;
        if n == 0 {
            let _ = file.flush().await;
            return Err(FetchError::Transient(format!(
                "連線在 {} / {} bytes 時中斷",
                start + written,
                job.size
            )));
        }
        file.write_all(&buf[..n])
            .await
            .map_err(|e| transient("無法寫入暫存檔", e))?;
        written += n as u64;
        since_progress += n as u64;
        if since_progress >= PROGRESS_EVERY {
            since_progress = 0;
            let _ = file.flush().await;
            on_progress();
        }
    }
    // 沒有 Content-Length 的回應：超過提議的大小即不符（多讀 1 byte 確認已到結尾）
    if content_length.is_none() {
        let mut one = [0u8; 1];
        if timed(IDLE_TIMEOUT, "下載", io.read(&mut one)).await? != 0 {
            drop(file);
            let _ = tokio::fs::remove_file(&job.part).await;
            return Err(FetchError::Transient("檔案比提議的大小還大".to_string()));
        }
    }
    file.flush()
        .await
        .map_err(|e| transient("無法寫入暫存檔", e))?;
    file.sync_all()
        .await
        .map_err(|e| transient("無法寫入暫存檔", e))?;
    Ok(())
}

/// 暫存檔的路徑
pub fn part_path(dir: &Path, filename: &str) -> PathBuf {
    dir.join(format!("{filename}.part"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

    fn list(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    struct TestServer {
        addr: std::net::SocketAddr,
        ca_pem: String,
        requests: Arc<std::sync::Mutex<Vec<String>>>,
        hits: Arc<AtomicUsize>,
    }

    /// 本機的 HTTPS 伺服器（rcgen 的 CA 與 `repo.test` 的伺服器憑證）：`respond(請求的標頭) → 回應的 bytes`
    async fn server(respond: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> TestServer {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["repo.test".to_string()])
            .unwrap()
            .signed_by(&key, &rcgen::Issuer::from_params(&ca_params, &ca_key))
            .unwrap();
        let cfg = ServerConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.der().to_vec())],
            PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        )
        .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(cfg));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let (reqs, h) = (requests.clone(), hits.clone());
        let respond = Arc::new(respond);
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let (acceptor, reqs, h, respond) =
                    (acceptor.clone(), reqs.clone(), h.clone(), respond.clone());
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut head = Vec::new();
                    let mut b = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        match tls.read(&mut b).await {
                            Ok(1) => head.push(b[0]),
                            _ => return,
                        }
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    h.fetch_add(1, Ordering::SeqCst);
                    reqs.lock().unwrap().push(head.clone());
                    let _ = tls.write_all(&respond(&head)).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        TestServer {
            addr,
            ca_pem: ca.pem(),
            requests,
            hits,
        }
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pulse-fetch-{tag}-{}-{}",
            std::process::id(),
            crate::backoff::random_spread(Duration::from_secs(1_000_000)).as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn job(dir: &Path, size: u64) -> UrlJob {
        UrlJob {
            upgrade_id: "u".into(),
            filename: "pulse.tar.gz".into(),
            url: "https://repo.test/kairos-pulse/0.10.0/pulse.tar.gz".into(),
            size,
            part: part_path(dir, "pulse.tar.gz"),
        }
    }

    fn tls_for(dir: &Path, ca_pem: &str) -> Arc<ClientConfig> {
        let ca = dir.join("ca.pem");
        std::fs::write(&ca, ca_pem).unwrap();
        client_config(&ca.to_string_lossy()).unwrap()
    }

    const ALLOW: &str = "https://repo.test/kairos-pulse/";

    #[tokio::test]
    async fn downloads_over_https_with_the_external_ca_and_resumes_with_range() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let body = data.clone();
        let s = server(move |head| {
            match head
                .lines()
                .find_map(|l| l.strip_prefix("Range: bytes="))
                .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok())
            {
                Some(from) => {
                    let mut r = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{}\r\n\r\n",
                        body.len() - from,
                        body.len() - 1,
                        body.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(&body[from..]);
                    r
                }
                None => ok(&body),
            }
        })
        .await;
        let dir = temp("ok");
        let tls = tls_for(&dir, &s.ca_pem);
        let j = job(&dir, data.len() as u64);
        let allow = list(&[ALLOW]);
        download(&j, &allow, tls.clone(), Some(s.addr), &|| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&j.part).unwrap(), data);
        let first = s.requests.lock().unwrap()[0].clone();
        assert!(
            first.starts_with("GET /kairos-pulse/0.10.0/pulse.tar.gz HTTP/1.1\r\n"),
            "{first}"
        );
        assert!(first.contains("\r\nHost: repo.test\r\n") && !first.contains("Range:"));
        // 已完整：不再連線
        download(&j, &allow, tls.clone(), Some(s.addr), &|| {})
            .await
            .unwrap();
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        // 續傳：暫存檔只有前 1000 bytes
        std::fs::write(&j.part, &data[..1000]).unwrap();
        download(&j, &allow, tls, Some(s.addr), &|| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&j.part).unwrap(), data);
        assert!(s.requests.lock().unwrap()[1].contains("\r\nRange: bytes=1000-\r\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refuses_urls_outside_the_allowlist_untrusted_servers_redirects_and_wrong_sizes() {
        let s = server(|head| {
            if head.starts_with("GET /kairos-pulse/0.10.0/redirect") {
                b"HTTP/1.1 302 Found\r\nLocation: https://evil.test/x\r\nContent-Length: 0\r\n\r\n"
                    .to_vec()
            } else if head.starts_with("GET /kairos-pulse/0.10.0/big") {
                // 沒有 Content-Length、比提議的大
                let mut r = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
                r.extend_from_slice(&[7u8; 64]);
                r
            } else if head.starts_with("GET /kairos-pulse/0.10.0/short") {
                let mut r = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
                r.extend_from_slice(&[7u8; 8]);
                r
            } else if head.starts_with("GET /kairos-pulse/0.10.0/missing") {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
            } else {
                ok(&[7u8; 64])
            }
        })
        .await;
        let dir = temp("bad");
        let tls = tls_for(&dir, &s.ca_pem);
        let allow = list(&[ALLOW]);
        // 白名單以外：不連線
        let mut j = job(&dir, 32);
        j.url = "https://repo.test/other/pulse.tar.gz".into();
        let e = download(&j, &allow, tls.clone(), Some(s.addr), &|| {})
            .await
            .unwrap_err();
        assert_eq!(e, FetchError::NotAllowed("url_not_allowed".into()));
        let e = download(&job(&dir, 32), &[], tls.clone(), Some(s.addr), &|| {})
            .await
            .unwrap_err();
        assert!(matches!(e, FetchError::NotAllowed(_)));
        assert_eq!(
            s.hits.load(Ordering::SeqCst),
            0,
            "不在白名單的 URL 不應該連線"
        );
        // 大小不符（Content-Length 64，提議 32）
        let e = download(&job(&dir, 32), &allow, tls.clone(), Some(s.addr), &|| {})
            .await
            .unwrap_err();
        assert!(
            matches!(&e, FetchError::Transient(m) if m.contains("大小")),
            "{e}"
        );
        // 重新導向、404、比提議大、提早結束
        for (name, needle) in [
            ("redirect", "重新導向"),
            ("missing", "HTTP 404"),
            ("big", "還大"),
            ("short", "中斷"),
        ] {
            let mut j = job(&dir, 32);
            let _ = std::fs::remove_file(&j.part);
            j.url = format!("https://repo.test/kairos-pulse/0.10.0/{name}");
            let e = download(&j, &allow, tls.clone(), Some(s.addr), &|| {})
                .await
                .unwrap_err();
            assert!(
                matches!(&e, FetchError::Transient(m) if m.contains(needle)),
                "{name}: {e}"
            );
        }
        // 不信任的伺服器憑證（另一個 CA）：TLS 交握失敗，沒有送出請求
        let other = server(|_| ok(&[7u8; 32])).await;
        let before = other.hits.load(Ordering::SeqCst);
        let e = download(&job(&dir, 32), &allow, tls, Some(other.addr), &|| {})
            .await
            .unwrap_err();
        assert!(
            matches!(&e, FetchError::Transient(m) if m.contains("TLS")),
            "{e}"
        );
        assert_eq!(other.hits.load(Ordering::SeqCst), before);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
