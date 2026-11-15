//! 通知的傳送：Webhook（HTTP/1.1 over TLS，hyper 的 client conn）與 Email（SMTP，STARTTLS／TLS）。
//!
//! - TLS：rustls（ring），信任作業系統的 CA（`rustls-native-certs`），管道可以另外指定 `ca_file`
//!   （內部郵件伺服器或 SIEM 使用私有 CA 時；S11）。不提供「略過憑證驗證」。
//! - 逾時：連線、TLS 握手、整個請求都受管道的 `timeout_secs`（預設 10 秒，1～60）限制。
//! - 錯誤對應到產品代碼（`copywriting.md` §3.3 的 51xx）；錯誤訊息在寫入日誌與發送紀錄前以
//!   `secrets::scrub` 移除機密。

use hyper::body::{Body, Bytes, Incoming};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use ring::hmac;
use serde_json::Value;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

use super::render::{Message, View, webhook_body};
use crate::alerts::model::{Channel, ChannelKind, Severity};
use crate::alerts::secrets::scrub;
use crate::api::http::ApiBody;

/// 無法連線到 SMTP 伺服器
pub const CODE_SMTP_CONNECT: &str = "KRS-E5101";
/// SMTP 驗證失敗
pub const CODE_SMTP_AUTH: &str = "KRS-E5102";
/// 通知目的地的伺服器憑證不受信任
pub const CODE_TLS_UNTRUSTED: &str = "KRS-E5103";
/// 無法連線到外部網路（LINE、Teams）
pub const CODE_EXTERNAL_NETWORK: &str = "KRS-E5104";
/// LINE channel access token 無效或已撤銷
pub const CODE_LINE_TOKEN: &str = "KRS-E5105";
/// Teams Webhook 回應 4xx
pub const CODE_TEAMS_4XX: &str = "KRS-E5106";
/// Webhook 目的地無法連線或回應不是 2xx
pub const CODE_WEBHOOK: &str = "KRS-E5108";
/// SMTP 伺服器拒絕寄件者、收件者或內容（設計包 `decisions/INTEGRATION.md` §28.3）
pub const CODE_SMTP_REJECTED: &str = "KRS-E5109";
/// 通知管道的設定無效（缺少欄位、網址格式錯誤；設計包 §28.3）
pub const CODE_CHANNEL_CONFIG: &str = "KRS-E5110";

/// LINE Messaging API 的 push 端點（`url` 未設定時）
pub const LINE_PUSH_URL: &str = "https://api.line.me/v2/bot/message/push";
/// 回應本文最多讀取的長度（錯誤訊息用）
const MAX_RESPONSE: usize = 4096;

/// 一次發送的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub ok: bool,
    pub code: Option<&'static str>,
    pub message: String,
    pub response_code: Option<u16>,
    /// 對方要求的退避（429 的 `Retry-After`）
    pub retry_after_secs: Option<u64>,
    pub elapsed_ms: u64,
}

impl Outcome {
    fn ok(message: impl Into<String>, response_code: Option<u16>, started: Instant) -> Self {
        Self {
            ok: true,
            code: None,
            message: message.into(),
            response_code,
            retry_after_secs: None,
            elapsed_ms: started.elapsed().as_millis() as u64,
        }
    }

    fn fail(code: &'static str, message: impl Into<String>, started: Instant) -> Self {
        Self {
            ok: false,
            code: Some(code),
            message: message.into(),
            response_code: None,
            retry_after_secs: None,
            elapsed_ms: started.elapsed().as_millis() as u64,
        }
    }
}

fn native_roots() -> &'static Vec<CertificateDer<'static>> {
    static ROOTS: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let loaded = rustls_native_certs::load_native_certs();
        if !loaded.errors.is_empty() {
            log::debug!("讀取作業系統的 CA 時有 {} 個錯誤", loaded.errors.len());
        }
        loaded.certs
    })
}

