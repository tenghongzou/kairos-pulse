//! 領域 store（設定、稽核、使用者、API token、PKI、註冊）與交易語意在兩個後端上的行為

use serde_json::json;
use std::time::{Duration, Instant};

use super::{TestDb, backend_tests, log};
use crate::audit::{Audit, AuditFilter};
use crate::enroll::store::{BlockRow, DbKind, DbState, RequestFilter, RequestRow, TokenRow};
use crate::pki::issue::{Reason, RevokedEntry};
use crate::pki::store::CertRow;
use crate::rbac::{Assignment, Role, Scope};
use crate::store::db::{StoreErrorKind, TxMode, in_write_tx};
use crate::users::store::{NewSession, NewUser};
use kairos_proto::enroll::TokenApproval;

const T0: i64 = 1_790_000_000_000;
const DAY: i64 = 86_400_000;

fn settings_and_meta(db: &TestDb) -> String {
    let mut out = String::new();
    let s = db.handle();
    assert_eq!(s.get_meta("missing").unwrap(), None);
    s.put_meta("k", "v1").unwrap();
    s.put_meta("k", "v2").unwrap();
    s.put_meta("n", "42").unwrap();
    assert_eq!(s.get_meta("k").unwrap().as_deref(), Some("v2"));
    assert_eq!(s.get_meta("n").unwrap().as_deref(), Some("42"));
    assert!(s.load_settings().unwrap().is_empty());
    s.set_setting("enroll.mode", &json!("open"), "ann", T0)
        .unwrap();
    s.set_setting("limits", &json!({"a": 1, "b": [true, null]}), "bob", T0 + 1)
        .unwrap();
    s.set_setting("enroll.mode", &json!("closed"), "cat", T0 + 2)
        .unwrap();
    log!(out, "settings", s.load_settings().unwrap());
    assert!(s.delete_setting("limits").unwrap());
    assert!(!s.delete_setting("limits").unwrap());
    log!(out, "after delete", s.load_settings().unwrap());
    // 唯讀連線讀得到、不能寫
    let r = db.read();
    assert_eq!(r.get_meta("k").unwrap().as_deref(), Some("v2"));
    assert!(r.put_meta("k", "v3").is_err());
    assert_eq!(db.read().get_meta("k").unwrap().as_deref(), Some("v2"));
    out
}

fn transactions(db: &TestDb) -> String {
    let s = db.handle();
    assert!(s.is_autocommit());
    // 沒有 commit 就 drop：回復
    {
        let tx = s.begin(TxMode::Immediate).unwrap();
        assert!(!tx.is_autocommit());
        tx.put_meta("a", "1").unwrap();
        // 交易中再開交易：失敗
        assert!(tx.begin(TxMode::Deferred).is_err());
        assert!(s.begin(TxMode::Deferred).is_err());
    }
    assert!(s.is_autocommit());
    assert_eq!(s.get_meta("a").unwrap(), None);
    let tx = s.begin(TxMode::Deferred).unwrap();
    tx.put_meta("a", "2").unwrap();
    tx.commit().unwrap();
    assert_eq!(s.get_meta("a").unwrap().as_deref(), Some("2"));
    // in_write_tx：成功才提交；已在交易中時沿用
    let r: Result<(), crate::store::db::StoreError> = in_write_tx(s.as_ref(), |s| {
        s.put_meta("b", "1")?;
        Err(std::io::Error::other("中止").into())
    });
    assert!(r.is_err());
    assert_eq!(s.get_meta("b").unwrap(), None);
    in_write_tx::<_, crate::store::db::StoreError>(s.as_ref(), |s| {
        assert!(!s.is_autocommit());
        in_write_tx::<_, crate::store::db::StoreError>(s, |s| s.put_meta("b", "2"))
    })
    .unwrap();
    assert_eq!(s.get_meta("b").unwrap().as_deref(), Some("2"));

    // 兩條連線的 Immediate 交易互斥：第二條等不到時回 Busy；第一條結束後可以取得
    let other = db.handle_busy(Duration::from_millis(300));
    let tx = s.begin(TxMode::Immediate).unwrap();
    tx.put_meta("c", "1").unwrap();
    let started = Instant::now();
    let e = other
        .begin(TxMode::Immediate)
        .err()
        .expect("第二個寫入交易應該等不到鎖");
    assert_eq!(e.kind(), StoreErrorKind::Busy, "{e}");
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "{:?}",
        started.elapsed()
    );
    // 被拒之後這條連線仍可使用（讀不到未提交的資料）
    assert!(other.is_autocommit());
    assert_eq!(other.get_meta("c").unwrap(), None);
    tx.commit().unwrap();
    let tx2 = other.begin(TxMode::Immediate).unwrap();
    assert_eq!(tx2.get_meta("c").unwrap().as_deref(), Some("1"));
    tx2.commit().unwrap();
    // 等待中的交易在對方提交後繼續
    let waiter = db.handle_busy(Duration::from_secs(5));
    let tx = s.begin(TxMode::Immediate).unwrap();
    let handle = std::thread::spawn(move || {
        let t = waiter.begin(TxMode::Immediate).unwrap();
        let seen = t.get_meta("d").unwrap();
        t.commit().unwrap();
        seen
    });
    std::thread::sleep(Duration::from_millis(200));
    tx.put_meta("d", "1").unwrap();
    tx.commit().unwrap();
    assert_eq!(handle.join().unwrap().as_deref(), Some("1"));
    String::new()
}

fn audit(
    actor: &'static str,
    action: &'static str,
    object: Option<&'static str>,
) -> Audit<'static> {
    Audit {
        actor,
        actor_ip: Some("10.0.0.9"),
        category: "users",
        action,
        object,
        result: "success",
        code: None,
        details: Some(json!({"n": 1, "名稱": "測試"})),
    }
}

