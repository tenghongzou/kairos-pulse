//! API token 的保存（`api_tokens`，schema v3；v6 加上擁有者與角色）：`kairos api-token` 與主控台 API 共用
//! （`kairos/API.md` 第 4 節、第 12.5 節）。
//!
//! token 的權限有三種來源（[`TokenGrant`]）：
//! - **角色**（0.6 起建立的預設）：`role` 為內建角色，全站範圍；
//! - **使用者**：`owner_user_id` 為擁有者，權限＝擁有者目前的權限（∩ `role`，有指定時）；擁有者停用或刪除時
//!   token 立即失效；
//! - **舊範圍**（0.4／0.5 建立）：`role`、`owner_user_id` 為 NULL，`scope` 為 `read`／`operate`／`admin`，
//!   權限凍結在升級前（[`crate::rbac::LegacyScope`]）。
//!
//! 建立與撤銷在同一個交易中寫入稽核雜湊鏈（[`crate::audit`]）。
//!
//! token 為 `kat_<token_id>_<secret>`：`token_id` 是 16 個十六進位字元（8 bytes 隨機值），公開、
//! 用於查詢與撤銷；`secret` 是 32 bytes 隨機值的 base64url（43 個字元），資料庫只保存它的 SHA-256。

use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use super::http::{b64url_decode, b64url_encode};
use crate::rbac::{LegacyScope, Role};

/// token 的前綴（secret 掃描工具以此辨識）
pub const TOKEN_PREFIX: &str = "kat_";
/// `token_id` 的長度（十六進位字元）
pub const TOKEN_ID_LEN: usize = 16;
/// 秘密的 bytes 數
pub const SECRET_BYTES: usize = 32;
/// 秘密的 base64url 長度
pub const SECRET_LEN: usize = 43;

/// 0.4／0.5 的範圍 `read`（測試建立舊 token 用；新的 token 以角色或擁有者建立）
#[cfg(test)]
pub const SCOPE_READ: &str = "read";

/// `api_tokens.logged`：建立已寫入 Kairos 日誌
pub const LOGGED_CREATED: i64 = 1;
/// `api_tokens.logged`：撤銷已寫入 Kairos 日誌
pub const LOGGED_REVOKED: i64 = 2;

/// 名稱的長度上限（字元）
pub const MAX_NAME_CHARS: usize = 64;

/// 拆開後的 token
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedToken<'a> {
    pub token_id: &'a str,
    pub secret: &'a str,
}

/// 解析 `kat_<16 hex>_<43 base64url>`；格式不符時為 `None`
pub fn parse_token(text: &str) -> Option<ParsedToken<'_>> {
    let rest = text.strip_prefix(TOKEN_PREFIX)?;
    let (token_id, secret) = rest.split_once('_')?;
    let id_ok = token_id.len() == TOKEN_ID_LEN
        && token_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let secret_ok = secret.len() == SECRET_LEN
        && b64url_decode(secret).is_some_and(|b| b.len() == SECRET_BYTES);
    (id_ok && secret_ok).then_some(ParsedToken { token_id, secret })
}

/// 秘密的 SHA-256（資料庫保存的值）
pub fn hash_secret(secret: &str) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, secret.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// 以作業系統的 CSPRNG 產生新的 (token_id, secret)
pub fn generate() -> Result<(String, String), String> {
    let rng = SystemRandom::new();
    let mut id = [0u8; TOKEN_ID_LEN / 2];
    let mut secret = [0u8; SECRET_BYTES];
    rng.fill(&mut id)
        .and_then(|_| rng.fill(&mut secret))
        .map_err(|_| "無法取得隨機數（作業系統的 CSPRNG）".to_string())?;
    let id: String = id.iter().map(|b| format!("{b:02x}")).collect();
    Ok((id, b64url_encode(&secret)))
}

/// token 的權限來源
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenGrant {
    /// 0.4／0.5 的範圍（`read`、`operate`、`admin`；`--scope`，已棄用）
    Legacy(LegacyScope),
    /// 內建角色（全站）
    Role(Role),
    /// 屬於使用者（權限＝擁有者的權限，∩ 角色）
    User {
        user_id: i64,
        username: String,
        role: Option<Role>,
    },
}

impl TokenGrant {
    /// 列表與日誌中的寫法：`role:operator`、`user:chen.y`、`user:chen.y∩viewer`、`scope:read`
    pub fn label(&self) -> String {
        match self {
            TokenGrant::Legacy(s) => format!("scope:{}", s.as_str()),
            TokenGrant::Role(r) => format!("role:{}", r.as_str()),
            TokenGrant::User {
                username,
                role: None,
                ..
            } => format!("user:{username}"),
            TokenGrant::User {
                username,
                role: Some(r),
                ..
            } => format!("user:{username}∩{}", r.as_str()),
        }
    }
}

