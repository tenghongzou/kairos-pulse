//! 使用者與角色（S16）、目錄服務的群組對應（S17）、API token（S21「整合」）的管理端點
//! （`kairos/API.md` 第 12.5～12.7 節）。
//!
//! 每個寫入都在同一個交易中寫入稽核雜湊鏈；防呆（設計包 S16）：不可移除或停用最後一位全站管理員、
//! 不可停用或刪除自己；對他人「結束所有工作階段」，對象是自己時回 409 並指向
//! `POST /api/v1/me/sessions/revoke-others`（D25）。

use chrono::{DateTime, Utc};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;

use super::ApiContext;
use super::account::storage_error;
use super::auth::Principal;
use super::db::query_failed;
use super::http::{self, ApiBody, ApiError, Query, read_json, str_field, ts_ms};
use super::tokens::{self, Revoked, TokenGrant};
use crate::audit::{Event, in_write_tx};
use crate::rbac::{Assignment, Permission as P, Role};
use crate::users::password::{self, HashParams};
use crate::users::store::{
    self as us, NewUser, SOURCE_LOCAL, SUBJECT_LDAP_GROUP, SUBJECT_USER, UserRow,
};

/// 寫入本文的上限
pub const MAX_BODY: usize = 64 * 1024;
/// API token 的預設與上限（天；`cli-style.md` §11.6）
pub const TOKEN_DEFAULT_DAYS: i64 = 90;
pub const TOKEN_MAX_DAYS: i64 = 3650;

fn user_not_found(name: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "user_not_found",
        format!("找不到使用者 {name}"),
    )
}

fn last_admin() -> ApiError {
    ApiError::conflict(
        "last_admin",
        "這是最後一位全站管理員：不能移除管理員角色、停用或刪除（至少需要一位）",
    )
}

/// 交易中的商業規則錯誤（回到 async 之後轉成 [`ApiError`]）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refused {
    LastAdmin,
    Exists,
    NotLocal,
}

fn refused(r: Refused, name: &str) -> ApiError {
    match r {
        Refused::LastAdmin => last_admin(),
        Refused::Exists => ApiError::conflict("user_exists", format!("使用者 {name} 已存在")),
        Refused::NotLocal => ApiError::conflict(
            "directory_account",
            "目錄服務帳號的密碼與二階段驗證由目錄服務（AD）管理",
        ),
    }
}

/// 失去全站管理員之後，還有沒有其他啟用中的全站管理員
fn leaves_no_admin(conn: &Connection, uid: i64) -> rusqlite::Result<bool> {
    let admins = us::active_global_admins(conn)?;
    Ok(admins.contains(&uid) && admins.len() == 1)
}

fn is_global_admin(list: &[Assignment]) -> bool {
    list.iter()
        .any(|a| a.role == Role::Admin && a.scope == crate::rbac::Scope::Global)
}

