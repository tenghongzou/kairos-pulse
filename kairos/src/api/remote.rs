//! 遠端管理的主控台端點（商業審計 F8；`crates/kairos-proto/REMOTE.md` 第 8 節、`kairos/API.md` 第 17 節）：
//!
//! | 端點 | 權限 |
//! |---|---|
//! | `GET /api/v1/config/schema`、`/config/profiles`、`/config/profiles/{id}`、`…/revisions`、`/config/assignments`、`/config/status`；`POST /config/preview` | `config:read` |
//! | `POST /config/profiles`、`PUT`／`DELETE /config/profiles/{id}`、`POST …/revert`、`POST /config/assignments`、`DELETE /config/assignments/{id}` | `config:write` |
//! | `GET /api/v1/hosts/{agent_id}/config` | `config:read` |
//! | `GET /api/v1/releases`、`/releases/{version}` | `releases:read` |
//! | `POST /api/v1/releases`、`PUT /releases/{version}/artifacts/{filename}`、`DELETE /releases/{version}` | `releases:write` |
//! | `GET /api/v1/rollouts`、`/rollouts/{id}`、`/rollouts/{id}/hosts`；`POST /rollouts/preview` | `rollouts:read` |
//! | `POST /rollouts`、`PUT /rollouts/{id}`、`POST …/submit`、`…/resume`、`…/continue`、`…/cancel`、`…/hosts/{agent_id}/retry`、`…/acknowledge` | `rollouts:write` |
//! | `POST /rollouts/{id}/pause` | `rollouts:pause` |
//! | `GET /api/v1/hosts/{agent_id}/upgrade` | `rollouts:read` |
//!
//! `[remote]` 停用（`config_enabled`／`upgrade_enabled`）或不是 SQLite 時回 503 `remote_unavailable`。

use chrono::{DateTime, Utc};
use hyper::body::{Body as _, Incoming};
use hyper::header::{CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use super::ApiContext;
use super::auth::{Permission, Principal};
use super::db::query_failed;
use super::http::{self, ApiBody, ApiError, Query};
use crate::remote::RemoteService;
use crate::remote::desired::{self, Selector};
use crate::remote::ops::{self, Actor, PreviewChange, ProfileInput};
use crate::remote::rollout::{self, RolloutSpec};

/// JSON 本文的上限（REMOTE.md 第 8 節：1 MiB，發佈檔上傳除外）
const MAX_BODY: usize = 1024 * 1024;

/// 是否為這個模組的路徑
pub fn is_route(rest: &[&str]) -> bool {
    matches!(
        rest,
        ["config", ..] | ["releases", ..] | ["rollouts", ..] | ["hosts", _, "config" | "upgrade"]
    )
}

fn method_not_allowed(allowed: &str) -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        format!("這個端點只接受 {allowed}"),
    )
}

fn unavailable(why: &str) -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "remote_unavailable",
        format!("遠端管理無法使用：{why}"),
    )
}

/// 這個區域的服務（集中設定需要 `config_enabled`，版本庫與推送需要 `upgrade_enabled`）
fn service(ctx: &ApiContext, config_area: bool) -> Result<Arc<RemoteService>, ApiError> {
    let svc = ctx
        .remote
        .clone()
        .ok_or_else(|| unavailable("需要 SQLite 儲存（storage.backend = \"sqlite\"）"))?;
    if config_area && !svc.settings.config_enabled {
        return Err(unavailable(
            "集中設定已停用（remote.config_enabled = false）",
        ));
    }
    if !config_area && !svc.settings.upgrade_enabled {
        return Err(unavailable(
            "遠端升級已停用（remote.upgrade_enabled = false）",
        ));
    }
    Ok(svc)
}

fn actor(p: &Principal, peer: SocketAddr) -> Actor {
    Actor {
        actor: p.actor(),
        ip: Some(peer.ip().to_canonical().to_string()),
    }
}

fn id_of(text: &str, what: &str) -> Result<i64, ApiError> {
    text.parse::<i64>().ok().filter(|i| *i > 0).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("沒有這個{what}"),
        )
    })
}

/// RFC 3339 的時間欄位（`expires_at`）
fn time_field(b: &Value, name: &str) -> Result<Option<i64>, ApiError> {
    match b.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(t)) => DateTime::parse_from_rfc3339(t.trim())
            .map(|t| Some(t.timestamp_millis()))
            .map_err(|_| ApiError::param(name, format!("{name} 必須是 RFC 3339 時間"))),
        Some(_) => Err(ApiError::param(
            name,
            format!("{name} 必須是 RFC 3339 時間"),
        )),
    }
}

fn opt_string(b: &Value, name: &str) -> Result<Option<String>, ApiError> {
    match b.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(ApiError::param(name, format!("{name} 必須是字串"))),
    }
}

fn reject_unknown_fields(b: &Value, allowed: &[&str]) -> Result<(), ApiError> {
    let Value::Object(m) = b else {
        return Err(ApiError::param("body", "本文必須是 JSON 物件"));
    };
    for k in m.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(ApiError::param(k, format!("不認得的欄位 {k}")));
        }
    }
    Ok(())
}

/// 處理 `/api/v1/config/…`、`/releases/…`、`/rollouts/…`、`/hosts/{id}/config|upgrade`（已驗證）
pub async fn handle(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    p: &Principal,
    rest: &[&str],
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let method = req.method().clone();
    let query = Query::parse(req.uri().query())?;
    let get = method == Method::GET || method == Method::HEAD;
    match rest {
        ["hosts", id, "config"] if get => {
            query.reject_unknown(&[], &[])?;
            p.require(Permission::ConfigRead)?;
            host_config(ctx, id, now).await
        }
        ["hosts", id, "upgrade"] if get => {
            query.reject_unknown(&[], &[])?;
            p.require(Permission::RolloutsRead)?;
            host_upgrade(ctx, id, now).await
        }
        ["hosts", ..] => Err(method_not_allowed("GET")),
        ["config", sub @ ..] => config(ctx, peer, req, p, sub, &query, now).await,
        ["releases", sub @ ..] => releases(ctx, peer, req, p, sub, &query, now).await,
        ["rollouts", sub @ ..] => rollouts(ctx, peer, req, p, sub, &query, now).await,
        _ => Err(ApiError::not_found()),
    }
}

// ---- 集中設定 ----

