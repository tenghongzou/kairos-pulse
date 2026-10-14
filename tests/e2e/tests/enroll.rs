//! 註冊通道（8444）的端對端測試（Kairos 端）：以實際的 `kairos` 執行檔與 CLI，測試用的 HTTPS 用戶端
//! 打 8444（請求以 `kairos-proto` 的型別組成、CSR 以 rcgen 產生），取得的憑證再以 mTLS 連上 8443。
//! 不依賴 Pulse 的新功能（Pulse 端的 `pulse enroll` 由另一個分支實作，合併後另加端對端情境）。
//!
//! 情境：申請 → 以 CLI 核准 → 長輪詢取回憑證 → 以該憑證連 8443（hello_ack 宣告 renew／deregister）；
//! 同一個 CSR 重送得到同一筆；8443 的正常輪替（同一 CSR 重送同一張）；撤銷後 5 秒內被斷線；除役；
//! 過期續簽（測試掛鉤縮短效期）、重放在重新啟動後仍被拒；時鐘偏差；模式切換；限流 429。

use kairos_proto::enroll::{
    EnrollRequest, GraceRenewRequest, ServerVerification, SignatureAlg, b64url_encode,
    format_rfc3339_ms, grace_renew_signing_input, sign_ecdsa_p256_sha256,
};
use kp_e2e::{Proc, TestDir, WAIT, free_port, poll_until, run_to_exit};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, SanType,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ENROLL_STARTED: &str = "註冊端點（8444）已啟動";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn t(secs: i64) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(secs).unwrap()
}

/// 測試的 PKI：P-384 根 CA（Pulse 信任的 CA）與 localhost 的伺服器憑證（8443、8444、主控台共用）
struct Pki {
    key: KeyPair,
    params: CertificateParams,
    pem: String,
}

impl Pki {
    fn new() -> Self {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let mut p = CertificateParams::default();
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, "E2E Root CA");
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let now = now_ms() / 1000;
        p.not_before = t(now - 86_400);
        p.not_after = t(now + 3650 * 86_400);
        let pem = p.self_signed(&key).unwrap().pem();
        Self {
            key,
            params: p,
            pem,
        }
    }

    fn server_cert(&self) -> (String, String) {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut p = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        p.subject_alt_names
            .push(SanType::IpAddress("127.0.0.1".parse().unwrap()));
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, "localhost");
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.is_ca = IsCa::ExplicitNoCa;
        let now = now_ms() / 1000;
        p.not_before = t(now - 86_400);
        p.not_after = t(now + 365 * 86_400);
        let cert = p
            .signed_by(&key, &Issuer::from_params(&self.params, &self.key))
            .unwrap();
        (cert.pem(), key.serialize_pem())
    }

    /// 以根 CA 簽發簽發 CA 的 CSR（INSTALL 的 issuing_ca_ext 範本）
    fn sign_issuing(&self, csr_pem: &str) -> String {
        let mut csr = CertificateSigningRequestParams::from_pem(csr_pem).unwrap();
        csr.params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        csr.params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        csr.params.use_authority_key_identifier_extension = true;
        let now = now_ms() / 1000;
        csr.params.not_before = t(now - 3600);
        csr.params.not_after = t(now + 5 * 365 * 86_400);
        csr.signed_by(&Issuer::from_params(&self.params, &self.key))
            .unwrap()
            .pem()
    }

    fn roots(&self) -> rustls::RootCertStore {
        let mut roots = rustls::RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(self.pem.as_bytes()) {
            roots.add(c.unwrap()).unwrap();
        }
        roots
    }
}

/// 準備好的 Kairos：8443、8444、主控台各一個隨機埠，已安裝線上簽發 CA
struct Env {
    root: TestDir,
    dir: PathBuf,
    pki: Pki,
    agent_port: u16,
    enroll_port: u16,
    console_port: u16,
    envs: Vec<(String, String)>,
}

