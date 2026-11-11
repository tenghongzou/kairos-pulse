//! 端對端：**叢集模式（階段 A：主動／備援）**——兩個實際的 Kairos 共用一個 PostgreSQL，加上實際的 Pulse
//! （`kairos/HA.md` 第 3.2、6、11、17 節）。
//!
//! - `leader_failover_keeps_durable_data`（第 17 節 F1、F11、F14）：
//!   1. `kairos cluster init` 建立叢集；第二個節點在匯入金鑰之前不參與選舉（`/readyz` 的 `keys: mismatch`），
//!      `kairos cluster keys export`／`import` 之後成為備援（不監聽 agent 與主控台的連接埠）；
//!   2. 主動節點被強制終止（SIGKILL）→ 備援在租約到期後接手並開始監聽 → Pulse 經
//!      `server.fallback_addresses` 重連；
//!   3. 原本的節點重新啟動為備援；`kairos cluster step-down` 讓 leader 以結束代碼 75 結束，另一個節點接手；
//!   4. 全程 durable 的 report **不遺失、不重複**（同一個 `boot_id` 的 `report_seq` 連續），稽核鏈完整。
//! - `a_second_solo_kairos_is_refused`：沒有啟用叢集模式的 Kairos 連到已有另一個 Kairos 在服務的 schema 時
//!   拒絕啟動（`KRS-E4402`，結束代碼 3）。
//!
//! 需要 `KAIROS_TEST_PG_URL`（與 `tests/postgres.rs` 相同：沒有設定時明確略過，`KAIROS_TEST_PG_REQUIRED=1`
//! 時不可略過）。接手時間印在 stderr（`FAILOVER_SECS=…`），供量測使用。

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use kp_e2e::{
    GOOD, KAIROS_STARTED, Kairos, PULSE_REGISTERED, Proc, Pulse, TestDir, WAIT, poll_until,
    run_to_exit, test_pki,
};
use serde_json::Value;

/// 換手相關的等待上限：租約 15 秒＋取得 2 秒＋載入，再加上 Pulse 的退避（2、4、8、16 秒…）
const FAILOVER_WAIT: Duration = Duration::from_secs(90);
/// 規格的接手時間（HA.md 第 3.2 節）：租約到期 ≤ 15 秒、取得 ≤ 2 秒、載入與監聽約 2～5 秒；
/// 測試機忙碌時另留 8 秒
const TAKEOVER_LIMIT: Duration = Duration::from_secs(30);

struct PgUrl {
    user: String,
    password: String,
    host: String,
    port: u16,
    dbname: String,
}

/// 解析 `postgres://user:password@host:port/dbname`（與 `tests/postgres.rs` 相同的最小解析）
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
            "{test}：KAIROS_TEST_PG_REQUIRED=1 但沒有設定 KAIROS_TEST_PG_URL（叢集的端對端測試必須執行）"
        ),
        _ => {
            eprintln!(
                "略過 {test}：未設定 KAIROS_TEST_PG_URL（叢集的端對端測試需要一個可用的 PostgreSQL，例如 postgres://kairos:kairos@127.0.0.1:5432/kairos）"
            );
            None
        }
    }
}

fn new_schema() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "e2ec_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// 一個節點：[`Kairos`] 加上探針的連接埠
struct Node {
    kairos: Kairos,
    obs_port: u16,
}

