//! 8444 註冊通道的用戶端（ENROLL.md 第 2～8 節）：連線、信任驗證、逾時、keep-alive，以及把回應
//! 轉成型別（成功本文或 [`ApiError`]）。
//!
//! - 逾時（ENROLL.md 第 2 節）：TCP 5 秒、TLS 10 秒、等待回應 15 秒（長輪詢為 `wait` ＋ 15 秒）。
//! - 同一條連線依序送多個請求（`server-info` → `token-check` → 新註冊，設計包 D7）；連線被伺服器關閉後，
//!   下一個請求自動重新連線。重用的連線在送出前就被關閉（keep-alive 逾時）時自動重送一次：
//!   註冊通道的請求都是冪等的（ENROLL.md 第 4 節）。
//! - 每個回應（含錯誤）都帶 `server_time`：以它量測時鐘偏差（本機收到回應的時間 − `server_time`）。

use std::io;
use std::sync::Arc;
use std::time::Duration;

use kairos_proto::enroll::{
    self as ke, EnrollErrorCode, EnrollRequest, EnrollResult, ErrorBody, ErrorResponse,
    GraceRenewRequest, Route, ServerInfo, TokenCheckRequest, TokenCheckResponse, WithdrawChallenge,
    WithdrawRequest, WithdrawResult,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;

use super::http::{HttpConn, Response};
use super::trust::{self, EnrollVerifier, Observed, TrustSpec};
use crate::codes;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const TLS_TIMEOUT: Duration = Duration::from_secs(10);
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

/// 本機牆鐘時間（Unix 毫秒）。測試以它模擬本機時鐘偏差
pub type NowMs = Arc<dyn Fn() -> i64 + Send + Sync>;

/// 系統時鐘
pub fn system_now() -> NowMs {
    Arc::new(|| chrono::Utc::now().timestamp_millis())
}

/// 呼叫 8444 失敗的原因
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// 連不上或連線中斷（`PLS-E2002`～`E2005`、`E2201`）；`refused` 為 TCP 連線被拒
    Network {
        code: &'static str,
        detail: String,
        refused: bool,
    },
    /// 伺服器身分驗證失敗（`PLS-E1401`～`E1406`）。`E1403` 時另外取得的未驗證伺服器時間
    Trust {
        code: &'static str,
        detail: String,
        unverified_server_time: Option<String>,
    },
    /// 404／405 且本文不是本規格的錯誤格式：Kairos 不支援自動註冊（`PLS-E2106`）
    Unsupported { status: u16 },
    /// 回應不合預期（`PLS-W2107`）：本文無法解析、狀態碼與 `error.code` 對不上、不認得的錯誤碼
    Unexpected {
        status: Option<u16>,
        protocol_code: Option<String>,
        detail: String,
    },
    /// 本規格的錯誤回應
    Server(ServerError),
}

/// 本規格的錯誤回應
#[derive(Debug, Clone, PartialEq)]
pub struct ServerError {
    pub status: u16,
    pub code: EnrollErrorCode,
    pub body: ErrorBody,
    pub retry_after: Option<Duration>,
    pub server_time: String,
}

