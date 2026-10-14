//! 註冊服務的整合測試：實際的 8444（TLS＋hyper），以 HTTPS 用戶端送出 `kairos-proto` 的型別，
//! CSR 以 rcgen 產生。時鐘可以前進（過期續簽、自動暫停），資料庫是實際的 SQLite（schema v4）。

use kairos_proto::enroll::{
    EnrollMode, EnrollRequest, EnrollResult, ErrorResponse, GraceRenewRequest, InitiatedBy,
    RequestState, ServerInfo, ServerVerification, SignatureAlg, TokenApproval, TokenCheckRequest,
    TokenCheckResponse, WithdrawChallenge, WithdrawReason, WithdrawRequest, WithdrawResult,
    b64url_decode, b64url_encode, format_rfc3339_ms, grace_renew_signing_input,
    sign_ecdsa_p256_sha256, withdraw_signing_input,
};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, PublicKeyData, SanType,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use super::ops::{self, TokenInput};
use super::{Running, StartParams, store};
use crate::config::{AppConfig, PkiMode};
use crate::pki::store as ps;
use crate::pki::x509::{parse_cert, pem_decode_all};
use crate::shutdown::Shutdown;

const DAY: i64 = 86_400_000;

fn t(secs: i64) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(secs).unwrap()
}

/// 測試用的根 CA（P-384）與 localhost 的伺服器憑證
struct Root {
    key: KeyPair,
    params: CertificateParams,
    pem: String,
}

fn root() -> Root {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
    let mut p = CertificateParams::default();
    p.distinguished_name = DistinguishedName::new();
    p.distinguished_name
        .push(DnType::CommonName, "Test Root CA");
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let now = chrono::Utc::now().timestamp();
    p.not_before = t(now - 86_400);
    p.not_after = t(now + 3650 * 86_400);
    let pem = p.self_signed(&key).unwrap().pem();
    Root {
        key,
        params: p,
        pem,
    }
}

impl Root {
    fn issuer(&self) -> Issuer<'_, &KeyPair> {
        Issuer::from_params(&self.params, &self.key)
    }

    /// localhost／127.0.0.1 的伺服器憑證（serverAuth）
    fn server_cert(&self) -> (String, String) {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut p = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        p.subject_alt_names
            .push(SanType::IpAddress("127.0.0.1".parse().unwrap()));
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, "localhost");
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.is_ca = IsCa::ExplicitNoCa;
        let now = chrono::Utc::now().timestamp();
        p.not_before = t(now - 86_400);
        p.not_after = t(now + 365 * 86_400);
        let cert = p.signed_by(&key, &self.issuer()).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    /// 以根 CA 簽發簽發 CA 的 CSR（INSTALL 的 issuing_ca_ext 範本）
    fn sign_issuing(&self, csr_pem: &str) -> String {
        let mut csr = CertificateSigningRequestParams::from_pem(csr_pem).unwrap();
        csr.params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        csr.params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        csr.params.use_authority_key_identifier_extension = true;
        let now = chrono::Utc::now().timestamp();
        csr.params.not_before = t(now - 3600);
        csr.params.not_after = t(now + 5 * 365 * 86_400);
        csr.signed_by(&self.issuer()).unwrap().pem()
    }
}

/// 一個 Pulse 的身分：P-256 私鑰、CSR、poll_secret
struct Agent {
    id: String,
    key: KeyPair,
    csr: Vec<u8>,
    poll_secret: String,
    fingerprint: String,
}

fn agent(id: &str) -> Agent {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let mut p = CertificateParams::new(vec![id.to_string()]).unwrap();
    p.distinguished_name = DistinguishedName::new();
    p.distinguished_name.push(DnType::CommonName, id);
    let csr = p.serialize_request(&key).unwrap().der().to_vec();
    let mut secret = [0u8; 32];
    use ring::rand::SecureRandom;
    ring::rand::SystemRandom::new().fill(&mut secret).unwrap();
    Agent {
        id: id.to_string(),
        key,
        csr,
        poll_secret: b64url_encode(&secret),
        fingerprint: format!("sha256:{}", "ab".repeat(32)),
    }
}

impl Agent {
    fn rekey(&self) -> Agent {
        let mut a = agent(&self.id);
        a.fingerprint = self.fingerprint.clone();
        a
    }

