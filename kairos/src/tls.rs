//! TLS 設定：伺服器憑證、mTLS 客戶端 CA 與 CRL 的載入、可原子替換的目前設定，
//! 以及以目前的驗證器重新驗證既有連線的客戶端憑證。

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Error, ErrorKind};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::client::danger::HandshakeSignatureValid;
use tokio_rustls::rustls::pki_types::pem::{self, PemObject};
use tokio_rustls::rustls::pki_types::{
    CertificateDer, CertificateRevocationListDer, PrivateKeyDer, UnixTime,
};
use tokio_rustls::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use tokio_rustls::rustls::server::{NoServerSessionStorage, WebPkiClientVerifier};
use tokio_rustls::rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, RootCertStore, ServerConfig,
    SignatureScheme,
};

use crate::expiry::{self, Expiry, ExpiryKind};

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

/// 監看的 TLS 設定檔種類（熱重載日誌用；F8）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TlsFileKind {
    Cert,
    Key,
    ClientCa,
    Crl,
}

impl TlsFileKind {
    /// 與設定鍵相近的名稱：`cert`、`key`、`client_ca`、`crl`
    pub fn as_str(self) -> &'static str {
        match self {
            TlsFileKind::Cert => "cert",
            TlsFileKind::Key => "key",
            TlsFileKind::ClientCa => "client_ca",
            TlsFileKind::Crl => "crl",
        }
    }
}

impl TlsPaths {
    /// 需要監看變化的所有檔案
    pub fn watched_files(&self) -> Vec<&str> {
        self.watched_files_with_kind()
            .into_iter()
            .map(|(_, path)| path)
            .collect()
    }

    /// 需要監看變化的所有檔案及其種類，順序與 [`TlsPaths::watched_files`]（及 [`fingerprint`]）相同
    pub fn watched_files_with_kind(&self) -> Vec<(TlsFileKind, &str)> {
        let mut files = vec![
            (TlsFileKind::Cert, self.cert.as_str()),
            (TlsFileKind::Key, self.key.as_str()),
        ];
        if let Some(ca) = &self.client_ca {
            files.push((TlsFileKind::ClientCa, ca.as_str()));
            files.extend(self.crls.iter().map(|p| (TlsFileKind::Crl, p.as_str())));
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
    /// 伺服器憑證的到期時間與各 CRL 的下次更新時間（到期預警用；O1）。無法讀出日期的項目不列入
    pub expiries: Vec<Expiry>,
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

    let mut expiries: Vec<Expiry> = expiry::cert_not_after(certs[0].as_ref())
        .map(|at| Expiry {
            kind: ExpiryKind::ServerCert,
            path: paths.cert.clone(),
            index: 0,
            at,
        })
        .into_iter()
        .collect();

    let (verifier, crl_count) = match &paths.client_ca {
        Some(ca_path) => {
            let crls = load_crls(&paths.crls)?;
            for (path, index, crl) in &crls {
                if let Some(at) = expiry::crl_next_update(crl.as_ref()) {
                    expiries.push(Expiry {
                        kind: ExpiryKind::Crl,
                        path: path.clone(),
                        index: *index,
                        at,
                    });
                }
            }
            let count = crls.len();
            let crls = crls.into_iter().map(|(_, _, crl)| crl).collect();
            (Some(load_client_verifier(ca_path, crls)?), count)
        }
        None => (None, 0),
    };

    let builder = ServerConfig::builder();
    let builder = match &verifier {
        Some(v) => builder.with_client_cert_verifier(v.clone()),
        None => builder.with_no_client_auth(),
    };
    let mut config = builder.with_single_cert(certs, key).map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("{} 與 {} 無法組成 TLS 設定: {}", paths.cert, paths.key, e),
        )
    })?;
    if verifier.is_some() {
        // 啟用 mTLS 時停用 session resumption：恢復的 session 不會重新驗證客戶端憑證，
        // 過期或已撤銷的憑證可藉由 resumption 繼續連線。
        // 不保存 session（TLS 1.2 session ID 與 TLS 1.3 stateful resumption），也不發 TLS 1.3 ticket；
        // 預設的 ticketer 本來就不產生 stateless ticket。
        config.session_storage = Arc::new(NoServerSessionStorage {});
        config.send_tls13_tickets = 0;
    }

    Ok(TlsMaterial {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        verifier,
        crl_count,
        expiries,
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

/// 讀取所有 CRL 檔，回傳 (檔案路徑, 在檔案中的序號, CRL)
#[allow(clippy::type_complexity)]
fn load_crls(
    paths: &[String],
) -> Result<Vec<(String, usize, CertificateRevocationListDer<'static>)>, Error> {
    let mut all = Vec::new();
    for path in paths {
        all.extend(
            load_crl_file(path)?
                .into_iter()
                .enumerate()
                .map(|(i, crl)| (path.clone(), i, crl)),
        );
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
    let inner = builder.build().map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("無法以 {} 建立客戶端憑證驗證器: {}", ca_path, e),
        )
    })?;
    // 驗證結果不變；被拒時把憑證歸屬到 agent_id 並記錄（設計包 §24 #3）
    Ok(Arc::new(RecordingVerifier { inner }))
}

