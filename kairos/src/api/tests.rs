//! 主控台 API 的整合測試：以 [`super::start`] 啟動實際的 HTTPS 伺服器（SQLite、狀態表、API token），
//! 再以真的 TLS 用戶端（tokio-rustls，信任 `pulse/testdata/server-ca.pem`）送出 HTTP 請求。
//!
//! 請求一律用 HTTP/1.0（`Connection: close`）：回應不使用 chunked，SSE 的本文就是原始的事件串流。

use super::*;
use crate::protocol::HostInfo;
use crate::registry::HelloInfo;
use crate::storage::{ReportSink, prepare_data_dir};
use crate::store::tests::pulse_like;
use crate::tls::{RejectionReason, TlsPaths};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");
// 不使用測試 PKI 中憑證的 agent_id：tls 的測試在同一個行程中記錄被拒的憑證（全域的紀錄）
const A: &str = "6f1c0e4a-1b2c-4d3e-9f00-1122334455aa";
const B: &str = "0a9b8c7d-0000-4000-8000-000000000001";
const C: &str = "91d2e0aa-0000-4000-8000-000000000002";
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Fixture {
    pub dir: PathBuf,
    pub console: Console,
    pub registry: Arc<Registry>,
    pub sink: ReportSink,
    pub token: String,
    pub shutdown: Arc<Shutdown>,
    writer: tokio::task::JoinHandle<()>,
    pub config: AppConfig,
    /// 告警服務（`fixture_with(.., true)` 時）
    pub alerts: Option<Arc<crate::alerts::AlertService>>,
    alerts_thread: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    pub fn addr(&self) -> SocketAddr {
        self.console.local_addr
    }

    pub async fn stop(self) {
        self.shutdown.trigger();
        tokio::time::timeout(Duration::from_secs(10), self.console.join())
            .await
            .expect("主控台應在關機後結束");
        drop(self.alerts);
        if let Some(t) = self.alerts_thread {
            crate::alerts::AlertService::join(t, Duration::from_secs(10)).await;
        }
        drop(self.sink);
        let _ = tokio::time::timeout(Duration::from_secs(10), self.writer).await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    /// 管理用的連線（直接檢查資料庫；只經由 SQLite 開啟，不影響 writer 的檔案鎖）
    pub fn admin(&self) -> rusqlite::Connection {
        crate::store::sqlite::open_existing_read_write(
            &SqliteOptions::from_config(&self.config, Path::new(&self.config.storage.data_dir))
                .path,
            Duration::from_secs(5),
        )
        .unwrap()
    }
}

use std::path::Path;

pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kairos-api-{tag}-{}-{}",
        std::process::id(),
        crate::registry::next_conn_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 啟動主控台（SQLite 資料庫、狀態表、一個唯讀 token）
pub(crate) async fn fixture(tag: &str, tweak: impl FnOnce(&mut AppConfig)) -> Fixture {
    fixture_with(tag, tweak, false).await
}

fn real_clock() -> i64 {
    Utc::now().timestamp_millis()
}

/// [`fixture`]；`alerts` 時另外啟動告警服務（與正式程式相同的執行緒與判定週期）
pub(crate) async fn fixture_with(
    tag: &str,
    tweak: impl FnOnce(&mut AppConfig),
    alerts: bool,
) -> Fixture {
    let dir = temp_dir(tag);
    let mut config = AppConfig::default();
    config.storage.data_dir = dir.join("data").to_string_lossy().into_owned();
    config.storage.max_total_bytes = 0;
    config.server.cert_path = format!("{MANIFEST}/testdata/pki/server.crt");
    config.server.key_path = format!("{MANIFEST}/testdata/pki/server.key");
    config.console.listen = "127.0.0.1:0".into();
    // 主控台用 CA:FALSE、SAN 為 localhost 的伺服器憑證（用戶端以 server-ca.pem 驗證）
    config.console.cert_path = format!("{MANIFEST}/../pulse/testdata/server.pem");
    config.console.key_path = format!("{MANIFEST}/../pulse/testdata/server.key");
    tweak(&mut config);
    let paths = prepare_data_dir(Path::new(&config.storage.data_dir))
        .await
        .unwrap();
    let storage = crate::store::start(&config, &paths.data_dir, Utc::now)
        .await
        .unwrap();
    let writer = tokio::spawn(storage.writer);
    let registry = Arc::new(Registry::new());
    let conn = crate::store::sqlite::open_existing_read_write(
        &SqliteOptions::from_config(&config, &paths.data_dir).path,
        Duration::from_secs(5),
    )
    .unwrap();
    let token = tokens::create(
        &conn,
        "test",
        tokens::SCOPE_READ,
        None,
        Some("tester"),
        Utc::now().timestamp_millis(),
    )
    .unwrap()
    .token;
    drop(conn);
    let agent_tls = Arc::new(TlsState::new(
        crate::tls::build(&TlsPaths {
            cert: config.server.cert_path.clone(),
            key: config.server.key_path.clone(),
            client_ca: None,
            crls: Vec::new(),
            issuing_cas: None,
            issuing_crl: None,
        })
        .unwrap(),
    ));
    let shutdown = Arc::new(Shutdown::new());
    let (alert_svc, alerts_thread) = if alerts {
        let (svc, thread) = crate::alerts::AlertService::start(crate::alerts::StartParams {
            config: Arc::new(config.clone()),
            data_dir: paths.data_dir.clone(),
            db_path: SqliteOptions::from_config(&config, &paths.data_dir).path,
            registry: registry.clone(),
            write_stats: Some(storage.reports.stats().clone()),
            per_ip: None,
            tls: Some(agent_tls.clone()),
            enroll: None,
            agent_listen: Some("127.0.0.1:8080".parse().unwrap()),
            shutdown: shutdown.clone(),
            enroll_closed_default: false,
            clock: real_clock,
        })
        .await
        .unwrap();
        (Some(svc), Some(thread))
    } else {
        (None, None)
    };
    let console = match start(StartParams {
        config: Arc::new(config.clone()),
        config_file: None,
        registry: registry.clone(),
        write_stats: storage.reports.stats().clone(),
        agent_tls,
        per_ip: None,
        data_dir: paths.data_dir.clone(),
        shutdown: shutdown.clone(),
        agent_listen: "127.0.0.1:8080".parse().unwrap(),
        enroll: None,
        alerts: alert_svc.clone(),
    })
    .await
    {
        Ok(c) => c,
        Err(e) => panic!("主控台無法啟動: {e:?}"),
    };
    Fixture {
        dir,
        console,
        registry,
        sink: storage.reports,
        token,
        shutdown,
        writer,
        config,
        alerts: alert_svc,
        alerts_thread,
    }
}

fn connector() -> TlsConnector {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls::pki_types::CertificateDer::pem_file_iter(format!(
        "{MANIFEST}/../pulse/testdata/server-ca.pem"
    ))
    .unwrap()
    {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

async fn connect(addr: SocketAddr) -> TlsStream<TcpStream> {
    let tcp = TcpStream::connect(addr).await.unwrap();
    connector()
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            tcp,
        )
        .await
        .expect("TLS 握手失敗")
}

/// 一個 HTTP 回應
#[derive(Debug)]
pub(crate) struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "回應不是 JSON（{e}）：{} {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

fn parse_head(head: &str) -> (u16, Vec<(String, String)>) {
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    (status, headers)
}

fn request_text(method: &str, path: &str, token: Option<&str>, extra: &[(&str, &str)]) -> String {
    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: localhost\r\n");
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req
}

/// 送出一個請求並讀完回應
pub(crate) async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    extra: &[(&str, &str)],
) -> Resp {
    let mut tls = connect(addr).await;
    tls.write_all(request_text(method, path, token, extra).as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(IO_TIMEOUT, tls.read_to_end(&mut buf))
        .await
        .expect("讀取回應逾時")
        .ok();
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("回應沒有標頭");
    let (status, headers) = parse_head(&String::from_utf8_lossy(&buf[..split]));
    Resp {
        status,
        headers,
        body: buf[split + 4..].to_vec(),
    }
}

/// 送出帶 JSON 本文的請求
pub(crate) async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Resp {
    let mut tls = connect(addr).await;
    let text = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: localhost\r\n");
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if body.is_some() {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            text.len()
        ));
    }
    req.push_str("\r\n");
    req.push_str(&text);
    tls.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(IO_TIMEOUT, tls.read_to_end(&mut buf))
        .await
        .expect("讀取回應逾時")
        .ok();
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("回應沒有標頭");
    let (status, headers) = parse_head(&String::from_utf8_lossy(&buf[..split]));
    Resp {
        status,
        headers,
        body: buf[split + 4..].to_vec(),
    }
}

