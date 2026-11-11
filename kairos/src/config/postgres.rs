//! `[storage.postgres]`：PostgreSQL 後端的連線設定（`kairos/HA.md` 第 4.3、15.1 節）。
//!
//! 只在 `storage.backend = "postgres"` 時使用與檢查。密碼不寫在設定檔，只從 `password_file` 讀取
//! （與 `ldap.bind_password_file` 相同的慣例）；這個區段的值都不是機密，可以出現在日誌與 `kairos diag`。

use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind};
use std::net::IpAddr;
use std::time::Duration;

/// `storage.postgres.pool_size` 的範圍與預設值（每個 Kairos 行程）
pub const MIN_PG_POOL_SIZE: usize = 2;
pub const MAX_PG_POOL_SIZE: usize = 128;
pub const DEFAULT_PG_POOL_SIZE: usize = 16;

/// `storage.postgres.sslmode`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum PgSslMode {
    /// 加密，並以 `ca_file`（未設定時為作業系統的信任清單）驗證伺服器憑證與主機名稱
    #[serde(rename = "verify-full")]
    VerifyFull,
    /// 加密但不驗證伺服器憑證（擋得住被動竊聽，擋不住中間人）
    #[serde(rename = "require")]
    Require,
    /// 不加密
    #[serde(rename = "disable")]
    Disable,
}

impl PgSslMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PgSslMode::VerifyFull => "verify-full",
            PgSslMode::Require => "require",
            PgSslMode::Disable => "disable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct PostgresSettings {
    /// 主機名稱、IP，或 Unix socket 的目錄（以 `/` 開頭；只有 Unix）
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    /// 密碼檔（內容去掉結尾的換行）。空字串：不送密碼（peer／trust 驗證）
    pub password_file: String,
    /// Kairos 的資料表所在的 schema（不存在時建立）
    pub schema: String,
    /// 未設定時：loopback 與 Unix socket 為 `disable`，其他為 `verify-full`
    pub sslmode: Option<PgSslMode>,
    /// 驗證伺服器憑證的 CA（PEM，可含多張）。空字串：作業系統的信任清單
    pub ca_file: String,
    /// 連線池的連線數上限
    pub pool_size: usize,
    pub connect_timeout_secs: u64,
}

impl Default for PostgresSettings {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 5432,
            dbname: "kairos".to_string(),
            user: "kairos".to_string(),
            password_file: String::new(),
            schema: "kairos".to_string(),
            sslmode: None,
            ca_file: String::new(),
            pool_size: DEFAULT_PG_POOL_SIZE,
            connect_timeout_secs: 5,
        }
    }
}

fn invalid<T>(msg: String) -> Result<T, Error> {
    Err(Error::new(ErrorKind::InvalidInput, msg))
}

/// schema 名稱：小寫英文字母或底線開頭，之後是小寫英文字母、數字或底線，最多 63 個字元。
/// 名稱會直接寫進 SQL（`SET search_path`、`CREATE SCHEMA`），所以只接受不需要引號的識別字
pub fn is_schema_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && s.len() <= 63
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !s.starts_with("pg_")
}

impl PostgresSettings {
    /// `host` 是 Unix socket 的目錄
    pub fn is_unix_socket(&self) -> bool {
        self.host.starts_with('/')
    }

