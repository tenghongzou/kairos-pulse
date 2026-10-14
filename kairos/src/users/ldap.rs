//! LDAP／AD 的 bind 驗證與群組（設計包 S01「機關帳號（AD）」、S17；`kairos/API.md` 第 12.7 節）。
//!
//! 流程：連線（LDAPS 或 StartTLS，**一律驗證伺服器憑證與名稱**，只信任 `ldap.ca_file` 中的 CA）→
//! 以服務帳號 bind（沒有設定時匿名）→ 在 `ldap.user_base` 以 `(&<user_filter>(<login_attribute>=<帳號>))`
//! 搜尋（帳號經過 RFC 4515 跳脫）→ 以找到的 DN 與使用者輸入的密碼 bind → 讀取群組屬性。
//!
//! - **空密碼一律拒絕**：LDAP 的 simple bind 以空密碼會成為匿名 bind（「未驗證的 bind」）並回成功。
//! - 搜尋結果不是恰好一筆時視為查無使用者（不猜測）。
//! - 錯誤分類對應 `copywriting.md` 的 `KRS-E6002`、`E6005`～`E6009`；登入畫面只顯示不透露帳號是否
//!   存在的訊息（查無使用者與密碼錯誤同為 `KRS-E6001`）。

use ldap3::{LdapConnAsync, LdapConnSettings, Scope, SearchEntry, ldap_escape};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio_rustls::rustls;

use crate::config::LdapSettings;

/// 目錄服務中的使用者
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryUser {
    pub dn: String,
    /// 目錄服務中的帳號（登入屬性的值；沒有時為輸入的帳號）
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    /// 群組的 DN（原樣）
    pub groups: Vec<String>,
}

/// 目錄服務的錯誤（對應產品代碼）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirError {
    /// 無法連線（DNS、連接埠、防火牆、逾時）：`KRS-E6002`
    Unreachable(String),
    /// 伺服器憑證不受信任或名稱不符：`KRS-E6005`
    Untrusted(String),
    /// 服務帳號 bind 失敗：`KRS-E6006`
    ServiceBind(String),
    /// 搜尋基底不存在：`KRS-E6007`
    BaseNotFound(String),
    /// 查無使用者（或不只一筆）：`KRS-E6008`（登入時對外顯示為 `KRS-E6001`）
    UserNotFound,
    /// 密碼不正確：`KRS-E6001`
    InvalidCredentials,
    /// 帳號已停用或鎖定（AD 的 data 533／530／531／701／773／775）：`KRS-E6009`
    AccountDisabled,
    /// 設定或其他錯誤（服務帳號密碼檔無法讀取、CA 檔無法讀取…）：`KRS-E6002`
    Other(String),
}

impl DirError {
    pub fn product_code(&self) -> &'static str {
        match self {
            DirError::Unreachable(_) | DirError::Other(_) => "KRS-E6002",
            DirError::Untrusted(_) => "KRS-E6005",
            DirError::ServiceBind(_) => "KRS-E6006",
            DirError::BaseNotFound(_) => "KRS-E6007",
            DirError::UserNotFound => "KRS-E6008",
            DirError::InvalidCredentials => "KRS-E6001",
            DirError::AccountDisabled => "KRS-E6009",
        }
    }

    /// 機器代碼（API 的 `details.reason`）
    pub fn reason(&self) -> &'static str {
        match self {
            DirError::Unreachable(_) => "unreachable",
            DirError::Untrusted(_) => "untrusted_certificate",
            DirError::ServiceBind(_) => "service_bind_failed",
            DirError::BaseNotFound(_) => "search_base_not_found",
            DirError::UserNotFound => "user_not_found",
            DirError::InvalidCredentials => "invalid_credentials",
            DirError::AccountDisabled => "account_disabled",
            DirError::Other(_) => "error",
        }
    }

    /// 給管理員看的說明（日誌、S17 的測試結果；不含密碼）
    pub fn detail(&self) -> String {
        match self {
            DirError::Unreachable(s)
            | DirError::Untrusted(s)
            | DirError::ServiceBind(s)
            | DirError::BaseNotFound(s)
            | DirError::Other(s) => s.clone(),
            DirError::UserNotFound => "查無使用者（或符合的不只一筆）".to_string(),
            DirError::InvalidCredentials => "密碼不正確".to_string(),
            DirError::AccountDisabled => "帳號已停用或鎖定".to_string(),
        }
    }
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 目錄服務的驗證（主控台以 trait 物件使用，測試可以替換）
pub trait Directory: Send + Sync {
    fn authenticate<'a>(
        &'a self,
        username: &'a str,
        password: &'a str,
    ) -> BoxFut<'a, Result<DirectoryUser, DirError>>;
}