/// 本文中的角色指派：`["admin", "operator@group:B廠產線"]`
fn parse_roles(b: &Value) -> Result<Option<Vec<Assignment>>, ApiError> {
    let Some(list) = http::str_list(b, "roles")? else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for r in list {
        let a = Assignment::parse(&r).map_err(|e| ApiError::param("roles", e))?;
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out.sort();
    Ok(Some(out))
}

fn roles_json(list: &[Assignment]) -> Value {
    json!(list.iter().map(Assignment::json).collect::<Vec<_>>())
}

fn ip(peer: SocketAddr) -> String {
    peer.ip().to_canonical().to_string()
}

/// 處理 `/api/v1/users…`、`/api/v1/roles`、`/api/v1/directory…`、`/api/v1/api-tokens…`
pub async fn handle(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    rest: &[&str],
    req: Request<Incoming>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let method = req.method().clone();
    let query = Query::parse(req.uri().query())?;
    let get = method == Method::GET || method == Method::HEAD;
    let post = method == Method::POST || method == Method::PUT;
    if !get && !post {
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "這個端點不接受這個方法",
        ));
    }
    let body = if post {
        read_json(req, MAX_BODY).await?
    } else {
        Value::Null
    };
    let put = method == Method::PUT;
    match (get, put, rest) {
        (true, _, ["users"]) => {
            p.require(P::UsersRead)?;
            list_users(ctx, &query, now).await
        }
        (false, false, ["users"]) => {
            p.require(P::UsersWrite)?;
            create_user(ctx, p, peer, body, now).await
        }
        (true, _, ["users", name]) => {
            p.require(P::UsersRead)?;
            get_user(ctx, name, now).await
        }
        (false, true, ["users", name]) => {
            p.require(P::UsersWrite)?;
            update_profile(ctx, p, peer, name, body, now).await
        }
        (false, true, ["users", name, "roles"]) => {
            p.require(P::UsersWrite)?;
            set_roles(ctx, p, peer, name, body, now).await
        }
        (
            false,
            false,
            [
                "users",
                name,
                action @ ("disable" | "enable" | "delete" | "unlock"),
            ],
        ) => {
            p.require(P::UsersWrite)?;
            state_action(ctx, p, peer, name, action, body, now).await
        }
        (false, false, ["users", name, "password"]) => {
            p.require(P::UsersWrite)?;
            reset_password(ctx, p, peer, name, body, now).await
        }
        (false, false, ["users", name, "mfa", "reset"]) => {
            p.require(P::UsersWrite)?;
            reset_mfa(ctx, p, peer, name, body, now).await
        }
        (true, _, ["users", name, "sessions"]) => {
            p.require(P::UsersRead)?;
            user_sessions(ctx, name, now).await
        }
        (false, false, ["users", name, "sessions", "revoke-all"]) => {
            p.require(P::SessionsRevoke)?;
            revoke_all(ctx, p, peer, name, body, now).await
        }
        (true, _, ["roles"]) => {
            p.require(P::UsersRead)?;
            roles(ctx, now).await
        }
        (true, _, ["directory"]) => {
            p.require(P::DirectoryRead)?;
            directory(ctx, now).await
        }
        (false, true, ["directory", "mappings"]) => {
            p.require(P::DirectoryWrite)?;
            set_mappings(ctx, p, peer, body, now).await
        }
        (false, false, ["directory", "test"]) => {
            p.require(P::DirectoryWrite)?;
            directory_test(ctx, p, peer, body, now).await
        }
        (true, _, ["api-tokens"]) => {
            p.require(P::ApiTokensRead)?;
            list_tokens(ctx, &query, now).await
        }
        (false, false, ["api-tokens"]) => {
            p.require(P::ApiTokensWrite)?;
            create_token(ctx, p, peer, body, now).await
        }
        (false, false, ["api-tokens", id, "revoke"]) => {
            p.require(P::ApiTokensWrite)?;
            revoke_token(ctx, p, peer, id, body, now).await
        }
        _ => Err(ApiError::not_found()),
    }
}

async fn find(ctx: &Arc<ApiContext>, name: &str) -> Result<UserRow, ApiError> {
    let n = name.to_string();
    ctx.pool
        .read("使用者", move |c| {
            us::find_user(c, &n).map_err(query_failed("使用者"))
        })
        .await?
        .ok_or_else(|| user_not_found(name))
}

async fn list_users(
    ctx: &Arc<ApiContext>,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(&["source", "status", "role", "q"], &[])?;
    let now_ms = now.timestamp_millis();
    let rows = ctx
        .pool
        .read("使用者", move |c| {
            let users = us::list_users(c).map_err(query_failed("使用者"))?;
            let mut out = Vec::new();
            for u in users {
                let roles = us::user_assignments(c, u.user_id).map_err(query_failed("使用者"))?;
                let sessions = us::user_sessions(c, u.user_id, true, now_ms)
                    .map_err(query_failed("使用者"))?
                    .len();
                out.push((u, roles, sessions));
            }
            Ok(out)
        })
        .await?;
    let source = q.get("source");
    let status = q.get("status");
    let role = q.get("role").and_then(Role::parse);
    let text = q.get("q").map(str::to_lowercase);
    let data: Vec<Value> = rows
        .iter()
        .filter(|(u, _, _)| source.is_none_or(|s| u.source == s))
        .filter(|(u, _, _)| status.is_none_or(|s| u.status(now_ms) == s))
        .filter(|(_, r, _)| role.is_none_or(|x| r.iter().any(|a| a.role == x)))
        .filter(|(u, _, _)| {
            text.as_deref().is_none_or(|t| {
                u.username.to_lowercase().contains(t)
                    || u.display_name
                        .as_deref()
                        .is_some_and(|d| d.to_lowercase().contains(t))
            })
        })
        .map(|(u, r, n)| {
            let mut v = u.json(now_ms);
            v["roles"] = roles_json(r);
            v["active_sessions"] = json!(n);
            v
        })
        .collect();
    Ok(http::ok(now, json!({ "users": data })))
}

