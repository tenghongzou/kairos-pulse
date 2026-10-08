//! 與 Kairos 之間的單一長 TLS 連線。
//!
//! 流程：TCP connect（5 s）→ TLS 握手（10 s，rustls）→ `Framed<_, LinesCodec>`（NDJSON）
//! → 送 hello、等 hello_ack（5 s）→ `select!` 迴圈同時處理 heartbeat、report、
//! 伺服器訊息與讀取閒置逾時。任何錯誤都會關閉連線並依退避策略無限重連；
//! 收到結束要求（Ctrl+C 等）時，若已建立 TLS 連線會先送 close_notify 再結束。

use std::convert::Infallible;
use std::fmt;
use std::io;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{self, Instant, MissedTickBehavior};
use tokio_rustls::client::TlsStream;
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};
use tokio_util::sync::CancellationToken;

use crate::backoff::Backoff;
use crate::config_manager::AppConfig;
use crate::log_throttle::{DEFAULT_WINDOW, LogThrottle, suppressed_note};
use crate::protocol::{
    ClientMsg, MAX_LINE_LENGTH, PROTO_VERSION, ServerMsg, now_ts, sanitize_cpu_usage,
};
use crate::system_info::Sample;
use crate::tls::{self, TlsSetup};

/// TCP 連線逾時。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// TLS 握手逾時。
const TLS_TIMEOUT: Duration = Duration::from_secs(10);
/// 送出 hello 後等待 hello_ack 的逾時。
const HELLO_ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// 每則訊息寫出的逾時。
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// 讀取閒置逾時的下限（實際值見 `Settings::read_idle_timeout`）。
const MIN_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// 註冊後連線需穩定維持這麼久，斷線時才重置退避與失敗計數。
const STABLE_CONNECTION: Duration = Duration::from_secs(30);
/// 結束時送 TLS close_notify 的時間上限。
const CLOSE_NOTIFY_TIMEOUT: Duration = Duration::from_secs(1);

/// 依節流狀態記錄日誌：同一類別每 `DEFAULT_WINDOW` 最多一則，並附上被略過的次數。
macro_rules! throttled {
    ($throttle:expr, $level:ident, $($arg:tt)+) => {
        if let Some(n) = $throttle.check(std::time::Instant::now()) {
            log::$level!("{}{}", format_args!($($arg)+), suppressed_note(n, DEFAULT_WINDOW));
        }
    };
}

/// 「伺服器送了怪東西」各類日誌的節流狀態；跨連線保留，避免重連後又重新灌爆日誌。
#[derive(Debug, Default)]
struct ServerLogThrottles {
    /// 無法解析的行
    unparsable: LogThrottle,
    /// 註冊後又收到 hello_ack
    unexpected_hello_ack: LogThrottle,
    /// 註冊後的非致命 error（storage_unavailable 除外）
    nonfatal_error: LogThrottle,
    /// 伺服器無法保存 report（storage_unavailable）
    storage_unavailable: LogThrottle,
    /// 不合預期的 ack（註冊前收到、或 seq 超前）
    unexpected_ack: LogThrottle,
}

/// 連線 task 所需的設定（啟動時由 `AppConfig` 建立）。
#[derive(Debug, Clone)]
pub struct Settings {
    pub address: String,
    pub port: u16,
    /// TLS 名稱驗證用的名稱（僅用於日誌與錯誤提示；實際值在 `TlsSetup::server_name`）
    pub domain: String,
    pub agent_id: String,
    pub hostname: String,
    pub heartbeat_interval: Duration,
    pub report_interval: Duration,
    pub max_retries: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    /// 已設定的客戶端憑證路徑（用於錯誤提示）
    pub client_cert: Option<String>,
}

impl Settings {
    pub fn new(config: &AppConfig, agent_id: String, hostname: String) -> Self {
        Self {
            address: config.server.address.clone(),
            port: config.server.port,
            domain: config.server.domain.trim().to_string(),
            agent_id,
            hostname,
            heartbeat_interval: Duration::from_secs(config.heartbeat.interval),
            report_interval: Duration::from_secs(config.report.interval),
            max_retries: config.retry.max_retries,
            initial_delay: Duration::from_secs(config.retry.initial_delay),
            max_delay: Duration::from_secs(config.retry.max_delay),
            client_cert: Some(config.server.client_cert.trim().to_string())
                .filter(|p| !p.is_empty()),
        }
    }

