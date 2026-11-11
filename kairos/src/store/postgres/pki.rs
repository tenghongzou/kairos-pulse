//! PKI（簽發 CA、憑證、撤銷、CRL）的 PostgreSQL 實作（[`crate::pki::store`] 的 [`PkiStore`]）。逐函式對照
//! `store/sqlite/pki.rs`，差別只在 SQL 方言與並行控制（`kairos/HA.md` 第 5.3、5.4 節）。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

use crate::pki::issue::{Reason, RevokedEntry};
use serde_json::json;

use super::conn::{OptionalExtension, PgConn, PgResult, Row, array, params, pg_domain};
#[allow(unused_imports)]
use crate::pki::store::*;
use crate::store::db::TxMode;
use crate::store::purge::CRL_KEEP_AFTER_EXPIRY_MS;

pg_domain! { PkiStore {
    fn revoked_serials(&self) -> std::collections::HashSet<String> => revoked_serials;
    fn insert_pending_ca(&self, subject_of: &dyn Fn(i64) -> String, spki_sha256: &[u8], key_file: &dyn Fn(i64) -> String, now_ms: i64) -> i64 => insert_pending_ca;
    fn next_ca_id(&self) -> i64 => next_ca_id;
    fn list_cas(&self) -> Vec<CaRow> => list_cas;
    fn get_ca(&self, ca_id: i64) -> Option<CaRow> => get_ca;
    fn find_ca_by_spki(&self, spki_sha256: &[u8]) -> Option<CaRow> => find_ca_by_spki;
    fn install_ca(&self, ca_id: i64, subject: &str, cert_der: &[u8], not_before: i64, not_after: i64) -> bool => install_ca;
    fn activate_ca(&self, ca_id: i64) -> () => activate_ca;
    fn retire_ca(&self, ca_id: i64, now_ms: i64) -> () => retire_ca;
    fn trust_anchor_cas(&self) -> Vec<CaRow> => trust_anchor_cas;
    fn insert_cert(&self, c: &CertRow) -> () => insert_cert;
    fn serial_exists(&self, serial: &str) -> bool => serial_exists;
    fn get_cert(&self, serial: &str) -> Option<CertRow> => get_cert;
    fn certs_of_agent(&self, agent_id: &str) -> Vec<CertRow> => certs_of_agent;
    fn list_certs(&self, limit: usize, before: Option<(i64, &str)>) -> Vec<CertRow> => list_certs;
    fn spki_used_by_agent(&self, agent_id: &str, spki_sha256: &[u8]) -> bool => spki_used_by_agent;
    fn revoke(&self, serial: &str, at_ms: i64, effective_at_ms: i64, reason: Reason, by: &str, comment: Option<&str>) -> Revoke => revoke;
    fn revocations_for_crl(&self, ca_id: i64, now_ms: i64) -> Vec<RevokedEntry> => revocations_for_crl;
    fn ca_ids_with_pending_revocations(&self, now_ms: i64) -> Vec<i64> => ca_ids_with_pending_revocations;
    fn record_crl(&self, ca_id: i64, crl_number: i64, this_update: i64, next_update: i64, included: &[RevokedEntry], der: &[u8], source: &str, created_by: Option<&str>) -> () => record_crl;
    fn latest_crl(&self, ca_id: i64) -> Option<CrlRow> => latest_crl;
    fn crl_history(&self, ca_id: i64, limit: usize) -> Vec<CrlRow> => crl_history;
    fn mark_first_used(&self, serial: &str, now_ms: i64) -> bool => mark_first_used;
    fn older_unrevoked(&self, agent_id: &str, serial: &str) -> Vec<String> => older_unrevoked;
    fn issued_count_by_ca(&self, ca_id: i64) -> i64 => issued_count_by_ca;
    fn revoked_external_serials(&self, now_ms: i64, expired_after_ms: i64) -> std::collections::HashSet<String> => revoked_external_serials;
    fn delete_orphan_certificates(&self, now_ms: i64) -> usize => delete_orphan_certificates;
    fn revoke_active_certificates(&self, agent_id: &str, actor: &str, actor_ip: Option<&str>, now_ms: i64) -> Vec<String> => revoke_active_certificates;
    fn set_ca_key_file(&self, ca_id: i64, key_file: &str) -> () => set_ca_key_file;
    fn host_current_cert(&self, agent_id: &str) -> Option<(Option<String>, Option<i64>)> => host_current_cert;
    fn insert_imported_host(&self, agent_id: &str, now_ms: i64, serial: &str) -> () => insert_imported_host;
    fn set_host_current_serial(&self, agent_id: &str, serial: &str) -> () => set_host_current_serial;
} }

