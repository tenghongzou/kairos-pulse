//! 註冊通道（8444）的端對端測試：實際的 `kairos` 與 `pulse` 執行檔（`pulse enroll`、Pulse 服務、
//! `pulse decommission`），對應 `crates/kairos-proto/ENROLL.md` 第 19.3 節的情境。
//!
//! 每個測試有自己的暫存目錄、Kairos（8443、8444、主控台各一個隨機埠）與 Pulse 主機目錄
//! （`pulse.toml`、`agent.id`、`identity/`、狀態檔），並行執行互不干擾。
//!
//! | # | 測試 |
//! |---|---|
//! | 1 | [`install_string_auto_approval_registers_and_the_service_connects`] |
//! | 2 | [`manual_approval_is_fetched_by_the_background_service`] |
//! | 3、4、5、10 | [`typo_wrong_pin_unknown_token_rejection_and_withdrawal`] |
//! | 4 | [`expired_exhausted_and_disabled_tokens_are_refused`] |
//! | 16 | [`offline_signing_export_sign_import_then_the_service_connects`] |
//! | 7 | [`lost_response_is_resent_and_creates_one_request`] |
//! | 8、9 | [`rate_limit_is_waited_out_and_renew_only_refuses_new_enrollments`] |
//! | 11、12 | [`expired_certificate_is_renewed_in_the_grace_period`] |
//! | 15、17 | [`revoked_host_is_disconnected_and_can_enroll_again_with_force`] |
//! | 18 | [`rotation_over_8443_supersedes_the_old_certificate`] |
//! | 19 | [`decommission_then_only_deregister_is_accepted`] |
//! | 20 | [`new_pulse_against_a_kairos_without_8444`] |
//!
//! #13（重放在 Kairos 重新啟動後仍被拒）、#6（時鐘偏差）在 `enroll.rs` 以測試用的 HTTPS 用戶端涵蓋
//! （需要原樣重送同一個請求、或改變送出時間，實際的 Pulse 不會這麼做）。

mod enroll_common;

use enroll_common::*;
use kairos_proto::enroll::EnrollToken;
use kp_e2e::{GOOD, PULSE_REGISTERED, TestDir, WAIT, free_port, pulse_testdata, test_pki};
use serde_json::Value;
use std::time::{Duration, Instant};

/// token 的秘密部分（不含 `KRT1-` 與連字號），用來檢查是否出現在檔案或日誌中
fn token_secret(token: &str) -> String {
    token
        .trim_start_matches("KRT1-")
        .chars()
        .filter(|c| *c != '-')
        .collect()
}

/// PEM 私鑰的本文（第一行 base64），用來檢查是否出現在日誌或狀態檔中
fn key_body(path: &std::path::Path) -> String {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .find(|l| !l.starts_with("-----") && !l.trim().is_empty())
        .unwrap()
        .to_string()
}

/// 一個格式正確（檢查碼相符）但 Kairos 不認得的 token
fn unknown_token() -> String {
    EnrollToken::from_random(&[0x5a; 10]).canonical()
}

fn request_code(doc: &Value) -> String {
    doc["data"]["enrollment"]["request_code"]
        .as_str()
        .unwrap_or_else(|| panic!("沒有申請編號：{doc}"))
        .to_string()
}

