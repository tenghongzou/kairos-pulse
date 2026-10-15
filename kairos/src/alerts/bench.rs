//! 告警引擎對 report `ack` 延遲的影響（`#[ignore]`，手動執行；結果見 `kairos/ALERTS.md` 第 9 節）。
//!
//! ```bash
//! cargo test -p kairos --release -- --ignored --nocapture alerts_bench_acks_under_load
//! # 可調：KP_BENCH_HOSTS（預設 10000）、KP_BENCH_RATE（report 筆／秒，預設 2000）、KP_BENCH_SECS（每階段秒數，預設 30）
//! ```
//!
//! 與 `api/bench.rs` 相同的量測方法：每 10 ms 送出 rate/100 筆 report，量測每筆從放入佇列到收到結果
//! （即連線回 `ack`）的延遲。三個階段：
//! 1. 沒有告警引擎；
//! 2. 啟動告警引擎（1 萬台主機，一半線上；一條 CPU > 50% 持續 0 秒的規則讓約 2,500 台同時觸發，
//!    所有規則都通知到一個 Webhook）——含第一個判定週期一次開出數千個事件的寫入；
//! 3. 穩定期，另有每 10 秒 200 台主機上下線的擾動（產生主機離線、網段彙總、恢復）。

use chrono::Utc;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{AlertService, StartParams};
use crate::alerts::notify::fake::FakeHttp;
use crate::api::tests::{fixture, hello};
use crate::registry::LastReport;
use crate::shutdown::Shutdown;
use crate::store::sqlite::SqliteOptions;
use crate::store::tests::pulse_like;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn agent(i: u64) -> String {
    format!("{i:08x}-0000-4000-8000-00000000beef")
}

fn ip(i: u64) -> String {
    format!("10.{}.{}.{}:50000", i % 40, (i / 40) % 256, i / 10_240 + 1)
}

fn summary(name: &str, mut lat: Vec<Duration>) -> String {
    lat.sort();
    let pct = |p: f64| lat[((lat.len() as f64 * p) as usize).min(lat.len() - 1)];
    format!(
        "BENCH {name:<52} n={:<6} p50={:>8.2?} p99={:>8.2?} max={:>8.2?}",
        lat.len(),
        pct(0.50),
        pct(0.99),
        lat.last().unwrap()
    )
}

