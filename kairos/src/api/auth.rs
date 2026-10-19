//! 驗證（`kairos/API.md` 第 4 節、第 12 節）：API token 與主控台的工作階段。
//!
//! - **API token**：`Authorization: Bearer kat_…`。以 token_id 查詢資料庫，以常數時間比較秘密的 SHA-256；
//!   token_id 不存在時仍比較一次（回應時間不洩漏 token_id 是否存在）。權限依 [`TokenGrant`]
//!   （角色、擁有者、或 0.4／0.5 的凍結範圍）。
//! - **工作階段**：cookie `__Host-kairos_session`（HttpOnly、Secure、SameSite=Strict、Path=/）。資料庫只保存
//!   秘密的 SHA-256；閒置與絕對逾時、使用者停用或刪除時立即失效。會改變狀態的方法（POST、PUT、PATCH、
//!   DELETE）另需 `X-CSRF-Token`（由秘密以主控台金鑰衍生，[`crate::users::secrets::ConsoleKey::csrf_token`]），
//!   且 `Origin`（有帶時）必須與 `Host` 相同、`Sec-Fetch-Site` 不可是 `cross-site`。
//! - 同時帶 `Authorization` 與 cookie 時以 `Authorization` 為準（不混用兩種身分）。
//! - 同一來源（IPv6 以 /64 合併）驗證或登入失敗時扣額度（[`FailureLimiter`]），用完時該來源的請求一律 429。
//! - 得到 [`Principal`]：端點以 [`Principal::require`] 檢查 [`Permission`]（集中定義在 [`crate::rbac`]）。

use chrono::{DateTime, Utc};
use hyper::Method;
use hyper::header::{AUTHORIZATION, COOKIE, HOST, HeaderMap};
use log::warn;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

use super::db::{ReadPool, WriteConn, query_failed};
use super::http::{ApiError, CODE_RATE_LIMITED, ts_ms};
use super::tokens::{self, StoredToken, hash_secret, parse_token};
use crate::config::AppConfig;
pub use crate::rbac::Permission;
use crate::rbac::{Assignment, Grants, HostAccess, LegacyScope, PermSet, Role};
use crate::users::secrets::ConsoleKey;
use crate::users::store as us;

/// 驗證失敗的額度：可連續幾次
pub const FAILURE_BURST: f64 = 10.0;
/// 驗證失敗的額度：每秒恢復幾次（每 6 秒 1 次）
pub const FAILURE_REFILL_PER_SEC: f64 = 1.0 / 6.0;
/// 追蹤的來源數上限
pub const MAX_TRACKED_SOURCES: usize = 10_000;

/// 工作階段的 cookie 名稱（`__Host-` 前綴：瀏覽器只接受 Secure、Path=/、沒有 Domain 的設定）
pub const SESSION_COOKIE: &str = "__Host-kairos_session";
/// CSRF token 的標頭
pub const CSRF_HEADER: &str = "x-csrf-token";
/// 最後活動時間最多多久寫回一次
pub const TOUCH_EVERY_MS: i64 = 60_000;

/// 通過驗證的身分
#[derive(Debug, Clone, PartialEq)]
pub enum PrincipalKind {
    ApiToken {
        token_id: String,
        name: String,
        /// 0.4／0.5 的範圍（`role`、`owner` 都沒有時）
        legacy_scope: Option<String>,
        role: Option<Role>,
        /// 擁有者 (user_id, 帳號)
        owner: Option<(i64, String)>,
        expires_at: Option<i64>,
    },
    Session {
        user_id: i64,
        username: String,
        display_name: Option<String>,
        source: String,
        method: String,
        /// 公開的工作階段 id（清單中的「目前這個」）
        session_id: String,
        /// cookie 秘密的 SHA-256（結束目前或其他工作階段用；不輸出）
        session_hash: [u8; 32],
        created_at: i64,
        expires_at: i64,
        idle_timeout_ms: i64,
        must_change_password: bool,
        mfa: bool,
        /// 回給主控台的 CSRF token（`GET /api/v1/me`）
        csrf_token: String,
        /// 權限的來源（個人指派與目錄服務群組）
        assignments: Vec<(Assignment, Option<String>)>,
    },
}

/// 通過驗證的身分與它的權限
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub kind: PrincipalKind,
    pub grants: Grants,
}

