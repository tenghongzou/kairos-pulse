//! 使用者、工作階段、RBAC、稽核的整合測試：實際的 HTTPS 伺服器（[`super::tests::fixture`]）與 SQLite。
//!
//! 每個「拒絕」都有對應的測試：權限不足（403）、CSRF、工作階段結束或逾時（401）、鎖定、最後一位管理員、
//! 群組範圍外的主機（404）；以及升級前的 API token 保留原有權限。

use super::tests::{Fixture, Resp, fixture, fixture_with, send_raw};
use super::*;
use crate::rbac::Assignment;
use crate::store::prelude::*;
use crate::users::ldap::{DirError, Directory, DirectoryUser};
use crate::users::password::{self, HashParams};
use crate::users::store as us;
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;

/// 測試用的低成本 Argon2 參數（與設定相同，登入時不會重新雜湊）
const CHEAP: HashParams = HashParams {
    memory_kib: 8192,
    iterations: 1,
    parallelism: 1,
};

fn cheap(c: &mut AppConfig) {
    c.accounts.argon2_memory_kib = CHEAP.memory_kib;
    c.accounts.argon2_iterations = CHEAP.iterations;
}

const PW: &str = "correct horse battery";

fn add_user(f: &Fixture, name: &str, roles: &[&str], must_change: bool) -> i64 {
    let conn = f.admin();
    let h = password::hash(PW, CHEAP).unwrap();
    let id = conn
        .create_user(
            &us::NewUser {
                username: name,
                source: us::SOURCE_LOCAL,
                password_hash: Some(&h),
                must_change_password: must_change,
                ..us::NewUser::default()
            },
            "test",
            Utc::now().timestamp_millis(),
        )
        .unwrap()
        .unwrap();
    for r in roles {
        conn.add_assignment(
            us::SUBJECT_USER,
            &id.to_string(),
            &Assignment::parse(r).unwrap(),
            "test",
            0,
        )
        .unwrap();
    }
    id
}

#[derive(Debug, Clone)]
struct Session {
    cookie: String,
    csrf: String,
}

