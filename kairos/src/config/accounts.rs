//! 使用者、工作階段（`[accounts]`）、目錄服務（`[ldap]`）與稽核日誌（`[audit]`）的設定
//! （設計包 `kairos/screens.md` S01、S16～S18、S23；`kairos/API.md` 第 12 節）。
//!
//! 秘密不寫在設定檔：LDAP 服務帳號的密碼以 `ldap.bind_password_file` 指向只有服務帳號可讀的檔案
//! （`kairos config show --effective` 只顯示路徑）。

use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind};

/// `[accounts]`：本機帳號、密碼政策、登入失敗鎖定與工作階段
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct AccountSettings {
    /// 密碼至少幾個字元（設計包 S02：至少 12 字元）
    pub password_min_length: usize,
    /// 連續登入失敗幾次後鎖定（S01：5 次）
    pub lockout_threshold: u32,
    /// 鎖定幾分鐘（S01：15 分鐘）
    pub lockout_minutes: u32,
    /// 工作階段閒置多久後失效（分鐘）
    pub session_idle_minutes: u32,
    /// 工作階段從登入起最長多久（小時，絕對逾時）
    pub session_absolute_hours: u32,
    /// Argon2id 的記憶體成本（KiB）
    pub argon2_memory_kib: u32,
    /// Argon2id 的迭代次數
    pub argon2_iterations: u32,
    /// Argon2id 的平行度
    pub argon2_parallelism: u32,
    /// TOTP 驗證器 App 中顯示的發行者名稱
    pub mfa_issuer: String,
}

/// 密碼長度的下限（不可再低）
pub const MIN_PASSWORD_MIN_LENGTH: usize = 8;

impl Default for AccountSettings {
    fn default() -> Self {
        Self {
            password_min_length: 12,
            lockout_threshold: 5,
            lockout_minutes: 15,
            session_idle_minutes: 30,
            session_absolute_hours: 12,
            // OWASP 建議的 Argon2id 最低參數之一：m=19 MiB、t=2、p=1
            argon2_memory_kib: 19_456,
            argon2_iterations: 2,
            argon2_parallelism: 1,
            mfa_issuer: "Kairos".to_string(),
        }
    }
}

fn range(name: &str, v: u64, lo: u64, hi: u64) -> Result<(), Error> {
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{name} 必須介於 {lo}..={hi}（目前為 {v}）"),
        ))
    }
}

impl AccountSettings {
    pub fn validate(&self) -> Result<(), Error> {
        range(
            "accounts.password_min_length",
            self.password_min_length as u64,
            MIN_PASSWORD_MIN_LENGTH as u64,
            128,
        )?;
        range(
            "accounts.lockout_threshold",
            self.lockout_threshold.into(),
            1,
            100,
        )?;
        range(
            "accounts.lockout_minutes",
            self.lockout_minutes.into(),
            1,
            1440,
        )?;
        range(
            "accounts.session_idle_minutes",
            self.session_idle_minutes.into(),
            5,
            1440,
        )?;
        range(
            "accounts.session_absolute_hours",
            self.session_absolute_hours.into(),
            1,
            168,
        )?;
        if u64::from(self.session_idle_minutes) > u64::from(self.session_absolute_hours) * 60 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "accounts.session_idle_minutes 不可大於 accounts.session_absolute_hours"
                    .to_string(),
            ));
        }
        range(
            "accounts.argon2_memory_kib",
            self.argon2_memory_kib.into(),
            8_192,
            1_048_576,
        )?;
        range(
            "accounts.argon2_iterations",
            self.argon2_iterations.into(),
            1,
            10,
        )?;
        range(
            "accounts.argon2_parallelism",
            self.argon2_parallelism.into(),
            1,
            8,
        )?;
        let issuer = self.mfa_issuer.trim();
        if issuer.is_empty() || issuer.chars().count() > 64 || issuer.contains(':') {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "accounts.mfa_issuer 必須是 1～64 個字元，且不含冒號".to_string(),
            ));
        }
        Ok(())
    }

    pub fn idle_ms(&self) -> i64 {
        i64::from(self.session_idle_minutes) * 60_000
    }

    pub fn absolute_ms(&self) -> i64 {
        i64::from(self.session_absolute_hours) * 3_600_000
    }

    pub fn lockout_ms(&self) -> i64 {
        i64::from(self.lockout_minutes) * 60_000
    }
}

