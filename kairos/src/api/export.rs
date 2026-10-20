//! 指標轉出的端點（`kairos/EXPORT.md`；商業審計 F7）：
//!
//! | 端點 | 權限 | 說明 |
//! |---|---|---|
//! | `GET /api/v1/export/prometheus` | `hosts:read`（依主機群組篩選） | Prometheus text 0.0.4／OpenMetrics 1.0 |
//! | `GET /api/v1/export/settings` | `settings:read` | 生效的設定、來源與推送狀態（S21「整合」） |
//! | `PUT /api/v1/export/settings` | `export:write` | 修改主控台可修改的鍵（`null` 為改用設定檔的值） |
//! | `POST /api/v1/export/otlp/test` | `export:write` | 以已儲存的設定送出一個空的 OTLP 請求 |

use chrono::{DateTime, Utc};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::auth::{Permission, Principal};
use super::hosts::Status;
use super::http::{self, ApiBody, ApiError, Query};
use super::{ApiContext, account};
use crate::export::settings::{self as xs, CONSOLE_KEYS, KEY_OTLP_HEADERS};
use crate::export::{CODE_EXPORT_CONFIG, CODE_EXPORT_DISABLED, ExportHub, lock, prom};
use crate::store::prelude::*;

/// `PUT /api/v1/export/settings` 的本文上限
const MAX_BODY: usize = 64 * 1024;

fn hub(ctx: &ApiContext) -> Result<Arc<ExportHub>, ApiError> {
    ctx.registry.export_hub().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "export_unavailable",
            "指標轉出沒有啟動",
        )
    })
}

fn method_not_allowed(allowed: &str) -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        format!("這個端點只接受 {allowed}"),
    )
}

fn config_error(param: &str, message: impl Into<String>) -> ApiError {
    ApiError::param(param, message).with_product_code(CODE_EXPORT_CONFIG)
}

/// 處理 `/api/v1/export/…`（已驗證）
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
    match (method.as_str(), rest) {
        ("GET" | "HEAD", ["export", "prometheus"]) => prometheus(ctx, &req, p, &query, now).await,
        (_, ["export", "prometheus"]) => Err(method_not_allowed("GET")),
        ("GET" | "HEAD", ["export", "settings"]) => {
            query.reject_unknown(&[], &[])?;
            p.require(Permission::SettingsRead)?;
            let h = hub(ctx)?;
            Ok(http::ok(now, view(ctx, &h)))
        }
        ("PUT", ["export", "settings"]) => {
            p.require(Permission::ExportWrite)?;
            let body = http::read_json(req, MAX_BODY).await?;
            update(ctx, peer, p, body, now).await
        }
        (_, ["export", "settings"]) => Err(method_not_allowed("GET、PUT")),
        ("POST", ["export", "otlp", "test"]) => {
            p.require(Permission::ExportWrite)?;
            test(ctx, peer, p, now).await
        }
        (_, ["export", "otlp", "test"]) => Err(method_not_allowed("POST")),
        _ => Err(ApiError::not_found()),
    }
}

// ---- Prometheus ----