#[test]
fn install_string_auto_approval_registers_and_the_service_connects() {
    let env = Env::new("ep-auto", "", &[]);
    let mut kairos = env.start();
    let t = env.token_doc("auto", &[]);
    let token = t["token"].as_str().unwrap().to_string();
    let token_id = t["token_id"].as_str().unwrap().to_string();
    let install = t["install_string"].as_str().unwrap().to_string();
    assert!(install.starts_with("pulse-enroll:v1:"), "{t}");

    // pulse enroll --install-string：自動核准，當場取得憑證並以新憑證確認連上 8443
    let host = PulseHost::new(&env, "pulse");
    let (code, doc, p) = host.enroll(&["--install-string", &install]);
    assert_eq!(code, 0, "{}", p.dump());
    assert_eq!(doc["ok"], true, "{doc}");
    assert_eq!(doc["data"]["result"], "registered", "{doc}");
    assert_eq!(doc["data"]["config_updated"], true, "{doc}");
    let agent_id = host.agent_id();
    assert_eq!(doc["data"]["agent_id"], agent_id.as_str(), "{doc}");
    let serial = doc["data"]["certificate"]["serial"]
        .as_str()
        .unwrap()
        .to_ascii_uppercase();

    // identity/ 的檔案與權限：目錄 0700、私鑰與 enrollment.json 0600
    for f in ["client.pem", "client.key", "ca.pem", "enrollment.json"] {
        assert!(host.identity(f).is_file(), "缺少 identity/{f}");
    }
    if cfg!(unix) {
        assert_eq!(mode_of(&host.dir.join("identity")), Some(0o700));
        assert_eq!(mode_of(&host.identity("client.key")), Some(0o600));
        assert_eq!(mode_of(&host.identity("enrollment.json")), Some(0o600));
    }
    // Kairos 端：token 用了 1 次、1 筆已簽發的申請、清冊有這張憑證
    assert_eq!(env.token_used(&token_id), 1);
    let reqs = env.requests_of(&agent_id);
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0]["kind"], "enroll", "{reqs:?}");
    let certs = env.certificates_of(&agent_id);
    assert!(
        certs.iter().any(|c| c["serial"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(&serial))),
        "{certs:?}"
    );

    // 服務以新憑證連上 8443：hello 宣告 renew，Kairos 也列出；pulse status 為線上
    let mut svc = host.start();
    svc.wait_for(PULSE_REGISTERED, WAIT);
    let mut last = Value::Null;
    let online = kp_e2e::poll_until(WAIT, || {
        let (code, d) = host.status();
        last = d;
        code == 0 && last["data"]["connection"]["state"] == "registered"
    });
    assert!(online, "pulse status 應為線上：{last:#}\n{}", svc.dump());
    let features: Vec<String> =
        serde_json::from_value(last["data"]["connection"]["features"].clone()).unwrap();
    assert!(features.iter().any(|f| f == "renew"), "{last:#}");
    assert_eq!(last["data"]["connection"]["mtls"], true, "{last:#}");

    // 秘密不外洩：token、私鑰不出現在 Pulse 主機的任何檔案（私鑰檔本身除外）、Pulse 與 Kairos 的輸出；
    // 輪詢憑據只在 enrollment.json（0600）
    let secret = token_secret(&token);
    let key = key_body(&host.identity("client.key"));
    let poll_secret = host.enrollment()["poll_secret"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    for (path, text) in host.all_files_text() {
        assert!(!text.contains(&secret), "token 出現在 {}", path.display());
        if !path.ends_with("client.key") {
            assert!(!text.contains(&key), "私鑰出現在 {}", path.display());
        }
        if !poll_secret.is_empty() && !path.ends_with("enrollment.json") {
            assert!(
                !text.contains(&poll_secret),
                "輪詢憑據出現在 {}",
                path.display()
            );
        }
    }
    // Kairos 端：資料庫（含 WAL）與其他檔案只有 token 與輪詢憑據的雜湊；data/pki 0700、簽發 CA 私鑰 0600
    for (path, text) in files_text(&env.dir) {
        assert!(!text.contains(&secret), "token 出現在 {}", path.display());
        if !poll_secret.is_empty() {
            assert!(
                !text.contains(&poll_secret),
                "輪詢憑據出現在 {}",
                path.display()
            );
        }
    }
    if cfg!(unix) {
        let pki = env.dir.join("data").join("pki");
        assert_eq!(mode_of(&pki), Some(0o700));
        let keys: Vec<_> = std::fs::read_dir(&pki)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "key"))
            .collect();
        assert!(!keys.is_empty());
        for k in keys {
            assert_eq!(mode_of(&k), Some(0o600), "{}", k.display());
        }
    }
    for out in [p.output(), svc.output(), kairos.output()] {
        assert!(!out.contains(&secret), "token 出現在輸出中");
        assert!(!out.contains(&key), "私鑰出現在輸出中");
        if !poll_secret.is_empty() {
            assert!(!out.contains(&poll_secret), "輪詢憑據出現在輸出中");
        }
    }
    let (code, _) = svc.stop();
    assert_eq!(code, 0, "{}", svc.dump());
    kairos.stop();
}

