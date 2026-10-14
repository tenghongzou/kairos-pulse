//! 系統檢查（設計包 §24 #8；`alerts-ux.md` §2.2 的系統類規則）：每個 tick 從各元件的狀態與計數算出
//! 「目前成立的系統狀況」（[`SystemCondition`]），交給引擎以準位觸發的方式開啟、升級或恢復事件，
//! 並把狀態變化發布到事件匯流排（[`crate::bus::EventKind::Condition`]）。
//!
//! 計數類的檢查（被拒的連線、註冊端點的限流與封鎖）以「最近一次增加的時間」判斷「過去 1 小時是否發生」，
//! 計數本身在記憶體中（重新啟動後歸零，與系統健康相同）。資料庫的數值（容量事件、日期保護）由服務每 30 秒
//! 讀一次（[`DbFacts`]）。

use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::engine::SystemCondition;
use super::model::{Rule, Severity};
use super::rules::ids;
use crate::bus::{EventBus, EventKind};
use crate::config::AppConfig;
use crate::expiry::ExpiryKind;
use crate::per_ip::PerIpLimiter;
use crate::registry::Registry;
use crate::stats::WriteStats;
use crate::store::sqlite::CapacityEvent;
use crate::tls::TlsState;

/// 資料庫中的數值（服務定期讀取）
#[derive(Debug, Clone, Default)]
pub struct DbFacts {
    pub capacity: Option<CapacityEvent>,
    pub guard_paused: bool,
}

/// 系統檢查的來源
pub struct Probe {
    pub config: Arc<AppConfig>,
    pub registry: Arc<Registry>,
    pub write_stats: Option<Arc<WriteStats>>,
    pub per_ip: Option<Arc<PerIpLimiter>>,
    pub tls: Option<Arc<TlsState>>,
    pub enroll: Option<Arc<crate::enroll::EnrollService>>,
    pub agent_listen: Option<SocketAddr>,
    pub bus: Arc<EventBus>,
    /// 計數最近一次增加的時間（Unix 毫秒）
    per_source: HashMap<IpAddr, (u64, i64)>,
    global: (u64, u64, Option<i64>),
    enroll_counts: HashMap<&'static str, (u64, Option<i64>)>,
    /// 上一輪成立的項目（發布狀態變化用）：（規則, 對象）→ 嚴重度
    previous: HashMap<(&'static str, String), Severity>,
}

fn rule_param(rules: &[Rule], id: &str, key: &str, default: f64) -> f64 {
    rules
        .iter()
        .find(|r| r.rule_id == id)
        .and_then(|r| r.params.get(key))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(default)
}

fn days_text(days: f64) -> String {
    if days < 1.0 {
        format!("{:.0} 小時", (days * 24.0).max(0.0))
    } else {
        format!("{:.0} 天", days.floor())
    }
}

impl Probe {
    pub fn new(config: Arc<AppConfig>, registry: Arc<Registry>) -> Self {
        let bus = registry.bus();
        Self {
            config,
            registry,
            write_stats: None,
            per_ip: None,
            tls: None,
            enroll: None,
            agent_listen: None,
            bus,
            per_source: HashMap::new(),
            global: (0, 0, None),
            enroll_counts: HashMap::new(),
            previous: HashMap::new(),
        }
    }

