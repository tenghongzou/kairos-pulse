//! 註冊通道的 SQLite 實作（[`crate::enroll::store`] 的 [`EnrollStore`]），以及 `settings` 表
//! （[`crate::settings::SettingsStore`]）。SQL 原封不動自 `enroll/store.rs` 搬來。

use kairos_proto::enroll::TokenApproval;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

use super::conn::{AsConn, forward};
#[allow(unused_imports)]
use crate::enroll::store::*;
use crate::settings::SettingsStore;

impl<C: AsConn> EnrollStore for C {
    forward! {
        fn insert_token(&self, t: &TokenRow) -> () => insert_token;
        fn token_by_hash(&self, hash: &[u8]) -> Option<TokenRow> => token_by_hash;
        fn get_token(&self, token_id: &str) -> Option<TokenRow> => get_token;
        fn list_tokens(&self) -> Vec<TokenRow> => list_tokens;
        fn disable_token(&self, token_id: &str, by: &str, reason: Option<&str>, now_ms: i64) -> Option<(TokenRow, bool)> => disable_token;
        fn consume_token(&self, token_id: &str, now_ms: i64) -> () => consume_token;
        fn token_validity(&self, now_ms: i64) -> (usize, Option<i64>) => token_validity;
        fn batch_token_cidrs(&self, now_ms: i64) -> Vec<String> => batch_token_cidrs;
        fn insert_request(&self, q: &RequestRow) -> () => insert_request;
        fn get_request(&self, request_id: &str) -> Option<RequestRow> => get_request;
        fn request_by_csr(&self, agent_id: &str, csr_sha256: &[u8]) -> Option<RequestRow> => request_by_csr;
        fn requests_by_code(&self, code: &str) -> Vec<RequestRow> => requests_by_code;
        fn list_requests(&self, f: &RequestFilter) -> Vec<RequestRow> => list_requests;
        fn pending_count(&self) -> i64 => pending_count;
        fn pending_count_from(&self, source_ip: &str) -> i64 => pending_count_from;
        fn recent_count_from(&self, source_ip: &str, since_ms: i64) -> i64 => recent_count_from;
        fn replace_pending_for_agent(&self, agent_id: &str, keep_request_id: &str, now_ms: i64) -> Vec<String> => replace_pending_for_agent;
        fn hostname_in_use(&self, hostname: &str, agent_id: &str) -> bool => hostname_in_use;
        fn update_poll_secret(&self, request_id: &str, hash: &[u8]) -> () => update_poll_secret;
        fn approve(&self, request_id: &str, by: &str, lifetime_days: Option<i64>, now_ms: i64) -> bool => approve;
        fn reject(&self, request_id: &str, by: &str, code: &str, reason: &str, now_ms: i64) -> bool => reject;
        fn mark_issued(&self, request_id: &str, serial: &str, decided_by: &str, now_ms: i64) -> bool => mark_issued;
        fn set_issue_error(&self, request_id: &str, error: &str) -> () => set_issue_error;
        fn withdraw(&self, request_id: &str, reason: &str, initiated_by: &str, ip: &str, now_ms: i64) -> bool => withdraw;
        fn mark_delivered(&self, request_id: &str, now_ms: i64) -> () => mark_delivered;
        fn add_flag(&self, request_id: &str, flag: &str) -> () => add_flag;
        fn approved_requests(&self, limit: usize) -> Vec<RequestRow> => approved_requests;
        fn expire_and_purge(&self, now_ms: i64, closed_keep_ms: i64) -> (usize, usize) => expire_and_purge;
        fn replay_seen(&self, key: &[u8], now_ms: i64) -> bool => replay_seen;
        fn replay_insert(&self, key: &[u8], agent_id: &str, now_ms: i64, ttl_ms: i64) -> () => replay_insert;
        fn replay_count(&self, now_ms: i64) -> i64 => replay_count;
        fn replay_purge(&self, now_ms: i64) -> usize => replay_purge;
        fn load_blocks(&self) -> Vec<BlockRow> => load_blocks;
        fn upsert_block(&self, b: &BlockRow) -> () => upsert_block;
        fn purge_blocks(&self, now_ms: i64) -> usize => purge_blocks;
        fn active_relaxations(&self, now_ms: i64) -> Vec<String> => active_relaxations;
        fn purge_relaxations(&self, now_ms: i64) -> usize => purge_relaxations;
        #[cfg(test)]
        fn recent_audit(&self, limit: usize) -> Vec<(i64, String, String, Option<String>, String, Option<String>)> => recent_audit;
        fn host_enroll(&self, agent_id: &str) -> Option<HostEnroll> => host_enroll;
        fn upsert_host_on_issue(&self, agent_id: &str, hostname: &str, host_fingerprint: Option<&str>, display_name: Option<&str>, tags: Option<&str>, os: Option<&str>, serial: &str, now_ms: i64) -> () => upsert_host_on_issue;
        fn set_host_group_from_token(&self, agent_id: &str, token_id: Option<&str>) -> () => set_host_group_from_token;
        fn decommission_host(&self, agent_id: &str, hostname: &str, by: &str, now_ms: i64) -> bool => decommission_host;
        fn set_grace_override(&self, agent_id: &str, until_ms: Option<i64>) -> bool => set_grace_override;
        fn set_last_grace_attempt(&self, agent_id: &str, attempt: &Value) -> () => set_last_grace_attempt;
    }
}

