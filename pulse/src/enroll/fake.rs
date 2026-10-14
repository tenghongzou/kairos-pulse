//! 測試用的假 Kairos 8444 註冊端點（只在單元測試中編譯）。
//!
//! 依 `crates/kairos-proto/ENROLL.md` 與 `kairos-proto` 的共用型別實作最小的行為：路徑白名單、
//! `server-info`、`token-check`、新註冊（時鐘檢查、token、冪等、自動核准與人工核准）、輪詢與長輪詢、
//! 過期續簽（身分簽章、重放快取、冪等、寬限期）、撤回（nonce＋簽章）。真正的 Kairos 端實作完成後，
//! 以 `tests/e2e` 的端對端測試取代這裡的情境（見 Pulse README「與 Kairos 端合併」）。
//!
//! 刻意不實作：速率限制的計數（以 [`Script`] 直接指定回應）、持久化、主控台 API。

#![cfg(test)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kairos_proto::enroll::{
    self as ke, Accepts, CertPolicy, EnrollErrorCode as E, EnrollRequest, EnrollResult, ErrorBody,
    ErrorResponse, GraceRenewRequest, IssuedCertificate, RequestCode, RequestKind, RequestState,
    Route, ServerInfo, TokenApproval, TokenCheckRequest, TokenCheckResponse, TokenStatus,
    WithdrawChallenge, WithdrawRequest, WithdrawResult,
};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, PublicKeyData, SignatureAlgorithm,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_util::sync::CancellationToken;

use super::x509;

/// 伺服器憑證鏈的根 CA 演算法（新安裝為 P-384，既有部署可能是 RSA）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaKind {
    P256,
    P384,
    Rsa,
}

/// 指定下一次某個端點的回應
#[derive(Debug, Clone)]
pub enum Script {
    /// 本規格的錯誤回應（`retry_after` 秒）
    Error(E, Option<u64>),
    /// 非本規格的回應（例如沒有 8444 的舊版 Kairos 的 404 純文字）
    Plain(u16),
    /// 處理請求（建立申請等）後不回應、直接關閉連線（回應遺失）
    DropAfter,
}

#[derive(Debug, Clone)]
pub struct FakeToken {
    pub approval: TokenApproval,
    /// `valid`／`expired`／`exhausted`／`disabled`
    pub status: &'static str,
    pub remaining: u32,
    pub consumed: u32,
}

#[derive(Debug, Clone)]
pub struct FakeRequest {
    pub id: String,
    pub code: RequestCode,
    pub kind: RequestKind,
    pub agent_id: String,
    pub csr_sha: [u8; 32],
    pub poll_hash: [u8; 32],
    pub state: RequestState,
    pub spki: Vec<u8>,
    pub cert: Option<IssuedCertificate>,
    pub decided_by: Option<String>,
    pub reject_reason: Option<String>,
    pub expires_at: String,
}

/// 簽發過的用戶端憑證
#[derive(Debug, Clone)]
pub struct Issued {
    pub serial: String,
    pub spki: Vec<u8>,
    pub not_after_ms: i64,
    pub revoked: bool,
}

/// 假伺服器的狀態（測試直接讀寫）
pub struct FakeState {
    /// 假伺服器時鐘相對於系統時鐘的偏移（毫秒）
    pub clock_offset_ms: i64,
    pub tokens: HashMap<String, FakeToken>,
    pub requests: Vec<FakeRequest>,
    /// 依端點排定的回應（`server-info` 等的路徑名）
    pub script: HashMap<&'static str, VecDeque<Script>>,
    /// 各端點收到的請求數
    pub hits: HashMap<&'static str, u32>,
    pub enroll_features: Vec<String>,
    pub accepts: Accepts,
    pub poll_after_ms: u64,
    pub grace_days: u32,
    pub grace_requires_approval: bool,
    pub cert_lifetime_ms: i64,
    pub replay: HashSet<[u8; 32]>,
    pub issued: HashMap<String, Vec<Issued>>,
    pub host_fingerprints: HashMap<String, String>,
    /// 撤回的 nonce：`(nonce, request_id, 到期)`
    pub nonce: Option<(String, String, i64)>,
    /// `issued` 回應的 `ca_pem` 改送另一張 CA（測試中途替換）
    pub wrong_ca_pem: bool,
    /// 長輪詢中收到的 `wait`
    pub waits: Vec<u32>,
}

