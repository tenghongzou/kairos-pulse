//! 金鑰、CSR 與輪詢憑據（ENROLL.md 第 6.3、15.1 節；設計包 `enrollment-ux.md` §7）。
//!
//! - 私鑰：ECDSA P-256，以作業系統的 CSPRNG 在本機產生（ring），PKCS#8；**永不離開主機**。
//! - CSR：rcgen（以 ring 為後端），subject `CN=<agent_id>`、SAN `DNS:<agent_id>`，不帶其他擴充欄位。
//! - `poll_secret`：32 bytes CSPRNG，base64url（43 個字元）。

use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair};
use tokio_rustls::rustls::pki_types::PrivateKeyDer;
use tokio_rustls::rustls::pki_types::pem::PemObject;

/// 產生新的 P-256 私鑰（PKCS#8 DER，含公鑰）
pub fn generate_p256_pkcs8() -> Result<Vec<u8>, String> {
    let rng = SystemRandom::new();
    EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
        .map(|doc| doc.as_ref().to_vec())
        .map_err(|_| "無法產生 ECDSA P-256 私鑰（作業系統亂數來源無法使用）".to_string())
}

/// 以私鑰產生 CSR（DER）：subject `CN=<agent_id>`、SAN 恰好一個 `DNS:<agent_id>`
pub fn build_csr(pkcs8: &[u8], agent_id: &str) -> Result<Vec<u8>, String> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
    let agent_id = agent_id.to_ascii_lowercase();
    let key = KeyPair::try_from(pkcs8).map_err(|e| format!("無法載入私鑰：{e}"))?;
    let mut params = CertificateParams::new(vec![agent_id.clone()])
        .map_err(|e| format!("agent_id「{agent_id}」無法作為 SAN：{e}"))?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, agent_id.as_str());
    let csr = params
        .serialize_request(&key)
        .map_err(|e| format!("無法產生 CSR：{e}"))?;
    Ok(csr.der().as_ref().to_vec())
}

/// PKCS#8 私鑰的 PEM（`-----BEGIN PRIVATE KEY-----`）
pub fn pkcs8_pem(pkcs8: &[u8]) -> Result<String, String> {
    let key = rcgen::KeyPair::try_from(pkcs8).map_err(|e| format!("無法載入私鑰：{e}"))?;
    Ok(key.serialize_pem())
}

/// 讀取 PEM 私鑰；只接受 PKCS#8 的 P-256 私鑰（Pulse 自己產生的格式；過期續簽與撤回需要以它簽章）
pub fn pkcs8_from_pem(pem: &[u8]) -> Result<Vec<u8>, String> {
    match PrivateKeyDer::from_pem_slice(pem) {
        Ok(PrivateKeyDer::Pkcs8(k)) => {
            let der = k.secret_pkcs8_der().to_vec();
            kairos_proto::enroll::p256_spki_of_pkcs8(&der)
                .map_err(|_| "私鑰不是 ECDSA P-256（PKCS#8）".to_string())?;
            Ok(der)
        }
        Ok(_) => Err(
            "私鑰不是 PKCS#8 格式（過期續簽與撤回申請只支援 ECDSA P-256 的 PKCS#8 私鑰）"
                .to_string(),
        ),
        Err(e) => Err(format!("無法讀取私鑰：{e}")),
    }
}

/// 新的輪詢憑據：32 bytes CSPRNG 的 base64url
pub fn new_poll_secret() -> Result<String, String> {
    let mut bytes = [0u8; kairos_proto::enroll::POLL_SECRET_BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "作業系統亂數來源無法使用".to_string())?;
    Ok(kairos_proto::enroll::b64url_encode(&bytes))
}

/// 新的 UUID（8443 `renew_request`／`deregister` 的 `request_id`）
pub fn new_request_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enroll::vectors::Vectors;
    use kairos_proto::enroll::{b64url_decode, b64url_encode, request_code_from_spki, sha256_hex};

    #[test]
    fn generated_key_and_csr_are_p256_with_the_agent_id() {
        let pkcs8 = generate_p256_pkcs8().unwrap();
        let spki = kairos_proto::enroll::p256_spki_of_pkcs8(&pkcs8).unwrap();
        let csr = build_csr(&pkcs8, "6F1C0E4A-1B2C-4D3E-9F00-112233445566").unwrap();
        // CSR 中含同一個 SPKI 與小寫的 agent_id（SAN 與 CN）
        let contains = |needle: &[u8]| csr.windows(needle.len()).any(|w| w == needle);
        assert!(contains(&spki));
        assert!(contains(b"6f1c0e4a-1b2c-4d3e-9f00-112233445566"));
        assert!(!contains(b"6F1C0E4A"));
        // 兩把金鑰不同
        assert_ne!(pkcs8, generate_p256_pkcs8().unwrap());
        // PEM 往返
        let pem = pkcs8_pem(&pkcs8).unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert_eq!(pkcs8_from_pem(pem.as_bytes()).unwrap(), pkcs8);
    }

    #[test]
    fn csr_from_vector_key_matches_shared_vectors() {
        // ENROLL.md「測試向量」：以 new_key_pkcs8 產生的 CSR 的 SPKI 與申請編號一致
        let v = Vectors::load();
        let pkcs8 = b64url_decode(v.get("new_key_pkcs8")).unwrap();
        let csr = build_csr(&pkcs8, v.get("agent_id")).unwrap();
        let spki = kairos_proto::enroll::p256_spki_of_pkcs8(&pkcs8).unwrap();
        assert_eq!(b64url_encode(&spki), v.get("new_spki"));
        assert_eq!(
            request_code_from_spki(&spki).to_string(),
            v.get("request_code")
        );
        // ECDSA 簽章每次不同，CSR 的位元組因此不同；但向量中的 CSR 本身的雜湊必須與 csr_sha256 相同
        let vector_csr = b64url_decode(v.get("csr")).unwrap();
        assert_eq!(sha256_hex(&vector_csr), v.get("csr_sha256"));
        assert!(csr.windows(spki.len()).any(|w| w == spki.as_slice()));
    }

    #[test]
    fn poll_secret_is_32_random_bytes() {
        let a = new_poll_secret().unwrap();
        let b = new_poll_secret().unwrap();
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert_eq!(b64url_decode(&a).unwrap().len(), 32);
    }

    #[test]
    fn rejects_non_p256_or_non_pkcs8_keys() {
        let rsa = include_bytes!("../../testdata/client-rsa.key");
        assert!(pkcs8_from_pem(rsa).is_err());
        let sec1 = include_bytes!("../../testdata/client-ec.sec1.key");
        assert!(pkcs8_from_pem(sec1).is_err());
        let ec = include_bytes!("../../testdata/client-ec.key");
        assert!(pkcs8_from_pem(ec).is_ok());
    }
}