/// 用戶端的 TLS 設定：作業系統的 CA ＋ 管道指定的 `ca_file`
pub fn client_config(ca_file: Option<&str>) -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    for c in native_roots() {
        let _ = roots.add(c.clone());
    }
    if let Some(path) = ca_file.map(str::trim).filter(|p| !p.is_empty()) {
        let certs = CertificateDer::pem_file_iter(path)
            .and_then(|it| it.collect::<Result<Vec<_>, _>>())
            .map_err(|e| format!("無法讀取 ca_file {path}: {e}"))?;
        if certs.is_empty() {
            return Err(format!("ca_file {path} 中沒有憑證"));
        }
        for c in certs {
            roots
                .add(c)
                .map_err(|e| format!("ca_file {path} 的憑證無法使用: {e}"))?;
        }
    }
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
    Ok(Arc::new(config))
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// TLS 錯誤是否為憑證不受信任
fn untrusted(e: &std::io::Error) -> bool {
    let t = e.to_string();
    t.contains("UnknownIssuer")
        || t.contains("invalid peer certificate")
        || t.contains("NotValidForName")
        || t.contains("certificate")
}

/// 解析後的 URL
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// 解析 `https://host[:port]/path?query`（`allow_http` 時也接受 `http://`）
pub fn parse_url(url: &str, allow_http: bool) -> Result<Target, String> {
    let url = url.trim();
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        if !allow_http {
            return Err("Webhook 網址必須是 https://（內部閘道需要 http 時在管道設定 allow_http = true；網址中的 token 會以明文傳送）".into());
        }
        (false, r)
    } else {
        return Err("Webhook 網址必須以 https:// 開頭".into());
    };
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(split);
    if authority.contains('@') {
        return Err("Webhook 網址不可含帳號密碼（請改用自訂標頭）".into());
    }
    let (host, port) = if let Some(h) = authority.strip_prefix('[') {
        let (h, after) = h.split_once(']').ok_or("網址的 IPv6 位址格式不正確")?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| "網址的連接埠不正確")?,
            None => {
                if https {
                    443
                } else {
                    80
                }
            }
        };
        (h.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|_| "網址的連接埠不正確")?),
            None => (authority.to_string(), if https { 443 } else { 80 }),
        }
    };
    if host.is_empty() {
        return Err("網址沒有主機名稱".into());
    }
    let path = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    Ok(Target {
        https,
        host,
        port,
        path,
    })
}

fn timeout_of(channel: &Channel) -> Duration {
    let secs = channel
        .config
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 60);
    Duration::from_secs(secs)
}

async fn connect(
    target: &Target,
    ca_file: Option<&str>,
    timeout: Duration,
) -> Result<Box<dyn Io>, (bool, std::io::Error)> {
    let tcp = tokio::time::timeout(
        timeout,
        TcpStream::connect((target.host.as_str(), target.port)),
    )
    .await
    .map_err(|_| {
        (
            false,
            std::io::Error::new(std::io::ErrorKind::TimedOut, "連線逾時"),
        )
    })?
    .map_err(|e| (false, e))?;
    let _ = tcp.set_nodelay(true);
    if !target.https {
        return Ok(Box::new(tcp));
    }
    let config = client_config(ca_file).map_err(|e| {
        (
            false,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, e),
        )
    })?;
    let name = ServerName::try_from(target.host.clone()).map_err(|e| {
        (
            false,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()),
        )
    })?;
    let tls = tokio::time::timeout(timeout, TlsConnector::from(config).connect(name, tcp))
        .await
        .map_err(|_| {
            (
                true,
                std::io::Error::new(std::io::ErrorKind::TimedOut, "TLS 握手逾時"),
            )
        })?
        .map_err(|e| (true, e))?;
    Ok(Box::new(tls))
}