async fn get_user(
    ctx: &Arc<ApiContext>,
    name: &str,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let u = find(ctx, name).await?;
    let now_ms = now.timestamp_millis();
    let uid = u.user_id;
    let (roles, sessions, tokens_owned) = ctx
        .pool
        .read("使用者", move |c| {
            let r = us::user_assignments(c, uid).map_err(query_failed("使用者"))?;
            let s = us::user_sessions(c, uid, true, now_ms).map_err(query_failed("使用者"))?;
            let t = tokens::list(c)
                .map_err(query_failed("使用者"))?
                .into_iter()
                .filter(|t| t.owner_user_id == Some(uid))
                .collect::<Vec<_>>();
            Ok((r, s, t))
        })
        .await?;
    let mut v = u.json(now_ms);
    v["roles"] = roles_json(&roles);
    v["permissions"] = json!(crate::rbac::Grants::from_assignments(&roles).global.names());
    v["sessions"] = json!(
        sessions
            .iter()
            .map(|s| s.json(now_ms, false))
            .collect::<Vec<_>>()
    );
    v["api_tokens"] = json!(tokens_owned
        .iter()
        .map(|t| json!({"token_id": t.token_id, "name": t.name, "state": t.state(now_ms).as_str()}))
        .collect::<Vec<_>>());
    Ok(http::ok(now, v))
}

/// 新密碼：本文的 `password`（檢查政策），或沒有時產生一次性的密碼（回應中只出現這一次）
fn new_password(ctx: &ApiContext, b: &Value, username: &str) -> Result<(String, bool), ApiError> {
    match str_field(b, "password") {
        Some(pw) => {
            password::check_policy(pw, username, ctx.config.accounts.password_min_length)
                .map_err(|e| ApiError::param("password", e))?;
            Ok((pw.to_string(), false))
        }
        None => password::generate()
            .map(|p| (p, true))
            .map_err(|_| ApiError::internal()),
    }
}

async fn create_user(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let username = str_field(&b, "username").unwrap_or("").trim().to_string();
    us::validate_username(&username).map_err(|e| ApiError::param("username", e))?;
    let display = str_field(&b, "display_name").map(str::to_string);
    let email = str_field(&b, "email").map(str::to_string);
    for (k, v) in [("display_name", &display), ("email", &email)] {
        if let Some(v) = v {
            us::validate_text(k, v).map_err(|e| ApiError::param(k, e))?;
        }
    }
    let roles = parse_roles(&b)?.unwrap_or_default();
    let (pw, generated) = new_password(ctx, &b, &username)?;
    let params = HashParams::from(&ctx.config.accounts);
    let pw2 = pw.clone();
    let phc = ctx
        .login
        .blocking(move || password::hash(&pw2, params))
        .await?
        .map_err(|_| ApiError::internal())?;
    let now_ms = now.timestamp_millis();
    let actor = p.actor();
    let (name, roles2) = (username.clone(), roles.clone());
    let ev = Event {
        actor: actor.clone(),
        actor_ip: Some(ip(peer)),
        category: "user",
        action: "user.create",
        object: Some(username.clone()),
        result: "ok",
        details: Some(json!({
            "display_name": display,
            "email": email,
            "roles": roles.iter().map(Assignment::as_text).collect::<Vec<_>>(),
            "password": if generated { "generated" } else { "provided" },
        })),
        ..Event::default()
    };
    let r = ctx
        .admin
        .write(move |c| {
            in_write_tx(c, |c| {
                let Some(uid) = us::create_user(
                    c,
                    &NewUser {
                        username: &name,
                        display_name: display.as_deref(),
                        email: email.as_deref(),
                        source: SOURCE_LOCAL,
                        password_hash: Some(&phc),
                        // 管理員設定或系統產生的密碼：第一次登入時必須變更
                        must_change_password: true,
                        ..NewUser::default()
                    },
                    &actor,
                    now_ms,
                )?
                else {
                    return Ok(Err(Refused::Exists));
                };
                for a in &roles2 {
                    us::add_assignment(c, SUBJECT_USER, &uid.to_string(), a, &actor, now_ms)?;
                }
                ev.append(c, now_ms)?;
                Ok(Ok(us::get_user(c, uid)?))
            })
        })
        .await
        .map_err(storage_error)?
        .map_err(|r| refused(r, &username))?
        .ok_or_else(ApiError::internal)?;
    let mut data = r.json(now_ms);
    data["roles"] = roles_json(&roles);
    if generated {
        data["initial_password"] = json!(pw);
    }
    Ok(http::json(
        StatusCode::CREATED,
        now,
        json!({ "data": data }),
    ))
}

