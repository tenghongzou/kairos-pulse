//! 端對端：Pulse 的多個伺服器位址（`server.fallback_addresses`）與排空（feature `cluster`；
//! `kairos/HA.md` 第 8 節、PROTOCOL.md「叢集與排空」）。
//!
//! - 主位址的 Kairos 被強制終止（行程當掉）：Pulse 改連備用位址，durable 的資料不缺號；同一個 Kairos
//!   之內不重複。
//! - 主位址的 Kairos 優雅關機：先送 `drain`，Pulse 依隨機延遲改連下一個候選位址，不算連線失敗。
//!
//! 兩個 Kairos 各自使用自己的 SQLite（0.9 還沒有共用資料庫的叢集模式），因此換節點時重送的在途
//! report（最多 4 筆）可能兩邊各有一筆；共用 PostgreSQL 的叢集由全叢集的唯一索引去重（HA.md 第 7.2 節）。

use std::collections::BTreeSet;

use kp_e2e::{GOOD, Kairos, PULSE_REGISTERED, Pulse, TestDir, WAIT, poll_until, test_pki};
use serde_json::Value;

/// Pulse 同一時間最多在途的 durable report 筆數（`pulse/src/network.rs` 的 `DURABLE_IN_FLIGHT`）
const MAX_IN_FLIGHT: usize = 4;

/// 一個 Kairos 節點：`[cluster] node_id` 設為 `name`
fn node(root: &TestDir, name: &str) -> Kairos {
    let k = Kairos::named(root, name, test_pki::CRL_EMPTY);
    let path = k.dir.join("kairos.toml");
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str(&format!("\n[cluster]\nnode_id = \"{name}\"\n"));
    std::fs::write(&path, toml).unwrap();
    k
}

/// 某個 Kairos 保存的 durable report：所有的 boot_id 與 report_seq（依寫入順序）
fn durable_seqs(reports: &[Value]) -> (BTreeSet<String>, Vec<u64>) {
    let mut boots = BTreeSet::new();
    let mut seqs = Vec::new();
    for r in reports
        .iter()
        .filter(|r| r["agent_id"] == test_pki::GOOD_ID)
    {
        boots.insert(
            r["boot_id"]
                .as_str()
                .unwrap_or_else(|| panic!("協商了 durable 的 report 應有 boot_id：{r}"))
                .to_string(),
        );
        seqs.push(r["report_seq"].as_u64().unwrap());
    }
    (boots, seqs)
}

/// 兩個節點的資料合起來不缺號（1..=n），各自之內不重複，重疊（換節點時重送的在途 report）不超過在途上限
fn assert_no_gaps_or_duplicates(a: &[Value], b: &[Value]) {
    let (boots_a, seqs_a) = durable_seqs(a);
    let (boots_b, seqs_b) = durable_seqs(b);
    let boots: BTreeSet<_> = boots_a.union(&boots_b).collect();
    assert_eq!(
        boots.len(),
        1,
        "Pulse 只啟動一次，應只有一個 boot_id：{boots:?}"
    );
    for (name, seqs) in [("A", &seqs_a), ("B", &seqs_b)] {
        let unique: BTreeSet<_> = seqs.iter().collect();
        assert_eq!(
            unique.len(),
            seqs.len(),
            "{name} 有重複的 report_seq：{seqs:?}"
        );
    }
    assert!(
        !seqs_a.is_empty() && !seqs_b.is_empty(),
        "{seqs_a:?} {seqs_b:?}"
    );
    let set_a: BTreeSet<u64> = seqs_a.iter().copied().collect();
    let set_b: BTreeSet<u64> = seqs_b.iter().copied().collect();
    let all: Vec<u64> = set_a.union(&set_b).copied().collect();
    let expected: Vec<u64> = (1..=all.len() as u64).collect();
    assert_eq!(all, expected, "合起來應不缺號：A={seqs_a:?} B={seqs_b:?}");
    let overlap = set_a.intersection(&set_b).count();
    assert!(
        overlap <= MAX_IN_FLIGHT,
        "重疊 {overlap} 筆，超過在途上限：A={seqs_a:?} B={seqs_b:?}"
    );
}