/// `[ldap]`：LDAP／AD 的 bind 驗證與群組對應（S17）
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct LdapSettings {
    /// 啟用目錄服務登入
    pub enabled: bool,
    /// `ldaps://dc01.example.gov.tw:636`；`ldap://…:389` 需搭配 `starttls = true`
    pub url: String,
    /// 備援伺服器（依序嘗試；也接受以逗號分隔的單一字串）
    #[serde(deserialize_with = "super::string_or_list")]
    pub fallback_urls: Vec<String>,
    /// `ldap://` 以 StartTLS 升級
    pub starttls: bool,
    /// 允許未加密的 `ldap://`（S17：需要管理員明確確認；密碼會以明文經過網路）
    pub allow_plaintext: bool,
    /// 簽發網域控制站憑證的 CA（PEM）；使用 TLS 時必填，伺服器憑證一律驗證（含名稱）
    pub ca_file: String,
    /// 服務帳號的 DN（搜尋使用者用）；空字串時以匿名 bind 搜尋
    pub bind_dn: String,
    /// 服務帳號密碼所在的檔案（只有服務帳號可讀；內容的前後空白會被去除）
    pub bind_password_file: String,
    /// 使用者搜尋基底
    pub user_base: String,
    /// 額外的篩選條件
    pub user_filter: String,
    /// 登入屬性
    pub login_attribute: String,
    pub display_name_attribute: String,
    pub email_attribute: String,
    /// 使用者所屬群組的屬性（AD 為 `memberOf`）
    pub group_attribute: String,
    /// 連線與每個操作的逾時（秒）
    pub timeout_secs: u64,
}

impl Default for LdapSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            fallback_urls: Vec::new(),
            starttls: false,
            allow_plaintext: false,
            ca_file: String::new(),
            bind_dn: String::new(),
            bind_password_file: String::new(),
            user_base: String::new(),
            // AD 的電腦帳號（HOST$）也是 objectClass=user：只比對人員
            user_filter: "(&(objectCategory=person)(objectClass=user))".to_string(),
            login_attribute: "sAMAccountName".to_string(),
            display_name_attribute: "displayName".to_string(),
            email_attribute: "mail".to_string(),
            group_attribute: "memberOf".to_string(),
            timeout_secs: 10,
        }
    }
}

/// 屬性名稱：字母開頭，字母、數字與 `-`
fn valid_attribute(a: &str) -> bool {
    let mut chars = a.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
        && a.len() <= 64
}

impl LdapSettings {
    /// 所有伺服器（主要＋備援）
    pub fn urls(&self) -> Vec<String> {
        std::iter::once(self.url.trim().to_string())
            .chain(self.fallback_urls.iter().map(|u| u.trim().to_string()))
            .filter(|u| !u.is_empty())
            .collect()
    }

    pub fn validate(&self) -> Result<(), Error> {
        if !self.enabled {
            return Ok(());
        }
        let invalid = |msg: String| Err(Error::new(ErrorKind::InvalidInput, msg));
        let urls = self.urls();
        if urls.is_empty() {
            return invalid("ldap.enabled = true 時 ldap.url 必填".to_string());
        }
        for u in &urls {
            let lower = u.to_ascii_lowercase();
            if lower.starts_with("ldaps://") {
                if self.starttls {
                    return invalid(format!("ldap.starttls 只用於 ldap://（{u} 已是 LDAPS）"));
                }
            } else if lower.starts_with("ldap://") {
                if !self.starttls && !self.allow_plaintext {
                    return invalid(format!(
                        "{u} 未加密：改用 ldaps://、設定 ldap.starttls = true，或在確認風險後設定 ldap.allow_plaintext = true（密碼會以明文經過網路）"
                    ));
                }
            } else {
                return invalid(format!("ldap.url 必須是 ldaps:// 或 ldap://（目前為 {u}）"));
            }
        }
        let tls = self.starttls
            || urls
                .iter()
                .any(|u| u.to_ascii_lowercase().starts_with("ldaps://"));
        if tls && self.ca_file.trim().is_empty() {
            return invalid(
                "使用 LDAPS 或 StartTLS 時 ldap.ca_file 必填（簽發網域控制站憑證的 CA，PEM）"
                    .to_string(),
            );
        }
        if self.user_base.trim().is_empty() {
            return invalid("ldap.user_base 必填（使用者搜尋基底）".to_string());
        }
        if !self.bind_dn.trim().is_empty() && self.bind_password_file.trim().is_empty() {
            return invalid("設定 ldap.bind_dn 時 ldap.bind_password_file 必填".to_string());
        }
        for (k, v) in [
            ("ldap.login_attribute", &self.login_attribute),
            ("ldap.display_name_attribute", &self.display_name_attribute),
            ("ldap.email_attribute", &self.email_attribute),
            ("ldap.group_attribute", &self.group_attribute),
        ] {
            if !valid_attribute(v.trim()) {
                return invalid(format!("{k} 不是有效的屬性名稱（目前為 {v}）"));
            }
        }
        let f = self.user_filter.trim();
        if !f.is_empty() && !(f.starts_with('(') && f.ends_with(')')) {
            return invalid(format!(
                "ldap.user_filter 必須以括號包住，例如 (objectClass=user)（目前為 {f}）"
            ));
        }
        range("ldap.timeout_secs", self.timeout_secs, 1, 60)?;
        Ok(())
    }
}

