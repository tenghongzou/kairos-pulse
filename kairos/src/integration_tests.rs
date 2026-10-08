//! 透過真實 TLS 連線測試 `handle_connection`（T1）：hello 狀態機、fatal 後關閉、superseded、
//! 閒置逾時、close_notify，以及 K-S1／K-S3／K-S4／K-S6／K-S8／K-S10／K-C7 的端對端回歸測試。
//!
//! 伺服器在 127.0.0.1 的隨機埠上執行，客戶端使用 tokio-rustls；伺服器憑證沿用 `testdata/pki`，
//! mTLS 的客戶端憑證與 CA 見 [`crate::test_pki`]。

use chrono::Utc;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    CryptoProvider, verify_tls12_signature, verify_tls13_signature,
};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    self as rustls, ClientConfig, DigitallySignedStruct, HandshakeKind, SignatureScheme,
};

use crate::accept_loop;
use crate::auth::BindingPolicy;
use crate::config::TimeoutSettings;
use crate::connection::{ConnContext, REPORT_BURST, REPORT_RATE_PER_SEC};
use crate::per_ip::PerIpLimiter;
use crate::registry::{Registry, next_conn_id};
use crate::shutdown::Shutdown;
use crate::storage::{
    REPORT_FILES, RETENTION_SWEEP_INTERVAL, ReportSink, report_channel, start_report_writer,
};
use crate::test_pki;
use crate::throttle::LogThrottle;
use crate::tls::{self, TlsPaths, TlsState};

const PKI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/pki/");
const ID: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
const OTHER_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
/// 等待單則回應的上限（正常情況下遠小於此）
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// 測試伺服器的選項
struct Options {
    /// 啟用 mTLS 時的 CRL（PEM 內容）；`None` 表示不啟用 mTLS
    mtls_crls: Option<Vec<&'static str>>,
    max_agents: usize,
    max_connections_per_ip: usize,
    timeouts: TimeoutSettings,
    /// 取代真正的 report writer（例如模擬卡住的磁碟）
    reports: Option<ReportSink>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mtls_crls: None,
            max_agents: 100,
            max_connections_per_ip: 32,
            timeouts: TimeoutSettings::default(),
            reports: None,
        }
    }
}

struct Server {
    addr: SocketAddr,
    dir: PathBuf,
    registry: Arc<Registry>,
    tls: Arc<TlsState>,
    shutdown: Arc<Shutdown>,
    accept: JoinHandle<()>,
}

