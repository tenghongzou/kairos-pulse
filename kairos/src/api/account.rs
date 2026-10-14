//! 登入、二階段驗證、登出與個人設定（`kairos/API.md` 第 12.2～12.4 節；設計包 S01、S23、D25）。
//!
//! | 方法與路徑 | 說明 |
//! |---|---|
//! | `GET /api/v1/auth/methods` | 可用的登入方式（不需驗證） |
//! | `POST /api/v1/auth/login` | 帳號密碼（本機或目錄服務）；需要 TOTP 時回 `mfa_required` |
//! | `POST /api/v1/auth/login/mfa` | 第二步：TOTP 或復原碼 |
//! | `POST /api/v1/auth/logout` | 結束目前的工作階段 |
//! | `GET /api/v1/me/sessions` | 自己的有效工作階段 |
//! | `POST /api/v1/me/sessions/revoke-others` | 結束其他所有工作階段、保留目前這個（D25） |
//! | `POST /api/v1/me/sessions/{session_id}/revoke` | 結束自己的一個工作階段 |
//! | `POST /api/v1/me/password` | 變更自己的密碼（本機帳號） |
//! | `POST /api/v1/me/totp/setup`、`/enable`、`/disable` | 綁定、啟用、停用 TOTP（本機帳號） |
//!
//! 密碼、工作階段的秘密、TOTP 秘密與復原碼都不寫入日誌與稽核；回應只在建立的那一次帶出復原碼。

use chrono::{DateTime, Utc};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, SET_COOKIE, USER_AGENT};
use hyper::{HeaderMap, Request, Response, StatusCode};
use log::{error, warn};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;

use super::ApiContext;
use super::auth::{Principal, SESSION_COOKIE};
use super::http::{
    self, ApiBody, ApiError, CODE_BAD_CREDENTIALS, CODE_BAD_MFA, CODE_LOCKED, read_json, str_field,
    ts_ms,
};
use crate::audit::{Event, in_write_tx};
use crate::users::ldap::DirError;
use crate::users::password::{self, HashParams};
use crate::users::store::{self as us, NewSession, SOURCE_LDAP, SOURCE_LOCAL, UserRow};
use crate::users::{PhantomLockout, new_public_id, new_session_secret, sha256, totp};

/// 登入請求本文的上限
pub const MAX_LOGIN_BODY: usize = 16 * 1024;
/// 第二步的挑戰有效 5 分鐘
pub const MFA_CHALLENGE_TTL_MS: i64 = 5 * 60_000;
/// 同一個挑戰最多嘗試幾次
pub const MFA_MAX_ATTEMPTS: u32 = 5;
/// 記憶體中的挑戰上限
pub const MAX_MFA_CHALLENGES: usize = 10_000;
/// 同時進行的密碼雜湊（Argon2id 每次約 19 MiB 記憶體）上限
pub const HASH_CONCURRENCY: usize = 2;
/// 等待雜湊的請求上限（超過時回 503，不讓排隊的請求佔滿主控台的連線）
pub const HASH_QUEUE: usize = 32;

/// 第二步的挑戰
#[derive(Debug, Clone)]
struct MfaChallenge {
    user_id: i64,
    username: String,
    method: &'static str,
    ldap_groups: Vec<String>,
    lock_key: String,
    expires_at: i64,
    attempts: u32,
}

/// 登入相關的記憶體狀態
pub struct LoginState {
    pub phantom: PhantomLockout,
    mfa: Mutex<HashMap<[u8; 32], MfaChallenge>>,
    hashing: Arc<Semaphore>,
    /// 等待或進行中的雜湊數
    queued: std::sync::atomic::AtomicUsize,
}

impl Default for LoginState {
    fn default() -> Self {
        Self {
            phantom: PhantomLockout::default(),
            mfa: Mutex::new(HashMap::new()),
            hashing: Arc::new(Semaphore::new(HASH_CONCURRENCY)),
            queued: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

/// [`LoginState::queued`] 的計數（離開時減回）
struct Queued<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl LoginState {
    /// 在 blocking 執行緒上做密碼雜湊（同時最多 [`HASH_CONCURRENCY`] 個）
    pub async fn blocking<R: Send + 'static>(
        &self,
        f: impl FnOnce() -> R + Send + 'static,
    ) -> Result<R, ApiError> {
        use std::sync::atomic::Ordering;
        if self.queued.fetch_add(1, Ordering::Relaxed) >= HASH_QUEUE {
            self.queued.fetch_sub(1, Ordering::Relaxed);
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                "登入請求太多，請稍後再試",
            )
            .with_retry_after(1));
        }
        let _queued = Queued(&self.queued);
        let _permit = self
            .hashing
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ApiError::internal())?;
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|_| ApiError::internal())
    }

    /// 清掉逾時的挑戰
    pub fn prune(&self, now_ms: i64) {
        self.mfa
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, c| c.expires_at > now_ms);
    }
}

/// 寫入一筆稽核（失敗只記日誌，不影響回應）
pub(super) async fn audit(ctx: &Arc<ApiContext>, ev: Event, now_ms: i64) {
    if let Err(e) = ctx.admin.write(move |c| ev.append(c, now_ms)).await {
        warn!("無法寫入稽核紀錄：{e}");
    }
}

pub(super) fn storage_error(e: impl std::fmt::Display) -> ApiError {
    warn!("主控台：使用者資料無法讀寫：{e}");
    ApiError::storage("使用者資料")
}