pub(crate) async fn get(f: &Fixture, path: &str) -> Resp {
    request(f.addr(), "GET", path, Some(&f.token), &[]).await
}

/// SSE 的一則事件
#[derive(Debug, Clone)]
pub(crate) struct Event {
    pub id: Option<String>,
    pub event: String,
    pub data: Value,
}

/// 讀取中的 SSE 串流
pub(crate) struct Sse {
    tls: TlsStream<TcpStream>,
    buf: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl Sse {
    pub async fn open(addr: SocketAddr, token: &str, last_event_id: Option<&str>) -> Sse {
        let mut tls = connect(addr).await;
        let mut extra = vec![("Accept", "text/event-stream")];
        if let Some(id) = last_event_id {
            extra.push(("Last-Event-ID", id));
        }
        tls.write_all(request_text("GET", "/api/v1/events", Some(token), &extra).as_bytes())
            .await
            .unwrap();
        let mut raw = Vec::new();
        let split = loop {
            if let Some(p) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break p;
            }
            let mut chunk = [0u8; 4096];
            let n = tokio::time::timeout(IO_TIMEOUT, tls.read(&mut chunk))
                .await
                .expect("讀取 SSE 標頭逾時")
                .unwrap();
            assert!(n > 0, "連線在標頭之前就關閉了");
            raw.extend_from_slice(&chunk[..n]);
        };
        let (status, headers) = parse_head(&String::from_utf8_lossy(&raw[..split]));
        Sse {
            tls,
            buf: String::from_utf8_lossy(&raw[split + 4..]).into_owned(),
            status,
            headers,
        }
    }

