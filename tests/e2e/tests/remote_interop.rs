//! 端對端：**實際的 Kairos 與實際的 Pulse** 互通的集中設定（F8 K1 × P1；`crates/kairos-proto/REMOTE.md`
//! 第 3 節、PROTOCOL.md「遠端管理」）。
//!
//! `remote.rs` 以假 client 模擬 Pulse、`remote_config.rs` 以假 Kairos 驗證 Pulse，這裡兩端都是實際的執行檔
//! （SQLite、mTLS）：
//!
//! - hello 協商 `config`（Pulse 的 `install` 寫進 Kairos 的主機清冊），沒有設定時 `in_sync`
//! - 主控台 API 建立設定檔並指派給這台主機 → `pending.config_rev` → Pulse 取得並套用 → `config_status`：
//!   沒有本機值的 `report.interval` 即時生效（`status.json` 的 `report_interval_ms`、連線不中斷），
//!   本機 `pulse.toml` 的 `log.level` 優先而列在 `overridden`（Kairos 顯示 `overridden`）
//! - 修改設定檔（新的一版）→ Pulse 套用新版；`pulse config show --effective` 讀快取
//! - 刪除指派 → 期望設定回到第 0 版 → Pulse 回到內建預設
//!
//! 遠端升級需要 root、systemd 與 /opt/pulse 的安裝，不在這裡執行（`remote.rs` 與 Pulse 的
//! `remote::upgrade::helper::tests` 涵蓋兩端各自的部分）。

use kp_e2e::{
    GOOD, KAIROS_STARTED, Kairos, PULSE_REGISTERED, Pulse, TestDir, WAIT, api_request, poll_until,
    run_to_exit, start_kairos, test_pki,
};
use serde_json::{Value, json};
use std::time::Duration;

