//! 從 METRICS.md 解析測試向量與標準指標表（商業審計 F4）。
//!
//! 「範例」節的程式碼區塊中，`P ` 開頭的行是 Pulse → Kairos 的訊息，`K ` 開頭的是 Kairos → Pulse，
//! 依 `### <小節>` 分組。另外以凍結的 0.7.0 型別確認舊版 Kairos 看到新內容時的行為（METRICS.md
//! 「相容性」）。

use serde_json::{Value, json};

use crate::*;

/// 指標擴充規格全文（唯一的一份）
const SPEC: &str = include_str!("../METRICS.md");

/// `## <title>` 開頭的那一節（標題以 `title` 開頭即可，例如 `3. 標準指標`）的各行，到下一個 `## ` 為止
fn section(title: &str) -> Vec<&'static str> {
    let heading = format!("## {title}");
    let mut lines = SPEC.lines();
    assert!(
        lines.any(|l| l.starts_with(&heading)),
        "METRICS.md 找不到「{title}」節"
    );
    lines.take_while(|l| !l.starts_with("## ")).collect()
}

/// 「範例」節中 `### <sub>` 小節指定方向（`P` 或 `K`）的所有訊息
fn vectors(sub: &str, direction: char) -> Vec<&'static str> {
    let heading = format!("### {sub}");
    let lines = section("範例");
    let start = lines
        .iter()
        .position(|l| *l == heading)
        .unwrap_or_else(|| panic!("METRICS.md 的「範例」節找不到「{sub}」小節"));
    let prefix = format!("{direction} ");
    lines[start + 1..]
        .iter()
        .take_while(|l| !l.starts_with("### "))
        .filter_map(|l| l.strip_prefix(prefix.as_str()))
        .filter(|l| l.starts_with('{'))
        .collect()
}

const SUBSECTIONS: [&str; 4] = ["協商與 report", "inventory", "inventory（Windows）", "錯誤"];

fn one(sub: &str, direction: char, kind: &str) -> &'static str {
    let found: Vec<_> = vectors(sub, direction)
        .into_iter()
        .filter(|l| serde_json::from_str::<Value>(l).unwrap()["type"] == kind)
        .collect();
    assert_eq!(found.len(), 1, "{sub} 中 {kind} 的向量應恰好一則");
    found[0]
}

fn report_body(line: &str) -> ReportBody {
    match parse_client_msg(line).unwrap() {
        ClientMsg::Report { body, .. } => body,
        other => panic!("應解析為 report：{other:?}"),
    }
}

fn inventory_of(line: &str) -> InventoryReport {
    match parse_client_msg(line).unwrap() {
        ClientMsg::Inventory(inv) => *inv,
        other => panic!("應解析為 inventory：{other:?}"),
    }
}

#[test]
fn spec_vectors_are_found() {
    let counts: Vec<_> = SUBSECTIONS
        .iter()
        .map(|s| (vectors(s, 'P').len(), vectors(s, 'K').len()))
        .collect();
    assert_eq!(counts, [(2, 2), (1, 1), (1, 1), (0, 2)]);
}

#[test]
fn every_vector_round_trips_and_passes_the_shared_checks() {
    for sub in SUBSECTIONS {
        for line in vectors(sub, 'P') {
            let msg = parse_client_msg(line).unwrap();
            assert_ne!(msg, ClientMsg::Unknown, "{line}");
            assert_eq!(validate_client_msg(&msg), Ok(()), "{line}");
            match &msg {
                ClientMsg::Report { body, .. } => {
                    assert_eq!(validate_metrics(body.metric_list().unwrap()), Ok(()));
                    if let Some(p) = body.process_list().unwrap() {
                        assert_eq!(validate_process_list(p), Ok(()));
                    }
                    assert_eq!(validate_durable_seq(body.durable_seq().unwrap()), Ok(()));
                }
                ClientMsg::Inventory(inv) => {
                    assert_eq!(validate_inventory(&inv.inventory), Ok(()), "{line}")
                }
                _ => {}
            }
            let back: Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
            assert_eq!(back, serde_json::from_str::<Value>(line).unwrap(), "{line}");
        }
        for line in vectors(sub, 'K') {
            let msg = parse_server_msg(line).unwrap();
            if let ServerMsg::Error { code, fatal, .. } = &msg {
                let code = ErrorCode::from_code(code).expect("規格定義的錯誤碼");
                assert_eq!(code.default_fatal(), *fatal, "{line}");
            }
            let back: Value = serde_json::from_str(&msg.to_line().unwrap()).unwrap();
            assert_eq!(back, serde_json::from_str::<Value>(line).unwrap(), "{line}");
        }
    }
}

