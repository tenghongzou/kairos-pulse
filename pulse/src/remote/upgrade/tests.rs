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
    let req = Request::read_untrusted(&dir.join("state/upgrade/request.json")).unwrap();
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