impl FakeState {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis() + self.clock_offset_ms
    }

    fn hit(&mut self, name: &'static str) {
        *self.hits.entry(name).or_default() += 1;
    }

    pub fn hits(&self, name: &str) -> u32 {
        self.hits.get(name).copied().unwrap_or(0)
    }

    fn scripted(&mut self, name: &'static str) -> Option<Script> {
        self.script.get_mut(name).and_then(VecDeque::pop_front)
    }

    pub fn push(&mut self, name: &'static str, s: Script) {
        self.script.entry(name).or_default().push_back(s);
    }

    pub fn add_token(&mut self, token: &str, approval: TokenApproval, status: &'static str) {
        let canonical = ke::EnrollToken::parse(token).unwrap().canonical();
        self.tokens.insert(
            canonical,
            FakeToken {
                approval,
                status,
                remaining: 10,
                consumed: 0,
            },
        );
    }

    pub fn token_consumed(&self, token: &str) -> u32 {
        let canonical = ke::EnrollToken::parse(token).unwrap().canonical();
        self.tokens.get(&canonical).map_or(0, |t| t.consumed)
    }

    pub fn request_mut(&mut self, id: &str) -> &mut FakeRequest {
        self.requests.iter_mut().find(|r| r.id == id).unwrap()
    }
}

/// 一把測試用的 P-256 公鑰（SPKI 的未壓縮點），讓 rcgen 以 CSR 的公鑰簽發憑證
struct P256Pub(Vec<u8>);

impl PublicKeyData for P256Pub {
    fn der_bytes(&self) -> &[u8] {
        &self.0
    }
    fn algorithm(&self) -> &'static SignatureAlgorithm {
        &PKCS_ECDSA_P256_SHA256
    }
}

/// 簽發 CA（簽發用戶端憑證）
struct Issuer {
    params: CertificateParams,
    key: KeyPair,
}

/// 假的 Kairos 8444
pub struct FakeKairos {
    /// `127.0.0.1:<port>`
    pub endpoint: String,
    /// 伺服器鏈中根 CA 的指紋（安裝字串的 `p`）
    pub ca_pin: String,
    pub ca_pem: String,
    pub state: Arc<Mutex<FakeState>>,
    shutdown: CancellationToken,
}

impl Drop for FakeKairos {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// 伺服器的選項
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub ca: CaKind,
    /// 伺服器鏈中不附上根 CA（測試 CA 指紋找不到）
    pub omit_ca_from_chain: bool,
    /// 伺服器憑證已過期（測試本機時鐘偏差的例外）
    pub server_cert_expired: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            ca: CaKind::P384,
            omit_ca_from_chain: false,
            server_cert_expired: false,
        }
    }
}

fn ca_params(cn: &str) -> CertificateParams {
    let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.distinguished_name = DistinguishedName::new();
    p.distinguished_name.push(DnType::CommonName, cn);
    p
}

