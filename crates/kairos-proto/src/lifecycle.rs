//! 8443 上的憑證正常輪替（feature [`FEATURE_RENEW`]）與除役通知（feature [`FEATURE_DEREGISTER`]）的
//! 訊息內容（PROTOCOL.md「憑證輪替與除役」；原為 ENROLL.md 第 13 節的草稿）。
//!
//! 這些結構是 [`ClientMsg`](crate::ClientMsg)／[`ServerMsg`](crate::ServerMsg) 新變體的內容
//! （`ClientMsg::RenewRequest`、`ClientMsg::Deregister`、`ServerMsg::RenewResult`、
//! `ServerMsg::DeregisterAck`），`type` 欄位由外層的 enum 加上。兩個 feature 都只在協商結果為 v2、
//! 且雙方都宣告時使用：沒有宣告的舊版 Pulse 不會收到 `renew_result`／`deregister_ack`，
//! 舊版 Kairos 收到 `renew_request`／`deregister` 時照舊回 `unknown_type`。

use serde::{Deserialize, Serialize};

use crate::enroll::{CertPolicy, IssuedCertificate};

/// 正常輪替：憑證仍有效時經既有的 mTLS 連線送出新的 CSR
pub const FEATURE_RENEW: &str = "renew";
/// 除役通知：主機完全移除時經 8443 通知 Kairos
pub const FEATURE_DEREGISTER: &str = "deregister";
/// 8443 的註冊相關 feature。Kairos 只在 client 也宣告時才在 `hello_ack` 列出（沒有宣告的 client
/// 收到的 `hello_ack` 與 0.4 的 Kairos 完全相同）
pub const LIFECYCLE_FEATURES: [&str; 2] = [FEATURE_RENEW, FEATURE_DEREGISTER];
/// `hello.intent`：這條連線只送除役通知（不算上線、不更新最後連線時間、不取代既有連線）
pub const INTENT_DEREGISTER: &str = "deregister";
/// `deregister.reason`：安裝程式完全移除
pub const DEREGISTER_REASON_UNINSTALL_PURGE: &str = "uninstall_purge";
/// `renew_result.state`
pub const RENEW_STATE_ISSUED: &str = "issued";
/// `deregister_ack.state`
pub const DEREGISTER_STATE_DECOMMISSIONED: &str = "decommissioned";

/// `renew_request`（Pulse → Kairos）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewRequest {
    pub seq: u64,
    pub ts: String,
    /// UUID；同一個 CSR 重送時沿用
    pub request_id: String,
    /// base64url（不加 `=`）的 CSR DER；SAN 必須等於這條連線的 `agent_id`，新公鑰必須與目前的不同
    pub csr: String,
    /// RFC 3339 UTC 毫秒（只記錄；連線本身已以 mTLS 證明身分）
    pub sent_at: String,
}

/// `deregister`（Pulse → Kairos），只在 `intent: "deregister"` 的連線上送出
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deregister {
    pub seq: u64,
    pub ts: String,
    /// UUID；重送時沿用
    pub request_id: String,
    /// 必須等於用戶端憑證的 SAN（否則 `auth_failed`）
    pub agent_id: String,
    /// [`DEREGISTER_REASON_UNINSTALL_PURGE`]
    pub reason: String,
    /// `msi`／`install_script`／`package_manager`／`cli`
    pub initiated_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_fingerprint: Option<String>,
    pub hostname: String,
    pub pulse_version: String,
    pub sent_at: String,
}

/// `renew_result`（Kairos → Pulse）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewResult {
    /// 回應的 `renew_request` 的 `seq`
    pub seq: u64,
    pub request_id: String,
    /// [`RENEW_STATE_ISSUED`]
    pub state: String,
    pub certificate: IssuedCertificate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<CertPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_time: Option<String>,
}

/// `deregister_ack`（Kairos → Pulse）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeregisterAck {
    /// 回應的 `deregister` 的 `seq`
    pub seq: u64,
    pub request_id: String,
    pub agent_id: String,
    /// [`DEREGISTER_STATE_DECOMMISSIONED`]
    pub state: String,
    pub decommissioned_at: String,
    #[serde(default)]
    pub certificate_revoked: bool,
    /// 先前已除役（重送）
    #[serde(default)]
    pub already_decommissioned: bool,
}
