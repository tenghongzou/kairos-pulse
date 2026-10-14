//! 與 Kairos 之間的單一長 TLS 連線。
//!
//! 流程：TCP connect（5 s）→ TLS 握手（10 s，rustls）→ `Framed<_, LinesCodec>`（NDJSON）
//! → 送 hello、等 hello_ack（5 s）→ `select!` 迴圈同時處理 heartbeat、report、
//! 伺服器訊息與讀取閒置逾時。任何錯誤都會關閉連線並依退避策略無限重連；
//! 收到結束要求（Ctrl+C 等）時，若已建立 TLS 連線會先送 close_notify 再結束
//! （訊息只送出一部分時例外，見 [`Connection::close_gracefully`]）。
//!
//! 伺服器送來的文字（error 的 code / message、無法解析的原始行），以及可能夾帶伺服器內容的
//! TLS／I/O 錯誤文字（例如憑證名稱不符時列出的伺服器憑證 SAN），一律經
//! [`sanitize_for_log`] 轉義控制字元並截斷後才寫進日誌，避免偽造日誌行或夾帶終端跳脫序列。
//!
//! 沒有協商 `durable` 時（舊版 Kairos，或停用本機緩衝）report 為 at-most-once，一次只有一筆在途：
//! 上一筆 report 尚未收到伺服器回應（ack 或 error）時，新的取樣直接略過（節流 WARN 記錄略過次數），
//! 避免伺服器寫檔緩慢時在伺服器端無限積壓。
//!
//! 協商了 `durable` 時為 at-least-once（A5，[`serve_durable`]）：取樣先寫進本機緩衝（`spool`），
//! 連線 task 依序送出、收到 ack 才刪除；同一時間最多 [`DURABLE_IN_FLIGHT`] 筆在途，
//! 兩筆之間至少間隔 `buffer.replay_interval_ms`（低於 Kairos 的 report 速率限制）。

use std::collections::VecDeque;
use std::convert::Infallible;
use std::fmt;
use std::io;
use std::sync::Arc;
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
use crate::codes::{self, Event};
use crate::config_manager::AppConfig;
use crate::lifecycle::{self, HelloAckExtra};
use crate::local_status::{self, Disconnected, Registered};
use crate::log_throttle::{DEFAULT_WINDOW, LogThrottle, suppressed_note};
use crate::protocol::{
    CLIENT_FEATURES, ClientMsg, DurableSeq, HelloExt, HostInfo, MAX_LINE_LENGTH, Proto, ProtoOffer,
    SUPPORTED_PROTO, ServerMsg, ServerMsgError, Session, clock_skew_warning, hello_features,
    now_ts, parse_server_msg, report_message, sanitize_cpu_usage,
};
use crate::rotation::{Rotation, RotationLine};
use crate::service_status;
use crate::spool::{EntryInfo, SharedSpool, Spool, SpoolRecord};
use crate::system_info::{self, Sample};
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
/// 需大於 Kairos 最壞的回應延遲 7.55 秒：訊息速率延後 0.05 + report 限速延後 0.5
/// + `timeouts.write_secs` 上限 6 + 寬限 1。
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
/// 「連續被取代」與「伺服器致命錯誤」的 error 日誌節流視窗；視窗內的其他次數改記 warn。
const ATTENTION_LOG_WINDOW: Duration = Duration::from_secs(600);
/// 啟動時與每次斷線後第一次重連前的隨機延遲上限：`min(heartbeat.interval, MAX_CONNECT_SPREAD)`。
///
/// Kairos 重新啟動時所有 agent 同時斷線，若都以相同的初始退避（2 秒 ±20%）重連，
/// 會在約 1 秒內湧入；另加 0～5 秒的均勻分散，把同時重連攤到約 6 秒。
/// 上限取心跳間隔：分散不超過一個心跳週期，對伺服器的離線判定（5 × 心跳）沒有影響；
/// 另以 5 秒封頂，避免心跳間隔很長時啟動與恢復連線被拖慢。
const MAX_CONNECT_SPREAD: Duration = Duration::from_secs(5);
/// 伺服器 error 的 `code` 寫進日誌時的長度上限（字元）。
const LOG_CODE_LIMIT: usize = 64;
/// 伺服器送來的其他文字（error 的 `message`、無法解析的行）寫進日誌時的長度上限（字元）。
const LOG_TEXT_LIMIT: usize = 200;
/// TLS／I/O 錯誤文字寫進日誌時的長度上限（字元）；憑證名稱不符時會列出伺服器憑證的名稱，
/// 因此比 `LOG_TEXT_LIMIT` 寬一些。
const LOG_ERROR_LIMIT: usize = 400;
/// Kairos 因 CRL 過期或無法判定撤銷狀態而拒絕時，`certificate_rejected` 的 message（與 Kairos 約定）。
const REVOCATION_UNAVAILABLE: &str = "revocation status unavailable";

/// 啟動後、第一次註冊完成前的 systemd 狀態文字（含啟動時的隨機延遲）。
const STARTING_STATUS: &str = "連線中";

/// 連線中斷超過這麼久時記一次 `conn.outage`（ERROR），`pulse status` 的本機狀態也改為「嚴重」
/// （`cli-spec.md` §4.2「連線中斷 30 分鐘」、`log-ux.md` §5.1）
pub const OUTAGE_ERROR_AFTER: Duration = Duration::from_secs(30 * 60);

/// 本機時鐘與 Kairos 相差超過這麼久時記 `PLS-W8301`（`cli-spec.md` §4.2「時鐘」列的通過門檻）
const CLOCK_SKEW_NOTICE: chrono::TimeDelta = chrono::TimeDelta::seconds(30);

/// 依節流狀態記錄日誌：同一類別每 `DEFAULT_WINDOW` 最多一則，並附上被略過的次數。
macro_rules! throttled {
    ($throttle:expr, $level:ident, $($arg:tt)+) => {
        if let Some(n) = $throttle.check(Instant::now()) {
            log::$level!("{}{}", format_args!($($arg)+), suppressed_note(n, DEFAULT_WINDOW));
        }
    };
}

/// `storage_unavailable` 的 WARN 節流視窗：Kairos 的每日檔案大小上限滿時，
/// 每筆 report 都會收到 storage_unavailable 並持續到換日，10 秒一則會洗版一整天。
const STORAGE_UNAVAILABLE_LOG_WINDOW: Duration = Duration::from_secs(300);

/// 「伺服器送了怪東西」各類日誌的節流狀態；跨連線保留，避免重連後又重新灌爆日誌。
#[derive(Debug)]
struct ServerLogThrottles {
    /// 無法解析的行
    unparsable: LogThrottle,
    /// 不認得的訊息類型（DEBUG；A14）
    unknown_type: LogThrottle,
    /// 註冊後又收到 hello_ack
    unexpected_hello_ack: LogThrottle,
    /// 註冊後的非致命 error（storage_unavailable 除外）
    nonfatal_error: LogThrottle,
    /// 伺服器無法保存 report（storage_unavailable）
    storage_unavailable: LogThrottle,
    /// 不合預期的 ack（註冊前收到、或 seq 超前）
    unexpected_ack: LogThrottle,
    /// 上一筆 report 尚未收到回應而略過的取樣
    report_skipped: LogThrottle,
    /// durable：伺服器拒收（invalid_report、bad_request）而丟棄的緩衝記錄
    report_rejected: LogThrottle,
    /// durable：讀取本機緩衝失敗或記錄損毀
    spool_error: LogThrottle,
    /// durable：超過 buffer.max_age_hours 而丟棄的記錄
    spool_expired: LogThrottle,
    /// 與 Kairos 的時鐘偏差（`clock.skew`，跨連線保留：只在等級改變時記錄）
    clock_skew: ClockSkewLog,
}

impl Default for ServerLogThrottles {
    fn default() -> Self {
        Self {
            unparsable: LogThrottle::default(),
            unknown_type: LogThrottle::default(),
            unexpected_hello_ack: LogThrottle::default(),
            nonfatal_error: LogThrottle::default(),
            storage_unavailable: LogThrottle::new(STORAGE_UNAVAILABLE_LOG_WINDOW),
            unexpected_ack: LogThrottle::default(),
            report_skipped: LogThrottle::default(),
            report_rejected: LogThrottle::new(STORAGE_UNAVAILABLE_LOG_WINDOW),
            spool_error: LogThrottle::new(STORAGE_UNAVAILABLE_LOG_WINDOW),
            spool_expired: LogThrottle::new(STORAGE_UNAVAILABLE_LOG_WINDOW),
            clock_skew: ClockSkewLog::default(),
        }
    }
}

/// 依「類別」節流的 error 日誌：同一類別每 `ATTENTION_LOG_WINDOW` 最多一則 error，
/// 其餘改記 warn；類別改變時立即以 error 記錄。用於每次重連都會重複發生、需要人工處理的錯誤
/// （伺服器致命錯誤、協定版本不符），避免退避上限很小時每分鐘一則 ERROR。
#[derive(Debug)]
struct AttentionLog {
    key: Option<String>,
    throttle: LogThrottle,
}

impl AttentionLog {
    fn new() -> Self {
        Self {
            key: None,
            throttle: LogThrottle::new(ATTENTION_LOG_WINDOW),
        }
    }

    /// 同 [`LogThrottle::check`]；`key` 與上次不同時重新起算。
    fn check(&mut self, key: &str, now: Instant) -> Option<u64> {
        if self.key.as_deref() != Some(key) {
            self.key = Some(key.to_string());
            self.throttle = LogThrottle::new(ATTENTION_LOG_WINDOW);
        }
        self.throttle.check(now)
    }
}

/// 需要人工處理的斷線在節流判斷（[`AttentionLog::check`] 等）之後的處置：
/// 記錄的等級，以及是否同時寫一行到 stderr（讓 systemd 的 journal 看得到）。
///
/// 只有以 ERROR 記錄的那一則（同一原因每 `ATTENTION_LOG_WINDOW` 最多一則）才寫 stderr；
/// 視窗內改記 WARN 的不寫，避免 journal 洪水。一般連線錯誤不經過這裡，不寫 stderr。
fn attention_outcome(check: Option<u64>) -> (log::Level, bool) {
    match check {
        Some(_) => (log::Level::Error, true),
        None => (log::Level::Warn, false),
    }
}

/// 依節流結果以 error 或 warn 記錄斷線訊息；以 error 記錄時同時寫一行到 stderr。
fn log_attention(check: Option<u64>, msg: &str, what: &str) {
    let msg = match check {
        Some(n) if n > 0 => format!("{msg}（上次以 ERROR 記錄後，另有 {n} 次{what}已改記 WARN）"),
        _ => msg.to_string(),
    };
    let (level, to_stderr) = attention_outcome(check);
    log::log!(level, "{msg}");
    if to_stderr {
        service_status::attention(&msg);
    }
}

/// 連續失敗達 `retry.max_retries` 後的一般連線錯誤（拒絕連線、逾時、伺服器關閉連線等）：
/// 依節流結果以 error 或 warn 記錄（Q6）。與 [`log_attention`] 相同的 10 分鐘視窗，
/// 但這類錯誤不需人工介入即可能自行恢復（例如 Kairos 停機維護），因此不寫 stderr。
///
/// 沒有節流時，Kairos 停機一天、退避上限 60 秒的 agent 約產生 1,440 行 ERROR。
fn log_retries_exhausted(check: Option<u64>, msg: &str) {
    match check {
        Some(0) => log::error!("{msg}"),
        Some(n) => log::error!("{msg}（上次以 ERROR 記錄後，另有 {n} 次連線失敗已改記 WARN）"),
        None => log::warn!("{msg}"),
    }
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
    pub max_retries: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    /// 已設定的客戶端憑證路徑（用於錯誤提示）
    pub client_cert: Option<String>,
    /// 啟動時與斷線後第一次重連前的隨機延遲上限（見 [`MAX_CONNECT_SPREAD`]）
    pub connect_spread: Duration,
    /// 協定 v2 hello 的主機資訊（主機指紋、OS、顯示名稱、標籤、report 間隔）；
    /// 只在 hello 中附上，舊版 Kairos 會忽略
    pub host: HostInfo,
    /// durable：兩筆 report 之間的最短間隔（`buffer.replay_interval_ms`，不超過取樣間隔的 60%，
    /// 見 [`AppConfig::effective_replay_interval_ms`]）
    pub replay_interval: Duration,
    /// 已註冊時附加在 systemd 狀態文字後的提醒（例如無法開啟本機緩衝 `PLS-E4004`）
    pub status_note: Option<String>,
    /// hello 另外宣告的 features（例如身分由 `pulse enroll` 管理時的 `renew`）
    pub extra_features: Vec<String>,
    /// 8443 正常輪替（ENROLL.md 第 13.1 節）；身分不是由 `pulse enroll` 管理時為 `None`
    pub rotation: Option<Arc<std::sync::Mutex<Rotation>>>,
    /// Kairos 以 TLS alert `CertificateExpired` 拒絕用戶端憑證時結束（改走 8444 過期續簽）
    pub grace_on_expiry: bool,
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
            max_retries: config.retry.max_retries,
            initial_delay: Duration::from_secs(config.retry.initial_delay),
            max_delay: Duration::from_secs(config.retry.max_delay),
            client_cert: Some(config.server.client_cert.trim().to_string())
                .filter(|p| !p.is_empty()),
            connect_spread: connect_spread(Duration::from_secs(config.heartbeat.interval)),
            // 讀取機器 ID 與作業系統資訊（都很快），只在啟動時做一次
            host: system_info::host::collect(
                &config.agent.display_name,
                &config.agent.tags,
                Duration::from_secs(config.report.interval),
            ),
            replay_interval: Duration::from_millis(config.effective_replay_interval_ms()),
            status_note: None,
            extra_features: Vec::new(),
            rotation: None,
            grace_on_expiry: false,
        }
    }

    /// 讀取閒置逾時：`max(2 × hb, hb + 10 秒)`，`hb = heartbeat.interval`。
    ///
    /// 只看 heartbeat：report 依賴取樣任務（sysinfo 可能卡住），不保證按時送出；
    /// heartbeat 由連線任務自己定時送出。最壞情況下，距上一則伺服器訊息約 `hb` 之後才送出
    /// 下一則 heartbeat，伺服器再延遲最多約 8 秒才回應；`hb + 10 秒` 確保這種情況不會被誤判為斷線，
    /// `2 × hb` 則讓長間隔時至少容忍漏掉一次回應。
    fn read_idle_timeout(&self) -> Duration {
        let hb = self.heartbeat_interval;
        hb.saturating_mul(2)
            .max(hb.saturating_add(READ_IDLE_MARGIN))
    }
}

