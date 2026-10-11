//! 設定載入：程式內預設 → `kairos.toml`（或 `--config <path>`）→ `KAIROS__<SECTION>__<KEY>` 環境變數。

use config::{Config, Environment, File, FileFormat};
use log::LevelFilter;
use serde::{Deserialize, Deserializer};
use std::ffi::{OsStr, OsString};
use std::io::{Error, ErrorKind};
use std::net::{IpAddr, SocketAddr};
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
/// 訊息速率延後 0.05 秒 + report 限速延後 0.5 秒 + `write_secs` 6 秒 + 寫入已開始時的寬限 1 秒
/// = 7.55 秒 ≤ 8 秒。Pulse 的讀取閒置為 `max(2m, m + 10 秒)`（m 為 Pulse 的心跳間隔），
/// 伺服器最壞回應延遲 8 秒小於其中的 10 秒餘裕，對任何心跳間隔都不會被誤判為斷線。
pub const MAX_WRITE_SECS: u64 = 6;

/// `storage.max_daily_bytes` 的預設值：0（不限制）。合適的上限取決於部署規模，由管理者自行設定
pub const DEFAULT_MAX_DAILY_BYTES: u64 = 0;

/// 估算每日 report 量時，每行 JSONL 的大小（bytes）
pub const ESTIMATED_REPORT_LINE_BYTES: u64 = 250;

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
-V, --version    顯示版本與建置的 commit\n  \
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
    /// 每個 reports-YYYY-MM-DD.jsonl 的大小上限（bytes），0 = 不限制。達到上限後當天的 report
    /// 不再寫入，回 `storage_unavailable`（確定未保存）
    pub max_daily_bytes: u64,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            data_dir: "data".to_string(),
            retention_days: 30,
            max_daily_bytes: DEFAULT_MAX_DAILY_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct AuthSettings {
    /// 非空：要求客戶端憑證，以此 PEM（可含多張 CA）驗證，並強制 agent_id 與憑證綁定
    pub client_ca: String,
    /// CRL 檔路徑清單（PEM 或 DER）；只在啟用 mTLS 時使用。
    /// 也接受以 [`LIST_SEPARATOR`] 分隔的單一字串（環境變數 `KAIROS__AUTH__CRL=a.crl,b.crl`）
    #[serde(deserialize_with = "string_or_list")]
    pub crl: Vec<String>,
    /// 是否接受含多個 DNS SAN 的客戶端憑證。預設 `false`：憑證必須恰好有一個 DNS SAN，
    /// 避免一張憑證同時冒用（並踢掉）多個 agent
    pub allow_multiple_dns_sans: bool,
    /// 未啟用 mTLS（`client_ca` 為空）時，是否仍允許監聽非 loopback 位址（審計 S1）。
    /// 預設 `false`：監聽非 loopback 位址又沒有 mTLS 時拒絕啟動，見 [`AppConfig::check_listen_auth`]。
    /// 啟用 mTLS 時不生效
    pub allow_unauthenticated: bool,
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

/// 是否為 loopback 位址：IPv4 `127.0.0.0/8`、IPv6 `::1`，以及 IPv4-mapped 的 `::ffff:127.0.0.0/104`。
/// `0.0.0.0`、`::`（所有介面）與其他位址都不是
pub fn is_loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// 監聽位址與客戶端驗證的組合（[`AppConfig::check_listen_auth`] 的結果，決定啟動時記錄的訊息）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenAuth {
    /// 已啟用 mTLS
    Mutual,
    /// 未啟用 mTLS，但只監聽 loopback 位址（本機開發）
    LoopbackOnly,
    /// 未啟用 mTLS 且監聽非 loopback 位址，由 `auth.allow_unauthenticated = true` 明確允許
    UnauthenticatedAllowed { exposed: Vec<SocketAddr> },
}

/// 命令列解析結果
#[derive(Debug, PartialEq, Eq)]
pub enum CliAction {
    Run { config_path: Option<PathBuf> },
    Help,
    Version,
}

/// `--config=<path>` 的前綴
const CONFIG_EQ: &str = "--config=";

