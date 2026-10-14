//! syslog 外送（RFC 5424；傳輸：UDP RFC 5426、TCP RFC 6587 octet counting、本機 Unix socket）。
//!
//! 一行日誌：
//!
//! ```text
//! <PRI>1 TIMESTAMP HOSTNAME APP-NAME PROCID MSGID - MSG
//! <28>1 2026-10-14T06:22:16.123Z kairos01 kairos 4151 KRS-W3005 - 警告 KRS-W3005：已暫時封鎖…
//! ```
//!
//! - `PRI` ＝ facility × 8 ＋ severity（error 3、warn 4、info 6、debug／trace 7）。
//! - `MSGID` 為錯誤代碼（沒有時為事件名稱，最多 32 個字元），讓 SIEM 不必解析訊息就能篩選。
//! - `STRUCTURED-DATA` 為 `-`（沒有向 IANA 申請的企業代碼，不自訂 SD-ID）；需要欄位時改用
//!   `payload = "json"`，`MSG` 為與 `log.format = "json"` 相同的一行 JSON。
//! - `MSG` 不加 BOM（RFC 5424 的 MSG-ANY），避免接收端顯示亂碼。
//!
//! 送出在背景執行緒進行：記錄日誌的執行緒只把字串放進有上限的佇列（[`QUEUE_CAPACITY`]），佇列滿或
//! 接收端無法連線時丟棄並計數（[`SyslogStats`]），不阻塞服務本身。TCP 斷線後以 1～30 秒退避重連。

use chrono::{DateTime, Utc};
use log::Level;
use std::io::{self, Write};
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 背景佇列的上限（行）
pub const QUEUE_CAPACITY: usize = 4096;
/// UDP 一則訊息的上限（bytes）；超過時截斷（RFC 5426 要求接收端至少接受 2048，常見實作可到 64 KiB）
pub const UDP_MAX_BYTES: usize = 8192;
/// TCP、Unix socket 一則訊息的上限（bytes）
pub const STREAM_MAX_BYTES: usize = 64 * 1024;
/// 連線或寫入的逾時
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// 重連的退避
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// 傳輸方式
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// `udp://host:port`（預設埠 514）
    Udp(String),
    /// `tcp://host:port`（預設埠 601，RFC 6587 octet counting）
    Tcp(String),
    /// `unix:///dev/log`（datagram；Unix 平台）
    Unix(PathBuf),
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Transport::Udp(a) => write!(f, "udp://{a}"),
            Transport::Tcp(a) => write!(f, "tcp://{a}"),
            Transport::Unix(p) => write!(f, "unix://{}", p.display()),
        }
    }
}

/// 解析 `log.syslog.target`
pub fn parse_target(s: &str) -> Result<Transport, String> {
    let s = s.trim();
    let with_port = |rest: &str, default: u16| -> Result<String, String> {
        if rest.is_empty() {
            return Err(format!("{s}：缺少主機"));
        }
        // [IPv6]:port、host:port、host
        let has_port = if let Some(end) = rest.rfind(']') {
            rest[end..].contains(':')
        } else {
            rest.matches(':').count() == 1
        };
        if has_port {
            let port = rest.rsplit(':').next().unwrap_or("");
            port.parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .ok_or_else(|| format!("{s}：連接埠不合法"))?;
            Ok(rest.to_string())
        } else if rest.contains(':') && !rest.starts_with('[') {
            Err(format!(
                "{s}：IPv6 位址請以 [ ] 包住，例如 udp://[2001:db8::1]:514"
            ))
        } else {
            Ok(format!("{rest}:{default}"))
        }
    };
    if let Some(rest) = s.strip_prefix("udp://") {
        Ok(Transport::Udp(with_port(rest, 514)?))
    } else if let Some(rest) = s.strip_prefix("tcp://") {
        Ok(Transport::Tcp(with_port(rest, 601)?))
    } else if let Some(rest) = s.strip_prefix("unix://") {
        if !rest.starts_with('/') {
            return Err(format!(
                "{s}：Unix socket 必須是絕對路徑，例如 unix:///dev/log"
            ));
        }
        if cfg!(unix) {
            Ok(Transport::Unix(PathBuf::from(rest)))
        } else {
            Err(format!(
                "{s}：這個平台不支援 Unix socket，請改用 udp:// 或 tcp://"
            ))
        }
    } else {
        Err(format!(
            "{s}：格式應為 udp://主機:埠、tcp://主機:埠 或 unix:///路徑"
        ))
    }
}

