//! 端對端：首位管理員以實際的 `kairos users create --role admin` 建立（沒有預設帳號密碼）、以產生的
//! 一次性密碼登入主控台（必須先變更密碼）、工作階段的 cookie 與 CSRF；稽核雜湊鏈以
//! `kairos audit verify` 驗證（含啟動、關機的系統事件）。

use kp_e2e::{Kairos, TestDir, WAIT, api_raw, run_to_exit, test_pki};
use serde_json::Value;

fn post(port: u16, path: &str, cookie: Option<(&str, &str)>, body: &str) -> (u16, String, Value) {
    let auth = cookie
        .map(|(c, csrf)| format!("Cookie: {c}\r\nX-CSRF-Token: {csrf}\r\n"))
        .unwrap_or_default();
    let (s, head, body) = api_raw(
        port,
        &format!(
            "POST {path} HTTP/1.0\r\nHost: localhost\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    );
    (s, head, serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn get(port: u16, path: &str, cookie: &str) -> (u16, Value) {
    let (s, _, body) = api_raw(
        port,
        &format!("GET {path} HTTP/1.0\r\nHost: localhost\r\nCookie: {cookie}\r\n\r\n"),
    );
    (s, serde_json::from_str(&body).unwrap_or(Value::Null))
}

#[test]
fn first_admin_logs_in_changes_password_and_the_audit_chain_verifies() {
    let root = TestDir::new("users");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let cli = |args: &[&str]| {
        let mut all = args.to_vec();
        all.extend(["--config", "kairos.toml"]);
        run_to_exit("kairos", &kairos.dir, &all, &[])
    };
    // 沒有預設帳號：建立前沒有使用者
    let (code, p) = cli(&["users", "list"]);
    assert_eq!(code, 0, "{}", p.dump());
    // 首位管理員：一次性密碼只在 stdout
    let (code, p) = cli(&["users", "create", "admin", "--role", "admin"]);
    assert_eq!(code, 0, "{}", p.dump());
    let password = p.stdout().trim().to_string();
    assert_eq!(password.len(), 23, "{}", p.dump());
    assert!(p.stderr().contains("一次性"), "{}", p.dump());
    // 角色名稱錯誤：用法錯誤
    let (code, _) = cli(&["users", "create", "x", "--role", "root"]);
    assert_eq!(code, 2);
    // 最後一位管理員不能停用
    let (code, p) = cli(&["users", "disable", "admin", "--yes"]);
    assert_eq!(code, 6, "{}", p.dump());

    let mut k = kairos.start();
    k.wait_for("主控台 API 已啟動", WAIT);
    let port = kairos.console_port;
    // 錯誤的密碼：KRS-E6001
    let (s, _, v) = post(
        port,
        "/api/v1/auth/login",
        None,
        r#"{"username":"admin","password":"wrong password"}"#,
    );
    assert_eq!(s, 401, "{v}");
    assert_eq!(v["error"]["product_code"], "KRS-E6001");
    let (s, head, v) = post(
        port,
        "/api/v1/auth/login",
        None,
        &format!(r#"{{"username":"admin","password":"{password}"}}"#),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["must_change_password"], true);
    let set_cookie = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .expect("應有 Set-Cookie");
    for attr in ["HttpOnly", "Secure", "SameSite=Strict"] {
        assert!(set_cookie.contains(attr), "{set_cookie}");
    }
    let cookie = set_cookie
        .split_once(':')
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
        .trim()
        .to_string();
    let csrf = v["data"]["csrf_token"].as_str().unwrap().to_string();
    // 必須先變更密碼
    let (s, v) = get(port, "/api/v1/hosts", &cookie);
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["error"]["code"], "password_change_required");
    let (s, _, v) = post(
        port,
        "/api/v1/me/password",
        Some((&cookie, &csrf)),
        &format!(
            r#"{{"current_password":"{password}","new_password":"a long and memorable passphrase"}}"#
        ),
    );
    assert_eq!(s, 200, "{v}");
    let (s, v) = get(port, "/api/v1/hosts", &cookie);
    assert_eq!(s, 200, "{v}");
    // 管理員可以建立其他使用者；回應帶一次性密碼
    let (s, _, v) = post(
        port,
        "/api/v1/users",
        Some((&cookie, &csrf)),
        r#"{"username":"chen.y","display_name":"陳小姐","roles":["auditor"]}"#,
    );
    assert_eq!(s, 201, "{v}");
    assert!(v["data"]["initial_password"].as_str().is_some());
    // 日誌不出現密碼
    assert!(!k.output().contains(&password));
    assert!(!k.output().contains("a long and memorable passphrase"));
    k.stop();

    // 稽核雜湊鏈：完整，含啟動、登入、密碼變更、建立使用者、關機
    let (code, p) = cli(&["audit", "verify", "--json"]);
    assert_eq!(code, 0, "{}", p.dump());
    let v: Value = serde_json::from_str(p.stdout().trim()).unwrap();
    assert_eq!(v["data"]["ok"], true, "{v}");
    let (code, p) = cli(&["audit", "list", "--limit", "100", "--json"]);
    assert_eq!(code, 0, "{}", p.dump());
    let v: Value = serde_json::from_str(p.stdout().trim()).unwrap();
    let actions: Vec<&str> = v["data"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["action"].as_str())
        .collect();
    for a in [
        "user.create",
        "system.start",
        "login.failure",
        "login.success",
        "password.change",
        "system.shutdown",
    ] {
        assert!(actions.contains(&a), "{a}：{actions:?}");
    }
    let raw = p.stdout();
    assert!(!raw.contains(&password) && !raw.contains("memorable passphrase"));
}
