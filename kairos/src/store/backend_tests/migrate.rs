//! `kairos db migrate`（[`crate::store::migrate`]）：SQLite → PostgreSQL → SQLite 來回後內容相同、三項驗證、
//! `--reports-since`、目的端非空、未完成的目的端、來源或目的正在使用中。需要 PostgreSQL，否則略過

use std::time::Duration;

use super::TestDb;
use super::domain2::seed_hosts;
use super::report::{A, noon};
use crate::audit::Audit;
use crate::remote::store::{ArtifactRow, NewRolloutHost, ReleaseRow, RolloutRow};
use crate::store::migrate::{self, Options, TableResult};
use crate::store::postgres::conn::Value;
use crate::store::postgres::pool::Access;
use crate::store::postgres::{self, SchemaState, schema};
use crate::users::store::NewUser;

/// 來源資料庫：report（durable 與非 durable、`agent_ts` 兩種型別）、彙總、資產、使用者、稽核鏈（含 HMAC）、
/// 設定、PKI、告警規則、遠端管理（外部 URL 的版本、已登記的發佈檔、帶 schema v11 雙人覆核欄位的推送）
fn seed(db: &TestDb) -> std::sync::Arc<crate::audit::AuditKey> {
    seed_hosts(db);
    let key = crate::audit::ensure_key(&db.dir).unwrap();
    let s = db.handle();
    let uid = s
        .create_user(
            &NewUser {
                username: "Ann",
                display_name: Some("安"),
                email: None,
                source: "local",
                ldap_dn: None,
                password_hash: Some("$argon2id$x"),
                must_change_password: false,
            },
            "system",
            1,
        )
        .unwrap()
        .unwrap();
    for i in 0..20 {
        s.audit_append(
            1000 + i,
            &Audit {
                actor: "user:Ann",
                actor_ip: Some("10.0.0.1"),
                category: "users",
                action: "user.update",
                object: Some("user:Ann"),
                result: "success",
                code: None,
                details: Some(serde_json::json!({"i": i, "名稱": "測試"})),
            },
        )
        .unwrap();
    }
    s.put_meta("custom", "值").unwrap();
    s.put_meta("number", "42").unwrap();
    s.set_setting("enroll.mode", &serde_json::json!("open"), "user:Ann", 5)
        .unwrap();
    s.insert_api_token("tok", "t", &[7; 32], "read", 1, None, None, None, Some(uid))
        .unwrap();
    let ca = s
        .insert_pending_ca(
            &|id| format!("CN=CA {id}"),
            &[1; 32],
            &|id| format!("{id}.key"),
            1,
        )
        .unwrap();
    s.insert_cert(&crate::pki::store::CertRow {
        serial: "0a".into(),
        ca_id: Some(ca),
        agent_id: A.into(),
        spki_sha256: vec![3; 32],
        not_before: 1,
        not_after: 2,
        cert_der: vec![0, 1, 2, 255],
        issued_at: 1,
        issued_via: "enroll".into(),
        request_id: None,
        approved_by: None,
        first_used_at: None,
        revoked_at: None,
        revoke_effective_at: None,
        revocation_reason: None,
        revoked_by: None,
        revoke_comment: None,
        crl_number: None,
    })
    .unwrap();
    s.ensure_builtin_rules(1).unwrap();
    // 遠端管理（0.10.0）：外部 URL 的版本與已登記而未存入的發佈檔、待核准後被退回再核准的推送
    let release = s
        .insert_release(&ReleaseRow {
            id: 0,
            version: "0.10.1".into(),
            commit: "abc1234".into(),
            source: "external".into(),
            base_url: Some("https://mirror.example/pulse/0.10.1/".into()),
            sha256sums: "sums".into(),
            signature: "sig".into(),
            signer_fingerprint: None,
            signer_principal: None,
            manifest_name: None,
            manifest: None,
            imported_at: 10,
            imported_by: "cli:root".into(),
        })
        .unwrap();
    s.upsert_artifact(&ArtifactRow {
        release_id: release,
        filename: "pulse_0.10.1_amd64.deb".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        format: "deb".into(),
        sha256: "ee".into(),
        size: 4321,
        stored: false,
        stored_at: None,
    })
    .unwrap();
    let mut rollout = RolloutRow {
        id: ROLLOUT.into(),
        name: "週末升級".into(),
        release_id: release,
        target_version: "0.10.1".into(),
        selector: "{\"all\":true}".into(),
        waves: "[]".into(),
        schedule: "{\"starts_at\":1790000900000}".into(),
        options: "{}".into(),
        state: "draft".into(),
        current_wave: 0,
        wave_started_at: None,
        soak_until: None,
        awaiting_continue: false,
        pause_reason: None,
        pause_cause: None,
        cancel_reason: None,
        created_at: 20,
        created_by: "user:Ann".into(),
        started_at: None,
        completed_at: None,
        updated_at: 20,
        submitted_at: None,
        submitted_by: None,
        approved_at: None,
        approved_by: None,
        reject_reason: None,
    };
    s.insert_rollout(&rollout).unwrap();
    rollout.state = "scheduled".into();
    rollout.submitted_at = Some(21);
    rollout.submitted_by = Some("user:Ann".into());
    rollout.approved_at = Some(22);
    rollout.approved_by = Some("user:Bob".into());
    rollout.reject_reason = Some("先前被退回：時段不對".into());
    rollout.updated_at = 22;
    s.update_rollout_state(&rollout).unwrap();
    let host = s.host_id_of(A).unwrap().unwrap();
    s.insert_rollout_hosts(
        ROLLOUT,
        &[NewRolloutHost {
            host_id: host,
            upgrade_id: "up-1".into(),
            wave: 0,
            ordinal: 0,
            state: "pending".into(),
            reason: None,
            from_version: Some("0.9.0".into()),
        }],
        21,
    )
    .unwrap();
    key
}

