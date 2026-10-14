//! 註冊與 PKI 的管理端點（`kairos/API.md` 第 11 節；S12～S15、S21）。
//!
//! 寫入端點（`POST`、`PUT`）的本文為 JSON（`Content-Type: application/json`，上限 1 MiB；CSR 佇列匯入
//! 32 MiB），以 `principal.require(Permission::…)` 檢查範圍，每個寫入都寫入 `audit_events`
//! （執行者 `token:<token_id>`、來源 IP）。操作本身與 CLI 共用 [`crate::enroll::ops`]。

use chrono::{DateTime, Utc};
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};
use hyper::{Method, Request, Response, StatusCode};
use kairos_proto::enroll::{EnrollMode, TokenApproval};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;

use super::ApiContext;
use super::auth::{Permission, Principal};
use super::http::{self, ApiBody, ApiError, Query, ts_ms};
use crate::enroll::ops::{self, Decision, OpError, RevokeResult, TokenInput};
use crate::enroll::pki_mgr::IssueStatus;
use crate::enroll::store::{self, DbState, RequestFilter};
use crate::enroll::{EnrollService, handlers};
use crate::pki::issue::Reason;
use crate::pki::store as ps;
use crate::settings;

/// 寫入本文的上限
pub const MAX_BODY: usize = 1024 * 1024;
/// CSR 佇列匯入的上限
pub const MAX_IMPORT_BODY: usize = 32 * 1024 * 1024;

fn op_error(e: OpError) -> ApiError {
    let status = match e.code {
        "invalid_parameter" => StatusCode::BAD_REQUEST,
        "token_not_found" | "request_not_found" | "certificate_not_found" | "ca_not_found" => {
            StatusCode::NOT_FOUND
        }
        "storage_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "internal" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::CONFLICT,
    };
    let mut err = ApiError::new(status, e.code, e.message);
    if let Some(f) = e.field {
        err = err.with_details(json!({"parameter": f}));
    }
    if status == StatusCode::SERVICE_UNAVAILABLE {
        err = err.with_product_code("KRS-E4003");
    }
    err
}

fn db_error(e: impl std::fmt::Display) -> ApiError {
    log::warn!("註冊管理端點：資料庫無法讀寫：{e}");
    ApiError::storage("註冊資料")
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "enroll_unavailable",
        "註冊服務沒有啟動（需要 storage.backend = \"sqlite\"；見 Kairos 的日誌）",
    )
}

fn segments(path: &str) -> Vec<String> {
    path.trim_end_matches('/')
        .split('/')
        .skip(1)
        .filter_map(|s| http::percent_decode(s, false))
        .collect()
}

/// 處理 `/api/v1/enroll/…`、`/api/v1/pki/…`
pub async fn handle(
    ctx: Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
) -> Response<ApiBody> {
    let now = Utc::now();
    match route(&ctx, peer, req, now).await {
        Ok(r) => r,
        Err(e) => e.into_response(Utc::now()),
    }
}

async fn route(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    if ctx.shutdown.started_at().is_some() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "Kairos 正在關機",
        ));
    }
    let segs = segments(req.uri().path());
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let method = req.method().clone();
    let query = Query::parse(req.uri().query())?;
    let principal = ctx.auth.authenticate(req.headers(), peer.ip(), now).await?;
    principal.require(Permission::Read)?;
    let svc = ctx.enroll.clone().ok_or_else(unavailable)?;
    let actor = format!("token:{}", principal.token_id());
    let ip = peer.ip().to_canonical().to_string();
    let w = Writer {
        svc: &svc,
        principal: &principal,
        actor: &actor,
        ip: &ip,
    };
    let rest = match segs.as_slice() {
        ["api", "v1", rest @ ..] => rest,
        _ => return Err(ApiError::not_found()),
    };
    let is_write = method == Method::POST || method == Method::PUT;
    let body = if is_write {
        let max = if matches!(rest, ["pki", "signing-queue", "import"]) {
            MAX_IMPORT_BODY
        } else {
            MAX_BODY
        };
        Some(read_body(req, max).await?)
    } else {
        if method != Method::GET && method != Method::HEAD {
            return Err(method_not_allowed());
        }
        None
    };
    let b = || body.clone().unwrap_or(Value::Null);
    match (method.as_str(), rest) {
        ("GET" | "HEAD", ["enroll", "tokens"]) => list_tokens(&svc, &query, now).await,
        ("POST", ["enroll", "tokens"]) => create_token(&w, b(), now).await,
        ("GET" | "HEAD", ["enroll", "tokens", id]) => get_token(&svc, id, now).await,
        ("POST", ["enroll", "tokens", id, "disable"]) => disable_token(&w, id, b(), now).await,
        ("GET" | "HEAD", ["enroll", "requests"]) => list_requests(&svc, &query, now).await,
        ("POST", ["enroll", "requests", "approve"]) => approve(&w, b(), now).await,
        ("POST", ["enroll", "requests", "reject"]) => reject(&w, b(), now).await,
        ("GET" | "HEAD", ["enroll", "requests", id]) => get_request(&svc, id, now).await,
        ("GET" | "HEAD", ["enroll", "mode"]) => Ok(http::ok(now, mode_view(&svc))),
        ("GET" | "HEAD", ["enroll", "mode", "impact"]) => impact(&svc, &query, now).await,
        ("PUT", ["enroll", "mode"]) => set_mode(&w, b(), now).await,
        ("GET" | "HEAD", ["pki", "certificates"]) => list_certs(&svc, &query, now).await,
        ("POST", ["pki", "certificates", "revoke"]) => revoke(&w, b(), now).await,
        ("GET" | "HEAD", ["pki", "certificates", serial]) => get_cert(&svc, serial, now).await,
        ("PUT", ["pki", "policy"]) => set_policy(&w, b(), now).await,
        ("POST", ["pki", "grace-overrides"]) => grace_overrides(&w, b(), now).await,
        ("GET" | "HEAD", ["pki", "ca"]) => get_ca(&svc, now).await,
        ("GET" | "HEAD", ["pki", "crl"]) => get_crl(&svc, now).await,
        ("POST", ["pki", "crl", "regenerate"]) => regenerate_crl(&w, now).await,
        ("GET" | "HEAD", ["pki", "signing-queue"]) => signing_queue(&svc, now).await,
        ("GET" | "HEAD", ["pki", "signing-queue", "export"]) => {
            principal.require(Permission::PkiQueue)?;
            export_queue(&svc, now).await
        }
        ("POST", ["pki", "signing-queue", "import"]) => import_queue(&w, b(), now).await,
        _ => Err(ApiError::not_found()),
    }
}

fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "這個端點不接受這個方法",
    )
}

async fn read_body(req: Request<Incoming>, max: usize) -> Result<Value, ApiError> {
    let headers = req.headers();
    let ct = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "本文必須是 application/json",
        ));
    }
    if headers.contains_key(TRANSFER_ENCODING) {
        return Err(ApiError::param(
            "content-length",
            "請以 Content-Length 送出本文（不接受 chunked）",
        ));
    }
    let len = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
        .ok_or_else(|| ApiError::param("content-length", "需要 Content-Length"))?;
    if len > max {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("本文超過 {max} bytes"),
        ));
    }
    use hyper::body::Body;
    let mut body = req.into_body();
    let mut buf = Vec::with_capacity(len);
    loop {
        let frame = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await;
        match frame {
            None => break,
            Some(Err(_)) => return Err(ApiError::param("body", "本文無法讀取")),
            Some(Ok(f)) => {
                if let Ok(d) = f.into_data() {
                    if buf.len() + d.len() > len {
                        return Err(ApiError::param("body", "本文比 Content-Length 長"));
                    }
                    buf.extend_from_slice(&d);
                }
            }
        }
    }
    let bytes = Bytes::from(buf);
    serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::param("body", format!("本文不是有效的 JSON：{e}")))
}

/// 寫入的上下文
struct Writer<'a> {
    svc: &'a Arc<EnrollService>,
    principal: &'a Principal,
    actor: &'a str,
    ip: &'a str,
}

fn str_field<'a>(b: &'a Value, name: &str) -> Option<&'a str> {
    b.get(name).and_then(Value::as_str)
}

fn str_list(b: &Value, name: &str) -> Result<Option<Vec<String>>, ApiError> {
    match b.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| ApiError::param(name, format!("{name} 必須是字串陣列")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        _ => Err(ApiError::param(name, format!("{name} 必須是字串陣列"))),
    }
}

fn parse_time(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// L2 確認：`expected_count` 必須等於這次處理的筆數
fn check_expected(b: &Value, n: usize) -> Result<(), ApiError> {
    match b.get("expected_count").and_then(Value::as_u64) {
        None => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "confirmation_required",
            "這個操作需要 expected_count（L2 確認）",
        )),
        Some(c) if c as usize != n => Err(ApiError::new(
            StatusCode::CONFLICT,
            "count_mismatch",
            format!("expected_count {c} 與實際筆數 {n} 不同"),
        )
        .with_details(json!({"count": n}))),
        Some(_) => Ok(()),
    }
}

fn ids(b: &Value, name: &str) -> Result<Vec<String>, ApiError> {
    let list = str_list(b, name)?.ok_or_else(|| ApiError::param(name, format!("{name} 必填")))?;
    if list.is_empty() || list.len() > ops::MAX_BATCH {
        return Err(ApiError::param(name, format!("{name} 必須有 1～1000 筆")));
    }
    Ok(list
        .into_iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .collect())
}

// ---------------------------------------------------------------------------
// 註冊 token（S12）
// ---------------------------------------------------------------------------

async fn token_view(svc: &Arc<EnrollService>, t: &store::TokenRow, now_ms: i64) -> Value {
    let mut v = ops::token_json(t, now_ms);
    let id = t.token_id.clone();
    if let Ok(p) = svc
        .db
        .read(move |conn| {
            ops::token_progress(conn, &id).map_err(|e| crate::enroll::db::DbError(e.message))
        })
        .await
    {
        let mut p = p;
        let queued = svc
            .queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|(tid, _)| *tid == t.token_id)
            .count();
        let clock = svc
            .clock_rejected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|(tid, _)| tid.as_deref() == Some(t.token_id.as_str()))
            .count();
        p["queued"] = json!(queued);
        p["clock_rejected"] = json!(clock);
        v["progress"] = p;
    }
    v
}

async fn list_tokens(
    svc: &Arc<EnrollService>,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(&["state", "limit"], &[])?;
    let state = q.get("state").unwrap_or("active");
    if !matches!(
        state,
        "active" | "expired" | "disabled" | "exhausted" | "all"
    ) {
        return Err(ApiError::param(
            "state",
            "state 必須是 active、expired、exhausted、disabled 或 all",
        ));
    }
    let limit = limit_of(q)?;
    let now_ms = now.timestamp_millis();
    let rows = svc
        .db
        .read(|conn| Ok(store::list_tokens(conn)?))
        .await
        .map_err(db_error)?;
    let filtered: Vec<_> = rows
        .into_iter()
        .filter(|t| state == "all" || t.state(now_ms).as_str() == state)
        .collect();
    let total = filtered.len();
    let mut data = Vec::new();
    for t in filtered.iter().take(limit) {
        data.push(token_view(svc, t, now_ms).await);
    }
    Ok(http::json(
        StatusCode::OK,
        now,
        json!({"data": data, "page": {"limit": limit, "total": total, "next_cursor": null}}),
    ))
}