// ---- 被拒的客戶端憑證（設計包 §24 #3「握手失敗時從憑證 SAN／序號歸屬到 agent_id」）----

tokio::task_local! {
    /// TLS 握手期間的來源位址：`connection::handle_connection` 以 `scope` 包住握手，
    /// 客戶端憑證被拒時一併記錄。重新驗證現役連線（熱重載）時沒有設定
    pub static HANDSHAKE_PEER: SocketAddr;
}

/// 客戶端憑證被拒的原因（`kairos/API.md` 第 5.2 節）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    /// 已撤銷（`KRS-W1105`）
    Revoked,
    /// 已過期或尚未生效（`KRS-W1108`）
    Expired,
    /// 簽發者不受信任（`KRS-W1109`）
    UnknownIssuer,
    /// 伺服器無法判定撤銷狀態（CRL 過期或缺漏，`KRS-E1203`）：伺服器端的問題
    RevocationStatusUnavailable,
    /// 其他（用途不符等，`KRS-W1105`）
    Other,
}

impl RejectionReason {
    /// 設計包 `copywriting.md` §3.3 的代碼
    pub fn product_code(self) -> &'static str {
        match self {
            RejectionReason::Revoked | RejectionReason::Other => "KRS-W1105",
            RejectionReason::Expired => "KRS-W1108",
            RejectionReason::UnknownIssuer => "KRS-W1109",
            RejectionReason::RevocationStatusUnavailable => "KRS-E1203",
        }
    }

    /// 以 snake_case 表示（與序列化相同）
    pub fn as_str(self) -> &'static str {
        match self {
            RejectionReason::Revoked => "revoked",
            RejectionReason::Expired => "expired",
            RejectionReason::UnknownIssuer => "unknown_issuer",
            RejectionReason::RevocationStatusUnavailable => "revocation_status_unavailable",
            RejectionReason::Other => "other",
        }
    }

    /// [`RejectionReason::as_str`] 的反向（從主機清冊讀回）
    pub fn parse(s: &str) -> Option<Self> {
        [
            RejectionReason::Revoked,
            RejectionReason::Expired,
            RejectionReason::UnknownIssuer,
            RejectionReason::RevocationStatusUnavailable,
            RejectionReason::Other,
        ]
        .into_iter()
        .find(|r| r.as_str() == s)
    }

    fn of(e: &tokio_rustls::rustls::Error) -> Self {
        use tokio_rustls::rustls::Error as TlsError;
        match e {
            TlsError::InvalidCertificate(c) => match c {
                CertificateError::Revoked => RejectionReason::Revoked,
                CertificateError::Expired
                | CertificateError::ExpiredContext { .. }
                | CertificateError::NotValidYet
                | CertificateError::NotValidYetContext { .. } => RejectionReason::Expired,
                CertificateError::UnknownIssuer => RejectionReason::UnknownIssuer,
                CertificateError::UnknownRevocationStatus
                | CertificateError::ExpiredRevocationList
                | CertificateError::ExpiredRevocationListContext { .. } => {
                    RejectionReason::RevocationStatusUnavailable
                }
                _ => RejectionReason::Other,
            },
            _ => RejectionReason::Other,
        }
    }
}

