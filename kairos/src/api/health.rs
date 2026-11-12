//! 系統健康（`kairos/API.md` 第 5.5 節；設計包 S20、§25.5 #3、#4）。
//!
//! 每個檢查項目一列：狀態（`ok`／`warning`／`critical`）、對應的產品代碼、摘要與數值；整體取最嚴重者。
//!
//! 0.8 起簽發 CA、註冊端點、告警引擎、`/metrics` 端點也在這裡（0.9 另有遠端管理 `remote`）（設計包 INTEGRATION §29.6「S20 項目併入
//! `system/health`」）：**整體狀態對所有人相同**，但這幾項的摘要與數值只給有對應讀取權限的身分
//! （[`Item::detail`]；沒有權限時 `restricted: true`、`summary` 為空字串、`values` 為 `null`，狀態與代碼照常）。

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

use super::ApiContext;
use super::db::query_failed;
use super::http::{ApiError, ts, ts_ms};
use crate::enroll::pki_mgr::IssueStatus;
use crate::expiry::ExpiryKind;
use crate::rbac::{Grants, Permission};
use crate::registry::LiveAgent;
use crate::store::prelude::*;
use crate::store::sqlite::{CapacityEvent, META_CAPACITY_EVENT, META_RETENTION_LAST, RetentionRun};
use kairos_proto::enroll::EnrollMode;

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
    /// 摘要與數值因權限而省略（狀態與代碼照常）
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub restricted: bool,
    /// 看摘要與數值需要的權限（`None`：有 `health:read` 即可）
    #[serde(skip)]
    pub detail: Option<Permission>,
}

impl Item {
    fn new(id: &'static str, status: Level, summary: impl Into<String>, values: Value) -> Self {
        Self {
            id,
            status,
            product_code: None,
            summary: summary.into(),
            values,
            restricted: false,
            detail: None,
        }
    }

    fn code(mut self, code: &'static str) -> Self {
        self.product_code = Some(code);
        self
    }

