//! 8443 上的正常輪替（feature `renew`）與除役通知（feature `deregister`）的訊息
//! （ENROLL.md 第 13 節，草稿）。
//!
//! **暫時定義在 Pulse**：規格要求實作時把第 13 節移入 PROTOCOL.md，並在 `kairos-proto` 的
//! `ClientMsg`／`ServerMsg` 新增變體。Kairos 端與本衝刺平行開發，為了不在共用 crate 產生衝突，
//! Pulse 先以這裡的型別送出與解析（欄位與第 13 節逐一相同）；合併時改用 `kairos-proto` 的型別、
//! 刪除本檔（見 Pulse README「與 Kairos 端合併」）。
//!
//! - `kairos_proto::parse_server_msg` 把 `renew_result`、`deregister_ack` 解析為「未知的訊息類型」，
//!   由 [`parse_incoming`] 另外解析。
//! - `hello_ack` 的 `expired_renew_grace_days`、`renew_now` 以 [`HelloAckExtra`] 從同一行讀出。
//! - `hello` 的 `intent` 由 [`hello_with_intent`] 加進序列化後的 JSON。

use kairos_proto::ClientMsg;
use kairos_proto::enroll::{CertPolicy, IssuedCertificate};
use serde::{Deserialize, Serialize};

/// 正常輪替
pub const FEATURE_RENEW: &str = "renew";
/// 除役通知
pub const FEATURE_DEREGISTER: &str = "deregister";
/// `hello.intent`：這條連線只送除役通知
pub const INTENT_DEREGISTER: &str = "deregister";

/// `renew_request`（Pulse → Kairos）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "renew_request")]
pub struct RenewRequest {
    pub seq: u64,
    pub ts: String,
    /// 同一個 CSR 重送時沿用
    pub request_id: String,
    /// base64url DER；SAN 等於這條連線的 `agent_id`，公鑰與目前的不同
    pub csr: String,
    pub sent_at: String,
}

/// `deregister`（Pulse → Kairos）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "deregister")]
pub struct Deregister {
    pub seq: u64,
    pub ts: String,
    pub request_id: String,
    pub agent_id: String,
    /// `uninstall_purge`
    pub reason: String,
    /// `msi`／`install_script`／`package_manager`
    pub initiated_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_fingerprint: Option<String>,
    pub hostname: String,
    pub pulse_version: String,
    pub sent_at: String,
}

/// `renew_result`（Kairos → Pulse）
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RenewResult {
    #[serde(default)]
    pub seq: Option<u64>,
    pub request_id: String,
    pub state: String,
    pub certificate: IssuedCertificate,
    #[serde(default)]
    pub policy: Option<CertPolicy>,
    #[serde(default)]
    pub server_time: Option<String>,
}

/// `deregister_ack`（Kairos → Pulse）
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeregisterAck {
    #[serde(default)]
    pub seq: Option<u64>,
    pub request_id: String,
    pub agent_id: String,
    pub state: String,
    #[serde(default)]
    pub decommissioned_at: Option<String>,
    #[serde(default)]
    pub certificate_revoked: bool,
    #[serde(default)]
    pub already_decommissioned: bool,
}

/// `hello_ack` 中第 13 節新增的欄位（雙方協商了 `renew` 時才有）
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct HelloAckExtra {
    #[serde(default)]
    pub expired_renew_grace_days: Option<u32>,
    #[serde(default)]
    pub renew_now: Option<bool>,
}

impl HelloAckExtra {
    /// 從 `hello_ack` 那一行讀出（欄位不存在或型別不符時為預設值）
    pub fn from_line(line: &str) -> Self {
        serde_json::from_str(line).unwrap_or_default()
    }
}

/// 第 13 節的 Kairos → Pulse 訊息
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    RenewResult(Box<RenewResult>),
    DeregisterAck(DeregisterAck),
}

/// 解析 `kairos-proto` 不認得的訊息類型；不是第 13 節的訊息時 `None`
pub fn parse_incoming(line: &str) -> Option<Incoming> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(rename = "type")]
        kind: String,
    }
    match serde_json::from_str::<Probe>(line).ok()?.kind.as_str() {
        "renew_result" => serde_json::from_str(line)
            .ok()
            .map(|r| Incoming::RenewResult(Box::new(r))),
        "deregister_ack" => serde_json::from_str(line).ok().map(Incoming::DeregisterAck),
        _ => None,
    }
}

/// 序列化 `hello`，並加上 `intent`
pub fn hello_with_intent(hello: &ClientMsg, intent: Option<&str>) -> serde_json::Result<String> {
    let mut v = serde_json::to_value(hello)?;
    if let (Some(intent), Some(obj)) = (intent, v.as_object_mut()) {
        obj.insert(
            "intent".into(),
            serde_json::Value::String(intent.to_string()),
        );
    }
    serde_json::to_string(&v)
}

