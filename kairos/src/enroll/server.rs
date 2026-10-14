//! 8444 的 listener（`crates/kairos-proto/ENROLL.md` 第 2、10、11 節）。
//!
//! - **模式切換**：模式為關閉（或缺少 `pki.server_ca_path` 等前提）時不監聽；改為開放或僅續簽時重新
//!   監聽，不必重新啟動。改為關閉時停止接受新連線，進行中的請求（含長輪詢）在 5 秒內結束並關閉。
//! - **TLS**：只做伺服器端 TLS，ALPN 只有 `http/1.1`；憑證鏈中沒有被釘選的 CA（`pki.server_ca_path`）
//!   時把它接在鏈尾（8443 不受影響）。憑證檔改變時重新載入。
//! - **連線層的限制**（L1～L3）在 TLS 握手前檢查：超過時直接關閉 TCP（Pulse 視為 `PLS-E2201`，可重試）；
//!   只超過 L3 軟上限的連線完成握手後，請求一律 429（`scope: ip`）。

use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use kairos_proto::enroll::{EnrollMode, normalize_endpoint};
use log::{debug, error, info, warn};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_util::sync::CancellationToken;

use super::EnrollService;
use super::handlers::{self, ConnInfo};
use crate::per_ip::{address_key, source_key};
use crate::pki::x509::parse_cert;
use crate::tls::{Fingerprint, TlsPaths, fingerprint};

/// 請求行與標頭合計的上限
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
/// 模式改為關閉時，進行中的連線最多再等這麼久
pub const CLOSE_GRACE: Duration = Duration::from_secs(5);
/// 簽章內容可接受的註冊端點名稱上限
pub const MAX_ENDPOINT_NAMES: usize = 32;
/// L3 硬上限＝軟上限的 4 倍
pub const HARD_MULTIPLIER: f64 = 4.0;

