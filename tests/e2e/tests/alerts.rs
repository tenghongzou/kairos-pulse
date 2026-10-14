//! 端對端（0.6 的三條線整合）：實際的 Kairos 與 Pulse，日誌為 JSON（`log.format = "json"`）。
//!
//! 首位管理員（`kairos users create --role admin`）→ 登入（必須先變更密碼）→ 建立維運者與唯讀 →
//! 管理員新增 Webhook 管道（測試內的本機 HTTP 接收器）並綁到主機離線規則 → Pulse 連上後停止 →
//! 離線告警以 Webhook 送達 → 唯讀確認被拒（403）、沒有 CSRF token 被拒 → 維運者以工作階段確認 →
//! `/metrics` 有告警與主機的計數 → 稽核雜湊鏈以 `kairos audit verify` 驗證通過，告警操作在鏈中。

use kp_e2e::{
    GOOD, Kairos, PULSE_REGISTERED, Pulse, TestDir, WAIT, api_raw, run_to_exit, test_pki,
};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 收到的 Webhook 請求：(標頭, 本文)
type Received = Arc<Mutex<Vec<(String, String)>>>;

/// 本機的 HTTP 接收器（一個請求一條連線，回 200）
fn webhook_receiver() -> (u16, Received) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let got: Received = Arc::default();
    let sink = got.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // 讀到標頭結束，再依 Content-Length 讀本文
            let (head, body) = loop {
                let n = match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break (String::new(), String::new()),
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if rest.len() >= len {
                        break (head.to_string(), rest[..len].to_string());
                    }
                }
            };
            let _ =
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            if !head.is_empty() {
                sink.lock().unwrap().push((head, body));
            }
        }
    });
    (port, got)
}

/// 主控台的身分
#[derive(Clone, Copy)]
enum Who<'a> {
    None,
    /// 工作階段的 cookie 與 CSRF token
    Session(&'a str, &'a str),
    /// 只有 cookie（沒有 CSRF token）
    CookieOnly(&'a str),
}

fn call(
    port: u16,
    method: &str,
    path: &str,
    who: Who<'_>,
    body: Option<&Value>,
) -> (u16, String, Value) {
    let auth = match who {
        Who::None => String::new(),
        Who::Session(c, csrf) => format!("Cookie: {c}\r\nX-CSRF-Token: {csrf}\r\n"),
        Who::CookieOnly(c) => format!("Cookie: {c}\r\n"),
    };
    let body = body.map(Value::to_string).unwrap_or_default();
    let content = if method == "GET" {
        String::new()
    } else {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )
    };
    let (s, head, text) = api_raw(
        port,
        &format!("{method} {path} HTTP/1.0\r\nHost: localhost\r\n{auth}{content}\r\n{body}"),
    );
    (s, head, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// 登入，回傳 (cookie, csrf)；`must_change` 時以新密碼完成變更
fn login(port: u16, user: &str, password: &str, new_password: Option<&str>) -> (String, String) {
    let (s, head, v) = call(
        port,
        "POST",
        "/api/v1/auth/login",
        Who::None,
        Some(&json!({"username": user, "password": password})),
    );
    assert_eq!(s, 200, "{user}: {v}");
    let cookie = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.split(';').next().unwrap().trim().to_string())
        .expect("應有 Set-Cookie");
    let csrf = v["data"]["csrf_token"].as_str().unwrap().to_string();
    if let Some(np) = new_password {
        assert_eq!(v["data"]["must_change_password"], true, "{v}");
        let (s, _, v) = call(
            port,
            "POST",
            "/api/v1/me/password",
            Who::Session(&cookie, &csrf),
            Some(&json!({"current_password": password, "new_password": np})),
        );
        assert_eq!(s, 200, "{user}: {v}");
    }
    (cookie, csrf)
}

/// Prometheus 文字中一個樣本的值（`name{labels}` 完全相同的那一行）
fn sample(text: &str, series: &str) -> Option<f64> {
    text.lines().filter(|l| !l.starts_with('#')).find_map(|l| {
        l.strip_prefix(series)?
            .strip_prefix(' ')?
            .trim()
            .parse()
            .ok()
    })
}

/// 每一行都是 v1 的 JSON 日誌（`kp_obs::logfmt`），回傳所有的 `event`
fn json_log_events(what: &str, text: &str) -> Vec<String> {
    let mut events = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("{what} 的日誌不是 JSON（{e}）：{line}"));
        assert_eq!(v["v"], 1, "{line}");
        for k in [
            "ts", "level", "event", "msg", "product", "host", "run_id", "seq",
        ] {
            assert!(v.get(k).is_some(), "{what} 的日誌缺少 {k}：{line}");
        }
        events.push(v["event"].as_str().unwrap_or_default().to_string());
    }
    events
}

