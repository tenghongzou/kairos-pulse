//! 主控台 API 的量測（`#[ignore]`，手動執行；結果見 `kairos/API.md` 第 9 節）。
//!
//! ```bash
//! cargo test -p kairos --release -- --ignored --nocapture api_bench_queries
//! cargo test -p kairos --release -- --ignored --nocapture api_bench_acks_under_load
//! # 可調：KP_BENCH_HOSTS（預設 10000）、KP_BENCH_REQUESTS（每種查詢的次數，預設 300）、
//! #       KP_BENCH_FILL_HOSTS（另外放入 30 天 1 小時彙總的主機數，讓資料庫接近實際大小，預設 300）、
//! #       KP_BENCH_RATE（report 筆／秒，預設 2000）、KP_BENCH_SECS（每個階段的秒數，預設 30）、
//! #       KP_BENCH_CLIENTS（同時查詢的用戶端數，預設 4）
//! ```
//!
//! 以實際的 HTTPS 伺服器（[`super::start`]）與用戶端量測，每個請求一條新的 TLS 連線（含握手）。

use super::tests::{Fixture, fixture, get, hello};
use super::*;
use crate::store::tests::pulse_like;
use rusqlite::params;
use std::time::Duration;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn agent(i: u64) -> String {
    format!("{i:08x}-0000-4000-8000-00000000beef")
}

/// 第 i 台主機的 IP：10.(i%40).((i/40)%256).(i/10240+1)
fn ip(i: u64) -> String {
    format!("10.{}.{}.{}", i % 40, (i / 40) % 256, i / 10_240 + 1)
}

/// 放入 `hosts` 台主機：一半線上（狀態表），一半只在清冊中（離線）；第 0 台有 31 天的彙總與
/// 最近 1 小時的原始資料；另外 `fill` 台各有 30 天的 1 小時彙總（讓資料庫接近實際的大小）
async fn populate(f: &Fixture, hosts: u64, fill: u64) -> String {
    let now = Utc::now();
    let now_ms = now.timestamp_millis();
    // 第 0 台的原始資料（每 5 秒一筆，1 小時；同時建立序列）
    let h0 = agent(0);
    for n in 0..720u64 {
        let at = now - chrono::Duration::seconds(5 * (720 - n as i64));
        f.sink
            .write(pulse_like(&h0, n + 1, at, (n % 100) as f64))
            .await
            .unwrap();
    }
    let conn = f.admin();
    let tx = conn.unchecked_transaction().unwrap();
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO hosts (agent_id, hostname, agent_version, proto, host_fingerprint, tags, os,
                    report_interval_ms, peer, identity_verified, online, first_seen, connected_at, last_seen,
                    offline_after_ms, last_report)
                 VALUES (?1, ?2, ?3, 2, ?4, ?5, ?6, 5000, ?7, 1, 0, ?8, ?8, ?9, 75000, ?10)
                 ON CONFLICT (agent_id) DO NOTHING",
            )
            .unwrap();
        for i in 1..hosts {
            let tags = if i % 10 == 0 {
                r#"["line3"]"#
            } else {
                r#"["env:prod"]"#
            };
            let os = if i % 10 < 7 {
                r#"{"name":"Ubuntu","version":"24.04","arch":"x86_64"}"#
            } else {
                r#"{"name":"Windows","version":"10 IoT LTSC","arch":"x86_64"}"#
            };
            let last_seen = now_ms - (i as i64) * 7_000;
            let report = format!(
                r#"{{"recv_at":"{t}","seq":{i},"ts":"{t}","cpu_usage":{c},"total_memory":32768,"used_memory":{m},"metrics":17}}"#,
                t = crate::storage::fmt_time(now),
                c = (i % 100) as f64,
                m = 1024 + i % 30_000
            );
            stmt.execute(params![
                agent(i),
                format!("host-{i:05}"),
                if i % 5 == 0 { "0.2.0" } else { "0.3.0" },
                format!("sha256:{:064x}", i),
                tags,
                os,
                format!("{}:50000", ip(i)),
                now_ms - 30 * 86_400_000,
                last_seen,
                report,
            ])
            .unwrap();
        }
    }
    tx.commit().unwrap();
    // 彙總：第 0 台 31 天（5 分鐘、1 小時），另外 fill 台 30 天的 1 小時
    let series: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT series_id FROM series").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let host_id = |id: &str| -> i64 {
        conn.query_row("SELECT host_id FROM hosts WHERE agent_id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
    };
    let start = now_ms / 1000 - 31 * 86_400;
    let tx = conn.unchecked_transaction().unwrap();
    {
        let mut m5 = tx
            .prepare("INSERT OR IGNORE INTO rollup_5m VALUES (?1, ?2, ?3, 1, 99, 3000, 60)")
            .unwrap();
        let mut h1 = tx
            .prepare("INSERT OR IGNORE INTO rollup_1h VALUES (?1, ?2, ?3, 1, 99, 36000, 720)")
            .unwrap();
        let h0_id = host_id(&h0);
        let mut t = start - start % 300;
        while t < now_ms / 1000 - 3600 {
            for s in &series {
                m5.execute(params![h0_id, t, s]).unwrap();
            }
            t += 300;
        }
        for i in std::iter::once(0).chain(1..=fill.min(hosts.saturating_sub(1))) {
            let id = host_id(&agent(i));
            let mut t = start - start % 3600;
            while t < now_ms / 1000 - 3600 {
                for s in &series {
                    h1.execute(params![id, t, s]).unwrap();
                }
                t += 3600;
            }
        }
    }
    tx.commit().unwrap();
    conn.execute(
        "UPDATE hosts SET rollup_until = ?2 WHERE agent_id = ?1",
        params![h0, (now_ms / 1000 - 3600) * 1000],
    )
    .unwrap();
    // 狀態表：偶數號線上
    for i in (0..hosts).step_by(2) {
        let mut h = hello(
            &agent(i),
            &format!("host-{i:05}"),
            &format!("{}:50000", ip(i)),
        );
        h.agent_version = if i % 5 == 0 { "0.2.0" } else { "0.3.0" }.into();
        let _ = f
            .registry
            .register(1_000_000 + i, h, Instant::now(), Utc::now())
            .unwrap();
        f.registry.set_durable(&agent(i), 1_000_000 + i, i % 5 != 0);
    }
    f.console.ctx.hosts.invalidate().await;
    h0
}