impl FakeKairos {
    pub async fn start(opts: Options) -> Self {
        let (chain, server_key, ca_der) = server_chain(opts);
        let ca_info = x509::parse_cert(&ca_der).unwrap();
        let ca_pin = ke::ca_pin_from_spki(&ca_info.spki);
        let ca_pem = x509::cert_pem(&ca_der);
        let mut config = ServerConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, server_key)
        .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let state = Arc::new(Mutex::new(FakeState {
            clock_offset_ms: 0,
            tokens: HashMap::new(),
            requests: Vec::new(),
            script: HashMap::new(),
            hits: HashMap::new(),
            enroll_features: vec!["withdraw".into(), "grace_renew".into(), "long_poll".into()],
            accepts: Accepts {
                new_enrollment: true,
                grace_renewal: true,
            },
            poll_after_ms: 50,
            grace_days: 90,
            grace_requires_approval: false,
            cert_lifetime_ms: 365 * 86_400_000,
            replay: HashSet::new(),
            issued: HashMap::new(),
            host_fingerprints: HashMap::new(),
            nonce: None,
            wrong_ca_pem: false,
            waits: Vec::new(),
        }));
        let shutdown = CancellationToken::new();
        let ctx = Arc::new(Ctx {
            state: state.clone(),
            ca_pem: ca_pem.clone(),
            endpoint: endpoint.clone(),
        });
        let stop = shutdown.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = tokio::select! {
                    r = listener.accept() => match r { Ok(x) => x, Err(_) => return },
                    _ = stop.cancelled() => return,
                };
                let acceptor = acceptor.clone();
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Ok(tls) = acceptor.accept(tcp).await {
                        let _ = serve(tls, ctx).await;
                    }
                });
            }
        });
        Self {
            endpoint,
            ca_pin,
            ca_pem,
            state,
            shutdown,
        }
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap()
    }

    /// 主控台核准（線上簽發）
    pub fn approve(&self, request_id: &str) {
        let mut st = self.lock();
        let now = st.now_ms();
        let lifetime = st.cert_lifetime_ms;
        let r = st.request_mut(request_id).clone();
        let cert =
            self.issue_cert_from(&r.agent_id, &r.spki, now - 600_000, now + lifetime, &mut st);
        let r = st.request_mut(request_id);
        r.state = RequestState::Issued;
        r.cert = Some(cert);
        r.decided_by = Some("admin.chen".into());
    }

    pub fn reject(&self, request_id: &str, reason: &str) {
        let mut st = self.lock();
        let r = st.request_mut(request_id);
        r.state = RequestState::Rejected;
        r.decided_by = Some("admin.chen".into());
        r.reject_reason = Some(reason.into());
    }

    pub fn expire(&self, request_id: &str) {
        self.lock().request_mut(request_id).state = RequestState::RequestExpired;
    }

    /// 以簽發 CA 簽發一張用戶端憑證（測試準備「已註冊、憑證已過期」的主機）
    pub fn issue_for(
        &self,
        agent_id: &str,
        spki: &[u8],
        not_before_ms: i64,
        not_after_ms: i64,
    ) -> IssuedCertificate {
        let mut st = self.lock();
        self.issue_cert_from(agent_id, spki, not_before_ms, not_after_ms, &mut st)
    }

    fn issue_cert_from(
        &self,
        agent_id: &str,
        spki: &[u8],
        not_before_ms: i64,
        not_after_ms: i64,
        st: &mut FakeState,
    ) -> IssuedCertificate {
        issue(
            &ISSUER_FOR_TESTS,
            &self.ca_pem,
            agent_id,
            spki,
            not_before_ms,
            not_after_ms,
            st,
        )
    }
}

/// 測試準備與服務共用的簽發 CA（同一個行程內固定一把，讓 `issue_for` 簽出的憑證與伺服器簽發的
/// 由同一個 CA 簽發）
static ISSUER_FOR_TESTS: std::sync::LazyLock<Issuer> = std::sync::LazyLock::new(|| Issuer {
    params: ca_params("Fake Kairos Issuing CA"),
    key: KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap(),
});

struct Ctx {
    state: Arc<Mutex<FakeState>>,
    ca_pem: String,
    endpoint: String,
}

/// 伺服器憑證鏈、私鑰、根 CA
fn server_chain(
    opts: Options,
) -> (
    Vec<CertificateDer<'static>>,
    PrivateKeyDer<'static>,
    Vec<u8>,
) {
    if opts.ca == CaKind::Rsa {
        let leaf =
            CertificateDer::from_pem_slice(include_bytes!("../../testdata/enroll-rsa-server.pem"))
                .unwrap();
        let root =
            CertificateDer::from_pem_slice(include_bytes!("../../testdata/enroll-rsa-root.pem"))
                .unwrap();
        let key =
            PrivateKeyDer::from_pem_slice(include_bytes!("../../testdata/enroll-rsa-server.key"))
                .unwrap();
        let root_der = root.as_ref().to_vec();
        let mut chain = vec![leaf];
        if !opts.omit_ca_from_chain {
            chain.push(root);
        }
        return (chain, key, root_der);
    }
    let alg = match opts.ca {
        CaKind::P384 => &PKCS_ECDSA_P384_SHA384,
        _ => &PKCS_ECDSA_P256_SHA256,
    };
    let ca_key = KeyPair::generate_for(alg).unwrap();
    let ca = ca_params("Fake Kairos Root CA");
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let mut leaf =
        CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    if opts.server_cert_expired {
        leaf.not_before = rcgen::date_time_ymd(2020, 1, 1);
        leaf.not_after = rcgen::date_time_ymd(2021, 1, 1);
    }
    let issuer = rcgen::Issuer::new(ca, &ca_key);
    let leaf_cert = leaf.signed_by(&leaf_key, &issuer).unwrap();
    let mut chain = vec![leaf_cert.der().clone()];
    if !opts.omit_ca_from_chain {
        chain.push(ca_cert.der().clone());
    }
    let key = PrivateKeyDer::try_from(leaf_key.serialize_der()).unwrap();
    (chain, key, ca_cert.der().as_ref().to_vec())
}

