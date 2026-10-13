//! 系統健康（`kairos/API.md` 第 5.5 節；設計包 S20、§25.5 #3、#4）。
//!
//! 每個檢查項目一列：狀態（`ok`／`warning`／`critical`）、對應的產品代碼、摘要與數值；整體取最嚴重者。

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

use super::ApiContext;
use super::db::query_failed;
use super::http::{ApiError, ts, ts_ms};
use crate::expiry::ExpiryKind;
use crate::registry::LiveAgent;
use crate::store::sqlite::{CapacityEvent, META_CAPACITY_EVENT, META_RETENTION_LAST, RetentionRun};

/// 檢查項目的狀態（依嚴重度排序）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Warning,
    Critical,
}

/// 一個檢查項目
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: &'static str,
    pub status: Level,
    pub product_code: Option<&'static str>,
    pub summary: String,
    pub values: Value,
}

impl Item {
    fn new(id: &'static str, status: Level, summary: impl Into<String>, values: Value) -> Self {
        Self {
            id,
            status,
            product_code: None,
            summary: summary.into(),
            values,
        }
    }

    fn code(mut self, code: &'static str) -> Self {
        self.product_code = Some(code);
        self
    }
}

/// 系統健康的結果
#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub status: Level,
    pub items: Vec<Item>,
    pub service: Value,
}

/// 資料庫的數值（唯讀連線取得）
#[derive(Debug, Default)]
struct DbStats {
    used_bytes: u64,
    page_size: u64,
    capacity: Option<CapacityEvent>,
    retention: Option<RetentionRun>,
    guard: Option<date_guard::GuardState>,
}

fn read_db_stats(conn: &rusqlite::Connection) -> rusqlite::Result<DbStats> {
    let pragma = |name: &str| conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0));
    let page_size = pragma("page_size")?;
    let pages = pragma("page_count")? - pragma("freelist_count")?;
    let meta = |key: &str| -> rusqlite::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    };
    Ok(DbStats {
        used_bytes: u64::try_from(pages.max(0) * page_size).unwrap_or(0),
        page_size: u64::try_from(page_size).unwrap_or(0),
        capacity: meta(META_CAPACITY_EVENT)?.and_then(|s| serde_json::from_str(&s).ok()),
        retention: meta(META_RETENTION_LAST)?.and_then(|s| serde_json::from_str(&s).ok()),
        guard: meta("retention_guard")?.and_then(|s| serde_json::from_str(&s).ok()),
    })
}

/// 以 GB（10^9）表示，一位小數
pub fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

fn ratio(a: u64, b: u64) -> Option<f64> {
    (b > 0).then(|| a as f64 / b as f64)
}

fn percent(r: Option<f64>) -> String {
    r.map_or_else(|| "—".to_string(), |r| format!("{:.0}%", r * 100.0))
}

