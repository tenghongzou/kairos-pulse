//! 註冊相關資料表的讀寫（schema v4，`kairos/PKI.md` 第 7 節）：`enroll_tokens`、`enroll_requests`、
//! `enroll_replay`、`enroll_blocks`、`enroll_relaxations`、`settings`、`audit_events`，以及 `hosts` 中與
//! 註冊有關的欄位。
//!
//! 所有函式都是同步的；常駐行程在 blocking 執行緒上呼叫，CLI 直接呼叫。需要原子性的多步操作由呼叫端包在
//! 交易中（`rusqlite::Transaction` 會 deref 成 `Connection`）。

use kairos_proto::enroll::{ApprovalDetail, RequestKind, RequestState, TokenApproval};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

/// `enroll_requests.state`（Kairos 內部比協定多一個 `approved`：已核准、尚未簽發）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbState {
    PendingApproval,
    Approved,
    Issued,
    Rejected,
    RequestExpired,
    Withdrawn,
    Replaced,
}

impl DbState {
    pub const ALL: [DbState; 7] = [
        DbState::PendingApproval,
        DbState::Approved,
        DbState::Issued,
        DbState::Rejected,
        DbState::RequestExpired,
        DbState::Withdrawn,
        DbState::Replaced,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DbState::PendingApproval => "pending_approval",
            DbState::Approved => "approved",
            DbState::Issued => "issued",
            DbState::Rejected => "rejected",
            DbState::RequestExpired => "request_expired",
            DbState::Withdrawn => "withdrawn",
            DbState::Replaced => "replaced",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.as_str() == s)
    }

    /// 協定上的狀態與 `approval_detail`
    pub fn protocol(self) -> (RequestState, Option<ApprovalDetail>) {
        match self {
            DbState::PendingApproval => (
                RequestState::PendingApproval,
                Some(ApprovalDetail::AwaitingApproval),
            ),
            DbState::Approved => (
                RequestState::PendingApproval,
                Some(ApprovalDetail::ApprovedAwaitingIssuance),
            ),
            DbState::Issued => (RequestState::Issued, None),
            DbState::Rejected => (RequestState::Rejected, None),
            DbState::RequestExpired => (RequestState::RequestExpired, None),
            DbState::Withdrawn => (RequestState::Withdrawn, None),
            DbState::Replaced => (RequestState::Replaced, None),
        }
    }

    /// 仍在待核准佇列中（L9 計數、可撤回、可核准）
    pub fn is_pending(self) -> bool {
        matches!(self, DbState::PendingApproval | DbState::Approved)
    }
}

/// `enroll_requests.kind`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbKind {
    Enroll,
    GraceRenewal,
    /// 8443 的正常輪替（政策要求人工核准時才建立申請）
    Renewal,
}

impl DbKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DbKind::Enroll => "enroll",
            DbKind::GraceRenewal => "grace_renewal",
            DbKind::Renewal => "renewal",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [DbKind::Enroll, DbKind::GraceRenewal, DbKind::Renewal]
            .into_iter()
            .find(|k| k.as_str() == s)
    }

    /// 協定上的種類（8443 輪替不會出現在 8444 的回應中，以 `grace_renewal` 表示）
    pub fn protocol(self) -> RequestKind {
        match self {
            DbKind::Enroll => RequestKind::Enroll,
            DbKind::GraceRenewal | DbKind::Renewal => RequestKind::GraceRenewal,
        }
    }
}

fn json_list(v: Option<String>) -> Option<Vec<String>> {
    v.and_then(|s| serde_json::from_str(&s).ok())
}

fn list_json(v: &Option<Vec<String>>) -> Option<String> {
    v.as_ref().map(|l| Value::from(l.clone()).to_string())
}

// ---------------------------------------------------------------------------
// 註冊 token
// ---------------------------------------------------------------------------

/// `enroll_tokens` 的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRow {
    pub token_id: String,
    pub secret_hash: Vec<u8>,
    pub hint: String,
    pub name: String,
    pub group_name: String,
    pub tags: Option<Vec<String>>,
    pub approval: TokenApproval,
    pub max_uses: i64,
    pub used: i64,
    pub expires_at: i64,
    pub allowed_cidrs: Option<Vec<String>>,
    pub batch_mode: bool,
    pub allowed_agent_ids: Option<Vec<String>>,
    pub cert_lifetime_days: Option<i64>,
    pub created_at: i64,
    pub created_by: String,
    pub disabled_at: Option<i64>,
    pub disabled_by: Option<String>,
    pub disable_reason: Option<String>,
    pub last_used_at: Option<i64>,
}