struct Active {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

/// 8444 的 listener
pub struct Listener {
    /// 8443、主控台的監聽位址（`KRS-E0202` 的檢查）
    other: Vec<SocketAddr>,
    active: tokio::sync::Mutex<Option<Active>>,
    acceptor: Arc<RwLock<Option<TlsAcceptor>>>,
    tls_paths: TlsPaths,
    tls_fingerprint: Mutex<Option<Fingerprint>>,
    last_reason: Mutex<Option<String>>,
}

/// 8444 的 TLS 設定：伺服器憑證（必要時接上被釘選的 CA）、只有 http/1.1、不要求用戶端憑證
pub fn build_acceptor(cert: &str, key: &str, server_ca_der: &[u8]) -> Result<TlsAcceptor, String> {
    let mut certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("無法讀取註冊端點的憑證 {cert}：{e}"))?;
    if certs.is_empty() {
        return Err(format!("{cert} 中找不到註冊端點的憑證"));
    }
    let ca_spki = parse_cert(server_ca_der)
        .map(|i| i.spki_der)
        .map_err(|e| format!("pki.server_ca_path：{e}"))?;
    let has_ca = certs
        .iter()
        .any(|c| parse_cert(c.as_ref()).is_ok_and(|i| i.spki_der == ca_spki));
    if !has_ca {
        certs.push(CertificateDer::from(server_ca_der.to_vec()));
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|e| format!("無法讀取註冊端點的私鑰 {key}：{e}"))?;
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("註冊端點的憑證與私鑰無法組成 TLS 設定：{e}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// 簽章內容可接受的註冊端點名稱（ENROLL.md 第 16.3 節）：`enroll.advertise`、`enroll.listen`
/// （不是萬用位址時）、8444 伺服器憑證 SAN 中的每個 DNS 名稱與 IP，連接埠為實際監聽的連接埠
pub fn endpoint_names(advertise: &str, listen: &SocketAddr, cert_path: &str) -> Vec<String> {
    let port = listen.port();
    let mut names = Vec::new();
    let mut push = |n: String| {
        if let Ok(n) = normalize_endpoint(&n)
            && !names.contains(&n)
            && names.len() < MAX_ENDPOINT_NAMES
        {
            names.push(n);
        }
    };
    if !advertise.trim().is_empty() {
        push(advertise.trim().to_string());
        if let Some((host, _)) = advertise.trim().rsplit_once(':') {
            push(format!("{host}:{port}"));
        }
    }
    let host_port = |ip: std::net::IpAddr| match ip {
        std::net::IpAddr::V4(v4) => format!("{v4}:{port}"),
        std::net::IpAddr::V6(v6) => format!("[{v6}]:{port}"),
    };
    if !listen.ip().is_unspecified() {
        push(host_port(listen.ip()));
    }
    if let Ok(certs) =
        CertificateDer::pem_file_iter(cert_path).and_then(|it| it.collect::<Result<Vec<_>, _>>())
        && let Some(first) = certs.first()
        && let Ok(info) = parse_cert(first.as_ref())
    {
        for d in info.dns_sans {
            push(format!("{d}:{port}"));
        }
        for ip in info.ip_sans {
            push(host_port(ip));
        }
    }
    names
}

impl Listener {
    pub fn new(svc: &EnrollService, other: Vec<SocketAddr>) -> Result<Self, String> {
        let (cert, key) = svc.config.enroll.tls_files(&svc.config.server);
        let mut paths = TlsPaths {
            cert,
            key,
            client_ca: None,
            crls: Vec::new(),
            issuing_cas: None,
            issuing_crl: None,
        };
        if let Some(ca) = svc.config.pki.server_ca() {
            // server CA 的變化也要重新載入：放在 client_ca 的位置只為了納入指紋
            paths.client_ca = Some(ca.to_string());
        }
        Ok(Self {
            other,
            active: tokio::sync::Mutex::new(None),
            acceptor: Arc::new(RwLock::new(None)),
            tls_paths: paths,
            tls_fingerprint: Mutex::new(None),
            last_reason: Mutex::new(None),
        })
    }

    /// 憑證檔有變化時重新建立 TLS 設定；失敗時保留舊的並記 ERROR
    fn refresh_tls(&self, svc: &EnrollService) {
        let fp = fingerprint(&self.tls_paths);
        let mut last = self
            .tls_fingerprint
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if last.as_ref() == Some(&fp) {
            return;
        }
        let first = last.is_none();
        *last = Some(fp);
        // 被釘選的 CA 也可能更新
        if let Some(path) = svc.config.pki.server_ca() {
            match super::pki_mgr::load_server_ca(path) {
                Ok(ca) => *svc.pki.server_ca.write().unwrap_or_else(|e| e.into_inner()) = Some(ca),
                Err(e) => error!("pki.server_ca_path 無法載入：{e}"),
            }
        }
        let Some(ca) = svc.pki.server_ca() else {
            return;
        };
        match build_acceptor(&self.tls_paths.cert, &self.tls_paths.key, &ca.der) {
            Ok(a) => {
                *self.acceptor.write().unwrap_or_else(|e| e.into_inner()) = Some(a);
                if !first {
                    info!("註冊端點的憑證已重新載入（{}）", self.tls_paths.cert);
                }
            }
            Err(e) => error!("註冊端點的 TLS 設定無法建立（繼續使用舊的設定）：{e}"),
        }
    }

    fn set_reason(&self, svc: &EnrollService, reason: Option<String>, addr: Option<String>) {
        let mut st = svc.listen.lock().unwrap_or_else(|e| e.into_inner());
        st.listening = reason.is_none() && addr.is_some();
        st.addr = addr;
        st.reason = reason.clone();
        drop(st);
        let mut last = self.last_reason.lock().unwrap_or_else(|e| e.into_inner());
        if *last != reason {
            if let Some(r) = &reason {
                info!("註冊端點（8444）未監聽：{r}");
            }
            *last = reason;
        }
    }

    /// 依模式與前提監聽或停止（背景工作每 2 秒呼叫）
    pub async fn reconcile(&self, svc: &Arc<EnrollService>) {
        self.refresh_tls(svc);
        let mode = svc.effective().mode;
        let want = if mode == EnrollMode::Closed {
            Err("模式為關閉（enroll.mode = \"closed\"）".to_string())
        } else {
            svc.prerequisites()
        };
        let want = want.and_then(|()| {
            self.acceptor
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .ok_or_else(|| "註冊端點的 TLS 設定無法建立（見 ERROR 日誌）".to_string())
        });
        let mut active = self.active.lock().await;
        match (want, active.is_some()) {
            (Ok(_), true) => {}
            (Err(reason), false) => self.set_reason(svc, Some(reason), None),
            (Err(reason), true) => {
                if let Some(a) = active.take() {
                    info!("註冊端點（8444）停止監聽：{reason}；進行中的請求在 5 秒內結束");
                    a.cancel.cancel();
                    let _ = a.task.await;
                }
                self.set_reason(svc, Some(reason), None);
            }
            (Ok(acceptor), false) => match self.bind(svc).await {
                Ok((listener, addr)) => {
                    let names =
                        endpoint_names(&svc.config.enroll.advertise, &addr, &self.tls_paths.cert);
                    svc.set_endpoint_names(names.clone());
                    let cancel = CancellationToken::new();
                    let task = tokio::spawn(accept_loop(
                        svc.clone(),
                        listener,
                        self.acceptor.clone(),
                        cancel.clone(),
                    ));
                    info!(
                        "註冊端點（8444）已啟動，監聽 https://{addr}/v1/enroll/（模式 {}，{}，連線上限 {}，簽章名稱 {}）",
                        svc.mode_state().as_str(),
                        super::describe_pki(svc.pki.mode()),
                        svc.config.enroll.max_connections,
                        names.join("、")
                    );
                    let _ = acceptor;
                    *active = Some(Active { cancel, task });
                    self.set_reason(svc, None, Some(addr.to_string()));
                }
                Err(reason) => self.set_reason(svc, Some(reason), None),
            },
        }
    }

    async fn bind(&self, svc: &EnrollService) -> Result<(TcpListener, SocketAddr), String> {
        let listen = svc.config.enroll.listen_addr(&svc.config.server.listen);
        let resolved: Vec<SocketAddr> = tokio::net::lookup_host(&listen)
            .await
            .map_err(|e| format!("無法解析 enroll.listen {listen}：{e}"))?
            .collect();
        for a in &resolved {
            if let Some(o) = self
                .other
                .iter()
                .find(|o| crate::config::listen_addrs_conflict(a, o))
            {
                let e = crate::config::listen_conflict_between(
                    "enroll.listen",
                    &listen,
                    "server.listen／console.listen",
                    &o.to_string(),
                );
                error!("{e}");
                return Err(e.to_string());
            }
        }
        let listener = TcpListener::bind(listen.trim()).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                format!("錯誤 KRS-E2001：無法監聽 {listen}：連接埠已被其他程式佔用（{e}）；應在監聽但未監聽，每 2 秒重試")
            } else {
                format!("無法監聽 {listen}：{e}")
            }
        })?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        Ok((listener, addr))
    }

    /// 停止監聽（關機）
    pub async fn stop(&self) {
        if let Some(a) = self.active.lock().await.take() {
            a.cancel.cancel();
            let _ = a.task.await;
        }
    }

    /// 實際監聽的位址
    pub fn local_addr(svc: &EnrollService) -> Option<String> {
        svc.listen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .addr
            .clone()
    }
}

