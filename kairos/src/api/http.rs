//! HTTP 的共用部分：回應本文、JSON 回應與錯誤格式（`kairos/API.md` 第 1 節）、查詢參數、
//! base64url（游標與 token）。

use chrono::{DateTime, Utc};
use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::header::{self, HeaderValue};
use hyper::{Response, StatusCode};
use serde::Serialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::{OwnedSemaphorePermit, mpsc};

use crate::storage::fmt_time;

/// 回應本文：一次送完的 JSON，或 SSE 的串流（由送出端的 channel 提供）
#[derive(Debug)]
pub enum ApiBody {
    Full(Option<Bytes>),
    /// SSE：channel 關閉時結束；permit 在本文結束（連線關閉）時釋放（`console.max_streams`）
    Stream {
        rx: mpsc::Receiver<Bytes>,
        _permit: Option<OwnedSemaphorePermit>,
    },
}

impl Body for ApiBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            ApiBody::Full(data) => Poll::Ready(data.take().map(|d| Ok(Frame::data(d)))),
            ApiBody::Stream { rx, .. } => rx.poll_recv(cx).map(|c| c.map(|d| Ok(Frame::data(d)))),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, ApiBody::Full(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            ApiBody::Full(Some(d)) => SizeHint::with_exact(d.len() as u64),
            ApiBody::Full(None) => SizeHint::with_exact(0),
            ApiBody::Stream { .. } => SizeHint::default(),
        }
    }
}

/// RFC 3339、UTC、毫秒（`2026-10-13T06:03:27.123Z`）
pub fn ts(t: DateTime<Utc>) -> String {
    fmt_time(t)
}

/// Unix 毫秒 → RFC 3339
pub fn ts_ms(ms: i64) -> String {
    ts(DateTime::from_timestamp_millis(ms).unwrap_or_default())
}

/// 加上共用的標頭（不快取、不猜測內容類型）
fn common_headers(resp: &mut Response<ApiBody>) {
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
}

/// JSON 回應：`body` 的最外層加上 `server_time`
pub fn json(status: StatusCode, now: DateTime<Utc>, mut body: Value) -> Response<ApiBody> {
    if let Value::Object(map) = &mut body {
        map.insert("server_time".to_string(), Value::String(ts(now)));
    }
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let mut resp = Response::new(ApiBody::Full(Some(Bytes::from(bytes))));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    common_headers(&mut resp);
    resp
}

/// 成功回應：`{"server_time", "data"}`
pub fn ok(now: DateTime<Utc>, data: impl Serialize) -> Response<ApiBody> {
    json(
        StatusCode::OK,
        now,
        json!({ "data": serde_json::to_value(data).unwrap_or(Value::Null) }),
    )
}

/// SSE 的回應標頭
pub fn event_stream(body: ApiBody) -> Response<ApiBody> {
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    // 反向代理（nginx）不要緩衝
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    common_headers(&mut resp);
    resp
}

/// API 的錯誤（`kairos/API.md` 第 1.2 節）
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub product_code: Option<&'static str>,
    pub message: String,
    pub details: Option<Value>,
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            product_code: None,
            message: message.into(),
            details: None,
            retry_after: None,
        }
    }

    pub fn with_product_code(mut self, code: &'static str) -> Self {
        self.product_code = Some(code);
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs.max(1));
        self
    }

    /// 400 `invalid_parameter`
    pub fn param(name: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_parameter", message)
            .with_details(json!({ "parameter": name }))
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "需要有效的 API token（Authorization: Bearer <token>）",
        )
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "沒有這個路徑")
    }

    pub fn host_not_found(agent_id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "host_not_found",
            format!("主機清冊中沒有 {agent_id}（可能已依保留政策清除）"),
        )
    }

    /// 503 `storage_unavailable`（`KRS-E4003`）：資料庫查詢失敗；原因只記在日誌
    pub fn storage(what: &str) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            format!("無法載入{what}：資料庫查詢失敗"),
        )
        .with_product_code("KRS-E4003")
    }

    /// 500 `internal`
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "未預期的錯誤，請查看 Kairos 的日誌",
        )
        .with_product_code("KRS-E4003")
    }

    pub fn into_response(self, now: DateTime<Utc>) -> Response<ApiBody> {
        let mut error = json!({
            "code": self.code,
            "product_code": self.product_code,
            "message": self.message,
        });
        if let Some(details) = self.details {
            error["details"] = details;
        }
        let mut resp = json(self.status, now, json!({ "error": error }));
        if let Some(secs) = self.retry_after
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        if self.status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"kairos\""),
            );
        }
        if self.status == StatusCode::METHOD_NOT_ALLOWED {
            resp.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        }
        resp
    }
}

/// 解碼 URL 的百分比編碼；`plus_as_space` 用於查詢參數。結果不是 UTF-8 時為 `None`
pub fn percent_decode(s: &str, plus_as_space: bool) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                let v = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                out.push(v);
                i += 3;
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// 查詢參數
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pairs: Vec<(String, String)>,
}

/// 查詢參數的數量上限（避免以大量參數消耗 CPU）
pub const MAX_QUERY_PARAMS: usize = 64;

