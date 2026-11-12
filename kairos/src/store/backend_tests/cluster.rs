//! 叢集協調的儲存層測試（`kairos/HA.md` 第 6.2、23 節）：租約在兩個後端的語意相同；PostgreSQL 上的
//! fencing（舊 leader 的寫入被擋下、接手要等舊 leader 的交易結束）、同時搶租約只有一個成功、schema 從 v9
//! 升級、單機與叢集模式互斥，以及協調執行緒的接手與失去租約。
//! 沒有可用的 PostgreSQL 時印出原因後略過（見 [`super::TestDb::postgres`]）

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::TestDb;
use crate::cluster::store::{ClusterStore, LEASE_LEADER, NodeRegistration};
use crate::cluster::{self, ClusterState, Role};
use crate::config::ClusterSettings;
use crate::store::db::TxMode;
use crate::store::postgres::conn::params;
use crate::store::postgres::pool::{Access, Fence, PgPool, ServiceMode};
use crate::store::postgres::{self, SchemaState};
use crate::store::prelude::*;

const TTL_MS: i64 = 15_000;

/// 同一個 schema 的另一個連線池（代表另一個節點的行程）
fn second_pool(db: &TestDb) -> Arc<PgPool> {
    PgPool::with_data_dir(db.pool().settings().clone(), Some(db.dir.clone()))
}

fn put(conn: &postgres::conn::PgConn, key: &str, value: &str) -> postgres::conn::PgResult<usize> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
}

#[test]
fn lease_semantics_are_the_same_on_both_backends() {
    // 假時鐘（now 參數）下的租約：取得、續約、到期接手、交還；SQLite 與 PostgreSQL 的結果相同
    let mut dbs = vec![TestDb::sqlite("lease")];
    dbs.extend(TestDb::postgres("lease"));
    for db in dbs {
        let s = db.handle();
        let t0 = 1_700_000_000_000_i64;
        let a = s
            .cluster_lease_acquire(LEASE_LEADER, "kairos-a", TTL_MS, Some(t0))
            .unwrap()
            .unwrap();
        assert_eq!(
            (a.holder.as_str(), a.epoch, a.expires_at),
            ("kairos-a", 1, t0 + TTL_MS)
        );
        assert!(
            s.cluster_lease_acquire(LEASE_LEADER, "kairos-b", TTL_MS, Some(t0 + 5_000))
                .unwrap()
                .is_none(),
            "未到期時另一個節點取不到"
        );
        let r = s
            .cluster_lease_renew(LEASE_LEADER, "kairos-a", 1, TTL_MS, Some(t0 + 5_000))
            .unwrap()
            .unwrap();
        assert_eq!(
            (r.epoch, r.expires_at, r.now),
            (1, t0 + 5_000 + TTL_MS, t0 + 5_000)
        );
        // 到期（最後一次續約之後 TTL）：接手，epoch 加一
        let b = s
            .cluster_lease_acquire(LEASE_LEADER, "kairos-b", TTL_MS, Some(t0 + 20_001))
            .unwrap()
            .unwrap();
        assert_eq!((b.holder.as_str(), b.epoch), ("kairos-b", 2));
        // 舊 leader：續約失敗、也不能重新取得
        assert!(
            s.cluster_lease_renew(LEASE_LEADER, "kairos-a", 1, TTL_MS, Some(t0 + 20_002))
                .unwrap()
                .is_none()
        );
        assert!(
            s.cluster_lease_acquire(LEASE_LEADER, "kairos-a", TTL_MS, Some(t0 + 20_003))
                .unwrap()
                .is_none()
        );
        // step-down 後立即可以接手
        let v = s
            .cluster_lease_vacate(LEASE_LEADER, Some(t0 + 21_000))
            .unwrap()
            .unwrap();
        assert_eq!((v.holder.as_str(), v.epoch), ("kairos-b", 2));
        let lease = s
            .cluster_lease(LEASE_LEADER, Some(t0 + 21_000))
            .unwrap()
            .unwrap();
        assert!(!lease.is_live());
        let a2 = s
            .cluster_lease_acquire(LEASE_LEADER, "kairos-a", TTL_MS, Some(t0 + 21_001))
            .unwrap()
            .unwrap();
        assert_eq!(a2.epoch, 3);
        // 節點：註冊、心跳、失聯
        let reg = NodeRegistration {
            node_id: "kairos-a",
            hostname: "host-a",
            version: "kairos test",
            schema_version: 10,
            config_hash: "c",
            keys_hash: "k",
            advertise: Some("10.0.0.1"),
            info: Some("{}"),
        };
        let started = s.cluster_node_register(&reg, Some(t0)).unwrap();
        assert_eq!(started, t0);
        assert_eq!(
            s.cluster_node_heartbeat("kairos-a", started, "standby", "k", None, Some(t0 + 1))
                .unwrap()
                .as_deref(),
            Some("standby")
        );
        assert_eq!(
            s.cluster_mark_lost(20_000, "kairos-b", Some(t0 + 30_000))
                .unwrap(),
            vec!["kairos-a".to_string()]
        );
        let n = s
            .cluster_node("kairos-a", Some(t0 + 30_000))
            .unwrap()
            .unwrap();
        assert_eq!((n.role.as_str(), n.heartbeat_age_ms), ("lost", 29_999));
        assert_eq!(n.advertise.as_deref(), Some("10.0.0.1"));
    }
}