/// Pulse 的本機狀態檔
fn status_file(pulse: &Pulse) -> Value {
    let path = pulse.dir.join("state").join("status.json");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// 等到狀態檔記錄「已註冊到 `server`」
fn wait_registered_to(pulse: &Pulse, server: &str) -> Value {
    let mut last = Value::Null;
    let ok = poll_until(WAIT, || {
        last = status_file(pulse);
        last["connection"]["state"] == "registered" && last["connection"]["server"] == server
    });
    assert!(ok, "狀態檔應記錄已註冊到 {server}：{last:#}");
    last
}

#[test]
fn killing_the_primary_fails_over_and_keeps_durable_data() {
    let root = TestDir::new("ha-failover");
    let a = node(&root, "kairos-a");
    let b = node(&root, "kairos-b");
    let pulse = Pulse::new(&root, "pulse", a.port, GOOD);
    let primary = format!("127.0.0.1:{}", a.port);
    let fallback = format!("127.0.0.1:{}", b.port);

    let mut ka = a.start();
    let mut kb = b.start();
    let mut p = pulse.start(&[("PULSE__SERVER__FALLBACK_ADDRESSES", &fallback)]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    let first = p.lines_with(PULSE_REGISTERED);
    assert!(
        first[0].contains("節點 kairos-a") && first[0].contains(&primary),
        "{first:?}"
    );
    let s = wait_registered_to(&pulse, &primary);
    assert_eq!(s["connection"]["server_node"], "kairos-a", "{s:#}");
    assert_eq!(s["connection"]["candidates"], 2, "{s:#}");
    assert_eq!(s["connection"]["candidate_index"], 0, "{s:#}");
    assert!(
        s["connection"]["features"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("cluster")),
        "{s:#}"
    );
    assert!(
        poll_until(WAIT, || a.reports().len() >= 3),
        "{}\n{}",
        p.dump(),
        ka.dump()
    );

    // 主位址的 Kairos 當掉：Pulse 斷線後從主位址重試（拒絕連線）→ 立即改試備用位址
    ka.kill();
    p.wait_for_count(PULSE_REGISTERED, 2, WAIT);
    p.wait_for("conn.failover PLS-W2301", WAIT);
    let second = p.lines_with(PULSE_REGISTERED);
    assert!(
        second[1].contains("節點 kairos-b") && second[1].contains(&fallback),
        "{second:?}"
    );
    let s = wait_registered_to(&pulse, &fallback);
    assert_eq!(s["connection"]["server_node"], "kairos-b", "{s:#}");
    assert_eq!(s["connection"]["candidate_index"], 1, "{s:#}");
    // 停機期間的取樣補送到備用節點，之後的取樣照常送出
    let before = b.reports().len();
    assert!(
        poll_until(WAIT, || b.reports().len() >= before + 3),
        "{}\n{}",
        p.dump(),
        kb.dump()
    );
    assert!(
        poll_until(WAIT, || p.count("已補送完本機緩衝中的積壓") >= 1),
        "{}",
        p.dump()
    );

    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, _) = kb.stop();
    assert_eq!(code, 0, "{}", kb.dump());
    assert_no_gaps_or_duplicates(&a.reports(), &b.reports());
    assert!(!p.stderr().contains("[ERROR]"), "{}", p.dump());
}

#[test]
fn graceful_shutdown_drains_agents_to_the_next_node() {
    let root = TestDir::new("ha-drain");
    let a = node(&root, "kairos-a");
    let b = node(&root, "kairos-b");
    let pulse = Pulse::new(&root, "pulse", a.port, GOOD);
    let primary = format!("127.0.0.1:{}", a.port);
    let fallback = format!("127.0.0.1:{}", b.port);

    let mut ka = a.start();
    let mut kb = b.start();
    let mut p = pulse.start(&[("PULSE__SERVER__FALLBACK_ADDRESSES", &fallback)]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    wait_registered_to(&pulse, &primary);
    assert!(
        poll_until(WAIT, || a.reports().len() >= 2),
        "{}\n{}",
        p.dump(),
        ka.dump()
    );

    // 主位址的 Kairos 優雅關機：先對協商了 cluster 的連線送 drain
    let (code, _) = ka.stop();
    assert_eq!(code, 0, "{}", ka.dump());
    assert!(
        ka.stdout().contains("KRS-I8507")
            && ka
                .stdout()
                .contains("已對 1 條協商了 cluster 的連線送出 drain"),
        "{}",
        ka.dump()
    );
    p.wait_for("conn.drain PLS-I8507", WAIT);
    // 依 reconnect_within_ms（10 秒內的隨機延遲）改連下一個候選位址
    p.wait_for_count(PULSE_REGISTERED, 2, WAIT);
    let s = wait_registered_to(&pulse, &fallback);
    assert_eq!(s["connection"]["server_node"], "kairos-b", "{s:#}");
    // 排空不算連線失敗：沒有「連線中斷」，也沒有改試其他位址
    assert_eq!(p.count("連線中斷"), 0, "{}", p.dump());
    assert_eq!(p.count("conn.failover"), 0, "{}", p.dump());
    let before = b.reports().len();
    assert!(
        poll_until(WAIT, || b.reports().len() >= before + 2),
        "{}\n{}",
        p.dump(),
        kb.dump()
    );

    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, _) = kb.stop();
    assert_eq!(code, 0, "{}", kb.dump());
    assert_no_gaps_or_duplicates(&a.reports(), &b.reports());
    assert!(!p.stderr().contains("[ERROR]"), "{}", p.dump());
}