#[test]
fn manual_approval_is_fetched_by_the_background_service() {
    let env = Env::new("ep-manual", "", &[]);
    let mut kairos = env.start();
    let t = env.token_doc("manual", &[]);
    let token = t["token"].as_str().unwrap();
    let pin = t["ca_pin"].as_str().unwrap();

    // 送出後立即返回：等待核准（結束代碼 7），申請編號與 Kairos 端相同
    let host = PulseHost::new(&env, "pulse");
    let (code, doc, p) = host.enroll_with(&env, token, pin, &["--no-wait"]);
    assert_eq!(code, 7, "{}", p.dump());
    assert_eq!(
        doc["data"]["enrollment"]["state"], "pending_approval",
        "{doc}"
    );
    let code_p = request_code(&doc);
    let agent_id = host.agent_id();
    let reqs = env.requests_of(&agent_id);
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0]["request_code"], code_p.as_str(), "{reqs:?}");
    assert_eq!(reqs[0]["state"], "pending_approval", "{reqs:?}");
    assert!(host.identity("request.key").is_file());
    assert!(!host.identity("client.pem").exists());

    // 服務在背景接續（長輪詢），尚未核准時不連 8443
    let mut svc = host.start();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(svc.count(PULSE_REGISTERED), 0, "{}", svc.dump());

    // 以 CLI 依申請編號核准：服務自動取回憑證並連線
    let (c, out) = env.cli(&["enroll", "approve", &code_p, "--yes"]);
    assert_eq!(c, 0, "{}", out.dump());
    let approved = Instant::now();
    svc.wait_for(PULSE_REGISTERED, WAIT);
    eprintln!("核准到連上 8443：{:?}", approved.elapsed());
    assert!(host.identity("client.pem").is_file());
    wait_until("enrollment.json 為 registered", || {
        host.enrollment()["state"] == "registered"
    });
    assert_eq!(host.enrollment()["request_code"], code_p.as_str());
    assert!(
        env.requests_of(&agent_id)[0]["state"]
            .as_str()
            .is_some_and(|s| s == "issued" || s == "delivered"),
        "{:?}",
        env.requests_of(&agent_id)
    );
    svc.stop();
    kairos.stop();
}

