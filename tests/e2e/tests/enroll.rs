//! 註冊通道（8444）的端對端測試（Kairos 端）：以實際的 `kairos` 執行檔與 CLI，測試用的 HTTPS 用戶端
//! 打 8444（請求以 `kairos-proto` 的型別組成、CSR 以 rcgen 產生），取得的憑證再以 mTLS 連上 8443。
//! 以實際的 `pulse enroll` 與 Pulse 服務跑的情境在 `enroll_pulse.rs`；兩者共用 `enroll_common`。
//!
//! 情境：申請 → 以 CLI 核准 → 長輪詢取回憑證 → 以該憑證連 8443（hello_ack 宣告 renew／deregister）；
//! 同一個 CSR 重送得到同一筆；8443 的正常輪替（同一 CSR 重送同一張）；撤銷後 5 秒內被斷線；除役；
//! 過期續簽（測試掛鉤縮短效期）、重放在重新啟動後仍被拒；時鐘偏差；模式切換；限流 429。

mod enroll_common;

use enroll_common::*;
use kairos_proto::enroll::{
    GraceRenewRequest, SignatureAlg, b64url_encode, format_rfc3339_ms, grace_renew_signing_input,
    sign_ecdsa_p256_sha256,
};
use kairos_proto::{ClientMsg, RenewRequest};
use kp_e2e::{WAIT, poll_until};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

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
        // rcgen 的 pem() 在 Windows 上用 CRLF，Kairos 回傳的是 LF：比較前統一換行
        env.pki.pem.replace("\r\n", "\n").trim()
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
    // 共用型別（kairos-proto 的 ClientMsg::RenewRequest）
    let renew = serde_json::to_value(ClientMsg::RenewRequest(Box::new(RenewRequest {
        seq: 2,
        ts: format_rfc3339_ms(now_ms()),
        request_id: "5b0e9c1a-8d27-4f63-a1e4-7c9d2b6f3e81".into(),
        csr: b64url_encode(&b.csr),
        sent_at: format_rfc3339_ms(now_ms()),
    })))
    .unwrap();
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
    c1.send(&deregister(&d.id, 2));
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
    c3.send(&deregister(&d.id, 2));
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
    // 憑證清單帶主機名稱（還沒連上 8443 的主機用申請的主機名稱；主控台 S14），q 也比對主機名稱
    let eserial = v["data"]["results"][0]["serial"]
        .as_str()
        .unwrap()
        .to_string();
    let read = token_of("read");
    let (s, v) = env.console("GET", "/api/v1/pki/certificates?q=E2E-HOST", &read, None);
    assert_eq!(s, 200, "{v}");
    let row = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["serial"] == eserial.as_str())
        .unwrap_or_else(|| panic!("清單中沒有 {eserial}：{v}"))
        .clone();
    assert_eq!(row["hostname"], "e2e-host", "{row}");
    assert!(row.get("group").is_some(), "{row}");
    let (s, v) = env.console(
        "GET",
        &format!("/api/v1/pki/certificates/{eserial}"),
        &read,
        None,
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["hostname"], "e2e-host", "{v}");
    let (s, v) = env.console(
        "GET",
        "/api/v1/pki/certificates?q=no-such-host",
        &read,
        None,
    );
    assert_eq!((s, v["page"]["total"].as_u64()), (200, Some(0)), "{v}");
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

/// 過期續簽的請求（以舊憑證的私鑰簽章，新的 CSR 為 `new`）
fn grace_request(
    old: &Agent,
    old_der: &[u8],
    new: &Agent,
    sent_at: i64,
    endpoint: &str,
) -> GraceRenewRequest {
    let sent = format_rfc3339_ms(sent_at);
    let input = grace_renew_signing_input(&old.id, FP, &new.csr, &sent, endpoint).unwrap();
    GraceRenewRequest {
        agent_id: old.id.clone(),
        csr: b64url_encode(&new.csr),
        poll_secret: new.poll_secret.clone(),
        sent_at: sent,
        host_fingerprint: FP.into(),
        expired_certificate: b64url_encode(old_der),
        signature: sign_ecdsa_p256_sha256(&old.key.serialize_der(), &input).unwrap(),
        signature_alg: SignatureAlg::EcdsaP256Sha256,
        hostname: "e2e-host".into(),
        pulse_version: "0.9.0".into(),
    }
}

