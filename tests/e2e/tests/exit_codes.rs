//! 端對端：以實際的執行檔檢查常駐模式啟動時的結束代碼（設計包 `cli-style.md` §8、
//! `pulse/cli-spec.md` §1.3）。
//!
//! | 代碼 | 意義 | Kairos | Pulse |
//! |---|---|---|---|
//! | 0 | 成功 | `--help`、`--version`、優雅關機 | 同左 |
//! | 1 | 一般錯誤 | 日誌目錄無法建立等 | 同左 |
//! | 2 | 用法錯誤 | 未知參數、`--config` 缺少路徑 | 未知參數 |
//! | 3 | 設定錯誤 | 設定檔語法、範圍錯誤、找不到 `--config`、S1 拒絕啟動 | `pulse.toml`／環境變數錯誤、無效的 `server.domain` |
//! | 4 | 憑證錯誤 | 伺服器憑證、私鑰、client CA、CRL | `ca_cert`、客戶端憑證、私鑰、SAN 不符、`agent.id` |
//! | 11 | 衝突 | 連接埠被佔用 | （不適用） |
//!
//! 優雅關機的 0 見 `session.rs`。這些情境都在建立連線之前結束，不需要另一端。

use std::path::Path;

use kp_e2e::{GOOD, Kairos, Pulse, REVOKED, TestDir, run_to_exit, test_pki};

/// 執行 `name` 並檢查結束代碼與輸出（stdout + stderr）中的文字
fn expect_exit(
    name: &str,
    cwd: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    code: i32,
    needle: &str,
) {
    let (actual, p) = run_to_exit(name, cwd, args, envs);
    let output = p.output();
    assert_eq!(
        actual, code,
        "{name} {args:?} {envs:?} 的結束代碼應為 {code}\n{output}"
    );
    assert!(
        output.contains(needle),
        "{name} {args:?} {envs:?} 的輸出應含「{needle}」\n{output}"
    );
}

fn kairos_exit(k: &Kairos, args: &[&str], envs: &[(&str, &str)], code: i32, needle: &str) {
    expect_exit("kairos", &k.dir, args, envs, code, needle);
}

fn pulse_exit(p: &Pulse, envs: &[(&str, &str)], code: i32, needle: &str) {
    expect_exit("pulse", &p.dir, &[], envs, code, needle);
}

const CONFIG: [&str; 2] = ["--config", "kairos.toml"];

#[test]
fn help_and_version_exit_0() {
    let root = TestDir::new("codes-0");
    let k = Kairos::new(&root, test_pki::CRL_EMPTY);
    let p = Pulse::new(&root, "pulse", k.port, GOOD);
    kairos_exit(&k, &["--help"], &[], 0, "用法");
    kairos_exit(&k, &["--version"], &[], 0, "kairos ");
    expect_exit("pulse", &p.dir, &["--help"], &[], 0, "用法");
    expect_exit("pulse", &p.dir, &["--version"], &[], 0, "pulse ");
}

#[test]
fn usage_errors_exit_2() {
    let root = TestDir::new("codes-2");
    let k = Kairos::new(&root, test_pki::CRL_EMPTY);
    let p = Pulse::new(&root, "pulse", k.port, GOOD);
    kairos_exit(&k, &["--bogus"], &[], 2, "無法辨識的參數");
    kairos_exit(&k, &["--config"], &[], 2, "--config 需要指定設定檔路徑");
    kairos_exit(
        &k,
        &["--config", "a.toml", "--config", "b.toml"],
        &[],
        2,
        "--config 只能指定一次",
    );
    expect_exit("pulse", &p.dir, &["--bogus"], &[], 2, "未知的參數");
}

#[test]
fn config_errors_exit_3() {
    let root = TestDir::new("codes-3");
    let k = Kairos::new(&root, test_pki::CRL_EMPTY);
    // 範圍錯誤（環境變數覆寫設定檔）
    kairos_exit(
        &k,
        &CONFIG,
        &[("KAIROS__TIMEOUTS__WRITE_SECS", "10")],
        3,
        "write_secs",
    );
    // 明確指定的設定檔不存在
    kairos_exit(&k, &["--config", "missing.toml"], &[], 3, "missing.toml");
    // S1：未啟用 mTLS 又監聽非 loopback 位址
    let s1 = Kairos::named(&root, "kairos-s1", test_pki::CRL_EMPTY);
    std::fs::write(
        s1.dir.join("kairos.toml"),
        "[server]\nlisten = \"0.0.0.0:0\"\ncert_path = \"server.pem\"\nkey_path = \"server.key\"\n",
    )
    .unwrap();
    kairos_exit(&s1, &CONFIG, &[], 3, "auth.allow_unauthenticated");
    // 設定檔語法錯誤
    let broken = Kairos::named(&root, "kairos-broken", test_pki::CRL_EMPTY);
    std::fs::write(broken.dir.join("kairos.toml"), "[server\nlisten = 1\n").unwrap();
    kairos_exit(&broken, &CONFIG, &[], 3, "kairos.toml");

    let p = Pulse::new(&root, "pulse", k.port, GOOD);
    pulse_exit(
        &p,
        &[("PULSE__HEARTBEAT__INTERVAL", "0")],
        3,
        "heartbeat.interval",
    );
    pulse_exit(
        &p,
        &[("PULSE__SERVER__DOMAIN", "not a domain")],
        3,
        "server.domain",
    );
    let broken = Pulse::new(&root, "pulse-broken", k.port, GOOD);
    std::fs::write(broken.dir.join("pulse.toml"), "[server\nport = \"x\"\n").unwrap();
    pulse_exit(&broken, &[], 3, "無法載入設定");
}

