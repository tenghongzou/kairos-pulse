//! 註冊通道的請求與回應本文（JSON）。欄位與 ENROLL.md「端點」節逐一對應，單元測試以該文件
//! 「範例」節的每一則本文做往返（解析後再序列化，與原文的 JSON 值相同）。
//!
//! - 解析時忽略不認得的欄位（不使用 `deny_unknown_fields`），以便日後新增欄位。
//! - 選用欄位為 `None` 時不輸出；收到 `null` 視同省略。
//! - 列舉值（`state`、`kind` 等）不認得時整則解析失敗：新增列舉值必須以 `enroll_features` 宣告，
//!   由 client 表明支援後才使用（ENROLL.md「版本與相容」）。

use serde::{Deserialize, Serialize};

use super::codes::{
    ApprovalDetail, EnrollErrorCode, InitiatedBy, RateScope, RequestKind, RequestState,
    ServerVerification, SignatureAlg, TokenApproval, WithdrawReason,
};
use super::crypto::RequestCode;
use crate::OsInfo;

/// `GET /v1/enroll/server-info` 的回應
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub server_time: String,
    /// Kairos 的版本，例 `0.5.0`
    pub kairos_version: String,
    /// 支援的註冊 API 版本（路徑前綴 `/v1/enroll` 的 `v1`）
    pub enroll_api: Vec<String>,
    /// 註冊 API 的選用功能（ENROLL.md「enroll_features」）
    pub enroll_features: Vec<String>,
    /// 8443 支援的協定版本範圍（PROTOCOL.md「版本協商」），供 preflight PF07 在註冊前判斷相容性
    pub proto_min: u32,
    pub proto_max: u32,
    /// 8443 的 features（`hello_ack.features` 的同一份清單）
    pub features: Vec<String>,
    /// 目前受理的請求種類（自動暫停與僅續簽對外相同，不透露有沒有有效 token）
    pub accepts: Accepts,
    /// Kairos 看到的來源位址（通常是 NAT 出口）
    pub observed_ip: String,
}

/// `server-info` 的 `accepts`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepts {
    pub new_enrollment: bool,
    pub grace_renewal: bool,
}

/// `POST /v1/enroll/token-check` 的請求（不消耗使用次數）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCheckRequest {
    pub token: String,
    /// 只記錄、不判定時鐘（設計包 §24 #23 (a)）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pulse_version: Option<String>,
}

/// `token-check` 的回應（只有有效的 token 回 200；無效、過期、用完回錯誤）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCheckResponse {
    pub server_time: String,
    pub token: TokenStatus,
    pub observed_ip: String,
}

/// 有效 token 的資訊（preflight PF09 顯示「有效，剩餘 37 次，2026-10-16 到期」）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenStatus {
    pub name: String,
    pub approval: TokenApproval,
    pub expires_at: String,
    pub remaining_uses: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tags: Option<Vec<String>>,
    /// 來源位址是否在 token 的限定網段內；token 沒有限定網段時省略
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_allowed: Option<bool>,
}

/// `POST /v1/enroll/requests` 的請求（新註冊）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub token: String,
    pub agent_id: String,
    /// PKCS#10 CSR 的 DER，base64url（不加 `=`）
    pub csr: String,
    /// Pulse 產生的輪詢憑據：32 bytes 隨機值的 base64url。Kairos 只保存 SHA-256
    pub poll_secret: String,
    /// Pulse 送出時的本機時間（RFC 3339），Kairos 據此做時鐘檢查
    pub sent_at: String,
    pub hostname: String,
    pub pulse_version: String,
    pub server_verification: ServerVerification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<OsInfo>,
    /// 重新註冊時前一張憑證的序號（撤銷後重新註冊、`--force`；設計包 §5.7）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_serial: Option<String>,
}

/// `POST /v1/enroll/renewals` 的請求（過期寬限期續簽；不帶 token，以舊私鑰簽章）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraceRenewRequest {
    pub agent_id: String,
    /// 新私鑰的 CSR（DER，base64url）
    pub csr: String,
    pub poll_secret: String,
    pub sent_at: String,
    pub host_fingerprint: String,
    /// 已過期的用戶端憑證（DER，base64url）
    pub expired_certificate: String,
    /// 以舊私鑰對 [`grace_renew_signing_input`](super::grace_renew_signing_input) 的簽章
    /// （ASN.1 DER，base64url）
    pub signature: String,
    pub signature_alg: SignatureAlg,
    pub hostname: String,
    pub pulse_version: String,
}

