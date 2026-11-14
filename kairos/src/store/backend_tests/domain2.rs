//! 領域 store（告警、主機與 API 查詢、管理、遠端管理）在兩個後端上的行為

use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

use super::report::{A, B, dedup, f4_record, legacy, metrics_record, noon};
use super::{TestDb, backend_tests, log, set_now, test_clock};
use crate::alerts::engine::{NewDelivery, Timeline};
use crate::alerts::model::{
    Attention, Category, Channel, ChannelKind, Event, EventState, NotifyPolicy, NotifyState, Rule,
    Scope, Severity, Silence, SilenceKind,
};
use crate::alerts::store::EventFilter;
use crate::remote::store::{
    ArtifactRow, ConfigReport, DesiredUpdate, HostTransition, NewRolloutHost, ReleaseRow,
    RolloutRow,
};
use crate::storage::WriteStatus;
use crate::store::AssetWrite;
use crate::store::db::TxMode;

const T0: i64 = 1_790_000_000_000;
const DAY: i64 = 86_400_000;

fn sorted<K: Ord + Clone, V: Clone>(m: &std::collections::HashMap<K, V>) -> BTreeMap<K, V> {
    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn event(id: &str, rule: &str, sev: Severity, started: i64) -> Event {
    Event {
        event_id: id.into(),
        rule_id: rule.into(),
        dedup_key: format!("{rule}:{id}"),
        kind: "threshold".into(),
        category: Category::Host,
        severity: sev,
        attention: Attention::Action,
        state: EventState::Firing,
        acked: false,
        title: format!("CPU 過高 {id}"),
        subject: "web-01".into(),
        agent_id: Some(A.into()),
        code: None,
        parent_id: None,
        episode: Some(1),
        details: json!({"value": 97.5, "名稱": "測試"}),
        started_at: started,
        updated_at: started,
        last_signal_at: started,
        recovered_at: None,
        resolved_at: None,
        acked_at: None,
        acked_by: None,
        assignee: None,
        eta_at: None,
        root_cause: None,
        resolve_note: None,
        resolved_by: None,
        occurrences: 1,
        reopen_count: 0,
        flapping: false,
        silenced: false,
        in_maintenance: false,
        snooze_until: None,
        notify: NotifyState::default(),
    }
}

fn rule(id: &str, channels: &[&str]) -> Rule {
    Rule {
        rule_id: id.into(),
        builtin: false,
        kind: "threshold".into(),
        category: Category::Host,
        name: format!("規則 {id}"),
        enabled: true,
        severity: Severity::Warning,
        params: json!({"metric": "system.cpu.usage", "op": ">", "value": 90}),
        scope: Scope::default(),
        channels: channels.iter().map(|c| (*c).to_string()).collect(),
        notify: NotifyPolicy::default(),
        description: Some("說明".into()),
        disabled_reason: None,
        created_at: T0,
        created_by: "user:ann".into(),
        updated_at: T0,
        updated_by: "user:ann".into(),
    }
}

fn alerts(db: &TestDb) -> String {
    let mut out = String::new();
    let s = db.handle();
    // 設定版本與規則
    assert_eq!(s.config_rev().unwrap(), 0);
    s.bump_config_rev().unwrap();
    s.bump_config_rev().unwrap();
    assert_eq!(s.config_rev().unwrap(), 2);
    let builtins = log!(out, "builtins", s.ensure_builtin_rules(T0).unwrap());
    assert!(builtins > 0);
    assert_eq!(s.ensure_builtin_rules(T0 + 1).unwrap(), 0);
    // 受保護的系統規則（0.10.1 H1）：舊版寫入的列（params 為 {}、已被停用）在啟動的同步時修正；
    // 其他系統規則被停用的狀態不動
    {
        use crate::alerts::rules::ids;
        let mut old = s.get_rule(ids::ROLLOUT_APPROVAL_DISABLED).unwrap().unwrap();
        assert_eq!(old.params["unsilenceable"], true, "新安裝的列");
        old.params = json!({});
        old.enabled = false;
        old.disabled_reason = Some("先關掉".into());
        s.save_rule(&old).unwrap();
        let mut other = s.get_rule(ids::ROLLOUT_PAUSED).unwrap().unwrap();
        other.enabled = false;
        other.disabled_reason = Some("太吵".into());
        s.save_rule(&other).unwrap();
        let rev = s.config_rev().unwrap();
        assert_eq!(s.ensure_builtin_rules(T0 + 2).unwrap(), 0);
        let fixed = s.get_rule(ids::ROLLOUT_APPROVAL_DISABLED).unwrap().unwrap();
        assert!(fixed.enabled);
        assert_eq!(fixed.disabled_reason, None);
        assert_eq!(fixed.params, json!({"unsilenceable": true}));
        assert_eq!(
            (fixed.updated_at, fixed.updated_by.as_str()),
            (T0 + 2, "system")
        );
        assert!(s.config_rev().unwrap() > rev, "引擎會重新載入");
        let other = s.get_rule(ids::ROLLOUT_PAUSED).unwrap().unwrap();
        assert!(!other.enabled);
        // 已經正確：不再寫入
        let rev = s.config_rev().unwrap();
        assert_eq!(s.ensure_builtin_rules(T0 + 3).unwrap(), 0);
        assert_eq!(s.config_rev().unwrap(), rev);
        // 還原（之後的檢查不受影響）
        let mut other = s.get_rule(ids::ROLLOUT_PAUSED).unwrap().unwrap();
        other.enabled = true;
        other.disabled_reason = None;
        s.save_rule(&other).unwrap();
    }
    s.save_rule(&rule("r-cpu", &["ch-1"])).unwrap();
    s.save_rule(&rule("r-mem", &[])).unwrap();
    let mut changed = rule("r-cpu", &["ch-1", "ch-2"]);
    changed.enabled = false;
    changed.disabled_reason = Some("太吵".into());
    changed.updated_at = T0 + 5;
    changed.updated_by = "user:bob".into();
    s.save_rule(&changed).unwrap();
    log!(out, "rule", s.get_rule("r-cpu").unwrap());
    assert_eq!(s.load_rules().unwrap().len(), builtins + 2);
    log!(
        out,
        "rule order",
        s.load_rules()
            .unwrap()
            .iter()
            .map(|r| r.rule_id.clone())
            .collect::<Vec<_>>()
    );
    assert!(s.delete_rule("r-mem").unwrap());
    assert!(!s.delete_rule("r-mem").unwrap());
    let first_builtin = s.load_rules().unwrap()[0].rule_id.clone();
    assert!(!s.delete_rule(&first_builtin).unwrap());
    log!(out, "config rev", s.config_rev().unwrap());
    // 管道
    let channel = |id: &str| Channel {
        channel_id: id.into(),
        name: format!("管道 {id}"),
        kind: ChannelKind::Webhook,
        enabled: true,
        config: json!({"url": "https://example.com/hook"}),
        secrets: Json::Null,
        state: "ok".into(),
        consecutive_failures: 0,
        failed_since: None,
        last_attempt_at: None,
        last_success_at: None,
        last_error_code: None,
        last_error: None,
        created_at: T0,
        created_by: "user:ann".into(),
        updated_at: T0,
        updated_by: "user:ann".into(),
    };
    s.save_channel(&channel("ch-1"), None).unwrap();
    s.save_channel(&channel("ch-2"), Some(vec![1, 2, 3]))
        .unwrap();
    let mut failing = channel("ch-1");
    failing.state = "failed".into();
    failing.consecutive_failures = 3;
    failing.failed_since = Some(T0 + 10);
    failing.last_attempt_at = Some(T0 + 12);
    failing.last_error_code = Some("KRS-E5101".into());
    failing.last_error = Some("連線逾時".into());
    s.update_channel_status(&failing).unwrap();
    log!(out, "channels", s.load_channels(None).unwrap());
    log!(out, "channel", s.get_channel("ch-1", None).unwrap());
    assert_eq!(
        db.rows("SELECT secrets FROM alert_channels ORDER BY channel_id")
            .len(),
        2
    );
    log!(out, "rules using", s.rules_using_channel("ch-2").unwrap());
    assert!(s.delete_channel("ch-2").unwrap());
    assert!(!s.delete_channel("ch-2").unwrap());
    // 事件
    let mut events = vec![
        event("AL-20261009-0001", "r-cpu", Severity::Warning, T0 + 100),
        event("AL-20261009-0002", "r-cpu", Severity::Critical, T0 + 200),
        event("AL-20261009-0003", "r-disk", Severity::Info, T0 + 300),
        event("AL-20261010-0001", "r-cpu", Severity::Emergency, T0 + DAY),
    ];
    events[2].agent_id = None;
    events[2].category = Category::System;
    events[2].title = "磁碟 100% 已滿_x".into();
    let mut child = event("AL-20261009-0004", "r-cpu", Severity::Warning, T0 + 400);
    child.parent_id = Some("AL-20261009-0002".into());
    events.push(child);
    s.upsert_events(&events).unwrap();
    // upsert：更新既有事件
    let mut e = events[0].clone();
    e.state = EventState::Recovered;
    e.recovered_at = Some(T0 + 500);
    e.updated_at = T0 + 500;
    e.occurrences = 3;
    e.notify.sends = 2;
    e.notify.notified = true;
    let mut acked = events[1].clone();
    acked.state = EventState::Acknowledged;
    acked.acked = true;
    acked.acked_at = Some(T0 + 600);
    acked.acked_by = Some("user:ann".into());
    acked.assignee = Some("user:bob".into());
    acked.silenced = true;
    s.upsert_events(&[e, acked]).unwrap();
    log!(out, "event", s.get_event("AL-20261009-0001").unwrap());
    assert_eq!(s.get_event("nope").unwrap(), None);
    log!(out, "open", {
        // 查詢沒有指定順序（SQLite 依索引、PostgreSQL 依 event_id）：比較內容
        let mut ids: Vec<String> = s
            .load_open_events()
            .unwrap()
            .iter()
            .map(|e| e.event_id.clone())
            .collect();
        ids.sort();
        ids
    });
    log!(
        out,
        "seq",
        sorted(
            &s.load_seq(&["20261009".into(), "20261010".into(), "20261011".into()])
                .unwrap()
        )
    );
    log!(out, "suppressed", sorted(&s.load_suppressed(T0).unwrap()));
    log!(out, "children", s.children("AL-20261009-0002", 10).unwrap());
    log!(out, "counts", s.counts().unwrap());
    log!(out, "stats", sorted(&s.rule_stats(T0).unwrap()));
    let list = |f: EventFilter| {
        let (rows, total) = s.list_events(&f).unwrap();
        (
            rows.iter().map(|e| e.event_id.clone()).collect::<Vec<_>>(),
            total,
        )
    };
    let base = || EventFilter {
        limit: 10,
        ..Default::default()
    };
    log!(out, "all", list(base()));
    for view in ["active", "open", "history", "all", "mine"] {
        log!(
            out,
            view,
            list(EventFilter {
                view: view.into(),
                ..base()
            })
        );
    }
    log!(
        out,
        "with children",
        list(EventFilter {
            include_children: true,
            ..base()
        })
    );
    log!(
        out,
        "severity",
        list(EventFilter {
            severity: vec!["critical".into(), "emergency".into()],
            ..base()
        })
    );
    log!(
        out,
        "state",
        list(EventFilter {
            state: vec!["recovered".into()],
            ..base()
        })
    );
    log!(
        out,
        "rule",
        list(EventFilter {
            rule: vec!["r-disk".into()],
            ..base()
        })
    );
    log!(
        out,
        "category",
        list(EventFilter {
            category: vec!["system".into()],
            ..base()
        })
    );
    log!(
        out,
        "agent",
        list(EventFilter {
            agent_id: Some(A.into()),
            ..base()
        })
    );
    log!(
        out,
        "q",
        list(EventFilter {
            q: Some("cpu".into()),
            ..base()
        })
    );
    log!(
        out,
        "q escape",
        list(EventFilter {
            q: Some("100%".into()),
            ..base()
        })
    );
    log!(
        out,
        "q underscore",
        list(EventFilter {
            q: Some("滿_x".into()),
            ..base()
        })
    );
    log!(
        out,
        "q none",
        list(EventFilter {
            q: Some("100_".into()),
            ..base()
        })
    );
    log!(
        out,
        "range",
        list(EventFilter {
            from_ms: Some(T0 + 150),
            to_ms: Some(T0 + 350),
            ..base()
        })
    );
    // 分頁：每頁 2 筆，以上一頁最後一筆為游標走完
    let mut after = None;
    let mut pages = Vec::new();
    for _ in 0..5 {
        let (rows, _) = s
            .list_events(&EventFilter {
                limit: 2,
                after: after.clone(),
                ..Default::default()
            })
            .unwrap();
        let Some(last) = rows.last() else { break };
        pages.push(rows.iter().map(|e| e.event_id.clone()).collect::<Vec<_>>());
        after = Some((
            last.severity.rank(),
            i64::from(last.acked),
            -last.started_at,
            last.event_id.clone(),
        ));
    }
    log!(out, "pages", pages);
    // 時間軸
    let tl = |id: &str, at: i64, action: &'static str| Timeline {
        event_id: id.into(),
        at,
        actor: "system".into(),
        action,
        details: json!({"at": at}),
    };
    s.insert_timeline(&[
        tl("AL-20261009-0001", T0 + 100, "opened"),
        tl("AL-20261009-0001", T0 + 500, "recovered"),
        tl("AL-20261009-0002", T0 + 200, "opened"),
    ])
    .unwrap();
    log!(out, "timeline", s.timeline("AL-20261009-0001", 10).unwrap());
    log!(
        out,
        "timeline limit",
        s.timeline("AL-20261009-0001", 1).unwrap()
    );
    // 通知佇列
    let nd = |event: Option<&str>, kind: &'static str, at: i64| NewDelivery {
        event_id: event.map(str::to_string),
        channel_id: "ch-1".into(),
        kind,
        urgent: kind == "firing",
        payload: json!({"k": kind}),
        group: None,
        created_at: at,
    };
    let ids = log!(
        out,
        "delivery ids",
        s.insert_deliveries(&[
            nd(Some("AL-20261009-0001"), "firing", T0 + 100),
            nd(Some("AL-20261009-0001"), "recovered", T0 + 500),
            nd(None, "test", T0 + 600),
        ])
        .unwrap()
    );
    assert_eq!(ids, vec![1, 2, 3]);
    let mut pending = s.pending_deliveries().unwrap();
    log!(out, "pending", &pending);
    let d = &mut pending[0];
    d.state = "sent".into();
    d.attempts = 1;
    d.sent_at = Some(T0 + 110);
    d.response_code = Some(200);
    s.update_delivery(d).unwrap();
    let d = &mut pending[1];
    d.state = "pending".into();
    d.attempts = 2;
    d.next_attempt_at = T0 + 900;
    d.error_code = Some("KRS-E5101".into());
    d.error = Some("逾時".into());
    s.update_delivery(d).unwrap();
    log!(
        out,
        "pending after",
        s.pending_deliveries()
            .unwrap()
            .iter()
            .map(|d| (d.id, d.attempts, d.next_attempt_at))
            .collect::<Vec<_>>()
    );
    log!(
        out,
        "of event",
        s.deliveries_of_event("AL-20261009-0001", 10).unwrap()
    );
    log!(
        out,
        "of channel",
        s.deliveries_of_channel("ch-1", 2)
            .unwrap()
            .iter()
            .map(|d| d.id)
            .collect::<Vec<_>>()
    );
    // 靜音
    let silence = |id: &str, kind: SilenceKind, from: i64, to: i64| Silence {
        silence_id: id.into(),
        kind,
        name: format!("靜音 {id}"),
        scope: Scope::default(),
        rules: vec!["r-cpu".into()],
        starts_at: from,
        ends_at: to,
        reason: "維護".into(),
        notify_on_end: true,
        exclude_uptime: false,
        from_event: None,
        created_at: T0,
        created_by: "user:ann".into(),
        updated_at: T0,
        updated_by: "user:ann".into(),
        ended_at: None,
        ended_by: None,
        end_reason: None,
        end_notified: false,
    };
    s.save_silence(&silence("s-past", SilenceKind::Silence, T0, T0 + 100))
        .unwrap();
    s.save_silence(&silence(
        "s-now",
        SilenceKind::Maintenance,
        T0 + 50,
        T0 + 5000,
    ))
    .unwrap();
    s.save_silence(&silence(
        "s-future",
        SilenceKind::Silence,
        T0 + 9000,
        T0 + 9999,
    ))
    .unwrap();
    let mut ended = silence("s-ended", SilenceKind::Silence, T0, T0 + 8000);
    ended.ended_at = Some(T0 + 300);
    ended.ended_by = Some("user:bob".into());
    s.save_silence(&ended).unwrap();
    log!(out, "silence", s.get_silence("s-ended").unwrap());
    for status in ["active", "scheduled", "expired", "all"] {
        log!(out, status, {
            // 依 starts_at 排序，相同時的先後沒有規定：比較內容
            let mut ids: Vec<String> = s
                .list_silences(status, T0 + 1000, 10)
                .unwrap()
                .iter()
                .map(|x| x.silence_id.clone())
                .collect();
            ids.sort();
            ids
        });
    }
    log!(
        out,
        "load silences",
        s.load_silences(T0 + 200)
            .unwrap()
            .iter()
            .map(|x| x.silence_id.clone())
            .collect::<Vec<_>>()
    );
    s.mark_silences_notified(&["s-past".into(), "s-ended".into()])
        .unwrap();
    s.mark_silences_notified(&[]).unwrap();
    log!(
        out,
        "notified",
        db.rows("SELECT silence_id, end_notified FROM alert_silences ORDER BY silence_id")
    );
    // 動作佇列
    let a1 = s
        .enqueue_action(
            T0 + 700,
            "cli:root",
            "AL-20261009-0002",
            "ack",
            &json!({"note": "處理中"}),
        )
        .unwrap();
    let a2 = s
        .enqueue_action(
            T0 + 701,
            "cli:root",
            "AL-20261009-0003",
            "resolve",
            &json!({}),
        )
        .unwrap();
    assert_eq!((a1, a2), (1, 2));
    log!(out, "actions", s.pending_actions().unwrap());
    assert_eq!(s.action_result(a1).unwrap(), None);
    s.finish_action(a1, T0 + 710, &json!({"ok": true})).unwrap();
    log!(out, "action result", s.action_result(a1).unwrap());
    log!(
        out,
        "actions left",
        s.pending_actions()
            .unwrap()
            .iter()
            .map(|a| a.id)
            .collect::<Vec<_>>()
    );
    // 清除：已結束的事件超過保留期限
    let mut old = event("AL-20260101-0001", "r-cpu", Severity::Info, T0 - 200 * DAY);
    old.state = EventState::Resolved;
    old.resolved_at = Some(T0 - 199 * DAY);
    s.upsert_events(&[old]).unwrap();
    s.insert_timeline(&[tl("AL-20260101-0001", T0 - 200 * DAY, "opened")])
        .unwrap();
    log!(
        out,
        "purge",
        s.purge_alerts(T0 + DAY, 90, 1000, 100).unwrap()
    );
    log!(
        out,
        "purge again",
        s.purge_alerts(T0 + DAY, 90, 1000, 100).unwrap()
    );
    // 筆數上限：只留最新的 2 筆已結束的事件之外的……（以兩個後端的結果相同為準）
    log!(
        out,
        "purge max",
        s.purge_alerts(T0 + DAY, 0, 2, 100).unwrap()
    );
    log!(
        out,
        "events left",
        db.rows("SELECT event_id, state FROM alert_events ORDER BY event_id")
    );
    log!(
        out,
        "timeline left",
        db.int("SELECT count(*) FROM alert_timeline")
    );
    log!(
        out,
        "deliveries left",
        db.int("SELECT count(*) FROM alert_deliveries")
    );
    out
}

