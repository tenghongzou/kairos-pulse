//! CSR 的檢查（`crates/kairos-proto/ENROLL.md` 第 15.1 節，`invalid_csr`）。
//!
//! - PKCS#10（DER），自身的簽章有效（證明持有私鑰）；結尾不得有多餘資料。
//! - 公鑰為 ECDSA P-256（未壓縮點）；簽章演算法 `ecdsa-with-SHA256`。
//! - extensionRequest 的 `subjectAltName` **恰好一個** `dNSName`，等於 `agent_id`（小寫）；沒有其他 SAN。
//! - subject 可省略；有 CN 時必須等於 `agent_id`。
//! - 其他要求的擴充欄位一律忽略：簽發時依 Kairos 的範本組出憑證（`kairos/PKI.md` 第 5.1 節），
//!   **不複製** CSR 的任何擴充欄位（例如 `CA:TRUE`、額外的 SAN、`serverAuth`）。

use kairos_proto::enroll::{p256_point_from_spki, sha256};
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::oid_registry::OID_SIG_ECDSA_WITH_SHA256;
use x509_parser::prelude::FromDer;

/// 通過檢查的 CSR
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedCsr {
    /// CSR 公鑰的 SubjectPublicKeyInfo（DER）
    pub spki_der: Vec<u8>,
    /// CSR（DER）的 SHA-256（冪等鍵）
    pub csr_sha256: [u8; 32],
    /// SPKI 的 SHA-256（「公鑰不得重複使用」的檢查、主控台的公鑰指紋）
    pub spki_sha256: [u8; 32],
}