fn bad_credentials() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_credentials",
        "帳號或密碼不正確",
    )
    .with_product_code(CODE_BAD_CREDENTIALS)
}

fn account_locked(until: i64, now_ms: i64) -> ApiError {
    let secs = ((until - now_ms).max(1000) + 999) / 1000;
    ApiError::new(
        StatusCode::FORBIDDEN,
        "account_locked",
        format!(
            "連續登入失敗，帳號已暫時鎖定，約 {} 分鐘後解除",
            (secs + 59) / 60
        ),
    )
    .with_product_code(CODE_LOCKED)
    .with_details(json!({ "locked_until": ts_ms(until), "retry_after_secs": secs }))
    .with_retry_after(secs as u64)
}

fn not_a_session() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "not_a_session",
        "這個操作需要以帳號登入的工作階段（API token 不能使用）",
    )
}

fn console_key_missing() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "console_key_unavailable",
        "主控台金鑰無法使用（見 Kairos 的日誌）",
    )
}

/// 工作階段的 cookie（`__Host-`：Secure、Path=/、沒有 Domain；HttpOnly、SameSite=Strict）
pub fn session_cookie(secret: &str, max_age_secs: i64) -> String {
    format!(
        "{SESSION_COOKIE}={secret}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}"
    )
}

/// 清除 cookie
pub fn clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0")
}

fn with_cookie(mut resp: Response<ApiBody>, cookie: &str) -> Response<ApiBody> {
    if let Ok(v) = HeaderValue::from_str(cookie) {
        resp.headers_mut().append(SET_COOKIE, v);
    }
    resp
}

fn user_agent(h: &HeaderMap) -> Option<String> {
    h.get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().filter(|c| !c.is_control()).take(256).collect())
}

fn ip_text(ip: IpAddr) -> String {
    ip.to_canonical().to_string()
}

/// `GET /api/v1/auth/methods`
pub fn methods(ctx: &ApiContext, now: DateTime<Utc>) -> Response<ApiBody> {
    http::ok(
        now,
        json!({
            "local": true,
            "ldap": ctx.config.ldap.enabled,
            "mfa": "totp",
        }),
    )
}

/// 第一步通過後的結果
struct Primary {
    user_id: i64,
    username: String,
    method: &'static str,
    ldap_groups: Vec<String>,
    totp: bool,
    lock_key: String,
}

/// `POST /api/v1/auth/login`
pub async fn login(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let ip = peer.ip();
    if let Err(retry) = ctx.auth.failures.check(ip, Instant::now()) {
        return Err(ApiError::rate_limited(retry));
    }
    // 先扣一次來源的額度（同時送出的大量嘗試不會都在扣減之前通過檢查）；成功或與密碼無關的錯誤時退回
    ctx.auth.note_failure(ip, "登入嘗試");
    let r = login_charged(ctx, peer, req, now).await;
    if r.as_ref().err().is_none_or(|e| !counts_as_attempt(e)) {
        ctx.auth.failures.refund(ip, Instant::now());
    }
    r
}

/// 這個錯誤是否算一次猜測（密碼、驗證碼錯誤或帳號已鎖定）；本文格式錯誤、目錄服務無法使用等不算
fn counts_as_attempt(e: &ApiError) -> bool {
    matches!(
        e.code,
        "invalid_credentials"
            | "account_locked"
            | "account_disabled"
            | "invalid_mfa_code"
            | "mfa_challenge_expired"
    )
}

async fn login_charged(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let ip = peer.ip();
    let ua = user_agent(req.headers());
    let b = read_json(req, MAX_LOGIN_BODY).await?;
    let username = str_field(&b, "username").unwrap_or("").trim().to_string();
    let password = str_field(&b, "password").unwrap_or("").to_string();
    if username.is_empty()
        || username.chars().count() > 256
        || username.chars().any(char::is_control)
    {
        return Err(ApiError::param(
            "username",
            "username 必填（1～256 個字元）",
        ));
    }
    if ctx.auth.key().is_none() {
        return Err(console_key_missing());
    }
    let now_ms = now.timestamp_millis();
    let primary = match str_field(&b, "method").unwrap_or("local") {
        "local" => login_local(ctx, &username, password, ip, now_ms).await?,
        "ldap" if ctx.config.ldap.enabled => {
            login_ldap(ctx, &username, password, ip, now_ms).await?
        }
        "ldap" => {
            return Err(ApiError::param("method", "目錄服務（LDAP／AD）登入未啟用"));
        }
        _ => return Err(ApiError::param("method", "method 必須是 local 或 ldap")),
    };
    if primary.totp {
        let token = new_session_secret().map_err(|_| ApiError::internal())?;
        let expires_at = now_ms + MFA_CHALLENGE_TTL_MS;
        {
            let mut m = ctx.login.mfa.lock().unwrap_or_else(|e| e.into_inner());
            m.retain(|_, c| c.expires_at > now_ms);
            if m.len() >= MAX_MFA_CHALLENGES {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "busy",
                    "登入請求太多，請稍後再試",
                )
                .with_retry_after(5));
            }
            m.insert(
                sha256(token.as_bytes()),
                MfaChallenge {
                    user_id: primary.user_id,
                    username: primary.username.clone(),
                    method: primary.method,
                    ldap_groups: primary.ldap_groups.clone(),
                    lock_key: primary.lock_key.clone(),
                    expires_at,
                    attempts: 0,
                },
            );
        }
        return Ok(http::ok(
            now,
            json!({ "mfa_required": true, "mfa_token": token, "mfa_expires_at": ts_ms(expires_at) }),
        ));
    }
    create_session(ctx, &primary, false, ip, ua, now).await
}