/// 寫入兩台主機的資料（report、行程快照、資產、彙總），回傳後 store 已關閉
pub(super) fn seed_hosts(db: &TestDb) {
    set_now(noon());
    let (mut store, _) = db.store(test_clock);
    let list: kairos_proto::ProcessList = serde_json::from_value(json!({
        "top_cpu": [{"pid": 1, "name": "init", "cpu_usage": 1.5, "memory": 10}],
    }))
    .unwrap();
    for i in 0..12 {
        let t = noon() + chrono::Duration::seconds(i * 30);
        let a = f4_record(A, i as u64, t, 1000 * i as u64, 100 + 30 * i as u64)
            .with_dedup(dedup("boot-a", i as u64))
            .with_processes(Some(list.clone()));
        let b = legacy(B, i as u64, t, 5.0 + i as f32);
        assert_eq!(
            store.write_batch(&[a, b]),
            [WriteStatus::Written, WriteStatus::Written]
        );
    }
    assert_eq!(
        store.write_batch(&[metrics_record(B, 99, noon(), 3.25)]),
        [WriteStatus::Written]
    );
    for (ms, serial) in [(1000, "S1"), (2000, "S2")] {
        store
            .write_assets(&AssetWrite {
                agent_id: A.into(),
                hostname: "f4".into(),
                peer: "10.0.0.3:1".into(),
                collected_ms: ms,
                received_ms: ms,
                inventory: serde_json::from_value(json!({
                    "os": {"family": "linux", "long_version": "Linux (Ubuntu 24.04)"},
                    "cpu": {"model": "Xeon", "logical_cores": 8},
                    "memory": {"total": 8_589_934_592_u64},
                    "system": {"vendor": "QEMU", "serial": serial}
                }))
                .unwrap(),
            })
            .unwrap();
    }
    set_now(noon() + chrono::Duration::minutes(20));
    while store.maintain() {}
    store.close();
}

