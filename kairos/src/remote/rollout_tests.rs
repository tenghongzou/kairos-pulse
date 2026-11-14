//! 推送的內容、資格分類與推送引擎（純函式）的測試
use super::*;
use serde_json::json;

fn host(id: u8, version: &str, method: &str, upgrade: &str) -> UpgradeHost {
    UpgradeHost {
        host_id: i64::from(id),
        agent_id: format!("00000000-0000-4000-8000-0000000000{id:02x}"),
        hostname: format!("web-{id:02}"),
        agent_version: Some(version.to_string()),
        os: Some(r#"{"name":"Ubuntu","arch":"x86_64"}"#.to_string()),
        install_method: Some(method.to_string()),
        install_upgrade: Some(upgrade.to_string()),
        install_config: Some("enabled".into()),
        install_external_url: None,
        install_rollback: None,
        host_group: Some("Web".into()),
        tags: vec!["env:prod".into()],
        online: true,
        decommissioned: false,
    }
}

fn artifact(version: &str, format: &str) -> ArtifactRow {
    ArtifactRow {
        release_id: 1,
        filename: format!("pulse-{version}-abcdef1-linux-x86_64.{format}"),
        os: "linux".into(),
        arch: "x86_64".into(),
        format: format.into(),
        sha256: "0".repeat(64),
        size: 100,
        stored: true,
        stored_at: Some(1),
    }
}

fn manifest(version: &str, min_from: &str) -> ReleaseManifest {
    ReleaseManifest::parse(&format!(
        r#"{{"schema":"kairos-pulse.release/v1","product":"pulse","version":"{version}","commit":"abcdef1","released_at":"2026-11-18","min_kairos":"0.5.0","min_upgrade_from":"{min_from}","remote_features":["config","upgrade"],"breaking":false}}"#
    ))
    .unwrap()
}

fn ctx(target: &str) -> ReleaseContext {
    let mut by_version = HashMap::new();
    by_version.insert(
        target.to_string(),
        vec![artifact(target, "tar.gz"), artifact(target, "deb")],
    );
    by_version.insert("0.9.0".to_string(), vec![artifact("0.9.0", "deb")]);
    ReleaseContext {
        target: Version::parse(target).unwrap(),
        manifest: Some(manifest(target, "0.9.0")),
        artifacts: vec![artifact(target, "tar.gz"), artifact(target, "deb")],
        by_version,
        missing: Vec::new(),
        external_base_url: None,
    }
}

/// 外部 URL 的版本（REMOTE.md 第 4.1.2 節）：[`ctx`] 加上 `base_url`
fn external_ctx(target: &str) -> ReleaseContext {
    ReleaseContext {
        external_base_url: Some(format!(
            "https://repo.example.gov.tw/kairos-pulse/{target}/"
        )),
        ..ctx(target)
    }
}

fn declares(mut h: UpgradeHost, external_url: Option<&str>) -> UpgradeHost {
    h.install_external_url = external_url.map(str::to_string);
    h
}

/// 外部 URL 版本的資格分類（REMOTE.md 第 4.7.2 節）：宣告 `install.external_url = "none"` 的主機事先分類為
/// `url_not_allowed`；Pulse 低於 0.10.0（不支援外部 URL）分類為 `url_not_supported`；0.10.0 以上而沒有宣告、
/// 宣告 `allowlist`、不認得的值照舊算可升級
#[test]
fn external_url_declaration_is_classified_only_for_external_releases() {
    let o = RolloutOptions::default();
    let ext = external_ctx("0.10.2");
    let why = |h: &UpgradeHost, c: &ReleaseContext| classify(h, c, &o);
    let ready = || host(1, "0.10.0", "tar", "ready");
    assert_eq!(SKIP_URL_NOT_ALLOWED, "url_not_allowed");
    // 不允許（白名單是空的）
    let e = why(&declares(ready(), Some("none")), &ext);
    assert_eq!(e.skip, Some(SKIP_URL_NOT_ALLOWED));
    // 允許（有白名單；內容 Kairos 不知道）
    let e = why(&declares(ready(), Some("allowlist")), &ext);
    assert_eq!((e.skip, e.external_url), (None, ExternalUrl::Allowlist));
    // 未知：0.10.0、0.10.1 的 Pulse 沒有宣告、或不認得的值——現行行為（可升級，不允許的在推送中 declined）
    for unknown in [None, Some("per_host"), Some("")] {
        let e = why(&declares(ready(), unknown), &ext);
        assert_eq!(
            (e.skip, e.external_url),
            (None, ExternalUrl::Undeclared),
            "{unknown:?}"
        );
    }
    // 目標由 Kairos 代管：不看宣告（提議不含外部 URL）
    for declared in [None, Some("none"), Some("allowlist")] {
        let e = why(&declares(ready(), declared), &ctx("0.10.2"));
        assert_eq!(
            (e.skip, e.external_url),
            (None, ExternalUrl::NotApplicable),
            "{declared:?}"
        );
    }
    // 順序：install.upgrade、平台、發佈檔的原因優先；降版、min_upgrade_from、回滾在後
    assert_eq!(
        why(
            &declares(host(1, "0.10.0", "tar", "no_helper"), Some("none")),
            &ext
        )
        .skip,
        Some("no_helper")
    );
    assert_eq!(
        why(
            &declares(host(1, "0.10.2", "tar", "ready"), Some("none")),
            &ext
        )
        .skip,
        Some("already")
    );
    assert_eq!(
        why(
            &declares(host(1, "0.10.0", "rpm", "ready"), Some("none")),
            &ext
        )
        .skip,
        Some("no_artifact")
    );
    assert_eq!(
        why(
            &declares(host(1, "0.10.3", "tar", "ready"), Some("none")),
            &ext
        )
        .skip,
        Some(SKIP_URL_NOT_ALLOWED)
    );
    assert_eq!(
        why(
            &declares(host(1, "0.10.3", "tar", "ready"), Some("allowlist")),
            &ext
        )
        .skip,
        Some("downgrade_not_allowed")
    );
    // Pulse 低於 0.10.0：不支援外部 URL（0.9 一律 declined）→ url_not_supported。版本的判斷先於宣告
    // （0.9 不會宣告；即使帶了欄位也一樣），install.upgrade、平台、發佈檔的原因仍然優先
    assert_eq!(SKIP_URL_NOT_SUPPORTED, "url_not_supported");
    for declared in [None, Some("none"), Some("allowlist"), Some("future")] {
        for version in ["0.9.0", "0.9.5", "0.10.0-rc.1"] {
            let e = why(&declares(host(1, version, "tar", "ready"), declared), &ext);
            let want = if version.starts_with("0.9") {
                Some(SKIP_URL_NOT_SUPPORTED)
            } else if declared == Some("none") {
                Some(SKIP_URL_NOT_ALLOWED)
            } else {
                None
            };
            assert_eq!(e.skip, want, "{version} {declared:?}");
        }
    }
    assert_eq!(
        why(&host(1, "0.9.0", "tar", "no_helper"), &ext).skip,
        Some("no_helper")
    );
    assert_eq!(
        why(&host(1, "0.9.0", "rpm", "ready"), &ext).skip,
        Some("no_artifact")
    );
    // 由 Kairos 代管的版本：0.9 的主機照常可升級（不看外部 URL）
    assert_eq!(
        why(&host(1, "0.9.0", "tar", "ready"), &ctx("0.10.2")).skip,
        None
    );
    // 回滾檔的規則不變：deb 沒有目前版本的回滾檔時仍是 no_rollback（宣告 allowlist 的主機）
    let mut ext = ext;
    ext.by_version
        .insert("0.10.0".to_string(), vec![artifact("0.10.0", "deb")]);
    let why = |h: &UpgradeHost, c: &ReleaseContext| classify(h, c, &o);
    let mut no_rb = ext.clone();
    no_rb.by_version.remove("0.10.0");
    let deb = declares(host(1, "0.10.0", "deb", "ready"), Some("allowlist"));
    assert_eq!(why(&deb, &no_rb).skip, Some("no_rollback"));
    assert_eq!(why(&deb, &ext).skip, None);

    // 預覽的統計：url_not_allowed 的台數與樣本，可升級主機中沒有宣告與宣告 allowlist 的台數
    let hosts = vec![
        declares(host(1, "0.10.0", "tar", "ready"), Some("none")),
        declares(host(2, "0.10.0", "tar", "ready"), Some("none")),
        declares(host(3, "0.10.0", "tar", "ready"), Some("allowlist")),
        declares(host(4, "0.10.0", "tar", "ready"), None),
        declares(host(5, "0.10.0", "tar", "ready"), None),
        declares(host(6, "0.10.0", "tar", "ready"), Some("future")),
        // 0.9 的 Pulse：不支援外部 URL（不計入宣告的統計）
        host(8, "0.9.0", "tar", "ready"),
        declares(host(9, "0.9.3", "tar", "ready"), Some("allowlist")),
        // 不可升級的主機不計入宣告的統計
        declares(host(7, "0.10.0", "tar", "disabled"), None),
    ];
    let all = SelectorSpec {
        all: true,
        ..Default::default()
    };
    let waves = WaveSpec::default_template();
    let (p, classified, assigned) = plan("r", &hosts, &all, &waves, &o, &ext, &HashMap::new());
    assert_eq!((p.matched, p.eligible), (9, 4));
    assert_eq!(p.skipped.get(SKIP_URL_NOT_SUPPORTED), Some(&2));
    assert_eq!(p.samples[SKIP_URL_NOT_SUPPORTED].len(), 2);
    assert_eq!(p.skipped.get(SKIP_URL_NOT_ALLOWED), Some(&2));
    assert_eq!(p.samples[SKIP_URL_NOT_ALLOWED].len(), 2);
    assert_eq!(
        p.external_url,
        Some(ExternalUrlPreview {
            base_url: "https://repo.example.gov.tw/kairos-pulse/0.10.2/".into(),
            undeclared: 3,
            allowlist: 1,
        })
    );
    assert_eq!(assigned.len(), 4);
    assert_eq!(
        classified
            .iter()
            .filter(|(_, e)| e.skip == Some(SKIP_URL_NOT_ALLOWED))
            .count(),
        2
    );
    let j = p.json();
    assert_eq!(j["skipped"]["url_not_allowed"], 2);
    assert_eq!(j["skipped"]["url_not_supported"], 2);
    assert_eq!(
        j["external_url"],
        json!({"base_url": "https://repo.example.gov.tw/kairos-pulse/0.10.2/", "undeclared": 3, "allowlist": 1})
    );
    // 代管的版本：沒有 external_url，也沒有這個原因
    let (p, _, _) = plan(
        "r",
        &hosts,
        &all,
        &waves,
        &o,
        &ctx("0.10.2"),
        &HashMap::new(),
    );
    assert_eq!((p.eligible, p.external_url.clone()), (8, None));
    assert_eq!(p.skipped.get(SKIP_URL_NOT_ALLOWED), None);
    assert_eq!(p.skipped.get(SKIP_URL_NOT_SUPPORTED), None);
    assert_eq!(p.json()["external_url"], Value::Null);
}

#[test]
fn spec_defaults_and_validation() {
    let s = RolloutSpec::parse(&json!({
        "name": " 0.9.1 全站 ",
        "target_version": "0.9.1",
        "selector": {"all": true, "exclude_agent_ids": ["AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE"]},
    }))
    .unwrap();
    assert_eq!(s.name, "0.9.1 全站");
    assert_eq!(s.waves, WaveSpec::default_template());
    assert_eq!(s.options, RolloutOptions::default());
    assert_eq!(
        s.selector.exclude_agent_ids,
        vec!["aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_string()]
    );
    assert_eq!(s.waves_json()[0]["size"], "1%");
    // 往返：資料表中的批次
    assert_eq!(waves_from_db(&s.waves_json().to_string()), s.waves);
    for (body, field) in [
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "schedule": []}),
            "schedule",
        ),
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "windows": []}),
            "windows",
        ),
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "schedule": {"timezone": "Asia/Taipe"}}),
            "schedule.timezone",
        ),
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "waves": vec![json!({"size": "rest"}); MAX_WAVES + 1]}),
            "waves",
        ),
        (
            json!({"name": "", "target_version": "0.9.1", "selector": {"all": true}}),
            "name",
        ),
        (
            json!({"name": "x", "target_version": "v1", "selector": {"all": true}}),
            "target_version",
        ),
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {}}),
            "selector",
        ),
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {"agent_ids": ["web-01"]}}),
            "selector.agent_ids",
        ),
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "waves": []}),
            "waves",
        ),
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "waves": [{"size": 1}]}),
            "waves[0]",
        ),
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "waves": [{"size": "rest"}, {"size": "rest"}]}),
            "waves[0]",
        ),
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "options": {"health_timeout_secs": 60}}),
            "options.health_timeout_secs",
        ),
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "options": {"max_in_progress": 0}}),
            "options.max_in_progress",
        ),
        (
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "waves": [{"size": "rest"}], "options": {"canary_agent_ids": ["aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"]}}),
            "options.canary_agent_ids",
        ),
    ] {
        let e = RolloutSpec::parse(&body).unwrap_err();
        assert_eq!(e.0, field, "{body}: {e:?}");
    }
    let one = RolloutSpec::parse(&json!({"name": "x", "target_version": "0.9.1", "selector": {"groups": ["Web"]}, "waves": [{"size": "rest", "max_failure_ratio": 0.1}]})).unwrap();
    assert_eq!(one.waves.len(), 1);
    assert_eq!(one.selector.groups, vec!["web".to_string()]);
}