const CA_COLUMNS: &str = "ca_id, subject, spki_sha256, cert_der, not_before, not_after, key_file, state, next_crl_number, created_at, retired_at";

fn ca_row(r: &Row) -> PgResult<CaRow> {
    Ok(CaRow {
        ca_id: r.get(0)?,
        subject: r.get(1)?,
        spki_sha256: r.get(2)?,
        cert_der: r.get(3)?,
        not_before: r.get(4)?,
        not_after: r.get(5)?,
        key_file: r.get(6)?,
        state: r.get(7)?,
        next_crl_number: r.get(8)?,
        created_at: r.get(9)?,
        retired_at: r.get(10)?,
    })
}

/// 新增一筆 `pending` 的簽發 CA，回傳 `ca_id`；私鑰檔名由 `key_file(ca_id)` 決定
pub fn insert_pending_ca(
    conn: &PgConn,
    subject_of: impl Fn(i64) -> String,
    spki_sha256: &[u8],
    key_file: impl Fn(i64) -> String,
    now_ms: i64,
) -> PgResult<i64> {
    // 先以暫時的 subject 取得 ca_id（identity 欄位自動產生），再寫入依 ca_id 命名的 subject 與私鑰檔名
    let ca_id: i64 = conn.query_row(
        "INSERT INTO pki_cas (subject, spki_sha256, state, created_at) VALUES ('', ?1, ?2, ?3)
         RETURNING ca_id",
        params![spki_sha256, CA_PENDING, now_ms],
        |r| r.get(0),
    )?;
    conn.execute(
        "UPDATE pki_cas SET subject = ?2, key_file = ?3 WHERE ca_id = ?1",
        params![ca_id, subject_of(ca_id), key_file(ca_id)],
    )?;
    Ok(ca_id)
}

/// 下一個 `ca_id`（產生 CSR 時 subject 要含 ca_id）。只是預估：實際的編號由 [`insert_pending_ca`] 回傳，
/// 兩者不同時（並行建立、identity 序列有跳號）呼叫端會依實際編號重新命名私鑰檔
pub fn next_ca_id(conn: &PgConn) -> PgResult<i64> {
    conn.query_row("SELECT coalesce(max(ca_id), 0) + 1 FROM pki_cas", (), |r| {
        r.get(0)
    })
}

pub fn list_cas(conn: &PgConn) -> PgResult<Vec<CaRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {CA_COLUMNS} FROM pki_cas ORDER BY ca_id"))?;
    let rows = stmt.query_map((), ca_row)?;
    rows.collect()
}

pub fn get_ca(conn: &PgConn, ca_id: i64) -> PgResult<Option<CaRow>> {
    conn.query_row(
        &format!("SELECT {CA_COLUMNS} FROM pki_cas WHERE ca_id = ?1"),
        [ca_id],
        ca_row,
    )
    .optional()
}

pub fn find_ca_by_spki(conn: &PgConn, spki_sha256: &[u8]) -> PgResult<Option<CaRow>> {
    conn.query_row(
        &format!("SELECT {CA_COLUMNS} FROM pki_cas WHERE spki_sha256 = ?1"),
        [spki_sha256],
        ca_row,
    )
    .optional()
}

