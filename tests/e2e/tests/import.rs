//! 端對端：`kairos reports import`（匯入 0.2.x 的 JSONL）以實際的執行檔執行。
//!
//! - Kairos 執行中時拒絕匯入（結束代碼 11，資料庫使用中；檔案鎖跨行程），`--dry-run` 可以；
//! - 停止 Kairos 後匯入，重複匯入不重複寫入；
//! - Kairos 再啟動後（資料庫照常開啟）以 `reports export` 取出匯入的 report，主機出現在清冊中（離線）。

use std::time::{SystemTime, UNIX_EPOCH};

use kp_e2e::{Kairos, TestDir, run_to_exit, test_pki};
use serde_json::{Value, json};

/// Unix 毫秒 → `YYYY-MM-DDTHH:MM:SS.mmmZ`（civil from days，Howard Hinnant 的演算法；不另外引入 chrono）
fn iso_ms(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

const AGENT_V1: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
const AGENT_V2: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

fn import(k: &Kairos, args: &[&str]) -> (i32, String, String) {
    let mut all = vec!["reports", "import"];
    all.extend_from_slice(args);
    all.extend_from_slice(&["--config", "kairos.toml", "--ascii"]);
    let (code, p) = run_to_exit("kairos", &k.dir, &all, &[]);
    (code, p.stdout(), p.stderr())
}

#[test]
fn import_refuses_a_running_kairos_and_the_data_survives_a_restart() {
    let root = TestDir::new("import");
    let k = Kairos::new(&root, test_pki::CRL_EMPTY);
    // 0.2.x 的資料目錄：0.1.0 的行（沒有 schema）、schema 2 的行與一個壞行
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let recv = now - 2 * 3_600_000;
    let old = k.dir.join("old-data");
    std::fs::create_dir_all(&old).unwrap();
    let v1 = json!({
        "recv_at": iso_ms(recv), "peer": "127.0.0.1:50123", "agent_id": AGENT_V1,
        "hostname": "web-01", "seq": 6, "ts": iso_ms(recv - 90),
        "cpu_usage": 17.84, "total_memory": 49152, "used_memory": 37034
    });
    let v2 = json!({
        "schema": 2, "recv_at": iso_ms(recv + 5000), "peer": "127.0.0.1:50124", "agent_id": AGENT_V2,
        "hostname": "web-02", "seq": 3, "ts": iso_ms(recv + 4900),
        "metrics": [{"name": "system.cpu.usage", "value": 17.84, "unit": "%"}]
    });
    std::fs::write(
        old.join(format!("reports-{}.jsonl", &iso_ms(recv)[..10])),
        format!("{v1}\n{v2}\nnot json\n"),
    )
    .unwrap();

    let mut kairos = k.start();
    let (code, out, err) = import(&k, &["old-data"]);
    assert_eq!(code, 11, "Kairos 執行中應拒絕匯入\n{out}{err}");
    assert!(err.contains("KRS-E4304") && err.contains("使用中"), "{err}");
    // 試執行只讀取，Kairos 執行中也可以
    let (code, out, err) = import(&k, &["old-data", "--dry-run"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("將匯入      2 筆"), "{out}");
    kairos.stop();

    let (code, out, err) = import(&k, &["old-data"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        out.contains("匯入        2 筆（原始資料 2、只進彙總 0）"),
        "{out}"
    );
    assert!(out.contains("壞行        1"), "{out}");
    assert!(err.contains("第 3 行（不是合法的 JSON）"), "{err}");
    // 再匯入一次：全部為重複；--json 為純 ASCII 的單一文件
    let (code, out, err) = import(&k, &["old-data", "--json"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.is_ascii() && err.is_empty(), "{out}{err}");
    let doc: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(doc["schema"], "kairos.reports.import/v1");
    assert_eq!(doc["data"]["totals"]["duplicates"], 2, "{doc}");
    assert_eq!(doc["data"]["totals"]["imported"], 0, "{doc}");

    // Kairos 照常啟動，匯入的資料在資料庫中
    let mut kairos = k.start();
    let reports = k.reports();
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert_eq!(reports[0]["agent_id"], AGENT_V1);
    assert_eq!(reports[0]["schema"], 1);
    assert_eq!(reports[0]["recv_at"], v1["recv_at"]);
    assert_eq!(reports[0]["ts"], v1["ts"]);
    assert_eq!(reports[1]["metrics"], v2["metrics"]);
    let hosts = k.hosts();
    assert_eq!(hosts.len(), 2, "{hosts:?}");
    assert!(hosts.iter().all(|h| h["status"] == "offline"), "{hosts:?}");
    kairos.stop();
}
