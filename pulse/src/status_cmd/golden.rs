//! 輸出 golden：以設計包 `pulse/cli-spec.md` §4.3～§4.7 與原型 `prototype/pulse/cli-scenarios.js`
//! 的情境組出狀態檔，逐字比對 `pulse status` 的輸出。
//!
//! 與設計包不同的地方（這個版本沒有的功能）在各測試中註明，並列在 `ERROR-CODES.md`「與設計包不一致之處」：
//! - 「身分」列沒有「，2027-06-09 起自動更新」（這個版本不自動更新憑證）
//! - `PLS-E2003` 的原因不寫解析出的 IP；處理方式以 `nc -vz` 取代尚未實作的 `pulse check-config --connect`
//! - `PLS-E1105` 的處理方式改為「請管理者重新簽發用戶端憑證」（`pulse enroll` 尚未實作）

use chrono::{DateTime, FixedOffset, Utc};

use super::eval::{self, ConfigView, FileState, Input};
use super::fmt::Zone;
use super::render;
use super::service::{ServiceInfo, SvcState};
use super::term::Style;
use crate::status_file::{self as sf, StatusFile};

const AG: &str = "9422cb70-1b2c-4d3e-9f00-112233445566";
const SRV: &str = "kairos.example.gov.tw:8443";
/// 範例的「現在」：2026-10-09 14:35:02 UTC+8
const NOW: &str = "2026-10-09T06:35:02.418Z";
const MONO: u64 = 900_000_000;

fn utc(s: &str) -> DateTime<Utc> {
    sf::parse_ts(s).unwrap()
}

fn unicode() -> Style {
    Style {
        unicode: true,
        color: false,
    }
}

/// 線上、連線中、沒有積壓的基本狀態檔（cli-spec §4.3）
fn base_file() -> StatusFile {
    StatusFile {
        schema: sf::SCHEMA.into(),
        updated_at: "2026-10-09T06:35:00.002Z".into(),
        mono_ms: Some(MONO - 2_416),
        pid: 4151145,
        started_at: "2026-10-06T02:31:10.000Z".into(),
        starts_1h: vec![],
        running: true,
        version: "0.2.0".into(),
        commit: "a67c715".into(),
        agent_id: Some(AG.into()),
        hostname: Some("plc-hmi-07".into()),
        working_dir: Some("/etc/pulse".into()),
        config_file: Some("/etc/pulse/pulse.toml".into()),
        report_interval_ms: 5000,
        service: sf::ServiceHint {
            manager: Some("systemd".into()),
            unit: Some("pulse.service".into()),
        },
        connection: sf::Connection {
            state: "registered".into(),
            server: SRV.into(),
            tls_version: Some("TLSv1.3".into()),
            mtls: true,
            server_verified: true,
            proto: Some(2),
            features: vec!["metrics".into(), "durable".into()],
            since: Some("2026-10-06T02:31:12.000Z".into()),
            last_registered_at: Some("2026-10-06T02:31:12.000Z".into()),
            ..sf::Connection::default()
        },
        reporting: sf::Reporting {
            last_report_at: Some("2026-10-09T06:34:59.000Z".into()),
            last_ack_at: Some("2026-10-09T06:34:59.120Z".into()),
            window_1h: sf::Window1h {
                sampled: 720,
                acked: 720,
                ..sf::Window1h::default()
            },
            delivery: "durable".into(),
            delivery_reason: None,
            replay_interval_ms: 1200,
            buffer: sf::Buffer {
                enabled: true,
                state: "ok".into(),
                dir: Some("/opt/pulse/buffer".into()),
                pending: Some(0),
                pending_bytes: Some(0),
                max_bytes: Some(67_108_864),
                max_records: Some(50_000),
                max_age_hours: Some(168),
                usage_ratio: Some(0.0),
                dropped_full: Some(0),
                dropped_expired: Some(0),
                dropped_rejected: Some(0),
                write_failures: Some(0),
                ..sf::Buffer::default()
            },
        },
        identity: sf::Identity {
            client_cert: Some("/opt/pulse/client.pem".into()),
            cert_serial: Some("10:02".into()),
            not_before: Some("2026-10-09T06:25:00.000Z".into()),
            not_after: Some("2027-10-09T06:25:00.000Z".into()),
            ..sf::Identity::default()
        },
        clock: sf::Clock {
            offset_ms: Some(210),
            measured_at: Some("2026-10-09T06:22:10.000Z".into()),
            measured_mono_ms: Some(MONO - 800_000),
            source: Some("hello_ack".into()),
        },
        ..StatusFile::default()
    }
}

fn running_service() -> Option<ServiceInfo> {
    Some(ServiceInfo {
        manager: "systemd",
        unit: "pulse.service".into(),
        state: SvcState::Running,
        main_pid: Some(4151145),
        exit_code: None,
        exit_at: None,
    })
}