impl ServerError {
    /// `Retry-After` 與 `error.retry_after_ms` 中較長者
    pub fn wait_hint(&self) -> Option<Duration> {
        let ms = self.body.retry_after_ms.map(Duration::from_millis);
        match (self.retry_after, ms) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

impl ApiError {
    /// 對應的 Pulse 代碼
    pub fn code(&self) -> &'static str {
        match self {
            Self::Network { code, .. } | Self::Trust { code, .. } => code,
            Self::Unsupported { .. } => codes::E_ENROLL_UNSUPPORTED,
            Self::Unexpected { .. } => codes::W_SERVER_UNEXPECTED,
            Self::Server(e) => e.code.pulse_code().unwrap_or(codes::W_SERVER_UNEXPECTED),
        }
    }

    /// 原始的協定錯誤碼（`details.protocol_code`）
    pub fn protocol_code(&self) -> Option<&str> {
        match self {
            Self::Server(e) => Some(e.code.as_str()),
            Self::Unexpected { protocol_code, .. } => protocol_code.as_deref(),
            _ => None,
        }
    }

    pub fn server_code(&self) -> Option<EnrollErrorCode> {
        match self {
            Self::Server(e) => Some(e.code),
            _ => None,
        }
    }

    /// 一行說明（日誌、`last_error`）
    pub fn detail(&self) -> String {
        match self {
            Self::Network { detail, .. }
            | Self::Trust { detail, .. }
            | Self::Unexpected { detail, .. } => detail.clone(),
            Self::Unsupported { status } => {
                format!("註冊端點回應 HTTP {status}，Kairos 版本不支援自動註冊")
            }
            Self::Server(e) => format!(
                "HTTP {}；{}；{}",
                e.status,
                e.code.as_str(),
                crate::network::sanitize_for_log(&e.body.message, 200)
            ),
        }
    }
}

/// 成功的回應與量測到的時鐘偏差
#[derive(Debug, Clone)]
pub struct Reply<T> {
    pub body: T,
    pub server_time: String,
    /// 本機減 Kairos（毫秒；正值＝本機較快）
    pub offset_ms: Option<i64>,
}

/// 8444 用戶端
pub struct EnrollClient {
    /// 正規寫法的 `host:port`
    pub endpoint: String,
    verifier: Arc<EnrollVerifier>,
    connector: TlsConnector,
    server_name: ServerName<'static>,
    conn: Option<HttpConn<TlsStream<TcpStream>>>,
    now: NowMs,
    /// 最近一次握手的 TLS 版本
    pub tls_version: Option<String>,
    /// 最近一次回應量測的偏差（含錯誤回應）
    pub last_offset_ms: Option<i64>,
    pub last_server_time: Option<String>,
}

impl EnrollClient {
    /// `endpoint` 必須是 [`ke::normalize_endpoint`] 的正規寫法
    pub fn new(
        endpoint: &str,
        trust: TrustSpec,
        ignore_validity: bool,
        now: NowMs,
    ) -> Result<Self, String> {
        let verifier = Arc::new(EnrollVerifier::new(trust, ignore_validity));
        let config = trust::client_config(verifier.clone())?;
        let host = ke::endpoint_host(endpoint);
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|e| format!("註冊端點「{endpoint}」的主機名稱無法作為 TLS 名稱：{e}"))?;
        Ok(Self {
            endpoint: endpoint.to_string(),
            verifier,
            connector: TlsConnector::from(Arc::new(config)),
            server_name,
            conn: None,
            now,
            tls_version: None,
            last_offset_ms: None,
            last_server_time: None,
        })
    }

    /// 最近一次握手觀察到的鏈與信任錨
    pub fn observed(&self) -> Observed {
        self.verifier.observed()
    }

    /// 本機目前時間（Unix 毫秒）
    pub fn now_ms(&self) -> i64 {
        (self.now)()
    }

