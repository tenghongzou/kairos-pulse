//! 主控台 API（審計 F1；規格見 `kairos/API.md`）。
//!
//! HTTPS（rustls）＋ hyper 的 HTTP/1.1 伺服器，監聽 `console.listen`（預設 127.0.0.1:9443），與
//! agent 連線埠的名額、TLS 設定與連線處理完全分開。所有端點唯讀：
//!
//! | 模組 | 內容 |
//! |---|---|
//! | [`http`] | 回應本文、JSON 與錯誤格式、查詢參數 |
//! | [`auth`]、[`tokens`] | API token 的驗證與保存（`kairos api-token` 共用） |
//! | [`db`] | 唯讀連線池（WAL 讀者）與低頻寫入的連線 |
//! | [`hosts`] | 主機清冊：列表與詳情 |
//! | [`metrics`] | 指標趨勢 |
//! | [`overview`]、[`health`] | 總覽、系統健康 |
//! | [`events`] | 即時變更推送（SSE） |
//! | [`maint`] | 背景工作：每分鐘上線數、token 的最後使用與稽核日誌、被拒的憑證寫入清冊 |

pub mod auth;
pub mod db;
pub mod events;
pub mod health;
pub mod hosts;
pub mod http;
mod maint;
pub mod metrics;
pub mod overview;
pub mod tokens;

#[cfg(test)]
mod tests;

use chrono::{DateTime, Utc};
use hyper::body::Incoming;
use hyper::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use log::{debug, info, warn};
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::AppConfig;
use crate::per_ip::PerIpLimiter;
use crate::registry::Registry;
use crate::shutdown::Shutdown;
use crate::stats::WriteStats;
use crate::store::sqlite::SqliteOptions;
use crate::tls::{TlsPaths, TlsState};
use auth::{Authenticator, Permission, Principal};
use db::{ReadPool, WriteConn};
use events::EventHub;
use hosts::{HostIndex, MergeEnv};
use http::{ApiBody, ApiError, Query};
use overview::MinuteSeries;

/// TLS 握手的上限
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 讀取請求標頭的上限（含 keep-alive 的閒置）
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// 每個請求的處理上限（SSE 除外）
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 請求行與標頭合計的上限
pub const MAX_HEADER_BYTES: usize = 16 * 1024;

/// 所有請求共用的狀態
pub struct ApiContext {
    pub config: Arc<AppConfig>,
    pub config_file: Option<PathBuf>,
    pub registry: Arc<Registry>,
    pub write_stats: Arc<WriteStats>,
    /// agent 連線埠的 TLS 設定（系統健康：憑證與 CRL 的到期、是否啟用 mTLS）
    pub agent_tls: Arc<TlsState>,
    pub per_ip: Option<Arc<PerIpLimiter>>,
    pub db_path: PathBuf,
    pub pool: Arc<ReadPool>,
    pub writer: Arc<WriteConn>,
    pub auth: Authenticator,
    pub hosts: HostIndex,
    pub events: Arc<EventHub>,
    pub minutes: Mutex<MinuteSeries>,
    pub streams: Arc<Semaphore>,
    pub max_streams: usize,
    pub shutdown: Arc<Shutdown>,
    pub started: DateTime<Utc>,
    pub started_i: Instant,
    pub agent_listen: SocketAddr,
    pub console_listen: SocketAddr,
}

impl ApiContext {
    pub fn merge_env(&self) -> MergeEnv {
        MergeEnv {
            now: Utc::now(),
            now_i: Instant::now(),
            started: self.started,
            uptime: self.started_i.elapsed(),
            mtls: self.agent_tls.current().auth_enabled(),
        }
    }
}

/// [`start`] 的參數
pub struct StartParams {
    pub config: Arc<AppConfig>,
    pub config_file: Option<PathBuf>,
    pub registry: Arc<Registry>,
    pub write_stats: Arc<WriteStats>,
    pub agent_tls: Arc<TlsState>,
    pub per_ip: Option<Arc<PerIpLimiter>>,
    pub data_dir: PathBuf,
    pub shutdown: Arc<Shutdown>,
    pub agent_listen: SocketAddr,
}