fn input(file: StatusFile) -> Input {
    Input {
        file: FileState::Ok(Box::new(file)),
        status_path: "/opt/pulse/logs/status.json".into(),
        service: running_service(),
        pid_alive: Some(true),
        config: ConfigView {
            file: Some("/etc/pulse/pulse.toml".into()),
            expected: "/etc/pulse/pulse.toml".into(),
            error: None,
        },
        now: utc(NOW),
        now_mono: Some(MONO),
        zone: Zone::Fixed(FixedOffset::east_opt(8 * 3600).unwrap()),
        windows: false,
        hostname: "plc-hmi-07".into(),
        agent_id: Some(AG.into()),
        version: "0.3.0".into(),
        commit: "b81e2d4".into(),
    }
}

fn assert_text(actual: &str, expected: &str) {
    if actual != expected {
        let a: Vec<&str> = actual.lines().collect();
        let e: Vec<&str> = expected.lines().collect();
        for (i, (x, y)) in a.iter().zip(&e).enumerate() {
            assert_eq!(x, y, "第 {} 行不同\n--- 實際 ---\n{actual}", i + 1);
        }
        assert_eq!(a.len(), e.len(), "行數不同\n--- 實際 ---\n{actual}");
        assert_eq!(actual, expected);
    }
}

#[test]
fn online() {
    // cli-spec §4.3（身分列沒有「起自動更新」）
    let report = eval::evaluate(&input(base_file()));
    assert_text(
        &render::rows(&report, &unicode()),
        "\
Pulse 0.2.0 (a67c715) · plc-hmi-07 · agent_id 9422cb70-1b2c-4d3e-9f00-112233445566

✓ 通過 服務   pulse.service 執行中（PID 4151145，已執行 3 天 4 小時）
✓ 通過 連線   已註冊到 kairos.example.gov.tw:8443（TLS 1.3，mTLS），已連線 3 天 4 小時
✓ 通過 回報   最後回報 3 秒前；最近 1 小時 720／720 筆已送達
✓ 通過 身分   憑證有效至 2027-10-09（剩餘 365 天）
✓ 通過 設定   /etc/pulse/pulse.toml，無警告
✓ 通過 時鐘   與 Kairos 相差 0.2 秒

本機狀態：線上
結果：6 項通過。
",
    );
    assert_eq!(render::issues(&report, &unicode(), false, false), "");
    assert_eq!(report.status.exit_code(), 0);
    // cli-spec §4.7
    assert_eq!(
        report.brief,
        "online    plc-hmi-07 registered kairos.example.gov.tw:8443 cert=365d"
    );
}

/// cli-spec §4.5 的離線情境
fn offline_file() -> StatusFile {
    let mut f = base_file();
    f.connection = sf::Connection {
        state: "reconnecting".into(),
        server: SRV.into(),
        mtls: true,
        server_verified: true,
        since: Some("2026-10-09T06:22:32.000Z".into()),
        consecutive_failures: 9,
        next_retry_at: Some("2026-10-09T06:35:54.418Z".into()),
        last_error_code: Some("PLS-E2003".into()),
        last_error: Some("TCP 連線失敗：Connection refused (os error 111)".into()),
        last_registered_at: Some("2026-10-09T06:22:10.000Z".into()),
        ..sf::Connection::default()
    };
    f.reporting.last_report_at = Some("2026-10-09T06:22:28.418Z".into());
    f.reporting.window_1h = sf::Window1h {
        sampled: 720,
        acked: 571,
        buffered: 149,
        ..sf::Window1h::default()
    };
    let b = &mut f.reporting.buffer;
    b.pending = Some(149);
    b.pending_bytes = Some(223_500);
    b.oldest_sample_at = Some("2026-10-09T06:22:33.418Z".into());
    b.usage_ratio = Some(0.003);
    f
}

#[test]
fn offline() {
    // cli-spec §4.5：逐字相同
    let report = eval::evaluate(&input(offline_file()));
    assert_text(
        &render::rows(&report, &unicode()),
        "\
Pulse 0.2.0 (a67c715) · plc-hmi-07 · agent_id 9422cb70-1b2c-4d3e-9f00-112233445566

✓ 通過 服務   pulse.service 執行中（PID 4151145，已執行 3 天 4 小時）
○ 離線 連線   重新連線中：kairos.example.gov.tw:8443 拒絕連線（PLS-E2003）
              已中斷 12 分 30 秒，連續失敗 9 次，下次重試 52 秒後
              上次註冊成功：2026-10-09 14:22:10 UTC+8
⚠ 警告 回報   最後回報 12 分 34 秒前
              待補送 149 筆（最舊 14:22:33 UTC+8，12 分 29 秒前），恢復連線後自動補送；
              本機緩衝已用 0.3%（上限 64 MiB／50,000 筆，最多保留 7 天）
✓ 通過 身分   憑證有效至 2027-10-09（剩餘 365 天）
✓ 通過 設定   /etc/pulse/pulse.toml，無警告
✓ 通過 時鐘   與 Kairos 相差 0.2 秒（上次連線時量測）

本機狀態：離線
結果：1 項離線、1 個警告、4 項通過。
",
    );
    // 原因不寫 IP；檢查網路路徑改用 nc（pulse check-config --connect 尚未實作）
    assert_text(
        &render::issues(&report, &unicode(), false, false),
        "
⚠ 警告 PLS-E2003：無法連線到 Kairos（連線被拒絕）
  原因：kairos.example.gov.tw 的 8443 連接埠拒絕連線，已持續 12 分 30 秒。
        多半是 Kairos 正在重新啟動或維護；也可能是連接埠改變或防火牆規則變更。
  處理方式：服務會自動重連，通常不需處理。超過 30 分鐘仍未恢復時：
    1. 向 Kairos 管理者確認服務狀態。
    2. 檢查網路路徑：
         nc -vz kairos.example.gov.tw 8443
  說明：pulse explain PLS-E2003
",
    );
    assert_eq!(report.status.exit_code(), 9);
    assert_eq!(report.status.nagios_code(), 2);
    assert_eq!(
        report.brief,
        "offline   plc-hmi-07 reconnecting 12m30s PLS-E2003 backlog=149"
    );
    // ASCII 模式（非 TTY、舊主控台）：6 欄寬的標籤，欄位對齊不變
    let ascii = render::rows(&report, &Style::PLAIN);
    assert!(
        ascii.contains("\n[ OK ] 服務   pulse.service 執行中"),
        "{ascii}"
    );
    assert!(ascii.contains("\n[DOWN] 連線   重新連線中"), "{ascii}");
    assert!(ascii.contains("\n[WARN] 回報   最後回報"), "{ascii}");
    assert!(
        ascii.contains("\n              已中斷 12 分 30 秒"),
        "{ascii}"
    );
    assert!(render::issues(&report, &Style::PLAIN, false, false).contains("[WARN] 警告 PLS-E2003"));
}