/// 資料庫項目（S20「資料庫」）
fn database(ctx: &ApiContext, db: &DbStats, live: &[LiveAgent], now_ms: i64) -> Item {
    let max = ctx.config.storage.max_total_bytes;
    let file = std::fs::metadata(&ctx.db_path).map_or(0, |m| m.len());
    let mut wal_path = ctx.db_path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let wal = std::fs::metadata(&wal_path).map_or(0, |m| m.len());
    // 目前回報中的主機與各自宣告的 report 間隔（設計包 §24 #19、§25.5 #4）
    let reporting: Vec<u64> = live
        .iter()
        .filter(|l| l.online)
        .map(|l| {
            l.host
                .as_ref()
                .and_then(|h| h.report_interval_ms)
                .unwrap_or(crate::config::ESTIMATE_REPORT_INTERVAL_SECS * 1000)
        })
        .collect();
    let estimate: u64 = reporting
        .iter()
        .map(|ms| ctx.config.estimated_sqlite_bytes_per_host(*ms))
        .sum();
    let recent = |e: &CapacityEvent| e.ended_at.is_none_or(|end| now_ms - end < 24 * 3_600_000);
    let capacity_recent = db.capacity.as_ref().is_some_and(recent);
    let deleting = db.capacity.as_ref().is_some_and(|e| e.ended_at.is_none());
    let over = max > 0 && estimate > max;
    let status = if capacity_recent || over {
        Level::Warning
    } else {
        Level::Ok
    };
    let limit = if max == 0 {
        "不限制".to_string()
    } else {
        format!("上限 {}", gb(max))
    };
    let summary = if deleting {
        "資料庫已達容量上限，正在提早刪除最舊的資料".to_string()
    } else if capacity_recent {
        "最近 24 小時內曾因容量上限提早刪除最舊的資料".to_string()
    } else if over {
        format!(
            "{} / {}，保留期滿約 {}，超過上限",
            gb(db.used_bytes),
            limit,
            gb(estimate)
        )
    } else {
        format!(
            "{} / {}（{}）· 保留期滿約 {}",
            gb(db.used_bytes),
            limit,
            percent(ratio(db.used_bytes, max)),
            gb(estimate)
        )
    };
    let item = Item::new(
        "database",
        status,
        summary,
        json!({
            "used_bytes": db.used_bytes,
            "file_bytes": file,
            "wal_bytes": wal,
            "page_size": db.page_size,
            "max_total_bytes": max,
            "usage_ratio": ratio(db.used_bytes, max),
            "estimate": { "hosts": reporting.len(), "bytes": estimate, "exceeds_limit": over },
            "capacity": {
                "active": deleting,
                "last": db.capacity.as_ref().map(|e| json!({
                    "started_at": ts_ms(e.started_at),
                    "ended_at": e.ended_at.map(ts_ms),
                    "used_bytes": e.used_bytes,
                    "limit_bytes": e.limit_bytes,
                    "deleted": {
                        "raw": e.deleted_raw,
                        "rollup_5m": e.deleted_rollup_5m,
                        "rollup_1h": e.deleted_rollup_1h,
                    },
                    "exhausted": e.exhausted,
                })),
            },
        }),
    );
    if capacity_recent {
        item.code(crate::store::sqlite::CODE_CAPACITY)
    } else {
        item
    }
}

fn retention(ctx: &ApiContext, db: &DbStats) -> Item {
    let s = &ctx.config.storage;
    let paused = db
        .guard
        .as_ref()
        .is_some_and(|g| g.jump_since_unix.is_some() || g.rollback_since_unix.is_some());
    let days = format!(
        "{}／{}／{} 天",
        s.raw_retention_days, s.rollup_5m_retention_days, s.rollup_1h_retention_days
    );
    let summary = match (&db.retention, paused) {
        (_, true) => "系統日期異常，暫停清除過期資料".to_string(),
        (Some(r), _) => format!(
            "正常 · 上次 {} 刪除原始 report {} 筆（{}）",
            ts_ms(r.finished_at),
            r.deleted_raw,
            days
        ),
        (None, _) => format!("正常（{days}）"),
    };
    let item = Item::new(
        "retention",
        if paused { Level::Warning } else { Level::Ok },
        summary,
        json!({
            "raw_retention_days": s.raw_retention_days,
            "rollup_5m_retention_days": s.rollup_5m_retention_days,
            "rollup_1h_retention_days": s.rollup_1h_retention_days,
            "paused": paused,
            "last_run": db.retention.as_ref().map(|r| json!({
                "finished_at": ts_ms(r.finished_at),
                "deleted": { "raw": r.deleted_raw, "rollup_5m": r.deleted_rollup_5m, "rollup_1h": r.deleted_rollup_1h },
            })),
        }),
    );
    if paused { item.code("KRS-W4201") } else { item }
}

fn report_writes(ctx: &ApiContext) -> Item {
    let now = Instant::now();
    let stats = &ctx.write_stats;
    let hour = stats.window(now, 60);
    let totals = stats.totals();
    // 最近 5 分鐘每一分鐘都有失敗：持續發生
    let sustained = stats.uptime(now).as_secs() >= 5 * 60
        && stats.per_minute(now, 5).iter().all(|c| c.has_failures());
    let status = if sustained {
        Level::Critical
    } else if hour.has_failures() {
        Level::Warning
    } else {
        Level::Ok
    };
    let summary = format!(
        "過去 1 小時：成功 {} · storage_unavailable {} · 結果未知 {} · 重複已去除 {}",
        percent(hour.success_ratio()),
        hour.storage_unavailable,
        hour.unknown,
        hour.duplicate
    );
    let item = Item::new(
        "report_writes",
        status,
        summary,
        json!({
            "window_1h": hour,
            "success_ratio_1h": hour.success_ratio(),
            "since_start": totals,
        }),
    );
    if status == Level::Ok {
        item
    } else {
        item.code("KRS-E4101")
    }
}

