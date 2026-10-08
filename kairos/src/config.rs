//! 設定載入：程式內預設 → `kairos.toml`（或 `--config <path>`）→ `KAIROS__<SECTION>__<KEY>` 環境變數。

use config::{Config, Environment, File, FileFormat};
use log::LevelFilter;
use serde::Deserialize;
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

/// 未指定 `--config` 時讀取的設定檔（不存在則略過）
pub const DEFAULT_CONFIG_FILE: &str = "kairos.toml";

/// 逾時設定的上限（秒），避免極大值造成時間運算溢位或連線永久卡住
pub const MAX_TIMEOUT_SECS: u64 = 3600;

/// 同時連線數上限的最大值
pub const MAX_MAX_CONNECTIONS: usize = 65535;

/// reports 與日誌保留天數的上限（0 表示永久保留）
pub const MAX_RETENTION_DAYS: u32 = 3650;

/// TLS 熱重載檢查間隔的上限（秒；0 表示停用）
pub const MAX_RELOAD_INTERVAL_SECS: u64 = 3600;

/// `auth.crl` 以環境變數設定時的清單分隔字元
pub const LIST_SEPARATOR: &str = ",";

pub const USAGE: &str = "用法: kairos [--config <path>]\n\n\
選項:\n  \
--config <path>  指定設定檔（預設讀取目前工作目錄的 kairos.toml，不存在則使用預設值）\n  \
-h, --help       顯示此說明\n\n\
環境變數 KAIROS__<SECTION>__<KEY> 可覆寫設定，例如 KAIROS__SERVER__LISTEN=0.0.0.0:8080";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerSettings,
    pub timeouts: TimeoutSettings,
    pub storage: StorageSettings,
    pub auth: AuthSettings,
    pub log: LogSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ServerSettings {
    pub listen: String,
    pub cert_path: String,
    pub key_path: String,
    pub max_connections: usize,
    /// 每隔幾秒檢查憑證、私鑰、客戶端 CA 與 CRL 檔是否變更（並重新驗證現役連線的客戶端憑證）；0 = 停用
    pub reload_interval_secs: u64,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".to_string(),
            cert_path: "cert.pem".to_string(),
            key_path: "key.pem".to_string(),
            max_connections: 1024,
            reload_interval_secs: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct TimeoutSettings {
    pub handshake_secs: u64,
    pub hello_secs: u64,
    pub write_secs: u64,
}

impl Default for TimeoutSettings {
    fn default() -> Self {
        Self {
            handshake_secs: 10,
            hello_secs: 5,
            write_secs: 5,
        }
    }
}

impl TimeoutSettings {
    pub fn handshake(&self) -> Duration {
        Duration::from_secs(self.handshake_secs)
    }

    pub fn hello(&self) -> Duration {
        Duration::from_secs(self.hello_secs)
    }

    pub fn write(&self) -> Duration {
        Duration::from_secs(self.write_secs)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct StorageSettings {
    pub data_dir: String,
    /// reports-YYYY-MM-DD.jsonl 保留天數（今天與前 N 天保留），0 = 永久保留
    pub retention_days: u32,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            data_dir: "data".to_string(),
            retention_days: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct AuthSettings {
    /// 非空：要求客戶端憑證，以此 PEM（可含多張 CA）驗證，並強制 agent_id 與憑證綁定
    pub client_ca: String,
    /// CRL 檔路徑清單（PEM 或 DER）；只在啟用 mTLS 時使用
    pub crl: Vec<String>,
}

impl AuthSettings {
    /// 啟用 mTLS 時回傳 CA 檔路徑
    pub fn client_ca_path(&self) -> Option<&str> {
        let path = self.client_ca.trim();
        (!path.is_empty()).then_some(path)
    }

    /// 去除空白項目後的 CRL 路徑清單
    pub fn crl_paths(&self) -> Vec<String> {
        self.crl
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LogSettings {
    pub level: String,
    /// logs/kairos-YYYY-MM-DD.log 保留天數（今天與前 N 天），0 = 永久保留
    pub retention_days: u32,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            retention_days: 14,
        }
    }
}

impl LogSettings {
    pub fn level_filter(&self) -> Result<LevelFilter, Error> {
        LevelFilter::from_str(self.level.trim()).map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "log.level 的值 \"{}\" 無效，可用值: off、error、warn、info、debug、trace",
                    self.level
                ),
            )
        })
    }
}

/// 命令列解析結果
#[derive(Debug, PartialEq, Eq)]
pub enum CliAction {
    Run { config_path: Option<PathBuf> },
    Help,
}