async fn login_local(
    ctx: &Arc<ApiContext>,
    username: &str,
    password: String,
    ip: IpAddr,
    now_ms: i64,
) -> Result<Primary, ApiError> {
    let name = username.to_string();
    let (user, hash) =
        ctx.pool
            .read("使用者", move |c| {
                let u = us::find_user(c, &name).map_err(super::db::query_failed("使用者"))?;
                let h = match &u {
                    Some(u) => us::password_hash(c, u.user_id)
                        .map_err(super::db::query_failed("使用者"))?,
                    None => None,
                };
                Ok((u, h))
            })
            .await?;
    let lock_key = format!("local:{}", username.to_lowercase());
    if let Some(until) = locked_until(ctx, user.as_ref(), &lock_key, now_ms) {
        return Err(account_locked(until, now_ms));
    }
    let params = HashParams::from(&ctx.config.accounts);
    let h = hash.clone();
    // 參數調整過時，登入成功後以新參數重新雜湊（需要明文密碼，只在這裡短暫保留）
    let rehash_pw = hash
        .as_deref()
        .filter(|h| password::needs_rehash(h, params))
        .map(|_| password.clone());
    let pw = password;
    let ok = ctx
        .login
        .blocking(move || match &h {
            Some(h) => password::verify(&pw, h),
            None => {
                password::dummy_verify(&pw, params);
                false
            }
        })
        .await?;
    let why = match &user {
        None => "unknown_user",
        Some(u) if u.source != SOURCE_LOCAL => "directory_account",
        Some(_) if !ok => "bad_password",
        Some(u) if !u.active() => "disabled",
        Some(_) => "",
    };
    if !why.is_empty() {
        return Err(login_failure(
            ctx,
            user.as_ref(),
            &lock_key,
            username,
            "local",
            why,
            None,
            ip,
            now_ms,
        )
        .await);
    }
    let u = user.ok_or_else(ApiError::internal)?;
    if let (Some(pw), Some(old)) = (rehash_pw, hash) {
        let (ctx2, uid) = (ctx.clone(), u.user_id);
        tokio::spawn(async move {
            if let Ok(Ok(phc)) = ctx2
                .login
                .blocking(move || password::hash(&pw, params))
                .await
            {
                // 只在密碼沒有同時被變更時取代（不讓舊密碼覆蓋剛變更的密碼）
                let _ = ctx2
                    .admin
                    .write(move |c| us::rehash_password(c, uid, &old, &phc))
                    .await;
            }
        });
    }
    Ok(Primary {
        user_id: u.user_id,
        username: u.username,
        method: "local",
        ldap_groups: Vec::new(),
        totp: u.totp_enabled_at.is_some(),
        lock_key,
    })
}

/// 鎖定到何時（帳號存在時看資料庫；不存在時看記憶體中的計數）
fn locked_until(
    ctx: &ApiContext,
    user: Option<&UserRow>,
    lock_key: &str,
    now_ms: i64,
) -> Option<i64> {
    match user {
        Some(u) => u.locked_until.filter(|t| *t > now_ms),
        None => ctx.login.phantom.locked_until(lock_key, now_ms),
    }
}