/// 安裝根 CA 簽發的憑證：狀態改為 `trusted`；沒有 `active` 的 CA 時同時啟用（第一個簽發 CA）。
/// 回傳是否已啟用
pub fn install_ca(
    conn: &PgConn,
    ca_id: i64,
    subject: &str,
    cert_der: &[u8],
    not_before: i64,
    not_after: i64,
) -> PgResult<bool> {
    let has_active: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pki_cas WHERE state = ?1)",
        [CA_ACTIVE],
        |r| r.get(0),
    )?;
    let state = if has_active { CA_TRUSTED } else { CA_ACTIVE };
    conn.execute(
        "UPDATE pki_cas SET subject = ?2, cert_der = ?3, not_before = ?4, not_after = ?5, state = ?6
         WHERE ca_id = ?1",
        params![ca_id, subject, cert_der, not_before, not_after, state],
    )?;
    Ok(!has_active)
}

/// 啟用：新簽發改用這個 CA（其他 `active` 改為 `trusted`）
pub fn activate_ca(conn: &PgConn, ca_id: i64) -> PgResult<()> {
    conn.execute(
        "UPDATE pki_cas SET state = ?2 WHERE state = ?1 AND ca_id != ?3",
        params![CA_ACTIVE, CA_TRUSTED, ca_id],
    )?;
    conn.execute(
        "UPDATE pki_cas SET state = ?2 WHERE ca_id = ?1",
        params![ca_id, CA_ACTIVE],
    )?;
    Ok(())
}

/// 退役：從信任錨移除
pub fn retire_ca(conn: &PgConn, ca_id: i64, now_ms: i64) -> PgResult<()> {
    conn.execute(
        "UPDATE pki_cas SET state = ?2, retired_at = ?3 WHERE ca_id = ?1",
        params![ca_id, CA_RETIRED, now_ms],
    )?;
    Ok(())
}

/// 目前的信任錨（`active`、`trusted` 且已安裝憑證）
pub fn trust_anchor_cas(conn: &PgConn) -> PgResult<Vec<CaRow>> {
    Ok(list_cas(conn)?
        .into_iter()
        .filter(|c| (c.state == CA_ACTIVE || c.state == CA_TRUSTED) && c.cert_der.is_some())
        .collect())
}

const CERT_COLUMNS: &str = "serial, ca_id, agent_id, spki_sha256, not_before, not_after, cert_der, issued_at, issued_via, request_id, approved_by, first_used_at, revoked_at, revoke_effective_at, revocation_reason, revoked_by, revoke_comment, crl_number";

fn cert_row(r: &Row) -> PgResult<CertRow> {
    Ok(CertRow {
        serial: r.get(0)?,
        ca_id: r.get(1)?,
        agent_id: r.get(2)?,
        spki_sha256: r.get(3)?,
        not_before: r.get(4)?,
        not_after: r.get(5)?,
        cert_der: r.get(6)?,
        issued_at: r.get(7)?,
        issued_via: r.get(8)?,
        request_id: r.get(9)?,
        approved_by: r.get(10)?,
        first_used_at: r.get(11)?,
        revoked_at: r.get(12)?,
        revoke_effective_at: r.get(13)?,
        revocation_reason: r.get(14)?,
        revoked_by: r.get(15)?,
        revoke_comment: r.get(16)?,
        crl_number: r.get(17)?,
    })
}

pub fn insert_cert(conn: &PgConn, c: &CertRow) -> PgResult<()> {
    conn.execute(
        &format!(
            "INSERT INTO pki_certificates ({CERT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)"
        ),
        params![
            c.serial,
            c.ca_id,
            c.agent_id,
            c.spki_sha256,
            c.not_before,
            c.not_after,
            c.cert_der,
            c.issued_at,
            c.issued_via,
            c.request_id,
            c.approved_by,
            c.first_used_at,
            c.revoked_at,
            c.revoke_effective_at,
            c.revocation_reason,
            c.revoked_by,
            c.revoke_comment,
            c.crl_number,
        ],
    )?;
    Ok(())
}