/// 線上主機中協商了補送、正在補送的台數
pub fn delivery_counts(live: &[LiveAgent], now: Instant) -> (usize, usize, usize) {
    let online: Vec<&LiveAgent> = live.iter().filter(|l| l.online).collect();
    let durable = online.iter().filter(|l| l.durable == Some(true)).count();
    let replaying = online.iter().filter(|l| l.replaying(now)).count();
    (online.len(), durable, replaying)
}

fn delivery(live: &[LiveAgent]) -> Item {
    let (online, durable, replaying) = delivery_counts(live, Instant::now());
    Item::new(
        "delivery",
        Level::Ok,
        format!("支援補送 {durable} / {online} 台 · 補送中 {replaying} 台"),
        json!({ "online": online, "durable": durable, "replaying": replaying }),
    )
}

fn connections(ctx: &ApiContext) -> Item {
    let (agents, connected) = ctx.registry.sizes();
    let max = ctx.config.server.max_connections;
    let usage = ratio(connected as u64, max as u64).unwrap_or(0.0);
    let status = if usage >= 0.98 {
        Level::Critical
    } else if usage >= 0.9 {
        Level::Warning
    } else {
        Level::Ok
    };
    let rejections = ctx
        .per_ip
        .as_ref()
        .map(|p| p.snapshot(Instant::now()).rejections);
    let item = Item::new(
        "connections",
        status,
        format!(
            "{connected} / {max}（server.max_connections）· 使用率 {}",
            percent(Some(usage))
        ),
        json!({
            "connections": connected,
            "max_connections": max,
            "usage_ratio": usage,
            "registry": { "agents": agents, "max_agents": ctx.registry.max_agents() },
            "rejected_since_start": rejections.map(|r| json!({
                "per_ip": r.per_ip.total,
                "per_source_rate": r.per_source_rate.total,
                "global_pauses": r.global.pauses,
                "global_paused_secs": r.global.paused.as_secs_f64(),
                "paused_now": r.global.paused_now,
                "handshake_limit": r.global.handshake_limit_rejected,
            })),
        }),
    );
    if status == Level::Ok {
        item
    } else {
        item.code("KRS-W2204")
    }
}

fn tls(ctx: &ApiContext, now: DateTime<Utc>) -> Item {
    let material = ctx.agent_tls.current();
    let mut status = Level::Ok;
    let mut code = None;
    let mut list = Vec::new();
    let mut parts = Vec::new();
    for e in &material.expiries {
        let days = (e.at - now).num_seconds() as f64 / 86_400.0;
        let (level, c, what) = match e.kind {
            ExpiryKind::ServerCert => {
                let l = if days <= 7.0 {
                    Level::Critical
                } else if days <= 30.0 {
                    Level::Warning
                } else {
                    Level::Ok
                };
                (l, "KRS-W1001", "伺服器憑證")
            }
            ExpiryKind::Crl => {
                let (l, c) = if days <= 1.0 {
                    (Level::Critical, "KRS-E1203")
                } else if days <= 7.0 {
                    (Level::Warning, "KRS-W1201")
                } else {
                    (Level::Ok, "KRS-W1201")
                };
                (l, c, "CRL")
            }
        };
        if level > status {
            status = level;
            code = Some(c);
        }
        parts.push(format!("{what} {} 天後到期", days.floor().max(0.0)));
        list.push(json!({
            "kind": match e.kind { ExpiryKind::ServerCert => "server_certificate", ExpiryKind::Crl => "crl" },
            "path": e.path,
            "index": e.index,
            "expires_at": ts(e.at),
            "days_left": days,
            "status": level,
        }));
    }
    let mut item = Item::new(
        "tls",
        status,
        if parts.is_empty() {
            "沒有可讀出到期日的項目".to_string()
        } else {
            parts.join(" · ")
        },
        json!({ "mtls": material.auth_enabled(), "crl_count": material.crl_count, "expiries": list }),
    );
    item.product_code = code.filter(|_| status != Level::Ok);
    item
}

