//! 8443 上與註冊有關的訊息（`crates/kairos-proto/ENROLL.md` 第 13 節）：正常輪替 `renew`、
//! 除役 `deregister`，以及輪替後撤銷舊憑證（新序號第一次成功 `hello` 時）。
//!
//! 這兩個 feature 的訊息型別目前定義在 Kairos 端（[`RenewRequest`]、[`Deregister`]），回應以 JSON
//! 組成後直接送出：`kairos-proto` 的 `ServerMsg` 加入新變體會讓 Pulse 端的 `match` 無法編譯，
//! 共用型別待兩端合併時移入 `kairos-proto`（見 PKI.md 第 12 節「與 Pulse 端合併時」）。

use kairos_proto::enroll::{b64url_decode, format_rfc3339_ms};
use log::{info, warn};
use rusqlite::TransactionBehavior;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::EnrollService;
use super::pki_mgr::{IssueError, IssueOutcome};
use super::store::{self, Audit, DbKind, DbState, RequestRow};
use crate::pki::csr::check_csr;
use crate::pki::issue::Reason;
use crate::pki::store::{self as ps, CertRow, Revoke};
use crate::pki::x509::pem_encode;

/// 8443 的 feature：憑證仍有效時經既有的 mTLS 連線輪替
pub const FEATURE_RENEW: &str = "renew";
/// 8443 的 feature：主機自行除役
pub const FEATURE_DEREGISTER: &str = "deregister";
/// `hello.intent` 的除役值
pub const INTENT_DEREGISTER: &str = "deregister";

/// `renew_request`（Pulse → Kairos）
#[derive(Debug, Clone, Deserialize)]
pub struct RenewRequest {
    pub seq: u64,
    pub request_id: String,
    pub csr: String,
    /// 只記錄、不判定時鐘（連線本身已以 mTLS 證明身分）
    #[serde(default)]
    #[allow(dead_code)]
    pub sent_at: Option<String>,
}

/// `deregister`（Pulse → Kairos）
#[derive(Debug, Clone, Deserialize)]
pub struct Deregister {
    #[serde(default)]
    pub seq: Option<u64>,
    pub request_id: String,
    pub agent_id: String,
    pub reason: String,
    #[serde(default)]
    pub initiated_by: Option<String>,
    #[serde(default)]
    pub host_fingerprint: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub pulse_version: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub sent_at: Option<String>,
}

/// 一行 client 訊息的 `type` 與 hello 的 `intent`（解析失敗時 `None`）
pub fn probe(line: &str) -> (Option<String>, Option<String>) {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(rename = "type")]
        kind: Option<Value>,
        intent: Option<Value>,
    }
    match serde_json::from_str::<Probe>(line) {
        Ok(p) => (
            p.kind.and_then(|v| v.as_str().map(str::to_string)),
            p.intent.and_then(|v| v.as_str().map(str::to_string)),
        ),
        Err(_) => (None, None),
    }
}

/// 8443 上的錯誤（`error` 訊息的 `code`、`message` 與是否 fatal）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    pub code: &'static str,
    pub message: String,
    pub fatal: bool,
}

impl AgentError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fatal: false,
        }
    }

    /// `error` 訊息的一行 JSON
    pub fn line(&self, seq: Option<u64>) -> String {
        json!({
            "type": "error",
            "seq": seq,
            "code": self.code,
            "message": self.message,
            "fatal": self.fatal,
        })
        .to_string()
    }
}

fn storage() -> AgentError {
    AgentError::new("storage_unavailable", "server could not store the request")
}

fn certificate_json(svc: &EnrollService, c: &CertRow) -> Option<Value> {
    let ca = svc.pki.server_ca()?;
    Some(json!({
        "serial": c.serial,
        "not_before": format_rfc3339_ms(c.not_before),
        "not_after": format_rfc3339_ms(c.not_after),
        "client_cert_pem": pem_encode("CERTIFICATE", &c.cert_der),
        "chain_pem": "",
        "ca_pem": ca.pem,
    }))
}

