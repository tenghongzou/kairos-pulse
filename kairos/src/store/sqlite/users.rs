//! 使用者、角色指派與工作階段的 SQLite 實作（[`crate::users::store`] 的 [`UserStore`]）。SQL 原封不動自
//! `users/store.rs` 搬來。

use crate::rbac::{Assignment, Grants, Role, Scope};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use super::conn::{AsConn, forward};
#[allow(unused_imports)]
use crate::users::store::*;

impl<C: AsConn> UserStore for C {
    forward! {
        fn get_user(&self, user_id: i64) -> Option<UserRow> => get_user;
        fn find_user(&self, username: &str) -> Option<UserRow> => find_user;
        fn list_users(&self) -> Vec<UserRow> => list_users;
        fn password_hash(&self, user_id: i64) -> Option<String> => password_hash;
        fn create_user(&self, u: &NewUser<'_>, by: &str, now_ms: i64) -> Option<i64> => create_user;
        fn upsert_ldap_user(&self, username: &str, dn: &str, display_name: Option<&str>, email: Option<&str>, now_ms: i64) -> Option<i64> => upsert_ldap_user;
        fn set_password(&self, user_id: i64, hash: &str, must_change: bool, now_ms: i64) -> bool => set_password;
        fn rehash_password(&self, user_id: i64, old: &str, new: &str) -> bool => rehash_password;
        fn set_profile(&self, user_id: i64, display_name: Option<&str>, email: Option<&str>, now_ms: i64) -> bool => set_profile;
        fn set_state(&self, user_id: i64, active: bool, by: &str, reason: Option<&str>, now_ms: i64) -> bool => set_state;
        fn delete_user(&self, user_id: i64, by: &str, now_ms: i64) -> bool => delete_user;
        fn login_succeeded(&self, user_id: i64, ip: &str, now_ms: i64) -> () => login_succeeded;
        fn login_failed(&self, user_id: i64, threshold: u32, lock_ms: i64, now_ms: i64) -> Option<i64> => login_failed;
        fn unlock(&self, user_id: i64) -> bool => unlock;
        fn totp_state(&self, user_id: i64) -> Option<(Option<Vec<u8>>, Option<Vec<u8>>, Option<i64>)> => totp_state;
        fn set_totp_pending(&self, user_id: i64, sealed: &[u8]) -> () => set_totp_pending;
        fn enable_totp(&self, user_id: i64, step: i64, recovery_hashes: &[Vec<u8>], now_ms: i64) -> bool => enable_totp;
        fn clear_totp(&self, user_id: i64, now_ms: i64) -> bool => clear_totp;
        fn use_totp_step(&self, user_id: i64, step: i64) -> bool => use_totp_step;
        fn use_recovery_code(&self, user_id: i64, hash: &[u8], now_ms: i64) -> bool => use_recovery_code;
        fn user_assignments(&self, user_id: i64) -> Vec<Assignment> => user_assignments;
        fn grants_for(&self, user_id: i64, ldap_groups: &[String]) -> (Grants, Sourced) => grants_for;
        fn add_assignment(&self, subject_type: &str, subject: &str, a: &Assignment, by: &str, now_ms: i64) -> bool => add_assignment;
        fn remove_assignment(&self, subject_type: &str, subject: &str, a: &Assignment) -> bool => remove_assignment;
        fn replace_assignments(&self, subject_type: &str, subject: &str, new: &[Assignment], by: &str, now_ms: i64) -> (Vec<Assignment>, Vec<Assignment>) => replace_assignments;
        fn ldap_mappings(&self) -> Vec<(String, Assignment)> => ldap_mappings;
        fn active_global_admins(&self) -> Vec<i64> => active_global_admins;
        fn role_member_counts(&self) -> Vec<(String, i64)> => role_member_counts;
        fn create_session(&self, s: &NewSession<'_>, now_ms: i64) -> () => create_session;
        fn session_by_hash(&self, hash: &[u8]) -> Option<SessionRow> => session_by_hash;
        fn user_sessions(&self, user_id: i64, active_only: bool, now_ms: i64) -> Vec<SessionRow> => user_sessions;
        fn touch_session(&self, hash: &[u8], now_ms: i64) -> () => touch_session;
        fn end_session(&self, hash: &[u8], reason: &str, by: &str, now_ms: i64) -> bool => end_session;
        fn end_session_by_public_id(&self, user_id: i64, public_id: &str, reason: &str, by: &str, now_ms: i64) -> bool => end_session_by_public_id;
        fn end_user_sessions(&self, user_id: i64, except: Option<&[u8]>, reason: &str, by: &str, now_ms: i64) -> usize => end_user_sessions;
        fn purge_sessions(&self, now_ms: i64) -> usize => purge_sessions;
    }
}

const USER_COLUMNS: &str = "user_id, username, display_name, email, source, ldap_dn, \
    password_hash IS NOT NULL, password_changed_at, must_change_password, totp_enabled_at, state, \
    failed_logins, locked_until, last_login_at, last_login_ip, created_at, created_by, disabled_at, \
    disabled_by, disable_reason";

fn user_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        user_id: r.get(0)?,
        username: r.get(1)?,
        display_name: r.get(2)?,
        email: r.get(3)?,
        source: r.get(4)?,
        ldap_dn: r.get(5)?,
        has_password: r.get(6)?,
        password_changed_at: r.get(7)?,
        must_change_password: r.get(8)?,
        totp_enabled_at: r.get(9)?,
        state: r.get(10)?,
        failed_logins: r.get(11)?,
        locked_until: r.get(12)?,
        last_login_at: r.get(13)?,
        last_login_ip: r.get(14)?,
        created_at: r.get(15)?,
        created_by: r.get(16)?,
        disabled_at: r.get(17)?,
        disabled_by: r.get(18)?,
        disable_reason: r.get(19)?,
    })
}