    /// 一輪檢查：（成立的項目, 這一輪涵蓋的規則）
    pub fn check(
        &mut self,
        now: DateTime<Utc>,
        rules: &[Rule],
        db: Option<&DbFacts>,
    ) -> (Vec<SystemCondition>, Vec<&'static str>) {
        let now_ms = now.timestamp_millis();
        let mut out = Vec::new();
        let mut checked = Vec::new();
        if let Some(tls) = &self.tls {
            checked.extend([
                ids::CRL_EXPIRING,
                ids::CRL_EXPIRED,
                ids::SERVER_CERT_EXPIRING,
                ids::UNAUTHENTICATED,
            ]);
            self.tls_checks(tls.clone(), now, rules, &mut out);
        }
        if let Some(stats) = &self.write_stats {
            checked.push(ids::REPORT_WRITES);
            let window_min = (rule_param(rules, ids::REPORT_WRITES, "window_secs", 300.0) / 60.0)
                .ceil()
                .max(1.0) as usize;
            let w = stats.window(Instant::now(), window_min);
            if w.has_failures() {
                out.push(SystemCondition {
                    rule_id: ids::REPORT_WRITES,
                    subject: "report_writes".into(),
                    severity: Severity::Emergency,
                    title: format!(
                        "回報無法寫入：過去 {window_min} 分鐘有 {} 筆 report 未保存、{} 筆寫入結果未知",
                        w.storage_unavailable, w.unknown
                    ),
                    code: Some("KRS-E4101"),
                    details: json!({
                        "window_minutes": window_min,
                        "storage_unavailable": w.storage_unavailable,
                        "unknown": w.unknown,
                        "written": w.written,
                    }),
                });
            }
        }
        if let Some(db) = db {
            checked.extend([ids::DB_CAPACITY, ids::RETENTION_PAUSED]);
            self.db_checks(db, now_ms, &mut out);
        }
        checked.extend([ids::REGISTRY_CAPACITY, ids::CONNECTIONS]);
        self.connection_checks(now_ms, rules, &mut out);
        if self.per_ip.is_some() {
            checked.push(ids::PER_IP_LIMIT);
            self.per_ip_checks(now_ms, &mut out);
        }
        if let Some(svc) = self.enroll.clone() {
            checked.extend([
                ids::ENROLL_OVERLOAD,
                ids::ENROLL_BLOCKED,
                ids::ENROLL_RATE_LIMITED,
                ids::ENROLL_CLOCK_SKEW,
                ids::ENROLL_AUTO_PAUSED,
            ]);
            self.enroll_checks(&svc, now_ms, rules, &mut out);
        }
        self.publish(&out, now);
        (out, checked)
    }

    /// 狀態變化發布到匯流排（成立、嚴重度改變、解除）
    fn publish(&mut self, current: &[SystemCondition], now: DateTime<Utc>) {
        let mut next = HashMap::new();
        for c in current {
            let key = (c.rule_id, c.subject.clone());
            if self.previous.get(&key) != Some(&c.severity) {
                self.bus.publish_at(
                    now,
                    EventKind::Condition {
                        check: c.rule_id.to_string(),
                        subject: c.subject.clone(),
                        active: true,
                        severity: Some(c.severity.as_str()),
                        code: c.code,
                        summary: c.title.clone(),
                    },
                );
            }
            next.insert(key, c.severity);
        }
        for (rule, subject) in self.previous.keys() {
            if !next.contains_key(&(*rule, subject.clone())) {
                self.bus.publish_at(
                    now,
                    EventKind::Condition {
                        check: rule.to_string(),
                        subject: subject.clone(),
                        active: false,
                        severity: None,
                        code: None,
                        summary: String::new(),
                    },
                );
            }
        }
        self.previous = next;
    }