    /// 下一則事件（略過心跳註解與 `retry:`）；連線關閉時為 `None`
    pub async fn next(&mut self) -> Option<Event> {
        loop {
            while let Some(p) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..p + 2).collect();
                let (mut id, mut event, mut data) = (None, None, String::new());
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("id: ") {
                        id = Some(v.to_string());
                    } else if let Some(v) = line.strip_prefix("event: ") {
                        event = Some(v.to_string());
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        data.push_str(v);
                    }
                }
                if let Some(event) = event {
                    return Some(Event {
                        id,
                        event,
                        data: serde_json::from_str(&data).unwrap_or(Value::Null),
                    });
                }
            }
            let mut chunk = [0u8; 4096];
            let n = tokio::time::timeout(IO_TIMEOUT, self.tls.read(&mut chunk))
                .await
                .expect("等待 SSE 事件逾時")
                .unwrap_or(0);
            if n == 0 {
                return None;
            }
            self.buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    }

    /// 等到某種事件（略過其他的）
    pub async fn wait_for(&mut self, kind: &str) -> Event {
        loop {
            let e = self
                .next()
                .await
                .unwrap_or_else(|| panic!("等待 {kind} 時串流結束"));
            if e.event == kind {
                return e;
            }
        }
    }
}

pub(crate) fn hello(id: &str, name: &str, peer: &str) -> HelloInfo {
    HelloInfo {
        agent_id: id.into(),
        hostname: name.into(),
        agent_version: "0.3.0".into(),
        proto: 2,
        host: Some(Arc::new(HostInfo {
            host_fingerprint: Some(format!("sha256:{}", "ab".repeat(32))),
            display_name: None,
            tags: Some(vec!["line3".into()]),
            report_interval_ms: Some(5000),
            os: Some(kairos_proto::OsInfo {
                name: Some("Ubuntu".into()),
                version: Some("24.04".into()),
                kernel: None,
                arch: Some("x86_64".into()),
            }),
        })),
        peer: peer.into(),
        offline_after: Duration::from_secs(75),
        client_chain: None,
    }
}

