//! ADR-0002 切片 S1～S3 的服務層測試：核准的確認字串與 digest（A1、A2）、三人邊界與可核准者（A3、A5）、
//! 狀態轉移的競態（A7）、版本庫檔案的 TOCTOU（A9）、刪除版本的保護（A10）、政策的改變（A12）、稽核（A13）

use super::*;
use crate::rbac::{Assignment, Role, Scope};
use crate::users::store::{NewUser, SOURCE_LOCAL, SUBJECT_USER};

fn user(name: &str) -> Actor {
    Actor {
        actor: format!("user:{name}"),
        ip: None,
    }
}

/// 啟用雙人覆核的服務：版本庫有 0.9.0 與 0.9.2，主機 A（tar）與 B（deb）在 0.9.0、已連線
async fn fixture(tag: &str, seed: u8) -> (Fx, super::test_sign::TestRelease) {
    let signer = TestSigner::new(seed);
    let fx = Fx::new(tag, |s, dir| {
        trust_settings(&signer)(s, dir);
        s.rollout_approval = "required".into();
    })
    .await;
    import(&fx, &release(&signer, "0.9.0", "1111111")).await;
    let r092 = release(&signer, "0.9.2", "2222222");
    import(&fx, &r092).await;
    fx.add_host(A, "0.9.0", "Web", &[], "tar", "ready").await;
    fx.add_host(B, "0.9.0", "Web", &[], "deb", "ready").await;
    fx.connect(A, "0.9.0", "tar");
    fx.connect(B, "0.9.0", "deb");
    (fx, r092)
}

fn body(name: &str) -> Value {
    json!({"name": name, "target_version": "0.9.2", "selector": {"agent_ids": [A, B]}, "waves": [{"size": "rest"}]})
}

