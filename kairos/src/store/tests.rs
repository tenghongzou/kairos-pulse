//! writer（[`run_store_writer`]）與 [`start`] 的測試：commit 後才回覆、批次（group commit）、
//! 失敗與結果未知的回覆、取消的要求、JSONL 鏡像、關機前的清冊同步；以及吞吐量量測（ignored）

use super::*;
use crate::protocol::Metric;
use crate::registry::{AgentSnapshot, next_conn_id};
use crate::storage::{DedupKey, RecordBody, fmt_time};
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use std::sync::Mutex;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kairos-store-{}-{}-{}",
        tag,
        std::process::id(),
        next_conn_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn record(agent: &str, seq: u64) -> ReportRecord {
    ReportRecord::new(
        Utc::now(),
        "127.0.0.1:1".into(),
        agent.into(),
        "h".into(),
        seq,
        "t".into(),
        RecordBody::Legacy {
            cpu_usage: 1.0,
            total_memory: 2,
            used_memory: 1,
        },
    )
}

/// 記錄呼叫的假 store：第 `fail_at` 批回 Failed、`unknown_at` 批回 Unknown，其餘 Written
#[derive(Default)]
struct Calls {
    batches: Vec<Vec<u64>>,
    synced: usize,
    maintained: usize,
    closed: bool,
}

struct MockStore {
    calls: Arc<Mutex<Calls>>,
    fail_at: Option<usize>,
    unknown_at: Option<usize>,
    /// 每批寫入時的延遲（模擬磁碟），讓後續的要求在佇列中累積
    delay: Duration,
}

impl ReportStore for MockStore {
    fn write_batch(&mut self, records: &[ReportRecord]) -> Vec<WriteStatus> {
        std::thread::sleep(self.delay);
        let mut calls = self.calls.lock().unwrap();
        let n = calls.batches.len();
        calls.batches.push(records.iter().map(|r| r.seq).collect());
        records
            .iter()
            .map(|_| match (self.fail_at, self.unknown_at) {
                (Some(f), _) if f == n => WriteStatus::Failed("disk full".into()),
                (_, Some(u)) if u == n => WriteStatus::Unknown("io".into()),
                _ => WriteStatus::Written,
            })
            .collect()
    }

    fn sync_inventory(&mut self, _: &RegistrySnapshot) {
        self.calls.lock().unwrap().synced += 1;
    }

    fn maintain(&mut self) -> bool {
        self.calls.lock().unwrap().maintained += 1;
        false
    }

    fn close(&mut self) {
        self.calls.lock().unwrap().closed = true;
    }
}

fn mock(
    fail_at: Option<usize>,
    unknown_at: Option<usize>,
    delay: Duration,
) -> (MockStore, Arc<Mutex<Calls>>) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    (
        MockStore {
            calls: calls.clone(),
            fail_at,
            unknown_at,
            delay,
        },
        calls,
    )
}

#[tokio::test]
async fn queued_requests_are_committed_in_batches_and_each_gets_its_result() {
    let (store, calls) = mock(None, None, Duration::from_millis(20));
    let (sink, rx) = report_channel(REPORT_QUEUE_CAPACITY);
    let (_feed, inventory) = watch::channel(None);
    let writer = tokio::spawn(run_store_writer(store, rx, inventory, None));
    let mut pending = Vec::new();
    for seq in 0..300 {
        pending.push(sink.submit(record("a", seq)).await.unwrap());
    }
    for mut p in pending {
        assert_eq!(p.wait().await, WriteStatus::Written);
    }
    drop(sink);
    writer.await.unwrap();
    let calls = calls.lock().unwrap();
    // 寫入期間累積的要求成為下一批：300 筆遠少於 300 個交易
    assert!(calls.batches.len() < 50, "{}", calls.batches.len());
    assert!(calls.batches.iter().any(|b| b.len() > 1));
    // 依佇列順序寫入
    let all: Vec<u64> = calls.batches.iter().flatten().copied().collect();
    assert_eq!(all, (0..300).collect::<Vec<_>>());
    assert!(calls.closed);
}

