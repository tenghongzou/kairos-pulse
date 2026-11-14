//! 告警的端點（`kairos/API.md` 第 13 節；設計包 S06～S11、§24 #9）：事件、規則、靜音與維護時段、
//! 通知管道。
//!
//! **權限集中標記**：每個端點需要的權限只在 [`route`] 這一張表決定（[`ROUTES`] 是同一張表的文件版，
//! 測試確認兩者一致）。權限與角色的對應在 [`crate::rbac`]（管理員全部；維運者處理事件、自訂規則、靜音、
//! 管道的清單與測試；稽核員與唯讀唯讀，`ia.md` §4.2）。驗證（API token 或工作階段；工作階段的寫入另需
//! CSRF token）在 [`super::route`] 完成後才進入這裡。
//! 寫入都寫入稽核雜湊鏈（`audit_events`，類別 `alerts`；執行者 `token:<token_id>` 或 `user:<帳號>`、來源 IP）。

use chrono::{DateTime, Utc};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Map, Value, json};
use std::net::SocketAddr;
use std::sync::Arc;

use super::ApiContext;
use super::auth::{Permission, Principal};
use super::http::{self, ApiBody, ApiError, Query, b64url_decode, b64url_encode, ts_ms};
use crate::alerts::engine::ActionError;
use crate::alerts::model::{
    Category, Channel, ChannelKind, NotifyPolicy, Rule, Scope, Severity, SilenceKind,
};
use crate::alerts::notify::transport::{CODE_CHANNEL_CONFIG, parse_url, valid_address};
use crate::alerts::ops::{self, SilenceInput};
use crate::alerts::rules::{self, CODE_RULE_INVALID, kind};
use crate::alerts::secrets::{masked, scrub};
use crate::alerts::{AlertService, backtest, scope, store};
use crate::enroll::store::Audit;
use crate::store::prelude::*;

/// 本文的上限
pub const MAX_BODY: usize = 256 * 1024;

/// 端點（[`route`] 的結果）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    AlertsList,
    AlertsSummary,
    AlertsBatchAck,
    AlertGet(String),
    AlertAction(String, String),
    RulesList,
    RuleCreate,
    RulePreview,
    RuleGet(String),
    RuleUpdate(String),
    RuleEnable(String, bool),
    RuleDelete(String),
    SilencesList,
    SilenceCreate,
    SilencePreview,
    SilenceGet(String),
    SilenceUpdate(String),
    SilenceEnd(String),
    ChannelsList,
    ChannelCreate,
    ChannelTestUnsaved,
    ChannelGet(String),
    ChannelUpdate(String),
    ChannelDelete(String),
    ChannelTest(String),
}

/// 端點與權限的對照（文件版；測試確認與 [`route`] 一致）
#[cfg_attr(not(test), allow(dead_code))]
pub const ROUTES: &[(&str, &str, Permission)] = &[
    ("GET", "/api/v1/alerts", Permission::AlertsRead),
    ("GET", "/api/v1/alerts/summary", Permission::AlertsRead),
    ("GET", "/api/v1/alerts/{event_id}", Permission::AlertsRead),
    ("POST", "/api/v1/alerts/ack", Permission::AlertsWrite),
    (
        "POST",
        "/api/v1/alerts/{event_id}/ack",
        Permission::AlertsWrite,
    ),
    (
        "POST",
        "/api/v1/alerts/{event_id}/assign",
        Permission::AlertsWrite,
    ),
    (
        "POST",
        "/api/v1/alerts/{event_id}/snooze",
        Permission::AlertsWrite,
    ),
    (
        "POST",
        "/api/v1/alerts/{event_id}/resolve",
        Permission::AlertsWrite,
    ),
    (
        "POST",
        "/api/v1/alerts/{event_id}/comments",
        Permission::AlertsWrite,
    ),
    ("GET", "/api/v1/alert-rules", Permission::AlertRulesRead),
    (
        "GET",
        "/api/v1/alert-rules/{rule_id}",
        Permission::AlertRulesRead,
    ),
    ("POST", "/api/v1/alert-rules", Permission::AlertRulesWrite),
    (
        "POST",
        "/api/v1/alert-rules/preview",
        Permission::AlertRulesRead,
    ),
    (
        "PUT",
        "/api/v1/alert-rules/{rule_id}",
        Permission::AlertRulesWrite,
    ),
    (
        "POST",
        "/api/v1/alert-rules/{rule_id}/enable",
        Permission::AlertRulesWrite,
    ),
    (
        "POST",
        "/api/v1/alert-rules/{rule_id}/disable",
        Permission::AlertRulesWrite,
    ),
    (
        "DELETE",
        "/api/v1/alert-rules/{rule_id}",
        Permission::AlertRulesWrite,
    ),
    ("GET", "/api/v1/silences", Permission::SilencesRead),
    (
        "GET",
        "/api/v1/silences/{silence_id}",
        Permission::SilencesRead,
    ),
    ("POST", "/api/v1/silences", Permission::SilencesWrite),
    ("POST", "/api/v1/silences/preview", Permission::SilencesRead),
    (
        "PUT",
        "/api/v1/silences/{silence_id}",
        Permission::SilencesWrite,
    ),
    (
        "POST",
        "/api/v1/silences/{silence_id}/end",
        Permission::SilencesWrite,
    ),
    ("GET", "/api/v1/channels", Permission::ChannelsRead),
    (
        "GET",
        "/api/v1/channels/{channel_id}",
        Permission::ChannelsRead,
    ),
    ("POST", "/api/v1/channels", Permission::ChannelsWrite),
    ("POST", "/api/v1/channels/test", Permission::ChannelsWrite),
    (
        "PUT",
        "/api/v1/channels/{channel_id}",
        Permission::ChannelsWrite,
    ),
    (
        "DELETE",
        "/api/v1/channels/{channel_id}",
        Permission::ChannelsWrite,
    ),
    (
        "POST",
        "/api/v1/channels/{channel_id}/test",
        Permission::ChannelsTest,
    ),
];

/// 路徑與方法 → 端點與需要的權限（權限的唯一來源）
pub fn route(method: &str, rest: &[&str]) -> Option<(Endpoint, Permission)> {
    use Endpoint as E;
    use Permission as P;
    let s = |x: &str| x.to_string();
    Some(match (method, rest) {
        ("GET" | "HEAD", ["alerts"]) => (E::AlertsList, P::AlertsRead),
        ("GET" | "HEAD", ["alerts", "summary"]) => (E::AlertsSummary, P::AlertsRead),
        ("POST", ["alerts", "ack"]) => (E::AlertsBatchAck, P::AlertsWrite),
        ("GET" | "HEAD", ["alerts", id]) => (E::AlertGet(s(id)), P::AlertsRead),
        (
            "POST",
            [
                "alerts",
                id,
                a @ ("ack" | "assign" | "snooze" | "resolve" | "comments"),
            ],
        ) => (E::AlertAction(s(id), s(a)), P::AlertsWrite),
        ("GET" | "HEAD", ["alert-rules"]) => (E::RulesList, P::AlertRulesRead),
        ("POST", ["alert-rules"]) => (E::RuleCreate, P::AlertRulesWrite),
        ("POST", ["alert-rules", "preview"]) => (E::RulePreview, P::AlertRulesRead),
        ("GET" | "HEAD", ["alert-rules", id]) => (E::RuleGet(s(id)), P::AlertRulesRead),
        ("PUT", ["alert-rules", id]) => (E::RuleUpdate(s(id)), P::AlertRulesWrite),
        ("POST", ["alert-rules", id, "enable"]) => (E::RuleEnable(s(id), true), P::AlertRulesWrite),
        ("POST", ["alert-rules", id, "disable"]) => {
            (E::RuleEnable(s(id), false), P::AlertRulesWrite)
        }
        ("DELETE", ["alert-rules", id]) => (E::RuleDelete(s(id)), P::AlertRulesWrite),
        ("GET" | "HEAD", ["silences"]) => (E::SilencesList, P::SilencesRead),
        ("POST", ["silences"]) => (E::SilenceCreate, P::SilencesWrite),
        ("POST", ["silences", "preview"]) => (E::SilencePreview, P::SilencesRead),
        ("GET" | "HEAD", ["silences", id]) => (E::SilenceGet(s(id)), P::SilencesRead),
        ("PUT", ["silences", id]) => (E::SilenceUpdate(s(id)), P::SilencesWrite),
        ("POST", ["silences", id, "end"]) => (E::SilenceEnd(s(id)), P::SilencesWrite),
        ("GET" | "HEAD", ["channels"]) => (E::ChannelsList, P::ChannelsRead),
        ("POST", ["channels"]) => (E::ChannelCreate, P::ChannelsWrite),
        ("POST", ["channels", "test"]) => (E::ChannelTestUnsaved, P::ChannelsWrite),
        ("GET" | "HEAD", ["channels", id]) => (E::ChannelGet(s(id)), P::ChannelsRead),
        ("PUT", ["channels", id]) => (E::ChannelUpdate(s(id)), P::ChannelsWrite),
        ("DELETE", ["channels", id]) => (E::ChannelDelete(s(id)), P::ChannelsWrite),
        ("POST", ["channels", id, "test"]) => (E::ChannelTest(s(id)), P::ChannelsTest),
        _ => return None,
    })
}

/// 這個路徑是否屬於告警的端點（路由在 [`super::route`] 的 `known_route`；這裡供測試對照）
#[cfg(test)]
pub fn owns(path: &str) -> bool {
    [
        "/api/v1/alerts",
        "/api/v1/alert-rules",
        "/api/v1/silences",
        "/api/v1/channels",
    ]
    .iter()
    .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "alerts_unavailable",
        "告警引擎沒有啟動（alerts.enabled = false、storage.backend = \"jsonl\"，或啟動失敗；見 Kairos 的日誌）",
    )
}

