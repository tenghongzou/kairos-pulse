//! 稽核日誌（S18；`kairos/API.md` 第 12.8 節）：查詢、匯出（JSONL）與完整性驗證。
//!
//! | 方法與路徑 | 權限 | 說明 |
//! |---|---|---|
//! | `GET /api/v1/audit` | `audit:read`（或 `audit:read_own`：只看自己的） | 新到舊，游標分頁 |
//! | `GET /api/v1/audit/export` | `audit:export` | JSONL（每行一筆，含 `prev_hash`、`hash`；最後一行為鏈的起訖值） |
//! | `POST /api/v1/audit/verify` | `audit:read` | 重新計算雜湊鏈；斷裂時 `KRS-E6201` |
//!
//! 稽核員與管理員自己的查詢、匯出與驗證也會被記錄（設計包 `ia.md` §4.1）；沒有任何角色可以修改或刪除。

use chrono::{DateTime, Utc};
use hyper::body::Bytes;
use hyper::header::{CONTENT_DISPOSITION, CONTENT_TYPE, HeaderValue};
use hyper::{Method, Response, StatusCode};
use log::error;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::ApiContext;
use super::account::{audit as write_audit, storage_error};
use super::auth::{Permission, Principal};
use super::db::query_failed;
use super::http::{self, ApiBody, ApiError, Query, ts};
use crate::audit::{self, AuditFilter, CODE_CHAIN_BROKEN, Event};
use crate::store::prelude::*;

/// 鏈的一端：(id, 雜湊)
type ChainEnd = (i64, Option<String>);

/// 列表的筆數上限
pub const MAX_LIMIT: usize = 1000;
/// 匯出每批讀幾筆
pub const EXPORT_BATCH: usize = 2000;

const LIST_PARAMS: [&str; 9] = [
    "actor", "category", "action", "target", "result", "from", "to", "limit", "cursor",
];

fn parse_time(q: &Query, name: &str) -> Result<Option<i64>, ApiError> {
    match q.get(name) {
        None => Ok(None),
        Some(v) => DateTime::parse_from_rfc3339(v)
            .map(|t| Some(t.timestamp_millis()))
            .map_err(|_| ApiError::param(name, format!("{name} 必須是 RFC 3339 時間"))),
    }
}

/// 查詢條件；只能看自己的操作時固定 `actor`
fn filter_of(q: &Query, p: &Principal) -> Result<(AuditFilter, bool), ApiError> {
    let own_only = !p.grants.has(Permission::AuditRead);
    if own_only && !p.grants.has(Permission::AuditReadOwn) {
        p.require(Permission::AuditRead)?;
    }
    let result = q.get("result").map(str::to_string);
    if let Some(r) = &result
        && !matches!(r.as_str(), "ok" | "failed" | "denied" | "rejected")
    {
        return Err(ApiError::param(
            "result",
            "result 必須是 ok、failed、denied 或 rejected",
        ));
    }
    let f = AuditFilter {
        actor: if own_only {
            Some(p.actor())
        } else {
            q.get("actor").map(str::to_string)
        },
        category: q.get("category").map(str::to_string),
        action: q.get("action").map(str::to_string),
        target: q.get("target").map(str::to_string),
        result,
        from_ms: parse_time(q, "from")?,
        to_ms: parse_time(q, "to")?,
        before_id: None,
        after_id: None,
    };
    Ok((f, own_only))
}

fn filter_json(f: &AuditFilter) -> Value {
    json!({
        "actor": f.actor, "category": f.category, "action": f.action, "target": f.target,
        "result": f.result, "from": f.from_ms.map(http::ts_ms), "to": f.to_ms.map(http::ts_ms),
    })
}

pub async fn handle(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    rest: &[&str],
    method: &Method,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let get = *method == Method::GET || *method == Method::HEAD;
    match (get, rest) {
        (true, []) => list(ctx, p, peer, q, now).await,
        (true, ["export"]) => export(ctx, p, peer, q, now).await,
        (false, ["verify"]) if *method == Method::POST => verify(ctx, p, peer, now).await,
        (_, [] | ["export"] | ["verify"]) => Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "稽核日誌不能修改或刪除",
        )),
        _ => Err(ApiError::not_found()),
    }
}

