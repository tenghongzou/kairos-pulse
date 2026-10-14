//! 請求本文的欄位驗證（ENROLL.md 各端點的「欄位限制」）。
//!
//! Kairos 用來判定 `bad_request`（附 `field`）；Pulse 在送出前用同一份規則自我檢查。驗證通過時
//! 回傳已解碼、已正規化的值，Kairos 不必再自己解碼一次。CSR 與憑證的 ASN.1 內容（金鑰種類、
//! SAN、簽章）不在這裡檢查，由 Kairos 以 x509-parser 檢查（ENROLL.md「CSR 的要求」）。

use std::fmt;

use super::crypto::{b64url_decode, b64url_decode_exact};
use super::messages::{EnrollRequest, GraceRenewRequest, TokenCheckRequest, WithdrawRequest};
use super::signing::NONCE_BYTES;
use super::time::parse_rfc3339_ms;
use super::token::EnrollToken;
use crate::validate::validate_display_text;
use crate::{
    HostInfo, MAX_AGENT_VERSION_BYTES, MAX_HOSTNAME_BYTES, is_host_fingerprint, is_uuid_text,
    validate_host_info,
};

/// `poll_secret` 的長度（bytes；base64url 後 43 個字元）
pub const POLL_SECRET_BYTES: usize = 32;
/// CSR（DER）的大小上限
pub const MAX_CSR_DER_BYTES: usize = 4096;
/// 憑證（DER）的大小上限
pub const MAX_CERT_DER_BYTES: usize = 8192;
/// 序號的長度上限（十六進位字元；RFC 5280 的 20 octets）
pub const MAX_SERIAL_HEX: usize = 40;
/// 長輪詢最多保持的秒數（`wait` 查詢參數的上限）
pub const MAX_LONG_POLL_SECS: u32 = 60;

/// 欄位不合法（Kairos 回 400 `bad_request`，`error.field` 為欄位名稱）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: &'static str,
    pub reason: String,
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

impl std::error::Error for FieldError {}

fn fe(field: &'static str, reason: impl Into<String>) -> FieldError {
    FieldError {
        field,
        reason: reason.into(),
    }
}

fn agent_id(s: &str) -> Result<String, FieldError> {
    if is_uuid_text(s) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err(fe("agent_id", "must be a UUID (8-4-4-4-12 hex digits)"))
    }
}

fn der(field: &'static str, s: &str, max: usize) -> Result<Vec<u8>, FieldError> {
    let bytes = b64url_decode(s).map_err(|e| fe(field, e.to_string()))?;
    if bytes.is_empty() || bytes.len() > max {
        return Err(fe(field, format!("must be 1..={max} bytes of DER")));
    }
    Ok(bytes)
}

fn sent_at(s: &str) -> Result<i64, FieldError> {
    parse_rfc3339_ms(s).map_err(|e| fe("sent_at", e.to_string()))
}

fn common_text(hostname: &str, pulse_version: &str) -> Result<(), FieldError> {
    validate_display_text("hostname", hostname, MAX_HOSTNAME_BYTES)
        .map_err(|e| fe("hostname", e))?;
    validate_display_text("pulse_version", pulse_version, MAX_AGENT_VERSION_BYTES)
        .map_err(|e| fe("pulse_version", e))
}

/// 序號：1～40 個大寫十六進位字元
pub fn is_serial_hex(s: &str) -> bool {
    (1..=MAX_SERIAL_HEX).contains(&s.len())
        && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'A'..=b'F'))
}

/// 已驗證的 `token-check`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedTokenCheck {
    pub token: EnrollToken,
}

/// 驗證 `token-check`（`sent_at` 只檢查格式，不判定時鐘）
pub fn validate_token_check(req: &TokenCheckRequest) -> Result<CheckedTokenCheck, FieldError> {
    let token = EnrollToken::parse(&req.token).map_err(|e| fe("token", e.to_string()))?;
    if let Some(s) = &req.sent_at {
        sent_at(s)?;
    }
    if let Some(v) = &req.pulse_version {
        validate_display_text("pulse_version", v, MAX_AGENT_VERSION_BYTES)
            .map_err(|e| fe("pulse_version", e))?;
    }
    Ok(CheckedTokenCheck { token })
}

/// 已驗證的新註冊請求
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedEnrollRequest {
    pub token: EnrollToken,
    /// 小寫
    pub agent_id: String,
    pub csr_der: Vec<u8>,
    pub poll_secret: Vec<u8>,
    pub sent_at_ms: i64,
}

