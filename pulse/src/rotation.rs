//! 8443 上的正常輪替（feature `renew`；ENROLL.md 第 13.1 節、設計包 `enrollment-ux.md` §5.6、§8）。
//!
//! - 只在雙方都宣告 `renew`、且用戶端憑證由 `pulse enroll` 管理（`identity/client.pem`）時使用。
//! - 時點：已使用壽命的 2/3，或剩餘 ≤ `identity.renew_before_days`，取較早者；「現在」以
//!   `hello_ack.server_time` 的偏差校正。`hello_ack.renew_now` 時在 0～6 小時的隨機延遲後開始。
//! - 新私鑰與 CSR 先寫入 `identity/client.next.key` 與 `enrollment.json`（`rotation`），重送時沿用
//!   同一個 `request_id` 與 CSR（同一個 CSR 只會得到一張憑證）。
//! - 取得 `renew_result` 後驗證（SAN、與私鑰成對、`ca_pem` 與 `ca.pem` 相同）並寫入
//!   `client.next.pem`；連線以 [`RotationLine::Installed`] 結束，由主流程以新憑證另建連線送 `hello`，
//!   成功後才以 [`promote`] 取代 `client.pem`／`client.key`（設計包 §5.6）。
//! - 失敗（`renew_requires_approval`、`invalid_csr`、`storage_unavailable`、`rate_limited`）：
//!   `PLS-W1111`，1 → 12 小時退避，繼續使用舊憑證。
//! - Kairos 不支援 `renew`（舊版）：不輪替，維持 0.4 的行為（剩餘 14 天內 `PLS-W1110`）。

use kairos_proto::enroll as ke;
use kairos_proto::{ServerMsg, parse_server_msg};
use tokio::time::Instant;

use crate::codes;
use crate::enroll::schedule::{self, Exp};
use crate::enroll::store::{self, FileKind, IdentityPaths, Rotation as Pending};
use crate::enroll::{keys, x509};
use crate::lifecycle::{HelloAckExtra, Incoming, RenewRequest, parse_incoming, rotation_due};

/// 一條連線上 [`Rotation::on_line`] 的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotationLine {
    /// 不是輪替的訊息
    NotMine,
    /// 輪替的訊息，已處理（失敗或忽略）
    Handled,
    /// 已取得並驗證新憑證（`client.next.pem`），以新憑證重新連線
    Installed,
}

/// 正常輪替的狀態（跨連線保留）
#[derive(Debug)]
pub struct Rotation {
    paths: IdentityPaths,
    agent_id: String,
    renew_before_days: u32,
    /// 這條連線雙方都宣告了 `renew`
    negotiated: bool,
    /// 已送出、尚未回應的 `renew_request` 的 seq
    in_flight: Option<u64>,
    /// `renew_now`：不早於這個時間開始
    start_after: Option<Instant>,
    /// 失敗後的下次嘗試
    next_attempt: Option<Instant>,
    backoff: Exp,
    /// 本機減 Kairos（毫秒）
    offset_ms: Option<i64>,
}

impl Rotation {
    pub fn new(paths: IdentityPaths, agent_id: &str, renew_before_days: u32) -> Self {
        let saved = paths
            .load_enrollment()
            .ok()
            .flatten()
            .and_then(|f| f.rotation)
            .and_then(|r| r.backoff_ms);
        Self {
            paths,
            agent_id: agent_id.to_ascii_lowercase(),
            renew_before_days,
            negotiated: false,
            in_flight: None,
            start_after: None,
            next_attempt: None,
            backoff: Exp::resume(
                schedule::RENEW_BACKOFF_INITIAL,
                schedule::RENEW_BACKOFF_MAX,
                saved,
            ),
            offset_ms: None,
        }
    }

    /// 收到 `hello_ack`：記下協商結果與時鐘偏差；快取寬限天數、把註冊狀態改為 `registered`
    pub fn on_hello_ack(
        &mut self,
        negotiated: bool,
        extra: &HelloAckExtra,
        offset_ms: Option<i64>,
    ) {
        self.negotiated = negotiated;
        self.in_flight = None;
        self.offset_ms = offset_ms;
        if negotiated && extra.renew_now == Some(true) && self.start_after.is_none() {
            let delay = crate::backoff::random_spread(schedule::RENEW_NOW_SPREAD);
            log::info!(
                "Kairos 要求提早更新用戶端憑證（renew_now），{} 分鐘後開始",
                delay.as_secs() / 60
            );
            self.start_after = Some(Instant::now() + delay);
        }
        if let Ok(Some(mut file)) = self.paths.load_enrollment() {
            let mut changed = false;
            if let Some(days) = extra
                .expired_renew_grace_days
                .filter(|d| file.grace_days != Some(*d))
            {
                file.grace_days = Some(days);
                changed = true;
            }
            if file.state == store::state::ISSUED {
                file.state = store::state::REGISTERED.to_string();
                file.last_code = None;
                changed = true;
            }
            if changed && let Err(e) = self.paths.save_enrollment(&file) {
                log::warn!("無法更新 {}：{e}", self.paths.enrollment().display());
            }
        }
    }

