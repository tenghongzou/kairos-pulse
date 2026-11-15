//! 遠端升級（P2）Pulse 端狀態機的測試：以 `kairos-proto/testdata/release` 的真實簽章向量（0.9.1、測試金鑰 A）
//! 模擬 Kairos 的提議與分段下載，不需要網路或 root。

use super::*;
use kairos_proto::mgmt::{self, ApplyWindow, UPGRADE_STATE_ACCEPTED};
use std::time::Duration;

const SIGNERS: &str =
    include_str!("../../../../crates/kairos-proto/testdata/release/allowed_signers");
const OTHER_PUB: &str =
    include_str!("../../../../crates/kairos-proto/testdata/release/other-b.pub");
const TAR: &[u8] = include_bytes!(
    "../../../../crates/kairos-proto/testdata/release/pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz"
);
const PROTOCOL: &str = include_str!("../../../../crates/kairos-proto/PROTOCOL.md");
const ID: &str = "9d3e7a10-4b2c-4f6e-8a1d-2c3b4a5f6e70";
const FILE: &str = "pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz";

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pulse-upg-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_anchor(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}

fn env(dir: &Path, version: &str) -> UpgradeEnv {
    let anchor = dir.join("allowed_signers");
    write_anchor(&anchor, SIGNERS);
    UpgradeEnv {
        state_dir: dir.join("state"),
        detection: Detection {
            method: mgmt::INSTALL_METHOD_TAR,
            upgrade: mgmt::INSTALL_UPGRADE_READY,
            reason: None,
            trust_anchor: anchor,
            fingerprints: Vec::new(),
        },
        current_version: version.into(),
        arch: "x86_64".into(),
        extra_principals: Vec::new(),
        allow_downgrade: false,
        health_timeout_secs: 0,
        require_root_owner: false,
        free_space: |_| None,
        redetect: None,
        url_allowlist: Vec::new(),
        external_ca: String::new(),
        own_binary_sha256: None,
        pkg_db: |_| None,
    }
}