    fn enroll_request(&self, token: &str, sent_at_ms: i64) -> EnrollRequest {
        EnrollRequest {
            token: token.to_string(),
            agent_id: self.id.clone(),
            csr: b64url_encode(&self.csr),
            poll_secret: self.poll_secret.clone(),
            sent_at: format_rfc3339_ms(sent_at_ms),
            hostname: format!("host-{}", &self.id[..8]),
            pulse_version: "0.5.0".into(),
            server_verification: ServerVerification::CaPin,
            display_name: None,
            tags: None,
            host_fingerprint: Some(self.fingerprint.clone()),
            os: None,
            previous_serial: None,
        }
    }
}

/// 測試環境：實際的 8444、可前進的時鐘、資料庫
struct Env {
    dir: PathBuf,
    root: Root,
    config: AppConfig,
    offset: Arc<AtomicI64>,
    running: Option<Running>,
    shutdown: Arc<Shutdown>,
    port: u16,
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kairos-enroll-{tag}-{}-{}",
        std::process::id(),
        crate::registry::next_conn_id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

impl Env {
    async fn new(tag: &str, tweak: impl FnOnce(&mut AppConfig)) -> Env {
        let dir = temp_dir(tag);
        let root = root();
        let (cert, key) = root.server_cert();
        std::fs::write(dir.join("server.pem"), cert).unwrap();
        std::fs::write(dir.join("server.key"), key).unwrap();
        std::fs::write(dir.join("root.pem"), &root.pem).unwrap();
        let mut c = AppConfig::default();
        let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
        c.server.listen = "127.0.0.1:0".into();
        c.server.cert_path = p("server.pem");
        c.server.key_path = p("server.key");
        c.auth.client_ca = p("root.pem");
        c.storage.data_dir = p("data");
        c.console.enabled = false;
        c.enroll.mode = Some(EnrollMode::Open);
        c.enroll.listen = "127.0.0.1:0".into();
        c.pki.mode = Some(PkiMode::Online);
        c.pki.server_ca_path = p("root.pem");
        tweak(&mut c);
        // 資料庫（schema v4）與簽發 CA
        let data = PathBuf::from(&c.storage.data_dir);
        std::fs::create_dir_all(&data).unwrap();
        let opts = crate::store::sqlite::SqliteOptions::from_config(&c, &data);
        let conn = crate::store::sqlite::open_admin(&opts, Duration::from_secs(5)).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let (_, n) = crate::pki::manage::create(&conn, &data, "Test", now).unwrap();
        let cert = root.sign_issuing(&n.csr_pem);
        let installed = crate::pki::manage::install(&conn, &data, &cert, now).unwrap();
        assert!(installed.activated);
        drop(conn);
        let mut env = Env {
            dir,
            root,
            config: c,
            offset: Arc::new(AtomicI64::new(0)),
            running: None,
            shutdown: Arc::new(Shutdown::new()),
            port: 0,
        };
        env.start().await;
        env
    }

    async fn start(&mut self) {
        let offset = self.offset.clone();
        let data = PathBuf::from(&self.config.storage.data_dir);
        self.shutdown = Arc::new(Shutdown::new());
        let running = super::start(StartParams {
            config: Arc::new(self.config.clone()),
            data_dir: data.clone(),
            db_path: super::db_path(&self.config, &data),
            shutdown: self.shutdown.clone(),
            other_listens: Vec::new(),
            clock: Arc::new(move || {
                chrono::Utc::now().timestamp_millis() + offset.load(Ordering::Relaxed)
            }),
        })
        .await
        .unwrap();
        let svc = running.service.clone();
        self.running = Some(running);
        for _ in 0..200 {
            if let Some(addr) = super::server::Listener::local_addr(&svc) {
                self.port = addr.rsplit_once(':').unwrap().1.parse().unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("註冊端點沒有開始監聽：{:?}", svc.listen.lock().unwrap());
    }

    async fn stop(&mut self) {
        self.shutdown.trigger();
        if let Some(r) = self.running.take() {
            r.join().await;
        }
    }

    fn svc(&self) -> Arc<super::EnrollService> {
        self.running.as_ref().unwrap().service.clone()
    }

    fn now(&self) -> i64 {
        self.svc().now_ms()
    }

    fn advance(&self, ms: i64) {
        self.offset.fetch_add(ms, Ordering::Relaxed);
    }

    fn conn(&self) -> rusqlite::Connection {
        let data = PathBuf::from(&self.config.storage.data_dir);
        let opts = crate::store::sqlite::SqliteOptions::from_config(&self.config, &data);
        crate::store::sqlite::open_admin(&opts, Duration::from_secs(5)).unwrap()
    }

    fn token(&self, approval: TokenApproval, max_uses: i64) -> String {
        let mut conn = self.conn();
        let input = TokenInput {
            name: "test".into(),
            group: "g".into(),
            max_uses,
            approval: Some(approval),
            allowed_cidrs: (approval == TokenApproval::Auto).then(|| vec!["127.0.0.0/8".into()]),
            ..TokenInput::default()
        };
        ops::create_token(&mut conn, &input, "tester", None, self.now())
            .unwrap()
            .token
            .canonical()
    }

    fn endpoint(&self) -> String {
        format!("localhost:{}", self.port)
    }

    /// HTTPS 請求（每次一條新連線、Connection: close），回傳 (狀態碼, 標頭, JSON 本文)
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        auth: Option<&str>,
    ) -> (u16, Vec<(String, String)>, Value) {
        let mut roots = RootCertStore::empty();
        for (_, der) in pem_decode_all(&self.root.pem, Some("CERTIFICATE")).unwrap() {
            roots.add(CertificateDer::from(der)).unwrap();
        }
        let mut cfg = ClientConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .unwrap();
        let mut tls = TlsConnector::from(Arc::new(cfg))
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .unwrap();
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut req =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
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
        tls.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let _ = tls.read_to_end(&mut buf).await;
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, rest) = text.split_once("\r\n\r\n").expect("回應沒有標頭");
        let mut lines = head.lines();
        let status: u16 = lines
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let json = serde_json::from_str(rest).unwrap_or(Value::Null);
        (status, headers, json)
    }