#[test]
fn typo_wrong_pin_unknown_token_rejection_and_withdrawal() {
    let env = Env::new("ep-reject", "", &[]);
    let mut kairos = env.start();
    let t = env.token_doc("manual", &[]);
    let token = t["token"].as_str().unwrap().to_string();
    let token_id = t["token_id"].as_str().unwrap().to_string();
    let pin = t["ca_pin"].as_str().unwrap().to_string();

    // #4 token 打錯一個字：本機以檢查碼攔下（PLS-E3001，結束代碼 2），不送出、不產生檔案
    let mut chars: Vec<char> = token.chars().collect();
    let last = chars.len() - 2;
    chars[last] = if chars[last] == '7' { '8' } else { '7' };
    let typo: String = chars.into_iter().collect();
    let host = PulseHost::new(&env, "typo");
    let (code, doc, p) = host.enroll_with(&env, &typo, &pin, &[]);
    assert_eq!(code, 2, "{}", p.dump());
    assert_eq!(doc["error"]["code"], "PLS-E3001", "{doc}");
    assert!(!host.identity("enrollment.json").exists());

    // #5 CA 指紋不符：PLS-E1406（結束代碼 5），沒有送出 token
    let wrong = format!("sha256:{}", "ab".repeat(32));
    let (code, doc, p) = host.enroll_with(&env, &token, &wrong, &[]);
    assert_eq!(code, 5, "{}", p.dump());
    assert_eq!(doc["error"]["code"], "PLS-E1406", "{doc}");

    // #4 格式正確、但 Kairos 不認得的 token：token_invalid（PLS-E3002，結束代碼 6），不產生檔案
    let (code, doc, p) = host.enroll_with(&env, &unknown_token(), &pin, &[]);
    assert_eq!(code, 6, "{}", p.dump());
    assert_eq!(doc["error"]["code"], "PLS-E3002", "{doc}");
    assert!(!host.identity("enrollment.json").exists());
    assert!(!host.identity("request.key").exists());
    // 以上都沒有建立申請、也沒有消耗 token
    assert!(env.requests().is_empty(), "{:?}", env.requests());
    assert_eq!(env.token_used(&token_id), 0);

    // #3 被拒絕：等待中的 pulse enroll 以結束代碼 6 結束，私鑰已刪除
    let rejected = PulseHost::new(&env, "rejected");
    let server = format!("localhost:{}", env.agent_port);
    let enroll = format!(":{}", env.enroll_port);
    let mut waiter = kp_e2e::Proc::spawn(
        "pulse",
        &rejected.dir,
        &[
            "enroll",
            "--server",
            &server,
            "--enroll-server",
            &enroll,
            "--token",
            &token,
            "--ca-pin",
            &pin,
            "--wait",
            "60",
            "--non-interactive",
            "--json",
        ],
        &[],
    );
    let mut code_r = String::new();
    wait_until("申請送達 Kairos", || {
        match std::fs::read_to_string(rejected.dir.join("agent.id")) {
            Ok(id) => match env.requests_of(id.trim()).first() {
                Some(r) => {
                    code_r = r["request_code"].as_str().unwrap_or_default().to_string();
                    true
                }
                None => false,
            },
            Err(_) => false,
        }
    });
    let (c, out) = env.cli(&[
        "enroll",
        "reject",
        &code_r,
        "--reason",
        "e2e",
        "--yes",
        "--expect-count",
        "1",
    ]);
    assert_eq!(c, 0, "{}", out.dump());
    let code = waiter.wait_exit(WAIT);
    assert_eq!(code, 6, "{}", waiter.dump());
    let doc: Value = serde_json::from_str(waiter.stdout().trim()).unwrap();
    assert_eq!(doc["data"]["enrollment"]["state"], "rejected", "{doc}");
    assert!(
        !rejected.identity("request.key").exists(),
        "被拒絕後私鑰應刪除"
    );

    // #10 撤回：等待中時 pulse enroll --cancel，Kairos 端為 withdrawn，本機私鑰刪除
    let withdrawn = PulseHost::new(&env, "withdrawn");
    let (code, _, p) = withdrawn.enroll_with(&env, &token, &pin, &["--no-wait"]);
    assert_eq!(code, 7, "{}", p.dump());
    let wid = withdrawn.agent_id();
    let (code, doc, p) = withdrawn.enroll(&["--cancel", "--yes"]);
    assert_eq!(code, 0, "{}", p.dump());
    assert_eq!(doc["ok"], true, "{doc}");
    let reqs = env.requests_of(&wid);
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0]["state"], "withdrawn", "{reqs:?}");
    assert!(!withdrawn.identity("request.key").exists());
    kairos.stop();
}

#[test]
fn lost_response_is_resent_and_creates_one_request() {
    // #7：Kairos 建立申請後不送回應（測試掛鉤），Pulse 以同一個 CSR 與輪詢憑據重送
    let env = Env::new("ep-lost", "", &[("KAIROS_TEST_DROP_ENROLL_RESPONSES", "1")]);
    let mut kairos = env.start();
    let t = env.token_doc("auto", &[]);
    let token_id = t["token_id"].as_str().unwrap().to_string();
    let host = PulseHost::new(&env, "pulse");
    let (code, doc, p) = host.enroll(&["--install-string", t["install_string"].as_str().unwrap()]);
    assert_eq!(code, 0, "{}", p.dump());
    assert_eq!(doc["data"]["result"], "registered", "{doc}");
    kairos.wait_for("測試掛鉤：丟棄 POST /v1/enroll/requests", WAIT);
    let agent_id = host.agent_id();
    let reqs = env.requests_of(&agent_id);
    assert_eq!(reqs.len(), 1, "只能有一筆申請：{reqs:?}");
    assert_eq!(env.token_used(&token_id), 1, "token 只扣一次");
    assert_eq!(env.certificates_of(&agent_id).len(), 1);
    kairos.stop();
}

