//! 簽發器：依範本簽發用戶端憑證、產生 CRL，以及代理簽發 CA 的金鑰與 CSR（`kairos/PKI.md` 第 3～5 節）。
//!
//! 用戶端憑證的格式（第 5.1 節）：
//!
//! | 欄位 | 值 |
//! |---|---|
//! | 序號 | 16 bytes CSPRNG，最高位元清為 0、第一個 byte 不為 0（DER 不會補 `00`，大寫十六進位恰好 32 個字元） |
//! | subject | `CN=<agent_id>` |
//! | SAN | 恰好一個 `DNS:<agent_id>` |
//! | `notBefore` | 簽發時間 − 10 分鐘 |
//! | `notAfter` | 簽發時間 ＋ 有效天數，且不晚於簽發 CA 的 `notAfter` |
//! | basicConstraints | critical，`CA:FALSE` |
//! | keyUsage | critical，`digitalSignature` |
//! | EKU | `clientAuth` |
//! | SKI、AKI | 有 |
//!
//! CSR 的擴充欄位一律不複製：只取 CSR 的公鑰。

use rcgen::{
    CertificateParams, CertificateRevocationListParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
    PublicKeyData, RevocationReason, RevokedCertParams, SanType, SerialNumber,
    SubjectPublicKeyInfo,
};
use ring::rand::{SecureRandom, SystemRandom};
use time::OffsetDateTime;
use tokio_rustls::rustls::pki_types::CertificateDer;

use super::x509::{CertInfo, parse_cert, serial_bytes};

/// `notBefore` 往前推的時間（容許 ±5 分鐘的時鐘偏差）
pub const NOT_BEFORE_BACKDATE_MS: i64 = 10 * 60 * 1000;

/// 簽發 CA 的有效期限要求：安裝時至少剩 2 年（`kairos/PKI.md` 第 4 節）
pub const MIN_ISSUING_CA_REMAINING_MS: i64 = 2 * 365 * 86_400_000;

/// 撤銷原因（`pki_certificates.revocation_reason`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Unspecified,
    KeyCompromise,
    Superseded,
    CessationOfOperation,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Unspecified => "unspecified",
            Reason::KeyCompromise => "keyCompromise",
            Reason::Superseded => "superseded",
            Reason::CessationOfOperation => "cessationOfOperation",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            Reason::Unspecified,
            Reason::KeyCompromise,
            Reason::Superseded,
            Reason::CessationOfOperation,
        ]
        .into_iter()
        .find(|r| r.as_str() == s)
    }

    fn rcgen(self) -> RevocationReason {
        match self {
            Reason::Unspecified => RevocationReason::Unspecified,
            Reason::KeyCompromise => RevocationReason::KeyCompromise,
            Reason::Superseded => RevocationReason::Superseded,
            Reason::CessationOfOperation => RevocationReason::CessationOfOperation,
        }
    }
}

/// CRL 收錄的一筆撤銷
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokedEntry {
    /// 大寫十六進位
    pub serial: String,
    pub revoked_at_ms: i64,
    pub reason: Reason,
}

/// 簽發的憑證
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    pub der: Vec<u8>,
    /// 大寫十六進位（32 個字元）
    pub serial: String,
    pub not_before_ms: i64,
    pub not_after_ms: i64,
}

/// 16 bytes 的隨機序號：最高位元清為 0（正整數）、第一個 byte 不為 0
pub fn random_serial() -> Result<[u8; 16], String> {
    let mut b = [0u8; 16];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| "無法取得隨機數".to_string())?;
    b[0] &= 0x7f;
    if b[0] == 0 {
        b[0] = 1;
    }
    Ok(b)
}

fn to_time(ms: i64) -> Result<OffsetDateTime, String> {
    OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000))
        .map_err(|e| format!("時間超出範圍：{e}"))
}

/// 線上（或在離線 CA 主機上以 `kairos pki sign-bundle`）簽發用的簽發 CA：憑證與私鑰
pub struct IssuingCa {
    pub ca_id: i64,
    pub cert_der: Vec<u8>,
    pub info: CertInfo,
    issuer: Issuer<'static, KeyPair>,
}

impl std::fmt::Debug for IssuingCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuingCa")
            .field("ca_id", &self.ca_id)
            .field("subject", &self.info.subject_text)
            .finish_non_exhaustive()
    }
}