impl<C: AsConn> SettingsStore for C {
    forward! {
        fn load_settings(&self) -> Vec<(String, Value, i64, String)> => load_settings;
        fn set_setting(&self, key: &str, value: &Value, by: &str, now_ms: i64) -> () => set_setting;
        fn delete_setting(&self, key: &str) -> bool => delete_setting;
        fn get_meta(&self, key: &str) -> Option<String> => super::alerts::get_meta;
        fn put_meta(&self, key: &str, value: &str) -> () => super::alerts::put_meta;
    }
}

const TOKEN_COLUMNS: &str = "token_id, secret_hash, hint, name, group_name, tags, approval, max_uses, used, expires_at, allowed_cidrs, batch_mode, allowed_agent_ids, cert_lifetime_days, created_at, created_by, disabled_at, disabled_by, disable_reason, last_used_at";

fn token_row(r: &Row<'_>) -> rusqlite::Result<TokenRow> {
    let approval: String = r.get(6)?;
    Ok(TokenRow {
        token_id: r.get(0)?,
        secret_hash: r.get(1)?,
        hint: r.get(2)?,
        name: r.get(3)?,
        group_name: r.get(4)?,
        tags: json_list(r.get(5)?),
        approval: if approval == "auto" {
            TokenApproval::Auto
        } else {
            TokenApproval::Manual
        },
        max_uses: r.get(7)?,
        used: r.get(8)?,
        expires_at: r.get(9)?,
        allowed_cidrs: json_list(r.get(10)?),
        batch_mode: r.get::<_, i64>(11)? != 0,
        allowed_agent_ids: json_list(r.get(12)?),
        cert_lifetime_days: r.get(13)?,
        created_at: r.get(14)?,
        created_by: r.get(15)?,
        disabled_at: r.get(16)?,
        disabled_by: r.get(17)?,
        disable_reason: r.get(18)?,
        last_used_at: r.get(19)?,
    })
}

pub fn insert_token(conn: &Connection, t: &TokenRow) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO enroll_tokens ({TOKEN_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)"
        ),
        params![
            t.token_id,
            t.secret_hash,
            t.hint,
            t.name,
            t.group_name,
            list_json(&t.tags),
            approval_str(t.approval),
            t.max_uses,
            t.used,
            t.expires_at,
            list_json(&t.allowed_cidrs),
            i64::from(t.batch_mode),
            list_json(&t.allowed_agent_ids),
            t.cert_lifetime_days,
            t.created_at,
            t.created_by,
            t.disabled_at,
            t.disabled_by,
            t.disable_reason,
            t.last_used_at,
        ],
    )?;
    Ok(())
}

pub fn token_by_hash(conn: &Connection, hash: &[u8]) -> rusqlite::Result<Option<TokenRow>> {
    conn.query_row(
        &format!("SELECT {TOKEN_COLUMNS} FROM enroll_tokens WHERE secret_hash = ?1"),
        [hash],
        token_row,
    )
    .optional()
}