#[tokio::test]
async fn hosts_metrics_overview_and_health_over_https() {
    let f = fixture("hosts", |_| {}).await;
    let now = Utc::now();
    // A：線上、協商了 durable；B：這次啟動後離線；C：只在清冊中
    let _a = f
        .registry
        .register(
            11,
            hello(A, "hmi-line3-07", "10.30.3.27:51544"),
            Instant::now(),
            now,
        )
        .unwrap();
    f.registry.set_durable(A, 11, true);
    let _b = f
        .registry
        .register(12, hello(B, "db-02", "10.30.4.9:5000"), Instant::now(), now)
        .unwrap();
    // 與連線相同的保存路徑（寫入統計、狀態表的 last_report）
    let store = |agent: &'static str, conn_id: u64, record| {
        let registry = f.registry.clone();
        let sink = f.sink.clone();
        async move {
            let revived = std::sync::atomic::AtomicBool::new(false);
            let outcome = crate::connection::store_report(
                &registry,
                &sink,
                conn_id,
                record,
                Duration::from_secs(5),
                &revived,
            )
            .await;
            assert!(
                matches!(outcome, crate::connection::StoreOutcome::Stored(_)),
                "{agent}: {outcome:?}"
            );
        }
    };
    for (i, secs) in [120, 60, 0].iter().enumerate() {
        let at = now - chrono::Duration::seconds(*secs);
        store(
            A,
            11,
            pulse_like(A, i as u64 + 1, at, 10.0 * (i + 1) as f64),
        )
        .await;
    }
    store(B, 12, pulse_like(B, 1, now, 5.0)).await;
    f.sink.write(pulse_like(C, 1, now, 1.0)).await.unwrap();
    assert!(f.registry.disconnect(B, 12, Instant::now()));
    // 索引最多 1 秒舊：讓這個測試的第一個查詢看到上面的狀態
    f.console.ctx.hosts.invalidate().await;

    // 每個回應都有 server_time 與 no-store；沒有 CORS 標頭
    let r = request(
        f.addr(),
        "GET",
        "/api/v1/hosts",
        Some(&f.token),
        &[("Origin", "https://evil.example")],
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert!(r.header("access-control-allow-origin").is_none());
    assert!(
        r.header("content-type")
            .unwrap()
            .starts_with("application/json")
    );
    let v = r.json();
    assert!(v["server_time"].as_str().unwrap().ends_with('Z'));
    assert_eq!(v["page"]["total"], 3);
    assert_eq!(v["meta"]["status_counts"]["online"], 1);
    assert_eq!(v["meta"]["status_counts"]["offline"], 2);
    assert_eq!(v["meta"]["status_counts"]["critical"], Value::Null);
    assert!(v["meta"]["last_event_id"].as_str().is_some());
    // 預設排序：離線在前，線上的最後
    let data = v["data"].as_array().unwrap();
    assert_eq!(data[2]["agent_id"], A);
    let a = &data[2];
    assert_eq!(a["status"], "online");
    assert_eq!(a["ip"], "10.30.3.27");
    assert_eq!(a["durable"], true);
    assert_eq!(a["tags"][0], "line3");
    assert_eq!(a["os"]["name"], "Ubuntu");
    assert_eq!(a["last_report"]["cpu_usage"], 30.0);
    let b = data.iter().find(|h| h["agent_id"] == B).unwrap();
    assert_eq!(b["status_detail"]["offline_reason"], "connection_closed");
    let c = data.iter().find(|h| h["agent_id"] == C).unwrap();
    assert_eq!(c["ip"], "10.0.0.1");
    assert_eq!(c["durable"], Value::Null);

    // 篩選
    let ids = |r: &Resp| -> Vec<String> {
        r.json()["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["agent_id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(&get(&f, "/api/v1/hosts?status=online").await), [A]);
    assert_eq!(ids(&get(&f, "/api/v1/hosts?q=10.30.3.27").await), [A]);
    assert_eq!(
        ids(&get(&f, "/api/v1/hosts?q=10.30.0.0%2F16&sort=ip").await),
        [A, B]
    );
    assert_eq!(
        ids(&get(
            &f,
            "/api/v1/hosts?subnet=10.0.0.0/8&status=offline&sort=-hostname"
        )
        .await)
        .len(),
        2
    );
    assert_eq!(ids(&get(&f, "/api/v1/hosts?q=HMI-line3").await), [A]);
    assert_eq!(ids(&get(&f, "/api/v1/hosts?q=0a9b8c7d").await), [B]);
    assert_eq!(ids(&get(&f, "/api/v1/hosts?durable=true").await), [A]);
    assert_eq!(
        ids(&get(&f, "/api/v1/hosts?offline_reason=connection_closed").await),
        [B]
    );
    // 游標分頁：每頁 1 台，3 頁走完
    let mut seen = Vec::new();
    let mut cursor = String::new();
    loop {
        let path = format!("/api/v1/hosts?limit=1&sort=hostname{cursor}");
        let v = get(&f, &path).await.json();
        seen.extend(
            v["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["agent_id"].clone()),
        );
        match v["page"]["next_cursor"].as_str() {
            Some(c) => cursor = format!("&cursor={c}"),
            None => break,
        }
    }
    assert_eq!(seen.len(), 3);
    // 錯誤的參數
    for (path, code) in [
        ("/api/v1/hosts?status=down", "invalid_parameter"),
        ("/api/v1/hosts?limit=0", "invalid_parameter"),
        ("/api/v1/hosts?color=red", "invalid_parameter"),
        ("/api/v1/hosts?sort=hostname&cursor=abc", "invalid_cursor"),
    ] {
        let r = get(&f, path).await;
        assert_eq!(r.status, 400, "{path}");
        assert_eq!(r.json()["error"]["code"], code, "{path}");
    }

    // 詳情
    let r = get(&f, &format!("/api/v1/hosts/{}", A.to_uppercase())).await;
    assert_eq!(r.status, 200);
    let d = r.json()["data"].clone();
    assert_eq!(d["agent_id"], A);
    assert_eq!(d["host_fingerprint"], format!("sha256:{}", "ab".repeat(32)));
    assert_eq!(d["connection"]["durable"], true);
    assert_eq!(d["last_report"]["metrics"], 17);
    assert_eq!(d["certificate_rejection"], Value::Null);
    for path in [
        "/api/v1/hosts/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
        "/api/v1/hosts/not-a-uuid",
    ] {
        let r = get(&f, path).await;
        assert_eq!(r.status, 404, "{path}");
        assert_eq!(r.json()["error"]["code"], "host_not_found");
    }

    // 指標清單與趨勢
    let v = get(&f, &format!("/api/v1/hosts/{A}/metrics")).await.json();
    let names: Vec<&str> = v["data"]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"system.cpu.usage"), "{names:?}");
    let v = get(
        &f,
        &format!("/api/v1/hosts/{A}/metrics/system.cpu.usage?range=1h"),
    )
    .await
    .json();
    let t = &v["data"];
    assert_eq!(t["resolution"], "raw");
    assert_eq!(t["bucket_secs"], Value::Null);
    let points = t["series"][0]["points"].as_array().unwrap();
    assert_eq!(points.len(), 3);
    assert_eq!(points[0]["min"], 10.0);
    assert_eq!(points[2]["count"], 1);
    // 5 分鐘：最新的桶尚未寫出彙總，由原始資料計算
    let v = get(
        &f,
        &format!("/api/v1/hosts/{A}/metrics/system.cpu.usage?range=1h&resolution=5m"),
    )
    .await
    .json();
    let points = v["data"]["series"][0]["points"].as_array().unwrap().clone();
    let count: u64 = points.iter().map(|p| p["count"].as_u64().unwrap()).sum();
    assert_eq!(count, 3, "{points:?}");
    assert_eq!(v["data"]["bucket_secs"], 300);
    let min = points
        .iter()
        .map(|p| p["min"].as_f64().unwrap())
        .fold(f64::MAX, f64::min);
    let max = points
        .iter()
        .map(|p| p["max"].as_f64().unwrap())
        .fold(f64::MIN, f64::max);
    assert_eq!((min, max), (10.0, 30.0));
    // 有 label 的序列分開；label 篩選
    let v = get(
        &f,
        &format!("/api/v1/hosts/{A}/metrics/system.filesystem.available?range=1h"),
    )
    .await
    .json();
    assert_eq!(v["data"]["series"].as_array().unwrap().len(), 3);
    let v = get(
        &f,
        &format!(
            "/api/v1/hosts/{A}/metrics/system.filesystem.available?range=1h&label.mountpoint=%2F"
        ),
    )
    .await
    .json();
    let series = v["data"]["series"].as_array().unwrap();
    assert_eq!(series.len(), 1);
    assert_eq!(series[0]["labels"]["mountpoint"], "/");
    // 點數未超過上限時不合併（合併的規則見 metrics::tests）
    let v = get(
        &f,
        &format!(
            "/api/v1/hosts/{A}/metrics/system.cpu.usage?range=1h&resolution=raw&max_points=10"
        ),
    )
    .await
    .json();
    assert_eq!(v["data"]["bucket_secs"], Value::Null);
    assert_eq!(
        v["data"]["series"][0]["points"].as_array().unwrap().len(),
        3
    );
    for (path, status, code) in [
        ("metrics/foo.bar", 404, "metric_not_found"),
        ("metrics/Foo", 400, "invalid_parameter"),
        (
            "metrics/system.cpu.usage?resolution=1m",
            400,
            "invalid_parameter",
        ),
        (
            "metrics/system.cpu.usage?range=1h&from=2026-01-01T00:00:00Z",
            400,
            "invalid_parameter",
        ),
        (
            "metrics/system.cpu.usage?from=2026-01-02T00:00:00Z&to=2026-01-01T00:00:00Z",
            400,
            "invalid_parameter",
        ),
    ] {
        let r = get(&f, &format!("/api/v1/hosts/{A}/{path}")).await;
        assert_eq!(
            r.status,
            status,
            "{path}: {}",
            String::from_utf8_lossy(&r.body)
        );
        assert_eq!(r.json()["error"]["code"], code, "{path}");
    }

    // 總覽與系統健康
    let v = get(&f, "/api/v1/overview").await.json();
    assert_eq!(v["data"]["total"], 3);
    assert_eq!(v["data"]["status_counts"]["online"], 1);
    assert_eq!(v["data"]["delivery"]["durable"], 1);
    assert_eq!(v["data"]["top_cpu"][0]["agent_id"], A);
    assert_eq!(v["data"]["longest_offline"].as_array().unwrap().len(), 2);
    let v = get(&f, "/api/v1/system/health").await.json();
    let items = v["data"]["items"].as_array().unwrap();
    let item = |id: &str| items.iter().find(|i| i["id"] == id).unwrap().clone();
    assert_eq!(item("database")["status"], "ok");
    // C 的 report 不經過連線（直接寫入 writer），不在統計中
    assert_eq!(item("report_writes")["values"]["since_start"]["written"], 4);
    assert_eq!(item("report_writes")["status"], "ok");
    assert_eq!(item("delivery")["values"]["durable"], 1);
    assert!(item("tls")["values"]["expiries"].as_array().unwrap().len() == 1);
    assert_eq!(v["data"]["service"]["version"], env!("CARGO_PKG_VERSION"));

    // /me
    let v = get(&f, "/api/v1/me").await.json();
    assert_eq!(v["data"]["type"], "api_token");
    assert_eq!(v["data"]["scopes"][0], "read");
    f.stop().await;
}