/// facility 名稱 → 數值（RFC 5424 表 1）
pub fn parse_facility(s: &str) -> Option<u8> {
    let s = s.trim().to_ascii_lowercase();
    Some(match s.as_str() {
        "user" => 1,
        "daemon" => 3,
        "auth" => 4,
        "authpriv" => 10,
        _ => {
            let n: u8 = s.strip_prefix("local")?.parse().ok()?;
            if n > 7 {
                return None;
            }
            16 + n
        }
    })
}

/// 等級 → severity
pub fn severity(level: Level) -> u8 {
    match level {
        Level::Error => 3,
        Level::Warn => 4,
        Level::Info => 6,
        Level::Debug | Level::Trace => 7,
    }
}

/// `MSG` 的內容
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Payload {
    /// 訊息文字（與日誌檔的 `text` 相同，但不含時間與等級，由 syslog 標頭表示）
    #[default]
    Text,
    /// 與 `log.format = "json"` 相同的一行 JSON
    Json,
}

impl Payload {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Payload::Text),
            "json" => Some(Payload::Json),
            _ => None,
        }
    }
}

/// 標頭欄位：只能是可列印的 ASCII（33～126），最多 `max` 個字元；空的為 `-`
fn header_field(s: &str, max: usize) -> String {
    let v: String = s
        .chars()
        .map(|c| if c.is_ascii_graphic() { c } else { '_' })
        .take(max)
        .collect();
    if v.is_empty() { "-".to_string() } else { v }
}

/// 組成一則 RFC 5424 訊息（不含傳輸層的分隔）
#[allow(clippy::too_many_arguments)]
pub fn format_rfc5424(
    facility: u8,
    level: Level,
    ts: DateTime<Utc>,
    hostname: &str,
    app: &str,
    procid: u32,
    msgid: &str,
    msg: &str,
) -> String {
    format!(
        "<{}>1 {} {} {} {} {} - {}",
        u16::from(facility) * 8 + u16::from(severity(level)),
        ts.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
        header_field(hostname, 255),
        header_field(app, 48),
        procid,
        header_field(msgid, 32),
        msg
    )
}

/// 截斷到 `max` bytes 以內（在字元邊界）
pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 外送的計數（`/metrics`）
#[derive(Debug, Default)]
pub struct SyslogStats {
    pub sent: AtomicU64,
    /// 佇列已滿或無法送出而丟棄
    pub dropped: AtomicU64,
    /// 送出失敗（連線、寫入）的次數
    pub errors: AtomicU64,
}

/// 等待送出的數量（[`Sender::flush`]）
#[derive(Debug, Default)]
struct Pending {
    count: Mutex<u64>,
    drained: Condvar,
}

/// 外送的設定
#[derive(Debug, Clone)]
pub struct Config {
    pub transport: Transport,
    pub facility: u8,
    pub app_name: String,
    pub hostname: String,
    pub payload: Payload,
}

/// 外送的入口（可複製，所有複本共用同一個背景執行緒）
#[derive(Debug, Clone)]
pub struct Sender {
    tx: SyncSender<String>,
    pub stats: Arc<SyslogStats>,
    pending: Arc<Pending>,
    pub config: Arc<Config>,
}

