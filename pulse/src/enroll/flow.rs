//! 註冊、輪詢、取回憑證、過期續簽與撤回的流程（ENROLL.md 第 6～8 節、設計包 `enrollment-ux.md` §5）。
//!
//! [`Driver`] 每一步（送出、輪詢、安裝、過期續簽）都先把狀態寫入 `enrollment.json` 再等待，所以：
//!
//! - `pulse enroll` 被 Ctrl+C 或斷電中斷後，重新執行或服務啟動時從 `enrollment.json` 接續
//!   （不重送新申請、不產生第二組私鑰；設計包 §7「中斷保護」）。
//! - 回應遺失後的重送一律沿用**同一把私鑰、同一個 CSR、同一個 `poll_secret`**，只更新 `sent_at`
//!   （ENROLL.md 第 6.3 節第 7 步的冪等）。
//! - 已取得、尚未完成安裝的憑證先存進 `enrollment.json`（`state = issued`），安裝中斷後可重來。

use std::time::Duration;

use kairos_proto::enroll::{
    self as ke, CertPolicy, EnrollErrorCode, EnrollRequest, EnrollResult, GraceRenewRequest,
    InitiatedBy, RequestState, SignatureAlg, WithdrawReason, WithdrawRequest,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::client::{ApiError, EnrollClient};
use super::keys;
use super::machine::{self, Decision, Phase};
use super::schedule::{self, Exp, Poller};
use super::store::{self, EnrollmentFile, FileKind, IdentityPaths, grace_state, state};
use super::x509;
use crate::codes;

/// 可調整的時間（測試縮短；正式版一律使用預設值）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// `clock_skew` 的重送間隔（5 分鐘）
    pub clock_retry: Duration,
    /// 長輪詢秒數（Kairos 宣告 `long_poll` 時使用；上限 60）
    pub long_poll_secs: u32,
    /// 偵測本機時間調整的檢查間隔
    pub clock_check: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            clock_retry: schedule::CLOCK_RETRY,
            long_poll_secs: ke::MAX_LONG_POLL_SECS,
            clock_check: Duration::from_secs(1),
        }
    }
}

/// 流程中的事件（呼叫端據此輸出進度、記日誌、更新狀態檔）
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Kairos 受理了申請（第一次得到 `pending_approval` 或 `issued`）
    Submitted,
    /// 等待核准（`first`：第一次進入）
    Pending { first: bool, wait: Duration },
    /// 排隊重送（429／503）或網路錯誤後重試
    Retry {
        code: &'static str,
        wait: Duration,
        rate_limited: bool,
    },
    /// Kairos 因時鐘偏差暫不受理
    ClockRejected { wait: Duration },
    /// 已核准（取得憑證）
    Approved,
    /// 已安裝憑證
    Installed,
    /// 結束（不再自動重試）
    Stopped { code: &'static str },
    /// 過期續簽：舊憑證依 Kairos 的時間仍有效（`certificate_not_expired`）
    NotExpired,
}

/// 事件的接收者
pub trait Observer: Send {
    fn on(&mut self, file: &EnrollmentFile, event: &Event);
}

/// 一步之後
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    Wait(Duration),
    Done(Finish),
}

/// 流程結束的原因
#[derive(Debug, Clone, PartialEq)]
pub enum Finish {
    /// 憑證已安裝（`state = issued`，尚未確認 8443）
    Installed,
    /// 不再自動重試（`state` 與 `last_code` 寫在 `enrollment.json`）
    Stopped { code: &'static str },
    /// 過期續簽時 Kairos 回應舊憑證仍有效：回到 8443
    NotExpired,
}

/// [`Driver::run`] 的結果
#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    Finished(Finish),
    /// `deadline` 到了仍在等待（申請、排隊或時鐘偏差）
    TimedOut,
    Cancelled,
}

/// 新註冊與過期續簽請求中的主機資訊
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestInfo {
    pub hostname: String,
    pub pulse_version: String,
    pub display_name: Option<String>,
    pub tags: Option<Vec<String>>,
    pub host_fingerprint: Option<String>,
    pub os: Option<kairos_proto::OsInfo>,
}

impl RequestInfo {
    /// 由主機名稱與 `HostInfo` 建立（主機名稱依協定的字元限制清理）
    pub fn new(hostname: &str, host: &kairos_proto::HostInfo) -> Self {
        Self {
            hostname: kairos_proto::sanitize_display_text(
                hostname,
                kairos_proto::MAX_HOSTNAME_BYTES,
            )
            .unwrap_or_else(|| "unknown".to_string()),
            pulse_version: env!("CARGO_PKG_VERSION").to_string(),
            display_name: host.display_name.clone(),
            tags: host.tags.clone().filter(|t| !t.is_empty()),
            host_fingerprint: host.host_fingerprint.clone(),
            os: host.os.clone(),
        }
    }
}

/// 申請流程
pub struct Driver<'a> {
    pub paths: IdentityPaths,
    pub file: EnrollmentFile,
    pub client: EnrollClient,
    pub timing: Timing,
    /// Kairos 宣告了 `long_poll`
    pub long_poll: bool,
    /// 遇到 `source_blocked` 時直接結束（互動式 CLI）；服務則等封鎖結束後重試
    pub stop_on_source_block: bool,
    net: Exp,
    poller: Option<Poller>,
    grace_backoff: Exp,
    rand: Box<dyn FnMut() -> f64 + Send>,
    observer: &'a mut dyn Observer,
    /// `run` 的期限（長輪詢不超過它）
    deadline: Option<Instant>,
    /// 過期續簽因 8444 無法連線而受阻（`grace_blocked`）時通知：服務據此對 8443 出示過期憑證「報到」
    pub on_blocked: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