    fn tls_checks(
        &self,
        tls: Arc<TlsState>,
        now: DateTime<Utc>,
        rules: &[Rule],
        out: &mut Vec<SystemCondition>,
    ) {
        let material = tls.current();
        for e in &material.expiries {
            let days = (e.at - now).num_seconds() as f64 / 86_400.0;
            match e.kind {
                ExpiryKind::Crl => {
                    if days <= 0.0 {
                        out.push(SystemCondition {
                            rule_id: ids::CRL_EXPIRED,
                            subject: "crl".into(),
                            severity: Severity::Emergency,
                            title: "撤銷清單（CRL）已過期：所有 agent 都會被拒絕連線".into(),
                            code: Some("KRS-E1203"),
                            details: json!({ "path": e.path, "index": e.index, "next_update": crate::api::http::ts(e.at) }),
                        });
                        continue;
                    }
                    let w = rule_param(rules, ids::CRL_EXPIRING, "warning_days", 7.0);
                    let c = rule_param(rules, ids::CRL_EXPIRING, "critical_days", 3.0);
                    let em = rule_param(rules, ids::CRL_EXPIRING, "emergency_days", 1.0);
                    let sev = if days <= em {
                        Severity::Emergency
                    } else if days <= c {
                        Severity::Critical
                    } else if days <= w {
                        Severity::Warning
                    } else {
                        continue;
                    };
                    out.push(SystemCondition {
                        rule_id: ids::CRL_EXPIRING,
                        subject: format!("crl:{}:{}", e.path, e.index),
                        severity: sev,
                        title: format!("撤銷清單（CRL）將於 {} 後到期", days_text(days)),
                        code: Some("KRS-W1201"),
                        details: json!({ "path": e.path, "index": e.index, "next_update": crate::api::http::ts(e.at), "days_left": days }),
                    });
                }
                ExpiryKind::ServerCert => {
                    let w = rule_param(rules, ids::SERVER_CERT_EXPIRING, "warning_days", 30.0);
                    let c = rule_param(rules, ids::SERVER_CERT_EXPIRING, "critical_days", 14.0);
                    let em = rule_param(rules, ids::SERVER_CERT_EXPIRING, "emergency_days", 7.0);
                    let sev = if days <= em {
                        Severity::Emergency
                    } else if days <= c {
                        Severity::Critical
                    } else if days <= w {
                        Severity::Warning
                    } else {
                        continue;
                    };
                    out.push(SystemCondition {
                        rule_id: ids::SERVER_CERT_EXPIRING,
                        subject: format!("server_cert:{}", e.path),
                        severity: sev,
                        title: if days <= 0.0 {
                            "伺服器憑證已過期".to_string()
                        } else {
                            format!("伺服器憑證將於 {} 後到期", days_text(days))
                        },
                        code: Some("KRS-W1001"),
                        details: json!({ "path": e.path, "not_after": crate::api::http::ts(e.at), "days_left": days }),
                    });
                }
            }
        }
        if let Some(listen) = self.agent_listen
            && !material.auth_enabled()
            && !crate::config::is_loopback(listen.ip())
        {
            out.push(SystemCondition {
                rule_id: ids::UNAUTHENTICATED,
                subject: "auth".into(),
                severity: Severity::Critical,
                title: "未認證連線已允許：未啟用 mTLS 卻監聽對外位址".into(),
                code: Some("KRS-W0302"),
                details: json!({ "listen": listen.to_string() }),
            });
        }
    }

    fn db_checks(&self, db: &DbFacts, now_ms: i64, out: &mut Vec<SystemCondition>) {
        let max = self.config.storage.max_total_bytes;
        let estimate: u64 = self
            .registry
            .live_view()
            .iter()
            .filter(|l| l.online)
            .map(|l| {
                self.config.estimated_sqlite_bytes_per_host(
                    l.host
                        .as_ref()
                        .and_then(|h| h.report_interval_ms)
                        .unwrap_or(crate::config::ESTIMATE_REPORT_INTERVAL_SECS * 1000),
                )
            })
            .sum();
        let recent = db
            .capacity
            .as_ref()
            .is_some_and(|e| e.ended_at.is_none_or(|end| now_ms - end < 24 * 3_600_000));
        let deleting = db.capacity.as_ref().is_some_and(|e| e.ended_at.is_none());
        if recent || (max > 0 && estimate > max) {
            out.push(SystemCondition {
                rule_id: ids::DB_CAPACITY,
                subject: "database".into(),
                severity: Severity::Warning,
                title: if deleting {
                    "資料庫已達總容量上限，正在提早刪除最舊的資料".into()
                } else if recent {
                    "最近 24 小時內曾因總容量上限提早刪除最舊的資料".into()
                } else {
                    "依目前回報中的主機估算，保留期滿的用量將超過資料庫總容量上限".into()
                },
                code: recent.then_some("KRS-W4202"),
                details: json!({
                    "max_total_bytes": max,
                    "estimate_bytes": estimate,
                    "deleting": deleting,
                }),
            });
        }
        if db.guard_paused {
            out.push(SystemCondition {
                rule_id: ids::RETENTION_PAUSED,
                subject: "retention".into(),
                severity: Severity::Warning,
                title: "系統日期異常，暫停清除過期資料（請勿刪除 .retention-guard.json）".into(),
                code: Some("KRS-W4201"),
                details: json!({}),
            });
        }
    }