/// 啟動與第一次重連的隨機延遲上限：`min(心跳間隔, MAX_CONNECT_SPREAD)`。
fn connect_spread(heartbeat_interval: Duration) -> Duration {
    heartbeat_interval.min(MAX_CONNECT_SPREAD)
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
    /// 經 8443 取得新的用戶端憑證（`renew_result`）：以新憑證重新連線
    Renewed,
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

/// I/O 錯誤（含 rustls 錯誤）的日誌用文字：可能含伺服器憑證中的名稱等外部內容，一律清理。
fn io_error_text(e: &io::Error) -> String {
    sanitize_for_log(&e.to_string(), LOG_ERROR_LIMIT)
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectTimeout => write!(f, "TCP 連線逾時（{} 秒）", CONNECT_TIMEOUT.as_secs()),
            Self::Connect(e) => write!(f, "TCP 連線失敗：{}", io_error_text(e)),
            Self::TlsTimeout => write!(f, "TLS 握手逾時（{} 秒）", TLS_TIMEOUT.as_secs()),
            Self::Tls(e) => write!(f, "TLS 握手失敗：{}", io_error_text(e)),
            Self::WriteTimeout => write!(f, "寫出逾時（{} 秒）", WRITE_TIMEOUT.as_secs()),
            Self::Codec(LinesCodecError::MaxLineLengthExceeded) => {
                write!(f, "伺服器送來的單行超過 {MAX_LINE_LENGTH} bytes")
            }
            Self::Codec(LinesCodecError::Io(e)) => write!(f, "連線 I/O 錯誤：{}", io_error_text(e)),
            Self::Serialize(e) => write!(f, "訊息序列化失敗：{e}"),
            Self::HelloAckTimeout => write!(
                f,
                "送出 hello 後 {} 秒內未收到 hello_ack",
                HELLO_ACK_TIMEOUT.as_secs()
            ),
            Self::ProtoMismatch(p) => {
                write!(
                    f,
                    "伺服器回覆的協定版本 {p} 不在本程式支援的範圍（{}～{}）內",
                    SUPPORTED_PROTO.min, SUPPORTED_PROTO.max
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
            Self::Renewed => write!(f, "已取得新的用戶端憑證，以新憑證重新連線"),
        }
    }
}

type Result<T> = std::result::Result<T, SessionError>;

/// 建立連線的方式。正式環境為 TCP + TLS（[`TlsDialer`]）；測試時換成記憶體內的串流，
/// 以便在 `tokio::time::pause()` 下驗證逾時與退避邏輯。
trait Dialer {
    type Stream: AsyncRead + AsyncWrite + Unpin;

    /// 建立一條連線（含 TLS 握手），並回傳這條連線的 TLS 資訊（註冊成功時寫進日誌）。
    async fn dial(&self) -> Result<(Self::Stream, TlsInfo)>;

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

    async fn dial(&self) -> Result<(Self::Stream, TlsInfo)> {
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

    /// 送出一行已序列化的 JSON（`kairos-proto` 尚未定義的訊息，見 `lifecycle`）
    async fn send_line(&mut self, line: String) -> Result<()> {
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
                log::warn!("送出 TLS close_notify 失敗：{}", io_error_text(&e));
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

/// [`run`] 結束的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunExit {
    /// `shutdown` 被觸發
    Stopped,
    /// Kairos 確認用戶端憑證已過期（`settings.grace_on_expiry`）：改走過期續簽
    CertExpired,
    /// 經 8443 取得新憑證（`client.next.pem`）：以新憑證確認後取代
    Renewed,
}

/// 連線主迴圈：無限重連，直到 `shutdown` 被觸發才返回。
///
/// `reports` 為 at-most-once 時的即時取樣；`spool` 為本機緩衝（停用時 `None`，不宣告 `durable`）。
pub async fn run(
    settings: Settings,
    tls: TlsSetup,
    reports: &mut mpsc::Receiver<Sample>,
    spool: Option<Arc<SharedSpool>>,
    shutdown: CancellationToken,
) -> RunExit {
    let dialer = TlsDialer {
        settings: &settings,
        tls: &tls,
    };
    run_with(&settings, &dialer, reports, spool.as_ref(), &shutdown).await
}

/// [`run`] 的實作，連線方式由 `dialer` 提供。
async fn run_with<D: Dialer>(
    settings: &Settings,
    dialer: &D,
    reports: &mut mpsc::Receiver<Sample>,
    spool: Option<&Arc<SharedSpool>>,
    shutdown: &CancellationToken,
) -> RunExit {
    let mut backoff = Backoff::new(settings.initial_delay, settings.max_delay);
    let mut throttles = ServerLogThrottles::default();
    // 連續因 superseded 而斷線的次數（其他原因斷線即歸零）
    let mut superseded_streak: u32 = 0;
    // 「連續被取代」的 error 日誌節流（跨連線保留）
    let mut superseded_log = LogThrottle::new(ATTENTION_LOG_WINDOW);
    // 伺服器致命錯誤、協定版本不符、客戶端憑證被拒的 error 日誌節流（跨連線保留，依錯誤碼分類）
    let mut attention_log = AttentionLog::new();
    // 連續失敗達 max_retries 後一般連線錯誤的 error 日誌節流；連線穩定（退避重置）時重新起算，
    // 下一次中斷達到 max_retries 時立即以 error 記錄
    let mut retries_exhausted_log = LogThrottle::new(ATTENTION_LOG_WINDOW);
    let has_client_cert = settings.client_cert.is_some();

    // systemctl status 從啟動起就有 Status: 行（否則第一次註冊或斷線前都是空的）
    service_status::set_status(STARTING_STATUS);
    local_status::record(local_status::State::connecting);

    // 啟動時先隨機延遲 0～connect_spread 再連線：大量主機同時開機或同時重啟 Pulse 時分散連線
    let startup_delay = crate::backoff::random_spread(settings.connect_spread);
    if !startup_delay.is_zero() {
        log::info!(
            "{:.1} 秒後開始連線（啟動時隨機延遲 0～{} 秒，避免大量 agent 同時連線）",
            startup_delay.as_secs_f64(),
            settings.connect_spread.as_secs()
        );
        tokio::select! {
            _ = time::sleep(startup_delay) => {}
            _ = shutdown.cancelled() => {
                log::debug!("啟動延遲中收到結束要求，連線任務已停止");
                return RunExit::Stopped;
            }
        }
    }

    loop {
        let mut registered_at = None;
        let err = match session(
            settings,
            dialer,
            reports,
            spool,
            &mut registered_at,
            &mut throttles,
            shutdown,
        )
        .await
        {
            Ok(never) => match never {},
            Err(SessionError::Shutdown) => {
                log::debug!("連線任務已停止");
                return RunExit::Stopped;
            }
            Err(SessionError::Renewed) => {
                log::info!("已取得新的用戶端憑證，結束目前的連線，改以新憑證連線");
                return RunExit::Renewed;
            }
            Err(e) => e,
        };
        if let Some(rot) = &settings.rotation {
            rot.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .on_disconnect();
        }

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
                retries_exhausted_log = LogThrottle::new(ATTENTION_LOG_WINDOW);
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
        if backoff.failures() == 1 {
            // 斷線後第一次重連（例如 Kairos 重新啟動，所有 agent 同時斷線）：另加 0～connect_spread
            // 的隨機延遲。之後的重試已由指數退避與 ±20% 抖動錯開
            delay = delay.saturating_add(crate::backoff::random_spread(settings.connect_spread));
        }
        let mut floor_note = "";
        if repeated_superseded && delay < SUPERSEDED_MIN_DELAY {
            delay = SUPERSEDED_MIN_DELAY;
            floor_note = "（連續被取代時延遲至少 30 秒，不受 retry.max_delay 限制）";
        }
        let failures = backoff.failures();
        let code = error_code(&err, superseded_streak, has_client_cert).or_else(|| {
            // 握手完成前、伺服器也還沒要求客戶端憑證就被關閉：多半是每 IP 連線上限（審計 A7）
            matches!(&err, SessionError::Tls(e) if closed_before_handshake(e))
                .then_some(codes::E_PER_IP_LIMIT)
                .filter(|_| !dialer.client_cert_was_requested())
        });
        let msg = format!(
            "{}連線中斷（連續第 {failures} 次失敗）：{err}{hint}{uptime_note}；{:.1} 秒後重連{floor_note}",
            codes::tag(disconnect_event(code), code),
            delay.as_secs_f64()
        );
        local_status::record(|s| {
            s.disconnected(Disconnected {
                code,
                message: err.to_string(),
                attention: repeated_superseded || attention_key(&err, has_client_cert).is_some(),
                consecutive_failures: u64::from(failures),
                retry_in: delay,
            });
        });
        if repeated_superseded {
            // 互踢會持續發生：error 每 ATTENTION_LOG_WINDOW 最多一則，其餘記 warn
            log_attention(
                superseded_log.check(Instant::now()),
                &msg,
                "「連續被取代」的斷線",
            );
        } else if let Some(key) = attention_key(&err, has_client_cert) {
            // 伺服器致命錯誤（auth_failed、certificate_rejected、狀態表滿等）與客戶端憑證被拒的
            // TLS alert 每次重連都會重複：同一原因的 error 每 ATTENTION_LOG_WINDOW 最多一則，其餘記 warn
            log_attention(
                attention_log.check(&key, Instant::now()),
                &msg,
                "相同原因的斷線",
            );
        } else if failures >= settings.max_retries {
            // 伺服器長時間停機時每次重試都會到這裡：error 每 ATTENTION_LOG_WINDOW 最多一則，其餘記 warn
            log_retries_exhausted(retries_exhausted_log.check(Instant::now()), &msg);
        } else {
            log::warn!("{msg}");
        }
        service_status::set_status(&disconnect_status(&err, superseded_streak, has_client_cert));
        if local_status::tracker()
            .is_some_and(|t| t.update(|s| s.outage_reached(OUTAGE_ERROR_AFTER)))
        {
            let msg = format!(
                "{}已中斷 {} 分鐘，超過告警門檻：請確認 Kairos 狀態，或檢查本機到 {}:{} 的網路（服務會持續重連）",
                codes::tag(codes::CONN_OUTAGE, code),
                OUTAGE_ERROR_AFTER.as_secs() / 60,
                settings.address,
                settings.port
            );
            log::error!("{msg}");
            service_status::attention(&msg);
        }

        // Kairos 確認用戶端憑證已過期：改走 8444 過期續簽（設計包 enrollment-ux.md §8.1 (b)）
        if settings.grace_on_expiry
            && code == Some(codes::E_CLIENT_CERT_EXPIRED)
            && client_cert_alert(&err, has_client_cert)
                == Some(tokio_rustls::rustls::AlertDescription::CertificateExpired)
        {
            return RunExit::CertExpired;
        }

        tokio::select! {
            _ = time::sleep(delay) => {}
            _ = shutdown.cancelled() => {
                log::debug!("退避等待中收到結束要求，連線任務已停止");
                return RunExit::Stopped;
            }
        }
    }
}

/// 斷線原因是否需要人工處理（以 error 等級記錄並寫 stderr，不論連續失敗次數；實際記錄時另有節流）：
/// 伺服器送來的致命錯誤或註冊前的錯誤、協定版本不符、伺服器以 TLS alert 拒絕客戶端憑證、
/// 伺服器憑證驗證失敗（見 [`server_cert_problem`]）；
/// `superseded` 只在連續發生 `SUPERSEDED_STREAK_ERROR` 次以上時才算。
#[cfg(test)]
fn needs_attention(err: &SessionError, superseded_streak: u32, has_client_cert: bool) -> bool {
    match err {
        SessionError::Server { code, .. } if code == "superseded" => {
            superseded_streak >= SUPERSEDED_STREAK_ERROR
        }
        _ => attention_key(err, has_client_cert).is_some(),
    }
}

/// 斷線原因對應的錯誤代碼（`copywriting.md` §3.3、§3.4）；無法歸類時為 `None`。
fn error_code(
    err: &SessionError,
    superseded_streak: u32,
    has_client_cert: bool,
) -> Option<&'static str> {
    use tokio_rustls::rustls::AlertDescription as A;
    match err {
        SessionError::ConnectTimeout => Some(codes::E_CONN_TIMEOUT),
        SessionError::Connect(e) => connect_error_code(e, &e.to_string()),
        SessionError::TlsTimeout | SessionError::HelloAckTimeout | SessionError::IdleTimeout(_) => {
            Some(codes::E_SERVER_SILENT)
        }
        SessionError::ProtoMismatch(_) => Some(codes::E_PROTO_VERSION),
        SessionError::Codec(LinesCodecError::MaxLineLengthExceeded) => Some(codes::E_LINE_TOO_LONG),
        SessionError::Server { code, message, .. } => match code.as_str() {
            "superseded" if superseded_streak >= SUPERSEDED_STREAK_ERROR => {
                Some(codes::E_AGENT_ID_CONFLICT)
            }
            "superseded" => Some(codes::W_SUPERSEDED),
            "certificate_rejected" if message.trim() == REVOCATION_UNAVAILABLE => {
                Some(codes::E_SERVER_CRL)
            }
            "certificate_rejected" => Some(codes::E_CLIENT_CERT_REJECTED),
            "auth_failed" if message.contains("exactly one DNS SAN") => {
                Some(codes::E_CLIENT_CERT_MULTI_SAN)
            }
            "auth_failed" => Some(codes::E_CLIENT_CERT_MISMATCH),
            "unsupported_version" => Some(codes::E_PROTO_VERSION),
            "hello_required" => Some(codes::E_HELLO_REQUIRED),
            "too_long" => Some(codes::E_LINE_TOO_LONG),
            "bad_request" if message.contains("registry is full") => Some(codes::E_REGISTRY_FULL),
            "bad_request" => Some(codes::E_HELLO_BAD_REQUEST),
            _ => None,
        },
        _ => {
            if let Some(alert) = client_cert_alert(err, has_client_cert) {
                return Some(match alert {
                    A::CertificateExpired => codes::E_CLIENT_CERT_EXPIRED,
                    A::CertificateRequired => codes::E_CLIENT_CERT_REQUIRED,
                    A::UnknownCA => codes::E_CLIENT_CERT_UNTRUSTED,
                    // 撤銷，以及伺服器沒有細分原因的拒絕（copywriting §3.3 PLS-E1105）
                    _ => codes::E_CLIENT_CERT_REJECTED,
                });
            }
            if let Some((reason, _)) = server_cert_problem(err) {
                return match reason {
                    "不受信任" => Some(codes::E_SERVER_CERT_UNTRUSTED),
                    "名稱不符" => Some(codes::E_SERVER_CERT_NAME),
                    "已過期" | "尚未生效" => Some(codes::E_SERVER_CERT_VALIDITY),
                    "已被撤銷" => Some(codes::E_SERVER_CERT_REVOKED),
                    "用途不符" | "被標記為 CA" => Some(codes::E_SERVER_CERT_PURPOSE),
                    _ => None,
                };
            }
            None
        }
    }
}

/// TCP 連線失敗的錯誤代碼：拒絕連線 `PLS-E2003`、逾時或無法到達 `PLS-E2004`、名稱解析失敗 `PLS-E2002`
fn connect_error_code(e: &io::Error, text: &str) -> Option<&'static str> {
    match e.kind() {
        io::ErrorKind::ConnectionRefused => Some(codes::E_CONN_REFUSED),
        io::ErrorKind::TimedOut
        | io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkUnreachable => Some(codes::E_CONN_TIMEOUT),
        // std 的名稱解析錯誤沒有專屬的 ErrorKind，依訊息判斷（Linux：「failed to lookup address
        // information」；Windows：「No such host is known」）
        _ if text.contains("lookup address")
            || text.contains("No such host")
            || text.contains("nodename nor servname") =>
        {
            Some(codes::E_DNS)
        }
        _ => None,
    }
}

/// 斷線日誌的事件（`log-ux.md` §4.2）：需要人工處理的錯誤有專屬事件，其餘為 `conn.lost`
fn disconnect_event(code: Option<&str>) -> Event {
    match code {
        Some(
            codes::E_SERVER_CERT_UNTRUSTED
            | codes::E_SERVER_CERT_NAME
            | codes::E_SERVER_CERT_VALIDITY
            | codes::E_SERVER_CERT_REVOKED
            | codes::E_SERVER_CERT_PURPOSE,
        ) => codes::TLS_SERVER_CERT_REJECTED,
        Some(codes::E_CLIENT_CERT_REQUIRED) => codes::AUTH_CLIENT_CERT_REQUIRED,
        Some(codes::E_CLIENT_CERT_UNTRUSTED) => codes::AUTH_UNTRUSTED,
        Some(codes::E_CLIENT_CERT_REJECTED) => codes::AUTH_REJECTED,
        Some(codes::E_CLIENT_CERT_EXPIRED) => codes::AUTH_EXPIRED,
        Some(codes::E_CLIENT_CERT_MISMATCH | codes::E_CLIENT_CERT_MULTI_SAN) => {
            codes::AUTH_MISMATCH
        }
        Some(codes::E_SERVER_CRL) => codes::AUTH_SERVER_CRL,
        Some(codes::W_SUPERSEDED | codes::E_AGENT_ID_CONFLICT) => codes::CONN_SUPERSEDED,
        Some(codes::E_PROTO_VERSION) => codes::PROTO_INCOMPATIBLE,
        Some(codes::E_REGISTRY_FULL) => codes::SERVER_FULL,
        _ => codes::CONN_LOST,
    }
}

/// 同一等級下，偏差再變化超過這麼多才再記一則 `clock.skew`（`log-ux.md` §4.5）
const CLOCK_SKEW_RELOG_CHANGE: i64 = 60;

/// `clock.skew`（事件 5000）的記錄規則（`log-ux.md` §4.5、`decisions/INTEGRATION.md` §26）：
/// 依每次註冊時 hello_ack 的 `server_time` 量測，只在等級改變（30 秒～5 分鐘 `WARN` `PLS-W8301`、
/// 超過 5 分鐘 `ERROR` `PLS-E8302`）或同一等級下偏差再變化超過 1 分鐘時記錄；回到 30 秒以內時記一則
/// `INFO`（代碼為 `-`）。跨連線保留，重連時不重複記錄同樣的偏差。
#[derive(Debug, Default)]
struct ClockSkewLog {
    /// 最近一則記錄的等級與偏差（秒，正值為本機較快）；在 30 秒以內時為 `None`
    last: Option<(log::Level, i64)>,
}

impl ClockSkewLog {
    /// 量測一次；需要記錄時回傳等級與訊息。`server_time` 無法解析時不記錄、不改變狀態
    fn observe(
        &mut self,
        server_time: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<(log::Level, String)> {
        let server = chrono::DateTime::parse_from_rfc3339(server_time).ok()?;
        let skew = now - server.with_timezone(&chrono::Utc);
        let secs = skew.num_seconds();
        let level = if skew.abs() > crate::protocol::MAX_CLOCK_SKEW {
            log::Level::Error
        } else if skew.abs() > CLOCK_SKEW_NOTICE {
            log::Level::Warn
        } else {
            return self.last.take().map(|_| {
                (
                    log::Level::Info,
                    format!(
                        "{}本機時鐘與伺服器的差距已回到 30 秒以內（約 {secs} 秒）",
                        codes::tag(codes::CLOCK_SKEW, None)
                    ),
                )
            });
        };
        if let Some((last_level, last_secs)) = self.last
            && last_level == level
            && (secs - last_secs).abs() <= CLOCK_SKEW_RELOG_CHANGE
        {
            return None;
        }
        self.last = Some((level, secs));
        let msg = if level == log::Level::Error {
            format!(
                "{}{}",
                codes::tag(codes::CLOCK_SKEW, Some(codes::E_CLOCK_SKEW)),
                clock_skew_warning(server_time, now).unwrap_or_default()
            )
        } else {
            let (direction, abs) = if secs > 0 {
                ("快", secs)
            } else {
                ("慢", -secs)
            };
            format!(
                "{}本機時鐘比伺服器{direction}約 {abs} 秒（超過 30 秒）：report 的 ts 與日誌時間會不準確，\
                 請檢查本機的時間同步（NTP）",
                codes::tag(codes::CLOCK_SKEW, Some(codes::W_CLOCK_SKEW))
            )
        };
        Some((level, msg))
    }
}

/// rustls 的 TLS 版本名稱（`TLSv1_3`）改為一般寫法（`TLSv1.3`）
fn tls_version_name(version: &str) -> String {
    version.replace('_', ".")
}

/// 需要人工處理的斷線原因（`superseded` 除外，另有規則）的節流分類鍵；其他原因回傳 `None`。
fn attention_key(err: &SessionError, has_client_cert: bool) -> Option<String> {
    match err {
        SessionError::Server { code, .. } if code == "superseded" => None,
        SessionError::Server { code, fatal, .. } => Some(format!("server:{code}:{fatal}")),
        SessionError::ProtoMismatch(p) => Some(format!("proto:{p}")),
        _ => client_cert_alert(err, has_client_cert)
            .map(|a| format!("tls:{a:?}"))
            .or_else(|| {
                server_cert_problem(err).map(|(reason, _)| format!("server_cert:{reason}"))
            }),
    }
}

/// 伺服器憑證驗證失敗（本機以 rustls 驗證伺服器憑證時的錯誤）時回傳 `(原因, 提示摘要)`。
///
/// 不受信任、名稱不符、過期、被標記為 CA 等都需要人工處理（改設定、換憑證或校正時鐘），
/// 每次重連都會重複，因此視為持續性錯誤。拒絕連線、逾時、連線重設等一般網路錯誤不在此列。
fn server_cert_problem(err: &SessionError) -> Option<(&'static str, &'static str)> {
    use tokio_rustls::rustls::{CertificateError as C, Error as TlsError};
    let TlsError::InvalidCertificate(e) = err.rustls_error()? else {
        return None;
    };
    Some(match e {
        C::UnknownIssuer => (
            "不受信任",
            "簽發者不在信任庫中，自簽憑證或私有 CA 請以 server.ca_cert 指定",
        ),
        C::NotValidForName | C::NotValidForNameContext { .. } => {
            ("名稱不符", "請把 server.domain 設為伺服器憑證 SAN 中的名稱")
        }
        C::Expired | C::ExpiredContext { .. } => {
            ("已過期", "請伺服器管理者更新憑證，或檢查本機時鐘")
        }
        C::NotValidYet | C::NotValidYetContext { .. } => {
            ("尚未生效", "請檢查本機時鐘或伺服器憑證的有效期間")
        }
        C::Revoked => ("已被撤銷", "請伺服器管理者更換伺服器憑證"),
        C::InvalidPurpose | C::InvalidPurposeContext { .. } => {
            ("用途不符", "伺服器憑證的 EKU 必須含 serverAuth")
        }
        C::Other(other)
            if other.0.downcast_ref::<webpki::Error>()
                == Some(&webpki::Error::CaUsedAsEndEntity) =>
        {
            (
                "被標記為 CA",
                "伺服器憑證必須是 basicConstraints CA:FALSE 的終端憑證，請重新簽發",
            )
        }
        _ => ("驗證失敗", "詳見 logs/"),
    })
}

/// 伺服器以 TLS alert 拒絕本機客戶端憑證（撤銷、過期、不信任、未提供）時回傳該 alert。
///
/// 憑證被撤銷後，第一次斷線通常是伺服器的 `certificate_rejected`，之後每次重連都在握手時
/// 收到 `CertificateRevoked` 等 alert；兩者都需要人工處理，因此一併視為持續性錯誤。
/// 不信任類的 alert（`UnknownCA` 等）只在設定了客戶端憑證時才算（與 [`tls::describe_tls_error`] 一致）。
fn client_cert_alert(
    err: &SessionError,
    has_client_cert: bool,
) -> Option<tokio_rustls::rustls::AlertDescription> {
    use tokio_rustls::rustls::{AlertDescription as A, Error as TlsError};
    match err.rustls_error()? {
        TlsError::AlertReceived(
            a @ (A::CertificateRevoked | A::CertificateExpired | A::CertificateRequired),
        ) => Some(*a),
        TlsError::AlertReceived(
            a @ (A::UnknownCA
            | A::BadCertificate
            | A::CertificateUnknown
            | A::UnsupportedCertificate
            | A::AccessDenied),
        ) if has_client_cert => Some(*a),
        _ => None,
    }
}

/// 需要人工處理的斷線原因的「錯誤碼」與簡短提示（systemd 的 STATUS 用，完整提示在日誌中）；
/// 其他原因（一般連線錯誤、首次被取代等）回傳 `None`。
fn attention_summary(
    err: &SessionError,
    superseded_streak: u32,
    has_client_cert: bool,
) -> Option<(String, String)> {
    use tokio_rustls::rustls::AlertDescription as A;
    let (code, summary) = match err {
        SessionError::Server { code, .. } if code == "superseded" => {
            if superseded_streak < SUPERSEDED_STREAK_ERROR {
                return None;
            }
            (code.clone(), "多台主機可能共用同一份 agent.id".to_string())
        }
        SessionError::Server { code, message, .. } => {
            let summary = match code.as_str() {
                "certificate_rejected" if message.trim() == REVOCATION_UNAVAILABLE => {
                    "伺服器的 CRL 已過期或無法判定撤銷狀態，請聯絡伺服器管理者"
                }
                "certificate_rejected" => {
                    "客戶端憑證已被撤銷、過期或不受信任，請聯絡管理者重新簽發"
                }
                "auth_failed" => "伺服器拒絕本機身分，請確認憑證 SAN 為 DNS:<agent_id>",
                "unsupported_version" => "協定版本不符，請更新 Pulse 或 Kairos",
                "bad_request" if message.contains("registry is full") => {
                    "伺服器狀態表已滿，請管理者調高 limits.max_agents"
                }
                _ => "伺服器拒絕註冊，詳見 logs/",
            };
            let code = if code.is_empty() {
                "（未提供錯誤碼）".to_string()
            } else {
                code.clone()
            };
            (code, summary.to_string())
        }
        SessionError::ProtoMismatch(p) => (
            "proto_mismatch".to_string(),
            format!(
                "伺服器的協定版本 {p} 不在本程式支援的範圍（{}～{}）內，請更新 Pulse 或 Kairos",
                SUPPORTED_PROTO.min, SUPPORTED_PROTO.max
            ),
        ),
        _ => {
            let alert = client_cert_alert(err, has_client_cert)?;
            let summary = match alert {
                A::CertificateRevoked => "客戶端憑證已被撤銷，請聯絡管理者重新簽發",
                A::CertificateExpired => "客戶端憑證已過期，請聯絡管理者重新簽發",
                A::CertificateRequired => {
                    "伺服器要求客戶端憑證，請設定 server.client_cert / server.client_key"
                }
                _ => "伺服器不信任此客戶端憑證，請確認由伺服器信任的 client CA 簽發",
            };
            (format!("TLS alert {alert:?}"), summary.to_string())
        }
    };
    Some((code, summary))
}

/// 斷線後的 systemd 狀態文字：伺服器憑證驗證失敗時為「無法連線：伺服器憑證<原因>（<提示摘要>）」，
/// 其他需要人工處理的錯誤為「無法註冊：<錯誤碼>（<提示摘要>）」，其餘為「重新連線中」。
fn disconnect_status(err: &SessionError, superseded_streak: u32, has_client_cert: bool) -> String {
    if let Some((reason, summary)) = server_cert_problem(err) {
        return format!("無法連線：伺服器憑證{reason}（{summary}）");
    }
    match attention_summary(err, superseded_streak, has_client_cert) {
        Some((code, summary)) => format!("無法註冊：{code}（{summary}）"),
        None => "重新連線中".to_string(),
    }
}

/// 註冊成功後的 systemd 狀態文字：「已註冊：<伺服器位址>:<連接埠>」（IPv6 位址加上方括號），
/// 有提醒時接在「；」之後（例如「；無法開啟本機緩衝（PLS-E4004，…）」）。
fn registered_status(settings: &Settings) -> String {
    let address = settings.address.trim();
    let base = if address.contains(':') && !address.starts_with('[') {
        format!("已註冊：[{address}]:{}", settings.port)
    } else {
        format!("已註冊：{address}:{}", settings.port)
    };
    match &settings.status_note {
        Some(note) => format!("{base}；{note}"),
        None => base,
    }
}

/// 建立一條連線並服務到它結束為止；只會以錯誤返回。
///
/// 收到結束要求時：尚未建立連線則直接返回；已建立則先送 close_notify。
async fn session<D: Dialer>(
    settings: &Settings,
    dialer: &D,
    reports: &mut mpsc::Receiver<Sample>,
    spool: Option<&Arc<SharedSpool>>,
    registered_at: &mut Option<Instant>,
    throttles: &mut ServerLogThrottles,
    shutdown: &CancellationToken,
) -> Result<Infallible> {
    let (stream, tls_info) = tokio::select! {
        res = dialer.dial() => res?,
        _ = shutdown.cancelled() => return Err(SessionError::Shutdown),
    };
    let mut conn = Connection::new(stream);

    let result = tokio::select! {
        res = serve(&mut conn, settings, Some(&tls_info), reports, spool, registered_at, throttles) => res,
        _ = shutdown.cancelled() => Err(SessionError::Shutdown),
    };
    if matches!(result, Err(SessionError::Shutdown)) {
        conn.close_gracefully().await;
    }
    result
}

/// 在已建立的連線上：hello / hello_ack，之後進入主迴圈。
///
/// `tls_info` 為這條連線的 TLS 資訊，註冊成功時附在日誌中（測試中直接使用記憶體內串流時為 `None`）。
async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<S>,
    settings: &Settings,
    tls_info: Option<&TlsInfo>,
    reports: &mut mpsc::Receiver<Sample>,
    spool: Option<&Arc<SharedSpool>>,
    registered_at: &mut Option<Instant>,
    throttles: &mut ServerLogThrottles,
) -> Result<Infallible> {
    // ---- hello / hello_ack ----
    // 版本協商（A3）：proto 送 1（v1 Kairos 只接受 1），另以 proto_min／proto_max 宣告支援 1～2；
    // v2 的欄位（features、主機資訊）舊版 Kairos 會忽略
    let seq = conn.next_seq();
    conn.send(&ClientMsg::Hello {
        seq,
        proto: Proto::Version(SUPPORTED_PROTO.min),
        agent_id: settings.agent_id.clone(),
        hostname: settings.hostname.clone(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        heartbeat_interval_ms: u64::try_from(settings.heartbeat_interval.as_millis())
            .unwrap_or(u64::MAX),
        ts: now_ts(),
        ext: Box::new(HelloExt::new(
            ProtoOffer {
                proto_min: Some(SUPPORTED_PROTO.min),
                proto_max: Some(SUPPORTED_PROTO.max),
                // 停用本機緩衝時不宣告 durable（A5）；身分由 pulse enroll 管理時另外宣告 renew
                features: Some({
                    let mut f = hello_features(spool.is_some());
                    f.extend(settings.extra_features.iter().cloned());
                    f
                }),
            },
            settings.host.clone(),
        )),
    })
    .await?;
    log::debug!("已送出 hello（seq={seq}），等待 hello_ack");

    let ack = wait_hello_ack(conn, seq, throttles).await?;
    let mut session = ack.session;
    if spool.is_none() {
        // 沒有宣告 durable：即使伺服器宣告了也不啟用
        session.durable = false;
    }
    *registered_at = Some(Instant::now());
    if let Some(rot) = &settings.rotation {
        let negotiated = session.is_v2()
            && settings
                .extra_features
                .iter()
                .any(|f| f == lifecycle::FEATURE_RENEW)
            && ack
                .server_features
                .iter()
                .any(|f| f == lifecycle::FEATURE_RENEW);
        let offset = ack
            .server_time
            .as_deref()
            .and_then(|t| kairos_proto::enroll::parse_rfc3339_ms(t).ok())
            .map(|st| chrono::Utc::now().timestamp_millis() - st);
        rot.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .on_hello_ack(negotiated, &ack.extra, offset);
    }
    let (level, msg) = registered_log(settings, &ack, tls_info);
    log::log!(level, "{}{msg}", codes::tag(codes::CONN_REGISTERED, None));
    if let Some((level, msg)) = ack
        .server_time
        .as_deref()
        .and_then(|t| throttles.clock_skew.observe(t, chrono::Utc::now()))
    {
        log::log!(level, "{msg}");
        if level == log::Level::Error {
            // 超過 5 分鐘：需要人工以 NTP 修正（log-ux.md §4.5、§3）
            service_status::attention(&msg);
        }
    }
    service_status::set_status(&registered_status(settings));
    let durable_session = session.durable && spool.is_some();
    let restored = local_status::tracker().and_then(|t| {
        t.update(|s| {
            s.registered(Registered {
                tls_version: tls_info.map(|i| tls_version_name(&i.version)),
                server_verified: tls_info.is_none_or(|i| i.verified),
                proto: session.proto,
                features: [
                    (session.metrics, kairos_proto::FEATURE_METRICS),
                    (session.durable, kairos_proto::FEATURE_DURABLE),
                ]
                .into_iter()
                .filter(|(on, _)| *on)
                .map(|(_, f)| f.to_string())
                .collect(),
                durable: durable_session,
                server_time: ack
                    .server_time
                    .as_deref()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&chrono::Utc)),
            })
        })
    });
    if let Some(restored) = restored {
        let backlog = match spool {
            Some(shared) if durable_session => {
                let spool = shared.lock();
                let stats = spool.stats();
                format!(
                    "；待補送 {} 筆（最舊 {}），開始補送  delivery=durable backlog={} dropped_full={} dropped_expired={}",
                    stats.pending,
                    spool.oldest_sampled_at().map_or_else(
                        || "—".to_string(),
                        |t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                    ),
                    stats.pending,
                    stats.dropped_full,
                    stats.dropped_expired
                )
            }
            _ => "  delivery=at_most_once".to_string(),
        };
        log::info!(
            "{}連線已恢復：中斷 {:.1} 秒，重試 {} 次{backlog}",
            codes::tag(codes::CONN_RESTORED, None),
            restored.outage.as_secs_f64(),
            restored.attempts
        );
    }

    // 斷線期間殘留在佇列中的舊取樣直接丟棄（at-most-once，不重送舊資料）
    while reports.try_recv().is_ok() {}

    // 本機緩衝（A5）：協商了 durable 時改走 at-least-once 的迴圈；沒有協商（舊版 Kairos）時，
    // 這條連線期間的新取樣照舊即時送出（at-most-once），緩衝中的積壓保留到連上支援 durable 的 Kairos
    if let Some(shared) = spool {
        if session.durable {
            return serve_durable(conn, settings, session, shared, throttles).await;
        }
        let backlog = shared.lock().len();
        if backlog > 0 {
            log::info!(
                "伺服器不支援 durable（Kairos 0.2.0 以前的版本）：本機緩衝中的 {backlog} 筆積壓暫不補送，\
                 保留到連上支援的 Kairos；這條連線期間的取樣即時送出，送不出去就丟棄"
            );
        }
    }
    // 連線期間的取樣由收集 task 直接送到 `reports`（不寫入緩衝）；離開時（斷線）恢復寫入緩衝
    let _live = spool.map(|shared| shared.set_live(true));

    // ---- 主迴圈 ----
    let read_idle = settings.read_idle_timeout();
    let idle = time::sleep(read_idle);
    tokio::pin!(idle);

    let hb_period = settings.heartbeat_interval;
    let mut heartbeat = time::interval_at(after(hb_period), hb_period);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // 已送出、尚未收到伺服器回應（ack 或帶 seq 的 error）的 report seq；同一時間最多一筆。
    // 屬於這條連線的狀態，斷線時隨之清除
    let mut pending_report: Option<u64> = None;
    // 這條連線上因上一筆 report 未回應而略過的取樣數
    let mut skipped_reports: u64 = 0;
    let mut rotation_tick = time::interval(ROTATION_CHECK);
    rotation_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let seq = conn.next_seq();
                conn.send(&ClientMsg::Heartbeat { seq, ts: now_ts() }).await?;
                log::trace!("已送出 heartbeat（seq={seq}）");
            }
            _ = rotation_tick.tick(), if settings.rotation.is_some() => {
                send_renew_if_due(conn, settings).await?;
            }
            Some(sample) = reports.recv() => {
                if let Some(pending) = pending_report {
                    // 伺服器寫檔緩慢時不再堆送：report 為 at-most-once，這次取樣直接丟棄
                    skipped_reports = skipped_reports.saturating_add(1);
                    local_status::record(local_status::State::dropped_disconnected);
                    throttled!(
                        throttles.report_skipped,
                        warn,
                        "{}上一筆 report（seq={pending}）尚未收到伺服器回應，略過此次取樣（ts={}，這條連線已略過 {skipped_reports} 次）；\
                         伺服器寫入可能較慢",
                        codes::tag(codes::REPORT_SKIPPED, None),
                        sample.ts
                    );
                    continue;
                }
                let seq = conn.next_seq();
                let sanitized = sanitize_cpu_usage(sample.cpu_usage);
                if sanitized != sample.cpu_usage {
                    log::debug!("CPU 使用率 {} 不合理，送出前調整為 {sanitized}", sample.cpu_usage);
                }
                // v1（或沒有協商 metrics）送三個欄位；v2 送通用指標（A4）
                let (msg, summary) = report_message(seq, sample, session, None);
                conn.send(&msg).await?;
                pending_report = Some(seq);
                local_status::record(local_status::State::report_sent);
                log::debug!("已送出 report（seq={seq}，{summary}）");
            }
            line = conn.framed.next() => {
                let line = line.ok_or(SessionError::ClosedByServer)?.map_err(SessionError::Codec)?;
                if rotation_line(settings, &line)? {
                    idle.as_mut().reset(after(read_idle));
                    continue;
                }
                let outcome = handle_server_line(&line, conn.seq, throttles)?;
                // 只有合法的協定訊息才算「伺服器還活著」（與 Kairos 的閒置判定一致）；
                // 無法解析的行不重置閒置計時，持續送垃圾的伺服器仍會被判定為閒置
                if outcome.valid {
                    idle.as_mut().reset(after(read_idle));
                }
                // 伺服器依序回應：回應到 report 或其後的訊息，代表該 report 已有結果
                if let (Some(pending), Some(answered)) = (pending_report, outcome.answered)
                    && answered >= pending
                {
                    pending_report = None;
                    if answered == pending && matches!(parse_server_msg(&line), Ok(ServerMsg::Ack { .. })) {
                        local_status::record(local_status::State::report_acked);
                    } else if answered == pending {
                        local_status::record(local_status::State::report_rejected);
                    }
                }
            }
            _ = &mut idle => {
                return Err(SessionError::IdleTimeout(read_idle));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// durable：at-least-once 投遞（A5）
// ---------------------------------------------------------------------------

/// durable：同一時間最多在途的 report 筆數（協定上限為 [`kairos_proto::MAX_DURABLE_IN_FLIGHT`]）。
///
/// 預設每 1.2 秒送一筆時，往返延遲（含 Kairos 寫檔）在 4.8 秒以內都不會被視窗卡住（最小間隔
/// 0.5 秒時為 2 秒）；
/// Kairos 逐則處理，每條連線最多排 4 筆 report，伺服器端不會無限積壓。
const DURABLE_IN_FLIGHT: usize = 4;
const _: () = assert!(DURABLE_IN_FLIGHT <= kairos_proto::MAX_DURABLE_IN_FLIGHT);

/// `storage_unavailable` 或讀取緩衝失敗後，重送前的退避：從 2 秒加倍到 60 秒（另加 ±20% 抖動），
/// 收到 ack 後重置。Kairos 過載（寫檔逾時回 storage_unavailable）時，大量主機會自動放慢補送
const DURABLE_RETRY_INITIAL: Duration = Duration::from_secs(2);
const DURABLE_RETRY_MAX: Duration = Duration::from_secs(60);

/// durable 連線上一則伺服器回應對 report 的意義
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reply {
    /// 已保存（或先前已保存過）：從緩衝移除
    Ack,
    /// 沒有保存（`storage_unavailable` 與其他非 fatal 的錯誤）：保留，退避後重送。
    /// 內容為代碼（`PLS-W4101` 無法保存、`PLS-W2203` 速率限制、`PLS-W2107` 其他）
    Retry(&'static str),
    /// 永遠不會被接受（`invalid_report`、`bad_request`）：丟棄，不重送
    Rejected(String),
}

/// 一行伺服器訊息中的回應：`ack` 與非 fatal 的 `error`，回傳 (回應的 seq, 意義)；其他訊息為 `None`
fn durable_reply(line: &str) -> Option<(Option<u64>, Reply)> {
    match parse_server_msg(line).ok()? {
        ServerMsg::Ack { seq } => Some((Some(seq), Reply::Ack)),
        ServerMsg::Error {
            seq,
            code,
            message,
            fatal: false,
            ..
        } => Some((
            seq,
            match code.as_str() {
                "invalid_report" | "bad_request" => Reply::Rejected(code),
                "storage_unavailable" => Reply::Retry(storage_code(&message)),
                _ => Reply::Retry(codes::W_SERVER_UNEXPECTED),
            },
        )),
        _ => None,
    }
}

/// `storage_unavailable` 的代碼：速率限制（`report rate limit exceeded`）為 `PLS-W2203`，
/// 其餘（無法保存）為 `PLS-W4101`（`copywriting.md` §3.4）
fn storage_code(message: &str) -> &'static str {
    if message.contains("rate limit") {
        codes::W_REPORT_RATE
    } else {
        codes::W_SERVER_STORAGE
    }
}

/// 等到 `t`；`None` 時永遠等待（`select!` 中的停用分支）
async fn sleep_until_opt(t: Option<Instant>) {
    match t {
        Some(t) => time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// 一條 durable 連線的送出狀態。斷線即丟棄：緩衝中未確認的記錄（含已送出、還沒收到 ack 的）
/// 下一條連線從最舊的開始重送
#[derive(Debug)]
struct Delivery {
    /// 兩筆 report 之間的最短間隔（`buffer.replay_interval_ms`）
    interval: Duration,
    /// 已送出、尚未收到回應的訊息（含 heartbeat），依送出順序：(seq, 緩衝記錄編號)
    outstanding: VecDeque<(u64, Option<u64>)>,
    /// 其中的 report 筆數
    in_flight: usize,
    /// 下一筆要送的緩衝記錄編號（≥）
    cursor: u64,
    /// 下一筆 report 最早的送出時間（速率、退避與開始補送前的隨機延遲）
    next_allowed: Instant,
    /// 有記錄沒有保存或結果不明：等在途的都有回應後，從最舊的未確認記錄重送
    rewind: bool,
    retry: Backoff,
    /// 本連線確認送達的筆數
    acked: u64,
    /// 連線開始時的積壓筆數（0 表示沒有積壓、不記「補送完成」）
    backlog: usize,
    /// 開始補送的時間（`buffer.replay_done` 的 `duration_ms`）
    started: Instant,
}

impl Delivery {
    fn new(interval: Duration, start: Instant, backlog: usize) -> Self {
        Self {
            interval,
            outstanding: VecDeque::new(),
            in_flight: 0,
            cursor: 0,
            next_allowed: start,
            rewind: false,
            retry: Backoff::new(DURABLE_RETRY_INITIAL, DURABLE_RETRY_MAX),
            acked: 0,
            backlog,
            started: start,
        }
    }

    /// 下一筆 report 可以送出的時間；視窗已滿、等待重送前的回應或沒有待送記錄時為 `None`
    fn next_send_at(&self, spool: &Spool) -> Option<Instant> {
        if self.in_flight >= DURABLE_IN_FLIGHT || (self.rewind && self.in_flight > 0) {
            return None;
        }
        let cursor = if self.rewind { 0 } else { self.cursor };
        spool.first_at_or_after(cursor).map(|_| self.next_allowed)
    }

    /// 取出下一筆要送的記錄（先丟掉過期的；損毀的略過）。讀取失敗時退避並回傳 `None`
    fn prepare_next(
        &mut self,
        spool: &mut Spool,
        throttles: &mut ServerLogThrottles,
    ) -> Option<(EntryInfo, SpoolRecord)> {
        if self.rewind {
            if self.in_flight > 0 {
                return None;
            }
            self.cursor = 0;
            self.rewind = false;
        }
        let expired = spool.drop_expired(chrono::Utc::now());
        if expired > 0 {
            local_status::record(local_status::State::buffer_changed);
            throttled!(
                throttles.spool_expired,
                warn,
                "{}本機緩衝中有 {expired} 筆取樣超過 buffer.max_age_hours 設定的時間，已丟棄、不送出（累計 {} 筆）",
                codes::tag(codes::BUFFER_EXPIRED, Some(codes::W_BUFFER_EXPIRED)),
                spool.stats().dropped_expired
            );
        }
        loop {
            let entry = spool.first_at_or_after(self.cursor)?;
            match spool.read(entry.id) {
                Ok(Some(record)) => return Some((entry, record)),
                Ok(None) => self.cursor = entry.id + 1,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    spool.discard_corrupted(entry.id);
                    throttled!(throttles.spool_error, warn, "略過本機緩衝中損毀的記錄：{e}");
                }
                Err(e) => {
                    let delay = self.retry.next_delay();
                    self.next_allowed = Instant::now() + delay;
                    throttled!(
                        throttles.spool_error,
                        warn,
                        "無法讀取本機緩衝，{:.1} 秒後重試：{e}",
                        delay.as_secs_f64()
                    );
                    return None;
                }
            }
        }
    }

    /// 送出了一則訊息（report 時 `entry` 為緩衝記錄編號）
    fn sent(&mut self, seq: u64, entry: Option<u64>) {
        self.outstanding.push_back((seq, entry));
        if let Some(id) = entry {
            self.in_flight += 1;
            self.cursor = id + 1;
            self.next_allowed = Instant::now() + self.interval;
        }
    }

    /// 處理一則回應。伺服器依序回應每一則訊息：`seq` 為 `null` 的 error 回應的是最早的未回應訊息；
    /// 在回應的 seq 之前、卻沒有收到回應的訊息（不應發生）視為結果不明，之後重送
    fn on_reply(
        &mut self,
        seq: Option<u64>,
        last_sent: u64,
        reply: Reply,
        spool: &mut Spool,
        throttles: &mut ServerLogThrottles,
    ) {
        let answered = match seq {
            Some(s) if s != 0 && s <= last_sent => {
                while let Some(&(q, entry)) = self.outstanding.front() {
                    if q >= s {
                        break;
                    }
                    self.outstanding.pop_front();
                    if entry.is_some() {
                        self.in_flight -= 1;
                        self.rewind = true;
                    }
                }
                match self.outstanding.front() {
                    Some(&(q, _)) if q == s => self.outstanding.pop_front(),
                    _ => None,
                }
            }
            Some(_) => None,
            None => self.outstanding.pop_front(),
        };
        let Some((_, Some(id))) = answered else {
            return;
        };
        self.in_flight -= 1;
        match reply {
            Reply::Ack => {
                if spool.remove(id) {
                    self.acked += 1;
                }
                self.retry.reset();
                local_status::record(local_status::State::report_acked);
            }
            Reply::Retry(code) => {
                // 這筆沒有保存：在途的其他筆有回應後，退避一段時間再從最舊的未確認記錄重送
                self.rewind = true;
                let delay = self.retry.next_delay();
                self.next_allowed = self.next_allowed.max(Instant::now() + delay);
                let at = chrono::Utc::now()
                    + chrono::Duration::from_std(self.next_allowed - Instant::now())
                        .unwrap_or_default();
                local_status::record(|s| s.server_backoff(at, code));
            }
            Reply::Rejected(code) => {
                local_status::record(local_status::State::report_rejected);
                if spool.discard_rejected(id) {
                    throttled!(
                        throttles.report_rejected,
                        warn,
                        "{}伺服器拒收本機緩衝中的 report（{}），已丟棄、不重送（累計 {} 筆）",
                        codes::tag(codes::REPORT_REJECTED, Some(codes::W_INVALID_REPORT)),
                        sanitize_for_log(&code, LOG_CODE_LIMIT),
                        spool.stats().dropped_rejected
                    );
                }
            }
        }
        if self.backlog > 0 && self.in_flight == 0 && spool.is_empty() {
            log::info!(
                "{}已補送完本機緩衝中的積壓（這條連線共確認 {} 筆，約 {:.1} 秒），之後的取樣即時送出  replayed={} duration_ms={}",
                codes::tag(codes::BUFFER_REPLAY_DONE, None),
                self.acked,
                self.started.elapsed().as_secs_f64(),
                self.acked,
                self.started.elapsed().as_millis()
            );
            self.backlog = 0;
            local_status::record(local_status::State::buffer_changed);
        }
    }
}

/// 協商了 durable 的連線主迴圈：依序送出緩衝中的記錄（先補送積壓、再送新的取樣），
/// 同一時間最多 [`DURABLE_IN_FLIGHT`] 筆在途，兩筆之間至少間隔 `buffer.replay_interval_ms`；
/// 收到 ack 才從緩衝移除。heartbeat、讀取閒置與伺服器訊息的處理與 at-most-once 的迴圈相同
async fn serve_durable<S: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<S>,
    settings: &Settings,
    session: Session,
    shared: &Arc<SharedSpool>,
    throttles: &mut ServerLogThrottles,
) -> Result<Infallible> {
    let read_idle = settings.read_idle_timeout();
    let idle = time::sleep(read_idle);
    tokio::pin!(idle);
    let hb_period = settings.heartbeat_interval;
    let mut heartbeat = time::interval_at(after(hb_period), hb_period);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let (backlog, oldest) = {
        let spool = shared.lock();
        (spool.len(), spool.oldest_sampled_at())
    };
    // 有積壓時（例如 Kairos 重新啟動，大量主機同時恢復連線）先隨機延遲 0～connect_spread 再開始補送
    let spread = if backlog > 0 {
        crate::backoff::random_spread(settings.connect_spread)
    } else {
        Duration::ZERO
    };
    if backlog > 0 {
        let interval_ms = settings.replay_interval.as_millis();
        let minutes = (backlog as u128 * interval_ms).div_ceil(60_000);
        log::info!(
            "本機緩衝有 {backlog} 筆待補送的 report（最早的取樣時間 {}），{:.1} 秒後依序補送，之後再送新的取樣\
             （每 {interval_ms} 毫秒最多一筆，至少約需 {minutes} 分鐘）",
            oldest.map_or_else(
                || "未知".to_string(),
                |t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            ),
            spread.as_secs_f64()
        );
    }
    let mut delivery = Delivery::new(settings.replay_interval, Instant::now() + spread, backlog);
    let mut rotation_tick = time::interval(ROTATION_CHECK);
    rotation_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        let next_send = delivery.next_send_at(&shared.lock());
        tokio::select! {
            _ = heartbeat.tick() => {
                let seq = conn.next_seq();
                conn.send(&ClientMsg::Heartbeat { seq, ts: now_ts() }).await?;
                delivery.sent(seq, None);
                log::trace!("已送出 heartbeat（seq={seq}）");
            }
            _ = rotation_tick.tick(), if settings.rotation.is_some() => {
                send_renew_if_due(conn, settings).await?;
            }
            // 收集 task 寫入了新的取樣：重新計算下一筆的送出時間
            _ = shared.notify.notified() => {}
            _ = sleep_until_opt(next_send) => {
                let next = delivery.prepare_next(&mut shared.lock(), throttles);
                let Some((entry, record)) = next else { continue };
                let seq = conn.next_seq();
                let durable = DurableSeq {
                    boot_id: record.boot_id,
                    report_seq: record.report_seq,
                };
                let (msg, summary) = report_message(seq, record.sample, session, Some(durable));
                conn.send(&msg).await?;
                delivery.sent(seq, Some(entry.id));
                local_status::record(local_status::State::report_sent);
                log::debug!("已送出 report（seq={seq}，report_seq={}，{summary}）", entry.report_seq);
            }
            line = conn.framed.next() => {
                let line = line.ok_or(SessionError::ClosedByServer)?.map_err(SessionError::Codec)?;
                if rotation_line(settings, &line)? {
                    idle.as_mut().reset(after(read_idle));
                    continue;
                }
                let outcome = handle_server_line_in(&line, conn.seq, throttles, true)?;
                if outcome.valid {
                    idle.as_mut().reset(after(read_idle));
                }
                if let Some((seq, reply)) = durable_reply(&line) {
                    delivery.on_reply(seq, conn.seq, reply, &mut shared.lock(), throttles);
                }
            }
            _ = &mut idle => {
                return Err(SessionError::IdleTimeout(read_idle));
            }
        }
    }
}

