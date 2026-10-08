//! 與 Kairos 之間的單一長 TLS 連線。
//!
//! 流程：TCP connect（5 s）→ TLS 握手（10 s，rustls）→ `Framed<_, LinesCodec>`（NDJSON）
//! → 送 hello、等 hello_ack（5 s）→ `select!` 迴圈同時處理 heartbeat、report、
//! 伺服器訊息與讀取閒置逾時。任何錯誤都會關閉連線並依退避策略無限重連；
//! 收到結束要求（Ctrl+C 等）時，若已建立 TLS 連線會先送 close_notify 再結束
//! （訊息只送出一部分時例外，見 [`Connection::close_gracefully`]）。
//!
//! 伺服器送來的文字（error 的 code / message、無法解析的原始行）一律經
//! [`sanitize_for_log`] 轉義控制字元並截斷後才寫進日誌，避免偽造日誌行或夾帶終端跳脫序列。

use std::convert::Infallible;
use std::fmt;
use std::io;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
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
/// 讀取閒置逾時中，容忍伺服器延遲回應的餘裕（實際值見 `Settings::read_idle_timeout`）。
/// 需大於伺服器最壞的回應延遲（Kairos 寫檔逾時上限 8 秒）。
const READ_IDLE_MARGIN: Duration = Duration::from_secs(10);
/// 註冊後連線需穩定維持這麼久，斷線時才重置退避與失敗計數。
const STABLE_CONNECTION: Duration = Duration::from_secs(30);
/// 結束時送 TLS close_notify 的時間上限。
const CLOSE_NOTIFY_TIMEOUT: Duration = Duration::from_secs(1);
/// 連續被 `superseded` 踢掉達此次數時，改以 error 記錄並提示 agent.id 可能被多台主機共用，
/// 且該次重連延遲至少為 `SUPERSEDED_MIN_DELAY`。
const SUPERSEDED_STREAK_ERROR: u32 = 2;
/// 連續被取代時的最短重連延遲。這是「`retry.max_delay` 為延遲上限」的明文例外：
/// 兩台主機共用同一份 agent.id 時，即使 max_delay 很小也不會每秒互踢。
const SUPERSEDED_MIN_DELAY: Duration = STABLE_CONNECTION;
/// 「連續被取代」的 error 日誌節流視窗；視窗內的其他次數改記 warn。
const SUPERSEDED_LOG_WINDOW: Duration = Duration::from_secs(600);
/// 伺服器 error 的 `code` 寫進日誌時的長度上限（字元）。
const LOG_CODE_LIMIT: usize = 64;
/// 伺服器送來的其他文字（error 的 `message`、無法解析的行）寫進日誌時的長度上限（字元）。
const LOG_TEXT_LIMIT: usize = 200;

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

    /// 讀取閒置逾時：`max(2 × m, m + 10 秒)`，`m = min(heartbeat.interval, report.interval)`。
    ///
    /// 最壞情況下，距上一則伺服器訊息約 `m` 之後才送出下一則訊息，伺服器再延遲最多 8 秒才回應；
    /// `m + 10 秒` 確保這種情況不會被誤判為斷線，`2 × m` 則讓長間隔時至少容忍漏掉一次回應。
    fn read_idle_timeout(&self) -> Duration {
        let m = self.heartbeat_interval.min(self.report_interval);
        m.saturating_mul(2).max(m.saturating_add(READ_IDLE_MARGIN))
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
    ///
    /// `code` / `message` 已經過 [`sanitize_for_log`]，可直接寫進日誌。
    Server {
        code: String,
        message: String,
        fatal: bool,
    },
    /// 收到結束要求（Ctrl+C 等）
    Shutdown,
}

impl SessionError {
    /// 由伺服器 error 建立；`code` / `message` 先轉義控制字元並截斷。
    fn server(code: &str, message: &str, fatal: bool) -> Self {
        Self::Server {
            code: sanitize_for_log(code, LOG_CODE_LIMIT),
            message: sanitize_for_log(message, LOG_TEXT_LIMIT),
            fatal,
        }
    }

    /// 伺服器 error 的錯誤碼；其他錯誤回傳 `None`。
    fn server_code(&self) -> Option<&str> {
        match self {
            Self::Server { code, .. } => Some(code),
            _ => None,
        }
    }

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
                "{} 秒內未收到任何合法的伺服器訊息，視為連線已中斷",
                d.as_secs()
            ),
            Self::Server {
                code,
                message,
                fatal,
            } => {
                let prefix = if *fatal {
                    "伺服器回報致命錯誤"
                } else {
                    "註冊前伺服器回報錯誤"
                };
                if code.is_empty() {
                    write!(f, "{prefix}（未提供錯誤碼）")?;
                } else {
                    write!(f, "{prefix} {code}")?;
                }
                if !message.is_empty() {
                    write!(f, "：{message}")?;
                }
                Ok(())
            }
            Self::Shutdown => write!(f, "收到結束要求"),
        }
    }
}

type Result<T> = std::result::Result<T, SessionError>;

/// 建立連線的方式。正式環境為 TCP + TLS（[`TlsDialer`]）；測試時換成記憶體內的串流，
/// 以便在 `tokio::time::pause()` 下驗證逾時與退避邏輯。
trait Dialer {
    type Stream: AsyncRead + AsyncWrite + Unpin;

    /// 建立一條連線（含 TLS 握手）。
    async fn dial(&self) -> Result<Self::Stream>;

    /// 最近一次 `dial` 的握手中，伺服器是否要求了客戶端憑證（用於錯誤提示）。
    fn client_cert_was_requested(&self) -> bool;
}

/// 正式環境的連線方式：TCP 連線 + rustls 握手。
struct TlsDialer<'a> {
    settings: &'a Settings,
    tls: &'a TlsSetup,
}

impl Dialer for TlsDialer<'_> {
    type Stream = TlsStream<TcpStream>;

    async fn dial(&self) -> Result<Self::Stream> {
        self.tls.reset_client_cert_request();
        connect(self.settings, self.tls).await
    }

    fn client_cert_was_requested(&self) -> bool {
        self.tls.client_cert_was_requested()
    }
}

/// 一條已建立的連線，與該連線的 seq 計數器。
struct Connection<S> {
    framed: Framed<S, LinesCodec>,
    /// 最後一次送出的 seq（每條連線從 1 開始）
    seq: u64,
    /// 最近一則送出訊息編碼後的長度（含 `\n`），用來判斷寫出是否只完成一部分
    last_line_len: usize,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Connection<S> {
    fn new(stream: S) -> Self {
        Self {
            framed: Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE_LENGTH)),
            seq: 0,
            last_line_len: 0,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    /// 送出一則訊息（LinesCodec 會補上 `\n`），含寫出逾時。
    async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        let line = serde_json::to_string(msg).map_err(SessionError::Serialize)?;
        self.last_line_len = line.len() + 1;
        match time::timeout(WRITE_TIMEOUT, self.framed.send(line)).await {
            Ok(res) => res.map_err(SessionError::Codec),
            Err(_) => Err(SessionError::WriteTimeout),
        }
    }

