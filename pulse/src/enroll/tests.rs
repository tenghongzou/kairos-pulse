//! 以假 8444 伺服器（[`super::fake`]）跑完整流程：自動核准、人工核准（長輪詢）、拒絕、token 無效、
//! 逾期、撤回、`clock_skew`、`grace_clock_rejected`、`replayed_request`、429、回應遺失後重送、
//! 伺服器身分（CA 指紋、P-384／RSA 根 CA、中途替換 CA）、Kairos 不支援。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use kairos_proto::enroll::{
    self as ke, EnrollErrorCode as E, EnrollToken, InitiatedBy, RequestState, ServerVerification,
    TokenApproval, WithdrawReason,
};
use tokio_util::sync::CancellationToken;

use super::client::{ApiError, EnrollClient, NowMs};
use super::fake::{CaKind, FakeKairos, Options, Script};
use super::flow::{self, Driver, Event, Finish, Observer, RequestInfo, RunOutcome, Timing};
use super::store::{self, EnrollmentFile, FileKind, IdentityPaths, grace_state, state};
use super::trust::TrustSpec;
use super::{NewEnrollment, keys, prepare, x509};
use crate::codes;

const AGENT: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
const FP: &str = "sha256:355cf5ed40956a057645145cccf74f594102726167e94e3511aae182acc4ec43";
const AUTO: &str = "KRT1-GE8Q-MMGW-WG5P-DP97";

fn temp_paths() -> IdentityPaths {
    let dir: PathBuf = std::env::temp_dir()
        .join("pulse-enroll-flow-tests")
        .join(uuid::Uuid::new_v4().to_string())
        .join("identity");
    IdentityPaths::new(dir)
}

/// 可調整的本機時鐘（系統時間＋偏移）
#[derive(Clone, Default)]
struct Clock(Arc<AtomicI64>);