/// 把 [`Kairos`] 的設定改成 PostgreSQL 後端（共用 `schema`）；`cluster` 時另啟用叢集模式與探針監聽
fn node(root: &TestDir, name: &str, pg: &PgUrl, schema: &str, cluster: bool) -> Node {
    let kairos = Kairos::named(root, name, test_pki::CRL_EMPTY);
    let obs_port = kp_e2e::free_port();
    let path = kairos.dir.join("kairos.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    // 叢集模式不支援 JSONL 鏡像（KRS-E0202）
    assert!(text.contains("jsonl_mirror = true\n"), "{text}");
    let text = text.replace("jsonl_mirror = true\n", "backend = \"postgres\"\n");
    let password_file = if pg.password.is_empty() {
        String::new()
    } else {
        std::fs::write(kairos.dir.join("pg.password"), format!("{}\n", pg.password)).unwrap();
        "password_file = \"pg.password\"\n".to_string()
    };
    let mut text = format!(
        "{text}\n[storage.postgres]\nhost = \"{}\"\nport = {}\ndbname = \"{}\"\nuser = \"{}\"\n{password_file}schema = \"{schema}\"\npool_size = 8\n",
        pg.host, pg.port, pg.dbname, pg.user
    );
    if cluster {
        text.push_str(&format!(
            "\n[cluster]\nenabled = true\nnode_id = \"{name}\"\nadvertise = \"127.0.0.1:{}\"\n\n[observability]\nlisten = \"127.0.0.1:{obs_port}\"\n",
            kairos.port
        ));
    } else {
        kp_e2e::release_port(obs_port);
    }
    std::fs::write(&path, text).unwrap();
    Node { kairos, obs_port }
}

fn cli(dir: &Path, args: &[&str]) -> (i32, Proc) {
    let mut full: Vec<&str> = args.to_vec();
    full.extend(["--config", "kairos.toml"]);
    run_to_exit("kairos", dir, &full, &[])
}

/// 以備援身分啟動（不等「已啟動，監聽」：備援不監聽）；回傳行程
fn spawn_standby(n: &Node) -> Proc {
    n.kairos.release_ports();
    kp_e2e::release_port(n.obs_port);
    let mut p = Proc::spawn("kairos", &n.kairos.dir, &["--config", "kairos.toml"], &[]);
    p.wait_for("等待取得 leader 租約", WAIT);
    p
}

/// 純 HTTP 的 GET（探針監聽）：回傳（狀態碼, 本文）；連不上時為 `None`
fn http_get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut text = String::new();
    s.read_to_string(&mut text).ok()?;
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    let body = text
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    Some((status, body))
}

/// `/readyz` 的（狀態碼, checks）
fn readyz(port: u16, query: &str) -> Option<(u16, Value)> {
    let (status, body) = http_get(port, &format!("/readyz{query}"))?;
    let v: Value = serde_json::from_str(body.trim()).ok()?;
    Some((status, v["checks"].clone()))
}

fn cluster_status(dir: &Path) -> (i32, Value) {
    let (code, p) = cli(dir, &["cluster", "status", "--json"]);
    let doc: Value = serde_json::from_str(p.stdout().trim())
        .unwrap_or_else(|e| panic!("cluster status --json 不是 JSON（{e}）\n{}", p.dump()));
    assert_eq!(doc["schema"], "kairos.cluster.status/v1", "{doc}");
    (code, doc["data"].clone())
}

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

/// 不遺失、不重複：只有一個 boot_id，report_seq 是 1..=n
fn assert_durable_complete(reports: &[Value]) -> usize {
    let keys = dedup_keys(reports);
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
    seqs.len()
}