#[tokio::test]
async fn requests_are_authenticated_and_validated() {
    let f = fixture("auth", |_| {}).await;
    let addr = f.addr();
    // 沒有 token、格式錯誤、不存在的 token
    let r = request(addr, "GET", "/api/v1/hosts", None, &[]).await;
    assert_eq!(r.status, 401);
    assert_eq!(
        r.header("www-authenticate"),
        Some("Bearer realm=\"kairos\"")
    );
    assert_eq!(r.json()["error"]["code"], "unauthorized");
    assert_eq!(r.json()["error"]["product_code"], "KRS-E6010");
    let r = request(addr, "GET", "/api/v1/hosts", Some("nope"), &[]).await;
    assert_eq!(r.status, 401);
    let (id, secret) = tokens::generate().unwrap();
    let r = request(
        addr,
        "GET",
        "/api/v1/hosts",
        Some(&format!("kat_{id}_{secret}")),
        &[],
    )
    .await;
    assert_eq!(r.status, 401);
    // 秘密錯誤的已知 token_id
    let parsed = tokens::parse_token(&f.token).unwrap();
    let wrong = format!("kat_{}_{}", parsed.token_id, secret);
    let r = request(addr, "GET", "/api/v1/hosts", Some(&wrong), &[]).await;
    assert_eq!(r.status, 401);
    // 方法、本文、路徑
    let r = request(addr, "POST", "/api/v1/hosts", Some(&f.token), &[]).await;
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("GET, HEAD"));
    let r = request(
        addr,
        "GET",
        "/api/v1/hosts",
        Some(&f.token),
        &[("Content-Length", "0")],
    )
    .await;
    assert_eq!(r.status, 200);
    let r = request(addr, "DELETE", "/api/v1/hosts/x", Some(&f.token), &[]).await;
    assert_eq!(r.status, 405);
    for path in [
        "/",
        "/api/v2/hosts",
        "/api/v1/nothing",
        "/api/v1/hosts/x/y/z/w",
    ] {
        let r = request(addr, "GET", path, Some(&f.token), &[]).await;
        assert_eq!(r.status, 404, "{path}");
        assert_eq!(r.json()["error"]["code"], "not_found");
    }
    // HEAD：沒有本文
    let r = request(addr, "HEAD", "/api/v1/me", Some(&f.token), &[]).await;
    assert_eq!(r.status, 200);
    assert!(r.body.is_empty());
    // 撤銷後立即無法使用
    let conn = f.admin();
    tokens::revoke(
        &conn,
        parsed.token_id,
        Some("tester"),
        None,
        Utc::now().timestamp_millis(),
    )
    .unwrap();
    let r = request(addr, "GET", "/api/v1/me", Some(&f.token), &[]).await;
    assert_eq!(r.status, 401);
    // 已過期的 token
    let expired = tokens::create(&conn, "old", "read", Some(1), None, 0).unwrap();
    let r = request(addr, "GET", "/api/v1/me", Some(&expired.token), &[]).await;
    assert_eq!(r.status, 401);
    // 到目前為止失敗 6 次；繼續失敗直到額度用完（10 次），之後連有效的 token 也 429
    let valid = tokens::create(&conn, "ok", "read", None, None, 0)
        .unwrap()
        .token;
    let mut limited = false;
    for _ in 0..12 {
        match request(addr, "GET", "/api/v1/me", None, &[]).await.status {
            401 => {}
            429 => {
                limited = true;
                break;
            }
            other => panic!("非預期的狀態碼 {other}"),
        }
    }
    assert!(limited, "驗證失敗 10 次後應回 429");
    let r = request(addr, "GET", "/api/v1/me", Some(&valid), &[]).await;
    assert_eq!(r.status, 429, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["error"]["code"], "rate_limited");
    assert_eq!(r.json()["error"]["product_code"], "KRS-W6011");
    assert!(r.header("retry-after").unwrap().parse::<u64>().unwrap() >= 1);
    f.stop().await;
}