fn audit_chain(db: &TestDb) -> String {
    let mut out = String::new();
    let s = db.handle();
    assert_eq!(s.audit_tail().unwrap(), None);
    log!(out, "empty verify", s.audit_verify().unwrap());
    for i in 0..5 {
        let id = s
            .audit_append(T0 + i, &audit("user:ann", "user.create", Some("user:bob")))
            .unwrap();
        assert_eq!(id, i + 1);
    }
    // 回復的交易不佔用 id
    {
        let tx = s.begin(TxMode::Immediate).unwrap();
        assert_eq!(
            tx.audit_append(T0 + 5, &audit("user:x", "user.delete", None))
                .unwrap(),
            6
        );
    }
    let failed = Audit {
        result: "failure",
        code: Some("KRS-E3101"),
        category: "auth",
        details: None,
        actor_ip: None,
        ..audit("user:Carol", "login", Some("session:ABC"))
    };
    assert_eq!(s.audit_append(T0 + 6, &failed).unwrap(), 6);
    // 與其他寫入在同一個交易
    let tx = s.begin(TxMode::Immediate).unwrap();
    tx.put_meta("with_audit", "1").unwrap();
    assert_eq!(
        tx.audit_append(
            T0 + 7,
            &audit("system", "settings.update", Some("enroll.mode"))
        )
        .unwrap(),
        7
    );
    tx.commit().unwrap();
    let report = log!(out, "verify", s.audit_verify().unwrap());
    assert!(report.broken.is_none());
    assert_eq!(
        (report.checked, report.first_id, report.last_id),
        (7, Some(1), Some(7))
    );
    // 兩個後端對相同的事件算出相同的雜湊
    let (rows, total) = s.audit_query(&AuditFilter::default(), 100).unwrap();
    assert_eq!(total, 7);
    log!(out, "rows", &rows);
    log!(out, "tail", s.audit_tail().unwrap());
    // 篩選
    let q = |f: AuditFilter| -> (Vec<i64>, u64) {
        let (rows, total) = s.audit_query(&f, 3).unwrap();
        (rows.iter().map(|r| r.id).collect(), total)
    };
    log!(
        out,
        "actor",
        q(AuditFilter {
            actor: Some("user:ann".into()),
            ..Default::default()
        })
    );
    log!(
        out,
        "category",
        q(AuditFilter {
            category: Some("auth".into()),
            ..Default::default()
        })
    );
    log!(
        out,
        "action",
        q(AuditFilter {
            action: Some("user.create".into()),
            ..Default::default()
        })
    );
    log!(
        out,
        "target",
        q(AuditFilter {
            target: Some("bob".into()),
            ..Default::default()
        })
    );
    log!(
        out,
        "target case",
        q(AuditFilter {
            target: Some("abc".into()),
            ..Default::default()
        })
    );
    log!(
        out,
        "result",
        q(AuditFilter {
            result: Some("failure".into()),
            ..Default::default()
        })
    );
    log!(
        out,
        "time",
        q(AuditFilter {
            from_ms: Some(T0 + 2),
            to_ms: Some(T0 + 6),
            ..Default::default()
        })
    );
    log!(
        out,
        "before",
        q(AuditFilter {
            before_id: Some(4),
            ..Default::default()
        })
    );
    log!(
        out,
        "after",
        q(AuditFilter {
            after_id: Some(4),
            ..Default::default()
        })
    );
    log!(
        out,
        "export",
        s.audit_export_batch(
            &AuditFilter {
                after_id: Some(2),
                ..Default::default()
            },
            3
        )
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect::<Vec<_>>()
    );
    // 竄改：驗證指出斷點
    db.exec("UPDATE audit_events SET actor = 'user:mallory' WHERE id = 3");
    let broken = log!(out, "tampered", s.audit_verify().unwrap());
    assert_eq!(broken.broken.as_ref().map(|b| b.id), Some(3));
    db.exec("UPDATE audit_events SET actor = 'user:ann' WHERE id = 3");
    assert!(s.audit_verify().unwrap().broken.is_none());
    // 保留清除：刪除較舊的紀錄後鏈仍可驗證，id 繼續遞增
    log!(out, "purged", s.audit_purge(T0 + 3, T0 + 100).unwrap());
    let after = log!(out, "verify after purge", s.audit_verify().unwrap());
    assert!(after.broken.is_none(), "{after:?}");
    assert_eq!(
        s.audit_append(T0 + 200, &audit("user:ann", "user.update", None))
            .unwrap(),
        8
    );
    log!(out, "final", s.audit_verify().unwrap());
    log!(
        out,
        "ids",
        db.rows("SELECT id, prev_hash, hash FROM audit_events ORDER BY id")
    );
    log!(
        out,
        "meta",
        db.rows("SELECT key FROM meta WHERE key LIKE 'audit%' ORDER BY key")
    );
    out
}

fn audit_hmac(db: &TestDb) -> String {
    let mut out = String::new();
    let s = db.handle();
    // 還沒有金鑰時的紀錄只有雜湊鏈
    s.audit_append(T0, &audit("user:ann", "user.create", None))
        .unwrap();
    let key = crate::audit::ensure_key(&db.dir).unwrap();
    for i in 1..4 {
        s.audit_append(T0 + i, &audit("user:ann", "user.update", None))
            .unwrap();
    }
    assert_eq!(
        s.audit_hmac_since().unwrap(),
        Some((2, key.key_id().to_string()))
    );
    let r = s.audit_verify().unwrap();
    assert!(r.broken.is_none(), "{r:?}");
    log!(
        out,
        "hmac",
        (
            r.hmac.since_id,
            r.hmac.status,
            r.hmac.verified,
            r.hmac.chain_only,
            r.checked
        )
    );
    assert_eq!(r.hmac.status, "verified");
    assert_eq!(
        db.int("SELECT count(*) FROM audit_events WHERE hmac IS NOT NULL"),
        3
    );
    // 竄改後重算雜湊鏈也騙不過 HMAC
    db.exec("UPDATE audit_events SET hmac = 'AAAA' WHERE id = 3");
    let r = s.audit_verify().unwrap();
    log!(
        out,
        "forged",
        (r.broken.as_ref().map(|b| (b.id, b.reason)), r.hmac.status)
    );
    assert!(r.broken.is_some() || r.hmac.status != "verified");
    out
}

