//! report 寫入路徑（[`crate::store::ReportStore`]）在兩個後端上的行為：批次寫入與讀回、去重、失敗回報、
//! 主機清冊、彙總（寫出、遲到資料、重新開啟後補回）、計數器速率、保留期限、容量上限、資產、刪除主機資料

use chrono::{DateTime, TimeZone, Utc};
use std::time::Duration;

use super::{TestDb, backend_tests, log, set_now, test_clock};
use crate::protocol::{HostInfo, Metric};
use crate::registry::{AgentSnapshot, LastReport, RegistrySnapshot};
use crate::storage::{DedupKey, RecordBody, ReportRecord, WriteStatus, fmt_time};
use crate::store::AssetWrite;
use crate::store::codec;
use crate::store::postgres::conn::Value;
use crate::store::purge::PurgeJob;
use crate::store::sqlite::{CapacityEvent, FailPoint, META_CAPACITY_EVENT};

pub(super) const A: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
pub(super) const B: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

pub(super) fn noon() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap()
}

pub(super) fn legacy(agent: &str, seq: u64, recv_at: DateTime<Utc>, cpu: f32) -> ReportRecord {
    ReportRecord::new(
        recv_at,
        "10.0.0.1:5000".into(),
        agent.into(),
        format!("host-{}", &agent[..4]),
        seq,
        fmt_time(recv_at),
        RecordBody::Legacy {
            cpu_usage: cpu,
            total_memory: 4096,
            used_memory: 1024,
        },
    )
}

pub(super) fn metrics_record(
    agent: &str,
    seq: u64,
    recv_at: DateTime<Utc>,
    cpu: f64,
) -> ReportRecord {
    let metrics = vec![
        Metric::new(
            "system.cpu.usage",
            serde_json::Number::from_f64(cpu).unwrap(),
            "%",
        ),
        Metric::new("system.memory.total", 51_539_607_552_u64, "By"),
        Metric::new("system.memory.used", 38_833_356_800_u64, "By"),
        Metric::new("system.filesystem.total", 502_468_108_288_u64, "By")
            .with_label("mountpoint", "/")
            .with_label("fstype", "ext4"),
        Metric::new("system.filesystem.used", 161_061_273_600_u64, "By")
            .with_label("fstype", "ext4")
            .with_label("mountpoint", "/"),
    ];
    ReportRecord::new(
        recv_at,
        "10.0.0.2:5000".into(),
        agent.into(),
        "web-01".into(),
        seq,
        "not-rfc3339".into(),
        RecordBody::Metrics { metrics },
    )
}

pub(super) fn f4_record(
    agent: &str,
    seq: u64,
    recv_at: DateTime<Utc>,
    rx: u64,
    uptime: u64,
) -> ReportRecord {
    let metrics = vec![
        Metric::new("system.cpu.usage", 10, "%"),
        Metric::new("system.uptime", uptime, "s"),
        Metric::new("system.network.received", rx, "By").with_label("device", "eth0"),
        Metric::new("system.filesystem.total", 1000_u64, "By")
            .with_label("mountpoint", "/")
            .with_label("fstype", "ext4"),
        Metric::new("system.filesystem.available", 250_u64, "By")
            .with_label("mountpoint", "/")
            .with_label("fstype", "ext4"),
    ];
    ReportRecord::new(
        recv_at,
        "10.0.0.3:5000".into(),
        agent.into(),
        "f4".into(),
        seq,
        fmt_time(recv_at),
        RecordBody::Metrics { metrics },
    )
}

pub(super) fn dedup(boot: &str, seq: u64) -> Option<DedupKey> {
    Some(DedupKey {
        boot_id: boot.into(),
        report_seq: seq,
    })
}

fn durable(
    agent: &str,
    seq: u64,
    recv_at: DateTime<Utc>,
    ts: DateTime<Utc>,
    cpu: f32,
    report_seq: u64,
) -> ReportRecord {
    let mut r = legacy(agent, seq, recv_at, cpu);
    r.ts = fmt_time(ts);
    r.with_dedup(dedup("0b1c2d3e-0000-4000-8000-0000000000aa", report_seq))
}

/// 彙總表的內容（以序列名稱而不是編號表示：失敗的交易在 PostgreSQL 會消耗序列值）
pub(super) fn rollup_dump(db: &TestDb, table: &str) -> Vec<Vec<Value>> {
    db.rows(&format!(
        "SELECT h.agent_id, r.bucket, s.name, s.labels, s.derived, r.vmin, r.vmax, r.vsum, r.cnt
         FROM {table} r JOIN hosts h ON h.host_id = r.host_id JOIN series s ON s.series_id = r.series_id
         ORDER BY h.agent_id, r.bucket, s.name, s.labels, s.derived"
    ))
}

fn cpu_rollup(db: &TestDb, table: &str, bucket: DateTime<Utc>) -> Vec<Vec<Value>> {
    db.rows(&format!(
        "SELECT r.vmin, r.vmax, r.vsum, r.cnt FROM {table} r JOIN series s ON s.series_id = r.series_id
         WHERE s.name = 'system.cpu.usage' AND r.bucket = {}",
        bucket.timestamp()
    ))
}

fn agent_ts_sql(db: &TestDb) -> &'static str {
    if db.is_postgres() {
        "COALESCE(CAST(r.agent_ts_ms AS TEXT), r.agent_ts_text)"
    } else {
        "CAST(r.agent_ts AS TEXT)"
    }
}