#[test]
fn rate_limit_is_waited_out_and_renew_only_refuses_new_enrollments() {
    // #9：同一來源的請求超過 L4（容量 2、每秒 1 個）時 429＋Retry-After，Pulse 依指示等候後成功
    let env = Env::new(
        "ep-rate",
        "max_requests_per_ip_burst = 2\nmax_requests_per_ip_per_minute = 60",
        &[],
    );
    let mut kairos = env.start();
    let t = env.token_doc("auto", &[]);
    let host = PulseHost::new(&env, "pulse");
    let (code, doc, p) = host.enroll(&[
        "--install-string",
        t["install_string"].as_str().unwrap(),
        "--wait",
        "60",
    ]);
    assert_eq!(code, 0, "{}", p.dump());
    assert_eq!(doc["data"]["result"], "registered", "{doc}");
    assert!(
        kairos.output().contains("→ 429"),
        "應至少被限流一次\n{}",
        kairos.dump()
    );

    // #8：僅續簽模式不受理新註冊（enroll_not_accepted → PLS-E2008，結束代碼 6）
    let (c, out) = env.cli(&["enroll", "mode", "renew_only", "--yes"]);
    assert_eq!(c, 0, "{}", out.dump());
    kairos.wait_for("註冊端點模式：open → renew_only", WAIT);
    let other = PulseHost::new(&env, "other");
    let t2 = env.token_doc("auto", &[]);
    // 讓 L4 的額度回滿（server-info、token-check 各一個）
    std::thread::sleep(Duration::from_secs(3));
    let (code, doc, p) = other.enroll(&[
        "--install-string",
        t2["install_string"].as_str().unwrap(),
        "--wait",
        "60",
    ]);
    assert_eq!(code, 6, "{}", p.dump());
    assert_eq!(doc["error"]["code"], "PLS-E2008", "{doc}");
    assert!(!other.identity("request.key").exists());
    kairos.stop();
}

#[test]
fn expired_certificate_is_renewed_in_the_grace_period() {
    // #11：憑證只有 20 秒效期（測試掛鉤）；過期後啟動服務 → 8443 以 CertificateExpired 拒絕 →
    // 經 8444 以舊私鑰簽章續簽 → 以新憑證連上。#12：續簽的回應遺失一次，重送得到同一張（只有一筆續簽）
    let env = Env::new(
        "ep-grace",
        "",
        &[
            ("KAIROS_TEST_CERT_LIFETIME_SECS", "20"),
            ("KAIROS_TEST_DROP_RENEWAL_RESPONSES", "1"),
        ],
    );
    let mut kairos = env.start();
    let t = env.token_doc("auto", &[]);
    let host = PulseHost::new(&env, "pulse");
    let (code, doc, p) = host.enroll(&["--install-string", t["install_string"].as_str().unwrap()]);
    assert_eq!(code, 0, "{}", p.dump());
    let old_serial = doc["data"]["certificate"]["serial"]
        .as_str()
        .unwrap()
        .to_ascii_uppercase();
    let not_after = doc["data"]["certificate"]["not_after"]
        .as_str()
        .unwrap()
        .to_string();
    let expires = kairos_proto::enroll::parse_rfc3339_ms(&not_after).unwrap();
    let wait_ms = expires - now_ms() + 1_500;
    if wait_ms > 0 {
        std::thread::sleep(Duration::from_millis(wait_ms as u64));
    }

    // Pulse 的測試掛鉤：過期續簽的啟動分散與退避改為 2 秒（預設 0～120 秒、2 分鐘起）
    let mut svc = host.start_with(&[("PULSE_TEST_GRACE_DELAY_SECS", "2")]);
    svc.wait_for("via=grace", Duration::from_secs(60));
    svc.wait_for(PULSE_REGISTERED, WAIT);
    kairos.wait_for("測試掛鉤：丟棄 POST /v1/enroll/renewals", WAIT);
    let agent_id = host.agent_id();
    let grace: Vec<Value> = env
        .requests_of(&agent_id)
        .into_iter()
        .filter(|r| r["kind"] == "grace_renewal")
        .collect();
    assert_eq!(grace.len(), 1, "回應遺失後重送只會有一筆續簽：{grace:?}");
    assert_eq!(grace[0]["expired_serial"], old_serial.as_str(), "{grace:?}");
    assert!(
        env.certificates_of(&agent_id).len() >= 2,
        "{:?}",
        env.certificates_of(&agent_id)
    );
    svc.stop();
    kairos.stop();
}

