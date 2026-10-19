//! 派送的測試：合併（另有 N 則）、緊急不排隊、每管道的速率、重試與退避、`Retry-After`、放棄、
//! 連續失敗 → 管道失敗 → 暫緩 → 試送成功 → 一則彙整、機密不出現在錯誤訊息；SMTP（STARTTLS、AUTH）。

use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use super::fake::{FakeHttp, FakeSmtp, MANIFEST};
use super::{DispatchConfig, Dispatcher, Pump, render, transport};
use crate::alerts::engine::NewDelivery;
use crate::alerts::model::{Channel, ChannelKind, Severity};
use crate::alerts::secrets::SecretBox;
use crate::alerts::store::Delivery;
use crate::enroll::db::EnrollDb;
use crate::store::prelude::*;

static NOW: AtomicI64 = AtomicI64::new(1_760_000_000_000);

fn clock() -> i64 {
    NOW.load(Ordering::SeqCst)
}

fn set(t: i64) {
    NOW.store(t, Ordering::SeqCst);
}

fn temp_db(tag: &str) -> (std::path::PathBuf, Arc<EnrollDb>) {
    let dir = std::env::temp_dir().join(format!(
        "kairos-dispatch-{tag}-{}-{}",
        std::process::id(),
        crate::registry::next_conn_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("kairos.db");
    let mut conn = rusqlite::Connection::open(&path).unwrap();
    crate::store::sqlite::migrate(&mut conn, crate::store::sqlite::MIGRATIONS).unwrap();
    drop(conn);
    (dir, EnrollDb::new(path))
}

fn webhook(id: &str, url: &str) -> Channel {
    Channel {
        channel_id: id.into(),
        name: "SIEM".into(),
        kind: ChannelKind::Webhook,
        enabled: true,
        config: json!({ "format": "kairos", "allow_http": true, "timeout_secs": 5 }),
        secrets: json!({ "url": url, "hmac_secret": "hk" }),
        state: "ok".into(),
        consecutive_failures: 0,
        failed_since: None,
        last_attempt_at: None,
        last_success_at: None,
        last_error_code: None,
        last_error: None,
        created_at: 0,
        created_by: "t".into(),
        updated_at: 0,
        updated_by: "t".into(),
    }
}

async fn setup(tag: &str, ch: &Channel) -> (std::path::PathBuf, Arc<Dispatcher>, Arc<EnrollDb>) {
    let (dir, db) = temp_db(tag);
    let sb = Arc::new(SecretBox::from_key(&[7u8; 32]).unwrap());
    let sealed = sb.seal(&ch.secrets).unwrap();
    let ch2 = ch.clone();
    db.write(move |conn| Ok(conn.save_channel(&ch2, Some(sealed))?))
        .await
        .unwrap();
    let d = Dispatcher::new(
        db.clone(),
        sb,
        DispatchConfig {
            offset: chrono::FixedOffset::east_opt(8 * 3600).unwrap(),
            console_url: "https://m.example".into(),
        },
    );
    (dir, d, db)
}

fn payload(title: &str, sev: Severity) -> Value {
    let mut p = render::test_payload(sev, "https://m.example", clock());
    p["kind"] = json!("firing");
    p["event"]["title"] = json!(title);
    p
}

/// 寫入並排入通知
async fn enqueue(d: &Dispatcher, db: &Arc<EnrollDb>, cid: &str, title: &str, urgent: bool) -> i64 {
    let nd = NewDelivery {
        event_id: Some(format!("E-{title}")),
        channel_id: cid.into(),
        kind: "firing",
        urgent,
        payload: payload(
            title,
            if urgent {
                Severity::Emergency
            } else {
                Severity::Critical
            },
        ),
        group: None,
        created_at: clock(),
    };
    let n2 = nd.clone();
    let ids = db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let ids = tx.insert_deliveries(&[n2])?;
            tx.commit()?;
            Ok(ids)
        })
        .await
        .unwrap();
    d.enqueue(vec![Delivery {
        id: ids[0],
        event_id: nd.event_id,
        channel_id: nd.channel_id,
        kind: "firing".into(),
        state: "pending".into(),
        urgent,
        payload: nd.payload,
        attempts: 0,
        created_at: nd.created_at,
        next_attempt_at: nd.created_at,
        sent_at: None,
        batch_id: None,
        response_code: None,
        error_code: None,
        error: None,
    }]);
    ids[0]
}

async fn delivery(db: &Arc<EnrollDb>, id: i64) -> Delivery {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, event_id, channel_id, kind, state, urgent, payload, attempts, created_at, next_attempt_at, sent_at, batch_id, response_code, error_code, error FROM alert_deliveries WHERE id = ?1",
        )?;
        Ok(stmt.query_row([id], |r| {
            Ok(Delivery {
                id: r.get(0)?,
                event_id: r.get(1)?,
                channel_id: r.get(2)?,
                kind: r.get(3)?,
                state: r.get(4)?,
                urgent: r.get::<_, i64>(5)? != 0,
                payload: Value::Null,
                attempts: r.get::<_, i64>(7)? as u32,
                created_at: r.get(8)?,
                next_attempt_at: r.get(9)?,
                sent_at: r.get(10)?,
                batch_id: r.get(11)?,
                response_code: r.get(12)?,
                error_code: r.get(13)?,
                error: r.get(14)?,
            })
        })?)
    })
    .await
    .unwrap()
}