#[test]
fn eligibility_follows_section_4_7_2() {
    let c = ctx("0.9.1");
    let o = RolloutOptions::default();
    let why = |h: &UpgradeHost, o: &RolloutOptions| classify(h, &c, o).skip;
    assert_eq!(why(&host(1, "0.9.0", "tar", "ready"), &o), None);
    assert_eq!(
        why(&host(1, "0.9.0", "deb", "ready"), &o),
        None,
        "rollback deb of 0.9.0 exists"
    );
    assert_eq!(why(&host(1, "0.9.1", "tar", "ready"), &o), Some("already"));
    assert_eq!(
        why(&host(1, "0.8.0", "tar", "ready"), &o),
        Some("unsupported_version")
    );
    assert_eq!(
        why(&host(1, "0.9.0", "tar", "disabled"), &o),
        Some("disabled")
    );
    assert_eq!(
        why(&host(1, "0.9.0", "tar", "no_helper"), &o),
        Some("no_helper")
    );
    assert_eq!(
        why(&host(1, "0.9.0", "tar", "something"), &o),
        Some("not_ready")
    );
    // Pulse 0.10.0 起的 `unsafe_path`：專屬的原因，不落入 not_ready；deb／rpm 也一樣（在平台與發佈檔的
    // 檢查之前：路徑不修正的話，有沒有發佈檔都無法升級）
    for method in ["tar", "deb", "rpm"] {
        assert_eq!(
            why(&host(1, "0.10.0", method, "unsafe_path"), &o),
            Some(SKIP_UNSAFE_PATH),
            "{method}"
        );
    }
    assert_eq!(SKIP_UNSAFE_PATH, "unsafe_path");
    assert_eq!(
        SKIP_UNSAFE_PATH,
        kairos_proto::mgmt::INSTALL_UPGRADE_UNSAFE_PATH
    );
    // 已是目標版本、已除役、Pulse 太舊的判斷仍然優先
    assert_eq!(
        why(&host(1, "0.9.1", "tar", "unsafe_path"), &o),
        Some("already")
    );
    let mut gone = host(1, "0.10.0", "tar", "unsafe_path");
    gone.decommissioned = true;
    assert_eq!(why(&gone, &o), Some("decommissioned"));
    // 新版 Kairos 收到舊 Pulse（0.9：沒有這個值）照舊；規格定義的每個值都有專屬的原因，不落入 not_ready
    for state in kairos_proto::mgmt::INSTALL_UPGRADE_STATES {
        let got = why(&host(1, "0.9.0", "tar", state), &o);
        if state == kairos_proto::mgmt::INSTALL_UPGRADE_READY {
            assert_eq!(got, None);
        } else {
            assert_eq!(got, Some(state), "install.upgrade = {state}");
        }
    }
    assert_eq!(
        why(&host(1, "0.9.0", "msi", "ready"), &o),
        Some("unsupported_platform")
    );
    assert_eq!(
        why(&host(1, "0.9.0", "unknown", "ready"), &o),
        Some("unsupported_platform")
    );
    assert_eq!(
        why(&host(1, "0.9.0", "rpm", "ready"), &o),
        Some("no_artifact")
    );
    let mut a = host(1, "0.9.0", "tar", "ready");
    a.os = Some(r#"{"arch":"aarch64"}"#.into());
    assert_eq!(why(&a, &o), Some("no_artifact"));
    // deb 沒有目前版本的回滾檔
    let d = host(1, "0.9.0", "deb", "ready");
    let mut c3 = ctx("0.9.1");
    c3.by_version.remove("0.9.0");
    assert_eq!(classify(&d, &c3, &o).skip, Some("no_rollback"));
    let allow = RolloutOptions {
        allow_no_rollback: true,
        ..Default::default()
    };
    let e = classify(&d, &c3, &allow);
    assert_eq!((e.skip, e.no_rollback), (None, true));
    assert_eq!(e.rollback, RollbackVia::BinaryOnly);
    // 預期的回滾方式（ADR-0003 第 5.4 節）：版本庫有目前的版本 → 回滾檔（＋bundle）；主機宣告本機封存
    // （install.rollback = "archived"）→ 不依賴版本庫，沒有目前的版本也不是 no_rollback；其他宣告
    // （binary_only、none、degraded、不認得的值）與沒有宣告相同，看版本庫；tar 不適用
    let e = classify(&d, &ctx("0.9.1"), &o);
    assert_eq!(
        (e.skip, e.no_rollback, e.rollback, e.degraded),
        (None, false, RollbackVia::Repository, false)
    );
    let declares_rollback = |v: &str| {
        let mut h = d.clone();
        h.install_rollback = Some(v.to_string());
        h
    };
    let e = classify(&declares_rollback("archived"), &c3, &o);
    assert_eq!(
        (e.skip, e.no_rollback, e.rollback),
        (None, false, RollbackVia::Archive)
    );
    // 兩者都有：封存優先（回滾綁定本機實際安裝過的那一份）
    let e = classify(&declares_rollback("archived"), &ctx("0.9.1"), &o);
    assert_eq!(e.rollback, RollbackVia::Archive);
    for v in ["binary_only", "none", "degraded", "future_value"] {
        let h = declares_rollback(v);
        assert_eq!(classify(&h, &c3, &o).skip, Some("no_rollback"), "{v}");
        let e = classify(&h, &c3, &allow);
        assert_eq!(
            (e.skip, e.no_rollback, e.rollback, e.degraded),
            (None, true, RollbackVia::BinaryOnly, v == "degraded"),
            "{v}"
        );
        assert_eq!(
            classify(&h, &ctx("0.9.1"), &o).rollback,
            RollbackVia::Repository,
            "{v}"
        );
    }
    let mut t = host(1, "0.9.0", "tar", "ready");
    t.install_rollback = Some("binary_only".into());
    let e = classify(&t, &c3, &o);
    assert_eq!(
        (e.skip, e.no_rollback, e.rollback),
        (None, false, RollbackVia::NotApplicable)
    );
    // 預發佈版本低於 min_upgrade_from（0.9.0-test.1 < 0.9.0）
    let mut pre = host(1, "0.9.0", "tar", "ready");
    pre.agent_version = Some("0.9.0-test.1".into());
    assert_eq!(why(&pre, &o), Some("min_upgrade_from"));
    // 降版
    let newer = host(1, "0.9.2", "tar", "ready");
    assert_eq!(why(&newer, &o), Some("downgrade_not_allowed"));
    let down = RolloutOptions {
        allow_downgrade: true,
        ..Default::default()
    };
    let e = classify(&newer, &c, &down);
    assert_eq!((e.skip, e.downgrade), (None, true));
    // min_upgrade_from
    let mut c2 = ctx("0.9.1");
    c2.manifest = Some(manifest("0.9.1", "0.9.1-rc.1"));
    assert_eq!(
        classify(&host(1, "0.9.0", "tar", "ready"), &c2, &o).skip,
        Some("min_upgrade_from")
    );
    c2.manifest = None;
    assert_eq!(
        classify(&host(1, "0.9.0", "tar", "ready"), &c2, &o).skip,
        Some("no_manifest")
    );
    let mut gone = host(1, "0.9.0", "tar", "ready");
    gone.decommissioned = true;
    assert_eq!(why(&gone, &o), Some("decommissioned"));
}

#[test]
fn waves_take_canary_first_and_respect_the_percent_cap() {
    let hosts: Vec<UpgradeHost> = (1..=50).map(|i| host(i, "0.9.0", "tar", "ready")).collect();
    let refs: Vec<&UpgradeHost> = hosts.iter().collect();
    let w = WaveSpec::default_template();
    // 1%（至少 1 台）
    let a = assign_waves("r-1", &refs, &w, &[]);
    assert_eq!(a.iter().filter(|x| x.1 == 0).count(), 1);
    assert_eq!(a.iter().filter(|x| x.1 == 1).count(), 49);
    // 順序穩定
    assert_eq!(a, assign_waves("r-1", &refs, &w, &[]));
    assert_ne!(a, assign_waves("r-2", &refs, &w, &[]));
    // 百分比的上限
    let mut w2 = w.clone();
    w2[0].size = WaveSize::Percent(50);
    w2[0].max_hosts = Some(10);
    assert_eq!(
        assign_waves("r-1", &refs, &w2, &[])
            .iter()
            .filter(|x| x.1 == 0)
            .count(),
        10
    );
    w2[0].size = WaveSize::Count(3);
    w2[0].max_hosts = None;
    assert_eq!(
        assign_waves("r-1", &refs, &w2, &[])
            .iter()
            .filter(|x| x.1 == 0)
            .count(),
        3
    );
    // 指定 canary
    let canary = vec![hosts[7].agent_id.clone(), hosts[3].agent_id.clone()];
    let a = assign_waves("r-1", &refs, &w, &canary);
    let first: Vec<i64> = a.iter().filter(|x| x.1 == 0).map(|x| x.0).collect();
    assert_eq!(first, vec![8, 4]);
    // 只有一批
    let one = vec![WaveSpec {
        size: WaveSize::Rest,
        ..w[1].clone()
    }];
    assert!(
        assign_waves("r-1", &refs, &one, &[])
            .iter()
            .all(|x| x.1 == 0)
    );
}

fn rh(id: u8, wave: i64, ordinal: i64, state: &str) -> RolloutHostRow {
    RolloutHostRow {
        rollout_id: "r-1".into(),
        host_id: i64::from(id),
        agent_id: format!("a{id}"),
        hostname: format!("h{id}"),
        upgrade_id: format!("u{id}"),
        wave,
        ordinal,
        state: state.into(),
        updated_at: 0,
        ..Default::default()
    }
}

fn opts(max: u32) -> RolloutOptions {
    RolloutOptions {
        max_in_progress: max,
        health_timeout_secs: 600,
        ..Default::default()
    }
}

const T0: i64 = 1_000_000_000;

static NO_SCHEDULE: ScheduleSpec = ScheduleSpec {
    starts_at: None,
    ends_at: None,
    windows: Vec::new(),
    timezone: None,
};

fn input<'a>(waves: &'a [WaveSpec], o: &'a RolloutOptions, now: i64) -> TickInput<'a> {
    TickInput {
        schedule: &NO_SCHEDULE,
        now_ms: now,
        started_ms: T0,
        start_grace_ms: START_GRACE_MS,
        current_wave: 0,
        soak_until: None,
        awaiting_continue: false,
        waves,
        options: o,
    }
}

fn capable(_: &str) -> HostLive {
    HostLive {
        capable: true,
        online: true,
        ..Default::default()
    }
}

#[test]
fn offers_respect_capability_limits_and_start_grace() {
    let w = WaveSpec::default_template();
    let o = opts(2);
    let hosts = vec![
        rh(1, 0, 0, HOST_PENDING),
        rh(2, 0, 1, HOST_PENDING),
        rh(3, 0, 2, HOST_PENDING),
        rh(4, 1, 0, HOST_PENDING),
    ];
    // Kairos 剛啟動（1 分鐘內）：不提議
    let (a, _) = plan_tick(&input(&w, &o, T0 + 1000), &hosts, &capable);
    assert!(a.is_empty());
    // 之後：同時最多 2 台、只提議目前這一批
    let (a, r) = plan_tick(&input(&w, &o, T0 + START_GRACE_MS), &hosts, &capable);
    assert_eq!(a, vec![(0, HostAction::Offer), (1, HostAction::Offer)]);
    assert_eq!(r, None);
    // 沒有協商 upgrade 的主機不提議；線上但沒有協商 → 跳過
    let live = |a: &str| match a {
        "a1" => HostLive::default(),
        "a2" => HostLive {
            online: true,
            ..Default::default()
        },
        _ => capable(a),
    };
    let (a, _) = plan_tick(&input(&w, &o, T0 + START_GRACE_MS), &hosts, &live);
    assert_eq!(
        a,
        vec![(1, HostAction::SkipNotReady), (2, HostAction::Offer)]
    );
}

#[test]
fn timeouts_mark_expired_unknown_and_offline_hosts() {
    let w = WaveSpec::default_template();
    let o = opts(50);
    let now = T0 + 100 * 3_600_000;
    let mut offered = rh(1, 0, 0, HOST_STAGED);
    offered.offered_at = Some(now - APPLY_WINDOW_MS - EXPIRE_GRACE_MS - 1);
    let mut applying = rh(2, 0, 1, HOST_APPLYING);
    applying.applying_at = Some(now - 600_000 - APPLY_STUCK_GRACE_MS - 1);
    let pending = rh(3, 0, 2, HOST_PENDING);
    let gone = rh(4, 0, 3, HOST_SKIPPED_OFFLINE);
    let hosts = vec![offered, applying, pending, gone];
    let live = |a: &str| match a {
        "a2" => HostLive::default(),
        "a3" => HostLive {
            offline_for_ms: Some(25 * 3_600_000),
            ..Default::default()
        },
        "a4" => capable(a),
        _ => capable(a),
    };
    let (a, _) = plan_tick(&input(&w, &o, now), &hosts, &live);
    assert!(a.contains(&(0, HostAction::Expire)));
    assert!(a.contains(&(1, HostAction::Unknown)));
    assert!(a.contains(&(2, HostAction::SkipOffline)));
    // 重新上線的 skipped_offline 回到 pending（這一輪因 unknown 計入失敗而暫停，不提議）
    assert!(a.contains(&(3, HostAction::Revive)));
    let (a, _) = plan_tick(&input(&w, &o, now), &hosts[2..], &live);
    assert!(a.contains(&(1, HostAction::Offer)), "{a:?}");
    // Kairos 這端造成的離線（server_lost、關機）：從 Kairos 啟動時重新起算
    let by_server = |_: &str| HostLive {
        offline_by_server: true,
        ..Default::default()
    };
    let mut i = input(&w, &o, T0 + 60_000);
    i.started_ms = T0;
    let mut stuck = rh(2, 0, 1, HOST_APPLYING);
    stuck.applying_at = Some(T0 - 100 * 3_600_000);
    let (a, _) = plan_tick(&i, &[stuck.clone()], &by_server);
    assert!(a.is_empty(), "{a:?}");
    let (a, _) = plan_tick(&i, &[stuck], &|_| HostLive::default());
    assert_eq!(a, vec![(0, HostAction::Unknown)]);
}

#[test]
fn failure_ratio_pauses_and_waves_advance_through_soak_and_continue() {
    let w = WaveSpec::default_template();
    let o = opts(50);
    let now = T0 + 10 * 60_000;
    // canary（上限 0）任何失敗都暫停
    let hosts = vec![rh(1, 0, 0, HOST_ROLLED_BACK), rh(2, 1, 0, HOST_PENDING)];
    let (_, r) = plan_tick(&input(&w, &o, now), &hosts, &capable);
    assert!(
        matches!(
            r,
            Some(RolloutAction::Pause {
                failures: 1,
                finished: 1,
                ..
            })
        ),
        "{r:?}"
    );
    // canary 成功：開始觀察 60 分鐘
    let hosts = vec![
        rh(1, 0, 0, HOST_SUCCEEDED),
        rh(2, 1, 0, HOST_PENDING),
        rh(3, -1, 0, HOST_SKIPPED),
    ];
    let (a, r) = plan_tick(&input(&w, &o, now), &hosts, &capable);
    assert!(a.is_empty(), "第二批在觀察前不提議");
    assert_eq!(
        r,
        Some(RolloutAction::StartSoak {
            until: now + 3_600_000
        })
    );
    // 觀察中
    let mut i = input(&w, &o, now + 60_000);
    i.soak_until = Some(now + 3_600_000);
    assert_eq!(plan_tick(&i, &hosts, &capable).1, None);
    // 觀察期滿：需要人工繼續
    i.now_ms = now + 3_600_001;
    assert_eq!(
        plan_tick(&i, &hosts, &capable).1,
        Some(RolloutAction::AwaitContinue)
    );
    i.awaiting_continue = true;
    assert_eq!(plan_tick(&i, &hosts, &capable), (vec![], None));
    // 不需要人工繼續時直接下一批
    let mut w2 = w.clone();
    w2[0].manual_continue = false;
    let mut i2 = input(&w2, &o, now + 3_600_001);
    i2.soak_until = Some(now + 3_600_000);
    assert_eq!(
        plan_tick(&i2, &hosts, &capable).1,
        Some(RolloutAction::NextWave)
    );
    // 最後一批：失敗比例在上限內，全部結束 → 完成
    let mut last: Vec<RolloutHostRow> = (0..20)
        .map(|i| rh(i + 10, 1, i64::from(i), HOST_SUCCEEDED))
        .collect();
    last[0].state = HOST_FAILED.into();
    last.push(rh(1, 0, 0, HOST_SUCCEEDED));
    let mut i3 = input(&w, &o, now);
    i3.current_wave = 1;
    assert_eq!(
        plan_tick(&i3, &last, &capable).1,
        Some(RolloutAction::Complete)
    );
    // 超過 5% → 暫停
    last[1].state = HOST_UNKNOWN.into();
    assert!(matches!(
        plan_tick(&i3, &last, &capable).1,
        Some(RolloutAction::Pause { failures: 2, .. })
    ));
    // declined、expired 不計入分母
    let mixed = vec![
        rh(1, 0, 0, HOST_DECLINED),
        rh(2, 0, 1, HOST_EXPIRED),
        rh(3, 1, 0, HOST_PENDING),
    ];
    assert!(matches!(
        plan_tick(&input(&w, &o, now), &mixed, &capable).1,
        Some(RolloutAction::StartSoak { .. })
    ));
}

#[test]
fn next_action_codes_and_confirm_text() {
    assert_eq!(
        next_action(ROLLOUT_RUNNING, None, true, None, None, 0)["code"],
        "waiting_continue"
    );
    assert_eq!(
        next_action(ROLLOUT_RUNNING, Some(120_000), false, None, None, 0)["minutes"],
        2
    );
    assert_eq!(
        next_action(ROLLOUT_PAUSED, None, false, Some("x"), None, 0)["reason"],
        "x"
    );
    // 暫停的原因分類：資料表沒有記錄（主控台暫停）時為 manual；不是暫停中時沒有
    assert_eq!(
        next_action(ROLLOUT_PAUSED, None, false, Some("x"), None, 0)["cause"],
        "manual"
    );
    assert_eq!(
        next_action(
            ROLLOUT_PAUSED,
            None,
            false,
            Some("x"),
            Some(PAUSE_FAILURE_RATIO),
            0
        )["cause"],
        "failure_ratio"
    );
    assert_eq!(pause_cause(ROLLOUT_PAUSED, None), Some(PAUSE_MANUAL));
    assert_eq!(
        pause_cause(ROLLOUT_PAUSED, Some(PAUSE_ROLLBACK_FAILED)),
        Some("rollback_failed")
    );
    assert_eq!(
        pause_cause(ROLLOUT_RUNNING, Some(PAUSE_SIGNATURE_FAILED)),
        None
    );
    assert_eq!(
        next_action(ROLLOUT_RUNNING, None, false, None, None, 0)["code"],
        "upgrading"
    );
    assert_eq!(confirm_text(312, false, 0), "升級 312 台");
    assert_eq!(confirm_text(3, true, 0), "降版 3 台");
    // 允許無法自動回滾的主機（設計包 S26 的字串，全形括號）；降版優先
    assert_eq!(
        confirm_text(312, false, 12),
        "升級 312 台（無法自動回滾 12 台）"
    );
    assert_eq!(confirm_text(3, true, 2), "降版 3 台");
    let p = progress(&[rh(1, 0, 0, HOST_SUCCEEDED), rh(2, 0, 0, HOST_SUCCEEDED)]);
    assert_eq!(p["succeeded"], 2);
    assert_eq!(p["pending"], 0);
    assert!(is_retryable(&rh(1, 0, 0, HOST_FAILED)));
    assert!(!is_retryable(&rh(1, 0, 0, HOST_ROLLBACK_FAILED)));
    let mut ack = rh(1, 0, 0, HOST_ROLLBACK_FAILED);
    ack.acknowledged_at = Some(1);
    assert!(is_retryable(&ack));
}

#[test]
fn selector_matches_groups_tags_ids_and_excludes() {
    let h = host(1, "0.9.0", "tar", "ready");
    let mut s = SelectorSpec {
        groups: vec!["web".into()],
        ..Default::default()
    };
    assert!(s.matches(&h));
    s.exclude_agent_ids = vec![h.agent_id.clone()];
    assert!(!s.matches(&h));
    let t = SelectorSpec {
        tags: vec!["env:prod".into()],
        ..Default::default()
    };
    assert!(t.matches(&h));
    let id = SelectorSpec {
        agent_ids: vec![h.agent_id.clone()],
        ..Default::default()
    };
    assert!(id.matches(&h));
    assert!(!SelectorSpec::default().matches(&h));
}

// ---- 0.10：自訂批次 ----

fn wave_counts(a: &[(i64, i64, i64)], waves: usize) -> Vec<usize> {
    (0..waves as i64)
        .map(|w| a.iter().filter(|x| x.1 == w).count())
        .collect()
}

#[test]
fn custom_waves_take_percentages_of_the_remaining_hosts() {
    let hosts: Vec<UpgradeHost> = (1..=200)
        .map(|i| host(i as u8, "0.9.0", "tar", "ready"))
        .enumerate()
        .map(|(n, mut h)| {
            h.host_id = n as i64 + 1;
            h.agent_id = format!("00000000-0000-4000-8000-{:012x}", n + 1);
            h
        })
        .collect();
    let refs: Vec<&UpgradeHost> = hosts.iter().collect();
    // 建議的四批範本：1%（2 台）→ 剩餘 198 的 10%（20 台）→ 剩餘 178 的 50%（89 台）→ 其餘 89 台
    let w = WaveSpec::recommended_template();
    assert_eq!(w.len(), 4);
    assert_eq!(w[0], WaveSpec::default_template()[0]);
    assert_eq!(w[3].size, WaveSize::Rest);
    let a = assign_waves("r-1", &refs, &w, &[]);
    assert_eq!(wave_counts(&a, 4), [2, 20, 89, 89]);
    // 每一批內的順序從 0 起連續，每台主機只出現一次
    for wave in 0..4 {
        let mut ords: Vec<i64> = a.iter().filter(|x| x.1 == wave).map(|x| x.2).collect();
        ords.sort();
        assert_eq!(ords, (0..ords.len() as i64).collect::<Vec<_>>());
    }
    let mut ids: Vec<i64> = a.iter().map(|x| x.0).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 200);
    // 兩批的結果與批次數無關的部分一致：第一批是同樣的主機（0.9 相容）
    let two = assign_waves("r-1", &refs, &WaveSpec::default_template(), &[]);
    let first = |a: &[(i64, i64, i64)]| -> Vec<i64> {
        a.iter().filter(|x| x.1 == 0).map(|x| x.0).collect()
    };
    assert_eq!(first(&a), first(&two));
    assert_eq!(wave_counts(&two, 2), [2, 198]);
    // 台數、max_hosts 與 canary
    let mut w3 = vec![w[0].clone(), w[1].clone(), w[3].clone()];
    w3[0].size = WaveSize::Count(5);
    w3[0].max_hosts = None;
    w3[1].size = WaveSize::Percent(50);
    w3[1].max_hosts = Some(30);
    assert_eq!(
        wave_counts(&assign_waves("r-1", &refs, &w3, &[]), 3),
        [5, 30, 165]
    );
    let canary = vec![hosts[7].agent_id.clone(), hosts[3].agent_id.clone()];
    let c = assign_waves("r-1", &refs, &w3, &canary);
    assert_eq!(first(&c), vec![hosts[7].host_id, hosts[3].host_id]);
    assert_eq!(wave_counts(&c, 3), [2, 30, 168]);
    // 主機比批次少：前面的批次各至少 1 台，後面的批次是空的
    let few: Vec<&UpgradeHost> = hosts.iter().take(2).collect();
    assert_eq!(
        wave_counts(&assign_waves("r-1", &few, &w, &[]), 4),
        [1, 1, 0, 0]
    );
    assert!(assign_waves("r-1", &[], &w, &[]).is_empty());
    // 解析：最多 MAX_WAVES 批、只有最後一批是 rest
    let body = |waves: serde_json::Value| json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "waves": waves});
    let s = RolloutSpec::parse(&body(json!([
        {"size": 1, "max_failure_ratio": 0, "manual_continue": true},
        {"size": "10%", "soak_minutes": 30},
        {"size": "50%", "soak_minutes": 30},
        {"size": "rest"},
    ])))
    .unwrap();
    assert_eq!(s.waves.len(), 4);
    assert_eq!(waves_from_db(&s.waves_json().to_string()), s.waves);
    assert_eq!(
        RolloutSpec::parse(&body(
            json!([{"size": "rest"}, {"size": "10%"}, {"size": "rest"}])
        ))
        .unwrap_err()
        .0,
        "waves[0]"
    );
}