fn reports_are_stored_and_read_back(db: &TestDb) -> String {
    let mut out = String::new();
    let (mut store, restored) = db.store(noon);
    assert!(restored.is_empty());
    let v1 = legacy(A, 6, noon(), 17.84);
    let v2 = metrics_record(B, 3, noon(), 17.84);
    assert_eq!(
        store.write_batch(&[v1, v2.clone()]),
        [WriteStatus::Written, WriteStatus::Written]
    );
    assert_eq!(
        store.write_batch(&[metrics_record(B, 4, noon(), 20.0)]),
        [WriteStatus::Written]
    );
    assert_eq!(db.int("SELECT count(*) FROM hosts"), 2);
    assert_eq!(db.int("SELECT count(*) FROM reports"), 3);
    assert_eq!(db.int("SELECT count(*) FROM series"), 5);
    let rows = log!(
        out,
        "reports",
        db.rows(&format!(
            "SELECT h.agent_id, r.ts, r.seq, {}, r.format, r.body, r.boot_ref, r.report_seq, r.recv_lag
             FROM reports r JOIN hosts h ON h.host_id = r.host_id ORDER BY r.id",
            agent_ts_sql(db)
        ))
    );
    // RFC 3339 毫秒的 ts 存成整數，其他原樣保存
    assert_eq!(
        rows[0][3],
        Value::Text(noon().timestamp_millis().to_string())
    );
    assert_eq!(rows[1][3], Value::Text("not-rfc3339".into()));
    let Value::Blob(body) = &rows[0][5] else {
        panic!("body 不是位元組")
    };
    assert_eq!(codec::decode_legacy(body), Some((17.84, 4096, 1024)));
    let Value::Blob(body) = &rows[1][5] else {
        panic!("body 不是位元組")
    };
    let decoded = codec::decode_metrics(body).unwrap();
    let RecordBody::Metrics { metrics } = &v2.body else {
        unreachable!()
    };
    for ((id, value), original) in decoded.iter().zip(metrics) {
        let s = db.rows(&format!(
            "SELECT name, unit, labels FROM series WHERE series_id = {id}"
        ));
        assert_eq!(s[0][0], Value::Text(original.name.clone()));
        assert_eq!(s[0][1], Value::Text(original.unit.clone()));
        assert_eq!(
            s[0][2],
            Value::Text(codec::canonical_labels(&original.labels))
        );
        assert_eq!(value.to_string(), original.value.to_string());
    }
    log!(
        out,
        "hosts",
        db.rows("SELECT agent_id, hostname, identity_verified, online, first_seen, last_seen, peer FROM hosts ORDER BY agent_id")
    );
    log!(
        out,
        "series",
        db.rows("SELECT series_id, name, unit, labels, derived FROM series ORDER BY series_id")
    );
    log!(
        out,
        "host_series",
        db.rows("SELECT host_id, series_id FROM host_series ORDER BY 1, 2")
    );
    // 一批 500 筆是一個交易；空批次沒有結果
    let batch: Vec<ReportRecord> = (0..500)
        .map(|i| legacy(if i % 2 == 0 { A } else { B }, i, noon(), 1.0))
        .collect();
    let results = store.write_batch(&batch);
    assert!(results.len() == 500 && results.iter().all(|r| *r == WriteStatus::Written));
    assert_eq!(db.int("SELECT count(*) FROM reports"), 503);
    assert!(store.write_batch(&[]).is_empty());
    // id 依寫入順序遞增
    log!(
        out,
        "order",
        db.rows("SELECT seq FROM reports WHERE id > 3 ORDER BY id LIMIT 20")
    );
    store.close();
    out
}

fn duplicates_are_acknowledged_but_stored_once(db: &TestDb) -> String {
    let mut out = String::new();
    let (mut store, _) = db.store(noon);
    let boot = "0b1c2d3e-0000-4000-8000-000000000001";
    let r = |agent, seq, rs| legacy(agent, seq, noon(), 1.0).with_dedup(dedup(boot, rs));
    assert_eq!(
        store.write_batch(&[r(A, 1, 1), r(A, 2, 2)]),
        [WriteStatus::Written, WriteStatus::Written]
    );
    assert_eq!(store.write_batch(&[r(A, 9, 2)]), [WriteStatus::Duplicate]);
    // 同一批內重複
    assert_eq!(
        store.write_batch(&[r(A, 10, 3), r(A, 11, 3)]),
        [WriteStatus::Written, WriteStatus::Duplicate]
    );
    // 同一批中：重複、新的、重複交錯
    assert_eq!(
        store.write_batch(&[
            r(A, 20, 1),
            r(A, 21, 4),
            r(A, 22, 4),
            r(A, 23, 5),
            r(A, 24, 2)
        ]),
        [
            WriteStatus::Duplicate,
            WriteStatus::Written,
            WriteStatus::Duplicate,
            WriteStatus::Written,
            WriteStatus::Duplicate
        ]
    );
    assert_eq!(store.write_batch(&[r(B, 1, 2)]), [WriteStatus::Written]);
    assert_eq!(
        store.write_batch(&[legacy(A, 12, noon(), 1.0).with_dedup(dedup("other-boot", 1))]),
        [WriteStatus::Written]
    );
    assert_eq!(store.write_batch(&[r(A, 13, 1)]), [WriteStatus::Duplicate]);
    assert_eq!(
        store.write_batch(&[legacy(A, 2, noon(), 1.0), legacy(A, 2, noon(), 1.0)]),
        [WriteStatus::Written, WriteStatus::Written]
    );
    assert_eq!(db.int("SELECT count(*) FROM reports"), 9);
    assert_eq!(db.int("SELECT count(*) FROM boots"), 3);
    // 重複的不加入彙總
    assert_eq!(store.rollup_len(), 2 * 3);
    assert_eq!(store.rollup_take_count(), 9 * 3);
    log!(
        out,
        "reports",
        db.rows("SELECT host_id, seq, boot_ref, report_seq FROM reports ORDER BY id")
    );
    // 重新開啟後（快取清空）仍能辨識
    store.close();
    drop(store);
    let (mut store, _) = db.store(noon);
    assert_eq!(store.write_batch(&[r(A, 14, 3)]), [WriteStatus::Duplicate]);
    store.close();
    out
}

