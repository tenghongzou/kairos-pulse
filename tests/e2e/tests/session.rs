//! 端對端：實際啟動 Kairos 與 Pulse，以 mTLS 連線（審計 Q1）。
//!
//! 每個測試使用獨立的暫存目錄與隨機埠，可平行執行。

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::Value;

use kp_e2e::{
    GOOD, KAIROS_STARTED, KAIROS_STOPPED, Kairos, PULSE_REGISTERED, PULSE_STOPPED, Pulse, REVOKED,
    TestDir, WAIT, poll_until, test_pki,
};

/// 優雅關機的時間上限（Kairos 的看門狗為 4 秒、Pulse 收尾上限 1.5 秒，加上 CI 的餘裕）
const GRACEFUL_LIMIT: Duration = Duration::from_secs(5);

/// 從 Pulse 的 trace 日誌取出含 `prefix` 的行中 `seq=` 後的數字
fn seqs(lines: &[String], prefix: &str) -> BTreeSet<u64> {
    lines
        .iter()
        .filter_map(|l| {
            let rest = &l[l.find(prefix)? + prefix.len()..];
            let rest = &rest[rest.find("seq=")? + 4..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .collect()
}

/// 協定 v2 的指標 report（`schema` 2）中沒有 label 的標準指標值
fn metric<'a>(r: &'a Value, name: &str) -> &'a Value {
    r["metrics"]
        .as_array()
        .unwrap_or_else(|| panic!("schema 2 的 report 應有 metrics：{r}"))
        .iter()
        .find(|m| m["name"] == name && m.get("labels").is_none())
        .unwrap_or_else(|| panic!("缺少 {name}：{r}"))
}

/// 這台機器的 Pulse 是否有可用的機器 ID（有才會在 hello 中送出 `host_fingerprint`）
fn has_machine_id() -> bool {
    if cfg!(windows) {
        return true; // 登錄機碼 MachineGuid 一定存在
    }
    std::fs::read_to_string("/etc/machine-id").is_ok_and(|s| !s.trim().is_empty())
}

#[test]
fn registers_heartbeats_reports_and_shuts_down_gracefully() {
    let root = TestDir::new("session");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);

    let mut k = kairos.start();
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    let registered = p.lines_with(PULSE_REGISTERED);
    assert!(
        registered[0].contains(&format!("agent_id={}", test_pki::GOOD_ID))
            && registered[0].contains("伺服器離線判定 5000 ms")
            && registered[0].contains("協定 v2"),
        "新版 Pulse 與新版 Kairos 應協商為協定 v2：{registered:?}"
    );
    // Kairos 的上線日誌也寫出協商結果
    k.wait_for("，協定 v", WAIT);
    let online = k.lines_with("，協定 v");
    assert!(
        online[0].contains(&format!("agent {}", test_pki::GOOD_ID))
            && online[0].contains("上線")
            && online[0].contains("協定 v2"),
        "{online:?}"
    );

    // report：等到至少 3 筆寫入 SQLite（以 kairos reports export 匯出），檢查每一筆的內容。
    // 協定 v2 且雙方都宣告 metrics：每行為 schema 2 的通用指標，不再有 cpu_usage／total_memory／used_memory
    assert!(
        poll_until(WAIT, || kairos.reports().len() >= 3),
        "SQLite 沒有寫入 3 筆 report：{:?}\n{}\n{}",
        kairos.reports(),
        p.dump(),
        k.dump()
    );
    let reports = kairos.reports();
    let mut last_seq = 0;
    for r in &reports {
        assert_eq!(r["agent_id"], test_pki::GOOD_ID, "{r}");
        assert!(r["peer"].as_str().unwrap().starts_with("127.0.0.1:"), "{r}");
        assert!(!r["hostname"].as_str().unwrap().is_empty(), "{r}");
        assert!(!r["recv_at"].as_str().unwrap().is_empty(), "{r}");
        assert!(!r["ts"].as_str().unwrap().is_empty(), "{r}");
        assert_eq!(r["schema"], 2, "{r}");
        for old in ["cpu_usage", "total_memory", "used_memory"] {
            assert!(r.get(old).is_none(), "schema 2 不應有 {old}：{r}");
        }
        let metrics = r["metrics"].as_array().unwrap();
        assert!(!metrics.is_empty() && metrics.len() <= 256, "{r}");
        for m in metrics {
            assert!(m["name"].as_str().unwrap().starts_with("system."), "{m}");
            assert!(m["value"].is_number(), "{m}");
            assert!(!m["unit"].as_str().unwrap().is_empty(), "{m}");
        }
        assert_eq!(metric(r, "system.cpu.usage")["unit"], "%", "{r}");
        assert_eq!(metric(r, "system.memory.total")["unit"], "By", "{r}");
        assert_eq!(metric(r, "system.memory.used")["unit"], "By", "{r}");
        let cpu = metric(r, "system.cpu.usage")["value"].as_f64().unwrap();
        assert!((0.0..=100.0).contains(&cpu), "{r}");
        // bytes 以整數保存（不會變成小數）
        let total = metric(r, "system.memory.total")["value"].as_u64().unwrap();
        let used = metric(r, "system.memory.used")["value"].as_u64().unwrap();
        assert!(total > 0 && used <= total, "{r}");
        let seq = r["seq"].as_u64().unwrap();
        assert!(seq > last_seq, "seq 應遞增：{reports:?}");
        last_seq = seq;
    }
    // JSONL 鏡像（storage.jsonl_mirror）：寫入 SQLite 的 report 也出現在 JSONL，內容相同
    assert!(
        poll_until(WAIT, || kairos.jsonl_reports().len() >= 3),
        "{:?}",
        kairos.jsonl_reports()
    );
    let mirrored = kairos.jsonl_reports();
    for (db, jsonl) in reports.iter().zip(&mirrored) {
        assert_eq!(db["seq"], jsonl["seq"]);
        assert_eq!(db["metrics"], jsonl["metrics"]);
        assert_eq!(db["recv_at"], jsonl["recv_at"]);
    }

    // heartbeat：Pulse 送出的 heartbeat 收到 Kairos 的 ack（trace 日誌中的 seq 相同）
    assert!(
        poll_until(WAIT, || {
            let sent = seqs(&p.lines_with("已送出 heartbeat"), "已送出 heartbeat");
            let acked = seqs(&p.lines_with("收到 ack"), "收到 ack");
            sent.intersection(&acked).count() >= 2
        }),
        "沒有收到 heartbeat 的 ack\n{}",
        p.dump()
    );

    // 優雅關機（Pulse）：送出 close_notify 後以 0 結束；Kairos 看到連線結束
    let (code, took) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    assert!(took < GRACEFUL_LIMIT, "Pulse 關機耗時 {took:?}");
    assert!(p.count(PULSE_STOPPED) == 1, "{}", p.dump());
    assert!(p.count("已送出 TLS close_notify") >= 1, "{}", p.dump());
    k.wait_for("連線關閉，標記為離線", WAIT);

    // 優雅關機（Kairos）：以 0 結束，最後的 agents.json 中 agent 已離線並保留最後一筆 report
    let (code, took) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
    assert!(took < GRACEFUL_LIMIT, "Kairos 關機耗時 {took:?}");
    assert_eq!(k.count(KAIROS_STARTED), 1);
    assert_eq!(k.count(KAIROS_STOPPED), 1, "{}", k.dump());
    let agents = kairos.agents();
    assert_eq!(agents["online"], 0, "{agents}");
    let agent = &agents["agents"][0];
    assert_eq!(agent["agent_id"], test_pki::GOOD_ID, "{agents}");
    assert_eq!(agent["online"], false, "{agents}");
    assert_eq!(agent["offline_after_ms"], 5000, "{agents}");
    // 協定 v2：協商出的版本與 hello 中的主機資訊
    assert_eq!(agent["proto"], 2, "{agents}");
    assert_eq!(agent["report_interval_ms"], 1000, "{agents}");
    for key in ["name", "arch"] {
        assert!(
            !agent["os"][key].as_str().unwrap_or_default().is_empty(),
            "{agents}"
        );
    }
    if has_machine_id() {
        let fp = agent["host_fingerprint"].as_str().unwrap_or_default();
        assert!(fp.starts_with("sha256:") && fp.len() == 7 + 64, "{agents}");
    }
    // 主機清冊（SQLite，A6）：關機時寫入最後的狀態（離線），以 kairos hosts list 查詢
    let hosts = kairos.hosts();
    assert_eq!(hosts.len(), 1, "{hosts:?}");
    let host = &hosts[0];
    assert_eq!(host["agent_id"], test_pki::GOOD_ID, "{host}");
    assert_eq!(host["status"], "offline", "{host}");
    assert_eq!(host["proto"], 2, "{host}");
    assert_eq!(host["identity_verified"], true, "{host}");
    assert_eq!(host["ip"], "127.0.0.1", "{host}");
    assert_eq!(
        host["host_fingerprint"], agent["host_fingerprint"],
        "{host}"
    );
    // display_name、tags 預設不送
    assert!(agent.get("display_name").is_none(), "{agents}");
    assert!(agent.get("tags").is_none(), "{agents}");
    // last_report 由標準指標換算（bytes → MB），另有指標數
    assert!(
        agent["last_report"]["total_memory"].as_u64().unwrap() > 0,
        "{agents}"
    );
    assert!(
        agent["last_report"]["metrics"].as_u64().unwrap() >= 3,
        "{agents}"
    );
    assert!(
        agent["last_report"]["seq"].as_u64().unwrap() >= last_seq,
        "{agents}"
    );
    // 正常流程不應有需要人工處理的錯誤
    assert!(!p.stderr().contains("[ERROR]"), "{}", p.dump());
}