#[test]
fn three_waves_advance_with_their_own_soak_limits_and_manual_continue() {
    let mut w = WaveSpec::recommended_template();
    w.remove(2); // canary（人工繼續、上限 0）→ 10%（觀察 30 分鐘、上限 5%）→ 其餘
    w[1].max_failure_ratio = 0.5;
    let o = opts(50);
    let now = T0 + START_GRACE_MS;
    let tick = |wave: i64, soak: Option<i64>, awaiting: bool, at: i64, hosts: &[RolloutHostRow]| {
        let mut i = input(&w, &o, at);
        i.current_wave = wave;
        i.soak_until = soak;
        i.awaiting_continue = awaiting;
        plan_tick(&i, hosts, &capable)
    };
    // 第 1 批結束 → 觀察 60 分鐘 → 等待人工繼續（不會自己進下一批）
    let hosts = vec![
        rh(1, 0, 0, HOST_SUCCEEDED),
        rh(2, 1, 0, HOST_PENDING),
        rh(3, 1, 1, HOST_PENDING),
        rh(4, 2, 0, HOST_PENDING),
    ];
    let (a, r) = tick(0, None, false, now, &hosts);
    assert!(a.is_empty(), "後面的批次還不提議");
    assert_eq!(
        r,
        Some(RolloutAction::StartSoak {
            until: now + 60 * 60_000
        })
    );
    let soak = Some(now + 60 * 60_000);
    assert_eq!(tick(0, soak, false, now + 1000, &hosts).1, None);
    assert_eq!(
        tick(0, soak, false, now + 61 * 60_000, &hosts).1,
        Some(RolloutAction::AwaitContinue)
    );
    assert_eq!(tick(0, soak, true, now + 62 * 60_000, &hosts).1, None);
    // 第 2 批：只提議第 2 批（與之前批次）的主機
    let (a, r) = tick(1, None, false, now, &hosts);
    assert_eq!(a, vec![(1, HostAction::Offer), (2, HostAction::Offer)]);
    assert_eq!(r, None);
    // 第 2 批一成一敗：失敗比例 50% 沒有超過這一批的上限 50% → 觀察 30 分鐘 → 自動進第 3 批
    let hosts = vec![
        rh(1, 0, 0, HOST_SUCCEEDED),
        rh(2, 1, 0, HOST_SUCCEEDED),
        rh(3, 1, 1, HOST_ROLLED_BACK),
        rh(4, 2, 0, HOST_PENDING),
    ];
    assert_eq!(
        tick(1, None, false, now, &hosts).1,
        Some(RolloutAction::StartSoak {
            until: now + 30 * 60_000
        })
    );
    assert_eq!(
        tick(1, Some(now + 30 * 60_000), false, now + 31 * 60_000, &hosts).1,
        Some(RolloutAction::NextWave)
    );
    // 同樣的結果在上限 5% 的批次就會暫停
    let mut strict = w.clone();
    strict[1].max_failure_ratio = 0.05;
    let mut i = input(&strict, &o, now);
    i.current_wave = 1;
    assert!(matches!(
        plan_tick(&i, &hosts, &capable).1,
        Some(RolloutAction::Pause {
            failures: 1,
            finished: 2,
            ..
        })
    ));
    // 第 1 批（canary）的上限是 0：第 2 批的失敗不影響第 1 批，但第 1 批任何失敗都暫停
    let canary_failed = vec![rh(1, 0, 0, HOST_FAILED), rh(2, 1, 0, HOST_PENDING)];
    assert!(matches!(
        tick(0, None, false, now, &canary_failed).1,
        Some(RolloutAction::Pause { .. })
    ));
    // 最後一批：提議、全部結束後完成（最後一批沒有觀察期）
    let (a, r) = tick(2, None, false, now, &hosts);
    assert_eq!(a, vec![(3, HostAction::Offer)]);
    assert_eq!(r, None);
    let done = vec![
        rh(1, 0, 0, HOST_SUCCEEDED),
        rh(2, 1, 0, HOST_SUCCEEDED),
        rh(3, 1, 1, HOST_ROLLED_BACK),
        rh(4, 2, 0, HOST_SUCCEEDED),
    ];
    assert_eq!(
        tick(2, None, false, now, &done).1,
        Some(RolloutAction::Complete)
    );
    // 批次的統計
    let j = waves_json(&w, &done, 2);
    assert_eq!(j.as_array().unwrap().len(), 3);
    assert_eq!(j[1]["hosts"], 2);
    assert_eq!(j[1]["failure_ratio"], 0.5);
    assert_eq!(j[2]["current"], true);
}

