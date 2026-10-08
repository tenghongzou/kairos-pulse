//! TLS 設定：伺服器憑證、mTLS 客戶端 CA 與 CRL 的載入、可原子替換的目前設定，
//! 以及以目前的驗證器重新驗證既有連線的客戶端憑證。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Error, ErrorKind};
use std::sync::{Arc, RwLock};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::pem::{self, PemObject};
use tokio_rustls::rustls::pki_types::{
    CertificateDer, CertificateRevocationListDer, PrivateKeyDer, UnixTime,
};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::server::danger::ClientCertVerifier;
use tokio_rustls::rustls::{RootCertStore, ServerConfig};

/// 組成 TLS 設定所需的檔案
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsPaths {
    pub cert: String,
    pub key: String,
    /// 設定時啟用 mTLS
    pub client_ca: Option<String>,
    /// CRL 檔（PEM 或 DER）；只在啟用 mTLS 時使用
    pub crls: Vec<String>,
}

impl TlsPaths {
    /// 需要監看變化的所有檔案
    pub fn watched_files(&self) -> Vec<&str> {
        let mut files = vec![self.cert.as_str(), self.key.as_str()];
        if let Some(ca) = &self.client_ca {
            files.push(ca);
            files.extend(self.crls.iter().map(String::as_str));
        }
        files
    }
}

/// 一份完整的 TLS 設定
pub struct TlsMaterial {
    pub acceptor: TlsAcceptor,
    /// 啟用 mTLS 時的客戶端憑證驗證器（同一個實例也用於重新驗證既有連線）
    pub verifier: Option<Arc<dyn ClientCertVerifier>>,
    /// 載入的 CRL 數量
    pub crl_count: usize,
}

impl TlsMaterial {
    pub fn auth_enabled(&self) -> bool {
        self.verifier.is_some()
    }
}

/// 依 `paths` 建立 TLS 設定；任何檔案有問題都回錯誤（訊息附路徑）
pub fn build(paths: &TlsPaths) -> Result<TlsMaterial, Error> {
    let certs = load_certs(&paths.cert, "證書")?;

    // 讀取第一把私鑰，接受 PKCS#8、PKCS#1（RSA）與 SEC1（EC）格式
    let key =
        PrivateKeyDer::from_pem_file(&paths.key).map_err(|e| pem_error(&paths.key, "私鑰", e))?;

    let (verifier, crl_count) = match &paths.client_ca {
        Some(ca_path) => {
            let crls = load_crls(&paths.crls)?;
            let count = crls.len();
            (Some(load_client_verifier(ca_path, crls)?), count)
        }
        None => (None, 0),
    };

    let builder = ServerConfig::builder();
    let builder = match &verifier {
        Some(v) => builder.with_client_cert_verifier(v.clone()),
        None => builder.with_no_client_auth(),
    };
    let config = builder.with_single_cert(certs, key).map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("{} 與 {} 無法組成 TLS 設定: {}", paths.cert, paths.key, e),
        )
    })?;

    Ok(TlsMaterial {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        verifier,
        crl_count,
    })
}

// 讀取 PEM 檔中的所有憑證，檔案中沒有任何憑證時回錯誤
fn load_certs(path: &str, item: &str) -> Result<Vec<CertificateDer<'static>>, Error> {
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|e| pem_error(path, item, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| pem_error(path, item, e))?;
    if certs.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{} 中找不到{}", path, item),
        ));
    }
    Ok(certs)
}

/// 讀取 CRL 檔：內容含 PEM 標頭時以 PEM 解析（可含多份），否則視為單一 DER。
/// 每份 CRL 都先解析驗證，讓錯誤訊息能指出是哪個檔案。
pub fn load_crl_file(path: &str) -> Result<Vec<CertificateRevocationListDer<'static>>, Error> {
    let bytes = std::fs::read(path)
        .map_err(|e| Error::new(e.kind(), format!("無法開啟 {}: {}", path, e)))?;
    let crls = if bytes.windows(11).any(|w| w == b"-----BEGIN ") {
        CertificateRevocationListDer::pem_slice_iter(&bytes)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| pem_error(path, "CRL", e))?
    } else {
        vec![CertificateRevocationListDer::from(bytes)]
    };
    if crls.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{} 中找不到 CRL", path),
        ));
    }
    for crl in &crls {
        webpki::OwnedCertRevocationList::from_der(crl.as_ref()).map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("{} 中的 CRL 格式錯誤: {:?}", path, e),
            )
        })?;
    }
    Ok(crls)
}

fn load_crls(paths: &[String]) -> Result<Vec<CertificateRevocationListDer<'static>>, Error> {
    let mut all = Vec::new();
    for path in paths {
        all.extend(load_crl_file(path)?);
    }
    Ok(all)
}

