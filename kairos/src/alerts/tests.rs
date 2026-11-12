//! 告警引擎的單元測試：規則判定、去重與重新開啟、抖動、關聯彙總、暖機期、靜音（範圍與系統事件）、
//! 通知的決定（重送、升級、確認、恢復）、處理類型、手動解決後同一段異常不再開啟。

use chrono::FixedOffset;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;

use super::engine::{
    Engine, EngineConfig, HostView, Loaded, Rejection, SystemCondition, Warmup, World,
};
use super::model::{
    Attention, Channel, ChannelKind, EventState, NotifyPolicy, RootCause, Rule, Scope, Severity,
    Silence, SilenceKind,
};
use super::rules::{self, ids};
use crate::bus::{BusEvent, DisconnectReason, EventKind};

const T0: i64 = 1_760_000_000_000;
const MIN: i64 = 60_000;

fn channel(id: &str, kind: ChannelKind) -> Channel {
    Channel {
        channel_id: id.into(),
        name: format!("ch-{id}"),
        kind,
        enabled: true,
        config: json!({}),
        secrets: json!({}),
        state: "ok".into(),
        consecutive_failures: 0,
        failed_since: None,
        last_attempt_at: None,
        last_success_at: None,
        last_error_code: None,
        last_error: None,
        created_at: 0,
        created_by: String::new(),
        updated_at: 0,
        updated_by: String::new(),
    }
}

/// 所有系統預設規則都送到 webhook 管道 `w`（緊急升級到 `boss`）
fn rules_with_channels() -> Vec<Rule> {
    rules::builtins()
        .into_iter()
        .map(|mut r| {
            r.channels = vec!["w".into(), "mail".into()];
            r.notify = NotifyPolicy {
                escalate_channels: vec!["boss".into()],
                ..NotifyPolicy::default()
            };
            r
        })
        .collect()
}

fn engine_with(rules: Vec<Rule>, silences: Vec<Silence>, expected: &[&str], warm: bool) -> Engine {
    let loaded = Loaded {
        rules,
        silences,
        channels: vec![
            channel("w", ChannelKind::Webhook),
            channel("mail", ChannelKind::Email),
            channel("boss", ChannelKind::Email),
        ],
        ..Loaded::default()
    };
    Engine::new(
        EngineConfig {
            offset: FixedOffset::east_opt(8 * 3600).unwrap(),
            prefix_v4: 24,
            prefix_v6: 64,
            console_url: "https://m.example".into(),
        },
        T0,
        Warmup {
            until_ms: if warm { T0 } else { T0 + 10 * MIN },
            expected: expected.iter().map(|s| s.to_string()).collect(),
            ended: false,
            missing: HashSet::new(),
        },
        loaded,
    )
}

fn engine() -> Engine {
    engine_with(rules_with_channels(), Vec::new(), &[], true)
}

fn host(id: &str, ip: &str) -> HostView {
    HostView {
        agent_id: Arc::from(id),
        hostname: Arc::from(format!("h-{id}")),
        display_name: None,
        tags: vec![],
        ip: ip.parse().ok(),
        online: true,
        offline_at: None,
        offline_reason: None,
        seen_since_start: true,
        decommissioned: false,
        connected_at: Some(T0),
        last_seen: T0,
        offline_after_ms: 25_000,
        report_interval_ms: None,
        last_report_at: None,
        cpu: None,
        memory: None,
        detail: None,
        logical_cores: None,
    }
}

fn offline(mut h: HostView, at: i64, reason: DisconnectReason) -> HostView {
    h.online = false;
    h.offline_at = Some(at);
    h.offline_reason = Some(reason);
    h.last_seen = at;
    h
}

fn world(now: i64, hosts: Vec<HostView>) -> World {
    World {
        now_ms: now,
        hosts,
        ..World::default()
    }
}

/// 進行中（非已解決）的事件：(規則, 對象, 狀態)
fn open(e: &Engine) -> Vec<(String, String, &'static str)> {
    let mut v: Vec<_> = e
        .open_events()
        .map(|x| (x.rule_id.clone(), x.subject.clone(), x.state.as_str()))
        .collect();
    v.sort();
    v
}

fn by_rule(e: &Engine, rule: &str) -> Vec<super::model::Event> {
    let mut v: Vec<_> = e
        .open_events()
        .filter(|x| x.rule_id == rule)
        .cloned()
        .collect();
    v.sort_by(|a, b| a.event_id.cmp(&b.event_id));
    v
}

#[test]
fn host_offline_fires_after_the_grace_period_and_recovers() {
    let mut e = engine();
    let a = host("a", "10.0.0.1");
    e.tick(world(T0, vec![a.clone()]));
    assert!(open(&e).is_empty(), "{:?}", open(&e));
    let off = offline(a.clone(), T0 + MIN, DisconnectReason::IdleTimeout);
    // 寬限 3 分鐘內不觸發
    e.tick(world(T0 + 3 * MIN, vec![off.clone()]));
    assert!(by_rule(&e, ids::HOST_OFFLINE).is_empty());
    e.tick(world(T0 + 4 * MIN + 1000, vec![off.clone()]));
    let evs = by_rule(&e, ids::HOST_OFFLINE);
    assert_eq!(evs.len(), 1);
    let ev = evs[0].clone();
    assert_eq!(ev.severity, Severity::Critical);
    assert_eq!(ev.state, EventState::Firing);
    assert!(ev.event_id.starts_with("E-20251009-"), "{}", ev.event_id);
    assert_eq!(ev.details["offline_reason"], "idle_timeout");
    let out = e.take_output();
    // 嚴重：送 webhook 與 email（警告才只送 email）
    let firing: Vec<_> = out
        .deliveries
        .iter()
        .filter(|d| d.kind == "firing")
        .collect();
    assert_eq!(firing.len(), 2, "{:?}", out.deliveries);
    assert!(out.changes.iter().any(|c| c.kind == "alert.created"));
    // 恢復：已通知過，嚴重預設發送恢復通知
    e.tick(world(T0 + 6 * MIN, vec![a.clone()]));
    let ev = e.event(&ev.event_id).unwrap().clone();
    assert_eq!(ev.state, EventState::Recovered);
    let out = e.take_output();
    assert_eq!(
        out.deliveries
            .iter()
            .filter(|d| d.kind == "recovery")
            .count(),
        2
    );
    // 已恢復、未確認：24 小時後自動結案（自行恢復）
    let id = ev.event_id.clone();
    e.tick(world(T0 + 6 * MIN + 24 * 60 * MIN, vec![a]));
    let out = e.take_output();
    let closed = out.events.iter().find(|x| x.event_id == id).unwrap();
    assert_eq!(closed.state, EventState::Resolved);
    assert_eq!(closed.root_cause, Some(RootCause::SelfRecovered));
    assert!(e.event(&id).is_none(), "已解決的事件移出記憶體");
}

#[test]
fn shutdown_and_hosts_offline_before_start_do_not_fire() {
    let mut e = engine();
    let s = offline(
        host("s", "10.0.0.2"),
        T0 + MIN,
        DisconnectReason::ServerShutdown,
    );
    // 啟動前就已離線（清冊載回）：不開主機離線事件
    let old = offline(
        host("o", "10.0.0.3"),
        T0 - 60 * MIN,
        DisconnectReason::AgentClosed,
    );
    e.tick(world(T0 + 30 * MIN, vec![s, old.clone()]));
    assert!(by_rule(&e, ids::HOST_OFFLINE).is_empty(), "{:?}", open(&e));
    // 長期失聯（24 小時）照常
    e.tick(world(T0 + 24 * 60 * MIN, vec![old]));
    let long = by_rule(&e, ids::HOST_LONG_OFFLINE);
    assert_eq!(long.len(), 1);
    assert_eq!(long[0].severity, Severity::Warning);
    let out = e.take_output();
    // 警告只送 Email
    let ch: Vec<&str> = out
        .deliveries
        .iter()
        .filter(|d| d.event_id.as_deref() == Some(long[0].event_id.as_str()))
        .map(|d| d.channel_id.as_str())
        .collect();
    assert_eq!(ch, ["mail"]);
}