impl Clock {
    fn now(&self) -> NowMs {
        let off = self.0.clone();
        Arc::new(move || chrono::Utc::now().timestamp_millis() + off.load(Ordering::SeqCst))
    }
    fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

/// 記下所有事件
#[derive(Default)]
struct Log(Vec<(String, Event)>);

impl Observer for Log {
    fn on(&mut self, file: &EnrollmentFile, event: &Event) {
        self.0.push((file.state.clone(), event.clone()));
    }
}

fn token(n: u8) -> String {
    EnrollToken::from_random(&[n; 10]).canonical()
}

fn info() -> RequestInfo {
    RequestInfo {
        hostname: "plc-hmi-07".into(),
        pulse_version: "0.5.0".into(),
        display_name: Some("PLC HMI 07".into()),
        tags: Some(vec!["site:tainan".into()]),
        host_fingerprint: Some(FP.into()),
        os: None,
    }
}

fn new_enrollment(fake: &FakeKairos, tok: &str) -> NewEnrollment {
    NewEnrollment {
        agent_id: AGENT.into(),
        server: Some("127.0.0.1:8443".into()),
        enroll_server: fake.endpoint.clone(),
        token: EnrollToken::parse(tok).unwrap(),
        verification: ServerVerification::CaPin,
        verified_by: "ca_pin".into(),
        ca_pin: Some(fake.ca_pin.clone()),
        trusted_pins: vec![fake.ca_pin.clone()],
        info: info(),
        previous_serial: None,
    }
}

fn client(fake: &FakeKairos, clock: &Clock) -> EnrollClient {
    EnrollClient::new(
        &fake.endpoint,
        TrustSpec::Pin(fake.ca_pin.clone()),
        false,
        clock.now(),
    )
    .unwrap()
}

fn fast() -> Timing {
    Timing {
        clock_retry: Duration::from_secs(60),
        long_poll_secs: 2,
        clock_check: Duration::from_millis(50),
    }
}

async fn run_with(
    fake: &FakeKairos,
    paths: &IdentityPaths,
    file: EnrollmentFile,
    clock: &Clock,
    log: &mut Log,
    secs: u64,
) -> (RunOutcome, EnrollmentFile) {
    let mut d =
        Driver::new(paths.clone(), file, client(fake, clock), log).with_rand(Box::new(|| 0.5));
    d.timing = fast();
    d.long_poll = true;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let out = d.run(Some(deadline), &CancellationToken::new()).await;
    let file = d.file.clone();
    (out, file)
}

fn assert_installed(paths: &IdentityPaths, fake: &FakeKairos) -> x509::CertInfo {
    let pem = std::fs::read_to_string(paths.client_cert()).unwrap();
    let leaf = x509::parse_cert(&x509::pem_certs(&pem).unwrap()[0]).unwrap();
    assert_eq!(leaf.dns_sans, [AGENT]);
    let key = keys::pkcs8_from_pem(&std::fs::read(paths.client_key()).unwrap()).unwrap();
    assert_eq!(
        ke::p256_spki_of_pkcs8(&key).unwrap(),
        leaf.spki,
        "私鑰與憑證成對"
    );
    let ca = std::fs::read_to_string(paths.ca()).unwrap();
    let ca = x509::parse_cert(&x509::pem_certs(&ca).unwrap()[0]).unwrap();
    assert_eq!(
        ke::ca_pin_from_spki(&ca.spki),
        fake.ca_pin,
        "ca.pem 為被釘選的 CA"
    );
    assert!(
        !paths.request_key().exists(),
        "申請中的私鑰已改名為 client.key"
    );
    leaf
}

#[tokio::test]
async fn auto_approved_token_installs_the_certificate() {
    let fake = FakeKairos::start(Options::default()).await;
    fake.lock().add_token(AUTO, TokenApproval::Auto, "valid");
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, AUTO)).unwrap();
    assert!(file.token.is_some(), "申請建立前暫存 token");
    let clock = Clock::default();
    let mut log = Log::default();
    let (out, file) = run_with(&fake, &paths, file, &clock, &mut log, 10).await;
    assert_eq!(out, RunOutcome::Finished(Finish::Installed));
    assert_eq!(file.state, state::ISSUED);
    assert_eq!(file.token, None, "受理後 token 不留在磁碟");
    assert_eq!(file.poll_secret, None);
    assert_eq!(file.grace_days, Some(90));
    assert!(file.request_code.is_some());
    let leaf = assert_installed(&paths, &fake);
    assert_eq!(
        file.cert_serial.as_deref(),
        Some(leaf.serial_hex().as_str())
    );
    assert_eq!(fake.lock().token_consumed(AUTO), 1);
    let saved = paths.load_enrollment().unwrap().unwrap();
    assert_eq!(saved.state, state::ISSUED);
    assert!(
        !std::fs::read_to_string(paths.enrollment())
            .unwrap()
            .contains("KRT1-")
    );
    let events: Vec<&Event> = log.0.iter().map(|(_, e)| e).collect();
    assert!(
        matches!(
            events.as_slice(),
            [Event::Submitted, Event::Approved, Event::Installed]
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn manual_approval_with_long_poll() {
    let fake = FakeKairos::start(Options::default()).await;
    let tok = token(1);
    fake.lock().add_token(&tok, TokenApproval::Manual, "valid");
    fake.lock().poll_after_ms = 100;
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, &tok)).unwrap();
    let clock = Clock::default();
    let approver = {
        let state = fake.state.clone();
        async move {
            loop {
                let id = state.lock().unwrap().requests.first().map(|r| r.id.clone());
                if let Some(id) = id
                    && state.lock().unwrap().hits("poll") >= 1
                {
                    return id;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    let mut log = Log::default();
    let run = run_with(&fake, &paths, file, &clock, &mut log, 20);
    let (id, (out, file)) = tokio::join!(
        async {
            let id = approver.await;
            fake.approve(&id);
            id
        },
        run
    );
    assert_eq!(out, RunOutcome::Finished(Finish::Installed));
    assert_eq!(file.request_id.as_deref(), Some(id.as_str()));
    assert_eq!(file.decided_by.as_deref(), Some("admin.chen"));
    assert!(
        fake.lock().waits.contains(&2),
        "使用長輪詢：{:?}",
        fake.lock().waits
    );
    // 申請編號兩端相同
    let code = fake.lock().requests[0].code.to_string();
    assert_eq!(file.request_code.as_deref(), Some(code.as_str()));
    assert_installed(&paths, &fake);
    assert!(
        log.0
            .iter()
            .any(|(_, e)| matches!(e, Event::Pending { first: true, .. }))
    );
}

#[tokio::test]
async fn pending_request_resumes_from_enrollment_json_without_resubmitting() {
    let fake = FakeKairos::start(Options::default()).await;
    let tok = token(2);
    fake.lock().add_token(&tok, TokenApproval::Manual, "valid");
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, &tok)).unwrap();
    let clock = Clock::default();
    let mut log = Log::default();
    // 第一次：送出後等待到期限（模擬 Ctrl+C 或 --no-wait）
    let (out, _) = run_with(&fake, &paths, file, &clock, &mut log, 1).await;
    assert_eq!(out, RunOutcome::TimedOut);
    let saved = paths.load_enrollment().unwrap().unwrap();
    assert_eq!(saved.state, state::PENDING_APPROVAL);
    assert!(saved.next_poll_at.is_some());
    let id = saved.request_id.clone().unwrap();
    fake.approve(&id);
    // 第二次：從 enrollment.json 接續（不再送出申請）
    let (out, _) = run_with(&fake, &paths, saved, &clock, &mut log, 10).await;
    assert_eq!(out, RunOutcome::Finished(Finish::Installed));
    assert_eq!(fake.lock().hits("requests"), 1, "沒有重送新申請");
    assert_eq!(fake.lock().requests.len(), 1);
}

#[tokio::test]
async fn rejected_request_deletes_the_unused_key() {
    let fake = FakeKairos::start(Options::default()).await;
    let tok = token(3);
    fake.lock().add_token(&tok, TokenApproval::Manual, "valid");
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, &tok)).unwrap();
    let clock = Clock::default();
    let mut log = Log::default();
    let (_, file) = run_with(&fake, &paths, file, &clock, &mut log, 1).await;
    fake.reject(file.request_id.as_deref().unwrap(), "非本單位主機");
    let (out, file) = run_with(&fake, &paths, file, &clock, &mut log, 10).await;
    assert_eq!(
        out,
        RunOutcome::Finished(Finish::Stopped {
            code: codes::E_REQUEST_REJECTED
        })
    );
    assert_eq!(file.state, state::REJECTED);
    assert_eq!(file.reject_reason.as_deref(), Some("非本單位主機"));
    assert!(!paths.request_key().exists(), "刪除未使用的私鑰");
    assert!(!paths.client_cert().exists());
}

#[tokio::test]
async fn expired_request_stops_with_e3303() {
    let fake = FakeKairos::start(Options::default()).await;
    let tok = token(4);
    fake.lock().add_token(&tok, TokenApproval::Manual, "valid");
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, &tok)).unwrap();
    let clock = Clock::default();
    let mut log = Log::default();
    let (_, file) = run_with(&fake, &paths, file, &clock, &mut log, 1).await;
    fake.expire(file.request_id.as_deref().unwrap());
    let (out, file) = run_with(&fake, &paths, file, &clock, &mut log, 10).await;
    assert_eq!(
        out,
        RunOutcome::Finished(Finish::Stopped {
            code: codes::E_REQUEST_EXPIRED
        })
    );
    assert_eq!(file.state, state::REQUEST_EXPIRED);
    // 申請已被清除（輪詢回 404 request_not_found）也視同逾期
    let tok = token(5);
    fake.lock().add_token(&tok, TokenApproval::Manual, "valid");
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, &tok)).unwrap();
    let (_, file) = run_with(&fake, &paths, file, &clock, &mut log, 1).await;
    fake.lock()
        .requests
        .retain(|r| Some(&r.id) != file.request_id.as_ref());
    let (out, _) = run_with(&fake, &paths, file, &clock, &mut log, 10).await;
    assert_eq!(
        out,
        RunOutcome::Finished(Finish::Stopped {
            code: codes::E_REQUEST_EXPIRED
        })
    );
}

