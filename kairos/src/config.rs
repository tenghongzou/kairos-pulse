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

/// `timeouts.write_secs` 的上限（秒）。
///
/// 等待寫檔期間同一連線的 heartbeat 也拿不到 `ack`。
/// 從收到 report 到回應的最壞延遲預算（見 `connection::MAX_REPORT_RESPONSE_DELAY`）：
/// 限速延後 0.5 秒 + `write_secs` 6 秒 + 寫入已開始時的寬限 1 秒 = 7.5 秒 ≤ 8 秒。
/// Pulse 的讀取閒置為 `max(2m, m + 10 秒)`（m = min(heartbeat, report) 間隔），
/// 伺服器最壞回應延遲 8 秒 < 10 秒，對任何心跳間隔都不會被誤判為斷線。
pub const MAX_WRITE_SECS: u64 = 6;

/// 同時連線數上限的最大值
pub const MAX_MAX_CONNECTIONS: usize = 65535;

/// 狀態表筆數上限的最大值。狀態表已滿時，每次註冊新 agent 都要在全域鎖下以 O(n) 找出
/// 離線最久者，因此上限不宜過大
pub const MAX_MAX_AGENTS: usize = 100_000;

/// 未設定 `server.max_connections_per_ip` 時的預設值（不超過 `server.max_connections`）
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 32;

