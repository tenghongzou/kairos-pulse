//! 註冊通道的量測（`#[ignore]`，手動執行；結果見 `kairos/PKI.md` 第 12 節）。
//!
//! ```bash
//! cargo test -p kairos --release -- --ignored --nocapture enroll_bench_10k
//! cargo test -p kairos --release -- --ignored --nocapture enroll_bench_default_limits
//! # 可調：KP_BENCH_AGENTS（主機數，預設 10000）、KP_BENCH_CONCURRENCY（同時的請求數，預設 256）、
//! #       KP_BENCH_RATE（report 筆／秒，預設 2000）、KP_BENCH_SECS（default_limits 的秒數，預設 30）
//! ```
//!
//! 以實際的 8444（TLS＋hyper）與 report 的 writer（同一個 SQLite 資料庫）量測：每個請求一條新的 TLS
//! 連線（含握手；最壞情況）。用戶端在另一個 runtime 上執行（與伺服器共用 CPU）。

use chrono::Utc;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use super::StartParams;
use super::ops::{self, TokenInput};
use super::tests::{Agent, agent, root};
use crate::config::{AppConfig, PkiMode};
use crate::pki::x509::pem_decode_all;
use crate::shutdown::Shutdown;
use crate::store::tests::pulse_like;
use kairos_proto::enroll::{EnrollMode, TokenApproval};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn summary(name: &str, mut lat: Vec<Duration>) -> String {
    if lat.is_empty() {
        return format!("{name}: no samples");
    }
    lat.sort();
    let p = |q: f64| lat[((lat.len() as f64 - 1.0) * q) as usize];
    format!(
        "{name}: n={} p50={:.2} ms p99={:.2} ms max={:.2} ms",
        lat.len(),
        p(0.5).as_secs_f64() * 1000.0,
        p(0.99).as_secs_f64() * 1000.0,
        lat.last().unwrap().as_secs_f64() * 1000.0
    )
}

struct Bench {
    dir: PathBuf,
    port: u16,
    roots: Arc<RootCertStore>,
    sink: crate::storage::ReportSink,
    running: Option<super::Running>,
    writer: Option<tokio::task::JoinHandle<()>>,
    shutdown: Arc<Shutdown>,
    config: AppConfig,
}

