//! 到期預警（審計 O1）：伺服器憑證的到期日（notAfter）與 CRL 的下次更新時間（nextUpdate）。
//!
//! 何時檢查：啟動時、熱重載套用新設定時，以及之後定期檢查（熱重載的每個週期；停用熱重載時
//! 每 [`PERIODIC_CHECK_WITHOUT_RELOAD`]）。到期前 N 天（`server.cert_expiry_warning_days`、
//! `auth.crl_expiry_warning_days`）記 `WARN`，已過期記 `ERROR`。
//!
//! 這裡只負責預警，不改變任何驗證行為：CRL 超過 nextUpdate 時仍由 rustls 的
//! `enforce_revocation_expiration` 拒絕所有客戶端憑證（fail closed，見 `tls.rs`）。
//!
//! 日誌節流（K-S7）：每個檔案（每份 CRL）各自計時，與 `throttle.rs` 的類別完全分開，不共用額度，
//! 因此握手失敗、掃描等大量日誌無法掩蓋它。狀態改變（進入預警、過期、到期時間變更、恢復）以及
//! 啟動與重載時立即記錄，其餘每 [`REMIND_INTERVAL`] 最多提醒一次。
//!
//! 日期以手寫的最小 DER 解析取得（rustls-webpki 解析了這些欄位但沒有公開），只讀取需要的欄位；
//! 輸入格式不符時回傳 `None`，不會 panic。

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use log::{Level, log};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{MissedTickBehavior, interval};

use crate::shutdown::Shutdown;
use crate::tls::TlsState;

/// 進入預警後重複提醒的間隔
pub const REMIND_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// 停用熱重載時定期檢查的間隔
pub const PERIODIC_CHECK_WITHOUT_RELOAD: Duration = Duration::from_secs(60 * 60);

// ---- 最小 DER 解析 ----

const TAG_INTEGER: u8 = 0x02;
const TAG_SEQUENCE: u8 = 0x30;
const TAG_UTC_TIME: u8 = 0x17;
const TAG_GENERALIZED_TIME: u8 = 0x18;
/// `[0] EXPLICIT`（憑證的 version）
const TAG_CONTEXT_0: u8 = 0xA0;

/// 讀取一個 TLV，回傳 (tag, 內容, 其餘)。只支援單一 byte 的 tag 與最多 4 bytes 的長度（DER 憑證／CRL 足夠）
fn read_tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    if tag & 0x1F == 0x1F {
        // 多 byte 的 tag：這裡用不到
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7F) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | b as usize);
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    let (content, rest) = rest.split_at(len);
    Some((tag, content, rest))
}

/// 讀取一個指定 tag 的 TLV，回傳 (內容, 其餘)
fn expect(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (t, content, rest) = read_tlv(input)?;
    (t == tag).then_some((content, rest))
}

/// 兩位數字
fn two_digits(b: &[u8]) -> Option<u32> {
    match b {
        [a, c] if a.is_ascii_digit() && c.is_ascii_digit() => {
            Some(((a - b'0') * 10 + (c - b'0')) as u32)
        }
        _ => None,
    }
}

/// 解析 X.509 的時間：UTCTime `YYMMDDHHMMSSZ`（YY < 50 為 20YY，否則 19YY，RFC 5280 §4.1.2.5.1）
/// 或 GeneralizedTime `YYYYMMDDHHMMSSZ`（DER 不含小數秒）
fn parse_time(tag: u8, content: &[u8]) -> Option<DateTime<Utc>> {
    let (year, rest) = match tag {
        TAG_UTC_TIME if content.len() == 13 => {
            let yy = two_digits(&content[..2])? as i32;
            (if yy < 50 { 2000 + yy } else { 1900 + yy }, &content[2..])
        }
        TAG_GENERALIZED_TIME if content.len() == 15 => {
            let year = two_digits(&content[..2])? * 100 + two_digits(&content[2..4])?;
            (year as i32, &content[4..])
        }
        _ => return None,
    };
    if rest.last() != Some(&b'Z') {
        return None;
    }
    let field = |i: usize| two_digits(&rest[i * 2..i * 2 + 2]);
    let date = NaiveDate::from_ymd_opt(year, field(0)?, field(1)?)?;
    let time = NaiveTime::from_hms_opt(field(2)?, field(3)?, field(4)?)?;
    Some(NaiveDateTime::new(date, time).and_utc())
}