/// 正常輪替的時點檢查間隔（連線建立後立即檢查一次）
const ROTATION_CHECK: Duration = Duration::from_secs(600);

/// 到了正常輪替的時點時送出 `renew_request`
async fn send_renew_if_due<S: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<S>,
    settings: &Settings,
) -> Result<()> {
    let Some(rot) = &settings.rotation else {
        return Ok(());
    };
    let seq = conn.seq.saturating_add(1);
    let req = rot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .next_request(seq, chrono::Utc::now().timestamp_millis());
    if let Some(req) = req {
        let sent = conn.next_seq();
        debug_assert_eq!(sent, req.seq);
        conn.send_line(serde_json::to_string(&req).map_err(SessionError::Serialize)?)
            .await?;
        log::debug!(
            "已送出 renew_request（seq={sent}，request_id={}）",
            req.request_id
        );
    }
    Ok(())
}

/// 一行伺服器訊息是否屬於正常輪替（是的話已處理，不再交給一般的處理）；取得新憑證時結束連線
fn rotation_line(settings: &Settings, line: &str) -> Result<bool> {
    let Some(rot) = &settings.rotation else {
        return Ok(false);
    };
    match rot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .on_line(line)
    {
        RotationLine::Installed => Err(SessionError::Renewed),
        RotationLine::Handled => Ok(true),
        RotationLine::NotMine => Ok(false),
    }
}

