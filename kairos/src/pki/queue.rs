//! 離線簽發模式的 CSR 佇列（`kairos/PKI.md` 第 6 節）：
//!
//! - 匯出 `kairos-csr-queue/v1`：已核准、尚未簽發的申請（不含秘密；不改變狀態，可重複匯出）。
//! - 在 CA 主機以 `kairos pki sign-bundle` 依第 5.1 節的格式簽發，輸出 `kairos-signed-certs/v1`
//!   （不連網、不需要 Kairos 的資料庫）。
//! - 匯入時逐張檢查：鏈到目前的信任錨、SAN 與 CN 等於申請的 `agent_id`、公鑰等於申請 CSR 的公鑰、
//!   序號未使用、`notAfter` 在未來、EKU `clientAuth`。通過的申請改為 `issued`；不通過的列出原因。

use kairos_proto::enroll::{b64url_decode, b64url_encode, format_rfc3339_ms};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use super::csr::check_csr;
use super::issue::{IssuingCa, random_serial};
use super::store::CertRow;
use super::x509::{parse_cert, verify_cert_signature};
use crate::enroll::pki_mgr::issued_via;
use crate::enroll::store::{Audit, DbState};
use crate::store::prelude::*;

pub const QUEUE_SCHEMA: &str = "kairos-csr-queue/v1";
pub const SIGNED_SCHEMA: &str = "kairos-signed-certs/v1";

