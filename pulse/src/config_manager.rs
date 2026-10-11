use config::{Config, ConfigError, Environment, File, FileFormat, Map};
use log::LevelFilter;
use serde::Deserialize;

/// 編譯時嵌入的預設設定。
const EMBEDDED_CONFIG: &str = include_str!("../config.toml");

/// 本機覆蓋設定檔（目前工作目錄，可不存在）。
pub const LOCAL_CONFIG_FILE: &str = "pulse.toml";

/// `heartbeat.interval` 上限（秒），與伺服器將 heartbeat_interval_ms 夾在 60000 一致。
pub const MAX_HEARTBEAT_INTERVAL: u64 = 60;
/// `report.interval` 上限（秒）。
pub const MAX_REPORT_INTERVAL: u64 = 3600;
/// `retry.initial_delay` / `retry.max_delay` 上限（秒）。
pub const MAX_RETRY_DELAY: u64 = 3600;
/// `log.retention_days` 上限（天，約 10 年）。
pub const MAX_LOG_RETENTION_DAYS: u32 = 3650;

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub address: String,
    pub port: u16,
    /// TLS 用的主機名稱（SNI / 憑證驗證）
    pub domain: String,
    /// 是否略過 TLS 憑證驗證（僅供開發測試，正式環境應為 false）
    pub accept_invalid_certs: bool,
    /// 額外信任的 CA / 自簽憑證 PEM 路徑；空字串代表只用系統信任庫
    pub ca_cert: String,
    /// 是否信任作業系統信任庫；`None`（未設定）時依是否設定了 `ca_cert` 決定，
    /// 見 [`ServerConfig::trusts_system_roots`]。明確設為 false 時 `ca_cert` 必填
    #[serde(default)]
    pub use_system_roots: Option<bool>,
    /// 客戶端憑證 PEM 路徑（mTLS，可含中繼 CA 鏈）；空字串代表不出示客戶端憑證
    pub client_cert: String,
    /// 客戶端憑證私鑰 PEM 路徑（PKCS#8 / PKCS#1 / SEC1，不可加密）；須與 `client_cert` 成對設定
    pub client_key: String,
}