#[tokio::test]
async fn token_errors_stop_without_retrying() {
    let fake = FakeKairos::start(Options::default()).await;
    let clock = Clock::default();
    for (n, status, code) in [
        (10, None, "PLS-E3002"),
        (11, Some("expired"), "PLS-E3003"),
        (12, Some("exhausted"), "PLS-E3004"),
        (13, Some("disabled"), "PLS-E3002"),
    ] {
        let tok = token(n);
        if let Some(s) = status {
            fake.lock().add_token(&tok, TokenApproval::Auto, s);
        }
        let paths = temp_paths();
        let file = prepare(&paths, &new_enrollment(&fake, &tok)).unwrap();
        let mut log = Log::default();
        let (out, file) = run_with(&fake, &paths, file, &clock, &mut log, 10).await;
        assert_eq!(
            out,
            RunOutcome::Finished(Finish::Stopped { code }),
            "{status:?}"
        );
        assert_eq!(file.state, state::TOKEN_ERROR);
        assert_eq!(file.token, None);
        assert!(!paths.request_key().exists());
    }
    assert_eq!(fake.lock().hits("requests"), 4, "每個只送一次，不重試");
}

#[tokio::test]
async fn not_accepted_and_unsupported() {
    let fake = FakeKairos::start(Options::default()).await;
    fake.lock().add_token(AUTO, TokenApproval::Auto, "valid");
    fake.lock().accepts.new_enrollment = false;
    let clock = Clock::default();
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, AUTO)).unwrap();
    let mut log = Log::default();
    let (out, _) = run_with(&fake, &paths, file, &clock, &mut log, 10).await;
    assert_eq!(
        out,
        RunOutcome::Finished(Finish::Stopped {
            code: codes::E_ENROLL_NOT_ACCEPTED
        })
    );
    // 沒有 8444 的舊版 Kairos（404 且不是本規格的本文）
    fake.lock().push("server-info", Script::Plain(404));
    let mut c = client(&fake, &clock);
    assert_eq!(
        c.server_info().await.unwrap_err(),
        ApiError::Unsupported { status: 404 }
    );
    // 路徑不在白名單：本規格的 404 not_found
    fake.lock()
        .push("server-info", Script::Error(E::NotFound, None));
    assert_eq!(
        c.server_info().await.unwrap_err().server_code(),
        Some(E::NotFound)
    );
}

#[tokio::test]
async fn rate_limited_requests_wait_for_retry_after_and_keep_the_same_csr() {
    let fake = FakeKairos::start(Options::default()).await;
    fake.lock().add_token(AUTO, TokenApproval::Auto, "valid");
    fake.lock()
        .push("requests", Script::Error(E::RateLimited, Some(1)));
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, AUTO)).unwrap();
    let csr = file.csr.clone();
    let clock = Clock::default();
    let mut log = Log::default();
    let started = std::time::Instant::now();
    let (out, file) = run_with(&fake, &paths, file, &clock, &mut log, 20).await;
    assert_eq!(out, RunOutcome::Finished(Finish::Installed));
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "不早於 Retry-After"
    );
    assert_eq!(fake.lock().hits("requests"), 2);
    assert_eq!(fake.lock().token_consumed(AUTO), 1, "429 不消耗次數");
    assert!(log.0.iter().any(|(st, e)| st == state::SUBMITTING
        && matches!(
            e,
            Event::Retry {
                code: "PLS-E2202",
                rate_limited: true,
                ..
            }
        )));
    assert_eq!(file.observed_ip.as_deref(), Some("127.0.0.1"));
    let sent = ke::sha256(&ke::b64url_decode(csr.as_deref().unwrap()).unwrap());
    assert_eq!(fake.lock().requests[0].csr_sha, sent, "重送沿用同一個 CSR");
}

#[tokio::test]
async fn lost_response_is_resent_with_the_same_csr_and_poll_secret() {
    let fake = FakeKairos::start(Options::default()).await;
    let tok = token(20);
    fake.lock().add_token(&tok, TokenApproval::Manual, "valid");
    fake.lock().push("requests", Script::DropAfter);
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, &tok)).unwrap();
    let clock = Clock::default();
    let mut log = Log::default();
    let (out, file) = run_with(&fake, &paths, file, &clock, &mut log, 4).await;
    assert_eq!(out, RunOutcome::TimedOut);
    assert_eq!(file.state, state::PENDING_APPROVAL);
    let st = fake.lock();
    assert_eq!(st.hits("requests"), 2, "回應遺失後重送一次");
    assert_eq!(st.requests.len(), 1, "只有一筆申請（冪等）");
    assert_eq!(st.token_consumed(&tok), 1, "token 只扣一次");
    assert_eq!(file.request_id.as_deref(), Some(st.requests[0].id.as_str()));
}

#[tokio::test]
async fn clock_skew_waits_and_resends_after_the_clock_is_fixed() {
    let fake = FakeKairos::start(Options::default()).await;
    fake.lock().add_token(AUTO, TokenApproval::Auto, "valid");
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, AUTO)).unwrap();
    let clock = Clock::default();
    clock.set(7 * 60_000); // 本機快 7 分鐘
    let mut log = Log::default();
    let fixer = {
        let clock = clock.clone();
        let state = fake.state.clone();
        async move {
            while state.lock().unwrap().hits("requests") == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            clock.set(0); // NTP 校正
        }
    };
    let (_, (out, file)) = tokio::join!(fixer, run_with(&fake, &paths, file, &clock, &mut log, 30));
    assert_eq!(out, RunOutcome::Finished(Finish::Installed));
    assert_eq!(
        fake.lock().hits("requests"),
        2,
        "時間調整後立即重送（不等 5 分鐘）"
    );
    assert_eq!(
        fake.lock().token_consumed(AUTO),
        1,
        "clock_skew 不消耗 token"
    );
    let rejected = log
        .0
        .iter()
        .find(|(st, _)| st == state::CLOCK_REJECTED)
        .expect("曾進入 clock_rejected");
    assert!(matches!(rejected.1, Event::ClockRejected { .. }));
    assert_eq!(file.state, state::ISSUED);
}