/// 一台主機最近一次被拒的憑證
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertRejection {
    pub at: DateTime<Utc>,
    pub reason: RejectionReason,
    /// 憑證序號（小寫十六進位，去掉前導的 00）
    pub serial: Option<String>,
    /// 來源位址（握手時；重新驗證現役連線時沒有）
    pub peer: Option<IpAddr>,
    /// 同一台主機被拒的累計次數（Kairos 這次啟動以來，加上清冊中讀回的值）
    pub count: u64,
}

/// 記憶體中最多記錄幾台主機（超過時移除最舊的）
pub const MAX_REJECTIONS: usize = 10_000;

/// 被拒的憑證（依 agent_id）。主控台 API 讀取，並定期寫入主機清冊（`hosts.cert_rejected_at`）
#[derive(Debug, Default)]
pub struct RejectionLog {
    /// agent_id → (最近一次, 已寫入清冊的次數)
    entries: Mutex<HashMap<String, (CertRejection, u64)>>,
}

impl RejectionLog {
    pub fn record(
        &self,
        agent_id: String,
        reason: RejectionReason,
        serial: Option<String>,
        peer: Option<IpAddr>,
        at: DateTime<Utc>,
    ) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if !entries.contains_key(&agent_id) && entries.len() >= MAX_REJECTIONS {
            let oldest = entries
                .iter()
                .min_by_key(|(_, (r, _))| r.at)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        let (count, persisted) = entries
            .get(&agent_id)
            .map_or((0, 0), |(r, p)| (r.count, *p));
        entries.insert(
            agent_id,
            (
                CertRejection {
                    at,
                    reason,
                    serial,
                    peer,
                    count: count + 1,
                },
                persisted,
            ),
        );
    }

    pub fn get(&self, agent_id: &str) -> Option<CertRejection> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.get(agent_id).map(|(r, _)| r.clone())
    }

    /// 所有紀錄的複本
    pub fn all(&self) -> HashMap<String, CertRejection> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .map(|(id, (r, _))| (id.clone(), r.clone()))
            .collect()
    }

    /// 取出尚未寫入清冊的紀錄：(agent_id, 最近一次, 上次寫入後新增的次數)，並標為已寫入；
    /// 寫入失敗時以 [`RejectionLog::unsave`] 放回
    pub fn take_dirty(&self) -> Vec<(String, CertRejection, u64)> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .iter_mut()
            .filter(|(_, (r, persisted))| r.count > *persisted)
            .map(|(id, (r, persisted))| {
                let delta = r.count - *persisted;
                *persisted = r.count;
                (id.clone(), r.clone(), delta)
            })
            .collect()
    }

    /// 寫入失敗：這些次數下次再寫
    pub fn unsave(&self, items: &[(String, u64)]) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        for (id, delta) in items {
            if let Some((_, persisted)) = entries.get_mut(id) {
                *persisted = persisted.saturating_sub(*delta);
            }
        }
    }
}

/// 行程內唯一的紀錄（驗證器在 rustls 內部被呼叫，無法另外傳入狀態）
pub fn rejections() -> &'static RejectionLog {
    static LOG: LazyLock<RejectionLog> = LazyLock::new(RejectionLog::default);
    &LOG
}

/// 憑證中第一個是 agent_id（UUID）的 DNS SAN（小寫）
pub fn cert_agent_id(cert: &CertificateDer<'_>) -> Option<String> {
    let ee = webpki::EndEntityCert::try_from(cert).ok()?;
    ee.valid_dns_names()
        .map(kairos_proto::normalize_agent_id)
        .find(|id| kairos_proto::is_uuid_text(id))
}

/// 讀取 DER 的一個 TLV，回傳 (tag, 內容, 其餘)
fn der_tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, mut rest) = rest.split_first()?;
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let mut len = 0usize;
        for &b in &rest[..n] {
            len = (len << 8) | usize::from(b);
        }
        rest = &rest[n..];
        len
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// 憑證的序號（小寫十六進位，去掉前導的 00）：Certificate → tbsCertificate → [0] version（可省略）→ serial
pub fn cert_serial_hex(der: &[u8]) -> Option<String> {
    let (0x30, cert, _) = der_tlv(der)? else {
        return None;
    };
    let (0x30, tbs, _) = der_tlv(cert)? else {
        return None;
    };
    let (mut tag, mut value, rest) = der_tlv(tbs)?;
    if tag == 0xa0 {
        (tag, value, _) = der_tlv(rest)?;
    }
    if tag != 0x02 || value.is_empty() {
        return None;
    }
    while value.len() > 1 && value[0] == 0 {
        value = &value[1..];
    }
    Some(value.iter().map(|b| format!("{b:02x}")).collect())
}

