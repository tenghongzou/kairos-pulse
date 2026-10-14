//! `pki_cas`、`pki_certificates`、`pki_crls` 的讀寫（schema v4，`kairos/PKI.md` 第 7 節）。
//!
//! 所有函式都是同步的，呼叫端在 blocking 執行緒（或 CLI）上使用；寫入多筆的操作由呼叫端包在交易中。

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::issue::{Reason, RevokedEntry};

/// `pki_cas.state`
pub const CA_PENDING: &str = "pending";
pub const CA_ACTIVE: &str = "active";
pub const CA_TRUSTED: &str = "trusted";
pub const CA_RETIRED: &str = "retired";

/// CRL 收錄撤銷的期限：憑證過期超過 1 天後不再收錄（`kairos/PKI.md` 第 10 節）
pub const CRL_KEEP_EXPIRED_MS: i64 = 86_400_000;
/// 每個 CA 保留的 CRL 版本數
pub const CRL_HISTORY: i64 = 100;

/// 代理簽發 CA
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaRow {
    pub ca_id: i64,
    pub subject: String,
    pub spki_sha256: Vec<u8>,
    pub cert_der: Option<Vec<u8>>,
    pub not_before: Option<i64>,
    pub not_after: Option<i64>,
    pub key_file: Option<String>,
    pub state: String,
    pub next_crl_number: i64,
    pub created_at: i64,
    pub retired_at: Option<i64>,
}

const CA_COLUMNS: &str = "ca_id, subject, spki_sha256, cert_der, not_before, not_after, key_file, state, next_crl_number, created_at, retired_at";

fn ca_row(r: &Row<'_>) -> rusqlite::Result<CaRow> {
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
    conn: &Connection,
    subject_of: impl Fn(i64) -> String,
    spki_sha256: &[u8],
    key_file: impl Fn(i64) -> String,
    now_ms: i64,
) -> rusqlite::Result<i64> {
    // 先以暫時的 subject 取得 ca_id，再寫入依 ca_id 命名的 subject 與私鑰檔名
    conn.execute(
        "INSERT INTO pki_cas (subject, spki_sha256, state, created_at) VALUES ('', ?1, ?2, ?3)",
        params![spki_sha256, CA_PENDING, now_ms],
    )?;
    let ca_id = conn.last_insert_rowid();
    conn.execute(
        "UPDATE pki_cas SET subject = ?2, key_file = ?3 WHERE ca_id = ?1",
        params![ca_id, subject_of(ca_id), key_file(ca_id)],
    )?;
    Ok(ca_id)
}

/// 下一個 `ca_id`（產生 CSR 時 subject 要含 ca_id）
pub fn next_ca_id(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT coalesce(max(ca_id), 0) + 1 FROM pki_cas", [], |r| {
        r.get(0)
    })
}

pub fn list_cas(conn: &Connection) -> rusqlite::Result<Vec<CaRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {CA_COLUMNS} FROM pki_cas ORDER BY ca_id"))?;
    let rows = stmt.query_map([], ca_row)?;
    rows.collect()
}

pub fn get_ca(conn: &Connection, ca_id: i64) -> rusqlite::Result<Option<CaRow>> {
    conn.query_row(
        &format!("SELECT {CA_COLUMNS} FROM pki_cas WHERE ca_id = ?1"),
        [ca_id],
        ca_row,
    )
    .optional()
}