#[test]
fn offline_for_30_minutes_is_critical() {
    let mut f = offline_file();
    f.connection.since = Some("2026-10-09T06:05:02.000Z".into());
    let report = eval::evaluate(&input(f));
    assert_eq!(report.status, eval::HostStatus::Critical);
    let rows = render::rows(&report, &unicode());
    assert!(
        rows.contains("✗ 錯誤 連線   重新連線中：kairos.example.gov.tw:8443 拒絕連線（PLS-E2003）"),
        "{rows}"
    );
    assert!(render::issues(&report, &unicode(), false, false).contains("✗ 錯誤 PLS-E2003："));
    assert!(
        report
            .brief
            .starts_with("critical  plc-hmi-07 reconnecting 30m"),
        "{}",
        report.brief
    );
}

/// cli-spec §4.5a：恢復連線後補送中，本機緩衝曾滿
fn buffer_file() -> StatusFile {
    let mut f = base_file();
    f.version = "0.3.0".into();
    f.commit = "b81e2d4".into();
    f.started_at = "2026-09-27T02:31:10.000Z".into();
    f.connection.since = Some("2026-10-09T06:33:00.418Z".into());
    f.connection.last_registered_at = f.connection.since.clone();
    f.connection.last_outage_from = Some("2026-10-06T04:00:00.000Z".into());
    f.connection.last_outage_until = Some("2026-10-09T06:33:00.000Z".into());
    f.reporting.last_report_at = Some("2026-10-09T06:35:00.418Z".into());
    f.reporting.window_1h = sf::Window1h {
        sampled: 720,
        acked: 100,
        buffered: 720,
        ..sf::Window1h::default()
    };
    let b = &mut f.reporting.buffer;
    b.pending = Some(49_924);
    b.pending_bytes = Some(66_700_000);
    b.oldest_sample_at = Some("2026-10-06T09:14:40.418Z".into());
    b.usage_ratio = Some(0.998);
    b.dropped_full = Some(3676);
    b.last_dropped_at = Some("2026-10-09T06:32:55.000Z".into());
    b.last_dropped_full_at = b.last_dropped_at.clone();
    b.dropped_full_from = Some("2026-10-06T04:00:00.000Z".into());
    b.dropped_full_until = Some("2026-10-06T09:06:20.000Z".into());
    f.clock.measured_at = Some("2026-10-09T06:33:00.000Z".into());
    f
}

