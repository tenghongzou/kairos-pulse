//! 註冊通道（8444）端對端測試的共用環境：測試用 PKI、已安裝線上簽發 CA 的 Kairos（8443、8444、
//! 主控台各一個隨機埠，放在每個測試自己的暫存目錄）、8444／8443／主控台的測試用戶端，以及以實際的
//! `pulse` 執行檔註冊的主機（[`PulseHost`]）。
//!
//! `enroll.rs`（測試用 HTTPS 用戶端）與 `enroll_pulse.rs`（實際的 `pulse enroll` 與服務）共用。

#![allow(dead_code)]

use kairos_proto::enroll::{
    EnrollRequest, GraceRenewRequest, ServerVerification, b64url_encode, format_rfc3339_ms,
};
use kairos_proto::{ClientMsg, Deregister};
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ENROLL_STARTED: &str = "註冊端點（8444）已啟動";

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

pub fn t(secs: i64) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(secs).unwrap()
}

/// 測試的 PKI：P-384 根 CA（Pulse 信任的 CA）與 localhost 的伺服器憑證（8443、8444、主控台共用）
pub struct Pki {
    key: KeyPair,
    params: CertificateParams,
    pub pem: String,
}

impl Pki {
    pub fn new() -> Self {
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

    pub fn server_cert(&self) -> (String, String) {
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
    pub fn sign_issuing(&self, csr_pem: &str) -> String {
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

    /// 以根 CA 直接簽發用戶端憑證（手動簽發的既有部署：INSTALL 舊版以 openssl 的 client_ext 範本）
    pub fn sign_client(&self, agent_id: &str, key: &KeyPair) -> String {
        let mut p = CertificateParams::new(vec![agent_id.to_string()]).unwrap();
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, agent_id);
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.is_ca = IsCa::ExplicitNoCa;
        let now = now_ms() / 1000;
        p.not_before = t(now - 3600);
        p.not_after = t(now + 365 * 86_400);
        p.signed_by(key, &Issuer::from_params(&self.params, &self.key))
            .unwrap()
            .pem()
    }

    pub fn roots(&self) -> rustls::RootCertStore {
        let mut roots = rustls::RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(self.pem.as_bytes()) {
            roots.add(c.unwrap()).unwrap();
        }
        roots
    }
}

/// 準備好的 Kairos：8443、8444、主控台各一個隨機埠，已安裝線上簽發 CA
pub struct Env {
    pub root: TestDir,
    pub dir: PathBuf,
    pub pki: Pki,
    pub agent_port: u16,
    pub enroll_port: u16,
    pub console_port: u16,
    pub envs: Vec<(String, String)>,
}

pub fn distinct_ports(n: usize) -> Vec<u16> {
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
    pub fn new(name: &str, extra_enroll: &str, envs: &[(&str, &str)]) -> Env {
        Self::with(name, extra_enroll, "", envs)
    }

    /// 同 [`Env::new`]，`extra_toml` 加在設定檔最後（例如 `[pki.policy]`）
    pub fn with(name: &str, extra_enroll: &str, extra_toml: &str, envs: &[(&str, &str)]) -> Env {
        Self::build(name, extra_enroll, extra_toml, envs, false)
    }

    /// 離線簽發模式（`pki.mode = "offline"`，ENROLL.md 第 19.3 節 #16）：Kairos 沒有 CA 私鑰。
    /// 測試另以根 CA 簽發一個簽發 CA（`issuing.pem`、`issuing.key`，供 `kairos pki sign-bundle`），
    /// `auth.client_ca` 為根 CA 加上這個簽發 CA
    pub fn offline(name: &str) -> Env {
        Self::build(name, "", "", &[], true)
    }

    fn build(
        name: &str,
        extra_enroll: &str,
        extra_toml: &str,
        envs: &[(&str, &str)],
        offline: bool,
    ) -> Env {
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
client_ca = "{client_ca}"

[log]
level = "debug"

[console]
listen = "127.0.0.1:{c}"

[enroll]
mode = "open"
listen = "127.0.0.1:{e}"
# 待核准時的輪詢間隔用允許的最小值（預設 30 秒），核准後 Pulse 很快開始長輪詢
poll_after_base_secs = 5
{extra_enroll}

[pki]
mode = "{pki_mode}"
server_ca_path = "root.pem"

{extra_toml}
"#,
                a = ports[0],
                e = ports[1],
                c = ports[2],
                client_ca = if offline { "client-ca.pem" } else { "root.pem" },
                pki_mode = if offline { "offline" } else { "online" },
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
        if offline {
            // 離線的 CA 主機：簽發 CA 的私鑰不交給 Kairos
            let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
            let mut p = CertificateParams::default();
            p.distinguished_name = DistinguishedName::new();
            p.distinguished_name
                .push(DnType::CommonName, "E2E Offline Issuing CA");
            let csr = p.serialize_request(&key).unwrap().pem().unwrap();
            let issuing = env.pki.sign_issuing(&csr);
            std::fs::write(env.dir.join("issuing.pem"), &issuing).unwrap();
            std::fs::write(env.dir.join("issuing.key"), key.serialize_pem()).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(
                    env.dir.join("issuing.key"),
                    std::fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            }
            std::fs::write(
                env.dir.join("client-ca.pem"),
                format!("{}{}", env.pki.pem, issuing),
            )
            .unwrap();
            return env;
        }
        // 簽發 CA：Kairos 產生私鑰與 CSR → 以根 CA 簽發 → 安裝（第一個自動啟用）
        let (code, p) = env.cli(&["pki", "issuing-ca", "new", "--out", "issuing.csr"]);
        assert_eq!(code, 0, "{}", p.dump());
        let csr = std::fs::read_to_string(env.dir.join("issuing.csr")).unwrap();
        std::fs::write(env.dir.join("issuing.pem"), env.pki.sign_issuing(&csr)).unwrap();
        let (code, p) = env.cli(&["pki", "issuing-ca", "install", "issuing.pem"]);
        assert_eq!(code, 0, "{}", p.dump());
        assert!(p.stdout().contains("啟用中"), "{}", p.dump());
        let _ = &env.root;
        env
    }

    /// 以 `kairos pki init` 建立 PKI 的 Kairos（kairos/PKI.md 第 15 節）：根 CA（P-384）、localhost／127.0.0.1
    /// 的伺服器憑證與線上簽發 CA 都由 Kairos 產生，設定檔只照 init 印出的建議值填寫
    /// （`data/pki/server.pem`、`auth.client_ca = data/pki/issuing-cas.pem`、`pki.server_ca_path = data/pki/root-ca.pem`）
    pub fn from_pki_init(name: &str) -> (Env, Value) {
        let root = TestDir::new(name);
        let dir = root.sub("kairos");
        let ports = distinct_ports(3);
        let base = format!(
            "[server]\nlisten = \"127.0.0.1:{a}\"\nmax_connections = 64\nreload_interval_secs = 1\n\n[storage]\ndata_dir = \"data\"\n\n[log]\nlevel = \"debug\"\n\n[console]\nlisten = \"127.0.0.1:{c}\"\n",
            a = ports[0],
            c = ports[2],
        );
        std::fs::write(dir.join("kairos.toml"), &base).unwrap();
        let (code, p) = run_to_exit(
            "kairos",
            &dir,
            &[
                "pki",
                "init",
                "--name",
                "localhost",
                "--name",
                "127.0.0.1",
                "--organization",
                "E2E",
                "--root-dir",
                "root-ca",
                "--config",
                "kairos.toml",
                "--json",
            ],
            &[],
        );
        assert_eq!(code, 0, "{}", p.dump());
        let doc: Value = serde_json::from_str(p.stdout().trim()).unwrap();
        let cfg = &doc["data"]["config"];
        let s = |k: &str| cfg[k].as_str().unwrap().to_string();
        // 寫進 TOML 基本字串的路徑：Windows 的反斜線是跳脫字元，改成正斜線
        let t = |k: &str| s(k).replace('\\', "/");
        let with_cert = base.replace(
            "reload_interval_secs = 1\n",
            &format!(
                "reload_interval_secs = 1\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
                t("server.cert_path"),
                t("server.key_path")
            ),
        );
        std::fs::write(
            dir.join("kairos.toml"),
            format!(
                "{with_cert}\n[auth]\nclient_ca = \"{}\"\n\n[enroll]\nmode = \"open\"\nlisten = \"127.0.0.1:{}\"\npoll_after_base_secs = 5\n\n[pki]\nmode = \"online\"\nserver_ca_path = \"{}\"\n",
                t("auth.client_ca"),
                ports[1],
                t("pki.server_ca_path"),
            ),
        )
        .unwrap();
        let root_pem = std::fs::read_to_string(dir.join(s("pki.server_ca_path"))).unwrap();
        let env = Env {
            root,
            dir,
            pki: Pki {
                pem: root_pem,
                ..Pki::new()
            },
            agent_port: ports[0],
            enroll_port: ports[1],
            console_port: ports[2],
            envs: Vec::new(),
        };
        (env, doc)
    }

    pub fn cli(&self, args: &[&str]) -> (i32, Proc) {
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["--config", "kairos.toml"]);
        let envs: Vec<(&str, &str)> = self
            .envs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        run_to_exit("kairos", &self.dir, &all, &envs)
    }

    pub fn start(&self) -> Proc {
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

    pub fn token(&self, approval: &str) -> String {
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

    pub fn tls(
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
    pub fn call(
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
    pub fn console(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
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

    pub fn post(&self, path: &str, body: &impl ToJson) -> (u16, Value) {
        let (s, _, v) = self.call("POST", path, Some(&body.to_json()), None);
        (s, v)
    }
}

/// Pulse 的身分（測試用）
pub struct Agent {
    pub id: String,
    pub key: KeyPair,
    pub csr: Vec<u8>,
    pub poll_secret: String,
}

pub fn agent(id: &str) -> Agent {
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

pub const FP: &str = "sha256:abababababababababababababababababababababababababababababababab";

pub fn enroll_request(a: &Agent, token: &str, sent_at: i64) -> EnrollRequest {
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
pub struct Agent8443 {
    reader: BufReader<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>,
}

impl Agent8443 {
    pub fn connect(env: &Env, cert_pem: &str, key_pem: &str) -> Self {
        Self {
            reader: BufReader::new(env.tls(env.agent_port, Some((cert_pem, key_pem)))),
        }
    }

    pub fn send(&mut self, v: &Value) {
        let s = self.reader.get_mut();
        s.write_all(format!("{v}\n").as_bytes()).unwrap();
        s.flush().unwrap();
    }

    pub fn recv(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(&line).ok(),
        }
    }

    pub fn hello(&mut self, agent_id: &str, intent: Option<&str>) -> Value {
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

/// `deregister`（共用型別 kairos-proto 的 ClientMsg::Deregister）
pub fn deregister(agent_id: &str, seq: u64) -> Value {
    let now = format_rfc3339_ms(now_ms());
    serde_json::to_value(ClientMsg::Deregister(Box::new(Deregister {
        seq,
        ts: now.clone(),
        request_id: "6c0e9c1a-8d27-4f63-a1e4-7c9d2b6f3e81".into(),
        agent_id: agent_id.into(),
        reason: "uninstall_purge".into(),
        initiated_by: "cli".into(),
        host_fingerprint: Some(FP.into()),
        hostname: "e2e-host".into(),
        pulse_version: "0.5.0".into(),
        sent_at: now,
    })))
    .unwrap()
}

pub fn cert_of(v: &Value) -> String {
    v["certificate"]["client_cert_pem"]
        .as_str()
        .unwrap()
        .to_string()
}

/// 請求本文（`kairos-proto` 的型別）轉成 JSON
pub trait ToJson {
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

// ---------------------------------------------------------------------------
// Kairos 端的查詢（CLI 的 --json）
// ---------------------------------------------------------------------------

impl Env {
    /// `kairos enroll-token create … --json` 的 `data`（含 `token`、`install_string`、`ca_pin`）
    pub fn token_doc(&self, approval: &str, extra: &[&str]) -> Value {
        let mut args = vec![
            "enroll-token",
            "create",
            "--name",
            "e2e",
            "--group",
            "g",
            "--approval",
            approval,
            "--json",
        ];
        if !extra.contains(&"--max-uses") {
            args.extend(["--max-uses", "10"]);
        }
        if approval == "auto" {
            args.extend(["--allow-cidr", "127.0.0.0/8"]);
        }
        args.extend_from_slice(extra);
        let (code, p) = self.cli(&args);
        assert_eq!(code, 0, "{}", p.dump());
        let v: Value = serde_json::from_str(p.stdout().trim()).unwrap();
        v["data"].clone()
    }

    /// CLI 的 `--json` 文件的 `data`（結束代碼必須為 0）
    pub fn cli_data(&self, args: &[&str]) -> Value {
        let mut all = args.to_vec();
        all.push("--json");
        let (code, p) = self.cli(&all);
        assert_eq!(code, 0, "{}", p.dump());
        let v: Value = serde_json::from_str(p.stdout().trim())
            .unwrap_or_else(|e| panic!("不是 JSON（{e}）：{}", p.dump()));
        v["data"].clone()
    }

    /// 所有申請（進行中的與已結束的）
    pub fn requests(&self) -> Vec<Value> {
        let mut all = Vec::new();
        for args in [&["enroll", "list"][..], &["enroll", "list", "--closed"][..]] {
            all.extend(
                self.cli_data(args)["requests"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        all
    }

    /// 某個 agent_id 的所有申請
    pub fn requests_of(&self, agent_id: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r["agent_id"] == agent_id)
            .collect()
    }

    /// 所有 token（含已停用、用完的）
    pub fn tokens(&self) -> Vec<Value> {
        self.cli_data(&["enroll-token", "list", "--all"])["tokens"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// token 已使用的次數（以 `token_id` 查）
    pub fn token_used(&self, token_id: &str) -> u64 {
        self.tokens()
            .iter()
            .find(|t| t["token_id"] == token_id)
            .and_then(|t| t["used"].as_u64())
            .unwrap_or_else(|| panic!("找不到 token {token_id}"))
    }

    /// 某個 agent_id 的所有憑證（`kairos pki certificates list`）
    pub fn certificates_of(&self, agent_id: &str) -> Vec<Value> {
        self.cli_data(&["pki", "certificates", "list"])["certificates"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c["agent_id"] == agent_id)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 以實際的 pulse 執行檔註冊的主機
// ---------------------------------------------------------------------------

/// 一台以 `pulse enroll` 註冊的主機：`root/<name>` 為工作目錄（`pulse.toml`、`agent.id`、`identity/`、
/// 狀態檔都在其中，測試之間互不干擾）。設定只有測試用的短間隔；`[server]`、`[enroll]` 由
/// `pulse enroll` 寫入
pub struct PulseHost {
    pub dir: PathBuf,
}

/// Pulse 註冊成功（收到 hello_ack）的日誌
pub const REGISTERED: &str = "已向伺服器註冊";

impl PulseHost {
    pub fn new(env: &Env, name: &str) -> Self {
        Self::with_toml(env, name, "")
    }

    /// 同 [`PulseHost::new`]，`extra` 加在 `pulse.toml` 最後
    pub fn with_toml(env: &Env, name: &str, extra: &str) -> Self {
        Self::in_root(&env.root, name, extra)
    }

    /// 放在 `root/<name>`（沒有 [`Env`] 的測試，例如 0.4 的 Kairos）
    pub fn in_root(root: &TestDir, name: &str, extra: &str) -> Self {
        let dir = root.sub(name);
        std::fs::write(
            dir.join("pulse.toml"),
            format!(
                r#"# e2e：pulse enroll 會加上 [server]、[enroll]
[retry]
initial_delay = 1
max_delay = 2

[heartbeat]
interval = 1

[report]
interval = 1

[status]
file = "state/status.json"

[log]
level = "debug"
{extra}
"#
            ),
        )
        .unwrap();
        Self { dir }
    }

    /// `pulse enroll <args> --non-interactive --json`（最多等 `timeout`）：結束代碼、`pulse.enroll/v1`
    /// 文件與行程（stderr 有進度）
    pub fn enroll_within(&self, args: &[&str], timeout: Duration) -> (i32, Value, Proc) {
        let mut all = vec!["enroll"];
        all.extend_from_slice(args);
        all.extend(["--non-interactive", "--json"]);
        let mut p = Proc::spawn("pulse", &self.dir, &all, &[]);
        let code = p.wait_exit(timeout);
        let doc: Value = serde_json::from_str(p.stdout().trim()).unwrap_or(Value::Null);
        if !doc.is_null() {
            assert_eq!(doc["schema"], "pulse.enroll/v1", "{}", p.dump());
            assert_eq!(doc["exit_code"], code, "{}", p.dump());
        }
        (code, doc, p)
    }

    /// 同 [`PulseHost::enroll_within`]，最多等 30 秒
    pub fn enroll(&self, args: &[&str]) -> (i32, Value, Proc) {
        self.enroll_within(args, WAIT)
    }

    /// 以 `--server`／`--enroll-server`／`--token`／`--ca-pin` 註冊（不用安裝字串）
    pub fn enroll_with(
        &self,
        env: &Env,
        token: &str,
        pin: &str,
        extra: &[&str],
    ) -> (i32, Value, Proc) {
        let server = format!("localhost:{}", env.agent_port);
        let enroll = format!(":{}", env.enroll_port);
        let mut args = vec![
            "--server",
            server.as_str(),
            "--enroll-server",
            enroll.as_str(),
            "--token",
            token,
            "--ca-pin",
            pin,
        ];
        args.extend_from_slice(extra);
        self.enroll(&args)
    }

    /// 啟動 Pulse 服務（前景執行；工作目錄為主機目錄）
    pub fn start(&self) -> Proc {
        self.start_with(&[])
    }

    /// 同 [`PulseHost::start`]，另外指定環境變數（測試掛鉤）
    pub fn start_with(&self, envs: &[(&str, &str)]) -> Proc {
        Proc::spawn("pulse", &self.dir, &[], envs)
    }

    /// 執行其他子命令到結束
    pub fn run(&self, args: &[&str]) -> (i32, Proc) {
        run_to_exit("pulse", &self.dir, args, &[])
    }

    pub fn agent_id(&self) -> String {
        std::fs::read_to_string(self.dir.join("agent.id"))
            .unwrap()
            .trim()
            .to_string()
    }

    pub fn identity(&self, name: &str) -> PathBuf {
        self.dir.join("identity").join(name)
    }

    /// `identity/enrollment.json`（不存在時 `Null`）
    pub fn enrollment(&self) -> Value {
        std::fs::read_to_string(self.identity("enrollment.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null)
    }

    /// `identity/client.pem` 的序號（大寫十六進位；以 `enrollment.json` 的記錄為準）
    pub fn cert_serial(&self) -> String {
        self.enrollment()["cert_serial"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_uppercase()
    }

    /// `pulse status --json`
    pub fn status(&self) -> (i32, Value) {
        let (code, p) = self.run(&["status", "--json"]);
        let doc = serde_json::from_str(p.stdout().trim()).unwrap_or(Value::Null);
        (code, doc)
    }

    /// 這台主機目錄下所有檔案的內容（檢查秘密是否外洩用）
    pub fn all_files_text(&self) -> Vec<(PathBuf, String)> {
        files_text(&self.dir)
    }
}

/// 目錄下所有檔案的內容（遞迴；二進位檔如 SQLite 資料庫以 lossy 讀取），檢查秘密是否外洩用
pub fn files_text(dir: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if let Ok(b) = std::fs::read(&p) {
                out.push((p, String::from_utf8_lossy(&b).into_owned()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

/// 等到 `cond` 成立（最多 `WAIT`），逾時 panic 並附上 `what`
pub fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    assert!(poll_until(WAIT, &mut cond), "等待「{what}」逾時");
}

/// Unix 權限（其他平台 `None`）
pub fn mode_of(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Some(std::fs::metadata(path).ok()?.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}