    /// 連線不離開本機（Unix socket、loopback 位址或 `localhost`）
    pub fn is_local(&self) -> bool {
        let host = self.host.trim();
        self.is_unix_socket()
            || host.eq_ignore_ascii_case("localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .is_ok_and(super::is_loopback)
    }

    /// 實際使用的 sslmode（未設定時依 `host` 決定）
    pub fn effective_sslmode(&self) -> PgSslMode {
        match self.sslmode {
            Some(m) => m,
            None if self.is_local() => PgSslMode::Disable,
            None => PgSslMode::VerifyFull,
        }
    }

    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs)
    }

    /// 顯示用的位置（不含密碼）：`postgres://user@host:port/dbname（schema）`
    pub fn display_target(&self) -> String {
        format!(
            "postgres://{}@{}:{}/{}（schema {}）",
            self.user, self.host, self.port, self.dbname, self.schema
        )
    }

    /// 只在 `storage.backend = "postgres"` 時呼叫
    pub fn validate(&self) -> Result<(), Error> {
        if self.host.trim().is_empty() {
            return invalid("storage.postgres.host 不可為空".to_string());
        }
        if cfg!(not(unix)) && self.is_unix_socket() {
            return invalid(format!(
                "storage.postgres.host「{}」是 Unix socket 的目錄，這個平台不支援；請填主機名稱或 IP",
                self.host
            ));
        }
        if self.port == 0 {
            return invalid("storage.postgres.port 不可為 0".to_string());
        }
        for (key, value) in [("dbname", &self.dbname), ("user", &self.user)] {
            if value.trim().is_empty() || value.contains('\0') {
                return invalid(format!("storage.postgres.{key} 不可為空"));
            }
        }
        if !is_schema_name(&self.schema) {
            return invalid(format!(
                "storage.postgres.schema「{}」不合法：必須以小寫英文字母或 _ 開頭，只含小寫英文字母、數字與 _，最多 63 個字元，且不以 pg_ 開頭",
                self.schema
            ));
        }
        if !(MIN_PG_POOL_SIZE..=MAX_PG_POOL_SIZE).contains(&self.pool_size) {
            return invalid(format!(
                "storage.postgres.pool_size 必須介於 {MIN_PG_POOL_SIZE}..={MAX_PG_POOL_SIZE}（目前為 {}）",
                self.pool_size
            ));
        }
        if !(1..=120).contains(&self.connect_timeout_secs) {
            return invalid(format!(
                "storage.postgres.connect_timeout_secs 必須介於 1..=120（目前為 {}）",
                self.connect_timeout_secs
            ));
        }
        if self.is_unix_socket() && self.sslmode.is_some_and(|m| m != PgSslMode::Disable) {
            return invalid(
                "storage.postgres.host 是 Unix socket 時不使用 TLS；請移除 storage.postgres.sslmode 或設為 \"disable\"".to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sslmode_defaults_by_host() {
        let mut p = PostgresSettings::default();
        assert!(p.is_local());
        assert_eq!(p.effective_sslmode(), PgSslMode::Disable);
        for local in [
            "localhost",
            "::1",
            "[::1]",
            "127.0.0.2",
            "/var/run/postgresql",
        ] {
            p.host = local.to_string();
            assert_eq!(p.effective_sslmode(), PgSslMode::Disable, "{local}");
        }
        for remote in ["db.example.com", "10.0.0.5", "2001:db8::1"] {
            p.host = remote.to_string();
            assert!(!p.is_local(), "{remote}");
            assert_eq!(p.effective_sslmode(), PgSslMode::VerifyFull, "{remote}");
        }
        p.sslmode = Some(PgSslMode::Disable);
        assert_eq!(p.effective_sslmode(), PgSslMode::Disable);
    }

    #[test]
    fn invalid_values_are_rejected() {
        let ok = PostgresSettings::default();
        assert!(ok.validate().is_ok());
        let cases: Vec<(&str, Box<dyn Fn(&mut PostgresSettings)>)> = vec![
            ("host", Box::new(|p| p.host = " ".into())),
            ("port", Box::new(|p| p.port = 0)),
            ("dbname", Box::new(|p| p.dbname = String::new())),
            ("user", Box::new(|p| p.user = String::new())),
            ("schema", Box::new(|p| p.schema = "Kairos".into())),
            ("schema", Box::new(|p| p.schema = "a-b".into())),
            ("schema", Box::new(|p| p.schema = "pg_catalog".into())),
            ("schema", Box::new(|p| p.schema = "1a".into())),
            ("pool_size", Box::new(|p| p.pool_size = 1)),
            ("pool_size", Box::new(|p| p.pool_size = 129)),
            (
                "connect_timeout_secs",
                Box::new(|p| p.connect_timeout_secs = 0),
            ),
        ];
        for (key, change) in cases {
            let mut p = ok.clone();
            change(&mut p);
            let e = p.validate().unwrap_err().to_string();
            assert!(e.contains(&format!("storage.postgres.{key}")), "{key}: {e}");
        }
        assert!(is_schema_name("kairos_test_1") && is_schema_name("_k"));
    }

    #[test]
    fn display_target_has_no_secret() {
        let p = PostgresSettings {
            password_file: "/etc/kairos/pg.password".into(),
            ..PostgresSettings::default()
        };
        assert_eq!(
            p.display_target(),
            "postgres://kairos@127.0.0.1:5432/kairos（schema kairos）"
        );
    }
}