#[test]
fn the_example_session_negotiates_both_features() {
    let hello = one("協商與 report", 'P', "hello");
    let ClientMsg::Hello { proto, ext, .. } = parse_client_msg(hello).unwrap() else {
        panic!("應解析為 hello");
    };
    let range = client_proto_range(&proto, &ext.offer).unwrap();
    let version = negotiate(range, SUPPORTED_PROTO).unwrap();
    let client = ext.offer.unwrap().features.unwrap();
    let ServerMsg::HelloAck {
        proto: acked,
        features: Some(server),
        ..
    } = parse_server_msg(one("協商與 report", 'K', "hello_ack")).unwrap()
    else {
        panic!("應解析為帶 features 的 hello_ack");
    };
    assert_eq!(acked, version);
    let ours = Session::new(version, &server, &client);
    let theirs = Session::new(acked, &client, &server);
    assert_eq!(ours, theirs);
    assert!(ours.metrics && ours.durable && ours.inventory && ours.processes);
}

#[test]
fn the_example_report_uses_every_standard_metric() {
    let body = report_body(one("協商與 report", 'P', "report"));
    let metrics = body.metric_list().unwrap();
    for std in STANDARD_METRICS {
        assert!(
            metrics.iter().any(|m| m.name == std.name),
            "範例 report 缺少 {}",
            std.name
        );
    }
    // label 名稱與第 3 節一致
    for m in metrics {
        let std = standard_metric(&m.name).unwrap();
        let keys: Vec<&str> = m.labels.keys().map(String::as_str).collect();
        let mut expected = std.labels.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected, "{}", m.name);
    }
    // 使用率與 filesystem_usage 一致、used ≤ total
    let value = |name: &str, mount: &str| {
        metrics
            .iter()
            .find(|m| {
                m.name == name && m.labels.get("mountpoint").map(String::as_str) == Some(mount)
            })
            .unwrap()
            .value
            .clone()
    };
    for mount in ["/", "/data"] {
        let used = value(METRIC_FILESYSTEM_USED, mount).as_u64().unwrap();
        let avail = value(METRIC_FILESYSTEM_AVAILABLE, mount).as_u64().unwrap();
        let total = value(METRIC_FILESYSTEM_TOTAL, mount).as_u64().unwrap();
        assert!(used + avail <= total, "{mount}");
        assert_eq!(
            value(METRIC_FILESYSTEM_USAGE, mount).as_f64(),
            filesystem_usage(used, avail),
            "{mount}"
        );
    }
    let p = body.process_list().unwrap().unwrap();
    assert_eq!((p.top_cpu.len(), p.top_memory.len()), (2, 2));
    assert!(
        p.top_cpu
            .windows(2)
            .all(|w| w[0].cpu_usage >= w[1].cpu_usage)
    );
    assert!(p.top_memory.windows(2).all(|w| w[0].memory >= w[1].memory));
    assert!(
        p.top_cpu
            .iter()
            .all(|e| e.command.is_none() && e.user.is_none()),
        "預設不送命令列與使用者"
    );
}