// ---- 0.10：排程與升級時段 ----

fn ms(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .unwrap()
        .timestamp_millis()
}

fn nightly() -> ScheduleSpec {
    ScheduleSpec::parse(Some(
        &json!({"windows": [{"start": "01:00", "end": "05:00"}], "timezone": "+08:00"}),
    ))
    .unwrap()
}

fn windowed_input<'a>(
    w: &'a [WaveSpec],
    o: &'a RolloutOptions,
    s: &'a ScheduleSpec,
    now: i64,
) -> TickInput<'a> {
    TickInput {
        schedule: s,
        started_ms: 0,
        ..input(w, o, now)
    }
}

#[test]
fn hosts_are_only_offered_inside_a_maintenance_window() {
    let w = WaveSpec::default_template();
    let o = opts(50);
    let s = nightly();
    let hosts = vec![rh(1, 0, 0, HOST_PENDING)];
    // 時段外：不提議，批次也不會結束
    let (a, r) = plan_tick(
        &windowed_input(&w, &o, &s, ms("2026-11-12T00:30:00+08:00")),
        &hosts,
        &capable,
    );
    assert!(a.is_empty());
    assert_eq!(r, None);
    // 時段內：提議
    let (a, _) = plan_tick(
        &windowed_input(&w, &o, &s, ms("2026-11-12T01:00:00+08:00")),
        &hosts,
        &capable,
    );
    assert_eq!(a, vec![(0, HostAction::Offer)]);
    // 替換中的主機在時段結束後照常完成（不受時段影響、不會被重新排入）
    let mut applying = rh(1, 0, 0, HOST_APPLYING);
    applying.offered_at = Some(ms("2026-11-12T04:50:00+08:00"));
    applying.applying_at = Some(ms("2026-11-12T04:58:00+08:00"));
    let (a, _) = plan_tick(
        &windowed_input(&w, &o, &s, ms("2026-11-12T05:10:00+08:00")),
        &[applying],
        &capable,
    );
    assert!(a.is_empty());
}

