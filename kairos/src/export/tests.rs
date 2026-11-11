//! 指標轉出的整合測試：假的 OTLP collector（HTTP 與 HTTPS，解出 protobuf 驗證內容）、重試與丟棄、
//! 佇列上限，以及主控台的端點（Prometheus 輸出、設定的來源與鎖定、標頭加密、稽核、測試連線）。

use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use super::proto::{self, Msg, decode, field, fields};
use super::settings::{self as xs, Effective};
use super::{ExportHub, ExportSample, otlp};
use crate::api::tests::{call, fixture, hello, request};
use crate::api::tokens::{self, TokenGrant};
use crate::config::AppConfig;
use crate::rbac::{Assignment, Role};
use crate::shutdown::Shutdown;
use crate::store::prelude::*;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");
const A: &str = "5e0c9a10-0000-4000-8000-0000000000e1";
const B: &str = "5e0c9a10-0000-4000-8000-0000000000e2";

// ---- 假的 collector ----

#[derive(Debug, Clone)]
struct Captured {
    head: String,
    body: Vec<u8>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    }
}

/// 回應：(狀態碼, Retry-After, 本文, 本文是 protobuf)
type Reply = (u16, Option<u64>, Vec<u8>, bool);

struct FakeCollector {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Captured>>>,
    script: Arc<Mutex<VecDeque<Reply>>>,
}

fn acceptor() -> TlsAcceptor {
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(format!("{MANIFEST}/../pulse/testdata/server.pem"))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    let key =
        PrivateKeyDer::from_pem_file(format!("{MANIFEST}/../pulse/testdata/server.key")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    mut sock: S,
    requests: Arc<Mutex<Vec<Captured>>>,
    script: Arc<Mutex<VecDeque<Reply>>>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let len: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    requests.lock().unwrap().push(Captured { head, body });
    let (code, retry, rbody, proto_body) =
        script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((200, None, Vec::new(), true));
    let mut resp = format!(
        "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: {}\r\n",
        rbody.len(),
        if proto_body {
            "application/x-protobuf"
        } else {
            "text/plain"
        }
    );
    if let Some(r) = retry {
        resp.push_str(&format!("Retry-After: {r}\r\n"));
    }
    resp.push_str("\r\n");
    let mut out = resp.into_bytes();
    out.extend_from_slice(&rbody);
    let _ = sock.write_all(&out).await;
    let _ = sock.shutdown().await;
}

impl FakeCollector {
    async fn start(tls: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<Captured>>> = Arc::default();
        let script: Arc<Mutex<VecDeque<Reply>>> = Arc::default();
        let (r, sc) = (requests.clone(), script.clone());
        let acc = tls.then(acceptor);
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let (r, sc, acc) = (r.clone(), sc.clone(), acc.clone());
                tokio::spawn(async move {
                    match acc {
                        Some(a) => {
                            if let Ok(s) = a.accept(sock).await {
                                serve(s, r, sc).await;
                            }
                        }
                        None => serve(sock, r, sc).await,
                    }
                });
            }
        });
        Self {
            addr,
            requests,
            script,
        }
    }

    fn url(&self, https: bool) -> String {
        if https {
            format!("https://localhost:{}/v1/metrics", self.addr.port())
        } else {
            format!("http://127.0.0.1:{}/v1/metrics", self.addr.port())
        }
    }

    fn push(&self, r: Reply) {
        self.script.lock().unwrap().push_back(r);
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn all(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }
}

async fn wait_for<F: Fn() -> bool>(what: &str, limit: Duration, cond: F) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < limit, "等待逾時：{what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---- 解出 ExportMetricsServiceRequest ----

#[derive(Debug, Clone, PartialEq)]
enum PV {
    Int(i64),
    Double(f64),
}

#[derive(Debug, Clone)]
struct Point {
    attrs: BTreeMap<String, String>,
    start: u64,
    time: u64,
    value: PV,
}

#[derive(Debug, Clone)]
struct DMetric {
    name: String,
    unit: String,
    sum: bool,
    temporality: u64,
    monotonic: bool,
    points: Vec<Point>,
}

#[derive(Debug, Clone)]
struct DResource {
    attrs: BTreeMap<String, String>,
    scope: (String, String),
    metrics: Vec<DMetric>,
    schema_url: String,
}

impl DResource {
    fn metric(&self, name: &str) -> &DMetric {
        self.metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "沒有 {name}：{:?}",
                    self.metrics.iter().map(|m| &m.name).collect::<Vec<_>>()
                )
            })
    }
}