async fn config(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    p: &Principal,
    sub: &[&str],
    query: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let method = req.method().clone();
    let get = method == Method::GET || method == Method::HEAD;
    match (method.as_str(), sub) {
        (_, ["schema"]) if get => {
            query.reject_unknown(&[], &[])?;
            p.require(Permission::ConfigRead)?;
            service(ctx, true)?;
            Ok(http::ok(now, desired::schema_json()))
        }
        (_, ["status"]) if get => {
            query.reject_unknown(&[], &[])?;
            p.require(Permission::ConfigRead)?;
            service(ctx, true)?;
            config_status(ctx, now).await
        }
        (_, ["schema"] | ["status"]) => Err(method_not_allowed("GET")),
        (_, ["profiles"]) if get => {
            query.reject_unknown(&["q"], &[])?;
            p.require(Permission::ConfigRead)?;
            service(ctx, true)?;
            list_profiles(ctx, search_term(query)?, now).await
        }
        ("POST", ["profiles"]) => {
            p.require(Permission::ConfigWrite)?;
            let svc = service(ctx, true)?;
            let b = http::read_json(req, MAX_BODY).await?;
            reject_unknown_fields(&b, &["name", "description", "settings", "reason"])?;
            let input = ProfileInput {
                name: opt_string(&b, "name")?,
                description: opt_string(&b, "description")?,
                settings: b.get("settings").cloned(),
                reason: ops::reason_of(&b)?,
                impact_token: None,
            };
            let v = svc.create_profile(&actor(p, peer), input).await?;
            Ok(http::json(StatusCode::CREATED, now, json!({"data": v})))
        }
        (_, ["profiles"]) => Err(method_not_allowed("GET、POST")),
        (_, ["profiles", id]) if get => {
            query.reject_unknown(&[], &[])?;
            p.require(Permission::ConfigRead)?;
            service(ctx, true)?;
            profile_detail(ctx, id_of(id, "設定檔")?, now).await
        }
        ("PUT", ["profiles", id]) => {
            p.require(Permission::ConfigWrite)?;
            let svc = service(ctx, true)?;
            let id = id_of(id, "設定檔")?;
            let b = http::read_json(req, MAX_BODY).await?;
            reject_unknown_fields(
                &b,
                &["name", "description", "settings", "reason", "impact_token"],
            )?;
            let input = ProfileInput {
                name: opt_string(&b, "name")?,
                description: opt_string(&b, "description")?,
                settings: b.get("settings").cloned().filter(|v| !v.is_null()),
                reason: ops::reason_of(&b)?,
                impact_token: opt_string(&b, "impact_token")?,
            };
            Ok(http::ok(
                now,
                svc.update_profile(&actor(p, peer), id, input, None).await?,
            ))
        }
        ("DELETE", ["profiles", id]) => {
            p.require(Permission::ConfigWrite)?;
            let svc = service(ctx, true)?;
            let id = id_of(id, "設定檔")?;
            let reason = optional_body_reason(req).await?;
            Ok(http::ok(
                now,
                svc.delete_profile(&actor(p, peer), id, reason).await?,
            ))
        }
        (_, ["profiles", _]) => Err(method_not_allowed("GET、PUT、DELETE")),
        (_, ["profiles", id, "revisions"]) if get => {
            query.reject_unknown(&[], &[])?;
            p.require(Permission::ConfigRead)?;
            service(ctx, true)?;
            revisions(ctx, id_of(id, "設定檔")?, now).await
        }
        (_, ["profiles", _, "revisions"]) => Err(method_not_allowed("GET")),
        ("POST", ["profiles", id, "revert"]) => {
            p.require(Permission::ConfigWrite)?;
            let svc = service(ctx, true)?;
            let id = id_of(id, "設定檔")?;
            let b = http::read_json(req, MAX_BODY).await?;
            reject_unknown_fields(&b, &["revision", "reason", "impact_token"])?;
            let revision = b
                .get("revision")
                .and_then(Value::as_i64)
                .filter(|r| *r > 0)
                .ok_or_else(|| ApiError::param("revision", "revision 必須是設定檔的版本號"))?;
            let input = ProfileInput {
                name: None,
                description: None,
                settings: None,
                reason: ops::reason_of(&b)?,
                impact_token: opt_string(&b, "impact_token")?,
            };
            Ok(http::ok(
                now,
                svc.update_profile(&actor(p, peer), id, input, Some(revision))
                    .await?,
            ))
        }
        (_, ["profiles", _, "revert"]) => Err(method_not_allowed("POST")),
        (_, ["assignments"]) if get => {
            query.reject_unknown(&["profile_id", "selector_kind"], &[])?;
            p.require(Permission::ConfigRead)?;
            service(ctx, true)?;
            list_assignments(ctx, query, now).await
        }
        ("POST", ["assignments"]) => {
            p.require(Permission::ConfigWrite)?;
            let svc = service(ctx, true)?;
            let b = http::read_json(req, MAX_BODY).await?;
            reject_unknown_fields(
                &b,
                &[
                    "profile_id",
                    "selector",
                    "priority",
                    "expires_at",
                    "impact_token",
                ],
            )?;
            let profile_id = b
                .get("profile_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| ApiError::param("profile_id", "需要 profile_id"))?;
            let selector = Selector::parse(http::str_field(&b, "selector").unwrap_or(""))
                .map_err(|e| ApiError::param("selector", e))?;
            let priority = match b.get("priority") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_i64()
                        .ok_or_else(|| ApiError::param("priority", "priority 必須是整數"))?,
                ),
            };
            let expires_at = time_field(&b, "expires_at")?;
            let v = svc
                .create_assignment(
                    &actor(p, peer),
                    profile_id,
                    selector,
                    priority,
                    expires_at,
                    opt_string(&b, "impact_token")?,
                )
                .await?;
            Ok(http::json(StatusCode::CREATED, now, json!({"data": v})))
        }
        (_, ["assignments"]) => Err(method_not_allowed("GET、POST")),
        ("DELETE", ["assignments", id]) => {
            p.require(Permission::ConfigWrite)?;
            let svc = service(ctx, true)?;
            let id = id_of(id, "指派")?;
            let token = match body_json_optional(req).await? {
                Some(b) => opt_string(&b, "impact_token")?,
                None => query.get("impact_token").map(str::to_string),
            };
            Ok(http::ok(
                now,
                svc.delete_assignment(&actor(p, peer), id, token).await?,
            ))
        }
        (_, ["assignments", _]) => Err(method_not_allowed("DELETE")),
        ("POST", ["preview"]) => {
            p.require(Permission::ConfigRead)?;
            let svc = service(ctx, true)?;
            let b = http::read_json(req, MAX_BODY).await?;
            Ok(http::ok(
                now,
                svc.preview_change(preview_change(&b)?).await?,
            ))
        }
        (_, ["preview"]) => Err(method_not_allowed("POST")),
        _ => Err(ApiError::not_found()),
    }
}