    /// 斷線：在途的請求作廢（下一條連線以同一個 CSR 重送）
    pub fn on_disconnect(&mut self) {
        self.in_flight = None;
        self.negotiated = false;
    }

    fn current_validity(&self) -> Option<(i64, i64)> {
        let pem = std::fs::read_to_string(self.paths.client_cert()).ok()?;
        let leaf = x509::pem_certs(&pem).ok()?;
        let info = x509::parse_cert(&leaf[0])?;
        Some((
            info.not_before.timestamp_millis(),
            info.not_after.timestamp_millis(),
        ))
    }

    /// 該送 `renew_request` 時回傳它（以 `seq` 為序號）
    pub fn next_request(&mut self, seq: u64, now_wall_ms: i64) -> Option<RenewRequest> {
        if !self.negotiated || self.in_flight.is_some() {
            return None;
        }
        let now = Instant::now();
        if self.next_attempt.is_some_and(|t| now < t) {
            return None;
        }
        let corrected = now_wall_ms - self.offset_ms.unwrap_or(0);
        let (nb, na) = self.current_validity()?;
        let forced = self.start_after.is_some_and(|t| now >= t);
        if !forced && !rotation_due(nb, na, corrected, self.renew_before_days) {
            return None;
        }
        let mut file = self
            .paths
            .load_enrollment()
            .ok()
            .flatten()
            .unwrap_or_else(|| store::EnrollmentFile {
                kind: "enroll".into(),
                state: store::state::REGISTERED.into(),
                agent_id: self.agent_id.clone(),
                ..Default::default()
            });
        let pending = match file.rotation.clone() {
            Some(p) if self.paths.next_key().exists() && !p.csr.is_empty() => p,
            _ => match self.start_new() {
                Ok(p) => {
                    log::info!(
                        "{}用戶端憑證已到更新時點（有效至 {}），經 8443 送出新的 CSR",
                        codes::tag(codes::CERT_RENEW_STARTED, Some(codes::I_RENEWING)),
                        ke::format_rfc3339_ms(na)
                    );
                    p
                }
                Err(e) => {
                    log::warn!("無法準備憑證更新：{e}");
                    self.next_attempt = Some(now + self.backoff.next(0.5));
                    return None;
                }
            },
        };
        let mut pending = pending;
        pending.attempts = pending.attempts.saturating_add(1);
        file.rotation = Some(pending.clone());
        if let Err(e) = self.paths.save_enrollment(&file) {
            log::warn!("無法寫入 {}：{e}", self.paths.enrollment().display());
        }
        self.in_flight = Some(seq);
        let ts = ke::format_rfc3339_ms(now_wall_ms);
        Some(RenewRequest {
            seq,
            ts: ts.clone(),
            request_id: pending.request_id,
            csr: pending.csr,
            sent_at: ts,
        })
    }

    fn start_new(&self) -> Result<Pending, String> {
        let pkcs8 = keys::generate_p256_pkcs8()?;
        let pem = keys::pkcs8_pem(&pkcs8)?;
        store::write_atomic(&self.paths.next_key(), pem.as_bytes(), FileKind::Secret)
            .map_err(|e| format!("無法寫入 {}：{e}", self.paths.next_key().display()))?;
        let csr = keys::build_csr(&pkcs8, &self.agent_id)?;
        Ok(Pending {
            request_id: keys::new_request_uuid(),
            csr: ke::b64url_encode(&csr),
            started_at: crate::status_file::ts(chrono::Utc::now()),
            ..Default::default()
        })
    }