pub fn serial_exists(conn: &PgConn, serial: &str) -> PgResult<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pki_certificates WHERE serial = ?1)",
        [serial],
        |r| r.get(0),
    )
}

pub fn get_cert(conn: &PgConn, serial: &str) -> PgResult<Option<CertRow>> {
    conn.query_row(
        &format!("SELECT {CERT_COLUMNS} FROM pki_certificates WHERE serial = ?1"),
        [serial],
        cert_row,
    )
    .optional()
}

/// 某台主機的所有憑證（新到舊）
pub fn certs_of_agent(conn: &PgConn, agent_id: &str) -> PgResult<Vec<CertRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CERT_COLUMNS} FROM pki_certificates WHERE agent_id = ?1 ORDER BY issued_at DESC, serial"
    ))?;
    let rows = stmt.query_map([agent_id], cert_row)?;
    rows.collect()
}

/// 列表（主控台、CLI）：新到舊，最多 `limit` 筆，`before` 為上一頁最後一筆的 (issued_at, serial)
pub fn list_certs(
    conn: &PgConn,
    limit: usize,
    before: Option<(i64, &str)>,
) -> PgResult<Vec<CertRow>> {
    // 沒有 `before` 時不帶條件（SQLite 版以 `?1 = ?1` 佔位；PostgreSQL 推斷不出這種參數的型別）
    let rows = match before {
        Some((at, serial)) => conn
            .prepare(&format!(
                "SELECT {CERT_COLUMNS} FROM pki_certificates WHERE (issued_at, serial) < (?1, ?2)
                 ORDER BY issued_at DESC, serial DESC LIMIT ?3"
            ))?
            .query_map(params![at, serial, limit], cert_row)?,
        None => conn
            .prepare(&format!(
                "SELECT {CERT_COLUMNS} FROM pki_certificates
                 ORDER BY issued_at DESC, serial DESC LIMIT ?1"
            ))?
            .query_map([limit], cert_row)?,
    };
    rows.collect()
}

/// 這台主機先前的憑證是否用過這把公鑰（換鑰才算輪替，ENROLL.md 第 15.1 節）
pub fn spki_used_by_agent(conn: &PgConn, agent_id: &str, spki_sha256: &[u8]) -> PgResult<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pki_certificates WHERE agent_id = ?1 AND spki_sha256 = ?2)",
        params![agent_id, spki_sha256],
        |r| r.get(0),
    )
}

/// 撤銷（立即寫入；`effective_at` 為進入 CRL 的時間，一般為現在、除役為 +10 分鐘）。
/// UPDATE 帶 `revoked_at IS NULL`：並行撤銷時只有一方寫入，另一方得到 `Already`
pub fn revoke(
    conn: &PgConn,
    serial: &str,
    at_ms: i64,
    effective_at_ms: i64,
    reason: Reason,
    by: &str,
    comment: Option<&str>,
) -> PgResult<Revoke> {
    let Some(row) = get_cert(conn, serial)? else {
        return Ok(Revoke::NotFound);
    };
    if row.revoked_at.is_some() {
        return Ok(Revoke::Already(row));
    }
    let changed = conn.execute(
        "UPDATE pki_certificates SET revoked_at = ?2, revoke_effective_at = ?3, revocation_reason = ?4,
            revoked_by = ?5, revoke_comment = ?6 WHERE serial = ?1 AND revoked_at IS NULL",
        params![serial, at_ms, effective_at_ms, reason.as_str(), by, comment],
    )?;
    let current = get_cert(conn, serial)?.unwrap_or(row);
    Ok(if changed > 0 {
        Revoke::Now(current)
    } else {
        Revoke::Already(current)
    })
}