#[test]
fn warmup_suppresses_offline_and_merges_unrecovered_hosts() {
    let mut e = engine_with(rules_with_channels(), Vec::new(), &["a", "b", "c"], false);
    // 重新啟動：b、c 尚未連上（由清冊載回、這次啟動後沒有連線）
    let a = host("a", "10.0.0.1");
    let mut b = offline(
        host("b", "10.0.1.2"),
        T0 - 2 * MIN,
        DisconnectReason::ServerShutdown,
    );
    b.seen_since_start = false;
    b.offline_reason = None;
    let mut c = b.clone();
    c.agent_id = Arc::from("c");
    c.ip = "10.0.2.3".parse().ok();
    // 暖機期內：沒有任何主機離線事件
    e.tick(world(T0 + 5 * MIN, vec![a.clone(), b.clone(), c.clone()]));
    assert!(by_rule(&e, ids::HOST_OFFLINE).is_empty());
    assert!(!e.warmup().ended);
    // 暖機期結束（10 分鐘）：b、c 合併成一則「重新啟動後未恢復連線」
    e.tick(world(T0 + 10 * MIN, vec![a.clone(), b.clone(), c.clone()]));
    assert!(e.warmup().ended);
    let parents = by_rule(&e, ids::RESTART_UNRECOVERED);
    assert_eq!(parents.len(), 1);
    let pid = parents[0].event_id.clone();
    let kids = by_rule(&e, ids::HOST_OFFLINE);
    assert_eq!(kids.len(), 2);
    assert!(
        kids.iter()
            .all(|k| k.parent_id.as_deref() == Some(pid.as_str()))
    );
    let out = e.take_output();
    // 只有父事件通知（子事件不通知）
    let firing: HashSet<_> = out
        .deliveries
        .iter()
        .filter(|d| d.kind == "firing")
        .filter_map(|d| d.event_id.clone())
        .collect();
    assert_eq!(firing, HashSet::from([pid.clone()]));
    // b、c 都恢復 → 父事件恢復
    let mut b2 = host("b", "10.0.1.2");
    b2.seen_since_start = true;
    let mut c2 = host("c", "10.0.2.3");
    c2.seen_since_start = true;
    e.tick(world(T0 + 15 * MIN, vec![a, b2, c2]));
    assert_eq!(e.event(&pid).unwrap().state, EventState::Recovered);
}

#[test]
fn warmup_ends_early_when_98_percent_reconnect() {
    let mut e = engine_with(rules_with_channels(), Vec::new(), &["a", "b"], false);
    e.tick(world(
        T0 + MIN,
        vec![host("a", "10.0.0.1"), host("b", "10.0.0.2")],
    ));
    assert!(e.warmup().ended);
    assert!(by_rule(&e, ids::RESTART_UNRECOVERED).is_empty());
}

#[test]
fn subnet_aggregation_absorbs_hosts_and_notifies_once() {
    let mut e = engine();
    let hosts: Vec<HostView> = (1..=10)
        .map(|i| host(&format!("h{i}"), &format!("10.30.3.{i}")))
        .collect();
    let other = host("x", "10.40.0.1");
    let mut all = hosts.clone();
    all.push(other.clone());
    e.tick(world(T0, all.clone()));
    // 6 台在 40 秒內逾時離線
    let mut now_hosts: Vec<HostView> = hosts
        .iter()
        .enumerate()
        .map(|(i, h)| {
            if i < 6 {
                offline(
                    h.clone(),
                    T0 + MIN + i as i64 * 5000,
                    DisconnectReason::IdleTimeout,
                )
            } else {
                h.clone()
            }
        })
        .collect();
    now_hosts.push(other.clone());
    e.tick(world(T0 + MIN + 40_000, now_hosts.clone()));
    let subnet = by_rule(&e, ids::SUBNET_OFFLINE);
    assert_eq!(subnet.len(), 1, "{:?}", open(&e));
    assert_eq!(subnet[0].subject, "10.30.3.0/24");
    assert_eq!(subnet[0].severity, Severity::Emergency);
    let pid = subnet[0].event_id.clone();
    let kids = by_rule(&e, ids::HOST_OFFLINE);
    assert_eq!(kids.len(), 6);
    assert!(
        kids.iter()
            .all(|k| k.parent_id.as_deref() == Some(pid.as_str()))
    );
    // 先彙總再通知：成立當下不通知（60 秒內可能被大規模離線吸收）
    let out = e.take_output();
    assert!(!out.deliveries.iter().any(|d| d.kind == "firing"));
    e.tick(world(T0 + 2 * MIN + 40_000, now_hosts.clone()));
    let out = e.take_output();
    let firing: Vec<_> = out
        .deliveries
        .iter()
        .filter(|d| d.kind == "firing")
        .collect();
    assert_eq!(firing.len(), 2);
    assert!(
        firing
            .iter()
            .all(|d| d.event_id.as_deref() == Some(pid.as_str()))
    );
    // 緊急的第一則：不排隊
    assert!(firing.iter().all(|d| d.urgent));
    // 寬限期到了也不另外通知
    e.tick(world(T0 + 6 * MIN, now_hosts.clone()));
    let out = e.take_output();
    assert!(
        !out.deliveries.iter().any(|d| d.kind == "firing"),
        "{:?}",
        out.deliveries
    );
    // 5 台恢復（83%，未達 90%）→ 父事件仍觸發中；第 6 台恢復 → 父事件恢復
    let mut back = now_hosts.clone();
    for h in back.iter_mut().take(5) {
        let id = h.agent_id.clone();
        *h = host(&id, &h.ip.unwrap().to_string());
    }
    e.tick(world(T0 + 8 * MIN, back.clone()));
    assert!(e.event(&pid).unwrap().state.is_active());
    let all_back: Vec<HostView> = back
        .iter()
        .map(|h| host(&h.agent_id, &h.ip.unwrap().to_string()))
        .collect();
    e.tick(world(T0 + 9 * MIN, all_back));
    assert_eq!(e.event(&pid).unwrap().state, EventState::Recovered);
}

#[test]
fn parent_recovery_detaches_unrecovered_children() {
    let mut e = engine();
    let hosts: Vec<HostView> = (1..=20)
        .map(|i| host(&format!("h{i}"), &format!("10.30.4.{i}")))
        .collect();
    e.tick(world(T0, hosts.clone()));
    let down: Vec<HostView> = hosts
        .iter()
        .map(|h| offline(h.clone(), T0 + MIN, DisconnectReason::AgentClosed))
        .collect();
    e.tick(world(T0 + MIN + 10_000, down));
    let pid = by_rule(&e, ids::SUBNET_OFFLINE)[0].event_id.clone();
    e.take_output();
    // 19 台恢復（95%）：父事件恢復，h20 脫離成為獨立事件並通知（合併成一組）
    let mut back: Vec<HostView> = hosts.iter().take(19).cloned().collect();
    back.push(offline(
        hosts[19].clone(),
        T0 + MIN,
        DisconnectReason::AgentClosed,
    ));
    e.tick(world(T0 + 20 * MIN, back));
    assert_eq!(e.event(&pid).unwrap().state, EventState::Recovered);
    let left = by_rule(&e, ids::HOST_OFFLINE)
        .into_iter()
        .filter(|x| x.state.is_active())
        .collect::<Vec<_>>();
    assert_eq!(left.len(), 1);
    assert!(left[0].parent_id.is_none());
    let out = e.take_output();
    let detached: Vec<_> = out
        .deliveries
        .iter()
        .filter(|d| d.kind == "detached")
        .collect();
    assert!(!detached.is_empty());
    assert!(
        detached
            .iter()
            .all(|d| d.group.as_deref() == Some(&format!("detach:{pid}") as &str))
    );
}