fn summary(name: &str, mut lat: Vec<Duration>) -> String {
    lat.sort();
    let pct = |p: f64| lat[((lat.len() as f64 * p) as usize).min(lat.len() - 1)];
    format!(
        "BENCH {name:<44} n={:<5} p50={:>8.2?} p99={:>8.2?} max={:>8.2?}",
        lat.len(),
        pct(0.50),
        pct(0.99),
        lat.last().unwrap()
    )
}

async fn measure(f: &Fixture, n: u64, path: &str, cold: bool) -> Vec<Duration> {
    let mut lat = Vec::with_capacity(n as usize);
    for _ in 0..n {
        if cold {
            f.console.ctx.hosts.invalidate().await;
        }
        let t0 = Instant::now();
        let r = get(f, path).await;
        lat.push(t0.elapsed());
        assert_eq!(
            r.status,
            200,
            "{path}: {}",
            String::from_utf8_lossy(&r.body)
        );
    }
    lat
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "量測，手動執行"]
async fn api_bench_queries() {
    let hosts = env_u64("KP_BENCH_HOSTS", 10_000);
    let n = env_u64("KP_BENCH_REQUESTS", 300);
    let fill = env_u64("KP_BENCH_FILL_HOSTS", 300);
    let f = fixture("bench-q", |c| {
        c.limits.max_agents = Some(100_000);
        c.storage.max_hosts = Some(200_000);
    })
    .await;
    let t0 = Instant::now();
    let h0 = populate(&f, hosts, fill).await;
    let conn = f.admin();
    let q = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    println!(
        "BENCH setup hosts={hosts} fill={fill} in {:.1}s; db_bytes={} rollup_1h={} rollup_5m={} reports={}",
        t0.elapsed().as_secs_f64(),
        q(
            "SELECT (SELECT page_count FROM pragma_page_count) * (SELECT page_size FROM pragma_page_size)"
        ),
        q("SELECT count(*) FROM rollup_1h"),
        q("SELECT count(*) FROM rollup_5m"),
        q("SELECT count(*) FROM reports")
    );
    // 列表
    let total = get(&f, "/api/v1/hosts?limit=1").await.json()["page"]["total"].clone();
    println!("BENCH total={total}");
    let cases: Vec<(&str, String, bool)> = vec![
        ("list default (limit 100)", "/api/v1/hosts".into(), false),
        (
            "list offline sort -last_seen",
            "/api/v1/hosts?status=offline&sort=-last_seen".into(),
            false,
        ),
        (
            "list CIDR /16 sort ip",
            "/api/v1/hosts?q=10.12.0.0%2F16&sort=ip".into(),
            false,
        ),
        (
            "list tag+os+version sort hostname",
            "/api/v1/hosts?tag=line3&os=ubuntu&version=0.3.0&sort=hostname".into(),
            false,
        ),
        (
            "list offline_for=1h durable=false q=host-0",
            "/api/v1/hosts?offline_for=1h&durable=false&q=host-0".into(),
            false,
        ),
        ("list limit 1000", "/api/v1/hosts?limit=1000".into(), false),
        (
            "list cold (rebuild index each request)",
            "/api/v1/hosts?status=offline&q=10.0.0.0%2F8&sort=-last_seen".into(),
            true,
        ),
        ("overview", "/api/v1/overview".into(), false),
        ("system health", "/api/v1/system/health".into(), false),
        (
            "trend 30d auto (1h)",
            format!("/api/v1/hosts/{h0}/metrics/system.cpu.usage?range=30d"),
            false,
        ),
        (
            "trend 30d 1h filesystem (3 series)",
            format!("/api/v1/hosts/{h0}/metrics/system.filesystem.available?range=30d"),
            false,
        ),
        (
            "trend 7d 5m (merged to <=1000)",
            format!("/api/v1/hosts/{h0}/metrics/system.cpu.usage?range=7d&resolution=5m"),
            false,
        ),
        (
            "trend 24h auto (5m + raw tail)",
            format!("/api/v1/hosts/{h0}/metrics/system.cpu.usage?range=24h"),
            false,
        ),
        (
            "trend 1h raw",
            format!("/api/v1/hosts/{h0}/metrics/system.cpu.usage?range=1h"),
            false,
        ),
    ];
    for (name, path, cold) in &cases {
        let lat = measure(&f, n, path, *cold).await;
        println!("{}", summary(name, lat));
    }
    // 游標分頁：1000 筆一頁走完全部
    let mut lat = Vec::new();
    let mut cursor = String::new();
    loop {
        let t0 = Instant::now();
        let v = get(
            &f,
            &format!("/api/v1/hosts?limit=1000&sort=-last_seen{cursor}"),
        )
        .await
        .json();
        lat.push(t0.elapsed());
        match v["page"]["next_cursor"].as_str() {
            Some(c) => cursor = format!("&cursor={c}"),
            None => break,
        }
    }
    println!("{}", summary("cursor pages of 1000 (all hosts)", lat));
    // 參考：TLS 握手＋最小的請求
    let lat = measure(&f, n, "/api/v1/me", false).await;
    println!("{}", summary("baseline /me (TLS handshake + auth)", lat));
    f.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "量測，手動執行"]