    /// 讀取閒置逾時：`max(10 秒, 2 × min(heartbeat.interval, report.interval))`。
    fn read_idle_timeout(&self) -> Duration {
        self.heartbeat_interval
            .min(self.report_interval)
            .saturating_mul(2)
            .max(MIN_READ_IDLE_TIMEOUT)
    }
}

/// 單次連線結束的原因。
#[derive(Debug)]
enum SessionError {
    ConnectTimeout,
    Connect(io::Error),
    TlsTimeout,
    Tls(io::Error),
    WriteTimeout,
    Codec(LinesCodecError),
    Serialize(serde_json::Error),
    HelloAckTimeout,
    ProtoMismatch(u32),
    ClosedByServer,
    IdleTimeout(Duration),
    /// 伺服器送來的 error：註冊後只有 fatal 會結束連線；註冊前任何 error 都會結束連線。
    Server {
        code: String,
        message: String,
        fatal: bool,
    },
    /// 收到結束要求（Ctrl+C 等）
    Shutdown,
}

impl SessionError {
    /// 若錯誤來自 TLS 層（握手或之後的讀寫），取出 rustls 錯誤。
    fn rustls_error(&self) -> Option<&tokio_rustls::rustls::Error> {
        match self {
            Self::Tls(e) | Self::Codec(LinesCodecError::Io(e)) => tls::rustls_error(e),
            _ => None,
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectTimeout => write!(f, "TCP 連線逾時（{} 秒）", CONNECT_TIMEOUT.as_secs()),
            Self::Connect(e) => write!(f, "TCP 連線失敗：{e}"),
            Self::TlsTimeout => write!(f, "TLS 握手逾時（{} 秒）", TLS_TIMEOUT.as_secs()),
            Self::Tls(e) => write!(f, "TLS 握手失敗：{e}"),
            Self::WriteTimeout => write!(f, "寫出逾時（{} 秒）", WRITE_TIMEOUT.as_secs()),
            Self::Codec(LinesCodecError::MaxLineLengthExceeded) => {
                write!(f, "伺服器送來的單行超過 {MAX_LINE_LENGTH} bytes")
            }
            Self::Codec(LinesCodecError::Io(e)) => write!(f, "連線 I/O 錯誤：{e}"),
            Self::Serialize(e) => write!(f, "訊息序列化失敗：{e}"),
            Self::HelloAckTimeout => write!(
                f,
                "送出 hello 後 {} 秒內未收到 hello_ack",
                HELLO_ACK_TIMEOUT.as_secs()
            ),
            Self::ProtoMismatch(p) => {
                write!(
                    f,
                    "伺服器回覆的協定版本 {p} 與本程式（{PROTO_VERSION}）不符"
                )
            }
            Self::ClosedByServer => write!(f, "伺服器關閉了連線"),
            Self::IdleTimeout(d) => write!(
                f,
                "{} 秒內未收到任何伺服器訊息，視為連線已中斷",
                d.as_secs()
            ),
            Self::Server {
                code,
                message,
                fatal: true,
            } => write!(f, "伺服器回報致命錯誤 {code}：{message}"),
            Self::Server { code, message, .. } => {
                write!(f, "註冊前伺服器回報錯誤 {code}：{message}")
            }
            Self::Shutdown => write!(f, "收到結束要求"),
        }
    }
}

type Result<T> = std::result::Result<T, SessionError>;

/// 一條已完成 TLS 握手的連線，與該連線的 seq 計數器。
struct Connection {
    framed: Framed<TlsStream<TcpStream>, LinesCodec>,
    /// 最後一次送出的 seq（每條連線從 1 開始）
    seq: u64,
}

impl Connection {
    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    /// 送出一則訊息（LinesCodec 會補上 `\n`），含寫出逾時。
    async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        let line = serde_json::to_string(msg).map_err(SessionError::Serialize)?;
        match time::timeout(WRITE_TIMEOUT, self.framed.send(line)).await {
            Ok(res) => res.map_err(SessionError::Codec),
            Err(_) => Err(SessionError::WriteTimeout),
        }
    }

    /// 正常關閉：送出 TLS close_notify（最多等 `CLOSE_NOTIFY_TIMEOUT`）。
    ///
    /// 只關閉底層 TLS 串流，不 flush `Framed` 的寫入緩衝，
    /// 以免把被中斷的半行訊息送給伺服器。
    async fn close_gracefully(&mut self) {
        match time::timeout(CLOSE_NOTIFY_TIMEOUT, self.framed.get_mut().shutdown()).await {
            Ok(Ok(())) => log::info!("已送出 TLS close_notify，連線正常關閉"),
            Ok(Err(e)) => log::warn!("送出 TLS close_notify 失敗：{e}"),
            Err(_) => log::warn!(
                "送出 TLS close_notify 逾時（{} 秒），直接關閉連線",
                CLOSE_NOTIFY_TIMEOUT.as_secs()
            ),
        }
    }
}