#[tokio::test]
async fn clock_rejected_records_offset_and_server_time() {
    let fake = FakeKairos::start(Options::default()).await;
    fake.lock().add_token(AUTO, TokenApproval::Auto, "valid");
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, AUTO)).unwrap();
    let clock = Clock::default();
    clock.set(-462_120); // 本機慢 7 分 42 秒
    let mut log = Log::default();
    let (out, file) = run_with(&fake, &paths, file, &clock, &mut log, 1).await;
    assert_eq!(out, RunOutcome::TimedOut);
    assert_eq!(file.state, state::CLOCK_REJECTED);
    assert_eq!(file.last_code.as_deref(), Some("PLS-E8302"));
    let off = file.offset_ms.unwrap();
    assert!((-463_500..=-461_000).contains(&off), "{off}");
    assert!(
        file.server_time.is_some() && file.local_time.is_some() && file.next_submit_at.is_some()
    );
    assert!(file.token.is_some(), "申請尚未建立：token 保留給服務重送");
    assert!(paths.request_key().exists(), "保留私鑰與 CSR");
}

#[tokio::test]
async fn withdraw_pending_request_and_repeat() {
    let fake = FakeKairos::start(Options::default()).await;
    let tok = token(30);
    fake.lock().add_token(&tok, TokenApproval::Manual, "valid");
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, &tok)).unwrap();
    let clock = Clock::default();
    let mut log = Log::default();
    let (_, file) = run_with(&fake, &paths, file, &clock, &mut log, 1).await;
    let mut c = client(&fake, &clock);
    let w = flow::withdraw(
        &paths,
        &file,
        &mut c,
        WithdrawReason::EnrollCancelled,
        InitiatedBy::Cli,
    )
    .await;
    assert_eq!(
        (w.result, w.code, w.already_withdrawn),
        ("ok", None, false),
        "{w:?}"
    );
    assert_eq!(fake.lock().requests[0].state, RequestState::Withdrawn);
    // 重送：already_withdrawn
    let w = flow::withdraw(
        &paths,
        &file,
        &mut c,
        WithdrawReason::EnrollCancelled,
        InitiatedBy::Cli,
    )
    .await;
    assert_eq!((w.result, w.already_withdrawn), ("ok", true), "{w:?}");
}

#[tokio::test]
async fn withdraw_after_approval_or_after_rejection() {
    let fake = FakeKairos::start(Options::default()).await;
    let clock = Clock::default();
    let mut log = Log::default();
    // 撤回前剛被核准：PLS-W3310 already_approved
    let tok = token(31);
    fake.lock().add_token(&tok, TokenApproval::Manual, "valid");
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, &tok)).unwrap();
    let (_, file) = run_with(&fake, &paths, file, &clock, &mut log, 1).await;
    fake.approve(file.request_id.as_deref().unwrap());
    let mut c = client(&fake, &clock);
    let w = flow::withdraw(
        &paths,
        &file,
        &mut c,
        WithdrawReason::UninstallPurge,
        InitiatedBy::Msi,
    )
    .await;
    assert_eq!(
        (w.result, w.code, w.reason),
        ("unreported", Some("PLS-W3310"), Some("already_approved")),
        "{w:?}"
    );
    // 已被拒絕：不需撤回
    let tok = token(32);
    fake.lock().add_token(&tok, TokenApproval::Manual, "valid");
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, &tok)).unwrap();
    let (_, file) = run_with(&fake, &paths, file, &clock, &mut log, 1).await;
    fake.reject(file.request_id.as_deref().unwrap(), "x");
    let w = flow::withdraw(
        &paths,
        &file,
        &mut c,
        WithdrawReason::EnrollCancelled,
        InitiatedBy::Cli,
    )
    .await;
    assert_eq!(
        (w.result, w.request_state.as_deref()),
        ("not_needed", Some("rejected")),
        "{w:?}"
    );
    // 尚未送出（clock_rejected）：Kairos 上沒有申請
    let mut skipped = file.clone();
    skipped.state = state::CLOCK_REJECTED.into();
    let w = flow::withdraw(
        &paths,
        &skipped,
        &mut c,
        WithdrawReason::EnrollCancelled,
        InitiatedBy::Cli,
    )
    .await;
    assert_eq!(w.result, "skipped");
}

#[tokio::test]
async fn withdraw_retries_once_on_a_stale_nonce_and_reports_unreachable() {
    let fake = FakeKairos::start(Options::default()).await;
    let tok = token(33);
    fake.lock().add_token(&tok, TokenApproval::Manual, "valid");
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, &tok)).unwrap();
    let clock = Clock::default();
    let mut log = Log::default();
    let (_, file) = run_with(&fake, &paths, file, &clock, &mut log, 1).await;
    fake.lock()
        .push("withdraw", Script::Error(E::NonceExpired, None));
    let mut c = client(&fake, &clock);
    let w = flow::withdraw(
        &paths,
        &file,
        &mut c,
        WithdrawReason::EnrollCancelled,
        InitiatedBy::Cli,
    )
    .await;
    assert_eq!((w.result, w.attempts), ("ok", 2), "{w:?}");
    assert_eq!(
        fake.lock().hits("withdraw-challenge"),
        2,
        "從 challenge 重來"
    );
    // 註冊端點關閉（連線被拒）：重試 1 次後 PLS-W3310 unreachable
    let mut closed = file.clone();
    closed.enroll_server = "127.0.0.1:1".into();
    let mut c = EnrollClient::new(
        "127.0.0.1:1",
        TrustSpec::Pin(fake.ca_pin.clone()),
        false,
        clock.now(),
    )
    .unwrap();
    let w = flow::withdraw(
        &paths,
        &closed,
        &mut c,
        WithdrawReason::UninstallPurge,
        InitiatedBy::Msi,
    )
    .await;
    assert_eq!(
        (w.result, w.reason, w.cause_code, w.attempts),
        ("unreported", Some("unreachable"), Some("PLS-E2003"), 2),
        "{w:?}"
    );
}