#[test]
fn mass_offline_across_subnets() {
    let mut e = engine();
    let hosts: Vec<HostView> = (0..30)
        .map(|i| host(&format!("m{i}"), &format!("10.50.{}.{}", i % 6, i + 1)))
        .collect();
    e.tick(world(T0, hosts.clone()));
    let down: Vec<HostView> = hosts
        .iter()
        .map(|h| offline(h.clone(), T0 + MIN, DisconnectReason::IdleTimeout))
        .collect();
    e.tick(world(T0 + MIN + 30_000, down));
    let mass = by_rule(&e, ids::MASS_OFFLINE);
    assert_eq!(mass.len(), 1);
    let mid = mass[0].event_id.clone();
    let (total, _) = e.member_counts(&mid);
    assert_eq!(total, 30);
    // 主機事件都在彙總之下：只有大規模離線通知
    let out = e.take_output();
    let firing: HashSet<_> = out
        .deliveries
        .iter()
        .filter(|d| d.kind == "firing")
        .filter_map(|d| d.event_id.clone())
        .collect();
    assert_eq!(firing, HashSet::from([mid]));
}

#[test]
fn reopen_within_ten_minutes_and_flapping_suppresses_notifications() {
    let mut e = engine();
    let a = host("a", "10.0.0.1");
    e.tick(world(T0, vec![a.clone()]));
    let mut t = T0 + MIN;
    let down = |t: i64| offline(host("a", "10.0.0.1"), t, DisconnectReason::IdleTimeout);
    e.tick(world(t + 4 * MIN, vec![down(t)]));
    let id = by_rule(&e, ids::HOST_OFFLINE)[0].event_id.clone();
    // 恢復後 5 分鐘內再離線（超過寬限）→ 同一事件重新開啟
    t += 5 * MIN;
    e.tick(world(t, vec![a.clone()]));
    e.tick(world(t + 4 * MIN, vec![down(t)]));
    let ev = e.event(&id).unwrap();
    assert_eq!(ev.state, EventState::Firing);
    assert_eq!(ev.reopen_count, 1);
    e.take_output();
    // 持續上下線：30 分鐘內 4 次以上轉換 → 不穩定，之後不再逐次通知
    for _ in 0..3 {
        t += 5 * MIN;
        e.tick(world(t, vec![a.clone()]));
        e.tick(world(t + 4 * MIN, vec![down(t)]));
    }
    let ev = e.event(&id).unwrap();
    assert!(ev.flapping);
    let out = e.take_output();
    assert!(
        !out.deliveries
            .iter()
            .any(|d| d.kind == "reopen" || d.kind == "recovery"),
        "{:?}",
        out.deliveries.iter().map(|d| d.kind).collect::<Vec<_>>()
    );
}

#[test]
fn silences_suppress_notifications_but_not_records_and_never_system_events_by_host_scope() {
    let now = T0;
    let silence = |kind: SilenceKind, rules: Vec<String>, scope: Scope| Silence {
        silence_id: format!("s-{}", rules.len()),
        kind,
        name: "維護".into(),
        scope,
        rules,
        starts_at: now - MIN,
        ends_at: now + 60 * MIN,
        reason: "r".into(),
        notify_on_end: true,
        exclude_uptime: false,
        from_event: None,
        created_at: now,
        created_by: "t".into(),
        updated_at: now,
        updated_by: "t".into(),
        ended_at: None,
        ended_by: None,
        end_reason: None,
        end_notified: false,
    };
    let host_scope = Scope {
        subnets: vec!["10.0.0.0/24".into()],
        ..Scope::default()
    };
    let mut e = engine_with(
        rules_with_channels(),
        vec![silence(SilenceKind::Maintenance, vec![], host_scope)],
        &[],
        true,
    );
    let a = host("a", "10.0.0.1");
    let b = host("b", "10.9.0.1");
    e.tick(world(now, vec![a.clone(), b.clone()]));
    let crl = SystemCondition {
        rule_id: ids::CRL_EXPIRING,
        subject: "crl:x:0".into(),
        severity: Severity::Critical,
        title: "CRL 將於 3 天後到期".into(),
        code: Some("KRS-W1201"),
        details: json!({}),
    };
    let w = |t: i64, hosts: Vec<HostView>| World {
        now_ms: t,
        hosts,
        conditions: vec![crl.clone()],
        rejections: vec![],
        checked: vec![ids::CRL_EXPIRING],
    };
    e.tick(w(
        now + 5 * MIN,
        vec![
            offline(a.clone(), now + MIN, DisconnectReason::IdleTimeout),
            offline(b.clone(), now + MIN, DisconnectReason::IdleTimeout),
        ],
    ));
    let ha: Vec<_> = by_rule(&e, ids::HOST_OFFLINE);
    assert_eq!(ha.len(), 2, "事件照常記錄");
    let silenced: Vec<_> = ha.iter().filter(|x| x.silenced).collect();
    assert_eq!(silenced.len(), 1);
    assert_eq!(silenced[0].agent_id.as_deref(), Some("a"));
    assert!(silenced[0].in_maintenance);
    let out = e.take_output();
    let notified: HashSet<_> = out
        .deliveries
        .iter()
        .filter(|d| d.kind == "firing")
        .filter_map(|d| d.event_id.clone())
        .collect();
    // 維護時段內的 a 不通知；b 與系統事件（不受維護時段影響）通知
    let crl_id = by_rule(&e, ids::CRL_EXPIRING)[0].event_id.clone();
    assert!(notified.contains(&crl_id));
    assert!(!notified.contains(&silenced[0].event_id));
    assert_eq!(notified.len(), 2);
    // 維護時段結束：a 仍離線 → 一則「結束時仍有異常」
    e.tick(w(
        now + 61 * MIN,
        vec![
            offline(a.clone(), now + MIN, DisconnectReason::IdleTimeout),
            offline(b.clone(), now + MIN, DisconnectReason::IdleTimeout),
        ],
    ));
    let out = e.take_output();
    assert!(
        out.deliveries
            .iter()
            .any(|d| d.kind == "silence_end" && d.payload["count"] == 1)
    );
    assert_eq!(out.silences_notified, vec!["s-0".to_string()]);

    // 系統事件只有在臨時靜音明確選取該規則時才會被靜音
    let mut e2 = engine_with(
        rules_with_channels(),
        vec![silence(SilenceKind::Silence, vec![], Scope::default())],
        &[],
        true,
    );
    e2.tick(w(now, vec![]));
    assert!(!by_rule(&e2, ids::CRL_EXPIRING)[0].silenced);
    let mut e3 = engine_with(
        rules_with_channels(),
        vec![silence(
            SilenceKind::Silence,
            vec![ids::CRL_EXPIRING.into()],
            Scope::default(),
        )],
        &[],
        true,
    );
    e3.tick(w(now, vec![]));
    assert!(by_rule(&e3, ids::CRL_EXPIRING)[0].silenced);
    let mut e4 = engine_with(
        rules_with_channels(),
        vec![silence(
            SilenceKind::Maintenance,
            vec![ids::CRL_EXPIRING.into()],
            Scope::default(),
        )],
        &[],
        true,
    );
    e4.tick(w(now, vec![]));
    assert!(
        !by_rule(&e4, ids::CRL_EXPIRING)[0].silenced,
        "維護時段不抑制系統事件"
    );
}