fn ts(offset_secs: i64) -> String {
    (chrono::Utc::now() + chrono::TimeDelta::seconds(offset_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// PROTOCOL.md「遠端管理」範例的提議（真實的 SHA256SUMS、簽章與清單），時段改為 `window`
fn offer(window: (i64, i64)) -> UpgradeOffer {
    let line = PROTOCOL
        .lines()
        .find_map(|l| {
            l.strip_prefix("K ")
                .filter(|l| l.contains("\"upgrade_offer\""))
        })
        .unwrap();
    let ServerMsg::UpgradeOffer(o) = kairos_proto::parse_server_msg(line).unwrap() else {
        panic!("not an offer");
    };
    let mut o = *o;
    o.apply_window = ApplyWindow {
        not_before: ts(window.0),
        not_after: ts(window.1),
    };
    o
}

use kairos_proto::ServerMsg;

fn pending(id: &str) -> Pending {
    Pending {
        config_rev: None,
        upgrade_id: Some(id.into()),
    }
}

fn statuses(agent: &UpgradeAgent) -> Vec<StatusRecord> {
    agent.lock().file.outbox.clone()
}

fn states(agent: &UpgradeAgent) -> Vec<(String, Option<String>)> {
    statuses(agent)
        .into_iter()
        .map(|r| (r.state, r.code))
        .collect()
}

/// outbox 中的狀態全部視為已送出並收到 ack（並以 kairos-proto 的限制檢查每一則）；不取出其他請求
fn ack_all(agent: &UpgradeAgent, cs: &mut ConnState) -> Vec<StatusRecord> {
    let mut sent = Vec::new();
    loop {
        let next = agent.lock().file.outbox.first().cloned();
        let Some(r) = next else {
            break;
        };
        mgmt::validate_upgrade_status(&r.message(1)).unwrap();
        agent.on_status_acked(cs, &r);
        sent.push(r);
    }
    sent
}

fn chunk(offset: u64, data: &[u8], eof: bool) -> UpgradeChunk {
    UpgradeChunk::new(9, ID, FILE, offset, data, eof)
}

/// 送出 upgrade_get、收到提議、送出 accepted
fn accept(agent: &UpgradeAgent, o: UpgradeOffer) -> ConnState {
    let mut cs = agent.on_registered(true, Some(&pending(ID)));
    assert_eq!(
        agent.poll(&mut cs, Instant::now()),
        Some(Out::Get(ID.into()))
    );
    agent.on_offer(&mut cs, o);
    cs
}

#[test]
fn download_verify_stage_handoff_and_report_after_restart() {
    let dir = tmp("happy");
    let agent = UpgradeAgent::new(env(&dir, "0.9.0"));
    let mut cs = accept(&agent, offer((-60, 3600)));
    let sent = ack_all(&agent, &mut cs);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].state, UPGRADE_STATE_ACCEPTED);
    assert_eq!(
        (sent[0].from_version.as_str(), sent[0].to_version.as_str()),
        ("0.9.0", "0.9.1")
    );
    // 分段下載（54 bytes：一段）
    let Some(Out::Chunk {
        filename,
        offset,
        length,
        ..
    }) = agent.poll(&mut cs, Instant::now())
    else {
        panic!("expected a chunk request");
    };
    assert_eq!((filename.as_str(), offset, length), (FILE, 0, 54));
    assert_eq!(agent.poll(&mut cs, Instant::now()), None, "檔案只剩這一段");
    assert_eq!(
        agent.next_wake(&cs, Instant::now(), false),
        None,
        "等回應時不空轉"
    );
    agent.on_chunk(&mut cs, &chunk(0, TAR, true), (filename, offset, length));
    // 第一次驗證通過 → staged（附進度 100%）
    let sent = ack_all(&agent, &mut cs);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].state, UPGRADE_STATE_STAGED);
    assert_eq!(
        sent[0].progress,
        Some(UpgradeProgress {
            bytes: 54,
            total: 54
        })
    );
    let staging = dir.join("state/upgrade").join(ID);
    for f in [
        FILE,
        files::SUMS_FILE,
        files::SIG_FILE,
        "pulse-0.9.1-5e6f7a8-release.json",
    ] {
        assert!(staging.join(f).is_file(), "{f}");
    }
    assert!(
        dir.join("state/upgrade/releases/0.9.1/SHA256SUMS")
            .is_file(),
        "驗證過的發佈紀錄"
    );
    // 進入時段：先向 Kairos 再確認提議，再送 applying，收到 ack 後寫 request.json
    agent.tick(&mut cs, Instant::now(), false);
    assert_eq!(
        agent.view().phase.as_deref(),
        Some("staged"),
        "再確認前不交付"
    );
    assert_eq!(
        agent.poll(&mut cs, Instant::now()),
        Some(Out::Get(ID.into()))
    );
    agent.on_offer(&mut cs, offer((-60, 3600)));
    agent.tick(&mut cs, Instant::now(), false);
    let sent = ack_all(&agent, &mut cs);
    assert_eq!(sent[0].state, UPGRADE_STATE_APPLYING);
    let req = Request::from_untrusted_bytes(
        &std::fs::read(dir.join("state/upgrade/request.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(req.upgrade_id, ID);
    assert_eq!(req.method, "tar");
    assert_eq!(req.artifact, FILE);
    assert_eq!(req.health_timeout_secs, 600);
    assert_eq!(req.from_version, "0.9.0");
    assert_eq!(agent.view().phase.as_deref(), Some("applying"));
    // 輔助程式寫結果後重新啟動（新的行程）：讀 result.json，回報 succeeded（收到 ack 才算回報）
    let result = UpgradeResult {
        schema: files::RESULT_SCHEMA.into(),
        upgrade_id: ID.into(),
        state: UPGRADE_STATE_SUCCEEDED.into(),
        code: Some(codes::I_UPGRADE_COMPLETED.into()),
        detail: String::new(),
        from_version: "0.9.0".into(),
        to_version: "0.9.1".into(),
        finished_at: ts(0),
    };
    std::fs::write(
        dir.join("state/upgrade/result.json"),
        serde_json::to_vec(&result).unwrap(),
    )
    .unwrap();
    drop(agent);
    let agent = UpgradeAgent::new(env(&dir, "0.9.1"));
    assert_eq!(
        states(&agent),
        [("succeeded".into(), Some(codes::I_UPGRADE_COMPLETED.into()))]
    );
    assert!(!dir.join("state/upgrade/result.json").exists());
    assert!(!staging.exists(), "暫存已刪除");
    // 未收到 ack 就斷線：下一條連線重送
    let mut cs = agent.on_registered(true, None);
    assert!(matches!(
        agent.poll(&mut cs, Instant::now()),
        Some(Out::Status(_))
    ));
    let mut cs = agent.on_registered(true, None);
    let sent = ack_all(&agent, &mut cs);
    assert_eq!(sent.len(), 1);
    assert!(statuses(&agent).is_empty());
    // 已處理過的 upgrade_id 不再要求
    let mut cs = agent.on_registered(true, Some(&pending(ID)));
    assert_eq!(agent.poll(&mut cs, Instant::now()), None);
    assert_eq!(agent.view().last.unwrap().state, "succeeded");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn result_is_picked_up_while_still_running() {
    let dir = tmp("poll");
    let agent = UpgradeAgent::new(env(&dir, "0.9.0"));
    let mut cs = accept(&agent, offer((-60, 3600)));
    ack_all(&agent, &mut cs);
    let Some(Out::Chunk {
        filename,
        offset,
        length,
        ..
    }) = agent.poll(&mut cs, Instant::now())
    else {
        panic!();
    };
    agent.on_chunk(&mut cs, &chunk(0, TAR, true), (filename, offset, length));
    ack_all(&agent, &mut cs);
    // 交付：applying 沒有收到 ack，15 秒後仍寫 request.json
    let t0 = Instant::now();
    agent.tick(&mut cs, t0, false);
    assert_eq!(agent.poll(&mut cs, t0), Some(Out::Get(ID.into())));
    agent.on_offer(&mut cs, offer((-60, 3600)));
    agent.tick(&mut cs, t0, false);
    assert!(matches!(agent.poll(&mut cs, t0), Some(Out::Status(_))));
    agent.tick(&mut cs, t0 + Duration::from_secs(16), false);
    assert!(dir.join("state/upgrade/request.json").is_file());
    // 預檢失敗（Pulse 沒有被重新啟動）：輪詢到 result.json
    let result = UpgradeResult {
        schema: files::RESULT_SCHEMA.into(),
        upgrade_id: ID.into(),
        state: UPGRADE_STATE_FAILED.into(),
        code: Some(codes::E_UPGRADE_PRECHECK.into()),
        detail: "check-config exit 3".into(),
        from_version: "0.9.0".into(),
        to_version: "0.9.1".into(),
        finished_at: ts(0),
    };
    std::fs::write(
        dir.join("state/upgrade/result.json"),
        serde_json::to_vec(&result).unwrap(),
    )
    .unwrap();
    agent.tick(&mut cs, t0 + Duration::from_secs(30), false);
    let last = statuses(&agent).pop().unwrap();
    assert_eq!(
        (last.state.as_str(), last.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_PRECHECK))
    );
    assert_eq!(last.detail, "check-config exit 3");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn offers_are_declined_by_local_policy() {
    // (目前版本, 調整, 預期的代碼, detail 開頭)
    type Tweak = fn(&mut UpgradeEnv, &mut UpgradeOffer);
    let cases: [(&str, Tweak, &str, &str); 7] = [
        (
            "0.9.0",
            |e, _| e.detection.upgrade = mgmt::INSTALL_UPGRADE_DISABLED,
            codes::I_UPGRADE_DISABLED,
            "disabled",
        ),
        // 路徑不安全的主機不宣告 upgrade，正常不會收到提議；收到時以輔助程式會回報的同一個代碼拒絕
        (
            "0.9.0",
            |e, _| e.detection.upgrade = mgmt::INSTALL_UPGRADE_UNSAFE_PATH,
            codes::E_UPGRADE_PRECHECK,
            "unsafe_path",
        ),
        (
            "0.9.0",
            |e, _| e.detection.upgrade = mgmt::INSTALL_UPGRADE_NO_HELPER,
            codes::W_UPGRADE_UNSUPPORTED,
            "no_helper",
        ),
        (
            "0.9.2",
            |_, _| {},
            codes::W_UPGRADE_DOWNGRADE,
            "downgrade_not_allowed",
        ),
        (
            "0.8.9",
            |_, _| {},
            codes::W_UPGRADE_UNSUPPORTED,
            "min_upgrade_from",
        ),
        (
            "0.9.0",
            |_, o| {
                o.artifact.source = mgmt::SOURCE_URL.into();
                o.artifact.url = Some("https://repo.example.gov.tw/pulse.tar.gz".into());
            },
            codes::W_UPGRADE_UNSUPPORTED,
            "url_not_allowed",
        ),
        (
            "0.9.0",
            |e, _| e.free_space = |_| Some(1024),
            codes::W_UPGRADE_SPACE,
            "insufficient_space",
        ),
    ];
    for (i, (version, tweak, code, detail)) in cases.into_iter().enumerate() {
        let dir = tmp(&format!("decline-{i}"));
        let mut e = env(&dir, version);
        let mut o = offer((-60, 3600));
        tweak(&mut e, &mut o);
        let agent = UpgradeAgent::new(e);
        let mut cs = accept(&agent, o);
        let sent = ack_all(&agent, &mut cs);
        assert_eq!(sent.len(), 1, "{version} {detail}: {sent:?}");
        assert_eq!(sent[0].state, UPGRADE_STATE_DECLINED, "{detail}");
        assert_eq!(sent[0].code.as_deref(), Some(code), "{detail}");
        assert!(
            sent[0].detail.starts_with(detail),
            "{detail}: {}",
            sent[0].detail
        );
        assert!(agent.lock().file.current.is_none());
        assert!(!dir.join("state/upgrade").join(ID).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
    // 本機與推送都允許時接受降版
    let dir = tmp("downgrade-ok");
    let mut e = env(&dir, "0.9.2");
    e.allow_downgrade = true;
    let mut o = offer((-60, 3600));
    o.allow_downgrade = true;
    o.action = mgmt::ACTION_DOWNGRADE.into();
    let agent = UpgradeAgent::new(e);
    let mut cs = accept(&agent, o);
    assert_eq!(ack_all(&agent, &mut cs)[0].state, UPGRADE_STATE_ACCEPTED);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn signature_and_integrity_failures_are_reported_and_not_retried() {
    type Tweak = fn(&Path, &mut UpgradeEnv, &mut UpgradeOffer);
    let cases: [(Tweak, &str); 5] = [
        // 信任錨中沒有簽署這次發佈的金鑰
        (
            |dir, e, _| {
                let line =
                    format!("release@kairos-pulse namespaces=\"kairos-pulse-release\" {OTHER_PUB}");
                write_anchor(&dir.join("allowed_signers"), &line);
                e.detection.trust_anchor = dir.join("allowed_signers");
            },
            codes::E_UPGRADE_SIGNATURE,
        ),
        // SHA256SUMS 被竄改
        (
            |_, _, o| o.sha256sums = o.sha256sums.replace("a13150fd", "a13150fe"),
            codes::E_UPGRADE_SIGNATURE,
        ),
        // 發佈檔的雜湊與簽署的 SHA256SUMS 不符
        (
            |_, _, o| o.artifact.sha256 = "0".repeat(64),
            codes::E_UPGRADE_INTEGRITY,
        ),
        // 平台不符
        (
            |_, e, _| e.arch = "aarch64".into(),
            codes::E_UPGRADE_INTEGRITY,
        ),
        // 清單與 SHA256SUMS 不符
        (
            |_, _, o| {
                if let Some(m) = o.manifest.as_mut() {
                    m.content = m.content.replace("\"breaking\":false", "\"breaking\":true");
                }
            },
            codes::E_UPGRADE_INTEGRITY,
        ),
    ];
    for (i, (tweak, code)) in cases.into_iter().enumerate() {
        let dir = tmp(&format!("verify-{i}"));
        let mut e = env(&dir, "0.9.0");
        let mut o = offer((-60, 3600));
        tweak(&dir, &mut e, &mut o);
        let agent = UpgradeAgent::new(e);
        let mut cs = accept(&agent, o);
        let sent = ack_all(&agent, &mut cs);
        assert_eq!(sent.len(), 1, "case {i}: {sent:?}");
        assert_eq!(sent[0].state, UPGRADE_STATE_FAILED, "case {i}");
        assert_eq!(
            sent[0].code.as_deref(),
            Some(code),
            "case {i}: {}",
            sent[0].detail
        );
        // 同一個提議不重試
        let mut cs = agent.on_registered(true, Some(&pending(ID)));
        assert_eq!(agent.poll(&mut cs, Instant::now()), None, "case {i}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn hash_mismatch_redownloads_then_gives_up_after_three_times() {
    let dir = tmp("mismatch");
    let agent = UpgradeAgent::new(env(&dir, "0.9.0"));
    let mut cs = accept(&agent, offer((-60, 3600)));
    ack_all(&agent, &mut cs);
    let mut bad = TAR.to_vec();
    bad[0] ^= 1;
    let mut t = Instant::now();
    for attempt in 1..=3 {
        if attempt > 1 {
            // 段落請求之間至少隔 CHUNK_REQUEST_INTERVAL（不超過 Kairos 每條連線的訊息速率）
            assert_eq!(agent.poll(&mut cs, t), None, "attempt {attempt}");
            let next = t + CHUNK_REQUEST_INTERVAL;
            assert_eq!(agent.next_wake(&cs, t, false), Some(next));
            t = next;
        }
        let Some(Out::Chunk {
            filename,
            offset,
            length,
            ..
        }) = agent.poll(&mut cs, t)
        else {
            panic!("attempt {attempt}: expected a chunk request");
        };
        assert_eq!(offset, 0, "從頭重新下載");
        agent.on_chunk(&mut cs, &chunk(0, &bad, true), (filename, offset, length));
    }
    let sent = ack_all(&agent, &mut cs);
    assert_eq!(sent.last().unwrap().state, UPGRADE_STATE_FAILED);
    assert_eq!(
        sent.last().unwrap().code.as_deref(),
        Some(codes::E_UPGRADE_INTEGRITY)
    );
    assert!(agent.lock().file.current.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rate_limit_resume_and_cancellation() {
    let dir = tmp("resume");
    let agent = UpgradeAgent::new(env(&dir, "0.9.0"));
    let mut cs = accept(&agent, offer((-60, 3600)));
    ack_all(&agent, &mut cs);
    let out = agent.poll(&mut cs, Instant::now()).unwrap();
    // 下載併發已滿：依 retry_after_ms（±20%）等待
    agent.on_error(&mut cs, &out, "rate_limited", Some(30_000));
    let now = Instant::now();
    assert_eq!(agent.poll(&mut cs, now), None);
    assert!(agent.poll(&mut cs, now + Duration::from_secs(20)).is_none());
    let Some(Out::Chunk {
        filename,
        offset,
        length,
        ..
    }) = agent.poll(&mut cs, now + Duration::from_secs(37))
    else {
        panic!("retry after the wait");
    };
    // 收到前半段後斷線
    let half = &TAR[..20];
    agent.on_chunk(&mut cs, &chunk(0, half, false), (filename, offset, length));
    drop(cs);
    // 重新啟動（新的行程）後續傳
    drop(agent);
    let agent = UpgradeAgent::new(env(&dir, "0.9.0"));
    assert_eq!(agent.view().bytes, Some(20));
    let mut cs = agent.on_registered(true, None);
    let progress = ack_all(&agent, &mut cs);
    assert_eq!(
        progress[0].progress,
        Some(UpgradeProgress {
            bytes: 20,
            total: 54
        }),
        "25% 的進度"
    );
    let Some(Out::Chunk { offset, length, .. }) = agent.poll(&mut cs, Instant::now()) else {
        panic!("resume");
    };
    assert_eq!((offset, length), (20, 34));
    // 推送被取消：刪除暫存
    let out = Out::Chunk {
        upgrade_id: ID.into(),
        filename: FILE.into(),
        offset,
        length,
    };
    agent.on_error(&mut cs, &out, "upgrade_unavailable", None);
    assert!(agent.lock().file.current.is_none());
    assert!(!dir.join("state/upgrade").join(ID).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn staged_upgrade_waits_for_the_window_and_the_config_confirmation_then_expires() {
    let dir = tmp("window");
    let agent = UpgradeAgent::new(env(&dir, "0.9.0"));
    // 時段在 1 小時後
    let mut cs = accept(&agent, offer((3600, 7200)));
    ack_all(&agent, &mut cs);
    let Some(Out::Chunk {
        filename,
        offset,
        length,
        ..
    }) = agent.poll(&mut cs, Instant::now())
    else {
        panic!();
    };
    agent.on_chunk(&mut cs, &chunk(0, TAR, true), (filename, offset, length));
    ack_all(&agent, &mut cs);
    agent.tick(&mut cs, Instant::now(), false);
    assert_eq!(
        agent.view().phase.as_deref(),
        Some("staged"),
        "時段還沒開始"
    );
    let wake = agent.next_wake(&cs, Instant::now(), false).unwrap();
    assert!(wake <= Instant::now() + Duration::from_secs(61));
    // 時段開始但集中設定還在確認期：不交付
    {
        let mut inner = agent.lock();
        let c = inner.file.current.as_mut().unwrap();
        c.offer.apply_window = ApplyWindow {
            not_before: ts(-10),
            not_after: ts(3600),
        };
    }
    agent.tick(&mut cs, Instant::now(), true);
    assert_eq!(agent.view().phase.as_deref(), Some("staged"));
    let now = Instant::now();
    assert!(
        agent.next_wake(&cs, now, true).unwrap() >= now + Duration::from_secs(5),
        "確認期內不空轉"
    );
    // 時段已過：expired，刪除暫存
    {
        let mut inner = agent.lock();
        let c = inner.file.current.as_mut().unwrap();
        c.offer.apply_window = ApplyWindow {
            not_before: ts(-7200),
            not_after: ts(-3600),
        };
    }
    agent.tick(&mut cs, Instant::now(), false);
    let sent = ack_all(&agent, &mut cs);
    assert_eq!(sent[0].state, UPGRADE_STATE_EXPIRED);
    assert!(!dir.join("state/upgrade").join(ID).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cancelled_rollout_is_not_applied_from_a_stale_staged_download() {
    let dir = tmp("cancelled");
    let agent = UpgradeAgent::new(env(&dir, "0.9.0"));
    let mut cs = accept(&agent, offer((-60, 3600)));
    ack_all(&agent, &mut cs);
    let Some(Out::Chunk {
        filename,
        offset,
        length,
        ..
    }) = agent.poll(&mut cs, Instant::now())
    else {
        panic!();
    };
    agent.on_chunk(&mut cs, &chunk(0, TAR, true), (filename, offset, length));
    ack_all(&agent, &mut cs);
    // 時段內再確認：Kairos 回 upgrade_unavailable（推送已取消）→ 不交付、保留暫存
    let now = Instant::now();
    agent.tick(&mut cs, now, false);
    let out = agent.poll(&mut cs, now).unwrap();
    assert_eq!(out, Out::Get(ID.into()));
    agent.on_error(&mut cs, &out, "upgrade_unavailable", None);
    agent.tick(&mut cs, now, false);
    assert_eq!(agent.poll(&mut cs, now), None, "10 分鐘內不再確認");
    assert_eq!(agent.view().phase.as_deref(), Some("staged"));
    assert!(dir.join("state/upgrade").join(ID).join(FILE).is_file());
    // 10 分鐘後再確認一次；提議內容被換掉時不採用
    agent.tick(&mut cs, now + Duration::from_secs(601), false);
    assert_eq!(
        agent.poll(&mut cs, now + Duration::from_secs(601)),
        Some(Out::Get(ID.into()))
    );
    let mut changed = offer((-60, 3600));
    changed.target.version = "0.9.2".into();
    agent.on_offer(&mut cs, changed);
    agent.tick(&mut cs, now + Duration::from_secs(602), false);
    assert_eq!(agent.view().phase.as_deref(), Some("staged"));
    assert!(!dir.join("state/upgrade/request.json").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reoffer_of_the_same_target_reuses_the_staged_files() {
    let dir = tmp("reoffer");
    let agent = UpgradeAgent::new(env(&dir, "0.9.0"));
    let mut cs = accept(&agent, offer((3600, 7200)));
    ack_all(&agent, &mut cs);
    let Some(Out::Chunk {
        filename,
        offset,
        length,
        ..
    }) = agent.poll(&mut cs, Instant::now())
    else {
        panic!();
    };
    agent.on_chunk(&mut cs, &chunk(0, TAR, true), (filename, offset, length));
    ack_all(&agent, &mut cs);
    let new_id = "1d3e7a10-4b2c-4f6e-8a1d-2c3b4a5f6e71";
    let mut o = offer((3600, 7200));
    o.upgrade_id = new_id.into();
    let mut cs = agent.on_registered(true, Some(&pending(new_id)));
    assert_eq!(
        agent.poll(&mut cs, Instant::now()),
        Some(Out::Get(new_id.into()))
    );
    agent.on_offer(&mut cs, o);
    let sent = ack_all(&agent, &mut cs);
    let got: Vec<(&str, &str)> = sent
        .iter()
        .map(|r| (r.upgrade_id.as_str(), r.state.as_str()))
        .collect();
    assert_eq!(got, [(new_id, "accepted"), (new_id, "staged")]);
    assert!(dir.join("state/upgrade").join(new_id).join(FILE).is_file());
    assert_eq!(agent.poll(&mut cs, Instant::now()), None, "不需要重新下載");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn readiness_is_detected_again_before_each_connection() {
    let dir = tmp("redetect");
    let mut e = env(&dir, "0.9.0");
    e.detection.upgrade = mgmt::INSTALL_UPGRADE_NO_HELPER;
    fn now_ready() -> Detection {
        Detection {
            method: mgmt::INSTALL_METHOD_DEB,
            upgrade: mgmt::INSTALL_UPGRADE_READY,
            reason: None,
            trust_anchor: PathBuf::new(),
            fingerprints: Vec::new(),
        }
    }
    e.redetect = Some(now_ready);
    let agent = UpgradeAgent::new(e);
    assert_eq!(agent.install_info().upgrade.as_deref(), Some("no_helper"));
    agent.refresh_detection();
    let info = agent.install_info();
    assert_eq!(
        (info.method.as_deref(), info.upgrade.as_deref()),
        (Some("deb"), Some("ready"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn install_info_and_status_text_limits() {
    let dir = tmp("info");
    let agent = UpgradeAgent::new(env(&dir, "0.9.0"));
    let info = agent.install_info();
    assert_eq!(info.method.as_deref(), Some("tar"));
    assert_eq!(info.upgrade.as_deref(), Some("ready"));
    // 白名單預設是空的：宣告不允許外部 URL（Kairos 的資格預覽據此事先標出；REMOTE.md 第 2.1 節）
    assert_eq!(info.external_url.as_deref(), Some("none"));
    mgmt::validate_install(&info).unwrap();
    let long = "é".repeat(600);
    let t = truncate_detail(&format!("x\u{202e}y{long}"));
    assert!(t.len() <= mgmt::MAX_UPGRADE_DETAIL_BYTES);
    assert!(t.starts_with("x?y"));
    // 沒有協商 upgrade 的連線：什麼都不做
    let mut cs = agent.on_registered(false, Some(&pending(ID)));
    assert_eq!(agent.poll(&mut cs, Instant::now()), None);
    assert_eq!(agent.next_wake(&cs, Instant::now(), false), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// hello 的 `install.external_url`（REMOTE.md 第 2.1 節）：只宣告白名單有沒有有效的項目，不含白名單的內容；
/// 與 `install.upgrade` 的狀態無關（停用、缺少輔助程式的主機也宣告）
#[test]
fn hello_declares_whether_external_urls_are_allowed_without_revealing_the_allowlist() {
    let dir = tmp("exturl");
    for (list, want) in [
        (vec![], "none"),
        // 不合法的項目不會比對到任何 URL：等同沒有白名單
        (
            vec!["http://repo.example.gov.tw/", "repo.example.gov.tw"],
            "none",
        ),
        (
            vec!["https://repo.example.gov.tw/kairos-pulse/"],
            "allowlist",
        ),
        (
            vec!["not a url", " https://repo.example.gov.tw/kairos-pulse "],
            "allowlist",
        ),
    ] {
        for upgrade in [
            mgmt::INSTALL_UPGRADE_READY,
            mgmt::INSTALL_UPGRADE_DISABLED,
            mgmt::INSTALL_UPGRADE_NO_HELPER,
        ] {
            let mut e = env(&dir, "0.9.0");
            e.detection.upgrade = upgrade;
            e.url_allowlist = list.iter().map(|s| s.to_string()).collect();
            let info = UpgradeAgent::new(e).install_info();
            assert_eq!(
                info.external_url.as_deref(),
                Some(want),
                "{list:?} {upgrade}"
            );
            mgmt::validate_install(&info).unwrap();
            // 送出去的只有固定的代碼：白名單的主機名稱與路徑不在 hello 中
            let json = serde_json::to_string(&info).unwrap();
            assert!(
                json.contains(&format!(r#""external_url":"{want}""#)),
                "{json}"
            );
            assert!(!json.contains("repo.example"), "{json}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 外部 URL 的版本庫（REMOTE.md 第 4.1.2 節，0.10.0）：URL 符合本機白名單且指向提議的檔名才接受；檔案由背景
/// 的下載工作取得（不送段落請求），下載完成後與分段下載走同一條驗證路徑（提議的 SHA-256、簽章驗證過的
/// SHA256SUMS）才 staged；內容不符時刪除重來，連續 3 次後放棄
#[test]
fn external_url_needs_the_local_allowlist_and_the_same_verification() {
    let url = format!("https://repo.example.gov.tw/kairos-pulse/0.9.1/{FILE}");
    let with_url = |u: &str| {
        let mut o = offer((-60, 3600));
        o.artifact.source = mgmt::SOURCE_URL.into();
        o.artifact.url = Some(u.to_string());
        o
    };
    fn allow(e: &mut UpgradeEnv, list: &[&str]) {
        e.url_allowlist = list.iter().map(|s| s.to_string()).collect();
    }
    // 不在白名單、白名單是另一台主機（字串前綴相同）、URL 指向別的檔名：一律 declined（url_not_allowed）
    for (i, (list, u)) in [
        (vec![], url.clone()),
        (vec!["https://repo.example.gov.tw/other/"], url.clone()),
        (
            vec!["https://repo.example.gov"],
            "https://repo.example.gov.tw/x/".to_string() + FILE,
        ),
        (
            vec!["https://repo.example.gov.tw/kairos-pulse/"],
            "https://repo.example.gov.tw/kairos-pulse/0.9.1/other.tar.gz".to_string(),
        ),
        (
            vec!["https://repo.example.gov.tw/kairos-pulse/"],
            "http://repo.example.gov.tw/kairos-pulse/0.9.1/".to_string() + FILE,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = tmp(&format!("url-declined-{i}"));
        let mut e = env(&dir, "0.9.0");
        allow(&mut e, &list);
        let agent = UpgradeAgent::new(e);
        let _cs = accept(&agent, with_url(&u));
        let s = statuses(&agent);
        assert_eq!(s.len(), 1, "{i}: {s:?}");
        assert_eq!(s[0].state, mgmt::UPGRADE_STATE_DECLINED, "{i}");
        assert!(agent.next_url_job(Instant::now()).is_none(), "{i}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 在白名單內：接受，不送段落請求，由下載工作取得
    let dir = tmp("url-ok");
    let mut e = env(&dir, "0.9.0");
    allow(&mut e, &["https://repo.example.gov.tw/kairos-pulse"]);
    let agent = UpgradeAgent::new(e);
    let mut cs = accept(&agent, with_url(&url));
    let sent = ack_all(&agent, &mut cs);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].state, UPGRADE_STATE_ACCEPTED);
    assert_eq!(
        agent.poll(&mut cs, Instant::now()),
        None,
        "不經 Kairos 下載"
    );
    let job = agent.next_url_job(Instant::now()).expect("url job");
    assert_eq!(
        (job.url.as_str(), job.filename.as_str(), job.size),
        (url.as_str(), FILE, TAR.len() as u64)
    );
    // 下載失敗：稍後重試（等待期內沒有工作）
    agent.on_url_result(
        &job,
        Err(fetch::FetchError::Transient("連線：refused".into())),
    );
    assert!(agent.next_url_job(Instant::now()).is_none());
    assert!(
        agent
            .next_url_job(Instant::now() + Duration::from_secs(120))
            .is_some()
    );
    // 內容被換掉（大小相同、雜湊不符）：刪除重來，不會 staged
    let mut bad = TAR.to_vec();
    bad[0] ^= 1;
    std::fs::write(&job.part, &bad).unwrap();
    agent.on_url_result(&job, Ok(()));
    assert!(!job.part.exists(), "雜湊不符的暫存檔要刪除");
    assert!(statuses(&agent).is_empty());
    // 正確的內容：staged
    std::fs::write(&job.part, TAR).unwrap();
    agent.on_url_result(&job, Ok(()));
    let sent = ack_all(&agent, &mut cs);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].state, mgmt::UPGRADE_STATE_STAGED);
    assert!(agent.next_url_job(Instant::now()).is_none());
    // 過期的下載工作（提議已被取代）不影響狀態
    agent.on_url_result(&job, Ok(()));
    let _ = std::fs::remove_dir_all(&dir);

    // 連續 3 次雜湊不符：放棄（failed，PLS-E8402），與分段下載相同
    let dir = tmp("url-mismatch");
    let mut e = env(&dir, "0.9.0");
    allow(&mut e, &["https://repo.example.gov.tw/"]);
    let agent = UpgradeAgent::new(e);
    let mut cs = accept(&agent, with_url(&url));
    ack_all(&agent, &mut cs);
    for _ in 0..3 {
        let job = agent
            .next_url_job(Instant::now() + Duration::from_secs(600))
            .expect("url job");
        std::fs::write(&job.part, &bad).unwrap();
        agent.on_url_result(&job, Ok(()));
    }
    let s = states(&agent);
    assert_eq!(
        s.last().map(|x| x.0.as_str()),
        Some(mgmt::UPGRADE_STATE_FAILED),
        "{s:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- 回滾檔的簽章資料（rollback_bundle；ADR-0003 階段 1、REMOTE.md 第 4.4.2 節） ----

/// deb 主機（目前 0.9.1）收到升級到 0.9.2 的提議：以臨時金鑰簽署的兩次發佈
struct BundleFx {
    dir: PathBuf,
    key: testsign::TestKey,
    /// 目標（0.9.2）的 deb
    target: (String, Vec<u8>),
    /// 回滾檔（0.9.1）的 deb
    rollback: (String, Vec<u8>),
    rollback_release: testsign::TestRelease,
    offer: UpgradeOffer,
}

impl BundleFx {
    fn new(name: &str) -> Self {
        let dir = tmp(name);
        let key = testsign::TestKey::generate();
        let target = (
            "pulse-0.9.2-6f7a8b9-linux-x86_64.deb".to_string(),
            b"deb 0.9.2\n".to_vec(),
        );
        let rollback = (
            "pulse-0.9.1-5e6f7a8-linux-x86_64.deb".to_string(),
            b"deb 0.9.1\n".to_vec(),
        );
        let rel = testsign::release(&key, "0.9.2", "6f7a8b9", &[(&target.0, &target.1)], None);
        let rollback_release = testsign::release(
            &key,
            "0.9.1",
            "5e6f7a8",
            &[(&rollback.0, &rollback.1)],
            None,
        );
        let artifact = |f: &(String, Vec<u8>)| mgmt::ArtifactRef {
            filename: f.0.clone(),
            size: f.1.len() as u64,
            sha256: files::hex(ring::digest::digest(&ring::digest::SHA256, &f.1).as_ref()),
            source: mgmt::SOURCE_KAIROS.into(),
            url: None,
        };
        let offer = UpgradeOffer {
            seq: 5,
            upgrade_id: ID.into(),
            rollout_id: "r-20261210-0001".into(),
            action: mgmt::ACTION_UPGRADE.into(),
            target: mgmt::UpgradeTarget {
                version: "0.9.2".into(),
                commit: "6f7a8b9".into(),
            },
            artifact: artifact(&target),
            rollback_artifact: Some(artifact(&rollback)),
            rollback_bundle: Some(mgmt::RollbackBundle {
                sha256sums: rollback_release.sums.clone(),
                signature: rollback_release.sig.clone(),
            }),
            manifest: Some(mgmt::ManifestRef {
                filename: rel.manifest_name.clone(),
                content: rel.manifest.clone(),
            }),
            sha256sums: rel.sums.clone(),
            signature: rel.sig.clone(),
            apply_window: ApplyWindow {
                not_before: ts(-60),
                not_after: ts(3600),
            },
            health_timeout_secs: 600,
            allow_downgrade: false,
        };
        mgmt::validate_upgrade_offer(&offer).unwrap();
        Self {
            dir,
            key,
            target,
            rollback,
            rollback_release,
            offer,
        }
    }

    fn agent(&self, method: &'static str) -> Arc<UpgradeAgent> {
        let mut e = env(&self.dir, "0.9.1");
        write_anchor(&e.detection.trust_anchor, &self.key.allowed_signers());
        e.detection.method = method;
        UpgradeAgent::new(e)
    }

    /// 本機紀錄（`releases/0.9.1/`）
    fn write_record(&self, sums: &str, sig: &str) {
        let rel = self.dir.join("state/upgrade/releases/0.9.1");
        std::fs::create_dir_all(&rel).unwrap();
        std::fs::write(rel.join(files::SUMS_FILE), sums).unwrap();
        std::fs::write(rel.join(files::SIG_FILE), sig).unwrap();
    }

    /// 這次提議要下載的檔名、是否採用回滾檔、來源
    fn accepted(agent: &UpgradeAgent) -> (Vec<String>, bool, Option<String>) {
        let inner = agent.lock();
        let c = inner.file.current.as_ref().expect("提議被接受");
        (
            c.files.iter().map(|f| f.name.clone()).collect(),
            c.rollback_trusted,
            c.rollback_source.clone(),
        )
    }
}

/// 新 Kairos × 新 Pulse（ADR-0003 第 6.4 節）：第一次遠端升級的 deb 主機本機沒有目前版本的紀錄，以提議的
/// `rollback_bundle` 驗證回滾檔——下載、暫存它的簽章資料、寫進 `request.json`（輔助程式會以 root 重新驗證）
#[test]
fn a_first_remote_upgrade_uses_the_rollback_bundle_from_the_offer() {
    let fx = BundleFx::new("bundle-first");
    let agent = fx.agent(mgmt::INSTALL_METHOD_DEB);
    let mut cs = accept(&agent, fx.offer.clone());
    assert_eq!(
        BundleFx::accepted(&agent),
        (
            vec![fx.target.0.clone(), fx.rollback.0.clone()],
            true,
            Some(ROLLBACK_SOURCE_BUNDLE.to_string())
        )
    );
    ack_all(&agent, &mut cs);
    // 下載兩個檔案
    let mut served = Vec::new();
    let mut reported = Vec::new();
    // 段落請求之間有最短間隔：每次往後推
    let mut now = Instant::now();
    while let Some(Out::Chunk {
        filename,
        offset,
        length,
        ..
    }) = {
        now += Duration::from_secs(60);
        // 進度回報（每 25%）先送出並收到 ack，poll 才輪到段落請求
        reported.extend(ack_all(&agent, &mut cs));
        agent.poll(&mut cs, now)
    } {
        let data = if filename == fx.target.0 {
            &fx.target.1
        } else {
            assert_eq!(filename, fx.rollback.0);
            &fx.rollback.1
        };
        assert_eq!((offset, length as usize), (0, data.len()));
        served.push(filename.clone());
        let c = UpgradeChunk::new(9, ID, filename.clone(), offset, data, true);
        agent.on_chunk(&mut cs, &c, (filename, offset, length));
    }
    assert_eq!(served.len(), 2, "{served:?}");
    reported.extend(ack_all(&agent, &mut cs));
    assert_eq!(
        reported.last().map(|r| r.state.as_str()),
        Some(mgmt::UPGRADE_STATE_STAGED),
        "{reported:?}"
    );
    // 暫存：回滾檔與它所屬發佈的簽章資料（來自 bundle）
    let staging = fx.dir.join("state/upgrade").join(ID);
    assert_eq!(
        std::fs::read_to_string(staging.join(files::ROLLBACK_SUMS_FILE)).unwrap(),
        fx.rollback_release.sums
    );
    assert_eq!(
        std::fs::read_to_string(staging.join(files::ROLLBACK_SIG_FILE)).unwrap(),
        fx.rollback_release.sig
    );
    assert_eq!(
        std::fs::read(staging.join(&fx.rollback.0)).unwrap(),
        fx.rollback.1
    );
    // bundle 不會被存成本機紀錄（本機紀錄只記這台主機驗證並暫存過的**目標**發佈）
    assert!(!fx.dir.join("state/upgrade/releases/0.9.1").exists());
    assert!(
        fx.dir
            .join("state/upgrade/releases/0.9.2/SHA256SUMS")
            .is_file()
    );
    // 交付：request.json 帶回滾檔
    agent.tick(&mut cs, Instant::now(), false);
    assert_eq!(
        agent.poll(&mut cs, Instant::now()),
        Some(Out::Get(ID.into()))
    );
    agent.on_offer(&mut cs, fx.offer.clone());
    agent.tick(&mut cs, Instant::now(), false);
    ack_all(&agent, &mut cs);
    let req = Request::from_untrusted_bytes(
        &std::fs::read(fx.dir.join("state/upgrade/request.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(req.method, "deb");
    assert_eq!(
        req.rollback_artifact.as_deref(),
        Some(fx.rollback.0.as_str())
    );
    let _ = std::fs::remove_dir_all(&fx.dir);
}

type OfferTweak = fn(&BundleFx, &mut UpgradeOffer, &testsign::TestKey);

/// 來源的順序與資格（ADR-0003 第 5.4 節；INV-4、INV-5）：本機紀錄優先於 bundle；兩者都只是提示，都以
/// 本機信任錨完整驗證；都不通過時**照常升級**、不下載回滾檔（Kairos 能做的只有附或不附，驗證失敗不會
/// 讓升級失敗，也不會放寬條件）。新 Pulse × 舊 Kairos（沒有 bundle）與 0.11.0 以前相同
#[test]
fn rollback_bundle_and_local_record_are_hints_verified_with_the_local_trust_anchor() {
    let other = testsign::TestKey::generate();
    let cases: [(&str, &'static str, OfferTweak, Option<&str>); 9] = [
        // 舊 Kairos：沒有 bundle、本機也沒有紀錄
        ("no-bundle", "deb", |_, o, _| o.rollback_bundle = None, None),
        // 舊 Kairos＋本機紀錄（0.11.0 以前就有的路徑）
        (
            "record-only",
            "deb",
            |fx, o, _| {
                o.rollback_bundle = None;
                fx.write_record(&fx.rollback_release.sums, &fx.rollback_release.sig);
            },
            Some(ROLLBACK_SOURCE_RECORD),
        ),
        // 本機紀錄有效：優先於 bundle（即使 bundle 是壞的）
        (
            "record-first",
            "deb",
            |fx, o, _| {
                fx.write_record(&fx.rollback_release.sums, &fx.rollback_release.sig);
                o.rollback_bundle.as_mut().unwrap().signature = "junk".into();
            },
            Some(ROLLBACK_SOURCE_RECORD),
        ),
        // 本機紀錄被改壞（服務帳號可寫，只是提示）：改看 bundle
        (
            "record-bad",
            "deb",
            |fx, _, _| fx.write_record(&fx.rollback_release.sums, "junk"),
            Some(ROLLBACK_SOURCE_BUNDLE),
        ),
        // bundle 的簽章不是本機信任錨接受的金鑰簽的
        (
            "bundle-other-key",
            "deb",
            |fx, o, other| {
                let r = testsign::release(
                    other,
                    "0.9.1",
                    "5e6f7a8",
                    &[(&fx.rollback.0, &fx.rollback.1)],
                    None,
                );
                o.rollback_bundle = Some(mgmt::RollbackBundle {
                    sha256sums: r.sums,
                    signature: r.sig,
                });
            },
            None,
        ),
        // bundle 的內容被改過（簽章對不上）
        (
            "bundle-tampered",
            "deb",
            |_, o, _| {
                o.rollback_bundle
                    .as_mut()
                    .unwrap()
                    .sha256sums
                    .push_str("# x\n")
            },
            None,
        ),
        // 官方簽署，但回滾檔不在這份 SHA256SUMS 中（拿目標版本的簽章資料充當）
        (
            "bundle-other-release",
            "deb",
            |_, o, _| {
                o.rollback_bundle = Some(mgmt::RollbackBundle {
                    sha256sums: o.sha256sums.clone(),
                    signature: o.signature.clone(),
                })
            },
            None,
        ),
        // 回滾檔的雜湊與 bundle 列的不同
        (
            "artifact-hash",
            "deb",
            |_, o, _| o.rollback_artifact.as_mut().unwrap().sha256 = "0".repeat(64),
            None,
        ),
        // tar 不需要回滾檔：不論有沒有 bundle 都忽略
        ("tar", "tar", |_, _, _| {}, None),
    ];
    for (name, method, tweak, source) in cases {
        let fx = BundleFx::new(&format!("bundle-{name}"));
        let mut o = fx.offer.clone();
        if method == "tar" {
            // tar 主機的提議：目標是 tar.gz（另簽一次），回滾檔與 bundle 照附
            let t = (
                "pulse-0.9.2-6f7a8b9-linux-x86_64.tar.gz".to_string(),
                b"tar 0.9.2\n".to_vec(),
            );
            let rel = testsign::release(&fx.key, "0.9.2", "6f7a8b9", &[(&t.0, &t.1)], None);
            o.artifact.filename = t.0.clone();
            o.artifact.size = t.1.len() as u64;
            o.artifact.sha256 =
                files::hex(ring::digest::digest(&ring::digest::SHA256, &t.1).as_ref());
            o.manifest = Some(mgmt::ManifestRef {
                filename: rel.manifest_name,
                content: rel.manifest,
            });
            o.sha256sums = rel.sums;
            o.signature = rel.sig;
        }
        tweak(&fx, &mut o, &other);
        let agent = fx.agent(method);
        let mut cs = accept(&agent, o.clone());
        // 不論回滾檔能不能用，提議都被接受（不是 declined、也不是驗證失敗）
        let sent = ack_all(&agent, &mut cs);
        assert_eq!(
            sent.iter().map(|s| s.state.as_str()).collect::<Vec<_>>(),
            [UPGRADE_STATE_ACCEPTED],
            "{name}"
        );
        let (names, trusted, got) = BundleFx::accepted(&agent);
        assert_eq!(got.as_deref(), source, "{name}");
        assert_eq!(trusted, source.is_some(), "{name}");
        assert_eq!(
            names.len(),
            if source.is_some() { 2 } else { 1 },
            "{name}: {names:?}"
        );
        assert_eq!(names[0], o.artifact.filename, "{name}");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }
}

/// 0.11.0 以前的狀態檔（`Current` 沒有 `rollback_source`）在升級後照常讀得回來：有 `rollback_trusted`
/// 而沒有來源＝本機紀錄
#[test]
fn state_files_written_before_the_rollback_bundle_still_load() {
    let fx = BundleFx::new("bundle-old-state");
    fx.write_record(&fx.rollback_release.sums, &fx.rollback_release.sig);
    let agent = fx.agent(mgmt::INSTALL_METHOD_DEB);
    let mut o = fx.offer.clone();
    o.rollback_bundle = None;
    let _cs = accept(&agent, o);
    let c = agent.lock().file.current.clone().unwrap();
    assert_eq!(c.rollback_source.as_deref(), Some(ROLLBACK_SOURCE_RECORD));
    let mut v = serde_json::to_value(&c).unwrap();
    // 舊版寫的：沒有 rollback_source，offer 也沒有 rollback_bundle
    v.as_object_mut().unwrap().remove("rollback_source");
    assert!(v["offer"].get("rollback_bundle").is_none());
    let old: Current = serde_json::from_value(v).unwrap();
    assert!(old.rollback_trusted);
    assert_eq!(old.rollback_source, None);
    let _ = std::fs::remove_dir_all(&fx.dir);
}

/// 降級回滾的結果（輔助程式寫的 `result.json`）：照樣回報 `rolled_back`，代碼與 `detail` 原樣轉給 Kairos；
/// `pulse upgrade status`／`pulse status` 標示需要修復套件
#[test]
fn a_degraded_rollback_is_forwarded_and_flagged_for_package_repair() {
    let dir = tmp("degraded");
    let agent = UpgradeAgent::new(env(&dir, "0.9.0"));
    let mut cs = accept(&agent, offer((-60, 3600)));
    ack_all(&agent, &mut cs);
    let Some(Out::Chunk {
        filename,
        offset,
        length,
        ..
    }) = agent.poll(&mut cs, Instant::now())
    else {
        panic!("expected a chunk request");
    };
    agent.on_chunk(&mut cs, &chunk(0, TAR, true), (filename, offset, length));
    ack_all(&agent, &mut cs);
    agent.tick(&mut cs, Instant::now(), false);
    assert_eq!(
        agent.poll(&mut cs, Instant::now()),
        Some(Out::Get(ID.into()))
    );
    agent.on_offer(&mut cs, offer((-60, 3600)));
    agent.tick(&mut cs, Instant::now(), false);
    ack_all(&agent, &mut cs);
    assert!(!agent.view().package_repair_needed);
    let detail = "degraded:package_rollback_failed,package_db_inconsistent; crash_restart (NRestarts 0 → 1); rollback: dpkg: error";
    let result = UpgradeResult {
        schema: files::RESULT_SCHEMA.into(),
        upgrade_id: ID.into(),
        state: UPGRADE_STATE_ROLLED_BACK.into(),
        code: Some(codes::W_UPGRADE_ROLLED_BACK_DEGRADED.into()),
        detail: detail.into(),
        from_version: "0.9.0".into(),
        to_version: "0.9.1".into(),
        finished_at: ts(0),
    };
    std::fs::write(
        dir.join("state/upgrade/result.json"),
        serde_json::to_vec(&result).unwrap(),
    )
    .unwrap();
    drop(agent);
    // 回滾後舊版的行程重新啟動：讀 result.json 並回報
    let agent = UpgradeAgent::new(env(&dir, "0.9.0"));
    let mut cs = agent.on_registered(true, None);
    let sent = ack_all(&agent, &mut cs);
    let last = sent.last().expect("回報結果");
    assert_eq!(
        (
            last.state.as_str(),
            last.code.as_deref(),
            last.detail.as_str()
        ),
        (
            "rolled_back",
            Some(codes::W_UPGRADE_ROLLED_BACK_DEGRADED),
            detail
        )
    );
    assert!(agent.view().package_repair_needed);
    let _ = std::fs::remove_dir_all(&dir);
}

/// hello 的 `install.rollback`（REMOTE.md 第 2.1、17.10.6 節）：由輔助程式（root）留下的提示算出。提示只在
/// 它描述的是這個行程的執行檔時採用；`degraded` 另外要求套件資料庫的狀態沒有變。提示在服務帳號可寫的
/// 目錄裡——它只決定回報給 Kairos 的值，讀不到、不合法或不相符時一律退回 `binary_only`
#[test]
fn hello_declares_the_rollback_capability_from_the_helper_hint() {
    const OWN: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const OTHER: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    fn db(_: &str) -> Option<String> {
        Some("0.9.2 ii".to_string())
    }
    let dir = tmp("rollback-state");
    let updir = dir.join("state/upgrade");
    std::fs::create_dir_all(&updir).unwrap();
    let write = |state: &str, binary: &str, pkg_db: Option<&str>| {
        let h = files::RollbackHint {
            schema: files::ROLLBACK_HINT_SCHEMA.into(),
            state: state.into(),
            binary_sha256: binary.into(),
            pkg_db: pkg_db.map(str::to_string),
            updated_at: ts(0),
        };
        std::fs::write(
            updir.join(files::ROLLBACK_HINT_FILE),
            serde_json::to_vec(&h).unwrap(),
        )
        .unwrap();
    };
    let state = |ready: bool, method: &str, own: Option<&str>| {
        rollback_state(ready, method, &updir, own, db)
    };
    // 沒有提示
    assert_eq!(state(true, "deb", Some(OWN)), "binary_only");
    assert_eq!(state(true, "tar", Some(OWN)), "binary_only");
    assert_eq!(state(false, "deb", Some(OWN)), "none");
    // archived：描述的是這個行程的執行檔才算
    write("archived", OWN, Some("0.9.1 ii"));
    assert_eq!(state(true, "deb", Some(OWN)), "archived");
    assert_eq!(state(true, "rpm", Some(OWN)), "archived");
    assert_eq!(
        state(true, "deb", Some(OTHER)),
        "binary_only",
        "升級或回滾之後舊的提示失效"
    );
    assert_eq!(state(true, "deb", None), "binary_only", "算不出自己的雜湊");
    assert_eq!(state(true, "tar", Some(OWN)), "binary_only", "tar 沒有封存");
    assert_eq!(state(false, "deb", Some(OWN)), "none", "遠端升級沒有就緒");
    // degraded：套件資料庫仍是降級回滾當時的狀態才算；重新安裝套件之後不再是
    write("degraded", OWN, Some("0.9.2 ii"));
    assert_eq!(state(true, "deb", Some(OWN)), "degraded");
    write("degraded", OWN, Some("0.9.3 ii"));
    assert_eq!(state(true, "deb", Some(OWN)), "binary_only");
    write("degraded", OTHER, Some("0.9.2 ii"));
    assert_eq!(state(true, "deb", Some(OWN)), "binary_only");
    // 不認得的狀態、schema 或不是合法的內容
    write("future", OWN, None);
    assert_eq!(state(true, "deb", Some(OWN)), "binary_only");
    std::fs::write(
        updir.join(files::ROLLBACK_HINT_FILE),
        r#"{"schema":"pulse.upgrade-rollback/v9","state":"archived","binary_sha256":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
    )
    .unwrap();
    assert_eq!(state(true, "deb", Some(OWN)), "binary_only");
    std::fs::write(updir.join(files::ROLLBACK_HINT_FILE), "{").unwrap();
    assert_eq!(state(true, "deb", Some(OWN)), "binary_only");
    for s in ["archived", "binary_only", "none", "degraded"] {
        assert!(mgmt::INSTALL_ROLLBACK_STATES.contains(&s));
    }

    // 經 UpgradeAgent：hello 的 install 與 view 都帶出；需要修復套件以提示為準
    let mut e = env(&dir, "0.9.1");
    e.detection.method = mgmt::INSTALL_METHOD_DEB;
    e.own_binary_sha256 = Some(OWN.to_string());
    e.pkg_db = db;
    write("archived", OWN, None);
    let agent = UpgradeAgent::new(e);
    let info = agent.install_info();
    assert_eq!(info.rollback.as_deref(), Some("archived"));
    mgmt::validate_install(&info).unwrap();
    let v = agent.view();
    assert_eq!(v.rollback.as_deref(), Some("archived"));
    assert!(!v.package_repair_needed);
    // 與其他 install 欄位相同：每次連線前重新計算
    write("degraded", OWN, Some("0.9.2 ii"));
    assert_eq!(agent.install_info().rollback.as_deref(), Some("degraded"));
    assert!(agent.view().package_repair_needed);
    let _ = std::fs::remove_dir_all(&dir);
}