/// 連線主迴圈：無限重連，直到 `shutdown` 被觸發才返回。
pub async fn run(
    settings: Settings,
    tls: TlsSetup,
    mut reports: mpsc::Receiver<Sample>,
    shutdown: CancellationToken,
) {
    let mut backoff = Backoff::new(settings.initial_delay, settings.max_delay);
    let mut throttles = ServerLogThrottles::default();

    loop {
        let mut registered_at = None;
        let err = match session(
            &settings,
            &tls,
            &mut reports,
            &mut registered_at,
            &mut throttles,
            &shutdown,
        )
        .await
        {
            Ok(never) => match never {},
            Err(SessionError::Shutdown) => {
                log::debug!("連線任務已停止");
                return;
            }
            Err(e) => e,
        };

        if let SessionError::Server { code, .. } = &err
            && let Some(hint) = auth_error_hint(&settings, code)
        {
            log::error!("{hint}");
        }
        let tls_hint = connection_hint(
            &err,
            &settings,
            tls.client_cert_was_requested(),
            registered_at.is_some(),
        )
        .map(|hint| format!("（{hint}）"))
        .unwrap_or_default();
        // 只有註冊後穩定維持 STABLE_CONNECTION 以上才重置退避；
        // 註冊後很快就斷（例如被 superseded 踢掉）仍視為連續失敗，延遲繼續加倍。
        let uptime_note = match registered_at.map(|t| t.elapsed()) {
            Some(up) if up >= STABLE_CONNECTION => {
                backoff.reset();
                format!("（連線已維持 {} 秒，退避已重置）", up.as_secs())
            }
            Some(up) => format!(
                "（註冊後僅維持 {:.1} 秒，未滿 {} 秒，退避不重置）",
                up.as_secs_f64(),
                STABLE_CONNECTION.as_secs()
            ),
            None => String::new(),
        };

        let delay = backoff.next_delay();
        let failures = backoff.failures();
        let msg = format!(
            "連線中斷（連續第 {failures} 次失敗）：{err}{tls_hint}{uptime_note}；{:.1} 秒後重連",
            delay.as_secs_f64()
        );
        if failures >= settings.max_retries {
            log::error!("{msg}");
        } else {
            log::warn!("{msg}");
        }

        tokio::select! {
            _ = time::sleep(delay) => {}
            _ = shutdown.cancelled() => {
                log::debug!("退避等待中收到結束要求，連線任務已停止");
                return;
            }
        }
    }
}

/// 建立一條連線並服務到它結束為止；只會以錯誤返回。
///
/// 收到結束要求時：尚未建立 TLS 連線則直接返回；已建立則先送 close_notify。
async fn session(
    settings: &Settings,
    tls: &TlsSetup,
    reports: &mut mpsc::Receiver<Sample>,
    registered_at: &mut Option<Instant>,
    throttles: &mut ServerLogThrottles,
    shutdown: &CancellationToken,
) -> Result<Infallible> {
    tls.reset_client_cert_request();
    let mut conn = tokio::select! {
        res = connect(settings, tls) => res?,
        _ = shutdown.cancelled() => return Err(SessionError::Shutdown),
    };

    let result = tokio::select! {
        res = serve(&mut conn, settings, reports, registered_at, throttles) => res,
        _ = shutdown.cancelled() => Err(SessionError::Shutdown),
    };
    if matches!(result, Err(SessionError::Shutdown)) {
        conn.close_gracefully().await;
    }
    result
}