fn listeners(ctx: &ApiContext) -> Item {
    Item::new(
        "listeners",
        Level::Ok,
        format!(
            "agent {}（{}）· 主控台 {}",
            ctx.agent_listen,
            if ctx.agent_tls.current().auth_enabled() {
                "mTLS"
            } else {
                "未啟用 mTLS"
            },
            ctx.console_listen
        ),
        json!({ "agent": ctx.agent_listen.to_string(), "console": ctx.console_listen.to_string() }),
    )
}

fn file_descriptors(ctx: &ApiContext) -> Item {
    let needed = ctx.config.fd_needed();
    let suggested = crate::config::suggested_nofile(needed);
    let soft = crate::nofile_soft_limit();
    let low = soft.is_some_and(|s| s < needed);
    Item::new(
        "file_descriptors",
        if low { Level::Warning } else { Level::Ok },
        match soft {
            Some(s) if s == u64::MAX => format!("不限制 · 需要 {needed}"),
            Some(s) => format!("{s}（需要 {needed}，建議 LimitNOFILE={suggested}）"),
            None => format!("無法讀取（需要 {needed}）"),
        },
        json!({
            "soft_limit": soft.filter(|s| *s != u64::MAX),
            "unlimited": soft == Some(u64::MAX),
            "needed": needed,
            "suggested": suggested,
        }),
    )
}

fn auth(ctx: &ApiContext) -> Item {
    let mtls = ctx.agent_tls.current().auth_enabled();
    let exposed = !crate::config::is_loopback(ctx.agent_listen.ip());
    let unauthenticated = !mtls && exposed;
    let item = Item::new(
        "auth",
        if unauthenticated {
            Level::Critical
        } else {
            Level::Ok
        },
        if mtls {
            "已啟用 mTLS".to_string()
        } else if exposed {
            "未啟用 mTLS 卻監聽對外位址（auth.allow_unauthenticated = true）".to_string()
        } else {
            "未啟用 mTLS，只監聽 loopback".to_string()
        },
        json!({
            "mtls": mtls,
            "allow_unauthenticated": ctx.config.auth.allow_unauthenticated,
            "exposed": exposed,
        }),
    );
    if unauthenticated {
        item.code("KRS-W0302")
    } else {
        item
    }
}

/// 系統健康
pub async fn compute(ctx: &Arc<ApiContext>, now: DateTime<Utc>) -> Result<Health, ApiError> {
    let db = ctx
        .pool
        .read("系統健康", |conn| {
            read_db_stats(conn).map_err(query_failed("系統健康"))
        })
        .await?;
    let live = ctx.registry.live_view();
    let items = vec![
        database(ctx, &db, &live, now.timestamp_millis()),
        retention(ctx, &db),
        report_writes(ctx),
        delivery(&live),
        connections(ctx),
        tls(ctx, now),
        listeners(ctx),
        file_descriptors(ctx),
        auth(ctx),
    ];
    let status = items.iter().map(|i| i.status).max().unwrap_or(Level::Ok);
    Ok(Health {
        status,
        items,
        service: json!({
            "version": env!("CARGO_PKG_VERSION"),
            "commit": env!("KAIROS_BUILD_COMMIT"),
            "started_at": ts(ctx.started),
            "uptime_secs": ctx.started_i.elapsed().as_secs(),
            "config_file": ctx.config_file.as_ref().map(|p| p.display().to_string()),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_helpers() {
        assert_eq!(gb(36_400_000_000), "36.4 GB");
        assert_eq!(percent(ratio(53, 100)), "53%");
        assert_eq!(percent(ratio(1, 0)), "—");
        assert!(Level::Critical > Level::Warning && Level::Warning > Level::Ok);
        assert_eq!(serde_json::to_value(Level::Warning).unwrap(), "warning");
    }
}