    /// 關閉目前的連線（送 close_notify，最多 1 秒）
    pub async fn close(&mut self) {
        if let Some(mut conn) = self.conn.take() {
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                use tokio::io::AsyncWriteExt as _;
                conn.get_mut().shutdown().await
            })
            .await;
        }
    }

    async fn connect(&mut self) -> Result<(), ApiError> {
        let (host, port) = split_endpoint(&self.endpoint);
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
            .await
            .map_err(|_| ApiError::Network {
                code: codes::E_CONN_TIMEOUT,
                detail: format!(
                    "連線到 {} 逾時（{} 秒）",
                    self.endpoint,
                    CONNECT_TIMEOUT.as_secs()
                ),
                refused: false,
            })?
            .map_err(|e| network_error(&self.endpoint, &e))?;
        let _ = tcp.set_nodelay(true);
        let tls = tokio::time::timeout(
            TLS_TIMEOUT,
            self.connector.connect(self.server_name.clone(), tcp),
        )
        .await
        .map_err(|_| ApiError::Network {
            code: codes::E_SERVER_SILENT,
            detail: format!(
                "與 {} 的 TLS 握手逾時（{} 秒）",
                self.endpoint,
                TLS_TIMEOUT.as_secs()
            ),
            refused: false,
        })?
        .map_err(|e| tls_error(&self.endpoint, &e))?;
        self.tls_version = tls.get_ref().1.protocol_version().map(|v| format!("{v:?}"));
        self.conn = Some(HttpConn::new(tls, self.endpoint.clone()));
        Ok(())
    }

    /// 送出一個請求；回傳原始回應
    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        auth: Option<&str>,
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Response, ApiError> {
        let auth_header = auth.map(|s| format!("Bearer {s}"));
        let mut headers: Vec<(&str, &str)> = vec![
            ("Accept", "application/json"),
            ("User-Agent", concat!("pulse/", env!("CARGO_PKG_VERSION"))),
        ];
        if let Some(a) = &auth_header {
            headers.push(("Authorization", a));
        }
        for attempt in 0..2 {
            let reused = self.conn.as_ref().is_some_and(HttpConn::is_reusable);
            if !reused {
                self.conn = None;
                self.connect().await?;
            }
            let conn = self.conn.as_mut().expect("剛建立的連線");
            match conn.send(method, path, &headers, body, timeout).await {
                Ok(resp) => {
                    if resp.close {
                        self.conn = None;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    self.conn = None;
                    let stale = reused
                        && attempt == 0
                        && matches!(
                            e.kind(),
                            io::ErrorKind::UnexpectedEof
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::BrokenPipe
                                | io::ErrorKind::ConnectionAborted
                        );
                    if stale {
                        continue;
                    }
                    return Err(match e.kind() {
                        io::ErrorKind::TimedOut => ApiError::Network {
                            code: codes::E_SERVER_SILENT,
                            detail: format!(
                                "{} 在 {} 秒內沒有回應",
                                self.endpoint,
                                timeout.as_secs()
                            ),
                            refused: false,
                        },
                        io::ErrorKind::InvalidData => ApiError::Unexpected {
                            status: None,
                            protocol_code: None,
                            detail: format!("{} 的回應不是合法的 HTTP：{e}", self.endpoint),
                        },
                        _ => match crate::tls::rustls_error(&e).and_then(trust::trust_error_code) {
                            Some(code) => ApiError::Trust {
                                code,
                                detail: trust::trust_error_text(code).to_string(),
                                unverified_server_time: None,
                            },
                            None => ApiError::Network {
                                code: codes::E_SERVER_SILENT,
                                detail: format!("與 {} 的連線中斷：{e}", self.endpoint),
                                refused: false,
                            },
                        },
                    });
                }
            }
        }
        unreachable!("迴圈最多兩次，第二次一定回傳")
    }

    /// 送出請求並解析回應
    async fn call<T: DeserializeOwned>(
        &mut self,
        route: &Route,
        query: &str,
        auth: Option<&str>,
        body: Option<&impl Serialize>,
        timeout: Duration,
    ) -> Result<Reply<T>, ApiError> {
        let body = match body {
            Some(b) => Some(serde_json::to_vec(b).map_err(|e| ApiError::Unexpected {
                status: None,
                protocol_code: None,
                detail: format!("無法序列化請求：{e}"),
            })?),
            None => None,
        };
        let path = format!("{}{query}", route.path());
        let resp = self
            .raw(route.method(), &path, auth, body.as_deref(), timeout)
            .await?;
        let received_ms = self.now_ms();
        self.interpret(resp, received_ms)
    }

    fn record_time(&mut self, server_time: &str, received_ms: i64) -> Option<i64> {
        let offset = ke::parse_rfc3339_ms(server_time)
            .ok()
            .map(|st| received_ms.saturating_sub(st));
        self.last_offset_ms = offset;
        self.last_server_time = Some(server_time.to_string());
        offset
    }

    fn interpret<T: DeserializeOwned>(
        &mut self,
        resp: Response,
        received_ms: i64,
    ) -> Result<Reply<T>, ApiError> {
        if (200..300).contains(&resp.status) {
            #[derive(serde::Deserialize)]
            struct Time {
                server_time: String,
            }
            let parsed: Result<T, _> = serde_json::from_slice(&resp.body);
            let time: Option<Time> = serde_json::from_slice(&resp.body).ok();
            return match (parsed, time) {
                (Ok(body), Some(t)) => {
                    let offset_ms = self.record_time(&t.server_time, received_ms);
                    Ok(Reply {
                        body,
                        server_time: t.server_time,
                        offset_ms,
                    })
                }
                (Err(e), _) => Err(ApiError::Unexpected {
                    status: Some(resp.status),
                    protocol_code: None,
                    detail: format!("無法解析 HTTP {} 的回應本文：{e}", resp.status),
                }),
                (Ok(_), None) => Err(ApiError::Unexpected {
                    status: Some(resp.status),
                    protocol_code: None,
                    detail: "回應缺少 server_time".to_string(),
                }),
            };
        }
        let parsed: Option<ErrorResponse> = serde_json::from_slice(&resp.body).ok();
        let Some(err) = parsed else {
            return Err(if matches!(resp.status, 404 | 405) {
                ApiError::Unsupported {
                    status: resp.status,
                }
            } else {
                ApiError::Unexpected {
                    status: Some(resp.status),
                    protocol_code: None,
                    detail: format!("HTTP {} 的回應本文不是註冊通道的錯誤格式", resp.status),
                }
            });
        };
        self.record_time(&err.server_time, received_ms);
        match err.error.known_code() {
            Some(code) if code.http_status() == resp.status => Err(ApiError::Server(ServerError {
                status: resp.status,
                code,
                retry_after: resp.retry_after(),
                server_time: err.server_time,
                body: err.error,
            })),
            _ => Err(ApiError::Unexpected {
                status: Some(resp.status),
                protocol_code: Some(crate::network::sanitize_for_log(&err.error.code, 64)),
                detail: format!(
                    "HTTP {} 的錯誤碼「{}」不合預期",
                    resp.status,
                    crate::network::sanitize_for_log(&err.error.code, 64)
                ),
            }),
        }
    }

    pub async fn server_info(&mut self) -> Result<Reply<ServerInfo>, ApiError> {
        self.call(&Route::ServerInfo, "", None, None::<&()>, RESPONSE_TIMEOUT)
            .await
    }

    pub async fn token_check(
        &mut self,
        req: &TokenCheckRequest,
    ) -> Result<Reply<TokenCheckResponse>, ApiError> {
        self.call(&Route::TokenCheck, "", None, Some(req), RESPONSE_TIMEOUT)
            .await
    }

    pub async fn create_request(
        &mut self,
        req: &EnrollRequest,
    ) -> Result<Reply<EnrollResult>, ApiError> {
        self.call(&Route::CreateRequest, "", None, Some(req), RESPONSE_TIMEOUT)
            .await
    }

    /// 輪詢；`wait` 為長輪詢秒數（0 為不等待）
    pub async fn poll(
        &mut self,
        request_id: &str,
        poll_secret: &str,
        wait: u32,
    ) -> Result<Reply<EnrollResult>, ApiError> {
        let query = if wait > 0 {
            format!("?wait={}", wait.min(ke::MAX_LONG_POLL_SECS))
        } else {
            String::new()
        };
        let timeout = RESPONSE_TIMEOUT + Duration::from_secs(u64::from(wait));
        self.call(
            &Route::Poll(request_id.to_ascii_lowercase()),
            &query,
            Some(poll_secret),
            None::<&()>,
            timeout,
        )
        .await
    }

    pub async fn create_renewal(
        &mut self,
        req: &GraceRenewRequest,
    ) -> Result<Reply<EnrollResult>, ApiError> {
        self.call(&Route::CreateRenewal, "", None, Some(req), RESPONSE_TIMEOUT)
            .await
    }

    pub async fn withdraw_challenge(
        &mut self,
        request_id: &str,
        timeout: Duration,
    ) -> Result<Reply<WithdrawChallenge>, ApiError> {
        self.call(
            &Route::WithdrawChallenge(request_id.to_ascii_lowercase()),
            "",
            None,
            Some(&serde_json::json!({})),
            timeout,
        )
        .await
    }

    pub async fn withdraw(
        &mut self,
        req: &WithdrawRequest,
        timeout: Duration,
    ) -> Result<Reply<WithdrawResult>, ApiError> {
        self.call(
            &Route::Withdraw(req.request_id.to_ascii_lowercase()),
            "",
            None,
            Some(req),
            timeout,
        )
        .await
    }
}