/// 包住 rustls 的 `WebPkiClientVerifier`：驗證結果完全相同，只在 `verify_client_cert` 失敗時把
/// 憑證歸屬到 agent_id（DNS SAN）並記錄原因、序號與來源位址。
///
/// 只歸屬「由信任的 CA 簽發」的憑證：已撤銷、無法判定撤銷狀態（兩者都在鏈驗證之後才檢查），以及
/// 已過期但在效期內驗證得過的憑證。自簽或不受信任的 CA 簽發的憑證（`UnknownIssuer` 等）不歸屬：
/// SAN 由出示者自己決定，否則任何人都能讓別台主機顯示「憑證被拒」，也能以大量假的 agent_id 填滿紀錄
#[derive(Debug)]
struct RecordingVerifier {
    inner: Arc<dyn ClientCertVerifier>,
}

impl RecordingVerifier {
    /// 被拒的憑證是否由信任的 CA 簽發（SAN 可信，可以歸屬到 agent_id）
    fn attested(
        &self,
        reason: RejectionReason,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> bool {
        match reason {
            RejectionReason::Revoked | RejectionReason::RevocationStatusUnavailable => true,
            RejectionReason::Expired => {
                // 以效期的最後一秒重新驗證：鏈可信時通過（或只因撤銷相關的原因失敗）
                let Some(not_after) = crate::expiry::cert_not_after(end_entity.as_ref()) else {
                    return false;
                };
                let Ok(secs) = u64::try_from(not_after.timestamp() - 1) else {
                    return false;
                };
                let at = UnixTime::since_unix_epoch(std::time::Duration::from_secs(secs));
                match self.inner.verify_client_cert(end_entity, intermediates, at) {
                    Ok(_) => true,
                    Err(e) => matches!(
                        RejectionReason::of(&e),
                        RejectionReason::Revoked | RejectionReason::RevocationStatusUnavailable
                    ),
                }
            }
            RejectionReason::UnknownIssuer | RejectionReason::Other => false,
        }
    }
}

impl ClientCertVerifier for RecordingVerifier {
    fn offer_client_auth(&self) -> bool {
        self.inner.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, tokio_rustls::rustls::Error> {
        let result = self
            .inner
            .verify_client_cert(end_entity, intermediates, now);
        if let Err(e) = &result
            && let reason = RejectionReason::of(e)
            && self.attested(reason, end_entity, intermediates)
            && let Some(agent_id) = cert_agent_id(end_entity)
        {
            rejections().record(
                agent_id,
                reason,
                cert_serial_hex(end_entity.as_ref()),
                HANDSHAKE_PEER.try_with(|p| p.ip()).ok(),
                Utc::now(),
            );
        }
        result
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }
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

/// 握手失敗原因中與撤銷相關的部分，用來把這類日誌與一般的握手失敗分開節流
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationFailure {
    /// 客戶端憑證已被撤銷
    Revoked,
    /// CRL 已過期（超過 nextUpdate）或無法判定撤銷狀態：多半是伺服器端的 CRL 需要更新
    StatusUnavailable,
}

/// 判斷 rustls 的憑證驗證錯誤是否與撤銷相關
fn classify_tls_error(e: &tokio_rustls::rustls::Error) -> Option<RevocationFailure> {
    use tokio_rustls::rustls::Error as TlsError;
    match e {
        TlsError::InvalidCertificate(CertificateError::Revoked) => Some(RevocationFailure::Revoked),
        TlsError::InvalidCertificate(
            CertificateError::UnknownRevocationStatus
            | CertificateError::ExpiredRevocationList
            | CertificateError::ExpiredRevocationListContext { .. },
        ) => Some(RevocationFailure::StatusUnavailable),
        _ => None,
    }
}

/// 從握手錯誤（tokio-rustls 以 `io::Error` 包裝的 `rustls::Error`）判斷是否與撤銷相關
pub fn revocation_failure(e: &Error) -> Option<RevocationFailure> {
    classify_tls_error(e.get_ref()?.downcast_ref::<tokio_rustls::rustls::Error>()?)
}

/// `certificate_rejected` 的 message：憑證已被撤銷、過期或不再受信任
pub const MSG_CERTIFICATE_REJECTED: &str =
    "client certificate has been revoked or is no longer trusted";

/// `certificate_rejected` 的 message：伺服器無法判定撤銷狀態（CRL 過期或缺少該層 CA 的 CRL），
/// 屬於伺服器端的問題，客戶端不需要重新簽發憑證（R2-K26；Pulse 依此字串提示聯絡管理者）
pub const MSG_REVOCATION_STATUS_UNAVAILABLE: &str = "revocation status unavailable";

/// 重新驗證客戶端憑證鏈失敗的原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainRejection {
    /// 給日誌的說明
    pub reason: String,
    /// 與撤銷相關時的分類
    pub revocation: Option<RevocationFailure>,
}

impl ChainRejection {
    pub fn new(reason: impl Into<String>, revocation: Option<RevocationFailure>) -> Self {
        Self {
            reason: reason.into(),
            revocation,
        }
    }

