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
    }
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
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "schedule": {}}),
            "schedule",
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
            json!({"name": "x", "target_version": "0.9.1", "selector": {"all": true}, "waves": [{"size": 1}, {"size": 2}, {"size": "rest"}]}),
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

fn input<'a>(waves: &'a [WaveSpec], o: &'a RolloutOptions, now: i64) -> TickInput<'a> {
    TickInput {
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
        next_action(ROLLOUT_RUNNING, None, true, None, 0)["code"],
        "waiting_continue"
    );
    assert_eq!(
        next_action(ROLLOUT_RUNNING, Some(120_000), false, None, 0)["minutes"],
        2
    );
    assert_eq!(
        next_action(ROLLOUT_PAUSED, None, false, Some("x"), 0)["reason"],
        "x"
    );
    assert_eq!(
        next_action(ROLLOUT_RUNNING, None, false, None, 0)["code"],
        "upgrading"
    );
    assert_eq!(confirm_text(312, false), "升級 312 台");
    assert_eq!(confirm_text(3, true), "降版 3 台");
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
