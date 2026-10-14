//! 註冊通道（8444 HTTPS）的共用型別與純函式。規格只有一份：本 crate 目錄下的
//! [`ENROLL.md`](../../ENROLL.md)；單元測試以 `include_str!` 讀入該文件，從「範例」節解析每一則
//! 請求與回應本文、從「測試向量」節取得簽章內容與雜湊的預期值、從「錯誤碼」節比對對照表。
//!
//! 內容：
//! - 錯誤碼與產品代碼、HTTP 狀態碼的對照（[`EnrollErrorCode`]），申請狀態與各種列舉
//! - 請求與回應本文（[`EnrollRequest`]、[`EnrollResult`] 等）與欄位驗證（`validate_*`）
//! - 路徑白名單（[`parse_route`]）與模式白名單（[`mode_decision`]）
//! - 註冊 token（[`EnrollToken`]：格式、檢查碼、雜湊）與安裝字串（[`InstallString`]）
//! - CA 指紋、公鑰指紋、申請編號（[`request_code_from_spki`]）
//! - 簽章內容的正規化（[`withdraw_signing_input`]、[`grace_renew_signing_input`]）、
//!   重放快取鍵與 ECDSA P-256 的簽章與驗證
//! - `sent_at` 的解析與時鐘偏差的判定
//!
//! 不含伺服器或客戶端邏輯（HTTP、資料庫、速率限制、憑證簽發），也不讀系統時間。

mod codes;
mod crypto;
mod endpoint;
mod messages;
mod route;
mod signing;
mod time;
mod token;
mod validate;

#[cfg(test)]
mod tests;

pub use codes::{
    ApprovalDetail, EndpointClass, EnrollErrorCode, EnrollMode, InitiatedBy, ModeDecision,
    RateClass, RateScope, RequestKind, RequestState, ServerVerification, SignatureAlg,
    TokenApproval, WithdrawReason, mode_decision,
};
pub use crypto::{
    B64Error, CaPinError, REQUEST_CODE_DOMAIN, RequestCode, SignatureError, b64url_decode,
    b64url_decode_exact, b64url_encode, ca_pin_display, ca_pin_from_spki, hex_lower,
    normalize_ca_pin, p256_point_from_spki, p256_spki_from_point, p256_spki_of_pkcs8,
    request_code_from_spki, sha256, sha256_hex, sign_ecdsa_p256_sha256, spki_fingerprint,
    spki_fingerprint_short, verify_ecdsa_p256_sha256,
};
pub use endpoint::{EndpointError, endpoint_host, normalize_endpoint};
pub use messages::{
    Accepts, CertPolicy, EnrollRequest, EnrollResult, ErrorBody, ErrorResponse, GraceRenewRequest,
    IssuedCertificate, ServerInfo, TokenCheckRequest, TokenCheckResponse, TokenStatus,
    WithdrawChallenge, WithdrawRequest, WithdrawResult,
};
pub use route::{DOWNLOAD_PREFIX, ENROLL_API_PREFIX, Route, RouteError, parse_route};
pub use signing::{
    NONCE_BYTES, PURPOSE_GRACE_RENEW, PURPOSE_WITHDRAW, SigningInputError,
    grace_renew_signing_input, replay_cache_key, withdraw_signing_input,
};
pub use time::{
    MAX_CLOCK_SKEW_MS, MAX_SENT_AT_BYTES, TimeFormatError, clock_offset_ms, exceeds_clock_skew,
    format_rfc3339_ms, parse_rfc3339_ms,
};
pub use token::{
    CROCKFORD_ALPHABET, EnrollToken, INSTALL_STRING_SCHEME, INSTALL_STRING_VERSION, InstallString,
    InstallStringError, TOKEN_HASH_DOMAIN, TOKEN_PREFIX, TOKEN_SECRET_SYMBOLS, TokenFormatError,
    crockford_value, token_check_symbol,
};
pub use validate::{
    CheckedEnrollRequest, CheckedGraceRenewRequest, CheckedTokenCheck, CheckedWithdrawRequest,
    FieldError, MAX_CERT_DER_BYTES, MAX_CSR_DER_BYTES, MAX_LONG_POLL_SECS, MAX_SERIAL_HEX,
    POLL_SECRET_BYTES, is_serial_hex, parse_poll_authorization, parse_wait_secs,
    validate_enroll_request, validate_grace_renew_request, validate_token_check,
    validate_withdraw_request,
};

/// 註冊端點的預設連接埠
pub const DEFAULT_ENROLL_PORT: u16 = 8444;
/// 註冊 API 版本（`server-info` 的 `enroll_api`）
pub const ENROLL_API_VERSION: &str = "v1";
/// `enroll_features`：撤回待核准申請
pub const ENROLL_FEATURE_WITHDRAW: &str = "withdraw";
/// `enroll_features`：過期寬限期續簽
pub const ENROLL_FEATURE_GRACE_RENEW: &str = "grace_renew";
/// `enroll_features`：長輪詢（`wait` 查詢參數）
pub const ENROLL_FEATURE_LONG_POLL: &str = "long_poll";
/// 撤回 nonce 的有效期間（毫秒）
pub const WITHDRAW_NONCE_TTL_MS: i64 = 60_000;
/// 過期續簽重放快取的保存期間（毫秒；涵蓋 ±5 分鐘的整個有效視窗）
pub const REPLAY_CACHE_TTL_MS: i64 = 600_000;
/// 請求本文的大小上限（bytes；`enroll.max_body_bytes` 的預設值）
pub const MAX_BODY_BYTES: usize = 65_536;
/// 第一次輪詢前的等待（毫秒；動態 `poll_after_ms` 的下限）
pub const POLL_AFTER_BASE_MS: u64 = 30_000;
/// 動態 `poll_after_ms` 的上限（毫秒）
pub const POLL_AFTER_MAX_MS: u64 = 900_000;
/// 同一筆申請兩次輪詢的最短間隔（毫秒；可連續 3 次）
pub const POLL_MIN_INTERVAL_MS: u64 = 10_000;
