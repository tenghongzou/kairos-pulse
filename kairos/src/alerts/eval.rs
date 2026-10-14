//! 每個 tick 的判定（[`Engine::tick`]）：暖機期、關聯彙總、系統檢查、主機規則、生命週期（重送、升級、
//! 自動結案、抖動、靜音結束）。設計依據見 `engine.rs` 的模組說明與 `kairos/ALERTS.md` 第 5 節。

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::engine::{
    COUNT_STEPS, Engine, FLAP_WINDOW_MS, HostView, REOPEN_WINDOW_MS, Rejection, Signal,
    SystemCondition, WARMUP_RECONNECT_RATIO, World, dedup_key,
};
use super::model::{Attention, Category, EventState, RootCause, Rule, Severity};
use super::rules::{ids, kind};
use super::scope::Compiled;
use crate::bus::DisconnectReason;
use crate::per_ip::cidr::Cidr;

/// 網段與大規模離線只計入這兩種原因（§3.3 C）
fn aggregatable(r: Option<DisconnectReason>) -> bool {
    matches!(
        r,
        Some(DisconnectReason::IdleTimeout | DisconnectReason::AgentClosed) | None
    )
}

fn minutes_text(ms: i64) -> String {
    let m = (ms / 60_000).max(0);
    if m < 60 {
        format!("{m} 分鐘")
    } else if m < 24 * 60 {
        format!("{} 小時 {} 分", m / 60, m % 60)
    } else {
        format!("{} 天", m / (24 * 60))
    }
}

/// 主機事件的內容（通知的範本變數 `host.*`）
pub(super) fn host_details(h: &HostView, prefix_v4: u8, prefix_v6: u8) -> Value {
    json!({
        "hostname": h.name(),
        "ip": h.ip.map(|i| i.to_string()),
        "subnet": h.ip.map(|i| Cidr::containing(i, prefix_v4, prefix_v6).to_string()),
        "tags": h.tags,
        "offline_reason": h.offline_reason.map(DisconnectReason::as_str),
        "offline_reason_label": h.offline_reason.map(DisconnectReason::label),
        "offline_at": h.offline_at,
        "offline_after_ms": h.offline_after_ms,
        "last_seen": h.last_seen,
    })
}

impl Engine {
    /// 一個 tick
    pub fn tick(&mut self, world: World) {
        let now = world.now_ms;
        self.last_hosts = Arc::new(world.hosts);
        let hosts = self.last_hosts.clone();
        self.host_index = hosts
            .iter()
            .enumerate()
            .map(|(i, h)| (h.agent_id.clone(), i))
            .collect();
        self.update_silences(now);
        self.step_warmup(&hosts, now);
        if self.warmup.ended {
            // 大規模離線先判斷：同一輪成立的網段事件直接成為它的子事件
            self.eval_mass_offline(&hosts, now);
            self.eval_subnets(&hosts, now);
        }
        self.eval_system(&world.conditions, &world.checked, now);
        self.eval_host_offline(&hosts, now);
        self.eval_long_offline(&hosts, now);
        self.eval_report_stale(&hosts, now);
        self.eval_metrics(&hosts, now);
        self.eval_identity(&hosts, now);
        self.eval_cert_rejected(&hosts, &world.rejections, now);
        self.eval_mass_decommission(now);
        self.lifecycle(now);
    }

    fn enabled_rules(&self, kind_name: &str) -> Vec<Rule> {
        self.rules
            .iter()
            .filter(|r| r.enabled && r.kind == kind_name)
            .cloned()
            .collect()
    }

    fn enabled_rule(&self, id: &str) -> Option<Rule> {
        self.rules
            .iter()
            .find(|r| r.rule_id == id && r.enabled)
            .cloned()
    }

    /// 進行中（觸發中或已確認）的事件
    pub(super) fn active_event(&self, rule_id: &str, subject: &str) -> Option<String> {
        let id = self.by_dedup.get(&dedup_key(rule_id, subject))?;
        self.events
            .get(id)
            .filter(|e| e.state.is_active())
            .map(|e| e.event_id.clone())
    }

    /// 準位觸發的比對：`signals` 為目前成立的對象；不在其中的進行中事件轉為已恢復（`keep_open` 時不轉）
    pub(super) fn reconcile(
        &mut self,
        rule: &Rule,
        signals: Vec<(String, Signal)>,
        now: i64,
        keep_open: bool,
    ) {
        let mut active = HashSet::new();
        for (subject, sig) in signals {
            active.insert(subject.clone());
            let key = dedup_key(&rule.rule_id, &subject);
            let existing = self.by_dedup.get(&key).cloned();
            match existing
                .and_then(|id| self.events.get(&id).map(|e| (id, e.state, e.recovered_at)))
            {
                Some((id, state, _)) if state.is_active() => self.refresh(&id, sig, now),
                Some((id, EventState::Recovered, rec))
                    if now - rec.unwrap_or(now) <= REOPEN_WINDOW_MS =>
                {
                    self.reopen(&id, now);
                    self.refresh(&id, sig, now);
                }
                _ => {
                    if sig.episode.is_some() && self.suppressed.get(&key) == sig.episode.as_ref() {
                        continue;
                    }
                    self.suppressed.remove(&key);
                    self.open(rule, &subject, sig, now);
                }
            }
        }
        if keep_open {
            return;
        }
        let stale: Vec<String> = self
            .by_rule
            .get(&rule.rule_id)
            .into_iter()
            .flatten()
            .filter_map(|id| self.events.get(id))
            .filter(|e| e.state.is_active() && !active.contains(&e.subject))
            .map(|e| e.event_id.clone())
            .collect();
        for id in stale {
            self.recover(&id, now, "condition_cleared");
        }
    }