#[test]
fn inventory_examples() {
    let linux = inventory_of(one("inventory", 'P', "inventory"));
    let windows = inventory_of(one("inventory（Windows）", 'P', "inventory"));
    assert_eq!(linux.seq, 2);
    let sys = linux.inventory.system.as_ref().unwrap();
    assert_eq!(sys.serial.as_deref(), Some("7XK2LM3"));
    assert!(windows.inventory.system.as_ref().unwrap().serial.is_none());
    assert_ne!(
        inventory_digest(&linux.inventory),
        inventory_digest(&windows.inventory)
    );
    // 資產的掛載點與 report 的 label 對應（METRICS.md 2.1）
    let body = report_body(one("協商與 report", 'P', "report"));
    for disk in linux.inventory.disks.as_ref().unwrap() {
        let device = disk.device.as_deref().unwrap();
        let metrics = body.metric_list().unwrap();
        assert!(
            metrics
                .iter()
                .any(|m| m.labels.get("mountpoint") == Some(&disk.mountpoint))
        );
        assert!(
            metrics
                .iter()
                .any(|m| m.labels.get("device").map(String::as_str) == Some(device))
        );
    }
    // 錯誤範例的 message 就是 validate_inventory 的說明
    let mut bad = linux.inventory.clone();
    bad.network_interfaces.as_mut().unwrap()[0].mac = Some("52:54:00:12:34:5G".into());
    let message = validate_inventory(&bad).unwrap_err();
    let errors: Vec<Value> = vectors("錯誤", 'K')
        .into_iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        errors
            .iter()
            .any(|e| e["code"] == "invalid_report" && e["message"] == message.as_str()),
        "{message}"
    );
}

#[test]
fn standard_metric_table_matches_constants() {
    // 第 3 節：| `name` | `unit` | 種類 | 範圍 | labels | 說明 |
    let rows: Vec<Vec<String>> = section("3. 標準指標")
        .into_iter()
        .filter(|l| l.starts_with("| `system."))
        .map(|l| {
            l.split('|')
                .map(|c| c.trim().to_string())
                .collect::<Vec<_>>()
        })
        // 6 欄的指標表（同一節中 3.2「各平台的來源」是 3 欄的表）
        .filter(|cells| cells.len() == 8)
        .collect();
    assert_eq!(rows.len(), STANDARD_METRICS.len());
    for (std, row) in STANDARD_METRICS.iter().zip(&rows) {
        let name = row[1].trim_matches('`');
        let unit = row[2].trim_matches('`');
        assert_eq!((std.name, std.unit), (name, unit));
        let kind = match row[3].as_str() {
            "量測值" => MetricKind::Gauge,
            "計數器" => MetricKind::Counter,
            other => panic!("{name} 的種類無法判讀：{other}"),
        };
        assert_eq!(std.kind, kind, "{name}");
        let range = match row[4].as_str() {
            "0～100" => (Some(0.0), Some(100.0)),
            "≥ 0" => (Some(0.0), None),
            "不限" => (None, None),
            other => panic!("{name} 的範圍無法判讀：{other}"),
        };
        assert_eq!((std.min, std.max), range, "{name}");
        let labels: Vec<&str> = if row[5] == "無" {
            vec![]
        } else {
            row[5].split('、').map(|l| l.trim_matches('`')).collect()
        };
        assert_eq!(std.labels, labels.as_slice(), "{name}");
    }
}

#[test]
fn spec_limits_match_constants() {
    let has = |s: String| assert!(SPEC.contains(&s), "METRICS.md 應包含：{s}");
    has(format!("各 0～{MAX_TOP_PROCESSES} 項（`MAX_TOP_PROCESSES`"));
    has(format!(
        "1～{MAX_PROCESS_NAME_BYTES} bytes，字元限制同 `hostname`（Pulse 以 `sanitize_display_text`"
    ));
    has(format!("選用，1～{MAX_PROCESS_COMMAND_BYTES} bytes"));
    has(format!(
        "陣列，最多 {MAX_INVENTORY_DISKS} 項，每項 `mountpoint`"
    ));
    has(format!(
        "陣列，最多 {MAX_INVENTORY_INTERFACES} 項，每項 `name`"
    ));
    has(format!(
        "最多 {MAX_INTERFACE_ADDRESSES} 個，不含 IPv6 的 zone"
    ));
    has(format!("字串欄位一律 1～{MAX_INVENTORY_TEXT_BYTES} bytes"));
    has(format!(
        "間隔不到 {} 秒（`INVENTORY_MIN_INTERVAL_MS`）",
        INVENTORY_MIN_INTERVAL_MS / 1000
    ));
    has(format!("全部 {} 個標準指標", STANDARD_METRICS.len()));
    has(format!("前 {LEGACY_STANDARD_METRICS} 個是 0.2.0 起的指標"));
    has(format!("每筆 report 最多 {MAX_METRICS} 個指標"));
    for feature in [FEATURE_INVENTORY, FEATURE_PROCESSES] {
        has(format!("| `{feature}` |"));
    }
}