#[test]
fn rotation_over_8443_supersedes_the_old_certificate() {
    // #18：簽發時 not_before 回溯 10 分鐘，5 分鐘效期的憑證一連上就到了輪替時點（已用 2/3）。
    // 服務在 8443 送 renew_request → 以新憑證另建連線成功後取代 → Kairos 把舊憑證以 superseded 撤銷
    let env = Env::new(
        "ep-rotate",
        "",
        &[("KAIROS_TEST_CERT_LIFETIME_SECS", "300")],
    );
    let mut kairos = env.start();
    let t = env.token_doc("auto", &[]);
    let host = PulseHost::new(&env, "pulse");
    let (code, doc, p) = host.enroll(&["--install-string", t["install_string"].as_str().unwrap()]);
    assert_eq!(code, 0, "{}", p.dump());
    let old_serial = doc["data"]["certificate"]["serial"]
        .as_str()
        .unwrap()
        .to_ascii_uppercase();
    let agent_id = host.agent_id();

    let mut svc = host.start();
    svc.wait_for("via=rotation", WAIT);
    svc.wait_for_count(PULSE_REGISTERED, 2, WAIT);
    wait_until("舊憑證以 superseded 撤銷", || {
        env.certificates_of(&agent_id).iter().any(|c| {
            c["serial"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&old_serial))
                && c["revocation_reason"] == "superseded"
        })
    });
    // 8443 的輪替申請（kind renewal）
    assert!(
        env.requests_of(&agent_id)
            .iter()
            .any(|r| r["kind"] == "renewal"),
        "{:?}",
        env.requests_of(&agent_id)
    );
    assert!(
        kairos.output().contains("經 8443 輪替憑證"),
        "{}",
        kairos.dump()
    );
    svc.stop();
    kairos.stop();
}

#[test]
fn decommission_then_only_deregister_is_accepted() {
    // #19：pulse decommission 經 8443（intent: deregister）除役；10 分鐘內一般的 hello 被拒，
    // 再次除役得到 already_decommissioned
    let env = Env::new("ep-decom", "", &[]);
    let mut kairos = env.start();
    let t = env.token_doc("auto", &[]);
    let host = PulseHost::new(&env, "pulse");
    let (code, _, p) = host.enroll(&["--install-string", t["install_string"].as_str().unwrap()]);
    assert_eq!(code, 0, "{}", p.dump());
    let agent_id = host.agent_id();
    let serial = host.cert_serial();
    let mut svc = host.start();
    svc.wait_for(PULSE_REGISTERED, WAIT);
    svc.stop();

    let decommission = || {
        let (code, p) = host.run(&["decommission", "--initiated-by", "install_script", "--json"]);
        let doc: Value =
            serde_json::from_str(p.stdout().trim()).unwrap_or_else(|e| panic!("{e}：{}", p.dump()));
        assert_eq!(doc["schema"], "pulse.decommission/v1", "{doc}");
        (code, doc["data"]["deregistration"].clone(), p)
    };
    let (code, d, p) = decommission();
    assert_eq!(code, 0, "{}", p.dump());
    assert_eq!(d["result"], "ok", "{d}");
    assert_eq!(d["certificate_revoked"], true, "{d}");
    assert_eq!(d["already_decommissioned"], false, "{d}");
    kairos.wait_for(&format!("agent {agent_id} 已自行除役"), WAIT);

    // 10 分鐘的視窗內：一般的 hello 以 certificate_rejected 關閉
    let mut again = host.start();
    kairos.wait_for(&format!("agent {agent_id} 已除役，拒絕 hello"), WAIT);
    assert_eq!(again.count(PULSE_REGISTERED), 0, "{}", again.dump());
    again.stop();
    // 重送除役：already_decommissioned
    let (code, d, p) = decommission();
    assert_eq!(code, 0, "{}", p.dump());
    assert_eq!(d["already_decommissioned"], true, "{d}");
    // 憑證已排定撤銷（cessationOfOperation，10 分鐘後生效）
    let certs = env.certificates_of(&agent_id);
    assert!(
        certs.iter().any(|c| c["serial"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(&serial))
            && c["revocation_reason"] == "cessationOfOperation"),
        "{certs:?}"
    );
    kairos.stop();
}

