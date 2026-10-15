//! 管理者的 CA 工具（`kairos/PKI.md` 第 15 節）：`kairos pki init`、`kairos pki sign`、`kairos pki gencrl`、
//! `kairos pki revocations export`。
//!
//! 取代 INSTALL 舊版手寫 `ca.cnf` 與 `openssl ca` 的流程，並與 8444 的線上簽發、主控台 S15「CA 與撤銷清單」
//! 共用同一套程式碼與儲存：
//!
//! - 用戶端憑證的範本與 CRL 的產生沿用 [`super::issue`]（[`client_params`]、[`crl_der`]）。
//! - `pki init` 的代理簽發 CA 以 [`super::manage::create`]／[`super::manage::install`] 建立與安裝
//!   （`pki_cas`、`data/pki/issuing-<ca_id>.key`），與 `kairos pki issuing-ca new|install` 相同。
//! - 線上模式的 `pki gencrl` 以 [`crate::enroll::pki_mgr::PkiManager::regenerate_crl`] 產生
//!   （主控台 S15「立即產生 CRL」的同一個函式，寫入 `pki_crls` 與 `data/pki/crl/issuing.crl`）。
//!
//! 憑證範本：
//!
//! | 種類 | 金鑰 | 擴充欄位 | 有效期間 |
//! |---|---|---|---|
//! | 根 CA（`init`） | ECDSA P-384 | `CA:TRUE, pathlen:1`（critical）、`keyCertSign, cRLSign`（critical）、SKI | 10 年 |
//! | 伺服器（`--profile server`） | `init` 產生 P-256；CSR 可為 P-256／P-384／RSA | `CA:FALSE`（critical）、`digitalSignature`（critical）、EKU `serverAuth`、SAN（DNS／IP）、SKI／AKI | 預設 825 天，上限 825 天 |
//! | 代理簽發 CA（`--profile issuing-ca`） | CSR 必須是 P-256 | `CA:TRUE, pathlen:0`、`keyCertSign, cRLSign`、EKU `clientAuth`、SKI／AKI（第 5.2 節） | 5 年 |
//! | 用戶端（`--profile client`） | CSR 必須是 P-256 | 第 5.1 節（與線上簽發相同） | 預設 365 天，上限 825 天 |
//!
//! 所有憑證的有效期間都截在簽發者的 `notAfter` 之前；序號為 16 bytes 隨機值；CSR 的擴充欄位一律不複製。
//! 簽發者私鑰可以是 ECDSA P-256、P-384 或 RSA（既有的 RSA 根 CA 照用，PKI.md 第 1 節第 4 項）。
//!
//! CA 主機上的紀錄：`sign`、`gencrl` 在 CA 私鑰所在的目錄附加一行 JSON 到 `kairos-ca.log`（0600；
//! 時間、執行者、動作、序號、subject、SAN、CRL 版本），作用同 `openssl ca` 的 `index.txt`；寫不進去時不簽發。

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384,
    PKCS_RSA_SHA256, PKCS_RSA_SHA384, PKCS_RSA_SHA512, PublicKeyData, SanType, SerialNumber,
    SubjectPublicKeyInfo,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use tokio_rustls::rustls::pki_types::CertificateDer;
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::prelude::FromDer;
use zeroize::Zeroizing;

use super::issue::{
    Issued, NOT_BEFORE_BACKDATE_MS, Reason, RevokedEntry, client_params, crl_der, random_serial,
    to_time,
};
use super::store::{self as ps, CRL_KEEP_EXPIRED_MS};
use super::x509::{CertInfo, parse_cert, pem_decode_all, pem_encode};

pub const DAY_MS: i64 = 86_400_000;
/// 根 CA 的有效天數（設計包 S02「根 CA 10 年」）
pub const ROOT_DAYS: i64 = 3650;
/// 代理簽發 CA 的有效天數（第 5.2 節）
pub const ISSUING_DAYS: i64 = 1825;
/// 伺服器憑證的預設與上限天數（與 INSTALL 舊版的 `-days 825` 相同）
pub const SERVER_DAYS: i64 = 825;
/// 用戶端憑證的預設天數（D2）與上限（token 覆寫的上限，S12）
pub const CLIENT_DAYS: i64 = 365;
pub const MAX_LEAF_DAYS: i64 = 825;
/// `gencrl` 的 CRL 有效天數預設（`nextUpdate`；與 `pki.crl_validity_days` 的預設相同）
pub const CRL_DAYS: i64 = 30;

/// `kairos pki revocations export` 的格式
pub const REVOCATIONS_SCHEMA: &str = "kairos-revocations/v1";
/// CA 主機上的簽發紀錄（CA 私鑰所在的目錄）
pub const JOURNAL_FILE: &str = "kairos-ca.log";
/// `pki init` 寫出的檔案
pub const ROOT_KEY_FILE: &str = "root-ca.key";
pub const ROOT_CERT_FILE: &str = "root-ca.pem";
pub const SERVER_CERT_FILE: &str = "server.pem";
pub const SERVER_KEY_FILE: &str = "server.key";

// ---------------------------------------------------------------------------
// 伺服器名稱（SAN）
// ---------------------------------------------------------------------------

/// 伺服器憑證的名稱：DNS 名稱或 IP 位址（`--name`，可加 `DNS:`／`IP:` 前綴）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerName {
    Dns(String),
    Ip(IpAddr),
}