    async fn post(&self, path: &str, body: &impl serde::Serialize) -> (u16, Value) {
        let (s, _, v) = self
            .call(
                "POST",
                path,
                Some(&serde_json::to_value(body).unwrap()),
                None,
            )
            .await;
        (s, v)
    }

    async fn enroll(&self, a: &Agent, token: &str) -> (u16, Value) {
        self.post("/v1/enroll/requests", &a.enroll_request(token, self.now()))
            .await
    }

    async fn poll(&self, a: &Agent, request_id: &str) -> (u16, Value) {
        let (s, _, v) = self
            .call(
                "GET",
                &format!("/v1/enroll/requests/{request_id}"),
                None,
                Some(&a.poll_secret),
            )
            .await;
        (s, v)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

fn issued_cert(v: &Value) -> crate::pki::x509::CertInfo {
    let pem = v["certificate"]["client_cert_pem"].as_str().unwrap();
    let der = pem_decode_all(pem, Some("CERTIFICATE"))
        .unwrap()
        .remove(0)
        .1;
    parse_cert(&der).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_enrollment_is_approved_and_delivered_with_the_pinned_ca() {
    let env = Env::new("manual", |_| {}).await;
    // server-info：8444 的鏈附上被釘選的 CA
    let (s, _, v) = env.call("GET", "/v1/enroll/server-info", None, None).await;
    assert_eq!(s, 200, "{v}");
    let info: ServerInfo = serde_json::from_value(v.clone()).unwrap();
    assert!(info.accepts.new_enrollment && info.accepts.grace_renewal);
    assert_eq!(info.observed_ip, "127.0.0.1");
    assert!(info.enroll_features.contains(&"withdraw".to_string()));
    assert!(info.features.contains(&"renew".to_string()));

    let token = env.token(TokenApproval::Manual, 3);
    // token-check
    let (s, v) = env
        .post(
            "/v1/enroll/token-check",
            &TokenCheckRequest {
                token: token.clone(),
                sent_at: None,
                pulse_version: None,
            },
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let tc: TokenCheckResponse = serde_json::from_value(v).unwrap();
    assert_eq!(tc.token.remaining_uses, 3);
    // 不存在的 token（檢查碼正確）：403 token_invalid
    let bogus = kairos_proto::enroll::EnrollToken::from_random(&[7; 10]).canonical();
    let (s, v) = env
        .post(
            "/v1/enroll/token-check",
            &TokenCheckRequest {
                token: bogus,
                sent_at: None,
                pulse_version: None,
            },
        )
        .await;
    assert_eq!((s, code(&v)), (403, "token_invalid"));

    let a = agent("6f1c0e4a-1b2c-4d3e-9f00-112233445566");
    let (s, v) = env.enroll(&a, &token).await;
    assert_eq!(s, 200, "{v}");
    let r: EnrollResult = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(r.state, RequestState::PendingApproval);
    assert!(r.poll_after_ms.unwrap() >= 30_000);
    assert_eq!(
        r.request_code,
        kairos_proto::enroll::request_code_from_spki(&a.key.subject_public_key_info())
    );
    // 冪等：同一個 CSR＋同一個 poll_secret 回傳同一筆，不再扣 token
    let (s, v) = env.enroll(&a, &token).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["request_id"], r.request_id.as_str());
    let used: i64 = env
        .conn()
        .query_row("SELECT used FROM enroll_tokens", [], |row| row.get(0))
        .unwrap();
    assert_eq!(used, 1);
    // poll_secret 不同：409
    let mut other = agent(&a.id);
    other.csr = a.csr.clone();
    let (s, v) = env.enroll(&other, &token).await;
    assert_eq!((s, code(&v)), (409, "request_conflict"));
    // 輪詢憑據錯誤：404（不透露是否存在）
    let (s, v) = env.poll(&other, &r.request_id).await;
    assert_eq!((s, code(&v)), (404, "request_not_found"));
    // 核准（CLI 的方式：只寫資料庫）→ 常駐行程在 2 秒內簽發；長輪詢立即取得
    let mut conn = env.conn();
    ops::approve(
        &mut conn,
        std::slice::from_ref(&r.request_id),
        "admin.chen",
        None,
        None,
        env.now(),
    )
    .unwrap();
    env.svc().wake();
    let (s, _, v) = env
        .call(
            "GET",
            &format!("/v1/enroll/requests/{}?wait=10", r.request_id),
            None,
            Some(&a.poll_secret),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "issued", "{v}");
    assert_eq!(v["decided_by"], "admin.chen");
    let cert = issued_cert(&v);
    assert_eq!(cert.dns_sans, vec![a.id.clone()]);
    assert_eq!(cert.spki_der, a.key.subject_public_key_info());
    assert_eq!(cert.serial, v["certificate"]["serial"].as_str().unwrap());
    // ca_pem 是被釘選的根 CA
    assert_eq!(
        v["certificate"]["ca_pem"].as_str().unwrap().trim(),
        env.root.pem.trim()
    );
    // 憑證由 Kairos 的簽發 CA 簽發；8443 的 mTLS 綁定接受
    let der = pem_decode_all(v["certificate"]["client_cert_pem"].as_str().unwrap(), None)
        .unwrap()
        .remove(0)
        .1;
    assert!(
        crate::auth::cert_matches_agent_id(&CertificateDer::from(der), &a.id, Default::default())
            .is_ok()
    );
    // 再取一次：同一張
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (s, v2) = env.poll(&a, &r.request_id).await;
    assert_eq!(s, 200);
    assert_eq!(v2["certificate"]["serial"], v["certificate"]["serial"]);
    // 清冊：current_serial
    let host = store::host_enroll(&env.conn(), &a.id).unwrap().unwrap();
    assert_eq!(host.current_serial.as_deref(), Some(cert.serial.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_token_issues_immediately_and_resend_returns_the_same_certificate() {
    let env = Env::new("auto", |_| {}).await;
    let token = env.token(TokenApproval::Auto, 5);
    let a = agent("11111111-2222-4333-8444-555555555555");
    let (s, v) = env.enroll(&a, &token).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "issued");
    assert_eq!(v["decided_by"], "auto");
    let (s, again) = env.enroll(&a, &token).await;
    assert_eq!(s, 200);
    assert_eq!(again["certificate"]["serial"], v["certificate"]["serial"]);
    // 同一個 agent_id 已有有效憑證時，自動核准的 token 也改為待核准
    let b = a.rekey();
    let (s, v) = env.enroll(&b, &token).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "pending_approval");
    let flags: String = env
        .conn()
        .query_row(
            "SELECT flags FROM enroll_requests WHERE request_id = ?1",
            [v["request_id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(flags.contains("agent_id_exists"), "{flags}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_skew_is_rejected_without_consuming_the_token() {
    let env = Env::new("clock", |_| {}).await;
    let token = env.token(TokenApproval::Manual, 1);
    let a = agent("22222222-2222-4333-8444-555555555555");
    let req = a.enroll_request(&token, env.now() + 7 * 60_000);
    let (s, v) = env.post("/v1/enroll/requests", &req).await;
    assert_eq!((s, code(&v)), (400, "clock_skew"), "{v}");
    let e: ErrorResponse = serde_json::from_value(v.clone()).unwrap();
    assert!(e.error.offset_ms.unwrap() > 300_000);
    assert_eq!(e.error.max_skew_ms, Some(300_000));
    assert_eq!(e.error.client_time.as_deref(), Some(req.sent_at.as_str()));
    // token 未消耗：同一個 token 仍可使用（次數上限 1）
    let (s, v) = env.enroll(&a, &token).await;
    assert_eq!(s, 200, "{v}");
    // 恰好 5 分鐘受理（以伺服器收到請求標頭的時間比較；容許 1 秒的執行時間）
    let b = agent("33333333-2222-4333-8444-555555555555");
    let token2 = env.token(TokenApproval::Manual, 1);
    let (s, v) = env
        .post(
            "/v1/enroll/requests",
            &b.enroll_request(&token2, env.now() - 299_000),
        )
        .await;
    assert_eq!(s, 200, "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grace_renewal_issues_a_new_certificate_and_replays_stay_rejected_after_restart() {
    let mut env = Env::new("grace", |_| {}).await;
    let token = env.token(TokenApproval::Auto, 5);
    let a = agent("44444444-2222-4333-8444-555555555555");
    let (s, v) = env.enroll(&a, &token).await;
    assert_eq!(s, 200, "{v}");
    let old = pem_decode_all(v["certificate"]["client_cert_pem"].as_str().unwrap(), None)
        .unwrap()
        .remove(0)
        .1;
    let renew = |env: &Env, new: &Agent, sent_at: i64| -> GraceRenewRequest {
        let sent = format_rfc3339_ms(sent_at);
        let input =
            grace_renew_signing_input(&a.id, &a.fingerprint, &new.csr, &sent, &env.endpoint())
                .unwrap();
        GraceRenewRequest {
            agent_id: a.id.clone(),
            csr: b64url_encode(&new.csr),
            poll_secret: new.poll_secret.clone(),
            sent_at: sent,
            host_fingerprint: a.fingerprint.clone(),
            expired_certificate: b64url_encode(&old),
            signature: sign_ecdsa_p256_sha256(&a.key.serialize_der(), &input).unwrap(),
            signature_alg: SignatureAlg::EcdsaP256Sha256,
            hostname: "h".into(),
            pulse_version: "0.5.0".into(),
        }
    };
    let new = a.rekey();
    // 還沒過期：409 certificate_not_expired
    let (s, v) = env
        .post("/v1/enroll/renewals", &renew(&env, &new, env.now()))
        .await;
    assert_eq!((s, code(&v)), (409, "certificate_not_expired"), "{v}");
    // 過期 10 天（寬限期 90 天內）
    env.advance(375 * DAY);
    let req = renew(&env, &new, env.now());
    let (s, v) = env.post("/v1/enroll/renewals", &req).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["kind"], "grace_renewal");
    assert_eq!(v["state"], "issued");
    assert_eq!(v["decided_by"], "system");
    let serial = v["certificate"]["serial"].clone();
    // 原樣重送：重放
    let (s, v) = env.post("/v1/enroll/renewals", &req).await;
    assert_eq!((s, code(&v)), (400, "replayed_request"), "{v}");
    // 同一個 CSR、新的 sent_at 與簽章（回應遺失後重送）：同一張憑證
    let (s, v) = env
        .post("/v1/enroll/renewals", &renew(&env, &new, env.now() + 1))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["certificate"]["serial"], serial);
    // 重新啟動後重放仍被拒（重放快取持久化）
    env.stop().await;
    // 同一個連接埠（簽章內容的最後一行含連接埠）
    env.config.enroll.listen = format!("127.0.0.1:{}", env.port);
    env.start().await;
    let (s, v) = env.post("/v1/enroll/renewals", &req).await;
    assert_eq!((s, code(&v)), (400, "replayed_request"), "{v}");
    // 舊憑證已不是最新的一張：以另一把新鑰換不到憑證
    let third = a.rekey();
    let (s, v) = env
        .post("/v1/enroll/renewals", &renew(&env, &third, env.now() + 2))
        .await;
    assert_eq!((s, code(&v)), (403, "renew_identity_mismatch"), "{v}");
    // 簽章錯誤（以別的私鑰簽）：renew_identity_mismatch
    let mut forged = renew(&env, &third, env.now() + 3);
    forged.signature = sign_ecdsa_p256_sha256(
        &third.key.serialize_der(),
        &grace_renew_signing_input(
            &a.id,
            &a.fingerprint,
            &third.csr,
            &forged.sent_at,
            &env.endpoint(),
        )
        .unwrap(),
    )
    .unwrap();
    let (s, v) = env.post("/v1/enroll/renewals", &forged).await;
    assert_eq!((s, code(&v)), (403, "renew_identity_mismatch"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grace_period_exceeded_and_override() {
    let env = Env::new("grace-exceeded", |_| {}).await;
    let token = env.token(TokenApproval::Auto, 5);
    let a = agent("55555555-2222-4333-8444-555555555555");
    let (_, v) = env.enroll(&a, &token).await;
    let old = pem_decode_all(v["certificate"]["client_cert_pem"].as_str().unwrap(), None)
        .unwrap()
        .remove(0)
        .1;
    env.advance(500 * DAY);
    let new = a.rekey();
    let mk = |sent_at: i64| {
        let sent = format_rfc3339_ms(sent_at);
        let input =
            grace_renew_signing_input(&a.id, &a.fingerprint, &new.csr, &sent, &env.endpoint())
                .unwrap();
        GraceRenewRequest {
            agent_id: a.id.clone(),
            csr: b64url_encode(&new.csr),
            poll_secret: new.poll_secret.clone(),
            sent_at: sent,
            host_fingerprint: a.fingerprint.clone(),
            expired_certificate: b64url_encode(&old),
            signature: sign_ecdsa_p256_sha256(&a.key.serialize_der(), &input).unwrap(),
            signature_alg: SignatureAlg::EcdsaP256Sha256,
            hostname: "h".into(),
            pulse_version: "0.5.0".into(),
        }
    };
    let (s, v) = env.post("/v1/enroll/renewals", &mk(env.now())).await;
    assert_eq!((s, code(&v)), (403, "renew_grace_exceeded"), "{v}");
    assert!(v["error"]["grace_ended_at"].is_string());
    // 允許再次自動續簽
    let mut conn = env.conn();
    ops::grace_override(
        &mut conn,
        std::slice::from_ref(&a.id),
        env.now() + 7 * DAY,
        "admin",
        None,
        "現場開機",
        env.now(),
    )
    .unwrap();
    let (s, v) = env.post("/v1/enroll/renewals", &mk(env.now() + 1)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "issued");
}

async fn challenge(env: &Env, rid: &str) -> String {
    let (s, v) = env
        .post(
            &format!("/v1/enroll/requests/{rid}/withdraw/challenge"),
            &json!({}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    serde_json::from_value::<WithdrawChallenge>(v)
        .unwrap()
        .nonce
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withdraw_with_the_csr_key() {
    let env = Env::new("withdraw", |c| c.enroll.poll_min_interval_secs = 1).await;
    let token = env.token(TokenApproval::Manual, 5);
    let a = agent("66666666-2222-4333-8444-555555555555");
    let (_, v) = env.enroll(&a, &token).await;
    let rid = v["request_id"].as_str().unwrap().to_string();
    let withdraw = |nonce: &str, key: &KeyPair| WithdrawRequest {
        request_id: rid.clone(),
        agent_id: a.id.clone(),
        nonce: nonce.to_string(),
        signature: sign_ecdsa_p256_sha256(
            &key.serialize_der(),
            &withdraw_signing_input(&rid, &a.id, nonce, &env.endpoint()).unwrap(),
        )
        .unwrap(),
        signature_alg: SignatureAlg::EcdsaP256Sha256,
        reason: WithdrawReason::UninstallPurge,
        initiated_by: InitiatedBy::Msi,
        hostname: "h".into(),
        pulse_version: "0.5.0".into(),
        sent_at: format_rfc3339_ms(env.now()),
        host_fingerprint: None,
    };
    // 別的私鑰的簽章：403，nonce 作廢
    let nonce = challenge(&env, &rid).await;
    let wrong = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let (s, v) = env
        .post(
            &format!("/v1/enroll/requests/{rid}/withdraw"),
            &withdraw(&nonce, &wrong),
        )
        .await;
    assert_eq!((s, code(&v)), (403, "withdraw_signature_invalid"), "{v}");
    let (s, v) = env
        .post(
            &format!("/v1/enroll/requests/{rid}/withdraw"),
            &withdraw(&nonce, &a.key),
        )
        .await;
    assert_eq!(
        (s, code(&v)),
        (400, "nonce_invalid"),
        "nonce 用過就作廢：{v}"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    let nonce = challenge(&env, &rid).await;
    let (s, v) = env
        .post(
            &format!("/v1/enroll/requests/{rid}/withdraw"),
            &withdraw(&nonce, &a.key),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let w: WithdrawResult = serde_json::from_value(v).unwrap();
    assert!(!w.already_withdrawn);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let nonce = challenge(&env, &rid).await;
    let (s, v) = env
        .post(
            &format!("/v1/enroll/requests/{rid}/withdraw"),
            &withdraw(&nonce, &a.key),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["already_withdrawn"], true);
    assert_eq!(v["withdrawn_at"], w.withdrawn_at.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limits_return_429_with_retry_after_and_distinct_invalid_tokens_block() {
    let env = Env::new("rate", |c| {
        c.enroll.max_requests_per_ip_burst = 3;
        c.enroll.max_requests_per_ip_per_minute = 1;
        c.enroll.max_invalid_tokens_per_ip = 5;
    })
    .await;
    for _ in 0..3 {
        let (s, _, _) = env.call("GET", "/v1/enroll/server-info", None, None).await;
        assert_eq!(s, 200);
    }
    let (s, headers, v) = env.call("GET", "/v1/enroll/server-info", None, None).await;
    assert_eq!((s, code(&v)), (429, "rate_limited"), "{v}");
    assert_eq!(v["error"]["scope"], "ip");
    let retry: u64 = headers
        .iter()
        .find(|(k, _)| k == "retry-after")
        .unwrap()
        .1
        .parse()
        .unwrap();
    assert!((1..=600).contains(&retry), "{retry}");
    assert_eq!(v["error"]["retry_after_ms"], retry * 1000);
    assert_eq!(v["error"]["observed_ip"], "127.0.0.1");

    // L7：在另一個環境（L4 寬鬆）以 5 個相異的無效 token 觸發封鎖；同一個無效值重複不算
    let env = Env::new("l7", |c| c.enroll.max_invalid_tokens_per_ip = 5).await;
    let same = kairos_proto::enroll::EnrollToken::from_random(&[1; 10]).canonical();
    for _ in 0..10 {
        let (s, v) = env
            .post("/v1/enroll/token-check", &json!({"token": same}))
            .await;
        assert_eq!((s, code(&v)), (403, "token_invalid"));
    }
    for i in 2..6u8 {
        let t = kairos_proto::enroll::EnrollToken::from_random(&[i; 10]).canonical();
        env.post("/v1/enroll/token-check", &json!({"token": t}))
            .await;
    }
    let (s, headers, v) = env
        .call(
            "POST",
            "/v1/enroll/token-check",
            Some(&json!({"token": same})),
            None,
        )
        .await;
    assert_eq!((s, code(&v)), (429, "source_blocked"), "{v}");
    assert_eq!(v["error"]["scope"], "blocked");
    let retry: u64 = headers
        .iter()
        .find(|(k, _)| k == "retry-after")
        .unwrap()
        .1
        .parse()
        .unwrap();
    assert!(retry > 800, "封鎖 15 分鐘：{retry}");
    // 封鎖寫入資料庫（重新啟動後仍有效）
    let n: i64 = env
        .conn()
        .query_row("SELECT count(*) FROM enroll_blocks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_switching_and_auto_pause() {
    let env = Env::new("mode", |c| {
        c.enroll.auto_pause_after_hours = 1;
    })
    .await;
    let svc = env.svc();
    // 沒有任何 token：開放後 1 小時自動暫停
    env.advance(3_600_000 + 1000);
    svc.wake();
    for _ in 0..100 {
        if svc.is_paused() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(svc.is_paused());
    let (_, _, v) = env.call("GET", "/v1/enroll/server-info", None, None).await;
    assert_eq!(v["accepts"]["new_enrollment"], false);
    let bogus = kairos_proto::enroll::EnrollToken::from_random(&[3; 10]).canonical();
    let (s, v) = env
        .post("/v1/enroll/token-check", &json!({"token": bogus}))
        .await;
    assert_eq!((s, code(&v)), (403, "enroll_not_accepted"));
    // 建立 token：立即解除
    let token = env.token(TokenApproval::Manual, 1);
    svc.wake();
    for _ in 0..100 {
        if !svc.is_paused() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!svc.is_paused());
    let (s, _) = env
        .post("/v1/enroll/token-check", &json!({"token": token}))
        .await;
    assert_eq!(s, 200);
    let audit = store::recent_audit(&env.conn(), 50).unwrap();
    assert!(audit.iter().any(|a| a.2 == "mode.auto_pause"));
    assert!(audit.iter().any(|a| a.2 == "mode.auto_resume"));

    // 僅續簽：新註冊 403，server-info 照常
    let mut conn = env.conn();
    let impact = ops::impact(&conn, 90, env.now()).unwrap();
    ops::set_mode(
        &mut conn,
        Some(EnrollMode::RenewOnly),
        None,
        None,
        "admin",
        None,
        None,
        &impact,
        "L1",
        env.now(),
    )
    .unwrap();
    svc.wake();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (s, v) = env
        .post("/v1/enroll/token-check", &json!({"token": token}))
        .await;
    assert_eq!((s, code(&v)), (403, "enroll_not_accepted"), "{v}");
    // 關閉：不監聽（連線被拒）
    ops::set_mode(
        &mut conn,
        Some(EnrollMode::Closed),
        None,
        None,
        "admin",
        None,
        Some("維護"),
        &impact,
        "L1",
        env.now(),
    )
    .unwrap();
    svc.wake();
    let mut refused = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", env.port))
            .await
            .is_err()
        {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(refused, "模式為關閉時不應監聽");
    assert!(!svc.listen.lock().unwrap().listening);
    // 改回開放：重新監聽（連接埠由作業系統重新分配）
    ops::set_mode(
        &mut conn,
        Some(EnrollMode::Open),
        None,
        None,
        "admin",
        None,
        None,
        &impact,
        "L1",
        env.now(),
    )
    .unwrap();
    svc.wake();
    let mut back = false;
    for _ in 0..100 {
        if svc.listen.lock().unwrap().listening {
            back = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(back);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_regenerates_the_crl_and_blocks_grace_renewal() {
    let env = Env::new("revoke", |_| {}).await;
    let token = env.token(TokenApproval::Auto, 5);
    let a = agent("77777777-2222-4333-8444-555555555555");
    let (_, v) = env.enroll(&a, &token).await;
    let serial = v["certificate"]["serial"].as_str().unwrap().to_string();
    let data = PathBuf::from(&env.config.storage.data_dir);
    let crl_path = crate::pki::issuing_crl_path(&data);
    let before = std::fs::read_to_string(&crl_path).unwrap();
    let mut conn = env.conn();
    ops::revoke(
        &mut conn,
        std::slice::from_ref(&serial),
        crate::pki::issue::Reason::KeyCompromise,
        "測試",
        "admin",
        None,
        env.now(),
    )
    .unwrap();
    env.svc().wake();
    let mut listed = false;
    for _ in 0..100 {
        let text = std::fs::read_to_string(&crl_path).unwrap_or_default();
        if text != before {
            let der = pem_decode_all(&text, Some("X509 CRL")).unwrap().remove(0).1;
            let (_, crl) = x509_parser::parse_x509_crl(&der).unwrap();
            listed = crl
                .iter_revoked_certificates()
                .any(|r| crate::pki::x509::serial_hex(r.raw_serial()) == serial);
            if listed {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(listed, "撤銷的序號應在 2 秒內進入 CRL");
    let c = ps::get_cert(&conn, &serial).unwrap().unwrap();
    assert!(c.crl_number.is_some());
    // 撤銷後不能再以過期續簽換憑證
    let old = c.cert_der;
    env.advance(370 * DAY);
    let new = a.rekey();
    let sent = format_rfc3339_ms(env.now());
    let input =
        grace_renew_signing_input(&a.id, &a.fingerprint, &new.csr, &sent, &env.endpoint()).unwrap();
    let req = GraceRenewRequest {
        agent_id: a.id.clone(),
        csr: b64url_encode(&new.csr),
        poll_secret: new.poll_secret.clone(),
        sent_at: sent,
        host_fingerprint: a.fingerprint.clone(),
        expired_certificate: b64url_encode(&old),
        signature: sign_ecdsa_p256_sha256(&a.key.serialize_der(), &input).unwrap(),
        signature_alg: SignatureAlg::EcdsaP256Sha256,
        hostname: "h".into(),
        pulse_version: "0.5.0".into(),
    };
    let (s, v) = env.post("/v1/enroll/renewals", &req).await;
    assert_eq!((s, code(&v)), (403, "certificate_revoked"), "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_errors_have_the_spec_shape() {
    let env = Env::new("errors", |_| {}).await;
    let (s, _, v) = env.call("GET", "/v1/enroll/nope", None, None).await;
    assert_eq!((s, code(&v)), (404, "not_found"));
    let (s, h, v) = env.call("GET", "/v1/enroll/token-check", None, None).await;
    assert_eq!((s, code(&v)), (405, "method_not_allowed"));
    assert!(h.iter().any(|(k, v)| k == "allow" && v == "POST"));
    let (s, _, v) = env.call("GET", "/v1/enroll/server-info/", None, None).await;
    assert_eq!(s, 404, "{v}");
    let (s, v) = env.post("/v1/enroll/requests", &json!({"token": 1})).await;
    assert_eq!((s, code(&v)), (400, "bad_request"));
    // 下載：本版 404
    let (s, _, _) = env.call("GET", "/agent/install.sh", None, None).await;
    assert_eq!(s, 404);
    let (_, h, _) = env.call("GET", "/v1/enroll/server-info", None, None).await;
    assert!(
        h.iter()
            .any(|(k, v)| k == "cache-control" && v == "no-store")
    );
    assert!(b64url_decode("AA").is_ok());
    let _ = Path::new("x");
}