    fn connection_checks(&mut self, now_ms: i64, rules: &[Rule], out: &mut Vec<SystemCondition>) {
        let (agents, connected) = self.registry.sizes();
        let max_agents = self.registry.max_agents();
        let ratio = rule_param(rules, ids::REGISTRY_CAPACITY, "ratio", 0.8);
        if max_agents > 0 && agents as f64 / max_agents as f64 >= ratio {
            out.push(SystemCondition {
                rule_id: ids::REGISTRY_CAPACITY,
                subject: "registry".into(),
                severity: Severity::Warning,
                title: format!("狀態表已使用 {agents} / {max_agents}（limits.max_agents）"),
                code: Some("KRS-E3201"),
                details: json!({ "agents": agents, "max_agents": max_agents }),
            });
        }
        let max = self.config.server.max_connections;
        let usage = connected as f64 / max.max(1) as f64;
        let warn = rule_param(rules, ids::CONNECTIONS, "warning_ratio", 0.9);
        let crit = rule_param(rules, ids::CONNECTIONS, "critical_ratio", 0.98);
        let window = rule_param(rules, ids::CONNECTIONS, "window_secs", 3600.0) as i64 * 1000;
        let (paused_now, pauses, handshake) = match &self.per_ip {
            Some(p) => {
                let g = p.snapshot(Instant::now()).rejections.global;
                (g.paused_now, g.pauses, g.handshake_limit_rejected)
            }
            None => (false, 0, 0),
        };
        if pauses > self.global.0 || handshake > self.global.1 {
            self.global.2 = Some(now_ms);
        }
        self.global.0 = pauses;
        self.global.1 = handshake;
        let rejected_recently = self.global.2.is_some_and(|t| now_ms - t <= window);
        let sev = if usage >= crit || paused_now {
            Some(Severity::Critical)
        } else if usage >= warn || rejected_recently {
            Some(Severity::Warning)
        } else {
            None
        };
        if let Some(sev) = sev {
            out.push(SystemCondition {
                rule_id: ids::CONNECTIONS,
                subject: "connections".into(),
                severity: sev,
                title: if paused_now {
                    format!("已達同時連線上限 {max}，暫停接受新連線")
                } else {
                    format!(
                        "連線數接近上限：{connected} / {max}（使用率 {:.0}%）",
                        usage * 100.0
                    )
                },
                code: Some("KRS-W2204"),
                details: json!({
                    "connections": connected,
                    "max_connections": max,
                    "usage_ratio": usage,
                    "paused": paused_now,
                    "rejected_last_hour": rejected_recently,
                }),
            });
        }
    }

    fn per_ip_checks(&mut self, now_ms: i64, out: &mut Vec<SystemCondition>) {
        let Some(p) = &self.per_ip else {
            return;
        };
        let snap = p.snapshot(Instant::now()).rejections.per_ip;
        for s in &snap.sources {
            let entry = self.per_source.entry(s.source).or_insert((0, now_ms));
            if s.rejected > entry.0 {
                entry.1 = now_ms;
            }
            entry.0 = s.rejected;
        }
        // 計數是 Kairos 這次啟動以來的累計：最近一次增加在 1 小時內時成立
        for s in &snap.sources {
            let Some((_, at)) = self.per_source.get(&s.source) else {
                continue;
            };
            if now_ms - at > 3_600_000 {
                continue;
            }
            out.push(SystemCondition {
                rule_id: ids::PER_IP_LIMIT,
                subject: s.source.to_string(),
                severity: Severity::Warning,
                title: format!(
                    "同一 IP 連線數超過上限：{} 的連線在握手前被拒（上限 {}）",
                    s.source, s.limit
                ),
                code: Some("KRS-W2201"),
                details: json!({
                    "source": s.source.to_string(),
                    "rejected_since_start": s.rejected,
                    "limit": s.limit,
                    "allowlisted": s.allowlisted,
                }),
            });
        }
        if self.per_source.len() > 4096 {
            self.per_source
                .retain(|_, (_, at)| now_ms - *at <= 3_600_000);
        }
    }

