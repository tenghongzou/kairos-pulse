//! 個資保護（商業審計 S6；`kairos/PRIVACY.md`）的端對端測試：以實際的 `kairos`、`pulse` 執行檔。
//!
//! - `[privacy]`：主機名稱假名化、來源位址截成網段 → 清冊（`kairos hosts list`）、`agents.json` 與匯出都只有
//!   假名與網段；`kairos hosts pseudonym` 算出同一個假名；`data/privacy.key` 為 0600。
//! - 刪除主機資料：主控台 API（Kairos 執行中，影響預覽 → L2 確認）與 CLI（執行中拒絕，停止後可以）；
//!   個資匯出（`kairos hosts export`）。

use kp_e2e::{
    GOOD, Kairos, PULSE_REGISTERED, Pulse, REVOKED, TestDir, WAIT, api_raw, poll_until,
    run_to_exit, test_pki,
};
use serde_json::{Value, json};

fn kairos_cli(k: &Kairos, args: &[&str]) -> (i32, kp_e2e::Proc) {
    let mut all = args.to_vec();
    all.extend(["--config", "kairos.toml"]);
    run_to_exit("kairos", &k.dir, &all, &[])
}

fn admin_token(k: &Kairos) -> String {
    let (code, p) = kairos_cli(
        k,
        &[
            "api-token",
            "create",
            "--name",
            "privacy",
            "--role",
            "admin",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{}", p.dump());
    serde_json::from_str::<Value>(p.stdout().trim()).unwrap()["data"]["token"]
        .as_str()
        .unwrap()
        .to_string()
}

fn post(k: &Kairos, token: &str, path: &str, body: &Value) -> (u16, Value) {
    let text = body.to_string();
    let (status, _, resp) = api_raw(
        k.console_port,
        &format!(
            "POST {path} HTTP/1.0\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{text}",
            text.len()
        ),
    );
    (status, serde_json::from_str(&resp).unwrap_or(Value::Null))
}

#[test]
fn privacy_settings_pseudonymize_and_purge_deletes_a_host() {
    let root = TestDir::new("privacy");
    let k = Kairos::new(&root, test_pki::CRL_EMPTY);
    let mut toml = std::fs::read_to_string(k.dir.join("kairos.toml")).unwrap();
    toml.push_str("\n[privacy]\nstore_peer_ip = \"truncate\"\nhostname = \"pseudonymize\"\n");
    std::fs::write(k.dir.join("kairos.toml"), toml).unwrap();
    let mut kairos = k.start();
    assert!(kairos.output().contains("個資保護"), "{}", kairos.dump());
    let key = k.dir.join("data").join("privacy.key");
    assert_eq!(std::fs::metadata(&key).unwrap().len(), 32);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let pulse = Pulse::new(&root, "pulse", k.port, GOOD);
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    assert!(
        poll_until(WAIT, || !k.hosts().is_empty()),
        "清冊應有這台主機"
    );
    let host = k.hosts().remove(0);
    let name = host["hostname"].as_str().unwrap().to_string();
    assert!(name.starts_with("host-") && name.len() == 17, "{host}");
    assert_eq!(host["ip"], "127.0.0.0/24", "{host}");
    // 狀態表的快照（agents.json）也只有假名與網段
    assert!(poll_until(WAIT, || {
        if !k.dir.join("data").join("agents.json").exists() {
            return false;
        }
        let a = k.agents().to_string();
        a.contains(&name) && a.contains("127.0.0.0/24")
    }));
    // kairos hosts pseudonym：以金鑰算出同一個假名（Pulse 送出的是這台主機的名稱）
    #[cfg(target_os = "linux")]
    if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
        let (code, out) = kairos_cli(&k, &["hosts", "pseudonym", h.trim()]);
        assert_eq!(code, 0, "{}", out.dump());
        assert_eq!(out.stdout().trim(), format!("{}\t{name}", h.trim()));
    }
    // JSONL 鏡像也沒有原本的位址與埠
    assert!(poll_until(WAIT, || !k.jsonl_reports().is_empty()));
    assert!(
        k.jsonl_reports()
            .iter()
            .all(|r| r["peer"] == "127.0.0.0/24" && r["hostname"] == name.as_str())
    );

    // 主控台：仍連線中不能刪除
    let token = admin_token(&k);
    let id = GOOD.agent_id;
    let api = k.api(&token);
    let (s, v) = api.get(&format!("/api/v1/hosts/{id}/purge"));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["online"], true, "{v}");
    p.stop();
    // 離線後：個資匯出 → 刪除（L2：確認字串是清冊中的名稱，也就是假名）
    assert!(poll_until(WAIT, || {
        api.get(&format!("/api/v1/hosts/{id}/purge")).1["data"]["online"] == false
    }));
    let (s, v) = api.get(&format!("/api/v1/hosts/{id}/personal-data"));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["host"]["hostname"], name.as_str());
    assert_eq!(v["data"]["privacy"]["hostname"], "pseudonymize");
    let preview = api.get(&format!("/api/v1/hosts/{id}/purge")).1["data"].clone();
    assert_eq!(preview["confirm_text"], name.as_str());
    let (s, v) = post(
        &k,
        &token,
        &format!("/api/v1/hosts/{id}/purge"),
        &json!({"confirm": name, "impact_token": preview["impact_token"], "reason": "e2e"}),
    );
    assert_eq!(s, 200, "{v}");
    assert!(
        v["data"]["deleted"]["reports"].as_u64().unwrap() >= 1,
        "{v}"
    );
    assert!(k.hosts().is_empty(), "{:?}", k.hosts());
    assert!(k.reports().iter().all(|r| r["agent_id"] != id));
    assert!(kairos.output().contains("KRS-I4204"), "{}", kairos.dump());
    let (code, out) = kairos_cli(&k, &["hosts", "export", id]);
    assert_eq!(code, 1, "{}", out.dump());

    // CLI：另一台主機。Kairos 執行中不能直接刪除（KRS-E4304，結束代碼 11），停止後可以
    let other = Pulse::new(&root, "other", k.port, REVOKED);
    let mut p2 = other.start(&[]);
    p2.wait_for(PULSE_REGISTERED, WAIT);
    let rid = REVOKED.agent_id;
    assert!(poll_until(WAIT, || k
        .hosts()
        .iter()
        .any(|h| h["agent_id"] == rid)));
    p2.stop();
    let (code, out) = kairos_cli(&k, &["hosts", "purge", rid, "--dry-run", "--json"]);
    assert_eq!(code, 0, "{}", out.dump());
    let dry: Value = serde_json::from_str(out.stdout().trim()).unwrap();
    assert_eq!(dry["schema"], "kairos.hosts.purge/v1");
    assert_eq!(dry["data"]["counts"]["hosts"], 1);
    let other_name = k
        .hosts()
        .into_iter()
        .find(|h| h["agent_id"] == rid)
        .unwrap()["hostname"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, out) = kairos_cli(
        &k,
        &["hosts", "purge", rid, "--yes", "--confirm", &other_name],
    );
    assert_eq!(code, 11, "{}", out.dump());
    assert!(out.stderr().contains("KRS-E4304"), "{}", out.dump());
    kairos.stop();
    let (code, out) = kairos_cli(
        &k,
        &[
            "hosts",
            "purge",
            rid,
            "--yes",
            "--confirm",
            &other_name,
            "--json",
        ],
    );
    assert_eq!(code, 0, "{}", out.dump());
    let done: Value = serde_json::from_str(out.stdout().trim()).unwrap();
    assert_eq!(done["data"]["deleted"]["hosts"], 1, "{done}");
    assert!(k.hosts().is_empty());
}