fn kvs(list: Vec<proto::Value<'_>>) -> BTreeMap<String, String> {
    list.into_iter()
        .map(|kv| {
            let f = decode(kv.bytes()).unwrap();
            let k = field(&f, 1).unwrap().str().to_string();
            let any = decode(field(&f, 2).unwrap().bytes()).unwrap();
            (
                k,
                field(&any, 1)
                    .map(|v| v.str().to_string())
                    .unwrap_or_default(),
            )
        })
        .collect()
}

fn decode_request(body: &[u8]) -> Vec<DResource> {
    let top = decode(body).unwrap();
    fields(&top, 1)
        .into_iter()
        .map(|rm| {
            let rm = decode(rm.bytes()).unwrap();
            let res = decode(field(&rm, 1).unwrap().bytes()).unwrap();
            let sm = decode(field(&rm, 2).unwrap().bytes()).unwrap();
            let scope = decode(field(&sm, 1).unwrap().bytes()).unwrap();
            let metrics = fields(&sm, 2)
                .into_iter()
                .map(|m| {
                    let m = decode(m.bytes()).unwrap();
                    let (sum, data) = match field(&m, 7) {
                        Some(d) => (true, decode(d.bytes()).unwrap()),
                        None => (false, decode(field(&m, 5).unwrap().bytes()).unwrap()),
                    };
                    let points = fields(&data, 1)
                        .into_iter()
                        .map(|dp| {
                            let dp = decode(dp.bytes()).unwrap();
                            Point {
                                attrs: kvs(fields(&dp, 7)),
                                start: field(&dp, 2).map_or(0, |v| v.u64()),
                                time: field(&dp, 3).map_or(0, |v| v.u64()),
                                value: match (field(&dp, 4), field(&dp, 6)) {
                                    (Some(d), _) => PV::Double(d.f64()),
                                    (_, Some(i)) => PV::Int(i.u64() as i64),
                                    _ => panic!("資料點沒有值"),
                                },
                            }
                        })
                        .collect();
                    DMetric {
                        name: field(&m, 1).unwrap().str().to_string(),
                        unit: field(&m, 3)
                            .map(|v| v.str().to_string())
                            .unwrap_or_default(),
                        sum,
                        temporality: field(&data, 2).map_or(0, |v| v.u64()),
                        monotonic: field(&data, 3).is_some_and(|v| v.u64() == 1),
                        points,
                    }
                })
                .collect();
            DResource {
                attrs: kvs(fields(&res, 1)),
                scope: (
                    field(&scope, 1).unwrap().str().to_string(),
                    field(&scope, 2)
                        .map(|v| v.str().to_string())
                        .unwrap_or_default(),
                ),
                metrics,
                schema_url: field(&rm, 3)
                    .map(|v| v.str().to_string())
                    .unwrap_or_default(),
            }
        })
        .collect()
}

// ---- 測試資料 ----

fn sample(agent: &str, host: &str, ts_ms: i64, cpu: f64, rx: u64) -> Arc<ExportSample> {
    use kairos_proto::Metric;
    let n = |v: f64| serde_json::Number::from_f64(v).unwrap();
    Arc::new(ExportSample {
        agent_id: Arc::from(agent),
        hostname: Arc::from(host),
        ts_ms,
        metrics: vec![
            Metric::new("system.cpu.usage", n(cpu), "%"),
            Metric::new("system.memory.used", 4_294_967_296u64, "By"),
            Metric::new("system.memory.total", 8_589_934_592u64, "By"),
            Metric::new("system.filesystem.used", 100u64, "By")
                .with_label("mountpoint", "/")
                .with_label("fstype", "ext4"),
            Metric::new("system.filesystem.available", 300u64, "By")
                .with_label("mountpoint", "/")
                .with_label("fstype", "ext4"),
            Metric::new("system.network.received", rx, "By").with_label("device", "eth0"),
            Metric::new("system.disk.io_time", 2_500u64, "ms").with_label("device", "sda"),
            Metric::new("system.uptime", 3_600u64, "s"),
            Metric::new("system.temperature", 55u64, "Cel").with_label("sensor", "cpu"),
            Metric::new("app.jobs", 3u64, "{jobs}").with_label("queue", "default"),
        ],
        processes: None,
    })
}

fn otlp_config(endpoint: &str, tweak: impl FnOnce(&mut AppConfig)) -> AppConfig {
    let mut c = AppConfig::default();
    c.export.otlp.enabled = true;
    c.export.otlp.endpoint = endpoint.to_string();
    c.export.otlp.interval_secs = 1;
    c.export.otlp.timeout_secs = 5;
    tweak(&mut c);
    c
}