pub fn get_user(conn: &Connection, user_id: i64) -> rusqlite::Result<Option<UserRow>> {
    conn.query_row(
        &format!("SELECT {USER_COLUMNS} FROM users WHERE user_id = ?1"),
        [user_id],
        user_of,
    )
    .optional()
}

/// 以帳號查詢（不分大小寫）
pub fn find_user(conn: &Connection, username: &str) -> rusqlite::Result<Option<UserRow>> {
    conn.query_row(
        &format!("SELECT {USER_COLUMNS} FROM users WHERE username = ?1"),
        [username],
        user_of,
    )
    .optional()
}

pub fn list_users(conn: &Connection) -> rusqlite::Result<Vec<UserRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {USER_COLUMNS} FROM users ORDER BY username"
    ))?;
    stmt.query_map([], user_of)?.collect()
}

/// 密碼雜湊（登入驗證用）
pub fn password_hash(conn: &Connection, user_id: i64) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT password_hash FROM users WHERE user_id = ?1",
        [user_id],
        |r| r.get(0),
    )
    .optional()
    .map(Option::flatten)
}

/// 建立使用者；帳號已存在時回 `Ok(None)`
pub fn create_user(
    conn: &Connection,
    u: &NewUser<'_>,
    by: &str,
    now_ms: i64,
) -> rusqlite::Result<Option<i64>> {
    if find_user(conn, u.username)?.is_some() {
        return Ok(None);
    }
    conn.execute(
        "INSERT INTO users (username, display_name, email, source, ldap_dn, password_hash,
            password_changed_at, must_change_password, created_at, created_by, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?9)",
        params![
            u.username,
            u.display_name,
            u.email,
            u.source,
            u.ldap_dn,
            u.password_hash,
            u.password_hash.map(|_| now_ms),
            u.must_change_password,
            now_ms,
            by
        ],
    )?;
    Ok(Some(conn.last_insert_rowid()))
}

/// LDAP 使用者第一次登入時建立、之後更新顯示名稱、Email 與 DN；回傳 user_id。
/// 同名的本機帳號存在時回 `Ok(None)`（不讓目錄服務的帳號接管本機帳號）
pub fn upsert_ldap_user(
    conn: &Connection,
    username: &str,
    dn: &str,
    display_name: Option<&str>,
    email: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<Option<i64>> {
    match find_user(conn, username)? {
        Some(u) if u.source != SOURCE_LDAP => Ok(None),
        Some(u) => {
            conn.execute(
                "UPDATE users SET ldap_dn = ?2, display_name = coalesce(?3, display_name),
                    email = coalesce(?4, email), updated_at = ?5 WHERE user_id = ?1",
                params![u.user_id, dn, display_name, email, now_ms],
            )?;
            Ok(Some(u.user_id))
        }
        None => create_user(
            conn,
            &NewUser {
                username,
                display_name,
                email,
                source: SOURCE_LDAP,
                ldap_dn: Some(dn),
                ..NewUser::default()
            },
            "directory",
            now_ms,
        ),
    }
}

pub fn set_password(
    conn: &Connection,
    user_id: i64,
    hash: &str,
    must_change: bool,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE users SET password_hash = ?2, password_changed_at = ?3, must_change_password = ?4,
            failed_logins = 0, locked_until = NULL, updated_at = ?3 WHERE user_id = ?1",
        params![user_id, hash, now_ms, must_change],
    )? > 0)
}