async fn writes(
    sink: crate::storage::ReportSink,
    hosts: u64,
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
            let a = agent(sent % hosts);
            let rec = pulse_like(&a, seq_base + sent, Utc::now(), (sent % 100) as f64);
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

fn real_clock() -> i64 {
    Utc::now().timestamp_millis()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "量測，手動執行"]
async fn alerts_bench_acks_under_load() {
    let hosts = env_u64("KP_BENCH_HOSTS", 10_000);
    let rate = env_u64("KP_BENCH_RATE", 2000);
    let secs = env_u64("KP_BENCH_SECS", 30);
    let f = fixture("bench-alerts", |c| {
        c.limits.max_agents = Some(100_000);
        c.storage.max_hosts = Some(200_000);
        c.alerts.warmup_secs = 60;
    })
    .await;
    // 狀態表：偶數號線上，其中一半 CPU 75%（觸發 CPU > 50% 的規則）
    for i in (0..hosts).step_by(2) {
        let conn_id = 1_000_000 + i;
        f.registry
            .register(
                conn_id,
                hello(&agent(i), &format!("host-{i:05}"), &ip(i)),
                Instant::now(),
                Utc::now(),
            )
            .unwrap();
        let cpu = if i % 4 == 0 { 75.0 } else { 20.0 };
        f.registry.set_last_report(
            &agent(i),
            conn_id,
            LastReport {
                recv_at: crate::storage::fmt_time(Utc::now()),
                seq: 1,
                ts: crate::storage::fmt_time(Utc::now()),
                cpu_usage: Some(cpu),
                total_memory: Some(1000),
                used_memory: Some(500),
                metrics: Some(3),
                detail: Default::default(),
            },
        );
    }

    let lat = writes(f.sink.clone(), hosts, rate, secs, 1_000_000).await;
    println!(
        "{}",
        summary(&format!("ack {rate}/s, alerts engine off"), lat)
    );

    // 告警引擎：一個 Webhook 管道、所有規則通知到它、CPU > 50% 持續 0 秒的規則
    let hook = FakeHttp::start().await;
    let shutdown = Arc::new(Shutdown::new());
    let db_path =
        SqliteOptions::from_config(&f.config, std::path::Path::new(&f.config.storage.data_dir))
            .path;
    {
        let conn = f.admin();
        let sb = super::secrets::SecretBox::open(std::path::Path::new(&f.config.storage.data_dir))
            .unwrap();
        let ch = crate::alerts::model::Channel {
            channel_id: "c-bench".into(),
            name: "bench".into(),
            kind: crate::alerts::model::ChannelKind::Webhook,
            enabled: true,
            config: json!({ "format": "kairos", "allow_http": true, "min_severity": "warning" }),
            secrets: json!({ "url": format!("http://{}/hook", hook.addr) }),
            state: "ok".into(),
            consecutive_failures: 0,
            failed_since: None,
            last_attempt_at: None,
            last_success_at: None,
            last_error_code: None,
            last_error: None,
            created_at: 0,
            created_by: "bench".into(),
            updated_at: 0,
            updated_by: "bench".into(),
        };
        let sealed = sb.seal(&ch.secrets).unwrap();
        super::store::save_channel(&conn, &ch, Some(sealed)).unwrap();
        super::store::ensure_builtin_rules(&conn, real_clock()).unwrap();
        for mut r in super::store::load_rules(&conn).unwrap() {
            r.channels = vec!["c-bench".into()];
            super::store::save_rule(&conn, &r).unwrap();
        }
        let mut cpu = super::rules::builtin_default(super::rules::ids::HOST_OFFLINE).unwrap();
        cpu.rule_id = "r-bench-cpu".into();
        cpu.builtin = false;
        cpu.kind = super::rules::kind::METRIC_THRESHOLD.into();
        cpu.name = "CPU > 50%".into();
        cpu.severity = crate::alerts::model::Severity::Warning;
        cpu.params = json!({ "metric": "cpu", "op": ">", "threshold": 50.0, "for_secs": 0 });
        cpu.channels = vec!["c-bench".into()];
        super::store::save_rule(&conn, &cpu).unwrap();
    }
    let (svc, thread) = AlertService::start(StartParams {
        config: Arc::new(f.config.clone()),
        data_dir: std::path::PathBuf::from(&f.config.storage.data_dir),
        db_path,
        registry: f.registry.clone(),
        write_stats: Some(f.sink.stats().clone()),
        per_ip: None,
        tls: None,
        enroll: None,
        agent_listen: None,
        shutdown: shutdown.clone(),
        enroll_closed_default: false,
        clock: real_clock,
    })
    .await
    .unwrap();
    let lat = writes(f.sink.clone(), hosts, rate, secs, 2_000_000).await;
    println!(
        "{}",
        summary(
            &format!("ack {rate}/s, alerts engine starting (event burst)"),
            lat
        )
    );
    let events: i64 = f
        .admin()
        .query_row("SELECT count(*) FROM alert_events", [], |r| r.get(0))
        .unwrap();
    println!(
        "BENCH alerts: events={events} webhook_requests={} status={}",
        hook.count(),
        svc.status().await
    );

    // 擾動：每 10 秒 200 台主機下線、上一輪下線的上線
    let stop = Arc::new(AtomicBool::new(false));
    let churn = {
        let reg = f.registry.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut round = 0u64;
            let mut down: Vec<(String, u64)> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                for (a, c) in down.drain(..) {
                    let _ = reg.register(
                        c + 1,
                        hello(&a, "x", "10.0.0.1:1"),
                        Instant::now(),
                        Utc::now(),
                    );
                }
                for k in 0..200u64 {
                    let i = ((round * 200 + k) * 2) % hosts;
                    let conn_id = 1_000_000 + i;
                    if reg.disconnect(&agent(i), conn_id, Instant::now()) {
                        down.push((agent(i), conn_id));
                    }
                }
                round += 1;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        })
    };
    let mut ticks = Vec::new();
    let sampler = {
        let svc = svc.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut v = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if let Some(ms) = svc.status().await["last_tick_duration_ms"].as_f64() {
                    v.push(ms);
                }
            }
            v
        })
    };
    let lat = writes(f.sink.clone(), hosts, rate, secs, 3_000_000).await;
    stop.store(true, Ordering::Relaxed);
    let _ = churn.await;
    ticks.extend(sampler.await.unwrap());
    println!(
        "{}",
        summary(
            &format!("ack {rate}/s, alerts engine steady + churn 200 hosts/10 s"),
            lat
        )
    );
    ticks.sort_by(f64::total_cmp);
    println!(
        "BENCH alerts tick duration (ms): n={} median={:.1} max={:.1}",
        ticks.len(),
        ticks.get(ticks.len() / 2).copied().unwrap_or(0.0),
        ticks.last().copied().unwrap_or(0.0)
    );
    let events: i64 = f
        .admin()
        .query_row("SELECT count(*) FROM alert_events", [], |r| r.get(0))
        .unwrap();
    println!(
        "BENCH alerts: events={events} webhook_requests={}",
        hook.count()
    );
    shutdown.trigger();
    AlertService::join(thread, Duration::from_secs(10)).await;
    drop(svc);
    f.stop().await;
}
