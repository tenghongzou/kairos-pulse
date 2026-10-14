//! 告警端點的整合測試：實際的 HTTPS 主控台＋告警服務（自己的執行緒、5 秒的判定週期）＋假的 Webhook。
//! 權限（集中標記、403 帶權限名稱）、管道機密的遮蔽與加密、主機離線 → 事件 → Webhook（簽章）→ SSE →
//! 確認、靜音的伺服器端上限與符合台數、規則的調整、稽核紀錄。

use serde_json::{Value, json};
use std::time::{Duration, Instant};

use super::tests::{Sse, call, fixture_alerts, hello};
use super::tokens::{self, TokenGrant};
use crate::alerts::notify::fake::FakeHttp;
use crate::rbac::Role;

const H: &str = "7e2f0a11-0000-4000-8000-0000000000a1";

/// 角色的 API token（0.6 的 RBAC；`f.token` 是 0.4／0.5 的 `read` 範圍，沒有告警的權限）
fn token(f: &super::tests::Fixture, role: Role) -> String {
    let conn = f.admin();
    tokens::create_with(
        &conn,
        role.as_str(),
        &TokenGrant::Role(role),
        None,
        Some("tester"),
        None,
        chrono::Utc::now().timestamp_millis(),
    )
    .unwrap()
    .token
}