async fn prometheus(
    ctx: &Arc<ApiContext>,
    req: &Request<Incoming>,
    p: &Principal,
    query: &Query,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    query.reject_unknown(&["group"], &[])?;
    let access = p.host_access()?;
    let hub = hub(ctx)?;
    let eff = hub.effective();
    if !eff.prometheus.enabled {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "export_disabled",
            "Prometheus exporter 未啟用：在主控台「系統設定 → 整合」或 kairos.toml 的 [export.prometheus] 設定 enabled = true",
        )
        .with_product_code(CODE_EXPORT_DISABLED));
    }
    let groups: Vec<String> = query
        .list("group")
        .into_iter()
        .map(|g| g.trim().to_lowercase())
        .filter(|g| !g.is_empty())
        .collect();
    let started = Instant::now();
    let snap = ctx.hosts.snapshot(ctx).await?;
    let latest = hub.latest();
    let rows: Vec<prom::HostRow> = snap
        .hosts
        .iter()
        .filter(|h| access.allows(h.host_group.as_deref()))
        .filter(|h| {
            groups.is_empty()
                || h.host_group
                    .as_deref()
                    .is_some_and(|g| groups.contains(&g.trim().to_lowercase()))
        })
        .map(|h| prom::HostRow {
            agent_id: h.agent_id.clone(),
            hostname: h.hostname.clone(),
            display_name: h.display_name.clone(),
            group: h.host_group.clone(),
            up: h.status == Status::Online,
            agent_version: h.agent_version.clone(),
            os_family: h.inventory.as_ref().and_then(|i| i.os_family.clone()),
            tags: h.tags.clone(),
            sample: latest.get(&h.agent_id).cloned(),
        })
        .collect();
    drop(latest);
    let openmetrics = prom::wants_openmetrics(
        req.headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok()),
    );
    let opts = prom::Options {
        now_ms: now.timestamp_millis(),
        stale_after_ms: i64::try_from(eff.prometheus.stale_after_secs)
            .unwrap_or(i64::MAX / 1000)
            .saturating_mul(1000),
        include_processes: eff.prometheus.include_processes,
        processes_top_n: eff.prometheus.processes_top_n as usize,
        openmetrics,
    };
    // 數千台主機時輸出數 MB：在 blocking 執行緒上產生，不佔用主控台的工作執行緒
    let rendered = tokio::task::spawn_blocking(move || prom::render(&rows, &opts))
        .await
        .map_err(|_| ApiError::internal())?;
    hub.stats.prom_scrapes.fetch_add(1, Ordering::Relaxed);
    *lock(&hub.prom_status) = crate::export::PromStatus {
        last_scrape_ms: Some(now.timestamp_millis()),
        last_duration_ms: started.elapsed().as_millis() as u64,
        last_hosts: rendered.hosts,
        last_series: rendered.series,
        last_bytes: rendered.body.len(),
    };
    let mut resp = Response::new(ApiBody::Full(Some(Bytes::from(rendered.body))));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if openmetrics {
            prom::CONTENT_TYPE_OPENMETRICS
        } else {
            prom::CONTENT_TYPE_TEXT
        }),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(resp)
}

// ---- 設定與狀態 ----

fn opt_ts(ms: Option<i64>) -> Value {
    ms.map_or(Value::Null, |m| json!(http::ts_ms(m)))
}

