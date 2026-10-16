//! 主控台 API（審計 F1；規格見 `kairos/API.md`）。
//!
//! HTTPS（rustls）＋ hyper 的 HTTP/1.1 伺服器，監聽 `console.listen`（預設 127.0.0.1:9443），與
//! agent 連線埠的名額、TLS 設定與連線處理完全分開：
//!
//! | 模組 | 內容 |
//! |---|---|
//! | [`http`] | 回應本文、JSON 與錯誤格式、查詢參數 |
//! | [`auth`]、[`tokens`] | API token 與工作階段的驗證、權限（`kairos api-token` 共用 token 的保存） |
//! | [`account`] | 登入、兩步驟驗證、登出、個人設定（`/api/v1/auth/…`、`/api/v1/me/…`） |
//! | [`users`] | 使用者與角色、目錄服務的群組對應、API token 的管理 |
//! | [`audit_log`] | 稽核日誌的查詢、匯出與完整性驗證 |
//! | [`enroll`] | 註冊與 PKI 的管理端點 |
//! | [`db`] | 唯讀連線池（WAL 讀者）與低頻寫入的連線 |
//! | [`hosts`] | 主機清冊：列表、詳情、篩選值（`/hosts/facets`） |
//! | [`conn_log`] | 每台主機最近的上線與離線紀錄（記憶體中，`/hosts/{agent_id}/connections`） |
//! | [`metrics`] | 指標趨勢 |
//! | [`privacy`] | 刪除主機資料、個資匯出（商業審計 S6） |
//! | [`overview`]、[`health`] | 總覽、系統健康 |
//! | [`events`] | 即時變更推送（SSE） |
//! | [`maint`] | 背景工作：每分鐘上線數、token 的最後使用與稽核日誌、被拒的憑證寫入清冊 |
//! | [`web`] | 網頁介面的靜態檔（編譯時嵌入）與所有回應的安全標頭 |

pub mod account;
pub mod alerts;
pub mod assets;
pub mod audit_log;
pub mod auth;
pub mod conn_log;
pub mod db;
pub mod enroll;
pub mod events;
pub mod health;
pub mod hosts;
pub mod http;
mod maint;
pub mod metrics;
pub mod overview;
pub mod privacy;
pub mod tokens;
pub mod users;
pub mod web;

#[cfg(test)]
mod alerts_tests;
#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod bench;
#[cfg(test)]
mod privacy_tests;
#[cfg(test)]
mod probe_tests;
#[cfg(test)]
pub(crate) mod tests;

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
    /// 註冊服務（SQLite 時；註冊與 PKI 的管理端點使用）
    pub enroll: Option<Arc<crate::enroll::EnrollService>>,
    /// 告警服務（SQLite 且 `alerts.enabled` 時；`/api/v1/alerts` 等端點使用）
    pub alerts: Option<Arc<crate::alerts::AlertService>>,
    /// 使用者管理、登入、稽核等互動寫入（busy 最多等 2 秒）
    pub admin: Arc<WriteConn>,
    /// 登入的記憶體狀態（不存在帳號的鎖定、第二步的挑戰、雜湊的同時數）
    pub login: account::LoginState,
    /// 目錄服務（`[ldap]` 啟用時）
    pub directory: Option<Arc<dyn crate::users::ldap::Directory>>,
    /// 被拒絕的請求寫入稽核的節流（同一個執行者每分鐘最多 [`DeniedThrottle::PER_MINUTE`] 筆）
    pub denied: DeniedThrottle,
    /// `/healthz`、`/readyz`、`/metrics`（`observability.on_console` 時在主控台的監聽上提供）
    pub obs: Option<Arc<crate::probe::ObsContext>>,
    /// 每台主機最近的上線與離線紀錄（記憶體中；`/api/v1/hosts/{agent_id}/connections`）
    pub conn_log: Arc<conn_log::ConnLog>,
    /// 個資政策（`[privacy]`；被拒憑證的來源位址、個資匯出的說明）
    pub privacy: Arc<crate::privacy::PrivacyPolicy>,
    /// 刪除主機資料（交給 report 的 writer；`POST /api/v1/hosts/{agent_id}/purge`）
    pub purger: Option<crate::store::PurgeSink>,
}

