//! feature `config`、`upgrade` 的測試：PROTOCOL.md「範例／遠端管理」的測試向量、協商、驗證、
//! `pending` 的容錯、正規化 JSON 與設定白名單。

use super::settings::*;
use super::*;
use crate::tests::{Ver, vectors};
use crate::{
    ClientMsg, ErrorCode, ServerMsg, message_type, parse_client_msg, parse_server_msg,
    validate_client_msg,
};
use serde_json::json;

const UPGRADE_ID: &str = "9d3e7a10-4b2c-4f6e-8a1d-2c3b4a5f6e70";

fn client(kind: &str, seq: u64) -> ClientMsg {
    let line = vectors(Ver::Mgmt, 'P')
        .into_iter()
        .find(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["type"] == kind && v["seq"] == seq
        })
        .unwrap_or_else(|| panic!("找不到 {kind} seq={seq} 的向量"));
    parse_client_msg(line).unwrap()
}

fn server(kind: &str, seq: u64) -> (&'static str, ServerMsg) {
    let line = vectors(Ver::Mgmt, 'K')
        .into_iter()
        .find(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["type"] == kind && v["seq"] == seq
        })
        .unwrap_or_else(|| panic!("找不到 {kind} seq={seq} 的向量"));
    (line, parse_server_msg(line).unwrap())
}

fn same_json(a: &str, b: &str) {
    assert_eq!(
        serde_json::from_str::<Value>(a).unwrap(),
        serde_json::from_str::<Value>(b).unwrap()
    );
}

fn offer() -> UpgradeOffer {
    match server("upgrade_offer", 5).1 {
        ServerMsg::UpgradeOffer(o) => *o,
        other => panic!("{other:?}"),
    }
}

// ---- 測試向量 ----

#[test]
fn hello_declares_features_and_install() {
    let ClientMsg::Hello { ext, .. } = client("hello", 1) else {
        panic!("應解析為 hello");
    };
    let features = ext.offer.as_ref().unwrap().features.clone().unwrap();
    let host = ext.host.as_ref().unwrap();
    let install = host.install.clone().unwrap();
    assert_eq!(install.method.as_deref(), Some(INSTALL_METHOD_TAR));
    assert_eq!(install.upgrade.as_deref(), Some(INSTALL_UPGRADE_READY));
    assert_eq!(install.config.as_deref(), Some(INSTALL_CONFIG_ENABLED));
    assert!(crate::validate_host_info(host).is_ok());
    // Pulse 宣告的 feature 與 install 一致
    for f in client_mgmt_features(true, &install) {
        assert!(features.iter().any(|x| x == f), "{f}");
    }
    // 協商：Kairos 的條件都成立時列出兩個 feature
    let (_, ack) = server("hello_ack", 1);
    let ServerMsg::HelloAck {
        features: Some(listed),
        pending,
        ..
    } = ack
    else {
        panic!("應解析為 hello_ack");
    };
    let policy = ServerMgmtPolicy {
        config_enabled: true,
        upgrade_enabled: true,
        sqlite: true,
        mtls: true,
    };
    assert_eq!(policy.features_to_list(2, &features), MGMT_FEATURES);
    let session = MgmtSession::new(2, &listed, &features);
    assert_eq!(
        session,
        MgmtSession {
            config: true,
            upgrade: true
        }
    );
    assert_eq!(
        pending,
        Some(Pending {
            config_rev: Some(42),
            upgrade_id: None
        })
    );
    assert_eq!(Pending::for_session(session, 42, None), pending);
    assert!(pending.unwrap().needs_config_get(37));
}

#[test]
fn config_round_trip_with_real_hash() {
    let ClientMsg::ConfigGet(config_get) = client("config_get", 2) else {
        panic!("應解析為 config_get");
    };
    assert_eq!(config_get.have_rev, 37);
    let (line, msg) = server("config", 2);
    let ServerMsg::Config(cfg) = msg else {
        panic!("應解析為 config");
    };
    assert!(validate_remote_config(&cfg).is_ok());
    assert_eq!(cfg.rev, 42);
    // Kairos 以同樣的 settings 組出的訊息（hash 由 settings 計算）與向量相同
    let built = RemoteConfig::new(
        2,
        42,
        cfg.settings.clone(),
        cfg.sources.clone(),
        cfg.issued_at.clone(),
    );
    same_json(&ServerMsg::Config(Box::new(built)).to_line().unwrap(), line);
    // 白名單：全部接受
    let check = check_remote(&cfg.settings);
    assert!(
        check.errors.is_empty() && check.ignored.is_empty(),
        "{check:?}"
    );
    assert_eq!(check.accepted.len(), 3);
    assert!(validate_profile(&cfg.settings).is_ok());

    let ClientMsg::ConfigStatus(status) = client("config_status", 3) else {
        panic!("應解析為 config_status");
    };
    assert!(validate_config_status(&status).is_ok());
    assert_eq!(status.hash, cfg.hash);
    assert_eq!(status.state, CONFIG_STATE_APPLIED);
    assert_eq!(status.code.as_deref(), Some("PLS-I0503"));
    // overridden 與 merge_key 的結果一致
    let spec = setting("report.interval").unwrap();
    let merged = merge_key(
        spec,
        &json!(5),
        get(&cfg.settings, "report.interval"),
        Some(LocalValue {
            value: &json!(5),
            source: OVERRIDE_SOURCE_FILE,
        }),
    );
    assert_eq!(merged.effective, json!(5));
    assert_eq!(merged.overridden.as_ref(), status.overridden.first());
    let (line, ack) = server("ack", 3);
    assert_eq!(ack, ServerMsg::ack(3));
    assert!(!line.contains("pending"));
}