/// `[audit]`：稽核日誌
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct AuditSettings {
    /// 保留天數（獨立於 report 的保留天數；設計包 S18：預設 3 年）
    pub retention_days: u32,
    /// 每隔幾分鐘把鏈尾的雜湊寫一行到日誌（`audit.chain_anchor`，`KRS-I6202`；經 syslog
    /// 外送後成為資料庫之外的副本）。0 為不寫；5～1440，預設 60
    pub anchor_log_minutes: u32,
    /// 每隔幾小時自我驗證一次雜湊鏈與 HMAC（失敗時 `KRS-E6201` 並開「稽核日誌完整性」事件）。
    /// 1～168，預設 24（設計包 §28.5 #10）；不提供關閉
    pub verify_interval_hours: u32,
}

impl Default for AuditSettings {
    fn default() -> Self {
        Self {
            retention_days: 1095,
            anchor_log_minutes: 60,
            verify_interval_hours: 24,
        }
    }
}

impl AuditSettings {
    pub fn validate(&self) -> Result<(), Error> {
        range(
            "audit.retention_days",
            self.retention_days.into(),
            90,
            36_500,
        )?;
        if self.anchor_log_minutes != 0 {
            range(
                "audit.anchor_log_minutes",
                self.anchor_log_minutes.into(),
                5,
                1440,
            )?;
        }
        range(
            "audit.verify_interval_hours",
            self.verify_interval_hours.into(),
            1,
            168,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_ranges_are_checked() {
        assert!(AccountSettings::default().validate().is_ok());
        assert!(LdapSettings::default().validate().is_ok());
        assert!(AuditSettings::default().validate().is_ok());
        let mut a = AccountSettings {
            password_min_length: 7,
            ..AccountSettings::default()
        };
        assert!(a.validate().is_err());
        a.password_min_length = 12;
        a.session_idle_minutes = 24 * 60;
        a.session_absolute_hours = 12;
        assert!(a.validate().is_err());
        a = AccountSettings {
            argon2_memory_kib: 1024,
            ..AccountSettings::default()
        };
        assert!(a.validate().is_err());
        let audit = |retention_days, anchor_log_minutes, verify_interval_hours| AuditSettings {
            retention_days,
            anchor_log_minutes,
            verify_interval_hours,
        };
        assert!(audit(30, 60, 24).validate().is_err());
        assert!(audit(1095, 0, 24).validate().is_ok());
        assert!(audit(1095, 4, 24).validate().is_err());
        assert!(audit(1095, 60, 0).validate().is_err());
        assert!(audit(1095, 60, 169).validate().is_err());
    }

    #[test]
    fn ldap_requires_tls_with_a_ca_unless_plaintext_is_explicit() {
        let base = LdapSettings {
            enabled: true,
            url: "ldaps://dc01.example.gov.tw:636".into(),
            ca_file: "/opt/kairos/ad-ca.pem".into(),
            user_base: "OU=Staff,DC=example,DC=gov,DC=tw".into(),
            ..LdapSettings::default()
        };
        assert!(base.validate().is_ok());
        // 沒有 CA：拒絕（不使用系統信任清單，也不提供略過驗證的選項）
        let mut l = LdapSettings {
            ca_file: String::new(),
            ..base.clone()
        };
        assert!(l.validate().is_err());
        // 未加密：需要 StartTLS 或明確允許
        l = LdapSettings {
            url: "ldap://dc01:389".into(),
            ..base.clone()
        };
        assert!(l.validate().is_err());
        l.starttls = true;
        assert!(l.validate().is_ok());
        l.starttls = false;
        l.allow_plaintext = true;
        l.ca_file = String::new();
        assert!(l.validate().is_ok());
        // LDAPS 不能同時 StartTLS；其他協定、缺少搜尋基底、屬性名稱不合法、bind 沒有密碼檔
        for bad in [
            LdapSettings {
                starttls: true,
                ..base.clone()
            },
            LdapSettings {
                url: "http://dc01".into(),
                ..base.clone()
            },
            LdapSettings {
                user_base: " ".into(),
                ..base.clone()
            },
            LdapSettings {
                login_attribute: "sAM)(x".into(),
                ..base.clone()
            },
            LdapSettings {
                bind_dn: "CN=svc".into(),
                ..base.clone()
            },
            LdapSettings {
                user_filter: "objectClass=user".into(),
                ..base.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        assert_eq!(
            LdapSettings {
                fallback_urls: vec!["ldaps://dc02:636".into(), " ".into()],
                ..base
            }
            .urls()
            .len(),
            2
        );
    }
}