/// 被拒絕（403）的寫入請求寫入稽核的節流：同一個執行者每分鐘最多 20 筆，超過的計數併入下一筆
#[derive(Debug, Default)]
pub struct DeniedThrottle {
    entries: Mutex<std::collections::HashMap<String, (Instant, u32, u64)>>,
}

impl DeniedThrottle {
    pub const PER_MINUTE: u32 = 20;

    /// 可以寫入時回傳之前被略過的筆數；這一分鐘已達上限時回 `None`
    pub fn allow(&self, actor: &str, now: Instant) -> Option<u64> {
        let mut m = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() > 10_000 {
            m.retain(|_, (start, _, _)| now.duration_since(*start) < Duration::from_secs(60));
        }
        let e = m.entry(actor.to_string()).or_insert((now, 0, 0));
        if now.duration_since(e.0) >= Duration::from_secs(60) {
            e.0 = now;
            e.1 = 0;
        }
        if e.1 >= Self::PER_MINUTE {
            e.2 += 1;
            return None;
        }
        e.1 += 1;
        Some(std::mem::take(&mut e.2))
    }
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
    pub enroll: Option<Arc<crate::enroll::EnrollService>>,
    pub alerts: Option<Arc<crate::alerts::AlertService>>,
    /// 測試用：以替代的目錄服務取代 `[ldap]` 的連線
    pub directory: Option<Arc<dyn crate::users::ldap::Directory>>,
    pub obs: Option<Arc<crate::probe::ObsContext>>,
    /// 刪除主機資料的佇列（SQLite 時；沒有時 `POST /api/v1/hosts/{agent_id}/purge` 回 503）
    pub purger: Option<crate::store::PurgeSink>,
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
    /// 主控台專用的 tokio runtime（與 agent 連線、report 的寫入分開排程）
    runtime: Option<tokio::runtime::Runtime>,
}