#[test]
fn replaying_after_the_buffer_was_full() {
    let report = eval::evaluate(&input(buffer_file()));
    // 身分列沒有「，2027-06-09 起自動更新」，其餘與 cli-spec §4.5a 逐字相同
    assert_text(
        &render::rows(&report, &unicode()),
        "\
Pulse 0.3.0 (b81e2d4) · plc-hmi-07 · agent_id 9422cb70-1b2c-4d3e-9f00-112233445566

✓ 通過 服務   pulse.service 執行中（PID 4151145，已執行 12 天 4 小時）
✓ 通過 連線   已註冊到 kairos.example.gov.tw:8443（TLS 1.3，mTLS），已連線 2 分 2 秒
⚠ 警告 回報   最後回報 2 秒前
              補送中：待補送 49,924 筆（最舊 2026-10-06 17:14:40 UTC+8，2 天 21 小時前），
              預計約 21 小時 54 分補完
              本機緩衝曾滿：已丟掉最舊的 3,676 筆（2026-10-06 12:00～17:06 的取樣，PLS-W4202）
✓ 通過 身分   憑證有效至 2027-10-09（剩餘 365 天）
✓ 通過 設定   /etc/pulse/pulse.toml，無警告
✓ 通過 時鐘   與 Kairos 相差 0.2 秒

本機狀態：警告
結果：1 個警告、5 項通過。
",
    );
    // copywriting §4.2 範例 69／cli-spec §4.5a：逐字相同
    assert_text(
        &render::issues(&report, &unicode(), false, false),
        "
⚠ 警告 PLS-W4202：本機緩衝已滿，最舊的待補送資料已丟棄
  原因：與 Kairos 中斷期間（2026-10-06 12:00～10-09 14:33 UTC+8，約 3 天 2 小時），待補送資料
        達到上限 buffer.max_records（50,000 筆）。Pulse 丟掉最舊的 3,676 筆
        （2026-10-06 12:00～17:06 的取樣），這段資料不會補送。
  處理方式：
    1. 本機不需處理：其餘的資料正在自動補送。
    2. 預期會有更長的中斷時，調大 pulse.toml 的 [buffer] max_records 與 max_mb，
       並讓 buffer.max_age_hours 不超過 Kairos 原始資料的保留期。
  說明：pulse explain PLS-W4202
",
    );
    assert_eq!(report.status.exit_code(), 8);
    // cli-spec §4.7
    assert_eq!(
        report.brief,
        "warning   plc-hmi-07 registered backlog=49924 replay_eta=21h54m dropped=3676 PLS-W4202"
    );
    // 只是補送中、沒有丟棄：回報列通過，結束代碼 0
    let mut f = buffer_file();
    f.reporting.buffer.last_dropped_full_at = Some("2026-10-08T06:00:00.000Z".into());
    let report = eval::evaluate(&input(f));
    assert_eq!(report.status.exit_code(), 0);
    assert!(render::rows(&report, &unicode()).contains("✓ 通過 回報   最後回報 2 秒前\n"));
}