/// 以 auth.client_ca（可含多張 CA）與 CRL 建立客戶端憑證驗證器。
///
/// 設定 CRL 時採用最嚴格的檢查（fail closed）：
/// - 檢查整條鏈（不只 end-entity）的撤銷狀態；
/// - 無法判定撤銷狀態（缺少該層 CA 的 CRL）視為失敗；
/// - CRL 超過 nextUpdate 視為失敗，避免過期的 CRL 漏掉之後的撤銷。
fn load_client_verifier(
    ca_path: &str,
    crls: Vec<CertificateRevocationListDer<'static>>,
) -> Result<Arc<dyn ClientCertVerifier>, Error> {
    let mut roots = RootCertStore::empty();
    for cert in load_certs(ca_path, "CA 憑證")? {
        roots.add(cert).map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("{} 中的 CA 憑證無效: {}", ca_path, e),
            )
        })?;
    }
    let mut builder = WebPkiClientVerifier::builder(Arc::new(roots));
    if !crls.is_empty() {
        builder = builder.with_crls(crls).enforce_revocation_expiration();
    }
    builder.build().map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("無法以 {} 建立客戶端憑證驗證器: {}", ca_path, e),
        )
    })
}

// 將 PEM 解析錯誤轉換為 std::io::Error，錯誤訊息附上路徑以便判斷是哪個檔案出問題
fn pem_error(path: &str, item: &str, e: pem::Error) -> Error {
    match e {
        pem::Error::Io(e) => Error::new(e.kind(), format!("無法開啟 {}: {}", path, e)),
        pem::Error::NoItemsFound => Error::new(
            ErrorKind::InvalidInput,
            format!("{} 中找不到{}", path, item),
        ),
        e => Error::new(
            ErrorKind::InvalidData,
            format!("{} 中的{}格式錯誤: {}", path, item, e),
        ),
    }
}

/// 監看檔案的內容指紋：(路徑, 內容雜湊)；讀不到時雜湊為 `None`。
/// 以內容而非修改時間比對，避免複製工具保留 mtime 或時間解析度不足時漏掉變更。
pub type Fingerprint = Vec<(String, Option<u64>)>;

pub fn fingerprint(paths: &TlsPaths) -> Fingerprint {
    paths
        .watched_files()
        .into_iter()
        .map(|path| {
            let hash = std::fs::read(path).ok().map(|bytes| {
                let mut h = DefaultHasher::new();
                bytes.hash(&mut h);
                h.finish()
            });
            (path.to_string(), hash)
        })
        .collect()
}

/// 目前生效的 TLS 設定；重載時整份原子替換，既有連線不受影響
pub struct TlsState {
    current: RwLock<Arc<TlsMaterial>>,
}

impl TlsState {
    pub fn new(material: TlsMaterial) -> Self {
        Self {
            current: RwLock::new(Arc::new(material)),
        }
    }