/// [`seed`] 建立的推送
const ROLLOUT: &str = "r-20261101-0001";

fn dump(db: &TestDb, table: &str) -> Vec<Vec<Value>> {
    let mut rows = db.rows(&format!("SELECT * FROM {table}"));
    if table == "meta" {
        rows.retain(|r| !matches!(&r[0], Value::Text(k) if k == "schema_upgraded_by"));
    }
    rows.sort_by_key(|r| format!("{r:?}"));
    rows
}

fn quiet() -> impl FnMut(&TableResult) {
    |_| {}
}

#[test]
fn sqlite_to_postgres_and_back_is_lossless() {
    let Some(pg) = TestDb::postgres("migrate") else {
        return;
    };
    let src = TestDb::sqlite("migrate-src");
    let key = seed(&src);
    // 試執行：不寫入
    let dry = migrate::sqlite_to_postgres(
        &src.opts(),
        &pg.pool(),
        &Options {
            dry_run: true,
            ..Default::default()
        },
        Some(&key),
        &mut quiet(),
    )
    .unwrap();
    assert!(dry.dry_run && dry.total_rows > 100 && dry.audit_intact);
    assert_eq!(
        postgres::schema_state(
            &pg.pool()
                .get(Access::ReadOnly, Duration::from_secs(1))
                .unwrap()
        )
        .unwrap(),
        SchemaState::Empty
    );

    let mut seen = Vec::new();
    let summary = migrate::sqlite_to_postgres(
        &src.opts(),
        &pg.pool(),
        &Options::default(),
        Some(&key),
        &mut |t| seen.push(t.name),
    )
    .unwrap();
    assert_eq!(seen, schema::TABLES);
    assert_eq!(summary.total_rows, dry.total_rows);
    assert_eq!(summary.sampled, 25);
    assert_eq!(summary.audit_checked, 20);
    assert!(summary.audit_intact);
    // 目的端可以使用：稽核鏈以同一把金鑰驗證通過、可以繼續寫入（序列已對齊）
    std::fs::copy(
        src.dir.join("audit-hmac.key"),
        pg.dir.join("audit-hmac.key"),
    )
    .unwrap();
    let s = pg.handle();
    let v = s.audit_verify().unwrap();
    assert!(v.broken.is_none() && v.hmac.status == "verified", "{v:?}");
    assert_eq!(s.get_meta("custom").unwrap().as_deref(), Some("值"));
    assert_eq!(s.find_user("ann").unwrap().map(|u| u.user_id), Some(1));
    assert_eq!(
        pg.int("SELECT count(*) FROM reports WHERE agent_ts_ms IS NOT NULL"),
        24
    );
    assert_eq!(
        pg.int("SELECT count(*) FROM reports WHERE agent_ts_text IS NOT NULL"),
        1
    );
    // schema v11 的欄位（rollouts 的雙人覆核）與外部 URL 的版本一併搬移
    let moved = s.get_rollout(ROLLOUT).unwrap().unwrap();
    assert_eq!(
        (
            moved.state.as_str(),
            moved.submitted_at,
            moved.submitted_by.as_deref(),
            moved.approved_at,
            moved.approved_by.as_deref(),
            moved.reject_reason.as_deref()
        ),
        (
            "scheduled",
            Some(21),
            Some("user:Ann"),
            Some(22),
            Some("user:Bob"),
            Some("先前被退回：時段不對")
        )
    );
    assert_eq!(moved, src.handle().get_rollout(ROLLOUT).unwrap().unwrap());
    assert_eq!(s.rollout_hosts(ROLLOUT).unwrap().len(), 1);
    let release = s.get_release("0.10.1").unwrap().unwrap();
    assert_eq!(
        (release.source.as_str(), release.base_url.as_deref()),
        ("external", Some("https://mirror.example/pulse/0.10.1/"))
    );
    let artifacts = s.release_artifacts(release.id).unwrap();
    assert_eq!(
        artifacts
            .iter()
            .map(|a| (a.size, a.stored))
            .collect::<Vec<_>>(),
        [(4321, false)]
    );
    // 兩個後端的 rollouts 欄位一致（搬移時逐欄對應，缺一個就會拒絕）
    let pg_columns: Vec<String> = pg
        .rows(
            "SELECT column_name::text FROM information_schema.columns
             WHERE table_schema = current_schema() AND table_name = 'rollouts'
             ORDER BY column_name",
        )
        .into_iter()
        .filter_map(|r| match r.into_iter().next() {
            Some(Value::Text(t)) => Some(t),
            _ => None,
        })
        .collect();
    let mut lite_columns: Vec<String> = src
        .rows("SELECT name FROM pragma_table_info('rollouts')")
        .into_iter()
        .filter_map(|r| match r.into_iter().next() {
            Some(Value::Text(t)) => Some(t),
            _ => None,
        })
        .collect();
    lite_columns.sort();
    assert_eq!(pg_columns, lite_columns);
    for c in ["submitted_at", "submitted_by", "reject_reason"] {
        assert!(pg_columns.iter().any(|x| x == c), "{c}");
    }
    drop(s);
    // 目的端非空：拒絕，不做合併
    let e = migrate::sqlite_to_postgres(
        &src.opts(),
        &pg.pool(),
        &Options::default(),
        Some(&key),
        &mut quiet(),
    )
    .unwrap_err();
    assert_eq!(e.code, "KRS-E4404");
    assert!(e.message.contains("已經有"), "{}", e.message);

    // 搬回 SQLite：與原本逐表相同
    let back = TestDb::sqlite("migrate-back");
    let dest = back.opts().path;
    let summary = migrate::postgres_to_sqlite(
        &pg.pool(),
        &dest,
        &back.opts(),
        &Options::default(),
        Some(&key),
        &mut quiet(),
    )
    .unwrap();
    assert_eq!(summary.sampled, 25);
    assert!(dest.is_file() && !migrate::partial_path(&dest).exists());
    for table in schema::TABLES {
        assert_eq!(dump(&back, table), dump(&src, table), "{table}");
    }
    // 無型別欄位的型別也還原
    let types = "SELECT typeof(agent_ts), count(*) FROM reports GROUP BY 1 ORDER BY 1";
    assert_eq!(back.rows(types), src.rows(types));
    std::fs::copy(
        src.dir.join("audit-hmac.key"),
        back.dir.join("audit-hmac.key"),
    )
    .unwrap();
    let v = back.handle().audit_verify().unwrap();
    assert!(v.broken.is_none() && v.hmac.status == "verified", "{v:?}");
    // 目的檔已存在：拒絕
    let e = migrate::postgres_to_sqlite(
        &pg.pool(),
        &dest,
        &back.opts(),
        &Options::default(),
        Some(&key),
        &mut quiet(),
    )
    .unwrap_err();
    assert!(e.config && e.message.contains("已經存在"), "{}", e.message);
    // 搬移後的 PostgreSQL 可以直接啟動、繼續寫入（序列已對齊）
    {
        let (mut store, restored) = pg.store(noon);
        assert_eq!(restored.len(), 2);
        let r = super::report::legacy("cccccccc-bbbb-4ccc-8ddd-eeeeeeeeeeee", 1, noon(), 1.0);
        assert_eq!(
            store.write_batch(&[r]),
            [crate::storage::WriteStatus::Written]
        );
        assert_eq!(pg.int("SELECT max(host_id) FROM hosts"), 3);
        assert_eq!(pg.int("SELECT max(id) FROM reports"), 26);
        store.close();
    }
}