/// `POST /api/v1/config/preview` 的本文：`{"profile_id", "settings"}`（修改設定檔）、
/// `{"assign": {"profile_id", "selector", "priority", "expires_at"}}`（新指派）或 `{"unassign": id}`
fn preview_change(b: &Value) -> Result<PreviewChange, ApiError> {
    reject_unknown_fields(b, &["profile_id", "settings", "assign", "unassign"])?;
    if let Some(a) = b.get("assign").filter(|v| !v.is_null()) {
        reject_unknown_fields(a, &["profile_id", "selector", "priority", "expires_at"])?;
        let selector = Selector::parse(http::str_field(a, "selector").unwrap_or(""))
            .map_err(|e| ApiError::param("assign.selector", e))?;
        let priority = a
            .get("priority")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| selector.default_priority());
        return Ok(PreviewChange::Assign {
            profile_id: a
                .get("profile_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| ApiError::param("assign.profile_id", "需要 profile_id"))?,
            selector,
            priority,
            expires_at: time_field(a, "expires_at")?,
        });
    }
    if let Some(id) = b.get("unassign").filter(|v| !v.is_null()) {
        return Ok(PreviewChange::Unassign {
            id: id
                .as_i64()
                .ok_or_else(|| ApiError::param("unassign", "unassign 必須是指派的 id"))?,
        });
    }
    let id = b.get("profile_id").and_then(Value::as_i64).ok_or_else(|| {
        ApiError::param(
            "profile_id",
            "需要 profile_id＋settings、assign 或 unassign",
        )
    })?;
    let (settings, _) = desired::check_profile_settings(b.get("settings").unwrap_or(&json!({})))
        .map_err(|e| ops::config_invalid(&e))?;
    Ok(PreviewChange::Profile { id, settings })
}

/// DELETE 的本文是選用的（`reason`）
async fn body_json_optional(req: Request<Incoming>) -> Result<Option<Value>, ApiError> {
    let has_body = req.headers().contains_key(TRANSFER_ENCODING)
        || req
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() != "0");
    if !has_body {
        return Ok(None);
    }
    Ok(Some(http::read_json(req, MAX_BODY).await?))
}

async fn optional_body_reason(req: Request<Incoming>) -> Result<Option<String>, ApiError> {
    match body_json_optional(req).await? {
        Some(b) => ops::reason_of(&b),
        None => Ok(None),
    }
}

/// 清單的 `q`（全域搜尋）：去掉前後空白、轉小寫；空字串視為沒有；最多 128 個字元
fn search_term(query: &Query) -> Result<Option<String>, ApiError> {
    match query.get("q").map(str::trim) {
        None | Some("") => Ok(None),
        Some(q) if q.chars().count() > 128 => Err(ApiError::param("q", "q 最多 128 個字元")),
        Some(q) => Ok(Some(q.to_lowercase())),
    }
}

/// 每個設定檔的指派數與比對到的主機數
async fn list_profiles(
    ctx: &Arc<ApiContext>,
    q: Option<String>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let now_ms = now.timestamp_millis();
    let data = ctx
        .pool
        .read("設定檔", move |s| {
            let profiles = s.list_profiles().map_err(query_failed("設定檔"))?;
            // `q`：名稱或說明的子字串（不分大小寫；全域搜尋）
            let profiles: Vec<_> = profiles
                .into_iter()
                .filter(|p| {
                    q.as_deref().is_none_or(|q| {
                        p.name.to_lowercase().contains(q)
                            || p.description.to_lowercase().contains(q)
                    })
                })
                .collect();
            let snap = desired::ConfigSnapshot::load(s).map_err(query_failed("設定檔"))?;
            let targets = s.config_targets().map_err(query_failed("設定檔"))?;
            Ok(profiles
                .iter()
                .map(|p| {
                    let assigns: Vec<&desired::Assignment> = snap
                        .assignments
                        .iter()
                        .filter(|a| a.profile_id == p.id)
                        .collect();
                    let hosts = targets
                        .iter()
                        .filter(|t| {
                            assigns.iter().any(|a| {
                                a.selector.matches(t) && a.expires_at.is_none_or(|e| e > now_ms)
                            })
                        })
                        .count();
                    ops::profile_json(p, assigns.len(), hosts)
                })
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(http::ok(now, data))
}

async fn profile_detail(
    ctx: &Arc<ApiContext>,
    id: i64,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let now_ms = now.timestamp_millis();
    let data = ctx
        .pool
        .read("設定檔", move |s| {
            let Some(p) = s.get_profile(id).map_err(query_failed("設定檔"))? else {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "profile_not_found",
                    format!("沒有設定檔 #{id}"),
                ));
            };
            let targets = s.config_targets().map_err(query_failed("設定檔"))?;
            let assigns: Vec<_> = s
                .list_assignments()
                .map_err(query_failed("設定檔"))?
                .into_iter()
                .filter(|a| a.profile_id == id)
                .collect();
            let mut total = std::collections::HashSet::new();
            let list: Vec<Value> = assigns
                .iter()
                .map(|a| {
                    let sel = Selector::from_db(&a.selector_kind, &a.selector_value);
                    let n = targets
                        .iter()
                        .filter(|t| {
                            sel.as_ref().is_some_and(|x| x.matches(t))
                                && a.expires_at.is_none_or(|e| e > now_ms)
                        })
                        .inspect(|t| {
                            total.insert(t.agent_id.clone());
                        })
                        .count();
                    ops::assignment_json(a, Some(&p.name), n)
                })
                .collect();
            let mut v = ops::profile_json(&p, assigns.len(), total.len());
            v["assignment_list"] = json!(list);
            Ok(v)
        })
        .await?;
    Ok(http::ok(now, data))
}

async fn revisions(
    ctx: &Arc<ApiContext>,
    id: i64,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let data = ctx
        .pool
        .read("設定檔的版本", move |s| {
            if s.get_profile(id)
                .map_err(query_failed("設定檔的版本"))?
                .is_none()
            {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "profile_not_found",
                    format!("沒有設定檔 #{id}"),
                ));
            }
            let revs = s
                .profile_revisions(id)
                .map_err(query_failed("設定檔的版本"))?;
            // 每一版與前一版的差異（由新到舊）
            let list: Vec<Value> = revs
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let cur = desired::parse_settings(&r.settings);
                    let prev = revs
                        .get(i + 1)
                        .map(|p| desired::parse_settings(&p.settings));
                    json!({
                        "revision": r.revision,
                        "settings": cur,
                        "description": r.description,
                        "changes": prev.map(|p| changes(&p, &cur)),
                        "changed_at": http::ts_ms(r.changed_at),
                        "changed_by": r.changed_by,
                        "reason": r.reason,
                    })
                })
                .collect();
            Ok(list)
        })
        .await?;
    Ok(http::ok(now, data))
}