fn distinct_ports(n: usize) -> Vec<u16> {
    let mut v: Vec<u16> = Vec::new();
    while v.len() < n {
        let p = free_port();
        if !v.contains(&p) {
            v.push(p);
        }
    }
    v
}

impl Env {
    fn new(name: &str, extra_enroll: &str, envs: &[(&str, &str)]) -> Env {
        let root = TestDir::new(name);
        let dir = root.sub("kairos");
        let pki = Pki::new();
        let (cert, key) = pki.server_cert();
        std::fs::write(dir.join("server.pem"), cert).unwrap();
        std::fs::write(dir.join("server.key"), key).unwrap();
        std::fs::write(dir.join("root.pem"), &pki.pem).unwrap();
        let ports = distinct_ports(3);
        std::fs::write(
            dir.join("kairos.toml"),
            format!(
                r#"[server]
listen = "127.0.0.1:{a}"
cert_path = "server.pem"
key_path = "server.key"
max_connections = 64
reload_interval_secs = 1

[storage]
data_dir = "data"

[auth]
client_ca = "root.pem"

[log]
level = "debug"

[console]
listen = "127.0.0.1:{c}"

[enroll]
mode = "open"
listen = "127.0.0.1:{e}"
{extra_enroll}

[pki]
mode = "online"
server_ca_path = "root.pem"
"#,
                a = ports[0],
                e = ports[1],
                c = ports[2],
            ),
        )
        .unwrap();
        let env = Env {
            root,
            dir,
            pki,
            agent_port: ports[0],
            enroll_port: ports[1],
            console_port: ports[2],
            envs: envs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        // 簽發 CA：Kairos 產生私鑰與 CSR → 以根 CA 簽發 → 安裝（第一個自動啟用）
        let (code, p) = env.cli(&["pki", "issuing-ca", "new", "--out", "issuing.csr"]);
        assert_eq!(code, 0, "{}", p.dump());
        let csr = std::fs::read_to_string(env.dir.join("issuing.csr")).unwrap();
        std::fs::write(env.dir.join("issuing.pem"), env.pki.sign_issuing(&csr)).unwrap();
        let (code, p) = env.cli(&["pki", "issuing-ca", "install", "issuing.pem"]);
        assert_eq!(code, 0, "{}", p.dump());
        assert!(p.stdout().contains("已啟用"), "{}", p.dump());
        let _ = &env.root;
        env
    }

    fn cli(&self, args: &[&str]) -> (i32, Proc) {
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["--config", "kairos.toml"]);
        let envs: Vec<(&str, &str)> = self
            .envs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        run_to_exit("kairos", &self.dir, &all, &envs)
    }

    fn start(&self) -> Proc {
        let envs: Vec<(&str, &str)> = self
            .envs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut p = Proc::spawn("kairos", &self.dir, &["--config", "kairos.toml"], &envs);
        p.wait_for(ENROLL_STARTED, WAIT);
        p.wait_for(kp_e2e::KAIROS_STARTED, WAIT);
        p
    }

    fn token(&self, approval: &str) -> String {
        let mut args = vec![
            "enroll-token",
            "create",
            "--name",
            "e2e",
            "--group",
            "g",
            "--max-uses",
            "10",
            "--approval",
            approval,
            "--json",
        ];
        if approval == "auto" {
            args.extend(["--allow-cidr", "127.0.0.0/8"]);
        }
        let (code, p) = self.cli(&args);
        assert_eq!(code, 0, "{}", p.dump());
        let v: Value = serde_json::from_str(p.stdout().trim()).unwrap();
        assert_eq!(v["schema"], "kairos.enroll-token.create/v1");
        assert!(
            v["data"]["install_string"]
                .as_str()
                .unwrap()
                .starts_with("pulse-enroll:v1:")
        );
        v["data"]["token"].as_str().unwrap().to_string()
    }