/// 登入失敗：扣來源的失敗額度、累計帳號的失敗次數（達到門檻時鎖定）、寫入稽核
#[allow(clippy::too_many_arguments)]
async fn login_failure(
    ctx: &Arc<ApiContext>,
    user: Option<&UserRow>,
    lock_key: &str,
    username: &str,
    method: &str,
    why: &'static str,
    code: Option<&'static str>,
    ip: IpAddr,
    now_ms: i64,
) -> ApiError {
    let acc = &ctx.config.accounts;
    let (threshold, lock_ms) = (acc.lockout_threshold, acc.lockout_ms());
    let locked = match user {
        Some(u) => {
            let uid = u.user_id;
            // 計數寫不進去時不能當作沒失敗（否則資料庫忙碌時可以不受鎖定限制地猜密碼）
            match ctx
                .admin
                .write(move |c| us::login_failed(c, uid, threshold, lock_ms, now_ms))
                .await
            {
                Ok(l) => l,
                Err(e) => return storage_error(e),
            }
        }
        None => ctx.login.phantom.fail(lock_key, threshold, lock_ms, now_ms),
    };
    // 不存在的帳號：輸入不像帳號（例如把密碼打進帳號欄）時不保存原文
    let object = if user.is_some() || us::validate_username(username).is_ok() {
        username.to_string()
    } else {
        format!(
            "（不符合帳號格式，SHA-256 前 8 碼 {}）",
            sha256(username.as_bytes())[..4]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    };
    audit(
        ctx,
        Event {
            // 失敗的嘗試不一定是帳號本人：執行者一律 anonymous，帳號記在對象
            actor: "anonymous".to_string(),
            actor_ip: Some(ip_text(ip)),
            category: "login",
            action: "login.failure",
            object: Some(object.clone()),
            result: "failed",
            code: Some(code.unwrap_or(CODE_BAD_CREDENTIALS)),
            details: Some(json!({ "method": method, "reason": why, "locked": locked.map(ts_ms) })),
        },
        now_ms,
    )
    .await;
    match locked {
        Some(until) => {
            audit(
                ctx,
                Event {
                    actor: "system".to_string(),
                    actor_ip: Some(ip_text(ip)),
                    category: "login",
                    action: "account.lock",
                    object: Some(object),
                    result: "ok",
                    code: Some(CODE_LOCKED),
                    details: Some(json!({ "locked_until": ts_ms(until) })),
                },
                now_ms,
            )
            .await;
            account_locked(until, now_ms)
        }
        None if code == Some(CODE_BAD_MFA) => bad_mfa(),
        None => bad_credentials(),
    }
}

async fn login_ldap(
    ctx: &Arc<ApiContext>,
    username: &str,
    password: String,
    ip: IpAddr,
    now_ms: i64,
) -> Result<Primary, ApiError> {
    let name = username.to_string();
    let existing = ctx
        .pool
        .read("使用者", move |c| {
            us::find_user(c, &name).map_err(super::db::query_failed("使用者"))
        })
        .await?
        .filter(|u| u.source == SOURCE_LDAP);
    let lock_key = format!("ldap:{}", username.to_lowercase());
    if let Some(until) = locked_until(ctx, existing.as_ref(), &lock_key, now_ms) {
        return Err(account_locked(until, now_ms));
    }
    let dir = ctx.directory.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "directory_unavailable",
            "目錄服務沒有設定",
        )
        .with_product_code("KRS-E6002")
    })?;
    let result = dir.authenticate(username, &password).await;
    drop(password);
    match result {
        Ok(du) => {
            let (dname, dn, display, email) = (
                du.username.clone(),
                du.dn.clone(),
                du.display_name.clone(),
                du.email.clone(),
            );
            let id = ctx
                .admin
                .write(move |c| {
                    us::upsert_ldap_user(
                        c,
                        &dname,
                        &dn,
                        display.as_deref(),
                        email.as_deref(),
                        now_ms,
                    )
                })
                .await
                .map_err(storage_error)?;
            let user = match id {
                Some(id) => {
                    ctx.pool
                        .read("使用者", move |c| {
                            us::get_user(c, id).map_err(super::db::query_failed("使用者"))
                        })
                        .await?
                }
                None => None,
            };
            let why = match &user {
                None => "local_account_conflict",
                Some(u) if !u.active() => "disabled",
                Some(_) => "",
            };
            if !why.is_empty() {
                return Err(login_failure(
                    ctx,
                    user.as_ref(),
                    &lock_key,
                    username,
                    "ldap",
                    why,
                    None,
                    ip,
                    now_ms,
                )
                .await);
            }
            let u = user.ok_or_else(ApiError::internal)?;
            Ok(Primary {
                user_id: u.user_id,
                username: u.username,
                method: "ldap",
                ldap_groups: du.groups,
                totp: u.totp_enabled_at.is_some(),
                lock_key,
            })
        }
        Err(DirError::InvalidCredentials | DirError::UserNotFound) => Err(login_failure(
            ctx,
            existing.as_ref(),
            &lock_key,
            username,
            "ldap",
            "bad_password",
            None,
            ip,
            now_ms,
        )
        .await),
        Err(DirError::AccountDisabled) => {
            audit(
                ctx,
                Event {
                    actor: "anonymous".to_string(),
                    actor_ip: Some(ip_text(ip)),
                    category: "login",
                    action: "login.failure",
                    object: Some(username.to_string()),
                    result: "failed",
                    code: Some("KRS-E6009"),
                    details: Some(
                        json!({ "method": "ldap", "reason": "directory_account_disabled" }),
                    ),
                },
                now_ms,
            )
            .await;
            Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "account_disabled",
                "目錄服務回報這個帳號已停用或鎖定",
            )
            .with_product_code("KRS-E6009"))
        }
        Err(e) => {
            warn!("目錄服務登入失敗（{}）：{}", e.product_code(), e.detail());
            audit(
                ctx,
                Event {
                    actor: "anonymous".to_string(),
                    actor_ip: Some(ip_text(ip)),
                    category: "login",
                    action: "login.failure",
                    object: Some(username.to_string()),
                    result: "failed",
                    code: Some(e.product_code()),
                    details: Some(json!({ "method": "ldap", "reason": e.reason() })),
                },
                now_ms,
            )
            .await;
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "directory_unavailable",
                "無法使用目錄服務登入；可以改用本機帳號登入，或請管理員檢查目錄服務設定",
            )
            .with_product_code(e.product_code())
            .with_details(json!({ "reason": e.reason() })))
        }
    }
}

fn bad_mfa() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_mfa_code",
        "驗證碼不正確或已使用過",
    )
    .with_product_code(CODE_BAD_MFA)
}

/// `POST /api/v1/auth/login/mfa`
pub async fn login_mfa(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let ip = peer.ip();
    if let Err(retry) = ctx.auth.failures.check(ip, Instant::now()) {
        return Err(ApiError::rate_limited(retry));
    }
    ctx.auth.note_failure(ip, "二階段驗證嘗試");
    let r = login_mfa_charged(ctx, peer, req, now).await;
    if r.as_ref().err().is_none_or(|e| !counts_as_attempt(e)) {
        ctx.auth.failures.refund(ip, Instant::now());
    }
    r
}