#[test]
fn reports_since_copies_only_recent_raw_data() {
    let Some(pg) = TestDb::postgres("migrate-since") else {
        return;
    };
    let src = TestDb::sqlite("migrate-since-src");
    seed(&src);
    let cutoff = noon().timestamp_millis() + 5 * 30_000;
    let expected = src.int(&format!(
        "SELECT count(*) FROM reports WHERE ts >= {cutoff}"
    ));
    assert!(expected > 0 && expected < 25);
    let summary = migrate::sqlite_to_postgres(
        &src.opts(),
        &pg.pool(),
        &Options {
            reports_since_ms: Some(cutoff),
            ..Default::default()
        },
        None,
        &mut quiet(),
    )
    .unwrap();
    assert_eq!(pg.int("SELECT count(*) FROM reports"), expected);
    assert_eq!(
        pg.int("SELECT count(*) FROM process_snapshots"),
        src.int(&format!(
            "SELECT count(*) FROM process_snapshots WHERE ts >= {cutoff}"
        ))
    );
    // 彙總、清冊、稽核一律全部複製
    for table in ["rollup_5m", "rollup_1h", "hosts", "audit_events", "boots"] {
        assert_eq!(
            pg.int(&format!("SELECT count(*) FROM {table}")),
            src.int(&format!("SELECT count(*) FROM {table}")),
            "{table}"
        );
    }
    assert_eq!(i64::try_from(summary.sampled).unwrap(), expected);
    // 來源沒有被修改
    assert_eq!(src.int("SELECT count(*) FROM reports"), 25);
}

