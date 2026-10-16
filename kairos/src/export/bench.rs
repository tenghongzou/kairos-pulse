//! 指標轉出的量測（`#[ignore]`，手動執行；結果記在 `kairos/EXPORT.md` 第 5 節）。
//!
//! ```bash
//! cargo test -p kairos --release -- --ignored --nocapture export_bench
//! # 可調：KP_BENCH_HOSTS（預設 10000）、KP_BENCH_ROUNDS（預設 5）
//! ```
//!
//! 量測 Prometheus 輸出（每台主機 43 個指標：pulse_like 的 3 個掛載點、2 個介面＋0.8 的其他標準指標）與
//! OTLP 編碼（每台主機一筆 report）的時間、輸出大小，以及每台主機在記憶體中保存最新一筆的成本。

use std::sync::Arc;
use std::time::Instant;

use super::prom::{HostRow, Options, render};
use super::{ExportSample, otlp, settings};
use crate::config::AppConfig;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn host_sample(i: u64, ts_ms: i64) -> Arc<ExportSample> {
    use kairos_proto::Metric;
    let agent = format!("{i:08x}-0000-4000-8000-00000000beef");
    let mut rec = crate::store::tests::pulse_like(
        &agent,
        i,
        chrono::DateTime::from_timestamp_millis(ts_ms).unwrap_or_default(),
        (i % 100) as f64,
    );
    rec.hostname = format!("host-{i:05}");
    let mut s = ExportSample::from_record(&rec);
    let n = |v: f64| serde_json::Number::from_f64(v).unwrap();
    // 0.8 的其他標準指標（負載、置換空間、磁碟 I/O、封包、行程數、運作時間）
    for (name, v, unit) in [
        ("system.cpu.load_1m", n(0.5), "1"),
        ("system.cpu.load_5m", n(0.4), "1"),
        ("system.cpu.load_15m", n(0.3), "1"),
        ("system.memory.available", n(1e10), "By"),
        ("system.swap.total", n(4e9), "By"),
        ("system.swap.used", n(1e8), "By"),
        ("system.process.count", n(214.0), "{processes}"),
        ("system.uptime", n(86_400.0), "s"),
    ] {
        s.metrics.push(Metric::new(name, v, unit));
    }
    for dev in ["sda", "nvme0n1"] {
        for (name, unit) in [
            ("system.disk.read", "By"),
            ("system.disk.written", "By"),
            ("system.disk.read_ops", "{operations}"),
            ("system.disk.write_ops", "{operations}"),
            ("system.disk.io_time", "ms"),
        ] {
            s.metrics
                .push(Metric::new(name, 123_456_789u64 + i, unit).with_label("device", dev));
        }
    }
    for dev in ["eth0", "eth1"] {
        for name in [
            "system.network.packets_received",
            "system.network.packets_transmitted",
            "system.network.errors_received",
            "system.network.errors_transmitted",
        ] {
            s.metrics
                .push(Metric::new(name, 1_000u64 + i, "{packets}").with_label("device", dev));
        }
    }
    Arc::new(s)
}

#[test]
#[ignore]
fn export_bench() {
    let hosts = env_u64("KP_BENCH_HOSTS", 10_000);
    let rounds = env_u64("KP_BENCH_ROUNDS", 5).max(1);
    let now = chrono::Utc::now().timestamp_millis();
    let samples: Vec<Arc<ExportSample>> = (0..hosts).map(|i| host_sample(i, now - 2_000)).collect();
    let per_host = samples[0].metrics.len();
    let approx_bytes: usize = samples[0]
        .metrics
        .iter()
        .map(|m| {
            std::mem::size_of_val(m)
                + m.name.len()
                + m.unit.len()
                + m.labels
                    .iter()
                    .map(|(k, v)| k.len() + v.len() + 48)
                    .sum::<usize>()
        })
        .sum();
    println!(
        "主機 {hosts} 台、每台 {per_host} 個指標；每台最新一筆約 {:.1} KB（{hosts} 台約 {:.0} MB）",
        approx_bytes as f64 / 1024.0,
        (approx_bytes as f64 * hosts as f64) / 1024.0 / 1024.0
    );
    let rows: Vec<HostRow> = samples
        .iter()
        .enumerate()
        .map(|(i, s)| HostRow {
            agent_id: s.agent_id.clone(),
            hostname: s.hostname.clone(),
            display_name: None,
            group: Some(Arc::from(format!("line{}", i % 20))),
            up: i % 10 != 0,
            agent_version: Some(Arc::from("0.8.0")),
            os_family: Some("linux".into()),
            tags: Arc::from(vec!["env:prod".to_string()]),
            sample: Some(s.clone()),
        })
        .collect();
    for om in [false, true] {
        let opts = Options {
            now_ms: now,
            stale_after_ms: 300_000,
            include_processes: false,
            processes_top_n: 5,
            openmetrics: om,
        };
        let mut best = f64::MAX;
        let mut out = None;
        for _ in 0..rounds {
            let t = Instant::now();
            let r = render(&rows, &opts);
            best = best.min(t.elapsed().as_secs_f64());
            out = Some(r);
        }
        let r = out.unwrap();
        println!(
            "Prometheus（{}）：{:.1} ms（最佳 {rounds} 次）、{} 個序列、{:.1} MB",
            if om { "OpenMetrics" } else { "text 0.0.4" },
            best * 1000.0,
            r.series,
            r.body.len() as f64 / 1024.0 / 1024.0
        );
        if !om {
            let t = Instant::now();
            kp_obs::prom::parse(&r.body).unwrap();
            println!(
                "  （驗證：嚴格解析 {:.0} ms）",
                t.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    // OTLP：一批 8192 個資料點（約 160 台主機各一筆）與全部主機各一筆
    let mut c = AppConfig::default();
    c.export.otlp.enabled = true;
    c.export.otlp.endpoint = "http://127.0.0.1:4318/v1/metrics".into();
    let eff = settings::effective(&c, &[], None);
    let per_batch = (8_192 / per_host).max(1);
    let meta = Default::default();
    let t = Instant::now();
    let mut total = 0usize;
    let mut bytes = 0usize;
    for chunk in samples.chunks(per_batch) {
        let (body, points) = otlp::encode(chunk, &meta, &eff);
        total += points;
        bytes += body.len();
    }
    let secs = t.elapsed().as_secs_f64();
    println!(
        "OTLP 編碼（semconv）：{total} 個資料點、{} 批、{:.1} MB，{:.1} ms（{:.0} 萬點／秒）",
        samples.len().div_ceil(per_batch),
        bytes as f64 / 1024.0 / 1024.0,
        secs * 1000.0,
        total as f64 / secs / 10_000.0
    );
}
