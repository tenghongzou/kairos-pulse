//! 個資：刪除主機資料與個資匯出（商業審計 S6；`kairos/API.md` 第 15 節、`kairos/PRIVACY.md`）。
//!
//! | 端點 | 權限 | 說明 |
//! |---|---|---|
//! | `GET /api/v1/hosts/{agent_id}/purge` | `hosts:purge` | 影響預覽：各資料表會刪除的筆數、會撤銷的憑證、`impact_token`、L2 的確認字串 |
//! | `POST /api/v1/hosts/{agent_id}/purge` | `hosts:purge` | 刪除（L2：`confirm` 為主機名稱、`impact_token`、選填的 `reason`） |
//! | `GET /api/v1/hosts/{agent_id}/personal-data` | `hosts:personal_data` | 個資匯出（資料主體存取請求），`kairos.host.personal-data/v1` |
//!
//! 刪除的順序：主機必須離線 → 從狀態表移除 → 撤銷尚未撤銷的有效憑證（`cessationOfOperation`）→ 告警引擎
//! 忘記這台主機的進行中事件 → report 的 writer 逐步刪除（[`crate::store::purge`]，最後一步寫稽核
//! `host.purge`）→ 清除主控台的快取。稽核日誌本身不刪除（雜湊鏈）。

use chrono::{DateTime, Utc};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use log::{info, warn};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use super::ApiContext;
use super::auth::{Permission, Principal};
use super::http::{self, ApiBody, ApiError, str_field};
use crate::store::prelude::*;
use crate::store::purge::{
    self, CODE_PURGE_FAILED, CODE_PURGED, PurgeCounts, PurgeJob, verify_impact_token,
};

/// 刪除的等待上限（writer 每一步刪除 5000 列、與 report 交錯；1 萬筆 report 約數秒）
const PURGE_TIMEOUT: Duration = Duration::from_secs(120);

/// 本文上限
const MAX_BODY: usize = 64 * 1024;

/// 是不是這個模組的路徑（`["hosts", id, "purge" | "personal-data"]`）
pub fn is_route(rest: &[&str]) -> bool {
    matches!(rest, ["hosts", _, "purge" | "personal-data"])
}

/// 處理 `/api/v1/hosts/{agent_id}/purge`、`/personal-data`（已驗證）
pub async fn handle(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    req: Request<Incoming>,
    principal: &Principal,
    rest: &[&str],
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let method = req.method().clone();
    let (id, what) = match rest {
        ["hosts", id, what] => (*id, *what),
        _ => return Err(ApiError::not_found()),
    };
    match (method.as_str(), what) {
        ("GET" | "HEAD", "purge") => {
            principal.require(Permission::HostsPurge)?;
            let agent_id = super::hosts::parse_agent_id(id)?;
            super::hosts::check_access(ctx, &agent_id, &principal.host_access()?).await?;
            let p = preview(ctx, &agent_id, now).await?;
            Ok(http::ok(now, p.json(now)))
        }
        ("POST", "purge") => {
            principal.require(Permission::HostsPurge)?;
            let agent_id = super::hosts::parse_agent_id(id)?;
            super::hosts::check_access(ctx, &agent_id, &principal.host_access()?).await?;
            let body = http::read_json(req, MAX_BODY).await?;
            purge_host(ctx, peer, principal, &agent_id, &body, now).await
        }
        ("GET" | "HEAD", "personal-data") => {
            principal.require(Permission::HostsPersonalData)?;
            let agent_id = super::hosts::parse_agent_id(id)?;
            super::hosts::check_access(ctx, &agent_id, &principal.host_access()?).await?;
            export(ctx, peer, principal, &agent_id, now).await
        }
        (_, "purge") => Err(method_not_allowed("GET、POST")),
        _ => Err(method_not_allowed("GET")),
    }
}

fn method_not_allowed(allow: &str) -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        format!("這個端點只接受 {allow}"),
    )
}

/// 影響預覽
struct Preview {
    agent_id: String,
    hostname: Option<String>,
    display_name: Option<String>,
    online: bool,
    counts: PurgeCounts,
}

impl Preview {
    /// L2 的確認字串：主機名稱（清冊中沒有時為 agent_id）
    fn confirm_text(&self) -> String {
        self.hostname
            .clone()
            .unwrap_or_else(|| self.agent_id.clone())
    }