impl Server {
    async fn start(opts: Options) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kairos-it-{}-{}",
            std::process::id(),
            next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tls_paths = Self::tls_paths(&dir, opts.mtls_crls.as_deref());
        let tls = Arc::new(TlsState::new(tls::build(&tls_paths).unwrap()));
        let reports = match opts.reports {
            Some(sink) => sink,
            None => {
                let (sink, writer) =
                    start_report_writer(dir.join("data"), 30, Utc::now, RETENTION_SWEEP_INTERVAL)
                        .await
                        .unwrap();
                tokio::spawn(writer);
                sink
            }
        };
        let registry = Arc::new(Registry::with_max_agents(opts.max_agents));
        let shutdown = Arc::new(Shutdown::new());
        let ctx = Arc::new(ConnContext {
            registry: registry.clone(),
            reports,
            timeouts: opts.timeouts,
            shutdown: shutdown.clone(),
            tls: tls.clone(),
            throttle: Arc::new(LogThrottle::default()),
            binding: BindingPolicy::default(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // 與正式程式相同的 accept 迴圈（含全域與每 IP 的連線上限）
        let per_ip = PerIpLimiter::new(opts.max_connections_per_ip);
        let accept = tokio::spawn(async move {
            let mut connections = accept_loop(listener, ctx, 64, per_ip).await.unwrap();
            // 關機後等待連線送出 close_notify 並結束（JoinSet 被釋放時會中止仍在執行的連線）
            while connections.join_next().await.is_some() {}
        });
        Self {
            addr,
            dir,
            registry,
            tls,
            shutdown,
            accept,
        }
    }

    /// 在 `dir` 寫出 mTLS 用的 CA 與 CRL 檔，回傳 TLS 設定檔路徑
    fn tls_paths(dir: &std::path::Path, crls: Option<&[&str]>) -> TlsPaths {
        let s = |p: PathBuf| p.to_string_lossy().to_string();
        let (client_ca, crl_paths) = match crls {
            None => (None, Vec::new()),
            Some(crls) => {
                let ca = dir.join("client-ca.crt");
                std::fs::write(&ca, test_pki::CA).unwrap();
                let paths = crls
                    .iter()
                    .enumerate()
                    .map(|(i, pem)| {
                        let p = dir.join(format!("crl-{}-{}.crl", next_conn_id(), i));
                        std::fs::write(&p, pem).unwrap();
                        s(p)
                    })
                    .collect();
                (Some(s(ca)), paths)
            }
        };
        TlsPaths {
            cert: format!("{PKI}server.crt"),
            key: format!("{PKI}server.key"),
            client_ca,
            crls: crl_paths,
        }
    }

    /// 熱重載：改用新的 CRL
    fn reload_crls(&self, crls: &[&str]) {
        let paths = Self::tls_paths(&self.dir, Some(crls));
        self.tls.replace(tls::build(&paths).unwrap());
    }

    fn reports_file(&self) -> PathBuf {
        self.dir
            .join("data")
            .join(REPORT_FILES.file_name(Utc::now().date_naive()))
    }

    fn agent_ids(&self) -> Vec<String> {
        self.registry
            .snapshot(Utc::now())
            .agents
            .into_iter()
            .map(|a| a.agent_id)
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.accept.abort();
        // writer 仍開著檔案時 Windows 無法刪除；暫存目錄留給系統清理
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 測試用的伺服器憑證驗證：`testdata/pki/server.crt` 是 CA:TRUE 的自簽憑證，webpki 不接受它作為
/// 終端憑證，因此改為釘選該憑證的 DER；握手簽章仍以 ring 實際驗證。
#[derive(Debug)]
struct PinnedServerCert {
    der: CertificateDer<'static>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedServerCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.der.as_ref() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "unexpected server certificate".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 建立客戶端 TLS 設定：信任測試伺服器憑證；`identity` 為 mTLS 的 (憑證, 私鑰) PEM
fn connector(identity: Option<(&str, &str)>) -> TlsConnector {
    let verifier = PinnedServerCert {
        der: CertificateDer::from_pem_file(format!("{PKI}server.crt")).unwrap(),
        provider: Arc::new(rustls::crypto::ring::default_provider()),
    };
    let builder = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier));
    let config = match identity {
        None => builder.with_no_client_auth(),
        Some((cert, key)) => builder
            .with_client_auth_cert(
                vec![CertificateDer::from_pem_slice(cert.as_bytes()).unwrap()],
                PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap(),
            )
            .unwrap(),
    };
    TlsConnector::from(Arc::new(config))
}

struct Client {
    lines: Lines<BufReader<ReadHalf<TlsStream<TcpStream>>>>,
    writer: WriteHalf<TlsStream<TcpStream>>,
    handshake: Option<HandshakeKind>,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        Self::connect_with(&connector(None), addr).await
    }

    async fn connect_with(connector: &TlsConnector, addr: SocketAddr) -> Self {
        let tcp = TcpStream::connect(addr).await.unwrap();
        let tls = connector
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .unwrap();
        let handshake = tls.get_ref().1.handshake_kind();
        let (r, w) = tokio::io::split(tls);
        Self {
            lines: BufReader::new(r).lines(),
            writer: w,
            handshake,
        }
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn send(&mut self, line: &str) {
        self.send_raw(format!("{line}\n").as_bytes()).await;
    }

    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(RECV_TIMEOUT, self.lines.next_line())
            .await
            .expect("等待伺服器回應逾時")
            .expect("讀取失敗")
            .expect("連線已關閉");
        serde_json::from_str(&line).unwrap()
    }

    /// 收到一則 error，檢查錯誤碼與 fatal
    async fn expect_error(&mut self, code: &str, fatal: bool) -> Value {
        let msg = self.recv().await;
        assert_eq!(msg["type"], "error", "{msg}");
        assert_eq!(msg["code"], code, "{msg}");
        assert_eq!(msg["fatal"], fatal, "{msg}");
        msg
    }

    /// 伺服器送出 close_notify 後正常關閉（不是 RST 或沒有 close_notify 的 EOF）
    async fn expect_clean_close(&mut self) {
        let res = tokio::time::timeout(RECV_TIMEOUT, self.lines.next_line())
            .await
            .expect("等待伺服器關閉連線逾時");
        match res {
            Ok(None) => {}
            other => panic!("預期收到 close_notify 後關閉，實際: {other:?}"),
        }
    }

    /// 送出 hello 並收到 hello_ack，回傳 hello_ack
    async fn register(&mut self, agent_id: &str) -> Value {
        self.send(&hello(1, agent_id, 1000)).await;
        let ack = self.recv().await;
        assert_eq!(ack["type"], "hello_ack", "{ack}");
        ack
    }
}

fn hello(seq: u64, agent_id: &str, heartbeat_interval_ms: i64) -> String {
    json!({
        "type": "hello", "seq": seq, "proto": 1, "agent_id": agent_id, "hostname": "web-01",
        "agent_version": "0.1.0", "heartbeat_interval_ms": heartbeat_interval_ms,
        "ts": "2026-10-09T00:00:00.000Z"
    })
    .to_string()
}

fn heartbeat(seq: u64) -> String {
    json!({"type": "heartbeat", "seq": seq, "ts": "2026-10-09T00:00:00.000Z"}).to_string()
}

fn report(seq: u64) -> String {
    json!({
        "type": "report", "seq": seq, "ts": "2026-10-09T00:00:00.000Z",
        "cpu_usage": 12.5, "total_memory": 1000, "used_memory": 500
    })
    .to_string()
}

#[tokio::test]
async fn hello_state_machine_before_registration() {
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    // hello 前：壞 JSON、未知 type 都是非 fatal，繼續等待 hello
    c.send("not json").await;
    c.expect_error("bad_request", false).await;
    c.send(r#"{"type":"bogus","seq":1}"#).await;
    c.expect_error("unknown_type", false).await;
    // K-S10：hostname 含格式字元 → bad_request（非 fatal）
    let bad_host = hello(1, ID, 1000).replace("web-01", "web\u{202e}10");
    c.send(&bad_host).await;
    c.expect_error("bad_request", false).await;
    // hello 前送 heartbeat → hello_required（fatal）並關閉
    c.send(&heartbeat(2)).await;
    let err = c.expect_error("hello_required", true).await;
    assert_eq!(err["seq"], 2);
    c.expect_clean_close().await;
    assert!(server.agent_ids().is_empty());
}

#[tokio::test]
async fn registered_flow_writes_reports_with_normalized_agent_id() {
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    // K-S3：大寫的 agent_id 以小寫註冊、記錄與寫檔
    let ack = c.register(&ID.to_ascii_uppercase()).await;
    assert_eq!(
        ack,
        json!({"type":"hello_ack","seq":1,"proto":1,"offline_after_ms":5000})
    );
    assert_eq!(server.agent_ids(), [ID]);

    c.send(&heartbeat(2)).await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":2}));
    c.send(&report(3)).await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":3}));
    // ack 時資料已在檔案中
    let text = std::fs::read_to_string(server.reports_file()).unwrap();
    let line: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(line["agent_id"], ID);
    assert_eq!(line["seq"], 3);

    // 第二次 hello → already_registered（非 fatal）
    c.send(&hello(4, ID, 1000)).await;
    c.expect_error("already_registered", false).await;
    // invalid_report（非 fatal）
    c.send(&report(5).replace("12.5", "101")).await;
    c.expect_error("invalid_report", false).await;
    c.send(&heartbeat(6)).await;
    assert_eq!(c.recv().await["seq"], 6);
}

#[tokio::test]
async fn new_connection_supersedes_old_one_regardless_of_case() {
    let server = Server::start(Options::default()).await;
    let mut old = Client::connect(server.addr).await;
    old.register(ID).await;
    // K-S3：同一 agent_id 的大寫形式也會取代舊連線，不會變成兩個 agent
    let mut new = Client::connect(server.addr).await;
    new.register(&ID.to_ascii_uppercase()).await;
    old.expect_error("superseded", true).await;
    old.expect_clean_close().await;
    assert_eq!(server.agent_ids(), [ID]);
    assert_eq!(server.registry.counts(), (1, 1));
    new.send(&heartbeat(2)).await;
    assert_eq!(new.recv().await["type"], "ack");
}

#[tokio::test]
async fn interleaved_unknown_types_count_toward_the_limit() {
    // K-S8：bad_request 與 unknown_type 交錯送出，合計第 5 則改為 fatal
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    c.register(ID).await;
    for i in 0..4 {
        if i % 2 == 0 {
            c.send("{").await;
            c.expect_error("bad_request", false).await;
        } else {
            c.send(r#"{"type":"bogus"}"#).await;
            c.expect_error("unknown_type", false).await;
        }
    }
    c.send(r#"{"type":"bogus"}"#).await;
    c.expect_error("unknown_type", true).await;
    c.expect_clean_close().await;
}

#[tokio::test]
async fn valid_message_resets_the_invalid_message_count() {
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    c.register(ID).await;
    for round in 0..3 {
        for _ in 0..4 {
            c.send(r#"{"type":"bogus"}"#).await;
            c.expect_error("unknown_type", false).await;
        }
        c.send(&heartbeat(10 + round)).await;
        assert_eq!(c.recv().await["type"], "ack");
    }
}

#[tokio::test]
async fn line_length_boundary_with_carriage_return() {
    // T2：65535 bytes + "\r\n" 是一行（內容不是 JSON → bad_request），65536 bytes + "\r\n" → too_long
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    let mut ok = vec![b'a'; 65535];
    ok.extend_from_slice(b"\r\n");
    c.send_raw(&ok).await;
    c.expect_error("bad_request", false).await;

    let mut too_long = vec![b'a'; 65536];
    too_long.extend_from_slice(b"\r\n");
    c.send_raw(&too_long).await;
    c.expect_error("too_long", true).await;
    c.expect_clean_close().await;
}

#[tokio::test]
async fn lenient_hello_parsing() {
    // K-C7：proto 不是合法 u32 → unsupported_version（不是 bad_request）
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    c.send(&hello(1, ID, 1000).replace(r#""proto":1"#, r#""proto":"1""#))
        .await;
    let err = c.expect_error("unsupported_version", true).await;
    assert_eq!(err["seq"], 1);
    c.expect_clean_close().await;

    // heartbeat_interval_ms 為負數或小數 → 夾值，不是錯誤
    let mut c = Client::connect(server.addr).await;
    c.send(&hello(1, ID, -5)).await;
    assert_eq!(c.recv().await["offline_after_ms"], 5000);
    let mut c = Client::connect(server.addr).await;
    c.send(&hello(1, OTHER_ID, 1000).replace("1000", "3000.7"))
        .await;
    assert_eq!(c.recv().await["offline_after_ms"], 15000);
}

#[tokio::test]
async fn hello_timeout_closes_with_close_notify() {
    let server = Server::start(Options {
        timeouts: TimeoutSettings {
            hello_secs: 1,
            ..TimeoutSettings::default()
        },
        ..Options::default()
    })
    .await;
    let mut c = Client::connect(server.addr).await;
    let started = Instant::now();
    c.expect_clean_close().await;
    assert!(started.elapsed() >= Duration::from_millis(900));
}

#[tokio::test]
async fn idle_registered_connection_is_closed_after_offline_threshold() {
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    let started = Instant::now();
    c.register(ID).await;
    // 壞行與未知 type 不延後閒置期限
    c.send(r#"{"type":"bogus"}"#).await;
    c.expect_error("unknown_type", false).await;
    c.expect_clean_close().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(4900) && elapsed < Duration::from_secs(9),
        "{elapsed:?}"
    );
    // 伺服器送出 close_notify 後會等對方關閉（drain）才標記離線
    drop(c);
    let mut offline = false;
    for _ in 0..150 {
        if server.registry.counts() == (0, 1) {
            offline = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(offline, "{:?}", server.registry.counts());
}

#[tokio::test]
async fn shutdown_sends_close_notify_to_all_connections() {
    let server = Server::start(Options::default()).await;
    let mut registered = Client::connect(server.addr).await;
    registered.register(ID).await;
    let mut waiting = Client::connect(server.addr).await;
    // 以一則非 fatal 的無效訊息確認伺服器端已完成握手、正在等待 hello
    waiting.send("{").await;
    waiting.expect_error("bad_request", false).await;
    server.shutdown.trigger();
    registered.expect_clean_close().await;
    waiting.expect_clean_close().await;
}

#[tokio::test]
async fn full_registry_rejects_new_agent_with_fatal_error() {
    // K-S1：狀態表已滿且沒有可淘汰的離線 agent → fatal 的 bad_request
    let server = Server::start(Options {
        max_agents: 1,
        ..Options::default()
    })
    .await;
    let mut a = Client::connect(server.addr).await;
    a.register(ID).await;
    let mut b = Client::connect(server.addr).await;
    b.send(&hello(1, OTHER_ID, 1000)).await;
    let err = b.expect_error("bad_request", true).await;
    assert_eq!(err["seq"], 1);
    assert!(err["message"].as_str().unwrap().contains("full"), "{err}");
    b.expect_clean_close().await;
    assert_eq!(server.agent_ids(), [ID]);

    // a 離線後，新 agent 可淘汰它
    drop(a);
    for _ in 0..100 {
        if server.registry.counts() == (0, 1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut b = Client::connect(server.addr).await;
    b.register(OTHER_ID).await;
    assert_eq!(server.agent_ids(), [OTHER_ID]);
}

#[tokio::test]
async fn reports_beyond_the_rate_limit_are_delayed() {
    // K-S6：超過可連續的筆數後，每秒最多處理 REPORT_RATE_PER_SEC 則
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    c.register(ID).await;
    let extra = 4u64;
    let total = REPORT_BURST as u64 + extra;
    let started = Instant::now();
    for seq in 0..total {
        c.send(&report(10 + seq)).await;
    }
    for seq in 0..total {
        assert_eq!(c.recv().await, json!({"type":"ack","seq":10 + seq}));
    }
    let min = Duration::from_secs_f64(extra as f64 / REPORT_RATE_PER_SEC);
    let elapsed = started.elapsed();
    assert!(
        elapsed >= min - Duration::from_millis(100),
        "{elapsed:?} < {min:?}"
    );
}

#[tokio::test]
async fn mtls_binds_agent_id_to_certificate_case_insensitively() {
    let server = Server::start(Options {
        mtls_crls: Some(vec![test_pki::CRL_EMPTY]),
        ..Options::default()
    })
    .await;
    let good = connector(Some((test_pki::GOOD_CERT, test_pki::GOOD_KEY)));
    // 憑證屬於 GOOD_ID：以別的 agent_id 註冊 → auth_failed
    let mut c = Client::connect_with(&good, server.addr).await;
    c.send(&hello(1, test_pki::REVOKED_ID, 1000)).await;
    let err = c.expect_error("auth_failed", true).await;
    assert_eq!(err["seq"], 1);
    c.expect_clean_close().await;
    assert!(server.agent_ids().is_empty());

    // K-S3：大寫的 agent_id 仍與憑證相符，以小寫註冊
    let mut c = Client::connect_with(&good, server.addr).await;
    c.register(&test_pki::GOOD_ID.to_ascii_uppercase()).await;
    assert_eq!(server.agent_ids(), [test_pki::GOOD_ID]);
}

#[tokio::test]
async fn mtls_disables_session_resumption() {
    // K-S4：恢復的 session 不會重新驗證客戶端憑證，啟用 mTLS 時每次都必須完整握手
    let server = Server::start(Options {
        mtls_crls: Some(vec![test_pki::CRL_EMPTY]),
        ..Options::default()
    })
    .await;
    let good = connector(Some((test_pki::GOOD_CERT, test_pki::GOOD_KEY)));
    for _ in 0..3 {
        let mut c = Client::connect_with(&good, server.addr).await;
        assert_eq!(c.handshake, Some(HandshakeKind::Full));
        // 交換訊息，讓客戶端收下伺服器可能送出的 session ticket
        c.register(test_pki::GOOD_ID).await;
        c.send(&heartbeat(2)).await;
        c.recv().await;
    }

    // 對照組：未啟用 mTLS 時 session resumption 照常運作（證明上面的檢查有效）
    let plain_server = Server::start(Options::default()).await;
    let plain = connector(None);
    let mut first = Client::connect_with(&plain, plain_server.addr).await;
    assert_eq!(first.handshake, Some(HandshakeKind::Full));
    first.register(ID).await;
    first.send(&heartbeat(2)).await;
    first.recv().await;
    let second = Client::connect_with(&plain, plain_server.addr).await;
    assert_eq!(second.handshake, Some(HandshakeKind::Resumed));
}

#[tokio::test]
async fn certificate_revoked_after_handshake_is_rejected_at_hello() {
    // K-S4：握手後、hello 前才撤銷（重載了新的 CRL），hello 時重新驗證並拒絕
    let server = Server::start(Options {
        mtls_crls: Some(vec![test_pki::CRL_EMPTY]),
        ..Options::default()
    })
    .await;
    let revoked = connector(Some((test_pki::REVOKED_CERT, test_pki::REVOKED_KEY)));
    let mut c = Client::connect_with(&revoked, server.addr).await;
    server.reload_crls(&[test_pki::CRL_REVOKE]);
    c.send(&hello(1, test_pki::REVOKED_ID, 1000)).await;
    let err = c.expect_error("certificate_rejected", true).await;
    assert_eq!(err["seq"], 1);
    c.expect_clean_close().await;
    assert!(server.agent_ids().is_empty());

    // 撤銷後連握手都無法完成
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let res = revoked
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await;
    // TLS 1.3 的客戶端憑證在客戶端握手完成後才被伺服器驗證，失敗可能在第一次讀取時才出現
    if let Ok(stream) = res {
        let (r, _w) = tokio::io::split(stream);
        let mut lines = BufReader::new(r).lines();
        let read = tokio::time::timeout(RECV_TIMEOUT, lines.next_line())
            .await
            .unwrap();
        assert!(read.is_err(), "撤銷的憑證不應能建立連線: {read:?}");
    }
}

#[tokio::test]
async fn revoked_certificate_cannot_handshake() {
    let server = Server::start(Options {
        mtls_crls: Some(vec![test_pki::CRL_REVOKE]),
        ..Options::default()
    })
    .await;
    let revoked = connector(Some((test_pki::REVOKED_CERT, test_pki::REVOKED_KEY)));
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let res = revoked
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await;
    let err = match res {
        Err(e) => e,
        Ok(stream) => {
            let (r, _w) = tokio::io::split(stream);
            let mut lines = BufReader::new(r).lines();
            tokio::time::timeout(RECV_TIMEOUT, lines.next_line())
                .await
                .unwrap()
                .unwrap_err()
        }
    };
    // 伺服器以 alert 拒絕（certificate_revoked）
    let text = err.to_string().to_lowercase();
    assert!(text.contains("revoked") || text.contains("alert"), "{text}");
    // 未撤銷的憑證照常可用
    let good = connector(Some((test_pki::GOOD_CERT, test_pki::GOOD_KEY)));
    let mut c = Client::connect_with(&good, server.addr).await;
    c.register(test_pki::GOOD_ID).await;
}

#[tokio::test]
async fn unknown_write_result_closes_without_reply() {
    // K-C1：writer 已開始寫入卻卡住 → 不能回 ack 也不能回 storage_unavailable，
    // 伺服器不回應，送出 close_notify 後關閉連線
    let (sink, mut rx) = report_channel(16);
    let stuck = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Some(req) = rx.recv().await {
            assert!(req.ticket.claim());
            held.push(req);
        }
    });
    let server = Server::start(Options {
        timeouts: TimeoutSettings {
            write_secs: 1,
            ..TimeoutSettings::default()
        },
        reports: Some(sink),
        ..Options::default()
    })
    .await;
    let mut c = Client::connect(server.addr).await;
    c.register(ID).await;
    let started = Instant::now();
    c.send(&report(2)).await;
    c.expect_clean_close().await;
    let elapsed = started.elapsed();
    // write_secs 1 秒 + 寬限 min(1 秒, write_secs) = 約 2 秒
    assert!(
        elapsed >= Duration::from_millis(1900) && elapsed < Duration::from_secs(8),
        "{elapsed:?}"
    );
    stuck.abort();
}

#[tokio::test]
async fn per_ip_limit_is_enforced_by_the_accept_loop() {
    // K-S2：同一來源 IP 超過上限的連線在 TLS 握手前就被關閉
    let server = Server::start(Options {
        max_connections_per_ip: 2,
        ..Options::default()
    })
    .await;
    let mut a = Client::connect(server.addr).await;
    let mut b = Client::connect(server.addr).await;
    // 確認兩條都已被伺服器接受並完成握手
    for c in [&mut a, &mut b] {
        c.send("{").await;
        c.expect_error("bad_request", false).await;
    }

    // 第 3 條：TCP 可以建立，但伺服器不進行握手直接關閉
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let third = tokio::time::timeout(
        RECV_TIMEOUT,
        connector(None).connect(ServerName::try_from("localhost").unwrap(), tcp),
    )
    .await
    .expect("伺服器應立即關閉超過上限的連線");
    assert!(third.is_err(), "超過每 IP 上限的連線不應完成握手");

    // 釋放一條後（伺服器端連線結束、歸還名額），新連線可以建立
    drop(a);
    let mut admitted = None;
    for _ in 0..100 {
        let tcp = TcpStream::connect(server.addr).await.unwrap();
        if let Ok(Ok(tls)) = tokio::time::timeout(
            RECV_TIMEOUT,
            connector(None).connect(ServerName::try_from("localhost").unwrap(), tcp),
        )
        .await
        {
            admitted = Some(tls);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(admitted.is_some(), "名額歸還後應可再建立連線");
    b.send(&heartbeat(1)).await;
    b.expect_error("hello_required", true).await;
}