async fn setup(tag: &str, tweak: impl FnOnce(&mut AppConfig)) -> Bench {
    let dir =
        std::env::temp_dir().join(format!("kairos-enroll-bench-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let r = root();
    let (cert, key) = r.server_cert();
    std::fs::write(dir.join("server.pem"), cert).unwrap();
    std::fs::write(dir.join("server.key"), key).unwrap();
    std::fs::write(dir.join("root.pem"), &r.pem).unwrap();
    let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
    let mut c = AppConfig::default();
    c.server.listen = "127.0.0.1:0".into();
    c.server.cert_path = p("server.pem");
    c.server.key_path = p("server.key");
    c.auth.client_ca = p("root.pem");
    c.storage.data_dir = p("data");
    c.limits.max_agents = Some(100_000);
    c.storage.max_hosts = Some(200_000);
    c.console.enabled = false;
    c.enroll.mode = Some(EnrollMode::Open);
    c.enroll.listen = "127.0.0.1:0".into();
    c.pki.mode = Some(PkiMode::Online);
    c.pki.server_ca_path = p("root.pem");
    tweak(&mut c);
    let data = PathBuf::from(&c.storage.data_dir);
    std::fs::create_dir_all(&data).unwrap();
    // report 的 writer（與 Kairos 相同：同一個資料庫）
    let storage = crate::store::start(&c, &data, Utc::now).await.unwrap();
    let writer = tokio::spawn(storage.writer);
    let opts = crate::store::sqlite::SqliteOptions::from_config(&c, &data);
    let conn =
        crate::store::sqlite::open_existing_read_write(&opts.path, Duration::from_secs(5)).unwrap();
    let now = Utc::now().timestamp_millis();
    let (_, n) = crate::pki::manage::create(&conn, &data, "Bench", now).unwrap();
    crate::pki::manage::install(&conn, &data, &r.sign_issuing(&n.csr_pem), now).unwrap();
    drop(conn);
    let shutdown = Arc::new(Shutdown::new());
    let running = super::start(StartParams {
        config: Arc::new(c.clone()),
        data_dir: data.clone(),
        db_path: super::db_path(&c, &data),
        shutdown: shutdown.clone(),
        other_listens: Vec::new(),
        clock: super::system_clock(),
    })
    .await
    .unwrap();
    let svc = running.service.clone();
    let mut port = 0;
    for _ in 0..200 {
        if let Some(a) = super::server::Listener::local_addr(&svc) {
            port = a.rsplit_once(':').unwrap().1.parse().unwrap();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut roots = RootCertStore::empty();
    for (_, der) in pem_decode_all(&r.pem, Some("CERTIFICATE")).unwrap() {
        roots.add(CertificateDer::from(der)).unwrap();
    }
    Bench {
        dir,
        port,
        roots: Arc::new(roots),
        sink: storage.reports,
        running: Some(running),
        writer: Some(writer),
        shutdown,
        config: c,
    }
}

impl Bench {
    fn token(&self, approval: TokenApproval) -> String {
        let data = PathBuf::from(&self.config.storage.data_dir);
        let opts = crate::store::sqlite::SqliteOptions::from_config(&self.config, &data);
        let mut conn =
            crate::store::sqlite::open_existing_read_write(&opts.path, Duration::from_secs(5))
                .unwrap();
        let input = TokenInput {
            name: "bench".into(),
            group: "g".into(),
            max_uses: 100_000,
            approval: Some(approval),
            allowed_cidrs: Some(vec!["127.0.0.0/8".into()]),
            ..TokenInput::default()
        };
        ops::create_token(
            &mut conn,
            &input,
            "bench",
            None,
            Utc::now().timestamp_millis(),
        )
        .unwrap()
        .token
        .canonical()
    }

    async fn stop(mut self) {
        self.shutdown.trigger();
        if let Some(r) = self.running.take() {
            r.join().await;
        }
        drop(self.sink);
        if let Some(w) = self.writer.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), w).await;
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 一個 HTTPS 請求（新連線），回傳狀態碼與本文
async fn call(
    port: u16,
    roots: Arc<RootCertStore>,
    method: &str,
    path: &str,
    body: Option<String>,
    auth: Option<String>,
) -> (u16, Value) {
    let cfg = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let Ok(tcp) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return (0, Value::Null);
    };
    let Ok(mut tls) = TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
    else {
        return (0, Value::Null);
    };
    let body = body.unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: Bearer {a}\r\n"));
    }
    if method == "POST" {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    if tls.write_all(req.as_bytes()).await.is_err() {
        return (0, Value::Null);
    }
    let mut buf = Vec::new();
    let _ = tls.read_to_end(&mut buf).await;
    let text = String::from_utf8_lossy(&buf);
    let Some((head, rest)) = text.split_once("\r\n\r\n") else {
        return (0, Value::Null);
    };
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, serde_json::from_str(rest).unwrap_or(Value::Null))
}

/// report 的寫入負載：每 10 ms 送出 rate/100 筆，量測每筆的 ack 延遲，直到 `stop`
async fn report_load(
    sink: crate::storage::ReportSink,
    rate: u64,
    stop: Arc<std::sync::atomic::AtomicBool>,
    seq_base: u64,
) -> Vec<Duration> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let per_tick = (rate / 100).max(1);
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let mut sent = 0u64;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        tick.tick().await;
        for _ in 0..per_tick {
            let a = format!("{:08x}-0000-4000-8000-00000000beef", sent % 10_000);
            let rec = pulse_like(&a, seq_base + sent, Utc::now(), (sent % 100) as f64);
            let sink = sink.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let t0 = Instant::now();
                if let Ok(mut p) = sink.submit(rec).await {
                    p.wait().await;
                }
                let _ = tx.send(t0.elapsed());
            });
            sent += 1;
        }
    }
    drop(tx);
    let mut lat = Vec::new();
    while let Some(d) = rx.recv().await {
        lat.push(d);
    }
    lat
}