    /// 進行中事件收到新的訊號：更新內容；嚴重度升高時通知（同一事件逐級升級）；被上層吸收
    fn refresh(&mut self, id: &str, sig: Signal, now: i64) {
        let Some(ev) = self.events.get_mut(id) else {
            return;
        };
        ev.last_signal_at = now;
        // 標題、嚴重度、代碼的變化立即寫回並推送；內容（數值、台數）只合併，最多每 60 秒寫回一次
        let mut changed = false;
        if ev.title != sig.title {
            ev.title = sig.title;
            changed = true;
        }
        if ev.code != sig.code && sig.code.is_some() {
            ev.code = sig.code;
            changed = true;
        }
        let escalated = sig.severity < ev.severity;
        if sig.severity != ev.severity {
            ev.severity = sig.severity;
            changed = true;
        }
        let mut details_changed = false;
        match (&mut ev.details, sig.details) {
            (Value::Object(old), Value::Object(new)) => {
                for (k, v) in new {
                    if old.get(&k) != Some(&v) {
                        old.insert(k, v);
                        details_changed = true;
                    }
                }
            }
            (old, new) => {
                if *old != new {
                    *old = new;
                    details_changed = true;
                }
            }
        }
        let soft = details_changed && !changed && now - ev.updated_at >= 60_000;
        if changed || soft {
            ev.updated_at = now;
        }
        let notified_rank = ev.notify.notified_rank;
        let id = id.to_string();
        if soft {
            self.dirty.insert(id.clone());
        }
        if changed {
            self.touch(&id, true);
        }
        if escalated {
            self.timeline(
                &id,
                now,
                "system",
                "escalated",
                json!({ "severity": sig.severity }),
            );
            if notified_rank.is_none_or(|r| sig.severity.rank() < r) {
                self.notify(&id, "update", now, json!({ "severity_raised": true }));
            }
        }
        if let Some(parent) = sig.parent
            && self.events.get(&id).is_some_and(|e| e.parent_id.is_none())
            && self
                .events
                .get(&parent)
                .is_some_and(|p| p.state.is_active())
        {
            self.adopt(&parent, &id, now);
        }
    }

    // ------------------------------------------------------------------
    // 靜音的開始與結束
    // ------------------------------------------------------------------

    fn update_silences(&mut self, now: i64) {
        let active: HashSet<String> = self
            .silences
            .iter()
            .filter(|(s, _)| s.is_active(now))
            .map(|(s, _)| s.silence_id.clone())
            .collect();
        let ended: Vec<String> = self.active_silences.difference(&active).cloned().collect();
        for sid in ended {
            self.silence_ended(&sid, now);
        }
        self.active_silences = active;
    }

    /// 靜音或維護時段結束時仍有異常：立即發送一則彙整通知（§4.2）
    fn silence_ended(&mut self, sid: &str, now: i64) {
        let Some((s, scope)) = self
            .silences
            .iter()
            .find(|(s, _)| s.silence_id == sid)
            .map(|(s, c)| (s.clone(), c.clone()))
        else {
            return;
        };
        if !s.notify_on_end || s.end_notified {
            return;
        }
        let mut still: Vec<(String, String, Vec<String>)> = Vec::new();
        for ev in self.events.values() {
            if !ev.state.is_active()
                || ev.parent_id.is_some()
                || ev.attention != Attention::Action
                || ev.category == Category::System
            {
                continue;
            }
            if !s.rules.is_empty() && !s.rules.contains(&ev.rule_id) {
                continue;
            }
            let host = ev
                .agent_id
                .as_deref()
                .and_then(|a| self.last_hosts.iter().find(|h| &*h.agent_id == a));
            let hit = match host {
                Some(h) => scope.matches(&h.facts()),
                None => scope.is_all(),
            };
            if hit {
                let channels = self
                    .rules
                    .iter()
                    .find(|r| r.rule_id == ev.rule_id)
                    .map(|r| r.channels.clone())
                    .unwrap_or_default();
                still.push((ev.event_id.clone(), ev.title.clone(), channels));
            }
        }
        self.out.silences_notified.push(sid.to_string());
        if still.is_empty() {
            return;
        }
        let mut channels: Vec<String> = still
            .iter()
            .flat_map(|(_, _, c)| c.clone())
            .filter(|c| self.channels.get(c).is_some_and(|ch| ch.enabled))
            .collect();
        channels.sort();
        channels.dedup();
        let sample: Vec<Value> = still
            .iter()
            .take(super::engine::MAX_LISTED)
            .map(|(id, title, _)| json!({ "event_id": id, "title": title }))
            .collect();
        let payload = json!({
            "kind": "silence_end",
            "silence": {
                "silence_id": s.silence_id,
                "name": s.name,
                "kind": s.kind,
                "starts_at": s.starts_at,
                "ends_at": s.effective_end(),
            },
            "count": still.len(),
            "sample": sample,
            "link": self.cfg.console_url.trim().trim_end_matches('/').to_string(),
        });
        for c in channels {
            self.out.deliveries.push(super::engine::NewDelivery {
                event_id: None,
                channel_id: c,
                kind: "silence_end",
                urgent: false,
                payload: payload.clone(),
                group: None,
                created_at: now,
            });
        }
    }

    // ------------------------------------------------------------------
    // 暖機期（§3.3 B）
    // ------------------------------------------------------------------