/// `api_tokens` 的一列（不含雜湊）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRow {
    pub token_id: String,
    pub name: String,
    /// 舊範圍（0.4／0.5）；角色或使用者的 token 為空字串
    pub scope: String,
    /// 角色（0.6）
    pub role: Option<String>,
    /// 擁有者
    pub owner_user_id: Option<i64>,
    pub owner_username: Option<String>,
    pub created_at: i64,
    pub created_by: Option<String>,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub revoked_by: Option<String>,
    pub revoke_reason: Option<String>,
    pub last_used_at: Option<i64>,
    pub last_used_ip: Option<String>,
}

/// token 的狀態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenState {
    Active,
    Revoked,
    Expired,
}

impl TokenState {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenState::Active => "active",
            TokenState::Revoked => "revoked",
            TokenState::Expired => "expired",
        }
    }
}

impl TokenRow {
    /// 權限來源的寫法（[`TokenGrant::label`]）；無法辨識時為 `unknown`
    pub fn grant_label(&self) -> String {
        match (&self.owner_user_id, &self.role) {
            (Some(_), r) => {
                let u = self.owner_username.as_deref().unwrap_or("（已刪除）");
                match r {
                    Some(r) => format!("user:{u}∩{r}"),
                    None => format!("user:{u}"),
                }
            }
            (None, Some(r)) => format!("role:{r}"),
            (None, None) if LegacyScope::parse(&self.scope).is_some() => {
                format!("scope:{}", self.scope)
            }
            _ => "unknown".to_string(),
        }
    }

    pub fn state(&self, now_ms: i64) -> TokenState {
        if self.revoked_at.is_some() {
            TokenState::Revoked
        } else if self.expires_at.is_some_and(|e| e <= now_ms) {
            TokenState::Expired
        } else {
            TokenState::Active
        }
    }
}

const COLUMNS: &str = "token_id, name, scope, created_at, created_by, expires_at, revoked_at, \
                       revoked_by, revoke_reason, last_used_at, last_used_ip, role, owner_user_id, \
                       (SELECT username FROM users WHERE users.user_id = api_tokens.owner_user_id)";

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<TokenRow> {
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

/// 名稱的檢查：1～64 個字元、不含控制字元
pub fn validate_name(name: &str) -> Result<(), String> {
    let n = name.chars().count();
    if n == 0 || name.trim().is_empty() {
        return Err("名稱不可為空".to_string());
    }
    if n > MAX_NAME_CHARS {
        return Err(format!("名稱最多 {MAX_NAME_CHARS} 個字元（目前 {n} 個）"));
    }
    if name.chars().any(char::is_control) {
        return Err("名稱不可含控制字元".to_string());
    }
    Ok(())
}

/// 新建立的 token：完整的 token 字串只在這裡出現一次
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewToken {
    pub token: String,
    pub row: TokenRow,
}

/// 建立 token（0.4／0.5 的範圍；測試用）
#[cfg(test)]
pub fn create(
    conn: &Connection,
    name: &str,
    scope: &str,
    expires_at: Option<i64>,
    created_by: Option<&str>,
    now_ms: i64,
) -> Result<NewToken, String> {
    let grant = TokenGrant::Legacy(
        LegacyScope::parse(scope).ok_or_else(|| format!("範圍 \"{scope}\" 無效"))?,
    );
    create_with(conn, name, &grant, expires_at, created_by, None, now_ms)
}

/// 建立 token，並在同一個交易中寫入稽核
pub fn create_with(
    conn: &Connection,
    name: &str,
    grant: &TokenGrant,
    expires_at: Option<i64>,
    created_by: Option<&str>,
    ip: Option<&str>,
    now_ms: i64,
) -> Result<NewToken, String> {
    validate_name(name)?;
    let (token_id, secret) = generate()?;
    let hash = hash_secret(&secret);
    let (scope, role, owner) = match grant {
        TokenGrant::Legacy(s) => (s.as_str().to_string(), None, None),
        TokenGrant::Role(r) => (String::new(), Some(r.as_str()), None),
        TokenGrant::User { user_id, role, .. } => {
            (String::new(), role.map(Role::as_str), Some(*user_id))
        }
    };
    let by = created_by.unwrap_or("（未知）");
    crate::audit::in_write_tx(conn, |conn| {
        conn.execute(
            "INSERT INTO api_tokens (token_id, name, secret_hash, scope, created_at, created_by, expires_at,
                role, owner_user_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![token_id, name, &hash[..], scope, now_ms, created_by, expires_at, role, owner],
        )?;
        crate::audit::append(
            conn,
            now_ms,
            &crate::audit::Audit {
                actor: by,
                actor_ip: ip,
                category: "api_token",
                action: "token.create",
                object: Some(&token_id),
                result: "ok",
                details: Some(json!({
                    "name": name,
                    "grant": grant.label(),
                    "expires_at": expires_at.map(super::http::ts_ms),
                })),
                ..Default::default()
            },
        )?;
        Ok(())
    })
    .map_err(|e| format!("無法寫入 api_tokens: {e}"))?;
    Ok(NewToken {
        token: format!("{TOKEN_PREFIX}{token_id}_{secret}"),
        row: TokenRow {
            token_id,
            name: name.to_string(),
            scope,
            role: role.map(str::to_string),
            owner_user_id: owner,
            owner_username: match grant {
                TokenGrant::User { username, .. } => Some(username.clone()),
                _ => None,
            },
            created_at: now_ms,
            created_by: created_by.map(str::to_string),
            expires_at,
            revoked_at: None,
            revoked_by: None,
            revoke_reason: None,
            last_used_at: None,
            last_used_ip: None,
        },
    })
}

/// 所有 token（依建立時間）
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<TokenRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM api_tokens ORDER BY created_at, token_id"
    ))?;
    let rows = stmt.query_map([], row_of)?;
    rows.collect()
}