/// 只更新雜湊（參數改變後的重新雜湊；不改變密碼的時間）。密碼在這之間被變更時不取代
pub fn rehash_password(
    conn: &Connection,
    user_id: i64,
    old: &str,
    new: &str,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE users SET password_hash = ?3 WHERE user_id = ?1 AND password_hash = ?2",
        params![user_id, old, new],
    )? > 0)
}

pub fn set_profile(
    conn: &Connection,
    user_id: i64,
    display_name: Option<&str>,
    email: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE users SET display_name = ?2, email = ?3, updated_at = ?4 WHERE user_id = ?1",
        params![user_id, display_name, email, now_ms],
    )? > 0)
}

/// 停用或啟用；回傳是否有變更
pub fn set_state(
    conn: &Connection,
    user_id: i64,
    active: bool,
    by: &str,
    reason: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let n = if active {
        conn.execute(
            "UPDATE users SET state = 'active', disabled_at = NULL, disabled_by = NULL,
                disable_reason = NULL, failed_logins = 0, locked_until = NULL, updated_at = ?2
             WHERE user_id = ?1 AND state != 'active'",
            params![user_id, now_ms],
        )?
    } else {
        conn.execute(
            "UPDATE users SET state = 'disabled', disabled_at = ?2, disabled_by = ?3,
                disable_reason = ?4, updated_at = ?2
             WHERE user_id = ?1 AND state = 'active'",
            params![user_id, now_ms, by, reason],
        )?
    };
    Ok(n > 0)
}

/// 刪除使用者：角色指派、復原碼一併刪除，工作階段結束，擁有的 API token 撤銷
pub fn delete_user(
    conn: &Connection,
    user_id: i64,
    by: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    end_user_sessions(conn, user_id, None, "user_deleted", by, now_ms)?;
    conn.execute(
        "UPDATE api_tokens SET revoked_at = ?2, revoked_by = ?3, revoke_reason = '擁有者已刪除'
         WHERE owner_user_id = ?1 AND revoked_at IS NULL",
        params![user_id, now_ms, by],
    )?;
    conn.execute(
        "DELETE FROM role_assignments WHERE subject_type = 'user' AND subject = ?1",
        [user_id.to_string()],
    )?;
    conn.execute(
        "DELETE FROM user_recovery_codes WHERE user_id = ?1",
        [user_id],
    )?;
    Ok(conn.execute("DELETE FROM users WHERE user_id = ?1", [user_id])? > 0)
}

/// 登入成功：清除失敗次數、記下時間與來源
pub fn login_succeeded(
    conn: &Connection,
    user_id: i64,
    ip: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE users SET failed_logins = 0, locked_until = NULL, last_login_at = ?2, last_login_ip = ?3
         WHERE user_id = ?1",
        params![user_id, now_ms, ip],
    )?;
    Ok(())
}

/// 登入失敗：累計次數，達到門檻時鎖定並歸零；回傳鎖定到何時（這次鎖定時）
pub fn login_failed(
    conn: &Connection,
    user_id: i64,
    threshold: u32,
    lock_ms: i64,
    now_ms: i64,
) -> rusqlite::Result<Option<i64>> {
    let n: i64 = conn.query_row(
        "UPDATE users SET failed_logins = failed_logins + 1 WHERE user_id = ?1 RETURNING failed_logins",
        [user_id],
        |r| r.get(0),
    )?;
    if n >= i64::from(threshold) {
        let until = now_ms + lock_ms;
        conn.execute(
            "UPDATE users SET failed_logins = 0, locked_until = ?2 WHERE user_id = ?1",
            params![user_id, until],
        )?;
        return Ok(Some(until));
    }
    Ok(None)
}

/// 解除鎖定
pub fn unlock(conn: &Connection, user_id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE users SET failed_logins = 0, locked_until = NULL WHERE user_id = ?1 AND (locked_until IS NOT NULL OR failed_logins > 0)",
        [user_id],
    )? > 0)
}

// ---------------------------------------------------------------------------
// TOTP 與復原碼
// ---------------------------------------------------------------------------