/// 在已建立的連線上：hello / hello_ack，之後進入主迴圈。
async fn serve(
    conn: &mut Connection,
    settings: &Settings,
    reports: &mut mpsc::Receiver<Sample>,
    registered_at: &mut Option<Instant>,
    throttles: &mut ServerLogThrottles,
) -> Result<Infallible> {
    // ---- hello / hello_ack ----
    let seq = conn.next_seq();
    conn.send(&ClientMsg::Hello {
        seq,
        proto: PROTO_VERSION,
        agent_id: settings.agent_id.clone(),
        hostname: settings.hostname.clone(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        heartbeat_interval_ms: u64::try_from(settings.heartbeat_interval.as_millis())
            .unwrap_or(u64::MAX),
        ts: now_ts(),
    })
    .await?;
    log::debug!("已送出 hello（seq={seq}），等待 hello_ack");

    let offline_after_ms = wait_hello_ack(conn, seq, throttles).await?;
    *registered_at = Some(Instant::now());
    log::info!(
        "已向伺服器註冊（agent_id={}，伺服器離線判定 {} ms）",
        settings.agent_id,
        offline_after_ms
    );

    // 斷線期間殘留在佇列中的舊取樣直接丟棄（at-most-once，不重送舊資料）
    while reports.try_recv().is_ok() {}

    // ---- 主迴圈 ----
    let read_idle = settings.read_idle_timeout();
    let idle = time::sleep(read_idle);
    tokio::pin!(idle);

    let hb_period = settings.heartbeat_interval;
    let mut heartbeat = time::interval_at(after(hb_period), hb_period);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let seq = conn.next_seq();
                conn.send(&ClientMsg::Heartbeat { seq, ts: now_ts() }).await?;
                log::trace!("已送出 heartbeat（seq={seq}）");
            }
            Some(sample) = reports.recv() => {
                let seq = conn.next_seq();
                let cpu_usage = sanitize_cpu_usage(sample.cpu_usage);
                if cpu_usage != sample.cpu_usage {
                    log::debug!("CPU 使用率 {} 不合理，送出前調整為 {cpu_usage}", sample.cpu_usage);
                }
                conn.send(&ClientMsg::Report {
                    seq,
                    ts: sample.ts,
                    cpu_usage,
                    total_memory: sample.total_memory,
                    used_memory: sample.used_memory,
                })
                .await?;
                log::debug!(
                    "已送出 report（seq={seq}，CPU {cpu_usage:.2}%，記憶體 {}/{} MB）",
                    sample.used_memory,
                    sample.total_memory
                );
            }
            line = conn.framed.next() => {
                let line = line.ok_or(SessionError::ClosedByServer)?.map_err(SessionError::Codec)?;
                idle.as_mut().reset(after(read_idle));
                handle_server_line(&line, conn.seq, throttles)?;
            }
            _ = &mut idle => {
                return Err(SessionError::IdleTimeout(read_idle));
            }
        }
    }
}

/// 現在時間加上 `d`；溢位時改用一年後（不 panic）。
fn after(d: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(d)
        .unwrap_or_else(|| now + Duration::from_secs(365 * 24 * 3600))
}

/// TCP 連線 + TLS 握手，各自有逾時。
async fn connect(settings: &Settings, tls: &TlsSetup) -> Result<Connection> {
    let target = (settings.address.as_str(), settings.port);
    let tcp = time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target))
        .await
        .map_err(|_| SessionError::ConnectTimeout)?
        .map_err(SessionError::Connect)?;
    // 心跳是小封包，關閉 Nagle 以降低延遲
    if let Err(e) = tcp.set_nodelay(true) {
        log::debug!("無法設定 TCP_NODELAY：{e}");
    }

    let stream = time::timeout(
        TLS_TIMEOUT,
        tls.connector.connect(tls.server_name.clone(), tcp),
    )
    .await
    .map_err(|_| SessionError::TlsTimeout)?
    .map_err(SessionError::Tls)?;
    let (_, state) = stream.get_ref();
    log::info!(
        "已建立 TLS 加密連線：{}:{}（domain={}，{}）",
        settings.address,
        settings.port,
        settings.domain,
        state
            .protocol_version()
            .map_or_else(|| "未知版本".to_string(), |v| format!("{v:?}"))
    );

    Ok(Connection {
        framed: Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE_LENGTH)),
        seq: 0,
    })
}

/// 等待 hello_ack（整體 5 s）。回傳伺服器給的 `offline_after_ms`。
async fn wait_hello_ack(
    conn: &mut Connection,
    hello_seq: u64,
    throttles: &mut ServerLogThrottles,
) -> Result<u64> {
    let deadline = after(HELLO_ACK_TIMEOUT);
    loop {
        let line = time::timeout_at(deadline, conn.framed.next())
            .await
            .map_err(|_| SessionError::HelloAckTimeout)?
            .ok_or(SessionError::ClosedByServer)?
            .map_err(SessionError::Codec)?;
        if let Some(offline_after_ms) = handle_pre_ack_line(&line, hello_seq, throttles)? {
            return Ok(offline_after_ms);
        }
    }
}