/// Webhook 的格式與目的地
fn webhook_target(channel: &Channel) -> Result<(String, String), String> {
    let format = channel
        .config
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("kairos")
        .to_string();
    let url = channel
        .secrets
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|u| !u.is_empty())
        .or_else(|| (format == "line").then(|| LINE_PUSH_URL.to_string()))
        .ok_or("Webhook 管道沒有設定網址")?;
    Ok((format, url))
}

/// 送出 Webhook
pub async fn send_webhook(channel: &Channel, body: &Value) -> Outcome {
    let started = Instant::now();
    let (format, url) = match webhook_target(channel) {
        Ok(x) => x,
        Err(e) => return Outcome::fail(CODE_CHANNEL_CONFIG, e, started),
    };
    let external = matches!(format.as_str(), "line" | "teams");
    let allow_http = channel
        .config
        .get("allow_http")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let target = match parse_url(&url, allow_http) {
        Ok(t) => t,
        Err(e) => return Outcome::fail(CODE_CHANNEL_CONFIG, e, started),
    };
    let timeout = timeout_of(channel);
    let ca = channel.config.get("ca_file").and_then(Value::as_str);
    let result = tokio::time::timeout(timeout * 2, async {
        let io = connect(&target, ca, timeout).await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
            .await
            .map_err(|e| (false, std::io::Error::other(e.to_string())))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        let method = match channel.config.get("method").and_then(Value::as_str) {
            Some("PUT") => Method::PUT,
            _ => Method::POST,
        };
        let host_header =
            if (target.https && target.port == 443) || (!target.https && target.port == 80) {
                target.host.clone()
            } else {
                format!("{}:{}", target.host, target.port)
            };
        let mut req = Request::builder()
            .method(method)
            .uri(&target.path)
            .header(hyper::header::HOST, host_header)
            .header(
                hyper::header::CONTENT_TYPE,
                "application/json; charset=utf-8",
            )
            .header(
                hyper::header::USER_AGENT,
                format!("Kairos/{}", env!("CARGO_PKG_VERSION")),
            )
            .header(hyper::header::CONTENT_LENGTH, bytes.len());
        if let Some(secret) = channel
            .secrets
            .get("hmac_secret")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
            let tag = hmac::sign(&key, &bytes);
            let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
            req = req.header("x-kairos-signature", format!("sha256={hex}"));
        }
        if format == "line"
            && let Some(token) = channel.secrets.get("line_token").and_then(Value::as_str)
        {
            req = req.header(hyper::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(Value::Object(h)) = channel.secrets.get("headers") {
            for (k, v) in h {
                if let Some(v) = v.as_str() {
                    req = req.header(k.as_str(), v);
                }
            }
        }
        let req = req
            .body(ApiBody::Full(Some(Bytes::from(bytes))))
            .map_err(|e| (false, std::io::Error::other(e.to_string())))?;
        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| (false, std::io::Error::other(e.to_string())))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get(hyper::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let text = read_limited(resp.into_body()).await;
        Ok::<_, (bool, std::io::Error)>((status, retry_after, text))
    })
    .await;
    let scrubbed = |t: &str| scrub(t, &channel.secrets);
    match result {
        Err(_) => Outcome::fail(
            if external {
                CODE_EXTERNAL_NETWORK
            } else {
                CODE_WEBHOOK
            },
            "請求逾時",
            started,
        ),
        Ok(Err((tls, e))) => {
            let code = if tls && untrusted(&e) {
                CODE_TLS_UNTRUSTED
            } else if external {
                CODE_EXTERNAL_NETWORK
            } else {
                CODE_WEBHOOK
            };
            Outcome::fail(
                code,
                scrubbed(&format!("無法連線到 {}：{e}", target.host)),
                started,
            )
        }
        Ok(Ok((status, retry, text))) => {
            if (200..300).contains(&status) {
                return Outcome::ok(format!("HTTP {status}"), Some(status), started);
            }
            let code = match (format.as_str(), status) {
                ("line", 401 | 403) => CODE_LINE_TOKEN,
                ("teams", 400..=499) => CODE_TEAMS_4XX,
                _ => CODE_WEBHOOK,
            };
            let mut o = Outcome::fail(
                code,
                scrubbed(&format!(
                    "{} 回應 HTTP {status}{}",
                    target.host,
                    if text.is_empty() {
                        String::new()
                    } else {
                        format!("：{}", text.chars().take(300).collect::<String>())
                    }
                )),
                started,
            );
            o.response_code = Some(status);
            o.retry_after_secs = retry.filter(|_| status == 429 || status == 503);
            o
        }
    }
}

async fn read_limited(mut body: Incoming) -> String {
    let mut buf = Vec::new();
    loop {
        let frame = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await;
        match frame {
            Some(Ok(f)) => {
                if let Ok(d) = f.into_data() {
                    buf.extend_from_slice(&d);
                    if buf.len() >= MAX_RESPONSE {
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    buf.truncate(MAX_RESPONSE);
    String::from_utf8_lossy(&buf).replace(['\r', '\n'], " ")
}

// ---------------------------------------------------------------------------
// SMTP
// ---------------------------------------------------------------------------

/// RFC 2047 的 encoded-word（UTF-8、base64）：主旨含中文
pub fn encode_header(text: &str) -> String {
    if text.is_ascii() {
        return text.to_string();
    }
    // 每個 encoded-word 最多 75 個字元：以字元為單位切開（不切斷 UTF-8）
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in text.chars() {
        if chunk.len() + c.len_utf8() > 42 {
            words.push(format!("=?UTF-8?B?{}?=", base64(chunk.as_bytes())));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", base64(chunk.as_bytes())));
    }
    words.join("\r\n ")
}

/// 標準 base64（含 `=`）
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// 位址是否合理（不含 CR、LF、`<`、`>`，有 `@`）：避免標頭注入
pub fn valid_address(a: &str) -> bool {
    let a = a.trim();
    a.len() <= 254
        && a.contains('@')
        && !a.starts_with('@')
        && !a.ends_with('@')
        && !a.chars().any(|c| {
            c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | '"')
        })
}

/// `EHLO` 的參數是否合理（安全稽核 E4）：主機名稱或位址字面值——只有 ASCII 英數字與 `.`、`-`、`:`、
/// `[`、`]`，1～255 個字元。這個值原樣寫進 SMTP 命令列，不可含 CR、LF、空白或其他控制字元
pub fn valid_helo(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 255
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
}

/// 組成郵件（純文字、UTF-8、base64；緊急另加 `Importance: high`、`X-Priority: 1`，§2.1.1）
pub fn compose_mail(from: &str, to: &[String], msg: &Message, now_ms: i64, id: &str) -> String {
    let date = chrono::DateTime::from_timestamp_millis(now_ms)
        .unwrap_or_default()
        .to_rfc2822();
    let domain = from.rsplit('@').next().unwrap_or("kairos");
    let mut out = String::new();
    out.push_str(&format!("From: {from}\r\n"));
    out.push_str(&format!("To: {}\r\n", to.join(", ")));
    out.push_str(&format!("Subject: {}\r\n", encode_header(&msg.subject)));
    out.push_str(&format!("Date: {date}\r\n"));
    out.push_str(&format!("Message-ID: <{id}@{domain}>\r\n"));
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    out.push_str("Content-Transfer-Encoding: base64\r\n");
    out.push_str("Auto-Submitted: auto-generated\r\n");
    if msg.severity == Severity::Emergency {
        out.push_str("Importance: high\r\nX-Priority: 1\r\n");
    }
    out.push_str("\r\n");
    let body = base64(msg.text.replace('\n', "\r\n").as_bytes());
    for line in body.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(line).unwrap_or(""));
        out.push_str("\r\n");
    }
    out
}

struct Smtp {
    reader: BufReader<Box<dyn Io>>,
}

impl Smtp {
    /// 讀一個回應（多行時讀到 `NNN ` 為止）；回傳（代碼, 全文）
    async fn reply(&mut self) -> Result<(u16, String), String> {
        let mut text = String::new();
        loop {
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .await
                .map_err(|e| format!("讀取 SMTP 回應失敗：{e}"))?;
            if n == 0 {
                return Err("SMTP 伺服器關閉了連線".into());
            }
            if line.len() > 4096 || text.len() > 64 * 1024 {
                return Err("SMTP 回應過長".into());
            }
            let code: u16 = line
                .get(..3)
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| format!("SMTP 回應格式不正確：{}", line.trim()))?;
            text.push_str(line.get(4..).unwrap_or("").trim_end());
            text.push('\n');
            if line.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, text));
            }
        }
    }

    async fn cmd(&mut self, line: &str) -> Result<(u16, String), String> {
        let w = self.reader.get_mut();
        w.write_all(line.as_bytes())
            .await
            .and(w.write_all(b"\r\n").await)
            .map_err(|e| format!("寫入 SMTP 失敗：{e}"))?;
        w.flush()
            .await
            .map_err(|e| format!("寫入 SMTP 失敗：{e}"))?;
        self.reply().await
    }

    fn into_inner(self) -> Box<dyn Io> {
        self.reader.into_inner()
    }
}