// ---- 伺服器身分 ----

#[tokio::test]
async fn pin_mismatch_or_missing_ca_sends_nothing() {
    for opts in [
        Options::default(),
        Options {
            omit_ca_from_chain: true,
            ..Options::default()
        },
    ] {
        let fake = FakeKairos::start(opts).await;
        let pin = if opts.omit_ca_from_chain {
            fake.ca_pin.clone()
        } else {
            format!("sha256:{}", "ab".repeat(32))
        };
        let mut c = EnrollClient::new(
            &fake.endpoint,
            TrustSpec::Pin(pin),
            false,
            Clock::default().now(),
        )
        .unwrap();
        let err = c.server_info().await.unwrap_err();
        assert!(
            matches!(
                err,
                ApiError::Trust {
                    code: "PLS-E1406",
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(
            fake.lock().hits.is_empty(),
            "不送出任何請求：{:?}",
            fake.lock().hits
        );
    }
}

#[tokio::test]
async fn p384_and_rsa_roots_are_accepted() {
    for ca in [CaKind::P256, CaKind::P384, CaKind::Rsa] {
        let fake = FakeKairos::start(Options {
            ca,
            ..Options::default()
        })
        .await;
        let mut c = client(&fake, &Clock::default());
        let info = c
            .server_info()
            .await
            .unwrap_or_else(|e| panic!("{ca:?}: {e:?}"));
        assert_eq!(info.body.enroll_api, ["v1"]);
        let observed = c.observed();
        let anchor = x509::parse_cert(observed.anchor.as_ref().unwrap()).unwrap();
        assert_eq!(ke::ca_pin_from_spki(&anchor.spki), fake.ca_pin);
        // TOFU：同一條鏈的最上層 CA
        let mut t = EnrollClient::new(
            &fake.endpoint,
            TrustSpec::Tofu,
            false,
            Clock::default().now(),
        )
        .unwrap();
        t.server_info().await.unwrap();
        let anchor = x509::parse_cert(t.observed().anchor.as_ref().unwrap()).unwrap();
        assert_eq!(ke::ca_pin_from_spki(&anchor.spki), fake.ca_pin, "{ca:?}");
        // ca.pem（--ca-file）
        let certs = x509::pem_certs(&fake.ca_pem).unwrap();
        let mut f = EnrollClient::new(
            &fake.endpoint,
            TrustSpec::Anchors(certs),
            false,
            Clock::default().now(),
        )
        .unwrap();
        f.server_info().await.unwrap();
    }
}

#[tokio::test]
async fn replaced_ca_in_issued_response_is_not_installed() {
    let fake = FakeKairos::start(Options::default()).await;
    fake.lock().add_token(AUTO, TokenApproval::Auto, "valid");
    fake.lock().wrong_ca_pem = true;
    let paths = temp_paths();
    let file = prepare(&paths, &new_enrollment(&fake, AUTO)).unwrap();
    let clock = Clock::default();
    let mut log = Log::default();
    let (out, file) = run_with(&fake, &paths, file, &clock, &mut log, 10).await;
    assert_eq!(
        out,
        RunOutcome::Finished(Finish::Stopped { code: "PLS-E1406" })
    );
    assert_eq!(file.state, state::FAILED);
    assert!(
        !paths.client_cert().exists() && !paths.ca().exists(),
        "不寫入取得的憑證"
    );
}

#[tokio::test]
async fn expired_server_certificate_allows_only_an_unverified_server_time() {
    let fake = FakeKairos::start(Options {
        server_cert_expired: true,
        ..Options::default()
    })
    .await;
    let mut c = client(&fake, &Clock::default());
    let err = c.server_info().await.unwrap_err();
    assert!(
        matches!(
            err,
            ApiError::Trust {
                code: "PLS-E1403",
                ..
            }
        ),
        "{err:?}"
    );
    // 忽略有效期間：只取 server_time
    let mut c = EnrollClient::new(
        &fake.endpoint,
        TrustSpec::Pin(fake.ca_pin.clone()),
        true,
        Clock::default().now(),
    )
    .unwrap();
    let info = c.server_info().await.unwrap();
    assert!(ke::parse_rfc3339_ms(&info.server_time).is_ok());
    // 其他檢查照常：錯的指紋仍然失敗
    let mut c = EnrollClient::new(
        &fake.endpoint,
        TrustSpec::Pin(format!("sha256:{}", "cd".repeat(32))),
        true,
        Clock::default().now(),
    )
    .unwrap();
    assert!(matches!(
        c.server_info().await.unwrap_err(),
        ApiError::Trust {
            code: "PLS-E1406",
            ..
        }
    ));
}

// ---- 過期續簽 ----

/// 準備一台「已註冊、憑證已過期」的主機：回傳舊憑證的序號
fn expired_identity(fake: &FakeKairos, paths: &IdentityPaths, expired_days_ago: i64) -> String {
    paths.ensure_dir().unwrap();
    let pkcs8 = keys::generate_p256_pkcs8().unwrap();
    let spki = ke::p256_spki_of_pkcs8(&pkcs8).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let cert = fake.issue_for(
        AGENT,
        &spki,
        now - 400 * 86_400_000,
        now - expired_days_ago * 86_400_000,
    );
    store::write_atomic(
        &paths.client_key(),
        keys::pkcs8_pem(&pkcs8).unwrap().as_bytes(),
        FileKind::Secret,
    )
    .unwrap();
    store::write_atomic(
        &paths.client_cert(),
        cert.client_cert_pem.as_bytes(),
        FileKind::Certificate,
    )
    .unwrap();
    store::write_atomic(&paths.ca(), fake.ca_pem.as_bytes(), FileKind::Certificate).unwrap();
    fake.lock()
        .host_fingerprints
        .insert(AGENT.into(), FP.into());
    cert.serial
}

fn grace_file(fake: &FakeKairos) -> EnrollmentFile {
    let mut f = EnrollmentFile {
        agent_id: AGENT.into(),
        hostname: Some("HMI-LINE5-03".into()),
        host_fingerprint: Some(FP.into()),
        ..Default::default()
    };
    flow::start_grace(&mut f, &fake.endpoint, vec![fake.ca_pin.clone()]);
    f
}

fn ca_client(fake: &FakeKairos, clock: &Clock) -> EnrollClient {
    EnrollClient::new(
        &fake.endpoint,
        TrustSpec::Anchors(x509::pem_certs(&fake.ca_pem).unwrap()),
        false,
        clock.now(),
    )
    .unwrap()
}

async fn run_grace(
    fake: &FakeKairos,
    paths: &IdentityPaths,
    file: EnrollmentFile,
    clock: &Clock,
    log: &mut Log,
    secs: u64,
) -> (RunOutcome, EnrollmentFile) {
    let mut d =
        Driver::new(paths.clone(), file, ca_client(fake, clock), log).with_rand(Box::new(|| 0.5));
    d.timing = fast();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let out = d.run(Some(deadline), &CancellationToken::new()).await;
    (out, d.file.clone())
}

#[tokio::test]
async fn grace_renewal_issues_a_new_certificate() {
    let fake = FakeKairos::start(Options::default()).await;
    let paths = temp_paths();
    let old_serial = expired_identity(&fake, &paths, 12);
    let clock = Clock::default();
    let mut log = Log::default();
    let (out, file) = run_grace(&fake, &paths, grace_file(&fake), &clock, &mut log, 10).await;
    assert_eq!(out, RunOutcome::Finished(Finish::Installed));
    let leaf = assert_installed(&paths, &fake);
    assert_ne!(leaf.serial_hex(), old_serial);
    assert!(!paths.grace_key().exists());
    assert_eq!(file.state, state::ISSUED);
    assert_eq!(file.grace_renew_state, None);
    assert!(file.grace_last_sent_at.is_some());
}

#[tokio::test]
async fn grace_clock_rejected_then_fixed() {
    let fake = FakeKairos::start(Options::default()).await;
    let paths = temp_paths();
    expired_identity(&fake, &paths, 12);
    let clock = Clock::default();
    clock.set(-(2 * 3600 + 13 * 60) * 1000); // 慢 2 小時 13 分
    let mut log = Log::default();
    let (out, file) = run_grace(&fake, &paths, grace_file(&fake), &clock, &mut log, 1).await;
    assert_eq!(out, RunOutcome::TimedOut);
    assert_eq!(file.state, grace_state::CLOCK_REJECTED);
    assert_eq!(
        file.grace_renew_state.as_deref(),
        Some(grace_state::CLOCK_REJECTED)
    );
    assert_eq!(file.last_code.as_deref(), Some("PLS-E8302"));
    assert!(file.offset_ms.unwrap() < -7_000_000);
    assert!(paths.grace_key().exists(), "保留續簽用的新私鑰");
    let csr = file.csr.clone();
    let first_sent = file.grace_last_sent_at.clone();
    clock.set(0);
    let (out, file) = run_grace(&fake, &paths, file, &clock, &mut log, 10).await;
    assert_eq!(out, RunOutcome::Finished(Finish::Installed));
    // 同一個 CSR、新的 sent_at
    let req = fake.lock().requests.last().cloned().unwrap();
    assert_eq!(
        ke::sha256(&ke::b64url_decode(csr.as_deref().unwrap()).unwrap()),
        req.csr_sha
    );
    assert_ne!(file.grace_last_sent_at, first_sent);
}

#[tokio::test]
async fn replayed_request_is_resigned_once() {
    let fake = FakeKairos::start(Options::default()).await;
    let paths = temp_paths();
    expired_identity(&fake, &paths, 12);
    fake.lock()
        .push("renewals", Script::Error(E::ReplayedRequest, None));
    let clock = Clock::default();
    let mut log = Log::default();
    let (out, _) = run_grace(&fake, &paths, grace_file(&fake), &clock, &mut log, 10).await;
    assert_eq!(out, RunOutcome::Finished(Finish::Installed));
    assert_eq!(fake.lock().hits("renewals"), 2, "重新簽章後立即重送一次");
    // 第二次仍是 replayed_request：PLS-W2107，依一般退避（不再立即重送）
    let paths = temp_paths();
    expired_identity(&fake, &paths, 12);
    fake.lock()
        .push("renewals", Script::Error(E::ReplayedRequest, None));
    fake.lock()
        .push("renewals", Script::Error(E::ReplayedRequest, None));
    let before = fake.lock().hits("renewals");
    let (out, file) = run_grace(&fake, &paths, grace_file(&fake), &clock, &mut log, 2).await;
    assert_eq!(out, RunOutcome::TimedOut);
    assert_eq!(fake.lock().hits("renewals") - before, 2);
    assert_eq!(file.last_code.as_deref(), Some("PLS-W2107"));
    assert_eq!(file.state, grace_state::RENEWING);
    assert_eq!(file.protocol_code.as_deref(), Some("replayed_request"));
}

#[tokio::test]
async fn real_replay_is_rejected_by_content_not_by_signature_bytes() {
    // 擷取一次請求原樣重送：Kairos 以簽章內容判定重放
    let fake = FakeKairos::start(Options::default()).await;
    let paths = temp_paths();
    expired_identity(&fake, &paths, 12);
    let mut file = grace_file(&fake);
    let now = chrono::Utc::now().timestamp_millis();
    let req = flow::build_grace_request(&paths, &mut file, now, &fake.endpoint).unwrap();
    let mut c = ca_client(&fake, &Clock::default());
    c.create_renewal(&req).await.unwrap();
    let err = c.create_renewal(&req).await.unwrap_err();
    assert_eq!(err.server_code(), Some(E::ReplayedRequest));
    // 同一個 CSR、新的 sent_at：冪等，回傳同一張憑證
    let again = flow::build_grace_request(&paths, &mut file, now, &fake.endpoint).unwrap();
    assert_ne!(again.sent_at, req.sent_at, "sent_at 不重複");
    let r = c.create_renewal(&again).await.unwrap();
    let first = fake.lock().requests[0].cert.clone().unwrap();
    assert_eq!(
        r.body.certificate.unwrap().serial,
        first.serial,
        "同一個 CSR 只得到一張憑證"
    );
}

#[tokio::test]
async fn grace_outcomes_exceeded_mismatch_revoked_not_expired() {
    let fake = FakeKairos::start(Options::default()).await;
    let clock = Clock::default();
    let mut log = Log::default();
    // 超過寬限期（90 天）
    let paths = temp_paths();
    expired_identity(&fake, &paths, 120);
    let (out, file) = run_grace(&fake, &paths, grace_file(&fake), &clock, &mut log, 1).await;
    assert_eq!(out, RunOutcome::TimedOut, "每 6 小時再試");
    assert_eq!(file.state, grace_state::EXCEEDED);
    assert_eq!(file.last_code.as_deref(), Some("PLS-E1113"));
    // 主機指紋不符
    let paths = temp_paths();
    expired_identity(&fake, &paths, 12);
    fake.lock()
        .host_fingerprints
        .insert(AGENT.into(), format!("sha256:{}", "0".repeat(64)));
    let (out, file) = run_grace(&fake, &paths, grace_file(&fake), &clock, &mut log, 5).await;
    assert_eq!(
        out,
        RunOutcome::Finished(Finish::Stopped { code: "PLS-E1114" })
    );
    assert_eq!(file.state, grace_state::REJECTED);
    // 已撤銷
    let paths = temp_paths();
    expired_identity(&fake, &paths, 12);
    fake.lock()
        .issued
        .get_mut(AGENT)
        .unwrap()
        .last_mut()
        .unwrap()
        .revoked = true;
    let (out, file) = run_grace(&fake, &paths, grace_file(&fake), &clock, &mut log, 5).await;
    assert_eq!(
        out,
        RunOutcome::Finished(Finish::Stopped { code: "PLS-E1105" })
    );
    assert_eq!(file.state, grace_state::REVOKED);
    // 依 Kairos 的時間仍有效（本機時鐘偏快）：回到 8443
    let paths = temp_paths();
    expired_identity(&fake, &paths, -3);
    let (out, file) = run_grace(&fake, &paths, grace_file(&fake), &clock, &mut log, 5).await;
    assert_eq!(out, RunOutcome::Finished(Finish::NotExpired));
    assert_eq!(file.state, state::REGISTERED);
}

#[tokio::test]
async fn grace_pending_approval_then_polling() {
    let fake = FakeKairos::start(Options::default()).await;
    fake.lock().grace_requires_approval = true;
    let paths = temp_paths();
    expired_identity(&fake, &paths, 12);
    let clock = Clock::default();
    let mut log = Log::default();
    let (out, file) = run_grace(&fake, &paths, grace_file(&fake), &clock, &mut log, 1).await;
    assert_eq!(out, RunOutcome::TimedOut);
    assert_eq!(file.state, grace_state::PENDING);
    fake.approve(file.request_id.as_deref().unwrap());
    let (out, _) = run_grace(&fake, &paths, file, &clock, &mut log, 10).await;
    assert_eq!(out, RunOutcome::Finished(Finish::Installed));
    assert_installed(&paths, &fake);
}

#[tokio::test]
async fn grace_blocked_when_the_enroll_endpoint_is_closed() {
    let fake = FakeKairos::start(Options::default()).await;
    let paths = temp_paths();
    expired_identity(&fake, &paths, 12);
    let clock = Clock::default();
    let mut log = Log::default();
    let mut file = grace_file(&fake);
    file.enroll_server = "127.0.0.1:1".into();
    let c = EnrollClient::new(
        "127.0.0.1:1",
        TrustSpec::Anchors(x509::pem_certs(&fake.ca_pem).unwrap()),
        false,
        clock.now(),
    )
    .unwrap();
    let mut d = Driver::new(paths.clone(), file, c, &mut log).with_rand(Box::new(|| 0.5));
    d.timing = fast();
    let out = d
        .run(
            Some(tokio::time::Instant::now() + Duration::from_secs(1)),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(out, RunOutcome::TimedOut);
    assert_eq!(d.file.state, grace_state::BLOCKED);
    assert_eq!(d.file.last_code.as_deref(), Some("PLS-E2007"));
    assert_eq!(
        d.file.grace_backoff_ms,
        Some(240_000),
        "2 分鐘後重試，之後倍增"
    );
}

// ---- 共用測試向量 ----

#[test]
fn grace_and_withdraw_signing_inputs_match_the_shared_vectors() {
    use crate::enroll::vectors::Vectors;
    let v = Vectors::load();
    let csr = ke::b64url_decode(v.get("csr")).unwrap();
    let input = ke::grace_renew_signing_input(
        v.get("agent_id"),
        v.get("host_fingerprint"),
        &csr,
        v.get("sent_at"),
        v.get("enroll_endpoint"),
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(input.clone()).unwrap(),
        v.block("過期續簽的簽章內容")
    );
    assert_eq!(
        ke::hex_lower(&ke::replay_cache_key(&input)),
        v.get("replay_key")
    );
    // Pulse 以舊私鑰簽出的簽章可被舊公鑰驗證；向量中的簽章也可被驗證
    let old = ke::b64url_decode(v.get("old_key_pkcs8")).unwrap();
    let sig = ke::sign_ecdsa_p256_sha256(&old, &input).unwrap();
    let old_spki = ke::b64url_decode(v.get("old_spki")).unwrap();
    assert!(ke::verify_ecdsa_p256_sha256(&old_spki, &input, &sig).is_ok());
    assert!(ke::verify_ecdsa_p256_sha256(&old_spki, &input, v.get("grace_signature")).is_ok());
    let w = ke::withdraw_signing_input(
        v.get("request_id"),
        v.get("agent_id"),
        v.get("nonce"),
        v.get("enroll_endpoint"),
    )
    .unwrap();
    assert_eq!(String::from_utf8(w).unwrap(), v.block("撤回的簽章內容"));
    // 安裝字串
    let s = ke::InstallString::decode(v.get("install_string")).unwrap();
    assert_eq!(s.t, v.get("token"));
    assert_eq!(s.p, v.get("ca_pin"));
}

#[test]
fn build_grace_request_signs_with_the_old_key_and_never_reuses_sent_at() {
    let paths = temp_paths();
    paths.ensure_dir().unwrap();
    let old = keys::generate_p256_pkcs8().unwrap();
    store::write_atomic(
        &paths.client_key(),
        keys::pkcs8_pem(&old).unwrap().as_bytes(),
        FileKind::Secret,
    )
    .unwrap();
    // 自簽的「舊憑證」即可（只取 DER）
    let kp = rcgen::KeyPair::try_from(old.as_slice()).unwrap();
    let cert = rcgen::CertificateParams::new(vec![AGENT.to_string()])
        .unwrap()
        .self_signed(&kp)
        .unwrap();
    store::write_atomic(
        &paths.client_cert(),
        cert.pem().as_bytes(),
        FileKind::Certificate,
    )
    .unwrap();
    let mut file = EnrollmentFile {
        agent_id: AGENT.into(),
        host_fingerprint: Some(FP.into()),
        hostname: Some("h".into()),
        ..Default::default()
    };
    let now = 1_791_252_785_000;
    let a =
        flow::build_grace_request(&paths, &mut file, now, "Kairos.Example.gov.tw:8444").unwrap();
    let b =
        flow::build_grace_request(&paths, &mut file, now, "Kairos.Example.gov.tw:8444").unwrap();
    assert_eq!(a.csr, b.csr, "同一個 CSR");
    assert_eq!(a.poll_secret, b.poll_secret);
    assert_eq!(
        ke::parse_rfc3339_ms(&b.sent_at).unwrap(),
        now + 1,
        "本機時間沒有前進時 +1 毫秒"
    );
    let input = ke::grace_renew_signing_input(
        AGENT,
        FP,
        &ke::b64url_decode(&b.csr).unwrap(),
        &b.sent_at,
        "kairos.example.gov.tw:8444",
    )
    .unwrap();
    assert!(
        ke::verify_ecdsa_p256_sha256(&ke::p256_spki_of_pkcs8(&old).unwrap(), &input, &b.signature)
            .is_ok()
    );
    assert!(ke::validate_grace_renew_request(&b).is_ok());
    // 新私鑰與舊私鑰不同
    let new_spki = x509::csr_spki(&ke::b64url_decode(&b.csr).unwrap()).unwrap();
    assert_ne!(new_spki, ke::p256_spki_of_pkcs8(&old).unwrap());
    // RSA 的舊私鑰不適用過期續簽
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/client-rsa.key"),
        paths.client_key(),
    )
    .unwrap();
    let err = flow::build_grace_request(&paths, &mut file, now, "kairos.example.gov.tw:8444")
        .unwrap_err();
    assert_eq!(
        (err.0, err.1),
        (grace_state::REJECTED, codes::E_GRACE_IDENTITY)
    );
}

#[test]
fn install_is_idempotent_after_an_interruption() {
    // 斷電模擬：安裝到一半（私鑰已改名、client.pem 還沒寫）後重來，結果相同
    let paths = temp_paths();
    paths.ensure_dir().unwrap();
    let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let ca_spki = ke::p256_spki_of_pkcs8(&ca_key.serialize_der()).unwrap();
    let pkcs8 = keys::generate_p256_pkcs8().unwrap();
    store::write_atomic(
        &paths.request_key(),
        keys::pkcs8_pem(&pkcs8).unwrap().as_bytes(),
        FileKind::Secret,
    )
    .unwrap();
    let kp = rcgen::KeyPair::try_from(pkcs8.as_slice()).unwrap();
    let issuer = rcgen::Issuer::new(ca, &ca_key);
    let leaf = rcgen::CertificateParams::new(vec![AGENT.to_string()])
        .unwrap()
        .signed_by(&kp, &issuer)
        .unwrap();
    let mut file = EnrollmentFile {
        agent_id: AGENT.into(),
        state: state::ISSUED.into(),
        trusted_ca_pins: vec![ke::ca_pin_from_spki(&ca_spki)],
        certificate: Some(ke::IssuedCertificate {
            serial: "01".into(),
            not_before: String::new(),
            not_after: String::new(),
            client_cert_pem: leaf.pem(),
            chain_pem: String::new(),
            ca_pem: ca_cert.pem(),
        }),
        ..Default::default()
    };
    // 中斷：私鑰已改名
    std::fs::rename(paths.request_key(), paths.client_key()).unwrap();
    let mut again = file.clone();
    flow::install(&paths, &mut again).unwrap();
    assert!(paths.client_cert().exists());
    assert_eq!(again.state, state::ISSUED);
    assert!(again.certificate.is_none());
    // 私鑰遺失：不成對（PLS-E1107）
    std::fs::remove_file(paths.client_key()).unwrap();
    let err = flow::install(&paths, &mut file).unwrap_err();
    assert_eq!(err.0, "PLS-E1107");
}