    /// 回給客戶端的 `certificate_rejected` message
    pub fn client_message(&self) -> &'static str {
        match self.revocation {
            Some(RevocationFailure::StatusUnavailable) => MSG_REVOCATION_STATUS_UNAVAILABLE,
            _ => MSG_CERTIFICATE_REJECTED,
        }
    }
}

impl std::fmt::Display for ChainRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// 以驗證器檢查握手時取得的客戶端憑證鏈（[0] 為 end-entity，其餘為中繼憑證）。
/// 用於重載 CA/CRL 後與定期檢查憑證是否過期。
pub fn verify_chain(
    verifier: &dyn ClientCertVerifier,
    chain: Option<&[CertificateDer<'static>]>,
    now: UnixTime,
) -> Result<(), ChainRejection> {
    let Some((end_entity, intermediates)) = chain.and_then(|c| c.split_first()) else {
        return Err(ChainRejection::new("連線沒有客戶端憑證", None));
    };
    verifier
        .verify_client_cert(end_entity, intermediates, now)
        .map(|_| ())
        .map_err(|e| ChainRejection::new(e.to_string(), classify_tls_error(&e)))
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
    fn rejected_certificates_are_attributed_to_the_agent_id() {
        // 設計包 §24 #3：撤銷的憑證在驗證失敗時以 SAN 歸屬到 agent_id，記下原因、序號與來源
        let v = verifier(&["crl-revoke-a.der"]);
        let cert = chain("client-a.crt");
        let agent_id = cert_agent_id(&cert[0]).expect("client-a.crt 應有 UUID 形式的 DNS SAN");
        let peer: SocketAddr = "192.0.2.7:51000".parse().unwrap();
        let before = rejections().get(&agent_id).map_or(0, |r| r.count);
        let result = futures_util::FutureExt::now_or_never(HANDSHAKE_PEER.scope(peer, async {
            verify_chain(v.as_ref(), Some(&cert), UnixTime::now())
        }))
        .unwrap();
        assert!(result.is_err());
        let r = rejections().get(&agent_id).unwrap();
        assert_eq!(r.reason, RejectionReason::Revoked);
        assert_eq!(r.reason.product_code(), "KRS-W1105");
        assert_eq!(r.peer, Some(peer.ip()));
        assert!(r.count > before);
        assert_eq!(r.serial, cert_serial_hex(cert[0].as_ref()));
        assert!(
            r.serial
                .as_deref()
                .is_some_and(|s| !s.is_empty() && !s.starts_with("00"))
        );
        // 通過驗證的不記錄（只在 verify_client_cert 失敗時記錄；紀錄是行程內全域的，
        // 其他測試同時記錄的不在這裡檢查）
        assert!(verify_chain(v.as_ref(), Some(&chain("client-b.crt")), UnixTime::now()).is_ok());
        // 過期的憑證：沒有來源位址（不在握手中）
        let v = verifier(&[]);
        let expired = chain("client-expired.crt");
        if let Some(id) = cert_agent_id(&expired[0]) {
            assert!(verify_chain(v.as_ref(), Some(&expired), UnixTime::now()).is_err());
            let r = rejections().get(&id).unwrap();
            assert_eq!(r.reason, RejectionReason::Expired);
            assert_eq!(r.reason.product_code(), "KRS-W1108");
            assert_eq!(r.peer, None);
        }
        // 不受信任的 CA 簽發（SAN 同樣是這個 agent_id）：不歸屬，避免冒用
        let other =
            vec![CertificateDer::from_pem_slice(include_bytes!("../testdata/san.crt")).unwrap()];
        let spoofed = cert_agent_id(&other[0]).unwrap();
        assert!(verify_chain(v.as_ref(), Some(&other), UnixTime::now()).is_err());
        assert!(
            rejections()
                .get(&spoofed)
                .is_none_or(|r| r.reason != RejectionReason::UnknownIssuer)
        );
        for r in [
            RejectionReason::Revoked,
            RejectionReason::Expired,
            RejectionReason::UnknownIssuer,
            RejectionReason::RevocationStatusUnavailable,
            RejectionReason::Other,
        ] {
            assert_eq!(RejectionReason::parse(r.as_str()), Some(r));
        }
    }

    #[test]
    fn rejection_log_is_bounded_and_tracks_unsaved_entries() {
        let log = RejectionLog::default();
        let t0 = Utc::now();
        log.record("a".into(), RejectionReason::Revoked, None, None, t0);
        log.record("a".into(), RejectionReason::Expired, None, None, t0);
        assert_eq!(log.get("a").unwrap().count, 2);
        let dirty = log.take_dirty();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].2, 2);
        assert!(log.take_dirty().is_empty());
        log.unsave(&[("a".to_string(), 2)]);
        log.record("a".into(), RejectionReason::Expired, None, None, t0);
        let dirty = log.take_dirty();
        assert_eq!((dirty[0].1.count, dirty[0].2), (3, 3));
        for i in 0..MAX_REJECTIONS + 5 {
            log.record(
                format!("h{i}"),
                RejectionReason::Other,
                None,
                None,
                t0 + chrono::Duration::seconds(i as i64 + 1),
            );
        }
        assert_eq!(log.all().len(), MAX_REJECTIONS);
        // 最舊的（a）先被移除
        assert!(log.get("a").is_none());
    }