fn limit_of(q: &Query) -> Result<usize, ApiError> {
    match q.get("limit") {
        None => Ok(100),
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=1000).contains(n))
            .ok_or_else(|| ApiError::param("limit", "limit 必須介於 1..=1000")),
    }
}

async fn get_token(
    svc: &Arc<EnrollService>,
    id: &str,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let tid = id.to_string();
    let t = svc
        .db
        .read(move |conn| Ok(store::get_token(conn, &tid)?))
        .await
        .map_err(db_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "token_not_found",
                format!("找不到註冊 token {id}"),
            )
        })?;
    Ok(http::ok(
        now,
        token_view(svc, &t, now.timestamp_millis()).await,
    ))
}

/// 安裝字串與 CA 指紋（ENROLL.md 第 14.7 節）
fn install_info(svc: &EnrollService, token: &str, tags: Option<Vec<String>>) -> Value {
    let agent = svc
        .config
        .enroll
        .advertise
        .trim()
        .rsplit_once(':')
        .map(|(h, _)| h.to_string())
        .filter(|h| !h.is_empty());
    let server_listen = svc.config.server.listen.clone();
    let agent_endpoint = agent
        .map(|h| {
            let port = server_listen.rsplit_once(':').map_or("8443", |(_, p)| p);
            format!("{h}:{port}")
        })
        .unwrap_or_else(|| server_listen.clone());
    let enroll_endpoint = if svc.config.enroll.advertise.trim().is_empty() {
        crate::enroll::server::Listener::local_addr(svc)
            .unwrap_or_else(|| svc.config.enroll.listen_addr(&server_listen))
    } else {
        svc.config.enroll.advertise.trim().to_string()
    };
    let pin = svc.pki.server_ca().map(|c| c.pin);
    let s = kairos_proto::enroll::normalize_endpoint(&agent_endpoint).ok();
    let e = kairos_proto::enroll::normalize_endpoint(&enroll_endpoint).ok();
    let derived = s.as_ref().map(|s| {
        format!(
            "{}:{}",
            kairos_proto::enroll::endpoint_host(s),
            kairos_proto::enroll::DEFAULT_ENROLL_PORT
        )
    });
    let install = match (&s, &pin) {
        (Some(s), Some(p)) => kairos_proto::enroll::InstallString {
            s: s.clone(),
            e: e.clone().filter(|e| {
                Some(e.clone())
                    != derived
                        .as_ref()
                        .and_then(|d| kairos_proto::enroll::normalize_endpoint(d).ok())
            }),
            t: token.to_string(),
            p: p.clone(),
            g: tags,
        }
        .encode()
        .ok(),
        _ => None,
    };
    json!({
        "install_string": install,
        "ca_pin": pin,
        "ca_pin_display": pin.as_deref().and_then(|p| kairos_proto::enroll::ca_pin_display(p).ok()),
        "agent_endpoint": s.unwrap_or(agent_endpoint),
        "enroll_endpoint": e.unwrap_or(enroll_endpoint),
    })
}

async fn create_token(
    w: &Writer<'_>,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    w.principal.require(Permission::EnrollTokenWrite)?;
    let approval = match str_field(&b, "approval") {
        Some("manual") => Some(TokenApproval::Manual),
        Some("auto") => Some(TokenApproval::Auto),
        _ => {
            return Err(ApiError::param(
                "approval",
                "approval 必須是 manual 或 auto",
            ));
        }
    };
    let batch_mode = b
        .get("batch_mode")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if approval == Some(TokenApproval::Auto)
        || (batch_mode && approval == Some(TokenApproval::Auto))
    {
        w.principal.require(Permission::EnrollTokenAuto)?;
    }
    let now_ms = now.timestamp_millis();
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
        (Some(s), None) => Some(now_ms + s.saturating_mul(1000)),
        (None, Some(t)) => Some(
            parse_time(t)
                .ok_or_else(|| ApiError::param("expires_at", "expires_at 必須是 RFC 3339"))?,
        ),
        (None, None) => None,
    };
    let input = TokenInput {
        name: str_field(&b, "name").unwrap_or("").to_string(),
        group: str_field(&b, "group").unwrap_or("").to_string(),
        tags: str_list(&b, "tags")?,
        expires_at,
        max_uses: b.get("max_uses").and_then(Value::as_i64).unwrap_or(0),
        approval,
        allowed_cidrs: str_list(&b, "allowed_cidrs")?,
        batch_mode,
        allowed_agent_ids: str_list(&b, "allowed_agent_ids")?,
        cert_lifetime_days: b.get("cert_lifetime_days").and_then(Value::as_i64),
    };
    let actor = w.actor.to_string();
    let ip = w.ip.to_string();
    let was_paused = w.svc.is_paused();
    let created = w
        .svc
        .db
        .write(move |conn| Ok(ops::create_token(conn, &input, &actor, Some(&ip), now_ms)))
        .await
        .map_err(db_error)?
        .map_err(op_error)?;
    w.svc.wake();
    let mut data = token_view(w.svc, &created.row, now_ms).await;
    let token = created.token.canonical();
    data["token"] = json!(token);
    if let Value::Object(m) = install_info(w.svc, &token, created.row.tags.clone()) {
        for (k, v) in m {
            data[k] = v;
        }
    }
    data["enroll_mode"] = json!(w.svc.mode_state().as_str());
    data["auto_pause_cleared"] = json!(was_paused);
    Ok(http::json(StatusCode::CREATED, now, json!({"data": data})))
}