pub fn get_token(conn: &Connection, token_id: &str) -> rusqlite::Result<Option<TokenRow>> {
    conn.query_row(
        &format!("SELECT {TOKEN_COLUMNS} FROM enroll_tokens WHERE token_id = ?1"),
        [token_id],
        token_row,
    )
    .optional()
}

/// 所有 token（新到舊）
pub fn list_tokens(conn: &Connection) -> rusqlite::Result<Vec<TokenRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TOKEN_COLUMNS} FROM enroll_tokens ORDER BY created_at DESC, token_id"
    ))?;
    let rows = stmt.query_map([], token_row)?;
    rows.collect()
}

/// 停用（冪等）：回傳停用後的列，不存在時 `None`；`already` 為先前已停用
pub fn disable_token(
    conn: &Connection,
    token_id: &str,
    by: &str,
    reason: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<Option<(TokenRow, bool)>> {
    let Some(row) = get_token(conn, token_id)? else {
        return Ok(None);
    };
    if row.disabled_at.is_some() {
        return Ok(Some((row, true)));
    }
    conn.execute(
        "UPDATE enroll_tokens SET disabled_at = ?2, disabled_by = ?3, disable_reason = ?4 WHERE token_id = ?1",
        params![token_id, now_ms, by, reason],
    )?;
    Ok(get_token(conn, token_id)?.map(|r| (r, false)))
}

/// 使用次數 +1（建立申請時，在同一個交易中）
pub fn consume_token(conn: &Connection, token_id: &str, now_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE enroll_tokens SET used = used + 1, last_used_at = ?2 WHERE token_id = ?1",
        params![token_id, now_ms],
    )?;
    Ok(())
}

/// 目前是否有有效 token（未停用、未過期、未用完），以及最後一個 token 失效的時間（自動暫停的計時起點）
pub fn token_validity(conn: &Connection, now_ms: i64) -> rusqlite::Result<(usize, Option<i64>)> {
    let tokens = list_tokens(conn)?;
    let active = tokens
        .iter()
        .filter(|t| t.state(now_ms) == TokenState::Active)
        .count();
    let last_end = tokens
        .iter()
        .filter_map(|t| match t.state(now_ms) {
            TokenState::Active => None,
            TokenState::Disabled => t.disabled_at.map(|d| d.min(t.expires_at)),
            TokenState::Expired => Some(t.expires_at),
            // 用完的時間以最後一次使用近似
            TokenState::Exhausted => t.last_used_at.or(Some(t.created_at)),
        })
        .max();
    Ok((active, last_end))
}

/// 以批次部署模式生效中的 token 的限定網段（`enroll-rate-limit.md` §4.3）
pub fn batch_token_cidrs(conn: &Connection, now_ms: i64) -> rusqlite::Result<Vec<String>> {
    Ok(list_tokens(conn)?
        .into_iter()
        .filter(|t| t.batch_mode && t.state(now_ms) == TokenState::Active)
        .flat_map(|t| t.allowed_cidrs.unwrap_or_default())
        .collect())
}

const REQUEST_COLUMNS: &str = "request_id, kind, agent_id, request_code, state, csr_der, csr_sha256, spki_sha256, poll_secret_hash, token_id, expired_serial, previous_serial, source_ip, hostname, display_name, tags, os, host_fingerprint, pulse_version, server_verification, flags, cert_lifetime_days, created_at, expires_at, decided_at, decided_by, reject_reason_code, reject_reason, issued_serial, issue_error, delivered_at, withdrawn_at, withdraw_reason, withdraw_initiated_by, withdraw_ip, closed_at";