/// TOTP 的加密秘密與上次使用的時間步：(啟用中的秘密, 綁定中的秘密, 上次的時間步)
#[allow(clippy::type_complexity)]
pub fn totp_state(
    conn: &Connection,
    user_id: i64,
) -> rusqlite::Result<Option<(Option<Vec<u8>>, Option<Vec<u8>>, Option<i64>)>> {
    conn.query_row(
        "SELECT totp_secret, totp_pending, totp_last_step FROM users WHERE user_id = ?1",
        [user_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

pub fn set_totp_pending(conn: &Connection, user_id: i64, sealed: &[u8]) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE users SET totp_pending = ?2 WHERE user_id = ?1",
        params![user_id, sealed],
    )?;
    Ok(())
}

/// 啟用綁定中的秘密並換上新的復原碼
pub fn enable_totp(
    conn: &Connection,
    user_id: i64,
    step: i64,
    recovery_hashes: &[Vec<u8>],
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE users SET totp_secret = totp_pending, totp_pending = NULL, totp_enabled_at = ?2,
            totp_last_step = ?3, updated_at = ?2
         WHERE user_id = ?1 AND totp_pending IS NOT NULL",
        params![user_id, now_ms, step],
    )?;
    if n == 0 {
        return Ok(false);
    }
    conn.execute(
        "DELETE FROM user_recovery_codes WHERE user_id = ?1",
        [user_id],
    )?;
    for h in recovery_hashes {
        conn.execute(
            "INSERT INTO user_recovery_codes (user_id, code_hash) VALUES (?1, ?2)",
            params![user_id, h],
        )?;
    }
    Ok(true)
}

/// 停用或重設 TOTP（秘密、綁定中的秘密與復原碼一併刪除）；回傳先前是否啟用
pub fn clear_totp(conn: &Connection, user_id: i64, now_ms: i64) -> rusqlite::Result<bool> {
    let was: bool = conn
        .query_row(
            "SELECT totp_enabled_at IS NOT NULL FROM users WHERE user_id = ?1",
            [user_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    conn.execute(
        "UPDATE users SET totp_secret = NULL, totp_pending = NULL, totp_enabled_at = NULL,
            totp_last_step = NULL, updated_at = ?2 WHERE user_id = ?1",
        params![user_id, now_ms],
    )?;
    conn.execute(
        "DELETE FROM user_recovery_codes WHERE user_id = ?1",
        [user_id],
    )?;
    Ok(was)
}

/// 記下使用過的時間步（只在比上次新時成功；同一個碼不能用兩次）
pub fn use_totp_step(conn: &Connection, user_id: i64, step: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE users SET totp_last_step = ?2 WHERE user_id = ?1
            AND (totp_last_step IS NULL OR totp_last_step < ?2)",
        params![user_id, step],
    )? > 0)
}

/// 使用一組復原碼（只能用一次）
pub fn use_recovery_code(
    conn: &Connection,
    user_id: i64,
    hash: &[u8],
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE user_recovery_codes SET used_at = ?3
         WHERE user_id = ?1 AND code_hash = ?2 AND used_at IS NULL",
        params![user_id, hash, now_ms],
    )? > 0)
}

#[cfg(test)]
pub fn recovery_codes_left(conn: &Connection, user_id: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM user_recovery_codes WHERE user_id = ?1 AND used_at IS NULL",
        [user_id],
        |r| r.get(0),
    )
}

fn assignment_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<Assignment>> {
    let role: String = r.get(0)?;
    let kind: String = r.get(1)?;
    let value: String = r.get(2)?;
    Ok(Role::parse(&role)
        .zip(Scope::from_db(&kind, &value))
        .map(|(role, scope)| Assignment { role, scope }))
}

/// 主體的角色指派（不認得的角色或範圍略過：不會因為資料庫中有未來版本的值而多出權限）
pub fn assignments(
    conn: &Connection,
    subject_type: &str,
    subject: &str,
) -> rusqlite::Result<Vec<Assignment>> {
    let mut stmt = conn.prepare(
        "SELECT role, scope_type, scope_value FROM role_assignments
         WHERE subject_type = ?1 AND subject = ?2 ORDER BY role, scope_type, scope_value",
    )?;
    let rows = stmt.query_map(params![subject_type, subject], assignment_of)?;
    Ok(rows.filter_map(|r| r.ok().flatten()).collect())
}

pub fn user_assignments(conn: &Connection, user_id: i64) -> rusqlite::Result<Vec<Assignment>> {
    assignments(conn, SUBJECT_USER, &user_id.to_string())
}