#[test]
fn unfinished_and_busy_targets_are_refused() {
    let Some(pg) = TestDb::postgres("migrate-busy") else {
        return;
    };
    let src = TestDb::sqlite("migrate-busy-src");
    seed(&src);
    // 來源正在使用中（Kairos 執行中）：KRS-E4304
    {
        let (mut store, _) = src.store(noon);
        let e = migrate::sqlite_to_postgres(
            &src.opts(),
            &pg.pool(),
            &Options::default(),
            None,
            &mut quiet(),
        )
        .unwrap_err();
        assert_eq!(e.code, "KRS-E4304", "{}", e.message);
        store.close();
    }
    // 沒有來源資料庫
    let empty = TestDb::sqlite("migrate-empty");
    let e = migrate::sqlite_to_postgres(
        &empty.opts(),
        &pg.pool(),
        &Options::default(),
        None,
        &mut quiet(),
    )
    .unwrap_err();
    assert!(e.config && e.message.contains("找不到"), "{}", e.message);
    // 模擬中斷：目的端只有「搬移中」的標記
    {
        let c = pg
            .pool()
            .get(Access::ReadWrite, Duration::from_secs(1))
            .unwrap();
        let tx = c.begin_tx(crate::store::db::TxMode::Deferred).unwrap();
        postgres::create_tables(&tx).unwrap();
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, 'x')",
            [postgres::META_MIGRATION],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO hosts (agent_id, hostname, first_seen, last_seen) VALUES ('leftover', 'h', 1, 1)",
            (),
        )
        .unwrap();
        tx.commit().unwrap();
        // Kairos 拒絕使用未完成的目的端
        let e = postgres::ensure_schema(&c).unwrap_err();
        assert!(e.to_string().contains("KRS-E4404"), "{e}");
        drop(c);
        let e = postgres::report::PgStore::open(pg.pool(), pg.opts(), noon)
            .err()
            .unwrap();
        assert!(e.to_string().contains("KRS-E4404"), "{e}");
    }
    let e = migrate::sqlite_to_postgres(
        &src.opts(),
        &pg.pool(),
        &Options::default(),
        None,
        &mut quiet(),
    )
    .unwrap_err();
    assert!(e.message.contains("--restart"), "{}", e.message);
    migrate::sqlite_to_postgres(
        &src.opts(),
        &pg.pool(),
        &Options {
            restart: true,
            ..Default::default()
        },
        None,
        &mut quiet(),
    )
    .unwrap();
    assert_eq!(pg.int("SELECT count(*) FROM hosts"), 2);
    // PostgreSQL 正在使用中（有 Kairos 節點連著）：拒絕
    let (mut store, _) = pg.store(noon);
    let out = TestDb::sqlite("migrate-out");
    let path = out.opts().path;
    let e = migrate::postgres_to_sqlite(
        &pg.pool(),
        &path,
        &out.opts(),
        &Options::default(),
        None,
        &mut quiet(),
    )
    .unwrap_err();
    assert_eq!(e.code, "KRS-E4304", "{}", e.message);
    assert!(!path.exists() && !migrate::partial_path(&path).exists());
    store.close();
    drop(store);
    // 未完成的 SQLite 目的檔：需要 --restart
    std::fs::write(migrate::partial_path(&path), b"junk").unwrap();
    let e = migrate::postgres_to_sqlite(
        &pg.pool(),
        &path,
        &out.opts(),
        &Options::default(),
        None,
        &mut quiet(),
    )
    .unwrap_err();
    assert!(e.message.contains("--restart"), "{}", e.message);
    migrate::postgres_to_sqlite(
        &pg.pool(),
        &path,
        &out.opts(),
        &Options {
            restart: true,
            ..Default::default()
        },
        None,
        &mut quiet(),
    )
    .unwrap();
    assert_eq!(out.int("SELECT count(*) FROM hosts"), 2);
}