/// 兩版之間改變的鍵：`{"<鍵>": {"before", "after"}}`
fn changes(before: &Map<String, Value>, after: &Map<String, Value>) -> Value {
    let flat = |m: &Map<String, Value>| -> BTreeMap<String, Value> {
        kairos_proto::mgmt::settings::flatten(m)
            .map(|f| f.into_iter().map(|(k, v)| (k, v.clone())).collect())
            .unwrap_or_default()
    };
    let (a, b) = (flat(before), flat(after));
    let mut out = Map::new();
    for k in a.keys().chain(b.keys()) {
        if a.get(k) != b.get(k) && !out.contains_key(k) {
            out.insert(k.clone(), json!({"before": a.get(k), "after": b.get(k)}));
        }
    }
    Value::Object(out)
}

async fn list_assignments(
    ctx: &Arc<ApiContext>,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let profile_id = match q.get("profile_id") {
        Some(v) => Some(
            v.parse::<i64>()
                .map_err(|_| ApiError::param("profile_id", "profile_id 必須是整數"))?,
        ),
        None => None,
    };
    let kind = q.get("selector_kind").map(str::to_string);
    if kind
        .as_deref()
        .is_some_and(|k| !matches!(k, "global" | "group" | "tag" | "host"))
    {
        return Err(ApiError::param(
            "selector_kind",
            "selector_kind 必須是 global、group、tag 或 host",
        ));
    }
    let now_ms = now.timestamp_millis();
    let data = ctx
        .pool
        .read("指派", move |s| {
            let names: HashMap<i64, String> = s
                .list_profiles()
                .map_err(query_failed("指派"))?
                .into_iter()
                .map(|p| (p.id, p.name))
                .collect();
            let targets = s.config_targets().map_err(query_failed("指派"))?;
            Ok(s.list_assignments()
                .map_err(query_failed("指派"))?
                .iter()
                .filter(|a| profile_id.is_none_or(|p| a.profile_id == p))
                .filter(|a| kind.as_deref().is_none_or(|k| a.selector_kind == k))
                .map(|a| {
                    let sel = Selector::from_db(&a.selector_kind, &a.selector_value);
                    let n = targets
                        .iter()
                        .filter(|t| {
                            sel.as_ref().is_some_and(|x| x.matches(t))
                                && a.expires_at.is_none_or(|e| e > now_ms)
                        })
                        .count();
                    ops::assignment_json(a, names.get(&a.profile_id).map(String::as_str), n)
                })
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(http::ok(now, data))
}

/// 一台主機的設定狀態（[`desired::config_state`]）
fn config_state(
    t: &crate::remote::store::ConfigTarget,
    row: Option<&crate::remote::store::HostConfigRow>,
) -> &'static str {
    let input = row.map(|r| desired::ConfigStateInput {
        desired_rev: r.desired_rev,
        applied_rev: r.applied_rev,
        state: r.state.clone(),
        overridden: desired::has_overridden(r.overridden.as_deref()),
    });
    desired::config_state(
        t.agent_version.as_deref(),
        t.install_config.as_deref(),
        input.as_ref(),
    )
}

async fn config_status(
    ctx: &Arc<ApiContext>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let data = ctx
        .pool
        .read("集中設定的同步狀態", move |s| {
            let targets = s
                .config_targets()
                .map_err(query_failed("集中設定的同步狀態"))?;
            let rows: HashMap<String, crate::remote::store::HostConfigRow> = s
                .host_configs()
                .map_err(query_failed("集中設定的同步狀態"))?
                .into_iter()
                .map(|r| (r.agent_id.clone(), r))
                .collect();
            let rev = s
                .remote_config_rev()
                .map_err(query_failed("集中設定的同步狀態"))?;
            let mut counts: BTreeMap<&'static str, usize> = [
                "in_sync",
                "pending",
                "rejected",
                "rolled_back",
                "overridden",
                "disabled",
                "unsupported",
                "never_reported",
            ]
            .into_iter()
            .map(|k| (k, 0))
            .collect();
            for t in &targets {
                *counts
                    .entry(config_state(t, rows.get(&t.agent_id)))
                    .or_default() += 1;
            }
            Ok(json!({"config_rev": rev, "hosts": targets.len(), "counts": counts}))
        })
        .await?;
    Ok(http::ok(now, data))
}

fn parse_json_text(text: Option<&str>) -> Value {
    text.and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or(Value::Null)
}

/// `GET /api/v1/hosts/{agent_id}/config`
async fn host_config(
    ctx: &Arc<ApiContext>,
    id: &str,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let agent_id = super::hosts::parse_agent_id(id)?;
    let a = agent_id.clone();
    let data = ctx
        .pool
        .read("主機的設定", move |s| {
            let target = s.config_target(&a).map_err(query_failed("主機的設定"))?;
            let host = s.upgrade_host(&a).map_err(query_failed("主機的設定"))?;
            let row = s.host_config(&a).map_err(query_failed("主機的設定"))?;
            let Some(t) = target else {
                return Err(ApiError::host_not_found(&a));
            };
            let desired = row.as_ref().map(|r| desired::parse_settings(&r.desired)).unwrap_or_default();
            let sources: BTreeMap<String, String> = row
                .as_ref()
                .and_then(|r| serde_json::from_str(&r.sources).ok())
                .unwrap_or_default();
            let keys: Vec<Value> = kairos_proto::mgmt::settings::flatten(&desired)
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| json!({"key": k, "value": v, "source": sources.get(&k)}))
                .collect();
            Ok(json!({
                "agent_id": a,
                "config_state": config_state(&t, row.as_ref()),
                "desired": desired,
                "desired_keys": keys,
                "desired_rev": row.as_ref().map_or(0, |r| r.desired_rev),
                "desired_hash": row.as_ref().map(|r| r.desired_hash.clone()).filter(|h| !h.is_empty()),
                "applied_rev": row.as_ref().map_or(0, |r| r.applied_rev),
                "applied_hash": row.as_ref().map(|r| r.applied_hash.clone()).filter(|h| !h.is_empty()),
                "state": row.as_ref().and_then(|r| r.state.clone()),
                "effective": parse_json_text(row.as_ref().and_then(|r| r.effective.as_deref())),
                "overridden": parse_json_text(row.as_ref().and_then(|r| r.overridden.as_deref())),
                "ignored": parse_json_text(row.as_ref().and_then(|r| r.ignored.as_deref())),
                "errors": parse_json_text(row.as_ref().and_then(|r| r.errors.as_deref())),
                "code": row.as_ref().and_then(|r| r.code.clone()),
                "reported_at": row.as_ref().and_then(|r| r.reported_at).map(http::ts_ms),
                "agent_version": t.agent_version,
                "install": host.map(|h| json!({"method": h.install_method, "upgrade": h.install_upgrade, "config": h.install_config})),
            }))
        })
        .await?;
    let mut data = data;
    if let Some(svc) = &ctx.remote {
        data["config_negotiated"] = json!(svc.session(&agent_id).is_some_and(|s| s.config));
    }
    Ok(http::ok(now, data))
}

