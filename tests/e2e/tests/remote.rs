//! 端對端：集中設定與遠端升級的 Kairos 端（F8 K1／K2；`crates/kairos-proto/REMOTE.md`）。
//!
//! 實際的 Kairos 執行檔（SQLite、mTLS、`[remote] allowed_signers` 為 `kairos-proto` 的測試向量金鑰），
//! 以主控台 API 建立設定檔與指派、匯入測試向量的發佈（`ssh-keygen -Y sign` 產生的真實簽章）並上傳發佈檔、
//! 建立並送出推送；Pulse 端以 [`RawClient`] 模擬 0.9.0 的 Pulse（P1／P2 在另一條線）：協商 `config`、
//! `upgrade`，依 `pending` 取得設定、提議並經 8443 分段下載，回報升級的各個狀態，替換後以新版本重新連線
//! 回報成功，推送完成。

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

    // 推送：預覽 → 建立 → 送出（L2）
    let spec = json!({"name": "0.9.1", "target_version": "0.9.1", "selector": {"agent_ids": [GOOD.agent_id]}, "waves": [{"size": "rest"}]});
    let (s, v) = post("/api/v1/rollouts/preview", spec.clone());
    assert_eq!((s, v["data"]["eligible"].as_i64()), (200, Some(1)), "{v}");
    let (s, v) = post("/api/v1/rollouts", spec);
    assert_eq!(s, 201, "{v}");
    let id = v["data"]["id"].as_str().unwrap().to_string();
    let (s, v) = post(
        &format!("/api/v1/rollouts/{id}/submit"),
        json!({"confirm": "升級 1 台"}),
    );
    assert_eq!(s, 200, "{v}");

    // 下一次心跳帶出 upgrade_id
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
    c.close();
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
    assert!(
        k.count("rollout.complete") >= 1 || k.output().contains("已完成"),
        "{}",
        k.dump()
    );
}