pub fn find_ca_by_spki(conn: &Connection, spki_sha256: &[u8]) -> rusqlite::Result<Option<CaRow>> {
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
    conn: &Connection,
    ca_id: i64,
    subject: &str,
    cert_der: &[u8],
    not_before: i64,
    not_after: i64,
) -> rusqlite::Result<bool> {
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
pub fn activate_ca(conn: &Connection, ca_id: i64) -> rusqlite::Result<()> {
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
pub fn retire_ca(conn: &Connection, ca_id: i64, now_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE pki_cas SET state = ?2, retired_at = ?3 WHERE ca_id = ?1",
        params![ca_id, CA_RETIRED, now_ms],
    )?;
    Ok(())
}

/// 目前的信任錨（`active`、`trusted` 且已安裝憑證）
pub fn trust_anchor_cas(conn: &Connection) -> rusqlite::Result<Vec<CaRow>> {
    Ok(list_cas(conn)?
        .into_iter()
        .filter(|c| (c.state == CA_ACTIVE || c.state == CA_TRUSTED) && c.cert_der.is_some())
        .collect())
}

/// 簽發紀錄
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CertRow {
    pub serial: String,
    pub ca_id: Option<i64>,
    pub agent_id: String,
    pub spki_sha256: Vec<u8>,
    pub not_before: i64,
    pub not_after: i64,
    pub cert_der: Vec<u8>,
    pub issued_at: i64,
    pub issued_via: String,
    pub request_id: Option<String>,
    pub approved_by: Option<String>,
    pub first_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub revoke_effective_at: Option<i64>,
    pub revocation_reason: Option<String>,
    pub revoked_by: Option<String>,
    pub revoke_comment: Option<String>,
    pub crl_number: Option<i64>,
}

const CERT_COLUMNS: &str = "serial, ca_id, agent_id, spki_sha256, not_before, not_after, cert_der, issued_at, issued_via, request_id, approved_by, first_used_at, revoked_at, revoke_effective_at, revocation_reason, revoked_by, revoke_comment, crl_number";

fn cert_row(r: &Row<'_>) -> rusqlite::Result<CertRow> {
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

impl CertRow {
    /// 撤銷已生效（`revoke_effective_at` 已到）
    pub fn revoked_effective(&self, now_ms: i64) -> bool {
        self.revoke_effective_at.is_some_and(|t| t <= now_ms)
    }
}

pub fn insert_cert(conn: &Connection, c: &CertRow) -> rusqlite::Result<()> {
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

pub fn serial_exists(conn: &Connection, serial: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pki_certificates WHERE serial = ?1)",
        [serial],
        |r| r.get(0),
    )
}

pub fn get_cert(conn: &Connection, serial: &str) -> rusqlite::Result<Option<CertRow>> {
    conn.query_row(
        &format!("SELECT {CERT_COLUMNS} FROM pki_certificates WHERE serial = ?1"),
        [serial],
        cert_row,
    )
    .optional()
}

/// 某台主機的所有憑證（新到舊）
pub fn certs_of_agent(conn: &Connection, agent_id: &str) -> rusqlite::Result<Vec<CertRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CERT_COLUMNS} FROM pki_certificates WHERE agent_id = ?1 ORDER BY issued_at DESC, serial"
    ))?;
    let rows = stmt.query_map([agent_id], cert_row)?;
    rows.collect()
}

/// 列表（主控台、CLI）：新到舊，最多 `limit` 筆，`before` 為上一頁最後一筆的 (issued_at, serial)
pub fn list_certs(
    conn: &Connection,
    limit: usize,
    before: Option<(i64, &str)>,
) -> rusqlite::Result<Vec<CertRow>> {
    let (cond, at, serial) = match before {
        Some((at, s)) => ("WHERE (issued_at, serial) < (?1, ?2)", at, s.to_string()),
        None => ("WHERE ?1 = ?1 AND ?2 = ?2", 0, String::new()),
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {CERT_COLUMNS} FROM pki_certificates {cond} ORDER BY issued_at DESC, serial DESC LIMIT ?3"
    ))?;
    let rows = stmt.query_map(params![at, serial, limit as i64], cert_row)?;
    rows.collect()
}

/// 這台主機先前的憑證是否用過這把公鑰（換鑰才算輪替，ENROLL.md 第 15.1 節）
pub fn spki_used_by_agent(
    conn: &Connection,
    agent_id: &str,
    spki_sha256: &[u8],
) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pki_certificates WHERE agent_id = ?1 AND spki_sha256 = ?2)",
        params![agent_id, spki_sha256],
        |r| r.get(0),
    )
}

/// 撤銷的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revoke {
    Now(CertRow),
    Already(CertRow),
    NotFound,
}

/// 撤銷（立即寫入；`effective_at` 為進入 CRL 的時間，一般為現在、除役為 +10 分鐘）
pub fn revoke(
    conn: &Connection,
    serial: &str,
    at_ms: i64,
    effective_at_ms: i64,
    reason: Reason,
    by: &str,
    comment: Option<&str>,
) -> rusqlite::Result<Revoke> {
    let Some(row) = get_cert(conn, serial)? else {
        return Ok(Revoke::NotFound);
    };
    if row.revoked_at.is_some() {
        return Ok(Revoke::Already(row));
    }
    conn.execute(
        "UPDATE pki_certificates SET revoked_at = ?2, revoke_effective_at = ?3, revocation_reason = ?4,
            revoked_by = ?5, revoke_comment = ?6 WHERE serial = ?1 AND revoked_at IS NULL",
        params![serial, at_ms, effective_at_ms, reason.as_str(), by, comment],
    )?;
    Ok(Revoke::Now(get_cert(conn, serial)?.unwrap_or(row)))
}

