//! mTLS 身分綁定：檢查客戶端憑證是否屬於 hello 宣告的 agent_id。
//!
//! 憑證鏈（由 `auth.client_ca` 簽發、EKU 含 clientAuth、未過期）已在 TLS 握手時由
//! rustls 的 `WebPkiClientVerifier` 驗證；這裡只負責名稱綁定：憑證的 SAN 必須包含
//! `DNS:<agent_id>`（不分大小寫）。預設還要求憑證**恰好只有一個** DNS SAN，
//! 避免一張憑證同時冒用（並以 `superseded` 踢掉）多個 agent。

use std::fmt;
use tokio_rustls::rustls::pki_types::{CertificateDer, DnsName, ServerName};

/// `auth_failed` 時回給客戶端的訊息：憑證含多個 DNS SAN
pub const MSG_MULTIPLE_DNS_SANS: &str = "client certificate must contain exactly one DNS SAN";

/// `auth_failed` 時回給客戶端的訊息：agent_id 與憑證不符
pub const MSG_AGENT_ID_MISMATCH: &str = "agent_id does not match client certificate";

/// 名稱綁定失敗的原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingError {
    /// 憑證含多個 DNS SAN，而設定要求恰好一個
    MultipleDnsSans(usize),
    /// agent_id 與憑證的 SAN 不符，或憑證無法解析（內含說明）
    Mismatch(String),
}

impl BindingError {
    /// 回給客戶端的 `auth_failed` 訊息
    pub fn client_message(&self) -> &'static str {
        match self {
            BindingError::MultipleDnsSans(_) => MSG_MULTIPLE_DNS_SANS,
            BindingError::Mismatch(_) => MSG_AGENT_ID_MISMATCH,
        }
    }
}

impl fmt::Display for BindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BindingError::MultipleDnsSans(count) => write!(
                f,
                "客戶端憑證含 {} 個 DNS SAN，必須恰好一個（auth.allow_multiple_dns_sans = false）",
                count
            ),
            BindingError::Mismatch(reason) => f.write_str(reason),
        }
    }
}

/// 名稱綁定的規則
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BindingPolicy {
    /// 是否接受含多個 DNS SAN 的憑證（`auth.allow_multiple_dns_sans`）
    pub allow_multiple_dns_sans: bool,
}

/// 檢查 end-entity 憑證對 `DNS:<agent_id>` 是否有效。
/// `agent_id` 應已通過 UUID 格式驗證；比對不分大小寫。
pub fn cert_matches_agent_id(
    cert: &CertificateDer<'_>,
    agent_id: &str,
    policy: BindingPolicy,
) -> Result<(), BindingError> {
    // DNS 名稱比對本身不分大小寫，這裡再統一轉成小寫，避免依賴實作細節
    let name = DnsName::try_from(agent_id.to_ascii_lowercase()).map_err(|_| {
        BindingError::Mismatch(format!("agent_id {:?} 不是有效的 DNS 名稱", agent_id))
    })?;
    let ee = webpki::EndEntityCert::try_from(cert)
        .map_err(|e| BindingError::Mismatch(format!("無法解析客戶端憑證: {e}")))?;
    if !policy.allow_multiple_dns_sans {
        // 只用來計數；是否相符仍交給 verify_is_valid_for_subject_name 判斷
        let count = ee.valid_dns_names().count();
        if count > 1 {
            return Err(BindingError::MultipleDnsSans(count));
        }
    }
    ee.verify_is_valid_for_subject_name(&ServerName::DnsName(name))
        .map_err(|e| BindingError::Mismatch(format!("客戶端憑證不含 DNS:{}（{e}）", agent_id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_rustls::rustls::pki_types::pem::PemObject;

    const AGENT: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
    const OTHER: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const STRICT: BindingPolicy = BindingPolicy {
        allow_multiple_dns_sans: false,
    };
    const RELAXED: BindingPolicy = BindingPolicy {
        allow_multiple_dns_sans: true,
    };

    fn cert(pem: &str) -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()
    }

    #[test]
    fn matching_san_is_accepted_case_insensitively() {
        let c = cert(include_str!("../testdata/san.crt"));
        assert!(cert_matches_agent_id(&c, AGENT, STRICT).is_ok());
        assert!(cert_matches_agent_id(&c, &AGENT.to_ascii_uppercase(), STRICT).is_ok());

        // 憑證中的 SAN 為大寫時同樣比對成功
        let upper = cert(include_str!("../testdata/san_upper.crt"));
        assert!(cert_matches_agent_id(&upper, AGENT, STRICT).is_ok());
    }

    #[test]
    fn other_agent_id_is_rejected() {
        let c = cert(include_str!("../testdata/san.crt"));
        assert!(cert_matches_agent_id(&c, OTHER, STRICT).is_err());
        assert!(cert_matches_agent_id(&c, OTHER, RELAXED).is_err());
    }

    #[test]
    fn cn_without_san_is_rejected() {
        // 只有 CN=<agent_id>、沒有 SAN 的憑證不接受（現代 TLS 只看 SAN）
        let c = cert(include_str!("../testdata/cn_only.crt"));
        assert!(cert_matches_agent_id(&c, AGENT, STRICT).is_err());
        assert!(cert_matches_agent_id(&c, AGENT, RELAXED).is_err());
    }

    #[test]
    fn multiple_dns_sans_are_rejected_by_default() {
        // K-S9：multi.crt 含 DNS:other.example 與 DNS:<AGENT>
        let c = cert(include_str!("../testdata/multi.crt"));
        let err = cert_matches_agent_id(&c, AGENT, STRICT).unwrap_err();
        assert_eq!(err, BindingError::MultipleDnsSans(2));
        assert!(err.to_string().contains("2 個 DNS SAN"), "{err}");
        // 回給客戶端的訊息與「agent_id 不符」不同
        assert_eq!(
            err.client_message(),
            "client certificate must contain exactly one DNS SAN"
        );
        let mismatch =
            cert_matches_agent_id(&cert(include_str!("../testdata/san.crt")), OTHER, STRICT)
                .unwrap_err();
        assert!(matches!(mismatch, BindingError::Mismatch(_)));
        assert_eq!(
            mismatch.client_message(),
            "agent_id does not match client certificate"
        );
        assert!(cert_matches_agent_id(&c, OTHER, STRICT).is_err());
    }

    #[test]
    fn any_matching_san_entry_is_accepted_when_relaxed() {
        let c = cert(include_str!("../testdata/multi.crt"));
        assert!(cert_matches_agent_id(&c, AGENT, RELAXED).is_ok());
        assert!(cert_matches_agent_id(&c, OTHER, RELAXED).is_err());
    }

    #[test]
    fn garbage_der_is_rejected() {
        let c = CertificateDer::from(vec![0x30, 0x03, 0x02, 0x01, 0x00]);
        assert!(cert_matches_agent_id(&c, AGENT, STRICT).is_err());
    }
}
