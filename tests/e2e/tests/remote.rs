//! 端對端：集中設定與遠端升級的 Kairos 端（F8 K1／K2；`crates/kairos-proto/REMOTE.md`）。
//!
//! 實際的 Kairos 執行檔（SQLite、mTLS、`[remote] allowed_signers` 為 `kairos-proto` 的測試向量金鑰），
//! 以主控台 API 建立設定檔與指派、匯入測試向量的發佈（`ssh-keygen -Y sign` 產生的真實簽章）並上傳發佈檔、
//! 建立並送出推送；Pulse 端以 [`RawClient`] 模擬 0.9.0 的 Pulse（升級流程需要 root 與 systemd，實際的 Pulse
//! 無法在這裡替換自己；實際兩端的集中設定互通見 `remote_interop.rs`）：協商 `config`、
//! `upgrade`，依 `pending` 取得設定、提議並經 8443 分段下載，回報升級的各個狀態，替換後以新版本重新連線
//! 回報成功，推送完成。主機離線規則的寬限設為 0 秒：`applying` 之後斷線的期間沒有離線事件（升級中的離線告警
//! 抑制，REMOTE.md 第 4.7.5 節），升級結束後同一台主機離線則照常開事件。

use kp_e2e::{
    GOOD, KAIROS_STARTED, Kairos, RawClient, TestDir, WAIT, api_request, poll_until, run_to_exit,
    start_kairos, test_pki, workspace_root,
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const TS: &str = "2026-11-20T02:00:00.000Z";

fn vector(name: &str) -> Vec<u8> {
    std::fs::read(
        workspace_root()
            .join("crates/kairos-proto/testdata/release")
            .join(name),
    )
    .unwrap()
}

fn text(name: &str) -> String {
    String::from_utf8(vector(name)).unwrap()
}

fn hello(version: &str) -> Value {
    json!({
        "type": "hello", "seq": 1, "proto": 1, "proto_min": 1, "proto_max": 2,
        "features": ["metrics", "config", "upgrade"],
        "agent_id": GOOD.agent_id, "hostname": "web-01", "agent_version": version,
        "heartbeat_interval_ms": 15000, "ts": TS, "tags": ["env:prod"],
        "os": {"name": "Ubuntu", "version": "24.04", "arch": "x86_64"},
        "install": {"method": "tar", "upgrade": "ready", "config": "enabled"}
    })
}

/// 送出心跳直到 `ack` 的 `pending` 符合 `cond`（推送引擎每 5 秒一輪）
fn heartbeat_until(c: &mut RawClient, seq: &mut u64, cond: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        *seq += 1;
        c.send(&json!({"type": "heartbeat", "seq": *seq, "ts": TS}));
        let ack = c.recv();
        assert_eq!(ack["type"], "ack", "{ack}");
        if cond(&ack["pending"]) {
            return ack;
        }
        assert!(Instant::now() < deadline, "pending 沒有出現：{ack}");
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
fn central_config_and_remote_upgrade_with_a_simulated_pulse() {
    let root = TestDir::new("remote");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let signers = workspace_root().join("crates/kairos-proto/testdata/release/allowed_signers");
    let toml = kairos.dir.join("kairos.toml");
    let mut cfg = std::fs::read_to_string(&toml).unwrap();
    cfg.push_str(&format!(
        "\n[remote]\nallowed_signers = {:?}\nmax_concurrent_downloads = 1\n",
        signers.to_string_lossy()
    ));
    std::fs::write(&toml, cfg).unwrap();
    let mut k = start_kairos(
        &kairos.dir,
        &[("KAIROS_TEST_REMOTE_START_GRACE_MS", "1")],
        &kairos.ports(),
        &[KAIROS_STARTED],
    );
    let (code, p) = run_to_exit(
        "kairos",
        &kairos.dir,
        &[
            "api-token",
            "create",
            "--name",
            "admin",
            "--role",
            "admin",
            "--config",
            "kairos.toml",
            "--json",
        ],
        &[],
    );
    assert_eq!(code, 0, "{}", p.dump());
    let doc: Value = serde_json::from_str(p.stdout().trim()).unwrap();
    let token = doc["data"]["token"].as_str().unwrap().to_string();
    let port = kairos.console_port;
    let post = |path: &str, body: Value| {
        api_request(
            port,
            "POST",
            path,
            &token,
            Some(("application/json", body.to_string().as_bytes())),
        )
    };

    // 主機離線規則的寬限改為 0 秒：升級重啟期間的離線告警抑制（REMOTE.md 第 4.7.5 節）才看得出來
    let (s, v) = api_request(
        port,
        "PUT",
        "/api/v1/alert-rules/sys.host_offline",
        &token,
        Some((
            "application/json",
            json!({"params": {"grace_secs": 0}}).to_string().as_bytes(),
        )),
    );
    assert_eq!(s, 200, "{v}");
    let offline_events = || -> Vec<Value> {
        let (s, v) = api_request(port, "GET", "/api/v1/alerts?limit=200", &token, None);
        assert_eq!(s, 200, "{v}");
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["rule_id"] == "sys.host_offline" && e["agent_id"] == GOOD.agent_id)
            .cloned()
            .collect()
    };

    // 模擬的 Pulse 0.9.0：協商 config、upgrade
    let mut c = RawClient::connect(kairos.port, GOOD);
    c.send(&hello("0.9.0"));
    let ack = c.recv();
    assert_eq!(ack["type"], "hello_ack", "{ack}");
    let features: Vec<&str> = ack["features"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        features.contains(&"config") && features.contains(&"upgrade"),
        "{ack}"
    );
    assert_eq!(ack["pending"], json!({"config_rev": 0}));
    let mut seq = 1;
    seq += 1;
    c.send(&json!({"type": "report", "seq": seq, "ts": TS, "metrics": [{"name": "system.cpu.usage", "value": 3.0, "unit": "%"}]}));
    assert_eq!(c.recv()["type"], "ack");

    // 集中設定：標籤 env:prod 的設定檔
    let (s, v) = post(
        "/api/v1/config/profiles",
        json!({"name": "正式環境", "settings": {"report": {"interval": 10}, "log": {"level": "debug"}}}),
    );
    assert_eq!(s, 201, "{v}");
    let profile = v["data"]["id"].as_i64().unwrap();
    let (s, v) = post(
        "/api/v1/config/assignments",
        json!({"profile_id": profile, "selector": "tag:env:prod"}),
    );
    assert_eq!(s, 201, "{v}");
    let ack = heartbeat_until(&mut c, &mut seq, |p| {
        p["config_rev"].as_u64().is_some_and(|r| r > 0)
    });
    let rev = ack["pending"]["config_rev"].as_u64().unwrap();
    seq += 1;
    c.send(&json!({"type": "config_get", "seq": seq, "ts": TS, "have_rev": 0}));
    let cfg = c.recv();
    assert_eq!(
        (cfg["type"].as_str(), cfg["rev"].as_u64()),
        (Some("config"), Some(rev)),
        "{cfg}"
    );
    assert_eq!(
        cfg["settings"],
        json!({"log": {"level": "debug"}, "report": {"interval": 10}})
    );
    seq += 1;
    c.send(&json!({
        "type": "config_status", "seq": seq, "ts": TS, "rev": rev, "hash": cfg["hash"], "state": "applied",
        "effective": {"report": {"interval": 10}, "log": {"level": "debug"}},
        "overridden": [], "ignored": [], "errors": [], "code": null
    }));
    assert_eq!(c.recv()["type"], "ack");
    let (s, v) = api_request(
        port,
        "GET",
        &format!("/api/v1/hosts/{}/config", GOOD.agent_id),
        &token,
        None,
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["config_state"], "in_sync", "{v}");

    // 版本庫：測試向量的發佈（真實的 ssh-keygen 簽章）
    let (s, v) = post(
        "/api/v1/releases",
        json!({"sha256sums": text("SHA256SUMS"), "signature": text("SHA256SUMS.sig"), "manifest": text("pulse-0.9.1-5e6f7a8-release.json")}),
    );
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["data"]["verified"], true);
    let tar_name = "pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz";
    let tar = vector(tar_name);
    let (s, v) = api_request(
        port,
        "PUT",
        &format!("/api/v1/releases/0.9.1/artifacts/{tar_name}"),
        &token,
        Some(("application/octet-stream", &tar)),
    );
    assert_eq!(s, 200, "{v}");

    // 升級時段（0.10）：現在在時段外的推送不提議任何主機（下一步為 waiting_window），取消後再建正式的推送
    let now = time::OffsetDateTime::now_utc();
    let hhmm = |t: time::OffsetDateTime| format!("{:02}:{:02}", t.hour(), t.minute());
    let rfc3339 = |t: time::OffsetDateTime| {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        )
    };
    let closed = json!({
        "name": "時段外", "target_version": "0.9.1", "selector": {"agent_ids": [GOOD.agent_id]},
        "waves": [{"size": "rest"}],
        "schedule": {"windows": [{"start": hhmm(now + time::Duration::hours(2)), "end": hhmm(now + time::Duration::hours(3))}], "timezone": "UTC"}
    });
    let (s, v) = post("/api/v1/rollouts/preview", closed.clone());
    assert_eq!(
        (s, v["data"]["window"]["open"].as_bool()),
        (200, Some(false)),
        "{v}"
    );
    let (s, v) = post("/api/v1/rollouts", closed);
    assert_eq!(s, 201, "{v}");
    let closed_id = v["data"]["id"].as_str().unwrap().to_string();
    let (s, v) = post(
        &format!("/api/v1/rollouts/{closed_id}/submit"),
        json!({"confirm": "升級 1 台"}),
    );
    assert_eq!(
        (s, v["data"]["state"].as_str()),
        (200, Some("running")),
        "{v}"
    );
    assert_eq!(v["data"]["next_action"]["code"], "waiting_window", "{v}");
    // 推送引擎每 5 秒一輪：兩輪之後仍然沒有提議
    std::thread::sleep(Duration::from_secs(11));
    seq += 1;
    c.send(&json!({"type": "heartbeat", "seq": seq, "ts": TS}));
    let ack = c.recv();
    assert!(
        ack["pending"].get("upgrade_id").is_none(),
        "時段外不應該提議：{ack}"
    );
    let (_, v) = api_request(
        port,
        "GET",
        &format!("/api/v1/rollouts/{closed_id}/hosts"),
        &token,
        None,
    );
    assert_eq!(v["data"][0]["state"], "pending", "{v}");
    let (s, v) = post(
        &format!("/api/v1/rollouts/{closed_id}/cancel"),
        json!({"reason": "e2e：時段外的推送"}),
    );
    assert_eq!(s, 200, "{v}");

    // 推送：預覽 → 建立 → 送出（L2）。排程（0.10）：6 秒後開始（送出後為 scheduled，到時由推送引擎開始），
    // 升級時段涵蓋現在（跨午夜也可以），提議的 apply_window 到時段結束為止
    let window_end = now + time::Duration::hours(3);
    let spec = json!({
        "name": "0.9.1", "target_version": "0.9.1", "selector": {"agent_ids": [GOOD.agent_id]},
        "waves": [{"size": "rest"}],
        "schedule": {
            "starts_at": rfc3339(time::OffsetDateTime::now_utc() + time::Duration::seconds(6)),
            "windows": [{"start": hhmm(now - time::Duration::hours(1)), "end": hhmm(window_end)}],
            "timezone": "UTC"
        }
    });
    let (s, v) = post("/api/v1/rollouts/preview", spec.clone());
    assert_eq!((s, v["data"]["eligible"].as_i64()), (200, Some(1)), "{v}");
    assert_eq!(v["data"]["window"]["open"], true, "{v}");
    let (s, v) = post("/api/v1/rollouts", spec);
    assert_eq!(s, 201, "{v}");
    let id = v["data"]["id"].as_str().unwrap().to_string();
    let (s, v) = post(
        &format!("/api/v1/rollouts/{id}/submit"),
        json!({"confirm": "升級 1 台"}),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["state"], "scheduled", "{v}");
    assert_eq!(v["data"]["next_action"]["code"], "scheduled", "{v}");
    seq += 1;
    c.send(&json!({"type": "heartbeat", "seq": seq, "ts": TS}));
    let ack = c.recv();
    assert!(
        ack["pending"].get("upgrade_id").is_none(),
        "starts_at 之前不應該提議：{ack}"
    );

    // starts_at 之後的心跳帶出 upgrade_id
    let ack = heartbeat_until(&mut c, &mut seq, |p| p["upgrade_id"].is_string());
    let upgrade_id = ack["pending"]["upgrade_id"].as_str().unwrap().to_string();
    seq += 1;
    c.send(&json!({"type": "upgrade_get", "seq": seq, "ts": TS, "upgrade_id": upgrade_id}));
    let offer = c.recv();
    assert_eq!(offer["type"], "upgrade_offer", "{offer}");
    assert_eq!(offer["artifact"]["filename"], tar_name);
    assert_eq!(
        offer["target"],
        json!({"version": "0.9.1", "commit": "5e6f7a8"})
    );
    // apply_window 到升級時段結束為止（不是提議後 24 小時）
    let not_after = offer["apply_window"]["not_after"].as_str().unwrap();
    assert!(
        not_after.contains(&format!("T{}:00", hhmm(window_end))),
        "{offer}"
    );
    let (_, v) = api_request(port, "GET", &format!("/api/v1/rollouts/{id}"), &token, None);
    assert_eq!(v["data"]["state"], "running", "{v}");
    let parsed: kairos_proto::mgmt::UpgradeOffer = serde_json::from_value(offer.clone()).unwrap();
    // Pulse 端以本機信任錨驗證（測試向量的金鑰）
    let trust = kairos_proto::release::AllowedSigners::parse(&text("allowed_signers"));
    let verified = kairos_proto::release::verify_bundle(
        &kairos_proto::release::ReleaseBundle {
            sha256sums: &parsed.sha256sums,
            signature: &parsed.signature,
            manifest: parsed
                .manifest
                .as_ref()
                .map(|m| (m.filename.as_str(), m.content.as_str())),
        },
        &kairos_proto::release::TrustPolicy {
            signers: &trust,
            extra_principals: &[],
            now_unix: 1_790_000_000,
        },
    )
    .unwrap();
    assert!(
        verified
            .check_artifact(
                &parsed.artifact.filename,
                &parsed.artifact.sha256,
                &Default::default()
            )
            .is_ok()
    );
    seq += 1;
    c.send(&json!({"type": "upgrade_status", "seq": seq, "ts": TS, "upgrade_id": upgrade_id, "state": "accepted", "from_version": "0.9.0", "to_version": "0.9.1", "code": null, "detail": ""}));
    assert_eq!(c.recv()["type"], "ack");
    // 經 8443 分段下載
    let mut data = Vec::new();
    loop {
        seq += 1;
        c.send(&json!({"type": "upgrade_chunk_get", "seq": seq, "ts": TS, "upgrade_id": upgrade_id, "filename": tar_name, "offset": data.len(), "length": 16}));
        let chunk = c.recv();
        assert_eq!(chunk["type"], "upgrade_chunk", "{chunk}");
        let m: kairos_proto::mgmt::UpgradeChunk = serde_json::from_value(chunk).unwrap();
        data.extend(m.decode_data().unwrap());
        if m.eof {
            break;
        }
    }
    assert_eq!(data, tar);
    assert_eq!(
        kairos_proto::release::sha256_hex(&data),
        parsed.artifact.sha256
    );
    for state in ["staged", "applying"] {
        seq += 1;
        c.send(&json!({"type": "upgrade_status", "seq": seq, "ts": TS, "upgrade_id": upgrade_id, "state": state, "from_version": "0.9.0", "to_version": "0.9.1", "code": null, "detail": ""}));
        assert_eq!(c.recv()["type"], "ack");
    }
    c.close();

    // 替換、重新啟動期間主機離線：告警引擎（每 5 秒一輪）不開離線事件（寬限 0 秒也一樣）
    std::thread::sleep(Duration::from_secs(12));
    assert!(
        offline_events().is_empty(),
        "升級中的主機不應該有離線事件：{:?}\n{}",
        offline_events(),
        k.dump()
    );

    // 替換後的新版重新連線：回報成功，推送完成
    let mut c = RawClient::connect(kairos.port, GOOD);
    c.send(&hello("0.9.1"));
    let ack = c.recv();
    assert!(ack["pending"].get("upgrade_id").is_none(), "{ack}");
    c.send(&json!({"type": "upgrade_status", "seq": 2, "ts": TS, "upgrade_id": upgrade_id, "state": "succeeded", "from_version": "0.9.0", "to_version": "0.9.1", "code": "PLS-I8416", "detail": ""}));
    assert_eq!(c.recv()["type"], "ack");
    assert!(
        poll_until(WAIT, || {
            let (_, v) = api_request(port, "GET", &format!("/api/v1/rollouts/{id}"), &token, None);
            v["data"]["state"] == "completed"
        }),
        "推送沒有完成\n{}",
        k.dump()
    );
    let (_, v) = api_request(
        port,
        "GET",
        &format!("/api/v1/rollouts/{id}/hosts"),
        &token,
        None,
    );
    assert_eq!(v["data"][0]["state"], "succeeded", "{v}");
    let (_, v) = api_request(
        port,
        "GET",
        &format!("/api/v1/hosts/{}/upgrade", GOOD.agent_id),
        &token,
        None,
    );
    assert_eq!(v["data"]["upgrades"][0]["to_version"], "0.9.1", "{v}");
    assert!(offline_events().is_empty(), "{:?}", offline_events());
    c.close();
    // 對照：升級結束後抑制解除，同一台主機離線就照常開事件
    assert!(
        poll_until(Duration::from_secs(40), || !offline_events().is_empty()),
        "升級結束後的離線沒有開事件\n{}",
        k.dump()
    );
    let ev = offline_events().remove(0);
    let (_, v) = api_request(
        port,
        "GET",
        &format!("/api/v1/alerts/{}", ev["event_id"].as_str().unwrap()),
        &token,
        None,
    );
    assert!(
        v["data"]["details"].get("upgrade").is_none(),
        "不是升級期間的離線：{v}"
    );
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
    assert!(
        k.count("rollout.complete") >= 1 || k.output().contains("已完成"),
        "{}",
        k.dump()
    );
}