/// `hello_ack` 另帶的欄位（雙方協商了 `renew` 時）：`expired_renew_grace_days`、
/// 以及以非啟用中的簽發 CA 簽發的憑證要求提早輪替的 `renew_now`
pub fn hello_ack_extra(
    svc: &EnrollService,
    client_cert: Option<&[u8]>,
) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "expired_renew_grace_days".into(),
        json!(svc.effective().policy.expired_renew_grace_days),
    );
    if let (super::pki_mgr::IssueStatus::Online { ca_id }, Some(der)) =
        (svc.pki.status(), client_cert)
        && let Ok(info) = crate::pki::x509::parse_cert(der)
    {
        let active_subject = svc
            .db
            .read_blocking(|conn| Ok(ps::get_ca(conn, ca_id)?.and_then(|c| c.cert_der)))
            .ok()
            .flatten()
            .and_then(|der| crate::pki::x509::parse_cert(&der).ok())
            .map(|i| i.subject_raw);
        let issued_by_kairos = svc
            .db
            .read_blocking(|conn| Ok(ps::get_cert(conn, &info.serial)?.is_some()))
            .unwrap_or(false);
        if issued_by_kairos && active_subject.is_some_and(|s| s != info.issuer_raw) {
            m.insert("renew_now".into(), json!(true));
        }
    }
    m
}

/// 新序號第一次成功 `hello`：寫入 `first_used_at`，並依政策以 `superseded` 撤銷同一台主機的舊憑證
/// （背景執行，不延遲 `hello_ack`）
pub fn on_hello(svc: &Arc<EnrollService>, agent_id: &str, serial: &str) {
    if !svc.first_seen_serial(serial) {
        return;
    }
    let svc = svc.clone();
    let agent = agent_id.to_string();
    let serial = serial.to_string();
    tokio::task::spawn_blocking(move || {
        let now = svc.now_ms();
        let revoke_old = svc.effective().policy.revoke_superseded;
        let result = svc.db.write_blocking(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if ps::get_cert(&tx, &serial)?.is_none() || !ps::mark_first_used(&tx, &serial, now)? {
                return Ok(Vec::new());
            }
            let mut revoked = Vec::new();
            if revoke_old {
                for old in ps::older_unrevoked(&tx, &agent, &serial)? {
                    if let Revoke::Now(_) =
                        ps::revoke(&tx, &old, now, now, Reason::Superseded, "system", Some("新憑證已開始使用"))?
                    {
                        store::audit(
                            &tx,
                            now,
                            &Audit {
                                actor: "system",
                                category: "pki",
                                action: "cert.revoke",
                                object: Some(&old),
                                result: "ok",
                                details: Some(json!({"reason": "superseded", "agent_id": agent, "new_serial": serial})),
                                ..Audit::default()
                            },
                        )?;
                        revoked.push(old);
                    }
                }
            }
            tx.commit()?;
            Ok(revoked)
        });
        match result {
            Ok(revoked) if !revoked.is_empty() => {
                info!(
                    "agent {agent} 的新憑證 {serial} 已開始使用，舊憑證以 superseded 撤銷：{}",
                    revoked.join("、")
                );
                svc.wake();
            }
            Ok(_) => {}
            Err(e) => warn!("無法記錄 {agent} 的憑證 {serial} 第一次使用：{e}"),
        }
    });
}