/// 未設定 `limits.max_agents` 時的預設值（不小於 `server.max_connections`）
pub const DEFAULT_MAX_AGENTS: usize = 10000;

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
    pub limits: LimitSettings,
    pub log: LogSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ServerSettings {
    pub listen: String,
    pub cert_path: String,
    pub key_path: String,
    pub max_connections: usize,
    /// 同一個來源 IP 的同時連線數上限（含握手中、尚未送出 hello 的連線）。
    /// 未設定時為 `min(32, max_connections)`，見 [`AppConfig::max_connections_per_ip`]
    pub max_connections_per_ip: Option<usize>,
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
            max_connections_per_ip: None,
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
            handshake_secs: 5,
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
    /// 是否接受含多個 DNS SAN 的客戶端憑證。預設 `false`：憑證必須恰好有一個 DNS SAN，
    /// 避免一張憑證同時冒用（並踢掉）多個 agent
    pub allow_multiple_dns_sans: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct LimitSettings {
    /// 狀態表（記憶體與 agents.json）最多保留幾個 agent。達上限時先淘汰離線最久的 agent；
    /// 沒有可淘汰的離線 agent 時拒絕新的 agent_id。必須不小於 `server.max_connections`，
    /// 確保線上的 agent 一定放得下。未設定時為 `max(10000, max_connections)`，
    /// 見 [`AppConfig::max_agents`]
    pub max_agents: Option<usize>,
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

/// `KAIROS__<SECTION>__<KEY>` 環境變數來源
fn environment() -> Environment {
    Environment::with_prefix("KAIROS")
        .prefix_separator("__")
        .separator("__")
        .try_parsing(true)
        // KAIROS__AUTH__CRL=a.crl,b.crl
        .list_separator(LIST_SEPARATOR)
        .with_list_parse_key("auth.crl")
}

impl AppConfig {
    /// 實際使用的每 IP 連線上限：明確設定的值，或 `min(32, max_connections)`。
    /// 預設值隨 `max_connections` 調整，讓只設定 `max_connections` 的舊設定檔仍能啟動。
    pub fn max_connections_per_ip(&self) -> usize {
        self.server
            .max_connections_per_ip
            .unwrap_or(DEFAULT_MAX_CONNECTIONS_PER_IP.min(self.server.max_connections))
    }

    /// 實際使用的狀態表上限：明確設定的值，或 `max(10000, max_connections)`
    pub fn max_agents(&self) -> usize {
        self.limits
            .max_agents
            .unwrap_or(DEFAULT_MAX_AGENTS.max(self.server.max_connections))
    }

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
            .add_source(environment())
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
        // 只有明確寫出的值才可能不合法；未設定時的預設值依 max_connections 推導，一定合法
        if let Some(per_ip) = self.server.max_connections_per_ip
            && !(1..=self.server.max_connections).contains(&per_ip)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "server.max_connections_per_ip 必須介於 1..=server.max_connections（{}）（目前為 {}）",
                    self.server.max_connections, per_ip
                ),
            ));
        }
        if let Some(max_agents) = self.limits.max_agents
            && !(self.server.max_connections..=MAX_MAX_AGENTS).contains(&max_agents)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "limits.max_agents 必須介於 server.max_connections（{}）..={}（目前為 {}）",
                    self.server.max_connections, MAX_MAX_AGENTS, max_agents
                ),
            ));
        }
        for (name, value, max) in [
            (
                "timeouts.handshake_secs",
                self.timeouts.handshake_secs,
                MAX_TIMEOUT_SECS,
            ),
            (
                "timeouts.hello_secs",
                self.timeouts.hello_secs,
                MAX_TIMEOUT_SECS,
            ),
            (
                "timeouts.write_secs",
                self.timeouts.write_secs,
                MAX_WRITE_SECS,
            ),
        ] {
            if !(1..=max).contains(&value) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("{} 必須介於 1..={}（目前為 {}）", name, max, value),
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
        assert_eq!(c.server.max_connections_per_ip, None);
        assert_eq!(c.max_connections_per_ip(), 32);
        assert_eq!(c.limits.max_agents, None);
        assert_eq!(c.max_agents(), 10000);
        assert!(!c.auth.allow_multiple_dns_sans);
        assert_eq!(c.timeouts.handshake_secs, 5);
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
        c.timeouts.write_secs = MAX_WRITE_SECS;
        c.server.max_connections = MAX_MAX_CONNECTIONS;
        c.server.max_connections_per_ip = Some(MAX_MAX_CONNECTIONS);
        c.limits.max_agents = Some(MAX_MAX_AGENTS);
        assert!(c.validate().is_ok());

        let mutations: [fn(&mut AppConfig); 9] = [
            |c| c.timeouts.handshake_secs = MAX_TIMEOUT_SECS + 1,
            |c| c.timeouts.hello_secs = u64::MAX,
            // X2：寫出逾時必須小於 Pulse 的 10 秒讀取閒置
            |c| c.timeouts.write_secs = MAX_WRITE_SECS + 1,
            |c| c.timeouts.write_secs = 0,
            |c| c.server.max_connections = MAX_MAX_CONNECTIONS + 1,
            |c| c.server.max_connections_per_ip = Some(0),
            |c| c.server.max_connections_per_ip = Some(c.server.max_connections + 1),
            // 線上的 agent 必須放得下
            |c| c.limits.max_agents = Some(c.server.max_connections - 1),
            |c| c.limits.max_agents = Some(MAX_MAX_AGENTS + 1),
        ];
        for mutate in mutations {
            let mut c = AppConfig::default();
            mutate(&mut c);
            assert!(c.validate().is_err());
        }
    }

    /// 以注入的環境變數（取代行程環境）建立設定
    fn from_env(vars: &[(&str, &str)]) -> Result<AppConfig, config::ConfigError> {
        let map = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::builder()
            .add_source(environment().source(Some(map)))
            .build()
            .and_then(|c| c.try_deserialize())
    }

    #[test]
    fn environment_overrides_and_crl_list() {
        let c = from_env(&[
            ("KAIROS__SERVER__LISTEN", "0.0.0.0:9000"),
            ("KAIROS__SERVER__MAX_CONNECTIONS", "64"),
            ("KAIROS__SERVER__MAX_CONNECTIONS_PER_IP", "4"),
            ("KAIROS__TIMEOUTS__WRITE_SECS", "6"),
            ("KAIROS__LIMITS__MAX_AGENTS", "500"),
            ("KAIROS__AUTH__CLIENT_CA", "ca.pem"),
            ("KAIROS__AUTH__CRL", "a.crl,b.crl"),
            ("KAIROS__AUTH__ALLOW_MULTIPLE_DNS_SANS", "true"),
        ])
        .unwrap();
        assert_eq!(c.server.listen, "0.0.0.0:9000");
        assert_eq!(c.server.max_connections, 64);
        assert_eq!(c.server.max_connections_per_ip, Some(4));
        assert_eq!(c.max_connections_per_ip(), 4);
        assert_eq!(c.timeouts.write_secs, 6);
        assert_eq!(c.limits.max_agents, Some(500));
        assert_eq!(c.max_agents(), 500);
        assert_eq!(c.auth.client_ca_path(), Some("ca.pem"));
        assert_eq!(c.auth.crl_paths(), ["a.crl", "b.crl"]);
        assert!(c.auth.allow_multiple_dns_sans);
        assert!(c.validate().is_ok());

        // 單一 CRL 也解析成清單；未設定的項目維持預設
        let c = from_env(&[("KAIROS__AUTH__CRL", "only.crl")]).unwrap();
        assert_eq!(c.auth.crl, ["only.crl"]);
        assert_eq!(c.server, ServerSettings::default());

        // 型別錯誤回錯誤
        assert!(from_env(&[("KAIROS__SERVER__MAX_CONNECTIONS", "many")]).is_err());
        // 超出上限的值由 validate 擋下
        let c = from_env(&[("KAIROS__TIMEOUTS__WRITE_SECS", "7")]).unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn only_max_connections_set_keeps_old_configs_working() {
        // 升級相容：舊設定檔只設定 max_connections 時，新項目的預設值隨之調整，仍可啟動
        for (max_connections, per_ip, agents) in [
            (1, 1, 10000),
            (31, 31, 10000),
            (32, 32, 10000),
            (1024, 32, 10000),
            (10000, 32, 10000),
            (10001, 32, 10001),
            (65535, 32, 65535),
        ] {
            let toml = format!("[server]\nmax_connections = {max_connections}\n");
            let from_file: AppConfig = Config::builder()
                .add_source(File::from_str(&toml, FileFormat::Toml))
                .build()
                .unwrap()
                .try_deserialize()
                .unwrap();
            let from_env = from_env(&[(
                "KAIROS__SERVER__MAX_CONNECTIONS",
                &max_connections.to_string(),
            )])
            .unwrap();
            for c in [from_file, from_env] {
                assert!(c.validate().is_ok(), "max_connections = {max_connections}");
                assert_eq!(c.max_connections_per_ip(), per_ip, "{max_connections}");
                assert_eq!(c.max_agents(), agents, "{max_connections}");
            }
        }

        // 明確寫出不合法的值才報錯
        let c = from_env(&[
            ("KAIROS__SERVER__MAX_CONNECTIONS", "16"),
            ("KAIROS__SERVER__MAX_CONNECTIONS_PER_IP", "32"),
        ])
        .unwrap();
        assert!(c.validate().is_err());
        let c = from_env(&[
            ("KAIROS__SERVER__MAX_CONNECTIONS", "20000"),
            ("KAIROS__LIMITS__MAX_AGENTS", "10000"),
        ])
        .unwrap();
        assert!(c.validate().is_err());
        // 明確設定且合法時以設定為準
        let c = from_env(&[
            ("KAIROS__SERVER__MAX_CONNECTIONS", "16"),
            ("KAIROS__SERVER__MAX_CONNECTIONS_PER_IP", "16"),
            ("KAIROS__LIMITS__MAX_AGENTS", "16"),
        ])
        .unwrap();
        assert!(c.validate().is_ok());
        assert_eq!((c.max_connections_per_ip(), c.max_agents()), (16, 16));
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
