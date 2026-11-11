//! 端對端：實際的 Kairos 與 Pulse 跑在 **PostgreSQL 後端**（`storage.backend = "postgres"`；
//! `kairos/HA.md` 第 5、7 節）上。
//!
//! 需要一個可用的 PostgreSQL（14 以上），以環境變數指定：
//!
//! ```text
//! KAIROS_TEST_PG_URL=postgres://<帳號>:<密碼>@<主機>:<埠>/<資料庫>
//! ```
//!
//! - 沒有設定時測試**明確略過**（在 stderr 印出原因後返回），不假裝驗證過任何東西；
//! - `KAIROS_TEST_PG_REQUIRED=1`（CI 的 Linux job 會設定）時沒有設定就失敗，確保這些測試真的跑了。
//!
//! 每個測試使用自己的 schema（`e2e_<pid>_<序號>`），不互相影響；測試結束後 schema 留在資料庫中
//! （CI 的資料庫是用完即丟的容器；本機請定期清掉測試用的資料庫）。

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use kp_e2e::{
    GOOD, Kairos, PULSE_REGISTERED, Proc, Pulse, TestDir, WAIT, poll_until, run_to_exit, test_pki,
};
use serde_json::Value;

/// `KAIROS_TEST_PG_URL` 的內容
struct PgUrl {
    user: String,
    password: String,
    host: String,
    port: u16,
    dbname: String,
}

/// 解析 `postgres://user:password@host:port/dbname`（測試用的最小解析：不處理百分比編碼與查詢參數）
fn parse_url(url: &str) -> Result<PgUrl, String> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or("必須以 postgres:// 開頭")?;
    let (auth, location) = rest.rsplit_once('@').ok_or("缺少 <帳號>:<密碼>@")?;
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (hostport, dbname) = location.split_once('/').ok_or("缺少 /<資料庫>")?;
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse::<u16>().map_err(|_| format!("埠號「{p}」不合法"))?,
        ),
        None => (hostport, 5432),
    };
    if user.is_empty() || host.is_empty() || dbname.is_empty() {
        return Err("帳號、主機與資料庫都不可為空".to_string());
    }
    Ok(PgUrl {
        user: user.to_string(),
        password: password.to_string(),
        host: host.to_string(),
        port,
        dbname: dbname.to_string(),
    })
}

/// 測試用的 PostgreSQL；沒有設定時印出略過的原因並回傳 `None`
fn test_database(test: &str) -> Option<PgUrl> {
    let required = std::env::var("KAIROS_TEST_PG_REQUIRED").is_ok_and(|v| v == "1");
    match std::env::var("KAIROS_TEST_PG_URL") {
        Ok(url) if !url.trim().is_empty() => Some(
            parse_url(url.trim()).unwrap_or_else(|e| panic!("KAIROS_TEST_PG_URL 不合法（{e}）")),
        ),
        _ if required => panic!(
            "{test}：KAIROS_TEST_PG_REQUIRED=1 但沒有設定 KAIROS_TEST_PG_URL（PostgreSQL 後端的端對端測試必須執行）"
        ),
        _ => {
            eprintln!(
                "略過 {test}：未設定 KAIROS_TEST_PG_URL（PostgreSQL 後端的端對端測試需要一個可用的資料庫，例如 postgres://kairos:kairos@127.0.0.1:5432/kairos）"
            );
            None
        }
    }
}

#[test]
fn url_parser_accepts_the_documented_form() {
    let u = parse_url("postgres://kairos:s3cret@127.0.0.1:55414/kairos_test").unwrap();
    assert_eq!(
        (
            u.user.as_str(),
            u.password.as_str(),
            u.host.as_str(),
            u.port,
            u.dbname.as_str()
        ),
        ("kairos", "s3cret", "127.0.0.1", 55414, "kairos_test")
    );
    let u = parse_url("postgresql://k@db.example.com/x").unwrap();
    assert_eq!((u.password.as_str(), u.port), ("", 5432));
    assert!(parse_url("mysql://a:b@c/d").is_err());
    assert!(parse_url("postgres://a:b@c").is_err());
}

/// 把 [`Kairos`] 的設定改成 PostgreSQL 後端：獨立的 schema、密碼放在檔案中
fn use_postgres(kairos: &Kairos, pg: &PgUrl) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let schema = format!(
        "e2e_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = kairos.dir.join("kairos.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("data_dir = \"data\"\n"), "{text}");
    let text = text.replace(
        "data_dir = \"data\"\n",
        "data_dir = \"data\"\nbackend = \"postgres\"\n",
    );
    let password_file = if pg.password.is_empty() {
        String::new()
    } else {
        std::fs::write(kairos.dir.join("pg.password"), format!("{}\n", pg.password)).unwrap();
        "password_file = \"pg.password\"\n".to_string()
    };
    let text = format!(
        "{text}\n[storage.postgres]\nhost = \"{}\"\nport = {}\ndbname = \"{}\"\nuser = \"{}\"\n{password_file}schema = \"{schema}\"\npool_size = 8\n",
        pg.host, pg.port, pg.dbname, pg.user
    );
    std::fs::write(&path, text).unwrap();
    schema
}

