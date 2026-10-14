//! 離線簽發模式的 CSR 佇列（`kairos/PKI.md` 第 6 節）：
//!
//! - 匯出 `kairos-csr-queue/v1`：已核准、尚未簽發的申請（不含秘密；不改變狀態，可重複匯出）。
//! - 在 CA 主機以 `kairos pki sign-bundle` 依第 5.1 節的格式簽發，輸出 `kairos-signed-certs/v1`
//!   （不連網、不需要 Kairos 的資料庫）。
//! - 匯入時逐張檢查：鏈到目前的信任錨、SAN 與 CN 等於申請的 `agent_id`、公鑰等於申請 CSR 的公鑰、
//!   序號未使用、`notAfter` 在未來、EKU `clientAuth`。通過的申請改為 `issued`；不通過的列出原因。

use kairos_proto::enroll::{b64url_decode, b64url_encode, format_rfc3339_ms};
use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Value, json};

use super::csr::check_csr;
use super::issue::{IssuingCa, random_serial};
use super::store::{self as ps, CertRow};
use super::x509::{parse_cert, verify_cert_signature};
use crate::enroll::pki_mgr::issued_via;
use crate::enroll::store::{self as es, Audit, DbState};

pub const QUEUE_SCHEMA: &str = "kairos-csr-queue/v1";
pub const SIGNED_SCHEMA: &str = "kairos-signed-certs/v1";

/// 匯出待簽發的申請
pub fn export(
    conn: &Connection,
    instance: &str,
    default_days: u32,
    now_ms: i64,
) -> Result<Value, String> {
    let rows = es::approved_requests(conn, 100_000).map_err(|e| e.to_string())?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "request_id": r.request_id,
                "kind": r.kind.as_str(),
                "agent_id": r.agent_id,
                "csr": b64url_encode(&r.csr_der),
                "lifetime_days": r.cert_lifetime_days.unwrap_or(i64::from(default_days)),
                "approved_by": r.decided_by,
                "approved_at": r.decided_at.map(format_rfc3339_ms),
            })
        })
        .collect();
    Ok(json!({
        "schema": QUEUE_SCHEMA,
        "exported_at": format_rfc3339_ms(now_ms),
        "kairos_instance": instance,
        "items": items,
    }))
}

/// 在 CA 主機上簽發佇列中的每一筆（CSR 不合法的列在 `errors`）
pub fn sign_bundle(queue: &Value, ca: &IssuingCa, now_ms: i64) -> Result<Value, String> {
    if queue["schema"] != QUEUE_SCHEMA {
        return Err(format!("輸入檔不是 {QUEUE_SCHEMA}"));
    }
    let mut items = Vec::new();
    let mut errors = Vec::new();
    for item in queue["items"].as_array().cloned().unwrap_or_default() {
        let request_id = item["request_id"].as_str().unwrap_or_default().to_string();
        let agent_id = item["agent_id"].as_str().unwrap_or_default().to_string();
        let result = (|| -> Result<Value, String> {
            let der = b64url_decode(item["csr"].as_str().unwrap_or_default())
                .map_err(|_| "csr 不是 base64url".to_string())?;
            let csr = check_csr(&der, &agent_id)?;
            let days = item["lifetime_days"].as_i64().unwrap_or(365).clamp(1, 825);
            let issued = ca.sign_client(
                &csr.spki_der,
                &agent_id,
                now_ms,
                days * 86_400_000,
                random_serial()?,
            )?;
            Ok(json!({"request_id": request_id, "certificate": b64url_encode(&issued.der)}))
        })();
        match result {
            Ok(v) => items.push(v),
            Err(e) => errors.push(json!({"request_id": request_id, "error": e})),
        }
    }
    Ok(json!({
        "schema": SIGNED_SCHEMA,
        "signed_at": format_rfc3339_ms(now_ms),
        "items": items,
        "errors": errors,
    }))
}

/// 匯入的逐筆結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub request_id: String,
    /// `Ok(序號)` 或不通過的原因
    pub result: Result<String, String>,
}