fn der_of(pem: &str) -> Vec<u8> {
    CertificateDer::from_pem_slice(pem.as_bytes())
        .unwrap()
        .to_vec()
}

fn not_after_ms(v: &Value) -> i64 {
    kairos_proto::enroll::parse_rfc3339_ms(v["certificate"]["not_after"].as_str().unwrap()).unwrap()
}

fn sleep_until(ms: i64) {
    let wait = ms - now_ms();
    if wait > 0 {
        std::thread::sleep(Duration::from_millis(wait as u64));
    }
}

fn api_token(env: &Env, role: &str) -> String {
    let (code, p) = env.cli(&[
        "api-token",
        "create",
        "--name",
        role,
        "--role",
        role,
        "--json",
    ]);
    assert_eq!(code, 0, "{}", p.dump());
    serde_json::from_str::<Value>(p.stdout().trim()).unwrap()["data"]["token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn grace_period_is_enforced_and_an_override_extends_it() {
    // ENROLL.md 第 19.3 節 #14：憑證 4 秒效期、寬限期 6 秒（測試掛鉤，debug 建置才生效）。
    // 過期後、寬限期內續簽成功；超過寬限期 renew_grace_exceeded（附 grace_ended_at）；
    // 管理者「允許再次自動續簽」（POST /api/v1/pki/grace-overrides）之後成功
    let env = Env::new(
        "enroll-grace-14",
        "",
        &[
            ("KAIROS_TEST_CERT_LIFETIME_SECS", "4"),
            ("KAIROS_TEST_GRACE_SECS", "6"),
        ],
    );
    let mut kairos = env.start();
    let auto = env.token("auto");
    let endpoint = format!("localhost:{}", env.enroll_port);
    let a = agent("5a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d");
    let (s, v) = env.post("/v1/enroll/requests", &enroll_request(&a, &auto, now_ms()));
    assert_eq!((s, v["state"].as_str()), (200, Some("issued")), "{v}");
    let first = der_of(&cert_of(&v));
    // 1. 過期後、寬限期內：成功
    sleep_until(not_after_ms(&v) + 500);
    let b = agent(&a.id);
    let (s, v) = env.post(
        "/v1/enroll/renewals",
        &grace_request(&a, &first, &b, now_ms(), &endpoint),
    );
    assert_eq!((s, v["state"].as_str()), (200, Some("issued")), "{v}");
    let second = der_of(&cert_of(&v));
    // 2. 第二張過期且超過寬限期（4 秒效期 + 6 秒寬限）：renew_grace_exceeded
    sleep_until(not_after_ms(&v) + 6_000 + 1_000);
    let c = agent(&a.id);
    let (s, v) = env.post(
        "/v1/enroll/renewals",
        &grace_request(&b, &second, &c, now_ms(), &endpoint),
    );
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (403, Some("renew_grace_exceeded")),
        "{v}"
    );
    assert!(v["error"]["grace_ended_at"].is_string(), "{v}");
    // 3. 允許再次自動續簽（GraceOverride；L1，原因必填）後同一台主機可以續簽
    let admin = api_token(&env, "admin");
    let until = format_rfc3339_ms(now_ms() + 3_600_000);
    let (s, v) = env.console(
        "POST",
        "/api/v1/pki/grace-overrides",
        &admin,
        Some(&json!({"agent_ids": [a.id], "until": until, "reason": "e2e：產線停機超過寬限期"})),
    );
    assert_eq!(s, 200, "{v}");
    let (s, v) = env.post(
        "/v1/enroll/renewals",
        &grace_request(&b, &second, &c, now_ms() + 1, &endpoint),
    );
    assert_eq!((s, v["state"].as_str()), (200, Some("issued")), "{v}");
    assert_eq!(
        env.certificates_of(&a.id).len(),
        3,
        "{:?}",
        env.certificates_of(&a.id)
    );
    kairos.stop();
}