/// 用戶端憑證是否到了正常輪替的時點（設計包 `enrollment-ux.md` §8）：已使用壽命的 2/3，或剩餘
/// ≤ `renew_before_days`，取較早者。`now_ms` 為（以時鐘偏差校正後的）目前時間
pub fn rotation_due(
    not_before_ms: i64,
    not_after_ms: i64,
    now_ms: i64,
    renew_before_days: u32,
) -> bool {
    let lifetime = not_after_ms.saturating_sub(not_before_ms).max(0);
    let two_thirds = not_before_ms.saturating_add(lifetime / 3 * 2);
    let before = if renew_before_days == 0 {
        i64::MAX
    } else {
        not_after_ms.saturating_sub(i64::from(renew_before_days) * 86_400_000)
    };
    now_ms >= two_thirds.min(before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_section_13_fields() {
        let r = RenewRequest {
            seq: 7,
            ts: "2026-10-11T02:13:05.000Z".into(),
            request_id: "0d5f7b2e-3c41-4a8e-9b6f-2e7c1d9a4b30".into(),
            csr: "MIIB".into(),
            sent_at: "2026-10-11T02:13:05.000Z".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["type"], "renew_request");
        assert_eq!(v["seq"], 7);
        let d = Deregister {
            seq: 2,
            ts: String::new(),
            request_id: "x".into(),
            agent_id: "a".into(),
            reason: "uninstall_purge".into(),
            initiated_by: "msi".into(),
            host_fingerprint: None,
            hostname: "h".into(),
            pulse_version: "0.5.0".into(),
            sent_at: String::new(),
        };
        let v: serde_json::Value = serde_json::to_value(&d).unwrap();
        assert_eq!(v["type"], "deregister");
        assert!(v.get("host_fingerprint").is_none());

        let line = r#"{"type":"deregister_ack","seq":2,"request_id":"x","agent_id":"a","state":"decommissioned","decommissioned_at":"2026-10-11T02:13:05.000Z","certificate_revoked":true,"already_decommissioned":false}"#;
        assert!(
            matches!(parse_incoming(line), Some(Incoming::DeregisterAck(a)) if a.certificate_revoked)
        );
        let line = r#"{"type":"renew_result","seq":3,"request_id":"x","state":"issued","certificate":{"serial":"01","not_before":"","not_after":"","client_cert_pem":"","chain_pem":"","ca_pem":""},"policy":{"cert_lifetime_days":365,"renew_before_days":30,"expired_renew_grace_days":90},"server_time":"2026-10-11T02:13:05.000Z"}"#;
        assert!(
            matches!(parse_incoming(line), Some(Incoming::RenewResult(r)) if r.state == "issued")
        );
        assert_eq!(parse_incoming(r#"{"type":"something_new"}"#), None);
        assert_eq!(parse_incoming("not json"), None);

        let extra = HelloAckExtra::from_line(
            r#"{"type":"hello_ack","seq":1,"proto":2,"offline_after_ms":75000,"expired_renew_grace_days":60,"renew_now":true}"#,
        );
        assert_eq!(extra.expired_renew_grace_days, Some(60));
        assert_eq!(extra.renew_now, Some(true));
        assert_eq!(
            HelloAckExtra::from_line(r#"{"type":"hello_ack"}"#),
            HelloAckExtra::default()
        );
    }

    #[test]
    fn hello_intent_is_added_to_the_json() {
        let hello = ClientMsg::Heartbeat {
            seq: 1,
            ts: "t".into(),
        };
        let line = hello_with_intent(&hello, Some(INTENT_DEREGISTER)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["intent"], "deregister");
        let line = hello_with_intent(&hello, None).unwrap();
        assert!(!line.contains("intent"));
    }

    #[test]
    fn rotation_is_due_at_two_thirds_or_thirty_days_left() {
        let day = 86_400_000i64;
        // 365 天的憑證：2/3 在第 243 天；剩 30 天在第 335 天 → 取較早者 243
        assert!(!rotation_due(0, 365 * day, 243 * day, 30));
        assert!(rotation_due(0, 365 * day, 244 * day, 30));
        // 60 天的憑證：2/3 在第 40 天，剩 30 天在第 30 天 → 30
        assert!(!rotation_due(0, 60 * day, 29 * day, 30));
        assert!(rotation_due(0, 60 * day, 30 * day, 30));
        // renew_before_days = 0：只看 2/3
        assert!(!rotation_due(0, 60 * day, 30 * day, 0));
        assert!(rotation_due(0, 60 * day, 40 * day, 0));
    }
}