#[test]
fn certificate_errors_exit_4() {
    let root = TestDir::new("codes-4");

    // Kairos：伺服器憑證、私鑰、client CA、CRL
    let k = Kairos::named(&root, "kairos-cert", test_pki::CRL_EMPTY);
    std::fs::remove_file(k.dir.join("server.pem")).unwrap();
    kairos_exit(&k, &CONFIG, &[], 4, "server.pem");

    let k = Kairos::named(&root, "kairos-key", test_pki::CRL_EMPTY);
    std::fs::write(k.dir.join("server.key"), test_pki::GOOD_KEY).unwrap();
    kairos_exit(&k, &CONFIG, &[], 4, "Kairos 啟動失敗");

    let k = Kairos::named(&root, "kairos-ca", test_pki::CRL_EMPTY);
    std::fs::write(k.dir.join("client-ca.pem"), "not a pem file\n").unwrap();
    kairos_exit(&k, &CONFIG, &[], 4, "client-ca.pem");

    let k = Kairos::named(&root, "kairos-crl", test_pki::CRL_EMPTY);
    std::fs::remove_file(k.dir.join("agents.crl")).unwrap();
    kairos_exit(&k, &CONFIG, &[], 4, "agents.crl");

    // Pulse：信任的 CA、客戶端憑證、私鑰、SAN 與 agent_id、agent.id 內容
    let port = k.port;
    let p = Pulse::new(&root, "pulse-ca", port, GOOD);
    std::fs::write(p.dir.join("server-ca.pem"), "not a pem file\n").unwrap();
    pulse_exit(&p, &[], 4, "server.ca_cert");

    let p = Pulse::new(&root, "pulse-cert", port, GOOD);
    std::fs::remove_file(p.dir.join("client.pem")).unwrap();
    pulse_exit(&p, &[], 4, "client.pem");

    let p = Pulse::new(&root, "pulse-key", port, GOOD);
    std::fs::write(p.dir.join("client.key"), REVOKED.key).unwrap();
    pulse_exit(&p, &[], 4, "啟動失敗");

    let p = Pulse::new(&root, "pulse-san", port, GOOD);
    std::fs::write(p.dir.join("agent.id"), format!("{}\n", REVOKED.agent_id)).unwrap();
    pulse_exit(&p, &[], 4, REVOKED.agent_id);

    let p = Pulse::new(&root, "pulse-id", port, GOOD);
    std::fs::write(p.dir.join("agent.id"), "not-a-uuid\n").unwrap();
    pulse_exit(&p, &[], 4, "agent.id");

    // 以非 0 結束的原因另寫一行到 stderr（pulse.service 的 stdout 不進 journal）
    let (code, p) = run_to_exit("pulse", &p.dir, &[], &[]);
    assert_eq!(code, 4);
    assert!(
        p.stderr().contains("[ERROR]") && p.stderr().contains("agent.id"),
        "{}",
        p.dump()
    );
}

#[test]
fn port_in_use_exits_11() {
    let root = TestDir::new("codes-11");
    let k = Kairos::new(&root, test_pki::CRL_EMPTY);
    let _occupied = std::net::TcpListener::bind(("127.0.0.1", k.port)).unwrap();
    kairos_exit(&k, &CONFIG, &[], 11, "無法監聽");
}

#[test]
fn other_startup_failures_exit_1() {
    // 日誌目錄無法建立（工作目錄中已有同名的檔案）：未分類的錯誤，交由服務管理員重試
    let root = TestDir::new("codes-1");
    let k = Kairos::new(&root, test_pki::CRL_EMPTY);
    std::fs::write(k.dir.join("logs"), b"not a directory").unwrap();
    kairos_exit(&k, &CONFIG, &[], 1, "logs");

    let p = Pulse::new(&root, "pulse", k.port, GOOD);
    std::fs::write(p.dir.join("logs"), b"not a directory").unwrap();
    pulse_exit(&p, &[], 1, "無法初始化日誌");
}