async fn wait_for<F: Fn() -> bool>(what: &str, limit: Duration, cond: F) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < limit, "等待逾時：{what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_end_to_end_over_https() {
    let hook = FakeHttp::start().await;
    let f = fixture_alerts("alerts", |c| c.alerts.warmup_secs = 60).await;
    let addr = f.addr();
    let admin = token(&f, Role::Admin);
    let operate = token(&f, Role::Operator);
    let viewer = token(&f, Role::Viewer);
    let auditor = token(&f, Role::Auditor);
    let legacy = f.token.clone();

    // 0.5 的 token（範圍 read）：升級後權限凍結，沒有告警的權限（403，所需的最低角色為 viewer）
    let r = call(addr, "GET", "/api/v1/alerts", Some(&legacy), None).await;
    assert_eq!(r.status, 403);
    assert_eq!(r.json()["error"]["details"]["permission"], "alerts:read");
    // 唯讀看不到規則（S08 為「—」），稽核員可以
    let r = call(addr, "GET", "/api/v1/alert-rules", Some(&viewer), None).await;
    assert_eq!(r.status, 403);
    assert_eq!(
        r.json()["error"]["details"]["permission"],
        "alert-rules:read"
    );
    assert_eq!(
        call(addr, "GET", "/api/v1/alert-rules", Some(&auditor), None)
            .await
            .status,
        200
    );
    // 稽核員看不到通知管道、不能建立靜音
    assert_eq!(
        call(addr, "GET", "/api/v1/channels", Some(&auditor), None)
            .await
            .status,
        403
    );
    assert_eq!(
        call(
            addr,
            "POST",
            "/api/v1/silences",
            Some(&auditor),
            Some(&json!({"kind": "silence", "name": "x", "scope": {}, "duration_secs": 600, "reason": "x"}))
        )
        .await
        .status,
        403
    );
    let url = format!("http://{}/hook?token=SECRETTOKEN", hook.addr);
    let channel = json!({
        "name": "SIEM",
        "kind": "webhook",
        "config": { "format": "kairos", "allow_http": true },
        "secrets": { "url": url, "hmac_secret": "hk-secret" },
    });

    // 權限：唯讀 token 不能新增管道；403 帶需要的範圍與權限名稱
    let r = call(
        addr,
        "POST",
        "/api/v1/channels",
        Some(&viewer),
        Some(&channel),
    )
    .await;
    assert_eq!(r.status, 403);
    let v = r.json();
    assert_eq!(v["error"]["product_code"], "KRS-E6104");
    assert_eq!(v["error"]["details"]["permission"], "channels:write");
    assert_eq!(v["error"]["details"]["required"], "admin");
    // 維運者也不能新增管道，但可以看清單
    assert_eq!(
        call(
            addr,
            "POST",
            "/api/v1/channels",
            Some(&operate),
            Some(&channel)
        )
        .await
        .status,
        403
    );
    assert_eq!(
        call(addr, "GET", "/api/v1/channels", Some(&operate), None)
            .await
            .status,
        200
    );
    // 唯讀看不到管道（S11：稽核員與唯讀看不到此頁）
    assert_eq!(
        call(addr, "GET", "/api/v1/channels", Some(&viewer), None)
            .await
            .status,
        403
    );

    // 新增管道：回應只有遮蔽值；資料庫中的機密是密文
    let r = call(
        addr,
        "POST",
        "/api/v1/channels",
        Some(&admin),
        Some(&channel),
    )
    .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let text = String::from_utf8_lossy(&r.body).into_owned();
    assert!(
        !text.contains("SECRETTOKEN") && !text.contains("hk-secret"),
        "{text}"
    );
    let v = r.json();
    let cid = v["data"]["channel_id"].as_str().unwrap().to_string();
    assert_eq!(
        v["data"]["secrets"]["url"],
        format!("http://{}/…", hook.addr)
    );
    assert_eq!(v["data"]["secrets"]["hmac_secret"], "••••");
    let blob: Vec<u8> = f
        .admin()
        .query_row(
            "SELECT secrets FROM alert_channels WHERE channel_id = ?1",
            [&cid],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!String::from_utf8_lossy(&blob).contains("SECRETTOKEN"));
    // 不合法的設定：KRS-E5110（暫定）
    let bad = call(
        addr,
        "POST",
        "/api/v1/channels",
        Some(&admin),
        Some(&json!({"name": "x", "kind": "webhook", "config": {}, "secrets": {"url": "http://plain/"}})),
    )
    .await;
    assert_eq!(bad.status, 400);
    assert_eq!(bad.json()["error"]["code"], "channel_invalid");

    // 規則：維運者不能改系統預設規則的條件（只有管理員）
    let r = call(
        addr,
        "PUT",
        "/api/v1/alert-rules/sys.host_offline",
        Some(&operate),
        Some(&json!({ "params": { "grace_secs": 0 } })),
    )
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(
        r.json()["error"]["details"]["permission"],
        "alert-rules:admin"
    );
    let r = call(
        addr,
        "PUT",
        "/api/v1/alert-rules/sys.host_offline",
        Some(&admin),
        Some(&json!({ "channels": [cid], "params": { "grace_secs": 0 } })),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["data"]["params"]["grace_secs"], 0);
    // 系統規則的嚴重度不可低於預設
    let r = call(
        addr,
        "PUT",
        "/api/v1/alert-rules/sys.crl_expired",
        Some(&admin),
        Some(&json!({ "severity": "warning" })),
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["product_code"], "KRS-E5001");
    // 緊急等級的自訂規則必須設定升級對象
    let r = call(
        addr,
        "POST",
        "/api/v1/alert-rules",
        Some(&operate),
        Some(
            &json!({ "kind": "metric_threshold", "name": "CPU", "severity": "emergency",
                      "params": { "metric": "cpu", "op": ">", "threshold": 90 } }),
        ),
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("升級對象")
    );
    let r = call(
        addr,
        "POST",
        "/api/v1/alert-rules",
        Some(&operate),
        Some(
            &json!({ "kind": "metric_threshold", "name": "產線 CPU", "severity": "warning",
                      "params": { "metric": "cpu", "op": ">", "threshold": 90, "for_secs": 600 },
                      "scope": { "tags": ["line3"] }, "channels": [cid] }),
        ),
    )
    .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let custom = r.json()["data"]["rule_id"].as_str().unwrap().to_string();
    // 歷史試算（沒有彙總資料：supported、0 次、附說明）
    let r = call(
        addr,
        "POST",
        "/api/v1/alert-rules/preview",
        Some(&auditor),
        Some(&json!({ "rule_id": custom })),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["data"]["supported"], true);
    let r = call(
        addr,
        "POST",
        "/api/v1/alert-rules/preview",
        Some(&auditor),
        Some(&json!({ "rule_id": "sys.host_offline" })),
    )
    .await;
    assert_eq!(r.json()["data"]["supported"], false);

    // 主機上線、離線 → 事件 → Webhook
    let mut sse = Sse::open(addr, &viewer, None).await;
    let _ = sse.next().await; // ready
    // 0.5 的 token 也可以開 SSE（主機的讀取），但收不到告警的事件
    let mut legacy_sse = Sse::open(addr, &legacy, None).await;
    let _ = legacy_sse.next().await; // ready
    f.registry
        .register(
            77,
            hello(H, "hmi-line3-07", "10.30.3.27:50000"),
            Instant::now(),
            chrono::Utc::now(),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5500)).await;
    assert!(f.registry.disconnect(H, 77, Instant::now()));
    wait_for("Webhook 收到通知", Duration::from_secs(30), || {
        hook.count() >= 1
    })
    .await;
    let req = hook.last().unwrap();
    let body: Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(body["schema"], "kairos.alert.v1");
    assert_eq!(body["severity"], "critical");
    assert_eq!(body["kind"], "host_offline");
    assert_eq!(body["hosts"][0]["offline_reason"], "agent_closed");
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"hk-secret");
    let tag = ring::hmac::sign(&key, req.body.as_bytes());
    let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        req.header("x-kairos-signature"),
        Some(format!("sha256={hex}"))
    );
    let event_id = body["event_id"].as_str().unwrap().to_string();
    // SSE：alert.created
    let mut saw = false;
    for _ in 0..20 {
        let Some(ev) = tokio::time::timeout(Duration::from_secs(10), sse.next())
            .await
            .ok()
            .flatten()
        else {
            break;
        };
        if ev.event == "alert.created" && ev.data["event_id"] == event_id {
            saw = true;
            break;
        }
    }
    assert!(saw, "SSE 應推送 alert.created");
    // /metrics：告警引擎的狀態、建立的事件與 Webhook 的發送（全域計數，其他測試也會累加：只檢查下限）
    let r = super::tests::request(addr, "GET", "/metrics", None, &[]).await;
    assert_eq!(r.status, 200);
    let text = String::from_utf8(r.body.clone()).unwrap();
    let p = kp_obs::prom::parse(&text).unwrap_or_else(|e| {
        panic!(
            "{e}
{text}"
        )
    });
    kp_obs::prom::lint(&p, "kairos_").unwrap();
    assert_eq!(p.get("kairos_alerts_engine_enabled", &[]), Some(1.0));
    assert!(
        p.get("kairos_alerts_fired_total", &[("severity", "critical")])
            .unwrap()
            >= 1.0,
        "{text}"
    );
    assert!(
        p.get(
            "kairos_alert_notification_sends_total",
            &[("kind", "webhook"), ("result", "ok")]
        )
        .unwrap()
            >= 1.0,
        "{text}"
    );
    assert!(
        p.get(
            "kairos_alerts_open",
            &[("severity", "critical"), ("state", "firing")]
        )
        .unwrap()
            >= 1.0,
        "{text}"
    );
    assert_eq!(
        p.get("kairos_alert_channels", &[("state", "ok")]),
        Some(1.0)
    );
    // 0.5 的 token 的串流：同一段時間的主機事件有送出，告警的事件沒有
    let mut legacy_kinds = Vec::new();
    while let Ok(Some(ev)) =
        tokio::time::timeout(Duration::from_millis(1500), legacy_sse.next()).await
    {
        legacy_kinds.push(ev.event);
    }
    assert!(
        legacy_kinds.iter().any(|k| k.starts_with("host.")),
        "{legacy_kinds:?}"
    );
    assert!(
        !legacy_kinds.iter().any(|k| k.starts_with("alert.")),
        "{legacy_kinds:?}"
    );

    // 列表、摘要、詳情
    let r = call(
        addr,
        "GET",
        "/api/v1/alerts?view=needs_me",
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["page"]["total"], 1);
    assert_eq!(v["data"][0]["event_id"], event_id);
    assert_eq!(v["meta"]["engine"]["running"], true);
    let r = call(addr, "GET", "/api/v1/alerts/summary", Some(&viewer), None).await;
    assert_eq!(
        r.json()["data"]["counts"]["active_by_severity"]["critical"],
        1
    );
    let r = call(
        addr,
        "GET",
        &format!("/api/v1/alerts/{event_id}"),
        Some(&viewer),
        None,
    )
    .await;
    let v = r.json();
    assert!(
        v["data"]["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["action"] == "notified")
    );
    assert_eq!(v["data"]["deliveries"][0]["channel_name"], "SIEM");
    assert!(
        v["data"]["actions"]
            .as_array()
            .unwrap()
            .contains(&json!("ack"))
    );

    // 確認：唯讀不行；維運者可以（確認並指派給我）
    assert_eq!(
        call(
            addr,
            "POST",
            &format!("/api/v1/alerts/{event_id}/ack"),
            Some(&viewer),
            Some(&json!({}))
        )
        .await
        .status,
        403
    );
    let r = call(
        addr,
        "POST",
        &format!("/api/v1/alerts/{event_id}/ack"),
        Some(&operate),
        Some(&json!({ "assign_to_me": true, "eta_minutes": 30 })),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = r.json();
    assert_eq!(v["data"]["event"]["state"], "acknowledged");
    assert!(
        v["data"]["event"]["assignee"]
            .as_str()
            .unwrap()
            .starts_with("token ")
    );
    // 不合法的根因
    let r = call(
        addr,
        "POST",
        &format!("/api/v1/alerts/{event_id}/resolve"),
        Some(&operate),
        Some(&json!({ "root_cause": "bad" })),
    )
    .await;
    assert_eq!(r.status, 400);
    let r = call(
        addr,
        "POST",
        "/api/v1/alerts/ack",
        Some(&operate),
        Some(&json!({ "event_ids": [event_id, "E-NOPE"] })),
    )
    .await;
    assert_eq!(r.json()["data"]["summary"]["failed"], 1);

    // 靜音：伺服器端的時長上限（只選「撤銷清單即將到期」：系統＋緊急 → 4 小時）
    let r = call(
        addr,
        "POST",
        "/api/v1/silences",
        Some(&operate),
        Some(&json!({ "kind": "silence", "name": "CRL", "reason": "更新中", "duration_secs": 5 * 3600,
                      "rules": ["sys.crl_expiring"] })),
    )
    .await;
    assert_eq!(r.status, 400);
    let v = r.json();
    assert_eq!(v["error"]["code"], "silence_too_long");
    assert_eq!(v["error"]["details"]["max_secs"], 4 * 3600);
    // 預覽：符合台數與組合說明
    let r = call(
        addr,
        "POST",
        "/api/v1/silences/preview",
        Some(&viewer),
        Some(
            &json!({ "scope": { "tags": ["line3"], "subnets": ["10.30.3.0/24", "10.99.0.0/16"] } }),
        ),
    )
    .await;
    let v = r.json();
    assert_eq!(v["data"]["matching_hosts"], 1, "{v}");
    assert_eq!(
        v["data"]["scope_text"],
        "範圍：標籤為 line3，且網段為 10.30.3.0/24 或 10.99.0.0/16"
    );
    assert_eq!(
        v["data"]["limit"]["max_secs"],
        4 * 3600,
        "不選規則＝所有規則（含緊急）"
    );
    // 群組還不支援
    let r = call(
        addr,
        "POST",
        "/api/v1/silences/preview",
        Some(&viewer),
        Some(&json!({ "scope": { "groups": ["B廠"] } })),
    )
    .await;
    assert_eq!(r.status, 400);
    // 建立、列表、提前結束
    let r = call(
        addr,
        "POST",
        "/api/v1/silences",
        Some(&operate),
        Some(&json!({ "kind": "maintenance", "name": "歲修", "reason": "年度歲修", "duration_secs": 2 * 86_400,
                      "scope": { "subnets": ["10.30.3.0/24"] } })),
    )
    .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let sid = r.json()["data"]["silence_id"].as_str().unwrap().to_string();
    let r = call(
        addr,
        "GET",
        "/api/v1/silences?status=active",
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.json()["data"][0]["matching_hosts"], 1);
    let r = call(
        addr,
        "POST",
        &format!("/api/v1/silences/{sid}/end"),
        Some(&operate),
        Some(&json!({ "reason": "提前完工" })),
    )
    .await;
    assert_eq!(r.json()["data"]["status"], "ended");

    // 規則清單：系統預設規則不可刪除；停用需原因
    let r = call(
        addr,
        "DELETE",
        "/api/v1/alert-rules/sys.host_offline",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(r.status, 409);
    let r = call(
        addr,
        "POST",
        "/api/v1/alert-rules/sys.report_stale/disable",
        Some(&operate),
        Some(&json!({})),
    )
    .await;
    assert_eq!(r.status, 400);
    let r = call(
        addr,
        "POST",
        "/api/v1/alert-rules/sys.report_stale/disable",
        Some(&operate),
        Some(&json!({ "reason": "測試" })),
    )
    .await;
    assert_eq!(r.json()["data"]["enabled"], false);
    let r = call(addr, "GET", "/api/v1/alert-rules", Some(&auditor), None).await;
    let rules = r.json()["data"].as_array().unwrap().clone();
    assert!(rules.len() >= 31);
    let ho = rules
        .iter()
        .find(|x| x["rule_id"] == "sys.host_offline")
        .unwrap();
    assert_eq!(ho["fires_7d"], 1);

    // 刪除管道（L2）：需要名稱確認與替代管道或確認沒有通知對象
    let r = call(
        addr,
        "DELETE",
        &format!("/api/v1/channels/{cid}"),
        Some(&admin),
        Some(&json!({})),
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(
        !r.json()["error"]["details"]["rules"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let r = call(
        addr,
        "DELETE",
        &format!("/api/v1/channels/{cid}"),
        Some(&admin),
        Some(&json!({ "confirm_name": "SIEM", "accept_no_channel": true })),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));

    // 稽核紀錄：寫入都有記錄（類別 alerts），不含機密
    let audit: Vec<(String, String)> = {
        let conn = f.admin();
        let mut stmt = conn
            .prepare(
                "SELECT action, coalesce(details, '') FROM audit_events WHERE category = 'alerts'",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    for action in [
        "channel.create",
        "alert_rule.update",
        "alert.ack",
        "silence.create",
        "silence.end",
        "channel.delete",
    ] {
        assert!(
            audit.iter().any(|(a, _)| a == action),
            "{action}: {audit:?}"
        );
    }
    assert!(
        audit
            .iter()
            .all(|(_, d)| !d.contains("SECRETTOKEN") && !d.contains("hk-secret"))
    );
    // 告警與靜音的操作接上稽核雜湊鏈：執行者為 token:<id>、結果 ok，鏈完整
    {
        let conn = f.admin();
        let (actor, result, hash): (String, String, Option<String>) = conn
            .query_row(
                "SELECT actor, result, hash FROM audit_events WHERE category = 'alerts' AND action = 'alert.ack' ORDER BY id LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(actor.starts_with("token:"), "{actor}");
        assert_eq!(result, "ok");
        assert_eq!(hash.map(|h| h.len()), Some(64));
        let v = crate::audit::verify(&conn).unwrap();
        assert!(v.ok(), "{v:?}");
    }
    f.stop().await;
}