#[test]
fn heartbeat_ack_carries_the_upgrade_id() {
    let (line, ack) = server("ack", 4);
    let pending = Pending::for_session(
        MgmtSession {
            config: true,
            upgrade: true,
        },
        42,
        Some(UPGRADE_ID),
    );
    assert_eq!(ack, ServerMsg::heartbeat_ack(4, pending.clone()));
    same_json(
        &ServerMsg::heartbeat_ack(4, pending.clone())
            .to_line()
            .unwrap(),
        line,
    );
    let pending = pending.unwrap();
    assert_eq!(pending.new_upgrade_id::<&str>(&[]), Some(UPGRADE_ID));
    assert_eq!(pending.new_upgrade_id(&[UPGRADE_ID.to_uppercase()]), None);
    assert!(!pending.needs_config_get(42));
    let ClientMsg::UpgradeGet(get) = client("upgrade_get", 5) else {
        panic!("應解析為 upgrade_get");
    };
    assert_eq!(get.upgrade_id, UPGRADE_ID);
    assert!(validate_upgrade_get(&get).is_ok());
}

#[test]
fn upgrade_offer_vector_verifies_against_the_test_signers() {
    use crate::release::*;
    let o = offer();
    assert!(validate_upgrade_offer(&o).is_ok());
    assert!(o.file(&o.artifact.filename).is_some());
    assert!(o.file("pulse-0.9.0-1a2b3c4-linux-x86_64.tar.gz").is_none());
    // 本機信任錨（testdata 的 allowed_signers，與 RELEASING.md 的產生方式相同）
    let signers = AllowedSigners::parse(include_str!("../../testdata/release/allowed_signers"));
    let manifest = o.manifest.as_ref().unwrap();
    let bundle = verify_bundle(
        &ReleaseBundle {
            sha256sums: &o.sha256sums,
            signature: &o.signature,
            manifest: Some((&manifest.filename, &manifest.content)),
        },
        &TrustPolicy {
            signers: &signers,
            extra_principals: &[],
            now_unix: 1_795_000_000,
        },
    )
    .unwrap();
    assert_eq!(bundle.signer.principal, RELEASE_PRINCIPAL);
    let (os, arch, format) = artifact_kind(INSTALL_METHOD_TAR, "x86_64").unwrap();
    let file = bundle
        .check_artifact(
            &o.artifact.filename,
            &o.artifact.sha256,
            &ArtifactExpectation {
                product: Some(Product::Pulse),
                version: Some(&o.target.version),
                commit: Some(&o.target.commit),
                os: Some(os),
                arch: Some(arch),
                format: Some(format),
            },
        )
        .unwrap();
    assert_eq!(file.version.to_string(), "0.9.1");
    let current = Version::parse("0.9.0").unwrap();
    assert_eq!(
        check_version_policy(
            &current,
            &file.version,
            bundle.manifest.as_ref(),
            o.allow_downgrade,
            false
        ),
        Ok(VersionAction::Upgrade)
    );
    // 時段
    let not_before = crate::enroll::parse_rfc3339_ms("2026-11-20T17:00:00.000Z").unwrap();
    assert!(!o.apply_window.contains(not_before - 1));
    assert!(o.apply_window.contains(not_before));
    assert!(!o.apply_window.has_expired(not_before));
    assert!(o.apply_window.has_expired(not_before + 4 * 3_600_000 + 1));
}

#[test]
fn chunk_and_status_vectors() {
    let o = offer();
    let ClientMsg::UpgradeChunkGet(get) = client("upgrade_chunk_get", 7) else {
        panic!("應解析為 upgrade_chunk_get");
    };
    assert!(validate_upgrade_chunk_get(&get).is_ok());
    assert_eq!(get.length, MAX_CHUNK_BYTES);
    let (line, msg) = server("upgrade_chunk", 7);
    let ServerMsg::UpgradeChunk(chunk) = msg else {
        panic!("應解析為 upgrade_chunk");
    };
    let data = chunk.decode_data().unwrap();
    assert_eq!(data.len() as u64, o.artifact.size);
    assert_eq!(crate::release::sha256_hex(&data), o.artifact.sha256);
    assert_eq!(
        data.as_slice(),
        include_bytes!("../../testdata/release/pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz")
    );
    let built = UpgradeChunk::new(7, UPGRADE_ID, &get.filename, 0, &data, true);
    same_json(
        &ServerMsg::UpgradeChunk(Box::new(built)).to_line().unwrap(),
        line,
    );
    for (seq, state) in [(6, UPGRADE_STATE_ACCEPTED), (8, UPGRADE_STATE_STAGED)] {
        let ClientMsg::UpgradeStatus(s) = client("upgrade_status", seq) else {
            panic!("應解析為 upgrade_status");
        };
        assert_eq!(s.state, state);
        assert!(validate_upgrade_status(&s).is_ok());
        assert!(!is_terminal_upgrade_state(&s.state));
        assert!(validate_client_msg(&ClientMsg::UpgradeStatus(s)).is_ok());
    }
}

#[test]
fn error_vectors() {
    let (line, msg) = server("error", 9);
    same_json(
        &ServerMsg::rate_limited(
            Some(9),
            "too many concurrent downloads; retry later",
            30_000,
        )
        .to_line()
        .unwrap(),
        line,
    );
    assert!(!msg.is_fatal());
    let (line, msg) = server("error", 10);
    same_json(
        &ServerMsg::error(
            Some(10),
            ErrorCode::UpgradeUnavailable,
            "the rollout was cancelled",
        )
        .to_line()
        .unwrap(),
        line,
    );
    assert!(!msg.is_fatal());
    assert_eq!(
        ErrorCode::from_code("upgrade_unavailable"),
        Some(ErrorCode::UpgradeUnavailable)
    );
    // retry_after_ms 不是合法 u32 時視為沒有
    let line = r#"{"type":"error","seq":1,"code":"rate_limited","message":"m","fatal":false,"retry_after_ms":"30000"}"#;
    assert!(matches!(
        parse_server_msg(line).unwrap(),
        ServerMsg::Error {
            retry_after_ms: None,
            ..
        }
    ));
}

// ---- 相容性 ----

#[test]
fn older_vectors_have_no_management_fields() {
    for ver in [
        Ver::V1,
        Ver::V2,
        Ver::Durable,
        Ver::Renew,
        Ver::Deregister,
        Ver::Cluster,
    ] {
        for line in vectors(ver, 'K').into_iter().chain(vectors(ver, 'P')) {
            for field in [
                "pending",
                "\"install\"",
                "retry_after_ms",
                "\"config\"",
                "\"upgrade\"",
            ] {
                assert!(!line.contains(field), "{line}");
            }
        }
    }
    // ServerMsg::ack 與舊版完全相同（不帶 pending）
    assert_eq!(
        serde_json::to_value(ServerMsg::ack(2)).unwrap(),
        json!({"type":"ack","seq":2})
    );
}