fn db_error(e: impl std::fmt::Display) -> ApiError {
    log::warn!(event = "alerts.api.storage_error"; "告警端點：資料庫無法讀寫：{e}");
    ApiError::storage("告警資料")
}

fn action_error(e: ActionError) -> ApiError {
    let status = match e.code {
        "event_not_found" => StatusCode::NOT_FOUND,
        "invalid_parameter" => StatusCode::BAD_REQUEST,
        _ => StatusCode::CONFLICT,
    };
    ApiError::new(status, e.code, e.message)
}

fn invalid(e: ops::Invalid) -> ApiError {
    let status = StatusCode::BAD_REQUEST;
    let mut details = json!({ "parameter": e.field });
    if let (Value::Object(d), Value::Object(extra)) = (&mut details, &e.details) {
        for (k, v) in extra {
            d.insert(k.clone(), v.clone());
        }
    }
    ApiError::new(status, e.code, e.message).with_details(details)
}

fn rule_invalid(e: rules::Invalid) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "rule_invalid",
        format!("告警規則無法儲存：{}", e.message),
    )
    .with_product_code(CODE_RULE_INVALID)
    .with_details(json!({ "parameter": e.field }))
}

fn segments(path: &str) -> Vec<String> {
    path.trim_end_matches('/')
        .split('/')
        .skip(1)
        .filter_map(|s| http::percent_decode(s, false))
        .collect()
}

/// 寫入的上下文
struct Ctx<'a> {
    api: &'a Arc<ApiContext>,
    svc: &'a Arc<AlertService>,
    principal: &'a Principal,
    actor: String,
    ip: String,
    now: DateTime<Utc>,
}

impl Ctx<'_> {
    fn now_ms(&self) -> i64 {
        self.now.timestamp_millis()
    }

    async fn audit(&self, action: &str, object: Option<&str>, result: &str, details: Value) {
        let (actor, ip, action, object, result) = (
            self.actor.clone(),
            self.ip.clone(),
            action.to_string(),
            object.map(str::to_string),
            result.to_string(),
        );
        let now = self.now_ms();
        if let Err(e) = self
            .svc
            .db
            .write(move |conn| {
                Ok(conn.audit(
                    now,
                    &Audit {
                        actor: &actor,
                        actor_ip: Some(&ip),
                        category: "alerts",
                        action: &action,
                        object: object.as_deref(),
                        result: &result,
                        code: None,
                        details: Some(details),
                    },
                )?)
            })
            .await
        {
            log::warn!(event = "audit.write_failed"; "告警端點：無法寫入稽核紀錄：{e}");
        }
    }
}

/// 處理 `/api/v1/alerts…`、`/alert-rules…`、`/silences…`、`/channels…`（已通過驗證；
/// 驗證與 CSRF 在 [`super::route`]）
pub async fn handle(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    principal: &Principal,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let segs = segments(req.uri().path());
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let ["api", "v1", rest @ ..] = segs.as_slice() else {
        return Err(ApiError::not_found());
    };
    let method = req.method().clone();
    let (endpoint, permission) = route(method.as_str(), rest).ok_or_else(|| {
        if route("GET", rest).is_some()
            || route("POST", rest).is_some()
            || route("PUT", rest).is_some()
        {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "這個端點不接受這個方法",
            )
        } else {
            ApiError::not_found()
        }
    })?;
    let query = Query::parse(req.uri().query())?;
    principal.require(permission)?;
    let svc = ctx.alerts.clone().ok_or_else(unavailable)?;
    let body = if method == Method::POST || method == Method::PUT || method == Method::DELETE {
        let has_len = req
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() != "0");
        if has_len {
            http::read_json(req, MAX_BODY).await?
        } else {
            json!({})
        }
    } else {
        Value::Null
    };
    let c = Ctx {
        api: ctx,
        svc: &svc,
        principal,
        actor: principal.actor(),
        ip: peer.ip().to_canonical().to_string(),
        now,
    };
    use Endpoint as E;
    match endpoint {
        E::AlertsList => list_alerts(&c, &query).await,
        E::AlertsSummary => summary(&c).await,
        E::AlertsBatchAck => batch_ack(&c, &body).await,
        E::AlertGet(id) => get_alert(&c, &id).await,
        E::AlertAction(id, a) => alert_action(&c, &id, &a, &body).await,
        E::RulesList => list_rules(&c, &query).await,
        E::RuleGet(id) => get_rule(&c, &id).await,
        E::RuleCreate => create_rule(&c, &body).await,
        E::RuleUpdate(id) => update_rule(&c, &id, &body).await,
        E::RuleEnable(id, on) => enable_rule(&c, &id, on, &body).await,
        E::RuleDelete(id) => delete_rule(&c, &id).await,
        E::RulePreview => preview_rule(&c, &body).await,
        E::SilencesList => list_silences(&c, &query).await,
        E::SilenceGet(id) => get_silence(&c, &id).await,
        E::SilenceCreate => save_silence(&c, None, &body).await,
        E::SilenceUpdate(id) => save_silence(&c, Some(&id), &body).await,
        E::SilencePreview => preview_silence(&c, &body).await,
        E::SilenceEnd(id) => end_silence(&c, &id, &body).await,
        E::ChannelsList => list_channels(&c).await,
        E::ChannelGet(id) => get_channel(&c, &id).await,
        E::ChannelCreate => save_channel(&c, None, &body).await,
        E::ChannelUpdate(id) => save_channel(&c, Some(&id), &body).await,
        E::ChannelDelete(id) => delete_channel(&c, &id, &body).await,
        E::ChannelTest(id) => test_channel(&c, Some(&id), &body).await,
        E::ChannelTestUnsaved => test_channel(&c, None, &body).await,
    }
}

fn created(now: DateTime<Utc>, data: Value) -> Response<ApiBody> {
    http::json(StatusCode::CREATED, now, json!({ "data": data }))
}

fn str_list(q: &Query, name: &str, allowed: &[&str]) -> Result<Vec<String>, ApiError> {
    let list = q.list(name);
    if !allowed.is_empty()
        && let Some(bad) = list.iter().find(|v| !allowed.contains(&v.as_str()))
    {
        return Err(ApiError::param(
            name,
            format!("{name} 的值 \"{bad}\" 無效（{}）", allowed.join("、")),
        ));
    }
    Ok(list)
}

fn time_param(q: &Query, name: &str) -> Result<Option<i64>, ApiError> {
    q.get(name)
        .map(|v| {
            DateTime::parse_from_rfc3339(v)
                .map(|t| t.timestamp_millis())
                .map_err(|_| ApiError::param(name, format!("{name} 必須是 RFC 3339 時間")))
        })
        .transpose()
}

fn limit_param(q: &Query, default: usize, max: usize) -> Result<usize, ApiError> {
    match q.get("limit") {
        None => Ok(default),
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=max).contains(n))
            .ok_or_else(|| ApiError::param("limit", format!("limit 必須是 1～{max}"))),
    }
}

// ---------------------------------------------------------------------------
// 事件
// ---------------------------------------------------------------------------

async fn list_alerts(c: &Ctx<'_>, q: &Query) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(
        &[
            "view",
            "severity",
            "state",
            "rule",
            "category",
            "agent_id",
            "q",
            "from",
            "to",
            "include_children",
            "limit",
            "cursor",
        ],
        &[],
    )?;
    let view = q.get("view").unwrap_or("active").to_string();
    if !matches!(view.as_str(), "active" | "needs_me" | "recovered" | "all") {
        return Err(ApiError::param(
            "view",
            "view 必須是 active、needs_me、recovered 或 all",
        ));
    }
    let after = match q.get("cursor") {
        None => None,
        Some(cur) => {
            let bytes = b64url_decode(cur).ok_or_else(|| {
                ApiError::new(StatusCode::BAD_REQUEST, "invalid_cursor", "cursor 無法解析")
            })?;
            let v: (i64, i64, i64, String) = serde_json::from_slice(&bytes).map_err(|_| {
                ApiError::new(StatusCode::BAD_REQUEST, "invalid_cursor", "cursor 無法解析")
            })?;
            Some(v)
        }
    };
    let filter = store::EventFilter {
        view,
        severity: str_list(q, "severity", &["emergency", "critical", "warning", "info"])?,
        state: str_list(
            q,
            "state",
            &["firing", "acknowledged", "recovered", "resolved"],
        )?,
        rule: q.list("rule"),
        category: str_list(q, "category", &["host", "identity", "aggregate", "system"])?,
        agent_id: q.get("agent_id").map(|a| a.to_ascii_lowercase()),
        q: q.get("q").map(str::to_string),
        from_ms: time_param(q, "from")?,
        to_ms: time_param(q, "to")?,
        include_children: q.bool("include_children")?.unwrap_or(false),
        limit: limit_param(q, 100, 1000)?,
        after,
    };
    let limit = filter.limit;
    let (events, total, counts) = c
        .svc
        .db
        .read(move |conn| {
            let (e, t) = conn.list_events(&filter)?;
            Ok((e, t, conn.counts()?))
        })
        .await
        .map_err(db_error)?;
    let next = (events.len() == limit)
        .then(|| events.last())
        .flatten()
        .map(|e| {
            b64url_encode(
                serde_json::to_string(&(
                    e.severity.rank(),
                    i64::from(e.acked),
                    e.started_at,
                    &e.event_id,
                ))
                .unwrap_or_default()
                .as_bytes(),
            )
        });
    let data: Vec<Value> = events.iter().map(ops::event_json).collect();
    Ok(http::json(
        StatusCode::OK,
        c.now,
        json!({
            "data": data,
            "page": { "limit": limit, "total": total, "next_cursor": next },
            "meta": { "counts": counts, "engine": c.svc.status().await },
        }),
    ))
}