impl Principal {
    /// 檢查全站範圍的權限；沒有時回 403 `forbidden`（`details.permission` 為所需的權限，
    /// `details.required` 為擁有它的最低角色；0.4／0.5 的 token 為最低範圍）
    pub fn require(&self, permission: Permission) -> Result<(), ApiError> {
        if self.grants.has(permission) {
            return Ok(());
        }
        let required = match &self.kind {
            PrincipalKind::ApiToken {
                legacy_scope: Some(_),
                ..
            } => LegacyScope::required_for(permission)
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| permission.least_role().as_str().to_string()),
            _ => permission.least_role().as_str().to_string(),
        };
        Err(ApiError::forbidden().with_details(json!({
            "required": required,
            "permission": permission.name(),
        })))
    }

    /// 主機的可見範圍；完全沒有主機的讀取權限時回 403
    pub fn host_access(&self) -> Result<HostAccess, ApiError> {
        match self.grants.host_access() {
            HostAccess::None => self
                .require(Permission::HostsRead)
                .map(|_| HostAccess::None),
            a => Ok(a),
        }
    }

    /// 日誌中的身分（不含秘密）
    pub fn label(&self) -> String {
        match &self.kind {
            PrincipalKind::ApiToken { token_id, .. } => format!("token {token_id}"),
            PrincipalKind::Session { username, .. } => format!("user {username}"),
        }
    }

    /// 稽核的執行者：`token:<token_id>` 或 `user:<帳號>`
    pub fn actor(&self) -> String {
        match &self.kind {
            PrincipalKind::ApiToken { token_id, .. } => format!("token:{token_id}"),
            PrincipalKind::Session { username, .. } => format!("user:{username}"),
        }
    }

    /// 工作階段的使用者 (user_id, 帳號)；擁有者的 token 不算（只有工作階段能做個人設定）
    pub fn session_user(&self) -> Option<(i64, &str)> {
        match &self.kind {
            PrincipalKind::Session {
                user_id, username, ..
            } => Some((*user_id, username)),
            PrincipalKind::ApiToken { .. } => None,
        }
    }

    /// 代表的使用者：工作階段的使用者，或 token 的擁有者（「不能停用自己」等防呆用）
    pub fn acting_user_id(&self) -> Option<i64> {
        match &self.kind {
            PrincipalKind::Session { user_id, .. } => Some(*user_id),
            PrincipalKind::ApiToken { owner, .. } => owner.as_ref().map(|(id, _)| *id),
        }
    }

    pub fn session_hash(&self) -> Option<&[u8; 32]> {
        match &self.kind {
            PrincipalKind::Session { session_hash, .. } => Some(session_hash),
            PrincipalKind::ApiToken { .. } => None,
        }
    }

    /// 管理員以 `kairos users create` 或重設密碼後，第一次登入必須先變更密碼
    pub fn must_change_password(&self) -> bool {
        matches!(
            self.kind,
            PrincipalKind::Session {
                must_change_password: true,
                ..
            }
        )
    }

    /// `GET /api/v1/me`
    pub fn me_json(&self) -> Value {
        let mut v = match &self.kind {
            PrincipalKind::ApiToken {
                token_id,
                name,
                legacy_scope,
                role,
                owner,
                expires_at,
            } => json!({
                "type": "api_token",
                "token_id": token_id,
                "name": name,
                "scopes": legacy_scope.iter().collect::<Vec<_>>(),
                "role": role.map(Role::as_str),
                "owner": owner.as_ref().map(|(id, u)| json!({"user_id": id, "username": u})),
                "expires_at": expires_at.map(ts_ms),
            }),
            PrincipalKind::Session {
                user_id,
                username,
                display_name,
                source,
                method,
                session_id,
                created_at,
                expires_at,
                idle_timeout_ms,
                must_change_password,
                mfa,
                csrf_token,
                assignments,
                ..
            } => json!({
                "type": "session",
                "user": {
                    "user_id": user_id,
                    "username": username,
                    "display_name": display_name,
                    "source": source,
                    "mfa": mfa,
                    "must_change_password": must_change_password,
                },
                "roles": assignments.iter().map(|(a, from)| {
                    let mut j = a.json();
                    j["via_group"] = json!(from);
                    j
                }).collect::<Vec<_>>(),
                "session": {
                    "session_id": session_id,
                    "method": method,
                    "created_at": ts_ms(*created_at),
                    "expires_at": ts_ms(*expires_at),
                    "idle_timeout_secs": idle_timeout_ms / 1000,
                },
                "csrf_token": csrf_token,
            }),
        };
        let g = self.grants.json();
        v["permissions"] = g["permissions"].clone();
        v["group_permissions"] = g["group_permissions"].clone();
        v
    }
}