fn cli(dir: &Path, args: &[&str]) -> (i32, Proc) {
    let mut full: Vec<&str> = args.to_vec();
    full.extend(["--config", "kairos.toml"]);
    run_to_exit("kairos", dir, &full, &[])
}

/// `kairos reports export`（PostgreSQL 後端沒有 `data/kairos.db`，不能用 [`Kairos::reports`]）
fn export_reports(dir: &Path) -> Vec<Value> {
    let (code, p) = cli(dir, &["reports", "export", "--since", "7d"]);
    assert_eq!(code, 0, "kairos reports export 失敗\n{}", p.dump());
    p.stdout()
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("匯出的一行不是 JSON（{e}）：{line}"))
        })
        .collect()
}

/// durable report 的去重鍵（boot_id, report_seq）
fn dedup_keys(reports: &[Value]) -> Vec<(String, u64)> {
    reports
        .iter()
        .filter(|r| r["agent_id"] == GOOD.agent_id)
        .map(|r| {
            (
                r["boot_id"]
                    .as_str()
                    .expect("durable 的 report 應有 boot_id")
                    .to_string(),
                r["report_seq"].as_u64().expect("應有 report_seq"),
            )
        })
        .collect()
}

#[test]
fn kairos_and_pulse_run_on_the_postgres_backend() {
    let Some(pg) = test_database("kairos_and_pulse_run_on_the_postgres_backend") else {
        return;
    };
    let root = TestDir::new("postgres");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let schema = use_postgres(&kairos, &pg);

    // --check-config：實際連線，回報 PostgreSQL 的版本；schema 尚未建立
    let (code, p) = cli(&kairos.dir, &["--check-config"]);
    assert_eq!(code, 0, "{}", p.dump());
    assert!(p.output().contains("PostgreSQL"), "{}", p.dump());
    assert!(p.output().contains(&schema), "{}", p.dump());
    assert!(
        !p.output().contains(&pg.password) || pg.password.is_empty(),
        "密碼不應出現在輸出中"
    );

    // 啟動：建立 schema、Pulse 註冊、report 寫入 PostgreSQL
    let mut k = kairos.start();
    k.wait_for("主控台 API 已啟動", WAIT);
    assert!(k.output().contains("PostgreSQL"), "{}", k.dump());
    assert!(
        !kairos.dir.join("data").join("kairos.db").exists(),
        "PostgreSQL 後端不應建立 SQLite 資料庫檔"
    );
    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    k.wait_for("durable：report 可補送、去重", WAIT);
    assert!(
        poll_until(WAIT, || export_reports(&kairos.dir).len() >= 3),
        "report 應寫入 PostgreSQL\n{}\n{}",
        p.dump(),
        k.dump()
    );

    // CLI 與主控台 API 都讀得到（token 的建立寫入稽核鏈）
    let hosts = kairos.hosts();
    assert_eq!(hosts.len(), 1, "{hosts:?}");
    assert_eq!(hosts[0]["agent_id"], GOOD.agent_id, "{hosts:?}");
    let token = kairos.create_token("e2e-pg");
    let api = kairos.api(&token);
    assert!(
        poll_until(WAIT, || {
            api.data("/api/v1/hosts?status=online")[0]["agent_id"] == GOOD.agent_id
        }),
        "主機清單應列出線上的主機\n{}",
        k.dump()
    );
    assert!(
        poll_until(WAIT, || {
            let (status, v) = api.get(&format!(
                "/api/v1/hosts/{}/metrics/system.cpu.usage?range=1h",
                GOOD.agent_id
            ));
            status == 200
                && v["data"]["series"][0]["points"]
                    .as_array()
                    .is_some_and(|points| points.len() >= 2)
        }),
        "趨勢應有原始資料的點\n{}",
        k.dump()
    );
    // 系統健康：資料庫項目標明後端與連線池
    let health = api.data("/api/v1/system/health");
    let database = health["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "database")
        .unwrap_or_else(|| panic!("系統健康應有 database 項目：{health}"))
        .clone();
    assert_eq!(database["values"]["backend"], "postgres", "{database}");
    assert!(
        database["values"]["used_bytes"].as_u64().unwrap() > 0,
        "{database}"
    );
    assert_eq!(
        database["values"]["postgres"]["pool"]["size"], 8,
        "{database}"
    );

    // 資料庫正在使用中：db migrate 拒絕（KRS-E4304）；db backup 不支援 PostgreSQL（暫定 KRS-E4406）
    let export = root.sub("export").join("export.db");
    let to = format!("sqlite:{}", export.display());
    let (code, m) = cli(&kairos.dir, &["db", "migrate", "--to", &to]);
    assert_eq!(code, 1, "{}", m.dump());
    assert!(m.output().contains("KRS-E4304"), "{}", m.dump());
    assert!(!export.exists());
    let backup = root.sub("backup").join("kairos.db");
    let (code, b) = cli(&kairos.dir, &["db", "backup", backup.to_str().unwrap()]);
    assert_eq!(code, 1, "{}", b.dump());
    assert!(
        b.output().contains("KRS-E4406") && b.output().contains("pg_dump"),
        "{}",
        b.dump()
    );
    assert!(!backup.exists());

    // Kairos 重新啟動：主機從 PostgreSQL 載回，停機期間的取樣補送、不重複
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
    drop(k);
    std::thread::sleep(std::time::Duration::from_secs(3));
    let mut k = kairos.start();
    p.wait_for_count(PULSE_REGISTERED, 2, WAIT);
    let before = export_reports(&kairos.dir).len();
    assert!(
        poll_until(WAIT, || export_reports(&kairos.dir).len() >= before + 2),
        "{}",
        k.dump()
    );
    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());

    let reports = export_reports(&kairos.dir);
    let keys = dedup_keys(&reports);
    let unique: BTreeSet<&(String, u64)> = keys.iter().collect();
    assert_eq!(
        unique.len(),
        keys.len(),
        "不應有重複的 (boot_id, report_seq)：{keys:?}"
    );
    let boots: BTreeSet<&str> = keys.iter().map(|(b, _)| b.as_str()).collect();
    assert_eq!(boots.len(), 1, "Pulse 只啟動一次：{boots:?}");
    let mut seqs: Vec<u64> = keys.iter().map(|(_, s)| *s).collect();
    seqs.sort_unstable();
    let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
    assert_eq!(seqs, expected, "report_seq 不應缺號");
    assert!(!k.stderr().contains("[ERROR]"), "{}", k.dump());

    // 稽核雜湊鏈在 PostgreSQL 上完整（啟動、token 建立、關機…）
    let (code, v) = cli(&kairos.dir, &["audit", "verify", "--json"]);
    assert_eq!(code, 0, "{}", v.dump());
    let doc: Value = serde_json::from_str(v.stdout().trim()).unwrap();
    assert_eq!(doc["data"]["ok"], true, "{doc}");

    // 停機後搬到 SQLite：通過驗證，匯出的 SQLite 可以由 SQLite 後端的 Kairos 讀取，內容相同
    let (code, m) = cli(&kairos.dir, &["db", "migrate", "--to", &to, "--json"]);
    assert_eq!(code, 0, "{}", m.dump());
    let doc: Value = serde_json::from_str(m.stdout().trim())
        .unwrap_or_else(|e| panic!("db migrate --json 不是 JSON（{e}）：{}", m.stdout()));
    assert_eq!(doc["schema"], "kairos.db.migrate/v1", "{doc}");
    assert_eq!(doc["ok"], true, "{doc}");
    assert_eq!(doc["data"]["verification"]["audit_intact"], true, "{doc}");
    assert!(export.is_file());
    let single = Kairos::named(&root, "single", test_pki::CRL_EMPTY);
    single.release_ports();
    std::fs::create_dir_all(single.dir.join("data")).unwrap();
    std::fs::copy(&export, single.dir.join("data").join("kairos.db")).unwrap();
    // 稽核 HMAC 的金鑰跟著資料目錄
    for name in ["audit-hmac.key"] {
        let from = kairos.dir.join("data").join(name);
        if from.is_file() {
            std::fs::copy(&from, single.dir.join("data").join(name)).unwrap();
        }
    }
    let migrated = single.reports();
    assert_eq!(
        migrated, reports,
        "搬到 SQLite 後的 report 應與 PostgreSQL 中的相同"
    );
    let (code, v) = cli(&single.dir, &["audit", "verify", "--json"]);
    assert_eq!(code, 0, "{}", v.dump());
    let doc: Value = serde_json::from_str(v.stdout().trim()).unwrap();
    assert_eq!(doc["data"]["ok"], true, "{doc}");
}