    fn json(&self, now: DateTime<Utc>) -> Value {
        let now_ms = now.timestamp_millis();
        json!({
            "agent_id": self.agent_id,
            "hostname": self.hostname,
            "display_name": self.display_name,
            "online": self.online,
            "blocked": self.online.then(|| json!({
                "code": "host_online",
                "product_code": CODE_PURGE_FAILED,
                "message": "這台主機目前仍連線中。請先在主機上停止並移除 Pulse（或撤銷憑證讓它斷線），離線後再刪除。",
            })),
            "level": "L2",
            "confirm_text": self.confirm_text(),
            "impact_token": purge::impact_token(&self.agent_id, &self.counts, now_ms),
            "counts": self.counts.json(),
            "revoke": {
                "certificates": self.counts.certificates_active,
                "reason": "cessationOfOperation",
            },
            "kept": {
                "audit_log": true,
                "certificates_for_crl": self.counts.certificates_kept,
                "logs": true,
                "backups": true,
            },
        })
    }
}

async fn preview(
    ctx: &Arc<ApiContext>,
    agent_id: &str,
    now: DateTime<Utc>,
) -> Result<Preview, ApiError> {
    let id = agent_id.to_string();
    let now_ms = now.timestamp_millis();
    let (counts, names) = ctx
        .pool
        .read("刪除主機資料的影響", move |conn| {
            let counts = purge::plan(conn, &id, now_ms)
                .map_err(super::db::query_failed("刪除主機資料的影響"))?;
            let names: Option<(String, Option<String>)> =
                rusqlite::OptionalExtension::optional(conn.query_row(
                    "SELECT hostname, display_name FROM hosts WHERE agent_id = ?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                ))
                .map_err(super::db::query_failed("刪除主機資料的影響"))?;
            Ok((counts, names))
        })
        .await?;
    if counts.total() == 0 {
        return Err(ApiError::host_not_found(agent_id));
    }
    // 狀態表中有、清冊還沒同步的主機（第一筆 report 之前）也算
    let live = ctx.registry.live_get(agent_id);
    let (hostname, display_name) = match names {
        Some((h, d)) => (Some(h), d),
        None => (
            live.as_ref().map(|l| l.hostname.to_string()),
            live.as_ref()
                .and_then(|l| l.host.as_ref().and_then(|h| h.display_name.clone())),
        ),
    };
    Ok(Preview {
        agent_id: agent_id.to_string(),
        hostname,
        display_name,
        online: ctx.registry.is_connected(agent_id),
        counts,
    })
}

fn host_online() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "host_online",
        "這台主機目前仍連線中，不能刪除它的資料。請先在主機上停止並移除 Pulse（或撤銷憑證讓它斷線），離線後再刪除。",
    )
    .with_product_code(CODE_PURGE_FAILED)
}