/// 啟動失敗的種類（決定結束代碼）
#[derive(Debug)]
pub enum StartError {
    /// 憑證或私鑰（結束代碼 4）
    Certificate(Error),
    /// 監聽失敗（連接埠被佔用為 11）
    Listen(Error),
    /// 其他（資料庫等）
    Other(Error),
}

/// 啟動中的主控台
pub struct Console {
    #[cfg_attr(not(test), allow(dead_code))]
    pub local_addr: SocketAddr,
    #[cfg_attr(not(test), allow(dead_code))]
    pub ctx: Arc<ApiContext>,
    task: tokio::task::JoinHandle<()>,
}

impl Console {
    /// 等待主控台結束（關機時：停止接受連線並中斷所有連線）
    pub async fn join(self) {
        let _ = self.task.await;
    }
}

/// 主控台的 TLS 設定：只做伺服器端 TLS（不要求用戶端憑證），ALPN 只有 http/1.1
pub fn build_acceptor(cert: &str, key: &str) -> Result<TlsAcceptor, Error> {
    let certs = CertificateDer::pem_file_iter(cert)
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("無法讀取主控台的憑證 {cert}: {e}"),
            )
        })?;
    if certs.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("{cert} 中找不到主控台的憑證"),
        ));
    }
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("無法讀取主控台的私鑰 {key}: {e}"),
        )
    })?;
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("主控台的憑證與私鑰無法組成 TLS 設定: {e}"),
            )
        })?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// 啟動主控台：建立 TLS 設定、監聽、開啟唯讀連線池，並啟動背景工作與 accept 迴圈
pub async fn start(p: StartParams) -> Result<Console, StartError> {
    let console = &p.config.console;
    let (cert, key) = console.tls_files(&p.config.server);
    let acceptor = build_acceptor(&cert, &key).map_err(StartError::Certificate)?;
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host(&console.listen)
        .await
        .map_err(|e| {
            StartError::Listen(Error::new(
                e.kind(),
                format!("無法解析 console.listen {}: {}", console.listen, e),
            ))
        })?
        .collect();
    if resolved
        .iter()
        .any(|a| crate::config::listen_addrs_conflict(a, &p.agent_listen))
    {
        return Err(StartError::Other(crate::config::listen_conflict_error(
            &p.agent_listen.to_string(),
            &console.listen,
        )));
    }
    let listener = TcpListener::bind(&console.listen).await.map_err(|e| {
        StartError::Listen(Error::new(
            e.kind(),
            format!("無法監聽主控台 console.listen {}: {}", console.listen, e),
        ))
    })?;
    let local_addr = listener.local_addr().map_err(StartError::Listen)?;

    let db_opts = SqliteOptions::from_config(&p.config, &p.data_dir);
    let db_path = db_opts.path.clone();
    let pool = ReadPool::new(db_path.clone());
    let known: Vec<String> = pool
        .read("主機清單", |conn| {
            let mut stmt = conn
                .prepare("SELECT agent_id FROM hosts")
                .map_err(db::query_failed("主機清單"))?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(db::query_failed("主機清單"))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db::query_failed("主機清單"))
        })
        .await
        .map_err(|e| StartError::Other(Error::other(e.message)))?;

    let started = Utc::now();
    let ctx = Arc::new(ApiContext {
        auth: Authenticator::new(pool.clone()),
        writer: WriteConn::new(db_opts),
        hosts: HostIndex::default(),
        events: EventHub::new(started.timestamp_millis()),
        minutes: Mutex::new(MinuteSeries::default()),
        streams: Arc::new(Semaphore::new(console.max_streams)),
        max_streams: console.max_streams,
        config: p.config.clone(),
        config_file: p.config_file,
        registry: p.registry.clone(),
        write_stats: p.write_stats,
        agent_tls: p.agent_tls,
        per_ip: p.per_ip,
        db_path,
        pool,
        shutdown: p.shutdown.clone(),
        started,
        started_i: Instant::now(),
        agent_listen: p.agent_listen,
        console_listen: local_addr,
    });
    ctx.hosts.load_known(known);

    // 狀態表的變化 → SSE 事件
    let index_ctx = ctx.clone();
    tokio::spawn(events::pump(
        p.registry.subscribe(),
        ctx.events.clone(),
        move |id| index_ctx.hosts.mark_known(id),
        p.shutdown.clone(),
    ));
    tokio::spawn(maint::run(ctx.clone()));
    let acceptor = Arc::new(RwLock::new(acceptor));
    if p.config.server.reload_interval_secs > 0 {
        tokio::spawn(reload_certificate(
            TlsPaths {
                cert: cert.clone(),
                key: key.clone(),
                client_ca: None,
                crls: Vec::new(),
            },
            acceptor.clone(),
            Duration::from_secs(p.config.server.reload_interval_secs),
            p.shutdown.clone(),
        ));
    }
    info!(
        "主控台 API 已啟動，監聽 https://{}/api/v1/（連線上限 {}，SSE 上限 {}，憑證 {}）",
        local_addr, console.max_connections, console.max_streams, cert
    );
    let task = tokio::spawn(accept_loop(
        listener,
        ctx.clone(),
        acceptor,
        console.max_connections,
    ));
    Ok(Console {
        local_addr,
        ctx,
        task,
    })
}