/// 非密碼學用途的亂數（抖動）
pub fn jitter_rand() -> Box<dyn FnMut() -> f64 + Send> {
    Box::new(|| {
        let d = crate::backoff::random_spread(Duration::from_secs(1_000_000));
        d.as_secs_f64() / 1_000_000.0
    })
}

fn now_ts(ms: i64) -> String {
    ke::format_rfc3339_ms(ms)
}

impl<'a> Driver<'a> {
    pub fn new(
        paths: IdentityPaths,
        file: EnrollmentFile,
        client: EnrollClient,
        observer: &'a mut dyn Observer,
    ) -> Self {
        let poller = (file.poll_floor_ms.is_some() || file.poll_interval_ms.is_some())
            .then(|| Poller::resume(file.poll_interval_ms, file.poll_floor_ms));
        let grace_backoff = Exp::resume(
            schedule::GRACE_BACKOFF_INITIAL,
            schedule::GRACE_BACKOFF_MAX,
            file.grace_backoff_ms,
        );
        Self {
            paths,
            file,
            client,
            timing: Timing::default(),
            long_poll: false,
            stop_on_source_block: false,
            net: Exp::new(schedule::NET_BACKOFF_INITIAL, schedule::NET_BACKOFF_MAX),
            poller,
            grace_backoff,
            rand: jitter_rand(),
            observer,
            deadline: None,
            on_blocked: None,
        }
    }

    /// 測試用：固定的亂數
    #[cfg(test)]
    pub fn with_rand(mut self, rand: Box<dyn FnMut() -> f64 + Send>) -> Self {
        self.rand = rand;
        self
    }

    fn save(&mut self) {
        if let Err(e) = self.paths.save_enrollment(&self.file) {
            log::error!(
                "無法寫入 {}：{e}（中斷後可能無法接續這次申請）",
                self.paths.enrollment().display()
            );
        }
    }

    fn emit(&mut self, event: Event) {
        self.observer.on(&self.file, &event);
    }

    fn now_ms(&self) -> i64 {
        self.client.now_ms()
    }

    fn at(&self, wait: Duration) -> String {
        now_ts(self.now_ms() + i64::try_from(wait.as_millis()).unwrap_or(i64::MAX / 2))
    }

    /// 依 `enrollment.json` 的下次時間，啟動後先等多久（服務重新啟動後不立即輪詢）
    pub fn initial_wait(&self) -> Duration {
        let next = match self.file.state.as_str() {
            state::PENDING_APPROVAL | grace_state::PENDING => self.file.next_poll_at.as_deref(),
            state::SUBMITTING | state::CLOCK_REJECTED => self.file.next_submit_at.as_deref(),
            s if s.starts_with("grace_") => self.file.grace_next_attempt_at.as_deref(),
            _ => None,
        };
        next.and_then(|t| ke::parse_rfc3339_ms(t).ok())
            .map(|t| t - self.now_ms())
            .filter(|ms| *ms > 0)
            .map(|ms| Duration::from_millis(ms.unsigned_abs()))
            .unwrap_or_default()
    }

    /// 執行到結束、`deadline` 到或被取消。等待中偵測到本機時間被調整時，`clock_rejected` 立即重送
    pub async fn run(
        &mut self,
        deadline: Option<Instant>,
        cancel: &CancellationToken,
    ) -> RunOutcome {
        self.deadline = deadline;
        let mut wait = self.initial_wait();
        loop {
            if !wait.is_zero() {
                // 等到期限為止（時鐘偏差暫不受理時，期間內時間被校正仍會立即重送）
                let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
                let sleep_for = left.map_or(wait, |l| l.min(wait));
                let watch_clock = matches!(
                    self.file.state.as_str(),
                    state::CLOCK_REJECTED | grace_state::CLOCK_REJECTED
                );
                match sleep_watching_clock(
                    sleep_for,
                    watch_clock,
                    &self.client,
                    self.timing.clock_check,
                    cancel,
                )
                .await
                {
                    SleepEnd::Cancelled => return RunOutcome::Cancelled,
                    SleepEnd::Elapsed if sleep_for < wait => return RunOutcome::TimedOut,
                    SleepEnd::Elapsed | SleepEnd::ClockAdjusted => {}
                }
            } else if deadline.is_some_and(|d| Instant::now() >= d) {
                return RunOutcome::TimedOut;
            }
            if cancel.is_cancelled() {
                return RunOutcome::Cancelled;
            }
            match self.step().await {
                Next::Done(f) => return RunOutcome::Finished(f),
                Next::Wait(w) => wait = w,
            }
        }
    }

    /// 依目前的狀態做一步
    pub async fn step(&mut self) -> Next {
        match self.file.state.clone().as_str() {
            state::SUBMITTING | state::CLOCK_REJECTED => self.submit_once().await,
            state::PENDING_APPROVAL | grace_state::PENDING => self.poll_once().await,
            state::ISSUED if self.file.certificate.is_some() => self.install_step(),
            state::ISSUED | state::REGISTERED => Next::Done(Finish::Installed),
            grace_state::RENEWING
            | grace_state::BLOCKED
            | grace_state::EXCEEDED
            | grace_state::CLOCK_REJECTED => self.grace_once().await,
            _ => Next::Done(Finish::Stopped {
                code: static_code(self.file.last_code.as_deref()),
            }),
        }
    }

    // ---- 新註冊 ----