async fn purge_host(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    principal: &Principal,
    agent_id: &str,
    body: &Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let Some(purger) = ctx.purger.clone() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "刪除主機資料需要 SQLite（storage.backend = \"sqlite\"）",
        )
        .with_product_code(CODE_PURGE_FAILED));
    };
    let p = preview(ctx, agent_id, now).await?;
    if p.online {
        return Err(host_online());
    }
    // L2：輸入主機名稱
    let confirm = str_field(body, "confirm").map(str::trim).unwrap_or("");
    if confirm != p.confirm_text() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "confirmation_required",
            "刪除主機資料需要在 confirm 輸入主機名稱（L2 確認）",
        )
        .with_details(json!({"confirm_text": p.confirm_text()})));
    }
    let now_ms = now.timestamp_millis();
    let token = str_field(body, "impact_token").unwrap_or("");
    if !verify_impact_token(agent_id, &p.counts, token, now_ms) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "impact_changed",
            "影響的內容已改變（或確認已逾時），請重新確認",
        )
        .with_details(p.json(now)));
    }
    let reason = str_field(body, "reason")
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(|r| r.chars().take(500).collect::<String>());
    // 1. 從狀態表移除（之後的快照、清冊同步、主控台都看不到它）；連線仍在時拒絕
    if ctx.registry.forget(agent_id).is_err() {
        return Err(host_online());
    }
    // 指標轉出（kairos/EXPORT.md）：Prometheus exporter 的最新值與 OTLP 佇列中尚未送出的資料一併丟棄
    if let Some(hub) = ctx.registry.export_hub() {
        hub.forget(agent_id);
    }
    let actor = principal.actor();
    let ip = peer.ip().to_canonical().to_string();
    // 2. 撤銷尚未撤銷的有效憑證
    let (a, act, ipc) = (agent_id.to_string(), actor.clone(), ip.clone());
    let revoked = ctx
        .admin
        .write(move |conn| {
            Ok(purge::revoke_active_certificates(
                conn,
                &a,
                &act,
                Some(&ipc),
                now_ms,
            )?)
        })
        .await
        .map_err(|e| {
            warn!("刪除主機資料：無法撤銷 {agent_id} 的憑證：{e}");
            ApiError::storage("憑證").with_product_code(CODE_PURGE_FAILED)
        })?;
    if let Some(svc) = &ctx.enroll {
        svc.set_decommissioned(agent_id, false);
        if !revoked.is_empty() {
            svc.wake();
        }
    }
    // 3. 告警引擎忘記這台主機的進行中事件
    if let Some(alerts) = &ctx.alerts {
        alerts.forget_host(agent_id).await;
    }
    // 4. writer 逐步刪除
    let mut job = PurgeJob::new(agent_id, &actor, Some(&ip), reason.as_deref());
    job.revoked = revoked.clone();
    let counts = match purger.purge(job, PURGE_TIMEOUT).await {
        Ok(c) => c,
        Err(e) => {
            warn!(
                event = "host.purge_failed", code = CODE_PURGE_FAILED;
                "錯誤 {CODE_PURGE_FAILED}：刪除主機 {agent_id} 的資料沒有完成：{e}"
            );
            let (a, act, ipc, msg) = (agent_id.to_string(), actor.clone(), ip.clone(), e.clone());
            let _ = ctx
                .admin
                .write(move |conn| {
                    conn.audit_append(
                        now_ms,
                        &crate::audit::Audit {
                            actor: &act,
                            actor_ip: Some(&ipc),
                            category: "host",
                            action: "host.purge",
                            object: Some(&a),
                            result: "failed",
                            code: Some(CODE_PURGE_FAILED),
                            details: Some(json!({"error": msg})),
                        },
                    )
                })
                .await;
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                format!("刪除沒有完成：{e}。已刪除的部分不會復原，請稍後重新執行（會接續完成）"),
            )
            .with_product_code(CODE_PURGE_FAILED));
        }
    };
    // 5. 主控台的快取
    ctx.conn_log.forget(agent_id);
    ctx.hosts.invalidate_db().await;
    info!(
        event = "host.purged", code = CODE_PURGED;
        "資訊 {CODE_PURGED}：已刪除主機 {agent_id} 的資料（{} 列；撤銷憑證 {} 張；執行者 {actor}）。稽核日誌不刪除",
        counts.total(),
        revoked.len()
    );
    Ok(http::ok(
        now,
        json!({
            "agent_id": agent_id,
            "deleted": counts.json(),
            "revoked_serials": revoked,
            "kept": {
                "audit_log": true,
                "certificates_for_crl": counts.certificates_kept,
                "logs": true,
                "backups": true,
            },
        }),
    ))
}

async fn export(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    principal: &Principal,
    agent_id: &str,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let id = agent_id.to_string();
    let settings = ctx.config.privacy;
    let now_ms = now.timestamp_millis();
    let doc = ctx
        .pool
        .read("個資匯出", move |conn| {
            crate::privacy::personal_data(conn, &id, &settings, now_ms)
                .map_err(super::db::query_failed("個資匯出"))
        })
        .await?
        .ok_or_else(|| ApiError::host_not_found(agent_id))?;
    let (a, act, ipc) = (
        agent_id.to_string(),
        principal.actor(),
        peer.ip().to_canonical().to_string(),
    );
    if let Err(e) = ctx
        .admin
        .write(move |conn| {
            conn.audit_append(
                now_ms,
                &crate::audit::Audit {
                    actor: &act,
                    actor_ip: Some(&ipc),
                    category: "host",
                    action: "host.personal_data.export",
                    object: Some(&a),
                    result: "ok",
                    ..crate::audit::Audit::default()
                },
            )
        })
        .await
    {
        // 匯出照常（稽核寫不進去時記錄）
        warn!("個資匯出的稽核紀錄無法寫入：{e}");
    }
    Ok(http::ok(now, doc))
}