async fn update_profile(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    name: &str,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let u = find(ctx, name).await?;
    let display = match b.get("display_name") {
        Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| ApiError::param("display_name", "必須是字串"))?
                .to_string(),
        ),
        None => u.display_name.clone(),
    };
    let email = match b.get("email") {
        Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| ApiError::param("email", "必須是字串"))?
                .to_string(),
        ),
        None => u.email.clone(),
    };
    for (k, v) in [("display_name", &display), ("email", &email)] {
        if let Some(v) = v {
            us::validate_text(k, v).map_err(|e| ApiError::param(k, e))?;
        }
    }
    let now_ms = now.timestamp_millis();
    let ev = Event {
        actor: p.actor(),
        actor_ip: Some(ip(peer)),
        category: "user",
        action: "user.update",
        object: Some(u.username.clone()),
        result: "ok",
        details: Some(json!({
            "before": {"display_name": u.display_name, "email": u.email},
            "after": {"display_name": display, "email": email},
        })),
        ..Event::default()
    };
    let uid = u.user_id;
    let row = ctx
        .admin
        .write(move |c| {
            in_write_tx(c, |c| {
                us::set_profile(c, uid, display.as_deref(), email.as_deref(), now_ms)?;
                ev.append(c, now_ms)?;
                us::get_user(c, uid)
            })
        })
        .await
        .map_err(storage_error)?
        .ok_or_else(|| user_not_found(name))?;
    Ok(http::ok(now, row.json(now_ms)))
}

/// `PUT /api/v1/users/{name}/roles`：以新的一組取代（L1，回應帶權限差異）
async fn set_roles(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    name: &str,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let roles =
        parse_roles(&b)?.ok_or_else(|| ApiError::param("roles", "roles 必填（可為空陣列）"))?;
    let reason = str_field(&b, "reason").map(str::to_string);
    let u = find(ctx, name).await?;
    let (uid, actor, now_ms) = (u.user_id, p.actor(), now.timestamp_millis());
    let mut ev = Event {
        actor: actor.clone(),
        actor_ip: Some(ip(peer)),
        category: "user",
        action: "role.change",
        object: Some(u.username.clone()),
        result: "ok",
        ..Event::default()
    };
    let new = roles.clone();
    let (added, removed, before) = ctx
        .admin
        .write(move |c| {
            in_write_tx(c, |c| {
                let before = us::user_assignments(c, uid)?;
                if is_global_admin(&before) && !is_global_admin(&new) && leaves_no_admin(c, uid)? {
                    return Ok(Err(Refused::LastAdmin));
                }
                let (added, removed) =
                    us::replace_assignments(c, SUBJECT_USER, &uid.to_string(), &new, &actor, now_ms)?;
                if !added.is_empty() || !removed.is_empty() {
                    let gain = crate::rbac::Grants::from_assignments(&new).global;
                    let had = crate::rbac::Grants::from_assignments(&before).global;
                    ev.details = Some(json!({
                        "added": added.iter().map(Assignment::as_text).collect::<Vec<_>>(),
                        "removed": removed.iter().map(Assignment::as_text).collect::<Vec<_>>(),
                        "permissions_added": gain.list().into_iter().filter(|x| !had.contains(*x)).map(P::name).collect::<Vec<_>>(),
                        "permissions_removed": had.list().into_iter().filter(|x| !gain.contains(*x)).map(P::name).collect::<Vec<_>>(),
                        "reason": reason,
                    }));
                    ev.append(c, now_ms)?;
                }
                Ok(Ok((added, removed, before)))
            })
        })
        .await
        .map_err(storage_error)?
        .map_err(|r| refused(r, name))?;
    let gain = crate::rbac::Grants::from_assignments(&roles).global;
    let had = crate::rbac::Grants::from_assignments(&before).global;
    Ok(http::ok(
        now,
        json!({
            "roles": roles_json(&roles),
            "added": roles_json(&added),
            "removed": roles_json(&removed),
            "permissions_added": gain.list().into_iter().filter(|x| !had.contains(*x)).map(P::name).collect::<Vec<_>>(),
            "permissions_removed": had.list().into_iter().filter(|x| !gain.contains(*x)).map(P::name).collect::<Vec<_>>(),
        }),
    ))
}