fn hosts_and_api_queries(db: &TestDb) -> String {
    let mut out = String::new();
    seed_hosts(db);
    let s = db.read();
    let (page, pages) = s.db_page_stats().unwrap();
    assert!(page > 0 && pages > 0, "{page} {pages}");
    let a = s.host_id_of(A).unwrap().unwrap();
    let b = s.host_id_of(B).unwrap().unwrap();
    assert_eq!(s.host_id_of("nobody").unwrap(), None);
    log!(out, "ids", (a, b));
    log!(out, "trend_host", s.trend_host(A).unwrap());
    log!(
        out,
        "host_series",
        s.host_series(a, "system.network.received").unwrap()
    );
    log!(
        out,
        "standard",
        s.standard_series_id("system.cpu.usage", "%").unwrap()
    );
    log!(out, "uptime", s.uptime_series_id("system.uptime").unwrap());
    log!(out, "series_info", s.series_info(1).unwrap());
    assert_eq!(s.series_info(9999).unwrap(), None);
    let mid = noon().timestamp_millis() + 100_000;
    log!(out, "ts before", s.report_ts_before(a, mid).unwrap());
    assert_eq!(s.report_ts_before(a, 0).unwrap(), None);
    log!(out, "latest", s.latest_report(a).unwrap());
    let mut seen = Vec::new();
    s.for_each_report(
        a,
        noon().timestamp_millis() + 30_000,
        mid,
        &mut |ts, format, body| {
            seen.push((ts, format, body.len()));
        },
    )
    .unwrap();
    log!(out, "reports", seen);
    for table in ["rollup_5m", "rollup_1h"] {
        let mut rows = Vec::new();
        s.for_each_rollup(
            table,
            a,
            0,
            i64::MAX / 2,
            &|id| id % 2 == 1,
            &mut |bucket, series, agg| {
                rows.push((bucket, series, agg));
            },
        )
        .unwrap();
        assert!(!rows.is_empty());
        log!(out, table, rows);
    }
    // 序列編號是內部的（兩個後端建立衍生序列的先後不同）：以序列的內容比較
    let mut recent: Vec<String> = s
        .recent_rollup_series(a, 0)
        .unwrap()
        .into_iter()
        .map(|id| format!("{:?}", s.series_info(id).unwrap()))
        .collect();
    recent.sort();
    log!(out, "recent series", recent);
    log!(out, "group", s.host_group_of(A).unwrap());
    assert_eq!(s.host_group_of("nobody").unwrap(), None);
    log!(out, "basics", s.host_basics(A).unwrap());
    log!(out, "inventory", s.host_inventory(a).unwrap());
    assert_eq!(s.host_inventory(b).unwrap(), None);
    log!(
        out,
        "changes",
        s.inventory_changes(a, i64::MAX, 10).unwrap()
    );
    log!(
        out,
        "snapshot",
        s.process_snapshot_at(a, mid)
            .unwrap()
            .map(|(ts, body)| (ts, body.len()))
    );
    assert_eq!(s.process_snapshot_at(b, mid).unwrap(), None);
    log!(out, "agents", s.all_agent_ids().unwrap());
    log!(out, "names", s.host_names(A).unwrap());
    log!(out, "list_hosts", s.list_hosts().unwrap());
    log!(out, "meta rows", s.host_meta_rows().unwrap());
    // 匯出：與寫入的內容相同（JSONL 的每一行）
    let mut lines = Vec::new();
    let n = s
        .export_reports(0, None, &mut |r| {
            lines.push(format!(
                "{} {} {} {:?} {:?}",
                r.agent_id, r.seq, r.ts, r.dedup, r.body
            ));
            Ok(())
        })
        .unwrap();
    assert_eq!(n, 25);
    log!(out, "export", lines);
    let mut only_b = 0;
    s.export_reports(noon().timestamp_millis() + 60_000, Some(B), &mut |r| {
        assert_eq!(r.agent_id, B);
        only_b += 1;
        Ok(())
    })
    .unwrap();
    log!(out, "export b since", only_b);
    // 輸出端的錯誤照原樣回報（BrokenPipe）
    let e = s
        .export_reports(0, None, &mut |_| {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        })
        .unwrap_err();
    assert_eq!(e.io_kind(), Some(std::io::ErrorKind::BrokenPipe), "{e}");
    // API 的查詢
    log!(out, "load_hosts", s.load_hosts().unwrap());
    log!(out, "load_host", s.load_host(A).unwrap());
    assert!(s.load_host("nobody").unwrap().is_none());
    assert_eq!(s.max_host_id().unwrap(), 2);
    log!(out, "cert hosts", sorted(&s.cert_hosts(None).unwrap()));
    log!(out, "latest cert", s.latest_cert_of(A).unwrap());
    // 告警用的查詢
    log!(out, "inv", s.alert_inventory().unwrap());
    log!(out, "seen since", {
        let mut v: Vec<_> = s.active_hosts_seen_since(0).unwrap().into_iter().collect();
        v.sort();
        v
    });
    log!(
        out,
        "plain",
        s.plain_series_ids("system.cpu.usage").unwrap()
    );
    log!(
        out,
        "by name",
        // 序列編號是內部的（兩個後端建立衍生序列的先後不同）
        s.host_series_by_name(a, "system.filesystem.usage")
            .unwrap()
            .into_iter()
            .map(|(_, labels, derived)| (labels, derived))
            .collect::<Vec<_>>()
    );
    log!(out, "cores", s.host_logical_cores(a).unwrap());
    log!(out, "means", s.rollup_means("rollup_5m", a, 1, 0).unwrap());
    log!(out, "backtest hosts", s.backtest_hosts().unwrap());
    log!(out, "backtest info", s.backtest_host_info(a).unwrap());
    log!(out, "scope rows", s.active_host_scope_rows().unwrap());
    // 被拒的憑證（寫入）
    let w = db.handle();
    log!(
        out,
        "rejections",
        w.store_cert_rejections(&[
            (
                A.into(),
                T0,
                "revoked".into(),
                Some("0a".into()),
                Some("10.0.0.1".into()),
                3
            ),
            ("nobody".into(), T0, "expired".into(), None, None, 1),
            (
                A.into(),
                T0 + 5,
                "revoked".into(),
                Some("0a".into()),
                None,
                2
            ),
        ])
        .unwrap()
    );
    log!(
        out,
        "rejection row",
        // JSON 文字的鍵順序與空白兩個後端不同（PostgreSQL 由 jsonb 產生）：比較解析後的內容
        serde_json::from_str::<serde_json::Value>(
            &db.text("SELECT cert_rejection FROM hosts WHERE cert_rejection IS NOT NULL")
                .unwrap_or_default()
        )
        .ok()
    );
    // 管理
    let counts = log!(out, "diag", w.diag_counts().unwrap());
    assert_eq!(
        counts.0,
        Some(i64::from(crate::store::sqlite::SCHEMA_VERSION))
    );
    assert_eq!(w.database_dir().unwrap().map(|d| d == db.dir), Some(true));
    let data = w
        .personal_data(A, &crate::privacy::PrivacySettings::default(), T0)
        .unwrap()
        .unwrap();
    log!(out, "personal data", data);
    assert!(
        w.personal_data("nobody", &crate::privacy::PrivacySettings::default(), T0)
            .unwrap()
            .is_none()
    );
    // backup_to：只有 SQLite 支援
    let dest = db.dir.join("backup.db");
    if db.is_postgres() {
        assert!(w.backup_to(&dest).is_err());
        assert!(!dest.exists());
    } else {
        w.backup_to(&dest).unwrap();
        assert!(dest.is_file());
    }
    // 離線刪除（CLI）：一次做完
    let plan = w.purge_plan(B, T0).unwrap();
    let mut job = crate::store::purge::PurgeJob::new(B, "cli:root", None, None);
    w.purge_run_to_end(&mut job, T0).unwrap();
    assert_eq!(job.counts, plan);
    log!(out, "purged", &job.counts);
    assert_eq!(w.all_agent_ids().unwrap(), vec![A.to_string()]);
    out
}