/// 驗證新註冊請求
pub fn validate_enroll_request(req: &EnrollRequest) -> Result<CheckedEnrollRequest, FieldError> {
    let token = EnrollToken::parse(&req.token).map_err(|e| fe("token", e.to_string()))?;
    let agent_id = agent_id(&req.agent_id)?;
    let csr_der = der("csr", &req.csr, MAX_CSR_DER_BYTES)?;
    let poll_secret = b64url_decode_exact(&req.poll_secret, POLL_SECRET_BYTES)
        .map_err(|e| fe("poll_secret", e))?;
    let sent_at_ms = sent_at(&req.sent_at)?;
    common_text(&req.hostname, &req.pulse_version)?;
    let host = HostInfo {
        host_fingerprint: req.host_fingerprint.clone(),
        display_name: req.display_name.clone(),
        tags: req.tags.clone(),
        report_interval_ms: None,
        os: req.os.clone(),
    };
    validate_host_info(&host).map_err(|e| {
        let field = if e.starts_with("host_fingerprint") {
            "host_fingerprint"
        } else if e.starts_with("display_name") {
            "display_name"
        } else if e.starts_with("os.") {
            "os"
        } else {
            "tags"
        };
        fe(field, e)
    })?;
    if let Some(serial) = &req.previous_serial
        && !is_serial_hex(serial)
    {
        return Err(fe("previous_serial", "must be 1..=40 uppercase hex digits"));
    }
    Ok(CheckedEnrollRequest {
        token,
        agent_id,
        csr_der,
        poll_secret,
        sent_at_ms,
    })
}

/// 已驗證的過期續簽請求
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedGraceRenewRequest {
    pub agent_id: String,
    pub csr_der: Vec<u8>,
    pub poll_secret: Vec<u8>,
    pub sent_at_ms: i64,
    pub expired_certificate_der: Vec<u8>,
}

/// 驗證過期續簽請求（簽章本身由 Kairos 以舊憑證的公鑰驗證，見 ENROLL.md「過期續簽的判定順序」）
pub fn validate_grace_renew_request(
    req: &GraceRenewRequest,
) -> Result<CheckedGraceRenewRequest, FieldError> {
    let agent_id = agent_id(&req.agent_id)?;
    let csr_der = der("csr", &req.csr, MAX_CSR_DER_BYTES)?;
    let poll_secret = b64url_decode_exact(&req.poll_secret, POLL_SECRET_BYTES)
        .map_err(|e| fe("poll_secret", e))?;
    let sent_at_ms = sent_at(&req.sent_at)?;
    if !is_host_fingerprint(&req.host_fingerprint) {
        return Err(fe("host_fingerprint", "invalid host fingerprint"));
    }
    let expired_certificate_der = der(
        "expired_certificate",
        &req.expired_certificate,
        MAX_CERT_DER_BYTES,
    )?;
    b64url_decode(&req.signature).map_err(|e| fe("signature", e.to_string()))?;
    common_text(&req.hostname, &req.pulse_version)?;
    Ok(CheckedGraceRenewRequest {
        agent_id,
        csr_der,
        poll_secret,
        sent_at_ms,
        expired_certificate_der,
    })
}

/// 已驗證的撤回請求
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedWithdrawRequest {
    /// 小寫
    pub request_id: String,
    /// 小寫
    pub agent_id: String,
}

/// 驗證撤回請求；`path_request_id` 為路徑中的 `request_id`（本文的 `request_id` 必須相同，不分大小寫）
pub fn validate_withdraw_request(
    path_request_id: &str,
    req: &WithdrawRequest,
) -> Result<CheckedWithdrawRequest, FieldError> {
    if !is_uuid_text(&req.request_id) || !req.request_id.eq_ignore_ascii_case(path_request_id) {
        return Err(fe("request_id", "must be the UUID in the request path"));
    }
    let agent_id = agent_id(&req.agent_id)?;
    b64url_decode_exact(&req.nonce, NONCE_BYTES).map_err(|e| fe("nonce", e))?;
    b64url_decode(&req.signature).map_err(|e| fe("signature", e.to_string()))?;
    sent_at(&req.sent_at)?;
    common_text(&req.hostname, &req.pulse_version)?;
    if let Some(fp) = &req.host_fingerprint
        && !is_host_fingerprint(fp)
    {
        return Err(fe("host_fingerprint", "invalid host fingerprint"));
    }
    Ok(CheckedWithdrawRequest {
        request_id: req.request_id.to_ascii_lowercase(),
        agent_id,
    })
}

/// 輪詢的 `wait` 查詢參數（長輪詢秒數，0～60；省略為 0）。不合法時 `None`（400 `bad_request`）
pub fn parse_wait_secs(query: Option<&str>) -> Option<u32> {
    let Some(query) = query.filter(|q| !q.is_empty()) else {
        return Some(0);
    };
    let mut wait = 0;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("wait", v))
                if !v.is_empty() && v.len() <= 2 && v.bytes().all(|b| b.is_ascii_digit()) =>
            {
                wait = v.parse::<u32>().ok()?;
            }
            Some(("wait", _)) => return None,
            _ => {}
        }
    }
    (wait <= MAX_LONG_POLL_SECS).then_some(wait)
}

/// 從 `Authorization` 標頭取出輪詢憑據（`Bearer <poll_secret>`，scheme 不分大小寫）並解碼
pub fn parse_poll_authorization(header: &str) -> Option<Vec<u8>> {
    let (scheme, value) = header.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    b64url_decode_exact(value.trim(), POLL_SECRET_BYTES).ok()
}