/// 取出 `--config=<path>` 中的路徑；不是這個形式時回傳 `None`。
/// 路徑可以不是 UTF-8：Unix 上以位元組切開；其他平台無法安全地切開非 UTF-8 的參數，回錯誤請改用
/// `--config <path>`
fn strip_config_eq(arg: &OsStr) -> Option<Result<OsString, String>> {
    if let Some(s) = arg.to_str() {
        return s.strip_prefix(CONFIG_EQ).map(|p| Ok(OsString::from(p)));
    }
    if !arg.as_encoded_bytes().starts_with(CONFIG_EQ.as_bytes()) {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(Ok(
            OsStr::from_bytes(&arg.as_bytes()[CONFIG_EQ.len()..]).to_os_string()
        ))
    }
    #[cfg(not(unix))]
    {
        Some(Err(format!(
            "--config= 後的路徑含有無法辨識的字元（不是有效的 Unicode）: {}；請改用 --config <path>",
            arg.to_string_lossy()
        )))
    }
}

/// 解析命令列參數（不含程式名稱）。只支援 `--config <path>`、`--config=<path>`、`-V/--version`
/// 與 `-h/--help`。
///
/// 參數以 `OsString` 接收（`std::env::args_os()`；審計 Q5）：`std::env::args()` 遇到非 UTF-8 的參數
/// 會 panic。設定檔路徑可以不是 UTF-8；其他非 UTF-8 的參數回錯誤（訊息中以 U+FFFD 取代無法辨識的部分）。
pub fn parse_args<I: IntoIterator<Item = OsString>>(args: I) -> Result<CliAction, String> {
    let mut config_path = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let path = if arg == "--config" {
            match iter.next() {
                Some(p) if !p.is_empty() => p,
                _ => return Err("--config 需要指定設定檔路徑".to_string()),
            }
        } else if let Some(p) = strip_config_eq(&arg) {
            let p = p?;
            if p.is_empty() {
                return Err("--config 需要指定設定檔路徑".to_string());
            }
            p
        } else if arg == "-h" || arg == "--help" {
            return Ok(CliAction::Help);
        } else if arg == "-V" || arg == "--version" {
            return Ok(CliAction::Version);
        } else if arg.to_str().is_none() {
            return Err(format!(
                "無法辨識的參數（含有不是有效 Unicode 的字元）: {}",
                arg.to_string_lossy()
            ));
        } else {
            return Err(format!("無法辨識的參數: {}", arg.to_string_lossy()));
        };
        if config_path.replace(PathBuf::from(path)).is_some() {
            return Err("--config 只能指定一次".to_string());
        }
    }
    Ok(CliAction::Run { config_path })
}

/// 字串清單：接受陣列，或以 [`LIST_SEPARATOR`] 分隔的單一字串
fn string_or_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => s.split(LIST_SEPARATOR).map(str::to_string).collect(),
        OneOrMany::Many(v) => v,
    })
}

/// `KAIROS__<SECTION>__<KEY>` 環境變數來源。
///
/// 不使用 `try_parsing`（R2-K24）：它會把看似數字或布林的值先轉型，字串設定（路徑、位址）會被改寫
/// （例如 `DATA_DIR=0001` 變成 `"1"`）。所有值都以字串保留，數值與布林欄位在反序列化時才由
/// config 解析；`auth.crl` 的逗號分隔由 [`string_or_list`] 處理。
fn environment() -> Environment {
    Environment::with_prefix("KAIROS")
        .prefix_separator("__")
        .separator("__")
}

impl AppConfig {
    /// 實際使用的每 IP 連線上限：明確設定的值，或 `min(32, max_connections)`。
    /// 預設值隨 `max_connections` 調整，讓只設定 `max_connections` 的舊設定檔仍能啟動。
    pub fn max_connections_per_ip(&self) -> usize {
        self.server
            .max_connections_per_ip
            .unwrap_or(DEFAULT_MAX_CONNECTIONS_PER_IP.min(self.server.max_connections))
    }