    async fn submit_once(&mut self) -> Next {
        let Some(token) = self.file.token.clone() else {
            return self.stop(
                state::FAILED,
                codes::E_NOT_ENROLLED,
                "申請尚未送出，但本機沒有 token（請重新執行 pulse enroll --token …）".into(),
            );
        };
        let Some(csr) = self.file.csr.clone() else {
            return self.stop(
                state::FAILED,
                codes::E_NOT_ENROLLED,
                "enrollment.json 缺少 CSR".into(),
            );
        };
        let Some(poll_secret) = self.file.poll_secret.clone() else {
            return self.stop(
                state::FAILED,
                codes::E_NOT_ENROLLED,
                "enrollment.json 缺少 poll_secret".into(),
            );
        };
        let sent_at = now_ts(self.now_ms());
        let req = EnrollRequest {
            token,
            agent_id: self.file.agent_id.clone(),
            csr,
            poll_secret,
            sent_at: sent_at.clone(),
            hostname: self.file.hostname.clone().unwrap_or_default(),
            pulse_version: env!("CARGO_PKG_VERSION").to_string(),
            server_verification: self
                .file
                .server_verification
                .unwrap_or(ke::ServerVerification::CaPin),
            display_name: self.file.display_name.clone(),
            tags: self.file.tags.clone(),
            host_fingerprint: self.file.host_fingerprint.clone(),
            os: self.file.os.clone(),
            previous_serial: self.file.previous_serial.clone(),
        };
        if let Err(e) = ke::validate_enroll_request(&req) {
            return self.stop(
                state::FAILED,
                codes::E_REQUEST_INVALID,
                format!("申請內容不合法：{e}"),
            );
        }
        self.file.attempts = self.file.attempts.saturating_add(1);
        match self.client.create_request(&req).await {
            Ok(reply) => self.on_result(reply.body),
            Err(e) => self.on_error(e, Phase::Submit, &sent_at),
        }
    }

    async fn poll_once(&mut self) -> Next {
        let (Some(id), Some(secret)) =
            (self.file.request_id.clone(), self.file.poll_secret.clone())
        else {
            return self.stop(
                state::FAILED,
                codes::E_NOT_ENROLLED,
                "enrollment.json 缺少 request_id 或 poll_secret".into(),
            );
        };
        let mut wait = if self.long_poll || self.file.long_poll {
            self.timing.long_poll_secs
        } else {
            0
        };
        if let Some(d) = self.deadline {
            let left = d.saturating_duration_since(Instant::now()).as_secs();
            wait = wait.min(u32::try_from(left).unwrap_or(u32::MAX));
        }
        match self.client.poll(&id, &secret, wait).await {
            Ok(reply) => self.on_result(reply.body),
            Err(e) => {
                let now = now_ts(self.now_ms());
                self.on_error(e, Phase::Poll, &now)
            }
        }
    }

    /// 申請的結果（新註冊、過期續簽、輪詢共用）
    fn on_result(&mut self, r: EnrollResult) -> Next {
        let first = self.file.request_id.is_none();
        let (next_state, code) = machine::result_state(&r);
        self.file.request_id = Some(r.request_id.clone());
        self.file.request_code = Some(r.request_code.to_string());
        if self.file.submitted_at.is_none() {
            self.file.submitted_at = Some(now_ts(self.now_ms()));
        }
        if r.expires_at.is_some() {
            self.file.expires_at = r.expires_at.clone();
        }
        self.file.approval_detail = r.approval_detail;
        self.file.decided_by = r.decided_by.clone().or(self.file.decided_by.take());
        self.file.decided_at = r.decided_at.clone().or(self.file.decided_at.take());
        self.file.reject_reason = r
            .reject_reason
            .as_deref()
            .map(|s| crate::network::sanitize_for_log(s, 200));
        if let Some(ip) = &r.observed_ip {
            self.file.observed_ip = Some(ip.clone());
        }
        // 申請已建立：token 不再需要，立即從磁碟刪除
        self.file.token = None;
        self.file.last_code = code.map(str::to_string);
        self.file.last_error = None;
        self.file.protocol_code = None;
        self.file.offset_ms = None;
        self.file.next_submit_at = None;
        self.file.state = next_state.to_string();
        if self.file.is_grace() {
            self.file.grace_renew_state = Some(next_state.to_string());
        }
        if first {
            self.save();
            self.emit(Event::Submitted);
        }
        match r.state {
            RequestState::PendingApproval => {
                let poll_after = r.poll_after_ms.map(Duration::from_millis);
                let wait = match &mut self.poller {
                    Some(p) => p.next(poll_after, (self.rand)()),
                    None => {
                        let mut p = Poller::new(
                            poll_after.unwrap_or(Duration::from_millis(ke::POLL_AFTER_BASE_MS)),
                        );
                        let w = p.next(poll_after, (self.rand)());
                        self.poller = Some(p);
                        w
                    }
                };
                if let Some(p) = &self.poller {
                    self.file.poll_interval_ms = u64::try_from(p.interval.as_millis()).ok();
                    self.file.poll_floor_ms = u64::try_from(p.floor.as_millis()).ok();
                }
                self.file.next_poll_at = Some(self.at(wait));
                self.save();
                self.emit(Event::Pending { first, wait });
                Next::Wait(wait)
            }
            RequestState::Issued => {
                let Some(cert) = r.certificate else {
                    return self.stop(
                        state::FAILED,
                        codes::W_SERVER_UNEXPECTED,
                        "issued 的回應沒有憑證".into(),
                    );
                };
                self.file.certificate = Some(cert);
                self.file.policy = r.policy;
                self.file.next_poll_at = None;
                self.save();
                self.emit(Event::Approved);
                self.install_step()
            }
            _ => {
                // 拒絕、逾期、被取代、已撤回：刪除未使用的私鑰（agent.id 保留）
                let key = if self.file.is_grace() {
                    self.paths.grace_key()
                } else {
                    self.paths.request_key()
                };
                let _ = store::remove_if_exists(&key);
                self.file.poll_secret = None;
                self.file.csr = None;
                self.file.next_poll_at = None;
                self.save();
                let code = code.unwrap_or(codes::E_REQUEST_EXPIRED);
                self.emit(Event::Stopped { code });
                Next::Done(Finish::Stopped { code })
            }
        }
    }