/// 一個來源的失敗額度（token bucket）
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
    /// 已記過「開始限制」的日誌
    logged: bool,
}

/// 驗證失敗的速率限制
#[derive(Debug, Default)]
pub struct FailureLimiter {
    sources: Mutex<HashMap<IpAddr, Bucket>>,
}

impl FailureLimiter {
    fn refill(b: &mut Bucket, now: Instant) {
        let elapsed = now.saturating_duration_since(b.updated).as_secs_f64();
        b.tokens = (b.tokens + elapsed * FAILURE_REFILL_PER_SEC).min(FAILURE_BURST);
        b.updated = now;
    }

    /// 請求開始時檢查：額度用完時回傳恢復 1 次所需的秒數
    pub fn check(&self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        let key = crate::per_ip::source_key(ip);
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        let Some(b) = sources.get_mut(&key) else {
            return Ok(());
        };
        Self::refill(b, now);
        if b.tokens >= 1.0 {
            Ok(())
        } else {
            Err(((1.0 - b.tokens) / FAILURE_REFILL_PER_SEC).ceil().max(1.0) as u64)
        }
    }

    /// 驗證失敗時扣一次額度；回傳是否剛開始被限制（第一次用完，供節流的日誌）
    pub fn fail(&self, ip: IpAddr, now: Instant) -> bool {
        let key = crate::per_ip::source_key(ip);
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        if !sources.contains_key(&key) && sources.len() >= MAX_TRACKED_SOURCES {
            // 清掉最久沒有失敗的來源
            if let Some(oldest) = sources
                .iter()
                .min_by_key(|(_, b)| b.updated)
                .map(|(k, _)| *k)
            {
                sources.remove(&oldest);
            }
        }
        let b = sources.entry(key).or_insert(Bucket {
            tokens: FAILURE_BURST,
            updated: now,
            logged: false,
        });
        Self::refill(b, now);
        b.tokens = (b.tokens - 1.0).max(0.0);
        if b.tokens < 1.0 && !b.logged {
            b.logged = true;
            true
        } else {
            if b.tokens >= FAILURE_BURST - 1.0 {
                b.logged = false;
            }
            false
        }
    }

    /// 退回一次額度（登入先預扣、成功或與密碼無關的錯誤時退回）
    pub fn refund(&self, ip: IpAddr, now: Instant) {
        let key = crate::per_ip::source_key(ip);
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = sources.get_mut(&key) {
            Self::refill(b, now);
            b.tokens = (b.tokens + 1.0).min(FAILURE_BURST);
        }
    }

    /// 清掉已恢復滿額度的來源
    pub fn prune(&self, now: Instant) {
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        sources.retain(|_, b| {
            Self::refill(b, now);
            b.tokens < FAILURE_BURST
        });
    }
}

/// 最近一次成功使用（記憶體中；背景工作每 60 秒寫回資料庫）
#[derive(Debug, Default)]
pub struct LastUsed {
    entries: Mutex<HashMap<String, (i64, String)>>,
}

impl LastUsed {
    pub fn note(&self, token_id: &str, at_ms: i64, ip: IpAddr) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(token_id.to_string(), (at_ms, ip.to_string()));
    }

    /// 取出所有待寫回的紀錄
    pub fn take(&self) -> Vec<(String, i64, String)> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.drain().map(|(id, (at, ip))| (id, at, ip)).collect()
    }

    /// 寫回失敗時放回（不覆蓋較新的）
    pub fn restore(&self, items: Vec<(String, i64, String)>) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        for (id, at, ip) in items {
            entries.entry(id).or_insert((at, ip));
        }
    }
}

