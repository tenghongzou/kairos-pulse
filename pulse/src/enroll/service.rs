//! 服務（`pulse run`）的註冊閘門、等待核准的背景輪詢與過期寬限期續簽
//! （設計包 `enrollment-ux.md` §4、§5.9、§5.12、§5.13、§8、§9）。
//!
//! - **啟動時**（[`gate`]）：`enrollment.json` 有進行中的申請、而沒有可用的身分時，先完成申請
//!   （[`run_pending`]：依 `next_poll_at`／`next_submit_at` 恢復，不立即輪詢）；申請已結束且無法自動恢復
//!   （被拒、逾期、token 錯誤…）時不連線，等待新的 `pulse enroll`（[`idle`]）。沒有 `enrollment.json`
//!   （手動憑證或未啟用 mTLS 的既有部署）時照舊連線。
//! - **憑證過期**：Kairos 以 TLS alert `CertificateExpired` 拒絕時（`network::RunExit::CertExpired`），
//!   改走 8444 過期續簽（[`run_grace`]）；每輪 8444 無法連線時對 8443 出示過期憑證報到一次。
//! - 每一步之前取得 `identity/.lock`（與 `pulse enroll` 互斥），之間不持有，`pulse enroll --cancel`
//!   可以隨時撤回。
//! - 狀態寫進 `status.json` 的 `identity`（`pulse status` 的「身分」「連線」列）與 systemd `Status:`。

use std::path::Path;
use std::time::Duration;

use kairos_proto::enroll as ke;
use tokio_util::sync::CancellationToken;

use super::client::{EnrollClient, system_now};
use super::flow::{self, Driver, Event, Finish, Next, Observer, SleepEnd};
use super::schedule;
use super::store::{EnrollmentFile, IdentityPaths, grace_state, state};
use super::trust::TrustSpec;
use super::x509;
use crate::codes::{self, Event as LogEvent};
use crate::config_manager::AppConfig;
use crate::status_file::{EnrollmentStatus, GraceStatus};
use crate::{local_status, service_status};

/// 啟動時的判斷
#[derive(Debug, Clone, PartialEq)]
pub enum Gate {
    /// 照常啟動；`background` 為仍在進行、但目前的身分還能用的申請（`pulse enroll --force` 等）
    Normal {
        background: Option<Box<EnrollmentFile>>,
    },
    /// 沒有可用的身分：先完成申請
    Pending(Box<EnrollmentFile>),
    /// 申請已結束、無法自動恢復，也沒有可用的身分：不連線
    Idle(Box<EnrollmentFile>),
    /// 憑證已過期、過期續簽進行中（服務重新啟動後接續）
    Grace(Box<EnrollmentFile>),
}

/// 設定中的用戶端憑證與私鑰都存在
fn identity_usable(config: &AppConfig, base: &Path) -> bool {
    let cert = config.server.client_cert.trim();
    let key = config.server.client_key.trim();
    !cert.is_empty() && !key.is_empty() && base.join(cert).is_file() && base.join(key).is_file()
}

