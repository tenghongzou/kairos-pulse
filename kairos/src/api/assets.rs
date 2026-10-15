//! 主機資產與行程排行（`kairos/API.md` 第 5.10 節；`crates/kairos-proto/METRICS.md` 第 10.2 節）。
//!
//! - `GET /api/v1/hosts/{id}/inventory`：目前的資產（Pulse 回報的、與上一次合併後的結果）；沒有資產時以
//!   hello 的 `os` 代替（`source: "hello"`）
//! - `GET /api/v1/hosts/{id}/inventory/changes`：資產變更歷史（新到舊，游標分頁）
//! - `GET /api/v1/hosts/{id}/processes`：時間不晚於 `at` 的最近一筆行程快照
//!
//! 權限沿用 `hosts:read`（群組範圍外的主機回 404，由呼叫端以 `hosts::check_access` 檢查）。查詢資產與
//! 行程不寫稽核紀錄（與趨勢查詢相同；METRICS.md 第 8 節）。

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::sync::Arc;

use super::ApiContext;
use super::db::query_failed;
use super::hosts::parse_agent_id;
use super::http::{ApiError, Query, ts_ms};

/// 變更歷史每頁的筆數：預設、上限
pub const DEFAULT_CHANGES_LIMIT: usize = 50;
pub const MAX_CHANGES_LIMIT: usize = 200;

/// 清冊中的主機編號、hello 的 `os`、開機時間
type HostRow = (i64, Option<String>, Option<i64>);