/// CRL 收錄的撤銷：這個 CA、已生效、憑證過期未超過 1 天
pub fn revocations_for_crl(conn: &PgConn, ca_id: i64, now_ms: i64) -> PgResult<Vec<RevokedEntry>> {
    let mut stmt = conn.prepare(
        "SELECT serial, revoked_at, revocation_reason FROM pki_certificates
         WHERE ca_id = ?1 AND revoked_at IS NOT NULL AND revoke_effective_at <= ?2 AND not_after > ?3
         ORDER BY serial",
    )?;
    let rows = stmt.query_map(params![ca_id, now_ms, now_ms - CRL_KEEP_EXPIRED_MS], |r| {
        let reason: Option<String> = r.get(2)?;
        Ok(RevokedEntry {
            serial: r.get(0)?,
            revoked_at_ms: r.get(1)?,
            reason: reason
                .as_deref()
                .and_then(Reason::parse)
                .unwrap_or(Reason::Unspecified),
        })
    })?;
    rows.collect()
}

/// 已生效、但還沒有收錄到任何 CRL 的撤銷（CLI 寫入或延後生效的撤銷到期時，常駐行程據此重新產生 CRL）
pub fn ca_ids_with_pending_revocations(conn: &PgConn, now_ms: i64) -> PgResult<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT ca_id FROM pki_certificates
         WHERE revoked_at IS NOT NULL AND crl_number IS NULL AND revoke_effective_at <= ?1
           AND ca_id IS NOT NULL ORDER BY ca_id",
    )?;
    let rows = stmt.query_map([now_ms], |r| r.get(0))?;
    rows.collect()
}

fn crl_row(r: &Row) -> PgResult<CrlRow> {
    Ok(CrlRow {
        ca_id: r.get(0)?,
        crl_number: r.get(1)?,
        this_update: r.get(2)?,
        next_update: r.get(3)?,
        revoked_count: r.get(4)?,
        der: r.get(5)?,
        source: r.get(6)?,
        created_by: r.get(7)?,
    })
}

/// 記錄新的 CRL：寫入 `pki_crls`、遞增 `next_crl_number`、標記這次收錄的撤銷，並只保留最近 100 版。
///
/// `kairos/HA.md` 第 5.4 節 D5：序號由呼叫端在同一個 `TxMode::Immediate` 交易中以 [`get_ca`] 讀出
/// （交易之間已由寫入鎖序列化）；兩個寫入者用到同一個序號時，`pki_crls` 的主鍵 `(ca_id, crl_number)`
/// 會讓後到的 INSERT 失敗、整個交易回復
#[allow(clippy::too_many_arguments)]
pub fn record_crl(
    conn: &PgConn,
    ca_id: i64,
    crl_number: i64,
    this_update: i64,
    next_update: i64,
    included: &[RevokedEntry],
    der: &[u8],
    source: &str,
    created_by: Option<&str>,
) -> PgResult<()> {
    conn.execute(
        "INSERT INTO pki_crls (ca_id, crl_number, this_update, next_update, revoked_count, der, source, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            ca_id,
            crl_number,
            this_update,
            next_update,
            included.len(),
            der,
            source,
            created_by
        ],
    )?;
    conn.execute(
        "UPDATE pki_cas SET next_crl_number = GREATEST(next_crl_number, ?2::bigint + 1) WHERE ca_id = ?1",
        params![ca_id, crl_number],
    )?;
    if !included.is_empty() {
        // 一個敘述標記這次收錄的所有序號（SQLite 版逐筆執行）
        conn.execute(
            "UPDATE pki_certificates SET crl_number = ?2
             WHERE serial = ANY(?1::text[]) AND crl_number IS NULL",
            params![
                array(included.iter().map(|r| r.serial.as_str())),
                crl_number
            ],
        )?;
    }
    conn.execute(
        "DELETE FROM pki_crls WHERE ca_id = ?1 AND crl_number <= ?2",
        params![ca_id, crl_number - CRL_HISTORY],
    )?;
    Ok(())
}