fn request_row(r: &Row<'_>) -> rusqlite::Result<RequestRow> {
    let kind: String = r.get(1)?;
    let state: String = r.get(4)?;
    Ok(RequestRow {
        request_id: r.get(0)?,
        kind: DbKind::parse(&kind).unwrap_or(DbKind::Enroll),
        agent_id: r.get(2)?,
        request_code: r.get(3)?,
        state: DbState::parse(&state).unwrap_or(DbState::RequestExpired),
        csr_der: r.get(5)?,
        csr_sha256: r.get(6)?,
        spki_sha256: r.get(7)?,
        poll_secret_hash: r.get(8)?,
        token_id: r.get(9)?,
        expired_serial: r.get(10)?,
        previous_serial: r.get(11)?,
        source_ip: r.get(12)?,
        hostname: r.get(13)?,
        display_name: r.get(14)?,
        tags: json_list(r.get(15)?),
        os: r.get(16)?,
        host_fingerprint: r.get(17)?,
        pulse_version: r.get(18)?,
        server_verification: r.get(19)?,
        flags: json_list(r.get(20)?).unwrap_or_default(),
        cert_lifetime_days: r.get(21)?,
        created_at: r.get(22)?,
        expires_at: r.get(23)?,
        decided_at: r.get(24)?,
        decided_by: r.get(25)?,
        reject_reason_code: r.get(26)?,
        reject_reason: r.get(27)?,
        issued_serial: r.get(28)?,
        issue_error: r.get(29)?,
        delivered_at: r.get(30)?,
        withdrawn_at: r.get(31)?,
        withdraw_reason: r.get(32)?,
        withdraw_initiated_by: r.get(33)?,
        withdraw_ip: r.get(34)?,
        closed_at: r.get(35)?,
    })
}

pub fn insert_request(conn: &Connection, q: &RequestRow) -> rusqlite::Result<()> {
    let flags = (!q.flags.is_empty()).then(|| Value::from(q.flags.clone()).to_string());
    conn.execute(
        &format!(
            "INSERT INTO enroll_requests ({REQUEST_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36)"
        ),
        rusqlite::params_from_iter([
            &q.request_id as &dyn rusqlite::ToSql,
            &q.kind.as_str(),
            &q.agent_id,
            &q.request_code,
            &q.state.as_str(),
            &q.csr_der,
            &q.csr_sha256,
            &q.spki_sha256,
            &q.poll_secret_hash,
            &q.token_id,
            &q.expired_serial,
            &q.previous_serial,
            &q.source_ip,
            &q.hostname,
            &q.display_name,
            &list_json(&q.tags),
            &q.os,
            &q.host_fingerprint,
            &q.pulse_version,
            &q.server_verification,
            &flags,
            &q.cert_lifetime_days,
            &q.created_at,
            &q.expires_at,
            &q.decided_at,
            &q.decided_by,
            &q.reject_reason_code,
            &q.reject_reason,
            &q.issued_serial,
            &q.issue_error,
            &q.delivered_at,
            &q.withdrawn_at,
            &q.withdraw_reason,
            &q.withdraw_initiated_by,
            &q.withdraw_ip,
            &q.closed_at,
        ]),
    )?;
    Ok(())
}

pub fn get_request(conn: &Connection, request_id: &str) -> rusqlite::Result<Option<RequestRow>> {
    conn.query_row(
        &format!("SELECT {REQUEST_COLUMNS} FROM enroll_requests WHERE request_id = ?1"),
        [request_id],
        request_row,
    )
    .optional()
}

/// 冪等鍵：同一 `agent_id`＋同一 CSR
pub fn request_by_csr(
    conn: &Connection,
    agent_id: &str,
    csr_sha256: &[u8],
) -> rusqlite::Result<Option<RequestRow>> {
    conn.query_row(
        &format!(
            "SELECT {REQUEST_COLUMNS} FROM enroll_requests WHERE agent_id = ?1 AND csr_sha256 = ?2"
        ),
        params![agent_id, csr_sha256],
        request_row,
    )
    .optional()
}

/// 申請編號（不含連字號、大寫）對到的申請（不保證唯一）
pub fn requests_by_code(conn: &Connection, code: &str) -> rusqlite::Result<Vec<RequestRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {REQUEST_COLUMNS} FROM enroll_requests WHERE request_code = ?1 ORDER BY created_at DESC"
    ))?;
    let rows = stmt.query_map([code], request_row)?;
    rows.collect()
}

