//! 8444 上如何信任 Kairos（ENROLL.md 第 3 節、設計包 `enrollment-ux.md` §6）。
//!
//! **絕不**使用作業系統信任庫。依序只信任其中一種來源：
//!
//! | 來源 | 驗證 |
//! |---|---|
//! | CA 指紋（安裝字串的 `p`、`--ca-pin`） | 在伺服器送出的鏈中找出 SPKI 的 SHA-256 等於指紋、且 `CA:TRUE` 的憑證，以**它為唯一的信任錨**照常以 webpki 驗證（簽章、有效期間、EKU、名稱）；找不到時 `PLS-E1406`（不送出任何請求） |
//! | `--ca-file`、既有的 `identity/ca.pem` | 以檔案中的憑證為信任錨 |
//! | TOFU | 以鏈中最上層的 CA 為信任錨（呼叫端先顯示指紋請人確認，或依 `--trust-on-first-use`） |
//!
//! 根 CA 可以是 P-256、P-384 或 RSA（既有的部署）：簽章驗證使用 ring 的預設演算法清單。
//!
//! **本機時鐘偏差很大時**（ENROLL.md 第 3 節最後一段）：只有 `PLS-E1403`（過期／尚未生效）時，呼叫端
//! 可以用 `ignore_validity` 再驗證一次（其他檢查一律照常），只送 `server-info` 取得未驗證的參考時間。

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{self, CryptoProvider};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore,
    SignatureScheme,
};

use super::x509;
use crate::codes;

/// [`TlsError::General`] 中表示「鏈中找不到指紋相符的 CA」的前綴（對應 `PLS-E1406`）
const PIN_NOT_FOUND: &str = "PLS-E1406 ca pin not found";
/// TOFU：鏈中沒有任何 CA 憑證
const NO_CA_IN_CHAIN: &str = "PLS-E1406 no ca certificate in chain";