async fn login_mfa_charged(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let ip = peer.ip();
    let ua = user_agent(req.headers());
    let b = read_json(req, MAX_LOGIN_BODY).await?;
    let token = str_field(&b, "mfa_token").unwrap_or("");
    let code = str_field(&b, "code").unwrap_or("").to_string();
    let key = ctx.auth.key().cloned().ok_or_else(console_key_missing)?;
    let now_ms = now.timestamp_millis();
    let k = sha256(token.as_bytes());
    let challenge = {
        let mut m = ctx.login.mfa.lock().unwrap_or_else(|e| e.into_inner());
        match m.get_mut(&k) {
            Some(c) if c.expires_at > now_ms && c.attempts < MFA_MAX_ATTEMPTS => {
                c.attempts += 1;
                Some(c.clone())
            }
            Some(_) => {
                m.remove(&k);
                None
            }
            None => None,
        }
    };
    let Some(ch) = challenge else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "mfa_challenge_expired",
            "驗證步驟已逾時或無效，請重新輸入帳號密碼",
        ));
    };
    let uid = ch.user_id;
    let (user, state) = ctx
        .pool
        .read("使用者", move |c| {
            let u = us::get_user(c, uid).map_err(super::db::query_failed("使用者"))?;
            let s = us::totp_state(c, uid).map_err(super::db::query_failed("使用者"))?;
            Ok((u, s))
        })
        .await?;
    let Some(user) = user.filter(|u| u.active()) else {
        ctx.login
            .mfa
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&k);
        return Err(bad_credentials());
    };
    if let Some(until) = user.locked_until.filter(|t| *t > now_ms) {
        ctx.login
            .mfa
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&k);
        return Err(account_locked(until, now_ms));
    }
    let trimmed = code.trim().replace([' ', '-'], "");
    let mut used_recovery = false;
    let ok =
        if trimmed.len() == totp::DIGITS as usize && trimmed.bytes().all(|b| b.is_ascii_digit()) {
            let secret = state
                .as_ref()
                .and_then(|(s, _, _)| s.as_ref())
                .and_then(|sealed| key.open(sealed, uid.to_string().as_bytes()));
            let last = state.as_ref().and_then(|(_, _, l)| *l);
            match secret.and_then(|s| totp::verify(&s, &trimmed, now_ms / 1000, last)) {
                Some(step) => ctx
                    .admin
                    .write(move |c| us::use_totp_step(c, uid, step))
                    .await
                    .map_err(storage_error)?,
                None => false,
            }
        } else if let Some(norm) = totp::normalize_recovery(&code) {
            let h = key.recovery_hash(uid, &norm);
            used_recovery = true;
            ctx.admin
                .write(move |c| us::use_recovery_code(c, uid, &h, now_ms))
                .await
                .map_err(storage_error)?
        } else {
            false
        };
    if !ok {
        let err = login_failure(
            ctx,
            Some(&user),
            &ch.lock_key,
            &ch.username,
            ch.method,
            "bad_mfa_code",
            Some(CODE_BAD_MFA),
            ip,
            now_ms,
        )
        .await;
        if err.code == "account_locked" {
            ctx.login
                .mfa
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&k);
        }
        return Err(err);
    }
    ctx.login
        .mfa
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&k);
    let primary = Primary {
        user_id: uid,
        username: ch.username,
        method: ch.method,
        ldap_groups: ch.ldap_groups,
        totp: true,
        lock_key: ch.lock_key,
    };
    create_session(ctx, &primary, used_recovery, ip, ua, now).await
}

/// 建立工作階段並回應（Set-Cookie）
async fn create_session(
    ctx: &Arc<ApiContext>,
    p: &Primary,
    used_recovery: bool,
    ip: IpAddr,
    ua: Option<String>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let key = ctx.auth.key().cloned().ok_or_else(console_key_missing)?;
    let secret = new_session_secret().map_err(|_| ApiError::internal())?;
    let hash = sha256(secret.as_bytes());
    let public_id = new_public_id().map_err(|_| ApiError::internal())?;
    let now_ms = now.timestamp_millis();
    let acc = &ctx.config.accounts;
    let (idle, expires_at) = (acc.idle_ms(), now_ms + acc.absolute_ms());
    let (uid, method, groups, ipt, pid) = (
        p.user_id,
        p.method,
        p.ldap_groups.clone(),
        ip_text(ip),
        public_id.clone(),
    );
    let ev = Event {
        actor: format!("user:{}", p.username),
        actor_ip: Some(ipt.clone()),
        category: "login",
        action: "login.success",
        object: Some(p.username.clone()),
        result: "ok",
        code: None,
        details: Some(json!({
            "method": method,
            "mfa": p.totp,
            "recovery_code_used": used_recovery,
            "session_id": public_id,
        })),
    };
    let created = ctx
        .admin
        .write(move |c| {
            in_write_tx(c, |c| {
                // 雜湊與等待期間帳號可能已被鎖定、停用或刪除：在建立工作階段的交易中再確認一次
                match us::get_user(c, uid)? {
                    Some(u) if !u.active() => return Ok(Err(None)),
                    Some(u) if u.locked(now_ms) => return Ok(Err(u.locked_until)),
                    None => return Ok(Err(None)),
                    Some(_) => {}
                }
                us::create_session(
                    c,
                    &NewSession {
                        session_hash: &hash,
                        public_id: &pid,
                        user_id: uid,
                        method,
                        ldap_groups: &groups,
                        idle_timeout_ms: idle,
                        expires_at,
                        ip: &ipt,
                        user_agent: ua.as_deref(),
                    },
                    now_ms,
                )?;
                us::login_succeeded(c, uid, &ipt, now_ms)?;
                ev.append(c, now_ms)?;
                Ok(Ok(us::get_user(c, uid)?))
            })
        })
        .await
        .map_err(storage_error)?;
    let user = match created {
        Ok(u) => u.ok_or_else(ApiError::internal)?,
        Err(Some(until)) => return Err(account_locked(until, now_ms)),
        Err(None) => return Err(bad_credentials()),
    };
    ctx.login.phantom.succeed(&p.lock_key);
    let body = json!({
        "data": {
            "mfa_required": false,
            "user": user.json(now_ms),
            "must_change_password": user.must_change_password,
            "csrf_token": key.csrf_token(&secret),
            "session": {
                "session_id": public_id,
                "expires_at": ts_ms(expires_at),
                "idle_timeout_secs": idle / 1000,
            },
        }
    });
    let resp = http::json(StatusCode::OK, now, body);
    Ok(with_cookie(
        resp,
        &session_cookie(&secret, (expires_at - now_ms) / 1000),
    ))
}