pub fn list_requests(conn: &Connection, f: &RequestFilter) -> rusqlite::Result<Vec<RequestRow>> {
    let mut sql = format!("SELECT {REQUEST_COLUMNS} FROM enroll_requests WHERE ");
    sql.push_str(if f.closed {
        "state NOT IN ('pending_approval', 'approved')"
    } else {
        "state IN ('pending_approval', 'approved')"
    });
    let mut args: Vec<String> = Vec::new();
    if let Some(t) = &f.token_id {
        args.push(t.clone());
        sql.push_str(&format!(" AND token_id = ?{}", args.len()));
    }
    if let Some(k) = f.kind {
        args.push(k.as_str().to_string());
        sql.push_str(&format!(" AND kind = ?{}", args.len()));
    }
    if let Some(s) = f.state {
        args.push(s.as_str().to_string());
        sql.push_str(&format!(" AND state = ?{}", args.len()));
    }
    sql.push_str(if f.closed {
        " ORDER BY closed_at DESC, request_id"
    } else {
        " ORDER BY created_at, request_id"
    });
    sql.push_str(&format!(" LIMIT {}", f.limit.max(1)));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), request_row)?;
    rows.collect()
}

/// 待核准佇列的筆數（L9）
pub fn pending_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM enroll_requests WHERE state IN ('pending_approval', 'approved')",
        [],
        |r| r.get(0),
    )
}

/// 同一來源的待核准筆數（動態 `poll_after_ms`）
pub fn pending_count_from(conn: &Connection, source_ip: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM enroll_requests WHERE source_ip = ?1 AND state = 'pending_approval'",
        [source_ip],
        |r| r.get(0),
    )
}

/// 最近 `window_ms` 內同一來源建立的申請數（異常旗標 `burst_from_ip`）
pub fn recent_count_from(
    conn: &Connection,
    source_ip: &str,
    since_ms: i64,
) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM enroll_requests WHERE source_ip = ?1 AND created_at >= ?2",
        params![source_ip, since_ms],
        |r| r.get(0),
    )
}

/// 同一 `agent_id` 其他待核准的新註冊申請改為 `replaced`（回傳被取代的 request_id）
pub fn replace_pending_for_agent(
    conn: &Connection,
    agent_id: &str,
    keep_request_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT request_id FROM enroll_requests
         WHERE agent_id = ?1 AND kind = 'enroll' AND state = 'pending_approval' AND request_id != ?2",
    )?;
    let ids: Vec<String> = stmt
        .query_map(params![agent_id, keep_request_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in &ids {
        conn.execute(
            "UPDATE enroll_requests SET state = 'replaced', closed_at = ?2 WHERE request_id = ?1",
            params![id, now_ms],
        )?;
    }
    Ok(ids)
}

/// 其他主機名稱相同、待核准或已簽發的申請是否存在（異常旗標 `duplicate_hostname`）
pub fn hostname_in_use(
    conn: &Connection,
    hostname: &str,
    agent_id: &str,
) -> rusqlite::Result<bool> {
    let in_requests: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM enroll_requests WHERE hostname = ?1 AND agent_id != ?2
            AND state IN ('pending_approval', 'approved'))",
        params![hostname, agent_id],
        |r| r.get(0),
    )?;
    if in_requests {
        return Ok(true);
    }
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM hosts WHERE hostname = ?1 AND agent_id != ?2 AND lifecycle = 'active')",
        params![hostname, agent_id],
        |r| r.get(0),
    )
}

/// 更新 `poll_secret_hash`（過期續簽的冪等重送：待核准時以這次的值為準）
pub fn update_poll_secret(
    conn: &Connection,
    request_id: &str,
    hash: &[u8],
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE enroll_requests SET poll_secret_hash = ?2 WHERE request_id = ?1",
        params![request_id, hash],
    )?;
    Ok(())
}

/// 核准（不簽發）：`pending_approval` → `approved`。回傳是否有變更（狀態已不同時 `false`）
pub fn approve(
    conn: &Connection,
    request_id: &str,
    by: &str,
    lifetime_days: Option<i64>,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE enroll_requests SET state = 'approved', decided_at = ?2, decided_by = ?3,
            cert_lifetime_days = coalesce(?4, cert_lifetime_days)
         WHERE request_id = ?1 AND state = 'pending_approval'",
        params![request_id, now_ms, by, lifetime_days],
    )? > 0)
}