/// 新註冊、過期續簽與輪詢的回應（HTTP 200）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollResult {
    pub server_time: String,
    pub request_id: String,
    pub request_code: RequestCode,
    pub kind: RequestKind,
    pub state: RequestState,
    pub agent_id: String,
    /// `pending_approval`：申請的有效期限
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// `pending_approval`：下次輪詢前至少等待的毫秒數（Pulse 一律遵守）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_detail: Option<ApprovalDetail>,
    /// `issued`：簽發的憑證
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<IssuedCertificate>,
    /// `issued`：憑證政策（Pulse 快取寬限天數）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<CertPolicy>,
    /// `issued`、`rejected`：核准或拒絕的時間
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    /// `issued`、`rejected`：核准者或拒絕者（`auto`：token 免審核；`system`：過期續簽自動核准）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    /// `rejected`：拒絕原因（管理者填寫的文字）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_ip: Option<String>,
}

/// 簽發的憑證
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedCertificate {
    /// 序號（大寫十六進位）
    pub serial: String,
    pub not_before: String,
    pub not_after: String,
    /// 用戶端憑證（PEM，一張）
    pub client_cert_pem: String,
    /// 簽發 CA 到信任錨之間的中繼憑證（PEM，可為空字串）。Pulse 寫入 `client.pem` 時接在用戶端憑證之後
    pub chain_pem: String,
    /// Pulse 驗證 Kairos 伺服器憑證用的 CA（PEM）；必須與 CA 指紋或既有的 `ca.pem` 相符
    pub ca_pem: String,
}

/// 憑證政策（設計包 D2、S14）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertPolicy {
    pub cert_lifetime_days: u32,
    pub renew_before_days: u32,
    pub expired_renew_grace_days: u32,
}

/// `POST /v1/enroll/requests/{request_id}/withdraw/challenge` 的回應
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawChallenge {
    pub server_time: String,
    pub request_id: String,
    /// 32 bytes 隨機值的 base64url；60 秒內有效、只能用一次
    pub nonce: String,
    pub nonce_expires_at: String,
}

/// `POST /v1/enroll/requests/{request_id}/withdraw` 的請求
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawRequest {
    pub request_id: String,
    pub agent_id: String,
    pub nonce: String,
    pub signature: String,
    pub signature_alg: SignatureAlg,
    pub reason: WithdrawReason,
    pub initiated_by: InitiatedBy,
    pub hostname: String,
    pub pulse_version: String,
    pub sent_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_fingerprint: Option<String>,
}

/// 撤回成功（含已撤回過）的回應
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawResult {
    pub server_time: String,
    pub request_id: String,
    pub request_code: RequestCode,
    pub state: RequestState,
    pub withdrawn_at: String,
    pub already_withdrawn: bool,
}

/// 錯誤回應（所有 4xx、5xx）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub server_time: String,
    pub error: ErrorBody,
}

/// 錯誤本文。`code` 以外的欄位依錯誤碼而定（ENROLL.md「錯誤碼」的「附加欄位」）
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    /// `bad_request`、`invalid_csr`：哪一個欄位
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// `rate_limited`、`source_blocked`、`enroll_busy`、`pending_queue_full`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<RateScope>,
    /// 與 `Retry-After` 標頭相同的等待時間（毫秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_ip: Option<String>,
    /// `clock_skew`：回顯 `sent_at`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_time: Option<String>,
    /// `clock_skew`：`sent_at` − 伺服器收到請求的時間（負值＝主機較慢）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_ms: Option<i64>,
    /// `clock_skew`：門檻（300000）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_skew_ms: Option<i64>,
    /// `request_not_pending`：申請目前的狀態
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<RequestState>,
    /// `token_expired`：到期時間
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired_at: Option<String>,
    /// `renew_grace_exceeded`：寬限期結束的時間
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_ended_at: Option<String>,
}

impl ErrorBody {
    /// 只有錯誤碼與說明的本文
    pub fn new(code: EnrollErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().to_string(),
            message: message.into(),
            ..Self::default()
        }
    }

    /// 錯誤碼；不認得時 `None`
    pub fn known_code(&self) -> Option<EnrollErrorCode> {
        EnrollErrorCode::from_code(&self.code)
    }
}