async fn summary(c: &Ctx<'_>) -> Result<Response<ApiBody>, ApiError> {
    let now = c.now_ms();
    let (counts, silences) = c
        .svc
        .db
        .read(move |conn| Ok((conn.counts()?, conn.list_silences("active", now, 1000)?)))
        .await
        .map_err(db_error)?;
    // 「目前有 2 個靜音規則生效，影響 301 台主機」：以主機為範圍的靜音涵蓋的主機（聯集）
    let mut union = std::collections::HashSet::new();
    let hosts = c.svc.matching_hosts_list(&silences).await;
    for h in hosts {
        union.insert(h);
    }
    Ok(http::ok(
        c.now,
        json!({
            "counts": counts,
            "silences": { "active": silences.len(), "hosts": union.len() },
            "engine": c.svc.status().await,
        }),
    ))
}

async fn get_alert(c: &Ctx<'_>, id: &str) -> Result<Response<ApiBody>, ApiError> {
    let id = id.to_string();
    let r = c
        .svc
        .db
        .read(move |conn| {
            let Some(ev) = conn.get_event(&id)? else {
                return Ok(None);
            };
            let timeline = conn.timeline(&id, 500)?;
            let (children, total) = conn.children(&id, 100)?;
            let parent = match &ev.parent_id {
                Some(p) => conn.get_event(p)?,
                None => None,
            };
            let deliveries = conn.deliveries_of_event(&id, 100)?;
            let channels = conn.load_channels(None)?;
            let rule = conn.get_rule(&ev.rule_id)?;
            Ok(Some((
                ev, timeline, children, total, parent, deliveries, channels, rule,
            )))
        })
        .await
        .map_err(db_error)?;
    let Some((ev, timeline, children, total, parent, deliveries, channels, rule)) = r else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "event_not_found",
            "沒有這個事件（可能已依保留期限清除）",
        ));
    };
    let name_of = |cid: &str| {
        channels
            .iter()
            .find(|ch| ch.channel_id == cid)
            .map(|ch| ch.name.as_str())
    };
    let mut data = ops::event_json(&ev);
    let actions: Vec<&str> = if ev.state == crate::alerts::model::EventState::Resolved
        || ev.attention != crate::alerts::model::Attention::Action
    {
        vec!["comment"]
    } else {
        let mut a = vec!["assign", "snooze", "resolve", "comment", "silence"];
        // 不可靜音的系統規則：不提供「建立靜音」
        if rules::is_unsilenceable(&ev.rule_id) {
            a.retain(|x| *x != "silence");
        }
        if !ev.acked {
            a.insert(0, "ack");
        }
        a
    };
    data["timeline"] = json!(timeline);
    data["children"] = json!({
        "total": total,
        "items": children.iter().map(ops::event_json).collect::<Vec<_>>(),
    });
    data["parent"] = parent.map_or(Value::Null, |p| {
        json!({
            "event_id": p.event_id,
            "title": p.title,
            "severity": p.severity,
            "state": p.state,
            "banner": format!("此事件由{}「{}」造成，請先處理該事件", if p.category == Category::System { "系統事件" } else { "彙總事件" }, p.title),
        })
    });
    data["deliveries"] = json!(
        deliveries
            .iter()
            .map(|d| store::delivery_json(d, name_of(&d.channel_id)))
            .collect::<Vec<_>>()
    );
    data["rule"] = rule.map_or(Value::Null, |r| json!({ "rule_id": r.rule_id, "name": r.name, "description": r.description, "builtin": r.builtin }));
    data["actions"] = json!(actions);
    data["link"] = json!(c.svc.settings.event_link(&ev.event_id));
    Ok(http::ok(c.now, data))
}

async fn alert_action(
    c: &Ctx<'_>,
    id: &str,
    action: &str,
    body: &Value,
) -> Result<Response<ApiBody>, ApiError> {
    let action = if action == "comments" {
        "comment"
    } else {
        action
    };
    let mut params = body.clone();
    // 「確認並指派給我」：負責人為目前的身分
    if action == "ack" && params.get("assign_to_me").and_then(Value::as_bool) == Some(true) {
        params["assignee"] = json!(c.principal.label());
    }
    let actor = c.principal.label();
    let r = c.svc.act(id, action, &params, &actor).await;
    let r = match r {
        Err(e) if e.code == "event_not_found" && action == "comment" => {
            // 已解決（不在記憶體中）的事件也可以新增註解
            comment_resolved(c, id, body).await
        }
        other => other.map_err(action_error),
    };
    let ok = r.is_ok();
    c.audit(
        &format!("alert.{action}"),
        Some(id),
        if ok { "ok" } else { "failed" },
        json!({ "params": body }),
    )
    .await;
    let result = r?;
    let id2 = id.to_string();
    let ev = c
        .svc
        .db
        .read(move |conn| Ok(conn.get_event(&id2)?))
        .await
        .map_err(db_error)?;
    Ok(http::ok(
        c.now,
        json!({ "result": result, "event": ev.as_ref().map(ops::event_json) }),
    ))
}

async fn comment_resolved(c: &Ctx<'_>, id: &str, body: &Value) -> Result<Value, ApiError> {
    let text = body
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty() && t.chars().count() <= 2000)
        .ok_or_else(|| ApiError::param("text", "註解必須是 1～2000 個字元"))?
        .to_string();
    let (id, actor, now) = (id.to_string(), c.principal.label(), c.now_ms());
    let found = c
        .svc
        .db
        .write(move |conn| {
            if conn.get_event(&id)?.is_none() {
                return Ok(false);
            }
            let tx = conn.begin(TxMode::Deferred)?;
            tx.insert_timeline(&[crate::alerts::engine::Timeline {
                event_id: id.clone(),
                at: now,
                actor,
                action: "commented",
                details: json!({ "text": text }),
            }])?;
            tx.commit()?;
            Ok(true)
        })
        .await
        .map_err(db_error)?;
    if !found {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "event_not_found",
            "沒有這個事件",
        ));
    }
    Ok(json!({ "commented": true }))
}

async fn batch_ack(c: &Ctx<'_>, body: &Value) -> Result<Response<ApiBody>, ApiError> {
    let ids: Vec<String> = body
        .get("event_ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() || ids.len() > 1000 {
        return Err(ApiError::param("event_ids", "event_ids 必須有 1～1000 筆"));
    }
    let actor = c.principal.label();
    let mut results = Vec::new();
    let (mut acked, mut skipped, mut failed) = (0, 0, 0);
    for id in &ids {
        match c.svc.act(id, "ack", &json!({}), &actor).await {
            Ok(_) => {
                acked += 1;
                results.push(json!({ "event_id": id, "result": "acknowledged" }));
            }
            Err(e) if e.code == "not_acknowledgeable" => {
                skipped += 1;
                results.push(json!({ "event_id": id, "result": "skipped", "message": e.message }));
            }
            Err(e) => {
                failed += 1;
                results.push(json!({ "event_id": id, "result": "failed", "code": e.code, "message": e.message }));
            }
        }
    }
    c.audit(
        "alert.ack_batch",
        None,
        "ok",
        json!({ "count": ids.len(), "acked": acked, "skipped": skipped, "failed": failed }),
    )
    .await;
    Ok(http::ok(
        c.now,
        json!({
            "results": results,
            "summary": { "acknowledged": acked, "skipped": skipped, "failed": failed },
            "message": (skipped > 0).then(|| format!("已略過 {skipped} 筆不需要確認的事件")),
        }),
    ))
}

// ---------------------------------------------------------------------------
// 規則
// ---------------------------------------------------------------------------

fn rule_json(r: &Rule, stats: Option<(u64, u64)>) -> Value {
    let (fires, quick) = stats.unwrap_or((0, 0));
    let noise = (fires > 0).then(|| quick as f64 / fires as f64);
    json!({
        "rule_id": r.rule_id,
        "builtin": r.builtin,
        "kind": r.kind,
        "category": r.category,
        "name": r.name,
        "enabled": r.enabled,
        // 受保護的系統規則（由程式碼決定，rules.rs）：事件不受靜音影響、不可停用
        "unsilenceable": rules::is_unsilenceable(&r.rule_id),
        "always_enabled": rules::is_always_enabled(&r.rule_id),
        "severity": r.severity,
        "max_severity": r.max_severity(),
        "attention": r.attention(),
        "params": r.params,
        "scope": r.scope,
        "scope_text": scope::sentence(&r.scope),
        "channels": r.channels,
        "notify": r.notify,
        "notify_effective": {
            "repeat_interval_secs": r.notify.effective(r.severity).repeat_interval_secs,
            "repeat_max": r.notify.effective(r.severity).repeat_max,
            "notify_recovery": r.notify.effective(r.severity).notify_recovery,
            "escalate_after_secs": r.notify.effective(r.severity).escalate_after_secs,
        },
        "description": r.description,
        "disabled_reason": r.disabled_reason,
        "fires_7d": fires,
        "noise_7d": noise,
        "noisy": noise.is_some_and(|n| n > 0.5) && fires >= 5,
        "created_at": ts_ms(r.created_at),
        "created_by": r.created_by,
        "updated_at": ts_ms(r.updated_at),
        "updated_by": r.updated_by,
    })
}

async fn list_rules(c: &Ctx<'_>, q: &Query) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(&["kind", "category", "severity", "enabled"], &[])?;
    let since = c.now_ms() - 7 * 86_400_000;
    let (rules, stats) = c
        .svc
        .db
        .read(move |conn| Ok((conn.load_rules()?, conn.rule_stats(since)?)))
        .await
        .map_err(db_error)?;
    let kinds = q.list("kind");
    let cats = q.list("category");
    let sevs = q.list("severity");
    let enabled = q.bool("enabled")?;
    let data: Vec<Value> = rules
        .iter()
        .filter(|r| kinds.is_empty() || kinds.contains(&r.kind))
        .filter(|r| cats.is_empty() || cats.iter().any(|c| c == r.category.as_str()))
        .filter(|r| {
            sevs.is_empty()
                || sevs
                    .iter()
                    .any(|s| s == r.severity.as_str() || s == r.max_severity().as_str())
        })
        .filter(|r| enabled.is_none_or(|e| e == r.enabled))
        .map(|r| rule_json(r, stats.get(&r.rule_id).copied()))
        .collect();
    Ok(http::ok(c.now, data))
}