pub fn latest_crl(conn: &PgConn, ca_id: i64) -> PgResult<Option<CrlRow>> {
    conn.query_row(
        "SELECT ca_id, crl_number, this_update, next_update, revoked_count, der, source, created_by
         FROM pki_crls WHERE ca_id = ?1 ORDER BY crl_number DESC LIMIT 1",
        [ca_id],
        crl_row,
    )
    .optional()
}

/// CRL 版本歷史（新到舊，不含 DER 以外的大欄位時可再精簡）
pub fn crl_history(conn: &PgConn, ca_id: i64, limit: usize) -> PgResult<Vec<CrlRow>> {
    let mut stmt = conn.prepare(
        "SELECT ca_id, crl_number, this_update, next_update, revoked_count, der, source, created_by
         FROM pki_crls WHERE ca_id = ?1 ORDER BY crl_number DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![ca_id, limit], crl_row)?;
    rows.collect()
}

/// 第一次以這張憑證成功 hello：寫入 `first_used_at`。回傳是否是第一次（呼叫端據此撤銷舊憑證）
pub fn mark_first_used(conn: &PgConn, serial: &str, now_ms: i64) -> PgResult<bool> {
    Ok(conn.execute(
        "UPDATE pki_certificates SET first_used_at = ?2 WHERE serial = ?1 AND first_used_at IS NULL",
        params![serial, now_ms],
    )? > 0)
}

/// 同一台主機在 `serial` 之前簽發、尚未撤銷的憑證（輪替成功後以 `superseded` 撤銷）
pub fn older_unrevoked(conn: &PgConn, agent_id: &str, serial: &str) -> PgResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT c.serial FROM pki_certificates c, pki_certificates n
         WHERE n.serial = ?2 AND c.agent_id = ?1 AND c.serial != ?2 AND c.revoked_at IS NULL
           AND c.issued_at <= n.issued_at ORDER BY c.serial",
    )?;
    let rows = stmt.query_map(params![agent_id, serial], |r| r.get(0))?;
    rows.collect()
}

/// 依簽發 CA 統計簽發張數
pub fn issued_count_by_ca(conn: &PgConn, ca_id: i64) -> PgResult<i64> {
    conn.query_row(
        "SELECT count(*) FROM pki_certificates WHERE ca_id = ?1",
        [ca_id],
        |r| r.get(0),
    )
}

/// 已撤銷的所有序號（原本在 `alerts/mod.rs`）
pub fn revoked_serials(conn: &PgConn) -> PgResult<std::collections::HashSet<String>> {
    let mut stmt =
        conn.prepare("SELECT serial FROM pki_certificates WHERE revoked_at IS NOT NULL")?;
    stmt.query_map((), |r| r.get::<_, String>(0))?
        .collect::<PgResult<std::collections::HashSet<String>>>()
}

/// 外部簽發、撤銷已生效的序號（原本在 `enroll/mod.rs`）
pub fn revoked_external_serials(
    conn: &PgConn,
    now_ms: i64,
    expired_after_ms: i64,
) -> PgResult<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT serial FROM pki_certificates WHERE ca_id IS NULL AND revoked_at IS NOT NULL
         AND revoke_effective_at <= ?1 AND not_after > ?2",
    )?;
    let rows = stmt.query_map(params![now_ms, expired_after_ms], |r| r.get::<_, String>(0))?;
    rows.collect::<PgResult<std::collections::HashSet<String>>>()
}

/// 已撤銷而保留的憑證紀錄：主機資料已刪除（清冊中沒有）、過期超過 1 天之後刪除（註冊服務每小時的清除）。
/// SQLite 版在 `store/purge.rs`
pub fn delete_orphan_certificates(conn: &PgConn, now_ms: i64) -> PgResult<usize> {
    conn.execute(
        "DELETE FROM pki_certificates WHERE not_after + ?1::bigint <= ?2
         AND agent_id NOT IN (SELECT agent_id FROM hosts)",
        params![CRL_KEEP_AFTER_EXPIRY_MS, now_ms],
    )
}