impl Sender {
    /// 啟動背景執行緒。只檢查設定（不連線）：接收端暫時無法連線不影響服務啟動
    pub fn start(config: Config) -> io::Result<Self> {
        let (tx, rx) = sync_channel(QUEUE_CAPACITY);
        let stats = Arc::new(SyslogStats::default());
        let pending = Arc::new(Pending::default());
        let config = Arc::new(config);
        let worker = Worker {
            transport: config.transport.clone(),
            stats: stats.clone(),
            pending: pending.clone(),
            conn: None,
            retry_at: None,
            backoff: BACKOFF_MIN,
            failing: false,
        };
        std::thread::Builder::new()
            .name("kp-syslog".into())
            .spawn(move || worker.run(rx))?;
        Ok(Self {
            tx,
            stats,
            pending,
            config,
        })
    }

    /// 放入一則已組好的訊息；佇列滿時丟棄並計數
    pub fn send(&self, message: String) {
        {
            let mut n = self.pending.count.lock().unwrap_or_else(|e| e.into_inner());
            *n += 1;
        }
        match self.tx.try_send(message) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                self.pending.done();
            }
        }
    }

    /// 組成並放入一行日誌
    pub fn log(&self, level: Level, ts: DateTime<Utc>, msgid: &str, msg: &str) {
        let c = &self.config;
        self.send(format_rfc5424(
            c.facility,
            level,
            ts,
            &c.hostname,
            &c.app_name,
            std::process::id(),
            msgid,
            msg,
        ));
    }

    /// 等待佇列送完（結束前呼叫），最多 `timeout`；回傳是否已送完
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut n = self.pending.count.lock().unwrap_or_else(|e| e.into_inner());
        while *n > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            n = self
                .pending
                .drained
                .wait_timeout(n, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }
}

impl Pending {
    fn done(&self) {
        let mut n = self.count.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
        if *n == 0 {
            self.drained.notify_all();
        }
    }
}

enum Conn {
    Udp(UdpSocket),
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixDatagram),
}

struct Worker {
    transport: Transport,
    stats: Arc<SyslogStats>,
    pending: Arc<Pending>,
    conn: Option<Conn>,
    retry_at: Option<Instant>,
    backoff: Duration,
    /// 目前是否處於送不出去的狀態（只在狀態改變時寫一行 stderr，不經過 logger 以免遞迴）
    failing: bool,
}