/// SMTP 的設定
struct SmtpConfig {
    host: String,
    port: u16,
    /// `starttls`（預設）、`tls`、`none`
    security: String,
    username: Option<String>,
    password: Option<String>,
    from: String,
    to: Vec<String>,
    helo: String,
}

fn smtp_config(channel: &Channel) -> Result<SmtpConfig, String> {
    let c = &channel.config;
    let get = |k: &str| {
        c.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let host = get("host").ok_or("沒有設定 SMTP 主機（host）")?.to_string();
    let security = get("security").unwrap_or("starttls").to_string();
    if !matches!(security.as_str(), "starttls" | "tls" | "none") {
        return Err("security 必須是 starttls、tls 或 none".into());
    }
    let port = c
        .get("port")
        .and_then(Value::as_u64)
        .map(|p| p as u16)
        .unwrap_or(if security == "tls" { 465 } else { 587 });
    let from = get("from").ok_or("沒有設定寄件者（from）")?.to_string();
    if !valid_address(&from) {
        return Err(format!("寄件者 {from} 的格式不正確"));
    }
    let to: Vec<String> = c
        .get("to")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .collect()
        })
        .unwrap_or_default();
    if to.is_empty() {
        return Err("沒有設定收件者（to）".into());
    }
    if let Some(bad) = to.iter().find(|a| !valid_address(a)) {
        return Err(format!("收件者 {bad} 的格式不正確"));
    }
    let username = get("username").map(str::to_string);
    let password = channel
        .secrets
        .get("password")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    if security == "none" && password.is_some() {
        return Err("security = none 時不可設定密碼（密碼會以明文傳送）".into());
    }
    // 安全稽核 E4：API 已在儲存時驗證；這裡再擋一次（較舊版本存下的管道、直接改資料庫的值）
    let helo = get("helo").unwrap_or("kairos");
    if !valid_helo(helo) {
        return Err("helo 必須是主機名稱或位址（不可含空白、換行或其他符號）".into());
    }
    let helo = helo.to_string();
    Ok(SmtpConfig {
        host,
        port,
        security,
        username,
        password,
        from,
        to,
        helo,
    })
}