/// token 與工作階段的驗證
pub struct Authenticator {
    pool: Arc<ReadPool>,
    writer: Arc<WriteConn>,
    key: Option<Arc<ConsoleKey>>,
    config: Arc<AppConfig>,
    pub failures: FailureLimiter,
    pub last_used: LastUsed,
}

/// 找不到 token_id 時用來比較的雜湊（讓兩種情況的計算量相同）
const DUMMY_HASH: [u8; 32] = [0x5a; 32];

/// 會改變狀態的方法（需要 CSRF token）
pub fn is_unsafe(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// 取出 cookie 的值（多個 `Cookie` 標頭、`;` 分隔）
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim().to_string())
}

/// 同源檢查：`Origin`（有帶時）必須是 `https://<Host>`；`Sec-Fetch-Site` 不可是 `cross-site`
fn same_origin(headers: &HeaderMap) -> bool {
    if headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("cross-site"))
    {
        return false;
    }
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    !host.is_empty() && origin.eq_ignore_ascii_case(&format!("https://{host}"))
}

/// 由 token 的資料計算權限（擁有者的權限 ∩ 角色；舊範圍為凍結的集合；無法辨識時沒有權限）
fn token_grants(
    stored: &StoredToken,
    owner: Option<&(us::UserRow, Grants)>,
) -> (Grants, Option<Role>, Option<String>) {
    let role = stored.role.as_deref().and_then(Role::parse);
    match (stored.owner_user_id, &stored.role) {
        (Some(_), _) => {
            let Some((_, g)) = owner else {
                return (Grants::default(), role, None);
            };
            let g = match (&stored.role, role) {
                (Some(_), Some(r)) => g.intersect(&Grants::global(r.permissions())),
                // 不認得的角色：沒有權限
                (Some(_), None) => Grants::default(),
                (None, _) => g.clone(),
            };
            (g, role, None)
        }
        (None, Some(_)) => (
            Grants::global(role.map_or(PermSet::EMPTY, Role::permissions)),
            role,
            None,
        ),
        (None, None) => {
            // 0.4／0.5 的範圍：以逗號分隔時取聯集（與 0.5 的「任一範圍足夠」相同）
            let set = stored
                .scope
                .split(',')
                .filter_map(LegacyScope::parse)
                .fold(PermSet::EMPTY, |acc, s| acc.union(s.permissions()));
            (Grants::global(set), None, Some(stored.scope.clone()))
        }
    }
}

/// 資料庫查詢的結果（工作階段）
struct SessionLookup {
    session: us::SessionRow,
    user: us::UserRow,
    grants: Grants,
    assignments: Vec<(Assignment, Option<String>)>,
}

impl Authenticator {
    pub fn new(
        pool: Arc<ReadPool>,
        writer: Arc<WriteConn>,
        key: Option<Arc<ConsoleKey>>,
        config: Arc<AppConfig>,
    ) -> Self {
        Self {
            pool,
            writer,
            key,
            config,
            failures: FailureLimiter::default(),
            last_used: LastUsed::default(),
        }
    }

    pub fn key(&self) -> Option<&Arc<ConsoleKey>> {
        self.key.as_ref()
    }