/// 以 `[ldap]` 設定連線的實作
pub struct LdapDirectory {
    pub settings: LdapSettings,
}

/// 帳號的檢查：1～256 個字元、不含控制字元
fn valid_login(username: &str) -> bool {
    let n = username.chars().count();
    (1..=256).contains(&n) && !username.chars().any(char::is_control)
}

/// AD 在 bind 失敗（result 49）的診斷訊息中以 `data <hex>` 表示原因；帳號停用、鎖定、過期等
/// 只在密碼正確時才會回報（密碼錯誤是 52e），回報這類原因不會讓猜密碼的人得到額外資訊
fn ad_account_problem(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "data 530", "data 531", "data 532", "data 533", "data 701", "data 773", "data 775",
    ]
    .iter()
    .any(|c| lower.contains(c))
}

/// 只信任 `ldap.ca_file` 中的 CA 的 TLS 設定（rustls＋ring；驗證憑證鏈與名稱）
pub fn tls_config(ca_file: &str) -> Result<Arc<rustls::ClientConfig>, DirError> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    let certs = CertificateDer::pem_file_iter(ca_file)
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .map_err(|e| DirError::Other(format!("無法讀取 ldap.ca_file {ca_file}：{e}")))?;
    for c in certs {
        roots
            .add(c)
            .map_err(|e| DirError::Other(format!("ldap.ca_file 中的憑證無效：{e}")))?;
    }
    if roots.is_empty() {
        return Err(DirError::Other(format!(
            "ldap.ca_file {ca_file} 中沒有憑證"
        )));
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| DirError::Other(format!("TLS 設定錯誤：{e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// 服務帳號的密碼（檔案內容去除前後空白）
fn bind_password(path: &str) -> Result<String, DirError> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| DirError::Other(format!("無法讀取 ldap.bind_password_file {path}：{e}")))
}

fn classify_connect_error(url: &str, e: &ldap3::LdapError) -> DirError {
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("certificate")
        || lower.contains("unknownissuer")
        || lower.contains("notvalidforname")
        || lower.contains("invalid peer certificate")
    {
        DirError::Untrusted(format!("{url}：伺服器憑證不受信任或名稱不符（{text}）"))
    } else {
        DirError::Unreachable(format!("{url}：{text}"))
    }
}

impl LdapDirectory {
    async fn connect(&self, url: &str) -> Result<ldap3::Ldap, DirError> {
        let s = &self.settings;
        let timeout = Duration::from_secs(s.timeout_secs);
        let mut settings = LdapConnSettings::new()
            .set_conn_timeout(timeout)
            .set_starttls(s.starttls);
        let tls = s.starttls || url.to_ascii_lowercase().starts_with("ldaps://");
        if tls {
            settings = settings.set_config(tls_config(s.ca_file.trim())?);
        }
        let (conn, ldap) = LdapConnAsync::with_settings(settings, url)
            .await
            .map_err(|e| classify_connect_error(url, &e))?;
        ldap3::drive!(conn);
        Ok(ldap)
    }

    async fn authenticate_at(
        &self,
        url: &str,
        username: &str,
        password: &str,
    ) -> Result<DirectoryUser, DirError> {
        let s = &self.settings;
        let timeout = Duration::from_secs(s.timeout_secs);
        let mut ldap = self.connect(url).await?;
        // 1. 服務帳號（或匿名）
        let bind_dn = s.bind_dn.trim();
        if !bind_dn.is_empty() {
            let pw = bind_password(s.bind_password_file.trim())?;
            if pw.is_empty() {
                return Err(DirError::ServiceBind(
                    "ldap.bind_password_file 是空的（空密碼會成為匿名 bind）".to_string(),
                ));
            }
            let r = ldap
                .with_timeout(timeout)
                .simple_bind(bind_dn, &pw)
                .await
                .map_err(|e| DirError::Unreachable(format!("{url}：{e}")))?;
            if r.rc != 0 {
                return Err(DirError::ServiceBind(format!(
                    "服務帳號 {bind_dn} bind 失敗（LDAP result {}：{}）",
                    r.rc, r.text
                )));
            }
        }
        // 2. 搜尋使用者
        let login_attr = s.login_attribute.trim();
        let filter = format!(
            "(&{}({}={}))",
            s.user_filter.trim(),
            login_attr,
            ldap_escape(username)
        );
        let attrs = [
            login_attr,
            s.display_name_attribute.trim(),
            s.email_attribute.trim(),
            s.group_attribute.trim(),
        ];
        let result = ldap
            .with_timeout(timeout)
            .search(s.user_base.trim(), Scope::Subtree, &filter, attrs.to_vec())
            .await
            .map_err(|e| DirError::Unreachable(format!("{url}：{e}")))?;
        let (entries, res) = (result.0, result.1);
        match res.rc {
            0 | 4 => {}
            32 => {
                return Err(DirError::BaseNotFound(format!(
                    "搜尋基底 {} 不存在（LDAP result 32）",
                    s.user_base.trim()
                )));
            }
            rc => {
                return Err(DirError::Other(format!(
                    "搜尋失敗（LDAP result {rc}：{}）",
                    res.text
                )));
            }
        }
        // 只接受恰好一筆（referral 等其他回應略過）
        let found: Vec<SearchEntry> = entries.into_iter().map(SearchEntry::construct).collect();
        let [entry] = found.as_slice() else {
            return Err(DirError::UserNotFound);
        };
        // 3. 以使用者的 DN 與密碼 bind
        let r = ldap
            .with_timeout(timeout)
            .simple_bind(&entry.dn, password)
            .await
            .map_err(|e| DirError::Unreachable(format!("{url}：{e}")))?;
        match r.rc {
            0 => {}
            49 if ad_account_problem(&r.text) => return Err(DirError::AccountDisabled),
            49 => return Err(DirError::InvalidCredentials),
            53 => return Err(DirError::AccountDisabled),
            rc => {
                return Err(DirError::Other(format!(
                    "使用者 bind 失敗（LDAP result {rc}：{}）",
                    r.text
                )));
            }
        }
        let _ = ldap.unbind().await;
        let first = |attr: &str| {
            entry
                .attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(attr))
                .and_then(|(_, v)| v.first().cloned())
                .filter(|v| !v.trim().is_empty())
        };
        let groups = entry
            .attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(s.group_attribute.trim()))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        Ok(DirectoryUser {
            dn: entry.dn.clone(),
            username: first(login_attr).unwrap_or_else(|| username.to_string()),
            display_name: first(s.display_name_attribute.trim()),
            email: first(s.email_attribute.trim()),
            groups,
        })
    }
}

