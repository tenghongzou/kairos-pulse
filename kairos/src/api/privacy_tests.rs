//! 個資端點（[`super::privacy`]）的整合測試：實際的 HTTPS 主控台＋SQLite＋告警服務。
//! 權限（只有管理員）、影響預覽、仍連線中拒絕、L2 確認、`impact_token`、個資匯出、刪除後的清冊與稽核、
//! 撤銷憑證、刪除後不會被建立回來。

use serde_json::{Value, json};
use std::time::{Duration, Instant};

use super::tests::{call, fixture_alerts, hello};
use super::tokens::{self, TokenGrant};
use crate::rbac::Role;
use crate::store::tests::pulse_like;

const H: &str = "7e2f0a11-0000-4000-8000-0000000000b7";

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_and_personal_data_over_https() {
    let f = fixture_alerts("privacy", |c| c.alerts.warmup_secs = 60).await;
    let addr = f.addr();
    let admin = token(&f, Role::Admin);
    let operator = token(&f, Role::Operator);
    let now = chrono::Utc::now();
    f.registry
        .register(
            21,
            hello(H, "ann-laptop", "10.30.3.27:51544"),
            Instant::now(),
            now,
        )
        .unwrap();
    // 指標轉出（Prometheus exporter、OTLP 都啟用；不啟動推送工作，資料留在佇列）：刪除後記憶體中不留這台主機的資料
    let hub = {
        let mut c = crate::config::AppConfig::default();
        c.export.prometheus.enabled = true;
        c.export.otlp.enabled = true;
        c.export.otlp.endpoint = "http://127.0.0.1:9/v1/metrics".into();
        crate::export::ExportHub::new(
            crate::export::settings::effective(&c, &[], None),
            None,
            None,
        )
    };
    f.registry.set_export_hub(hub.clone());
    for i in 1..=2u64 {
        let revived = std::sync::atomic::AtomicBool::new(false);
        let outcome = crate::connection::store_report(
            &f.registry,
            &f.sink,
            21,
            pulse_like(H, i, now, 5.0),
            Duration::from_secs(5),
            &revived,
        )
        .await;
        assert!(matches!(
            outcome,
            crate::connection::StoreOutcome::Stored(_)
        ));
    }
    let now_ms = now.timestamp_millis();
    // 清冊的主機名稱（第一筆 report 寫入的是 pulse_like 的名稱；30 秒的清冊同步會改成 hello 的名稱）
    f.admin()
        .execute(
            "UPDATE hosts SET hostname = 'ann-laptop' WHERE agent_id = ?1",
            [H],
        )
        .unwrap();
    f.admin()
        .execute(
            "INSERT INTO pki_certificates (serial, ca_id, agent_id, spki_sha256, not_before, not_after, cert_der, issued_at, issued_via)
             VALUES ('C0FFEE01', 1, ?1, x'01', ?2, ?3, x'02', ?2, 'token')",
            rusqlite::params![H, now_ms - 86_400_000, now_ms + 300 * 86_400_000],
        )
        .unwrap();
    let path = format!("/api/v1/hosts/{H}/purge");
    assert!(hub.latest().contains_key(H));
    assert!(hub.queue_points() > 0);

    // 只有管理員：維運者 403（權限名稱）
    let r = call(addr, "GET", &path, Some(&operator), None).await;
    assert_eq!(r.status, 403);
    assert_eq!(r.json()["error"]["details"]["permission"], "hosts:purge");
    let r = call(
        addr,
        "GET",
        &format!("/api/v1/hosts/{H}/personal-data"),
        Some(&operator),
        None,
    )
    .await;
    assert_eq!(r.status, 403);

    // 影響預覽：仍連線中
    let r = call(addr, "GET", &path, Some(&admin), None).await;
    assert_eq!(r.status, 200, "{}", r.json());
    let p = r.json()["data"].clone();
    assert_eq!(p["online"], true);
    assert_eq!(p["blocked"]["code"], "host_online");
    assert_eq!(p["confirm_text"], "ann-laptop");
    assert_eq!(p["level"], "L2");
    assert_eq!(p["counts"]["reports"], 2);
    assert_eq!(p["revoke"]["certificates"], 1);
    let body =
        json!({"confirm": "ann-laptop", "impact_token": p["impact_token"], "reason": "員工離職"});
    let r = call(addr, "POST", &path, Some(&admin), Some(&body)).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json()["error"]["code"], "host_online");
    assert_eq!(r.json()["error"]["product_code"], "KRS-E4205");

    // 離線後：確認字串不符、impact_token 不符
    assert!(f.registry.disconnect(H, 21, Instant::now()));
    let bad = json!({"confirm": "ann", "impact_token": p["impact_token"]});
    let r = call(addr, "POST", &path, Some(&admin), Some(&bad)).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "confirmation_required");
    let stale = json!({"confirm": "ann-laptop", "impact_token": "0000"});
    let r = call(addr, "POST", &path, Some(&admin), Some(&stale)).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json()["error"]["code"], "impact_changed");
    assert_eq!(r.json()["error"]["details"]["confirm_text"], "ann-laptop");

    // 個資匯出
    let r = call(
        addr,
        "GET",
        &format!("/api/v1/hosts/{H}/personal-data"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    let doc = r.json()["data"].clone();
    assert_eq!(doc["schema"], "kairos.host.personal-data/v1");
    assert_eq!(doc["host"]["hostname"], "ann-laptop");
    assert_eq!(doc["certificates"][0]["serial"], "C0FFEE01");
    assert!(doc["certificates"][0].get("cert_der").is_none());
    assert_eq!(doc["reports"]["count"], 2);

    // 刪除
    let r = call(addr, "POST", &path, Some(&admin), Some(&body)).await;
    assert_eq!(r.status, 200, "{}", r.json());
    let d = r.json()["data"].clone();
    assert_eq!(d["deleted"]["reports"], 2);
    assert_eq!(d["deleted"]["hosts"], 1);
    assert_eq!(d["revoked_serials"], json!(["C0FFEE01"]));
    assert_eq!(d["kept"]["certificates_for_crl"], 1);
    assert!(f.registry.live_get(H).is_none());
    assert!(
        !hub.latest().contains_key(H),
        "Prometheus exporter 的最新值應已丟棄"
    );
    assert_eq!(hub.queue_points(), 0, "OTLP 佇列中這台主機的資料應已丟棄");
    let conn = f.admin();
    let one = |sql: &str| -> i64 { conn.query_row(sql, [H], |r| r.get(0)).unwrap() };
    assert_eq!(one("SELECT COUNT(*) FROM hosts WHERE agent_id = ?1"), 0);
    assert_eq!(
        one(
            "SELECT COUNT(*) FROM pki_certificates WHERE agent_id = ?1 AND revocation_reason = 'cessationOfOperation'"
        ),
        1
    );
    let actions: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT action || ':' || result FROM audit_events WHERE object IN (?1, 'C0FFEE01') ORDER BY id")
            .unwrap();
        stmt.query_map([H], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(
        actions,
        [
            "host.personal_data.export:ok",
            "cert.revoke:ok",
            "host.purge:ok"
        ]
    );
    let details: Value = serde_json::from_str(
        &conn
            .query_row(
                "SELECT details FROM audit_events WHERE action = 'host.purge'",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(details["reason"], "員工離職");
    assert_eq!(details["audit_log"], "kept");
    // 刪除後：預覽 404；同一台主機的 report 暫時被拒（不會被建立回來）
    let r = call(addr, "GET", &path, Some(&admin), None).await;
    assert_eq!(r.status, 404);
    let st = f.sink.write(pulse_like(H, 9, now, 1.0)).await;
    assert!(st.is_err(), "{st:?}");
    assert_eq!(one("SELECT COUNT(*) FROM hosts WHERE agent_id = ?1"), 0);
    // 方法不符
    let r = call(addr, "DELETE", &path, Some(&admin), None).await;
    assert_eq!(r.status, 405);
    f.stop().await;
}
