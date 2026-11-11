//! 端對端：`kairos releases import|list|delete`（REMOTE.md 第 8.2 節）以實際的執行檔操作版本庫——Kairos
//! 啟動前先匯入（離線環境）、執行中補上發佈檔，主控台 API 看到同一份版本庫；版本庫的檔案遺失時系統健康的
//! 「遠端管理」為警告（`KRS-W8417`〔暫定〕），重新匯入後恢復；稽核雜湊鏈有 `cli:` 的 `release.*` 事件。
//! 發佈為 `kairos-proto` 的測試向量（`crates/kairos-proto/testdata/release/`）。

use kp_e2e::{
    KAIROS_STARTED, Kairos, TestDir, run_to_exit, start_kairos, test_pki, workspace_root,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

const VERSION: &str = "0.9.1";
const DEB: &str = "pulse-0.9.1-5e6f7a8-linux-x86_64.deb";
const TAR: &str = "pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz";

fn vectors() -> PathBuf {
    workspace_root().join("crates/kairos-proto/testdata/release")
}

/// 把測試向量的發佈複製到 `dir`（`files` 為要一起複製的發佈檔）
fn bundle(dir: &Path, files: &[&str]) {
    std::fs::create_dir_all(dir).unwrap();
    for f in [
        "SHA256SUMS",
        "SHA256SUMS.sig",
        "pulse-0.9.1-5e6f7a8-release.json",
    ]
    .iter()
    .chain(files)
    {
        std::fs::copy(vectors().join(f), dir.join(f)).unwrap();
    }
}

fn remote_item(health: &Value) -> Value {
    health["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["id"] == "remote")
        .cloned()
        .expect("remote 項目")
}

#[test]
fn releases_cli_imports_lists_and_deletes_alongside_a_running_kairos() {
    let root = TestDir::new("releases-cli");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let toml = kairos.dir.join("kairos.toml");
    let mut cfg = std::fs::read_to_string(&toml).unwrap();
    cfg.push_str(&format!(
        "\n[remote]\nallowed_signers = {:?}\n",
        vectors().join("allowed_signers").to_string_lossy()
    ));
    std::fs::write(&toml, cfg).unwrap();
    let cli = |args: &[&str]| {
        let mut all = vec!["releases"];
        all.extend(args);
        all.extend(["--config", "kairos.toml"]);
        run_to_exit("kairos", &kairos.dir, &all, &[])
    };
    let json = |p: &kp_e2e::Proc| -> Value {
        assert!(p.stdout().is_ascii(), "{}", p.dump());
        serde_json::from_str(p.stdout().trim())
            .unwrap_or_else(|e| panic!("不是 JSON（{e}）：{}", p.dump()))
    };

    // 用法錯誤與說明
    let (code, p) = cli(&["frobnicate"]);
    assert_eq!(code, 2, "{}", p.dump());
    assert!(
        p.stderr().contains("kairos releases import"),
        "{}",
        p.dump()
    );
    let (code, p) = cli(&["--help"]);
    assert_eq!(code, 0);
    assert!(
        p.stdout().contains("kairos releases delete"),
        "{}",
        p.dump()
    );

    // Kairos 還沒啟動（離線環境先匯入）：只有 deb
    let first = root.sub("first");
    bundle(&first, &[DEB]);
    let (code, p) = cli(&["import", first.to_str().unwrap(), "--json"]);
    assert_eq!(code, 0, "{}", p.dump());
    let v = json(&p);
    assert_eq!(v["schema"], "kairos.releases.import/v1", "{v}");
    assert_eq!(
        (
            v["data"]["version"].as_str(),
            v["data"]["created"].as_bool(),
            v["data"]["verified"].as_bool()
        ),
        (Some(VERSION), Some(true), Some(true)),
        "{v}"
    );
    assert_eq!(v["data"]["stored"], serde_json::json!([DEB]));
    assert_eq!(v["data"]["not_in_directory"], serde_json::json!([TAR]));

    // 被改過的發佈檔：拒絕（KRS-E8413），版本庫不變
    let bad = root.sub("bad");
    bundle(&bad, &[TAR]);
    std::fs::write(bad.join(TAR), b"tampered").unwrap();
    let (code, p) = cli(&["import", bad.to_str().unwrap()]);
    assert_eq!(code, 1, "{}", p.dump());
    assert!(p.stderr().contains("KRS-E8413"), "{}", p.dump());

    let mut k = start_kairos(&kairos.dir, &[], &kairos.ports(), &[KAIROS_STARTED]);
    let token = {
        let (code, p) = run_to_exit(
            "kairos",
            &kairos.dir,
            &[
                "api-token",
                "create",
                "--name",
                "admin",
                "--role",
                "admin",
                "--config",
                "kairos.toml",
                "--json",
            ],
            &[],
        );
        assert_eq!(code, 0, "{}", p.dump());
        json(&p)["data"]["token"].as_str().unwrap().to_string()
    };
    let api = kairos.api(&token);

    // 主控台 API 看到 CLI 匯入的版本
    let list = api.data("/api/v1/releases");
    assert_eq!(list[0]["version"], VERSION, "{list}");
    assert!(
        list[0]["imported_by"]
            .as_str()
            .is_some_and(|b| b.starts_with("cli:")),
        "{list}"
    );
    let stored = |v: &Value, name: &str| -> bool {
        v["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["filename"] == name)
            .unwrap()["stored"]
            .as_bool()
            .unwrap()
    };
    assert!(stored(&list[0], DEB) && !stored(&list[0], TAR), "{list}");

    // Kairos 執行中補上 tar.gz
    let all = root.sub("all");
    bundle(&all, &[DEB, TAR]);
    let (code, p) = cli(&["import", all.to_str().unwrap()]);
    assert_eq!(code, 0, "{}", p.dump());
    assert!(p.stdout().contains("已補上 Pulse 0.9.1"), "{}", p.dump());
    assert!(p.stdout().contains(TAR), "{}", p.dump());
    let list = api.data("/api/v1/releases");
    assert!(stored(&list[0], DEB) && stored(&list[0], TAR), "{list}");
    let health = remote_item(&api.data("/api/v1/system/health"));
    assert_eq!(health["status"], "ok", "{health}");
    assert_eq!(health["values"]["release_files_missing"], 0, "{health}");

    // 版本庫的檔案遺失（例如還原備份之後）：系統健康警告、list 標出；重新匯入後恢復
    let (code, p) = cli(&["list", "--json"]);
    assert_eq!(code, 0, "{}", p.dump());
    let v = json(&p);
    assert_eq!(v["schema"], "kairos.releases.list/v1", "{v}");
    let arts = v["data"]["releases"][0]["artifacts"]
        .as_array()
        .unwrap()
        .clone();
    let deb_sha = arts.iter().find(|a| a["filename"] == DEB).unwrap()["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    let store = kairos.dir.join("data").join("releases");
    std::fs::remove_file(store.join(&deb_sha)).expect("版本庫的 deb");
    let health = remote_item(&api.data("/api/v1/system/health"));
    assert_eq!(
        (health["status"].as_str(), health["product_code"].as_str()),
        (Some("warning"), Some("KRS-W8417")),
        "{health}"
    );
    assert!(
        health["summary"]
            .as_str()
            .unwrap()
            .contains(&format!("版本庫的檔案遺失：{VERSION} 的 {DEB}")),
        "{health}"
    );
    let (code, p) = cli(&["list"]);
    assert_eq!(code, 0, "{}", p.dump());
    assert!(p.stdout().contains("檔案遺失"), "{}", p.dump());
    let (code, p) = cli(&["import", all.to_str().unwrap(), "--json"]);
    assert_eq!(code, 0, "{}", p.dump());
    assert_eq!(json(&p)["data"]["stored"], serde_json::json!([DEB]));
    let health = remote_item(&api.data("/api/v1/system/health"));
    assert_eq!(health["status"], "ok", "{health}");

    // 刪除：L2（--confirm 為版本號）
    let (code, p) = cli(&["delete", VERSION]);
    assert_eq!(code, 2, "{}", p.dump());
    let (code, p) = cli(&["delete", VERSION, "--confirm", VERSION, "--reason", "e2e"]);
    assert_eq!(code, 0, "{}", p.dump());
    assert_eq!(api.data("/api/v1/releases"), serde_json::json!([]));
    assert_eq!(
        std::fs::read_dir(&store).unwrap().count(),
        0,
        "發佈檔一併刪除"
    );
    let (code, p) = cli(&["list"]);
    assert_eq!(code, 0);
    assert!(p.stdout().contains("版本庫沒有任何版本"), "{}", p.dump());

    // 稽核：cli 的 release.* 事件在雜湊鏈中，鏈完整
    let audit = api.data("/api/v1/audit?category=release&limit=50");
    let actions: Vec<(&str, &str)> = audit
        .as_array()
        .expect("稽核事件")
        .iter()
        .map(|e| (e["action"].as_str().unwrap(), e["actor"].as_str().unwrap()))
        .collect();
    for want in ["release.import", "release.import_failed", "release.delete"] {
        assert!(
            actions
                .iter()
                .any(|(a, who)| *a == want && who.starts_with("cli:")),
            "缺少 {want}：{actions:?}"
        );
    }
    k.stop();
    let (code, p) = run_to_exit(
        "kairos",
        &kairos.dir,
        &["audit", "verify", "--config", "kairos.toml"],
        &[],
    );
    assert_eq!(code, 0, "{}", p.dump());
}