/// 停用（L1）、啟用、刪除（L2：`confirm` 必須等於帳號）、解除鎖定
async fn state_action(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    name: &str,
    action: &str,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let u = find(ctx, name).await?;
    let self_target = p.acting_user_id() == Some(u.user_id);
    if self_target && matches!(action, "disable" | "delete") {
        return Err(ApiError::conflict(
            "cannot_target_self",
            "不能停用或刪除自己的帳號",
        ));
    }
    if action == "delete" {
        let confirm = str_field(&b, "confirm").unwrap_or("");
        if !confirm.eq_ignore_ascii_case(&u.username) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "confirmation_required",
                "刪除使用者需要 confirm（輸入帳號，L2）",
            ));
        }
    }
    let reason = str_field(&b, "reason").map(str::to_string);
    let (uid, actor, now_ms) = (u.user_id, p.actor(), now.timestamp_millis());
    let act = action.to_string();
    let mut ev = Event {
        actor: actor.clone(),
        actor_ip: Some(ip(peer)),
        category: "user",
        action: match action {
            "disable" => "user.disable",
            "enable" => "user.enable",
            "delete" => "user.delete",
            _ => "user.unlock",
        },
        object: Some(u.username.clone()),
        result: "ok",
        ..Event::default()
    };
    let changed = ctx
        .admin
        .write(move |c| {
            in_write_tx(c, |c| {
                if matches!(act.as_str(), "disable" | "delete") && leaves_no_admin(c, uid)? {
                    return Ok(Err(Refused::LastAdmin));
                }
                let (changed, sessions) = match act.as_str() {
                    "disable" => {
                        let ch = us::set_state(c, uid, false, &actor, reason.as_deref(), now_ms)?;
                        let n =
                            us::end_user_sessions(c, uid, None, "user_disabled", &actor, now_ms)?;
                        (ch, n)
                    }
                    "enable" => (us::set_state(c, uid, true, &actor, None, now_ms)?, 0),
                    "delete" => {
                        let n = us::user_sessions(c, uid, true, now_ms)?.len();
                        (us::delete_user(c, uid, &actor, now_ms)?, n)
                    }
                    _ => (us::unlock(c, uid)?, 0),
                };
                if changed {
                    ev.details = Some(json!({ "reason": reason, "sessions_ended": sessions }));
                    ev.append(c, now_ms)?;
                }
                Ok(Ok((changed, sessions)))
            })
        })
        .await
        .map_err(storage_error)?
        .map_err(|r| refused(r, name))?;
    Ok(http::ok(
        now,
        json!({ "username": u.username, "action": action, "changed": changed.0, "sessions_ended": changed.1 }),
    ))
}

/// 管理員重設密碼：本文的 `password` 或產生一次性的密碼；所有工作階段結束、下次登入必須變更
async fn reset_password(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    name: &str,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let u = find(ctx, name).await?;
    if u.source != SOURCE_LOCAL {
        return Err(refused(Refused::NotLocal, name));
    }
    let (pw, generated) = new_password(ctx, &b, &u.username)?;
    let params = HashParams::from(&ctx.config.accounts);
    let pw2 = pw.clone();
    let phc = ctx
        .login
        .blocking(move || password::hash(&pw2, params))
        .await?
        .map_err(|_| ApiError::internal())?;
    let (uid, actor, now_ms) = (u.user_id, p.actor(), now.timestamp_millis());
    let mut ev = Event {
        actor: actor.clone(),
        actor_ip: Some(ip(peer)),
        category: "user",
        action: "password.reset",
        object: Some(u.username.clone()),
        result: "ok",
        ..Event::default()
    };
    let ended = ctx
        .admin
        .write(move |c| {
            in_write_tx(c, |c| {
                us::set_password(c, uid, &phc, true, now_ms)?;
                let n = us::end_user_sessions(c, uid, None, "password_reset", &actor, now_ms)?;
                ev.details = Some(json!({ "sessions_ended": n, "password": if generated { "generated" } else { "provided" } }));
                ev.append(c, now_ms)?;
                Ok(n)
            })
        })
        .await
        .map_err(storage_error)?;
    let mut data =
        json!({ "username": u.username, "sessions_ended": ended, "must_change_password": true });
    if generated {
        data["password"] = json!(pw);
    }
    Ok(http::ok(now, data))
}