#[test]
fn old_pulse_treats_new_server_types_as_unknown() {
    const TYPES_0_9_CLUSTER: [&str; 6] = [
        "hello_ack",
        "ack",
        "error",
        "renew_result",
        "deregister_ack",
        "drain",
    ];
    for kind in ["config", "upgrade_offer", "upgrade_chunk"] {
        assert!(!TYPES_0_9_CLUSTER.contains(&kind));
        assert!(crate::SERVER_MSG_TYPES.contains(&kind));
    }
}

#[test]
fn malformed_management_messages_keep_their_type() {
    // Kairos 沒有協商時要回 unknown_type（同 renew_request），因此必須能取出 type
    for t in MGMT_CLIENT_MSG_TYPES {
        let line = format!(r#"{{"type":"{t}","seq":2}}"#);
        assert!(parse_client_msg(&line).is_err(), "{line}");
        assert_eq!(message_type(&line).as_deref(), Some(t));
    }
    // 欄位格式不合法的也由 validate_client_msg 擋下
    let line = r#"{"type":"upgrade_get","seq":5,"ts":"t","upgrade_id":"not-a-uuid"}"#;
    let msg = parse_client_msg(line).unwrap();
    assert!(
        validate_client_msg(&msg)
            .unwrap_err()
            .contains("upgrade_id")
    );
    let s = MgmtSession {
        config: true,
        upgrade: false,
    };
    assert!(s.accepts_client_type("config_get") && s.accepts_client_type("config_status"));
    assert!(!s.accepts_client_type("upgrade_get") && !s.accepts_client_type("heartbeat"));
}

#[test]
fn negotiation_needs_v2_both_sides_and_server_conditions() {
    let both = MGMT_FEATURES;
    let none: [&str; 0] = [];
    assert_eq!(MgmtSession::new(1, &both, &both), MgmtSession::NONE);
    assert_eq!(MgmtSession::new(2, &both, &none), MgmtSession::NONE);
    assert_eq!(MgmtSession::new(2, &none, &both), MgmtSession::NONE);
    assert!(!MgmtSession::NONE.any());
    let only_config = MgmtSession::new(2, &both, &[FEATURE_CONFIG]);
    assert!(only_config.config && !only_config.upgrade && only_config.any());
    assert_eq!(
        Pending::for_session(MgmtSession::NONE, 42, Some(UPGRADE_ID)),
        None
    );
    assert_eq!(
        Pending::for_session(only_config, 0, Some(UPGRADE_ID)),
        Some(Pending {
            config_rev: Some(0),
            upgrade_id: None
        })
    );
    let only_upgrade = MgmtSession::new(2, &both, &[FEATURE_UPGRADE]);
    assert_eq!(
        Pending::for_session(only_upgrade, 42, None),
        Some(Pending::default())
    );

    let ok = ServerMgmtPolicy {
        config_enabled: true,
        upgrade_enabled: true,
        sqlite: true,
        mtls: true,
    };
    let empty: Vec<&str> = Vec::new();
    assert_eq!(ok.features_to_list(1, &both), empty);
    assert_eq!(ok.features_to_list(2, &none), empty);
    assert_eq!(ok.features_to_list(2, &["upgrade"]), ["upgrade"]);
    for policy in [
        ServerMgmtPolicy {
            sqlite: false,
            ..ok
        },
        ServerMgmtPolicy { mtls: false, ..ok },
    ] {
        assert_eq!(policy.features_to_list(2, &both), empty);
    }
    assert_eq!(
        ServerMgmtPolicy {
            config_enabled: false,
            ..ok
        }
        .features_to_list(2, &both),
        ["upgrade"]
    );
    // Pulse：install.upgrade 不是 ready 時不宣告 upgrade
    for state in INSTALL_UPGRADE_STATES {
        let install = InstallInfo {
            upgrade: Some(state.to_string()),
            ..InstallInfo::default()
        };
        let declared = client_mgmt_features(false, &install);
        assert_eq!(
            declared.contains(&FEATURE_UPGRADE),
            state == INSTALL_UPGRADE_READY
        );
        assert!(!declared.contains(&FEATURE_CONFIG));
    }
}

#[test]
fn pending_is_lenient() {
    for (pending, expected) in [
        (json!(7), None),
        (json!("x"), None),
        (json!(null), None),
        (json!({}), Some(Pending::default())),
        (
            json!({"config_rev":"42","upgrade_id":"x"}),
            Some(Pending::default()),
        ),
        (
            json!({"config_rev":-1,"upgrade_id":UPGRADE_ID.to_uppercase()}),
            Some(Pending {
                config_rev: None,
                upgrade_id: Some(UPGRADE_ID.to_string()),
            }),
        ),
    ] {
        let line = json!({"type":"ack","seq":4,"pending":pending}).to_string();
        let ServerMsg::Ack { pending: p, .. } = parse_server_msg(&line).unwrap() else {
            panic!("{line}");
        };
        assert_eq!(p, expected, "{line}");
        let line = json!({"type":"hello_ack","seq":1,"proto":2,"offline_after_ms":75000,"pending":pending})
            .to_string();
        let ServerMsg::HelloAck { pending: p, .. } = parse_server_msg(&line).unwrap() else {
            panic!("{line}");
        };
        assert_eq!(p, expected, "{line}");
    }
}

#[test]
fn install_limits() {
    for ok in ["tar", "no_trust_anchor", "a", &"x".repeat(32), "msi2"] {
        assert!(is_install_token(ok), "{ok}");
    }
    for bad in ["", "Tar", "no-helper", "a b", &"x".repeat(33), "安裝"] {
        assert!(!is_install_token(bad), "{bad}");
    }
    // 不認得的值不是錯誤
    assert!(
        validate_install(&InstallInfo {
            method: Some("snap".into()),
            upgrade: Some("future_state".into()),
            config: None,
            external_url: Some("future_mode".into()),
        })
        .is_ok()
    );
    let base = vectors(Ver::Mgmt, 'P')[0];
    // 格式錯誤：hello 可以解析，validate_host_info 失敗（伺服器回 bad_request）
    let bad = base.replace(r#""method":"tar""#, r#""method":"TAR""#);
    let ClientMsg::Hello { ext, .. } = parse_client_msg(&bad).unwrap() else {
        panic!();
    };
    assert!(
        crate::validate_host_info(ext.host.as_ref().unwrap())
            .unwrap_err()
            .contains("install.method")
    );
    // 型別錯誤：host 的部分無法解析（與 os 相同）
    for replacement in [r#""install":"tar""#, r#""install":{"method":5}"#] {
        let bad = base.replace(
            r#""install":{"method":"tar","upgrade":"ready","config":"enabled"}"#,
            replacement,
        );
        let ClientMsg::Hello { ext, .. } = parse_client_msg(&bad).unwrap() else {
            panic!();
        };
        assert!(ext.host.is_err(), "{bad}");
    }
    // null 視同省略；其他欄位忽略
    let line = base.replace(
        r#""install":{"method":"tar","upgrade":"ready","config":"enabled"}"#,
        r#""install":null"#,
    );
    let ClientMsg::Hello { ext, .. } = parse_client_msg(&line).unwrap() else {
        panic!();
    };
    assert_eq!(ext.host.as_ref().unwrap().install, None);
}

/// `install.upgrade = "unsafe_path"`（Pulse 0.10.0 起）不需要改協定版本：它符合 0.9.0 就定下的 `install`
/// 欄位格式（1～32 bytes 的小寫英文字母、數字或 `_`；不認得的值不是錯誤、原樣保存），所以 0.9 的 Kairos
/// 收到新的 Pulse 的 hello 一樣能解析並通過驗證；Pulse 不因此宣告 `upgrade`，與其他非 `ready` 的值相同
#[test]
fn unsafe_path_is_a_valid_install_upgrade_value_for_old_and_new_peers() {
    assert_eq!(INSTALL_UPGRADE_UNSAFE_PATH, "unsafe_path");
    assert!(INSTALL_UPGRADE_STATES.contains(&INSTALL_UPGRADE_UNSAFE_PATH));
    // 0.9.0 的格式限制（這個函式自 0.9.0 起沒有改變；常數本身必須永遠符合它）
    assert!(is_install_token(INSTALL_UPGRADE_UNSAFE_PATH));
    assert!(INSTALL_UPGRADE_UNSAFE_PATH.len() <= MAX_INSTALL_FIELD_BYTES);
    for state in INSTALL_UPGRADE_STATES {
        assert!(is_install_token(state), "{state}");
    }
    // 新的 Pulse 的 hello：可以解析、通過驗證，值原樣保存
    let base = vectors(Ver::Mgmt, 'P')[0];
    let line = base.replace(r#""upgrade":"ready""#, r#""upgrade":"unsafe_path""#);
    assert_ne!(line, base);
    let ClientMsg::Hello { ext, .. } = parse_client_msg(&line).unwrap() else {
        panic!();
    };
    let host = ext.host.as_ref().unwrap();
    crate::validate_host_info(host).unwrap();
    let install = host.install.clone().unwrap();
    assert_eq!(install.upgrade.as_deref(), Some("unsafe_path"));
    assert_eq!(install.method.as_deref(), Some(INSTALL_METHOD_TAR));
    // Pulse 不宣告 upgrade（集中設定不受影響）
    assert_eq!(client_mgmt_features(true, &install), [FEATURE_CONFIG]);
    // 序列化後再解析結果相同（Kairos 存入資料庫的就是這個字串）
    let json = serde_json::to_string(&install).unwrap();
    assert!(json.contains(r#""upgrade":"unsafe_path""#), "{json}");
    assert_eq!(serde_json::from_str::<InstallInfo>(&json).unwrap(), install);
}

/// `install.external_url`（0.10.1 之後的 Pulse；REMOTE.md 第 2.1 節）：選用欄位，三種狀態——`none`、
/// `allowlist`、沒有這個欄位（未知）。協定版本不變、不是新的 feature：舊的 Kairos 依「其他欄位忽略」
/// 略過它，舊的 Pulse 不送它
#[test]
fn external_url_declaration_is_optional_and_compatible_with_old_peers() {
    assert_eq!(INSTALL_EXTERNAL_URL_NONE, "none");
    assert_eq!(INSTALL_EXTERNAL_URL_ALLOWLIST, "allowlist");
    for state in INSTALL_EXTERNAL_URL_STATES {
        assert!(is_install_token(state), "{state}");
    }
    // 測試向量：新的 Pulse 的 hello（none、allowlist），可以解析、通過驗證
    let hellos = vectors(Ver::MgmtExternalUrl, 'P');
    let installs: Vec<InstallInfo> = hellos
        .iter()
        .map(|line| {
            let ClientMsg::Hello { ext, .. } = parse_client_msg(line).unwrap() else {
                panic!("{line}");
            };
            let host = ext.host.as_ref().unwrap();
            crate::validate_host_info(host).unwrap();
            host.install.clone().unwrap()
        })
        .collect();
    assert_eq!(installs[0].external_url.as_deref(), Some("none"));
    assert_eq!(installs[1].external_url.as_deref(), Some("allowlist"));
    // 宣告與 install.upgrade 無關：第二則的主機缺少輔助程式，照樣宣告；feature 的宣告不受影響
    assert_eq!(installs[1].upgrade.as_deref(), Some("no_helper"));
    assert_eq!(
        client_mgmt_features(true, &installs[0]),
        [FEATURE_CONFIG, FEATURE_UPGRADE]
    );
    assert_eq!(client_mgmt_features(true, &installs[1]), [FEATURE_CONFIG]);
    // 舊的 Pulse（0.9、0.10 的向量）沒有這個欄位：未知；序列化時也不會多出這個鍵
    let ClientMsg::Hello { ext, .. } = parse_client_msg(vectors(Ver::Mgmt, 'P')[0]).unwrap() else {
        panic!();
    };
    let old = ext.host.as_ref().unwrap().install.clone().unwrap();
    assert_eq!(old.external_url, None);
    assert!(
        !serde_json::to_string(&old)
            .unwrap()
            .contains("external_url")
    );
    // 舊的 Kairos（0.9、0.10：`install` 只有三個欄位、其他欄位忽略）收到新的 hello：照常解析，
    // 三個欄位的值不變
    #[derive(serde::Deserialize)]
    struct InstallV010 {
        method: Option<String>,
        upgrade: Option<String>,
        config: Option<String>,
    }
    for (line, new) in hellos.iter().zip(&installs) {
        let v: Value = serde_json::from_str(line).unwrap();
        let old: InstallV010 = serde_json::from_value(v["install"].clone()).unwrap();
        assert_eq!(
            (&old.method, &old.upgrade, &old.config),
            (&new.method, &new.upgrade, &new.config)
        );
    }
    // 不認得的值不是錯誤（原樣保存，Kairos 視為未知）；格式不符或型別錯誤是 bad_request
    let line = hellos[0].replace(r#""external_url":"none""#, r#""external_url":"per_host""#);
    let ClientMsg::Hello { ext, .. } = parse_client_msg(&line).unwrap() else {
        panic!();
    };
    crate::validate_host_info(ext.host.as_ref().unwrap()).unwrap();
    let line = hellos[0].replace(
        r#""external_url":"none""#,
        r#""external_url":"https://repo.example/""#,
    );
    let ClientMsg::Hello { ext, .. } = parse_client_msg(&line).unwrap() else {
        panic!();
    };
    assert!(crate::validate_host_info(ext.host.as_ref().unwrap()).is_err());
    // 型別錯誤：host 的部分無法解析（與 os、install 的其他欄位相同）
    let line = hellos[0].replace(r#""external_url":"none""#, r#""external_url":["none"]"#);
    let ClientMsg::Hello { ext, .. } = parse_client_msg(&line).unwrap() else {
        panic!();
    };
    assert!(ext.host.is_err(), "{line}");
    // 白名單 → 宣告：只看有沒有有效的項目
    use super::url::allowlist_state;
    assert_eq!(allowlist_state(&[]), "none");
    assert_eq!(allowlist_state(&["ftp://x/".to_string()]), "none");
    assert_eq!(
        allowlist_state(&[
            "bad".to_string(),
            "https://repo.example.gov.tw/p/".to_string()
        ]),
        "allowlist"
    );
}

// ---- 驗證 ----

fn status() -> ConfigStatus {
    match client("config_status", 3) {
        ClientMsg::ConfigStatus(s) => *s,
        other => panic!("{other:?}"),
    }
}

#[test]
fn config_status_limits() {
    let ok = status();
    let mut cases: Vec<(ConfigStatus, &str)> = Vec::new();
    let mut s = ok.clone();
    s.state = "done".into();
    cases.push((s, "state"));
    let mut s = ok.clone();
    s.hash = "sha256:ABC".into();
    cases.push((s, "hash"));
    let mut s = ok.clone();
    s.hash = String::new();
    cases.push((s, "hash"));
    let mut s = ok.clone();
    s.code = Some("PLS-X0503".into());
    cases.push((s, "code"));
    let mut s = ok.clone();
    s.ignored = (0..65)
        .map(|i| IgnoredKey {
            key: format!("x.k{i}"),
            reason: IGNORED_UNKNOWN_KEY.into(),
        })
        .collect();
    cases.push((s, "ignored"));
    let mut s = ok.clone();
    s.errors = vec![KeyError {
        key: "report.interval".into(),
        reason: ERR_OUT_OF_RANGE.into(),
        message: "x".repeat(257),
    }];
    cases.push((s, "message"));
    let mut s = ok.clone();
    s.effective.insert("Report".into(), json!({}));
    cases.push((s, "effective"));
    let mut s = ok.clone();
    s.effective.insert("report".into(), json!(5));
    cases.push((s, "effective"));
    for (s, needle) in cases {
        let err = validate_config_status(&s).unwrap_err();
        assert!(err.contains(needle), "{needle}: {err}");
    }
    // disabled 而從未套用：rev 0、hash 空字串
    let mut s = ok.clone();
    s.state = CONFIG_STATE_DISABLED.into();
    s.rev = 0;
    s.hash = String::new();
    s.code = Some("PLS-I0505".into());
    assert!(validate_config_status(&s).is_ok());
    // 截斷
    let mut s = ok;
    s.ignored = (0..100)
        .map(|i| IgnoredKey {
            key: format!("x.k{i}"),
            reason: IGNORED_UNKNOWN_KEY.into(),
        })
        .collect();
    s.truncate_lists();
    assert_eq!(s.ignored.len(), MAX_STATUS_ITEMS);
    assert_eq!(
        s.ignored.last().unwrap(),
        &IgnoredKey {
            key: "*".into(),
            reason: REASON_TRUNCATED.into()
        }
    );
    assert!(validate_config_status(&s).is_ok());
}

#[test]
fn remote_config_hash_must_match() {
    let ServerMsg::Config(mut cfg) = server("config", 2).1 else {
        panic!();
    };
    cfg.settings.insert("log".into(), json!({"level": "trace"}));
    assert!(validate_remote_config(&cfg).unwrap_err().contains("hash"));
    cfg.hash = settings_hash(&cfg.settings);
    assert!(validate_remote_config(&cfg).is_ok());
    cfg.issued_at = "yesterday".into();
    assert!(
        validate_remote_config(&cfg)
            .unwrap_err()
            .contains("issued_at")
    );
}

#[test]
fn upgrade_message_limits() {
    let ok = offer();
    let mut cases: Vec<(UpgradeOffer, &str)> = Vec::new();
    let mut o = ok.clone();
    o.upgrade_id = "x".into();
    cases.push((o, "upgrade_id"));
    let mut o = ok.clone();
    o.action = "reinstall".into();
    cases.push((o, "action"));
    let mut o = ok.clone();
    o.target.commit = "XYZ".into();
    cases.push((o, "commit"));
    let mut o = ok.clone();
    o.artifact.size = MAX_ARTIFACT_BYTES + 1;
    cases.push((o, "size"));
    let mut o = ok.clone();
    o.artifact.filename = "../pulse".into();
    cases.push((o, "filename"));
    let mut o = ok.clone();
    o.artifact.source = SOURCE_URL.into();
    cases.push((o, "url"));
    let mut o = ok.clone();
    o.artifact.url = Some("https://repo.example/x".into());
    cases.push((o, "url"));
    let mut o = ok.clone();
    o.health_timeout_secs = 60;
    cases.push((o, "health_timeout_secs"));
    let mut o = ok.clone();
    o.apply_window = ApplyWindow {
        not_before: "2026-11-21T00:00:00.000Z".into(),
        not_after: "2026-11-20T00:00:00.000Z".into(),
    };
    cases.push((o, "apply_window"));
    let mut o = ok.clone();
    o.signature = "s".repeat(MAX_SIGNATURE_BYTES + 1);
    cases.push((o, "signature"));
    for (o, needle) in cases {
        let err = validate_upgrade_offer(&o).unwrap_err();
        assert!(err.contains(needle), "{needle}: {err}");
    }
    // 外部 URL（0.10.0）的格式
    let mut o = ok.clone();
    o.artifact.source = SOURCE_URL.into();
    o.artifact.url = Some(format!("https://repo.example/{}", o.artifact.filename));
    assert!(validate_upgrade_offer(&o).is_ok());
    // 沒有清單（0.9.0 以前的發佈，降版或回滾目標）
    let mut o = ok;
    o.manifest = None;
    assert!(validate_upgrade_offer(&o).is_ok());
    assert!(
        serde_json::to_value(&o).unwrap()["manifest"].is_null(),
        "manifest 為 null 時照樣輸出"
    );

    let ClientMsg::UpgradeChunkGet(get) = client("upgrade_chunk_get", 7) else {
        panic!();
    };
    for (length, offset) in [(0, 0), (MAX_CHUNK_BYTES + 1, 0), (1, MAX_ARTIFACT_BYTES)] {
        let mut g = (*get).clone();
        g.length = length;
        g.offset = offset;
        assert!(validate_upgrade_chunk_get(&g).is_err(), "{length} {offset}");
    }

    let ClientMsg::UpgradeStatus(st) = client("upgrade_status", 8) else {
        panic!();
    };
    let mut s = (*st).clone();
    s.state = "done".into();
    assert!(validate_upgrade_status(&s).unwrap_err().contains("state"));
    let mut s = (*st).clone();
    s.detail = "x".repeat(MAX_UPGRADE_DETAIL_BYTES + 1);
    assert!(validate_upgrade_status(&s).unwrap_err().contains("detail"));
    let mut s = (*st).clone();
    s.detail = "dpkg: error\u{1b}[31m".into();
    assert!(validate_upgrade_status(&s).unwrap_err().contains("detail"));
    let mut s = (*st).clone();
    s.progress = Some(UpgradeProgress { bytes: 2, total: 1 });
    assert!(
        validate_upgrade_status(&s)
            .unwrap_err()
            .contains("progress")
    );
    let mut s = (*st).clone();
    s.code = Some("PLS-E8401".into());
    s.state = UPGRADE_STATE_FAILED.into();
    assert!(validate_upgrade_status(&s).is_ok());
    assert!(is_terminal_upgrade_state(&s.state));
}

#[test]
fn chunk_data_is_strict_base64_and_bounded() {
    let mut c = UpgradeChunk::new(1, UPGRADE_ID, "f", 0, &[0u8; 32768], false);
    assert_eq!(c.decode_data().unwrap().len(), 32768);
    c.data = crate::b64::encode(&[0u8; 32769]);
    assert!(c.decode_data().is_err());
    c.data = "SGVsbG8".into();
    assert!(c.decode_data().is_err());
    c.data = "SGVs bG8=".into();
    assert!(c.decode_data().is_err());
}

#[test]
fn product_codes_and_hashes() {
    for ok in ["PLS-W0501", "KRS-E8413", "PLS-I8416"] {
        assert!(is_product_code(ok), "{ok}");
    }
    for bad in [
        "PLS-X0501",
        "PLS-W050",
        "pls-w0501",
        "ABC-W0501",
        "PLS-W05011",
    ] {
        assert!(!is_product_code(bad), "{bad}");
    }
    assert!(is_config_hash(&format!("sha256:{}", "a".repeat(64))));
    assert!(!is_config_hash(&"a".repeat(64)));
    assert!(!is_config_hash(&format!("sha256:{}", "A".repeat(64))));
}

#[test]
fn health_timeout_and_staging_space() {
    assert_eq!(effective_health_timeout_secs(600, 0), 600);
    assert_eq!(effective_health_timeout_secs(600, 900), 900);
    assert_eq!(effective_health_timeout_secs(600, 300), 600);
    assert_eq!(
        effective_health_timeout_secs(10, 0),
        MIN_HEALTH_TIMEOUT_SECS
    );
    assert_eq!(
        effective_health_timeout_secs(600, 99_999),
        MAX_HEALTH_TIMEOUT_SECS
    );
    assert_eq!(
        staging_space_needed(10 * 1024 * 1024),
        30 * 1024 * 1024 + STAGING_EXTRA_BYTES
    );
    assert_eq!(staging_space_needed(u64::MAX), u64::MAX);
}

// ---- 正規化 JSON ----

#[test]
fn canonical_json_sorts_keys_and_drops_whitespace() {
    let v = json!({"b": {"z": 1, "a": [true, null, "x"]}, "a": "中文\"\\\n", "c": 1.5});
    assert_eq!(
        canonical_json(&v),
        r#"{"a":"中文\"\\\n","b":{"a":[true,null,"x"],"z":1},"c":1.5}"#
    );
    // 插入順序不影響雜湊
    let mut m1 = Map::new();
    m1.insert("report".into(), json!({"interval": 10}));
    m1.insert("log".into(), json!({"level": "debug"}));
    let mut m2 = Map::new();
    m2.insert("log".into(), json!({"level": "debug"}));
    m2.insert("report".into(), json!({"interval": 10}));
    assert_eq!(settings_hash(&m1), settings_hash(&m2));
    // 空的設定（config_rev 0）
    assert_eq!(
        settings_hash(&Map::new()),
        format!("sha256:{}", crate::enroll::sha256_hex(b"{}"))
    );
}

// ---- 設定白名單 ----

#[test]
fn whitelist_is_consistent_with_remote_md() {
    const REMOTE: &str = include_str!("../../REMOTE.md");
    let start = REMOTE.find("### 3.1 可下發的設定").unwrap();
    let end = REMOTE.find("### 3.2 永遠只能在本機改的設定").unwrap();
    let table = &REMOTE[start..end];
    let mut seen = Vec::new();
    for s in SETTINGS {
        assert!(!seen.contains(&s.key), "重複的鍵 {}", s.key);
        seen.push(s.key);
        let (section, name) = s.key.rsplit_once('.').unwrap();
        let mut inner = Map::new();
        inner.insert(name.into(), json!(true));
        let mut obj = Map::new();
        obj.insert(section.into(), Value::Object(inner));
        assert!(check_shape(&obj).is_ok(), "{}", s.key);
        assert!(crate::release::Version::parse(s.min_pulse).is_ok());
        let name = s.key.rsplit_once('.').unwrap().1;
        assert!(table.contains(name), "REMOTE.md 第 3.1 節沒有 {}", s.key);
    }
    // 第 3.2 節的鍵不在白名單
    for key in [
        "server.address",
        "server.ca_cert",
        "enroll.address",
        "agent.tags",
        "identity.dir",
        "buffer.dir",
        "status.file",
        "metrics.listen",
        "metrics.textfile_interval",
        "log.format",
        "log.stdout",
        "log.timezone",
        "processes.include_user",
        "remote_config.enabled",
        "upgrade.enabled",
        "upgrade.allowed_signers",
    ] {
        assert!(setting(key).is_none(), "{key}");
    }
    // 隱私敏感的三個鍵
    for key in [
        "processes.command",
        "inventory.hardware_serials",
        "inventory.network_addresses",
    ] {
        assert_ne!(setting(key).unwrap().privacy, Privacy::None, "{key}");
    }
    // 套用方式：需要重新連線的鍵
    let reconnect: Vec<&str> = SETTINGS
        .iter()
        .filter(|s| s.apply == Apply::Reconnect)
        .map(|s| s.key)
        .collect();
    assert_eq!(
        reconnect,
        [
            "heartbeat.interval",
            "processes.enabled",
            "inventory.enabled",
            "buffer.enabled"
        ]
    );
}

#[test]
fn value_checks() {
    let check = |key: &str, v: Value| setting(key).unwrap().check_value(&v).map_err(|e| e.reason);
    assert_eq!(check("report.interval", json!(1)), Ok(()));
    assert_eq!(check("report.interval", json!(3600)), Ok(()));
    assert_eq!(check("report.interval", json!(0)), Err(ERR_OUT_OF_RANGE));
    assert_eq!(check("report.interval", json!(-5)), Err(ERR_OUT_OF_RANGE));
    assert_eq!(check("report.interval", json!(3601)), Err(ERR_OUT_OF_RANGE));
    assert_eq!(
        check("report.interval", json!(10.0)),
        Err(ERR_TYPE_MISMATCH)
    );
    assert_eq!(
        check("report.interval", json!("10")),
        Err(ERR_TYPE_MISMATCH)
    );
    assert_eq!(check("retry.max_retries", json!(u32::MAX)), Ok(()));
    assert_eq!(
        check("retry.max_retries", json!(u64::from(u32::MAX) + 1)),
        Err(ERR_OUT_OF_RANGE)
    );
    assert_eq!(check("collect.network", json!(false)), Ok(()));
    assert_eq!(check("collect.network", json!(0)), Err(ERR_TYPE_MISMATCH));
    assert_eq!(check("log.level", json!("debug")), Ok(()));
    assert_eq!(check("log.level", json!("DEBUG")), Err(ERR_INVALID_VALUE));
    assert_eq!(check("processes.command", json!("executable")), Ok(()));
    assert_eq!(
        check("processes.command", json!("all")),
        Err(ERR_INVALID_VALUE)
    );
    // full 是合法的值，但只能在本機設定
    assert_eq!(check("processes.command", json!("full")), Ok(()));
    assert!(
        setting("processes.command")
            .unwrap()
            .is_local_only(&json!("full"))
    );
    assert_eq!(
        check("collect.interface_exclude", json!(["tap*", "wg?"])),
        Ok(())
    );
    assert_eq!(
        check("collect.interface_exclude", json!(vec!["x"; 33])),
        Err(ERR_OUT_OF_RANGE)
    );
    assert_eq!(
        check("collect.interface_exclude", json!([" "])),
        Err(ERR_INVALID_VALUE)
    );
    assert_eq!(
        check("collect.interface_exclude", json!(["a", 1])),
        Err(ERR_TYPE_MISMATCH)
    );
    assert_eq!(
        check("collect.interface_exclude", json!("tap*")),
        Err(ERR_TYPE_MISMATCH)
    );
}

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[test]
fn kairos_profile_validation() {
    assert!(validate_profile(&obj(json!({"report": {"interval": 10}}))).is_ok());
    let errs = validate_profile(&obj(json!({
        "server": {"address": "evil.example"},
        "processes": {"command": "full"},
        "report": {"interval": 0},
        "retry": {"initial_delay": 30, "max_delay": 10},
    })))
    .unwrap_err();
    let reasons: Vec<(&str, &str)> = errs
        .iter()
        .map(|e| (e.key.as_str(), e.reason.as_str()))
        .collect();
    assert_eq!(
        reasons,
        [
            ("processes.command", ERR_INVALID_VALUE),
            ("report.interval", ERR_OUT_OF_RANGE),
            ("server.address", ERR_UNKNOWN_KEY),
            ("retry.max_delay", ERR_CONFLICT),
        ]
    );
    assert!(validate_profile(&obj(json!({"report": 5}))).is_err());
}

#[test]
fn pulse_remote_check() {
    let c = check_remote(&obj(json!({
        "server": {"address": "evil.example"},
        "processes": {"command": "full", "top": 3},
        "future": {"key": 1},
    })));
    assert!(c.errors.is_empty(), "{c:?}");
    assert_eq!(c.accepted, [("processes.top".to_string(), json!(3))]);
    let ignored: Vec<&str> = c.ignored.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        ignored,
        ["future.key", "processes.command", "server.address"]
    );
    assert!(c.ignored.iter().all(|i| i.reason == IGNORED_UNKNOWN_KEY));
    // 任何一個白名單鍵不合法：整份拒絕
    let c = check_remote(&obj(
        json!({"report": {"interval": "fast"}, "log": {"level": "debug"}}),
    ));
    assert_eq!(c.errors.len(), 1);
    assert_eq!(c.errors[0].reason, ERR_TYPE_MISMATCH);
    let c = check_remote(&obj(json!({"report": [1]})));
    assert_eq!(c.errors.len(), 1);
}

#[test]
fn merge_rules() {
    let s = |k: &str| setting(k).unwrap();
    fn file(v: &Value) -> Option<LocalValue<'_>> {
        Some(LocalValue {
            value: v,
            source: OVERRIDE_SOURCE_FILE,
        })
    }
    // 一般的鍵：本機優先；相同的值不列為覆蓋
    let m = merge_key(s("log.level"), &json!("info"), Some(&json!("debug")), None);
    assert_eq!(
        (m.effective, m.overridden, m.ignored),
        (json!("debug"), None, None)
    );
    let local = json!("warn");
    let m = merge_key(
        s("log.level"),
        &json!("info"),
        Some(&json!("debug")),
        file(&local),
    );
    assert_eq!(m.effective, json!("warn"));
    assert_eq!(m.overridden.unwrap().source, OVERRIDE_SOURCE_FILE);
    let local = json!("debug");
    let m = merge_key(
        s("log.level"),
        &json!("info"),
        Some(&json!("debug")),
        file(&local),
    );
    assert!(m.overridden.is_none());
    let m = merge_key(s("log.level"), &json!("info"), None, None);
    assert_eq!(m.effective, json!("info"));
    // processes.command：內建預設 none，下發 executable 不更保守 → 忽略
    let cmd = s("processes.command");
    let m = merge_key(cmd, &json!("none"), Some(&json!("executable")), None);
    assert_eq!(m.effective, json!("none"));
    assert_eq!(m.ignored.unwrap().reason, IGNORED_NOT_MORE_RESTRICTIVE);
    // 本機 full、下發 executable（更保守）→ 套用（但本機值仍優先，列為覆蓋）
    let local = json!("full");
    let m = merge_key(
        cmd,
        &json!("none"),
        Some(&json!("executable")),
        file(&local),
    );
    assert!(m.ignored.is_none());
    assert_eq!(m.effective, json!("full"));
    assert!(m.overridden.is_some());
    // 本機 none、下發 executable → 忽略（本機值更保守）
    let local = json!("none");
    let m = merge_key(
        cmd,
        &json!("executable"),
        Some(&json!("executable")),
        file(&local),
    );
    assert_eq!(m.ignored.unwrap().reason, IGNORED_NOT_MORE_RESTRICTIVE);
    // 資產序號：只能下發 false；true 只在本機已是 true 時視為相同
    let serials = s("inventory.hardware_serials");
    let m = merge_key(serials, &json!(true), Some(&json!(false)), None);
    assert_eq!(m.effective, json!(false));
    let m = merge_key(serials, &json!(true), Some(&json!(true)), None);
    assert_eq!((m.effective, m.ignored), (json!(true), None));
    let local = json!(false);
    let m = merge_key(serials, &json!(true), Some(&json!(true)), file(&local));
    assert_eq!(m.effective, json!(false));
    assert_eq!(m.ignored.unwrap().reason, IGNORED_NOT_MORE_RESTRICTIVE);
    // interface_exclude：聯集，本機在前、不重複、不算覆蓋
    let ex = s("collect.interface_exclude");
    let local = json!(["tap*", "wg?"]);
    let m = merge_key(ex, &json!([]), Some(&json!(["wg?", "vpn*"])), file(&local));
    assert_eq!(m.effective, json!(["tap*", "wg?", "vpn*"]));
    assert!(m.overridden.is_none() && m.ignored.is_none());
    let m = merge_key(ex, &json!([]), Some(&json!(["vpn*"])), None);
    assert_eq!(m.effective, json!(["vpn*"]));
}

#[test]
fn reconnect_overlay_and_version_filter() {
    assert!(needs_reconnect(&["report.interval", "heartbeat.interval"]));
    assert!(!needs_reconnect(&[
        "report.interval",
        "log.level",
        "nope.x"
    ]));
    let global = obj(json!({"report": {"interval": 10}, "log": {"level": "info"}}));
    let host = obj(json!({"log": {"level": "debug"}}));
    let merged = overlay([&global, &host]);
    assert_eq!(
        Value::Object(merged.clone()),
        json!({"report": {"interval": 10}, "log": {"level": "debug"}})
    );
    let with_unknown = overlay([&merged, &obj(json!({"future": {"x": 1}}))]);
    assert_eq!(filter_for_version(&with_unknown, "0.9.0"), merged);
    assert_eq!(filter_for_version(&with_unknown, "0.9.0-test.1"), merged);
    assert_eq!(filter_for_version(&with_unknown, "1.2.3"), merged);
    assert!(filter_for_version(&with_unknown, "0.8.0").is_empty());
    assert!(filter_for_version(&with_unknown, "dev").is_empty());
    // flatten／nest 互為反函式
    let flat = flatten(&merged).unwrap();
    assert_eq!(
        flat.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        ["log.level", "report.interval"]
    );
    assert_eq!(nest(flat.into_iter().map(|(k, v)| (k, v.clone()))), merged);
    assert_eq!(get(&merged, "log.level"), Some(&json!("debug")));
    assert_eq!(get(&merged, "log.nope"), None);
    for bad in [
        json!({"Log": {}}),
        json!({"log": {"Level": 1}}),
        json!({"log.": {"x": 1}}),
        json!({"a..b": {"x": 1}}),
        json!({"1log": {"x": 1}}),
        json!({"log": []}),
    ] {
        assert!(check_shape(&obj(bad.clone())).is_err(), "{bad}");
    }
    assert!(check_shape(&obj(json!({"log.syslog": {"target": ""}}))).is_ok());
}