#[test]
fn hosts_that_miss_the_window_are_requeued_for_the_next_one() {
    let w = WaveSpec::default_template();
    let o = opts(50);
    let s = nightly();
    let end = ms("2026-11-12T05:00:00+08:00");
    let mut staged = rh(1, 0, 0, HOST_STAGED);
    staged.offered_at = Some(ms("2026-11-12T04:55:00+08:00"));
    // 時段剛結束（留給 Pulse 回報 expired 的時間內）：不動
    let (a, _) = plan_tick(
        &windowed_input(&w, &o, &s, end + WINDOW_EXPIRE_GRACE_MS),
        std::slice::from_ref(&staged),
        &capable,
    );
    assert!(a.is_empty());
    // 之後仍停在提議中：回到 pending（下一個時段重新提議），不是 expired、不算失敗
    let (a, r) = plan_tick(
        &windowed_input(&w, &o, &s, end + WINDOW_EXPIRE_GRACE_MS + 1),
        std::slice::from_ref(&staged),
        &capable,
    );
    assert_eq!(a, vec![(0, HostAction::Requeue)]);
    assert_eq!(r, None, "批次還沒結束");
    // Pulse 自己回報 expired：同樣重新排入；即使下一個時段已經開始，也要等下一輪（先換 upgrade_id）才提議
    let mut expired = rh(1, 0, 0, HOST_EXPIRED);
    expired.offered_at = staged.offered_at;
    let (a, r) = plan_tick(
        &windowed_input(&w, &o, &s, ms("2026-11-13T01:30:00+08:00")),
        std::slice::from_ref(&expired),
        &capable,
    );
    assert_eq!(a, vec![(0, HostAction::Requeue)]);
    assert_eq!(r, None);
    // 重新排入的次數有上限：超過後留在 expired（批次可以結束，可手動重試）
    expired.attempts = MAX_WINDOW_REQUEUES;
    let (a, r) = plan_tick(
        &windowed_input(&w, &o, &s, ms("2026-11-13T01:30:00+08:00")),
        std::slice::from_ref(&expired),
        &capable,
    );
    assert!(a.is_empty());
    assert!(r.is_some());
    staged.attempts = MAX_WINDOW_REQUEUES;
    let (a, _) = plan_tick(
        &windowed_input(&w, &o, &s, end + WINDOW_EXPIRE_GRACE_MS + 1),
        std::slice::from_ref(&staged),
        &capable,
    );
    assert_eq!(a, vec![(0, HostAction::Expire)]);
    // ends_at 造成的 expired 不重新排入
    let mut ended = rh(1, 0, 0, HOST_EXPIRED);
    ended.reason = Some(REASON_ROLLOUT_ENDED.into());
    let (a, _) = plan_tick(
        &windowed_input(&w, &o, &s, ms("2026-11-13T01:30:00+08:00")),
        &[ended],
        &capable,
    );
    assert!(a.is_empty());
    // 沒有時段的推送維持 0.9 的行為：expired 是終止狀態，提議 24＋1 小時後才逾時
    let mut old = rh(1, 0, 0, HOST_EXPIRED);
    old.offered_at = Some(T0);
    let (a, _) = plan_tick(&input(&w, &o, T0 + START_GRACE_MS), &[old], &capable);
    assert!(a.is_empty());
}