fn users_and_sessions(db: &TestDb) -> String {
    let mut out = String::new();
    let s = db.handle();
    let new = |name: &'static str| NewUser {
        username: name,
        display_name: Some("顯示"),
        email: Some("a@example.com"),
        source: "local",
        ldap_dn: None,
        password_hash: Some("$argon2id$x"),
        must_change_password: true,
    };
    let ann = s.create_user(&new("Ann"), "system", T0).unwrap().unwrap();
    let bob = s
        .create_user(&new("bob"), "user:Ann", T0 + 1)
        .unwrap()
        .unwrap();
    assert_eq!((ann, bob), (1, 2));
    // 大小寫不同的同名被拒
    assert_eq!(s.create_user(&new("ANN"), "system", T0 + 2).unwrap(), None);
    assert_eq!(s.find_user("ann").unwrap().map(|u| u.user_id), Some(ann));
    assert_eq!(
        s.find_user("ANN").unwrap().map(|u| u.username),
        Some("Ann".to_string())
    );
    assert_eq!(s.find_user("nobody").unwrap(), None);
    let carol = s
        .create_user(&new("carol"), "system", T0 + 3)
        .unwrap()
        .unwrap();
    log!(out, "users", s.list_users().unwrap());
    log!(out, "get", s.get_user(bob).unwrap());
    assert_eq!(
        s.password_hash(ann).unwrap().as_deref(),
        Some("$argon2id$x")
    );
    // LDAP
    let ldap = log!(
        out,
        "ldap new",
        s.upsert_ldap_user("Dave", "cn=dave,dc=x", Some("Dave"), None, T0 + 4)
            .unwrap()
    );
    assert_eq!(
        log!(
            out,
            "ldap again",
            s.upsert_ldap_user("dave", "cn=dave2,dc=x", None, Some("d@x"), T0 + 5)
                .unwrap()
        ),
        ldap
    );
    log!(
        out,
        "ldap vs local",
        s.upsert_ldap_user("ann", "cn=ann,dc=x", None, None, T0 + 6)
            .unwrap()
    );
    log!(out, "ldap row", s.find_user("DAVE").unwrap());
    // 密碼、個人資料、狀態
    assert!(s.set_password(bob, "$argon2id$y", false, T0 + 10).unwrap());
    assert!(!s.set_password(999, "$z", false, T0).unwrap());
    assert!(
        s.rehash_password(bob, "$argon2id$y", "$argon2id$y2")
            .unwrap()
    );
    assert!(
        !s.rehash_password(bob, "$argon2id$y", "$argon2id$y3")
            .unwrap()
    );
    assert!(
        s.set_profile(bob, None, Some("bob@example.com"), T0 + 11)
            .unwrap()
    );
    assert!(
        s.set_state(bob, false, "user:Ann", Some("離職"), T0 + 12)
            .unwrap()
    );
    log!(out, "bob", s.get_user(bob).unwrap());
    assert!(s.set_state(bob, true, "user:Ann", None, T0 + 13).unwrap());
    // 登入失敗計數與鎖定
    assert_eq!(s.login_failed(carol, 3, 60_000, T0 + 20).unwrap(), None);
    assert_eq!(s.login_failed(carol, 3, 60_000, T0 + 21).unwrap(), None);
    let locked = log!(
        out,
        "locked",
        s.login_failed(carol, 3, 60_000, T0 + 22).unwrap()
    );
    assert_eq!(locked, Some(T0 + 22 + 60_000));
    log!(out, "carol locked", s.get_user(carol).unwrap());
    assert!(s.unlock(carol).unwrap());
    assert!(!s.unlock(carol).unwrap());
    s.login_succeeded(carol, "10.0.0.7", T0 + 30).unwrap();
    log!(out, "carol", s.get_user(carol).unwrap());
    // TOTP
    assert_eq!(s.totp_state(ann).unwrap(), Some((None, None, None)));
    s.set_totp_pending(ann, b"sealed-secret").unwrap();
    assert!(!s.clear_totp(bob, T0).unwrap());
    assert!(
        s.enable_totp(ann, 100, &[vec![1; 32], vec![2; 32]], T0 + 40)
            .unwrap()
    );
    log!(out, "totp", s.totp_state(ann).unwrap());
    assert!(!s.use_totp_step(ann, 100).unwrap());
    assert!(s.use_totp_step(ann, 101).unwrap());
    assert!(!s.use_totp_step(ann, 101).unwrap());
    assert!(s.use_recovery_code(ann, &[1; 32], T0 + 41).unwrap());
    assert!(!s.use_recovery_code(ann, &[1; 32], T0 + 42).unwrap());
    assert!(!s.use_recovery_code(ann, &[9; 32], T0 + 42).unwrap());
    assert!(s.clear_totp(ann, T0 + 43).unwrap());
    assert!(!s.clear_totp(ann, T0 + 44).unwrap());
    log!(out, "totp cleared", s.totp_state(ann).unwrap());
    assert_eq!(db.int("SELECT count(*) FROM user_recovery_codes"), 0);
    // 角色指派
    let admin = Assignment {
        role: Role::Admin,
        scope: Scope::Global,
    };
    let op = Assignment {
        role: Role::Operator,
        scope: Scope::Group("Web".into()),
    };
    let viewer = Assignment {
        role: Role::Viewer,
        scope: Scope::Global,
    };
    assert!(
        s.add_assignment("user", &ann.to_string(), &admin, "system", T0)
            .unwrap()
    );
    assert!(
        !s.add_assignment("user", &ann.to_string(), &admin, "system", T0)
            .unwrap()
    );
    assert!(
        s.add_assignment("user", &bob.to_string(), &op, "user:Ann", T0)
            .unwrap()
    );
    assert!(
        s.add_assignment("ldap_group", "cn=ops,dc=x", &viewer, "user:Ann", T0)
            .unwrap()
    );
    log!(out, "ann assignments", s.user_assignments(ann).unwrap());
    log!(out, "bob assignments", s.user_assignments(bob).unwrap());
    let (g, src) = s.grants_for(bob, &["cn=ops,dc=x".to_string()]).unwrap();
    log!(out, "grants", format!("{g:?} {src:?}"));
    log!(out, "ldap mappings", s.ldap_mappings().unwrap());
    log!(out, "admins", s.active_global_admins().unwrap());
    log!(out, "role counts", s.role_member_counts().unwrap());
    log!(
        out,
        "replace",
        s.replace_assignments(
            "user",
            &bob.to_string(),
            &[viewer.clone(), op.clone()],
            "user:Ann",
            T0 + 50
        )
        .unwrap()
    );
    assert!(s.remove_assignment("user", &bob.to_string(), &op).unwrap());
    assert!(!s.remove_assignment("user", &bob.to_string(), &op).unwrap());
    log!(out, "bob after", s.user_assignments(bob).unwrap());
    // 工作階段
    let groups = vec!["cn=ops,dc=x".to_string()];
    let session = |hash: &'static [u8], public: &'static str, user: i64| NewSession {
        session_hash: hash,
        public_id: public,
        user_id: user,
        method: "password",
        ldap_groups: if user == 2 { &[] } else { &[] },
        idle_timeout_ms: 1_800_000,
        expires_at: T0 + DAY,
        ip: "10.0.0.5",
        user_agent: Some("curl/8"),
    };
    let _ = &groups;
    s.create_session(
        &session(b"hash-a1-0000000000000000000000000", "pub-a1", ann),
        T0 + 100,
    )
    .unwrap();
    s.create_session(
        &session(b"hash-a2-0000000000000000000000000", "pub-a2", ann),
        T0 + 101,
    )
    .unwrap();
    s.create_session(
        &session(b"hash-a3-0000000000000000000000000", "pub-a3", ann),
        T0 + 102,
    )
    .unwrap();
    s.create_session(
        &session(b"hash-b1-0000000000000000000000000", "pub-b1", bob),
        T0 + 103,
    )
    .unwrap();
    log!(
        out,
        "session",
        s.session_by_hash(b"hash-a1-0000000000000000000000000")
            .unwrap()
    );
    assert_eq!(s.session_by_hash(b"nope").unwrap(), None);
    s.touch_session(b"hash-a1-0000000000000000000000000", T0 + 500)
        .unwrap();
    s.touch_session(b"hash-a1-0000000000000000000000000", T0 + 400)
        .unwrap();
    assert_eq!(
        s.session_by_hash(b"hash-a1-0000000000000000000000000")
            .unwrap()
            .map(|x| x.last_seen_at),
        Some(T0 + 500)
    );
    assert!(
        s.end_session(
            b"hash-a2-0000000000000000000000000",
            "logout",
            "user:Ann",
            T0 + 600
        )
        .unwrap()
    );
    assert!(
        !s.end_session(
            b"hash-a2-0000000000000000000000000",
            "logout",
            "user:Ann",
            T0 + 601
        )
        .unwrap()
    );
    assert!(
        !s.end_session_by_public_id(bob, "pub-a3", "revoked", "user:Ann", T0 + 602)
            .unwrap()
    );
    log!(out, "active", s.user_sessions(ann, true, T0 + 700).unwrap());
    log!(
        out,
        "all",
        s.user_sessions(ann, false, T0 + 700).unwrap().len()
    );
    // 除了目前這個之外全部結束：回傳仍有效的數量
    assert_eq!(
        log!(
            out,
            "end others",
            s.end_user_sessions(
                ann,
                Some(b"hash-a1-0000000000000000000000000"),
                "password_changed",
                "user:Ann",
                T0 + 800
            )
            .unwrap()
        ),
        1
    );
    assert_eq!(
        s.end_user_sessions(ann, None, "disabled", "user:Ann", T0 + 900)
            .unwrap(),
        1
    );
    assert_eq!(
        s.end_user_sessions(ann, None, "disabled", "user:Ann", T0 + 901)
            .unwrap(),
        0
    );
    assert!(
        s.end_session_by_public_id(bob, "pub-b1", "revoked", "user:Ann", T0 + 902)
            .unwrap()
    );
    log!(out, "sessions", db.rows("SELECT public_id, user_id, ended_at, end_reason, ended_by, last_seen_at FROM sessions ORDER BY public_id"));
    log!(
        out,
        "purge sessions",
        s.purge_sessions(T0 + 40 * DAY).unwrap()
    );
    // 刪除使用者
    assert!(s.delete_user(bob, "user:Ann", T0 + 1000).unwrap());
    assert!(!s.delete_user(bob, "user:Ann", T0 + 1001).unwrap());
    log!(out, "final users", s.list_users().unwrap());
    log!(out, "final assignments", db.rows("SELECT subject_type, subject, role, scope_type, scope_value FROM role_assignments ORDER BY 1, 2, 3"));
    // 刪除後同名可以重新建立，編號不重複使用（PostgreSQL 的序列在被拒的建立時也會前進，編號可能有空號）
    assert!(
        s.create_user(&new("BOB"), "system", T0 + 1100)
            .unwrap()
            .unwrap()
            >= 5
    );
    out
}