/// 讀取一個時間欄位，回傳 (時間, 其餘)
fn read_time(input: &[u8]) -> Option<(DateTime<Utc>, &[u8])> {
    let (tag, content, rest) = read_tlv(input)?;
    Some((parse_time(tag, content)?, rest))
}

/// 憑證（DER）的到期時間 notAfter：
/// `Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { [0] version OPTIONAL, serialNumber,
/// signature, issuer, validity SEQUENCE { notBefore, notAfter }, ... }, ... }`
pub fn cert_not_after(der: &[u8]) -> Option<DateTime<Utc>> {
    let (cert, _) = expect(der, TAG_SEQUENCE)?;
    let (tbs, _) = expect(cert, TAG_SEQUENCE)?;
    let tbs = match read_tlv(tbs)? {
        (TAG_CONTEXT_0, _, rest) => rest,
        _ => tbs,
    };
    let (_serial, rest) = expect(tbs, TAG_INTEGER)?;
    let (_signature, rest) = expect(rest, TAG_SEQUENCE)?;
    let (_issuer, rest) = expect(rest, TAG_SEQUENCE)?;
    let (validity, _) = expect(rest, TAG_SEQUENCE)?;
    let (_not_before, rest) = read_time(validity)?;
    let (not_after, _) = read_time(rest)?;
    Some(not_after)
}

/// CRL（DER）的下次更新時間 nextUpdate；CRL 沒有這個欄位時為 `None`：
/// `CertificateList ::= SEQUENCE { tbsCertList SEQUENCE { version INTEGER OPTIONAL, signature,
/// issuer, thisUpdate, nextUpdate OPTIONAL, ... }, ... }`
pub fn crl_next_update(der: &[u8]) -> Option<DateTime<Utc>> {
    let (list, _) = expect(der, TAG_SEQUENCE)?;
    let (tbs, _) = expect(list, TAG_SEQUENCE)?;
    let tbs = match read_tlv(tbs)? {
        (TAG_INTEGER, _, rest) => rest,
        _ => tbs,
    };
    let (_signature, rest) = expect(tbs, TAG_SEQUENCE)?;
    let (_issuer, rest) = expect(rest, TAG_SEQUENCE)?;
    let (_this_update, rest) = read_time(rest)?;
    let (next_update, _) = read_time(rest)?;
    Some(next_update)
}

// ---- 預警 ----

/// 有到期時間的項目種類
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpiryKind {
    /// 伺服器憑證（`server.cert_path` 的第一張憑證）
    ServerCert,
    /// CRL（`auth.crl`）
    Crl,
}

/// 一個有到期時間的項目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expiry {
    pub kind: ExpiryKind,
    /// 檔案路徑
    pub path: String,
    /// 同一個檔案中的第幾份（從 0 起算；PEM 檔可含多份 CRL）
    pub index: usize,
    /// 伺服器憑證的 notAfter，或 CRL 的 nextUpdate
    pub at: DateTime<Utc>,
}

/// 預警門檻（天）；0 表示不預警（已過期時仍記錄 ERROR）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpiryPolicy {
    pub cert_warning_days: u32,
    pub crl_warning_days: u32,
}

/// 一個項目目前的狀態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// 在預警期間內
    Soon,
    /// 已過期：伺服器憑證已過 notAfter，或 CRL 已過 nextUpdate（與 rustls 相同，`now >= nextUpdate` 即過期）
    Expired,
}

pub fn status(at: DateTime<Utc>, now: DateTime<Utc>, warning_days: u32) -> Status {
    if now >= at {
        Status::Expired
    } else if at - now <= chrono::Duration::days(warning_days as i64) {
        Status::Soon
    } else {
        Status::Ok
    }
}

/// 一則要記錄的日誌
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub level: Level,
    pub message: String,
}