fn remote_config_and_rollouts(db: &TestDb) -> String {
    let mut out = String::new();
    seed_hosts(db);
    let s = db.handle();
    assert_eq!(s.remote_config_rev().unwrap(), 0);
    assert_eq!(s.bump_remote_config_rev().unwrap(), 1);
    assert_eq!(s.bump_remote_config_rev().unwrap(), 2);
    assert_eq!(s.remote_config_rev().unwrap(), 2);
    // 設定檔：名稱不分大小寫
    let web = s
        .insert_profile(
            "Web",
            "網站",
            "{\"report\":{\"interval_secs\":5}}",
            T0,
            "user:ann",
            Some("建立"),
        )
        .unwrap();
    let db_p = s
        .insert_profile("db", "", "{}", T0 + 1, "user:ann", None)
        .unwrap();
    assert_eq!((web, db_p), (1, 2));
    assert!(
        s.insert_profile("WEB", "", "{}", T0 + 2, "user:ann", None)
            .is_err()
    );
    assert_eq!(s.profile_id_by_name("web").unwrap(), Some(web));
    assert_eq!(s.profile_id_by_name("nope").unwrap(), None);
    log!(
        out,
        "update",
        s.update_profile(
            web,
            "web",
            "網站 v2",
            "{\"report\":{\"interval_secs\":10}}",
            T0 + 3,
            "user:bob",
            Some("調整")
        )
        .unwrap()
    );
    assert!(
        s.update_profile(db_p, "WEB", "", "{}", T0 + 4, "user:bob", None)
            .is_err()
    );
    log!(out, "profiles", s.list_profiles().unwrap());
    log!(out, "profile", s.get_profile(web).unwrap());
    log!(out, "revisions", s.profile_revisions(web).unwrap());
    // 指派
    let a1 = log!(
        out,
        "assign all",
        s.insert_assignment(web, "global", "", 0, None, T0 + 10, "user:ann")
            .unwrap()
    );
    let a2 = log!(
        out,
        "assign host",
        s.insert_assignment(db_p, "host", A, 10, Some(T0 + 5000), T0 + 11, "user:ann")
            .unwrap()
    );
    log!(
        out,
        "assign dup",
        s.insert_assignment(db_p, "host", A, 20, None, T0 + 12, "user:ann")
            .unwrap()
    );
    log!(out, "assignments", s.list_assignments().unwrap());
    log!(out, "assignment", s.get_assignment(a2.unwrap()).unwrap());
    log!(
        out,
        "expired",
        s.expired_assignments(T0 + 6000)
            .unwrap()
            .iter()
            .map(|a| a.id)
            .collect::<Vec<_>>()
    );
    log!(out, "targets", s.config_targets().unwrap());
    log!(out, "target", s.config_target(A).unwrap());
    assert!(s.config_target("nobody").unwrap().is_none());
    // 主機的設定狀態
    s.upsert_hello_info(
        A,
        "[\"env:prod\"]",
        "0.9.0",
        Some("{\"method\":\"deb\",\"upgrade\":\"allowed\",\"config\":\"managed\"}"),
    )
    .unwrap();
    s.upsert_hello_info(B, "[]", "0.8.0", None).unwrap();
    s.upsert_desired(&[
        DesiredUpdate {
            agent_id: A.into(),
            desired_rev: 3,
            desired_hash: "h3".into(),
            desired: "{\"a\":1}".into(),
            sources: "[]".into(),
        },
        DesiredUpdate {
            agent_id: B.into(),
            desired_rev: 3,
            desired_hash: "h3b".into(),
            desired: "{}".into(),
            sources: "[]".into(),
        },
    ])
    .unwrap();
    s.upsert_desired(&[DesiredUpdate {
        agent_id: A.into(),
        desired_rev: 4,
        desired_hash: "h4".into(),
        desired: "{\"a\":2}".into(),
        sources: "[1]".into(),
    }])
    .unwrap();
    log!(out, "desired", s.desired_all().unwrap());
    let report = |rev: i64, state: &str, at: i64| ConfigReport {
        agent_id: A.into(),
        rev,
        hash: format!("h{rev}"),
        state: state.into(),
        effective: "{\"a\":2}".into(),
        overridden: "[\"report.interval_secs\"]".into(),
        ignored: "[]".into(),
        errors: "[]".into(),
        code: None,
        reported_at: at,
    };
    log!(
        out,
        "status applied",
        s.record_config_status(&report(4, "applied", T0 + 100))
            .unwrap()
    );
    // 晚到的舊版回報
    log!(
        out,
        "status late",
        s.record_config_status(&report(3, "failed", T0 + 101))
            .unwrap()
    );
    log!(
        out,
        "status again",
        s.record_config_status(&report(4, "applied", T0 + 102))
            .unwrap()
    );
    log!(out, "host config", s.host_config(A).unwrap());
    log!(out, "host configs", s.host_configs().unwrap().len());
    assert!(s.host_config("nobody").unwrap().is_none());
    assert!(s.delete_assignment(a1.unwrap()).unwrap());
    assert!(!s.delete_assignment(a1.unwrap()).unwrap());
    assert!(s.delete_assignment(a2.unwrap()).unwrap());
    assert!(s.delete_profile(db_p).unwrap());
    assert!(!s.delete_profile(db_p).unwrap());
    // 發佈檔
    let release = |version: &str| ReleaseRow {
        id: 0,
        version: version.into(),
        commit: "abc1234".into(),
        source: "hosted".into(),
        sha256sums: "sums".into(),
        signature: "sig".into(),
        signer_fingerprint: Some("SHA256:xx".into()),
        signer_principal: None,
        manifest_name: None,
        manifest: None,
        imported_at: T0,
        imported_by: "user:ann".into(),
        base_url: None,
    };
    let r1 = s.insert_release(&release("0.9.1")).unwrap();
    let r2 = s.insert_release(&release("0.10.0")).unwrap();
    assert_eq!((r1, r2), (1, 2));
    assert!(s.insert_release(&release("0.9.1")).is_err());
    let artifact = |rel: i64, name: &str, sha: &str, stored: bool| ArtifactRow {
        release_id: rel,
        filename: name.into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        format: "deb".into(),
        sha256: sha.into(),
        size: 1234,
        stored,
        stored_at: stored.then_some(T0),
    };
    s.upsert_artifact(&artifact(r1, "pulse_0.9.1_amd64.deb", "aa", false))
        .unwrap();
    s.upsert_artifact(&artifact(r1, "pulse_0.9.1_amd64.deb", "aa", true))
        .unwrap();
    s.upsert_artifact(&artifact(r2, "pulse_0.10.0_amd64.deb", "aa", true))
        .unwrap();
    s.upsert_artifact(&artifact(r2, "pulse_0.10.0_arm64.deb", "bb", false))
        .unwrap();
    log!(out, "releases", s.list_releases().unwrap());
    log!(
        out,
        "release",
        s.get_release("0.10.0").unwrap().map(|r| r.id)
    );
    log!(
        out,
        "by id",
        s.get_release_by_id(r1).unwrap().map(|r| r.version)
    );
    log!(out, "artifacts", s.release_artifacts(r1).unwrap());
    log!(out, "all artifacts", s.all_artifacts().unwrap().len());
    log!(
        out,
        "refs",
        (
            s.artifact_refs("aa").unwrap(),
            s.artifact_refs("bb").unwrap(),
            s.artifact_refs("zz").unwrap()
        )
    );
    log!(out, "upgrade hosts", s.upgrade_hosts().unwrap());
    log!(out, "upgrade host", s.upgrade_host(A).unwrap());
    // 推送
    let rollout = |id: &str, state: &str| RolloutRow {
        id: id.into(),
        name: format!("推送 {id}"),
        release_id: r2,
        target_version: "0.10.0".into(),
        selector: "{\"all\":true}".into(),
        waves: "[1,100]".into(),
        schedule: "{}".into(),
        options: "{}".into(),
        state: state.into(),
        current_wave: 0,
        wave_started_at: None,
        soak_until: None,
        awaiting_continue: false,
        pause_reason: None,
        pause_cause: None,
        cancel_reason: None,
        created_at: T0 + 200,
        created_by: "user:ann".into(),
        started_at: None,
        completed_at: None,
        updated_at: T0 + 200,
        submitted_at: None,
        submitted_by: None,
        approved_at: None,
        approved_by: None,
        reject_reason: None,
    };
    s.insert_rollout(&rollout("RO-20261009-01", "draft"))
        .unwrap();
    s.insert_rollout(&rollout("RO-20261009-02", "draft"))
        .unwrap();
    assert_eq!(s.count_rollouts_with_prefix("RO-20261009-").unwrap(), 2);
    assert!(
        s.update_rollout_spec(
            "RO-20261009-01",
            "新名稱",
            r2,
            "{\"group\":\"web\"}",
            "[50,100]",
            "{\"x\":1}",
            T0 + 210
        )
        .unwrap()
    );
    let mut running = s.get_rollout("RO-20261009-01").unwrap().unwrap();
    running.state = "running".into();
    running.current_wave = 1;
    running.wave_started_at = Some(T0 + 220);
    running.started_at = Some(T0 + 220);
    running.awaiting_continue = true;
    running.updated_at = T0 + 220;
    s.update_rollout_state(&running).unwrap();
    // 執行中的推送不能再改規格
    log!(
        out,
        "spec while running",
        s.update_rollout_spec("RO-20261009-01", "x", r2, "{}", "[]", "{}", T0 + 230)
            .unwrap()
    );
    log!(out, "rollout", s.get_rollout("RO-20261009-01").unwrap());
    log!(
        out,
        "rollouts",
        s.list_rollouts(None)
            .unwrap()
            .iter()
            .map(|r| (r.id.clone(), r.state.clone()))
            .collect::<Vec<_>>()
    );
    log!(
        out,
        "running",
        s.list_rollouts(Some("running")).unwrap().len()
    );
    assert_eq!(s.rollouts_using_release(r2).unwrap(), 2);
    assert_eq!(s.rollouts_using_release(r1).unwrap(), 0);
    let a = s.host_id_of(A).unwrap().unwrap();
    let b = s.host_id_of(B).unwrap().unwrap();
    let nh = |host: i64, n: i64| NewRolloutHost {
        host_id: host,
        upgrade_id: format!("up-{host}"),
        wave: n,
        ordinal: n,
        state: "pending".into(),
        reason: None,
        from_version: Some("0.9.0".into()),
    };
    s.insert_rollout_hosts("RO-20261009-01", &[nh(a, 1), nh(b, 2)], T0 + 240)
        .unwrap();
    log!(out, "hosts", s.rollout_hosts("RO-20261009-01").unwrap());
    s.update_rollout_host(
        "RO-20261009-01",
        a,
        &HostTransition {
            state: Some("offered".into()),
            offered_at: Some(Some(T0 + 250)),
            to_version: Some("0.10.0".into()),
            bump_attempts: true,
            ..Default::default()
        },
        T0 + 250,
    )
    .unwrap();
    log!(out, "active offers", s.active_offers().unwrap());
    s.update_rollout_host(
        "RO-20261009-01",
        a,
        &HostTransition {
            state: Some("rollback_failed".into()),
            reason: Some(Some("安裝失敗".into())),
            bytes_done: Some(10),
            bytes_total: Some(100),
            code: Some(Some("PLS-E8401".into())),
            detail: Some(Some("dpkg".into())),
            applying_at: Some(Some(T0 + 260)),
            finished_at: Some(Some(T0 + 270)),
            bump_attempts: true,
            ..Default::default()
        },
        T0 + 270,
    )
    .unwrap();
    log!(out, "host", s.rollout_host("RO-20261009-01", A).unwrap());
    log!(
        out,
        "by upgrade id",
        s.rollout_host_by_upgrade_id(&format!("up-{b}"))
            .unwrap()
            .map(|h| h.agent_id)
    );
    assert!(s.rollout_host_by_upgrade_id("nope").unwrap().is_none());
    log!(out, "upgrades", s.host_upgrades(A, 5).unwrap().len());
    log!(out, "in active", s.hosts_in_active_rollouts().unwrap());
    log!(
        out,
        "rollback failures",
        s.unacknowledged_rollback_failures().unwrap()
    );
    // 0.10.1（H3）：推送清單的進度由資料庫彙總。這時 01（running）的 a 是回滾失敗未處理、b 是 pending；
    // 02 是草稿、沒有主機（沒有列）
    {
        use crate::remote::HostSummary;
        use crate::remote::store::RolloutHostCount;
        let count = |state: &str, hosts: i64, flagged: i64| RolloutHostCount {
            rollout_id: "RO-20261009-01".into(),
            state: state.into(),
            hosts,
            flagged,
        };
        let all = log!(out, "host counts", s.rollout_host_counts(None).unwrap());
        assert_eq!(
            all,
            vec![count("pending", 1, 0), count("rollback_failed", 1, 1)]
        );
        assert_eq!(s.rollout_host_counts(Some("running")).unwrap(), all);
        assert!(s.rollout_host_counts(Some("draft")).unwrap().is_empty());
        assert!(s.rollout_host_counts(Some("nope")).unwrap().is_empty());
        // 等待版本庫檔案的 pending 另外計數；彙總的結果與從主機列計算的相同
        let waiting = HostTransition {
            reason: Some(Some("artifact_missing".into())),
            ..Default::default()
        };
        s.update_rollout_host("RO-20261009-01", b, &waiting, T0 + 271)
            .unwrap();
        let counts = s.rollout_host_counts(None).unwrap();
        assert_eq!(
            counts,
            vec![count("pending", 1, 1), count("rollback_failed", 1, 1)]
        );
        let by_rollout = HostSummary::from_counts(counts);
        assert_eq!(by_rollout.len(), 1);
        let from_rows = HostSummary::of(&s.rollout_hosts("RO-20261009-01").unwrap());
        assert_eq!(by_rollout["RO-20261009-01"], from_rows);
        assert_eq!(
            (
                from_rows.hosts_total(),
                from_rows.rollback_failed_unacknowledged,
                from_rows.waiting_artifact,
                from_rows.progress.len(),
            ),
            (2, 1, 1, 15)
        );
        let clear = HostTransition {
            reason: Some(None),
            ..Default::default()
        };
        s.update_rollout_host("RO-20261009-01", b, &clear, T0 + 272)
            .unwrap();
        log!(out, "state counts", s.rollout_state_counts().unwrap());
        assert_eq!(
            s.rollout_state_counts().unwrap(),
            vec![("draft".to_string(), 1), ("running".to_string(), 1)]
        );
        assert_eq!(s.count_unacknowledged_rollback_failures().unwrap(), 1);
    }
    s.update_rollout_host(
        "RO-20261009-01",
        a,
        &HostTransition {
            acknowledged: Some((T0 + 300, "user:ann".into())),
            reason: Some(None),
            ..Default::default()
        },
        T0 + 300,
    )
    .unwrap();
    assert!(s.unacknowledged_rollback_failures().unwrap().is_empty());
    assert_eq!(s.count_unacknowledged_rollback_failures().unwrap(), 0);
    assert_eq!(
        s.rollout_host_counts(None)
            .unwrap()
            .iter()
            .map(|c| (c.state.as_str(), c.hosts, c.flagged))
            .collect::<Vec<_>>(),
        vec![("pending", 1, 0), ("rollback_failed", 1, 0)],
        "已標記為已處理的不再計入"
    );
    log!(
        out,
        "acknowledged",
        s.rollout_host("RO-20261009-01", A).unwrap()
    );
    // 清除舊的推送
    let mut done = s.get_rollout("RO-20261009-02").unwrap().unwrap();
    done.state = "completed".into();
    done.completed_at = Some(T0 + 400);
    s.update_rollout_state(&done).unwrap();
    log!(out, "delete old", s.delete_old_rollouts(T0 + 500).unwrap());
    log!(
        out,
        "left",
        s.list_rollouts(None)
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>()
    );
    // 沒有被推送使用的發佈檔可以刪除（連同檔案清單）
    {
        let tx = s.begin(TxMode::Immediate).unwrap();
        assert!(tx.delete_release(r1).unwrap());
        assert!(!tx.delete_release(r1).unwrap());
        tx.commit().unwrap();
    }
    log!(
        out,
        "artifacts left",
        s.all_artifacts()
            .unwrap()
            .iter()
            .map(|a| a.filename.clone())
            .collect::<Vec<_>>()
    );
    out
}