fn failures_and_unknown_commits(db: &TestDb) -> String {
    let mut out = String::new();
    set_now(noon());
    let (mut store, _) = db.store(test_clock);
    assert_eq!(
        store.write_batch(&[legacy(A, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    // 提交前失敗：整批回復，新主機也不存在
    store.set_fail(FailPoint::BeforeCommit);
    let results = store.write_batch(&[legacy(A, 2, noon(), 1.0), legacy(B, 1, noon(), 1.0)]);
    assert!(
        results.iter().all(|r| matches!(r, WriteStatus::Failed(_))),
        "{results:?}"
    );
    assert_eq!(db.int("SELECT count(*) FROM reports"), 1);
    assert_eq!(db.int("SELECT count(*) FROM hosts"), 1);
    assert_eq!(
        store.write_batch(&[legacy(B, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    assert_eq!(db.int("SELECT count(*) FROM hosts"), 2);
    // 提交時磁碟已滿：確定未寫入
    store.set_fail(FailPoint::CommitDiskFull);
    let results = store.write_batch(&[legacy(A, 3, noon(), 1.0)]);
    assert!(matches!(&results[0], WriteStatus::Failed(_)), "{results:?}");
    assert_eq!(db.int("SELECT count(*) FROM reports"), 2);
    // 結果不明（實際已寫入）：重送的 durable report 判為重複，彙總只算一次
    store.set_fail(FailPoint::CommitUnknown);
    let d = |seq| legacy(A, seq, noon(), 40.0).with_dedup(dedup("boot-u", 1));
    let results = store.write_batch(&[d(4)]);
    assert!(
        matches!(&results[0], WriteStatus::Unknown(_)),
        "{results:?}"
    );
    assert_eq!(store.write_batch(&[d(5)]), [WriteStatus::Duplicate]);
    assert_eq!(
        store.write_batch(&[legacy(A, 6, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    assert_eq!(db.int("SELECT count(*) FROM reports"), 4);
    set_now(noon() + chrono::Duration::seconds(320));
    while store.maintain() {}
    log!(out, "rollup_5m", rollup_dump(db, "rollup_5m"));
    let cpu = cpu_rollup(db, "rollup_5m", noon());
    // A：1.0、40.0、1.0；B：1.0（各自一列）
    assert_eq!(cpu.len(), 2, "{cpu:?}");
    store.close();
    out
}

fn host_limit_rejects_only_new_hosts(db: &TestDb) -> String {
    let mut o = db.opts();
    o.max_hosts = 1;
    let (mut store, _) = db.store_with(o, noon);
    let results = store.write_batch(&[
        legacy(A, 1, noon(), 1.0),
        legacy(B, 1, noon(), 1.0),
        legacy(A, 2, noon(), 1.0),
    ]);
    assert_eq!(results[0], WriteStatus::Written);
    assert!(
        matches!(&results[1], WriteStatus::Failed(r) if r.contains("storage.max_hosts")),
        "{results:?}"
    );
    assert_eq!(results[2], WriteStatus::Written);
    store.close();
    format!("{results:?}")
}

fn agent_snapshot(agent: &str, online: bool, last_seen: DateTime<Utc>) -> AgentSnapshot {
    AgentSnapshot {
        agent_id: agent.into(),
        hostname: format!("host-{}", &agent[..4]),
        agent_version: "0.2.0".into(),
        proto: 2,
        host: Some(HostInfo {
            host_fingerprint: Some(format!("sha256:{}", "c".repeat(64))),
            display_name: Some("顯示名稱".into()),
            tags: Some(vec!["env:prod".into()]),
            report_interval_ms: Some(5000),
            ..Default::default()
        }),
        peer: "10.1.2.3:40000".into(),
        online,
        conn_id: 3,
        connected_at: fmt_time(noon() - chrono::Duration::hours(1)),
        last_seen: fmt_time(last_seen),
        offline_after_ms: 75_000,
        last_report: Some(LastReport {
            recv_at: fmt_time(last_seen),
            seq: 42,
            ts: "t".into(),
            cpu_usage: Some(12.5),
            total_memory: Some(4096),
            used_memory: Some(1024),
            metrics: Some(8),
            detail: Default::default(),
        }),
    }
}

fn snapshot(agents: Vec<AgentSnapshot>) -> RegistrySnapshot {
    RegistrySnapshot {
        generated_at: fmt_time(noon()),
        online: agents.iter().filter(|a| a.online).count(),
        total: agents.len(),
        agents,
    }
}

fn inventory_is_persisted_and_restored(db: &TestDb) -> String {
    let mut out = String::new();
    let (mut store, _) = db.store(noon);
    let old = noon() - chrono::Duration::days(4);
    store.sync_inventory(&snapshot(vec![
        agent_snapshot(A, true, noon()),
        agent_snapshot(B, false, old),
    ]));
    while store.maintain() {}
    let hosts = db.handle().list_hosts().unwrap();
    assert_eq!(hosts.len(), 2);
    assert_eq!(hosts[0].agent_id, B);
    assert!(!hosts[0].online);
    assert_eq!(hosts[0].last_seen, old);
    assert!(hosts[1].online && hosts[1].identity_verified);
    assert_eq!(hosts[1].display_name.as_deref(), Some("顯示名稱"));
    log!(out, "list_hosts", &hosts);
    // 模擬異常結束（沒有最後一次同步）
    drop(store);
    let (mut store, restored) = db.store(noon);
    assert_eq!(db.int("SELECT count(*) FROM hosts WHERE online = 1"), 0);
    assert_eq!(restored.len(), 1);
    let r = &restored[0];
    assert_eq!(r.agent_id, A);
    assert_eq!(r.hostname, "host-6f1c");
    assert_eq!(r.proto, 2);
    assert_eq!(r.last_seen, noon());
    assert_eq!(r.connected_at, noon() - chrono::Duration::hours(1));
    assert_eq!(r.offline_after, Duration::from_secs(75));
    let host = r.host.as_ref().unwrap();
    assert_eq!(host.tags, Some(vec!["env:prod".to_string()]));
    assert_eq!(host.report_interval_ms, Some(5000));
    assert_eq!(r.last_report.as_ref().map(|l| l.seq), Some(42));
    log!(
        out,
        "restored",
        format!("{:?} {:?} {:?}", r.peer, r.host, r.last_report)
    );

    // 只寫入有變化的主機
    let snap = snapshot(vec![agent_snapshot(A, true, noon())]);
    assert_eq!(store.sync_now(&snap), 1);
    assert_eq!(store.sync_now(&snap), 0);
    let later = |s: i64, online: bool| {
        snapshot(vec![agent_snapshot(
            A,
            online,
            noon() + chrono::Duration::seconds(s),
        )])
    };
    assert_eq!(store.sync_now(&later(59, true)), 0);
    assert_eq!(store.sync_now(&later(60, true)), 1);
    assert_eq!(store.sync_now(&later(61, false)), 1);
    assert_eq!(
        db.int(&format!("SELECT online FROM hosts WHERE agent_id = '{A}'")),
        0
    );
    // v1 重新連線（沒有主機資訊）：保留上次的指紋
    let mut v1 = agent_snapshot(A, true, noon() + chrono::Duration::seconds(70));
    v1.host = None;
    v1.proto = 1;
    assert_eq!(store.sync_now(&snapshot(vec![v1])), 1);
    // 先有 report 才同步：同一列
    let c = "cccccccc-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    assert_eq!(
        store.write_batch(&[legacy(c, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    assert_eq!(
        store.sync_now(&snapshot(vec![agent_snapshot(c, true, noon())])),
        1
    );
    assert_eq!(db.int("SELECT count(*) FROM hosts"), 3);
    // 清冊已滿時不新增
    store.set_max_hosts(3);
    let d = "dddddddd-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    assert_eq!(
        store.sync_now(&snapshot(vec![agent_snapshot(d, true, noon())])),
        0
    );
    log!(
        out,
        "hosts",
        db.rows(
            "SELECT agent_id, hostname, agent_version, proto, host_fingerprint, display_name, tags, os,
                    report_interval_ms, peer, identity_verified, online, first_seen, connected_at, last_seen,
                    offline_after_ms, last_report FROM hosts ORDER BY agent_id"
        )
    );
    store.close();
    out
}

fn rollups_are_flushed_and_recovered(db: &TestDb) -> String {
    let mut out = String::new();
    set_now(noon());
    let (mut store, _) = db.store(test_clock);
    for (i, cpu) in [10.0, 30.0, 20.0].into_iter().enumerate() {
        let t = noon() + chrono::Duration::seconds(i as i64 * 60);
        assert_eq!(
            store.write_batch(&[legacy(A, i as u64, t, cpu)]),
            [WriteStatus::Written]
        );
    }
    store.write_batch(&[legacy(A, 9, noon() + chrono::Duration::minutes(5), 50.0)]);
    // 桶尚未結束＋寬限：不寫出
    set_now(noon() + chrono::Duration::seconds(300 + 14));
    assert!(!store.maintain());
    assert_eq!(db.int("SELECT count(*) FROM rollup_5m"), 0);
    set_now(noon() + chrono::Duration::seconds(300 + 15));
    assert!(store.maintain());
    assert_eq!(
        cpu_rollup(db, "rollup_5m", noon()),
        vec![vec![
            Value::Real(10.0),
            Value::Real(30.0),
            Value::Real(60.0),
            Value::Integer(3)
        ]]
    );
    assert_eq!(
        cpu_rollup(db, "rollup_1h", noon()),
        cpu_rollup(db, "rollup_5m", noon())
    );
    assert_eq!(
        db.real("SELECT r.vmax FROM rollup_5m r JOIN series s ON s.series_id = r.series_id WHERE s.name = 'system.memory.total'"),
        4096.0 * 1024.0 * 1024.0
    );
    assert_eq!(
        db.int("SELECT rollup_until FROM hosts"),
        (noon() + chrono::Duration::minutes(5)).timestamp_millis()
    );
    // 第二個桶寫出後合併進同一小時
    set_now(noon() + chrono::Duration::seconds(600 + 15));
    store.maintain();
    log!(out, "5m", rollup_dump(db, "rollup_5m"));
    log!(out, "1h", rollup_dump(db, "rollup_1h"));
    assert_eq!(cpu_rollup(db, "rollup_1h", noon())[0][3], Value::Integer(4));

    // 第三個桶：寫入原始資料後「當掉」（記憶體中的彙總遺失）
    let t2 = noon() + chrono::Duration::minutes(10);
    store.write_batch(&[legacy(A, 3, t2, 30.0), metrics_record(A, 4, t2, 40.0)]);
    drop(store);
    let (mut store, _) = db.store(test_clock);
    // 從 rollup_until 起補回，不含已寫出的桶
    assert_eq!(store.rollup_len(), 5);
    set_now(noon() + chrono::Duration::minutes(16));
    while store.maintain() {}
    log!(out, "5m after recovery", rollup_dump(db, "rollup_5m"));
    log!(out, "1h after recovery", rollup_dump(db, "rollup_1h"));
    let hour = cpu_rollup(db, "rollup_1h", noon());
    assert_eq!(hour[0][3], Value::Integer(6), "每筆只算一次");
    assert_eq!(hour[0][2], Value::Real(180.0));
    store.close();
    out
}

fn late_samples_are_merged_by_sample_time(db: &TestDb) -> String {
    let mut out = String::new();
    set_now(noon());
    let (mut store, _) = db.store(test_clock);
    store.write_batch(&[legacy(A, 1, noon(), 10.0)]);
    store.write_batch(&[legacy(A, 2, noon() + chrono::Duration::seconds(60), 30.0)]);
    let now = noon() + chrono::Duration::seconds(320);
    set_now(now);
    while store.maintain() {}
    // 補送：取樣時間 12:01（桶已寫出）與 11:00（更早，還沒有這個桶）；同一批中同一個桶兩筆
    let t1 = noon() + chrono::Duration::seconds(60);
    let t0 = noon() - chrono::Duration::hours(1);
    assert_eq!(
        store.write_batch(&[
            durable(A, 3, now, t1, 50.0, 1),
            durable(A, 4, now, t0, 5.0, 2),
            durable(A, 5, now, t1 + chrono::Duration::seconds(1), 2.5, 3),
        ]),
        [
            WriteStatus::Written,
            WriteStatus::Written,
            WriteStatus::Written
        ]
    );
    assert_eq!(
        cpu_rollup(db, "rollup_5m", noon()),
        vec![vec![
            Value::Real(2.5),
            Value::Real(50.0),
            Value::Real(92.5),
            Value::Integer(4)
        ]]
    );
    assert_eq!(
        cpu_rollup(db, "rollup_1h", noon()),
        cpu_rollup(db, "rollup_5m", noon())
    );
    assert_eq!(
        cpu_rollup(db, "rollup_5m", t0),
        vec![vec![
            Value::Real(5.0),
            Value::Real(5.0),
            Value::Real(5.0),
            Value::Integer(1)
        ]]
    );
    assert_eq!(store.rollup_len(), 0);
    assert_eq!(
        db.int("SELECT ts FROM reports WHERE report_seq = 1"),
        t1.timestamp_millis()
    );
    log!(
        out,
        "reports",
        db.rows("SELECT seq, ts, recv_lag, report_seq FROM reports ORDER BY id")
    );
    // 重送：重複的不再併入
    assert_eq!(
        store.write_batch(&[durable(A, 6, now, t1, 50.0, 1)]),
        [WriteStatus::Duplicate]
    );
    log!(out, "5m", rollup_dump(db, "rollup_5m"));
    log!(out, "1h", rollup_dump(db, "rollup_1h"));
    store.close();
    out
}

fn counters_become_rates(db: &TestDb) -> String {
    let mut out = String::new();
    set_now(noon());
    let (mut store, _) = db.store(test_clock);
    let at = |s: i64| noon() + chrono::Duration::seconds(s);
    // 5 秒 5000 bytes → 1000 B/s；第 3 筆重新開機 → 沒有速率；第 4 筆 500 B/s
    for (i, (rx, up)) in [(1000, 100), (6000, 105), (6100, 2), (8600, 7)]
        .into_iter()
        .enumerate()
    {
        let r = f4_record(A, i as u64, at(i as i64 * 5), rx, up);
        assert_eq!(store.write_batch(&[r]), [WriteStatus::Written]);
    }
    // 重新啟動後：上一筆從原始資料取得，下一筆仍算得出速率（5 秒 2500 bytes → 500 B/s）
    drop(store);
    let (mut store, _) = db.store(test_clock);
    assert_eq!(
        store.write_batch(&[f4_record(A, 9, at(20), 11_100, 12)]),
        [WriteStatus::Written]
    );
    // 同一批兩筆：批內相鄰的也算
    assert_eq!(
        store.write_batch(&[
            f4_record(A, 10, at(25), 12_100, 17),
            f4_record(A, 11, at(30), 13_100, 22)
        ]),
        [WriteStatus::Written, WriteStatus::Written]
    );
    set_now(noon() + chrono::Duration::seconds(320));
    while store.maintain() {}
    let of = |name: &str, derived: i64| {
        db.rows(&format!(
            "SELECT COALESCE(SUM(r.cnt), 0), COALESCE(SUM(r.vsum), 0.0) FROM rollup_5m r
             JOIN series s ON s.series_id = r.series_id WHERE s.name = '{name}' AND s.derived = {derived}"
        ))
    };
    assert_eq!(of("system.network.received", 0)[0][0], Value::Integer(0));
    let rate = of("system.network.received", 1);
    assert_eq!(rate[0][0], Value::Integer(5), "{rate:?}");
    assert_eq!(
        rate[0][1],
        Value::Real(1000.0 + 500.0 + 500.0 + 200.0 + 200.0)
    );
    assert_eq!(
        db.text("SELECT unit FROM series WHERE name = 'system.network.received' AND derived = 1")
            .as_deref(),
        Some("By/s")
    );
    assert_eq!(of("system.filesystem.usage", 2)[0][0], Value::Integer(7));
    log!(out, "5m", rollup_dump(db, "rollup_5m"));
    log!(
        out,
        "series",
        db.rows("SELECT name, unit, labels, derived FROM series ORDER BY series_id")
    );
    store.close();
    out
}

fn retention_deletes_expired_rows(db: &TestDb) -> String {
    let mut out = String::new();
    set_now(noon());
    let mut o = db.opts();
    o.raw_retention_days = 2;
    o.rollup_5m_retention_days = 3;
    o.rollup_1h_retention_days = 4;
    let (mut store, _) = db.store_with(o.clone(), test_clock);
    let list: kairos_proto::ProcessList = serde_json::from_value(serde_json::json!({
        "top_cpu": [{"pid": 1, "name": "init", "cpu_usage": 1.5, "memory": 10}],
    }))
    .unwrap();
    for d in 0..=6 {
        let t = noon() - chrono::Duration::days(d);
        let r = f4_record(A, d as u64, t, 1, 1)
            .with_dedup(dedup(&format!("boot-{d}"), 1))
            .with_processes(Some(list.clone()));
        assert_eq!(store.write_batch(&[r.clone()]), [WriteStatus::Written]);
        // durable 重送：重複的 report 不寫出重複的快照
        assert_eq!(store.write_batch(&[r]), [WriteStatus::Duplicate]);
        for table in ["rollup_5m", "rollup_1h"] {
            db.exec(&format!(
                "INSERT INTO {table} VALUES (1, {}, 1, 1, 1, 1, 1)",
                t.timestamp()
            ));
        }
    }
    assert_eq!(db.int("SELECT count(*) FROM process_snapshots"), 7);
    let snap = db.rows(&format!(
        "SELECT body FROM process_snapshots WHERE ts = {}",
        noon().timestamp_millis()
    ));
    let Value::Blob(body) = &snap[0][0] else {
        panic!("body 不是位元組")
    };
    let back: kairos_proto::ProcessList = serde_json::from_slice(body).unwrap();
    assert_eq!(back, list);
    store.rollup_clear();
    store.defer_capacity();
    while store.maintain() {}
    // 今天與前 N 天保留，其餘刪除
    assert_eq!(db.int("SELECT count(*) FROM reports"), 3);
    assert_eq!(db.int("SELECT count(*) FROM process_snapshots"), 3);
    assert_eq!(db.int("SELECT count(*) FROM rollup_5m"), 4);
    assert_eq!(db.int("SELECT count(*) FROM rollup_1h"), 5);
    assert_eq!(
        db.int("SELECT min(ts) FROM reports"),
        (noon() - chrono::Duration::days(2)).timestamp_millis()
    );
    // 沒有 report 引用的 boot 也刪除
    assert_eq!(db.int("SELECT count(*) FROM boots"), 3);
    log!(
        out,
        "retention_last",
        db.text("SELECT value FROM meta WHERE key = 'retention_last'")
    );
    // 時鐘被誤校到一年後：不刪除（DateGuard），重新啟動後也一樣
    store.reset_retention();
    set_now(noon() + chrono::Duration::days(365));
    while store.maintain() {}
    assert_eq!(db.int("SELECT count(*) FROM reports"), 3);
    drop(store);
    let (mut store, _) = db.store_with(o, test_clock);
    store.defer_capacity();
    while store.maintain() {}
    assert_eq!(db.int("SELECT count(*) FROM reports"), 3);
    store.close();
    out
}

/// 容量上限：兩個後端的規則不同（PostgreSQL 刪除後檔案不會變小，以估計量決定刪多少），共同的性質是
/// 「從最舊的原始資料刪起、有刪除、沒有刪光、事件記在 meta、主機清冊不刪」
fn capacity_limit_deletes_the_oldest_first(db: &TestDb) -> String {
    let mut o = db.opts();
    o.raw_retention_days = 0;
    o.rollup_5m_retention_days = 0;
    o.rollup_1h_retention_days = 0;
    let (mut store, _) = db.store_with(o, noon);
    for chunk in 0..20 {
        let batch: Vec<ReportRecord> = (0..1000)
            .map(|i| metrics_record(A, chunk * 1000 + i, noon(), 1.0))
            .collect();
        store.write_batch(&batch);
    }
    db.exec("INSERT INTO rollup_5m VALUES (1, 0, 1, 1, 1, 1, 1)");
    let used = store.used();
    let cap = used / 4 * 3;
    store.set_max_total_bytes(cap);
    store.rollup_clear();
    store.reset_capacity();
    while store.maintain() {}
    let event: CapacityEvent = serde_json::from_str(
        &db.text(&format!(
            "SELECT value FROM meta WHERE key = '{META_CAPACITY_EVENT}'"
        ))
        .expect("容量事件應該記在 meta"),
    )
    .unwrap();
    assert_eq!(event.limit_bytes, cap);
    assert_eq!(event.started_at, noon().timestamp_millis());
    assert_eq!(event.ended_at, Some(noon().timestamp_millis()));
    assert!(event.deleted_raw >= 3000, "{event:?}");
    assert!(!event.exhausted, "{event:?}");
    // 刪除的是最舊的，最新的保留；原始資料還有剩時不刪彙總
    let left = db.int("SELECT count(*) FROM reports");
    assert!(left > 5000 && left < 20_000, "剩下 {left} 列");
    assert!(db.int("SELECT min(seq) FROM reports") > 0);
    assert_eq!(db.int("SELECT max(seq) FROM reports"), 19_999);
    assert_eq!(db.int("SELECT count(*) FROM rollup_5m"), 1);
    // 沒有新資料時不會再刪一輪
    store.reset_capacity();
    while store.maintain() {}
    if !db.is_postgres() {
        assert_eq!(db.int("SELECT count(*) FROM reports"), left);
    }
    assert_eq!(db.int("SELECT count(*) FROM hosts"), 1);
    store.close();
    String::new()
}

fn per_host_series_limit(db: &TestDb) -> String {
    let mut out = String::new();
    let mut o = db.opts();
    o.max_series_per_host = 4;
    let (mut store, _) = db.store_with(o, noon);
    assert_eq!(
        store.write_batch(&[f4_record(A, 1, noon(), 1, 1)]),
        [WriteStatus::Written]
    );
    assert_eq!(db.int("SELECT count(*) FROM host_series"), 4);
    let rows = db.rows("SELECT body FROM reports");
    let Value::Blob(body) = &rows[0][0] else {
        panic!("body 不是位元組")
    };
    assert_eq!(codec::decode_metrics(body).unwrap().len(), 4);
    assert_eq!(
        db.int("SELECT count(*) FROM hosts WHERE series_limited_at IS NOT NULL"),
        1
    );
    assert_eq!(
        store.write_batch(&[f4_record(B, 1, noon(), 1, 1)]),
        [WriteStatus::Written]
    );
    assert_eq!(db.int("SELECT count(*) FROM host_series"), 8);
    log!(
        out,
        "host_series",
        db.rows("SELECT host_id, series_id FROM host_series ORDER BY 1, 2")
    );
    store.close();
    out
}

fn assets_are_merged(db: &TestDb) -> String {
    let mut out = String::new();
    let (mut store, _) = db.store(noon);
    let inv =
        |v: serde_json::Value| -> kairos_proto::Inventory { serde_json::from_value(v).unwrap() };
    let w = |ms: i64, i: kairos_proto::Inventory| AssetWrite {
        agent_id: A.into(),
        hostname: "web-01".into(),
        peer: "10.0.0.1:1".into(),
        collected_ms: ms,
        received_ms: ms,
        inventory: i,
    };
    let first = inv(serde_json::json!({
        "os": {"family": "linux", "long_version": "Linux (Ubuntu 24.04)"},
        "boot_time": "2026-10-02T08:11:04Z",
        "cpu": {"model": "Xeon", "logical_cores": 8},
        "memory": {"total": 8_589_934_592_u64},
        "system": {"vendor": "QEMU", "product": "Standard PC", "serial": "S1"}
    }));
    let o = log!(
        out,
        "first",
        store.write_assets(&w(1000, first.clone())).unwrap()
    );
    assert!(o.first && !o.changed);
    assert_eq!(o.logical_cores, Some(8));
    assert_eq!(db.int("SELECT count(*) FROM inventory_changes"), 0);
    assert_eq!(db.int("SELECT boot_time FROM hosts"), 1_790_928_664);
    let mut same = first.clone();
    same.boot_time = Some("2026-10-03T00:00:00Z".into());
    let o = log!(out, "same", store.write_assets(&w(2000, same)).unwrap());
    assert!(!o.first && !o.changed);
    let o = log!(
        out,
        "changed",
        store
            .write_assets(&w(
                3000,
                inv(serde_json::json!({"memory": {"total": 17_179_869_184_u64}, "system": {"vendor": "QEMU"}})),
            ))
            .unwrap()
    );
    assert!(o.changed);
    assert_eq!(o.history, 1);
    log!(
        out,
        "changes",
        db.rows("SELECT host_id, changed_at, changes FROM inventory_changes ORDER BY id")
    );
    log!(
        out,
        "inventory",
        db.rows("SELECT serial, memory_total, changed_at FROM host_inventory")
    );
    log!(
        out,
        "host",
        db.rows("SELECT agent_id, hostname, os, boot_time FROM hosts")
    );
    // 清冊已滿時拒絕新主機的資產
    store.close();
    drop(store);
    let mut o = db.opts();
    o.max_hosts = 1;
    let (mut store, _) = db.store_with(o, noon);
    let mut other = w(4000, first);
    other.agent_id = B.into();
    assert!(store.write_assets(&other).is_err());
    store.close();
    out
}

fn purging_a_host_keeps_the_others(db: &TestDb) -> String {
    let mut out = String::new();
    set_now(noon());
    let (mut store, _) = db.store(test_clock);
    let list: kairos_proto::ProcessList = serde_json::from_value(serde_json::json!({
        "top_cpu": [{"pid": 1, "name": "x", "cpu_usage": 1.0, "memory": 1}],
    }))
    .unwrap();
    for agent in [A, B] {
        for i in 0..30 {
            let t = noon() + chrono::Duration::seconds(i * 20);
            let r = f4_record(agent, i as u64, t, 100 * i as u64, 10 + i as u64)
                .with_dedup(dedup("boot-x", i as u64))
                .with_processes(Some(list.clone()));
            assert_eq!(store.write_batch(&[r]), [WriteStatus::Written]);
        }
        store
            .write_assets(&AssetWrite {
                agent_id: agent.into(),
                hostname: "f4".into(),
                peer: "10.0.0.3:1".into(),
                collected_ms: noon().timestamp_millis(),
                received_ms: noon().timestamp_millis(),
                inventory: serde_json::from_value(serde_json::json!({"system": {"serial": "S1"}}))
                    .unwrap(),
            })
            .unwrap();
        store
            .write_assets(&AssetWrite {
                agent_id: agent.into(),
                hostname: "f4".into(),
                peer: "10.0.0.3:1".into(),
                collected_ms: noon().timestamp_millis() + 1,
                received_ms: noon().timestamp_millis() + 1,
                inventory: serde_json::from_value(serde_json::json!({"system": {"serial": "S2"}}))
                    .unwrap(),
            })
            .unwrap();
    }
    set_now(noon() + chrono::Duration::minutes(20));
    while store.maintain() {}
    let now_ms = test_clock().timestamp_millis();
    let plan = log!(out, "plan", db.handle().purge_plan(A, now_ms).unwrap());
    assert!(plan.reports == 30 && plan.rollup_5m > 0, "{plan:?}");
    let mut job = PurgeJob::new(A, "tester", Some("127.0.0.1"), Some("測試"));
    let mut steps = 0;
    while !store.purge_step(&mut job).unwrap() {
        steps += 1;
        assert!(steps < 1000);
    }
    log!(out, "counts", &job.counts);
    assert_eq!(job.counts, plan);
    for table in [
        "reports",
        "process_snapshots",
        "rollup_5m",
        "rollup_1h",
        "boots",
        "host_series",
        "host_inventory",
        "inventory_changes",
    ] {
        let left = db.rows(&format!(
            "SELECT h.agent_id, count(*) FROM {table} t JOIN hosts h ON h.host_id = t.host_id GROUP BY h.agent_id ORDER BY 1"
        ));
        assert_eq!(left.len(), 1, "{table}: {left:?}");
        assert_eq!(left[0][0], Value::Text(B.into()), "{table}");
        log!(out, table, left);
    }
    assert_eq!(db.int("SELECT count(*) FROM hosts"), 1);
    // 稽核紀錄（稽核日誌不刪除）
    log!(
        out,
        "audit",
        db.rows("SELECT id, actor, actor_ip, category, action, object, result, code, details, prev_hash, hash FROM audit_events ORDER BY id")
    );
    // 刪除後的一段時間內，這台主機的 report 被拒，另一台照常
    let results = store.write_batch(&[
        f4_record(A, 99, test_clock(), 1, 1),
        f4_record(B, 99, test_clock(), 9000, 2000),
    ]);
    assert!(matches!(&results[0], WriteStatus::Failed(_)), "{results:?}");
    assert_eq!(results[1], WriteStatus::Written);
    assert_eq!(db.int("SELECT count(*) FROM hosts"), 1);
    // 不存在的主機：立即完成，計數為 0
    let mut none = PurgeJob::new("ffffffff-bbbb-4ccc-8ddd-eeeeeeeeeeee", "tester", None, None);
    let mut done = store.purge_step(&mut none);
    for _ in 0..10 {
        if !matches!(done, Ok(false)) {
            break;
        }
        done = store.purge_step(&mut none);
    }
    log!(out, "missing host", (&done, none.counts.total()));
    store.close();
    out
}

backend_tests!(
    reports_are_stored_and_read_back,
    duplicates_are_acknowledged_but_stored_once,
    failures_and_unknown_commits,
    host_limit_rejects_only_new_hosts,
    inventory_is_persisted_and_restored,
    rollups_are_flushed_and_recovered,
    late_samples_are_merged_by_sample_time,
    counters_become_rates,
    retention_deletes_expired_rows,
    capacity_limit_deletes_the_oldest_first,
    per_host_series_limit,
    assets_are_merged,
    purging_a_host_keeps_the_others,
);