#[test]
fn fencing_rejects_every_write_of_a_deposed_leader() {
    let Some(db) = TestDb::postgres("fence") else {
        return;
    };
    let t0 = 1_700_000_000_000_i64;
    let pool_a = db.pool();
    let pool_b = second_pool(&db);
    let a = postgres::open_admin(&pool_a, Duration::from_secs(5)).unwrap();
    let b = postgres::open_admin(&pool_b, Duration::from_secs(5)).unwrap();
    let lost_calls = Arc::new(AtomicUsize::new(0));
    {
        let lost_calls = lost_calls.clone();
        pool_a.on_fence_lost(move || {
            lost_calls.fetch_add(1, Ordering::SeqCst);
        });
    }

    // A 取得租約（epoch 1）並開始 fencing
    let lease = a
        .cluster_lease_acquire(LEASE_LEADER, "kairos-a", TTL_MS, Some(t0))
        .unwrap()
        .unwrap();
    pool_a.set_fence(Some(Fence {
        node: "kairos-a".into(),
        epoch: lease.epoch,
        now: Some(t0 + 1_000),
    }));
    // 租約有效時三種寫入都成功：序列化的交易、一般的交易、自動提交的單一敘述
    {
        let tx = a.begin_tx(TxMode::Immediate).unwrap();
        put(&tx, "fence_immediate", "1").unwrap();
        tx.commit().unwrap();
        let tx = a.begin_tx(TxMode::Deferred).unwrap();
        put(&tx, "fence_deferred", "1").unwrap();
        tx.commit().unwrap();
        assert_eq!(put(&a, "fence_auto", "1").unwrap(), 1);
        // 領域方法（自動提交）與稽核（自己開交易）也照常
        a.put_meta("fence_domain", "1").unwrap();
        a.audit(
            t0,
            &crate::audit::Audit {
                actor: "system",
                category: "system",
                action: "test.fence",
                result: "ok",
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert!(!pool_a.fence_lost());

    // 租約到期、B 接手（epoch 2）
    let taken = b
        .cluster_lease_acquire(LEASE_LEADER, "kairos-b", TTL_MS, Some(t0 + TTL_MS + 1))
        .unwrap()
        .unwrap();
    assert_eq!(taken.epoch, 2);
    pool_b.set_fence(Some(Fence {
        node: "kairos-b".into(),
        epoch: 2,
        now: Some(t0 + TTL_MS + 2),
    }));
    // 舊 leader 恢復（仍以 epoch 1 寫入，時間也已經過了）：所有寫入都被擋下，資料不變
    pool_a.set_fence(Some(Fence {
        node: "kairos-a".into(),
        epoch: 1,
        now: Some(t0 + TTL_MS + 2),
    }));
    let e = a
        .begin_tx(TxMode::Immediate)
        .err()
        .expect("fencing 應擋下序列化的交易");
    assert!(e.to_string().contains(cluster::CODE_LEASE_LOST), "{e}");
    let e = a
        .begin_tx(TxMode::Deferred)
        .err()
        .expect("fencing 應擋下一般的交易");
    assert!(e.to_string().contains(cluster::CODE_LEASE_LOST), "{e}");
    let e = put(&a, "fence_auto", "stale").unwrap_err();
    assert!(e.to_string().contains(cluster::CODE_LEASE_LOST), "{e}");
    assert!(a.put_meta("fence_domain", "stale").is_err());
    assert!(
        a.audit(
            t0,
            &crate::audit::Audit {
                actor: "system",
                category: "system",
                action: "test.fence.stale",
                result: "ok",
                ..Default::default()
            },
        )
        .is_err()
    );
    // 擋下之後連線仍可使用（交易已回復），讀取不受影響
    assert!(!a.is_broken());
    assert!(!a.in_transaction());
    assert_eq!(a.get_meta("fence_auto").unwrap().as_deref(), Some("1"));
    assert_eq!(a.get_meta("fence_domain").unwrap().as_deref(), Some("1"));
    let ro = pool_a
        .get(Access::ReadOnly, Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        ro.get_meta("fence_immediate").unwrap().as_deref(),
        Some("1")
    );
    let snapshot = a.begin_snapshot().unwrap();
    assert!(snapshot.get_meta("fence_deferred").unwrap().is_some());
    drop(snapshot);
    // 「失去租約」只通知一次
    assert!(pool_a.fence_lost());
    assert_eq!(lost_calls.load(Ordering::SeqCst), 1);
    let audits: i64 = b
        .query_row(
            "SELECT count(*) FROM audit_events WHERE action LIKE 'test.fence%'",
            (),
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audits, 1, "舊 leader 的稽核紀錄沒有寫入");

    // 新 leader 的寫入成功
    let tx = b.begin_tx(TxMode::Immediate).unwrap();
    put(&tx, "fence_auto", "b").unwrap();
    tx.commit().unwrap();
    assert_eq!(put(&b, "fence_domain", "b").unwrap(), 1);
    assert_eq!(b.get_meta("fence_auto").unwrap().as_deref(), Some("b"));

    // 沒有人接手、但租約已到期：同樣拒絕寫入（leader 必須先續約）
    pool_b.set_fence(Some(Fence {
        node: "kairos-b".into(),
        epoch: 2,
        now: Some(t0 + 10 * TTL_MS),
    }));
    assert!(put(&b, "fence_auto", "expired").is_err());
    assert_eq!(b.get_meta("fence_auto").unwrap().as_deref(), Some("b"));
    // 沒有 fencing 的連線池（CLI、單機）不受影響
    pool_b.set_fence(None);
    assert_eq!(put(&b, "fence_auto", "cli").unwrap(), 1);
}

#[test]
fn takeover_waits_for_the_old_leaders_open_transaction() {
    // HA.md 第 6.2 節：FOR SHARE 讓接手的 UPDATE 等到舊 leader 的交易結束——舊 leader 的交易要嘛在接手前
    // commit，要嘛在接手後失敗，不會重疊。以真實的資料庫時間與很短的 TTL 驗證
    let Some(db) = TestDb::postgres("fencewait") else {
        return;
    };
    let pool_a = db.pool();
    let pool_b = second_pool(&db);
    let a = postgres::open_admin(&pool_a, Duration::from_secs(5)).unwrap();
    let ttl_ms = 800;
    let lease = a
        .cluster_lease_acquire(LEASE_LEADER, "kairos-a", ttl_ms, None)
        .unwrap()
        .unwrap();
    pool_a.set_fence(Some(Fence {
        node: "kairos-a".into(),
        epoch: lease.epoch,
        now: None,
    }));
    // 舊 leader 開始一個交易（通過 fencing，持有租約列的 FOR SHARE），然後「停住」到租約到期之後
    let tx = a.begin_tx(TxMode::Immediate).unwrap();
    put(&tx, "inflight", "from-old-leader").unwrap();
    std::thread::sleep(Duration::from_millis(1_000));
    // 新 leader 嘗試接手：租約已到期，但要等舊 leader 的交易結束
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let waiter = {
        let done = done.clone();
        std::thread::spawn(move || {
            let b = postgres::open_admin(&pool_b, Duration::from_secs(5)).unwrap();
            let got = b
                .cluster_lease_acquire(LEASE_LEADER, "kairos-b", 15_000, None)
                .unwrap();
            done.store(true, Ordering::SeqCst);
            got
        })
    };
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !done.load(Ordering::SeqCst),
        "舊 leader 的交易還開著時，接手必須等待"
    );
    // 舊 leader 的交易提交：在接手之前，資料保留
    tx.commit().unwrap();
    let got = waiter.join().unwrap().expect("交易結束後接手成功");
    assert_eq!((got.holder.as_str(), got.epoch), ("kairos-b", 2));
    assert_eq!(
        a.get_meta("inflight").unwrap().as_deref(),
        Some("from-old-leader")
    );
    // 接手之後舊 leader 的下一個交易失敗
    assert!(a.begin_tx(TxMode::Immediate).is_err());
    assert!(put(&a, "inflight", "after-takeover").is_err());
    assert_eq!(
        a.get_meta("inflight").unwrap().as_deref(),
        Some("from-old-leader")
    );
}

#[test]
fn concurrent_acquisition_yields_exactly_one_leader() {
    // 同時多個節點搶一個空的（或剛到期的）租約：每一輪恰好一個成功，epoch 每輪加一
    let Some(db) = TestDb::postgres("race") else {
        return;
    };
    drop(db.pg());
    const NODES: usize = 6;
    let pools: Vec<Arc<PgPool>> = (0..NODES).map(|_| second_pool(&db)).collect();
    for round in 1..=5_i64 {
        let barrier = Arc::new(std::sync::Barrier::new(NODES));
        let handles: Vec<_> = pools
            .iter()
            .enumerate()
            .map(|(i, pool)| {
                let pool = pool.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let conn = pool.get(Access::ReadWrite, Duration::from_secs(5)).unwrap();
                    barrier.wait();
                    conn.cluster_lease_acquire(LEASE_LEADER, &format!("node-{i}"), 60_000, None)
                        .unwrap()
                })
            })
            .collect();
        let winners: Vec<_> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(winners.len(), 1, "第 {round} 輪：{winners:?}");
        assert_eq!(winners[0].epoch, round);
        let conn = db.pg();
        let lease = conn.cluster_lease(LEASE_LEADER, None).unwrap().unwrap();
        assert_eq!(lease.holder, winners[0].holder);
        // 讓租約到期，下一輪重新搶
        assert!(
            conn.cluster_lease_vacate(LEASE_LEADER, None)
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn sqlite_v9_is_upgraded_to_v11_and_keeps_data() {
    // 0.9 的 SQLite（v9）升級到 v11：v10 只新增叢集的資料表與三個欄位、v11 只新增 rollouts 的三個欄位，
    // 既有資料不變
    use crate::store::sqlite::{MIGRATIONS, migrate};
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    let v9: Vec<_> = MIGRATIONS
        .iter()
        .copied()
        .filter(|m| m.version <= 9)
        .collect();
    assert_eq!(migrate(&mut conn, &v9).unwrap(), 9);
    conn.execute(
        "INSERT INTO hosts (agent_id, hostname, first_seen, last_seen) VALUES ('a-1', 'h', 1, 2)",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO meta (key, value) VALUES ('keep', 'me')", [])
        .unwrap();
    assert_eq!(migrate(&mut conn, MIGRATIONS).unwrap(), 11);
    conn.execute(
        "UPDATE rollouts SET submitted_at = 1, submitted_by = 'u', reject_reason = 'r' WHERE 0",
        [],
    )
    .unwrap();
    let tables: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    // 兩個後端的資料表相同（kairos db migrate 依這份清單一對一複製）
    let mut expected: Vec<String> = postgres::schema::TABLES
        .iter()
        .map(|t| (*t).to_string())
        .collect();
    expected.sort();
    assert_eq!(tables, expected);
    let hosts: i64 = conn
        .query_row("SELECT count(*) FROM hosts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(hosts, 1);
    assert_eq!(conn.get_meta("keep").unwrap().as_deref(), Some("me"));
    conn.execute("UPDATE alert_deliveries SET claim_epoch = 1 WHERE 0", [])
        .unwrap();
    conn.execute(
        "UPDATE enroll_requests SET issuing_node = 'n', issuing_at = 1 WHERE 0",
        [],
    )
    .unwrap();
    // 單機的 SQLite 上叢集資料表保持空白
    assert!(conn.cluster_nodes(None).unwrap().is_empty());
    assert!(conn.cluster_lease(LEASE_LEADER, None).unwrap().is_none());
}

#[test]
fn schema_v9_is_upgraded_in_place() {
    // 0.9 的 PostgreSQL schema（v9）由第一個以新版啟動的節點升級；既有資料不變，再執行一次沒有變化
    let Some(db) = TestDb::postgres("upgrade") else {
        return;
    };
    let pool = db.pool();
    let conn = pool.get(Access::ReadWrite, Duration::from_secs(5)).unwrap();
    {
        let tx = conn.begin_tx(TxMode::Deferred).unwrap();
        tx.execute_batch(&format!("CREATE SCHEMA IF NOT EXISTS {}", pool.schema()))
            .unwrap();
        tx.execute_batch(postgres::schema::SCHEMA_V9).unwrap();
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('application', 'KRS1'), ('schema_version', '9'),
             ('keep', 'me')",
            (),
        )
        .unwrap();
        tx.execute(
            "INSERT INTO hosts (agent_id, hostname, first_seen, last_seen) VALUES ('a-1', 'h', 1, 2)",
            (),
        )
        .unwrap();
        tx.commit().unwrap();
    }
    conn.clear_statements();
    assert_eq!(
        postgres::schema_state(&conn).unwrap(),
        SchemaState::Version(9)
    );
    // 同時啟動的節點：只升級一次
    let handles: Vec<_> = (0..3)
        .map(|_| {
            let pool = pool.clone();
            std::thread::spawn(move || {
                let c = pool.get(Access::ReadWrite, Duration::from_secs(5)).unwrap();
                postgres::ensure_schema(&c).unwrap()
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), crate::store::sqlite::SCHEMA_VERSION);
    }
    assert_eq!(
        postgres::schema_state(&conn).unwrap(),
        SchemaState::Version(crate::store::sqlite::SCHEMA_VERSION)
    );
    assert_eq!(conn.get_meta("keep").unwrap().as_deref(), Some("me"));
    let hosts: i64 = conn
        .query_row("SELECT count(*) FROM hosts", (), |r| r.get(0))
        .unwrap();
    assert_eq!(hosts, 1);
    // 升級後的資料表與「直接建立」的相同
    let tables: Vec<String> = conn
        .prepare("SELECT tablename::text FROM pg_tables WHERE schemaname = ?1 ORDER BY 1")
        .unwrap()
        .query_map([pool.schema()], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut expected: Vec<String> = postgres::schema::TABLES
        .iter()
        .map(|t| (*t).to_string())
        .collect();
    expected.sort();
    assert_eq!(tables, expected);
    // 新欄位可以使用
    conn.execute(
        "UPDATE alert_deliveries SET claim_epoch = 1 WHERE false",
        (),
    )
    .unwrap();
    conn.execute(
        "UPDATE enroll_requests SET issuing_node = 'n', issuing_at = 1 WHERE false",
        (),
    )
    .unwrap();
    // v11：rollouts 的雙人覆核欄位
    conn.execute(
        "UPDATE rollouts SET submitted_at = 1, submitted_by = 'u', reject_reason = 'r' WHERE false",
        (),
    )
    .unwrap();
    assert!(
        conn.cluster_lease_acquire(LEASE_LEADER, "kairos-a", TTL_MS, None)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        postgres::ensure_schema(&conn).unwrap(),
        crate::store::sqlite::SCHEMA_VERSION
    );
}

#[test]
fn schema_v10_is_upgraded_to_v11_and_keeps_rollouts() {
    // 只到 v10 的 PostgreSQL schema（0.10.0 開發期間、叢集協調先建立的資料庫）升級到 v11：只套用
    // rollouts 的三個欄位，既有的推送保留、新欄位為 NULL，叢集的資料表不重建
    use crate::remote::store::RemoteStore;
    let Some(db) = TestDb::postgres("upgrade11") else {
        return;
    };
    assert_eq!(
        postgres::schema::UPGRADES
            .iter()
            .map(|(v, _)| *v)
            .collect::<Vec<_>>(),
        [10, 11]
    );
    assert_eq!(crate::store::sqlite::SCHEMA_VERSION, 11);
    let pool = db.pool();
    let conn = pool.get(Access::ReadWrite, Duration::from_secs(5)).unwrap();
    {
        let tx = conn.begin_tx(TxMode::Deferred).unwrap();
        tx.execute_batch(&format!("CREATE SCHEMA IF NOT EXISTS {}", pool.schema()))
            .unwrap();
        tx.execute_batch(postgres::schema::SCHEMA_V9).unwrap();
        tx.execute_batch(postgres::schema::SCHEMA_V10).unwrap();
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('application', 'KRS1'), ('schema_version', '10')",
            (),
        )
        .unwrap();
        tx.execute(
            "INSERT INTO rollouts (rollout_id, name, release_id, target_version, selector, waves,
                schedule, options, state, created_at, created_by, updated_at)
             VALUES ('r-20261020-0001', 'x', 1, '0.9.1', '{}', '[]', '{}', '{}', 'completed', 1,
                'user:a', 2)",
            (),
        )
        .unwrap();
        tx.execute(
            "INSERT INTO cluster_revs (name, rev) VALUES ('keep', 7)",
            (),
        )
        .unwrap();
        tx.commit().unwrap();
    }
    conn.clear_statements();
    assert_eq!(
        postgres::schema_state(&conn).unwrap(),
        SchemaState::Version(10)
    );
    assert_eq!(postgres::ensure_schema(&conn).unwrap(), 11);
    assert_eq!(
        postgres::schema_state(&conn).unwrap(),
        SchemaState::Version(11)
    );
    let rev: i64 = conn
        .query_row(
            "SELECT rev FROM cluster_revs WHERE name = 'keep'",
            (),
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rev, 7);
    let r = conn.get_rollout("r-20261020-0001").unwrap().unwrap();
    assert_eq!(
        (r.state.as_str(), r.created_by.as_str(), r.updated_at),
        ("completed", "user:a", 2)
    );
    assert_eq!(
        (
            r.submitted_at,
            r.submitted_by.clone(),
            r.approved_at,
            r.reject_reason.clone()
        ),
        (None, None, None, None)
    );
    let mut r2 = r.clone();
    r2.submitted_at = Some(5);
    r2.submitted_by = Some("user:b".into());
    r2.approved_at = Some(6);
    r2.approved_by = Some("user:c".into());
    r2.reject_reason = Some("原因".into());
    conn.update_rollout_state(&r2).unwrap();
    assert_eq!(conn.get_rollout("r-20261020-0001").unwrap().unwrap(), r2);
    // 再執行一次：沒有變化
    assert_eq!(postgres::ensure_schema(&conn).unwrap(), 11);
}

#[test]
fn solo_and_cluster_services_cannot_share_a_schema() {
    // 啟動保護：單機模式的 Kairos 同時只能有一個，也不能與叢集的節點並存；叢集的節點彼此可以並存
    let Some(db) = TestDb::postgres("mode") else {
        return;
    };
    drop(db.pg());
    let noon = super::report::noon;
    let solo_a = second_pool(&db);
    solo_a.set_service_mode(Some(ServiceMode::Solo));
    let (store_a, _) = postgres::report::PgStore::open(solo_a.clone(), db.opts(), noon).unwrap();
    // 第二個單機模式的 Kairos：拒絕啟動（KRS-E4402）
    let solo_b = second_pool(&db);
    solo_b.set_service_mode(Some(ServiceMode::Solo));
    let e = postgres::report::PgStore::open(solo_b.clone(), db.opts(), noon)
        .expect_err("第二個單機模式的 Kairos 應被拒絕");
    assert_eq!(e.kind(), std::io::ErrorKind::ResourceBusy, "{e}");
    assert!(
        e.to_string().contains(postgres::pool::CODE_PG_REQUIREMENTS)
            && e.to_string().contains("已有另一個 Kairos"),
        "{e}"
    );
    // 叢集的節點也不能加入正被單機模式使用的 schema
    let cluster_a = second_pool(&db);
    cluster_a.set_service_mode(Some(ServiceMode::Cluster));
    let conn_a = cluster_a.dedicated(Duration::from_secs(1)).unwrap();
    let e = cluster_a.claim_service_mode(&conn_a).unwrap_err();
    assert!(e.to_string().contains("沒有啟用叢集模式的 Kairos"), "{e}");
    // CLI 與測試（沒有服務模式）不受影響
    assert!(db.pool().claim_service_mode(&db.pg()).is_ok());
    // 第一個結束之後（連線關閉，鎖自動釋放；伺服器處理連線結束需要一點時間）：可以啟動
    drop(store_a);
    wait_until("單機模式的鎖釋放", Duration::from_secs(5), || {
        cluster_a.claim_service_mode(&conn_a).is_ok()
    });
    // 兩個叢集節點並存
    let cluster_b = second_pool(&db);
    cluster_b.set_service_mode(Some(ServiceMode::Cluster));
    let conn_b = cluster_b.dedicated(Duration::from_secs(1)).unwrap();
    cluster_b.claim_service_mode(&conn_b).unwrap();
    // 有叢集節點在的時候，單機模式的 Kairos 拒絕啟動
    let e = postgres::report::PgStore::open(solo_b.clone(), db.opts(), noon)
        .expect_err("叢集使用中的 schema 不能以單機模式啟動");
    assert!(
        e.to_string().contains(postgres::pool::CODE_PG_REQUIREMENTS),
        "{e}"
    );
    drop(conn_a);
    drop(conn_b);
    let mut opened = None;
    wait_until("叢集節點的鎖釋放", Duration::from_secs(5), || {
        opened = postgres::report::PgStore::open(solo_b.clone(), db.opts(), noon).ok();
        opened.is_some()
    });
}

/// 協調執行緒測試用的節點
struct Node {
    state: Arc<ClusterState>,
    handle: cluster::runtime::ClusterHandle,
    lost: Arc<std::sync::Mutex<Vec<String>>>,
    pool: Arc<PgPool>,
}

fn start_node(db: &TestDb, id: &str, settings: &ClusterSettings) -> Node {
    let pool = second_pool(db);
    let state = Arc::new(ClusterState::new(
        id.to_string(),
        settings.clone(),
        "confhash".to_string(),
    ));
    let lost = Arc::new(std::sync::Mutex::new(Vec::new()));
    let on_lost: cluster::runtime::LostAction = {
        let lost = lost.clone();
        Arc::new(move |reason: String| lost.lock().unwrap().push(reason))
    };
    let handle = cluster::runtime::start(cluster::runtime::StartParams {
        state: state.clone(),
        pool: pool.clone(),
        data_dir: db.dir.clone(),
        hostname: format!("host-{id}"),
        shutdown: Arc::new(crate::shutdown::Shutdown::new()),
        on_lost,
    })
    .unwrap();
    Node {
        state,
        handle,
        lost,
        pool,
    }
}

fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) -> Duration {
    let started = Instant::now();
    while !f() {
        assert!(started.elapsed() < timeout, "等待逾時：{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
    started.elapsed()
}

#[test]
fn coordinator_elects_one_leader_hands_over_and_detects_loss() {
    let Some(db) = TestDb::postgres("coord") else {
        return;
    };
    // 短的租約：TTL 6 秒（續約間隔 2 秒、3 秒沒有續約成功就自我降級）、心跳 1 秒
    let settings = ClusterSettings {
        enabled: true,
        lease_ttl_secs: 6,
        heartbeat_secs: 1,
        node_lost_secs: 3,
        ..ClusterSettings::default()
    };
    let admin = db.pg();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // 還沒有 cluster init：節點註冊了，但金鑰檢查不通過，不參與選舉
    let mut a = start_node(&db, "kairos-a", &settings);
    wait_until("節點 a 註冊", Duration::from_secs(10), || {
        admin.cluster_node("kairos-a", None).unwrap().is_some()
    });
    wait_until("金鑰檢查完成", Duration::from_secs(10), || {
        !a.state.key_problems().is_empty()
    });
    assert!(!a.state.keys_ok());
    assert_eq!(a.state.keys_word(), "mismatch");
    assert!(admin.cluster_lease(LEASE_LEADER, None).unwrap().is_none());
    assert!(a.state.key_problems()[0].contains("kairos cluster init"));

    // cluster init（建立金鑰、記錄指紋）之後：a 在下一次金鑰檢查通過並取得租約
    cluster::keys::create_missing(&db.dir).unwrap();
    let local = cluster::keys::LocalKeys::read(&db.dir);
    admin
        .put_meta(cluster::META_CLUSTER_KEYS, &local.to_meta())
        .unwrap();
    admin
        .put_meta(
            cluster::META_CLUSTER_ID,
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
    let grant = rt
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(15), a.handle.wait_leader()).await
        })
        .expect("a 應在金鑰一致後取得租約")
        .unwrap()
        .unwrap();
    assert_eq!((grant.epoch, grant.previous.clone()), (1, None));
    assert!(a.state.is_leader());
    assert_eq!(
        a.state.cluster_id().as_deref(),
        Some("11111111-2222-4333-8444-555555555555")
    );
    // 取得租約的同時 fencing 已生效：a 的池可以寫，epoch 正確
    let conn_a = postgres::open_write(&a.pool, Duration::from_secs(5)).unwrap();
    put(&conn_a, "coord", "a").unwrap();

    // b 啟動：是備援，a 持續續約期間（超過一個 TTL）取不到租約
    let mut b = start_node(&db, "kairos-b", &settings);
    wait_until("節點 b 成為備援", Duration::from_secs(10), || {
        b.state.role() == Role::Standby
    });
    std::thread::sleep(Duration::from_millis(7_500));
    assert!(a.state.is_leader() && !b.state.is_leader());
    assert!(a.lost.lock().unwrap().is_empty());
    assert!(a.state.renew_ok.load(Ordering::Relaxed) >= 3);
    let nodes = admin.cluster_nodes(None).unwrap();
    assert_eq!(
        nodes
            .iter()
            .map(|n| (n.node_id.as_str(), n.role.as_str()))
            .collect::<Vec<_>>(),
        vec![("kairos-a", "leader"), ("kairos-b", "standby")]
    );
    assert_eq!(nodes[0].keys_hash, nodes[1].keys_hash);

    // a 正常停止：交還租約，b 不必等 TTL 就接手（NOTIFY 或 2 秒內的輪詢）
    let stopping = Instant::now();
    rt.block_on(a.handle.stop());
    let grant_b = rt
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(10), b.handle.wait_leader()).await
        })
        .expect("b 應接手")
        .unwrap()
        .unwrap();
    assert_eq!(
        (grant_b.epoch, grant_b.previous.as_deref()),
        (2, Some("kairos-a"))
    );
    assert!(
        stopping.elapsed() < Duration::from_secs(4),
        "交還之後的接手不應等到租約到期：{:?}",
        stopping.elapsed()
    );
    assert!(a.lost.lock().unwrap().is_empty(), "正常停止不算失去租約");
    assert_eq!(
        admin.cluster_node("kairos-a", None).unwrap().unwrap().role,
        "stopped"
    );
    // 舊 leader 的池仍帶著 epoch 1 的 fencing：之後的寫入被擋下
    assert!(put(&conn_a, "coord", "stale").is_err());
    assert_eq!(admin.get_meta("coord").unwrap().as_deref(), Some("a"));

    // b 的租約被 step-down：b 在下一次續約發現並回報「失去租約」（正式程式在這裡以代碼 75 結束）
    assert!(
        admin
            .cluster_lease_vacate(LEASE_LEADER, None)
            .unwrap()
            .is_some()
    );
    let took = wait_until("b 發現失去租約", Duration::from_secs(8), || {
        !b.lost.lock().unwrap().is_empty()
    });
    assert!(took < Duration::from_secs(4), "{took:?}");
    assert!(b.state.lease_lost() && !b.state.is_leader());
    assert_eq!(b.state.check_word(), "lost");
    rt.block_on(b.handle.stop());
}