/// 0.10.0（schema v11）的遠端管理介面：外部 URL 的版本庫（`releases.base_url`、已登記而未存入的發佈檔）、
/// 推送的排程（`update_rollout_schedule`、狀態 `scheduled`）、雙人覆核（`pending_approval`、
/// `submitted_*`／`approved_*`／`reject_reason`）、退回草稿時刪除固定的對象（`delete_rollout_hosts`）、
/// 替換中的主機（`applying_hosts`）
fn rollout_schedule_approval_and_external_releases(db: &TestDb) -> String {
    let mut out = String::new();
    seed_hosts(db);
    let s = db.handle();
    // 外部 URL 的版本庫：base_url 保存與讀回；代管的版本為 NULL
    let release = |version: &str, source: &str, base_url: Option<&str>| ReleaseRow {
        id: 0,
        version: version.into(),
        commit: "abc1234".into(),
        source: source.into(),
        sha256sums: "sums".into(),
        signature: "sig".into(),
        signer_fingerprint: None,
        signer_principal: None,
        manifest_name: Some("pulse-release.json".into()),
        manifest: Some("{}".into()),
        imported_at: T0,
        imported_by: "cli:root".into(),
        base_url: base_url.map(str::to_string),
    };
    let hosted = s
        .insert_release(&release("0.10.0", "hosted", None))
        .unwrap();
    let external = s
        .insert_release(&release(
            "0.10.1",
            "external",
            Some("https://mirror.example/pulse/0.10.1/"),
        ))
        .unwrap();
    assert!(
        s.insert_release(&release("0.10.2", "elsewhere", None))
            .is_err()
    );
    log!(out, "releases", s.list_releases().unwrap());
    log!(
        out,
        "external",
        s.get_release("0.10.1")
            .unwrap()
            .map(|r| (r.source, r.base_url))
    );
    log!(
        out,
        "hosted",
        s.get_release_by_id(hosted)
            .unwrap()
            .map(|r| (r.source, r.base_url))
    );
    // 外部版本的發佈檔：size > 0 且 stored = 0 表示已登記（檔案不在 Kairos）；不計入 artifact_refs
    let artifact = |rel: i64, name: &str, sha: &str, size: i64, stored: bool| ArtifactRow {
        release_id: rel,
        filename: name.into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        format: "deb".into(),
        sha256: sha.into(),
        size,
        stored,
        stored_at: stored.then_some(T0),
    };
    for a in [
        artifact(external, "pulse_0.10.1_amd64.deb", "ee", 0, false),
        artifact(external, "pulse_0.10.1_arm64.deb", "ff", 0, false),
        artifact(external, "pulse_0.10.1_amd64.deb", "ee", 4321, false),
        artifact(hosted, "pulse_0.10.0_amd64.deb", "ee", 4321, true),
    ] {
        s.upsert_artifact(&a).unwrap();
    }
    log!(
        out,
        "external artifacts",
        s.release_artifacts(external).unwrap()
    );
    log!(
        out,
        "registered",
        s.release_artifacts(external)
            .unwrap()
            .iter()
            .map(|a| (a.filename.clone(), a.size > 0 && !a.stored))
            .collect::<Vec<_>>()
    );
    log!(
        out,
        "refs",
        (
            s.artifact_refs("ee").unwrap(),
            s.artifact_refs("ff").unwrap()
        )
    );
    // 推送：insert_rollout 不寫雙人覆核的欄位（送出、核准時才由 update_rollout_state 寫入），讀回為 NULL
    let rollout = |id: &str, at: i64| RolloutRow {
        id: id.into(),
        name: format!("推送 {id}"),
        release_id: external,
        target_version: "0.10.1".into(),
        selector: "{\"all\":true}".into(),
        waves: "[]".into(),
        schedule: "{}".into(),
        options: "{\"health_timeout_secs\":300}".into(),
        state: "draft".into(),
        current_wave: 0,
        wave_started_at: None,
        soak_until: None,
        awaiting_continue: false,
        pause_reason: None,
        pause_cause: None,
        cancel_reason: None,
        created_at: at,
        created_by: "user:ann".into(),
        started_at: None,
        completed_at: None,
        updated_at: at,
        submitted_at: Some(1),
        submitted_by: Some("user:x".into()),
        approved_at: Some(1),
        approved_by: Some("user:x".into()),
        reject_reason: Some("x".into()),
    };
    const R1: &str = "r-20261101-0001";
    const R2: &str = "r-20261101-0002";
    const R3: &str = "r-20261101-0003";
    for (i, id) in [R1, R2, R3].into_iter().enumerate() {
        s.insert_rollout(&rollout(id, T0 + 100 + i as i64)).unwrap();
    }
    log!(out, "draft", s.get_rollout(R1).unwrap());
    // 排程：只有草稿可以修改
    let schedule = "{\"starts_at\":1790000900000,\"timezone\":\"+08:00\",\"windows\":[{\"days\":[1,2],\"start\":\"22:00\",\"end\":\"02:00\"}]}";
    log!(
        out,
        "schedule draft",
        s.update_rollout_schedule(R1, schedule, T0 + 110).unwrap()
    );
    log!(
        out,
        "schedule missing",
        s.update_rollout_schedule("r-nope", schedule, T0 + 110)
            .unwrap()
    );
    // 送出：雙人覆核時先進入 pending_approval，對象已固定
    let a = s.host_id_of(A).unwrap().unwrap();
    let b = s.host_id_of(B).unwrap().unwrap();
    let nh = |host: i64, n: i64| NewRolloutHost {
        host_id: host,
        upgrade_id: format!("up-{n}-{host}"),
        wave: 0,
        ordinal: n,
        state: "pending".into(),
        reason: None,
        from_version: Some("0.9.0".into()),
    };
    let mut r1 = s.get_rollout(R1).unwrap().unwrap();
    r1.state = "pending_approval".into();
    r1.submitted_at = Some(T0 + 120);
    r1.submitted_by = Some("user:ann".into());
    r1.updated_at = T0 + 120;
    s.update_rollout_state(&r1).unwrap();
    s.insert_rollout_hosts(R1, &[nh(a, 1), nh(b, 2)], T0 + 120)
        .unwrap();
    assert_eq!(s.get_rollout(R1).unwrap().unwrap(), r1);
    log!(out, "pending approval", s.get_rollout(R1).unwrap());
    log!(
        out,
        "schedule while pending",
        s.update_rollout_schedule(R1, "{}", T0 + 121).unwrap()
    );
    log!(
        out,
        "list pending",
        s.list_rollouts(Some("pending_approval"))
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>()
    );
    // 待核准、已排程的推送都算使用中（版本不能刪、主機不能再加入另一個推送）
    log!(
        out,
        "using release",
        s.rollouts_using_release(external).unwrap()
    );
    log!(out, "in active", s.hosts_in_active_rollouts().unwrap());
    log!(out, "offers while pending", s.active_offers().unwrap());
    // 退回：回到草稿、記下原因、刪除固定的對象
    let mut rejected = r1.clone();
    rejected.state = "draft".into();
    rejected.reject_reason = Some("時段不對，改到週末".into());
    rejected.updated_at = T0 + 130;
    {
        let tx = s.begin(TxMode::Immediate).unwrap();
        tx.update_rollout_state(&rejected).unwrap();
        log!(out, "delete hosts", tx.delete_rollout_hosts(R1).unwrap());
        log!(out, "delete again", tx.delete_rollout_hosts(R1).unwrap());
        tx.commit().unwrap();
    }
    assert!(s.rollout_hosts(R1).unwrap().is_empty());
    log!(out, "rejected", s.get_rollout(R1).unwrap());
    // 重新送出、由另一位核准：有 starts_at 時進入 scheduled
    let mut approved = rejected.clone();
    approved.state = "scheduled".into();
    approved.submitted_at = Some(T0 + 140);
    approved.submitted_by = Some("user:ann".into());
    approved.approved_at = Some(T0 + 150);
    approved.approved_by = Some("user:bob".into());
    approved.reject_reason = None;
    approved.updated_at = T0 + 150;
    s.update_rollout_state(&approved).unwrap();
    s.insert_rollout_hosts(R1, &[nh(a, 3), nh(b, 4)], T0 + 140)
        .unwrap();
    assert_eq!(s.get_rollout(R1).unwrap().unwrap(), approved);
    log!(out, "scheduled", s.get_rollout(R1).unwrap());
    log!(
        out,
        "list scheduled",
        s.list_rollouts(Some("scheduled"))
            .unwrap()
            .iter()
            .map(|r| (r.id.clone(), r.approved_by.clone()))
            .collect::<Vec<_>>()
    );
    log!(
        out,
        "all",
        s.list_rollouts(None)
            .unwrap()
            .iter()
            .map(|r| (r.id.clone(), r.state.clone()))
            .collect::<Vec<_>>()
    );
    log!(
        out,
        "in active scheduled",
        s.hosts_in_active_rollouts().unwrap()
    );
    // 開始：替換中的主機（applying_at 沒有時用 updated_at），不論推送之後的狀態
    assert!(s.applying_hosts().unwrap().is_empty());
    let mut running = approved.clone();
    running.state = "running".into();
    running.started_at = Some(T0 + 900);
    running.wave_started_at = Some(T0 + 900);
    running.updated_at = T0 + 900;
    s.update_rollout_state(&running).unwrap();
    s.update_rollout_host(
        R1,
        a,
        &HostTransition {
            state: Some("applying".into()),
            applying_at: Some(Some(T0 + 910)),
            ..Default::default()
        },
        T0 + 911,
    )
    .unwrap();
    s.update_rollout_host(
        R1,
        b,
        &HostTransition {
            state: Some("applying".into()),
            ..Default::default()
        },
        T0 + 920,
    )
    .unwrap();
    let applying = |s: &dyn crate::store::db::Store| {
        let mut rows = s.applying_hosts().unwrap();
        rows.sort();
        rows
    };
    log!(out, "applying", applying(s.as_ref()));
    let mut cancelled = running.clone();
    cancelled.state = "cancelled".into();
    cancelled.cancel_reason = Some("停止".into());
    cancelled.completed_at = Some(T0 + 930);
    cancelled.updated_at = T0 + 930;
    s.update_rollout_state(&cancelled).unwrap();
    log!(out, "applying after cancel", applying(s.as_ref()));
    s.update_rollout_host(
        R1,
        a,
        &HostTransition {
            state: Some("succeeded".into()),
            finished_at: Some(Some(T0 + 940)),
            ..Default::default()
        },
        T0 + 940,
    )
    .unwrap();
    log!(out, "applying left", applying(s.as_ref()));
    // 其他推送不受影響
    log!(
        out,
        "others",
        [R2, R3]
            .iter()
            .map(|id| s
                .get_rollout(id)
                .unwrap()
                .map(|r| (r.state, r.submitted_at)))
            .collect::<Vec<_>>()
    );
    assert_eq!(s.delete_rollout_hosts(R2).unwrap(), 0);
    out
}