/// 主控台憑證的熱重載：內容改變時重新建立 TLS 設定；失敗時保留舊的並記 ERROR
async fn reload_certificate(
    paths: TlsPaths,
    acceptor: Arc<RwLock<TlsAcceptor>>,
    every: Duration,
    shutdown: Arc<Shutdown>,
) {
    let mut last = crate::tls::fingerprint(&paths);
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let current = crate::tls::fingerprint(&paths);
        if current == last {
            continue;
        }
        match build_acceptor(&paths.cert, &paths.key) {
            Ok(a) => {
                *acceptor.write().await = a;
                last = current;
                info!("主控台的憑證已重新載入（{}）", paths.cert);
            }
            Err(e) => log::error!("主控台的憑證重新載入失敗，繼續使用舊的設定: {e}"),
        }
    }
}

/// accept 迴圈：先取得連線名額再 accept；關機時停止接受並中斷所有連線
async fn accept_loop(
    listener: TcpListener,
    ctx: Arc<ApiContext>,
    acceptor: Arc<RwLock<TlsAcceptor>>,
    max_connections: usize,
) {
    let limit = Arc::new(Semaphore::new(max_connections));
    let mut connections = JoinSet::new();
    let shutdown = ctx.shutdown.clone();
    loop {
        while connections.try_join_next().is_some() {}
        let permit = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            p = limit.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
        };
        let (socket, peer) = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    debug!("主控台 accept 失敗: {e}");
                    drop(permit);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let acceptor = acceptor.read().await.clone();
        let ctx = ctx.clone();
        connections.spawn(async move {
            let _permit = permit;
            let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(socket)).await {
                Ok(Ok(tls)) => tls,
                Ok(Err(e)) => {
                    debug!("主控台：{peer} 的 TLS 握手失敗: {e}");
                    return;
                }
                Err(_) => {
                    debug!("主控台：{peer} 的 TLS 握手逾時");
                    return;
                }
            };
            let service = service_fn(move |req| {
                let ctx = ctx.clone();
                async move { Ok::<_, std::convert::Infallible>(handle(ctx, peer, req).await) }
            });
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .max_buf_size(MAX_HEADER_BYTES)
                .keep_alive(true);
            if let Err(e) = builder.serve_connection(TokioIo::new(tls), service).await {
                debug!("主控台：與 {peer} 的連線結束: {e}");
            }
        });
    }
    drop(listener);
    // 關機：中斷所有連線（含 SSE）
    connections.shutdown().await;
    debug!("主控台已停止");
}

