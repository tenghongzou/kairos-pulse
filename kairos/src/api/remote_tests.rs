//! 遠端管理端點（[`super::remote`]）的整合測試：實際的 HTTPS 主控台＋SQLite＋遠端管理服務。
//! 集中設定（設定檔、版本、指派、衝突、預覽、同步狀態、主機的設定）、版本庫（匯入、上傳、刪除）、
//! 推送（預覽、建立、送出的 L2 確認、主機表分頁、暫停與繼續的權限、`pause_cause`、清單的 meta、取消）、系統健康的
//! `remote` 項目、`[remote]` 停用時的 503。

use serde_json::{Value, json};

use super::tests::{Fixture, Resp, call, fixture, send_raw};
use super::tokens::{self, TokenGrant};
use crate::rbac::Role;
use crate::remote::test_sign::{TestSigner, release};

const H: &str = "7e2f0a11-0000-4000-8000-0000000000c1";

fn token(f: &Fixture, role: Role) -> String {
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

fn add_host(f: &Fixture, id: &str, version: &str, method: &str) {
    let conn = f.admin();
    conn.execute(
            "INSERT INTO hosts (agent_id, hostname, agent_version, host_group, tags, os, install_method,
                install_upgrade, install_config, first_seen, last_seen, online)
             VALUES (?1, 'web-01', ?2, 'web', '[\"env:prod\"]', '{\"arch\":\"x86_64\"}', ?3, 'ready', 'enabled', 0, 0, 1)",
            rusqlite::params![id, version, method],
        )
        .unwrap();
}

fn body(r: &Resp) -> Value {
    r.json()
}