/// 刪除前撤銷這台主機尚未撤銷、尚未過期的憑證（`cessationOfOperation`，立即生效；每張一筆稽核
/// `cert.revoke`）。回傳撤銷的序號。SQLite 版在 `store/purge.rs`。
///
/// 撤銷與稽核在同一個交易：自動提交模式下自己開交易；已經在呼叫端的交易中時直接沿用
pub fn revoke_active_certificates(
    conn: &PgConn,
    agent_id: &str,
    actor: &str,
    actor_ip: Option<&str>,
    now_ms: i64,
) -> PgResult<Vec<String>> {
    let serials: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT serial FROM pki_certificates
             WHERE agent_id = ?1 AND revoked_at IS NULL AND not_after > ?2 ORDER BY issued_at",
        )?;
        let rows = stmt.query_map(params![agent_id, now_ms], |r| r.get(0))?;
        rows.collect::<PgResult<_>>()?
    };
    if serials.is_empty() {
        return Ok(serials);
    }
    let tx = if conn.in_transaction() {
        None
    } else {
        Some(conn.begin_tx(TxMode::Deferred)?)
    };
    let mut out = Vec::new();
    for s in serials {
        if let Revoke::Now(_) = revoke(
            conn,
            &s,
            now_ms,
            now_ms,
            Reason::CessationOfOperation,
            actor,
            Some("刪除主機資料"),
        )? {
            super::audit::append(
                conn,
                now_ms,
                &crate::audit::Audit {
                    actor,
                    actor_ip,
                    category: "pki",
                    action: "cert.revoke",
                    object: Some(&s),
                    result: "ok",
                    details: Some(json!({
                        "reason": "cessationOfOperation",
                        "comment": "刪除主機資料",
                        "agent_id": agent_id,
                    })),
                    ..crate::audit::Audit::default()
                },
            )?;
            out.push(s);
        }
    }
    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(out)
}

/// 更改簽發 CA 的私鑰檔名（原本在 `pki/manage.rs`）
pub fn set_ca_key_file(conn: &PgConn, ca_id: i64, key_file: &str) -> PgResult<()> {
    conn.execute(
        "UPDATE pki_cas SET key_file = ?2 WHERE ca_id = ?1",
        params![ca_id, key_file],
    )?;
    Ok(())
}

/// 清冊中這台主機的最新憑證（原本在 `pki/queue.rs` 的 `import_existing`）
pub fn host_current_cert(
    conn: &PgConn,
    agent_id: &str,
) -> PgResult<Option<(Option<String>, Option<i64>)>> {
    conn.query_row(
        "SELECT h.current_serial, c.not_after FROM hosts h
         LEFT JOIN pki_certificates c ON c.serial = h.current_serial WHERE h.agent_id = ?1",
        [agent_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// 匯入既有憑證時新增清冊（原本在 `pki/queue.rs`）
pub fn insert_imported_host(
    conn: &PgConn,
    agent_id: &str,
    now_ms: i64,
    serial: &str,
) -> PgResult<()> {
    conn.execute(
        "INSERT INTO hosts (agent_id, hostname, online, first_seen, last_seen, source, current_serial, lifecycle)
         VALUES (?1, ?1, 0, ?2, ?2, 'cert_import', ?3, 'active')",
        params![agent_id, now_ms, serial],
    )?;
    Ok(())
}

/// 改清冊的最新憑證（原本在 `pki/queue.rs`）
pub fn set_host_current_serial(conn: &PgConn, agent_id: &str, serial: &str) -> PgResult<()> {
    conn.execute(
        "UPDATE hosts SET current_serial = ?2 WHERE agent_id = ?1",
        params![agent_id, serial],
    )?;
    Ok(())
}
