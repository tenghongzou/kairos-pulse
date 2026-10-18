//! 透過真實 TLS 連線測試 `handle_connection`（T1）：hello 狀態機、fatal 後關閉、superseded、
//! 閒置逾時、close_notify，以及 K-S1／K-S3／K-S4／K-S6／K-S8／K-S10／K-C7 的端對端回歸測試。
//!
//! 伺服器在 127.0.0.1 的隨機埠上執行，客戶端使用 tokio-rustls；伺服器憑證沿用 `testdata/pki`，
//! mTLS 的客戶端憑證與 CA 見 [`crate::test_pki`]。

use chrono::Utc;
use serde_json::{Value, json};
use std::net::{IpAddr, SocketAddr};
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
use crate::connection::ConnContext;
use crate::connection::{MESSAGE_BURST, MESSAGES_PER_SEC};
use crate::per_ip::cidr::Allowlist;
use crate::per_ip::{LimiterSnapshot, PerIpLimiter, PerIpPolicy};
use crate::rate::{REPORT_INITIAL_TOKENS, REPORT_RATE_PER_SEC, ReportLimiter};
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
    max_connections: usize,
    max_connections_per_ip: usize,
    /// `server.per_ip_allowlist`
    per_ip_allowlist: Vec<&'static str>,
    /// 允許清單內每個位址的實際上限
    max_connections_per_allowlisted_ip: usize,
    timeouts: TimeoutSettings,
    /// 取代真正的 report writer（例如模擬卡住的磁碟）
    reports: Option<ReportSink>,
    /// 日誌節流的時間窗
    throttle_interval: Duration,
    /// 以 SQLite 後端保存（`<dir>/kairos.db`；宣告 `inventory`、`processes`）。預設為 JSONL
    sqlite: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mtls_crls: None,
            max_agents: 100,
            max_connections: 64,
            max_connections_per_ip: 32,
            per_ip_allowlist: Vec::new(),
            max_connections_per_allowlisted_ip: 64,
            timeouts: TimeoutSettings::default(),
            reports: None,
            throttle_interval: crate::throttle::DEFAULT_INTERVAL,
            sqlite: false,
        }
    }
}