/// 處理 `renew_request`：成功時回傳 `renew_result` 的一行 JSON
pub async fn renew(
    svc: &Arc<EnrollService>,
    agent_id: &str,
    req: RenewRequest,
) -> Result<String, AgentError> {
    // 每個 agent_id 每小時 10 次
    if svc
        .limits
        .renew_8443
        .take(
            &agent_id.to_string(),
            10.0,
            10.0 / 3600.0,
            Instant::now(),
            Duration::from_secs(3600),
        )
        .is_err()
    {
        return Err(AgentError::new(
            "rate_limited",
            "too many renewal requests; retry later",
        ));
    }
    let der = b64url_decode(&req.csr)
        .ok()
        .filter(|d| !d.is_empty() && d.len() <= kairos_proto::enroll::MAX_CSR_DER_BYTES)
        .ok_or_else(|| AgentError::new("invalid_csr", "csr must be base64url DER"))?;
    let csr = check_csr(&der, agent_id).map_err(|m| AgentError::new("invalid_csr", m))?;
    let s = svc.clone();
    let agent = agent_id.to_string();
    let eff = svc.effective();
    let now = svc.now_ms();
    let request_id = req.request_id.clone();
    let source = "8443".to_string();
    let created = svc
        .db
        .write(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // 冪等：同一 agent_id＋同一 CSR 只簽發一張
            if let Some(r) = store::request_by_csr(&tx, &agent, &csr.csr_sha256)? {
                return Ok(Ok(r));
            }
            if ps::spki_used_by_agent(&tx, &agent, &csr.spki_sha256)? {
                return Ok(Err(AgentError::new(
                    "invalid_csr",
                    "the new key must differ from previous keys of this agent_id",
                )));
            }
            let host = store::host_enroll(&tx, &agent)?;
            let needs_approval = eff.policy.renew_requires_approval;
            let id = if kairos_proto::is_uuid_text(&request_id) {
                request_id.to_ascii_lowercase()
            } else {
                return Ok(Err(AgentError::new("bad_request", "request_id must be a UUID")));
            };
            if store::get_request(&tx, &id)?.is_some() {
                return Ok(Err(AgentError::new(
                    "bad_request",
                    "request_id is already used by a different CSR",
                )));
            }
            let row = RequestRow {
                request_id: id.clone(),
                kind: DbKind::Renewal,
                agent_id: agent.clone(),
                request_code: kairos_proto::enroll::request_code_from_spki(&csr.spki_der).compact(),
                state: if needs_approval {
                    DbState::PendingApproval
                } else {
                    DbState::Approved
                },
                csr_der: der,
                csr_sha256: csr.csr_sha256.to_vec(),
                spki_sha256: csr.spki_sha256.to_vec(),
                poll_secret_hash: None,
                token_id: None,
                expired_serial: None,
                previous_serial: host.as_ref().and_then(|h| h.current_serial.clone()),
                source_ip: source,
                hostname: host.as_ref().map(|h| h.hostname.clone()).unwrap_or_default(),
                display_name: None,
                tags: None,
                os: None,
                host_fingerprint: host.as_ref().and_then(|h| h.host_fingerprint.clone()),
                pulse_version: None,
                server_verification: None,
                flags: Vec::new(),
                cert_lifetime_days: None,
                created_at: now,
                expires_at: now + i64::from(s.config.enroll.request_expiry_days) * 86_400_000,
                decided_at: (!needs_approval).then_some(now),
                decided_by: (!needs_approval).then(|| "system".to_string()),
                reject_reason_code: None,
                reject_reason: None,
                issued_serial: None,
                issue_error: None,
                delivered_at: None,
                withdrawn_at: None,
                withdraw_reason: None,
                withdraw_initiated_by: None,
                withdraw_ip: None,
                closed_at: None,
            };
            store::insert_request(&tx, &row)?;
            store::audit(
                &tx,
                now,
                &Audit {
                    actor: &format!("host:{}", &agent[..8]),
                    category: "pki",
                    action: "renewal.create",
                    object: Some(&id),
                    result: "ok",
                    details: Some(json!({"agent_id": agent, "via": "8443", "requires_approval": needs_approval})),
                    ..Audit::default()
                },
            )?;
            tx.commit()?;
            Ok(Ok(row))
        })
        .await
        .map_err(|_| storage())??;
    let cert = match created.state {
        DbState::Issued => {
            let serial = created.issued_serial.clone().unwrap_or_default();
            svc.db
                .read(move |conn| Ok(ps::get_cert(conn, &serial)?))
                .await
                .map_err(|_| storage())?
        }
        DbState::Approved => {
            let s = svc.clone();
            let id = created.request_id.clone();
            let eff = svc.effective();
            let outcome = svc
                .db
                .write(move |conn| Ok(s.pki.issue(conn, &id, "system", &eff, now)))
                .await
                .map_err(|_| storage())?;
            match outcome {
                Ok(IssueOutcome::Issued(c)) | Ok(IssueOutcome::Already(c)) => {
                    svc.stats
                        .issued
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    info!("agent {agent_id} 經 8443 輪替憑證：新序號 {}", c.serial);
                    Some(c)
                }
                Ok(_) => None,
                Err(IssueError::Db(_)) => return Err(storage()),
                Err(_) => None,
            }
        }
        DbState::PendingApproval => None,
        _ => {
            return Err(AgentError::new(
                "invalid_csr",
                "a previous request with this CSR was closed; generate a new key",
            ));
        }
    };
    let Some(cert) = cert.filter(|c| c.revoked_at.is_none()) else {
        return Err(AgentError::new(
            "renew_requires_approval",
            "renewal requires approval or offline issuance; resend the same CSR later",
        ));
    };
    let certificate = certificate_json(svc, &cert).ok_or_else(storage)?;
    Ok(json!({
        "type": "renew_result",
        "seq": req.seq,
        "request_id": created.request_id,
        "state": "issued",
        "certificate": certificate,
        "policy": svc.effective().protocol_policy(),
        "server_time": format_rfc3339_ms(svc.now_ms()),
    })
    .to_string())
}

