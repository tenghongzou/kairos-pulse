//! TLS 設定（rustls）。
//!
//! - 信任：作業系統信任庫（rustls-native-certs，`server.use_system_roots = false` 時不載入）
//!   + `server.ca_cert`（可含多張）。
//! - 客戶端憑證（mTLS）：`server.client_cert`（可含中繼 CA 鏈）+ `server.client_key`
//!   （PKCS#8 / PKCS#1 / SEC1，RSA / ECDSA / Ed25519，不可加密）。金鑰與憑證不成對時在啟動時即報錯。
//!   私鑰檔案權限過寬（Unix 的群組／其他使用者權限、Windows 的 ACL）時記 warn，規則見
//!   [`crate::file_perms`]。
//!   憑證有效期限在啟動時與之後每 24 小時檢查一次（[`TlsSetup::log_client_cert_validity`]）：
//!   已過期記 error（仍會嘗試連線，以伺服器判定為準：可能只是本機時鐘不正確）、
//!   尚未生效或 30 天內到期記 warn。
//! - `accept_invalid_certs = true`：只略過伺服器憑證的信任鏈與名稱驗證，握手簽章仍會正確驗證。
//!   啟動時以 error 等級警告（`log.level` 設為 error 也看得到），每次連線成功的日誌也會標註。
//! - 握手失敗時把 rustls 錯誤轉成具體的處置提示（[`describe_tls_error`]）。

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use log::Level;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::client::{ResolvesClientCert, WebPkiServerVerifier};
use tokio_rustls::rustls::crypto::{self, CryptoProvider};
use tokio_rustls::rustls::pki_types::pem::{self, PemObject};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio_rustls::rustls::sign::CertifiedKey;
use tokio_rustls::rustls::{
    AlertDescription, CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError,
    InconsistentKeys, RootCertStore, SignatureScheme,
};

use crate::config_manager::ServerConfig;

/// 客戶端憑證剩餘有效期限少於此值時記 warn。
const CERT_EXPIRY_WARNING: Duration = Duration::from_secs(30 * 24 * 3600);

/// 啟動後重新檢查客戶端憑證有效期限的間隔。
pub const CERT_RECHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// 一天的秒數（換算剩餘天數用）。
const SECS_PER_DAY: u64 = 24 * 3600;

/// 客戶端憑證有效期限的檢查結果（要記錄的等級與訊息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertFinding {
    pub level: Level,
    pub message: String,
}

impl CertFinding {
    fn error(message: String) -> Self {
        Self {
            level: Level::Error,
            message,
        }
    }

    fn warn(message: String) -> Self {
        Self {
            level: Level::Warn,
            message,
        }
    }
}

/// 啟動時建立一次、所有連線共用的 TLS 設定。
#[derive(Clone)]
pub struct TlsSetup {
    pub connector: TlsConnector,
    /// SNI 與伺服器憑證名稱驗證用的名稱（DNS 名稱或 IP）
    pub server_name: ServerName<'static>,
    /// 伺服器是否在最近一次握手中要求客戶端憑證（見 [`RecordingClientCertResolver`]）
    client_cert_requested: Arc<AtomicBool>,
    /// 客戶端憑證的路徑與整條憑證鏈（第一張為葉憑證；用於檢查 SAN 與有效期限）
    client_chain: Option<(String, Vec<CertificateDer<'static>>)>,
    /// 是否驗證伺服器憑證（`accept_invalid_certs = true` 時為 false）
    verifies_server_cert: bool,
}

impl TlsSetup {
    /// 每次連線前呼叫：清除「伺服器要求客戶端憑證」的紀錄。
    ///
    /// Pulse 同一時間只有一條連線，因此共用一個旗標即可。
    pub fn reset_client_cert_request(&self) {
        self.client_cert_requested.store(false, Ordering::Relaxed);
    }

    /// 伺服器是否在本次連線的握手中要求了客戶端憑證（送出 CertificateRequest）。
    pub fn client_cert_was_requested(&self) -> bool {
        self.client_cert_requested.load(Ordering::Relaxed)
    }

    /// 是否驗證伺服器憑證的信任鏈與名稱（`accept_invalid_certs = true` 時為 false）。
    pub fn verifies_server_cert(&self) -> bool {
        self.verifies_server_cert
    }

    /// 確認客戶端憑證的 SAN 含 `DNS:<agent_id>`（伺服器以此比對 hello 的 agent_id，不分大小寫）。
    ///
    /// 未設定客戶端憑證時直接通過。不符時回傳具體的錯誤訊息，讓程式在啟動時就結束，
    /// 而不是連上伺服器後才收到 `auth_failed`。
    pub fn verify_client_cert_identity(&self, agent_id: &str) -> Result<(), String> {
        let Some((path, chain)) = &self.client_chain else {
            return Ok(());
        };
        check_cert_san(path, &chain[0], agent_id)
    }

    /// 是否設定了客戶端憑證。
    pub fn has_client_cert(&self) -> bool {
        self.client_chain.is_some()
    }

    /// 以 `now` 檢查客戶端憑證鏈的有效期限；未設定客戶端憑證時為空。
    pub fn client_cert_validity(&self, now: DateTime<Utc>) -> Vec<CertFinding> {
        match &self.client_chain {
            Some((path, chain)) => check_chain_validity(path, chain, now),
            None => Vec::new(),
        }
    }