async fn get_rule(c: &Ctx<'_>, id: &str) -> Result<Response<ApiBody>, ApiError> {
    let (id2, since) = (id.to_string(), c.now_ms() - 7 * 86_400_000);
    let (r, stats) = c
        .svc
        .db
        .read(move |conn| Ok((conn.get_rule(&id2)?, conn.rule_stats(since)?)))
        .await
        .map_err(db_error)?;
    let r = r.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "rule_not_found",
            format!("沒有規則 {id}"),
        )
    })?;
    Ok(http::ok(
        c.now,
        rule_json(&r, stats.get(&r.rule_id).copied()),
    ))
}

/// 本文中的規則欄位（建立與修改共用）
/// 規則的欄位：名稱、嚴重度、條件、範圍、通知管道、通知設定、說明
type RuleFields = (
    String,
    Severity,
    Value,
    Scope,
    Vec<String>,
    NotifyPolicy,
    Option<String>,
);

fn rule_fields(
    body: &Value,
    kind_name: &str,
    builtin: bool,
    channel_exists: &dyn Fn(&str) -> bool,
    base: Option<&Rule>,
) -> Result<RuleFields, ApiError> {
    let name = match body.get("name").and_then(Value::as_str) {
        Some(n) => rules::validate_name(n).map_err(rule_invalid)?,
        None => base.map(|b| b.name.clone()).ok_or_else(|| {
            rule_invalid(rules::Invalid {
                field: "name".into(),
                message: "name 必填".into(),
            })
        })?,
    };
    let severity = match body.get("severity").and_then(Value::as_str) {
        Some(s) => Severity::parse(s).ok_or_else(|| {
            rule_invalid(rules::Invalid {
                field: "severity".into(),
                message: "severity 必須是 emergency、critical、warning、info".into(),
            })
        })?,
        None => base.map(|b| b.severity).ok_or_else(|| {
            rule_invalid(rules::Invalid {
                field: "severity".into(),
                message: "severity 必填".into(),
            })
        })?,
    };
    let params = match (body.get("params"), base) {
        (Some(p), _) if !builtin => rules::validate_params(kind_name, p).map_err(rule_invalid)?,
        (None, Some(b)) => b.params.clone(),
        (None, None) => rules::validate_params(kind_name, &Value::Null).map_err(rule_invalid)?,
        (Some(_), Some(b)) => b.params.clone(),
        (Some(p), None) => rules::validate_params(kind_name, p).map_err(rule_invalid)?,
    };
    let scope = match body.get("scope") {
        Some(s) => {
            if s.get("groups").is_some() {
                return Err(rule_invalid(rules::Invalid {
                    field: "scope.groups".into(),
                    message: "主機清冊還沒有群組，範圍請用標籤、網段或主機".into(),
                }));
            }
            let s: Scope = serde_json::from_value(s.clone()).map_err(|e| {
                rule_invalid(rules::Invalid {
                    field: "scope".into(),
                    message: e.to_string(),
                })
            })?;
            rules::validate_scope(&s).map_err(rule_invalid)?
        }
        None => base.map(|b| b.scope.clone()).unwrap_or_default(),
    };
    let channels = match body.get("channels") {
        Some(v) => {
            let list: Vec<String> = serde_json::from_value(v.clone()).map_err(|_| {
                rule_invalid(rules::Invalid {
                    field: "channels".into(),
                    message: "channels 必須是字串陣列".into(),
                })
            })?;
            rules::validate_channels(&list, channel_exists).map_err(rule_invalid)?
        }
        None => base.map(|b| b.channels.clone()).unwrap_or_default(),
    };
    let notify = match body.get("notify") {
        Some(v) => {
            let n: NotifyPolicy = serde_json::from_value(v.clone()).map_err(|e| {
                rule_invalid(rules::Invalid {
                    field: "notify".into(),
                    message: e.to_string(),
                })
            })?;
            rules::validate_notify(
                &n,
                severity,
                builtin && base.is_some_and(|b| b.notify == n),
                channel_exists,
            )
            .map_err(rule_invalid)?
        }
        None => {
            let n = base.map(|b| b.notify.clone()).unwrap_or_default();
            rules::validate_notify(&n, severity, builtin, channel_exists).map_err(rule_invalid)?
        }
    };
    let description = match body.get("description") {
        Some(Value::Null) => None,
        Some(v) => rules::validate_description(v.as_str()).map_err(rule_invalid)?,
        None => base.and_then(|b| b.description.clone()),
    };
    Ok((name, severity, params, scope, channels, notify, description))
}

async fn channel_ids(c: &Ctx<'_>) -> Result<Vec<String>, ApiError> {
    c.svc
        .db
        .read(|conn| {
            Ok(conn
                .load_channels(None)?
                .into_iter()
                .map(|ch| ch.channel_id)
                .collect())
        })
        .await
        .map_err(db_error)
}

async fn create_rule(c: &Ctx<'_>, body: &Value) -> Result<Response<ApiBody>, ApiError> {
    let kind_name = body
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !kind::USER.contains(&kind_name.as_str()) {
        return Err(rule_invalid(rules::Invalid {
            field: "kind".into(),
            message: format!(
                "kind 必須是 {}（系統事件類的規則不能新增，只能調整系統預設規則）",
                kind::USER.join("、")
            ),
        }));
    }
    let ids = channel_ids(c).await?;
    let exists = |x: &str| ids.iter().any(|i| i == x);
    let (name, severity, params, scope, channels, notify, description) =
        rule_fields(body, &kind_name, false, &exists, None)?;
    let category = if kind_name == kind::IDENTITY_CONFLICT {
        Category::Identity
    } else {
        Category::Host
    };
    let now = c.now_ms();
    let rule = Rule {
        rule_id: ops::new_id("r"),
        builtin: false,
        kind: kind_name,
        category,
        name,
        enabled: body.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        severity,
        params,
        scope,
        channels,
        notify,
        description,
        disabled_reason: None,
        created_at: now,
        created_by: c.actor.clone(),
        updated_at: now,
        updated_by: c.actor.clone(),
    };
    let r2 = rule.clone();
    c.svc
        .db
        .write(move |conn| Ok(conn.save_rule(&r2)?))
        .await
        .map_err(db_error)?;
    c.svc.request_reload();
    c.audit(
        "alert_rule.create",
        Some(&rule.rule_id),
        "ok",
        json!({ "name": rule.name, "kind": rule.kind, "severity": rule.severity }),
    )
    .await;
    Ok(created(c.now, rule_json(&rule, None)))
}

async fn load_rule(c: &Ctx<'_>, id: &str) -> Result<Rule, ApiError> {
    let id2 = id.to_string();
    c.svc
        .db
        .read(move |conn| Ok(conn.get_rule(&id2)?))
        .await
        .map_err(db_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "rule_not_found",
                format!("沒有規則 {id}"),
            )
        })
}

async fn update_rule(c: &Ctx<'_>, id: &str, body: &Value) -> Result<Response<ApiBody>, ApiError> {
    let old = load_rule(c, id).await?;
    let ids = channel_ids(c).await?;
    let exists = |x: &str| ids.iter().any(|i| i == x);
    let mut rule = old.clone();
    if old.builtin {
        if body.get("kind").is_some() || body.get("name").is_some() {
            return Err(rule_invalid(rules::Invalid {
                field: "name".into(),
                message: "系統預設規則的種類與名稱不能修改".into(),
            }));
        }
        let (_, severity, _, scope, channels, notify, description) =
            rule_fields(body, &old.kind, true, &exists, Some(&old))?;
        let default = rules::builtin_default(id);
        // 系統事件的最低等級不可低於預設（§2.1.1）
        if let Some(d) = &default
            && old.category == Category::System
            && severity > d.severity
        {
            return Err(rule_invalid(rules::Invalid {
                field: "severity".into(),
                message: format!("系統規則的嚴重度不可低於預設（{}）", d.severity.label()),
            }));
        }
        if let Some(p) = body.get("params") {
            // 系統預設規則的條件只有管理員能改（S09）
            c.principal
                .require(crate::api::auth::Permission::AlertRulesAdmin)?;
            let def = default.clone().unwrap_or_else(|| old.clone());
            let mut base = def.clone();
            base.params = old.params.clone();
            rule.params = rules::validate_builtin_params(&base, p).map_err(rule_invalid)?;
        }
        if body.get("scope").is_some() && old.category == Category::System {
            return Err(rule_invalid(rules::Invalid {
                field: "scope".into(),
                message: "系統事件沒有主機範圍".into(),
            }));
        }
        rule.severity = severity;
        rule.scope = scope;
        rule.channels = channels;
        rule.notify = notify;
        rule.description = description;
    } else {
        if body
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|k| k != old.kind)
        {
            return Err(rule_invalid(rules::Invalid {
                field: "kind".into(),
                message: "規則的種類不能修改；請建立新的規則".into(),
            }));
        }
        let (name, severity, params, scope, channels, notify, description) =
            rule_fields(body, &old.kind, false, &exists, Some(&old))?;
        rule.name = name;
        rule.severity = severity;
        rule.params = params;
        rule.scope = scope;
        rule.channels = channels;
        rule.notify = notify;
        rule.description = description;
    }
    rule.updated_at = c.now_ms();
    rule.updated_by = c.actor.clone();
    let r2 = rule.clone();
    c.svc
        .db
        .write(move |conn| Ok(conn.save_rule(&r2)?))
        .await
        .map_err(db_error)?;
    c.svc.request_reload();
    c.audit(
        "alert_rule.update",
        Some(id),
        "ok",
        json!({ "changes": body }),
    )
    .await;
    Ok(http::ok(c.now, rule_json(&rule, None)))
}