impl ServerName {
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim();
        let (forced, t) = if let Some(r) = t.strip_prefix("DNS:").or_else(|| t.strip_prefix("dns:"))
        {
            (Some(false), r)
        } else if let Some(r) = t.strip_prefix("IP:").or_else(|| t.strip_prefix("ip:")) {
            (Some(true), r)
        } else {
            (None, t)
        };
        let t = t.trim();
        if forced != Some(false)
            && let Ok(ip) = t.trim_start_matches('[').trim_end_matches(']').parse()
        {
            return Ok(Self::Ip(ip));
        }
        if forced == Some(true) {
            return Err(format!("「{text}」不是有效的 IP 位址"));
        }
        let name = t.trim_end_matches('.').to_ascii_lowercase();
        let valid_label = |l: &str| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        };
        if name.is_empty() || name.len() > 253 || !name.split('.').all(valid_label) {
            return Err(format!(
                "「{text}」不是有效的 DNS 名稱或 IP 位址（不接受萬用字元 *；國際化網域名稱請先轉成 xn-- 形式）"
            ));
        }
        if name
            .split('.')
            .all(|l| l.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(format!("「{text}」看起來是 IP 位址但格式不正確"));
        }
        Ok(Self::Dns(name))
    }

    fn san(&self) -> Result<SanType, String> {
        Ok(match self {
            Self::Dns(n) => SanType::DnsName(
                n.clone()
                    .try_into()
                    .map_err(|e| format!("{n} 無法放入 SAN：{e}"))?,
            ),
            Self::Ip(ip) => SanType::IpAddress(*ip),
        })
    }

    /// `DNS:monitor.example.com`、`IP:10.0.0.5`
    pub fn text(&self) -> String {
        match self {
            Self::Dns(n) => format!("DNS:{n}"),
            Self::Ip(ip) => format!("IP:{ip}"),
        }
    }

    fn common_name(&self) -> String {
        match self {
            Self::Dns(n) => n.clone(),
            Self::Ip(ip) => ip.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// 簽發者（根 CA、代理 CA、簽發 CA）
// ---------------------------------------------------------------------------

/// 私鑰的演算法（給人看）；不支援的演算法回 `Err`
fn key_label(key: &KeyPair) -> Result<&'static str, String> {
    let alg = key.algorithm();
    if std::ptr::eq(alg, &PKCS_ECDSA_P256_SHA256) {
        Ok("ECDSA P-256")
    } else if std::ptr::eq(alg, &PKCS_ECDSA_P384_SHA384) {
        Ok("ECDSA P-384")
    } else if [&PKCS_RSA_SHA256, &PKCS_RSA_SHA384, &PKCS_RSA_SHA512]
        .iter()
        .any(|a| std::ptr::eq(alg, *a))
    {
        Ok("RSA")
    } else {
        Err("CA 私鑰的演算法不支援（只接受 ECDSA P-256、P-384 或 RSA）".to_string())
    }
}

/// CA 憑證與私鑰（簽發伺服器憑證、代理簽發 CA、用戶端憑證與 CRL）
pub struct CaKey {
    pub cert_der: Vec<u8>,
    pub info: CertInfo,
    pub key_alg: &'static str,
    issuer: Issuer<'static, KeyPair>,
}

impl std::fmt::Debug for CaKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaKey")
            .field("subject", &self.info.subject_text)
            .field("key_alg", &self.key_alg)
            .finish_non_exhaustive()
    }
}

impl CaKey {
    /// 由 CA 憑證（PEM；取第一張）與未加密的 PKCS#8 私鑰（PEM）建立：檢查是 CA、可簽發憑證、兩者成對
    pub fn load(cert_pem: &str, key_pem: &str) -> Result<Self, String> {
        let der = pem_decode_all(cert_pem, Some("CERTIFICATE"))?
            .into_iter()
            .next()
            .ok_or("CA 憑證檔中找不到憑證（PEM 的 CERTIFICATE）")?
            .1;
        let key = KeyPair::from_pem(key_pem).map_err(|e| {
            format!(
                "CA 私鑰無法讀取（需要未加密的 PKCS#8 PEM「BEGIN PRIVATE KEY」；「BEGIN RSA PRIVATE KEY」或「BEGIN EC PRIVATE KEY」\
                 以 openssl pkcs8 -topk8 -nocrypt -in <舊檔> -out <新檔> 轉換）：{e}"
            )
        })?;
        Self::from_parts(der, key)
    }

