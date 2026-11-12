//! 遠端管理的服務層測試（暫存的 SQLite 資料庫、假的時鐘）：集中設定、版本庫、推送引擎與 8443 的處理

use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use super::ops::{Actor, ProfileInput};
use super::rollout::RolloutSpec;
use super::test_sign::{TestSigner, release};
use super::*;

const A: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
const B: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const C: &str = "cccccccc-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const D: &str = "dddddddd-bbbb-4ccc-8ddd-eeeeeeeeeeee";

const T0: i64 = 1_800_000_000_000;

struct Fx {
    dir: PathBuf,
    svc: Arc<RemoteService>,
    now: Arc<AtomicI64>,
    registry: Arc<crate::registry::Registry>,
}

impl Fx {
    async fn new(tag: &str, tweak: impl FnOnce(&mut RemoteSettings, &std::path::Path)) -> Fx {
        let dir = std::env::temp_dir().join(format!(
            "kairos-remote-{tag}-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(crate::store::sqlite::DB_FILE);
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        crate::store::sqlite::migrate(&mut conn, crate::store::sqlite::MIGRATIONS).unwrap();
        drop(conn);
        let mut settings = RemoteSettings::default();
        tweak(&mut settings, &dir);
        let now = Arc::new(AtomicI64::new(T0));
        let n = now.clone();
        let registry = Arc::new(crate::registry::Registry::new());
        let svc = RemoteService::open(StartParams {
            settings,
            data_dir: dir.clone(),
            db_path: path,
            registry: registry.clone(),
            shutdown: Arc::new(crate::shutdown::Shutdown::new()),
            clock: Arc::new(move || n.load(Ordering::SeqCst)),
        })
        .await
        .unwrap();
        Fx {
            dir,
            svc,
            now,
            registry,
        }
    }

    fn set_now(&self, t: i64) {
        self.now.store(t, Ordering::SeqCst);
    }

    /// 以 SQL 執行（準備資料或檢查結果）
    async fn sql<R: Send + 'static>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> R + Send + 'static,
    ) -> R {
        self.svc
            .db
            .write(move |s| Ok(f(s.test_sqlite())))
            .await
            .unwrap()
    }

    async fn add_host(
        &self,
        id: &str,
        version: &str,
        group: &str,
        tags: &[&str],
        method: &str,
        upgrade: &str,
    ) {
        let (id, version, group, method, upgrade) = (
            id.to_string(),
            version.to_string(),
            group.to_string(),
            method.to_string(),
            upgrade.to_string(),
        );
        let tags = serde_json::to_string(tags).unwrap();
        self.sql(move |c| {
            c.execute(
                "INSERT INTO hosts (agent_id, hostname, agent_version, host_group, tags, os, install_method,
                    install_upgrade, install_config, first_seen, last_seen, online)
                 VALUES (?1, ?1, ?2, ?3, ?4, '{\"name\":\"Ubuntu\",\"arch\":\"x86_64\"}', ?5, ?6, 'enabled', 0, 0, 1)",
                rusqlite::params![id, version, group, tags, method, upgrade],
            )
            .unwrap();
        })
        .await;
    }

    /// 主機上線、協商了 config 與 upgrade（狀態表與遠端管理的連線表）
    fn connect(&self, id: &str, version: &str, method: &str) {
        let conn_id = crate::registry::next_conn_id();
        self.registry
            .register(
                conn_id,
                crate::registry::HelloInfo {
                    agent_id: id.to_string(),
                    hostname: id.to_string(),
                    agent_version: version.to_string(),
                    proto: 2,
                    host: None,
                    peer: "127.0.0.1:1".into(),
                    offline_after: Duration::from_secs(75),
                    client_chain: None,
                },
                Instant::now(),
                chrono::Utc::now(),
            )
            .unwrap();
        self.svc.attach(
            id,
            AgentSession {
                conn_id,
                agent_version: version.to_string(),
                install_method: Some(method.to_string()),
                install_upgrade: Some("ready".into()),
                arch: Some("x86_64".into()),
                config: true,
                upgrade: true,
            },
        );
    }

    async fn audit_actions(&self) -> Vec<String> {
        self.sql(|c| {
            let mut st = c
                .prepare("SELECT action FROM audit_events ORDER BY id")
                .unwrap();
            st.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        })
        .await
    }