    fn counter_recent(
        &mut self,
        name: &'static str,
        value: u64,
        now_ms: i64,
        window_ms: i64,
    ) -> bool {
        let entry = self.enroll_counts.entry(name).or_insert((0, None));
        if value > entry.0 {
            entry.1 = Some(now_ms);
        }
        entry.0 = value;
        entry.1.is_some_and(|t| now_ms - t <= window_ms)
    }

    fn enroll_checks(
        &mut self,
        svc: &crate::enroll::EnrollService,
        now_ms: i64,
        rules: &[Rule],
        out: &mut Vec<SystemCondition>,
    ) {
        let st = &svc.stats;
        let w_over = rule_param(rules, ids::ENROLL_OVERLOAD, "window_secs", 600.0) as i64 * 1000;
        if self.counter_recent("busy", st.busy.load(Ordering::Relaxed), now_ms, w_over) {
            out.push(SystemCondition {
                rule_id: ids::ENROLL_OVERLOAD,
                subject: "enroll".into(),
                severity: Severity::Critical,
                title: "註冊端點過載或待核准佇列已滿：新的註冊請求暫緩".into(),
                code: Some("KRS-W2205"),
                details: json!({ "busy_since_start": st.busy.load(Ordering::Relaxed) }),
            });
        }
        let w_block = rule_param(rules, ids::ENROLL_BLOCKED, "window_secs", 3600.0) as i64 * 1000;
        if self.counter_recent(
            "blocked",
            st.blocked.load(Ordering::Relaxed),
            now_ms,
            w_block,
        ) {
            out.push(SystemCondition {
                rule_id: ids::ENROLL_BLOCKED,
                subject: "enroll".into(),
                severity: Severity::Warning,
                title: "註冊來源被暫時封鎖：有來源嘗試過多無效的 token 或簽章".into(),
                code: Some("KRS-W3005"),
                details: json!({ "blocked_since_start": st.blocked.load(Ordering::Relaxed) }),
            });
        }
        let w_rate =
            rule_param(rules, ids::ENROLL_RATE_LIMITED, "window_secs", 3600.0) as i64 * 1000;
        let limited =
            st.limited_enroll.load(Ordering::Relaxed) + st.limited_renew.load(Ordering::Relaxed);
        if self.counter_recent("limited", limited, now_ms, w_rate) {
            out.push(SystemCondition {
                rule_id: ids::ENROLL_RATE_LIMITED,
                subject: "enroll".into(),
                severity: Severity::Info,
                title: "註冊端點速率限制：有請求被延後（429）".into(),
                code: Some("KRS-W2202"),
                details: json!({
                    "enroll_since_start": st.limited_enroll.load(Ordering::Relaxed),
                    "renew_since_start": st.limited_renew.load(Ordering::Relaxed),
                }),
            });
        }
        let skewed: Vec<String> = svc
            .clock_rejected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, (_, at))| now_ms - at <= 15 * 60_000)
            .map(|(a, _)| a.clone())
            .collect();
        if !skewed.is_empty() {
            out.push(SystemCondition {
                rule_id: ids::ENROLL_CLOCK_SKEW,
                subject: "enroll".into(),
                severity: Severity::Info,
                title: format!("註冊因時鐘偏差暫不受理：{} 台主機", skewed.len()),
                code: Some("KRS-W8302"),
                details: json!({ "hosts": skewed.len(), "sample": skewed.iter().take(20).collect::<Vec<_>>() }),
            });
        }
        if svc.is_paused() {
            out.push(SystemCondition {
                rule_id: ids::ENROLL_AUTO_PAUSED,
                subject: "enroll".into(),
                severity: Severity::Info,
                title: "註冊端點自動暫停：一段時間沒有有效的註冊 token".into(),
                code: None,
                details: json!({ "paused_since": svc.paused_since() }),
            });
        }
    }
}
