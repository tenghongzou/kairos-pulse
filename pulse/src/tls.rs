//! TLS 設定（rustls）。
//!
//! - 信任：作業系統信任庫（rustls-native-certs）+ `server.ca_cert`（可含多張）。
//! - 客戶端憑證（mTLS）：`server.client_cert`（可含中繼 CA 鏈）+ `server.client_key`
//!   （PKCS#8 / PKCS#1 / SEC1，RSA / ECDSA / Ed25519）。金鑰與憑證不成對時在啟動時即報錯。
//! - `accept_invalid_certs = true`：只略過伺服器憑證的信任鏈與名稱驗證，握手簽章仍會正確驗證。
//! - 握手失敗時把 rustls 錯誤轉成具體的處置提示（[`describe_tls_error`]）。

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// 啟動時建立一次、所有連線共用的 TLS 設定。
#[derive(Clone)]
pub struct TlsSetup {
    pub connector: TlsConnector,
    /// SNI 與伺服器憑證名稱驗證用的名稱（DNS 名稱或 IP）
    pub server_name: ServerName<'static>,
    /// 伺服器是否在最近一次握手中要求客戶端憑證（見 [`RecordingClientCertResolver`]）
    client_cert_requested: Arc<AtomicBool>,
    /// 客戶端憑證的路徑與葉憑證（用於啟動時檢查 SAN）
    client_leaf: Option<(String, CertificateDer<'static>)>,
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

    /// 確認客戶端憑證的 SAN 含 `DNS:<agent_id>`（伺服器以此比對 hello 的 agent_id，不分大小寫）。
    ///
    /// 未設定客戶端憑證時直接通過。不符時回傳具體的錯誤訊息，讓程式在啟動時就結束，
    /// 而不是連上伺服器後才收到 `auth_failed`。
    pub fn verify_client_cert_identity(&self, agent_id: &str) -> Result<(), String> {
        let Some((path, leaf)) = &self.client_leaf else {
            return Ok(());
        };
        check_cert_san(path, leaf, agent_id)
    }
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
        if !server.ca_cert.trim().is_empty() {
            log::warn!(
                "同時設定了 server.ca_cert 與 accept_invalid_certs = true：憑證驗證已停用，ca_cert 形同無效"
            );
        }
        // ⚠️ 僅供開發測試，正式環境不應開啟
        log::warn!("已停用 TLS 憑證驗證（accept_invalid_certs = true），請勿用於正式環境");
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert {
                provider: provider.clone(),
            }))
    } else {
        let roots = load_roots(server.ca_cert.trim())?;
        let verifier =
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .build()
                .map_err(|e| format!("無法建立伺服器憑證驗證器：{e}"))?;
        builder.with_webpki_verifier(verifier)
    };

    let cert_path = server.client_cert.trim();
    let key_path = server.client_key.trim();
    let (client_key, client_leaf) = if cert_path.is_empty() || key_path.is_empty() {
        (None, None)
    } else {
        let (chain, key) = load_client_cert(cert_path, key_path)?;
        let leaf = chain[0].clone();
        let count = chain.len();
        // from_der 會檢查私鑰與憑證是否成對（keys_match）
        let certified = CertifiedKey::from_der(chain, key, &provider)
            .map_err(|e| client_cert_error(cert_path, key_path, &e))?;
        log::info!("已載入客戶端憑證：{cert_path}（憑證鏈 {count} 張；私鑰：{key_path}）");
        (
            Some(Arc::new(certified)),
            Some((cert_path.to_string(), leaf)),
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
        client_leaf,
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

/// 信任庫：作業系統信任庫 + `ca_cert`（可含多張）。
///
/// 系統信任庫中個別憑證載入失敗只記 warn；兩者合計沒有任何可用憑證時回傳錯誤。
fn load_roots(ca_path: &str) -> Result<RootCertStore, String> {
    let mut roots = RootCertStore::empty();

    let native = rustls_native_certs::load_native_certs();
    for e in &native.errors {
        log::warn!("載入作業系統信任庫時發生錯誤（略過）：{e}");
    }
    let (added, ignored) = roots.add_parsable_certificates(native.certs);
    if ignored > 0 {
        log::warn!("作業系統信任庫中有 {ignored} 張憑證無法使用，已略過");
    }
    log::debug!("已從作業系統信任庫載入 {added} 張憑證");

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
        return Err(
            "沒有任何可信任的憑證：作業系統信任庫無法載入，且未設定 server.ca_cert".to_string(),
        );
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
    // 讀取第一把私鑰，接受 PKCS#8、PKCS#1（RSA）與 SEC1（EC）格式
    let key = PrivateKeyDer::from_pem_file(key_path)
        .map_err(|e| pem_error(key_path, "server.client_key", "私鑰", e))?;
    Ok((chain, key))
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
        let roots = load_roots(&data("ca-bundle.pem")).unwrap();
        // 系統信任庫 + bundle 中的 2 張
        assert!(roots.len() >= 2);
        assert!(build(&server("ca-bundle.pem", "", "")).is_ok());
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
        assert!(build(&cfg).is_ok());
        // 搭配客戶端憑證也可以
        let mut cfg = server("", "client-ec-chain.pem", "client-ec.key");
        cfg.accept_invalid_certs = true;
        assert!(build(&cfg).is_ok());
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