fn status_file(pulse: &Pulse) -> Value {
    let path = pulse.dir.join("state").join("status.json");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// 等到 Pulse 的狀態檔記錄「已註冊到節點 `node`」
fn wait_registered_to(pulse: &Pulse, node: &str) -> Value {
    let mut last = Value::Null;
    let ok = poll_until(FAILOVER_WAIT, || {
        last = status_file(pulse);
        last["connection"]["state"] == "registered" && last["connection"]["server_node"] == node
    });
    assert!(ok, "狀態檔應記錄已註冊到節點 {node}：{last:#}");
    last
}

#[test]
fn leader_failover_keeps_durable_data() {
    let Some(pg) = test_database("leader_failover_keeps_durable_data") else {
        return;
    };
    let root = TestDir::new("cluster-failover");
    let schema = new_schema();
    let a = node(&root, "kairos-a", &pg, &schema, true);
    let b = node(&root, "kairos-b", &pg, &schema, true);

    // ---- 建立叢集：第一個節點 init，取得租約 ----
    let (code, p) = cli(&a.kairos.dir, &["cluster", "init"]);
    assert_eq!(code, 0, "{}", p.dump());
    assert!(p.output().contains("叢集已建立"), "{}", p.dump());
    for key in [
        "alerts-secrets.key",
        "console.key",
        "audit-hmac.key",
        "privacy.key",
    ] {
        assert!(a.kairos.dir.join("data").join(key).is_file(), "{key}");
    }
    kp_e2e::release_port(a.obs_port);
    let mut ka = a.kairos.start();
    assert!(ka.output().contains("KRS-I8501"), "{}", ka.dump());

    // ---- 第二個節點：金鑰還沒匯入時不參與選舉，也不自動建立金鑰 ----
    let mut kb = spawn_standby(&b);
    assert!(
        poll_until(WAIT, || {
            readyz(b.obs_port, "").is_some_and(|(s, c)| s == 503 && c["keys"] == "mismatch")
        }),
        "金鑰不一致的節點 /readyz 應為 503、keys: mismatch\n{}",
        kb.dump()
    );
    kb.wait_for("KRS-E8504", WAIT);
    assert!(
        !b.kairos.dir.join("data").join("console.key").exists(),
        "叢集模式下不應自動建立金鑰"
    );
    assert_eq!(
        http_get(b.obs_port, "/healthz").map(|r| r.0),
        Some(200),
        "備援節點的 /healthz 仍是 200"
    );

    // ---- 分發金鑰：export（A）→ import（B）----
    let pw = root.sub("secrets").join("keys.password");
    std::fs::write(&pw, "correct horse battery staple\n").unwrap();
    let bundle = root.sub("secrets").join("cluster.keys");
    let (code, p) = cli(
        &a.kairos.dir,
        &[
            "cluster",
            "keys",
            "export",
            bundle.to_str().unwrap(),
            "--password-file",
            pw.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{}", p.dump());
    let (code, p) = cli(
        &b.kairos.dir,
        &[
            "cluster",
            "keys",
            "import",
            bundle.to_str().unwrap(),
            "--password-file",
            pw.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{}", p.dump());
    assert!(p.output().contains("與叢集一致"), "{}", p.dump());
    assert!(
        poll_until(WAIT, || {
            readyz(b.obs_port, "")
                .is_some_and(|(s, c)| s == 503 && c["keys"] == "ok" && c["cluster"] == "standby")
        }),
        "匯入金鑰後 B 應成為備援（/readyz 503、cluster: standby、keys: ok）\n{}",
        kb.dump()
    );
    // 備援不監聽 agent 與主控台的連接埠
    assert!(
        kp_e2e::port_is_free(b.kairos.port),
        "備援不應監聽 agent 連線埠"
    );
    assert!(
        kp_e2e::port_is_free(b.kairos.console_port),
        "備援不應監聽主控台"
    );
    assert_eq!(kb.count(KAIROS_STARTED), 0, "{}", kb.dump());

    // ---- 主動節點：/readyz 200；kairos cluster status 一個 leader、一個備援 ----
    assert!(
        poll_until(WAIT, || {
            readyz(a.obs_port, "").is_some_and(|(s, c)| s == 200 && c["cluster"] == "leader")
        }),
        "{}",
        ka.dump()
    );
    assert_eq!(readyz(a.obs_port, "?scope=leader").map(|r| r.0), Some(200));
    assert_eq!(readyz(b.obs_port, "?scope=leader").map(|r| r.0), Some(503));
    let mut status = Value::Null;
    assert!(
        poll_until(WAIT, || {
            status = cluster_status(&b.kairos.dir).1;
            status["leader"]["node_id"] == "kairos-a"
                && status["nodes"][1]["role"] == "standby"
                && status["nodes"][1]["keys_ok"] == true
        }),
        "{status:#}"
    );
    assert_eq!(status["leader"]["epoch"], 1, "{status:#}");
    assert_eq!(status["nodes"][0]["role"], "leader", "{status:#}");
    assert_eq!(status["nodes"][0]["config_drift"], false, "{status:#}");
    assert_eq!(status["nodes"][1]["config_drift"], false, "{status:#}");
    let metrics = http_get(b.obs_port, "/metrics").unwrap().1;
    assert!(
        metrics.contains("kairos_cluster_role{role=\"standby\"} 1")
            && metrics.contains("kairos_cluster_lease_epoch 1"),
        "{metrics}"
    );

    // ---- Pulse：主位址 A、備用位址 B ----
    let pulse = Pulse::new(&root, "pulse", a.kairos.port, GOOD);
    let fallback = format!("127.0.0.1:{}", b.kairos.port);
    let mut p = pulse.start(&[("PULSE__SERVER__FALLBACK_ADDRESSES", &fallback)]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    let s = wait_registered_to(&pulse, "kairos-a");
    assert_eq!(s["connection"]["candidates"], 2, "{s:#}");
    assert!(
        poll_until(WAIT, || export_reports(&a.kairos.dir).len() >= 3),
        "report 應寫入 PostgreSQL\n{}\n{}",
        p.dump(),
        ka.dump()
    );

    // ---- F1：主動節點被強制終止 → 備援接手 → Pulse 重連 ----
    let killed = Instant::now();
    ka.kill();
    kb.wait_for("KRS-I8501", FAILOVER_WAIT);
    let acquired = killed.elapsed();
    kb.wait_for(KAIROS_STARTED, FAILOVER_WAIT);
    let listening = killed.elapsed();
    eprintln!(
        "FAILOVER_SECS={:.2} LEASE_SECS={:.2}（強制終止 → 備援取得租約 → 開始監聽）",
        listening.as_secs_f64(),
        acquired.as_secs_f64()
    );
    assert!(
        listening <= TAKEOVER_LIMIT,
        "備援應在租約到期（15 秒）加上取得與載入的時間內接手，實際 {listening:?}\n{}",
        kb.dump()
    );
    // 租約沒到期之前不會被接手（不會有兩個 leader）：最後一次續約到期至少還要 10 秒
    assert!(
        acquired >= Duration::from_secs(9),
        "接手不應早於租約到期：{acquired:?}"
    );
    assert!(
        kb.lines_with("KRS-I8501")[0].contains("上一個 leader 是 kairos-a"),
        "{}",
        kb.dump()
    );
    assert!(
        poll_until(WAIT, || {
            readyz(b.obs_port, "").is_some_and(|(s, c)| s == 200 && c["cluster"] == "leader")
        }),
        "{}",
        kb.dump()
    );
    p.wait_for_count(PULSE_REGISTERED, 2, FAILOVER_WAIT);
    let reconnected = killed.elapsed();
    eprintln!("PULSE_RECONNECT_SECS={:.2}", reconnected.as_secs_f64());
    let s = wait_registered_to(&pulse, "kairos-b");
    assert_eq!(s["connection"]["candidate_index"], 1, "{s:#}");
    let before = export_reports(&b.kairos.dir).len();
    assert!(
        poll_until(WAIT, || export_reports(&b.kairos.dir).len() >= before + 3),
        "接手後應繼續收到 report\n{}\n{}",
        p.dump(),
        kb.dump()
    );
    let (_, status) = cluster_status(&b.kairos.dir);
    assert_eq!(status["leader"]["node_id"], "kairos-b", "{status:#}");
    assert_eq!(status["leader"]["epoch"], 2, "{status:#}");

    // ---- 原本的節點重新啟動：成為備援（不監聽）----
    let mut ka = spawn_standby(&a);
    assert!(
        poll_until(WAIT, || {
            readyz(a.obs_port, "").is_some_and(|(s, c)| s == 503 && c["cluster"] == "standby")
        }),
        "{}",
        ka.dump()
    );
    assert_eq!(ka.count(KAIROS_STARTED), 0, "{}", ka.dump());

    // ---- F14：step-down → leader 以代碼 75 結束，另一個節點在數秒內接手（不必等 TTL）----
    let stepped = Instant::now();
    let (code, sd) = cli(&a.kairos.dir, &["cluster", "step-down"]);
    assert_eq!(code, 0, "{}", sd.dump());
    assert!(sd.output().contains("kairos-b"), "{}", sd.dump());
    let code = kb.wait_exit(WAIT);
    assert_eq!(
        code,
        75,
        "失去租約的 leader 應以結束代碼 75 結束\n{}",
        kb.dump()
    );
    assert!(kb.output().contains("KRS-W8502"), "{}", kb.dump());
    ka.wait_for(KAIROS_STARTED, FAILOVER_WAIT);
    let handover = stepped.elapsed();
    eprintln!("STEPDOWN_SECS={:.2}", handover.as_secs_f64());
    assert!(
        handover < Duration::from_secs(12),
        "step-down 後的接手不應等到租約到期：{handover:?}\n{}",
        ka.dump()
    );
    p.wait_for_count(PULSE_REGISTERED, 3, FAILOVER_WAIT);
    wait_registered_to(&pulse, "kairos-a");
    let before = export_reports(&a.kairos.dir).len();
    assert!(
        poll_until(WAIT, || export_reports(&a.kairos.dir).len() >= before + 3),
        "{}\n{}",
        p.dump(),
        ka.dump()
    );

    // ---- 收尾：資料完整 ----
    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, _) = ka.stop();
    assert_eq!(code, 0, "{}", ka.dump());
    assert!(ka.output().contains("已交還 leader 租約"), "{}", ka.dump());
    let reports = export_reports(&a.kairos.dir);
    let n = assert_durable_complete(&reports);
    assert!(n >= 9, "三個階段都應有 report：{n}");
    eprintln!("DURABLE_REPORTS={n}（兩次換手，不缺號、不重複）");
    // 稽核雜湊鏈：三次啟動、CLI 的操作，兩個節點寫的是同一條鏈
    let (code, v) = cli(&a.kairos.dir, &["audit", "verify", "--json"]);
    assert_eq!(code, 0, "{}", v.dump());
    let doc: Value = serde_json::from_str(v.stdout().trim()).unwrap();
    assert_eq!(doc["data"]["ok"], true, "{doc}");
    // 全部停止後：沒有 leader（嚴重，結束代碼 9），兩個節點都是 stopped／lost
    let (code, status) = cluster_status(&a.kairos.dir);
    assert_eq!(code, 9, "{status:#}");
    assert!(status["leader"].is_null(), "{status:#}");
    assert_eq!(status["nodes"][0]["role"], "stopped", "{status:#}");
}

/// 建立兩個節點的叢集並啟動：A 是主動節點、B 是備援（金鑰已分發）。回傳（A 的行程, B 的行程）
#[cfg(target_os = "linux")]
fn start_pair(root: &TestDir, a: &Node, b: &Node) -> (Proc, Proc) {
    let (code, p) = cli(&a.kairos.dir, &["cluster", "init"]);
    assert_eq!(code, 0, "{}", p.dump());
    let pw = root.sub("secrets").join("keys.password");
    std::fs::write(&pw, "correct horse battery staple\n").unwrap();
    let bundle = root.sub("secrets").join("cluster.keys");
    for (dir, verb) in [(&a.kairos.dir, "export"), (&b.kairos.dir, "import")] {
        let (code, p) = cli(
            dir,
            &[
                "cluster",
                "keys",
                verb,
                bundle.to_str().unwrap(),
                "--password-file",
                pw.to_str().unwrap(),
            ],
        );
        assert_eq!(code, 0, "{}", p.dump());
    }
    kp_e2e::release_port(a.obs_port);
    let ka = a.kairos.start();
    let mut kb = spawn_standby(b);
    assert!(
        poll_until(WAIT, || {
            readyz(b.obs_port, "").is_some_and(|(s, c)| s == 503 && c["cluster"] == "standby")
        }),
        "{}",
        kb.dump()
    );
    (ka, kb)
}

/// 稽核紀錄的筆數（`kairos audit list --json` 的 `data.total`）
#[cfg(target_os = "linux")]
fn audit_count(dir: &Path) -> u64 {
    let (code, v) = cli(dir, &["audit", "list", "--limit", "1", "--json"]);
    assert_eq!(code, 0, "{}", v.dump());
    let doc: Value = serde_json::from_str(v.stdout().trim()).unwrap();
    doc["data"]["total"]
        .as_u64()
        .unwrap_or_else(|| panic!("audit list --json 沒有 total：{doc}"))
}

/// 第 17 節 F9（腦裂嘗試）、F12（設定不一致）與 F14（排空）：
///
/// 1. 主動節點被凍結（SIGSTOP）超過租約的時間：行程還在、連線沒有斷，但不再續約 → 備援接手；
/// 2. 恢復執行（SIGCONT）後，舊的主動節點**立刻發現已失去租約並以代碼 75 結束**，沒有寫入任何資料
///    （durable 的 report 不缺號、不重複）；
/// 3. 設定不一致的節點記 `KRS-W8505`，`kairos cluster status` 標示；
/// 4. `kairos cluster drain`：備援不再參與選舉；主動節點正常關機並交還租約（結束代碼 0）。
#[cfg(target_os = "linux")]
#[test]
fn a_frozen_leader_is_fenced_and_drain_hands_over() {
    let Some(pg) = test_database("a_frozen_leader_is_fenced_and_drain_hands_over") else {
        return;
    };
    let root = TestDir::new("cluster-freeze");
    let schema = new_schema();
    let a = node(&root, "kairos-a", &pg, &schema, true);
    let b = node(&root, "kairos-b", &pg, &schema, true);
    // B 的一個需要一致的設定與 A 不同（F12）：只警告，照常運作
    let path = b.kairos.dir.join("kairos.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("max_connections = 64\n"), "{text}");
    std::fs::write(
        &path,
        text.replace("max_connections = 64\n", "max_connections = 65\n"),
    )
    .unwrap();
    let (mut ka, mut kb) = start_pair(&root, &a, &b);
    kb.wait_for("KRS-W8505", WAIT);
    let mut status = Value::Null;
    assert!(
        poll_until(WAIT, || {
            status = cluster_status(&a.kairos.dir).1;
            status["nodes"][1]["config_drift"] == true
        }),
        "{status:#}"
    );
    assert_eq!(status["nodes"][0]["config_drift"], false, "{status:#}");

    let pulse = Pulse::new(&root, "pulse", a.kairos.port, GOOD);
    let fallback = format!("127.0.0.1:{}", b.kairos.port);
    let mut p = pulse.start(&[("PULSE__SERVER__FALLBACK_ADDRESSES", &fallback)]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    assert!(
        poll_until(WAIT, || export_reports(&a.kairos.dir).len() >= 3),
        "{}\n{}",
        p.dump(),
        ka.dump()
    );

    // ---- F9：凍結主動節點 ----
    let frozen = Instant::now();
    ka.freeze();
    kb.wait_for("KRS-I8501", FAILOVER_WAIT);
    let acquired = frozen.elapsed();
    kb.wait_for(KAIROS_STARTED, FAILOVER_WAIT);
    eprintln!(
        "FREEZE_FAILOVER_SECS={:.2} LEASE_SECS={:.2}（凍結 → 備援取得租約 → 開始監聽）",
        frozen.elapsed().as_secs_f64(),
        acquired.as_secs_f64()
    );
    assert!(acquired >= Duration::from_secs(9), "{acquired:?}");
    // 舊節點的連線還開著但沒有回應：Pulse 在讀取閒置逾時後改連 B（主位址的握手會逾時）
    wait_registered_to(&pulse, "kairos-b");
    eprintln!(
        "FREEZE_PULSE_RECONNECT_SECS={:.2}",
        frozen.elapsed().as_secs_f64()
    );
    let before = export_reports(&b.kairos.dir).len();
    assert!(
        poll_until(WAIT, || export_reports(&b.kairos.dir).len() >= before + 3),
        "{}\n{}",
        p.dump(),
        kb.dump()
    );
    let (_, status) = cluster_status(&b.kairos.dir);
    assert_eq!(status["leader"]["node_id"], "kairos-b", "{status:#}");
    assert_eq!(status["leader"]["epoch"], 2, "{status:#}");
    let audit_before = audit_count(&b.kairos.dir);

    // ---- 恢復執行：舊的主動節點立刻以代碼 75 結束，沒有留下任何寫入 ----
    let thawed = Instant::now();
    ka.thaw();
    let code = ka.wait_exit(WAIT);
    eprintln!("FENCED_EXIT_SECS={:.2}", thawed.elapsed().as_secs_f64());
    assert_eq!(
        code,
        75,
        "恢復後的舊 leader 應以結束代碼 75 結束\n{}",
        ka.dump()
    );
    assert!(ka.output().contains("KRS-W8502"), "{}", ka.dump());
    assert!(
        thawed.elapsed() < Duration::from_secs(5),
        "舊 leader 應在恢復後立刻結束：{:?}",
        thawed.elapsed()
    );
    // 舊節點沒有寫入：leader 仍是 B（epoch 2），稽核鏈沒有多出舊節點的紀錄（它沒有走關機流程）
    let (_, status) = cluster_status(&b.kairos.dir);
    assert_eq!(status["leader"]["node_id"], "kairos-b", "{status:#}");
    assert_eq!(status["leader"]["epoch"], 2, "{status:#}");
    assert_eq!(audit_count(&b.kairos.dir), audit_before);

    // ---- F14：排空 ----
    // 舊節點重新啟動為備援，排空它：不再參與選舉
    let mut ka = spawn_standby(&a);
    assert!(
        poll_until(WAIT, || {
            readyz(a.obs_port, "").is_some_and(|(_, c)| c["cluster"] == "standby")
        }),
        "{}",
        ka.dump()
    );
    let (code, d) = cli(&b.kairos.dir, &["cluster", "drain", "kairos-a"]);
    assert_eq!(code, 0, "{}", d.dump());
    ka.wait_for("KRS-I8507", WAIT);
    assert!(
        poll_until(WAIT, || {
            readyz(a.obs_port, "").is_some_and(|(s, c)| s == 503 && c["cluster"] == "draining")
        }),
        "{}",
        ka.dump()
    );
    // 排空主動節點：正常關機（先對 Pulse 送 drain）、交還租約、結束代碼 0
    let (code, d) = cli(&a.kairos.dir, &["cluster", "drain", "kairos-b"]);
    assert_eq!(code, 0, "{}", d.dump());
    let code = kb.wait_exit(WAIT);
    assert_eq!(code, 0, "被排空的 leader 應正常結束\n{}", kb.dump());
    assert!(
        kb.output().contains("已交還 leader 租約") && kb.output().contains("KRS-I8507"),
        "{}",
        kb.dump()
    );
    // 另一個節點在排空中：不接手（租約已交還，沒有 leader）
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(
        ka.count("KRS-I8501"),
        0,
        "排空中的節點不應取得租約\n{}",
        ka.dump()
    );
    let (code, status) = cluster_status(&a.kairos.dir);
    assert_eq!(code, 9, "{status:#}");
    assert!(status["leader"].is_null(), "{status:#}");
    assert_eq!(status["nodes"][0]["role"], "draining", "{status:#}");
    assert_eq!(status["nodes"][1]["role"], "stopped", "{status:#}");
    // 已停止的節點可以 forget；仍在執行的不行
    let (code, f) = cli(&a.kairos.dir, &["cluster", "forget", "kairos-a"]);
    assert_eq!(code, 11, "{}", f.dump());
    let (code, f) = cli(&a.kairos.dir, &["cluster", "forget", "kairos-b"]);
    assert_eq!(code, 0, "{}", f.dump());

    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, _) = ka.stop();
    assert_eq!(code, 0, "{}", ka.dump());
    let n = assert_durable_complete(&export_reports(&a.kairos.dir));
    eprintln!("DURABLE_REPORTS={n}（凍結的 leader 恢復後被擋下，不缺號、不重複）");
    let (code, v) = cli(&a.kairos.dir, &["audit", "verify", "--json"]);
    assert_eq!(code, 0, "{}", v.dump());
}

/// 以 `docker <動作> <容器>` 控制測試用的 PostgreSQL 容器
#[cfg(target_os = "linux")]
fn docker(action: &str, container: &str) {
    let out = std::process::Command::new("docker")
        .args([action, container])
        .output()
        .unwrap_or_else(|e| panic!("無法執行 docker {action} {container}：{e}"));
    assert!(
        out.status.success(),
        "docker {action} {container} 失敗：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 第 17 節 F6：PostgreSQL 完全停止後恢復。
///
/// 會**停止並重新啟動** `KAIROS_TEST_PG_CONTAINER` 指定的 docker 容器（`KAIROS_TEST_PG_URL` 必須指向它），
/// 所以預設不執行，也不能與其他使用同一個資料庫的測試同時執行：
///
/// ```text
/// KAIROS_TEST_PG_URL=… KAIROS_TEST_PG_CONTAINER=<容器名稱> \
///   cargo test -p kp-e2e --test cluster -- --ignored --nocapture database_outage
/// ```
///
/// 1. 資料庫停止 → 主動節點續約失敗，在 `lease_ttl_secs − 5` 秒後以代碼 75 結束；備援節點不結束
///    （`/readyz` 的 `cluster` 為 `lost`，記 `KRS-E4401`）；
/// 2. 被重新啟動的節點同樣等待，不會因為連不上資料庫而以設定錯誤結束；
/// 3. 資料庫恢復 → 其中一個節點取得租約並開始監聽，Pulse 重連並補送，資料不缺號、不重複。
#[cfg(target_os = "linux")]
#[test]
#[ignore = "會停止並重新啟動 KAIROS_TEST_PG_CONTAINER 指定的 PostgreSQL 容器；必須單獨執行"]
fn database_outage_makes_nodes_wait_and_recover() {
    let Some(pg) = test_database("database_outage_makes_nodes_wait_and_recover") else {
        return;
    };
    let container = std::env::var("KAIROS_TEST_PG_CONTAINER")
        .expect("這個測試需要 KAIROS_TEST_PG_CONTAINER（要停止與啟動的 docker 容器名稱）");
    let root = TestDir::new("cluster-dbdown");
    let schema = new_schema();
    let a = node(&root, "kairos-a", &pg, &schema, true);
    let b = node(&root, "kairos-b", &pg, &schema, true);
    let (mut ka, mut kb) = start_pair(&root, &a, &b);
    let pulse = Pulse::new(&root, "pulse", a.kairos.port, GOOD);
    let fallback = format!("127.0.0.1:{}", b.kairos.port);
    let mut p = pulse.start(&[("PULSE__SERVER__FALLBACK_ADDRESSES", &fallback)]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    assert!(
        poll_until(WAIT, || export_reports(&a.kairos.dir).len() >= 3),
        "{}\n{}",
        p.dump(),
        ka.dump()
    );

    // ---- 資料庫停止 ----
    let stopped = Instant::now();
    docker("stop", &container);
    let code = ka.wait_exit(FAILOVER_WAIT);
    eprintln!(
        "DB_DOWN_LEADER_EXIT_SECS={:.2}（docker stop 送出 → 主動節點結束；含 docker stop 本身的時間）",
        stopped.elapsed().as_secs_f64()
    );
    assert_eq!(code, 75, "{}", ka.dump());
    assert!(ka.output().contains("KRS-W8502"), "{}", ka.dump());
    // 備援不結束：等待資料庫
    kb.wait_for("KRS-E4401", FAILOVER_WAIT);
    assert!(
        poll_until(WAIT, || {
            readyz(b.obs_port, "").is_some_and(|(s, c)| s == 503 && c["cluster"] == "lost")
        }),
        "{}",
        kb.dump()
    );
    // 被服務管理員重新啟動的節點：連不上資料庫也不結束
    let mut ka = spawn_standby(&a);
    ka.wait_for("KRS-E4401", WAIT);
    std::thread::sleep(Duration::from_secs(5));
    assert!(ka.is_running(), "{}", ka.dump());
    assert!(kb.is_running(), "{}", kb.dump());
    assert_eq!(
        ka.count(KAIROS_STARTED) + kb.count(KAIROS_STARTED),
        0,
        "資料庫停止期間沒有節點服務"
    );
    assert_eq!(http_get(a.obs_port, "/healthz").map(|r| r.0), Some(200));

    // ---- 資料庫恢復 ----
    let started = Instant::now();
    docker("start", &container);
    assert!(
        poll_until(FAILOVER_WAIT, || {
            ka.count(KAIROS_STARTED) + kb.count(KAIROS_STARTED) >= 1
        }),
        "資料庫恢復後應有一個節點取得租約並開始監聽\n{}\n{}",
        ka.dump(),
        kb.dump()
    );
    eprintln!(
        "DB_UP_RECOVERY_SECS={:.2}（docker start 送出 → 某個節點開始監聽）",
        started.elapsed().as_secs_f64()
    );
    assert_eq!(
        ka.count(KAIROS_STARTED) + kb.count(KAIROS_STARTED),
        1,
        "只有一個節點服務\n{}\n{}",
        ka.dump(),
        kb.dump()
    );
    p.wait_for_count(PULSE_REGISTERED, 2, FAILOVER_WAIT);
    eprintln!(
        "DB_UP_PULSE_RECONNECT_SECS={:.2}",
        started.elapsed().as_secs_f64()
    );
    let before = export_reports(&a.kairos.dir).len();
    assert!(
        poll_until(WAIT, || export_reports(&a.kairos.dir).len() >= before + 3),
        "{}",
        p.dump()
    );
    let (_, status) = cluster_status(&a.kairos.dir);
    assert_eq!(status["leader"]["epoch"], 2, "{status:#}");

    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, _) = ka.stop();
    assert_eq!(code, 0, "{}", ka.dump());
    let (code, _) = kb.stop();
    assert_eq!(code, 0, "{}", kb.dump());
    let n = assert_durable_complete(&export_reports(&a.kairos.dir));
    eprintln!("DURABLE_REPORTS={n}（資料庫停止又恢復，不缺號、不重複）");
    let (code, v) = cli(&a.kairos.dir, &["audit", "verify", "--json"]);
    assert_eq!(code, 0, "{}", v.dump());
}

#[test]
fn a_second_solo_kairos_is_refused() {
    let Some(pg) = test_database("a_second_solo_kairos_is_refused") else {
        return;
    };
    let root = TestDir::new("cluster-solo");
    let schema = new_schema();
    let first = node(&root, "first", &pg, &schema, false);
    let second = node(&root, "second", &pg, &schema, false);
    let mut k1 = first.kairos.start();
    // 第二個單機模式的 Kairos 指到同一個 schema：拒絕啟動（設定錯誤，不重試）
    second.kairos.release_ports();
    let (code, p) = run_to_exit(
        "kairos",
        &second.kairos.dir,
        &["--config", "kairos.toml"],
        &[],
    );
    assert_eq!(code, 3, "{}", p.dump());
    assert!(
        p.output().contains("KRS-E4402") && p.output().contains("已有另一個 Kairos"),
        "{}",
        p.dump()
    );
    // 叢集模式的節點也不能加入正被單機模式使用的 schema
    let third = node(&root, "third", &pg, &schema, true);
    third.kairos.release_ports();
    kp_e2e::release_port(third.obs_port);
    let (code, p) = run_to_exit(
        "kairos",
        &third.kairos.dir,
        &["--config", "kairos.toml"],
        &[],
    );
    assert_eq!(code, 3, "{}", p.dump());
    assert!(p.output().contains("KRS-E4402"), "{}", p.dump());
    // CLI 不受影響
    let (code, p) = cli(&second.kairos.dir, &["cluster", "status"]);
    assert_eq!(code, 0, "{}", p.dump());
    assert!(p.output().contains("未啟用"), "{}", p.dump());
    // 第一個結束之後可以啟動
    let (code, _) = k1.stop();
    assert_eq!(code, 0, "{}", k1.dump());
    let mut k2 = second.kairos.start();
    let (code, _) = k2.stop();
    assert_eq!(code, 0, "{}", k2.dump());
}