    fn step_warmup(&mut self, hosts: &[HostView], now: i64) {
        if self.warmup.ended {
            return;
        }
        let online: HashSet<&str> = hosts
            .iter()
            .filter(|h| h.online)
            .map(|h| &*h.agent_id)
            .collect();
        let decommissioned: HashSet<&str> = hosts
            .iter()
            .filter(|h| h.decommissioned)
            .map(|h| &*h.agent_id)
            .collect();
        let expected: Vec<String> = self
            .warmup
            .expected
            .iter()
            .filter(|a| !decommissioned.contains(a.as_str()))
            .cloned()
            .collect();
        let back = expected
            .iter()
            .filter(|a| online.contains(a.as_str()))
            .count();
        let ratio = if expected.is_empty() {
            1.0
        } else {
            back as f64 / expected.len() as f64
        };
        if now < self.warmup.until_ms && ratio < WARMUP_RECONNECT_RATIO {
            return;
        }
        self.warmup.ended = true;
        let missing: HashSet<String> = expected
            .iter()
            .filter(|a| !online.contains(a.as_str()))
            .cloned()
            .collect();
        let expected_n = expected.len();
        log::info!(
            "告警暖機期結束：重新啟動前在線上的 {} 台主機中有 {} 台已重新連上",
            expected_n,
            back
        );
        if missing.is_empty() {
            return;
        }
        if let Some(rule) = self.enabled_rule(ids::RESTART_UNRECOVERED) {
            let n = missing.len();
            let subject = format!("restart:{}", self.started_ms);
            self.reconcile(
                &rule,
                vec![(
                    subject,
                    Signal {
                        severity: rule.severity,
                        title: format!("Kairos 重新啟動後有 {n} 台主機未恢復連線"),
                        code: None,
                        agent_id: None,
                        details: json!({ "count": n, "expected": expected_n }),
                        episode: Some(self.started_ms),
                        parent: None,
                    },
                )],
                now,
                true,
            );
        }
        self.warmup.missing = missing;
    }

    // ------------------------------------------------------------------
    // 關聯彙總（§3.3 C）
    // ------------------------------------------------------------------

    fn in_window(&self, h: &HostView, now: i64, window_ms: i64) -> bool {
        !h.online
            && !h.decommissioned
            && aggregatable(h.offline_reason)
            && h.offline_at
                .is_some_and(|t| t >= (now - window_ms).max(self.started_ms))
    }

    fn eval_subnets(&mut self, hosts: &[HostView], now: i64) {
        let Some(rule) = self.enabled_rule(ids::SUBNET_OFFLINE) else {
            return;
        };
        let window = rule.param_u64("window_secs", 120) as i64 * 1000;
        let min_hosts = rule.param_u64("min_hosts", 5) as usize;
        let ratio = rule.param_f64("min_ratio", 0.3);
        let mut groups: HashMap<Cidr, (usize, Vec<&HostView>)> = HashMap::new();
        for h in hosts.iter().filter(|h| !h.decommissioned) {
            let Some(ip) = h.ip else {
                continue;
            };
            let subnet = Cidr::containing(ip, self.cfg.prefix_v4, self.cfg.prefix_v6);
            let g = groups.entry(subnet).or_default();
            if h.online {
                g.0 += 1;
            } else if self.in_window(h, now, window) {
                g.1.push(h);
            }
        }
        // 時間窗內成立的大規模離線事件：新的網段事件成為它的子事件
        let mass = self.active_event(ids::MASS_OFFLINE, "all").filter(|m| {
            let start = self.events[m].started_at;
            now - start <= 30 * 60_000
        });
        let mut signals = Vec::new();
        for (subnet, (online, off)) in &groups {
            let subject = subnet.to_string();
            let open = self.active_event(&rule.rule_id, &subject).is_some();
            let n = off.len();
            let triggered = n >= min_hosts && n as f64 / (online + n) as f64 >= ratio;
            if !triggered && !open {
                continue;
            }
            let total = online + n;
            let sample: Vec<&str> = off.iter().take(3).map(|h| h.name()).collect();
            let title = if open {
                // 台數以子事件為準（lifecycle 更新）；這裡保留原標題
                self.events
                    .get(
                        &self
                            .active_event(&rule.rule_id, &subject)
                            .unwrap_or_default(),
                    )
                    .map(|e| e.title.clone())
                    .unwrap_or_default()
            } else {
                format!("網段事件：{subject} 有 {n} 台主機同時離線")
            };
            signals.push((
                subject.clone(),
                Signal {
                    severity: rule.severity,
                    title,
                    code: None,
                    agent_id: None,
                    details: json!({
                        "subnet": subject,
                        "window_offline": n,
                        "online": online,
                        "total": total,
                        "sample": sample,
                    }),
                    episode: None,
                    parent: mass.clone(),
                },
            ));
        }
        // 開著的網段事件但網段已沒有主機（全部除役等）：仍保留，由子事件判斷恢復
        self.reconcile(&rule, signals, now, true);
        self.recover_childless(&rule.rule_id, now);
    }

    /// 彙總事件沒有任何子事件、且觸發條件已不成立時轉為已恢復
    fn recover_childless(&mut self, rule_id: &str, now: i64) {
        let ids: Vec<String> = self
            .by_rule
            .get(rule_id)
            .into_iter()
            .flatten()
            .filter_map(|id| self.events.get(id))
            .filter(|e| e.state.is_active())
            .filter(|e| now - e.last_signal_at > 0)
            .map(|e| e.event_id.clone())
            .collect();
        for id in ids {
            if self.member_counts(&id).0 == 0 && now - self.events[&id].started_at > 60_000 {
                self.recover(&id, now, "condition_cleared");
            }
        }
    }

    fn eval_mass_offline(&mut self, hosts: &[HostView], now: i64) {
        let Some(rule) = self.enabled_rule(ids::MASS_OFFLINE) else {
            return;
        };
        let window = rule.param_u64("window_secs", 120) as i64 * 1000;
        let min_hosts = rule.param_u64("min_hosts", 10) as usize;
        let ratio = rule.param_f64("min_ratio", 0.5);
        let online = hosts
            .iter()
            .filter(|h| h.online && !h.decommissioned)
            .count();
        let off = hosts
            .iter()
            .filter(|h| self.in_window(h, now, window))
            .count();
        let subject = "all".to_string();
        let open = self.active_event(&rule.rule_id, &subject);
        let triggered = off >= min_hosts && off as f64 / (online + off) as f64 >= ratio;
        if !triggered && open.is_none() {
            return;
        }
        let title = match &open {
            Some(id) => self.events[id].title.clone(),
            None => format!(
                "大規模離線：已有 {off} 台主機在 {} 內離線（可能是 Kairos 伺服器端網路中斷）",
                minutes_text(window)
            ),
        };
        self.reconcile(
            &rule,
            vec![(
                subject.clone(),
                Signal {
                    severity: rule.severity,
                    title,
                    code: None,
                    agent_id: None,
                    details: json!({ "window_offline": off, "online": online }),
                    episode: None,
                    parent: None,
                },
            )],
            now,
            true,
        );
        // 時間窗內成立的網段事件成為子事件
        if let Some(mass) = self.active_event(&rule.rule_id, &subject) {
            let start = self.events[&mass].started_at;
            let join = 30 * 60_000;
            let subnets: Vec<String> = self
                .by_rule
                .get(ids::SUBNET_OFFLINE)
                .into_iter()
                .flatten()
                .filter_map(|id| self.events.get(id))
                .filter(|e| {
                    e.state.is_active()
                        && e.parent_id.is_none()
                        && e.started_at >= start - window
                        && e.started_at <= start + join
                })
                .map(|e| e.event_id.clone())
                .collect();
            for s in subnets {
                self.adopt(&mass, &s, now);
            }
        }
        self.recover_childless(&rule.rule_id, now);
    }