async fn disable_token(
    w: &Writer<'_>,
    id: &str,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    w.principal.require(Permission::EnrollTokenWrite)?;
    let reason = str_field(&b, "reason").map(str::to_string);
    let (actor, ip, tid) = (w.actor.to_string(), w.ip.to_string(), id.to_string());
    let now_ms = now.timestamp_millis();
    let (row, already) = w
        .svc
        .db
        .write(move |conn| {
            Ok(ops::disable_token(
                conn,
                &tid,
                &actor,
                Some(&ip),
                reason.as_deref(),
                now_ms,
            ))
        })
        .await
        .map_err(db_error)?
        .map_err(op_error)?;
    let mut data = ops::token_json(&row, now_ms);
    data["already_disabled"] = json!(already);
    Ok(http::ok(now, data))
}

// ---------------------------------------------------------------------------
// 申請（S13）
// ---------------------------------------------------------------------------

async fn list_requests(
    svc: &Arc<EnrollService>,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(&["view", "token", "kind", "flag", "q", "limit"], &[])?;
    let closed = match q.get("view").unwrap_or("pending") {
        "pending" => false,
        "closed" => true,
        _ => return Err(ApiError::param("view", "view 必須是 pending 或 closed")),
    };
    let kind = match q.get("kind") {
        None => None,
        Some(k) => Some(ops::parse_kind(k).ok_or_else(|| {
            ApiError::param("kind", "kind 必須是 enroll、grace_renewal 或 renewal")
        })?),
    };
    let limit = limit_of(q)?;
    let filter = RequestFilter {
        closed,
        token_id: q.get("token").map(str::to_string),
        kind,
        state: None,
        limit: 100_000,
    };
    let rows = svc
        .db
        .read(move |conn| Ok(store::list_requests(conn, &filter)?))
        .await
        .map_err(db_error)?;
    let flag = q.get("flag").map(str::to_string);
    let text = q.get("q").map(|s| s.trim().to_ascii_lowercase());
    let code = text
        .as_deref()
        .and_then(kairos_proto::enroll::RequestCode::parse)
        .map(|c| c.compact());
    let matched: Vec<Value> = rows
        .iter()
        .filter(|r| match flag.as_deref() {
            None => true,
            Some("none") => r.flags.is_empty(),
            Some("any") => !r.flags.is_empty(),
            Some(f) => r.flags.iter().any(|x| x == f),
        })
        .filter(|r| match &text {
            None => true,
            Some(t) => {
                code.as_deref() == Some(r.request_code.as_str())
                    || r.hostname.to_ascii_lowercase().contains(t.as_str())
                    || r.source_ip == *t
                    || r.agent_id.starts_with(t.as_str())
            }
        })
        .map(ops::request_json)
        .collect();
    let total = matched.len();
    let data: Vec<Value> = matched.into_iter().take(limit).collect();
    Ok(http::json(
        StatusCode::OK,
        now,
        json!({"data": data, "page": {"limit": limit, "total": total, "next_cursor": null}}),
    ))
}

async fn get_request(
    svc: &Arc<EnrollService>,
    id: &str,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let input = id.to_string();
    let resolved = svc
        .db
        .read(move |conn| {
            ops::resolve_request(conn, &input).map_err(|e| crate::enroll::db::DbError(e.message))
        })
        .await
        .map_err(db_error)?;
    match resolved {
        ops::Resolved::One(r) => Ok(http::ok(now, ops::request_json(&r))),
        ops::Resolved::NotFound => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "request_not_found",
            format!("找不到申請 {id}"),
        )),
        ops::Resolved::Ambiguous(rows) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "ambiguous_request_code",
            "申請編號對到多筆申請，請改用 request_id",
        )
        .with_details(
            json!({"request_ids": rows.iter().map(|r| r.request_id.clone()).collect::<Vec<_>>()}),
        )),
    }
}

fn summary(results: &[Value]) -> Value {
    let mut m = serde_json::Map::new();
    for r in results {
        let k = r["result"].as_str().unwrap_or("failed").to_string();
        let n = m.get(&k).and_then(Value::as_u64).unwrap_or(0) + 1;
        m.insert(k, json!(n));
    }
    Value::Object(m)
}

async fn approve(
    w: &Writer<'_>,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    w.principal.require(Permission::EnrollDecide)?;
    let ids = ids(&b, "request_ids")?;
    let lifetime = b.get("cert_lifetime_days").and_then(Value::as_i64);
    if let Some(d) = lifetime
        && !(90..=825).contains(&d)
    {
        return Err(ApiError::param(
            "cert_lifetime_days",
            "cert_lifetime_days 必須介於 90..=825",
        ));
    }
    let (actor, ip) = (w.actor.to_string(), w.ip.to_string());
    let now_ms = now.timestamp_millis();
    let ids2 = ids.clone();
    let decisions = w
        .svc
        .db
        .write(move |conn| {
            Ok(ops::approve(
                conn,
                &ids2,
                &actor,
                Some(&ip),
                lifetime,
                now_ms,
            ))
        })
        .await
        .map_err(db_error)?
        .map_err(op_error)?;
    let mut results = Vec::new();
    for (id, d) in decisions {
        let mut r = json!({"request_id": id, "result": d.as_str()});
        match d {
            Decision::Approved => {
                // 線上簽發：立即簽發（離線或私鑰無法使用時停在「已核准，待簽發」）
                match handlers::issue_now(w.svc, &id, w.actor).await {
                    Ok(Some((req, cert))) if req.state == DbState::Issued => {
                        r["result"] = json!("issued");
                        r["serial"] = json!(cert.map(|c| c.serial));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        r["result"] = json!("failed");
                        r["code"] = json!(e.code.as_str());
                    }
                }
                w.svc.notify_request(&id);
            }
            Decision::AlreadyDecided(s) => r["state"] = json!(s.as_str()),
            _ => {}
        }
        results.push(r);
    }
    Ok(http::ok(
        now,
        json!({"results": results, "summary": summary(&results)}),
    ))
}