pub fn get(conn: &Connection, token_id: &str) -> rusqlite::Result<Option<TokenRow>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM api_tokens WHERE token_id = ?1"),
        [token_id],
        row_of,
    )
    .optional()
}

/// 撤銷的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revoked {
    /// 這次撤銷
    Now(TokenRow),
    /// 先前已撤銷（不算錯誤）
    Already(TokenRow),
    NotFound,
}

pub fn revoke(
    conn: &Connection,
    token_id: &str,
    by: Option<&str>,
    reason: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<Revoked> {
    revoke_with(conn, token_id, by, None, reason, now_ms)
}

/// 撤銷，並在同一個交易中寫入稽核（重複撤銷不寫入）
pub fn revoke_with(
    conn: &Connection,
    token_id: &str,
    by: Option<&str>,
    ip: Option<&str>,
    reason: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<Revoked> {
    crate::audit::in_write_tx(conn, |conn| {
        let changed = conn.execute(
            "UPDATE api_tokens SET revoked_at = ?2, revoked_by = ?3, revoke_reason = ?4
             WHERE token_id = ?1 AND revoked_at IS NULL",
            params![token_id, now_ms, by, reason],
        )?;
        if changed > 0 {
            crate::audit::append(
                conn,
                now_ms,
                &crate::audit::Audit {
                    actor: by.unwrap_or("（未知）"),
                    actor_ip: ip,
                    category: "api_token",
                    action: "token.revoke",
                    object: Some(token_id),
                    result: "ok",
                    details: Some(json!({ "reason": reason })),
                    ..Default::default()
                },
            )?;
        }
        Ok(match (changed, get(conn, token_id)?) {
            (_, None) => Revoked::NotFound,
            (0, Some(row)) => Revoked::Already(row),
            (_, Some(row)) => Revoked::Now(row),
        })
    })
}

/// 驗證用：token_id 對應的雜湊與狀態
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredToken {
    pub secret_hash: Vec<u8>,
    pub name: String,
    pub scope: String,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub role: Option<String>,
    pub owner_user_id: Option<i64>,
}

pub fn lookup(conn: &Connection, token_id: &str) -> rusqlite::Result<Option<StoredToken>> {
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

/// 建立或撤銷尚未寫入 Kairos 日誌的 token：(列, 目前的 logged 旗標)
pub fn pending_audit(conn: &Connection) -> rusqlite::Result<Vec<(TokenRow, i64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS}, logged FROM api_tokens
         WHERE (logged & 1) = 0 OR (revoked_at IS NOT NULL AND (logged & 2) = 0)
         ORDER BY created_at LIMIT 100"
    ))?;
    let rows = stmt.query_map([], |r| Ok((row_of(r)?, r.get::<_, i64>(14)?)))?;
    rows.collect()
}

pub fn mark_logged(conn: &Connection, token_id: &str, flags: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE api_tokens SET logged = logged | ?2 WHERE token_id = ?1",
        params![token_id, flags],
    )
    .map(|_| ())
}