/// 處理註冊前收到的一行伺服器訊息。
///
/// - `hello_ack` → `Ok(Some(offline_after_ms))`（seq 不符只記 warn）
/// - 任何 `error`（不論是否 fatal）→ 記 error 並結束這條連線
/// - 其他 → 忽略（節流記錄），繼續等待
fn handle_pre_ack_line(
    line: &str,
    hello_seq: u64,
    throttles: &mut ServerLogThrottles,
) -> Result<Option<u64>> {
    match serde_json::from_str::<ServerMsg>(line) {
        Ok(ServerMsg::HelloAck {
            seq,
            proto,
            offline_after_ms,
        }) => {
            if proto != PROTO_VERSION {
                return Err(SessionError::ProtoMismatch(proto));
            }
            if seq != hello_seq {
                // 每條連線最多一次（收到後即離開等待），不需節流
                log::warn!(
                    "hello_ack 的 seq={seq} 與 hello 的 seq={hello_seq} 不符，仍視為註冊成功"
                );
            }
            Ok(Some(offline_after_ms))
        }
        Ok(ServerMsg::Error {
            seq,
            code,
            message,
            fatal,
        }) => {
            // 會結束連線，頻率受退避限制，不需節流
            log_server_error(seq, &code, &message, fatal, true);
            Err(SessionError::Server {
                code,
                message,
                fatal,
            })
        }
        Ok(ServerMsg::Ack { seq }) => {
            throttled!(
                throttles.unexpected_ack,
                debug,
                "尚未收到 hello_ack 前收到 ack（seq={seq}），忽略"
            );
            Ok(None)
        }
        Err(e) => {
            throttled!(
                throttles.unparsable,
                warn,
                "無法解析伺服器訊息（{e}），忽略：{}",
                truncate(line)
            );
            Ok(None)
        }
    }
}

/// 處理註冊後收到的一行伺服器訊息。`last_sent_seq` 用於比對 ack。
fn handle_server_line(
    line: &str,
    last_sent_seq: u64,
    throttles: &mut ServerLogThrottles,
) -> Result<()> {
    match serde_json::from_str::<ServerMsg>(line) {
        Ok(ServerMsg::Ack { seq }) => {
            if seq == 0 || seq > last_sent_seq {
                throttled!(
                    throttles.unexpected_ack,
                    debug,
                    "收到 ack seq={seq}，但目前只送到 seq={last_sent_seq}"
                );
            } else {
                log::trace!("收到 ack seq={seq}");
            }
            Ok(())
        }
        Ok(ServerMsg::HelloAck { seq, .. }) => {
            throttled!(
                throttles.unexpected_hello_ack,
                warn,
                "已註冊後又收到 hello_ack（seq={seq}），忽略"
            );
            Ok(())
        }
        Ok(ServerMsg::Error {
            seq,
            code,
            message,
            fatal: true,
        }) => {
            log_server_error(seq, &code, &message, true, false);
            Err(SessionError::Server {
                code,
                message,
                fatal: true,
            })
        }
        Ok(ServerMsg::Error {
            seq, code, message, ..
        }) if code == "storage_unavailable" => {
            // report 的 ack 代表已寫入伺服器資料檔；storage_unavailable 代表這筆沒有保存。
            // report 為 at-most-once：視為未送達，不重送，連線保持。
            let seq_text = seq.map_or_else(|| "null".to_string(), |s| s.to_string());
            throttled!(
                throttles.storage_unavailable,
                warn,
                "伺服器無法保存 report（seq={seq_text}，storage_unavailable）：{message}；此筆資料未送達，不會重送"
            );
            Ok(())
        }
        Ok(ServerMsg::Error {
            seq, code, message, ..
        }) => {
            let seq_text = seq.map_or_else(|| "null".to_string(), |s| s.to_string());
            throttled!(
                throttles.nonfatal_error,
                warn,
                "伺服器回報錯誤（seq={seq_text}）{code}：{message}"
            );
            Ok(())
        }
        Err(e) => {
            throttled!(
                throttles.unparsable,
                warn,
                "無法解析伺服器訊息（{e}），忽略：{}",
                truncate(line)
            );
            Ok(())
        }
    }
}