#[test]
fn system_condition_escalates_the_same_event_and_recovers() {
    let mut e = engine();
    let cond = |sev| SystemCondition {
        rule_id: ids::CRL_EXPIRING,
        subject: "crl:a:0".into(),
        severity: sev,
        title: "CRL 即將到期".into(),
        code: Some("KRS-W1201"),
        details: json!({}),
    };
    let w = |t, c: Vec<SystemCondition>| World {
        now_ms: t,
        conditions: c,
        checked: vec![ids::CRL_EXPIRING],
        ..World::default()
    };
    e.tick(w(T0, vec![cond(Severity::Warning)]));
    let id = by_rule(&e, ids::CRL_EXPIRING)[0].event_id.clone();
    e.take_output();
    e.tick(w(T0 + 60 * MIN, vec![cond(Severity::Critical)]));
    e.tick(w(T0 + 120 * MIN, vec![cond(Severity::Emergency)]));
    assert_eq!(by_rule(&e, ids::CRL_EXPIRING).len(), 1, "同一事件逐級升級");
    assert_eq!(e.event(&id).unwrap().severity, Severity::Emergency);
    let out = e.take_output();
    assert_eq!(
        out.deliveries.iter().filter(|d| d.kind == "update").count(),
        4,
        "警告→嚴重、嚴重→緊急各通知一次（兩個管道）"
    );
    // 沒有涵蓋這條規則的一輪：維持
    e.tick(World {
        now_ms: T0 + 121 * MIN,
        ..World::default()
    });
    assert!(e.event(&id).unwrap().state.is_active());
    // 條件解除
    e.tick(w(T0 + 122 * MIN, vec![]));
    assert_eq!(e.event(&id).unwrap().state, EventState::Recovered);
}

#[test]
fn metric_threshold_needs_the_duration_and_uses_hysteresis() {
    let mut r = rules::builtin_default(ids::HOST_OFFLINE).unwrap();
    r.rule_id = "r-cpu".into();
    r.builtin = false;
    r.kind = rules::kind::METRIC_THRESHOLD.into();
    r.name = "CPU 偏高".into();
    r.severity = Severity::Warning;
    r.params = json!({"metric": "cpu", "op": ">", "threshold": 90.0, "for_secs": 600, "clear_threshold": 80.0, "clear_for_secs": 0});
    r.channels = vec!["mail".into()];
    let mut rules = rules_with_channels();
    rules.push(r);
    let mut e = engine_with(rules, vec![], &[], true);
    let cpu = |v: f64| {
        let mut h = host("a", "10.0.0.1");
        h.cpu = Some(v);
        h
    };
    e.tick(world(T0, vec![cpu(95.0)]));
    e.tick(world(T0 + 5 * MIN, vec![cpu(95.0)]));
    assert!(by_rule(&e, "r-cpu").is_empty());
    e.tick(world(T0 + 10 * MIN, vec![cpu(96.0)]));
    let ev = by_rule(&e, "r-cpu");
    assert_eq!(ev.len(), 1);
    let id = ev[0].event_id.clone();
    // 85：低於觸發門檻但高於恢復門檻 → 仍觸發中
    e.tick(world(T0 + 11 * MIN, vec![cpu(85.0)]));
    assert!(e.event(&id).unwrap().state.is_active());
    // 70：低於恢復門檻 → 恢復
    e.tick(world(T0 + 12 * MIN, vec![cpu(70.0)]));
    assert_eq!(e.event(&id).unwrap().state, EventState::Recovered);
    // 中途中斷的持續不算
    let mut e = engine_with(
        {
            let mut v = rules_with_channels();
            let mut r = rules::builtin_default(ids::HOST_OFFLINE).unwrap();
            r.rule_id = "r2".into();
            r.kind = rules::kind::METRIC_THRESHOLD.into();
            r.params = json!({"metric": "memory", "op": ">=", "threshold": 50.0, "for_secs": 600});
            v.push(r);
            v
        },
        vec![],
        &[],
        true,
    );
    let mem = |v: f64| {
        let mut h = host("a", "10.0.0.1");
        h.memory = Some(v);
        h
    };
    e.tick(world(T0, vec![mem(60.0)]));
    e.tick(world(T0 + 5 * MIN, vec![mem(10.0)]));
    e.tick(world(T0 + 11 * MIN, vec![mem(60.0)]));
    assert!(by_rule(&e, "r2").is_empty());
}

#[test]
fn repeats_escalation_and_ack() {
    let mut e = engine();
    let a = host("a", "10.0.0.1");
    e.tick(world(T0, vec![a.clone()]));
    let down = offline(a.clone(), T0, DisconnectReason::IdleTimeout);
    e.tick(world(T0 + 4 * MIN, vec![down.clone()]));
    let id = by_rule(&e, ids::HOST_OFFLINE)[0].event_id.clone();
    e.take_output();
    // 嚴重：每 60 分鐘重送、最多 3 次；60 分鐘未確認升級
    let mut repeats = 0;
    let mut escalations = 0;
    for i in 1..=5 {
        e.tick(world(
            T0 + 4 * MIN + i * 60 * MIN + 1000,
            vec![down.clone()],
        ));
        let out = e.take_output();
        repeats += out
            .deliveries
            .iter()
            .filter(|d| d.kind == "repeat" && d.channel_id == "w")
            .count();
        escalations += out
            .deliveries
            .iter()
            .filter(|d| d.kind == "escalation")
            .count();
    }
    assert_eq!(repeats, 3);
    assert_eq!(escalations, 1);
    // 確認：之後不再重送；已通知過 → 送出「已確認」
    let mut e = engine();
    e.tick(world(T0, vec![a.clone()]));
    e.tick(world(T0 + 4 * MIN, vec![down.clone()]));
    let id2 = by_rule(&e, ids::HOST_OFFLINE)[0].event_id.clone();
    e.take_output();
    assert!(
        e.ack(&id2, "token:abc", true, Some(30), T0 + 5 * MIN)
            .unwrap()
    );
    assert!(
        !e.ack(&id2, "token:abc", false, None, T0 + 5 * MIN).unwrap(),
        "重複確認不是錯誤"
    );
    let ev = e.event(&id2).unwrap();
    assert_eq!(ev.state, EventState::Acknowledged);
    assert_eq!(ev.assignee.as_deref(), Some("token:abc"));
    let out = e.take_output();
    assert!(out.deliveries.iter().any(|d| d.kind == "ack"));
    assert!(out.timeline.iter().any(|t| t.action == "acknowledged"));
    e.tick(world(T0 + 3 * 60 * MIN, vec![down.clone()]));
    let out = e.take_output();
    assert!(
        !out.deliveries
            .iter()
            .any(|d| d.kind == "repeat" || d.kind == "escalation")
    );
    let _ = id;
}

#[test]
fn records_and_advisories_are_not_notified_or_acknowledgeable() {
    let mut e = engine();
    let cond = SystemCondition {
        rule_id: ids::ENROLL_RATE_LIMITED,
        subject: "enroll".into(),
        severity: Severity::Info,
        title: "速率限制".into(),
        code: Some("KRS-W2202"),
        details: json!({}),
    };
    e.tick(World {
        now_ms: T0,
        conditions: vec![cond],
        checked: vec![ids::ENROLL_RATE_LIMITED],
        ..World::default()
    });
    let ev = by_rule(&e, ids::ENROLL_RATE_LIMITED)[0].clone();
    assert_eq!(ev.attention, Attention::Record);
    let out = e.take_output();
    assert!(out.deliveries.is_empty());
    let err = e.ack(&ev.event_id, "t", false, None, T0).unwrap_err();
    assert_eq!(err.code, "not_acknowledgeable");
    // 條件解除：直接為已解決（條件解除（自動））
    e.tick(World {
        now_ms: T0 + MIN,
        checked: vec![ids::ENROLL_RATE_LIMITED],
        ..World::default()
    });
    let out = e.take_output();
    let closed = out
        .events
        .iter()
        .find(|x| x.event_id == ev.event_id)
        .unwrap();
    assert_eq!(closed.state, EventState::Resolved);
    assert_eq!(closed.root_cause, Some(RootCause::ConditionCleared));
}