    /// 驗證請求（`Authorization` 優先，其次是工作階段的 cookie）
    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
        method: &Method,
        peer: IpAddr,
        now: DateTime<Utc>,
    ) -> Result<Principal, ApiError> {
        if let Err(retry) = self.failures.check(peer, Instant::now()) {
            return Err(ApiError::rate_limited(retry));
        }
        if headers.contains_key(AUTHORIZATION) {
            return self.authenticate_token(headers, peer, now).await;
        }
        match cookie_value(headers, SESSION_COOKIE) {
            Some(secret) => {
                self.authenticate_session(headers, method, &secret, peer, now)
                    .await
            }
            // 沒有出示任何憑據（未登入的瀏覽器開啟主控台、登入頁讀取 /api/v1/me）不是猜測，不扣失敗額度
            // （0.7.0；API.md 第 4.3 節）。出示了錯誤的 token 或已失效的工作階段照常扣
            None => Err(ApiError::unauthorized()),
        }
    }

    async fn authenticate_token(
        &self,
        headers: &HeaderMap,
        peer: IpAddr,
        now: DateTime<Utc>,
    ) -> Result<Principal, ApiError> {
        let token = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                let (scheme, rest) = v.trim().split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
            })
            .map(str::to_string);
        let Some(token) = token else {
            return Err(self.failed(peer, "Authorization 不是 Bearer"));
        };
        let Some(parsed) = parse_token(&token) else {
            return Err(self.failed(peer, "token 格式不正確"));
        };
        let token_id = parsed.token_id.to_string();
        let presented = hash_secret(parsed.secret);
        let id = token_id.clone();
        let (stored, owner) = self
            .pool
            .read("API token", move |conn| {
                let stored = tokens::lookup(conn, &id).map_err(query_failed("API token"))?;
                let owner = match stored.as_ref().and_then(|s| s.owner_user_id) {
                    Some(uid) => {
                        match us::get_user(conn, uid).map_err(query_failed("API token"))? {
                            Some(u) if u.active() => {
                                let (g, _) = us::grants_for(conn, uid, &[])
                                    .map_err(query_failed("API token"))?;
                                Some((u, g))
                            }
                            _ => None,
                        }
                    }
                    None => None,
                };
                Ok((stored, owner))
            })
            .await?;
        let valid = verify(stored.as_ref(), &presented, now.timestamp_millis());
        let Some(stored) = stored.filter(|_| valid) else {
            return Err(self.failed(peer, "token 不存在、已撤銷、已過期或不正確"));
        };
        // 擁有者停用或刪除：token 失效
        if stored.owner_user_id.is_some() && owner.is_none() {
            return Err(self.failed(peer, "token 的擁有者已停用或刪除"));
        }
        let (grants, role, legacy) = token_grants(&stored, owner.as_ref());
        self.last_used.note(&token_id, now.timestamp_millis(), peer);
        Ok(Principal {
            kind: PrincipalKind::ApiToken {
                token_id,
                name: stored.name,
                legacy_scope: legacy,
                role,
                owner: owner.map(|(u, _)| (u.user_id, u.username)),
                expires_at: stored.expires_at,
            },
            grants,
        })
    }

    async fn authenticate_session(
        &self,
        headers: &HeaderMap,
        method: &Method,
        secret: &str,
        peer: IpAddr,
        now: DateTime<Utc>,
    ) -> Result<Principal, ApiError> {
        let Some(key) = self.key.clone() else {
            return Err(ApiError::unauthorized());
        };
        // cookie 的值：43 個 base64url 字元（32 bytes）
        if secret.len() != 43 || super::http::b64url_decode(secret).is_none_or(|b| b.len() != 32) {
            return Err(self.failed(peer, "工作階段的格式不正確"));
        }
        let hash = crate::users::sha256(secret.as_bytes());
        let now_ms = now.timestamp_millis();
        let ldap_enabled = self.config.ldap.enabled;
        let found: Option<SessionLookup> = self
            .pool
            .read("工作階段", move |conn| {
                let Some(session) =
                    us::session_by_hash(conn, &hash).map_err(query_failed("工作階段"))?
                else {
                    return Ok(None);
                };
                let Some(user) =
                    us::get_user(conn, session.user_id).map_err(query_failed("工作階段"))?
                else {
                    return Ok(None);
                };
                let groups = if ldap_enabled {
                    session.ldap_groups.clone()
                } else {
                    Vec::new()
                };
                let (grants, assignments) = us::grants_for(conn, user.user_id, &groups)
                    .map_err(query_failed("工作階段"))?;
                Ok(Some(SessionLookup {
                    session,
                    user,
                    grants,
                    assignments,
                }))
            })
            .await?;
        let Some(found) = found else {
            return Err(self.failed(peer, "工作階段不存在"));
        };
        let SessionLookup {
            session,
            user,
            grants,
            assignments,
        } = found;
        // 已結束、逾時、使用者停用、目錄服務帳號但目錄服務已停用：都需要重新登入（不扣失敗額度）
        let usable = session.valid(now_ms)
            && user.active()
            && (user.source != us::SOURCE_LDAP || ldap_enabled);
        if !usable {
            return Err(ApiError::session_expired());
        }
        if is_unsafe(method) {
            let presented = headers
                .get(CSRF_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !key.csrf_matches(secret, presented) || !same_origin(headers) {
                return Err(ApiError::csrf_failed());
            }
        }
        if now_ms - session.last_seen_at >= TOUCH_EVERY_MS {
            let writer = self.writer.clone();
            tokio::spawn(async move {
                let _ = writer
                    .write(move |conn| Ok(us::touch_session(conn, &hash, now_ms)?))
                    .await;
            });
        }
        Ok(Principal {
            kind: PrincipalKind::Session {
                user_id: user.user_id,
                username: user.username,
                display_name: user.display_name,
                source: user.source,
                method: session.method,
                session_id: session.public_id,
                session_hash: hash,
                created_at: session.created_at,
                expires_at: session.expires_at,
                idle_timeout_ms: session.idle_timeout_ms,
                must_change_password: user.must_change_password,
                mfa: user.totp_enabled_at.is_some(),
                csrf_token: key.csrf_token(secret),
                assignments,
            },
            grants,
        })
    }

    /// 重新確認身分並重新計算主機的可見範圍（SSE 每次心跳時）：撤銷、過期、工作階段結束、使用者停用、
    /// 或已經沒有主機的讀取權限時回 `Some(HostAccess::None)`（關閉串流）；資料庫暫時無法查詢時回 `None`
    /// （沿用目前的範圍，不中斷串流）
    #[cfg(test)]
    pub async fn revalidate(&self, p: &Principal, now: DateTime<Utc>) -> Option<HostAccess> {
        self.revalidate_grants(p, now)
            .await
            .map(|g| g.host_access())
    }

    /// 重新確認身分並重新計算權限（SSE 每次心跳時；主機的範圍與告警事件的推送都依它）：撤銷、過期、
    /// 工作階段結束、使用者停用時回空的權限；資料庫暫時無法查詢時回 `None`（沿用目前的權限）
    pub async fn revalidate_grants(&self, p: &Principal, now: DateTime<Utc>) -> Option<Grants> {
        let now_ms = now.timestamp_millis();
        let ldap_enabled = self.config.ldap.enabled;
        let r = match &p.kind {
            PrincipalKind::ApiToken { token_id, .. } => {
                let id = token_id.clone();
                self.pool
                    .read("API token", move |conn| {
                        let Some(s) =
                            tokens::lookup(conn, &id).map_err(query_failed("API token"))?
                        else {
                            return Ok(Grants::default());
                        };
                        if s.revoked_at.is_some() || s.expires_at.is_some_and(|e| e <= now_ms) {
                            return Ok(Grants::default());
                        }
                        let owner = match s.owner_user_id {
                            Some(uid) => {
                                match us::get_user(conn, uid).map_err(query_failed("API token"))? {
                                    Some(u) if u.active() => {
                                        let (g, _) = us::grants_for(conn, uid, &[])
                                            .map_err(query_failed("API token"))?;
                                        Some((u, g))
                                    }
                                    _ => return Ok(Grants::default()),
                                }
                            }
                            None => None,
                        };
                        Ok(token_grants(&s, owner.as_ref()).0)
                    })
                    .await
            }
            PrincipalKind::Session {
                session_hash,
                user_id,
                ..
            } => {
                let (hash, uid) = (*session_hash, *user_id);
                self.pool
                    .read("工作階段", move |conn| {
                        let s =
                            us::session_by_hash(conn, &hash).map_err(query_failed("工作階段"))?;
                        let u = us::get_user(conn, uid).map_err(query_failed("工作階段"))?;
                        // SSE 本身不更新最後活動時間：閒置逾時以最後一個一般請求計算
                        match (s, u) {
                            (Some(s), Some(u))
                                if s.valid(now_ms)
                                    && u.active()
                                    && (u.source != us::SOURCE_LDAP || ldap_enabled) =>
                            {
                                let groups = if ldap_enabled {
                                    s.ldap_groups
                                } else {
                                    Vec::new()
                                };
                                us::grants_for(conn, uid, &groups)
                                    .map(|(g, _)| g)
                                    .map_err(query_failed("工作階段"))
                            }
                            _ => Ok(Grants::default()),
                        }
                    })
                    .await
            }
        };
        r.ok()
    }

    /// 驗證失敗：扣額度（第一次限制這個來源時記 `WARN`）並回 401
    pub fn failed(&self, peer: IpAddr, why: &str) -> ApiError {
        self.note_failure(peer, why);
        ApiError::unauthorized()
    }

    /// 扣失敗額度（登入失敗也使用）
    pub fn note_failure(&self, peer: IpAddr, why: &str) {
        if self.failures.fail(peer, Instant::now()) {
            warn!(
                event = "auth.rate_limited";
                "警告 {}：主控台 API：來自 {} 的請求驗證失敗太多次（{}），之後每 {} 秒只接受 1 次嘗試",
                CODE_RATE_LIMITED,
                crate::per_ip::source_key(peer),
                why,
                Duration::from_secs_f64(1.0 / FAILURE_REFILL_PER_SEC).as_secs()
            );
        }
    }
}

