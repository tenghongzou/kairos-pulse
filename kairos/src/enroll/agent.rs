//! 8443 上與註冊有關的訊息（`crates/kairos-proto/PROTOCOL.md`「憑證輪替與除役」）：正常輪替 `renew`、
//! 除役 `deregister`，以及輪替後撤銷舊憑證（新序號第一次成功 `hello` 時）。
//!
//! 訊息型別是 `kairos-proto` 的共用型別（[`RenewRequest`]、[`Deregister`]、[`RenewResult`]、
//! [`DeregisterAck`]，`ClientMsg`／`ServerMsg` 的對應變體）；本模組只處理資料庫與簽發。

use kairos_proto::enroll::{IssuedCertificate, b64url_decode, format_rfc3339_ms};
use kairos_proto::lifecycle::{DEREGISTER_STATE_DECOMMISSIONED, RENEW_STATE_ISSUED};
use kairos_proto::{Deregister, DeregisterAck, ErrorCode, RenewRequest, RenewResult, ServerMsg};
use log::{info, warn};
use rusqlite::TransactionBehavior;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::EnrollService;
use super::pki_mgr::{IssueError, IssueOutcome};
use super::store::{self, Audit, DbKind, DbState, RequestRow};
use crate::pki::csr::check_csr;
use crate::pki::issue::Reason;
use crate::pki::store::{self as ps, CertRow, Revoke};
use crate::pki::x509::pem_encode;

/// 8443 上的錯誤（`error` 訊息的 `code` 與 `message`；都不是 fatal）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    pub code: ErrorCode,
    pub message: String,
}

impl AgentError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 回應 `seq` 的 `error` 訊息（非 fatal）
    pub fn to_msg(&self, seq: Option<u64>) -> ServerMsg {
        ServerMsg::error_with_fatal(seq, self.code, self.message.clone(), false)
    }
}

fn storage() -> AgentError {
    AgentError::new(
        ErrorCode::StorageUnavailable,
        "server could not store the request",
    )
}

fn issued_certificate(svc: &EnrollService, c: &CertRow) -> Option<IssuedCertificate> {
    let ca = svc.pki.server_ca()?;
    Some(IssuedCertificate {
        serial: c.serial.clone(),
        not_before: format_rfc3339_ms(c.not_before),
        not_after: format_rfc3339_ms(c.not_after),
        client_cert_pem: pem_encode("CERTIFICATE", &c.cert_der),
        chain_pem: String::new(),
        ca_pem: ca.pem.clone(),
    })
}

/// `hello_ack` 另帶的欄位（雙方協商了 `renew` 時）：`expired_renew_grace_days`、
/// 以及以非啟用中的簽發 CA 簽發的憑證要求提早輪替的 `renew_now`（不需要時 `None`，不輸出）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HelloAckRenew {
    pub expired_renew_grace_days: Option<u32>,
    pub renew_now: Option<bool>,
}

/// 計算 [`HelloAckRenew`]（會讀資料庫，在 blocking 執行緒呼叫）
pub fn hello_ack_renew(svc: &EnrollService, client_cert: Option<&[u8]>) -> HelloAckRenew {
    let mut m = HelloAckRenew {
        expired_renew_grace_days: Some(svc.effective().policy.expired_renew_grace_days),
        renew_now: None,
    };
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
            m.renew_now = Some(true);
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

/// 處理 `renew_request`：成功時回傳 `renew_result`
pub async fn renew(
    svc: &Arc<EnrollService>,
    agent_id: &str,
    req: RenewRequest,
) -> Result<RenewResult, AgentError> {
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
            ErrorCode::RateLimited,
            "too many renewal requests; retry later",
        ));
    }
    let der = b64url_decode(&req.csr)
        .ok()
        .filter(|d| !d.is_empty() && d.len() <= kairos_proto::enroll::MAX_CSR_DER_BYTES)
        .ok_or_else(|| AgentError::new(ErrorCode::InvalidCsr, "csr must be base64url DER"))?;
    let csr = check_csr(&der, agent_id).map_err(|m| AgentError::new(ErrorCode::InvalidCsr, m))?;
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
                    ErrorCode::InvalidCsr,
                    "the new key must differ from previous keys of this agent_id",
                )));
            }
            let host = store::host_enroll(&tx, &agent)?;
            let needs_approval = eff.policy.renew_requires_approval;
            let id = if kairos_proto::is_uuid_text(&request_id) {
                request_id.to_ascii_lowercase()
            } else {
                return Ok(Err(AgentError::new(
                    ErrorCode::BadRequest,
                    "request_id must be a UUID",
                )));
            };
            if store::get_request(&tx, &id)?.is_some() {
                return Ok(Err(AgentError::new(
                    ErrorCode::BadRequest,
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
                ErrorCode::InvalidCsr,
                "a previous request with this CSR was closed; generate a new key",
            ));
        }
    };
    let Some(cert) = cert.filter(|c| c.revoked_at.is_none()) else {
        return Err(AgentError::new(
            ErrorCode::RenewRequiresApproval,
            "renewal requires approval or offline issuance; resend the same CSR later",
        ));
    };
    let certificate = issued_certificate(svc, &cert).ok_or_else(storage)?;
    Ok(RenewResult {
        seq: req.seq,
        request_id: created.request_id,
        state: RENEW_STATE_ISSUED.to_string(),
        certificate,
        policy: Some(svc.effective().protocol_policy()),
        server_time: Some(format_rfc3339_ms(svc.now_ms())),
    })
}

/// 處理 `deregister`：生命週期改為已除役、依政策撤銷這條連線的憑證（10 分鐘後生效），回傳
/// `deregister_ack`。已除役時回 `already_decommissioned: true`（冪等）
pub async fn deregister(
    svc: &Arc<EnrollService>,
    agent_id: &str,
    serial: Option<String>,
    peer: &str,
    msg: Deregister,
) -> Result<DeregisterAck, AgentError> {
    let now = svc.now_ms();
    let revoke = svc.effective().policy.revoke_on_self_decommission;
    let agent = agent_id.to_string();
    let peer = peer.to_string();
    let m = msg.clone();
    let (already, revoked, at) = svc
        .db
        .write(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let hostname = if m.hostname.is_empty() {
                agent.clone()
            } else {
                m.hostname.clone()
            };
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
            msg.initiated_by,
            msg.request_id,
            if revoked {
                "，憑證在 10 分鐘後撤銷"
            } else {
                ""
            }
        );
    }
    Ok(DeregisterAck {
        seq: msg.seq,
        request_id: msg.request_id,
        agent_id: agent_id.to_string(),
        state: DEREGISTER_STATE_DECOMMISSIONED.to_string(),
        decommissioned_at: format_rfc3339_ms(at),
        certificate_revoked: revoked,
        already_decommissioned: already,
    })
}