/// 等到所有 SSE 的名額都釋放（用戶端關閉連線後，伺服器在下一次讀寫時才發現）
async fn wait_streams_released(f: &Fixture) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while f.console.ctx.streams.available_permits() < f.console.ctx.max_streams {
        assert!(Instant::now() < deadline, "關閉的串流應釋放名額");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn sse_pushes_host_changes_and_resumes_with_last_event_id() {
    let f = fixture("sse", |c| c.console.max_streams = 2).await;
    let addr = f.addr();
    let mut s = Sse::open(addr, &f.token, None).await;
    assert_eq!(s.status, 200);
    assert!(
        s.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("content-type")
                && v.starts_with("text/event-stream"))
    );
    let ready = s.wait_for("ready").await;
    assert_eq!(ready.data["resumed"], false);
    // 新主機上線 → host.added；離線 → host.offline；再上線 → host.online
    let t = Instant::now();
    let _d = f
        .registry
        .register(21, hello(A, "hmi-07", "10.30.3.27:1"), t, Utc::now())
        .unwrap();
    let e = s.wait_for("host.added").await;
    assert_eq!(e.data["agent_id"], A);
    assert_eq!(e.data["status"], "online");
    assert_eq!(e.data["ip"], "10.30.3.27");
    let first_id = e.id.clone().unwrap();
    assert!(f.registry.disconnect(A, 21, t));
    let e = s.wait_for("host.offline").await;
    assert_eq!(e.data["reason"], "connection_closed");
    let _d = f
        .registry
        .register(22, hello(A, "hmi-07", "10.30.3.27:2"), t, Utc::now())
        .unwrap();
    let e = s.wait_for("host.online").await;
    let last_seen_id = e.id.unwrap();
    drop(s);
    wait_streams_released(&f).await;

    // 斷線期間的事件：以 Last-Event-ID 重連時補送
    let _e = f
        .registry
        .register(23, hello(B, "db-02", "10.30.4.9:1"), t, Utc::now())
        .unwrap();
    assert!(f.registry.disconnect(B, 23, t));
    let mut s = Sse::open(addr, &f.token, Some(&last_seen_id)).await;
    let ready = s.wait_for("ready").await;
    assert_eq!(ready.data["resumed"], true);
    let e = s.next().await.unwrap();
    assert_eq!(
        (e.event.as_str(), e.data["agent_id"].as_str()),
        ("host.added", Some(B))
    );
    let e = s.next().await.unwrap();
    assert_eq!(
        (e.event.as_str(), e.data["agent_id"].as_str()),
        ("host.offline", Some(B))
    );
    // 列表的 meta.last_event_id 與最新的事件一致
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let v = get(&f, "/api/v1/hosts").await.json();
    assert_eq!(v["meta"]["last_event_id"].as_str(), e.id.as_deref());

    // 另一次啟動的編號：要求重新抓取
    let mut s2 = Sse::open(addr, &f.token, Some("zzzz-1")).await;
    assert_eq!(s2.wait_for("ready").await.data["resumed"], false);
    assert_eq!(s2.next().await.unwrap().event, "resync");
    // 同時連線數上限（2）
    let r = request(addr, "GET", "/api/v1/events", Some(&f.token), &[]).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.json()["error"]["code"], "too_many_streams");
    assert_eq!(r.header("retry-after"), Some("10"));
    drop(s2);
    // 用戶端關閉後名額釋放，可以再連
    let deadline = Instant::now() + Duration::from_secs(20);
    while f.console.ctx.streams.available_permits() < 1 {
        assert!(Instant::now() < deadline, "關閉的串流應釋放名額");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut s3 = Sse::open(addr, &f.token, Some(&first_id)).await;
    assert_eq!(s3.status, 200);
    assert_eq!(s3.wait_for("ready").await.data["resumed"], true);
    drop(s3);
    // 關機：串流結束
    f.shutdown.trigger();
    while s.next().await.is_some() {}
    f.stop().await;
}