async fn list(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(&LIST_PARAMS, &[])?;
    let (mut f, own_only) = filter_of(q, p)?;
    let limit = match q.get("limit") {
        None => 100,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| ApiError::param("limit", format!("limit 必須介於 1～{MAX_LIMIT}")))?,
    };
    let first_page = q.get("cursor").is_none();
    if let Some(c) = q.get("cursor") {
        f.before_id = Some(c.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(|| {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid_cursor", "cursor 無效")
        })?);
    }
    let f2 = f.clone();
    let (rows, total) = ctx
        .pool
        .read("稽核日誌", move |c| {
            c.audit_query(&f2, limit).map_err(query_failed("稽核日誌"))
        })
        .await?;
    // 頁首「完整性」（設計包 S18 0.7）：最近一次驗證（自我驗證或手動）與 HMAC 的起點；只看自己操作的身分沒有
    let integrity = if own_only || !first_page {
        None
    } else {
        Some(
            ctx.pool
                .read("稽核日誌", |c| {
                    integrity_of(c).map_err(query_failed("稽核日誌"))
                })
                .await?,
        )
    };
    let next = (rows.len() == limit)
        .then(|| rows.last().map(|r| r.id.to_string()))
        .flatten();
    // 稽核員與管理員的查詢也記錄（只記第一頁，翻頁不重複記錄）
    if first_page {
        write_audit(
            ctx,
            Event {
                actor: p.actor(),
                actor_ip: Some(peer.ip().to_canonical().to_string()),
                category: "audit",
                action: "audit.query",
                object: None,
                result: "ok",
                details: Some(json!({ "filter": filter_json(&f), "matched": total })),
                ..Event::default()
            },
            now.timestamp_millis(),
        )
        .await;
    }
    Ok(http::json(
        StatusCode::OK,
        now,
        json!({
            "data": rows.iter().map(audit::AuditRow::json).collect::<Vec<_>>(),
            "page": { "limit": limit, "total": total, "next_cursor": next },
            "meta": { "own_only": own_only, "integrity": integrity },
        }),
    ))
}