async fn reset_mfa(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    name: &str,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let u = find(ctx, name).await?;
    let (uid, now_ms) = (u.user_id, now.timestamp_millis());
    let ev = Event {
        actor: p.actor(),
        actor_ip: Some(ip(peer)),
        category: "user",
        action: "mfa.reset",
        object: Some(u.username.clone()),
        result: "ok",
        details: Some(json!({ "reason": str_field(&b, "reason") })),
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
    Ok(http::ok(
        now,
        json!({ "username": u.username, "reset": was }),
    ))
}

async fn user_sessions(
    ctx: &Arc<ApiContext>,
    name: &str,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let u = find(ctx, name).await?;
    let (uid, now_ms) = (u.user_id, now.timestamp_millis());
    let list = ctx
        .pool
        .read("工作階段", move |c| {
            us::user_sessions(c, uid, true, now_ms).map_err(query_failed("工作階段"))
        })
        .await?;
    Ok(http::ok(
        now,
        json!({ "username": u.username, "sessions": list.iter().map(|s| s.json(now_ms, false)).collect::<Vec<_>>() }),
    ))
}

/// 結束某使用者的**全部**工作階段（S16 對他人，D25）
async fn revoke_all(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    name: &str,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let u = find(ctx, name).await?;
    if p.acting_user_id() == Some(u.user_id) {
        return Err(ApiError::conflict(
            "target_is_self",
            "對象是自己：請改用 POST /api/v1/me/sessions/revoke-others（結束其他所有工作階段、保留目前這個）",
        ));
    }
    let (uid, actor, now_ms) = (u.user_id, p.actor(), now.timestamp_millis());
    let reason = str_field(&b, "reason").map(str::to_string);
    let mut ev = Event {
        actor: actor.clone(),
        actor_ip: Some(ip(peer)),
        category: "user",
        action: "session.revoke_all",
        object: Some(u.username.clone()),
        result: "ok",
        ..Event::default()
    };
    let n = ctx
        .admin
        .write(move |c| {
            in_write_tx(c, |c| {
                let n = us::end_user_sessions(c, uid, None, "revoked_by_admin", &actor, now_ms)?;
                ev.details = Some(json!({ "ended": n, "reason": reason }));
                ev.append(c, now_ms)?;
                Ok(n)
            })
        })
        .await
        .map_err(storage_error)?;
    Ok(http::ok(now, json!({ "username": u.username, "ended": n })))
}

/// `GET /api/v1/roles`：內建角色的權限矩陣與成員數（S16「角色與權限」分頁）
async fn roles(ctx: &Arc<ApiContext>, now: DateTime<Utc>) -> Result<Response<ApiBody>, ApiError> {
    let counts = ctx
        .pool
        .read("角色", |c| {
            us::role_member_counts(c).map_err(query_failed("角色"))
        })
        .await?;
    let roles: Vec<Value> = Role::ALL
        .iter()
        .map(|r| {
            json!({
                "role": r.as_str(),
                "label": r.label(),
                "members": counts.iter().find(|(k, _)| k == r.as_str()).map_or(0, |(_, n)| *n),
                "permissions": r.permissions().names(),
            })
        })
        .collect();
    Ok(http::ok(
        now,
        json!({
            "roles": roles,
            "permissions": P::ALL.iter().map(|x| x.name()).collect::<Vec<_>>(),
            "scopes": ["global", "group:<主機群組>"],
            "custom_roles": false,
        }),
    ))
}

/// `GET /api/v1/directory`：目錄服務的設定（bind 密碼不輸出，只有檔案路徑）與群組對應
async fn directory(
    ctx: &Arc<ApiContext>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let l = &ctx.config.ldap;
    let mappings = ctx
        .pool
        .read("目錄服務", |c| {
            us::ldap_mappings(c).map_err(query_failed("目錄服務"))
        })
        .await?;
    Ok(http::ok(
        now,
        json!({
            "enabled": l.enabled,
            "urls": l.urls(),
            "starttls": l.starttls,
            "allow_plaintext": l.allow_plaintext,
            "ca_file": l.ca_file,
            "bind_dn": l.bind_dn,
            "bind_password": if l.bind_password_file.trim().is_empty() { Value::Null } else { json!("********") },
            "user_base": l.user_base,
            "user_filter": l.user_filter,
            "login_attribute": l.login_attribute,
            "display_name_attribute": l.display_name_attribute,
            "email_attribute": l.email_attribute,
            "group_attribute": l.group_attribute,
            "timeout_secs": l.timeout_secs,
            "mappings": mappings.iter().map(|(g, a)| { let mut j = a.json(); j["group_dn"] = json!(g); j }).collect::<Vec<_>>(),
        }),
    ))
}

/// `PUT /api/v1/directory/mappings`：`[{"group_dn": "...", "role": "operator@group:B廠"}]` 取代全部
async fn set_mappings(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let list = b
        .get("mappings")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::param("mappings", "mappings 必須是陣列"))?;
    if list.len() > 1000 {
        return Err(ApiError::param("mappings", "mappings 最多 1000 筆"));
    }
    let mut new: Vec<(String, Assignment)> = Vec::new();
    for m in list {
        let dn = m
            .get("group_dn")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|d| !d.is_empty() && d.len() <= 1024 && !d.chars().any(char::is_control))
            .ok_or_else(|| ApiError::param("mappings", "group_dn 必填（1～1024 字元）"))?;
        let a = Assignment::parse(m.get("role").and_then(Value::as_str).unwrap_or(""))
            .map_err(|e| ApiError::param("mappings", e))?;
        let key = us::normalize_dn(dn);
        if !new.iter().any(|(k, x)| *k == key && *x == a) {
            new.push((key, a));
        }
    }
    let (actor, now_ms) = (p.actor(), now.timestamp_millis());
    let mut ev = Event {
        actor: actor.clone(),
        actor_ip: Some(ip(peer)),
        category: "user",
        action: "directory.mappings.change",
        object: Some("ldap".to_string()),
        result: "ok",
        ..Event::default()
    };
    let n = new.len();
    ctx.admin
        .write(move |c| {
            in_write_tx(c, |c| {
                let old = us::ldap_mappings(c)?;
                let removed: Vec<String> = old
                    .iter()
                    .filter(|o| !new.contains(o))
                    .map(|(g, a)| format!("{g} → {}", a.as_text()))
                    .collect();
                let added: Vec<String> = new
                    .iter()
                    .filter(|x| !old.contains(x))
                    .map(|(g, a)| format!("{g} → {}", a.as_text()))
                    .collect();
                for (g, a) in old.iter().filter(|o| !new.contains(o)) {
                    us::remove_assignment(c, SUBJECT_LDAP_GROUP, g, a)?;
                }
                for (g, a) in new.iter().filter(|x| !old.contains(x)) {
                    us::add_assignment(c, SUBJECT_LDAP_GROUP, g, a, &actor, now_ms)?;
                }
                if !added.is_empty() || !removed.is_empty() {
                    ev.details = Some(json!({ "added": added, "removed": removed }));
                    ev.append(c, now_ms)?;
                }
                Ok(())
            })
        })
        .await
        .map_err(storage_error)?;
    Ok(http::ok(now, json!({ "mappings": n })))
}