/// 第 4.3 節的每類上限（Pulse 實作時以同一組數字為預設）
const BUDGET_UNLABELED: usize = 11;
const BUDGET_FILESYSTEMS: usize = 16;
const BUDGET_DISKS: usize = 8;
const BUDGET_INTERFACES: usize = 16;
const BUDGET_SENSORS: usize = 32;

fn max_metrics(mount_len: usize, device_len: usize, sensor_len: usize) -> Vec<Metric> {
    let label = |prefix: &str, i: usize, len: usize| {
        let s = format!("{prefix}{i}");
        format!("{s}{}", "x".repeat(len.saturating_sub(s.len())))
    };
    let unlabeled = STANDARD_METRICS.iter().filter(|m| m.labels.is_empty());
    let mut out: Vec<Metric> = unlabeled
        .map(|m| {
            if m.unit == "%" {
                Metric::new(m.name, serde_json::Number::from_f64(99.99).unwrap(), m.unit)
            } else {
                Metric::new(m.name, u64::MAX / 3, m.unit)
            }
        })
        .collect();
    assert_eq!(out.len(), BUDGET_UNLABELED);
    let with = |labels: &[&str], n: usize, len: usize, out: &mut Vec<Metric>| {
        for i in 0..n {
            for m in STANDARD_METRICS
                .iter()
                .filter(|m| m.labels == labels && m.name != METRIC_FILESYSTEM_USAGE)
            {
                let mut metric = Metric::new(m.name, u64::MAX / 3, m.unit);
                for key in m.labels {
                    let v = if *key == "fstype" {
                        "ext4".to_string()
                    } else {
                        label(key, i, len)
                    };
                    metric = metric.with_label(key, v);
                }
                out.push(metric);
            }
        }
    };
    with(
        &["mountpoint", "fstype"],
        BUDGET_FILESYSTEMS,
        mount_len,
        &mut out,
    );
    for i in 0..BUDGET_FILESYSTEMS {
        out.push(
            Metric::new(
                METRIC_FILESYSTEM_USAGE,
                serde_json::Number::from_f64(99.99).unwrap(),
                "%",
            )
            .with_label("mountpoint", label("mountpoint", i, mount_len))
            .with_label("fstype", "ext4"),
        );
    }
    let disk_metrics: Vec<_> = STANDARD_METRICS
        .iter()
        .filter(|m| m.name.starts_with("system.disk."))
        .collect();
    for i in 0..BUDGET_DISKS {
        for m in &disk_metrics {
            out.push(
                Metric::new(m.name, u64::MAX / 3, m.unit)
                    .with_label("device", label("disk", i, device_len)),
            );
        }
    }
    let net_metrics: Vec<_> = STANDARD_METRICS
        .iter()
        .filter(|m| m.name.starts_with("system.network."))
        .collect();
    for i in 0..BUDGET_INTERFACES {
        for m in &net_metrics {
            out.push(
                Metric::new(m.name, u64::MAX / 3, m.unit)
                    .with_label("device", label("if", i, device_len)),
            );
        }
    }
    for i in 0..BUDGET_SENSORS {
        out.push(
            Metric::new(
                METRIC_TEMPERATURE,
                serde_json::Number::from_f64(-12.25).unwrap(),
                "Cel",
            )
            .with_label("sensor", label("sensor", i, sensor_len)),
        );
    }
    out
}