#[test]
fn manually_issued_certificate_is_imported_and_revocation_takes_effect() {
    // PKI.md 第 16 節：根 CA（auth.client_ca）直接簽發的既有憑證（手動 PKI 的部署）可以連線；
    // kairos pki certificates import 匯入簽發紀錄後出現在清冊；在 Kairos 撤銷後，下一次 hello 被拒
    let env = Env::new("enroll-import", "", &[]);
    let mut kairos = env.start();
    let a = agent("6a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d");
    let pem = env.pki.sign_client(&a.id, &a.key);
    std::fs::create_dir_all(env.dir.join("manual")).unwrap();
    std::fs::write(env.dir.join("manual").join("host.pem"), &pem).unwrap();
    // 匯入前：以 auth.client_ca 驗證，可以連線
    let mut c = Agent8443::connect(&env, &pem, &a.key.serialize_pem());
    assert_eq!(c.hello(&a.id, None)["type"], "hello_ack");
    drop(c);
    let (code, p) = env.cli(&["pki", "certificates", "import", "manual", "--json"]);
    assert_eq!(code, 0, "{}", p.dump());
    let v: Value = serde_json::from_str(p.stdout().trim()).unwrap();
    assert_eq!(v["schema"], "kairos.pki.certificates.import/v1");
    let r = &v["data"]["results"][0];
    assert_eq!(r["result"], "imported", "{v}");
    assert_eq!(r["agent_id"], a.id.as_str());
    let serial = r["serial"].as_str().unwrap().to_string();
    let certs = env.certificates_of(&a.id);
    assert!(
        certs.iter().any(|c| c["serial"] == serial.as_str()),
        "{certs:?}"
    );
    // 同一張再匯入：already_imported（API）
    let token = api_token(&env, "operator");
    let (s, v) = env.console(
        "POST",
        "/api/v1/pki/certificates/import",
        &token,
        Some(&json!({"certificates": pem})),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["results"][0]["result"], "already_imported", "{v}");
    // 撤銷（CLI，L2）→ 2 秒內生效：hello 以 certificate_rejected 拒絕
    let (code, p) = env.cli(&[
        "pki",
        "revoke",
        &serial,
        "--reason",
        "keyCompromise",
        "--comment",
        "e2e",
        "--yes",
        "--expect-count",
        "1",
    ]);
    assert_eq!(code, 0, "{}", p.dump());
    let rejected = poll_until(WAIT, || {
        let mut c = Agent8443::connect(&env, &pem, &a.key.serialize_pem());
        c.hello(&a.id, None)["code"] == "certificate_rejected"
    });
    assert!(rejected, "撤銷後應拒絕 hello");
    // 日誌由背景的寫出執行緒送到 stdout，可能比回應晚一點出現：等到出現為止，不是立即檢查
    kairos.wait_for("KRS-W1105", WAIT);
    kairos.stop();
}

