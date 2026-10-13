//! 端對端：`pulse status` 與本機狀態檔（設計包 `pulse/cli-spec.md` §4）。
//!
//! - Pulse 連上 Kairos：`pulse status` 結束代碼 0，`--json` 的欄位（連線、補送、身分、時鐘、狀態檔）正確。
//! - Kairos 停機：本機狀態「離線」、結束代碼 9，待補送筆數增加，`--brief` 與 Nagios 代碼；
//!   Kairos 恢復後補送完畢、回到線上。
//! - Pulse 結束：服務未執行（`PLS-E8003`），狀態檔記下結束代碼。
//!
//! 測試主機不一定有 systemd unit `pulse.service`（e2e 以前景行程執行 Pulse），
//! `pulse status` 以狀態檔記錄的 PID 判斷 Pulse 是否在執行（「未由服務管理員管理」）。

use std::path::Path;
use std::time::Duration;

use kp_e2e::{
    GOOD, Kairos, PULSE_REGISTERED, Pulse, TestDir, WAIT, poll_until, run_to_exit, test_pki,
};
use serde_json::Value;

/// 執行 `pulse status <args>`，回傳結束代碼、stdout、stderr
fn status(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let mut all = vec!["status"];
    all.extend_from_slice(args);
    let (code, p) = run_to_exit("pulse", dir, &all, &[]);
    (code, p.stdout(), p.stderr())
}

/// `pulse status --json`：結束代碼與解析後的文件
fn status_json(dir: &Path) -> (i32, Value) {
    let (code, out, err) = status(dir, &["--json"]);
    assert!(err.is_empty(), "--json 時 stderr 應為空：{err}");
    assert!(out.is_ascii(), "--json 應為純 ASCII：{out}");
    let doc: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}：{out}"));
    assert_eq!(doc["schema"], "pulse.status/v1", "{doc}");
    assert_eq!(doc["exit_code"], code, "{doc}");
    (code, doc)
}

/// 等到 `pulse status --json` 符合條件；逾時時 panic 並附上最後一份輸出
fn wait_status(dir: &Path, what: &str, cond: impl Fn(i32, &Value) -> bool) -> (i32, Value) {
    let mut last = (0, Value::Null);
    let ok = poll_until(WAIT, || {
        last = status_json(dir);
        cond(last.0, &last.1)
    });
    assert!(
        ok,
        "等待「{what}」逾時，最後一次：結束代碼 {}\n{:#}",
        last.0, last.1
    );
    last
}

