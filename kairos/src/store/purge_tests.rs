//! 刪除主機資料（[`super`]）的單元測試：以實際的 [`SqliteStore`] 寫入兩台主機的資料，刪除其中一台，
//! 確認另一台不受影響、計數與試執行相同、憑證的保留規則、稽核紀錄，以及刪除後的寫入被拒

use super::*;
use crate::config::SqliteSynchronous;
use crate::protocol::Metric;
use crate::registry::next_conn_id;
use crate::storage::{RecordBody, ReportRecord, WriteStatus};
use crate::store::ReportStore;
use crate::store::prelude::*;
use crate::store::sqlite::{DB_FILE, SqliteOptions, SqliteStore};
use chrono::{DateTime, TimeZone, Utc};
use std::path::{Path, PathBuf};

const A: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
const B: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const DAY: i64 = 86_400_000;

fn noon() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kairos-purge-{tag}-{}-{}",
        std::process::id(),
        next_conn_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn opts(dir: &Path) -> SqliteOptions {
    SqliteOptions {
        path: dir.join(DB_FILE),
        synchronous: SqliteSynchronous::Normal,
        raw_retention_days: 7,
        rollup_5m_retention_days: 31,
        rollup_1h_retention_days: 400,
        max_total_bytes: 0,
        max_hosts: 1000,
        max_series_per_host: crate::config::DEFAULT_MAX_SERIES_PER_HOST,
        mtls: true,
    }
}

fn record(agent: &str, seq: u64, at: DateTime<Utc>) -> ReportRecord {
    ReportRecord::new(
        at,
        "10.0.0.2:5000".into(),
        agent.into(),
        format!("host-{}", &agent[..4]),
        seq,
        crate::storage::fmt_time(at),
        RecordBody::Metrics {
            metrics: vec![Metric::new("system.cpu.usage", 12, "%")],
        },
    )
    .with_processes(Some(kairos_proto::ProcessList {
        top_cpu: vec![kairos_proto::ProcessEntry {
            pid: 1,
            name: "x".into(),
            cpu_usage: 1.0,
            memory: 1,
            user: None,
            command: None,
        }],
        top_memory: Vec::new(),
    }))
}

