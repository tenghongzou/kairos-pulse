//! 註冊通道的列舉：錯誤碼（與產品代碼、HTTP 狀態碼的對照）、申請狀態、模式與請求分類。
//!
//! 對照表與 ENROLL.md「錯誤碼」節逐列相同，單元測試直接比對該節的表格。

use serde::{Deserialize, Serialize};

/// 註冊通道回應的 `error.code`（ENROLL.md「錯誤碼」）。
///
/// 收到不認得的值時不要讓整則回應解析失敗：回應型別的 `code` 是字串，以
/// [`EnrollErrorCode::from_code`] 轉換，`None` 即為不認得（Pulse 以 `PLS-W2107` 處理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollErrorCode {
    BadRequest,
    InvalidCsr,
    ClockSkew,
    ReplayedRequest,
    NonceInvalid,
    NonceExpired,
    TokenInvalid,
    TokenExpired,
    TokenExhausted,
    EnrollNotAccepted,
    RenewGraceExceeded,
    RenewIdentityMismatch,
    CertificateRevoked,
    LicenseExhausted,
    WithdrawSignatureInvalid,
    NotFound,
    RequestNotFound,
    MethodNotAllowed,
    RequestNotPending,
    RequestConflict,
    CertificateNotExpired,
    PayloadTooLarge,
    UnsupportedMediaType,
    RateLimited,
    SourceBlocked,
    Internal,
    EnrollBusy,
    PendingQueueFull,
    StorageUnavailable,
    ShuttingDown,
}

impl EnrollErrorCode {
    /// 全部的錯誤碼（依 ENROLL.md「錯誤碼」表的順序）
    pub const ALL: [EnrollErrorCode; 30] = [
        Self::BadRequest,
        Self::InvalidCsr,
        Self::ClockSkew,
        Self::ReplayedRequest,
        Self::NonceInvalid,
        Self::NonceExpired,
        Self::TokenInvalid,
        Self::TokenExpired,
        Self::TokenExhausted,
        Self::EnrollNotAccepted,
        Self::RenewGraceExceeded,
        Self::RenewIdentityMismatch,
        Self::CertificateRevoked,
        Self::LicenseExhausted,
        Self::WithdrawSignatureInvalid,
        Self::NotFound,
        Self::RequestNotFound,
        Self::MethodNotAllowed,
        Self::RequestNotPending,
        Self::RequestConflict,
        Self::CertificateNotExpired,
        Self::PayloadTooLarge,
        Self::UnsupportedMediaType,
        Self::RateLimited,
        Self::SourceBlocked,
        Self::Internal,
        Self::EnrollBusy,
        Self::PendingQueueFull,
        Self::StorageUnavailable,
        Self::ShuttingDown,
    ];