/// 使用者的權限：個人指派＋所屬目錄服務群組的對應
pub fn grants_for(
    conn: &Connection,
    user_id: i64,
    ldap_groups: &[String],
) -> rusqlite::Result<(Grants, Sourced)> {
    let mut all: Vec<(Assignment, Option<String>)> = user_assignments(conn, user_id)?
        .into_iter()
        .map(|a| (a, None))
        .collect();
    for g in ldap_groups {
        let key = normalize_dn(g);
        for a in assignments(conn, SUBJECT_LDAP_GROUP, &key)? {
            all.push((a, Some(g.clone())));
        }
    }
    let grants = Grants::from_assignments(all.iter().map(|(a, _)| a));
    Ok((grants, all))
}

/// 新增一筆指派；已存在時回 `false`
pub fn add_assignment(
    conn: &Connection,
    subject_type: &str,
    subject: &str,
    a: &Assignment,
    by: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let (kind, value) = a.scope.to_db();
    Ok(conn.execute(
        "INSERT OR IGNORE INTO role_assignments (subject_type, subject, role, scope_type, scope_value, created_at, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![subject_type, subject, a.role.as_str(), kind, value, now_ms, by],
    )? > 0)
}

pub fn remove_assignment(
    conn: &Connection,
    subject_type: &str,
    subject: &str,
    a: &Assignment,
) -> rusqlite::Result<bool> {
    let (kind, value) = a.scope.to_db();
    Ok(conn.execute(
        "DELETE FROM role_assignments WHERE subject_type = ?1 AND subject = ?2 AND role = ?3
            AND scope_type = ?4 AND scope_value = ?5",
        params![subject_type, subject, a.role.as_str(), kind, value],
    )? > 0)
}

/// 以新的一組取代主體的所有指派；回傳 (新增, 移除)
pub fn replace_assignments(
    conn: &Connection,
    subject_type: &str,
    subject: &str,
    new: &[Assignment],
    by: &str,
    now_ms: i64,
) -> rusqlite::Result<(Vec<Assignment>, Vec<Assignment>)> {
    let old = assignments(conn, subject_type, subject)?;
    let added: Vec<Assignment> = new.iter().filter(|a| !old.contains(a)).cloned().collect();
    let removed: Vec<Assignment> = old.iter().filter(|a| !new.contains(a)).cloned().collect();
    for a in &removed {
        remove_assignment(conn, subject_type, subject, a)?;
    }
    for a in &added {
        add_assignment(conn, subject_type, subject, a, by, now_ms)?;
    }
    Ok((added, removed))
}

/// 目錄服務群組的對應：(群組 DN, 指派)
pub fn ldap_mappings(conn: &Connection) -> rusqlite::Result<Vec<(String, Assignment)>> {
    let mut stmt = conn.prepare(
        "SELECT subject, role, scope_type, scope_value FROM role_assignments
         WHERE subject_type = 'ldap_group' ORDER BY subject, role",
    )?;
    let rows = stmt.query_map([], |r| {
        let subject: String = r.get(0)?;
        let role: String = r.get(1)?;
        let kind: String = r.get(2)?;
        let value: String = r.get(3)?;
        Ok((subject, role, kind, value))
    })?;
    Ok(rows
        .filter_map(|r| r.ok())
        .filter_map(|(s, role, kind, value)| {
            Some((
                s,
                Assignment {
                    role: Role::parse(&role)?,
                    scope: Scope::from_db(&kind, &value)?,
                },
            ))
        })
        .collect())
}

/// 啟用中、有全站管理員個人指派的使用者數（「不可移除最後一位管理員」）
pub fn active_global_admins(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT u.user_id FROM users u JOIN role_assignments a
            ON a.subject_type = 'user' AND a.subject = CAST(u.user_id AS TEXT)
         WHERE u.state = 'active' AND a.role = 'admin' AND a.scope_type = 'global'
         ORDER BY u.user_id",
    )?;
    stmt.query_map([], |r| r.get(0))?.collect()
}

/// 各角色目前的成員數（個人指派，不含群組對應；S16「角色與權限」分頁）
pub fn role_member_counts(conn: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT a.role, count(DISTINCT a.subject) FROM role_assignments a
            JOIN users u ON a.subject_type = 'user' AND a.subject = CAST(u.user_id AS TEXT)
         WHERE u.state = 'active' GROUP BY a.role",
    )?;
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

pub fn create_session(conn: &Connection, s: &NewSession<'_>, now_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sessions (session_hash, public_id, user_id, method, ldap_groups, created_at,
            last_seen_at, expires_at, idle_timeout_ms, ip, user_agent)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10)",
        params![
            s.session_hash,
            s.public_id,
            s.user_id,
            s.method,
            (!s.ldap_groups.is_empty()).then(|| json!(s.ldap_groups).to_string()),
            now_ms,
            s.expires_at,
            s.idle_timeout_ms,
            s.ip,
            s.user_agent
        ],
    )?;
    Ok(())
}