async fn reject(
    w: &Writer<'_>,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    w.principal.require(Permission::EnrollDecide)?;
    let ids = ids(&b, "request_ids")?;
    check_expected(&b, ids.len())?;
    let reason = str_field(&b, "reason")
        .filter(|r| !r.trim().is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "confirmation_required",
                "拒絕必須填寫 reason",
            )
        })?
        .to_string();
    let code = str_field(&b, "reason_code").unwrap_or("other").to_string();
    let (actor, ip) = (w.actor.to_string(), w.ip.to_string());
    let now_ms = now.timestamp_millis();
    let decisions = w
        .svc
        .db
        .write(move |conn| {
            Ok(ops::reject(
                conn,
                &ids,
                &actor,
                Some(&ip),
                &code,
                &reason,
                now_ms,
            ))
        })
        .await
        .map_err(db_error)?
        .map_err(op_error)?;
    let results: Vec<Value> = decisions
        .into_iter()
        .map(|(id, d)| {
            w.svc.notify_request(&id);
            let mut r = json!({"request_id": id, "result": d.as_str()});
            if let Decision::AlreadyDecided(s) = d {
                r["state"] = json!(s.as_str());
            }
            r
        })
        .collect();
    Ok(http::ok(
        now,
        json!({"results": results, "summary": summary(&results)}),
    ))
}

// ---------------------------------------------------------------------------
// 模式（S21）
// ---------------------------------------------------------------------------

fn mode_view(svc: &EnrollService) -> Value {
    let eff = svc.effective();
    let listen = svc.listen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    json!({
        "mode": eff.mode.as_str(),
        "state": svc.mode_state().as_str(),
        "auto_pause_enabled": eff.auto_pause_enabled,
        "auto_pause_after_hours": eff.auto_pause_after_hours,
        "paused_since": svc.paused_since().map(ts_ms),
        "listening": listen.listening,
        "listen_addr": listen.addr,
        "not_listening_reason": listen.reason,
        "pki_mode": svc.pki.mode().as_str(),
        "issuance": match svc.pki.status() {
            IssueStatus::Online { ca_id } => json!({"status": "online", "ca_id": ca_id}),
            IssueStatus::Offline => json!({"status": "offline"}),
            IssueStatus::Unavailable(r) => json!({"status": "unavailable", "reason": r, "product_code": "KRS-E1302"}),
        },
        "sources": eff.sources_json(),
        "stats": {
            "requests": svc.stats.requests.load(std::sync::atomic::Ordering::Relaxed),
            "enroll_requests": svc.stats.enroll_requests.load(std::sync::atomic::Ordering::Relaxed),
            "renew_requests": svc.stats.renew_requests.load(std::sync::atomic::Ordering::Relaxed),
            "rate_limited_enroll": svc.stats.limited_enroll.load(std::sync::atomic::Ordering::Relaxed),
            "rate_limited_renew": svc.stats.limited_renew.load(std::sync::atomic::Ordering::Relaxed),
            "blocked_requests": svc.stats.blocked.load(std::sync::atomic::Ordering::Relaxed),
            "busy": svc.stats.busy.load(std::sync::atomic::Ordering::Relaxed),
            "clock_rejected": svc.stats.clock_rejected.load(std::sync::atomic::Ordering::Relaxed),
            "clock_rejected_hosts_15m": svc.clock_rejected.lock().unwrap_or_else(|e| e.into_inner()).len(),
            "pre_handshake_closed": svc.stats.pre_handshake_closed.load(std::sync::atomic::Ordering::Relaxed),
            "issued": svc.stats.issued.load(std::sync::atomic::Ordering::Relaxed),
            "pending_created": svc.stats.pending_created.load(std::sync::atomic::Ordering::Relaxed),
            "blocked_sources": svc.limits.l7.blocked_count(svc.now_ms()),
        },
    })
}

async fn impact(
    svc: &Arc<EnrollService>,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(&["mode"], &[])?;
    let mode = q
        .get("mode")
        .and_then(EnrollMode::parse)
        .ok_or_else(|| ApiError::param("mode", "mode 必須是 open、renew_only 或 closed"))?;
    let grace = svc.effective().policy.expired_renew_grace_days;
    let now_ms = now.timestamp_millis();
    let i = svc
        .db
        .read(move |conn| {
            ops::impact(conn, grace, now_ms).map_err(|e| crate::enroll::db::DbError(e.message))
        })
        .await
        .map_err(db_error)?;
    Ok(http::ok(now, i.json(mode, now_ms)))
}

fn locked(svc: &EnrollService, key: &str) -> Result<(), ApiError> {
    if svc.config.provenance.source(key) == crate::config::SettingSource::Environment {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "locked_by_environment",
            format!("{key} 由環境變數設定，無法從主控台修改"),
        )
        .with_details(json!({"variable": crate::config::Provenance::env_var(key)})));
    }
    Ok(())
}

