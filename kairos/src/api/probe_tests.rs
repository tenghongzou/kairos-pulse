//! `/healthz`、`/readyz`、`/metrics`（`crate::probe`）的端對端測試：經由主控台的 HTTPS 監聽，
//! 以及 `observability.listen` 的純 HTTP 監聽。

use super::tests::{Fixture, fixture, request, temp_dir};
use crate::probe;
use crate::stats::WriteOutcome;
use crate::store::tests::pulse_like;
use chrono::Utc;
use kp_obs::prom;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn obs(f: &Fixture) -> Arc<probe::ObsContext> {
    f.console.ctx.obs.clone().expect("fixture 應有探針的狀態")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_and_readyz_need_no_token_and_reveal_nothing() {
    let f = fixture("probe-health", |_| {}).await;
    let r = request(f.addr(), "GET", "/healthz", None, &[]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json(), serde_json::json!({"status": "ok"}));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let r = request(f.addr(), "HEAD", "/healthz", None, &[]).await;
    assert_eq!(r.status, 200);
    assert!(r.body.is_empty());
    let r = request(f.addr(), "POST", "/healthz", None, &[]).await;
    assert_eq!(r.status, 405);

    let r = request(f.addr(), "GET", "/readyz", None, &[]).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = r.json();
    assert_eq!(v["status"], "ready");
    assert_eq!(v["checks"]["database"], "ok");
    assert_eq!(v["checks"]["storage"], "ok");
    assert_eq!(v["checks"]["agent_listener"], "ok");
    assert_eq!(v["checks"]["enroll"], "disabled");
    // 不洩漏版本、路徑、位址
    let body = String::from_utf8_lossy(&r.body);
    for leak in [env!("CARGO_PKG_VERSION"), "/", "127.0.0.1", "kairos.db"] {
        assert!(!body.contains(leak), "{leak} 不應出現在 /readyz：{body}");
    }
    // 只負責 8444 的負載平衡器：註冊端點沒有啟用時不就緒
    let r = request(f.addr(), "GET", "/readyz?scope=enroll", None, &[]).await;
    assert_eq!(r.status, 503);

    // 儲存無法使用：最近 1 分鐘只有寫入失敗
    f.sink.stats().record(WriteOutcome::Unavailable);
    let r = request(f.addr(), "GET", "/readyz", None, &[]).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.json()["checks"]["storage"], "unavailable");
    f.sink.stats().record(WriteOutcome::Written);
    let r = request(f.addr(), "GET", "/readyz", None, &[]).await;
    assert_eq!(r.status, 200);

    // agent 連線埠還沒開始 accept
    obs(&f).agent_listening.store(false, Ordering::Relaxed);
    let r = request(f.addr(), "GET", "/readyz", None, &[]).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.json()["checks"]["agent_listener"], "not_listening");
    obs(&f).agent_listening.store(true, Ordering::Relaxed);

    // 關機中
    f.shutdown.trigger();
    f.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_is_valid_prometheus_text_from_loopback() {
    let f = fixture("probe-metrics", |_| {}).await;
    // 幾筆寫入（ack 延遲）、一筆結果（計數）、一個 API 請求與一次認證失敗
    for seq in 1..=3 {
        f.sink
            .write(pulse_like(
                "6f1c0e4a-1b2c-4d3e-9f00-112233445566",
                seq,
                Utc::now(),
                1.0,
            ))
            .await
            .unwrap();
    }
    f.sink.stats().record(WriteOutcome::Written);
    let _ = request(f.addr(), "GET", "/api/v1/hosts", None, &[]).await;
    let _ = request(f.addr(), "GET", "/api/v1/me", Some(&f.token), &[]).await;

    let r = request(f.addr(), "GET", "/metrics", None, &[]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some(prom::CONTENT_TYPE));
    let text = String::from_utf8(r.body.clone()).unwrap();
    let p = prom::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    prom::lint(&p, "kairos_").unwrap();
    for family in [
        "kairos_build_info",
        "kairos_ready",
        "kairos_agent_connections",
        "kairos_agent_disconnects_total",
        "kairos_registry_entries",
        "kairos_registry_evictions_total",
        "kairos_connections_rejected_total",
        "kairos_report_rate_limited_total",
        "kairos_reports_received_total",
        "kairos_reports_total",
        "kairos_report_write_queue_depth",
        "kairos_report_ack_duration_seconds",
        "kairos_storage_unavailable",
        "kairos_database_writable",
        "kairos_database_used_bytes",
        "kairos_retention_paused",
        "kairos_storage_capacity_deleting",
        "kairos_client_cert_rejected_total",
        "kairos_certificate_days_remaining",
        "kairos_api_requests_total",
        "kairos_api_auth_failures_total",
        "kairos_log_messages_total",
        "kairos_agent_auth_failures_total",
        "kairos_alerts_engine_enabled",
        "kairos_alerts_engine_up",
        "kairos_alerts_open",
        "kairos_alerts_fired_total",
        "kairos_alerts_resolved_total",
        "kairos_alert_channels",
        "kairos_alert_notification_sends_total",
        "kairos_alert_notifications_total",
    ] {
        assert!(p.families.contains_key(family), "缺少 {family}\n{text}");
    }
    assert_eq!(p.get("kairos_ready", &[]), Some(1.0));
    assert_eq!(p.get("kairos_database_writable", &[]), Some(1.0));
    assert!(
        p.get("kairos_report_ack_duration_seconds_count", &[])
            .unwrap()
            >= 3.0
    );
    assert!(
        p.get("kairos_reports_total", &[("result", "written")])
            .unwrap()
            >= 1.0
    );
    assert!(
        p.get(
            "kairos_certificate_days_remaining",
            &[("kind", "server_certificate"), ("index", "0")]
        )
        .is_some()
    );
    assert!(
        p.get(
            "kairos_api_requests_total",
            &[("route", "hosts"), ("code", "4xx")]
        )
        .unwrap()
            >= 1.0
    );
    assert!(
        p.get(
            "kairos_api_auth_failures_total",
            &[("reason", "unauthorized")]
        )
        .unwrap()
            >= 1.0
    );
    // 標籤基數：沒有任何樣本以 agent_id 或 IP 為標籤
    for s in &p.samples {
        for v in s.labels.values() {
            assert!(!v.contains("6f1c0e4a"), "{} 的標籤含 agent_id", s.name);
            assert!(!v.contains("127.0.0.1"), "{} 的標籤含 IP", s.name);
        }
    }
    // 非 loopback 來源不允許（沒有設定 token 時）
    let o = obs(&f);
    assert!(!o.metrics_allowed("10.0.0.5:5000".parse().unwrap(), None));
    assert!(o.metrics_allowed("[::1]:5000".parse().unwrap(), None));
    f.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_requires_the_bearer_token_when_configured() {
    let dir = temp_dir("probe-token");
    let token_file = dir.join("metrics.token");
    std::fs::write(&token_file, "s3cr3t-metrics-token-0123456789\n").unwrap();
    let path = token_file.to_string_lossy().into_owned();
    let f = fixture("probe-token", |c| c.observability.metrics_token_file = path).await;
    let r = request(f.addr(), "GET", "/metrics", None, &[]).await;
    assert_eq!(r.status, 401);
    assert!(r.header("www-authenticate").is_some());
    let r = request(
        f.addr(),
        "GET",
        "/metrics",
        Some("wrong-token-0123456789"),
        &[],
    )
    .await;
    assert_eq!(r.status, 401);
    let r = request(
        f.addr(),
        "GET",
        "/metrics",
        Some("s3cr3t-metrics-token-0123456789"),
        &[],
    )
    .await;
    assert_eq!(r.status, 200);
    let text = String::from_utf8(r.body).unwrap();
    assert!(!text.contains("s3cr3t"), "token 不可出現在輸出中");
    let p = prom::parse(&text).unwrap();
    assert!(
        p.get(
            "kairos_probe_requests_total",
            &[("endpoint", "metrics"), ("result", "denied")]
        )
        .unwrap()
            >= 2.0
    );
    // /healthz 不需要 token
    let r = request(f.addr(), "GET", "/healthz", None, &[]).await;
    assert_eq!(r.status, 200);
    // 設定了 token：經反向代理（帶轉送標頭）時以 bearer 驗證，照常允許
    let r = request(
        f.addr(),
        "GET",
        "/metrics",
        Some("s3cr3t-metrics-token-0123456789"),
        &[("X-Forwarded-For", "203.0.113.9")],
    )
    .await;
    assert_eq!(r.status, 200);
    f.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_without_token_rejects_requests_forwarded_by_a_proxy() {
    // 沒有 token：loopback 來源照常允許；帶任何一個轉送標頭（大小寫不拘）一律 403
    let f = fixture("probe-proxy", |_| {}).await;
    let r = request(f.addr(), "GET", "/metrics", None, &[]).await;
    assert_eq!(r.status, 200);
    for (name, value) in [
        ("Forwarded", "for=203.0.113.9;proto=https"),
        ("X-Forwarded-For", "203.0.113.9"),
        ("x-real-ip", "203.0.113.9"),
        ("X-Forwarded-Host", "metrics.example.com"),
        ("X-Forwarded-Proto", "https"),
        ("Via", "1.1 nginx"),
        // 值為空字串也算（只看標頭是否存在）
        ("X-Forwarded-For", ""),
    ] {
        let r = request(f.addr(), "GET", "/metrics", None, &[(name, value)]).await;
        assert_eq!(r.status, 403, "{name}: {value}");
        assert_eq!(r.json(), serde_json::json!({"status": "forbidden"}));
        // 帶了錯誤的 bearer 也一樣（沒有設定 token 時不看 Authorization）
        let r = request(
            f.addr(),
            "GET",
            "/metrics",
            Some("x".repeat(40).as_str()),
            &[(name, value)],
        )
        .await;
        assert_eq!(r.status, 403, "{name}");
    }
    // /healthz、/readyz 不受影響
    let r = request(
        f.addr(),
        "GET",
        "/healthz",
        None,
        &[("X-Forwarded-For", "203.0.113.9")],
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(probe::is_proxied(&{
        let mut h = hyper::HeaderMap::new();
        h.insert(
            "x-real-ip",
            hyper::header::HeaderValue::from_static("1.2.3.4"),
        );
        h
    }));
    assert!(!probe::is_proxied(&hyper::HeaderMap::new()));
    f.stop().await;
}

/// 以純 HTTP 取得一個路徑：(狀態碼, 本文)
async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), s.read_to_end(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, body.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_plain_http_listener() {
    let f = fixture("probe-listener", |_| {}).await;
    let server = probe::start_listener(obs(&f), "127.0.0.1:0").unwrap();
    let addr = server.local_addr;
    let (status, body) = http_get(addr, "/healthz").await;
    assert_eq!((status, body.trim()), (200, "{\"status\":\"ok\"}"));
    let (status, _) = http_get(addr, "/readyz").await;
    assert_eq!(status, 200);
    let (status, body) = http_get(addr, "/metrics").await;
    assert_eq!(status, 200);
    prom::parse(&body).unwrap();
    // 只有這三個端點；主控台 API 不在這個監聽上
    let (status, _) = http_get(addr, "/api/v1/hosts").await;
    assert_eq!(status, 404);
    f.shutdown.trigger();
    tokio::task::spawn_blocking(move || server.join())
        .await
        .unwrap();
    f.stop().await;
}