impl ServerConfig {
    /// 實際是否信任作業系統信任庫（S5）：
    ///
    /// - 明確設定 `use_system_roots` 時以設定為準；
    /// - 未設定時：設定了 `ca_cert`（私有 CA 部署）→ `false`，只信任 `ca_cert`，
    ///   避免任何公開 CA 簽發的同名憑證也被接受；沒有 `ca_cert` → `true`（公開 CA 簽發的伺服器憑證）。
    pub fn trusts_system_roots(&self) -> bool {
        self.use_system_roots
            .unwrap_or_else(|| self.ca_cert.trim().is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RetryConfig {
    /// 連續失敗達此次數後，重連日誌由 warn 升級為 error（不限制重試次數）
    pub max_retries: u32,
    /// 初始重連延遲（秒）
    pub initial_delay: u64,
    /// 重連延遲上限（秒）
    pub max_delay: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatConfig {
    pub interval: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReportConfig {
    pub interval: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    /// 存放 agent_id 的檔案路徑
    pub id_file: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogConfig {
    /// 日誌級別；載入後已正規化為小寫並驗證過
    pub level: String,
    /// 日誌檔保留天數（今天與前 N 天）；0 = 永久保留，上限 `MAX_LOG_RETENTION_DAYS`
    pub retention_days: u32,
}

impl LogConfig {
    /// 對應的 `LevelFilter`（`level` 已在載入時驗證，這裡不會失敗）。
    pub fn level_filter(&self) -> LevelFilter {
        parse_level(&self.level).unwrap_or(LevelFilter::Info)
    }
}

/// 解析日誌級別（不分大小寫）；未知值回傳 `None`。
fn parse_level(level: &str) -> Option<LevelFilter> {
    match level.trim().to_ascii_lowercase().as_str() {
        "trace" => Some(LevelFilter::Trace),
        "debug" => Some(LevelFilter::Debug),
        "info" => Some(LevelFilter::Info),
        "warn" => Some(LevelFilter::Warn),
        "error" => Some(LevelFilter::Error),
        _ => None,
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub retry: RetryConfig,
    pub heartbeat: HeartbeatConfig,
    pub report: ReportConfig,
    pub agent: AgentConfig,
    pub log: LogConfig,
    /// 載入時目前工作目錄下是否有 `pulse.toml`（啟動時記錄，方便確認設定來源）
    #[serde(skip)]
    pub local_file_found: bool,
}

impl AppConfig {
    /// 依序疊加：嵌入的 config.toml → `pulse.toml`（可不存在）→ `PULSE__*` 環境變數。
    pub fn load() -> Result<Self, ConfigError> {
        let found = std::path::Path::new(LOCAL_CONFIG_FILE).is_file();
        let mut config = Self::build(Some(LOCAL_CONFIG_FILE), None)?;
        config.local_file_found = found;
        Ok(config)
    }

    /// 測試用：只用嵌入的預設值（不讀 pulse.toml 與行程環境變數）。
    #[cfg(test)]
    pub fn default_for_tests() -> Self {
        Self::build(None, Some(Map::new())).expect("嵌入的預設設定必須合法")
    }

    /// 實際的載入邏輯。
    ///
    /// `env_override` 為 `Some` 時改用給定的鍵值取代真正的行程環境變數（供測試使用）。
    ///
    /// 環境變數一律以字串讀入（不開 `try_parsing`）：數值與布林欄位在反序列化時由 config
    /// 把字串轉成對應型別；字串欄位（路徑、domain、address）則保持原樣，
    /// 例如 `PULSE__AGENT__ID_FILE=0001` 不會被改寫成 `"1"`。
    fn build(
        local_file: Option<&str>,
        env_override: Option<Map<String, String>>,
    ) -> Result<Self, ConfigError> {
        let mut builder =
            Config::builder().add_source(File::from_str(EMBEDDED_CONFIG, FileFormat::Toml));
        if let Some(path) = local_file {
            builder = builder.add_source(File::new(path, FileFormat::Toml).required(false));
        }
        let env = Environment::with_prefix("PULSE")
            .prefix_separator("__")
            .separator("__")
            .try_parsing(false)
            .source(env_override);

        let mut config: Self = builder.add_source(env).build()?.try_deserialize()?;
        config.log.level = config.log.level.trim().to_ascii_lowercase();
        config.validate()?;
        Ok(config)
    }

    /// 檢查數值是否在合理範圍，避免執行期才出現 panic 或異常行為。
    fn validate(&self) -> Result<(), ConfigError> {
        fn range(name: &str, value: u64, min: u64, max: u64) -> Result<(), ConfigError> {
            if (min..=max).contains(&value) {
                Ok(())
            } else {
                Err(ConfigError::Message(format!(
                    "{name} 必須介於 {min} 到 {max} 之間（目前為 {value}）"
                )))
            }
        }
        if self.server.address.trim().is_empty() {
            return Err(ConfigError::Message(
                "server.address 不可為空字串（請填伺服器的 IP 或主機名稱）".to_string(),
            ));
        }
        range("server.port", u64::from(self.server.port), 1, 65535)?;
        if self.server.domain.trim().is_empty() {
            return Err(ConfigError::Message(
                "server.domain 不可為空字串（TLS 以此驗證伺服器憑證的名稱）".to_string(),
            ));
        }
        range(
            "heartbeat.interval",
            self.heartbeat.interval,
            1,
            MAX_HEARTBEAT_INTERVAL,
        )?;
        range(
            "report.interval",
            self.report.interval,
            1,
            MAX_REPORT_INTERVAL,
        )?;
        range(
            "retry.initial_delay",
            self.retry.initial_delay,
            1,
            MAX_RETRY_DELAY,
        )?;
        range("retry.max_delay", self.retry.max_delay, 1, MAX_RETRY_DELAY)?;
        if self.retry.max_delay < self.retry.initial_delay {
            return Err(ConfigError::Message(
                "retry.max_delay 不可小於 retry.initial_delay".to_string(),
            ));
        }
        // accept_invalid_certs = true 時不使用信任庫，不需要 ca_cert。
        // 未明確設定 use_system_roots 時，沒有 ca_cert 就一定信任系統信任庫，不會走到這裡
        if !self.server.trusts_system_roots()
            && self.server.ca_cert.trim().is_empty()
            && !self.server.accept_invalid_certs
        {
            return Err(ConfigError::Message(
                "server.use_system_roots = false 時必須設定 server.ca_cert（否則沒有任何可信任的伺服器憑證）"
                    .to_string(),
            ));
        }
        let has_cert = !self.server.client_cert.trim().is_empty();
        let has_key = !self.server.client_key.trim().is_empty();
        if has_cert != has_key {
            let (set, unset) = if has_cert {
                ("server.client_cert", "server.client_key")
            } else {
                ("server.client_key", "server.client_cert")
            };
            return Err(ConfigError::Message(format!(
                "已設定 {set} 但未設定 {unset}：客戶端憑證與私鑰必須同時設定或同時留空"
            )));
        }
        if self.agent.id_file.trim().is_empty() {
            return Err(ConfigError::Message(
                "agent.id_file 不可為空字串".to_string(),
            ));
        }
        range(
            "log.retention_days",
            u64::from(self.log.retention_days),
            0,
            u64::from(MAX_LOG_RETENTION_DAYS),
        )?;
        if parse_level(&self.log.level).is_none() {
            return Err(ConfigError::Message(format!(
                "log.level「{}」不是有效的日誌級別（可用：trace / debug / info / warn / error，不分大小寫）",
                self.log.level
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Map<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn build_err(pairs: &[(&str, &str)]) -> String {
        AppConfig::build(None, Some(env(pairs)))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn embedded_defaults() {
        let cfg = AppConfig::build(None, Some(Map::new())).unwrap();
        assert_eq!(cfg.server.address, "127.0.0.1");
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.server.domain, "localhost");
        assert!(!cfg.server.accept_invalid_certs);
        assert_eq!(cfg.server.ca_cert, "");
        // S5：預設不寫死，依是否設定 ca_cert 決定
        assert_eq!(cfg.server.use_system_roots, None);
        assert!(cfg.server.trusts_system_roots());
        assert_eq!(cfg.retry.max_retries, 5);
        assert_eq!(cfg.retry.initial_delay, 2);
        assert_eq!(cfg.retry.max_delay, 60);
        assert_eq!(cfg.heartbeat.interval, 1);
        assert_eq!(cfg.report.interval, 5);
        assert_eq!(cfg.agent.id_file, "agent.id");
        assert_eq!(cfg.log.level, "info");
        assert_eq!(cfg.log.level_filter(), LevelFilter::Info);
    }

    #[test]
    fn env_overrides_embedded() {
        let cfg = AppConfig::build(
            None,
            Some(env(&[
                ("PULSE__SERVER__PORT", "28080"),
                ("PULSE__SERVER__CA_CERT", "C:\\certs\\cert.pem"),
                ("PULSE__SERVER__ACCEPT_INVALID_CERTS", "true"),
                ("PULSE__RETRY__MAX_DELAY", "30"),
                ("PULSE__AGENT__ID_FILE", "data/agent.id"),
                // 非 PULSE 前綴的變數應被忽略
                ("OTHER__SERVER__PORT", "1"),
            ])),
        )
        .unwrap();
        assert_eq!(cfg.server.port, 28080);
        assert_eq!(cfg.server.ca_cert, "C:\\certs\\cert.pem");
        assert!(cfg.server.accept_invalid_certs);
        assert_eq!(cfg.retry.max_delay, 30);
        assert_eq!(cfg.agent.id_file, "data/agent.id");
        // 未覆蓋的欄位維持嵌入值
        assert_eq!(cfg.server.address, "127.0.0.1");
    }

    #[test]
    fn invalid_values_rejected() {
        assert!(build_err(&[("PULSE__HEARTBEAT__INTERVAL", "0")]).contains("heartbeat.interval"));
        assert!(build_err(&[("PULSE__RETRY__MAX_DELAY", "1")]).contains("max_delay"));
    }

    #[test]
    fn port_zero_and_blank_address_or_domain_rejected() {
        // R2-P13
        let err = build_err(&[("PULSE__SERVER__PORT", "0")]);
        assert!(err.contains("server.port"), "{err}");
        for blank in ["", "   "] {
            let err = build_err(&[("PULSE__SERVER__ADDRESS", blank)]);
            assert!(err.contains("server.address"), "{blank:?}: {err}");
            let err = build_err(&[("PULSE__SERVER__DOMAIN", blank)]);
            assert!(err.contains("server.domain"), "{blank:?}: {err}");
        }
        // 超出 u16 → 反序列化錯誤
        assert!(AppConfig::build(None, Some(env(&[("PULSE__SERVER__PORT", "65536")]))).is_err());
        let cfg = AppConfig::build(None, Some(env(&[("PULSE__SERVER__PORT", "65535")]))).unwrap();
        assert_eq!(cfg.server.port, 65535);
    }

    #[test]
    fn string_settings_from_env_are_not_reinterpreted() {
        // R2-P13：看似數字或布林的字串保持原樣
        let cfg = AppConfig::build(
            None,
            Some(env(&[
                ("PULSE__AGENT__ID_FILE", "0001"),
                ("PULSE__SERVER__CA_CERT", "00.10"),
                ("PULSE__SERVER__CLIENT_CERT", "1e3"),
                ("PULSE__SERVER__CLIENT_KEY", "007"),
                ("PULSE__SERVER__DOMAIN", "true"),
                ("PULSE__SERVER__ADDRESS", "0010"),
            ])),
        )
        .unwrap();
        assert_eq!(cfg.agent.id_file, "0001");
        assert_eq!(cfg.server.ca_cert, "00.10");
        assert_eq!(cfg.server.client_cert, "1e3");
        assert_eq!(cfg.server.client_key, "007");
        assert_eq!(cfg.server.domain, "true");
        assert_eq!(cfg.server.address, "0010");
        // 數值與布林仍可從環境變數的字串解析
        let cfg = AppConfig::build(
            None,
            Some(env(&[
                ("PULSE__SERVER__PORT", "09000"),
                ("PULSE__SERVER__USE_SYSTEM_ROOTS", "false"),
                ("PULSE__SERVER__CA_CERT", "ca.pem"),
                ("PULSE__REPORT__INTERVAL", "30"),
                ("PULSE__LOG__RETENTION_DAYS", "7"),
            ])),
        )
        .unwrap();
        assert_eq!(cfg.server.port, 9000);
        assert_eq!(cfg.server.use_system_roots, Some(false));
        assert_eq!(cfg.report.interval, 30);
        assert_eq!(cfg.log.retention_days, 7);
        // 非數字的數值欄位仍是錯誤
        assert!(AppConfig::build(None, Some(env(&[("PULSE__SERVER__PORT", "abc")]))).is_err());
        assert!(
            AppConfig::build(
                None,
                Some(env(&[("PULSE__SERVER__ACCEPT_INVALID_CERTS", "maybe")]))
            )
            .is_err()
        );
    }

    #[test]
    fn upper_bounds_enforced() {
        // 邊界值可接受
        let cfg = AppConfig::build(
            None,
            Some(env(&[
                ("PULSE__HEARTBEAT__INTERVAL", "60"),
                ("PULSE__REPORT__INTERVAL", "3600"),
                ("PULSE__RETRY__INITIAL_DELAY", "3600"),
                ("PULSE__RETRY__MAX_DELAY", "3600"),
            ])),
        )
        .unwrap();
        assert_eq!(cfg.heartbeat.interval, 60);

        assert!(build_err(&[("PULSE__HEARTBEAT__INTERVAL", "61")]).contains("heartbeat.interval"));
        assert!(build_err(&[("PULSE__REPORT__INTERVAL", "3601")]).contains("report.interval"));
        assert!(
            build_err(&[
                ("PULSE__RETRY__INITIAL_DELAY", "18446744073709551615"),
                ("PULSE__RETRY__MAX_DELAY", "18446744073709551615"),
            ])
            .contains("retry.initial_delay")
        );
        assert!(build_err(&[("PULSE__RETRY__MAX_DELAY", "3601")]).contains("retry.max_delay"));
    }

    #[test]
    fn log_level_case_insensitive_and_validated() {
        let cfg = AppConfig::build(None, Some(env(&[("PULSE__LOG__LEVEL", " DEBUG ")]))).unwrap();
        assert_eq!(cfg.log.level, "debug");
        assert_eq!(cfg.log.level_filter(), LevelFilter::Debug);

        let cfg = AppConfig::build(None, Some(env(&[("PULSE__LOG__LEVEL", "Warn")]))).unwrap();
        assert_eq!(cfg.log.level_filter(), LevelFilter::Warn);

        assert!(build_err(&[("PULSE__LOG__LEVEL", "verbose")]).contains("log.level"));
    }

    #[test]
    fn log_retention_days_bounds() {
        let cfg = AppConfig::build(None, Some(Map::new())).unwrap();
        assert_eq!(cfg.log.retention_days, 14);
        for ok in ["0", "3650"] {
            let cfg =
                AppConfig::build(None, Some(env(&[("PULSE__LOG__RETENTION_DAYS", ok)]))).unwrap();
            assert_eq!(cfg.log.retention_days.to_string(), ok);
        }
        assert!(
            build_err(&[("PULSE__LOG__RETENTION_DAYS", "3651")]).contains("log.retention_days")
        );
    }

    #[test]
    fn client_cert_and_key_must_be_paired() {
        // 預設皆為空
        let cfg = AppConfig::build(None, Some(Map::new())).unwrap();
        assert_eq!(cfg.server.client_cert, "");
        assert_eq!(cfg.server.client_key, "");

        // 同時設定可接受
        let cfg = AppConfig::build(
            None,
            Some(env(&[
                ("PULSE__SERVER__CLIENT_CERT", "client.pem"),
                ("PULSE__SERVER__CLIENT_KEY", "client.key"),
            ])),
        )
        .unwrap();
        assert_eq!(cfg.server.client_cert, "client.pem");
        assert_eq!(cfg.server.client_key, "client.key");

        // 只設定其中一個 → 錯誤，訊息指出缺哪一個
        let err = build_err(&[("PULSE__SERVER__CLIENT_CERT", "client.pem")]);
        assert!(err.contains("未設定 server.client_key"), "{err}");
        let err = build_err(&[("PULSE__SERVER__CLIENT_KEY", "client.key")]);
        assert!(err.contains("未設定 server.client_cert"), "{err}");
        // 只有空白視同未設定
        let err = build_err(&[
            ("PULSE__SERVER__CLIENT_CERT", "client.pem"),
            ("PULSE__SERVER__CLIENT_KEY", "  "),
        ]);
        assert!(err.contains("未設定 server.client_key"), "{err}");
    }

    #[test]
    fn use_system_roots_false_requires_ca_cert() {
        let err = build_err(&[("PULSE__SERVER__USE_SYSTEM_ROOTS", "false")]);
        assert!(
            err.contains("server.use_system_roots") && err.contains("server.ca_cert"),
            "{err}"
        );
        // 只有空白視同未設定
        let err = build_err(&[
            ("PULSE__SERVER__USE_SYSTEM_ROOTS", "false"),
            ("PULSE__SERVER__CA_CERT", "  "),
        ]);
        assert!(err.contains("server.ca_cert"), "{err}");

        let cfg = AppConfig::build(
            None,
            Some(env(&[
                ("PULSE__SERVER__USE_SYSTEM_ROOTS", "false"),
                ("PULSE__SERVER__CA_CERT", "ca.pem"),
            ])),
        )
        .unwrap();
        assert!(!cfg.server.trusts_system_roots());
        assert_eq!(cfg.server.ca_cert, "ca.pem");

        // accept_invalid_certs = true 時不檢查信任庫，可不設 ca_cert
        let cfg = AppConfig::build(
            None,
            Some(env(&[
                ("PULSE__SERVER__USE_SYSTEM_ROOTS", "false"),
                ("PULSE__SERVER__ACCEPT_INVALID_CERTS", "true"),
            ])),
        )
        .unwrap();
        assert!(!cfg.server.trusts_system_roots() && cfg.server.accept_invalid_certs);
    }

    #[test]
    fn system_roots_default_depends_on_ca_cert() {
        // S5：未明確設定 use_system_roots 時，有 ca_cert 就只信任 ca_cert
        let cfg =
            AppConfig::build(None, Some(env(&[("PULSE__SERVER__CA_CERT", "ca.pem")]))).unwrap();
        assert_eq!(cfg.server.use_system_roots, None);
        assert!(!cfg.server.trusts_system_roots());
        // 只有空白視同未設定 ca_cert → 信任系統信任庫
        let cfg = AppConfig::build(None, Some(env(&[("PULSE__SERVER__CA_CERT", "  ")]))).unwrap();
        assert!(cfg.server.trusts_system_roots());
        // 明確設定時以設定為準（含環境變數的字串）
        for (value, expected) in [("true", true), ("false", false)] {
            let cfg = AppConfig::build(
                None,
                Some(env(&[
                    ("PULSE__SERVER__CA_CERT", "ca.pem"),
                    ("PULSE__SERVER__USE_SYSTEM_ROOTS", value),
                ])),
            )
            .unwrap();
            assert_eq!(cfg.server.use_system_roots, Some(expected));
            assert_eq!(cfg.server.trusts_system_roots(), expected, "{value}");
        }
        // 沒有 ca_cert 時明確設定 true 也可以；不是布林值則報錯
        let cfg = AppConfig::build(
            None,
            Some(env(&[("PULSE__SERVER__USE_SYSTEM_ROOTS", "true")])),
        )
        .unwrap();
        assert!(cfg.server.trusts_system_roots());
        assert!(
            AppConfig::build(
                None,
                Some(env(&[("PULSE__SERVER__USE_SYSTEM_ROOTS", "maybe")]))
            )
            .is_err()
        );
    }

    #[test]
    fn system_roots_from_pulse_toml() {
        // pulse.toml 只寫 ca_cert（INSTALL 的範例去掉 use_system_roots）→ 只信任 ca_cert；
        // 寫明 use_system_roots = true → 同時信任系統信任庫
        let dir = std::env::temp_dir()
            .join("pulse-config-tests")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pulse.toml");
        let load = |content: &str| {
            std::fs::write(&file, content).unwrap();
            AppConfig::build(Some(file.to_str().unwrap()), Some(Map::new())).unwrap()
        };
        assert!(
            !load("[server]\nca_cert = \"ca.pem\"\n")
                .server
                .trusts_system_roots()
        );
        assert!(
            load("[server]\nca_cert = \"ca.pem\"\nuse_system_roots = true\n")
                .server
                .trusts_system_roots()
        );
        assert!(load("[server]\nport = 9000\n").server.trusts_system_roots());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