impl Directory for LdapDirectory {
    fn authenticate<'a>(
        &'a self,
        username: &'a str,
        password: &'a str,
    ) -> BoxFut<'a, Result<DirectoryUser, DirError>> {
        Box::pin(async move {
            if !valid_login(username) || password.is_empty() {
                // 空密碼：不送出（LDAP 會當成匿名 bind 而回成功）
                return Err(DirError::InvalidCredentials);
            }
            let mut last = DirError::Unreachable("沒有設定 ldap.url".to_string());
            for url in self.settings.urls() {
                match self.authenticate_at(&url, username, password).await {
                    // 只有連不上（或憑證問題）時才改試備援伺服器
                    Err(e @ (DirError::Unreachable(_) | DirError::Untrusted(_))) => last = e,
                    other => return other,
                }
            }
            Err(last)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_product_codes() {
        assert_eq!(
            DirError::Unreachable("x".into()).product_code(),
            "KRS-E6002"
        );
        assert_eq!(DirError::Untrusted("x".into()).product_code(), "KRS-E6005");
        assert_eq!(
            DirError::ServiceBind("x".into()).product_code(),
            "KRS-E6006"
        );
        assert_eq!(
            DirError::BaseNotFound("x".into()).product_code(),
            "KRS-E6007"
        );
        assert_eq!(DirError::UserNotFound.product_code(), "KRS-E6008");
        assert_eq!(DirError::AccountDisabled.product_code(), "KRS-E6009");
        assert!(ad_account_problem(
            "80090308: LdapErr: DSID-0C09044E, comment: AcceptSecurityContext error, data 533, v4563"
        ));
        assert!(!ad_account_problem(
            "80090308: LdapErr: DSID-0C09044E, comment: AcceptSecurityContext error, data 52e, v4563"
        ));
    }

    #[test]
    fn filter_values_are_escaped() {
        // RFC 4515：* ( ) \ NUL 以 \XX 表示，帳號中的特殊字元不能改變篩選條件
        assert_eq!(ldap_escape("a*)(uid=*"), "a\\2a\\29\\28uid=\\2a");
        assert!(!valid_login(""));
        assert!(!valid_login("a\0b"));
        assert!(valid_login("chen.y"));
    }

    #[tokio::test]
    async fn empty_passwords_are_never_sent() {
        let d = LdapDirectory {
            settings: LdapSettings {
                enabled: true,
                url: "ldaps://127.0.0.1:1".into(),
                ..LdapSettings::default()
            },
        };
        assert_eq!(
            d.authenticate("admin", "").await,
            Err(DirError::InvalidCredentials)
        );
    }

    #[tokio::test]
    async fn unreachable_servers_and_missing_ca() {
        let dir = std::env::temp_dir();
        // 沒有 CA 檔：設定錯誤
        let d = LdapDirectory {
            settings: LdapSettings {
                enabled: true,
                url: "ldaps://127.0.0.1:1".into(),
                ca_file: dir.join("no-such-ca.pem").to_string_lossy().into_owned(),
                user_base: "DC=x".into(),
                timeout_secs: 2,
                ..LdapSettings::default()
            },
        };
        let e = d.authenticate("admin", "pw").await.unwrap_err();
        assert_eq!(e.product_code(), "KRS-E6002", "{e:?}");
        // 連不上（明文、連接埠 1）
        let d = LdapDirectory {
            settings: LdapSettings {
                enabled: true,
                url: "ldap://127.0.0.1:1".into(),
                allow_plaintext: true,
                user_base: "DC=x".into(),
                timeout_secs: 2,
                ..LdapSettings::default()
            },
        };
        let e = d.authenticate("admin", "pw").await.unwrap_err();
        assert!(matches!(e, DirError::Unreachable(_)), "{e:?}");
    }

    #[tokio::test]
    async fn tls_requires_the_configured_ca() {
        // 以測試 PKI 的 CA 建立設定：可以；以不是憑證的檔案：錯誤
        let ca = format!(
            "{}/../pulse/testdata/server-ca.pem",
            env!("CARGO_MANIFEST_DIR")
        );
        assert!(tls_config(&ca).is_ok());
        let not_pem = format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR"));
        assert!(tls_config(&not_pem).is_err());
    }
}