async fn enable_rule(
    c: &Ctx<'_>,
    id: &str,
    on: bool,
    body: &Value,
) -> Result<Response<ApiBody>, ApiError> {
    let mut rule = load_rule(c, id).await?;
    let reason = body
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    if !on && rule.builtin && reason.is_none() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "confirmation_required",
            "停用系統預設規則必須填寫原因（L1）",
        )
        .with_details(json!({ "parameter": "reason" })));
    }
    // 不可停用的系統規則（0.10.1：「雙人覆核已關閉」）：停用它等於讓關閉雙人覆核不留下告警
    if !on && rules::is_always_enabled(&rule.rule_id) {
        c.audit(
            "alert_rule.disable",
            Some(id),
            "rejected",
            json!({ "reason": reason, "error": "rule_always_enabled" }),
        )
        .await;
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "rule_always_enabled",
            format!(
                "系統規則「{}」不可停用；它的事件也不受靜音影響。不需要通知時可以調整通知管道，但事件仍會記錄",
                rule.name
            ),
        ));
    }
    if rule.enabled == on {
        return Ok(http::ok(c.now, rule_json(&rule, None)));
    }
    rule.enabled = on;
    rule.disabled_reason = if on { None } else { reason.clone() };
    rule.updated_at = c.now_ms();
    rule.updated_by = c.actor.clone();
    let r2 = rule.clone();
    c.svc
        .db
        .write(move |conn| Ok(conn.save_rule(&r2)?))
        .await
        .map_err(db_error)?;
    c.svc.request_reload();
    c.audit(
        if on {
            "alert_rule.enable"
        } else {
            "alert_rule.disable"
        },
        Some(id),
        "ok",
        json!({ "reason": reason }),
    )
    .await;
    Ok(http::ok(c.now, rule_json(&rule, None)))
}

async fn delete_rule(c: &Ctx<'_>, id: &str) -> Result<Response<ApiBody>, ApiError> {
    let rule = load_rule(c, id).await?;
    if rule.builtin {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "builtin_rule",
            "系統預設規則不可刪除；不需要時請停用並填寫原因",
        ));
    }
    let id2 = id.to_string();
    c.svc
        .db
        .write(move |conn| Ok(conn.delete_rule(&id2)?))
        .await
        .map_err(db_error)?;
    c.svc.request_reload();
    c.audit(
        "alert_rule.delete",
        Some(id),
        "ok",
        json!({ "name": rule.name }),
    )
    .await;
    Ok(http::ok(c.now, json!({ "deleted": true, "rule_id": id })))
}

async fn preview_rule(c: &Ctx<'_>, body: &Value) -> Result<Response<ApiBody>, ApiError> {
    let days = body
        .get("days")
        .and_then(Value::as_u64)
        .unwrap_or(7)
        .clamp(1, u64::from(backtest::MAX_DAYS)) as u32;
    let rule = match body.get("rule_id").and_then(Value::as_str) {
        Some(id) => load_rule(c, id).await?,
        None => {
            let kind_name = body
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let params =
                rules::validate_params(&kind_name, body.get("params").unwrap_or(&Value::Null))
                    .map_err(rule_invalid)?;
            let scope = match body.get("scope") {
                Some(s) => {
                    let s: Scope = serde_json::from_value(s.clone())
                        .map_err(|e| ApiError::param("scope", e.to_string()))?;
                    rules::validate_scope(&s).map_err(rule_invalid)?
                }
                None => Scope::default(),
            };
            let mut r = rules::builtin_default(rules::ids::HOST_OFFLINE).expect("內建規則存在");
            r.rule_id = "preview".into();
            r.builtin = false;
            r.kind = kind_name;
            r.params = params;
            r.scope = scope;
            r
        }
    };
    let now = c.now_ms();
    let offset = c.svc.settings.offset();
    let result = c
        .svc
        .db
        .read(move |conn| Ok(backtest::preview(conn, &rule, days, now, offset)?))
        .await
        .map_err(db_error)?;
    Ok(http::ok(c.now, result))
}

// ---------------------------------------------------------------------------
// 靜音
// ---------------------------------------------------------------------------

fn silence_input(body: &Value) -> Result<SilenceInput, ApiError> {
    let time = |k: &str| -> Result<Option<i64>, ApiError> {
        match body.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| Some(t.timestamp_millis()))
                .ok_or_else(|| ApiError::param(k, format!("{k} 必須是 RFC 3339 時間"))),
        }
    };
    let scope = match body.get("scope") {
        None | Some(Value::Null) => None,
        Some(s) => {
            if s.get("groups").is_some() {
                return Err(ApiError::param(
                    "scope.groups",
                    "主機清冊還沒有群組，範圍請用標籤、網段或主機",
                ));
            }
            Some(
                serde_json::from_value::<Scope>(s.clone())
                    .map_err(|e| ApiError::param("scope", e.to_string()))?,
            )
        }
    };
    let rules = match body.get("rules") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            serde_json::from_value::<Vec<String>>(v.clone())
                .map_err(|_| ApiError::param("rules", "rules 必須是字串陣列"))?,
        ),
    };
    Ok(SilenceInput {
        kind: body.get("kind").and_then(Value::as_str).map(str::to_string),
        name: body.get("name").and_then(Value::as_str).map(str::to_string),
        scope,
        rules,
        starts_at: time("starts_at")?,
        ends_at: time("ends_at")?,
        duration_secs: body.get("duration_secs").and_then(Value::as_u64),
        reason: body
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        notify_on_end: body.get("notify_on_end").and_then(Value::as_bool),
        exclude_uptime: body.get("exclude_uptime").and_then(Value::as_bool),
        from_event: body
            .get("from_event")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

async fn from_event(
    c: &Ctx<'_>,
    id: Option<&str>,
) -> Result<Option<crate::alerts::model::Event>, ApiError> {
    let Some(id) = id else {
        return Ok(None);
    };
    let id2 = id.to_string();
    let ev = c
        .svc
        .db
        .read(move |conn| Ok(conn.get_event(&id2)?))
        .await
        .map_err(db_error)?;
    ev.map(Some)
        .ok_or_else(|| ApiError::param("from_event", format!("沒有事件 {id}")))
}

async fn list_silences(c: &Ctx<'_>, q: &Query) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(&["status", "limit"], &[])?;
    let status = q.get("status").unwrap_or("all").to_string();
    if !matches!(status.as_str(), "active" | "upcoming" | "ended" | "all") {
        return Err(ApiError::param(
            "status",
            "status 必須是 active、upcoming、ended 或 all",
        ));
    }
    let limit = limit_param(q, 200, 1000)?;
    let now = c.now_ms();
    let list = c
        .svc
        .db
        .read(move |conn| Ok(conn.list_silences(&status, now, limit)?))
        .await
        .map_err(db_error)?;
    let mut data = Vec::new();
    for s in &list {
        let n = c.svc.matching_hosts(&s.scope).await;
        data.push(ops::silence_json(s, now, Some(n)));
    }
    Ok(http::ok(c.now, data))
}

async fn get_silence(c: &Ctx<'_>, id: &str) -> Result<Response<ApiBody>, ApiError> {
    let id2 = id.to_string();
    let s = c
        .svc
        .db
        .read(move |conn| Ok(conn.get_silence(&id2)?))
        .await
        .map_err(db_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "silence_not_found",
                format!("沒有靜音 {id}"),
            )
        })?;
    let n = c.svc.matching_hosts(&s.scope).await;
    Ok(http::ok(c.now, ops::silence_json(&s, c.now_ms(), Some(n))))
}

async fn preview_silence(c: &Ctx<'_>, body: &Value) -> Result<Response<ApiBody>, ApiError> {
    let input = silence_input(body)?;
    let kind = input
        .kind
        .as_deref()
        .map_or(Some(SilenceKind::Silence), SilenceKind::parse)
        .ok_or_else(|| ApiError::param("kind", "kind 必須是 silence 或 maintenance"))?;
    let scope = scope::normalize(&input.scope.clone().unwrap_or_default())
        .map_err(|(f, m)| ApiError::param(f, m))?;
    let ev = from_event(c, input.from_event.as_deref()).await?;
    let rules = c.svc.rules().await;
    let matching = c.svc.matching_hosts(&scope).await;
    let plan = ops::plan(
        kind,
        scope.is_all(),
        input.rules.as_deref().unwrap_or(&[]),
        &rules,
        ev.as_ref()
            .is_some_and(|e| e.severity == Severity::Emergency),
        matching,
    )
    .map_err(invalid)?;
    let rule_names: Vec<String> = input
        .rules
        .clone()
        .unwrap_or_default()
        .iter()
        .filter_map(|id| {
            rules
                .iter()
                .find(|r| &r.rule_id == id)
                .map(|r| r.name.clone())
        })
        .collect();
    Ok(http::ok(
        c.now,
        json!({
            "matching_hosts": matching,
            "scope": scope,
            "scope_text": scope::sentence(&scope),
            "rules_text": if rule_names.is_empty() { "所有規則（以主機為範圍的靜音不抑制系統事件）".to_string() } else { format!("規則：{}", rule_names.join("、")) },
            "limit": match kind {
                SilenceKind::Silence => json!({
                    "max_secs": plan.limit.max_secs,
                    "max_text": scope::duration_text(plan.limit.max_secs),
                    "system": plan.limit.system,
                    "emergency": plan.limit.emergency,
                    "reason": plan.limit.reason,
                }),
                SilenceKind::Maintenance => json!({ "max_secs": scope::LIMIT_MAINTENANCE_SECS, "reason": "維護時段不適用臨時靜音的上限，但不抑制系統事件" }),
            },
            "confirm": { "required": !plan.confirm.is_empty(), "reasons": plan.confirm },
        }),
    ))
}

