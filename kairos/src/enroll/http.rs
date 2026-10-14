//! 8444 的回應格式（`crates/kairos-proto/ENROLL.md` 第 2、5 節）：
//!
//! - 成功：HTTP 200，本文是 JSON 物件，`server_time` 在最外層（沒有 `data` 包裝）。
//! - 錯誤：`{"server_time", "error": {"code", "message", …附加欄位}}`；`code` 與 HTTP 狀態碼的對照
//!   由 `kairos_proto::enroll::EnrollErrorCode` 決定。
//! - 每個回應都有 `Cache-Control: no-store`；429、503 另帶 `Retry-After`（整數秒）。

use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Response, StatusCode};
use kairos_proto::enroll::{
    EnrollErrorCode, ErrorBody, ErrorResponse, RateScope, format_rfc3339_ms,
};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

use super::limits::retry_after_secs;
use crate::api::http::ApiBody;

/// 8444 的錯誤（轉成 [`ErrorResponse`]）
#[derive(Debug, Clone, PartialEq)]
pub struct EnrollError {
    pub code: EnrollErrorCode,
    /// 錯誤本文（裝箱：讓 `Result<_, EnrollError>` 保持精簡）
    pub body: Box<ErrorBody>,
    /// `Retry-After`（秒）
    pub retry_after: Option<u64>,
    /// 405 的 `Allow`
    pub allow: Option<&'static str>,
}

impl EnrollError {
    pub fn new(code: EnrollErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            body: Box::new(ErrorBody::new(code, message)),
            retry_after: None,
            allow: None,
        }
    }

    /// 400 `bad_request`（`field`）
    pub fn bad_request(field: &str, message: impl Into<String>) -> Self {
        let mut e = Self::new(EnrollErrorCode::BadRequest, message);
        e.body.field = Some(field.to_string());
        e
    }

    /// 429／503 的限流錯誤：`Retry-After`＝等待時間（無條件進位），`retry_after_ms` 相同
    pub fn limited(
        code: EnrollErrorCode,
        scope: RateScope,
        wait: Duration,
        observed_ip: &str,
    ) -> Self {
        let secs = retry_after_secs(wait);
        let message = match code {
            EnrollErrorCode::SourceBlocked => "source is temporarily blocked",
            EnrollErrorCode::EnrollBusy => "enrollment endpoint is busy",
            EnrollErrorCode::PendingQueueFull => "pending approval queue is full",
            _ => "enroll rate limit exceeded",
        };
        let mut e = Self::new(code, message);
        e.body.scope = Some(scope);
        e.body.retry_after_ms = Some(secs * 1000);
        e.body.observed_ip = Some(observed_ip.to_string());
        e.retry_after = Some(secs);
        e
    }

    /// 503 `storage_unavailable`
    pub fn storage() -> Self {
        let mut e = Self::new(
            EnrollErrorCode::StorageUnavailable,
            "storage is temporarily unavailable",
        );
        e.body.retry_after_ms = Some(5000);
        e.retry_after = Some(5);
        e
    }

    pub fn not_found() -> Self {
        Self::new(EnrollErrorCode::NotFound, "not found")
    }

    pub fn request_not_found() -> Self {
        Self::new(
            EnrollErrorCode::RequestNotFound,
            "enrollment request not found",
        )
    }

    pub fn into_response(self, now_ms: i64) -> Response<ApiBody> {
        let status = StatusCode::from_u16(self.code.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = ErrorResponse {
            server_time: format_rfc3339_ms(now_ms),
            error: *self.body,
        };
        let mut resp = json_response(status, &body);
        if let Some(secs) = self.retry_after
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        if let Some(allow) = self.allow {
            resp.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static(allow));
        }
        resp
    }
}