    /// 協定上的字串（snake_case）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::InvalidCsr => "invalid_csr",
            Self::ClockSkew => "clock_skew",
            Self::ReplayedRequest => "replayed_request",
            Self::NonceInvalid => "nonce_invalid",
            Self::NonceExpired => "nonce_expired",
            Self::TokenInvalid => "token_invalid",
            Self::TokenExpired => "token_expired",
            Self::TokenExhausted => "token_exhausted",
            Self::EnrollNotAccepted => "enroll_not_accepted",
            Self::RenewGraceExceeded => "renew_grace_exceeded",
            Self::RenewIdentityMismatch => "renew_identity_mismatch",
            Self::CertificateRevoked => "certificate_revoked",
            Self::LicenseExhausted => "license_exhausted",
            Self::WithdrawSignatureInvalid => "withdraw_signature_invalid",
            Self::NotFound => "not_found",
            Self::RequestNotFound => "request_not_found",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::RequestNotPending => "request_not_pending",
            Self::RequestConflict => "request_conflict",
            Self::CertificateNotExpired => "certificate_not_expired",
            Self::PayloadTooLarge => "payload_too_large",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::RateLimited => "rate_limited",
            Self::SourceBlocked => "source_blocked",
            Self::Internal => "internal",
            Self::EnrollBusy => "enroll_busy",
            Self::PendingQueueFull => "pending_queue_full",
            Self::StorageUnavailable => "storage_unavailable",
            Self::ShuttingDown => "shutting_down",
        }
    }

    /// 由協定字串轉換；不認得時 `None`
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == code)
    }

    /// 這個錯誤碼一律使用的 HTTP 狀態碼
    pub fn http_status(self) -> u16 {
        match self {
            Self::BadRequest
            | Self::InvalidCsr
            | Self::ClockSkew
            | Self::ReplayedRequest
            | Self::NonceInvalid
            | Self::NonceExpired => 400,
            Self::TokenInvalid
            | Self::TokenExpired
            | Self::TokenExhausted
            | Self::EnrollNotAccepted
            | Self::RenewGraceExceeded
            | Self::RenewIdentityMismatch
            | Self::CertificateRevoked
            | Self::LicenseExhausted
            | Self::WithdrawSignatureInvalid => 403,
            Self::NotFound | Self::RequestNotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::RequestNotPending | Self::RequestConflict | Self::CertificateNotExpired => 409,
            Self::PayloadTooLarge => 413,
            Self::UnsupportedMediaType => 415,
            Self::RateLimited | Self::SourceBlocked => 429,
            Self::Internal => 500,
            Self::EnrollBusy
            | Self::PendingQueueFull
            | Self::StorageUnavailable
            | Self::ShuttingDown => 503,
        }
    }

    /// Kairos 端記錄（稽核、事件、S20 計數）使用的產品代碼；沒有對應代碼時 `None`
    /// （設計包 `copywriting.md` §3.4）
    pub fn kairos_code(self) -> Option<&'static str> {
        Some(match self {
            Self::InvalidCsr => "KRS-E3304",
            Self::ClockSkew => "KRS-W8302",
            Self::TokenInvalid => "KRS-W3002",
            Self::TokenExpired => "KRS-W3003",
            Self::TokenExhausted => "KRS-W3004",
            Self::EnrollNotAccepted => "KRS-W2008",
            Self::RenewGraceExceeded => "KRS-W1113",
            Self::RenewIdentityMismatch => "KRS-W1114",
            Self::CertificateRevoked => "KRS-W1105",
            Self::LicenseExhausted => "KRS-E7101",
            Self::RateLimited => "KRS-W2202",
            Self::SourceBlocked => "KRS-W3005",
            Self::EnrollBusy => "KRS-W2205",
            Self::PendingQueueFull => "KRS-E3309",
            _ => return None,
        })
    }

    /// Pulse 端呈現的產品代碼；沒有對應代碼時 `None`（Pulse 依端點自行處理，見 ENROLL.md
    /// 「Pulse 的處理」）。`PLS-W2107` 為「Kairos 回應不合預期」
    pub fn pulse_code(self) -> Option<&'static str> {
        Some(match self {
            Self::BadRequest
            | Self::ReplayedRequest
            | Self::RequestConflict
            | Self::PayloadTooLarge
            | Self::UnsupportedMediaType
            | Self::Internal => "PLS-W2107",
            Self::InvalidCsr => "PLS-E3304",
            Self::ClockSkew => "PLS-E8302",
            Self::NonceInvalid | Self::NonceExpired | Self::WithdrawSignatureInvalid => "PLS-W3310",
            Self::TokenInvalid => "PLS-E3002",
            Self::TokenExpired => "PLS-E3003",
            Self::TokenExhausted => "PLS-E3004",
            Self::EnrollNotAccepted => "PLS-E2008",
            Self::RenewGraceExceeded => "PLS-E1113",
            Self::RenewIdentityMismatch => "PLS-E1114",
            Self::CertificateRevoked => "PLS-E1105",
            Self::LicenseExhausted => "PLS-E7101",
            Self::RateLimited | Self::EnrollBusy => "PLS-E2202",
            Self::SourceBlocked => "PLS-E3005",
            Self::PendingQueueFull => "PLS-E3309",
            _ => return None,
        })
    }

    /// Pulse 是否應在等待後重送同一個請求（暫時性的錯誤）。`clock_skew` 另有自己的重送規則
    /// （每 5 分鐘），不算在這裡
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimited
                | Self::SourceBlocked
                | Self::EnrollBusy
                | Self::PendingQueueFull
                | Self::StorageUnavailable
                | Self::ShuttingDown
                | Self::Internal
        )
    }
}

/// 註冊申請的狀態（`state`；ENROLL.md「申請的狀態機」）。
///
/// Kairos 內部另有「已核准、尚未簽發」，對外仍是 [`RequestState::PendingApproval`]，以
/// [`ApprovalDetail`] 區分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    PendingApproval,
    Issued,
    Rejected,
    RequestExpired,
    Withdrawn,
    Replaced,
}

impl RequestState {
    /// 已結束（不會再變化）
    pub fn is_final(self) -> bool {
        !matches!(self, Self::PendingApproval)
    }
}