#[test]
fn the_per_report_budget_fits_the_old_limits() {
    // 第 4.3 節：最壞情況 243 個 ≤ 256（0.7.0 的 Kairos 超過 256 會整筆拒絕）
    let metrics = max_metrics(16, 16, 32);
    assert_eq!(
        metrics.len(),
        BUDGET_UNLABELED
            + 4 * BUDGET_FILESYSTEMS
            + 5 * BUDGET_DISKS
            + 6 * BUDGET_INTERFACES
            + BUDGET_SENSORS
    );
    assert_eq!(metrics.len(), 243);
    assert!(metrics.len() <= MAX_METRICS);
    assert_eq!(validate_metrics(&metrics), Ok(()));

    // label 值為一般長度（掛載點 64、裝置 32、感測器 48 bytes）、行程各 5 個不含命令列時，
    // 加上 durable 欄位仍在單行上限內，不需要截斷（第 4.4 節）
    let processes = ProcessList {
        top_cpu: (0..5)
            .map(|i| ProcessEntry {
                pid: 4_000_000 + i,
                name: "n".repeat(64),
                cpu_usage: 99.99,
                memory: u64::MAX / 3,
                user: None,
                command: None,
            })
            .collect(),
        top_memory: vec![],
    };
    let mut processes = processes;
    processes.top_memory = processes.top_cpu.clone();
    let report = ClientMsg::Report {
        seq: u64::MAX,
        ts: "2026-10-20T02:00:05.210Z".into(),
        body: ReportBody::metrics(max_metrics(64, 32, 48))
            .with_durable(DurableSeq {
                boot_id: "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e".into(),
                report_seq: u64::MAX,
            })
            .with_processes(processes),
    };
    let len = serde_json::to_string(&report).unwrap().len();
    assert!(len <= MAX_LINE_LENGTH, "{len}");
}

// ---- 相容性（METRICS.md 第 7 節） ----