    #[test]
    fn der_serial_parsing() {
        // SEQUENCE { SEQUENCE { [0] { INTEGER 2 }, INTEGER 00 ff 01 } }
        let der = [
            0x30, 0x0c, 0x30, 0x0a, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x03, 0x00, 0xff, 0x01,
        ];
        assert_eq!(cert_serial_hex(&der).as_deref(), Some("ff01"));
        // 沒有 version 欄位（v1 憑證）
        let der = [0x30, 0x05, 0x30, 0x03, 0x02, 0x01, 0x2a];
        assert_eq!(cert_serial_hex(&der).as_deref(), Some("2a"));
        for bad in [&[][..], &[0x30], &[0x30, 0x02, 0x04, 0x00], &[0x30, 0x81]] {
            assert_eq!(cert_serial_hex(bad), None, "{bad:?}");
        }
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
    fn build_collects_expiry_dates() {
        // O1：伺服器憑證的 notAfter 與每份 CRL 的 nextUpdate
        let at = chrono::DateTime::parse_from_rfc3339("2126-09-14T20:51:35Z")
            .unwrap()
            .to_utc();
        let plain = build(&paths(None, &[])).unwrap();
        assert_eq!(
            plain.expiries,
            [Expiry {
                kind: ExpiryKind::ServerCert,
                path: p("server.crt"),
                index: 0,
                at
            }]
        );
        let mtls = build(&paths(
            Some("test-ca.crt"),
            &["crl-revoke-a.crl", "crl-empty.crl"],
        ))
        .unwrap();
        let crls: Vec<_> = mtls
            .expiries
            .iter()
            .filter(|e| e.kind == ExpiryKind::Crl)
            .map(|e| (e.path.clone(), e.index, e.at))
            .collect();
        assert_eq!(
            crls,
            [(p("crl-revoke-a.crl"), 0, at), (p("crl-empty.crl"), 0, at)]
        );
    }

    #[test]
    fn session_resumption_is_disabled_only_with_mtls() {
        // K-S4：恢復的 session 不會重新驗證客戶端憑證，啟用 mTLS 時必須停用
        let mtls = build(&paths(Some("test-ca.crt"), &[])).unwrap();
        let config = mtls.acceptor.config();
        assert_eq!(config.send_tls13_tickets, 0);
        assert!(!config.session_storage.can_cache());
        assert!(!config.ticketer.enabled());

        let plain = build(&paths(None, &[])).unwrap();
        assert!(plain.acceptor.config().session_storage.can_cache());
    }

    #[test]
    fn revocation_failures_are_classified() {
        use tokio_rustls::rustls::Error as TlsError;
        let wrap = |e: TlsError| Error::new(ErrorKind::InvalidData, e);
        assert_eq!(
            revocation_failure(&wrap(TlsError::InvalidCertificate(
                CertificateError::Revoked
            ))),
            Some(RevocationFailure::Revoked)
        );
        for status in [
            CertificateError::ExpiredRevocationList,
            CertificateError::UnknownRevocationStatus,
        ] {
            assert_eq!(
                revocation_failure(&wrap(TlsError::InvalidCertificate(status))),
                Some(RevocationFailure::StatusUnavailable)
            );
        }
        assert_eq!(
            revocation_failure(&wrap(TlsError::InvalidCertificate(
                CertificateError::UnknownIssuer
            ))),
            None
        );
        assert_eq!(revocation_failure(&Error::other("x")), None);
        assert_eq!(revocation_failure(&Error::from(ErrorKind::TimedOut)), None);
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
        assert!(err.reason.to_lowercase().contains("revoked"), "{err}");
        assert_eq!(err.revocation, Some(RevocationFailure::Revoked));
        assert_eq!(err.client_message(), MSG_CERTIFICATE_REJECTED);
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
        assert!(err.reason.to_lowercase().contains("expired"), "{err}");
        assert_eq!(err.client_message(), MSG_CERTIFICATE_REJECTED);
        // 在效期內的時間點則通過（2020-06-01）
        let in_validity = UnixTime::since_unix_epoch(Duration::from_secs(1_590_969_600));
        assert!(verify_chain(v.as_ref(), Some(&chain("client-expired.crt")), in_validity).is_ok());

        assert!(verify_chain(v.as_ref(), None, UnixTime::now()).is_err());
        assert!(verify_chain(v.as_ref(), Some(&[]), UnixTime::now()).is_err());
    }

    #[test]
    fn unknown_revocation_status_has_a_distinct_client_message() {
        // R2-K26：CRL 不涵蓋該憑證的簽發者（或已過期）→ 無法判定撤銷狀態，屬伺服器端問題。
        // test_pki 的 CRL 由另一個 CA 簽發，不適用於 testdata/pki 的憑證
        let dir = std::env::temp_dir().join(format!(
            "kairos-tls-unknown-crl-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let crl = dir.join("other-ca.crl");
        std::fs::write(&crl, crate::test_pki::CRL_EMPTY).unwrap();
        let mut tp = paths(Some("test-ca.crt"), &[]);
        tp.crls = vec![crl.to_string_lossy().to_string()];
        let v = build(&tp).unwrap().verifier.unwrap();
        let err =
            verify_chain(v.as_ref(), Some(&chain("client-b.crt")), UnixTime::now()).unwrap_err();
        assert_eq!(
            err.revocation,
            Some(RevocationFailure::StatusUnavailable),
            "{err}"
        );
        assert_eq!(err.client_message(), "revocation status unavailable");
        std::fs::remove_dir_all(&dir).unwrap();

        // 過期的 CRL 也歸為同一類
        use tokio_rustls::rustls::Error as TlsError;
        assert_eq!(
            classify_tls_error(&TlsError::InvalidCertificate(
                CertificateError::ExpiredRevocationList
            )),
            Some(RevocationFailure::StatusUnavailable)
        );
        // 沒有客戶端憑證、或其他原因：維持原本的 message
        let none = verify_chain(v.as_ref(), None, UnixTime::now()).unwrap_err();
        assert_eq!(none.client_message(), MSG_CERTIFICATE_REJECTED);
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
