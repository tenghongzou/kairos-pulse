//! 端對端：durable（at-least-once，審計 A5）。
//!
//! - Pulse 持續送資料時讓 Kairos 停機一段時間再啟動：停機期間的取樣全部補到資料檔，沒有重複。
//! - Kairos 寫入了、但 client 沒收到 `ack`（結果不明）時重送：回 `ack`、不重寫；Kairos 重新啟動後
//!   （SQLite 的唯一索引）也一樣。Kairos 真的「寫入逾時、結果不明而斷線」需要讓磁碟變慢，
//!   這裡以「已寫入、client 卻沒收到 ack 就斷線」重現 client 看到的同一種情況。

use std::collections::BTreeMap;
use std::time::Duration;

use kp_e2e::{
    GOOD, Kairos, PULSE_REGISTERED, Pulse, REVOKED, RawClient, TestDir, WAIT, poll_until, test_pki,
};
use serde_json::{Value, json};

/// Kairos 停機的時間（Pulse 每秒取樣一次，期間約有這麼多筆寫進本機緩衝）
const OUTAGE: Duration = Duration::from_secs(5);

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` → Unix 毫秒（測試只需要相減，不另外引入 chrono）
fn unix_ms(ts: &str) -> i64 {
    let n = |r: std::ops::Range<usize>| -> i64 { ts[r].parse().unwrap() };
    let (y, m, d) = (n(0..4), n(5..7), n(8..10));
    // days from civil（Howard Hinnant 的演算法）
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    ((days * 24 + n(11..13)) * 60 + n(14..16)) * 60_000 + n(17..19) * 1000 + n(20..23)
}

#[test]
fn unix_ms_matches_known_values() {
    assert_eq!(unix_ms("1970-01-01T00:00:00.000Z"), 0);
    assert_eq!(unix_ms("2026-10-08T03:15:45.210Z"), 1_791_429_345_210);
}

/// 資料檔中某 agent 的 durable 行：boot_id → [(report_seq, ts, recv_at)]
fn durable_lines(
    reports: &[Value],
    agent_id: &str,
) -> BTreeMap<String, Vec<(u64, String, String)>> {
    let mut out: BTreeMap<String, Vec<(u64, String, String)>> = BTreeMap::new();
    for r in reports.iter().filter(|r| r["agent_id"] == agent_id) {
        let boot = r["boot_id"]
            .as_str()
            .unwrap_or_else(|| panic!("協商了 durable 的 report 應有 boot_id：{r}"));
        out.entry(boot.to_string()).or_default().push((
            r["report_seq"].as_u64().unwrap(),
            r["ts"].as_str().unwrap().to_string(),
            r["recv_at"].as_str().unwrap().to_string(),
        ));
    }
    out
}

#[test]
fn kairos_outage_is_backfilled_without_gaps_or_duplicates() {
    let root = TestDir::new("durable-outage");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);

    let mut k = kairos.start();
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    k.wait_for("durable：report 可補送、去重", WAIT);
    assert!(
        poll_until(WAIT, || kairos.reports().len() >= 2),
        "{}\n{}",
        p.dump(),
        k.dump()
    );

    // Kairos 停機：Pulse 的取樣寫進本機緩衝
    let replays_before = p.count("待補送的 report");
    let drained_before = p.count("已補送完本機緩衝中的積壓");
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
    drop(k);
    std::thread::sleep(OUTAGE);
    assert!(p.is_running(), "{}", p.dump());

    // 重新啟動：Pulse 重新註冊後先補送積壓，再送新的取樣
    let mut k = kairos.start();
    p.wait_for_count(PULSE_REGISTERED, 2, WAIT);
    p.wait_for_count("待補送的 report", replays_before + 1, WAIT);
    p.wait_for_count("已補送完本機緩衝中的積壓", drained_before + 1, WAIT);
    // 再多送幾筆即時的取樣
    let before = kairos.reports().len();
    assert!(
        poll_until(WAIT, || kairos.reports().len() >= before + 2),
        "{}",
        k.dump()
    );

    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());

    let reports = kairos.reports();
    let by_boot = durable_lines(&reports, test_pki::GOOD_ID);
    assert_eq!(
        by_boot.len(),
        1,
        "Pulse 只啟動一次，應只有一個 boot_id：{by_boot:?}"
    );
    let (boot, lines) = by_boot.into_iter().next().unwrap();
    assert!(boot.len() == 36, "{boot}");
    // 沒有重複、沒有缺號：report_seq 為 1..=n，各一行
    let seqs: Vec<u64> = lines.iter().map(|(s, ..)| *s).collect();
    let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
    assert_eq!(seqs, expected, "{lines:#?}");
    // 停機期間的取樣是補送的：收到時間比取樣時間晚 2 秒以上，且至少有停機秒數減 1 筆
    let backfilled = lines
        .iter()
        .filter(|(_, ts, recv_at)| unix_ms(recv_at) - unix_ms(ts) >= 2000)
        .count();
    assert!(
        backfilled as u64 + 1 >= OUTAGE.as_secs(),
        "補送的筆數 {backfilled}：{lines:#?}"
    );
    // 取樣時間依 report_seq 遞增（補送依序進行）
    assert!(lines.windows(2).all(|w| w[0].1 <= w[1].1), "{lines:#?}");
    assert!(!p.stderr().contains("[ERROR]"), "{}", p.dump());
}

/// 協商 durable 的 hello（模擬 0.3.0 的 Pulse）
fn durable_hello(agent_id: &str) -> Value {
    json!({
        "type": "hello", "seq": 1, "proto": 1, "proto_min": 1, "proto_max": 2,
        "features": ["metrics", "durable"], "agent_id": agent_id, "hostname": "raw-client",
        "agent_version": "0.3.0", "heartbeat_interval_ms": 1000, "ts": "2026-10-08T03:15:40.001Z"
    })
}

fn durable_report(seq: u64, boot_id: &str, report_seq: u64) -> Value {
    json!({
        "type": "report", "seq": seq, "ts": format!("2026-10-08T03:15:{:02}.000Z", report_seq),
        "metrics": [
            {"name": "system.cpu.usage", "value": 12.5, "unit": "%"},
            {"name": "system.memory.total", "value": 2048, "unit": "By"},
            {"name": "system.memory.used", "value": 1024, "unit": "By"}
        ],
        "boot_id": boot_id, "report_seq": report_seq
    })
}

fn connect_durable(kairos: &Kairos) -> RawClient {
    let mut c = RawClient::connect(kairos.port, REVOKED);
    c.send(&durable_hello(REVOKED.agent_id));
    let ack = c.recv();
    assert_eq!(ack["type"], "hello_ack", "{ack}");
    assert_eq!(ack["features"], json!(["metrics", "durable"]), "{ack}");
    c
}

#[test]
fn resend_after_unknown_result_is_acked_but_not_written_twice() {
    const BOOT: &str = "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e";
    let root = TestDir::new("durable-resend");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let mut k = kairos.start();
    let count = |report_seq: u64| {
        kairos
            .reports()
            .iter()
            .filter(|r| r["boot_id"] == BOOT && r["report_seq"] == report_seq)
            .count()
    };

    // 寫入了，但 client 沒收到 ack 就斷線（對 client 而言結果不明）
    let mut c = connect_durable(&kairos);
    c.send(&durable_report(2, BOOT, 1));
    assert!(poll_until(WAIT, || count(1) == 1), "{}", k.dump());
    drop(c);

    // 重連後重送同一筆（新的 seq）：回 ack，資料檔中仍只有一行
    let mut c = connect_durable(&kairos);
    c.send(&durable_report(2, BOOT, 1));
    assert_eq!(c.recv(), json!({"type":"ack","seq":2}));
    c.send(&durable_report(3, BOOT, 2));
    assert_eq!(c.recv(), json!({"type":"ack","seq":3}));
    assert_eq!((count(1), count(2)), (1, 1));
    c.close();

    // Kairos 重新啟動後：SQLite 的唯一索引在資料庫中，重送仍不會重複
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
    drop(k);
    let mut k = kairos.start();
    let mut c = connect_durable(&kairos);
    c.send(&durable_report(2, BOOT, 2));
    assert_eq!(c.recv(), json!({"type":"ack","seq":2}));
    c.send(&durable_report(3, BOOT, 3));
    assert_eq!(c.recv(), json!({"type":"ack","seq":3}));
    c.close();
    assert!(poll_until(WAIT, || count(3) == 1), "{}", k.dump());
    assert_eq!((count(1), count(2), count(3)), (1, 1, 1));
    // 保存的行帶 boot_id／report_seq，ts 為原本的取樣時間
    let line = kairos
        .reports()
        .into_iter()
        .find(|r| r["report_seq"] == 3)
        .unwrap();
    assert_eq!(line["ts"], "2026-10-08T03:15:03.000Z", "{line}");
    assert_eq!(line["schema"], 2, "{line}");
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
}