/// 現在時間加上 `d`；溢位時改用一年後（不 panic）。
fn after(d: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(d)
        .unwrap_or_else(|| now + Duration::from_secs(365 * 24 * 3600))
}

// ---------------------------------------------------------------------------
// 一次性的連線：`pulse enroll` 以新憑證確認、正常輪替後的確認、除役通知（ENROLL.md 第 13.2 節）
// ---------------------------------------------------------------------------

/// 一次性連線失敗的原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneShotError {
    /// 對應的 Pulse 代碼（無法歸類時 `None`）
    pub code: Option<&'static str>,
    pub message: String,
    /// 伺服器送來的 `error.code`
    pub server_code: Option<String>,
    /// 一般網路錯誤（連不上、逾時、收到回應前斷線）：可以重試
    pub retryable: bool,
}

impl OneShotError {
    fn from_session(e: &SessionError, has_client_cert: bool) -> Self {
        let code = error_code(e, 0, has_client_cert);
        let retryable = matches!(
            e,
            SessionError::ConnectTimeout
                | SessionError::Connect(_)
                | SessionError::TlsTimeout
                | SessionError::HelloAckTimeout
                | SessionError::ClosedByServer
                | SessionError::IdleTimeout(_)
                | SessionError::WriteTimeout
        ) || (matches!(
            e,
            SessionError::Tls(_) | SessionError::Codec(LinesCodecError::Io(_))
        ) && client_cert_alert(e, has_client_cert).is_none()
            && server_cert_problem(e).is_none())
            || matches!(e, SessionError::Server { code, .. } if code == "storage_unavailable");
        Self {
            code: code.or(match e {
                SessionError::Connect(_) | SessionError::ConnectTimeout => {
                    Some(codes::E_CONN_TIMEOUT)
                }
                _ if retryable => Some(codes::E_SERVER_SILENT),
                _ => None,
            }),
            message: e.to_string(),
            server_code: e.server_code().map(str::to_string),
            retryable,
        }
    }
}

/// 憑證已過期、8444 無法連線時對 8443 出示目前的（過期）憑證「報到」一次（設計包 §8：讓 Kairos 知道
/// 主機已回到網路）。結果不影響流程，只記 DEBUG
pub async fn check_in(settings: &Settings, tls: &TlsSetup) {
    match connect(settings, tls).await {
        Ok((mut stream, _)) => {
            let mut buf = [0u8; 1];
            let _ = time::timeout(
                Duration::from_secs(2),
                tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
            )
            .await;
            log::debug!(
                "已對 {}:{} 出示過期憑證報到",
                settings.address,
                settings.port
            );
        }
        Err(e) => log::debug!("對 8443 報到：{e}"),
    }
}

/// 已完成 hello／hello_ack 的一次性連線
pub struct OneShot {
    conn: Connection<TlsStream<TcpStream>>,
    /// 伺服器宣告的 features
    pub server_features: Vec<String>,
    pub proto: u32,
    has_client_cert: bool,
}

/// TCP＋TLS＋hello（可帶 `intent`）＋等待 hello_ack。`features` 為 hello 宣告的 features
pub async fn one_shot(
    settings: &Settings,
    tls: &TlsSetup,
    features: Vec<String>,
    intent: Option<&str>,
) -> std::result::Result<OneShot, OneShotError> {
    let has_client_cert = tls.has_client_cert();
    let (stream, _) = connect(settings, tls)
        .await
        .map_err(|e| OneShotError::from_session(&e, has_client_cert))?;
    let mut conn = Connection::new(stream);
    let seq = conn.next_seq();
    let hello = ClientMsg::Hello {
        seq,
        proto: Proto::Version(SUPPORTED_PROTO.min),
        agent_id: settings.agent_id.clone(),
        hostname: settings.hostname.clone(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        heartbeat_interval_ms: u64::try_from(settings.heartbeat_interval.as_millis())
            .unwrap_or(u64::MAX),
        ts: now_ts(),
        ext: Box::new(HelloExt::new(
            ProtoOffer {
                proto_min: Some(SUPPORTED_PROTO.min),
                proto_max: Some(SUPPORTED_PROTO.max),
                features: Some(features),
            },
            settings.host.clone(),
        )),
    };
    let result = async {
        let line = lifecycle::hello_with_intent(&hello, intent).map_err(SessionError::Serialize)?;
        conn.send_line(line).await?;
        wait_hello_ack(&mut conn, seq, &mut ServerLogThrottles::default()).await
    }
    .await;
    match result {
        Ok(ack) => Ok(OneShot {
            conn,
            server_features: ack.server_features,
            proto: ack.session.proto,
            has_client_cert,
        }),
        Err(e) => {
            conn.close_gracefully().await;
            Err(OneShotError::from_session(&e, has_client_cert))
        }
    }
}

impl OneShot {
    /// 伺服器是否宣告了 `feature`
    pub fn supports(&self, feature: &str) -> bool {
        self.proto >= kairos_proto::PROTO_V2 && self.server_features.iter().any(|f| f == feature)
    }

    /// 下一個 seq
    pub fn next_seq(&mut self) -> u64 {
        self.conn.next_seq()
    }

    /// 送出一則已序列化的訊息
    pub async fn send_line(&mut self, line: String) -> std::result::Result<(), OneShotError> {
        let has = self.has_client_cert;
        self.conn
            .send_line(line)
            .await
            .map_err(|e| OneShotError::from_session(&e, has))
    }

    /// 等待下一行（`timeout` 內）
    pub async fn next_line(
        &mut self,
        timeout: Duration,
    ) -> std::result::Result<String, OneShotError> {
        let has = self.has_client_cert;
        let line = time::timeout(timeout, self.conn.framed.next())
            .await
            .map_err(|_| OneShotError::from_session(&SessionError::IdleTimeout(timeout), has))?
            .ok_or(SessionError::ClosedByServer)
            .and_then(|r| r.map_err(SessionError::Codec))
            .map_err(|e| OneShotError::from_session(&e, has))?;
        Ok(line)
    }

    /// 送出 close_notify 並關閉
    pub async fn close(mut self) {
        self.conn.close_gracefully().await;
    }
}

/// 一條已完成握手的 TLS 連線的資訊（寫進日誌用）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct TlsInfo {
    /// 協商出的 TLS 版本（例如 `TLSv1_3`）
    version: String,
    /// 是否驗證了伺服器憑證（`accept_invalid_certs = true` 時為 false）
    verified: bool,
}

impl TlsInfo {
    /// 日誌中描述這條連線的文字（目標、domain、TLS 版本、伺服器憑證驗證狀態）。
    fn describe(&self, settings: &Settings) -> String {
        let note = if self.verified {
            "，已驗證伺服器憑證"
        } else {
            "；⚠️ 未驗證伺服器憑證（accept_invalid_certs = true），無法確認對方身分"
        };
        format!(
            "{}:{}，domain={}，{}{note}",
            settings.address, settings.port, settings.domain, self.version
        )
    }
}

/// TCP 連線 + TLS 握手，各自有逾時。
async fn connect(settings: &Settings, tls: &TlsSetup) -> Result<(TlsStream<TcpStream>, TlsInfo)> {
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
    let info = TlsInfo {
        version: state
            .protocol_version()
            .map_or_else(|| "未知版本".to_string(), |v| format!("{v:?}")),
        verified: tls.verifies_server_cert(),
    };
    let (level, msg) = established_log(settings, &info);
    log::log!(level, "{msg}");
    Ok((stream, info))
}

/// 「已建立 TLS 加密連線」的日誌等級與內容。
///
/// 握手完成不代表連線成功（TLS 1.3 下伺服器在握手後才驗證客戶端憑證，可能緊接著拒絕），
/// 因此一般情況只記 debug，成功與否以註冊成功的 INFO（[`registered_log`]）為準。
/// 未驗證伺服器憑證時仍記 warn：hello（含 agent_id）即將送給身分未確認的對象，
/// 即使之後註冊失敗也不能漏掉這個警告。
fn established_log(settings: &Settings, info: &TlsInfo) -> (log::Level, String) {
    let level = if info.verified {
        log::Level::Debug
    } else {
        log::Level::Warn
    };
    (
        level,
        format!("已建立 TLS 加密連線（{}）", info.describe(settings)),
    )
}

/// 註冊成功（收到 hello_ack）的日誌等級與內容，附上這條連線的 TLS 資訊。
///
/// 未驗證伺服器憑證時記 warn，讓只看 INFO 以上的人每次連上都看得到警告。
fn registered_log(
    settings: &Settings,
    ack: &HelloAckInfo,
    tls_info: Option<&TlsInfo>,
) -> (log::Level, String) {
    let base = format!(
        "已向伺服器註冊（agent_id={}，伺服器離線判定 {} ms，協定 v{}",
        settings.agent_id, ack.offline_after_ms, ack.session.proto
    );
    match tls_info {
        Some(info) => (
            if info.verified {
                log::Level::Info
            } else {
                log::Level::Warn
            },
            format!("{base}；TLS 加密連線 {}）", info.describe(settings)),
        ),
        None => (log::Level::Info, format!("{base}）")),
    }
}

/// hello_ack 帶來的資訊
#[derive(Debug, Clone, PartialEq, Eq)]
struct HelloAckInfo {
    offline_after_ms: u64,
    /// 協商結果（版本與 report 格式）
    session: Session,
    /// 伺服器時間（v2）
    server_time: Option<String>,
    /// 伺服器宣告的 features（v2；`renew`、`deregister` 的協商用）
    server_features: Vec<String>,
    /// ENROLL.md 第 13 節的欄位（`expired_renew_grace_days`、`renew_now`）
    extra: HelloAckExtra,
}

/// 等待 hello_ack（整體 5 s）。
async fn wait_hello_ack<S: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<S>,
    hello_seq: u64,
    throttles: &mut ServerLogThrottles,
) -> Result<HelloAckInfo> {
    let deadline = after(HELLO_ACK_TIMEOUT);
    loop {
        let line = time::timeout_at(deadline, conn.framed.next())
            .await
            .map_err(|_| SessionError::HelloAckTimeout)?
            .ok_or(SessionError::ClosedByServer)?
            .map_err(SessionError::Codec)?;
        if let Some(ack) = handle_pre_ack_line(&line, hello_seq, throttles)? {
            return Ok(ack);
        }
    }
}