/// 8444 的 GET（每次一條連線），回傳 (狀態碼, 標頭（小寫名稱）, 原始本文)
fn raw_get(env: &Env, path: &str, extra: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    use std::io::{Read, Write};
    let mut s = env.tls(env.enroll_port, None);
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{extra}\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("沒有標頭");
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (status, headers, buf[split + 4..].to_vec())
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

#[test]
fn download_endpoint_serves_the_release_files() {
    // ENROLL.md 第 19.4 節第 3 項：enroll.download_dir 的發佈檔案經 8444 的 /agent/<檔名> 提供；
    // 路徑穿越與子目錄 404、內容類型、快取標頭、ETag 的 304；主控台的 downloads 顯示可用與 base_url
    let env = Env::new("enroll-downloads", "download_dir = \"dl\"\n", &[]);
    let dl = env.dir.join("dl");
    std::fs::create_dir_all(dl.join("sub")).unwrap();
    let deb: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dl.join("pulse-install.sh"), "#!/bin/sh\necho e2e\n").unwrap();
    std::fs::write(dl.join("pulse_0.9.0_amd64.deb"), &deb).unwrap();
    std::fs::write(dl.join("SHA256SUMS"), "0000  pulse_0.9.0_amd64.deb\n").unwrap();
    std::fs::write(dl.join("SHA256SUMS.sig"), "-----BEGIN SSH SIGNATURE-----\n").unwrap();
    std::fs::write(dl.join("sub").join("secret.txt"), "no").unwrap();
    let mut kairos = env.start();
    assert!(
        kairos.output().contains("下載端點 /agent/"),
        "{}",
        kairos.dump()
    );

    let (s, h, body) = raw_get(&env, "/agent/pulse_0.9.0_amd64.deb", "");
    assert_eq!(s, 200);
    assert_eq!(body, deb);
    assert_eq!(
        header(&h, "content-type"),
        Some("application/vnd.debian.binary-package")
    );
    assert_eq!(header(&h, "content-length"), Some("300000"));
    assert_eq!(header(&h, "cache-control"), Some("public, max-age=3600"));
    assert_eq!(header(&h, "x-content-type-options"), Some("nosniff"));
    let etag = header(&h, "etag").unwrap().to_string();
    let (s, _, body) = raw_get(
        &env,
        "/agent/pulse_0.9.0_amd64.deb",
        &format!("If-None-Match: {etag}\r\n"),
    );
    assert_eq!((s, body.len()), (304, 0));
    let (s, h, body) = raw_get(&env, "/agent/SHA256SUMS", "");
    assert_eq!(s, 200);
    assert_eq!(header(&h, "cache-control"), Some("no-cache"));
    assert_eq!(
        header(&h, "content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert!(String::from_utf8_lossy(&body).contains("pulse_0.9.0_amd64.deb"));
    let (s, h, _) = raw_get(&env, "/agent/pulse-install.sh", "");
    assert_eq!(s, 200);
    assert_eq!(
        header(&h, "content-type"),
        Some("text/x-shellscript; charset=utf-8")
    );
    // 路徑穿越、子目錄、百分比編碼、不存在、隱藏檔：一律 404
    for p in [
        "/agent/../kairos.toml",
        "/agent/sub/secret.txt",
        "/agent/sub",
        "/agent/%2e%2e%2fkairos.toml",
        "/agent/missing.deb",
        "/agent/.hidden",
    ] {
        let (s, _, _) = raw_get(&env, p, "");
        assert_eq!(s, 404, "{p}");
    }
    // 與安裝腳本相同的 curl 參數（--proto =https --tlsv1.2 --cacert）可以下載。
    // 安裝腳本只在 Unix 執行；Windows 內建的 curl（Schannel）對自建 CA 會做撤銷檢查而失敗
    // （CERT_TRUST_REVOCATION_STATUS_UNKNOWN）
    if cfg!(unix)
        && std::process::Command::new("curl")
            .arg("--version")
            .output()
            .is_ok()
    {
        let out = std::process::Command::new("curl")
            .args(["-fsSL", "--proto", "=https", "--tlsv1.2", "--cacert"])
            .arg(env.dir.join("root.pem"))
            .arg(format!(
                "https://localhost:{}/agent/pulse-install.sh",
                env.enroll_port
            ))
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "#!/bin/sh\necho e2e\n"
        );
    }
    // 主控台：downloads 可用（主控台據此顯示一行安裝指令）
    let token = api_token(&env, "auditor");
    let (s, v) = env.console("GET", "/api/v1/enroll/mode", &token, None);
    assert_eq!(s, 200, "{v}");
    let d = &v["data"]["downloads"];
    assert_eq!(d["available"], true, "{v}");
    assert_eq!(
        d["base_url"],
        format!("https://127.0.0.1:{}/agent", env.enroll_port)
    );
    assert!(
        d["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "pulse_0.9.0_amd64.deb")
    );
    // 僅續簽：不提供下載（403）
    let (code, p) = env.cli(&["enroll", "mode", "renew_only", "--yes"]);
    assert_eq!(code, 0, "{}", p.dump());
    let ok = poll_until(WAIT, || raw_get(&env, "/agent/SHA256SUMS", "").0 == 403);
    assert!(ok, "僅續簽時下載應回 403");
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