    fn tls(
        &self,
        port: u16,
        client: Option<(&str, &str)>,
    ) -> rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream> {
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(self.pki.roots());
        let config = match client {
            Some((cert, key)) => builder
                .with_client_auth_cert(
                    CertificateDer::pem_slice_iter(cert.as_bytes())
                        .map(|c| c.unwrap())
                        .collect(),
                    PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap(),
                )
                .unwrap(),
            None => builder.with_no_client_auth(),
        };
        let conn = rustls::ClientConnection::new(
            Arc::new(config),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        tcp.set_read_timeout(Some(WAIT)).unwrap();
        rustls::StreamOwned::new(conn, tcp)
    }

    /// 8444 的請求（每次一條連線），回傳 (狀態碼, 標頭, 本文)
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        auth: Option<&str>,
    ) -> (u16, String, Value) {
        let mut s = self.tls(self.enroll_port, None);
        let body = body.map(Value::to_string).unwrap_or_default();
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
        s.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, rest) = text
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("沒有標頭：{text}"));
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (
            status,
            head.to_string(),
            serde_json::from_str(rest).unwrap_or(Value::Null),
        )
    }

    /// 主控台 API（HTTPS、Bearer token）
    fn console(&self, method: &str, path: &str, token: &str, body: Option<&Value>) -> (u16, Value) {
        let mut s = self.tls(self.console_port, None);
        let body = body.map(Value::to_string).unwrap_or_default();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {token}\r\n"
        );
        if method != "GET" {
            req.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        req.push_str("\r\n");
        req.push_str(&body);
        s.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, rest) = text
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("沒有標頭：{text}"));
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(rest).unwrap_or(Value::Null))
    }

    fn post(&self, path: &str, body: &impl ToJson) -> (u16, Value) {
        let (s, _, v) = self.call("POST", path, Some(&body.to_json()), None);
        (s, v)
    }
}

/// Pulse 的身分（測試用）
struct Agent {
    id: String,
    key: KeyPair,
    csr: Vec<u8>,
    poll_secret: String,
}

fn agent(id: &str) -> Agent {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let mut p = CertificateParams::new(vec![id.to_string()]).unwrap();
    p.distinguished_name = DistinguishedName::new();
    p.distinguished_name.push(DnType::CommonName, id);
    let csr = p.serialize_request(&key).unwrap().der().to_vec();
    let secret: Vec<u8> = (0..32u8)
        .map(|i| i.wrapping_mul(7).wrapping_add(id.len() as u8))
        .collect();
    Agent {
        id: id.to_string(),
        key,
        csr,
        poll_secret: b64url_encode(&secret),
    }
}

const FP: &str = "sha256:abababababababababababababababababababababababababababababababab";

fn enroll_request(a: &Agent, token: &str, sent_at: i64) -> EnrollRequest {
    EnrollRequest {
        token: token.to_string(),
        agent_id: a.id.clone(),
        csr: b64url_encode(&a.csr),
        poll_secret: a.poll_secret.clone(),
        sent_at: format_rfc3339_ms(sent_at),
        hostname: "e2e-host".into(),
        pulse_version: "0.5.0".into(),
        server_verification: ServerVerification::CaPin,
        display_name: None,
        tags: None,
        host_fingerprint: Some(FP.into()),
        os: None,
        previous_serial: None,
    }
}

/// 8443 的 NDJSON 連線
struct Agent8443 {
    reader: BufReader<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>,
}

impl Agent8443 {
    fn connect(env: &Env, cert_pem: &str, key_pem: &str) -> Self {
        Self {
            reader: BufReader::new(env.tls(env.agent_port, Some((cert_pem, key_pem)))),
        }
    }

    fn send(&mut self, v: &Value) {
        let s = self.reader.get_mut();
        s.write_all(format!("{v}\n").as_bytes()).unwrap();
        s.flush().unwrap();
    }

