//! mTLS 身分綁定：檢查客戶端憑證是否屬於 hello 宣告的 agent_id。
//!
//! 憑證鏈（由 `auth.client_ca` 簽發、EKU 含 clientAuth、未過期）已在 TLS 握手時由
//! rustls 的 `WebPkiClientVerifier` 驗證；這裡只負責名稱綁定：憑證的 SAN 必須包含
//! `DNS:<agent_id>`（不分大小寫）。

use tokio_rustls::rustls::pki_types::{CertificateDer, DnsName, ServerName};

/// 檢查 end-entity 憑證對 `DNS:<agent_id>` 是否有效。
/// `agent_id` 應已通過 UUID 格式驗證；比對不分大小寫。
pub fn cert_matches_agent_id(cert: &CertificateDer<'_>, agent_id: &str) -> Result<(), String> {
    // DNS 名稱比對本身不分大小寫，這裡再統一轉成小寫，避免依賴實作細節
    let name = DnsName::try_from(agent_id.to_ascii_lowercase())
        .map_err(|_| format!("agent_id {:?} 不是有效的 DNS 名稱", agent_id))?;
    let ee =
        webpki::EndEntityCert::try_from(cert).map_err(|e| format!("無法解析客戶端憑證: {e}"))?;
    ee.verify_is_valid_for_subject_name(&ServerName::DnsName(name))
        .map_err(|e| format!("客戶端憑證不含 DNS:{}（{e}）", agent_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_rustls::rustls::pki_types::pem::PemObject;

    const AGENT: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
    const OTHER: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    fn cert(pem: &str) -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()
    }

    #[test]
    fn matching_san_is_accepted_case_insensitively() {
        let c = cert(include_str!("../testdata/san.crt"));
        assert!(cert_matches_agent_id(&c, AGENT).is_ok());
        assert!(cert_matches_agent_id(&c, &AGENT.to_ascii_uppercase()).is_ok());

        // 憑證中的 SAN 為大寫時同樣比對成功
        let upper = cert(include_str!("../testdata/san_upper.crt"));
        assert!(cert_matches_agent_id(&upper, AGENT).is_ok());
    }

    #[test]
    fn other_agent_id_is_rejected() {
        let c = cert(include_str!("../testdata/san.crt"));
        assert!(cert_matches_agent_id(&c, OTHER).is_err());
    }

    #[test]
    fn cn_without_san_is_rejected() {
        // 只有 CN=<agent_id>、沒有 SAN 的憑證不接受（現代 TLS 只看 SAN）
        let c = cert(include_str!("../testdata/cn_only.crt"));
        assert!(cert_matches_agent_id(&c, AGENT).is_err());
    }

    #[test]
    fn any_matching_san_entry_is_accepted() {
        let c = cert(include_str!("../testdata/multi.crt"));
        assert!(cert_matches_agent_id(&c, AGENT).is_ok());
        assert!(cert_matches_agent_id(&c, OTHER).is_err());
    }

    #[test]
    fn garbage_der_is_rejected() {
        let c = CertificateDer::from(vec![0x30, 0x03, 0x02, 0x01, 0x00]);
        assert!(cert_matches_agent_id(&c, AGENT).is_err());
    }
}