    /// 寫出被中斷時，最近一則訊息是否已有一部分交給底層串流（之後可能送到伺服器）。
    ///
    /// `Framed` 只保留尚未寫出的部分：剩餘量介於 0 與整行長度之間代表「寫了一半」；
    /// 等於整行長度代表一個 byte 都還沒寫出，丟棄即可。
    fn has_partial_write(&self) -> bool {
        let pending = self.framed.write_buffer().len();
        pending > 0 && pending < self.last_line_len
    }

    /// 正常關閉：送出 TLS close_notify（最多等 `CLOSE_NOTIFY_TIMEOUT`）。回傳是否成功送出。
    ///
    /// 只關閉底層串流，不 flush `Framed` 的寫入緩衝，以免把被中斷的訊息補送給伺服器。
    /// 若被中斷的訊息已有一部分送出，則**不送** close_notify、直接關閉：
    /// 正常關閉會讓伺服器把已收到的半行當成最後一則訊息處理；直接關閉時伺服器只會看到
    /// 「未正常關閉的連線」並丟棄不完整的行。
    async fn close_gracefully(&mut self) -> bool {
        if self.has_partial_write() {
            log::warn!(
                "結束時有一則訊息只送出一部分，略過 TLS close_notify 直接關閉連線，避免伺服器收到半行訊息"
            );
            return false;
        }
        match time::timeout(CLOSE_NOTIFY_TIMEOUT, self.framed.get_mut().shutdown()).await {
            Ok(Ok(())) => {
                log::info!("已送出 TLS close_notify，連線正常關閉");
                true
            }
            Ok(Err(e)) => {
                log::warn!("送出 TLS close_notify 失敗：{e}");
                false
            }
            Err(_) => {
                log::warn!(
                    "送出 TLS close_notify 逾時（{} 秒），直接關閉連線",
                    CLOSE_NOTIFY_TIMEOUT.as_secs()
                );
                false
            }
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
    let dialer = TlsDialer {
        settings: &settings,
        tls: &tls,
    };
    run_with(&settings, &dialer, &mut reports, &shutdown).await;
}

/// [`run`] 的實作，連線方式由 `dialer` 提供。
async fn run_with<D: Dialer>(
    settings: &Settings,
    dialer: &D,
    reports: &mut mpsc::Receiver<Sample>,
    shutdown: &CancellationToken,
) {
    let mut backoff = Backoff::new(settings.initial_delay, settings.max_delay);
    let mut throttles = ServerLogThrottles::default();
    // 連續因 superseded 而斷線的次數（其他原因斷線即歸零）
    let mut superseded_streak: u32 = 0;
    // 「連續被取代」的 error 日誌節流（跨連線保留）
    let mut superseded_log = LogThrottle::new(SUPERSEDED_LOG_WINDOW);

    loop {
        let mut registered_at = None;
        let err = match session(
            settings,
            dialer,
            reports,
            &mut registered_at,
            &mut throttles,
            shutdown,
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

        let superseded = err.server_code() == Some("superseded");
        superseded_streak = if superseded {
            superseded_streak.saturating_add(1)
        } else {
            0
        };

        // 每次斷線只記一則日誌：錯誤本身 + 處置提示 + 退避狀態
        let hint = server_error_hint(settings, &err, superseded_streak)
            .or_else(|| {
                connection_hint(
                    &err,
                    settings,
                    dialer.client_cert_was_requested(),
                    registered_at.is_some(),
                )
            })
            .map(|hint| format!("（{hint}）"))
            .unwrap_or_default();
        // 只有註冊後穩定維持 STABLE_CONNECTION 以上才重置退避；
        // 註冊後很快就斷仍視為連續失敗，延遲繼續加倍。
        // 第一次被 superseded 踢掉視同一般斷線（穩定連線後照常重置）；
        // 連續第 2 次起不論維持多久都不重置：兩台主機共用同一份 agent.id 時，
        // 雙方各自維持 30 秒以上再被對方踢掉，若重置就會以最短間隔永久互踢。
        let repeated_superseded = superseded && superseded_streak >= SUPERSEDED_STREAK_ERROR;
        let uptime_note = match registered_at.map(|t| t.elapsed()) {
            Some(up) if repeated_superseded => format!(
                "（註冊後維持 {:.1} 秒，連續被取代，退避不重置）",
                up.as_secs_f64()
            ),
            Some(up) if up >= STABLE_CONNECTION => {
                backoff.reset();
                if superseded {
                    format!("（連線已維持 {} 秒，首次被取代，退避已重置）", up.as_secs())
                } else {
                    format!("（連線已維持 {} 秒，退避已重置）", up.as_secs())
                }
            }
            Some(up) => format!(
                "（註冊後僅維持 {:.1} 秒，未滿 {} 秒，退避不重置）",
                up.as_secs_f64(),
                STABLE_CONNECTION.as_secs()
            ),
            None => String::new(),
        };

        let mut delay = backoff.next_delay();
        let mut floor_note = "";
        if repeated_superseded && delay < SUPERSEDED_MIN_DELAY {
            delay = SUPERSEDED_MIN_DELAY;
            floor_note = "（連續被取代時延遲至少 30 秒，不受 retry.max_delay 限制）";
        }
        let failures = backoff.failures();
        let msg = format!(
            "連線中斷（連續第 {failures} 次失敗）：{err}{hint}{uptime_note}；{:.1} 秒後重連{floor_note}",
            delay.as_secs_f64()
        );
        if repeated_superseded {
            // 互踢會持續發生：error 每 SUPERSEDED_LOG_WINDOW 最多一則，其餘記 warn
            match superseded_log.check(std::time::Instant::now()) {
                Some(0) => log::error!("{msg}"),
                Some(n) => log::error!(
                    "{msg}（上次以 ERROR 記錄後，另有 {n} 次「連續被取代」的斷線已改記 WARN）"
                ),
                None => log::warn!("{msg}"),
            }
        } else if failures >= settings.max_retries || needs_attention(&err, superseded_streak) {
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

/// 斷線原因是否需要人工處理（以 error 等級記錄，不論連續失敗次數）：
/// 伺服器送來的致命錯誤或註冊前的錯誤、協定版本不符；
/// `superseded` 只在連續發生 `SUPERSEDED_STREAK_ERROR` 次以上時才算。
fn needs_attention(err: &SessionError, superseded_streak: u32) -> bool {
    match err {
        SessionError::Server { code, .. } if code == "superseded" => {
            superseded_streak >= SUPERSEDED_STREAK_ERROR
        }
        SessionError::Server { .. } | SessionError::ProtoMismatch(_) => true,
        _ => false,
    }
}

/// 建立一條連線並服務到它結束為止；只會以錯誤返回。
///
/// 收到結束要求時：尚未建立連線則直接返回；已建立則先送 close_notify。
async fn session<D: Dialer>(
    settings: &Settings,
    dialer: &D,
    reports: &mut mpsc::Receiver<Sample>,
    registered_at: &mut Option<Instant>,
    throttles: &mut ServerLogThrottles,
    shutdown: &CancellationToken,
) -> Result<Infallible> {
    let stream = tokio::select! {
        res = dialer.dial() => res?,
        _ = shutdown.cancelled() => return Err(SessionError::Shutdown),
    };
    let mut conn = Connection::new(stream);

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
async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<S>,
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
                // 只有合法的協定訊息才算「伺服器還活著」（與 Kairos 的閒置判定一致）；
                // 無法解析的行不重置閒置計時，持續送垃圾的伺服器仍會被判定為閒置
                if handle_server_line(&line, conn.seq, throttles)? {
                    idle.as_mut().reset(after(read_idle));
                }
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
async fn connect(settings: &Settings, tls: &TlsSetup) -> Result<TlsStream<TcpStream>> {
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
    let version = state
        .protocol_version()
        .map_or_else(|| "未知版本".to_string(), |v| format!("{v:?}"));
    let msg = established_message(settings, &version, tls.verifies_server_cert());
    if tls.verifies_server_cert() {
        log::info!("{msg}");
    } else {
        log::warn!("{msg}");
    }
    Ok(stream)
}

/// 「已建立 TLS 連線」的日誌內容；未驗證伺服器憑證時明確標註。
fn established_message(settings: &Settings, version: &str, verified: bool) -> String {
    let note = if verified {
        ""
    } else {
        "；⚠️ 未驗證伺服器憑證（accept_invalid_certs = true），無法確認對方身分"
    };
    format!(
        "已建立 TLS 加密連線：{}:{}（domain={}，{version}{note}）",
        settings.address, settings.port, settings.domain
    )
}

/// 等待 hello_ack（整體 5 s）。回傳伺服器給的 `offline_after_ms`。
async fn wait_hello_ack<S: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<S>,
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
/// - 任何 `error`（不論是否 fatal）→ 結束這條連線（由重連迴圈記錄 error）
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
        // 會結束連線，頻率受退避限制，不需節流；日誌由重連迴圈統一記錄一次
        Ok(ServerMsg::Error {
            code,
            message,
            fatal,
            ..
        }) => Err(SessionError::server(&code, &message, fatal)),
        Ok(ServerMsg::Ack { seq }) => {
            throttled!(
                throttles.unexpected_ack,
                debug,
                "尚未收到 hello_ack 前收到 ack（seq={seq}），忽略"
            );
            Ok(None)
        }
        Err(e) => {
            log_unparsable(throttles, line, &e);
            Ok(None)
        }
    }
}

/// 處理註冊後收到的一行伺服器訊息。`last_sent_seq` 用於比對 ack。
///
/// 回傳 `Ok(true)` 表示這是一則合法的協定訊息（用於重置讀取閒置計時），
/// `Ok(false)` 表示無法解析而被忽略；fatal error 回傳 `Err` 結束連線。
fn handle_server_line(
    line: &str,
    last_sent_seq: u64,
    throttles: &mut ServerLogThrottles,
) -> Result<bool> {
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
            Ok(true)
        }
        Ok(ServerMsg::HelloAck { seq, .. }) => {
            throttled!(
                throttles.unexpected_hello_ack,
                warn,
                "已註冊後又收到 hello_ack（seq={seq}），忽略"
            );
            Ok(true)
        }
        Ok(ServerMsg::Error {
            code,
            message,
            fatal: true,
            ..
        }) => Err(SessionError::server(&code, &message, true)),
        Ok(ServerMsg::Error {
            seq, code, message, ..
        }) if code == "storage_unavailable" => {
            // report 的 ack 代表已寫入伺服器資料檔；storage_unavailable 代表這筆沒有保存。
            // report 為 at-most-once：視為未送達，不重送，連線保持。
            throttled!(
                throttles.storage_unavailable,
                warn,
                "伺服器無法保存 report（seq={}，storage_unavailable）：{}；此筆資料未送達，不會重送",
                seq_text(seq),
                sanitize_for_log(&message, LOG_TEXT_LIMIT)
            );
            Ok(true)
        }
        Ok(ServerMsg::Error {
            seq, code, message, ..
        }) => {
            throttled!(
                throttles.nonfatal_error,
                warn,
                "伺服器回報錯誤（seq={}）{}：{}",
                seq_text(seq),
                sanitize_for_log(&code, LOG_CODE_LIMIT),
                sanitize_for_log(&message, LOG_TEXT_LIMIT)
            );
            Ok(true)
        }
        Err(e) => {
            log_unparsable(throttles, line, &e);
            Ok(false)
        }
    }
}

/// 日誌用的 seq 文字（`null` 表示伺服器無法取得 seq）。
fn seq_text(seq: Option<u64>) -> String {
    seq.map_or_else(|| "null".to_string(), |s| s.to_string())
}

/// 記錄無法解析的伺服器訊息（節流）；解析錯誤本身也可能含有原始內容，一併清理。
fn log_unparsable(throttles: &mut ServerLogThrottles, line: &str, e: &serde_json::Error) {
    throttled!(
        throttles.unparsable,
        warn,
        "無法解析伺服器訊息（{}），忽略：{}",
        sanitize_for_log(&e.to_string(), LOG_TEXT_LIMIT),
        sanitize_for_log(line, LOG_TEXT_LIMIT)
    );
}

/// 伺服器 error 的處置提示；不是伺服器 error 或沒有特別提示的錯誤碼回傳 `None`。
fn server_error_hint(
    settings: &Settings,
    err: &SessionError,
    superseded_streak: u32,
) -> Option<String> {
    match err.server_code()? {
        "superseded" if superseded_streak >= SUPERSEDED_STREAK_ERROR => Some(format!(
            "已連續 {superseded_streak} 次被同一 agent_id（{}）的其他連線取代：極可能有多台主機共用同一份 agent.id\
             （例如複製了 VM 映像檔），請在多出來的主機上刪除 agent.id 後重新啟動，以產生新的 agent_id",
            settings.agent_id
        )),
        "superseded" => Some(
            "此連線已被同一 agent_id 的新連線取代；若非預期，請確認是否有多台主機共用同一份 agent.id"
                .to_string(),
        ),
        "unsupported_version" => Some(format!(
            "伺服器不支援本程式的協定版本 proto {PROTO_VERSION}，請更新 Pulse 或 Kairos"
        )),
        // Kairos 的狀態表達到 limits.max_agents 時，以 fatal bad_request 拒絕新的 agent_id
        "bad_request" if err_message(err).contains("registry is full") => Some(
            "伺服器狀態表已滿，無法登記新的 agent，請管理者調高 limits.max_agents".to_string(),
        ),
        code => auth_error_hint(settings, code, err_message(err)),
    }
}

/// 伺服器 error 的 message（已清理）；其他錯誤回傳空字串。
fn err_message(err: &SessionError) -> &str {
    match err {
        SessionError::Server { message, .. } => message,
        _ => "",
    }
}

/// 身分驗證相關錯誤碼（`auth_failed` / `certificate_rejected`）的處置提示，
/// 含本機 agent_id 與目前的客戶端憑證設定；其他錯誤碼回傳 `None`。
///
/// `message` 為伺服器給的說明：Kairos 因憑證含多個 DNS SAN 而拒絕時，
/// message 為 `client certificate must contain exactly one DNS SAN`，據此給出更具體的提示。
fn auth_error_hint(settings: &Settings, code: &str, message: &str) -> Option<String> {
    let cert = match &settings.client_cert {
        Some(path) => format!("目前使用的客戶端憑證：{path}"),
        None => "目前未設定 server.client_cert".to_string(),
    };
    match code {
        "certificate_rejected" => Some(format!(
            "客戶端憑證已被撤銷、過期或不再受伺服器信任，請聯絡管理者重新簽發；{cert}"
        )),
        "auth_failed" if message.contains("exactly one DNS SAN") => Some(format!(
            "客戶端憑證含有多個 DNS SAN：伺服器預設只接受恰好一個 DNS SAN 的客戶端憑證，\
             請以單一 SAN DNS:{} 重新簽發（或由管理者設定 auth.allow_multiple_dns_sans = true）；{cert}",
            settings.agent_id
        )),
        "auth_failed" => Some(format!(
            "伺服器拒絕了本機的身分：客戶端憑證的 SAN 必須是 DNS:{}（本機 agent_id，可用 `pulse --print-agent-id` 確認），\
             且伺服器預設只接受恰好一個 DNS SAN 的客戶端憑證；{cert}。若 agent.id 曾重新產生，需請管理者重新簽發憑證",
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
    // TLS 握手尚未完成、伺服器也還沒要求客戶端憑證就被關閉（EOF / RST）：
    // 常見原因是超過伺服器的每 IP 連線上限（Kairos 會在握手前直接關閉）
    if let SessionError::Tls(e) = err
        && !client_cert_requested
        && closed_before_handshake(e)
    {
        return Some(
            "伺服器在 TLS 握手完成前就關閉了連線：可能超過伺服器的每 IP 連線上限\
             （同一 IP 的連線過多），或伺服器正在重新啟動、連到的連接埠不是 TLS 服務"
                .to_string(),
        );
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

/// 握手錯誤是否為「連線被對方關閉」（EOF / 重設），而非 rustls 的協定或憑證錯誤。
fn closed_before_handshake(e: &io::Error) -> bool {
    tls::rustls_error(e).is_none()
        && matches!(
            e.kind(),
            io::ErrorKind::UnexpectedEof
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::BrokenPipe
        )
}

/// 日誌用：把不受信任的文字（伺服器送來的內容）轉成安全的單行文字。
///
/// - `\n`、`\r`、`\t` 轉成對應的跳脫字串；其他控制字元（C0 / DEL / C1，含 ESC）與
///   會改變顯示方向或不可見的字元（Unicode 格式字元 Cf，例如 U+200B、U+202E、U+FEFF，
///   以及 U+2028、U+2029）轉成 `\u{XXXX}`，避免偽造日誌行或夾帶終端跳脫序列
/// - 超過 `limit` 個字元時截斷，並附上原始長度
fn sanitize_for_log(s: &str, limit: usize) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len().min(limit));
    for (i, c) in s.chars().enumerate() {
        if i == limit {
            let _ = write!(out, "…（共 {} bytes）", s.len());
            break;
        }
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if is_unsafe_for_log(c) => {
                let _ = write!(out, "\\u{{{:04x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

/// 寫進日誌前必須轉義的字元（見 [`sanitize_for_log`]）。
///
/// 與 hello 顯示用字串的規則相同（Cc、全部 Cf、U+2028、U+2029），見
/// [`crate::protocol::is_forbidden_display_char`]。
fn is_unsafe_for_log(c: char) -> bool {
    crate::protocol::is_forbidden_display_char(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tokio::io::{DuplexStream, duplex};
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
        assert!(handle_server_line(r#"{"type":"ack","seq":2}"#, 2, &mut t).unwrap());
        assert!(handle_server_line(r#"{"type":"ack","seq":9}"#, 2, &mut t).unwrap());
        assert!(
            handle_server_line(
                r#"{"type":"error","seq":null,"code":"unknown_type","message":"unknown message type","fatal":false}"#,
                2,
                &mut t
            )
            .unwrap()
        );
        // 無法解析的行只記錄，不斷線，但不算合法訊息（不重置閒置計時）
        assert!(!handle_server_line("not json", 2, &mut t).unwrap());
        assert!(!handle_server_line(r#"{"type":"future_msg"}"#, 2, &mut t).unwrap());
    }

    #[test]
    fn storage_unavailable_keeps_connection_and_is_throttled_separately() {
        let mut t = ServerLogThrottles::default();
        let line = r#"{"type":"error","seq":6,"code":"storage_unavailable","message":"disk full","fatal":false}"#;
        for _ in 0..100 {
            assert!(handle_server_line(line, 6, &mut t).unwrap());
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
        // P4：缺 fatal 欄位視為 fatal，缺 message 也不會被忽略
        let err =
            handle_server_line(r#"{"type":"error","code":"auth_failed"}"#, 1, &mut t).unwrap_err();
        assert!(
            matches!(err, SessionError::Server { ref code, fatal: true, .. } if code == "auth_failed")
        );
        assert_eq!(err.to_string(), "伺服器回報致命錯誤 auth_failed");
        let err = handle_server_line(r#"{"type":"error"}"#, 1, &mut t).unwrap_err();
        assert!(err.to_string().contains("未提供錯誤碼"), "{err}");
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
        // P4：缺 message / fatal 的 error 也會結束連線，不會被當成雜訊忽略
        let err =
            handle_pre_ack_line(r#"{"type":"error","code":"auth_failed"}"#, 1, &mut t).unwrap_err();
        assert!(matches!(err, SessionError::Server { fatal: true, .. }));

        // 非 error 的雜訊則繼續等待
        assert_eq!(
            handle_pre_ack_line(r#"{"type":"ack","seq":1}"#, 1, &mut t).unwrap(),
            None
        );
        assert_eq!(handle_pre_ack_line("garbage", 1, &mut t).unwrap(), None);
    }

    #[test]
    fn read_idle_timeout_rules() {
        // max(2 × m, m + 10 秒)，m = min(heartbeat, report)
        assert_eq!(settings(1, 5).read_idle_timeout(), Duration::from_secs(11));
        assert_eq!(settings(5, 5).read_idle_timeout(), Duration::from_secs(15));
        assert_eq!(settings(10, 5).read_idle_timeout(), Duration::from_secs(15));
        assert_eq!(
            settings(10, 10).read_idle_timeout(),
            Duration::from_secs(20)
        );
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
        let hint = auth_error_hint(&s, "auth_failed", "").unwrap();
        assert!(
            hint.contains("DNS:6f1c0e4a-1b2c-4d3e-9f00-112233445566"),
            "{hint}"
        );
        assert!(hint.contains("未設定 server.client_cert"), "{hint}");
        // P6：行接續不得夾帶一大段空白
        assert!(!hint.contains("  "), "{hint:?}");
        s.client_cert = Some("certs/client.pem".into());
        assert!(
            auth_error_hint(&s, "auth_failed", "")
                .unwrap()
                .contains("certs/client.pem")
        );
    }

    #[test]
    fn certificate_rejected_hint_is_selected_by_code() {
        let s = settings(1, 5);
        let hint = auth_error_hint(&s, "certificate_rejected", "").unwrap();
        assert!(hint.contains("撤銷"), "{hint}");
        assert!(!hint.contains("SAN"), "{hint}");
        assert!(
            !auth_error_hint(&s, "auth_failed", "")
                .unwrap()
                .contains("撤銷")
        );
        assert!(auth_error_hint(&s, "superseded", "").is_none());
    }

    #[test]
    fn auth_failed_hint_mentions_single_dns_san_rule() {
        let mut s = settings(1, 5);
        s.agent_id = "6f1c0e4a-1b2c-4d3e-9f00-112233445566".into();
        // 一般 auth_failed：提示中也說明伺服器預設只接受恰好一個 DNS SAN
        let hint = auth_error_hint(&s, "auth_failed", "agent_id mismatch").unwrap();
        assert!(hint.contains("恰好一個 DNS SAN"), "{hint}");
        assert!(!hint.contains("含有多個"), "{hint}");
        // Kairos 因多個 DNS SAN 拒絕時：更具體的提示
        let err = SessionError::server(
            "auth_failed",
            "client certificate must contain exactly one DNS SAN",
            true,
        );
        let hint = server_error_hint(&s, &err, 0).unwrap();
        assert!(
            hint.contains("含有多個 DNS SAN")
                && hint.contains("DNS:6f1c0e4a-1b2c-4d3e-9f00-112233445566")
                && hint.contains("allow_multiple_dns_sans"),
            "{hint}"
        );
        assert!(!hint.contains("  "), "{hint:?}");
    }

    #[test]
    fn closed_before_handshake_hints_per_ip_limit() {
        let s = settings(1, 5);
        let closed = |kind: io::ErrorKind| SessionError::Tls(io::Error::from(kind));
        for kind in [
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
        ] {
            let hint = connection_hint(&closed(kind), &s, false, false).unwrap();
            assert!(hint.contains("每 IP 連線上限"), "{kind:?}: {hint}");
        }
        // 伺服器已要求客戶端憑證 → 握手已進行到一半，改為客戶端憑證的提示
        assert!(
            !connection_hint(&closed(io::ErrorKind::ConnectionReset), &s, true, false)
                .unwrap()
                .contains("每 IP")
        );
        // 其他 I/O 錯誤、rustls 錯誤、TCP 連線失敗 → 不是這個提示
        assert_eq!(
            connection_hint(&closed(io::ErrorKind::InvalidData), &s, false, false),
            None
        );
        assert_eq!(
            connection_hint(
                &SessionError::Connect(io::Error::from(io::ErrorKind::ConnectionRefused)),
                &s,
                false,
                false
            ),
            None
        );
    }

    #[tokio::test]
    async fn real_close_before_handshake_is_recognized() {
        // 模擬 Kairos 超過每 IP 上限：accept 後不握手直接關閉
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            drop(tcp);
        });
        let mut s = settings(1, 5);
        s.port = port;
        let tls = tls::build(&crate::config_manager::ServerConfig {
            address: "127.0.0.1".into(),
            port,
            domain: "localhost".into(),
            accept_invalid_certs: true,
            ca_cert: String::new(),
            use_system_roots: false,
            client_cert: String::new(),
            client_key: String::new(),
        })
        .unwrap();
        let err = connect(&s, &tls).await.unwrap_err();
        assert!(matches!(err, SessionError::Tls(_)), "{err:?}");
        let hint = connection_hint(&err, &s, tls.client_cert_was_requested(), false).unwrap();
        assert!(hint.contains("每 IP 連線上限"), "{err}: {hint}");
        srv.await.unwrap();
    }

    #[test]
    fn server_error_hints() {
        let s = settings(1, 5);
        let err = |code: &str| SessionError::server(code, "m", true);
        assert!(
            server_error_hint(&s, &err("superseded"), 1)
                .unwrap()
                .contains("新連線取代")
        );
        assert!(
            server_error_hint(&s, &err("superseded"), 2)
                .unwrap()
                .contains("已連續 2 次")
        );
        assert!(
            server_error_hint(&s, &err("unsupported_version"), 0)
                .unwrap()
                .contains("請更新 Pulse 或 Kairos")
        );
        assert!(
            server_error_hint(&s, &err("auth_failed"), 0)
                .unwrap()
                .contains("SAN")
        );
        assert_eq!(server_error_hint(&s, &err("bad_request"), 0), None);
        // Kairos 狀態表已滿（limits.max_agents）
        let full = SessionError::server("bad_request", "server agent registry is full", true);
        let hint = server_error_hint(&s, &full, 0).unwrap();
        assert!(
            hint.contains("狀態表已滿") && hint.contains("limits.max_agents"),
            "{hint}"
        );
        assert!(needs_attention(&full, 0));
        assert_eq!(
            server_error_hint(&s, &SessionError::ClosedByServer, 0),
            None
        );
        // 記錄等級：伺服器錯誤一律 error；superseded 只有連續發生才是 error
        assert!(needs_attention(&err("auth_failed"), 0));
        assert!(needs_attention(&SessionError::ProtoMismatch(2), 0));
        assert!(!needs_attention(&err("superseded"), 1));
        assert!(needs_attention(&err("superseded"), 2));
        assert!(!needs_attention(&SessionError::ClosedByServer, 0));
    }

    #[test]
    fn established_message_marks_unverified_server() {
        // P2
        let s = settings(1, 5);
        let msg = established_message(&s, "TLSv1_3", true);
        assert!(!msg.contains("未驗證"), "{msg}");
        let msg = established_message(&s, "TLSv1_3", false);
        assert!(
            msg.contains("未驗證伺服器憑證") && msg.contains("accept_invalid_certs"),
            "{msg}"
        );
    }

    // ---- P1：伺服器送來的文字寫進日誌前必須清理 ----

    #[test]
    fn sanitize_escapes_control_and_bidi_chars() {
        assert_eq!(sanitize_for_log("short", 200), "short");
        assert_eq!(
            sanitize_for_log("a\nb\r\tc", 200),
            "a\\nb\\r\\tc",
            "換行不得出現在日誌中"
        );
        assert_eq!(
            sanitize_for_log("\u{1b}[31mred\u{1b}[0m", 200),
            "\\u{001b}[31mred\\u{001b}[0m"
        );
        assert_eq!(
            sanitize_for_log("\u{0}\u{7f}\u{85}", 200),
            "\\u{0000}\\u{007f}\\u{0085}"
        );
        assert_eq!(
            sanitize_for_log("x\u{202e}y\u{2028}z\u{200b}\u{feff}", 200),
            "x\\u{202e}y\\u{2028}z\\u{200b}\\u{feff}"
        );
        // 一般非 ASCII 文字保留
        assert_eq!(sanitize_for_log("磁碟已滿 é", 200), "磁碟已滿 é");
    }

    #[test]
    fn sanitize_truncates_long_text() {
        let long = "字".repeat(300);
        let t = sanitize_for_log(&long, 200);
        assert!(t.starts_with(&"字".repeat(200)));
        assert!(!t.starts_with(&"字".repeat(201)));
        assert!(t.contains("共 900 bytes"));
        // 剛好 limit 個字元不截斷
        assert_eq!(sanitize_for_log(&"a".repeat(200), 200), "a".repeat(200));
    }

    /// 日誌訊息中不得出現可偽造日誌行或改變終端顯示的字元。
    fn assert_log_safe(logs: &[(log::Level, String)]) {
        for (_, msg) in logs {
            assert!(
                !msg.chars().any(is_unsafe_for_log),
                "日誌含未轉義的控制字元：{msg:?}"
            );
        }
    }

    #[test]
    fn server_text_in_logs_is_sanitized() {
        let evil = "x\n2026-10-09 00:00:00 [INFO] 偽造的日誌行\u{1b}[2J";
        let evil_json = serde_json::to_string(evil).unwrap();
        let ((), logs) = crate::test_log::capture(|| {
            let mut t = ServerLogThrottles::default();
            // 非致命 error 的 code / message
            let line = format!(
                r#"{{"type":"error","seq":1,"code":{evil_json},"message":{evil_json},"fatal":false}}"#
            );
            assert!(handle_server_line(&line, 1, &mut t).unwrap());
            // storage_unavailable 的 message
            let line = format!(
                r#"{{"type":"error","seq":1,"code":"storage_unavailable","message":{evil_json},"fatal":false}}"#
            );
            assert!(handle_server_line(&line, 1, &mut t).unwrap());
            // 無法解析的原始行（含 serde 錯誤訊息中的原始內容），且超長
            let line = format!(r#"{{"type":{evil_json}}}{}"#, "y".repeat(10_000));
            assert!(!handle_server_line(&line, 1, &mut t).unwrap());
            assert_eq!(handle_pre_ack_line(evil, 1, &mut t).unwrap(), None);
        });
        assert!(logs.len() >= 3, "{logs:?}");
        assert_log_safe(&logs);
        assert!(logs.iter().all(|(_, m)| m.len() < 2000), "{logs:?}");

        // 結束連線的 error：清理後才放進 SessionError（之後會被寫進日誌）
        let mut t = ServerLogThrottles::default();
        let line = format!(
            r#"{{"type":"error","seq":1,"code":{evil_json},"message":{evil_json},"fatal":true}}"#
        );
        let text = handle_server_line(&line, 1, &mut t)
            .unwrap_err()
            .to_string();
        assert!(!text.chars().any(is_unsafe_for_log), "{text:?}");
        assert!(text.contains("\\n2026-10-09"), "{text}");
        let long = format!(
            r#"{{"type":"error","code":"x","message":"{}","fatal":false}}"#,
            "m".repeat(100_000)
        );
        let text = handle_pre_ack_line(&long, 1, &mut t)
            .unwrap_err()
            .to_string();
        assert!(text.len() < 1000, "{}", text.len());
    }

    // ---- 以記憶體內串流 + 暫停的時鐘測試 serve() 主迴圈（T6） ----

    type ServerSide = Framed<DuplexStream, LinesCodec>;

    fn server_side(stream: DuplexStream) -> ServerSide {
        // 伺服器端不限制行長（測試會故意送超長行）
        Framed::new(stream, LinesCodec::new())
    }

    /// 讀取 hello 並回覆 hello_ack。
    async fn accept_hello(f: &mut ServerSide) {
        let line = f.next().await.unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "hello", "{line}");
        f.send(format!(
            r#"{{"type":"hello_ack","seq":{},"proto":1,"offline_after_ms":5000}}"#,
            v["seq"]
        ))
        .await
        .unwrap();
    }

    /// 在 `d` 期間對收到的每則訊息回 ack；客戶端關閉連線時提早結束。
    async fn ack_for(f: &mut ServerSide, d: Duration) {
        let deadline = Instant::now() + d;
        loop {
            tokio::select! {
                line = f.next() => {
                    let Some(Ok(line)) = line else { return };
                    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let _ = f.send(format!(r#"{{"type":"ack","seq":{}}}"#, v["seq"])).await;
                }
                _ = time::sleep_until(deadline) => return,
            }
        }
    }

    /// 讀取並丟棄客戶端送來的訊息，直到連線關閉。
    async fn drain(f: &mut ServerSide) {
        while let Some(Ok(_)) = f.next().await {}
    }

    /// 在 `stream` 上執行 serve()，回傳結束原因與是否完成註冊。
    async fn serve_once(stream: DuplexStream, s: &Settings) -> (SessionError, bool) {
        let (_tx, mut rx) = mpsc::channel::<Sample>(1);
        let mut conn = Connection::new(stream);
        let mut registered_at = None;
        let mut throttles = ServerLogThrottles::default();
        let err = match serve(&mut conn, s, &mut rx, &mut registered_at, &mut throttles).await {
            Ok(never) => match never {},
            Err(e) => e,
        };
        (err, registered_at.is_some())
    }

    #[tokio::test(start_paused = true)]
    async fn serve_ends_on_read_idle_timeout() {
        let (client, server) = duplex(64 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello(&mut f).await;
            drain(&mut f).await; // 之後只收不回
        });
        let start = Instant::now();
        let (err, registered) = serve_once(client, &settings(1, 5)).await;
        assert!(registered);
        assert!(
            matches!(err, SessionError::IdleTimeout(d) if d == Duration::from_secs(11)),
            "{err:?}"
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_secs(11) && elapsed < Duration::from_secs(12),
            "{elapsed:?}"
        );
        srv.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn garbage_does_not_reset_read_idle_timer() {
        // P5：只有合法的協定訊息才重置閒置計時
        let (client, server) = duplex(64 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello(&mut f).await;
            loop {
                tokio::select! {
                    line = f.next() => if line.is_none() { return },
                    _ = time::sleep(Duration::from_secs(1)) => {
                        if f.send("garbage".to_string()).await.is_err() { return }
                    }
                }
            }
        });
        let start = Instant::now();
        let (err, _) = serve_once(client, &settings(1, 5)).await;
        assert!(matches!(err, SessionError::IdleTimeout(_)), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(12));
        srv.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn acks_keep_connection_alive() {
        // 對照組：伺服器持續回 ack 時不會閒置逾時，直到伺服器關閉連線
        let (client, server) = duplex(64 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello(&mut f).await;
            ack_for(&mut f, Duration::from_secs(60)).await;
        });
        let start = Instant::now();
        let (err, _) = serve_once(client, &settings(1, 5)).await;
        assert!(matches!(err, SessionError::ClosedByServer), "{err:?}");
        assert!(start.elapsed() >= Duration::from_secs(60));
        srv.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn slow_server_acks_within_margin_keep_connection() {
        // X2：heartbeat = report = 5 秒時，伺服器每則訊息都延遲 8 秒才回 ack（Kairos 最壞情況）
        // 也不會被判定閒置（閒置逾時 15 秒；舊公式為 10 秒會誤斷線）
        let (client, server) = duplex(64 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello(&mut f).await;
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                let Some(Ok(line)) = f.next().await else {
                    return;
                };
                time::sleep(Duration::from_secs(8)).await;
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                if f.send(format!(r#"{{"type":"ack","seq":{}}}"#, v["seq"]))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        let start = Instant::now();
        let (err, _) = serve_once(client, &settings(5, 5)).await;
        assert!(matches!(err, SessionError::ClosedByServer), "{err:?}");
        assert!(start.elapsed() >= Duration::from_secs(60));
        srv.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn overlong_line_ends_connection() {
        let (client, server) = duplex(256 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello(&mut f).await;
            // 剛好 MAX_LINE_LENGTH 的行可接受（無法解析，忽略）
            f.send("x".repeat(MAX_LINE_LENGTH)).await.unwrap();
            f.send(r#"{"type":"ack","seq":1}"#.to_string())
                .await
                .unwrap();
            time::sleep(Duration::from_secs(1)).await;
            // 超過 1 byte → 斷線
            f.send("x".repeat(MAX_LINE_LENGTH + 1)).await.unwrap();
            drain(&mut f).await;
        });
        let (err, registered) = serve_once(client, &settings(1, 5)).await;
        assert!(registered);
        assert!(
            matches!(
                err,
                SessionError::Codec(LinesCodecError::MaxLineLengthExceeded)
            ),
            "{err:?}"
        );
        srv.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn non_utf8_line_ends_connection() {
        let (client, server) = duplex(64 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello(&mut f).await;
            f.get_mut().write_all(b"\xff\xfe{}\n").await.unwrap();
            drain(&mut f).await;
        });
        let (err, _) = serve_once(client, &settings(1, 5)).await;
        assert!(
            matches!(&err, SessionError::Codec(LinesCodecError::Io(e)) if e.kind() == io::ErrorKind::InvalidData),
            "{err:?}"
        );
        srv.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn hello_ack_timeout() {
        let (client, server) = duplex(64 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            // 讀到 hello 後只送不相干的訊息，永遠不回 hello_ack
            let _ = f.next().await;
            let _ = f.send(r#"{"type":"ack","seq":1}"#.to_string()).await;
            let _ = f.send("noise".to_string()).await;
            drain(&mut f).await;
        });
        let start = Instant::now();
        let (err, registered) = serve_once(client, &settings(1, 5)).await;
        assert!(!registered);
        assert!(matches!(err, SessionError::HelloAckTimeout), "{err:?}");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= HELLO_ACK_TIMEOUT && elapsed < HELLO_ACK_TIMEOUT + Duration::from_secs(1),
            "{elapsed:?}"
        );
        srv.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn close_skips_close_notify_after_partial_write() {
        // P13：寫出被中斷、已送出半行時，不送 close_notify
        let (client, _server) = duplex(16); // 伺服器不讀，緩衝區只有 16 bytes
        let mut conn = Connection::new(client);
        let msg = ClientMsg::Heartbeat {
            seq: 1,
            ts: now_ts(),
        };
        tokio::select! {
            _ = conn.send(&msg) => panic!("不應寫完"),
            _ = time::sleep(Duration::from_millis(100)) => {}
        }
        assert!(conn.has_partial_write());
        assert!(!conn.close_gracefully().await);

        // 對照組：完整送出後可正常關閉
        let (client, _server) = duplex(1024);
        let mut conn = Connection::new(client);
        conn.send(&ClientMsg::Heartbeat {
            seq: 1,
            ts: now_ts(),
        })
        .await
        .unwrap();
        assert!(!conn.has_partial_write());
        assert!(conn.close_gracefully().await);
    }

    // ---- run_with()：退避重置與 superseded（X1、P6） ----

    /// 測試用連線方式：每次 dial 建立一對記憶體內串流，伺服器端交給測試劇本。
    struct FakeDialer {
        servers: mpsc::UnboundedSender<DuplexStream>,
        dial_times: RefCell<Vec<Instant>>,
    }

    impl Dialer for FakeDialer {
        type Stream = DuplexStream;

        async fn dial(&self) -> Result<DuplexStream> {
            self.dial_times.borrow_mut().push(Instant::now());
            let (client, server) = duplex(64 * 1024);
            self.servers
                .send(server)
                .map_err(|_| SessionError::Connect(io::Error::other("測試伺服器已結束")))?;
            Ok(client)
        }

        fn client_cert_was_requested(&self) -> bool {
            false
        }
    }

    /// 以暫停的時鐘執行 `run_with`：前 `sessions` 次連線依序交給 `script(i, 伺服器端)`；
    /// 第 `sessions + 1` 次連線時觸發結束（該連線保持開啟，確保結束原因只有結束要求）。
    /// 回傳每次 dial 的時間（相對於開始）與期間的日誌。
    fn run_script<F, Fut>(
        s: Settings,
        sessions: usize,
        script: F,
    ) -> (Vec<Duration>, Vec<(log::Level, String)>)
    where
        F: Fn(usize, ServerSide) -> Fut + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        crate::test_log::capture(|| {
            rt.block_on(async move {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let dialer = FakeDialer {
                    servers: tx,
                    dial_times: RefCell::new(Vec::new()),
                };
                let shutdown = CancellationToken::new();
                let token = shutdown.clone();
                let local = tokio::task::LocalSet::new();
                local.spawn_local(async move {
                    for i in 0..sessions {
                        let Some(stream) = rx.recv().await else {
                            return;
                        };
                        script(i, server_side(stream)).await;
                    }
                    let _last = rx.recv().await;
                    token.cancel();
                    std::future::pending::<()>().await;
                });
                let (_report_tx, mut reports) = mpsc::channel::<Sample>(1);
                let start = Instant::now();
                local
                    .run_until(run_with(&s, &dialer, &mut reports, &shutdown))
                    .await;
                let times = dialer.dial_times.borrow();
                times.iter().map(|t| *t - start).collect()
            })
        })
    }

    /// 斷線日誌（「連線中斷」開頭）的等級與內容。
    fn disconnect_logs(logs: &[(log::Level, String)]) -> Vec<(log::Level, String)> {
        logs.iter()
            .filter(|(_, m)| m.starts_with("連線中斷"))
            .cloned()
            .collect()
    }

    #[test]
    fn backoff_reset_rules_including_superseded() {
        // X1 + T6：穩定連線後正常斷線 → 重置；短連線 → 不重置；
        // 穩定連線後第一次被 superseded → 照常重置；連續被取代 → 不重置、error、延遲至少 30 秒
        let (dials, logs) = run_script(settings(1, 5), 5, |i, mut f| async move {
            accept_hello(&mut f).await;
            let up = if matches!(i, 1 | 3) { 1 } else { 31 };
            ack_for(&mut f, Duration::from_secs(up)).await;
            if i >= 2 {
                f.send(
                    r#"{"type":"error","seq":null,"code":"superseded","message":"replaced by a newer connection","fatal":true}"#
                        .to_string(),
                )
                .await
                .unwrap();
            }
        });
        assert_eq!(dials.len(), 6, "{dials:?}");
        let gaps: Vec<f64> = dials
            .windows(2)
            .map(|w| (w[1] - w[0]).as_secs_f64())
            .collect();
        let within = |gap: f64, session: f64, lo: f64, hi: f64| {
            let delay = gap - session;
            assert!(
                delay >= lo - 0.01 && delay <= hi + 0.01,
                "delay {delay} not in [{lo}, {hi}]；gaps {gaps:?}"
            );
        };
        // 第 1 次：維持 31 秒後斷線 → 退避重置，延遲 2 s ±20%
        within(gaps[0], 31.0, 1.6, 2.4);
        // 第 2 次：只維持 1 秒 → 不重置，延遲 4 s ±20%
        within(gaps[1], 1.0, 3.2, 4.8);
        // 第 3 次：維持 31 秒後第一次被 superseded → 照常重置，延遲 2 s ±20%（不沿用舊的失敗計數）
        within(gaps[2], 31.0, 1.6, 2.4);
        // 第 4 次：連續第 2 次被取代（只維持 1 秒）→ 基準 4 s，但至少 30 秒
        within(gaps[3], 1.0, 30.0, 30.0);
        // 第 5 次：連續第 3 次被取代，即使維持 31 秒也不重置 → 基準 8 s，至少 30 秒
        within(gaps[4], 31.0, 30.0, 30.0);

        let d = disconnect_logs(&logs);
        assert_eq!(d.len(), 5, "{d:?}");
        assert!(d[0].1.contains("連續第 1 次失敗") && d[0].1.contains("退避已重置"));
        assert_eq!(d[0].0, log::Level::Warn);
        assert!(d[1].1.contains("連續第 2 次失敗") && d[1].1.contains("退避不重置"));
        // 穩定連線後第一次被取代：warn，重置退避
        assert!(d[2].1.contains("superseded") && d[2].1.contains("首次被取代，退避已重置"));
        assert!(d[2].1.contains("連續第 1 次失敗"), "{}", d[2].1);
        assert_eq!(d[2].0, log::Level::Warn, "{}", d[2].1);
        // 連續被取代：error，提示 agent.id 可能被共用，延遲有下限
        assert_eq!(d[3].0, log::Level::Error, "{}", d[3].1);
        assert!(d[3].1.contains("已連續 2 次"), "{}", d[3].1);
        assert!(d[3].1.contains("至少 30 秒"), "{}", d[3].1);
        // 第 3 次連續被取代：不重置；error 已在節流視窗內，改記 warn
        assert!(d[4].1.contains("連續被取代，退避不重置"), "{}", d[4].1);
        assert!(d[4].1.contains("連續第 3 次失敗"), "{}", d[4].1);
        assert_eq!(d[4].0, log::Level::Warn, "{}", d[4].1);
        assert_log_safe(&logs);
    }

    #[test]
    fn repeated_superseded_delay_ignores_small_max_delay() {
        // X1：max_delay = 1 時，連續被取代後延遲仍至少 30 秒，不會每秒互踢；error 有節流
        let mut s = settings(1, 5);
        s.initial_delay = Duration::from_secs(1);
        s.max_delay = Duration::from_secs(1);
        let (dials, logs) = run_script(s, 4, |_, mut f| async move {
            accept_hello(&mut f).await;
            ack_for(&mut f, Duration::from_secs(1)).await;
            f.send(r#"{"type":"error","code":"superseded","fatal":true}"#.to_string())
                .await
                .unwrap();
        });
        assert_eq!(dials.len(), 5, "{dials:?}");
        let delays: Vec<f64> = dials
            .windows(2)
            .map(|w| (w[1] - w[0]).as_secs_f64() - 1.0)
            .collect();
        // 第一次被取代：照 max_delay（1 s，抖動後 0.8～1 s）
        assert!(delays[0] >= 0.79 && delays[0] <= 1.01, "{delays:?}");
        // 第 2 次起：至少 30 秒
        for d in &delays[1..] {
            assert!(*d >= 30.0 - 0.01, "{delays:?}");
        }
        let d = disconnect_logs(&logs);
        assert_eq!(d.len(), 4, "{d:?}");
        let levels: Vec<_> = d.iter().map(|(l, _)| *l).collect();
        assert_eq!(
            levels,
            [
                log::Level::Warn,
                log::Level::Error,
                log::Level::Warn,
                log::Level::Warn
            ],
            "{d:?}"
        );
    }

    #[test]
    fn superseded_streak_is_broken_by_other_errors() {
        let superseded = r#"{"type":"error","code":"superseded","message":"","fatal":true}"#;
        let (_, logs) = run_script(settings(1, 5), 3, move |i, mut f| async move {
            accept_hello(&mut f).await;
            ack_for(&mut f, Duration::from_secs(1)).await;
            // 第 0、2 次被取代；中間第 1 次是一般斷線
            if i != 1 {
                f.send(superseded.to_string()).await.unwrap();
            }
        });
        let d = disconnect_logs(&logs);
        assert_eq!(d.len(), 3, "{d:?}");
        assert_eq!(d[0].0, log::Level::Warn);
        assert_eq!(
            d[2].0,
            log::Level::Warn,
            "中間有其他斷線原因，不算連續：{}",
            d[2].1
        );
        assert!(!d[2].1.contains("已連續"), "{}", d[2].1);
    }

    #[test]
    fn fatal_auth_error_is_logged_once_with_hint() {
        // P6：同一錯誤只產生一則日誌，提示合併在同一行
        let mut s = settings(1, 5);
        s.agent_id = "6f1c0e4a-1b2c-4d3e-9f00-112233445566".into();
        let (_, logs) = run_script(s, 1, |_, mut f| async move {
            let _ = f.next().await; // hello
            f.send(
                r#"{"type":"error","seq":1,"code":"auth_failed","message":"agent_id does not match certificate","fatal":true}"#
                    .to_string(),
            )
            .await
            .unwrap();
        });
        let related: Vec<_> = logs
            .iter()
            .filter(|(_, m)| m.contains("auth_failed") || m.contains("SAN"))
            .collect();
        assert_eq!(related.len(), 1, "{related:?}");
        let (level, msg) = related[0];
        assert_eq!(*level, log::Level::Error);
        assert!(msg.contains("agent_id does not match certificate"), "{msg}");
        assert!(
            msg.contains("DNS:6f1c0e4a-1b2c-4d3e-9f00-112233445566"),
            "{msg}"
        );
        assert!(!msg.contains("  "), "{msg:?}");
    }
}