    fn on_error(&mut self, e: ApiError, phase: Phase, sent_at: &str) -> Next {
        let decision = machine::on_error(
            &e,
            phase,
            &mut self.net,
            self.timing.clock_retry,
            &mut *self.rand,
        );
        self.record_error(&e);
        match decision {
            Decision::Retry {
                wait,
                code,
                state: st,
                rate_limited,
            } => {
                if code == codes::E_SOURCE_BLOCKED && self.stop_on_source_block {
                    return self.stop(state::SOURCE_BLOCKED, code, e.detail());
                }
                self.file.state = st.to_string();
                self.file.last_code = Some(code.to_string());
                let at = self.at(wait);
                if phase == Phase::Poll {
                    self.file.next_poll_at = Some(at);
                } else {
                    self.file.next_submit_at = Some(at);
                }
                self.save();
                self.emit(Event::Retry {
                    code,
                    wait,
                    rate_limited,
                });
                Next::Wait(wait)
            }
            Decision::ClockRejected {
                wait,
                offset_ms,
                server_time,
            } => {
                self.file.state = state::CLOCK_REJECTED.to_string();
                self.file.last_code = Some(codes::E_CLOCK_SKEW.to_string());
                self.file.offset_ms = offset_ms;
                self.file.server_time = Some(server_time);
                self.file.local_time = Some(sent_at.to_string());
                self.file.next_submit_at = Some(self.at(wait));
                self.save();
                self.emit(Event::ClockRejected { wait });
                Next::Wait(wait)
            }
            Decision::Stop { state: st, code } => self.stop(st, code, e.detail()),
            Decision::ResignOnce => Next::Wait(Duration::ZERO),
        }
    }

    fn record_error(&mut self, e: &ApiError) {
        self.file.last_error = Some(e.detail());
        self.file.protocol_code = e.protocol_code().map(str::to_string);
        if let ApiError::Server(s) = e {
            self.file.scope = s.body.scope.map(|sc| {
                serde_json::to_value(sc)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            });
            if let Some(ip) = &s.body.observed_ip {
                self.file.observed_ip = Some(crate::network::sanitize_for_log(ip, 64));
            }
        }
    }

    fn stop(&mut self, st: &'static str, code: &'static str, detail: String) -> Next {
        self.file.state = st.to_string();
        if self.file.is_grace() && st.starts_with("grace_") {
            self.file.grace_renew_state = Some(st.to_string());
        }
        self.file.last_code = Some(code.to_string());
        self.file.last_error = Some(detail);
        self.file.next_submit_at = None;
        self.file.next_poll_at = None;
        if matches!(
            st,
            state::TOKEN_ERROR
                | state::NOT_ACCEPTED
                | state::SERVER_UNSUPPORTED
                | state::SOURCE_BLOCKED
        ) {
            // 申請沒有建立：token 與未使用的私鑰都不保留（之後以新的 pulse enroll 重來）
            self.file.token = None;
            self.file.csr = None;
            self.file.poll_secret = None;
            let _ = store::remove_if_exists(&self.paths.request_key());
        }
        self.save();
        self.emit(Event::Stopped { code });
        if st == state::REGISTERED {
            return Next::Done(Finish::NotExpired);
        }
        Next::Done(Finish::Stopped { code })
    }

    // ---- 安裝 ----

    fn install_step(&mut self) -> Next {
        match install(&self.paths, &mut self.file) {
            Ok(()) => {
                self.save();
                self.emit(Event::Installed);
                Next::Done(Finish::Installed)
            }
            Err((code, detail)) => self.stop(state::FAILED, code, detail),
        }
    }

    // ---- 過期續簽 ----

