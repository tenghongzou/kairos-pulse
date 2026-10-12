//! 端對端：協定 v1 相容（審計 A3、O6）。
//!
//! 新版 Pulse 一律宣告支援 1～2、沒有只用 v1 的設定，因此舊版（0.1.0）Pulse 以 [`RawClient`] 模擬：
//! 送出與舊版 Pulse 完全相同的 hello、heartbeat、report（`crates/kairos-proto/PROTOCOL.md`「範例 / v1」）。
//! 新版 Pulse 連舊版 Kairos 的方向由 Pulse 的單元測試（記憶體內的 v1 伺服器）涵蓋。

use kp_e2e::{
    GOOD, Kairos, PULSE_REGISTERED, Pulse, REVOKED, RawClient, TestDir, WAIT, poll_until, test_pki,
};
use serde_json::{Value, json};

/// 舊版 Pulse 的 hello（只有 `proto` 1，沒有 `proto_min`／`proto_max`、features 與主機資訊）
fn v1_hello(agent_id: &str) -> Value {
    json!({
        "type": "hello", "seq": 1, "proto": 1, "agent_id": agent_id, "hostname": "old-pulse",
        "agent_version": "0.1.0", "heartbeat_interval_ms": 1000, "ts": "2026-10-08T03:15:40.001Z"
    })
}

/// JSONL 中 `agent_id` 的行
fn lines_of(reports: &[Value], agent_id: &str) -> Vec<Value> {
    reports
        .iter()
        .filter(|r| r["agent_id"] == agent_id)
        .cloned()
        .collect()
}

#[test]
fn v1_client_and_new_pulse_share_one_new_kairos() {
    // 混用版本：同一個新版 Kairos 同時接受舊版 Pulse（v1）與新版 Pulse（v2），
    // 兩種 report 寫進同一個 JSONL 檔，以 schema 區分
    let root = TestDir::new("compat");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);
    let old_id = REVOKED.agent_id; // CRL_EMPTY 下是有效的身分，作為第二台主機

    let mut k = kairos.start();
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    assert!(
        p.lines_with(PULSE_REGISTERED)[0].contains("協定 v2"),
        "{}",
        p.dump()
    );

    // 舊版 Pulse：hello_ack 與 v1 Kairos 完全相同（沒有 features、server_time）
    let mut old = RawClient::connect(kairos.port, REVOKED);
    old.send(&v1_hello(old_id));
    assert_eq!(
        old.recv(),
        json!({"type":"hello_ack","seq":1,"proto":1,"offline_after_ms":5000})
    );
    old.send(&json!({"type":"heartbeat","seq":2,"ts":"2026-10-08T03:15:41.002Z"}));
    assert_eq!(old.recv(), json!({"type":"ack","seq":2}));
    old.send(&json!({
        "type": "report", "seq": 3, "ts": "2026-10-08T03:15:45.210Z",
        "cpu_usage": 17.84, "total_memory": 49152, "used_memory": 37034
    }));
    assert_eq!(old.recv(), json!({"type":"ack","seq":3}));
    // v1 連線送通用指標格式：與 v1 Kairos 相同，是缺少 cpu_usage 的 bad_request（連線保持）
    old.send(&json!({
        "type": "report", "seq": 4, "ts": "2026-10-08T03:15:46.210Z",
        "metrics": [{"name": "system.cpu.usage", "value": 1.0, "unit": "%"}]
    }));
    let err = old.recv();
    assert_eq!(
        (&err["type"], &err["code"], &err["seq"], &err["fatal"]),
        (
            &json!("error"),
            &json!("bad_request"),
            &Value::Null,
            &json!(false)
        ),
        "{err}"
    );
    assert!(
        err["message"].as_str().unwrap().contains("cpu_usage"),
        "{err}"
    );
    old.send(&json!({"type":"heartbeat","seq":5,"ts":"2026-10-08T03:15:47.002Z"}));
    assert_eq!(old.recv(), json!({"type":"ack","seq":5}));

    // Kairos 的上線日誌寫出各自協商的版本
    k.wait_for(&format!("agent {old_id}（old-pulse，v0.1.0"), WAIT);
    let online = k.lines_with(&format!("agent {old_id}（old-pulse，v0.1.0"));
    assert!(online[0].contains("協定 v1"), "{online:?}");

    // JSONL：舊版 Pulse 的行是 schema 1，欄位與舊版相同（只多了 schema）；新版 Pulse 的行是 schema 2
    assert!(
        poll_until(WAIT, || {
            let reports = kairos.reports();
            !lines_of(&reports, old_id).is_empty()
                && !lines_of(&reports, test_pki::GOOD_ID).is_empty()
        }),
        "兩台主機的 report 都應寫入：{:?}\n{}",
        kairos.reports(),
        k.dump()
    );
    let reports = kairos.reports();
    let v1_lines = lines_of(&reports, old_id);
    assert_eq!(v1_lines.len(), 1, "{v1_lines:?}");
    let line = &v1_lines[0];
    let mut keys: Vec<&str> = line
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "agent_id",
            "cpu_usage",
            "hostname",
            "peer",
            "recv_at",
            "schema",
            "seq",
            "total_memory",
            "ts",
            "used_memory"
        ],
        "{line}"
    );
    assert_eq!(line["schema"], 1);
    assert_eq!(line["seq"], 3);
    assert_eq!(line["hostname"], "old-pulse");
    assert_eq!(line["cpu_usage"], 17.84);
    assert_eq!(line["total_memory"], 49152);
    assert_eq!(line["used_memory"], 37034);
    for r in lines_of(&reports, test_pki::GOOD_ID) {
        assert_eq!(r["schema"], 2, "{r}");
        assert!(
            r["metrics"].is_array() && r.get("cpu_usage").is_none(),
            "{r}"
        );
    }
    // 兩種格式在同一個檔案中（同一天）
    let files: Vec<_> = std::fs::read_dir(kairos.dir.join("data"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .collect();
    if files.len() == 1 {
        let text = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(
            text.contains(r#""schema":1"#) && text.contains(r#""schema":2"#),
            "{text}"
        );
    }

    // 關機後的 agents.json：v1 的 agent 沒有主機資訊，v2 的有
    old.close();
    k.wait_for_count("連線關閉，標記為離線", 1, WAIT);
    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
    let agents = kairos.agents();
    let find = |id: &str| {
        agents["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["agent_id"] == id)
            .unwrap_or_else(|| panic!("agents.json 沒有 {id}：{agents}"))
            .clone()
    };
    let v1 = find(old_id);
    assert_eq!(v1["proto"], 1, "{agents}");
    assert_eq!(v1["agent_version"], "0.1.0", "{agents}");
    for key in [
        "host_fingerprint",
        "display_name",
        "tags",
        "report_interval_ms",
        "os",
    ] {
        assert!(v1.get(key).is_none(), "v1 的 agent 不應有 {key}：{agents}");
    }
    assert_eq!(v1["last_report"]["cpu_usage"], 17.84, "{agents}");
    assert_eq!(v1["last_report"]["total_memory"], 49152, "{agents}");
    assert!(v1["last_report"].get("metrics").is_none(), "{agents}");
    let v2 = find(test_pki::GOOD_ID);
    assert_eq!(v2["proto"], 2, "{agents}");
    assert!(v2["os"].is_object(), "{agents}");
    assert!(!p.stderr().contains("[ERROR]"), "{}", p.dump());
}