async fn create(fx: &Fx, who: &Actor, body: &Value) -> String {
    fx.svc
        .create_rollout(who, RolloutSpec::parse(body).unwrap())
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn submit(fx: &Fx, who: &Actor, id: &str, confirm: &str) -> Value {
    fx.svc
        .submit_rollout(who, id.to_string(), Some(confirm.into()), None)
        .await
        .unwrap()
}

async fn state_of(fx: &Fx, id: &str) -> super::store::RolloutRow {
    let rid = id.to_string();
    fx.svc
        .db
        .read(move |s| Ok(s.get_rollout(&rid)?.unwrap()))
        .await
        .unwrap()
}

/// 稽核中某個動作最後一筆的 (執行者, details)
async fn last_audit(fx: &Fx, action: &'static str) -> Option<(String, Value)> {
    fx.sql(move |c| {
        c.query_row(
            "SELECT actor, details FROM audit_events WHERE action = ?1 ORDER BY id DESC LIMIT 1",
            [action],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .ok()
        .map(|(a, d)| {
            (
                a,
                d.and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or(Value::Null),
            )
        })
    })
    .await
}

async fn audit_count(fx: &Fx, action: &'static str) -> i64 {
    fx.sql(move |c| {
        c.query_row(
            "SELECT COUNT(*) FROM audit_events WHERE action = ?1",
            [action],
            |r| r.get(0),
        )
        .unwrap()
    })
    .await
}

/// 新增一位本機帳號並指派全站角色
async fn add_user(fx: &Fx, name: &'static str, role: Role) -> i64 {
    fx.svc
        .db
        .write(move |s| {
            let id = s
                .create_user(
                    &NewUser {
                        username: name,
                        source: SOURCE_LOCAL,
                        ..Default::default()
                    },
                    "test",
                    T0,
                )?
                .unwrap();
            s.add_assignment(
                SUBJECT_USER,
                &id.to_string(),
                &Assignment {
                    role,
                    scope: Scope::Global,
                },
                "test",
                T0,
            )?;
            Ok(id)
        })
        .await
        .unwrap()
}

/// 目前的 `submission`（主控台看到的）
async fn submission_json_of(fx: &Fx, id: &str) -> Value {
    let rid = id.to_string();
    let live = fx.svc.live_hellos();
    let dir = fx.svc.releases_dir.clone();
    let now = fx.svc.now_ms();
    fx.svc
        .in_tx(move |s| {
            let r = s.get_rollout(&rid)?.unwrap();
            let hosts = s.rollout_hosts(&rid)?;
            let cx = super::submission::PendingContext::load(s, &live, &dir, now)?;
            super::submission::submission_json(s, &r, &hosts, &cx)
        })
        .await
        .unwrap()
}

fn file_of(fx: &Fx, r: &super::test_sign::TestRelease, suffix: &str) -> PathBuf {
    let (_, data) = r.files.iter().find(|(n, _)| n.ends_with(suffix)).unwrap();
    releases::artifact_path(
        &fx.svc.releases_dir,
        &kairos_proto::release::sha256_hex(data),
    )
}

/// 重新上傳一個發佈檔（S25 的補檔）
async fn reupload(fx: &Fx, r: &super::test_sign::TestRelease, suffix: &str) {
    let (name, data) = r.files.iter().find(|(n, _)| n.ends_with(suffix)).unwrap();
    let mut up = releases::Upload::create(&fx.svc.releases_dir, 1 << 20).unwrap();
    up.write(data).unwrap();
    let (sha, size) = up.finish().unwrap();
    fx.svc
        .store_artifact(&who(), r.version.clone(), name.clone(), up, sha, size)
        .await
        .unwrap();
}

/// A1：核准的確認字串由伺服器計算並驗證——空的本文、空字串、舊的台數、降版的字串、送出用的字串都是 400
/// `confirmation_required` 且狀態不變；沒有 digest 是 400；前後空白比照 submit 去掉；稽核記下 digest
#[tokio::test]
async fn approve_requires_the_server_computed_phrase_and_digest() {
    let (fx, _) = fixture("a1", 41).await;
    let carol = user("carol");
    let id = create(&fx, &who(), &body("x")).await;
    let v = submit(&fx, &who(), &id, "升級 2 台").await;
    assert_eq!(v["state"], "pending_approval");
    let sub = submission_of(&fx, &id).await;
    assert_eq!(
        (
            sub.confirm_text().as_str(),
            sub.confirm_level(),
            sub.eligible
        ),
        ("核准 2 台", "L2", 2)
    );
    // 送出的回應就帶著 submission，與之後讀到的相同
    assert_eq!(v["submission"]["digest"], sub.digest.as_str());
    assert_eq!(v["submission"]["confirm_text"], "核准 2 台");
    assert_eq!(v["submission"]["waves"], json!([2]));
    let req = |confirm: Option<&str>, digest: Option<&str>| ApproveRequest {
        confirm: confirm.map(str::to_string),
        digest: digest.map(str::to_string),
        reason: None,
    };
    let d = sub.digest.clone();
    for (confirm, what) in [
        (None, "空的本文"),
        (Some(""), "空字串"),
        (Some("核准 1 台"), "舊的台數"),
        (Some("核准 20 台"), "不同的台數"),
        (Some("核准降版 2 台"), "非降版時用降版的字串"),
        (Some("升級 2 台"), "送出用的字串"),
        (Some("核准2台"), "少了空白"),
        (Some("核准 2 台。"), "多了標點"),
    ] {
        let e = fx
            .svc
            .approve_rollout(&carol, id.clone(), req(confirm, Some(&d)))
            .await
            .unwrap_err();
        assert_eq!(
            (e.status.as_u16(), e.code),
            (400, "confirmation_required"),
            "{what}"
        );
        let details = e.details.unwrap();
        assert_eq!(details["confirm_text"], "核准 2 台", "{what}");
        assert_eq!(details["confirm_level"], "L2", "{what}");
        assert_eq!(state_of(&fx, &id).await.state, "pending_approval", "{what}");
    }
    // 沒有本文（confirm 與 digest 都沒有）也是 confirmation_required
    let e = fx
        .svc
        .approve_rollout(&carol, id.clone(), ApproveRequest::default())
        .await
        .unwrap_err();
    assert_eq!((e.status.as_u16(), e.code), (400, "confirmation_required"));
    // 字串正確但沒有 digest：400；digest 不符：409 submission_changed
    let e = fx
        .svc
        .approve_rollout(&carol, id.clone(), req(Some("核准 2 台"), None))
        .await
        .unwrap_err();
    assert_eq!((e.status.as_u16(), e.code), (400, "invalid_parameter"));
    assert_eq!(e.details.unwrap()["parameter"], "digest");
    let e = fx
        .svc
        .approve_rollout(
            &carol,
            id.clone(),
            req(Some("核准 2 台"), Some(&"0".repeat(64))),
        )
        .await
        .unwrap_err();
    assert_eq!((e.status.as_u16(), e.code), (409, "submission_changed"));
    assert_eq!(e.details.unwrap()["digest"], d.as_str());
    assert_eq!(state_of(&fx, &id).await.state, "pending_approval");
    assert_eq!(audit_count(&fx, "rollout.approve").await, 0);
    // 前後空白去掉後比對（與 submit 相同）
    let v = fx
        .svc
        .approve_rollout(&carol, id.clone(), req(Some("  核准 2 台\n"), Some(&d)))
        .await
        .unwrap();
    assert_eq!(v["state"], "running");
    let (actor, details) = last_audit(&fx, "rollout.approve").await.unwrap();
    assert_eq!(actor, "user:carol");
    assert_eq!(details["digest"], d.as_str());
    assert_eq!(
        (
            details["eligible"].as_i64(),
            details["confirm_level"].as_str()
        ),
        (Some(2), Some("L2"))
    );
    assert_eq!(details["submitted_by"], "user:admin");
    // 送出的稽核也有同一個 digest
    let (_, details) = last_audit(&fx, "rollout.submit").await.unwrap();
    assert_eq!(details["digest"], d.as_str());
    // 重複的請求沒有副作用
    let e = fx
        .svc
        .approve_rollout(&carol, id.clone(), req(Some("核准 2 台"), Some(&d)))
        .await
        .unwrap_err();
    assert_eq!((e.status.as_u16(), e.code), (409, "invalid_state"));
    assert_eq!(audit_count(&fx, "rollout.approve").await, 1);
}

/// A2：核准者取得摘要後推送被修改並重新送出 → 舊的 digest 是 409 `submission_changed`；同樣的內容重新送出
/// 的 digest 也不同（`submitted_at` 不同）
#[tokio::test]
async fn a_digest_from_before_a_resubmission_is_rejected() {
    let (fx, _) = fixture("a2", 42).await;
    let carol = user("carol");
    let id = create(&fx, &who(), &body("x")).await;
    submit(&fx, &who(), &id, "升級 2 台").await;
    let seen = approval(&fx, &id).await;
    // 送審後內容被修改（只留一台）、重新送出
    fx.set_now(T0 + MIN);
    let mut smaller = body("x");
    smaller["selector"] = json!({"agent_ids": [A]});
    let v = fx
        .svc
        .update_rollout(&who(), id.clone(), RolloutSpec::parse(&smaller).unwrap())
        .await
        .unwrap();
    assert_eq!(v["state"], "draft");
    submit(&fx, &who(), &id, "升級 1 台").await;
    // 核准者送出他看到的那一份（2 台）：digest 不符，優先於確認字串的檢查
    let e = fx
        .svc
        .approve_rollout(&carol, id.clone(), seen.clone())
        .await
        .unwrap_err();
    assert_eq!((e.status.as_u16(), e.code), (409, "submission_changed"));
    // 只換新的確認字串、沿用舊的 digest 也不行
    let e = fx
        .svc
        .approve_rollout(
            &carol,
            id.clone(),
            ApproveRequest {
                confirm: Some("核准 1 台".into()),
                ..seen.clone()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "submission_changed");
    assert_eq!(state_of(&fx, &id).await.state, "pending_approval");
    // 同樣的內容退回後重新送出：digest 仍然不同
    let before = submission_of(&fx, &id).await.digest;
    fx.svc
        .reject_rollout(&carol, id.clone(), "再看一次".into())
        .await
        .unwrap();
    fx.set_now(T0 + 2 * MIN);
    submit(&fx, &who(), &id, "升級 1 台").await;
    let after = submission_of(&fx, &id).await.digest;
    assert_ne!(before, after);
    assert_ne!(seen.digest.as_deref(), Some(after.as_str()));
    approve(&fx, &carol, &id).await.unwrap();
}

/// A1（降版）：含降版的推送核准時是 L3 字串「核准降版 N 台」＋原因必填
#[tokio::test]
async fn approving_a_downgrade_needs_the_l3_phrase_and_a_reason() {
    let signer = TestSigner::new(43);
    let fx = Fx::new("a1d", |s, dir| {
        trust_settings(&signer)(s, dir);
        s.rollout_approval = "required".into();
    })
    .await;
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    fx.add_host(A, "0.9.3", "Web", &[], "tar", "ready").await;
    fx.connect(A, "0.9.3", "tar");
    let carol = user("carol");
    let spec = json!({"name": "降版", "target_version": "0.9.2", "selector": {"agent_ids": [A]}, "waves": [{"size": "rest"}], "options": {"allow_downgrade": true}});
    let id = create(&fx, &who(), &spec).await;
    fx.svc
        .submit_rollout(
            &who(),
            id.clone(),
            Some("降版 1 台".into()),
            Some("回到穩定版".into()),
        )
        .await
        .unwrap();
    let sub = submission_of(&fx, &id).await;
    assert_eq!(
        (
            sub.confirm_text().as_str(),
            sub.confirm_level(),
            sub.downgrade
        ),
        ("核准降版 1 台", "L3", 1)
    );
    let req = |confirm: &str, reason: Option<&str>| ApproveRequest {
        confirm: Some(confirm.into()),
        digest: Some(sub.digest.clone()),
        reason: reason.map(str::to_string),
    };
    // 降版時用非降版的字串
    let e = fx
        .svc
        .approve_rollout(&carol, id.clone(), req("核准 1 台", Some("好")))
        .await
        .unwrap_err();
    assert_eq!((e.status.as_u16(), e.code), (400, "confirmation_required"));
    let details = e.details.unwrap();
    assert_eq!(
        (
            details["confirm_text"].as_str(),
            details["confirm_level"].as_str()
        ),
        (Some("核准降版 1 台"), Some("L3"))
    );
    // 字串正確但沒有原因
    let e = fx
        .svc
        .approve_rollout(&carol, id.clone(), req("核准降版 1 台", None))
        .await
        .unwrap_err();
    assert_eq!((e.status.as_u16(), e.code), (400, "confirmation_required"));
    assert_eq!(state_of(&fx, &id).await.state, "pending_approval");
    let v = fx
        .svc
        .approve_rollout(&carol, id.clone(), req("核准降版 1 台", Some("已確認")))
        .await
        .unwrap();
    assert_eq!(v["state"], "running");
    let (_, details) = last_audit(&fx, "rollout.approve").await.unwrap();
    assert_eq!(
        (
            details["confirm_level"].as_str(),
            details["reason"].as_str(),
            details["downgrade"].as_i64()
        ),
        (Some("L3"), Some("已確認"), Some(1))
    );
}

/// A3、A5：建立者與送出者都不能核准（三人邊界）；`approvers_available` 由後端依帳號與權限計算，可核准者
/// 被停用或撤銷權限後數字改變
#[tokio::test]
async fn creator_and_submitter_cannot_approve_and_available_approvers_are_counted() {
    let (fx, _) = fixture("a3", 44).await;
    let (alice, bob, carol) = (user("alice"), user("bob"), user("carol"));
    add_user(&fx, "alice", Role::Admin).await;
    let bob_id = add_user(&fx, "bob", Role::Admin).await;
    add_user(&fx, "olga", Role::Operator).await;
    // 甲建立＋甲送出：甲 403；只有兩位管理員時乙可以核准（approvers_available = 1）
    let id = create(&fx, &alice, &body("x")).await;
    let v = submit(&fx, &alice, &id, "升級 2 台").await;
    assert_eq!(v["submission"]["approvers_available"], 1);
    assert!(
        !v["submission"]["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!("no_approvers"))
    );
    let e = approve(&fx, &alice, &id).await.unwrap_err();
    assert_eq!(
        (e.status.as_u16(), e.code, e.product_code),
        (403, "self_approval", Some("KRS-E6106"))
    );
    assert_eq!(
        e.details.unwrap(),
        json!({"created_by": "user:alice", "submitted_by": "user:alice"})
    );
    approve(&fx, &bob, &id).await.unwrap();
    fx.svc
        .cancel_rollout(&alice, id, "結束".into())
        .await
        .unwrap();

    // 甲建立＋乙送出：甲、乙都 403，只有兩位管理員時沒有人可以核准
    let id = create(&fx, &alice, &body("y")).await;
    let v = submit(&fx, &bob, &id, "升級 2 台").await;
    assert_eq!(v["submission"]["approvers_available"], 0);
    assert!(
        v["submission"]["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!("no_approvers"))
    );
    for a in [&alice, &bob] {
        let e = approve(&fx, a, &id).await.unwrap_err();
        assert_eq!((e.status.as_u16(), e.code), (403, "self_approval"));
        assert_eq!(
            e.details.unwrap(),
            json!({"created_by": "user:alice", "submitted_by": "user:bob"})
        );
    }
    // 建立時的預覽（以目前的使用者為建立者與送出者）：甲看到的是 1（乙）
    let p = fx
        .svc
        .preview_rollout(
            RolloutSpec::parse(
                &json!({"name": "p", "target_version": "0.9.2", "selector": {"agent_ids": [C]}}),
            )
            .unwrap(),
            None,
            Some("user:alice".into()),
        )
        .await
        .unwrap();
    assert_eq!(p["approvers_available"], 1);
    // 第三位管理員加入後可以核准；維運者不算
    let carol_id = add_user(&fx, "carol", Role::Admin).await;
    assert_eq!(submission_json_of(&fx, &id).await["approvers_available"], 1);
    // 停用丙：又沒有人可以核准
    fx.svc
        .db
        .write(move |s| {
            s.set_state(carol_id, false, "test", Some("離職"), T0)?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(submission_json_of(&fx, &id).await["approvers_available"], 0);
    fx.svc
        .db
        .write(move |s| {
            s.set_state(carol_id, true, "test", None, T0)?;
            // 撤銷乙的管理員角色不影響（乙本來就不能核准）；撤銷丙的才會
            s.remove_assignment(
                SUBJECT_USER,
                &bob_id.to_string(),
                &Assignment {
                    role: Role::Admin,
                    scope: Scope::Global,
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(submission_json_of(&fx, &id).await["approvers_available"], 1);
    fx.svc
        .db
        .write(move |s| {
            s.remove_assignment(
                SUBJECT_USER,
                &carol_id.to_string(),
                &Assignment {
                    role: Role::Admin,
                    scope: Scope::Global,
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let sub = submission_json_of(&fx, &id).await;
    assert_eq!(sub["approvers_available"], 0);
    assert!(
        sub["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!("no_approvers"))
    );
    // 核准的身分檢查在引擎（與帳號表無關）：丙成功
    approve(&fx, &carol, &id).await.unwrap();
}

/// A4：`rollout_approval` 改為 `none` 之後，既有的待核准推送仍然要核准或退回（核准只看推送的狀態）
#[tokio::test]
async fn pending_rollouts_still_need_approval_after_the_policy_is_turned_off() {
    let (fx, _) = fixture("a4", 45).await;
    let id = create(&fx, &who(), &body("x")).await;
    submit(&fx, &who(), &id, "升級 2 台").await;
    // 同一個資料庫、設定改為 none 的服務（重新啟動之後）
    let n = fx.now.clone();
    let svc2 = RemoteService::open(StartParams {
        settings: RemoteSettings {
            allowed_signers: "allowed_signers".into(),
            ..RemoteSettings::default()
        },
        data_dir: fx.dir.clone(),
        db_path: fx.dir.join(crate::store::sqlite::DB_FILE),
        registry: fx.registry.clone(),
        shutdown: Arc::new(crate::shutdown::Shutdown::new()),
        clock: Arc::new(move || n.load(Ordering::SeqCst)),
    })
    .await
    .unwrap();
    assert!(!svc2.settings.approval_required());
    let req = approval(&fx, &id).await;
    let e = svc2
        .approve_rollout(&who(), id.clone(), req.clone())
        .await
        .unwrap_err();
    assert_eq!(e.code, "self_approval", "關閉覆核後建立者仍然不能核准");
    let v = svc2
        .approve_rollout(&user("carol"), id.clone(), req)
        .await
        .unwrap();
    assert_eq!(v["state"], "running");
}

/// A7：同時核准（兩位）、退回、修改同一個待核准的推送——每個操作都在交易內先檢查狀態再轉移，核准最多一個
/// 成功、核准與退回互斥、輸的一方得到 409 `invalid_state`，資料庫沒有半更新的狀態
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_approve_reject_and_update_leave_a_consistent_state() {
    let (fx, _) = fixture("a7", 46).await;
    let fx = Arc::new(fx);
    for round in 0..12 {
        fx.set_now(T0 + round * MIN);
        let id = create(&fx, &who(), &body(&format!("r{round}"))).await;
        submit(&fx, &who(), &id, "升級 2 台").await;
        let req = approval(&fx, &id).await;
        let spawn_approve = |name: &'static str| {
            let (fx, id, req) = (fx.clone(), id.clone(), req.clone());
            tokio::spawn(async move { fx.svc.approve_rollout(&user(name), id, req).await })
        };
        let a1 = spawn_approve("carol");
        let a2 = spawn_approve("dave");
        let rj = {
            let (fx, id) = (fx.clone(), id.clone());
            tokio::spawn(async move {
                fx.svc
                    .reject_rollout(&user("erin"), id, "不行".into())
                    .await
            })
        };
        let up = {
            let (fx, id) = (fx.clone(), id.clone());
            let spec = RolloutSpec::parse(&body(&format!("r{round}-改"))).unwrap();
            tokio::spawn(async move { fx.svc.update_rollout(&who(), id, spec).await })
        };
        let (a1, a2, rj, up) = (
            a1.await.unwrap(),
            a2.await.unwrap(),
            rj.await.unwrap(),
            up.await.unwrap(),
        );
        let approvals = usize::from(a1.is_ok()) + usize::from(a2.is_ok());
        assert!(approvals <= 1, "第 {round} 輪：兩個核准都成功");
        assert!(
            !(approvals == 1 && rj.is_ok()),
            "第 {round} 輪：核准與退回都成功"
        );
        assert!(
            !(approvals == 1 && up.is_ok()),
            "第 {round} 輪：核准後（已開始）仍被修改"
        );
        assert!(
            approvals == 1 || rj.is_ok() || up.is_ok(),
            "第 {round} 輪：沒有任何操作成功"
        );
        for r in [&a1, &a2, &rj] {
            if let Err(e) = r {
                assert_eq!(
                    (e.status.as_u16(), e.code),
                    (409, "invalid_state"),
                    "第 {round} 輪：{}",
                    e.message
                );
            }
        }
        if let Err(e) = &up {
            assert_eq!((e.status.as_u16(), e.code), (409, "invalid_state"));
        }
        // 最後的狀態一致：已核准 → 進行中、對象還在、有核准者；否則是草稿、沒有對象、沒有送審與核准欄位
        let r = state_of(&fx, &id).await;
        let rid = id.clone();
        let hosts = fx
            .svc
            .db
            .read(move |s| Ok(s.rollout_hosts(&rid)?.len()))
            .await
            .unwrap();
        if approvals == 1 {
            assert_eq!(r.state, "running");
            assert!(r.approved_by.is_some() && r.submitted_by.is_some());
            assert_eq!(hosts, 2);
        } else {
            assert_eq!(r.state, "draft", "第 {round} 輪");
            assert_eq!(
                (r.submitted_by, r.submitted_at, r.approved_by, r.approved_at),
                (None, None, None, None)
            );
            assert_eq!(hosts, 0);
        }
        // 讓出主機給下一輪
        fx.svc
            .cancel_rollout(&who(), id, "下一輪".into())
            .await
            .unwrap();
    }
}

/// A7（取消）：同時核准與取消——取消在任何未結束的狀態都可以，結果一定是「已取消」，核准要嘛先成功、
/// 要嘛得到 409；不會留下「已取消但之後又被核准而開始」的推送
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_approve_and_cancel_always_end_cancelled() {
    let (fx, _) = fixture("a7c", 47).await;
    let fx = Arc::new(fx);
    for round in 0..8 {
        fx.set_now(T0 + round * MIN);
        let id = create(&fx, &who(), &body(&format!("c{round}"))).await;
        submit(&fx, &who(), &id, "升級 2 台").await;
        let req = approval(&fx, &id).await;
        let ap = {
            let (fx, id) = (fx.clone(), id.clone());
            tokio::spawn(async move { fx.svc.approve_rollout(&user("carol"), id, req).await })
        };
        let ca = {
            let (fx, id) = (fx.clone(), id.clone());
            tokio::spawn(async move { fx.svc.cancel_rollout(&who(), id, "不做了".into()).await })
        };
        let (ap, ca) = (ap.await.unwrap(), ca.await.unwrap());
        ca.unwrap();
        if let Err(e) = &ap {
            assert_eq!((e.status.as_u16(), e.code), (409, "invalid_state"));
        }
        let r = state_of(&fx, &id).await;
        assert_eq!(r.state, "cancelled");
        assert_eq!(r.approved_by.is_some(), ap.is_ok());
        fx.svc.tick().await.unwrap();
        assert_eq!(fx.svc.offer_of(A), None, "已取消的推送不提議");
    }
}

/// A9：版本庫檔案的 TOCTOU。預覽後刪除檔案 → 送出時需要它的主機是 `artifact_missing`（確認字串的台數跟著
/// 變）；送出後、核准前刪除或截斷檔案 → 核准 409 `artifact_missing`、狀態不變；補檔後可以核准
#[tokio::test]
async fn files_deleted_between_preview_submit_and_approval_are_caught() {
    let (fx, r092) = fixture("a9", 48).await;
    let carol = user("carol");
    let spec = RolloutSpec::parse(&body("x")).unwrap();
    let p = fx
        .svc
        .preview_rollout(spec.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(
        (p["eligible"].as_i64(), p["confirm_text"].as_str()),
        (Some(2), Some("升級 2 台"))
    );
    assert_eq!(p["artifact_problems"], json!([]));
    let id = create(&fx, &who(), &body("x")).await;
    // 預覽之後 tar.gz 被刪除
    let tar = file_of(&fx, &r092, "tar.gz");
    std::fs::remove_file(&tar).unwrap();
    let p = fx
        .svc
        .preview_rollout(spec.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(p["eligible"], 1, "{p}");
    assert_eq!(p["skipped"]["artifact_missing"], 1);
    assert_eq!(p["skipped"].get("no_artifact"), None);
    assert_eq!(p["artifact_problems"][0]["file_problem"], "missing");
    assert!(
        p["artifact_problems"][0]["filename"]
            .as_str()
            .unwrap()
            .ends_with("tar.gz")
    );
    assert!(
        p["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!("artifact_problem"))
    );
    // 用預覽當時的字串送出：台數已經不同
    let e = fx
        .svc
        .submit_rollout(&who(), id.clone(), Some("升級 2 台".into()), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "confirmation_required");
    assert_eq!(e.details.unwrap()["confirm_text"], "升級 1 台");
    let v = submit(&fx, &who(), &id, "升級 1 台").await;
    assert_eq!(v["submission"]["eligible"], 1);
    assert_eq!(v["submission"]["skipped"]["artifact_missing"], 1);
    let a = fx.host_state(&id, A).await;
    assert_eq!(
        (a.state.as_str(), a.reason.as_deref()),
        ("skipped", Some("artifact_missing"))
    );
    // 送出之後 deb 被截斷（大小不符）：核准 409、狀態不變、沒有稽核
    let deb = file_of(&fx, &r092, ".deb");
    let original = std::fs::read(&deb).unwrap();
    std::fs::write(&deb, b"short").unwrap();
    let sub = submission_json_of(&fx, &id).await;
    assert_eq!(sub["artifact_problems"][0]["file_problem"], "size_mismatch");
    assert_eq!(sub["artifact_problems"][0]["hosts"], 1);
    assert!(
        sub["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!("artifact_problem"))
    );
    let e = approve(&fx, &carol, &id).await.unwrap_err();
    assert_eq!(
        (e.status.as_u16(), e.code, e.product_code),
        (409, "artifact_missing", Some("KRS-W8417"))
    );
    let files = e.details.unwrap()["files"].clone();
    assert_eq!(files[0]["file_problem"], "size_mismatch");
    assert!(files[0]["filename"].as_str().unwrap().ends_with(".deb"));
    assert_eq!(state_of(&fx, &id).await.state, "pending_approval");
    // 刪除也一樣
    std::fs::remove_file(&deb).unwrap();
    let e = approve(&fx, &carol, &id).await.unwrap_err();
    assert_eq!(e.code, "artifact_missing");
    assert_eq!(e.details.unwrap()["files"][0]["file_problem"], "missing");
    assert_eq!(audit_count(&fx, "rollout.approve").await, 0);
    // 補回檔案：digest 沒有變（內容與對象沒有變），可以核准
    let digest = submission_of(&fx, &id).await.digest;
    std::fs::write(&deb, &original).unwrap();
    assert_eq!(submission_of(&fx, &id).await.digest, digest);
    let v = approve(&fx, &carol, &id).await.unwrap();
    assert_eq!(v["state"], "running");
}

/// A9（核准之後）：核准後檔案被刪除 → 不提議、主機留在 `pending`（`artifact_missing`）、已提議的收回；
/// 不計入失敗、不暫停；補檔後下一輪自動繼續
#[tokio::test]
async fn hosts_wait_for_a_missing_file_and_continue_after_it_is_restored() {
    let (fx, r092) = fixture("a9b", 49).await;
    let id = create(&fx, &who(), &body("x")).await;
    submit(&fx, &who(), &id, "升級 2 台").await;
    approve(&fx, &user("carol"), &id).await.unwrap();
    // 核准之後、提議之前 tar.gz 被刪除
    let tar = file_of(&fx, &r092, "tar.gz");
    std::fs::remove_file(&tar).unwrap();
    fx.set_now(T0 + 2 * MIN);
    fx.svc.tick().await.unwrap();
    assert_eq!(fx.svc.offer_of(A), None, "檔案遺失的主機不提議");
    assert!(fx.svc.offer_of(B).is_some(), "其他主機照常");
    let a = fx.host_state(&id, A).await;
    assert_eq!(
        (a.state.as_str(), a.reason.as_deref(), a.attempts),
        ("pending", Some("artifact_missing"), 0)
    );
    let rid = id.clone();
    let v = fx
        .svc
        .db
        .read(move |s| {
            let r = s.get_rollout(&rid)?.unwrap();
            Ok(rollout_json(
                &r,
                &s.rollout_hosts(&rid)?,
                T0 + 2 * MIN,
                true,
            ))
        })
        .await
        .unwrap();
    assert_eq!(v["state"], "running");
    assert_eq!(
        v["next_action"],
        json!({"code": "waiting_artifact", "hosts": 1})
    );
    assert_eq!(v["progress"]["failed"], 0);
    // 已提議的主機（B）的檔案也不見了：不發出無法下載的提議，下一輪收回
    let up_b = fx.svc.offer_of(B).unwrap();
    assert!(fx.svc.build_offer(B, &up_b, 1).await.unwrap().is_some());
    let deb = file_of(&fx, &r092, ".deb");
    let original = std::fs::read(&deb).unwrap();
    std::fs::remove_file(&deb).unwrap();
    assert!(fx.svc.build_offer(B, &up_b, 2).await.unwrap().is_none());
    fx.svc.tick().await.unwrap();
    assert_eq!(fx.svc.offer_of(B), None);
    let b = fx.host_state(&id, B).await;
    assert_eq!(
        (b.state.as_str(), b.reason.as_deref()),
        ("pending", Some("artifact_missing"))
    );
    assert_ne!(b.upgrade_id, up_b, "收回的提議換新的 upgrade_id");
    // 等再久也不會被當成失敗、逾期或自動暫停
    fx.set_now(T0 + 30 * 60 * MIN);
    fx.svc.tick().await.unwrap();
    let r = state_of(&fx, &id).await;
    assert_eq!((r.state.as_str(), r.pause_cause), ("running", None));
    assert_eq!(fx.host_state(&id, A).await.state, "pending");
    // 補檔（S25 重新上傳 tar.gz、deb 放回原處）：下一輪自動繼續
    reupload(&fx, &r092, "tar.gz").await;
    std::fs::write(&deb, &original).unwrap();
    fx.svc.tick().await.unwrap();
    for agent in [A, B] {
        let h = fx.host_state(&id, agent).await;
        assert_eq!((h.state.as_str(), h.reason), ("offered", None), "{agent}");
        let up = fx.svc.offer_of(agent).unwrap();
        assert!(fx.svc.build_offer(agent, &up, 3).await.unwrap().is_some());
    }
    assert!(
        !fx.audit_actions()
            .await
            .contains(&"rollout.auto_pause".to_string())
    );
}

/// A10：送審之後、已排程的推送使用中的版本不能刪除（409 `release_in_use`）
#[tokio::test]
async fn releases_used_by_pending_or_scheduled_rollouts_cannot_be_deleted() {
    let (fx, _) = fixture("a10", 50).await;
    let mut later = body("x");
    later["schedule"] = json!({"starts_at": crate::api::http::ts_ms(T0 + 60 * MIN)});
    let id = create(&fx, &who(), &later).await;
    submit(&fx, &who(), &id, "升級 2 台").await;
    let delete = || async {
        fx.svc
            .delete_release(&who(), "0.9.2".into(), None)
            .await
            .unwrap_err()
    };
    assert_eq!(state_of(&fx, &id).await.state, "pending_approval");
    let e = delete().await;
    assert_eq!((e.status.as_u16(), e.code), (409, "release_in_use"));
    assert_eq!(e.details.unwrap()["in_use_by_rollouts"], 1);
    approve(&fx, &user("carol"), &id).await.unwrap();
    assert_eq!(state_of(&fx, &id).await.state, "scheduled");
    assert_eq!(delete().await.code, "release_in_use");
    // 取消之後才可以刪
    fx.svc
        .cancel_rollout(&who(), id, "結束".into())
        .await
        .unwrap();
    fx.svc
        .delete_release(&who(), "0.9.2".into(), None)
        .await
        .unwrap();
}

async fn observe(
    fx: &Fx,
    value: &'static str,
    source: &'static str,
) -> (super::policy::Outcome, super::policy::Observed) {
    let now = fx.svc.now_ms();
    fx.svc
        .in_tx(move |s| super::policy::observe(s, value, source, "node-a", now))
        .await
        .unwrap()
}

/// A12：雙人覆核政策的前值與改變——沒有前值記基準；`none` → `required`、`required` → `none`、只有來源改變
/// 各寫一筆稽核（執行者 system）；沒有改變不寫；還原舊的資料庫依前值比對
#[tokio::test]
async fn approval_policy_changes_are_recorded_against_the_stored_value() {
    use super::policy::{ACTION_BASELINE, ACTION_CHANGE, META_KEY, Outcome};
    let fx = Fx::new("a12", |_, _| {}).await;
    // 第一次：基準，不算改變
    let (o, rec) = observe(&fx, "none", "default").await;
    assert_eq!(o, Outcome::Baseline);
    assert_eq!((rec.changed_at, rec.disabled_at), (None, None));
    let (actor, d) = last_audit(&fx, ACTION_BASELINE).await.unwrap();
    assert_eq!(actor, "system");
    assert_eq!(
        (
            d["value"].as_str(),
            d["source"].as_str(),
            d["node"].as_str()
        ),
        (Some("none"), Some("default"), Some("node-a"))
    );
    // 相同：不寫
    fx.set_now(T0 + MIN);
    let (o, rec) = observe(&fx, "none", "default").await;
    assert_eq!((o, rec.observed_at), (Outcome::Unchanged, T0));
    assert_eq!(audit_count(&fx, ACTION_CHANGE).await, 0);
    // none → required
    let (o, rec) = observe(&fx, "required", "file").await;
    assert_eq!(o, Outcome::Enabled);
    assert_eq!(
        (rec.changed_at, rec.changed_from.as_deref(), rec.disabled_at),
        (Some(T0 + MIN), Some("none"), None)
    );
    let (actor, d) = last_audit(&fx, ACTION_CHANGE).await.unwrap();
    assert_eq!(actor, "system");
    assert_eq!(
        (
            d["from"].as_str(),
            d["to"].as_str(),
            d["source"].as_str(),
            d["previous_source"].as_str()
        ),
        (
            Some("none"),
            Some("required"),
            Some("file"),
            Some("default")
        )
    );
    // 只有來源改變（設定檔 → 環境變數）
    fx.set_now(T0 + 2 * MIN);
    let (o, rec) = observe(&fx, "required", "environment").await;
    assert_eq!(o, Outcome::SourceChanged);
    assert_eq!(
        rec.changed_at,
        Some(T0 + MIN),
        "值沒有變，上一次改變的時間不動"
    );
    let (_, d) = last_audit(&fx, ACTION_CHANGE).await.unwrap();
    assert_eq!(d["source_changed"], true);
    // required → none：記下關閉的時間
    fx.set_now(T0 + 3 * MIN);
    let (o, rec) = observe(&fx, "none", "environment").await;
    assert_eq!(o, Outcome::Disabled);
    assert_eq!(rec.disabled_at, Some(T0 + 3 * MIN));
    let (_, d) = last_audit(&fx, ACTION_CHANGE).await.unwrap();
    assert_eq!(
        (d["from"].as_str(), d["to"].as_str(), d["basis"].as_str()),
        (Some("required"), Some("none"), Some("database_record"))
    );
    assert_eq!(audit_count(&fx, ACTION_CHANGE).await, 3);
    // 之後維持 none：不再寫，但「已關閉」的紀錄留著
    fx.set_now(T0 + 4 * MIN);
    let (o, rec) = observe(&fx, "none", "environment").await;
    assert_eq!(
        (o, rec.disabled_at),
        (Outcome::Unchanged, Some(T0 + 3 * MIN))
    );
    assert_eq!(audit_count(&fx, ACTION_CHANGE).await, 3);
    assert_eq!(rec.recent_change_json(T0 + 5 * MIN)["to"], "none");
    assert_eq!(
        rec.recent_change_json(T0 + 3 * MIN + super::policy::RECENT_CHANGE_MS),
        Value::Null,
        "超過 30 天不再顯示"
    );
    // 還原舊的資料庫（前值是備份當時的 required）：與目前的 none 不同，再報一次
    fx.svc
        .db
        .write(|s| {
            s.put_meta(
                META_KEY,
                r#"{"value":"required","source":"file","observed_at":1,"node":"old-node"}"#,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (o, _) = observe(&fx, "none", "environment").await;
    assert_eq!(o, Outcome::Disabled);
    let (_, d) = last_audit(&fx, ACTION_CHANGE).await.unwrap();
    assert_eq!(d["previous_node"], "old-node");
    // 改回 required：清除「已關閉」
    let (o, rec) = observe(&fx, "required", "file").await;
    assert_eq!((o, rec.disabled_at), (Outcome::Enabled, None));
    // 稽核的雜湊鏈完整（A13）
    let report = fx.svc.db.read(|s| Ok(s.audit_verify()?)).await.unwrap();
    assert!(report.broken.is_none(), "{:?}", report.broken);
    assert!(report.checked >= 6);
}

/// A12：沒有前值、目前是 `none`，但資料庫裡有待核准或核准過的推送（這台 Kairos 曾經啟用雙人覆核）→ 視同
/// `required` → `none`；前值無法解析時視同沒有前值
#[tokio::test]
async fn a_missing_baseline_with_reviewed_rollouts_counts_as_disabled() {
    use super::policy::{ACTION_BASELINE, ACTION_CHANGE, META_KEY, Outcome};
    let (fx, _) = fixture("a12b", 51).await;
    let id = create(&fx, &who(), &body("x")).await;
    submit(&fx, &who(), &id, "升級 2 台").await;
    // 有待核准的推送
    let (o, rec) = observe(&fx, "none", "file").await;
    assert_eq!(o, Outcome::BaselineDisabled);
    assert_eq!(
        (rec.disabled_at, rec.changed_from.as_deref()),
        (Some(T0), Some("required"))
    );
    let (actor, d) = last_audit(&fx, ACTION_CHANGE).await.unwrap();
    assert_eq!(actor, "system");
    assert_eq!(
        (d["from"].as_str(), d["to"].as_str(), d["basis"].as_str()),
        (Some("required"), Some("none"), Some("reviewed_rollouts"))
    );
    assert_eq!(audit_count(&fx, ACTION_BASELINE).await, 0);
    // 核准過的推送（紀錄遺失之後）也算；前值損毀視同沒有前值
    approve(&fx, &user("carol"), &id).await.unwrap();
    fx.svc
        .db
        .write(|s| {
            s.put_meta(META_KEY, "不是 JSON")?;
            Ok(())
        })
        .await
        .unwrap();
    let (o, _) = observe(&fx, "none", "file").await;
    assert_eq!(o, Outcome::BaselineDisabled);
    let (_, d) = last_audit(&fx, ACTION_CHANGE).await.unwrap();
    assert_eq!(d["previous_unreadable"], true);
    // 目前是 required 時只是基準
    fx.svc
        .db
        .write(|s| {
            s.put_meta(META_KEY, "不是 JSON")?;
            Ok(())
        })
        .await
        .unwrap();
    let (o, _) = observe(&fx, "required", "file").await;
    assert_eq!(o, Outcome::Baseline);
}

/// A12：服務啟動時的比對——關閉時開告警的系統規則「雙人覆核已關閉」（不會自己結束）；稽核寫不進去時回錯誤
/// （呼叫端不啟動遠端管理）
#[tokio::test]
async fn disabling_approval_opens_an_alert_and_a_failed_audit_write_blocks_startup() {
    use crate::alerts::rules::ids;
    let fx = Fx::new("a12c", |_, _| {}).await;
    // 資料庫記錄的上一次設定是 required，這次以 none 啟動
    fx.svc
        .db
        .write(|s| {
            s.put_meta(
                super::policy::META_KEY,
                r#"{"value":"required","source":"file","observed_at":1,"node":"n1"}"#,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(fx.svc.approval_policy().is_none());
    let o = fx.svc.observe_approval_policy("file", "n1").await.unwrap();
    assert_eq!(o, super::policy::Outcome::Disabled);
    let p = fx.svc.approval_policy().unwrap();
    assert_eq!((p.value.as_str(), p.disabled_at), ("none", Some(T0)));
    let conditions = |fx: &Fx| {
        let mut bus = fx.registry.bus().subscribe();
        let svc = fx.svc.clone();
        async move {
            svc.reconcile_alerts().await;
            let mut out = Vec::new();
            while let Ok(ev) = bus.try_recv() {
                if let crate::bus::EventKind::RemoteCondition {
                    rule,
                    subject,
                    active,
                    title,
                    details,
                } = &ev.kind
                {
                    out.push((
                        *rule,
                        subject.clone(),
                        *active,
                        title.clone(),
                        details.clone(),
                    ));
                }
            }
            out
        }
    };
    let c = conditions(&fx).await;
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(
        (c[0].0, c[0].1.as_str(), c[0].2),
        (ids::ROLLOUT_APPROVAL_DISABLED, "remote", true)
    );
    assert!(c[0].3.contains("雙人覆核已關閉"));
    assert_eq!(c[0].4["from"], "required");
    // 之後的每一輪不重複發布，也不會自己結束
    fx.set_now(T0 + 40 * 24 * 60 * MIN);
    assert!(conditions(&fx).await.is_empty());
    // 系統規則存在、警告等級
    let rule = crate::alerts::rules::builtin_default(ids::ROLLOUT_APPROVAL_DISABLED).unwrap();
    assert_eq!(rule.severity, crate::alerts::model::Severity::Warning);

    // 稽核寫不進去：回錯誤、沒有留下新的前值
    let fx2 = Fx::new("a12d", |_, _| {}).await;
    fx2.sql(|c| c.execute_batch("DROP TABLE audit_events").unwrap())
        .await;
    let e = fx2.svc.observe_approval_policy("file", "n1").await;
    assert!(e.is_err(), "{e:?}");
    assert!(fx2.svc.approval_policy().is_none());
    let meta = fx2
        .svc
        .db
        .read(|s| Ok(s.get_meta(super::policy::META_KEY)?))
        .await
        .unwrap();
    assert_eq!(meta, None, "交易回復，沒有只寫入前值而沒有稽核");
}

/// 0.10.1（H2）：`approval_of` 的每一種情況
#[test]
fn approval_of_is_derived_from_the_rollout_row() {
    use super::engine::approval_of;
    let row =
        |state: &str, submitted: bool, approved: bool, started: bool| super::store::RolloutRow {
            id: "ro-1".into(),
            state: state.into(),
            submitted_at: submitted.then_some(T0),
            submitted_by: submitted.then(|| "user:admin".to_string()),
            approved_at: approved.then_some(T0 + 1),
            approved_by: approved.then(|| "user:carol".to_string()),
            started_at: started.then_some(T0 + 2),
            ..Default::default()
        };
    // 還沒開始也沒有排定：null
    assert_eq!(approval_of(&row("draft", false, false, false)), None);
    assert_eq!(
        approval_of(&row("draft", true, false, false)),
        None,
        "退回後的草稿"
    );
    assert_eq!(
        approval_of(&row("pending_approval", true, false, false)),
        None
    );
    assert_eq!(
        approval_of(&row("cancelled", true, false, false)),
        None,
        "開始之前就取消（待核准或已排程時）"
    );
    assert_eq!(approval_of(&row("cancelled", false, false, false)), None);
    // 有核准紀錄
    for state in ["scheduled", "running", "paused", "completed", "cancelled"] {
        let started = state != "scheduled";
        assert_eq!(
            approval_of(&row(state, true, true, started)),
            Some("approved"),
            "{state}"
        );
        // 送出時政策為 none：沒有核准就排程或開始
        assert_eq!(
            approval_of(&row(state, true, false, started)),
            Some("not_required"),
            "{state}"
        );
    }
    // 0.9.x 的推送（沒有 submitted_at）：無法判斷，不誤標成 not_required
    for state in ["running", "paused", "completed", "cancelled"] {
        assert_eq!(
            approval_of(&row(state, false, false, true)),
            Some("unknown"),
            "{state}"
        );
    }
}

/// 0.10.1（H2）：推送開始時生效的政策與有沒有經過核准，留在稽核（`rollout.submit`、`rollout.start`），
/// API 的 `approval` 標示沒有經過核准就開始的推送——不依賴 `meta` 的政策前值
#[tokio::test]
async fn the_policy_in_effect_is_recorded_when_a_rollout_starts() {
    // 雙人覆核：送出 → 待核准（approval 為 null）→ 核准 → 開始
    let (fx, _) = fixture("h2a", 61).await;
    let id = create(&fx, &who(), &body("x")).await;
    let v = submit(&fx, &who(), &id, "升級 2 台").await;
    assert_eq!(v["state"], "pending_approval");
    assert_eq!(v["approval"], Value::Null);
    let (_, d) = last_audit(&fx, "rollout.submit").await.unwrap();
    assert_eq!(d["rollout_approval"], "required");
    assert_eq!(audit_count(&fx, "rollout.start").await, 0);
    let req = approval(&fx, &id).await;
    let v = fx
        .svc
        .approve_rollout(&user("carol"), id.clone(), req)
        .await
        .unwrap();
    assert_eq!(
        (v["state"].as_str(), v["approval"].as_str()),
        (Some("running"), Some("approved"))
    );
    let (actor, d) = last_audit(&fx, "rollout.start").await.unwrap();
    assert_eq!(actor, "user:carol");
    assert_eq!(
        (
            d["rollout_approval"].as_str(),
            d["approval"].as_str(),
            d["approved_by"].as_str(),
            d["submitted_by"].as_str(),
            d["approved"].as_bool(),
            d["rollout_id"].as_str(),
        ),
        (
            Some("required"),
            Some("approved"),
            Some("user:carol"),
            Some("user:admin"),
            Some(true),
            Some(id.as_str())
        )
    );
    fx.svc
        .cancel_rollout(&who(), id.clone(), "結束".into())
        .await
        .unwrap();
    assert_eq!(state_of(&fx, &id).await.state, "cancelled");
    let rid = id.clone();
    let r = state_of(&fx, &rid).await;
    assert_eq!(
        super::engine::approval_of(&r),
        Some("approved"),
        "取消後仍看得出核准過"
    );

    // 同一個資料庫、政策改為 none（重新啟動之後）。把 meta 的前值刪掉（模擬能寫資料庫的人湮滅政策的紀錄）：
    // 推送本身的稽核與 approval 不受影響
    let n = fx.now.clone();
    let svc2 = RemoteService::open(StartParams {
        settings: RemoteSettings {
            allowed_signers: "allowed_signers".into(),
            ..RemoteSettings::default()
        },
        data_dir: fx.dir.clone(),
        db_path: fx.dir.join(crate::store::sqlite::DB_FILE),
        registry: fx.registry.clone(),
        shutdown: Arc::new(crate::shutdown::Shutdown::new()),
        clock: Arc::new(move || n.load(Ordering::SeqCst)),
    })
    .await
    .unwrap();
    fx.sql(|c| {
        c.execute("DELETE FROM meta WHERE key = ?1", [super::policy::META_KEY])
            .unwrap();
    })
    .await;
    fx.set_now(T0 + 10 * MIN);
    let id2 = svc2
        .create_rollout(&who(), RolloutSpec::parse(&body("y")).unwrap())
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let v = svc2
        .submit_rollout(&who(), id2.clone(), Some("升級 2 台".into()), None)
        .await
        .unwrap();
    assert_eq!(
        (v["state"].as_str(), v["approval"].as_str()),
        (Some("running"), Some("not_required")),
        "政策為 none：送出就開始，標示為沒有經過核准"
    );
    let (_, d) = last_audit(&fx, "rollout.submit").await.unwrap();
    assert_eq!(d["rollout_approval"], "none");
    let (actor, d) = last_audit(&fx, "rollout.start").await.unwrap();
    assert_eq!(actor, "user:admin");
    assert_eq!(
        (
            d["rollout_approval"].as_str(),
            d["approval"].as_str(),
            d["approved_by"].as_str(),
            d["submitted_by"].as_str(),
            d["rollout_id"].as_str(),
        ),
        (
            Some("none"),
            Some("not_required"),
            None,
            Some("user:admin"),
            Some(id2.as_str())
        )
    );
    svc2.cancel_rollout(&who(), id2, "結束".into())
        .await
        .unwrap();

    // 政策為 none、有 starts_at：先排程（已經標示），到時間由 system 開始，稽核同樣記下政策
    fx.set_now(T0 + 20 * MIN);
    let mut later = body("z");
    later["schedule"] = json!({"starts_at": crate::api::http::ts_ms(T0 + 60 * MIN)});
    let id3 = svc2
        .create_rollout(&who(), RolloutSpec::parse(&later).unwrap())
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let v = svc2
        .submit_rollout(&who(), id3.clone(), Some("升級 2 台".into()), None)
        .await
        .unwrap();
    assert_eq!(
        (v["state"].as_str(), v["approval"].as_str()),
        (Some("scheduled"), Some("not_required"))
    );
    assert_eq!(audit_count(&fx, "rollout.start").await, 2, "還沒開始");
    fx.set_now(T0 + 61 * MIN);
    svc2.tick().await.unwrap();
    // 開始了（這個服務的主機在同一輪就處理完，推送可能已經是 completed；看的是開始的時間與稽核）
    let r3 = state_of(&fx, &id3).await;
    assert_eq!(r3.started_at, Some(T0 + 61 * MIN));
    assert_eq!(super::engine::approval_of(&r3), Some("not_required"));
    let (actor, d) = last_audit(&fx, "rollout.start").await.unwrap();
    assert_eq!(actor, "system");
    assert_eq!(
        (
            d["rollout_approval"].as_str(),
            d["approval"].as_str(),
            d["scheduled"].as_bool(),
            d["submitted_by"].as_str(),
            d["rollout_id"].as_str(),
        ),
        (
            Some("none"),
            Some("not_required"),
            Some(true),
            Some("user:admin"),
            Some(id3.as_str())
        )
    );
    // 稽核雜湊鏈完整（這些紀錄受 HMAC 保護）
    assert_eq!(audit_count(&fx, "rollout.start").await, 3);
}