#[tokio::test]
async fn rejected_certificates_show_up_in_details_and_the_inventory() {
    // 只在這個測試使用的 agent_id（被拒的憑證是行程內全域的紀錄）
    const R: &str = "c0ffee00-0000-4000-8000-0000000000aa";
    let f = fixture("reject", |_| {}).await;
    f.sink
        .write(pulse_like(R, 1, Utc::now(), 1.0))
        .await
        .unwrap();
    f.console.ctx.hosts.invalidate().await;
    crate::tls::rejections().record(
        R.to_string(),
        RejectionReason::Revoked,
        Some("1000".into()),
        Some("10.30.3.27".parse().unwrap()),
        Utc::now() + chrono::Duration::seconds(5),
    );
    let d = get(&f, &format!("/api/v1/hosts/{R}")).await.json()["data"].clone();
    assert_eq!(d["certificate_rejection"]["reason"], "revoked");
    assert_eq!(d["certificate_rejection"]["product_code"], "KRS-W1105");
    assert_eq!(d["certificate_rejection"]["serial"], "1000");
    assert_eq!(d["status_detail"]["offline_reason"], "certificate_rejected");
    // 列表的篩選
    let v = get(&f, "/api/v1/hosts?offline_reason=certificate_rejected")
        .await
        .json();
    assert!(
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["agent_id"] == R)
    );
    // 背景工作（每 10 秒）寫入主機清冊與 token 的稽核日誌；這裡直接呼叫同一個函式
    let conn = f.admin();
    let mut persisted = false;
    for _ in 0..100 {
        super::maint::persist_rejections(&f.console.ctx).await;
        super::maint::audit_tokens(&f.console.ctx).await;
        let (at, logged): (Option<i64>, i64) = conn
            .query_row(
                "SELECT (SELECT cert_rejected_at FROM hosts WHERE agent_id = ?1), (SELECT min(logged) FROM api_tokens)",
                [R],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        if at.is_some() && logged == tokens::LOGGED_CREATED {
            persisted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(persisted, "被拒的憑證與 token 的稽核應寫入資料庫");
    let json: String = conn
        .query_row(
            "SELECT cert_rejection FROM hosts WHERE agent_id = ?1",
            [R],
            |r| r.get(0),
        )
        .unwrap();
    let stored: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(stored["reason"], "revoked");
    assert!(stored["count"].as_u64().unwrap() >= 1);
    f.stop().await;
}