#[test]
fn manual_resolve_does_not_reopen_the_same_episode() {
    let mut e = engine();
    let a = host("a", "10.0.0.1");
    e.tick(world(T0, vec![a.clone()]));
    let down = offline(a.clone(), T0 + MIN, DisconnectReason::IdleTimeout);
    e.tick(world(T0 + 5 * MIN, vec![down.clone()]));
    let id = by_rule(&e, ids::HOST_OFFLINE)[0].event_id.clone();
    assert_eq!(
        e.resolve(
            &id,
            "t",
            RootCause::FalsePositive,
            Some("測試機".into()),
            T0 + 6 * MIN
        )
        .unwrap(),
        0
    );
    let err = e
        .resolve(&id, "t", RootCause::Other, None, T0 + 6 * MIN)
        .unwrap_err();
    assert_eq!(err.code, "invalid_state");
    e.take_output();
    // 同一段離線：不重新開啟
    e.tick(world(T0 + 10 * MIN, vec![down.clone()]));
    assert!(by_rule(&e, ids::HOST_OFFLINE).is_empty());
    // 上線後再次離線（新的一段）：開新事件
    e.tick(world(T0 + 11 * MIN, vec![a.clone()]));
    let down2 = offline(a, T0 + 12 * MIN, DisconnectReason::AgentClosed);
    e.tick(world(T0 + 16 * MIN, vec![down2]));
    let evs = by_rule(&e, ids::HOST_OFFLINE);
    assert_eq!(evs.len(), 1);
    assert_ne!(evs[0].event_id, id);
    // 不合法的根因由 API 層檢查；註解與指派
    assert!(
        e.comment(&evs[0].event_id.clone(), "t", "看過了", T0 + 17 * MIN)
            .is_ok()
    );
    assert!(
        e.comment(&evs[0].event_id.clone(), "t", "  ", T0 + 17 * MIN)
            .is_err()
    );
    assert!(
        e.snooze(&evs[0].event_id.clone(), "t", 4, T0 + 17 * MIN)
            .is_ok()
    );
    assert!(
        e.snooze(&evs[0].event_id.clone(), "t", 48, T0 + 17 * MIN)
            .is_err()
    );
}

#[test]
fn identity_conflict_needs_count_and_sources() {
    let mut e = engine();
    let a = host("a", "10.0.0.1");
    let sup = |i: i64, ip: &str| BusEvent {
        seq: i as u64,
        at: chrono::DateTime::from_timestamp_millis(T0 + i * MIN).unwrap(),
        kind: EventKind::HostSuperseded {
            agent_id: Arc::from("a"),
            previous_ip: None,
            ip: ip.parse().ok(),
        },
    };
    for i in 0..5 {
        e.on_bus(&sup(i, "10.0.0.1"));
    }
    e.tick(world(T0 + 6 * MIN, vec![a.clone()]));
    assert!(
        by_rule(&e, ids::IDENTITY_CONFLICT).is_empty(),
        "只有一個來源"
    );
    e.on_bus(&sup(6, "10.0.0.9"));
    e.tick(world(T0 + 7 * MIN, vec![a.clone()]));
    let ev = by_rule(&e, ids::IDENTITY_CONFLICT);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].code.as_deref(), Some("KRS-W3102"));
    // 時間窗過了：恢復
    e.tick(world(T0 + 7 * MIN + 61 * MIN, vec![a]));
    assert!(!by_rule(&e, ids::IDENTITY_CONFLICT)[0].state.is_active());
}

#[test]
fn cert_rejections_skip_admin_revocations_and_crl_expiry_absorbs_them() {
    let mut e = engine();
    let mut a = offline(
        host("a", "10.0.0.1"),
        T0,
        DisconnectReason::CertificateRejected,
    );
    a.last_seen = T0 - MIN;
    let b = offline(
        host("b", "10.0.0.2"),
        T0,
        DisconnectReason::CertificateRejected,
    );
    let rej = |id: &str, reason: &str, admin: bool| Rejection {
        agent_id: id.into(),
        at_ms: T0 + MIN,
        reason: reason.into(),
        code: "KRS-W1105",
        serial: Some("1a".into()),
        ip: None,
        admin_revoked: admin,
    };
    e.tick(World {
        now_ms: T0 + 2 * MIN,
        hosts: vec![a.clone(), b.clone()],
        rejections: vec![rej("a", "revoked", false), rej("b", "revoked", true)],
        ..World::default()
    });
    let evs = by_rule(&e, ids::CERT_REJECTED);
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].agent_id.as_deref(), Some("a"));
    // CRL 過期：無法判定撤銷狀態而被拒的主機事件成為它的子事件
    let mut e = engine();
    let crl = SystemCondition {
        rule_id: ids::CRL_EXPIRED,
        subject: "crl".into(),
        severity: Severity::Emergency,
        title: "CRL 已過期".into(),
        code: Some("KRS-E1203"),
        details: json!({}),
    };
    e.tick(World {
        now_ms: T0 + 2 * MIN,
        hosts: vec![a],
        conditions: vec![crl],
        rejections: vec![rej("a", "revocation_status_unavailable", false)],
        checked: vec![ids::CRL_EXPIRED],
    });
    let parent = by_rule(&e, ids::CRL_EXPIRED)[0].event_id.clone();
    let kid = by_rule(&e, ids::CERT_REJECTED);
    assert_eq!(kid[0].parent_id.as_deref(), Some(parent.as_str()));
}

#[test]
fn channel_failure_event_goes_to_the_other_channels_only() {
    let mut e = engine();
    let mut chans = vec![
        channel("w", ChannelKind::Webhook),
        channel("mail", ChannelKind::Email),
        channel("boss", ChannelKind::Email),
    ];
    chans[0].state = "failed".into();
    chans[0].consecutive_failures = 3;
    e.set_channels(chans);
    e.tick(world(T0, vec![]));
    let ev = by_rule(&e, ids::CHANNEL_FAILED);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].subject, "w");
    let out = e.take_output();
    let to: HashSet<_> = out
        .deliveries
        .iter()
        .filter(|d| d.kind == "firing")
        .map(|d| d.channel_id.as_str())
        .collect();
    assert_eq!(to, HashSet::from(["mail", "boss"]));
}

#[test]
fn undecryptable_channel_opens_a_channel_failure_with_e5110() {
    let mut e = engine();
    let mut chans = vec![
        channel("w", ChannelKind::Webhook),
        channel("mail", ChannelKind::Email),
    ];
    chans[0].state = super::store::STATE_SECRETS_UNAVAILABLE.into();
    e.set_channels(chans);
    e.tick(world(T0, vec![]));
    let ev = by_rule(&e, ids::CHANNEL_FAILED);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].subject, "w");
    assert_eq!(ev[0].code.as_deref(), Some("KRS-E5110"));
}

#[test]
fn audit_self_check_failure_opens_the_integrity_event_until_it_passes() {
    let mut e = engine();
    let at = |t: i64| chrono::DateTime::from_timestamp_millis(t).unwrap();
    e.on_bus(&BusEvent {
        seq: 1,
        at: at(T0),
        kind: EventKind::AuditIntegrity {
            ok: false,
            reason: Some("hmac_mismatch".into()),
            id: Some(42),
            summary: "第 42 筆：HMAC 不符".into(),
        },
    });
    e.tick(world(T0, vec![]));
    let ev = by_rule(&e, ids::AUDIT_INTEGRITY);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].severity, Severity::Emergency);
    assert_eq!(ev[0].code.as_deref(), Some("KRS-E6201"));
    assert!(ev[0].title.contains("第 42 筆"), "{}", ev[0].title);
    // 下一次驗證通過：恢復
    e.on_bus(&BusEvent {
        seq: 2,
        at: at(T0 + MIN),
        kind: EventKind::AuditIntegrity {
            ok: true,
            reason: None,
            id: None,
            summary: String::new(),
        },
    });
    e.tick(world(T0 + MIN, vec![]));
    assert!(
        by_rule(&e, ids::AUDIT_INTEGRITY)
            .iter()
            .all(|x| x.state != super::model::EventState::Firing)
    );
}