#[test]
fn revoked_certificate_is_disconnected_and_rejected() {
    let root = TestDir::new("revoke");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let pulse = Pulse::new(&root, "pulse", kairos.port, REVOKED);

    // Linux：模擬 systemd 的 NOTIFY_SOCKET，檢查 `systemctl status` 會看到的狀態文字
    #[cfg(unix)]
    let notify = kp_e2e::NotifySocket::bind(root.path());
    #[cfg(unix)]
    let notify_env = notify.env_value();
    #[cfg(unix)]
    let envs = [("NOTIFY_SOCKET", notify_env.as_str())];
    #[cfg(not(unix))]
    let envs: [(&str, &str); 0] = [];

    let mut k = kairos.start();
    let mut p = pulse.start(&envs);
    p.wait_for(PULSE_REGISTERED, WAIT);
    assert!(
        poll_until(WAIT, || !kairos.reports().is_empty()),
        "撤銷前應能正常寫入 report\n{}",
        k.dump()
    );

    // 撤銷：換上撤銷這張憑證的 CRL。Kairos 在下一個重載週期（1 秒）重新驗證現役連線並踢掉它
    kairos.replace_crl(test_pki::CRL_REVOKE);
    k.wait_for(
        &format!(
            "agent {} 的客戶端憑證已被撤銷、不再受信任或無法判定撤銷狀態",
            test_pki::REVOKED_ID
        ),
        WAIT,
    );
    // Pulse 收到 certificate_rejected；之後每次重連都在 TLS 握手時被拒（CertificateRevoked）
    p.wait_for("certificate_rejected", WAIT);
    k.wait_for("出示的客戶端憑證已被撤銷，TLS 握手失敗", WAIT);
    p.wait_for("CertificateRevoked", WAIT);
    // 需要人工處理的錯誤也寫到 stderr（服務的 journal 看得到）
    assert!(
        poll_until(WAIT, || p.stderr().contains("[ERROR]")
            && p.stderr().contains("撤銷")),
        "stderr 應有撤銷的錯誤\n{}",
        p.dump()
    );
    // 撤銷後不再註冊成功，也不再寫入 report
    let reports_after_kick = kairos.reports().len();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(p.count(PULSE_REGISTERED), 1, "{}", p.dump());
    assert_eq!(kairos.reports().len(), reports_after_kick);
    assert!(k.is_running() && p.is_running());

    #[cfg(unix)]
    {
        let statuses = notify.statuses();
        assert!(
            statuses.iter().any(|s| s.starts_with("已註冊：127.0.0.1:")),
            "{statuses:?}"
        );
        // 註冊之後：被踢（certificate_rejected）與之後的重連（TLS alert）都顯示為「無法註冊」並提示撤銷
        let after: Vec<&String> = statuses
            .iter()
            .skip_while(|s| !s.starts_with("已註冊："))
            .collect();
        assert!(
            after
                .iter()
                .any(|s| s.starts_with("無法註冊：") && s.contains("撤銷")),
            "{statuses:?}"
        );
        assert!(
            !after.iter().skip(1).any(|s| s.starts_with("已註冊：")),
            "撤銷後不應再註冊成功：{statuses:?}"
        );
    }

    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
}

#[test]
fn pulse_reconnects_after_kairos_restart() {
    let root = TestDir::new("restart");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);

    let mut k = kairos.start();
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    assert!(
        poll_until(WAIT, || !kairos.reports().is_empty()),
        "{}",
        k.dump()
    );

    // Kairos 優雅關機：Pulse 偵測到斷線後開始重連（Kairos 停止期間連線失敗）
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
    drop(k);
    let before_restart = kairos.reports().len();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(p.is_running(), "{}", p.dump());

    // 同一個連接埠重新啟動 Kairos：主機清冊中的主機載回狀態表（A6），Pulse 自動重新註冊並繼續送 report
    let mut k = kairos.start();
    assert_eq!(k.count("已從主機清冊載回 1 台"), 1, "{}", k.dump());
    p.wait_for_count(PULSE_REGISTERED, 2, WAIT);
    k.wait_for(&format!("agent {}", test_pki::GOOD_ID), WAIT);
    assert!(
        poll_until(WAIT, || kairos.reports().len() > before_restart),
        "重啟後沒有新的 report\n{}\n{}",
        p.dump(),
        k.dump()
    );
    assert!(!p.stderr().contains("[ERROR]"), "{}", p.dump());

    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
}