#[test]
fn leader_demotes_itself_when_the_database_stops_answering() {
    // 資料庫沒有回應（這裡以「佔住租約列」讓續約一直等不到鎖來模擬）：超過 TTL − 5 秒（短 TTL 時為 TTL 的
    // 一半）沒有續約成功，leader 自我降級——早於租約在資料庫中到期，別的節點那時才可能接手
    let Some(db) = TestDb::postgres("demote") else {
        return;
    };
    let settings = ClusterSettings {
        enabled: true,
        lease_ttl_secs: 6,
        heartbeat_secs: 1,
        node_lost_secs: 3,
        ..ClusterSettings::default()
    };
    let admin = db.pg();
    cluster::keys::create_missing(&db.dir).unwrap();
    admin
        .put_meta(
            cluster::META_CLUSTER_KEYS,
            &cluster::keys::LocalKeys::read(&db.dir).to_meta(),
        )
        .unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut a = start_node(&db, "kairos-a", &settings);
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), a.handle.wait_leader()).await
    })
    .expect("a 應取得租約")
    .unwrap()
    .unwrap();
    // 另一條連線鎖住租約列不放：a 的續約等不到鎖（lock_timeout）而失敗
    let blocker = db.pg();
    let tx = blocker.begin_tx(TxMode::Deferred).unwrap();
    tx.execute(
        "SELECT 1 FROM cluster_leases WHERE name = 'leader' FOR UPDATE",
        (),
    )
    .unwrap();
    let blocked_at = Instant::now();
    let took = wait_until("a 自我降級", Duration::from_secs(12), || {
        !a.lost.lock().unwrap().is_empty()
    });
    // 自我降級的時限是 3 秒（TTL 6 秒的一半），從最後一次成功的續約起算（最多在鎖住前一個續約間隔 2 秒）
    assert!(took < Duration::from_millis(4_500), "{took:?}");
    assert!(
        a.lost.lock().unwrap()[0].contains("沒有續約成功"),
        "{:?}",
        a.lost.lock().unwrap()
    );
    // 這時租約在資料庫中還沒到期（或剛到期）：自我降級不晚於租約到期
    let lease = admin.cluster_lease(LEASE_LEADER, None).unwrap().unwrap();
    assert_eq!(lease.holder, "kairos-a");
    assert!(
        blocked_at.elapsed() < Duration::from_secs(6) || !lease.is_live(),
        "自我降級應早於租約到期"
    );
    drop(tx);
    rt.block_on(a.handle.stop());
}