/// 凍結的 0.7.0 型別（client 訊息的外層，沒有 `deny_unknown_fields`）。不要隨規格修改
mod frozen_v07 {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    pub struct Metric {
        pub name: String,
        pub value: serde_json::Number,
        pub unit: String,
        #[serde(default)]
        pub labels: std::collections::BTreeMap<String, String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    #[allow(dead_code)]
    pub enum ClientMsg {
        Hello {
            seq: u64,
        },
        Heartbeat {
            seq: u64,
        },
        Report {
            seq: u64,
            ts: String,
            metrics: Vec<Metric>,
            boot_id: Option<String>,
            report_seq: Option<u64>,
        },
        RenewRequest {
            seq: u64,
        },
        Deregister {
            seq: u64,
        },
        #[serde(other)]
        Unknown,
    }
}

#[test]
fn old_kairos_treats_inventory_as_an_unknown_type() {
    for sub in ["inventory", "inventory（Windows）"] {
        let line = one(sub, 'P', "inventory");
        assert!(matches!(
            serde_json::from_str::<frozen_v07::ClientMsg>(line).unwrap(),
            frozen_v07::ClientMsg::Unknown
        ));
        // 新版在沒有協商時同樣不使用：Kairos 依 Session::inventory 決定（未協商時回 unknown_type）
        assert!(!Session::new(2, SERVER_FEATURES, &[FEATURE_INVENTORY]).inventory);
    }
}

#[test]
fn old_kairos_ignores_processes_and_accepts_the_new_metrics() {
    let line = one("協商與 report", 'P', "report");
    let frozen_v07::ClientMsg::Report {
        metrics,
        report_seq,
        ..
    } = serde_json::from_str::<frozen_v07::ClientMsg>(line).unwrap()
    else {
        panic!("0.7.0 應解析為 report");
    };
    assert_eq!(report_seq, Some(1201));
    // 0.7.0 的檢查：前 8 個標準指標檢查單位與範圍，其他只檢查格式。新的檢查是它的超集
    // （前 8 個的規則凍結），因此通過新的檢查就一定通過舊的；這裡另外確認新指標確實只受格式檢查
    for m in &metrics {
        assert!(
            is_valid_metric_name(&m.name) && is_valid_unit(&m.unit),
            "{m:?}"
        );
        assert!(m.labels.keys().all(|k| is_valid_label_key(k)), "{m:?}");
    }
    let legacy: Vec<&str> = STANDARD_METRICS[..LEGACY_STANDARD_METRICS]
        .iter()
        .map(|m| m.name)
        .collect();
    let new_ones = metrics
        .iter()
        .filter(|m| !legacy.contains(&m.name.as_str()))
        .count();
    assert!(new_ones > 0);
    assert!(metrics.len() <= MAX_METRICS);

    // 0.8.0 的 Pulse 連 0.7.0 的 Kairos：沒有協商 processes，送出的 report 不帶 processes
    let mut body = report_body(line);
    let session = Session::new(
        2,
        &[FEATURE_METRICS, FEATURE_DURABLE],
        &[
            FEATURE_METRICS,
            FEATURE_DURABLE,
            FEATURE_INVENTORY,
            FEATURE_PROCESSES,
        ],
    );
    assert!(!session.processes && !session.inventory && session.metrics);
    if !session.processes {
        body.processes = Ok(None);
    }
    let v = serde_json::to_value(ClientMsg::Report {
        seq: 3,
        ts: "t".into(),
        body,
    })
    .unwrap();
    assert!(v.get("processes").is_none());
    assert!(v.get("metrics").is_some() && v.get("report_seq").is_some());
}

#[test]
fn processes_field_is_parsed_separately() {
    // 型別錯誤的 processes 不讓整則 report 失敗：沒有協商時忽略，協商了時由 process_list 回 bad_request
    let line = r#"{"type":"report","seq":1,"ts":"t","metrics":[{"name":"a.b","value":1,"unit":"1"}],"processes":{"top_cpu":{}}}"#;
    let body = report_body(line);
    assert!(body.metric_list().is_ok());
    assert!(
        body.process_list()
            .unwrap_err()
            .starts_with("invalid message:")
    );
    // null 與省略相同
    let body = report_body(
        r#"{"type":"report","seq":1,"ts":"t","metrics":[{"name":"a.b","value":1,"unit":"1"}],"processes":null}"#,
    );
    assert_eq!(body.process_list(), Ok(None));
    // 解析失敗的 processes 不能序列化
    let msg = ClientMsg::Report {
        seq: 1,
        ts: "t".into(),
        body: ReportBody {
            processes: Err("x".into()),
            ..ReportBody::metrics(vec![Metric::new("a.b", 1, "1")])
        },
    };
    assert!(serde_json::to_string(&msg).is_err());
    // v1 格式的 report 也可以帶（只有協商了 processes 才用，而那需要 metrics，所以實際上不會發生）
    let v = serde_json::to_value(ClientMsg::Report {
        seq: 1,
        ts: "t".into(),
        body: ReportBody::legacy(1.0, 2, 1).with_processes(ProcessList::default()),
    })
    .unwrap();
    assert_eq!(v["processes"], json!({"top_cpu": [], "top_memory": []}));
}

#[test]
fn inventory_message_errors() {
    // 型別錯誤 → 解析失敗（伺服器回 bad_request，或未協商時 unknown_type）；ts 的限制同其他訊息
    for bad in [
        r#"{"type":"inventory","seq":2,"ts":"t"}"#,
        r#"{"type":"inventory","seq":2,"ts":"t","inventory":"x"}"#,
        r#"{"type":"inventory","seq":2,"inventory":{}}"#,
        r#"{"type":"inventory","seq":2,"ts":"t","inventory":{"memory":{"total":-1}}}"#,
    ] {
        assert!(parse_client_msg(bad).is_err(), "{bad}");
        assert_eq!(message_type(bad).as_deref(), Some("inventory"));
    }
    let long_ts = format!(
        r#"{{"type":"inventory","seq":2,"ts":"{}","inventory":{{}}}}"#,
        "t".repeat(MAX_TS_BYTES + 1)
    );
    let msg = parse_client_msg(&long_ts).unwrap();
    assert!(validate_client_msg(&msg).is_err());
    // 空的 inventory 合法（全部取不到）
    let msg = parse_client_msg(r#"{"type":"inventory","seq":2,"ts":"t","inventory":{}}"#).unwrap();
    assert_eq!(validate_client_msg(&msg), Ok(()));
    let ClientMsg::Inventory(inv) = msg else {
        panic!()
    };
    assert_eq!(validate_inventory(&inv.inventory), Ok(()));
}