    pub fn current(&self) -> Arc<TlsMaterial> {
        // 只做 Arc 的複製與替換，持鎖期間不會 panic；被污染也沿用內部資料
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn replace(&self, material: TlsMaterial) {
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(material);
    }
}

/// 以驗證器檢查握手時取得的客戶端憑證鏈（[0] 為 end-entity，其餘為中繼憑證）。
/// 用於重載 CA/CRL 後與定期檢查憑證是否過期。
pub fn verify_chain(
    verifier: &dyn ClientCertVerifier,
    chain: Option<&[CertificateDer<'static>]>,
    now: UnixTime,
) -> Result<(), String> {
    let Some((end_entity, intermediates)) = chain.and_then(|c| c.split_first()) else {
        return Err("連線沒有客戶端憑證".to_string());
    };
    verifier
        .verify_client_cert(end_entity, intermediates, now)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/pki/");

    fn p(name: &str) -> String {
        format!("{DIR}{name}")
    }

    fn paths(client_ca: Option<&str>, crls: &[&str]) -> TlsPaths {
        TlsPaths {
            cert: p("server.crt"),
            key: p("server.key"),
            client_ca: client_ca.map(p),
            crls: crls.iter().map(|c| p(c)).collect(),
        }
    }

    fn chain(name: &str) -> Vec<CertificateDer<'static>> {
        vec![CertificateDer::from_pem_file(p(name)).unwrap()]
    }

    fn verifier(crls: &[&str]) -> Arc<dyn ClientCertVerifier> {
        build(&paths(Some("test-ca.crt"), crls))
            .unwrap()
            .verifier
            .unwrap()
    }

    #[test]
    fn missing_client_ca_file_reports_path() {
        let err = load_client_verifier("does-not-exist/ca.pem", Vec::new())
            .err()
            .unwrap();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(err.to_string().contains("does-not-exist/ca.pem"));
    }

    #[test]
    fn client_ca_without_certificates_is_error() {
        let path = std::env::temp_dir().join(format!("kairos-empty-ca-{}.pem", std::process::id()));
        std::fs::write(&path, "not a pem file\n").unwrap();
        let p = path.to_string_lossy().to_string();
        let err = load_client_verifier(&p, Vec::new()).err().unwrap();
        assert!(err.to_string().contains(&p), "{err}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn build_without_and_with_mtls() {
        let plain = build(&paths(None, &[])).unwrap();
        assert!(!plain.auth_enabled());
        let mtls = build(&paths(Some("test-ca.crt"), &["crl-revoke-a.crl"])).unwrap();
        assert!(mtls.auth_enabled());
        assert_eq!(mtls.crl_count, 1);
    }

    #[test]
    fn crl_pem_and_der_are_equivalent() {
        let pem = load_crl_file(&p("crl-revoke-a.crl")).unwrap();
        let der = load_crl_file(&p("crl-revoke-a.der")).unwrap();
        assert_eq!(pem.len(), 1);
        assert_eq!(pem, der);
    }

    #[test]
    fn bad_crl_files_are_rejected_with_path() {
        let dir = std::env::temp_dir();
        let cases: [(&str, &[u8]); 3] = [
            ("garbage.der", b"\x30\x03\x02\x01\x00"),
            (
                "bad.crl",
                b"-----BEGIN X509 CRL-----\nAAAA\n-----END X509 CRL-----\n",
            ),
            (
                "cert-not-crl.crl",
                b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
            ),
        ];
        for (name, content) in cases {
            let path = dir.join(format!("kairos-{}-{}", std::process::id(), name));
            std::fs::write(&path, content).unwrap();
            let p = path.to_string_lossy().to_string();
            let err = load_crl_file(&p).unwrap_err();
            assert!(err.to_string().contains(&p), "{name}: {err}");
            std::fs::remove_file(&path).unwrap();
        }
        assert_eq!(
            load_crl_file("missing.crl").unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn revoked_certificate_is_rejected_and_others_accepted() {
        let now = UnixTime::now();
        let v = verifier(&["crl-revoke-a.der"]);
        let err = verify_chain(v.as_ref(), Some(&chain("client-a.crt")), now).unwrap_err();
        assert!(err.to_lowercase().contains("revoked"), "{err}");
        assert!(verify_chain(v.as_ref(), Some(&chain("client-b.crt")), now).is_ok());

        // 沒有撤銷任何憑證的 CRL：兩者都接受
        let v = verifier(&["crl-empty.crl"]);
        assert!(verify_chain(v.as_ref(), Some(&chain("client-a.crt")), now).is_ok());
        // 不設定 CRL 時不檢查撤銷
        let v = verifier(&[]);
        assert!(verify_chain(v.as_ref(), Some(&chain("client-a.crt")), now).is_ok());
    }

    #[test]
    fn expired_certificate_and_missing_chain_are_rejected() {
        let v = verifier(&[]);
        let err = verify_chain(
            v.as_ref(),
            Some(&chain("client-expired.crt")),
            UnixTime::now(),
        )
        .unwrap_err();
        assert!(err.to_lowercase().contains("expired"), "{err}");
        // 在效期內的時間點則通過（2020-06-01）
        let in_validity = UnixTime::since_unix_epoch(Duration::from_secs(1_590_969_600));
        assert!(verify_chain(v.as_ref(), Some(&chain("client-expired.crt")), in_validity).is_ok());

        assert!(verify_chain(v.as_ref(), None, UnixTime::now()).is_err());
        assert!(verify_chain(v.as_ref(), Some(&[]), UnixTime::now()).is_err());
    }

    #[test]
    fn certificate_from_unknown_ca_is_rejected() {
        let v = verifier(&[]);
        let other =
            vec![CertificateDer::from_pem_slice(include_bytes!("../testdata/san.crt")).unwrap()];
        assert!(verify_chain(v.as_ref(), Some(&other), UnixTime::now()).is_err());
    }

    #[test]
    fn fingerprint_tracks_content_and_watched_files() {
        let dir = std::env::temp_dir().join(format!("kairos-fp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = |n: &str| dir.join(n).to_string_lossy().to_string();
        std::fs::write(f("c"), "cert").unwrap();
        std::fs::write(f("k"), "key").unwrap();
        let mut tp = TlsPaths {
            cert: f("c"),
            key: f("k"),
            client_ca: None,
            crls: vec![f("crl")],
        };
        // 未啟用 mTLS 時不監看 CRL
        assert_eq!(tp.watched_files().len(), 2);
        let a = fingerprint(&tp);
        assert_eq!(a, fingerprint(&tp));

        std::fs::write(f("c"), "cert2").unwrap();
        let b = fingerprint(&tp);
        assert_ne!(a, b);

        // 內容改回原樣 → 指紋相同（不受修改時間影響）
        std::fs::write(f("c"), "cert").unwrap();
        assert_eq!(a, fingerprint(&tp));

        // 檔案消失 → 雜湊為 None
        tp.client_ca = Some(f("ca"));
        let c = fingerprint(&tp);
        assert_eq!(c.len(), 4);
        assert_eq!(c[2].1, None);
        assert_eq!(c[3].1, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