impl Worker {
    fn run(mut self, rx: Receiver<String>) {
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(msg) => {
                    self.deliver(&msg);
                    self.pending.done();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn connect(&self) -> io::Result<Conn> {
        match &self.transport {
            Transport::Udp(addr) => {
                let target = addr
                    .to_socket_addrs()?
                    .next()
                    .ok_or_else(|| io::Error::other("無法解析位址"))?;
                let bind = if target.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                };
                let s = UdpSocket::bind(bind)?;
                s.connect(target)?;
                Ok(Conn::Udp(s))
            }
            Transport::Tcp(addr) => {
                let mut last = io::Error::other("無法解析位址");
                for a in addr.to_socket_addrs()? {
                    match TcpStream::connect_timeout(&a, IO_TIMEOUT) {
                        Ok(s) => {
                            s.set_write_timeout(Some(IO_TIMEOUT))?;
                            let _ = s.set_nodelay(true);
                            return Ok(Conn::Tcp(s));
                        }
                        Err(e) => last = e,
                    }
                }
                Err(last)
            }
            #[cfg(unix)]
            Transport::Unix(path) => {
                let s = std::os::unix::net::UnixDatagram::unbound()?;
                s.connect(path)?;
                s.set_write_timeout(Some(IO_TIMEOUT))?;
                Ok(Conn::Unix(s))
            }
            #[cfg(not(unix))]
            Transport::Unix(_) => Err(io::Error::other("這個平台不支援 Unix socket")),
        }
    }

    fn write(conn: &mut Conn, msg: &str) -> io::Result<()> {
        match conn {
            Conn::Udp(s) => s.send(truncate(msg, UDP_MAX_BYTES).as_bytes()).map(|_| ()),
            Conn::Tcp(s) => {
                let m = truncate(msg, STREAM_MAX_BYTES);
                s.write_all(format!("{} {m}", m.len()).as_bytes())
            }
            #[cfg(unix)]
            Conn::Unix(s) => s
                .send(truncate(msg, STREAM_MAX_BYTES).as_bytes())
                .map(|_| ()),
        }
    }

    fn deliver(&mut self, msg: &str) {
        if self.conn.is_none() {
            if self.retry_at.is_some_and(|t| Instant::now() < t) {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            match self.connect() {
                Ok(c) => self.conn = Some(c),
                Err(e) => {
                    self.fail(&e);
                    self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
        }
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        match Self::write(conn, msg) {
            Ok(()) => {
                self.stats.sent.fetch_add(1, Ordering::Relaxed);
                self.backoff = BACKOFF_MIN;
                self.retry_at = None;
                if std::mem::take(&mut self.failing) {
                    eprintln!("syslog 外送已恢復（{}）", self.transport);
                }
            }
            Err(e) => {
                self.conn = None;
                self.fail(&e);
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn fail(&mut self, e: &io::Error) {
        self.stats.errors.fetch_add(1, Ordering::Relaxed);
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
        if !std::mem::replace(&mut self.failing, true) {
            eprintln!(
                "syslog 外送失敗（{}）：{e}；之後的日誌在恢復前丟棄（日誌檔不受影響），{} 秒後重試",
                self.transport,
                self.backoff.as_secs()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::Read;

    fn ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 14, 6, 22, 16).unwrap() + chrono::Duration::milliseconds(123)
    }

    /// RFC 5424 的標頭語法檢查（測試用的最小解析器）
    fn parse_5424(s: &str) -> (u16, Vec<&str>, &str) {
        let rest = s.strip_prefix('<').unwrap();
        let (pri, rest) = rest.split_once('>').unwrap();
        let pri: u16 = pri.parse().unwrap();
        assert!(pri <= 191);
        let parts: Vec<&str> = rest.splitn(8, ' ').collect();
        assert_eq!(parts.len(), 8, "{s}");
        assert_eq!(parts[0], "1");
        DateTime::parse_from_rfc3339(parts[1]).unwrap();
        for (i, max) in [(2, 255), (3, 48), (4, 128), (5, 32)] {
            assert!(!parts[i].is_empty() && parts[i].len() <= max);
            assert!(
                parts[i].bytes().all(|b| (33..=126).contains(&b)),
                "{}",
                parts[i]
            );
        }
        assert_eq!(parts[6], "-");
        (pri, parts[..7].to_vec(), parts[7])
    }

    #[test]
    fn rfc5424_header_fields() {
        let s = format_rfc5424(
            3,
            Level::Warn,
            ts(),
            "kairos 01",
            "kairos",
            4151,
            "KRS-W3005",
            "警告 KRS-W3005：已暫時封鎖",
        );
        assert_eq!(
            s,
            "<28>1 2026-10-14T06:22:16.123Z kairos_01 kairos 4151 KRS-W3005 - 警告 KRS-W3005：已暫時封鎖"
        );
        let (pri, parts, msg) = parse_5424(&s);
        assert_eq!(pri, 28);
        assert_eq!(parts[2], "kairos_01");
        assert_eq!(msg, "警告 KRS-W3005：已暫時封鎖");
        let long = "a.".repeat(40);
        let s = format_rfc5424(16, Level::Error, ts(), "", "pulse", 1, &long, "x");
        let (pri, parts, _) = parse_5424(&s);
        assert_eq!(pri, 16 * 8 + 3);
        assert_eq!(parts[2], "-");
        assert_eq!(parts[5].len(), 32);
    }

    #[test]
    fn targets_and_facilities() {
        assert_eq!(
            parse_target("udp://10.0.0.5").unwrap(),
            Transport::Udp("10.0.0.5:514".into())
        );
        assert_eq!(
            parse_target("tcp://siem.example.com:6514").unwrap(),
            Transport::Tcp("siem.example.com:6514".into())
        );
        assert_eq!(
            parse_target("tcp://[2001:db8::1]").unwrap(),
            Transport::Tcp("[2001:db8::1]:601".into())
        );
        assert!(parse_target("tcp://2001:db8::1").is_err());
        assert!(parse_target("udp://host:0").is_err());
        assert!(parse_target("udp://").is_err());
        assert!(parse_target("http://x").is_err());
        assert!(parse_target("unix://dev/log").is_err());
        if cfg!(unix) {
            assert_eq!(
                parse_target("unix:///dev/log").unwrap(),
                Transport::Unix("/dev/log".into())
            );
        }
        assert_eq!(parse_facility("daemon"), Some(3));
        assert_eq!(parse_facility("LOCAL7"), Some(23));
        assert_eq!(parse_facility("local8"), None);
        assert_eq!(parse_facility("kern"), None);
        assert_eq!(severity(Level::Trace), 7);
        assert_eq!(truncate("中文字", 4), "中");
    }

    fn config(t: Transport) -> Config {
        Config {
            transport: t,
            facility: 3,
            app_name: "kairos".into(),
            hostname: "h".into(),
            payload: Payload::Text,
        }
    }

    #[test]
    fn udp_delivery() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let addr = rx.local_addr().unwrap();
        let s = Sender::start(config(Transport::Udp(addr.to_string()))).unwrap();
        s.log(Level::Info, ts(), "agent.start", "啟動");
        assert!(s.flush(Duration::from_secs(5)));
        let mut buf = [0u8; 2048];
        let n = rx.recv(&mut buf).unwrap();
        let got = std::str::from_utf8(&buf[..n]).unwrap();
        let (pri, parts, msg) = parse_5424(got);
        assert_eq!(pri, 30);
        assert_eq!(parts[5], "agent.start");
        assert_eq!(msg, "啟動");
        assert_eq!(s.stats.sent.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn tcp_delivery_uses_octet_counting() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let s = Sender::start(config(Transport::Tcp(addr.to_string()))).unwrap();
        s.log(Level::Error, ts(), "KRS-E1205", "第一則");
        s.log(Level::Warn, ts(), "-", "第二則 含 空白");
        let (mut c, _) = l.accept().unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert!(s.flush(Duration::from_secs(5)));
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while got.len() < 20 || !String::from_utf8_lossy(&got).contains("第二則 含 空白") {
            let n = c.read(&mut buf).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        // octet counting：「長度 空白 訊息」重複
        let text = String::from_utf8(got).unwrap();
        let mut rest = text.as_str();
        let mut msgs = Vec::new();
        while !rest.is_empty() {
            let (len, after) = rest.split_once(' ').unwrap();
            let len: usize = len.parse().unwrap();
            msgs.push(&after[..len]);
            rest = &after[len..];
        }
        assert_eq!(msgs.len(), 2);
        assert_eq!(parse_5424(msgs[0]).2, "第一則");
        assert_eq!(parse_5424(msgs[1]).0, 28);
    }

    #[test]
    fn unreachable_tcp_drops_without_blocking() {
        // 綁定後立即關閉：連線被拒
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let s = Sender::start(config(Transport::Tcp(addr.to_string()))).unwrap();
        let t0 = Instant::now();
        for _ in 0..100 {
            s.log(Level::Info, ts(), "-", "x");
        }
        assert!(t0.elapsed() < Duration::from_secs(1), "送出不可阻塞");
        assert!(s.flush(Duration::from_secs(10)));
        assert_eq!(s.stats.dropped.load(Ordering::Relaxed), 100);
        assert!(s.stats.errors.load(Ordering::Relaxed) >= 1);
    }

    #[cfg(unix)]
    #[test]
    fn unix_datagram_delivery() {
        let dir = std::env::temp_dir().join(format!("kp-obs-syslog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.sock");
        let _ = std::fs::remove_file(&path);
        let rx = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let s = Sender::start(config(Transport::Unix(path.clone()))).unwrap();
        s.log(Level::Debug, ts(), "-", "本機");
        assert!(s.flush(Duration::from_secs(5)));
        let mut buf = [0u8; 2048];
        let n = rx.recv(&mut buf).unwrap();
        let (pri, _, msg) = parse_5424(std::str::from_utf8(&buf[..n]).unwrap());
        assert_eq!((pri, msg), (31, "本機"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
