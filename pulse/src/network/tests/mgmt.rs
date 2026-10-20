//! 遠端管理（feature `config`、`upgrade`）在連線上的整合測試：以假的 Kairos（記憶體內的串流）驗證 hello 的
//! `install` 與 features、`pending` → `config_get` → `config` → `config_status`、需要重新連線的鍵、
//! `upgrade_get` → 提議 → 分段下載 → 狀態回報，以及 durable 連線上回應順序的對應。Pulse 送出的每一則
//! 遠端管理訊息都以 `kairos-proto` 的 Kairos 端驗證檢查。

use super::*;
use crate::remote::config::{CentralConfig, LocalSources};
use crate::remote::install::Detection;
use crate::remote::upgrade::{UpgradeAgent, UpgradeEnv};
use serde_json::json;
use std::path::{Path, PathBuf};

const SIGNERS: &str =
    include_str!("../../../../crates/kairos-proto/testdata/release/allowed_signers");
const TAR: &[u8] = include_bytes!(
    "../../../../crates/kairos-proto/testdata/release/pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz"
);
const PROTOCOL: &str = include_str!("../../../../crates/kairos-proto/PROTOCOL.md");
const UPGRADE_ID: &str = "9d3e7a10-4b2c-4f6e-8a1d-2c3b4a5f6e70";

struct MgmtDir(PathBuf);

impl Drop for MgmtDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mgmt_dir(tag: &str) -> MgmtDir {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "pulse-net-mgmt-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    MgmtDir(d)
}

/// 遠端管理就緒的 Pulse（tar、信任錨為測試金鑰 A、快取在暫存目錄）
fn mgmt_settings(dir: &Path, local: AppConfig) -> (Settings, Arc<RemoteMgmt>) {
    let anchor = dir.join("allowed_signers");
    std::fs::write(&anchor, SIGNERS).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&anchor, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let central = CentralConfig::new(
        local,
        LocalSources::default(),
        Some(dir.join(crate::remote::config::CACHE_RELATIVE)),
    );
    let agent = UpgradeAgent::new(UpgradeEnv {
        state_dir: dir.to_path_buf(),
        detection: Detection {
            method: kairos_proto::mgmt::INSTALL_METHOD_TAR,
            upgrade: kairos_proto::mgmt::INSTALL_UPGRADE_READY,
            reason: None,
            trust_anchor: anchor,
            fingerprints: Vec::new(),
        },
        current_version: "0.9.0".into(),
        arch: "x86_64".into(),
        extra_principals: Vec::new(),
        allow_downgrade: false,
        health_timeout_secs: 0,
        require_root_owner: false,
        free_space: |_| None,
        redetect: None,
    });
    let mgmt = RemoteMgmt::new(central, agent);
    let mut s = settings(60);
    s.mgmt = Some(mgmt.clone());
    (s, mgmt)
}