/// 寄出 Email
pub async fn send_email(
    channel: &Channel,
    msg: &Message,
    now_ms: i64,
    message_id: &str,
) -> Outcome {
    let started = Instant::now();
    let cfg = match smtp_config(channel) {
        Ok(c) => c,
        Err(e) => return Outcome::fail(CODE_CHANNEL_CONFIG, e, started),
    };
    let timeout = timeout_of(channel);
    let ca = channel
        .config
        .get("ca_file")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mail = compose_mail(&cfg.from, &cfg.to, msg, now_ms, message_id);
    let r = tokio::time::timeout(timeout * 3, smtp_session(cfg, ca, timeout, mail)).await;
    match r {
        Err(_) => Outcome::fail(CODE_SMTP_CONNECT, "SMTP 工作階段逾時", started),
        Ok(Err((code, e))) => Outcome::fail(code, scrub(&e, &channel.secrets), started),
        Ok(Ok(text)) => Outcome::ok(text, Some(250), started),
    }
}

async fn smtp_session(
    cfg: SmtpConfig,
    ca: Option<String>,
    timeout: Duration,
    mail: String,
) -> Result<String, (&'static str, String)> {
    let target = Target {
        https: cfg.security == "tls",
        host: cfg.host.clone(),
        port: cfg.port,
        path: String::new(),
    };
    let io = connect(&target, ca.as_deref(), timeout)
        .await
        .map_err(|(tls, e)| {
            if tls && untrusted(&e) {
                (
                    CODE_TLS_UNTRUSTED,
                    format!("{} 的憑證不受信任：{e}", cfg.host),
                )
            } else {
                (
                    CODE_SMTP_CONNECT,
                    format!("無法連線到 {}:{}：{e}", cfg.host, cfg.port),
                )
            }
        })?;
    let conn_err = |e: String| (CODE_SMTP_CONNECT, e);
    let mut s = Smtp {
        reader: BufReader::new(io),
    };
    let (code, text) = s.reply().await.map_err(conn_err)?;
    if code != 220 {
        return Err((
            CODE_SMTP_CONNECT,
            format!("SMTP 伺服器拒絕連線：{code} {}", text.trim()),
        ));
    }
    let (code, mut caps) = s
        .cmd(&format!("EHLO {}", cfg.helo))
        .await
        .map_err(conn_err)?;
    if code != 250 {
        return Err((
            CODE_SMTP_CONNECT,
            format!("EHLO 失敗：{code} {}", text.trim()),
        ));
    }
    if cfg.security == "starttls" {
        if !caps
            .lines()
            .any(|l| l.trim().eq_ignore_ascii_case("STARTTLS"))
        {
            return Err((
                CODE_SMTP_CONNECT,
                format!(
                    "{} 不支援 STARTTLS（可改用 security = \"tls\" 與 465 埠）",
                    cfg.host
                ),
            ));
        }
        let (code, text) = s.cmd("STARTTLS").await.map_err(conn_err)?;
        if code != 220 {
            return Err((
                CODE_SMTP_CONNECT,
                format!("STARTTLS 失敗：{code} {}", text.trim()),
            ));
        }
        let plain = s.into_inner();
        let config = client_config(ca.as_deref()).map_err(|e| (CODE_CHANNEL_CONFIG, e))?;
        let name = ServerName::try_from(cfg.host.clone())
            .map_err(|e| (CODE_CHANNEL_CONFIG, e.to_string()))?;
        let tls = tokio::time::timeout(timeout, TlsConnector::from(config).connect(name, plain))
            .await
            .map_err(|_| (CODE_SMTP_CONNECT, "STARTTLS 的 TLS 握手逾時".to_string()))?
            .map_err(|e| {
                if untrusted(&e) {
                    (
                        CODE_TLS_UNTRUSTED,
                        format!("{} 的憑證不受信任：{e}", cfg.host),
                    )
                } else {
                    (CODE_SMTP_CONNECT, format!("STARTTLS 失敗：{e}"))
                }
            })?;
        s = Smtp {
            reader: BufReader::new(Box::new(tls)),
        };
        let (code, c2) = s
            .cmd(&format!("EHLO {}", cfg.helo))
            .await
            .map_err(conn_err)?;
        if code != 250 {
            return Err((CODE_SMTP_CONNECT, format!("EHLO 失敗：{code}")));
        }
        caps = c2;
    }
    if let (Some(user), Some(pass)) = (&cfg.username, &cfg.password) {
        let auth_line = caps
            .lines()
            .find(|l| l.to_ascii_uppercase().starts_with("AUTH"))
            .unwrap_or("")
            .to_ascii_uppercase();
        let (code, text) = if auth_line.contains("PLAIN") || !auth_line.contains("LOGIN") {
            let token = base64(format!("\0{user}\0{pass}").as_bytes());
            s.cmd(&format!("AUTH PLAIN {token}"))
                .await
                .map_err(conn_err)?
        } else {
            let (c, _) = s.cmd("AUTH LOGIN").await.map_err(conn_err)?;
            if c != 334 {
                return Err((CODE_SMTP_AUTH, format!("AUTH LOGIN 失敗：{c}")));
            }
            let (c, _) = s.cmd(&base64(user.as_bytes())).await.map_err(conn_err)?;
            if c != 334 {
                return Err((CODE_SMTP_AUTH, format!("AUTH LOGIN 失敗：{c}")));
            }
            s.cmd(&base64(pass.as_bytes())).await.map_err(conn_err)?
        };
        if code != 235 {
            return Err((
                CODE_SMTP_AUTH,
                format!("SMTP 驗證失敗：{code} {}", text.trim()),
            ));
        }
    }
    let reject = |what: &str, code: u16, text: &str| {
        (
            CODE_SMTP_REJECTED,
            format!("{what}被拒：{code} {}", text.trim()),
        )
    };
    let (code, text) = s
        .cmd(&format!("MAIL FROM:<{}>", cfg.from))
        .await
        .map_err(conn_err)?;
    if code != 250 {
        return Err(reject("寄件者", code, &text));
    }
    for rcpt in &cfg.to {
        let (code, text) = s
            .cmd(&format!("RCPT TO:<{rcpt}>"))
            .await
            .map_err(conn_err)?;
        if code != 250 && code != 251 {
            return Err(reject(&format!("收件者 {rcpt} "), code, &text));
        }
    }
    let (code, text) = s.cmd("DATA").await.map_err(conn_err)?;
    if code != 354 {
        return Err(reject("DATA ", code, &text));
    }
    // 點填充（行首的 `.` 加倍）；本文是 base64，只有標頭可能以 `.` 開頭
    let mut data = String::with_capacity(mail.len() + 8);
    for line in mail.split("\r\n") {
        if line.starts_with('.') {
            data.push('.');
        }
        data.push_str(line);
        data.push_str("\r\n");
    }
    while data.ends_with("\r\n\r\n") {
        data.truncate(data.len() - 2);
    }
    data.push('.');
    let (code, text) = s.cmd(&data).await.map_err(conn_err)?;
    if code != 250 {
        return Err(reject("郵件內容", code, &text));
    }
    let _ = s.cmd("QUIT").await;
    Ok(format!("已送達 {}（{code} {}）", cfg.host, text.trim()))
}