/// 用戶端憑證是否由 `pulse enroll` 管理（設定指向 `identity/client.pem`）
pub fn identity_managed(config: &AppConfig, base: &Path, paths: &IdentityPaths) -> bool {
    let cert = config.server.client_cert.trim();
    !cert.is_empty() && same_file(&base.join(cert), &paths.client_cert())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// 啟動時的註冊閘門
pub fn gate(paths: &IdentityPaths, config: &AppConfig, base: &Path) -> Gate {
    crate::rotation::recover(paths);
    let file = match paths.load_enrollment() {
        Ok(Some(f)) => f,
        Ok(None) => return Gate::Normal { background: None },
        Err(e) => {
            log::warn!("{e}；略過註冊狀態，照常啟動");
            return Gate::Normal { background: None };
        }
    };
    let mut file = file;
    // 已取得、尚未安裝完成的憑證（安裝中斷）：先完成安裝
    if file.state == state::ISSUED && file.certificate.is_some() {
        match flow::install(paths, &mut file) {
            Ok(()) => {
                let _ = paths.save_enrollment(&file);
                log::info!(
                    "{}已完成上次中斷的憑證安裝（序號 {}）",
                    codes::tag(codes::ENROLL_APPROVED, Some(codes::I_ISSUED)),
                    file.cert_serial.as_deref().unwrap_or("?")
                );
            }
            Err((code, detail)) => log::error!(
                "{}無法安裝已取得的憑證：{detail}",
                codes::tag(codes::AGENT_FATAL, Some(code))
            ),
        }
    }
    let usable = identity_usable(config, base);
    publish_status(&file, paths);
    if file.is_grace() && file.state.starts_with("grace_") {
        return Gate::Grace(Box::new(file));
    }
    match file.state.as_str() {
        state::ISSUED | state::REGISTERED => Gate::Normal { background: None },
        s if state::is_active(s) && usable => Gate::Normal {
            background: Some(Box::new(file)),
        },
        s if state::is_active(s) => Gate::Pending(Box::new(file)),
        _ if usable => Gate::Normal { background: None },
        _ => Gate::Idle(Box::new(file)),
    }
}

/// [`run_pending`]、[`run_grace`] 的結果
#[derive(Debug, Clone, PartialEq)]
pub enum End {
    /// 已安裝新憑證
    Installed,
    /// 依 Kairos 的時間憑證仍有效（過期續簽）
    NotExpired,
    /// 不再自動重試
    Stopped,
    /// `enrollment.json` 被 `pulse enroll` 改變（取消、重新申請）
    Changed,
    Shutdown,
}

/// 8444 的信任來源：`identity/ca.pem`，沒有時 `enrollment.json` 的指紋，再沒有時 `server.ca_cert`
pub fn trust_for(
    paths: &IdentityPaths,
    file: &EnrollmentFile,
    config: Option<(&AppConfig, &Path)>,
) -> Option<TrustSpec> {
    if let Some(certs) = std::fs::read_to_string(paths.ca())
        .ok()
        .and_then(|p| x509::pem_certs(&p).ok())
    {
        return Some(TrustSpec::Anchors(certs));
    }
    if let Some(pin) = file
        .ca_pin
        .clone()
        .or_else(|| file.trusted_ca_pins.first().cloned())
    {
        return Some(TrustSpec::Pin(pin));
    }
    let (config, base) = config?;
    let ca = config.server.ca_cert.trim();
    if ca.is_empty() {
        return None;
    }
    std::fs::read_to_string(base.join(ca))
        .ok()
        .and_then(|p| x509::pem_certs(&p).ok())
        .map(TrustSpec::Anchors)
}

/// 步進執行申請：每一步之前取得檔案鎖並重新讀取 `enrollment.json`（`pulse enroll` 可能已改變它）
async fn step_loop(
    paths: &IdentityPaths,
    mut driver: Driver<'_>,
    shutdown: &CancellationToken,
) -> End {
    let mut wait = driver.initial_wait();
    let started = driver.file.clone();
    loop {
        let watch = matches!(
            driver.file.state.as_str(),
            state::CLOCK_REJECTED | grace_state::CLOCK_REJECTED
        );
        match flow::sleep_watching_clock(
            wait,
            watch,
            &driver.client,
            Duration::from_secs(1),
            shutdown,
        )
        .await
        {
            SleepEnd::Cancelled => return End::Shutdown,
            SleepEnd::Elapsed | SleepEnd::ClockAdjusted => {}
        }
        let lock = match paths.try_lock() {
            Ok(Some(l)) => l,
            Ok(None) => {
                log::debug!("pulse enroll 正在執行（identity/.lock），30 秒後再檢查");
                wait = Duration::from_secs(30);
                continue;
            }
            Err(e) => {
                log::warn!("無法取得 {}：{e}", paths.lock().display());
                wait = Duration::from_secs(60);
                continue;
            }
        };
        match paths.load_enrollment() {
            Ok(Some(f)) if same_request(&f, &driver.file, &started) => {
                // 同一筆申請：沿用磁碟上的版本（`pulse enroll` 可能已推進它）
                driver.file = f;
            }
            _ => {
                drop(lock);
                return End::Changed;
            }
        }
        let next = driver.step().await;
        drop(lock);
        match next {
            Next::Wait(w) => wait = w,
            Next::Done(Finish::Installed) => return End::Installed,
            Next::Done(Finish::NotExpired) => return End::NotExpired,
            Next::Done(Finish::Stopped { .. }) => return End::Stopped,
        }
    }
}

/// 磁碟上的 `enrollment.json`（`disk`）是否仍是這個迴圈處理中的同一筆申請：與 driver 目前的版本
/// （`current`，driver 每次改變都會存檔）或開始時的版本（`started`）的 `request_id`、CSR、種類相同。
///
/// 只和開始時比對是不夠的：過期續簽開始時還沒有 CSR，第一步產生並存檔後，若第一次送出沒有成功
/// （回應遺失、429、時鐘偏差、網路錯誤），下一輪會誤判為「被 `pulse enroll` 改變」而停止續簽。
/// `pulse enroll` 取消或重新申請時 `request_id`／CSR 會與兩者都不同
fn same_request(disk: &EnrollmentFile, current: &EnrollmentFile, started: &EnrollmentFile) -> bool {
    let same = |a: &EnrollmentFile, b: &EnrollmentFile| {
        a.request_id == b.request_id && a.csr == b.csr && a.kind == b.kind
    };
    same(disk, current)
        || same(disk, started)
        || (current.request_id.is_none() && disk.csr == started.csr)
}

/// 完成進行中的新註冊申請（送出、排隊重送、時鐘偏差重送、輪詢）
pub async fn run_pending(
    paths: &IdentityPaths,
    file: EnrollmentFile,
    shutdown: &CancellationToken,
) -> End {
    let Some(trust) = trust_for(paths, &file, None) else {
        log::error!(
            "{}{} 沒有可用的信任來源（CA 指紋或 ca.pem），無法接續註冊申請；請重新執行 pulse enroll",
            codes::tag(codes::AGENT_FATAL, Some(codes::E_CA_PIN)),
            paths.enrollment().display()
        );
        return End::Stopped;
    };
    let client = match EnrollClient::new(&file.enroll_server, trust, false, system_now()) {
        Ok(c) => c,
        Err(e) => {
            log::error!("無法接續註冊申請：{e}");
            return End::Stopped;
        }
    };
    let mut observer = ServiceObserver::new(paths.clone());
    observer.announce(&file);
    let mut driver = Driver::new(paths.clone(), file, client, &mut observer);
    // 服務在背景：遇到 source_blocked 時等封鎖結束後再試
    driver.stop_on_source_block = false;
    step_loop(paths, driver, shutdown).await
}

/// 申請已結束、沒有可用的身分：每分鐘檢查 `enrollment.json` 是否被新的 `pulse enroll` 改變
pub async fn idle(
    paths: &IdentityPaths,
    file: &EnrollmentFile,
    shutdown: &CancellationToken,
) -> End {
    let code = file
        .last_code
        .clone()
        .unwrap_or_else(|| codes::E_NOT_ENROLLED.to_string());
    service_status::set_status(&status_text(file));
    log::error!(
        "{}{}；不連線 Kairos，等待重新註冊（pulse enroll）",
        codes::tag(codes::AGENT_FATAL, codes::known(&code)),
        status_text(file)
    );
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            _ = shutdown.cancelled() => return End::Shutdown,
        }
        match paths.load_enrollment() {
            Ok(Some(f))
                if f.state == file.state
                    && f.request_id == file.request_id
                    && f.csr == file.csr => {}
            _ => return End::Changed,
        }
    }
}

