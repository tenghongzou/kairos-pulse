//! 簽章內容的正規化（canonical bytes）：撤回申請與過期續簽（ENROLL.md「簽章內容」）。
//!
//! 兩者都是 UTF-8、各行以 LF（`\n`）分隔、**結尾沒有換行**，第一行是用途標籤
//! （與 CSR 本身的簽章及另一種請求區分，避免互相重放），最後一行是註冊端點的正規寫法
//! （避免請求被轉送到另一台 Kairos）。函式在組合前驗證每一行，任何一行含換行或格式錯誤時
//! 回傳錯誤，所以兩端只要使用這裡的函式就不會產生歧義的內容。

use std::fmt;

use super::crypto::{b64url_decode_exact, sha256, sha256_hex};
use super::endpoint::normalize_endpoint;
use super::time::parse_rfc3339_ms;
use crate::{is_host_fingerprint, is_uuid_text};

/// 撤回申請的用途標籤
pub const PURPOSE_WITHDRAW: &str = "pulse-enroll-withdraw/v1";
/// 過期續簽的用途標籤
pub const PURPOSE_GRACE_RENEW: &str = "pulse-renew-grace/v1";
/// 撤回 nonce 的長度（bytes；base64url 後 43 個字元）
pub const NONCE_BYTES: usize = 32;

/// 簽章內容的某一欄不合法
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningInputError {
    pub field: &'static str,
    pub reason: String,
}

impl fmt::Display for SigningInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

impl std::error::Error for SigningInputError {}

fn err(field: &'static str, reason: impl Into<String>) -> SigningInputError {
    SigningInputError {
        field,
        reason: reason.into(),
    }
}

fn uuid_lower(field: &'static str, s: &str) -> Result<String, SigningInputError> {
    if is_uuid_text(s) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err(err(field, "must be a UUID"))
    }
}

fn join(lines: &[&str]) -> Vec<u8> {
    lines.join("\n").into_bytes()
}

/// 撤回申請的簽章內容：
///
/// ```text
/// pulse-enroll-withdraw/v1
/// <request_id（小寫）>
/// <agent_id（小寫）>
/// <nonce（Kairos 發出的 base64url 原字串）>
/// <註冊端點 host:port（正規寫法）>
/// ```
pub fn withdraw_signing_input(
    request_id: &str,
    agent_id: &str,
    nonce: &str,
    enroll_endpoint: &str,
) -> Result<Vec<u8>, SigningInputError> {
    let request_id = uuid_lower("request_id", request_id)?;
    let agent_id = uuid_lower("agent_id", agent_id)?;
    b64url_decode_exact(nonce, NONCE_BYTES).map_err(|e| err("nonce", e))?;
    let endpoint =
        normalize_endpoint(enroll_endpoint).map_err(|e| err("enroll_endpoint", e.to_string()))?;
    Ok(join(&[
        PURPOSE_WITHDRAW,
        &request_id,
        &agent_id,
        nonce,
        &endpoint,
    ]))
}

/// 過期續簽的簽章內容：
///
/// ```text
/// pulse-renew-grace/v1
/// <agent_id（小寫）>
/// <host_fingerprint>
/// <新 CSR（DER）的 SHA-256，小寫十六進位>
/// <sent_at（請求中的原字串，逐字相同）>
/// <註冊端點 host:port（正規寫法）>
/// ```
pub fn grace_renew_signing_input(
    agent_id: &str,
    host_fingerprint: &str,
    csr_der: &[u8],
    sent_at: &str,
    enroll_endpoint: &str,
) -> Result<Vec<u8>, SigningInputError> {
    let agent_id = uuid_lower("agent_id", agent_id)?;
    if !is_host_fingerprint(host_fingerprint) {
        return Err(err("host_fingerprint", "invalid host fingerprint"));
    }
    if csr_der.is_empty() {
        return Err(err("csr", "must not be empty"));
    }
    parse_rfc3339_ms(sent_at).map_err(|e| err("sent_at", e.to_string()))?;
    let endpoint =
        normalize_endpoint(enroll_endpoint).map_err(|e| err("enroll_endpoint", e.to_string()))?;
    Ok(join(&[
        PURPOSE_GRACE_RENEW,
        &agent_id,
        host_fingerprint,
        &sha256_hex(csr_der),
        sent_at,
        &endpoint,
    ]))
}

/// 過期續簽重放快取的鍵：**簽章內容**的 SHA-256（不是簽章位元組：ECDSA 簽章不唯一，
/// 以位元組為鍵可被改寫後繞過）
pub fn replay_cache_key(signing_input: &[u8]) -> [u8; 32] {
    sha256(signing_input)
}