    /// 以 `max_connections` 條連線、每條每秒 1 則 report、每行約 250 bytes 估算的每日 report 量
    pub fn estimated_daily_report_bytes(&self) -> u64 {
        (self.server.max_connections as u64)
            .saturating_mul(24 * 60 * 60)
            .saturating_mul(ESTIMATED_REPORT_LINE_BYTES)
    }

    /// `storage.max_daily_bytes` 已設定且小於估算的每日量時，回傳 (上限, 估算量)，供啟動時警告
    pub fn daily_limit_below_estimate(&self) -> Option<(u64, u64)> {
        let max = self.storage.max_daily_bytes;
        let estimate = self.estimated_daily_report_bytes();
        (max > 0 && estimate > max).then_some((max, estimate))
    }

    /// 實際使用的狀態表上限：明確設定的值，或 `max(10000, max_connections)`
    pub fn max_agents(&self) -> usize {
        self.limits
            .max_agents
            .unwrap_or(DEFAULT_MAX_AGENTS.max(self.server.max_connections))
    }

    /// 預設拒絕未認證的對外監聽（審計 S1）：`resolved` 為 `server.listen` 解析出的位址。
    /// 沒有設定 `auth.client_ca`（mTLS）又有任何一個位址不是 loopback 時，除非明確設定
    /// `auth.allow_unauthenticated = true`，否則回錯誤（訊息說明原因與修正方式），拒絕啟動。
    pub fn check_listen_auth(&self, resolved: &[SocketAddr]) -> Result<ListenAuth, Error> {
        if self.auth.client_ca_path().is_some() {
            return Ok(ListenAuth::Mutual);
        }
        let exposed: Vec<SocketAddr> = resolved
            .iter()
            .copied()
            .filter(|a| !is_loopback(a.ip()))
            .collect();
        if exposed.is_empty() {
            return Ok(ListenAuth::LoopbackOnly);
        }
        if self.auth.allow_unauthenticated {
            return Ok(ListenAuth::UnauthenticatedAllowed { exposed });
        }
        let exposed_list = exposed
            .iter()
            .map(SocketAddr::to_string)
            .collect::<Vec<_>>()
            .join("、");
        // server.listen 是主機名稱時，附上解析出的非 loopback 位址
        let resolved_note = if exposed_list == self.server.listen {
            String::new()
        } else {
            format!("（解析為 {}）", exposed_list)
        };
        Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "未啟用客戶端憑證驗證（mTLS）卻監聽非 loopback 位址，拒絕啟動\n\
                 \x20 原因：server.listen = \"{}\"{} 不是 loopback 位址，但沒有設定 auth.client_ca。\
                 這樣任何能連到這個位址的人都能以任意 agent_id 註冊、寫入 report，或用已知的 agent_id 把合法的 agent 踢下線。\n\
                 \x20 處理方式：\n\
                 \x20   1. 建議：設定 auth.client_ca（與 auth.crl）啟用 mTLS，步驟見 README「啟用 mTLS」與 INSTALL.md 第 6、7 節。\n\
                 \x20   2. 只在本機測試：把 server.listen 改成 loopback 位址，例如 127.0.0.1:8080 或 [::1]:8080。\n\
                 \x20   3. 只在隔離的測試環境：設定 auth.allow_unauthenticated = true（或環境變數 KAIROS__AUTH__ALLOW_UNAUTHENTICATED=true），明確允許未認證的連線。",
                self.server.listen, resolved_note
            ),
        ))
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

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
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
        assert!(!c.auth.allow_unauthenticated);
        assert_eq!(c.timeouts.handshake_secs, 5);
        assert_eq!(c.timeouts.hello_secs, 5);
        assert_eq!(c.timeouts.write_secs, 5);
        assert_eq!(c.storage.data_dir, "data");
        assert_eq!(c.storage.retention_days, 30);
        assert_eq!(c.storage.max_daily_bytes, 0);
        assert_eq!(c.daily_limit_below_estimate(), None);
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
            // X2：伺服器最壞回應延遲（含 write_secs）必須小於 Pulse 讀取閒置中的 10 秒餘裕
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
            ("KAIROS__AUTH__ALLOW_UNAUTHENTICATED", "true"),
        ])
        .unwrap();
        assert!(c.auth.allow_unauthenticated);
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
    fn daily_limit_warning_uses_the_estimated_volume() {
        // 1024 條連線 × 86400 秒 × 250 bytes ≈ 22 GB
        let mut c = AppConfig::default();
        assert_eq!(c.estimated_daily_report_bytes(), 1024 * 86400 * 250);
        c.storage.max_daily_bytes = 10 * 1024 * 1024 * 1024;
        assert_eq!(
            c.daily_limit_below_estimate(),
            Some((10 * 1024 * 1024 * 1024, 1024 * 86400 * 250))
        );
        c.storage.max_daily_bytes = 1024 * 86400 * 250;
        assert_eq!(c.daily_limit_below_estimate(), None);
        c.storage.max_daily_bytes = 0;
        assert_eq!(c.daily_limit_below_estimate(), None);
    }

    #[test]
    fn environment_strings_are_not_reinterpreted() {
        // R2-K24：看似數字或布林的字串設定保持原樣，數值與布林欄位仍可由環境變數解析
        let c = from_env(&[
            ("KAIROS__STORAGE__DATA_DIR", "0001"),
            ("KAIROS__SERVER__CERT_PATH", "true"),
            ("KAIROS__SERVER__KEY_PATH", "1e3"),
            ("KAIROS__SERVER__LISTEN", "[::1]:08080"),
            ("KAIROS__AUTH__CLIENT_CA", "007"),
            ("KAIROS__AUTH__CRL", "0001,2.50"),
            ("KAIROS__LOG__LEVEL", "WARN"),
            ("KAIROS__STORAGE__MAX_DAILY_BYTES", "1048576"),
            ("KAIROS__STORAGE__RETENTION_DAYS", "07"),
            ("KAIROS__AUTH__ALLOW_MULTIPLE_DNS_SANS", "TRUE"),
        ])
        .unwrap();
        assert_eq!(c.storage.data_dir, "0001");
        assert_eq!(c.server.cert_path, "true");
        assert_eq!(c.server.key_path, "1e3");
        assert_eq!(c.server.listen, "[::1]:08080");
        assert_eq!(c.auth.client_ca, "007");
        assert_eq!(c.auth.crl, ["0001", "2.50"]);
        assert_eq!(c.log.level, "WARN");
        assert_eq!(c.storage.max_daily_bytes, 1048576);
        assert_eq!(c.storage.retention_days, 7);
        assert!(c.auth.allow_multiple_dns_sans);
        // 數值欄位不是數字時仍是錯誤
        assert!(from_env(&[("KAIROS__STORAGE__MAX_DAILY_BYTES", "lots")]).is_err());
        // 設定檔中的 crl 仍可為陣列或字串
        let c: AppConfig = Config::builder()
            .add_source(File::from_str(
                "[auth]\nclient_ca = \"ca\"\ncrl = \"a.crl,b.crl\"\n",
                FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();
        assert_eq!(c.auth.crl, ["a.crl", "b.crl"]);
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

    fn addrs(list: &[&str]) -> Vec<SocketAddr> {
        list.iter().map(|a| a.parse().unwrap()).collect()
    }

    #[test]
    fn loopback_addresses() {
        // S1：127.0.0.0/8 與 ::1（含 IPv4-mapped）是 loopback；0.0.0.0、:: 與其他位址不是
        for ip in [
            "127.0.0.1",
            "127.0.0.2",
            "127.255.255.254",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_loopback(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "0.0.0.0",
            "::",
            "10.0.0.5",
            "192.168.1.1",
            "128.0.0.1",
            "fe80::1",
            "fd00::1",
            "::ffff:10.0.0.1",
            "::ffff:0.0.0.0",
        ] {
            assert!(!is_loopback(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn unauthenticated_exposure_is_refused_by_default() {
        // S1：未啟用 mTLS 又監聽非 loopback 位址時拒絕啟動
        let mut c = AppConfig::default();
        assert_eq!(
            c.check_listen_auth(&addrs(&["127.0.0.1:8080"])).unwrap(),
            ListenAuth::LoopbackOnly
        );
        // localhost 解析出的 IPv4 與 IPv6 都是 loopback
        assert_eq!(
            c.check_listen_auth(&addrs(&["127.0.0.1:8080", "[::1]:8080"]))
                .unwrap(),
            ListenAuth::LoopbackOnly
        );
        for exposed in [
            &["0.0.0.0:8080"][..],
            &["[::]:8080"],
            &["10.0.0.5:8080"],
            // 解析出的位址只要有一個不是 loopback 就拒絕
            &["127.0.0.1:8080", "10.0.0.5:8080"],
        ] {
            c.server.listen = exposed[0].to_string();
            let err = c.check_listen_auth(&addrs(exposed)).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::PermissionDenied);
            let msg = err.to_string();
            // 訊息要說清楚怎麼修
            for hint in [
                "auth.client_ca",
                "127.0.0.1:8080",
                "auth.allow_unauthenticated = true",
                "KAIROS__AUTH__ALLOW_UNAUTHENTICATED=true",
            ] {
                assert!(msg.contains(hint), "{hint}: {msg}");
            }
        }

        // 明確允許時照常啟動（記 WARN）
        c.auth.allow_unauthenticated = true;
        assert_eq!(
            c.check_listen_auth(&addrs(&["127.0.0.1:8080", "0.0.0.0:8080"]))
                .unwrap(),
            ListenAuth::UnauthenticatedAllowed {
                exposed: addrs(&["0.0.0.0:8080"])
            }
        );

        // 啟用 mTLS 時不論監聽位址都允許
        let mut c = AppConfig::default();
        c.auth.client_ca = "ca.pem".into();
        assert_eq!(
            c.check_listen_auth(&addrs(&["0.0.0.0:8080"])).unwrap(),
            ListenAuth::Mutual
        );
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
        assert_eq!(parse_args(args(&["--version"])), Ok(CliAction::Version));
        assert_eq!(parse_args(args(&["-V"])), Ok(CliAction::Version));
        assert_eq!(
            parse_args(args(&["--config", "a.toml", "--version"])),
            Ok(CliAction::Version)
        );
        assert!(parse_args(args(&["-v"])).is_err());
        assert!(USAGE.contains("--version"));
        assert!(parse_args(args(&["--config"])).is_err());
        assert!(parse_args(args(&["--config="])).is_err());
        assert!(parse_args(args(&["--config", "a", "--config", "b"])).is_err());
        assert!(parse_args(args(&["--port", "1"])).is_err());
    }

    /// 平台上「不是有效 Unicode」的參數：Unix 為非 UTF-8 的位元組，Windows 為未配對的 surrogate
    fn non_unicode(prefix: &str) -> OsString {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let mut bytes = prefix.as_bytes().to_vec();
            bytes.extend_from_slice(b"caf\xe9.toml");
            OsString::from_vec(bytes)
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let mut wide: Vec<u16> = prefix.encode_utf16().collect();
            wide.extend([0x63, 0xD800, 0x2E, 0x74]);
            OsString::from_wide(&wide)
        }
    }

    #[test]
    fn parse_args_handles_non_unicode_without_panic() {
        // Q5：非 UTF-8 的參數不 panic，給出錯誤訊息
        let err = parse_args([non_unicode("--bogus")]).unwrap_err();
        assert!(err.contains("無法辨識的參數"), "{err}");
        assert!(err.contains('\u{FFFD}'), "{err}");
        let err = parse_args([non_unicode("")]).unwrap_err();
        assert!(err.contains("不是有效 Unicode"), "{err}");

        // 設定檔路徑本身可以不是 UTF-8
        let path = non_unicode("");
        assert_eq!(
            parse_args([OsString::from("--config"), path.clone()]),
            Ok(CliAction::Run {
                config_path: Some(PathBuf::from(path.clone()))
            })
        );
        // --config=<非 UTF-8>：Unix 可解析；其他平台回錯誤並提示改用 --config <path>
        let result = parse_args([non_unicode("--config=")]);
        #[cfg(unix)]
        assert_eq!(
            result,
            Ok(CliAction::Run {
                config_path: Some(PathBuf::from(path))
            })
        );
        #[cfg(not(unix))]
        assert!(result.unwrap_err().contains("--config <path>"));
    }
}