/// `POST /api/v1/auth/logout`
pub async fn logout(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let (Some(hash), Some((_, username))) = (p.session_hash().copied(), p.session_user()) else {
        return Err(not_a_session());
    };
    let now_ms = now.timestamp_millis();
    let actor = p.actor();
    let ev = Event {
        actor: actor.clone(),
        actor_ip: Some(ip_text(peer.ip())),
        category: "login",
        action: "logout",
        object: Some(username.to_string()),
        result: "ok",
        ..Event::default()
    };
    ctx.admin
        .write(move |c| {
            in_write_tx(c, |c| {
                if us::end_session(c, &hash, "logout", &actor, now_ms)? {
                    ev.append(c, now_ms)?;
                }
                Ok(())
            })
        })
        .await
        .map_err(storage_error)?;
    Ok(with_cookie(
        http::ok(now, json!({ "logged_out": true })),
        &clear_cookie(),
    ))
}

/// `/api/v1/me/…`
pub async fn me(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    rest: &[&str],
    req: Request<Incoming>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let method = req.method().clone();
    if !matches!(
        method,
        hyper::Method::GET | hyper::Method::HEAD | hyper::Method::POST
    ) {
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "這個端點不接受這個方法",
        ));
    }
    let is_get = method != hyper::Method::POST;
    match (is_get, rest) {
        (true, []) => Ok(http::ok(now, p.me_json())),
        (true, ["sessions"]) => my_sessions(ctx, p, now).await,
        (false, ["sessions", "revoke-others"]) => revoke_others(ctx, p, peer, now).await,
        (false, ["sessions", id, "revoke"]) => revoke_one(ctx, p, peer, id, now).await,
        (false, ["password"]) => {
            let b = read_json(req, MAX_LOGIN_BODY).await?;
            change_password(ctx, p, peer, b, now).await
        }
        (false, ["totp", action]) => {
            let b = read_json(req, MAX_LOGIN_BODY).await?;
            totp_action(ctx, p, peer, action, b, now).await
        }
        _ => Err(ApiError::not_found()),
    }
}

async fn my_sessions(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let Some((uid, _)) = p.session_user() else {
        return Err(not_a_session());
    };
    let now_ms = now.timestamp_millis();
    let current = match &p.kind {
        super::auth::PrincipalKind::Session { session_id, .. } => session_id.clone(),
        _ => String::new(),
    };
    let list = ctx
        .pool
        .read("工作階段", move |c| {
            us::user_sessions(c, uid, true, now_ms).map_err(super::db::query_failed("工作階段"))
        })
        .await?;
    Ok(http::ok(
        now,
        json!({ "sessions": list.iter().map(|s| s.json(now_ms, s.public_id == current)).collect::<Vec<_>>() }),
    ))
}

/// 結束其他所有工作階段、保留目前這個（S23、S16 對象是自己；D25）
async fn revoke_others(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let (Some(hash), Some((uid, username))) = (p.session_hash().copied(), p.session_user()) else {
        return Err(not_a_session());
    };
    let now_ms = now.timestamp_millis();
    let actor = p.actor();
    let mut ev = Event {
        actor: actor.clone(),
        actor_ip: Some(ip_text(peer.ip())),
        category: "user",
        action: "session.revoke_others",
        object: Some(username.to_string()),
        result: "ok",
        ..Event::default()
    };
    let n = ctx
        .admin
        .write(move |c| {
            in_write_tx(c, |c| {
                let n =
                    us::end_user_sessions(c, uid, Some(&hash), "revoked_by_user", &actor, now_ms)?;
                ev.details = Some(json!({ "ended": n, "kept_current": true }));
                ev.append(c, now_ms)?;
                Ok(n)
            })
        })
        .await
        .map_err(storage_error)?;
    Ok(http::ok(now, json!({ "ended": n, "kept_current": true })))
}

async fn revoke_one(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    id: &str,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let Some((uid, username)) = p.session_user() else {
        return Err(not_a_session());
    };
    let current = matches!(&p.kind, super::auth::PrincipalKind::Session { session_id, .. } if session_id == id);
    let now_ms = now.timestamp_millis();
    let (actor, sid) = (p.actor(), id.to_string());
    let ev = Event {
        actor: actor.clone(),
        actor_ip: Some(ip_text(peer.ip())),
        category: "user",
        action: "session.revoke",
        object: Some(username.to_string()),
        result: "ok",
        details: Some(json!({ "session_id": id, "current": current })),
        ..Event::default()
    };
    let ended = ctx
        .admin
        .write(move |c| {
            in_write_tx(c, |c| {
                let ended =
                    us::end_session_by_public_id(c, uid, &sid, "revoked_by_user", &actor, now_ms)?;
                if ended {
                    ev.append(c, now_ms)?;
                }
                Ok(ended)
            })
        })
        .await
        .map_err(storage_error)?;
    if !ended {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "找不到這個工作階段（可能已結束）",
        ));
    }
    let resp = http::ok(now, json!({ "ended": 1, "current": current }));
    Ok(if current {
        with_cookie(resp, &clear_cookie())
    } else {
        resp
    })
}