/// 記錄會結束連線的伺服器 error（致命錯誤，或註冊前的任何錯誤），一律為 error 等級。
fn log_server_error(seq: Option<u64>, code: &str, message: &str, fatal: bool, before_ack: bool) {
    let seq_text = seq.map_or_else(|| "null".to_string(), |s| s.to_string());
    match code {
        "unsupported_version" => log::error!(
            "協定版本不符：伺服器不支援本程式的 proto {PROTO_VERSION}（{message}），請更新 Pulse 或 Kairos"
        ),
        "auth_failed" => log::error!("伺服器拒絕身分驗證（auth_failed）：{message}"),
        "certificate_rejected" => {
            log::error!("伺服器拒絕了本機的客戶端憑證（certificate_rejected）：{message}")
        }
        "superseded" => log::error!(
            "此連線已被同一 agent_id 的新連線取代（{message}）；若非預期，請確認是否有多台主機共用同一份 agent.id"
        ),
        _ if before_ack => log::error!(
            "註冊前伺服器回報錯誤（seq={seq_text}，fatal={fatal}）{code}：{message}，結束此連線"
        ),
        _ => log::error!("伺服器回報致命錯誤（seq={seq_text}）{code}：{message}"),
    }
}

/// 身分驗證相關錯誤碼（`auth_failed` / `certificate_rejected`）的處置提示，
/// 含本機 agent_id 與目前的客戶端憑證設定；其他錯誤碼回傳 `None`。
fn auth_error_hint(settings: &Settings, code: &str) -> Option<String> {
    let cert = match &settings.client_cert {
        Some(path) => format!("目前使用的客戶端憑證：{path}"),
        None => "目前未設定 server.client_cert".to_string(),
    };
    match code {
        "certificate_rejected" => Some(format!(
            "客戶端憑證已被撤銷、過期或不再受伺服器信任，請聯絡管理者重新簽發；{cert}"
        )),
        "auth_failed" => Some(format!(
            "伺服器拒絕了本機的身分（auth_failed）：客戶端憑證的 SAN 必須是 DNS:{}（本機 agent_id，可用 `pulse --print-agent-id` 確認）；{cert}。             若 agent.id 曾重新產生，需請管理者重新簽發憑證",
            settings.agent_id
        )),
        _ => None,
    }
}

/// 連線失敗時的處置提示。
///
/// 1. 錯誤本身是 rustls 錯誤（含收到的 alert）→ 依 [`tls::describe_tls_error`] 說明。
/// 2. 握手成功但在 hello_ack 之前連線就被中斷，且伺服器在握手時要求了客戶端憑證 →
///    幾乎可以確定是客戶端憑證的問題。TLS 1.3 下伺服器在握手後才驗證客戶端憑證，
///    拒絕時送出的 alert 常因連線被重設而收不到，因此改以「是否收到 CertificateRequest」判斷。
fn connection_hint(
    err: &SessionError,
    settings: &Settings,
    client_cert_requested: bool,
    registered: bool,
) -> Option<String> {
    let has_client_cert = settings.client_cert.is_some();
    if let Some(hint) = err
        .rustls_error()
        .and_then(|e| tls::describe_tls_error(e, &settings.domain, has_client_cert))
    {
        return Some(hint);
    }
    let dropped_before_ack = !registered
        && matches!(
            err,
            SessionError::ClosedByServer
                | SessionError::Tls(_)
                | SessionError::Codec(LinesCodecError::Io(_))
        );
    if !(dropped_before_ack && client_cert_requested) {
        return None;
    }
    Some(if has_client_cert {
        "伺服器在握手時要求了客戶端憑證，並在註冊前中斷連線：伺服器可能不接受此客戶端憑證，\
         請確認憑證由伺服器信任的 client CA 簽發、server.client_cert 含完整中繼 CA 鏈、EKU 含 clientAuth，且未被撤銷或過期"
            .to_string()
    } else {
        "伺服器要求客戶端憑證（mTLS），但未設定 server.client_cert / server.client_key".to_string()
    })
}