/// `POST /api/v1/directory/test`：「以帳號測試登入」（S17）：顯示這個帳號會得到的角色，不建立工作階段
async fn directory_test(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let Some(dir) = ctx.directory.clone().filter(|_| ctx.config.ldap.enabled) else {
        return Err(ApiError::conflict(
            "directory_disabled",
            "目錄服務（[ldap]）未啟用",
        ));
    };
    let username = str_field(&b, "username").unwrap_or("").trim().to_string();
    let password = str_field(&b, "password").unwrap_or("").to_string();
    if username.is_empty() {
        return Err(ApiError::param("username", "username 必填"));
    }
    let now_ms = now.timestamp_millis();
    if let Err(retry) = ctx
        .auth
        .failures
        .check(peer.ip(), std::time::Instant::now())
    {
        return Err(ApiError::rate_limited(retry));
    }
    let result = dir.authenticate(&username, &password).await;
    drop(password);
    // 測試登入也不能用來猜密碼：失敗與登入失敗一樣扣來源的額度
    if matches!(
        result,
        Err(crate::users::ldap::DirError::InvalidCredentials
            | crate::users::ldap::DirError::UserNotFound)
    ) {
        ctx.auth.note_failure(peer.ip(), "目錄服務測試登入失敗");
    }
    let (data, code) = match result {
        Ok(du) => {
            let groups = du.groups.clone();
            let (grants, list) = ctx
                .pool
                .read("目錄服務", move |c| {
                    // 個人指派只在帳號已存在時才有；這裡只看群組對應
                    us::grants_for(c, -1, &groups).map_err(query_failed("目錄服務"))
                })
                .await?;
            (
                json!({
                    "ok": true,
                    "user": {"dn": du.dn, "username": du.username, "display_name": du.display_name, "email": du.email, "groups": du.groups},
                    "roles": list.iter().map(|(a, g)| { let mut j = a.json(); j["via_group"] = json!(g); j }).collect::<Vec<_>>(),
                    "permissions": grants.global.names(),
                }),
                None,
            )
        }
        Err(e) => (
            json!({
                "ok": false,
                "product_code": e.product_code(),
                "reason": e.reason(),
                "detail": e.detail(),
            }),
            Some(e.product_code()),
        ),
    };
    super::account::audit(
        ctx,
        Event {
            actor: p.actor(),
            actor_ip: Some(ip(peer)),
            category: "user",
            action: "directory.test",
            object: Some(username),
            result: if code.is_none() { "ok" } else { "failed" },
            code,
            ..Event::default()
        },
        now_ms,
    )
    .await;
    Ok(http::ok(now, data))
}

fn token_json(t: &tokens::TokenRow, now_ms: i64) -> Value {
    json!({
        "token_id": t.token_id,
        "name": t.name,
        "grant": t.grant_label(),
        "role": t.role,
        "owner": t.owner_username,
        "scope": (!t.scope.is_empty()).then_some(&t.scope),
        "state": t.state(now_ms).as_str(),
        "created_at": ts_ms(t.created_at),
        "created_by": t.created_by,
        "expires_at": t.expires_at.map(ts_ms),
        "revoked_at": t.revoked_at.map(ts_ms),
        "revoked_by": t.revoked_by,
        "revoke_reason": t.revoke_reason,
        "last_used_at": t.last_used_at.map(ts_ms),
        "last_used_ip": t.last_used_ip,
    })
}