fn asset(agent: &str, serial: &str) -> crate::store::AssetWrite {
    crate::store::AssetWrite {
        agent_id: agent.into(),
        hostname: format!("host-{}", &agent[..4]),
        peer: "10.0.0.2:5000".into(),
        collected_ms: noon().timestamp_millis(),
        received_ms: noon().timestamp_millis(),
        inventory: kairos_proto::Inventory {
            system: Some(kairos_proto::SystemInventory {
                serial: Some(serial.into()),
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

fn exec(dir: &Path, sql: &str, p: impl rusqlite::Params) {
    let c = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
    c.execute(sql, p).unwrap();
}

fn count(dir: &Path, sql: &str, p: impl rusqlite::Params) -> i64 {
    let c = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
    c.query_row(sql, p, |r| r.get(0)).unwrap()
}

/// 兩台主機：report、行程快照、資產（兩次，有一筆變更）、彙總、告警事件、註冊申請、憑證
fn populate(store: &mut SqliteStore, dir: &Path) {
    let t = noon();
    let recs: Vec<ReportRecord> = (1..=5)
        .flat_map(|i| {
            let at = t + chrono::Duration::seconds(i * 5);
            [record(A, i as u64, at), record(B, i as u64, at)]
        })
        .collect();
    assert!(
        store
            .write_batch(&recs)
            .iter()
            .all(|s| *s == WriteStatus::Written)
    );
    for a in [A, B] {
        store.write_assets(&asset(a, "S1")).unwrap();
        store.write_assets(&asset(a, "S2")).unwrap();
    }
    let now = t.timestamp_millis();
    for (i, a) in [A, B].iter().enumerate() {
        let host: i64 = count(dir, "SELECT host_id FROM hosts WHERE agent_id = ?1", [a]);
        exec(
            dir,
            "INSERT INTO rollup_5m (host_id, bucket, series_id, vmin, vmax, vsum, cnt) VALUES (?1, 1, 1, 1, 1, 1, 1)",
            [host],
        );
        exec(
            dir,
            "INSERT INTO rollup_1h (host_id, bucket, series_id, vmin, vmax, vsum, cnt) VALUES (?1, 1, 1, 1, 1, 1, 1)",
            [host],
        );
        let ev = format!("E-20261009-000{i}");
        exec(
            dir,
            "INSERT INTO alert_events (event_id, rule_id, dedup_key, kind, category, severity, sev_rank,
                attention, state, title, subject, agent_id, details, started_at, updated_at, last_signal_at, notify_state)
             VALUES (?1, 'host_offline', ?2, 'host', 'host', 'critical', 3, 'none', 'firing', 'host-x 離線', ?3, ?3, '{}', ?4, ?4, ?4, '{}')",
            rusqlite::params![ev, format!("host_offline|{a}"), a, now],
        );
        exec(
            dir,
            "INSERT INTO alert_timeline (event_id, at, actor, action) VALUES (?1, ?2, 'system', 'opened')",
            rusqlite::params![ev, now],
        );
        exec(
            dir,
            "INSERT INTO alert_deliveries (event_id, channel_id, kind, state, payload, created_at, next_attempt_at)
             VALUES (?1, 'c1', 'firing', 'sent', '{\"host\":\"host-x\"}', ?2, ?2)",
            rusqlite::params![ev, now],
        );
        exec(
            dir,
            "INSERT INTO enroll_requests (request_id, kind, agent_id, request_code, state, csr_der, csr_sha256,
                spki_sha256, source_ip, hostname, created_at, expires_at)
             VALUES (?1, 'enroll', ?2, 'ABCD1234', 'issued', x'00', x'01', x'02', '10.0.0.2', 'host-x', ?3, ?3)",
            rusqlite::params![format!("req-{i}"), a, now],
        );
    }
    // A 的憑證：有效（未撤銷）、已撤銷未過期（CRL 需要）、已過期很久
    for (serial, revoked, not_after) in [
        ("AA01", None, now + 300 * DAY),
        ("AA02", Some(now - DAY), now + 100 * DAY),
        ("AA03", None, now - 30 * DAY),
    ] {
        exec(
            dir,
            "INSERT INTO pki_certificates (serial, ca_id, agent_id, spki_sha256, not_before, not_after, cert_der,
                issued_at, issued_via, revoked_at, revoke_effective_at, revocation_reason)
             VALUES (?1, 1, ?2, x'03', ?3, ?4, x'04', ?3, 'token', ?5, ?5, CASE WHEN ?5 IS NULL THEN NULL ELSE 'superseded' END)",
            rusqlite::params![serial, A, now - 400 * DAY, not_after, revoked],
        );
    }
    exec(
        dir,
        "INSERT INTO pki_certificates (serial, ca_id, agent_id, spki_sha256, not_before, not_after, cert_der, issued_at, issued_via)
         VALUES ('BB01', 1, ?1, x'05', ?2, ?3, x'06', ?2, 'token')",
        rusqlite::params![B, now - DAY, now + 300 * DAY],
    );
}

fn table_counts(dir: &Path, agent: &str) -> Vec<i64> {
    let host: Option<i64> = rusqlite::Connection::open(dir.join(DB_FILE))
        .unwrap()
        .query_row(
            "SELECT host_id FROM hosts WHERE agent_id = ?1",
            [agent],
            |r| r.get(0),
        )
        .ok();
    let h = host.unwrap_or(-1);
    let mut v: Vec<i64> = [
        "reports",
        "process_snapshots",
        "rollup_5m",
        "rollup_1h",
        "host_inventory",
        "inventory_changes",
        "host_series",
    ]
    .iter()
    .map(|t| {
        count(
            dir,
            &format!("SELECT COUNT(*) FROM {t} WHERE host_id = ?1"),
            [h],
        )
    })
    .collect();
    for t in [
        "alert_events",
        "enroll_requests",
        "pki_certificates",
        "hosts",
    ] {
        v.push(count(
            dir,
            &format!("SELECT COUNT(*) FROM {t} WHERE agent_id = ?1"),
            [agent],
        ));
    }
    v
}

#[test]
fn purging_one_host_deletes_its_rows_keeps_the_other_and_audits() {
    let dir = temp_dir("all");
    let (mut store, _) = SqliteStore::open(opts(&dir), noon).unwrap();
    populate(&mut store, &dir);
    let b_before = table_counts(&dir, B);
    let now = noon().timestamp_millis();
    let planned = {
        let c = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
        plan(&c, A, now).unwrap()
    };
    assert_eq!(planned.reports, 5);
    assert_eq!(planned.process_snapshots, 5);
    assert_eq!((planned.rollup_5m, planned.rollup_1h), (1, 1));
    assert_eq!((planned.inventory, planned.inventory_changes), (1, 1));
    assert_eq!(
        (
            planned.alert_events,
            planned.alert_timeline,
            planned.alert_deliveries
        ),
        (1, 1, 1)
    );
    assert_eq!(planned.enroll_requests, 1);
    // AA03 已過期：刪除；AA02 已撤銷未過期、AA01 有效（先撤銷）：保留
    assert_eq!(
        (
            planned.certificates,
            planned.certificates_kept,
            planned.certificates_active
        ),
        (1, 2, 1)
    );
    assert_eq!(planned.hosts, 1);

    // 主控台的流程：先撤銷有效的憑證，再由 writer 逐步刪除（每一步最多 5000 列）
    let c = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
    let revoked = revoke_active_certificates(&c, A, "user:admin", Some("10.9.9.9"), now).unwrap();
    assert_eq!(revoked, ["AA01"]);
    drop(c);
    let mut job = PurgeJob::new(A, "user:admin", Some("10.9.9.9"), Some("離職員工的筆電"));
    job.revoked = revoked;
    let mut steps = 0;
    while !store.purge_step(&mut job).unwrap() {
        steps += 1;
        assert!(steps < 100);
    }
    let c = &job.counts;
    assert_eq!(c.reports, planned.reports);
    assert_eq!(c.process_snapshots, planned.process_snapshots);
    assert_eq!(c.alert_events, 1);
    assert_eq!(c.certificates, 1);
    assert_eq!((c.certificates_kept, c.certificates_active), (2, 0));
    assert_eq!(c.hosts, 1);
    assert_eq!(table_counts(&dir, A), vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0]);
    assert_eq!(table_counts(&dir, B), b_before, "另一台主機不受影響");
    assert_eq!(
        count(
            &dir,
            "SELECT COUNT(*) FROM alert_timeline WHERE event_id NOT IN (SELECT event_id FROM alert_events)",
            []
        ),
        0
    );
    // 稽核：撤銷一筆、刪除一筆（不含主機名稱）；完整性驗證通過
    let conn = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
    let details: String = conn
        .query_row(
            "SELECT details FROM audit_events WHERE action = 'host.purge' AND object = ?1",
            [A],
            |r| r.get(0),
        )
        .unwrap();
    assert!(details.contains("離職員工的筆電") && details.contains("AA01"));
    assert!(!details.contains("host-6f1c"), "{details}");
    assert_eq!(
        count(
            &dir,
            "SELECT COUNT(*) FROM audit_events WHERE action = 'cert.revoke'",
            []
        ),
        1
    );
    assert!(conn.audit_verify().unwrap().ok());
    // 刪除後：這台主機的 report 與資產暫時被拒，不會被建立回來；另一台照常
    let st = store.write_batch(&[record(A, 9, noon()), record(B, 9, noon())]);
    assert!(matches!(st[0], WriteStatus::Failed(_)), "{st:?}");
    assert_eq!(st[1], WriteStatus::Written);
    assert!(store.write_assets(&asset(A, "S3")).is_err());
    assert_eq!(
        count(&dir, "SELECT COUNT(*) FROM hosts WHERE agent_id = ?1", [A]),
        0
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn small_budgets_take_several_steps_and_the_result_is_the_same() {
    let dir = temp_dir("steps");
    let (mut store, _) = SqliteStore::open(opts(&dir), noon).unwrap();
    populate(&mut store, &dir);
    store.close();
    drop(store);
    let mut conn = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
    let now = noon().timestamp_millis();
    let mut job = PurgeJob::new(A, "root", None, None);
    let mut steps = 0;
    while !step(&mut conn, &mut job, 2, now).unwrap() {
        steps += 1;
    }
    assert!(steps >= 6, "{steps}");
    assert_eq!(job.counts.reports, 5);
    assert_eq!(job.counts.process_snapshots, 5);
    // 有效的憑證沒有先撤銷：不刪除（刪掉就無法再撤銷）
    assert_eq!(job.counts.certificates_active, 1);
    assert_eq!(table_counts(&dir, A)[..7], [0, 0, 0, 0, 0, 0, 0]);
    // 第二次：沒有資料（清冊已沒有這台主機）
    assert_eq!(plan(&conn, A, now).unwrap().total(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_agents_have_an_empty_plan_and_orphan_certificates_expire() {
    let dir = temp_dir("orphan");
    let (mut store, _) = SqliteStore::open(opts(&dir), noon).unwrap();
    populate(&mut store, &dir);
    let now = noon().timestamp_millis();
    let conn = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
    let p = plan(&conn, "11111111-2222-4333-8444-555555555555", now).unwrap();
    assert_eq!(p, PurgeCounts::default());
    let mut job = PurgeJob::new(A, "root", None, None);
    while !store.purge_step(&mut job).unwrap() {}
    // 保留的兩張（AA01 撤銷後、AA02）：過期 1 天後才由每小時的清除刪除
    assert_eq!(delete_orphan_certificates(&conn, now).unwrap(), 0);
    assert_eq!(
        delete_orphan_certificates(&conn, now + 101 * DAY).unwrap(),
        1
    );
    assert_eq!(
        delete_orphan_certificates(&conn, now + 302 * DAY).unwrap(),
        1
    );
    // B 的憑證（清冊中有 B）不受影響
    assert_eq!(
        count(
            &dir,
            "SELECT COUNT(*) FROM pki_certificates WHERE agent_id = ?1",
            [B]
        ),
        1
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn impact_tokens_change_with_the_host_and_expire() {
    let c = PurgeCounts {
        hosts: 1,
        certificates_active: 1,
        ..Default::default()
    };
    let now = noon().timestamp_millis();
    let t = impact_token(A, &c, now);
    assert!(verify_impact_token(A, &c, &t, now));
    assert!(verify_impact_token(A, &c, &t, now + 5 * 60_000));
    assert!(!verify_impact_token(A, &c, &t, now + 11 * 60_000));
    assert!(!verify_impact_token(B, &c, &t, now));
    let more = PurgeCounts {
        certificates_active: 2,
        ..c.clone()
    };
    assert!(!verify_impact_token(A, &more, &t, now));
    assert!(c.json()["total"].is_u64());
}