/// ADR-0002 A7、A12 在儲存層的部分：(1) 對同一個待核准的推送，多條連線同時在序列化的寫入交易
/// （`TxMode::Immediate`）中「先檢查狀態再轉移」（核准、退回、取消、逾時退回），每一輪恰好一個成功、沒有
/// 半更新的狀態——SQLite 靠單一寫入者，PostgreSQL 靠交易層級的 advisory lock；(2) 雙人覆核政策的前值
/// （`meta` 表）與稽核在同一個交易中寫入，交易回復時兩者都不留下
fn rollout_review_transitions_and_policy_record(db: &TestDb) -> String {
    use crate::remote::policy;
    let mut out = String::new();
    seed_hosts(db);
    let s = db.handle();
    let release = s
        .insert_release(&ReleaseRow {
            id: 0,
            version: "0.10.0".into(),
            commit: "abc1234".into(),
            source: "hosted".into(),
            sha256sums: "sums".into(),
            signature: "sig".into(),
            signer_fingerprint: None,
            signer_principal: None,
            manifest_name: None,
            manifest: None,
            imported_at: T0,
            imported_by: "cli:root".into(),
            base_url: None,
        })
        .unwrap();
    let a = s.host_id_of(A).unwrap().unwrap();
    let b = s.host_id_of(B).unwrap().unwrap();
    const ROUNDS: i64 = 6;
    // 測試的連線池只有 4 條連線：這條加上三個同時操作的連線
    const WORKERS: usize = 3;
    let mut winners = Vec::new();
    for round in 0..ROUNDS {
        let id = format!("r-20261113-{:04}", round + 1);
        let at = T0 + round * 1000;
        s.insert_rollout(&RolloutRow {
            id: id.clone(),
            name: format!("推送 {id}"),
            release_id: release,
            target_version: "0.10.0".into(),
            selector: "{\"all\":true}".into(),
            waves: "[]".into(),
            schedule: "{}".into(),
            options: "{}".into(),
            state: "draft".into(),
            current_wave: 0,
            wave_started_at: None,
            soak_until: None,
            awaiting_continue: false,
            pause_reason: None,
            pause_cause: None,
            cancel_reason: None,
            created_at: at,
            created_by: "user:ann".into(),
            started_at: None,
            completed_at: None,
            updated_at: at,
            submitted_at: None,
            submitted_by: None,
            approved_at: None,
            approved_by: None,
            reject_reason: None,
        })
        .unwrap();
        let mut r = s.get_rollout(&id).unwrap().unwrap();
        r.state = "pending_approval".into();
        r.submitted_at = Some(at + 1);
        r.submitted_by = Some("user:ann".into());
        s.update_rollout_state(&r).unwrap();
        let nh = |host: i64, n: i64| NewRolloutHost {
            host_id: host,
            upgrade_id: format!("up-{round}-{n}"),
            wave: 0,
            ordinal: n,
            state: "pending".into(),
            reason: None,
            from_version: Some("0.9.0".into()),
        };
        s.insert_rollout_hosts(&id, &[nh(a, 1), nh(b, 2)], at + 1)
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(WORKERS));
        let handles: Vec<_> = (0..WORKERS)
            .map(|i| {
                let conn = db.handle_busy(std::time::Duration::from_secs(20));
                let (barrier, id) = (barrier.clone(), id.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    let tx = conn.begin(TxMode::Immediate).unwrap();
                    let mut r = tx.get_rollout(&id).unwrap().unwrap();
                    if r.state != "pending_approval" {
                        // 交易回復：輸的一方不寫任何東西（API 回 409 invalid_state）
                        return None;
                    }
                    let kind = match i {
                        // 核准
                        0 => {
                            r.state = "running".into();
                            r.approved_at = Some(at + 10);
                            r.approved_by = Some(format!("user:approver-{i}"));
                            r.started_at = Some(at + 10);
                            "approve"
                        }
                        // 退回（與待核准逾時的退回相同的轉移）
                        1 => {
                            tx.delete_rollout_hosts(&id).unwrap();
                            r.state = "draft".into();
                            r.submitted_at = None;
                            r.submitted_by = None;
                            r.reject_reason = Some("不行".into());
                            "reject"
                        }
                        // 取消
                        _ => {
                            r.state = "cancelled".into();
                            r.cancel_reason = Some("不做了".into());
                            r.completed_at = Some(at + 10);
                            "cancel"
                        }
                    };
                    r.updated_at = at + 10;
                    tx.update_rollout_state(&r).unwrap();
                    tx.commit().unwrap();
                    Some(kind)
                })
            })
            .collect();
        let won: Vec<&str> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(won.len(), 1, "第 {round} 輪：{won:?}");
        let r = s.get_rollout(&id).unwrap().unwrap();
        let hosts = s.rollout_hosts(&id).unwrap().len();
        // 最後的狀態與贏的那個操作一致
        let consistent = match won[0] {
            "approve" => {
                r.state == "running"
                    && r.approved_by.is_some()
                    && r.submitted_by.is_some()
                    && hosts == 2
            }
            "reject" => {
                r.state == "draft"
                    && r.approved_by.is_none()
                    && r.submitted_by.is_none()
                    && r.reject_reason.is_some()
                    && hosts == 0
            }
            _ => r.state == "cancelled" && r.approved_by.is_none() && hosts == 2,
        };
        assert!(consistent, "第 {round} 輪：{} {r:?}（{hosts} 台）", won[0]);
        winners.push(won.len());
        // 讓出主機給下一輪
        s.delete_rollout_hosts(&id).unwrap();
    }
    // 哪一個操作贏是不確定的；兩個後端的紀錄只比「每輪恰好一個」
    log!(out, "winners per round", winners);

    // 政策的前值與稽核：同一個交易
    let audits = |s: &dyn crate::store::db::Store| {
        let filter = crate::audit::AuditFilter {
            action: Some("settings.rollout_approval.".into()),
            ..Default::default()
        };
        let mut rows = s.audit_query(&filter, 50).unwrap().0;
        rows.sort_by_key(|e| e.id);
        rows.into_iter()
            .map(|e| (e.action, e.actor, e.at, e.object))
            .collect::<Vec<_>>()
    };
    {
        let tx = s.begin(TxMode::Immediate).unwrap();
        let (outcome, rec) = policy::observe(&*tx, "required", "file", "node-a", T0).unwrap();
        log!(out, "baseline", (format!("{outcome:?}"), rec.disabled_at));
        // 沒有提交：前值與稽核都不留下
    }
    log!(
        out,
        "after rollback",
        (s.get_meta(policy::META_KEY).unwrap(), audits(s.as_ref()))
    );
    for (value, source, at) in [
        ("required", "file", T0),
        ("required", "file", T0 + 1),
        ("none", "file", T0 + 2),
        ("none", "environment", T0 + 3),
        ("required", "environment", T0 + 4),
    ] {
        let tx = s.begin(TxMode::Immediate).unwrap();
        let (outcome, rec) = policy::observe(&*tx, value, source, "node-a", at).unwrap();
        tx.commit().unwrap();
        log!(
            out,
            "observe",
            (
                format!("{outcome:?}"),
                rec.value,
                rec.source,
                rec.changed_at,
                rec.disabled_at
            )
        );
    }
    log!(out, "record", s.get_meta(policy::META_KEY).unwrap());
    log!(out, "audit", audits(s.as_ref()));
    let report = s.audit_verify().unwrap();
    assert!(report.broken.is_none(), "{:?}", report.broken);
    log!(out, "chain", report.checked);
    out
}

backend_tests!(
    alerts,
    hosts_and_api_queries,
    remote_config_and_rollouts,
    rollout_schedule_approval_and_external_releases,
    rollout_review_transitions_and_policy_record
);