    /// 處理一行伺服器訊息
    pub fn on_line(&mut self, line: &str) -> RotationLine {
        if let Some(Incoming::RenewResult(r)) = parse_incoming(line) {
            let pending = self
                .paths
                .load_enrollment()
                .ok()
                .flatten()
                .and_then(|f| f.rotation);
            if pending
                .as_ref()
                .is_none_or(|p| p.request_id != r.request_id)
            {
                log::debug!(
                    "收到不屬於目前更新申請的 renew_result（request_id={}），忽略",
                    crate::network::sanitize_for_log(&r.request_id, 64)
                );
                return RotationLine::Handled;
            }
            self.in_flight = None;
            return match install_next(&self.paths, &self.agent_id, &r.certificate) {
                Ok(serial) => {
                    if let Ok(Some(mut f)) = self.paths.load_enrollment() {
                        if let Some(p) = f.rotation.as_mut() {
                            p.issued_serial = Some(serial);
                        }
                        if let Some(policy) = r.policy {
                            f.grace_days = Some(policy.expired_renew_grace_days);
                        }
                        let _ = self.paths.save_enrollment(&f);
                    }
                    self.backoff.reset();
                    RotationLine::Installed
                }
                Err((code, detail)) => {
                    self.fail(code, &detail);
                    RotationLine::Handled
                }
            };
        }
        let Some(seq) = self.in_flight else {
            return RotationLine::NotMine;
        };
        match parse_server_msg(line) {
            Ok(ServerMsg::Error {
                seq: Some(s),
                code,
                message,
                fatal: false,
                ..
            }) if s == seq => {
                self.in_flight = None;
                let why = match code.as_str() {
                    "renew_requires_approval" => "Kairos 的政策要求人工核准（已進入待核准佇列，核准後以同一個 CSR 重送即可取得）".to_string(),
                    "invalid_csr" => {
                        // CSR 被拒：下次重新產生私鑰與 CSR
                        let _ = store::remove_if_exists(&self.paths.next_key());
                        if let Ok(Some(mut f)) = self.paths.load_enrollment() {
                            f.rotation = None;
                            let _ = self.paths.save_enrollment(&f);
                        }
                        format!("Kairos 拒絕了 CSR：{}", crate::network::sanitize_for_log(&message, 200))
                    }
                    other => format!(
                        "{}：{}",
                        crate::network::sanitize_for_log(other, 64),
                        crate::network::sanitize_for_log(&message, 200)
                    ),
                };
                self.fail(codes::W_RENEW_FAILED, &why);
                RotationLine::Handled
            }
            _ => RotationLine::NotMine,
        }
    }

    fn fail(&mut self, code: &'static str, detail: &str) {
        let wait = self.backoff.next(0.5);
        self.next_attempt = Some(Instant::now() + wait);
        log::warn!(
            "{}用戶端憑證更新失敗，繼續使用目前的憑證：{detail}；{} 小時後重試",
            codes::tag(codes::CERT_RENEW_FAILED, Some(code)),
            wait.as_secs() / 3600
        );
        if let Ok(Some(mut f)) = self.paths.load_enrollment() {
            if let Some(p) = f.rotation.as_mut() {
                p.last_code = Some(code.to_string());
                p.backoff_ms = Some(self.backoff.current_ms());
                p.next_attempt_at = Some(crate::status_file::ts(
                    chrono::Utc::now() + chrono::Duration::from_std(wait).unwrap_or_default(),
                ));
            }
            let _ = self.paths.save_enrollment(&f);
        }
        crate::local_status::record(|s| s.renew_failed(code));
    }

    /// 以新憑證確認失敗（另建的連線無法 hello）：保留新憑證，退避後再試
    pub fn probe_failed(&mut self, detail: &str) {
        self.fail(
            codes::W_RENEW_FAILED,
            &format!("以新憑證連線失敗：{detail}"),
        );
    }
}