    /// 以目前時間檢查客戶端憑證鏈的有效期限並記錄結果（啟動時與之後每 24 小時呼叫）。
    pub fn log_client_cert_validity(&self) {
        log_findings(&self.client_cert_validity(Utc::now()));
    }
}

/// 依各自的等級記錄檢查結果；需要人工處理的（本機客戶端憑證已過期，見 [`mirrors_to_stderr`]）
/// 同時寫一行到 stderr，讓 systemd 的 journal 也看得到。
fn log_findings(findings: &[CertFinding]) {
    for finding in findings {
        log::log!(finding.level, "{}", finding.message);
        if mirrors_to_stderr(finding) {
            crate::service_status::attention(&finding.message);
        }
    }
}

/// 有效期限檢查結果是否也要寫到 stderr：只有 error（憑證已過期）。
///
/// 檢查只在啟動時與之後每 `CERT_RECHECK_INTERVAL`（24 小時）執行一次，
/// 頻率本身就遠低於 10 分鐘的節流視窗，不另外節流。
fn mirrors_to_stderr(finding: &CertFinding) -> bool {
    finding.level == Level::Error
}

/// 依設定建立 TLS 設定；任何問題都回傳具體的錯誤訊息（啟動時即結束程式）。
pub fn build(server: &ServerConfig) -> Result<TlsSetup, String> {
    let domain = server.domain.trim();
    let server_name = ServerName::try_from(domain.to_string())
        .map_err(|e| format!("server.domain「{domain}」不是有效的 DNS 名稱或 IP 位址：{e}"))?;

    let provider = Arc::new(crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("無法建立 TLS 設定：{e}"))?;

    let builder = if server.accept_invalid_certs {
        // ⚠️ 僅供開發測試，正式環境不應開啟。以 error 等級記錄：log.level 最高只能設為 error，
        // 因此不論設定為何都一定會出現在終端與日誌檔
        if !server.ca_cert.trim().is_empty() {
            log::error!(
                "同時設定了 server.ca_cert 與 accept_invalid_certs = true：憑證驗證已停用，ca_cert 形同無效"
            );
        }
        log::error!(
            "已停用 TLS 伺服器憑證驗證（accept_invalid_certs = true）：無法確認連到的是真正的伺服器，\
             連線可能遭中間人竊聽或竄改，請勿用於正式環境"
        );
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert {
                provider: provider.clone(),
            }))
    } else {
        let roots = load_roots(server.ca_cert.trim(), server.use_system_roots)?;
        let verifier =
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .build()
                .map_err(|e| format!("無法建立伺服器憑證驗證器：{e}"))?;
        builder.with_webpki_verifier(verifier)
    };

    let cert_path = server.client_cert.trim();
    let key_path = server.client_key.trim();
    let (client_key, client_chain) = if cert_path.is_empty() || key_path.is_empty() {
        (None, None)
    } else {
        let (chain, key) = load_client_cert(cert_path, key_path)?;
        // 私鑰權限過寬只記 warn、不阻止啟動（與 agent.id 的策略一致）
        if let Some(warning) = crate::file_perms::permission_warning(
            std::path::Path::new(key_path),
            crate::file_perms::CLIENT_KEY_FILE,
        ) {
            log::warn!("{warning}");
        }
        // 已過期也不阻止啟動（記 error）：本機時鐘可能不正確，以伺服器的判定為準
        log_findings(&check_chain_validity(cert_path, &chain, Utc::now()));
        if let Some(warning) = multiple_dns_san_warning(cert_path, &chain[0]) {
            log::warn!("{warning}");
        }
        let count = chain.len();
        // from_der 會檢查私鑰與憑證是否成對（keys_match）
        let certified = CertifiedKey::from_der(chain.clone(), key, &provider)
            .map_err(|e| client_cert_error(cert_path, key_path, &e))?;
        log::info!("已載入客戶端憑證：{cert_path}（憑證鏈 {count} 張；私鑰：{key_path}）");
        (
            Some(Arc::new(certified)),
            Some((cert_path.to_string(), chain)),
        )
    };
    let client_cert_requested = Arc::new(AtomicBool::new(false));
    let config = builder.with_client_cert_resolver(Arc::new(RecordingClientCertResolver {
        key: client_key,
        requested: client_cert_requested.clone(),
    }));

    Ok(TlsSetup {
        connector: TlsConnector::from(Arc::new(config)),
        server_name,
        client_cert_requested,
        client_chain,
        verifies_server_cert: !server.accept_invalid_certs,
    })
}

/// 客戶端憑證解析器：行為與 rustls 的 `SingleCertAndKey` / 無客戶端憑證相同，
/// 另外記錄伺服器是否送出 CertificateRequest。
///
/// TLS 1.3 下伺服器在握手「之後」才驗證客戶端憑證，拒絕時送出的 alert 常因連線被重設
/// 而收不到；藉由這個紀錄，即使沒收到 alert 也能判斷「伺服器要求了客戶端憑證」。
#[derive(Debug)]
struct RecordingClientCertResolver {
    key: Option<Arc<CertifiedKey>>,
    requested: Arc<AtomicBool>,
}

impl ResolvesClientCert for RecordingClientCertResolver {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        self.requested.store(true, Ordering::Relaxed);
        // 簽章演算法的選擇由 rustls 依 sigschemes 處理（與 SingleCertAndKey 相同）
        self.key.clone()
    }

    fn has_certs(&self) -> bool {
        self.key.is_some()
    }
}

/// 信任庫：作業系統信任庫（`use_system_roots` 為 true 時）+ `ca_cert`（可含多張）。
///
/// 系統信任庫中個別憑證載入失敗只記 warn；兩者合計沒有任何可用憑證時回傳錯誤。
fn load_roots(ca_path: &str, use_system_roots: bool) -> Result<RootCertStore, String> {
    let mut roots = RootCertStore::empty();

    if use_system_roots {
        let native = rustls_native_certs::load_native_certs();
        for e in &native.errors {
            log::warn!("載入作業系統信任庫時發生錯誤（略過）：{e}");
        }
        let (added, ignored) = roots.add_parsable_certificates(native.certs);
        if ignored > 0 {
            log::warn!("作業系統信任庫中有 {ignored} 張憑證無法使用，已略過");
        }
        log::debug!("已從作業系統信任庫載入 {added} 張憑證");
    } else {
        log::info!("server.use_system_roots = false：不信任作業系統信任庫，只信任 server.ca_cert");
    }

    if !ca_path.is_empty() {
        let certs = load_certs(ca_path, "server.ca_cert", "CA 憑證")?;
        let count = certs.len();
        for cert in certs {
            roots
                .add(cert)
                .map_err(|e| format!("server.ca_cert「{ca_path}」中的憑證無法作為信任錨：{e}"))?;
        }
        log::info!("已載入額外信任的憑證：{ca_path}（{count} 張）");
    }

    if roots.is_empty() {
        return Err(if use_system_roots {
            "沒有任何可信任的憑證：作業系統信任庫無法載入，且未設定 server.ca_cert".to_string()
        } else {
            "沒有任何可信任的憑證：server.use_system_roots = false 且未設定 server.ca_cert"
                .to_string()
        });
    }
    Ok(roots)
}

/// 讀取 PEM 檔中的所有憑證；一張都沒有時回傳錯誤。
fn load_certs(path: &str, field: &str, item: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|e| pem_error(path, field, item, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| pem_error(path, field, item, e))?;
    if certs.is_empty() {
        return Err(format!(
            "{field}「{path}」中找不到{item}（需要 -----BEGIN CERTIFICATE----- 區塊）"
        ));
    }
    Ok(certs)
}

