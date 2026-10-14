//! 除役通知（feature `deregister`；`kairos-proto` 的 PROTOCOL.md「憑證輪替與除役」、設計包
//! `enrollment-ux.md` §5.10）。訊息是 `kairos-proto` 的共用型別（`ClientMsg::Deregister`、
//! `ServerMsg::DeregisterAck`）。
//!
//! 完全移除時、刪除私鑰之前，經 8443 以這台主機的用戶端憑證送出 `hello`（`intent: "deregister"`）
//! 與 `deregister`。每次嘗試的握手與等待各 5 秒；可重試的失敗在 2 秒後重試 1 次；整體最多 15 秒。
//! 失敗時回報 `PLS-W3107`（`reason`：`unreachable`／`unsupported`／`cert_expired`／`cert_rejected`／
//! `server_error`），本機照常清除（由安裝程式負責）。

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::codes;
use crate::config_manager::AppConfig;
use crate::network;
use kairos_proto::lifecycle::DEREGISTER_REASON_UNINSTALL_PURGE;
use kairos_proto::{
    ClientMsg, Deregister, DeregisterAck, FEATURE_DEREGISTER, INTENT_DEREGISTER, ServerMsg,
    parse_server_msg,
};

/// 整體時限
const TOTAL: Duration = Duration::from_secs(15);
/// 等待 `deregister_ack`
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// 除役通知的結果（`install-result` 的 `deregistration`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deregistration {
    /// `ok`／`unreported`／`skipped`
    pub result: &'static str,
    pub code: Option<&'static str>,
    pub reason: Option<&'static str>,
    pub cause_code: Option<&'static str>,
    pub protocol_code: Option<String>,
    pub certificate_revoked: Option<bool>,
    pub already_decommissioned: Option<bool>,
    pub attempts: u32,
}

impl Deregistration {
    fn ok(ack: &DeregisterAck, attempts: u32) -> Self {
        Self {
            result: "ok",
            code: None,
            reason: None,
            cause_code: None,
            protocol_code: None,
            certificate_revoked: Some(ack.certificate_revoked),
            already_decommissioned: Some(ack.already_decommissioned),
            attempts,
        }
    }

    fn unreported(reason: &'static str, cause: Option<&'static str>, attempts: u32) -> Self {
        Self {
            result: "unreported",
            code: Some(codes::W_DEREGISTER_UNREPORTED),
            reason: Some(reason),
            cause_code: cause,
            protocol_code: None,
            certificate_revoked: None,
            already_decommissioned: None,
            attempts,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "result": self.result,
            "code": self.code,
            "reason": self.reason,
            "cause_code": self.cause_code,
            "protocol_code": self.protocol_code,
            "certificate_revoked": self.certificate_revoked,
            "already_decommissioned": self.already_decommissioned,
        })
    }
}

/// 一次嘗試的結果：完成或可重試
enum Attempt {
    Done(Deregistration),
    Retry(&'static str, Option<&'static str>),
}