/// 檢查 CSR；失敗時回傳給 Pulse 的英文說明（`invalid_csr` 的 `message`）
pub fn check_csr(der: &[u8], agent_id: &str) -> Result<CheckedCsr, String> {
    let (rest, csr) = X509CertificationRequest::from_der(der)
        .map_err(|_| "csr is not a valid PKCS#10 request".to_string())?;
    if !rest.is_empty() {
        return Err("csr has trailing data after the PKCS#10 request".to_string());
    }
    let info = &csr.certification_request_info;
    let spki = info.subject_pki.raw;
    if p256_point_from_spki(spki).is_none() {
        return Err("csr public key must be an uncompressed ECDSA P-256 key".to_string());
    }
    if csr.signature_algorithm.algorithm != OID_SIG_ECDSA_WITH_SHA256 {
        return Err("csr signature algorithm must be ecdsa-with-SHA256".to_string());
    }
    csr.verify_signature()
        .map_err(|_| "csr signature is invalid (proof of possession failed)".to_string())?;
    // subject：CN 只能有一個且等於 agent_id；沒有 CN 也可以
    let cns: Vec<&str> = info
        .subject
        .iter_common_name()
        .map(|a| a.as_str().unwrap_or("\u{fffd}"))
        .collect();
    if cns.len() > 1 || cns.first().is_some_and(|cn| *cn != agent_id) {
        return Err("csr subject CN must equal agent_id".to_string());
    }
    // SAN：恰好一個 dNSName 且等於 agent_id，沒有其他種類的名稱
    let mut sans: Vec<String> = Vec::new();
    let mut other = 0usize;
    let mut san_extensions = 0usize;
    if let Some(exts) = csr.requested_extensions() {
        for ext in exts {
            match ext {
                ParsedExtension::SubjectAlternativeName(san) => {
                    san_extensions += 1;
                    for name in &san.general_names {
                        match name {
                            GeneralName::DNSName(n) => sans.push((*n).to_string()),
                            _ => other += 1,
                        }
                    }
                }
                ParsedExtension::ParseError { .. } => {
                    return Err("csr requested extensions cannot be parsed".to_string());
                }
                // 其他要求的擴充欄位不檢查、不複製
                _ => {}
            }
        }
    }
    if san_extensions != 1 || other != 0 || sans.len() != 1 || sans[0] != agent_id {
        return Err(format!(
            "csr subjectAltName must contain exactly one dNSName equal to agent_id {agent_id}"
        ));
    }
    Ok(CheckedCsr {
        spki_der: spki.to_vec(),
        csr_sha256: sha256(der),
        spki_sha256: sha256(spki),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        CertificateParams, CustomExtension, DistinguishedName, DnType, IsCa, KeyPair,
        PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, PublicKeyData, SanType,
    };

    const ID: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";

    fn csr(sans: Vec<SanType>, cn: Option<&str>, key: &KeyPair) -> Vec<u8> {
        let mut p = CertificateParams::default();
        p.distinguished_name = DistinguishedName::new();
        if let Some(cn) = cn {
            p.distinguished_name.push(DnType::CommonName, cn);
        }
        p.subject_alt_names = sans;
        p.serialize_request(key).unwrap().der().to_vec()
    }

    fn dns(name: &str) -> SanType {
        SanType::DnsName(name.try_into().unwrap())
    }

    #[test]
    fn accepts_the_pulse_csr_and_the_spec_vector() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let der = csr(vec![dns(ID)], Some(ID), &key);
        let c = check_csr(&der, ID).unwrap();
        assert_eq!(c.spki_der, key.subject_public_key_info());
        assert_eq!(c.csr_sha256, sha256(&der));
        // subject 可省略
        let der = csr(vec![dns(ID)], None, &key);
        assert!(check_csr(&der, ID).is_ok());
        // ENROLL.md「測試向量」的 CSR
        let vector = kairos_proto::enroll::b64url_decode("MIIBKzCB0wIBADAvMS0wKwYDVQQDDCQ2ZjFjMGU0YS0xYjJjLTRkM2UtOWYwMC0xMTIyMzM0NDU1NjYwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAARgNMbPtTQP1-VTvmN3xg-pCD1955QBQCVGOIE3bv1cNmmMugTtTreii7ExWus49IYkZVjdJtbzpQtkHiX_nqrLoEIwQAYJKoZIhvcNAQkOMTMwMTAvBgNVHREEKDAmgiQ2ZjFjMGU0YS0xYjJjLTRkM2UtOWYwMC0xMTIyMzM0NDU1NjYwCgYIKoZIzj0EAwIDRwAwRAIgJndw_8aCQ9yt-XvNWH2oALgjI3vllqColw89ojJEUiACIFSNVS7yAv8O7P9LwpJ2BGAOrkpZmXW4ey7P-uhwSra6").unwrap();
        let c = check_csr(&vector, ID).unwrap();
        assert_eq!(
            kairos_proto::enroll::hex_lower(&c.csr_sha256),
            "a7e278b234d073fbc5bf7efa0860f821b3a5ac47d15623def278097c629a1a70"
        );
    }

    #[test]
    fn rejects_bad_names_keys_and_signatures() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let other = "00000000-0000-4000-8000-000000000000";
        // SAN 與 agent_id 不符、多一個 SAN、沒有 SAN、CN 不符、大寫
        for der in [
            csr(vec![dns(other)], None, &key),
            csr(vec![dns(ID), dns(other)], None, &key),
            csr(vec![], Some(ID), &key),
            csr(vec![dns(ID)], Some(other), &key),
            csr(vec![dns(&ID.to_ascii_uppercase())], None, &key),
            csr(
                vec![dns(ID), SanType::IpAddress("10.0.0.1".parse().unwrap())],
                None,
                &key,
            ),
        ] {
            assert!(check_csr(&der, ID).is_err());
        }
        // 不是 P-256
        let p384 = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let err = check_csr(&csr(vec![dns(ID)], None, &p384), ID).unwrap_err();
        assert!(err.contains("P-256"), "{err}");
        // 簽章被竄改
        let mut der = csr(vec![dns(ID)], None, &key);
        let n = der.len();
        der[n - 3] ^= 0x01;
        assert!(check_csr(&der, ID).is_err());
        // 結尾有多餘資料、不是 CSR
        let mut der = csr(vec![dns(ID)], None, &key);
        der.push(0);
        assert!(check_csr(&der, ID).is_err());
        assert!(check_csr(b"not a csr", ID).is_err());
    }

    #[test]
    fn requested_extensions_other_than_san_are_ignored() {
        // 要求 CA:TRUE、serverAuth 的 CSR 仍可通過檢查（簽發時不複製，見 issue.rs 的測試）
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut p = CertificateParams::new(vec![ID.to_string()]).unwrap();
        p.distinguished_name = DistinguishedName::new();
        p.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        p.custom_extensions = vec![CustomExtension::from_oid_content(
            &[1, 2, 3, 4],
            vec![0x05, 0x00],
        )];
        let der = p.serialize_request(&key).unwrap().der().to_vec();
        assert!(check_csr(&der, ID).is_ok());
    }
}