impl IssuingCa {
    /// 由 CA 憑證（DER）與私鑰（PKCS#8 PEM）建立；檢查兩者成對、私鑰為 P-256、憑證可簽發與產生 CRL
    pub fn load(ca_id: i64, cert_der: &[u8], key_pem: &str) -> Result<Self, String> {
        let info = parse_cert(cert_der)?;
        let key = KeyPair::from_pem(key_pem).map_err(|e| format!("私鑰無法讀取：{e}"))?;
        if !std::ptr::eq(key.algorithm(), &PKCS_ECDSA_P256_SHA256) {
            return Err("簽發 CA 的私鑰必須是 ECDSA P-256".to_string());
        }
        if key.subject_public_key_info() != info.spki_der {
            return Err("簽發 CA 的憑證與私鑰不成對".to_string());
        }
        check_issuing_ca(&info)?;
        let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(cert_der.to_vec()), key)
            .map_err(|e| format!("簽發 CA 的憑證無法使用：{e}"))?;
        Ok(Self {
            ca_id,
            cert_der: cert_der.to_vec(),
            info,
            issuer,
        })
    }

    /// 依範本簽發用戶端憑證（只使用 CSR 的公鑰）。`serial` 為 [`random_serial`] 的結果
    pub fn sign_client(
        &self,
        spki_der: &[u8],
        agent_id: &str,
        now_ms: i64,
        lifetime_ms: i64,
        serial: [u8; 16],
    ) -> Result<Issued, String> {
        let public =
            SubjectPublicKeyInfo::from_der(spki_der).map_err(|e| format!("公鑰無法使用：{e}"))?;
        let not_before_ms = now_ms - NOT_BEFORE_BACKDATE_MS;
        let not_after_ms = (now_ms + lifetime_ms).min(self.info.not_after_ms);
        if not_after_ms <= now_ms {
            return Err("簽發 CA 已過期，無法簽發".to_string());
        }
        let mut p = CertificateParams::default();
        p.serial_number = Some(SerialNumber::from_slice(&serial));
        p.not_before = to_time(not_before_ms)?;
        p.not_after = to_time(not_after_ms)?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, agent_id);
        p.distinguished_name = dn;
        p.subject_alt_names = vec![SanType::DnsName(
            agent_id
                .to_string()
                .try_into()
                .map_err(|e| format!("agent_id 無法放入 SAN：{e}"))?,
        )];
        p.is_ca = IsCa::ExplicitNoCa;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        p.use_authority_key_identifier_extension = true;
        let cert = p
            .signed_by(&public, &self.issuer)
            .map_err(|e| format!("簽發失敗：{e}"))?;
        let der = cert.der().to_vec();
        let info = parse_cert(&der)?;
        Ok(Issued {
            der,
            serial: info.serial,
            // X.509 的時間只到秒
            not_before_ms: info.not_before_ms,
            not_after_ms: info.not_after_ms,
        })
    }

    /// 產生 CRL（DER）
    pub fn sign_crl(
        &self,
        crl_number: u64,
        this_update_ms: i64,
        next_update_ms: i64,
        revoked: &[RevokedEntry],
    ) -> Result<Vec<u8>, String> {
        let mut revoked_certs = Vec::with_capacity(revoked.len());
        for r in revoked {
            revoked_certs.push(RevokedCertParams {
                serial_number: SerialNumber::from_slice(
                    &serial_bytes(&r.serial).ok_or_else(|| format!("序號 {} 不合法", r.serial))?,
                ),
                revocation_time: to_time(r.revoked_at_ms)?,
                reason_code: Some(r.reason.rcgen()),
                invalidity_date: None,
            });
        }
        let params = CertificateRevocationListParams {
            this_update: to_time(this_update_ms)?,
            next_update: to_time(next_update_ms)?,
            crl_number: SerialNumber::from_slice(&crl_number.to_be_bytes()),
            issuing_distribution_point: None,
            revoked_certs,
            key_identifier_method: rcgen::KeyIdMethod::Sha256,
        };
        let crl = params
            .signed_by(&self.issuer)
            .map_err(|e| format!("CRL 產生失敗：{e}"))?;
        Ok(crl.der().to_vec())
    }
}

/// 代理簽發 CA 憑證的要求（`kairos/PKI.md` 第 5.2 節）：`CA:TRUE`、`pathlen:0`、`keyCertSign`＋`cRLSign`、
/// EKU 含 `clientAuth`（不得含 `serverAuth`）、公鑰為 P-256
pub fn check_issuing_ca(info: &CertInfo) -> Result<(), String> {
    if !info.is_ca {
        return Err("不是 CA 憑證（basicConstraints 必須是 CA:TRUE）".to_string());
    }
    if info.path_len != Some(0) {
        return Err("簽發 CA 必須帶 pathLenConstraint = 0（pathlen:0）".to_string());
    }
    match info.key_usage {
        Some(ku) if ku.key_cert_sign && ku.crl_sign => {}
        _ => return Err("keyUsage 必須含 keyCertSign 與 cRLSign".to_string()),
    }
    match info.eku {
        Some((true, false, false)) => {}
        Some((_, true, _)) | Some((_, _, true)) => {
            return Err(
                "EKU 不得含 serverAuth 或 anyExtendedKeyUsage（簽發 CA 只用於用戶端憑證）"
                    .to_string(),
            );
        }
        _ => return Err("extendedKeyUsage 必須含 clientAuth".to_string()),
    }
    if kairos_proto::enroll::p256_point_from_spki(&info.spki_der).is_none() {
        return Err("簽發 CA 的公鑰必須是 ECDSA P-256".to_string());
    }
    Ok(())
}

