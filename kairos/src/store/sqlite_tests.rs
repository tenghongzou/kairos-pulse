//! [`SqliteStore`] 的單元測試：schema 與遷移、批次寫入、失敗回報、去重、主機清冊與載回、
//! 彙總（含重新啟動後補回）、保留期限與總容量上限

use super::*;
use crate::protocol::Metric;
use crate::registry::{AgentSnapshot, next_conn_id};
use crate::storage::DedupKey;
use chrono::TimeZone;
use std::sync::atomic::{AtomicI64, Ordering};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kairos-sqlite-{}-{}-{}",
        tag,
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
        mtls: true,
    }
}

fn at(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap()
}

/// 測試用固定時鐘：2026-10-09 12:00:00 UTC
fn noon() -> DateTime<Utc> {
    at(2026, 10, 9, 12, 0, 0)
}

/// 可由測試調整的時鐘：每個測試各用一個 static，避免平行執行時互相干擾
macro_rules! settable_clock {
    ($name:ident, $cell:ident) => {
        static $cell: AtomicI64 = AtomicI64::new(0);
        fn $name() -> DateTime<Utc> {
            DateTime::from_timestamp_millis($cell.load(Ordering::SeqCst)).unwrap()
        }
    };
}

fn set_clock(cell: &AtomicI64, t: DateTime<Utc>) {
    cell.store(t.timestamp_millis(), Ordering::SeqCst);
}

const A: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
const B: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

fn legacy(agent: &str, seq: u64, recv_at: DateTime<Utc>, cpu: f32) -> ReportRecord {
    ReportRecord::new(
        recv_at,
        "10.0.0.1:5000".into(),
        agent.into(),
        format!("host-{}", &agent[..4]),
        seq,
        crate::storage::fmt_time(recv_at),
        RecordBody::Legacy {
            cpu_usage: cpu,
            total_memory: 4096,
            used_memory: 1024,
        },
    )
}

