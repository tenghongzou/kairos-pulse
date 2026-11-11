//! [`crate::api::tokens`] 的 PostgreSQL 實作（[`TokenStore`]）。逐函式對照 `store/sqlite/tokens.rs`。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

#[allow(unused_imports)]
use super::conn::{OptionalExtension, PgConn, PgResult, Row, params, pg_domain};
use crate::api::tokens::{StoredToken, TokenRow, TokenStore};
use crate::store::db::TxMode;

pg_domain! { TokenStore {
    fn insert_api_token(&self, token_id: &str, name: &str, secret_hash: &[u8], scope: &str, now_ms: i64, created_by: Option<&str>, expires_at: Option<i64>, role: Option<&str>, owner_user_id: Option<i64>) -> () => insert_api_token;
    fn list_api_tokens(&self) -> Vec<TokenRow> => list_api_tokens;
    fn get_api_token(&self, token_id: &str) -> Option<TokenRow> => get_api_token;
    fn revoke_api_token(&self, token_id: &str, now_ms: i64, by: Option<&str>, reason: Option<&str>) -> usize => revoke_api_token;
    fn lookup_api_token(&self, token_id: &str) -> Option<StoredToken> => lookup_api_token;
    fn api_tokens_pending_audit(&self) -> Vec<(TokenRow, i64)> => api_tokens_pending_audit;
    fn mark_api_token_logged(&self, token_id: &str, flags: i64) -> () => mark_api_token_logged;
    fn store_api_token_last_used(&self, entries: &[(String, i64, String)]) -> () => store_api_token_last_used;
} }

const COLUMNS: &str = "token_id, name, scope, created_at, created_by, expires_at, revoked_at, \
                       revoked_by, revoke_reason, last_used_at, last_used_ip, role, owner_user_id, \
                       (SELECT username FROM users WHERE users.user_id = api_tokens.owner_user_id)";

fn row_of(r: &Row) -> PgResult<TokenRow> {
    Ok(TokenRow {
        token_id: r.get(0)?,
        name: r.get(1)?,
        scope: r.get(2)?,
        role: r.get(11)?,
        owner_user_id: r.get(12)?,
        owner_username: r.get(13)?,
        created_at: r.get(3)?,
        created_by: r.get(4)?,
        expires_at: r.get(5)?,
        revoked_at: r.get(6)?,
        revoked_by: r.get(7)?,
        revoke_reason: r.get(8)?,
        last_used_at: r.get(9)?,
        last_used_ip: r.get(10)?,
    })
}

/// 新增一個 token（原本在 `api/tokens.rs` 的 `create_with`）
#[allow(clippy::too_many_arguments)]
pub fn insert_api_token(
    conn: &PgConn,
    token_id: &str,
    name: &str,
    secret_hash: &[u8],
    scope: &str,
    now_ms: i64,
    created_by: Option<&str>,
    expires_at: Option<i64>,
    role: Option<&str>,
    owner: Option<i64>,
) -> PgResult<()> {
    conn.execute(
        "INSERT INTO api_tokens (token_id, name, secret_hash, scope, created_at, created_by, expires_at,
            role, owner_user_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![token_id, name, secret_hash, scope, now_ms, created_by, expires_at, role, owner],
    )?;
    Ok(())
}

/// 所有 token（依建立時間）
pub fn list_api_tokens(conn: &PgConn) -> PgResult<Vec<TokenRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM api_tokens ORDER BY created_at, token_id"
    ))?;
    let rows = stmt.query_map((), row_of)?;
    rows.collect()
}

pub fn get_api_token(conn: &PgConn, token_id: &str) -> PgResult<Option<TokenRow>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM api_tokens WHERE token_id = ?1"),
        [token_id],
        row_of,
    )
    .optional()
}

/// 撤銷（原本在 `api/tokens.rs` 的 `revoke_with`）
pub fn revoke_api_token(
    conn: &PgConn,
    token_id: &str,
    now_ms: i64,
    by: Option<&str>,
    reason: Option<&str>,
) -> PgResult<usize> {
    conn.execute(
        "UPDATE api_tokens SET revoked_at = ?2, revoked_by = ?3, revoke_reason = ?4
         WHERE token_id = ?1 AND revoked_at IS NULL",
        params![token_id, now_ms, by, reason],
    )
}

pub fn lookup_api_token(conn: &PgConn, token_id: &str) -> PgResult<Option<StoredToken>> {
    conn.query_row(
        "SELECT secret_hash, name, scope, expires_at, revoked_at, role, owner_user_id
         FROM api_tokens WHERE token_id = ?1",
        [token_id],
        |r| {
            Ok(StoredToken {
                secret_hash: r.get(0)?,
                name: r.get(1)?,
                scope: r.get(2)?,
                expires_at: r.get(3)?,
                revoked_at: r.get(4)?,
                role: r.get(5)?,
                owner_user_id: r.get(6)?,
            })
        },
    )
    .optional()
}

pub fn api_tokens_pending_audit(conn: &PgConn) -> PgResult<Vec<(TokenRow, i64)>> {
    // 與建立時間相同的 token 之間以 token_id 排出固定的順序（SQLite 版依主鍵的儲存順序，結果相同）
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS}, logged FROM api_tokens
         WHERE (logged & 1) = 0 OR (revoked_at IS NOT NULL AND (logged & 2) = 0)
         ORDER BY created_at, token_id LIMIT 100"
    ))?;
    let rows = stmt.query_map((), |r| Ok((row_of(r)?, r.get::<_, i64>(14)?)))?;
    rows.collect()
}

pub fn mark_api_token_logged(conn: &PgConn, token_id: &str, flags: i64) -> PgResult<()> {
    conn.execute(
        "UPDATE api_tokens SET logged = logged | ?2::bigint WHERE token_id = ?1",
        params![token_id, flags],
    )
    .map(|_| ())
}

/// 寫回最後使用的時間與來源（原本在 `api/tokens.rs` 的 `store_last_used`）。
/// 條件式 UPDATE（只往後）：多個節點同時寫回時，較新的時間留下
pub fn store_api_token_last_used(conn: &PgConn, entries: &[(String, i64, String)]) -> PgResult<()> {
    let tx = conn.begin_tx(TxMode::Deferred)?;
    {
        let mut stmt = tx.prepare(
            "UPDATE api_tokens SET last_used_at = ?2, last_used_ip = ?3
             WHERE token_id = ?1 AND (last_used_at IS NULL OR last_used_at < ?2)",
        )?;
        for (id, at, ip) in entries {
            stmt.execute(params![id, at, ip])?;
        }
    }
    tx.commit()
}