/// 新的代理簽發 CA：P-256 私鑰（PKCS#8 PEM）與 CSR（PEM）
pub struct NewIssuingCa {
    pub key_pem: String,
    pub csr_pem: String,
    pub spki_der: Vec<u8>,
    pub subject: String,
}

/// 產生代理簽發 CA 的私鑰與 CSR：subject `CN=<組織> Kairos Agent Issuing CA <ca_id>`
pub fn new_issuing_ca(organization: &str, ca_id: i64) -> Result<NewIssuingCa, String> {
    let key =
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|e| format!("無法產生私鑰：{e}"))?;
    let subject = format!("{organization} Kairos Agent Issuing CA {ca_id}");
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, subject.as_str());
    p.distinguished_name = dn;
    let csr = p
        .serialize_request(&key)
        .map_err(|e| format!("無法產生 CSR：{e}"))?;
    Ok(NewIssuingCa {
        key_pem: key.serialize_pem(),
        csr_pem: csr.pem().map_err(|e| format!("無法產生 CSR：{e}"))?,
        spki_der: key.subject_public_key_info(),
        subject: format!("CN={subject}"),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::pki::csr::check_csr;
    use crate::pki::x509::serial_hex;
    use rcgen::{BasicConstraints, PKCS_ECDSA_P384_SHA384};

    const ID: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
    const NOW: i64 = 1_760_000_000_000;
    const DAY: i64 = 86_400_000;

    /// 測試用的 PKI：P-384 根 CA ＋ 以它簽發的代理簽發 CA（P-256）
    pub struct TestPki {
        pub root_key: KeyPair,
        pub root_der: Vec<u8>,
        pub root_params: CertificateParams,
        pub issuing_key_pem: String,
        pub issuing_der: Vec<u8>,
    }

    pub fn test_pki(issuing_days: i64) -> TestPki {
        let root_key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let mut root = CertificateParams::default();
        root.distinguished_name = DistinguishedName::new();
        root.distinguished_name
            .push(DnType::CommonName, "Test Root CA");
        root.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        root.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        root.not_before = to_time(NOW - DAY).unwrap();
        root.not_after = to_time(NOW + 3650 * DAY).unwrap();
        let root_der = root.self_signed(&root_key).unwrap().der().to_vec();
        let ik = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ip = CertificateParams::default();
        ip.distinguished_name = DistinguishedName::new();
        ip.distinguished_name
            .push(DnType::CommonName, "Test Kairos Agent Issuing CA 1");
        ip.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        ip.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ip.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        ip.use_authority_key_identifier_extension = true;
        ip.not_before = to_time(NOW - DAY).unwrap();
        ip.not_after = to_time(NOW + issuing_days * DAY).unwrap();
        let root_issuer = Issuer::from_params(&root, &root_key);
        let issuing_der = ip.signed_by(&ik, &root_issuer).unwrap().der().to_vec();
        TestPki {
            root_key,
            root_der,
            root_params: root,
            issuing_key_pem: ik.serialize_pem(),
            issuing_der,
        }
    }

    fn client_csr(key: &KeyPair, ca_true: bool) -> Vec<u8> {
        let mut p = CertificateParams::new(vec![ID.to_string()]).unwrap();
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, ID);
        if ca_true {
            p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            p.subject_alt_names
                .push(SanType::DnsName("evil.example".try_into().unwrap()));
        }
        p.serialize_request(key).unwrap().der().to_vec()
    }

    #[test]
    fn issues_client_certificates_by_the_template_only() {
        let pki = test_pki(1825);
        let ca = IssuingCa::load(1, &pki.issuing_der, &pki.issuing_key_pem).unwrap();
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let csr = check_csr(&client_csr(&key, false), ID).unwrap();
        let serial = random_serial().unwrap();
        let issued = ca
            .sign_client(&csr.spki_der, ID, NOW, 365 * DAY, serial)
            .unwrap();
        assert_eq!(issued.serial, serial_hex(&serial));
        assert_eq!(issued.serial.len(), 32);
        let info = parse_cert(&issued.der).unwrap();
        assert_eq!(info.dns_sans, vec![ID.to_string()]);
        assert_eq!(info.other_sans, 0);
        assert_eq!(info.common_names, vec![ID.to_string()]);
        assert!(!info.is_ca);
        assert_eq!(info.eku, Some((true, false, false)));
        let ku = info.key_usage.unwrap();
        assert!(ku.digital_signature && !ku.key_cert_sign && !ku.crl_sign);
        assert_eq!(info.spki_der, key.subject_public_key_info());
        assert_eq!(info.not_before_ms, NOW - NOT_BEFORE_BACKDATE_MS);
        assert_eq!(info.not_after_ms, NOW + 365 * DAY);
        assert!(crate::pki::x509::verify_cert_signature(
            &issued.der,
            &pki.issuing_der
        ));
        // 與 8443 的 mTLS 驗證一致：SAN 恰好一個且等於 agent_id
        let der = CertificateDer::from(issued.der.clone());
        assert!(crate::auth::cert_matches_agent_id(&der, ID, Default::default()).is_ok());
    }

    #[test]
    fn csr_extensions_are_never_copied() {
        // 安全審查 #7：CSR 要求 CA:TRUE、serverAuth、額外的 SAN，簽出的憑證都沒有
        let pki = test_pki(1825);
        let ca = IssuingCa::load(1, &pki.issuing_der, &pki.issuing_key_pem).unwrap();
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        // 多一個 SAN 的 CSR 在檢查時就被拒；簽發器本身也只使用公鑰
        assert!(check_csr(&client_csr(&key, true), ID).is_err());
        let issued = ca
            .sign_client(
                &key.subject_public_key_info(),
                ID,
                NOW,
                365 * DAY,
                random_serial().unwrap(),
            )
            .unwrap();
        let info = parse_cert(&issued.der).unwrap();
        assert!(!info.is_ca);
        assert_eq!(info.dns_sans, vec![ID.to_string()]);
        assert_eq!(info.eku, Some((true, false, false)));
    }

    #[test]
    fn not_after_is_capped_by_the_issuing_ca() {
        let pki = test_pki(100);
        let ca = IssuingCa::load(1, &pki.issuing_der, &pki.issuing_key_pem).unwrap();
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let issued = ca
            .sign_client(
                &key.subject_public_key_info(),
                ID,
                NOW,
                365 * DAY,
                random_serial().unwrap(),
            )
            .unwrap();
        assert_eq!(issued.not_after_ms, NOW + 100 * DAY);
        assert!(
            ca.sign_client(
                &key.subject_public_key_info(),
                ID,
                NOW + 200 * DAY,
                365 * DAY,
                random_serial().unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn crl_lists_revoked_serials_and_parses_with_webpki() {
        let pki = test_pki(1825);
        let ca = IssuingCa::load(1, &pki.issuing_der, &pki.issuing_key_pem).unwrap();
        let revoked = vec![RevokedEntry {
            serial: "5E0B2C77A1D94F3E8B6A0C21D4F7E913".into(),
            revoked_at_ms: NOW,
            reason: Reason::CessationOfOperation,
        }];
        let der = ca.sign_crl(7, NOW, NOW + 30 * DAY, &revoked).unwrap();
        let crl = webpki::OwnedCertRevocationList::from_der(&der).unwrap();
        drop(crl);
        let (_, parsed) = x509_parser::parse_x509_crl(&der).unwrap();
        let serials: Vec<String> = parsed
            .iter_revoked_certificates()
            .map(|r| serial_hex(r.raw_serial()))
            .collect();
        assert_eq!(
            serials,
            vec!["5E0B2C77A1D94F3E8B6A0C21D4F7E913".to_string()]
        );
        assert_eq!(parsed.crl_number().map(|n| n.to_string()), Some("7".into()));
        // 空的 CRL 也可以產生
        assert!(ca.sign_crl(8, NOW, NOW + DAY, &[]).is_ok());
    }

    #[test]
    fn issuing_ca_requirements() {
        let pki = test_pki(1825);
        assert!(IssuingCa::load(1, &pki.issuing_der, &pki.issuing_key_pem).is_ok());
        // 私鑰不成對
        let other = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let err = IssuingCa::load(1, &pki.issuing_der, &other.serialize_pem()).unwrap_err();
        assert!(err.contains("不成對"), "{err}");
        // 根 CA 本身（沒有 pathlen:0、沒有 clientAuth、P-384）不能當簽發 CA
        let root = parse_cert(&pki.root_der).unwrap();
        assert!(check_issuing_ca(&root).is_err());
        // 新的簽發 CA：CSR 的公鑰與私鑰相符
        let n = new_issuing_ca("Example Org", 3).unwrap();
        assert_eq!(n.subject, "CN=Example Org Kairos Agent Issuing CA 3");
        let csr =
            super::super::x509::pem_decode_all(&n.csr_pem, Some("CERTIFICATE REQUEST")).unwrap();
        assert_eq!(csr.len(), 1);
        assert_eq!(
            KeyPair::from_pem(&n.key_pem)
                .unwrap()
                .subject_public_key_info(),
            n.spki_der
        );
        let _ = &pki.root_key;
        let _ = &pki.root_params;
    }
}
