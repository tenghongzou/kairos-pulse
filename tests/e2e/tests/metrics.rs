//! 端對端：指標擴充（`crates/kairos-proto/METRICS.md` 第 16.3 節）。
//!
//! - 0.8 Pulse × 0.8 Kairos（SQLite）：新指標、行程排行與資產經實際的連線寫入資料庫，可由主控台 API 查到
//!   （速率、資產、行程快照）。
//! - 0.8 Pulse × 以 0.7 規則運作的 Kairos：以送出 0.7 `hello_ack`（只有 `metrics`、`durable`）的假伺服器
//!   接收，確認 Pulse 不送 `processes` 與 `inventory`，新指標以 0.7 的檢查規則（名稱與單位格式、數量、
//!   單行上限）可以通過。
//! - 0.7 Pulse 的訊息 × 0.8 Kairos：以 [`RawClient`] 送 0.7 格式（8 個指標）的 report，主控台的推算使用率、
//!   網路速率與資產的空狀態。

use kp_e2e::{
    GOOD, Kairos, PULSE_REGISTERED, Pulse, REVOKED, RawClient, TestDir, WAIT, poll_until,
    pulse_testdata, test_pki,
};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 查詢字串的值做百分比編碼（RFC 3986 的 unreserved 以外都編碼）。網路介面名稱可能有空白與括號，
/// 例如 Windows 的「Ethernet 3」「vEthernet (nat)」
fn query_value(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Unix 毫秒 → `YYYY-MM-DDTHH:MM:SS.mmmZ`（同 `durable.rs`）
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

/// 行程排行每 2 秒一次（預設 60 秒）
fn with_fast_processes(p: &Pulse) {
    let path = p.dir.join("pulse.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n[processes]\ninterval = 2\n");
    std::fs::write(&path, text).unwrap();
}

#[test]
fn pulse_08_and_kairos_08_store_metrics_processes_and_inventory() {
    let root = TestDir::new("metrics-08");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let mut k = kairos.start();
    k.wait_for("主控台 API 已啟動", WAIT);
    let api = kairos.api(&kairos.create_token("e2e"));
    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);
    with_fast_processes(&pulse);
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    let id = GOOD.agent_id;

    // 資產：hello_ack 之後立即送出，寫入資料庫並由 API 查到
    let mut inv = Value::Null;
    assert!(
        poll_until(WAIT, || {
            let (status, v) = api.get(&format!("/api/v1/hosts/{id}/inventory"));
            inv = v["data"].clone();
            status == 200 && inv["source"] == "inventory"
        }),
        "應收到資產：{inv}\n{}\n{}",
        k.dump(),
        p.dump()
    );
    assert_eq!(inv["supported"], true, "{inv}");
    let os = &inv["inventory"]["os"];
    assert_eq!(
        os["family"],
        if cfg!(windows) { "windows" } else { "linux" }
    );
    assert!(os["long_version"].is_string(), "{inv}");
    assert!(
        inv["inventory"]["cpu"]["logical_cores"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "{inv}"
    );
    assert!(inv["inventory"]["memory"]["total"].as_u64().unwrap_or(0) > 0);
    assert!(inv["digest"].as_str().unwrap().starts_with("sha256:"));
    assert!(inv["boot_time"].is_string(), "{inv}");
    // 第一次收到不算變更
    let changes = api.data(&format!("/api/v1/hosts/{id}/inventory/changes"));
    assert_eq!(changes["items"], json!([]), "{changes}");

    // 行程排行：每 2 秒附在 report 上
    let mut procs = Value::Null;
    assert!(
        poll_until(WAIT, || {
            let (status, v) = api.get(&format!("/api/v1/hosts/{id}/processes"));
            procs = v["data"].clone();
            status == 200 && procs["sampled_at"].is_string()
        }),
        "應有行程快照：{procs}\n{}",
        p.dump()
    );
    assert_eq!(procs["supported"], true);
    let top = procs["top_memory"].as_array().unwrap();
    assert!(!top.is_empty(), "{procs}");
    assert!(top[0]["name"].is_string() && top[0]["memory"].as_u64().unwrap() > 0);
    // 預設不送命令列
    assert!(top.iter().all(|t| t.get("command").is_none()), "{procs}");

    // 新指標：清單有 uptime、行程數、Pulse 送的使用率（不是推算）、網路計數器
    let metrics = api.data(&format!("/api/v1/hosts/{id}/metrics"));
    let list = metrics["metrics"].as_array().unwrap().clone();
    let has = |name: &str| list.iter().any(|m| m["name"] == name);
    for name in [
        "system.uptime",
        "system.process.count",
        "system.memory.available",
    ] {
        assert!(has(name), "{name}：{metrics}");
    }
    if let Some(usage) = list.iter().find(|m| m["name"] == "system.filesystem.usage") {
        assert!(usage.get("derived").is_none(), "{usage}");
    }
    let net = list
        .iter()
        .find(|m| m["name"] == "system.network.received")
        .cloned();
    // 網路：預設回傳每秒速率（需要兩筆 report）
    if let Some(net) = net {
        assert_eq!(net["kind"], "counter");
        let device = query_value(net["labels"]["device"].as_str().unwrap());
        assert!(
            poll_until(WAIT, || {
                let (status, v) = api.get(&format!(
                    "/api/v1/hosts/{id}/metrics/system.network.received?range=1h&label.device={device}"
                ));
                status == 200
                    && v["data"]["series"][0]["unit"] == "By/s"
                    && v["data"]["series"][0]["points"]
                        .as_array()
                        .is_some_and(|p| !p.is_empty())
            }),
            "網路應有速率的點\n{}",
            k.dump()
        );
    }
    // 主機清單與詳情：資產摘要、開機時間、目前數值
    kp_e2e::poll_until(WAIT, || {
        api.data("/api/v1/hosts")[0]["inventory"].is_object()
    });
    let host = &api.data("/api/v1/hosts")[0];
    assert!(
        host["inventory"]["cpu_logical_cores"].as_u64().unwrap() > 0,
        "{host}"
    );
    assert!(host["inventory"].get("serial").is_none());
    assert!(host["uptime_secs"].as_i64().unwrap() > 0, "{host}");
    let detail = api.data(&format!("/api/v1/hosts/{id}"));
    assert_eq!(detail["features"]["inventory"], true, "{detail}");
    assert!(detail["last_report"]["process_count"].as_u64().unwrap() > 0);
    assert!(
        !detail["last_report"]["filesystems"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // 沒有協定錯誤
    assert_eq!(k.count("unknown_type"), 0, "{}", k.dump());
    assert_eq!(k.count("KRS-W2108"), 0, "{}", k.dump());
    p.stop();
    k.stop();
}

/// 送出 0.7 的 `hello_ack`、對 heartbeat 與 report 回 `ack` 的假伺服器；回傳收到的訊息
fn old_kairos(listener: TcpListener, run_for: Duration) -> Vec<String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(pulse_testdata("server.pem"))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    let key = PrivateKeyDer::from_pem_file(pulse_testdata("server.key")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    let (tcp, _) = listener.accept().unwrap();
    tcp.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let conn = rustls::ServerConnection::new(Arc::new(config)).unwrap();
    let mut stream = BufReader::new(rustls::StreamOwned::new(conn, tcp));
    let deadline = Instant::now() + run_for;
    let mut got = Vec::new();
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        match stream.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => break,
        }
        let msg: Value = serde_json::from_str(line.trim_end()).unwrap();
        let reply = match msg["type"].as_str() {
            Some("hello") => json!({
                "type": "hello_ack", "seq": msg["seq"], "proto": 2, "offline_after_ms": 5000,
                "features": ["metrics", "durable"], "server_time": "2026-10-15T00:00:00.000Z"
            }),
            Some("heartbeat" | "report") => json!({"type": "ack", "seq": msg["seq"]}),
            _ => {
                json!({"type": "error", "seq": null, "code": "unknown_type", "message": "unknown message type", "fatal": false})
            }
        };
        got.push(line.trim_end().to_string());
        let out = stream.get_mut();
        writeln!(out, "{reply}").unwrap();
        out.flush().unwrap();
    }
    got
}

#[test]
fn pulse_08_with_a_kairos_07_sends_no_processes_or_inventory() {
    let root = TestDir::new("metrics-07k");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || old_kairos(listener, Duration::from_secs(8)));
    let pulse = Pulse::new(&root, "pulse", port, GOOD);
    with_fast_processes(&pulse);
    let mut p = pulse.start(&[]);
    let got = server.join().unwrap();
    p.stop();
    let msgs: Vec<Value> = got
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let hello = &msgs[0];
    assert_eq!(hello["type"], "hello", "{hello}");
    // Pulse 宣告了新的 feature，但 0.7 的 Kairos 沒有宣告：不送 inventory、不帶 processes
    let features = hello["features"].as_array().unwrap();
    assert!(features.contains(&json!("inventory")) && features.contains(&json!("processes")));
    assert!(
        msgs.iter().all(|m| m["type"] != "inventory"),
        "{got:?}\n{}",
        p.dump()
    );
    let reports: Vec<&Value> = msgs.iter().filter(|m| m["type"] == "report").collect();
    assert!(reports.len() >= 3, "{got:?}\n{}", p.dump());
    for (line, r) in got.iter().zip(&msgs).filter(|(_, m)| m["type"] == "report") {
        assert!(r.get("processes").is_none(), "{r}");
        // 0.7 的檢查規則：每筆最多 256 個指標、單行 65536 bytes、名稱與單位的格式
        let metrics = r["metrics"].as_array().unwrap();
        assert!(metrics.len() <= 256 && line.len() <= 65536);
        for m in metrics {
            let name = m["name"].as_str().unwrap();
            assert!(kairos_proto::is_valid_metric_name(name), "{name}");
            assert!(
                kairos_proto::is_valid_unit(m["unit"].as_str().unwrap()),
                "{m}"
            );
        }
    }
    // 新的標準指標照送（0.7 的 Kairos 原樣保存）
    let names: Vec<&str> = reports.last().unwrap()["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"system.uptime"), "{names:?}");
    assert!(names.contains(&"system.process.count"), "{names:?}");
}

#[test]
fn pulse_07_messages_get_estimates_and_empty_inventory_on_kairos_08() {
    let root = TestDir::new("metrics-07p");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let mut k = kairos.start();
    k.wait_for("主控台 API 已啟動", WAIT);
    let api = kairos.api(&kairos.create_token("e2e"));
    // 0.7 的 Pulse：v2、metrics＋durable、hello 有 os；report 只有 0.2.0 的 8 個指標
    let mut c = RawClient::connect(kairos.port, REVOKED);
    let id = REVOKED.agent_id;
    c.send(&json!({
        "type": "hello", "seq": 1, "proto": 1, "proto_min": 1, "proto_max": 2,
        "features": ["metrics", "durable"], "agent_id": id, "hostname": "pulse-07",
        "agent_version": "0.7.0", "heartbeat_interval_ms": 1000, "ts": "2026-10-15T00:00:00.000Z",
        "os": {"name": "Ubuntu", "version": "24.04", "kernel": "6.8.0", "arch": "x86_64"}
    }));
    let ack = c.recv();
    assert_eq!(ack["type"], "hello_ack", "{ack}");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    for i in 0..3_i64 {
        let ts = iso_ms(now_ms - (3 - i) * 5000);
        c.send(&json!({
            "type": "report", "seq": 2 + i, "ts": ts,
            "boot_id": "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e", "report_seq": 1 + i,
            "metrics": [
                {"name": "system.cpu.usage", "value": 5.0, "unit": "%"},
                {"name": "system.memory.total", "value": 1000, "unit": "By"},
                {"name": "system.memory.used", "value": 500, "unit": "By"},
                {"name": "system.filesystem.total", "value": 1000, "unit": "By", "labels": {"mountpoint": "/", "fstype": "ext4"}},
                {"name": "system.filesystem.available", "value": 400, "unit": "By", "labels": {"mountpoint": "/", "fstype": "ext4"}},
                {"name": "system.network.received", "value": 1000 + i * 5000, "unit": "By", "labels": {"device": "eth0"}},
                {"name": "system.network.transmitted", "value": 1000, "unit": "By", "labels": {"device": "eth0"}},
                {"name": "system.temperature", "value": 40.0, "unit": "Cel", "labels": {"sensor": "cpu"}}
            ]
        }));
        assert_eq!(c.recv(), json!({"type": "ack", "seq": 2 + i}));
    }
    // 推算的使用率（60%）與網路速率（1000 B/s）
    let v = api.data(&format!(
        "/api/v1/hosts/{id}/metrics/system.filesystem.usage?range=1h"
    ));
    let s = &v["series"][0];
    assert_eq!(s["derived"], "estimated", "{v}");
    assert_eq!(s["points"][0]["avg"], 60.0, "{v}");
    let v = api.data(&format!(
        "/api/v1/hosts/{id}/metrics/system.network.received?range=1h"
    ));
    let points = v["series"][0]["points"].as_array().unwrap();
    assert_eq!(points.len(), 2, "{v}");
    assert_eq!(points[0]["avg"], 1000.0, "{v}");
    // 主機清單：磁碟使用率標示推算；資產為 hello 的 os、不支援
    poll_until(WAIT, || {
        api.data("/api/v1/hosts")[0]["last_report"]["disk"].is_object()
    });
    let host = &api.data("/api/v1/hosts")[0];
    assert_eq!(host["last_report"]["disk"]["estimated"], true, "{host}");
    assert_eq!(host["inventory"], Value::Null);
    let inv = api.data(&format!("/api/v1/hosts/{id}/inventory"));
    assert_eq!(inv["source"], "hello", "{inv}");
    assert_eq!(inv["supported"], false);
    assert_eq!(inv["inventory"]["os"]["name"], "Ubuntu");
    let procs = api.data(&format!("/api/v1/hosts/{id}/processes"));
    assert_eq!(procs["supported"], false);
    assert_eq!(procs["sampled_at"], Value::Null);
    c.close();
    k.stop();
}