    fn from_parts(der: Vec<u8>, key: KeyPair) -> Result<Self, String> {
        let info = parse_cert(&der)?;
        if !info.is_ca {
            return Err("不是 CA 憑證（basicConstraints 必須是 CA:TRUE）".to_string());
        }
        if info.key_usage.is_some_and(|ku| !ku.key_cert_sign) {
            return Err("CA 憑證的 keyUsage 沒有 keyCertSign，不能簽發憑證".to_string());
        }
        if key.subject_public_key_info() != info.spki_der {
            return Err("CA 憑證與私鑰不成對".to_string());
        }
        let key_alg = key_label(&key)?;
        let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(der.clone()), key)
            .map_err(|e| format!("CA 憑證無法使用：{e}"))?;
        Ok(Self {
            cert_der: der,
            info,
            key_alg,
            issuer,
        })
    }

    /// CA 私鑰（PEM）
    pub(crate) fn key_pem(&self) -> Zeroizing<String> {
        Zeroizing::new(self.issuer.key().serialize_pem())
    }

    /// 有效期間：`notBefore` 往前 10 分鐘，`notAfter` 截在簽發者的 `notAfter` 之前
    fn window(&self, now_ms: i64, days: i64) -> Result<(i64, i64), String> {
        let not_after = (now_ms + days * DAY_MS).min(self.info.not_after_ms);
        if not_after <= now_ms {
            return Err("CA 憑證已過期，無法簽發".to_string());
        }
        if self.info.not_before_ms > now_ms + 600_000 {
            return Err("CA 憑證尚未生效（notBefore 在未來；檢查這台主機的時鐘）".to_string());
        }
        Ok((now_ms - NOT_BEFORE_BACKDATE_MS, not_after))
    }

    fn base_params(&self, cn: &str, nb: i64, na: i64) -> Result<CertificateParams, String> {
        let mut p = CertificateParams::default();
        p.serial_number = Some(SerialNumber::from_slice(&random_serial()?));
        p.not_before = to_time(nb)?;
        p.not_after = to_time(na)?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cn);
        p.distinguished_name = dn;
        p.use_authority_key_identifier_extension = true;
        Ok(p)
    }

    fn finish(&self, p: &CertificateParams, spki_der: &[u8]) -> Result<Issued, String> {
        let public =
            SubjectPublicKeyInfo::from_der(spki_der).map_err(|e| format!("公鑰無法使用：{e}"))?;
        let cert = p
            .signed_by(&public, &self.issuer)
            .map_err(|e| format!("簽發失敗：{e}"))?;
        let der = cert.der().to_vec();
        let info = parse_cert(&der)?;
        Ok(Issued {
            der,
            serial: info.serial,
            not_before_ms: info.not_before_ms,
            not_after_ms: info.not_after_ms,
        })
    }

    /// 伺服器憑證：CN＝第一個名稱，SAN＝全部名稱，`CA:FALSE`、`digitalSignature`、EKU `serverAuth`
    pub fn sign_server(
        &self,
        spki_der: &[u8],
        names: &[ServerName],
        days: i64,
        now_ms: i64,
    ) -> Result<Issued, String> {
        let first = names.first().ok_or("至少需要一個伺服器名稱（--name）")?;
        if !(1..=SERVER_DAYS).contains(&days) {
            return Err(format!("伺服器憑證的有效天數必須是 1～{SERVER_DAYS}"));
        }
        let (nb, na) = self.window(now_ms, days)?;
        let mut p = self.base_params(&first.common_name(), nb, na)?;
        p.subject_alt_names = names
            .iter()
            .map(ServerName::san)
            .collect::<Result<_, _>>()?;
        p.is_ca = IsCa::ExplicitNoCa;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        self.finish(&p, spki_der)
    }

    /// 代理簽發 CA（第 5.2 節）：`CA:TRUE, pathlen:0`、`keyCertSign, cRLSign`、EKU `clientAuth`；公鑰必須是 P-256
    pub fn sign_issuing_ca(
        &self,
        spki_der: &[u8],
        cn: &str,
        days: i64,
        now_ms: i64,
    ) -> Result<Issued, String> {
        if kairos_proto::enroll::p256_point_from_spki(spki_der).is_none() {
            return Err(
                "代理簽發 CA 的公鑰必須是 ECDSA P-256（以 kairos pki issuing-ca new 產生）"
                    .to_string(),
            );
        }
        if self.info.path_len == Some(0) {
            return Err("這個 CA 帶 pathlen:0，不能再簽發下層 CA；請以根 CA 簽發".to_string());
        }
        if cn.trim().is_empty() {
            return Err("CSR 沒有 subject 的 CN".to_string());
        }
        if !(1..=ISSUING_DAYS).contains(&days) {
            return Err(format!("代理簽發 CA 的有效天數必須是 1～{ISSUING_DAYS}"));
        }
        let (nb, na) = self.window(now_ms, days)?;
        let mut p = self.base_params(cn.trim(), nb, na)?;
        p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        self.finish(&p, spki_der)
    }

    /// 用戶端憑證（第 5.1 節的範本，與線上簽發相同）；公鑰必須是 P-256
    pub fn sign_client(
        &self,
        spki_der: &[u8],
        agent_id: &str,
        days: i64,
        now_ms: i64,
    ) -> Result<Issued, String> {
        if !kairos_proto::is_uuid_text(agent_id) {
            return Err(format!("agent_id「{agent_id}」不是 UUID"));
        }
        if kairos_proto::enroll::p256_point_from_spki(spki_der).is_none() {
            return Err("用戶端憑證的公鑰必須是 ECDSA P-256".to_string());
        }
        if !(1..=MAX_LEAF_DAYS).contains(&days) {
            return Err(format!("用戶端憑證的有效天數必須是 1～{MAX_LEAF_DAYS}"));
        }
        let (nb, na) = self.window(now_ms, days)?;
        let p = client_params(&agent_id.to_ascii_lowercase(), &random_serial()?, nb, na)?;
        self.finish(&p, spki_der)
    }

    /// CRL（DER）
    pub fn sign_crl(
        &self,
        crl_number: u64,
        this_update_ms: i64,
        next_update_ms: i64,
        revoked: &[RevokedEntry],
    ) -> Result<Vec<u8>, String> {
        if self.info.key_usage.is_some_and(|ku| !ku.crl_sign) {
            return Err("CA 憑證的 keyUsage 沒有 cRLSign，不能產生 CRL".to_string());
        }
        crl_der(
            &self.issuer,
            crl_number,
            this_update_ms,
            next_update_ms,
            revoked,
        )
    }

    /// CA 公鑰的 SHA-256（十六進位小寫；`kairos-revocations/v1` 與 CA 主機紀錄用來辨識 CA）
    pub fn spki_sha256_hex(&self) -> String {
        hex(&kairos_proto::enroll::sha256(&self.info.spki_der))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 根 CA 的 CN：`<組織> Kairos Root CA`；組織名稱就是 `Kairos`（預設）時為 `Kairos Root CA`
fn root_common_name(organization: &str) -> String {
    let org = organization.trim();
    if org.eq_ignore_ascii_case("kairos") {
        "Kairos Root CA".to_string()
    } else {
        format!("{org} Kairos Root CA")
    }
}

/// 新的根 CA：ECDSA P-384、`CN=<組織> Kairos Root CA`、`CA:TRUE, pathlen:1`、10 年
pub fn new_root(organization: &str, now_ms: i64) -> Result<CaKey, String> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384)
        .map_err(|e| format!("無法產生根 CA 私鑰：{e}"))?;
    let mut p = CertificateParams::default();
    p.serial_number = Some(SerialNumber::from_slice(&random_serial()?));
    p.not_before = to_time(now_ms - NOT_BEFORE_BACKDATE_MS)?;
    p.not_after = to_time(now_ms + ROOT_DAYS * DAY_MS)?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, root_common_name(organization));
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let der = p
        .self_signed(&key)
        .map_err(|e| format!("無法產生根 CA 憑證：{e}"))?
        .der()
        .to_vec();
    CaKey::from_parts(der, key)
}

/// 新的 ECDSA P-256 私鑰：(PKCS#8 PEM, SPKI DER)
pub fn new_p256_key() -> Result<(Zeroizing<String>, Vec<u8>), String> {
    let key =
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|e| format!("無法產生私鑰：{e}"))?;
    Ok((
        Zeroizing::new(key.serialize_pem()),
        key.subject_public_key_info(),
    ))
}