#[tokio::test]
async fn failed_and_unknown_batches_are_reported_per_request() {
    let (store, _calls) = mock(Some(1), Some(2), Duration::ZERO);
    let (sink, rx) = report_channel(16);
    let (_feed, inventory) = watch::channel(None);
    let writer = tokio::spawn(run_store_writer(store, rx, inventory, None));
    let mut results = Vec::new();
    for seq in 0..4 {
        // 一次一筆，確保每批只有一筆
        results.push(sink.submit(record("a", seq)).await.unwrap().wait().await);
    }
    assert_eq!(results[0], WriteStatus::Written);
    assert!(matches!(results[1], WriteStatus::Failed(_)));
    assert!(matches!(results[2], WriteStatus::Unknown(_)));
    assert_eq!(results[3], WriteStatus::Written);
    drop(sink);
    writer.await.unwrap();
}

#[tokio::test]
async fn cancelled_requests_are_never_written() {
    // K-C1：連線逾時取消（已回 storage_unavailable）的要求，writer 不得寫入
    let (store, calls) = mock(None, None, Duration::ZERO);
    let (sink, rx) = report_channel(16);
    let mut p1 = sink.submit(record("a", 1)).await.unwrap();
    let p2 = sink.submit(record("a", 2)).await.unwrap();
    assert!(p2.cancel());
    let (_feed, inventory) = watch::channel(None);
    let writer = tokio::spawn(run_store_writer(store, rx, inventory, None));
    assert_eq!(p1.wait().await, WriteStatus::Written);
    assert!(!p1.cancel(), "已認領的要求不能再取消");
    drop(sink);
    writer.await.unwrap();
    assert_eq!(calls.lock().unwrap().batches, [vec![1]]);
}

#[tokio::test]
async fn writer_syncs_the_last_snapshot_before_closing() {
    let (store, calls) = mock(None, None, Duration::ZERO);
    let (sink, rx) = report_channel(16);
    let (feed, inventory) = watch::channel(None);
    let writer = tokio::spawn(run_store_writer(store, rx, inventory, None));
    // 一段時間後才有快照：同步只在間隔到期與結束前
    tokio::time::sleep(MAINTENANCE_INTERVAL * 3).await;
    assert!(calls.lock().unwrap().maintained >= 1);
    assert_eq!(calls.lock().unwrap().synced, 0);
    feed.send_replace(Some(Arc::new(RegistrySnapshot {
        generated_at: String::new(),
        online: 0,
        total: 0,
        agents: Vec::new(),
    })));
    drop(sink);
    writer.await.unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(calls.synced, 1);
    assert!(calls.closed);
}

fn sqlite_config(dir: &Path) -> AppConfig {
    let mut c = AppConfig::default();
    c.storage.data_dir = dir.to_string_lossy().into_owned();
    c.auth.client_ca = "ca.pem".into();
    c
}