/// `host:port`（正規寫法）拆成連線用的主機與連接埠；IPv6 去掉 `[]`
fn split_endpoint(endpoint: &str) -> (&str, u16) {
    let host = ke::endpoint_host(endpoint);
    let port = endpoint
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(ke::DEFAULT_ENROLL_PORT);
    (host, port)
}

/// TCP 連線失敗的代碼與說明
fn network_error(endpoint: &str, e: &io::Error) -> ApiError {
    let text = e.to_string();
    let code = match e.kind() {
        io::ErrorKind::ConnectionRefused => codes::E_CONN_REFUSED,
        io::ErrorKind::TimedOut
        | io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkUnreachable => codes::E_CONN_TIMEOUT,
        _ if text.contains("lookup address")
            || text.contains("No such host")
            || text.contains("nodename nor servname") =>
        {
            codes::E_DNS
        }
        _ => codes::E_CONN_TIMEOUT,
    };
    ApiError::Network {
        code,
        detail: format!("無法連線到註冊端點 {endpoint}：{text}"),
        refused: e.kind() == io::ErrorKind::ConnectionRefused,
    }
}

/// TLS 握手失敗：伺服器身分問題對應 `PLS-E1401`～`E1406`，握手前就被關閉為 `PLS-E2201`（連線層的限制）
fn tls_error(endpoint: &str, e: &io::Error) -> ApiError {
    if let Some(code) = crate::tls::rustls_error(e).and_then(trust::trust_error_code) {
        return ApiError::Trust {
            code,
            detail: trust::trust_error_text(code).to_string(),
            unverified_server_time: None,
        };
    }
    let closed = matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    );
    ApiError::Network {
        code: if closed {
            codes::E_PER_IP_LIMIT
        } else {
            codes::E_SERVER_SILENT
        },
        detail: format!("與註冊端點 {endpoint} 的 TLS 握手失敗：{e}"),
        refused: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_endpoints() {
        assert_eq!(
            split_endpoint("kairos.example.gov.tw:8444"),
            ("kairos.example.gov.tw", 8444)
        );
        assert_eq!(split_endpoint("[2001:db8::1]:9444"), ("2001:db8::1", 9444));
        assert_eq!(split_endpoint("10.0.0.1:1"), ("10.0.0.1", 1));
    }

    #[test]
    fn server_error_wait_hint_takes_the_longer_value() {
        let mut body = ErrorBody::new(EnrollErrorCode::RateLimited, "x");
        body.retry_after_ms = Some(41_500);
        let e = ServerError {
            status: 429,
            code: EnrollErrorCode::RateLimited,
            body,
            retry_after: Some(Duration::from_secs(42)),
            server_time: String::new(),
        };
        assert_eq!(e.wait_hint(), Some(Duration::from_secs(42)));
    }
}