/// accept 迴圈：L1～L3 在 TLS 握手前檢查，每條連線以 hyper 的 HTTP/1.1 處理
async fn accept_loop(
    svc: Arc<EnrollService>,
    listener: TcpListener,
    acceptor: Arc<RwLock<Option<TlsAcceptor>>>,
    cancel: CancellationToken,
) {
    let c = &svc.config.enroll;
    let limit = Arc::new(Semaphore::new(c.max_connections));
    let handshakes = Arc::new(Semaphore::new((c.max_connections / 2).max(1)));
    let mut conns: JoinSet<()> = JoinSet::new();
    let header_timeout =
        Duration::from_secs(u64::from(c.header_timeout_secs.min(c.idle_timeout_secs)));
    let handshake_timeout = svc.config.timeouts.handshake();
    loop {
        while conns.try_join_next().is_some() {}
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            p = limit.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
        };
        let (socket, peer) = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    debug!("註冊端點 accept 失敗：{e}");
                    drop(permit);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let now = Instant::now();
        let ip = peer.ip().to_canonical();
        let m = svc.multiplier(ip);
        let drop_conn = |what: &str| {
            svc.stats
                .pre_handshake_closed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            debug!("註冊端點：在 TLS 握手前關閉來自 {peer} 的連線（{what}）");
        };
        // L2：每個位址的同時連線
        let Some(addr_slot) = svc.limits.conn_per_addr.try_acquire(
            address_key(ip),
            ((c.max_connections_per_ip as f64) * m) as usize,
            usize::MAX,
        ) else {
            drop_conn("L2 每 IP 同時連線");
            continue;
        };
        // L1：握手中的連線數、全域新連線速率
        let Ok(handshake) = handshakes.clone().try_acquire_owned() else {
            drop_conn("L1 握手中的連線數");
            continue;
        };
        if svc
            .limits
            .conn_global
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take(now, Duration::ZERO)
            .is_err()
        {
            warn!(
                "警告 KRS-W2205：註冊端點的新連線超過全域上限（每秒 {} 條，可連續 {} 條），在 TLS 握手前關閉",
                c.max_new_connections_per_sec, c.max_new_connections_burst
            );
            drop_conn("L1 全域新連線速率");
            continue;
        }
        // L3：硬上限（4 倍）握手前關閉；軟上限完成握手後回 429
        let src = source_key(ip);
        let soft_burst = f64::from(c.max_new_connections_per_ip_burst) * m;
        let soft_rate = f64::from(c.max_new_connections_per_ip_per_sec) * m;
        if svc
            .limits
            .conn_hard
            .take(
                &src,
                soft_burst * HARD_MULTIPLIER,
                soft_rate * HARD_MULTIPLIER,
                now,
                Duration::ZERO,
            )
            .is_err()
        {
            drop_conn("L3 每 IP 新連線（硬上限）");
            continue;
        }
        let soft_limited = svc
            .limits
            .conn_soft
            .take(
                &src,
                soft_burst,
                soft_rate,
                now,
                Duration::from_secs(u64::from(c.retry_after_max_secs)),
            )
            .err();
        let Some(acceptor) = acceptor.read().unwrap_or_else(|e| e.into_inner()).clone() else {
            continue;
        };
        let svc2 = svc.clone();
        conns.spawn(async move {
            let _permit = permit;
            let _addr_slot = addr_slot;
            let tls = match tokio::time::timeout(handshake_timeout, acceptor.accept(socket)).await {
                Ok(Ok(tls)) => tls,
                Ok(Err(e)) => {
                    debug!("註冊端點：{peer} 的 TLS 握手失敗：{e}");
                    return;
                }
                Err(_) => {
                    debug!("註冊端點：{peer} 的 TLS 握手逾時");
                    return;
                }
            };
            drop(handshake);
            let info = Arc::new(ConnInfo { peer, soft_limited });
            let service = service_fn(move |req| {
                let svc = svc2.clone();
                let info = info.clone();
                async move { Ok::<_, std::convert::Infallible>(handlers::handle(svc, info, req).await) }
            });
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(header_timeout)
                .max_buf_size(MAX_HEADER_BYTES)
                .keep_alive(true);
            if let Err(e) = builder.serve_connection(TokioIo::new(tls), service).await {
                debug!("註冊端點：與 {peer} 的連線結束：{e}");
            }
        });
    }
    drop(listener);
    // 停止監聽：進行中的連線最多再等 5 秒
    let drained = tokio::time::timeout(CLOSE_GRACE, async {
        while conns.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        conns.abort_all();
        while conns.join_next().await.is_some() {}
    }
}