/// 驗證目前使用者的密碼（變更密碼、TOTP 設定前）；失敗時與登入失敗同樣計數
async fn verify_current_password(
    ctx: &Arc<ApiContext>,
    uid: i64,
    username: &str,
    password: String,
    ip: IpAddr,
    now_ms: i64,
) -> Result<UserRow, ApiError> {
    let (user, hash) = ctx
        .pool
        .read("使用者", move |c| {
            let u = us::get_user(c, uid).map_err(super::db::query_failed("使用者"))?;
            let h = us::password_hash(c, uid).map_err(super::db::query_failed("使用者"))?;
            Ok((u, h))
        })
        .await?;
    let user = user.ok_or_else(ApiError::session_expired)?;
    if user.source != SOURCE_LOCAL {
        return Err(ApiError::conflict(
            "directory_account",
            "目錄服務帳號的密碼與二階段驗證由目錄服務（AD）管理",
        ));
    }
    if let Some(until) = user.locked_until.filter(|t| *t > now_ms) {
        return Err(account_locked(until, now_ms));
    }
    let ok = ctx
        .login
        .blocking(move || {
            hash.as_deref()
                .is_some_and(|h| password::verify(&password, h))
        })
        .await?;
    if !ok {
        ctx.auth.note_failure(ip, "目前的密碼不正確");
        let err = login_failure(
            ctx,
            Some(&user),
            &format!("local:{}", username.to_lowercase()),
            username,
            "local",
            "bad_current_password",
            None,
            ip,
            now_ms,
        )
        .await;
        return Err(if err.code == "invalid_credentials" {
            ApiError::new(
                StatusCode::FORBIDDEN,
                "invalid_credentials",
                "目前的密碼不正確",
            )
            .with_product_code(CODE_BAD_CREDENTIALS)
        } else {
            err
        });
    }
    Ok(user)
}

/// `POST /api/v1/me/password`：變更自己的密碼；其他工作階段結束、保留目前這個
async fn change_password(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let (Some(hash), Some((uid, username))) = (p.session_hash().copied(), p.session_user()) else {
        return Err(not_a_session());
    };
    let current = str_field(&b, "current_password").unwrap_or("").to_string();
    let new = str_field(&b, "new_password").unwrap_or("").to_string();
    password::check_policy(&new, username, ctx.config.accounts.password_min_length).map_err(
        |e| {
            ApiError::param("new_password", e)
                .with_details(json!({"parameter": "new_password", "reason": "password_policy"}))
        },
    )?;
    if new == current {
        return Err(ApiError::param(
            "new_password",
            "新密碼不可與目前的密碼相同",
        ));
    }
    let now_ms = now.timestamp_millis();
    verify_current_password(ctx, uid, username, current, peer.ip(), now_ms).await?;
    let params = HashParams::from(&ctx.config.accounts);
    let phc = ctx
        .login
        .blocking(move || password::hash(&new, params))
        .await?
        .map_err(|e| {
            error!("無法雜湊密碼：{e}");
            ApiError::internal()
        })?;
    let actor = p.actor();
    let mut ev = Event {
        actor: actor.clone(),
        actor_ip: Some(ip_text(peer.ip())),
        category: "user",
        action: "password.change",
        object: Some(username.to_string()),
        result: "ok",
        ..Event::default()
    };
    let ended = ctx
        .admin
        .write(move |c| {
            in_write_tx(c, |c| {
                us::set_password(c, uid, &phc, false, now_ms)?;
                let n =
                    us::end_user_sessions(c, uid, Some(&hash), "password_changed", &actor, now_ms)?;
                ev.details = Some(json!({ "other_sessions_ended": n }));
                ev.append(c, now_ms)?;
                Ok(n)
            })
        })
        .await
        .map_err(storage_error)?;
    Ok(http::ok(
        now,
        json!({ "changed": true, "other_sessions_ended": ended }),
    ))
}

/// 驗證 TOTP（或一組復原碼）；通過時記下時間步或用掉復原碼
async fn second_factor_ok(
    ctx: &Arc<ApiContext>,
    key: &crate::users::secrets::ConsoleKey,
    uid: i64,
    code: &str,
    now_ms: i64,
) -> Result<bool, ApiError> {
    let state = ctx
        .pool
        .read("使用者", move |c| {
            us::totp_state(c, uid).map_err(super::db::query_failed("使用者"))
        })
        .await?;
    let trimmed = code.trim().replace([' ', '-'], "");
    if trimmed.len() == totp::DIGITS as usize && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        let secret = state
            .as_ref()
            .and_then(|(s, _, _)| s.as_ref())
            .and_then(|sealed| key.open(sealed, uid.to_string().as_bytes()));
        let last = state.as_ref().and_then(|(_, _, l)| *l);
        match secret.and_then(|s| totp::verify(&s, &trimmed, now_ms / 1000, last)) {
            Some(step) => ctx
                .admin
                .write(move |c| us::use_totp_step(c, uid, step))
                .await
                .map_err(storage_error),
            None => Ok(false),
        }
    } else if let Some(norm) = totp::normalize_recovery(code) {
        let h = key.recovery_hash(uid, &norm);
        ctx.admin
            .write(move |c| us::use_recovery_code(c, uid, &h, now_ms))
            .await
            .map_err(storage_error)
    } else {
        Ok(false)
    }
}