/// 信任來源
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustSpec {
    /// `sha256:<64 個小寫十六進位>`（已正規化）
    Pin(String),
    /// `--ca-file` 或 `identity/ca.pem` 的憑證
    Anchors(Vec<CertificateDer<'static>>),
    /// 尚未確認的 trust-on-first-use
    Tofu,
}

/// 握手時觀察到的內容（顯示 TOFU 指紋、寫入 `ca.pem`）
#[derive(Debug, Clone, Default)]
pub struct Observed {
    /// 伺服器送出的整條鏈（第一張為伺服器憑證）
    pub chain: Vec<CertificateDer<'static>>,
    /// 用來驗證的信任錨（指紋相符的 CA、TOFU 選出的 CA；`Anchors` 時為 `None`）
    pub anchor: Option<CertificateDer<'static>>,
}

/// 8444 的伺服器憑證驗證器
#[derive(Debug)]
pub struct EnrollVerifier {
    spec: TrustSpec,
    provider: Arc<CryptoProvider>,
    ignore_validity: bool,
    observed: Arc<Mutex<Observed>>,
}

impl EnrollVerifier {
    pub fn new(spec: TrustSpec, ignore_validity: bool) -> Self {
        Self {
            spec,
            provider: Arc::new(crypto::ring::default_provider()),
            ignore_validity,
            observed: Arc::new(Mutex::new(Observed::default())),
        }
    }

    /// 最近一次握手觀察到的鏈與信任錨
    pub fn observed(&self) -> Observed {
        self.observed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn pick_anchors(
        &self,
        chain: &[CertificateDer<'static>],
    ) -> Result<Vec<CertificateDer<'static>>, TlsError> {
        match &self.spec {
            TrustSpec::Pin(pin) => chain
                .iter()
                .find(|c| {
                    x509::parse_cert(c).is_some_and(|info| {
                        info.is_ca && kairos_proto::enroll::ca_pin_from_spki(&info.spki) == *pin
                    })
                })
                .map(|c| vec![c.clone()])
                .ok_or_else(|| TlsError::General(PIN_NOT_FOUND.to_string())),
            TrustSpec::Anchors(list) => Ok(list.clone()),
            TrustSpec::Tofu => chain
                .iter()
                .rev()
                .find(|c| x509::parse_cert(c).is_some_and(|i| i.is_ca))
                .map(|c| vec![c.clone()])
                .ok_or_else(|| TlsError::General(NO_CA_IN_CHAIN.to_string())),
        }
    }
}

/// 忽略有效期間時使用的驗證時間：鏈中所有憑證（含信任錨）`notBefore` 的最大值再加 1 秒
fn validity_free_time(certs: &[&CertificateDer<'_>]) -> Option<UnixTime> {
    let latest = certs
        .iter()
        .filter_map(|c| x509::parse_cert(c))
        .map(|i| i.not_before)
        .max()?;
    let secs = u64::try_from(latest.timestamp()).ok()?.saturating_add(1);
    Some(UnixTime::since_unix_epoch(Duration::from_secs(secs)))
}

impl ServerCertVerifier for EnrollVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let chain: Vec<CertificateDer<'static>> = std::iter::once(end_entity)
            .chain(intermediates)
            .map(|c| c.clone().into_owned())
            .collect();
        {
            let mut obs = self.observed.lock().unwrap_or_else(PoisonError::into_inner);
            obs.chain = chain.clone();
            obs.anchor = None;
        }
        let anchors = self.pick_anchors(&chain)?;
        let mut roots = RootCertStore::empty();
        for a in &anchors {
            roots
                .add(a.clone())
                .map_err(|e| TlsError::General(format!("PLS-E1401 bad trust anchor: {e}")))?;
        }
        let verifier =
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), self.provider.clone())
                .build()
                .map_err(|e| TlsError::General(format!("PLS-E1401 verifier: {e}")))?;
        let now = if self.ignore_validity {
            let all: Vec<&CertificateDer<'_>> = chain.iter().chain(anchors.iter()).collect();
            validity_free_time(&all).unwrap_or(now)
        } else {
            now
        };
        let verified = verifier.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        if !matches!(self.spec, TrustSpec::Anchors(_)) {
            self.observed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .anchor = anchors.into_iter().next();
        }
        Ok(verified)
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

/// 8444 的 TLS 用戶端設定：自訂驗證器、不出示用戶端憑證、ALPN 只有 `http/1.1`
pub fn client_config(verifier: Arc<EnrollVerifier>) -> Result<ClientConfig, String> {
    let provider = Arc::new(crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("無法建立 TLS 設定：{e}"))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// rustls 錯誤對應的伺服器身分代碼（`PLS-E1401`～`PLS-E1406`）；不是伺服器憑證的問題時 `None`
pub fn trust_error_code(e: &TlsError) -> Option<&'static str> {
    use CertificateError as C;
    Some(match e {
        TlsError::General(s) if s.starts_with("PLS-E1406") => codes::E_CA_PIN,
        TlsError::General(s) if s.starts_with("PLS-E1401") => codes::E_SERVER_CERT_UNTRUSTED,
        TlsError::InvalidCertificate(c) => match c {
            C::NotValidForName | C::NotValidForNameContext { .. } => codes::E_SERVER_CERT_NAME,
            C::Expired
            | C::ExpiredContext { .. }
            | C::NotValidYet
            | C::NotValidYetContext { .. } => codes::E_SERVER_CERT_VALIDITY,
            C::Revoked => codes::E_SERVER_CERT_REVOKED,
            C::InvalidPurpose | C::InvalidPurposeContext { .. } => codes::E_SERVER_CERT_PURPOSE,
            C::Other(other)
                if other.0.downcast_ref::<webpki::Error>()
                    == Some(&webpki::Error::CaUsedAsEndEntity) =>
            {
                codes::E_SERVER_CERT_PURPOSE
            }
            _ => codes::E_SERVER_CERT_UNTRUSTED,
        },
        _ => return None,
    })
}

/// 伺服器身分代碼的一句話說明（三段式的「原因」）
pub fn trust_error_text(code: &str) -> &'static str {
    match code {
        codes::E_CA_PIN => "註冊端點送出的憑證鏈中，找不到與 CA 指紋相符的 CA 憑證",
        codes::E_SERVER_CERT_NAME => "註冊端點的伺服器憑證名稱與連線位址不符",
        codes::E_SERVER_CERT_VALIDITY => {
            "註冊端點的伺服器憑證已過期或尚未生效（也可能是本機時鐘不正確）"
        }
        codes::E_SERVER_CERT_REVOKED => "註冊端點的伺服器憑證已被撤銷",
        codes::E_SERVER_CERT_PURPOSE => "註冊端點的伺服器憑證用途不符（需要 serverAuth、CA:FALSE）",
        _ => "註冊端點的伺服器憑證不是由信任的 CA 簽發",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_rustls_errors_to_codes() {
        use CertificateError as C;
        let cases = [
            (TlsError::General(PIN_NOT_FOUND.into()), codes::E_CA_PIN),
            (TlsError::General(NO_CA_IN_CHAIN.into()), codes::E_CA_PIN),
            (
                TlsError::InvalidCertificate(C::UnknownIssuer),
                codes::E_SERVER_CERT_UNTRUSTED,
            ),
            (
                TlsError::InvalidCertificate(C::NotValidForName),
                codes::E_SERVER_CERT_NAME,
            ),
            (
                TlsError::InvalidCertificate(C::Expired),
                codes::E_SERVER_CERT_VALIDITY,
            ),
            (
                TlsError::InvalidCertificate(C::NotValidYet),
                codes::E_SERVER_CERT_VALIDITY,
            ),
            (
                TlsError::InvalidCertificate(C::Revoked),
                codes::E_SERVER_CERT_REVOKED,
            ),
            (
                TlsError::InvalidCertificate(C::InvalidPurpose),
                codes::E_SERVER_CERT_PURPOSE,
            ),
        ];
        for (err, code) in cases {
            assert_eq!(trust_error_code(&err), Some(code), "{err:?}");
        }
        assert_eq!(trust_error_code(&TlsError::HandshakeNotComplete), None);
    }
}