/// 解析命令列參數（不含程式名稱）。只支援 `--config <path>`、`--config=<path>` 與 `-h/--help`。
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<CliAction, String> {
    let mut config_path = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let path = if arg == "--config" {
            match iter.next() {
                Some(p) if !p.is_empty() => p,
                _ => return Err("--config 需要指定設定檔路徑".to_string()),
            }
        } else if let Some(p) = arg.strip_prefix("--config=") {
            if p.is_empty() {
                return Err("--config 需要指定設定檔路徑".to_string());
            }
            p.to_string()
        } else if arg == "-h" || arg == "--help" {
            return Ok(CliAction::Help);
        } else {
            return Err(format!("無法辨識的參數: {}", arg));
        };
        if config_path.replace(PathBuf::from(path)).is_some() {
            return Err("--config 只能指定一次".to_string());
        }
    }
    Ok(CliAction::Run { config_path })
}

impl AppConfig {
    /// 依序疊加預設值、設定檔與環境變數
    pub fn load(config_path: Option<&Path>) -> Result<Self, Error> {
        let file_source = match config_path {
            Some(path) => {
                // 明確指定的設定檔必須存在
                if !path.is_file() {
                    return Err(Error::new(
                        ErrorKind::NotFound,
                        format!("找不到設定檔 {}", path.display()),
                    ));
                }
                File::from(path).format(FileFormat::Toml).required(true)
            }
            None => File::from(Path::new(DEFAULT_CONFIG_FILE))
                .format(FileFormat::Toml)
                .required(false),
        };
        let source_name = config_path
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| DEFAULT_CONFIG_FILE.to_string());

        let settings: AppConfig = Config::builder()
            .add_source(file_source)
            .add_source(
                Environment::with_prefix("KAIROS")
                    .prefix_separator("__")
                    .separator("__")
                    .try_parsing(true)
                    // KAIROS__AUTH__CRL=a.crl,b.crl
                    .list_separator(LIST_SEPARATOR)
                    .with_list_parse_key("auth.crl"),
            )
            .build()
            .and_then(|c| c.try_deserialize())
            .map_err(|e| {
                Error::new(
                    ErrorKind::InvalidData,
                    format!("載入設定失敗（{} / KAIROS__ 環境變數）: {}", source_name, e),
                )
            })?;