fn metrics_record(agent: &str, seq: u64, recv_at: DateTime<Utc>, cpu: f64) -> ReportRecord {
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
        Metric::new("system.temperature", -5, "Cel").with_label("sensor", "外部"),
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

fn dedup(boot: &str, seq: u64) -> Option<DedupKey> {
    Some(DedupKey {
        boot_id: boot.into(),
        report_seq: seq,
    })
}

fn count(store: &mut SqliteStore, sql: &str) -> i64 {
    store.ensure_ready().unwrap();
    store
        .conn
        .as_ref()
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

fn open(dir: &Path, clock: Clock) -> (SqliteStore, Vec<RestoredAgent>) {
    SqliteStore::open(opts(dir), clock).unwrap()
}

#[test]
fn fresh_database_has_the_current_schema() {
    let dir = temp_dir("schema");
    let (mut store, restored) = open(&dir, noon);
    assert!(restored.is_empty());
    assert_eq!(
        count(&mut store, "PRAGMA user_version"),
        i64::from(SCHEMA_VERSION)
    );
    assert_eq!(
        count(&mut store, "PRAGMA application_id"),
        i64::from(APPLICATION_ID)
    );
    let mode: String = store
        .conn
        .as_ref()
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    for table in [
        "meta",
        "hosts",
        "series",
        "boots",
        "reports",
        "rollup_5m",
        "rollup_1h",
        "import_keys",
        "alert_rules",
        "alert_events",
        "alert_timeline",
        "alert_silences",
        "alert_channels",
        "alert_deliveries",
        "alert_actions",
    ] {
        assert_eq!(
            count(
                &mut store,
                &format!(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'"
                )
            ),
            1,
            "{table}"
        );
    }
    drop(store);
    // 再開一次：不重複建立
    let (mut store, _) = open(&dir, noon);
    assert_eq!(
        count(&mut store, "PRAGMA user_version"),
        i64::from(SCHEMA_VERSION)
    );
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn database_files_are_0640() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    let dir = temp_dir("perm");
    let path = dir.join(DB_FILE);
    // 既有的寬鬆權限在開啟時收緊；-wal、-shm 沿用資料庫檔的權限
    std::fs::write(&path, b"").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    let (mut store, _) = open(&dir, noon);
    assert_eq!(
        store.write(&[legacy(A, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    assert_eq!(mode(&path), 0o640);
    assert_eq!(mode(&sidecar(&path, "-wal")), 0o640);
    assert_eq!(mode(&sidecar(&path, "-shm")), 0o640);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn migrations_apply_in_order_and_refuse_newer_databases() {
    let dir = temp_dir("migrate");
    let path = dir.join("m.db");
    let v1 = Migration {
        version: 1,
        sql: "CREATE TABLE meta (key TEXT PRIMARY KEY, value) WITHOUT ROWID;
              CREATE TABLE t (a INTEGER);",
    };
    let v2 = Migration {
        version: 2,
        sql: "ALTER TABLE t ADD COLUMN b TEXT NOT NULL DEFAULT 'x';",
    };
    let bad_v3 = Migration {
        version: 3,
        sql: "ALTER TABLE t ADD COLUMN c INTEGER; THIS IS NOT SQL;",
    };
    let mut conn = Connection::open(&path).unwrap();
    assert_eq!(migrate(&mut conn, &[v1]).unwrap(), 1);
    conn.execute("INSERT INTO t (a) VALUES (7)", []).unwrap();
    // 升級：資料保留，新欄位有預設值
    assert_eq!(migrate(&mut conn, &[v1, v2]).unwrap(), 2);
    let (a, b): (i64, String) = conn
        .query_row("SELECT a, b FROM t", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((a, b.as_str()), (7, "x"));
    // 失敗的升級整版不生效（交易回復）
    assert!(migrate(&mut conn, &[v1, v2, bad_v3]).is_err());
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
    let columns: i64 = conn
        .query_row("SELECT count(*) FROM pragma_table_info('t')", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(columns, 2);
    // 降版執行：資料庫比程式新時拒絕，不做任何變更
    let err = migrate(&mut conn, &[v1]).unwrap_err();
    assert!(
        err.to_string().contains("比本版 Kairos 支援的 1 新"),
        "{err}"
    );
    drop(conn);

    // 由較新版本建立的 kairos.db：開啟失敗（Kairos 不啟動）
    let newer = temp_dir("newer");
    let (store, _) = open(&newer, noon);
    store
        .conn
        .as_ref()
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    drop(store);
    let err = SqliteStore::open(opts(&newer), noon).unwrap_err();
    assert!(err.to_string().contains("schema 版本 99"), "{err}");
    // 唯讀查詢同樣拒絕
    assert!(open_read_only(&newer.join(DB_FILE)).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&newer).unwrap();
}

#[test]
fn foreign_sqlite_files_are_refused() {
    let dir = temp_dir("foreign");
    let conn = Connection::open(dir.join(DB_FILE)).unwrap();
    conn.execute_batch("CREATE TABLE something (x)").unwrap();
    drop(conn);
    let err = SqliteStore::open(opts(&dir), noon).unwrap_err();
    assert!(err.to_string().contains("不是 Kairos 的資料庫"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn reports_are_stored_compactly_and_read_back_exactly() {
    let dir = temp_dir("roundtrip");
    let (mut store, _) = open(&dir, noon);
    let v1 = legacy(A, 6, noon(), 17.84);
    let v2 = metrics_record(B, 3, noon(), 17.84);
    assert_eq!(
        store.write(&[v1.clone(), v2.clone()]),
        [WriteStatus::Written, WriteStatus::Written]
    );
    // 同一台主機的第二批沿用同一個 host_id、同一組序列
    assert_eq!(
        store.write(&[metrics_record(B, 4, noon(), 20.0)]),
        [WriteStatus::Written]
    );
    assert_eq!(count(&mut store, "SELECT count(*) FROM hosts"), 2);
    assert_eq!(count(&mut store, "SELECT count(*) FROM reports"), 3);
    // 序列：v2 的 5 個；v1 的三個欄位對應到其中 3 個標準序列
    assert_eq!(count(&mut store, "SELECT count(*) FROM series"), 5);
    assert_eq!(
        count(
            &mut store,
            "SELECT count(*) FROM series WHERE labels = '{\"fstype\":\"ext4\",\"mountpoint\":\"/\"}'"
        ),
        1
    );

    let conn = store.conn.as_ref().unwrap();
    let read = |agent: &str| -> Vec<(i64, i64, SqlValue, i64, Vec<u8>)> {
        let mut stmt = conn
            .prepare(
                "SELECT r.ts, r.seq, r.agent_ts, r.format, r.body FROM reports r
                 JOIN hosts h USING (host_id) WHERE h.agent_id = ?1 ORDER BY r.id",
            )
            .unwrap();
        stmt.query_map([agent], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    };
    let rows = read(A);
    assert_eq!(rows.len(), 1);
    let (ts, seq, agent_ts, format, body) = &rows[0];
    assert_eq!(*ts, noon().timestamp_millis());
    assert_eq!(*seq, 6);
    // RFC 3339 毫秒的 ts 存成整數
    assert_eq!(*agent_ts, SqlValue::Integer(noon().timestamp_millis()));
    assert_eq!(*format, FORMAT_LEGACY);
    assert_eq!(codec::decode_legacy(body), Some((17.84, 4096, 1024)));
    assert_eq!(body.len(), 4 + 2 + 2);

    let rows = read(B);
    let (_, seq, agent_ts, format, body) = &rows[0];
    assert_eq!((*seq, *format), (3, FORMAT_METRICS));
    assert_eq!(*agent_ts, SqlValue::Text("not-rfc3339".into()));
    let decoded = codec::decode_metrics(body).unwrap();
    let RecordBody::Metrics { metrics } = &v2.body else {
        unreachable!()
    };
    // 依序列編號還原名稱、單位、label，與收到的相同（數字表示也相同）
    let mut series = conn
        .prepare("SELECT name, unit, labels FROM series WHERE series_id = ?1")
        .unwrap();
    for ((id, value), original) in decoded.iter().zip(metrics) {
        let (name, unit, labels): (String, String, String) = series
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!(name, original.name);
        assert_eq!(unit, original.unit);
        assert_eq!(labels, canonical_labels(&original.labels));
        assert_eq!(value.to_string(), original.value.to_string());
    }
    // 緊湊：5 個指標約 40 bytes（JSONL 同一行約 500 bytes）
    assert!(body.len() < 48, "{}", body.len());

    // 主機清冊：第一次出現時建立（名稱、mTLS 身分）
    let (hostname, verified, online): (String, bool, bool) = conn
        .query_row(
            "SELECT hostname, identity_verified, online FROM hosts WHERE agent_id = ?1",
            [B],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (hostname.as_str(), verified, online),
        ("web-01", true, true)
    );
    drop(series);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_batch_is_one_transaction() {
    let dir = temp_dir("batch");
    let (mut store, _) = open(&dir, noon);
    let batch: Vec<ReportRecord> = (0..500)
        .map(|i| legacy(if i % 2 == 0 { A } else { B }, i, noon(), 1.0))
        .collect();
    let results = store.write(&batch);
    assert_eq!(results.len(), 500);
    assert!(results.iter().all(|r| *r == WriteStatus::Written));
    assert_eq!(count(&mut store, "SELECT count(*) FROM reports"), 500);
    assert!(store.write(&[]).is_empty());
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn duplicate_dedup_keys_are_acknowledged_but_not_stored_twice() {
    // durable（A5）：同一主機、同一 (boot_id, report_seq) 只保存一次，重複的回 Duplicate
    let dir = temp_dir("dedup");
    let (mut store, _) = open(&dir, noon);
    let boot = "0b1c2d3e-0000-4000-8000-000000000001";
    let r = |agent, seq, rs| legacy(agent, seq, noon(), 1.0).with_dedup(dedup(boot, rs));
    assert_eq!(
        store.write(&[r(A, 1, 1), r(A, 2, 2)]),
        [WriteStatus::Written, WriteStatus::Written]
    );
    // 重送（連線的 seq 不同、去重鍵相同）：不再寫入
    assert_eq!(store.write(&[r(A, 9, 2)]), [WriteStatus::Duplicate]);
    // 同一批內重複
    assert_eq!(
        store.write(&[r(A, 10, 3), r(A, 11, 3)]),
        [WriteStatus::Written, WriteStatus::Duplicate]
    );
    // 另一台主機用相同的 boot_id 與 report_seq：不同的去重鍵
    assert_eq!(store.write(&[r(B, 1, 2)]), [WriteStatus::Written]);
    // 新的 boot：從頭編號也不衝突
    assert_eq!(
        store.write(&[legacy(A, 12, noon(), 1.0).with_dedup(dedup("other-boot", 1))]),
        [WriteStatus::Written]
    );
    // 舊 boot 的重送在 boot 切換後仍能辨識
    assert_eq!(store.write(&[r(A, 13, 1)]), [WriteStatus::Duplicate]);
    // 沒有去重鍵的 report 一律寫入
    assert_eq!(
        store.write(&[legacy(A, 2, noon(), 1.0), legacy(A, 2, noon(), 1.0)]),
        [WriteStatus::Written, WriteStatus::Written]
    );
    assert_eq!(count(&mut store, "SELECT count(*) FROM reports"), 7);
    assert_eq!(count(&mut store, "SELECT count(*) FROM boots"), 3);
    // 重複的不加入彙總
    assert_eq!(store.rollup.len(), 2 * 3);
    let cnt: i64 = store
        .rollup
        .take_ready(i64::MAX, usize::MAX)
        .iter()
        .map(|r| r.agg.count as i64)
        .sum();
    assert_eq!(cnt, 7 * 3);
    // 重新開啟後（快取清空）仍能辨識
    drop(store);
    let (mut store, _) = open(&dir, noon);
    assert_eq!(store.write(&[r(A, 14, 3)]), [WriteStatus::Duplicate]);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn failures_before_commit_store_nothing_and_unknown_commits_are_reported() {
    let dir = temp_dir("fail");
    let (mut store, _) = open(&dir, noon);
    assert_eq!(
        store.write(&[legacy(A, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );

    // 提交前失敗：整批回復，新主機也不存在，回 Failed（storage_unavailable）
    store.fail_next = Some(FailPoint::BeforeCommit);
    let results = store.write(&[legacy(A, 2, noon(), 1.0), legacy(B, 1, noon(), 1.0)]);
    assert!(
        results.iter().all(|r| matches!(r, WriteStatus::Failed(_))),
        "{results:?}"
    );
    assert_eq!(count(&mut store, "SELECT count(*) FROM reports"), 1);
    assert_eq!(count(&mut store, "SELECT count(*) FROM hosts"), 1);
    // 之後照常寫入（快取已重新載入，B 重新建立）
    assert_eq!(
        store.write(&[legacy(B, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    assert_eq!(count(&mut store, "SELECT count(*) FROM hosts"), 2);

    // 提交時磁碟已滿：交易回復，確定未寫入
    store.fail_next = Some(FailPoint::CommitDiskFull);
    let results = store.write(&[legacy(A, 3, noon(), 1.0)]);
    assert!(
        matches!(&results[0], WriteStatus::Failed(r) if r.contains("提交失敗")),
        "{results:?}"
    );
    assert_eq!(count(&mut store, "SELECT count(*) FROM reports"), 2);

    // 提交時發生其他錯誤：無法確定 → Unknown（這裡實際已寫入）
    store.fail_next = Some(FailPoint::CommitUnknown);
    let results = store.write(&[legacy(A, 4, noon(), 1.0)]);
    assert!(
        matches!(&results[0], WriteStatus::Unknown(_)),
        "{results:?}"
    );
    // 連線已重新開啟，照常寫入
    assert_eq!(
        store.write(&[legacy(A, 5, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    assert_eq!(count(&mut store, "SELECT count(*) FROM reports"), 4);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn opening_fails_when_the_data_dir_is_unusable_and_writes_report_failure() {
    let dir = temp_dir("unusable");
    let not_dir = dir.join("file");
    std::fs::write(&not_dir, b"x").unwrap();
    let mut o = opts(&not_dir);
    o.path = not_dir.join(DB_FILE);
    let err = SqliteStore::open(o, noon).unwrap_err();
    assert!(err.to_string().contains("無法建立資料目錄"), "{err}");

    // 執行中資料目錄被換成檔案：這一批確定未寫入；恢復後繼續
    let data = dir.join("data");
    let (mut store, _) = open(&data, noon);
    assert_eq!(
        store.write(&[legacy(A, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    drop(store.conn.take());
    std::fs::remove_dir_all(&data).unwrap();
    std::fs::write(&data, b"x").unwrap();
    let results = store.write(&[legacy(A, 2, noon(), 1.0)]);
    assert!(
        matches!(&results[0], WriteStatus::Failed(r) if r.contains("無法建立資料目錄")),
        "{results:?}"
    );
    std::fs::remove_file(&data).unwrap();
    assert_eq!(
        store.write(&[legacy(A, 3, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn deleted_database_is_recreated_instead_of_writing_into_the_void() {
    // 與 JSONL 的 needs_reopen 相同：檔案被刪除（或資料目錄被刪除）後不能繼續寫進已刪除的檔案
    let dir = temp_dir("deleted");
    let (mut store, _) = open(&dir, noon);
    assert_eq!(
        store.write(&[legacy(A, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        store.write(&[legacy(A, 2, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    // 新的資料庫中只有刪除後的那一筆
    let conn = open_read_only(&dir.join(DB_FILE)).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM reports", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    drop(conn);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn host_limit_rejects_only_new_hosts() {
    let dir = temp_dir("hostcap");
    let mut o = opts(&dir);
    o.max_hosts = 1;
    let (mut store, _) = SqliteStore::open(o, noon).unwrap();
    let results = store.write(&[
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
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
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
        connected_at: crate::storage::fmt_time(noon() - chrono::Duration::hours(1)),
        last_seen: crate::storage::fmt_time(last_seen),
        offline_after_ms: 75_000,
        last_report: Some(LastReport {
            recv_at: crate::storage::fmt_time(last_seen),
            seq: 42,
            ts: "t".into(),
            cpu_usage: Some(12.5),
            total_memory: Some(4096),
            used_memory: Some(1024),
            metrics: Some(8),
        }),
    }
}

fn snapshot(agents: Vec<AgentSnapshot>) -> RegistrySnapshot {
    RegistrySnapshot {
        generated_at: crate::storage::fmt_time(noon()),
        online: agents.iter().filter(|a| a.online).count(),
        total: agents.len(),
        agents,
    }
}

#[test]
fn inventory_is_persisted_and_restored_after_restart() {
    // A6：主機清冊在 SQLite；重新啟動後最近 24 小時內的主機回到狀態表，更早的仍在清冊中
    let dir = temp_dir("inventory");
    let (mut store, _) = open(&dir, noon);
    let old = noon() - chrono::Duration::days(4);
    store.sync_inventory(&snapshot(vec![
        agent_snapshot(A, true, noon()),
        agent_snapshot(B, false, old),
    ]));
    let hosts = list_hosts(store.conn.as_ref().unwrap()).unwrap();
    assert_eq!(hosts.len(), 2);
    // 依 last_seen 由舊到新
    assert_eq!(hosts[0].agent_id, B);
    assert!(!hosts[0].online);
    assert_eq!(hosts[0].last_seen, old);
    assert_eq!(hosts[1].agent_id, A);
    assert!(hosts[1].online && hosts[1].identity_verified);
    assert_eq!(hosts[1].display_name.as_deref(), Some("顯示名稱"));
    assert_eq!(hosts[1].proto, Some(2));
    assert_eq!(hosts[1].first_seen, noon() - chrono::Duration::hours(1));
    // 模擬異常結束（沒有最後一次同步）：A 在清冊中仍是線上
    drop(store);

    let (mut store, restored) = open(&dir, noon);
    // 重新啟動：仍標為線上的改為離線，只有 24 小時內的主機放回狀態表
    assert_eq!(
        count(&mut store, "SELECT count(*) FROM hosts WHERE online = 1"),
        0
    );
    assert_eq!(restored.len(), 1);
    let r = &restored[0];
    assert_eq!(r.agent_id, A);
    assert_eq!(r.hostname, "host-6f1c");
    assert_eq!(r.proto, 2);
    assert_eq!(r.peer, "10.1.2.3:40000");
    assert_eq!(r.last_seen, noon());
    assert_eq!(r.connected_at, noon() - chrono::Duration::hours(1));
    assert_eq!(r.offline_after, Duration::from_secs(75));
    let host = r.host.as_ref().unwrap();
    assert_eq!(
        host.host_fingerprint,
        Some(format!("sha256:{}", "c".repeat(64)))
    );
    assert_eq!(host.tags, Some(vec!["env:prod".to_string()]));
    assert_eq!(host.report_interval_ms, Some(5000));
    assert_eq!(r.last_report.as_ref().map(|l| l.seq), Some(42));
    // 放回狀態表後：快照與原本相同（離線）
    let registry = crate::registry::Registry::new();
    assert_eq!(registry.restore(restored, Instant::now(), noon()), 1);
    let snap = registry.snapshot(noon());
    assert_eq!(snap.agents[0].agent_id, A);
    assert!(!snap.agents[0].online);
    assert_eq!(
        snap.agents[0]
            .host
            .as_ref()
            .unwrap()
            .display_name
            .as_deref(),
        Some("顯示名稱")
    );
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn inventory_sync_writes_only_changes() {
    let dir = temp_dir("sync");
    let (mut store, _) = open(&dir, noon);
    let snap = snapshot(vec![agent_snapshot(A, true, noon())]);
    assert_eq!(store.sync(&snap).unwrap(), 1);
    assert_eq!(store.sync(&snap).unwrap(), 0);
    // last_seen 前進不到 60 秒：不寫
    let later = snapshot(vec![agent_snapshot(
        A,
        true,
        noon() + chrono::Duration::seconds(59),
    )]);
    assert_eq!(store.sync(&later).unwrap(), 0);
    let later = snapshot(vec![agent_snapshot(
        A,
        true,
        noon() + chrono::Duration::seconds(60),
    )]);
    assert_eq!(store.sync(&later).unwrap(), 1);
    // 其他欄位改變（離線）立即寫入
    let offline = snapshot(vec![agent_snapshot(
        A,
        false,
        noon() + chrono::Duration::seconds(61),
    )]);
    assert_eq!(store.sync(&offline).unwrap(), 1);
    assert_eq!(count(&mut store, "SELECT online FROM hosts"), 0);
    // v1 重新連線（沒有主機資訊）：保留上次的指紋等資訊
    let mut v1 = agent_snapshot(A, true, noon() + chrono::Duration::seconds(70));
    v1.host = None;
    v1.proto = 1;
    assert_eq!(store.sync(&snapshot(vec![v1])).unwrap(), 1);
    let (proto, fp): (i64, Option<String>) = store
        .conn
        .as_ref()
        .unwrap()
        .query_row("SELECT proto, host_fingerprint FROM hosts", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(proto, 1);
    assert!(fp.is_some());
    // 先有 report 才同步：同一列（不重複建立）
    assert_eq!(
        store.write(&[legacy(B, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    assert_eq!(
        store
            .sync(&snapshot(vec![agent_snapshot(B, true, noon())]))
            .unwrap(),
        1
    );
    assert_eq!(count(&mut store, "SELECT count(*) FROM hosts"), 2);
    // 清冊已滿時不新增
    let mut o = opts(&dir);
    o.max_hosts = 2;
    store.opts = o;
    let c = "cccccccc-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    assert_eq!(
        store
            .sync(&snapshot(vec![agent_snapshot(c, true, noon())]))
            .unwrap(),
        0
    );
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn rollups_are_flushed_after_the_bucket_ends() {
    settable_clock!(clock, NOW);
    // 12:00:00 起的 5 分鐘桶
    set_clock(&NOW, noon());
    let dir = temp_dir("rollup");
    let (mut store, _) = open(&dir, clock);
    for (i, cpu) in [10.0, 30.0, 20.0].into_iter().enumerate() {
        let t = noon() + chrono::Duration::seconds(i as i64 * 60);
        assert_eq!(
            store.write(&[legacy(A, i as u64, t, cpu)]),
            [WriteStatus::Written]
        );
    }
    // 下一個桶
    store.write(&[legacy(A, 9, noon() + chrono::Duration::minutes(5), 50.0)]);
    // 桶尚未結束 + 寬限：不寫出
    set_clock(&NOW, noon() + chrono::Duration::seconds(300 + 14));
    assert!(!store.maintain());
    assert_eq!(count(&mut store, "SELECT count(*) FROM rollup_5m"), 0);
    set_clock(&NOW, noon() + chrono::Duration::seconds(300 + 15));
    assert!(store.maintain());
    let conn = store.conn.as_ref().unwrap();
    let cpu_series: i64 = conn
        .query_row(
            "SELECT series_id FROM series WHERE name = 'system.cpu.usage'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let row = |table: &str| -> (i64, f64, f64, f64, i64) {
        conn.query_row(
            &format!("SELECT bucket, vmin, vmax, vsum, cnt FROM {table} WHERE series_id = ?1 ORDER BY bucket LIMIT 1"),
            [cpu_series],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap()
    };
    assert_eq!(row("rollup_5m"), (noon().timestamp(), 10.0, 30.0, 60.0, 3));
    assert_eq!(row("rollup_1h"), (noon().timestamp(), 10.0, 30.0, 60.0, 3));
    // v1 的記憶體換算成 bytes，與 v2 的標準指標同一個序列
    let mem: f64 = conn
        .query_row(
            "SELECT vmax FROM rollup_5m JOIN series USING (series_id) WHERE name = 'system.memory.total'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(mem, 4096.0 * 1024.0 * 1024.0);
    let until: i64 = conn
        .query_row("SELECT rollup_until FROM hosts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        until,
        (noon() + chrono::Duration::minutes(5)).timestamp_millis()
    );

    // 第二個桶寫出後合併進同一小時
    set_clock(&NOW, noon() + chrono::Duration::seconds(600 + 15));
    store.maintain();
    assert_eq!(
        count(
            &mut store,
            "SELECT count(*) FROM rollup_5m WHERE series_id = 1"
        ),
        2
    );
    let conn = store.conn.as_ref().unwrap();
    let (vmax, cnt): (f64, i64) = conn
        .query_row(
            "SELECT vmax, cnt FROM rollup_1h WHERE series_id = ?1",
            [cpu_series],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((vmax, cnt), (50.0, 4));
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unflushed_rollups_are_recovered_after_a_crash_without_double_counting() {
    settable_clock!(clock, NOW);
    set_clock(&NOW, noon());
    let dir = temp_dir("recover");
    let (mut store, _) = open(&dir, clock);
    // 第一個桶：寫出
    store.write(&[
        legacy(A, 1, noon(), 10.0),
        legacy(A, 2, noon() + chrono::Duration::seconds(1), 20.0),
    ]);
    set_clock(&NOW, noon() + chrono::Duration::seconds(320));
    store.maintain();
    // 第二個桶：寫入原始資料後「當掉」（記憶體中的彙總遺失）
    let t2 = noon() + chrono::Duration::minutes(5);
    store.write(&[legacy(A, 3, t2, 30.0), metrics_record(A, 4, t2, 40.0)]);
    drop(store);

    let (mut store, _) = open(&dir, clock);
    // 從 rollup_until 起補回，不含已寫出的第一個桶
    assert_eq!(store.rollup.len(), 5);
    set_clock(&NOW, noon() + chrono::Duration::minutes(11));
    while store.maintain() {}
    let conn = store.conn.as_ref().unwrap();
    let cnt: i64 = conn
        .query_row(
            "SELECT cnt FROM rollup_1h JOIN series USING (series_id) WHERE name = 'system.cpu.usage'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cnt, 4, "每筆只算一次");
    let sum: f64 = conn
        .query_row(
            "SELECT vsum FROM rollup_1h JOIN series USING (series_id) WHERE name = 'system.cpu.usage'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sum, 100.0);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn retention_deletes_expired_rows_in_batches() {
    settable_clock!(clock, NOW);
    set_clock(&NOW, noon());
    let dir = temp_dir("retention");
    let mut o = opts(&dir);
    o.raw_retention_days = 2;
    o.rollup_5m_retention_days = 3;
    o.rollup_1h_retention_days = 4;
    let (mut store, _) = SqliteStore::open(o.clone(), clock).unwrap();
    // 每天一筆，從 6 天前到今天；同時直接放入對應的彙總
    let boot = "boot";
    for d in 0..=6 {
        let t = noon() - chrono::Duration::days(d);
        store.write(&[legacy(A, d as u64, t, 1.0).with_dedup(dedup(&format!("{boot}-{d}"), 1))]);
        let conn = store.conn.as_ref().unwrap();
        for table in ["rollup_5m", "rollup_1h"] {
            conn.execute(
                &format!("INSERT INTO {table} VALUES (1, ?1, 1, 1, 1, 1, 1)"),
                [t.timestamp()],
            )
            .unwrap();
        }
    }
    store.rollup.clear();
    store.capacity.next_check = Some(Instant::now() + Duration::from_secs(3600));
    while store.maintain() {}
    // 今天與前 N 天保留（10-07 起），其餘刪除
    assert_eq!(count(&mut store, "SELECT count(*) FROM reports"), 3);
    assert_eq!(count(&mut store, "SELECT count(*) FROM rollup_5m"), 4);
    assert_eq!(count(&mut store, "SELECT count(*) FROM rollup_1h"), 5);
    assert_eq!(
        count(&mut store, "SELECT min(ts) FROM reports"),
        (noon() - chrono::Duration::days(2)).timestamp_millis()
    );
    // 沒有 report 引用的 boot 也刪除
    assert_eq!(count(&mut store, "SELECT count(*) FROM boots"), 3);

    // 時鐘被誤校到一年後：不刪除（DateGuard），重新啟動後也一樣
    store.next_retention = None;
    set_clock(&NOW, noon() + chrono::Duration::days(365));
    while store.maintain() {}
    assert_eq!(count(&mut store, "SELECT count(*) FROM reports"), 3);
    drop(store);
    let (mut store, _) = SqliteStore::open(o, clock).unwrap();
    store.capacity.next_check = Some(Instant::now() + Duration::from_secs(3600));
    while store.maintain() {}
    assert_eq!(count(&mut store, "SELECT count(*) FROM reports"), 3);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn capacity_limit_deletes_the_oldest_raw_reports_first() {
    let dir = temp_dir("capacity");
    let mut o = opts(&dir);
    o.raw_retention_days = 0;
    o.rollup_5m_retention_days = 0;
    o.rollup_1h_retention_days = 0;
    let (mut store, _) = SqliteStore::open(o.clone(), noon).unwrap();
    // 約 4 MB 的原始資料
    for chunk in 0..20 {
        let batch: Vec<ReportRecord> = (0..1000)
            .map(|i| metrics_record(A, chunk * 1000 + i, noon(), 1.0))
            .collect();
        store.write(&batch);
    }
    let conn = store.conn.as_ref().unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    conn.execute("INSERT INTO rollup_5m VALUES (1, 0, 1, 1, 1, 1, 1)", [])
        .unwrap();
    let used = store.used_bytes().unwrap();
    // 上限設為目前用量的 3/4：刪掉最舊的一批（10000 筆）就降到上限的 95% 以下
    let cap = used / 4 * 3;
    store.opts.max_total_bytes = cap;
    store.rollup.clear();
    while store.maintain() {}
    let after = store.used_bytes().unwrap();
    assert!(after <= cap / 100 * 95, "{after} > {cap}");
    // KRS-W4202：開始與結束記在 meta.capacity_event（系統健康的「最近一次容量刪除」）
    let event: CapacityEvent = serde_json::from_str(
        &store
            .conn
            .as_ref()
            .unwrap()
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [META_CAPACITY_EVENT],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(event.limit_bytes, cap);
    assert_eq!(event.started_at, noon().timestamp_millis());
    assert_eq!(event.ended_at, Some(noon().timestamp_millis()));
    assert!(event.deleted_raw >= 5000, "{event:?}");
    assert!(!event.exhausted);
    // 刪除的是最舊的（seq 小的），最新的保留；原始資料還有剩時不刪彙總
    let min_seq = count(&mut store, "SELECT min(seq) FROM reports");
    assert!(min_seq > 0);
    assert_eq!(count(&mut store, "SELECT max(seq) FROM reports"), 19_999);
    assert_eq!(count(&mut store, "SELECT count(*) FROM rollup_5m"), 1);

    // 原始資料刪光仍超過：再刪彙總
    store.opts.max_total_bytes = 1;
    store.capacity = Capacity::default();
    while store.maintain() {}
    assert_eq!(count(&mut store, "SELECT count(*) FROM reports"), 0);
    assert_eq!(count(&mut store, "SELECT count(*) FROM rollup_5m"), 0);
    // 主機清冊不刪
    assert_eq!(count(&mut store, "SELECT count(*) FROM hosts"), 1);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn host_trend_queries_use_the_indexes() {
    // 「某台主機 30 天趨勢」：原始資料走 (host_id, ts) 索引，彙總走主鍵範圍，不掃描整個表
    let dir = temp_dir("plan");
    let (store, _) = open(&dir, noon);
    let conn = store.conn.as_ref().unwrap();
    let plan = |sql: &str| -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let raw = plan("SELECT ts, body FROM reports WHERE host_id = 1 AND ts BETWEEN 0 AND 100");
    assert!(raw.contains("reports_host_ts"), "{raw}");
    for table in ["rollup_5m", "rollup_1h"] {
        let p = plan(&format!(
            "SELECT bucket, vmin, vmax, vsum / cnt FROM {table} WHERE host_id = 1 AND bucket BETWEEN 0 AND 100 AND series_id = 3"
        ));
        assert!(p.contains("PRIMARY KEY") && !p.starts_with("SCAN"), "{p}");
    }
    let hosts = plan("SELECT agent_id FROM hosts WHERE last_seen < 100");
    assert!(hosts.contains("hosts_last_seen"), "{hosts}");
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn export_reproduces_the_jsonl_lines() {
    // kairos reports export：與 JSONL writer 寫出的行相同（peer、hostname 取清冊中的值）
    let dir = temp_dir("export");
    let (mut store, _) = open(&dir, noon);
    let v1 = legacy(A, 6, noon(), 17.84);
    let v2 = metrics_record(B, 3, noon() + chrono::Duration::seconds(1), 17.84)
        .with_dedup(dedup("0b1c2d3e-0000-4000-8000-000000000001", 42));
    let old = legacy(A, 1, noon() - chrono::Duration::days(2), 1.0);
    store.write(&[old, v1.clone(), v2.clone()]);
    let conn = open_read_only(&dir.join(DB_FILE)).unwrap();
    let mut lines = Vec::new();
    let since = (noon() - chrono::Duration::days(1)).timestamp_millis();
    let n = export_reports(&conn, since, None, |r| {
        lines.push(serde_json::to_string(r).unwrap());
        Ok(())
    })
    .unwrap();
    assert_eq!(n, 2);
    // peer、hostname 為清冊中的值：這裡是第一次出現時那一筆的值，與原本相同
    assert_eq!(lines[0], serde_json::to_string(&v1).unwrap());
    assert_eq!(lines[1], serde_json::to_string(&v2).unwrap());
    // 只匯出一台
    let mut count = 0;
    export_reports(&conn, 0, Some(A), |r| {
        assert_eq!(r.agent_id, A);
        count += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(count, 2);
    drop(conn);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn online_backup_is_a_consistent_copy() {
    let dir = temp_dir("backup");
    let (mut store, _) = open(&dir, noon);
    store.write(&[legacy(A, 1, noon(), 1.0), metrics_record(B, 2, noon(), 2.0)]);
    store.sync_inventory(&snapshot(vec![agent_snapshot(A, false, noon())]));
    // 寫入的連線仍開著（WAL 中有尚未 checkpoint 的資料）
    let conn = open_read_only(&dir.join(DB_FILE)).unwrap();
    let dest = dir.join("backup.db");
    backup_to(&conn, &dest).unwrap();
    // 已存在時不覆寫
    assert_eq!(
        backup_to(&conn, &dest).unwrap_err().kind(),
        ErrorKind::AlreadyExists
    );
    drop(conn);
    // 備份是完整的 Kairos 資料庫：可以唯讀開啟、內容相同，也可以直接拿來啟動
    let copy = open_read_only(&dest).unwrap();
    let n: i64 = copy
        .query_row("SELECT count(*) FROM reports", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(list_hosts(&copy).unwrap().len(), 2);
    drop(copy);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o640);
    }
    let restored = temp_dir("restored");
    std::fs::copy(&dest, restored.join(DB_FILE)).unwrap();
    let (mut again, _) = open(&restored, noon);
    assert_eq!(count(&mut again, "SELECT count(*) FROM reports"), 2);
    drop(again);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&restored).unwrap();
}

#[test]
fn read_only_queries_work_while_the_writer_is_open() {
    let dir = temp_dir("readonly");
    let (mut store, _) = open(&dir, noon);
    store.sync_inventory(&snapshot(vec![agent_snapshot(A, true, noon())]));
    let conn = open_read_only(&dir.join(DB_FILE)).unwrap();
    assert_eq!(list_hosts(&conn).unwrap().len(), 1);
    // 唯讀連線無法寫入
    assert!(conn.execute("DELETE FROM hosts", []).is_err());
    // 讀取中的連線不會擋住寫入（WAL）
    assert_eq!(
        store.write(&[legacy(A, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    drop(conn);
    assert!(open_read_only(&dir.join("missing.db")).is_err());
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- durable 補送的歷史資料：取樣時間、遲到資料併入已寫出的彙總、結果不明後的重送 ----

/// durable report：`recv_at` 收到、取樣時間為 `ts`
fn durable(
    agent: &str,
    seq: u64,
    recv_at: DateTime<Utc>,
    ts: DateTime<Utc>,
    cpu: f32,
    report_seq: u64,
) -> ReportRecord {
    let mut r = legacy(agent, seq, recv_at, cpu);
    r.ts = crate::storage::fmt_time(ts);
    r.with_dedup(dedup("0b1c2d3e-0000-4000-8000-0000000000aa", report_seq))
}

/// CPU 序列在某個彙總表、某個桶的 (vmin, vmax, vsum, cnt)
fn cpu_rollup(
    store: &mut SqliteStore,
    table: &str,
    bucket: DateTime<Utc>,
) -> Option<(f64, f64, f64, i64)> {
    store.ensure_ready().unwrap();
    store
        .conn
        .as_ref()
        .unwrap()
        .query_row(
            &format!(
                "SELECT vmin, vmax, vsum, cnt FROM {table} JOIN series USING (series_id)
                 WHERE name = 'system.cpu.usage' AND bucket = ?1"
            ),
            [bucket.timestamp()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .unwrap()
}

#[test]
fn sample_time_is_the_client_ts_only_for_durable_reports_within_range() {
    let recv = noon();
    let at = |ts: &str| {
        let mut r = legacy(A, 1, recv, 1.0);
        r.ts = ts.into();
        r
    };
    let with_key = |r: ReportRecord| r.with_dedup(dedup("b", 1));
    let ms = |t: DateTime<Utc>| t.timestamp_millis();
    // 非 durable：一律為收到的時間（與 0.2 相同，不受 client 時鐘影響）
    assert_eq!(sample_time_ms(&at("2026-10-01T00:00:00.000Z")), ms(recv));
    // durable：client 的 ts（任何 RFC 3339 寫法），補送 7 天、甚至 1 年前的資料都照收
    let week_ago = recv - chrono::Duration::days(7);
    assert_eq!(
        sample_time_ms(&with_key(at(&crate::storage::fmt_time(week_ago)))),
        ms(week_ago)
    );
    assert_eq!(
        sample_time_ms(&with_key(at("2026-10-09T20:00:00+08:00"))),
        ms(recv)
    );
    let year_ago = recv - chrono::Duration::days(366);
    assert_eq!(
        sample_time_ms(&with_key(at(&crate::storage::fmt_time(year_ago)))),
        ms(year_ago)
    );
    // 超出範圍（client 時鐘錯誤）或無法解析：收到的時間
    for ts in [
        crate::storage::fmt_time(recv - chrono::Duration::days(368)),
        crate::storage::fmt_time(recv + chrono::Duration::minutes(6)),
        "not a time".to_string(),
    ] {
        assert_eq!(sample_time_ms(&with_key(at(&ts))), ms(recv), "{ts}");
    }
    // 稍微偏快（5 分鐘內）：照用
    let ahead = recv + chrono::Duration::minutes(4);
    assert_eq!(
        sample_time_ms(&with_key(at(&crate::storage::fmt_time(ahead)))),
        ms(ahead)
    );
}

#[test]
fn late_samples_are_merged_into_flushed_rollups_by_sample_time() {
    settable_clock!(clock, NOW);
    set_clock(&NOW, noon());
    let dir = temp_dir("late");
    let (mut store, _) = open(&dir, clock);
    // 即時資料：12:00 的桶，12:05:20 寫出
    store.write(&[legacy(A, 1, noon(), 10.0)]);
    store.write(&[legacy(A, 2, noon() + chrono::Duration::seconds(60), 30.0)]);
    let now = noon() + chrono::Duration::seconds(320);
    set_clock(&NOW, now);
    while store.maintain() {}
    assert_eq!(
        cpu_rollup(&mut store, "rollup_5m", noon()),
        Some((10.0, 30.0, 40.0, 2))
    );

    // 補送：取樣時間 12:01（桶已寫出）與 11:00（更早，還沒有這個桶），12:05:20 收到
    let t1 = noon() + chrono::Duration::seconds(60);
    let t0 = noon() - chrono::Duration::hours(1);
    assert_eq!(
        store.write(&[
            durable(A, 3, now, t1, 50.0, 1),
            durable(A, 4, now, t0, 5.0, 2)
        ]),
        [WriteStatus::Written, WriteStatus::Written]
    );
    // 在同一個交易中合併進已寫出的桶（累加，不覆蓋）；更早的桶新增一列；不進記憶體
    assert_eq!(
        cpu_rollup(&mut store, "rollup_5m", noon()),
        Some((10.0, 50.0, 90.0, 3))
    );
    assert_eq!(
        cpu_rollup(&mut store, "rollup_1h", noon()),
        Some((10.0, 50.0, 90.0, 3))
    );
    assert_eq!(
        cpu_rollup(&mut store, "rollup_5m", t0),
        Some((5.0, 5.0, 5.0, 1))
    );
    assert_eq!(
        cpu_rollup(&mut store, "rollup_1h", t0),
        Some((5.0, 5.0, 5.0, 1))
    );
    assert_eq!(store.rollup.len(), 0);
    // 原始資料以取樣時間保存，收到的時間由 recv_lag 還原
    assert_eq!(
        count(&mut store, "SELECT ts FROM reports WHERE report_seq = 1"),
        t1.timestamp_millis()
    );
    assert_eq!(
        count(
            &mut store,
            "SELECT recv_lag FROM reports WHERE report_seq = 1"
        ),
        (now - t1).num_milliseconds()
    );
    assert_eq!(
        count(
            &mut store,
            "SELECT count(*) FROM reports WHERE recv_lag IS NULL"
        ),
        2,
        "即時資料不存 recv_lag"
    );
    // 重送（結果不明後）：Duplicate，彙總不變
    assert_eq!(
        store.write(&[durable(A, 9, now, t1, 50.0, 1)]),
        [WriteStatus::Duplicate]
    );
    assert_eq!(
        cpu_rollup(&mut store, "rollup_5m", noon()),
        Some((10.0, 50.0, 90.0, 3))
    );
    // 匯出：recv_at 為收到的時間、ts 為原本的取樣時間
    let conn = open_read_only(&dir.join(DB_FILE)).unwrap();
    let mut lines = Vec::new();
    export_reports(&conn, t1.timestamp_millis(), Some(A), |r| {
        lines.push(r.clone());
        Ok(())
    })
    .unwrap();
    let backfilled = lines.iter().find(|r| r.seq == 3).unwrap();
    assert_eq!(backfilled.recv_at, now);
    assert_eq!(backfilled.ts, crate::storage::fmt_time(t1));
    assert!(lines.iter().all(|r| r.seq != 4), "11:00 的取樣早於 --since");
    drop(conn);

    // 當掉後重新開啟：直接合併的值不會再被補回一次
    drop(store);
    let (mut store, _) = open(&dir, clock);
    assert_eq!(store.rollup.len(), 0);
    set_clock(&NOW, noon() + chrono::Duration::hours(2));
    while store.maintain() {}
    assert_eq!(
        cpu_rollup(&mut store, "rollup_1h", noon()),
        Some((10.0, 50.0, 90.0, 3))
    );
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_backfilled_bucket_is_flushed_once_after_it_is_complete() {
    settable_clock!(clock, NOW);
    set_clock(&NOW, noon());
    let dir = temp_dir("backfill");
    let (mut store, _) = open(&dir, clock);
    // 新主機補送 3 天前的資料：桶早已結束，但補送進行中（Pulse 預設每 1.2 秒一筆）不寫出
    let base = at(2026, 10, 6, 0, 0, 0);
    for i in 0..10u64 {
        let ts = base + chrono::Duration::seconds(i as i64 * 5);
        assert_eq!(
            store.write(&[durable(B, i + 1, clock(), ts, i as f32, i + 1)]),
            [WriteStatus::Written]
        );
        set_clock(&NOW, clock() + chrono::Duration::milliseconds(1200));
    }
    assert!(store.rollup.has_ready(clock().timestamp_millis()));
    while store.maintain() {}
    assert_eq!(cpu_rollup(&mut store, "rollup_5m", base), None);
    assert_eq!(store.rollup.len(), 3);
    // 15 秒沒有新值：一次寫出整個桶，rollup_until 推進到桶的結束
    set_clock(&NOW, clock() + chrono::Duration::seconds(15));
    while store.maintain() {}
    assert_eq!(
        cpu_rollup(&mut store, "rollup_5m", base),
        Some((0.0, 9.0, 45.0, 10))
    );
    assert_eq!(
        count(&mut store, "SELECT rollup_until FROM hosts"),
        (base + chrono::Duration::minutes(5)).timestamp_millis()
    );
    // 之後才到的同一個桶的值（例如在途的那幾筆）直接合併
    store.write(&[durable(
        B,
        11,
        clock(),
        base + chrono::Duration::seconds(50),
        100.0,
        11,
    )]);
    assert_eq!(
        cpu_rollup(&mut store, "rollup_5m", base),
        Some((0.0, 100.0, 145.0, 11))
    );
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn after_an_unknown_commit_the_resent_report_is_rolled_up_exactly_once() {
    // 提交結果不明 → 連線不回應並關閉 → Pulse 重送：唯一索引判定重複（不重寫），
    // 記憶體中的彙總由原始資料重建，那一筆的值仍只算一次
    settable_clock!(clock, NOW);
    set_clock(&NOW, noon());
    let dir = temp_dir("unknown-resend");
    let (mut store, _) = open(&dir, clock);
    store.write(&[legacy(A, 1, noon(), 10.0)]);
    let t = noon() + chrono::Duration::seconds(1);
    store.fail_next = Some(FailPoint::CommitUnknown);
    let results = store.write(&[durable(A, 2, t, t, 20.0, 1)]);
    assert!(
        matches!(&results[0], WriteStatus::Unknown(_)),
        "{results:?}"
    );
    // 重連後重送（新的 seq，相同的去重鍵）
    assert_eq!(
        store.write(&[durable(A, 1, t, t, 20.0, 1)]),
        [WriteStatus::Duplicate]
    );
    assert_eq!(count(&mut store, "SELECT count(*) FROM reports"), 2);
    set_clock(&NOW, noon() + chrono::Duration::seconds(320));
    while store.maintain() {}
    assert_eq!(
        cpu_rollup(&mut store, "rollup_5m", noon()),
        Some((10.0, 20.0, 30.0, 2))
    );
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn inventory_sync_is_written_in_short_transactions() {
    let dir = temp_dir("sync-chunks");
    let (mut store, _) = open(&dir, noon);
    store.opts.max_hosts = 10_000;
    let n = INVENTORY_CHUNK_ROWS * 2 + 100;
    let agents: Vec<String> = (0..n)
        .map(|i| format!("00000000-0000-4000-8000-{i:012x}"))
        .collect();
    let snap = |online| {
        snapshot(
            agents
                .iter()
                .map(|a| agent_snapshot(a, online, noon()))
                .collect(),
        )
    };
    // 第一次同步：只寫入一批，其餘留給之後的維護步驟；中間照常寫入 report
    assert_eq!(store.sync(&snap(true)).unwrap(), INVENTORY_CHUNK_ROWS);
    assert_eq!(store.pending_inventory.len(), n - INVENTORY_CHUNK_ROWS);
    assert_eq!(
        store.write(&[legacy(A, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    assert!(store.maintain());
    assert_eq!(store.pending_inventory.len(), 100);
    assert!(store.maintain());
    assert!(store.pending_inventory.is_empty());
    assert_eq!(
        count(&mut store, "SELECT count(*) FROM hosts WHERE online = 1"),
        n as i64 + 1
    );
    // 已寫入的不再寫
    assert_eq!(store.sync(&snap(true)).unwrap(), 0);
    // 關機：最後一份快照（全部離線）在關閉前寫完
    assert_eq!(store.sync(&snap(false)).unwrap(), INVENTORY_CHUNK_ROWS);
    store.close();
    drop(store);
    let conn = open_read_only(&dir.join(DB_FILE)).unwrap();
    let offline: i64 = conn
        .query_row("SELECT count(*) FROM hosts WHERE online = 0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(offline, n as i64);
    drop(conn);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 主機清冊第一次同步 1 萬台的耗時（`kairos/STORAGE.md`「量測」）：分批（每批
/// [`INVENTORY_CHUNK_ROWS`] 列）時每個交易的最長耗時，與一次寫入 1 萬列的單一交易比較。
/// `cargo test -p kairos --release -- --ignored --nocapture inventory_sync_timing`
#[test]
#[ignore = "量測，手動執行"]
fn inventory_sync_timing() {
    let hosts: usize = std::env::var("KP_BENCH_HOSTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let agents: Vec<String> = (0..hosts)
        .map(|i| format!("00000000-0000-4000-8000-{i:012x}"))
        .collect();
    let snap = snapshot(
        agents
            .iter()
            .map(|a| agent_snapshot(a, true, noon()))
            .collect(),
    );
    // 分批
    let dir = temp_dir("sync-timing");
    let (mut store, _) = open(&dir, noon);
    store.opts.max_hosts = hosts * 2;
    let started = Instant::now();
    let t = Instant::now();
    store.sync(&snap).unwrap();
    let mut chunks = vec![t.elapsed()];
    while !store.pending_inventory.is_empty() {
        let t = Instant::now();
        store.sync_chunk().unwrap();
        chunks.push(t.elapsed());
    }
    let total = started.elapsed();
    let max = chunks.iter().max().copied().unwrap_or_default();
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
    // 單一交易（改成分批前的行為）
    let dir = temp_dir("sync-timing-single");
    let (mut store, _) = open(&dir, noon);
    store.opts.max_hosts = hosts * 2;
    let rows: Vec<InventoryRow> = snap
        .agents
        .iter()
        .map(|a| InventoryRow::of(a, true))
        .collect();
    let t = Instant::now();
    store.write_inventory(&rows).unwrap();
    let single = t.elapsed();
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
    println!(
        "BENCH inventory hosts={hosts} chunk_rows={INVENTORY_CHUNK_ROWS} chunks={} chunked_total={total:?} chunk_max={max:?} single_transaction={single:?}",
        chunks.len()
    );
}

/// 子行程：以 [`open_exclusive`] 嘗試取得資料庫的獨占鎖（與 `kairos reports import` 相同），以結束代碼
/// 回報：0 取得、3 使用中、4 其他錯誤。只由 [`probe_exclusive`] 以環境變數啟動
#[test]
#[ignore = "只由 prepare_files_keeps_the_locks_of_open_connections 以子行程執行"]
fn exclusive_lock_probe() {
    let Some(dir) = std::env::var_os("KAIROS_TEST_LOCK_PROBE_DIR") else {
        return;
    };
    let code = match open_exclusive(&opts(Path::new(&dir)), Duration::from_millis(100)) {
        Ok(_) => 0,
        Err(e) if e.kind() == ErrorKind::ResourceBusy => 3,
        Err(_) => 4,
    };
    std::process::exit(code);
}

/// 在另一個行程中嘗試取得獨占鎖（POSIX 的 fcntl 鎖在同一個行程內不會互相衝突）
#[cfg(unix)]
fn probe_exclusive(dir: &Path) -> i32 {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "store::sqlite::tests::exclusive_lock_probe",
            "--ignored",
            "--test-threads=1",
        ])
        .env("KAIROS_TEST_LOCK_PROBE_DIR", dir)
        .output()
        .unwrap();
    out.status.code().unwrap_or(-1)
}

/// writer 在資料庫被替換後重新開啟時會再呼叫 `prepare_files`，此時同一個行程內可能已有其他連線
/// （主控台的唯讀連線池、背景 checkpoint）開著同一個檔：`prepare_files` 不得以 `std::fs` 開關它，
/// 否則行程在這個檔上的 fcntl 鎖全部被放掉，`kairos reports import` 偵測不到 Kairos 正在執行
#[cfg(unix)]
#[test]
fn prepare_files_keeps_the_locks_of_open_connections() {
    let dir = temp_dir("locks");
    let path = opts(&dir).path;
    let (mut store, _) = open(&dir, noon);
    assert_eq!(
        store.write(&[legacy(A, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    let reader = open_read_only(&path).unwrap();
    let n: i64 = reader
        .query_row("SELECT count(*) FROM reports", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(probe_exclusive(&dir), 3, "連線開著時應拿不到獨占鎖");
    prepare_files(&path).unwrap();
    assert_eq!(
        probe_exclusive(&dir),
        3,
        "prepare_files 之後仍應拿不到獨占鎖（行程的鎖被放掉了）"
    );
    // 探測本身有效：所有連線關閉後可以取得
    drop(reader);
    drop(store);
    assert_eq!(probe_exclusive(&dir), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn upgrade_from_v4_through_v5_to_v6_keeps_data_and_adds_the_alert_and_user_tables() {
    let dir = temp_dir("v4v5");
    let path = dir.join("m.db");
    let mut conn = Connection::open(&path).unwrap();
    // 0.5 的資料庫（schema v4）：有註冊的設定與稽核紀錄
    assert_eq!(migrate(&mut conn, &MIGRATIONS[..4]).unwrap(), 4);
    conn.execute(
        "INSERT INTO settings (key, value, updated_at, updated_by) VALUES ('enroll.mode', 'open', 1, 't')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO audit_events (at, actor, category, action, result) VALUES (1, 't', 'enroll', 'x', 'success')",
        [],
    )
    .unwrap();
    let has = |conn: &Connection, t: &str| -> bool {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [t],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    };
    assert!(!has(&conn, "alert_events"));
    // 只套用 v5：告警的資料表建立
    assert_eq!(migrate(&mut conn, &MIGRATIONS[..5]).unwrap(), 5);
    assert!(has(&conn, "alert_events"));
    assert!(!has(&conn, "users"));
    // 再升級到 v7（整合後的線性順序 v4 → v5 → v6 → v7）：使用者的資料表建立
    assert_eq!(migrate(&mut conn, MIGRATIONS).unwrap(), 7);
    assert_eq!(
        MIGRATIONS.iter().map(|m| m.version).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6, 7]
    );
    for t in [
        "users",
        "role_assignments",
        "sessions",
        "user_recovery_codes",
    ] {
        assert!(has(&conn, t), "{t}");
    }
    let mode: String = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'enroll.mode'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(mode, "open");
    for t in [
        "alert_rules",
        "alert_events",
        "alert_timeline",
        "alert_silences",
        "alert_channels",
        "alert_deliveries",
        "alert_actions",
    ] {
        assert!(has(&conn, t), "{t}");
    }
    // 系統預設規則可以寫入；重複執行不重複（INSERT OR IGNORE）
    let n = crate::alerts::store::ensure_builtin_rules(&conn, 1).unwrap();
    assert_eq!(n, crate::alerts::rules::builtins().len());
    assert_eq!(
        crate::alerts::store::ensure_builtin_rules(&conn, 2).unwrap(),
        0
    );
    // 再執行一次升級：不變
    assert_eq!(migrate(&mut conn, MIGRATIONS).unwrap(), 7);
    drop(conn);
    std::fs::remove_dir_all(&dir).unwrap();
}