// ---------------------------------------------------------------------------
// CSR
// ---------------------------------------------------------------------------

/// 通過簽章驗證的 CSR：只取公鑰與 CN（擴充欄位一律不使用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Csr {
    pub spki_der: Vec<u8>,
    pub common_name: Option<String>,
}

/// 讀取 PEM 的 CSR（`CERTIFICATE REQUEST`／`NEW CERTIFICATE REQUEST`），驗證自身的簽章（證明持有私鑰）
pub fn parse_csr(text: &str) -> Result<Csr, String> {
    let der = pem_decode_all(text, None)?
        .into_iter()
        .find(|(label, _)| label == "CERTIFICATE REQUEST" || label == "NEW CERTIFICATE REQUEST")
        .ok_or("檔案中找不到 CSR（PEM 的 CERTIFICATE REQUEST）")?
        .1;
    let (rest, csr) =
        X509CertificationRequest::from_der(&der).map_err(|e| format!("CSR 無法解析：{e}"))?;
    if !rest.is_empty() {
        return Err("CSR 的 DER 結尾有多餘的資料".to_string());
    }
    csr.verify_signature()
        .map_err(|_| "CSR 的簽章無效（無法證明持有私鑰）".to_string())?;
    let info = &csr.certification_request_info;
    Ok(Csr {
        spki_der: info.subject_pki.raw.to_vec(),
        common_name: info
            .subject
            .iter_common_name()
            .next()
            .and_then(|a| a.as_str().ok())
            .map(str::to_string),
    })
}

// ---------------------------------------------------------------------------
// CA 主機上的檔案：私鑰權限、簽發紀錄
// ---------------------------------------------------------------------------