    async fn host_state(&self, rollout: &str, agent: &str) -> super::store::RolloutHostRow {
        let (r, a) = (rollout.to_string(), agent.to_string());
        self.svc
            .db
            .read(move |s| Ok(s.rollout_host(&r, &a)?.unwrap()))
            .await
            .unwrap()
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn who() -> Actor {
    Actor {
        actor: "user:admin".into(),
        ip: Some("127.0.0.1".into()),
    }
}

fn profile(name: &str, settings: Value) -> ProfileInput {
    ProfileInput {
        name: Some(name.into()),
        description: Some("測試".into()),
        settings: Some(settings),
        reason: None,
        impact_token: None,
    }
}

#[tokio::test]
async fn profiles_and_assignments_update_only_hosts_whose_settings_change() {
    let fx = Fx::new("cfg", |_, _| {}).await;
    fx.add_host(A, "0.9.0", "Web", &["env:prod"], "tar", "ready")
        .await;
    fx.add_host(B, "0.9.0", "DB", &[], "deb", "ready").await;
    fx.add_host(C, "0.8.0", "Web", &[], "tar", "ready").await;
    let p1 = fx
        .svc
        .create_profile(&who(), profile("全站", json!({"report": {"interval": 10}})))
        .await
        .unwrap();
    let p1 = p1["id"].as_i64().unwrap();
    assert_eq!(fx.svc.desired_rev(A), 0, "沒有指派");
    // 同名（不分大小寫）
    let e = fx
        .svc
        .create_profile(&who(), profile("全站", json!({})))
        .await
        .unwrap_err();
    assert_eq!(e.code, "profile_exists");
    let e = fx
        .svc
        .create_profile(
            &who(),
            profile("壞", json!({"server": {"address": "evil"}})),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (e.code, e.product_code),
        ("config_invalid", Some(CODE_CONFIG_INVALID))
    );
    assert_eq!(e.details.as_ref().unwrap()["reason"], "unknown_key");

    let a1 = fx
        .svc
        .create_assignment(&who(), p1, desired::Selector::Global, None, None, None)
        .await
        .unwrap();
    assert_eq!(a1["priority"], 0);
    let rev_global = fx.svc.desired_rev(A);
    assert!(rev_global > 0);
    assert_eq!(fx.svc.desired_rev(B), rev_global);
    assert_eq!(fx.svc.desired_rev(C), rev_global);
    // global 最多一筆
    let p_other = fx
        .svc
        .create_profile(&who(), profile("其他", json!({"log": {"level": "warn"}})))
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let e = fx
        .svc
        .create_assignment(&who(), p_other, desired::Selector::Global, None, None, None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "global_exists");

    // 只有 A 的設定改變
    let p2 = fx
        .svc
        .create_profile(
            &who(),
            profile("web-01 除錯", json!({"log": {"level": "debug"}})),
        )
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    fx.svc
        .create_assignment(
            &who(),
            p2,
            desired::Selector::Host(A.into()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let rev_a = fx.svc.desired_rev(A);
    assert!(rev_a > rev_global);
    assert_eq!(
        fx.svc.desired_rev(B),
        rev_global,
        "B 不受影響、不會重新套用"
    );

    // config：合併、雜湊、來源、依版本過濾
    let cfg = fx.svc.config_for(A, "0.9.0", 7).await.unwrap();
    assert_eq!((cfg.seq, cfg.rev), (7, rev_a as u64));
    assert_eq!(
        Value::Object(cfg.settings.clone()),
        json!({"report": {"interval": 10}, "log": {"level": "debug"}})
    );
    assert!(kairos_proto::mgmt::validate_remote_config(&cfg).is_ok());
    assert_eq!(cfg.sources.as_ref().unwrap()["log.level"], "web-01 除錯");
    let old = fx.svc.config_for(C, "0.8.0", 1).await.unwrap();
    assert!(old.settings.is_empty(), "0.8 不認得任何鍵");
    let none = fx.svc.config_for(D, "0.9.0", 1).await.unwrap();
    assert_eq!((none.rev, none.settings.len()), (0, 0));

    // 相同優先序、相同的鍵、同一台主機 → 409
    let p3 = fx
        .svc
        .create_profile(
            &who(),
            profile("web 警告", json!({"log": {"level": "warn"}})),
        )
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let e = fx
        .svc
        .create_assignment(
            &who(),
            p3,
            desired::Selector::Group("web".into()),
            Some(1000),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "assignment_conflict");
    assert_eq!(e.details.as_ref().unwrap()["keys"], json!(["log.level"]));
    // 不同優先序可以
    fx.svc
        .create_assignment(
            &who(),
            p3,
            desired::Selector::Group("web".into()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        fx.svc.config_for(A, "0.9.0", 1).await.unwrap().settings["log"]["level"],
        "debug",
        "host 優先於 group"
    );

    // 修改設定檔：所有指派到的主機重新計算
    let before_b = fx.svc.desired_rev(B);
    fx.svc
        .update_profile(
            &who(),
            p1,
            ProfileInput {
                name: None,
                description: None,
                settings: Some(json!({"report": {"interval": 20}})),
                reason: Some("降低頻率".into()),
                impact_token: None,
            },
            None,
        )
        .await
        .unwrap();
    assert!(fx.svc.desired_rev(B) > before_b);
    // 回復第 1 版
    let v = fx
        .svc
        .update_profile(
            &who(),
            p1,
            ProfileInput {
                name: None,
                description: None,
                settings: None,
                reason: None,
                impact_token: None,
            },
            Some(1),
        )
        .await
        .unwrap();
    assert_eq!(v["revision"], 3);
    assert_eq!(v["settings"], json!({"report": {"interval": 10}}));

    // 有指派的設定檔不能刪
    let e = fx.svc.delete_profile(&who(), p1, None).await.unwrap_err();
    assert_eq!(e.code, "profile_in_use");
    let assignment = a1["id"].as_i64().unwrap();
    fx.svc
        .delete_assignment(&who(), assignment, None)
        .await
        .unwrap();
    assert_eq!(fx.svc.desired_rev(B), 0, "沒有任何設定時 desired_rev = 0");
    fx.svc.delete_profile(&who(), p1, None).await.unwrap();

    let actions = fx.audit_actions().await;
    for a in [
        "config.profile.create",
        "config.assign",
        "config.profile.update",
        "config.profile.revert",
        "config.unassign",
        "config.profile.delete",
    ] {
        assert!(actions.iter().any(|x| x == a), "{a}: {actions:?}");
    }
    // 重新載入服務：記憶體中的 desired_rev 由資料庫載回
    let rows = fx.svc.db.read(|s| Ok(s.desired_all()?)).await.unwrap();
    assert!(
        rows.iter()
            .any(|(a, rev, _)| a == A && *rev == fx.svc.desired_rev(A))
    );
}

#[tokio::test]
async fn config_status_does_not_move_the_applied_revision_backwards() {
    let fx = Fx::new("status", |_, _| {}).await;
    let status = |rev: u64, interval: u64, state: &str| kairos_proto::mgmt::ConfigStatus {
        seq: 1,
        ts: "2026-11-20T02:00:00.000Z".into(),
        rev,
        hash: format!("sha256:{}", "a".repeat(64)),
        state: state.into(),
        effective: json!({"report": {"interval": interval}})
            .as_object()
            .unwrap()
            .clone(),
        overridden: vec![],
        ignored: vec![],
        errors: vec![],
        code: None,
    };
    let mut rx = fx.svc.subscribe();
    fx.svc
        .record_config_status(A, "web-01", &status(5, 10, "applied"))
        .await
        .unwrap();
    let ev = rx.try_recv().expect("config.host");
    assert_eq!(ev.kind, "config.host");
    assert_eq!(
        (
            ev.data["agent_id"].as_str(),
            ev.data["applied_rev"].as_u64()
        ),
        (Some(A), Some(5))
    );
    fx.svc
        .record_config_status(A, "web-01", &status(3, 7, "rejected"))
        .await
        .unwrap();
    let row = fx
        .svc
        .db
        .read(|s| Ok(s.host_config(A)?.unwrap()))
        .await
        .unwrap();
    assert_eq!(
        (row.applied_rev, row.state.as_deref()),
        (5, Some("applied"))
    );
    assert_eq!(
        row.effective.as_deref(),
        Some(r#"{"report":{"interval":7}}"#)
    );
    fx.svc
        .record_config_status(A, "web-01", &status(6, 7, "rejected"))
        .await
        .unwrap();
    let row = fx
        .svc
        .db
        .read(|s| Ok(s.host_config(A)?.unwrap()))
        .await
        .unwrap();
    assert_eq!(
        (row.applied_rev, row.state.as_deref()),
        (6, Some("rejected"))
    );
    // 期望設定回到第 0 版（沒有任何設定）：rev 0 是目前的期望版本，雖然比已記錄的小也照常記錄
    assert_eq!(fx.svc.desired_rev(A), 0);
    fx.svc
        .record_config_status(A, "web-01", &status(0, 5, "applied"))
        .await
        .unwrap();
    let row = fx
        .svc
        .db
        .read(|s| Ok(s.host_config(A)?.unwrap()))
        .await
        .unwrap();
    assert_eq!(
        (row.applied_rev, row.state.as_deref()),
        (0, Some("applied"))
    );
}

#[tokio::test]
async fn large_changes_need_an_impact_token() {
    let fx = Fx::new("impact", |_, _| {}).await;
    fx.sql(|c| {
        let tx = c.unchecked_transaction().unwrap();
        for i in 0..1000 {
            tx.execute(
                "INSERT INTO hosts (agent_id, hostname, agent_version, first_seen, last_seen)
                 VALUES (?1, 'h', '0.9.0', 0, 0)",
                [format!("{i:08x}-0000-4000-8000-000000000000")],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    })
    .await;
    let p = fx
        .svc
        .create_profile(&who(), profile("p", json!({"log": {"level": "debug"}})))
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let e = fx
        .svc
        .create_assignment(&who(), p, desired::Selector::Global, None, None, None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "confirmation_required");
    let d = e.details.unwrap();
    assert_eq!(d["affected_hosts"], 1000);
    let token = d["impact_token"].as_str().unwrap().to_string();
    let e = fx
        .svc
        .create_assignment(
            &who(),
            p,
            desired::Selector::Global,
            None,
            None,
            Some("wrong".into()),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "impact_changed");
    fx.svc
        .create_assignment(
            &who(),
            p,
            desired::Selector::Global,
            None,
            None,
            Some(token),
        )
        .await
        .unwrap();
    // 預覽
    let v = fx
        .svc
        .preview_change(ops::PreviewChange::Profile {
            id: p,
            settings: json!({"log": {"level": "warn"}})
                .as_object()
                .unwrap()
                .clone(),
        })
        .await
        .unwrap();
    assert_eq!(v["affected_hosts"], 1000);
    assert_eq!(v["by_change"]["log.level"], 1000);
    assert_eq!(v["confirm_required"], true);
}

#[tokio::test]
async fn expired_assignments_are_removed_by_the_system() {
    let fx = Fx::new("expire", |_, _| {}).await;
    fx.add_host(A, "0.9.0", "Web", &[], "tar", "ready").await;
    let p = fx
        .svc
        .create_profile(
            &who(),
            profile("暫時除錯", json!({"log": {"level": "debug"}})),
        )
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let e = fx
        .svc
        .create_assignment(
            &who(),
            p,
            desired::Selector::Host(A.into()),
            None,
            Some(T0 - 1),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "invalid_parameter");
    fx.svc
        .create_assignment(
            &who(),
            p,
            desired::Selector::Host(A.into()),
            None,
            Some(T0 + 60_000),
            None,
        )
        .await
        .unwrap();
    assert!(fx.svc.desired_rev(A) > 0);
    fx.svc.expire_assignments().await.unwrap();
    assert!(fx.svc.desired_rev(A) > 0, "還沒到期");
    fx.set_now(T0 + 60_000);
    fx.svc.expire_assignments().await.unwrap();
    assert_eq!(fx.svc.desired_rev(A), 0);
    assert!(
        fx.audit_actions()
            .await
            .iter()
            .any(|a| a == "config.assign_expired")
    );
}

#[tokio::test]
async fn hello_recomputes_with_the_tags_from_hello() {
    let fx = Fx::new("hello", |_, _| {}).await;
    let p = fx
        .svc
        .create_profile(&who(), profile("prod", json!({"report": {"interval": 5}})))
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    fx.svc
        .create_assignment(
            &who(),
            p,
            desired::Selector::Tag("env:prod".into()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    // D 不在清冊（第一次連線、report 尚未寫入）
    assert_eq!(fx.svc.desired_rev(D), 0);
    let rev = fx
        .svc
        .on_hello_config(D, vec!["env:prod".into()], "0.9.0", Some("enabled".into()))
        .await
        .unwrap();
    assert!(rev > 0);
    assert_eq!(fx.svc.desired_rev(D), rev);
    // 同樣的標籤：不寫入、同一版
    assert_eq!(
        fx.svc
            .on_hello_config(D, vec!["env:prod".into()], "0.9.0", None)
            .await
            .unwrap(),
        rev
    );
    // 標籤移除：沒有設定
    assert_eq!(
        fx.svc
            .on_hello_config(D, vec![], "0.9.0", None)
            .await
            .unwrap(),
        0
    );
}

fn trust_settings(signer: &TestSigner) -> impl FnOnce(&mut RemoteSettings, &std::path::Path) + '_ {
    move |s, dir| {
        std::fs::write(dir.join("allowed_signers"), signer.allowed_signers()).unwrap();
        s.allowed_signers = "allowed_signers".into();
    }
}

/// 匯入一次測試發佈並上傳所有發佈檔
async fn import(fx: &Fx, r: &super::test_sign::TestRelease) {
    fx.svc
        .import_release(
            &who(),
            r.sha256sums.clone(),
            r.signature.clone(),
            Some(r.manifest.clone()),
        )
        .await
        .unwrap();
    for (name, data) in &r.files {
        let mut up = releases::Upload::create(&fx.svc.releases_dir, 1 << 20).unwrap();
        up.write(data).unwrap();
        let (sha, size) = up.finish().unwrap();
        fx.svc
            .store_artifact(&who(), r.version.clone(), name.clone(), up, sha, size)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn releases_are_verified_stored_and_protected_while_in_use() {
    let signer = TestSigner::new(3);
    let fx = Fx::new("rel", trust_settings(&signer)).await;
    let r = release(&signer, "0.9.2", "abcdef1");
    // 另一把金鑰簽的：簽章失敗，稽核 release.import_failed
    let other = TestSigner::new(4);
    let e = fx
        .svc
        .import_release(
            &who(),
            r.sha256sums.clone(),
            other.sign(r.sha256sums.as_bytes()),
            Some(r.manifest.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (e.code, e.product_code),
        ("release_signature_invalid", Some(CODE_IMPORT_FAILED))
    );
    assert!(
        fx.audit_actions()
            .await
            .iter()
            .any(|a| a == "release.import_failed")
    );

    let v = fx
        .svc
        .import_release(
            &who(),
            r.sha256sums.clone(),
            r.signature.clone(),
            Some(r.manifest.clone()),
        )
        .await
        .unwrap();
    assert_eq!(v["version"], "0.9.2");
    assert_eq!(v["verified"], true);
    assert_eq!(v["artifacts"].as_array().unwrap().len(), 3);
    assert_eq!(v["artifacts"][0]["stored"], false);
    let e = fx
        .svc
        .import_release(
            &who(),
            r.sha256sums.clone(),
            r.signature.clone(),
            Some(r.manifest.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "release_exists");

    // 上傳：雜湊不符、不在 SHA256SUMS
    let up_with = |data: &[u8]| {
        let mut up = releases::Upload::create(&fx.svc.releases_dir, 1 << 20).unwrap();
        up.write(data).unwrap();
        let (sha, size) = up.finish().unwrap();
        (up, sha, size)
    };
    let (name, data) = r.files[0].clone();
    let (up, sha, size) = up_with(b"tampered");
    let e = fx
        .svc
        .store_artifact(&who(), "0.9.2".into(), name.clone(), up, sha, size)
        .await
        .unwrap_err();
    assert_eq!(e.code, "artifact_hash_mismatch");
    let (up, sha, size) = up_with(&data);
    let e = fx
        .svc
        .store_artifact(
            &who(),
            "0.9.2".into(),
            "pulse-0.9.2-abcdef1-windows-x86_64.msi".into(),
            up,
            sha,
            size,
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "artifact_not_listed");
    let (up, sha, size) = up_with(&data);
    let a = fx
        .svc
        .store_artifact(&who(), "0.9.2".into(), name.clone(), up, sha.clone(), size)
        .await
        .unwrap();
    assert_eq!(
        (a["stored"].as_bool(), a["size"].as_i64()),
        (Some(true), Some(data.len() as i64))
    );
    let path = releases::artifact_path(&fx.svc.releases_dir, &sha);
    assert_eq!(std::fs::read(&path).unwrap(), data);

    // 推送使用中：不能刪
    let spec = RolloutSpec::parse(
        &json!({"name": "x", "target_version": "0.9.2", "selector": {"all": true}}),
    )
    .unwrap();
    fx.svc.create_rollout(&who(), spec).await.unwrap();
    let e = fx
        .svc
        .delete_release(&who(), "0.9.2".into(), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "release_in_use");
    assert_eq!(e.details.unwrap()["in_use_by_rollouts"], 1);
    // 沒有設定信任錨時只檢查格式與簽章本身
    let fx2 = Fx::new("rel2", |_, _| {}).await;
    let v = fx2
        .svc
        .import_release(
            &who(),
            r.sha256sums.clone(),
            other.sign(r.sha256sums.as_bytes()),
            Some(r.manifest.clone()),
        )
        .await
        .unwrap();
    assert_eq!(v["verified"], false);
    // 清單被改：雜湊不符
    let r3 = release(&signer, "0.9.3", "abcdef2");
    let e = fx
        .svc
        .import_release(
            &who(),
            r3.sha256sums.clone(),
            r3.signature.clone(),
            Some(r3.manifest.replace("false", "true")),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "release_invalid");
}

#[tokio::test]
async fn rollout_offers_downloads_statuses_pauses_and_retries() {
    let signer = TestSigner::new(5);
    let fx = Fx::new("rollout", trust_settings(&signer)).await;
    let r090 = release(&signer, "0.9.0", "1111111");
    let r092 = release(&signer, "0.9.2", "2222222");
    import(&fx, &r090).await;
    import(&fx, &r092).await;
    fx.add_host(A, "0.9.0", "Web", &[], "tar", "ready").await;
    fx.add_host(B, "0.9.0", "Web", &[], "deb", "ready").await;
    fx.add_host(C, "0.9.0", "Web", &[], "tar", "disabled").await;
    fx.connect(A, "0.9.0", "tar");
    fx.connect(B, "0.9.0", "deb");
    let spec = RolloutSpec::parse(&json!({
        "name": "0.9.2 web",
        "target_version": "0.9.2",
        "selector": {"groups": ["web"]},
        "waves": [{"size": 1, "soak_minutes": 0, "max_failure_ratio": 0, "manual_continue": true}, {"size": "rest", "max_failure_ratio": 0.05}],
        "options": {"canary_agent_ids": [A]},
    }))
    .unwrap();
    let preview = fx.svc.preview_rollout(spec.clone(), None).await.unwrap();
    assert_eq!(preview["eligible"], 2);
    assert_eq!(preview["skipped"]["disabled"], 1);
    assert_eq!(preview["waves"], json!([1, 1]));
    assert_eq!(preview["confirm_text"], "升級 2 台");
    let created = fx.svc.create_rollout(&who(), spec.clone()).await.unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(
        id.starts_with("r-20270115-") || id.starts_with("r-"),
        "{id}"
    );
    assert_eq!(created["state"], "draft");
    let e = fx
        .svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "confirmation_required");
    assert_eq!(e.details.unwrap()["confirm_text"], "升級 2 台");
    let v = fx
        .svc
        .submit_rollout(&who(), id.clone(), Some("升級 2 台".into()), None)
        .await
        .unwrap();
    assert_eq!(v["state"], "running");
    assert_eq!(fx.host_state(&id, C).await.state, "skipped");
    // 另一個推送不能涵蓋同樣的主機
    let e = fx.svc.create_rollout(&who(), spec).await.unwrap_err();
    assert_eq!(e.code, "rollout_overlap");

    // Kairos 剛啟動：1 分鐘內不提議
    fx.svc.tick().await.unwrap();
    assert_eq!(fx.svc.offer_of(A), None);
    fx.set_now(T0 + 61_000);
    fx.svc.tick().await.unwrap();
    let up_a = fx.svc.offer_of(A).expect("canary offered");
    assert_eq!(fx.svc.offer_of(B), None, "第二批還沒開始");
    let offer = fx.svc.build_offer(A, &up_a, 9).await.unwrap().unwrap();
    assert_eq!(offer.seq, 9);
    assert_eq!(offer.action, "upgrade");
    assert_eq!(offer.target.version, "0.9.2");
    assert!(offer.artifact.filename.ends_with("linux-x86_64.tar.gz"));
    assert!(offer.rollback_artifact.is_none(), "tar 不需要回滾檔");
    assert_eq!(offer.manifest.as_ref().unwrap().content, r092.manifest);
    // 下載：只有提議中的檔案
    assert!(
        fx.svc
            .offered_file(A, &up_a, &r092.files[1].0)
            .await
            .unwrap()
            .is_none()
    );
    let (path, size) = fx
        .svc
        .offered_file(A, &up_a, &offer.artifact.filename)
        .await
        .unwrap()
        .unwrap();
    let (chunk, eof) = releases::read_chunk(&path, 0, 32768, size).unwrap();
    assert_eq!((chunk.len(), eof), (32768, false));
    let (rest, eof) = releases::read_chunk(&path, 32768, 32768, size).unwrap();
    assert!(eof);
    let mut all = chunk;
    all.extend(rest);
    assert_eq!(
        kairos_proto::release::sha256_hex(&all),
        offer.artifact.sha256
    );
    // 其他主機不能下載
    assert!(
        fx.svc
            .offered_file(B, &up_a, &offer.artifact.filename)
            .await
            .unwrap()
            .is_none()
    );

    let status = |id: &str, state: &str, code: Option<&str>| kairos_proto::mgmt::UpgradeStatus {
        seq: 1,
        ts: "2026-11-20T02:00:00.000Z".into(),
        upgrade_id: id.to_string(),
        state: state.into(),
        from_version: "0.9.0".into(),
        to_version: "0.9.2".into(),
        code: code.map(str::to_string),
        detail: String::new(),
        progress: None,
    };
    for s in ["accepted", "staged", "applying"] {
        fx.svc
            .apply_upgrade_status(A, "0.9.0", status(&up_a, s, None))
            .await
            .unwrap();
        assert_eq!(fx.host_state(&id, A).await.state, s);
    }
    assert_eq!(fx.svc.offer_of(A), None, "交給輔助程式後不再提議");
    // 晚到的較早狀態不覆蓋
    fx.svc
        .apply_upgrade_status(A, "0.9.0", status(&up_a, "accepted", None))
        .await
        .unwrap();
    assert_eq!(fx.host_state(&id, A).await.state, "applying");
    fx.svc
        .apply_upgrade_status(A, "0.9.2", status(&up_a, "succeeded", Some("PLS-I8416")))
        .await
        .unwrap();
    assert_eq!(fx.host_state(&id, A).await.state, "succeeded");
    // canary 結束 → 觀察（0 分鐘）→ 等待人工繼續
    fx.svc.tick().await.unwrap();
    fx.svc.tick().await.unwrap();
    let r = fx
        .svc
        .db
        .read({
            let id = id.clone();
            move |s| Ok(s.get_rollout(&id)?.unwrap())
        })
        .await
        .unwrap();
    assert!(r.awaiting_continue, "{r:?}");
    assert_eq!(fx.svc.offer_of(B), None);
    fx.svc.continue_rollout(&who(), id.clone()).await.unwrap();
    fx.svc.tick().await.unwrap();
    let up_b = fx.svc.offer_of(B).expect("second wave offered");
    let offer_b = fx.svc.build_offer(B, &up_b, 1).await.unwrap().unwrap();
    assert!(offer_b.artifact.filename.ends_with(".deb"));
    let rb = offer_b
        .rollback_artifact
        .as_ref()
        .expect("deb 的回滾檔：目前版本 0.9.0");
    assert!(rb.filename.starts_with("pulse-0.9.0-1111111-"));
    assert!(
        fx.svc
            .offered_file(B, &up_b, &rb.filename)
            .await
            .unwrap()
            .is_some()
    );

    // 簽章失敗：稽核、暫停推送
    fx.svc
        .apply_upgrade_status(B, "0.9.0", status(&up_b, "failed", Some("PLS-E8401")))
        .await
        .unwrap();
    let r = fx
        .svc
        .db
        .read({
            let id = id.clone();
            move |s| Ok(s.get_rollout(&id)?.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(r.state, "paused");
    assert!(r.pause_reason.unwrap().contains("PLS-E8401"));
    let actions = fx.audit_actions().await;
    assert!(actions.iter().any(|a| a == "host.upgrade.signature_failed"));
    assert!(actions.iter().any(|a| a == "rollout.auto_pause"));
    // 重試（新的 upgrade_id）並繼續
    fx.svc
        .retry_host(&who(), id.clone(), B.into())
        .await
        .unwrap();
    let retried = fx.host_state(&id, B).await;
    assert_eq!((retried.state.as_str(), retried.attempts), ("pending", 1));
    assert_ne!(retried.upgrade_id, up_b);
    fx.svc.resume_rollout(&who(), id.clone()).await.unwrap();
    fx.svc.tick().await.unwrap();
    let up_b2 = fx.svc.offer_of(B).unwrap();
    assert_eq!(up_b2, retried.upgrade_id);
    // 舊的 upgrade_id：不再提議
    assert!(fx.svc.build_offer(B, &up_b, 1).await.unwrap().is_none());
    // 回報成功但版本不是目標 → 失敗（KRS-W8402）→ 下一輪失敗比例超過上限而暫停
    fx.svc
        .apply_upgrade_status(B, "0.9.0", status(&up_b2, "succeeded", None))
        .await
        .unwrap();
    let h = fx.host_state(&id, B).await;
    assert_eq!(
        (h.state.as_str(), h.reason.as_deref(), h.code.as_deref()),
        (
            "failed",
            Some("version_mismatch"),
            Some(CODE_VERSION_MISMATCH)
        )
    );
    fx.svc.tick().await.unwrap();
    let r = fx
        .svc
        .db
        .read({
            let id = id.clone();
            move |s| Ok(s.get_rollout(&id)?.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(r.state, "paused");
    // 取消
    let v = fx
        .svc
        .cancel_rollout(&who(), id.clone(), "測試結束".into())
        .await
        .unwrap();
    assert_eq!(v["state"], "cancelled");
    let e = fx
        .svc
        .cancel_rollout(&who(), id.clone(), "x".into())
        .await
        .unwrap_err();
    assert_eq!(e.code, "invalid_state");
    for a in [
        "rollout.create",
        "rollout.submit",
        "rollout.start",
        "rollout.resume",
        "rollout.update",
        "rollout.cancel",
    ] {
        assert!(fx.audit_actions().await.iter().any(|x| x == a), "{a}");
    }
}

#[tokio::test]
async fn rollback_failure_pauses_and_needs_acknowledgement_before_retry() {
    let signer = TestSigner::new(6);
    let fx = Fx::new("rbf", trust_settings(&signer)).await;
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    fx.add_host(A, "0.9.0", "Web", &[], "tar", "ready").await;
    fx.connect(A, "0.9.0", "tar");
    let spec = RolloutSpec::parse(&json!({"name": "x", "target_version": "0.9.2", "selector": {"agent_ids": [A]}, "waves": [{"size": "rest"}]})).unwrap();
    let id = fx.svc.create_rollout(&who(), spec).await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    fx.svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    fx.set_now(T0 + 61_000);
    let mut rx = fx.svc.subscribe();
    fx.svc.tick().await.unwrap();
    let up = fx.svc.offer_of(A).unwrap();
    fx.svc.flush_progress().await;
    let ev = rx.try_recv().expect("rollout.progress");
    assert_eq!(ev.kind, "rollout.progress");
    assert_eq!(ev.data["rollout_id"], id.as_str());
    assert_eq!(ev.data["progress"]["offered"], 1);
    let st = kairos_proto::mgmt::UpgradeStatus {
        seq: 1,
        ts: "2026-11-20T02:00:00.000Z".into(),
        upgrade_id: up.clone(),
        state: "rollback_failed".into(),
        from_version: "0.9.0".into(),
        to_version: "0.9.2".into(),
        code: Some("PLS-E8408".into()),
        detail: "pulse.service failed".into(),
        progress: None,
    };
    fx.svc
        .apply_upgrade_status(A, "0.9.0", st.clone())
        .await
        .unwrap();
    // 重送（at-least-once）：去重
    fx.svc.apply_upgrade_status(A, "0.9.0", st).await.unwrap();
    let r = fx
        .svc
        .db
        .read({
            let id = id.clone();
            move |s| Ok(s.get_rollout(&id)?.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(r.state, "paused");
    assert_eq!(
        fx.audit_actions()
            .await
            .iter()
            .filter(|a| *a == "host.upgrade.rollback_failed")
            .count(),
        1
    );
    let e = fx
        .svc
        .retry_host(&who(), id.clone(), A.into())
        .await
        .unwrap_err();
    assert_eq!(e.code, "invalid_state");
    // 告警的系統規則：自動暫停（回滾失敗，嚴重）與回滾失敗的主機
    let mut bus = fx.registry.bus().subscribe();
    fx.svc.reconcile_alerts().await;
    let mut active = Vec::new();
    while let Ok(ev) = bus.try_recv() {
        if let crate::bus::EventKind::RemoteCondition {
            rule,
            subject,
            active: a,
            details,
            ..
        } = &ev.kind
        {
            assert!(*a);
            active.push((*rule, subject.clone(), details.clone()));
        }
    }
    active.sort_by(|a, b| a.0.cmp(b.0));
    assert_eq!(active.len(), 2, "{active:?}");
    assert_eq!(
        (active[0].0, active[0].1.as_str()),
        ("sys.rollout_paused", id.as_str())
    );
    assert_eq!(active[0].2["cause"], "rollback_failed");
    assert_eq!(
        (active[1].0, active[1].1.as_str()),
        ("sys.upgrade_rollback_failed", A)
    );
    // 沒有改變：不重複發布
    fx.svc.reconcile_alerts().await;
    assert!(bus.try_recv().is_err());
    fx.svc
        .acknowledge_host(&who(), id.clone(), A.into(), Some("已在現場修復".into()))
        .await
        .unwrap();
    fx.svc.resume_rollout(&who(), id.clone()).await.unwrap();
    fx.svc.reconcile_alerts().await;
    let mut cleared = 0;
    while let Ok(ev) = bus.try_recv() {
        if let crate::bus::EventKind::RemoteCondition { active: false, .. } = &ev.kind {
            cleared += 1;
        }
    }
    assert_eq!(cleared, 2, "標記處理、推送繼續後恢復");
    fx.svc
        .retry_host(&who(), id.clone(), A.into())
        .await
        .unwrap();
    assert_eq!(fx.host_state(&id, A).await.state, "pending");
}

#[tokio::test]
async fn download_slots_are_limited_and_released_when_idle_or_staged() {
    let fx = Fx::new("dl", |s, _| s.max_concurrent_downloads = 1).await;
    assert!(fx.svc.acquire_download(A));
    assert!(fx.svc.acquire_download(A), "同一台繼續下載");
    assert!(!fx.svc.acquire_download(B), "名額已滿 → rate_limited");
    assert_eq!(fx.svc.downloads_in_progress(), 1);
    fx.svc.on_staged(A);
    assert!(fx.svc.acquire_download(B));
}

#[test]
fn pulse_codes_map_to_mirrored_kairos_codes() {
    assert_eq!(mirror_code("PLS-W0501"), Some("KRS-W0501"));
    assert_eq!(mirror_code("PLS-E8402"), Some("KRS-W8402"));
    assert_eq!(mirror_code("PLS-E8403"), None);
    assert_eq!(mirror_code("KRS-W8402"), None);
}

#[tokio::test]
async fn pending_carries_config_rev_and_upgrade_id_by_session() {
    let fx = Fx::new("pending", |_, _| {}).await;
    fx.svc.apply_desired(&[(A.to_string(), 42)]);
    fx.svc
        .set_offer(A, Some("9d3e7a10-4b2c-4f6e-8a1d-2c3b4a5f6e70"));
    let both = kairos_proto::mgmt::MgmtSession {
        config: true,
        upgrade: true,
    };
    let p = fx.svc.pending(both, A).unwrap();
    assert_eq!(p.config_rev, Some(42));
    assert_eq!(
        p.upgrade_id.as_deref(),
        Some("9d3e7a10-4b2c-4f6e-8a1d-2c3b4a5f6e70")
    );
    let cfg_only = kairos_proto::mgmt::MgmtSession {
        config: true,
        upgrade: false,
    };
    assert_eq!(fx.svc.pending(cfg_only, A).unwrap().upgrade_id, None);
    assert!(
        fx.svc
            .pending(kairos_proto::mgmt::MgmtSession::NONE, A)
            .is_none()
    );
    assert_eq!(fx.svc.pending(both, B).unwrap().config_rev, Some(0));
}

#[tokio::test]
async fn cancel_withdraws_offers_but_records_an_apply_that_already_started() {
    let signer = TestSigner::new(13);
    let fx = Fx::new("cancel", trust_settings(&signer)).await;
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    fx.add_host(A, "0.9.0", "Web", &[], "tar", "ready").await;
    fx.connect(A, "0.9.0", "tar");
    let spec = RolloutSpec::parse(&json!({"name": "x", "target_version": "0.9.2", "selector": {"agent_ids": [A]}, "waves": [{"size": "rest"}]})).unwrap();
    let id = fx.svc.create_rollout(&who(), spec).await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    fx.svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    fx.set_now(T0 + 61_000);
    fx.svc.tick().await.unwrap();
    let up = fx.svc.offer_of(A).unwrap();
    fx.svc
        .cancel_rollout(&who(), id.clone(), "改期".into())
        .await
        .unwrap();
    assert_eq!(fx.svc.offer_of(A), None, "取消後不再提議");
    assert!(fx.svc.build_offer(A, &up, 1).await.unwrap().is_none());
    assert_eq!(fx.host_state(&id, A).await.state, "cancelled");
    // Pulse 其實已經交給輔助程式：照實記錄
    let st = |state: &str| kairos_proto::mgmt::UpgradeStatus {
        seq: 1,
        ts: "2026-11-20T02:00:00.000Z".into(),
        upgrade_id: up.clone(),
        state: state.into(),
        from_version: "0.9.0".into(),
        to_version: "0.9.2".into(),
        code: None,
        detail: String::new(),
        progress: None,
    };
    fx.svc
        .apply_upgrade_status(A, "0.9.0", st("staged"))
        .await
        .unwrap();
    assert_eq!(
        fx.host_state(&id, A).await.state,
        "cancelled",
        "取消後的下載進度不改變"
    );
    fx.svc
        .apply_upgrade_status(A, "0.9.0", st("applying"))
        .await
        .unwrap();
    fx.svc
        .apply_upgrade_status(A, "0.9.2", st("succeeded"))
        .await
        .unwrap();
    assert_eq!(fx.host_state(&id, A).await.state, "succeeded");
}

/// 主機上線但沒有協商遠端管理（例如沒有 mTLS）：只在狀態表，hello 帶 `install`
fn connect_hello_only(fx: &Fx, id: &str, version: &str, method: &str, upgrade: &str) {
    fx.registry
        .register(
            crate::registry::next_conn_id(),
            crate::registry::HelloInfo {
                agent_id: id.to_string(),
                hostname: id.to_string(),
                agent_version: version.to_string(),
                proto: 2,
                host: Some(Arc::new(kairos_proto::HostInfo {
                    os: Some(kairos_proto::OsInfo {
                        name: Some("Ubuntu".into()),
                        arch: Some("x86_64".into()),
                        ..Default::default()
                    }),
                    install: Some(kairos_proto::mgmt::InstallInfo {
                        method: Some(method.into()),
                        upgrade: Some(upgrade.into()),
                        config: Some("enabled".into()),
                    }),
                    tags: Some(vec!["site:b".into()]),
                    ..Default::default()
                })),
                peer: "127.0.0.1:1".into(),
                offline_after: Duration::from_secs(75),
                client_chain: None,
            },
            Instant::now(),
            chrono::Utc::now(),
        )
        .unwrap();
}

#[tokio::test]
async fn preview_and_submit_use_the_live_hello_instead_of_the_inventory() {
    let signer = TestSigner::new(21);
    let fx = Fx::new("livehello", trust_settings(&signer)).await;
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    // 清冊還是舊的資料（每 30 秒才同步）：0.8.0、沒有 install、沒有標籤
    fx.add_host(A, "0.8.0", "Web", &[], "tar", "ready").await;
    fx.sql(|c| {
        c.execute(
            "UPDATE hosts SET install_method = NULL, install_upgrade = NULL, os = NULL",
            [],
        )
        .unwrap();
    })
    .await;
    let spec = RolloutSpec::parse(&json!({"name": "x", "target_version": "0.9.2", "selector": {"tags": ["site:b"]}, "waves": [{"size": "rest"}]})).unwrap();
    let p = fx.svc.preview_rollout(spec.clone(), None).await.unwrap();
    assert_eq!(p["matched"], 0, "清冊沒有標籤：比對不到");
    // 剛連線、hello 已升到 0.9.0（tar、就緒、標籤 site:b），沒有協商 upgrade：資格依 hello
    connect_hello_only(&fx, A, "0.9.0", "tar", "ready");
    let p = fx.svc.preview_rollout(spec.clone(), None).await.unwrap();
    assert_eq!(
        (p["matched"].as_i64(), p["eligible"].as_i64()),
        (Some(1), Some(1)),
        "{p}"
    );
    assert_eq!(p["confirm_text"], "升級 1 台");
    let id = fx.svc.create_rollout(&who(), spec).await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    fx.svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    let h = fx.host_state(&id, A).await;
    assert_eq!(
        (h.state.as_str(), h.from_version.as_deref()),
        ("pending", Some("0.9.0"))
    );
    // 單台主機的 hello（`/hosts/{id}/upgrade` 使用）
    let l = fx.svc.live_hello(A).unwrap();
    assert_eq!(
        (l.agent_version.as_str(), l.install_upgrade.as_deref()),
        ("0.9.0", Some("ready"))
    );
    assert!(fx.svc.live_hello(B).is_none());
    // 協商了遠端管理的連線優先（發提議時使用的資訊）
    fx.connect(A, "0.9.1", "deb");
    let l = fx.svc.live_hello(A).unwrap();
    assert_eq!(
        (l.agent_version.as_str(), l.install_method.as_deref()),
        ("0.9.1", Some("deb"))
    );
}

#[tokio::test]
async fn hosts_without_a_rollback_artifact_need_the_longer_l2_phrase() {
    let signer = TestSigner::new(22);
    let fx = Fx::new("norb", trust_settings(&signer)).await;
    // 版本庫只有目標版本：deb 主機（0.9.0）沒有回滾檔
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    fx.add_host(A, "0.9.0", "Web", &[], "deb", "ready").await;
    fx.add_host(B, "0.9.0", "Web", &[], "tar", "ready").await;
    let body = |allow: bool| json!({"name": "x", "target_version": "0.9.2", "selector": {"all": true}, "waves": [{"size": "rest"}], "options": {"allow_no_rollback": allow}});
    let p = fx
        .svc
        .preview_rollout(RolloutSpec::parse(&body(false)).unwrap(), None)
        .await
        .unwrap();
    assert_eq!(
        (p["eligible"].as_i64(), p["skipped"]["no_rollback"].as_i64()),
        (Some(1), Some(1))
    );
    assert_eq!(p["confirm_text"], "升級 1 台");
    let spec = RolloutSpec::parse(&body(true)).unwrap();
    let p = fx.svc.preview_rollout(spec.clone(), None).await.unwrap();
    assert_eq!(
        (p["eligible"].as_i64(), p["no_rollback"].as_i64()),
        (Some(2), Some(1))
    );
    assert_eq!(p["confirm_text"], "升級 2 台（無法自動回滾 1 台）");
    assert_eq!(p["confirm_level"], "L2");
    let id = fx.svc.create_rollout(&who(), spec).await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    // 只接受設計包的字串：「升級 N 台」、半形括號、台數不符都不行
    for wrong in [
        "升級 2 台",
        "升級 2 台(無法自動回滾 1 台)",
        "升級 2 台（無法自動回滾 2 台）",
    ] {
        let e = fx
            .svc
            .submit_rollout(&who(), id.clone(), Some(wrong.into()), None)
            .await
            .unwrap_err();
        assert_eq!(e.code, "confirmation_required", "{wrong}");
        let d = e.details.unwrap();
        assert_eq!(d["confirm_text"], "升級 2 台（無法自動回滾 1 台）");
        assert_eq!(d["no_rollback"], 1);
    }
    let v = fx
        .svc
        .submit_rollout(
            &who(),
            id.clone(),
            Some("升級 2 台（無法自動回滾 1 台）".into()),
            None,
        )
        .await
        .unwrap();
    assert_eq!(v["state"], "running");
}

#[tokio::test]
async fn pause_cause_and_open_rollback_failures_are_in_the_rollout_json() {
    let signer = TestSigner::new(23);
    let fx = Fx::new("cause", trust_settings(&signer)).await;
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    fx.add_host(A, "0.9.0", "Web", &[], "tar", "ready").await;
    fx.connect(A, "0.9.0", "tar");
    let spec = RolloutSpec::parse(&json!({"name": "x", "target_version": "0.9.2", "selector": {"agent_ids": [A]}, "waves": [{"size": "rest"}]})).unwrap();
    let id = fx.svc.create_rollout(&who(), spec).await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let v = fx
        .svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    assert_eq!(v["pause_cause"], Value::Null);
    assert_eq!(v["rollback_failed_unacknowledged"], 0);
    // 主控台暫停：manual
    let v = fx
        .svc
        .pause_rollout(&who(), id.clone(), "先停".into())
        .await
        .unwrap();
    assert_eq!(v["pause_cause"], "manual");
    assert_eq!(v["next_action"]["cause"], "manual");
    let v = fx.svc.resume_rollout(&who(), id.clone()).await.unwrap();
    assert_eq!(v["pause_cause"], Value::Null);
    // 回滾失敗：自動暫停（rollback_failed），SSE 的 rollout.progress 也帶 pause_cause
    fx.set_now(T0 + 61_000);
    fx.svc.tick().await.unwrap();
    let up = fx.svc.offer_of(A).unwrap();
    fx.svc.flush_progress().await;
    let mut rx = fx.svc.subscribe();
    fx.svc
        .apply_upgrade_status(
            A,
            "0.9.0",
            kairos_proto::mgmt::UpgradeStatus {
                seq: 1,
                ts: "2026-11-20T02:00:00.000Z".into(),
                upgrade_id: up,
                state: "rollback_failed".into(),
                from_version: "0.9.0".into(),
                to_version: "0.9.2".into(),
                code: Some("PLS-E8408".into()),
                detail: "x".into(),
                progress: None,
            },
        )
        .await
        .unwrap();
    fx.svc.flush_progress().await;
    let ev = rx.try_recv().expect("rollout.progress");
    assert_eq!(ev.data["pause_cause"], "rollback_failed");
    assert_eq!(ev.data["next_action"]["cause"], "rollback_failed");
    let (r, hosts) = {
        let id = id.clone();
        fx.svc
            .db
            .read(move |s| Ok((s.get_rollout(&id)?.unwrap(), s.rollout_hosts(&id)?)))
            .await
            .unwrap()
    };
    let v = rollout_json(&r, &hosts, T0, false);
    assert_eq!(v["pause_cause"], "rollback_failed");
    assert_eq!(v["rollback_failed_unacknowledged"], 1);
    fx.svc
        .acknowledge_host(&who(), id.clone(), A.into(), None)
        .await
        .unwrap();
    let hosts = {
        let id = id.clone();
        fx.svc
            .db
            .read(move |s| Ok(s.rollout_hosts(&id)?))
            .await
            .unwrap()
    };
    let v = rollout_json(&r, &hosts, T0, false);
    assert_eq!(v["rollback_failed_unacknowledged"], 0, "已標記為已處理");
    assert_eq!(v["progress"]["rollback_failed"], 1, "progress 仍含已處理的");
}

/// 升級中主機的離線告警抑制（REMOTE.md 第 4.7.5 節）：進入 `applying` 時發布期限、到達終止狀態或期限已過時
/// 解除；只有這台主機自己、對 Kairos 提議給它的 upgrade_id 回報才算；Kairos 重新啟動後從資料庫重建
#[tokio::test]
async fn upgrade_windows_follow_applying_hosts_and_survive_restart() {
    let signer = TestSigner::new(6);
    let fx = Fx::new("window", trust_settings(&signer)).await;
    import(&fx, &release(&signer, "0.9.0", "1111111")).await;
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    fx.add_host(A, "0.9.0", "Web", &[], "tar", "ready").await;
    fx.add_host(B, "0.9.0", "Web", &[], "tar", "ready").await;
    fx.connect(A, "0.9.0", "tar");
    fx.connect(B, "0.9.0", "tar");
    let spec = RolloutSpec::parse(&json!({
        "name": "0.9.2 web",
        "target_version": "0.9.2",
        "selector": {"groups": ["web"]},
        "waves": [{"size": "rest", "max_failure_ratio": 1}],
        "options": {"health_timeout_secs": 300},
    }))
    .unwrap();
    let id = fx.svc.create_rollout(&who(), spec).await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    fx.svc
        .submit_rollout(&who(), id.clone(), Some("升級 2 台".into()), None)
        .await
        .unwrap();
    fx.set_now(T0 + 61_000);
    fx.svc.tick().await.unwrap();
    let up_a = fx.svc.offer_of(A).unwrap();
    let up_b = fx.svc.offer_of(B).unwrap();
    let status = |up: &str, state: &str| kairos_proto::mgmt::UpgradeStatus {
        seq: 1,
        ts: "2026-11-20T02:00:00.000Z".into(),
        upgrade_id: up.to_string(),
        state: state.into(),
        from_version: "0.9.0".into(),
        to_version: "0.9.2".into(),
        code: None,
        detail: String::new(),
        progress: None,
    };
    let windows = |bus: &mut tokio::sync::broadcast::Receiver<Arc<crate::bus::BusEvent>>| {
        let mut v = Vec::new();
        while let Ok(ev) = bus.try_recv() {
            if let crate::bus::EventKind::UpgradeWindow {
                agent_id,
                rollout_id,
                until_ms,
            } = &ev.kind
            {
                v.push((agent_id.clone(), rollout_id.clone(), *until_ms));
            }
        }
        v.sort();
        v
    };
    let mut bus = fx.registry.bus().subscribe();
    // 下載、暫存不抑制
    for s in ["accepted", "staged"] {
        fx.svc
            .apply_upgrade_status(A, "0.9.0", status(&up_a, s))
            .await
            .unwrap();
    }
    assert!(windows(&mut bus).is_empty());
    // 其他主機拿 A 的 upgrade_id 回報 applying、或回報不存在的 upgrade_id：不抑制（不能藉此隱藏離線）
    fx.svc
        .apply_upgrade_status(B, "0.9.0", status(&up_a, "applying"))
        .await
        .unwrap();
    fx.svc
        .apply_upgrade_status(
            B,
            "0.9.0",
            status("00000000-0000-4000-8000-000000000000", "applying"),
        )
        .await
        .unwrap();
    assert!(windows(&mut bus).is_empty());
    assert_eq!(fx.svc.upgrade_windows(), 0);
    // A、B 交給輔助程式：期限為 applying 的時間＋health_timeout_secs（300 秒）＋10 分鐘
    let applied = T0 + 61_000;
    let until = applied + 300_000 + 600_000;
    assert_eq!(rollout::suppress_until(applied, 300), until);
    for (agent, up) in [(A, &up_a), (B, &up_b)] {
        fx.svc
            .apply_upgrade_status(agent, "0.9.0", status(up, "applying"))
            .await
            .unwrap();
    }
    assert_eq!(
        windows(&mut bus),
        vec![
            (A.to_string(), id.clone(), Some(until)),
            (B.to_string(), id.clone(), Some(until))
        ]
    );
    // 重送與調和：沒有改變就不重複發布
    fx.svc
        .apply_upgrade_status(A, "0.9.0", status(&up_a, "applying"))
        .await
        .unwrap();
    fx.set_now(applied + 10_000);
    fx.svc.reconcile_upgrade_windows().await;
    assert!(windows(&mut bus).is_empty());
    // 推送暫停後，已在替換中的主機照常抑制
    fx.svc
        .pause_rollout(&who(), id.clone(), "看一下".into())
        .await
        .unwrap();
    fx.svc.reconcile_upgrade_windows().await;
    assert!(windows(&mut bus).is_empty());
    assert_eq!(fx.svc.upgrade_windows(), 2);
    // B 升級完成：立即解除
    fx.svc
        .apply_upgrade_status(B, "0.9.2", status(&up_b, "succeeded"))
        .await
        .unwrap();
    assert_eq!(windows(&mut bus), vec![(B.to_string(), id.clone(), None)]);
    // Kairos 重新啟動：從資料庫重建（只有還在 applying 的 A）
    let registry2 = Arc::new(crate::registry::Registry::new());
    let n = fx.now.clone();
    let svc2 = RemoteService::open(StartParams {
        settings: RemoteSettings::default(),
        data_dir: fx.dir.clone(),
        db_path: fx.dir.join(crate::store::sqlite::DB_FILE),
        registry: registry2.clone(),
        shutdown: Arc::new(crate::shutdown::Shutdown::new()),
        clock: Arc::new(move || n.load(Ordering::SeqCst)),
    })
    .await
    .unwrap();
    let mut bus2 = registry2.bus().subscribe();
    svc2.reconcile_upgrade_windows().await;
    assert_eq!(
        windows(&mut bus2),
        vec![(A.to_string(), id.clone(), Some(until))]
    );
    // 期限已過而 A 仍在 applying（沒有回報）：解除抑制，離線照常告警
    fx.set_now(until + 1);
    fx.svc.reconcile_upgrade_windows().await;
    assert_eq!(windows(&mut bus), vec![(A.to_string(), id.clone(), None)]);
    assert_eq!(fx.svc.upgrade_windows(), 0);
    svc2.reconcile_upgrade_windows().await;
    assert_eq!(windows(&mut bus2), vec![(A.to_string(), id.clone(), None)]);
    // 期限後才啟動的 Kairos 不會發布已過期的抑制
    fx.svc.reconcile_upgrade_windows().await;
    assert!(windows(&mut bus).is_empty());
}

// ---- 0.10：排程、維護時段與自訂批次 ----

fn status(up: &str, state: &str) -> kairos_proto::mgmt::UpgradeStatus {
    kairos_proto::mgmt::UpgradeStatus {
        seq: 1,
        ts: "2027-01-15T08:00:00.000Z".into(),
        upgrade_id: up.to_string(),
        state: state.into(),
        from_version: "0.9.0".into(),
        to_version: "0.9.2".into(),
        code: None,
        detail: String::new(),
        progress: None,
    }
}

const MIN: i64 = 60_000;

/// T0 是 2027-01-15T08:00:00Z：排程 08:30 開始、每天 09:00～10:00（UTC）的時段
#[tokio::test]
async fn scheduled_rollouts_start_later_and_only_offer_inside_windows() {
    let signer = TestSigner::new(21);
    let fx = Fx::new("schedule", trust_settings(&signer)).await;
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    fx.add_host(A, "0.9.0", "Web", &[], "tar", "ready").await;
    fx.connect(A, "0.9.0", "tar");
    let ts = crate::api::http::ts_ms;
    let body = |name: &str| {
        json!({
            "name": name,
            "target_version": "0.9.2",
            "selector": {"agent_ids": [A]},
            "waves": [{"size": "rest"}],
            "schedule": {
                "starts_at": ts(T0 + 30 * MIN),
                "windows": [{"start": "09:00", "end": "10:00"}],
                "timezone": "UTC",
            },
        })
    };
    let spec = RolloutSpec::parse(&body("x")).unwrap();
    let preview = fx.svc.preview_rollout(spec.clone(), None).await.unwrap();
    assert_eq!(
        preview["window"],
        json!({"open": false, "next": ts(T0 + 60 * MIN)})
    );
    assert_eq!(
        preview["wave_templates"]["recommended"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let created = fx.svc.create_rollout(&who(), spec).await.unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["schedule"]["windows"][0]["start"], "09:00");
    assert_eq!(created["schedule"]["timezone"], "UTC");
    // 送出：對象固定，但還沒到 starts_at → scheduled，不寫 rollout.start
    let v = fx
        .svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    assert_eq!(v["state"], "scheduled");
    assert_eq!(v["next_action"]["code"], "scheduled");
    assert_eq!(v["next_action"]["starts_at"], ts(T0 + 30 * MIN));
    assert_eq!(fx.host_state(&id, A).await.state, "pending");
    assert!(
        !fx.audit_actions()
            .await
            .contains(&"rollout.start".to_string())
    );
    fx.set_now(T0 + 2 * MIN);
    fx.svc.tick().await.unwrap();
    assert_eq!(fx.svc.offer_of(A), None, "還沒到開始時間");
    // 已排程的主機算在進行中的推送裡（rollout_overlap）
    let other = RolloutSpec::parse(
        &json!({"name": "y", "target_version": "0.9.2", "selector": {"agent_ids": [A]}}),
    )
    .unwrap();
    let e = fx.svc.create_rollout(&who(), other).await.unwrap_err();
    assert_eq!(e.code, "rollout_overlap");
    // 修改已排程的推送：退回草稿、刪除已固定的對象，要重新送出
    let v = fx
        .svc
        .update_rollout(&who(), id.clone(), RolloutSpec::parse(&body("x2")).unwrap())
        .await
        .unwrap();
    assert_eq!(
        (v["state"].as_str(), v["name"].as_str()),
        (Some("draft"), Some("x2"))
    );
    let rid = id.clone();
    let hosts = fx
        .svc
        .db
        .read(move |s| Ok(s.rollout_hosts(&rid)?.len()))
        .await
        .unwrap();
    assert_eq!(hosts, 0);
    let v = fx
        .svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    assert_eq!(v["state"], "scheduled");
    // 到了開始時間：running（rollout.start 由 system 寫入），但 08:31 在時段外 → 不提議
    fx.set_now(T0 + 31 * MIN);
    fx.svc.tick().await.unwrap();
    let rid = id.clone();
    let r = fx
        .svc
        .db
        .read(move |s| Ok(s.get_rollout(&rid)?.unwrap()))
        .await
        .unwrap();
    assert_eq!(r.state, "running");
    assert_eq!(r.started_at, Some(T0 + 31 * MIN));
    assert!(
        fx.audit_actions()
            .await
            .contains(&"rollout.start".to_string())
    );
    assert_eq!(fx.svc.offer_of(A), None, "時段外不提議");
    let na = rollout_json(&r, &[], fx.svc.now_ms(), false)["next_action"].clone();
    assert_eq!(
        na,
        json!({"code": "waiting_window", "until": ts(T0 + 60 * MIN)})
    );
    // 進入時段：提議，apply_window 到時段結束為止
    fx.set_now(T0 + 65 * MIN);
    fx.svc.tick().await.unwrap();
    let up = fx.svc.offer_of(A).expect("時段內提議");
    let offer = fx.svc.build_offer(A, &up, 1).await.unwrap().unwrap();
    assert_eq!(offer.apply_window.not_before, ts(T0 + 65 * MIN));
    assert_eq!(offer.apply_window.not_after, ts(T0 + 120 * MIN));
    fx.svc
        .apply_upgrade_status(A, "0.9.0", status(&up, "staged"))
        .await
        .unwrap();
    // 時段結束後 Pulse 交付前的再確認：upgrade_unavailable（不會在時段外替換）
    fx.set_now(T0 + 120 * MIN + 1000);
    assert!(fx.svc.build_offer(A, &up, 2).await.unwrap().is_none());
    fx.svc.tick().await.unwrap();
    assert_eq!(
        fx.host_state(&id, A).await.state,
        "staged",
        "留給 Pulse 回報 expired 的時間"
    );
    // 之後仍停在 staged：回到 pending、換新的 upgrade_id，推送不結束
    fx.set_now(T0 + 123 * MIN);
    fx.svc.tick().await.unwrap();
    let h = fx.host_state(&id, A).await;
    assert_eq!((h.state.as_str(), h.attempts), ("pending", 1));
    assert_eq!(h.reason.as_deref(), Some("window_closed"));
    assert_ne!(h.upgrade_id, up);
    assert_eq!(fx.svc.offer_of(A), None);
    // 隔天的時段：以新的 upgrade_id 重新提議
    let day = 24 * 60 * MIN;
    fx.set_now(T0 + day + 61 * MIN);
    fx.svc.tick().await.unwrap();
    let up2 = fx.svc.offer_of(A).expect("下一個時段重新提議");
    assert_eq!(up2, h.upgrade_id);
    let offer = fx.svc.build_offer(A, &up2, 3).await.unwrap().unwrap();
    assert_eq!(offer.apply_window.not_after, ts(T0 + day + 120 * MIN));
    // Pulse 在時段結束時自己回報 expired：同樣重新排入，再下一個時段提議
    fx.svc
        .apply_upgrade_status(A, "0.9.0", status(&up2, "expired"))
        .await
        .unwrap();
    assert_eq!(fx.host_state(&id, A).await.state, "expired");
    fx.set_now(T0 + day + 121 * MIN);
    fx.svc.tick().await.unwrap();
    let h = fx.host_state(&id, A).await;
    assert_eq!((h.state.as_str(), h.attempts), ("pending", 2));
    fx.set_now(T0 + 2 * day + 61 * MIN);
    fx.svc.tick().await.unwrap();
    let up3 = fx.svc.offer_of(A).unwrap();
    assert_ne!(up3, up2);
    // 這次在時段內替換並成功 → 推送完成
    for s in ["applying", "succeeded"] {
        let v = if s == "succeeded" { "0.9.2" } else { "0.9.0" };
        fx.svc
            .apply_upgrade_status(A, v, status(&up3, s))
            .await
            .unwrap();
    }
    fx.svc.tick().await.unwrap();
    let rid = id.clone();
    let r = fx
        .svc
        .db
        .read(move |s| Ok(s.get_rollout(&rid)?.unwrap()))
        .await
        .unwrap();
    assert_eq!(r.state, "completed");
}

#[tokio::test]
async fn ends_at_expires_hosts_that_have_not_started_and_waits_for_applying_ones() {
    let signer = TestSigner::new(22);
    let fx = Fx::new("ends", trust_settings(&signer)).await;
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    for a in [A, B] {
        fx.add_host(a, "0.9.0", "Web", &[], "tar", "ready").await;
        fx.connect(a, "0.9.0", "tar");
    }
    let ts = crate::api::http::ts_ms;
    // 有時段而沒有指定時區：以預設（alerts.timezone，沒有設定時 +08:00）填入
    fx.svc.set_default_timezone("+09:00");
    let spec = |extra: Value| {
        let mut b = json!({
            "name": "x",
            "target_version": "0.9.2",
            "selector": {"all": true},
            "waves": [{"size": "rest", "max_failure_ratio": 1.0}],
            "options": {"max_in_progress": 1},
        });
        b["schedule"] = extra;
        RolloutSpec::parse(&b).unwrap()
    };
    let e = fx
        .svc
        .create_rollout(&who(), spec(json!({"ends_at": ts(T0 - 1)})))
        .await
        .unwrap_err();
    assert_eq!(e.code, "invalid_parameter");
    let tz = fx
        .svc
        .create_rollout(
            &who(),
            spec(json!({"windows": [{"start": "00:00", "end": "23:59"}]})),
        )
        .await
        .unwrap();
    assert_eq!(tz["schedule"]["timezone"], "+09:00");
    let created = fx
        .svc
        .create_rollout(&who(), spec(json!({"ends_at": ts(T0 + 120 * MIN)})))
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let v = fx
        .svc
        .submit_rollout(&who(), id.clone(), Some("升級 2 台".into()), None)
        .await
        .unwrap();
    assert_eq!(v["state"], "running", "沒有 starts_at：立即開始");
    fx.set_now(T0 + 2 * MIN);
    fx.svc.tick().await.unwrap();
    // 同時最多 1 台：一台被提議，另一台還在 pending
    let (first, second) = if fx.svc.offer_of(A).is_some() {
        (A, B)
    } else {
        (B, A)
    };
    let up = fx.svc.offer_of(first).unwrap();
    let offer = fx.svc.build_offer(first, &up, 1).await.unwrap().unwrap();
    assert_eq!(
        offer.apply_window.not_after,
        ts(T0 + 120 * MIN),
        "apply_window 不超過 ends_at"
    );
    fx.svc
        .apply_upgrade_status(first, "0.9.0", status(&up, "applying"))
        .await
        .unwrap();
    // ends_at 到了：還沒開始的主機 expired（rollout_ended），替換中的照常完成後推送才結束
    fx.set_now(T0 + 120 * MIN);
    fx.svc.tick().await.unwrap();
    let h = fx.host_state(&id, second).await;
    assert_eq!(
        (h.state.as_str(), h.reason.as_deref()),
        ("expired", Some("rollout_ended"))
    );
    assert_eq!(fx.host_state(&id, first).await.state, "applying");
    let state = |fx: &Fx, id: &str| {
        let (svc, id) = (fx.svc.clone(), id.to_string());
        async move {
            svc.db
                .read(move |s| Ok(s.get_rollout(&id)?.unwrap().state))
                .await
                .unwrap()
        }
    };
    assert_eq!(state(&fx, &id).await, "running");
    fx.svc
        .apply_upgrade_status(first, "0.9.2", status(&up, "succeeded"))
        .await
        .unwrap();
    fx.svc.tick().await.unwrap();
    assert_eq!(state(&fx, &id).await, "completed");
    assert!(
        fx.audit_actions()
            .await
            .contains(&"rollout.complete".to_string())
    );
    // 已過 ends_at 的草稿不能送出
    let late = fx
        .svc
        .create_rollout(&who(), spec(json!({"ends_at": ts(T0 + 121 * MIN)})))
        .await
        .unwrap();
    fx.set_now(T0 + 122 * MIN);
    let e = fx
        .svc
        .submit_rollout(
            &who(),
            late["id"].as_str().unwrap().to_string(),
            Some("升級 1 台".into()),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "schedule_ended");
}

#[tokio::test]
async fn four_wave_rollouts_advance_wave_by_wave() {
    let signer = TestSigner::new(23);
    let fx = Fx::new("waves4", trust_settings(&signer)).await;
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    let hosts = [A, B, C, D];
    for a in hosts {
        fx.add_host(a, "0.9.0", "Web", &[], "tar", "ready").await;
        fx.connect(a, "0.9.0", "tar");
    }
    let spec = RolloutSpec::parse(&json!({
        "name": "四批",
        "target_version": "0.9.2",
        "selector": {"all": true},
        "waves": [
            {"size": 1, "soak_minutes": 0, "max_failure_ratio": 0, "manual_continue": true},
            {"size": 1, "soak_minutes": 10},
            {"size": "50%", "soak_minutes": 0},
            {"size": "rest"},
        ],
    }))
    .unwrap();
    let preview = fx.svc.preview_rollout(spec.clone(), None).await.unwrap();
    assert_eq!(preview["waves"], json!([1, 1, 1, 1]));
    let id = fx.svc.create_rollout(&who(), spec).await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let v = fx
        .svc
        .submit_rollout(&who(), id.clone(), Some("升級 4 台".into()), None)
        .await
        .unwrap();
    assert_eq!(v["waves_total"], 4);
    let mut now = T0 + 2 * MIN;
    let mut done = 0;
    // 每一批：提議一台 → 成功 → （第 1 批：人工繼續；第 2 批：觀察 10 分鐘）→ 下一批
    for wave in 0..4i64 {
        fx.set_now(now);
        fx.svc.tick().await.unwrap();
        let offered: Vec<&str> = hosts
            .iter()
            .copied()
            .filter(|a| fx.svc.offer_of(a).is_some())
            .collect();
        assert_eq!(offered.len(), 1, "第 {} 批只提議這一批的主機", wave + 1);
        let a = offered[0];
        assert_eq!(fx.host_state(&id, a).await.wave, wave);
        let up = fx.svc.offer_of(a).unwrap();
        fx.svc
            .apply_upgrade_status(a, "0.9.0", status(&up, "applying"))
            .await
            .unwrap();
        fx.svc
            .apply_upgrade_status(a, "0.9.2", status(&up, "succeeded"))
            .await
            .unwrap();
        done += 1;
        // 這批結束 → 開始觀察
        fx.svc.tick().await.unwrap();
        let rid = id.clone();
        let r = fx
            .svc
            .db
            .read(move |s| Ok(s.get_rollout(&rid)?.unwrap()))
            .await
            .unwrap();
        match wave {
            0 => {
                // 觀察期 0 分鐘、需要人工繼續
                fx.svc.tick().await.unwrap();
                let e = fx.svc.offer_of(hosts[0]).is_some() as usize
                    + hosts
                        .iter()
                        .filter(|a| fx.svc.offer_of(a).is_some())
                        .count();
                assert_eq!(e, 0, "等待繼續時不提議下一批");
                fx.svc.continue_rollout(&who(), id.clone()).await.unwrap();
            }
            1 => {
                assert_eq!(r.soak_until, Some(now + 10 * MIN));
                fx.set_now(now + 5 * MIN);
                fx.svc.tick().await.unwrap();
                assert!(
                    hosts.iter().all(|a| fx.svc.offer_of(a).is_none()),
                    "觀察中不提議"
                );
                now += 10 * MIN;
                fx.set_now(now);
                fx.svc.tick().await.unwrap();
            }
            2 => {
                // 觀察期 0 分鐘、不需要人工：下一輪自動進最後一批
                fx.svc.tick().await.unwrap();
            }
            _ => assert_eq!(r.state, "completed"),
        }
        now += MIN;
    }
    assert_eq!(done, 4);
    let rid = id.clone();
    let r = fx
        .svc
        .db
        .read(move |s| Ok(s.get_rollout(&rid)?.unwrap()))
        .await
        .unwrap();
    assert_eq!((r.state.as_str(), r.current_wave), ("completed", 3));
}

/// 雙人覆核（REMOTE.md 第 5.4 節）：送出後待核准、不能核准自己建立或送出的推送（KRS-E6106）、退回回到草稿、
/// 修改待核准的推送要重新送審、核准後才開始（有 starts_at 時先排程）
#[tokio::test]
async fn approval_gates_the_rollout_until_someone_else_approves() {
    let signer = TestSigner::new(23);
    let fx = Fx::new("approval", |s, dir| {
        trust_settings(&signer)(s, dir);
        s.rollout_approval = "required".into();
    })
    .await;
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    fx.add_host(A, "0.9.0", "Web", &[], "tar", "ready").await;
    fx.connect(A, "0.9.0", "tar");
    let other = Actor {
        actor: "user:bob".into(),
        ip: None,
    };
    let third = Actor {
        actor: "user:carol".into(),
        ip: None,
    };
    let body = |name: &str| json!({"name": name, "target_version": "0.9.2", "selector": {"agent_ids": [A]}, "waves": [{"size": "rest"}]});
    let created = fx
        .svc
        .create_rollout(&who(), RolloutSpec::parse(&body("x")).unwrap())
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    // 草稿不能核准
    let e = fx
        .svc
        .approve_rollout(&other, id.clone())
        .await
        .unwrap_err();
    assert_eq!(e.code, "invalid_state");
    // 送出（另一位管理員 bob）：待核准，對象已固定但不提議、沒有 rollout.start
    let v = fx
        .svc
        .submit_rollout(&other, id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    assert_eq!(v["state"], "pending_approval");
    assert_eq!(v["next_action"]["code"], "pending_approval");
    assert_eq!(v["submitted_by"], "user:bob");
    assert_eq!(fx.host_state(&id, A).await.state, "pending");
    fx.set_now(T0 + MIN);
    fx.svc.tick().await.unwrap();
    assert_eq!(fx.svc.offer_of(A), None, "核准前不提議");
    assert!(
        !fx.audit_actions()
            .await
            .contains(&"rollout.start".to_string())
    );
    // 待核准的主機算在未結束的推送裡
    let e = fx
        .svc
        .create_rollout(&who(), RolloutSpec::parse(&body("y")).unwrap())
        .await
        .unwrap_err();
    assert_eq!(e.code, "rollout_overlap");
    // 建立者與送出者都不能核准
    for actor in [who(), other.clone()] {
        let e = fx
            .svc
            .approve_rollout(&actor, id.clone())
            .await
            .unwrap_err();
        assert_eq!(
            (e.status.as_u16(), e.code, e.product_code),
            (403, "self_approval", Some("KRS-E6106")),
            "{}",
            actor.actor
        );
    }
    // 退回：回到草稿、對象刪除、留下原因
    let v = fx
        .svc
        .reject_rollout(&third, id.clone(), "時段不對".into())
        .await
        .unwrap();
    assert_eq!(
        (v["state"].as_str(), v["reject_reason"].as_str()),
        (Some("draft"), Some("時段不對"))
    );
    assert_eq!(v["submitted_by"], Value::Null);
    let rid = id.clone();
    let n = fx
        .svc
        .db
        .read(move |s| Ok(s.rollout_hosts(&rid)?.len()))
        .await
        .unwrap();
    assert_eq!(n, 0);
    let e = fx
        .svc
        .reject_rollout(&third, id.clone(), "x".into())
        .await
        .unwrap_err();
    assert_eq!(e.code, "invalid_state");
    // 重新送出（建立者）：退回的原因清除；修改待核准的推送 → 退回草稿，要重新送審
    let v = fx
        .svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    assert_eq!(
        (v["state"].as_str(), v["reject_reason"].is_null()),
        (Some("pending_approval"), true)
    );
    let v = fx
        .svc
        .update_rollout(&who(), id.clone(), RolloutSpec::parse(&body("x2")).unwrap())
        .await
        .unwrap();
    assert_eq!(v["state"], "draft");
    let e = fx
        .svc
        .approve_rollout(&third, id.clone())
        .await
        .unwrap_err();
    assert_eq!(e.code, "invalid_state", "修改後核准失效");
    fx.svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    // 第三人核准：開始推送，稽核有 approve 與 start
    let v = fx.svc.approve_rollout(&third, id.clone()).await.unwrap();
    assert_eq!(
        (v["state"].as_str(), v["approved_by"].as_str()),
        (Some("running"), Some("user:carol"))
    );
    let e = fx
        .svc
        .approve_rollout(&third, id.clone())
        .await
        .unwrap_err();
    assert_eq!(e.code, "invalid_state", "不能重複核准");
    fx.set_now(T0 + 3 * MIN);
    fx.svc.tick().await.unwrap();
    assert!(fx.svc.offer_of(A).is_some(), "核准後才提議");
    let actions = fx.audit_actions().await;
    for a in [
        "rollout.submit",
        "rollout.reject",
        "rollout.approve",
        "rollout.start",
    ] {
        assert!(actions.contains(&a.to_string()), "{a}：{actions:?}");
    }
    fx.svc
        .cancel_rollout(&who(), id, "結束".into())
        .await
        .unwrap();

    // 有 starts_at：核准後先排程
    let mut later = body("z");
    later["schedule"] = json!({"starts_at": crate::api::http::ts_ms(T0 + 60 * MIN)});
    let created = fx
        .svc
        .create_rollout(&who(), RolloutSpec::parse(&later).unwrap())
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    fx.svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    let v = fx.svc.approve_rollout(&third, id.clone()).await.unwrap();
    assert_eq!(v["state"], "scheduled");
    // 待核准的推送也可以取消
    fx.svc
        .cancel_rollout(&who(), id, "結束".into())
        .await
        .unwrap();
}

/// 沒有啟用雙人覆核時（預設）送出後直接開始，核准／退回為 invalid_state
#[tokio::test]
async fn approval_is_off_by_default() {
    let signer = TestSigner::new(24);
    let fx = Fx::new("noapproval", trust_settings(&signer)).await;
    import(&fx, &release(&signer, "0.9.2", "2222222")).await;
    fx.add_host(A, "0.9.0", "Web", &[], "tar", "ready").await;
    fx.connect(A, "0.9.0", "tar");
    let spec = RolloutSpec::parse(
        &json!({"name": "x", "target_version": "0.9.2", "selector": {"agent_ids": [A]}, "waves": [{"size": "rest"}]}),
    )
    .unwrap();
    let created = fx.svc.create_rollout(&who(), spec).await.unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let v = fx
        .svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    assert_eq!(
        (v["state"].as_str(), v["submitted_by"].as_str()),
        (Some("running"), Some("user:admin"))
    );
    let bob = Actor {
        actor: "user:bob".into(),
        ip: None,
    };
    let e = fx.svc.approve_rollout(&bob, id.clone()).await.unwrap_err();
    assert_eq!(e.code, "invalid_state");
    let e = fx
        .svc
        .reject_rollout(&bob, id, "x".into())
        .await
        .unwrap_err();
    assert_eq!(e.code, "invalid_state");
}

/// 外部 URL 的版本庫（REMOTE.md 第 4.1.2 節，0.10.0）：只登記簽章資料（驗證與代管相同），發佈檔不能上傳；
/// 提議的 artifact 為 `source: url`、`url` = base_url＋檔名，簽章、SHA256SUMS 與雜湊照舊附上；外部 URL 的
/// 檔案不經 8443 提供。回滾檔所屬的版本是外部 URL 時，只在目標也是外部 URL 時附上
#[tokio::test]
async fn external_releases_offer_urls_and_keep_the_signature_chain() {
    let signer = TestSigner::new(27);
    let fx = Fx::new("external", trust_settings(&signer)).await;
    let r091 = release(&signer, "0.9.1", "1111111");
    let r092 = release(&signer, "0.9.2", "2222222");
    let r093 = release(&signer, "0.9.3", "3333333");
    // base_url 的檢查：https、不含查詢字串或帳號密碼
    for bad in [
        "http://repo.example.gov.tw/p/",
        "https://repo.example.gov.tw/p/?token=1",
        "https://user@repo.example.gov.tw/p/",
    ] {
        let e = fx
            .svc
            .import_release_from(
                &who(),
                r091.sha256sums.clone(),
                r091.signature.clone(),
                Some(r091.manifest.clone()),
                Some(bad.into()),
            )
            .await
            .unwrap_err();
        assert_eq!(
            (
                e.status.as_u16(),
                e.details.as_ref().map(|d| d["parameter"].clone())
            ),
            (400, Some(json!("base_url"))),
            "{bad}"
        );
    }
    // 簽章不符的外部版本一樣被拒（驗證不因外部 URL 而省略）
    let e = fx
        .svc
        .import_release_from(
            &who(),
            r091.sha256sums.clone(),
            TestSigner::new(28).sign(r091.sha256sums.as_bytes()),
            Some(r091.manifest.clone()),
            Some("https://repo.example.gov.tw/p/".into()),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "release_signature_invalid");
    // 0.9.1（主機目前的版本）與 0.9.2 是外部 URL 的版本，0.9.3 由 Kairos 代管
    for (r, base) in [
        (&r091, "https://Repo.example.gov.tw/kairos-pulse/0.9.1"),
        (&r092, "https://repo.example.gov.tw/kairos-pulse/0.9.2/"),
    ] {
        let v = fx
            .svc
            .import_release_from(
                &who(),
                r.sha256sums.clone(),
                r.signature.clone(),
                Some(r.manifest.clone()),
                Some(base.into()),
            )
            .await
            .unwrap();
        assert_eq!(v["source"], "external");
        assert_eq!(v["verified"], true);
        assert!(
            v["base_url"]
                .as_str()
                .unwrap()
                .starts_with("https://repo.example.gov.tw/kairos-pulse/0.9."),
            "{v}"
        );
        assert!(v["base_url"].as_str().unwrap().ends_with('/'));
    }
    import(&fx, &r093).await;
    let stored_before = std::fs::read_dir(&fx.svc.releases_dir).unwrap().count();
    // 尚未登記的外部版本沒有可以提議的發佈檔（提議需要大小）
    fx.add_host(A, "0.9.1", "Web", &[], "deb", "ready").await;
    fx.connect(A, "0.9.1", "deb");
    let p = fx
        .svc
        .preview_rollout(
            RolloutSpec::parse(
                &json!({"name": "x", "target_version": "0.9.2", "selector": {"agent_ids": [A]}}),
            )
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(p["skipped"]["no_artifact"], 1, "{p}");
    // 登記發佈檔：雜湊不符被拒；相符時記下大小，檔案不存入 Kairos
    let (name, data) = r092.files[0].clone();
    let mut up = releases::Upload::create(&fx.svc.releases_dir, 1 << 20).unwrap();
    up.write(b"not the file").unwrap();
    let (sha, size) = up.finish().unwrap();
    let e = fx
        .svc
        .store_artifact(&who(), "0.9.2".into(), name, up, sha, size)
        .await
        .unwrap_err();
    assert_eq!(e.code, "artifact_hash_mismatch");
    for r in [&r091, &r092] {
        for (name, data) in &r.files {
            let mut up = releases::Upload::create(&fx.svc.releases_dir, 1 << 20).unwrap();
            up.write(data).unwrap();
            let (sha, size) = up.finish().unwrap();
            let v = fx
                .svc
                .store_artifact(&who(), r.version.clone(), name.clone(), up, sha, size)
                .await
                .unwrap();
            assert_eq!(
                (
                    v["stored"].as_bool(),
                    v["registered"].as_bool(),
                    v["size"].as_u64()
                ),
                (Some(false), Some(true), Some(data.len() as u64)),
                "{v}"
            );
        }
    }
    assert_eq!(
        std::fs::read_dir(&fx.svc.releases_dir).unwrap().count(),
        stored_before,
        "外部版本的發佈檔不存入版本庫"
    );
    let _ = data;

    let rollout = |target: &str| {
        RolloutSpec::parse(&json!({
            "name": target, "target_version": target, "selector": {"agent_ids": [A]},
            "waves": [{"size": "rest"}], "options": {"allow_no_rollback": true},
        }))
        .unwrap()
    };
    // 目標是外部 URL 的版本：可升級（發佈檔不必存入）；回滾檔也在外部 URL，不算「無法自動回滾」
    let p = fx
        .svc
        .preview_rollout(rollout("0.9.2"), None)
        .await
        .unwrap();
    assert_eq!(
        (p["eligible"].as_i64(), p["no_rollback"].as_i64()),
        (Some(1), Some(0)),
        "{p}"
    );
    let created = fx
        .svc
        .create_rollout(&who(), rollout("0.9.2"))
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    fx.svc
        .submit_rollout(&who(), id.clone(), Some("升級 1 台".into()), None)
        .await
        .unwrap();
    fx.set_now(T0 + 61_000);
    fx.svc.tick().await.unwrap();
    let up_a = fx.svc.offer_of(A).expect("offered");
    let offer = fx.svc.build_offer(A, &up_a, 1).await.unwrap().unwrap();
    kairos_proto::mgmt::validate_upgrade_offer(&offer).unwrap();
    assert_eq!(offer.artifact.source, "url");
    assert_eq!(
        offer.artifact.url.as_deref(),
        Some(
            format!(
                "https://repo.example.gov.tw/kairos-pulse/0.9.2/{}",
                offer.artifact.filename
            )
            .as_str()
        )
    );
    assert!(offer.artifact.filename.ends_with(".deb"));
    // 簽章鏈不變：提議附的 SHA256SUMS 與簽章就是匯入時驗證過的那一份，artifact 的雜湊列在其中
    assert_eq!(offer.sha256sums, r092.sha256sums);
    assert_eq!(offer.signature, r092.signature);
    assert!(offer.sha256sums.contains(&offer.artifact.sha256));
    let rb = offer.rollback_artifact.as_ref().expect("回滾檔");
    assert_eq!(rb.source, "url");
    assert!(
        rb.url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://repo.example.gov.tw/kairos-pulse/0.9.1/")),
        "{rb:?}"
    );
    // 外部 URL 的檔案不經 8443 提供
    assert!(
        fx.svc
            .offered_file(A, &up_a, &offer.artifact.filename)
            .await
            .unwrap()
            .is_none()
    );
    fx.svc
        .cancel_rollout(&who(), id, "換目標".into())
        .await
        .unwrap();

    // 目標由 Kairos 代管、主機目前的版本只在外部 URL：不附回滾檔（不讓不允許外部 URL 的主機因此拒絕提議），
    // 資格分類算作「無法自動回滾」
    let p = fx
        .svc
        .preview_rollout(rollout("0.9.3"), None)
        .await
        .unwrap();
    assert_eq!(
        (p["eligible"].as_i64(), p["no_rollback"].as_i64()),
        (Some(1), Some(1)),
        "{p}"
    );
    let created = fx
        .svc
        .create_rollout(&who(), rollout("0.9.3"))
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    fx.svc
        .submit_rollout(
            &who(),
            id.clone(),
            Some("升級 1 台（無法自動回滾 1 台）".into()),
            None,
        )
        .await
        .unwrap();
    fx.set_now(T0 + 70_000);
    fx.svc.tick().await.unwrap();
    let up_a = fx.svc.offer_of(A).expect("offered");
    let offer = fx.svc.build_offer(A, &up_a, 2).await.unwrap().unwrap();
    assert_eq!(
        (
            offer.artifact.source.as_str(),
            offer.artifact.url.as_deref()
        ),
        ("kairos", None)
    );
    assert!(offer.rollback_artifact.is_none());
    assert!(
        fx.svc
            .offered_file(A, &up_a, &offer.artifact.filename)
            .await
            .unwrap()
            .is_some()
    );
}