/// `GET /api/v1/export/settings` 的 `data`
pub fn view(ctx: &ApiContext, hub: &ExportHub) -> Value {
    let eff = hub.effective();
    let s = &hub.stats;
    let st = lock(&hub.otlp_status).clone();
    let ps = lock(&hub.prom_status).clone();
    let queue = hub.queue_points();
    let state = if !eff.otlp.enabled {
        "disabled"
    } else if eff.otlp.endpoint.trim().is_empty() {
        "not_configured"
    } else if st.consecutive_failures > 0 {
        "failing"
    } else if st.last_success_ms.is_some() {
        "ok"
    } else {
        "pending"
    };
    let settings: Vec<Value> = CONSOLE_KEYS
        .iter()
        .map(|k| {
            let src = eff.source(k);
            let env_key = if *k == KEY_OTLP_HEADERS {
                xs::FILE_KEY_HEADERS
            } else {
                k
            };
            let upd = eff.updated.get(k);
            json!({
                "key": k,
                "value": eff.value_of(k),
                "source": crate::settings::short_source(src),
                "env_var": crate::config::Provenance::env_var(env_key),
                "locked": xs::locked(&ctx.config, k),
                "updated_by": upd.map(|u| u.1.clone()),
                "updated_at": upd.map(|u| http::ts_ms(u.0)),
            })
        })
        .collect();
    let o = &eff.otlp;
    json!({
        "prometheus": {
            "enabled": eff.prometheus.enabled,
            "path": "/api/v1/export/prometheus",
            "stale_after_secs": eff.prometheus.stale_after_secs,
            "include_processes": eff.prometheus.include_processes,
            "processes_top_n": eff.prometheus.processes_top_n,
            "status": {
                "scrapes": s.prom_scrapes.load(Ordering::Relaxed),
                "last_scrape_at": opt_ts(ps.last_scrape_ms),
                "last_duration_ms": ps.last_scrape_ms.map(|_| ps.last_duration_ms),
                "last_hosts": ps.last_scrape_ms.map(|_| ps.last_hosts),
                "last_series": ps.last_scrape_ms.map(|_| ps.last_series),
                "last_bytes": ps.last_scrape_ms.map(|_| ps.last_bytes),
            },
        },
        "otlp": {
            "enabled": o.enabled,
            "endpoint": o.endpoint,
            "protocol": "http/protobuf",
            "interval_secs": o.interval_secs,
            "timeout_secs": o.timeout_secs,
            "naming": o.naming,
            "max_batch_points": o.max_batch_points,
            "queue_max_points": o.queue_max_points,
            "drop_policy": o.drop_policy,
            "max_retries": o.max_retries,
            "headers": xs::masked_headers(&eff.headers),
            "headers_source": eff.headers_from.as_str(),
            // 機密的金鑰檔（alerts-secrets.key）可以使用：false 時不能保存或更換標頭（PUT 回 503 secrets_unavailable）
            "secrets_available": hub.secrets.is_some(),
            "ca_file": o.ca_file,
            "client_certificate": !o.client_cert.trim().is_empty(),
            "resource_attributes": o.resource_attributes,
            "status": {
                "state": state,
                "queue_points": queue,
                "sent_points": s.otlp_sent_points.load(Ordering::Relaxed),
                "requests_ok": s.otlp_requests_ok.load(Ordering::Relaxed),
                "requests_failed": s.otlp_requests_failed.load(Ordering::Relaxed),
                "dropped_points": {
                    "queue_full": s.otlp_dropped_queue_full.load(Ordering::Relaxed),
                    "failed": s.otlp_dropped_failed.load(Ordering::Relaxed),
                    "rejected": s.otlp_dropped_rejected.load(Ordering::Relaxed),
                },
                "consecutive_failures": st.consecutive_failures,
                "last_attempt_at": opt_ts(st.last_attempt_ms),
                "last_success_at": opt_ts(st.last_success_ms),
                "last_request_ms": st.last_success_ms.map(|_| st.last_request_ms),
                "last_request_bytes": st.last_success_ms.map(|_| st.last_request_bytes),
                "last_error": st.last_error.as_ref().map(|(code, msg, at)| json!({"code": code, "message": msg, "at": http::ts_ms(*at)})),
            },
        },
        "settings": settings,
        "problems": eff.problems,
    })
}

/// 本文中的一個鍵：`Some(Some(v))` 設定、`Some(None)` 為 `null`（改用設定檔的值）、`None` 不變
fn take<'a>(obj: Option<&'a Value>, name: &str) -> Option<Option<&'a Value>> {
    let v = obj?.get(name)?;
    Some((!v.is_null()).then_some(v))
}