/// `GET /api/v1/hosts/{agent_id}/upgrade`：安裝方式、目前版本、最近 10 次升級
async fn host_upgrade(
    ctx: &Arc<ApiContext>,
    id: &str,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let agent_id = super::hosts::parse_agent_id(id)?;
    let a = agent_id.clone();
    let data = ctx
        .pool
        .read("主機的升級紀錄", move |s| {
            let Some(h) = s.upgrade_host(&a).map_err(query_failed("主機的升級紀錄"))? else {
                return Err(ApiError::host_not_found(&a));
            };
            let list = s.host_upgrades(&a, 10).map_err(query_failed("主機的升級紀錄"))?;
            let rollback_failed = list.iter().any(|u| u.state == rollout::HOST_ROLLBACK_FAILED && u.acknowledged_at.is_none());
            Ok(json!({
                "agent_id": a,
                "agent_version": h.agent_version,
                "install": {"method": h.install_method, "upgrade": h.install_upgrade, "config": h.install_config},
                "arch": rollout::arch_of(h.os.as_deref()),
                "rollback_failed": rollback_failed,
                "upgrades": list.iter().map(rollout::host_json).collect::<Vec<_>>(),
            }))
        })
        .await?;
    let mut data = data;
    if let Some(svc) = &ctx.remote {
        // 連線中：版本與 install 以 hello 為準（與推送的資格分類相同；清冊每 30 秒才同步）
        if let Some(l) = svc.live_hello(&agent_id) {
            data["agent_version"] = json!(l.agent_version);
            for (k, v) in [
                ("method", &l.install_method),
                ("upgrade", &l.install_upgrade),
                ("config", &l.install_config),
            ] {
                if v.is_some() {
                    data["install"][k] = json!(v);
                }
            }
            if let Some(arch) = rollout::arch_of(l.os.as_deref()) {
                data["arch"] = json!(arch);
            }
        }
        data["upgrade_negotiated"] = json!(svc.session(&agent_id).is_some_and(|s| s.upgrade));
        data["offered_upgrade_id"] = json!(svc.offer_of(&agent_id));
    }
    Ok(http::ok(now, data))
}

// ---- 版本庫 ----

async fn releases(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    p: &Principal,
    sub: &[&str],
    query: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let method = req.method().clone();
    let get = method == Method::GET || method == Method::HEAD;
    match (method.as_str(), sub) {
        (_, []) if get => {
            query.reject_unknown(&[], &[])?;
            p.require(Permission::ReleasesRead)?;
            service(ctx, false)?;
            list_releases(ctx, None, now).await
        }
        ("POST", []) => {
            p.require(Permission::ReleasesWrite)?;
            let svc = service(ctx, false)?;
            let b = http::read_json(req, MAX_BODY).await?;
            reject_unknown_fields(
                &b,
                &["sha256sums", "signature", "manifest", "source", "base_url"],
            )?;
            // 外部 URL 的版本庫（0.10.0，REMOTE.md 第 4.1.2 節）：source = external 時必須有 base_url
            let base_url = opt_string(&b, "base_url")?;
            let base_url = match (http::str_field(&b, "source").unwrap_or("hosted"), base_url) {
                ("hosted", None) => None,
                ("hosted", Some(_)) => {
                    return Err(ApiError::param(
                        "base_url",
                        "base_url 只用於外部 URL 的版本庫（source: external）",
                    ));
                }
                ("external", Some(u)) => Some(u),
                ("external", None) => {
                    return Err(ApiError::param(
                        "base_url",
                        "外部 URL 的版本庫（source: external）需要 base_url（https://…/）",
                    ));
                }
                _ => {
                    return Err(ApiError::param(
                        "source",
                        "source 必須是 hosted 或 external",
                    ));
                }
            };
            let sums = opt_string(&b, "sha256sums")?.ok_or_else(|| {
                ApiError::param("sha256sums", "需要 sha256sums（SHA256SUMS 的全文）")
            })?;
            let sig = opt_string(&b, "signature")?.ok_or_else(|| {
                ApiError::param("signature", "需要 signature（SHA256SUMS.sig 的全文）")
            })?;
            let manifest = opt_string(&b, "manifest")?;
            let v = svc
                .import_release_from(&actor(p, peer), sums, sig, manifest, base_url)
                .await?;
            Ok(http::json(StatusCode::CREATED, now, json!({"data": v})))
        }
        (_, []) => Err(method_not_allowed("GET、POST")),
        (_, [version]) if get => {
            query.reject_unknown(&[], &[])?;
            p.require(Permission::ReleasesRead)?;
            service(ctx, false)?;
            list_releases(ctx, Some(version.to_string()), now).await
        }
        ("DELETE", [version]) => {
            p.require(Permission::ReleasesWrite)?;
            let svc = service(ctx, false)?;
            let b = body_json_optional(req).await?.unwrap_or(json!({}));
            reject_unknown_fields(&b, &["confirm", "reason"])?;
            // L2：輸入版本號
            if http::str_field(&b, "confirm").map(str::trim) != Some(version) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "confirmation_required",
                    "刪除發佈版本需要在 confirm 輸入版本號（L2 確認）",
                )
                .with_details(json!({"confirm_text": version, "confirm_level": "L2"})));
            }
            let reason = ops::reason_of(&b)?;
            Ok(http::ok(
                now,
                svc.delete_release(&actor(p, peer), version.to_string(), reason)
                    .await?,
            ))
        }
        (_, [_]) => Err(method_not_allowed("GET、DELETE")),
        ("PUT", [version, "artifacts", filename]) => {
            p.require(Permission::ReleasesWrite)?;
            let svc = service(ctx, false)?;
            upload_artifact(&svc, peer, p, req, version, filename, now).await
        }
        (_, [_, "artifacts", _]) => Err(method_not_allowed("PUT")),
        _ => Err(ApiError::not_found()),
    }
}