fn status(pulse: &Pulse) -> Value {
    std::fs::read_to_string(pulse.dir.join("state/status.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn keys(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|o| o.as_str().or_else(|| o["key"].as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn real_kairos_pushes_central_config_to_a_real_pulse() {
    let root = TestDir::new("remote-interop");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let mut k = start_kairos(&kairos.dir, &[], &kairos.ports(), &[KAIROS_STARTED]);
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
    let call = |method: &str, path: &str, body: Option<Value>| {
        let body = body.map(|b| b.to_string());
        api_request(
            port,
            method,
            path,
            &token,
            body.as_deref().map(|b| ("application/json", b.as_bytes())),
        )
    };
    let host_config = || {
        let (s, v) = call(
            "GET",
            &format!("/api/v1/hosts/{}/config", GOOD.agent_id),
            None,
        );
        assert_eq!(s, 200, "{v}");
        v["data"].clone()
    };

    // 實際的 Pulse：本機沒有 [report]（讓下發的 report.interval 生效），log.level = trace 是本機值
    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);
    let toml_path = pulse.dir.join("pulse.toml");
    let toml = std::fs::read_to_string(&toml_path)
        .unwrap()
        .replace("[report]\ninterval = 1\n", "");
    std::fs::write(&toml_path, toml).unwrap();
    let state = pulse.dir.join("state");
    let envs = [("STATE_DIRECTORY", state.to_str().unwrap())];
    let mut p = pulse.start(&envs);
    p.wait_for(PULSE_REGISTERED, WAIT);

    // 協商：兩端都宣告 config（upgrade 依 Pulse 本機是否就緒，測試環境沒有輔助程式與信任錨）
    assert!(
        poll_until(WAIT, || {
            let s = status(&pulse);
            keys(&s["connection"]["features"]).contains(&"config".to_string())
        }),
        "Pulse 沒有協商 config：{}\n{}",
        status(&pulse),
        p.dump()
    );
    assert!(
        poll_until(WAIT, || host_config()["config_negotiated"] == true),
        "{}",
        host_config()
    );
    let hc = host_config();
    assert_eq!(hc["agent_version"], "0.9.0", "{hc}");
    assert_eq!(hc["config_state"], "in_sync", "{hc}");
    assert_eq!(status(&pulse)["report_interval_ms"], 5000, "內建預設");

    // 設定檔 → 指派給這台主機
    let (s, v) = call(
        "POST",
        "/api/v1/config/profiles",
        Some(
            json!({"name": "互通測試", "settings": {"report": {"interval": 3}, "log": {"level": "debug"}}}),
        ),
    );
    assert_eq!(s, 201, "{v}");
    let profile = v["data"]["id"].as_i64().unwrap();
    let (s, v) = call(
        "POST",
        "/api/v1/config/assignments",
        Some(json!({"profile_id": profile, "selector": format!("host:{}", GOOD.agent_id)})),
    );
    assert_eq!(s, 201, "{v}");
    let assignment = v["data"]["id"].as_i64().unwrap();

    // Pulse 取得、套用並回報；Kairos 記錄實際生效的值與本機覆蓋
    assert!(
        poll_until(WAIT, || {
            let hc = host_config();
            hc["desired_rev"].as_u64().unwrap_or(0) > 0
                && hc["applied_rev"] == hc["desired_rev"]
                && hc["state"] == "applied"
        }),
        "{}\n{}\n{}",
        host_config(),
        p.dump(),
        k.dump()
    );
    let hc = host_config();
    let rev1 = hc["desired_rev"].as_u64().unwrap();
    assert_eq!(hc["applied_hash"], hc["desired_hash"], "{hc}");
    assert_eq!(hc["effective"]["report"]["interval"], 3, "{hc}");
    assert_eq!(hc["effective"]["log"]["level"], "trace", "本機優先：{hc}");
    assert_eq!(keys(&hc["overridden"]), ["log.level"], "{hc}");
    assert_eq!(hc["code"], "PLS-I0503", "{hc}");
    assert_eq!(hc["config_state"], "overridden", "{hc}");
    p.wait_for("config.applied", WAIT);
    // 即時套用：report 的間隔改為 3 秒，連線沒有中斷
    assert!(
        poll_until(WAIT, || status(&pulse)["report_interval_ms"] == 3000),
        "{}",
        status(&pulse)
    );
    assert_eq!(p.count(PULSE_REGISTERED), 1, "即時的鍵不需要重新連線");
    let view = &status(&pulse)["management"]["config"];
    assert_eq!(
        (view["usage"].as_str(), view["rev"].as_u64()),
        (Some("in_use"), Some(rev1)),
        "{view}"
    );

    // 修改設定檔（新的一版）：Pulse 套用新版
    let (s, v) = call(
        "PUT",
        &format!("/api/v1/config/profiles/{profile}"),
        Some(json!({"settings": {"report": {"interval": 4}, "log": {"level": "debug"}}})),
    );
    assert_eq!(s, 200, "{v}");
    assert!(
        poll_until(WAIT, || {
            let hc = host_config();
            hc["desired_rev"].as_u64().unwrap_or(0) > rev1
                && hc["applied_rev"] == hc["desired_rev"]
                && hc["effective"]["report"]["interval"] == 4
        }),
        "{}\n{}",
        host_config(),
        p.dump()
    );
    let rev2 = host_config()["desired_rev"].as_u64().unwrap();
    assert!(
        poll_until(WAIT, || status(&pulse)["report_interval_ms"] == 4000),
        "{}",
        status(&pulse)
    );
    // pulse config show --effective：讀快取（不連線）
    let (code, show) = run_to_exit(
        "pulse",
        &pulse.dir,
        &["config", "show", "--effective", "--sources", "--json"],
        &envs,
    );
    assert_eq!(code, 0, "{}", show.dump());
    let doc: Value = serde_json::from_str(show.stdout().trim()).unwrap();
    assert_eq!(doc["remote"]["rev"].as_u64(), Some(rev2), "{doc}");
    let row = |key: &str| {
        doc["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == key)
            .cloned()
            .unwrap()
    };
    assert_eq!(row("report.interval")["value"], 4, "{doc}");
    assert!(
        row("report.interval")["source"]
            .as_str()
            .is_some_and(|s| s.starts_with("remote")),
        "{doc}"
    );
    assert_eq!(row("log.level")["source"], "pulse.toml", "{doc}");

    // 刪除指派：期望設定回到第 0 版，Pulse 回到內建預設
    let (s, v) = call(
        "DELETE",
        &format!("/api/v1/config/assignments/{assignment}"),
        None,
    );
    assert!(s == 200 || s == 204, "{s} {v}");
    assert!(
        poll_until(WAIT, || status(&pulse)["report_interval_ms"] == 5000),
        "{}\n{}\n{}",
        status(&pulse),
        host_config(),
        p.dump()
    );
    assert!(
        poll_until(WAIT, || {
            let hc = host_config();
            hc["desired_rev"] == 0 && hc["applied_rev"] == 0 && hc["config_state"] == "in_sync"
        }),
        "{}",
        host_config()
    );
    assert_eq!(p.count(PULSE_REGISTERED), 1, "{}", p.dump());
    assert_eq!(p.count("agent.fatal"), 0, "{}", p.dump());

    // hello 的 install 寫進主機清冊（清冊每 30 秒與連線狀態同步一次，API.md 第 17 節）
    assert!(
        poll_until(Duration::from_secs(75), || {
            let hc = host_config();
            hc["install"]["config"] == "enabled" && hc["install"]["method"].is_string()
        }),
        "{}",
        host_config()
    );

    p.stop();
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
}