fn example_offer(seq: u64) -> serde_json::Value {
    let line = PROTOCOL
        .lines()
        .find_map(|l| {
            l.strip_prefix("K ")
                .filter(|l| l.contains("\"upgrade_offer\""))
        })
        .unwrap();
    let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
    v["seq"] = seq.into();
    let now = chrono::Utc::now();
    v["apply_window"] = json!({
        "not_before": (now + chrono::TimeDelta::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "not_after": (now + chrono::TimeDelta::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    });
    v
}

/// Kairos 端的回應規則：hello_ack 帶 `features` 與 `pending`，之後依訊息類型回應（config 依 `config`），
/// 收集 `window` 內收到的訊息
async fn fake_kairos(
    server: DuplexStream,
    features: &'static [&'static str],
    pending: serde_json::Value,
    config: serde_json::Value,
    window: Duration,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    let mut f = server_side(server);
    let line = f.next().await.unwrap().unwrap();
    let hello: serde_json::Value = serde_json::from_str(&line).unwrap();
    let mut ack = json!({"type":"hello_ack","seq":hello["seq"],"proto":2,"offline_after_ms":5000,
        "features":features,"server_time":now_ts()});
    if !pending.is_null() {
        ack["pending"] = pending;
    }
    f.send(ack.to_string()).await.unwrap();
    let mut got = Vec::new();
    let deadline = Instant::now() + window;
    while let Ok(Some(Ok(line))) = time::timeout_at(deadline, f.next()).await {
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        // Kairos 端的驗證（協商了 config／upgrade 時的新訊息）
        let msg = kairos_proto::parse_client_msg(&line).unwrap_or_else(|e| panic!("{line}: {e:?}"));
        kairos_proto::validate_client_msg(&msg).unwrap_or_else(|e| panic!("{line}: {e}"));
        match &msg {
            kairos_proto::ClientMsg::ConfigStatus(s) => {
                kairos_proto::mgmt::validate_config_status(s).unwrap()
            }
            kairos_proto::ClientMsg::UpgradeStatus(s) => {
                kairos_proto::mgmt::validate_upgrade_status(s).unwrap()
            }
            kairos_proto::ClientMsg::UpgradeGet(g) => {
                kairos_proto::mgmt::validate_upgrade_get(g).unwrap()
            }
            kairos_proto::ClientMsg::UpgradeChunkGet(g) => {
                kairos_proto::mgmt::validate_upgrade_chunk_get(g).unwrap()
            }
            _ => {}
        }
        let seq = v["seq"].as_u64().unwrap();
        let reply = match v["type"].as_str().unwrap() {
            "config_get" => {
                let settings = config.as_object().unwrap().clone();
                serde_json::to_value(kairos_proto::ServerMsg::Config(Box::new(
                    kairos_proto::RemoteConfig::new(seq, 42, settings, None, now_ts()),
                )))
                .unwrap()
            }
            "upgrade_get" => example_offer(seq),
            "upgrade_chunk_get" => {
                let off = usize::try_from(v["offset"].as_u64().unwrap()).unwrap();
                let len = usize::try_from(v["length"].as_u64().unwrap()).unwrap();
                let end = (off + len).min(TAR.len());
                serde_json::to_value(kairos_proto::ServerMsg::UpgradeChunk(Box::new(
                    kairos_proto::UpgradeChunk::new(
                        seq,
                        UPGRADE_ID,
                        v["filename"].as_str().unwrap(),
                        off as u64,
                        &TAR[off..end],
                        end == TAR.len(),
                    ),
                )))
                .unwrap()
            }
            _ => json!({"type":"ack","seq":seq}),
        };
        if f.send(reply.to_string()).await.is_err() {
            break;
        }
        got.push(v);
    }
    (hello, got)
}

fn of<'a>(got: &'a [serde_json::Value], t: &str) -> Vec<&'a serde_json::Value> {
    got.iter().filter(|v| v["type"] == t).collect()
}

#[tokio::test]
async fn hello_declares_config_and_upgrade_and_pending_drives_both_flows() {
    let dir = mgmt_dir("flows");
    let (s, mgmt) = mgmt_settings(&dir.0, AppConfig::default_for_tests());
    let (client, server) = duplex(256 * 1024);
    let srv = tokio::spawn(fake_kairos(
        server,
        &["metrics", "config", "upgrade"],
        json!({"config_rev": 42, "upgrade_id": UPGRADE_ID}),
        json!({"log": {"level": "debug"}, "processes": {"top": 10}}),
        Duration::from_millis(1500),
    ));
    let (tx, mut rx) = mpsc::channel::<Sample>(1);
    let (err, registered) = serve_with_reports(client, &s, &mut rx).await;
    drop(tx);
    assert!(registered, "{err:?}");
    let (hello, got) = srv.await.unwrap();
    // hello：install 與 features
    assert_eq!(
        hello["install"],
        json!({"method": "tar", "upgrade": "ready", "config": "enabled"})
    );
    let features: Vec<&str> = hello["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(
        features.contains(&"config") && features.contains(&"upgrade"),
        "{features:?}"
    );
    // 集中設定：hello_ack 後送一次 config_status（rev 0），config_get，套用後再送一次
    let statuses = of(&got, "config_status");
    assert_eq!(statuses.len(), 2, "{got:?}");
    assert_eq!(
        (statuses[0]["rev"].as_u64(), statuses[0]["state"].as_str()),
        (Some(0), Some("applied"))
    );
    assert_eq!(of(&got, "config_get")[0]["have_rev"], 0);
    assert_eq!(
        (statuses[1]["rev"].as_u64(), statuses[1]["state"].as_str()),
        (Some(42), Some("applied"))
    );
    assert_eq!(statuses[1]["effective"]["log"]["level"], "debug");
    assert_eq!(mgmt.config.effective().log.level, "debug");
    assert_eq!(mgmt.config.applied_rev(), 42);
    // 遠端升級：upgrade_get → 提議 → accepted → 分段下載 → staged（時段在 1 小時後，不交付）
    assert_eq!(of(&got, "upgrade_get")[0]["upgrade_id"], UPGRADE_ID);
    let ups: Vec<&str> = of(&got, "upgrade_status")
        .iter()
        .map(|v| v["state"].as_str().unwrap())
        .collect();
    assert_eq!(ups, ["accepted", "staged"], "{got:?}");
    let chunks = of(&got, "upgrade_chunk_get");
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        (chunks[0]["offset"].as_u64(), chunks[0]["length"].as_u64()),
        (Some(0), Some(54))
    );
    assert_eq!(mgmt.upgrade.view().phase.as_deref(), Some("staged"));
    assert!(!dir.0.join("upgrade/request.json").exists());
}