/// `PUT /api/v1/releases/{version}/artifacts/{filename}`：本文為檔案內容（`application/octet-stream`，
/// 需要 `Content-Length`，上限 `remote.max_release_bytes`）
async fn upload_artifact(
    svc: &Arc<RemoteService>,
    peer: SocketAddr,
    p: &Principal,
    req: Request<Incoming>,
    version: &str,
    filename: &str,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let ct = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if ct != "application/octet-stream" {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "發佈檔的本文必須是 application/octet-stream",
        ));
    }
    if req.headers().contains_key(TRANSFER_ENCODING) {
        return Err(ApiError::param(
            "content-length",
            "請以 Content-Length 送出本文（不接受 chunked）",
        ));
    }
    let max = svc.settings.max_release_bytes;
    let len = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .ok_or_else(|| ApiError::param("content-length", "需要 Content-Length"))?;
    if len == 0 || len > max {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("發佈檔必須是 1～{max} bytes（remote.max_release_bytes）"),
        ));
    }
    if kairos_proto::release::ReleaseFile::parse(filename).is_err() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "artifact_not_listed",
            format!("{filename} 不是 Pulse 的發佈檔名"),
        )
        .with_product_code(crate::remote::CODE_IMPORT_FAILED));
    }
    let dir = svc.releases_dir.clone();
    let mut up =
        tokio::task::spawn_blocking(move || crate::remote::releases::Upload::create(&dir, max))
            .await
            .map_err(|_| ApiError::internal())?
            .map_err(|e| {
                log::warn!("版本庫：無法建立暫存檔：{e}");
                ApiError::internal()
            })?;
    let mut body = req.into_body();
    let mut buf: Vec<u8> = Vec::with_capacity(1024 * 1024);
    let mut received: u64 = 0;
    loop {
        let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        let done = match frame {
            None => true,
            Some(Err(_)) => return Err(ApiError::param("body", "本文無法讀取（上傳中斷）")),
            Some(Ok(f)) => {
                if let Ok(d) = f.into_data() {
                    received += d.len() as u64;
                    if received > len {
                        return Err(ApiError::param("body", "本文比 Content-Length 長"));
                    }
                    buf.extend_from_slice(&d);
                }
                false
            }
        };
        if buf.len() >= 1024 * 1024 || (done && !buf.is_empty()) {
            let data = std::mem::take(&mut buf);
            let (back, r) = tokio::task::spawn_blocking(move || {
                let r = up.write(&data);
                (up, r)
            })
            .await
            .map_err(|_| ApiError::internal())?;
            up = back;
            if let Err(e) = r {
                return Err(match e {
                    None => ApiError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "payload_too_large",
                        format!("發佈檔超過 {max} bytes"),
                    ),
                    Some(e) => {
                        log::warn!("版本庫：寫入暫存檔失敗：{e}");
                        ApiError::internal()
                    }
                });
            }
        }
        if done {
            break;
        }
    }
    if received != len {
        return Err(ApiError::param(
            "body",
            "本文比 Content-Length 短（上傳中斷）",
        ));
    }
    let (up, fin) = tokio::task::spawn_blocking(move || {
        let r = up.finish();
        (up, r)
    })
    .await
    .map_err(|_| ApiError::internal())?;
    let (sha, size) = fin.map_err(|e| {
        log::warn!("版本庫：寫入暫存檔失敗：{e}");
        ApiError::internal()
    })?;
    let v = svc
        .store_artifact(
            &actor(p, peer),
            version.to_string(),
            filename.to_string(),
            up,
            sha,
            size,
        )
        .await?;
    Ok(http::ok(now, v))
}

async fn list_releases(
    ctx: &Arc<ApiContext>,
    version: Option<String>,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let detail = version.is_some();
    let data = ctx
        .pool
        .read("Pulse 版本庫", move |s| {
            let releases = s.list_releases().map_err(query_failed("Pulse 版本庫"))?;
            let hosts = s.upgrade_hosts().map_err(query_failed("Pulse 版本庫"))?;
            let mut out = Vec::new();
            for r in releases
                .iter()
                .filter(|r| version.as_deref().is_none_or(|v| v == r.version))
            {
                let artifacts = s
                    .release_artifacts(r.id)
                    .map_err(query_failed("Pulse 版本庫"))?;
                let on = hosts
                    .iter()
                    .filter(|h| h.agent_version.as_deref() == Some(r.version.as_str()))
                    .count();
                let rollback = ops::rollback_target_count(&hosts, &r.version, &artifacts);
                let rollouts = s
                    .rollouts_using_release(r.id)
                    .map_err(query_failed("Pulse 版本庫"))?;
                let mut v = ops::release_json(r, &artifacts, Some((on, rollback, rollouts)));
                if version.is_some() {
                    // 詳情：各平台與安裝方式的主機數（目前執行這個版本）
                    let mut by: BTreeMap<String, usize> = BTreeMap::new();
                    for h in hosts
                        .iter()
                        .filter(|h| h.agent_version.as_deref() == Some(r.version.as_str()))
                    {
                        let key = format!(
                            "{}/{}",
                            h.install_method.as_deref().unwrap_or("unknown"),
                            rollout::arch_of(h.os.as_deref()).unwrap_or_else(|| "unknown".into())
                        );
                        *by.entry(key).or_default() += 1;
                    }
                    v["hosts_by_platform"] = json!(by);
                    v["sha256sums"] = json!(r.sha256sums);
                }
                out.push(v);
            }
            Ok(out)
        })
        .await?;
    if detail {
        let v = data.into_iter().next().ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "release_not_found",
                "版本庫沒有這個版本",
            )
        })?;
        return Ok(http::ok(now, v));
    }
    let trust_configured = ctx
        .remote
        .as_ref()
        .is_some_and(|s| !s.settings.allowed_signers.trim().is_empty());
    let downloads = ctx.remote.as_ref().map_or(0, |s| s.downloads_in_progress());
    Ok(http::json(
        StatusCode::OK,
        now,
        json!({"data": data, "meta": {
            "allowed_signers_configured": trust_configured,
            "downloads_in_progress": downloads,
            "max_concurrent_downloads": ctx.remote.as_ref().map(|s| s.settings.max_concurrent_downloads),
        }}),
    ))
}

// ---- 推送 ----

