//! 系統健康（`kairos/API.md` 第 5.5 節；設計包 S20、§25.5 #3、#4）。
//!
//! 每個檢查項目一列：狀態（`ok`／`warning`／`critical`）、對應的產品代碼、摘要與數值；整體取最嚴重者。
//!
//! 0.8 起簽發 CA、註冊端點、告警引擎、`/metrics` 端點也在這裡（設計包 INTEGRATION §29.6「S20 項目併入
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