async fn set_mode(
    w: &Writer<'_>,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    w.principal.require(Permission::EnrollModeWrite)?;
    let mode =
        match b.get("mode") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_str().and_then(EnrollMode::parse).ok_or_else(|| {
                ApiError::param("mode", "mode 必須是 open、renew_only 或 closed")
            })?),
        };
    let auto = b.get("auto_pause_enabled").and_then(Value::as_bool);
    let hours = match b.get("auto_pause_after_hours") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|h| (1..=168).contains(h))
                .ok_or_else(|| {
                    ApiError::param(
                        "auto_pause_after_hours",
                        "auto_pause_after_hours 必須介於 1..=168",
                    )
                })? as u32,
        ),
    };
    if mode.is_some() {
        locked(w.svc, settings::KEY_MODE)?;
    }
    if auto.is_some() {
        locked(w.svc, settings::KEY_AUTO_PAUSE_ENABLED)?;
    }
    if hours.is_some() {
        locked(w.svc, settings::KEY_AUTO_PAUSE_AFTER_HOURS)?;
    }
    let now_ms = now.timestamp_millis();
    let grace = w.svc.effective().policy.expired_renew_grace_days;
    let current = w
        .svc
        .db
        .read(move |conn| {
            ops::impact(conn, grace, now_ms).map_err(|e| crate::enroll::db::DbError(e.message))
        })
        .await
        .map_err(db_error)?;
    let target = mode.unwrap_or(w.svc.effective().mode);
    let (level, _) = current.level(target);
    let reason = str_field(&b, "reason").map(str::to_string);
    if mode == Some(EnrollMode::Closed) {
        let token = str_field(&b, "impact_token").ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "confirmation_required",
                "切換到 closed 需要 impact_token",
            )
        })?;
        if !current.verify(EnrollMode::Closed, token, now_ms) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "impact_changed",
                "影響數字在確認後改變，請重新確認",
            )
            .with_details(current.json(EnrollMode::Closed, now_ms)));
        }
        if level == "L2" && reason.as_deref().is_none_or(|r| r.trim().is_empty()) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "confirmation_required",
                "L2 的操作必須填寫 reason",
            ));
        }
    }
    let (actor, ip) = (w.actor.to_string(), w.ip.to_string());
    let level = level.to_string();
    w.svc
        .db
        .write(move |conn| {
            Ok(ops::set_mode(
                conn,
                mode,
                auto,
                hours,
                &actor,
                Some(&ip),
                reason.as_deref(),
                &current,
                &level,
                now_ms,
            ))
        })
        .await
        .map_err(db_error)?
        .map_err(op_error)?;
    // 立即生效：背景工作重新讀取設定並監聽或停止
    w.svc.wake();
    for _ in 0..50 {
        if w.svc.effective().mode == target {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Ok(http::ok(now, mode_view(w.svc)))
}

// ---------------------------------------------------------------------------
// 憑證、政策、CA、CRL（S14、S15）
// ---------------------------------------------------------------------------

fn cert_state(c: &ps::CertRow, grace_days: u32, now_ms: i64) -> &'static str {
    if c.revoked_at.is_some() {
        if c.revoked_effective(now_ms) {
            "revoked"
        } else {
            "revocation_pending"
        }
    } else if c.not_after <= now_ms {
        if now_ms < c.not_after + i64::from(grace_days) * 86_400_000 {
            "expired_in_grace"
        } else {
            "expired_beyond_grace"
        }
    } else if c.not_after - now_ms <= 30 * 86_400_000 {
        "expiring"
    } else {
        "valid"
    }
}

fn cert_json(c: &ps::CertRow, grace_days: u32, now_ms: i64, with_pem: bool) -> Value {
    let mut v = json!({
        "serial": c.serial,
        "agent_id": c.agent_id,
        "ca_id": c.ca_id,
        "state": cert_state(c, grace_days, now_ms),
        "not_before": ts_ms(c.not_before),
        "not_after": ts_ms(c.not_after),
        "issued_at": ts_ms(c.issued_at),
        "issued_via": c.issued_via,
        "request_id": c.request_id,
        "approved_by": c.approved_by,
        "first_used_at": c.first_used_at.map(ts_ms),
        "revoked_at": c.revoked_at.map(ts_ms),
        "revoke_effective_at": c.revoke_effective_at.map(ts_ms),
        "revocation_reason": c.revocation_reason,
        "revoked_by": c.revoked_by,
        "revoke_comment": c.revoke_comment,
        "crl_number": c.crl_number,
        "spki_fingerprint_short": kairos_proto::enroll::hex_lower(&c.spki_sha256)[..8].to_ascii_uppercase(),
    });
    if with_pem {
        v["cert_pem"] = json!(crate::pki::x509::pem_encode("CERTIFICATE", &c.cert_der));
    }
    v
}

async fn list_certs(
    svc: &Arc<EnrollService>,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(&["state", "agent_id", "q", "expires_within", "limit"], &[])?;
    let limit = limit_of(q)?;
    let rows = svc
        .db
        .read(|conn| Ok(ps::list_certs(conn, 1_000_000, None)?))
        .await
        .map_err(db_error)?;
    let now_ms = now.timestamp_millis();
    let grace = svc.effective().policy.expired_renew_grace_days;
    let within = match q.get("expires_within") {
        None => None,
        Some(v) => Some(
            crate::hosts_cli::parse_duration(v)
                .ok_or_else(|| ApiError::param("expires_within", "expires_within 例如 30d"))?
                .as_millis() as i64,
        ),
    };
    let state = q.get("state");
    let agent = q.get("agent_id").map(str::to_ascii_lowercase);
    let text = q.get("q").map(str::to_ascii_uppercase);
    let matched: Vec<Value> = rows
        .iter()
        .filter(|c| state.is_none_or(|s| cert_state(c, grace, now_ms) == s))
        .filter(|c| agent.as_deref().is_none_or(|a| c.agent_id == a))
        .filter(|c| {
            text.as_deref().is_none_or(|t| {
                c.serial.contains(t) || c.agent_id.to_ascii_uppercase().starts_with(t)
            })
        })
        .filter(|c| within.is_none_or(|w| c.not_after > now_ms && c.not_after - now_ms <= w))
        .map(|c| cert_json(c, grace, now_ms, false))
        .collect();
    let total = matched.len();
    Ok(http::json(
        StatusCode::OK,
        now,
        json!({"data": matched.into_iter().take(limit).collect::<Vec<_>>(), "page": {"limit": limit, "total": total, "next_cursor": null}}),
    ))
}