    async fn grace_once(&mut self) -> Next {
        let mut resigned = false;
        loop {
            let req = match build_grace_request(
                &self.paths,
                &mut self.file,
                self.client.now_ms(),
                &self.client.endpoint,
            ) {
                Ok(r) => r,
                Err((st, code, detail)) => return self.stop(st, code, detail),
            };
            // 送出前先記下 sent_at：這個值之後不再使用（Kairos 以簽章內容為重放快取的鍵）
            self.file.grace_attempts = self.file.grace_attempts.saturating_add(1);
            self.save();
            match self.client.create_renewal(&req).await {
                Ok(reply) => {
                    self.grace_backoff.reset();
                    self.file.grace_backoff_ms = None;
                    return self.on_result(reply.body);
                }
                Err(e) => {
                    let d = machine::on_grace_error(
                        &e,
                        &mut self.grace_backoff,
                        self.timing.clock_retry,
                        resigned,
                        &mut *self.rand,
                    );
                    self.record_error(&e);
                    self.file.grace_backoff_ms = Some(self.grace_backoff.current_ms());
                    match d {
                        Decision::ResignOnce => {
                            resigned = true;
                            continue;
                        }
                        Decision::Retry {
                            wait,
                            code,
                            state: st,
                            rate_limited,
                        } => {
                            self.file.state = st.to_string();
                            self.file.grace_renew_state = Some(st.to_string());
                            self.file.last_code = Some(code.to_string());
                            self.file.grace_next_attempt_at = Some(self.at(wait));
                            self.save();
                            if st == grace_state::BLOCKED
                                && let Some(tx) = &self.on_blocked
                            {
                                let _ = tx.send(());
                            }
                            self.emit(Event::Retry {
                                code,
                                wait,
                                rate_limited,
                            });
                            return Next::Wait(wait);
                        }
                        Decision::ClockRejected {
                            wait,
                            offset_ms,
                            server_time,
                        } => {
                            self.file.state = grace_state::CLOCK_REJECTED.to_string();
                            self.file.grace_renew_state =
                                Some(grace_state::CLOCK_REJECTED.to_string());
                            self.file.last_code = Some(codes::E_CLOCK_SKEW.to_string());
                            self.file.offset_ms = offset_ms;
                            self.file.server_time = Some(server_time);
                            self.file.local_time = Some(req.sent_at.clone());
                            self.file.grace_next_attempt_at = Some(self.at(wait));
                            self.save();
                            self.emit(Event::ClockRejected { wait });
                            return Next::Wait(wait);
                        }
                        Decision::Stop { state: st, code } => {
                            if st == state::REGISTERED {
                                // 依 Kairos 的時間仍有效：保留新私鑰與 CSR，回到 8443
                                self.file.state = state::REGISTERED.to_string();
                                self.file.grace_renew_state = None;
                                self.file.kind = "enroll".to_string();
                                self.save();
                                self.emit(Event::NotExpired);
                                return Next::Done(Finish::NotExpired);
                            }
                            return self.stop(st, code, e.detail());
                        }
                    }
                }
            }
        }
    }
}