/// 處理註冊前收到的一行伺服器訊息。
///
/// - `hello_ack` → `Ok(Some(…))`（seq 不符只記 warn；`proto` 不在本程式支援的範圍內 → 斷線）
/// - 任何 `error`（不論是否 fatal）→ 結束這條連線（由重連迴圈記錄 error）
/// - 其他 → 忽略（節流記錄），繼續等待
fn handle_pre_ack_line(
    line: &str,
    hello_seq: u64,
    throttles: &mut ServerLogThrottles,
) -> Result<Option<HelloAckInfo>> {
    match parse_server_msg(line) {
        Ok(ServerMsg::HelloAck {
            seq,
            proto,
            offline_after_ms,
            features,
            server_time,
        }) => {
            if !(SUPPORTED_PROTO.min..=SUPPORTED_PROTO.max).contains(&proto) {
                return Err(SessionError::ProtoMismatch(proto));
            }
            // v1 Kairos 不送 features；v2 時取雙方都宣告的功能
            let server_features = features.unwrap_or_default();
            let session = Session::new(proto, CLIENT_FEATURES, &server_features);
            if seq != hello_seq {
                // 每條連線最多一次（收到後即離開等待），不需節流
                log::warn!(
                    "hello_ack 的 seq={seq} 與 hello 的 seq={hello_seq} 不符，仍視為註冊成功"
                );
            }
            Ok(Some(HelloAckInfo {
                offline_after_ms,
                session,
                server_time: server_time.filter(|_| session.is_v2()),
                server_features: if session.is_v2() {
                    server_features
                } else {
                    Vec::new()
                },
                extra: if session.is_v2() {
                    HelloAckExtra::from_line(line)
                } else {
                    HelloAckExtra::default()
                },
            }))
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

/// 註冊後一行伺服器訊息的處理結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineOutcome {
    /// 是否為合法的協定訊息（用於重置讀取閒置計時）
    valid: bool,
    /// 這則訊息回應的 seq（ack 或帶 seq 的 error；只計 1..=已送出的最大 seq）
    answered: Option<u64>,
}

impl LineOutcome {
    const IGNORED: Self = Self {
        valid: false,
        answered: None,
    };

    fn valid(answered: Option<u64>) -> Self {
        Self {
            valid: true,
            answered,
        }
    }
}

/// 伺服器回應的 seq 是否對應到已送出的訊息。
fn answered_seq(seq: Option<u64>, last_sent_seq: u64) -> Option<u64> {
    seq.filter(|&s| s != 0 && s <= last_sent_seq)
}

/// 處理註冊後收到的一行伺服器訊息。`last_sent_seq` 用於比對 ack。
///
/// 回傳的 [`LineOutcome`] 說明這是否為合法的協定訊息（用於重置讀取閒置計時），
/// 以及它回應了哪個 seq（用於追蹤在途的 report）；fatal error 回傳 `Err` 結束連線。
fn handle_server_line(
    line: &str,
    last_sent_seq: u64,
    throttles: &mut ServerLogThrottles,
) -> Result<LineOutcome> {
    handle_server_line_in(line, last_sent_seq, throttles, false)
}

/// 同 [`handle_server_line`]；`durable` 為 `true` 時（at-least-once 的連線）
/// `storage_unavailable` 的日誌改為「稍後重送」
fn handle_server_line_in(
    line: &str,
    last_sent_seq: u64,
    throttles: &mut ServerLogThrottles,
    durable: bool,
) -> Result<LineOutcome> {
    match parse_server_msg(line) {
        Ok(ServerMsg::Ack { seq }) => {
            let answered = answered_seq(Some(seq), last_sent_seq);
            if answered.is_none() {
                throttled!(
                    throttles.unexpected_ack,
                    debug,
                    "收到 ack seq={seq}，但目前只送到 seq={last_sent_seq}"
                );
            } else {
                log::trace!("收到 ack seq={seq}");
            }
            Ok(LineOutcome::valid(answered))
        }
        Ok(ServerMsg::HelloAck { seq, .. }) => {
            throttled!(
                throttles.unexpected_hello_ack,
                warn,
                "{}已註冊後又收到 hello_ack（seq={seq}），忽略",
                codes::tag(codes::SERVER_UNEXPECTED, Some(codes::W_SERVER_UNEXPECTED))
            );
            Ok(LineOutcome::valid(None))
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
            // 節流視窗較長（STORAGE_UNAVAILABLE_LOG_WINDOW），不用 throttled!（固定 10 秒）
            if let Some(n) = throttles.storage_unavailable.check(Instant::now()) {
                log::warn!(
                    "{}伺服器無法保存 report（seq={}，storage_unavailable）：{}；{}{}",
                    codes::tag(codes::REPORT_REJECTED, Some(storage_code(&message))),
                    seq_text(seq),
                    sanitize_for_log(&message, LOG_TEXT_LIMIT),
                    if durable {
                        "資料保留在本機緩衝，稍後重送"
                    } else {
                        "此筆資料未送達，不會重送"
                    },
                    suppressed_note(n, STORAGE_UNAVAILABLE_LOG_WINDOW)
                );
            }
            Ok(LineOutcome::valid(answered_seq(seq, last_sent_seq)))
        }
        Ok(ServerMsg::Error {
            seq, code, message, ..
        }) => {
            let (event, pls) = if code == "invalid_report" {
                (codes::REPORT_REJECTED, codes::W_INVALID_REPORT)
            } else {
                (codes::SERVER_UNEXPECTED, codes::W_SERVER_UNEXPECTED)
            };
            throttled!(
                throttles.nonfatal_error,
                warn,
                "{}伺服器回報錯誤（seq={}）{}：{}",
                codes::tag(event, Some(pls)),
                seq_text(seq),
                sanitize_for_log(&code, LOG_CODE_LIMIT),
                sanitize_for_log(&message, LOG_TEXT_LIMIT)
            );
            Ok(LineOutcome::valid(answered_seq(seq, last_sent_seq)))
        }
        Err(e) => {
            log_unparsable(throttles, line, &e);
            Ok(LineOutcome::IGNORED)
        }
    }
}

/// 日誌用的 seq 文字（`null` 表示伺服器無法取得 seq）。
fn seq_text(seq: Option<u64>) -> String {
    seq.map_or_else(|| "null".to_string(), |s| s.to_string())
}

/// 記錄無法解析的伺服器訊息（節流）；解析錯誤本身也可能含有原始內容，一併清理。
///
/// 不認得的訊息類型（A14）：協定規定雙方忽略不認得的內容以便日後擴充，較新版的 Kairos
/// 可能送出本程式不認得的訊息，因此只記 DEBUG（仍節流），不當成伺服器異常記 WARN。
/// 處置與其他無法解析的行相同：忽略，且不算合法訊息（不重置讀取閒置計時）。
fn log_unparsable(throttles: &mut ServerLogThrottles, line: &str, e: &ServerMsgError) {
    if let ServerMsgError::UnknownType(kind) = e {
        throttled!(
            throttles.unknown_type,
            debug,
            "伺服器送來本程式不認得的訊息類型 {}（可能是較新版的 Kairos），忽略：{}",
            sanitize_for_log(kind, LOG_CODE_LIMIT),
            sanitize_for_log(line, LOG_TEXT_LIMIT)
        );
        return;
    }
    throttled!(
        throttles.unparsable,
        warn,
        "{}無法解析伺服器訊息（{}），忽略：{}",
        codes::tag(codes::SERVER_UNEXPECTED, Some(codes::W_SERVER_UNEXPECTED)),
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
        "superseded" if superseded_streak >= SUPERSEDED_STREAK_ERROR => {
            // 啟用 mTLS 時客戶端憑證的 SAN 綁定 agent_id：只刪 agent.id 會因 SAN 不符而無法啟動
            let mtls_note = if settings.client_cert.is_some() {
                "；本機已啟用 mTLS：產生新的 agent_id 後，必須請管理者以新的 agent_id（SAN 為 DNS:<新 agent_id>，\
                 可用 `pulse --print-agent-id` 取得）重新簽發客戶端憑證並更新 server.client_cert / server.client_key，\
                 否則 Pulse 會因憑證 SAN 不符而無法啟動"
            } else {
                ""
            };
            Some(format!(
                "已連續 {superseded_streak} 次被同一 agent_id（{}）的其他連線取代：極可能有多台主機共用同一份 agent.id\
                 （例如複製了 VM 映像檔），請在多出來的主機上刪除 agent.id 後重新啟動，以產生新的 agent_id{mtls_note}",
                settings.agent_id
            ))
        }
        "superseded" => Some(
            "此連線已被同一 agent_id 的新連線取代；若非預期，請確認是否有多台主機共用同一份 agent.id"
                .to_string(),
        ),
        "unsupported_version" => Some(format!(
            "伺服器不支援本程式的協定版本（{}～{}），請更新 Pulse 或 Kairos",
            SUPPORTED_PROTO.min, SUPPORTED_PROTO.max
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
        // Kairos 的 CRL 過期或無法判定撤銷狀態：伺服器端問題，重新簽發也無濟於事
        "certificate_rejected" if message.trim() == REVOCATION_UNAVAILABLE => Some(
            "伺服器的 CRL 已過期或無法判定此憑證的撤銷狀態，屬伺服器端問題（不是本機憑證的問題，\
             不需要重新簽發），請聯絡伺服器管理者更新 CRL"
                .to_string(),
        ),
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
    // 常見原因是超過伺服器的每 IP 連線上限或新連線速率限制（Kairos 會在握手前直接關閉）
    if let SessionError::Tls(e) = err
        && !client_cert_requested
        && closed_before_handshake(e)
    {
        return Some(
            "伺服器在 TLS 握手完成前就關閉了連線：可能超過伺服器的每 IP 連線上限\
             （同一 IP 位址的連線過多）、新連線速率過快（同一 IP，IPv6 為同一 /64）或伺服器握手中的連線過多，\
             或伺服器正在重新啟動、連到的連接埠不是 TLS 服務"
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
/// - `\n`、`\r`、`\t` 轉成對應的跳脫字串，反斜線本身轉成 `\\`（字面的 `\n` 與轉義後的換行
///   才能區分）；其他控制字元（C0 / DEL / C1，含 ESC）與會改變顯示方向、不可見或無法辨識的字元
///   （Unicode 格式字元 Cf，例如 U+200B、U+202E、U+FEFF，U+2028、U+2029，以及 Hangul filler、
///   變體選擇字元、私用區等，見 [`crate::protocol::is_forbidden_display_char`]）轉成 `\u{XXXX}`，
///   避免偽造日誌行或夾帶終端跳脫序列
/// - 超過 `limit` 個字元時截斷，並附上原始長度
pub(crate) fn sanitize_for_log(s: &str, limit: usize) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len().min(limit));
    for (i, c) in s.chars().enumerate() {
        if i == limit {
            let _ = write!(out, "…（共 {} bytes）", s.len());
            break;
        }
        match c {
            '\\' => out.push_str("\\\\"),
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
/// 與 hello 顯示用字串的規則相同（Cc、全部 Cf、U+2028、U+2029、Hangul filler、
/// 變體選擇字元、私用區等），見 [`crate::protocol::is_forbidden_display_char`]。
fn is_unsafe_for_log(c: char) -> bool {
    crate::protocol::is_forbidden_display_char(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tokio::io::{DuplexStream, duplex};
    use tokio_rustls::rustls::{AlertDescription, Error as TlsError};

    fn settings(hb: u64) -> Settings {
        Settings {
            address: "127.0.0.1".into(),
            port: 1,
            domain: "localhost".into(),
            agent_id: "a".into(),
            hostname: "h".into(),
            heartbeat_interval: Duration::from_secs(hb),
            max_retries: 5,
            initial_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
            client_cert: None,
            // 既有測試驗證精確的退避時間，不加隨機分散；分散另有測試
            connect_spread: Duration::ZERO,
            host: HostInfo::default(),
            replay_interval: Duration::from_millis(600),
            status_note: None,
            extra_features: Vec::new(),
            rotation: None,
            grace_on_expiry: false,
        }
    }

    #[test]
    fn registered_status_appends_the_buffer_note() {
        let mut s = settings(1);
        assert_eq!(registered_status(&s), "已註冊：127.0.0.1:1");
        s.status_note = Some("無法開啟本機緩衝（PLS-E4004，斷線期間的資料不會補送）".into());
        assert_eq!(
            registered_status(&s),
            "已註冊：127.0.0.1:1；無法開啟本機緩衝（PLS-E4004，斷線期間的資料不會補送）"
        );
    }

    #[test]
    fn ack_and_nonfatal_error_keep_connection() {
        let mut t = ServerLogThrottles::default();
        let outcome =
            |line: &str, t: &mut ServerLogThrottles| handle_server_line(line, 2, t).unwrap();
        assert_eq!(
            outcome(r#"{"type":"ack","seq":2}"#, &mut t),
            LineOutcome::valid(Some(2))
        );
        // seq 超前或為 0 的 ack 仍是合法訊息，但不算回應了任何已送出的訊息
        assert_eq!(
            outcome(r#"{"type":"ack","seq":9}"#, &mut t),
            LineOutcome::valid(None)
        );
        assert_eq!(
            outcome(r#"{"type":"ack","seq":0}"#, &mut t),
            LineOutcome::valid(None)
        );
        assert_eq!(
            outcome(
                r#"{"type":"error","seq":null,"code":"unknown_type","message":"unknown message type","fatal":false}"#,
                &mut t
            ),
            LineOutcome::valid(None)
        );
        assert_eq!(
            outcome(
                r#"{"type":"error","seq":1,"code":"invalid_report","message":"x","fatal":false}"#,
                &mut t
            ),
            LineOutcome::valid(Some(1))
        );
        // 無法解析的行只記錄，不斷線，但不算合法訊息（不重置閒置計時）
        assert_eq!(outcome("not json", &mut t), LineOutcome::IGNORED);
        assert_eq!(
            outcome(r#"{"type":"future_msg"}"#, &mut t),
            LineOutcome::IGNORED
        );
    }

    #[test]
    fn storage_unavailable_keeps_connection_and_is_throttled_separately() {
        let mut t = ServerLogThrottles::default();
        let line = r#"{"type":"error","seq":6,"code":"storage_unavailable","message":"disk full","fatal":false}"#;
        for _ in 0..100 {
            assert_eq!(
                handle_server_line(line, 6, &mut t).unwrap(),
                LineOutcome::valid(Some(6))
            );
        }
        // 走自己的節流類別：已記錄過一次，其餘被略過；一般非致命錯誤的類別未被使用
        assert_eq!(t.storage_unavailable.check(Instant::now()), None);
        assert_eq!(t.nonfatal_error.check(Instant::now()), Some(0));
    }

    #[test]
    fn storage_unavailable_warn_uses_longer_window() {
        // 每日大小上限滿時會持續一整天：每 300 秒最多一則 WARN，而不是 10 秒
        let mut t = ServerLogThrottles::default();
        let t0 = Instant::now();
        assert_eq!(t.storage_unavailable.check(t0), Some(0));
        assert_eq!(
            t.storage_unavailable.check(t0 + Duration::from_secs(60)),
            None
        );
        assert_eq!(
            t.storage_unavailable.check(t0 + Duration::from_secs(299)),
            None
        );
        assert_eq!(
            t.storage_unavailable
                .check(t0 + STORAGE_UNAVAILABLE_LOG_WINDOW),
            Some(2)
        );
        // 其他類別仍是 10 秒
        assert_eq!(t.nonfatal_error.check(t0), Some(0));
        assert_eq!(t.nonfatal_error.check(t0 + DEFAULT_WINDOW), Some(0));
        let line = r#"{"type":"error","seq":1,"code":"storage_unavailable","message":"daily size limit reached","fatal":false}"#;
        let ((), logs) = crate::test_log::capture(|| {
            let mut t = ServerLogThrottles::default();
            for _ in 0..50 {
                handle_server_line(line, 1, &mut t).unwrap();
            }
        });
        let warns: Vec<_> = logs
            .iter()
            .filter(|(_, m)| m.contains("storage_unavailable"))
            .collect();
        assert_eq!(warns.len(), 1, "{warns:?}");
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
        assert_eq!(t.unparsable.check(Instant::now()), None);
        assert_eq!(t.unexpected_hello_ack.check(Instant::now()), None);
    }

    #[test]
    fn unknown_message_type_is_debug_and_throttled() {
        // A14：較新版 Kairos 的新訊息類型只記 DEBUG（節流），壞掉的訊息仍記 WARN
        let ((), logs) = crate::test_log::capture(|| {
            let mut t = ServerLogThrottles::default();
            for _ in 0..100 {
                assert_eq!(
                    handle_server_line(r#"{"type":"future_msg","seq":3}"#, 3, &mut t).unwrap(),
                    LineOutcome::IGNORED
                );
                assert_eq!(
                    handle_pre_ack_line(r#"{"type":"future_msg"}"#, 1, &mut t).unwrap(),
                    None
                );
            }
            // 已知類型但欄位錯誤：仍是無法解析的訊息
            assert_eq!(
                handle_server_line(r#"{"type":"ack"}"#, 3, &mut t).unwrap(),
                LineOutcome::IGNORED
            );
            // 未知類型走自己的節流類別，沒有用掉「無法解析」的額度
            assert_eq!(t.unknown_type.check(Instant::now()), None);
            assert_eq!(t.unparsable.check(Instant::now()), None);
        });
        let unknown: Vec<_> = logs
            .iter()
            .filter(|(_, m)| m.contains("不認得的訊息類型"))
            .collect();
        assert_eq!(unknown.len(), 1, "{logs:?}");
        assert_eq!(unknown[0].0, log::Level::Debug);
        assert!(unknown[0].1.contains("future_msg"), "{unknown:?}");
        let warns: Vec<_> = logs
            .iter()
            .filter(|(l, _)| *l == log::Level::Warn)
            .collect();
        assert_eq!(warns.len(), 1, "{logs:?}");
        assert!(warns[0].1.contains("無法解析伺服器訊息"), "{warns:?}");
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
        assert_eq!(ok.map(|a| a.offline_after_ms), Some(5000));
        // seq 不符只記 warn，不斷線
        let ok = handle_pre_ack_line(
            r#"{"type":"hello_ack","seq":7,"proto":1,"offline_after_ms":6000}"#,
            1,
            &mut t,
        )
        .unwrap();
        assert_eq!(ok.map(|a| a.offline_after_ms), Some(6000));
        // proto 不在支援範圍（1～2）內則斷線
        for proto in [0, 3, 99] {
            let line = format!(
                r#"{{"type":"hello_ack","seq":1,"proto":{proto},"offline_after_ms":5000}}"#
            );
            assert!(
                matches!(
                    handle_pre_ack_line(&line, 1, &mut t),
                    Err(SessionError::ProtoMismatch(p)) if p == proto
                ),
                "{proto}"
            );
        }
    }

    #[test]
    fn hello_ack_negotiates_the_session() {
        // A3：v1 Kairos（沒有 features）→ v1；v2 且雙方都有 metrics → 指標格式
        let mut t = ServerLogThrottles::default();
        let ack = |line: &str, t: &mut ServerLogThrottles| {
            handle_pre_ack_line(line, 1, t).unwrap().unwrap()
        };
        let v1 = ack(
            r#"{"type":"hello_ack","seq":1,"proto":1,"offline_after_ms":5000}"#,
            &mut t,
        );
        assert_eq!(v1.session, Session::V1);
        assert_eq!(v1.server_time, None);
        let v2 = ack(
            r#"{"type":"hello_ack","seq":1,"proto":2,"offline_after_ms":5000,"features":["metrics","x"],"server_time":"2026-10-08T03:15:40.012Z"}"#,
            &mut t,
        );
        assert_eq!(
            v2.session,
            Session {
                proto: 2,
                metrics: true,
                durable: false,
            }
        );
        assert_eq!(v2.server_time.as_deref(), Some("2026-10-08T03:15:40.012Z"));
        let v2_plain = ack(
            r#"{"type":"hello_ack","seq":1,"proto":2,"offline_after_ms":5000,"features":[]}"#,
            &mut t,
        );
        assert_eq!(
            v2_plain.session,
            Session {
                proto: 2,
                metrics: false,
                durable: false,
            }
        );
        // v1 的 hello_ack 夾帶 v2 欄位：不使用
        let odd = ack(
            r#"{"type":"hello_ack","seq":1,"proto":1,"offline_after_ms":5000,"features":["metrics"],"server_time":"x"}"#,
            &mut t,
        );
        assert_eq!((odd.session, odd.server_time), (Session::V1, None));
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
        // R2-P2：max(2 × hb, hb + 10 秒)，只看 heartbeat.interval（report.interval 不影響）
        assert_eq!(settings(1).read_idle_timeout(), Duration::from_secs(11));
        assert_eq!(settings(5).read_idle_timeout(), Duration::from_secs(15));
        assert_eq!(settings(10).read_idle_timeout(), Duration::from_secs(20));
        assert_eq!(settings(30).read_idle_timeout(), Duration::from_secs(60));
        assert_eq!(settings(60).read_idle_timeout(), Duration::from_secs(120));
        // 極大值也不 panic
        let mut s = settings(1);
        s.heartbeat_interval = Duration::MAX;
        assert_eq!(s.read_idle_timeout(), Duration::MAX);
    }

    #[test]
    fn read_idle_timeout_ignores_report_interval() {
        // R2-P2：hb 大、report 小時，舊公式取 min 會得到 15 秒；取樣卡住時就會誤斷線
        let mut config = crate::config_manager::AppConfig::default_for_tests();
        config.heartbeat.interval = 60;
        config.report.interval = 5;
        let s = Settings::new(&config, "a".into(), "h".into());
        assert_eq!(s.read_idle_timeout(), Duration::from_secs(120));
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
        let mut s = settings(1);
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
        let mut s = settings(1);
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
        let s = settings(1);
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
        let mut s = settings(1);
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
        let s = settings(1);
        let closed = |kind: io::ErrorKind| SessionError::Tls(io::Error::from(kind));
        for kind in [
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
        ] {
            let hint = connection_hint(&closed(kind), &s, false, false).unwrap();
            assert!(hint.contains("每 IP 連線上限"), "{kind:?}: {hint}");
            assert!(
                hint.contains("同一 /64")
                    && hint.contains("新連線速率過快")
                    && hint.contains("握手中的連線過多"),
                "{hint}"
            );
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
        let mut s = settings(1);
        s.port = port;
        let tls = tls::build(&crate::config_manager::ServerConfig {
            address: "127.0.0.1".into(),
            port,
            domain: "localhost".into(),
            accept_invalid_certs: true,
            ca_cert: String::new(),
            use_system_roots: Some(false),
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
        let s = settings(1);
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
        assert!(needs_attention(&full, 0, true));
        assert_eq!(
            server_error_hint(&s, &SessionError::ClosedByServer, 0),
            None
        );
        // 記錄等級：伺服器錯誤一律 error；superseded 只有連續發生才是 error
        assert!(needs_attention(&err("auth_failed"), 0, true));
        assert!(needs_attention(&SessionError::ProtoMismatch(3), 0, true));
        assert!(!needs_attention(&err("superseded"), 1, true));
        assert!(needs_attention(&err("superseded"), 2, true));
        assert!(!needs_attention(&SessionError::ClosedByServer, 0, true));
    }

    fn tls_info(verified: bool) -> TlsInfo {
        TlsInfo {
            version: "TLSv1_3".into(),
            verified,
        }
    }

    #[test]
    fn established_log_is_debug_unless_server_unverified() {
        // F4：握手完成不代表連線成功（客戶端憑證可能緊接著被拒），一般情況只記 debug
        let s = settings(1);
        let (level, msg) = established_log(&s, &tls_info(true));
        assert_eq!(level, log::Level::Debug);
        assert!(
            msg.contains("已建立 TLS 加密連線") && msg.contains("TLSv1_3"),
            "{msg}"
        );
        assert!(!msg.contains("未驗證"), "{msg}");
        // P2：未驗證伺服器憑證時仍在連線當下記 warn，不因之後註冊失敗而遺漏
        let (level, msg) = established_log(&s, &tls_info(false));
        assert_eq!(level, log::Level::Warn);
        assert!(
            msg.contains("未驗證伺服器憑證") && msg.contains("accept_invalid_certs"),
            "{msg}"
        );
    }

    #[test]
    fn registered_log_carries_tls_details() {
        // F4：原本「已建立 TLS 加密連線」INFO 的資訊改附在註冊成功的 INFO
        let s = settings(1);
        let ack = |offline_after_ms, proto| HelloAckInfo {
            offline_after_ms,
            session: Session::new(proto, CLIENT_FEATURES, CLIENT_FEATURES),
            server_time: None,
            server_features: Vec::new(),
            extra: HelloAckExtra::default(),
        };
        let (level, msg) = registered_log(&s, &ack(5000, 2), Some(&tls_info(true)));
        assert_eq!(level, log::Level::Info);
        assert_eq!(
            msg,
            "已向伺服器註冊（agent_id=a，伺服器離線判定 5000 ms，協定 v2；\
             TLS 加密連線 127.0.0.1:1，domain=localhost，TLSv1_3，已驗證伺服器憑證）"
        );
        // 未驗證伺服器憑證：註冊成功的那一行升為 warn 並帶警告
        let (level, msg) = registered_log(&s, &ack(5000, 2), Some(&tls_info(false)));
        assert_eq!(level, log::Level::Warn);
        assert!(
            msg.starts_with("已向伺服器註冊")
                && msg.contains("TLSv1_3")
                && msg.contains("未驗證伺服器憑證")
                && msg.contains("accept_invalid_certs"),
            "{msg}"
        );
        // 沒有 TLS 資訊（測試用的記憶體內串流）
        let (level, msg) = registered_log(&s, &ack(6000, 1), None);
        assert_eq!(level, log::Level::Info);
        assert_eq!(
            msg,
            "已向伺服器註冊（agent_id=a，伺服器離線判定 6000 ms，協定 v1）"
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
            assert!(handle_server_line(&line, 1, &mut t).unwrap().valid);
            // storage_unavailable 的 message
            let line = format!(
                r#"{{"type":"error","seq":1,"code":"storage_unavailable","message":{evil_json},"fatal":false}}"#
            );
            assert!(handle_server_line(&line, 1, &mut t).unwrap().valid);
            // 無法解析的原始行（含 serde 錯誤訊息中的原始內容），且超長
            let line = format!(r#"{{"type":{evil_json}}}{}"#, "y".repeat(10_000));
            assert!(!handle_server_line(&line, 1, &mut t).unwrap().valid);
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
        serve_with_reports(stream, s, &mut rx).await
    }

    /// 同 [`serve_once`]，取樣由呼叫端從 `rx` 的另一端送入。
    async fn serve_with_reports(
        stream: DuplexStream,
        s: &Settings,
        rx: &mut mpsc::Receiver<Sample>,
    ) -> (SessionError, bool) {
        let mut conn = Connection::new(stream);
        let mut registered_at = None;
        let mut throttles = ServerLogThrottles::default();
        let err = match serve(
            &mut conn,
            s,
            None,
            rx,
            None,
            &mut registered_at,
            &mut throttles,
        )
        .await
        {
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
        let (err, registered) = serve_once(client, &settings(1)).await;
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
        let (err, _) = serve_once(client, &settings(1)).await;
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
        let (err, _) = serve_once(client, &settings(1)).await;
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
        let (err, _) = serve_once(client, &settings(5)).await;
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
        let (err, registered) = serve_once(client, &settings(1)).await;
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
        let (err, _) = serve_once(client, &settings(1)).await;
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
        let (err, registered) = serve_once(client, &settings(1)).await;
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

    // ---- durable：at-least-once（A5） ----

    /// 測試用的本機緩衝目錄（結束時刪除）
    struct SpoolDir(std::path::PathBuf);

    impl Drop for SpoolDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_spool(tag: &str, samples: usize) -> (SpoolDir, Arc<SharedSpool>) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pulse-net-spool-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let spool = Spool::open(crate::spool::SpoolSettings {
            dir: dir.clone(),
            max_bytes: 1024 * 1024,
            max_records: 1000,
            max_age: Duration::from_secs(7 * 24 * 3600),
        })
        .unwrap();
        let shared = SharedSpool::new(spool);
        for i in 0..samples {
            shared.lock().append(&durable_sample(i as f32)).unwrap();
        }
        (SpoolDir(dir), shared)
    }

    fn durable_sample(cpu: f32) -> Sample {
        Sample {
            ts: now_ts(),
            cpu_usage: cpu,
            total_memory: 2,
            used_memory: 1,
            total_memory_bytes: 2048,
            used_memory_bytes: 1024,
            extra: Vec::new(),
        }
    }

    /// 讀取 hello，以協定 v2 與 `features` 回覆 hello_ack；回傳 hello
    async fn accept_hello_v2(f: &mut ServerSide, features: &[&str]) -> serde_json::Value {
        let line = f.next().await.unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "hello", "{line}");
        f.send(
            serde_json::json!({"type":"hello_ack","seq":v["seq"],"proto":2,
                "offline_after_ms":5000,"features":features})
            .to_string(),
        )
        .await
        .unwrap();
        v
    }

    /// 讀取下一則 report（heartbeat 立即回 ack）；連線關閉時為 `None`
    async fn next_report(f: &mut ServerSide) -> Option<serde_json::Value> {
        loop {
            let line = f.next().await?.ok()?;
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["type"] == "report" {
                return Some(v);
            }
            f.send(format!(r#"{{"type":"ack","seq":{}}}"#, v["seq"]))
                .await
                .ok()?;
        }
    }

    async fn reply(f: &mut ServerSide, json: serde_json::Value) {
        f.send(json.to_string()).await.unwrap();
    }

    fn ack_of(report: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type":"ack","seq":report["seq"]})
    }

    /// 在 `stream` 上以本機緩衝執行 serve()
    async fn serve_spool(
        stream: DuplexStream,
        s: &Settings,
        shared: &Arc<SharedSpool>,
    ) -> SessionError {
        let (_tx, mut rx) = mpsc::channel::<Sample>(1);
        let mut conn = Connection::new(stream);
        let mut registered_at = None;
        let mut throttles = ServerLogThrottles::default();
        match serve(
            &mut conn,
            s,
            None,
            &mut rx,
            Some(shared),
            &mut registered_at,
            &mut throttles,
        )
        .await
        {
            Ok(never) => match never {},
            Err(e) => e,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn durable_session_replays_backlog_in_order_at_the_replay_rate() {
        let (_dir, shared) = test_spool("order", 6);
        let boot = shared.lock().boot_id().to_string();
        let (client, server) = duplex(256 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            let hello = accept_hello_v2(&mut f, &["metrics", "durable"]).await;
            let mut got = Vec::new();
            while got.len() < 6 {
                let r = next_report(&mut f).await.unwrap();
                reply(&mut f, ack_of(&r)).await;
                got.push((r, Instant::now()));
            }
            (hello, got)
        });
        let err = serve_spool(client, &settings(60), &shared).await;
        assert!(matches!(err, SessionError::ClosedByServer), "{err:?}");
        let (hello, got) = srv.await.unwrap();
        // 啟用本機緩衝時宣告 durable
        assert_eq!(hello["features"], serde_json::json!(["metrics", "durable"]));
        let seqs: Vec<u64> = got
            .iter()
            .map(|(r, _)| r["report_seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6]);
        assert!(
            got.iter()
                .all(|(r, _)| r["boot_id"] == boot.as_str() && r["metrics"].is_array())
        );
        // 補送速率：兩筆之間至少 buffer.replay_interval_ms（600 ms）
        for pair in got.windows(2) {
            let gap = pair[1].1 - pair[0].1;
            assert!(gap >= Duration::from_millis(600), "{gap:?}");
        }
        assert!(shared.lock().is_empty(), "收到 ack 的都已從緩衝移除");
    }

    #[tokio::test(start_paused = true)]
    async fn durable_window_limits_in_flight_and_storage_unavailable_is_resent() {
        let (_dir, shared) = test_spool("window", 6);
        let (client, server) = duplex(256 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello_v2(&mut f, &["metrics", "durable"]).await;
            // 不回應：最多 4 筆在途，之後不再送
            let mut held = Vec::new();
            for _ in 0..DURABLE_IN_FLIGHT {
                held.push(next_report(&mut f).await.unwrap());
            }
            let more = time::timeout(Duration::from_secs(20), next_report(&mut f)).await;
            assert!(more.is_err(), "視窗已滿仍送出：{more:?}");
            // 依序回應：第 1 筆沒有保存，其餘已保存
            reply(
                &mut f,
                serde_json::json!({"type":"error","seq":held[0]["seq"],"code":"storage_unavailable",
                    "message":"report could not be stored","fatal":false}),
            )
            .await;
            for r in &held[1..] {
                reply(&mut f, ack_of(r)).await;
            }
            let start = Instant::now();
            let mut rest = Vec::new();
            while rest.len() < 3 {
                let r = next_report(&mut f).await.unwrap();
                reply(&mut f, ack_of(&r)).await;
                rest.push(r);
            }
            let held_seq: Vec<u64> = held
                .iter()
                .map(|r| r["report_seq"].as_u64().unwrap())
                .collect();
            (held_seq, rest, start)
        });
        let s = settings(60);
        let session = serve_spool(client, &s, &shared);
        let (err, server) = tokio::join!(session, srv);
        assert!(matches!(err, SessionError::ClosedByServer), "{err:?}");
        let (held, rest, _) = server.unwrap();
        assert_eq!(held, vec![1, 2, 3, 4]);
        // storage_unavailable 的那筆退避後重送（內容不變），之後接著送 5、6；已 ack 的不重送
        let rest_seq: Vec<u64> = rest
            .iter()
            .map(|r| r["report_seq"].as_u64().unwrap())
            .collect();
        assert_eq!(rest_seq, vec![1, 5, 6]);
        assert!(shared.lock().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn unacked_reports_are_resent_unchanged_on_the_next_connection() {
        let (_dir, shared) = test_spool("resend", 2);
        // 第一條連線：收到兩筆，只 ack 第一筆就斷線（第二筆的結果對 Pulse 來說不明）
        let (client, server) = duplex(256 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello_v2(&mut f, &["metrics", "durable"]).await;
            let first = next_report(&mut f).await.unwrap();
            reply(&mut f, ack_of(&first)).await;
            next_report(&mut f).await.unwrap()
        });
        let err = serve_spool(client, &settings(60), &shared).await;
        assert!(matches!(err, SessionError::ClosedByServer), "{err:?}");
        let lost = srv.await.unwrap();
        assert_eq!(shared.lock().len(), 1);

        // 第二條連線：同一筆以新的 seq 重送，boot_id、report_seq、ts 與內容都不變
        let (client, server) = duplex(256 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello_v2(&mut f, &["metrics", "durable"]).await;
            let again = next_report(&mut f).await.unwrap();
            reply(&mut f, ack_of(&again)).await;
            again
        });
        let err = serve_spool(client, &settings(60), &shared).await;
        assert!(matches!(err, SessionError::ClosedByServer), "{err:?}");
        let mut again = srv.await.unwrap();
        let mut lost = lost;
        assert_eq!(again["report_seq"], 2);
        lost.as_object_mut().unwrap().remove("seq");
        again.as_object_mut().unwrap().remove("seq");
        assert_eq!(lost, again);
        assert!(shared.lock().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn rejected_reports_are_dropped_not_resent() {
        let (_dir, shared) = test_spool("reject", 3);
        let (client, server) = duplex(256 * 1024);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello_v2(&mut f, &["metrics", "durable"]).await;
            let r1 = next_report(&mut f).await.unwrap();
            reply(
                &mut f,
                serde_json::json!({"type":"error","seq":r1["seq"],"code":"invalid_report",
                    "message":"x","fatal":false}),
            )
            .await;
            let _r2 = next_report(&mut f).await.unwrap();
            // 欄位錯誤的 bad_request 沒有 seq：對應最早的未回應訊息
            reply(
                &mut f,
                serde_json::json!({"type":"error","seq":null,"code":"bad_request",
                    "message":"invalid message","fatal":false}),
            )
            .await;
            let r3 = next_report(&mut f).await.unwrap();
            reply(&mut f, ack_of(&r3)).await;
            let more = time::timeout(Duration::from_secs(20), next_report(&mut f)).await;
            assert!(more.is_err(), "拒收的不應重送：{more:?}");
        });
        let err = serve_spool(client, &settings(60), &shared).await;
        assert!(matches!(err, SessionError::ClosedByServer), "{err:?}");
        srv.await.unwrap();
        let stats = shared.lock().stats();
        assert_eq!((stats.pending, stats.dropped_rejected), (0, 2));
    }

    #[tokio::test(start_paused = true)]
    async fn old_kairos_without_durable_keeps_the_backlog_and_sends_live_samples() {
        let (_dir, shared) = test_spool("old", 3);
        let (client, server) = duplex(256 * 1024);
        let probe = shared.clone();
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello_v2(&mut f, &["metrics"]).await;
            let more = time::timeout(Duration::from_secs(10), next_report(&mut f)).await;
            assert!(
                more.is_err(),
                "積壓不應送給不支援 durable 的伺服器：{more:?}"
            );
            // 連線期間收集 task 改為即時送出（不寫入緩衝）
            assert!(probe.is_live());
        });
        let err = serve_spool(client, &settings(60), &shared).await;
        assert!(matches!(err, SessionError::ClosedByServer), "{err:?}");
        srv.await.unwrap();
        assert!(!shared.is_live(), "斷線後恢復寫入緩衝");
        assert_eq!(shared.lock().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn without_spool_durable_is_not_declared_or_used() {
        let (client, server) = duplex(256 * 1024);
        let (tx, mut rx) = mpsc::channel::<Sample>(1);
        let srv = tokio::spawn(async move {
            let mut f = server_side(server);
            // 伺服器宣告 durable，但 Pulse 沒有宣告：不啟用
            let hello = accept_hello_v2(&mut f, &["metrics", "durable"]).await;
            // 註冊後才送入取樣（註冊時會清掉佇列中殘留的舊取樣）
            time::sleep(Duration::from_secs(1)).await;
            tx.send(durable_sample(1.0)).await.unwrap();
            let r = next_report(&mut f).await.unwrap();
            reply(&mut f, ack_of(&r)).await;
            (hello, r)
        });
        let (err, _) = serve_with_reports(client, &settings(60), &mut rx).await;
        assert!(matches!(err, SessionError::ClosedByServer), "{err:?}");
        let (hello, report) = srv.await.unwrap();
        assert_eq!(hello["features"], serde_json::json!(["metrics"]));
        assert!(
            report.get("boot_id").is_none() && report.get("report_seq").is_none(),
            "{report}"
        );
    }

    #[test]
    fn durable_replies_are_classified() {
        assert_eq!(
            durable_reply(r#"{"type":"ack","seq":3}"#),
            Some((Some(3), Reply::Ack))
        );
        for (code, message, expected) in [
            (
                "storage_unavailable",
                "report could not be stored",
                "PLS-W4101",
            ),
            (
                "storage_unavailable",
                "report rate limit exceeded",
                "PLS-W2203",
            ),
            ("something_new", "m", "PLS-W2107"),
        ] {
            let line = format!(
                r#"{{"type":"error","seq":4,"code":"{code}","message":"{message}","fatal":false}}"#
            );
            assert_eq!(
                durable_reply(&line),
                Some((Some(4), Reply::Retry(expected))),
                "{code}"
            );
        }
        for code in ["invalid_report", "bad_request"] {
            let line = format!(
                r#"{{"type":"error","seq":null,"code":"{code}","message":"m","fatal":false}}"#
            );
            assert_eq!(
                durable_reply(&line),
                Some((None, Reply::Rejected(code.into())))
            );
        }
        // fatal 由 handle_server_line 結束連線；其他訊息不是回應
        assert_eq!(
            durable_reply(
                r#"{"type":"error","seq":4,"code":"superseded","message":"m","fatal":true}"#
            ),
            None
        );
        assert_eq!(durable_reply("garbage"), None);
    }

    // ---- run_with()：退避重置與 superseded（X1、P6） ----

    /// 測試用連線方式：每次 dial 建立一對記憶體內串流，伺服器端交給測試劇本。
    struct FakeDialer {
        servers: mpsc::UnboundedSender<DuplexStream>,
        dial_times: RefCell<Vec<Instant>>,
    }

    impl Dialer for FakeDialer {
        type Stream = DuplexStream;

        async fn dial(&self) -> Result<(DuplexStream, TlsInfo)> {
            self.dial_times.borrow_mut().push(Instant::now());
            let (client, server) = duplex(64 * 1024);
            self.servers
                .send(server)
                .map_err(|_| SessionError::Connect(io::Error::other("測試伺服器已結束")))?;
            Ok((client, tls_info(true)))
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
                    .run_until(run_with(&s, &dialer, &mut reports, None, &shutdown))
                    .await;
                let times = dialer.dial_times.borrow();
                times.iter().map(|t| *t - start).collect()
            })
        })
    }

    #[test]
    fn registration_info_line_includes_tls_details() {
        // F4：連線流程中註冊成功的 INFO 帶有 dial 回傳的 TLS 資訊
        let (_, logs) = run_script(settings(1), 1, |_, mut f| async move {
            accept_hello(&mut f).await;
        });
        let registered: Vec<_> = logs
            .iter()
            .filter(|(_, m)| m.starts_with("conn.registered - 已向伺服器註冊"))
            .collect();
        assert_eq!(registered.len(), 1, "{logs:?}");
        let (level, msg) = registered[0];
        assert_eq!(*level, log::Level::Info);
        assert!(
            msg.contains("伺服器離線判定 5000 ms")
                && msg.contains("TLSv1_3")
                && msg.contains("已驗證伺服器憑證"),
            "{msg}"
        );
        // 不再有 INFO 等級的「已建立 TLS 加密連線」
        assert!(
            !logs
                .iter()
                .any(|(l, m)| *l <= log::Level::Info && m.contains("已建立 TLS 加密連線")),
            "{logs:?}"
        );
    }

    /// 日誌訊息去掉開頭的「`<event> <code> `」（見 `codes::tag`）
    fn untagged(m: &str) -> &str {
        match m.split_once(' ') {
            Some((event, rest)) if event.contains('.') && event.is_ascii() => {
                rest.split_once(' ').map_or(rest, |(_, msg)| msg)
            }
            _ => m,
        }
    }

    /// 斷線日誌（「連線中斷」開頭）的等級與內容（去掉事件與代碼）。
    fn disconnect_logs(logs: &[(log::Level, String)]) -> Vec<(log::Level, String)> {
        logs.iter()
            .filter(|(_, m)| untagged(m).starts_with("連線中斷"))
            .map(|(l, m)| (*l, untagged(m).to_string()))
            .collect()
    }

    #[test]
    fn disconnect_logs_carry_event_and_code() {
        // log-ux §4.2、§6.1：事件與代碼在訊息開頭
        let refused = SessionError::Connect(io::Error::from(io::ErrorKind::ConnectionRefused));
        assert_eq!(error_code(&refused, 0, false), Some("PLS-E2003"));
        assert_eq!(disconnect_event(Some("PLS-E2003")).name, "conn.lost");
        assert_eq!(
            error_code(&SessionError::ConnectTimeout, 0, false),
            Some("PLS-E2004")
        );
        let dns =
            io::Error::other("failed to lookup address information: Name or service not known");
        assert_eq!(
            connect_error_code(&dns, &dns.to_string()),
            Some("PLS-E2002")
        );
        assert_eq!(
            error_code(
                &SessionError::IdleTimeout(Duration::from_secs(30)),
                0,
                false
            ),
            Some("PLS-E2005")
        );
        let fatal = |code: &str, message: &str| SessionError::server(code, message, true);
        for (err, streak, code, event) in [
            (
                fatal("certificate_rejected", "x"),
                0,
                "PLS-E1105",
                "auth.rejected",
            ),
            (
                fatal("certificate_rejected", REVOCATION_UNAVAILABLE),
                0,
                "PLS-E1203",
                "auth.server_crl",
            ),
            (fatal("auth_failed", "x"), 0, "PLS-E1102", "auth.mismatch"),
            (
                fatal(
                    "auth_failed",
                    "client certificate must contain exactly one DNS SAN",
                ),
                0,
                "PLS-E1103",
                "auth.mismatch",
            ),
            (fatal("superseded", ""), 1, "PLS-W3101", "conn.superseded"),
            (fatal("superseded", ""), 2, "PLS-E3102", "conn.superseded"),
            (
                fatal("bad_request", "server agent registry is full"),
                0,
                "PLS-E3201",
                "server.full",
            ),
            (
                fatal("unsupported_version", ""),
                0,
                "PLS-E2101",
                "proto.incompatible",
            ),
            (
                SessionError::ProtoMismatch(3),
                0,
                "PLS-E2101",
                "proto.incompatible",
            ),
        ] {
            let c = error_code(&err, streak, true);
            assert_eq!(c, Some(code), "{err}");
            assert_eq!(disconnect_event(c).name, event, "{err}");
        }
        assert_eq!(error_code(&SessionError::ClosedByServer, 0, true), None);
        assert_eq!(disconnect_event(None).name, "conn.lost");
        assert_eq!(untagged("conn.lost PLS-E2003 連線中斷"), "連線中斷");
        assert_eq!(untagged("連線中斷：x y"), "連線中斷：x y");
    }

    #[test]
    fn clock_skew_is_logged_on_level_changes_and_escalates_to_error() {
        // log-ux.md §4.5：30 秒～5 分鐘 WARN（PLS-W8301）、超過 5 分鐘 ERROR（PLS-E8302）；
        // 只在等級改變或偏差再變化超過 1 分鐘時記錄；回到 30 秒以內記一則 INFO
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-08T03:15:40Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // 本機比伺服器快 secs 秒時，伺服器的時間
        let server = |secs: i64| {
            (now - chrono::TimeDelta::seconds(secs))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        };
        let mut c = ClockSkewLog::default();
        assert_eq!(c.observe(&server(20), now), None, "20 秒：不記錄");
        let (level, w) = c.observe(&server(60), now).unwrap();
        assert_eq!(level, log::Level::Warn);
        assert!(
            w.starts_with("clock.skew PLS-W8301 本機時鐘比伺服器快約 60 秒"),
            "{w}"
        );
        assert_eq!(c.observe(&server(100), now), None, "同一等級、變化 40 秒");
        assert_eq!(c.observe(&server(60), now), None, "重連後同樣的偏差");
        let (level, _) = c.observe(&server(130), now).unwrap();
        assert_eq!(level, log::Level::Warn, "變化超過 1 分鐘再記一則");
        let (level, e) = c.observe(&server(-600), now).unwrap();
        assert_eq!(level, log::Level::Error);
        assert!(
            e.starts_with("clock.skew PLS-E8302 本機時鐘比伺服器慢約 600 秒"),
            "{e}"
        );
        assert_eq!(c.observe(&server(-640), now), None);
        assert!(c.observe(&server(-720), now).is_some(), "變化 120 秒");
        assert_eq!(c.observe("x", now), None, "無法解析：不改變狀態");
        let (level, i) = c.observe(&server(5), now).unwrap();
        assert_eq!(level, log::Level::Info);
        assert!(i.starts_with("clock.skew - "), "{i}");
        assert_eq!(c.observe(&server(5), now), None, "INFO 只記一則");
        assert_eq!(tls_version_name("TLSv1_3"), "TLSv1.3");
    }

    #[test]
    fn backoff_reset_rules_including_superseded() {
        // X1 + T6：穩定連線後正常斷線 → 重置；短連線 → 不重置；
        // 穩定連線後第一次被 superseded → 照常重置；連續被取代 → 不重置、error、延遲至少 30 秒
        let (dials, logs) = run_script(settings(1), 5, |i, mut f| async move {
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
    fn startup_and_first_reconnect_are_spread() {
        // A11：啟動時延遲 0～connect_spread 再連線；斷線後第一次重連另加 0～connect_spread，
        // 之後的重試只有指數退避與 ±20% 抖動
        assert_eq!(
            connect_spread(Duration::from_secs(15)),
            Duration::from_secs(5)
        );
        assert_eq!(
            connect_spread(Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        let cfg = AppConfig::default_for_tests();
        let s = Settings::new(&cfg, "a".into(), "h".into());
        assert_eq!(s.heartbeat_interval, Duration::from_secs(15));
        assert_eq!(s.connect_spread, Duration::from_secs(5));

        let spread = 5.0;
        let mut startup = Vec::new();
        for _ in 0..8 {
            let mut s = settings(15);
            s.connect_spread = Duration::from_secs(5);
            // 第 0 次：註冊後維持 31 秒（重置退避）再斷線；第 1、2 次：hello 後立即斷線
            let (dials, logs) = run_script(s, 3, |i, mut f| async move {
                if i == 0 {
                    accept_hello(&mut f).await;
                    ack_for(&mut f, Duration::from_secs(31)).await;
                } else {
                    let _ = f.next().await;
                }
            });
            assert_eq!(dials.len(), 4, "{dials:?}");
            let first = dials[0].as_secs_f64();
            assert!((0.0..=spread + 0.01).contains(&first), "{dials:?}");
            startup.push(first);
            if first > 0.0 {
                assert!(
                    logs.iter().any(|(l, m)| *l == log::Level::Info
                        && m.contains("秒後開始連線")
                        && m.contains("0～5 秒")),
                    "{logs:?}"
                );
            }
            let gap = |i: usize| (dials[i + 1] - dials[i]).as_secs_f64();
            // 第一次重連：2 s ±20% + 0～5 s
            let d1 = gap(0) - 31.0;
            assert!(d1 >= 1.6 - 0.01 && d1 <= 2.4 + spread + 0.01, "{dials:?}");
            // 連續第 2、3 次失敗：只有指數退避（4 s、8 s ±20%），不再加分散
            assert!(gap(1) >= 3.2 - 0.01 && gap(1) <= 4.8 + 0.01, "{dials:?}");
            assert!(gap(2) >= 6.4 - 0.01 && gap(2) <= 9.6 + 0.01, "{dials:?}");
        }
        // 啟動延遲是隨機的，不會全部相同
        assert!(startup.iter().any(|t| *t != startup[0]), "{startup:?}");
    }

    #[test]
    fn startup_delay_is_interrupted_by_shutdown() {
        // A11：啟動延遲期間收到結束要求時立即結束，不連線
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(async {
            let mut s = settings(15);
            s.connect_spread = Duration::from_secs(3600);
            let (tx, _rx) = mpsc::unbounded_channel();
            let dialer = FakeDialer {
                servers: tx,
                dial_times: RefCell::new(Vec::new()),
            };
            let shutdown = CancellationToken::new();
            shutdown.cancel();
            let (_report_tx, mut reports) = mpsc::channel::<Sample>(1);
            let start = Instant::now();
            run_with(&s, &dialer, &mut reports, None, &shutdown).await;
            assert!(dialer.dial_times.borrow().is_empty());
            assert!(start.elapsed() < Duration::from_secs(1));
        });
    }

    #[test]
    fn errors_after_max_retries_are_throttled() {
        // Q6：連續失敗達 max_retries 後的一般連線錯誤，ERROR 每 10 分鐘最多一則，其餘記 WARN
        let mut s = settings(1);
        s.max_delay = Duration::from_secs(30);
        let (dials, logs) = run_script(s, 40, |_, f| async move {
            drop(f); // 註冊前就關閉連線
        });
        let d = disconnect_logs(&logs);
        assert_eq!(d.len(), 40, "{d:?}");
        // 前 4 次未達 max_retries（5）：WARN
        for (level, msg) in &d[..4] {
            assert_eq!(*level, log::Level::Warn, "{msg}");
        }
        let errors = error_positions(&d);
        assert_eq!(errors[0], (4, 0), "第 5 次失敗立即以 ERROR 記錄：{d:?}");
        assert!(errors.len() >= 2, "{d:?}");
        let (second, n) = errors[1];
        assert_eq!(n, u64::try_from(second - 5).unwrap(), "{}", d[second].1);
        assert!(
            d[second].1.contains("次連線失敗已改記 WARN"),
            "{}",
            d[second].1
        );
        for (level, msg) in &d[5..second] {
            assert_eq!(*level, log::Level::Warn, "{msg}");
            assert!(msg.contains("連續第"), "{msg}");
        }
        // 第 i 則斷線日誌在第 i + 1 次 dial 之前
        assert!(dials[second] - dials[4] >= ATTENTION_LOG_WINDOW);
        assert!(dials[second - 1] - dials[4] < ATTENTION_LOG_WINDOW + Duration::from_secs(1));
        // 一般連線錯誤不寫 stderr（不經過 attention）
        assert!(!d.iter().any(|(_, m)| m.contains("相同原因的斷線")));
    }

    #[test]
    fn retries_exhausted_error_restarts_after_stable_connection() {
        // Q6：連線穩定（退避重置）後再次中斷，達到 max_retries 時立即以 ERROR 記錄，不受上一段節流影響
        let (_, logs) = run_script(settings(1), 11, |i, mut f| async move {
            if i == 5 {
                accept_hello(&mut f).await;
                ack_for(&mut f, Duration::from_secs(31)).await;
            } else {
                drop(f);
            }
        });
        let d = disconnect_logs(&logs);
        assert_eq!(d.len(), 11, "{d:?}");
        let levels: Vec<_> = d.iter().map(|(l, _)| *l).collect();
        use log::Level::{Error as E, Warn as W};
        // 第 6 則是穩定連線後的斷線（重新起算的連續第 1 次失敗）；第 10 則是新一段的第 5 次失敗，
        // 距上一則 ERROR 不到 10 分鐘仍立即記 ERROR；第 11 則回到節流
        assert_eq!(levels, [W, W, W, W, E, W, W, W, W, E, W], "{d:?}");
        assert!(d[5].1.contains("退避已重置"), "{}", d[5].1);
        assert!(d[9].1.contains("連續第 5 次失敗"), "{}", d[9].1);
        assert!(!d[9].1.contains("改記 WARN"), "{}", d[9].1);
    }

    #[test]
    fn repeated_superseded_delay_ignores_small_max_delay() {
        // X1：max_delay = 1 時，連續被取代後延遲仍至少 30 秒，不會每秒互踢；error 有節流
        let mut s = settings(1);
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
        let (_, logs) = run_script(settings(1), 3, move |i, mut f| async move {
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
        let mut s = settings(1);
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

    // ---- 第二輪審計的回歸測試 ----

    #[test]
    fn tls_and_io_error_text_is_sanitized() {
        // R2-P1：TLS／I/O 錯誤的文字可能含伺服器憑證的 SAN 原文
        let evil = "bad\\name\n2026-10-09 00:00:00 [INFO] 偽造\u{1b}[2J\u{202e}\u{e000}";
        let errors = [
            SessionError::Tls(io::Error::other(evil)),
            SessionError::Codec(LinesCodecError::Io(io::Error::other(evil))),
            SessionError::Connect(io::Error::other(evil)),
        ];
        for err in &errors {
            let text = err.to_string();
            assert!(!text.chars().any(is_unsafe_for_log), "{text:?}");
            assert!(text.contains("bad\\\\name\\n2026-10-09"), "{text}");
        }
        // 實際的 rustls 錯誤：名稱不符時會列出伺服器憑證中的名稱
        let presented = TlsError::InvalidCertificate(
            tokio_rustls::rustls::CertificateError::NotValidForNameContext {
                expected: tokio_rustls::rustls::pki_types::ServerName::try_from("kairos.local")
                    .unwrap(),
                presented: vec!["evil\n[ERROR] 偽造的日誌行\u{1b}[31m".to_string()],
            },
        );
        let err = SessionError::Tls(io::Error::new(io::ErrorKind::InvalidData, presented));
        let text = err.to_string();
        assert!(!text.chars().any(is_unsafe_for_log), "{text:?}");
        assert!(text.contains("evil\\n[ERROR]"), "{text}");
        // 仍可取出 rustls 錯誤給處置提示使用
        assert!(err.rustls_error().is_some());
        // 超長的錯誤文字會被截斷
        let long = SessionError::Tls(io::Error::other("x".repeat(100_000)));
        assert!(long.to_string().len() < 1000);
    }

    #[test]
    fn sanitize_escapes_backslash() {
        // R2-P12：字面的 `\n` 與轉義後的換行必須可區分
        assert_eq!(sanitize_for_log("a\\nb", 200), "a\\\\nb");
        assert_eq!(sanitize_for_log("a\nb", 200), "a\\nb");
        assert_ne!(
            sanitize_for_log("a\\nb", 200),
            sanitize_for_log("a\nb", 200)
        );
        assert_eq!(sanitize_for_log("\\u{1b}", 200), "\\\\u{1b}");
        assert_eq!(sanitize_for_log("\u{1b}", 200), "\\u{001b}");
    }

    #[test]
    fn sanitize_escapes_fillers_selectors_and_private_use() {
        // R2-K23：與 hostname 規則相同的新增碼位
        assert_eq!(
            sanitize_for_log("a\u{3164}b\u{fe0f}c\u{e000}d\u{115f}", 200),
            "a\\u{3164}b\\u{fe0f}c\\u{e000}d\\u{115f}"
        );
        assert_eq!(
            sanitize_for_log("\u{34f}\u{ffa0}\u{e0100}\u{10fffd}", 200),
            "\\u{034f}\\u{ffa0}\\u{e0100}\\u{10fffd}"
        );
    }

    #[test]
    fn repeated_superseded_hint_mentions_reissuing_client_cert_under_mtls() {
        // R2-P8：mTLS 下只刪 agent.id 會因 SAN 不符而無法啟動
        let mut s = settings(1);
        let err = SessionError::server("superseded", "", true);
        let hint = server_error_hint(&s, &err, 2).unwrap();
        assert!(hint.contains("刪除 agent.id"), "{hint}");
        assert!(!hint.contains("mTLS"), "{hint}");
        s.client_cert = Some("certs/client.pem".into());
        let hint = server_error_hint(&s, &err, 2).unwrap();
        assert!(
            hint.contains("mTLS") && hint.contains("重新簽發客戶端憑證"),
            "{hint}"
        );
        assert!(hint.contains("--print-agent-id"), "{hint}");
        // 第一次被取代的提示不變
        assert!(!server_error_hint(&s, &err, 1).unwrap().contains("重新簽發"));
    }

    #[test]
    fn revocation_unavailable_is_reported_as_server_side_problem() {
        // R2-P14：Kairos 的 CRL 過期時不可叫使用者重新簽發
        let mut s = settings(1);
        s.client_cert = Some("certs/client.pem".into());
        let err = SessionError::server("certificate_rejected", REVOCATION_UNAVAILABLE, true);
        let hint = server_error_hint(&s, &err, 0).unwrap();
        assert!(
            hint.contains("CRL") && hint.contains("伺服器端問題") && hint.contains("管理者"),
            "{hint}"
        );
        assert!(hint.contains("不需要重新簽發"), "{hint}");
        // 其他 certificate_rejected 仍提示重新簽發
        let err = SessionError::server("certificate_rejected", "certificate revoked", true);
        let hint = server_error_hint(&s, &err, 0).unwrap();
        assert!(
            hint.contains("聯絡管理者重新簽發") && hint.contains("certs/client.pem"),
            "{hint}"
        );
        assert!(!hint.contains("CRL"), "{hint}");
    }

    #[test]
    fn attention_log_restarts_when_reason_changes() {
        let mut log = AttentionLog::new();
        let t0 = Instant::now();
        assert_eq!(log.check("a", t0), Some(0));
        assert_eq!(log.check("a", t0 + Duration::from_secs(1)), None);
        assert_eq!(log.check("a", t0 + Duration::from_secs(2)), None);
        // 原因改變：立即記 error
        assert_eq!(log.check("b", t0 + Duration::from_secs(3)), Some(0));
        assert_eq!(log.check("a", t0 + Duration::from_secs(4)), Some(0));
        assert_eq!(log.check("a", t0 + Duration::from_secs(5)), None);
        // 視窗到期：記 error 並附上被改記 warn 的次數
        assert_eq!(
            log.check("a", t0 + Duration::from_secs(4) + ATTENTION_LOG_WINDOW),
            Some(1)
        );
        // 分類鍵：superseded 與非伺服器錯誤不在此列
        assert_eq!(
            attention_key(&SessionError::server("superseded", "", true), true),
            None
        );
        assert_eq!(attention_key(&SessionError::ClosedByServer, true), None);
        assert_ne!(
            attention_key(&SessionError::server("auth_failed", "", true), true),
            attention_key(
                &SessionError::server("certificate_rejected", "", true),
                true
            )
        );
        assert!(attention_key(&SessionError::ProtoMismatch(3), true).is_some());
    }

    /// 斷線日誌中，ERROR 的索引與其「另有 n 次…已改記 WARN」中的 n（沒有時為 0）。
    fn error_positions(d: &[(log::Level, String)]) -> Vec<(usize, u64)> {
        d.iter()
            .enumerate()
            .filter(|(_, (level, _))| *level == log::Level::Error)
            .map(|(i, (_, m))| {
                let n = m
                    .split("另有 ")
                    .nth(1)
                    .and_then(|rest| rest.split(' ').next())
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                (i, n)
            })
            .collect()
    }

    #[test]
    fn repeated_superseded_error_is_logged_again_after_ten_minutes() {
        // R2-T3：暫停的時鐘下驗證 10 分鐘節流：視窗內改記 WARN，視窗到期再記 ERROR 並附上次數
        let (dials, logs) = run_script(settings(1), 16, |_, mut f| async move {
            accept_hello(&mut f).await;
            ack_for(&mut f, Duration::from_secs(1)).await;
            f.send(r#"{"type":"error","code":"superseded","fatal":true}"#.to_string())
                .await
                .unwrap();
        });
        let d = disconnect_logs(&logs);
        assert_eq!(d.len(), 16, "{d:?}");
        let errors = error_positions(&d);
        // 第 1 次被取代為 warn；連續第 2 次起為 error；之後要到 10 分鐘後才再記 error
        assert!(errors.len() >= 2, "{d:?}");
        assert_eq!(errors[0], (1, 0), "{d:?}");
        let (second, n) = errors[1];
        assert_eq!(
            n,
            u64::try_from(second - 2).unwrap(),
            "第二則 ERROR 應附上中間改記 WARN 的次數：{}",
            d[second].1
        );
        assert!(d[second].1.contains("「連續被取代」的斷線已改記 WARN"));
        for (level, msg) in &d[2..second] {
            assert_eq!(*level, log::Level::Warn, "{msg}");
        }
        // 第 i 則斷線日誌在第 i + 1 次 dial 之前；兩則 ERROR 之間至少相隔 10 分鐘，
        // 且第二則 ERROR 之前那一則 WARN 距第一則 ERROR 不到 10 分鐘
        let first_error_at = dials[1];
        let second_error_at = dials[second];
        assert!(second_error_at - first_error_at >= ATTENTION_LOG_WINDOW);
        assert!(dials[second - 1] - first_error_at < ATTENTION_LOG_WINDOW + Duration::from_secs(2));
    }

    #[test]
    fn repeated_fatal_server_errors_are_throttled() {
        // R2-P7：auth_failed 等致命錯誤每次重連都會重複，ERROR 每 10 分鐘最多一則
        let mut s = settings(1);
        s.max_delay = Duration::from_secs(30);
        let (dials, logs) = run_script(s, 30, |_, mut f| async move {
            let _ = f.next().await; // hello
            f.send(
                r#"{"type":"error","seq":1,"code":"auth_failed","message":"agent_id does not match certificate","fatal":true}"#
                    .to_string(),
            )
            .await
            .unwrap();
        });
        let d = disconnect_logs(&logs);
        assert_eq!(d.len(), 30, "{d:?}");
        let errors = error_positions(&d);
        assert_eq!(errors[0], (0, 0), "第一次立即以 ERROR 記錄：{d:?}");
        assert!(errors.len() >= 2, "{d:?}");
        let (second, n) = errors[1];
        assert_eq!(n, u64::try_from(second - 1).unwrap(), "{}", d[second].1);
        assert!(
            d[second].1.contains("相同原因的斷線已改記 WARN"),
            "{}",
            d[second].1
        );
        // 視窗內（即使連續失敗已超過 retry.max_retries）改記 WARN，提示仍在
        for (level, msg) in &d[1..second] {
            assert_eq!(*level, log::Level::Warn, "{msg}");
            assert!(msg.contains("auth_failed") && msg.contains("SAN"), "{msg}");
        }
        assert!(second > 5, "應涵蓋超過 max_retries 的情況：{second}");
        assert!(dials[second] - dials[0] >= ATTENTION_LOG_WINDOW);
        assert!(dials[second - 1] - dials[0] < ATTENTION_LOG_WINDOW);
    }

    fn test_sample(n: u32) -> Sample {
        Sample {
            ts: format!("t{n}"),
            cpu_usage: 1.0,
            total_memory: 2,
            used_memory: 1,
            total_memory_bytes: 2 * 1024 * 1024,
            used_memory_bytes: 1024 * 1024,
            extra: Vec::new(),
        }
    }

    /// 測試用伺服器：回覆 hello_ack，把收到的 report 交給回傳的接收端，並送出 `lines` 送來的每一行；
    /// `lines` 的傳送端全部關閉時關閉連線。
    fn spawn_report_server(
        server: DuplexStream,
    ) -> (
        mpsc::UnboundedReceiver<serde_json::Value>,
        mpsc::UnboundedSender<String>,
    ) {
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut f = server_side(server);
            accept_hello(&mut f).await;
            loop {
                tokio::select! {
                    line = f.next() => {
                        let Some(Ok(line)) = line else { return };
                        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                        if v["type"] == "report" {
                            let _ = seen_tx.send(v);
                        }
                    }
                    line = line_rx.recv() => {
                        let Some(line) = line else { return };
                        if f.send(line).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        (seen_rx, line_tx)
    }

    #[test]
    fn report_is_skipped_while_previous_one_is_unanswered() {
        // R2-P9：上一筆 report 尚未收到回應時略過新取樣；ack 或帶 seq 的 error 都算回應；
        // 斷線後未確認狀態清除
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let ((), logs) = crate::test_log::capture(|| {
            rt.block_on(async {
                let s = settings(60);
                let (tx, mut rx) = mpsc::channel::<Sample>(1);
                let (client1, server1) = duplex(64 * 1024);
                let (client2, server2) = duplex(64 * 1024);
                let (mut seen1, lines1) = spawn_report_server(server1);
                let (mut seen2, lines2) = spawn_report_server(server2);
                let one_sec = || time::sleep(Duration::from_secs(1));

                let client = async {
                    let (err, registered) = serve_with_reports(client1, &s, &mut rx).await;
                    assert!(registered && matches!(err, SessionError::ClosedByServer), "{err:?}");
                    let (err, registered) = serve_with_reports(client2, &s, &mut rx).await;
                    assert!(registered && matches!(err, SessionError::ClosedByServer), "{err:?}");
                };
                let driver = async {
                    one_sec().await;
                    tx.send(test_sample(1)).await.unwrap();
                    let r1 = seen1.recv().await.unwrap();
                    assert_eq!(r1["ts"], "t1");
                    // 未回應期間的取樣全部略過
                    for n in 2..=4 {
                        tx.send(test_sample(n)).await.unwrap();
                        one_sec().await;
                    }
                    assert!(seen1.try_recv().is_err(), "未回應時不得再送 report");
                    // 收到 ack 後恢復送出
                    lines1
                        .send(format!(r#"{{"type":"ack","seq":{}}}"#, r1["seq"]))
                        .unwrap();
                    one_sec().await;
                    tx.send(test_sample(5)).await.unwrap();
                    let r2 = seen1.recv().await.unwrap();
                    assert_eq!(r2["ts"], "t5");
                    // storage_unavailable（帶 seq 的 error）也算回應
                    lines1
                        .send(format!(
                            r#"{{"type":"error","seq":{},"code":"storage_unavailable","message":"disk full","fatal":false}}"#,
                            r2["seq"]
                        ))
                        .unwrap();
                    one_sec().await;
                    tx.send(test_sample(6)).await.unwrap();
                    let r3 = seen1.recv().await.unwrap();
                    assert_eq!(r3["ts"], "t6");
                    // r3 未回應就斷線
                    drop(lines1);
                    one_sec().await;
                    // 新連線不受上一條連線未確認的 report 影響
                    tx.send(test_sample(7)).await.unwrap();
                    let r4 = seen2.recv().await.unwrap();
                    assert_eq!(r4["ts"], "t7");
                    drop(lines2);
                };
                tokio::join!(client, driver);
            })
        });
        let skipped: Vec<_> = logs
            .iter()
            .filter(|(_, m)| m.contains("尚未收到伺服器回應"))
            .collect();
        // 3 次略過在 10 秒節流視窗內只記一則 WARN
        assert_eq!(skipped.len(), 1, "{logs:?}");
        assert_eq!(skipped[0].0, log::Level::Warn);
        assert!(
            skipped[0].1.contains("ts=t2") && skipped[0].1.contains("已略過 1 次"),
            "{}",
            skipped[0].1
        );
    }

    // ---- N4：需要人工處理的錯誤寫 stderr、systemd STATUS ----

    use crate::service_status::Event;

    /// 期間的 stderr 輸出（不含 STATUS）。
    fn stderr_events(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Stderr(m) => Some(m.as_str()),
                Event::Status(_) => None,
            })
            .collect()
    }

    /// 期間送出的 STATUS 文字。
    fn status_events(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Status(s) => Some(s.as_str()),
                Event::Stderr(_) => None,
            })
            .collect()
    }

    fn tls_alert(alert: AlertDescription) -> SessionError {
        SessionError::Tls(io::Error::new(
            io::ErrorKind::InvalidData,
            TlsError::AlertReceived(alert),
        ))
    }

    #[test]
    fn stderr_is_written_only_when_attention_is_logged_as_error() {
        // 節流判斷後以 ERROR 記錄（第一次、或視窗到期）才寫 stderr；視窗內改記 WARN 的不寫
        assert_eq!(attention_outcome(Some(0)), (log::Level::Error, true));
        assert_eq!(attention_outcome(Some(12)), (log::Level::Error, true));
        assert_eq!(attention_outcome(None), (log::Level::Warn, false));

        // 與 AttentionLog 搭配：同一原因 10 分鐘內只寫一次 stderr
        let mut log = AttentionLog::new();
        let t0 = Instant::now();
        let written = |c| attention_outcome(c).1;
        assert!(written(log.check("server:auth_failed:true", t0)));
        assert!(!written(
            log.check("server:auth_failed:true", t0 + Duration::from_secs(60))
        ));
        assert!(written(
            log.check("server:auth_failed:true", t0 + ATTENTION_LOG_WINDOW)
        ));
        // 原因改變時立即再寫
        assert!(written(log.check(
            "tls:CertificateRevoked",
            t0 + ATTENTION_LOG_WINDOW + Duration::from_secs(1)
        )));
    }

    #[test]
    fn only_manual_attention_errors_qualify_for_stderr() {
        // 需要人工處理：伺服器致命錯誤、協定版本不符、客戶端憑證被拒的 TLS alert
        let fatal = |code: &str, msg: &str| SessionError::server(code, msg, true);
        for err in [
            fatal("certificate_rejected", "revoked"),
            fatal("auth_failed", "agent_id does not match certificate"),
            fatal("unsupported_version", "supported proto: 3"),
            fatal("bad_request", "agent registry is full"),
            SessionError::ProtoMismatch(3),
            tls_alert(AlertDescription::CertificateRevoked),
            tls_alert(AlertDescription::CertificateExpired),
            tls_alert(AlertDescription::CertificateRequired),
        ] {
            assert!(needs_attention(&err, 0, false), "{err}");
        }
        // 不信任類的 alert 只在設定了客戶端憑證時才算
        assert!(needs_attention(
            &tls_alert(AlertDescription::UnknownCA),
            0,
            true
        ));
        assert!(!needs_attention(
            &tls_alert(AlertDescription::UnknownCA),
            0,
            false
        ));
        // 一般連線錯誤：不寫 stderr
        for err in [
            SessionError::ConnectTimeout,
            SessionError::Connect(io::Error::from(io::ErrorKind::ConnectionRefused)),
            SessionError::TlsTimeout,
            SessionError::Tls(io::Error::from(io::ErrorKind::ConnectionReset)),
            tls_alert(AlertDescription::HandshakeFailure),
            SessionError::ClosedByServer,
            SessionError::HelloAckTimeout,
            SessionError::IdleTimeout(Duration::from_secs(10)),
            SessionError::WriteTimeout,
        ] {
            assert!(!needs_attention(&err, 0, true), "{err}");
        }
        // superseded 只有連續被取代才算
        assert!(!needs_attention(&fatal("superseded", ""), 1, true));
        assert!(needs_attention(&fatal("superseded", ""), 2, true));
    }

    #[test]
    fn systemd_status_texts() {
        let fatal = |code: &str, msg: &str| SessionError::server(code, msg, true);
        assert_eq!(
            disconnect_status(&fatal("certificate_rejected", "revoked"), 0, true),
            "無法註冊：certificate_rejected（客戶端憑證已被撤銷、過期或不受信任，請聯絡管理者重新簽發）"
        );
        assert_eq!(
            disconnect_status(
                &fatal("certificate_rejected", "revocation status unavailable"),
                0,
                true
            ),
            "無法註冊：certificate_rejected（伺服器的 CRL 已過期或無法判定撤銷狀態，請聯絡伺服器管理者）"
        );
        assert_eq!(
            disconnect_status(&fatal("auth_failed", "x"), 0, true),
            "無法註冊：auth_failed（伺服器拒絕本機身分，請確認憑證 SAN 為 DNS:<agent_id>）"
        );
        assert_eq!(
            disconnect_status(&fatal("unsupported_version", "x"), 0, true),
            "無法註冊：unsupported_version（協定版本不符，請更新 Pulse 或 Kairos）"
        );
        assert_eq!(
            disconnect_status(&fatal("bad_request", "agent registry is full"), 0, true),
            "無法註冊：bad_request（伺服器狀態表已滿，請管理者調高 limits.max_agents）"
        );
        assert_eq!(
            disconnect_status(&SessionError::server("invalid_agent", "x", false), 0, true),
            "無法註冊：invalid_agent（伺服器拒絕註冊，詳見 logs/）"
        );
        assert_eq!(
            disconnect_status(&fatal("", ""), 0, true),
            "無法註冊：（未提供錯誤碼）（伺服器拒絕註冊，詳見 logs/）"
        );
        assert_eq!(
            disconnect_status(&SessionError::ProtoMismatch(3), 0, true),
            "無法註冊：proto_mismatch（伺服器的協定版本 3 不在本程式支援的範圍（1～2）內，請更新 Pulse 或 Kairos）"
        );
        assert_eq!(
            disconnect_status(&tls_alert(AlertDescription::CertificateRevoked), 0, true),
            "無法註冊：TLS alert CertificateRevoked（客戶端憑證已被撤銷，請聯絡管理者重新簽發）"
        );
        assert_eq!(
            disconnect_status(&tls_alert(AlertDescription::CertificateExpired), 0, true),
            "無法註冊：TLS alert CertificateExpired（客戶端憑證已過期，請聯絡管理者重新簽發）"
        );
        assert_eq!(
            disconnect_status(&tls_alert(AlertDescription::UnknownCA), 0, true),
            "無法註冊：TLS alert UnknownCA（伺服器不信任此客戶端憑證，請確認由伺服器信任的 client CA 簽發）"
        );
        assert_eq!(
            disconnect_status(&fatal("superseded", ""), 2, true),
            "無法註冊：superseded（多台主機可能共用同一份 agent.id）"
        );
        // 一般連線錯誤與首次被取代：重新連線中
        for (err, streak) in [
            (SessionError::ClosedByServer, 0),
            (SessionError::ConnectTimeout, 0),
            (tls_alert(AlertDescription::UnknownCA), 0),
            (fatal("superseded", ""), 1),
        ] {
            let has_client_cert = !matches!(err, SessionError::Tls(_));
            assert_eq!(
                disconnect_status(&err, streak, has_client_cert),
                "重新連線中",
                "{err}"
            );
        }
        // 註冊成功
        let mut s = settings(1);
        s.address = "kairos.example.com".into();
        s.port = 8443;
        assert_eq!(registered_status(&s), "已註冊：kairos.example.com:8443");
        s.address = "::1".into();
        assert_eq!(registered_status(&s), "已註冊：[::1]:8443");
    }

    #[test]
    fn status_texts_from_server_are_single_line() {
        // 伺服器送來的 code 已清理：不會夾帶換行到 STATUS（sd_notify 以換行分隔欄位）
        let err = SessionError::server("bad\ncode\r\u{1b}[31m", "x", true);
        let status = disconnect_status(&err, 0, true);
        assert!(!status.contains('\n') && !status.contains('\r') && !status.contains('\u{1b}'));
    }

    /// 同 [`run_script`]，另外回傳期間的 stderr／STATUS 輸出。
    fn run_script_with_events<F, Fut>(
        s: Settings,
        sessions: usize,
        script: F,
    ) -> (Vec<Duration>, Vec<(log::Level, String)>, Vec<Event>)
    where
        F: Fn(usize, ServerSide) -> Fut + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        let ((dials, logs), events) =
            crate::service_status::capture(|| run_script(s, sessions, script));
        (dials, logs, events)
    }

    #[test]
    fn revoked_certificate_is_reported_to_stderr_and_systemd_status() {
        // N4：註冊後被撤銷（certificate_rejected）→ 之後每次重連都被拒。
        // stderr 只在 ERROR 時寫（10 分鐘最多一次），STATUS 顯示錯誤碼與提示摘要
        let mut s = settings(1);
        s.max_delay = Duration::from_secs(30);
        s.client_cert = Some("client.pem".into());
        let (dials, logs, events) = run_script_with_events(s, 30, |i, mut f| async move {
            if i == 0 {
                accept_hello(&mut f).await;
                ack_for(&mut f, Duration::from_secs(40)).await;
                f.send(
                    r#"{"type":"error","seq":null,"code":"certificate_rejected","message":"client certificate has been revoked","fatal":true}"#
                        .to_string(),
                )
                .await
                .unwrap();
                drain(&mut f).await;
            } else {
                let _ = f.next().await; // hello
                f.send(
                    r#"{"type":"error","seq":1,"code":"certificate_rejected","message":"client certificate has been revoked","fatal":true}"#
                        .to_string(),
                )
                .await
                .unwrap();
            }
        });
        let d = disconnect_logs(&logs);
        assert_eq!(d.len(), 30, "{d:?}");
        let errors = error_positions(&d);
        let stderr = stderr_events(&events);
        // journal 中也有事件與代碼（log-ux §4.2：certificate_rejected → auth.rejected PLS-E1105）
        assert!(
            stderr
                .iter()
                .all(|s| s.starts_with("auth.rejected PLS-E1105 連線中斷")),
            "{stderr:?}"
        );
        let stderr: Vec<&str> = stderr.iter().map(|s| untagged(s)).collect();
        // stderr 的內容與 ERROR 日誌完全相同，且只有 ERROR 那幾則
        let error_msgs: Vec<&str> = errors.iter().map(|(i, _)| d[*i].1.as_str()).collect();
        assert_eq!(stderr, error_msgs, "{events:?}");
        assert!(errors.len() >= 2 && errors.len() < d.len(), "{d:?}");
        assert!(
            stderr[0].contains("certificate_rejected") && stderr[0].contains("重新簽發"),
            "{}",
            stderr[0]
        );
        // 第二次寫 stderr 距第一次至少 10 分鐘
        let (second, _) = errors[1];
        assert!(dials[second] - dials[0] >= ATTENTION_LOG_WINDOW);

        let status = status_events(&events);
        assert_eq!(status[..2], ["連線中", "已註冊：127.0.0.1:1"], "{status:?}");
        assert_eq!(status.len(), 2 + d.len(), "{status:?}");
        for st in &status[2..] {
            assert_eq!(
                *st,
                "無法註冊：certificate_rejected（客戶端憑證已被撤銷、過期或不受信任，請聯絡管理者重新簽發）"
            );
        }
    }

    #[test]
    fn ordinary_disconnects_do_not_write_stderr() {
        // 一般連線錯誤（伺服器關閉連線、註冊前被關閉），即使超過 retry.max_retries 而記 ERROR，也不寫 stderr
        let (_, logs, events) = run_script_with_events(settings(1), 8, |i, mut f| async move {
            if i == 0 {
                accept_hello(&mut f).await;
                ack_for(&mut f, Duration::from_secs(1)).await;
            }
        });
        let d = disconnect_logs(&logs);
        assert_eq!(d.len(), 8, "{d:?}");
        assert!(
            d.iter().any(|(l, _)| *l == log::Level::Error),
            "應有超過 max_retries 的 ERROR：{d:?}"
        );
        assert!(stderr_events(&events).is_empty(), "{events:?}");
        let status = status_events(&events);
        assert_eq!(status[..2], ["連線中", "已註冊：127.0.0.1:1"]);
        assert!(status[2..].iter().all(|s| *s == "重新連線中"), "{status:?}");
        assert_eq!(status.len(), 10, "{status:?}");
    }

    #[test]
    fn status_returns_to_registered_after_recovery() {
        // 狀態表已滿 → 管理者調高上限後重新註冊成功
        let (_, _, events) = run_script_with_events(settings(1), 3, |i, mut f| async move {
            if i < 2 {
                let _ = f.next().await; // hello
                f.send(
                    r#"{"type":"error","seq":1,"code":"bad_request","message":"agent registry is full","fatal":true}"#
                        .to_string(),
                )
                .await
                .unwrap();
            } else {
                accept_hello(&mut f).await;
                ack_for(&mut f, Duration::from_secs(1)).await;
            }
        });
        let full = "無法註冊：bad_request（伺服器狀態表已滿，請管理者調高 limits.max_agents）";
        assert_eq!(
            status_events(&events),
            ["連線中", full, full, "已註冊：127.0.0.1:1", "重新連線中"]
        );
        // 狀態表已滿只寫一次 stderr（第二次在 10 分鐘視窗內）
        let stderr = stderr_events(&events);
        assert_eq!(stderr.len(), 1, "{stderr:?}");
        assert!(stderr[0].contains("limits.max_agents"), "{}", stderr[0]);
    }

    fn server_cert_error(e: tokio_rustls::rustls::CertificateError) -> SessionError {
        SessionError::Tls(io::Error::new(
            io::ErrorKind::InvalidData,
            TlsError::InvalidCertificate(e),
        ))
    }

    #[test]
    fn server_certificate_problems_are_persistent_errors() {
        use tokio_rustls::rustls::{CertificateError as C, OtherError};
        let ca_as_leaf = C::Other(OtherError(std::sync::Arc::new(
            webpki::Error::CaUsedAsEndEntity,
        )));
        let cases = [
            (
                server_cert_error(C::UnknownIssuer),
                "無法連線：伺服器憑證不受信任（簽發者不在信任庫中，自簽憑證或私有 CA 請以 server.ca_cert 指定）",
            ),
            (
                server_cert_error(C::NotValidForName),
                "無法連線：伺服器憑證名稱不符（請把 server.domain 設為伺服器憑證 SAN 中的名稱）",
            ),
            (
                server_cert_error(C::Expired),
                "無法連線：伺服器憑證已過期（請伺服器管理者更新憑證，或檢查本機時鐘）",
            ),
            (
                server_cert_error(C::NotValidYet),
                "無法連線：伺服器憑證尚未生效（請檢查本機時鐘或伺服器憑證的有效期間）",
            ),
            (
                server_cert_error(C::Revoked),
                "無法連線：伺服器憑證已被撤銷（請伺服器管理者更換伺服器憑證）",
            ),
            (
                server_cert_error(C::InvalidPurpose),
                "無法連線：伺服器憑證用途不符（伺服器憑證的 EKU 必須含 serverAuth）",
            ),
            (
                server_cert_error(ca_as_leaf),
                "無法連線：伺服器憑證被標記為 CA（伺服器憑證必須是 basicConstraints CA:FALSE 的終端憑證，請重新簽發）",
            ),
            (
                server_cert_error(C::BadSignature),
                "無法連線：伺服器憑證驗證失敗（詳見 logs/）",
            ),
        ];
        for (err, status) in &cases {
            assert!(needs_attention(err, 0, false), "{err}");
            assert_eq!(disconnect_status(err, 0, false), *status);
        }
        // 讀取時才出現的驗證錯誤（包在 Codec 的 I/O 錯誤中）也算
        let read_err = SessionError::Codec(LinesCodecError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            TlsError::InvalidCertificate(C::Expired),
        )));
        assert!(needs_attention(&read_err, 0, false));
        // 不同原因的節流鍵不同（原因改變時立即再記 ERROR）
        assert_ne!(
            attention_key(&cases[0].0, false),
            attention_key(&cases[1].0, false)
        );
        // 一般網路錯誤仍不算
        for err in [
            SessionError::Connect(io::Error::from(io::ErrorKind::ConnectionRefused)),
            SessionError::ConnectTimeout,
            SessionError::TlsTimeout,
            SessionError::Tls(io::Error::from(io::ErrorKind::ConnectionReset)),
        ] {
            assert!(!needs_attention(&err, 0, false), "{err}");
            assert_eq!(disconnect_status(&err, 0, false), "重新連線中");
        }
    }

    #[test]
    fn server_certificate_problem_is_logged_as_error_and_written_to_stderr() {
        // 第一次就 ERROR 並寫 stderr；之後 10 分鐘內改記 WARN、不寫 stderr；視窗到期再寫
        let err = server_cert_error(tokio_rustls::rustls::CertificateError::UnknownIssuer);
        let key = attention_key(&err, false).unwrap();
        let mut log = AttentionLog::new();
        let t0 = Instant::now();
        let ((), logs) = crate::test_log::capture(|| {
            for (at, expect_stderr) in [
                (t0, true),
                (t0 + Duration::from_secs(5), false),
                (t0 + ATTENTION_LOG_WINDOW, true),
            ] {
                let ((), events) = crate::service_status::capture(|| {
                    log_attention(log.check(&key, at), "連線中斷：x", "相同原因的斷線");
                });
                assert_eq!(!events.is_empty(), expect_stderr, "{events:?}");
            }
        });
        let levels: Vec<_> = logs.iter().map(|(l, _)| *l).collect();
        assert_eq!(
            levels,
            [log::Level::Error, log::Level::Warn, log::Level::Error]
        );
    }

    // ---- 協定 v2：版本協商與 report 格式（A3、A4 的相容矩陣，Pulse 端） ----

    /// 測試用伺服器：讀取 hello、回覆 `hello_ack`（`seq` 由收到的 hello 帶入），之後把第一則
    /// report 連同 hello 回傳，並關閉連線
    fn spawn_versioned_server(
        server: DuplexStream,
        hello_ack: &'static str,
    ) -> tokio::task::JoinHandle<(serde_json::Value, Option<serde_json::Value>)> {
        tokio::spawn(async move {
            let mut f = server_side(server);
            let line = f.next().await.unwrap().unwrap();
            let hello: serde_json::Value = serde_json::from_str(&line).unwrap();
            f.send(hello_ack.replace("SEQ", &hello["seq"].to_string()))
                .await
                .unwrap();
            while let Some(Ok(line)) = f.next().await {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                if v["type"] == "report" {
                    return (hello, Some(v));
                }
            }
            (hello, None)
        })
    }

    async fn report_against(
        hello_ack: &'static str,
    ) -> (serde_json::Value, Option<serde_json::Value>, SessionError) {
        let (client, server) = duplex(64 * 1024);
        let srv = spawn_versioned_server(server, hello_ack);
        let mut s = settings(60);
        s.host = HostInfo {
            display_name: Some("Web 01".into()),
            report_interval_ms: Some(5000),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::channel::<Sample>(1);
        let mut sample = test_sample(1);
        sample.extra = vec![
            crate::protocol::Metric::new("system.filesystem.total", 10, "By")
                .with_label("mountpoint", "/"),
        ];
        let feeder = async {
            time::sleep(Duration::from_secs(1)).await;
            let _ = tx.send(sample).await;
        };
        let ((err, _), ()) = tokio::join!(serve_with_reports(client, &s, &mut rx), feeder);
        let (hello, report) = srv.await.unwrap();
        (hello, report, err)
    }

    #[tokio::test(start_paused = true)]
    async fn hello_offers_versions_1_to_2_with_v2_fields() {
        let (hello, _, _) =
            report_against(r#"{"type":"hello_ack","seq":SEQ,"proto":1,"offline_after_ms":5000}"#)
                .await;
        // proto 送 1（v1 Kairos 只接受 1），另以範圍與 features 宣告 v2
        assert_eq!(hello["proto"], 1);
        assert_eq!(hello["proto_min"], 1);
        assert_eq!(hello["proto_max"], 2);
        assert_eq!(hello["features"], serde_json::json!(["metrics"]));
        assert_eq!(hello["display_name"], "Web 01");
        assert_eq!(hello["report_interval_ms"], 5000);
        // 沒有值的主機資訊欄位不送
        assert!(hello.get("tags").is_none(), "{hello}");
    }

    #[tokio::test(start_paused = true)]
    async fn old_kairos_gets_v1_reports() {
        // 新 Pulse ↔ 舊 Kairos：hello_ack 沒有 v2 欄位 → report 為三欄位
        let (_, report, err) =
            report_against(r#"{"type":"hello_ack","seq":SEQ,"proto":1,"offline_after_ms":5000}"#)
                .await;
        let report = report.expect("應送出 report");
        assert!(matches!(err, SessionError::ClosedByServer), "{err:?}");
        assert_eq!(report["cpu_usage"], 1.0);
        assert_eq!(report["total_memory"], 2);
        assert_eq!(report["used_memory"], 1);
        assert!(report.get("metrics").is_none(), "{report}");
    }

    #[tokio::test(start_paused = true)]
    async fn new_kairos_gets_metric_reports() {
        // 新 ↔ 新：v2 且雙方都宣告 metrics → 通用指標
        let (_, report, _) = report_against(
            r#"{"type":"hello_ack","seq":SEQ,"proto":2,"offline_after_ms":5000,"features":["metrics"],"server_time":"2026-10-08T03:15:40.012Z"}"#,
        )
        .await;
        let report = report.expect("應送出 report");
        assert!(report.get("cpu_usage").is_none(), "{report}");
        let names: Vec<&str> = report["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "system.cpu.usage",
                "system.memory.total",
                "system.memory.used",
                "system.filesystem.total"
            ]
        );
        assert_eq!(report["metrics"][1]["value"], 2 * 1024 * 1024);
    }

    #[tokio::test(start_paused = true)]
    async fn v2_kairos_without_metrics_gets_v1_reports() {
        let (_, report, _) = report_against(
            r#"{"type":"hello_ack","seq":SEQ,"proto":2,"offline_after_ms":5000,"features":[]}"#,
        )
        .await;
        let report = report.expect("應送出 report");
        assert_eq!(report["cpu_usage"], 1.0);
        assert!(report.get("metrics").is_none(), "{report}");
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_negotiated_version_disconnects() {
        let (_, report, err) =
            report_against(r#"{"type":"hello_ack","seq":SEQ,"proto":3,"offline_after_ms":5000}"#)
                .await;
        assert!(matches!(err, SessionError::ProtoMismatch(3)), "{err:?}");
        assert!(report.is_none());
    }
}