#[test]
fn unreachable_postgres_is_reported_with_its_code() {
    // 不需要資料庫：連到一個沒有人監聽的埠
    let root = TestDir::new("postgres-down");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    kairos.release_ports();
    let closed = kp_e2e::free_port();
    kp_e2e::release_port(closed);
    let pg = PgUrl {
        user: "kairos".into(),
        password: "not-used-secret".into(),
        host: "127.0.0.1".into(),
        port: closed,
        dbname: "kairos".into(),
    };
    use_postgres(&kairos, &pg);
    let (code, p) = cli(&kairos.dir, &["--check-config"]);
    assert_ne!(code, 0, "{}", p.dump());
    assert!(p.output().contains("KRS-E4401"), "{}", p.dump());
    assert!(
        !p.output().contains("not-used-secret"),
        "密碼不應出現在輸出中\n{}",
        p.dump()
    );
    // 啟動同樣失敗，訊息帶代碼，不建立 SQLite 資料庫
    let (code, p) = run_to_exit("kairos", &kairos.dir, &["--config", "kairos.toml"], &[]);
    assert_ne!(code, 0, "{}", p.dump());
    assert!(p.output().contains("KRS-E4401"), "{}", p.dump());
    assert!(!p.output().contains("not-used-secret"), "{}", p.dump());
    assert!(!kairos.dir.join("data").join("kairos.db").exists());
}
