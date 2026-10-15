//! [`Importer`] 的測試：schema 1／2／沒有 schema、壞行、重複匯入、durable 與鏡像的去重、保留期限、
//! 彙總（含與 Kairos 尚未寫出的彙總、遲到資料）、主機清冊、時間範圍、試執行與獨占鎖

use super::*;
use crate::config::SqliteSynchronous;
use crate::registry::next_conn_id;
use crate::storage::WriteStatus;
use crate::store::ReportStore;
use crate::store::sqlite::{DB_FILE, SqliteStore, export_reports};
use chrono::TimeZone;
use std::path::PathBuf;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kairos-import-{}-{}-{}",
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
        max_series_per_host: crate::config::DEFAULT_MAX_SERIES_PER_HOST,
        mtls: true,
    }
}

fn at(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap()
}

/// 測試用的「現在」：2026-10-09 12:00:00 UTC
fn noon() -> DateTime<Utc> {
    at(2026, 10, 9, 12, 0, 0)
}

const A: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
const B: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const BOOT: &str = "0b1c2d3e-0000-4000-8000-000000000001";

fn settings(dry_run: bool) -> ImportSettings {
    ImportSettings {
        dry_run,
        since_ms: None,
        until_ms: None,
        now: noon(),
    }
}

fn legacy(agent: &str, seq: u64, recv_at: DateTime<Utc>, cpu: f32) -> ReportRecord {
    ReportRecord::new(
        recv_at,
        "10.0.0.1:5000".into(),
        agent.into(),
        "web-01".into(),
        seq,
        crate::storage::fmt_time(recv_at),
        RecordBody::Legacy {
            cpu_usage: cpu,
            total_memory: 4096,
            used_memory: 1024,
        },
    )
}

fn metrics(agent: &str, seq: u64, recv_at: DateTime<Utc>, cpu: f64) -> ReportRecord {
    let metrics = vec![
        Metric::new(
            "system.cpu.usage",
            serde_json::Number::from_f64(cpu).unwrap(),
            "%",
        ),
        Metric::new("system.memory.total", 51_539_607_552_u64, "By"),
        Metric::new("system.filesystem.total", 502_468_108_288_u64, "By")
            .with_label("mountpoint", "/")
            .with_label("fstype", "ext4"),
    ];
    ReportRecord::new(
        recv_at,
        "10.0.0.2:5000".into(),
        agent.into(),
        "db-01".into(),
        seq,
        crate::storage::fmt_time(recv_at),
        RecordBody::Metrics { metrics },
    )
}

fn durable(record: ReportRecord, report_seq: u64, ts: DateTime<Utc>) -> ReportRecord {
    let mut r = record.with_dedup(Some(DedupKey {
        boot_id: BOOT.into(),
        report_seq,
    }));
    r.ts = crate::storage::fmt_time(ts);
    r
}

fn line(r: &ReportRecord) -> String {
    serde_json::to_string(r).unwrap()
}

/// 0.1.0 的行：沒有 `schema`
fn line_without_schema(r: &ReportRecord) -> String {
    let mut v: serde_json::Value = serde_json::to_value(r).unwrap();
    v.as_object_mut().unwrap().remove("schema");
    v.to_string()
}

fn jsonl(lines: &[String]) -> Vec<u8> {
    let mut out = lines.join("\n");
    out.push('\n');
    out.into_bytes()
}

fn import(dir: &Path, data: &[u8], settings: ImportSettings) -> (FileReport, Finished) {
    let mut imp = Importer::open(&opts(dir), settings).unwrap();
    let report = imp.import_reader(data, &mut |_| {}).unwrap();
    let finished = imp.finish().unwrap();
    (report, finished)
}

fn conn(dir: &Path) -> Connection {
    open_read_only(&dir.join(DB_FILE)).unwrap()
}

fn query_i64(dir: &Path, sql: &str) -> i64 {
    conn(dir).query_row(sql, [], |r| r.get(0)).unwrap()
}