#[tokio::test]
async fn sqlite_storage_acks_after_commit_mirrors_to_jsonl_and_persists_the_inventory() {
    let dir = temp_dir("sqlite");
    let mut config = sqlite_config(&dir);
    config.storage.jsonl_mirror = true;
    let storage = start(&config, &dir, Utc::now).await.unwrap();
    assert!(storage.restored.is_empty());
    let feed = storage.inventory.clone().unwrap();
    let writer = tokio::spawn(storage.writer);
    let agent = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
    let boot = Some(DedupKey {
        boot_id: "b".into(),
        report_seq: 1,
    });
    // ack（Written）時資料已在資料庫中
    let mut p = storage
        .reports
        .submit(record(agent, 1).with_dedup(boot.clone()))
        .await
        .unwrap();
    assert_eq!(p.wait().await, WriteStatus::Written);
    let db = dir.join(sqlite::DB_FILE);
    let conn = sqlite::open_read_only(&db).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM reports", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    // 重送：Duplicate（連線照常 ack），不寫入、不鏡像
    let mut p = storage
        .reports
        .submit(record(agent, 2).with_dedup(boot))
        .await
        .unwrap();
    assert_eq!(p.wait().await, WriteStatus::Duplicate);

    // 關機：最後一份快照寫入清冊
    let last_seen = Utc::now();
    feed.send_replace(Some(Arc::new(RegistrySnapshot {
        generated_at: fmt_time(last_seen),
        online: 0,
        total: 1,
        agents: vec![AgentSnapshot {
            agent_id: agent.into(),
            hostname: "web-01".into(),
            agent_version: "0.2.0".into(),
            proto: 2,
            host: None,
            peer: "127.0.0.1:1".into(),
            online: false,
            conn_id: 1,
            connected_at: fmt_time(last_seen),
            last_seen: fmt_time(last_seen),
            offline_after_ms: 75_000,
            last_report: None,
        }],
    })));
    drop(storage.reports);
    writer.await.unwrap();
    let hosts = sqlite::list_hosts(&conn).unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].hostname, "web-01");
    assert!(!hosts[0].online && hosts[0].identity_verified);
    drop(conn);
    // JSONL 鏡像：只有真正寫入的那一筆（帶去重鍵）
    let jsonl: Vec<serde_json::Value> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .flat_map(|p| {
            std::fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(jsonl.len(), 1, "{jsonl:?}");
    assert_eq!(jsonl[0]["seq"], 1);
    assert_eq!(jsonl[0]["boot_id"], "b");

    // 重新啟動：清冊中的主機載回
    let storage = start(&config, &dir, Utc::now).await.unwrap();
    assert_eq!(storage.restored.len(), 1);
    assert_eq!(storage.restored[0].agent_id, agent);
    drop(storage.reports);
    storage.writer.await;
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn jsonl_backend_still_works_without_a_database() {
    let dir = temp_dir("jsonl");
    let mut config = sqlite_config(&dir);
    config.storage.backend = StorageBackend::Jsonl;
    let storage = start(&config, &dir, Utc::now).await.unwrap();
    assert!(storage.inventory.is_none());
    let writer = tokio::spawn(storage.writer);
    assert_eq!(storage.reports.write(record("a", 1)).await, Ok(()));
    drop(storage.reports);
    writer.await.unwrap();
    assert!(!dir.join(sqlite::DB_FILE).exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn startup_fails_when_the_database_cannot_be_used() {
    let dir = temp_dir("bad");
    std::fs::write(
        dir.join(sqlite::DB_FILE),
        b"this is not a database file, just text....",
    )
    .unwrap();
    let err = start(&sqlite_config(&dir), &dir, Utc::now)
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains(sqlite::DB_FILE), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Pulse 0.2.0 的典型 report：17 個指標（cpu、記憶體 2、檔案系統 3 個掛載點 × 2、網路 2 張網卡 × 2、溫度 4）
pub(crate) fn pulse_like(agent: &str, seq: u64, at: DateTime<Utc>, cpu: f64) -> ReportRecord {
    let mut metrics = vec![
        Metric::new(
            "system.cpu.usage",
            serde_json::Number::from_f64(cpu).unwrap(),
            "%",
        ),
        Metric::new("system.memory.total", 34_359_738_368_u64, "By"),
        Metric::new("system.memory.used", 12_884_901_888_u64 + seq * 4096, "By"),
    ];
    for (mp, fs) in [("/", "ext4"), ("/boot", "vfat"), ("/data", "xfs")] {
        for (name, v) in [
            ("system.filesystem.total", 502_468_108_288_u64),
            ("system.filesystem.available", 211_312_345_088 - seq * 512),
        ] {
            metrics.push(
                Metric::new(name, v, "By")
                    .with_label("mountpoint", mp)
                    .with_label("fstype", fs),
            );
        }
    }
    for dev in ["eth0", "eth1"] {
        metrics.push(
            Metric::new("system.network.received", 98_765_432_101 + seq * 1500, "By")
                .with_label("device", dev),
        );
        metrics.push(
            Metric::new(
                "system.network.transmitted",
                1_234_567_890 + seq * 700,
                "By",
            )
            .with_label("device", dev),
        );
    }
    for (i, sensor) in [
        "coretemp Package id 0",
        "Core 0",
        "Core 1",
        "nvme Composite",
    ]
    .iter()
    .enumerate()
    {
        metrics.push(
            Metric::new(
                "system.temperature",
                serde_json::Number::from_f64(40.0 + i as f64 + cpu / 10.0).unwrap(),
                "Cel",
            )
            .with_label("sensor", *sensor),
        );
    }
    ReportRecord::new(
        at,
        "10.0.0.1:50000".into(),
        agent.into(),
        format!("host-{agent}"),
        seq,
        fmt_time(at),
        RecordBody::Metrics { metrics },
    )
}

/// 吞吐量量測（`kairos/STORAGE.md`「量測」）：以 `KP_BENCH_RATE` 筆／秒（預設 2000，相當於 1 萬台 ×
/// 每 5 秒一筆）持續 `KP_BENCH_SECS` 秒（預設 30），每筆從放入佇列到收到結果的延遲；結束時印出
/// p50／p99／最大延遲、超過 `timeouts.write_secs`（5 秒，連線會回 `storage_unavailable`）的筆數、
/// 資料庫大小與每筆平均大小。在 kp-build 上執行：
/// `cargo test -p kairos --release -- --ignored --nocapture store_throughput`
///
/// - `KP_BENCH_DURABLE=1`：模擬 1 萬台同時恢復連線的補送尖峰：每筆帶去重鍵，取樣時間從
///   `KP_BENCH_BACKFILL_HOURS`（預設 72）小時前起、每台每筆前進 5 秒（彙總的桶都是已結束的舊桶）；
///   `KP_BENCH_DUP_PERCENT`（預設 0）% 的筆數另外重送一次（模擬結果不明後的重送，應為 `Duplicate`）
/// - `KP_BENCH_INVENTORY=1`：一開始就送出 `KP_BENCH_HOSTS` 台線上主機的狀態表快照，量測期間
///   （第 30 秒）的第一次清冊同步會寫入這麼多列（重新啟動後的情況）
/// - `KP_BENCH_TIMEOUT=1`：與連線（`connection::store_report`）相同地等待：最多 5 秒
///   （`timeouts.write_secs` 預設值），逾時時 writer 尚未認領的取消（連線回 `storage_unavailable`），
///   已認領的再等 1 秒，仍沒有結果算「結果不明」（連線不回應並關閉）。負載是固定速率（不像 Pulse
///   收到 `storage_unavailable` 會退避），用來確認寫入跟不上時的回應時間上限
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "吞吐量量測，手動執行"]
async fn store_throughput() {
    let rate: u64 = std::env::var("KP_BENCH_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let secs: u64 = std::env::var("KP_BENCH_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let hosts: u64 = std::env::var("KP_BENCH_HOSTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let durable = std::env::var("KP_BENCH_DURABLE").is_ok_and(|v| v == "1");
    let backfill_hours: i64 = std::env::var("KP_BENCH_BACKFILL_HOURS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(72);
    let dup_percent: u64 = std::env::var("KP_BENCH_DUP_PERCENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let inventory = std::env::var("KP_BENCH_INVENTORY").is_ok_and(|v| v == "1");
    let emulate_timeout = std::env::var("KP_BENCH_TIMEOUT").is_ok_and(|v| v == "1");
    let dir = std::env::var("KP_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| temp_dir("bench"));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = sqlite_config(&dir);
    config.storage.max_total_bytes = 0;
    if std::env::var("KP_BENCH_SYNC").is_ok_and(|v| v == "full") {
        config.storage.sqlite_synchronous = crate::config::SqliteSynchronous::Full;
    }
    // `KP_BENCH_BACKEND=postgres`：改用 PostgreSQL 後端（連線取自 `KAIROS_TEST_PG_URL`，使用自己的 schema，
    // 結束時刪除；`KP_BENCH_DIR` 有設定時保留 schema 供檢查）
    let postgres_backend = std::env::var("KP_BENCH_BACKEND").is_ok_and(|v| v == "postgres");
    if postgres_backend {
        let url = std::env::var(backend_tests::ENV_URL)
            .expect("KP_BENCH_BACKEND=postgres 需要 KAIROS_TEST_PG_URL");
        let (mut settings, password) = backend_tests::parse_url(&url).expect("KAIROS_TEST_PG_URL");
        if !password.is_empty() {
            let file = dir.join("pg.password");
            std::fs::write(&file, &password).unwrap();
            settings.password_file = file.display().to_string();
        }
        settings.schema = format!("bench_{}_{}", std::process::id(), next_conn_id());
        config.storage.backend = StorageBackend::Postgres;
        config.storage.postgres = settings;
    }
    let storage = start(&config, &dir, Utc::now).await.unwrap();
    let feed = storage.inventory.clone().unwrap();
    let writer = tokio::spawn(storage.writer);
    let sink = storage.reports;
    let agents: Vec<String> = (0..hosts)
        .map(|i| format!("00000000-0000-4000-8000-{i:012x}"))
        .collect();
    if inventory {
        let now = fmt_time(Utc::now());
        feed.send_replace(Some(Arc::new(RegistrySnapshot {
            generated_at: now.clone(),
            online: agents.len(),
            total: agents.len(),
            agents: agents
                .iter()
                .map(|a| AgentSnapshot {
                    agent_id: a.clone(),
                    hostname: format!("host-{a}"),
                    agent_version: "0.3.0".into(),
                    proto: 2,
                    host: None,
                    peer: "10.0.0.1:50000".into(),
                    online: true,
                    conn_id: 1,
                    connected_at: now.clone(),
                    last_seen: now.clone(),
                    offline_after_ms: 75_000,
                    last_report: None,
                })
                .collect(),
        })));
    }
    let backfill_from = Utc::now() - chrono::Duration::hours(backfill_hours);

    // 每 10 ms 送出 rate/100 筆；每筆在自己的 task 等待結果（與連線相同）
    let (lat_tx, mut lat_rx) = mpsc::unbounded_channel::<(Duration, WriteStatus)>();
    let total = rate * secs;
    let per_tick = (rate / 100).max(1);
    let started = Instant::now();
    let mut tick = interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(MissedTickBehavior::Burst);
    let mut sent = 0u64;
    while sent < total {
        tick.tick().await;
        for _ in 0..per_tick.min(total - sent) {
            let host = sent % hosts;
            let agent = &agents[host as usize];
            let n = sent / hosts;
            let mut rec = pulse_like(agent, n, Utc::now(), (sent % 100) as f64);
            if durable {
                rec.ts = fmt_time(backfill_from + chrono::Duration::seconds(n as i64 * 5));
                rec = rec.with_dedup(Some(DedupKey {
                    boot_id: format!("{host:08x}-0000-4000-8000-000000000000"),
                    report_seq: n + 1,
                }));
            }
            let resend = durable && dup_percent > 0 && sent % 100 < dup_percent;
            let copies = if resend {
                vec![rec.clone(), rec]
            } else {
                vec![rec]
            };
            for rec in copies {
                let sink = sink.clone();
                let tx = lat_tx.clone();
                tokio::spawn(async move {
                    let t0 = Instant::now();
                    let status = if emulate_timeout {
                        submit_like_a_connection(&sink, rec).await
                    } else {
                        match sink.submit(rec).await {
                            Ok(mut p) => p.wait().await,
                            Err(e) => WriteStatus::Failed(e),
                        }
                    };
                    let _ = tx.send((t0.elapsed(), status));
                });
            }
            sent += 1;
        }
    }
    drop(lat_tx);
    let mut lat = Vec::with_capacity(total as usize);
    let (mut failed, mut duplicates, mut unavailable, mut unknown) = (0, 0, 0, 0);
    while let Some((d, status)) = lat_rx.recv().await {
        lat.push(d);
        match status {
            WriteStatus::Written => {}
            WriteStatus::Duplicate => duplicates += 1,
            WriteStatus::Failed(r) if r == BENCH_TIMED_OUT => unavailable += 1,
            WriteStatus::Unknown(_) => unknown += 1,
            WriteStatus::Failed(_) => failed += 1,
        }
    }
    // 連線等待寫入的上限（timeouts.write_secs 預設 5 秒）：超過時回 storage_unavailable 或不回應
    let over_timeout = lat.iter().filter(|d| **d >= Duration::from_secs(5)).count();
    let elapsed = started.elapsed();
    // 執行中的 WAL 大小（關閉時會 checkpoint 並刪除）
    let wal_bytes =
        std::fs::metadata(dir.join(format!("{}-wal", sqlite::DB_FILE))).map_or(0, |m| m.len());
    drop(sink);
    writer.await.unwrap();
    lat.sort();
    let pct = |p: f64| lat[((lat.len() as f64 * p) as usize).min(lat.len() - 1)];
    let (bytes, rows, rollups, raw_bytes): (i64, i64, i64, i64) = if postgres_backend {
        // PostgreSQL：資料表與索引的大小（不含 WAL）；reports 的原始大小含它的索引與 TOAST
        let pool = postgres::pool::PgPool::new(config.storage.postgres.clone());
        let conn = postgres::open_read(&pool).unwrap();
        let q = |sql: &str| conn.query_row(sql, (), |r| r.get::<_, i64>(0)).unwrap();
        let out = (
            q(
                "SELECT COALESCE(SUM(pg_total_relation_size(c.oid)), 0)::bigint FROM pg_class c
               JOIN pg_namespace n ON n.oid = c.relnamespace
               WHERE n.nspname = current_schema() AND c.relkind = 'r'",
            ),
            q("SELECT count(*) FROM reports"),
            q("SELECT count(*) FROM rollup_5m") + q("SELECT count(*) FROM rollup_1h"),
            q("SELECT pg_total_relation_size('reports')::bigint"),
        );
        drop(conn);
        if std::env::var("KP_BENCH_DIR").is_err() {
            let admin = postgres::open_write(&pool, Duration::from_secs(5)).unwrap();
            admin
                .execute_batch(&format!("DROP SCHEMA {} CASCADE", pool.schema()))
                .unwrap();
        }
        out
    } else {
        let db = dir.join(sqlite::DB_FILE);
        let conn = sqlite::open_read_only(&db).unwrap();
        let q = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        let pages = q("PRAGMA page_count") - q("PRAGMA freelist_count");
        let raw_bytes: i64 = conn
            .query_row(
                "SELECT sum(pgsize) FROM dbstat WHERE name IN ('reports', 'reports_host_ts')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        (
            pages * q("PRAGMA page_size"),
            q("SELECT count(*) FROM reports"),
            q("SELECT count(*) FROM rollup_5m") + q("SELECT count(*) FROM rollup_1h"),
            raw_bytes,
        )
    };
    println!(
        "BENCH backend={} rate={rate}/s secs={secs} hosts={hosts} durable={durable} backfill_hours={backfill_hours} dup_percent={dup_percent} inventory={inventory}\n\
         BENCH sent={total} elapsed={:.1}s failed={failed} duplicates={duplicates} over_write_timeout_5s={over_timeout} timeout_emulated={emulate_timeout} storage_unavailable={unavailable} unknown={unknown}\n\
         BENCH latency p50={:?} p99={:?} p999={:?} max={:?}\n\
         BENCH db_bytes={bytes} wal_bytes_at_end={wal_bytes} reports={rows} rollup_rows={rollups} bytes_per_report_total={:.0} bytes_per_report_raw_table_and_index={}",
        config.storage.backend.as_str(),
        elapsed.as_secs_f64(),
        pct(0.50),
        pct(0.99),
        pct(0.999),
        lat.last().unwrap(),
        bytes as f64 / rows.max(1) as f64,
        if raw_bytes > 0 {
            format!("{:.0}", raw_bytes as f64 / rows.max(1) as f64)
        } else {
            "n/a（dbstat 未啟用）".into()
        },
    );
    assert_eq!(failed, 0);
    if emulate_timeout {
        // 連線等待寫入的上限：5 秒 + 已開始寫入時的寬限 1 秒（PROTOCOL.md 的 8 秒預算中的這一段）
        assert!(
            *lat.last().unwrap() < Duration::from_millis(6_100),
            "max = {:?}",
            lat.last()
        );
    } else {
        assert_eq!(rows as u64, total);
        if durable {
            assert_eq!(duplicates as u64, total * dup_percent.min(100) / 100);
        } else {
            // 平常的負載（每秒 2000 筆）：遠低於 timeouts.write_secs
            assert!(pct(0.99) < Duration::from_secs(1), "p99 = {:?}", pct(0.99));
        }
    }
    if std::env::var("KP_BENCH_DIR").is_err() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// [`store_throughput`] 的 `KP_BENCH_TIMEOUT=1`：逾時取消的回 `Failed(BENCH_TIMED_OUT)`
const BENCH_TIMED_OUT: &str = "bench: timed out";

/// 與 `connection::store_report` 相同的等待方式（逾時 5 秒、已認領時再等 1 秒）
async fn submit_like_a_connection(sink: &ReportSink, rec: ReportRecord) -> WriteStatus {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut pending = match tokio::time::timeout_at(deadline, sink.submit(rec)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return WriteStatus::Failed(e),
        Err(_) => return WriteStatus::Failed(BENCH_TIMED_OUT.into()),
    };
    match tokio::time::timeout_at(deadline, pending.wait()).await {
        Ok(status) => status,
        Err(_) if pending.cancel() => WriteStatus::Failed(BENCH_TIMED_OUT.into()),
        Err(_) => match tokio::time::timeout(Duration::from_secs(1), pending.wait()).await {
            Ok(status) => status,
            Err(_) => WriteStatus::Unknown("bench: claimed but no result".into()),
        },
    }
}

/// 補送尖峰的剖析（`kairos/STORAGE.md`「量測」）：直接驅動 [`sqlite::SqliteStore`]（不經佇列），
/// 每批 `KP_BENCH_BATCH`（預設 1000）筆 durable report（1 萬台輪流、取樣時間從 72 小時前起），
/// 每批之後做一步維護，每 `KP_BENCH_REPORT_EVERY` 批印出寫入與維護的平均耗時，找出持續寫入時
/// 變慢的部分。`cargo test -p kairos --release -- --ignored --nocapture sqlite_backfill_profile`
#[test]
#[ignore = "剖析，手動執行"]
fn sqlite_backfill_profile() {
    let env = |k: &str, d: u64| -> u64 {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let total = env("KP_BENCH_TOTAL", 4_800_000);
    let batch = env("KP_BENCH_BATCH", 1000);
    let hosts = env("KP_BENCH_HOSTS", 10_000);
    let every = env("KP_BENCH_REPORT_EVERY", 500);
    let durable = env("KP_BENCH_DURABLE", 1) == 1;
    let dir = temp_dir("profile");
    let config = sqlite_config(&dir);
    let mut opts = sqlite::SqliteOptions::from_config(&config, &dir);
    opts.max_total_bytes = 0;
    let (mut store, _) = sqlite::SqliteStore::open(opts, Utc::now).unwrap();
    let agents: Vec<String> = (0..hosts)
        .map(|i| format!("00000000-0000-4000-8000-{i:012x}"))
        .collect();
    let from = Utc::now() - chrono::Duration::hours(72);
    let (mut w, mut m, mut wmax, mut mmax) = (
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
    );
    let started = Instant::now();
    let mut sent = 0u64;
    let mut n_batches = 0u64;
    while sent < total {
        let records: Vec<ReportRecord> = (0..batch.min(total - sent))
            .map(|i| {
                let k = sent + i;
                let host = k % hosts;
                let n = k / hosts;
                let mut rec = pulse_like(&agents[host as usize], n, Utc::now(), (k % 100) as f64);
                if durable {
                    rec.ts = fmt_time(from + chrono::Duration::seconds(n as i64 * 5));
                    rec = rec.with_dedup(Some(DedupKey {
                        boot_id: format!("{host:08x}-0000-4000-8000-000000000000"),
                        report_seq: n + 1,
                    }));
                }
                rec
            })
            .collect();
        sent += records.len() as u64;
        let t = Instant::now();
        let results = store.write_batch(&records);
        let d = t.elapsed();
        assert!(results.iter().all(|r| *r == WriteStatus::Written));
        w += d;
        wmax = wmax.max(d);
        let t = Instant::now();
        store.maintain();
        let d = t.elapsed();
        m += d;
        mmax = mmax.max(d);
        n_batches += 1;
        if n_batches.is_multiple_of(every) {
            let db = std::fs::metadata(dir.join(sqlite::DB_FILE)).map_or(0, |m| m.len());
            println!(
                "PROFILE rows={sent} elapsed={:.0}s rate={:.0}/s write_avg={:?} write_max={wmax:?} maintain_avg={m_avg:?} maintain_max={mmax:?} db_bytes={db} {store:?}",
                started.elapsed().as_secs_f64(),
                (every * batch) as f64 / (w + m).as_secs_f64(),
                w / every as u32,
                m_avg = m / every as u32,
            );
            (w, m, wmax, mmax) = (
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            );
        }
    }
    store.close();
    std::fs::remove_dir_all(&dir).unwrap();
}