#[test]
fn operator_acknowledges_an_offline_alert_delivered_by_webhook_with_json_logs() {
    let root = TestDir::new("alerts");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let cfg = kairos.dir.join("kairos.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    let text = text.replace(
        "[log]\nlevel = \"debug\"\n",
        "[log]\nlevel = \"debug\"\nformat = \"json\"\n",
    );
    assert!(text.contains("format = \"json\""));
    std::fs::write(&cfg, format!("{text}\n[alerts]\nwarmup_secs = 60\n")).unwrap();
    let cli = |args: &[&str]| {
        let mut all = args.to_vec();
        all.extend(["--config", "kairos.toml"]);
        run_to_exit("kairos", &kairos.dir, &all, &[])
    };
    // 首位管理員
    let (code, p) = cli(&["users", "create", "admin", "--role", "admin"]);
    assert_eq!(code, 0, "{}", p.dump());
    let admin_pw = p.stdout().trim().to_string();

    let (hook_port, received) = webhook_receiver();
    let mut k = kairos.start();
    k.wait_for("主控台 API 已啟動", WAIT);
    let port = kairos.console_port;
    let (admin_c, admin_x) = login(
        port,
        "admin",
        &admin_pw,
        Some("correct horse battery staple"),
    );
    let admin = Who::Session(&admin_c, &admin_x);

    // 維運者與唯讀
    let mut accounts = Vec::new();
    for (name, role) in [("op1", "operator"), ("vi1", "viewer")] {
        let (s, _, v) = call(
            port,
            "POST",
            "/api/v1/users",
            admin,
            Some(&json!({"username": name, "roles": [role]})),
        );
        assert_eq!(s, 201, "{v}");
        let pw = v["data"]["initial_password"].as_str().unwrap().to_string();
        accounts.push(login(port, name, &pw, Some("purple elephant sings loudly")));
    }
    let (op_c, op_x) = accounts[0].clone();
    let (vi_c, vi_x) = accounts[1].clone();
    let op = Who::Session(&op_c, &op_x);
    let vi = Who::Session(&vi_c, &vi_x);

    // 管道：只有管理員能新增；維運者可以看清單（機密只有遮蔽值）
    let channel = json!({
        "name": "e2e-hook",
        "kind": "webhook",
        "config": {"format": "kairos", "allow_http": true},
        "secrets": {"url": format!("http://127.0.0.1:{hook_port}/hook?token=E2EHOOKSECRET"), "hmac_secret": "e2e-hmac"},
    });
    let (s, _, v) = call(port, "POST", "/api/v1/channels", op, Some(&channel));
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["error"]["details"]["permission"], "channels:write");
    let (s, _, v) = call(port, "POST", "/api/v1/channels", admin, Some(&channel));
    assert_eq!(s, 201, "{v}");
    let cid = v["data"]["channel_id"].as_str().unwrap().to_string();
    let (s, _, v) = call(port, "GET", "/api/v1/channels", op, None);
    assert_eq!(s, 200, "{v}");
    assert!(!v.to_string().contains("E2EHOOKSECRET"), "{v}");
    let (s, _, _) = call(port, "GET", "/api/v1/channels", vi, None);
    assert_eq!(s, 403);
    // 主機離線規則：寬限 0 秒（只有管理員能改系統規則的條件）、綁到管道
    let (s, _, v) = call(
        port,
        "PUT",
        "/api/v1/alert-rules/sys.host_offline",
        admin,
        Some(&json!({"channels": [cid], "params": {"grace_secs": 0}})),
    );
    assert_eq!(s, 200, "{v}");

    // Pulse（JSON 日誌）連上後停止 → 離線告警 → Webhook
    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);
    let pcfg = pulse.dir.join("pulse.toml");
    let ptext = std::fs::read_to_string(&pcfg).unwrap();
    std::fs::write(
        &pcfg,
        ptext.replace(
            "[log]\nlevel = \"trace\"\n",
            "[log]\nlevel = \"trace\"\nformat = \"json\"\n",
        ),
    )
    .unwrap();
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    // 告警引擎每 5 秒判定一次：先讓它看到主機在線上
    std::thread::sleep(Duration::from_secs(7));
    p.stop();
    let t0 = Instant::now();
    while received.lock().unwrap().is_empty() {
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "60 秒內沒有收到 Webhook\n{}",
            k.dump()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!(
        "[e2e] Webhook 在 Pulse 停止後 {:.1} 秒送達",
        t0.elapsed().as_secs_f64()
    );
    let (head, body) = received.lock().unwrap()[0].clone();
    let hook: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(hook["schema"], "kairos.alert.v1", "{hook}");
    assert_eq!(hook["kind"], "host_offline", "{hook}");
    assert!(
        head.to_ascii_lowercase()
            .contains("x-kairos-signature: sha256="),
        "{head}"
    );
    let event_id = hook["event_id"].as_str().unwrap().to_string();
    let ack = format!("/api/v1/alerts/{event_id}/ack");

    // 唯讀：看得到事件，確認被拒；沒有 CSRF token 的確認被拒
    let (s, _, v) = call(port, "GET", &format!("/api/v1/alerts/{event_id}"), vi, None);
    assert_eq!(s, 200, "{v}");
    let (s, _, v) = call(port, "POST", &ack, vi, Some(&json!({})));
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["error"]["details"]["permission"], "alerts:write", "{v}");
    let (s, _, v) = call(port, "POST", &ack, Who::CookieOnly(&op_c), Some(&json!({})));
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["error"]["code"], "csrf_failed", "{v}");
    // 維運者確認
    let (s, _, v) = call(port, "POST", &ack, op, Some(&json!({"assign_to_me": true})));
    assert_eq!(s, 200, "{v}");
    let (_, _, v) = call(port, "GET", &format!("/api/v1/alerts/{event_id}"), vi, None);
    assert_eq!(v["data"]["acked"], true, "{v}");

    // /metrics（loopback、不需 token）
    let (s, _, metrics) = api_raw(port, "GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n");
    assert_eq!(s, 200);
    for (series, min) in [
        ("kairos_alerts_engine_up", 1.0),
        ("kairos_alerts_fired_total{severity=\"critical\"}", 1.0),
        (
            "kairos_alert_notification_sends_total{kind=\"webhook\",result=\"ok\"}",
            1.0,
        ),
        (
            "kairos_alert_notifications_total{kind=\"webhook\",state=\"sent\"}",
            1.0,
        ),
        (
            "kairos_alerts_open{severity=\"critical\",state=\"acknowledged\"}",
            1.0,
        ),
        ("kairos_alert_channels{state=\"ok\"}", 1.0),
        ("kairos_agent_online_total", 1.0),
        (
            "kairos_agent_disconnects_total{reason=\"connection_closed\"}",
            1.0,
        ),
    ] {
        let v = sample(&metrics, series).unwrap_or_else(|| panic!("缺少 {series}\n{metrics}"));
        assert!(v >= min, "{series} = {v}\n{metrics}");
    }
    assert!(
        !metrics.contains(GOOD.agent_id),
        "/metrics 不以 agent_id 當標籤"
    );

    k.stop();
    // JSON 日誌：Kairos 與 Pulse 的輸出每一行都是 v1 的 JSON；告警與帳號的事件有固定名稱
    let kev = json_log_events("Kairos", &k.stdout());
    assert!(kev.iter().any(|e| e == "alerts.engine.started"), "{kev:?}");
    let pev = json_log_events("Pulse", &p.stdout());
    assert!(!pev.is_empty());
    let logs = std::fs::read_dir(kairos.dir.join("logs"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(logs.iter().any(|n| n.ends_with(".jsonl")), "{logs:?}");
    // 日誌與 Webhook 以外的地方沒有機密與密碼
    for secret in [
        "E2EHOOKSECRET",
        "e2e-hmac",
        "correct horse battery staple",
        admin_pw.as_str(),
    ] {
        assert!(!k.output().contains(secret), "Kairos 的日誌不應有 {secret}");
    }

    // 稽核雜湊鏈：完整；告警的操作與被拒的確認都在鏈中
    let (code, p) = cli(&["audit", "verify", "--json"]);
    assert_eq!(code, 0, "{}", p.dump());
    let v: Value = serde_json::from_str(p.stdout().trim()).unwrap();
    assert_eq!(v["data"]["ok"], true, "{v}");
    let (code, p) = cli(&["audit", "list", "--limit", "500", "--json"]);
    assert_eq!(code, 0, "{}", p.dump());
    let v: Value = serde_json::from_str(p.stdout().trim()).unwrap();
    let events = v["data"]["events"].as_array().unwrap();
    let has = |action: &str, actor: &str| {
        events
            .iter()
            .any(|e| e["action"] == action && e["actor"] == actor)
    };
    assert!(has("alert.ack", "user:op1"), "{v}");
    assert!(has("channel.create", "user:admin"), "{v}");
    assert!(has("alert_rule.update", "user:admin"), "{v}");
    assert!(has("access.denied", "user:vi1"), "{v}");
    assert!(!p.stdout().contains("E2EHOOKSECRET"));
}