/// 處理一個請求：方法與本文的檢查、驗證、路由、逾時；記錄 DEBUG 日誌
async fn handle(
    ctx: Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
) -> Response<ApiBody> {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let (resp, who) = match route(&ctx, peer, &req).await {
        Ok((resp, who)) => (resp, who),
        Err(e) => (e.into_response(Utc::now()), None),
    };
    debug!(
        "主控台 {} {} {} → {}（{:.1} ms{}）",
        peer.ip(),
        method,
        path,
        resp.status().as_u16(),
        started.elapsed().as_secs_f64() * 1000.0,
        who.map(|w| format!("，{w}")).unwrap_or_default()
    );
    resp
}

/// 路徑的各段（已解碼）
fn segments(path: &str) -> Option<Vec<String>> {
    path.trim_end_matches('/')
        .split('/')
        .skip(1)
        .map(|s| http::percent_decode(s, false))
        .collect()
}

async fn route(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: &Request<Incoming>,
) -> Result<(Response<ApiBody>, Option<String>), ApiError> {
    if ctx.shutdown.started_at().is_some() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "Kairos 正在關機",
        ));
    }
    let segs = segments(req.uri().path()).ok_or_else(ApiError::not_found)?;
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let ["api", "v1", rest @ ..] = segs.as_slice() else {
        return Err(ApiError::not_found());
    };
    if !matches!(
        rest,
        ["me"]
            | ["hosts"]
            | ["hosts", _]
            | ["hosts", _, "metrics"]
            | ["hosts", _, "metrics", _]
            | ["overview"]
            | ["system", "health"]
            | ["events"]
    ) {
        return Err(ApiError::not_found());
    }
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "本版的端點只接受 GET",
        ));
    }
    let has_body = req.headers().contains_key(TRANSFER_ENCODING)
        || req
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() != "0");
    if has_body {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "這個端點不接受請求本文",
        ));
    }
    let query = Query::parse(req.uri().query())?;
    let now = Utc::now();
    let principal = ctx.auth.authenticate(req.headers(), peer.ip(), now).await?;
    principal.require(Permission::Read)?;
    let who = Some(principal.label());

    if let ["events"] = rest {
        let last = req
            .headers()
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| query.get("last_event_id").map(str::to_string));
        return Ok((events::stream(ctx.clone(), principal, last).await?, who));
    }
    let work = dispatch(ctx, rest, &query, &principal, now);
    match tokio::time::timeout(REQUEST_TIMEOUT, work).await {
        Ok(r) => r.map(|resp| (resp, who)),
        Err(_) => {
            warn!(
                "主控台的請求 {} 超過 {} 秒",
                req.uri().path(),
                REQUEST_TIMEOUT.as_secs()
            );
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                "查詢逾時",
            )
            .with_product_code("KRS-E4003"))
        }
    }
}

async fn dispatch(
    ctx: &Arc<ApiContext>,
    rest: &[&str],
    query: &Query,
    principal: &Principal,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    match rest {
        ["me"] => {
            query.reject_unknown(&[], &[])?;
            Ok(http::ok(now, principal))
        }
        ["hosts"] => hosts::list(ctx, query, now).await,
        ["hosts", id] => {
            query.reject_unknown(&[], &[])?;
            hosts::detail(ctx, id, now).await
        }
        ["hosts", id, "metrics"] => {
            query.reject_unknown(&[], &[])?;
            metrics::list(ctx, id, now).await
        }
        ["hosts", id, "metrics", name] => metrics::trend(ctx, id, name, query, now).await,
        ["overview"] => {
            query.reject_unknown(&["group"], &[])?;
            overview::get(ctx, now).await
        }
        ["system", "health"] => {
            query.reject_unknown(&[], &[])?;
            Ok(http::ok(now, health::compute(ctx, now).await?))
        }
        _ => Err(ApiError::not_found()),
    }
}