async fn rollouts(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    p: &Principal,
    sub: &[&str],
    query: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let method = req.method().clone();
    let get = method == Method::GET || method == Method::HEAD;
    let who = actor(p, peer);
    match (method.as_str(), sub) {
        (_, []) if get => {
            query.reject_unknown(&["state", "q"], &[])?;
            p.require(Permission::RolloutsRead)?;
            service(ctx, false)?;
            let q = search_term(query)?;
            let state = query.get("state").map(str::to_string);
            if state.as_deref().is_some_and(|s| {
                !matches!(
                    s,
                    "draft"
                        | "pending_approval"
                        | "scheduled"
                        | "running"
                        | "paused"
                        | "completed"
                        | "cancelled"
                )
            }) {
                return Err(ApiError::param(
                    "state",
                    "state 必須是 draft、pending_approval、scheduled、running、paused、completed 或 cancelled",
                ));
            }
            let now_ms = now.timestamp_millis();
            let approval = ctx
                .remote
                .as_ref()
                .map(|s| s.settings.rollout_approval.clone());
            let (data, meta) = ctx
                .pool
                .read("升級推送", move |s| {
                    let list = s
                        .list_rollouts(state.as_deref())
                        .map_err(query_failed("升級推送"))?;
                    // `q`：名稱、編號或目標版本的子字串（不分大小寫；全域搜尋）
                    let list: Vec<_> = list
                        .into_iter()
                        .filter(|r| {
                            q.as_deref().is_none_or(|q| {
                                r.name.to_lowercase().contains(q)
                                    || r.id.to_lowercase().contains(q)
                                    || r.target_version.to_lowercase().contains(q)
                            })
                        })
                        .collect();
                    let mut out = Vec::new();
                    for r in &list {
                        let hosts = s.rollout_hosts(&r.id).map_err(query_failed("升級推送"))?;
                        out.push(crate::remote::rollout_json(r, &hosts, now_ms, false));
                    }
                    // 需要人處理的數量（SideNav 的計數泡泡；不受 state 篩選影響）
                    let paused = s
                        .list_rollouts(Some(rollout::ROLLOUT_PAUSED))
                        .map_err(query_failed("升級推送"))?
                        .len();
                    let failures = s
                        .unacknowledged_rollback_failures()
                        .map_err(query_failed("升級推送"))?
                        .len();
                    // 雙人覆核（0.10）：待核准的推送數與目前的設定
                    let pending = s
                        .list_rollouts(Some(rollout::ROLLOUT_PENDING_APPROVAL))
                        .map_err(query_failed("升級推送"))?
                        .len();
                    Ok((
                        out,
                        json!({
                            "paused": paused,
                            "rollback_failed_unacknowledged": failures,
                            "pending_approval": pending,
                            "rollout_approval": approval,
                        }),
                    ))
                })
                .await?;
            Ok(http::json(
                StatusCode::OK,
                now,
                json!({"data": data, "meta": meta}),
            ))
        }
        ("POST", []) => {
            p.require(Permission::RolloutsWrite)?;
            let svc = service(ctx, false)?;
            let b = http::read_json(req, MAX_BODY).await?;
            let spec = RolloutSpec::parse(&b).map_err(|(f, m)| ApiError::param(&f, m))?;
            let v = svc.create_rollout(&who, spec).await?;
            Ok(http::json(StatusCode::CREATED, now, json!({"data": v})))
        }
        (_, []) => Err(method_not_allowed("GET、POST")),
        ("POST", ["preview"]) => {
            p.require(Permission::RolloutsRead)?;
            let svc = service(ctx, false)?;
            let b = http::read_json(req, MAX_BODY).await?;
            let spec = RolloutSpec::parse(&b).map_err(|(f, m)| ApiError::param(&f, m))?;
            Ok(http::ok(now, svc.preview_rollout(spec, None).await?))
        }
        (_, ["preview"]) => Err(method_not_allowed("POST")),
        (_, [id]) if get => {
            query.reject_unknown(&[], &[])?;
            p.require(Permission::RolloutsRead)?;
            let svc = service(ctx, false)?;
            rollout_detail(ctx, &svc, id.to_string(), now).await
        }
        ("PUT", [id]) => {
            p.require(Permission::RolloutsWrite)?;
            let svc = service(ctx, false)?;
            let b = http::read_json(req, MAX_BODY).await?;
            let spec = RolloutSpec::parse(&b).map_err(|(f, m)| ApiError::param(&f, m))?;
            Ok(http::ok(
                now,
                svc.update_rollout(&who, id.to_string(), spec).await?,
            ))
        }
        (_, [_]) => Err(method_not_allowed("GET、PUT")),
        (_, [id, "hosts"]) if get => {
            query.reject_unknown(
                &["wave", "state", "reason", "code", "q", "limit", "cursor"],
                &[],
            )?;
            p.require(Permission::RolloutsRead)?;
            service(ctx, false)?;
            rollout_hosts(ctx, id.to_string(), query, now).await
        }
        (_, [_, "hosts"]) => Err(method_not_allowed("GET")),
        (
            "POST",
            [
                id,
                action @ ("submit" | "pause" | "resume" | "continue" | "cancel"),
            ],
        ) => {
            if *action == "pause" {
                p.require(Permission::RolloutsPause)?;
            } else {
                p.require(Permission::RolloutsWrite)?;
            }
            let svc = service(ctx, false)?;
            let b = body_json_optional(req).await?.unwrap_or(json!({}));
            let id = id.to_string();
            let reason = ops::reason_of(&b)?;
            let need_reason =
                |r: Option<String>| r.ok_or_else(|| ApiError::param("reason", "需要 reason"));
            let v = match *action {
                "submit" => {
                    reject_unknown_fields(&b, &["confirm", "reason"])?;
                    svc.submit_rollout(&who, id, opt_string(&b, "confirm")?, reason)
                        .await?
                }
                "pause" => {
                    reject_unknown_fields(&b, &["reason"])?;
                    svc.pause_rollout(&who, id, need_reason(reason)?).await?
                }
                "cancel" => {
                    reject_unknown_fields(&b, &["reason"])?;
                    svc.cancel_rollout(&who, id, need_reason(reason)?).await?
                }
                "resume" => svc.resume_rollout(&who, id).await?,
                _ => svc.continue_rollout(&who, id).await?,
            };
            Ok(http::ok(now, v))
        }
        // 雙人覆核（REMOTE.md 第 5.4 節）：需要 rollouts:approve 與以帳號登入的工作階段（API token 不能核准、
        // 退回）；核准者不得是建立者或送出者（403 self_approval，KRS-E6106）
        ("POST", [id, action @ ("approve" | "reject")]) => {
            p.require(Permission::RolloutsApprove)?;
            if !matches!(p.kind, super::auth::PrincipalKind::Session { .. }) {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "session_required",
                    "核准與退回推送需要以帳號登入的工作階段（API token 不能使用）",
                ));
            }
            let svc = service(ctx, false)?;
            let b = body_json_optional(req).await?.unwrap_or(json!({}));
            reject_unknown_fields(&b, &["reason"])?;
            let reason = ops::reason_of(&b)?;
            let v = if *action == "approve" {
                svc.approve_rollout(&who, id.to_string()).await?
            } else {
                let reason = reason.ok_or_else(|| ApiError::param("reason", "需要 reason"))?;
                svc.reject_rollout(&who, id.to_string(), reason).await?
            };
            Ok(http::ok(now, v))
        }
        (
            _,
            [
                _,
                "submit" | "pause" | "resume" | "continue" | "cancel" | "approve" | "reject",
            ],
        ) => Err(method_not_allowed("POST")),
        ("POST", [id, "hosts", agent, action @ ("retry" | "acknowledge")]) => {
            p.require(Permission::RolloutsWrite)?;
            let svc = service(ctx, false)?;
            let agent = super::hosts::parse_agent_id(agent)?;
            let b = body_json_optional(req).await?.unwrap_or(json!({}));
            let v = if *action == "retry" {
                svc.retry_host(&who, id.to_string(), agent).await?
            } else {
                reject_unknown_fields(&b, &["note"])?;
                svc.acknowledge_host(&who, id.to_string(), agent, opt_string(&b, "note")?)
                    .await?
            };
            Ok(http::ok(now, v))
        }
        (_, [_, "hosts", _, "retry" | "acknowledge"]) => Err(method_not_allowed("POST")),
        _ => Err(ApiError::not_found()),
    }
}