/// 外部 URL 版本的資格預覽（REMOTE.md 第 2.1、4.7.2 節）：Pulse 在 hello 的 `install.external_url` 宣告本機是否
/// 允許外部 URL，實際的 Kairos 據此分類——
///
/// - Pulse 低於 0.10.0（不支援外部 URL）：預覽列為 `url_not_supported`
/// - 宣告 `none`（白名單是空的）：預覽列為 `url_not_allowed`，送出時固定為 `skipped`，不會被提議
/// - 宣告 `allowlist`：可升級（Kairos 不知道白名單的內容）
/// - 沒有宣告（0.10.0、0.10.1 的 Pulse；舊 Pulse 連新 Kairos）：照舊可升級，預覽的 `external_url.undeclared`
///   註明台數；連線中的主機以這次 hello 為準，不沿用上一次宣告的值
/// - 由 Kairos 代管的版本不看這個宣告
#[test]
fn external_url_declaration_reaches_the_eligibility_preview() {
    let root = TestDir::new("remote-exturl");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let signers = workspace_root().join("crates/kairos-proto/testdata/release/allowed_signers");
    let toml = kairos.dir.join("kairos.toml");
    let mut cfg = std::fs::read_to_string(&toml).unwrap();
    cfg.push_str(&format!(
        "\n[remote]\nallowed_signers = {:?}\n",
        signers.to_string_lossy()
    ));
    std::fs::write(&toml, cfg).unwrap();
    let mut k = start_kairos(
        &kairos.dir,
        &[("KAIROS_TEST_REMOTE_START_GRACE_MS", "1")],
        &kairos.ports(),
        &[KAIROS_STARTED],
    );
    let (code, p) = run_to_exit(
        "kairos",
        &kairos.dir,
        &[
            "api-token",
            "create",
            "--name",
            "admin",
            "--role",
            "admin",
            "--config",
            "kairos.toml",
            "--json",
        ],
        &[],
    );
    assert_eq!(code, 0, "{}", p.dump());
    let doc: Value = serde_json::from_str(p.stdout().trim()).unwrap();
    let token = doc["data"]["token"].as_str().unwrap().to_string();
    let port = kairos.console_port;
    let post = |path: &str, body: Value| {
        api_request(
            port,
            "POST",
            path,
            &token,
            Some(("application/json", body.to_string().as_bytes())),
        )
    };

    // 外部 URL 的版本庫：測試向量的發佈（真實的簽章），發佈檔只登記雜湊與大小
    let base_url = "https://repo.example.gov.tw/kairos-pulse/0.9.1/";
    let (s, v) = post(
        "/api/v1/releases",
        json!({
            "sha256sums": text("SHA256SUMS"), "signature": text("SHA256SUMS.sig"),
            "manifest": text("pulse-0.9.1-5e6f7a8-release.json"),
            "source": "external", "base_url": base_url,
        }),
    );
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["data"]["source"], "external", "{v}");
    let tar_name = "pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz";
    let (s, v) = api_request(
        port,
        "PUT",
        &format!("/api/v1/releases/0.9.1/artifacts/{tar_name}"),
        &token,
        Some(("application/octet-stream", &vector(tar_name))),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["registered"], true, "{v}");

    // 模擬的 Pulse：hello 帶（或不帶）install.external_url；送一筆 report 讓主機進入清冊
    let connect = |version: &str, declared: Option<&str>| {
        let mut h = hello(version);
        if let Some(d) = declared {
            h["install"]["external_url"] = json!(d);
        }
        let mut c = RawClient::connect(kairos.port, GOOD);
        c.send(&h);
        let ack = c.recv();
        assert_eq!(ack["type"], "hello_ack", "{declared:?}: {ack}");
        // 協定版本與 feature 不因這個欄位改變
        assert_eq!(ack["proto"], 2, "{ack}");
        c.send(&json!({"type": "report", "seq": 2, "ts": TS, "metrics": [{"name": "system.cpu.usage", "value": 3.0, "unit": "%"}]}));
        assert_eq!(c.recv()["type"], "ack");
        c
    };
    // 測試向量的發佈是 0.9.1；支援外部 URL 的 Pulse 至少是 0.10.0，所以對它們而言這是降版（允許降版，
    // 資格分類中外部 URL 的判斷在降版之前）
    let spec = json!({
        "name": "外部 URL", "target_version": "0.9.1",
        "selector": {"agent_ids": [GOOD.agent_id]}, "waves": [{"size": "rest"}],
        "options": {"allow_downgrade": true},
    });
    let preview = || {
        let (s, v) = post("/api/v1/rollouts/preview", spec.clone());
        assert_eq!(s, 200, "{v}");
        v["data"].clone()
    };
    let declared_in_api = || {
        let (s, v) = api_request(
            port,
            "GET",
            &format!("/api/v1/hosts/{}/upgrade", GOOD.agent_id),
            &token,
            None,
        );
        if s == 200 {
            v["data"]["install"]["external_url"].clone()
        } else {
            json!("（清冊還沒有這台主機）")
        }
    };

    // 0. Pulse 0.9（不支援外部 URL，也不會宣告）：事先列為 url_not_supported，不算可升級
    let c = connect("0.9.0", None);
    assert!(
        poll_until(WAIT, || preview()["skipped"]["url_not_supported"] == 1),
        "{}\n{}",
        preview(),
        k.dump()
    );
    let p = preview();
    assert_eq!(p["eligible"], 0, "{p}");
    assert_eq!(p["skipped"], json!({"url_not_supported": 1}), "{p}");
    assert_eq!(
        p["samples"]["url_not_supported"][0]["agent_id"], GOOD.agent_id,
        "{p}"
    );
    assert_eq!(
        p["external_url"],
        json!({"base_url": base_url, "undeclared": 0, "allowlist": 0}),
        "{p}"
    );
    c.close();

    // 1. 宣告 none：事先列為 url_not_allowed，不算可升級
    let c = connect("0.10.1", Some("none"));
    assert!(
        poll_until(WAIT, || preview()["skipped"]["url_not_allowed"] == 1),
        "{}\n{}",
        preview(),
        k.dump()
    );
    let p = preview();
    assert_eq!(p["eligible"], 0, "{p}");
    assert_eq!(
        p["samples"]["url_not_allowed"][0]["agent_id"], GOOD.agent_id,
        "{p}"
    );
    assert_eq!(
        p["external_url"],
        json!({"base_url": base_url, "undeclared": 0, "allowlist": 0}),
        "{p}"
    );
    assert_eq!(declared_in_api(), "none");
    c.close();

    // 2. 宣告 allowlist（修改 pulse.toml 後重新啟動的 Pulse）：可升級，註明 Kairos 不知道白名單的內容
    let c = connect("0.10.1", Some("allowlist"));
    assert!(
        poll_until(WAIT, || preview()["eligible"] == 1),
        "{}\n{}",
        preview(),
        k.dump()
    );
    let p = preview();
    assert_eq!(p["skipped"], json!({}), "{p}");
    assert_eq!(
        p["external_url"],
        json!({"base_url": base_url, "undeclared": 0, "allowlist": 1}),
        "{p}"
    );
    assert_eq!(declared_in_api(), "allowlist");
    c.close();

    // 3. 沒有宣告（0.10.0、0.10.1 的 Pulse 連新的 Kairos）：照舊可升級；不沿用上一次連線宣告的值
    let mut c = connect("0.10.1", None);
    assert!(
        poll_until(WAIT, || preview()["external_url"]["undeclared"] == 1),
        "{}\n{}",
        preview(),
        k.dump()
    );
    let p = preview();
    assert_eq!(
        (p["eligible"].as_i64(), &p["skipped"]),
        (Some(1), &json!({})),
        "{p}"
    );
    assert_eq!(declared_in_api(), Value::Null);
    // 送出並提議：外部 URL 的提議照現行的流程（source: url）；主機拒絕時是 declined
    let (s, v) = post("/api/v1/rollouts", spec.clone());
    assert_eq!(s, 201, "{v}");
    let id = v["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(p["confirm_text"], "降版 1 台", "{p}");
    let (s, v) = post(
        &format!("/api/v1/rollouts/{id}/submit"),
        json!({"confirm": "降版 1 台", "reason": "e2e：外部 URL 的測試向量是 0.9.1"}),
    );
    assert_eq!(
        (s, v["data"]["state"].as_str()),
        (200, Some("running")),
        "{v}"
    );
    let mut seq = 2;
    let ack = heartbeat_until(&mut c, &mut seq, |p| p.get("upgrade_id").is_some());
    let upgrade_id = ack["pending"]["upgrade_id"].as_str().unwrap().to_string();
    seq += 1;
    c.send(&json!({"type": "upgrade_get", "seq": seq, "ts": TS, "upgrade_id": upgrade_id}));
    let offer = c.recv();
    assert_eq!(offer["type"], "upgrade_offer", "{offer}");
    assert_eq!(offer["artifact"]["source"], "url", "{offer}");
    assert_eq!(
        offer["artifact"]["url"],
        format!("{base_url}{tar_name}"),
        "{offer}"
    );
    seq += 1;
    c.send(&json!({
        "type": "upgrade_status", "seq": seq, "ts": TS, "upgrade_id": upgrade_id, "state": "declined",
        "from_version": "0.10.1", "to_version": "0.9.1", "code": "PLS-W8411", "detail": "url_not_allowed"
    }));
    assert_eq!(c.recv()["type"], "ack");
    let (s, v) = api_request(
        port,
        "GET",
        &format!("/api/v1/rollouts/{id}/hosts"),
        &token,
        None,
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"][0]["state"], "declined", "{v}");
    c.close();
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
}