async fn round_and_settle(p: &mut Pump, t: i64) {
    set(t);
    p.round(t).await;
    while p.settle(t).await {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merging_rate_limit_retries_and_channel_failure() {
    let http = FakeHttp::start().await;
    let url = format!("http://{}/hook?token=SECRETTOKEN", http.addr);
    let ch = webhook("c1", &url);
    let (dir, d, db) = setup("merge", &ch).await;
    let t0 = 1_760_000_000_000;
    set(t0);
    let mut p = Pump::new(d.clone(), clock).await;
    // 緊急的第一則單獨送出；其他 3 則合併成一則「另有 2 則」
    let urgent = enqueue(&d, &db, "c1", "urgent", true).await;
    let a = enqueue(&d, &db, "c1", "a", false).await;
    let b = enqueue(&d, &db, "c1", "b", false).await;
    let c = enqueue(&d, &db, "c1", "c", false).await;
    round_and_settle(&mut p, t0).await;
    assert_eq!(http.count(), 2);
    let reqs = http.requests.lock().unwrap().clone();
    let first: Value = serde_json::from_str(&reqs[0].body).unwrap();
    assert_eq!(first["schema"], "kairos.alert.v1");
    assert_eq!(first["severity"], "emergency");
    // HMAC-SHA256 簽章
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"hk");
    let tag = ring::hmac::sign(&key, reqs[0].body.as_bytes());
    let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        reqs[0].header("x-kairos-signature"),
        Some(format!("sha256={hex}"))
    );
    let merged: Value = serde_json::from_str(&reqs[1].body).unwrap();
    assert!(
        merged["title"].as_str().unwrap().contains("另有 2 則"),
        "{merged}"
    );
    for id in [urgent, a, b, c] {
        assert_eq!(delivery(&db, id).await.state, "sent");
    }
    assert_eq!(delivery(&db, b).await.batch_id, Some(a));
    // 每管道 30 秒最多 1 則
    let e = enqueue(&d, &db, "c1", "e", false).await;
    round_and_settle(&mut p, t0 + 10_000).await;
    assert_eq!(http.count(), 2);
    round_and_settle(&mut p, t0 + 31_000).await;
    assert_eq!(http.count(), 3);
    assert_eq!(delivery(&db, e).await.state, "sent");

    // 失敗：退避 30 秒、60 秒；連續 3 次 → 管道失敗（KRS-E5107 的條件）
    for _ in 0..3 {
        http.push(500, None);
    }
    let t1 = t0 + 100_000;
    set(t1);
    let f = enqueue(&d, &db, "c1", "f", false).await;
    round_and_settle(&mut p, t1).await;
    let x = delivery(&db, f).await;
    assert_eq!((x.state.as_str(), x.attempts), ("pending", 1));
    assert_eq!(x.next_attempt_at, t1 + 30_000);
    assert_eq!(x.error_code.as_deref(), Some("KRS-E5108"));
    assert_eq!(x.response_code, Some(500));
    assert!(
        !x.error.unwrap().contains("SECRETTOKEN"),
        "錯誤訊息不含網址中的 token"
    );
    round_and_settle(&mut p, t1 + 10_000).await; // 還沒到重試時間
    assert_eq!(delivery(&db, f).await.attempts, 1);
    round_and_settle(&mut p, t1 + 31_000).await;
    assert_eq!(delivery(&db, f).await.next_attempt_at, t1 + 31_000 + 60_000);
    round_and_settle(&mut p, t1 + 92_000).await;
    let status = d
        .channels()
        .into_iter()
        .find(|c| c.channel_id == "c1")
        .unwrap();
    assert_eq!(status.state, "failed");
    assert_eq!(status.consecutive_failures, 3);
    // 管道失敗期間：新的通知暫緩，不逐則嘗試
    let g = enqueue(&d, &db, "c1", "g", false).await;
    let before = http.count();
    round_and_settle(&mut p, t1 + 100_000).await;
    assert_eq!(delivery(&db, g).await.state, "held");
    assert_eq!(http.count(), before);
    // 2 分鐘後以最舊的一則試送：成功 → 管道恢復，其餘暫緩的改送一則彙整
    round_and_settle(&mut p, t1 + 92_000 + 121_000).await;
    let status = d
        .channels()
        .into_iter()
        .find(|c| c.channel_id == "c1")
        .unwrap();
    assert_eq!(status.state, "ok");
    assert_eq!(delivery(&db, f).await.state, "sent");
    assert_eq!(delivery(&db, g).await.state, "merged");
    round_and_settle(&mut p, t1 + 92_000 + 121_000 + 31_000).await;
    let last = http.last().unwrap();
    assert!(last.body.contains("通知管道已恢復"), "{}", last.body);

    // 429 依 Retry-After 退避
    http.push(429, Some(120));
    let t2 = t1 + 600_000;
    let h = enqueue(&d, &db, "c1", "h", false).await;
    round_and_settle(&mut p, t2).await;
    assert_eq!(delivery(&db, h).await.next_attempt_at, t2 + 120_000);
    // 超過 15 分鐘仍無法送出：失敗
    for _ in 0..3 {
        http.push(503, None);
    }
    round_and_settle(&mut p, t2 + 16 * 60_000).await;
    let x = delivery(&db, h).await;
    assert_eq!(x.state, "failed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_and_disabled_channels() {
    // 連線被拒：KRS-E5108；LINE 格式的連外失敗：KRS-E5104
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let ch = webhook("c2", &format!("http://{addr}/x"));
    let o = transport::send_webhook(&ch, &json!({})).await;
    assert!(!o.ok);
    assert_eq!(o.code, Some(transport::CODE_WEBHOOK));
    let mut line = ch.clone();
    line.config =
        json!({ "format": "line", "line_to": "C1", "allow_http": true, "timeout_secs": 2 });
    line.secrets = json!({ "url": format!("http://{addr}/push"), "line_token": "tok" });
    assert_eq!(
        transport::send_webhook(&line, &json!({})).await.code,
        Some(transport::CODE_EXTERNAL_NETWORK)
    );
    // 停用的管道：通知放棄
    let srv = FakeHttp::start().await;
    let mut ch = webhook("c3", &format!("http://{}/x", srv.addr));
    ch.enabled = false;
    let (dir, d, db) = setup("disabled", &ch).await;
    let mut p = Pump::new(d.clone(), clock).await;
    let id = enqueue(&d, &db, "c3", "x", false).await;
    round_and_settle(&mut p, clock()).await;
    assert_eq!(delivery(&db, id).await.state, "dropped");
    assert_eq!(srv.count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

fn smtp_channel(port: u16, password: &str) -> Channel {
    Channel {
        channel_id: "m1".into(),
        name: "值班 Email".into(),
        kind: ChannelKind::Email,
        enabled: true,
        config: json!({
            "host": "localhost",
            "port": port,
            "security": "starttls",
            "username": "kairos",
            "from": "kairos@example.com",
            "to": ["oncall@example.com", "boss@example.com"],
            "ca_file": format!("{MANIFEST}/../pulse/testdata/server-ca.pem"),
            "timeout_secs": 5,
        }),
        secrets: json!({ "password": password }),
        state: "ok".into(),
        consecutive_failures: 0,
        failed_since: None,
        last_attempt_at: None,
        last_success_at: None,
        last_error_code: None,
        last_error: None,
        created_at: 0,
        created_by: String::new(),
        updated_at: 0,
        updated_by: String::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smtp_with_starttls_and_auth() {
    let smtp = FakeSmtp::start("s3cret-pw").await;
    let ch = smtp_channel(smtp.addr.port(), "s3cret-pw");
    let cfg = DispatchConfig {
        offset: chrono::FixedOffset::east_opt(8 * 3600).unwrap(),
        console_url: String::new(),
    };
    let o = super::send_test(&ch, Severity::Emergency, &cfg, clock()).await;
    assert!(o.ok, "{o:?}");
    let mails = smtp.mails.lock().unwrap().clone();
    assert_eq!(mails.len(), 1);
    assert!(mails[0].contains("Subject: =?UTF-8?B?"));
    assert!(mails[0].contains("X-Priority: 1"), "緊急加優先權標頭");
    assert!(mails[0].contains("To: oncall@example.com, boss@example.com"));
    // AUTH PLAIN 在 STARTTLS 之後才送出（假伺服器在 TLS 之前不接受 AUTH）
    assert_eq!(smtp.auth.lock().unwrap().len(), 1);
    // 密碼錯誤：KRS-E5102，錯誤訊息不含密碼
    let bad = smtp_channel(smtp.addr.port(), "wrong-password");
    let o = super::send_test(&bad, Severity::Critical, &cfg, clock()).await;
    assert!(!o.ok);
    assert_eq!(o.code, Some(transport::CODE_SMTP_AUTH));
    assert!(!o.message.contains("wrong-password"));
    // 不信任伺服器憑證（沒有指定 ca_file）：KRS-E5103
    let mut untrusted = smtp_channel(smtp.addr.port(), "s3cret-pw");
    untrusted.config["ca_file"] = json!("");
    let o = super::send_test(&untrusted, Severity::Critical, &cfg, clock()).await;
    assert_eq!(o.code, Some(transport::CODE_TLS_UNTRUSTED), "{o:?}");
    // 連不上：KRS-E5101
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let o = super::send_test(&smtp_channel(port, "x"), Severity::Critical, &cfg, clock()).await;
    assert_eq!(o.code, Some(transport::CODE_SMTP_CONNECT));
}