/// 拒絕：待核准 → `rejected`
pub fn reject(
    conn: &Connection,
    request_id: &str,
    by: &str,
    code: &str,
    reason: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE enroll_requests SET state = 'rejected', decided_at = ?2, decided_by = ?3,
            reject_reason_code = ?4, reject_reason = ?5, closed_at = ?2
         WHERE request_id = ?1 AND state IN ('pending_approval', 'approved')",
        params![request_id, now_ms, by, code, reason],
    )? > 0)
}

/// 簽發完成：`issued`（決定者沿用核准時的值；自動核准時寫入 `decided_by`）
pub fn mark_issued(
    conn: &Connection,
    request_id: &str,
    serial: &str,
    decided_by: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE enroll_requests SET state = 'issued', issued_serial = ?2,
            decided_at = coalesce(decided_at, ?3), decided_by = coalesce(decided_by, ?4),
            issue_error = NULL, closed_at = ?3
         WHERE request_id = ?1 AND state IN ('pending_approval', 'approved')",
        params![request_id, serial, now_ms, decided_by],
    )? > 0)
}

/// 簽發失敗的原因（主控台顯示；申請維持 `approved`）
pub fn set_issue_error(conn: &Connection, request_id: &str, error: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE enroll_requests SET issue_error = ?2 WHERE request_id = ?1",
        params![request_id, error],
    )?;
    Ok(())
}

/// 撤回：待核准 → `withdrawn`
pub fn withdraw(
    conn: &Connection,
    request_id: &str,
    reason: &str,
    initiated_by: &str,
    ip: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE enroll_requests SET state = 'withdrawn', withdrawn_at = ?2, withdraw_reason = ?3,
            withdraw_initiated_by = ?4, withdraw_ip = ?5, closed_at = ?2
         WHERE request_id = ?1 AND state IN ('pending_approval', 'approved')",
        params![request_id, now_ms, reason, initiated_by, ip],
    )? > 0)
}

/// 第一次在輪詢回應中交出憑證
pub fn mark_delivered(conn: &Connection, request_id: &str, now_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE enroll_requests SET delivered_at = ?2 WHERE request_id = ?1 AND delivered_at IS NULL",
        params![request_id, now_ms],
    )?;
    Ok(())
}

/// 新增異常旗標（不重複）
pub fn add_flag(conn: &Connection, request_id: &str, flag: &str) -> rusqlite::Result<()> {
    if let Some(mut r) = get_request(conn, request_id)?
        && !r.flags.iter().any(|f| f == flag)
    {
        r.flags.push(flag.to_string());
        conn.execute(
            "UPDATE enroll_requests SET flags = ?2 WHERE request_id = ?1",
            params![request_id, Value::from(r.flags).to_string()],
        )?;
    }
    Ok(())
}

/// 已核准、尚未簽發的申請（常駐行程每 2 秒簽發；離線模式的匯出）
pub fn approved_requests(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<RequestRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {REQUEST_COLUMNS} FROM enroll_requests WHERE state = 'approved' ORDER BY decided_at, request_id LIMIT ?1"
    ))?;
    let rows = stmt.query_map([limit as i64], request_row)?;
    rows.collect()
}

/// 保留清除：待核准逾期改為 `request_expired`；結束超過 `closed_keep_ms` 的刪除。回傳 (逾期, 刪除)
pub fn expire_and_purge(
    conn: &Connection,
    now_ms: i64,
    closed_keep_ms: i64,
) -> rusqlite::Result<(usize, usize)> {
    let expired = conn.execute(
        "UPDATE enroll_requests SET state = 'request_expired', closed_at = ?1
         WHERE state = 'pending_approval' AND expires_at <= ?1",
        [now_ms],
    )?;
    let purged = conn.execute(
        "DELETE FROM enroll_requests WHERE closed_at IS NOT NULL AND closed_at < ?1
            AND state NOT IN ('pending_approval', 'approved')",
        [now_ms - closed_keep_ms],
    )?;
    Ok((expired, purged))
}

// ---------------------------------------------------------------------------
// 重放快取、封鎖、暫時放寬
// ---------------------------------------------------------------------------

/// 重放快取中是否有未過期的鍵
pub fn replay_seen(conn: &Connection, key: &[u8], now_ms: i64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM enroll_replay WHERE key = ?1 AND expires_at > ?2)",
        params![key, now_ms],
        |r| r.get(0),
    )
}

