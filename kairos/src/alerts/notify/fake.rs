//! 測試用的假伺服器：記錄請求並依腳本回應的 HTTP 伺服器（Webhook）、支援 STARTTLS 與 AUTH 的 SMTP 伺服器。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

/// 收到的 HTTP 請求
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub head: String,
    pub body: String,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    }
}

/// 回應的腳本：（狀態碼, Retry-After）
type Script = Arc<Mutex<VecDeque<(u16, Option<u64>)>>>;

/// 假的 HTTP 伺服器：依 `script` 的順序回應（狀態碼, Retry-After），腳本用完後回 200
pub struct FakeHttp {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<HttpRequest>>>,
    pub script: Script,
}

impl FakeHttp {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<HttpRequest>>> = Arc::default();
        let script: Script = Arc::default();
        let (r, sc) = (requests.clone(), script.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let (r, sc) = (r.clone(), sc.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p;
                        }
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    let mut body = buf[head_end + 4..].to_vec();
                    while body.len() < len {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&chunk[..n]),
                        }
                    }
                    r.lock().unwrap().push(HttpRequest {
                        head,
                        body: String::from_utf8_lossy(&body).into_owned(),
                    });
                    let (code, retry) = sc.lock().unwrap().pop_front().unwrap_or((200, None));
                    let mut resp =
                        format!("HTTP/1.1 {code} X\r\nContent-Length: 2\r\nConnection: close\r\n");
                    if let Some(r) = retry {
                        resp.push_str(&format!("Retry-After: {r}\r\n"));
                    }
                    resp.push_str("\r\nok");
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Self {
            addr,
            requests,
            script,
        }
    }

    pub fn push(&self, code: u16, retry: Option<u64>) {
        self.script.lock().unwrap().push_back((code, retry));
    }

    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub fn last(&self) -> Option<HttpRequest> {
        self.requests.lock().unwrap().last().cloned()
    }
}

/// 假的 SMTP 伺服器（STARTTLS 使用 `pulse/testdata/server.pem`，SAN 為 localhost）。
/// `password` 為接受的密碼；收到的郵件內容記在 `mails`
pub struct FakeSmtp {
    pub addr: SocketAddr,
    pub mails: Arc<Mutex<Vec<String>>>,
    pub auth: Arc<Mutex<Vec<String>>>,
}

fn acceptor() -> TlsAcceptor {
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(format!("{MANIFEST}/../pulse/testdata/server.pem"))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    let key =
        PrivateKeyDer::from_pem_file(format!("{MANIFEST}/../pulse/testdata/server.key")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

impl FakeSmtp {
    pub async fn start(password: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mails: Arc<Mutex<Vec<String>>> = Arc::default();
        let auth: Arc<Mutex<Vec<String>>> = Arc::default();
        let (m, a) = (mails.clone(), auth.clone());
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let (m, a) = (m.clone(), a.clone());
                tokio::spawn(async move {
                    let mut plain = BufReader::new(sock);
                    let _ = plain.get_mut().write_all(b"220 fake ESMTP\r\n").await;
                    let mut line = String::new();
                    // STARTTLS 之前
                    loop {
                        line.clear();
                        if plain.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let cmd = line.trim().to_ascii_uppercase();
                        if cmd.starts_with("EHLO") {
                            let _ = plain
                                .get_mut()
                                .write_all(b"250-fake\r\n250-STARTTLS\r\n250 AUTH PLAIN LOGIN\r\n")
                                .await;
                        } else if cmd == "STARTTLS" {
                            let _ = plain.get_mut().write_all(b"220 go ahead\r\n").await;
                            break;
                        } else {
                            let _ = plain
                                .get_mut()
                                .write_all(b"530 must issue STARTTLS first\r\n")
                                .await;
                        }
                    }
                    let Ok(tls) = acceptor().accept(plain.into_inner()).await else {
                        return;
                    };
                    let mut s = BufReader::new(tls);
                    loop {
                        line.clear();
                        if s.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let raw = line.trim().to_string();
                        let cmd = raw.to_ascii_uppercase();
                        let reply: &[u8] = if cmd.starts_with("EHLO") {
                            b"250-fake\r\n250 AUTH PLAIN LOGIN\r\n"
                        } else if let Some(token) = raw.strip_prefix("AUTH PLAIN ") {
                            a.lock().unwrap().push(token.to_string());
                            let expected = crate::alerts::notify::transport::base64(
                                format!("\0kairos\0{password}").as_bytes(),
                            );
                            if token == expected {
                                b"235 ok\r\n"
                            } else {
                                b"535 bad credentials\r\n"
                            }
                        } else if cmd.starts_with("MAIL FROM") || cmd.starts_with("RCPT TO") {
                            b"250 ok\r\n"
                        } else if cmd == "DATA" {
                            let _ = s.get_mut().write_all(b"354 send\r\n").await;
                            let mut data = String::new();
                            loop {
                                let mut l = String::new();
                                if s.read_line(&mut l).await.unwrap_or(0) == 0 {
                                    return;
                                }
                                if l == ".\r\n" {
                                    break;
                                }
                                data.push_str(&l);
                            }
                            m.lock().unwrap().push(data);
                            b"250 queued as 42\r\n"
                        } else if cmd == "QUIT" {
                            let _ = s.get_mut().write_all(b"221 bye\r\n").await;
                            return;
                        } else {
                            b"500 what\r\n"
                        };
                        let _ = s.get_mut().write_all(reply).await;
                    }
                });
            }
        });
        Self { addr, mails, auth }
    }
}