async fn rollout_detail(
    ctx: &Arc<ApiContext>,
    svc: &Arc<RemoteService>,
    id: String,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let now_ms = now.timestamp_millis();
    let i = id.clone();
    let (r, v) = ctx
        .pool
        .read("升級推送", move |s| {
            let Some(r) = s.get_rollout(&i).map_err(query_failed("升級推送"))? else {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "rollout_not_found",
                    format!("沒有推送 {i}"),
                ));
            };
            let hosts = s.rollout_hosts(&r.id).map_err(query_failed("升級推送"))?;
            let v = crate::remote::rollout_json(&r, &hosts, now_ms, true);
            Ok((r, v))
        })
        .await?;
    let mut v = v;
    if r.state == rollout::ROLLOUT_DRAFT {
        // 草稿：資格分類的即時預覽（對象在送出時才固定）
        let spec = RolloutSpec {
            name: r.name.clone(),
            target_version: r.target_version.clone(),
            selector: rollout::selector_from_db(&r.selector),
            waves: rollout::waves_from_db(&r.waves),
            options: rollout::options_from_db(&r.options),
            schedule: crate::remote::schedule::ScheduleSpec::from_db(&r.schedule),
        };
        v["preview"] = svc.preview_rollout(spec, Some(r.id.clone())).await?;
    }
    Ok(http::ok(now, v))
}

/// `GET /api/v1/rollouts/{id}/hosts`：分頁（`limit` 1～1000、`cursor`），篩選 `wave`、`state`、`reason`、`code`、`q`
async fn rollout_hosts(
    ctx: &Arc<ApiContext>,
    id: String,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let limit = match q.get("limit") {
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=1000).contains(n))
            .ok_or_else(|| ApiError::param("limit", "limit 必須介於 1..=1000"))?,
        None => 100,
    };
    let after: Option<(i64, i64, i64)> = match q.get("cursor") {
        Some(c) => Some(
            http::b64url_decode(c)
                .and_then(|b| String::from_utf8(b).ok())
                .and_then(|t| {
                    let p: Vec<i64> = t.split(',').filter_map(|x| x.parse().ok()).collect();
                    (p.len() == 3).then(|| (p[0], p[1], p[2]))
                })
                .ok_or_else(|| {
                    ApiError::new(StatusCode::BAD_REQUEST, "invalid_cursor", "cursor 無法解析")
                })?,
        ),
        None => None,
    };
    let wave = match q.get("wave") {
        Some(w) => Some(
            w.parse::<i64>()
                .map_err(|_| ApiError::param("wave", "wave 必須是整數"))?,
        ),
        None => None,
    };
    let states = q.list("state");
    for s in &states {
        if !rollout::HOST_STATES.contains(&s.as_str()) {
            return Err(ApiError::param("state", format!("不認得的狀態 {s}")));
        }
    }
    let reason = q.get("reason").map(str::to_string);
    let code = q.get("code").map(str::to_string);
    let text = q.get("q").map(|t| t.to_lowercase());
    let (rows, total) = ctx
        .pool
        .read("推送的主機", move |s| {
            if s.get_rollout(&id)
                .map_err(query_failed("推送的主機"))?
                .is_none()
            {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "rollout_not_found",
                    format!("沒有推送 {id}"),
                ));
            }
            let mut all: Vec<_> = s
                .rollout_hosts(&id)
                .map_err(query_failed("推送的主機"))?
                .into_iter()
                .filter(|h| wave.is_none_or(|w| h.wave == w))
                .filter(|h| states.is_empty() || states.contains(&h.state))
                .filter(|h| {
                    reason
                        .as_deref()
                        .is_none_or(|r| h.reason.as_deref() == Some(r))
                })
                .filter(|h| code.as_deref().is_none_or(|c| h.code.as_deref() == Some(c)))
                .filter(|h| {
                    text.as_deref().is_none_or(|t| {
                        h.hostname.to_lowercase().contains(t) || h.agent_id.contains(t)
                    })
                })
                .collect();
            all.sort_by_key(|h| (h.wave, h.ordinal, h.host_id));
            let total = all.len();
            let rows: Vec<_> = all
                .into_iter()
                .filter(|h| after.is_none_or(|a| (h.wave, h.ordinal, h.host_id) > a))
                .take(limit + 1)
                .collect();
            Ok((rows, total))
        })
        .await?;
    let more = rows.len() > limit;
    let page: Vec<_> = rows.into_iter().take(limit).collect();
    let next = if more {
        page.last().map(|h| {
            http::b64url_encode(format!("{},{},{}", h.wave, h.ordinal, h.host_id).as_bytes())
        })
    } else {
        None
    };
    Ok(http::json(
        StatusCode::OK,
        now,
        json!({
            "data": page.iter().map(rollout::host_json).collect::<Vec<_>>(),
            "page": {"limit": limit, "total": total, "next_cursor": next},
        }),
    ))
}