/// 寫回最後使用的時間與來源（不會讓時間倒退）
pub fn store_last_used(
    conn: &Connection,
    entries: &[(String, i64, String)],
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sqlite::MIGRATIONS;

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::store::sqlite::migrate(&mut conn, MIGRATIONS).unwrap();
        conn
    }

    #[test]
    fn token_format_and_parsing() {
        let (id, secret) = generate().unwrap();
        assert_eq!(id.len(), TOKEN_ID_LEN);
        assert_eq!(secret.len(), SECRET_LEN);
        let token = format!("{TOKEN_PREFIX}{id}_{secret}");
        let parsed = parse_token(&token).unwrap();
        assert_eq!(parsed.token_id, id);
        assert_eq!(parsed.secret, secret);
        // 每次產生的都不同
        assert_ne!(generate().unwrap(), (id.clone(), secret.clone()));
        for bad in [
            "".to_string(),
            format!("kat_{id}"),
            format!("kat_{id}_"),
            format!("kat_{}_{secret}", &id[..15]),
            format!("kat_{}_{secret}", id.to_uppercase()),
            format!("xat_{id}_{secret}"),
            format!("kat_{id}_{}", &secret[..42]),
            format!("kat_{id}_{secret}x"),
            format!("kat_{id}_{}!", &secret[..42]),
        ] {
            assert!(parse_token(&bad).is_none(), "{bad}");
        }
        // 雜湊是 SHA-256
        assert_eq!(
            hash_secret("abc")[..4],
            [0xba, 0x78, 0x16, 0xbf],
            "SHA-256(\"abc\") 的前 4 bytes"
        );
    }

    #[test]
    fn create_list_lookup_and_revoke() {
        let conn = db();
        let t = create(
            &conn,
            "grafana",
            SCOPE_READ,
            Some(5_000),
            Some("ops"),
            1_000,
        )
        .unwrap();
        let parsed = parse_token(&t.token).unwrap();
        assert_eq!(parsed.token_id, t.row.token_id);
        let stored = lookup(&conn, parsed.token_id).unwrap().unwrap();
        // 只保存雜湊：資料庫中沒有秘密本身
        assert_eq!(stored.secret_hash, hash_secret(parsed.secret).to_vec());
        let raw: Vec<u8> = conn
            .query_row("SELECT secret_hash FROM api_tokens", [], |r| r.get(0))
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&raw).contains(parsed.secret),
            "秘密不應出現在資料庫"
        );
        assert_eq!(stored.scope, "read");
        assert_eq!(t.row.state(1_000), TokenState::Active);
        assert_eq!(t.row.state(5_000), TokenState::Expired);
        assert!(lookup(&conn, "0000000000000000").unwrap().is_none());

        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].created_by.as_deref(), Some("ops"));

        // 撤銷、重複撤銷、不存在
        let r = revoke(&conn, &t.row.token_id, Some("ops"), Some("離職"), 2_000).unwrap();
        let Revoked::Now(row) = r else {
            panic!("{r:?}")
        };
        assert_eq!(row.revoked_at, Some(2_000));
        assert_eq!(row.revoke_reason.as_deref(), Some("離職"));
        assert_eq!(row.state(2_000), TokenState::Revoked);
        assert!(matches!(
            revoke(&conn, &t.row.token_id, None, None, 3_000).unwrap(),
            Revoked::Already(row) if row.revoked_at == Some(2_000)
        ));
        assert_eq!(
            revoke(&conn, "ffffffffffffffff", None, None, 3_000).unwrap(),
            Revoked::NotFound
        );

        // 稽核：建立與撤銷各記一次
        let pending = pending_audit(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, 0);
        mark_logged(&conn, &t.row.token_id, LOGGED_CREATED | LOGGED_REVOKED).unwrap();
        assert!(pending_audit(&conn).unwrap().is_empty());

        // 最後使用：不倒退
        store_last_used(&conn, &[(t.row.token_id.clone(), 9_000, "10.0.0.1".into())]).unwrap();
        store_last_used(&conn, &[(t.row.token_id.clone(), 8_000, "10.0.0.2".into())]).unwrap();
        let row = get(&conn, &t.row.token_id).unwrap().unwrap();
        assert_eq!(row.last_used_at, Some(9_000));
        assert_eq!(row.last_used_ip.as_deref(), Some("10.0.0.1"));

        // 名稱的檢查
        for bad in ["", "   ", "a\nb", &"x".repeat(65)] {
            assert!(
                create(&conn, bad, SCOPE_READ, None, None, 0).is_err(),
                "{bad:?}"
            );
        }
        assert!(create(&conn, &"主".repeat(64), "read", None, None, 0).is_ok());
    }
}