async fn save_silence(
    c: &Ctx<'_>,
    id: Option<&str>,
    body: &Value,
) -> Result<Response<ApiBody>, ApiError> {
    let input = silence_input(body)?;
    let existing = match id {
        Some(id) => {
            let id2 = id.to_string();
            let s = c
                .svc
                .db
                .read(move |conn| Ok(conn.get_silence(&id2)?))
                .await
                .map_err(db_error)?
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::NOT_FOUND,
                        "silence_not_found",
                        format!("沒有靜音 {id}"),
                    )
                })?;
            if s.status(c.now_ms()) == "ended" {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "invalid_state",
                    "已結束的靜音不能修改",
                ));
            }
            if input.kind.as_deref().is_some_and(|k| k != s.kind.as_str()) {
                return Err(ApiError::param("kind", "靜音的種類不能修改"));
            }
            Some(s)
        }
        None => None,
    };
    let ev = from_event(c, input.from_event.as_deref()).await?;
    let rules = c.svc.rules().await;
    let scope_for_count = scope::normalize(
        &input
            .scope
            .clone()
            .or_else(|| existing.as_ref().map(|e| e.scope.clone()))
            .unwrap_or_default(),
    )
    .map_err(|(f, m)| ApiError::param(f, m))?;
    let matching = c.svc.matching_hosts(&scope_for_count).await;
    let (silence, plan) = ops::validate_silence(
        &input,
        existing.as_ref(),
        &rules,
        ev.as_ref(),
        matching,
        &c.actor,
        c.now_ms(),
    )
    .map_err(invalid)?;
    let s2 = silence.clone();
    c.svc
        .db
        .write(move |conn| Ok(conn.save_silence(&s2)?))
        .await
        .map_err(db_error)?;
    c.svc.request_reload();
    c.audit(
        if existing.is_some() {
            "silence.update"
        } else {
            "silence.create"
        },
        Some(&silence.silence_id),
        "ok",
        json!({
            "kind": silence.kind,
            "name": silence.name,
            "scope": scope::sentence(&silence.scope),
            "rules": silence.rules,
            "starts_at": ts_ms(silence.starts_at),
            "ends_at": ts_ms(silence.ends_at),
            "reason": silence.reason,
            "matching_hosts": matching,
            "confirm": plan.confirm,
        }),
    )
    .await;
    let mut data = ops::silence_json(&silence, c.now_ms(), Some(matching));
    data["warnings"] = json!(plan.confirm);
    Ok(if existing.is_some() {
        http::ok(c.now, data)
    } else {
        created(c.now, data)
    })
}

async fn end_silence(c: &Ctx<'_>, id: &str, body: &Value) -> Result<Response<ApiBody>, ApiError> {
    let id2 = id.to_string();
    let mut s = c
        .svc
        .db
        .read(move |conn| Ok(conn.get_silence(&id2)?))
        .await
        .map_err(db_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "silence_not_found",
                format!("沒有靜音 {id}"),
            )
        })?;
    let now = c.now_ms();
    if s.status(now) == "ended" {
        return Ok(http::ok(c.now, ops::silence_json(&s, now, None)));
    }
    s.ended_at = Some(now);
    s.ended_by = Some(c.actor.clone());
    s.end_reason = body
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    s.updated_at = now;
    s.updated_by = c.actor.clone();
    let s2 = s.clone();
    c.svc
        .db
        .write(move |conn| Ok(conn.save_silence(&s2)?))
        .await
        .map_err(db_error)?;
    c.svc.request_reload();
    c.audit(
        "silence.end",
        Some(id),
        "ok",
        json!({ "reason": s.end_reason }),
    )
    .await;
    Ok(http::ok(c.now, ops::silence_json(&s, now, None)))
}

// ---------------------------------------------------------------------------
// 通知管道
// ---------------------------------------------------------------------------

/// 驗證管道的設定與機密；回傳（不含機密的設定, 機密）
pub fn validate_channel(
    kind: ChannelKind,
    config: &Value,
    secrets: &Value,
) -> Result<(Value, Value), ApiError> {
    let cfg = config.as_object().cloned().unwrap_or_default();
    let sec = secrets.as_object().cloned().unwrap_or_default();
    let bad = |f: &str, m: String| {
        ApiError::new(StatusCode::BAD_REQUEST, "channel_invalid", m)
            .with_product_code(CODE_CHANNEL_CONFIG)
            .with_details(json!({ "parameter": f }))
    };
    let common = [
        "timeout_secs",
        "timezone",
        "show_ip",
        "min_severity",
        "notify_ack",
        "ca_file",
    ];
    let allowed: Vec<&str> = match kind {
        ChannelKind::Email => vec!["host", "port", "security", "username", "from", "to", "helo"],
        ChannelKind::Webhook => vec!["format", "method", "allow_http", "line_to"],
    };
    for k in cfg.keys() {
        if !allowed.contains(&k.as_str()) && !common.contains(&k.as_str()) {
            return Err(bad(
                &format!("config.{k}"),
                format!("不認得的設定 {k}（機密請放在 secrets）"),
            ));
        }
    }
    let secret_keys: &[&str] = match kind {
        ChannelKind::Email => &["password"],
        ChannelKind::Webhook => &["url", "headers", "hmac_secret", "line_token"],
    };
    for k in sec.keys() {
        if !secret_keys.contains(&k.as_str()) {
            return Err(bad(
                &format!("secrets.{k}"),
                format!("不認得的機密欄位 {k}"),
            ));
        }
    }
    if let Some(t) = cfg.get("timeout_secs")
        && !t.as_u64().is_some_and(|t| (1..=60).contains(&t))
    {
        return Err(bad(
            "config.timeout_secs",
            "timeout_secs 必須是 1～60".into(),
        ));
    }
    if let Some(t) = cfg.get("timezone")
        && !t
            .as_str()
            .is_some_and(|t| crate::config::alerts_parse_offset(t).is_some())
    {
        return Err(bad(
            "config.timezone",
            "timezone 必須是固定位移，例如 +08:00".into(),
        ));
    }
    if let Some(v) = cfg.get("show_ip")
        && !matches!(v.as_str(), Some("full" | "subnet" | "none"))
    {
        return Err(bad(
            "config.show_ip",
            "show_ip 必須是 full、subnet 或 none".into(),
        ));
    }
    if let Some(v) = cfg.get("min_severity")
        && !v.as_str().is_some_and(|s| Severity::parse(s).is_some())
    {
        return Err(bad(
            "config.min_severity",
            "min_severity 必須是 emergency、critical、warning".into(),
        ));
    }
    if let Some(p) = cfg.get("ca_file").and_then(Value::as_str)
        && !p.trim().is_empty()
        && let Err(e) = crate::alerts::notify::transport::client_config(Some(p))
    {
        return Err(bad("config.ca_file", e));
    }
    let s = |k: &str| {
        cfg.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
    };
    match kind {
        ChannelKind::Email => {
            if s("host").is_empty() || s("host").contains(char::is_whitespace) {
                return Err(bad("config.host", "SMTP 主機（host）必填".into()));
            }
            if let Some(p) = cfg.get("port")
                && !p.as_u64().is_some_and(|p| (1..=65535).contains(&p))
            {
                return Err(bad("config.port", "port 必須是 1～65535".into()));
            }
            let security = if s("security").is_empty() {
                "starttls"
            } else {
                s("security")
            };
            if !matches!(security, "starttls" | "tls" | "none") {
                return Err(bad(
                    "config.security",
                    "security 必須是 starttls、tls 或 none".into(),
                ));
            }
            if !valid_address(s("from")) {
                return Err(bad("config.from", "寄件者（from）的格式不正確".into()));
            }
            let to: Vec<String> = cfg
                .get("to")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if to.is_empty() || to.len() > 50 {
                return Err(bad("config.to", "收件者（to）必須有 1～50 個".into()));
            }
            if let Some(b) = to.iter().find(|a| !valid_address(a)) {
                return Err(bad("config.to", format!("收件者 {b} 的格式不正確")));
            }
            let has_pw = sec
                .get("password")
                .and_then(Value::as_str)
                .is_some_and(|p| !p.is_empty());
            if security == "none" && has_pw {
                return Err(bad(
                    "secrets.password",
                    "security = none 時不可設定密碼（密碼會以明文傳送）".into(),
                ));
            }
            if has_pw && s("username").is_empty() {
                return Err(bad(
                    "config.username",
                    "設定密碼時需要帳號（username）".into(),
                ));
            }
        }
        ChannelKind::Webhook => {
            let format = if s("format").is_empty() {
                "kairos"
            } else {
                s("format")
            };
            if !matches!(format, "kairos" | "slack" | "teams" | "line") {
                return Err(bad(
                    "config.format",
                    "format 必須是 kairos、slack、teams 或 line".into(),
                ));
            }
            if !matches!(s("method"), "" | "POST" | "PUT") {
                return Err(bad("config.method", "method 必須是 POST 或 PUT".into()));
            }
            let allow_http = cfg
                .get("allow_http")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match sec.get("url").and_then(Value::as_str) {
                Some(u) => {
                    parse_url(u, allow_http).map_err(|e| bad("secrets.url", e))?;
                }
                None if format == "line" => {}
                None => return Err(bad("secrets.url", "Webhook 網址（secrets.url）必填".into())),
            }
            if format == "line" {
                if s("line_to").is_empty() {
                    return Err(bad(
                        "config.line_to",
                        "LINE 的推播對象（使用者或群組 ID，config.line_to）必填".into(),
                    ));
                }
                if !sec
                    .get("line_token")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
                {
                    return Err(bad(
                        "secrets.line_token",
                        "LINE channel access token（secrets.line_token）必填".into(),
                    ));
                }
            }
            if let Some(h) = sec.get("headers") {
                let Some(map) = h.as_object() else {
                    return Err(bad("secrets.headers", "headers 必須是物件".into()));
                };
                for (k, v) in map {
                    let ok_name =
                        !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
                    let ok_value = v
                        .as_str()
                        .is_some_and(|v| !v.chars().any(|c| c == '\r' || c == '\n'));
                    if !ok_name
                        || !ok_value
                        || matches!(
                            k.to_ascii_lowercase().as_str(),
                            "host" | "content-length" | "content-type"
                        )
                    {
                        return Err(bad("secrets.headers", format!("標頭 {k} 無效")));
                    }
                }
            }
        }
    }
    Ok((Value::Object(cfg), Value::Object(sec)))
}