/// CPU 的 5 分鐘與 1 小時彙總：(總筆數, 總和)
fn cpu_totals(dir: &Path, table: &str) -> (i64, f64) {
    conn(dir)
        .query_row(
            &format!(
                "SELECT coalesce(sum(cnt), 0), coalesce(sum(vsum), 0) FROM {table}
                 JOIN series USING (series_id) WHERE name = 'system.cpu.usage'"
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn exported(dir: &Path) -> Vec<ReportRecord> {
    let mut out = Vec::new();
    export_reports(&conn(dir), 0, None, |r| {
        out.push(r.clone());
        Ok(())
    })
    .unwrap();
    out
}

#[test]
fn schema_1_schema_2_and_lines_without_schema_are_imported_as_written() {
    let dir = temp_dir("schemas");
    let day = at(2026, 10, 8, 3, 0, 0);
    let records = vec![
        legacy(A, 1, day, 10.5),
        metrics(B, 1, day + chrono::Duration::seconds(5), 20.0),
        legacy(A, 2, day + chrono::Duration::seconds(10), 30.0),
    ];
    let data = jsonl(&[
        line(&records[0]),
        line(&records[1]),
        line_without_schema(&records[2]),
    ]);
    let (report, finished) = import(&dir, &data, settings(false));
    // 關閉時 checkpoint 並刪除 -wal（exclusive 模式不使用 -shm），Kairos 之後照常開啟
    assert!(!dir.join("kairos.db-wal").exists());
    assert!(!dir.join("kairos.db-shm").exists());
    assert_eq!(
        report.counts,
        Counts {
            lines_read: 3,
            imported_raw: 3,
            ..Counts::default()
        }
    );
    assert_eq!(finished.new_hosts, 2);
    // 匯出後與原本的行相同（durable 以外的取樣時間 = recv_at）
    let mut got = exported(&dir);
    got.sort_by_key(|r| r.recv_at);
    assert_eq!(got, records);
    // 主機：source = import、離線、時間取自資料
    let (source, online, first, last): (String, i64, i64, i64) = conn(&dir)
        .query_row(
            "SELECT source, online, first_seen, last_seen FROM hosts WHERE agent_id = ?1",
            [A],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(source, HOST_SOURCE_IMPORT);
    assert_eq!(online, 0);
    assert_eq!(first, day.timestamp_millis());
    assert_eq!(
        last,
        (day + chrono::Duration::seconds(10)).timestamp_millis()
    );
    // 彙總：每筆的 CPU 都只算一次，rollup_until 推進到最後一個桶的結束
    assert_eq!(cpu_totals(&dir, "rollup_5m"), (3, 60.5));
    assert_eq!(cpu_totals(&dir, "rollup_1h"), (3, 60.5));
    assert_eq!(
        query_i64(
            &dir,
            &format!("SELECT rollup_until FROM hosts WHERE agent_id = '{A}'")
        ),
        (day + chrono::Duration::minutes(5)).timestamp_millis()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bad_lines_are_skipped_counted_and_sampled() {
    let dir = temp_dir("bad");
    let day = at(2026, 10, 8, 3, 0, 0);
    let good = line(&legacy(A, 1, day, 1.0));
    let mut missing = serde_json::to_value(legacy(A, 2, day, 1.0)).unwrap();
    missing.as_object_mut().unwrap().remove("agent_id");
    let mut bad_schema = serde_json::to_value(legacy(A, 3, day, 1.0)).unwrap();
    bad_schema["schema"] = 3.into();
    let mut bad_agent = serde_json::to_value(legacy(A, 4, day, 1.0)).unwrap();
    bad_agent["agent_id"] = "web-01".into();
    let mut half_durable = serde_json::to_value(legacy(A, 5, day, 1.0)).unwrap();
    half_durable["boot_id"] = BOOT.into();
    let mut out_of_range = serde_json::to_value(legacy(A, 6, day, 1.0)).unwrap();
    out_of_range["cpu_usage"] = 250.0.into();
    let mut data = jsonl(&[
        good.clone(),
        "not json".to_string(),
        String::new(),
        missing.to_string(),
        bad_schema.to_string(),
        bad_agent.to_string(),
        half_durable.to_string(),
        out_of_range.to_string(),
        "{\"schema\":2}".to_string(),
    ]);
    // 過長的行與沒有換行的最後一行
    data.extend(vec![b'x'; MAX_LINE_BYTES + 10]);
    data.push(b'\n');
    data.extend_from_slice(good.replace("\"seq\":1", "\"seq\":9").as_bytes());
    let (report, _) = import(&dir, &data, settings(false));
    assert_eq!(report.counts.lines_read, 10, "空行不計");
    assert_eq!(report.counts.imported_raw, 1);
    assert_eq!(report.counts.bad_lines, 9);
    assert!(report.incomplete_tail);
    let lines: Vec<u64> = report.bad_samples.iter().map(|b| b.line).collect();
    assert_eq!(lines, vec![2, 4, 5, 6, 7], "前 5 個壞行（行號含空行）");
    assert_eq!(report.bad_samples[0].reason, "不是合法的 JSON");
    assert!(
        report.bad_samples[1].reason.contains("agent_id"),
        "{:?}",
        report.bad_samples[1]
    );
    assert!(report.bad_samples[2].reason.contains("schema 3"));
    assert!(report.bad_samples[3].reason.contains("UUID"));
    assert!(report.bad_samples[4].reason.contains("boot_id"));
    assert_eq!(query_i64(&dir, "SELECT count(*) FROM reports"), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn importing_the_same_file_again_adds_nothing() {
    let dir = temp_dir("again");
    let day = at(2026, 10, 8, 3, 0, 0);
    let lines: Vec<String> = (0..50)
        .map(|i| {
            line(&metrics(
                if i % 2 == 0 { A } else { B },
                i,
                day + chrono::Duration::seconds(i as i64 * 5),
                i as f64,
            ))
        })
        .collect();
    // 同一行在檔案中出現兩次（例如複本合併）也只算一次
    let mut with_copy = lines.clone();
    with_copy.push(lines[0].clone());
    let (first, _) = import(&dir, &jsonl(&with_copy), settings(false));
    assert_eq!(first.counts.imported_raw, 50);
    assert_eq!(first.counts.duplicates, 1);
    let rollup = cpu_totals(&dir, "rollup_5m");
    assert_eq!(rollup.0, 50);
    let (again, finished) = import(&dir, &jsonl(&lines), settings(false));
    assert_eq!(
        again.counts,
        Counts {
            lines_read: 50,
            duplicates: 50,
            ..Counts::default()
        }
    );
    assert_eq!(finished.new_hosts, 0);
    assert_eq!(query_i64(&dir, "SELECT count(*) FROM reports"), 50);
    assert_eq!(cpu_totals(&dir, "rollup_5m"), rollup);
    assert_eq!(cpu_totals(&dir, "rollup_1h"), rollup);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn reports_already_stored_by_kairos_are_duplicates() {
    let dir = temp_dir("stored");
    fn clock() -> DateTime<Utc> {
        noon()
    }
    let day = at(2026, 10, 9, 11, 0, 0);
    // Kairos 已保存：durable 的一筆，與一筆非 durable（JSONL 鏡像中也有）
    let d = durable(legacy(A, 7, day, 5.0), 42, day - chrono::Duration::hours(1));
    let mirror = metrics(B, 3, day, 7.0);
    let (mut store, _) = SqliteStore::open(opts(&dir), clock).unwrap();
    assert_eq!(
        store.write_batch(&[d.clone(), mirror.clone()]),
        [WriteStatus::Written, WriteStatus::Written]
    );
    store.close();
    drop(store);
    // 鏡像中的同一筆、durable 的重送（recv_at 不同，內容雜湊不同，但去重鍵相同）、與一筆新的
    let mut resent = d.clone();
    resent.recv_at = day + chrono::Duration::minutes(3);
    let new = metrics(B, 4, day + chrono::Duration::seconds(5), 9.0);
    let data = jsonl(&[line(&mirror), line(&resent), line(&new)]);
    let (dry, _) = import(&dir, &data, settings(true));
    let (report, _) = import(&dir, &data, settings(false));
    assert_eq!(dry.counts, report.counts, "試執行與實際的計數相同");
    assert_eq!(report.counts.duplicates, 2);
    assert_eq!(report.counts.imported_raw, 1);
    assert_eq!(query_i64(&dir, "SELECT count(*) FROM reports"), 3);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn durable_lines_use_the_sample_time_and_keep_the_dedup_key() {
    let dir = temp_dir("durable");
    let recv = at(2026, 10, 9, 10, 0, 0);
    let sampled = recv - chrono::Duration::hours(30);
    let r = durable(metrics(A, 1, recv, 3.0), 7, sampled);
    let (report, _) = import(&dir, &jsonl(&[line(&r)]), settings(false));
    assert_eq!(report.counts.imported_raw, 1);
    let (ts, lag): (i64, i64) = conn(&dir)
        .query_row("SELECT ts, recv_lag FROM reports", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(ts, sampled.timestamp_millis());
    assert_eq!(lag, recv.timestamp_millis() - ts);
    assert_eq!(exported(&dir), vec![r.clone()]);
    // Pulse 之後重送同一筆：Kairos 的唯一索引判定為重複
    fn clock() -> DateTime<Utc> {
        noon()
    }
    let (mut store, _) = SqliteStore::open(opts(&dir), clock).unwrap();
    assert_eq!(store.write_batch(&[r]), [WriteStatus::Duplicate]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn old_samples_go_to_the_rollups_only_or_are_skipped() {
    let dir = temp_dir("retention");
    // 原始資料保留到 2026-10-02（今天 10-09 − 7 天）；5 分鐘彙總到 09-08；1 小時到 2025-09-04
    let raw = legacy(A, 1, at(2026, 10, 2, 0, 0, 1), 1.0);
    let only_rollup = legacy(A, 2, at(2026, 10, 1, 23, 59, 0), 2.0);
    let only_1h = legacy(A, 3, at(2026, 9, 1, 0, 0, 0), 4.0);
    let expired = legacy(A, 4, at(2025, 9, 1, 0, 0, 0), 8.0);
    let data = jsonl(&[
        line(&raw),
        line(&only_rollup),
        line(&only_1h),
        line(&expired),
    ]);
    let (report, _) = import(&dir, &data, settings(false));
    assert_eq!(
        report.counts,
        Counts {
            lines_read: 4,
            imported_raw: 1,
            imported_rollup_only: 2,
            expired: 1,
            ..Counts::default()
        }
    );
    assert_eq!(query_i64(&dir, "SELECT count(*) FROM reports"), 1);
    assert_eq!(
        cpu_totals(&dir, "rollup_5m"),
        (2, 3.0),
        "超過 5 分鐘彙總期限的不寫 5 分鐘桶"
    );
    assert_eq!(cpu_totals(&dir, "rollup_1h"), (3, 7.0));
    // 再匯入一次：只進彙總的也不會重複計算
    let (again, _) = import(&dir, &data, settings(false));
    assert_eq!(again.counts.duplicates, 3);
    assert_eq!(again.counts.expired, 1);
    assert_eq!(cpu_totals(&dir, "rollup_1h"), (3, 7.0));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn rollups_stay_exact_with_unflushed_kairos_data_and_late_files() {
    let dir = temp_dir("rollup");
    fn clock() -> DateTime<Utc> {
        noon()
    }
    // Kairos 寫了即時資料，彙總還在記憶體中就停止（原始資料在 rollup_until 之後）
    let live = at(2026, 10, 9, 11, 0, 0);
    let (mut store, _) = SqliteStore::open(opts(&dir), clock).unwrap();
    let live_records: Vec<ReportRecord> = (0..3)
        .map(|i| legacy(A, 100 + i, live + chrono::Duration::seconds(i as i64), 1.0))
        .collect();
    store.write_batch(&live_records);
    store.close();
    drop(store);
    assert_eq!(cpu_totals(&dir, "rollup_5m"), (0, 0.0));
    // 先匯入較新的一天，再匯入較舊的一天（遲到資料走直接合併）
    let newer: Vec<String> = (0..10)
        .map(|i| {
            line(&legacy(
                A,
                i,
                at(2026, 10, 8, 0, 0, 0) + chrono::Duration::minutes(i as i64),
                2.0,
            ))
        })
        .collect();
    let older: Vec<String> = (0..10)
        .map(|i| {
            line(&legacy(
                A,
                i,
                at(2026, 10, 7, 0, 0, 0) + chrono::Duration::minutes(i as i64),
                4.0,
            ))
        })
        .collect();
    let mut imp = Importer::open(&opts(&dir), settings(false)).unwrap();
    imp.import_reader(jsonl(&newer).as_slice(), &mut |_| {})
        .unwrap();
    imp.import_reader(jsonl(&older).as_slice(), &mut |_| {})
        .unwrap();
    imp.finish().unwrap();
    // 即時資料 3 筆 + 匯入 20 筆，各算一次
    assert_eq!(cpu_totals(&dir, "rollup_5m"), (23, 3.0 + 20.0 + 40.0));
    assert_eq!(cpu_totals(&dir, "rollup_1h"), (23, 63.0));
    // Kairos 再啟動：不會再從原始資料補回已寫出的部分
    let (mut store, _) = SqliteStore::open(opts(&dir), clock).unwrap();
    while store.maintain() {}
    store.close();
    drop(store);
    assert_eq!(cpu_totals(&dir, "rollup_5m"), (23, 63.0));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn existing_hosts_keep_newer_information() {
    let dir = temp_dir("hosts");
    fn clock() -> DateTime<Utc> {
        noon()
    }
    let (mut store, _) = SqliteStore::open(opts(&dir), clock).unwrap();
    let mut live = legacy(A, 1, at(2026, 10, 9, 11, 0, 0), 1.0);
    live.hostname = "web-01-new".into();
    store.write_batch(&[live]);
    store.close();
    drop(store);
    let old = legacy(A, 1, at(2026, 10, 5, 0, 0, 0), 1.0);
    let (_, finished) = import(&dir, &jsonl(&[line(&old)]), settings(false));
    assert_eq!(finished.new_hosts, 0);
    let (hostname, source, first): (String, Option<String>, i64) = conn(&dir)
        .query_row(
            "SELECT hostname, source, first_seen FROM hosts WHERE agent_id = ?1",
            [A],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(hostname, "web-01-new", "較舊的匯入資料不覆蓋主機名稱");
    assert_eq!(source, None, "由連線建立的主機不標為匯入");
    assert_eq!(
        first,
        at(2026, 10, 5, 0, 0, 0).timestamp_millis(),
        "first_seen 往前推"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn time_range_and_host_limit() {
    let dir = temp_dir("range");
    let day = at(2026, 10, 8, 0, 0, 0);
    let data = jsonl(&[
        line(&legacy(A, 1, day, 1.0)),
        line(&legacy(A, 2, day + chrono::Duration::hours(2), 1.0)),
        line(&legacy(B, 1, day + chrono::Duration::hours(1), 1.0)),
    ]);
    let mut s = settings(false);
    s.since_ms = Some((day + chrono::Duration::minutes(30)).timestamp_millis());
    s.until_ms = Some((day + chrono::Duration::hours(2)).timestamp_millis());
    let (report, _) = import(&dir, &data, s);
    assert_eq!(report.counts.out_of_range, 2);
    assert_eq!(report.counts.imported_raw, 1);
    // 主機數上限
    let dir2 = temp_dir("limit");
    let mut o = opts(&dir2);
    o.max_hosts = 1;
    let mut imp = Importer::open(&o, settings(false)).unwrap();
    let report = imp.import_reader(data.as_slice(), &mut |_| {}).unwrap();
    imp.finish().unwrap();
    assert_eq!(report.counts.imported_raw, 2);
    assert_eq!(report.counts.rejected, 1);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&dir2).unwrap();
}

#[test]
fn dry_run_counts_like_the_real_import_and_writes_nothing() {
    let dir = temp_dir("dry");
    let day = at(2026, 10, 8, 0, 0, 0);
    let mut lines: Vec<String> = (0..20)
        .map(|i| {
            line(&metrics(
                A,
                i,
                day + chrono::Duration::seconds(i as i64),
                1.0,
            ))
        })
        .collect();
    lines.push(lines[3].clone());
    lines.push("{".into());
    lines.push(line(&legacy(B, 1, at(2020, 1, 1, 0, 0, 0), 1.0)));
    let data = jsonl(&lines);
    // 資料庫不存在：試執行不建立
    let (dry, finished) = import(&dir, &data, settings(true));
    assert!(!dir.join(DB_FILE).exists());
    assert_eq!(finished.new_hosts, 1);
    let (real, finished) = import(&dir, &data, settings(false));
    assert_eq!(dry.counts, real.counts);
    assert_eq!(finished.new_hosts, 1);
    // 匯入之後再試執行：全部為重複
    let (dry, _) = import(&dir, &data, settings(true));
    assert_eq!(dry.counts.duplicates, 21);
    assert_eq!(dry.counts.imported(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn import_needs_the_database_exclusively() {
    let dir = temp_dir("lock");
    fn clock() -> DateTime<Utc> {
        noon()
    }
    let data = jsonl(&[line(&legacy(A, 1, at(2026, 10, 8, 0, 0, 0), 1.0))]);
    // Kairos 執行中：拒絕寫入，試執行可以
    let (store, _) = SqliteStore::open(opts(&dir), clock).unwrap();
    let err = Importer::open(&opts(&dir), settings(false)).err().unwrap();
    assert_eq!(err.kind(), ErrorKind::ResourceBusy, "{err}");
    let (dry, _) = import(&dir, &data, settings(true));
    assert_eq!(dry.counts.imported_raw, 1);
    drop(store);
    // 匯入期間其他連線（例如 Kairos 啟動）無法讀取資料庫
    let mut imp = Importer::open(&opts(&dir), settings(false)).unwrap();
    let other = Connection::open(dir.join(DB_FILE)).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    let busy = other.query_row("SELECT count(*) FROM hosts", [], |r| r.get::<_, i64>(0));
    assert!(busy.is_err(), "{busy:?}");
    imp.import_reader(data.as_slice(), &mut |_| {}).unwrap();
    imp.finish().unwrap();
    // 結束後 Kairos 可以開啟
    let (mut store, _) = SqliteStore::open(opts(&dir), clock).unwrap();
    while store.maintain() {}
    drop(store);
    assert_eq!(query_i64(&dir, "SELECT count(*) FROM reports"), 1);
    drop(other);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn content_hash_is_stable_and_covers_the_content() {
    let r = legacy(A, 1, at(2026, 10, 8, 0, 0, 0), 1.0);
    // 保存在資料庫中：不同版本、平台必須算出相同的值
    assert_eq!(content_hash(&r), content_hash(&r.clone()));
    let mut other = r.clone();
    other.seq = 2;
    assert_ne!(content_hash(&r), content_hash(&other));
    let mut other = r.clone();
    other.recv_at += chrono::Duration::milliseconds(1);
    assert_ne!(content_hash(&r), content_hash(&other));
    // 欄位順序、空白不同的同一行：解析後相同
    let text = line(&r);
    let reordered: serde_json::Value = serde_json::from_str(&text).unwrap();
    let spaced = serde_json::to_string_pretty(&reordered)
        .unwrap()
        .replace('\n', " ");
    assert_eq!(
        content_hash(&parse_line(spaced.as_bytes()).unwrap()),
        content_hash(&parse_line(text.as_bytes()).unwrap())
    );
}

#[test]
#[ignore]
fn import_throughput() {
    // cargo test -p kairos --release -- --ignored --nocapture import_throughput
    // 可調：KP_IMPORT_LINES（預設 1000000）、KP_IMPORT_HOSTS（預設 200）、KP_IMPORT_DAYS_AGO（資料的結束時間，
    // 預設 1 天前；大於 storage.raw_retention_days 時走「只進彙總」）、KP_IMPORT_DIR（保留資料庫與檔案）
    use std::io::Write as _;
    use std::time::Instant;
    let env = |k: &str, d: u64| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let lines = env("KP_IMPORT_LINES", 1_000_000);
    let hosts = env("KP_IMPORT_HOSTS", 200);
    let days_ago = env("KP_IMPORT_DAYS_AGO", 1);
    let dir = std::env::var("KP_IMPORT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| temp_dir("bench"));
    std::fs::create_dir_all(&dir).unwrap();
    let now = Utc::now();
    let per_host = lines.div_ceil(hosts);
    let end = now - chrono::Duration::days(days_ago as i64);
    let start = end - chrono::Duration::seconds(per_host as i64 * 5);
    let file = dir.join("reports-bench.jsonl");
    let t = Instant::now();
    {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&file).unwrap());
        let agents: Vec<String> = (0..hosts)
            .map(|i| format!("00000000-0000-4000-8000-{i:012x}"))
            .collect();
        let mut n = 0;
        'outer: for step in 0..per_host {
            let recv = start + chrono::Duration::seconds(step as i64 * 5);
            for (h, agent) in agents.iter().enumerate() {
                if n == lines {
                    break 'outer;
                }
                // 半數 schema 2（17 個指標，Pulse 0.2.0 的典型值）、半數 schema 1
                let r = if h % 2 == 0 {
                    let mut m = metrics(agent, step, recv, (step % 100) as f64);
                    if let RecordBody::Metrics { metrics } = &mut m.body {
                        for i in 0..14 {
                            metrics.push(
                                Metric::new("system.network.received", step * 1000 + i, "By")
                                    .with_label("interface", format!("eth{i}")),
                            );
                        }
                    }
                    m
                } else {
                    legacy(agent, step, recv, (step % 100) as f32)
                };
                serde_json::to_writer(&mut out, &r).unwrap();
                out.write_all(b"\n").unwrap();
                n += 1;
            }
        }
        out.flush().unwrap();
    }
    let size = std::fs::metadata(&file).unwrap().len();
    eprintln!(
        "產生 {lines} 行（{hosts} 台，{} MB）：{:.1} 秒",
        size / 1_000_000,
        t.elapsed().as_secs_f64()
    );
    let o = SqliteOptions {
        max_total_bytes: 0,
        max_hosts: 100_000,
        max_series_per_host: crate::config::DEFAULT_MAX_SERIES_PER_HOST,
        ..opts(&dir)
    };
    for (label, dry) in [
        ("試執行", true),
        ("匯入", false),
        ("再匯入（全部重複）", false),
    ] {
        let t = Instant::now();
        let mut imp = Importer::open(
            &o,
            ImportSettings {
                dry_run: dry,
                since_ms: None,
                until_ms: None,
                now,
            },
        )
        .unwrap();
        let reader =
            std::io::BufReader::with_capacity(256 * 1024, std::fs::File::open(&file).unwrap());
        let report = imp.import_reader(reader, &mut |_| {}).unwrap();
        let finished = imp.finish().unwrap();
        let secs = t.elapsed().as_secs_f64();
        eprintln!(
            "{label}：{secs:.1} 秒，{:.0} 行／秒；{:?}；資料庫 {} MB",
            report.counts.lines_read as f64 / secs,
            report.counts,
            std::fs::metadata(dir.join(DB_FILE)).map_or(0, |m| m.len() / 1_000_000)
        );
        let _ = finished;
    }
    // 去重鍵（import_keys）佔用的空間：刪掉後 VACUUM，比較前後的大小
    let db = Connection::open(dir.join(DB_FILE)).unwrap();
    let size = |c: &Connection| -> i64 {
        c.query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    db.execute_batch("VACUUM").unwrap();
    let before = size(&db);
    db.execute_batch("DELETE FROM import_keys; VACUUM").unwrap();
    let after = size(&db);
    eprintln!(
        "資料庫（VACUUM 後）{} MB，其中 import_keys {} MB（每行 {:.1} bytes）",
        before / 1_000_000,
        (before - after) / 1_000_000,
        (before - after) as f64 / lines as f64
    );
    drop(db);
    if std::env::var("KP_IMPORT_DIR").is_err() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