#[tokio::test]
async fn reconnect_keys_end_the_session_after_the_status() {
    let dir = mgmt_dir("reconnect");
    let (s, mgmt) = mgmt_settings(&dir.0, AppConfig::default_for_tests());
    let (client, server) = duplex(256 * 1024);
    let srv = tokio::spawn(fake_kairos(
        server,
        &["metrics", "config"],
        json!({"config_rev": 42}),
        json!({"heartbeat": {"interval": 30}}),
        Duration::from_millis(1500),
    ));
    let (_tx, mut rx) = mpsc::channel::<Sample>(1);
    let (err, registered) = serve_with_reports(client, &s, &mut rx).await;
    assert!(registered);
    assert!(matches!(err, SessionError::Reconfigure), "{err:?}");
    let (_, got) = srv.await.unwrap();
    let statuses = of(&got, "config_status");
    assert_eq!(
        statuses.last().unwrap()["rev"],
        42,
        "先回報 applied 再重新連線：{got:?}"
    );
    assert_eq!(mgmt.config.effective().heartbeat.interval, 30);
    assert!(of(&got, "upgrade_get").is_empty(), "沒有協商 upgrade");
}

#[tokio::test]
async fn old_kairos_gets_install_but_no_management_messages() {
    let dir = mgmt_dir("old");
    let mut local = AppConfig::default_for_tests();
    local.remote_config.enabled = false;
    let (s, _) = mgmt_settings(&dir.0, local);
    let (client, server) = duplex(256 * 1024);
    let srv = tokio::spawn(fake_kairos(
        server,
        &["metrics"],
        serde_json::Value::Null,
        json!({}),
        Duration::from_millis(600),
    ));
    let (_tx, mut rx) = mpsc::channel::<Sample>(1);
    let _ = serve_with_reports(client, &s, &mut rx).await;
    let (hello, got) = srv.await.unwrap();
    assert_eq!(hello["install"]["config"], "disabled");
    let features: Vec<&str> = hello["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    // 停用後第一次連線仍宣告 config（送一次 disabled）；Kairos 沒有列出 → 什麼都不送
    assert!(features.contains(&"config"), "{features:?}");
    assert!(
        got.iter()
            .all(|v| !v["type"].as_str().unwrap().starts_with("config")
                && !v["type"].as_str().unwrap().starts_with("upgrade")),
        "{got:?}"
    );
}

#[tokio::test]
async fn durable_session_keeps_report_acks_and_management_replies_in_order() {
    let dir = mgmt_dir("durable");
    let (s, mgmt) = mgmt_settings(&dir.0, AppConfig::default_for_tests());
    let (_spool_dir, shared) = test_spool("mgmt", 3);
    let (client, server) = duplex(256 * 1024);
    let srv = tokio::spawn(fake_kairos(
        server,
        &["metrics", "durable", "config", "upgrade"],
        json!({"config_rev": 42, "upgrade_id": UPGRADE_ID}),
        json!({"report": {"interval": 10}}),
        Duration::from_millis(3000),
    ));
    let serve = tokio::spawn({
        let s = s.clone();
        let shared = shared.clone();
        async move { serve_spool(client, &s, &shared).await }
    });
    let (_, got) = srv.await.unwrap();
    serve.abort();
    // 積壓的 3 筆 report 都送達（遠端管理的回應插在中間也不會被誤當成 report 的 ack）
    assert_eq!(of(&got, "report").len(), 3, "{got:?}");
    assert!(shared.lock().is_empty());
    assert_eq!(mgmt.config.applied_rev(), 42);
    assert_eq!(mgmt.config.effective().report.interval, 10);
    assert_eq!(mgmt.upgrade.view().phase.as_deref(), Some("staged"));
}