fn channel_json(ch: &Channel, rules_using: Option<usize>) -> Value {
    json!({
        "channel_id": ch.channel_id,
        "name": ch.name,
        "kind": ch.kind,
        "enabled": ch.enabled,
        "config": ch.config,
        "secrets": masked(&ch.secrets),
        "state": if ch.enabled { ch.state.as_str() } else { "disabled" },
        "consecutive_failures": ch.consecutive_failures,
        "failed_since": ch.failed_since.map(ts_ms),
        "last_attempt_at": ch.last_attempt_at.map(ts_ms),
        "last_success_at": ch.last_success_at.map(ts_ms),
        "last_error_code": ch.last_error_code,
        "last_error": ch.last_error.as_deref().map(|e| scrub(e, &ch.secrets)),
        "rules_using": rules_using,
        "created_at": ts_ms(ch.created_at),
        "created_by": ch.created_by,
        "updated_at": ts_ms(ch.updated_at),
        "updated_by": ch.updated_by,
    })
}

async fn list_channels(c: &Ctx<'_>) -> Result<Response<ApiBody>, ApiError> {
    let sb = c.svc.secrets.clone();
    let (channels, rules) = c
        .svc
        .db
        .read(move |conn| Ok((conn.load_channels(Some(sb.as_ref()))?, conn.load_rules()?)))
        .await
        .map_err(db_error)?;
    let data: Vec<Value> = channels
        .iter()
        .map(|ch| {
            let live = c.svc.channel_status(&ch.channel_id);
            let ch = live.as_ref().map_or(ch, |l| l);
            let using = rules
                .iter()
                .filter(|r| {
                    r.channels.contains(&ch.channel_id)
                        || r.notify.escalate_channels.contains(&ch.channel_id)
                })
                .count();
            channel_json(ch, Some(using))
        })
        .collect();
    Ok(http::ok(c.now, data))
}

async fn load_channel(c: &Ctx<'_>, id: &str) -> Result<Channel, ApiError> {
    let (sb, id2) = (c.svc.secrets.clone(), id.to_string());
    c.svc
        .db
        .read(move |conn| Ok(conn.get_channel(&id2, Some(sb.as_ref()))?))
        .await
        .map_err(db_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "channel_not_found",
                format!("沒有通知管道 {id}"),
            )
        })
}

async fn get_channel(c: &Ctx<'_>, id: &str) -> Result<Response<ApiBody>, ApiError> {
    let ch = load_channel(c, id).await?;
    let id2 = id.to_string();
    let (deliveries, rules) = c
        .svc
        .db
        .read(move |conn| {
            Ok((
                conn.deliveries_of_channel(&id2, 20)?,
                conn.rules_using_channel(&id2)?,
            ))
        })
        .await
        .map_err(db_error)?;
    let live = c.svc.channel_status(id);
    let mut data = channel_json(live.as_ref().unwrap_or(&ch), Some(rules.len()));
    data["rules"] = json!(
        rules
            .iter()
            .map(|(id, name)| json!({ "rule_id": id, "name": name }))
            .collect::<Vec<_>>()
    );
    data["deliveries"] = json!(
        deliveries
            .iter()
            .map(|d| store::delivery_json(d, Some(&ch.name)))
            .collect::<Vec<_>>()
    );
    Ok(http::ok(c.now, data))
}

fn name_of(body: &Value, base: Option<&Channel>) -> Result<String, ApiError> {
    let n = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| base.map(|b| b.name.clone()))
        .ok_or_else(|| ApiError::param("name", "name 必填"))?;
    let n = n.trim().to_string();
    if n.is_empty() || n.chars().count() > 100 || n.chars().any(char::is_control) {
        return Err(ApiError::param("name", "名稱必須是 1～100 個字元"));
    }
    Ok(n)
}

async fn save_channel(
    c: &Ctx<'_>,
    id: Option<&str>,
    body: &Value,
) -> Result<Response<ApiBody>, ApiError> {
    let base = match id {
        Some(id) => Some(load_channel(c, id).await?),
        None => None,
    };
    let kind = match (&base, body.get("kind").and_then(Value::as_str)) {
        (Some(b), Some(k)) if k != b.kind.as_str() => {
            return Err(ApiError::param("kind", "管道的種類不能修改"));
        }
        (Some(b), _) => b.kind,
        (None, Some(k)) => ChannelKind::parse(k)
            .ok_or_else(|| ApiError::param("kind", "kind 必須是 email 或 webhook"))?,
        (None, None) => return Err(ApiError::param("kind", "kind 必填（email 或 webhook）")),
    };
    let name = name_of(body, base.as_ref())?;
    let config = body
        .get("config")
        .cloned()
        .or_else(|| base.as_ref().map(|b| b.config.clone()))
        .unwrap_or_else(|| json!({}));
    // 機密：沒有提供的欄位沿用原本的值；值為 null 時刪除（只能更換，不能檢視，S11）
    let mut secrets: Map<String, Value> = base
        .as_ref()
        .and_then(|b| b.secrets.as_object().cloned())
        .unwrap_or_default();
    let mut secrets_changed = base.is_none();
    if let Some(Value::Object(new)) = body.get("secrets") {
        for (k, v) in new {
            secrets_changed = true;
            if v.is_null() {
                secrets.remove(k);
            } else {
                secrets.insert(k.clone(), v.clone());
            }
        }
    }
    let (config, secrets) = validate_channel(kind, &config, &Value::Object(secrets))?;
    let now = c.now_ms();
    let ch = Channel {
        channel_id: base
            .as_ref()
            .map_or_else(|| ops::new_id("c"), |b| b.channel_id.clone()),
        name,
        kind,
        enabled: body
            .get("enabled")
            .and_then(Value::as_bool)
            .or(base.as_ref().map(|b| b.enabled))
            .unwrap_or(true),
        config,
        secrets: secrets.clone(),
        state: base
            .as_ref()
            .map_or_else(|| "ok".to_string(), |b| b.state.clone()),
        consecutive_failures: base.as_ref().map_or(0, |b| b.consecutive_failures),
        failed_since: base.as_ref().and_then(|b| b.failed_since),
        last_attempt_at: base.as_ref().and_then(|b| b.last_attempt_at),
        last_success_at: base.as_ref().and_then(|b| b.last_success_at),
        last_error_code: base.as_ref().and_then(|b| b.last_error_code.clone()),
        last_error: base.as_ref().and_then(|b| b.last_error.clone()),
        created_at: base.as_ref().map_or(now, |b| b.created_at),
        created_by: base
            .as_ref()
            .map_or_else(|| c.actor.clone(), |b| b.created_by.clone()),
        updated_at: now,
        updated_by: c.actor.clone(),
    };
    let sealed = if secrets_changed {
        Some(c.svc.secrets.seal(&secrets).map_err(|e| {
            log::warn!(event = "alerts.channel.encrypt_failed"; "無法加密通知管道的機密：{e}");
            ApiError::internal()
        })?)
    } else {
        None
    };
    let ch2 = ch.clone();
    c.svc
        .db
        .write(move |conn| Ok(conn.save_channel(&ch2, sealed)?))
        .await
        .map_err(db_error)?;
    c.svc.request_reload();
    // 稽核不記錄機密的值，只記錄更換了哪些欄位
    let changed_secrets: Vec<String> = body
        .get("secrets")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    c.audit(
        if base.is_some() { "channel.update" } else { "channel.create" },
        Some(&ch.channel_id),
        "ok",
        json!({ "name": ch.name, "kind": ch.kind, "config": ch.config, "secrets_changed": changed_secrets }),
    )
    .await;
    let data = channel_json(&ch, None);
    Ok(if base.is_some() {
        http::ok(c.now, data)
    } else {
        created(c.now, data)
    })
}