    fn needs(mut self, perm: Permission) -> Self {
        self.detail = Some(perm);
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

impl Health {
    /// 依權限省略細節（整體狀態、各項的狀態與代碼不變）
    pub fn redact(mut self, grants: &Grants) -> Self {
        for item in &mut self.items {
            if item.detail.is_some_and(|p| !grants.has(p)) {
                item.summary = String::new();
                item.values = Value::Null;
                item.restricted = true;
            }
        }
        self
    }
}

/// 資料庫的數值（唯讀連線取得）
#[derive(Debug, Default)]
pub(crate) struct DbStats {
    pub(crate) used_bytes: u64,
    pub(crate) page_size: u64,
    pub(crate) capacity: Option<CapacityEvent>,
    pub(crate) retention: Option<RetentionRun>,
    pub(crate) guard: Option<date_guard::GuardState>,
}

pub(crate) fn read_db_stats(conn: &dyn Store) -> StoreResult<DbStats> {
    let (page_size, pages) = conn.db_page_stats()?;
    let meta = |key: &str| conn.get_meta(key);
    Ok(DbStats {
        used_bytes: u64::try_from(pages.max(0) * page_size).unwrap_or(0),
        page_size: u64::try_from(page_size).unwrap_or(0),
        capacity: meta(META_CAPACITY_EVENT)?.and_then(|s| serde_json::from_str(&s).ok()),
        retention: meta(META_RETENTION_LAST)?.and_then(|s| serde_json::from_str(&s).ok()),
        guard: meta("retention_guard")?.and_then(|s| serde_json::from_str(&s).ok()),
    })
}

/// PostgreSQL 後端的狀態（`storage.backend = "postgres"` 時；否則為 `null`）：伺服器版本、同步複寫、時差
/// 與連線池。伺服器的狀態由健康檢查（`/readyz`、`/metrics`）定期更新，尚未讀到時相關欄位為 `null`
fn postgres_values() -> serde_json::Value {
    let Some(pool) = crate::store::postgres::pool::active() else {
        return serde_json::Value::Null;
    };
    let s = pool.stats();
    let server = pool.last_server();
    json!({
        "target": pool.settings().display_target(),
        "sslmode": pool.settings().effective_sslmode().as_str(),
        "server_version": server.as_ref().map(|i| i.version.clone()),
        "synchronous_replication": server.as_ref().map(|i| i.synchronous_replication()),
        "clock_skew_ms": server.as_ref().map(|i| i.clock_skew_ms),
        "pool": { "size": s.size, "idle": s.idle, "busy": s.busy, "exhausted": s.exhausted },
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
            "backend": ctx.config.storage.backend.as_str(),
            "postgres": postgres_values(),
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

/// 簽發 CA（S20「簽發 CA」：即將到期 `KRS-W1304`；線上簽發但私鑰無法使用 `KRS-E1303`；
/// 離線簽發且有超過 24 小時的待簽發申請）
async fn issuing_ca(ctx: &ApiContext, now_ms: i64) -> Option<Item> {
    let svc = ctx.enroll.clone()?;
    let s2 = svc.clone();
    let read = svc
        .db
        .read(move |conn| {
            let cas = conn.list_cas()?;
            let threshold = s2.issuing_ca_warning_ms(conn, now_ms);
            let (stale, oldest): (i64, Option<i64>) =
                conn.stale_approvals(now_ms - 24 * 3_600_000)?;
            Ok((cas, threshold, stale, oldest))
        })
        .await;
    let (cas, threshold, stale, oldest) = match read {
        Ok(v) => v,
        Err(e) => {
            return Some(
                Item::new(
                    "issuing_ca",
                    Level::Warning,
                    format!("無法讀取簽發 CA：{e}"),
                    Value::Null,
                )
                .needs(Permission::PkiRead),
            );
        }
    };
    let offline = svc.pki.mode() == crate::config::PkiMode::Offline;
    let mut status = Level::Ok;
    let mut code = None;
    let mut parts = Vec::new();
    let mut list = Vec::new();
    for c in cas
        .iter()
        .filter(|c| c.state == crate::pki::store::CA_ACTIVE)
    {
        let left = c.not_after.map(|t| (t - now_ms) as f64 / 86_400_000.0);
        let (level, c_code) = match c.not_after {
            Some(t) if t <= now_ms => (Level::Critical, Some("KRS-E1303")),
            Some(t) if t - now_ms < threshold => (Level::Warning, Some("KRS-W1304")),
            _ => (Level::Ok, None),
        };
        if level > status {
            status = level;
            code = c_code;
        }
        parts.push(match left {
            Some(d) => format!("{} 剩 {} 天", c.subject, d.floor().max(0.0)),
            None => format!("{}：沒有到期日", c.subject),
        });
        list.push(json!({
            "ca_id": c.ca_id,
            "subject": c.subject,
            "not_after": c.not_after.map(ts_ms),
            "days_left": left,
            "status": level,
        }));
    }
    if let IssueStatus::Unavailable(reason) = svc.pki.status() {
        status = Level::Critical;
        code = Some("KRS-E1303");
        parts.insert(0, format!("線上簽發但無法簽發：{reason}"));
    }
    if offline && stale > 0 && status < Level::Warning {
        status = Level::Warning;
        parts.push(format!("待簽發有 {stale} 筆超過 24 小時"));
    }
    let summary = if parts.is_empty() {
        if offline {
            "離線簽發模式（簽發 CA 不在這台主機）".to_string()
        } else {
            "沒有啟用中的簽發 CA".to_string()
        }
    } else if offline {
        format!("離線簽發 · {}", parts.join(" · "))
    } else {
        parts.join(" · ")
    };
    let mut item = Item::new(
        "issuing_ca",
        status,
        summary,
        json!({
            "pki_mode": svc.pki.mode().as_str(),
            "expiry_warning_days": threshold / 86_400_000,
            "active": list,
            "signing_queue_stale": stale,
            "signing_queue_oldest": oldest.map(ts_ms),
        }),
    )
    .needs(Permission::PkiRead);
    item.product_code = code.filter(|_| status != Level::Ok);
    Some(item)
}

/// 註冊端點（S20「註冊端點」狀態表；`decisions/enroll-pause.md` §4.5）
async fn enroll(ctx: &ApiContext, now_ms: i64) -> Option<Item> {
    let svc = ctx.enroll.clone()?;
    let eff = svc.effective();
    let state = svc.mode_state();
    let listen = svc.listen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let grace = eff.policy.expired_renew_grace_days;
    let impact = svc
        .db
        .read(move |conn| {
            crate::enroll::ops::impact(conn, grace, now_ms)
                .map_err(|e| crate::enroll::db::DbError(e.message))
        })
        .await
        .ok();
    let blocked = svc.limits.l7.blocked_count(svc.now_ms());
    let mode_text = match state {
        crate::enroll::ModeState::Open => "開放",
        crate::enroll::ModeState::OpenPaused => "開放 · 新註冊已自動暫停",
        crate::enroll::ModeState::RenewOnly => "僅續簽",
        crate::enroll::ModeState::Closed => "關閉",
    };
    let addr = listen.addr.clone().unwrap_or_else(|| "—".into());
    let (mut status, mut code, mut summary) = (Level::Ok, None, String::new());
    let mut set = |l: Level, c: Option<&'static str>, text: String| {
        if l > status || summary.is_empty() {
            status = l;
            code = c;
            summary = text;
        }
    };
    if eff.mode != EnrollMode::Closed && !listen.listening {
        set(
            Level::Critical,
            Some("KRS-E2001"),
            format!("{mode_text} · 應在監聽但未監聽"),
        );
    }
    if let Some(i) = &impact {
        if eff.mode == EnrollMode::Closed && i.n + i.m > 0 {
            set(
                Level::Warning,
                Some("KRS-W2009"),
                format!(
                    "關閉 · {} 台在寬限期內或 30 天內將進入，無法續簽",
                    i.n + i.m
                ),
            );
        }
        if eff.mode == EnrollMode::RenewOnly && grace > 0 && i.t > 0 {
            set(
                Level::Warning,
                None,
                format!("僅續簽 · 但有 {} 個有效註冊 token 無法使用", i.t),
            );
        }
    }
    if eff.mode == EnrollMode::RenewOnly && grace == 0 {
        set(
            Level::Warning,
            Some("KRS-W2011"),
            "僅續簽 · 過期寬限期為 0 天，不會受理任何請求".to_string(),
        );
    }
    if blocked > 0 {
        set(
            Level::Warning,
            Some("KRS-W3005"),
            format!("封鎖中 {blocked} 個來源（短時間內大量無效 token）"),
        );
    }
    if summary.is_empty() {
        summary = match eff.mode {
            EnrollMode::Closed if impact.as_ref().is_some_and(|i| i.n + i.m == 0) => {
                "關閉 · 目前沒有需要過期續簽的主機".to_string()
            }
            EnrollMode::Closed => "關閉".to_string(),
            _ if grace > 0 => format!("{mode_text} · {addr} · 過期續簽可用"),
            _ => format!("{mode_text} · {addr}"),
        };
    }
    let item = Item::new(
        "enroll",
        status,
        summary,
        json!({
            "mode": eff.mode.as_str(),
            "state": state.as_str(),
            "listening": listen.listening,
            "listen_addr": listen.addr,
            "not_listening_reason": listen.reason,
            "expired_renew_grace_days": grace,
            "blocked_sources": blocked,
            "impact": impact.map(|i| json!({"N": i.n, "M": i.m, "L": i.l, "R": i.r, "T": i.t})),
        }),
    )
    .needs(Permission::EnrollRead);
    Some(match code {
        Some(c) => item.code(c),
        None => item,
    })
}

/// 告警引擎（S20「告警引擎／通知佇列」；`alerts.enabled = false` 時為正常並註明已停用）
async fn alerts(ctx: &ApiContext) -> Item {
    let Some(svc) = ctx.alerts.as_ref() else {
        return Item::new(
            "alerts",
            Level::Ok,
            "已停用（alerts.enabled = false）",
            json!({ "enabled": false }),
        )
        .needs(Permission::AlertsRead);
    };
    let st = svc.status().await;
    let running = st["running"].as_bool().unwrap_or(false);
    let warmup = st["warmup"]["active"].as_bool().unwrap_or(false);
    let summary = if !running {
        format!(
            "未執行（{}）",
            st["last_error"]
                .as_str()
                .unwrap_or("最近 60 秒沒有完成判定")
        )
    } else if warmup {
        "執行中 · 暖機期（暖機期間不開主機離線事件）".to_string()
    } else {
        "執行中".to_string()
    };
    let mut values = st;
    values["enabled"] = json!(true);
    Item::new(
        "alerts",
        if running { Level::Ok } else { Level::Critical },
        summary,
        values,
    )
    .needs(Permission::AlertsRead)
}

/// `/metrics` 端點（S20「/metrics 端點」列的三種寫法；D36、D42）
fn metrics_endpoint(ctx: &ApiContext) -> Item {
    let o = &ctx.config.observability;
    let token = !o.metrics_token_file.trim().is_empty();
    let proxied = ctx.obs.as_ref().is_some_and(|p| p.proxied_rejected());
    let mut parts = vec![if !o.on_console {
        "未在主控台的連接埠提供".to_string()
    } else if token {
        "以 bearer token 驗證".to_string()
    } else {
        "只接受本機（loopback）".to_string()
    }];
    if !o.listen.trim().is_empty() {
        parts.push(format!("另以 http://{} 提供", o.listen.trim()));
    }
    if proxied {
        parts.push("曾拒絕經反向代理轉送的請求".to_string());
    }
    Item::new(
        "metrics",
        Level::Ok,
        parts.join(" · "),
        json!({
            "on_console": o.on_console,
            "token": token,
            "listen": (!o.listen.trim().is_empty()).then(|| o.listen.trim().to_string()),
            "proxied_rejected": proxied,
        }),
    )
    .needs(Permission::SettingsRead)
}

/// 遠端管理（S20「遠端管理」列；設計包 screens.md S20「0.9 規格」、REMOTE.md R10）：集中設定與遠端升級是否
/// 啟用、信任錨、推送的暫停與回滾失敗、版本庫的檔案遺失。數值需要 `rollouts:read`。嚴重度：回滾失敗未處理
/// （`KRS-E8408`）＞ 自動暫停的推送（`KRS-W8414`）＞ 版本庫的檔案遺失（已存入的發佈檔不存在或大小不符，例如
/// 還原備份之後；`KRS-W8417`〔暫定〕）＞ 信任錨未設定（遠端升級啟用、版本庫有發佈版本時；版本庫是空的只註明）；
/// 主控台暫停的推送不算異常
async fn remote(ctx: &ApiContext) -> Item {
    let Some(svc) = ctx.remote.as_ref() else {
        return Item::new(
            "remote",
            Level::Ok,
            "未提供（需要 SQLite 儲存）",
            json!({"available": false, "config_enabled": false, "upgrade_enabled": false}),
        )
        .needs(Permission::RolloutsRead);
    };
    let st = &svc.settings;
    let trust = !st.allowed_signers.trim().is_empty();
    let mut values = json!({
        "available": true,
        "config_enabled": st.config_enabled,
        "upgrade_enabled": st.upgrade_enabled,
        "allowed_signers_configured": trust,
        "rollout_approval": st.rollout_approval,
    });
    if !st.config_enabled && !st.upgrade_enabled {
        return Item::new(
            "remote",
            Level::Ok,
            "已停用（remote.config_enabled、remote.upgrade_enabled = false）",
            values,
        )
        .needs(Permission::RolloutsRead);
    }
    if !st.upgrade_enabled {
        return Item::new(
            "remote",
            Level::Ok,
            "集中設定啟用 · 遠端升級已停用（remote.upgrade_enabled = false）",
            values,
        )
        .needs(Permission::RolloutsRead);
    }
    use crate::remote::rollout;
    let dir = svc.releases_dir.clone();
    let now_ms = svc.now_ms();
    // 雙人覆核：啟動時與資料庫的前值比對的結果（前值、來源、最近一次改變；ADR-0002 決策點 2i）
    let policy = svc.approval_policy();
    values["rollout_approval_observed"] = policy.map_or(Value::Null, |o| o.json());
    values["rollout_approval_change"] = policy.map_or(Value::Null, |o| o.recent_change_json(now_ms));
    let disabled_recently = policy.is_some_and(|o| {
        o.disabled_at
            .is_some_and(|at| now_ms - at < crate::remote::policy::RECENT_CHANGE_MS)
    });
    let counts = svc
        .db
        .read(move |s| {
            let releases = s.list_releases()?;
            // 版本庫的檔案遺失：已存入的發佈檔只 stat（不重算雜湊；這個讀取本來就在 blocking 執行緒）
            let mut missing = Vec::new();
            let mut hosts = None;
            for r in &releases {
                let artifacts = s.release_artifacts(r.id)?;
                for a in artifacts.iter().filter(|a| a.stored) {
                    let Some(problem) =
                        crate::remote::releases::file_problem(&dir, &a.sha256, a.size)
                    else {
                        continue;
                    };
                    if hosts.is_none() {
                        hosts = Some(s.upgrade_hosts()?);
                    }
                    // 這個檔案是多少台主機的回滾檔（deb／rpm、目前執行這個版本）
                    let rollback = crate::remote::ops::rollback_target_count(
                        hosts.as_deref().unwrap_or_default(),
                        &r.version,
                        std::slice::from_ref(a),
                    );
                    missing.push((r.version.clone(), a.filename.clone(), problem, rollback));
                }
            }
            // 進行中的推送裡等待版本庫檔案的主機（ADR-0002 F3：不提議、留在 pending，補檔後自動繼續）
            let running = s.list_rollouts(Some(rollout::ROLLOUT_RUNNING))?;
            let mut waiting = 0usize;
            if !missing.is_empty() {
                for r in &running {
                    waiting += s
                        .rollout_hosts(&r.id)?
                        .iter()
                        .filter(|h| {
                            h.state == rollout::HOST_PENDING
                                && h.reason.as_deref() == Some(rollout::SKIP_ARTIFACT_MISSING)
                        })
                        .count();
                }
            }
            // 待核准的推送，以及其中目前沒有任何人可以核准的（ADR-0002 決策點 2g）
            let pending = s.list_rollouts(Some(rollout::ROLLOUT_PENDING_APPROVAL))?;
            let no_approvers = if pending.is_empty() {
                0
            } else {
                let approvers = crate::remote::submission::approvers(s, now_ms)?;
                pending
                    .iter()
                    .filter(|r| {
                        crate::remote::submission::approvers_available(
                            &approvers,
                            &r.created_by,
                            r.submitted_by.as_deref(),
                        ) == 0
                    })
                    .count()
            };
            Ok((
                running.len(),
                s.list_rollouts(Some(rollout::ROLLOUT_PAUSED))?,
                releases.len(),
                s.unacknowledged_rollback_failures()?.len(),
                missing,
                (waiting, pending.len(), no_approvers),
            ))
        })
        .await;
    let (running, paused, releases, failures, missing, (waiting, pending, no_approvers)) = match counts {
        Ok(v) => v,
        Err(e) => {
            log::warn!("系統健康：無法讀取推送的狀態：{e}");
            values["error"] = json!("無法讀取推送的狀態");
            return Item::new("remote", Level::Warning, "無法讀取推送的狀態", values)
                .needs(Permission::RolloutsRead);
        }
    };
    let auto: Vec<_> = paused.iter().filter(|r| r.pause_cause.is_some()).collect();
    values["rollouts_running"] = json!(running);
    values["rollouts_paused"] = json!(paused.len());
    values["rollouts_auto_paused"] = json!(auto.len());
    values["paused"] = json!(
        paused
            .iter()
            .take(10)
            .map(|r| json!({
                "id": r.id,
                "name": r.name,
                "reason": r.pause_reason,
                "cause": rollout::pause_cause(&r.state, r.pause_cause.as_deref()),
            }))
            .collect::<Vec<_>>()
    );
    values["releases"] = json!(releases);
    values["release_files_missing"] = json!(missing.len());
    values["missing_files"] = json!(
        missing
            .iter()
            .take(20)
            .map(|(version, filename, problem, rollback)| json!({
                "version": version,
                "filename": filename,
                "problem": problem,
                "rollback_target_for": rollback,
            }))
            .collect::<Vec<_>>()
    );
    values["rollback_failed_unacknowledged"] = json!(failures);
    values["hosts_waiting_artifact"] = json!(waiting);
    values["pending_approval"] = json!(pending);
    values["pending_approval_no_approvers"] = json!(no_approvers);
    values["downloads_in_progress"] = json!(svc.downloads_in_progress());
    values["max_concurrent_downloads"] = json!(st.max_concurrent_downloads);
    let config_note = if st.config_enabled {
        ""
    } else {
        " · 集中設定已停用"
    };
    let (status, code, summary) = if failures > 0 {
        (
            Level::Critical,
            Some(crate::remote::CODE_ROLLBACK_FAILED),
            format!("{failures} 台主機升級回滾失敗，需要人工處理"),
        )
    } else if let Some(r) = auto.first() {
        let more = if auto.len() > 1 {
            format!(" 等 {} 個", auto.len())
        } else {
            String::new()
        };
        (
            Level::Warning,
            Some(crate::remote::CODE_ROLLOUT_PAUSED),
            format!(
                "推送已自動暫停：{}{more}（{}）",
                r.name,
                r.pause_reason.as_deref().unwrap_or("—")
            ),
        )
    } else if let Some((version, filename, problem, rollback)) = missing.first() {
        let what = if *problem == "size_mismatch" {
            "版本庫的檔案大小不符"
        } else {
            "版本庫的檔案遺失"
        };
        let need = if *rollback > 0 {
            format!("（{rollback} 台的回滾需要）")
        } else {
            String::new()
        };
        let more = if missing.len() > 1 {
            format!(" 等 {} 個檔案", missing.len())
        } else {
            String::new()
        };
        let waiting_note = if waiting > 0 {
            format!("；{waiting} 台主機等待中")
        } else {
            String::new()
        };
        (
            Level::Warning,
            Some(crate::remote::CODE_RELEASE_FILE_MISSING),
            format!(
                "{what}：{version} 的 {filename}{need}{more}{waiting_note}；請重新匯入這個版本的發佈檔"
            ),
        )
    } else if disabled_recently {
        (
            Level::Warning,
            None,
            "雙人覆核已關閉（remote.rollout_approval 由 required 改為 none）：送出的推送不需要另一位管理員核准"
                .to_string(),
        )
    } else if no_approvers > 0 {
        (
            Level::Warning,
            None,
            format!("{no_approvers} 個待核准的推送目前沒有可以核准的管理員（建立者與送出者不能核准）"),
        )
    } else if !trust && releases > 0 {
        (
            Level::Warning,
            None,
            "信任錨未設定：匯入的發佈版本無法驗證簽章（不影響 Pulse 端的驗證）".to_string(),
        )
    } else {
        let paused_note = if paused.is_empty() {
            String::new()
        } else {
            format!(" · 已暫停 {} 個", paused.len())
        };
        // 版本庫還是空的：信任錨未設定只註明（還沒有使用遠端升級，不列為警告）
        let trust_note = if trust { "" } else { " · 信任錨未設定" };
        (
            Level::Ok,
            None,
            format!(
                "進行中的推送 {running} 個{paused_note} · 版本庫 {releases} 個版本{trust_note}{config_note}"
            ),
        )
    };
    let item = Item::new("remote", status, summary, values).needs(Permission::RolloutsRead);
    match code {
        Some(c) => item.code(c),
        None => item,
    }
}

/// 叢集的狀態（`GET /api/v1/cluster` 與系統健康的 `cluster` 項目共用；`kairos/HA.md` 第 14.2、14.3 節）：
/// `enabled`、`mode`、`cluster_id`、`leader`、`nodes[]`、`database`，以及回應這個請求的節點 `node_id`。
/// 沒有啟用叢集模式時 `enabled` 為 `false`、沒有節點
pub async fn cluster_status(ctx: &Arc<ApiContext>) -> Result<Value, ApiError> {
    use crate::cluster::store::LEASE_LEADER;
    let state = crate::cluster::global();
    let (cluster_id, lease, nodes) = if state.is_some() {
        ctx.pool
            .read("叢集狀態", |conn| {
                Ok((
                    conn.get_meta(crate::cluster::META_CLUSTER_ID)
                        .map_err(query_failed("叢集狀態"))?,
                    conn.cluster_lease(LEASE_LEADER, None)
                        .map_err(query_failed("叢集狀態"))?,
                    conn.cluster_nodes(None).map_err(query_failed("叢集狀態"))?,
                ))
            })
            .await?
    } else {
        (None, None, Vec::new())
    };
    let mut values = crate::cluster_cli::status_value(
        &ctx.config,
        cluster_id.as_deref(),
        lease.as_ref(),
        &nodes,
    );
    values["node_id"] = json!(state.as_ref().map(|s| s.node_id.clone()));
    values["database"] = postgres_values();
    Ok(values)
}

/// 系統健康的 `cluster` 項目（只在叢集模式）：沒有 leader、金鑰不一致為嚴重；節點失聯、設定不一致、
/// 沒有同步複寫、時差過大、只有一個節點為警告
async fn cluster(ctx: &Arc<ApiContext>) -> Option<Item> {
    crate::cluster::global()?;
    let values = match cluster_status(ctx).await {
        Ok(v) => v,
        Err(_) => {
            return Some(
                Item::new("cluster", Level::Warning, "無法讀取叢集的狀態", Value::Null)
                    .needs(Permission::SettingsRead),
            );
        }
    };
    let mut problems = crate::cluster_cli::status_problems(&ctx.config, &values);
    if let Some(server) = crate::store::postgres::pool::active().and_then(|p| p.last_server()) {
        if !server.synchronous_replication() {
            problems.push((
                false,
                format!(
                    "{}：資料庫沒有同步複寫",
                    crate::store::postgres::pool::CODE_PG_NO_SYNC_REPLICATION
                ),
            ));
        }
        if server.clock_skew_ms.abs() > crate::store::postgres::pool::CLOCK_SKEW_WARN_MS {
            problems.push((
                false,
                format!(
                    "{}：這個節點與資料庫的時差 {} 毫秒",
                    crate::store::postgres::pool::CODE_PG_CLOCK_SKEW,
                    server.clock_skew_ms
                ),
            ));
        }
    }
    let level = if problems.iter().any(|(c, _)| *c) {
        Level::Critical
    } else if problems.is_empty() {
        Level::Ok
    } else {
        Level::Warning
    };
    let running = values["nodes"].as_array().map_or(0, |n| {
        n.iter()
            .filter(|n| !matches!(n["role"].as_str(), Some("stopped" | "lost")))
            .count()
    });
    let mut summary = match values["leader"]["node_id"].as_str() {
        Some(leader) => format!(
            "leader {leader}（epoch {}）；{running} 個節點在執行",
            values["leader"]["epoch"]
        ),
        None => format!("沒有 leader；{running} 個節點在執行"),
    };
    // 最嚴重的問題排在前面，代碼取第一個
    problems.sort_by_key(|(critical, _)| !*critical);
    let code = problems.first().and_then(|(_, m)| {
        [
            crate::cluster::CODE_COORDINATION,
            crate::cluster::CODE_KEYS,
            crate::cluster::CODE_NODE_LOST,
            crate::cluster::CODE_CONFIG_DRIFT,
            crate::store::postgres::pool::CODE_PG_NO_SYNC_REPLICATION,
            crate::store::postgres::pool::CODE_PG_CLOCK_SKEW,
        ]
        .into_iter()
        .find(|c| m.starts_with(c))
    });
    for (_, m) in &problems {
        summary.push('；');
        summary.push_str(m);
    }
    let mut item = Item::new("cluster", level, summary, values).needs(Permission::SettingsRead);
    if let Some(code) = code {
        item = item.code(code);
    }
    Some(item)
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
    let now_ms = now.timestamp_millis();
    let mut items = vec![
        database(ctx, &db, &live, now_ms),
        retention(ctx, &db),
        report_writes(ctx),
        delivery(&live),
        connections(ctx),
        tls(ctx, now),
        listeners(ctx),
        file_descriptors(ctx),
        auth(ctx),
    ];
    if let Some(i) = issuing_ca(ctx, now_ms).await {
        items.push(i);
    }
    if let Some(i) = enroll(ctx, now_ms).await {
        items.push(i);
    }
    items.push(alerts(ctx).await);
    items.push(metrics_endpoint(ctx));
    items.push(remote(ctx).await);
    if let Some(i) = cluster(ctx).await {
        items.push(i);
    }
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