/// 比較雜湊（常數時間）並檢查撤銷與到期
fn verify(stored: Option<&StoredToken>, presented: &[u8; 32], now_ms: i64) -> bool {
    let expected: &[u8] = stored.map_or(&DUMMY_HASH[..], |s| s.secret_hash.as_slice());
    let same: bool = if expected.len() == presented.len() {
        expected.ct_eq(presented).into()
    } else {
        false
    };
    let Some(s) = stored else {
        return false;
    };
    same && s.revoked_at.is_none() && s.expires_at.is_none_or(|e| e > now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_limiter_blocks_after_the_burst_and_recovers() {
        let l = FailureLimiter::default();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let t0 = Instant::now();
        assert!(l.check(ip, t0).is_ok());
        let mut started = 0;
        for _ in 0..FAILURE_BURST as usize {
            if l.fail(ip, t0) {
                started += 1;
            }
        }
        // 第 10 次失敗後額度用完：只記一次「開始限制」
        assert_eq!(started, 1);
        assert_eq!(l.check(ip, t0), Err(6));
        // 其他來源不受影響；同一個 IPv6 /64 視為同一個來源
        assert!(l.check("192.0.2.2".parse().unwrap(), t0).is_ok());
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::2".parse().unwrap();
        for _ in 0..10 {
            l.fail(a, t0);
        }
        assert!(l.check(b, t0).is_err());
        // 約 6 秒後恢復 1 次
        assert!(l.check(ip, t0 + Duration::from_secs(7)).is_ok());
        // 一分鐘後恢復滿額度，被清除
        l.prune(t0 + Duration::from_secs(120));
        assert!(l.sources.lock().unwrap().is_empty());
    }

    fn stored(scope: &str, role: Option<&str>, owner: Option<i64>) -> StoredToken {
        StoredToken {
            secret_hash: hash_secret("s3cret").to_vec(),
            name: "t".into(),
            scope: scope.into(),
            expires_at: Some(10_000),
            revoked_at: None,
            role: role.map(str::to_string),
            owner_user_id: owner,
        }
    }

    #[test]
    fn verification_checks_hash_revocation_and_expiry() {
        let stored = stored("read", None, None);
        let good = hash_secret("s3cret");
        let bad = hash_secret("other");
        assert!(verify(Some(&stored), &good, 9_999));
        assert!(!verify(Some(&stored), &bad, 9_999));
        assert!(!verify(Some(&stored), &good, 10_000));
        assert!(!verify(None, &good, 0));
        let revoked = StoredToken {
            revoked_at: Some(1),
            ..stored.clone()
        };
        assert!(!verify(Some(&revoked), &good, 0));
        let forever = StoredToken {
            expires_at: None,
            ..stored
        };
        assert!(verify(Some(&forever), &good, i64::MAX));
    }

    #[test]
    fn token_grants_by_kind() {
        use Permission::*;
        // 舊範圍：凍結的集合
        let (g, role, legacy) = token_grants(&stored("operate", None, None), None);
        assert_eq!(g.global, LegacyScope::Operate.permissions());
        assert_eq!((role, legacy.as_deref()), (None, Some("operate")));
        assert!(!g.has(UsersRead));
        // 不認得的範圍：沒有權限
        assert!(
            token_grants(&stored("root", None, None), None)
                .0
                .global
                .is_empty()
        );
        // 角色
        let (g, role, _) = token_grants(&stored("", Some("auditor"), None), None);
        assert_eq!(role, Some(Role::Auditor));
        assert!(g.has(AuditRead) && !g.has(CertRevoke));
        assert!(
            token_grants(&stored("", Some("superuser"), None), None)
                .0
                .global
                .is_empty()
        );
        // 擁有者：擁有者的權限 ∩ 角色；擁有者不存在時沒有權限
        let owner_grants = Grants::global(Role::Admin.permissions());
        let user = us::UserRow {
            user_id: 1,
            username: "a".into(),
            display_name: None,
            email: None,
            source: "local".into(),
            ldap_dn: None,
            has_password: true,
            password_changed_at: None,
            must_change_password: false,
            totp_enabled_at: None,
            state: "active".into(),
            failed_logins: 0,
            locked_until: None,
            last_login_at: None,
            last_login_ip: None,
            created_at: 0,
            created_by: "t".into(),
            disabled_at: None,
            disabled_by: None,
            disable_reason: None,
        };
        let o = (user, owner_grants);
        let (g, _, _) = token_grants(&stored("", Some("viewer"), Some(1)), Some(&o));
        assert_eq!(g.global, Role::Viewer.permissions());
        let (g, _, _) = token_grants(&stored("", None, Some(1)), Some(&o));
        assert_eq!(g.global, Role::Admin.permissions());
        assert!(
            token_grants(&stored("", None, Some(1)), None)
                .0
                .global
                .is_empty()
        );
    }

    #[test]
    fn principal_permissions_and_denials() {
        let p = Principal {
            kind: PrincipalKind::ApiToken {
                token_id: "0123456789abcdef".into(),
                name: "t".into(),
                legacy_scope: Some("read".into()),
                role: None,
                owner: None,
                expires_at: None,
            },
            grants: Grants::global(LegacyScope::Read.permissions()),
        };
        assert!(p.require(Permission::HostsRead).is_ok());
        assert_eq!(p.host_access().unwrap(), HostAccess::All);
        // 0.5 的 403 細節：所需的範圍
        let denied = p.require(Permission::EnrollDecide).unwrap_err();
        assert_eq!(denied.status, hyper::StatusCode::FORBIDDEN);
        assert_eq!(denied.code, "forbidden");
        assert_eq!(denied.product_code, Some("KRS-E6104"));
        let d = denied.details.unwrap();
        assert_eq!(d["required"], "operate");
        assert_eq!(d["permission"], "enroll.requests:decide");
        // 新權限：舊範圍沒有對應，提示角色
        let d = p
            .require(Permission::UsersWrite)
            .unwrap_err()
            .details
            .unwrap();
        assert_eq!(d["required"], "admin");
        let v = p.me_json();
        assert_eq!(v["type"], "api_token");
        assert_eq!(v["scopes"][0], "read");
        assert!(
            v["permissions"]
                .as_array()
                .unwrap()
                .contains(&json!("hosts:read"))
        );
        assert_eq!(p.label(), "token 0123456789abcdef");
        assert_eq!(p.actor(), "token:0123456789abcdef");
        // 沒有任何權限：主機也是 403
        let none = Principal {
            grants: Grants::default(),
            ..p
        };
        assert_eq!(
            none.host_access().unwrap_err().status,
            hyper::StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn cookies_and_origin_checks() {
        let mut h = HeaderMap::new();
        h.append(
            COOKIE,
            "a=1; __Host-kairos_session=abc ; b=2".parse().unwrap(),
        );
        h.append(COOKIE, "c=3".parse().unwrap());
        assert_eq!(cookie_value(&h, SESSION_COOKIE).as_deref(), Some("abc"));
        assert_eq!(cookie_value(&h, "c").as_deref(), Some("3"));
        assert_eq!(cookie_value(&h, "kairos_session"), None);
        h.insert(HOST, "kairos.example:9443".parse().unwrap());
        assert!(same_origin(&h));
        h.insert("origin", "https://kairos.example:9443".parse().unwrap());
        assert!(same_origin(&h));
        h.insert("origin", "https://evil.example".parse().unwrap());
        assert!(!same_origin(&h));
        h.insert("origin", "http://kairos.example:9443".parse().unwrap());
        assert!(!same_origin(&h));
        h.remove("origin");
        h.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert!(!same_origin(&h));
        h.insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert!(same_origin(&h));
        assert!(is_unsafe(&Method::POST) && is_unsafe(&Method::DELETE));
        assert!(!is_unsafe(&Method::GET) && !is_unsafe(&Method::HEAD));
    }
}