        settings.validate()?;
        Ok(settings)
    }

    /// 檢查不合理的設定值，避免伺服器以無法運作的狀態啟動
    pub fn validate(&self) -> Result<(), Error> {
        let invalid = |msg: &str| Err(Error::new(ErrorKind::InvalidInput, msg.to_string()));
        if self.server.listen.trim().is_empty() {
            return invalid("server.listen 不可為空");
        }
        if !(1..=MAX_MAX_CONNECTIONS).contains(&self.server.max_connections) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "server.max_connections 必須介於 1..={}（目前為 {}）",
                    MAX_MAX_CONNECTIONS, self.server.max_connections
                ),
            ));
        }
        for (name, value) in [
            ("timeouts.handshake_secs", self.timeouts.handshake_secs),
            ("timeouts.hello_secs", self.timeouts.hello_secs),
            ("timeouts.write_secs", self.timeouts.write_secs),
        ] {
            if !(1..=MAX_TIMEOUT_SECS).contains(&value) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "{} 必須介於 1..={}（目前為 {}）",
                        name, MAX_TIMEOUT_SECS, value
                    ),
                ));
            }
        }
        if self.storage.data_dir.trim().is_empty() {
            return invalid("storage.data_dir 不可為空");
        }
        if self.server.reload_interval_secs > MAX_RELOAD_INTERVAL_SECS {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "server.reload_interval_secs 必須介於 0..={}（目前為 {}）",
                    MAX_RELOAD_INTERVAL_SECS, self.server.reload_interval_secs
                ),
            ));
        }
        if self.log.retention_days > MAX_RETENTION_DAYS {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "log.retention_days 必須介於 0..={}（目前為 {}）",
                    MAX_RETENTION_DAYS, self.log.retention_days
                ),
            ));
        }
        if !self.auth.crl_paths().is_empty() && self.auth.client_ca_path().is_none() {
            return invalid("auth.crl 只在設定 auth.client_ca（啟用 mTLS）時有效");
        }
        if self.storage.retention_days > MAX_RETENTION_DAYS {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "storage.retention_days 必須介於 0..={}（目前為 {}）",
                    MAX_RETENTION_DAYS, self.storage.retention_days
                ),
            ));
        }
        self.log.level_filter()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_match_spec() {
        let c = AppConfig::default();
        assert_eq!(c.server.listen, "127.0.0.1:8080");
        assert_eq!(c.server.cert_path, "cert.pem");
        assert_eq!(c.server.key_path, "key.pem");
        assert_eq!(c.server.max_connections, 1024);
        assert_eq!(c.timeouts.handshake_secs, 10);
        assert_eq!(c.timeouts.hello_secs, 5);
        assert_eq!(c.timeouts.write_secs, 5);
        assert_eq!(c.storage.data_dir, "data");
        assert_eq!(c.storage.retention_days, 30);
        assert_eq!(c.server.reload_interval_secs, 30);
        assert_eq!(c.log.retention_days, 14);
        assert!(c.auth.crl.is_empty());
        assert_eq!(c.auth.client_ca, "");
        assert_eq!(c.auth.client_ca_path(), None);
        assert_eq!(c.log.level, "info");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn auth_and_retention_settings() {
        let c: AppConfig = Config::builder()
            .add_source(File::from_str(
                "[auth]\nclient_ca = \"ca.pem\"\n[storage]\nretention_days = 0\n",
                FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();
        assert_eq!(c.auth.client_ca_path(), Some("ca.pem"));
        assert_eq!(c.storage.retention_days, 0);
        assert_eq!(c.storage.data_dir, "data");
        assert!(c.validate().is_ok());

        let blank = AuthSettings {
            client_ca: "   ".into(),
            ..Default::default()
        };
        assert_eq!(blank.client_ca_path(), None);

        let mut c = AppConfig::default();
        c.server.reload_interval_secs = MAX_RELOAD_INTERVAL_SECS;
        c.log.retention_days = MAX_RETENTION_DAYS;
        c.storage.retention_days = MAX_RETENTION_DAYS;
        assert!(c.validate().is_ok());
        c.storage.retention_days = MAX_RETENTION_DAYS + 1;
        assert!(c.validate().is_err());

        let mut c = AppConfig::default();
        c.server.reload_interval_secs = MAX_RELOAD_INTERVAL_SECS + 1;
        assert!(c.validate().is_err());
        let mut c = AppConfig::default();
        c.log.retention_days = MAX_RETENTION_DAYS + 1;
        assert!(c.validate().is_err());

        // CRL 需要 client_ca
        let mut c = AppConfig::default();
        c.auth.crl = vec!["a.crl".into()];
        assert!(c.validate().is_err());
        c.auth.client_ca = "ca.pem".into();
        assert!(c.validate().is_ok());
        c.auth.crl = vec!["  ".into(), "b.crl".into()];
        assert_eq!(c.auth.crl_paths(), ["b.crl"]);
    }

    #[test]
    fn example_file_equals_defaults() {
        let c: AppConfig = Config::builder()
            .add_source(File::from_str(
                include_str!("../kairos.example.toml"),
                FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();
        assert_eq!(c, AppConfig::default());
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let c: AppConfig = Config::builder()
            .add_source(File::from_str(
                "[server]\nlisten = \"0.0.0.0:9000\"\n",
                FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();
        assert_eq!(c.server.listen, "0.0.0.0:9000");
        assert_eq!(c.server.max_connections, 1024);
        assert_eq!(c.timeouts, TimeoutSettings::default());
    }

    #[test]
    fn missing_explicit_config_is_error() {
        let err = AppConfig::load(Some(Path::new("does-not-exist/kairos.toml"))).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn validate_rejects_bad_values() {
        let mut c = AppConfig::default();
        c.server.max_connections = 0;
        assert!(c.validate().is_err());

        let mut c = AppConfig::default();
        c.log.level = "verbose".into();
        assert!(c.validate().is_err());

        let mut c = AppConfig::default();
        c.timeouts.hello_secs = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_upper_bounds() {
        let mut c = AppConfig::default();
        c.timeouts.handshake_secs = MAX_TIMEOUT_SECS;
        c.timeouts.hello_secs = MAX_TIMEOUT_SECS;
        c.timeouts.write_secs = MAX_TIMEOUT_SECS;
        c.server.max_connections = MAX_MAX_CONNECTIONS;
        assert!(c.validate().is_ok());

        let mutations: [fn(&mut AppConfig); 4] = [
            |c| c.timeouts.handshake_secs = MAX_TIMEOUT_SECS + 1,
            |c| c.timeouts.hello_secs = u64::MAX,
            |c| c.timeouts.write_secs = MAX_TIMEOUT_SECS + 1,
            |c| c.server.max_connections = MAX_MAX_CONNECTIONS + 1,
        ];
        for mutate in mutations {
            let mut c = AppConfig::default();
            mutate(&mut c);
            assert!(c.validate().is_err());
        }
    }

    #[test]
    fn parse_args_variants() {
        assert_eq!(
            parse_args(args(&[])),
            Ok(CliAction::Run { config_path: None })
        );
        assert_eq!(
            parse_args(args(&["--config", "a.toml"])),
            Ok(CliAction::Run {
                config_path: Some(PathBuf::from("a.toml"))
            })
        );
        assert_eq!(
            parse_args(args(&["--config=b.toml"])),
            Ok(CliAction::Run {
                config_path: Some(PathBuf::from("b.toml"))
            })
        );
        assert_eq!(parse_args(args(&["--help"])), Ok(CliAction::Help));
        assert!(parse_args(args(&["--config"])).is_err());
        assert!(parse_args(args(&["--config="])).is_err());
        assert!(parse_args(args(&["--config", "a", "--config", "b"])).is_err());
        assert!(parse_args(args(&["--port", "1"])).is_err());
    }
}
