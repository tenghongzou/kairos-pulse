use std::path::Path;
use config::{Config, ConfigError, File};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub address: String,
    pub port: u16,
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
pub struct LogConfig {
    pub level: String,
}

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub retry: RetryConfig,
    pub heartbeat: HeartbeatConfig,
    pub log: LogConfig,
}

impl AppConfig {
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        let path = Path::new(path);

        let builder = Config::builder()
            .add_source(File::from(path).required(true));

        let settings = builder.build()?;

        settings.try_deserialize()
    }
}