async fn export(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    p.require(Permission::AuditExport)?;
    q.reject_unknown(&LIST_PARAMS[..7], &[])?;
    let (f, _) = filter_of(q, p)?;
    // 匯出本身寫入稽核（先記錄再開始送出）
    write_audit(
        ctx,
        Event {
            actor: p.actor(),
            actor_ip: Some(peer.ip().to_canonical().to_string()),
            category: "audit",
            action: "audit.export",
            object: None,
            result: "ok",
            details: Some(json!({ "filter": filter_json(&f), "format": "jsonl" })),
            ..Event::default()
        },
        now.timestamp_millis(),
    )
    .await;
    let (tx, rx) = mpsc::channel::<Bytes>(8);
    let pool = ctx.pool.clone();
    let generated_at = ts(now);
    tokio::spawn(async move {
        let mut after = 0i64;
        let mut count = 0u64;
        let mut first: Option<ChainEnd> = None;
        let mut last: Option<ChainEnd> = None;
        loop {
            let mut batch = f.clone();
            batch.after_id = Some(after);
            let rows = match pool
                .read("稽核日誌", move |c| {
                    c.audit_export_batch(&batch, EXPORT_BATCH)
                        .map_err(query_failed("稽核日誌"))
                })
                .await
            {
                Ok(r) => r,
                Err(_) => {
                    let _ = tx
                        .send(Bytes::from(
                            json!({"type": "error", "message": "資料庫查詢失敗，匯出不完整"})
                                .to_string()
                                + "\n",
                        ))
                        .await;
                    return;
                }
            };
            if rows.is_empty() {
                break;
            }
            let mut buf = String::new();
            for r in &rows {
                if first.is_none() {
                    first = Some((r.id, r.prev_hash.clone()));
                }
                last = Some((r.id, r.hash.clone()));
                count += 1;
                buf.push_str(&r.export_json().to_string());
                buf.push('\n');
            }
            after = rows.last().map_or(after, |r| r.id);
            if tx.send(Bytes::from(buf)).await.is_err() {
                return;
            }
            if rows.len() < EXPORT_BATCH {
                break;
            }
        }
        let summary = json!({
            "type": "chain",
            "generated_at": generated_at,
            "count": count,
            "first_id": first.as_ref().map(|x| x.0),
            "first_prev_hash": first.and_then(|x| x.1),
            "last_id": last.as_ref().map(|x| x.0),
            "last_hash": last.and_then(|x| x.1),
            "algorithm": "sha256(prev_hash + \"\\n\" + json([id, at_ms, actor, actor_ip, category, action, object, result, code, details_raw]))",
        });
        let _ = tx.send(Bytes::from(summary.to_string() + "\n")).await;
    });
    let mut resp = Response::new(ApiBody::Stream { rx, _permit: None });
    let h = resp.headers_mut();
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!(
        "attachment; filename=\"kairos-audit-{}.jsonl\"",
        now.format("%Y%m%dT%H%M%SZ")
    )) {
        h.insert(CONTENT_DISPOSITION, v);
    }
    h.insert(
        hyper::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    h.insert(
        hyper::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(resp)
}

/// 最近一次完整性驗證（`audit.verify` 紀錄）與目前的 HMAC 起點（`meta.audit_hmac`）
fn integrity_of(c: &dyn Store) -> StoreResult<Value> {
    let since = c.audit_hmac_since()?.map(|(id, _)| id);
    let f = AuditFilter {
        action: Some("audit.verify".into()),
        ..AuditFilter::default()
    };
    let (rows, _) = c.audit_query(&f, 1)?;
    let last_details = rows
        .first()
        .and_then(|r| r.details.as_deref())
        .and_then(|d| serde_json::from_str::<Value>(d).ok())
        .unwrap_or(Value::Null);
    // 重新錨定過（`kairos audit reanchor`；指標還在＝未驗證的區段還沒被保留期限清除）：時間、執行者、原因，
    // 以及「稽核日誌完整性」告警因此保持開啟到什麼時候。`unverified` 取自最近一次驗證的結果
    let reanchor = match c.audit_reanchor_point()? {
        Some(p) => {
            let f = AuditFilter {
                action: Some(audit::ACTION_REANCHOR.into()),
                before_id: Some(p.event_id + 1),
                ..AuditFilter::default()
            };
            let (events, _) = c.audit_query(&f, 1)?;
            let reason = events
                .first()
                .filter(|r| r.id == p.event_id)
                .and_then(|r| r.details.as_deref())
                .and_then(|d| serde_json::from_str::<Value>(d).ok())
                .and_then(|d| d.get("reason").cloned())
                .unwrap_or(Value::Null);
            let unverified = (last_details["reanchor"]["event_id"] == json!(p.event_id))
                .then(|| last_details["reanchor"]["unverified"].clone());
            Some(json!({
                "event_id": p.event_id,
                "at": http::ts_ms(p.at),
                "by": p.by,
                "reason": reason,
                "unverified": unverified,
                "alert_until": http::ts_ms(p.at + audit::REANCHOR_ALERT_HOLD_MS),
            }))
        }
        None => None,
    };
    Ok(json!({
        "hmac_since_id": since,
        "reanchor": reanchor,
        "last_verify": rows.first().map(|r| {
            let d = r
                .details
                .as_deref()
                .and_then(|d| serde_json::from_str::<Value>(d).ok())
                .unwrap_or(Value::Null);
            json!({
                "id": r.id,
                "at": http::ts_ms(r.at),
                "actor": r.actor,
                "result": r.result,
                "self_check": d["via"] == "self_check",
                "checked": d["checked"],
                "broken": d["broken"],
                "hmac_since_id": d["hmac_since_id"],
            })
        }),
    }))
}

async fn verify(
    ctx: &Arc<ApiContext>,
    p: &Principal,
    peer: SocketAddr,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    p.require(Permission::AuditRead)?;
    let now_ms = now.timestamp_millis();
    // 升級前（沒有雜湊）的紀錄先補上（只有第一次），再以唯讀連線驗證
    ctx.admin
        .write(|c| c.audit_seal_legacy())
        .await
        .map_err(storage_error)?;
    let report = ctx
        .pool
        .read("稽核日誌", |c| {
            c.audit_verify().map_err(query_failed("稽核日誌"))
        })
        .await?;
    if let Some(b) = &report.broken {
        error!(
            event = "audit.chain_broken";
            "錯誤 {CODE_CHAIN_BROKEN}：稽核日誌完整性驗證失敗：第 {} 筆（{}）。紀錄可能被修改或刪除，請保全資料庫檔並與匯出或外送的副本比對",
            b.id, b.reason
        );
    }
    write_audit(
        ctx,
        Event {
            actor: p.actor(),
            actor_ip: Some(peer.ip().to_canonical().to_string()),
            category: "audit",
            action: "audit.verify",
            object: None,
            result: if report.ok() { "ok" } else { "failed" },
            code: (!report.ok()).then_some(CODE_CHAIN_BROKEN),
            details: Some(json!({
                "checked": report.checked,
                "hmac": report.hmac.status,
                "hmac_since_id": report.hmac.since_id,
                "broken": report.broken.as_ref().map(|b| json!({"id": b.id, "reason": b.reason})),
                "reanchor": report.reanchor.as_ref().map(|r| json!({"event_id": r.event_id, "unverified": r.unverified})),
            })),
        },
        now_ms,
    )
    .await;
    Ok(http::ok(now, report.json()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 列表的 meta.integrity.reanchor：重新錨定的時間、執行者、原因、未驗證的筆數與告警保持到什麼時候
    #[test]
    fn integrity_reports_the_reanchor_with_reason_and_unverified_count() {
        let dir = std::env::temp_dir().join(format!(
            "kairos-audit-integrity-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let cfg = dir.join("kairos.toml");
        std::fs::write(
            &cfg,
            format!(
                "[storage]\ndata_dir = \"{}\"\n",
                data.display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();
        let conn = crate::token_cli::open(Some(&cfg), true).unwrap().unwrap();
        audit::ensure_key(&data).unwrap();
        let ev = |action: &'static str, details: Value| audit::Audit {
            actor: "ops（uid 1000）",
            category: "audit",
            action,
            result: "ok",
            details: Some(details),
            ..Default::default()
        };
        for i in 0..3 {
            conn.audit_append(i, &ev("audit.query", json!({}))).unwrap();
        }
        assert_eq!(integrity_of(&*conn).unwrap()["reanchor"], Value::Null);
        let at = 1_800_000_000_000;
        let id = conn
            .audit_reanchor(at, &ev(audit::ACTION_REANCHOR, json!({"reason": "事故"})))
            .unwrap()
            .unwrap();
        let v = integrity_of(&*conn).unwrap();
        let r = &v["reanchor"];
        assert_eq!(r["event_id"], id);
        assert_eq!(r["by"], "ops（uid 1000）");
        assert_eq!(r["reason"], "事故");
        assert_eq!(r["at"], http::ts_ms(at));
        assert_eq!(
            r["alert_until"],
            http::ts_ms(at + audit::REANCHOR_ALERT_HOLD_MS)
        );
        // 還沒有驗證過：筆數未知；驗證（寫入 audit.verify 的 details.reanchor）之後帶出
        assert_eq!(r["unverified"], Value::Null);
        let report = conn.audit_verify().unwrap();
        conn.audit_append(
            at + 1,
            &ev(
                "audit.verify",
                json!({"reanchor": {"event_id": id, "unverified": report.reanchor.unwrap().unverified}}),
            ),
        )
        .unwrap();
        assert_eq!(integrity_of(&*conn).unwrap()["reanchor"]["unverified"], 3);
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