impl Query {
    pub fn parse(raw: Option<&str>) -> Result<Self, ApiError> {
        let mut pairs = Vec::new();
        for part in raw.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
            if pairs.len() >= MAX_QUERY_PARAMS {
                return Err(ApiError::param(
                    "query",
                    format!("查詢參數最多 {MAX_QUERY_PARAMS} 個"),
                ));
            }
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            let k = percent_decode(k, true)
                .ok_or_else(|| ApiError::param("query", "查詢參數的編碼不正確"))?;
            let v = percent_decode(v, true)
                .ok_or_else(|| ApiError::param(&k, "查詢參數的編碼不正確"))?;
            pairs.push((k, v));
        }
        Ok(Self { pairs })
    }

    /// 參數的值（出現多次時取最後一個）；空字串視為沒有
    pub fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    }

    /// 以逗號分隔、可重複的參數的所有值（去除空白與空項目）
    pub fn list(&self, name: &str) -> Vec<String> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == name)
            .flat_map(|(_, v)| v.split(','))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 名稱以 `prefix` 開頭的參數（`label.<key>`）
    pub fn with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.pairs
            .iter()
            .filter_map(move |(k, v)| k.strip_prefix(prefix).map(|k| (k, v.as_str())))
    }

    /// 是否有不認得的參數名稱（只接受 `known` 與 `prefixes` 開頭的）
    pub fn reject_unknown(&self, known: &[&str], prefixes: &[&str]) -> Result<(), ApiError> {
        for (k, _) in &self.pairs {
            if !known.contains(&k.as_str()) && !prefixes.iter().any(|p| k.starts_with(p)) {
                return Err(ApiError::param(k, format!("不認得的查詢參數 {k}")));
            }
        }
        Ok(())
    }

    /// 布林參數：`true`／`false`
    pub fn bool(&self, name: &str) -> Result<Option<bool>, ApiError> {
        match self.get(name) {
            None => Ok(None),
            Some("true") => Ok(Some(true)),
            Some("false") => Ok(Some(false)),
            Some(v) => Err(ApiError::param(
                name,
                format!("{name} 的值 \"{v}\" 必須是 true 或 false"),
            )),
        }
    }
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url（不補 `=`）
pub fn b64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(B64URL[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// [`b64url_encode`] 的反向；有不合法的字元或長度時為 `None`
pub fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| B64URL.iter().position(|&x| x == c).map(|p| p as u32);
    let bytes = text.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= value(c)? << (18 - 6 * i);
        }
        let len = chunk.len() - 1;
        for i in 0..len {
            out.push((n >> (16 - 8 * i)) as u8);
        }
        // 未使用的位元必須為 0（同一份資料只有一種編碼）
        let used_bits = len * 8;
        let mask = if used_bits >= 24 {
            0
        } else {
            (1u32 << (24 - used_bits)) - 1
        };
        if n & mask != 0 {
            return None;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trip() {
        for data in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            &[0xff, 0xfe, 0x00, 0x10],
        ] {
            let text = b64url_encode(data);
            assert!(!text.contains('='), "{text}");
            assert_eq!(b64url_decode(&text).as_deref(), Some(data), "{text}");
        }
        assert_eq!(b64url_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64url_encode(&[0xfb, 0xff]), "-_8");
        for bad in ["a", "Zm9v!", "Zm9=", "Zh"] {
            assert_eq!(b64url_decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn query_parameters() {
        let q = Query::parse(Some(
            "status=offline,unknown&status=online&q=hmi%2Dline3+07&label.mountpoint=%2F&empty=&x",
        ))
        .unwrap();
        assert_eq!(q.list("status"), ["offline", "unknown", "online"]);
        assert_eq!(q.get("q"), Some("hmi-line3 07"));
        assert_eq!(q.get("empty"), None);
        assert_eq!(q.get("missing"), None);
        assert_eq!(
            q.with_prefix("label.").collect::<Vec<_>>(),
            [("mountpoint", "/")]
        );
        assert!(
            q.reject_unknown(&["status", "q", "empty", "x"], &["label."])
                .is_ok()
        );
        let err = q.reject_unknown(&["status"], &[]).unwrap_err();
        assert_eq!(err.code, "invalid_parameter");
        assert!(Query::parse(Some("a=%zz")).is_err());
        assert!(Query::parse(Some("a=%e4")).is_err());
        let many = (0..=MAX_QUERY_PARAMS)
            .map(|i| format!("p{i}=1"))
            .collect::<Vec<_>>()
            .join("&");
        assert!(Query::parse(Some(&many)).is_err());
        let q = Query::parse(Some("durable=true&b=no")).unwrap();
        assert_eq!(q.bool("durable").unwrap(), Some(true));
        assert!(q.bool("b").is_err());
        assert_eq!(q.bool("c").unwrap(), None);
        assert_eq!(percent_decode("%E4%B8%BB", false).as_deref(), Some("主"));
        assert_eq!(percent_decode("a+b", false).as_deref(), Some("a+b"));
    }

    #[test]
    fn error_responses_have_the_envelope_and_headers() {
        let now = DateTime::from_timestamp_millis(1_760_335_407_123).unwrap();
        let resp = ApiError::unauthorized().into_response(now);
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers()[header::WWW_AUTHENTICATE],
            "Bearer realm=\"kairos\""
        );
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
        let ApiBody::Full(Some(body)) = resp.into_body() else {
            panic!("應有本文");
        };
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["server_time"], "2025-10-13T06:03:27.123Z");
        assert_eq!(v["error"]["code"], "unauthorized");
        assert_eq!(v["error"]["product_code"], Value::Null);

        let resp = ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "x")
            .with_retry_after(6)
            .into_response(now);
        assert_eq!(resp.headers()[header::RETRY_AFTER], "6");
        let resp = ApiError::storage("主機清單").into_response(now);
        let ApiBody::Full(Some(body)) = resp.into_body() else {
            panic!("應有本文");
        };
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["product_code"], "KRS-E4003");
        assert!(v["error"].get("details").is_none());
    }
}