impl Console {
    /// 等待主控台結束（關機時：停止接受連線並中斷所有連線），再關閉它的 runtime
    pub async fn join(mut self) {
        let _ = (&mut self.task).await;
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        // 在 async 的環境中不能以阻塞的方式關閉 runtime
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

/// 主控台 runtime 的工作執行緒數：API 的處理（TLS、JSON、篩選排序）不與 agent 連線、report 的
/// writer 共用 tokio 的工作執行緒，大量查詢時 report 的 `ack` 不需排在查詢之後（`kairos/API.md` 第 9 節）
pub const CONSOLE_WORKER_THREADS: usize = 2;
/// 主控台 runtime 的 blocking 執行緒上限（唯讀查詢 4 條、寫入 1 條，另有餘裕）
pub const CONSOLE_BLOCKING_THREADS: usize = 8;

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
    // 以標準函式庫監聽，再交給主控台的 runtime（socket 註冊在使用它的 runtime 上）
    let listener = std::net::TcpListener::bind(console.listen.trim())
        .and_then(|l| l.set_nonblocking(true).map(|_| l))
        .map_err(|e| {
            StartError::Listen(Error::new(
                e.kind(),
                format!("無法監聽主控台 console.listen {}: {}", console.listen, e),
            ))
        })?;
    let local_addr = listener.local_addr().map_err(StartError::Listen)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(CONSOLE_WORKER_THREADS)
        .max_blocking_threads(CONSOLE_BLOCKING_THREADS)
        .thread_name("kairos-console")
        .enable_all()
        .build()
        .map_err(|e| StartError::Other(Error::other(format!("無法建立主控台的 runtime: {e}"))))?;

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
    // 主控台金鑰（CSRF、TOTP 秘密的加密）：無法建立時工作階段無法使用，API token 照常
    let console_key = match crate::users::secrets::ConsoleKey::load_or_create(&p.data_dir) {
        Ok(k) => Some(Arc::new(k)),
        Err(e) => {
            log::error!(
                "主控台金鑰 {} 無法使用，帳號登入停用（API token 不受影響）：{e}",
                p.data_dir.join(crate::users::secrets::KEY_FILE).display()
            );
            None
        }
    };
    let admin = WriteConn::with_busy(db_opts.clone(), db::ADMIN_WRITE_BUSY);
    let directory: Option<Arc<dyn crate::users::ldap::Directory>> = match p.directory.clone() {
        Some(d) => Some(d),
        None if p.config.ldap.enabled => Some(Arc::new(crate::users::ldap::LdapDirectory {
            settings: p.config.ldap.clone(),
        })),
        None => None,
    };
    let ctx = Arc::new(ApiContext {
        auth: Authenticator::new(pool.clone(), admin.clone(), console_key, p.config.clone()),
        writer: WriteConn::new(db_opts),
        admin,
        login: account::LoginState::default(),
        directory,
        denied: DeniedThrottle::default(),
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
        enroll: p.enroll.clone(),
        alerts: p.alerts.clone(),
        obs: p.obs.clone().filter(|_| p.config.observability.on_console),
        conn_log: Arc::new(conn_log::ConnLog::default()),
        privacy: Arc::new(crate::privacy::PrivacyPolicy::load(
            p.config.privacy,
            &p.data_dir,
        )),
        purger: p.purger.clone(),
    });
    ctx.hosts.load_known(known);

    // 狀態表的變化 → SSE 事件；以下的工作都在主控台的 runtime 上執行
    let index_ctx = ctx.clone();
    runtime.spawn(events::pump(
        p.registry.subscribe(),
        ctx.events.clone(),
        ctx.conn_log.clone(),
        move |id| index_ctx.hosts.mark_known(id),
        p.shutdown.clone(),
    ));
    runtime.spawn(maint::run(ctx.clone()));
    // 告警的變化 → SSE（`alert.created`、`alert.updated`、`alert.resolved`）
    if let Some(svc) = &p.alerts {
        runtime.spawn(events::alert_pump(
            svc.subscribe(),
            ctx.events.clone(),
            p.shutdown.clone(),
        ));
    }
    let acceptor = Arc::new(RwLock::new(acceptor));
    if p.config.server.reload_interval_secs > 0 {
        runtime.spawn(reload_certificate(
            TlsPaths {
                cert: cert.clone(),
                key: key.clone(),
                client_ca: None,
                crls: Vec::new(),
                issuing_cas: None,
                issuing_crl: None,
                retired_cas: None,
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
    let loop_ctx = ctx.clone();
    let max_connections = console.max_connections;
    let task = runtime.spawn(async move {
        match TcpListener::from_std(listener) {
            Ok(listener) => accept_loop(listener, loop_ctx, acceptor, max_connections).await,
            Err(e) => log::error!("主控台無法使用監聽的 socket: {e}"),
        }
    });
    Ok(Console {
        local_addr,
        ctx,
        task,
        runtime: Some(runtime),
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

/// 處理一個請求：網頁介面的靜態檔（[`web`]）或 API／健康檢查；每個回應都加上安全標頭
async fn handle(
    ctx: Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
) -> Response<ApiBody> {
    if web::is_web_path(req.uri().path()) {
        let (mut resp, html) = web::serve(req.method(), req.uri().path(), req.headers());
        web::apply_security_headers(resp.headers_mut(), html);
        debug!(
            "主控台 {} {} {} → {}（網頁）",
            peer.ip(),
            req.method(),
            req.uri().path(),
            resp.status().as_u16()
        );
        return resp;
    }
    let mut resp = handle_api(ctx, peer, req).await;
    web::apply_security_headers(resp.headers_mut(), false);
    resp
}

/// 處理一個 API 或健康檢查的請求：驗證、路由、權限、逾時；記錄 DEBUG 日誌。被拒絕（403）的寫入請求寫入稽核
async fn handle_api(
    ctx: Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
) -> Response<ApiBody> {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // 健康檢查與 Prometheus（審計 O3）：不經過 API token／工作階段的驗證，規則見 crate::probe
    if let Some(obs) = &ctx.obs
        && crate::probe::is_probe_path(&path)
    {
        let resp =
            crate::probe::handle(obs, &method, &path, req.uri().query(), req.headers(), peer).await;
        crate::metrics::global().api_request(&path, resp.status().as_u16());
        return resp;
    }
    let now = Utc::now();
    let (result, principal) = route(&ctx, peer, req, now).await;
    let resp = match result {
        Ok(r) => r,
        Err(e) => {
            if e.status == StatusCode::FORBIDDEN
                && e.code == "forbidden"
                && auth::is_unsafe(&method)
                && let Some(p) = &principal
                && let Some(suppressed) = ctx.denied.allow(&p.actor(), Instant::now())
            {
                let mut details = e.details.clone().unwrap_or_else(|| serde_json::json!({}));
                if suppressed > 0 {
                    details["suppressed_since_last"] = serde_json::json!(suppressed);
                }
                account::audit(
                    &ctx,
                    crate::audit::Event {
                        actor: p.actor(),
                        actor_ip: Some(peer.ip().to_canonical().to_string()),
                        category: "access",
                        action: "access.denied",
                        object: Some(format!("{method} {path}")),
                        result: "denied",
                        code: e.product_code,
                        details: Some(details),
                    },
                    now.timestamp_millis(),
                )
                .await;
            }
            e.into_response(Utc::now())
        }
    };
    crate::metrics::global().api_request(&path, resp.status().as_u16());
    debug!(
        "主控台 {} {} {} → {}（{:.1} ms{}）",
        peer.ip(),
        method,
        path,
        resp.status().as_u16(),
        started.elapsed().as_secs_f64() * 1000.0,
        principal
            .map(|p| format!("，{}", p.label()))
            .unwrap_or_default()
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

/// 需要驗證的路徑（不在這裡的回 404，不先要求驗證）
fn known_route(rest: &[&str]) -> bool {
    matches!(
        rest,
        ["me", ..]
            | ["hosts"]
            | ["hosts", _]
            | ["hosts", _, "metrics"]
            | ["hosts", _, "metrics", _]
            | ["hosts", _, "connections"]
            | ["hosts", _, "inventory"]
            | ["hosts", _, "inventory", "changes"]
            | ["hosts", _, "processes"]
            | ["hosts", _, "purge" | "personal-data"]
            | ["overview"]
            | ["system", "health"]
            | ["events"]
            | ["enroll", ..]
            | ["pki", ..]
            | ["settings", ..]
            | ["users", ..]
            | ["roles"]
            | ["directory", ..]
            | ["api-tokens", ..]
            | ["audit", ..]
            | ["auth", "logout"]
            | ["alerts", ..]
            | ["alert-rules", ..]
            | ["silences", ..]
            | ["channels", ..]
    )
}

/// 必須先變更密碼時仍可使用的端點
fn allowed_before_password_change(method: &Method, rest: &[&str]) -> bool {
    matches!(
        (method.as_str(), rest),
        ("GET" | "HEAD", ["me"]) | ("POST", ["me", "password"]) | ("POST", ["auth", "logout"])
    )
}

async fn route(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    now: DateTime<Utc>,
) -> (Result<Response<ApiBody>, ApiError>, Option<Principal>) {
    if ctx.shutdown.started_at().is_some() {
        return (
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting_down",
                "Kairos 正在關機",
            )),
            None,
        );
    }
    let Some(segs) = segments(req.uri().path()) else {
        return (Err(ApiError::not_found()), None);
    };
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let ["api", "v1", rest @ ..] = segs.as_slice() else {
        return (Err(ApiError::not_found()), None);
    };
    let method = req.method().clone();
    // 不需要驗證的端點：登入
    match (method.as_str(), rest) {
        ("GET" | "HEAD", ["auth", "methods"]) => return (Ok(account::methods(ctx, now)), None),
        ("POST", ["auth", "login"]) => return (account::login(ctx, peer, req, now).await, None),
        ("POST", ["auth", "login", "mfa"]) => {
            return (account::login_mfa(ctx, peer, req, now).await, None);
        }
        (_, ["auth", "methods" | "login"] | ["auth", "login", "mfa"]) => {
            return (
                Err(ApiError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed",
                    "這個端點不接受這個方法",
                )),
                None,
            );
        }
        _ => {}
    }
    if !known_route(rest) {
        return (Err(ApiError::not_found()), None);
    }
    // 唯讀的端點：方法與本文在驗證之前檢查（與 0.4 相同）
    let read_only = !privacy::is_route(rest)
        && matches!(
            rest,
            ["hosts", ..] | ["overview"] | ["system", "health"] | ["events"]
        );
    if read_only {
        if method != Method::GET && method != Method::HEAD {
            return (
                Err(ApiError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed",
                    "這個端點只接受 GET",
                )),
                None,
            );
        }
        let has_body = req.headers().contains_key(TRANSFER_ENCODING)
            || req
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.trim() != "0");
        if has_body {
            return (
                Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    "這個端點不接受請求本文",
                )),
                None,
            );
        }
    }
    let query = match Query::parse(req.uri().query()) {
        Ok(q) => q,
        Err(e) => return (Err(e), None),
    };
    let principal = match ctx
        .auth
        .authenticate(req.headers(), &method, peer.ip(), now)
        .await
    {
        Ok(p) => p,
        Err(e) => return (Err(e), None),
    };
    if principal.must_change_password() && !allowed_before_password_change(&method, rest) {
        return (Err(ApiError::password_change_required()), Some(principal));
    }
    let p = &principal;
    let result = match rest {
        ["auth", "logout"] if method == Method::POST => account::logout(ctx, p, peer, now).await,
        ["auth", "logout"] => Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "登出只接受 POST",
        )),
        ["me"] if method == Method::GET || method == Method::HEAD => query
            .reject_unknown(&[], &[])
            .map(|_| http::ok(now, p.me_json())),
        ["me", sub @ ..] => account::me(ctx, p, peer, sub, req, now).await,
        ["enroll", ..] | ["pki", ..] | ["settings", ..] => {
            enroll::handle(ctx, peer, req, p, now).await
        }
        ["users", ..] | ["roles"] | ["directory", ..] | ["api-tokens", ..] => {
            users::handle(ctx, p, peer, rest, req, now).await
        }
        ["audit", sub @ ..] => audit_log::handle(ctx, p, peer, sub, &method, &query, now).await,
        // 個資：刪除主機資料與個資匯出（商業審計 S6；含寫入）
        _ if privacy::is_route(rest) => privacy::handle(ctx, peer, req, p, rest, now).await,
        // 告警（API.md 第 13 節；含寫入）：權限依 alerts::route 的表
        ["alerts", ..] | ["alert-rules", ..] | ["silences", ..] | ["channels", ..] => {
            alerts::handle(ctx, peer, req, p, now).await
        }
        ["events"] => match p.host_access() {
            Ok(access) => {
                let last = req
                    .headers()
                    .get("last-event-id")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
                    .or_else(|| query.get("last_event_id").map(str::to_string));
                events::stream(ctx.clone(), principal.clone(), access, last).await
            }
            Err(e) => Err(e),
        },
        _ => {
            let work = dispatch(ctx, rest, &query, p, now);
            match tokio::time::timeout(REQUEST_TIMEOUT, work).await {
                Ok(r) => r,
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
    };
    (result, Some(principal))
}

async fn dispatch(
    ctx: &Arc<ApiContext>,
    rest: &[&str],
    query: &Query,
    principal: &Principal,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    match rest {
        ["hosts"] => hosts::list(ctx, query, &principal.host_access()?, now).await,
        // agent_id 是 UUID，不會與 "facets" 相同
        ["hosts", "facets"] => hosts::facets(ctx, query, &principal.host_access()?, now).await,
        ["hosts", id, "connections"] => {
            query.reject_unknown(&[], &[])?;
            hosts::connections(ctx, id, &principal.host_access()?, now).await
        }
        ["hosts", id] => {
            query.reject_unknown(&[], &[])?;
            hosts::detail(ctx, id, &principal.host_access()?, now).await
        }
        ["hosts", id, "metrics"] => {
            query.reject_unknown(&[], &[])?;
            hosts::check_access(ctx, id, &principal.host_access()?).await?;
            metrics::list(ctx, id, now).await
        }
        ["hosts", id, "metrics", name] => {
            hosts::check_access(ctx, id, &principal.host_access()?).await?;
            metrics::trend(ctx, id, name, query, now).await
        }
        ["hosts", id, "inventory"] => {
            query.reject_unknown(&[], &[])?;
            hosts::check_access(ctx, id, &principal.host_access()?).await?;
            assets::inventory(ctx, id, now).await
        }
        ["hosts", id, "inventory", "changes"] => {
            hosts::check_access(ctx, id, &principal.host_access()?).await?;
            assets::changes(ctx, id, query, now).await
        }
        ["hosts", id, "processes"] => {
            hosts::check_access(ctx, id, &principal.host_access()?).await?;
            assets::processes(ctx, id, query, now).await
        }
        ["overview"] => {
            query.reject_unknown(&["group"], &[])?;
            let access = principal.host_access()?;
            let health_for = principal
                .grants
                .has(Permission::HealthRead)
                .then_some(&principal.grants);
            overview::get(ctx, &access, health_for, now).await
        }
        ["system", "health"] => {
            query.reject_unknown(&[], &[])?;
            principal.require(Permission::HealthRead)?;
            Ok(http::ok(
                now,
                health::compute(ctx, now).await?.redact(&principal.grants),
            ))
        }
        _ => Err(ApiError::not_found()),
    }
}