/// 以 `concurrency` 個同時的請求跑完 `jobs`，回傳 (延遲, 狀態碼的計數, 總時間)
async fn run_jobs(
    jobs: Vec<(String, String, Option<String>, Option<String>)>,
    port: u16,
    roots: Arc<RootCertStore>,
    concurrency: usize,
) -> (Vec<Duration>, BTreeMap<u16, u64>, Duration, Vec<Value>) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let t0 = Instant::now();
    let mut handles = Vec::with_capacity(jobs.len());
    for (method, path, body, auth) in jobs {
        let sem = sem.clone();
        let roots = roots.clone();
        handles.push(rt.spawn(async move {
            let _p = sem.acquire_owned().await.unwrap();
            let s = Instant::now();
            let (status, v) = call(port, roots, &method, &path, body, auth).await;
            (s.elapsed(), status, v)
        }));
    }
    let mut lat = Vec::new();
    let mut counts = BTreeMap::new();
    let mut bodies = Vec::new();
    for h in handles {
        let (d, s, v) = h.await.unwrap();
        lat.push(d);
        *counts.entry(s).or_insert(0) += 1;
        bodies.push(v);
    }
    let total = t0.elapsed();
    rt.shutdown_background();
    (lat, counts, total, bodies)
}

fn enroll_jobs(
    agents: &[Agent],
    token: &str,
) -> Vec<(String, String, Option<String>, Option<String>)> {
    agents
        .iter()
        .map(|a| {
            let body =
                serde_json::to_string(&a.enroll_request(token, Utc::now().timestamp_millis()))
                    .unwrap();
            (
                "POST".to_string(),
                "/v1/enroll/requests".to_string(),
                Some(body),
                None,
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "量測，手動執行"]
async fn enroll_bench_10k() {
    let n = env_u64("KP_BENCH_AGENTS", 10_000) as usize;
    let conc = env_u64("KP_BENCH_CONCURRENCY", 256) as usize;
    let rate = env_u64("KP_BENCH_RATE", 2000);
    // 容量量測：同一個來源（127.0.0.1）設為批次部署來源，L8 放寬，量測伺服器本身的處理能力
    let b = setup("10k", |c| {
        c.enroll.batch_sources = vec!["127.0.0.0/8".into()];
        c.enroll.batch_source_multiplier = 100;
        c.enroll.max_requests_per_sec = 10_000;
        c.enroll.max_requests_burst = 100_000;
        c.enroll.max_new_connections_per_sec = 10_000;
        c.enroll.max_new_connections_burst = 10_000;
        c.enroll.max_requests_per_agent_burst = 60;
        c.enroll.poll_min_interval_secs = 1;
        c.enroll.token_min_drain_secs = 0;
    })
    .await;
    println!("prepare {n} agents (P-256 keys and CSRs)...");
    let manual: Vec<Agent> = (0..n)
        .map(|i| agent(&format!("{i:08x}-1111-4000-8000-000000000001")))
        .collect();
    let auto: Vec<Agent> = (0..n)
        .map(|i| agent(&format!("{i:08x}-2222-4000-8000-000000000002")))
        .collect();
    let token_m = b.token(TokenApproval::Manual);
    let token_a = b.token(TokenApproval::Auto);

    // 階段 0：只有 report 寫入
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let load = tokio::spawn(report_load(b.sink.clone(), rate, stop.clone(), 1_000_000));
    tokio::time::sleep(Duration::from_secs(15)).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    println!(
        "{}",
        summary(
            &format!("ack {rate}/s, no enroll load"),
            load.await.unwrap()
        )
    );

    // 階段 1：1 萬台同時新註冊（需核准）＋report 寫入
    for (name, agents, token, extra) in [
        ("new enrollment (manual, pending)", &manual, &token_m, false),
        (
            "new enrollment (auto, issued online)",
            &auto,
            &token_a,
            true,
        ),
    ] {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let load = tokio::spawn(report_load(
            b.sink.clone(),
            rate,
            stop.clone(),
            if extra { 3_000_000 } else { 2_000_000 },
        ));
        let (lat, counts, total, bodies) =
            run_jobs(enroll_jobs(agents, token), b.port, b.roots.clone(), conc).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let ack = load.await.unwrap();
        println!(
            "{} — {:.0} req/s, statuses {:?}, total {:.1} s",
            summary(&format!("{name} x{n}, concurrency {conc}"), lat),
            n as f64 / total.as_secs_f64(),
            counts,
            total.as_secs_f64()
        );
        println!("{}", summary(&format!("ack {rate}/s during {name}"), ack));
        if !extra {
            // 階段 2：每台輪詢一次（含 poll_after_ms）＋report 寫入
            let jobs: Vec<_> = manual
                .iter()
                .zip(&bodies)
                .filter_map(|(a, v)| {
                    v["request_id"].as_str().map(|id| {
                        (
                            "GET".to_string(),
                            format!("/v1/enroll/requests/{id}"),
                            None,
                            Some(a.poll_secret.clone()),
                        )
                    })
                })
                .collect();
            let poll_after = bodies
                .iter()
                .filter_map(|v| v["poll_after_ms"].as_u64())
                .max();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let load = tokio::spawn(report_load(b.sink.clone(), rate, stop.clone(), 4_000_000));
            let (lat, counts, total, _) = run_jobs(jobs, b.port, b.roots.clone(), conc).await;
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            println!(
                "{} — {:.0} req/s, statuses {:?}, max poll_after_ms {:?}",
                summary(&format!("poll x{n}"), lat),
                n as f64 / total.as_secs_f64(),
                counts,
                poll_after
            );
            println!(
                "{}",
                summary(&format!("ack {rate}/s during poll"), load.await.unwrap())
            );
        }
    }
    let size = std::fs::metadata(PathBuf::from(&b.config.storage.data_dir).join("kairos.db"))
        .map(|m| m.len())
        .unwrap_or(0);
    let (pending, certs) = b
        .running
        .as_ref()
        .unwrap()
        .service
        .db
        .read(|c| {
            Ok((
                c.pending_count()?,
                c.test_sqlite()
                    .query_row("SELECT count(*) FROM pki_certificates", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
            ))
        })
        .await
        .unwrap();
    println!(
        "database {:.1} MB, pending {pending}, certificates {certs}",
        size as f64 / 1e6
    );
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "量測，手動執行"]
async fn enroll_bench_default_limits() {
    let n = env_u64("KP_BENCH_AGENTS", 10_000) as usize;
    let secs = env_u64("KP_BENCH_SECS", 30);
    let conc = env_u64("KP_BENCH_CONCURRENCY", 256) as usize;
    // 預設的限流：1 萬台從同一個 NAT（127.0.0.1）在 secs 秒內不斷送出新註冊。
    // KP_BENCH_BATCH=1：來源設為批次部署來源（L2～L4 放寬 10 倍），全域的 L8（每秒 50 筆）成為上限
    let batch = env_u64("KP_BENCH_BATCH", 0) == 1;
    let rate = env_u64("KP_BENCH_RATE", 2000);
    let b = setup("defaults", |c| {
        if batch {
            c.enroll.batch_sources = vec!["127.0.0.0/8".into()];
        }
    })
    .await;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let load = tokio::spawn(report_load(b.sink.clone(), rate, stop.clone(), 5_000_000));
    let agents: Vec<Agent> = (0..n)
        .map(|i| agent(&format!("{i:08x}-3333-4000-8000-000000000003")))
        .collect();
    let token = b.token(TokenApproval::Manual);
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut retry: Vec<u64> = Vec::new();
    let mut round = 0;
    while Instant::now() < deadline {
        let jobs = enroll_jobs(
            &agents[(round * 1000) % n..((round * 1000) % n + 1000).min(n)],
            &token,
        );
        let (_, _, _, bodies) = run_jobs(jobs, b.port, b.roots.clone(), conc).await;
        for v in bodies {
            let key = v["error"]["code"]
                .as_str()
                .map(str::to_string)
                .or_else(|| v["state"].as_str().map(str::to_string))
                .unwrap_or_else(|| "closed_before_handshake".into());
            if let Some(ms) = v["error"]["retry_after_ms"].as_u64() {
                retry.push(ms / 1000);
            }
            *counts.entry(key).or_insert(0) += 1;
        }
        round += 1;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    println!(
        "{}",
        summary(
            &format!("ack {rate}/s during default-limit load (batch source {batch})"),
            load.await.unwrap()
        )
    );
    retry.sort();
    let p = |q: f64| {
        retry
            .get(((retry.len() as f64 - 1.0).max(0.0) * q) as usize)
            .copied()
            .unwrap_or(0)
    };
    println!(
        "default limits (batch source {batch}), {secs} s from one source: {counts:?}; Retry-After p50 {} s p99 {} s max {} s",
        p(0.5),
        p(0.99),
        retry.last().copied().unwrap_or(0)
    );
    b.stop().await;
}