/// 驗證並寫入 `client.next.pem`；回傳新憑證的序號
fn install_next(
    paths: &IdentityPaths,
    agent_id: &str,
    cert: &ke::IssuedCertificate,
) -> Result<String, (&'static str, String)> {
    let leaf = x509::pem_certs(&cert.client_cert_pem)
        .map_err(|e| (codes::W_SERVER_UNEXPECTED, format!("client_cert_pem：{e}")))?;
    let info = x509::parse_cert(&leaf[0])
        .ok_or_else(|| (codes::W_SERVER_UNEXPECTED, "無法解析新憑證".to_string()))?;
    if info.dns_sans.len() != 1
        || !info.dns_sans[0].eq_ignore_ascii_case(agent_id)
        || info.other_sans != 0
    {
        return Err((
            codes::E_CLIENT_CERT_MISMATCH,
            format!("新憑證的 SAN 不是恰好一個 DNS:{agent_id}"),
        ));
    }
    let key = std::fs::read(paths.next_key())
        .map_err(|e| e.to_string())
        .and_then(|pem| keys::pkcs8_from_pem(&pem))
        .and_then(|k| ke::p256_spki_of_pkcs8(&k).map_err(|e| e.to_string()))
        .map_err(|e| (codes::E_KEY_MISMATCH, format!("讀不到申請時的私鑰：{e}")))?;
    if key != info.spki {
        return Err((
            codes::E_KEY_MISMATCH,
            "新憑證與申請時的私鑰不成對".to_string(),
        ));
    }
    // ca_pem 必須是目前信任的 CA（ca.pem）
    let ca =
        x509::pem_certs(&cert.ca_pem).map_err(|e| (codes::E_CA_PIN, format!("ca_pem：{e}")))?;
    let pin = x509::parse_cert(&ca[0])
        .map(|i| ke::ca_pin_from_spki(&i.spki))
        .ok_or_else(|| (codes::E_CA_PIN, "無法解析 ca_pem".to_string()))?;
    let trusted: Vec<String> = std::fs::read_to_string(paths.ca())
        .ok()
        .and_then(|p| x509::pem_certs(&p).ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| x509::parse_cert(c))
        .map(|i| ke::ca_pin_from_spki(&i.spki))
        .collect();
    if !trusted.is_empty() && !trusted.contains(&pin) {
        return Err((
            codes::E_CA_PIN,
            "renew_result 的 ca_pem 與 ca.pem 不同，未寫入新憑證".to_string(),
        ));
    }
    let mut pem = x509::cert_pem(&leaf[0]);
    if !cert.chain_pem.trim().is_empty() {
        for c in x509::pem_certs(&cert.chain_pem)
            .map_err(|e| (codes::W_SERVER_UNEXPECTED, format!("chain_pem：{e}")))?
        {
            pem.push_str(&x509::cert_pem(&c));
        }
    }
    store::write_atomic(&paths.next_cert(), pem.as_bytes(), FileKind::Certificate).map_err(
        |e| {
            (
                codes::E_CLIENT_CERT_LOAD,
                format!("無法寫入 {}：{e}", paths.next_cert().display()),
            )
        },
    )?;
    Ok(info.serial_hex())
}

/// 以新憑證連線成功後：原子取代 `client.key`／`client.pem`，更新 `enrollment.json`
pub fn promote(paths: &IdentityPaths) -> std::io::Result<String> {
    let pem = std::fs::read_to_string(paths.next_cert())?;
    let info = x509::pem_certs(&pem)
        .ok()
        .and_then(|c| x509::parse_cert(&c[0]))
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "client.next.pem 無法解析")
        })?;
    // 先換私鑰再換憑證；中斷時 enrollment.json 仍有 rotation.issued_serial，啟動時由 [`recover`] 完成
    store::rename_atomic(&paths.next_key(), &paths.client_key())?;
    store::rename_atomic(&paths.next_cert(), &paths.client_cert())?;
    if let Ok(Some(mut f)) = paths.load_enrollment() {
        let old = f.cert_serial.replace(info.serial_hex());
        f.previous_serial = old;
        f.cert_not_after = Some(crate::status_file::ts(info.not_after));
        f.rotation = None;
        f.state = store::state::REGISTERED.to_string();
        paths.save_enrollment(&f)?;
    }
    Ok(info.serial_colon())
}

/// 啟動時：上次輪替在取代檔案途中中斷（`client.key` 已換、`client.pem` 還是舊的）時完成它
pub fn recover(paths: &IdentityPaths) {
    if !paths.next_cert().exists() || paths.next_key().exists() {
        return;
    }
    let pair = (|| {
        let key = keys::pkcs8_from_pem(&std::fs::read(paths.client_key()).ok()?).ok()?;
        let spki = ke::p256_spki_of_pkcs8(&key).ok()?;
        let pem = std::fs::read_to_string(paths.next_cert()).ok()?;
        let info = x509::parse_cert(&x509::pem_certs(&pem).ok()?[0])?;
        Some(spki == info.spki)
    })();
    if pair == Some(true) {
        match store::rename_atomic(&paths.next_cert(), &paths.client_cert()) {
            Ok(()) => log::info!("已完成上次中斷的憑證更新"),
            Err(e) => log::warn!("無法完成上次中斷的憑證更新：{e}"),
        }
    }
}