/// CRL 收錄的撤銷：這個 CA、已生效、憑證過期未超過 1 天
pub fn revocations_for_crl(
    conn: &Connection,
    ca_id: i64,
    now_ms: i64,
) -> rusqlite::Result<Vec<RevokedEntry>> {
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
pub fn ca_ids_with_pending_revocations(
    conn: &Connection,
    now_ms: i64,
) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT ca_id FROM pki_certificates
         WHERE revoked_at IS NOT NULL AND crl_number IS NULL AND revoke_effective_at <= ?1
           AND ca_id IS NOT NULL",
    )?;
    let rows = stmt.query_map([now_ms], |r| r.get(0))?;
    rows.collect()
}

/// 下一個尚未生效的撤銷時間（除役的 10 分鐘視窗）
pub fn next_pending_effective(conn: &Connection, now_ms: i64) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT min(revoke_effective_at) FROM pki_certificates
         WHERE revoked_at IS NOT NULL AND crl_number IS NULL AND revoke_effective_at > ?1",
        [now_ms],
        |r| r.get(0),
    )
}

/// CRL 的一個版本
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrlRow {
    pub ca_id: i64,
    pub crl_number: i64,
    pub this_update: i64,
    pub next_update: i64,
    pub revoked_count: i64,
    pub der: Vec<u8>,
    pub source: String,
    pub created_by: Option<String>,
}

fn crl_row(r: &Row<'_>) -> rusqlite::Result<CrlRow> {
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

/// 記錄新的 CRL：寫入 `pki_crls`、遞增 `next_crl_number`、標記這次收錄的撤銷，並只保留最近 100 版
#[allow(clippy::too_many_arguments)]
pub fn record_crl(
    conn: &Connection,
    ca_id: i64,
    crl_number: i64,
    this_update: i64,
    next_update: i64,
    included: &[RevokedEntry],
    der: &[u8],
    source: &str,
    created_by: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO pki_crls (ca_id, crl_number, this_update, next_update, revoked_count, der, source, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            ca_id,
            crl_number,
            this_update,
            next_update,
            included.len() as i64,
            der,
            source,
            created_by
        ],
    )?;
    conn.execute(
        "UPDATE pki_cas SET next_crl_number = max(next_crl_number, ?2 + 1) WHERE ca_id = ?1",
        params![ca_id, crl_number],
    )?;
    {
        let mut stmt = conn.prepare(
            "UPDATE pki_certificates SET crl_number = ?2 WHERE serial = ?1 AND crl_number IS NULL",
        )?;
        for r in included {
            stmt.execute(params![r.serial, crl_number])?;
        }
    }
    conn.execute(
        "DELETE FROM pki_crls WHERE ca_id = ?1 AND crl_number <= ?2",
        params![ca_id, crl_number - CRL_HISTORY],
    )?;
    Ok(())
}

pub fn latest_crl(conn: &Connection, ca_id: i64) -> rusqlite::Result<Option<CrlRow>> {
    conn.query_row(
        "SELECT ca_id, crl_number, this_update, next_update, revoked_count, der, source, created_by
         FROM pki_crls WHERE ca_id = ?1 ORDER BY crl_number DESC LIMIT 1",
        [ca_id],
        crl_row,
    )
    .optional()
}

/// CRL 版本歷史（新到舊，不含 DER 以外的大欄位時可再精簡）
pub fn crl_history(conn: &Connection, ca_id: i64, limit: usize) -> rusqlite::Result<Vec<CrlRow>> {
    let mut stmt = conn.prepare(
        "SELECT ca_id, crl_number, this_update, next_update, revoked_count, der, source, created_by
         FROM pki_crls WHERE ca_id = ?1 ORDER BY crl_number DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![ca_id, limit as i64], crl_row)?;
    rows.collect()
}

/// 第一次以這張憑證成功 hello：寫入 `first_used_at`。回傳是否是第一次（呼叫端據此撤銷舊憑證）
pub fn mark_first_used(conn: &Connection, serial: &str, now_ms: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE pki_certificates SET first_used_at = ?2 WHERE serial = ?1 AND first_used_at IS NULL",
        params![serial, now_ms],
    )? > 0)
}

/// 同一台主機在 `serial` 之前簽發、尚未撤銷的憑證（輪替成功後以 `superseded` 撤銷）
pub fn older_unrevoked(
    conn: &Connection,
    agent_id: &str,
    serial: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT c.serial FROM pki_certificates c, pki_certificates n
         WHERE n.serial = ?2 AND c.agent_id = ?1 AND c.serial != ?2 AND c.revoked_at IS NULL
           AND c.issued_at <= n.issued_at",
    )?;
    let rows = stmt.query_map(params![agent_id, serial], |r| r.get(0))?;
    rows.collect()
}

/// 依簽發 CA 統計簽發張數
pub fn issued_count_by_ca(conn: &Connection, ca_id: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM pki_certificates WHERE ca_id = ?1",
        [ca_id],
        |r| r.get(0),
    )
}