const SESSION_COLUMNS: &str = "public_id, user_id, method, ldap_groups, created_at, last_seen_at, \
    expires_at, idle_timeout_ms, ip, user_agent, ended_at, end_reason";

fn session_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let groups: Option<String> = r.get(3)?;
    Ok(SessionRow {
        public_id: r.get(0)?,
        user_id: r.get(1)?,
        method: r.get(2)?,
        ldap_groups: groups
            .and_then(|g| serde_json::from_str(&g).ok())
            .unwrap_or_default(),
        created_at: r.get(4)?,
        last_seen_at: r.get(5)?,
        expires_at: r.get(6)?,
        idle_timeout_ms: r.get(7)?,
        ip: r.get(8)?,
        user_agent: r.get(9)?,
        ended_at: r.get(10)?,
        end_reason: r.get(11)?,
    })
}

/// 以 cookie 秘密的雜湊查詢
pub fn session_by_hash(conn: &Connection, hash: &[u8]) -> rusqlite::Result<Option<SessionRow>> {
    conn.query_row(
        &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE session_hash = ?1"),
        [hash],
        session_of,
    )
    .optional()
}

/// 使用者的工作階段（`active_only` 時只列有效的；新到舊）
pub fn user_sessions(
    conn: &Connection,
    user_id: i64,
    active_only: bool,
    now_ms: i64,
) -> rusqlite::Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SESSION_COLUMNS} FROM sessions WHERE user_id = ?1 ORDER BY created_at DESC LIMIT 200"
    ))?;
    let rows: Vec<SessionRow> = stmt
        .query_map([user_id], session_of)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .filter(|s| !active_only || s.valid(now_ms))
        .collect())
}

/// 更新最後活動時間（只往後）
pub fn touch_session(conn: &Connection, hash: &[u8], now_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE sessions SET last_seen_at = ?2 WHERE session_hash = ?1 AND last_seen_at < ?2 AND ended_at IS NULL",
        params![hash, now_ms],
    )?;
    Ok(())
}

/// 結束一個工作階段；回傳是否有變更
pub fn end_session(
    conn: &Connection,
    hash: &[u8],
    reason: &str,
    by: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE sessions SET ended_at = ?2, end_reason = ?3, ended_by = ?4
         WHERE session_hash = ?1 AND ended_at IS NULL",
        params![hash, now_ms, reason, by],
    )? > 0)
}

/// 以公開的 id 結束使用者的一個工作階段
pub fn end_session_by_public_id(
    conn: &Connection,
    user_id: i64,
    public_id: &str,
    reason: &str,
    by: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE sessions SET ended_at = ?3, end_reason = ?4, ended_by = ?5
         WHERE user_id = ?1 AND public_id = ?2 AND ended_at IS NULL",
        params![user_id, public_id, now_ms, reason, by],
    )? > 0)
}

/// 結束使用者的所有工作階段（`except` 為保留的那一個）；回傳結束的數量。
/// 已逾時但未標記結束的也一併標記（不計入回傳的數量）
pub fn end_user_sessions(
    conn: &Connection,
    user_id: i64,
    except: Option<&[u8]>,
    reason: &str,
    by: &str,
    now_ms: i64,
) -> rusqlite::Result<usize> {
    let active: usize = user_sessions(conn, user_id, true, now_ms)?.len()
        - usize::from(except.is_some_and(|h| {
            session_by_hash(conn, h)
                .ok()
                .flatten()
                .is_some_and(|s| s.user_id == user_id && s.valid(now_ms))
        }));
    conn.execute(
        "UPDATE sessions SET ended_at = ?2, end_reason = ?3, ended_by = ?4
         WHERE user_id = ?1 AND ended_at IS NULL AND (?5 IS NULL OR session_hash != ?5)",
        params![user_id, now_ms, reason, by, except],
    )?;
    Ok(active)
}

