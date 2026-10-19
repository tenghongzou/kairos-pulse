//! 主控台的背景工作（每秒醒來一次，各項依自己的週期執行，直到關機）：
//!
//! - 每 60 秒：記錄上線數（總覽的 24 小時 sparkline 與「比 1 小時前」，設計包 §24 #5）、
//!   寫回 token 的最後使用時間、清理驗證失敗的計數；
//! - 每 10 秒：把新建立或撤銷的 token 寫入 Kairos 的日誌（稽核，`kairos/API.md` 第 4.2 節）、
//!   把握手時被拒的憑證寫入主機清冊（設計包 §24 #3）。
//!
//! 寫入只用 [`super::db::WriteConn`] 的短交易；失敗時下次再試。

use chrono::Utc;
use log::{info, warn};
use rusqlite::params;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{MissedTickBehavior, interval};

use super::ApiContext;
use super::hosts::Status;
use super::http::ts_ms;
use super::overview;
use super::tokens::{self, LOGGED_CREATED, LOGGED_REVOKED};
use crate::store::prelude::*;

const TICK: Duration = Duration::from_secs(1);
const MINUTE_EVERY: u64 = 60;
const AUDIT_EVERY: u64 = 10;
/// 每小時清除結束或逾時超過 30 天的工作階段紀錄
const SESSION_PURGE_EVERY: u64 = 3600;