fn api_tokens(db: &TestDb) -> String {
    let mut out = String::new();
    let s = db.handle();
    let owner = s
        .create_user(
            &NewUser {
                username: "ann",
                display_name: None,
                email: None,
                source: "local",
                ldap_dn: None,
                password_hash: None,
                must_change_password: false,
            },
            "system",
            T0,
        )
        .unwrap();
    s.insert_api_token(
        "tok_b",
        "備份",
        &[7; 32],
        "read",
        T0 + 1,
        Some("user:ann"),
        Some(T0 + DAY),
        Some("viewer"),
        owner,
    )
    .unwrap();
    s.insert_api_token(
        "tok_a",
        "CI",
        &[8; 32],
        "admin",
        T0 + 1,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    // 重複的 token_id：唯一鍵
    let e = s
        .insert_api_token("tok_a", "dup", &[9; 32], "read", T0, None, None, None, None)
        .unwrap_err();
    assert_eq!(e.kind(), StoreErrorKind::Constraint, "{e}");
    log!(out, "list", s.list_api_tokens().unwrap());
    log!(out, "get", s.get_api_token("tok_b").unwrap());
    assert_eq!(s.get_api_token("nope").unwrap(), None);
    log!(out, "lookup", s.lookup_api_token("tok_b").unwrap());
    let pending = s.api_tokens_pending_audit().unwrap();
    log!(
        out,
        "pending",
        pending
            .iter()
            .map(|(t, f)| (t.token_id.clone(), *f))
            .collect::<Vec<_>>()
    );
    s.mark_api_token_logged("tok_a", 1).unwrap();
    assert_eq!(
        s.revoke_api_token("tok_a", T0 + 5, Some("user:ann"), Some("外洩"))
            .unwrap(),
        1
    );
    assert_eq!(s.revoke_api_token("tok_a", T0 + 6, None, None).unwrap(), 0);
    let pending = s.api_tokens_pending_audit().unwrap();
    log!(
        out,
        "pending after revoke",
        pending
            .iter()
            .map(|(t, f)| (t.token_id.clone(), *f))
            .collect::<Vec<_>>()
    );
    s.mark_api_token_logged("tok_a", 2).unwrap();
    s.mark_api_token_logged("tok_b", 1).unwrap();
    assert!(s.api_tokens_pending_audit().unwrap().is_empty());
    s.store_api_token_last_used(&[
        ("tok_b".into(), T0 + 100, "10.0.0.1".into()),
        ("tok_a".into(), T0 + 90, "10.0.0.2".into()),
        ("tok_b".into(), T0 + 50, "10.0.0.3".into()),
        ("missing".into(), T0, "x".into()),
    ])
    .unwrap();
    log!(out, "final", s.list_api_tokens().unwrap());
    out
}

fn cert(serial: &str, ca: Option<i64>, agent: &str, issued: i64) -> CertRow {
    CertRow {
        serial: serial.into(),
        ca_id: ca,
        agent_id: agent.into(),
        spki_sha256: vec![issued as u8; 32],
        not_before: issued,
        not_after: issued + 90 * DAY,
        cert_der: vec![1, 2, 3, issued as u8],
        issued_at: issued,
        issued_via: "enroll".into(),
        request_id: Some(format!("req-{serial}")),
        approved_by: Some("user:ann".into()),
        first_used_at: None,
        revoked_at: None,
        revoke_effective_at: None,
        revocation_reason: None,
        revoked_by: None,
        revoke_comment: None,
        crl_number: None,
    }
}

fn pki(db: &TestDb) -> String {
    let mut out = String::new();
    let s = db.handle();
    assert_eq!(s.next_ca_id().unwrap(), 1);
    assert!(s.list_cas().unwrap().is_empty());
    let subject = |id: i64| format!("CN=Kairos Issuing CA {id}");
    let key = |id: i64| format!("issuing-{id}.key");
    let ca1 = s.insert_pending_ca(&subject, &[1; 32], &key, T0).unwrap();
    let ca2 = s
        .insert_pending_ca(&subject, &[2; 32], &key, T0 + 1)
        .unwrap();
    assert_eq!((ca1, ca2), (1, 2));
    assert_eq!(s.next_ca_id().unwrap(), 3);
    log!(out, "pending", s.list_cas().unwrap());
    assert_eq!(
        s.find_ca_by_spki(&[2; 32]).unwrap().map(|c| c.ca_id),
        Some(2)
    );
    assert_eq!(s.find_ca_by_spki(&[9; 32]).unwrap(), None);
    log!(
        out,
        "install 1",
        s.install_ca(ca1, "CN=CA 1", &[9, 9], T0, T0 + 3650 * DAY)
            .unwrap()
    );
    log!(
        out,
        "install 2",
        s.install_ca(ca2, "CN=CA 2", &[8, 8], T0, T0 + 3650 * DAY)
            .unwrap()
    );
    log!(out, "installed", s.list_cas().unwrap());
    s.activate_ca(ca2).unwrap();
    log!(out, "activated", s.list_cas().unwrap());
    log!(out, "anchors", s.trust_anchor_cas().unwrap());
    s.set_ca_key_file(ca1, "moved.key").unwrap();
    s.retire_ca(ca1, T0 + 5).unwrap();
    log!(out, "retired", s.get_ca(ca1).unwrap());
    log!(
        out,
        "anchors after retire",
        s.trust_anchor_cas()
            .unwrap()
            .iter()
            .map(|c| c.ca_id)
            .collect::<Vec<_>>()
    );
    // 憑證
    for (i, (serial, agent)) in [
        ("0a", "agent-1"),
        ("0b", "agent-1"),
        ("0c", "agent-2"),
        ("0d", "agent-3"),
        ("0e", "agent-3"),
    ]
    .into_iter()
    .enumerate()
    {
        s.insert_cert(&cert(serial, Some(ca2), agent, T0 + 10 + (i as i64 / 2)))
            .unwrap();
    }
    s.insert_cert(&cert("ff", None, "agent-9", T0 + 20))
        .unwrap();
    assert!(
        s.insert_cert(&cert("0a", Some(ca2), "agent-x", T0))
            .is_err()
    );
    assert!(s.serial_exists("0a").unwrap() && !s.serial_exists("zz").unwrap());
    log!(out, "get", s.get_cert("0b").unwrap());
    log!(
        out,
        "of agent",
        s.certs_of_agent("agent-1")
            .unwrap()
            .iter()
            .map(|c| c.serial.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        s.spki_used_by_agent("agent-1", &cert("0a", None, "", T0 + 10).spki_sha256)
            .unwrap()
    );
    assert!(!s.spki_used_by_agent("agent-2", &[77; 32]).unwrap());
    // 分頁：依 (issued_at, serial) 由新到舊
    let page1 = s.list_certs(2, None).unwrap();
    log!(
        out,
        "page1",
        page1
            .iter()
            .map(|c| (c.serial.clone(), c.issued_at))
            .collect::<Vec<_>>()
    );
    let last = page1.last().unwrap();
    let page2 = s
        .list_certs(3, Some((last.issued_at, &last.serial)))
        .unwrap();
    log!(
        out,
        "page2",
        page2
            .iter()
            .map(|c| (c.serial.clone(), c.issued_at))
            .collect::<Vec<_>>()
    );
    let last = page2.last().unwrap();
    let page3 = s
        .list_certs(10, Some((last.issued_at, &last.serial)))
        .unwrap();
    log!(
        out,
        "page3",
        page3
            .iter()
            .map(|c| (c.serial.clone(), c.issued_at))
            .collect::<Vec<_>>()
    );
    assert_eq!(page1.len() + page2.len() + page3.len(), 6);
    assert!(s.mark_first_used("0b", T0 + 30).unwrap());
    assert!(!s.mark_first_used("0b", T0 + 31).unwrap());
    log!(out, "older", s.older_unrevoked("agent-1", "0b").unwrap());
    assert_eq!(s.issued_count_by_ca(ca2).unwrap(), 5);
    // 撤銷與 CRL
    assert!(s.revoked_serials().unwrap().is_empty());
    log!(
        out,
        "revoke",
        s.revoke(
            "0a",
            T0 + 40,
            T0 + 40,
            Reason::Superseded,
            "user:ann",
            Some("換新")
        )
        .unwrap()
    );
    log!(
        out,
        "revoke again",
        s.revoke(
            "0a",
            T0 + 41,
            T0 + 41,
            Reason::KeyCompromise,
            "user:bob",
            None
        )
        .unwrap()
    );
    log!(
        out,
        "revoke missing",
        s.revoke("zz", T0, T0, Reason::Unspecified, "x", None)
            .unwrap()
    );
    // 延後生效的撤銷
    s.revoke(
        "0c",
        T0 + 42,
        T0 + 5000,
        Reason::CessationOfOperation,
        "user:ann",
        None,
    )
    .unwrap();
    s.revoke(
        "ff",
        T0 + 43,
        T0 + 43,
        Reason::Unspecified,
        "user:ann",
        None,
    )
    .unwrap();
    log!(out, "revoked serials", {
        let mut v: Vec<_> = s.revoked_serials().unwrap().into_iter().collect();
        v.sort();
        v
    });
    let entries = log!(out, "for crl", s.revocations_for_crl(ca2, T0 + 50).unwrap());
    log!(
        out,
        "pending cas",
        s.ca_ids_with_pending_revocations(T0 + 50).unwrap()
    );
    assert_eq!(s.latest_crl(ca2).unwrap(), None);
    let n = s.get_ca(ca2).unwrap().unwrap().next_crl_number;
    s.record_crl(
        ca2,
        n,
        T0 + 50,
        T0 + 50 + DAY,
        &entries,
        &[5, 5, 5],
        "scheduled",
        None,
    )
    .unwrap();
    // 同一個編號不能記錄兩次
    assert!(
        s.record_crl(
            ca2,
            n,
            T0 + 51,
            T0 + 51 + DAY,
            &entries,
            &[6],
            "manual",
            Some("user:ann")
        )
        .is_err()
    );
    log!(
        out,
        "pending cas after crl",
        s.ca_ids_with_pending_revocations(T0 + 50).unwrap()
    );
    log!(
        out,
        "pending cas later",
        s.ca_ids_with_pending_revocations(T0 + 6000).unwrap()
    );
    let later = s.revocations_for_crl(ca2, T0 + 6000).unwrap();
    let n2 = s.get_ca(ca2).unwrap().unwrap().next_crl_number;
    assert_eq!(n2, n + 1);
    s.record_crl(
        ca2,
        n2 + 5,
        T0 + 6000,
        T0 + 6000 + DAY,
        &later,
        &[7],
        "manual",
        Some("user:ann"),
    )
    .unwrap();
    assert_eq!(s.get_ca(ca2).unwrap().unwrap().next_crl_number, n2 + 6);
    log!(out, "latest", s.latest_crl(ca2).unwrap());
    log!(
        out,
        "history",
        s.crl_history(ca2, 10)
            .unwrap()
            .iter()
            .map(|c| c.crl_number)
            .collect::<Vec<_>>()
    );
    log!(out, "cert crl numbers", db.rows("SELECT serial, crl_number, revoked_at, revoke_effective_at, revocation_reason FROM pki_certificates ORDER BY serial"));
    log!(out, "external", {
        let mut v: Vec<_> = s
            .revoked_external_serials(T0 + 100, T0)
            .unwrap()
            .into_iter()
            .collect();
        v.sort();
        v
    });
    let _ = RevokedEntry {
        serial: "x".into(),
        revoked_at_ms: 0,
        reason: Reason::Unspecified,
    };
    // 主機目前的憑證
    s.insert_imported_host("agent-9", T0 + 60, "ff").unwrap();
    log!(out, "current", s.host_current_cert("agent-9").unwrap());
    assert_eq!(s.host_current_cert("nobody").unwrap(), None);
    s.set_host_current_serial("agent-9", "0e").unwrap();
    log!(
        out,
        "current after",
        s.host_current_cert("agent-9").unwrap()
    );
    // 沒有主機的過期憑證
    log!(
        out,
        "orphans",
        s.delete_orphan_certificates(T0 + 400 * DAY).unwrap()
    );
    log!(
        out,
        "left",
        db.rows("SELECT serial FROM pki_certificates ORDER BY serial")
    );
    // 撤銷一台主機所有有效的憑證（含稽核）
    let tx = s.begin(TxMode::Immediate).unwrap();
    log!(
        out,
        "revoke active",
        tx.revoke_active_certificates("agent-3", "user:ann", Some("10.0.0.1"), T0 + 70)
            .unwrap()
    );
    tx.commit().unwrap();
    log!(out, "audit", db.rows("SELECT id, actor, category, action, object, result, details FROM audit_events ORDER BY id"));
    out
}

fn token(id: &str, max_uses: i64) -> TokenRow {
    TokenRow {
        token_id: id.into(),
        secret_hash: id.as_bytes().to_vec(),
        hint: "abcd".into(),
        name: format!("token {id}"),
        group_name: "Web".into(),
        tags: Some(vec!["env:prod".into()]),
        approval: TokenApproval::Auto,
        max_uses,
        used: 0,
        expires_at: T0 + 7 * DAY,
        allowed_cidrs: Some(vec!["10.0.0.0/8".into()]),
        batch_mode: true,
        allowed_agent_ids: None,
        cert_lifetime_days: Some(30),
        created_at: T0,
        created_by: "user:ann".into(),
        disabled_at: None,
        disabled_by: None,
        disable_reason: None,
        last_used_at: None,
    }
}

fn request(id: &str, agent: &str, created: i64) -> RequestRow {
    RequestRow {
        request_id: id.into(),
        kind: DbKind::Enroll,
        agent_id: agent.into(),
        request_code: format!("CODE-{id}"),
        state: DbState::PendingApproval,
        csr_der: vec![1, 2, 3],
        csr_sha256: id.as_bytes().to_vec(),
        spki_sha256: vec![4; 32],
        poll_secret_hash: Some(vec![5; 32]),
        token_id: Some("t1".into()),
        expired_serial: None,
        previous_serial: None,
        source_ip: "10.0.0.50".into(),
        hostname: format!("host-{agent}"),
        display_name: Some("顯示".into()),
        tags: Some(vec!["a".into(), "b".into()]),
        os: Some("linux".into()),
        host_fingerprint: Some("sha256:ff".into()),
        pulse_version: Some("0.9.0".into()),
        server_verification: None,
        flags: Vec::new(),
        cert_lifetime_days: None,
        created_at: created,
        expires_at: created + DAY,
        decided_at: None,
        decided_by: None,
        reject_reason_code: None,
        reject_reason: None,
        issued_serial: None,
        issue_error: None,
        delivered_at: None,
        withdrawn_at: None,
        withdraw_reason: None,
        withdraw_initiated_by: None,
        withdraw_ip: None,
        closed_at: None,
    }
}

fn enroll(db: &TestDb) -> String {
    let mut out = String::new();
    let s = db.handle();
    // token
    s.insert_token(&token("t1", 2)).unwrap();
    s.insert_token(&TokenRow {
        batch_mode: false,
        allowed_cidrs: None,
        tags: None,
        approval: TokenApproval::Manual,
        ..token("t2", 1)
    })
    .unwrap();
    log!(out, "tokens", s.list_tokens().unwrap());
    assert_eq!(
        s.token_by_hash(b"t1").unwrap().map(|t| t.token_id),
        Some("t1".into())
    );
    assert_eq!(s.token_by_hash(b"zz").unwrap(), None);
    log!(out, "validity", s.token_validity(T0 + 1).unwrap());
    log!(out, "cidrs", s.batch_token_cidrs(T0 + 1).unwrap());
    // 扣次數到用完：不會超過 max_uses
    for i in 0..2 {
        s.consume_token("t1", T0 + 10 + i).unwrap();
    }
    let t1 = log!(out, "t1 used", s.get_token("t1").unwrap()).unwrap();
    assert_eq!(t1.used, 2);
    log!(
        out,
        "disable",
        s.disable_token("t2", "user:ann", Some("外洩"), T0 + 20)
            .unwrap()
    );
    log!(
        out,
        "disable again",
        s.disable_token("t2", "user:bob", None, T0 + 21)
            .unwrap()
            .map(|(t, already)| (t.disabled_by, t.disabled_at, already))
    );
    assert_eq!(
        s.disable_token("nope", "x", None, T0).unwrap().map(|x| x.1),
        None
    );
    log!(out, "validity after", s.token_validity(T0 + 30).unwrap());
    // 申請
    for (id, agent, at) in [
        ("r1", "agent-1", T0 + 100),
        ("r2", "agent-2", T0 + 101),
        ("r3", "agent-1", T0 + 102),
        ("r4", "agent-3", T0 + 103),
    ] {
        s.insert_request(&request(id, agent, at)).unwrap();
    }
    assert!(s.insert_request(&request("r1", "agent-x", T0)).is_err());
    log!(out, "get", s.get_request("r1").unwrap());
    assert_eq!(
        s.request_by_csr("agent-1", b"r3")
            .unwrap()
            .map(|r| r.request_id),
        Some("r3".into())
    );
    assert_eq!(s.request_by_csr("agent-2", b"r3").unwrap(), None);
    assert_eq!(s.requests_by_code("CODE-r2").unwrap().len(), 1);
    assert_eq!(s.pending_count().unwrap(), 4);
    assert_eq!(s.pending_count_from("10.0.0.50").unwrap(), 4);
    assert_eq!(s.recent_count_from("10.0.0.50", T0 + 102).unwrap(), 2);
    assert!(s.hostname_in_use("host-agent-1", "agent-2").unwrap());
    assert!(!s.hostname_in_use("host-agent-1", "agent-1").unwrap());
    // 同一個 agent 的舊申請被新的取代
    log!(
        out,
        "replaced",
        s.replace_pending_for_agent("agent-1", "r3", T0 + 110)
            .unwrap()
    );
    s.update_poll_secret("r3", &[6; 32]).unwrap();
    s.add_flag("r3", "hostname_conflict").unwrap();
    s.add_flag("r3", "fingerprint_changed").unwrap();
    s.add_flag("r3", "hostname_conflict").unwrap();
    log!(
        out,
        "flags",
        s.get_request("r3")
            .unwrap()
            .map(|r| (r.flags, r.poll_secret_hash))
    );
    // 核准 → 簽發 → 送達
    assert!(s.approve("r3", "user:ann", Some(45), T0 + 120).unwrap());
    assert!(!s.approve("r3", "user:bob", None, T0 + 121).unwrap());
    log!(
        out,
        "approved",
        s.approved_requests(10)
            .unwrap()
            .iter()
            .map(|r| (r.request_id.clone(), r.cert_lifetime_days))
            .collect::<Vec<_>>()
    );
    log!(out, "stale", s.stale_approvals(T0 + 200).unwrap());
    s.set_issue_error("r3", "CA 私鑰無法使用").unwrap();
    assert!(s.mark_issued("r3", "0a", "user:ann", T0 + 130).unwrap());
    assert!(!s.mark_issued("r3", "0b", "user:ann", T0 + 131).unwrap());
    s.mark_delivered("r3", T0 + 140).unwrap();
    log!(out, "issued", s.get_request("r3").unwrap());
    // 拒絕、撤回
    assert!(
        s.reject("r2", "user:ann", "policy", "不在名單", T0 + 150)
            .unwrap()
    );
    assert!(
        !s.reject("r2", "user:ann", "policy", "again", T0 + 151)
            .unwrap()
    );
    assert!(
        s.withdraw("r4", "operator", "agent", "10.0.0.50", T0 + 160)
            .unwrap()
    );
    assert!(
        !s.withdraw("r4", "operator", "agent", "10.0.0.50", T0 + 161)
            .unwrap()
    );
    let list = |f: RequestFilter| {
        s.list_requests(&f)
            .unwrap()
            .iter()
            .map(|r| (r.request_id.clone(), r.state))
            .collect::<Vec<_>>()
    };
    log!(
        out,
        "open",
        list(RequestFilter {
            limit: 10,
            ..Default::default()
        })
    );
    log!(
        out,
        "closed",
        list(RequestFilter {
            closed: true,
            limit: 10,
            ..Default::default()
        })
    );
    log!(
        out,
        "closed limit",
        list(RequestFilter {
            closed: true,
            limit: 2,
            ..Default::default()
        })
    );
    log!(
        out,
        "by state",
        list(RequestFilter {
            closed: true,
            state: Some(DbState::Rejected),
            limit: 10,
            ..Default::default()
        })
    );
    log!(
        out,
        "by token",
        list(RequestFilter {
            closed: true,
            token_id: Some("t1".into()),
            kind: Some(DbKind::Enroll),
            limit: 10,
            ..Default::default()
        })
    );
    log!(out, "progress", s.token_progress_counts("t1").unwrap());
    log!(out, "pending enroll", s.pending_enroll_count().unwrap());
    // 過期與清除
    s.insert_request(&request("r5", "agent-5", T0 + 170))
        .unwrap();
    log!(
        out,
        "expire",
        s.expire_and_purge(T0 + 170 + 2 * DAY, 30 * DAY).unwrap()
    );
    log!(
        out,
        "r5",
        s.get_request("r5").unwrap().map(|r| (r.state, r.closed_at))
    );
    log!(
        out,
        "purge all",
        s.expire_and_purge(T0 + 100 * DAY, DAY).unwrap()
    );
    log!(out, "left", db.int("SELECT count(*) FROM enroll_requests"));
    // 重放快取
    assert!(!s.replay_seen(b"k1", T0).unwrap());
    s.replay_insert(b"k1", "agent-1", T0, 1000).unwrap();
    s.replay_insert(b"k2", "agent-2", T0, 5000).unwrap();
    assert!(s.replay_seen(b"k1", T0 + 500).unwrap());
    assert!(!s.replay_seen(b"k1", T0 + 1500).unwrap());
    s.replay_insert(b"k1", "agent-1", T0 + 1500, 1000).unwrap();
    assert!(s.replay_seen(b"k1", T0 + 2000).unwrap());
    log!(out, "replay count", s.replay_count(T0 + 2000).unwrap());
    log!(out, "replay purge", s.replay_purge(T0 + 3000).unwrap());
    log!(out, "replay left", s.replay_count(T0 + 3000).unwrap());
    // 封鎖清單
    let block = |source: &str, until: i64, offenses: i64| BlockRow {
        source: source.into(),
        class: "invalid".into(),
        blocked_until: until,
        offenses,
        last_offense_at: T0,
    };
    s.upsert_block(&block("10.0.0.1", T0 + 1000, 1)).unwrap();
    s.upsert_block(&block("10.0.0.2", T0 + 5000, 1)).unwrap();
    s.upsert_block(&block("10.0.0.1", T0 + 9000, 3)).unwrap();
    log!(out, "blocks", {
        let mut b = s.load_blocks().unwrap();
        b.sort_by(|a, b| a.source.cmp(&b.source));
        b
    });
    log!(out, "purge blocks", s.purge_blocks(T0 + 6000).unwrap());
    log!(out, "blocks left", s.load_blocks().unwrap());
    log!(
        out,
        "relaxations",
        (
            s.active_relaxations(T0).unwrap(),
            s.purge_relaxations(T0).unwrap()
        )
    );
    // 主機的註冊資訊
    assert_eq!(s.host_enroll("agent-1").unwrap(), None);
    s.upsert_host_on_issue(
        "agent-1",
        "web-01",
        Some("sha256:ff"),
        Some("顯示"),
        Some("[\"a\"]"),
        Some("linux"),
        "0a",
        T0 + 300,
    )
    .unwrap();
    s.upsert_host_on_issue("agent-1", "web-01b", None, None, None, None, "0b", T0 + 400)
        .unwrap();
    s.set_host_group_from_token("agent-1", Some("t1")).unwrap();
    assert!(s.set_grace_override("agent-1", Some(T0 + DAY)).unwrap());
    assert!(!s.set_grace_override("nobody", None).unwrap());
    s.set_last_grace_attempt("agent-1", &json!({"at": T0, "result": "denied"}))
        .unwrap();
    log!(out, "host enroll", s.host_enroll("agent-1").unwrap());
    log!(out, "host row", db.rows("SELECT agent_id, hostname, host_fingerprint, display_name, tags, os, host_group, first_seen, last_seen FROM hosts"));
    log!(out, "impact", s.impact_hosts().unwrap().len());
    log!(
        out,
        "expired certs",
        s.expired_cert_hosts(T0 + 500 * DAY).unwrap()
    );
    assert!(s.decommissioned_agents().unwrap().is_empty());
    log!(
        out,
        "decommission",
        s.decommission_host("agent-1", "web-01b", "user:ann", T0 + 500)
            .unwrap()
    );
    log!(
        out,
        "decommission again",
        s.decommission_host("agent-1", "web-01b", "user:ann", T0 + 501)
            .unwrap()
    );
    log!(
        out,
        "decommission unknown",
        s.decommission_host("agent-7", "new-host", "user:ann", T0 + 502)
            .unwrap()
    );
    log!(out, "decommissioned", {
        let mut v: Vec<_> = s.decommissioned_agents().unwrap().into_iter().collect();
        v.sort();
        v
    });
    log!(
        out,
        "purge tokens",
        s.purge_old_tokens(T0 + 400 * DAY).unwrap()
    );
    log!(out, "tokens left", s.list_tokens().unwrap().len());
    out
}

backend_tests!(
    settings_and_meta,
    transactions,
    audit_chain,
    audit_hmac,
    users_and_sessions,
    api_tokens,
    pki,
    enroll,
);