#[test]
fn revoked_host_is_disconnected_and_can_enroll_again_with_force() {
    // #17：撤銷線上主機的憑證 → CRL 重新產生，數秒內被斷線，重連在握手被拒；其他主機不受影響。
    // #15：以新的 token `pulse enroll --force` 重新註冊，申請帶 previous_serial
    let env = Env::new("ep-revoke", "", &[]);
    let mut kairos = env.start();
    let t = env.token_doc("auto", &[]);
    let host = PulseHost::new(&env, "pulse");
    let (code, _, p) = host.enroll(&["--install-string", t["install_string"].as_str().unwrap()]);
    assert_eq!(code, 0, "{}", p.dump());
    let other = PulseHost::new(&env, "other");
    let (code, _, p) = other.enroll(&["--install-string", t["install_string"].as_str().unwrap()]);
    assert_eq!(code, 0, "{}", p.dump());
    let agent_id = host.agent_id();
    let serial = host.cert_serial();
    let mut svc = host.start();
    let mut svc2 = other.start();
    svc.wait_for(PULSE_REGISTERED, WAIT);
    svc2.wait_for(PULSE_REGISTERED, WAIT);

    let (c, out) = env.cli(&[
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
    assert_eq!(c, 0, "{}", out.dump());
    let t0 = Instant::now();
    svc.wait_for("PLS-E1105", WAIT);
    eprintln!("撤銷到斷線：{:?}", t0.elapsed());
    assert!(t0.elapsed() < Duration::from_secs(15), "{:?}", t0.elapsed());
    // 另一台不受影響（只註冊過一次，沒有斷線重連）
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(svc2.count(PULSE_REGISTERED), 1, "{}", svc2.dump());
    svc.stop();
    svc2.stop();

    // 撤銷後以新 token 重新註冊（--force，沿用 agent_id、產生新私鑰）
    let t2 = env.token_doc("manual", &[]);
    let (code, doc, p) = host.enroll_with(
        &env,
        t2["token"].as_str().unwrap(),
        t2["ca_pin"].as_str().unwrap(),
        &["--force", "--yes", "--reason", "e2e", "--no-wait"],
    );
    assert_eq!(code, 7, "{}", p.dump());
    let code_f = request_code(&doc);
    let req = env
        .requests_of(&agent_id)
        .into_iter()
        .find(|r| r["request_code"] == code_f.as_str())
        .unwrap();
    assert_eq!(
        req["previous_serial"].as_str().map(str::to_ascii_uppercase),
        Some(serial),
        "{req}"
    );
    assert!(
        !req["flags"].as_array().unwrap().is_empty(),
        "撤銷後重新申請應有異常旗標：{req}"
    );
    kairos.stop();
}

#[test]
fn new_pulse_against_a_kairos_without_8444() {
    // #20：0.4 的 Kairos 沒有註冊端點。8444 沒有監聽 → PLS-E2007；該位址是別的 HTTPS 服務（不是註冊通道）
    // → PLS-E2106。手動憑證的既有部署不受影響
    let root = TestDir::new("ep-mixed");
    let kairos = kp_e2e::Kairos::new(&root, test_pki::CRL_EMPTY);
    let mut k = kairos.start();
    let manual = kp_e2e::Pulse::new(&root, "manual", kairos.port, GOOD);
    let mut m = manual.start(&[]);
    m.wait_for(PULSE_REGISTERED, WAIT);

    let ca_file = pulse_testdata("server-ca.pem");
    let ca_file = ca_file.to_str().unwrap();
    let server = format!("localhost:{}", kairos.port);
    let token = unknown_token();
    let host = PulseHost::in_root(&root, "new", "");
    let closed = format!(":{}", free_port());
    let (code, doc, p) = host.enroll(&[
        "--server",
        &server,
        "--enroll-server",
        &closed,
        "--token",
        &token,
        "--ca-file",
        ca_file,
    ]);
    assert_eq!(code, 5, "{}", p.dump());
    assert_eq!(doc["error"]["code"], "PLS-E2007", "{doc}");
    let console = format!(":{}", kairos.console_port);
    let (code, doc, p) = host.enroll(&[
        "--server",
        &server,
        "--enroll-server",
        &console,
        "--token",
        &token,
        "--ca-file",
        ca_file,
    ]);
    assert_eq!(code, 5, "{}", p.dump());
    assert_eq!(doc["error"]["code"], "PLS-E2106", "{doc}");
    assert!(!host.identity("request.key").exists());
    // 手動憑證的 Pulse 照常連線（只註冊過一次）
    assert_eq!(m.count(PULSE_REGISTERED), 1, "{}", m.dump());
    m.stop();
    k.stop();
}

#[test]
fn expired_exhausted_and_disabled_tokens_are_refused() {
    // #4：token 用完（PLS-E3004）、過期（PLS-E3003）、停用（對外不區分：PLS-E3002）；
    // token-check 被拒時不產生任何檔案（結束代碼 6）
    let env = Env::new("ep-tokens", "", &[]);
    let mut kairos = env.start();
    let refused = |host: &PulseHost, install: &str, code: &str| {
        let (c, doc, p) = host.enroll(&["--install-string", install]);
        assert_eq!(c, 6, "{}", p.dump());
        assert_eq!(doc["error"]["code"], code, "{doc}");
        assert!(!host.identity("enrollment.json").exists());
        assert!(!host.identity("request.key").exists());
    };

    let once = env.token_doc("auto", &["--max-uses", "1"]);
    let once_install = once["install_string"].as_str().unwrap();
    let (c, _, p) = PulseHost::new(&env, "first").enroll(&["--install-string", once_install]);
    assert_eq!(c, 0, "{}", p.dump());
    refused(&PulseHost::new(&env, "second"), once_install, "PLS-E3004");

    let soon = kairos_proto::enroll::format_rfc3339_ms(now_ms() + 2_000);
    let expiring = env.token_doc("auto", &["--expires-at", &soon]);
    std::thread::sleep(Duration::from_secs(3));
    refused(
        &PulseHost::new(&env, "expired"),
        expiring["install_string"].as_str().unwrap(),
        "PLS-E3003",
    );

    let disabled = env.token_doc("auto", &[]);
    let (c, out) = env.cli(&[
        "enroll-token",
        "disable",
        disabled["token_id"].as_str().unwrap(),
        "--reason",
        "e2e",
        "--yes",
    ]);
    assert_eq!(c, 0, "{}", out.dump());
    refused(
        &PulseHost::new(&env, "disabled"),
        disabled["install_string"].as_str().unwrap(),
        "PLS-E3002",
    );
    kairos.stop();
}

#[test]
fn offline_signing_export_sign_import_then_the_service_connects() {
    // #16：離線簽發模式。核准後仍待簽發 → 匯出 → 在「CA 主機」以 kairos pki sign-bundle 簽發
    // （不讀 Kairos 的設定與資料庫）→ 匯入 → Pulse 服務在下一次輪詢取得憑證並連上 8443
    let env = Env::offline("ep-offline");
    let mut kairos = env.start();
    let t = env.token_doc("manual", &[]);
    let host = PulseHost::new(&env, "pulse");
    let (code, doc, p) = host.enroll_with(
        &env,
        t["token"].as_str().unwrap(),
        t["ca_pin"].as_str().unwrap(),
        &["--no-wait"],
    );
    assert_eq!(code, 7, "{}", p.dump());
    let code_p = request_code(&doc);
    let (c, out) = env.cli(&["enroll", "approve", &code_p, "--yes"]);
    assert_eq!(c, 0, "{}", out.dump());
    let mut svc = host.start();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        svc.count(PULSE_REGISTERED),
        0,
        "核准後仍待簽發\n{}",
        svc.dump()
    );

    let (c, out) = env.cli(&["pki", "queue", "export", "--out", "queue.json"]);
    assert_eq!(c, 0, "{}", out.dump());
    let queue: Value =
        serde_json::from_str(&std::fs::read_to_string(env.dir.join("queue.json")).unwrap())
            .unwrap();
    assert_eq!(queue["items"].as_array().map(Vec::len), Some(1), "{queue}");
    let (c, out) = kp_e2e::run_to_exit(
        "kairos",
        &env.dir,
        &[
            "pki",
            "sign-bundle",
            "--in",
            "queue.json",
            "--ca-cert",
            "issuing.pem",
            "--ca-key",
            "issuing.key",
            "--out",
            "signed.json",
        ],
        &[],
    );
    assert_eq!(c, 0, "{}", out.dump());
    let (c, out) = env.cli(&["pki", "queue", "import", "signed.json"]);
    assert_eq!(c, 0, "{}", out.dump());
    svc.wait_for(PULSE_REGISTERED, WAIT);
    assert_eq!(host.enrollment()["request_code"], code_p.as_str());
    svc.stop();
    kairos.stop();
}