/// 依管道種類送出一則訊息
pub async fn send(
    channel: &Channel,
    payload: &Value,
    view: &View,
    now_ms: i64,
    message_id: &str,
) -> Outcome {
    // 機密無法解開：不連線，直接以 KRS-E5110 失敗（不要以空的 URL、密碼嘗試發送）
    if channel.secrets_unavailable() {
        return Outcome::fail(
            CODE_CHANNEL_CONFIG,
            crate::alerts::store::SECRETS_UNAVAILABLE_MESSAGE,
            Instant::now(),
        );
    }
    match channel.kind {
        ChannelKind::Email => {
            let msg = super::render::message(payload, view, now_ms);
            send_email(channel, &msg, now_ms, message_id).await
        }
        ChannelKind::Webhook => {
            let format = channel
                .config
                .get("format")
                .and_then(Value::as_str)
                .unwrap_or("kairos");
            let line_to = channel
                .config
                .get("line_to")
                .and_then(Value::as_str)
                .unwrap_or("");
            let body = webhook_body(format, payload, view, line_to, now_ms);
            send_webhook(channel, &body).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_parsed_and_http_is_refused_by_default() {
        let t = parse_url("https://hooks.example.com/services/T0/B1/x?y=1", false).unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("hooks.example.com", 443, "/services/T0/B1/x?y=1")
        );
        let t = parse_url("https://[2001:db8::1]:8443", false).unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("2001:db8::1", 8443, "/")
        );
        assert!(parse_url("http://gw.local/x", false).is_err());
        assert!(parse_url("http://gw.local/x", true).is_ok());
        assert!(parse_url("https://u:p@h/x", false).is_err());
        assert!(parse_url("ftp://h/x", false).is_err());
        assert!(parse_url("https://h:99999/", false).is_err());
    }

    #[test]
    fn mail_is_encoded_and_emergency_has_priority_headers() {
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"\0u\0p"), "AHUAcA==");
        let h = encode_header("【緊急】網段事件");
        assert!(h.starts_with("=?UTF-8?B?") && h.ends_with("?="));
        assert_eq!(encode_header("plain"), "plain");
        let msg = Message {
            subject: "【緊急】測試".into(),
            text: "第一行\n第二行".into(),
            severity: Severity::Emergency,
        };
        let mail = compose_mail(
            "kairos@example.com",
            &["a@example.com".into()],
            &msg,
            0,
            "x1",
        );
        assert!(mail.contains("Importance: high\r\nX-Priority: 1\r\n"));
        assert!(mail.contains("Content-Transfer-Encoding: base64"));
        assert!(!mail.contains("第一行"));
        let normal = Message {
            severity: Severity::Critical,
            ..msg
        };
        assert!(
            !compose_mail("k@e.com", &["a@e.com".into()], &normal, 0, "x2").contains("X-Priority")
        );
        assert!(valid_address("ops@example.com"));
        for bad in ["a", "a@", "@b", "a b@c", "a@b\r\nBcc: x@y", "<a@b>"] {
            assert!(!valid_address(bad), "{bad}");
        }
    }

    /// 安全稽核 E4：helo 原樣寫進 `EHLO` 命令列——含換行、空白或其他符號的值在送出前就被拒絕
    /// （含較舊版本存下、沒有經過 API 驗證的管道）
    #[test]
    fn helo_with_line_breaks_or_spaces_is_rejected_before_sending() {
        for ok in ["kairos", "monitor.example.com", "[192.0.2.1]", "[IPv6:2001:db8::1]"] {
            assert!(valid_helo(ok), "{ok}");
        }
        let long = "a".repeat(256);
        for bad in [
            "",
            "a b",
            "a\r\nMAIL FROM:<x@y>",
            "a\nb",
            "a\tb",
            "a\0b",
            "名稱",
            "a;b",
            long.as_str(),
        ] {
            assert!(!valid_helo(bad), "{bad:?}");
        }
        let channel = |helo: &str| Channel {
            channel_id: "c1".into(),
            name: "mail".into(),
            kind: crate::alerts::model::ChannelKind::Email,
            enabled: true,
            config: serde_json::json!({
                "host": "smtp.example.com", "from": "k@example.com",
                "to": ["ops@example.com"], "helo": helo,
            }),
            secrets: serde_json::json!({}),
            state: "ok".into(),
            consecutive_failures: 0,
            failed_since: None,
            last_attempt_at: None,
            last_success_at: None,
            last_error_code: None,
            last_error: None,
            created_at: 0,
            created_by: String::new(),
            updated_at: 0,
            updated_by: String::new(),
        };
        assert_eq!(
            smtp_config(&channel("mon.example.com")).unwrap().helo,
            "mon.example.com"
        );
        assert!(smtp_config(&channel("x\r\nRSET")).is_err());
        assert!(smtp_config(&channel("x y")).is_err());
    }
}