#[test]
fn replay_buffer_json() {
    // cli-spec §4.8、原型 status-buffer-json
    let f = buffer_file();
    let inp = input(f);
    let report = eval::evaluate(&inp);
    let text = super::json::document(&super::json::JsonInput {
        report: &report,
        file: &inp.file,
        status_path: &inp.status_path,
        service_manager: Some("systemd"),
        service_unit: Some("pulse.service"),
        config_file: None,
        exit_code: report.status.exit_code(),
        generated_at: inp.now,
    });
    assert!(text.is_ascii());
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["schema"], "pulse.status/v1");
    assert_eq!(v["ok"], true);
    assert_eq!(v["exit_code"], 8);
    assert_eq!(v["generated_at"], NOW);
    assert_eq!(v["warnings"], serde_json::json!([]));
    assert!(v["error"].is_null());
    let d = &v["data"];
    assert_eq!(d["status"], "warning");
    assert_eq!(d["agent_id"], AG);
    assert_eq!(d["hostname"], "plc-hmi-07");
    assert_eq!(d["config_file"], "/etc/pulse/pulse.toml");
    assert_eq!(d["service"]["manager"], "systemd");
    assert_eq!(d["service"]["unit"], "pulse.service");
    assert_eq!(d["service"]["state"], "running");
    assert_eq!(d["service"]["pid"], 4151145);
    assert_eq!(d["service"]["restarts_1h"], 0);
    assert_eq!(d["connection"]["state"], "registered");
    assert_eq!(d["connection"]["tls_version"], "TLSv1.3");
    assert_eq!(d["connection"]["mtls"], true);
    let r = &d["reporting"];
    assert_eq!(r["delivery"], "durable");
    assert!(r["delivery_reason"].is_null());
    assert_eq!(r["window_1h"]["sampled"], 720);
    let b = &r["buffer"];
    assert_eq!(b["state"], "ok");
    assert_eq!(b["pending"], 49_924);
    assert_eq!(b["oldest_sample_at"], "2026-10-06T09:14:40.418Z");
    assert_eq!(b["max_records"], 50_000);
    assert_eq!(b["usage_ratio"], 0.998);
    assert_eq!(b["replaying"], true);
    let eta = b["replay_eta_ms"].as_u64().unwrap();
    assert!((78_820_000..78_830_000).contains(&eta), "{eta}");
    assert_eq!(b["dropped_full"], 3676);
    assert_eq!(b["last_dropped_at"], "2026-10-09T06:32:55.000Z");
    assert_eq!(b["counters_since"], "2026-09-27T02:31:10.000Z");
    assert_eq!(d["identity"]["state"], "registered");
    assert_eq!(d["identity"]["cert_serial"], "10:02");
    assert_eq!(d["identity"]["rotation"], "none");
    assert_eq!(d["clock"]["offset_ms"], 210);
    assert_eq!(d["clock"]["source"], "hello_ack");
    assert_eq!(d["clock"]["valid"], true);
    assert_eq!(d["status_file"]["path"], "/opt/pulse/logs/status.json");
    assert_eq!(d["status_file"]["stale"], false);
    let checks: Vec<(String, String)> = d["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["id"].as_str().unwrap().to_string(),
                c["result"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        checks,
        [
            ("service", "ok"),
            ("connection", "ok"),
            ("reporting", "warning"),
            ("identity", "ok"),
            ("config", "ok"),
            ("clock", "ok"),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
    );
    assert_eq!(d["checks"][2]["code"], "PLS-W4202");
    let issue = &d["issues"][0];
    assert_eq!(issue["code"], "PLS-W4202");
    assert_eq!(issue["severity"], "warning");
    assert_eq!(issue["title"], "本機緩衝已滿，最舊的待補送資料已丟棄");
    assert_eq!(issue["explain"], "pulse explain PLS-W4202");
    assert_eq!(issue["details"]["dropped_full"], 3676);
    assert!(
        issue["cause"]
            .as_str()
            .unwrap()
            .contains("Pulse 丟掉最舊的 3,676 筆（2026-10-06 12:00～17:06 的取樣）")
    );
    assert_eq!(issue["fix"][0], "本機不需處理：其餘的資料正在自動補送。");
}

#[test]
fn revoked() {
    // cli-spec §4.6 第一例
    let mut f = base_file();
    f.started_at = "2026-10-09T04:35:00.000Z".into();
    f.pid = 4151889;
    f.connection = sf::Connection {
        state: "rejected".into(),
        server: SRV.into(),
        mtls: true,
        server_verified: true,
        since: Some("2026-10-09T06:22:16.000Z".into()),
        consecutive_failures: 18,
        last_error_code: Some("PLS-E1105".into()),
        last_error: Some(
            "伺服器回報致命錯誤 certificate_rejected：client certificate rejected".into(),
        ),
        last_registered_at: Some("2026-10-09T04:35:01.000Z".into()),
        last_rejection: Some(sf::Rejection {
            code: Some("PLS-E1105".into()),
            since: "2026-10-09T06:22:16.000Z".into(),
            message: "伺服器回報致命錯誤 certificate_rejected：client certificate rejected".into(),
        }),
        ..sf::Connection::default()
    };
    f.reporting.last_report_at = Some("2026-10-09T04:34:00.000Z".into());
    f.clock.offset_ms = Some(300);
    let mut inp = input(f);
    if let Some(s) = inp.service.as_mut() {
        s.main_pid = Some(4151889);
    }
    let report = eval::evaluate(&inp);
    // 時鐘列多了「（上次連線時量測）」：被拒期間沒有新的量測
    assert_text(
        &render::rows(&report, &unicode()),
        "\
Pulse 0.2.0 (a67c715) · plc-hmi-07 · agent_id 9422cb70-1b2c-4d3e-9f00-112233445566

✓ 通過 服務   pulse.service 執行中（PID 4151889，已執行 2 小時）
✗ 錯誤 連線   無法註冊：這台主機的憑證已被撤銷（PLS-E1105）
              自 2026-10-09 14:22:16 UTC+8 起已重試 18 次（服務會持續重試，但不會成功）
✗ 錯誤 回報   最後回報 2 小時 1 分前
✗ 錯誤 身分   已撤銷（序號 10:02）
✓ 通過 設定   /etc/pulse/pulse.toml，無警告
✓ 通過 時鐘   與 Kairos 相差 0.3 秒（上次連線時量測）

本機狀態：嚴重
結果：3 個錯誤、3 項通過。
",
    );
    assert_text(
        &render::issues(&report, &unicode(), false, false),
        "
✗ 錯誤 PLS-E1105：無法連線：這台主機的憑證已被撤銷
  原因：Kairos 拒絕了這台主機的憑證（序號 10:02）。通常是管理者撤銷了這台主機
        （主機汰換、疑似外洩或重新檢查）。
  處理方式：
    1. 向 Kairos 管理者確認撤銷原因。
    2. 若這台主機應繼續被監控，請管理者重新簽發用戶端憑證，更新 server.client_cert／server.client_key 後重新啟動：
         sudo systemctl restart pulse
  說明：pulse explain PLS-E1105
",
    );
    assert_eq!(report.status.exit_code(), 9);
    assert_eq!(
        report.brief,
        "critical  plc-hmi-07 rejected PLS-E1105 since=2026-10-09T06:22:16Z"
    );
    // Kairos 剛好停機（只剩拒絕連線）時，上次被拒的原因不會被蓋掉（DEPLOY-TEST N7）
    let mut f = match &inp.file {
        FileState::Ok(f) => (**f).clone(),
        _ => unreachable!(),
    };
    f.connection.state = "reconnecting".into();
    f.connection.last_error_code = Some("PLS-E2003".into());
    let report = eval::evaluate(&input(f));
    assert!(
        render::rows(&report, &unicode())
            .contains("✗ 錯誤 連線   重新連線中（上次被拒：PLS-E1105 這台主機的憑證已被撤銷）"),
        "{}",
        render::rows(&report, &unicode())
    );
    assert_eq!(report.status, eval::HostStatus::Critical);
}

#[test]
fn service_stopped() {
    // cli-spec §4.6「服務未執行時」、原型 status-stopped（Linux）
    let mut f = base_file();
    f.updated_at = "2026-10-09T06:01:30.000Z".into();
    f.reporting.last_report_at = Some("2026-10-09T06:01:28.000Z".into());
    let mut inp = input(f);
    inp.service = Some(ServiceInfo {
        manager: "systemd",
        unit: "pulse.service".into(),
        state: SvcState::Failed,
        main_pid: None,
        exit_code: Some(3),
        exit_at: Some(utc("2026-10-09T06:01:33Z")),
    });
    inp.pid_alive = Some(false);
    inp.config.error = Some((
        "PLS-E0101",
        "TOML parse error at line 4, column 1 | 4 | [server | ^ invalid table header".into(),
    ));
    let report = eval::evaluate(&inp);
    assert_text(
        &render::rows(&report, &unicode()),
        "\
Pulse 0.2.0 (a67c715) · plc-hmi-07 · agent_id 9422cb70-1b2c-4d3e-9f00-112233445566

✗ 錯誤 服務   pulse.service 未執行（上次結束：2026-10-09 14:01:33 UTC+8，結束代碼 3）
- 略過 連線   服務未執行，以下為最後已知狀態（14:01:30）
- 略過 回報   最後一筆 14:01:28
✓ 通過 身分   憑證有效至 2027-10-09（剩餘 365 天）
✗ 錯誤 設定   /etc/pulse/pulse.toml 第 4 行語法錯誤（PLS-E0101）
- 略過 時鐘   服務未執行，無法量測

本機狀態：嚴重
結果：2 個錯誤、3 項略過、1 項通過。
",
    );
    let issues = render::issues(&report, &unicode(), false, false);
    assert!(
        issues.starts_with(
            "\n✗ 錯誤 PLS-E8003：Pulse 服務未執行\n  原因：pulse.service 為 failed，最近一次結束代碼 3。最後一則錯誤為\n        PLS-E0101（設定檔語法錯誤，"
        ),
        "{issues}"
    );
    assert!(
        issues.contains("         sudo systemctl restart pulse\n"),
        "{issues}"
    );
    assert_eq!(report.status.exit_code(), 9);
    assert_eq!(
        report.brief,
        "critical  plc-hmi-07 stopped PLS-E8003 PLS-E0101"
    );
}

#[test]
fn stale_status_file_is_unknown() {
    // cli-spec §1.2：服務在執行，但狀態檔超過 60 秒未更新（PLS-W8004）
    let mut inp = input(base_file());
    inp.now_mono = Some(MONO + 130_000);
    let report = eval::evaluate(&inp);
    assert_eq!(report.status, eval::HostStatus::Unknown);
    assert_eq!(report.status.exit_code(), 9);
    assert_eq!(report.status.nagios_code(), 3);
    let rows = render::rows(&report, &unicode());
    assert!(
        rows.contains("⚠ 警告 服務   pulse.service 執行中（PID 4151145），但狀態檔已 2 分 12 秒未更新（PLS-W8004）"),
        "{rows}"
    );
    assert!(
        rows.contains("- 略過 連線   狀態檔過時，以下為最後已知狀態（14:35:00）"),
        "{rows}"
    );
    assert!(rows.contains("本機狀態：未知"), "{rows}");
    assert!(
        report.brief.starts_with("unknown   plc-hmi-07 stale 2m12s"),
        "{}",
        report.brief
    );
    // 沒有狀態檔、服務在執行：同樣是未知
    let mut inp = input(base_file());
    inp.file = FileState::Missing;
    let report = eval::evaluate(&inp);
    assert_eq!(report.status, eval::HostStatus::Unknown);
    assert!(render::rows(&report, &unicode()).contains("但沒有狀態檔（PLS-W8004）"));
}

#[test]
fn agent_id_hidden_without_mtls() {
    // 未啟用 mTLS：狀態檔不含 agent_id，讀不到 agent.id 的帳號看到「不顯示」
    let mut f = base_file();
    f.agent_id = None;
    f.agent_id_hidden = true;
    let mut inp = input(f);
    inp.agent_id = None;
    let report = eval::evaluate(&inp);
    assert!(
        render::rows(&report, &unicode())
            .starts_with("Pulse 0.2.0 (a67c715) · plc-hmi-07 · agent_id （未啟用 mTLS，不顯示）\n")
    );
    // root 讀得到 agent.id 時照常顯示
    let mut f = base_file();
    f.agent_id = None;
    f.agent_id_hidden = true;
    let report = eval::evaluate(&input(f));
    assert_eq!(report.agent_id.as_deref(), Some(AG));
}

#[test]
fn status_file_fallback_is_a_config_warning() {
    // 沿用舊 unit：狀態檔改寫在 logs/（PLS-E8204），設定列警告、結束代碼 8
    let mut f = base_file();
    f.config.warnings = vec![sf::CodeNote::new(
        "PLS-E8204",
        "無法寫入 /var/lib/pulse/status.json（Read-only file system (os error 30)），改寫在 /opt/pulse/logs/status.json",
    )];
    let report = eval::evaluate(&input(f));
    let rows = render::rows(&report, &unicode());
    assert!(
        rows.contains("⚠ 警告 設定   /etc/pulse/pulse.toml：無法寫入狀態檔（PLS-E8204）"),
        "{rows}"
    );
    assert_eq!(report.status.exit_code(), 8);
    let issues = render::issues(&report, &unicode(), false, false);
    assert!(issues.contains("StateDirectory=pulse"), "{issues}");
}

#[test]
fn not_installed_and_foreground_runs() {
    // 沒有服務、沒有狀態檔：未安裝
    let mut inp = input(base_file());
    inp.file = FileState::Missing;
    inp.service = None;
    inp.pid_alive = None;
    let report = eval::evaluate(&inp);
    assert_eq!(report.status, eval::HostStatus::Critical);
    assert!(
        render::rows(&report, &unicode()).contains("（PLS-E8002）")
            || report.rows[0].code == Some("PLS-E8002")
    );
    assert_eq!(report.brief, "critical  plc-hmi-07 not_installed PLS-E8002");
    // 前景執行（沒有服務管理員）：以狀態檔的 PID 判斷，可以是線上
    let mut inp = input(base_file());
    inp.service = None;
    let report = eval::evaluate(&inp);
    assert_eq!(report.status, eval::HostStatus::Online);
    assert!(render::rows(&report, &unicode()).contains(
        "✓ 通過 服務   Pulse 執行中（PID 4151145，已執行 3 天 4 小時；未由服務管理員管理）"
    ));
    // PID 已不在：嚴重（PLS-E8003）
    let mut inp = input(base_file());
    inp.service = None;
    inp.pid_alive = Some(false);
    let report = eval::evaluate(&inp);
    assert_eq!(report.rows[0].code, Some("PLS-E8003"));
}

#[test]
fn reporting_row_variants() {
    // 不補送（停用本機緩衝）
    let mut f = offline_file();
    f.reporting.delivery = "at_most_once".into();
    f.reporting.delivery_reason = Some("disabled".into());
    f.reporting.buffer = sf::Buffer {
        enabled: false,
        state: "disabled".into(),
        ..sf::Buffer::default()
    };
    let report = eval::evaluate(&input(f));
    let rows = render::rows(&report, &unicode());
    assert!(
        rows.contains("              不補送（buffer.enabled = false）：中斷期間的資料不會保留\n"),
        "{rows}"
    );
    assert!(
        report.brief.ends_with("delivery=at_most_once"),
        "{}",
        report.brief
    );

    // 本機緩衝無法使用（PLS-E4004，以警告呈現）
    let mut f = base_file();
    f.reporting.delivery = "at_most_once".into();
    f.reporting.delivery_reason = Some("unavailable".into());
    f.reporting.buffer = sf::Buffer {
        enabled: true,
        state: "unavailable".into(),
        dir: Some("/opt/pulse/buffer".into()),
        error: Some("Permission denied (os error 13)".into()),
        ..sf::Buffer::default()
    };
    let report = eval::evaluate(&input(f));
    let rows = render::rows(&report, &unicode());
    assert!(
        rows.contains("⚠ 警告 回報   最後回報 3 秒前；最近 1 小時 720／720 筆已送達\n              本機緩衝無法使用（PLS-E4004）：本次執行中斷期間的資料不會補送\n"),
        "{rows}"
    );
    assert_eq!(report.status.exit_code(), 8);
    let issues = render::issues(&report, &unicode(), false, false);
    assert!(
        issues.contains("⚠ 警告 PLS-E4004：無法開啟本機緩衝，斷線期間的資料不會補送"),
        "{issues}"
    );
    assert!(
        issues.contains("sudo install -d -o pulse -g pulse -m 750 /opt/pulse/buffer"),
        "{issues}"
    );

    // 連上的 Kairos 不支援補送
    let mut f = base_file();
    f.reporting.delivery = "at_most_once".into();
    f.reporting.delivery_reason = Some("server".into());
    let report = eval::evaluate(&input(f));
    assert!(
        render::rows(&report, &unicode())
            .contains("              連上的 Kairos 不支援補送（0.2 以前）\n")
    );
    assert_eq!(report.status.exit_code(), 0);

    // 無法寫入本機緩衝（最近 1 小時）與超過保存時間
    let mut f = base_file();
    let b = &mut f.reporting.buffer;
    b.write_failures = Some(37);
    b.last_write_failure_at = Some("2026-10-09T06:20:00.000Z".into());
    b.last_write_error = Some("No space left on device (os error 28)".into());
    b.dropped_expired = Some(1152);
    b.last_dropped_expired_at = Some("2026-10-09T06:00:00.000Z".into());
    let report = eval::evaluate(&input(f));
    let rows = render::rows(&report, &unicode());
    assert!(
        rows.contains("              超過保存時間（7 天）而丟掉 1,152 筆（PLS-W4203）\n"),
        "{rows}"
    );
    assert!(
        rows.contains("              無法寫入本機緩衝：37 筆取樣遺失（PLS-W4103）\n"),
        "{rows}"
    );
    let codes: Vec<&str> = report.issues.iter().map(|i| i.code.as_str()).collect();
    assert_eq!(codes, ["PLS-W4203", "PLS-W4103"]);

    // Kairos 暫時無法保存而退避
    let mut f = buffer_file();
    f.reporting.buffer.last_dropped_full_at = None;
    f.reporting.buffer.pending = Some(1284);
    f.reporting.buffer.oldest_sample_at = Some("2026-10-09T06:03:12.000Z".into());
    f.reporting.buffer.retry_at = Some("2026-10-09T06:35:34.418Z".into());
    f.reporting.buffer.retry_code = Some("PLS-W4101".into());
    let report = eval::evaluate(&input(f));
    let rows = render::rows(&report, &unicode());
    assert!(
        rows.contains("              Kairos 暫時無法保存（PLS-W4101）：待補送 1,284 筆（最舊 14:03:12 UTC+8），32 秒後重送\n"),
        "{rows}"
    );
    assert_eq!(report.status.exit_code(), 8);
}

#[test]
fn identity_and_clock_variants() {
    // 憑證 14 天內到期（不會自動更新）
    let mut f = base_file();
    f.identity.not_after = Some("2026-10-18T06:25:00.000Z".into());
    let report = eval::evaluate(&input(f));
    let rows = render::rows(&report, &unicode());
    assert!(
        rows.contains("⚠ 警告 身分   憑證剩餘 9 天，不會自動更新（PLS-W1110）"),
        "{rows}"
    );
    assert_eq!(
        report.brief,
        "warning   plc-hmi-07 registered cert=9d PLS-W1110"
    );

    // 依本機時間已過期、但沒有有效的時鐘偏差：警告（尚未經 Kairos 確認）
    let mut f = base_file();
    f.identity.not_after = Some("2026-09-28T06:25:00.000Z".into());
    f.clock = sf::Clock::default();
    let report = eval::evaluate(&input(f));
    assert!(render::rows(&report, &unicode()).contains(
        "⚠ 警告 身分   依本機時間，憑證已於 2026-09-28 過期；尚未經 Kairos 確認（本機時鐘可能不準），正在連線確認"
    ));
    // 有有效的時鐘偏差：錯誤
    let mut f = base_file();
    f.identity.not_after = Some("2026-09-28T06:25:00.000Z".into());
    let report = eval::evaluate(&input(f));
    assert!(
        render::rows(&report, &unicode())
            .contains("✗ 錯誤 身分   憑證已於 2026-09-28 過期（11 天前）（PLS-E1108）")
    );

    // 權限過寬
    let mut f = base_file();
    f.identity.warnings = vec![sf::CodeNote::new("PLS-W8202", "client.key 的權限為 0644")];
    let report = eval::evaluate(&input(f));
    assert!(render::rows(&report, &unicode()).contains(
        "⚠ 警告 身分   憑證有效至 2027-10-09（剩餘 365 天）；私鑰檔案權限過寬（PLS-W8202）"
    ));

    // 未啟用 mTLS
    let mut f = base_file();
    f.identity = sf::Identity::default();
    f.connection.mtls = false;
    let report = eval::evaluate(&input(f));
    let rows = render::rows(&report, &unicode());
    assert!(
        rows.contains("✓ 通過 身分   未使用用戶端憑證（mTLS 未啟用），以 agent_id 識別"),
        "{rows}"
    );
    assert!(rows.contains("（TLS 1.3），已連線"), "{rows}");

    // 時鐘偏差 30 秒～5 分鐘、超過 5 分鐘
    let mut f = base_file();
    f.clock.offset_ms = Some(-45_000);
    let report = eval::evaluate(&input(f));
    assert!(
        render::rows(&report, &unicode()).contains("⚠ 警告 時鐘   比 Kairos 慢 45 秒（PLS-W8301）")
    );
    let mut f = base_file();
    f.clock.offset_ms = Some(462_000);
    let report = eval::evaluate(&input(f));
    assert!(
        render::rows(&report, &unicode())
            .contains("✗ 錯誤 時鐘   比 Kairos 快 7 分 42 秒（PLS-E8302）")
    );
    assert_eq!(report.status.exit_code(), 9);

    // 設定警告
    let mut f = base_file();
    f.config.warnings = vec![sf::CodeNote::new(
        "PLS-W0302",
        "已停用伺服器憑證驗證（accept_invalid_certs = true）",
    )];
    let report = eval::evaluate(&input(f));
    assert!(
        render::rows(&report, &unicode())
            .contains("⚠ 警告 設定   /etc/pulse/pulse.toml：已停用伺服器憑證驗證（PLS-W0302）")
    );
    assert_eq!(report.status.exit_code(), 8);
}

#[test]
fn windows_service_labels() {
    let mut inp = input(base_file());
    inp.windows = true;
    inp.service = Some(ServiceInfo {
        manager: "scm",
        unit: "Pulse".into(),
        state: SvcState::Running,
        main_pid: Some(2216),
        exit_code: None,
        exit_at: None,
    });
    let report = eval::evaluate(&inp);
    assert!(
        render::rows(&report, &unicode())
            .contains("✓ 通過 服務   Pulse 服務執行中（PID 4151145，已執行 3 天 4 小時）")
    );
    inp.service = Some(ServiceInfo {
        manager: "scm",
        unit: "Pulse".into(),
        state: SvcState::Stopped,
        main_pid: None,
        exit_code: None,
        exit_at: None,
    });
    let report = eval::evaluate(&inp);
    let rows = render::rows(&report, &unicode());
    assert!(rows.contains("✗ 錯誤 服務   Pulse 服務已停止"), "{rows}");
    assert!(render::issues(&report, &unicode(), false, false).contains("Restart-Service Pulse"));
}