/// 寫入重放快取（鍵已存在但過期時覆寫）
pub fn replay_insert(
    conn: &Connection,
    key: &[u8],
    agent_id: &str,
    now_ms: i64,
    ttl_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO enroll_replay (key, expires_at, agent_id, first_seen_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (key) DO UPDATE SET expires_at = excluded.expires_at, agent_id = excluded.agent_id,
            first_seen_at = excluded.first_seen_at",
        params![key, now_ms + ttl_ms, agent_id, now_ms],
    )?;
    Ok(())
}

/// 未過期的重放快取筆數（上限 100,000）
pub fn replay_count(conn: &Connection, now_ms: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM enroll_replay WHERE expires_at > ?1",
        [now_ms],
        |r| r.get(0),
    )
}

pub fn replay_purge(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM enroll_replay WHERE expires_at <= ?1", [now_ms])
}

pub fn load_blocks(conn: &Connection) -> rusqlite::Result<Vec<BlockRow>> {
    let mut stmt = conn.prepare(
        "SELECT source, class, blocked_until, offenses, last_offense_at FROM enroll_blocks",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(BlockRow {
            source: r.get(0)?,
            class: r.get(1)?,
            blocked_until: r.get(2)?,
            offenses: r.get(3)?,
            last_offense_at: r.get(4)?,
        })
    })?;
    rows.collect()
}

pub fn upsert_block(conn: &Connection, b: &BlockRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO enroll_blocks (source, class, blocked_until, offenses, last_offense_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (source, class) DO UPDATE SET blocked_until = excluded.blocked_until,
            offenses = excluded.offenses, last_offense_at = excluded.last_offense_at",
        params![
            b.source,
            b.class,
            b.blocked_until,
            b.offenses,
            b.last_offense_at
        ],
    )?;
    Ok(())
}

/// 封鎖到期、且 24 小時沒有再犯的列刪除
pub fn purge_blocks(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM enroll_blocks WHERE blocked_until <= ?1 AND last_offense_at <= ?2",
        params![now_ms, now_ms - 86_400_000],
    )
}

/// 生效中的暫時放寬（CIDR）
pub fn active_relaxations(conn: &Connection, now_ms: i64) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT cidr FROM enroll_relaxations WHERE expires_at > ?1")?;
    let rows = stmt.query_map([now_ms], |r| r.get(0))?;
    rows.collect()
}

pub fn purge_relaxations(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM enroll_relaxations WHERE expires_at <= ?1",
        [now_ms],
    )
}

// ---------------------------------------------------------------------------
// settings、稽核
// ---------------------------------------------------------------------------

/// `settings` 表的所有值：(鍵, JSON 值, 更新時間, 更新者)
pub fn load_settings(conn: &Connection) -> rusqlite::Result<Vec<(String, Value, i64, String)>> {
    let mut stmt = conn.prepare("SELECT key, value, updated_at, updated_by FROM settings")?;
    let rows = stmt.query_map([], |r| {
        let v: String = r.get(1)?;
        Ok((
            r.get(0)?,
            serde_json::from_str(&v).unwrap_or(Value::Null),
            r.get(2)?,
            r.get(3)?,
        ))
    })?;
    rows.collect()
}

pub fn set_setting(
    conn: &Connection,
    key: &str,
    value: &Value,
    by: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at,
            updated_by = excluded.updated_by",
        params![key, value.to_string(), now_ms, by],
    )?;
    Ok(())
}

/// 刪除 `settings` 的鍵（改用設定檔的值）；回傳是否存在
pub fn delete_setting(conn: &Connection, key: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM settings WHERE key = ?1", [key])? > 0)
}

/// 最近的稽核事件（測試與 CLI）：(時間, 執行者, 動作, 對象, 結果, 代碼)
#[cfg(test)]
#[allow(clippy::type_complexity)]
pub fn recent_audit(
    conn: &Connection,
    limit: usize,
) -> rusqlite::Result<Vec<(i64, String, String, Option<String>, String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT at, actor, action, object, result, code FROM audit_events ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })?;
    rows.collect()
}