/// 上傳發佈檔（`application/octet-stream`）
async fn put_artifact(f: &Fixture, token: &str, version: &str, name: &str, data: &[u8]) -> Resp {
    let mut req = format!(
        "PUT /api/v1/releases/{version}/artifacts/{name} HTTP/1.0\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
        data.len()
    )
    .into_bytes();
    req.extend_from_slice(data);
    send_raw(f.addr(), &req).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn central_config_endpoints_over_https() {
    let f = fixture("remote-cfg", |_| {}).await;
    let addr = f.addr();
    let admin = token(&f, Role::Admin);
    let op = token(&f, Role::Operator);
    let viewer = token(&f, Role::Viewer);
    add_host(&f, H, "0.9.0", "tar");

    let r = call(addr, "GET", "/api/v1/config/schema", Some(&op), None).await;
    assert_eq!(r.status, 200);
    let keys = body(&r)["data"]["keys"].as_array().unwrap().len();
    assert_eq!(keys, kairos_proto::mgmt::settings::SETTINGS.len());
    let r = call(addr, "GET", "/api/v1/config/schema", Some(&viewer), None).await;
    assert_eq!(r.status, 403);
    assert_eq!(body(&r)["error"]["details"]["permission"], "config:read");

    // 建立（維運者可以，D9）；不合法的值 400 config_invalid（KRS-E0502）
    let bad = call(
        addr,
        "POST",
        "/api/v1/config/profiles",
        Some(&op),
        Some(&json!({"name": "x", "settings": {"report": {"interval": 0}}})),
    )
    .await;
    assert_eq!(bad.status, 400);
    let e = body(&bad);
    assert_eq!(
        (
            e["error"]["code"].as_str(),
            e["error"]["product_code"].as_str()
        ),
        (Some("config_invalid"), Some("KRS-E0502"))
    );
    assert_eq!(e["error"]["details"]["key"], "report.interval");
    let r = call(addr, "POST", "/api/v1/config/profiles", Some(&op), Some(&json!({"name": "正式環境 Linux", "description": "預設", "settings": {"report": {"interval": 10}}}))).await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let id = body(&r)["data"]["id"].as_i64().unwrap();
    assert_eq!(body(&r)["data"]["keys"], 1);
    let r = call(addr, "PUT", &format!("/api/v1/config/profiles/{id}"), Some(&op), Some(&json!({"settings": {"report": {"interval": 15}, "log": {"level": "debug"}}, "reason": "除錯"}))).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(body(&r)["data"]["revision"], 2);
    let r = call(
        addr,
        "GET",
        &format!("/api/v1/config/profiles/{id}/revisions"),
        Some(&op),
        None,
    )
    .await;
    let revs = body(&r)["data"].clone();
    assert_eq!(revs.as_array().unwrap().len(), 2);
    assert_eq!(
        revs[0]["changes"]["report.interval"],
        json!({"before": 10, "after": 15})
    );
    assert_eq!(revs[0]["reason"], "除錯");

    // 指派與影響預覽
    let r = call(
        addr,
        "POST",
        "/api/v1/config/preview",
        Some(&op),
        Some(&json!({"assign": {"profile_id": id, "selector": "group:WEB"}})),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(body(&r)["data"]["affected_hosts"], 1);
    assert!(body(&r)["data"]["impact_token"].is_string());
    let r = call(addr, "POST", "/api/v1/config/assignments", Some(&op), Some(&json!({"profile_id": id, "selector": "group:WEB", "expires_at": "2099-01-01T00:00:00Z"}))).await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let a = body(&r)["data"].clone();
    assert_eq!(
        (a["selector"].as_str(), a["priority"].as_i64()),
        (Some("group:web"), Some(100))
    );
    let r = call(
        addr,
        "POST",
        "/api/v1/config/assignments",
        Some(&op),
        Some(&json!({"profile_id": id, "selector": "host:web-01"})),
    )
    .await;
    assert_eq!(r.status, 400);
    let r = call(
        addr,
        "GET",
        "/api/v1/config/assignments?selector_kind=group",
        Some(&op),
        None,
    )
    .await;
    assert_eq!(body(&r)["data"][0]["hosts"], 1);
    let r = call(addr, "GET", "/api/v1/config/profiles", Some(&op), None).await;
    assert_eq!(
        (
            body(&r)["data"][0]["assignments"].as_i64(),
            body(&r)["data"][0]["hosts"].as_i64()
        ),
        (Some(1), Some(1))
    );

    // 主機的設定與全站的同步狀態（尚未回報）
    let r = call(
        addr,
        "GET",
        &format!("/api/v1/hosts/{H}/config"),
        Some(&op),
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let hc = body(&r)["data"].clone();
    assert_eq!(hc["config_state"], "never_reported");
    assert_eq!(
        hc["desired"],
        json!({"log": {"level": "debug"}, "report": {"interval": 15}})
    );
    assert_eq!(hc["desired_keys"][0]["source"], "正式環境 Linux");
    assert_eq!(hc["install"]["method"], "tar");
    let r = call(addr, "GET", "/api/v1/config/status", Some(&op), None).await;
    assert_eq!(body(&r)["data"]["counts"]["never_reported"], 1);
    let r = call(
        addr,
        "GET",
        "/api/v1/hosts/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee/config",
        Some(&op),
        None,
    )
    .await;
    assert_eq!(r.status, 404);
    // 主機清單的欄位與篩選（索引有短暫的快取：重新建立）
    f.console.ctx.hosts.invalidate().await;
    let r = call(
        addr,
        "GET",
        "/api/v1/hosts?config_state=never_reported&remote_upgrade=ready",
        Some(&op),
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let hosts = body(&r)["data"].clone();
    assert_eq!(
        hosts[0]["agent_id"],
        H,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(hosts[0]["config_state"], "never_reported");
    assert_eq!(hosts[0]["install"]["method"], "tar");
    let r = call(
        addr,
        "GET",
        "/api/v1/hosts?remote_upgrade=none",
        Some(&op),
        None,
    )
    .await;
    assert_eq!(body(&r)["data"].as_array().unwrap().len(), 0);
    let r = call(
        addr,
        "GET",
        "/api/v1/hosts?config_state=bogus",
        Some(&op),
        None,
    )
    .await;
    assert_eq!(r.status, 400);

    // 有指派的設定檔不能刪；解除指派後可以
    let r = call(
        addr,
        "DELETE",
        &format!("/api/v1/config/profiles/{id}"),
        Some(&op),
        None,
    )
    .await;
    assert_eq!(
        (r.status, body(&r)["error"]["code"].as_str()),
        (409, Some("profile_in_use"))
    );
    let aid = a["id"].as_i64().unwrap();
    let r = call(
        addr,
        "DELETE",
        &format!("/api/v1/config/assignments/{aid}"),
        Some(&op),
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let r = call(
        addr,
        "DELETE",
        &format!("/api/v1/config/profiles/{id}"),
        Some(&admin),
        Some(&json!({"reason": "不用了"})),
    )
    .await;
    assert_eq!(r.status, 200);
    let r = call(
        addr,
        "GET",
        &format!("/api/v1/config/profiles/{id}"),
        Some(&op),
        None,
    )
    .await;
    assert_eq!(r.status, 404);
    // 方法不符
    let r = call(
        addr,
        "PATCH",
        "/api/v1/config/profiles",
        Some(&op),
        Some(&json!({})),
    )
    .await;
    assert_eq!(r.status, 405);
    f.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_unavailable_when_disabled() {
    let f = fixture("remote-off", |c| {
        c.remote.config_enabled = false;
        c.remote.upgrade_enabled = false;
    })
    .await;
    let admin = token(&f, Role::Admin);
    for path in [
        "/api/v1/config/profiles",
        "/api/v1/releases",
        "/api/v1/rollouts",
    ] {
        let r = call(f.addr(), "GET", path, Some(&admin), None).await;
        assert_eq!(r.status, 503, "{path}");
        assert_eq!(body(&r)["error"]["code"], "remote_unavailable");
    }
    // 系統健康：停用是正常狀態
    let r = call(f.addr(), "GET", "/api/v1/system/health", Some(&admin), None).await;
    let it = body(&r)["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "remote")
        .cloned()
        .unwrap();
    assert_eq!(it["status"], "ok");
    assert_eq!(
        (
            it["values"]["config_enabled"].as_bool(),
            it["values"]["upgrade_enabled"].as_bool()
        ),
        (Some(false), Some(false))
    );
    assert!(it["summary"].as_str().unwrap().starts_with("已停用"));
    f.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn releases_and_rollouts_over_https() {
    let signer = TestSigner::new(11);
    let trust = crate::api::tests::temp_dir("remote-trust").join("allowed_signers");
    std::fs::create_dir_all(trust.parent().unwrap()).unwrap();
    std::fs::write(&trust, signer.allowed_signers()).unwrap();
    let t = trust.to_string_lossy().into_owned();
    let f = fixture("remote-rel", move |c| c.remote.allowed_signers = t).await;
    let addr = f.addr();
    let admin = token(&f, Role::Admin);
    let op = token(&f, Role::Operator);
    let auditor = token(&f, Role::Auditor);
    let rel = release(&signer, "0.9.2", "abcdef1");
    let import =
        json!({"sha256sums": rel.sha256sums, "signature": rel.signature, "manifest": rel.manifest});
    let r = call(addr, "POST", "/api/v1/releases", Some(&op), Some(&import)).await;
    assert_eq!(r.status, 403, "匯入只限管理員");
    let r = call(addr, "POST", "/api/v1/releases", Some(&admin), Some(&json!({"sha256sums": rel.sha256sums, "signature": rel.signature, "source": "external"}))).await;
    assert_eq!(r.status, 400);
    let r = call(addr, "POST", "/api/v1/releases", Some(&admin), Some(&json!({"sha256sums": rel.sha256sums, "signature": TestSigner::new(12).sign(rel.sha256sums.as_bytes()), "manifest": rel.manifest}))).await;
    assert_eq!(r.status, 400);
    assert_eq!(
        (
            body(&r)["error"]["code"].as_str(),
            body(&r)["error"]["product_code"].as_str()
        ),
        (Some("release_signature_invalid"), Some("KRS-E8413"))
    );
    let r = call(
        addr,
        "POST",
        "/api/v1/releases",
        Some(&admin),
        Some(&import),
    )
    .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(body(&r)["data"]["verified"], true);
    // 上傳
    let (tar_name, tar) = rel
        .files
        .iter()
        .find(|(n, _)| n.ends_with(".tar.gz"))
        .unwrap()
        .clone();
    let r = put_artifact(&f, &admin, "0.9.2", &tar_name, b"not the file").await;
    assert_eq!(
        (r.status, body(&r)["error"]["code"].as_str()),
        (400, Some("artifact_hash_mismatch"))
    );
    let r = put_artifact(&f, &admin, "0.9.2", &tar_name, &tar).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(body(&r)["data"]["stored"], true);
    let r = put_artifact(&f, &op, "0.9.2", &tar_name, &tar).await;
    assert_eq!(r.status, 403);
    let r = call(addr, "GET", "/api/v1/releases", Some(&auditor), None).await;
    assert_eq!(r.status, 200);
    let list = body(&r);
    assert_eq!(list["data"][0]["version"], "0.9.2");
    assert_eq!(list["meta"]["allowed_signers_configured"], true);
    let stored: Vec<bool> = list["data"][0]["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["stored"].as_bool().unwrap())
        .collect();
    assert_eq!(stored.iter().filter(|s| **s).count(), 1);
    let r = call(addr, "GET", "/api/v1/releases/0.9.2", Some(&auditor), None).await;
    assert_eq!(r.status, 200);
    assert!(body(&r)["data"]["sha256sums"].is_string());

    // 推送
    add_host(&f, H, "0.9.0", "tar");
    let spec = json!({"name": "0.9.2 web", "target_version": "0.9.2", "selector": {"groups": ["web"]}, "waves": [{"size": "rest"}]});
    let r = call(
        addr,
        "POST",
        "/api/v1/rollouts/preview",
        Some(&op),
        Some(&spec),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        (
            body(&r)["data"]["eligible"].as_i64(),
            body(&r)["data"]["confirm_text"].as_str()
        ),
        (Some(1), Some("升級 1 台"))
    );
    let r = call(addr, "POST", "/api/v1/rollouts", Some(&op), Some(&spec)).await;
    assert_eq!(r.status, 403, "建立只限管理員（D8）");
    let r = call(addr, "POST", "/api/v1/rollouts", Some(&admin), Some(&json!({"name": "x", "target_version": "0.9.2", "selector": {"all": true}, "schedule": {}}))).await;
    assert_eq!(
        (r.status, body(&r)["error"]["details"]["parameter"].as_str()),
        (400, Some("schedule"))
    );
    let r = call(addr, "POST", "/api/v1/rollouts", Some(&admin), Some(&spec)).await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let id = body(&r)["data"]["id"].as_str().unwrap().to_string();
    let r = call(
        addr,
        "GET",
        &format!("/api/v1/rollouts/{id}"),
        Some(&op),
        None,
    )
    .await;
    assert_eq!(body(&r)["data"]["preview"]["eligible"], 1);
    let r = call(
        addr,
        "POST",
        &format!("/api/v1/rollouts/{id}/submit"),
        Some(&admin),
        Some(&json!({})),
    )
    .await;
    assert_eq!(
        (
            r.status,
            body(&r)["error"]["details"]["confirm_text"].as_str()
        ),
        (400, Some("升級 1 台"))
    );
    let r = call(
        addr,
        "POST",
        &format!("/api/v1/rollouts/{id}/submit"),
        Some(&admin),
        Some(&json!({"confirm": "升級 1 台"})),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(body(&r)["data"]["state"], "running");
    let r = call(
        addr,
        "GET",
        &format!("/api/v1/rollouts/{id}/hosts?limit=1"),
        Some(&auditor),
        None,
    )
    .await;
    let page = body(&r);
    assert_eq!(
        (
            page["page"]["total"].as_i64(),
            page["data"][0]["state"].as_str()
        ),
        (Some(1), Some("pending"))
    );
    let r = call(
        addr,
        "GET",
        "/api/v1/rollouts?state=running",
        Some(&auditor),
        None,
    )
    .await;
    assert_eq!(body(&r)["data"][0]["next_action"]["code"], "upgrading");
    // 維運者可以暫停（緊急煞車），不能繼續
    let r = call(
        addr,
        "POST",
        &format!("/api/v1/rollouts/{id}/pause"),
        Some(&op),
        Some(&json!({})),
    )
    .await;
    assert_eq!(r.status, 400, "需要 reason");
    let r = call(
        addr,
        "POST",
        &format!("/api/v1/rollouts/{id}/pause"),
        Some(&op),
        Some(&json!({"reason": "值班暫停"})),
    )
    .await;
    assert_eq!(
        (r.status, body(&r)["data"]["state"].as_str()),
        (200, Some("paused"))
    );
    assert_eq!(body(&r)["data"]["pause_cause"], "manual");
    // 清單的 meta：需要人處理的數量（不受 state 篩選影響）
    let r = call(
        addr,
        "GET",
        "/api/v1/rollouts?state=running",
        Some(&auditor),
        None,
    )
    .await;
    let list = body(&r);
    assert_eq!(list["data"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        list["meta"],
        json!({"paused": 1, "rollback_failed_unacknowledged": 0})
    );
    // 系統健康的「遠端管理」：主控台暫停不算異常
    let viewer = token(&f, Role::Viewer);
    let remote_item = |r: &Resp| {
        body(r)["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == "remote")
            .cloned()
            .expect("remote 項目")
    };
    let r = call(addr, "GET", "/api/v1/system/health", Some(&auditor), None).await;
    let it = remote_item(&r);
    assert_eq!(it["status"], "ok", "{it}");
    assert_eq!(
        (
            it["values"]["upgrade_enabled"].as_bool(),
            it["values"]["allowed_signers_configured"].as_bool(),
            it["values"]["rollouts_paused"].as_i64(),
            it["values"]["rollouts_auto_paused"].as_i64(),
            it["values"]["releases"].as_i64(),
        ),
        (Some(true), Some(true), Some(1), Some(0), Some(1))
    );
    assert_eq!(it["values"]["paused"][0]["cause"], "manual");
    assert!(
        it["summary"].as_str().unwrap().contains("已暫停 1 個"),
        "{it}"
    );
    // 回滾失敗（以 SQL 模擬）：嚴重、KRS-E8408；清單的每筆與 meta 都有未處理的台數；唯讀看不到細節
    f.admin()
        .execute("UPDATE rollout_hosts SET state = 'rollback_failed'", [])
        .unwrap();
    let r = call(addr, "GET", "/api/v1/system/health", Some(&auditor), None).await;
    let it = remote_item(&r);
    assert_eq!(
        (it["status"].as_str(), it["product_code"].as_str()),
        (Some("critical"), Some("KRS-E8408"))
    );
    assert_eq!(body(&r)["data"]["status"], "critical");
    let r = call(addr, "GET", "/api/v1/system/health", Some(&viewer), None).await;
    let it = remote_item(&r);
    assert_eq!(
        (
            it["restricted"].as_bool(),
            it["values"].is_null(),
            it["status"].as_str()
        ),
        (Some(true), true, Some("critical"))
    );
    let r = call(addr, "GET", "/api/v1/rollouts", Some(&auditor), None).await;
    let list = body(&r);
    assert_eq!(list["meta"]["rollback_failed_unacknowledged"], 1);
    assert_eq!(list["data"][0]["rollback_failed_unacknowledged"], 1);
    f.admin()
        .execute("UPDATE rollout_hosts SET state = 'pending'", [])
        .unwrap();
    let r = call(
        addr,
        "POST",
        &format!("/api/v1/rollouts/{id}/resume"),
        Some(&op),
        None,
    )
    .await;
    assert_eq!(r.status, 403);
    let r = call(
        addr,
        "POST",
        &format!("/api/v1/rollouts/{id}/resume"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(
        (r.status, body(&r)["data"]["state"].as_str()),
        (200, Some("running"))
    );
    let r = call(
        addr,
        "POST",
        &format!("/api/v1/rollouts/{id}/continue"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(
        (r.status, body(&r)["error"]["code"].as_str()),
        (409, Some("invalid_state"))
    );
    // 使用中的版本不能刪
    let r = call(
        addr,
        "DELETE",
        "/api/v1/releases/0.9.2",
        Some(&admin),
        Some(&json!({"confirm": "0.9.2"})),
    )
    .await;
    assert_eq!(
        (r.status, body(&r)["error"]["code"].as_str()),
        (409, Some("release_in_use"))
    );
    let r = call(
        addr,
        "POST",
        &format!("/api/v1/rollouts/{id}/cancel"),
        Some(&admin),
        Some(&json!({"reason": "測試"})),
    )
    .await;
    assert_eq!(
        (r.status, body(&r)["data"]["state"].as_str()),
        (200, Some("cancelled"))
    );
    let r = call(
        addr,
        "GET",
        &format!("/api/v1/hosts/{H}/upgrade"),
        Some(&auditor),
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(body(&r)["data"]["upgrades"][0]["state"], "cancelled");
    assert_eq!(body(&r)["data"]["install"]["upgrade"], "ready");
    // L2：輸入版本號
    let r = call(
        addr,
        "DELETE",
        "/api/v1/releases/0.9.2",
        Some(&admin),
        Some(&json!({})),
    )
    .await;
    assert_eq!(
        (r.status, body(&r)["error"]["code"].as_str()),
        (400, Some("confirmation_required"))
    );
    let r = call(
        addr,
        "DELETE",
        "/api/v1/releases/0.9.2",
        Some(&admin),
        Some(&json!({"confirm": "0.9.2"})),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let r = call(addr, "GET", "/api/v1/releases/0.9.2", Some(&admin), None).await;
    assert_eq!(r.status, 404);
    // 稽核
    let conn = f.admin();
    let actions: Vec<String> = {
        let mut st = conn.prepare("SELECT action FROM audit_events").unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    for a in [
        "release.import",
        "release.import_failed",
        "release.delete",
        "rollout.create",
        "rollout.submit",
        "rollout.pause",
        "rollout.resume",
        "rollout.cancel",
    ] {
        assert!(actions.iter().any(|x| x == a), "{a}");
    }
    let _ = std::fs::remove_dir_all(trust.parent().unwrap());
    f.stop().await;
}