/// CA 私鑰檔的權限過寬（群組或其他使用者有任何權限）時的說明；非 Unix 時一律 `None`
pub fn ca_key_mode_problem(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o7777;
        (mode & 0o077 != 0).then(|| {
            format!(
                "私鑰 {} 的權限過寬（{mode:04o}），應為 0600（chmod 600 {}）",
                path.display(),
                path.display()
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// CA 私鑰所在目錄的簽發紀錄檔
pub fn journal_path(ca_key: &Path) -> PathBuf {
    ca_key
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join(JOURNAL_FILE)
}

/// 附加一筆紀錄（一行 JSON；新檔 0600）。寫不進去時回 `Err`，呼叫端不寫出憑證或 CRL
pub fn journal_append(path: &Path, entry: &Value) -> Result<(), String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("無法寫入簽發紀錄 {}：{e}", path.display()))?;
    f.write_all(format!("{entry}\n").as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("無法寫入簽發紀錄 {}：{e}", path.display()))
}

/// 紀錄中這個 CA（以公鑰雜湊辨識）最大的 CRL 版本
pub fn journal_last_crl_number(path: &Path, issuer_spki_sha256: &str) -> Option<u64> {
    let f = std::fs::File::open(path).ok()?;
    std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|v| v["action"] == "gencrl" && v["issuer_spki_sha256"] == issuer_spki_sha256)
        .filter_map(|v| v["crl_number"].as_u64())
        .max()
}

/// 紀錄的一筆（時間、執行者、動作與內容）
pub fn journal_entry(action: &str, actor: &str, now_ms: i64, mut details: Value) -> Value {
    if let Some(m) = details.as_object_mut() {
        m.insert(
            "at".into(),
            json!(kairos_proto::enroll::format_rfc3339_ms(now_ms)),
        );
        m.insert("actor".into(), json!(actor));
        m.insert("action".into(), json!(action));
        m.insert("kairos_version".into(), json!(env!("CARGO_PKG_VERSION")));
    }
    details
}

/// 離線 CRL 的版本：Unix 秒數（單調遞增、不需要狀態），且大於紀錄中同一個 CA 的最後一版
pub fn next_offline_crl_number(journal: &Path, issuer_spki_sha256: &str, now_ms: i64) -> u64 {
    let by_time = u64::try_from(now_ms.div_euclid(1000)).unwrap_or(1).max(1);
    match journal_last_crl_number(journal, issuer_spki_sha256) {
        Some(last) if last >= by_time => last + 1,
        _ => by_time,
    }
}

// ---------------------------------------------------------------------------
// 離線模式的撤銷清單（kairos-revocations/v1）
// ---------------------------------------------------------------------------

/// 匯出資料庫中「由 `ca` 簽發、撤銷已生效、憑證過期未超過 1 天」的撤銷（第 6 節離線模式的 CRL 來源）
pub fn export_revocations(conn: &Connection, ca: &CertInfo, now_ms: i64) -> Result<Value, String> {
    let rows = ps::list_certs(conn, 10_000_000, None).map_err(|e| e.to_string())?;
    let mut revoked: Vec<Value> = Vec::new();
    for c in rows {
        let (Some(at), true) = (c.revoked_at, c.revoked_effective(now_ms)) else {
            continue;
        };
        if c.not_after <= now_ms - CRL_KEEP_EXPIRED_MS {
            continue;
        }
        let issued_by_ca = parse_cert(&c.cert_der).is_ok_and(|i| i.issuer_raw == ca.subject_raw);
        if !issued_by_ca {
            continue;
        }
        revoked.push(json!({
            "serial": c.serial,
            "agent_id": c.agent_id,
            "revoked_at": kairos_proto::enroll::format_rfc3339_ms(at),
            "reason": c.revocation_reason.as_deref().unwrap_or("unspecified"),
        }));
    }
    revoked.sort_by(|a, b| a["serial"].as_str().cmp(&b["serial"].as_str()));
    Ok(json!({
        "schema": REVOCATIONS_SCHEMA,
        "exported_at": kairos_proto::enroll::format_rfc3339_ms(now_ms),
        "issuer": ca.subject_text,
        "issuer_spki_sha256": hex(&kairos_proto::enroll::sha256(&ca.spki_der)),
        "revoked": revoked,
    }))
}

/// 讀取 `kairos-revocations/v1`：必須是同一個 CA（公鑰雜湊相同）匯出的
pub fn parse_revocations(v: &Value, ca: &CaKey) -> Result<Vec<RevokedEntry>, String> {
    if v["schema"] != REVOCATIONS_SCHEMA {
        return Err(format!(
            "撤銷清單不是 {REVOCATIONS_SCHEMA}（以 kairos pki revocations export 產生）"
        ));
    }
    if v["issuer_spki_sha256"].as_str() != Some(ca.spki_sha256_hex().as_str()) {
        return Err(format!(
            "撤銷清單是為另一個 CA 匯出的（{}），與 --ca-cert（{}）不同",
            v["issuer"].as_str().unwrap_or("?"),
            ca.info.subject_text
        ));
    }
    let mut out = Vec::new();
    for item in v["revoked"].as_array().cloned().unwrap_or_default() {
        let serial = item["serial"]
            .as_str()
            .filter(|s| super::x509::serial_bytes(s).is_some())
            .ok_or("撤銷清單中有不合法的序號")?
            .to_ascii_uppercase();
        let at = item["revoked_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .ok_or_else(|| format!("序號 {serial} 的 revoked_at 不是 RFC 3339 時間"))?
            .timestamp_millis();
        let reason = item["reason"]
            .as_str()
            .and_then(Reason::parse)
            .unwrap_or(Reason::Unspecified);
        out.push(RevokedEntry {
            serial,
            revoked_at_ms: at,
            reason,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// kairos pki init
// ---------------------------------------------------------------------------

/// `pki init` 的輸入
#[derive(Debug, Clone)]
pub struct InitPlan {
    pub organization: String,
    pub names: Vec<ServerName>,
    /// 根 CA 私鑰與憑證的目錄（不得在 `data_dir` 之下；之後移到離線保存）
    pub root_dir: PathBuf,
    pub data_dir: PathBuf,
    pub server_days: i64,
}

/// `pki init` 的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOutcome {
    pub root_key: PathBuf,
    pub root_cert: PathBuf,
    /// Pulse 信任的根 CA（`pki.server_ca_path`）
    pub server_ca: PathBuf,
    pub server_cert: PathBuf,
    pub server_key: PathBuf,
    /// 8443 的 mTLS 信任錨（`auth.client_ca`）：Kairos 寫出的 `data/pki/issuing-cas.pem`
    pub client_ca: PathBuf,
    pub root_subject: String,
    pub root_not_after_ms: i64,
    /// 安裝字串的 CA 指紋（`sha256:…`）
    pub pin: String,
    pub server_serial: String,
    pub server_not_after_ms: i64,
    pub ca_id: i64,
    pub issuing_subject: String,
    pub issuing_not_after_ms: i64,
}

/// `pki init` 的失敗
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitError {
    /// 已經初始化過、檔案已存在（不覆寫）
    Conflict(String),
    /// 參數不合法
    Usage(String),
    Failed(String),
}

/// 新安裝的 PKI：根 CA（P-384）→ 伺服器憑證（P-256，`serverAuth`）＋ 代理簽發 CA（P-256，安裝並啟用）。
///
/// 只用於全新的安裝：資料庫已有簽發 CA，或任何一個要寫出的檔案已存在時拒絕（不覆寫）。寫出：
/// - `<root_dir>/root-ca.key`（0600，目錄 0700）、`<root_dir>/root-ca.pem`
/// - `data/pki/root-ca.pem`（`pki.server_ca_path`）、`data/pki/server.pem`、`data/pki/server.key`（0600）
/// - `data/pki/issuing-1.key`、`pki_cas` 的第一筆（`active`）、`data/pki/issuing-cas.pem`（`auth.client_ca`）
pub fn init(conn: &Connection, plan: &InitPlan, now_ms: i64) -> Result<InitOutcome, InitError> {
    if plan.names.is_empty() {
        return Err(InitError::Usage(
            "至少需要一個 --name（Pulse 連線時使用的伺服器名稱或 IP）".into(),
        ));
    }
    if plan.organization.trim().is_empty() {
        return Err(InitError::Usage(
            "組織名稱不可為空（--organization）".into(),
        ));
    }
    let failed = |e: String| InitError::Failed(e);
    if !ps::list_cas(conn)
        .map_err(|e| failed(e.to_string()))?
        .is_empty()
    {
        return Err(InitError::Conflict(
            "資料庫中已有簽發 CA：kairos pki init 只用於新安裝。簽發 CA 的輪替用 kairos pki issuing-ca new（PKI.md 第 4 節）"
                .into(),
        ));
    }
    let pki_dir = super::pki_dir(&plan.data_dir);
    let out = InitOutcome {
        root_key: plan.root_dir.join(ROOT_KEY_FILE),
        root_cert: plan.root_dir.join(ROOT_CERT_FILE),
        server_ca: pki_dir.join(ROOT_CERT_FILE),
        server_cert: pki_dir.join(SERVER_CERT_FILE),
        server_key: pki_dir.join(SERVER_KEY_FILE),
        client_ca: super::issuing_cas_path(&plan.data_dir),
        root_subject: String::new(),
        root_not_after_ms: 0,
        pin: String::new(),
        server_serial: String::new(),
        server_not_after_ms: 0,
        ca_id: 0,
        issuing_subject: String::new(),
        issuing_not_after_ms: 0,
    };
    let existing: Vec<String> = [
        &out.root_key,
        &out.root_cert,
        &out.server_ca,
        &out.server_cert,
        &out.server_key,
    ]
    .iter()
    .filter(|p| p.exists())
    .map(|p| p.display().to_string())
    .collect();
    if !existing.is_empty() {
        return Err(InitError::Conflict(format!(
            "下列檔案已存在，不覆寫：{}",
            existing.join("、")
        )));
    }
    // 根 CA 目錄：0700；不得在資料目錄之下（資料目錄由 Kairos 服務帳號擁有、會被備份與診斷包列出）
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(&plan.root_dir).map_err(|e| {
        failed(format!(
            "無法建立根 CA 目錄 {}：{e}",
            plan.root_dir.display()
        ))
    })?;
    super::ensure_pki_dir(&plan.data_dir)
        .map_err(|e| failed(format!("無法建立 {}：{e}", pki_dir.display())))?;
    if let (Ok(r), Ok(d)) = (plan.root_dir.canonicalize(), plan.data_dir.canonicalize())
        && r.starts_with(&d)
    {
        return Err(InitError::Usage(format!(
            "根 CA 目錄 {} 不可以在資料目錄 {} 之下（根 CA 私鑰之後要移到離線保存）",
            plan.root_dir.display(),
            plan.data_dir.display()
        )));
    }

    // 1. 根 CA
    let root = new_root(&plan.organization, now_ms).map_err(failed)?;
    super::write_private_key(&out.root_key, &root.key_pem()).map_err(|e| {
        failed(format!(
            "無法寫入根 CA 私鑰 {}：{e}",
            out.root_key.display()
        ))
    })?;
    let root_pem = pem_encode("CERTIFICATE", &root.cert_der);
    for p in [&out.root_cert, &out.server_ca] {
        super::write_public_atomically(p, root_pem.as_bytes())
            .map_err(|e| failed(format!("無法寫入 {}：{e}", p.display())))?;
    }

    // 2. 伺服器憑證（8443、8444、9443 共用）
    let (server_key_pem, server_spki) = new_p256_key().map_err(failed)?;
    let server = root
        .sign_server(&server_spki, &plan.names, plan.server_days, now_ms)
        .map_err(failed)?;
    super::write_private_key(&out.server_key, &server_key_pem)
        .map_err(|e| failed(format!("無法寫入 {}：{e}", out.server_key.display())))?;
    super::write_public_atomically(
        &out.server_cert,
        pem_encode("CERTIFICATE", &server.der).as_bytes(),
    )
    .map_err(|e| failed(format!("無法寫入 {}：{e}", out.server_cert.display())))?;

    // 3. 代理簽發 CA：與 kairos pki issuing-ca new → 以根 CA 簽發 → install 相同（manage.rs）
    let (_, n) = super::manage::create(conn, &plan.data_dir, plan.organization.trim(), now_ms)
        .map_err(failed)?;
    let csr = parse_csr(&n.csr_pem).map_err(failed)?;
    let issuing = root
        .sign_issuing_ca(
            &csr.spki_der,
            csr.common_name.as_deref().unwrap_or_default(),
            ISSUING_DAYS,
            now_ms,
        )
        .map_err(failed)?;
    let installed = super::manage::install(
        conn,
        &plan.data_dir,
        &pem_encode("CERTIFICATE", &issuing.der),
        now_ms,
    )
    .map_err(failed)?;
    // 寫出信任錨 data/pki/issuing-cas.pem（auth.client_ca）：與常駐行程同一個函式
    crate::enroll::pki_mgr::PkiManager::new(&plan.data_dir, crate::config::PkiMode::Online)
        .sync_cas(conn)
        .map_err(|e| failed(format!("無法寫出簽發 CA 的信任錨：{e:?}")))?;

    Ok(InitOutcome {
        root_subject: root.info.subject_text.clone(),
        root_not_after_ms: root.info.not_after_ms,
        pin: kairos_proto::enroll::ca_pin_from_spki(&root.info.spki_der),
        server_serial: server.serial,
        server_not_after_ms: server.not_after_ms,
        ca_id: installed.ca_id,
        issuing_subject: installed.subject,
        issuing_not_after_ms: installed.not_after_ms,
        ..out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::issue::IssuingCa;
    use crate::pki::x509::verify_cert_signature;

    const NOW: i64 = 1_760_000_000_000;
    const ID: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";

    fn csr_pem(key: &KeyPair, cn: &str) -> String {
        let mut p = CertificateParams::default();
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, cn);
        // CSR 夾帶的擴充欄位（CA:TRUE、額外的 SAN）不得被複製
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.subject_alt_names = vec![SanType::DnsName("evil.example".try_into().unwrap())];
        p.serialize_request(key).unwrap().pem().unwrap()
    }

    #[test]
    fn server_names_are_parsed_and_validated() {
        assert_eq!(
            ServerName::parse("Monitor.Example.COM.").unwrap(),
            ServerName::Dns("monitor.example.com".into())
        );
        assert_eq!(
            ServerName::parse("10.0.0.5").unwrap(),
            ServerName::Ip("10.0.0.5".parse().unwrap())
        );
        assert_eq!(
            ServerName::parse("IP:[fd00::5]").unwrap(),
            ServerName::Ip("fd00::5".parse().unwrap())
        );
        assert_eq!(
            ServerName::parse("DNS:kairos").unwrap().text(),
            "DNS:kairos"
        );
        for bad in [
            "",
            "*.example.com",
            "-a.example",
            "a..b",
            "IP:example.com",
            "10.0.0.256",
        ] {
            assert!(ServerName::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn root_ca_is_p384_with_ca_extensions() {
        let root = new_root("Example Org", NOW).unwrap();
        assert_eq!(root.key_alg, "ECDSA P-384");
        let i = &root.info;
        assert!(i.is_ca);
        assert_eq!(i.path_len, Some(1));
        let ku = i.key_usage.unwrap();
        assert!(ku.key_cert_sign && ku.crl_sign && !ku.digital_signature);
        assert_eq!(
            i.common_names,
            vec!["Example Org Kairos Root CA".to_string()]
        );
        assert_eq!(root_common_name(" Kairos "), "Kairos Root CA");
        assert_eq!(i.not_after_ms, NOW + ROOT_DAYS * DAY_MS);
        assert_eq!(i.serial.len(), 32);
        // PEM 往返：CaKey::load 讀回同一把金鑰
        let again =
            CaKey::load(&pem_encode("CERTIFICATE", &root.cert_der), &root.key_pem()).unwrap();
        assert_eq!(again.info.spki_der, i.spki_der);
    }

    #[test]
    fn server_certificate_has_san_eku_and_ca_false() {
        let root = new_root("Example Org", NOW).unwrap();
        let (_, spki) = new_p256_key().unwrap();
        let names = vec![
            ServerName::parse("monitor.example.com").unwrap(),
            ServerName::parse("10.0.0.5").unwrap(),
        ];
        let s = root.sign_server(&spki, &names, SERVER_DAYS, NOW).unwrap();
        let i = parse_cert(&s.der).unwrap();
        assert!(!i.is_ca);
        assert_eq!(i.dns_sans, vec!["monitor.example.com".to_string()]);
        assert_eq!(i.ip_sans, vec!["10.0.0.5".parse::<IpAddr>().unwrap()]);
        assert_eq!(i.common_names, vec!["monitor.example.com".to_string()]);
        assert_eq!(i.eku, Some((false, true, false)));
        assert!(i.key_usage.unwrap().digital_signature);
        assert_eq!(i.not_before_ms, NOW - NOT_BEFORE_BACKDATE_MS);
        assert_eq!(i.not_after_ms, NOW + SERVER_DAYS * DAY_MS);
        assert!(verify_cert_signature(&s.der, &root.cert_der));
        // rustls（webpki）接受它當作伺服器憑證：沿用 Pulse 的檢查（CA 簽發、SAN 相符、EKU serverAuth）
        let anchor = CertificateDer::from(root.cert_der.clone());
        let anchors = [webpki::anchor_from_trusted_cert(&anchor).unwrap()];
        let ee = CertificateDer::from(s.der.clone());
        let cert = webpki::EndEntityCert::try_from(&ee).unwrap();
        let t = tokio_rustls::rustls::pki_types::UnixTime::since_unix_epoch(
            std::time::Duration::from_millis((NOW + DAY_MS) as u64),
        );
        cert.verify_for_usage(
            webpki::ALL_VERIFICATION_ALGS,
            &anchors,
            &[],
            t,
            webpki::KeyUsage::server_auth(),
            None,
            None,
        )
        .unwrap();
        let name =
            tokio_rustls::rustls::pki_types::ServerName::try_from("monitor.example.com").unwrap();
        cert.verify_is_valid_for_subject_name(&name).unwrap();
        // 範圍
        assert!(root.sign_server(&spki, &names, 826, NOW).is_err());
        assert!(root.sign_server(&spki, &[], 30, NOW).is_err());
    }

    #[test]
    fn issuing_ca_signed_by_root_passes_the_install_checks() {
        let root = new_root("Example Org", NOW).unwrap();
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let csr = parse_csr(&csr_pem(&key, "Example Org Kairos Agent Issuing CA 2")).unwrap();
        let c = root
            .sign_issuing_ca(
                &csr.spki_der,
                csr.common_name.as_deref().unwrap(),
                ISSUING_DAYS,
                NOW,
            )
            .unwrap();
        let ca = IssuingCa::load(2, &c.der, &key.serialize_pem()).unwrap();
        assert_eq!(ca.info.path_len, Some(0));
        assert!(ca.info.dns_sans.is_empty(), "CSR 的 SAN 不得被複製");
        // 簽發 CA（pathlen:0）不能再簽下層 CA；P-384 的 CSR 不能當簽發 CA
        let issuing =
            CaKey::load(&pem_encode("CERTIFICATE", &c.der), &key.serialize_pem()).unwrap();
        assert!(
            issuing
                .sign_issuing_ca(&csr.spki_der, "x", 10, NOW)
                .unwrap_err()
                .contains("pathlen:0")
        );
        let p384 = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        assert!(
            root.sign_issuing_ca(&p384.subject_public_key_info(), "x", 10, NOW)
                .is_err()
        );
        // 用戶端憑證：與線上簽發相同的範本，可以由簽發 CA 簽
        let ck = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let leaf = issuing
            .sign_client(&ck.subject_public_key_info(), ID, CLIENT_DAYS, NOW)
            .unwrap();
        let li = parse_cert(&leaf.der).unwrap();
        assert_eq!(li.dns_sans, vec![ID.to_string()]);
        assert_eq!(li.eku, Some((true, false, false)));
        assert!(!li.is_ca);
        assert!(
            issuing
                .sign_client(&ck.subject_public_key_info(), "not-a-uuid", 30, NOW)
                .is_err()
        );
    }

    #[test]
    fn existing_rsa_root_is_accepted() {
        // 既有的 RSA 根 CA 照用（PKI.md 第 1 節第 4 項）：openssl genpkey 產生的 PKCS#8 私鑰
        // （ring 後端不能產生 RSA 金鑰，以 testdata 的公開測試金鑰自簽 CA 憑證）
        const KEY: &str = include_str!("../../testdata/pki/rsa-root.key");
        let key = KeyPair::from_pem(KEY).unwrap();
        let mut p = CertificateParams::default();
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name
            .push(DnType::CommonName, "Legacy RSA Root");
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        p.not_before = to_time(NOW - DAY_MS).unwrap();
        p.not_after = to_time(NOW + ROOT_DAYS * DAY_MS).unwrap();
        let der = p.self_signed(&key).unwrap().der().to_vec();
        let ca = CaKey::load(&pem_encode("CERTIFICATE", &der), KEY).unwrap();
        assert_eq!(ca.key_alg, "RSA");
        // 伺服器憑證與代理簽發 CA 都可以由 RSA 根 CA 簽發
        let (_, spki) = new_p256_key().unwrap();
        let s = ca
            .sign_server(&spki, &[ServerName::parse("kairos").unwrap()], 30, NOW)
            .unwrap();
        assert!(verify_cert_signature(&s.der, &ca.cert_der));
        let ik = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let ic = ca
            .sign_issuing_ca(&ik.subject_public_key_info(), "Issuing", ISSUING_DAYS, NOW)
            .unwrap();
        assert!(IssuingCa::load(1, &ic.der, &ik.serialize_pem()).is_ok());
        assert!(verify_cert_signature(&ic.der, &ca.cert_der));
        // 私鑰與憑證不成對
        let other = new_root("x", NOW).unwrap();
        assert!(
            CaKey::load(&pem_encode("CERTIFICATE", &other.cert_der), KEY)
                .unwrap_err()
                .contains("不成對")
        );
    }

    fn test_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::store::sqlite::migrate(&mut conn, crate::store::sqlite::MIGRATIONS).unwrap();
        conn
    }

    #[test]
    fn csr_signature_and_format_are_checked() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let pem = csr_pem(&key, "k");
        let c = parse_csr(&pem).unwrap();
        assert_eq!(c.spki_der, key.subject_public_key_info());
        assert_eq!(c.common_name.as_deref(), Some("k"));
        assert!(parse_csr("not a csr").is_err());
        // 竄改內容：簽章不符
        let mut der = pem_decode_all(&pem, None).unwrap().remove(0).1;
        let n = der.len();
        der[n / 2] ^= 0x01;
        assert!(parse_csr(&pem_encode("CERTIFICATE REQUEST", &der)).is_err());
    }

    #[test]
    fn offline_crl_numbers_and_journal() {
        let dir = std::env::temp_dir().join(format!("kairos-ca-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let j = journal_path(&dir.join("ca.key"));
        assert_eq!(j, dir.join(JOURNAL_FILE));
        assert_eq!(next_offline_crl_number(&j, "aa", NOW), (NOW / 1000) as u64);
        journal_append(
            &j,
            &journal_entry(
                "gencrl",
                "test",
                NOW,
                json!({"issuer_spki_sha256": "aa", "crl_number": (NOW / 1000) as u64 + 5}),
            ),
        )
        .unwrap();
        journal_append(
            &j,
            &journal_entry(
                "gencrl",
                "test",
                NOW,
                json!({"issuer_spki_sha256": "bb", "crl_number": 1}),
            ),
        )
        .unwrap();
        // 時鐘倒退時仍遞增；其他 CA 的紀錄不影響
        assert_eq!(
            next_offline_crl_number(&j, "aa", NOW),
            (NOW / 1000) as u64 + 6
        );
        assert_eq!(next_offline_crl_number(&j, "bb", NOW), (NOW / 1000) as u64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&j).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let k = dir.join("ca.key");
            std::fs::write(&k, "x").unwrap();
            std::fs::set_permissions(&k, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(ca_key_mode_problem(&k).unwrap().contains("0644"));
            std::fs::set_permissions(&k, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(ca_key_mode_problem(&k), None);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revocations_round_trip_into_an_offline_crl() {
        let root = new_root("Example Org", NOW).unwrap();
        let other = new_root("Other", NOW).unwrap();
        let entries = json!({
            "schema": REVOCATIONS_SCHEMA,
            "issuer": root.info.subject_text,
            "issuer_spki_sha256": root.spki_sha256_hex(),
            "revoked": [{"serial": "5E0B2C77A1D94F3E8B6A0C21D4F7E913", "revoked_at": "2025-10-09T08:53:20.000Z", "reason": "keyCompromise"}],
        });
        let list = parse_revocations(&entries, &root).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, Reason::KeyCompromise);
        assert!(parse_revocations(&entries, &other).is_err());
        let der = root
            .sign_crl(42, NOW, NOW + CRL_DAYS * DAY_MS, &list)
            .unwrap();
        let (_, crl) = x509_parser::parse_x509_crl(&der).unwrap();
        assert_eq!(crl.crl_number().map(|n| n.to_string()), Some("42".into()));
        assert_eq!(crl.iter_revoked_certificates().count(), 1);
        webpki::OwnedCertRevocationList::from_der(&der).unwrap();
    }

    #[test]
    fn init_creates_root_server_and_active_issuing_ca_once() {
        let base = std::env::temp_dir().join(format!("kairos-pki-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let conn = test_db();
        let plan = InitPlan {
            organization: "Example Org".into(),
            names: vec![
                ServerName::parse("monitor.example.com").unwrap(),
                ServerName::parse("10.0.0.5").unwrap(),
            ],
            root_dir: base.join("root-ca"),
            data_dir: data.clone(),
            server_days: SERVER_DAYS,
        };
        let o = init(&conn, &plan, NOW).unwrap();
        assert_eq!(o.ca_id, 1);
        assert!(o.pin.starts_with("sha256:"));
        let cas = ps::list_cas(&conn).unwrap();
        assert_eq!(cas.len(), 1);
        assert_eq!(cas[0].state, ps::CA_ACTIVE);
        // 簽發 CA 鏈到根 CA；伺服器憑證鏈到根 CA
        let root_der = pem_decode_all(&std::fs::read_to_string(&o.server_ca).unwrap(), None)
            .unwrap()
            .remove(0)
            .1;
        assert!(verify_cert_signature(
            cas[0].cert_der.as_ref().unwrap(),
            &root_der
        ));
        let server_der = pem_decode_all(&std::fs::read_to_string(&o.server_cert).unwrap(), None)
            .unwrap()
            .remove(0)
            .1;
        assert!(verify_cert_signature(&server_der, &root_der));
        // auth.client_ca：Kairos 寫出的信任錨含這個簽發 CA
        let anchors = std::fs::read_to_string(&o.client_ca).unwrap();
        assert_eq!(
            pem_decode_all(&anchors, Some("CERTIFICATE")).unwrap().len(),
            1
        );
        // 根 CA 的公鑰指紋與 8444 讀取的相同
        let sca = crate::enroll::pki_mgr::load_server_ca(&o.server_ca.to_string_lossy()).unwrap();
        assert_eq!(sca.pin, o.pin);
        // 私鑰權限
        assert_eq!(super::super::key_permission_problem(&o.server_key), None);
        assert_eq!(ca_key_mode_problem(&o.root_key), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(&plan.root_dir)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o700);
        }
        // 第二次：拒絕（不覆寫）
        assert!(matches!(
            init(&conn, &plan, NOW),
            Err(InitError::Conflict(_))
        ));
        // 根 CA 目錄不可在資料目錄之下
        let conn2 = test_db();
        let data2 = base.join("data2");
        std::fs::create_dir_all(&data2).unwrap();
        let bad = InitPlan {
            root_dir: data2.join("root"),
            data_dir: data2.clone(),
            ..plan.clone()
        };
        assert!(matches!(init(&conn2, &bad, NOW), Err(InitError::Usage(_))));
        let _ = std::fs::remove_dir_all(&base);
    }
}