/// `PUT /api/v1/export/settings`
async fn update(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    p: &Principal,
    body: Value,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let hub = hub(ctx)?;
    let Value::Object(map) = &body else {
        return Err(config_error("body", "本文必須是 JSON 物件"));
    };
    for k in map.keys() {
        if !matches!(k.as_str(), "prometheus" | "otlp" | "reason") {
            return Err(config_error(k, format!("未知的欄位 {k}")));
        }
    }
    let reason = http::str_field(&body, "reason")
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    if reason.as_deref().is_some_and(|r| r.chars().count() > 500) {
        return Err(config_error("reason", "原因最多 500 個字元"));
    }
    let prom_obj = body.get("prometheus");
    let otlp_obj = body.get("otlp");
    for (obj, allowed, section) in [
        (
            prom_obj,
            &["enabled", "stale_after_secs", "include_processes"][..],
            "prometheus",
        ),
        (
            otlp_obj,
            &["enabled", "endpoint", "interval_secs", "naming", "headers"][..],
            "otlp",
        ),
    ] {
        if let Some(o) = obj {
            let Value::Object(m) = o else {
                return Err(config_error(section, format!("{section} 必須是物件")));
            };
            for k in m.keys() {
                if !allowed.contains(&k.as_str()) {
                    return Err(config_error(
                        &format!("{section}.{k}"),
                        format!(
                            "{section}.{k} 不能從主控台修改（在 kairos.toml 的 [export.{section}] 設定）"
                        ),
                    ));
                }
            }
        }
    }
    // (鍵, 新值；None 為刪除資料庫的值)
    let mut changes: Vec<(&'static str, Option<Value>)> = Vec::new();
    let pairs: [(&'static str, Option<&Value>, &str); 7] = [
        (xs::KEY_PROM_ENABLED, prom_obj, "enabled"),
        (xs::KEY_PROM_STALE, prom_obj, "stale_after_secs"),
        (xs::KEY_PROM_PROCESSES, prom_obj, "include_processes"),
        (xs::KEY_OTLP_ENABLED, otlp_obj, "enabled"),
        (xs::KEY_OTLP_ENDPOINT, otlp_obj, "endpoint"),
        (xs::KEY_OTLP_INTERVAL, otlp_obj, "interval_secs"),
        (xs::KEY_OTLP_NAMING, otlp_obj, "naming"),
    ];
    for (key, obj, field) in pairs {
        if let Some(v) = take(obj, field) {
            if let Some(v) = v {
                let v = match v.as_str() {
                    Some(s) if key == xs::KEY_OTLP_ENDPOINT => json!(s.trim()),
                    _ => v.clone(),
                };
                xs::validate_value(key, &v).map_err(|e| config_error(key, e))?;
                changes.push((key, Some(v)));
            } else {
                changes.push((key, None));
            }
        }
    }
    let mut header_names: Option<Option<Vec<String>>> = None;
    if let Some(h) = take(otlp_obj, "headers") {
        match h {
            Some(v) => {
                let list =
                    xs::validate_headers(v).map_err(|e| config_error(KEY_OTLP_HEADERS, e))?;
                let secrets = hub.secrets.clone().ok_or_else(|| {
                    ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "secrets_unavailable",
                        format!(
                            "無法開啟資料目錄的 {}，不能保存標頭；改用 export.otlp.headers_file",
                            crate::alerts::secrets::KEY_FILE
                        ),
                    )
                    .with_product_code(CODE_EXPORT_CONFIG)
                })?;
                let sealed = xs::seal_headers(&secrets, &list).map_err(|e| {
                    log::warn!("無法加密 OTLP 標頭：{e}");
                    ApiError::internal()
                })?;
                header_names = Some(Some(list.iter().map(|(k, _)| k.clone()).collect()));
                changes.push((KEY_OTLP_HEADERS, Some(sealed)));
            }
            None => {
                header_names = Some(None);
                changes.push((KEY_OTLP_HEADERS, None));
            }
        }
    }
    if changes.is_empty() {
        return Err(config_error("body", "沒有要修改的設定"));
    }
    for (k, _) in &changes {
        if xs::locked(&ctx.config, k) {
            let var = crate::config::Provenance::env_var(if *k == KEY_OTLP_HEADERS {
                xs::FILE_KEY_HEADERS
            } else {
                k
            });
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "locked_by_environment",
                format!("{k} 由環境變數設定，無法從主控台修改"),
            )
            .with_details(json!({"variable": var, "key": k})));
        }
    }
    // 套用後的生效值必須合理：啟用 OTLP 時要有端點
    let before = hub.effective();
    let rows = current_rows(ctx).await?;
    let mut next: Vec<xs::Row> = rows
        .iter()
        .filter(|(k, ..)| !changes.iter().any(|(c, _)| c == k))
        .cloned()
        .collect();
    let actor = p.actor();
    let now_ms = now.timestamp_millis();
    for (k, v) in &changes {
        if let Some(v) = v {
            next.push((k.to_string(), v.clone(), now_ms, actor.clone()));
        }
    }
    let after = xs::effective(&ctx.config, &next, hub.secrets.as_deref());
    if after.otlp.enabled && after.otlp.endpoint.trim().is_empty() {
        return Err(config_error(
            xs::KEY_OTLP_ENDPOINT,
            "啟用 OTLP 時必須設定端點（例如 https://otel-collector:4318/v1/metrics）",
        ));
    }
    // 稽核的差異（標頭只記名稱）
    let mut diff = serde_json::Map::new();
    for (k, _) in &changes {
        let (b, a) = if *k == KEY_OTLP_HEADERS {
            (
                json!(before.headers.iter().map(|(n, _)| n).collect::<Vec<_>>()),
                json!(after.headers.iter().map(|(n, _)| n).collect::<Vec<_>>()),
            )
        } else if *k == xs::KEY_OTLP_ENDPOINT {
            // 網址的查詢字串可能含 API key：稽核只記到路徑
            (
                endpoint_for_audit(&before.otlp.endpoint),
                endpoint_for_audit(&after.otlp.endpoint),
            )
        } else {
            (before.value_of(k), after.value_of(k))
        };
        diff.insert(k.to_string(), json!({"before": b, "after": a, "source": crate::settings::short_source(after.source(k))}));
    }
    let writes = changes.clone();
    let event = crate::audit::Event {
        actor: actor.clone(),
        actor_ip: Some(peer.ip().to_canonical().to_string()),
        category: "settings",
        action: "export.change",
        object: Some("export".to_string()),
        result: "ok",
        code: None,
        details: Some(json!({"changes": diff, "reason": reason, "headers": header_names})),
    };
    ctx.admin
        .write(move |c| {
            let tx = c.begin(TxMode::Deferred)?;
            for (k, v) in &writes {
                match v {
                    Some(v) => tx.set_setting(k, v, &actor, now_ms)?,
                    None => {
                        tx.delete_setting(k)?;
                    }
                }
            }
            event.append(&*tx, now_ms)?;
            tx.commit()
        })
        .await
        .map_err(|e| {
            log::warn!("主控台：無法保存指標轉出的設定：{e}");
            ApiError::storage("設定")
        })?;
    let problems = after.problems.clone();
    hub.apply(after);
    log::info!(
        event = "export.settings_changed";
        "指標轉出的設定已由 {} 修改：{}",
        p.label(),
        changes.iter().map(|(k, _)| *k).collect::<Vec<_>>().join("、")
    );
    for pr in problems {
        log::warn!(event = "export.config", code = CODE_EXPORT_CONFIG; "警告 {CODE_EXPORT_CONFIG}：{pr}");
    }
    Ok(http::ok(now, view(ctx, &hub)))
}