/// 組出一則過期續簽請求（ENROLL.md 第 7.1 節）：新私鑰與 CSR 沿用 `grace.key`（沒有時產生），
/// 每次重新取 `sent_at`（不與最近一次相同）並以舊私鑰重新簽章
pub fn build_grace_request(
    paths: &IdentityPaths,
    file: &mut EnrollmentFile,
    now_ms: i64,
    endpoint: &str,
) -> Result<GraceRenewRequest, (&'static str, &'static str, String)> {
    let reject = |detail: String| (grace_state::REJECTED, codes::E_GRACE_IDENTITY, detail);
    let host_fingerprint = file.host_fingerprint.clone().ok_or_else(|| {
        reject(
            "讀不到這台主機的機器 ID（主機指紋），無法以舊私鑰證明身分；請以新的 token 重新註冊"
                .into(),
        )
    })?;
    let old_key = std::fs::read(paths.client_key())
        .map_err(|e| {
            reject(format!(
                "無法讀取舊私鑰 {}：{e}",
                paths.client_key().display()
            ))
        })
        .and_then(|pem| {
            keys::pkcs8_from_pem(&pem).map_err(|e| {
                reject(format!(
                    "{e}；不是 ECDSA P-256 的舊憑證不適用過期續簽，請以新的 token 重新註冊"
                ))
            })
        })?;
    let old_cert = std::fs::read_to_string(paths.client_cert())
        .map_err(|e| {
            reject(format!(
                "無法讀取舊憑證 {}：{e}",
                paths.client_cert().display()
            ))
        })
        .and_then(|pem| x509::pem_certs(&pem).map_err(reject))?;
    let leaf = old_cert[0].as_ref().to_vec();
    if file.csr.is_none() || !paths.grace_key().exists() {
        let pkcs8 = keys::generate_p256_pkcs8()
            .map_err(|e| (grace_state::RENEWING, codes::W_SERVER_UNEXPECTED, e))?;
        let pem = keys::pkcs8_pem(&pkcs8)
            .map_err(|e| (grace_state::RENEWING, codes::W_SERVER_UNEXPECTED, e))?;
        store::write_atomic(&paths.grace_key(), pem.as_bytes(), FileKind::Secret).map_err(|e| {
            (
                grace_state::RENEWING,
                codes::W_SERVER_UNEXPECTED,
                format!("無法寫入 {}：{e}", paths.grace_key().display()),
            )
        })?;
        let csr = keys::build_csr(&pkcs8, &file.agent_id)
            .map_err(|e| (grace_state::RENEWING, codes::W_SERVER_UNEXPECTED, e))?;
        file.csr = Some(ke::b64url_encode(&csr));
        file.poll_secret = Some(
            keys::new_poll_secret()
                .map_err(|e| (grace_state::RENEWING, codes::W_SERVER_UNEXPECTED, e))?,
        );
    }
    let csr_b64 = file.csr.clone().unwrap_or_default();
    let csr = ke::b64url_decode(&csr_b64).map_err(|e| {
        (
            grace_state::RENEWING,
            codes::W_SERVER_UNEXPECTED,
            format!("enrollment.json 的 CSR 損毀：{e}"),
        )
    })?;
    let last = file
        .grace_last_sent_at
        .as_deref()
        .and_then(|t| ke::parse_rfc3339_ms(t).ok());
    let sent_at = ke::format_rfc3339_ms(schedule::next_grace_sent_at(now_ms, last));
    let input =
        ke::grace_renew_signing_input(&file.agent_id, &host_fingerprint, &csr, &sent_at, endpoint)
            .map_err(|e| reject(format!("無法組出簽章內容：{e}")))?;
    let signature =
        ke::sign_ecdsa_p256_sha256(&old_key, &input).map_err(|e| reject(e.to_string()))?;
    file.grace_last_sent_at = Some(sent_at.clone());
    Ok(GraceRenewRequest {
        agent_id: file.agent_id.clone(),
        csr: csr_b64,
        poll_secret: file.poll_secret.clone().unwrap_or_default(),
        sent_at,
        host_fingerprint,
        expired_certificate: ke::b64url_encode(&leaf),
        signature,
        signature_alg: SignatureAlg::EcdsaP256Sha256,
        hostname: file.hostname.clone().unwrap_or_default(),
        pulse_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// 安裝已取得的憑證（設計包 §7、ENROLL.md 第 6.5 節）：確認 SAN 為 `DNS:<agent_id>`、與私鑰成對、
/// `ca_pem` 的 SPKI 與信任錨相同，再原子寫入。可重複執行（中斷後從 `enrollment.json` 重來）。
pub fn install(
    paths: &IdentityPaths,
    file: &mut EnrollmentFile,
) -> Result<(), (&'static str, String)> {
    let Some(cert) = file.certificate.clone() else {
        return Ok(());
    };
    let leaf_der = x509::pem_certs(&cert.client_cert_pem)
        .map_err(|e| (codes::W_SERVER_UNEXPECTED, format!("client_cert_pem：{e}")))?;
    if leaf_der.len() != 1 {
        return Err((
            codes::W_SERVER_UNEXPECTED,
            format!(
                "client_cert_pem 應恰好一張憑證（收到 {} 張）",
                leaf_der.len()
            ),
        ));
    }
    let leaf = x509::parse_cert(&leaf_der[0])
        .ok_or_else(|| (codes::W_SERVER_UNEXPECTED, "無法解析取得的憑證".to_string()))?;
    let agent_id = file.agent_id.to_ascii_lowercase();
    if leaf.dns_sans.len() != 1
        || !leaf.dns_sans[0].eq_ignore_ascii_case(&agent_id)
        || leaf.other_sans != 0
    {
        return Err((
            codes::E_CLIENT_CERT_MISMATCH,
            format!(
                "取得的憑證 SAN 不是恰好一個 DNS:{agent_id}（收到 {:?}）",
                leaf.dns_sans
            ),
        ));
    }
    // CA：SPKI 必須等於指紋或既有 ca.pem 的 SPKI（防止中途替換）
    let ca =
        x509::pem_certs(&cert.ca_pem).map_err(|e| (codes::E_CA_PIN, format!("ca_pem：{e}")))?;
    let ca_info =
        x509::parse_cert(&ca[0]).ok_or_else(|| (codes::E_CA_PIN, "無法解析 ca_pem".to_string()))?;
    let ca_pin = ke::ca_pin_from_spki(&ca_info.spki);
    let mut trusted = file.trusted_ca_pins.clone();
    if let Ok(existing) = std::fs::read_to_string(paths.ca())
        && let Ok(certs) = x509::pem_certs(&existing)
    {
        trusted.extend(
            certs
                .iter()
                .filter_map(|c| x509::parse_cert(c))
                .map(|i| ke::ca_pin_from_spki(&i.spki)),
        );
    }
    if !trusted.contains(&ca_pin) {
        return Err((
            codes::E_CA_PIN,
            format!(
                "Kairos 回傳的 ca_pem（{ca_pin}）與 CA 指紋或既有的 ca.pem 不同，已中止，未寫入憑證"
            ),
        ));
    }
    let chain = if cert.chain_pem.trim().is_empty() {
        Vec::new()
    } else {
        x509::pem_certs(&cert.chain_pem)
            .map_err(|e| (codes::W_SERVER_UNEXPECTED, format!("chain_pem：{e}")))?
    };
    // 私鑰：申請中的私鑰（新註冊 request.key、過期續簽 grace.key）；上次安裝中斷時可能已改名為 client.key
    let pending = if file.is_grace() {
        paths.grace_key()
    } else {
        paths.request_key()
    };
    let key_spki = |path: &std::path::Path| {
        std::fs::read(path)
            .ok()
            .and_then(|pem| keys::pkcs8_from_pem(&pem).ok())
            .and_then(|k| ke::p256_spki_of_pkcs8(&k).ok())
    };
    let key_path = if pending.exists() {
        if key_spki(&pending).as_deref() != Some(leaf.spki.as_slice()) {
            return Err((
                codes::E_KEY_MISMATCH,
                format!("取得的憑證與申請時的私鑰 {} 不成對", pending.display()),
            ));
        }
        Some(pending)
    } else if key_spki(&paths.client_key()).as_deref() == Some(leaf.spki.as_slice()) {
        None
    } else {
        return Err((
            codes::E_KEY_MISMATCH,
            "找不到與取得的憑證成對的私鑰（申請時的私鑰已遺失）".to_string(),
        ));
    };
    let mut pem = x509::cert_pem(&leaf_der[0]);
    for c in &chain {
        pem.push_str(&x509::cert_pem(c));
    }
    let mut ca_pem = String::new();
    for c in &ca {
        ca_pem.push_str(&x509::cert_pem(c));
    }
    let io_err =
        |what: &str, e: std::io::Error| (codes::E_CLIENT_CERT_LOAD, format!("無法寫入{what}：{e}"));
    store::write_atomic(&paths.ca(), ca_pem.as_bytes(), FileKind::Certificate)
        .map_err(|e| io_err("ca.pem", e))?;
    if let Some(src) = key_path {
        store::rename_atomic(&src, &paths.client_key()).map_err(|e| io_err("client.key", e))?;
    }
    store::write_atomic(&paths.client_cert(), pem.as_bytes(), FileKind::Certificate)
        .map_err(|e| io_err("client.pem", e))?;
    let old_serial = file.cert_serial.clone();
    file.cert_serial = Some(leaf.serial_hex());
    file.cert_not_after = Some(crate::status_file::ts(leaf.not_after));
    if let Some(CertPolicy {
        expired_renew_grace_days,
        ..
    }) = cert_policy(file)
    {
        file.grace_days = Some(expired_renew_grace_days);
    }
    file.previous_serial = old_serial.or(file.previous_serial.take());
    file.certificate = None;
    file.csr = None;
    file.poll_secret = None;
    file.token = None;
    file.state = state::ISSUED.to_string();
    file.kind = "enroll".to_string();
    file.grace_renew_state = None;
    file.grace_next_attempt_at = None;
    file.grace_ends_at = None;
    file.grace_attempts = 0;
    file.grace_backoff_ms = None;
    file.last_error = None;
    file.last_code = Some(codes::I_ISSUED.to_string());
    file.offset_ms = None;
    Ok(())
}

fn cert_policy(file: &EnrollmentFile) -> Option<CertPolicy> {
    file.policy
}

/// `last_code` 轉成程式中的常數（不認得時為 `PLS-W2107`）
pub fn static_code(code: Option<&str>) -> &'static str {
    codes::known(code.unwrap_or_default()).unwrap_or(codes::W_SERVER_UNEXPECTED)
}

/// 等待結束的原因
pub enum SleepEnd {
    Elapsed,
    ClockAdjusted,
    Cancelled,
}

/// 等待 `d`；`watch_clock` 時每 `check` 比對牆鐘與單調時鐘，本機時間被調整（差距變動超過 1 秒）
/// 時提前結束（設計包 §5.12：時間校正後立即重送）
pub async fn sleep_watching_clock(
    d: Duration,
    watch_clock: bool,
    client: &EnrollClient,
    check: Duration,
    cancel: &CancellationToken,
) -> SleepEnd {
    let end = Instant::now() + d;
    let wall0 = client.now_ms();
    let mono0 = Instant::now();
    loop {
        let step = if watch_clock {
            check.min(end.saturating_duration_since(Instant::now()))
        } else {
            end.saturating_duration_since(Instant::now())
        };
        tokio::select! {
            _ = tokio::time::sleep(step) => {}
            _ = cancel.cancelled() => return SleepEnd::Cancelled,
        }
        if Instant::now() >= end {
            return SleepEnd::Elapsed;
        }
        if watch_clock {
            let wall = client.now_ms() - wall0;
            let mono = i64::try_from(mono0.elapsed().as_millis()).unwrap_or(i64::MAX);
            if schedule::clock_adjusted(wall, mono) {
                log::info!("偵測到本機時間被調整，立即重新送出");
                return SleepEnd::ClockAdjusted;
            }
        }
    }
}

// ---- 撤回待核准的申請（ENROLL.md 第 8 節、設計包 §5.11） ----

/// 撤回的結果（`withdrawal` 物件，`install-ux.md` 5.3）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    /// `ok`／`not_needed`／`unreported`／`skipped`
    pub result: &'static str,
    /// 失敗時 `PLS-W3310`
    pub code: Option<&'static str>,
    /// `unreachable`／`unsupported`／`already_approved`／`signature_rejected`／`server_error`
    pub reason: Option<&'static str>,
    /// 底層代碼（例如 `PLS-E2003`）
    pub cause_code: Option<&'static str>,
    pub protocol_code: Option<String>,
    pub request_code: Option<String>,
    /// Kairos 回應的申請狀態（`withdrawn`／`issued`／`rejected`／`request_expired`）
    pub request_state: Option<String>,
    pub attempts: u32,
    pub already_withdrawn: bool,
}

impl Withdrawal {
    fn new(result: &'static str, file: &EnrollmentFile) -> Self {
        Self {
            result,
            code: None,
            reason: None,
            cause_code: None,
            protocol_code: None,
            request_code: file.request_code.clone(),
            request_state: None,
            attempts: 0,
            already_withdrawn: false,
        }
    }

    fn unreported(mut self, reason: &'static str, cause: Option<&'static str>) -> Self {
        self.result = "unreported";
        self.code = Some(codes::W_WITHDRAW_UNREPORTED);
        self.reason = Some(reason);
        self.cause_code = cause;
        self
    }
}

/// 撤回整體的時限（設計包 §5.11）
pub const WITHDRAW_TOTAL: Duration = Duration::from_secs(15);
/// 每個請求的回應逾時
const WITHDRAW_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// 撤回 `file` 這筆待核准的申請。私鑰為 `request.key`。不修改本機檔案（由呼叫端清除）
pub async fn withdraw(
    paths: &IdentityPaths,
    file: &EnrollmentFile,
    client: &mut EnrollClient,
    reason: WithdrawReason,
    initiated_by: InitiatedBy,
) -> Withdrawal {
    let base = Withdrawal::new("ok", file);
    match file.state.as_str() {
        state::PENDING_APPROVAL => {}
        // 申請尚未建立（排隊中、時鐘偏差暫不受理）或已結束：Kairos 上沒有待核准的申請
        state::SUBMITTING | state::CLOCK_REJECTED => {
            return Withdrawal {
                result: "skipped",
                ..base
            };
        }
        _ => {
            return Withdrawal {
                result: "not_needed",
                ..base
            };
        }
    }
    let Some(request_id) = file.request_id.clone() else {
        return base.unreported("unsupported", None);
    };
    let key = match std::fs::read(paths.request_key())
        .map_err(|e| e.to_string())
        .and_then(|pem| keys::pkcs8_from_pem(&pem))
    {
        Ok(k) => k,
        Err(_) => return base.unreported("signature_rejected", None),
    };
    let deadline = Instant::now() + WITHDRAW_TOTAL;
    let mut out = base;
    let mut retried = false;
    loop {
        out.attempts += 1;
        let left = deadline.saturating_duration_since(Instant::now());
        let step = match client
            .withdraw_challenge(&request_id, WITHDRAW_REQUEST_TIMEOUT.min(left))
            .await
        {
            Ok(ch) => {
                let signature = ke::withdraw_signing_input(
                    &request_id,
                    &file.agent_id,
                    &ch.body.nonce,
                    &file.enroll_server,
                )
                .map_err(|e| e.to_string())
                .and_then(|i| ke::sign_ecdsa_p256_sha256(&key, &i).map_err(|e| e.to_string()));
                let Ok(signature) = signature else {
                    return out.unreported("signature_rejected", None);
                };
                let req = WithdrawRequest {
                    request_id: request_id.clone(),
                    agent_id: file.agent_id.clone(),
                    nonce: ch.body.nonce,
                    signature,
                    signature_alg: SignatureAlg::EcdsaP256Sha256,
                    reason,
                    initiated_by,
                    hostname: file.hostname.clone().unwrap_or_else(|| "unknown".into()),
                    pulse_version: env!("CARGO_PKG_VERSION").to_string(),
                    sent_at: ke::format_rfc3339_ms(client.now_ms()),
                    host_fingerprint: file.host_fingerprint.clone(),
                };
                let left = deadline.saturating_duration_since(Instant::now());
                match client
                    .withdraw(&req, WITHDRAW_REQUEST_TIMEOUT.min(left))
                    .await
                {
                    Ok(r) => {
                        out.result = "ok";
                        out.request_state = Some("withdrawn".to_string());
                        out.already_withdrawn = r.body.already_withdrawn;
                        return out;
                    }
                    Err(e) => withdraw_error(out, &e),
                }
            }
            Err(e) => withdraw_error(out, &e),
        };
        match step {
            WStep::Done(done) => return done,
            WStep::Retry(o, wait, why, cause) => {
                if retried || Instant::now() + wait >= deadline {
                    return o.unreported(why, cause);
                }
                out = o;
                retried = true;
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// 撤回一步之後：結束，或等待後從 challenge 重來（只重試 1 次）
enum WStep {
    Done(Withdrawal),
    Retry(Withdrawal, Duration, &'static str, Option<&'static str>),
}

/// 撤回的錯誤處置（設計包 §5.11 的錯誤表）
fn withdraw_error(mut out: Withdrawal, e: &ApiError) -> WStep {
    out.protocol_code = e.protocol_code().map(str::to_string);
    match e {
        ApiError::Server(s) => match s.code {
            EnrollErrorCode::RequestNotPending => {
                let st = s.body.state.and_then(|st| {
                    serde_json::to_value(st)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                });
                out.request_state = st.clone();
                if st.as_deref() == Some("issued") {
                    WStep::Done(out.unreported("already_approved", None))
                } else {
                    out.result = "not_needed";
                    WStep::Done(out)
                }
            }
            EnrollErrorCode::RequestNotFound => {
                out.result = "not_needed";
                WStep::Done(out)
            }
            EnrollErrorCode::NonceInvalid | EnrollErrorCode::NonceExpired => {
                WStep::Retry(out, Duration::ZERO, "server_error", None)
            }
            EnrollErrorCode::WithdrawSignatureInvalid => {
                WStep::Done(out.unreported("signature_rejected", None))
            }
            EnrollErrorCode::StorageUnavailable => {
                WStep::Retry(out, Duration::from_secs(2), "server_error", None)
            }
            EnrollErrorCode::RateLimited
            | EnrollErrorCode::SourceBlocked
            | EnrollErrorCode::EnrollBusy => {
                let wait = s.wait_hint().unwrap_or(Duration::from_secs(60));
                WStep::Retry(out, wait, "unreachable", Some(codes::E_ENROLL_RATE_LIMITED))
            }
            _ => WStep::Done(out.unreported("server_error", None)),
        },
        ApiError::Unsupported { .. } => WStep::Done(out.unreported("unsupported", None)),
        // 連到的不是原本的 Kairos：不重試、不送出簽章
        ApiError::Trust { code, .. } => WStep::Done(out.unreported("unreachable", Some(code))),
        ApiError::Network { code, .. } => {
            WStep::Retry(out, Duration::from_secs(2), "unreachable", Some(code))
        }
        ApiError::Unexpected { .. } => WStep::Done(out.unreported("server_error", None)),
    }
}

/// 新的過期續簽（服務偵測到 Kairos 確認憑證已過期時）
pub fn start_grace(file: &mut EnrollmentFile, enroll_server: &str, trusted_pins: Vec<String>) {
    file.kind = "grace_renewal".to_string();
    file.state = grace_state::RENEWING.to_string();
    file.grace_renew_state = Some(grace_state::RENEWING.to_string());
    file.enroll_server = enroll_server.to_string();
    file.trusted_ca_pins = trusted_pins;
    file.request_id = None;
    file.request_code = None;
    file.submitted_at = None;
    file.poll_interval_ms = None;
    file.poll_floor_ms = None;
    file.last_code = Some(codes::E_CLIENT_CERT_EXPIRED.to_string());
}