/// JSON 回應（不快取）
pub fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<ApiBody> {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let mut resp = Response::new(ApiBody::Full(Some(Bytes::from(bytes))));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

/// 成功回應：`value` 序列化後的最外層 `server_time` 換成產生回應的時間
pub fn ok<T: Serialize>(value: &T, now_ms: i64) -> Response<ApiBody> {
    let mut v = serde_json::to_value(value).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut v {
        map.insert(
            "server_time".to_string(),
            Value::String(format_rfc3339_ms(now_ms)),
        );
    }
    json_response(StatusCode::OK, &v)
}

/// 純文字回應（下載端點在模式不受理時的 403 說明）
pub fn text(status: StatusCode, message: &str) -> Response<ApiBody> {
    let mut resp = Response::new(ApiBody::Full(Some(Bytes::from(message.to_string()))));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// 讀取 `POST` 的本文：必須帶 `Content-Length`（不接受 chunked）、不超過 `max` bytes、
/// `Content-Type` 為 `application/json`（可帶 `; charset=utf-8`）
pub async fn read_json_body(
    req: hyper::Request<Incoming>,
    max: usize,
) -> Result<Bytes, EnrollError> {
    let headers = req.headers();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !is_json_content_type(content_type) {
        return Err(EnrollError::new(
            EnrollErrorCode::UnsupportedMediaType,
            "Content-Type must be application/json",
        ));
    }
    if headers.contains_key(header::TRANSFER_ENCODING) {
        return Err(EnrollError::bad_request(
            "content-length",
            "chunked request bodies are not accepted; send Content-Length",
        ));
    }
    let len = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
        .ok_or_else(|| EnrollError::bad_request("content-length", "Content-Length is required"))?;
    if len > max {
        return Err(EnrollError::new(
            EnrollErrorCode::PayloadTooLarge,
            format!("request body exceeds {max} bytes"),
        ));
    }
    collect(req.into_body(), len).await
}

/// `GET` 不得帶本文
pub fn reject_body(headers: &hyper::HeaderMap) -> Result<(), EnrollError> {
    let has_body = headers.contains_key(header::TRANSFER_ENCODING)
        || headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() != "0");
    if has_body {
        return Err(EnrollError::bad_request(
            "body",
            "GET requests must not have a body",
        ));
    }
    Ok(())
}

fn is_json_content_type(v: &str) -> bool {
    let mut parts = v.split(';').map(str::trim);
    let media = parts.next().unwrap_or("");
    if !media.eq_ignore_ascii_case("application/json") {
        return false;
    }
    parts.all(|p| {
        p.is_empty()
            || p.split_once('=').is_some_and(|(k, v)| {
                k.trim().eq_ignore_ascii_case("charset")
                    && v.trim().trim_matches('"').eq_ignore_ascii_case("utf-8")
            })
    })
}

/// 讀取 `len` bytes 的本文（多送或少送都是 400）
async fn collect(mut body: Incoming, len: usize) -> Result<Bytes, EnrollError> {
    use hyper::body::Body;
    let mut buf = Vec::with_capacity(len);
    loop {
        let frame = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await;
        match frame {
            None => break,
            Some(Err(_)) => {
                return Err(EnrollError::bad_request(
                    "body",
                    "request body could not be read",
                ));
            }
            Some(Ok(f)) => {
                if let Ok(data) = f.into_data() {
                    if buf.len() + data.len() > len {
                        return Err(EnrollError::bad_request(
                            "content-length",
                            "request body is longer than Content-Length",
                        ));
                    }
                    buf.extend_from_slice(&data);
                }
            }
        }
    }
    if buf.len() != len {
        return Err(EnrollError::bad_request(
            "content-length",
            "request body is shorter than Content-Length",
        ));
    }
    Ok(Bytes::from(buf))
}

/// 解析 JSON 本文；失敗時 400 `bad_request`（`field` 盡量指出是哪一個欄位）
pub fn parse_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, EnrollError> {
    if !bytes.contains(&b'{') {
        return Err(EnrollError::bad_request(
            "body",
            "request body must be a JSON object",
        ));
    }
    serde_json::from_slice(bytes).map_err(|e| {
        let msg = e.to_string();
        // serde 的訊息：missing field `token`、unknown variant `x` … 取出欄位名稱
        let field = msg
            .split('`')
            .nth(1)
            .filter(|_| msg.starts_with("missing field"))
            .unwrap_or("body")
            .to_string();
        EnrollError::bad_request(&field, format!("invalid request body: {msg}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_accepts_json_with_utf8_charset_only() {
        assert!(is_json_content_type("application/json"));
        assert!(is_json_content_type("Application/JSON; charset=UTF-8"));
        assert!(is_json_content_type("application/json; charset=\"utf-8\""));
        assert!(!is_json_content_type("application/json; charset=latin1"));
        assert!(!is_json_content_type("text/plain"));
        assert!(!is_json_content_type(""));
    }

    #[test]
    fn errors_have_the_spec_shape_and_headers() {
        let e = EnrollError::limited(
            EnrollErrorCode::RateLimited,
            RateScope::Ip,
            Duration::from_millis(41_500),
            "203.0.113.10",
        );
        let resp = e.into_response(1_760_148_786_000);
        assert_eq!(resp.status(), 429);
        assert_eq!(resp.headers()[header::RETRY_AFTER], "42");
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
        let ApiBody::Full(Some(bytes)) = resp.into_body() else {
            panic!()
        };
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "rate_limited");
        assert_eq!(v["error"]["scope"], "ip");
        assert_eq!(v["error"]["retry_after_ms"], 42000);
        assert_eq!(v["error"]["observed_ip"], "203.0.113.10");
        assert!(v["server_time"].as_str().unwrap().ends_with('Z'));
        let parsed: ErrorResponse = serde_json::from_value(v).unwrap();
        assert_eq!(
            parsed.error.known_code(),
            Some(EnrollErrorCode::RateLimited)
        );
    }

    #[test]
    fn json_errors_name_the_missing_field() {
        let e = parse_json::<kairos_proto::enroll::TokenCheckRequest>(b"{}").unwrap_err();
        assert_eq!(e.body.field.as_deref(), Some("token"));
        let e = parse_json::<kairos_proto::enroll::TokenCheckRequest>(b"[1]").unwrap_err();
        assert_eq!(e.code, EnrollErrorCode::BadRequest);
    }
}