#[test]
fn next_action_reports_schedule_and_window() {
    let s = ScheduleSpec::parse(Some(&json!({
        "starts_at": "2026-11-12T01:00:00+08:00",
        "windows": [{"start": "01:00", "end": "05:00"}],
        "timezone": "+08:00",
    })))
    .unwrap();
    let na =
        |state: &str, now: &str| next_action_scheduled(state, None, false, None, None, &s, ms(now));
    let v = na(ROLLOUT_SCHEDULED, "2026-11-11T12:00:00+08:00");
    assert_eq!(v["code"], "scheduled");
    assert_eq!(v["starts_at"], "2026-11-11T17:00:00.000Z");
    let v = na(ROLLOUT_RUNNING, "2026-11-12T06:00:00+08:00");
    assert_eq!(v["code"], "waiting_window");
    assert_eq!(v["until"], "2026-11-12T17:00:00.000Z");
    assert_eq!(
        na(ROLLOUT_RUNNING, "2026-11-12T02:00:00+08:00")["code"],
        "upgrading"
    );
    // 觀察中、等待繼續、暫停優先於時段
    assert_eq!(
        next_action_scheduled(
            ROLLOUT_RUNNING,
            None,
            true,
            None,
            None,
            &s,
            ms("2026-11-12T06:00:00+08:00")
        )["code"],
        "waiting_continue"
    );
    assert_eq!(
        na(ROLLOUT_PAUSED, "2026-11-12T06:00:00+08:00")["code"],
        "paused"
    );
}