/// 日誌用：截斷過長的行，避免洗版。
fn truncate(line: &str) -> String {
    const LIMIT: usize = 200;
    match line.char_indices().nth(LIMIT) {
        Some((idx, _)) => format!("{}…（共 {} bytes）", &line[..idx], line.len()),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_rustls::rustls::{AlertDescription, Error as TlsError};

    fn settings(hb: u64, report: u64) -> Settings {
        Settings {
            address: "127.0.0.1".into(),
            port: 1,
            domain: "localhost".into(),
            agent_id: "a".into(),
            hostname: "h".into(),
            heartbeat_interval: Duration::from_secs(hb),
            report_interval: Duration::from_secs(report),
            max_retries: 5,
            initial_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
            client_cert: None,
        }
    }

    #[test]
    fn ack_and_nonfatal_error_keep_connection() {
        let mut t = ServerLogThrottles::default();
        assert!(handle_server_line(r#"{"type":"ack","seq":2}"#, 2, &mut t).is_ok());
        assert!(handle_server_line(r#"{"type":"ack","seq":9}"#, 2, &mut t).is_ok());
        assert!(
            handle_server_line(
                r#"{"type":"error","seq":null,"code":"unknown_type","message":"unknown message type","fatal":false}"#,
                2,
                &mut t
            )
            .is_ok()
        );
        // 無法解析的行只記錄，不斷線
        assert!(handle_server_line("not json", 2, &mut t).is_ok());
        assert!(handle_server_line(r#"{"type":"future_msg"}"#, 2, &mut t).is_ok());
    }

    #[test]
    fn storage_unavailable_keeps_connection_and_is_throttled_separately() {
        let mut t = ServerLogThrottles::default();
        let line = r#"{"type":"error","seq":6,"code":"storage_unavailable","message":"disk full","fatal":false}"#;
        for _ in 0..100 {
            assert!(handle_server_line(line, 6, &mut t).is_ok());
        }
        // 走自己的節流類別：已記錄過一次，其餘被略過；一般非致命錯誤的類別未被使用
        assert_eq!(t.storage_unavailable.check(std::time::Instant::now()), None);
        assert_eq!(t.nonfatal_error.check(std::time::Instant::now()), Some(0));
    }

    #[test]
    fn flood_of_garbage_is_throttled_but_keeps_connection() {
        let mut t = ServerLogThrottles::default();
        for _ in 0..1000 {
            assert!(handle_server_line("xxxx", 1, &mut t).is_ok());
            assert!(
                handle_server_line(
                    r#"{"type":"hello_ack","seq":1,"proto":1,"offline_after_ms":5000}"#,
                    1,
                    &mut t
                )
                .is_ok()
            );
        }
        // 第一則有記錄，其餘 999 則被略過並等待下次記錄時回報
        assert_eq!(t.unparsable.check(std::time::Instant::now()), None);
        assert_eq!(
            t.unexpected_hello_ack.check(std::time::Instant::now()),
            None
        );
    }

    #[test]
    fn fatal_error_ends_connection() {
        let mut t = ServerLogThrottles::default();
        let err = handle_server_line(
            r#"{"type":"error","seq":1,"code":"unsupported_version","message":"supported proto: 1","fatal":true}"#,
            1,
            &mut t,
        )
        .unwrap_err();
        assert!(
            matches!(err, SessionError::Server { ref code, fatal: true, .. } if code == "unsupported_version")
        );
    }

    #[test]
    fn pre_ack_hello_ack_accepted_even_with_wrong_seq() {
        let mut t = ServerLogThrottles::default();
        let ok = handle_pre_ack_line(
            r#"{"type":"hello_ack","seq":1,"proto":1,"offline_after_ms":5000}"#,
            1,
            &mut t,
        )
        .unwrap();
        assert_eq!(ok, Some(5000));
        // seq 不符只記 warn，不斷線
        let ok = handle_pre_ack_line(
            r#"{"type":"hello_ack","seq":7,"proto":1,"offline_after_ms":6000}"#,
            1,
            &mut t,
        )
        .unwrap();
        assert_eq!(ok, Some(6000));
        // proto 不符則斷線
        assert!(matches!(
            handle_pre_ack_line(
                r#"{"type":"hello_ack","seq":1,"proto":2,"offline_after_ms":5000}"#,
                1,
                &mut t
            ),
            Err(SessionError::ProtoMismatch(2))
        ));
    }

    #[test]
    fn pre_ack_any_error_ends_connection() {
        let mut t = ServerLogThrottles::default();
        let err = handle_pre_ack_line(
            r#"{"type":"error","seq":1,"code":"bad_request","message":"invalid agent_id","fatal":false}"#,
            1,
            &mut t,
        )
        .unwrap_err();
        assert!(
            matches!(err, SessionError::Server { ref code, fatal: false, .. } if code == "bad_request")
        );
        assert!(err.to_string().contains("bad_request"));

        // 非 error 的雜訊則繼續等待
        assert_eq!(
            handle_pre_ack_line(r#"{"type":"ack","seq":1}"#, 1, &mut t).unwrap(),
            None
        );
        assert_eq!(handle_pre_ack_line("garbage", 1, &mut t).unwrap(), None);
    }

    #[test]
    fn read_idle_timeout_rules() {
        // max(10 秒, 2 × min(heartbeat, report))
        assert_eq!(settings(1, 5).read_idle_timeout(), Duration::from_secs(10));
        assert_eq!(settings(10, 5).read_idle_timeout(), Duration::from_secs(10));
        assert_eq!(
            settings(30, 3600).read_idle_timeout(),
            Duration::from_secs(60)
        );
        assert_eq!(
            settings(60, 20).read_idle_timeout(),
            Duration::from_secs(40)
        );
        assert_eq!(
            settings(60, 3600).read_idle_timeout(),
            Duration::from_secs(120)
        );
        // 極大值也不 panic
        let mut s = settings(1, 1);
        s.heartbeat_interval = Duration::MAX;
        s.report_interval = Duration::MAX;
        assert_eq!(s.read_idle_timeout(), Duration::MAX);
    }

    #[test]
    fn after_never_panics() {
        let _ = after(Duration::MAX);
        let _ = after(Duration::from_secs(u64::MAX));
    }

    #[test]
    fn rustls_error_found_in_handshake_and_read_errors() {
        let make = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                TlsError::AlertReceived(AlertDescription::CertificateRequired),
            )
        };
        assert!(SessionError::Tls(make()).rustls_error().is_some());
        // TLS 1.3 下客戶端憑證被拒絕的 alert 會在第一次讀取時才出現
        assert!(
            SessionError::Codec(LinesCodecError::Io(make()))
                .rustls_error()
                .is_some()
        );
        assert!(SessionError::ClosedByServer.rustls_error().is_none());
        assert!(
            SessionError::Connect(io::Error::other("refused"))
                .rustls_error()
                .is_none()
        );
    }

    #[test]
    fn connection_hint_rules() {
        let io_err = || SessionError::Codec(LinesCodecError::Io(io::Error::other("reset")));
        let mut s = settings(1, 5);
        // 伺服器要求客戶端憑證、未設定、註冊前斷線 → 提示設定
        let hint = connection_hint(&io_err(), &s, true, false).unwrap();
        assert!(hint.contains("未設定 server.client_cert"), "{hint}");
        assert!(
            connection_hint(&SessionError::ClosedByServer, &s, true, false)
                .unwrap()
                .contains("未設定")
        );
        // 伺服器沒有要求、或已註冊、或錯誤與連線無關 → 不提示
        assert_eq!(connection_hint(&io_err(), &s, false, false), None);
        assert_eq!(connection_hint(&io_err(), &s, true, true), None);
        assert_eq!(
            connection_hint(&SessionError::HelloAckTimeout, &s, true, false),
            None
        );
        // 已設定客戶端憑證 → 提示伺服器可能不接受
        s.client_cert = Some("client.pem".into());
        assert!(
            connection_hint(&io_err(), &s, true, false)
                .unwrap()
                .contains("可能不接受此客戶端憑證")
        );
        // rustls 錯誤優先
        let alert = SessionError::Tls(io::Error::new(
            io::ErrorKind::InvalidData,
            TlsError::AlertReceived(AlertDescription::CertificateRevoked),
        ));
        assert!(
            connection_hint(&alert, &s, true, false)
                .unwrap()
                .contains("撤銷")
        );
    }

    #[test]
    fn auth_failed_hint_names_agent_id_and_cert() {
        let mut s = settings(1, 5);
        s.agent_id = "6f1c0e4a-1b2c-4d3e-9f00-112233445566".into();
        let hint = auth_error_hint(&s, "auth_failed").unwrap();
        assert!(
            hint.contains("DNS:6f1c0e4a-1b2c-4d3e-9f00-112233445566"),
            "{hint}"
        );
        assert!(hint.contains("未設定 server.client_cert"), "{hint}");
        s.client_cert = Some("certs/client.pem".into());
        assert!(
            auth_error_hint(&s, "auth_failed")
                .unwrap()
                .contains("certs/client.pem")
        );
    }

    #[test]
    fn certificate_rejected_hint_is_selected_by_code() {
        let s = settings(1, 5);
        let hint = auth_error_hint(&s, "certificate_rejected").unwrap();
        assert!(hint.contains("撤銷"), "{hint}");
        assert!(!hint.contains("SAN"), "{hint}");
        assert!(!auth_error_hint(&s, "auth_failed").unwrap().contains("撤銷"));
        assert!(auth_error_hint(&s, "superseded").is_none());
    }

    #[test]
    fn truncate_long_lines() {
        assert_eq!(truncate("short"), "short");
        let long = "字".repeat(300);
        let t = truncate(&long);
        assert!(t.starts_with(&"字".repeat(200)));
        assert!(t.contains("共 900 bytes"));
    }
}