/// 讀取客戶端憑證鏈（全部憑證，第一張為客戶端憑證本身）與私鑰。
fn load_client_cert(
    cert_path: &str,
    key_path: &str,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String> {
    let chain = load_certs(cert_path, "server.client_cert", "客戶端憑證")?;
    let pem = std::fs::read(key_path)
        .map_err(|e| pem_error(key_path, "server.client_key", "私鑰", pem::Error::Io(e)))?;
    // 加密私鑰（BEGIN ENCRYPTED PRIVATE KEY，或傳統格式的 Proc-Type: 4,ENCRYPTED）
    // 會被 PEM 解析器略過或解成無效金鑰，只會得到「找不到私鑰」之類的誤導訊息，因此先行偵測
    if is_encrypted_key_pem(&pem) {
        return Err(format!(
            "server.client_key「{key_path}」是加密的私鑰，Pulse 不支援加密私鑰：\
             請以 `openssl pkey -in <加密私鑰> -out <新檔案>` 解密後使用，並限制該檔案只有執行帳號可讀"
        ));
    }
    // 讀取第一把私鑰，接受 PKCS#8、PKCS#1（RSA）與 SEC1（EC）格式
    let key = PrivateKeyDer::from_pem_slice(&pem)
        .map_err(|e| pem_error(key_path, "server.client_key", "私鑰", e))?;
    Ok((chain, key))
}

/// PEM 內容是否為加密私鑰：PKCS#8 的 `ENCRYPTED PRIVATE KEY` 區塊，
/// 或 OpenSSL 傳統格式（`BEGIN RSA/EC PRIVATE KEY` 加上 `Proc-Type: 4,ENCRYPTED` 標頭）。
fn is_encrypted_key_pem(pem: &[u8]) -> bool {
    let contains = |needle: &[u8]| pem.windows(needle.len()).any(|w| w == needle);
    contains(b"-----BEGIN ENCRYPTED PRIVATE KEY-----") || contains(b"Proc-Type: 4,ENCRYPTED")
}

/// 檢查客戶端憑證鏈中每張憑證的有效期限（以 `now` 為準）。不阻止啟動，只回傳要記錄的結果：
///
/// - 已過期 → error（伺服器多半會拒絕；但也可能是本機時鐘超前，因此仍嘗試連線，以伺服器判定為準）
/// - 尚未生效、或 30 天內到期 → warn
/// - 無法解析有效期限 → warn（實際驗證交給伺服器）
fn check_chain_validity(
    path: &str,
    chain: &[CertificateDer<'_>],
    now: DateTime<Utc>,
) -> Vec<CertFinding> {
    let fmt = |t: DateTime<Utc>| t.format("%Y-%m-%d %H:%M:%S UTC").to_string();
    let mut findings = Vec::new();
    for (i, cert) in chain.iter().enumerate() {
        let label = if i == 0 {
            "客戶端憑證".to_string()
        } else {
            format!("憑證鏈第 {} 張（中繼 CA）", i + 1)
        };
        let Some((not_before, not_after)) = cert_validity(cert) else {
            findings.push(CertFinding::warn(format!(
                "無法解析 server.client_cert「{path}」中{label}的有效期限，略過到期檢查"
            )));
            continue;
        };
        if now > not_after {
            findings.push(CertFinding::error(format!(
                "server.client_cert「{path}」中的{label}已於 {} 過期（本機時間 {}），伺服器多半會拒絕連線：\
                 請聯絡管理者重新簽發；若憑證其實仍有效，請檢查本機時鐘是否正確\
                 （Pulse 仍會繼續嘗試連線，以伺服器的判定為準）",
                fmt(not_after),
                fmt(now)
            )));
            continue;
        }
        if now < not_before {
            findings.push(CertFinding::warn(format!(
                "server.client_cert「{path}」中的{label}要到 {} 才生效（本機時間 {}）：伺服器會拒絕連線，請確認本機時鐘",
                fmt(not_before),
                fmt(now)
            )));
        }
        let remaining = (not_after - now).to_std().unwrap_or_default();
        if remaining < CERT_EXPIRY_WARNING {
            findings.push(CertFinding::warn(format!(
                "server.client_cert「{path}」中的{label}將於 {} 到期（剩餘不超過 {} 天），請儘快聯絡管理者重新簽發",
                fmt(not_after),
                remaining_days(remaining)
            )));
        }
    }
    findings
}

/// 剩餘時間換算成天數（無條件進位，至少 1）：剩 9.5 天 → 10，剛好 10 天 → 10。
fn remaining_days(remaining: Duration) -> u64 {
    remaining.as_secs().div_ceil(SECS_PER_DAY).max(1)
}

/// 從 X.509 憑證 DER 取出 `(notBefore, notAfter)`；格式不符時回傳 `None`（不 panic）。
///
/// ```text
/// Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { [0] version OPTIONAL,
///   serialNumber INTEGER, signature AlgorithmIdentifier, issuer Name,
///   validity SEQUENCE { notBefore Time, notAfter Time }, ... }, ... }
/// ```
fn cert_validity(cert: &CertificateDer<'_>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    const SEQUENCE: u8 = 0x30;
    const INTEGER: u8 = 0x02;
    const VERSION: u8 = 0xa0;
    let (cert, _) = der_expect(cert.as_ref(), SEQUENCE)?;
    let (tbs, _) = der_expect(cert, SEQUENCE)?;
    let (tag, _, rest) = der_next(tbs)?;
    let rest = match tag {
        VERSION => der_expect(rest, INTEGER)?.1, // 略過 serialNumber
        INTEGER => rest,                         // 沒有 version（v1 憑證）
        _ => return None,
    };
    let (_, rest) = der_expect(rest, SEQUENCE)?; // signature
    let (_, rest) = der_expect(rest, SEQUENCE)?; // issuer
    let (validity, _) = der_expect(rest, SEQUENCE)?;
    let (tag, not_before, rest) = der_next(validity)?;
    let not_before = der_time(tag, not_before)?;
    let (tag, not_after, _) = der_next(rest)?;
    let not_after = der_time(tag, not_after)?;
    Some((not_before, not_after))
}

/// 讀取一個 DER TLV：回傳 `(tag, 內容, 其後剩餘)`。只支援單一位元組的 tag 與至多 4 位元組的長度。
fn der_next(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let (bytes, rest) = rest.split_at(n);
        let len = bytes
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        (len, rest)
    };
    if rest.len() < len {
        return None;
    }
    let (content, rest) = rest.split_at(len);
    Some((tag, content, rest))
}

/// 同 [`der_next`]，但 tag 必須是 `tag`；回傳 `(內容, 其後剩餘)`。
fn der_expect(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (t, content, rest) = der_next(input)?;
    (t == tag).then_some((content, rest))
}

/// 解析 UTCTime（`YYMMDDHHMMSSZ`，YY < 50 為 20YY）或 GeneralizedTime（`YYYYMMDDHHMMSSZ`）。
fn der_time(tag: u8, content: &[u8]) -> Option<DateTime<Utc>> {
    const UTC_TIME: u8 = 0x17;
    const GENERALIZED_TIME: u8 = 0x18;
    let digits = std::str::from_utf8(content).ok()?.strip_suffix('Z')?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num = |s: &str| s.parse::<u32>().ok();
    let (year, rest) = match (tag, digits.len()) {
        (UTC_TIME, 12) => {
            let yy = i32::try_from(num(&digits[..2])?).ok()?;
            (if yy < 50 { 2000 + yy } else { 1900 + yy }, &digits[2..])
        }
        (GENERALIZED_TIME, 14) => (i32::try_from(num(&digits[..4])?).ok()?, &digits[4..]),
        _ => return None,
    };
    let date = NaiveDate::from_ymd_opt(year, num(&rest[..2])?, num(&rest[2..4])?)?;
    let time = date.and_hms_opt(num(&rest[4..6])?, num(&rest[6..8])?, num(&rest[8..10])?)?;
    Some(time.and_utc())
}

/// 檢查憑證的 SAN 是否含 `DNS:<agent_id>`。
fn check_cert_san(path: &str, leaf: &CertificateDer<'_>, agent_id: &str) -> Result<(), String> {
    let cert = webpki::EndEntityCert::try_from(leaf)
        .map_err(|e| format!("無法解析 server.client_cert「{path}」中的客戶端憑證：{e:?}"))?;
    let name = ServerName::try_from(agent_id)
        .map_err(|e| format!("agent_id「{agent_id}」無法作為 DNS 名稱比對：{e}"))?;
    cert.verify_is_valid_for_subject_name(&name).map_err(|_| {
        format!(
            "server.client_cert「{path}」的 SAN 不含 DNS:{agent_id}（本機 agent_id）：伺服器會以 auth_failed 拒絕。\
             請以此 agent_id 重新簽發客戶端憑證（subjectAltName=DNS:{agent_id}），\
             或確認 agent.id 沒有被重新產生（可用 `pulse --print-agent-id` 查看）"
        )
    })
}

/// 客戶端憑證含多個 DNS SAN 時的警告：Kairos 預設（`auth.allow_multiple_dns_sans = false`）
/// 只接受恰好一個 DNS SAN 的客戶端憑證，否則以 auth_failed 拒絕。
/// 伺服器可能放寬此限制，因此只警告、不阻止啟動。無法解析憑證時回傳 `None`（SAN 檢查會另外報錯）。
fn multiple_dns_san_warning(path: &str, leaf: &CertificateDer<'_>) -> Option<String> {
    let cert = webpki::EndEntityCert::try_from(leaf).ok()?;
    let count = cert.valid_dns_names().count();
    (count > 1).then(|| {
        format!(
            "server.client_cert「{path}」含 {count} 個 DNS SAN：伺服器預設只接受恰好一個 DNS SAN（DNS:<agent_id>）\
             的客戶端憑證，除非伺服器設定了 auth.allow_multiple_dns_sans = true，否則會以 auth_failed 拒絕；\
             請以單一 SAN 重新簽發"
        )
    })
}

/// PEM 解析錯誤 → 附上欄位與路徑的錯誤訊息。
fn pem_error(path: &str, field: &str, item: &str, e: pem::Error) -> String {
    match e {
        pem::Error::Io(e) => format!("無法讀取 {field} 指定的檔案「{path}」：{e}"),
        pem::Error::NoItemsFound => format!("{field}「{path}」中找不到{item}"),
        e => format!("{field}「{path}」中的{item}格式錯誤：{e}"),
    }
}

/// `with_client_auth_cert` 失敗 → 具體說明。
fn client_cert_error(cert_path: &str, key_path: &str, e: &TlsError) -> String {
    match e {
        TlsError::InconsistentKeys(InconsistentKeys::KeyMismatch) => format!(
            "server.client_key「{key_path}」與 server.client_cert「{cert_path}」不成對（私鑰對應的公鑰與憑證不符）；\
             請確認兩者來自同一組金鑰"
        ),
        e => format!(
            "無法以 server.client_cert「{cert_path}」與 server.client_key「{key_path}」建立客戶端身分：{e}\
             （支援 RSA、ECDSA P-256 / P-384、Ed25519 私鑰）"
        ),
    }
}

/// 從連線錯誤中取出 rustls 錯誤（tokio-rustls 把它包在 `io::Error` 裡）。
pub fn rustls_error(e: &io::Error) -> Option<&TlsError> {
    e.get_ref()?.downcast_ref::<TlsError>()
}

/// 把握手 / 連線時的 rustls 錯誤轉成具體的處置提示；無法判斷時回傳 `None`。
///
/// TLS 1.3 下伺服器在握手「之後」才驗證客戶端憑證，因此客戶端憑證相關的 alert
/// 可能在第一次讀取時才出現，呼叫端對讀取錯誤也應套用此函式。
pub fn describe_tls_error(e: &TlsError, domain: &str, has_client_cert: bool) -> Option<String> {
    use AlertDescription as A;
    use CertificateError as C;
    let hint = match e {
        TlsError::AlertReceived(A::CertificateRequired) => {
            "伺服器要求客戶端憑證，請設定 server.client_cert / server.client_key".to_string()
        }
        TlsError::AlertReceived(A::HandshakeFailure) if !has_client_cert => {
            "伺服器拒絕握手；若伺服器要求客戶端憑證，請設定 server.client_cert / server.client_key"
                .to_string()
        }
        TlsError::AlertReceived(A::CertificateRevoked) => {
            "伺服器表示此客戶端憑證已被撤銷，請聯絡管理者重新簽發".to_string()
        }
        TlsError::AlertReceived(A::CertificateExpired) => {
            "伺服器表示此客戶端憑證已過期，請聯絡管理者重新簽發".to_string()
        }
        TlsError::AlertReceived(
            A::UnknownCA
            | A::BadCertificate
            | A::CertificateUnknown
            | A::UnsupportedCertificate
            | A::AccessDenied,
        ) if has_client_cert => format!(
            "伺服器不信任此客戶端憑證（{e}）：請確認憑證由伺服器信任的 client CA 簽發、\
             server.client_cert 含完整中繼 CA 鏈，且 EKU 含 clientAuth"
        ),
        TlsError::InvalidCertificate(C::UnknownIssuer) => {
            "伺服器憑證不受信任（簽發者不在信任庫中）：自簽憑證或私有 CA 請以 server.ca_cert 指定"
                .to_string()
        }
        TlsError::InvalidCertificate(C::NotValidForName | C::NotValidForNameContext { .. }) => {
            format!(
                "伺服器憑證的名稱與 server.domain「{domain}」不符：請把 server.domain 設為憑證 SAN 中的名稱，或重新簽發含此名稱的伺服器憑證"
            )
        }
        TlsError::InvalidCertificate(C::Expired | C::ExpiredContext { .. }) => {
            "伺服器憑證已過期（或本機時鐘不正確）".to_string()
        }
        TlsError::InvalidCertificate(C::NotValidYet | C::NotValidYetContext { .. }) => {
            "伺服器憑證尚未生效（或本機時鐘不正確）".to_string()
        }
        TlsError::InvalidCertificate(C::Other(other))
            if other.0.downcast_ref::<webpki::Error>() == Some(&webpki::Error::CaUsedAsEndEntity) =>
        {
            "伺服器憑證被標記為 CA（basicConstraints CA:TRUE），不能直接當伺服器憑證使用：\
             自簽憑證請在 openssl req 加上 -addext \"basicConstraints=critical,CA:FALSE\" 重新產生，\
             或改用私有 CA 簽發伺服器憑證"
                .to_string()
        }
        TlsError::InvalidCertificate(C::Revoked) => "伺服器憑證已被撤銷".to_string(),
        TlsError::InvalidCertificate(C::InvalidPurpose | C::InvalidPurposeContext { .. }) => {
            "伺服器憑證的用途（EKU）不含 serverAuth".to_string()
        }
        _ => return None,
    };
    Some(hint)
}

/// `accept_invalid_certs = true` 用的驗證器：接受任何伺服器憑證（不驗證信任鏈與名稱），
/// 但握手簽章仍以 crypto provider 的演算法正確驗證（與 `WebPkiServerVerifier` 相同的函式），
/// 確保對方確實持有該憑證的私鑰。
#[derive(Debug)]
struct AcceptAnyServerCert {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_rustls::rustls::OtherError;

    fn data(name: &str) -> String {
        format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn server(ca: &str, cert: &str, key: &str) -> ServerConfig {
        ServerConfig {
            address: "127.0.0.1".into(),
            port: 1,
            domain: "localhost".into(),
            accept_invalid_certs: false,
            ca_cert: if ca.is_empty() {
                String::new()
            } else {
                data(ca)
            },
            use_system_roots: true,
            client_cert: if cert.is_empty() {
                String::new()
            } else {
                data(cert)
            },
            client_key: if key.is_empty() {
                String::new()
            } else {
                data(key)
            },
        }
    }

    fn build_err(cfg: &ServerConfig) -> String {
        match build(cfg) {
            Ok(_) => panic!("應該要失敗"),
            Err(e) => e,
        }
    }

    #[test]
    fn ca_cert_with_multiple_certificates() {
        let roots = load_roots(&data("ca-bundle.pem"), true).unwrap();
        // 系統信任庫 + bundle 中的 2 張
        assert!(roots.len() >= 2);
        assert!(build(&server("ca-bundle.pem", "", "")).is_ok());
    }

    #[test]
    fn use_system_roots_false_trusts_only_ca_cert() {
        // P7：只信任 ca_cert 中的 2 張，不含任何系統信任庫的憑證
        let roots = load_roots(&data("ca-bundle.pem"), false).unwrap();
        assert_eq!(roots.len(), 2);
        let mut cfg = server("ca-bundle.pem", "", "");
        cfg.use_system_roots = false;
        assert!(build(&cfg).is_ok());
        // 兩者皆無 → 錯誤（設定驗證會先擋下，這裡是第二道防線）
        let err = load_roots("", false).unwrap_err();
        assert!(err.contains("use_system_roots"), "{err}");
    }

    #[test]
    fn bad_or_missing_ca_cert_reports_path() {
        let err = build_err(&server("not-pem.txt", "", ""));
        assert!(
            err.contains("server.ca_cert") && err.contains("not-pem.txt"),
            "{err}"
        );
        let err = build_err(&server("no-such-file.pem", "", ""));
        assert!(
            err.contains("無法讀取") && err.contains("no-such-file.pem"),
            "{err}"
        );
    }

    #[test]
    fn client_cert_rsa_pkcs8_and_pkcs1() {
        assert!(build(&server("root.pem", "client-rsa.pem", "client-rsa.key")).is_ok());
        assert!(
            build(&server(
                "root.pem",
                "client-rsa.pem",
                "client-rsa.pkcs1.key"
            ))
            .is_ok()
        );
    }

    #[test]
    fn client_cert_ec_chain_pkcs8_and_sec1() {
        let (chain, _) =
            load_client_cert(&data("client-ec-chain.pem"), &data("client-ec.key")).unwrap();
        assert_eq!(chain.len(), 2, "葉憑證 + 中繼 CA 都要送出");
        assert!(build(&server("root.pem", "client-ec-chain.pem", "client-ec.key")).is_ok());
        assert!(
            build(&server(
                "root.pem",
                "client-ec-chain.pem",
                "client-ec.sec1.key"
            ))
            .is_ok()
        );
    }

    #[cfg(unix)]
    #[test]
    fn client_key_permissions_are_checked() {
        // S4：私鑰權限過寬時記 warn，但不阻止啟動
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir()
            .join("pulse-tls-tests")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("client.key");
        std::fs::copy(data("client-rsa.key"), &key).unwrap();
        let mut cfg = server("root.pem", "client-rsa.pem", "");
        cfg.client_key = key.display().to_string();
        let key_warnings = |logs: &[(Level, String)]| {
            logs.iter()
                .filter(|(l, m)| *l == Level::Warn && m.contains("server.client_key"))
                .count()
        };

        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (res, logs) = crate::test_log::capture(|| build(&cfg));
        assert!(res.is_ok());
        assert_eq!(key_warnings(&logs), 0, "{logs:?}");

        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (res, logs) = crate::test_log::capture(|| build(&cfg));
        assert!(res.is_ok(), "權限過寬不阻止啟動");
        assert_eq!(key_warnings(&logs), 1, "{logs:?}");
        assert!(
            logs.iter()
                .any(|(_, m)| m.contains("644") && m.contains("chmod 600")),
            "{logs:?}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn client_key_acl_is_checked() {
        // S4：Windows 檢查 ACL；Users 可讀時記 warn，但不阻止啟動
        let dir = std::env::temp_dir()
            .join("pulse-tls-tests")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("client.key");
        std::fs::copy(data("client-rsa.key"), &key).unwrap();
        let mut cfg = server("root.pem", "client-rsa.pem", "");
        cfg.client_key = key.display().to_string();
        let key_warnings = |logs: &[(Level, String)]| {
            logs.iter()
                .filter(|(l, m)| *l == Level::Warn && m.contains("server.client_key"))
                .count()
        };
        // 使用者的 TEMP 只有本人、SYSTEM 與 Administrators 可存取
        let (res, logs) = crate::test_log::capture(|| build(&cfg));
        assert!(res.is_ok());
        assert_eq!(key_warnings(&logs), 0, "{logs:?}");

        let status = std::process::Command::new("icacls")
            .arg(&key)
            .args(["/grant", "*S-1-5-32-545:R"])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let (res, logs) = crate::test_log::capture(|| build(&cfg));
        assert!(res.is_ok(), "權限過寬不阻止啟動");
        assert_eq!(key_warnings(&logs), 1, "{logs:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn client_cert_san_must_match_agent_id() {
        const ID: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
        for (cert, key) in [
            ("client-rsa.pem", "client-rsa.key"),
            ("client-ec-chain.pem", "client-ec.key"),
        ] {
            let setup = build(&server("root.pem", cert, key)).unwrap();
            assert!(setup.verify_client_cert_identity(ID).is_ok());
            // 伺服器比對不分大小寫，本機檢查也一樣
            assert!(
                setup
                    .verify_client_cert_identity(&ID.to_uppercase())
                    .is_ok()
            );
            let err = setup
                .verify_client_cert_identity("11111111-2222-4333-8444-555555555555")
                .unwrap_err();
            assert!(
                err.contains("SAN 不含 DNS:11111111-2222-4333-8444-555555555555"),
                "{err}"
            );
        }
        // 未設定客戶端憑證時不檢查
        let setup = build(&server("root.pem", "", "")).unwrap();
        assert!(setup.verify_client_cert_identity("anything").is_ok());
    }
    #[test]
    fn multiple_dns_sans_warn_at_startup() {
        // 單一 DNS SAN（IP SAN 不算）→ 不警告
        for name in ["client-rsa.pem", "client-ec-chain.pem", "server.pem"] {
            assert_eq!(
                multiple_dns_san_warning(name, &first_cert(name)),
                None,
                "{name}"
            );
        }
        let warning = multiple_dns_san_warning("multi.pem", &first_cert("multi-san.pem")).unwrap();
        assert!(
            warning.contains("2 個 DNS SAN") && warning.contains("恰好一個"),
            "{warning}"
        );
        assert_eq!(
            multiple_dns_san_warning("x", &CertificateDer::from(vec![1, 2, 3])),
            None
        );
    }

    #[test]
    fn mismatched_key_is_rejected_at_startup() {
        let err = build_err(&server("root.pem", "client-rsa.pem", "client-ec.key"));
        assert!(err.contains("不成對"), "{err}");
        let err = build_err(&server("root.pem", "client-ec-chain.pem", "client-rsa.key"));
        assert!(err.contains("不成對"), "{err}");
    }

    #[test]
    fn missing_or_bad_client_files_report_field_and_path() {
        let err = build_err(&server("root.pem", "no-such-cert.pem", "client-rsa.key"));
        assert!(
            err.contains("server.client_cert") && err.contains("no-such-cert.pem"),
            "{err}"
        );
        let err = build_err(&server("root.pem", "client-rsa.pem", "not-pem.txt"));
        assert!(
            err.contains("server.client_key") && err.contains("not-pem.txt"),
            "{err}"
        );
        // 憑證檔誤給私鑰
        let err = build_err(&server("root.pem", "client-rsa.key", "client-rsa.key"));
        assert!(err.contains("server.client_cert"), "{err}");
    }

    #[test]
    fn accept_invalid_certs_builds_without_ca() {
        let mut cfg = server("", "", "");
        cfg.accept_invalid_certs = true;
        assert!(!build(&cfg).unwrap().verifies_server_cert());
        assert!(
            build(&server("root.pem", "", ""))
                .unwrap()
                .verifies_server_cert()
        );
        // 搭配客戶端憑證也可以
        let mut cfg = server("", "client-ec-chain.pem", "client-ec.key");
        cfg.accept_invalid_certs = true;
        assert!(build(&cfg).is_ok());
    }

    #[test]
    fn accept_invalid_certs_warning_is_error_level() {
        // P2：log.level = error 時也必須看得到
        let mut cfg = server("root.pem", "", "");
        cfg.accept_invalid_certs = true;
        let (res, logs) = crate::test_log::capture(|| build(&cfg));
        assert!(res.is_ok());
        let errors: Vec<_> = logs
            .iter()
            .filter(|(level, _)| *level == log::Level::Error)
            .map(|(_, msg)| msg.as_str())
            .collect();
        assert!(
            errors.iter().any(|m| m.contains("accept_invalid_certs")),
            "{logs:?}"
        );
        // 同時設定 ca_cert 的提醒也是 error 等級
        assert!(errors.iter().any(|m| m.contains("ca_cert")), "{logs:?}");
        assert!(
            !logs
                .iter()
                .any(|(level, msg)| *level > log::Level::Error && msg.contains("accept_invalid")),
            "{logs:?}"
        );
    }

    #[test]
    fn encrypted_private_key_has_specific_error() {
        // P10
        let dir = std::env::temp_dir()
            .join("pulse-tls-tests")
            .join(format!("enc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content) in [
            (
                "pkcs8.key",
                "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIBvTBXBgkqhkiG9w0BBQ0w\n-----END ENCRYPTED PRIVATE KEY-----\n",
            ),
            (
                "legacy.key",
                "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-256-CBC,0011\n\nAAAA\n-----END RSA PRIVATE KEY-----\n",
            ),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, content).unwrap();
            let mut cfg = server("root.pem", "client-rsa.pem", "");
            cfg.client_key = path.to_string_lossy().into_owned();
            let err = build_err(&cfg);
            assert!(
                err.contains("加密的私鑰") && err.contains(name),
                "{name}: {err}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
        // 未加密的私鑰不受影響
        assert!(!is_encrypted_key_pem(
            &std::fs::read(data("client-rsa.key")).unwrap()
        ));
        assert!(!is_encrypted_key_pem(
            &std::fs::read(data("client-ec.sec1.key")).unwrap()
        ));
    }

    fn parse_utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn first_cert(name: &str) -> CertificateDer<'static> {
        load_certs(&data(name), "test", "憑證").unwrap().remove(0)
    }

    #[test]
    fn cert_validity_is_parsed_from_der() {
        // 測試憑證以 -days 36500 簽發，notAfter 落在 2126 年（GeneralizedTime）
        for name in ["client-rsa.pem", "client-ec-chain.pem", "root.pem"] {
            let (not_before, not_after) = cert_validity(&first_cert(name)).unwrap();
            assert!(
                not_before.format("%Y").to_string().starts_with("20"),
                "{name}"
            );
            assert_eq!(not_after.format("%Y").to_string(), "2126", "{name}");
            assert!(not_before < not_after);
        }
        // UTCTime 兩位數年份
        assert_eq!(
            der_time(0x17, b"491231235959Z"),
            Some(parse_utc("2049-12-31T23:59:59Z"))
        );
        assert_eq!(
            der_time(0x17, b"500101000000Z"),
            Some(parse_utc("1950-01-01T00:00:00Z"))
        );
        assert_eq!(
            der_time(0x18, b"21260914213853Z"),
            Some(parse_utc("2126-09-14T21:38:53Z"))
        );
        // 格式錯誤一律 None，不 panic
        for (tag, bad) in [
            (0x17, &b"4912312359Z"[..]),
            (0x17, b"491231235959"),
            (0x18, b"491231235959Z"),
            (0x17, b"491331235959Z"),
            (0x17, b"49123123595aZ"),
            (0x05, b"491231235959Z"),
            (0x17, "４91231235959Z".as_bytes()),
        ] {
            assert_eq!(der_time(tag, bad), None, "{bad:?}");
        }
        for bad in [
            &[][..],
            &[0x30],
            &[0x30, 0x85, 0, 0, 0, 0, 1],
            &[0x30, 0x82, 0xff],
            &[0x30, 0x03, 0x30, 0x01],
        ] {
            assert_eq!(
                cert_validity(&CertificateDer::from(bad.to_vec())),
                None,
                "{bad:?}"
            );
        }
        // 截斷的真實憑證也不 panic
        let der = first_cert("client-rsa.pem").as_ref().to_vec();
        for len in 0..der.len().min(400) {
            let _ = cert_validity(&CertificateDer::from(der[..len].to_vec()));
        }
    }

    #[test]
    fn expired_client_cert_is_error_but_not_fatal_and_near_expiry_warns() {
        // P9 / R2-P10：以注入的「現在時間」驗證判斷規則
        let chain = load_certs(&data("client-ec-chain.pem"), "test", "憑證").unwrap();
        let (not_before, not_after) = cert_validity(&chain[0]).unwrap();
        let day = chrono::Duration::days(1);

        // 有效期間內、離到期還很久 → 無警告
        assert!(check_chain_validity("c.pem", &chain, not_before + day).is_empty());
        // 已過期 → error（不再阻止啟動），訊息含路徑、「過期」並提醒檢查本機時鐘
        let findings = check_chain_validity("c.pem", &chain[..1], not_after + day);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].level, Level::Error);
        let msg = &findings[0].message;
        assert!(
            msg.contains("c.pem") && msg.contains("過期") && msg.contains("本機時鐘"),
            "{msg}"
        );
        assert!(msg.contains("繼續嘗試連線"), "{msg}");
        // 尚未生效 → 警告（可能是本機時鐘不正確）
        let findings = check_chain_validity("c.pem", &chain[..1], not_before - day);
        assert!(
            findings
                .iter()
                .any(|f| f.level == Level::Warn && f.message.contains("才生效")),
            "{findings:?}"
        );
        // 無法解析的憑證 → 警告
        let findings =
            check_chain_validity("c.pem", &[CertificateDer::from(vec![1, 2, 3])], Utc::now());
        assert_eq!(findings[0].level, Level::Warn);
        assert!(findings[0].message.contains("無法解析"), "{findings:?}");
        // 目前的測試憑證可正常建立
        assert!(build(&server("root.pem", "client-ec-chain.pem", "client-ec.key")).is_ok());
    }

    #[test]
    fn remaining_days_are_rounded_up_for_leaf_only() {
        // R2-P4：只針對葉憑證（避免中繼 CA 的到期日讓測試碰巧通過），剩餘天數無條件進位
        let chain = load_certs(&data("client-ec-chain.pem"), "test", "憑證").unwrap();
        let leaf = &chain[..1];
        let (_, not_after) = cert_validity(&leaf[0]).unwrap();
        let secs = chrono::Duration::seconds;
        let expiry_days = |now| {
            let findings = check_chain_validity("c.pem", leaf, now);
            assert_eq!(findings.len(), 1, "{findings:?}");
            assert_eq!(findings[0].level, Level::Warn);
            findings[0].message.clone()
        };
        // 剛好 10 天 → 10；差 1 秒不到 10 天 → 10；多 1 秒 → 11
        let day10 = chrono::Duration::days(10);
        assert!(expiry_days(not_after - day10).contains("不超過 10 天"));
        assert!(expiry_days(not_after - day10 + secs(1)).contains("不超過 10 天"));
        assert!(expiry_days(not_after - day10 - secs(1)).contains("不超過 11 天"));
        // 剩 1 秒、剛好到期 → 1 天
        assert!(expiry_days(not_after - secs(1)).contains("不超過 1 天"));
        assert!(expiry_days(not_after).contains("不超過 1 天"));
        assert_eq!(remaining_days(Duration::from_secs(86_400 * 30)), 30);
        assert_eq!(remaining_days(Duration::from_secs(86_400 * 29 + 1)), 30);
        assert_eq!(remaining_days(Duration::ZERO), 1);
    }

    #[test]
    fn only_expired_findings_are_mirrored_to_stderr() {
        assert!(mirrors_to_stderr(&CertFinding::error("expired".into())));
        assert!(!mirrors_to_stderr(&CertFinding::warn(
            "expires soon".into()
        )));
    }

    #[test]
    fn client_cert_validity_can_be_rechecked_later() {
        // R2-P10：TlsSetup 保留憑證鏈，之後（每 24 小時）可用新的時間重新檢查
        let setup = build(&server("root.pem", "client-ec-chain.pem", "client-ec.key")).unwrap();
        assert!(setup.has_client_cert());
        let chain = load_certs(&data("client-ec-chain.pem"), "test", "憑證").unwrap();
        let (_, leaf_not_after) = cert_validity(&chain[0]).unwrap();
        assert!(setup.client_cert_validity(Utc::now()).is_empty());
        let later = setup.client_cert_validity(leaf_not_after - chrono::Duration::days(3));
        assert!(
            later
                .iter()
                .any(|f| f.level == Level::Warn && f.message.contains("不超過 3 天")),
            "{later:?}"
        );
        let expired = setup.client_cert_validity(leaf_not_after + chrono::Duration::days(1));
        assert!(
            expired.iter().any(|f| f.level == Level::Error),
            "{expired:?}"
        );
        // 記錄時依各自的等級輸出；已過期（error）同時寫到 stderr（N4），即將到期（warn）不寫
        let (((), logs), events) = crate::service_status::capture(|| {
            crate::test_log::capture(|| {
                log_findings(&expired);
                log_findings(&later);
            })
        });
        assert!(
            logs.iter()
                .any(|(l, m)| *l == Level::Error && m.contains("過期"))
        );
        let errors: Vec<_> = expired
            .iter()
            .filter(|f| f.level == Level::Error)
            .map(|f| crate::service_status::Event::Stderr(f.message.clone()))
            .collect();
        assert!(!errors.is_empty());
        assert_eq!(events, errors);
        // 未設定客戶端憑證：沒有可檢查的項目
        let none = build(&server("root.pem", "", "")).unwrap();
        assert!(!none.has_client_cert());
        assert!(none.client_cert_validity(Utc::now()).is_empty());
        assert_eq!(CERT_RECHECK_INTERVAL, Duration::from_secs(86_400));
    }

    // ---- 實際 TLS 握手（T5）：127.0.0.1 隨機埠上的 rustls 伺服器 ----

    /// 啟動只接受一條連線的 TLS 伺服器。`client_ca` 非空時要求並驗證客戶端憑證。
    ///
    /// 回傳位址與伺服器結果：握手成功時為客戶端送出的憑證鏈長度。
    async fn spawn_tls_server(
        client_ca: Option<&str>,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<io::Result<usize>>,
    ) {
        use tokio_rustls::rustls::server::WebPkiClientVerifier;
        let provider = Arc::new(crypto::ring::default_provider());
        let chain = load_certs(&data("server.pem"), "test", "憑證").unwrap();
        let key = PrivateKeyDer::from_pem_file(data("server.key")).unwrap();
        let builder = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap();
        let config = match client_ca {
            Some(ca) => {
                let mut roots = RootCertStore::empty();
                for cert in load_certs(&data(ca), "test", "憑證").unwrap() {
                    roots.add(cert).unwrap();
                }
                let verifier =
                    WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                        .build()
                        .unwrap();
                builder
                    .with_client_cert_verifier(verifier)
                    .with_single_cert(chain, key)
            }
            None => builder.with_no_client_auth().with_single_cert(chain, key),
        }
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await?;
            let stream = acceptor.accept(tcp).await?;
            let (_, conn) = stream.get_ref();
            Ok(conn.peer_certificates().map_or(0, <[_]>::len))
        });
        (addr, handle)
    }

    /// 以 `cfg` 建立的 TLS 設定連到 `addr`，回傳握手結果與 TlsSetup。
    async fn handshake(
        cfg: &ServerConfig,
        addr: std::net::SocketAddr,
    ) -> (
        io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
        TlsSetup,
    ) {
        let setup = build(cfg).unwrap();
        setup.reset_client_cert_request();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let res = setup
            .connector
            .connect(setup.server_name.clone(), tcp)
            .await;
        (res, setup)
    }

    fn trusting_test_server() -> ServerConfig {
        let mut cfg = server("", "", "");
        cfg.ca_cert = data("server-ca.pem");
        cfg.use_system_roots = false;
        cfg
    }

    #[tokio::test]
    async fn handshake_succeeds_with_trusted_server_cert() {
        for domain in ["localhost", "127.0.0.1"] {
            let (addr, server) = spawn_tls_server(None).await;
            let mut cfg = trusting_test_server();
            cfg.domain = domain.into();
            let (res, setup) = handshake(&cfg, addr).await;
            let stream = res.unwrap_or_else(|e| panic!("{domain}: {e}"));
            assert_eq!(server.await.unwrap().unwrap(), 0);
            assert!(!setup.client_cert_was_requested());
            drop(stream);
        }
    }

    #[tokio::test]
    async fn handshake_rejects_untrusted_server_cert() {
        // 只用系統信任庫（不含測試 CA）、或信任另一個 CA → 簽發者不受信任
        let mut other_ca = server("root.pem", "", "");
        other_ca.use_system_roots = false;
        for cfg in [server("", "", ""), other_ca] {
            let (addr, _server) = spawn_tls_server(None).await;
            let (res, _) = handshake(&cfg, addr).await;
            let err = res.unwrap_err();
            let tls_err = rustls_error(&err).expect("應為 rustls 錯誤");
            assert!(
                matches!(
                    tls_err,
                    TlsError::InvalidCertificate(CertificateError::UnknownIssuer)
                ),
                "{tls_err:?}"
            );
            assert!(
                describe_tls_error(tls_err, &cfg.domain, false)
                    .unwrap()
                    .contains("server.ca_cert")
            );
        }
    }

    #[tokio::test]
    async fn handshake_rejects_name_mismatch() {
        let (addr, _server) = spawn_tls_server(None).await;
        let mut cfg = trusting_test_server();
        cfg.domain = "kairos.example.com".into();
        let (res, _) = handshake(&cfg, addr).await;
        let err = res.unwrap_err();
        let tls_err = rustls_error(&err).expect("應為 rustls 錯誤");
        assert!(
            matches!(
                tls_err,
                TlsError::InvalidCertificate(
                    CertificateError::NotValidForName
                        | CertificateError::NotValidForNameContext { .. }
                )
            ),
            "{tls_err:?}"
        );
        assert!(
            describe_tls_error(tls_err, "kairos.example.com", false)
                .unwrap()
                .contains("kairos.example.com")
        );
    }

    #[tokio::test]
    async fn accept_invalid_certs_skips_chain_and_name_checks() {
        let (addr, srv) = spawn_tls_server(None).await;
        let mut cfg = server("", "", "");
        cfg.accept_invalid_certs = true;
        cfg.domain = "kairos.example.com".into();
        let (res, setup) = handshake(&cfg, addr).await;
        let stream = res.unwrap();
        assert!(!setup.verifies_server_cert());
        assert_eq!(srv.await.unwrap().unwrap(), 0);
        drop(stream);
    }

    #[tokio::test]
    async fn mtls_sends_client_cert_chain() {
        // 伺服器信任 root.pem；EC 客戶端憑證由中繼 CA 簽發，需送出整條鏈才能驗證
        let (addr, server) = spawn_tls_server(Some("root.pem")).await;
        let mut cfg = trusting_test_server();
        cfg.client_cert = data("client-ec-chain.pem");
        cfg.client_key = data("client-ec.key");
        let (res, setup) = handshake(&cfg, addr).await;
        let stream = res.unwrap();
        assert_eq!(server.await.unwrap().unwrap(), 2, "葉憑證 + 中繼 CA");
        assert!(setup.client_cert_was_requested());
        drop(stream);
    }

    #[tokio::test]
    async fn mtls_without_client_cert_is_rejected_by_server() {
        let (addr, server) = spawn_tls_server(Some("root.pem")).await;
        let (res, setup) = handshake(&trusting_test_server(), addr).await;
        // TLS 1.3 下客戶端的握手可能先完成，拒絕由伺服器端發生
        assert!(server.await.unwrap().is_err());
        assert!(setup.client_cert_was_requested());
        drop(res);
    }

    #[test]
    fn accept_any_verifier_still_lists_signature_schemes() {
        let v = AcceptAnyServerCert {
            provider: Arc::new(crypto::ring::default_provider()),
        };
        let schemes = v.supported_verify_schemes();
        assert!(schemes.contains(&SignatureScheme::ECDSA_NISTP256_SHA256));
        assert!(schemes.contains(&SignatureScheme::RSA_PSS_SHA256));
        // 簽章驗證本身（DigitallySignedStruct 無法在 crate 外建構）由整合測試以實際握手驗證
    }

    #[test]
    fn server_name_accepts_dns_and_ip() {
        for domain in ["localhost", "kairos.example.com", "127.0.0.1", "::1"] {
            let mut cfg = server("root.pem", "", "");
            cfg.domain = domain.into();
            assert!(build(&cfg).is_ok(), "{domain}");
        }
        let mut cfg = server("root.pem", "", "");
        cfg.domain = "bad name!".into();
        assert!(build_err(&cfg).contains("server.domain"));
    }

    #[test]
    fn tls_error_descriptions() {
        use AlertDescription as A;
        use CertificateError as C;
        let d = |e: TlsError, has_cert: bool| describe_tls_error(&e, "kairos.local", has_cert);

        assert!(
            d(TlsError::AlertReceived(A::CertificateRequired), false)
                .unwrap()
                .contains("server.client_cert")
        );
        assert!(
            d(TlsError::AlertReceived(A::HandshakeFailure), false)
                .unwrap()
                .contains("server.client_cert")
        );
        assert_eq!(d(TlsError::AlertReceived(A::HandshakeFailure), true), None);
        for alert in [A::UnknownCA, A::BadCertificate, A::AccessDenied] {
            assert!(
                d(TlsError::AlertReceived(alert), true)
                    .unwrap()
                    .contains("伺服器不信任此客戶端憑證")
            );
            assert_eq!(d(TlsError::AlertReceived(alert), false), None);
        }
        assert!(
            d(TlsError::AlertReceived(A::CertificateRevoked), true)
                .unwrap()
                .contains("撤銷")
        );
        assert!(
            d(TlsError::InvalidCertificate(C::UnknownIssuer), false)
                .unwrap()
                .contains("server.ca_cert")
        );
        let hint = d(TlsError::InvalidCertificate(C::NotValidForName), false).unwrap();
        assert!(
            hint.contains("server.domain") && hint.contains("kairos.local"),
            "{hint}"
        );
        assert!(
            d(TlsError::InvalidCertificate(C::Expired), false)
                .unwrap()
                .contains("過期")
        );
        let ca_as_ee = TlsError::InvalidCertificate(C::Other(OtherError(Arc::new(
            webpki::Error::CaUsedAsEndEntity,
        ))));
        assert!(d(ca_as_ee, false).unwrap().contains("CA:FALSE"));
        let other =
            TlsError::InvalidCertificate(C::Other(OtherError(Arc::new(webpki::Error::BadDer))));
        assert_eq!(d(other, false), None);
        assert_eq!(d(TlsError::DecryptError, true), None);
    }

    #[test]
    fn rustls_error_extracted_from_io_error() {
        let io = io::Error::new(
            io::ErrorKind::InvalidData,
            TlsError::AlertReceived(AlertDescription::CertificateRequired),
        );
        assert!(matches!(
            rustls_error(&io),
            Some(TlsError::AlertReceived(
                AlertDescription::CertificateRequired
            ))
        ));
        assert!(rustls_error(&io::Error::other("x")).is_none());
    }
}