struct Server {
    addr: SocketAddr,
    dir: PathBuf,
    registry: Arc<Registry>,
    tls: Arc<TlsState>,
    shutdown: Arc<Shutdown>,
    throttle: Arc<LogThrottle>,
    per_ip: Arc<PerIpLimiter>,
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
        let (reports, assets) = match opts.reports {
            Some(sink) => (sink, None),
            None if opts.sqlite => {
                let o = crate::store::sqlite::SqliteOptions {
                    path: dir.join(crate::store::sqlite::DB_FILE),
                    synchronous: crate::config::SqliteSynchronous::Normal,
                    raw_retention_days: 7,
                    rollup_5m_retention_days: 31,
                    rollup_1h_retention_days: 400,
                    max_total_bytes: 0,
                    max_hosts: 1000,
                    max_series_per_host: crate::config::DEFAULT_MAX_SERIES_PER_HOST,
                    mtls: false,
                };
                let (store, _) = crate::store::sqlite::SqliteStore::open(o, Utc::now).unwrap();
                let (sink, rx) = report_channel(1024);
                let (asink, arx) = crate::store::asset_channel(16);
                let (_feed, inventory_rx) = tokio::sync::watch::channel(None);
                tokio::spawn(crate::store::run_store_writer_with_assets(
                    store,
                    rx,
                    Some(arx),
                    inventory_rx,
                    None,
                ));
                (sink, Some(asink))
            }
            None => {
                let (sink, writer) = start_report_writer(
                    dir.join("data"),
                    30,
                    0,
                    Utc::now,
                    RETENTION_SWEEP_INTERVAL,
                )
                .await
                .unwrap();
                tokio::spawn(writer);
                (sink, None)
            }
        };
        let registry = Arc::new(Registry::with_max_agents(opts.max_agents));
        let shutdown = Arc::new(Shutdown::new());
        let throttle = Arc::new(LogThrottle::new(opts.throttle_interval));
        let policy = PerIpPolicy {
            max_per_ip: opts.max_connections_per_ip,
            allowlist: Arc::new(Allowlist::parse(&opts.per_ip_allowlist).unwrap()),
            max_per_allowlisted_ip: opts.max_connections_per_allowlisted_ip,
            mtls: opts.mtls_crls.is_some(),
        };
        let ctx = Arc::new(ConnContext {
            registry: registry.clone(),
            reports,
            timeouts: opts.timeouts,
            shutdown: shutdown.clone(),
            tls: tls.clone(),
            throttle: throttle.clone(),
            binding: BindingPolicy::default(),
            report_limiter: Arc::new(ReportLimiter::with_policy(policy.clone())),
            enroll: None,
            assets,
            cluster: crate::connection::ClusterContext::new("kairos-test"),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // 與正式程式相同的 accept 迴圈（含全域與每 IP 的連線上限）
        let per_ip = PerIpLimiter::with_policy(policy);
        let max_connections = opts.max_connections;
        let accept_per_ip = per_ip.clone();
        let accept = tokio::spawn(async move {
            let per_ip = accept_per_ip;
            let mut connections = accept_loop(listener, ctx, max_connections, per_ip)
                .await
                .unwrap();
            // 關機後等待連線送出 close_notify 並結束（JoinSet 被釋放時會中止仍在執行的連線）
            while connections.join_next().await.is_some() {}
        });
        Self {
            addr,
            dir,
            registry,
            tls,
            shutdown,
            throttle,
            per_ip,
            accept,
        }
    }

    /// 等待輸出的節流日誌中出現符合 `pred` 的 `count` 行（連線收尾在背景進行，日誌可能稍晚出現），
    /// 回傳目前為止所有輸出的節流日誌
    async fn wait_for_logs(&self, count: usize, pred: impl Fn(&str) -> bool) -> Vec<String> {
        let deadline = Instant::now() + RECV_TIMEOUT;
        let mut all = Vec::new();
        loop {
            all.extend(self.throttle.take_emitted());
            if all.iter().filter(|l| pred(l)).count() >= count || Instant::now() >= deadline {
                return all;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// 在 `dir` 寫出 mTLS 用的 CA 與 CRL 檔，回傳 TLS 設定檔路徑
    fn tls_paths(dir: &std::path::Path, crls: Option<&[&str]>) -> TlsPaths {
        let s = |p: PathBuf| p.to_string_lossy().to_string();
        let (client_ca, crl_paths) = match crls {
            None => (None, Vec::new()),
            Some(crls) => {
                let ca = dir.join("client-ca.crt");
                // 另含 NAT 情境用的測試 CA（testdata/pki/nat-ca.crt，簽發 nat-clients.bundle 的 40 個身分）
                let nat_ca = std::fs::read_to_string(format!("{PKI}nat-ca.crt")).unwrap();
                std::fs::write(&ca, format!("{}{}", test_pki::CA, nat_ca)).unwrap();
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
            issuing_cas: None,
            issuing_crl: None,
            retired_cas: None,
        }
    }

    /// 熱重載：改用新的 CRL（PEM 內容）
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
        Self::try_connect_with(connector, addr).await.unwrap()
    }

    /// 連線與 TLS 握手；失敗時回傳錯誤（伺服器拒絕超過上限的連線時，Linux 多半是 EOF、
    /// Windows 則常是 RST（os error 10054），呼叫端重試即可）
    async fn try_connect_with(connector: &TlsConnector, addr: SocketAddr) -> std::io::Result<Self> {
        let tcp = TcpStream::connect(addr).await?;
        let tls = connector
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await?;
        let handshake = tls.get_ref().1.handshake_kind();
        let (r, w) = tokio::io::split(tls);
        Ok(Self {
            lines: BufReader::new(r).lines(),
            writer: w,
            handshake,
        })
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
    let err = c.expect_error("bad_request", false).await;
    // F5：hello 的 JSON 合法、只有欄位驗證失敗時，回應帶回 hello 的 seq
    assert_eq!(err["seq"], 1, "{err}");
    // hello 前送 heartbeat → hello_required（fatal）並關閉
    c.send(&heartbeat(2)).await;
    let err = c.expect_error("hello_required", true).await;
    assert_eq!(err["seq"], 2);
    c.expect_clean_close().await;
    assert!(server.agent_ids().is_empty());
}

#[tokio::test]
async fn invalid_hello_fields_echo_the_seq() {
    // F5：hello 能取得 seq 但欄位驗證失敗時，bad_request 帶回該 seq（非 fatal）
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    // agent_id 格式錯誤
    c.send(&hello(5, "not-a-uuid", 1000)).await;
    assert_eq!(c.expect_error("bad_request", false).await["seq"], 5);
    // hostname 過長
    c.send(&hello(6, ID, 1000).replace("web-01", &"h".repeat(256)))
        .await;
    assert_eq!(c.expect_error("bad_request", false).await["seq"], 6);
    // hostname 型別錯誤（無法解析為 hello，但仍能取得 seq）
    c.send(&hello(7, ID, 1000).replace(r#""web-01""#, "42"))
        .await;
    assert_eq!(c.expect_error("bad_request", false).await["seq"], 7);
    // 壞 JSON：無法取得 seq
    c.send("{").await;
    assert_eq!(
        c.expect_error("bad_request", false).await["seq"],
        Value::Null
    );
    // 之後仍可正常註冊，hello_ack 帶回該則 hello 的 seq
    c.send(&hello(8, ID, 1000)).await;
    let ack = c.recv().await;
    assert_eq!(ack["type"], "hello_ack", "{ack}");
    assert_eq!(ack["seq"], 8);
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
async fn cluster_feature_is_listed_only_when_the_client_declares_it() {
    // PROTOCOL.md「叢集與排空」：0.8 以前的 Pulse（不宣告 cluster）收到的 hello_ack 不變
    let server = Server::start(Options::default()).await;
    let mut old = Client::connect(server.addr).await;
    old.send(&hello_v2(1, ID, &["metrics", "durable"]).to_string())
        .await;
    let ack = old.recv().await;
    assert_eq!(ack["features"], json!(["metrics", "durable"]), "{ack}");
    assert!(ack.get("server_node").is_none(), "{ack}");
    assert!(ack.get("cluster_id").is_none(), "{ack}");

    let mut new = Client::connect(server.addr).await;
    new.send(&hello_v2(1, OTHER_ID, &["metrics", "durable", "cluster"]).to_string())
        .await;
    let ack = new.recv().await;
    assert_eq!(
        ack["features"],
        json!(["metrics", "durable", "cluster"]),
        "{ack}"
    );
    assert_eq!(ack["server_node"], "kairos-test", "{ack}");
    // 單機的 Kairos 沒有 cluster_id
    assert!(ack.get("cluster_id").is_none(), "{ack}");

    // 協商為 v1 時不使用 features（宣告了也一樣）
    let mut v1 = Client::connect(server.addr).await;
    let mut h = hello_v2(1, "bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee", &["cluster"]);
    h["proto_max"] = json!(1);
    v1.send(&h.to_string()).await;
    let ack = v1.recv().await;
    assert_eq!(
        ack,
        json!({"type":"hello_ack","seq":1,"proto":1,"offline_after_ms":5000})
    );
}

#[tokio::test]
async fn shutdown_drains_only_connections_that_negotiated_cluster() {
    let server = Server::start(Options::default()).await;
    let mut old = Client::connect(server.addr).await;
    old.send(&hello_v2(1, ID, &["metrics", "durable"]).to_string())
        .await;
    assert_eq!(old.recv().await["type"], "hello_ack");
    let mut new = Client::connect(server.addr).await;
    new.send(&hello_v2(1, OTHER_ID, &["metrics", "durable", "cluster"]).to_string())
        .await;
    assert_eq!(new.recv().await["type"], "hello_ack");
    server.shutdown.trigger();
    // 協商了 cluster：先收到 drain（seq 為 null，分散時間至少 10 秒），再收到 close_notify
    let drain = new.recv().await;
    assert_eq!(
        drain,
        json!({"type":"drain","seq":null,"reason":"node_draining","reconnect_within_ms":10000})
    );
    new.expect_clean_close().await;
    // 沒有協商的連線與 0.8 相同：直接 close_notify
    old.expect_clean_close().await;
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
    // （R2-K3：新連線只有 REPORT_INITIAL_TOKENS 個額度）
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    c.register(ID).await;
    let extra = 4u64;
    let total = REPORT_INITIAL_TOKENS as u64 + extra;
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
async fn reconnecting_does_not_get_a_fresh_report_allowance() {
    // R2-K3：未啟用 mTLS 時以來源 IP 跨連線限速；重新連線不會取得新的（滿的）額度
    let server = Server::start(Options {
        max_connections_per_ip: 1,
        ..Options::default()
    })
    .await;
    let mut c = Client::connect(server.addr).await;
    c.register(ID).await;
    // 用完來源 IP 的初始額度（2 則）
    for seq in 0..REPORT_INITIAL_TOKENS as u64 {
        c.send(&report(10 + seq)).await;
        assert_eq!(c.recv().await["type"], "ack");
    }
    drop(c);
    // 等伺服器端釋放名額（每 IP 上限 1）後重新連線
    let mut c = None;
    let tls = connector(None);
    for _ in 0..100 {
        // 名額尚未釋放時伺服器直接關閉連線：握手或寫入失敗都視為這一次不成功，稍後重試
        if let Ok(mut candidate) = Client::try_connect_with(&tls, server.addr).await
            && candidate
                .writer
                .write_all(format!("{}\n", hello(1, ID, 1000)).as_bytes())
                .await
                .is_ok()
            && candidate.writer.flush().await.is_ok()
            && let Ok(Ok(Some(line))) =
                tokio::time::timeout(Duration::from_secs(2), candidate.lines.next_line()).await
            && line.contains("hello_ack")
        {
            c = Some(candidate);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut c = c.expect("重新連線應成功");
    // 新連線自己的 bucket 有 2 個額度，但來源 IP 的額度已用完：兩則都需等待（約 0.5 + 0.5 秒）
    let started = Instant::now();
    c.send(&report(20)).await;
    c.send(&report(21)).await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":20}));
    assert_eq!(c.recv().await, json!({"type":"ack","seq":21}));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(600), "{elapsed:?}");
}

#[tokio::test]
async fn message_rate_is_limited_per_connection() {
    // R2-K12：超過可連續的則數後，每秒最多處理 MESSAGES_PER_SEC 則（延後，不回錯誤）
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    c.register(ID).await;
    let extra = 10u64;
    let total = MESSAGE_BURST as u64 + extra;
    let mut batch = String::new();
    for seq in 0..total {
        batch.push_str(&heartbeat(100 + seq));
        batch.push('\n');
    }
    let started = Instant::now();
    c.send_raw(batch.as_bytes()).await;
    for seq in 0..total {
        assert_eq!(c.recv().await, json!({"type":"ack","seq":100 + seq}));
    }
    // hello 也算一則：至少 (extra - 1) / 速率
    let min = Duration::from_secs_f64((extra - 1) as f64 / MESSAGES_PER_SEC);
    let elapsed = started.elapsed();
    assert!(
        elapsed >= min - Duration::from_millis(50),
        "{elapsed:?} < {min:?}"
    );
}

#[tokio::test]
async fn handshake_slots_are_limited_and_reserved_for_registered_connections() {
    // R2-K2：握手中（尚未通過 hello）的連線最多佔 max_connections 的一半
    let server = Server::start(Options {
        max_connections: 4,
        ..Options::default()
    })
    .await;
    // 已通過 hello 的連線不佔握手名額
    let mut registered = Client::connect(server.addr).await;
    registered.register(ID).await;
    // 兩條只建立 TCP、不進行握手的連線佔滿握手名額（4 / 2 = 2）
    let stalled: Vec<TcpStream> = vec![
        TcpStream::connect(server.addr).await.unwrap(),
        TcpStream::connect(server.addr).await.unwrap(),
    ];
    // 全域名額仍有剩餘，但新連線在握手前就被關閉
    let mut rejected = false;
    for _ in 0..50 {
        let tcp = TcpStream::connect(server.addr).await.unwrap();
        match tokio::time::timeout(
            RECV_TIMEOUT,
            connector(None).connect(ServerName::try_from("localhost").unwrap(), tcp),
        )
        .await
        .expect("伺服器應立即關閉超過握手上限的連線")
        {
            Err(_) => {
                rejected = true;
                break;
            }
            // 停滯的連線可能尚未被 accept：稍後再試
            Ok(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    assert!(rejected, "握手名額用盡時新連線不應完成握手");
    // 因全域上限被拒（握手中上限）的次數；不計入每 IP 上限
    let snap = server.per_ip.snapshot(Instant::now());
    assert!(
        snap.rejections.global.handshake_limit_rejected >= 1,
        "{snap:?}"
    );
    assert_eq!(snap.rejections.per_ip.total, 0);
    // 已註冊的連線不受影響
    registered.send(&heartbeat(2)).await;
    assert_eq!(registered.recv().await["type"], "ack");
    // 停滯的連線關閉後可再建立新連線
    drop(stalled);
    let mut admitted = false;
    for _ in 0..100 {
        let tcp = TcpStream::connect(server.addr).await.unwrap();
        if let Ok(Ok(_)) = tokio::time::timeout(
            RECV_TIMEOUT,
            connector(None).connect(ServerName::try_from("localhost").unwrap(), tcp),
        )
        .await
        {
            admitted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(admitted);
}

#[tokio::test]
async fn unknown_revocation_status_is_reported_with_a_distinct_message() {
    // R2-K26：CRL 無法判定撤銷狀態（此處換成另一個 CA 的 CRL；CRL 過期亦同）時，
    // certificate_rejected 的 message 為 "revocation status unavailable"
    let server = Server::start(Options {
        mtls_crls: Some(vec![test_pki::CRL_EMPTY]),
        ..Options::default()
    })
    .await;
    let good = connector(Some((test_pki::GOOD_CERT, test_pki::GOOD_KEY)));
    let mut c = Client::connect_with(&good, server.addr).await;
    let other_ca_crl = std::fs::read_to_string(format!("{PKI}crl-empty.crl")).unwrap();
    server.reload_crls(&[&other_ca_crl]);
    c.send(&hello(1, test_pki::GOOD_ID, 1000)).await;
    let err = c.expect_error("certificate_rejected", true).await;
    assert_eq!(err["message"], "revocation status unavailable");
    c.expect_clean_close().await;

    // 撤銷的憑證維持原本的 message
    server.reload_crls(&[test_pki::CRL_EMPTY]);
    let revoked = connector(Some((test_pki::REVOKED_CERT, test_pki::REVOKED_KEY)));
    let mut c = Client::connect_with(&revoked, server.addr).await;
    server.reload_crls(&[test_pki::CRL_REVOKE]);
    c.send(&hello(1, test_pki::REVOKED_ID, 1000)).await;
    let err = c.expect_error("certificate_rejected", true).await;
    assert_eq!(
        err["message"],
        "client certificate has been revoked or is no longer trusted"
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

/// 含有 `agent_id` 與 `needle` 的日誌行數
fn count_lines(lines: &[String], agent_id: &str, needle: &str) -> usize {
    lines
        .iter()
        .filter(|l| l.contains(&format!("agent {agent_id}")) && l.contains(needle))
        .count()
}

#[tokio::test]
async fn mtls_agent_events_behind_one_ip_are_logged_per_agent() {
    // F1：啟用 mTLS 時，同一來源 IP 的多個 agent 的上線、取代、被取代、關機離線都逐筆記錄
    let server = Server::start(Options {
        mtls_crls: Some(vec![test_pki::CRL_EMPTY]),
        ..Options::default()
    })
    .await;
    // 空 CRL 下兩張憑證都有效：兩個不同的 agent，都從 127.0.0.1 連入
    let ids = [test_pki::GOOD_ID, test_pki::REVOKED_ID];
    let a = connector(Some((test_pki::GOOD_CERT, test_pki::GOOD_KEY)));
    let b = connector(Some((test_pki::REVOKED_CERT, test_pki::REVOKED_KEY)));
    let mut old_a = Client::connect_with(&a, server.addr).await;
    old_a.register(ids[0]).await;
    let mut old_b = Client::connect_with(&b, server.addr).await;
    old_b.register(ids[1]).await;
    let logs = server.wait_for_logs(2, |l| l.contains("）上線，")).await;
    for id in ids {
        assert_eq!(count_lines(&logs, id, "）上線，"), 1, "{id}: {logs:#?}");
    }

    // 兩個 agent 幾乎同時重新連線（互踢）：取代與被取代各自記錄
    let mut new_a = Client::connect_with(&a, server.addr).await;
    new_a.register(ids[0]).await;
    let mut new_b = Client::connect_with(&b, server.addr).await;
    new_b.register(ids[1]).await;
    old_a.expect_error("superseded", true).await;
    old_b.expect_error("superseded", true).await;
    let logs = server
        .wait_for_logs(4, |l| {
            l.contains("取代舊連線") || l.contains("已由新連線取代")
        })
        .await;
    for id in ids {
        assert_eq!(count_lines(&logs, id, "取代舊連線"), 1, "{id}: {logs:#?}");
        assert_eq!(
            count_lines(&logs, id, "已由新連線取代"),
            1,
            "{id}: {logs:#?}"
        );
    }

    // 關機：每條連線都記錄「連線關閉，標記為離線」
    server.shutdown.trigger();
    new_a.expect_clean_close().await;
    new_b.expect_clean_close().await;
    let logs = server
        .wait_for_logs(2, |l| l.contains("連線關閉，標記為離線"))
        .await;
    for id in ids {
        assert_eq!(
            count_lines(&logs, id, "連線關閉，標記為離線"),
            1,
            "{id}: {logs:#?}"
        );
    }
    // 沒有任何一則被略過
    assert!(logs.iter().all(|l| !l.contains("略過")), "{logs:#?}");
}

#[tokio::test]
async fn unauthenticated_agent_events_are_throttled_by_ip_and_list_skipped_agents() {
    // F1：未啟用 mTLS 時仍以來源 IP 節流，但下一則附註列出被略過的 agent_id
    let third = "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff";
    let server = Server::start(Options {
        throttle_interval: Duration::from_millis(300),
        ..Options::default()
    })
    .await;
    let mut a = Client::connect(server.addr).await;
    a.register(ID).await;
    let mut b = Client::connect(server.addr).await;
    b.register(OTHER_ID).await;
    let logs = server.wait_for_logs(1, |l| l.contains("）上線，")).await;
    assert_eq!(count_lines(&logs, ID, "）上線，"), 1, "{logs:#?}");
    assert_eq!(count_lines(&logs, OTHER_ID, "）上線，"), 0, "{logs:#?}");

    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut c = Client::connect(server.addr).await;
    c.register(third).await;
    let logs = server.wait_for_logs(1, |l| l.contains("）上線，")).await;
    let line = logs
        .iter()
        .find(|l| l.contains(&format!("agent {third}")))
        .unwrap_or_else(|| panic!("{logs:#?}"));
    assert!(
        line.ends_with(&format!(
            "（上次記錄後另有 1 則略過，涉及 agent：{OTHER_ID}）"
        )),
        "{line}"
    );
}

/// `testdata/pki/nat-clients.bundle` 的 (agent_id, 憑證 PEM, 私鑰 PEM)：40 個由 NAT 測試 CA 簽發的身分
fn nat_identities() -> Vec<(String, String, String)> {
    let bundle = std::fs::read_to_string(format!("{PKI}nat-clients.bundle")).unwrap();
    let end = "-----END PRIVATE KEY-----";
    let ids: Vec<_> = bundle
        .split_inclusive(end)
        .filter(|chunk| chunk.contains(end))
        .enumerate()
        .map(|(i, chunk)| {
            let at = chunk.find("-----BEGIN PRIVATE KEY-----").unwrap();
            (
                format!("4e470000-0000-4000-8000-{:012x}", i + 1),
                chunk[..at].trim().to_string(),
                chunk[at..].trim().to_string(),
            )
        })
        .collect();
    assert_eq!(ids.len(), 40);
    ids
}

/// 嘗試建立 TLS 連線直到被伺服器在握手前關閉（停滯的連線可能尚未被 accept，稍後再試）；回傳是否被拒
async fn rejected_before_handshake(addr: SocketAddr, connector: &TlsConnector) -> bool {
    for _ in 0..50 {
        let tcp = TcpStream::connect(addr).await.unwrap();
        match tokio::time::timeout(
            RECV_TIMEOUT,
            connector.connect(ServerName::try_from("localhost").unwrap(), tcp),
        )
        .await
        .expect("伺服器應立即關閉超過上限的連線")
        {
            Err(_) => return true,
            Ok(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    false
}

/// 重試建立 TLS 連線直到成功（伺服器端歸還名額在背景進行）
async fn connect_when_admitted(addr: SocketAddr, connector: &TlsConnector) -> Client {
    for _ in 0..100 {
        let tcp = TcpStream::connect(addr).await.unwrap();
        if let Ok(Ok(tls)) = tokio::time::timeout(
            RECV_TIMEOUT,
            connector.connect(ServerName::try_from("localhost").unwrap(), tcp),
        )
        .await
        {
            let handshake = tls.get_ref().1.handshake_kind();
            let (r, w) = tokio::io::split(tls);
            return Client {
                lines: BufReader::new(r).lines(),
                writer: w,
                handshake,
            };
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("名額歸還後應可再建立連線");
}

/// 等待 `pred` 對伺服器的計數快照成立，回傳最後一次的快照
async fn wait_for_snapshot(
    server: &Server,
    pred: impl Fn(&LimiterSnapshot) -> bool,
) -> LimiterSnapshot {
    let deadline = Instant::now() + RECV_TIMEOUT;
    loop {
        let snap = server.per_ip.snapshot(Instant::now());
        if pred(&snap) || Instant::now() >= deadline {
            return snap;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

const LOCALHOST: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

#[tokio::test]
async fn mtls_counts_by_identity_so_more_than_32_agents_behind_one_nat_can_connect() {
    // A7 根本解：同一個 NAT（此處為 127.0.0.1）後 40 台各自持有憑證的 agent，每 IP 上限維持預設 32，
    // 全部都能連上；通過 hello 後依憑證身分計數，不佔來源位址的名額
    let server = Server::start(Options {
        mtls_crls: Some(Vec::new()),
        max_connections: 128,
        max_agents: 128,
        ..Options::default()
    })
    .await;
    let identities = nat_identities();
    let mut clients = Vec::new();
    for (id, cert, key) in &identities {
        let mut c = Client::connect_with(&connector(Some((cert, key))), server.addr).await;
        c.register(id).await;
        clients.push(c);
    }
    assert_eq!(server.agent_ids().len(), 40);
    let snap = wait_for_snapshot(&server, |s| s.counted_connections == 0).await;
    assert_eq!(snap.counted_connections, 0, "{snap:?}");
    assert_eq!((snap.identities, snap.identity_connections), (40, 40));
    assert_eq!(snap.rejections.per_ip.total, 0);

    // 同一身分再連線：與既有的 superseded 行為一致，新連線取代舊連線，身分數不變
    let (id, cert, key) = &identities[0];
    let mut again = Client::connect_with(&connector(Some((cert, key))), server.addr).await;
    again.register(id).await;
    clients[0].expect_error("superseded", true).await;
    clients[0].expect_clean_close().await;
    let snap = wait_for_snapshot(&server, |s| s.identity_connections == 40).await;
    assert_eq!((snap.identities, snap.identity_connections), (40, 40));
    assert_eq!(server.agent_ids().len(), 40);
    for c in clients.iter_mut().skip(1) {
        c.send(&heartbeat(2)).await;
        assert_eq!(c.recv().await["type"], "ack");
    }
}

#[tokio::test]
async fn mtls_still_limits_connections_per_ip_before_hello() {
    // K-S2：mTLS 時每 IP 上限仍保護握手前與握手中的階段（慢速握手）；已通過 hello 的連線不受影響
    let server = Server::start(Options {
        mtls_crls: Some(Vec::new()),
        max_connections: 64,
        max_connections_per_ip: 4,
        ..Options::default()
    })
    .await;
    let identities = nat_identities();
    let mut registered = Vec::new();
    for (id, cert, key) in identities.iter().take(6) {
        let mut c = Client::connect_with(&connector(Some((cert, key))), server.addr).await;
        c.register(id).await;
        registered.push(c);
    }
    // 4 條只建立 TCP、不握手的連線佔滿該位址握手中的名額（全域握手中上限 32 仍有剩餘）
    let mut stalled = Vec::new();
    for _ in 0..4 {
        stalled.push(TcpStream::connect(server.addr).await.unwrap());
    }
    let (_, cert, key) = &identities[10];
    assert!(rejected_before_handshake(server.addr, &connector(Some((cert, key)))).await);
    let snap = server.per_ip.snapshot(Instant::now());
    let rejected = &snap.rejections.per_ip;
    assert!(rejected.total >= 1, "{snap:?}");
    assert_eq!(rejected.sources[0].source, LOCALHOST);
    assert_eq!(
        (rejected.sources[0].limit, rejected.sources[0].allowlisted),
        (4, false)
    );
    assert_eq!(snap.rejections.global.handshake_limit_rejected, 0);
    let needle = "來自 127.0.0.1 的握手中（尚未通過 hello）的連線數已達上限 4（server.max_connections_per_ip";
    let logs = server.wait_for_logs(1, |l| l.contains(needle)).await;
    assert!(logs.iter().any(|l| l.contains(needle)), "{logs:#?}");
    // 已通過 hello 的 6 條連線（超過每 IP 上限 4）不受影響
    for c in &mut registered {
        c.send(&heartbeat(2)).await;
        assert_eq!(c.recv().await["type"], "ack");
    }
    // 停滯的連線關閉後，新的 agent 可以連上
    drop(stalled);
    let (id, cert, key) = &identities[11];
    let mut c = connect_when_admitted(server.addr, &connector(Some((cert, key)))).await;
    c.register(id).await;
}

#[tokio::test]
async fn allowlisted_address_uses_the_allowlisted_limit_without_mtls() {
    // A7 過渡方案：允許清單內的位址改用允許清單的上限；被拒時依來源計數並記錄對應的設定
    let server = Server::start(Options {
        max_connections_per_ip: 2,
        per_ip_allowlist: vec!["127.0.0.0/8"],
        max_connections_per_allowlisted_ip: 5,
        ..Options::default()
    })
    .await;
    let mut clients = Vec::new();
    for i in 0..5 {
        let mut c = Client::connect(server.addr).await;
        c.register(&format!("4e470000-0000-4000-8000-{:012x}", 100 + i))
            .await;
        clients.push(c);
    }
    // 未啟用 mTLS：已註冊的連線仍依來源位址計數
    let snap = server.per_ip.snapshot(Instant::now());
    assert_eq!((snap.counted_connections, snap.identities), (5, 0));
    assert!(rejected_before_handshake(server.addr, &connector(None)).await);
    let snap = server.per_ip.snapshot(Instant::now());
    let source = snap.rejections.per_ip.sources[0];
    assert_eq!(source.source, LOCALHOST);
    assert_eq!((source.limit, source.allowlisted), (5, true));
    let needle = "來自 127.0.0.1 的同時連線數已達上限 5（此位址在 server.per_ip_allowlist 中";
    let logs = server.wait_for_logs(1, |l| l.contains(needle)).await;
    assert!(logs.iter().any(|l| l.contains(needle)), "{logs:#?}");
    // 名額歸還後可再連線
    drop(clients.pop());
    let mut c = connect_when_admitted(server.addr, &connector(None)).await;
    c.register(OTHER_ID).await;
}

#[tokio::test]
async fn global_limit_pause_time_is_counted() {
    // D15：因全域上限被拒（KRS-W2204）：暫停接受新連線的次數與累計時間
    let server = Server::start(Options {
        max_connections: 2,
        ..Options::default()
    })
    .await;
    let mut a = Client::connect(server.addr).await;
    a.register(ID).await;
    // 第 2 條停在握手前：用完全域名額，accept 迴圈暫停
    let stalled = TcpStream::connect(server.addr).await.unwrap();
    let snap = wait_for_snapshot(&server, |s| s.rejections.global.paused_now).await;
    assert!(snap.rejections.global.paused_now, "{snap:?}");
    assert_eq!(snap.rejections.global.pauses, 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    // 釋放後恢復，累計時間包含整段暫停
    drop(stalled);
    let snap = wait_for_snapshot(&server, |s| !s.rejections.global.paused_now).await;
    assert!(!snap.rejections.global.paused_now, "{snap:?}");
    assert!(
        snap.rejections.global.paused >= Duration::from_millis(50),
        "{snap:?}"
    );
    assert_eq!(snap.rejections.per_ip.total, 0);
    a.send(&heartbeat(2)).await;
    assert_eq!(a.recv().await["type"], "ack");
}

// ---- 協定 v2 與相容矩陣（A3、A4、O6） ----

/// 新版 Pulse 的 hello：proto 1 + 範圍 1～2 + features 與主機資訊
fn hello_v2(seq: u64, agent_id: &str, features: &[&str]) -> Value {
    json!({
        "type": "hello", "seq": seq, "proto": 1, "proto_min": 1, "proto_max": 2,
        "features": features, "agent_id": agent_id, "hostname": "web-01",
        "agent_version": "0.2.0", "heartbeat_interval_ms": 1000,
        "ts": "2026-10-09T00:00:00.000Z",
        "host_fingerprint": format!("sha256:{}", "a".repeat(64)),
        "display_name": "Web 01", "tags": ["env:prod"], "report_interval_ms": 5000,
        "os": {"name": "Ubuntu", "version": "24.04", "kernel": "6.8.0", "arch": "x86_64"}
    })
}

fn metrics_report(seq: u64, cpu: f64) -> String {
    json!({
        "type": "report", "seq": seq, "ts": "2026-10-09T00:00:00.000Z",
        "metrics": [
            {"name": "system.cpu.usage", "value": cpu, "unit": "%"},
            {"name": "system.memory.total", "value": 51_539_607_552_u64, "unit": "By"},
            {"name": "system.memory.used", "value": 38_833_356_800_u64, "unit": "By"},
            {"name": "system.filesystem.total", "value": 1000, "unit": "By",
             "labels": {"mountpoint": "/", "fstype": "ext4"}, "extra": "dropped"},
            {"name": "system.temperature", "value": -3.5, "unit": "Cel",
             "labels": {"sensor": "acpitz"}}
        ]
    })
    .to_string()
}

impl Server {
    /// report 檔的最後一行
    fn last_report_line(&self) -> Value {
        let text = std::fs::read_to_string(self.reports_file()).unwrap();
        serde_json::from_str(text.lines().last().unwrap()).unwrap()
    }
}

#[tokio::test]
async fn v1_pulse_gets_a_v1_session_on_the_new_server() {
    // 舊 Pulse ↔ 新 Kairos：hello_ack、report 格式、JSONL（除了新增的 schema）都與 v1 相同
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    let ack = c.register(ID).await;
    assert_eq!(
        ack,
        json!({"type":"hello_ack","seq":1,"proto":1,"offline_after_ms":5000})
    );
    c.send(&report(2)).await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":2}));
    let line = server.last_report_line();
    assert_eq!(line["schema"], 1);
    assert_eq!(line["cpu_usage"], 12.5);
    assert_eq!(line["total_memory"], 1000);
    assert!(line.get("metrics").is_none(), "{line}");
    // v1 連線送 metrics 格式：與 v1 Kairos 相同，是缺欄位的 bad_request（seq 為 null）
    c.send(&metrics_report(3, 1.0)).await;
    let err = c.expect_error("bad_request", false).await;
    assert_eq!(err["seq"], Value::Null);
    assert!(
        err["message"].as_str().unwrap().contains("cpu_usage"),
        "{err}"
    );
    // v1 report 夾帶型別錯誤的 metrics：v1 Kairos 會忽略，這裡也忽略
    let mut with_garbage: Value = serde_json::from_str(&report(4)).unwrap();
    with_garbage["metrics"] = json!(5);
    c.send(&with_garbage.to_string()).await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":4}));
    let snap = server.registry.snapshot(Utc::now());
    assert_eq!(snap.agents[0].proto, 1);
    assert_eq!(snap.agents[0].host, None);
}

#[tokio::test]
async fn v1_hello_ignores_v2_host_fields_with_bad_types() {
    // 協商為 v1 時，主機資訊欄位與 v1 Kairos 一樣被忽略（不檢查型別與限制）
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    let mut h: Value = serde_json::from_str(&hello(1, ID, 1000)).unwrap();
    h["display_name"] = json!(42);
    h["tags"] = json!("x");
    h["os"] = json!({"name": "a\nb"});
    c.send(&h.to_string()).await;
    assert_eq!(
        c.recv().await,
        json!({"type":"hello_ack","seq":1,"proto":1,"offline_after_ms":5000})
    );
}

#[tokio::test]
async fn new_pulse_negotiates_v2_with_metrics() {
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    c.send(&hello_v2(1, ID, &["metrics", "future-thing"]).to_string())
        .await;
    let ack = c.recv().await;
    assert_eq!(ack["type"], "hello_ack", "{ack}");
    assert_eq!(ack["proto"], 2);
    assert_eq!(ack["features"], json!(["metrics", "durable"]));
    assert_eq!(ack["offline_after_ms"], 5000);
    // server_time：RFC 3339 UTC 毫秒，接近現在
    let server_time = ack["server_time"].as_str().unwrap();
    assert_eq!(server_time.len(), 24, "{server_time}");
    assert!(server_time.ends_with('Z'));
    let t = chrono::DateTime::parse_from_rfc3339(server_time).unwrap();
    assert!((Utc::now() - t.with_timezone(&Utc)).num_seconds().abs() < 60);

    // 指標 report：原樣保存為 schema 2（指標物件中未定義的欄位不保存）
    c.send(&metrics_report(2, 17.84)).await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":2}));
    let line = server.last_report_line();
    assert_eq!(line["schema"], 2);
    assert_eq!(line["agent_id"], ID);
    assert!(line.get("cpu_usage").is_none(), "{line}");
    let mut expected: Value = serde_json::from_str(&metrics_report(2, 17.84)).unwrap();
    expected["metrics"][3]
        .as_object_mut()
        .unwrap()
        .remove("extra");
    assert_eq!(line["metrics"], expected["metrics"]);
    let text = std::fs::read_to_string(server.reports_file()).unwrap();
    assert!(text.contains(r#""value":51539607552,"#), "{text}");

    // agents.json：協定版本、主機資訊與由指標換算的 last_report
    let snap = serde_json::to_value(server.registry.snapshot(Utc::now())).unwrap();
    let a = &snap["agents"][0];
    assert_eq!(a["proto"], 2);
    assert_eq!(a["display_name"], "Web 01");
    assert_eq!(a["report_interval_ms"], 5000);
    assert_eq!(a["os"]["arch"], "x86_64");
    assert_eq!(a["last_report"]["total_memory"], 49152);
    assert_eq!(a["last_report"]["metrics"], 5);

    // v2 metrics 連線送 v1 三欄位：缺 metrics 的 bad_request
    c.send(&report(3)).await;
    let err = c.expect_error("bad_request", false).await;
    assert_eq!(err["seq"], Value::Null);
    assert!(
        err["message"].as_str().unwrap().contains("metrics"),
        "{err}"
    );
    // 內容不合限制：invalid_report（帶 seq，不保存，連線保持）
    c.send(&metrics_report(4, 101.0)).await;
    let err = c.expect_error("invalid_report", false).await;
    assert_eq!(err["seq"], 4);
    let mut bad_name: Value = serde_json::from_str(&metrics_report(5, 1.0)).unwrap();
    bad_name["metrics"][0]["name"] = json!("CPU");
    c.send(&bad_name.to_string()).await;
    assert_eq!(c.expect_error("invalid_report", false).await["seq"], 5);
    // metrics 型別錯誤：bad_request
    let mut bad_type: Value = serde_json::from_str(&metrics_report(6, 1.0)).unwrap();
    bad_type["metrics"][0]["value"] = json!("1");
    c.send(&bad_type.to_string()).await;
    assert_eq!(
        c.expect_error("bad_request", false).await["seq"],
        Value::Null
    );
    c.send(&heartbeat(7)).await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":7}));
    assert_eq!(server.last_report_line()["seq"], 2);
}

#[tokio::test]
async fn v2_without_the_metrics_feature_keeps_the_v1_report_format() {
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    c.send(&hello_v2(1, ID, &[]).to_string()).await;
    let ack = c.recv().await;
    assert_eq!(ack["proto"], 2, "{ack}");
    assert_eq!(ack["features"], json!(["metrics", "durable"]));
    c.send(&report(2)).await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":2}));
    assert_eq!(server.last_report_line()["schema"], 1);
    c.send(&metrics_report(3, 1.0)).await;
    c.expect_error("bad_request", false).await;
}

#[tokio::test]
async fn version_ranges_without_overlap_are_rejected_with_the_server_range() {
    let server = Server::start(Options::default()).await;
    for (proto, min, max) in [(3, Some(3), Some(4)), (0, None, None), (9, None, None)] {
        let mut c = Client::connect(server.addr).await;
        let mut h: Value = serde_json::from_str(&hello(5, ID, 1000)).unwrap();
        h["proto"] = json!(proto);
        if let (Some(min), Some(max)) = (min, max) {
            h["proto_min"] = json!(min);
            h["proto_max"] = json!(max);
        }
        c.send(&h.to_string()).await;
        let err = c.expect_error("unsupported_version", true).await;
        assert_eq!(err["seq"], 5);
        assert_eq!(err["message"], "supported proto: 1-2");
        assert_eq!(
            (err["proto_min"].clone(), err["proto_max"].clone()),
            (json!(1), json!(2))
        );
        c.expect_clean_close().await;
    }
    // proto 2（沒有範圍）：只支援 v2 的 client，新伺服器以 v2 接受
    let mut c = Client::connect(server.addr).await;
    let mut h: Value = serde_json::from_str(&hello(1, ID, 1000)).unwrap();
    h["proto"] = json!(2);
    c.send(&h.to_string()).await;
    assert_eq!(c.recv().await["proto"], 2);
    // 未來的 client（1～9）：取共同的最高版本 2
    let mut c = Client::connect(server.addr).await;
    let mut h = hello_v2(1, OTHER_ID, &["metrics"]);
    h["proto_max"] = json!(9);
    c.send(&h.to_string()).await;
    assert_eq!(c.recv().await["proto"], 2);
}

#[tokio::test]
async fn invalid_v2_hello_fields_are_bad_requests_with_the_seq() {
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    let cases: Vec<(&str, Value)> = vec![
        ("proto_min", json!(null)),       // 只有 proto_max
        ("proto_min", json!(3)),          // min > max
        ("proto_min", json!("1")),        // 型別錯誤
        ("features", json!(["Metrics"])), // 格式
    ];
    for (i, (field, value)) in cases.into_iter().enumerate() {
        let seq = 10 + i as u64;
        let mut h = hello_v2(seq, ID, &["metrics"]);
        h[field] = value;
        c.send(&h.to_string()).await;
        assert_eq!(
            c.expect_error("bad_request", false).await["seq"],
            seq,
            "{field}"
        );
        // 不讓連續無效訊息達到上限
        if i == 2 {
            c = Client::connect(server.addr).await;
        }
    }
    let mut c = Client::connect(server.addr).await;
    for (i, (field, value)) in [
        ("display_name", json!("a\u{202e}b")),
        ("tags", json!(["a", "a"])),
        ("report_interval_ms", json!(10)),
        ("host_fingerprint", json!("md5:abc")),
    ]
    .into_iter()
    .enumerate()
    {
        let seq = 20 + i as u64;
        let mut h = hello_v2(seq, ID, &["metrics"]);
        h[field] = value;
        c.send(&h.to_string()).await;
        assert_eq!(
            c.expect_error("bad_request", false).await["seq"],
            seq,
            "{field}"
        );
    }
    // 修正後的 hello 仍可在同一條連線註冊
    c.send(&hello_v2(30, ID, &["metrics"]).to_string()).await;
    let ack = c.recv().await;
    assert_eq!(
        (ack["type"].clone(), ack["seq"].clone()),
        (json!("hello_ack"), json!(30))
    );
}

#[tokio::test]
async fn changed_host_fingerprint_is_logged() {
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    c.send(&hello_v2(1, ID, &["metrics"]).to_string()).await;
    assert_eq!(c.recv().await["proto"], 2);
    let mut h = hello_v2(1, ID, &["metrics"]);
    h["host_fingerprint"] = json!(format!("sha256:{}", "b".repeat(64)));
    let mut other = Client::connect(server.addr).await;
    other.send(&h.to_string()).await;
    assert_eq!(other.recv().await["proto"], 2);
    let logs = server
        .wait_for_logs(1, |l| l.contains("主機指紋與上次不同"))
        .await;
    let line = logs
        .iter()
        .find(|l| l.contains("主機指紋與上次不同"))
        .expect("應記錄主機指紋改變");
    assert!(line.contains("sha256:aaaaaaaaaaaa…"), "{line}");
    assert!(line.contains("sha256:bbbbbbbbbbbb…"), "{line}");
}

// ---- durable：at-least-once（A5） ----

const BOOT: &str = "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e";

fn durable_report(seq: u64, boot_id: &str, report_seq: u64, ts: &str) -> String {
    let mut r: Value = serde_json::from_str(&metrics_report(seq, 1.5)).unwrap();
    r["boot_id"] = json!(boot_id);
    r["report_seq"] = json!(report_seq);
    r["ts"] = json!(ts);
    r.to_string()
}

async fn durable_client(server: &Server, seq: u64) -> Client {
    let mut c = Client::connect(server.addr).await;
    c.send(&hello_v2(seq, ID, &["metrics", "durable"]).to_string())
        .await;
    let ack = c.recv().await;
    assert_eq!(ack["features"], json!(["metrics", "durable"]), "{ack}");
    c
}

fn durable_lines(server: &Server) -> Vec<Value> {
    std::fs::read_to_string(server.reports_file())
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn durable_resend_is_acked_but_written_once() {
    let server = Server::start(Options::default()).await;
    let mut c = durable_client(&server, 1).await;
    // 補送的歷史資料（3 天前）照收，ts 原樣保存
    let old = "2026-10-06T03:15:45.210Z";
    c.send(&durable_report(2, BOOT, 41, old)).await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":2}));
    // boot_id 大小寫不同仍是同一筆（伺服器轉小寫）
    c.send(&durable_report(3, &BOOT.to_ascii_uppercase(), 41, old))
        .await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":3}));
    c.send(&durable_report(4, BOOT, 42, "2026-10-06T03:15:50.210Z"))
        .await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":4}));
    drop(c);

    // 結果不明時斷線、重連後重送：回 ack，不重寫
    let mut c = durable_client(&server, 1).await;
    c.send(&durable_report(2, BOOT, 42, "2026-10-06T03:15:50.210Z"))
        .await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":2}));
    c.send(&durable_report(3, BOOT, 43, "2026-10-06T03:15:55.210Z"))
        .await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":3}));

    let lines = durable_lines(&server);
    let keys: Vec<(String, u64, String)> = lines
        .iter()
        .map(|l| {
            (
                l["boot_id"].as_str().unwrap().to_string(),
                l["report_seq"].as_u64().unwrap(),
                l["ts"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            (BOOT.to_string(), 41, old.to_string()),
            (BOOT.to_string(), 42, "2026-10-06T03:15:50.210Z".to_string()),
            (BOOT.to_string(), 43, "2026-10-06T03:15:55.210Z".to_string()),
        ]
    );
    assert!(lines.iter().all(|l| l["schema"] == 2));
}

#[tokio::test]
async fn durable_report_without_valid_seq_fields_is_bad_request() {
    let server = Server::start(Options::default()).await;
    let mut c = durable_client(&server, 1).await;
    let base: Value = serde_json::from_str(&metrics_report(2, 1.5)).unwrap();
    let cases = [
        json!({}),                                   // 缺漏
        json!({"boot_id": BOOT}),                    // 只有一個
        json!({"boot_id": BOOT, "report_seq": "1"}), // 型別錯誤
        json!({"boot_id": BOOT, "report_seq": -1}),
        json!({"boot_id": "not-a-uuid", "report_seq": 1}), // 不是 UUID
        json!({"boot_id": BOOT, "report_seq": 0}),         // 至少為 1
    ];
    for (i, extra) in cases.iter().enumerate() {
        let mut r = base.clone();
        for (k, v) in extra.as_object().unwrap() {
            r[k] = v.clone();
        }
        c.send(&r.to_string()).await;
        let err = c.recv().await;
        assert_eq!(err["code"], "bad_request", "{extra}: {err}");
        assert_eq!(err["seq"], Value::Null, "{err}");
        // 連續 5 則無效訊息會斷線：中間穿插一則 heartbeat
        if i % 2 == 1 {
            c.send(r#"{"type":"heartbeat","seq":9,"ts":"t"}"#).await;
            assert_eq!(c.recv().await, json!({"type":"ack","seq":9}));
        }
    }
    assert!(durable_lines(&server).is_empty());
}

#[tokio::test]
async fn durable_fields_are_ignored_when_not_negotiated() {
    // 0.2.0 Pulse（不宣告 durable）：report 帶了也忽略，型別錯誤也不檢查，與 0.2.0 Kairos 相同
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    c.send(&hello_v2(1, ID, &["metrics"]).to_string()).await;
    let ack = c.recv().await;
    assert_eq!(ack["features"], json!(["metrics", "durable"]));
    let mut r: Value = serde_json::from_str(&metrics_report(2, 1.5)).unwrap();
    r["boot_id"] = json!(5);
    r["report_seq"] = json!("x");
    c.send(&r.to_string()).await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":2}));
    c.send(&durable_report(3, BOOT, 1, "2026-10-09T00:00:00.000Z"))
        .await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":3}));
    c.send(&durable_report(4, BOOT, 1, "2026-10-09T00:00:00.000Z"))
        .await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":4}));
    let lines = durable_lines(&server);
    assert_eq!(lines.len(), 3, "未協商 durable 時不去重");
    assert!(lines.iter().all(|l| l.get("boot_id").is_none()));
}

// ---- 指標擴充：processes、inventory（crates/kairos-proto/METRICS.md） ----

const METRICS_SPEC: &str = include_str!("../../crates/kairos-proto/METRICS.md");

/// METRICS.md「範例」節中某個小節的訊息：(方向 `P`／`K`, JSON)
fn spec_messages(section: &str) -> Vec<(char, Value)> {
    // 換行可能是 CRLF（Windows 的 checkout）
    let heading = format!("### {section}");
    let start = METRICS_SPEC
        .match_indices(&heading)
        .map(|(i, _)| i)
        .find(|i| {
            matches!(
                METRICS_SPEC.as_bytes().get(i + heading.len()),
                Some(b'\r' | b'\n')
            )
        })
        .unwrap_or_else(|| panic!("METRICS.md 沒有小節 {section}"));
    let rest = &METRICS_SPEC[start..];
    let block = rest.split("```").nth(1).unwrap();
    block
        .lines()
        .filter_map(|l| {
            let (dir, json) = l.trim_end().split_once(' ')?;
            let dir = dir.chars().next()?;
            matches!(dir, 'P' | 'K').then(|| (dir, serde_json::from_str(json).unwrap()))
        })
        .collect()
}

fn sqlite_server_options() -> Options {
    Options {
        sqlite: true,
        ..Options::default()
    }
}

impl Server {
    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.join(crate::store::sqlite::DB_FILE)).unwrap()
    }

    fn count(&self, sql: &str) -> i64 {
        self.db().query_row(sql, [], |r| r.get(0)).unwrap()
    }
}

#[tokio::test]
async fn metrics_spec_examples_get_the_documented_responses() {
    let server = Server::start(sqlite_server_options()).await;
    let mut c = Client::connect(server.addr).await;
    let msgs = spec_messages("協商與 report");
    // hello → hello_ack（server_time 以外與範例相同）
    assert_eq!(msgs[0].0, 'P');
    c.send(&msgs[0].1.to_string()).await;
    let mut ack = c.recv().await;
    let mut expected = msgs[1].1.clone();
    ack.as_object_mut().unwrap().remove("server_time");
    expected.as_object_mut().unwrap().remove("server_time");
    assert_eq!(ack, expected);
    // report（新指標、processes、durable）→ ack
    c.send(&msgs[2].1.to_string()).await;
    assert_eq!(c.recv().await, msgs[3].1);
    // inventory → ack
    let inv = spec_messages("inventory");
    c.send(&inv[0].1.to_string()).await;
    assert_eq!(c.recv().await, inv[1].1);
    // 60 秒內再送一次 → storage_unavailable；內容不合法 → invalid_report（不受 60 秒限制）
    let errors = spec_messages("錯誤");
    let mut again = inv[0].1.clone();
    again["seq"] = json!(4);
    c.send(&again.to_string()).await;
    assert_eq!(c.recv().await, errors[0].1);
    let mut bad = inv[0].1.clone();
    bad["seq"] = json!(5);
    bad["inventory"]["network_interfaces"][0]["mac"] = json!("52-54-00-12-34-56");
    c.send(&bad.to_string()).await;
    assert_eq!(c.recv().await, errors[1].1);
    // 型別錯誤（協商了 inventory）：bad_request，seq 為 null
    let mut wrong = inv[0].1.clone();
    wrong["seq"] = json!(6);
    wrong["inventory"]["memory"]["total"] = json!("8G");
    c.send(&wrong.to_string()).await;
    let err = c.expect_error("bad_request", false).await;
    assert_eq!(err["seq"], Value::Null);
    // 保存的內容：行程快照、資產、速率與推算以外的序列
    c.send(&heartbeat(7)).await;
    assert_eq!(c.recv().await["type"], "ack");
    assert_eq!(server.count("SELECT count(*) FROM process_snapshots"), 1);
    assert_eq!(server.count("SELECT count(*) FROM host_inventory"), 1);
    assert_eq!(
        server.count("SELECT cpu_logical_cores FROM host_inventory"),
        64
    );
    assert_eq!(
        server.count("SELECT count(*) FROM hosts WHERE boot_time IS NOT NULL"),
        1
    );
    // 36 個指標都是新序列（每主機上限 2048 之內）
    assert_eq!(server.count("SELECT count(*) FROM host_series"), 36);
    // registry：協商結果與資產摘要（告警的每核心負載）
    let live = server.registry.live_get(ID).unwrap();
    assert_eq!((live.inventory, live.processes), (Some(true), Some(true)));
    // 最近一次被拒的資產（KRS-W2108；主控台 S05 的提示）
    assert!(live.inventory_rejected.as_ref().unwrap().1.contains("mac"));
    assert_eq!(live.assets.unwrap().logical_cores, Some(64));
    let last = live.last_report.unwrap();
    assert_eq!(last.detail.filesystems.len(), 2);
    assert_eq!(last.detail.uptime_secs, Some(1_532_941));
}

#[tokio::test]
async fn processes_are_validated_only_when_negotiated() {
    let server = Server::start(sqlite_server_options()).await;
    let mut c = Client::connect(server.addr).await;
    c.send(&hello_v2(1, ID, &["metrics", "processes"]).to_string())
        .await;
    let ack = c.recv().await;
    assert_eq!(
        ack["features"],
        json!(["metrics", "durable", "inventory", "processes"])
    );
    let with = |seq: u64, processes: Value| {
        let mut r: Value = serde_json::from_str(&metrics_report(seq, 1.0)).unwrap();
        r["processes"] = processes;
        r.to_string()
    };
    // cpu_usage 超過 100：invalid_report（不保存）
    c.send(&with(
        2,
        json!({"top_cpu": [{"pid": 1, "name": "x", "cpu_usage": 120.0, "memory": 1}]}),
    ))
    .await;
    let err = c.expect_error("invalid_report", false).await;
    assert_eq!(err["seq"], 2);
    assert!(
        err["message"].as_str().unwrap().starts_with("processes"),
        "{err}"
    );
    // 型別錯誤：bad_request
    c.send(&with(3, json!({"top_cpu": [{"pid": -1}]}))).await;
    c.expect_error("bad_request", false).await;
    c.send(&with(
        4,
        json!({"top_memory": [{"pid": 9, "name": "java", "cpu_usage": 1.0, "memory": 5}]}),
    ))
    .await;
    assert_eq!(c.recv().await, json!({"type":"ack","seq":4}));
    assert_eq!(server.count("SELECT count(*) FROM process_snapshots"), 1);
    assert_eq!(server.count("SELECT count(*) FROM reports"), 1);
    // 沒有協商 inventory：inventory 為不認得的 type
    let inv = spec_messages("inventory");
    c.send(&inv[0].1.to_string()).await;
    c.expect_error("unknown_type", false).await;
    assert_eq!(server.count("SELECT count(*) FROM host_inventory"), 0);
}

#[tokio::test]
async fn jsonl_backend_does_not_declare_inventory_or_processes() {
    // METRICS.md 第 7.2、9.5 節：JSONL 後端只有 metrics、durable；processes 被忽略，新指標照存
    let server = Server::start(Options::default()).await;
    let mut c = Client::connect(server.addr).await;
    let msgs = spec_messages("協商與 report");
    c.send(&msgs[0].1.to_string()).await;
    let ack = c.recv().await;
    assert_eq!(ack["features"], json!(["metrics", "durable"]));
    let mut report = msgs[2].1.clone();
    report["processes"] = json!("沒有協商時型別錯誤也不檢查");
    c.send(&report.to_string()).await;
    assert_eq!(c.recv().await, msgs[3].1);
    let line = server.last_report_line();
    assert_eq!(line["metrics"].as_array().unwrap().len(), 36);
    assert!(line.get("processes").is_none(), "{line}");
    let inv = spec_messages("inventory");
    c.send(&inv[0].1.to_string()).await;
    c.expect_error("unknown_type", false).await;
    // 格式錯誤的 inventory 也與舊版相同
    c.send(r#"{"type":"inventory","seq":9,"ts":1}"#).await;
    c.expect_error("unknown_type", false).await;
}

#[tokio::test]
async fn old_pulse_metrics_get_estimated_usage_on_the_new_server() {
    // 0.2.0～0.7.0 的 Pulse（只有 8 個指標、不宣告新 feature）× 0.8.0 Kairos（SQLite）
    let server = Server::start(sqlite_server_options()).await;
    let mut c = Client::connect(server.addr).await;
    c.send(&hello_v2(1, ID, &["metrics", "durable"]).to_string())
        .await;
    assert_eq!(c.recv().await["type"], "hello_ack");
    let old = |seq: u64, ts: &str, rx: u64| {
        json!({
            "type": "report", "seq": seq, "ts": ts,
            "boot_id": "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e", "report_seq": seq,
            "metrics": [
                {"name": "system.cpu.usage", "value": 5.0, "unit": "%"},
                {"name": "system.filesystem.total", "value": 1000, "unit": "By", "labels": {"mountpoint": "/", "fstype": "ext4"}},
                {"name": "system.filesystem.available", "value": 400, "unit": "By", "labels": {"mountpoint": "/", "fstype": "ext4"}},
                {"name": "system.network.received", "value": rx, "unit": "By", "labels": {"device": "eth0"}}
            ]
        })
        .to_string()
    };
    let now = Utc::now();
    let t0 = crate::storage::fmt_time(now - chrono::Duration::seconds(10));
    let t1 = crate::storage::fmt_time(now - chrono::Duration::seconds(5));
    c.send(&old(2, &t0, 1000)).await;
    assert_eq!(c.recv().await["type"], "ack");
    c.send(&old(3, &t1, 6000)).await;
    assert_eq!(c.recv().await["type"], "ack");
    let last = server.registry.live_get(ID).unwrap().last_report.unwrap();
    let fs = &last.detail.filesystems[0];
    assert_eq!((fs.usage, fs.estimated), (Some(60.0), true));
    assert_eq!(last.detail.network[0].rx_rate, Some(1000.0));
    // 推算序列與速率序列已建立（彙總在桶結束後寫出）
    assert_eq!(
        server.count("SELECT count(*) FROM series WHERE derived = 2"),
        1
    );
    assert_eq!(
        server.count("SELECT count(*) FROM series WHERE derived = 1"),
        1
    );
}