/// 資料庫中目前的指標轉出設定
async fn current_rows(ctx: &Arc<ApiContext>) -> Result<Vec<xs::Row>, ApiError> {
    ctx.pool
        .read("設定", |conn| {
            conn.load_settings()
                .map(|rows| {
                    rows.into_iter()
                        .filter(|(k, ..)| CONSOLE_KEYS.contains(&k.as_str()))
                        .collect()
                })
                .map_err(super::db::query_failed("設定"))
        })
        .await
}

/// `POST /api/v1/export/otlp/test`
async fn test(
    ctx: &Arc<ApiContext>,
    peer: SocketAddr,
    p: &Principal,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let hub = hub(ctx)?;
    let eff = hub.effective();
    if eff.otlp.endpoint.trim().is_empty() {
        return Err(config_error(xs::KEY_OTLP_ENDPOINT, "還沒有設定 OTLP 端點"));
    }
    let out = crate::export::otlp::test_connection(&eff).await;
    account::audit(
        ctx,
        crate::audit::Event {
            actor: p.actor(),
            actor_ip: Some(peer.ip().to_canonical().to_string()),
            category: "settings",
            action: "export.otlp_test",
            object: Some(crate::alerts::secrets::mask_url(&eff.otlp.endpoint)),
            result: if out.ok { "ok" } else { "failed" },
            code: out.code.filter(|_| !out.ok),
            details: Some(json!({"status": out.status, "elapsed_ms": out.elapsed_ms})),
        },
        now.timestamp_millis(),
    )
    .await;
    Ok(http::ok(
        now,
        json!({
            "ok": out.ok,
            "status": out.status,
            "code": out.code,
            "message": out.message,
            "elapsed_ms": out.elapsed_ms,
            "endpoint": eff.otlp.endpoint,
        }),
    ))
}

/// 稽核中的 OTLP 端點：查詢字串（可能含 API key）以遮蔽值取代
fn endpoint_for_audit(url: &str) -> Value {
    match url.split_once('?') {
        Some((base, _)) => json!(format!("{base}?{}", crate::alerts::secrets::MASK)),
        None => json!(url),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_masks_the_endpoint_query_string() {
        assert_eq!(
            endpoint_for_audit("https://otlp.example.com/v1/metrics?api-key=s3cret"),
            json!("https://otlp.example.com/v1/metrics?••••")
        );
        assert_eq!(
            endpoint_for_audit("http://127.0.0.1:4318/v1/metrics"),
            json!("http://127.0.0.1:4318/v1/metrics")
        );
    }
}