#[test]
fn startup_records_restart_and_unplanned_downtime() {
    let mut e = engine();
    e.record_startup(Some((T0 - 23 * MIN, false)), true, false, T0);
    let out = e.take_output();
    let restarted = out
        .events
        .iter()
        .find(|x| x.rule_id == ids::RESTARTED)
        .unwrap();
    assert_eq!(restarted.severity, Severity::Critical, "上次沒有正常關機");
    let down = out
        .events
        .iter()
        .find(|x| x.rule_id == ids::DOWNTIME)
        .unwrap();
    assert!(down.title.contains("23 分鐘"), "{}", down.title);
    assert!(
        out.events
            .iter()
            .any(|x| x.rule_id == ids::ENROLL_CLOSED_DEFAULT && x.state == EventState::Resolved)
    );
    // 正常關機、維護時段內：資訊、沒有停機事件
    let mut e = engine();
    e.record_startup(Some((T0 - 30 * MIN, true)), false, true, T0);
    let out = e.take_output();
    assert!(out.events.iter().any(|x| x.rule_id == ids::RESTARTED
        && x.severity == Severity::Info
        && x.state == EventState::Resolved));
    assert!(!out.events.iter().any(|x| x.rule_id == ids::DOWNTIME));
}

#[test]
fn cluster_handover_replaces_restart_and_downtime_events() {
    use super::engine::Handover;
    // HA.md 第 13.2 節：上一個 leader 是另一個節點、心跳還很新 → 「leader 已切換」，不開停機事件
    let mut e = engine();
    e.set_handover(Some(Handover {
        from: "kairos-a".into(),
        to: "kairos-b".into(),
        recent: true,
    }));
    e.record_startup(Some((T0 - 6 * MIN, false)), false, false, T0);
    let out = e.take_output();
    let switched = out
        .events
        .iter()
        .find(|x| x.rule_id == ids::RESTARTED)
        .unwrap();
    assert!(
        switched
            .title
            .contains("叢集的 leader 已由 kairos-a 切換到 kairos-b"),
        "{}",
        switched.title
    );
    // 上一個 leader 沒有正常交還：警告（不是「上次沒有正常關機」的嚴重）
    assert_eq!(switched.severity, Severity::Warning);
    assert!(!out.events.iter().any(|x| x.rule_id == ids::DOWNTIME));

    // 正常交還：資訊，直接結案
    let mut e = engine();
    e.set_handover(Some(Handover {
        from: "kairos-a".into(),
        to: "kairos-b".into(),
        recent: true,
    }));
    e.record_startup(Some((T0 - 1000, true)), false, false, T0);
    let out = e.take_output();
    assert!(out.events.iter().any(|x| x.rule_id == ids::RESTARTED
        && x.severity == Severity::Info
        && x.state == EventState::Resolved
        && x.title == "叢集的 leader 已由 kairos-a 切換到 kairos-b"));

    // 上一個 leader 的心跳太舊：「叢集沒有 leader 達 N 分鐘」取代「停機 N 分鐘」
    let mut e = engine();
    e.set_handover(Some(Handover {
        from: "kairos-a".into(),
        to: "kairos-b".into(),
        recent: false,
    }));
    e.record_startup(Some((T0 - 23 * MIN, false)), false, false, T0);
    let out = e.take_output();
    let down = out
        .events
        .iter()
        .find(|x| x.rule_id == ids::DOWNTIME)
        .unwrap();
    assert!(
        down.title.contains("叢集沒有 leader 達 23 分鐘"),
        "{}",
        down.title
    );
}

#[test]
fn report_stale_is_suppressed_while_writes_fail() {
    let mut e = engine();
    let mut a = host("a", "10.0.0.1");
    a.report_interval_ms = Some(5000);
    a.last_report_at = Some(T0);
    let failing = SystemCondition {
        rule_id: ids::REPORT_WRITES,
        subject: "report_writes".into(),
        severity: Severity::Emergency,
        title: "回報無法寫入".into(),
        code: Some("KRS-E4101"),
        details: json!({}),
    };
    e.tick(World {
        now_ms: T0 + MIN,
        hosts: vec![a.clone()],
        conditions: vec![failing],
        checked: vec![ids::REPORT_WRITES],
        rejections: vec![],
    });
    assert!(by_rule(&e, ids::REPORT_STALE).is_empty());
    e.tick(World {
        now_ms: T0 + 2 * MIN,
        hosts: vec![a],
        checked: vec![ids::REPORT_WRITES],
        ..World::default()
    });
    assert_eq!(by_rule(&e, ids::REPORT_STALE).len(), 1);
}

#[test]
fn decommission_records_and_closes_open_host_events() {
    let mut e = engine();
    let a = host("a", "10.0.0.1");
    e.tick(world(T0, vec![a.clone()]));
    e.tick(world(
        T0 + 5 * MIN,
        vec![offline(a, T0 + MIN, DisconnectReason::AgentClosed)],
    ));
    let id = by_rule(&e, ids::HOST_OFFLINE)[0].event_id.clone();
    e.take_output();
    e.on_bus(&BusEvent {
        seq: 1,
        at: chrono::DateTime::from_timestamp_millis(T0 + 6 * MIN).unwrap(),
        kind: EventKind::HostDecommissioned {
            agent_id: "a".into(),
            ip: None,
            initiated_by: Some("msi".into()),
            already: false,
        },
    });
    let out = e.take_output();
    let closed = out.events.iter().find(|x| x.event_id == id).unwrap();
    assert_eq!(closed.root_cause, Some(RootCause::Decommissioned));
    let rec = out
        .events
        .iter()
        .find(|x| x.rule_id == ids::HOST_DECOMMISSIONED)
        .unwrap();
    assert_eq!(rec.attention, Attention::Record);
    assert!(out.deliveries.is_empty());
}

#[test]
fn disabled_rules_stop_and_recover_their_events() {
    let mut e = engine();
    let a = host("a", "10.0.0.1");
    e.tick(world(T0, vec![a.clone()]));
    let down = offline(a, T0 + MIN, DisconnectReason::IdleTimeout);
    e.tick(world(T0 + 5 * MIN, vec![down.clone()]));
    assert_eq!(by_rule(&e, ids::HOST_OFFLINE).len(), 1);
    let rules: Vec<Rule> = e
        .rules()
        .iter()
        .cloned()
        .map(|mut r| {
            if r.rule_id == ids::HOST_OFFLINE {
                r.enabled = false;
            }
            r
        })
        .collect();
    e.set_rules(rules, T0 + 6 * MIN);
    assert!(
        by_rule(&e, ids::HOST_OFFLINE)
            .iter()
            .all(|x| !x.state.is_active())
    );
    e.tick(world(T0 + 7 * MIN, vec![down]));
    assert!(
        by_rule(&e, ids::HOST_OFFLINE)
            .iter()
            .all(|x| !x.state.is_active())
    );
}

// ---- F4：磁碟、網路、置換空間、負載、重新開機（METRICS.md 第 11 節）----

fn metric_rule(id: &str, params: serde_json::Value) -> Rule {
    let mut r = rules::builtin_default(ids::HOST_OFFLINE).unwrap();
    r.rule_id = id.into();
    r.builtin = false;
    r.kind = rules::kind::METRIC_THRESHOLD.into();
    r.name = id.into();
    r.severity = Severity::Warning;
    r.params = params;
    r
}

fn with_detail(mut h: HostView, d: crate::registry::ReportDetail) -> HostView {
    h.detail = Some(Arc::new(crate::registry::LastReport {
        recv_at: "2026-10-20T00:00:00.000Z".into(),
        seq: 1,
        ts: "2026-10-20T00:00:00.000Z".into(),
        cpu_usage: None,
        total_memory: None,
        used_memory: None,
        metrics: Some(10),
        detail: d,
    }));
    h
}