enum Auth<'a> {
    None,
    Token(&'a str),
    /// 工作階段＋CSRF token
    Session(&'a Session),
    /// 工作階段，但不帶 CSRF token
    CookieOnly(&'a Session),
}

async fn call(
    f: &Fixture,
    method: &str,
    path: &str,
    auth: Auth<'_>,
    body: Option<Value>,
    extra: &[(&str, &str)],
) -> Resp {
    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: localhost\r\n");
    match auth {
        Auth::None => {}
        Auth::Token(t) => req.push_str(&format!("Authorization: Bearer {t}\r\n")),
        Auth::Session(s) => {
            req.push_str(&format!(
                "Cookie: {}\r\nX-CSRF-Token: {}\r\n",
                s.cookie, s.csrf
            ));
        }
        Auth::CookieOnly(s) => req.push_str(&format!("Cookie: {}\r\n", s.cookie)),
    }
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    if method != "GET" && method != "HEAD" {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    send_raw(f.addr(), req.as_bytes()).await
}

async fn try_login(f: &Fixture, user: &str, pw: &str) -> Resp {
    call(
        f,
        "POST",
        "/api/v1/auth/login",
        Auth::None,
        Some(json!({"username": user, "password": pw})),
        &[],
    )
    .await
}

fn session_of(r: &Resp) -> Session {
    let set = r.header("set-cookie").expect("應有 Set-Cookie");
    let cookie = set.split(';').next().unwrap().trim().to_string();
    Session {
        cookie,
        csrf: r.json()["data"]["csrf_token"].as_str().unwrap().to_string(),
    }
}

async fn login(f: &Fixture, user: &str) -> Session {
    let r = try_login(f, user, PW).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    session_of(&r)
}

fn audit_actions(f: &Fixture) -> Vec<(String, String, Option<String>, String)> {
    let conn = f.admin();
    let mut stmt = conn
        .prepare("SELECT action, actor, object, result FROM audit_events ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[tokio::test]
async fn login_session_cookie_csrf_and_logout() {
    let f = fixture("auth-login", cheap).await;
    add_user(&f, "admin", &["admin"], false);
    // 方式（不需驗證）
    let r = call(&f, "GET", "/api/v1/auth/methods", Auth::None, None, &[]).await;
    assert_eq!(r.json()["data"]["ldap"], false);

    let r = try_login(&f, "admin", PW).await;
    assert_eq!(r.status, 200);
    let set = r.header("set-cookie").unwrap().to_string();
    for attr in [
        "__Host-kairos_session=",
        "HttpOnly",
        "Secure",
        "SameSite=Strict",
        "Path=/",
    ] {
        assert!(set.contains(attr), "{set}");
    }
    let v = r.json();
    assert_eq!(v["data"]["mfa_required"], false);
    assert_eq!(v["data"]["user"]["username"], "admin");
    // 回應中沒有密碼或雜湊
    assert!(!String::from_utf8_lossy(&r.body).contains("argon2"));
    assert!(!String::from_utf8_lossy(&r.body).contains(PW));
    let s = session_of(&r);
    // 資料庫只有工作階段秘密的雜湊
    let secret = s.cookie.split_once('=').unwrap().1.to_string();
    let conn = f.admin();
    let raw: Vec<u8> = conn
        .query_row("SELECT session_hash FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(raw, crate::users::sha256(secret.as_bytes()).to_vec());
    let dump: i64 = conn
        .query_row(
            "SELECT count(*) FROM sessions WHERE instr(CAST(session_hash AS TEXT), ?1) > 0 OR public_id = ?1",
            [&secret],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dump, 0);

    let me = call(&f, "GET", "/api/v1/me", Auth::CookieOnly(&s), None, &[]).await;
    assert_eq!(me.status, 200);
    let v = me.json();
    assert_eq!(v["data"]["type"], "session");
    assert_eq!(v["data"]["user"]["username"], "admin");
    assert_eq!(v["data"]["csrf_token"], s.csrf.as_str());
    assert!(
        v["data"]["permissions"]
            .as_array()
            .unwrap()
            .contains(&json!("users:write"))
    );
    // 讀取不需要 CSRF；寫入需要
    let r = call(
        &f,
        "POST",
        "/api/v1/me/sessions/revoke-others",
        Auth::CookieOnly(&s),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(
        (r.status, r.json()["error"]["code"].clone()),
        (403, json!("csrf_failed"))
    );
    assert_eq!(r.json()["error"]["product_code"], "KRS-E6105");
    let wrong = Session {
        csrf: "x".repeat(43),
        ..s.clone()
    };
    let r = call(
        &f,
        "POST",
        "/api/v1/me/sessions/revoke-others",
        Auth::Session(&wrong),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(r.status, 403);
    // 其他網站送出的請求：Origin 不符
    let r = call(
        &f,
        "POST",
        "/api/v1/me/sessions/revoke-others",
        Auth::Session(&s),
        Some(json!({})),
        &[("Origin", "https://evil.example")],
    )
    .await;
    assert_eq!(r.status, 403);
    let r = call(
        &f,
        "POST",
        "/api/v1/me/sessions/revoke-others",
        Auth::Session(&s),
        Some(json!({})),
        &[("Origin", "https://localhost")],
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    // 偽造的 cookie
    let fake = Session {
        cookie: format!("__Host-kairos_session={}", "A".repeat(43)),
        csrf: String::new(),
    };
    let r = call(&f, "GET", "/api/v1/me", Auth::CookieOnly(&fake), None, &[]).await;
    assert_eq!(r.status, 401);
    // 登出：cookie 清除，之後 401
    let r = call(
        &f,
        "POST",
        "/api/v1/auth/logout",
        Auth::Session(&s),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.header("set-cookie").unwrap().contains("Max-Age=0"));
    let r = call(&f, "GET", "/api/v1/me", Auth::CookieOnly(&s), None, &[]).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["error"]["details"]["reason"], "session_expired");
    let actions: Vec<String> = audit_actions(&f).into_iter().map(|a| a.0).collect();
    assert!(
        actions.contains(&"login.success".to_string()),
        "{actions:?}"
    );
    assert!(actions.contains(&"logout".to_string()), "{actions:?}");
    f.stop().await;
}

#[tokio::test]
async fn failed_logins_lock_without_revealing_whether_the_account_exists() {
    let f = fixture("auth-lock", |c| {
        cheap(c);
        c.accounts.lockout_threshold = 3;
    })
    .await;
    add_user(&f, "alice", &["viewer"], false);
    for i in 0..3 {
        let a = try_login(&f, "alice", "wrong password!").await;
        let g = try_login(&f, "ghost", "wrong password!").await;
        if i < 2 {
            assert_eq!(a.status, 401);
            assert_eq!(a.json()["error"]["product_code"], "KRS-E6001");
            // 不存在的帳號：同樣的回應
            assert_eq!(g.status, a.status);
            assert_eq!(g.json()["error"]["code"], a.json()["error"]["code"]);
        } else {
            // 第 3 次：鎖定（不存在的帳號也一樣）
            for r in [&a, &g] {
                assert_eq!(r.status, 403, "{}", String::from_utf8_lossy(&r.body));
                assert_eq!(r.json()["error"]["product_code"], "KRS-E6003");
                assert!(r.header("retry-after").is_some());
            }
        }
    }
    // 鎖定期間，正確的密碼也不行
    let r = try_login(&f, "alice", PW).await;
    assert_eq!(r.json()["error"]["code"], "account_locked");
    // 管理員解除後可以登入
    let conn = f.admin();
    let id = conn.find_user("alice").unwrap().unwrap().user_id;
    conn.unlock(id).unwrap();
    assert_eq!(try_login(&f, "alice", PW).await.status, 200);
    // 同一來源失敗太多次（額度 10 次）：來源被限速，正確的密碼也一樣。
    // 額度以實際時間補充（每 6 秒 1 次）：慢的機器上（CI 的 Windows runner 跑到這裡已超過 12 秒）
    // 前面扣掉的額度會補回來，所以持續失敗到被限速為止，不假設剛好再 4 次；
    // 「剛好 10 次」由注入時間的 failure_limiter_blocks_after_the_burst_and_recovers 驗證
    let mut limited = false;
    for i in 0..40 {
        if try_login(&f, &format!("nobody-{i}"), "x").await.status == 429 {
            limited = true;
            break;
        }
    }
    assert!(limited, "失敗 40 次後仍未被限速");
    let r = try_login(&f, "alice", PW).await;
    assert_eq!(r.status, 429, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["error"]["product_code"], "KRS-W6011");
    // 稽核：失敗與鎖定都有紀錄，但沒有密碼
    let all = audit_actions(&f);
    assert!(all.iter().filter(|a| a.0 == "login.failure").count() >= 9);
    assert!(
        all.iter()
            .any(|a| a.0 == "account.lock" && a.2.as_deref() == Some("ghost"))
    );
    let raw: String = conn
        .query_row(
            "SELECT group_concat(coalesce(details,''), ' ') FROM audit_events",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!raw.contains("wrong password") && !raw.contains(PW));
    f.stop().await;
}

#[tokio::test]
async fn first_login_must_change_password() {
    let f = fixture("auth-mustchange", cheap).await;
    add_user(&f, "bob", &["viewer"], true);
    let s = login(&f, "bob").await;
    let r = call(&f, "GET", "/api/v1/hosts", Auth::Session(&s), None, &[]).await;
    assert_eq!(r.status, 403);
    assert_eq!(r.json()["error"]["code"], "password_change_required");
    assert_eq!(r.json()["error"]["product_code"], "KRS-E6013");
    assert_eq!(
        call(&f, "GET", "/api/v1/me", Auth::Session(&s), None, &[])
            .await
            .status,
        200
    );
    // 政策：太短、與目前的相同
    let r = call(
        &f,
        "POST",
        "/api/v1/me/password",
        Auth::Session(&s),
        Some(json!({"current_password": PW, "new_password": "short"})),
        &[],
    )
    .await;
    assert_eq!(r.status, 400);
    // 目前的密碼錯誤：403
    let r = call(
        &f,
        "POST",
        "/api/v1/me/password",
        Auth::Session(&s),
        Some(
            json!({"current_password": "nope nope nope", "new_password": "a brand new passphrase"}),
        ),
        &[],
    )
    .await;
    assert_eq!(r.status, 403);
    let r = call(
        &f,
        "POST",
        "/api/v1/me/password",
        Auth::Session(&s),
        Some(json!({"current_password": PW, "new_password": "a brand new passphrase"})),
        &[],
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        call(&f, "GET", "/api/v1/hosts", Auth::Session(&s), None, &[])
            .await
            .status,
        200
    );
    // 舊密碼不能再登入
    assert_eq!(try_login(&f, "bob", PW).await.status, 401);
    f.stop().await;
}

/// 每個角色對一組端點：允許的不是 403，拒絕的是 403
/// 系統健康：整體狀態與各項狀態對所有人相同，併入的項目（/metrics、告警引擎…）細節依權限省略（§29.6）
#[tokio::test]
async fn health_details_follow_permissions() {
    let f = fixture("auth-health", cheap).await;
    add_user(&f, "ad", &["admin"], false);
    add_user(&f, "vi", &["viewer"], false);
    let ad = login(&f, "ad").await;
    let vi = login(&f, "vi").await;
    let get = |s: Session| {
        let f = &f;
        async move {
            call(
                f,
                "GET",
                "/api/v1/system/health",
                Auth::Session(&s),
                None,
                &[],
            )
            .await
            .json()
        }
    };
    let a = get(ad.clone()).await;
    let v = get(vi.clone()).await;
    assert_eq!(a["data"]["status"], v["data"]["status"]);
    let ids = |x: &Value| {
        x["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| (i["id"].clone(), i["status"].clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&a), ids(&v));
    let item = |x: &Value, id: &str| {
        x["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .unwrap()
            .clone()
    };
    // 唯讀有 alerts:read（告警引擎看得到），沒有 settings:read（/metrics 端點只看狀態）
    assert!(item(&v, "alerts").get("restricted").is_none());
    let id = "metrics";
    assert_eq!(item(&v, id)["restricted"], true);
    assert_eq!(item(&v, id)["summary"], "");
    assert!(item(&v, id)["values"].is_null());
    assert!(item(&a, id).get("restricted").is_none());
    assert_ne!(item(&a, id)["summary"], "");
    // 原本的項目不受影響（唯讀的細節由主控台決定是否顯示）
    assert!(item(&v, "database").get("restricted").is_none());
    // 總覽的健康摘要同樣省略
    let o = call(&f, "GET", "/api/v1/overview", Auth::Session(&vi), None, &[])
        .await
        .json();
    assert_eq!(o["data"]["health"]["status"], a["data"]["status"]);
    f.stop().await;
}

#[tokio::test]
async fn every_role_is_denied_what_it_lacks() {
    let f = fixture("auth-rbac", cheap).await;
    for (u, r) in [
        ("ad", "admin"),
        ("op", "operator"),
        ("au", "auditor"),
        ("vi", "viewer"),
    ] {
        add_user(&f, u, &[r], false);
    }
    add_user(&f, "nobody", &[], false);
    add_user(&f, "spare", &["viewer"], false);
    let sessions = [
        ("ad", login(&f, "ad").await),
        ("op", login(&f, "op").await),
        ("au", login(&f, "au").await),
        ("vi", login(&f, "vi").await),
        ("nobody", login(&f, "nobody").await),
    ];
    // (方法, 路徑, 本文, 允許的帳號)
    let cases: Vec<(&str, &str, Option<Value>, &[&str])> = vec![
        ("GET", "/api/v1/hosts", None, &["ad", "op", "au", "vi"]),
        ("GET", "/api/v1/overview", None, &["ad", "op", "au", "vi"]),
        (
            "GET",
            "/api/v1/system/health",
            None,
            &["ad", "op", "au", "vi"],
        ),
        ("GET", "/api/v1/enroll/tokens", None, &["ad", "op", "au"]),
        ("GET", "/api/v1/pki/certificates", None, &["ad", "op", "au"]),
        ("GET", "/api/v1/settings", None, &["ad", "op", "au"]),
        ("GET", "/api/v1/users", None, &["ad", "au"]),
        ("GET", "/api/v1/roles", None, &["ad", "au"]),
        ("GET", "/api/v1/directory", None, &["ad", "au"]),
        ("GET", "/api/v1/api-tokens", None, &["ad", "au"]),
        ("GET", "/api/v1/audit", None, &["ad", "op", "au"]),
        ("GET", "/api/v1/audit/export", None, &["ad", "au"]),
        (
            "POST",
            "/api/v1/audit/verify",
            Some(json!({})),
            &["ad", "au"],
        ),
        (
            "POST",
            "/api/v1/users",
            Some(json!({"username": "new1", "roles": ["viewer"]})),
            &["ad"],
        ),
        (
            "PUT",
            "/api/v1/users/vi/roles",
            Some(json!({"roles": ["viewer"]})),
            &["ad"],
        ),
        (
            "POST",
            "/api/v1/users/spare/sessions/revoke-all",
            Some(json!({})),
            &["ad"],
        ),
        (
            "POST",
            "/api/v1/api-tokens",
            Some(json!({"name": "x", "role": "viewer"})),
            &["ad"],
        ),
        (
            "PUT",
            "/api/v1/directory/mappings",
            Some(json!({"mappings": []})),
            &["ad"],
        ),
        (
            "POST",
            "/api/v1/enroll/requests/approve",
            Some(json!({"request_ids": ["x"]})),
            &["ad", "op"],
        ),
        (
            "PUT",
            "/api/v1/enroll/mode",
            Some(json!({"mode": "open"})),
            &["ad"],
        ),
        ("PUT", "/api/v1/pki/policy", Some(json!({})), &["ad"]),
        // 影響數字的查詢（0.8）：讀取權限同所在區段
        (
            "GET",
            "/api/v1/pki/policy/impact?expired_renew_grace_days=30",
            None,
            &["ad", "op", "au"],
        ),
        (
            "GET",
            "/api/v1/settings/reset/impact?keys=enroll.mode",
            None,
            &["ad", "op", "au"],
        ),
        (
            "POST",
            "/api/v1/pki/certificates/revoke",
            Some(json!({})),
            &["ad", "op"],
        ),
        // 告警（設計包 ia.md §4.2 S06～S11；告警服務沒有啟動，允許的請求為 503）
        ("GET", "/api/v1/alerts", None, &["ad", "op", "au", "vi"]),
        (
            "GET",
            "/api/v1/alerts/summary",
            None,
            &["ad", "op", "au", "vi"],
        ),
        ("GET", "/api/v1/alerts/E-1", None, &["ad", "op", "au", "vi"]),
        (
            "POST",
            "/api/v1/alerts/E-1/ack",
            Some(json!({})),
            &["ad", "op"],
        ),
        (
            "POST",
            "/api/v1/alerts/ack",
            Some(json!({"event_ids": ["E-1"]})),
            &["ad", "op"],
        ),
        (
            "POST",
            "/api/v1/alerts/E-1/resolve",
            Some(json!({"root_cause": "other"})),
            &["ad", "op"],
        ),
        (
            "POST",
            "/api/v1/alerts/E-1/comments",
            Some(json!({"text": "x"})),
            &["ad", "op"],
        ),
        ("GET", "/api/v1/alert-rules", None, &["ad", "op", "au"]),
        (
            "POST",
            "/api/v1/alert-rules/preview",
            Some(json!({"rule_id": "sys.host_offline"})),
            &["ad", "op", "au"],
        ),
        (
            "POST",
            "/api/v1/alert-rules",
            Some(json!({})),
            &["ad", "op"],
        ),
        (
            "POST",
            "/api/v1/alert-rules/sys.host_offline/disable",
            Some(json!({"reason": "x"})),
            &["ad", "op"],
        ),
        ("GET", "/api/v1/silences", None, &["ad", "op", "au", "vi"]),
        ("POST", "/api/v1/silences", Some(json!({})), &["ad", "op"]),
        (
            "POST",
            "/api/v1/silences/S-1/end",
            Some(json!({})),
            &["ad", "op"],
        ),
        ("GET", "/api/v1/channels", None, &["ad", "op"]),
        ("GET", "/api/v1/channels/c-1", None, &["ad", "op"]),
        (
            "POST",
            "/api/v1/channels/c-1/test",
            Some(json!({})),
            &["ad", "op"],
        ),
        ("POST", "/api/v1/channels", Some(json!({})), &["ad"]),
        ("POST", "/api/v1/channels/test", Some(json!({})), &["ad"]),
        ("PUT", "/api/v1/channels/c-1", Some(json!({})), &["ad"]),
        ("DELETE", "/api/v1/channels/c-1", Some(json!({})), &["ad"]),
        // 遠端管理（REMOTE.md 第 5.2 節；允許的請求為 400／404／415，不改變狀態）
        ("GET", "/api/v1/config/schema", None, &["ad", "op", "au"]),
        ("GET", "/api/v1/config/profiles", None, &["ad", "op", "au"]),
        ("GET", "/api/v1/config/status", None, &["ad", "op", "au"]),
        (
            "POST",
            "/api/v1/config/preview",
            Some(json!({})),
            &["ad", "op", "au"],
        ),
        (
            "POST",
            "/api/v1/config/profiles",
            Some(json!({})),
            &["ad", "op"],
        ),
        (
            "DELETE",
            "/api/v1/config/assignments/999",
            Some(json!({})),
            &["ad", "op"],
        ),
        (
            "GET",
            "/api/v1/hosts/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee/config",
            None,
            &["ad", "op", "au"],
        ),
        ("GET", "/api/v1/releases", None, &["ad", "op", "au"]),
        ("POST", "/api/v1/releases", Some(json!({})), &["ad"]),
        ("DELETE", "/api/v1/releases/0.9.9", Some(json!({})), &["ad"]),
        (
            "PUT",
            "/api/v1/releases/0.9.9/artifacts/x",
            Some(json!({})),
            &["ad"],
        ),
        ("GET", "/api/v1/rollouts", None, &["ad", "op", "au"]),
        (
            "POST",
            "/api/v1/rollouts/preview",
            Some(json!({})),
            &["ad", "op", "au"],
        ),
        ("POST", "/api/v1/rollouts", Some(json!({})), &["ad"]),
        (
            "POST",
            "/api/v1/rollouts/r-x/pause",
            Some(json!({"reason": "x"})),
            &["ad", "op"],
        ),
        (
            "POST",
            "/api/v1/rollouts/r-x/submit",
            Some(json!({})),
            &["ad"],
        ),
        (
            "POST",
            "/api/v1/rollouts/r-x/hosts/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee/retry",
            Some(json!({})),
            &["ad"],
        ),
        (
            "GET",
            "/api/v1/hosts/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee/upgrade",
            None,
            &["ad", "op", "au"],
        ),
    ];
    for (method, path, body, allowed) in &cases {
        for (name, s) in &sessions {
            let r = call(&f, method, path, Auth::Session(s), body.clone(), &[]).await;
            let denied = r.status == 403;
            assert_eq!(
                denied,
                !allowed.contains(name),
                "{name} {method} {path} → {} {}",
                r.status,
                String::from_utf8_lossy(&r.body)
            );
            if denied {
                let v = r.json();
                assert_eq!(v["error"]["code"], "forbidden", "{v}");
                assert_eq!(v["error"]["product_code"], "KRS-E6104");
                assert!(v["error"]["details"]["permission"].is_string(), "{v}");
            }
        }
    }
    // 被拒絕的寫入請求寫入稽核
    let denied = audit_actions(&f)
        .into_iter()
        .filter(|a| a.0 == "access.denied" && a.3 == "denied")
        .count();
    assert!(denied >= 10, "{denied}");
    // 告警的寫入也需要 CSRF token 與同源（工作階段）
    let (_, op) = &sessions[1];
    for (extra, why) in [
        (vec![], "沒有 CSRF token"),
        (vec![("Sec-Fetch-Site", "cross-site")], "跨站"),
    ] {
        let auth = if extra.is_empty() {
            Auth::CookieOnly(op)
        } else {
            Auth::Session(op)
        };
        for (method, path) in [
            ("POST", "/api/v1/alerts/E-1/ack"),
            ("POST", "/api/v1/silences"),
            ("POST", "/api/v1/channels/c-1/test"),
        ] {
            let r = call(&f, method, path, auth_clone(&auth), Some(json!({})), &extra).await;
            assert_eq!(r.status, 403, "{why} {method} {path}");
            assert_eq!(r.json()["error"]["code"], "csrf_failed", "{why} {path}");
        }
    }
    f.stop().await;
}

fn auth_clone<'a>(a: &Auth<'a>) -> Auth<'a> {
    match a {
        Auth::None => Auth::None,
        Auth::Token(t) => Auth::Token(t),
        Auth::Session(s) => Auth::Session(s),
        Auth::CookieOnly(s) => Auth::CookieOnly(s),
    }
}

#[tokio::test]
async fn legacy_tokens_keep_their_permissions_after_the_v6_upgrade() {
    // 以 v4 的 schema 建立資料庫與 0.5 的三種 token（直接寫入，模擬升級前的資料）
    let dir = super::tests::temp_dir("auth-upgrade");
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let path = data.join(crate::store::sqlite::DB_FILE);
    let mut tokens = Vec::new();
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        let v4: Vec<_> = crate::store::sqlite::MIGRATIONS
            .iter()
            .copied()
            .filter(|m| m.version <= 4)
            .collect();
        crate::store::sqlite::migrate(&mut conn, &v4).unwrap();
        for scope in ["read", "operate", "admin"] {
            let (id, secret) = tokens::generate().unwrap();
            conn.execute(
                "INSERT INTO api_tokens (token_id, name, secret_hash, scope, created_at, created_by, expires_at)
                 VALUES (?1, ?2, ?3, ?4, 0, 'ops', NULL)",
                rusqlite::params![id, scope, &tokens::hash_secret(&secret)[..], scope],
            )
            .unwrap();
            tokens.push((scope, format!("kat_{id}_{secret}")));
        }
        // 0.5 的稽核紀錄（沒有雜湊）
        conn.execute(
            "INSERT INTO audit_events (at, actor, category, action, result) VALUES (1, 'system', 'pki', 'cert.issue', 'ok')",
            [],
        )
        .unwrap();
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 4);
    }
    let d = data.to_string_lossy().into_owned();
    let f = fixture("auth-upgrade-run", move |c| {
        cheap(c);
        c.storage.data_dir = d;
    })
    .await;
    let conn = f.admin();
    let v: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, crate::store::sqlite::SCHEMA_VERSION);
    // v5（告警）與 v6（使用者）的資料表都已建立
    for t in ["alert_events", "alert_channels", "users", "sessions"] {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [t],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "{t}");
    }
    for (scope, token) in &tokens {
        // 所有讀取（升級前的 read）
        for path in [
            "/api/v1/hosts",
            "/api/v1/overview",
            "/api/v1/system/health",
            "/api/v1/me",
        ] {
            let r = call(&f, "GET", path, Auth::Token(token), None, &[]).await;
            assert_eq!(
                r.status,
                200,
                "{scope} {path} {}",
                String::from_utf8_lossy(&r.body)
            );
        }
        let me = call(&f, "GET", "/api/v1/me", Auth::Token(token), None, &[])
            .await
            .json();
        assert_eq!(me["data"]["scopes"][0], *scope);
        // 註冊、PKI、設定的讀取：權限通過（沒有註冊服務時是 503，不是 403）
        for path in [
            "/api/v1/enroll/tokens",
            "/api/v1/pki/certificates",
            "/api/v1/settings",
        ] {
            let r = call(&f, "GET", path, Auth::Token(token), None, &[]).await;
            assert_eq!(r.status, 503, "{scope} {path}");
        }
        // 核准：operate 以上（0.5 的 403 細節：required = operate）
        let r = call(
            &f,
            "POST",
            "/api/v1/enroll/requests/approve",
            Auth::Token(token),
            Some(json!({"request_ids": ["x"]})),
            &[],
        )
        .await;
        if *scope == "read" {
            assert_eq!(r.status, 403);
            assert_eq!(r.json()["error"]["details"]["required"], "operate");
        } else {
            assert_eq!(r.status, 503, "{scope}");
        }
        // 模式：只有 admin
        let r = call(
            &f,
            "PUT",
            "/api/v1/enroll/mode",
            Auth::Token(token),
            Some(json!({"mode": "open"})),
            &[],
        )
        .await;
        assert_eq!(r.status == 403, *scope != "admin", "{scope}");
        // 升級後新增的權限不會給舊 token（使用者、稽核、API token 管理、告警；v4 → v5 → v6）
        for path in [
            "/api/v1/users",
            "/api/v1/audit",
            "/api/v1/api-tokens",
            "/api/v1/alerts",
            "/api/v1/silences",
            "/api/v1/alert-rules",
            "/api/v1/channels",
        ] {
            let r = call(&f, "GET", path, Auth::Token(token), None, &[]).await;
            assert_eq!(r.status, 403, "{scope} {path}");
        }
    }
    // 升級前的稽核紀錄被補上雜湊，鏈完整
    let r = conn.audit_verify().unwrap();
    assert!(
        r.ok()
            || r.broken
                .as_ref()
                .is_some_and(|b| b.reason == "missing_hash"),
        "{r:?}"
    );
    conn.audit_seal_legacy().unwrap();
    assert!(conn.audit_verify().unwrap().ok());
    f.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn ending_sessions_d25() {
    let f = fixture("auth-d25", cheap).await;
    add_user(&f, "root", &["admin"], false);
    add_user(&f, "ann", &["operator"], false);
    let root = login(&f, "root").await;
    let a1 = login(&f, "ann").await;
    let a2 = login(&f, "ann").await;
    let a3 = login(&f, "ann").await;
    // 自己：結束其他所有工作階段、保留目前這個
    let r = call(
        &f,
        "POST",
        "/api/v1/me/sessions/revoke-others",
        Auth::Session(&a1),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(r.json()["data"]["ended"], 2);
    assert_eq!(
        call(&f, "GET", "/api/v1/me", Auth::Session(&a1), None, &[])
            .await
            .status,
        200
    );
    assert_eq!(
        call(&f, "GET", "/api/v1/me", Auth::Session(&a2), None, &[])
            .await
            .status,
        401
    );
    assert_eq!(
        call(&f, "GET", "/api/v1/me", Auth::Session(&a3), None, &[])
            .await
            .status,
        401
    );
    // 清單：只有目前這個
    let r = call(
        &f,
        "GET",
        "/api/v1/me/sessions",
        Auth::Session(&a1),
        None,
        &[],
    )
    .await;
    let list = r.json()["data"]["sessions"].as_array().unwrap().clone();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["current"], true);
    // API token 不能使用個人的工作階段端點
    let r = call(
        &f,
        "POST",
        "/api/v1/me/sessions/revoke-others",
        Auth::Token(&f.token),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(r.json()["error"]["code"], "not_a_session");
    // 管理員對他人：結束全部
    let a4 = login(&f, "ann").await;
    let r = call(
        &f,
        "POST",
        "/api/v1/users/ann/sessions/revoke-all",
        Auth::Session(&root),
        Some(json!({"reason": "遺失筆電"})),
        &[],
    )
    .await;
    assert_eq!(
        r.json()["data"]["ended"],
        2,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    for s in [&a1, &a4] {
        assert_eq!(
            call(&f, "GET", "/api/v1/me", Auth::Session(s), None, &[])
                .await
                .status,
            401
        );
    }
    // 對象是自己：拒絕並指向 revoke-others
    let r = call(
        &f,
        "POST",
        "/api/v1/users/root/sessions/revoke-all",
        Auth::Session(&root),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(
        (r.status, r.json()["error"]["code"].clone()),
        (409, json!("target_is_self"))
    );
    assert_eq!(
        call(&f, "GET", "/api/v1/me", Auth::Session(&root), None, &[])
            .await
            .status,
        200
    );
    // 停用帳號：工作階段立即結束、不能登入
    let a5 = login(&f, "ann").await;
    let r = call(
        &f,
        "POST",
        "/api/v1/users/ann/disable",
        Auth::Session(&root),
        Some(json!({"reason": "離職"})),
        &[],
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        call(&f, "GET", "/api/v1/me", Auth::Session(&a5), None, &[])
            .await
            .status,
        401
    );
    assert_eq!(try_login(&f, "ann", PW).await.status, 401);
    // 重設密碼：工作階段結束、拿到一次性密碼
    call(
        &f,
        "POST",
        "/api/v1/users/ann/enable",
        Auth::Session(&root),
        Some(json!({})),
        &[],
    )
    .await;
    let a6 = login(&f, "ann").await;
    let r = call(
        &f,
        "POST",
        "/api/v1/users/ann/password",
        Auth::Session(&root),
        Some(json!({})),
        &[],
    )
    .await;
    let new_pw = r.json()["data"]["password"].as_str().unwrap().to_string();
    assert_eq!(
        call(&f, "GET", "/api/v1/me", Auth::Session(&a6), None, &[])
            .await
            .status,
        401
    );
    let r = try_login(&f, "ann", &new_pw).await;
    assert_eq!(r.json()["data"]["must_change_password"], true);
    // 不能停用、刪除自己；不能移除最後一位管理員
    let r = call(
        &f,
        "POST",
        "/api/v1/users/root/disable",
        Auth::Session(&root),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(r.json()["error"]["code"], "cannot_target_self");
    let r = call(
        &f,
        "PUT",
        "/api/v1/users/root/roles",
        Auth::Session(&root),
        Some(json!({"roles": ["viewer"]})),
        &[],
    )
    .await;
    assert_eq!(r.json()["error"]["code"], "last_admin");
    // 刪除（L2）需要輸入帳號
    let r = call(
        &f,
        "POST",
        "/api/v1/users/ann/delete",
        Auth::Session(&root),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(r.json()["error"]["code"], "confirmation_required");
    let r = call(
        &f,
        "POST",
        "/api/v1/users/ann/delete",
        Auth::Session(&root),
        Some(json!({"confirm": "ann"})),
        &[],
    )
    .await;
    assert_eq!(r.status, 200);
    let actions: Vec<String> = audit_actions(&f).into_iter().map(|a| a.0).collect();
    for a in [
        "session.revoke_others",
        "session.revoke_all",
        "user.disable",
        "password.reset",
        "user.delete",
    ] {
        assert!(actions.contains(&a.to_string()), "{a} {actions:?}");
    }
    assert!(f.admin().audit_verify().unwrap().ok());
    f.stop().await;
}

#[tokio::test]
async fn totp_second_step_and_recovery_codes() {
    let f = fixture("auth-totp", cheap).await;
    add_user(&f, "mfa", &["viewer"], false);
    let s = login(&f, "mfa").await;
    // 綁定需要密碼
    let r = call(
        &f,
        "POST",
        "/api/v1/me/totp/setup",
        Auth::Session(&s),
        Some(json!({"password": "wrong wrong wrong"})),
        &[],
    )
    .await;
    assert_eq!(r.status, 403);
    let r = call(
        &f,
        "POST",
        "/api/v1/me/totp/setup",
        Auth::Session(&s),
        Some(json!({"password": PW})),
        &[],
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let b32 = r.json()["data"]["secret"].as_str().unwrap().to_string();
    assert!(
        r.json()["data"]["otpauth_uri"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/Kairos:mfa?")
    );
    let secret = decode_b32(&b32);
    // 資料庫中的秘密是加密的
    let sealed: Vec<u8> = f
        .admin()
        .query_row(
            "SELECT totp_pending FROM users WHERE username = 'mfa'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!sealed.windows(secret.len()).any(|w| w == secret.as_slice()));
    let now = Utc::now().timestamp();
    let code = |t: i64| {
        format!(
            "{:06}",
            crate::users::totp::hotp(&secret, crate::users::totp::step_of(t) as u64)
        )
    };
    let r = call(
        &f,
        "POST",
        "/api/v1/me/totp/enable",
        Auth::Session(&s),
        Some(json!({"code": "000000"})),
        &[],
    )
    .await;
    assert_eq!(r.status, 401);
    // 用前一步的碼啟用，讓下面登入時可以用目前這一步
    let r = call(
        &f,
        "POST",
        "/api/v1/me/totp/enable",
        Auth::Session(&s),
        Some(json!({"code": code(now - 30)})),
        &[],
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let recovery: Vec<String> = r.json()["data"]["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(recovery.len(), 10);
    // 第一步：需要第二步，沒有 cookie
    let r = try_login(&f, "mfa", PW).await;
    assert_eq!(r.json()["data"]["mfa_required"], true);
    assert!(r.header("set-cookie").is_none());
    let token = r.json()["data"]["mfa_token"].as_str().unwrap().to_string();
    let step2 = |token: String, code: String| {
        let f = &f;
        async move {
            call(
                f,
                "POST",
                "/api/v1/auth/login/mfa",
                Auth::None,
                Some(json!({"mfa_token": token, "code": code})),
                &[],
            )
            .await
        }
    };
    let r = step2(token.clone(), "123456".into()).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["error"]["product_code"], "KRS-E6012");
    let r = step2(token.clone(), code(now)).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert!(r.header("set-cookie").is_some());
    // 同一個挑戰不能再用；同一個碼不能再用（重放）
    assert_eq!(step2(token.clone(), code(now)).await.status, 401);
    let t2 = try_login(&f, "mfa", PW).await.json()["data"]["mfa_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(step2(t2.clone(), code(now)).await.status, 401);
    // 復原碼：只能用一次（不分大小寫、可省略連字號）
    let r = step2(t2, recovery[0].to_lowercase().replace('-', "")).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let t3 = try_login(&f, "mfa", PW).await.json()["data"]["mfa_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(step2(t3, recovery[0].clone()).await.status, 401);
    // 停用 TOTP：只有密碼不行，需要驗證碼或復原碼
    let s2 = session_of(
        &step2(
            try_login(&f, "mfa", PW).await.json()["data"]["mfa_token"]
                .as_str()
                .unwrap()
                .to_string(),
            recovery[2].clone(),
        )
        .await,
    );
    let r = call(
        &f,
        "POST",
        "/api/v1/me/totp/disable",
        Auth::Session(&s2),
        Some(json!({"password": PW})),
        &[],
    )
    .await;
    assert_eq!(r.status, 401, "{}", String::from_utf8_lossy(&r.body));
    let r = call(
        &f,
        "POST",
        "/api/v1/me/totp/disable",
        Auth::Session(&s2),
        Some(json!({"password": PW, "code": recovery[3]})),
        &[],
    )
    .await;
    assert_eq!(
        r.json()["data"]["disabled"],
        true,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(
        try_login(&f, "mfa", PW).await.json()["data"]["mfa_required"],
        false
    );
    f.stop().await;
}

fn decode_b32(s: &str) -> Vec<u8> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bits = 0u64;
    let mut n = 0;
    let mut out = Vec::new();
    for c in s.bytes() {
        bits = (bits << 5) | alphabet.iter().position(|x| *x == c).unwrap() as u64;
        n += 5;
        if n >= 8 {
            out.push((bits >> (n - 8)) as u8);
            n -= 8;
        }
    }
    out
}

#[tokio::test]
async fn group_scoped_roles_only_see_their_hosts() {
    let f = fixture("auth-scope", cheap).await;
    let conn = f.admin();
    for (id, name, group) in [
        (
            "11111111-0000-4000-8000-000000000001",
            "line3-a",
            Some("Line3"),
        ),
        (
            "11111111-0000-4000-8000-000000000002",
            "line3-b",
            Some("line3"),
        ),
        (
            "11111111-0000-4000-8000-000000000003",
            "office",
            Some("Office"),
        ),
        ("11111111-0000-4000-8000-000000000004", "nogroup", None),
    ] {
        conn.execute(
            "INSERT INTO hosts (agent_id, hostname, online, first_seen, last_seen, host_group) VALUES (?1, ?2, 0, 0, 0, ?3)",
            rusqlite::params![id, name, group],
        )
        .unwrap();
    }
    // 主控台啟動時背景工作已建立過索引（當時還沒有這些主機）
    f.console.ctx.hosts.invalidate().await;
    add_user(&f, "p4", &["operator@group:LINE3"], false);
    add_user(&f, "boss", &["viewer"], false);
    let p4 = login(&f, "p4").await;
    let boss = login(&f, "boss").await;
    let r = call(&f, "GET", "/api/v1/hosts", Auth::Session(&p4), None, &[]).await;
    let names: Vec<String> = r.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["hostname"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.iter().all(|n| n.starts_with("line3")));
    assert_eq!(r.json()["page"]["total"], 2);
    assert_eq!(
        call(&f, "GET", "/api/v1/hosts", Auth::Session(&boss), None, &[])
            .await
            .json()["page"]["total"],
        4
    );
    // 範圍外的主機與不存在相同（404）
    let r = call(
        &f,
        "GET",
        "/api/v1/hosts/11111111-0000-4000-8000-000000000003",
        Auth::Session(&p4),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, 404);
    let r = call(
        &f,
        "GET",
        "/api/v1/hosts/11111111-0000-4000-8000-000000000003/metrics",
        Auth::Session(&p4),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, 404);
    // 資產與行程（METRICS.md 第 10 節）：同樣是 hosts:read 加群組範圍
    for sub in ["inventory", "inventory/changes", "processes"] {
        for (id, status) in [("3", 404), ("1", 200)] {
            let r = call(
                &f,
                "GET",
                &format!("/api/v1/hosts/11111111-0000-4000-8000-00000000000{id}/{sub}"),
                Auth::Session(&p4),
                None,
                &[],
            )
            .await;
            assert_eq!(r.status, status, "{sub} {id}");
        }
    }
    assert_eq!(
        call(
            &f,
            "GET",
            "/api/v1/hosts/11111111-0000-4000-8000-000000000001",
            Auth::Session(&p4),
            None,
            &[]
        )
        .await
        .status,
        200
    );
    // 總覽只算範圍內
    let v = call(&f, "GET", "/api/v1/overview", Auth::Session(&p4), None, &[])
        .await
        .json();
    assert_eq!(v["data"]["total"], 2);
    assert!(v["data"]["delivery"].is_null());
    // group 參數
    let v = call(
        &f,
        "GET",
        "/api/v1/hosts?group=office",
        Auth::Session(&boss),
        None,
        &[],
    )
    .await
    .json();
    assert_eq!(v["page"]["total"], 1);
    // 0.7.0：篩選值（/hosts/facets）與連線紀錄（/hosts/{id}/connections）同樣只含範圍內的主機
    let v = call(
        &f,
        "GET",
        "/api/v1/hosts/facets",
        Auth::Session(&p4),
        None,
        &[],
    )
    .await
    .json();
    assert_eq!(v["data"]["total"], 2);
    let groups = v["data"]["group"]["values"].as_array().unwrap();
    assert!(
        groups
            .iter()
            .all(|g| g["value"].as_str().unwrap().eq_ignore_ascii_case("line3")),
        "{groups:?}"
    );
    let v = call(
        &f,
        "GET",
        "/api/v1/hosts/facets",
        Auth::Session(&boss),
        None,
        &[],
    )
    .await
    .json();
    assert_eq!(v["data"]["total"], 4);
    assert_eq!(v["data"]["no_group"]["count"], 1);
    for (id, status) in [
        ("11111111-0000-4000-8000-000000000003", 404),
        ("11111111-0000-4000-8000-000000000001", 200),
    ] {
        let r = call(
            &f,
            "GET",
            &format!("/api/v1/hosts/{id}/connections"),
            Auth::Session(&p4),
            None,
            &[],
        )
        .await;
        assert_eq!(r.status, status, "{id}");
    }
    // 群組範圍的維運者沒有系統層級的權限（核准、設定、健康）
    assert_eq!(
        call(&f, "GET", "/api/v1/settings", Auth::Session(&p4), None, &[])
            .await
            .status,
        403
    );
    assert_eq!(
        call(
            &f,
            "GET",
            "/api/v1/system/health",
            Auth::Session(&p4),
            None,
            &[]
        )
        .await
        .status,
        403
    );
    f.stop().await;
}

/// 測試用的目錄服務
struct FakeDirectory;

impl Directory for FakeDirectory {
    fn authenticate<'a>(
        &'a self,
        username: &'a str,
        password: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<DirectoryUser, DirError>> + Send + 'a>> {
        Box::pin(async move {
            match (username, password) {
                ("chen.y", "ad-password") => Ok(DirectoryUser {
                    dn: "CN=Chen,OU=Staff,DC=city".into(),
                    username: "chen.y".into(),
                    display_name: Some("陳小姐".into()),
                    email: None,
                    groups: vec!["CN=IT-Ops,OU=Groups,DC=city".into()],
                }),
                ("root", "ad-root") => Ok(DirectoryUser {
                    dn: "CN=root,DC=city".into(),
                    username: "root".into(),
                    display_name: None,
                    email: None,
                    groups: vec![],
                }),
                ("disabled.user", _) => Err(DirError::AccountDisabled),
                ("down", _) => Err(DirError::Unreachable("dc01:636：connection refused".into())),
                _ => Err(DirError::InvalidCredentials),
            }
        })
    }
}

#[tokio::test]
async fn directory_login_maps_groups_to_roles() {
    let f = fixture_with(
        "auth-ldap",
        |c| {
            cheap(c);
            c.ldap.enabled = true;
        },
        Some(Arc::new(FakeDirectory)),
    )
    .await;
    add_user(&f, "root", &["admin"], false);
    let root = login(&f, "root").await;
    let r = call(&f, "PUT", "/api/v1/directory/mappings", Auth::Session(&root), Some(json!({"mappings": [{"group_dn": "cn=it-ops, ou=groups, dc=city", "role": "operator"}]})), &[]).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let ldap = |u: &str, p: &str| {
        let f = &f;
        let body = json!({"username": u, "password": p, "method": "ldap"});
        async move { call(f, "POST", "/api/v1/auth/login", Auth::None, Some(body), &[]).await }
    };
    let r = ldap("chen.y", "ad-password").await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let s = session_of(&r);
    let me = call(&f, "GET", "/api/v1/me", Auth::Session(&s), None, &[])
        .await
        .json();
    assert_eq!(me["data"]["user"]["source"], "ldap");
    assert_eq!(me["data"]["roles"][0]["role"], "operator");
    assert_eq!(
        me["data"]["roles"][0]["via_group"],
        "CN=IT-Ops,OU=Groups,DC=city"
    );
    assert!(
        me["data"]["permissions"]
            .as_array()
            .unwrap()
            .contains(&json!("enroll.requests:decide"))
    );
    assert_eq!(
        call(&f, "GET", "/api/v1/users", Auth::Session(&s), None, &[])
            .await
            .status,
        403
    );
    // 目錄服務帳號不能以本機方式登入、不能在 Kairos 變更密碼
    assert_eq!(try_login(&f, "chen.y", "ad-password").await.status, 401);
    let r = call(
        &f,
        "POST",
        "/api/v1/me/password",
        Auth::Session(&s),
        Some(json!({"current_password": "ad-password", "new_password": "something long enough"})),
        &[],
    )
    .await;
    assert_eq!(r.json()["error"]["code"], "directory_account");
    // 錯誤分類
    let r = ldap("chen.y", "wrong").await;
    assert_eq!(
        (r.status, r.json()["error"]["product_code"].clone()),
        (401, json!("KRS-E6001"))
    );
    let r = ldap("disabled.user", "x").await;
    assert_eq!(
        (r.status, r.json()["error"]["product_code"].clone()),
        (403, json!("KRS-E6009"))
    );
    let r = ldap("down", "x").await;
    assert_eq!(
        (r.status, r.json()["error"]["product_code"].clone()),
        (503, json!("KRS-E6002"))
    );
    // 不透露伺服器細節
    assert!(!String::from_utf8_lossy(&r.body).contains("dc01"));
    // 本機帳號同名時不會被目錄服務接管
    assert_eq!(ldap("root", "ad-root").await.status, 401);
    // 以帳號測試登入（不建立工作階段）
    let r = call(
        &f,
        "POST",
        "/api/v1/directory/test",
        Auth::Session(&root),
        Some(json!({"username": "chen.y", "password": "ad-password"})),
        &[],
    )
    .await;
    assert_eq!(r.json()["data"]["ok"], true);
    assert_eq!(r.json()["data"]["roles"][0]["role"], "operator");
    f.stop().await;
}

#[tokio::test]
async fn api_tokens_belong_to_roles_or_users() {
    let f = fixture("auth-tokens", cheap).await;
    add_user(&f, "root", &["admin"], false);
    add_user(&f, "bob", &["viewer"], false);
    let root = login(&f, "root").await;
    // 必須指定角色或擁有者
    let r = call(
        &f,
        "POST",
        "/api/v1/api-tokens",
        Auth::Session(&root),
        Some(json!({"name": "x"})),
        &[],
    )
    .await;
    assert_eq!(r.status, 400);
    // 不提供不過期或超過 3650 天的 token
    let r = call(
        &f,
        "POST",
        "/api/v1/api-tokens",
        Auth::Session(&root),
        Some(json!({"name": "x", "role": "viewer", "expires_in_secs": 3651i64 * 86_400})),
        &[],
    )
    .await;
    assert_eq!(r.status, 400);
    let r = call(
        &f,
        "POST",
        "/api/v1/api-tokens",
        Auth::Session(&root),
        Some(json!({"name": "bob-cli", "owner": "bob", "role": "admin"})),
        &[],
    )
    .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let token = r.json()["data"]["token"].as_str().unwrap().to_string();
    let id = r.json()["data"]["token_id"].as_str().unwrap().to_string();
    assert_eq!(r.json()["data"]["grant"], "user:bob∩admin");
    // 擁有者是唯讀：token 即使指定 admin 也只有唯讀
    assert_eq!(
        call(&f, "GET", "/api/v1/hosts", Auth::Token(&token), None, &[])
            .await
            .status,
        200
    );
    assert_eq!(
        call(&f, "GET", "/api/v1/users", Auth::Token(&token), None, &[])
            .await
            .status,
        403
    );
    let me = call(&f, "GET", "/api/v1/me", Auth::Token(&token), None, &[])
        .await
        .json();
    assert_eq!(me["data"]["owner"]["username"], "bob");
    // 列表不含秘密
    let r = call(
        &f,
        "GET",
        "/api/v1/api-tokens",
        Auth::Session(&root),
        None,
        &[],
    )
    .await;
    let secret = &token[4 + 16 + 1..];
    assert!(!String::from_utf8_lossy(&r.body).contains(secret));
    // 擁有者停用：token 立即失效
    call(
        &f,
        "POST",
        "/api/v1/users/bob/disable",
        Auth::Session(&root),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(
        call(&f, "GET", "/api/v1/hosts", Auth::Token(&token), None, &[])
            .await
            .status,
        401
    );
    call(
        &f,
        "POST",
        "/api/v1/users/bob/enable",
        Auth::Session(&root),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(
        call(&f, "GET", "/api/v1/hosts", Auth::Token(&token), None, &[])
            .await
            .status,
        200
    );
    // 撤銷
    let r = call(
        &f,
        "POST",
        &format!("/api/v1/api-tokens/{id}/revoke"),
        Auth::Session(&root),
        Some(json!({"reason": "測試"})),
        &[],
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        call(&f, "GET", "/api/v1/hosts", Auth::Token(&token), None, &[])
            .await
            .status,
        401
    );
    let actions: Vec<String> = audit_actions(&f).into_iter().map(|a| a.0).collect();
    assert!(
        actions.contains(&"token.create".to_string())
            && actions.contains(&"token.revoke".to_string())
    );
    f.stop().await;
}

#[tokio::test]
async fn audit_log_query_export_and_verify() {
    let f = fixture("auth-audit", cheap).await;
    add_user(&f, "root", &["admin"], false);
    add_user(&f, "aud", &["auditor"], false);
    add_user(&f, "op", &["operator"], false);
    let root = login(&f, "root").await;
    let aud = login(&f, "aud").await;
    let op = login(&f, "op").await;
    call(
        &f,
        "POST",
        "/api/v1/users",
        Auth::Session(&root),
        Some(json!({"username": "carol", "roles": ["viewer"]})),
        &[],
    )
    .await;
    // 稽核員：看得到所有人
    let r = call(
        &f,
        "GET",
        "/api/v1/audit?category=user&limit=5",
        Auth::Session(&aud),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert!(
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "user.create" && e["actor"] == "user:root"),
        "{v}"
    );
    assert!(v["data"][0]["hash"].as_str().unwrap().len() == 64);
    // 維運者：只看得到自己的
    let r = call(
        &f,
        "GET",
        "/api/v1/audit?actor=user:root",
        Auth::Session(&op),
        None,
        &[],
    )
    .await;
    let v = r.json();
    assert_eq!(v["meta"]["own_only"], true);
    assert!(
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["actor"] == "user:op"),
        "{v}"
    );
    // 任何角色都不能修改或刪除
    for m in ["PUT", "DELETE"] {
        let r = call(
            &f,
            m,
            "/api/v1/audit",
            Auth::Session(&root),
            Some(json!({})),
            &[],
        )
        .await;
        assert_eq!(r.status, 405, "{m}");
    }
    // 匯出：JSONL，最後一行是鏈的起訖值
    let r = call(
        &f,
        "GET",
        "/api/v1/audit/export",
        Auth::Session(&aud),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(
        r.header("content-type")
            .unwrap()
            .starts_with("application/x-ndjson")
    );
    let text = String::from_utf8_lossy(&r.body).into_owned();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let last = lines.last().unwrap();
    assert_eq!(last["type"], "chain");
    assert_eq!(last["count"].as_u64().unwrap() as usize, lines.len() - 1);
    for l in &lines[..lines.len() - 1] {
        let row = crate::audit::AuditRow {
            id: l["id"].as_i64().unwrap(),
            at: l["at_ms"].as_i64().unwrap(),
            actor: l["actor"].as_str().unwrap().into(),
            actor_ip: l["actor_ip"].as_str().map(str::to_string),
            category: l["category"].as_str().unwrap().into(),
            action: l["action"].as_str().unwrap().into(),
            object: l["object"].as_str().map(str::to_string),
            result: l["result"].as_str().unwrap().into(),
            code: l["code"].as_str().map(str::to_string),
            details: l["details_raw"].as_str().map(str::to_string),
            prev_hash: None,
            hash: None,
            hmac: None,
        };
        assert_eq!(
            crate::audit::compute_hash(l["prev_hash"].as_str().unwrap(), &row),
            l["hash"].as_str().unwrap(),
            "{l}"
        );
    }
    // 驗證：完整；竄改後指出第一個斷點並帶 KRS-E6201
    let r = call(
        &f,
        "POST",
        "/api/v1/audit/verify",
        Auth::Session(&aud),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(
        r.json()["data"]["ok"],
        true,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    f.admin()
        .execute(
            "UPDATE audit_events SET object = 'mallory' WHERE id = 3",
            [],
        )
        .unwrap();
    let r = call(
        &f,
        "POST",
        "/api/v1/audit/verify",
        Auth::Session(&aud),
        Some(json!({})),
        &[],
    )
    .await;
    let v = r.json();
    assert_eq!(v["data"]["ok"], false);
    assert_eq!(v["data"]["broken"]["id"], 3);
    assert_eq!(v["data"]["broken"]["product_code"], "KRS-E6201");
    // 頁首「完整性」：列表的 meta 帶最近一次驗證與 HMAC 起點（只看自己操作的身分沒有）
    let v = call(&f, "GET", "/api/v1/audit", Auth::Session(&aud), None, &[])
        .await
        .json();
    let i = &v["meta"]["integrity"];
    assert_eq!(i["last_verify"]["result"], "failed", "{i}");
    assert_eq!(i["last_verify"]["broken"]["id"], 3, "{i}");
    assert_eq!(i["last_verify"]["actor"], "user:aud", "{i}");
    assert_eq!(i["last_verify"]["self_check"], false, "{i}");
    // 測試的資料目錄沒有 audit-hmac.key：HMAC 尚未開始（null），欄位仍在
    assert!(i.get("hmac_since_id").is_some(), "{i}");
    assert_eq!(i["last_verify"]["hmac_since_id"], i["hmac_since_id"], "{i}");
    let v = call(&f, "GET", "/api/v1/audit", Auth::Session(&op), None, &[])
        .await
        .json();
    assert!(v["meta"]["integrity"].is_null(), "{v}");
    // 查詢與匯出本身也被記錄
    let actions: Vec<String> = audit_actions(&f).into_iter().map(|a| a.0).collect();
    for a in ["audit.query", "audit.export", "audit.verify"] {
        assert!(actions.contains(&a.to_string()), "{a}");
    }
    f.stop().await;
}

#[tokio::test]
async fn concurrent_guesses_are_bounded_by_the_source_budget() {
    let f = fixture("auth-concurrent", cheap).await;
    add_user(&f, "target", &["viewer"], false);
    // 同時送出 30 次錯誤的密碼：先扣額度，所以最多 10 次真的被驗證，其餘 429
    let mut tasks = Vec::new();
    for _ in 0..30 {
        let addr = f.addr();
        tasks.push(tokio::spawn(async move {
            let body = json!({"username": "target", "password": "wrong password!"}).to_string();
            let req = format!(
                "POST /api/v1/auth/login HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            send_raw(addr, req.as_bytes()).await.status
        }));
    }
    let mut statuses = Vec::new();
    for t in tasks {
        statuses.push(t.await.unwrap());
    }
    let guessed = statuses.iter().filter(|s| **s != 429).count();
    assert!(guessed <= 11, "{statuses:?}");
    assert!(statuses.contains(&429), "{statuses:?}");
    // 帳號在第 5 次失敗時已鎖定
    let conn = f.admin();
    let u = conn.find_user("target").unwrap().unwrap();
    assert!(u.locked(Utc::now().timestamp_millis()), "{u:?}");
    f.stop().await;
}

#[tokio::test]
async fn failed_logins_do_not_store_passwords_typed_as_usernames() {
    let f = fixture("auth-mask", cheap).await;
    let r = try_login(&f, "S3cret pass!", "x").await;
    assert_eq!(r.status, 401);
    let r = try_login(&f, "nobody.here", "x").await;
    assert_eq!(r.status, 401);
    let objects: Vec<Option<String>> = audit_actions(&f)
        .into_iter()
        .filter(|a| a.0 == "login.failure")
        .map(|a| a.2)
        .collect();
    assert_eq!(objects.len(), 2);
    assert!(
        objects
            .iter()
            .all(|o| !o.as_deref().unwrap_or("").contains("S3cret")),
        "{objects:?}"
    );
    assert!(objects.contains(&Some("nobody.here".to_string())));
    f.stop().await;
}

#[tokio::test]
async fn permissions_are_recomputed_for_long_lived_streams() {
    let f = fixture("auth-revalidate", cheap).await;
    let id = add_user(&f, "watcher", &["viewer"], false);
    let s = login(&f, "watcher").await;
    let mut headers = hyper::HeaderMap::new();
    headers.insert(hyper::header::COOKIE, s.cookie.parse().unwrap());
    let auth = &f.console.ctx.auth;
    let p = auth
        .authenticate(
            &headers,
            &hyper::Method::GET,
            "127.0.0.1".parse().unwrap(),
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(
        auth.revalidate(&p, Utc::now()).await,
        Some(crate::rbac::HostAccess::All)
    );
    // 降為群組範圍
    let conn = f.admin();
    conn.replace_assignments(
        us::SUBJECT_USER,
        &id.to_string(),
        &[Assignment::parse("viewer@group:Line3").unwrap()],
        "t",
        0,
    )
    .unwrap();
    assert_eq!(
        auth.revalidate(&p, Utc::now()).await,
        Some(crate::rbac::HostAccess::Groups(vec!["line3".into()]))
    );
    // 移除所有角色、或結束工作階段：沒有主機的讀取權限
    conn.replace_assignments(us::SUBJECT_USER, &id.to_string(), &[], "t", 0)
        .unwrap();
    assert_eq!(
        auth.revalidate(&p, Utc::now()).await,
        Some(crate::rbac::HostAccess::None)
    );
    conn.replace_assignments(
        us::SUBJECT_USER,
        &id.to_string(),
        &[Assignment::parse("viewer").unwrap()],
        "t",
        0,
    )
    .unwrap();
    conn.end_user_sessions(id, None, "test", "t", Utc::now().timestamp_millis())
        .unwrap();
    assert_eq!(
        auth.revalidate(&p, Utc::now()).await,
        Some(crate::rbac::HostAccess::None)
    );
    // token：撤銷後也是
    let p = auth
        .authenticate(
            &{
                let mut h = hyper::HeaderMap::new();
                h.insert(
                    hyper::header::AUTHORIZATION,
                    format!("Bearer {}", f.token).parse().unwrap(),
                );
                h
            },
            &hyper::Method::GET,
            "127.0.0.1".parse().unwrap(),
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(
        auth.revalidate(&p, Utc::now()).await,
        Some(crate::rbac::HostAccess::All)
    );
    conn.execute("UPDATE api_tokens SET revoked_at = 1", [])
        .unwrap();
    assert_eq!(
        auth.revalidate(&p, Utc::now()).await,
        Some(crate::rbac::HostAccess::None)
    );
    f.stop().await;
}

/// 雙人覆核（REMOTE.md 第 5.4 節；`[remote] rollout_approval = "required"`）：送出後待核准；建立者、送出者
/// 不能核准（403 `self_approval`，`KRS-E6106`）；維運者沒有 `rollouts:approve`；另一位管理員的工作階段核准後才開始；
/// 退回需要原因
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollout_approval_needs_another_admins_session() {
    use crate::remote::test_sign::{TestSigner, release};
    let signer = TestSigner::new(31);
    let trust = crate::api::tests::temp_dir("approval-trust").join("allowed_signers");
    std::fs::create_dir_all(trust.parent().unwrap()).unwrap();
    std::fs::write(&trust, signer.allowed_signers()).unwrap();
    let t = trust.to_string_lossy().into_owned();
    let f = fixture("approval", move |c| {
        cheap(c);
        c.remote.allowed_signers = t;
        c.remote.rollout_approval = "required".into();
    })
    .await;
    add_user(&f, "alice", &["admin"], false);
    add_user(&f, "bob", &["admin"], false);
    add_user(&f, "carol", &["admin"], false);
    add_user(&f, "oscar", &["operator"], false);
    let alice = login(&f, "alice").await;
    let bob = login(&f, "bob").await;
    let carol = login(&f, "carol").await;
    let oscar = login(&f, "oscar").await;
    f.admin()
        .execute(
            "INSERT INTO hosts (agent_id, hostname, agent_version, host_group, tags, os, install_method,
                install_upgrade, install_config, first_seen, last_seen, online)
             VALUES ('7e2f0a11-0000-4000-8000-0000000000c1', 'web-01', '0.9.0', 'web', '[]', '{\"arch\":\"x86_64\"}', 'tar', 'ready', 'enabled', 0, 0, 1)",
            [],
        )
        .unwrap();
    // 版本庫：匯入並上傳 tar（工作階段）
    let rel = release(&signer, "0.9.2", "abcdef1");
    let r = call(
        &f,
        "POST",
        "/api/v1/releases",
        Auth::Session(&alice),
        Some(json!({"sha256sums": rel.sha256sums, "signature": rel.signature, "manifest": rel.manifest})),
        &[],
    )
    .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let (tar_name, tar) = rel
        .files
        .iter()
        .find(|(n, _)| n.ends_with(".tar.gz"))
        .unwrap()
        .clone();
    let mut req = format!(
        "PUT /api/v1/releases/0.9.2/artifacts/{tar_name} HTTP/1.0\r\nHost: localhost\r\nCookie: {}\r\nX-CSRF-Token: {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
        alice.cookie,
        alice.csrf,
        tar.len()
    )
    .into_bytes();
    req.extend_from_slice(&tar);
    let r = send_raw(f.addr(), &req).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));

    // alice 建立、bob 送出：待核准
    let spec = json!({"name": "0.9.2 全站", "target_version": "0.9.2", "selector": {"all": true}, "waves": [{"size": "rest"}]});
    let r = call(
        &f,
        "POST",
        "/api/v1/rollouts/preview",
        Auth::Session(&alice),
        Some(spec.clone()),
        &[],
    )
    .await;
    assert_eq!(r.json()["data"]["approval_required"], true, "{}", r.json());
    let r = call(
        &f,
        "POST",
        "/api/v1/rollouts",
        Auth::Session(&alice),
        Some(spec),
        &[],
    )
    .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let id = r.json()["data"]["id"].as_str().unwrap().to_string();
    let path = |a: &str| format!("/api/v1/rollouts/{id}/{a}");
    let r = call(
        &f,
        "POST",
        &path("submit"),
        Auth::Session(&bob),
        Some(json!({"confirm": "升級 1 台"})),
        &[],
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["data"]["state"], "pending_approval");
    assert_eq!(r.json()["data"]["submitted_by"], "user:bob");
    // 送審摘要：確認字串與 digest 由伺服器計算（ADR-0002 決策點 2b）
    let digest = r.json()["data"]["submission"]["digest"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(digest.len(), 64);
    assert_eq!(
        (
            r.json()["data"]["submission"]["confirm_text"].as_str(),
            r.json()["data"]["submission"]["confirm_level"].as_str(),
            r.json()["data"]["submission"]["eligible"].as_i64(),
        ),
        (Some("核准 1 台"), Some("L2"), Some(1))
    );
    let r = call(
        &f,
        "GET",
        "/api/v1/rollouts?state=pending_approval",
        Auth::Session(&oscar),
        None,
        &[],
    )
    .await;
    assert_eq!(
        (
            r.json()["meta"]["pending_approval"].as_i64(),
            r.json()["meta"]["rollout_approval"].as_str()
        ),
        (Some(1), Some("required"))
    );
    // 「我可以核准的」筆數與每一筆的 viewer 由後端依這次請求的身分計算（A5）：維運者沒有權限、建立者與
    // 送出者不能核准自己的、API token 不能核准；只有第三位管理員的工作階段是 1。可核准者是 carol 一人
    let admin_token = super::tokens::create_with(
        &f.admin(),
        "admin",
        &super::tokens::TokenGrant::Role(crate::rbac::Role::Admin),
        None,
        Some("tester"),
        None,
        Utc::now().timestamp_millis(),
    )
    .unwrap()
    .token;
    for (auth, actionable, blocked, creator, submitter) in [
        (Auth::Session(&oscar), 0, Some("forbidden"), false, false),
        (Auth::Session(&alice), 0, Some("self_approval"), true, false),
        (Auth::Session(&bob), 0, Some("self_approval"), false, true),
        (Auth::Token(&admin_token), 0, Some("session_required"), false, false),
        (Auth::Session(&carol), 1, None, false, false),
    ] {
        let r = call(&f, "GET", "/api/v1/rollouts", auth, None, &[]).await;
        let j = r.json();
        assert_eq!(
            (
                j["meta"]["pending_approval"].as_i64(),
                j["meta"]["pending_approval_actionable"].as_i64()
            ),
            (Some(1), Some(actionable)),
            "{blocked:?}"
        );
        assert_eq!(
            j["data"][0]["viewer"],
            json!({"can_approve": blocked.is_none(), "blocked": blocked, "is_creator": creator, "is_submitter": submitter}),
        );
        assert_eq!(j["data"][0]["submission"]["digest"], digest.as_str());
        assert_eq!(j["data"][0]["submission"]["approvers_available"], 1);
    }
    // 系統健康：待核准 1 個、沒有「沒有可核准者」的推送
    let r = call(
        &f,
        "GET",
        "/api/v1/system/health",
        Auth::Session(&carol),
        None,
        &[],
    )
    .await;
    let j = r.json();
    let remote = j["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "remote")
        .unwrap();
    assert_eq!(
        (
            remote["values"]["pending_approval"].as_i64(),
            remote["values"]["pending_approval_no_approvers"].as_i64()
        ),
        (Some(1), Some(0))
    );

    // 維運者沒有權限；建立者與送出者不能核准；沒有 CSRF token 的請求被拒
    let r = call(
        &f,
        "POST",
        &path("approve"),
        Auth::Session(&oscar),
        None,
        &[],
    )
    .await;
    assert_eq!(
        (
            r.status,
            r.json()["error"]["details"]["permission"].as_str()
        ),
        (403, Some("rollouts:approve"))
    );
    for s in [&alice, &bob] {
        let r = call(&f, "POST", &path("approve"), Auth::Session(s), None, &[]).await;
        assert_eq!(
            (
                r.status,
                r.json()["error"]["code"].as_str(),
                r.json()["error"]["product_code"].as_str()
            ),
            (403, Some("self_approval"), Some("KRS-E6106"))
        );
    }
    let r = call(
        &f,
        "POST",
        &path("approve"),
        Auth::CookieOnly(&carol),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, 403, "CSRF");
    // 退回需要原因
    let r = call(
        &f,
        "POST",
        &path("reject"),
        Auth::Session(&carol),
        None,
        &[],
    )
    .await;
    assert_eq!(
        (r.status, r.json()["error"]["details"]["parameter"].as_str()),
        (400, Some("reason"))
    );
    // 仍然待核准、沒有開始
    let r = call(
        &f,
        "GET",
        &format!("/api/v1/rollouts/{id}"),
        Auth::Session(&oscar),
        None,
        &[],
    )
    .await;
    assert_eq!(r.json()["data"]["state"], "pending_approval");
    assert_eq!(r.json()["data"]["started_at"], Value::Null);

    // 直接打 API 的負面測試（A1、A2）：核准必須帶伺服器算出的確認字串與 digest——空的本文、空字串、
    // 舊的台數、降版的字串、沒有 digest、舊的 digest、不認得的欄位都被拒絕，狀態不變
    for (body, status, code) in [
        (None, 400, "confirmation_required"),
        (Some(json!({})), 400, "confirmation_required"),
        (Some(json!({"confirm": "", "digest": digest})), 400, "confirmation_required"),
        (Some(json!({"confirm": "核准 2 台", "digest": digest})), 400, "confirmation_required"),
        (Some(json!({"confirm": "核准降版 1 台", "digest": digest, "reason": "x"})), 400, "confirmation_required"),
        (Some(json!({"confirm": "升級 1 台", "digest": digest})), 400, "confirmation_required"),
        (Some(json!({"confirm": "核准 1 台"})), 400, "invalid_parameter"),
        (Some(json!({"confirm": "核准 1 台", "digest": "0".repeat(64)})), 409, "submission_changed"),
        (Some(json!({"confirm": "核准 1 台", "digest": digest, "force": true})), 400, "invalid_parameter"),
        (Some(json!({"confirm": 1, "digest": digest})), 400, "invalid_parameter"),
    ] {
        let r = call(
            &f,
            "POST",
            &path("approve"),
            Auth::Session(&carol),
            body.clone(),
            &[],
        )
        .await;
        assert_eq!(
            (r.status, r.json()["error"]["code"].as_str()),
            (status, Some(code)),
            "{body:?}"
        );
        if code == "confirmation_required" {
            assert_eq!(
                r.json()["error"]["details"]["confirm_text"],
                "核准 1 台",
                "{body:?}"
            );
        }
    }
    let r = call(
        &f,
        "GET",
        &format!("/api/v1/rollouts/{id}"),
        Auth::Session(&carol),
        None,
        &[],
    )
    .await;
    let j = r.json();
    assert_eq!(j["data"]["state"], "pending_approval");
    assert_eq!(j["data"]["submission"]["digest"], digest.as_str());
    assert_eq!(j["data"]["viewer"]["can_approve"], true);
    assert!(
        !audit_actions(&f)
            .iter()
            .any(|a| a.0 == "rollout.approve")
    );

    // 第三位管理員核准（前後空白比照 submit 去掉）：開始
    let r = call(
        &f,
        "POST",
        &path("approve"),
        Auth::Session(&carol),
        Some(json!({"confirm": " 核准 1 台 ", "digest": digest})),
        &[],
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        (
            r.json()["data"]["state"].as_str(),
            r.json()["data"]["approved_by"].as_str()
        ),
        (Some("running"), Some("user:carol"))
    );
    let r = call(
        &f,
        "POST",
        &path("approve"),
        Auth::Session(&carol),
        Some(json!({"confirm": "核准 1 台", "digest": digest})),
        &[],
    )
    .await;
    assert_eq!(
        (r.status, r.json()["error"]["code"].as_str()),
        (409, Some("invalid_state"))
    );
    // 核准之後清單不再有可以核准的，推送的 JSON 也不再帶 submission
    let r = call(&f, "GET", "/api/v1/rollouts", Auth::Session(&carol), None, &[]).await;
    assert_eq!(r.json()["meta"]["pending_approval_actionable"], 0);
    assert_eq!(r.json()["data"][0].get("submission"), None);
    let actions: Vec<(String, String)> =
        audit_actions(&f).into_iter().map(|a| (a.0, a.1)).collect();
    for (action, actor) in [
        ("rollout.submit", "user:bob"),
        ("rollout.approve", "user:carol"),
        ("rollout.start", "user:carol"),
    ] {
        assert!(
            actions.contains(&(action.to_string(), actor.to_string())),
            "{action} {actor}：{actions:?}"
        );
    }
    f.stop().await;
}