fn issue(
    issuer: &Issuer,
    ca_pem: &str,
    agent_id: &str,
    spki: &[u8],
    not_before_ms: i64,
    not_after_ms: i64,
    st: &mut FakeState,
) -> IssuedCertificate {
    let point = ke::p256_point_from_spki(spki).unwrap().to_vec();
    let mut params = CertificateParams::new(vec![agent_id.to_string()]).unwrap();
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, agent_id);
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let serial: [u8; 16] = rand16();
    let mut serial = serial;
    serial[0] &= 0x7f;
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));
    let ts = |ms: i64| time::OffsetDateTime::from_unix_timestamp(ms / 1000).unwrap();
    params.not_before = ts(not_before_ms);
    params.not_after = ts(not_after_ms);
    let iss = rcgen::Issuer::from_params(&issuer.params, &issuer.key);
    let cert = params.signed_by(&P256Pub(point), &iss).unwrap();
    let serial_hex: String = serial.iter().map(|b| format!("{b:02X}")).collect();
    st.issued
        .entry(agent_id.to_string())
        .or_default()
        .push(Issued {
            serial: serial_hex.clone(),
            spki: spki.to_vec(),
            not_after_ms,
            revoked: false,
        });
    let ca_pem = if st.wrong_ca_pem {
        let other = ca_params("Some Other CA")
            .self_signed(&KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap())
            .unwrap();
        x509::cert_pem(other.der())
    } else {
        ca_pem.to_string()
    };
    IssuedCertificate {
        serial: serial_hex,
        not_before: ke::format_rfc3339_ms(not_before_ms / 1000 * 1000),
        not_after: ke::format_rfc3339_ms(not_after_ms / 1000 * 1000),
        client_cert_pem: x509::cert_pem(cert.der()),
        chain_pem: String::new(),
        ca_pem,
    }
}

fn rand16() -> [u8; 16] {
    use ring::rand::SecureRandom as _;
    let mut b = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut b).unwrap();
    b
}

/// HTTP 回應
struct Reply {
    status: u16,
    body: String,
    retry_after: Option<u64>,
    /// 不回應、直接關閉連線
    drop: bool,
}

fn json_reply<T: serde::Serialize>(status: u16, body: &T) -> Reply {
    Reply {
        status,
        body: serde_json::to_string(body).unwrap(),
        retry_after: None,
        drop: false,
    }
}

fn error_reply(st: &FakeState, code: E, f: impl FnOnce(&mut ErrorBody)) -> Reply {
    let mut body = ErrorBody::new(code, format!("fake: {}", code.as_str()));
    f(&mut body);
    let retry_after = body.retry_after_ms.map(|ms| ms.div_ceil(1000));
    let mut r = json_reply(
        code.http_status(),
        &ErrorResponse {
            server_time: ke::format_rfc3339_ms(st.now_ms()),
            error: body,
        },
    );
    r.retry_after = retry_after;
    r
}

async fn serve<S>(stream: S, ctx: Arc<Ctx>) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut io = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if io.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let mut headers = HashMap::new();
        loop {
            let mut h = String::new();
            io.read_line(&mut h).await?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let len: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; len];
        io.read_exact(&mut body).await?;
        let reply = handle(&ctx, &method, &target, &headers, &body).await;
        if reply.drop {
            return Ok(());
        }
        let mut out = format!(
            "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\n",
            reply.status,
            reply.body.len()
        );
        if let Some(ra) = reply.retry_after {
            out.push_str(&format!("Retry-After: {ra}\r\n"));
        }
        out.push_str("\r\n");
        out.push_str(&reply.body);
        io.get_mut().write_all(out.as_bytes()).await?;
        io.get_mut().flush().await?;
    }
}

fn route_name(r: &Route) -> &'static str {
    match r {
        Route::ServerInfo => "server-info",
        Route::TokenCheck => "token-check",
        Route::CreateRequest => "requests",
        Route::CreateRenewal => "renewals",
        Route::Poll(_) => "poll",
        Route::WithdrawChallenge(_) => "withdraw-challenge",
        Route::Withdraw(_) => "withdraw",
        Route::Download(_) => "download",
    }
}

