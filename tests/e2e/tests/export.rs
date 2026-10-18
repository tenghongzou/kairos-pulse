//! 端對端：指標轉出（`kairos/EXPORT.md`；商業審計 F7）。實際的 Kairos（`[export.prometheus]`、
//! `[export.otlp]` 啟用）與 Pulse：
//! - 經由連線保存的 report 出現在 `GET /api/v1/export/prometheus`（唯讀 token），Pulse 結束後
//!   `kairos_host_up` 變為 0、不再輸出該主機的指標；
//! - OTLP/HTTP 推送到本測試的假 collector（純 HTTP），帶 `headers_file` 的標頭與 semconv 名稱的 protobuf；
//! - 關機時佇列中剩下的資料再送一次（日誌沒有錯誤）。

use kp_e2e::{GOOD, Kairos, PULSE_REGISTERED, Pulse, TestDir, WAIT, api_raw, poll_until, test_pki};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 收到的請求：(標頭, 本文)
type Captured = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// 假的 OTLP collector：每個連線讀一個請求、回 200
fn fake_collector() -> (u16, Captured) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let got: Captured = Arc::default();
    let g = got.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut s) = sock else { break };
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let head_end = loop {
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(p);
                }
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            let mut body = buf[head_end + 4..].to_vec();
            while body.len() < len {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => body.extend_from_slice(&chunk[..n]),
                }
            }
            g.lock().unwrap().push((head, body));
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    (port, got)
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// `GET /api/v1/export/prometheus`：(狀態碼, 本文)
fn scrape(port: u16, token: &str) -> (u16, String) {
    let (status, _, body) = api_raw(
        port,
        &format!(
            "GET /api/v1/export/prometheus HTTP/1.0\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\r\n"
        ),
    );
    (status, body)
}

#[test]
fn prometheus_exporter_and_otlp_push_with_real_binaries() {
    let root = TestDir::new("export");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let (otlp_port, got) = fake_collector();
    std::fs::write(
        kairos.dir.join("otlp-headers"),
        "Authorization: Bearer e2e-otlp-token\n",
    )
    .unwrap();
    let mut cfg = std::fs::read_to_string(kairos.dir.join("kairos.toml")).unwrap();
    cfg.push_str(&format!(
        r#"
[export.prometheus]
enabled = true
stale_after_secs = 60

[export.otlp]
enabled = true
endpoint = "http://127.0.0.1:{otlp_port}/v1/metrics"
headers_file = "otlp-headers"
interval_secs = 1
resource_attributes = ["deployment.environment=e2e"]
"#
    ));
    std::fs::write(kairos.dir.join("kairos.toml"), cfg).unwrap();
    let mut k = kairos.start();
    k.wait_for("Prometheus exporter 已啟用", WAIT);
    let token = kairos.create_token("prometheus");

    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);

    // Prometheus：Pulse 每秒一筆 report → 出現在輸出中
    let series = format!("system_cpu_usage_percent{{agent_id=\"{}\"", GOOD.agent_id);
    let mut last = String::new();
    assert!(
        poll_until(WAIT, || {
            let (status, body) = scrape(kairos.console_port, &token);
            last = body;
            status == 200 && last.contains(&series)
        }),
        "Prometheus 輸出應有 Pulse 的指標\n{last}\n{}",
        k.dump()
    );
    assert!(last.contains(&format!("kairos_host_up{{agent_id=\"{}\"", GOOD.agent_id)));
    assert!(
        last.contains("# TYPE system_memory_total_bytes gauge"),
        "{last}"
    );
    assert!(
        last.contains("kairos_exporter_hosts{state=\"fresh\"} 1"),
        "{last}"
    );

    // OTLP：collector 收到帶標頭、含這台主機與 semconv 名稱的 protobuf
    assert!(
        poll_until(WAIT, || {
            got.lock().unwrap().iter().any(|(h, b)| {
                h.starts_with("post /v1/metrics")
                    && h.contains("authorization: bearer e2e-otlp-token")
                    && h.contains("content-type: application/x-protobuf")
                    && contains(b, GOOD.agent_id)
                    && contains(b, "system.cpu.utilization")
                    && contains(b, "deployment.environment")
            })
        }),
        "collector 應收到 OTLP 請求（{} 個）\n{}",
        got.lock().unwrap().len(),
        k.dump()
    );

    // Pulse 結束 → 離線：kairos_host_up 0、不再輸出指標
    p.interrupt();
    p.wait_exit(WAIT);
    assert!(
        poll_until(WAIT, || {
            let (_, body) = scrape(kairos.console_port, &token);
            last = body;
            last.contains(&format!("kairos_host_up{{agent_id=\"{}\"", GOOD.agent_id))
                && last
                    .lines()
                    .any(|l| l.starts_with("kairos_host_up{") && l.ends_with(" 0"))
                && !last.contains(&series)
        }),
        "離線後應只有 kairos_host_up 0\n{last}"
    );

    // 自身的 /metrics（loopback）有推送的計數
    let (status, _, metrics) = api_raw(
        kairos.console_port,
        "GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(status, 200);
    assert!(
        metrics.contains("kairos_export_otlp_requests_total{result=\"ok\"}"),
        "{metrics}"
    );
    assert!(
        metrics.contains("kairos_export_prometheus_enabled 1"),
        "{metrics}"
    );

    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
    assert_eq!(k.count("KRS-E431"), 0, "不應有 OTLP 的錯誤\n{}", k.dump());
}