/// 8444 過期續簽（設計包 §5.9、§8）。`check_in` 在每輪 8444 無法連線後呼叫一次（對 8443 報到）
pub async fn run_grace<F, Fut>(
    paths: &IdentityPaths,
    config: &AppConfig,
    base: &Path,
    agent_id: &str,
    info: &flow::RequestInfo,
    shutdown: &CancellationToken,
    mut check_in: F,
) -> End
where
    F: FnMut() -> Fut + Send,
    Fut: std::future::Future<Output = ()> + Send,
{
    let endpoint = match super::configured_enroll_endpoint(config) {
        Ok(e) => e,
        Err(e) => {
            log::error!("過期續簽：註冊端點設定不合法：{e}");
            return End::Stopped;
        }
    };
    let mut file = paths.load_enrollment().ok().flatten().unwrap_or_default();
    let fresh = !file.is_grace() || !file.state.starts_with("grace_");
    if fresh {
        let pins: Vec<String> = match trust_for(paths, &file, Some((config, base))) {
            Some(TrustSpec::Anchors(certs)) => certs
                .iter()
                .filter_map(|c| x509::parse_cert(c))
                .map(|i| ke::ca_pin_from_spki(&i.spki))
                .collect(),
            Some(TrustSpec::Pin(p)) => vec![p],
            _ => Vec::new(),
        };
        file.agent_id = agent_id.to_ascii_lowercase();
        file.hostname = Some(info.hostname.clone());
        file.host_fingerprint = info.host_fingerprint.clone();
        file.grace_attempts = 0;
        file.csr = None;
        flow::start_grace(&mut file, &endpoint, pins);
        if file.grace_days.is_none() {
            file.grace_days = Some(90);
        }
        let _ = super::store::remove_if_exists(&paths.grace_key());
        let _ = paths.save_enrollment(&file);
    }
    let Some(trust) = trust_for(paths, &file, Some((config, base))) else {
        log::error!(
            "{}憑證已過期，但沒有可驗證註冊端點的 CA（identity/ca.pem 或 server.ca_cert），無法自動續簽",
            codes::tag(codes::CERT_GRACE_RENEW_BLOCKED, Some(codes::E_CA_PIN))
        );
        return End::Stopped;
    };
    let client = match EnrollClient::new(&file.enroll_server, trust, false, system_now()) {
        Ok(c) => c,
        Err(e) => {
            log::error!("過期續簽：{e}");
            return End::Stopped;
        }
    };
    log::error!(
        "{}Kairos 確認用戶端憑證已過期，經註冊端點 {} 自動續簽（過期寬限期 {} 天，以 Kairos 的判斷為準）",
        codes::tag(codes::CERT_EXPIRED, Some(codes::E_CLIENT_CERT_EXPIRED)),
        file.enroll_server,
        file.grace_days.unwrap_or(90)
    );
    if fresh {
        // 服務啟動或剛偵測到過期：0～120 秒的隨機延遲（大量主機同時恢復時分散）
        let spread = crate::backoff::random_spread(schedule::grace_start_spread());
        file.grace_next_attempt_at = Some(ke::format_rfc3339_ms(
            chrono::Utc::now().timestamp_millis() + i64::try_from(spread.as_millis()).unwrap_or(0),
        ));
        let _ = paths.save_enrollment(&file);
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut observer = ServiceObserver::new(paths.clone());
    observer.announce(&file);
    let mut driver = Driver::new(paths.clone(), file, client, &mut observer);
    driver.on_blocked = Some(tx);
    let run = step_loop(paths, driver, shutdown);
    tokio::pin!(run);
    loop {
        tokio::select! {
            end = &mut run => return end,
            Some(()) = rx.recv() => check_in().await,
        }
    }
}

/// `enrollment.json` → `status.json` 的 `identity`
pub fn publish_status(file: &EnrollmentFile, paths: &IdentityPaths) {
    let (state, enrollment, grace) = status_of(file, paths);
    local_status::record(|s| s.set_enrollment(state, enrollment, grace));
}

/// 狀態檔的 `identity.state`、`enrollment`、`grace`
pub fn status_of(
    file: &EnrollmentFile,
    paths: &IdentityPaths,
) -> (
    Option<String>,
    Option<EnrollmentStatus>,
    Option<GraceStatus>,
) {
    if file.is_grace() {
        if file.state == grace_state::REVOKED {
            return (Some("revoked".into()), None, None);
        }
        let not_after = std::fs::read_to_string(paths.client_cert())
            .ok()
            .and_then(|p| x509::pem_certs(&p).ok())
            .and_then(|c| x509::parse_cert(&c[0]))
            .map(|i| i.not_after);
        let days = file.grace_days.unwrap_or(90);
        let grace = GraceStatus {
            days: Some(days),
            ends_at: not_after
                .map(|t| crate::status_file::ts(t + chrono::Duration::days(i64::from(days)))),
            renew_state: file
                .grace_renew_state
                .clone()
                .unwrap_or_else(|| file.state.clone()),
            attempts: file.grace_attempts,
            last_attempt_at: file.grace_last_sent_at.clone(),
            next_attempt_at: file.grace_next_attempt_at.clone(),
            last_code: file.last_code.clone(),
            offset_ms: file
                .offset_ms
                .filter(|_| file.state == grace_state::CLOCK_REJECTED),
            server_time: file
                .server_time
                .clone()
                .filter(|_| file.state == grace_state::CLOCK_REJECTED),
            request_code: file.request_code.clone(),
        };
        let st = if state::is_active(&file.state) || file.state == state::REGISTERED {
            file.state.clone()
        } else {
            "cert_expired".to_string()
        };
        return (
            Some(st),
            None,
            (!matches!(file.state.as_str(), state::ISSUED | state::REGISTERED)).then_some(grace),
        );
    }
    let enrollment = (file.state != state::REGISTERED).then(|| EnrollmentStatus {
        state: file.state.clone(),
        request_code: file.request_code.clone(),
        submitted_at: file.submitted_at.clone(),
        expires_at: file.expires_at.clone(),
        approved_by: file
            .decided_by
            .clone()
            .filter(|_| file.state == state::ISSUED),
        rejected_reason: file.reject_reason.clone(),
        offset_ms: file
            .offset_ms
            .filter(|_| file.state == state::CLOCK_REJECTED),
        local_time: file
            .local_time
            .clone()
            .filter(|_| file.state == state::CLOCK_REJECTED),
        server_time: file
            .server_time
            .clone()
            .filter(|_| file.state == state::CLOCK_REJECTED),
        next_submit_at: file.next_submit_at.clone(),
        next_poll_at: file.next_poll_at.clone(),
        last_code: file.last_code.clone(),
        observed_ip: file.observed_ip.clone(),
        enroll_server: Some(file.enroll_server.clone()).filter(|s| !s.is_empty()),
        token_prefix: file.token_prefix.clone(),
    });
    (Some(file.state.clone()), enrollment, None)
}

/// systemd `Status:` 與 Windows 服務描述的文字（設計包 §4.2「Status 文字」）
pub fn status_text(file: &EnrollmentFile) -> String {
    let code = file.last_code.as_deref().unwrap_or("");
    let rc = file.request_code.as_deref().unwrap_or("?");
    match file.state.as_str() {
        state::SUBMITTING if code == codes::E_ENROLL_RATE_LIMITED => {
            "註冊中：註冊端點忙碌，稍後自動重試（PLS-E2202，不需處理）".into()
        }
        state::SUBMITTING if code == codes::E_PENDING_QUEUE_FULL => {
            "註冊中：Kairos 的待核准佇列已滿，每小時自動重試（PLS-E3309）".into()
        }
        state::SUBMITTING => "註冊中".into(),
        state::CLOCK_REJECTED => format!(
            "無法註冊：本機時鐘與 Kairos 相差 {}（PLS-E8302，請校正時間；校正後自動重新送出）",
            crate::status_cmd::fmt::offset_amount(file.offset_ms.unwrap_or(0))
        ),
        state::PENDING_APPROVAL => format!("等待核准：申請編號 {rc}"),
        state::ISSUED => "已核准，正在連線".into(),
        state::TOKEN_ERROR => format!(
            "無法註冊：{}（{code}，請向管理者索取新的 token）",
            codes::title(code).unwrap_or("註冊 token 無效")
        ),
        state::NOT_ACCEPTED => {
            "無法註冊：Kairos 目前不受理新的註冊申請（PLS-E2008，請管理者開放註冊端點）".into()
        }
        state::SOURCE_BLOCKED => "無法註冊：此網路位址暫時被封鎖（PLS-E3005，請確認 token 是否正確）".into(),
        state::SERVER_UNSUPPORTED => {
            "無法註冊：Kairos 版本不支援自動註冊（PLS-E2106，請升級 Kairos 或改用手動憑證）".into()
        }
        state::REJECTED => "無法註冊：註冊申請被拒絕（PLS-E3302，請向管理者確認原因）".into(),
        state::REQUEST_EXPIRED => {
            "無法註冊：註冊申請已逾期（PLS-E3303，請執行 pulse enroll --force 重新申請）".into()
        }
        grace_state::RENEWING => "無法連線：憑證已過期，正在自動續簽（PLS-E1108，不需處理）".into(),
        grace_state::BLOCKED => {
            "無法連線：憑證已過期，無法自動續簽（PLS-E2007，請管理者開放註冊端點）".into()
        }
        grace_state::PENDING => format!("無法連線：憑證已過期，續簽申請等待核准（申請編號 {rc}）"),
        grace_state::EXCEEDED => {
            "無法連線：憑證過期已超過寬限期（PLS-E1113，請管理者允許再次續簽或重新註冊）".into()
        }
        grace_state::CLOCK_REJECTED => {
            "無法連線：憑證已過期，Kairos 因時鐘偏差暫不受理續簽（PLS-E8302，請校正時間；校正後自動續簽）".into()
        }
        grace_state::REJECTED => {
            "無法連線：Kairos 拒絕自動續簽，主機識別資訊與紀錄不符（PLS-E1114，請重新註冊）".into()
        }
        grace_state::REVOKED => {
            "無法註冊：這台主機的憑證已被撤銷（PLS-E1105，請向管理者確認後執行 pulse enroll --force）".into()
        }
        _ => format!(
            "無法註冊：{}（{code}）",
            codes::title(code).unwrap_or("註冊申請無法完成")
        ),
    }
}

/// 服務端的事件記錄：日誌（`log-ux.md` §4.3、§4.4）、狀態檔與 systemd `Status:`
pub struct ServiceObserver {
    paths: IdentityPaths,
    last_code: Option<String>,
    last_offset: Option<i64>,
}

impl ServiceObserver {
    pub fn new(paths: IdentityPaths) -> Self {
        Self {
            paths,
            last_code: None,
            last_offset: None,
        }
    }

    /// 接續一筆申請時記一則日誌、更新狀態
    pub fn announce(&mut self, file: &EnrollmentFile) {
        publish_status(file, &self.paths);
        service_status::set_status(&status_text(file));
        let what = if file.is_grace() {
            "過期續簽"
        } else {
            "註冊申請"
        };
        log::info!(
            "接續{what}（狀態 {}，申請編號 {}，註冊端點 {}）",
            file.state,
            file.request_code.as_deref().unwrap_or("尚未建立"),
            file.enroll_server
        );
        self.last_code = file.last_code.clone();
    }
}

fn tag(event: LogEvent, code: &str) -> String {
    codes::tag(event, codes::known(code))
}

impl Observer for ServiceObserver {
    fn on(&mut self, file: &EnrollmentFile, event: &Event) {
        publish_status(file, &self.paths);
        service_status::set_status(&status_text(file));
        let rc = file.request_code.as_deref().unwrap_or("?");
        let grace = file.is_grace();
        let changed = self.last_code.as_deref() != file.last_code.as_deref();
        self.last_code = file.last_code.clone();
        match event {
            Event::Submitted => log::info!(
                "{}已送出{}，申請編號 {rc}  enroll_server={} token_prefix={}",
                codes::tag(codes::ENROLL_SUBMITTED, None),
                if grace {
                    "過期續簽申請"
                } else {
                    "註冊申請"
                },
                file.enroll_server,
                file.token_prefix.as_deref().unwrap_or("-")
            ),
            Event::Pending { first: true, .. } => log::info!(
                "{}等待 Kairos 管理者核准（申請編號 {rc}，有效至 {}）",
                codes::tag(codes::ENROLL_PENDING, Some(codes::I_PENDING_APPROVAL)),
                file.expires_at.as_deref().unwrap_or("?")
            ),
            Event::Pending { .. } => log::debug!("申請 {rc} 仍在等待核准"),
            Event::Retry {
                code,
                wait,
                rate_limited,
            } => {
                let detail = file.last_error.as_deref().unwrap_or("");
                let next = format!("{} 秒後重試", wait.as_secs());
                if grace {
                    if changed {
                        let msg = format!(
                            "{}{}；{next}  attempts={}",
                            tag(codes::CERT_GRACE_RENEW_BLOCKED, code),
                            status_text(file),
                            file.grace_attempts
                        );
                        log::error!("{msg}");
                        service_status::attention(&msg);
                    } else {
                        log::debug!(
                            "{}{detail}；{next}",
                            tag(codes::CERT_GRACE_RENEW_ATTEMPT, code)
                        );
                    }
                } else if *rate_limited {
                    let event = match *code {
                        codes::E_PENDING_QUEUE_FULL => codes::ENROLL_QUEUE_FULL,
                        codes::E_SOURCE_BLOCKED => codes::ENROLL_SOURCE_BLOCKED,
                        _ => codes::ENROLL_RATE_LIMITED,
                    };
                    if changed {
                        log::warn!(
                            "{}註冊端點忙碌，申請暫緩送出；{next}  scope={} observed_ip={}",
                            tag(event, code),
                            file.scope.as_deref().unwrap_or("-"),
                            file.observed_ip.as_deref().unwrap_or("-")
                        );
                    } else {
                        log::debug!("註冊端點仍忙碌；{next}");
                    }
                } else {
                    log::warn!(
                        "{}無法連線到註冊端點：{detail}；{next}",
                        tag(codes::CONN_LOST, code)
                    );
                }
            }
            Event::ClockRejected { wait } => {
                let offset = file.offset_ms.unwrap_or(0);
                if let Some(o) = file.offset_ms {
                    local_status::record(|s| s.clock_from_enroll(o, "clock_skew"));
                }
                let moved = self.last_offset.is_none_or(|l| (l - offset).abs() > 60_000);
                self.last_offset = Some(offset);
                let event = if grace {
                    codes::CERT_GRACE_RENEW_BLOCKED
                } else {
                    codes::ENROLL_CLOCK_REJECTED
                };
                let msg = format!(
                    "{}Kairos 因時鐘偏差暫不受理{}：本機與 Kairos 相差 {}；時間校正後立即重送，否則 {} 秒後再試  offset_ms={offset} server_time={} local_time={}",
                    codes::tag(event, Some(codes::E_CLOCK_SKEW)),
                    if grace { "自動續簽" } else { "註冊" },
                    crate::status_cmd::fmt::offset_amount(offset),
                    wait.as_secs(),
                    file.server_time.as_deref().unwrap_or("?"),
                    file.local_time.as_deref().unwrap_or("?")
                );
                if moved {
                    log::error!("{msg}");
                    service_status::attention(&msg);
                } else {
                    log::debug!("{msg}");
                }
            }
            Event::Approved => log::info!(
                "{}已核准（{}），正在安裝憑證",
                codes::tag(codes::ENROLL_APPROVED, Some(codes::I_ISSUED)),
                file.decided_by.as_deref().unwrap_or("?")
            ),
            Event::Installed if grace => {}
            Event::Installed => log::info!(
                "{}已安裝憑證（序號 {}，有效至 {}）",
                codes::tag(codes::CERT_RENEWED, None),
                file.cert_serial.as_deref().unwrap_or("?"),
                file.cert_not_after.as_deref().unwrap_or("?")
            ),
            Event::NotExpired => {
                log::info!("Kairos 回應目前的憑證仍有效（本機時鐘可能偏快），改以目前的憑證連線")
            }
            Event::Stopped { code } => {
                let event = match *code {
                    codes::E_REQUEST_REJECTED => codes::ENROLL_REJECTED,
                    codes::E_TOKEN_INVALID | codes::E_TOKEN_EXPIRED | codes::E_TOKEN_EXHAUSTED => {
                        codes::ENROLL_TOKEN_ERROR
                    }
                    codes::E_REQUEST_EXPIRED => codes::ENROLL_REQUEST_EXPIRED,
                    codes::E_ENROLL_NOT_ACCEPTED => codes::ENROLL_NOT_ACCEPTED,
                    codes::E_SOURCE_BLOCKED => codes::ENROLL_SOURCE_BLOCKED,
                    codes::E_GRACE_EXCEEDED => codes::CERT_GRACE_EXCEEDED,
                    codes::E_GRACE_IDENTITY => codes::CERT_GRACE_REJECTED,
                    _ => codes::AGENT_FATAL,
                };
                let msg = format!(
                    "{}{}{}",
                    codes::tag(event, Some(code)),
                    status_text(file),
                    file.last_error
                        .as_deref()
                        .map(|e| format!("：{e}"))
                        .unwrap_or_default()
                );
                log::error!("{msg}");
                service_status::attention(&msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> IdentityPaths {
        IdentityPaths::new(
            std::env::temp_dir()
                .join("pulse-enroll-service-tests")
                .join(uuid::Uuid::new_v4().to_string())
                .join("identity"),
        )
    }

    fn cfg() -> AppConfig {
        AppConfig::default_for_tests()
    }

    #[test]
    fn step_loop_keeps_a_grace_renewal_after_its_csr_is_created() {
        let started = EnrollmentFile {
            kind: "grace_renewal".into(),
            ..Default::default()
        };
        // 第一步產生 CSR 並存檔（尚未取得 request_id）：仍是同一筆
        let mut current = started.clone();
        current.csr = Some("csr-1".into());
        let disk = current.clone();
        assert!(same_request(&disk, &current, &started));
        // Kairos 受理後有 request_id：仍是同一筆
        current.request_id = Some("r-1".into());
        assert!(same_request(&current.clone(), &current, &started));
        // pulse enroll 改成另一筆申請（不同的 CSR 與 request_id）：已改變
        let mut other = current.clone();
        other.csr = Some("csr-2".into());
        other.request_id = Some("r-2".into());
        assert!(!same_request(&other, &current, &started));
        // 新註冊：開始時就有 CSR，pulse enroll 推進同一筆（取得 request_id）
        let mut new = EnrollmentFile {
            kind: "enroll".into(),
            csr: Some("c".into()),
            ..Default::default()
        };
        let begun = new.clone();
        new.request_id = Some("r".into());
        assert!(same_request(&new, &begun, &begun));
        // 取消（檔案刪除後重建、kind 不同）：已改變
        let cancelled = EnrollmentFile::default();
        assert!(!same_request(&cancelled, &new, &begun));
    }

    #[test]
    fn gate_follows_the_enrollment_state() {
        let p = paths();
        let base = p.dir.parent().unwrap().to_path_buf();
        let c = cfg();
        assert_eq!(
            gate(&p, &c, &base),
            Gate::Normal { background: None },
            "沒有 enrollment.json：照舊"
        );
        let mut f = EnrollmentFile {
            kind: "enroll".into(),
            state: state::PENDING_APPROVAL.into(),
            request_code: Some("1KGJ-N9XN".into()),
            ..Default::default()
        };
        p.save_enrollment(&f).unwrap();
        assert!(matches!(gate(&p, &c, &base), Gate::Pending(_)));
        f.state = state::REJECTED.into();
        p.save_enrollment(&f).unwrap();
        assert!(matches!(gate(&p, &c, &base), Gate::Idle(_)));
        f.state = state::REGISTERED.into();
        p.save_enrollment(&f).unwrap();
        assert_eq!(gate(&p, &c, &base), Gate::Normal { background: None });
        f.kind = "grace_renewal".into();
        f.state = grace_state::BLOCKED.into();
        p.save_enrollment(&f).unwrap();
        assert!(matches!(gate(&p, &c, &base), Gate::Grace(_)));
        // 進行中、但目前的身分仍可用（--force）：照常連線，申請在背景接續
        let mut c = cfg();
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("c.pem"), "x").unwrap();
        std::fs::write(base.join("c.key"), "x").unwrap();
        c.server.client_cert = "c.pem".into();
        c.server.client_key = "c.key".into();
        f.kind = "enroll".into();
        f.state = state::SUBMITTING.into();
        p.save_enrollment(&f).unwrap();
        assert!(matches!(
            gate(&p, &c, &base),
            Gate::Normal {
                background: Some(_)
            }
        ));
    }

    #[test]
    fn status_texts_follow_the_design_table() {
        let mut f = EnrollmentFile {
            state: state::PENDING_APPROVAL.into(),
            request_code: Some("7KQ2-M9XD".into()),
            ..Default::default()
        };
        assert_eq!(status_text(&f), "等待核准：申請編號 7KQ2-M9XD");
        f.state = state::CLOCK_REJECTED.into();
        f.offset_ms = Some(-462_000);
        assert_eq!(
            status_text(&f),
            "無法註冊：本機時鐘與 Kairos 相差 7 分 42 秒（PLS-E8302，請校正時間；校正後自動重新送出）"
        );
        f.state = state::SUBMITTING.into();
        f.last_code = Some("PLS-E2202".into());
        assert!(status_text(&f).contains("PLS-E2202，不需處理"));
        f.state = state::TOKEN_ERROR.into();
        f.last_code = Some("PLS-E3003".into());
        assert_eq!(
            status_text(&f),
            "無法註冊：註冊 token 已過期（PLS-E3003，請向管理者索取新的 token）"
        );
        f.state = grace_state::BLOCKED.into();
        assert!(status_text(&f).starts_with("無法連線：憑證已過期，無法自動續簽（PLS-E2007"));
    }

    #[test]
    fn status_of_hides_secrets_and_maps_grace() {
        let p = paths();
        let f = EnrollmentFile {
            kind: "enroll".into(),
            state: state::CLOCK_REJECTED.into(),
            token: Some("KRT1-GE8Q-MMGW-WG5P-DP97".into()),
            poll_secret: Some("secret".into()),
            offset_ms: Some(-462_000),
            server_time: Some("2026-10-09T06:23:16.000Z".into()),
            local_time: Some("2026-10-09T06:15:34.000Z".into()),
            last_code: Some("PLS-E8302".into()),
            ..Default::default()
        };
        let (st, e, g) = status_of(&f, &p);
        assert_eq!(st.as_deref(), Some("clock_rejected"));
        let e = e.unwrap();
        assert_eq!(e.offset_ms, Some(-462_000));
        assert!(g.is_none());
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("KRT1") && !json.contains("secret"), "{json}");
        let g = EnrollmentFile {
            kind: "grace_renewal".into(),
            state: grace_state::BLOCKED.into(),
            grace_renew_state: Some(grace_state::BLOCKED.into()),
            grace_days: Some(90),
            last_code: Some("PLS-E2007".into()),
            ..Default::default()
        };
        let (st, e, grace) = status_of(&g, &p);
        assert_eq!(st.as_deref(), Some("cert_expired"));
        assert!(e.is_none());
        let grace = grace.unwrap();
        assert_eq!(grace.renew_state, "grace_blocked");
        assert_eq!(grace.days, Some(90));
    }
}
