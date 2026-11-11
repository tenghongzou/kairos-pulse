//! PostgreSQL 專屬的測試：連線池、唯讀連線、服務鎖、schema 的檢查、並行的稽核寫入與 token 扣次數。
//! 沒有可用的資料庫時印出原因後略過（見 [`super::TestDb::postgres`]）

use std::io::ErrorKind;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::TestDb;
use super::report::{A, legacy, noon};
use crate::audit::Audit;
use crate::storage::WriteStatus;
use crate::store::db::{StoreErrorKind, TxMode};
use crate::store::postgres::conn::{LockKind, array, params};
use crate::store::postgres::pool::{Access, PgPool};
use crate::store::postgres::{self, SchemaState};
use crate::store::prelude::*;

#[test]
fn pool_hands_out_and_takes_back_connections() {
    let Some(db) = TestDb::postgres("pool") else {
        return;
    };
    let pool = db.pool();
    let busy = Duration::from_secs(1);
    {
        let _a = pool.get(Access::ReadWrite, busy).unwrap();
        let _b = pool.get(Access::ReadOnly, busy).unwrap();
        let s = pool.stats();
        assert_eq!((s.busy, s.size), (2, 4));
    }
    let s = pool.stats();
    assert_eq!((s.busy, s.idle), (0, 2), "{s:?}");
    // 池滿：等待 connect_timeout 後回 ResourceBusy；歸還後可以再取
    let small = PgPool::with_data_dir(
        crate::config::PostgresSettings {
            pool_size: 2,
            connect_timeout_secs: 1,
            ..pool.settings().clone()
        },
        None,
    );
    let a = small.get(Access::ReadWrite, busy).unwrap();
    let b = small.get(Access::ReadWrite, busy).unwrap();
    let started = Instant::now();
    let e = small.get(Access::ReadWrite, busy).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ResourceBusy, "{e}");
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert_eq!(small.stats().exhausted, 1);
    drop(a);
    let c = small.get(Access::ReadOnly, busy).unwrap();
    // 等待中的取用在歸還時被喚醒
    let waiter = {
        let small = small.clone();
        std::thread::spawn(move || {
            small
                .get(Access::ReadWrite, Duration::from_secs(1))
                .map(|_| ())
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    drop(b);
    waiter.join().unwrap().unwrap();
    drop(c);
    // 專用連線不佔池的額度
    let _d1 = small.dedicated(busy).unwrap();
    let _d2 = small.dedicated(busy).unwrap();
    let _d3 = small.dedicated(busy).unwrap();
    assert_eq!(small.stats().busy, 0);
    small.close_idle();
    assert_eq!(small.stats().idle, 0);
}

#[test]
fn connection_errors_are_reported_without_secrets() {
    let Some(db) = TestDb::postgres("badconn") else {
        return;
    };
    let good = db.pool().settings().clone();
    // 密碼錯誤：訊息不含密碼
    let file = db.dir.join("wrong.password");
    std::fs::write(&file, "definitely-wrong-PASSWORD\n").unwrap();
    let bad = PgPool::new(crate::config::PostgresSettings {
        password_file: file.display().to_string(),
        ..good.clone()
    });
    if let Err(e) = bad.get(Access::ReadOnly, Duration::from_secs(1)) {
        // trust 驗證的資料庫不檢查密碼；會檢查時訊息不可含密碼
        assert!(!e.to_string().contains("definitely-wrong-PASSWORD"), "{e}");
        assert_eq!(bad.stats().connect_errors, 1);
    }
    // 連不上
    let unreachable = PgPool::new(crate::config::PostgresSettings {
        port: 1,
        connect_timeout_secs: 1,
        ..good.clone()
    });
    let e = unreachable
        .get(Access::ReadOnly, Duration::from_secs(1))
        .unwrap_err();
    assert!(e.to_string().contains("無法連線到 PostgreSQL"), "{e}");
    let e = postgres::check_server(&unreachable).unwrap_err();
    assert!(
        e.to_string().contains(postgres::pool::CODE_PG_UNREACHABLE),
        "{e}"
    );
    // 可用的伺服器：通過要求的檢查
    let (info, _warnings) = postgres::check_server(&db.pool()).unwrap();
    assert!(info.version_num >= postgres::pool::MIN_SERVER_VERSION_NUM);
    assert_eq!(info.encoding, "UTF8");
    assert!(info.clock_skew_ms.abs() < 60_000);
    assert_eq!(db.pool().last_server(), Some(info));
}

#[test]
fn read_only_connections_cannot_write_and_switch_back() {
    let Some(db) = TestDb::postgres("readonly") else {
        return;
    };
    drop(db.handle());
    let pool = db.pool();
    for _ in 0..3 {
        let r = pool.get(Access::ReadOnly, Duration::from_secs(1)).unwrap();
        let e = r.put_meta("x", "1").unwrap_err();
        assert!(e.to_string().contains("read-only"), "{e}");
        assert_eq!(r.get_meta("x").unwrap(), None);
        drop(r);
        // 同一條連線歸還後改作讀寫
        let w = pool.get(Access::ReadWrite, Duration::from_secs(1)).unwrap();
        w.put_meta("y", "1").unwrap();
        assert!(postgres::probe_writable(&w).is_ok());
    }
    assert!(pool.stats().idle <= 2);
}

#[test]
fn values_round_trip_through_the_driver() {
    let Some(db) = TestDb::postgres("values") else {
        return;
    };
    let c = db.pg();
    c.execute_batch(
        "CREATE TABLE t (i BIGINT, s TEXT COLLATE \"C\", b BYTEA, f DOUBLE PRECISION, n BIGINT)",
    )
    .unwrap();
    c.execute(
        "INSERT INTO t VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            i64::MIN,
            "中文 'quote' ?1",
            vec![0_u8, 255, 1],
            1.5_f64,
            None::<i64>
        ],
    )
    .unwrap();
    c.execute(
        "INSERT INTO t VALUES (?, ?, ?, ?, ?)",
        params![true, "", Vec::<u8>::new(), 2, 7_u32],
    )
    .unwrap();
    let rows = c
        .prepare("SELECT i, s, b, f, n FROM t ORDER BY i")
        .unwrap()
        .query_map((), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, Option<u32>>("n")?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                i64::MIN,
                "中文 'quote' ?1".to_string(),
                vec![0, 255, 1],
                1.5,
                None
            ),
            (1, String::new(), vec![], 2.0, Some(7)),
        ]
    );
    // 彙總函式的結果型別（NUMERIC、BOOLEAN）、陣列參數（含 NULL 元素與 bytea[]）
    let (sum, avg, any, cnt): (i64, f64, bool, usize) = c
        .query_row(
            "SELECT sum(n), avg(f), bool_or(i = 1), count(*) FROM t",
            (),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!((sum, avg, any, cnt), (7, 1.75, true, 2));
    let n: i64 = c
        .query_row(
            "SELECT count(*) FROM t WHERE i = ANY(?1::bigint[])",
            [array([1_i64, 5])],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    c.execute(
        "INSERT INTO t (i, s, b) SELECT * FROM unnest(?1::bigint[], ?2::text[], ?3::bytea[])",
        params![
            array([10_i64, 11]),
            array([Some("a"), None]),
            array([Some(vec![1_u8]), None])
        ],
    )
    .unwrap();
    let nulls: i64 = c
        .query_row(
            "SELECT count(*) FROM t WHERE s IS NULL AND b IS NULL",
            (),
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(nulls, 1);
    // 參數個數不符、沒有列、型別不符都是錯誤而不是 panic
    assert!(c.execute("INSERT INTO t (i) VALUES (?1)", ()).is_err());
    assert!(matches!(
        c.query_row("SELECT i FROM t WHERE i = 12345", (), |r| r
            .get::<_, i64>(0)),
        Err(crate::store::postgres::conn::PgError::NoRows)
    ));
    assert!(
        c.query_row("SELECT s FROM t LIMIT 1", (), |r| r.get::<_, Vec<u8>>(0))
            .is_err()
    );
    // 敘述出錯後，自動提交模式的連線仍可使用；交易中出錯後整個交易作廢，回復後可繼續
    assert!(c.execute("INSERT INTO nope VALUES (1)", ()).is_err());
    let tx = c.begin_tx(TxMode::Deferred).unwrap();
    assert!(tx.execute("INSERT INTO nope VALUES (1)", ()).is_err());
    assert!(tx.execute("INSERT INTO t (i) VALUES (99)", ()).is_err());
    drop(tx);
    assert_eq!(c.execute("INSERT INTO t (i) VALUES (99)", ()).unwrap(), 1);
    assert!(!c.is_broken());
}

#[test]
fn schema_is_created_once_and_foreign_or_newer_schemas_are_refused() {
    let Some(db) = TestDb::postgres("schema") else {
        return;
    };
    let pool = db.pool();
    let raw = pool.get(Access::ReadWrite, Duration::from_secs(1)).unwrap();
    assert_eq!(postgres::schema_state(&raw).unwrap(), SchemaState::Empty);
    assert!(postgres::probe_writable(&raw).is_err());
    // 同時啟動：只建立一次
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
        postgres::schema_state(&raw).unwrap(),
        SchemaState::Version(crate::store::sqlite::SCHEMA_VERSION)
    );
    assert!(postgres::probe_writable(&raw).is_ok());
    // 所有資料表都存在，TABLES 與 IDENTITY_COLUMNS 與實際的 schema 一致
    let tables: Vec<String> = raw
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
    let identity: Vec<(String, String)> = raw
        .prepare(
            "SELECT table_name::text, column_name::text FROM information_schema.columns
             WHERE table_schema = ?1 AND is_identity = 'YES' ORDER BY 1, 2",
        )
        .unwrap()
        .query_map([pool.schema()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut expected: Vec<(String, String)> = postgres::schema::IDENTITY_COLUMNS
        .iter()
        .map(|(t, c)| ((*t).to_string(), (*c).to_string()))
        .collect();
    expected.sort();
    assert_eq!(identity, expected);
    // 較新的 schema：KRS-E4405
    raw.execute(
        "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
        (),
    )
    .unwrap();
    let e = postgres::ensure_schema(&raw).unwrap_err();
    assert!(e.to_string().contains("KRS-E4405"), "{e}");
    // 搬移未完成：KRS-E4404
    raw.execute("DELETE FROM meta WHERE key = 'schema_version'", ())
        .unwrap();
    raw.execute(
        "INSERT INTO meta (key, value) VALUES (?1, 'x')",
        [postgres::META_MIGRATION],
    )
    .unwrap();
    assert_eq!(
        postgres::schema_state(&raw).unwrap(),
        SchemaState::Migrating
    );
    let e = postgres::ensure_schema(&raw).unwrap_err();
    assert!(e.to_string().contains("KRS-E4404"), "{e}");
    // 不是 Kairos 的 schema：KRS-E4402，不動裡面的東西
    raw.execute_batch(&format!(
        "DROP SCHEMA {0} CASCADE; CREATE SCHEMA {0}; CREATE TABLE {0}.other (x int)",
        pool.schema()
    ))
    .unwrap();
    raw.clear_statements();
    let e = postgres::ensure_schema(&raw).unwrap_err();
    assert!(e.to_string().contains("KRS-E4402"), "{e}");
    let n: i64 = raw
        .query_row("SELECT count(*) FROM other", (), |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn exclusive_access_and_the_running_service_exclude_each_other() {
    let Some(db) = TestDb::postgres("service") else {
        return;
    };
    let pool = db.pool();
    // 服務執行中：獨占的 CLI 取不到
    let (mut store, _) = db.store(noon);
    let Err(e) = postgres::open_exclusive(&pool, Duration::from_millis(300)) else {
        panic!("應該被服務擋住");
    };
    assert_eq!(e.kind(), ErrorKind::ResourceBusy, "{e}");
    // 一般的讀寫連線不受影響
    db.handle().put_meta("k", "v").unwrap();
    assert_eq!(
        store.write_batch(&[legacy(A, 1, noon(), 1.0)]),
        [WriteStatus::Written]
    );
    store.close();
    drop(store);
    // 服務停止後可以獨占；獨占期間服務無法啟動，第二個獨占者也取不到
    let exclusive = postgres::open_exclusive(&pool, Duration::from_millis(300)).unwrap();
    let Err(e) = postgres::report::PgStore::open(pool.clone(), db.opts(), noon) else {
        panic!("獨占期間服務不應啟動");
    };
    assert_eq!(e.kind(), ErrorKind::ResourceBusy, "{e}");
    assert!(postgres::open_exclusive(&pool, Duration::from_millis(200)).is_err());
    drop(exclusive);
    // 獨占連線歸還連線池後鎖已釋放
    let (mut store, _) = db.store(noon);
    store.close();
}

#[test]
fn concurrent_audit_appends_form_one_chain() {
    let Some(db) = TestDb::postgres("auditpar") else {
        return;
    };
    drop(db.handle());
    let pool = PgPool::with_data_dir(
        crate::config::PostgresSettings {
            pool_size: 10,
            ..db.pool().settings().clone()
        },
        Some(db.dir.clone()),
    );
    const WRITERS: usize = 8;
    const EACH: usize = 25;
    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let pool: Arc<PgPool> = pool.clone();
            std::thread::spawn(move || {
                let conn = postgres::open_write(&pool, Duration::from_secs(20)).unwrap();
                let actor = format!("user:w{w}");
                for i in 0..EACH {
                    let a = Audit {
                        actor: &actor,
                        actor_ip: None,
                        category: "test",
                        action: "concurrent",
                        object: None,
                        result: "success",
                        code: None,
                        details: Some(serde_json::json!({"i": i})),
                    };
                    // 交錯三種寫法：自動提交、Immediate 交易、Deferred 交易（只靠稽核鎖）；偶爾回復
                    match i % 4 {
                        0 => {
                            conn.audit_append(i as i64, &a).unwrap();
                        }
                        1 => {
                            let tx = conn.begin(TxMode::Immediate).unwrap();
                            tx.put_meta(&actor, "x").unwrap();
                            tx.audit_append(i as i64, &a).unwrap();
                            tx.commit().unwrap();
                        }
                        2 => {
                            let tx = conn.begin(TxMode::Deferred).unwrap();
                            tx.audit_append(i as i64, &a).unwrap();
                            tx.commit().unwrap();
                        }
                        _ => {
                            let tx = conn.begin(TxMode::Deferred).unwrap();
                            tx.audit_append(i as i64, &a).unwrap();
                            drop(tx);
                            conn.audit_append(i as i64, &a).unwrap();
                        }
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let total = (WRITERS * EACH) as i64;
    let s = db.handle();
    let report = s.audit_verify().unwrap();
    assert!(report.broken.is_none(), "{report:?}");
    assert_eq!(report.checked, total as u64);
    // id 連續、沒有空號（回復的交易不佔用）
    assert_eq!(db.int("SELECT min(id) FROM audit_events"), 1);
    assert_eq!(db.int("SELECT max(id) FROM audit_events"), total);
    assert_eq!(db.int("SELECT count(*) FROM audit_events"), total);
    pool.close_idle();
}

#[test]
fn concurrent_updates_are_atomic() {
    let Some(db) = TestDb::postgres("atomic") else {
        return;
    };
    let s = db.handle();
    // D2：扣次數不會超過 max_uses
    s.insert_token(&crate::enroll::store::TokenRow {
        token_id: "t".into(),
        secret_hash: vec![1],
        hint: "h".into(),
        name: "n".into(),
        group_name: "g".into(),
        tags: None,
        approval: kairos_proto::enroll::TokenApproval::Auto,
        max_uses: 5,
        used: 0,
        expires_at: i64::MAX / 2,
        allowed_cidrs: None,
        batch_mode: false,
        allowed_agent_ids: None,
        cert_lifetime_days: None,
        created_at: 0,
        created_by: "x".into(),
        disabled_at: None,
        disabled_by: None,
        disable_reason: None,
        last_used_at: None,
    })
    .unwrap();
    let pool = db.pool();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let pool = pool.clone();
            std::thread::spawn(move || {
                let c = postgres::open_write(&pool, Duration::from_secs(5)).unwrap();
                for i in 0..10 {
                    c.consume_token("t", i).unwrap();
                    // D6：設定版本的遞增不會遺失
                    c.bump_config_rev().unwrap();
                    c.bump_remote_config_rev().unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(s.get_token("t").unwrap().unwrap().used, 5);
    assert_eq!(s.config_rev().unwrap(), 40);
    assert_eq!(s.remote_config_rev().unwrap(), 40);
    // 等不到列鎖／advisory lock 時是 Busy（不是無限等待）
    let other = db.handle_busy(Duration::from_millis(200));
    let tx = s.begin(TxMode::Immediate).unwrap();
    let e = other.begin(TxMode::Immediate).err().unwrap();
    assert_eq!(e.kind(), StoreErrorKind::Busy);
    drop(tx);
    let _ = LockKind::Write;
}

/// 連線池的最後一個參考在 tokio 的 worker 執行緒上被釋放時（writer task 結束），閒置的連線要能正常關閉：
/// 同步驅動關閉連線時會 `block_on`，直接在 worker 上執行會 panic（量測 `store_throughput` 時發現）
#[test]
fn pool_can_be_dropped_on_a_runtime_worker() {
    let Some(db) = TestDb::postgres("pool_drop") else {
        return;
    };
    let settings = db.pool().settings().clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let joined = runtime.block_on(async move {
        tokio::spawn(async move {
            let pool = PgPool::new(settings);
            // 取用後歸還：池中留有一條閒置的連線
            let conn = tokio::task::spawn_blocking({
                let pool = pool.clone();
                move || pool.get(Access::ReadOnly, Duration::from_secs(1)).unwrap()
            })
            .await
            .unwrap();
            // 連線本身也在 worker 上釋放（歸還連線池）
            drop(conn);
            assert_eq!(pool.stats().idle, 1);
            drop(pool);
        })
        .await
    });
    assert!(
        joined.is_ok(),
        "在 worker 上釋放連線池不應 panic：{joined:?}"
    );
}