async fn api_bench_acks_under_load() {
    let hosts = env_u64("KP_BENCH_HOSTS", 10_000);
    let fill = env_u64("KP_BENCH_FILL_HOSTS", 300);
    let rate = env_u64("KP_BENCH_RATE", 2000);
    let secs = env_u64("KP_BENCH_SECS", 30);
    let clients = env_u64("KP_BENCH_CLIENTS", 4);
    let f = Arc::new(
        fixture("bench-ack", |c| {
            c.limits.max_agents = Some(100_000);
            c.storage.max_hosts = Some(200_000);
        })
        .await,
    );
    let h0 = populate(&f, hosts, fill).await;
    let agents: Arc<Vec<String>> = Arc::new((0..hosts).map(agent).collect());

    // 一個階段：每 10 ms 送出 rate/100 筆，量測每筆從放入佇列到收到結果的延遲
    async fn writes(
        sink: crate::storage::ReportSink,
        agents: Arc<Vec<String>>,
        rate: u64,
        secs: u64,
        seq_base: u64,
    ) -> Vec<Duration> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let total = rate * secs;
        let per_tick = (rate / 100).max(1);
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        let mut sent = 0u64;
        while sent < total {
            tick.tick().await;
            for _ in 0..per_tick.min(total - sent) {
                let a = &agents[(sent % agents.len() as u64) as usize];
                let rec = pulse_like(a, seq_base + sent, Utc::now(), (sent % 100) as f64);
                let sink = sink.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let t0 = Instant::now();
                    let ok = match sink.submit(rec).await {
                        Ok(mut p) => matches!(
                            p.wait().await,
                            crate::storage::WriteStatus::Written
                                | crate::storage::WriteStatus::Duplicate
                        ),
                        Err(_) => false,
                    };
                    let _ = tx.send((t0.elapsed(), ok));
                });
                sent += 1;
            }
        }
        drop(tx);
        let mut lat = Vec::new();
        while let Some((d, ok)) = rx.recv().await {
            assert!(ok, "report 應寫入成功");
            lat.push(d);
        }
        lat
    }

    // 階段 1：只有寫入
    let lat = writes(f.sink.clone(), agents.clone(), rate, secs, 1_000_000).await;
    println!("{}", summary(&format!("ack {rate}/s, no API load"), lat));

    // 階段 2、3：寫入同時 clients 個用戶端查詢（列表、重建索引的列表、30 天趨勢、系統健康、總覽）。
    // 用戶端在另一個 runtime（另外的執行緒）上執行，不佔用伺服器 runtime 的排程（仍共用 CPU）。
    // 階段 2 連續查詢（壓力）；階段 3 每個用戶端每秒 1 個請求（接近實際的主控台使用量）
    for (phase, interval_ms) in [("continuous", 0u64), ("1 req/s each", 1000)] {
        let client_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut loaders = Vec::new();
        for c in 0..clients {
            let f = f.clone();
            let stop = stop.clone();
            let h0 = h0.clone();
            loaders.push(client_rt.spawn(async move {
            let paths = [
                "/api/v1/hosts?status=offline&q=10.0.0.0%2F8&sort=-last_seen".to_string(),
                "/api/v1/hosts?limit=1000&sort=hostname".to_string(),
                format!("/api/v1/hosts/{h0}/metrics/system.cpu.usage?range=30d"),
                format!("/api/v1/hosts/{h0}/metrics/system.filesystem.available?range=7d&resolution=5m"),
                "/api/v1/system/health".to_string(),
                "/api/v1/overview".to_string(),
            ];
            let mut lat = Vec::new();
            let mut i = c as usize;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if i.is_multiple_of(7) {
                    f.console.ctx.hosts.invalidate().await;
                }
                let t0 = Instant::now();
                let r = get(&f, &paths[i % paths.len()]).await;
                lat.push(t0.elapsed());
                assert_eq!(r.status, 200);
                i += 1;
                if interval_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(interval_ms)).await;
                }
            }
            lat
        }));
        }
        let seq_base = if interval_ms == 0 {
            2_000_000
        } else {
            3_000_000
        };
        let lat = writes(f.sink.clone(), agents.clone(), rate, secs, seq_base).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut api = Vec::new();
        for l in loaders {
            api.extend(l.await.unwrap());
        }
        client_rt.shutdown_background();
        println!(
            "{}",
            summary(&format!("ack {rate}/s, {clients} API clients {phase}"), lat)
        );
        println!(
            "{} ({:.0} req/s)",
            summary(&format!("API requests during load ({phase})"), api.clone()),
            api.len() as f64 / secs as f64
        );
    }
    match Arc::try_unwrap(f) {
        Ok(f) => f.stop().await,
        Err(_) => panic!("fixture 仍被使用"),
    }
}