/// 匯入簽好的憑證。`anchors` 為目前的信任錨（`auth.client_ca` 與簽發 CA 的 DER）
pub fn import(
    conn: &mut Connection,
    signed: &Value,
    anchors: &[Vec<u8>],
    by: &str,
    now_ms: i64,
) -> Result<Vec<Imported>, String> {
    if signed["schema"] != SIGNED_SCHEMA {
        return Err(format!("輸入檔不是 {SIGNED_SCHEMA}"));
    }
    let mut out = Vec::new();
    for item in signed["items"].as_array().cloned().unwrap_or_default() {
        let request_id = item["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let der = b64url_decode(item["certificate"].as_str().unwrap_or_default())
            .map_err(|_| "certificate 不是 base64url".to_string());
        let result = match der {
            Ok(der) => import_one(conn, &request_id, &der, anchors, by, now_ms),
            Err(e) => Err(e),
        };
        out.push(Imported { request_id, result });
    }
    Ok(out)
}

/// 匯入一張憑證（PEM 目錄的匯入也使用）
pub fn import_one(
    conn: &mut Connection,
    request_id: &str,
    der: &[u8],
    anchors: &[Vec<u8>],
    by: &str,
    now_ms: i64,
) -> Result<String, String> {
    let info = parse_cert(der)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let req = es::get_request(&tx, request_id)
        .map_err(|e| e.to_string())?
        .ok_or("找不到這筆申請")?;
    if req.state != DbState::Approved {
        return Err(format!(
            "申請的狀態是 {}，不是已核准待簽發",
            req.state.as_str()
        ));
    }
    let anchor = anchors
        .iter()
        .find(|a| verify_cert_signature(der, a))
        .ok_or("憑證沒有鏈到目前的信任錨（auth.client_ca 或 Kairos 的簽發 CA）")?;
    if info.dns_sans != [req.agent_id.clone()] || info.other_sans != 0 {
        return Err("SAN 必須恰好一個 DNS 名稱且等於 agent_id".into());
    }
    if info.common_names.iter().any(|cn| *cn != req.agent_id) {
        return Err("CN 不等於 agent_id".into());
    }
    let csr = check_csr(&req.csr_der, &req.agent_id)?;
    if csr.spki_der != info.spki_der {
        return Err("憑證的公鑰不是申請 CSR 的公鑰".into());
    }
    if info.not_after_ms <= now_ms {
        return Err("憑證已過期".into());
    }
    if info.eku.is_none_or(|(client, _, _)| !client) {
        return Err("EKU 必須含 clientAuth".into());
    }
    if info.is_ca {
        return Err("不能是 CA 憑證".into());
    }
    if ps::serial_exists(&tx, &info.serial).map_err(|e| e.to_string())? {
        return Err(format!("序號 {} 已使用過", info.serial));
    }
    let anchor_spki = parse_cert(anchor)?.spki_der;
    let ca_id = ps::find_ca_by_spki(&tx, &kairos_proto::enroll::sha256(&anchor_spki))
        .map_err(|e| e.to_string())?
        .map(|c| c.ca_id);
    let cert = CertRow {
        serial: info.serial.clone(),
        ca_id,
        agent_id: req.agent_id.clone(),
        spki_sha256: csr.spki_sha256.to_vec(),
        not_before: info.not_before_ms,
        not_after: info.not_after_ms,
        cert_der: der.to_vec(),
        issued_at: now_ms,
        issued_via: issued_via(req.kind).to_string(),
        request_id: Some(req.request_id.clone()),
        approved_by: req.decided_by.clone(),
        ..CertRow::default()
    };
    let e = |e: rusqlite::Error| e.to_string();
    ps::insert_cert(&tx, &cert).map_err(e)?;
    es::mark_issued(&tx, &req.request_id, &cert.serial, by, now_ms).map_err(e)?;
    es::upsert_host_on_issue(
        &tx,
        &req.agent_id,
        &req.hostname,
        req.host_fingerprint.as_deref(),
        req.display_name.as_deref(),
        None,
        req.os.as_deref(),
        &cert.serial,
        now_ms,
    )
    .map_err(e)?;
    es::audit(
        &tx,
        now_ms,
        &Audit {
            actor: by,
            category: "pki",
            action: "cert.import",
            object: Some(&cert.serial),
            result: "ok",
            details: Some(json!({"request_id": req.request_id, "agent_id": req.agent_id})),
            ..Audit::default()
        },
    )
    .map_err(e)?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(cert.serial)
}

/// 目前的信任錨（DER）：`auth.client_ca` 的檔案與 Kairos 的簽發 CA
pub fn anchors(conn: &Connection, client_ca: Option<&str>) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = ps::trust_anchor_cas(conn)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| c.cert_der)
        .collect();
    if let Some(path) = client_ca
        && let Ok(text) = std::fs::read_to_string(path)
        && let Ok(blocks) = super::x509::pem_decode_all(&text, Some("CERTIFICATE"))
    {
        out.extend(blocks.into_iter().map(|(_, d)| d));
    }
    out
}