/// 送出除役通知
pub async fn deregister(config: &AppConfig, base: &Path, initiated_by: &str) -> Deregistration {
    let mut server = config.server.clone();
    for p in [
        &mut server.client_cert,
        &mut server.client_key,
        &mut server.ca_cert,
    ] {
        if !p.trim().is_empty() {
            *p = base.join(p.trim()).display().to_string();
        }
    }
    let tls = match crate::tls::build(&server) {
        Ok(t) => t,
        Err(_) => {
            return Deregistration::unreported("cert_rejected", Some(codes::E_CLIENT_CERT_LOAD), 0);
        }
    };
    // 本機憑證已過期：不送（無法完成 8443 握手），不改走 8444
    if tls
        .client_cert_info()
        .and_then(|i| i.not_after)
        .is_some_and(|na| na < chrono::Utc::now())
    {
        let d = Deregistration::unreported("cert_expired", Some(codes::E_CLIENT_CERT_EXPIRED), 0);
        log_unreported(&d);
        return d;
    }
    let id_path = base.join(config.agent.id_file.trim());
    let agent_id = match crate::agent_id::load_or_create(&id_path) {
        Ok((id, _)) => id,
        Err(_) => return Deregistration::unreported("server_error", Some(codes::E_AGENT_ID_IO), 0),
    };
    let hostname = crate::system_info::hostname();
    let settings = network::Settings::new(config, agent_id.clone(), hostname.clone());
    let request_id = crate::enroll::keys::new_request_uuid();
    let deadline = Instant::now() + TOTAL;
    let mut attempts = 0;
    let result = loop {
        attempts += 1;
        let one = tokio::time::timeout_at(
            deadline,
            attempt(
                &settings,
                &tls,
                &request_id,
                &agent_id,
                &hostname,
                initiated_by,
                attempts,
            ),
        )
        .await;
        match one {
            Err(_) => {
                break Deregistration::unreported(
                    "unreachable",
                    Some(codes::E_SERVER_SILENT),
                    attempts,
                );
            }
            Ok(Attempt::Done(d)) => break d,
            Ok(Attempt::Retry(reason, cause)) => {
                if attempts >= 2 || Instant::now() + Duration::from_secs(2) >= deadline {
                    break Deregistration::unreported(reason, cause, attempts);
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    };
    if result.result == "unreported" {
        log_unreported(&result);
    } else {
        log::info!(
            "已通知 Kairos 這台主機已除役（agent_id {agent_id}，憑證{}撤銷）",
            if result.certificate_revoked == Some(true) {
                "已"
            } else {
                "未"
            }
        );
    }
    result
}

fn log_unreported(d: &Deregistration) {
    log::warn!(
        "{}無法通知 Kairos 這台主機已除役  reason={} cause_code={}",
        codes::tag(
            codes::INSTALL_DECOMMISSION_UNREPORTED,
            Some(codes::W_DEREGISTER_UNREPORTED)
        ),
        d.reason.unwrap_or("-"),
        d.cause_code.unwrap_or("-")
    );
}

async fn attempt(
    settings: &network::Settings,
    tls: &crate::tls::TlsSetup,
    request_id: &str,
    agent_id: &str,
    hostname: &str,
    initiated_by: &str,
    attempts: u32,
) -> Attempt {
    let features = vec![FEATURE_DEREGISTER.to_string()];
    let mut conn = match network::one_shot(settings, tls, features, Some(INTENT_DEREGISTER)).await {
        Ok(c) => c,
        Err(e) => {
            return match e.code {
                Some(
                    c @ (codes::E_CLIENT_CERT_REJECTED
                    | codes::E_CLIENT_CERT_UNTRUSTED
                    | codes::E_CLIENT_CERT_EXPIRED),
                ) => Attempt::Done(Deregistration::unreported(
                    "cert_rejected",
                    Some(c),
                    attempts,
                )),
                _ if e.retryable => Attempt::Retry("unreachable", e.code),
                Some(c) => Attempt::Done(Deregistration::unreported(
                    "server_error",
                    Some(c),
                    attempts,
                )),
                None => Attempt::Done(Deregistration::unreported("server_error", None, attempts)),
            };
        }
    };
    if !conn.supports(FEATURE_DEREGISTER) {
        conn.close().await;
        return Attempt::Done(Deregistration::unreported("unsupported", None, attempts));
    }
    let seq = conn.next_seq();
    let ts = kairos_proto::enroll::format_rfc3339_ms(chrono::Utc::now().timestamp_millis());
    let msg = Deregister {
        seq,
        ts: ts.clone(),
        request_id: request_id.to_string(),
        agent_id: agent_id.to_string(),
        reason: DEREGISTER_REASON_UNINSTALL_PURGE.into(),
        initiated_by: initiated_by.to_string(),
        host_fingerprint: settings.host.host_fingerprint.clone(),
        hostname: hostname.to_string(),
        pulse_version: env!("CARGO_PKG_VERSION").to_string(),
        sent_at: ts,
    };
    if conn
        .send(&ClientMsg::Deregister(Box::new(msg)))
        .await
        .is_err()
    {
        return Attempt::Retry("unreachable", Some(codes::E_SERVER_SILENT));
    }
    let deadline = Instant::now() + ACK_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = match conn.next_line(left).await {
            Ok(l) => l,
            Err(_) => return Attempt::Retry("unreachable", Some(codes::E_SERVER_SILENT)),
        };
        let msg = parse_server_msg(&line);
        if let Ok(ServerMsg::DeregisterAck(ack)) = &msg
            && ack.request_id == request_id
        {
            conn.close().await;
            return Attempt::Done(Deregistration::ok(ack, attempts));
        }
        if let Ok(ServerMsg::Error { code, .. }) = msg {
            conn.close().await;
            if code == "storage_unavailable" {
                return Attempt::Retry("server_error", None);
            }
            let mut d = Deregistration::unreported(
                if code == "auth_failed" {
                    "cert_rejected"
                } else {
                    "server_error"
                },
                None,
                attempts,
            );
            d.protocol_code = Some(crate::network::sanitize_for_log(&code, 64));
            return Attempt::Done(d);
        }
    }
}