    /// 主機離線事件的父事件（系統事件 → 彙總事件，§3.3 A、C）
    fn find_parent(&self, h: &HostView, now: i64) -> Option<String> {
        let reason = h.offline_reason;
        if matches!(
            reason,
            Some(DisconnectReason::CertificateRejected | DisconnectReason::RevocationUnavailable)
        ) && let Some(id) = self.active_event(ids::CRL_EXPIRED, "crl")
        {
            return Some(id);
        }
        if self.warmup.missing.contains(&*h.agent_id)
            && let Some(id) = self.active_event(
                ids::RESTART_UNRECOVERED,
                &format!("restart:{}", self.started_ms),
            )
        {
            return Some(id);
        }
        let off = h.offline_at.unwrap_or(now);
        if aggregatable(reason) {
            if let Some(ip) = h.ip {
                let subnet =
                    Cidr::containing(ip, self.cfg.prefix_v4, self.cfg.prefix_v6).to_string();
                if let Some(id) = self.active_event(ids::SUBNET_OFFLINE, &subnet) {
                    let rule = self.rule(ids::SUBNET_OFFLINE);
                    let window =
                        rule.map_or(120, |r| r.param_u64("window_secs", 120)) as i64 * 1000;
                    let join = rule.map_or(1800, |r| r.param_u64("join_secs", 1800)) as i64 * 1000;
                    let start = self.events[&id].started_at;
                    if off >= start - window && off <= start + join {
                        return Some(id);
                    }
                }
                if let Some(id) = self.active_event(ids::PER_IP_LIMIT, &ip.to_string()) {
                    return Some(id);
                }
            }
            if let Some(id) = self.active_event(ids::MASS_OFFLINE, "all") {
                let start = self.events[&id].started_at;
                if off >= start - 120_000 && off <= start + 30 * 60_000 {
                    return Some(id);
                }
            }
            if let Some(id) = self.active_event(ids::CONNECTIONS, "connections")
                && self.events[&id]
                    .details
                    .get("paused")
                    .and_then(Value::as_bool)
                    == Some(true)
            {
                return Some(id);
            }
        }
        None
    }

    // ------------------------------------------------------------------
    // 系統檢查
    // ------------------------------------------------------------------

    /// 引擎自己掌握的系統檢查：TLS 重新載入失敗、狀態表淘汰、通知管道失敗
    fn internal_conditions(&mut self, now: i64) -> Vec<SystemCondition> {
        let mut out = Vec::new();
        if let Some((at, crl, message)) = &self.tls_failed {
            out.push(SystemCondition {
                rule_id: ids::TLS_RELOAD_FAILED,
                subject: "tls".into(),
                severity: Severity::Critical,
                title: if *crl {
                    "TLS 設定重新載入失敗：新的撤銷可能沒有生效".into()
                } else {
                    "TLS 設定重新載入失敗，沿用舊的設定".into()
                },
                code: crl.then_some("KRS-E1204"),
                details: json!({ "since": at, "message": message }),
            });
        }
        let window = self
            .rule(ids::REGISTRY_CAPACITY)
            .map_or(3600, |r| r.param_u64("window_secs", 3600)) as i64
            * 1000;
        while self.evictions.front().is_some_and(|t| now - t > window) {
            self.evictions.pop_front();
        }
        if !self.evictions.is_empty() {
            out.push(SystemCondition {
                rule_id: ids::REGISTRY_CAPACITY,
                subject: "registry".into(),
                severity: Severity::Warning,
                title: format!(
                    "狀態表已滿，過去 1 小時淘汰了 {} 台離線最久的主機",
                    self.evictions.len()
                ),
                code: Some("KRS-E3201"),
                details: json!({ "evicted_1h": self.evictions.len() }),
            });
        }
        let failures = self
            .rule(ids::CHANNEL_FAILED)
            .map_or(3, |r| r.param_u64("failures", 3)) as u32;
        for c in self.channels.values() {
            if c.enabled && (c.failed() || c.consecutive_failures >= failures) {
                out.push(SystemCondition {
                    rule_id: ids::CHANNEL_FAILED,
                    subject: c.channel_id.clone(),
                    severity: Severity::Critical,
                    title: format!(
                        "通知管道失敗：{}（連續 {} 次發送失敗）",
                        c.name, c.consecutive_failures
                    ),
                    code: Some("KRS-E5107"),
                    details: json!({
                        "channel_id": c.channel_id,
                        "channel_name": c.name,
                        "consecutive_failures": c.consecutive_failures,
                        "last_error_code": c.last_error_code,
                        "failed_since": c.failed_since,
                    }),
                });
            }
        }
        out
    }

