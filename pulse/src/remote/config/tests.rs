//! 集中設定（P1）的測試：合併順序、隱私規則、整份拒絕、快取與重新啟動、確認期與回復、崩潰保護、
//! 本機停用與舊版 Kairos。

use super::*;
use kairos_proto::mgmt::settings::SETTINGS;
use serde_json::json;
use std::time::Duration;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pulse-cfg-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn settings(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn remote(rev: u64, v: Value) -> RemoteConfig {
    let mut sources = BTreeMap::new();
    for (k, _) in settings::flatten(&settings(v.clone())).unwrap() {
        sources.insert(k, "正式環境 Linux".to_string());
    }
    RemoteConfig::new(
        rev,
        rev,
        settings(v),
        Some(sources),
        "2026-11-20T02:00:00.025Z",
    )
}

fn defaults() -> AppConfig {
    AppConfig::embedded_defaults()
}

fn pending(rev: u64) -> Pending {
    Pending {
        config_rev: Some(rev),
        upgrade_id: None,
    }
}

#[test]
fn every_whitelisted_key_maps_to_an_app_config_field() {
    // kairos-proto 的白名單與 Pulse 的設定欄位必須一一對應（白名單新增鍵時這裡會失敗）
    let d = defaults();
    for spec in SETTINGS {
        let v = get_value(&d, spec.key).unwrap_or_else(|| panic!("{} 沒有對應的欄位", spec.key));
        spec.check_value(&v)
            .or_else(|e| {
                if spec.is_local_only(&v) {
                    Ok(())
                } else {
                    Err(e)
                }
            })
            .unwrap_or_else(|e| panic!("{} 的預設值不在白名單的範圍內：{e:?}", spec.key));
        let mut c = d.clone();
        set_value(&mut c, spec.key, &v).unwrap();
        assert_eq!(get_value(&c, spec.key), Some(v), "{}", spec.key);
    }
    assert!(get_value(&d, "server.address").is_none());
    assert!(set_value(&mut d.clone(), "server.address", &json!("x")).is_err());
}

#[test]
fn merge_order_is_default_remote_file_env() {
    let mut local = defaults();
    local.report.interval = 5; // pulse.toml
    local.log.level = "warn".into(); // 環境變數
    let sources = LocalSources::from_pairs(&[
        ("report.interval", mgmt::OVERRIDE_SOURCE_FILE),
        ("log.level", mgmt::OVERRIDE_SOURCE_ENV),
    ]);
    let r = settings(json!({
        "report": {"interval": 10},
        "log": {"level": "debug"},
        "processes": {"top": 10},
        "heartbeat": {"interval": 15}
    }));
    let m = merge(&local, &defaults(), &sources, Some(&r), None);
    assert!(m.errors.is_empty(), "{:?}", m.errors);
    // 本機優先
    assert_eq!(m.config.report.interval, 5);
    assert_eq!(m.config.log.level, "warn");
    // 沒有本機值的鍵用下發值
    assert_eq!(m.config.processes.top, 10);
    assert_eq!(
        settings::get(&m.effective, "processes.top"),
        Some(&json!(10))
    );
    // 白名單的每個鍵都在 effective 中
    assert_eq!(
        settings::flatten(&m.effective).unwrap().len(),
        SETTINGS.len()
    );
    // 被覆蓋的鍵：本機值與下發值不同才列；heartbeat.interval 下發值等於預設、沒有本機值，不算覆蓋
    let keys: Vec<(&str, &str)> = m
        .overridden
        .iter()
        .map(|o| (o.key.as_str(), o.source.as_str()))
        .collect();
    assert_eq!(
        keys,
        [("report.interval", "pulse.toml"), ("log.level", "env")]
    );
    assert_eq!(m.overridden[0].remote, json!(10));
    assert_eq!(m.overridden[0].local, json!(5));
    assert_eq!(m.sources["report.interval"], "pulse.toml");
    assert_eq!(m.sources["processes.top"], "remote:");
    assert_eq!(m.sources["collect.network"], "default");
    // 本機值與下發值相同時不列
    let same = settings(json!({"report": {"interval": 5}}));
    assert!(
        merge(&local, &defaults(), &sources, Some(&same), None)
            .overridden
            .is_empty()
    );
}

#[test]
fn interface_exclude_is_a_union_and_privacy_keys_only_accept_more_conservative_values() {
    let mut local = defaults();
    local.collect.interface_exclude = vec!["tap*".into()];
    let sources =
        LocalSources::from_pairs(&[("collect.interface_exclude", mgmt::OVERRIDE_SOURCE_FILE)]);
    let r = settings(json!({
        "collect": {"interface_exclude": ["wg?", "tap*"]},
        "processes": {"command": "executable"},
        "inventory": {"hardware_serials": false, "network_addresses": true}
    }));
    let m = merge(&local, &defaults(), &sources, Some(&r), None);
    assert!(m.errors.is_empty(), "{:?}", m.errors);
    assert_eq!(m.config.collect.interface_exclude, ["tap*", "wg?"]);
    assert!(m.overridden.is_empty(), "聯集不算覆蓋");
    // 內建預設 command = none：下發 executable 不更保守 → 忽略
    assert_eq!(m.config.processes.command, "none");
    // hardware_serials 下發 false（更保守）→ 套用；network_addresses 下發 true 而預設也是 true → 相同，套用
    assert!(!m.config.inventory.hardware_serials);
    assert!(m.config.inventory.network_addresses);
    let ignored: Vec<(&str, &str)> = m
        .ignored
        .iter()
        .map(|i| (i.key.as_str(), i.reason.as_str()))
        .collect();
    assert_eq!(
        ignored,
        [("processes.command", mgmt::IGNORED_NOT_MORE_RESTRICTIVE)]
    );

    // 本機關閉了序號：下發 true 被忽略
    let mut local = defaults();
    local.inventory.hardware_serials = false;
    let sources =
        LocalSources::from_pairs(&[("inventory.hardware_serials", mgmt::OVERRIDE_SOURCE_FILE)]);
    let r = settings(json!({"inventory": {"hardware_serials": true}}));
    let m = merge(&local, &defaults(), &sources, Some(&r), None);
    assert!(!m.config.inventory.hardware_serials);
    assert_eq!(m.ignored[0].reason, mgmt::IGNORED_NOT_MORE_RESTRICTIVE);
}

#[test]
fn unknown_and_local_only_keys_are_ignored_but_invalid_values_reject_everything() {
    let local = defaults();
    let s = LocalSources::default();
    let r = settings(json!({
        "server": {"address": "evil.example.com"},
        "processes": {"command": "full"},
        "report": {"interval": 10}
    }));
    let m = merge(&local, &defaults(), &s, Some(&r), None);
    assert!(m.errors.is_empty());
    assert_eq!(m.config.server.address, local.server.address);
    assert_eq!(m.config.report.interval, 10);
    let ignored: Vec<&str> = m.ignored.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(ignored, ["processes.command", "server.address"]);
    assert!(
        m.ignored
            .iter()
            .all(|i| i.reason == mgmt::IGNORED_UNKNOWN_KEY)
    );

    // 任何一個白名單鍵的值不合法：整份拒絕，生效的仍是本機設定
    let r = settings(json!({"report": {"interval": 0}, "log": {"level": "debug"}}));
    let m = merge(&local, &defaults(), &s, Some(&r), None);
    assert_eq!(m.errors[0].key, "report.interval");
    assert_eq!(m.errors[0].reason, settings::ERR_OUT_OF_RANGE);
    assert_eq!(m.config.log.level, local.log.level);
    assert_eq!(
        settings::get(&m.effective, "log.level"),
        Some(&json!("info"))
    );

    // 鍵之間矛盾（下發的兩個鍵）
    let r = settings(json!({"retry": {"initial_delay": 30, "max_delay": 10}}));
    let m = merge(&local, &defaults(), &s, Some(&r), None);
    assert_eq!(m.errors[0].reason, settings::ERR_CONFLICT);

    // 與本機的值矛盾：合併後的完整驗證（AppConfig::validate）
    let mut local2 = defaults();
    local2.retry.max_delay = 60;
    let s2 = LocalSources::from_pairs(&[("retry.max_delay", mgmt::OVERRIDE_SOURCE_FILE)]);
    let r = settings(json!({"retry": {"initial_delay": 120}}));
    let m = merge(&local2, &defaults(), &s2, Some(&r), None);
    assert_eq!(m.errors.len(), 1, "{:?}", m.errors);
    assert_eq!(m.errors[0].reason, settings::ERR_CONFLICT);
    assert!(m.errors[0].key.starts_with("retry."), "{:?}", m.errors);
    assert!(m.errors[0].message.len() <= mgmt::MAX_STATUS_MESSAGE_BYTES);
}

#[test]
fn local_sources_come_from_pulse_toml_and_env() {
    let dir = tmp("sources");
    let file = dir.join("pulse.toml");
    std::fs::write(
        &file,
        "[report]\ninterval = 5\n[log]\nlevel = \"debug\"\n[server]\naddress = \"k\"\n",
    )
    .unwrap();
    let mut env = config::Map::new();
    env.insert("PULSE__LOG__LEVEL".to_string(), "warn".to_string());
    env.insert("PULSE__HEARTBEAT__INTERVAL".to_string(), "20".to_string());
    let s = LocalSources::load(Some(&file), Some(env));
    assert_eq!(
        s.source("report.interval"),
        Some(mgmt::OVERRIDE_SOURCE_FILE)
    );
    assert_eq!(
        s.source("log.level"),
        Some(mgmt::OVERRIDE_SOURCE_ENV),
        "環境變數優先"
    );
    assert_eq!(
        s.source("heartbeat.interval"),
        Some(mgmt::OVERRIDE_SOURCE_ENV)
    );
    assert_eq!(s.source("collect.network"), None);
    assert_eq!(s.source("server.address"), None, "不在白名單的鍵不記");
    // 沒有檔案
    assert_eq!(
        LocalSources::load(Some(&dir.join("missing.toml")), Some(config::Map::new())),
        LocalSources::default()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn central(dir: &Path, local: AppConfig, sources: LocalSources) -> Arc<CentralConfig> {
    CentralConfig::new(local, sources, Some(dir.join(CACHE_RELATIVE)))
}

#[test]
fn applying_writes_the_cache_and_survives_a_restart() {
    let dir = tmp("apply");
    let mut local = defaults();
    local.report.interval = 5;
    let src = LocalSources::from_pairs(&[("report.interval", mgmt::OVERRIDE_SOURCE_FILE)]);
    let c = central(&dir, local.clone(), src.clone());
    assert!(c.declare());
    assert_eq!(c.install_state(), "enabled");
    // 第一條連線：pending 42 ≠ 0 → config_get
    let reg = c.on_registered(true, Some(&pending(42)));
    assert!(reg.send_status && reg.fetch && !reg.reconnect);
    let st = c.status_message(2);
    assert_eq!(
        (st.rev, st.hash.as_str(), st.state.as_str()),
        (0, "", "applied")
    );
    mgmt::validate_config_status(&st).unwrap();

    let mut live = c.subscribe();
    let msg = remote(
        42,
        json!({"report": {"interval": 10}, "log": {"level": "debug"}, "processes": {"top": 10}}),
    );
    let o = c.apply(&msg);
    assert!(o.accepted_message && !o.reconnect, "{o:?}");
    assert!(live.has_changed().unwrap());
    let eff = live.borrow_and_update().clone();
    assert_eq!(eff.log.level, "debug");
    assert_eq!(eff.processes.top, 10);
    assert_eq!(eff.report.interval, 5);
    let st = c.status_message(3);
    assert_eq!(st.state, "applied");
    assert_eq!(st.rev, 42);
    assert_eq!(st.hash, msg.hash);
    assert_eq!(st.code.as_deref(), Some(codes::I_REMOTE_CONFIG_OVERRIDDEN));
    assert_eq!(st.overridden.len(), 1);
    mgmt::validate_config_status(&st).unwrap();
    assert_eq!(c.applied_rev(), 42);

    // 快取：目錄 0750、檔案 0640，內容可解析且雜湊一致
    let path = dir.join(CACHE_RELATIVE);
    let cache = CacheFile::load(&path).unwrap().unwrap();
    assert_eq!(cache.applied.as_ref().unwrap().rev, 42);
    assert_eq!(
        cache.pending_apply,
        Some(PendingApply { rev: 42, starts: 0 })
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
    }
    // 即時套用：確認期內沒有斷線 → last-known-good
    c.on_tick(Instant::now() + Duration::from_secs(61));
    let cache = CacheFile::load(&path).unwrap().unwrap();
    assert_eq!(cache.last_known_good.as_ref().unwrap().rev, 42);
    assert_eq!(cache.pending_apply, None);

    // 重新啟動：以快取運作（不退回內建預設），同一版不再 config_get
    let c2 = central(&dir, local, src);
    assert_eq!(c2.effective().log.level, "debug");
    assert_eq!(c2.applied_rev(), 42);
    let reg = c2.on_registered(true, Some(&pending(42)));
    assert!(reg.send_status && !reg.fetch);
    let st = c2.status_message(2);
    assert_eq!((st.rev, st.state.as_str()), (42, "applied"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_hash_is_ignored_and_tampered_cache_falls_back_to_local() {
    let dir = tmp("hash");
    let c = central(&dir, defaults(), LocalSources::default());
    let mut msg = remote(7, json!({"log": {"level": "debug"}}));
    msg.hash = format!("sha256:{}", "0".repeat(64));
    let o = c.apply(&msg);
    assert!(!o.accepted_message, "雜湊不符：當作沒有收到");
    assert_eq!(c.effective().log.level, "info");

    // 竄改快取內容（雜湊不再一致）→ 啟動時忽略快取、用本機設定
    let ok = remote(8, json!({"log": {"level": "debug"}}));
    assert!(c.apply(&ok).accepted_message);
    let path = dir.join(CACHE_RELATIVE);
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\"debug\"", "\"trace\"");
    std::fs::write(&path, text).unwrap();
    let c2 = central(&dir, defaults(), LocalSources::default());
    assert_eq!(c2.effective().log.level, "info");
    assert_eq!(c2.applied_rev(), 0);
    assert!(c2.view().cache_error.is_some());
    // 損毀（不是 JSON）
    std::fs::write(&path, "{not json").unwrap();
    assert_eq!(
        central(&dir, defaults(), LocalSources::default()).applied_rev(),
        0
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejected_config_keeps_the_previous_version() {
    let dir = tmp("reject");
    let c = central(&dir, defaults(), LocalSources::default());
    assert!(
        c.apply(&remote(1, json!({"report": {"interval": 10}})))
            .accepted_message
    );
    let o = c.apply(&remote(
        2,
        json!({"report": {"interval": 9999}, "log": {"level": "debug"}}),
    ));
    assert!(o.accepted_message && !o.reconnect);
    assert_eq!(c.effective().report.interval, 10, "沿用第 1 版");
    assert_eq!(c.effective().log.level, "info");
    let st = c.status_message(5);
    assert_eq!((st.rev, st.state.as_str()), (2, "rejected"));
    assert_eq!(st.code.as_deref(), Some(codes::W_REMOTE_CONFIG_REJECTED));
    assert_eq!(st.errors[0].key, "report.interval");
    assert!(st.overridden.is_empty());
    mgmt::validate_config_status(&st).unwrap();
    assert_eq!(c.applied_rev(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reconnect_keys_and_confirmation_by_hello_ack() {
    let dir = tmp("reconnect");
    let c = central(&dir, defaults(), LocalSources::default());
    c.on_registered(true, Some(&pending(3)));
    let o = c.apply(&remote(
        3,
        json!({"heartbeat": {"interval": 30}, "report": {"interval": 10}}),
    ));
    assert!(o.reconnect, "heartbeat.interval 需要重新連線");
    assert!(c.confirming());
    let o = c.apply(&remote(
        4,
        json!({"heartbeat": {"interval": 30}, "report": {"interval": 20}}),
    ));
    assert!(!o.reconnect, "生效值沒有改變的鍵不觸發重連");
    // 重連後收到 hello_ack → 確認
    c.on_disconnected();
    c.on_registered(true, Some(&pending(4)));
    assert!(!c.confirming());
    let cache = CacheFile::load(&dir.join(CACHE_RELATIVE)).unwrap().unwrap();
    assert_eq!(cache.last_known_good.unwrap().rev, 4);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn three_failed_connections_roll_back_to_last_known_good() {
    let dir = tmp("rollback");
    let c = central(&dir, defaults(), LocalSources::default());
    c.on_registered(true, Some(&pending(1)));
    assert!(
        c.apply(&remote(1, json!({"report": {"interval": 10}})))
            .accepted_message
    );
    c.on_tick(Instant::now() + Duration::from_secs(120)); // 第 1 版確認
    let o = c.apply(&remote(
        2,
        json!({"processes": {"enabled": false}, "report": {"interval": 30}}),
    ));
    assert!(o.reconnect);
    c.on_disconnected();
    assert!(!c.on_connect_failed());
    assert!(!c.on_connect_failed());
    assert!(c.on_connect_failed(), "第 3 次失敗：回復");
    let eff = c.effective();
    assert_eq!(eff.report.interval, 10);
    assert!(eff.processes.enabled);
    assert_eq!(c.applied_rev(), 1);
    // 連上後回報 rolled_back（失敗的版本）
    let reg = c.on_registered(true, Some(&pending(2)));
    assert!(reg.send_status);
    assert!(!reg.fetch, "同一個 rev 不再自動套用");
    let st = c.status_message(2);
    assert_eq!((st.rev, st.state.as_str()), (2, "rolled_back"));
    assert_eq!(st.code.as_deref(), Some(codes::W_REMOTE_CONFIG_ROLLED_BACK));
    assert!(mgmt::is_config_hash(&st.hash));
    mgmt::validate_config_status(&st).unwrap();
    c.on_status_acked("rolled_back");
    let st = c.status_message(3);
    assert_eq!((st.rev, st.state.as_str()), (1, "applied"));
    // 新的 rev 才再試
    assert_eq!(c.on_pending(&pending(2)), (false, false, false));
    assert_eq!(c.on_pending(&pending(3)), (true, false, false));
    // 重新啟動後仍記得
    let c2 = central(&dir, defaults(), LocalSources::default());
    assert_eq!(c2.applied_rev(), 1);
    assert!(!c2.on_registered(true, Some(&pending(2))).fetch);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crash_protection_rolls_back_after_two_starts_in_the_confirmation_period() {
    let dir = tmp("crash");
    let c = central(&dir, defaults(), LocalSources::default());
    assert!(
        c.apply(&remote(1, json!({"log": {"level": "warn"}})))
            .accepted_message
    );
    c.on_tick(Instant::now() + Duration::from_secs(120));
    assert!(
        c.apply(&remote(2, json!({"log": {"level": "debug"}})))
            .accepted_message
    );
    drop(c);
    // 第 1 次重新啟動（確認前）：仍用第 2 版
    let c = central(&dir, defaults(), LocalSources::default());
    assert_eq!(c.effective().log.level, "debug");
    drop(c);
    // 第 2 次：改以 last-known-good 啟動並回報 rolled_back
    let c = central(&dir, defaults(), LocalSources::default());
    assert_eq!(c.effective().log.level, "warn");
    assert_eq!(c.applied_rev(), 1);
    c.on_registered(true, Some(&pending(2)));
    let st = c.status_message(2);
    assert_eq!((st.rev, st.state.as_str()), (2, "rolled_back"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disabled_locally_reports_once_and_ignores_the_cache() {
    let dir = tmp("disabled");
    let c = central(&dir, defaults(), LocalSources::default());
    assert!(
        c.apply(&remote(5, json!({"log": {"level": "debug"}})))
            .accepted_message
    );
    drop(c);
    let mut local = defaults();
    local.remote_config.enabled = false;
    let c = central(&dir, local.clone(), LocalSources::default());
    assert_eq!(c.install_state(), "disabled");
    assert_eq!(c.effective().log.level, "info", "停用時不使用快取");
    assert!(c.declare(), "停用後第一次連線宣告一次");
    let reg = c.on_registered(true, Some(&pending(5)));
    assert!(reg.send_status && !reg.fetch);
    let st = c.status_message(2);
    assert_eq!((st.rev, st.state.as_str()), (5, "disabled"));
    assert_eq!(st.code.as_deref(), Some(codes::I_REMOTE_CONFIG_DISABLED));
    mgmt::validate_config_status(&st).unwrap();
    // 不套用 config
    assert!(
        !c.apply(&remote(6, json!({"log": {"level": "trace"}})))
            .accepted_message
    );
    c.on_status_acked("disabled");
    assert!(!c.declare());
    // 重新啟動仍不宣告；重新啟用後恢復，下次停用再通知一次
    assert!(!central(&dir, local, LocalSources::default()).declare());
    let c = central(&dir, defaults(), LocalSources::default());
    assert!(c.declare());
    assert_eq!(c.effective().log.level, "debug", "重新啟用後沿用快取");
    let cache = CacheFile::load(&dir.join(CACHE_RELATIVE)).unwrap().unwrap();
    assert!(!cache.disabled_reported);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kairos_without_config_uses_local_settings_until_it_comes_back() {
    let dir = tmp("old-kairos");
    let c = central(&dir, defaults(), LocalSources::default());
    assert!(
        c.apply(&remote(
            1,
            json!({"processes": {"enabled": false}, "log": {"level": "debug"}})
        ))
        .accepted_message
    );
    c.on_tick(Instant::now() + Duration::from_secs(120));
    // 連上不支援 config 的 Kairos：快取保留但不使用；processes.enabled 改回 → 需要重新連線
    let reg = c.on_registered(false, None);
    assert!(!reg.send_status && reg.reconnect);
    assert_eq!(c.effective().log.level, "info");
    assert!(c.effective().processes.enabled);
    assert_eq!(c.view().usage, "unused");
    // 再連上支援的 Kairos：恢復
    let reg = c.on_registered(true, Some(&pending(1)));
    assert!(reg.send_status && reg.reconnect && !reg.fetch);
    assert_eq!(c.effective().log.level, "debug");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pending_zero_returns_to_local_settings() {
    let dir = tmp("zero");
    let c = central(&dir, defaults(), LocalSources::default());
    assert!(
        c.apply(&remote(9, json!({"log": {"level": "debug"}})))
            .accepted_message
    );
    let (fetch, status, reconnect) = c.on_pending(&pending(0));
    assert!(!fetch && status && !reconnect);
    assert_eq!(c.effective().log.level, "info");
    assert_eq!(c.applied_rev(), 0);
    let st = c.status_message(4);
    assert_eq!(
        (st.rev, st.hash.as_str(), st.state.as_str()),
        (0, "", "applied")
    );
    mgmt::validate_config_status(&st).unwrap();
    // 沒有下發時 pending 0 什麼都不做
    assert_eq!(c.on_pending(&pending(0)), (false, false, false));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unwritable_cache_rejects_with_e0506() {
    let dir = tmp("unwritable");
    // 快取目錄的位置是一個檔案：無法建立
    std::fs::write(dir.join("remote"), "x").unwrap();
    let c = central(&dir, defaults(), LocalSources::default());
    let o = c.apply(&remote(1, json!({"log": {"level": "debug"}})));
    assert!(o.accepted_message);
    assert_eq!(c.effective().log.level, "info", "無法寫入快取：不套用");
    let st = c.status_message(2);
    assert_eq!(st.state, "rejected");
    assert_eq!(st.code.as_deref(), Some(codes::E_REMOTE_CONFIG_CACHE));
    mgmt::validate_config_status(&st).unwrap();
    assert!(c.view().cache_error.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replacing_the_local_config_keeps_the_remote_layer() {
    let dir = tmp("replace");
    let c = central(&dir, defaults(), LocalSources::default());
    assert!(
        c.apply(&remote(1, json!({"log": {"level": "debug"}})))
            .accepted_message
    );
    let mut local = defaults();
    local.server.address = "kairos.example.com".into();
    let eff = c.replace_local(local, LocalSources::default());
    assert_eq!(eff.server.address, "kairos.example.com");
    assert_eq!(eff.log.level, "debug");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn effective_from_cache_for_commands() {
    let dir = tmp("show");
    let c = central(&dir, defaults(), LocalSources::default());
    assert!(
        c.apply(&remote(4, json!({"report": {"interval": 30}})))
            .accepted_message
    );
    let (m, applied) = effective_from_cache(
        &defaults(),
        &LocalSources::default(),
        &dir.join(CACHE_RELATIVE),
    )
    .unwrap();
    assert_eq!(applied.unwrap().rev, 4);
    assert_eq!(m.config.report.interval, 30);
    assert_eq!(m.sources["report.interval"], "remote:正式環境 Linux");
    // 本機停用時不使用
    let mut local = defaults();
    local.remote_config.enabled = false;
    let (m, applied) =
        effective_from_cache(&local, &LocalSources::default(), &dir.join(CACHE_RELATIVE)).unwrap();
    assert!(applied.is_none());
    assert_eq!(m.config.report.interval, 5);
    let _ = std::fs::remove_dir_all(&dir);
}