async fn get_cert(
    svc: &Arc<EnrollService>,
    serial: &str,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let s = serial.to_ascii_uppercase();
    let c = svc
        .db
        .read(move |conn| Ok(ps::get_cert(conn, &s)?))
        .await
        .map_err(db_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "certificate_not_found",
                format!("找不到憑證 {serial}"),
            )
        })?;
    let grace = svc.effective().policy.expired_renew_grace_days;
    Ok(http::ok(
        now,
        cert_json(&c, grace, now.timestamp_millis(), true),
    ))
}

async fn revoke(
    w: &Writer<'_>,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    w.principal.require(Permission::CertRevoke)?;
    let serials: Vec<String> = ids(&b, "serials")?
        .into_iter()
        .map(|s| s.to_ascii_uppercase())
        .collect();
    check_expected(&b, serials.len())?;
    let reason = str_field(&b, "reason")
        .and_then(Reason::parse)
        .ok_or_else(|| {
            ApiError::param(
                "reason",
                "reason 必須是 keyCompromise、cessationOfOperation、superseded 或 unspecified",
            )
        })?;
    let comment = str_field(&b, "comment")
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "confirmation_required",
                "撤銷必須填寫 comment",
            )
        })?
        .to_string();
    let (actor, ip) = (w.actor.to_string(), w.ip.to_string());
    let now_ms = now.timestamp_millis();
    let results = w
        .svc
        .db
        .write(move |conn| {
            Ok(ops::revoke(
                conn,
                &serials,
                reason,
                &comment,
                &actor,
                Some(&ip),
                now_ms,
            ))
        })
        .await
        .map_err(db_error)?
        .map_err(op_error)?;
    w.svc.wake();
    let online = matches!(w.svc.pki.status(), IssueStatus::Online { .. });
    let results: Vec<Value> = results
        .into_iter()
        .map(|(s, r)| match r {
            RevokeResult::Revoked { ca_id } => json!({"serial": s, "result": if online && ca_id.is_some() { "revoked" } else { "revocation_pending" }, "ca_id": ca_id}),
            RevokeResult::Already => json!({"serial": s, "result": "already_revoked"}),
            RevokeResult::NotFound => json!({"serial": s, "result": "not_found"}),
        })
        .collect();
    Ok(http::ok(
        now,
        json!({"results": results, "summary": summary(&results)}),
    ))
}