async fn list_tokens(
    ctx: &Arc<ApiContext>,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(&["all"], &[])?;
    let all = q.bool("all")?.unwrap_or(false);
    let now_ms = now.timestamp_millis();
    let rows = ctx
        .pool
        .read("API token", |c| {
            tokens::list(c).map_err(query_failed("API token"))
        })
        .await?;
    let data: Vec<Value> = rows
        .iter()
        .filter(|t| all || t.state(now_ms) == tokens::TokenState::Active)
        .map(|t| token_json(t, now_ms))
        .collect();
    Ok(http::ok(now, json!({ "tokens": data })))
}

async fn create_token(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let name = str_field(&b, "name").unwrap_or("").to_string();
    tokens::validate_name(&name).map_err(|e| ApiError::param("name", e))?;
    let role = match str_field(&b, "role") {
        Some(r) => Some(Role::parse(r).ok_or_else(|| {
            ApiError::param("role", "role 必須是 admin、operator、auditor 或 viewer")
        })?),
        None => None,
    };
    let now_ms = now.timestamp_millis();
    let grant = match str_field(&b, "owner") {
        Some(owner) => {
            let u = find(ctx, owner).await?;
            if !u.active() {
                return Err(ApiError::conflict("user_disabled", "擁有者已停用"));
            }
            TokenGrant::User {
                user_id: u.user_id,
                username: u.username,
                role,
            }
        }
        None => TokenGrant::Role(role.ok_or_else(|| {
            ApiError::param(
                "role",
                "role 或 owner 必填（API token 必須屬於某個角色或使用者）",
            )
        })?),
    };
    let expires_at = match (
        b.get("expires_in_secs").and_then(Value::as_i64),
        str_field(&b, "expires_at"),
    ) {
        (Some(_), Some(_)) => {
            return Err(ApiError::param(
                "expires_at",
                "expires_in_secs 與 expires_at 只能擇一",
            ));
        }
        (Some(s), None) => now_ms.saturating_add(s.saturating_mul(1000)),
        (None, Some(t)) => DateTime::parse_from_rfc3339(t)
            .map(|t| t.timestamp_millis())
            .map_err(|_| ApiError::param("expires_at", "expires_at 必須是 RFC 3339"))?,
        (None, None) => now_ms + TOKEN_DEFAULT_DAYS * 86_400_000,
    };
    if expires_at <= now_ms || expires_at > now_ms + TOKEN_MAX_DAYS * 86_400_000 {
        return Err(ApiError::param(
            "expires_at",
            format!("有效期限必須在未來、最長 {TOKEN_MAX_DAYS} 天（不提供不過期的 token）"),
        ));
    }
    let (actor, ipt) = (p.actor(), ip(peer));
    let new = ctx
        .admin
        .write(move |c| {
            tokens::create_with(
                c,
                &name,
                &grant,
                Some(expires_at),
                Some(&actor),
                Some(&ipt),
                now_ms,
            )
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))
        })
        .await
        .map_err(storage_error)?;
    let mut data = token_json(&new.row, now_ms);
    data["token"] = json!(new.token);
    Ok(http::json(
        StatusCode::CREATED,
        now,
        json!({ "data": data }),
    ))
}

async fn revoke_token(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    id: &str,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let tid = id.trim().to_ascii_lowercase();
    let reason = str_field(&b, "reason").map(str::to_string);
    let (actor, ipt, now_ms) = (p.actor(), ip(peer), now.timestamp_millis());
    let r = ctx
        .admin
        .write(move |c| {
            tokens::revoke_with(c, &tid, Some(&actor), Some(&ipt), reason.as_deref(), now_ms)
        })
        .await
        .map_err(storage_error)?;
    let (t, already) = match r {
        Revoked::Now(t) => (Some(t), false),
        Revoked::Already(t) => (Some(t), true),
        Revoked::NotFound => (None, false),
    };
    match t {
        Some(t) => {
            let mut v = token_json(&t, now_ms);
            v["already_revoked"] = json!(already);
            Ok(http::ok(now, v))
        }
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "token_not_found",
            format!("找不到 API token {id}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_parsed_and_deduplicated() {
        let b = json!({"roles": ["operator@group:A", "admin", "admin"]});
        let r = parse_roles(&b).unwrap().unwrap();
        assert_eq!(r.len(), 2);
        assert!(is_global_admin(&r));
        assert!(parse_roles(&json!({"roles": ["root"]})).is_err());
        assert!(parse_roles(&json!({"roles": "admin"})).is_err());
        assert_eq!(parse_roles(&json!({})).unwrap(), None);
    }
}