/// 剩餘時間的說明，例如 `6 天 23 小時`、`5 小時 3 分鐘`
fn describe_remaining(d: chrono::Duration) -> String {
    let minutes = d.num_minutes().max(0);
    let (days, hours, mins) = (minutes / 1440, minutes % 1440 / 60, minutes % 60);
    if days > 0 {
        format!("{} 天 {} 小時", days, hours)
    } else if hours > 0 {
        format!("{} 小時 {} 分鐘", hours, mins)
    } else {
        format!("{} 分鐘", mins)
    }
}

fn fmt_at(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

/// 檔案名稱，多份 CRL 時附上第幾份
fn describe_item(item: &Expiry) -> String {
    if item.index == 0 {
        item.path.clone()
    } else {
        format!("{}（第 {} 份）", item.path, item.index + 1)
    }
}

fn notice(item: &Expiry, status: Status, now: DateTime<Utc>, policy: &ExpiryPolicy) -> Notice {
    let name = describe_item(item);
    let at = fmt_at(item.at);
    let remaining = describe_remaining(item.at - now);
    let (level, message) = match (item.kind, status) {
        (ExpiryKind::Crl, Status::Expired) => (
            Level::Error,
            format!(
                "CRL {} 已過期：下次更新時間（nextUpdate）{} 已過，Kairos 無法判定撤銷狀態，所有 agent 都會被拒絕連線（fail closed）。請立即在管理主機重新產生 CRL 並部署（見 INSTALL 第 6.4、10.1 節），Kairos 會在下一個重載週期自動載入",
                name, at
            ),
        ),
        (ExpiryKind::Crl, _) => (
            Level::Warn,
            format!(
                "CRL {} 將在 {} 後過期（下次更新時間 nextUpdate：{}）；過期後所有 agent 都會被拒絕連線。請在管理主機重新產生 CRL 並部署（見 INSTALL 第 6.4、10.1 節），或確認 kairos-crl.timer 是否正常執行（預警門檻 auth.crl_expiry_warning_days = {} 天）",
                name, remaining, at, policy.crl_warning_days
            ),
        ),
        (ExpiryKind::ServerCert, Status::Expired) => (
            Level::Error,
            format!(
                "伺服器憑證 {} 已於 {} 過期，Pulse 會拒絕連線。請立即重新簽發並以 mv 覆寫（見 INSTALL 第 10.1 節），不需重新啟動",
                name, at
            ),
        ),
        (ExpiryKind::ServerCert, _) => (
            Level::Warn,
            format!(
                "伺服器憑證 {} 將在 {} 後到期（{}）；到期後 Pulse 會拒絕連線。請重新簽發並以 mv 覆寫（見 INSTALL 第 10.1 節），不需重新啟動（預警門檻 server.cert_expiry_warning_days = {} 天）",
                name, remaining, at, policy.cert_warning_days
            ),
        ),
    };
    Notice { level, message }
}

fn recovered_notice(item: &Expiry) -> Notice {
    let what = match item.kind {
        ExpiryKind::Crl => "CRL",
        ExpiryKind::ServerCert => "伺服器憑證",
    };
    Notice {
        level: Level::Info,
        message: format!(
            "{} {} 的到期時間已更新為 {}，解除預警",
            what,
            describe_item(item),
            fmt_at(item.at)
        ),
    }
}

/// 已記錄的預警狀態
#[derive(Debug, Clone)]
struct Logged {
    status: Status,
    at: DateTime<Utc>,
    logged_at: Instant,
}

/// 到期預警的狀態（每個項目各自計時）
#[derive(Debug)]
pub struct ExpiryWarner {
    policy: ExpiryPolicy,
    remind_every: Duration,
    logged: HashMap<(ExpiryKind, String, usize), Logged>,
}

impl ExpiryWarner {
    pub fn new(policy: ExpiryPolicy) -> Self {
        Self {
            policy,
            remind_every: REMIND_INTERVAL,
            logged: HashMap::new(),
        }
    }

    fn warning_days(&self, kind: ExpiryKind) -> u32 {
        match kind {
            ExpiryKind::ServerCert => self.policy.cert_warning_days,
            ExpiryKind::Crl => self.policy.crl_warning_days,
        }
    }

    /// 檢查目前設定中的項目，回傳應記錄的日誌。`force`（啟動與重載時）為真時，在預警期間內或已過期的
    /// 項目一律記錄；否則只在狀態改變、到期時間改變或距上次記錄超過 [`REMIND_INTERVAL`] 時記錄。
    pub fn check(
        &mut self,
        items: &[Expiry],
        now: DateTime<Utc>,
        mono: Instant,
        force: bool,
    ) -> Vec<Notice> {
        let mut notices = Vec::new();
        let mut seen = Vec::with_capacity(items.len());
        for item in items {
            let key = (item.kind, item.path.clone(), item.index);
            seen.push(key.clone());
            let status = status(item.at, now, self.warning_days(item.kind));
            if status == Status::Ok {
                if self.logged.remove(&key).is_some() {
                    notices.push(recovered_notice(item));
                }
                continue;
            }
            let due = match self.logged.get(&key) {
                None => true,
                Some(prev) => {
                    force
                        || prev.status != status
                        || prev.at != item.at
                        || mono.saturating_duration_since(prev.logged_at) >= self.remind_every
                }
            };
            if due {
                notices.push(notice(item, status, now, &self.policy));
                self.logged.insert(
                    key,
                    Logged {
                        status,
                        at: item.at,
                        logged_at: mono,
                    },
                );
            }
        }
        // 已不在設定中的項目（例如重載後檔案中的 CRL 變少）不再追蹤
        self.logged.retain(|k, _| seen.contains(k));
        notices
    }

    /// [`ExpiryWarner::check`] 並記錄日誌
    pub fn check_and_log(&mut self, items: &[Expiry], force: bool) {
        for n in self.check(items, Utc::now(), Instant::now(), force) {
            log!(n.level, "{}", n.message);
        }
    }
}

/// 停用熱重載時的定期檢查（啟用熱重載時由重載工作在每個週期檢查）
pub async fn run(
    mut warner: ExpiryWarner,
    state: Arc<TlsState>,
    period: Duration,
    shutdown: Arc<Shutdown>,
) {
    let mut ticker = interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 第一次 tick 立即完成；啟動時已檢查過，跳過
    ticker.tick().await;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        warner.check_and_log(&state.current().expiries, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tokio_rustls::rustls::pki_types::CertificateDer;
    use tokio_rustls::rustls::pki_types::pem::PemObject;

    const PKI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/pki/");

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    fn cert_der(path: &str) -> Vec<u8> {
        CertificateDer::from_pem_file(path).unwrap().to_vec()
    }

    #[test]
    fn parses_certificate_not_after() {
        // openssl x509 -enddate：GeneralizedTime（2050 年以後）
        assert_eq!(
            cert_not_after(&cert_der(&format!("{PKI}server.crt"))),
            Some(utc(2126, 9, 14, 20, 51, 35))
        );
        // UTCTime（client-expired.crt：2021-01-01 到期）
        let expired = cert_not_after(&cert_der(&format!("{PKI}client-expired.crt"))).unwrap();
        assert_eq!(
            expired.date_naive(),
            NaiveDate::from_ymd_opt(2021, 1, 1).unwrap()
        );
    }

    #[test]
    fn parses_crl_next_update() {
        let der = std::fs::read(format!("{PKI}crl-revoke-a.der")).unwrap();
        assert_eq!(crl_next_update(&der), Some(utc(2126, 9, 14, 20, 51, 35)));
        // PEM 版本內容相同
        let pem = crate::tls::load_crl_file(&format!("{PKI}crl-empty.crl")).unwrap();
        assert_eq!(
            crl_next_update(pem[0].as_ref()),
            Some(utc(2126, 9, 14, 20, 51, 35))
        );
    }

    #[test]
    fn malformed_input_returns_none_without_panic() {
        let der = std::fs::read(format!("{PKI}crl-revoke-a.der")).unwrap();
        // 每一種截斷都不 panic
        for n in 0..der.len() {
            let _ = crl_next_update(&der[..n]);
            let _ = cert_not_after(&der[..n]);
        }
        for bad in [
            &b""[..],
            b"\x30",
            b"\x30\x84\xff\xff\xff\xff",
            b"\x30\x85\x01\x01\x01\x01\x01",
            b"\x1f\x00",
            b"\x30\x03\x02\x01\x00",
        ] {
            assert_eq!(crl_next_update(bad), None);
            assert_eq!(cert_not_after(bad), None);
        }
        // 憑證不是 CRL、CRL 不是憑證
        assert_eq!(cert_not_after(&der), None);
    }

    #[test]
    fn parses_time_formats() {
        assert_eq!(
            parse_time(TAG_UTC_TIME, b"491231235959Z"),
            Some(utc(2049, 12, 31, 23, 59, 59))
        );
        assert_eq!(
            parse_time(TAG_UTC_TIME, b"500101000000Z"),
            Some(utc(1950, 1, 1, 0, 0, 0))
        );
        assert_eq!(
            parse_time(TAG_GENERALIZED_TIME, b"20261014010000Z"),
            Some(utc(2026, 10, 14, 1, 0, 0))
        );
        for (tag, bad) in [
            (TAG_UTC_TIME, &b"4912312359Z"[..]),
            (TAG_UTC_TIME, b"491231235959+"),
            (TAG_UTC_TIME, b"491331235959Z"),
            (TAG_UTC_TIME, b"49123123595aZ"),
            (TAG_GENERALIZED_TIME, b"20261014010000.5Z"),
            (TAG_GENERALIZED_TIME, b"20260230010000Z"),
            (TAG_INTEGER, b"20261014010000Z"),
        ] {
            assert_eq!(parse_time(tag, bad), None, "{bad:?}");
        }
    }

    #[test]
    fn status_thresholds() {
        let at = utc(2026, 10, 20, 0, 0, 0);
        assert_eq!(status(at, utc(2026, 10, 12, 0, 0, 0), 7), Status::Ok);
        assert_eq!(status(at, utc(2026, 10, 13, 0, 0, 0), 7), Status::Soon);
        assert_eq!(status(at, utc(2026, 10, 19, 23, 59, 59), 7), Status::Soon);
        // 與 rustls 相同：now >= nextUpdate 即過期
        assert_eq!(status(at, at, 7), Status::Expired);
        // 0 天：不預警，但過期仍是過期
        assert_eq!(status(at, utc(2026, 10, 19, 23, 0, 0), 0), Status::Ok);
        assert_eq!(status(at, utc(2026, 10, 21, 0, 0, 0), 0), Status::Expired);
    }

    #[test]
    fn remaining_is_human_readable() {
        assert_eq!(
            describe_remaining(chrono::Duration::hours(24 * 6 + 23)),
            "6 天 23 小時"
        );
        assert_eq!(
            describe_remaining(chrono::Duration::minutes(303)),
            "5 小時 3 分鐘"
        );
        assert_eq!(describe_remaining(chrono::Duration::seconds(59)), "0 分鐘");
    }

    fn policy() -> ExpiryPolicy {
        ExpiryPolicy {
            cert_warning_days: 30,
            crl_warning_days: 7,
        }
    }

    fn crl(at: DateTime<Utc>) -> Expiry {
        Expiry {
            kind: ExpiryKind::Crl,
            path: "agents.crl".into(),
            index: 0,
            at,
        }
    }

    fn cert(at: DateTime<Utc>) -> Expiry {
        Expiry {
            kind: ExpiryKind::ServerCert,
            path: "cert.pem".into(),
            index: 0,
            at,
        }
    }

    #[test]
    fn warner_logs_on_state_changes_and_reminds_daily() {
        let mut w = ExpiryWarner::new(policy());
        let t0 = Instant::now();
        let now = utc(2026, 10, 12, 0, 0, 0);
        let next = utc(2026, 10, 17, 1, 0, 0);
        let items = [crl(next), cert(utc(2027, 1, 1, 0, 0, 0))];

        // 啟動：CRL 剩 5 天 → WARN；憑證還很久 → 不記
        let n = w.check(&items, now, t0, true);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].level, Level::Warn);
        assert!(
            n[0].message
                .contains("CRL agents.crl 將在 5 天 1 小時 後過期"),
            "{}",
            n[0].message
        );
        assert!(
            n[0].message.contains("2026-10-17 01:00:00 UTC"),
            "{}",
            n[0].message
        );
        assert!(
            n[0].message.contains("auth.crl_expiry_warning_days = 7"),
            "{}",
            n[0].message
        );

        // 定期檢查：24 小時內不重複
        let later = t0 + Duration::from_secs(3600);
        assert!(w.check(&items, now, later, false).is_empty());
        // 重載時（force）：再記一次
        assert_eq!(w.check(&items, now, later, true).len(), 1);
        // 24 小時後提醒一次
        let day = later + REMIND_INTERVAL;
        assert_eq!(w.check(&items, now, day, false).len(), 1);
        assert!(w.check(&items, now, day, false).is_empty());

        // 過期：狀態改變立即記 ERROR
        let n = w.check(&items, next, day, false);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].level, Level::Error);
        assert!(
            n[0].message.contains("所有 agent 都會被拒絕連線"),
            "{}",
            n[0].message
        );

        // 更新 CRL 後恢復：記 INFO 一次，之後不再記
        let renewed = [crl(utc(2026, 11, 17, 0, 0, 0)), items[1].clone()];
        let n = w.check(&renewed, next, day, true);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].level, Level::Info);
        assert!(w.check(&renewed, next, day, true).is_empty());
    }

    #[test]
    fn warner_logs_when_expiry_changes_and_handles_server_certificates() {
        let mut w = ExpiryWarner::new(policy());
        let t0 = Instant::now();
        let now = utc(2026, 10, 12, 0, 0, 0);
        // 伺服器憑證剩 20 天（門檻 30 天）
        let n = w.check(&[cert(utc(2026, 11, 1, 0, 0, 0))], now, t0, false);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].level, Level::Warn);
        assert!(
            n[0].message
                .contains("server.cert_expiry_warning_days = 30"),
            "{}",
            n[0].message
        );
        // 換了一張仍在預警期間內的憑證：到期時間不同，立即記錄
        assert_eq!(
            w.check(&[cert(utc(2026, 11, 2, 0, 0, 0))], now, t0, false)
                .len(),
            1
        );
        // 已過期的伺服器憑證：ERROR
        let n = w.check(&[cert(utc(2026, 10, 1, 0, 0, 0))], now, t0, false);
        assert_eq!(n[0].level, Level::Error);
        // 門檻 0：不預警，但過期仍記錄
        let mut w = ExpiryWarner::new(ExpiryPolicy {
            cert_warning_days: 0,
            crl_warning_days: 0,
        });
        assert!(
            w.check(&[crl(utc(2026, 10, 12, 1, 0, 0))], now, t0, true)
                .is_empty()
        );
        assert_eq!(
            w.check(&[crl(utc(2026, 10, 11, 0, 0, 0))], now, t0, true)[0].level,
            Level::Error
        );
    }

    #[test]
    fn multiple_crls_in_one_file_are_tracked_separately() {
        let mut w = ExpiryWarner::new(policy());
        let t0 = Instant::now();
        let now = utc(2026, 10, 12, 0, 0, 0);
        let mut second = crl(utc(2026, 10, 14, 0, 0, 0));
        second.index = 1;
        let items = [crl(utc(2026, 10, 15, 0, 0, 0)), second];
        let n = w.check(&items, now, t0, false);
        assert_eq!(n.len(), 2);
        assert!(
            n[1].message.contains("agents.crl（第 2 份）"),
            "{}",
            n[1].message
        );
        // 第二份消失後不再追蹤；之後再出現視為新項目
        assert!(w.check(&items[..1], now, t0, false).is_empty());
        assert_eq!(w.check(&items, now, t0, false).len(), 1);
    }
}
