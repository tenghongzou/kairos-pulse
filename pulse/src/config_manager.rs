use config::{Config, ConfigError};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub address: String,
    pub port: u16,
    /// TLS 用的主機名稱（SNI / 憑證驗證）
    pub domain: String,
    /// 是否略過 TLS 憑證驗證（僅供開發測試，正式環境應為 false）
    pub accept_invalid_certs: bool,
}

#[derive(Debug, Deserialize)]
pub struct RetryConfig {
    pub max_retries: usize,
    pub initial_delay: u64,
}

#[derive(Debug, Deserialize)]
pub struct HeartbeatConfig {
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
pub struct ReportConfig {
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
pub struct LogConfig {
    pub level: String,
}

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub retry: RetryConfig,
    pub heartbeat: HeartbeatConfig,
    pub report: ReportConfig,
    pub log: LogConfig,
}

impl AppConfig {
    /// 讀取編譯時嵌入的 config.toml。
    ///
    /// 回傳 `Result` 讓呼叫端決定如何處理解析失敗，而非直接 panic。
    pub fn from_embedded() -> Result<Self, ConfigError> {
        // 使用 include_str! 讀取嵌入的配置
        let embedded_config = include_str!("../config.toml");
        Config::builder()
            .add_source(config::File::from_str(
                embedded_config,
                config::FileFormat::Toml,
            ))
            .build()?
            .try_deserialize()
    }
}