    fn recv(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(&line).ok(),
        }
    }

    fn hello(&mut self, agent_id: &str, intent: Option<&str>) -> Value {
        let mut h = json!({
            "type": "hello", "seq": 1, "proto": 1, "proto_min": 1, "proto_max": 2,
            "features": ["metrics", "renew", "deregister"],
            "agent_id": agent_id, "hostname": "e2e-host", "agent_version": "0.5.0",
            "heartbeat_interval_ms": 1000, "ts": format_rfc3339_ms(now_ms()),
            "host_fingerprint": FP,
        });
        if let Some(i) = intent {
            h["intent"] = json!(i);
        }
        self.send(&h);
        self.recv().expect("沒有收到 hello 的回應")
    }
}

fn cert_of(v: &Value) -> String {
    v["certificate"]["client_cert_pem"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn enroll_approve_connect_renew_revoke_and_deregister() {
    let env = Env::new(
        "enroll-flow",
        "max_requests_per_ip_burst = 40\nmax_requests_per_ip_per_minute = 60",
        &[],
    );
    let mut kairos = env.start();
    let token = env.token("manual");

    // server-info：8444 的鏈附上根 CA（只信任根 CA 的用戶端可以連上）
    let (s, _, info) = env.call("GET", "/v1/enroll/server-info", None, None);
    assert_eq!(s, 200, "{info}");
    assert_eq!(info["accepts"]["new_enrollment"], true);

    // 申請 → 待核准；同一個 CSR 重送得到同一筆
    let a = agent("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d");
    let (s, v) = env.post("/v1/enroll/requests", &enroll_request(&a, &token, now_ms()));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "pending_approval");
    let rid = v["request_id"].as_str().unwrap().to_string();
    let code = v["request_code"].as_str().unwrap().to_string();
    let (s, again) = env.post("/v1/enroll/requests", &enroll_request(&a, &token, now_ms()));
    assert_eq!(s, 200, "{again}");
    assert_eq!(again["request_id"], rid.as_str());

    // 以 CLI 依申請編號核准（Kairos 執行中；常駐行程在 2 秒內簽發），長輪詢取回
    let (c, p) = env.cli(&["enroll", "approve", &code, "--yes"]);
    assert_eq!(c, 0, "{}", p.dump());
    let started = Instant::now();
    let (s, _, v) = env.call(
        "GET",
        &format!("/v1/enroll/requests/{rid}?wait=15"),
        None,
        Some(&a.poll_secret),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "issued", "{v}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let cert = cert_of(&v);
    let serial = v["certificate"]["serial"].as_str().unwrap().to_string();
    assert_eq!(
        v["certificate"]["ca_pem"].as_str().unwrap().trim(),
        env.pki.pem.trim()
    );

    // 以該憑證連 8443：hello_ack 宣告 renew／deregister 並帶寬限天數
    let mut conn = Agent8443::connect(&env, &cert, &a.key.serialize_pem());
    let ack = conn.hello(&a.id, None);
    assert_eq!(ack["type"], "hello_ack", "{ack}");
    let features: Vec<String> = serde_json::from_value(ack["features"].clone()).unwrap();
    assert!(
        features.contains(&"renew".to_string()) && features.contains(&"deregister".to_string())
    );
    assert_eq!(ack["expired_renew_grace_days"], 90);

    // 8443 的正常輪替；同一個 CSR 重送得到同一張
    let b = agent(&a.id);
    let renew = json!({"type": "renew_request", "seq": 2, "ts": format_rfc3339_ms(now_ms()),
        "request_id": "5b0e9c1a-8d27-4f63-a1e4-7c9d2b6f3e81", "csr": b64url_encode(&b.csr), "sent_at": format_rfc3339_ms(now_ms())});
    conn.send(&renew);
    let r1 = conn.recv().unwrap();
    assert_eq!(r1["type"], "renew_result", "{r1}");
    assert_eq!(r1["seq"], 2);
    let renewed = cert_of(&r1);
    let new_serial = r1["certificate"]["serial"].as_str().unwrap().to_string();
    assert_ne!(new_serial, serial);
    let mut renew2 = renew.clone();
    renew2["seq"] = json!(3);
    conn.send(&renew2);
    let r2 = conn.recv().unwrap();
    assert_eq!(r2["certificate"]["serial"], new_serial.as_str(), "{r2}");
    drop(conn);

    // 新憑證第一次 hello：舊憑證以 superseded 撤銷
    let mut conn = Agent8443::connect(&env, &renewed, &b.key.serialize_pem());
    assert_eq!(conn.hello(&a.id, None)["type"], "hello_ack");
    assert!(poll_until(WAIT, || {
        let (_, p) = env.cli(&["pki", "certificates", "list", "--json"]);
        let v: Value = serde_json::from_str(p.stdout().trim()).unwrap_or(Value::Null);
        v["data"]["certificates"].as_array().is_some_and(|l| {
            l.iter()
                .any(|c| c["serial"] == serial.as_str() && c["revocation_reason"] == "superseded")
        })
    }));

    // 撤銷使用中的憑證：5 秒內被斷線（certificate_rejected）
    let (c, p) = env.cli(&[
        "pki",
        "revoke",
        &new_serial,
        "--reason",
        "keyCompromise",
        "--comment",
        "e2e",
        "--yes",
        "--expect-count",
        "1",
    ]);
    assert_eq!(c, 0, "{}", p.dump());
    let t0 = Instant::now();
    let mut kicked = None;
    while t0.elapsed() < Duration::from_secs(10) {
        match conn.recv() {
            Some(m) if m["type"] == "error" => {
                kicked = Some(m);
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    let kicked = kicked.expect("撤銷後應收到 certificate_rejected");
    assert_eq!(kicked["code"], "certificate_rejected", "{kicked}");
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());

    // 另一台：自動核准、除役（intent: deregister），之後一般的 hello 被拒
    let auto = env.token("auto");
    let d = agent("1a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d");
    let (s, v) = env.post("/v1/enroll/requests", &enroll_request(&d, &auto, now_ms()));
    assert_eq!((s, v["state"].as_str()), (200, Some("issued")), "{v}");
    let dcert = cert_of(&v);
    let mut c1 = Agent8443::connect(&env, &dcert, &d.key.serialize_pem());
    assert_eq!(c1.hello(&d.id, Some("deregister"))["type"], "hello_ack");
    c1.send(&json!({"type": "deregister", "request_id": "6c0e9c1a-8d27-4f63-a1e4-7c9d2b6f3e81", "agent_id": d.id,
        "reason": "uninstall_purge", "initiated_by": "cli", "host_fingerprint": FP, "hostname": "e2e-host",
        "pulse_version": "0.5.0", "sent_at": format_rfc3339_ms(now_ms())}));
    let ack = c1.recv().unwrap();
    assert_eq!(ack["type"], "deregister_ack", "{ack}");
    assert_eq!(ack["already_decommissioned"], false);
    assert_eq!(ack["certificate_revoked"], true);
    // 10 分鐘內：一般的 hello 以 certificate_rejected 關閉；重送 deregister 得到 already_decommissioned
    let mut c2 = Agent8443::connect(&env, &dcert, &d.key.serialize_pem());
    let r = c2.hello(&d.id, None);
    assert_eq!(r["code"], "certificate_rejected", "{r}");
    let mut c3 = Agent8443::connect(&env, &dcert, &d.key.serialize_pem());
    assert_eq!(c3.hello(&d.id, Some("deregister"))["type"], "hello_ack");
    c3.send(&json!({"type": "deregister", "request_id": "6c0e9c1a-8d27-4f63-a1e4-7c9d2b6f3e81", "agent_id": d.id, "reason": "uninstall_purge"}));
    assert_eq!(c3.recv().unwrap()["already_decommissioned"], true);

    // 主控台 API：read 範圍不能核准（403），operate 可以；批次結果逐筆回報
    let token_of = |scope: &str| {
        let (c, p) = env.cli(&[
            "api-token",
            "create",
            "--name",
            scope,
            "--scope",
            scope,
            "--json",
        ]);
        assert_eq!(c, 0, "{}", p.dump());
        let v: Value = serde_json::from_str(p.stdout().trim()).unwrap();
        v["data"]["token"].as_str().unwrap().to_string()
    };
    let e = agent("3a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d");
    let (_, v) = env.post("/v1/enroll/requests", &enroll_request(&e, &token, now_ms()));
    let erid = v["request_id"].as_str().unwrap().to_string();
    let body = json!({"request_ids": [erid]});
    let (s, v) = env.console(
        "POST",
        "/api/v1/enroll/requests/approve",
        &token_of("read"),
        Some(&body),
    );
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (403, Some("forbidden")),
        "{v}"
    );
    assert_eq!(v["error"]["details"]["required"], "operate");
    let (s, v) = env.console(
        "POST",
        "/api/v1/enroll/requests/approve",
        &token_of("operate"),
        Some(&body),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["results"][0]["result"], "issued", "{v}");
    let (s, v) = env.console("GET", "/api/v1/enroll/mode", &token_of("read"), None);
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["state"], "open");
    assert_eq!(v["data"]["sources"]["enroll.mode"], "file");
    kairos.stop();
}

#[test]
fn grace_renewal_replay_survives_restart_and_clock_skew_is_rejected() {
    // 測試掛鉤：簽發的憑證只有 3 秒效期（debug 建置才生效）
    let env = Env::new(
        "enroll-grace",
        "",
        &[("KAIROS_TEST_CERT_LIFETIME_SECS", "3")],
    );
    let mut kairos = env.start();
    let auto = env.token("auto");
    let a = agent("2a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d");
    // 時鐘快 7 分鐘：clock_skew，token 未消耗
    let (s, v) = env.post(
        "/v1/enroll/requests",
        &enroll_request(&a, &auto, now_ms() + 7 * 60_000),
    );
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (400, Some("clock_skew")),
        "{v}"
    );
    let (s, v) = env.post("/v1/enroll/requests", &enroll_request(&a, &auto, now_ms()));
    assert_eq!((s, v["state"].as_str()), (200, Some("issued")), "{v}");
    let old_pem = cert_of(&v);
    let old_der = CertificateDer::from_pem_slice(old_pem.as_bytes())
        .unwrap()
        .to_vec();
    std::thread::sleep(Duration::from_secs(4));
    let endpoint = format!("localhost:{}", env.enroll_port);
    let new = agent(&a.id);
    let renew = |sent_at: i64| {
        let sent = format_rfc3339_ms(sent_at);
        let input = grace_renew_signing_input(&a.id, FP, &new.csr, &sent, &endpoint).unwrap();
        GraceRenewRequest {
            agent_id: a.id.clone(),
            csr: b64url_encode(&new.csr),
            poll_secret: new.poll_secret.clone(),
            sent_at: sent,
            host_fingerprint: FP.into(),
            expired_certificate: b64url_encode(&old_der),
            signature: sign_ecdsa_p256_sha256(&a.key.serialize_der(), &input).unwrap(),
            signature_alg: SignatureAlg::EcdsaP256Sha256,
            hostname: "e2e-host".into(),
            pulse_version: "0.5.0".into(),
        }
    };
    // 過期的憑證在 8443 被拒（握手失敗，或 hello 前就被關閉）
    let mut expired = Agent8443::connect(&env, &old_pem, &a.key.serialize_pem());
    let mut ok = false;
    expired.send(&json!({"type": "heartbeat", "seq": 1, "ts": "x"}));
    if expired.recv().is_none() {
        ok = true;
    }
    assert!(ok, "過期的憑證不應能連上 8443");
    let req = renew(now_ms());
    let (s, v) = env.post("/v1/enroll/renewals", &req);
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "issued");
    let serial = v["certificate"]["serial"].clone();
    // 同一個 CSR、新的簽章：同一張
    let (s, v) = env.post("/v1/enroll/renewals", &renew(now_ms() + 1));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["certificate"]["serial"], serial);
    // 原樣重送：重放；重新啟動後仍被拒
    let (s, v) = env.post("/v1/enroll/renewals", &req);
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (400, Some("replayed_request")),
        "{v}"
    );
    kairos.stop();
    let mut kairos = env.start();
    let (s, v) = env.post("/v1/enroll/renewals", &req);
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (400, Some("replayed_request")),
        "{v}"
    );
    kairos.stop();
}