pub async fn run(ctx: Arc<ApiContext>) {
    let mut ticker = interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut tick: u64 = 0;
    loop {
        tokio::select! {
            biased;
            _ = ctx.shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        if tick.is_multiple_of(MINUTE_EVERY) {
            sample_minute(&ctx).await;
            flush_last_used(&ctx).await;
            ctx.auth.failures.prune(Instant::now());
            let now_ms = Utc::now().timestamp_millis();
            ctx.login.prune(now_ms);
            ctx.login.phantom.prune(now_ms);
        }
        if tick.is_multiple_of(SESSION_PURGE_EVERY) {
            purge_sessions(&ctx).await;
        }
        if tick.is_multiple_of(AUDIT_EVERY) {
            audit_tokens(&ctx).await;
            persist_rejections(&ctx).await;
        }
        tick = tick.wrapping_add(1);
    }
    // 關機前把最後使用時間寫回
    flush_last_used(&ctx).await;
}

async fn purge_sessions(ctx: &Arc<ApiContext>) {
    let now_ms = Utc::now().timestamp_millis();
    if let Err(e) = ctx
        .admin
        .write(move |c| Ok(c.purge_sessions(now_ms)?))
        .await
    {
        warn!("無法清除過期的工作階段紀錄（稍後重試）: {e}");
    }
}

/// 每分鐘的上線數
async fn sample_minute(ctx: &Arc<ApiContext>) {
    let Ok(snap) = ctx.hosts.snapshot(ctx).await else {
        return;
    };
    let counts = overview::counts(&snap);
    let online = counts.get(&Status::Online).copied().unwrap_or(0);
    ctx.minutes.lock().unwrap_or_else(|e| e.into_inner()).push(
        Utc::now().timestamp_millis(),
        online,
        snap.hosts.len() as u64,
    );
}

async fn flush_last_used(ctx: &Arc<ApiContext>) {
    let items = ctx.auth.last_used.take();
    if items.is_empty() {
        return;
    }
    let to_write = items.clone();
    if let Err(e) = ctx
        .writer
        .write(move |conn| Ok(tokens::store_last_used(conn, &to_write)?))
        .await
    {
        warn!("無法寫回 API token 的最後使用時間（稍後重試）: {e}");
        ctx.auth.last_used.restore(items);
    }
}

/// 新建立、新撤銷的 token 寫入日誌（之後標記，不重複記錄）
pub(super) async fn audit_tokens(ctx: &Arc<ApiContext>) {
    let result = ctx
        .writer
        .write(|conn| {
            let pending = tokens::pending_audit(conn)?;
            for (row, logged) in &pending {
                let mut flags = 0;
                if logged & LOGGED_CREATED == 0 {
                    flags |= LOGGED_CREATED;
                }
                if row.revoked_at.is_some() && logged & LOGGED_REVOKED == 0 {
                    flags |= LOGGED_REVOKED;
                }
                tokens::mark_logged(conn, &row.token_id, flags)?;
            }
            Ok(pending)
        })
        .await;
    let pending = match result {
        Ok(p) => p,
        Err(e) => {
            warn!("無法讀取 API token 的稽核紀錄（稍後重試）: {e}");
            return;
        }
    };
    for (row, logged) in pending {
        if logged & LOGGED_CREATED == 0 {
            info!(
                "API token 已建立：token_id {}、名稱「{}」、範圍 {}、建立時間 {}、到期 {}、建立者 {}",
                row.token_id,
                row.name.escape_debug(),
                row.scope,
                ts_ms(row.created_at),
                row.expires_at.map_or_else(|| "不過期".to_string(), ts_ms),
                row.created_by
                    .as_deref()
                    .unwrap_or("（未知）")
                    .escape_debug()
            );
        }
        if let Some(at) = row.revoked_at
            && logged & LOGGED_REVOKED == 0
        {
            info!(
                "API token 已撤銷：token_id {}、名稱「{}」、撤銷時間 {}、撤銷者 {}、原因 {}",
                row.token_id,
                row.name.escape_debug(),
                ts_ms(at),
                row.revoked_by
                    .as_deref()
                    .unwrap_or("（未知）")
                    .escape_debug(),
                row.revoke_reason
                    .as_deref()
                    .unwrap_or("（未填）")
                    .escape_debug()
            );
        }
    }
}

/// 握手時被拒的憑證寫入主機清冊（`hosts.cert_rejected_at`、`cert_rejection`）。清冊中沒有的主機
/// 留到下次再試（例如第一筆 report 之前；紀錄只來自信任的 CA 簽發的憑證，數量有限）
pub(super) async fn persist_rejections(ctx: &Arc<ApiContext>) {
    let dirty = crate::tls::rejections().take_dirty();
    if dirty.is_empty() {
        return;
    }
    let deltas: Vec<(String, u64)> = dirty.iter().map(|(id, _, d)| (id.clone(), *d)).collect();
    let privacy = ctx.privacy.clone();
    let result = ctx
        .writer
        .write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut missing = Vec::new();
            let mut updated = 0;
            {
                let mut stmt = tx.prepare(
                    "UPDATE hosts SET cert_rejected_at = ?2,
                        cert_rejection = json_object('reason', ?3, 'serial', ?4, 'peer', ?5,
                            'count', coalesce(json_extract(cert_rejection, '$.count'), 0) + ?6)
                     WHERE agent_id = ?1 AND (cert_rejected_at IS NULL OR cert_rejected_at <= ?2)",
                )?;
                for (id, r, delta) in &dirty {
                    let n = stmt.execute(params![
                        id,
                        r.at.timestamp_millis(),
                        r.reason.as_str(),
                        r.serial,
                        r.peer.and_then(|p| privacy.ip(p)),
                        i64::try_from(*delta).unwrap_or(i64::MAX),
                    ])?;
                    if n == 0 {
                        missing.push((id.clone(), *delta));
                    }
                    updated += n;
                }
            }
            tx.commit()?;
            Ok((updated, missing))
        })
        .await;
    match result {
        Ok((n, missing)) => {
            crate::tls::rejections().unsave(&missing);
            if n > 0 {
                ctx.hosts.invalidate_db().await;
            }
        }
        Err(e) => {
            warn!("無法把被拒的憑證寫入主機清冊（稍後重試）: {e}");
            crate::tls::rejections().unsave(&deltas);
        }
    }
}