async fn set_policy(
    w: &Writer<'_>,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    w.principal.require(Permission::CertPolicyWrite)?;
    let mut values: Vec<(&'static str, Value)> = Vec::new();
    for (name, key) in [
        ("cert_lifetime_days", settings::KEY_CERT_LIFETIME_DAYS),
        ("renew_before_days", settings::KEY_RENEW_BEFORE_DAYS),
        (
            "renew_requires_approval",
            settings::KEY_RENEW_REQUIRES_APPROVAL,
        ),
        ("expired_renew_grace_days", settings::KEY_GRACE_DAYS),
        (
            "revoke_on_self_decommission",
            settings::KEY_REVOKE_ON_SELF_DECOMMISSION,
        ),
    ] {
        if let Some(v) = b.get(name).filter(|v| !v.is_null()) {
            locked(w.svc, key)?;
            values.push((key, v.clone()));
        }
    }
    if values.is_empty() {
        return Err(ApiError::param("body", "至少要修改一項政策"));
    }
    let (actor, ip) = (w.actor.to_string(), w.ip.to_string());
    let now_ms = now.timestamp_millis();
    w.svc
        .db
        .write(move |conn| Ok(ops::set_policy(conn, &values, &actor, Some(&ip), now_ms)))
        .await
        .map_err(db_error)?
        .map_err(op_error)?;
    w.svc.wake();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let eff = w.svc.effective();
    Ok(http::ok(
        now,
        json!({"policy": eff.policy, "sources": eff.sources_json()}),
    ))
}

async fn grace_overrides(
    w: &Writer<'_>,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    w.principal.require(Permission::GraceOverride)?;
    let ids = ids(&b, "agent_ids")?;
    let until = str_field(&b, "until")
        .and_then(parse_time)
        .ok_or_else(|| ApiError::param("until", "until 必須是 RFC 3339"))?;
    let reason = str_field(&b, "reason").unwrap_or("").to_string();
    let also_renew_only = b
        .get("also_set_renew_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (actor, ip) = (w.actor.to_string(), w.ip.to_string());
    let now_ms = now.timestamp_millis();
    let svc = w.svc.clone();
    let results = w
        .svc
        .db
        .write(move |conn| {
            let r = ops::grace_override(conn, &ids, until, &actor, Some(&ip), &reason, now_ms);
            if r.is_ok() && also_renew_only && svc.effective().mode == EnrollMode::Closed {
                let grace = svc.effective().policy.expired_renew_grace_days;
                let impact = ops::impact(conn, grace, now_ms);
                if let Ok(i) = impact {
                    let _ = ops::set_mode(
                        conn,
                        Some(EnrollMode::RenewOnly),
                        None,
                        None,
                        &actor,
                        Some(&ip),
                        Some(&reason),
                        &i,
                        "L1",
                        now_ms,
                    );
                }
            }
            Ok(r)
        })
        .await
        .map_err(db_error)?
        .map_err(op_error)?;
    w.svc.wake();
    let results: Vec<Value> = results
        .into_iter()
        .map(
            |(id, found)| json!({"agent_id": id, "result": if found { "ok" } else { "not_found" }}),
        )
        .collect();
    Ok(http::ok(now, json!({"results": results})))
}

async fn get_ca(
    svc: &Arc<EnrollService>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let cas = svc
        .db
        .read(|conn| {
            let cas = ps::list_cas(conn)?;
            let mut out = Vec::new();
            for c in cas {
                let n = ps::issued_count_by_ca(conn, c.ca_id)?;
                out.push((c, n));
            }
            Ok(out)
        })
        .await
        .map_err(db_error)?;
    let server_ca = svc.pki.server_ca();
    let data = json!({
        "pki_mode": svc.pki.mode().as_str(),
        "server_ca": server_ca.map(|c| json!({"ca_pin": c.pin, "path": svc.config.pki.server_ca_path})),
        "client_ca_file": svc.config.auth.client_ca_path(),
        "issuing_cas": cas.iter().map(|(c, n)| json!({
            "ca_id": c.ca_id,
            "subject": c.subject,
            "state": c.state,
            "not_before": c.not_before.map(ts_ms),
            "not_after": c.not_after.map(ts_ms),
            "key": if c.key_file.is_some() { "on_server" } else { "offline" },
            "issued_count": n,
            "created_at": ts_ms(c.created_at),
            "retired_at": c.retired_at.map(ts_ms),
        })).collect::<Vec<_>>(),
    });
    Ok(http::ok(now, data))
}

async fn get_crl(
    svc: &Arc<EnrollService>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let data = svc
        .db
        .read(|conn| {
            let mut out = Vec::new();
            for ca in ps::trust_anchor_cas(conn)? {
                let history = ps::crl_history(conn, ca.ca_id, 100)?;
                out.push(json!({
                    "ca_id": ca.ca_id,
                    "current": history.first().map(|c| json!({
                        "crl_number": c.crl_number, "this_update": ts_ms(c.this_update),
                        "next_update": ts_ms(c.next_update), "revoked_count": c.revoked_count, "source": c.source,
                    })),
                    "history": history.iter().map(|c| json!({
                        "crl_number": c.crl_number, "this_update": ts_ms(c.this_update),
                        "next_update": ts_ms(c.next_update), "revoked_count": c.revoked_count, "source": c.source,
                    })).collect::<Vec<_>>(),
                }));
            }
            Ok(out)
        })
        .await
        .map_err(db_error)?;
    Ok(http::ok(now, json!({"crls": data})))
}

async fn regenerate_crl(w: &Writer<'_>, now: DateTime<Utc>) -> Result<Response<ApiBody>, ApiError> {
    w.principal.require(Permission::CertRevoke)?;
    let ids = w.svc.pki.issuer_ids();
    if ids.is_empty() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "issuance_unavailable",
            "線上簽發不可用（離線模式或私鑰無法使用），無法產生 CRL",
        )
        .with_product_code("KRS-E1302"));
    }
    let svc = w.svc.clone();
    let actor = w.actor.to_string();
    let validity = i64::from(svc.config.pki.crl_validity_days) * 86_400_000;
    let now_ms = now.timestamp_millis();
    let numbers = w
        .svc
        .db
        .write(move |conn| {
            let mut out = Vec::new();
            for id in ids {
                if let Ok(Some(n)) =
                    svc.pki
                        .regenerate_crl(conn, id, validity, "manual", Some(&actor), now_ms)
                {
                    out.push(json!({"ca_id": id, "crl_number": n}));
                }
            }
            svc.pki.write_crl_file(conn)?;
            Ok(out)
        })
        .await
        .map_err(db_error)?;
    w.svc.rebuild_tls();
    Ok(http::ok(now, json!({"crls": numbers})))
}

async fn signing_queue(
    svc: &Arc<EnrollService>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let rows = svc
        .db
        .read(|conn| Ok(store::approved_requests(conn, 100_000)?))
        .await
        .map_err(db_error)?;
    Ok(http::ok(
        now,
        rows.iter().map(ops::request_json).collect::<Vec<_>>(),
    ))
}

async fn export_queue(
    svc: &Arc<EnrollService>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let days = svc.effective().policy.cert_lifetime_days;
    let now_ms = now.timestamp_millis();
    let v = svc
        .db
        .read(move |conn| {
            crate::pki::queue::export(conn, "kairos", days, now_ms)
                .map_err(crate::enroll::db::DbError)
        })
        .await
        .map_err(db_error)?;
    Ok(http::ok(now, v))
}

async fn import_queue(
    w: &Writer<'_>,
    b: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    w.principal.require(Permission::PkiQueue)?;
    let client_ca = w.svc.config.auth.client_ca_path().map(str::to_string);
    let actor = w.actor.to_string();
    let now_ms = now.timestamp_millis();
    let results = w
        .svc
        .db
        .write(move |conn| {
            let anchors = crate::pki::queue::anchors(conn, client_ca.as_deref());
            Ok(crate::pki::queue::import(
                conn, &b, &anchors, &actor, now_ms,
            ))
        })
        .await
        .map_err(db_error)?
        .map_err(|e| ApiError::param("body", e))?;
    let results: Vec<Value> = results
        .into_iter()
        .map(|r| {
            w.svc.notify_request(&r.request_id);
            match r.result {
                Ok(serial) => {
                    json!({"request_id": r.request_id, "result": "issued", "serial": serial})
                }
                Err(e) => json!({"request_id": r.request_id, "result": "rejected", "reason": e}),
            }
        })
        .collect();
    Ok(http::ok(
        now,
        json!({"results": results, "summary": summary(&results)}),
    ))
}