/// 處理 `deregister`：生命週期改為已除役、依政策撤銷這條連線的憑證（10 分鐘後生效），回傳
/// `deregister_ack` 的一行 JSON。已除役時回 `already_decommissioned: true`（冪等）
pub async fn deregister(
    svc: &Arc<EnrollService>,
    agent_id: &str,
    serial: Option<String>,
    peer: &str,
    msg: Deregister,
) -> Result<String, AgentError> {
    let now = svc.now_ms();
    let revoke = svc.effective().policy.revoke_on_self_decommission;
    let agent = agent_id.to_string();
    let peer = peer.to_string();
    let m = msg.clone();
    let (already, revoked, at) = svc
        .db
        .write(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let hostname = m.hostname.clone().unwrap_or_else(|| agent.clone());
            let already = store::decommission_host(&tx, &agent, &hostname, "self", now)?;
            let host = store::host_enroll(&tx, &agent)?;
            let at = host.and_then(|h| h.decommissioned_at).unwrap_or(now);
            let mut revoked = false;
            if revoke && let Some(serial) = &serial {
                match ps::revoke(
                    &tx,
                    serial,
                    now,
                    now + super::DEREGISTER_WINDOW_MS,
                    Reason::CessationOfOperation,
                    "system",
                    Some("主機自行除役"),
                )? {
                    Revoke::Now(_) | Revoke::Already(_) => revoked = true,
                    Revoke::NotFound => {}
                }
            }
            store::audit(
                &tx,
                now,
                &Audit {
                    actor: &format!("host:{}", &agent[..8]),
                    actor_ip: Some(&peer),
                    category: "host",
                    action: "host.self_decommission",
                    object: Some(&agent),
                    result: "ok",
                    details: Some(json!({
                        "request_id": m.request_id,
                        "reason": m.reason,
                        "initiated_by": m.initiated_by,
                        "host_fingerprint": m.host_fingerprint,
                        "pulse_version": m.pulse_version,
                        "certificate_revoked": revoked,
                        "already_decommissioned": already,
                    })),
                    ..Audit::default()
                },
            )?;
            tx.commit()?;
            Ok((already, revoked, at))
        })
        .await
        .map_err(|_| storage())?;
    svc.set_decommissioned(agent_id, true);
    if !already {
        info!(
            "agent {agent_id} 已自行除役（{}，來源 {}）{}",
            msg.initiated_by.as_deref().unwrap_or("-"),
            msg.request_id,
            if revoked {
                "，憑證在 10 分鐘後撤銷"
            } else {
                ""
            }
        );
    }
    let mut ack = json!({
        "type": "deregister_ack",
        "request_id": msg.request_id,
        "agent_id": agent_id,
        "state": "decommissioned",
        "decommissioned_at": format_rfc3339_ms(at),
        "certificate_revoked": revoked,
        "already_decommissioned": already,
    });
    if let Some(seq) = msg.seq {
        ack["seq"] = json!(seq);
    }
    Ok(ack.to_string())
}