fn host_row(conn: &Connection, agent_id: &str) -> rusqlite::Result<Option<HostRow>> {
    conn.query_row(
        "SELECT host_id, os, boot_time FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

/// `GET /api/v1/hosts/{agent_id}/inventory`
pub async fn inventory(
    ctx: &Arc<ApiContext>,
    agent_id: &str,
    now: DateTime<Utc>,
) -> Result<hyper::Response<super::http::ApiBody>, ApiError> {
    let agent_id = parse_agent_id(agent_id)?;
    let id = agent_id.clone();
    type Row = (Option<HostRow>, Option<(String, String, i64, i64, i64)>);
    let (host, inv): Row = ctx
        .pool
        .read("主機資產", move |conn| {
            let failed = query_failed("主機資產");
            let host = host_row(conn, &id).map_err(&failed)?;
            let inv = match &host {
                Some((host_id, _, _)) => conn
                    .query_row(
                        "SELECT digest, body, collected_at, received_at, changed_at
                         FROM host_inventory WHERE host_id = ?1",
                        [host_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )
                    .optional()
                    .map_err(&failed)?,
                None => None,
            };
            Ok((host, inv))
        })
        .await?;
    let live = ctx.registry.live_get(&agent_id);
    if host.is_none() && live.is_none() {
        return Err(ApiError::host_not_found(&agent_id));
    }
    let supported = live
        .as_ref()
        .filter(|l| l.seen_since_start)
        .and_then(|l| l.inventory)
        .unwrap_or(inv.is_some());
    let hello_os: Option<Value> = live
        .as_ref()
        .and_then(|l| l.host.as_ref())
        .and_then(|h| h.os.as_ref())
        .and_then(|o| serde_json::to_value(o).ok())
        .or_else(|| {
            host.as_ref()
                .and_then(|h| h.1.as_deref())
                .and_then(|t| serde_json::from_str(t).ok())
        });
    let boot_time = host
        .as_ref()
        .and_then(|h| h.2)
        .and_then(|b| DateTime::from_timestamp(b, 0))
        .map(crate::storage::fmt_time);
    let data = match inv {
        Some((digest, body, collected, received, changed)) => json!({
            "agent_id": agent_id,
            "supported": supported,
            "source": "inventory",
            "digest": digest,
            "collected_at": ts_ms(collected),
            "received_at": ts_ms(received),
            "changed_at": ts_ms(changed),
            "boot_time": boot_time,
            "inventory": serde_json::from_str::<Value>(&body).unwrap_or(Value::Null),
        }),
        None => json!({
            "agent_id": agent_id,
            "supported": supported,
            "source": if hello_os.is_some() { Value::from("hello") } else { Value::Null },
            "digest": null,
            "collected_at": null,
            "received_at": null,
            "changed_at": null,
            "boot_time": boot_time,
            "inventory": hello_os.map(|os| json!({ "os": os })),
        }),
    };
    Ok(super::http::ok(now, data))
}

/// `GET /api/v1/hosts/{agent_id}/inventory/changes`
pub async fn changes(
    ctx: &Arc<ApiContext>,
    agent_id: &str,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<hyper::Response<super::http::ApiBody>, ApiError> {
    q.reject_unknown(&["limit", "before"], &[])?;
    let agent_id = parse_agent_id(agent_id)?;
    let limit = match q.get("limit") {
        None => DEFAULT_CHANGES_LIMIT,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_CHANGES_LIMIT).contains(n))
            .ok_or_else(|| {
                ApiError::param("limit", format!("limit 必須介於 1～{MAX_CHANGES_LIMIT}"))
            })?,
    };
    let before =
        match q.get("before") {
            None => i64::MAX,
            Some(v) => v.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(|| {
                ApiError::param("before", "before 必須是上一頁回傳的 next（正整數）")
            })?,
        };
    let id = agent_id.clone();
    let rows: Option<Vec<(i64, i64, String)>> = ctx
        .pool
        .read("資產變更歷史", move |conn| {
            let failed = query_failed("資產變更歷史");
            let Some((host_id, _, _)) = host_row(conn, &id).map_err(&failed)? else {
                return Ok(None);
            };
            let mut stmt = conn
                .prepare(
                    "SELECT id, changed_at, changes FROM inventory_changes
                     WHERE host_id = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3",
                )
                .map_err(&failed)?;
            let rows = stmt
                .query_map(params![host_id, before, limit as i64 + 1], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .map_err(&failed)?;
            Ok(Some(
                rows.collect::<rusqlite::Result<_>>().map_err(&failed)?,
            ))
        })
        .await?;
    let mut rows = match rows {
        Some(r) => r,
        None if ctx.registry.live_get(&agent_id).is_some() => Vec::new(),
        None => return Err(ApiError::host_not_found(&agent_id)),
    };
    let next = (rows.len() > limit).then(|| rows[limit - 1].0);
    rows.truncate(limit);
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(id, at, changes)| {
            json!({
                "id": id,
                "changed_at": ts_ms(at),
                "changes": serde_json::from_str::<Value>(&changes).unwrap_or_else(|_| json!([])),
            })
        })
        .collect();
    Ok(super::http::ok(
        now,
        json!({ "agent_id": agent_id, "items": items, "next": next.map(|n| n.to_string()) }),
    ))
}

/// `GET /api/v1/hosts/{agent_id}/processes`
pub async fn processes(
    ctx: &Arc<ApiContext>,
    agent_id: &str,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<hyper::Response<super::http::ApiBody>, ApiError> {
    q.reject_unknown(&["at"], &[])?;
    let agent_id = parse_agent_id(agent_id)?;
    let at_ms = match q.get("at") {
        None => now.timestamp_millis(),
        Some(t) => DateTime::parse_from_rfc3339(t)
            .map(|t| t.timestamp_millis())
            .map_err(|_| {
                ApiError::param(
                    "at",
                    format!("at 的值 \"{t}\" 不是 RFC 3339 時間（例如 2026-10-20T03:00:00Z）"),
                )
            })?,
    };
    let id = agent_id.clone();
    let row: Option<Option<(i64, Vec<u8>)>> = ctx
        .pool
        .read("行程排行", move |conn| {
            let failed = query_failed("行程排行");
            let Some((host_id, _, _)) = host_row(conn, &id).map_err(&failed)? else {
                return Ok(None);
            };
            conn.query_row(
                "SELECT ts, body FROM process_snapshots WHERE host_id = ?1 AND ts <= ?2
                 ORDER BY ts DESC LIMIT 1",
                params![host_id, at_ms],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map(Some)
            .map_err(&failed)
        })
        .await?;
    let live = ctx.registry.live_get(&agent_id);
    let row = match row {
        Some(r) => r,
        None if live.is_some() => None,
        None => return Err(ApiError::host_not_found(&agent_id)),
    };
    let supported = live
        .as_ref()
        .filter(|l| l.seen_since_start)
        .and_then(|l| l.processes)
        .unwrap_or(row.is_some());
    let (sampled_at, list) = match row {
        Some((ts, body)) => (
            Some(ts_ms(ts)),
            serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
        ),
        None => (None, Value::Null),
    };
    let field = |k: &str| list.get(k).cloned().unwrap_or_else(|| json!([]));
    Ok(super::http::ok(
        now,
        json!({
            "agent_id": agent_id,
            "at": ts_ms(at_ms),
            "sampled_at": sampled_at,
            "supported": supported,
            "top_cpu": field("top_cpu"),
            "top_memory": field("top_memory"),
        }),
    ))
}
