//! 遠端管理端點（[`super::remote`]）的整合測試：實際的 HTTPS 主控台＋SQLite＋遠端管理服務。
//! 集中設定（設定檔、版本、指派、衝突、預覽、同步狀態、主機的設定）、版本庫（匯入、上傳、刪除）、
//! 推送（預覽、建立、送出的 L2 確認、主機表分頁、暫停與繼續的權限、`pause_cause`、清單的 meta、取消）、系統健康的
//! `remote` 項目、`[remote]` 停用時的 503。

use serde_json::{Value, json};

use super::tests::{Fixture, Resp, Sse, call, fixture, send_raw};
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
    // 全域搜尋：清單的 q（名稱或說明；不分大小寫），需要 config:read
    for (q, n) in [("linux", 1), ("%E9%A0%90%E8%A8%AD", 1), ("zzz", 0)] {
        let r = call(
            addr,
            "GET",
            &format!("/api/v1/config/profiles?q={q}"),
            Some(&op),
            None,
        )
        .await;
        assert_eq!(r.status, 200);
        assert_eq!(body(&r)["data"].as_array().unwrap().len(), n, "q={q}");
    }
    let r = call(
        addr,
        "GET",
        "/api/v1/config/profiles?q=linux",
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.status, 403);
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
        "/api/v1/rollouts/summary",
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

    // 版本庫的檔案遺失（0.10.0）：清單與詳情的 artifacts[] 以 file_problem 標出（已存入而檔案不在、
    // 大小不符；未存入的為 null），主控台才不會把它當成「已上傳過」；重新上傳後補上
    let problems = |v: &serde_json::Value| -> Vec<(String, serde_json::Value)> {
        v["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["filename"].as_str().unwrap().to_string(),
                    a["file_problem"].clone(),
                )
            })
            .collect()
    };
    let detail = |who: &str| {
        let who = who.to_string();
        async move {
            let r = call(addr, "GET", "/api/v1/releases/0.9.2", Some(&who), None).await;
            assert_eq!(r.status, 200);
            body(&r)["data"].clone()
        }
    };
    let d = detail(&auditor).await;
    assert!(
        problems(&d).iter().all(|(_, p)| p.is_null()),
        "{:?}",
        problems(&d)
    );
    let tar_sha = d["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["filename"] == tar_name.as_str())
        .unwrap()["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    let stored_file = f.remote.as_ref().unwrap().releases_dir.join(&tar_sha);
    assert!(stored_file.is_file(), "{}", stored_file.display());
    std::fs::write(&stored_file, b"truncated").unwrap();
    let d = detail(&auditor).await;
    for (name, p) in problems(&d) {
        if name == tar_name {
            assert_eq!(p, "size_mismatch", "{name}");
        } else {
            assert!(p.is_null(), "{name}: {p}");
        }
    }
    std::fs::remove_file(&stored_file).unwrap();
    let r = call(addr, "GET", "/api/v1/releases", Some(&auditor), None).await;
    let listed = problems(&body(&r)["data"][0]);
    assert_eq!(
        listed
            .iter()
            .filter(|(_, p)| *p == "missing")
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>(),
        [tar_name.as_str()]
    );
    // 資料庫仍標為已存入
    assert_eq!(
        body(&r)["data"][0]["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["stored"] == true)
            .count(),
        1
    );
    let r = put_artifact(&f, &admin, "0.9.2", &tar_name, &tar).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert!(stored_file.is_file());
    let d = detail(&auditor).await;
    assert!(
        problems(&d).iter().all(|(_, p)| p.is_null()),
        "{:?}",
        problems(&d)
    );

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
    let r = call(addr, "POST", "/api/v1/rollouts", Some(&admin), Some(&json!({"name": "x", "target_version": "0.9.2", "selector": {"all": true}, "schedule": {"windows": [{"start": "25:00", "end": "01:00"}]}}))).await;
    assert_eq!(
        (r.status, body(&r)["error"]["details"]["parameter"].as_str()),
        (400, Some("schedule.windows[0]"))
    );
    let r = call(addr, "POST", "/api/v1/rollouts", Some(&admin), Some(&spec)).await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let id = body(&r)["data"]["id"].as_str().unwrap().to_string();
    // 雙人覆核的端點（0.10）：需要 rollouts:approve（只有管理員）與以帳號登入的工作階段——API token 不能核准、退回
    for action in ["approve", "reject"] {
        let path = format!("/api/v1/rollouts/{id}/{action}");
        let r = call(
            addr,
            "POST",
            &path,
            Some(&op),
            Some(&json!({"reason": "x"})),
        )
        .await;
        assert_eq!(
            (
                r.status,
                body(&r)["error"]["details"]["permission"].as_str()
            ),
            (403, Some("rollouts:approve")),
            "{action}"
        );
        let r = call(
            addr,
            "POST",
            &path,
            Some(&admin),
            Some(&json!({"reason": "x"})),
        )
        .await;
        assert_eq!(
            (r.status, body(&r)["error"]["code"].as_str()),
            (403, Some("session_required")),
            "{action}"
        );
        let r = call(addr, "GET", &path, Some(&admin), None).await;
        assert_eq!(r.status, 405, "{action}");
    }
    let r = call(
        addr,
        "GET",
        "/api/v1/rollouts?state=pending_approval",
        Some(&op),
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        body(&r)["meta"],
        json!({"paused": 0, "rollback_failed_unacknowledged": 0, "pending_approval": 0, "pending_approval_actionable": 0, "rollout_approval": "none", "rollout_approval_change": null})
    );
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
    // SSE（REMOTE.md 第 8.4 節）：`rollout.host` 只送給以 rollout_id 訂閱這個推送、且有 rollouts:read 的連線
    let ro = token(&f, Role::Viewer);
    let events = format!("/api/v1/events?rollout_id={id}");
    let mut watching = Sse::open_path(addr, &op, &events, None).await;
    let mut other =
        Sse::open_path(addr, &op, "/api/v1/events?rollout_id=r-20000101-0001", None).await;
    let mut plain = Sse::open(addr, &op, None).await;
    let mut no_perm = Sse::open_path(addr, &ro, &events, None).await;
    for s in [&mut watching, &mut other, &mut plain, &mut no_perm] {
        assert_eq!(s.status, 200);
        s.wait_for("ready").await;
    }
    let remote = f.remote.clone().expect("遠端管理");
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
    remote.flush_progress().await;
    let ev = watching.next().await.expect("rollout.host");
    assert_eq!(ev.event, "rollout.host", "主機的事件先於 rollout.progress");
    assert_eq!(
        (
            ev.data["rollout_id"].as_str(),
            ev.data["agent_id"].as_str(),
            ev.data["state"].as_str(),
            ev.data["wave"].as_i64()
        ),
        (Some(id.as_str()), Some(H), Some("pending"), Some(0))
    );
    assert_eq!(watching.next().await.unwrap().event, "rollout.progress");
    // 訂閱別的推送、沒有訂閱的連線只收到 rollout.progress；沒有 rollouts:read 的連線兩者都收不到
    for s in [&mut other, &mut plain] {
        let ev = s.next().await.unwrap();
        assert_eq!(ev.event, "rollout.progress");
        assert_eq!(ev.data["rollout_id"], id.as_str());
    }
    let quiet = tokio::time::timeout(std::time::Duration::from_millis(700), no_perm.next()).await;
    assert!(quiet.is_err(), "唯讀角色不應收到推送的事件");
    // 沒有改變的主機不重送；取消訂閱後不再逐台送出
    remote.mark_dirty(&id);
    remote.flush_progress().await;
    assert_eq!(watching.next().await.unwrap().event, "rollout.progress");
    drop(watching);
    drop(other);
    drop(plain);
    drop(no_perm);
    // 全域搜尋：清單的 q（名稱、編號、目標版本；不分大小寫），需要 rollouts:read
    for (q, n) in [
        ("WEB", 1),
        (id.as_str(), 1),
        ("0.9.2", 1),
        ("zzz", 0),
        ("", 1),
    ] {
        let r = call(
            addr,
            "GET",
            &format!("/api/v1/rollouts?q={q}"),
            Some(&auditor),
            None,
        )
        .await;
        assert_eq!(r.status, 200);
        assert_eq!(body(&r)["data"].as_array().unwrap().len(), n, "q={q}");
        assert_eq!(body(&r)["meta"]["paused"], 0, "meta 不受 q 影響");
    }
    let r = call(addr, "GET", "/api/v1/rollouts?q=web", Some(&ro), None).await;
    assert_eq!(r.status, 403);
    // S04 版本欄的 ⚠：建議的最低版本＝版本庫中最新版本的清單 min_upgrade_from（0.9.0）；只需要 hosts:read
    add_host(&f, "7e2f0a11-0000-4000-8000-0000000000c2", "0.8.0", "tar");
    f.console.ctx.hosts.invalidate().await;
    let r = call(
        addr,
        "GET",
        "/api/v1/hosts?sort=agent_version",
        Some(&ro),
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = body(&r);
    assert_eq!(v["meta"]["recommended_min_agent_version"], "0.9.0");
    let flags: Vec<(String, bool)> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["agent_version"].as_str().unwrap().to_string(),
                h["agent_outdated"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        flags,
        vec![("0.8.0".to_string(), true), ("0.9.0".to_string(), false)]
    );
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
        json!({"paused": 1, "rollback_failed_unacknowledged": 0, "pending_approval": 0, "pending_approval_actionable": 0, "rollout_approval": "none", "rollout_approval_change": null})
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
    // 版本庫的檔案遺失（例如還原備份之後）：警告、KRS-W8417；大小不符也算；補回後恢復
    let store = f.remote.as_ref().unwrap().releases_dir.clone();
    let stored_file = std::fs::read_dir(&store)
        .unwrap()
        .map(|e| e.unwrap().path())
        .next()
        .expect("版本庫有一個發佈檔");
    let content = std::fs::read(&stored_file).unwrap();
    std::fs::remove_file(&stored_file).unwrap();
    let r = call(addr, "GET", "/api/v1/system/health", Some(&auditor), None).await;
    let it = remote_item(&r);
    assert_eq!(
        (it["status"].as_str(), it["product_code"].as_str()),
        (Some("warning"), Some("KRS-W8417")),
        "{it}"
    );
    assert!(
        it["summary"]
            .as_str()
            .unwrap()
            .starts_with(&format!("版本庫的檔案遺失：0.9.2 的 {tar_name}")),
        "{it}"
    );
    assert_eq!(it["values"]["release_files_missing"], 1);
    assert_eq!(
        it["values"]["missing_files"][0],
        json!({"version": "0.9.2", "filename": tar_name, "problem": "missing", "rollback_target_for": 0})
    );
    std::fs::write(&stored_file, &content[..content.len() - 1]).unwrap();
    let r = call(addr, "GET", "/api/v1/system/health", Some(&auditor), None).await;
    let it = remote_item(&r);
    assert_eq!(it["values"]["missing_files"][0]["problem"], "size_mismatch");
    assert!(it["summary"].as_str().unwrap().contains("大小不符"), "{it}");
    // 重新上傳同一個檔案：取代大小不符的檔案
    let r = put_artifact(&f, &admin, "0.9.2", &tar_name, &tar).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(std::fs::read(&stored_file).unwrap(), content);
    let r = call(addr, "GET", "/api/v1/system/health", Some(&auditor), None).await;
    let it = remote_item(&r);
    assert_eq!(it["status"], "ok", "{it}");
    assert_eq!(it["values"]["release_files_missing"], 0);
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
    // 0.10.1（H3）：清單的進度由資料庫彙總——每一筆與詳情（從主機列計算）的數字相同，格式不變
    //（15 個狀態都有鍵）；state 篩選下也一樣
    let filtered = body(
        &call(
            addr,
            "GET",
            "/api/v1/rollouts?state=paused",
            Some(&auditor),
            None,
        )
        .await,
    );
    assert_eq!(filtered["data"], list["data"], "這時只有一個已暫停的推送");
    for item in list["data"].as_array().unwrap() {
        let rid = item["id"].as_str().unwrap();
        let detail = body(
            &call(
                addr,
                "GET",
                &format!("/api/v1/rollouts/{rid}"),
                Some(&auditor),
                None,
            )
            .await,
        )["data"]
            .clone();
        for key in [
            "progress",
            "hosts_total",
            "rollback_failed_unacknowledged",
            "next_action",
            "state",
            "approval",
        ] {
            assert_eq!(item[key], detail[key], "{rid} {key}");
        }
        assert_eq!(item["progress"].as_object().unwrap().len(), 15, "{rid}");
        let total: i64 = item["progress"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.as_str() != "skipped")
            .map(|(_, v)| v.as_i64().unwrap())
            .sum();
        assert_eq!(item["hosts_total"].as_i64(), Some(total), "{rid}");
        assert!(item.get("waves").is_none(), "清單沒有詳情的欄位");
    }
    assert_eq!(list["data"][0]["progress"]["rollback_failed"], 1);
    // 計數的輕量端點（SideNav 的泡泡）：與清單的 meta 相同，另有各狀態的推送數（七個狀態都有鍵）
    for who in [&auditor, &op, &admin] {
        let r = call(addr, "GET", "/api/v1/rollouts/summary", Some(who), None).await;
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        let s = body(&r)["data"].clone();
        for (k, v) in list["meta"].as_object().unwrap() {
            assert_eq!(&s[k], v, "{k}");
        }
        let states = s["states"].as_object().unwrap();
        assert_eq!(states.len(), 7);
        assert_eq!(states["paused"], 1);
        assert_eq!(
            states.values().map(|v| v.as_i64().unwrap()).sum::<i64>(),
            list["data"].as_array().unwrap().len() as i64
        );
        assert_eq!(s["paused"], 1);
    }
    let r = call(
        addr,
        "GET",
        "/api/v1/rollouts/summary?state=paused",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(r.status, 400, "沒有查詢參數");
    let r = call(
        addr,
        "POST",
        "/api/v1/rollouts/summary",
        Some(&admin),
        Some(&json!({})),
    )
    .await;
    assert_eq!(r.status, 405);
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

    // 外部 URL 的版本庫（0.10，REMOTE.md 第 4.1.2 節）：source: external 需要合法的 base_url；簽章驗證照舊；
    // 上傳發佈檔只登記雜湊與大小（維運者不能）
    let ext = release(&signer, "0.9.4", "4444444");
    let ext_body = |base: Option<&str>, sig: &str| {
        let mut b = json!({"sha256sums": ext.sha256sums, "signature": sig, "manifest": ext.manifest, "source": "external"});
        if let Some(u) = base {
            b["base_url"] = json!(u);
        }
        b
    };
    for (base, param) in [
        (None, "base_url"),
        (Some("http://repo.example.gov.tw/p/"), "base_url"),
        (Some("https://repo.example.gov.tw/p/?sig=1"), "base_url"),
    ] {
        let r = call(
            addr,
            "POST",
            "/api/v1/releases",
            Some(&admin),
            Some(&ext_body(base, &ext.signature)),
        )
        .await;
        assert_eq!(
            (r.status, body(&r)["error"]["details"]["parameter"].as_str()),
            (400, Some(param)),
            "{base:?}"
        );
    }
    let r = call(addr, "POST", "/api/v1/releases", Some(&admin), Some(&json!({"sha256sums": ext.sha256sums, "signature": ext.signature, "base_url": "https://repo.example.gov.tw/p/"}))).await;
    assert_eq!(r.status, 400, "base_url 只用於 source: external");
    let forged = TestSigner::new(12).sign(ext.sha256sums.as_bytes());
    let r = call(
        addr,
        "POST",
        "/api/v1/releases",
        Some(&admin),
        Some(&ext_body(Some("https://repo.example.gov.tw/p/"), &forged)),
    )
    .await;
    assert_eq!(
        (r.status, body(&r)["error"]["code"].as_str()),
        (400, Some("release_signature_invalid")),
        "外部版本的簽章同樣要驗證"
    );
    let r = call(
        addr,
        "POST",
        "/api/v1/releases",
        Some(&op),
        Some(&ext_body(
            Some("https://repo.example.gov.tw/p/"),
            &ext.signature,
        )),
    )
    .await;
    assert_eq!(r.status, 403);
    let r = call(
        addr,
        "POST",
        "/api/v1/releases",
        Some(&admin),
        Some(&ext_body(
            Some("https://Repo.example.gov.tw/p"),
            &ext.signature,
        )),
    )
    .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        (
            body(&r)["data"]["source"].as_str(),
            body(&r)["data"]["base_url"].as_str(),
            body(&r)["data"]["verified"].as_bool()
        ),
        (
            Some("external"),
            Some("https://repo.example.gov.tw/p/"),
            Some(true)
        )
    );
    let (ext_name, ext_data) = ext.files[0].clone();
    let r = put_artifact(&f, &admin, "0.9.4", &ext_name, b"something else").await;
    assert_eq!(
        (r.status, body(&r)["error"]["code"].as_str()),
        (400, Some("artifact_hash_mismatch"))
    );
    let r = put_artifact(&f, &admin, "0.9.4", &ext_name, &ext_data).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        (
            body(&r)["data"]["stored"].as_bool(),
            body(&r)["data"]["registered"].as_bool()
        ),
        (Some(false), Some(true))
    );
    let _ = std::fs::remove_dir_all(trust.parent().unwrap());
    f.stop().await;
}