async fn delete_channel(
    c: &Ctx<'_>,
    id: &str,
    body: &Value,
) -> Result<Response<ApiBody>, ApiError> {
    let ch = load_channel(c, id).await?;
    let id2 = id.to_string();
    let using = c
        .svc
        .db
        .read(move |conn| Ok(conn.rules_using_channel(&id2)?))
        .await
        .map_err(db_error)?;
    // L2：輸入管道名稱確認；有規則使用時需指定替代管道或確認「這些規則將沒有通知對象」
    let confirm = body
        .get("confirm_name")
        .and_then(Value::as_str)
        .map(str::trim);
    let replacement = body
        .get("replacement_channel_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let accept = body
        .get("accept_no_channel")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let affected = json!(
        using
            .iter()
            .map(|(id, name)| json!({ "rule_id": id, "name": name }))
            .collect::<Vec<_>>()
    );
    if confirm != Some(ch.name.as_str()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "confirmation_required",
            "刪除通知管道需要輸入管道名稱確認（confirm_name，L2）",
        )
        .with_details(json!({ "parameter": "confirm_name", "rules": affected })));
    }
    if !using.is_empty() && replacement.is_none() && !accept {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "confirmation_required", "有規則使用這個管道：請指定替代管道（replacement_channel_id），或確認這些規則將沒有通知對象（accept_no_channel）")
            .with_details(json!({ "parameter": "replacement_channel_id", "rules": affected })));
    }
    if let Some(r) = &replacement
        && (r == id || load_channel(c, r).await.is_err())
    {
        return Err(ApiError::param(
            "replacement_channel_id",
            format!("沒有通知管道 {r}"),
        ));
    }
    let (id3, actor, now, rep) = (
        id.to_string(),
        c.actor.clone(),
        c.now_ms(),
        replacement.clone(),
    );
    c.svc
        .db
        .write(move |conn| {
            let tx = conn.begin(TxMode::Deferred)?;
            for mut r in tx.load_rules()? {
                let before = (r.channels.clone(), r.notify.escalate_channels.clone());
                for list in [&mut r.channels, &mut r.notify.escalate_channels] {
                    let had = list.iter().any(|c| c == &id3);
                    list.retain(|c| c != &id3);
                    if had
                        && let Some(rep) = &rep
                        && !list.contains(rep)
                    {
                        list.push(rep.clone());
                    }
                }
                if (r.channels.clone(), r.notify.escalate_channels.clone()) != before {
                    r.updated_at = now;
                    r.updated_by = actor.clone();
                    tx.save_rule(&r)?;
                }
            }
            tx.delete_channel(&id3)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(db_error)?;
    c.svc.request_reload();
    c.audit(
        "channel.delete",
        Some(id),
        "ok",
        json!({ "name": ch.name, "rules": affected, "replacement": replacement }),
    )
    .await;
    Ok(http::ok(
        c.now,
        json!({ "deleted": true, "channel_id": id, "rules_updated": using.len() }),
    ))
}

async fn test_channel(
    c: &Ctx<'_>,
    id: Option<&str>,
    body: &Value,
) -> Result<Response<ApiBody>, ApiError> {
    let severity = match body.get("severity").and_then(Value::as_str) {
        None => Severity::Emergency,
        Some(s) => Severity::parse(s)
            .filter(|s| *s != Severity::Info)
            .ok_or_else(|| {
                ApiError::param("severity", "severity 必須是 emergency、critical 或 warning")
            })?,
    };
    let ch = match id {
        Some(id) => load_channel(c, id).await?,
        None => {
            let kind = body
                .get("kind")
                .and_then(Value::as_str)
                .and_then(ChannelKind::parse)
                .ok_or_else(|| ApiError::param("kind", "kind 必須是 email 或 webhook"))?;
            let (config, secrets) = validate_channel(
                kind,
                body.get("config").unwrap_or(&json!({})),
                body.get("secrets").unwrap_or(&json!({})),
            )?;
            Channel {
                channel_id: "unsaved".into(),
                name: body
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("（未儲存）")
                    .to_string(),
                kind,
                enabled: true,
                config,
                secrets,
                state: "ok".into(),
                consecutive_failures: 0,
                failed_since: None,
                last_attempt_at: None,
                last_success_at: None,
                last_error_code: None,
                last_error: None,
                created_at: 0,
                created_by: String::new(),
                updated_at: 0,
                updated_by: String::new(),
            }
        }
    };
    let now = c.now_ms();
    // 限速（每管道、每使用者）：超過時不送出、不寫發送紀錄
    if let Err(l) = c.svc.check_test_limit(id, &c.actor, now) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            match l.scope {
                "channel" => format!(
                    "這個管道的測試通知太頻繁（{} 秒內最多 {} 次），請 {} 秒後再試",
                    l.window_secs, l.limit, l.retry_after_secs
                ),
                _ => format!(
                    "你的測試通知太頻繁（{} 分鐘內最多 {} 次），請 {} 秒後再試",
                    l.window_secs / 60,
                    l.limit,
                    l.retry_after_secs
                ),
            },
        )
        .with_product_code(crate::alerts::test_limit::CODE_TEST_RATE_LIMITED)
        .with_details(json!({
            "scope": l.scope,
            "limit": l.limit,
            "window_secs": l.window_secs,
        }))
        .with_retry_after(l.retry_after_secs));
    }
    let o = crate::alerts::notify::send_test(&ch, severity, &c.svc.dispatch_config(), now).await;
    let message = scrub(&o.message, &ch.secrets);
    if id.is_some() {
        // 測試的結果記在發送紀錄（S11「最近發送」）
        let (cid, ok, code, msg, rc) = (
            ch.channel_id.clone(),
            o.ok,
            o.code,
            message.clone(),
            o.response_code,
        );
        let _ = c
            .svc
            .db
            .write(move |conn| {
                let tx = conn.begin(TxMode::Deferred)?;
                let ids = tx.insert_deliveries(&[crate::alerts::engine::NewDelivery {
                    event_id: None,
                    channel_id: cid.clone(),
                    kind: "test",
                    urgent: false,
                    payload: json!({ "kind": "test" }),
                    group: None,
                    created_at: now,
                }])?;
                for i in ids {
                    tx.update_delivery(&store::Delivery {
                        id: i,
                        event_id: None,
                        channel_id: cid.clone(),
                        kind: "test".into(),
                        state: if ok { "sent".into() } else { "failed".into() },
                        urgent: false,
                        payload: Value::Null,
                        attempts: 1,
                        created_at: now,
                        next_attempt_at: now,
                        sent_at: ok.then_some(now),
                        batch_id: None,
                        response_code: rc.map(i64::from),
                        error_code: code.map(str::to_string),
                        error: (!ok).then_some(msg.clone()),
                    })?;
                }
                tx.commit()?;
                Ok(())
            })
            .await;
        c.audit(
            "channel.test",
            id,
            if o.ok { "ok" } else { "failed" },
            json!({ "severity": severity, "code": o.code }),
        )
        .await;
    }
    let _ = c.api;
    Ok(http::ok(
        c.now,
        json!({
            "ok": o.ok,
            "product_code": o.code,
            "message": message,
            "response_code": o.response_code,
            "elapsed_ms": o.elapsed_ms,
            "severity": severity,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_table_matches_the_documented_permissions() {
        for (method, path, perm) in ROUTES {
            let segs: Vec<String> = path
                .trim_start_matches('/')
                .split('/')
                .map(|s| {
                    if s.starts_with('{') {
                        "x1".to_string()
                    } else {
                        s.to_string()
                    }
                })
                .collect();
            let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
            let ["api", "v1", rest @ ..] = segs.as_slice() else {
                panic!("{path}");
            };
            let (_, p) = route(method, rest).unwrap_or_else(|| panic!("{method} {path}"));
            assert_eq!(p, *perm, "{method} {path}");
        }
        assert!(route("GET", &["alerts", "x", "ack"]).is_none());
        assert!(
            owns("/api/v1/alerts") && owns("/api/v1/channels/c-1/test") && !owns("/api/v1/alertsx")
        );
        // 讀取為唯讀以上、處理為維運者以上、管道的設定為管理員（角色對應在 crate::rbac）
        use crate::rbac::Role;
        assert_eq!(Permission::AlertsRead.least_role(), Role::Viewer);
        assert_eq!(Permission::AlertsWrite.least_role(), Role::Operator);
        assert_eq!(Permission::ChannelsRead.least_role(), Role::Operator);
        assert_eq!(Permission::ChannelsWrite.least_role(), Role::Admin);
        assert_eq!(Permission::AlertRulesAdmin.least_role(), Role::Admin);
        assert_eq!(Permission::SilencesWrite.name(), "silences:write");
        // 每個寫入的端點都不是唯讀的權限；每個權限都在 rbac 的列舉中
        for (method, _, perm) in ROUTES {
            assert!(crate::rbac::Permission::ALL.contains(perm));
            if *method != "GET"
                && !matches!(perm, Permission::AlertRulesRead | Permission::SilencesRead)
            {
                assert!(
                    !crate::rbac::Role::Viewer.permissions().contains(*perm),
                    "{method} {perm:?}"
                );
            }
        }
    }

    #[test]
    fn channel_validation_rejects_insecure_or_incomplete_settings() {
        let ok = validate_channel(
            ChannelKind::Email,
            &json!({"host": "mail.example.com", "from": "k@example.com", "to": ["a@example.com"], "username": "k"}),
            &json!({"password": "p"}),
        );
        assert!(ok.is_ok());
        let e = validate_channel(
            ChannelKind::Email,
            &json!({"host": "mail.example.com", "security": "none", "from": "k@example.com", "to": ["a@example.com"], "username": "k"}),
            &json!({"password": "p"}),
        )
        .unwrap_err();
        assert_eq!(e.details.unwrap()["parameter"], "secrets.password");
        assert!(
            validate_channel(
                ChannelKind::Email,
                &json!({"host": "h", "from": "bad", "to": ["a@b.c"]}),
                &json!({})
            )
            .is_err()
        );
        assert!(
            validate_channel(
                ChannelKind::Email,
                &json!({"host": "h", "from": "a@b.c", "to": ["a@b.c"], "password": "x"}),
                &json!({})
            )
            .is_err()
        );
        assert!(
            validate_channel(
                ChannelKind::Webhook,
                &json!({}),
                &json!({"url": "http://x/"})
            )
            .is_err()
        );
        assert!(
            validate_channel(
                ChannelKind::Webhook,
                &json!({"allow_http": true}),
                &json!({"url": "http://x/"})
            )
            .is_ok()
        );
        assert!(
            validate_channel(ChannelKind::Webhook, &json!({"format": "line"}), &json!({})).is_err()
        );
        assert!(
            validate_channel(
                ChannelKind::Webhook,
                &json!({"format": "line", "line_to": "C1"}),
                &json!({"line_token": "t"})
            )
            .is_ok()
        );
        assert!(
            validate_channel(
                ChannelKind::Webhook,
                &json!({}),
                &json!({"url": "https://x/", "headers": {"X-A": "a\r\nb"}})
            )
            .is_err()
        );
        assert!(
            validate_channel(
                ChannelKind::Webhook,
                &json!({}),
                &json!({"url": "https://x/", "password": "p"})
            )
            .is_err()
        );
    }
}