/// 測試身分檔案的權限改為只有擁有者可讀，避免「權限過寬」（PLS-W8201／W8202）的警告
fn restrict_identity_files(dir: &Path) {
    #[cfg(unix)]
    for name in ["agent.id", "client.key"] {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[test]
fn status_follows_kairos_outage_and_pulse_exit() {
    let root = TestDir::new("status");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);
    restrict_identity_files(&pulse.dir);

    // Pulse 從沒啟動過：沒有狀態檔、也沒有服務 → 嚴重（未安裝）
    let (code, doc) = status_json(&pulse.dir);
    assert_eq!(code, 9, "{doc:#}");
    assert_eq!(doc["data"]["status"], "critical", "{doc:#}");
    assert_eq!(doc["data"]["checks"][0]["code"], "PLS-E8002", "{doc:#}");

    let mut k = kairos.start();
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);

    // ---- 線上：結束代碼 0 與欄位 ----
    let (_, doc) = wait_status(&pulse.dir, "線上", |code, d| {
        code == 0
            && d["data"]["reporting"]["window_1h"]["sampled"]
                .as_u64()
                .is_some_and(|n| n > 0)
    });
    let d = &doc["data"];
    assert_eq!(d["status"], "online", "{doc:#}");
    assert_eq!(d["agent_id"], test_pki::GOOD_ID, "{doc:#}");
    assert_eq!(d["service"]["state"], "running", "{doc:#}");
    assert_eq!(d["service"]["pid"], p.id(), "{doc:#}");
    let conn = &d["connection"];
    assert_eq!(conn["state"], "registered", "{doc:#}");
    assert_eq!(
        conn["server"],
        format!("127.0.0.1:{}", kairos.port),
        "{doc:#}"
    );
    assert_eq!(conn["mtls"], true, "{doc:#}");
    assert_eq!(conn["proto"], 2, "{doc:#}");
    assert_eq!(
        conn["features"],
        serde_json::json!(["metrics", "durable"]),
        "{doc:#}"
    );
    assert!(
        conn["tls_version"].as_str().unwrap().starts_with("TLSv1."),
        "{doc:#}"
    );
    let rep = &d["reporting"];
    assert_eq!(rep["delivery"], "durable", "{doc:#}");
    assert!(rep["delivery_reason"].is_null(), "{doc:#}");
    assert!(
        rep["last_report_at"].is_string() && rep["last_ack_at"].is_string(),
        "{doc:#}"
    );
    let buf = &rep["buffer"];
    assert_eq!(buf["state"], "ok", "{doc:#}");
    assert!(buf["dir"].as_str().unwrap().ends_with("buffer"), "{doc:#}");
    assert_eq!(buf["max_records"], 50_000, "{doc:#}");
    assert_eq!(buf["dropped_full"], 0, "{doc:#}");
    assert!(buf["last_dropped_at"].is_null(), "{doc:#}");
    assert_eq!(d["identity"]["state"], "registered", "{doc:#}");
    assert!(d["identity"]["cert_serial"].is_string(), "{doc:#}");
    assert!(d["identity"]["not_after"].is_string(), "{doc:#}");
    // Kairos 0.3 的 hello_ack 帶 server_time：時鐘偏差已量測（同一台主機，差距很小）
    let offset = d["clock"]["offset_ms"].as_i64().unwrap();
    assert!(offset.abs() < 5_000, "{doc:#}");
    assert_eq!(d["clock"]["source"], "hello_ack", "{doc:#}");
    assert_eq!(d["clock"]["valid"], true, "{doc:#}");
    assert_eq!(d["status_file"]["stale"], false, "{doc:#}");
    assert!(
        d["status_file"]["path"]
            .as_str()
            .unwrap()
            .ends_with("status.json"),
        "{doc:#}"
    );
    assert_eq!(d["issues"], serde_json::json!([]), "{doc:#}");
    // 人看的輸出（非 TTY：ASCII 標籤、不上色）
    let (code, out, err) = status(&pulse.dir, &[]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("[ OK ] 服務   Pulse 執行中（PID "), "{out}");
    assert!(
        out.contains(&format!("[ OK ] 連線   已註冊到 127.0.0.1:{}", kairos.port)),
        "{out}"
    );
    assert!(out.contains("本機狀態：線上\n結果：6 項通過。"), "{out}");
    assert!(err.is_empty(), "{err}");
    let (code, out, _) = status(&pulse.dir, &["--brief"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("online    "), "{out}");
    assert!(out.contains(" registered 127.0.0.1:"), "{out}");

    // ---- Kairos 停機：離線（9），取樣留在本機緩衝 ----
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
    drop(k);
    let (code, doc) = wait_status(&pulse.dir, "離線且有待補送", |_, d| {
        d["data"]["status"] == "offline"
            && d["data"]["reporting"]["buffer"]["pending"]
                .as_u64()
                .is_some_and(|n| n >= 2)
    });
    assert_eq!(code, 9, "{doc:#}");
    let d = &doc["data"];
    assert_eq!(d["connection"]["state"], "reconnecting", "{doc:#}");
    assert_eq!(d["connection"]["last_error_code"], "PLS-E2003", "{doc:#}");
    assert!(d["connection"]["last_registered_at"].is_string(), "{doc:#}");
    assert!(
        d["reporting"]["buffer"]["oldest_sample_at"].is_string(),
        "{doc:#}"
    );
    assert_eq!(d["reporting"]["buffer"]["replaying"], false, "{doc:#}");
    assert!(
        d["reporting"]["buffer"]["replay_eta_ms"].is_null(),
        "{doc:#}"
    );
    assert_eq!(d["checks"][1]["result"], "offline", "{doc:#}");
    assert_eq!(d["checks"][1]["code"], "PLS-E2003", "{doc:#}");
    assert_eq!(d["checks"][2]["result"], "warning", "{doc:#}");
    assert_eq!(d["issues"][0]["code"], "PLS-E2003", "{doc:#}");
    // 網路類錯誤在中斷未滿 30 分鐘時以警告呈現（cli-spec §4.5）
    assert_eq!(d["issues"][0]["severity"], "warning", "{doc:#}");
    let (code, out, err) = status(&pulse.dir, &[]);
    assert_eq!(code, 9, "{out}{err}");
    assert!(
        out.contains(&format!(
            "[DOWN] 連線   重新連線中：127.0.0.1:{} 拒絕連線（PLS-E2003）",
            kairos.port
        )),
        "{out}"
    );
    assert!(out.contains("[WARN] 回報   最後回報 "), "{out}");
    assert!(out.contains("，恢復連線後自動補送；\n"), "{out}");
    assert!(out.contains("本機狀態：離線"), "{out}");
    assert!(
        err.contains("[WARN] 警告 PLS-E2003：無法連線到 Kairos（連線被拒絕）"),
        "{err}"
    );
    let (code, out, _) = status(&pulse.dir, &["--brief"]);
    assert_eq!(code, 9);
    assert!(out.starts_with("offline   "), "{out}");
    assert!(
        out.contains(" reconnecting ") && out.contains(" PLS-E2003 backlog="),
        "{out}"
    );
    let (code, out, _) = status(&pulse.dir, &["--exit-codes=nagios"]);
    assert_eq!(code, 2, "{out}");

    // ---- Kairos 恢復：補送完畢、回到線上 ----
    let _k = kairos.start();
    let (_, doc) = wait_status(&pulse.dir, "補送完畢、回到線上", |code, d| {
        code == 0 && d["data"]["reporting"]["buffer"]["pending"] == 0
    });
    assert!(
        doc["data"]["connection"]["last_error_code"].is_null(),
        "{doc:#}"
    );

    // ---- Pulse 結束：服務未執行（嚴重，9） ----
    let pid = p.id();
    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, doc) = status_json(&pulse.dir);
    assert_eq!(code, 9, "{doc:#}");
    let d = &doc["data"];
    assert_eq!(d["status"], "critical", "{doc:#}");
    assert_eq!(d["service"]["state"], "stopped", "{doc:#}");
    assert_eq!(d["checks"][0]["code"], "PLS-E8003", "{doc:#}");
    assert_eq!(d["checks"][1]["result"], "skipped", "{doc:#}");
    let (_, out, err) = status(&pulse.dir, &[]);
    assert!(out.contains("[FAIL] 服務   Pulse 未執行"), "{out}");
    assert!(
        err.contains(&format!("Pulse（PID {pid}）已結束，結束代碼 0")),
        "{err}"
    );
    // 狀態檔：0640、最後一份記下結束代碼
    let file = pulse.dir.join("logs").join("status.json");
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(raw["schema"], "pulse.status-file/v1");
    assert_eq!(raw["running"], false);
    assert_eq!(raw["exit_code"], 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o007, 0, "狀態檔不可給其他使用者：{mode:o}");
    }
    // 暫存檔不會留下
    let leftovers: Vec<String> = std::fs::read_dir(pulse.dir.join("logs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn status_usage_errors_and_explain() {
    let root = TestDir::new("status-usage");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);
    let (code, _, err) = status(&pulse.dir, &["--bogus"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("PLS-E0001") && err.contains("pulse status"),
        "{err}"
    );
    let (code, _, err) = status(&pulse.dir, &["--brief", "--json"]);
    assert_eq!(code, 2);
    assert!(err.contains("PLS-E0003"), "{err}");
    let (code, out, _) = status(&pulse.dir, &["--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("結束代碼："), "{out}");
    let (code, p) = run_to_exit("pulse", &pulse.dir, &["explain", "PLS-W4202"], &[]);
    assert_eq!(code, 0);
    assert!(
        p.stdout().contains("PLS-W4202：本機緩衝已滿"),
        "{}",
        p.stdout()
    );
    let (code, _) = run_to_exit("pulse", &pulse.dir, &["explain", "PLS-E9999"], &[]);
    assert_eq!(code, 2);
    // 不需要連線：沒有 Kairos 也能在 1 秒內完成
    let started = std::time::Instant::now();
    let _ = status(&pulse.dir, &[]);
    assert!(started.elapsed() < Duration::from_secs(10));
}