/// 匯出待簽發的申請
pub fn export(
    conn: &Connection,
    instance: &str,
    default_days: u32,
    now_ms: i64,
) -> Result<Value, String> {
    let rows = conn.approved_requests(100_000).map_err(|e| e.to_string())?;
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
    let req = tx
        .get_request(request_id)
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
    if tx.serial_exists(&info.serial).map_err(|e| e.to_string())? {
        return Err(format!("序號 {} 已使用過", info.serial));
    }
    let anchor_spki = parse_cert(anchor)?.spki_der;
    let ca_id = tx
        .find_ca_by_spki(&kairos_proto::enroll::sha256(&anchor_spki))
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
    fn e(e: impl std::fmt::Display) -> String {
        e.to_string()
    }
    tx.insert_cert(&cert).map_err(e)?;
    tx.mark_issued(&req.request_id, &cert.serial, by, now_ms)
        .map_err(e)?;
    tx.upsert_host_on_issue(
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
    tx.audit(
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
    let mut out: Vec<Vec<u8>> = conn
        .trust_anchor_cas()
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

/// [`import_existing`] 的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingImport {
    /// 已加入簽發紀錄（`expired`：已過期，只供清冊、過期續簽的「最新憑證」判斷與撤銷紀錄）
    Imported {
        serial: String,
        agent_id: String,
        issuer: String,
        not_after_ms: i64,
        expired: bool,
        /// 由 Kairos 管理的簽發 CA 簽發時為它的編號；外部 CA（`auth.client_ca`）為 `None`
        ca_id: Option<i64>,
        /// 清冊中原本沒有這台主機（新增了最小的一列）
        new_host: bool,
    },
    /// 同一張憑證先前已匯入（或由 Kairos 簽發）
    Already { serial: String, agent_id: String },
}

/// 匯入手動簽發（外部 CA 或以 `kairos pki sign --profile client` 簽發）的既有用戶端憑證到簽發紀錄
/// （`kairos pki certificates import`、`POST /api/v1/pki/certificates/import`；PKI.md 第 9 節第 5 步）。
/// 之後這張憑證出現在 S14 的清冊、可以撤銷，過期續簽以它判斷「最新的憑證」。
///
/// 檢查（不通過時回傳原因；呼叫端以 `KRS-E1305` 呈現）：由目前的信任錨**直接**簽發（與過期續簽相同）、
/// 不是 CA、有 EKU 時必須含 `clientAuth`、SAN 恰好一個 DNS 名稱且是 UUID（`agent_id`）、CN 有的話等於它、
/// 序號沒有被另一張憑證使用。清冊中沒有這台主機時新增最小的一列（主機名稱暫為 `agent_id`，第一次連線後更新）
pub fn import_existing(
    conn: &mut Connection,
    der: &[u8],
    anchors: &[Vec<u8>],
    expected_agent: Option<&str>,
    by: &str,
    actor_ip: Option<&str>,
    now_ms: i64,
) -> Result<ExistingImport, String> {
    let info = parse_cert(der)?;
    if info.is_ca {
        return Err("這是 CA 憑證，不是用戶端憑證".into());
    }
    if info.eku.is_some_and(|(client, _, any)| !client && !any) {
        return Err("EKU 沒有 clientAuth".into());
    }
    let agent_id = match info.dns_sans.as_slice() {
        [one] if info.other_sans == 0 && kairos_proto::is_uuid_text(one) => {
            one.to_ascii_lowercase()
        }
        _ => return Err("SAN 必須恰好一個 DNS 名稱，且是 agent_id（UUID）".into()),
    };
    if info
        .common_names
        .iter()
        .any(|cn| !cn.eq_ignore_ascii_case(&agent_id))
    {
        return Err("CN 不等於 SAN 的 agent_id".into());
    }
    if let Some(e) = expected_agent
        && !e.eq_ignore_ascii_case(&agent_id)
    {
        return Err(format!("憑證的 agent_id 是 {agent_id}，不是指定的 {e}"));
    }
    let anchor = anchors
        .iter()
        .find(|a| verify_cert_signature(der, a))
        .ok_or("憑證不是由目前的信任錨直接簽發（auth.client_ca 或 Kairos 的簽發 CA）")?;
    fn e(e: impl std::fmt::Display) -> String {
        e.to_string()
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(e)?;
    if let Some(existing) = tx.get_cert(&info.serial).map_err(e)? {
        if existing.cert_der == der {
            return Ok(ExistingImport::Already {
                serial: existing.serial,
                agent_id: existing.agent_id,
            });
        }
        return Err(format!("序號 {} 已被另一張憑證使用", info.serial));
    }
    let anchor_spki = parse_cert(anchor)?.spki_der;
    let ca_id = tx
        .find_ca_by_spki(&kairos_proto::enroll::sha256(&anchor_spki))
        .map_err(e)?
        .map(|c| c.ca_id);
    let cert = CertRow {
        serial: info.serial.clone(),
        ca_id,
        agent_id: agent_id.clone(),
        spki_sha256: kairos_proto::enroll::sha256(&info.spki_der).to_vec(),
        not_before: info.not_before_ms,
        not_after: info.not_after_ms,
        cert_der: der.to_vec(),
        issued_at: info.not_before_ms,
        issued_via: "import".to_string(),
        approved_by: Some(by.to_string()),
        ..CertRow::default()
    };
    tx.insert_cert(&cert).map_err(e)?;
    // 清冊：沒有這台主機時新增；現有的最新憑證比這張早到期時改以這張為最新
    let current: Option<(Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT h.current_serial, c.not_after FROM hosts h
             LEFT JOIN pki_certificates c ON c.serial = h.current_serial WHERE h.agent_id = ?1",
            [&agent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(e)?;
    let new_host = current.is_none();
    match current {
        None => {
            tx.execute(
                "INSERT INTO hosts (agent_id, hostname, online, first_seen, last_seen, source, current_serial, lifecycle)
                 VALUES (?1, ?1, 0, ?2, ?2, 'cert_import', ?3, 'active')",
                params![agent_id, now_ms, cert.serial],
            )
            .map_err(e)?;
        }
        Some((serial, not_after)) => {
            if serial.is_none() || not_after.is_none_or(|t| t < cert.not_after) {
                tx.execute(
                    "UPDATE hosts SET current_serial = ?2 WHERE agent_id = ?1",
                    params![agent_id, cert.serial],
                )
                .map_err(e)?;
            }
        }
    }
    tx.audit(
        now_ms,
        &Audit {
            actor: by,
            actor_ip,
            category: "pki",
            action: "cert.import",
            object: Some(&cert.serial),
            result: "ok",
            details: Some(json!({
                "agent_id": agent_id,
                "issuer": info.issuer_text,
                "not_after": format_rfc3339_ms(info.not_after_ms),
                "ca_id": ca_id,
                "source": "existing",
            })),
            ..Audit::default()
        },
    )
    .map_err(e)?;
    tx.commit().map_err(e)?;
    Ok(ExistingImport::Imported {
        serial: cert.serial,
        agent_id,
        issuer: info.issuer_text,
        not_after_ms: info.not_after_ms,
        expired: info.not_after_ms <= now_ms,
        ca_id,
        new_host,
    })
}

/// 憑證檔的內容（PEM，可含多張；或單張 DER）→ DER 的清單
pub fn certificates_in(bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    match std::str::from_utf8(bytes) {
        Ok(text) if text.contains("-----BEGIN") => {
            let blocks = super::x509::pem_decode_all(text, Some("CERTIFICATE"))?;
            if blocks.is_empty() {
                return Err("沒有 CERTIFICATE 區塊".into());
            }
            Ok(blocks.into_iter().map(|(_, d)| d).collect())
        }
        _ => {
            parse_cert(bytes)?;
            Ok(vec![bytes.to_vec()])
        }
    }
}

/// [`import_existing`] 的結果（API 與 CLI 的 `--json`）
pub fn existing_json(source: &str, r: &Result<ExistingImport, String>) -> Value {
    match r {
        Ok(ExistingImport::Imported {
            serial,
            agent_id,
            issuer,
            not_after_ms,
            expired,
            ca_id,
            new_host,
        }) => json!({
            "source": source, "result": "imported", "serial": serial, "agent_id": agent_id,
            "issuer": issuer, "not_after": format_rfc3339_ms(*not_after_ms), "expired": expired,
            "ca_id": ca_id, "new_host": new_host,
        }),
        Ok(ExistingImport::Already { serial, agent_id }) => json!({
            "source": source, "result": "already_imported", "serial": serial, "agent_id": agent_id,
        }),
        Err(e) => json!({
            "source": source, "result": "rejected", "reason": e, "product_code": "KRS-E1305",
        }),
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use crate::test_pki::{CA, GOOD_CERT, GOOD_ID, REVOKED_CERT};

    fn db() -> (std::path::PathBuf, Connection) {
        let dir = std::env::temp_dir().join(format!(
            "kairos-cert-import-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = crate::config::AppConfig::default();
        config.storage.data_dir = dir.to_string_lossy().into_owned();
        let opts = crate::store::sqlite::SqliteOptions::from_config(&config, &dir);
        let conn =
            crate::store::sqlite::open_admin(&opts, std::time::Duration::from_secs(1)).unwrap();
        (dir, conn)
    }

    fn der(pem: &str) -> Vec<u8> {
        certificates_in(pem.as_bytes()).unwrap().remove(0)
    }

    #[test]
    fn existing_certificates_are_checked_and_imported_once() {
        let (dir, mut conn) = db();
        let anchors = vec![der(CA)];
        let now = 1_791_000_000_000;
        // 指定的 agent_id 不符、不是信任錨直接簽發、CA 憑證：拒絕，不寫入
        let e = import_existing(
            &mut conn,
            &der(GOOD_CERT),
            &anchors,
            Some("11111111-2222-4333-8444-555555555555"),
            "root",
            None,
            now,
        );
        assert!(e.unwrap_err().contains("不是指定的"));
        let e = import_existing(&mut conn, &der(GOOD_CERT), &[], None, "root", None, now);
        assert!(e.unwrap_err().contains("信任錨"));
        let e = import_existing(&mut conn, &der(CA), &anchors, None, "root", None, now);
        assert!(e.unwrap_err().contains("CA"));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM pki_certificates", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        // 匯入：清冊新增最小的一列（主機名稱暫為 agent_id）、稽核 cert.import
        let r = import_existing(
            &mut conn,
            &der(GOOD_CERT),
            &anchors,
            Some(GOOD_ID),
            "root",
            Some("10.0.0.9"),
            now,
        )
        .unwrap();
        let ExistingImport::Imported {
            serial,
            agent_id,
            ca_id,
            new_host,
            expired,
            ..
        } = r.clone()
        else {
            panic!("{r:?}");
        };
        assert_eq!(
            (agent_id.as_str(), ca_id, new_host, expired),
            (GOOD_ID, None, true, false)
        );
        let (hostname, current, source): (String, String, String) = conn
            .query_row(
                "SELECT hostname, current_serial, source FROM hosts WHERE agent_id = ?1",
                [GOOD_ID],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (hostname.as_str(), current.as_str(), source.as_str()),
            (GOOD_ID, serial.as_str(), "cert_import")
        );
        let c = conn.get_cert(&serial).unwrap().unwrap();
        assert_eq!(
            (c.issued_via.as_str(), c.approved_by.as_deref()),
            ("import", Some("root"))
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM audit_events WHERE action = 'cert.import' AND object = ?1",
                [&serial],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        // 同一張再匯入：already；JSON 的形式
        let again = import_existing(
            &mut conn,
            &der(GOOD_CERT),
            &anchors,
            None,
            "root",
            None,
            now,
        );
        assert_eq!(
            again,
            Ok(ExistingImport::Already {
                serial: serial.clone(),
                agent_id: GOOD_ID.into()
            })
        );
        assert_eq!(existing_json("a.pem", &again)["result"], "already_imported");
        assert_eq!(existing_json("a.pem", &Ok(r))["result"], "imported");
        assert_eq!(
            existing_json("b.pem", &Err("x".into()))["product_code"],
            "KRS-E1305"
        );
        // 第二台主機（另一張憑證）
        let r2 = import_existing(
            &mut conn,
            &der(REVOKED_CERT),
            &anchors,
            None,
            "root",
            None,
            now,
        )
        .unwrap();
        assert!(matches!(
            r2,
            ExistingImport::Imported { new_host: true, .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn certificate_files_may_hold_several_pem_blocks_or_one_der() {
        let both = format!("{GOOD_CERT}\n{REVOKED_CERT}\n");
        assert_eq!(certificates_in(both.as_bytes()).unwrap().len(), 2);
        let d = der(GOOD_CERT);
        assert_eq!(certificates_in(&d).unwrap(), vec![d.clone()]);
        assert!(certificates_in(b"not a certificate").is_err());
        assert!(
            certificates_in(b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n")
                .is_err()
        );
    }
}