/// token 的狀態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenState {
    Active,
    Expired,
    Exhausted,
    Disabled,
}

impl TokenState {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenState::Active => "active",
            TokenState::Expired => "expired",
            TokenState::Exhausted => "exhausted",
            TokenState::Disabled => "disabled",
        }
    }
}

impl TokenRow {
    pub fn state(&self, now_ms: i64) -> TokenState {
        if self.disabled_at.is_some() {
            TokenState::Disabled
        } else if self.expires_at <= now_ms {
            TokenState::Expired
        } else if self.used >= self.max_uses {
            TokenState::Exhausted
        } else {
            TokenState::Active
        }
    }

    pub fn remaining_uses(&self) -> u32 {
        u32::try_from((self.max_uses - self.used).max(0)).unwrap_or(u32::MAX)
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

pub fn approval_str(a: TokenApproval) -> &'static str {
    match a {
        TokenApproval::Manual => "manual",
        TokenApproval::Auto => "auto",
    }
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

// ---------------------------------------------------------------------------
// 申請
// ---------------------------------------------------------------------------

/// `enroll_requests` 的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRow {
    pub request_id: String,
    pub kind: DbKind,
    pub agent_id: String,
    pub request_code: String,
    pub state: DbState,
    pub csr_der: Vec<u8>,
    pub csr_sha256: Vec<u8>,
    pub spki_sha256: Vec<u8>,
    pub poll_secret_hash: Option<Vec<u8>>,
    pub token_id: Option<String>,
    pub expired_serial: Option<String>,
    pub previous_serial: Option<String>,
    pub source_ip: String,
    pub hostname: String,
    pub display_name: Option<String>,
    pub tags: Option<Vec<String>>,
    pub os: Option<String>,
    pub host_fingerprint: Option<String>,
    pub pulse_version: Option<String>,
    pub server_verification: Option<String>,
    pub flags: Vec<String>,
    pub cert_lifetime_days: Option<i64>,
    pub created_at: i64,
    pub expires_at: i64,
    pub decided_at: Option<i64>,
    pub decided_by: Option<String>,
    pub reject_reason_code: Option<String>,
    pub reject_reason: Option<String>,
    pub issued_serial: Option<String>,
    pub issue_error: Option<String>,
    pub delivered_at: Option<i64>,
    pub withdrawn_at: Option<i64>,
    pub withdraw_reason: Option<String>,
    pub withdraw_initiated_by: Option<String>,
    pub withdraw_ip: Option<String>,
    pub closed_at: Option<i64>,
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

/// 列表的篩選
#[derive(Debug, Clone, Default)]
pub struct RequestFilter {
    /// `true`：最近結束；`false`：待核准（含已核准待簽發）
    pub closed: bool,
    pub token_id: Option<String>,
    pub kind: Option<DbKind>,
    pub state: Option<DbState>,
    pub limit: usize,
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

/// L7 封鎖的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRow {
    pub source: String,
    pub class: String,
    pub blocked_until: i64,
    pub offenses: i64,
    pub last_offense_at: i64,
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

/// 稽核事件（`audit_events`）
#[derive(Debug, Clone, Default)]
pub struct Audit<'a> {
    pub actor: &'a str,
    pub actor_ip: Option<&'a str>,
    pub category: &'a str,
    pub action: &'a str,
    pub object: Option<&'a str>,
    pub result: &'a str,
    pub code: Option<&'a str>,
    pub details: Option<Value>,
}

pub fn audit(conn: &Connection, at_ms: i64, a: &Audit<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO audit_events (at, actor, actor_ip, category, action, object, result, code, details)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            at_ms,
            a.actor,
            a.actor_ip,
            a.category,
            a.action,
            a.object,
            a.result,
            a.code,
            a.details.as_ref().map(Value::to_string)
        ],
    )?;
    Ok(())
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

pub fn purge_audit(conn: &Connection, before_ms: i64) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM audit_events WHERE at < ?1", [before_ms])
}

// ---------------------------------------------------------------------------
// hosts（清冊）中與註冊有關的欄位
// ---------------------------------------------------------------------------

/// 清冊中與註冊、續簽、除役有關的欄位
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEnroll {
    pub agent_id: String,
    pub hostname: String,
    pub host_fingerprint: Option<String>,
    pub lifecycle: String,
    pub decommissioned_at: Option<i64>,
    pub current_serial: Option<String>,
    pub grace_override_until: Option<i64>,
    pub last_seen: i64,
    pub online: bool,
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