fn eff_of(c: &AppConfig) -> Effective {
    xs::effective(c, &[], None)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn otlp_pushes_semconv_metrics_with_resource_and_headers() {
    let col = FakeCollector::start(false).await;
    let dir = crate::api::tests::temp_dir("otlp-headers");
    let hfile = dir.join("otlp-headers");
    std::fs::write(
        &hfile,
        "Authorization: Bearer test-token-123\nX-Scope-OrgID: tenant-a\n",
    )
    .unwrap();
    let c = otlp_config(&col.url(false), |c| {
        c.export.otlp.headers_file = hfile.to_string_lossy().into_owned();
        c.export.otlp.resource_attributes = vec!["deployment.environment=test".into()];
    });
    let hub = ExportHub::new(eff_of(&c), None, None);
    let shutdown = Arc::new(Shutdown::new());
    assert!(hub.wants_samples());
    // 三筆都進佇列後才啟動送出的 task：送出每 1 秒一次，先啟動的話慢的機器上（CI 的 Windows runner）
    // 這三行之間可能剛好跨過一次送出，第一個請求就只有一部分的主機
    let t0 = 1_760_000_000_000i64;
    hub.accept(sample(A, "web-01", t0, 12.5, 1_000));
    hub.accept(sample(A, "web-01", t0 + 5_000, 50.0, 6_000));
    hub.accept(sample(B, "db-02", t0, 1.0, 10));
    let task = tokio::spawn(otlp::run(hub.clone(), shutdown.clone()));
    wait_for("collector 收到請求", Duration::from_secs(10), || {
        col.count() >= 1
    })
    .await;
    let req = col.all().remove(0);
    assert!(
        req.head.starts_with("POST /v1/metrics HTTP/1.1"),
        "{}",
        req.head
    );
    assert_eq!(
        req.header("content-type").as_deref(),
        Some("application/x-protobuf")
    );
    assert_eq!(
        req.header("authorization").as_deref(),
        Some("Bearer test-token-123")
    );
    assert_eq!(req.header("x-scope-orgid").as_deref(), Some("tenant-a"));
    assert!(req.header("user-agent").unwrap().starts_with("Kairos/"));
    let res = decode_request(&req.body);
    assert_eq!(res.len(), 2, "每台主機一個 ResourceMetrics");
    let a = &res[0];
    assert_eq!(a.attrs["host.name"], "web-01");
    assert_eq!(a.attrs["host.id"], A);
    assert_eq!(a.attrs["deployment.environment"], "test");
    assert_eq!(a.scope.0, "kairos");
    assert_eq!(a.scope.1, env!("CARGO_PKG_VERSION"));
    assert_eq!(a.schema_url, otlp::SCHEMA_URL);
    // CPU：semconv 的 utilization（0～1），兩筆 report 合成同一個指標的兩個資料點
    let cpu = a.metric("system.cpu.utilization");
    assert!(!cpu.sum);
    assert_eq!(cpu.unit, "1");
    assert_eq!(cpu.points.len(), 2);
    assert_eq!(cpu.points[0].value, PV::Double(0.125));
    assert_eq!(cpu.points[0].time, t0 as u64 * 1_000_000);
    assert_eq!(cpu.points[1].time, (t0 as u64 + 5_000) * 1_000_000);
    // 記憶體：usage{state=used} 與 limit
    let mu = a.metric("system.memory.usage");
    assert_eq!(mu.points[0].attrs["system.memory.state"], "used");
    assert_eq!(mu.points[0].value, PV::Int(4_294_967_296));
    a.metric("system.memory.limit");
    // 檔案系統：同一個指標、state 區分
    let fs = a.metric("system.filesystem.usage");
    assert_eq!(fs.points.len(), 4);
    let states: Vec<&str> = fs
        .points
        .iter()
        .map(|p| p.attrs["system.filesystem.state"].as_str())
        .collect();
    assert_eq!(states, ["used", "free", "used", "free"]);
    assert_eq!(fs.points[0].attrs["system.filesystem.mountpoint"], "/");
    assert_eq!(fs.points[0].attrs["system.filesystem.type"], "ext4");
    // 網路：cumulative、單調的 Sum；起點為開機時間（ts − uptime）
    let net = a.metric("system.network.io");
    assert!(net.sum && net.monotonic);
    assert_eq!(net.temporality, 2);
    assert_eq!(net.points[0].attrs["network.interface.name"], "eth0");
    assert_eq!(net.points[0].attrs["network.io.direction"], "receive");
    assert_eq!(net.points[0].value, PV::Int(1_000));
    assert_eq!(net.points[0].start, (t0 as u64 - 3_600_000) * 1_000_000);
    // 磁碟忙碌時間：ms → s（double）
    let io = a.metric("system.disk.io_time");
    assert_eq!(io.unit, "s");
    assert_eq!(io.points[0].value, PV::Double(2.5));
    assert_eq!(io.points[0].attrs["system.device"], "sda");
    // 沒有對應的保留 Kairos 名稱；自訂指標原樣
    assert_eq!(a.metric("system.temperature").unit, "Cel");
    let jobs = a.metric("app.jobs");
    assert_eq!(jobs.points[0].attrs["queue"], "default");
    assert_eq!(jobs.unit, "{jobs}");
    wait_for("統計", Duration::from_secs(5), || {
        hub.stats.otlp_requests_ok.load(Ordering::Relaxed) >= 1
    })
    .await;
    assert_eq!(hub.stats.otlp_sent_points.load(Ordering::Relaxed), 30);
    assert_eq!(hub.queue_points(), 0);
    assert!(super::lock(&hub.otlp_status).last_success_ms.is_some());

    // naming = kairos：原樣使用 Kairos 的名稱
    let mut e = eff_of(&c);
    e.otlp.naming = crate::config::export::NAMING_KAIROS.into();
    let (body, points) = otlp::encode(&[sample(A, "web-01", t0, 12.5, 1)], &Default::default(), &e);
    assert_eq!(points, 10);
    let r = decode_request(&body);
    assert_eq!(
        r[0].metric("system.cpu.usage").points[0].value,
        PV::Double(12.5)
    );
    assert!(r[0].metric("system.network.received").sum);
    assert_eq!(r[0].schema_url, "");

    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn otlp_retries_honor_retry_after_and_rejections_are_dropped() {
    let col = FakeCollector::start(false).await;
    let c = otlp_config(&col.url(false), |c| c.export.otlp.max_retries = 2);
    let hub = ExportHub::new(eff_of(&c), None, None);
    let shutdown = Arc::new(Shutdown::new());
    let task = tokio::spawn(otlp::run(hub.clone(), shutdown.clone()));
    // 503＋Retry-After 1 → 重試成功
    col.push((503, Some(1), Vec::new(), false));
    let t = Instant::now();
    hub.accept(sample(A, "web-01", 1_760_000_000_000, 1.0, 1));
    wait_for("重試後成功", Duration::from_secs(10), || {
        hub.stats.otlp_requests_ok.load(Ordering::Relaxed) == 1
    })
    .await;
    assert!(
        t.elapsed() >= Duration::from_millis(900),
        "依 Retry-After 等待"
    );
    assert_eq!(col.count(), 2);
    assert_eq!(hub.stats.otlp_requests_failed.load(Ordering::Relaxed), 1);
    assert_eq!(hub.stats.otlp_dropped_failed.load(Ordering::Relaxed), 0);
    // 400：不重試、丟棄（KRS-E4312）；錯誤本文是 google.rpc.Status
    let mut status = Msg::new();
    status.uint(1, 3).string(2, "invalid metric name");
    col.push((400, None, status.into_bytes(), true));
    hub.accept(sample(A, "web-01", 1_760_000_005_000, 1.0, 2));
    wait_for("被拒的一批", Duration::from_secs(10), || {
        hub.stats.otlp_dropped_rejected.load(Ordering::Relaxed) == 10
    })
    .await;
    assert_eq!(col.count(), 3);
    let st = super::lock(&hub.otlp_status).clone();
    let (code, msg, _) = st.last_error.unwrap();
    assert_eq!(code, super::CODE_OTLP_REJECTED);
    assert!(
        msg.contains("HTTP 400") && msg.contains("invalid metric name"),
        "{msg}"
    );
    // 2xx＋partial_success：被拒的資料點計入 rejected
    let mut ps = Msg::new();
    ps.int(1, 3).string(2, "3 points out of order");
    let mut resp = Msg::new();
    resp.message(1, &ps);
    col.push((200, None, resp.into_bytes(), true));
    hub.accept(sample(A, "web-01", 1_760_000_010_000, 1.0, 3));
    wait_for("partial_success", Duration::from_secs(10), || {
        hub.stats.otlp_dropped_rejected.load(Ordering::Relaxed) == 13
    })
    .await;
    assert_eq!(hub.stats.otlp_sent_points.load(Ordering::Relaxed), 10 + 7);
    // 一直失敗：重試 2 次後丟棄（KRS-E4311）
    for _ in 0..3 {
        col.push((502, None, Vec::new(), false));
    }
    hub.accept(sample(A, "web-01", 1_760_000_015_000, 1.0, 4));
    wait_for("重試用完", Duration::from_secs(15), || {
        hub.stats.otlp_dropped_failed.load(Ordering::Relaxed) == 10
    })
    .await;
    assert_eq!(
        super::lock(&hub.otlp_status).last_error.as_ref().unwrap().0,
        super::CODE_OTLP_SEND
    );
    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn otlp_over_tls_verifies_the_collector_certificate() {
    let col = FakeCollector::start(true).await;
    let c = otlp_config(&col.url(true), |c| {
        c.export.otlp.ca_file = format!("{MANIFEST}/../pulse/testdata/server-ca.pem");
    });
    let out = otlp::test_connection(&eff_of(&c)).await;
    assert!(out.ok, "{out:?}");
    assert_eq!(out.status, Some(200));
    assert_eq!(col.all()[0].body.len(), 0, "測試連線送出空的請求");
    // 沒有 ca_file：憑證不受信任，不送出
    let c = otlp_config(&col.url(true), |_| {});
    let out = otlp::test_connection(&eff_of(&c)).await;
    assert!(!out.ok);
    assert_eq!(out.code, Some(super::CODE_OTLP_SEND));
    assert!(out.message.contains("不受信任"), "{}", out.message);
    assert_eq!(col.count(), 1);
    // 無法連線
    let c = otlp_config("http://127.0.0.1:9/v1/metrics", |c| {
        c.export.otlp.timeout_secs = 2
    });
    let out = otlp::test_connection(&eff_of(&c)).await;
    assert!(!out.ok && out.retryable);
}

#[test]
fn queue_limit_drops_oldest_or_newest() {
    let mk = |policy: &str| {
        let c = otlp_config("http://127.0.0.1:9/v1/metrics", |c| {
            c.export.otlp.queue_max_points = 1_000;
            c.export.otlp.max_batch_points = 100;
            c.export.otlp.drop_policy = policy.into();
        });
        ExportHub::new(eff_of(&c), None, None)
    };
    for policy in ["oldest", "newest"] {
        let hub = mk(policy);
        for i in 0..150i64 {
            hub.accept(sample(A, "web-01", i, 1.0, 1));
        }
        assert_eq!(hub.queue_points(), 1_000, "{policy}");
        assert_eq!(
            hub.stats.otlp_dropped_queue_full.load(Ordering::Relaxed),
            500,
            "{policy}"
        );
        let batch = hub.take_batch(100);
        assert_eq!(batch.len(), 10);
        let first = batch[0].ts_ms;
        assert_eq!(first, if policy == "oldest" { 50 } else { 0 }, "{policy}");
        assert_eq!(hub.queue_points(), 900);
    }
    // 停用 OTLP：清空佇列，之後不再收
    let hub = mk("oldest");
    hub.accept(sample(A, "web-01", 1, 1.0, 1));
    let mut e = (*hub.effective()).clone();
    e.otlp.enabled = false;
    hub.apply(e);
    assert_eq!(hub.queue_points(), 0);
    assert!(!hub.wants_samples());
    hub.accept(sample(A, "web-01", 2, 1.0, 1));
    assert_eq!(hub.queue_points(), 0);
}

#[test]
fn latest_sample_ignores_older_backfill() {
    let mut c = AppConfig::default();
    c.export.prometheus.enabled = true;
    let hub = ExportHub::new(eff_of(&c), None, None);
    assert!(hub.wants_samples());
    hub.accept(sample(A, "web-01", 2_000, 1.0, 1));
    hub.accept(sample(A, "web-01", 1_000, 2.0, 1));
    assert_eq!(hub.latest()[A].ts_ms, 2_000);
    hub.accept(sample(A, "web-01", 3_000, 3.0, 1));
    assert_eq!(hub.latest()[A].ts_ms, 3_000);
    assert_eq!(hub.queue_points(), 0, "OTLP 未啟用時不進佇列");
}

#[test]
fn forget_drops_the_latest_sample_and_queued_points_of_one_host() {
    // 刪除主機資料（kairos/PRIVACY.md 第 4 節）：最新值與 OTLP 佇列中這台主機的資料都丟掉，其他主機不受影響
    let mut c = otlp_config("http://127.0.0.1:9/v1/metrics", |_| {});
    c.export.prometheus.enabled = true;
    let hub = ExportHub::new(eff_of(&c), None, None);
    hub.accept(sample(A, "web-01", 1_000, 1.0, 1));
    hub.accept(sample(B, "web-02", 1_000, 1.0, 1));
    hub.accept(sample(A, "web-01", 2_000, 1.0, 1));
    let per = sample(A, "web-01", 0, 0.0, 0).points();
    assert_eq!(hub.queue_points(), 3 * per);
    assert_eq!(hub.forget(A), 2 * per);
    assert!(!hub.latest().contains_key(A));
    assert!(hub.latest().contains_key(B));
    assert_eq!(hub.queue_points(), per);
    let rest = hub.take_batch(usize::MAX);
    assert_eq!(rest.len(), 1);
    assert_eq!(&*rest[0].agent_id, B);
    // 不認得的主機：沒有影響
    assert_eq!(hub.forget("nobody"), 0);
}

// ---- 主控台的端點 ----

fn token(f: &crate::api::tests::Fixture, grant: TokenGrant, name: &str) -> String {
    let conn = f.admin();
    tokens::create_with(
        &conn,
        name,
        &grant,
        None,
        Some("tester"),
        None,
        chrono::Utc::now().timestamp_millis(),
    )
    .unwrap()
    .token
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_endpoints_over_https() {
    let col = FakeCollector::start(false).await;
    let f = fixture("export", |c| {
        // naming 由環境變數設定：主控台不能修改（409）
        c.provenance.env_keys.insert(xs::KEY_OTLP_NAMING.into());
    })
    .await;
    let data_dir = std::path::PathBuf::from(&f.config.storage.data_dir);
    let (hub, task) = super::start(&f.config, &data_dir, f.shutdown.clone());
    f.registry.set_export_hub(hub.clone());
    let addr = f.addr();
    let admin = token(&f, TokenGrant::Role(Role::Admin), "admin");
    let operator = token(&f, TokenGrant::Role(Role::Operator), "operator");
    let viewer = token(&f, TokenGrant::Role(Role::Viewer), "viewer");
    assert_eq!(
        crate::rbac::Permission::ExportWrite.least_role(),
        Role::Admin
    );

    // 未啟用：404 export_disabled（KRS-E4315）
    let r = request(addr, "GET", "/api/v1/export/prometheus", Some(&viewer), &[]).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["code"], "export_disabled");
    assert_eq!(
        r.json()["error"]["product_code"],
        super::CODE_EXPORT_DISABLED
    );
    // 設定：唯讀看不到（settings:read）、維運者看得到但不能改
    assert_eq!(
        request(addr, "GET", "/api/v1/export/settings", Some(&viewer), &[])
            .await
            .status,
        403
    );
    let r = request(addr, "GET", "/api/v1/export/settings", Some(&operator), &[]).await;
    assert_eq!(r.status, 200);
    let d = r.json()["data"].clone();
    assert_eq!(d["prometheus"]["enabled"], false);
    assert_eq!(d["otlp"]["status"]["state"], "disabled");
    // SQLite：機密的金鑰檔可以使用（主控台不必等保存標頭被 503 拒絕才知道）
    assert_eq!(d["otlp"]["secrets_available"], true);
    let naming = d["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == xs::KEY_OTLP_NAMING)
        .unwrap()
        .clone();
    assert_eq!(naming["locked"], true);
    assert_eq!(naming["env_var"], "KAIROS__EXPORT__OTLP__NAMING");
    let body = json!({"prometheus": {"enabled": true}});
    let r = call(
        addr,
        "PUT",
        "/api/v1/export/settings",
        Some(&operator),
        Some(&body),
    )
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(r.json()["error"]["details"]["permission"], "export:write");

    // 管理員啟用 exporter：來源變為主控台、寫入稽核
    let r = call(addr, "PUT", "/api/v1/export/settings", Some(&admin), Some(&json!({"prometheus": {"enabled": true, "stale_after_secs": 120}, "reason": "接 Grafana"}))).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let d = r.json()["data"].clone();
    assert_eq!(d["prometheus"]["enabled"], true);
    assert_eq!(d["prometheus"]["stale_after_secs"], 120);
    let src = |d: &Value, k: &str| {
        d["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == k)
            .unwrap()["source"]
            .clone()
    };
    assert_eq!(src(&d, xs::KEY_PROM_ENABLED), "console");
    assert!(hub.effective().prometheus.enabled);
    let conn = f.admin();
    let (action, details): (String, String) = conn
        .query_row("SELECT action, details FROM audit_events WHERE action = 'export.change' ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(action, "export.change");
    assert!(
        details.contains("接 Grafana") && details.contains(xs::KEY_PROM_ENABLED),
        "{details}"
    );

    // 主機：A 線上、有 report（經由與連線相同的保存路徑）；B 只在清冊中（離線）
    let now = chrono::Utc::now();
    f.registry
        .register(
            21,
            hello(A, "hmi-line3-07", "10.30.3.27:51544"),
            Instant::now(),
            now,
        )
        .unwrap();
    let revived = std::sync::atomic::AtomicBool::new(false);
    let outcome = crate::connection::store_report(
        &f.registry,
        &f.sink,
        21,
        crate::store::tests::pulse_like(A, 1, now, 37.5),
        Duration::from_secs(5),
        &revived,
    )
    .await;
    assert!(
        matches!(outcome, crate::connection::StoreOutcome::Stored(_)),
        "{outcome:?}"
    );
    f.sink
        .write(crate::store::tests::pulse_like(B, 1, now, 1.0))
        .await
        .unwrap();
    f.console.ctx.hosts.invalidate().await;
    let expected = match crate::store::tests::pulse_like(A, 1, now, 1.0).body {
        crate::storage::RecordBody::Metrics { metrics } => metrics.len(),
        crate::storage::RecordBody::Legacy { .. } => 3,
    };
    assert_eq!(hub.latest()[A].metrics.len(), expected);

    let r = request(addr, "GET", "/api/v1/export/prometheus", Some(&viewer), &[]).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        r.header("content-type"),
        Some(super::prom::CONTENT_TYPE_TEXT)
    );
    let text = String::from_utf8(r.body.clone()).unwrap();
    let p = kp_obs::prom::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let la = [("agent_id", A), ("hostname", "hmi-line3-07")];
    assert_eq!(p.get("kairos_host_up", &la), Some(1.0));
    assert_eq!(p.get("system_cpu_usage_percent", &la), Some(37.5));
    assert_eq!(
        p.series("kairos_host_up")
            .iter()
            .find(|s| s.labels["agent_id"] == B)
            .map(|s| s.value),
        Some(0.0)
    );
    assert!(
        p.series("system_cpu_usage_percent")
            .iter()
            .all(|s| s.labels["agent_id"] == A),
        "離線主機不輸出指標"
    );
    assert!(!p.series("system_network_received_bytes_total").is_empty());
    // OpenMetrics
    let r = request(
        addr,
        "GET",
        "/api/v1/export/prometheus",
        Some(&viewer),
        &[("Accept", "application/openmetrics-text; version=1.0.0")],
    )
    .await;
    assert_eq!(
        r.header("content-type"),
        Some(super::prom::CONTENT_TYPE_OPENMETRICS)
    );
    assert!(String::from_utf8_lossy(&r.body).ends_with("# EOF\n"));
    // 群組範圍的 token：看不到沒有群組的主機；?group= 篩選
    let g = Assignment::parse("viewer@group:line9").unwrap();
    let gtoken = group_token(&f, g);
    let r = request(addr, "GET", "/api/v1/export/prometheus", Some(&gtoken), &[]).await;
    assert_eq!(r.status, 200);
    assert!(!String::from_utf8_lossy(&r.body).contains(A));
    let r = request(
        addr,
        "GET",
        "/api/v1/export/prometheus?group=line9",
        Some(&viewer),
        &[],
    )
    .await;
    assert!(!String::from_utf8_lossy(&r.body).contains(A));
    let r = request(
        addr,
        "GET",
        "/api/v1/export/prometheus?foo=1",
        Some(&viewer),
        &[],
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(
        request(
            addr,
            "POST",
            "/api/v1/export/prometheus",
            Some(&viewer),
            &[]
        )
        .await
        .status,
        405
    );

    // 驗證：不合法的端點、啟用 OTLP 卻沒有端點、不能從主控台改的鍵、環境變數鎖定
    for (body, param) in [
        (
            json!({"otlp": {"endpoint": "collector:4318"}}),
            xs::KEY_OTLP_ENDPOINT,
        ),
        (json!({"otlp": {"enabled": true}}), xs::KEY_OTLP_ENDPOINT),
        (json!({"otlp": {"interval_secs": 0}}), xs::KEY_OTLP_INTERVAL),
        (
            json!({"otlp": {"queue_max_points": 5}}),
            "otlp.queue_max_points",
        ),
        (
            json!({"otlp": {"headers": [{"name": "Host", "value": "x"}]}}),
            xs::KEY_OTLP_HEADERS,
        ),
    ] {
        let r = call(
            addr,
            "PUT",
            "/api/v1/export/settings",
            Some(&admin),
            Some(&body),
        )
        .await;
        assert_eq!(r.status, 400, "{body}");
        assert_eq!(
            r.json()["error"]["product_code"],
            super::CODE_EXPORT_CONFIG,
            "{body}"
        );
        assert_eq!(r.json()["error"]["details"]["parameter"], param, "{body}");
    }
    let r = call(
        addr,
        "PUT",
        "/api/v1/export/settings",
        Some(&admin),
        Some(&json!({"otlp": {"naming": "kairos"}})),
    )
    .await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json()["error"]["code"], "locked_by_environment");

    // OTLP：端點＋標頭（加密保存、只回傳遮蔽值）→ 推送到假的 collector
    let r = call(
        addr,
        "PUT",
        "/api/v1/export/settings",
        Some(&admin),
        Some(&json!({"otlp": {"enabled": true, "endpoint": col.url(false), "interval_secs": 1, "headers": [{"name": "Authorization", "value": "Bearer otlp-secret-xyz"}]}})),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let d = r.json()["data"].clone();
    assert_eq!(
        d["otlp"]["headers"],
        json!([{"name": "Authorization", "value": "••••"}])
    );
    assert_eq!(d["otlp"]["headers_source"], "console");
    assert!(!String::from_utf8_lossy(&r.body).contains("otlp-secret-xyz"));
    let stored: String = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [xs::KEY_OTLP_HEADERS],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!stored.contains("otlp-secret-xyz"), "資料庫中不可有明文");
    let audit_all: String = conn
        .query_row(
            "SELECT group_concat(coalesce(details, ''), ' ') FROM audit_events",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!audit_all.contains("otlp-secret-xyz"), "稽核不可有標頭的值");
    // 測試連線
    let r = call(addr, "POST", "/api/v1/export/otlp/test", Some(&admin), None).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["data"]["ok"], true);
    assert_eq!(
        col.all().last().unwrap().header("authorization").as_deref(),
        Some("Bearer otlp-secret-xyz")
    );
    assert_eq!(
        call(
            addr,
            "POST",
            "/api/v1/export/otlp/test",
            Some(&operator),
            None
        )
        .await
        .status,
        403
    );
    // 新的 report → collector
    let before = col.count();
    let outcome = crate::connection::store_report(
        &f.registry,
        &f.sink,
        21,
        crate::store::tests::pulse_like(A, 2, chrono::Utc::now(), 40.0),
        Duration::from_secs(5),
        &revived,
    )
    .await;
    assert!(matches!(
        outcome,
        crate::connection::StoreOutcome::Stored(_)
    ));
    wait_for("collector 收到 report", Duration::from_secs(10), || {
        col.all().iter().skip(before).any(|c| !c.body.is_empty())
    })
    .await;
    let req = col
        .all()
        .into_iter()
        .skip(before)
        .find(|c| !c.body.is_empty())
        .unwrap();
    let res = decode_request(&req.body);
    assert_eq!(res[0].attrs["host.id"], A);
    // host.name 是 report 的主機名稱（連線的 hello；pulse_like 為 host-<agent_id>）
    assert_eq!(res[0].attrs["host.name"], format!("host-{A}"));
    let r = request(addr, "GET", "/api/v1/export/settings", Some(&admin), &[]).await;
    let d = r.json()["data"].clone();
    wait_for("推送狀態", Duration::from_secs(5), || {
        hub.stats.otlp_requests_ok.load(Ordering::Relaxed) >= 1
    })
    .await;
    assert!(["ok", "pending"].contains(&d["otlp"]["status"]["state"].as_str().unwrap()));

    // null：改用設定檔的值（刪除資料庫的值）
    let r = call(
        addr,
        "PUT",
        "/api/v1/export/settings",
        Some(&admin),
        Some(&json!({"otlp": {"enabled": null, "headers": null}})),
    )
    .await;
    assert_eq!(r.status, 200);
    let d = r.json()["data"].clone();
    assert_eq!(d["otlp"]["enabled"], false);
    assert_eq!(d["otlp"]["headers_source"], "none");
    assert_eq!(src(&d, xs::KEY_OTLP_ENABLED), "default");
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM settings WHERE key IN (?1, ?2)",
            [xs::KEY_OTLP_ENABLED, xs::KEY_OTLP_HEADERS],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
    // 重新啟動後沿用資料庫的值（exporter 仍啟用、端點仍是 collector）
    let rows =
        xs::load_rows(&crate::store::sqlite::SqliteOptions::from_config(&f.config, &data_dir).path);
    let e = xs::effective(&f.config, &rows, None);
    assert!(e.prometheus.enabled);
    assert_eq!(e.otlp.endpoint, col.url(false));
    // 自身的 /metrics
    let r = request(addr, "GET", "/metrics", None, &[]).await;
    let m = kp_obs::prom::parse(&String::from_utf8_lossy(&r.body)).unwrap();
    assert!(
        m.get("kairos_export_prometheus_scrapes_total", &[])
            .unwrap()
            >= 2.0
    );
    assert_eq!(m.get("kairos_export_prometheus_enabled", &[]), Some(1.0));
    assert!(
        m.get("kairos_export_otlp_requests_total", &[("result", "ok")])
            .unwrap()
            >= 1.0
    );
    drop(conn);
    f.stop().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// 群組範圍的 API token（屬於一個有群組範圍角色的使用者）
fn group_token(f: &crate::api::tests::Fixture, a: Assignment) -> String {
    let conn = f.admin();
    let now = chrono::Utc::now().timestamp_millis();
    let user_id = conn
        .create_user(
            &crate::users::store::NewUser {
                username: "scoped",
                source: crate::users::store::SOURCE_LOCAL,
                ..crate::users::store::NewUser::default()
            },
            "test",
            now,
        )
        .unwrap()
        .unwrap();
    conn.add_assignment(
        crate::users::store::SUBJECT_USER,
        &user_id.to_string(),
        &a,
        "test",
        now,
    )
    .unwrap();
    tokens::create_with(
        &conn,
        "scoped",
        &TokenGrant::User {
            user_id,
            username: "scoped".into(),
            role: None,
        },
        None,
        Some("tester"),
        None,
        now,
    )
    .unwrap()
    .token
}