    fn eval_system(&mut self, external: &[SystemCondition], checked: &[&'static str], now: i64) {
        let mut conditions: Vec<SystemCondition> = external.to_vec();
        conditions.extend(self.internal_conditions(now));
        let internal = [ids::TLS_RELOAD_FAILED, ids::CHANNEL_FAILED];
        // 同一規則、同一對象取最嚴重的一項
        let mut by_rule: HashMap<&'static str, HashMap<String, SystemCondition>> = HashMap::new();
        for c in conditions {
            let slot = by_rule.entry(c.rule_id).or_default();
            match slot.get(&c.subject) {
                Some(old) if old.severity <= c.severity => {}
                _ => {
                    slot.insert(c.subject.clone(), c);
                }
            }
        }
        for rule in self.enabled_rules(kind::SYSTEM) {
            let id: &str = &rule.rule_id;
            let covered = checked.contains(&id)
                || internal.contains(&id)
                || (id == ids::REGISTRY_CAPACITY && !self.evictions.is_empty());
            if !covered {
                continue;
            }
            let signals: Vec<(String, Signal)> = by_rule
                .remove(id)
                .unwrap_or_default()
                .into_values()
                .map(|c| {
                    // 使用者可以調高系統規則的嚴重度，不能調低（§2.1.1）
                    let severity = c.severity.min(rule.severity);
                    (
                        c.subject,
                        Signal {
                            severity,
                            title: c.title,
                            code: c.code.map(str::to_string),
                            agent_id: None,
                            details: c.details,
                            episode: None,
                            parent: None,
                        },
                    )
                })
                .collect();
            self.reconcile(&rule, signals, now, false);
        }
    }

    // ------------------------------------------------------------------
    // 主機規則
    // ------------------------------------------------------------------

    fn eval_host_offline(&mut self, hosts: &[HostView], now: i64) {
        for rule in self.enabled_rules(kind::HOST_OFFLINE) {
            let grace = rule.param_u64("grace_secs", 180) as i64 * 1000;
            let scope = self
                .compiled
                .get(&rule.rule_id)
                .cloned()
                .unwrap_or_default();
            let mut signals = Vec::new();
            for h in hosts {
                if h.online || h.decommissioned || !scope.matches(&h.facts()) {
                    continue;
                }
                let Some(off) = h.offline_at else {
                    continue;
                };
                if h.offline_reason == Some(DisconnectReason::ServerShutdown) {
                    continue;
                }
                let open = self.active_event(&rule.rule_id, &h.agent_id).is_some();
                let missing = self.warmup.missing.contains(&*h.agent_id);
                // 暖機期內不開新的主機離線事件；Kairos 啟動前就已離線的主機不開新事件（重新啟動與升級
                // 不灌出通知，§7「計畫性重啟造成的主機離線通知 0 則」；長期失聯另有規則）
                if !open && (!self.warmup.ended || (off < self.started_ms && !missing)) {
                    continue;
                }
                let parent = self.find_parent(h, now);
                if !open && parent.is_none() && now - off < grace {
                    continue;
                }
                let name = h.name().to_string();
                signals.push((
                    h.agent_id.to_string(),
                    Signal {
                        severity: rule.severity,
                        title: format!("主機離線：{name}"),
                        code: None,
                        agent_id: Some(h.agent_id.to_string()),
                        details: host_details(h, self.cfg.prefix_v4, self.cfg.prefix_v6),
                        episode: Some(off),
                        parent,
                    },
                ));
            }
            self.reconcile(&rule, signals, now, false);
        }
    }

    fn eval_long_offline(&mut self, hosts: &[HostView], now: i64) {
        for rule in self.enabled_rules(kind::HOST_LONG_OFFLINE) {
            let after = rule.param_u64("after_secs", 86_400) as i64 * 1000;
            let scope = self
                .compiled
                .get(&rule.rule_id)
                .cloned()
                .unwrap_or_default();
            let mut signals = Vec::new();
            for h in hosts {
                if h.online || h.decommissioned || !scope.matches(&h.facts()) {
                    continue;
                }
                let off = h.offline_at.unwrap_or(h.last_seen);
                let open = self.active_event(&rule.rule_id, &h.agent_id).is_some();
                if now - off < after || (!open && !self.warmup.ended) {
                    continue;
                }
                signals.push((
                    h.agent_id.to_string(),
                    Signal {
                        severity: rule.severity,
                        title: format!(
                            "主機長期失聯：{}（已離線 {}）",
                            h.name(),
                            minutes_text(now - off)
                        ),
                        code: None,
                        agent_id: Some(h.agent_id.to_string()),
                        details: host_details(h, self.cfg.prefix_v4, self.cfg.prefix_v6),
                        episode: Some(off),
                        parent: None,
                    },
                ));
            }
            self.reconcile(&rule, signals, now, false);
        }
    }

    fn eval_report_stale(&mut self, hosts: &[HostView], now: i64) {
        // 回報無法寫入期間 last_report 自然不會更新：抑制回報延遲（§3.3 A）
        let writes_failing = self
            .active_event(ids::REPORT_WRITES, "report_writes")
            .is_some();
        for rule in self.enabled_rules(kind::REPORT_STALE) {
            let factor = rule.param_u64("factor", 3) as i64;
            let scope = self
                .compiled
                .get(&rule.rule_id)
                .cloned()
                .unwrap_or_default();
            let mut signals = Vec::new();
            for h in hosts {
                if writes_failing || !h.online || !h.seen_since_start || !scope.matches(&h.facts())
                {
                    continue;
                }
                let (Some(interval), Some(connected)) = (h.report_interval_ms, h.connected_at)
                else {
                    continue;
                };
                let base = h.last_report_at.unwrap_or(connected).max(connected);
                let limit = factor * interval as i64;
                if now - base <= limit {
                    continue;
                }
                signals.push((
                    h.agent_id.to_string(),
                    Signal {
                        severity: rule.severity,
                        title: format!(
                            "回報延遲：{}（{} 沒有新的 report）",
                            h.name(),
                            minutes_text(now - base)
                        ),
                        code: None,
                        agent_id: Some(h.agent_id.to_string()),
                        details: json!({
                            "hostname": h.name(),
                            "ip": h.ip.map(|i| i.to_string()),
                            "last_report_at": h.last_report_at,
                            "report_interval_ms": interval,
                        }),
                        episode: Some(base),
                        parent: None,
                    },
                ));
            }
            // 寫入失敗期間不轉為已恢復（之後一併重新判斷）
            self.reconcile(&rule, signals, now, writes_failing);
        }
    }

    fn eval_metrics(&mut self, hosts: &[HostView], now: i64) {
        let rules = self.enabled_rules(kind::METRIC_THRESHOLD);
        let mut seen = HashSet::new();
        for rule in rules {
            let metric = rule
                .params
                .get("metric")
                .and_then(Value::as_str)
                .unwrap_or("cpu")
                .to_string();
            let op = rule
                .params
                .get("op")
                .and_then(Value::as_str)
                .unwrap_or(">")
                .to_string();
            let threshold = rule.param_f64("threshold", 90.0);
            let for_ms = rule.param_u64("for_secs", 600) as i64 * 1000;
            let clear = rule.params.get("clear_threshold").and_then(Value::as_f64);
            let clear_for = rule.param_u64("clear_for_secs", 0) as i64 * 1000;
            let scope: Compiled = self
                .compiled
                .get(&rule.rule_id)
                .cloned()
                .unwrap_or_default();
            let cmp = |v: f64, t: f64| match op.as_str() {
                ">" => v > t,
                ">=" => v >= t,
                "<" => v < t,
                _ => v <= t,
            };
            let mut signals = Vec::new();
            for h in hosts {
                if h.decommissioned || !scope.matches(&h.facts()) {
                    continue;
                }
                let key = dedup_key(&rule.rule_id, &h.agent_id);
                seen.insert(key.clone());
                let open = self.active_event(&rule.rule_id, &h.agent_id).is_some();
                let value = if h.online {
                    if metric == "memory" { h.memory } else { h.cpu }
                } else {
                    None
                };
                let streak = self.streaks.entry(key).or_default();
                if let Some(v) = value {
                    if cmp(v, threshold) {
                        streak.since.get_or_insert(now);
                        streak.clear_since = None;
                    } else {
                        streak.since = None;
                        // 恢復條件：有恢復門檻時「越過恢復門檻」，沒有時「觸發條件不成立」
                        let cleared = match clear {
                            Some(c) => {
                                if op.starts_with('>') {
                                    v < c
                                } else {
                                    v > c
                                }
                            }
                            None => true,
                        };
                        if cleared {
                            streak.clear_since.get_or_insert(now);
                        } else {
                            streak.clear_since = None;
                        }
                    }
                }
                let streak = *streak;
                let active = if open {
                    !streak.clear_since.is_some_and(|c| now - c >= clear_for)
                } else {
                    streak.since.is_some_and(|s| now - s >= for_ms)
                };
                if !active {
                    continue;
                }
                let label = if metric == "memory" {
                    "記憶體"
                } else {
                    "CPU"
                };
                signals.push((
                    h.agent_id.to_string(),
                    Signal {
                        severity: rule.severity,
                        title: format!(
                            "{} {label} {op} {threshold}% 持續 {}：{}",
                            rule.name,
                            minutes_text(for_ms),
                            h.name()
                        ),
                        code: None,
                        agent_id: Some(h.agent_id.to_string()),
                        details: json!({
                            "hostname": h.name(),
                            "ip": h.ip.map(|i| i.to_string()),
                            "metric": metric,
                            "op": op,
                            "threshold": threshold,
                            "value": value,
                            "since": streak.since,
                        }),
                        episode: streak.since,
                        parent: None,
                    },
                ));
            }
            self.reconcile(&rule, signals, now, false);
        }
        self.streaks.retain(|k, _| seen.contains(k));
    }

    fn eval_identity(&mut self, hosts: &[HostView], now: i64) {
        let rules = self.enabled_rules(kind::IDENTITY_CONFLICT);
        let max_window = rules
            .iter()
            .map(|r| r.param_u64("window_secs", 3600))
            .max()
            .unwrap_or(3600) as i64
            * 1000;
        for q in self.superseded.values_mut() {
            while q.front().is_some_and(|(t, _)| now - t > max_window) {
                q.pop_front();
            }
        }
        self.superseded.retain(|_, q| !q.is_empty());
        let by_id: HashMap<&str, &HostView> = hosts.iter().map(|h| (&*h.agent_id, h)).collect();
        for rule in rules {
            let window = rule.param_u64("window_secs", 3600) as i64 * 1000;
            let min_count = rule.param_u64("min_count", 5) as usize;
            let min_sources = rule.param_u64("min_sources", 2) as usize;
            let scope = self
                .compiled
                .get(&rule.rule_id)
                .cloned()
                .unwrap_or_default();
            let mut signals = Vec::new();
            for (agent, q) in &self.superseded {
                let host = by_id.get(agent.as_str());
                if host.is_some_and(|h| !scope.matches(&h.facts()))
                    || (host.is_none() && !scope.is_all())
                {
                    continue;
                }
                let recent: Vec<&(i64, Option<std::net::IpAddr>)> =
                    q.iter().filter(|(t, _)| now - t <= window).collect();
                let sources: HashSet<_> = recent.iter().filter_map(|(_, ip)| *ip).collect();
                if recent.len() < min_count || sources.len() < min_sources {
                    continue;
                }
                let name = host.map_or(agent.as_str(), |h| h.name());
                let mut ips: Vec<String> = sources.iter().map(ToString::to_string).collect();
                ips.sort();
                signals.push((
                    agent.clone(),
                    Signal {
                        severity: rule.severity,
                        title: format!(
                            "疑似 agent_id 重複：{name}（{} 分鐘內被取代 {} 次）",
                            window / 60_000,
                            recent.len()
                        ),
                        code: Some("KRS-W3102".into()),
                        agent_id: Some(agent.clone()),
                        details: json!({
                            "hostname": name,
                            "superseded": recent.len(),
                            "sources": ips,
                        }),
                        episode: None,
                        parent: None,
                    },
                ));
            }
            self.reconcile(&rule, signals, now, false);
        }
    }

    fn eval_cert_rejected(&mut self, hosts: &[HostView], rejections: &[Rejection], now: i64) {
        let Some(rule) = self.enabled_rule(ids::CERT_REJECTED) else {
            return;
        };
        let by_id: HashMap<&str, &HostView> = hosts.iter().map(|h| (&*h.agent_id, h)).collect();
        let mut items: Vec<Rejection> = rejections.to_vec();
        for (agent, (at, ip)) in &self.auth_failed {
            items.push(Rejection {
                agent_id: agent.clone(),
                at_ms: *at,
                reason: "auth_failed".into(),
                code: "KRS-W1102",
                serial: None,
                ip: Some(*ip),
                admin_revoked: false,
            });
        }
        self.auth_failed
            .retain(|_, (at, _)| now - *at <= 24 * 3_600_000);
        let mut latest: HashMap<String, Rejection> = HashMap::new();
        for r in items {
            match latest.get(&r.agent_id) {
                Some(old) if old.at_ms >= r.at_ms => {}
                _ => {
                    latest.insert(r.agent_id.clone(), r);
                }
            }
        }
        let mut signals = Vec::new();
        for (agent, r) in latest {
            // 只歸屬到清冊中的主機（auth_failed 的 agent_id 由連線方自稱）
            let Some(h) = by_id.get(agent.as_str()) else {
                continue;
            };
            if h.decommissioned
                || r.admin_revoked
                || r.reason == "unknown_issuer"
                || h.last_seen >= r.at_ms
                || now - r.at_ms > 24 * 3_600_000
            {
                continue;
            }
            let parent = (r.reason == "revocation_status_unavailable")
                .then(|| self.active_event(ids::CRL_EXPIRED, "crl"))
                .flatten();
            let why = match r.reason.as_str() {
                "revoked" => "憑證已撤銷",
                "expired" => "憑證已過期",
                "revocation_status_unavailable" => "伺服器無法判定撤銷狀態",
                "auth_failed" => "agent_id 與憑證不符",
                _ => "憑證不被接受",
            };
            signals.push((
                agent.clone(),
                Signal {
                    severity: rule.severity,
                    title: format!("憑證被拒：{}（{why}）", h.name()),
                    code: Some(r.code.to_string()),
                    agent_id: Some(agent.clone()),
                    details: json!({
                        "hostname": h.name(),
                        "reason": r.reason,
                        "serial": r.serial,
                        "ip": r.ip.map(|i| i.to_string()),
                        "rejected_at": r.at_ms,
                    }),
                    episode: Some(h.last_seen),
                    parent,
                },
            ));
        }
        self.reconcile(&rule, signals, now, false);
    }

    fn eval_mass_decommission(&mut self, now: i64) {
        let Some(rule) = self.enabled_rule(ids::MASS_DECOMMISSION) else {
            return;
        };
        let window = rule.param_u64("window_secs", 600) as i64 * 1000;
        let min_hosts = rule.param_u64("min_hosts", 20) as usize;
        let quiet = rule.param_u64("quiet_secs", 3600) as i64 * 1000;
        while self
            .decommissions
            .front()
            .is_some_and(|(t, _)| now - t > window.max(quiet) * 2)
        {
            self.decommissions.pop_front();
        }
        let subject = "self".to_string();
        let open = self.active_event(&rule.rule_id, &subject);
        let recent = self
            .decommissions
            .iter()
            .filter(|(t, _)| now - t <= window)
            .count();
        let last = self.decommissions.back().map(|(t, _)| *t);
        let active = match &open {
            // 同一事件持續吸收新的自行除役；60 分鐘沒有新的除役時恢復
            Some(id) => last.is_some_and(|t| now - t < quiet) || self.events[id].started_at == now,
            None => recent >= min_hosts,
        };
        let mut signals = Vec::new();
        if active {
            let since = open
                .as_ref()
                .map_or(now - window, |id| self.events[id].started_at - window);
            let count = self
                .decommissions
                .iter()
                .filter(|(t, _)| *t >= since)
                .count();
            let sample: Vec<&str> = self
                .decommissions
                .iter()
                .filter(|(t, _)| *t >= since)
                .take(20)
                .map(|(_, a)| a.as_str())
                .collect();
            signals.push((
                subject,
                Signal {
                    severity: rule.severity,
                    title: format!("短時間大量除役：{count} 台主機自行除役"),
                    code: Some("KRS-W3108".into()),
                    agent_id: None,
                    details: json!({ "count": count, "sample": sample }),
                    episode: None,
                    parent: None,
                },
            ));
        }
        self.reconcile(&rule, signals, now, false);
    }

    // ------------------------------------------------------------------
    // 生命週期：靜音標記、自動結案、重送、升級、抖動、彙總的台數
    // ------------------------------------------------------------------

    fn lifecycle(&mut self, now: i64) {
        let ids: Vec<String> = self.events.keys().cloned().collect();
        for id in ids {
            let Some(ev) = self.events.get(&id) else {
                continue;
            };
            if ev.state == EventState::Resolved {
                continue;
            }
            let (silenced, maintenance, _) = self.silence_match(ev, now);
            if ev.silenced != silenced || ev.in_maintenance != maintenance {
                let ev = self.events.get_mut(&id).expect("事件存在");
                ev.silenced = silenced;
                ev.in_maintenance = maintenance;
                self.touch(&id, true);
            }
            let ev = &self.events[&id];
            // 自動結案（§5.1）：未確認 24 小時（緊急除外）、已確認 7 天
            if ev.state == EventState::Recovered {
                let rec = ev.recovered_at.unwrap_or(now);
                if !ev.acked
                    && ev.severity != Severity::Emergency
                    && now - rec >= super::engine::AUTO_CLOSE_UNACKED_MS
                {
                    self.close(&id, now, RootCause::SelfRecovered, "system", None);
                    continue;
                }
                if ev.acked && now - rec >= super::engine::AUTO_CLOSE_ACKED_MS {
                    self.close(
                        &id,
                        now,
                        RootCause::AutoClosed,
                        "system",
                        Some("已恢復且已確認，7 天未處理而自動結案".into()),
                    );
                    continue;
                }
            }
            let ev = &self.events[&id];
            // 抖動：30 分鐘沒有狀態轉換時解除；期間每 30 分鐘一則摘要
            if ev.flapping {
                let recent = self.flaps.get(&ev.dedup_key).map_or(0, |q| {
                    q.iter().filter(|t| now - **t <= FLAP_WINDOW_MS).count()
                });
                if recent == 0 {
                    let ev = self.events.get_mut(&id).expect("事件存在");
                    ev.flapping = false;
                    self.timeline(&id, now, "system", "stable", json!({}));
                    self.touch(&id, true);
                } else if ev
                    .notify
                    .flap_summary_at
                    .is_none_or(|t| now - t >= FLAP_WINDOW_MS)
                    && ev.notify.notified
                {
                    self.notify(&id, "flapping", now, json!({ "transitions_30m": recent }));
                }
            }
            let ev = &self.events[&id];
            if ev.state.is_active() && ev.attention == Attention::Action && ev.parent_id.is_none() {
                self.repeat_and_escalate(&id, now);
            }
            let ev = &self.events[&id];
            // 預計處理時間到期仍未恢復：提醒一次（記在處理紀錄）
            if ev.state.is_active()
                && ev.acked
                && ev.eta_at.is_some_and(|t| t <= now)
                && !ev.notify.eta_reminded
            {
                let ev = self.events.get_mut(&id).expect("事件存在");
                ev.notify.eta_reminded = true;
                let who = ev.assignee.clone();
                self.timeline(
                    &id,
                    now,
                    "system",
                    "eta_expired",
                    json!({ "assignee": who }),
                );
                self.dirty.insert(id.clone());
            }
            let ev = &self.events[&id];
            // 網段事件：成立 60 秒後仍是頂層（沒有被大規模離線吸收）才通知
            if ev.kind == kind::SUBNET_OFFLINE
                && ev.state.is_active()
                && ev.parent_id.is_none()
                && !ev.notify.notified
                && now - ev.started_at >= super::engine::AGGREGATE_HOLD_MS
            {
                self.notify(&id, "firing", now, json!({}));
            }
            let ev = &self.events[&id];
            if ev.category == Category::Aggregate && ev.state.is_active() {
                self.aggregate_counts(&id, now);
            }
        }
        // 彙總事件的恢復（子事件可能在這一輪恢復）
        let parents: Vec<String> = self
            .events
            .values()
            .filter(|e| e.category == Category::Aggregate && e.state.is_active())
            .map(|e| e.event_id.clone())
            .collect();
        for p in parents {
            self.check_parent(&p, now);
        }
        // 抖動紀錄的清理
        self.flaps.retain(|_, q| {
            while q.front().is_some_and(|t| now - t > FLAP_WINDOW_MS) {
                q.pop_front();
            }
            !q.is_empty()
        });
    }

    fn repeat_and_escalate(&mut self, id: &str, now: i64) {
        let ev = &self.events[id];
        if ev.acked || ev.silenced || ev.flapping || ev.snooze_until.is_some_and(|t| t > now) {
            return;
        }
        let Some(rule) = self.rule_of(id) else {
            return;
        };
        let ev = &self.events[id];
        let eff = rule.notify.effective(ev.severity);
        // 不可靜音的提醒（未認證連線）：每日一次、不限次數
        let remind = rule.params.get("remind_secs").and_then(Value::as_u64);
        let (interval, max) = match remind {
            Some(r) => (Some(r), u32::MAX),
            None => (eff.repeat_interval_secs, eff.repeat_max),
        };
        if ev.notify.notified
            && let Some(i) = interval
            && ev.notify.sends <= max
            && ev
                .notify
                .last_sent_at
                .is_some_and(|t| now - t >= i as i64 * 1000)
        {
            let n = ev.notify.sends;
            self.notify(id, "repeat", now, json!({ "repeat": n }));
        }
        let ev = &self.events[id];
        if !ev.notify.escalated
            && let Some(after) = eff.escalate_after_secs
            && now - ev.started_at >= after as i64 * 1000
        {
            self.notify(id, "escalation", now, json!({ "after_secs": after }));
        }
    }

    /// 彙總事件的台數：寫入內容，跨越 10、50、100、500 時發送一則更新
    fn aggregate_counts(&mut self, id: &str, now: i64) {
        let (total, recovered) = self.member_counts(id);
        let ev = &self.events[id];
        let old_total = ev.details.get("hosts").and_then(Value::as_u64);
        let old_rec = ev.details.get("recovered_hosts").and_then(Value::as_u64);
        if old_total != Some(total as u64) || old_rec != Some(recovered as u64) {
            self.set_detail(id, "hosts", json!(total));
            self.set_detail(id, "recovered_hosts", json!(recovered));
            let ev = self.events.get_mut(id).expect("事件存在");
            if ev.kind == kind::SUBNET_OFFLINE {
                ev.title = format!("網段事件：{} 有 {} 台主機同時離線", ev.subject, total);
            } else if ev.kind == kind::RESTART_UNRECOVERED {
                ev.title = format!("Kairos 重新啟動後有 {} 台主機未恢復連線", total - recovered);
            } else if ev.kind == kind::MASS_OFFLINE {
                ev.title = format!("大規模離線：受影響主機 {total} 台");
            }
            ev.updated_at = now;
            self.touch(id, true);
        }
        let ev = &self.events[id];
        let step = COUNT_STEPS
            .iter()
            .rev()
            .find(|s| total as u64 >= **s)
            .copied()
            .unwrap_or(0);
        if step > ev.notify.count_step && ev.notify.notified {
            let ev = self.events.get_mut(id).expect("事件存在");
            ev.notify.count_step = step;
            self.notify(id, "update", now, json!({ "hosts": total }));
        } else if step > ev.notify.count_step {
            let ev = self.events.get_mut(id).expect("事件存在");
            ev.notify.count_step = step;
            self.dirty.insert(id.to_string());
        }
    }
}