pub fn host_enroll(conn: &Connection, agent_id: &str) -> rusqlite::Result<Option<HostEnroll>> {
    conn.query_row(
        "SELECT agent_id, hostname, host_fingerprint, lifecycle, decommissioned_at, current_serial,
            grace_override_until, last_seen, online FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| {
            Ok(HostEnroll {
                agent_id: r.get(0)?,
                hostname: r.get(1)?,
                host_fingerprint: r.get(2)?,
                lifecycle: r.get(3)?,
                decommissioned_at: r.get(4)?,
                current_serial: r.get(5)?,
                grace_override_until: r.get(6)?,
                last_seen: r.get(7)?,
                online: r.get::<_, i64>(8)? != 0,
            })
        },
    )
    .optional()
}

/// 簽發時建立或更新清冊：`current_serial`、生命週期回到 `active`；新主機 `source = 'enroll'`、
/// `first_seen`／`last_seen` ＝ 簽發時間、離線（主控台判為從未連線）
#[allow(clippy::too_many_arguments)]
pub fn upsert_host_on_issue(
    conn: &Connection,
    agent_id: &str,
    hostname: &str,
    host_fingerprint: Option<&str>,
    display_name: Option<&str>,
    tags: Option<&str>,
    os: Option<&str>,
    serial: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO hosts (agent_id, hostname, host_fingerprint, display_name, tags, os, online,
            first_seen, last_seen, source, current_serial, lifecycle)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?7, 'enroll', ?8, 'active')
         ON CONFLICT (agent_id) DO UPDATE SET current_serial = excluded.current_serial,
            lifecycle = 'active', decommissioned_at = NULL, decommissioned_by = NULL,
            host_fingerprint = coalesce(hosts.host_fingerprint, excluded.host_fingerprint),
            display_name = coalesce(hosts.display_name, excluded.display_name),
            tags = coalesce(hosts.tags, excluded.tags),
            os = coalesce(hosts.os, excluded.os)",
        params![
            agent_id,
            hostname,
            host_fingerprint,
            display_name,
            tags,
            os,
            now_ms,
            serial
        ],
    )?;
    Ok(())
}

/// 主機群組（角色指派的範圍）：還沒有群組的主機，採用簽發它的註冊 token 的群組
pub fn set_host_group_from_token(
    conn: &Connection,
    agent_id: &str,
    token_id: Option<&str>,
) -> rusqlite::Result<()> {
    if let Some(t) = token_id {
        conn.execute(
            "UPDATE hosts SET host_group = (SELECT group_name FROM enroll_tokens WHERE token_id = ?2)
             WHERE agent_id = ?1 AND host_group IS NULL",
            params![agent_id, t],
        )?;
    }
    Ok(())
}

/// 除役（生命週期 → `decommissioned`）；清冊沒有這台時建立最小的紀錄。回傳是否先前已除役
pub fn decommission_host(
    conn: &Connection,
    agent_id: &str,
    hostname: &str,
    by: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let already = host_enroll(conn, agent_id)?.is_some_and(|h| h.lifecycle == "decommissioned");
    if already {
        return Ok(true);
    }
    conn.execute(
        "INSERT INTO hosts (agent_id, hostname, online, first_seen, last_seen, source, lifecycle,
            decommissioned_at, decommissioned_by)
         VALUES (?1, ?2, 0, ?3, ?3, 'enroll', 'decommissioned', ?3, ?4)
         ON CONFLICT (agent_id) DO UPDATE SET lifecycle = 'decommissioned',
            decommissioned_at = excluded.decommissioned_at, decommissioned_by = excluded.decommissioned_by,
            online = 0",
        params![agent_id, hostname, now_ms, by],
    )?;
    Ok(false)
}

/// 「允許再次自動續簽」（S14，D12）
pub fn set_grace_override(
    conn: &Connection,
    agent_id: &str,
    until_ms: Option<i64>,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE hosts SET grace_override_until = ?2 WHERE agent_id = ?1",
        params![agent_id, until_ms],
    )? > 0)
}

/// 過期續簽的最後一次嘗試（S14「最後嘗試」）
pub fn set_last_grace_attempt(
    conn: &Connection,
    agent_id: &str,
    attempt: &Value,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE hosts SET last_grace_attempt = ?2 WHERE agent_id = ?1",
        params![agent_id, attempt.to_string()],
    )?;
    Ok(())
}