/// 失敗時等待（測試用：取得目前的退避值）
#[cfg(test)]
impl Rotation {
    fn backoff_current(&self) -> std::time::Duration {
        self.backoff.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";

    fn setup(
        not_before_days: i64,
        not_after_days: i64,
    ) -> (
        IdentityPaths,
        rcgen::KeyPair,
        rcgen::CertificateParams,
        String,
    ) {
        let dir = std::env::temp_dir()
            .join("pulse-rotation-tests")
            .join(uuid::Uuid::new_v4().to_string())
            .join("identity");
        let paths = IdentityPaths::new(dir);
        paths.ensure_dir().unwrap();
        let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_pem = ca.self_signed(&ca_key).unwrap().pem();
        store::write_atomic(&paths.ca(), ca_pem.as_bytes(), FileKind::Certificate).unwrap();
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut leaf = rcgen::CertificateParams::new(vec![AGENT.to_string()]).unwrap();
        let now = time::OffsetDateTime::now_utc();
        leaf.not_before = now + time::Duration::days(not_before_days);
        leaf.not_after = now + time::Duration::days(not_after_days);
        let issuer = rcgen::Issuer::from_params(&ca, &ca_key);
        let cert = leaf.signed_by(&key, &issuer).unwrap();
        store::write_atomic(
            &paths.client_key(),
            key.serialize_pem().as_bytes(),
            FileKind::Secret,
        )
        .unwrap();
        store::write_atomic(
            &paths.client_cert(),
            cert.pem().as_bytes(),
            FileKind::Certificate,
        )
        .unwrap();
        paths
            .save_enrollment(&store::EnrollmentFile {
                kind: "enroll".into(),
                state: store::state::ISSUED.into(),
                agent_id: AGENT.into(),
                ..Default::default()
            })
            .unwrap();
        (paths, ca_key, ca, ca_pem)
    }

    fn issue(
        paths: &IdentityPaths,
        ca_key: &rcgen::KeyPair,
        ca: &rcgen::CertificateParams,
        ca_pem: &str,
        san: &str,
    ) -> ke::IssuedCertificate {
        let key = keys::pkcs8_from_pem(&std::fs::read(paths.next_key()).unwrap()).unwrap();
        let kp = rcgen::KeyPair::try_from(key.as_slice()).unwrap();
        let issuer = rcgen::Issuer::from_params(ca, ca_key);
        let cert = rcgen::CertificateParams::new(vec![san.to_string()])
            .unwrap()
            .signed_by(&kp, &issuer)
            .unwrap();
        ke::IssuedCertificate {
            serial: "01".into(),
            not_before: String::new(),
            not_after: String::new(),
            client_cert_pem: cert.pem(),
            chain_pem: String::new(),
            ca_pem: ca_pem.to_string(),
        }
    }

    #[test]
    fn not_due_or_not_negotiated_sends_nothing() {
        let (paths, ..) = setup(-10, 355);
        let mut r = Rotation::new(paths.clone(), AGENT, 30);
        let now = chrono::Utc::now().timestamp_millis();
        r.on_hello_ack(false, &HelloAckExtra::default(), None);
        assert!(r.next_request(5, now).is_none(), "Kairos 不支援 renew");
        r.on_hello_ack(
            true,
            &HelloAckExtra {
                expired_renew_grace_days: Some(60),
                renew_now: None,
            },
            Some(0),
        );
        assert!(r.next_request(5, now).is_none(), "還沒到時點");
        let f = paths.load_enrollment().unwrap().unwrap();
        assert_eq!(
            f.state,
            store::state::REGISTERED,
            "hello_ack 後為 registered"
        );
        assert_eq!(f.grace_days, Some(60), "快取寬限天數");
    }

    #[test]
    fn due_rotation_sends_the_same_csr_until_issued_then_promotes() {
        let (paths, ca_key, ca, ca_pem) = setup(-300, 65);
        let mut r = Rotation::new(paths.clone(), AGENT, 30);
        r.on_hello_ack(true, &HelloAckExtra::default(), Some(0));
        let now = chrono::Utc::now().timestamp_millis();
        let first = r.next_request(5, now).expect("已用 2/3");
        assert!(r.next_request(6, now).is_none(), "在途時不重送");
        // 斷線後重送：同一個 request_id 與 CSR
        r.on_disconnect();
        r.on_hello_ack(true, &HelloAckExtra::default(), Some(0));
        let again = r.next_request(2, now).unwrap();
        assert_eq!(
            (&again.request_id, &again.csr),
            (&first.request_id, &first.csr)
        );
        // 需要人工核准：PLS-W1111，退避 1 小時起
        let line = format!(
            r#"{{"type":"error","seq":2,"code":"renew_requires_approval","message":"x","fatal":false}}"#
        );
        assert_eq!(r.on_line(&line), RotationLine::Handled);
        assert!(r.next_request(3, now).is_none(), "退避中");
        assert_eq!(r.backoff_current(), std::time::Duration::from_secs(7200));
        // 核准後以同一個 CSR 取得新憑證
        r.next_attempt = None;
        let req = r.next_request(4, now).unwrap();
        assert_eq!(req.csr, first.csr);
        let cert = issue(&paths, &ca_key, &ca, &ca_pem, AGENT);
        let line = serde_json::json!({"type":"renew_result","seq":4,"request_id":req.request_id,"state":"issued","certificate":cert}).to_string();
        assert_eq!(r.on_line(&line), RotationLine::Installed);
        assert!(paths.next_cert().exists());
        let old_pem = std::fs::read_to_string(paths.client_cert()).unwrap();
        promote(&paths).unwrap();
        assert_ne!(
            std::fs::read_to_string(paths.client_cert()).unwrap(),
            old_pem
        );
        assert!(!paths.next_key().exists() && !paths.next_cert().exists());
        let f = paths.load_enrollment().unwrap().unwrap();
        assert!(f.rotation.is_none());
        assert!(f.cert_serial.is_some());
    }

    #[test]
    fn rejects_a_renew_result_for_another_agent_or_ca() {
        let (paths, ca_key, ca, ca_pem) = setup(-300, 65);
        let mut r = Rotation::new(paths.clone(), AGENT, 30);
        r.on_hello_ack(true, &HelloAckExtra::default(), Some(0));
        let req = r
            .next_request(5, chrono::Utc::now().timestamp_millis())
            .unwrap();
        let cert = issue(
            &paths,
            &ca_key,
            &ca,
            &ca_pem,
            "11111111-2222-4333-8444-555555555555",
        );
        let line = serde_json::json!({"type":"renew_result","seq":5,"request_id":req.request_id,"state":"issued","certificate":cert}).to_string();
        assert_eq!(r.on_line(&line), RotationLine::Handled);
        assert!(!paths.next_cert().exists());
        // 不是目前申請的 renew_result：忽略
        let line = serde_json::json!({"type":"renew_result","seq":5,"request_id":"other","state":"issued","certificate":cert}).to_string();
        assert_eq!(r.on_line(&line), RotationLine::Handled);
        assert_eq!(
            r.on_line(r#"{"type":"ack","seq":9}"#),
            RotationLine::NotMine
        );
    }

    #[test]
    fn renew_now_starts_after_a_random_delay() {
        let (paths, ..) = setup(-10, 355);
        let mut r = Rotation::new(paths, AGENT, 30);
        r.on_hello_ack(
            true,
            &HelloAckExtra {
                expired_renew_grace_days: None,
                renew_now: Some(true),
            },
            Some(0),
        );
        let t = r.start_after.unwrap();
        assert!(t <= Instant::now() + schedule::RENEW_NOW_SPREAD);
        r.start_after = Some(Instant::now());
        assert!(
            r.next_request(1, chrono::Utc::now().timestamp_millis())
                .is_some()
        );
    }

    #[test]
    fn recover_completes_an_interrupted_promotion() {
        let (paths, ca_key, ca, ca_pem) = setup(-300, 65);
        let mut r = Rotation::new(paths.clone(), AGENT, 30);
        r.on_hello_ack(true, &HelloAckExtra::default(), Some(0));
        let req = r
            .next_request(5, chrono::Utc::now().timestamp_millis())
            .unwrap();
        let cert = issue(&paths, &ca_key, &ca, &ca_pem, AGENT);
        let line = serde_json::json!({"type":"renew_result","seq":5,"request_id":req.request_id,"state":"issued","certificate":cert}).to_string();
        assert_eq!(r.on_line(&line), RotationLine::Installed);
        // 中斷：只換了私鑰
        std::fs::rename(paths.next_key(), paths.client_key()).unwrap();
        recover(&paths);
        assert!(!paths.next_cert().exists());
        let key = keys::pkcs8_from_pem(&std::fs::read(paths.client_key()).unwrap()).unwrap();
        let pem = std::fs::read_to_string(paths.client_cert()).unwrap();
        let info = x509::parse_cert(&x509::pem_certs(&pem).unwrap()[0]).unwrap();
        assert_eq!(ke::p256_spki_of_pkcs8(&key).unwrap(), info.spki);
    }
}