/// 清除結束或逾時超過 30 天的工作階段紀錄
pub fn purge_sessions(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    let before = now_ms - 30 * 86_400_000;
    conn.execute(
        "DELETE FROM sessions WHERE coalesce(ended_at, min(expires_at, last_seen_at + idle_timeout_ms)) < ?1",
        [before],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sqlite::{MIGRATIONS, migrate};

    pub(crate) fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, MIGRATIONS).unwrap();
        conn
    }

    fn user(conn: &Connection, name: &str) -> i64 {
        create_user(
            conn,
            &NewUser {
                username: name,
                source: SOURCE_LOCAL,
                password_hash: Some("$argon2id$x"),
                ..NewUser::default()
            },
            "test",
            1_000,
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn usernames_and_users() {
        assert!(validate_username("admin").is_ok());
        assert!(validate_username("chen.y@city").is_ok());
        for bad in ["", "-x", "a b", "名字", "a;b", &"x".repeat(65)] {
            assert!(validate_username(bad).is_err(), "{bad:?}");
        }
        let conn = db();
        let id = user(&conn, "Admin");
        // 不分大小寫：同名不能再建立
        assert_eq!(
            create_user(
                &conn,
                &NewUser {
                    username: "admin",
                    source: SOURCE_LOCAL,
                    ..NewUser::default()
                },
                "t",
                1
            )
            .unwrap(),
            None
        );
        let u = find_user(&conn, "ADMIN").unwrap().unwrap();
        assert_eq!(u.user_id, id);
        assert!(u.has_password);
        assert_eq!(u.status(0), "active");
        // JSON 不含密碼雜湊
        assert!(!u.json(0).to_string().contains("argon2"));
        // 鎖定
        assert_eq!(login_failed(&conn, id, 3, 60_000, 10).unwrap(), None);
        assert_eq!(login_failed(&conn, id, 3, 60_000, 10).unwrap(), None);
        assert_eq!(
            login_failed(&conn, id, 3, 60_000, 10).unwrap(),
            Some(60_010)
        );
        let u = get_user(&conn, id).unwrap().unwrap();
        assert!(u.locked(100));
        assert_eq!(u.status(100), "locked");
        assert!(!u.locked(60_010));
        assert!(unlock(&conn, id).unwrap());
        // 停用、啟用
        assert!(set_state(&conn, id, false, "ops", Some("離職"), 5).unwrap());
        assert!(!set_state(&conn, id, false, "ops", None, 6).unwrap());
        assert_eq!(get_user(&conn, id).unwrap().unwrap().status(0), "disabled");
        assert!(set_state(&conn, id, true, "ops", None, 7).unwrap());
        // 同名的本機帳號不會被目錄服務接管
        assert_eq!(
            upsert_ldap_user(&conn, "admin", "CN=admin", None, None, 1).unwrap(),
            None
        );
        let l = upsert_ldap_user(&conn, "chen.y", "CN=Chen", Some("陳小姐"), None, 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            upsert_ldap_user(&conn, "CHEN.Y", "CN=Chen2", None, None, 2).unwrap(),
            Some(l)
        );
        let u = get_user(&conn, l).unwrap().unwrap();
        assert_eq!(u.ldap_dn.as_deref(), Some("CN=Chen2"));
        assert_eq!(u.display_name.as_deref(), Some("陳小姐"));
        assert!(!u.has_password);
    }

    #[test]
    fn assignments_grants_and_last_admin() {
        let conn = db();
        let a = user(&conn, "a");
        let b = user(&conn, "b");
        let admin = Assignment::parse("admin").unwrap();
        assert!(add_assignment(&conn, SUBJECT_USER, &a.to_string(), &admin, "t", 1).unwrap());
        assert!(!add_assignment(&conn, SUBJECT_USER, &a.to_string(), &admin, "t", 1).unwrap());
        assert_eq!(active_global_admins(&conn).unwrap(), vec![a]);
        let (added, removed) = replace_assignments(
            &conn,
            SUBJECT_USER,
            &b.to_string(),
            &[Assignment::parse("operator@group:Line3").unwrap()],
            "t",
            1,
        )
        .unwrap();
        assert_eq!((added.len(), removed.len()), (1, 0));
        // 群組對應
        add_assignment(
            &conn,
            SUBJECT_LDAP_GROUP,
            &normalize_dn("CN=IT-Ops, OU=Groups,DC=city"),
            &Assignment::parse("auditor").unwrap(),
            "t",
            1,
        )
        .unwrap();
        let (g, list) =
            grants_for(&conn, b, &["cn=it-ops,ou=Groups, dc=city".to_string()]).unwrap();
        assert!(g.has(crate::rbac::Permission::AuditRead));
        assert_eq!(list.len(), 2);
        assert_eq!(ldap_mappings(&conn).unwrap().len(), 1);
        // 未知的角色值被忽略（不會因為資料庫中的未知值而多出權限）
        conn.execute(
            "INSERT INTO role_assignments (subject_type, subject, role, created_at, created_by) VALUES ('user', ?1, 'superuser', 1, 't')",
            [b.to_string()],
        )
        .unwrap();
        assert_eq!(user_assignments(&conn, b).unwrap().len(), 1);
        let counts = role_member_counts(&conn).unwrap();
        assert!(counts.contains(&("admin".to_string(), 1)));
        // 停用後不算管理員
        set_state(&conn, a, false, "t", None, 2).unwrap();
        assert!(active_global_admins(&conn).unwrap().is_empty());
        // 刪除：指派、工作階段、擁有的 token 一併處理
        assert!(delete_user(&conn, a, "t", 3).unwrap());
        assert!(user_assignments(&conn, a).unwrap().is_empty());
        assert!(get_user(&conn, a).unwrap().is_none());
    }

    #[test]
    fn sessions_expire_and_end() {
        let conn = db();
        let id = user(&conn, "a");
        let mk = |hash: &[u8], public: &str, now: i64| {
            create_session(
                &conn,
                &NewSession {
                    session_hash: hash,
                    public_id: public,
                    user_id: id,
                    method: "local",
                    ldap_groups: &[],
                    idle_timeout_ms: 1_000,
                    expires_at: now + 10_000,
                    ip: "10.0.0.1",
                    user_agent: Some("test"),
                },
                now,
            )
            .unwrap();
        };
        mk(b"h1", "p1", 0);
        mk(b"h2", "p2", 0);
        mk(b"h3", "p3", 0);
        let s = session_by_hash(&conn, b"h1").unwrap().unwrap();
        assert!(s.valid(999));
        // 閒置逾時
        assert!(!s.valid(1_000));
        touch_session(&conn, b"h1", 900).unwrap();
        assert!(session_by_hash(&conn, b"h1").unwrap().unwrap().valid(1_800));
        // 絕對逾時
        touch_session(&conn, b"h1", 9_999).unwrap();
        assert!(
            !session_by_hash(&conn, b"h1")
                .unwrap()
                .unwrap()
                .valid(10_000)
        );
        // 結束其他所有工作階段（保留目前這個）
        assert_eq!(
            end_user_sessions(&conn, id, Some(b"h1"), "user_revoked_others", "user:a", 500)
                .unwrap(),
            2
        );
        assert!(session_by_hash(&conn, b"h1").unwrap().unwrap().valid(600));
        assert!(!session_by_hash(&conn, b"h2").unwrap().unwrap().valid(600));
        // 結束全部
        mk(b"h4", "p4", 600);
        assert_eq!(
            end_user_sessions(&conn, id, None, "admin_revoked", "user:root", 700).unwrap(),
            2
        );
        assert!(user_sessions(&conn, id, true, 700).unwrap().is_empty());
        assert_eq!(user_sessions(&conn, id, false, 700).unwrap().len(), 4);
        // 以公開的 id 結束
        mk(b"h5", "p5", 800);
        assert!(end_session_by_public_id(&conn, id, "p5", "x", "y", 801).unwrap());
        assert!(!end_session_by_public_id(&conn, id, "p5", "x", "y", 802).unwrap());
        assert_eq!(purge_sessions(&conn, 31 * 86_400_000).unwrap(), 5);
    }

    #[test]
    fn totp_steps_and_recovery_codes_are_single_use() {
        let conn = db();
        let id = user(&conn, "a");
        set_totp_pending(&conn, id, b"sealed").unwrap();
        assert!(enable_totp(&conn, id, 100, &[b"r1".to_vec(), b"r2".to_vec()], 5).unwrap());
        assert!(!enable_totp(&conn, id, 100, &[], 5).unwrap());
        let (secret, pending, last) = totp_state(&conn, id).unwrap().unwrap();
        assert_eq!(secret.as_deref(), Some(&b"sealed"[..]));
        assert_eq!((pending, last), (None, Some(100)));
        assert!(!use_totp_step(&conn, id, 100).unwrap());
        assert!(use_totp_step(&conn, id, 101).unwrap());
        assert!(use_recovery_code(&conn, id, b"r1", 6).unwrap());
        assert!(!use_recovery_code(&conn, id, b"r1", 7).unwrap());
        assert_eq!(recovery_codes_left(&conn, id).unwrap(), 1);
        assert!(clear_totp(&conn, id, 8).unwrap());
        assert!(!clear_totp(&conn, id, 9).unwrap());
        assert_eq!(recovery_codes_left(&conn, id).unwrap(), 0);
    }
}