/// 申請的種類（`kind`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// 以註冊 token 授權的新註冊（含撤銷後、`--force`、`--reset-identity` 的重新註冊）
    Enroll,
    /// 過期寬限期內以舊私鑰簽章的續簽
    GraceRenewal,
}

/// 待核准申請的細節（`approval_detail`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDetail {
    /// 等待管理者核准
    AwaitingApproval,
    /// 已核准，等待簽發（簽發金鑰離線保存的模式，或線上簽發暫時失敗）
    ApprovedAwaitingIssuance,
}

/// 註冊 token 的核准方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenApproval {
    Manual,
    Auto,
}

/// 註冊端點模式（`enroll.mode`；設計包 D8）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollMode {
    Open,
    RenewOnly,
    Closed,
}

impl EnrollMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::RenewOnly => "renew_only",
            Self::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Self::Open, Self::RenewOnly, Self::Closed]
            .into_iter()
            .find(|m| m.as_str() == s)
    }
}

/// 速率限制回應的 `scope`（設計包 `decisions/enroll-rate-limit.md` §5.1）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateScope {
    Ip,
    Token,
    Agent,
    Request,
    Download,
    Blocked,
    Global,
}

/// Pulse 如何確認連到的是真的 Kairos（設計包 `enrollment-ux.md` §6）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerVerification {
    /// 以安裝字串或 `--ca-pin` 的 CA 指紋
    CaPin,
    /// 以 `--ca-file` 或既有的 `identity/ca.pem`
    CaFile,
    /// trust-on-first-use（未驗證伺服器身分；主控台標示異常旗標）
    Tofu,
}

/// 撤回申請的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WithdrawReason {
    UninstallPurge,
    EnrollCancelled,
}

/// 撤回申請的發起者
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitiatedBy {
    Msi,
    InstallScript,
    PackageManager,
    Cli,
}

/// 簽章演算法（`signature_alg`）。v1 只有一種
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SignatureAlg {
    #[serde(rename = "ecdsa-p256-sha256")]
    EcdsaP256Sha256,
}

/// 8444 上的請求分類：決定模式白名單與速率限制的層級（ENROLL.md「模式與請求白名單」）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndpointClass {
    ServerInfo,
    TokenCheck,
    NewEnrollment,
    GraceRenewal,
    Poll,
    Withdraw,
    Download,
}

/// 速率限制的計數類別（設計包 `decisions/INTEGRATION.md` §2.3）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateClass {
    /// 新註冊類：L4、L5、L5b、L6、L7（token）
    NewEnrollment,
    /// 續簽類：L4r、L6r、L7（續簽）
    Renewal,
    /// 每筆申請的輪詢間隔（L10）；不計入 L4／L4r
    PerRequest,
    /// 下載（L11）
    Download,
}

impl EndpointClass {
    pub fn rate_class(self) -> RateClass {
        match self {
            Self::ServerInfo | Self::TokenCheck | Self::NewEnrollment => RateClass::NewEnrollment,
            Self::GraceRenewal => RateClass::Renewal,
            Self::Poll | Self::Withdraw => RateClass::PerRequest,
            Self::Download => RateClass::Download,
        }
    }
}

/// 模式白名單的判定結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeDecision {
    Accept,
    /// 回 403 `enroll_not_accepted`（下載為 403 純文字說明）
    NotAccepted,
    /// 模式為關閉：Kairos 不監聽 8444，請求不會到達
    NotListening,
}

/// 依註冊端點模式判定是否受理（設計包 `decisions/enroll-pause.md` §2.3）。
/// `new_enrollment_paused`：開放模式下「沒有有效 token 而自動暫停新註冊」的狀態；其他模式忽略。
/// 自動暫停與僅續簽對外回應相同，不透露目前有沒有有效 token。
pub fn mode_decision(
    mode: EnrollMode,
    new_enrollment_paused: bool,
    class: EndpointClass,
) -> ModeDecision {
    let accepts_new = mode == EnrollMode::Open && !new_enrollment_paused;
    match (mode, class) {
        (EnrollMode::Closed, _) => ModeDecision::NotListening,
        (_, EndpointClass::ServerInfo)
        | (_, EndpointClass::GraceRenewal)
        | (_, EndpointClass::Poll)
        | (_, EndpointClass::Withdraw) => ModeDecision::Accept,
        (_, EndpointClass::TokenCheck | EndpointClass::NewEnrollment | EndpointClass::Download) => {
            if accepts_new {
                ModeDecision::Accept
            } else {
                ModeDecision::NotAccepted
            }
        }
    }
}