fn disks(usages: &[(&str, f64)]) -> crate::registry::ReportDetail {
    crate::registry::ReportDetail {
        filesystems: usages
            .iter()
            .map(|(m, u)| crate::registry::FsLast {
                mountpoint: m.to_string(),
                usage: Some(*u),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

#[test]
fn disk_threshold_fires_and_recovers_per_mountpoint() {
    let mut rules = rules_with_channels();
    rules.push(metric_rule(
        "r-disk",
        json!({"metric": "disk", "op": ">", "threshold": 90.0, "for_secs": 600, "clear_threshold": 85.0, "mountpoints": ["/", "/data*"]}),
    ));
    let mut e = engine_with(rules, vec![], &[], true);
    let h = |root: f64, data: f64, boot: f64| {
        with_detail(
            host("a", "10.0.0.1"),
            disks(&[("/", root), ("/data1", data), ("/boot", boot)]),
        )
    };
    e.tick(world(T0, vec![h(95.0, 95.0, 99.0)]));
    e.tick(world(T0 + 10 * MIN, vec![h(95.0, 95.0, 99.0)]));
    // 每個掛載點各一則；/boot 不在範圍內
    let subjects: Vec<String> = by_rule(&e, "r-disk")
        .iter()
        .map(|x| x.subject.clone())
        .collect();
    assert_eq!(subjects.len(), 2, "{subjects:?}");
    assert!(subjects.contains(&"a|/".to_string()));
    assert!(subjects.contains(&"a|/data1".to_string()));
    let ev = by_rule(&e, "r-disk");
    let root = ev.iter().find(|x| x.subject == "a|/").unwrap();
    assert_eq!(root.details["mountpoint"], "/");
    assert_eq!(root.agent_id.as_deref(), Some("a"));
    // / 恢復（低於 85）、/data1 仍在 88（未低於恢復門檻）
    e.tick(world(T0 + 11 * MIN, vec![h(50.0, 88.0, 99.0)]));
    let still: Vec<String> = by_rule(&e, "r-disk")
        .iter()
        .filter(|x| x.state.is_active())
        .map(|x| x.subject.clone())
        .collect();
    assert_eq!(still, ["a|/data1"]);
}

#[test]
fn network_swap_and_load_thresholds_use_their_units() {
    let mut rules = rules_with_channels();
    rules.push(metric_rule(
        "r-rx",
        json!({"metric": "network_rx", "op": ">", "threshold": 800.0, "for_secs": 0, "devices": ["eth*"]}),
    ));
    rules.push(metric_rule(
        "r-swap",
        json!({"metric": "swap", "op": ">", "threshold": 50.0, "for_secs": 0}),
    ));
    rules.push(metric_rule(
        "r-load",
        json!({"metric": "load", "op": ">", "threshold": 2.0, "for_secs": 0}),
    ));
    let mut e = engine_with(rules, vec![], &[], true);
    let io = |dev: &str, rate: f64| crate::registry::IoLast {
        device: dev.into(),
        rx_rate: Some(rate),
        ..Default::default()
    };
    let detail = crate::registry::ReportDetail {
        // 120 MB/s = 960 Mbps（eth0 觸發）；docker0 不在範圍內
        network: vec![io("eth0", 120_000_000.0), io("docker0", 900_000_000.0)],
        swap_total: Some(1000),
        swap_used: Some(600),
        load_5m: Some(18.0),
        ..Default::default()
    };
    let mut h = with_detail(host("a", "10.0.0.1"), detail.clone());
    h.logical_cores = Some(8);
    e.tick(world(T0, vec![h.clone()]));
    let rx = by_rule(&e, "r-rx");
    assert_eq!(rx.len(), 1);
    assert_eq!(rx[0].subject, "a|eth0");
    let v = rx[0].details["value"].as_f64().unwrap();
    assert!((v - 960.0).abs() < 1e-6, "{v}");
    assert_eq!(by_rule(&e, "r-swap").len(), 1);
    // 18 ÷ 8 = 2.25 倍
    assert_eq!(by_rule(&e, "r-load").len(), 1);
    // 沒有資產（核心數）或 swap 為 0 的主機沒有值：不觸發
    let mut e2 = engine_with(
        {
            let mut v = rules_with_channels();
            v.push(metric_rule(
                "r-load",
                json!({"metric": "load", "op": ">", "threshold": 0.0, "for_secs": 0}),
            ));
            v.push(metric_rule(
                "r-swap",
                json!({"metric": "swap", "op": ">=", "threshold": 0.0, "for_secs": 0}),
            ));
            v
        },
        vec![],
        &[],
        true,
    );
    let mut d = detail;
    d.swap_total = Some(0);
    e2.tick(world(T0, vec![with_detail(host("b", "10.0.0.2"), d)]));
    assert!(by_rule(&e2, "r-load").is_empty());
    assert!(by_rule(&e2, "r-swap").is_empty());
}

#[test]
fn reboot_is_recorded_as_information_without_notification() {
    let mut e = engine();
    e.tick(world(T0, vec![host("a", "10.0.0.1")]));
    e.take_output();
    e.on_bus(&BusEvent {
        seq: 1,
        at: chrono::DateTime::from_timestamp_millis(T0 + MIN).unwrap(),
        kind: EventKind::HostRebooted {
            agent_id: "a".into(),
            hostname: "h-a".into(),
            uptime_secs: 30,
            previous_uptime_secs: 86_400,
            boot_time: Some("2025-10-09T08:01:10.000Z".into()),
        },
    });
    let out = e.take_output();
    let rec = out
        .events
        .iter()
        .find(|x| x.rule_id == ids::HOST_REBOOTED)
        .unwrap();
    assert_eq!(rec.severity, Severity::Info);
    assert_eq!(rec.attention, Attention::Record);
    assert_eq!(rec.state, EventState::Resolved);
    assert_eq!(rec.details["uptime_secs"], 30);
    assert!(rec.title.contains("重新開機"), "{}", rec.title);
    assert!(out.deliveries.is_empty());
}

/// 遠端管理的系統檢查（REMOTE.md 第 4.7.4 節）：「升級推送已暫停」「升級回滾失敗」「發佈簽章驗證失敗」
#[test]
fn remote_conditions_open_system_events_until_cleared() {
    let mut e = engine();
    let at = |t: i64| chrono::DateTime::from_timestamp_millis(t).unwrap();
    let cond =
        |seq: u64, t: i64, rule: &'static str, subject: &str, active: bool, cause: &str| BusEvent {
            seq,
            at: at(t),
            kind: EventKind::RemoteCondition {
                rule,
                subject: subject.into(),
                active,
                title: format!("{rule} {subject}"),
                details: json!({"cause": cause}),
            },
        };
    e.on_bus(&cond(
        1,
        T0,
        ids::ROLLOUT_PAUSED,
        "r-1",
        true,
        "failure_ratio",
    ));
    e.on_bus(&cond(2, T0, ids::UPGRADE_ROLLBACK_FAILED, "a1", true, ""));
    e.tick(world(T0, vec![]));
    let paused = by_rule(&e, ids::ROLLOUT_PAUSED);
    assert_eq!(paused.len(), 1);
    assert_eq!(
        (paused[0].severity, paused[0].code.as_deref()),
        (Severity::Warning, Some("KRS-W8414"))
    );
    let rb = by_rule(&e, ids::UPGRADE_ROLLBACK_FAILED);
    assert_eq!(
        (rb[0].severity, rb[0].code.as_deref()),
        (Severity::Critical, Some("KRS-E8408"))
    );
    e.on_bus(&cond(3, T0 + MIN, ids::ROLLOUT_PAUSED, "r-1", false, ""));
    e.on_bus(&cond(
        4,
        T0 + MIN,
        ids::UPGRADE_ROLLBACK_FAILED,
        "a1",
        false,
        "",
    ));
    e.tick(world(T0 + MIN, vec![]));
    for rule in [ids::ROLLOUT_PAUSED, ids::UPGRADE_ROLLBACK_FAILED] {
        assert!(
            by_rule(&e, rule)
                .iter()
                .all(|x| x.state != super::model::EventState::Firing),
            "{rule}"
        );
    }
    assert!(rules::builtin_default(ids::RELEASE_SIGNATURE_FAILED).is_some());
}

// ---- 升級中主機的離線告警抑制（REMOTE.md 第 4.7.5 節） ----

fn upgrade_window(seq: u64, t: i64, agent: &str, until: Option<i64>) -> BusEvent {
    BusEvent {
        seq,
        at: chrono::DateTime::from_timestamp_millis(t).unwrap(),
        kind: EventKind::UpgradeWindow {
            agent_id: agent.into(),
            rollout_id: "r-20261111-0001".into(),
            until_ms: until,
        },
    }
}

/// 升級重啟期間（`applying` 到終止狀態之間）不開離線事件；主機回來後抑制結束，也沒有事件
#[test]
fn host_offline_is_suppressed_while_the_host_is_upgrading() {
    let mut e = engine();
    let a = host("a", "10.0.0.1");
    let b = host("b", "10.0.0.2");
    e.tick(world(T0, vec![a.clone(), b.clone()]));
    // a 進入 applying：健康確認 600 秒＋10 分鐘
    let until = T0 + MIN + 20 * MIN;
    e.on_bus(&upgrade_window(1, T0 + MIN, "a", Some(until)));
    let a_off = offline(a.clone(), T0 + MIN, DisconnectReason::IdleTimeout);
    let b_off = offline(b.clone(), T0 + MIN, DisconnectReason::IdleTimeout);
    // 超過 3 分鐘的寬限：沒有在升級的 b 照常開事件，a 不開
    e.tick(world(T0 + 5 * MIN, vec![a_off.clone(), b_off.clone()]));
    let evs = by_rule(&e, ids::HOST_OFFLINE);
    assert_eq!(evs.len(), 1, "{:?}", open(&e));
    assert_eq!(evs[0].subject, "b");
    assert!(evs[0].details.get("upgrade").is_none());
    let out = e.take_output();
    assert!(
        out.deliveries
            .iter()
            .all(|d| !d.payload.to_string().contains("h-a")),
        "{:?}",
        out.deliveries
    );
    // a 升級完成、重新連上：抑制結束，從頭到尾沒有 a 的事件
    e.on_bus(&upgrade_window(2, T0 + 6 * MIN, "a", None));
    e.tick(world(T0 + 6 * MIN, vec![a.clone(), b_off.clone()]));
    e.tick(world(T0 + 30 * MIN, vec![a, b_off]));
    assert!(
        by_rule(&e, ids::HOST_OFFLINE)
            .iter()
            .all(|x| x.subject == "b")
    );
    let out = e.take_output();
    assert!(
        out.timeline
            .iter()
            .all(|t| t.action != "upgrade_suppressed"),
        "{:?}",
        out.timeline
    );
}

/// 期限（`applying_at + health_timeout_secs + 10 分鐘`）過後仍離線：立即照常告警（以實際離線的時間計），
/// 事件的細節與處理紀錄註明「升級中」
#[test]
fn host_offline_fires_once_the_upgrade_window_has_passed() {
    let mut e = engine();
    let a = host("a", "10.0.0.1");
    e.tick(world(T0, vec![a.clone()]));
    let until = T0 + 21 * MIN;
    e.on_bus(&upgrade_window(1, T0 + MIN, "a", Some(until)));
    let off = offline(a.clone(), T0 + MIN, DisconnectReason::IdleTimeout);
    e.tick(world(T0 + 10 * MIN, vec![off.clone()]));
    e.tick(world(until - 1000, vec![off.clone()]));
    assert!(by_rule(&e, ids::HOST_OFFLINE).is_empty());
    assert!(e.take_output().deliveries.is_empty());
    // 期限一到（遠端管理服務的調和還沒送出解除也一樣）：這一輪就開事件並通知
    e.tick(world(until + 1000, vec![off.clone()]));
    let evs = by_rule(&e, ids::HOST_OFFLINE);
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].episode, Some(T0 + MIN), "離線時間以實際離線的時間計");
    assert_eq!(evs[0].details["upgrade"]["rollout_id"], "r-20261111-0001");
    assert_eq!(evs[0].details["upgrade"]["suppressed_until"], until);
    let out = e.take_output();
    assert_eq!(
        out.deliveries.iter().filter(|d| d.kind == "firing").count(),
        2,
        "{:?}",
        out.deliveries
    );
    let notes: Vec<_> = out
        .timeline
        .iter()
        .filter(|t| t.action == "upgrade_suppressed")
        .collect();
    assert_eq!(notes.len(), 1, "{:?}", out.timeline);
    assert_eq!(notes[0].details["until"], until);
    // 之後的解除不影響已開的事件；主機回來後照常恢復
    e.on_bus(&upgrade_window(2, until + 2000, "a", None));
    e.tick(world(until + MIN, vec![off]));
    assert_eq!(by_rule(&e, ids::HOST_OFFLINE)[0].state, EventState::Firing);
    e.tick(world(until + 2 * MIN, vec![a]));
    assert_eq!(
        by_rule(&e, ids::HOST_OFFLINE)[0].state,
        EventState::Recovered
    );
}

/// 抑制只針對離線類：升級中的主機照常觸發回報延遲；已經開著的離線事件不會因為進入升級而消失；
/// 長期失聯在期限內不開、期限後照常
#[test]
fn upgrade_window_only_suppresses_new_offline_events() {
    let mut e = engine();
    let mut a = host("a", "10.0.0.1");
    a.report_interval_ms = Some(5000);
    a.last_report_at = Some(T0);
    let b = host("b", "10.0.0.2");
    e.tick(world(T0, vec![a.clone(), b.clone()]));
    // b 已經離線並開了事件，之後才（不合理地）被標為升級中：事件維持
    let b_off = offline(b.clone(), T0, DisconnectReason::IdleTimeout);
    e.tick(world(T0 + 4 * MIN, vec![a.clone(), b_off.clone()]));
    assert_eq!(by_rule(&e, ids::HOST_OFFLINE).len(), 1);
    e.on_bus(&upgrade_window(
        1,
        T0 + 4 * MIN,
        "a",
        Some(T0 + 30 * 60 * MIN),
    ));
    e.on_bus(&upgrade_window(
        2,
        T0 + 4 * MIN,
        "b",
        Some(T0 + 30 * 60 * MIN),
    ));
    e.tick(world(T0 + 5 * MIN, vec![a.clone(), b_off.clone()]));
    assert_eq!(by_rule(&e, ids::HOST_OFFLINE).len(), 1);
    assert_eq!(by_rule(&e, ids::HOST_OFFLINE)[0].state, EventState::Firing);
    // a 連線中但沒有新的 report：回報延遲不受抑制
    assert_eq!(by_rule(&e, ids::REPORT_STALE).len(), 1, "{:?}", open(&e));
    // 長期失聯（預設 24 小時）：期限內不開新事件，期限後照常
    let a_off = offline(a, T0 + 5 * MIN, DisconnectReason::IdleTimeout);
    e.tick(world(
        T0 + 26 * 60 * MIN,
        vec![a_off.clone(), b_off.clone()],
    ));
    assert!(
        by_rule(&e, ids::HOST_LONG_OFFLINE)
            .iter()
            .all(|x| x.subject != "a"),
        "{:?}",
        open(&e)
    );
    e.tick(world(T0 + 31 * 60 * MIN, vec![a_off, b_off]));
    assert!(
        by_rule(&e, ids::HOST_LONG_OFFLINE)
            .iter()
            .any(|x| x.subject == "a")
    );
    assert!(
        by_rule(&e, ids::HOST_OFFLINE)
            .iter()
            .any(|x| x.subject == "a")
    );
}

/// 一批主機同時升級重啟不算大規模離線、網段離線
#[test]
fn upgrading_hosts_do_not_count_toward_mass_or_subnet_offline() {
    let mut e = engine();
    let hosts: Vec<HostView> = (0..30)
        .map(|i| host(&format!("m{i}"), &format!("10.50.{}.{}", i % 6, i + 1)))
        .collect();
    e.tick(world(T0, hosts.clone()));
    for (i, h) in hosts.iter().enumerate() {
        e.on_bus(&upgrade_window(
            i as u64 + 1,
            T0 + MIN,
            &h.agent_id,
            Some(T0 + 21 * MIN),
        ));
    }
    let down: Vec<HostView> = hosts
        .iter()
        .map(|h| offline(h.clone(), T0 + MIN, DisconnectReason::IdleTimeout))
        .collect();
    e.tick(world(T0 + MIN + 30_000, down.clone()));
    e.tick(world(T0 + 5 * MIN, down));
    assert!(open(&e).is_empty(), "{:?}", open(&e));
}