async fn handle(
    ctx: &Ctx,
    method: &str,
    target: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> Reply {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let route = match ke::parse_route(method, path) {
        Ok(r) => r,
        Err(_) => {
            let st = ctx.state.lock().unwrap();
            return error_reply(&st, E::NotFound, |_| {});
        }
    };
    let name = route_name(&route);
    let script = {
        let mut st = ctx.state.lock().unwrap();
        st.hit(name);
        st.scripted(name)
    };
    let mut drop_after = false;
    match script {
        Some(Script::Error(code, ra)) => {
            let st = ctx.state.lock().unwrap();
            return error_reply(&st, code, |b| {
                if let Some(s) = ra {
                    b.retry_after_ms = Some(s * 1000);
                    b.scope = Some(ke::RateScope::Ip);
                    b.observed_ip = Some("127.0.0.1".into());
                }
            });
        }
        Some(Script::Plain(status)) => {
            return Reply {
                status,
                body: "not found".into(),
                retry_after: None,
                drop: false,
            };
        }
        Some(Script::DropAfter) => drop_after = true,
        None => {}
    }
    let mut reply = match route {
        Route::ServerInfo => server_info(ctx),
        Route::TokenCheck => token_check(ctx, body),
        Route::CreateRequest => create_request(ctx, body),
        Route::CreateRenewal => create_renewal(ctx, body),
        Route::Poll(id) => poll(ctx, &id, query, headers).await,
        Route::WithdrawChallenge(id) => withdraw_challenge(ctx, &id),
        Route::Withdraw(id) => withdraw(ctx, &id, body),
        Route::Download(_) => {
            let st = ctx.state.lock().unwrap();
            error_reply(&st, E::NotFound, |_| {})
        }
    };
    reply.drop = drop_after;
    reply
}

fn server_info(ctx: &Ctx) -> Reply {
    let st = ctx.state.lock().unwrap();
    json_reply(
        200,
        &ServerInfo {
            server_time: ke::format_rfc3339_ms(st.now_ms()),
            kairos_version: "0.5.0".into(),
            enroll_api: vec!["v1".into()],
            enroll_features: st.enroll_features.clone(),
            proto_min: 1,
            proto_max: 2,
            features: vec![
                "metrics".into(),
                "durable".into(),
                "renew".into(),
                "deregister".into(),
            ],
            accepts: st.accepts,
            observed_ip: "127.0.0.1".into(),
        },
    )
}

fn bad_request(st: &FakeState, field: &str) -> Reply {
    let field = field.to_string();
    error_reply(st, E::BadRequest, |b| b.field = Some(field))
}

fn token_check(ctx: &Ctx, body: &[u8]) -> Reply {
    let st = ctx.state.lock().unwrap();
    let Ok(req) = serde_json::from_slice::<TokenCheckRequest>(body) else {
        return bad_request(&st, "body");
    };
    let checked = match ke::validate_token_check(&req) {
        Ok(c) => c,
        Err(e) => return bad_request(&st, e.field),
    };
    if !st.accepts.new_enrollment {
        return error_reply(&st, E::EnrollNotAccepted, |_| {});
    }
    match st.tokens.get(&checked.token.canonical()) {
        None => error_reply(&st, E::TokenInvalid, |_| {}),
        Some(t) => match t.status {
            "expired" => error_reply(&st, E::TokenExpired, |b| {
                b.expired_at = Some("2026-10-08T15:59:00.000Z".into())
            }),
            "exhausted" => error_reply(&st, E::TokenExhausted, |_| {}),
            "disabled" => error_reply(&st, E::TokenInvalid, |_| {}),
            _ => json_reply(
                200,
                &TokenCheckResponse {
                    server_time: ke::format_rfc3339_ms(st.now_ms()),
                    token: TokenStatus {
                        name: "測試 token".into(),
                        approval: t.approval,
                        expires_at: ke::format_rfc3339_ms(st.now_ms() + 7 * 86_400_000),
                        remaining_uses: t.remaining,
                        default_tags: None,
                        source_allowed: None,
                    },
                    observed_ip: "127.0.0.1".into(),
                },
            ),
        },
    }
}

fn result_of(st: &FakeState, r: &FakeRequest) -> EnrollResult {
    let pending = r.state == RequestState::PendingApproval;
    EnrollResult {
        server_time: ke::format_rfc3339_ms(st.now_ms()),
        request_id: r.id.clone(),
        request_code: r.code,
        kind: r.kind,
        state: r.state,
        agent_id: r.agent_id.clone(),
        expires_at: pending.then(|| r.expires_at.clone()),
        poll_after_ms: pending.then_some(st.poll_after_ms),
        approval_detail: pending.then_some(ke::ApprovalDetail::AwaitingApproval),
        certificate: r.cert.clone().filter(|_| r.state == RequestState::Issued),
        policy: (r.state == RequestState::Issued).then_some(CertPolicy {
            cert_lifetime_days: 365,
            renew_before_days: 30,
            expired_renew_grace_days: st.grace_days,
        }),
        decided_at: r
            .decided_by
            .as_ref()
            .map(|_| ke::format_rfc3339_ms(st.now_ms())),
        decided_by: r.decided_by.clone(),
        reject_reason: r.reject_reason.clone(),
        observed_ip: Some("127.0.0.1".into()),
    }
}

fn clock_check(st: &FakeState, sent_at: &str) -> Option<Reply> {
    let sent = ke::parse_rfc3339_ms(sent_at).ok()?;
    let now = st.now_ms();
    let offset = ke::clock_offset_ms(sent, now);
    ke::exceeds_clock_skew(offset).then(|| {
        error_reply(st, E::ClockSkew, |b| {
            b.client_time = Some(sent_at.to_string());
            b.offset_ms = Some(offset);
            b.max_skew_ms = Some(ke::MAX_CLOCK_SKEW_MS);
        })
    })
}

fn new_request(
    st: &FakeState,
    kind: RequestKind,
    agent_id: &str,
    csr: &[u8],
    poll_secret: &[u8],
    spki: Vec<u8>,
) -> FakeRequest {
    FakeRequest {
        id: uuid::Uuid::new_v4().to_string(),
        code: ke::request_code_from_spki(&spki),
        kind,
        agent_id: agent_id.to_string(),
        csr_sha: ke::sha256(csr),
        poll_hash: ke::sha256(poll_secret),
        state: RequestState::PendingApproval,
        spki,
        cert: None,
        decided_by: None,
        reject_reason: None,
        expires_at: ke::format_rfc3339_ms(st.now_ms() + 7 * 86_400_000),
    }
}

fn create_request(ctx: &Ctx, body: &[u8]) -> Reply {
    let mut st = ctx.state.lock().unwrap();
    if !st.accepts.new_enrollment {
        return error_reply(&st, E::EnrollNotAccepted, |_| {});
    }
    let Ok(req) = serde_json::from_slice::<EnrollRequest>(body) else {
        return bad_request(&st, "body");
    };
    let checked = match ke::validate_enroll_request(&req) {
        Ok(c) => c,
        Err(e) => return bad_request(&st, e.field),
    };
    if let Some(r) = clock_check(&st, &req.sent_at) {
        return r;
    }
    let canonical = checked.token.canonical();
    let approval = match st.tokens.get(&canonical) {
        None => return error_reply(&st, E::TokenInvalid, |_| {}),
        Some(t) if t.status == "expired" => {
            return error_reply(&st, E::TokenExpired, |b| {
                b.expired_at = Some("2026-10-08T15:59:00.000Z".into())
            });
        }
        Some(t) if t.status == "exhausted" => return error_reply(&st, E::TokenExhausted, |_| {}),
        Some(t) if t.status == "disabled" => return error_reply(&st, E::TokenInvalid, |_| {}),
        Some(t) => t.approval,
    };
    // 冪等：同一 agent_id＋同一 CSR
    let csr_sha = ke::sha256(&checked.csr_der);
    if let Some(existing) = st
        .requests
        .iter()
        .find(|r| {
            r.agent_id == checked.agent_id && r.csr_sha == csr_sha && r.kind == RequestKind::Enroll
        })
        .cloned()
    {
        if existing.poll_hash == ke::sha256(&checked.poll_secret) {
            return json_reply(200, &result_of(&st, &existing));
        }
        return error_reply(&st, E::RequestConflict, |_| {});
    }
    let Some(spki) =
        x509::csr_spki(&checked.csr_der).filter(|s| ke::p256_point_from_spki(s).is_some())
    else {
        return error_reply(&st, E::InvalidCsr, |b| b.field = Some("csr".into()));
    };
    let has_san = checked
        .csr_der
        .windows(checked.agent_id.len())
        .any(|w| w == checked.agent_id.as_bytes());
    if !has_san {
        return error_reply(&st, E::InvalidCsr, |b| b.field = Some("csr".into()));
    }
    if let Some(fp) = &req.host_fingerprint {
        st.host_fingerprints
            .insert(checked.agent_id.clone(), fp.clone());
    }
    let mut r = new_request(
        &st,
        RequestKind::Enroll,
        &checked.agent_id,
        &checked.csr_der,
        &checked.poll_secret,
        spki.clone(),
    );
    if let Some(t) = st.tokens.get_mut(&canonical) {
        t.consumed += 1;
        t.remaining = t.remaining.saturating_sub(1);
    }
    if approval == TokenApproval::Auto {
        let now = st.now_ms();
        let lifetime = st.cert_lifetime_ms;
        let cert = issue(
            &ISSUER_FOR_TESTS,
            &ctx.ca_pem,
            &checked.agent_id,
            &spki,
            now - 600_000,
            now + lifetime,
            &mut st,
        );
        r.state = RequestState::Issued;
        r.cert = Some(cert);
        r.decided_by = Some("auto".into());
    }
    let result = result_of(&st, &r);
    st.requests.push(r);
    json_reply(200, &result)
}

async fn poll(ctx: &Ctx, id: &str, query: &str, headers: &HashMap<String, String>) -> Reply {
    let secret = headers
        .get("authorization")
        .and_then(|h| ke::parse_poll_authorization(h));
    let Some(wait) = ke::parse_wait_secs(Some(query)) else {
        let st = ctx.state.lock().unwrap();
        return bad_request(&st, "wait");
    };
    ctx.state.lock().unwrap().waits.push(wait);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(wait));
    loop {
        {
            let st = ctx.state.lock().unwrap();
            let found = st
                .requests
                .iter()
                .find(|r| r.id == id && secret.as_deref().map(ke::sha256) == Some(r.poll_hash));
            let Some(r) = found else {
                return error_reply(&st, E::RequestNotFound, |_| {});
            };
            let long = st.enroll_features.iter().any(|f| f == "long_poll");
            if r.state != RequestState::PendingApproval
                || !long
                || tokio::time::Instant::now() >= deadline
            {
                return json_reply(200, &result_of(&st, r));
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn create_renewal(ctx: &Ctx, body: &[u8]) -> Reply {
    let mut st = ctx.state.lock().unwrap();
    let Ok(req) = serde_json::from_slice::<GraceRenewRequest>(body) else {
        return bad_request(&st, "body");
    };
    let checked = match ke::validate_grace_renew_request(&req) {
        Ok(c) => c,
        Err(e) => return bad_request(&st, e.field),
    };
    if let Some(r) = clock_check(&st, &req.sent_at) {
        return r;
    }
    // 身分：過期憑證的 SAN、公鑰與簽章（以每一個註冊端點名稱組出簽章內容）、主機指紋
    let Some(old) = x509::parse_cert(&checked.expired_certificate_der) else {
        return error_reply(&st, E::RenewIdentityMismatch, |_| {});
    };
    let names = [
        ctx.endpoint.clone(),
        ctx.endpoint.replace("127.0.0.1", "localhost"),
    ];
    let signed_ok = names.iter().any(|n| {
        ke::grace_renew_signing_input(
            &checked.agent_id,
            &req.host_fingerprint,
            &checked.csr_der,
            &req.sent_at,
            n,
        )
        .ok()
        .is_some_and(|input| {
            ke::verify_ecdsa_p256_sha256(&old.spki, &input, &req.signature).is_ok()
        })
    });
    let fp_ok = st.host_fingerprints.get(&checked.agent_id) == Some(&req.host_fingerprint);
    if old.dns_sans != [checked.agent_id.clone()] || !signed_ok || !fp_ok {
        return error_reply(&st, E::RenewIdentityMismatch, |_| {});
    }
    let input = ke::grace_renew_signing_input(
        &checked.agent_id,
        &req.host_fingerprint,
        &checked.csr_der,
        &req.sent_at,
        &ctx.endpoint,
    )
    .unwrap();
    let key = ke::replay_cache_key(&input);
    if !st.replay.insert(key) {
        return error_reply(&st, E::ReplayedRequest, |_| {});
    }
    // 冪等
    let csr_sha = ke::sha256(&checked.csr_der);
    if let Some(existing) = st
        .requests
        .iter()
        .find(|r| {
            r.agent_id == checked.agent_id
                && r.csr_sha == csr_sha
                && r.kind == RequestKind::GraceRenewal
        })
        .cloned()
    {
        return json_reply(200, &result_of(&st, &existing));
    }
    let latest = st
        .issued
        .get(&checked.agent_id)
        .and_then(|v| v.last())
        .cloned();
    let Some(latest) = latest else {
        return error_reply(&st, E::RenewIdentityMismatch, |_| {});
    };
    if latest.revoked {
        return error_reply(&st, E::CertificateRevoked, |_| {});
    }
    if latest.spki != old.spki {
        return error_reply(&st, E::RenewIdentityMismatch, |_| {});
    }
    let now = st.now_ms();
    if now < latest.not_after_ms {
        return error_reply(&st, E::CertificateNotExpired, |_| {});
    }
    let grace_end = latest.not_after_ms + i64::from(st.grace_days) * 86_400_000;
    if now > grace_end {
        return error_reply(&st, E::RenewGraceExceeded, |b| {
            b.grace_ended_at = Some(ke::format_rfc3339_ms(grace_end))
        });
    }
    let Some(spki) = x509::csr_spki(&checked.csr_der).filter(|s| *s != old.spki) else {
        return error_reply(&st, E::InvalidCsr, |b| b.field = Some("csr".into()));
    };
    let mut r = new_request(
        &st,
        RequestKind::GraceRenewal,
        &checked.agent_id,
        &checked.csr_der,
        &checked.poll_secret,
        spki.clone(),
    );
    if !st.grace_requires_approval {
        let lifetime = st.cert_lifetime_ms;
        let cert = issue(
            &ISSUER_FOR_TESTS,
            &ctx.ca_pem,
            &checked.agent_id,
            &spki,
            now - 600_000,
            now + lifetime,
            &mut st,
        );
        r.state = RequestState::Issued;
        r.cert = Some(cert);
        r.decided_by = Some("system".into());
    }
    let result = result_of(&st, &r);
    st.requests.push(r);
    json_reply(200, &result)
}

fn withdraw_challenge(ctx: &Ctx, id: &str) -> Reply {
    let mut st = ctx.state.lock().unwrap();
    let Some(r) = st.requests.iter().find(|r| r.id == id).cloned() else {
        return error_reply(&st, E::RequestNotFound, |_| {});
    };
    if matches!(
        r.state,
        RequestState::Rejected | RequestState::RequestExpired | RequestState::Replaced
    ) {
        return error_reply(&st, E::RequestNotPending, |b| b.state = Some(r.state));
    }
    let mut n = [0u8; 32];
    {
        use ring::rand::SecureRandom as _;
        ring::rand::SystemRandom::new().fill(&mut n).unwrap();
    }
    let nonce = ke::b64url_encode(&n);
    let expires = st.now_ms() + ke::WITHDRAW_NONCE_TTL_MS;
    st.nonce = Some((nonce.clone(), id.to_string(), expires));
    json_reply(
        200,
        &WithdrawChallenge {
            server_time: ke::format_rfc3339_ms(st.now_ms()),
            request_id: id.to_string(),
            nonce,
            nonce_expires_at: ke::format_rfc3339_ms(expires),
        },
    )
}

fn withdraw(ctx: &Ctx, id: &str, body: &[u8]) -> Reply {
    let mut st = ctx.state.lock().unwrap();
    let Ok(req) = serde_json::from_slice::<WithdrawRequest>(body) else {
        return bad_request(&st, "body");
    };
    if let Err(e) = ke::validate_withdraw_request(id, &req) {
        return bad_request(&st, e.field);
    }
    let nonce = st.nonce.take();
    match nonce {
        Some((n, rid, exp)) if n == req.nonce && rid == id => {
            if st.now_ms() > exp {
                return error_reply(&st, E::NonceExpired, |_| {});
            }
        }
        _ => return error_reply(&st, E::NonceInvalid, |_| {}),
    }
    let Some(r) = st.requests.iter().find(|r| r.id == id).cloned() else {
        return error_reply(&st, E::RequestNotFound, |_| {});
    };
    let ok = ke::withdraw_signing_input(id, &req.agent_id, &req.nonce, &ctx.endpoint)
        .ok()
        .is_some_and(|i| ke::verify_ecdsa_p256_sha256(&r.spki, &i, &req.signature).is_ok());
    if !ok || !req.agent_id.eq_ignore_ascii_case(&r.agent_id) {
        return error_reply(&st, E::WithdrawSignatureInvalid, |_| {});
    }
    let already = match r.state {
        RequestState::PendingApproval => false,
        RequestState::Withdrawn => true,
        s => return error_reply(&st, E::RequestNotPending, |b| b.state = Some(s)),
    };
    st.request_mut(id).state = RequestState::Withdrawn;
    json_reply(
        200,
        &WithdrawResult {
            server_time: ke::format_rfc3339_ms(st.now_ms()),
            request_id: id.to_string(),
            request_code: r.code,
            state: RequestState::Withdrawn,
            withdrawn_at: ke::format_rfc3339_ms(st.now_ms()),
            already_withdrawn: already,
        },
    )
}