/// `POST /api/v1/me/totp/{setup|enable|disable}`
async fn totp_action(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    action: &str,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let Some((uid, username)) = p.session_user() else {
        return Err(not_a_session());
    };
    let key = ctx.auth.key().cloned().ok_or_else(console_key_missing)?;
    let now_ms = now.timestamp_millis();
    let ip = peer.ip();
    let actor = p.actor();
    match action {
        "setup" => {
            let pw = str_field(&b, "password").unwrap_or("").to_string();
            let user = verify_current_password(ctx, uid, username, pw, ip, now_ms).await?;
            if user.totp_enabled_at.is_some() {
                return Err(ApiError::conflict(
                    "mfa_already_enabled",
                    "已啟用二階段驗證；要更換時先停用",
                ));
            }
            let secret = totp::generate_secret().map_err(|_| ApiError::internal())?;
            let sealed = key
                .seal(&secret, uid.to_string().as_bytes())
                .map_err(|_| ApiError::internal())?;
            ctx.admin
                .write(move |c| us::set_totp_pending(c, uid, &sealed))
                .await
                .map_err(storage_error)?;
            let b32 = totp::base32(&secret);
            Ok(http::ok(
                now,
                json!({
                    "secret": b32,
                    "otpauth_uri": totp::otpauth_uri(&ctx.config.accounts.mfa_issuer, username, &b32),
                    "digits": totp::DIGITS,
                    "period_secs": totp::STEP_SECS,
                }),
            ))
        }
        "enable" => {
            let code = str_field(&b, "code").unwrap_or("").to_string();
            let state = ctx
                .pool
                .read("使用者", move |c| {
                    us::totp_state(c, uid).map_err(super::db::query_failed("使用者"))
                })
                .await?;
            let pending = state
                .and_then(|(_, p, _)| p)
                .and_then(|sealed| key.open(&sealed, uid.to_string().as_bytes()))
                .ok_or_else(|| {
                    ApiError::conflict("mfa_not_pending", "請先以 /api/v1/me/totp/setup 產生秘密")
                })?;
            let Some(step) = totp::verify(&pending, &code, now_ms / 1000, None) else {
                ctx.auth.note_failure(ip, "TOTP 綁定的驗證碼不正確");
                return Err(bad_mfa());
            };
            let codes = totp::generate_recovery_codes().map_err(|_| ApiError::internal())?;
            let hashes: Vec<Vec<u8>> = codes.iter().map(|c| key.recovery_hash(uid, c)).collect();
            let ev = Event {
                actor: actor.clone(),
                actor_ip: Some(ip_text(ip)),
                category: "user",
                action: "mfa.enable",
                object: Some(username.to_string()),
                result: "ok",
                ..Event::default()
            };
            let ok = ctx
                .admin
                .write(move |c| {
                    in_write_tx(c, |c| {
                        let ok = us::enable_totp(c, uid, step, &hashes, now_ms)?;
                        if ok {
                            ev.append(c, now_ms)?;
                        }
                        Ok(ok)
                    })
                })
                .await
                .map_err(storage_error)?;
            if !ok {
                return Err(ApiError::conflict(
                    "mfa_not_pending",
                    "綁定已失效，請重新產生秘密",
                ));
            }
            Ok(http::ok(
                now,
                json!({ "enabled": true, "recovery_codes": codes }),
            ))
        }
        "disable" => {
            let pw = str_field(&b, "password").unwrap_or("").to_string();
            let code = str_field(&b, "code").unwrap_or("").to_string();
            verify_current_password(ctx, uid, username, pw, ip, now_ms).await?;
            // 停用也需要目前的驗證碼（或一組復原碼）：只拿到密碼的人不能關掉二階段驗證
            if !second_factor_ok(ctx, &key, uid, &code, now_ms).await? {
                ctx.auth.note_failure(ip, "停用 TOTP 的驗證碼不正確");
                return Err(bad_mfa());
            }
            let ev = Event {
                actor,
                actor_ip: Some(ip_text(ip)),
                category: "user",
                action: "mfa.disable",
                object: Some(username.to_string()),
                result: "ok",
                ..Event::default()
            };
            let was = ctx
                .admin
                .write(move |c| {
                    in_write_tx(c, |c| {
                        let was = us::clear_totp(c, uid, now_ms)?;
                        if was {
                            ev.append(c, now_ms)?;
                        }
                        Ok(was)
                    })
                })
                .await
                .map_err(storage_error)?;
            Ok(http::ok(now, json!({ "disabled": was })))
        }
        _ => Err(ApiError::not_found()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_have_the_required_attributes() {
        let c = session_cookie("abc", 3600);
        assert!(c.starts_with("__Host-kairos_session=abc;"));
        for attr in [
            "Path=/",
            "Secure",
            "HttpOnly",
            "SameSite=Strict",
            "Max-Age=3600",
        ] {
            assert!(c.contains(attr), "{c}");
        }
        assert!(!c.contains("Domain"));
        assert!(clear_cookie().contains("Max-Age=0"));
        let e = account_locked(90_000, 0);
        assert_eq!(e.product_code, Some("KRS-E6003"));
        assert_eq!(e.retry_after, Some(90));
        assert_eq!(bad_credentials().product_code, Some("KRS-E6001"));
        assert_eq!(bad_mfa().product_code, Some("KRS-E6012"));
    }
}