#[test]
fn mode_switching_and_rate_limit() {
    let env = Env::new(
        "enroll-mode",
        "max_requests_per_ip_burst = 5\nmax_requests_per_ip_per_minute = 1",
        &[],
    );
    let mut kairos = env.start();
    // 限流：第 6 個 server-info 回 429＋Retry-After
    for _ in 0..5 {
        assert_eq!(env.call("GET", "/v1/enroll/server-info", None, None).0, 200);
    }
    let (s, head, v) = env.call("GET", "/v1/enroll/server-info", None, None);
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (429, Some("rate_limited")),
        "{v}"
    );
    assert!(head.to_ascii_lowercase().contains("retry-after:"), "{head}");
    // 僅續簽：模式立即生效（不必重新啟動）
    let (c, p) = env.cli(&["enroll", "mode", "renew_only", "--yes"]);
    assert_eq!(c, 0, "{}", p.dump());
    kairos.wait_for("註冊端點模式：open → renew_only", WAIT);
    // 關閉：停止監聽
    let (c, p) = env.cli(&["enroll", "mode", "closed", "--yes", "--reason", "e2e"]);
    assert_eq!(c, 0, "{}", p.dump());
    assert!(poll_until(WAIT, || std::net::TcpStream::connect((
        "127.0.0.1",
        env.enroll_port
    ))
    .is_err()));
    // 改回開放：重新監聽
    let (c, p) = env.cli(&["enroll", "mode", "open", "--yes"]);
    assert_eq!(c, 0, "{}", p.dump());
    assert!(poll_until(WAIT, || std::net::TcpStream::connect((
        "127.0.0.1",
        env.enroll_port
    ))
    .is_ok()));
    // 實際生效的設定：模式的來源是 console
    let (c, p) = env.cli(&["config", "show", "--effective", "--json"]);
    assert_eq!(c, 0, "{}", p.dump());
    let v: Value = serde_json::from_str(p.stdout().trim()).unwrap();
    assert_eq!(v["schema"], "kairos.config.effective/v1");
    let find = |k: &str| {
        v["data"]["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == k)
            .cloned()
            .unwrap()
    };
    let mode = find("enroll.mode");
    assert_eq!(mode["source"], "console");
    assert_eq!(mode["value"], "open");
    assert!(mode["updated_by"].is_string());
    assert_eq!(find("server.listen")["source"], "file");
    // 改用設定檔的值
    let (c, p) = env.cli(&["enroll", "mode", "renew_only", "--yes"]);
    assert_eq!(c, 0, "{}", p.dump());
    let (c, p) = env.cli(&["config", "show", "--effective", "enroll.mode"]);
    assert_eq!(c, 0, "{}", p.dump());
    assert!(p.stdout().contains("設定檔：\"open\""), "{}", p.stdout());
    let (c, p) = env.cli(&["config", "reset", "enroll.mode", "--yes"]);
    assert_eq!(c, 0, "{}", p.dump());
    let (_, p) = env.cli(&["config", "show", "--effective", "enroll.mode", "--json"]);
    let v: Value = serde_json::from_str(p.stdout().trim()).unwrap();
    assert_eq!(v["data"]["settings"][0]["source"], "file");
    let _ = Path::new("x");
    kairos.stop();
}

/// 請求本文（`kairos-proto` 的型別）轉成 JSON
trait ToJson {
    fn to_json(&self) -> Value;
}

impl ToJson for EnrollRequest {
    fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap()
    }
}

impl ToJson for GraceRenewRequest {
    fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap()
    }
}
