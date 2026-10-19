use config::{Config, ConfigError, Environment, File, FileFormat, Map};
use log::LevelFilter;
use serde::Deserialize;
use tokio_rustls::rustls::pki_types::ServerName;

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
/// `buffer.max_mb` 的範圍（MiB）
pub const MIN_BUFFER_MB: u64 = 1;
pub const MAX_BUFFER_MB: u64 = 102_400;
/// `buffer.max_records` 的範圍
pub const MIN_BUFFER_RECORDS: u64 = 100;
pub const MAX_BUFFER_RECORDS: u64 = 10_000_000;
/// `buffer.max_age_hours` 的範圍（1 小時～1 年）
pub const MAX_BUFFER_AGE_HOURS: u64 = 8760;
/// `buffer.replay_interval_ms` 的範圍：下限 500 ms（= Kairos 的 report 速率限制每秒 2 則），上限 60 秒
pub const MIN_REPLAY_INTERVAL_MS: u64 = 500;
pub const MAX_REPLAY_INTERVAL_MS: u64 = 60_000;
/// 實際的補送間隔不超過 `report.interval` 的這個比例（千分比）：補送與新的取樣共用同一個速率，
/// 間隔不小於取樣間隔時，送出的速度追不上新的取樣、積壓永遠補不完。60% 時送出速率至少是取樣速率的
/// 1.67 倍（`report.interval = 1` 時為 600 ms）
pub const MAX_REPLAY_SHARE_PERMILLE: u64 = 600;
/// `collect.max_filesystems`／`max_disks`／`max_interfaces` 的上限（METRICS.md 第 4.3 節：
/// 合計最多 243 個指標，不超過每筆 report 256 個）
pub const MAX_COLLECT_FILESYSTEMS: u64 = 16;
pub const MAX_COLLECT_DISKS: u64 = 8;
pub const MAX_COLLECT_INTERFACES: u64 = 16;
/// `collect.interface_exclude` 的項目數上限
pub const MAX_INTERFACE_EXCLUDE: usize = 32;
/// `processes.interval` 的上限（秒）
pub const MAX_PROCESSES_INTERVAL: u64 = 3600;
/// `inventory.interval` 的範圍（秒）
pub const MIN_INVENTORY_INTERVAL: u64 = 300;
pub const MAX_INVENTORY_INTERVAL: u64 = 86_400;
/// `processes.command` 可用的值
pub const PROCESS_COMMAND_MODES: [&str; 3] = ["none", "executable", "full"];

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ServerConfig {
    pub address: String,
    pub port: u16,
    /// 主位址連不上時依序改試的其他位址（`主機` 或 `主機:埠`；`kairos/HA.md` 第 8 節）。
    /// 環境變數以逗號分隔，見 [`crate::server_addr::de_address_list`]
    #[serde(default, deserialize_with = "crate::server_addr::de_address_list")]
    pub fallback_addresses: Vec<String>,
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
    /// 候選位址：`address:port`，接著 `fallback_addresses`（重複的只留第一個；`kairos/HA.md` 第 8.2 節）
    pub fn candidates(&self) -> Vec<crate::server_addr::Candidate> {
        crate::server_addr::candidates(self.address.trim(), self.port, &self.fallback_addresses)
    }

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

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct RetryConfig {
    /// 連續失敗達此次數後，重連日誌由 warn 升級為 error（不限制重試次數）
    pub max_retries: u32,
    /// 初始重連延遲（秒）
    pub initial_delay: u64,
    /// 重連延遲上限（秒）
    pub max_delay: u64,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct HeartbeatConfig {
    pub interval: u64,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ReportConfig {
    pub interval: u64,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct AgentConfig {
    /// 存放 agent_id 的檔案路徑
    pub id_file: String,
    /// 主控台顯示名稱（協定 v2 的 hello `display_name`）；空字串為不送
    pub display_name: String,
    /// 標籤（協定 v2 的 hello `tags`）
    pub tags: Vec<String>,
}

/// 註冊取得的身分（`enroll` 模組）
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct IdentityConfig {
    /// `identity/` 目錄（相對路徑以工作目錄解析）
    pub dir: String,
    /// 剩餘天數 ≤ 此值或已用壽命 2/3 時自動更新（取較早者；0 = 只看 2/3）
    pub renew_before_days: u32,
}

/// 註冊端點（8444）
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct EnrollConfig {
    /// 空字串 = 與 `server.address` 相同主機
    pub address: String,
    pub port: u16,
}

/// 本機緩衝（A5，見 `spool` 模組）
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct BufferConfig {
    /// 是否啟用：啟用時向 Kairos 宣告 `durable`（at-least-once）；停用時與 0.2.0 相同（at-most-once）
    pub enabled: bool,
    /// 緩衝目錄（相對路徑以工作目錄解析）
    pub dir: String,
    /// 待送資料的總大小上限（MiB）
    pub max_mb: u64,
    /// 待送資料的筆數上限
    pub max_records: u64,
    /// 取樣時間超過這麼久（小時）的資料不送出、直接丟棄
    pub max_age_hours: u64,
    /// 兩筆 report 之間的最短間隔（毫秒；補送積壓時的速率）。實際使用的值見
    /// [`AppConfig::effective_replay_interval_ms`]
    pub replay_interval_ms: u64,
}

/// `[collect]`：標準指標的採集範圍與每類上限（METRICS.md 第 4.3、4.6 節）
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct CollectConfig {
    /// 送 `system.filesystem.*`
    pub filesystems: bool,
    /// 送 `system.disk.*`
    pub disk_io: bool,
    /// 送 `system.network.*`
    pub network: bool,
    /// 送 `system.temperature`
    pub temperature: bool,
    /// 掛載點上限（0～[`MAX_COLLECT_FILESYSTEMS`]；0 = 不送）
    pub max_filesystems: u64,
    /// 磁碟 I/O 裝置上限（0～[`MAX_COLLECT_DISKS`]）
    pub max_disks: u64,
    /// 網路介面上限（0～[`MAX_COLLECT_INTERFACES`]）
    pub max_interfaces: u64,
    /// 額外排除的網路介面名稱（glob：`*`、`?`；最多 [`MAX_INTERFACE_EXCLUDE`] 個）
    pub interface_exclude: Vec<String>,
}

/// `[processes]`：行程數與行程排行（METRICS.md 第 5 節）
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ProcessesConfig {
    /// 宣告 feature `processes` 並送 `system.process.count`
    pub enabled: bool,
    /// 行程排行的間隔（秒，1～3600；實際不小於 `report.interval`，見
    /// [`AppConfig::effective_processes_interval`]）
    pub interval: u64,
    /// 依 CPU、依記憶體各取幾個（1～10）
    pub top: u64,
    /// 命令列：`none`（不送）／`executable`（只送執行檔路徑）／`full`（含參數，截斷到 1024 bytes）
    pub command: String,
    /// 送行程的使用者名稱（METRICS.md 第 17 節 D3：本版不實作，設為 true 只記一則 WARN）
    pub include_user: bool,
}

/// `[inventory]`：主機資產（METRICS.md 第 6 節）
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct InventoryConfig {
    /// 宣告 feature `inventory`
    pub enabled: bool,
    /// 重新收集資產的間隔（秒，300～86400）
    pub interval: u64,
    /// 送系統序號（`system.serial`）
    pub hardware_serials: bool,
    /// 送網路介面的 MAC 與 IP 位址
    pub network_addresses: bool,
}

/// 本機狀態檔（`pulse status` 讀取，見 `status_file` 模組）
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct StatusConfig {
    /// 狀態檔路徑（相對路徑以工作目錄解析）；空字串為設計包的位置（`status_file::default_path`），
    /// 無法寫入時改寫工作目錄的 `logs/status.json`
    pub file: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct LogConfig {
    /// 日誌級別；載入後已正規化為小寫並驗證過
    pub level: String,
    /// 日誌檔保留天數（今天與前 N 天）；0 = 永久保留，上限 `MAX_LOG_RETENTION_DAYS`
    pub retention_days: u32,
    /// `text`（RFC 3339 帶時區）、`json`（`logs/pulse-YYYY-MM-DD.jsonl`）、`legacy`（0.5 以前的格式）
    pub format: String,
    /// `local` 或 `utc`（只影響 `text`）
    pub timezone: String,
    /// 同時寫到 stdout（互動執行時；服務的 stdout 由 unit 的 StandardOutput 決定）
    pub stdout: bool,
    /// syslog 外送（RFC 5424）
    pub syslog: SyslogConfig,
}

/// `[log.syslog]`
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct SyslogConfig {
    /// `udp://主機:埠`、`tcp://主機:埠`、`unix:///dev/log`；空字串＝不外送
    pub target: String,
    pub facility: String,
    pub app_name: String,
    /// `text` 或 `json`
    pub payload: String,
}

impl LogConfig {
    /// 對應的 `LevelFilter`（`level` 已在載入時驗證，這裡不會失敗）。
    pub fn level_filter(&self) -> LevelFilter {
        parse_level(&self.level).unwrap_or(LevelFilter::Info)
    }

    /// `log.format`（已在載入時驗證）
    pub fn log_format(&self) -> kp_obs::logfmt::LogFormat {
        kp_obs::logfmt::LogFormat::parse(&self.format).unwrap_or_default()
    }

    /// `log.timezone`（已在載入時驗證）
    pub fn log_timezone(&self) -> kp_obs::logfmt::LogTimezone {
        kp_obs::logfmt::LogTimezone::parse(&self.timezone).unwrap_or_default()
    }

    /// `[log.syslog]` 轉成外送設定；`target` 為空時為 `None`
    pub fn syslog_config(&self, hostname: &str) -> Result<Option<kp_obs::syslog::Config>, String> {
        let s = &self.syslog;
        if s.target.trim().is_empty() {
            return Ok(None);
        }
        let transport = kp_obs::syslog::parse_target(&s.target)
            .map_err(|e| format!("log.syslog.target 無效：{e}"))?;
        let facility = kp_obs::syslog::parse_facility(&s.facility).ok_or_else(|| {
            format!(
                "log.syslog.facility「{}」無效（可用：daemon、user、auth、authpriv、local0～local7）",
                s.facility
            )
        })?;
        let payload = kp_obs::syslog::Payload::parse(&s.payload).ok_or_else(|| {
            format!(
                "log.syslog.payload「{}」無效（可用：text、json）",
                s.payload
            )
        })?;
        if s.app_name.trim().is_empty() {
            return Err("log.syslog.app_name 不可為空字串".to_string());
        }
        Ok(Some(kp_obs::syslog::Config {
            transport,
            facility,
            app_name: s.app_name.trim().to_string(),
            hostname: hostname.to_string(),
            payload,
        }))
    }
}

/// `[metrics]`：本機的 Prometheus 指標（預設都關閉）
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct MetricsConfig {
    /// 本機 HTTP `/metrics` 的監聽位址（例如 `127.0.0.1:9465`）；空字串＝不監聽。只接受 loopback 位址
    pub listen: String,
    /// node_exporter textfile collector 的輸出檔（副檔名 `.prom`）；空字串＝不寫
    pub textfile: String,
    /// textfile 的更新間隔（秒，5～3600）
    pub textfile_interval: u64,
}

impl MetricsConfig {
    /// `listen` 解析後的位址（空字串時為 `None`）
    pub fn listen_addr(&self) -> Result<Option<std::net::SocketAddr>, String> {
        let l = self.listen.trim();
        if l.is_empty() {
            return Ok(None);
        }
        let addr: std::net::SocketAddr = l
            .parse()
            .map_err(|_| format!("metrics.listen「{l}」必須是 IP:埠，例如 127.0.0.1:9465"))?;
        if !addr.ip().to_canonical().is_loopback() {
            return Err(format!(
                "metrics.listen「{l}」必須是 loopback 位址（127.0.0.1 或 ::1）：本機指標不需要認證，不對外開放"
            ));
        }
        Ok(Some(addr))
    }
}

/// `[remote_config]`：集中設定的本機開關（`crates/kairos-proto/REMOTE.md` 第 5.6 節；只能在本機設定）
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct RemoteConfigConfig {
    /// 接受 Kairos 下發的設定（宣告 feature `config`）。`false` 時只用本機設定，遠端快取保留不用
    pub enabled: bool,
}

/// `[upgrade]`：遠端升級的本機開關與信任錨（REMOTE.md 第 5.6 節；只能在本機設定，特權輔助程式只讀
/// `pulse.toml`）
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct UpgradeConfig {
    /// 接受 Kairos 推送的升級。`pulse.toml` 與環境變數都不為 `false` 才算開啟（環境變數只能關閉）
    pub enabled: bool,
    /// 信任錨（`ssh-keygen` 的 `allowed_signers`）；空字串 = 預設位置
    /// （[`UpgradeConfig::allowed_signers_path`]）
    pub allowed_signers: String,
    /// 除 `release@kairos-pulse` 外另外接受的 principal（客戶以自己的金鑰重新簽署，REMOTE.md D14）
    pub extra_principals: Vec<String>,
    /// 接受遠端降版（推送也必須允許）；自動回滾不受影響
    pub allow_downgrade: bool,
    /// 允許從外部 URL 下載的前綴（0.10.0 起使用；0.9.0 只從 Kairos 下載）
    pub url_allowlist: Vec<String>,
    /// 外部 URL 的 CA（PEM，0.10.0 起使用）；空字串 = 作業系統信任庫
    pub external_ca: String,
    /// 健康確認期限（秒；0 = 依 Kairos 的值，非 0 時取兩者較大，120～3600）
    pub health_timeout_secs: u32,
}

/// 預設的信任錨位置：Linux `/etc/kairos-pulse/allowed_signers`、Windows `%ProgramData%\Pulsellowed_signers`
pub fn default_allowed_signers_path() -> std::path::PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("ProgramData")
            .filter(|v| !v.is_empty())
            .map_or_else(
                || std::path::PathBuf::from(r"C:\ProgramData"),
                std::path::PathBuf::from,
            );
        base.join("Pulse").join("allowed_signers")
    } else {
        std::path::PathBuf::from("/etc/kairos-pulse/allowed_signers")
    }
}

impl UpgradeConfig {
    /// 實際使用的信任錨路徑（`allowed_signers` 為空字串時為預設位置）
    pub fn allowed_signers_path(&self) -> std::path::PathBuf {
        match self.allowed_signers.trim() {
            "" => default_allowed_signers_path(),
            p => std::path::PathBuf::from(p),
        }
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

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub retry: RetryConfig,
    pub heartbeat: HeartbeatConfig,
    pub report: ReportConfig,
    pub agent: AgentConfig,
    pub identity: IdentityConfig,
    pub enroll: EnrollConfig,
    pub buffer: BufferConfig,
    pub collect: CollectConfig,
    pub processes: ProcessesConfig,
    pub inventory: InventoryConfig,
    pub status: StatusConfig,
    pub log: LogConfig,
    pub metrics: MetricsConfig,
    /// 集中設定的本機開關（REMOTE.md 第 5.6 節）
    pub remote_config: RemoteConfigConfig,
    /// 遠端升級的本機開關與信任錨（REMOTE.md 第 5.6 節）
    pub upgrade: UpgradeConfig,
    /// 載入時目前工作目錄下是否有 `pulse.toml`（啟動時記錄，方便確認設定來源）
    #[serde(skip)]
    pub local_file_found: bool,
}

impl AppConfig {
    /// 依序疊加：嵌入的 config.toml → `pulse.toml`（可不存在）→ `PULSE__*` 環境變數。
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_file(std::path::Path::new(LOCAL_CONFIG_FILE))
    }

    /// 同 [`AppConfig::load`]，但讀取指定的設定檔（`pulse status --config`、從執行檔所在目錄找到的
    /// `pulse.toml`）。設定中的相對路徑由呼叫端以設定檔所在的目錄解析。
    pub fn load_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let found = path.is_file();
        let mut config = Self::build(Some(path), None)?;
        config.local_file_found = found;
        Ok(config)
    }

    /// 不讀設定檔：嵌入的預設值加上環境變數；環境變數也有誤時只用嵌入的預設值
    /// （設定有誤時，`pulse status` 仍需要預設的狀態檔位置）。
    pub fn defaults() -> Self {
        Self::build(None, None)
            .or_else(|_| Self::build(None, Some(Map::new())))
            .expect("嵌入的預設設定必須合法")
    }

    /// 測試用：只用嵌入的預設值（不讀 pulse.toml 與行程環境變數）。
    #[cfg(test)]
    pub fn default_for_tests() -> Self {
        Self::embedded_defaults()
    }

    /// 只用嵌入的預設值（不讀 pulse.toml 與行程環境變數）：集中設定的「內建預設」層
    /// （REMOTE.md 第 3.3 節）
    pub fn embedded_defaults() -> Self {
        Self::build(None, Some(Map::new())).expect("嵌入的預設設定必須合法")
    }

    /// 只讀嵌入的預設值與指定的設定檔，不讀環境變數：特權輔助程式（`pulse upgrade apply`）只信任
    /// root 擁有的 `pulse.toml`（REMOTE.md 第 4.4.1 節第 3 步、第 5.6 節）
    pub fn load_file_only(path: &std::path::Path) -> Result<Self, ConfigError> {
        let found = path.is_file();
        let mut config = Self::build(Some(path), Some(Map::new()))?;
        config.local_file_found = found;
        Ok(config)
    }

    /// `PULSE__*` 環境變數的來源（`env_override` 為 `Some` 時改用給定的鍵值，供測試使用）。
    /// 集中設定判斷「本機明確寫出的鍵」時也用同一個來源（`remote::config`）
    pub(crate) fn env_source(env_override: Option<Map<String, String>>) -> Environment {
        Environment::with_prefix("PULSE")
            .prefix_separator("__")
            .separator("__")
            .try_parsing(false)
            .source(env_override)
    }

    /// 實際使用的補送間隔（毫秒）：`buffer.replay_interval_ms`，但不超過 `report.interval` 的
    /// [`MAX_REPLAY_SHARE_PERMILLE`]（且不低於 [`MIN_REPLAY_INTERVAL_MS`]），讓積壓一定補得完。
    /// 預設 1200 ms、`report.interval = 1` 時為 600 ms
    pub fn effective_replay_interval_ms(&self) -> u64 {
        let cap = self
            .report
            .interval
            .saturating_mul(MAX_REPLAY_SHARE_PERMILLE)
            .max(MIN_REPLAY_INTERVAL_MS);
        self.buffer.replay_interval_ms.min(cap)
    }

    /// 實際的行程排行間隔（秒）：`processes.interval`，但不小於 `report.interval`
    /// （行程排行附在 report 上，比 report 更密沒有意義；不因此拒絕舊的設定）
    pub fn effective_processes_interval(&self) -> u64 {
        self.processes.interval.max(self.report.interval)
    }

    /// 實際的載入邏輯。
    ///
    /// `env_override` 為 `Some` 時改用給定的鍵值取代真正的行程環境變數（供測試使用）。
    ///
    /// 環境變數一律以字串讀入（不開 `try_parsing`）：數值與布林欄位在反序列化時由 config
    /// 把字串轉成對應型別；字串欄位（路徑、domain、address）則保持原樣，
    /// 例如 `PULSE__AGENT__ID_FILE=0001` 不會被改寫成 `"1"`。
    fn build(
        local_file: Option<&std::path::Path>,
        env_override: Option<Map<String, String>>,
    ) -> Result<Self, ConfigError> {
        let mut builder =
            Config::builder().add_source(File::from_str(EMBEDDED_CONFIG, FileFormat::Toml));
        if let Some(path) = local_file {
            builder = builder.add_source(File::from(path).format(FileFormat::Toml).required(false));
        }
        let env = Self::env_source(env_override);

        let mut config: Self = builder.add_source(env).build()?.try_deserialize()?;
        config.log.level = config.log.level.trim().to_ascii_lowercase();
        config.validate()?;
        Ok(config)
    }

    /// 檢查數值是否在合理範圍，避免執行期才出現 panic 或異常行為。集中設定合併後也以同樣的規則
    /// 檢查一次（REMOTE.md 第 3.5 節第 1 步）
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
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
        crate::server_addr::validate_fallback(&self.server.fallback_addresses, self.server.port)
            .map_err(ConfigError::Message)?;
        if self.server.domain.trim().is_empty() {
            return Err(ConfigError::Message(
                "server.domain 不可為空字串（TLS 以此驗證伺服器憑證的名稱）".to_string(),
            ));
        }
        // 與 tls::build 相同的檢查；在這裡先檢查，讓它歸類為設定錯誤（結束代碼 3）而不是憑證錯誤
        let domain = self.server.domain.trim();
        if let Err(e) = ServerName::try_from(domain.to_string()) {
            return Err(ConfigError::Message(format!(
                "server.domain「{domain}」不是有效的 DNS 名稱或 IP 位址：{e}"
            )));
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
        // 與 Kairos 的 hello 驗證相同的規則（協定 v2 的 display_name、tags），避免連線後才被拒絕
        let host = kairos_proto::HostInfo {
            display_name: Some(self.agent.display_name.trim().to_string())
                .filter(|n| !n.is_empty()),
            tags: Some(self.agent.tags.clone()),
            ..Default::default()
        };
        if let Err(e) = kairos_proto::validate_host_info(&host) {
            let (field, detail) = if e.starts_with("display_name") {
                (
                    "agent.display_name",
                    format!("1～{} bytes", kairos_proto::MAX_DISPLAY_NAME_BYTES),
                )
            } else {
                (
                    "agent.tags",
                    format!(
                        "最多 {} 個、每個 1～{} bytes、不得重複",
                        kairos_proto::MAX_TAGS,
                        kairos_proto::MAX_TAG_BYTES
                    ),
                )
            };
            return Err(ConfigError::Message(format!(
                "{field} 不符合限制（{detail}，不得含控制字元、格式字元等不可見字元）：{e}"
            )));
        }
        if self.identity.dir.trim().is_empty() {
            return Err(ConfigError::Message(
                "identity.dir 不可為空字串".to_string(),
            ));
        }
        range(
            "identity.renew_before_days",
            u64::from(self.identity.renew_before_days),
            0,
            365,
        )?;
        range("enroll.port", u64::from(self.enroll.port), 1, 65535)?;
        if self.buffer.enabled && self.buffer.dir.trim().is_empty() {
            return Err(ConfigError::Message(
                "buffer.dir 不可為空字串（啟用本機緩衝時必須指定目錄）".to_string(),
            ));
        }
        range(
            "buffer.max_mb",
            self.buffer.max_mb,
            MIN_BUFFER_MB,
            MAX_BUFFER_MB,
        )?;
        range(
            "buffer.max_records",
            self.buffer.max_records,
            MIN_BUFFER_RECORDS,
            MAX_BUFFER_RECORDS,
        )?;
        range(
            "buffer.max_age_hours",
            self.buffer.max_age_hours,
            1,
            MAX_BUFFER_AGE_HOURS,
        )?;
        range(
            "buffer.replay_interval_ms",
            self.buffer.replay_interval_ms,
            MIN_REPLAY_INTERVAL_MS,
            MAX_REPLAY_INTERVAL_MS,
        )?;

        range(
            "collect.max_filesystems",
            self.collect.max_filesystems,
            0,
            MAX_COLLECT_FILESYSTEMS,
        )?;
        range(
            "collect.max_disks",
            self.collect.max_disks,
            0,
            MAX_COLLECT_DISKS,
        )?;
        range(
            "collect.max_interfaces",
            self.collect.max_interfaces,
            0,
            MAX_COLLECT_INTERFACES,
        )?;
        if self.collect.interface_exclude.len() > MAX_INTERFACE_EXCLUDE {
            return Err(ConfigError::Message(format!(
                "collect.interface_exclude 最多 {MAX_INTERFACE_EXCLUDE} 個（目前 {} 個）",
                self.collect.interface_exclude.len()
            )));
        }
        if let Some(bad) = self
            .collect
            .interface_exclude
            .iter()
            .find(|g| g.trim().is_empty() || g.len() > 256)
        {
            return Err(ConfigError::Message(format!(
                "collect.interface_exclude 的每一項必須是 1～256 bytes 的 glob（例如 \"tap*\"），不可為空字串：{bad:?}"
            )));
        }
        range(
            "processes.interval",
            self.processes.interval,
            1,
            MAX_PROCESSES_INTERVAL,
        )?;
        range(
            "processes.top",
            self.processes.top,
            1,
            kairos_proto::MAX_TOP_PROCESSES as u64,
        )?;
        if !PROCESS_COMMAND_MODES.contains(&self.processes.command.trim()) {
            return Err(ConfigError::Message(format!(
                "processes.command「{}」無效（可用：none、executable、full）",
                self.processes.command
            )));
        }
        range(
            "inventory.interval",
            self.inventory.interval,
            MIN_INVENTORY_INTERVAL,
            MAX_INVENTORY_INTERVAL,
        )?;
        range(
            "log.retention_days",
            u64::from(self.log.retention_days),
            0,
            u64::from(MAX_LOG_RETENTION_DAYS),
        )?;
        if kp_obs::logfmt::LogFormat::parse(&self.log.format).is_none() {
            return Err(ConfigError::Message(format!(
                "log.format「{}」無效（可用：text、json、legacy）",
                self.log.format
            )));
        }
        if kp_obs::logfmt::LogTimezone::parse(&self.log.timezone).is_none() {
            return Err(ConfigError::Message(format!(
                "log.timezone「{}」無效（可用：local、utc）",
                self.log.timezone
            )));
        }
        self.log.syslog_config("-").map_err(ConfigError::Message)?;
        self.metrics.listen_addr().map_err(ConfigError::Message)?;
        let textfile = self.metrics.textfile.trim();
        if !textfile.is_empty() && !textfile.ends_with(".prom") {
            return Err(ConfigError::Message(format!(
                "metrics.textfile「{textfile}」的副檔名必須是 .prom（node_exporter 的 textfile collector 只讀 *.prom）"
            )));
        }
        range(
            "metrics.textfile_interval",
            self.metrics.textfile_interval,
            5,
            3600,
        )?;
        if parse_level(&self.log.level).is_none() {
            return Err(ConfigError::Message(format!(
                "log.level「{}」不是有效的日誌級別（可用：trace / debug / info / warn / error，不分大小寫）",
                self.log.level
            )));
        }
        let h = self.upgrade.health_timeout_secs;
        if h != 0
            && !(kairos_proto::mgmt::MIN_HEALTH_TIMEOUT_SECS
                ..=kairos_proto::mgmt::MAX_HEALTH_TIMEOUT_SECS)
                .contains(&h)
        {
            return Err(ConfigError::Message(format!(
                "upgrade.health_timeout_secs 必須是 0（依 Kairos 的值）或 {} 到 {} 之間（目前為 {h}）",
                kairos_proto::mgmt::MIN_HEALTH_TIMEOUT_SECS,
                kairos_proto::mgmt::MAX_HEALTH_TIMEOUT_SECS
            )));
        }
        if let Some(bad) = self.upgrade.extra_principals.iter().find(|p| {
            p.is_empty()
                || p.len() > 256
                || p.chars()
                    .any(|c| c.is_whitespace() || c.is_control() || c == ',')
        }) {
            return Err(ConfigError::Message(format!(
                "upgrade.extra_principals 的每一項必須是 1～256 bytes、不含空白與逗號的名稱（目前有 {bad:?}）"
            )));
        }
        if let Some(bad) = self.upgrade.url_allowlist.iter().find(|u| {
            !u.starts_with("https://") || u.len() > kairos_proto::mgmt::MAX_ARTIFACT_URL_BYTES
        }) {
            return Err(ConfigError::Message(format!(
                "upgrade.url_allowlist 的每一項必須以 https:// 開頭（目前有 {bad:?}）"
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
        // A11：預設心跳 15 秒（Kairos 離線判定 5 × 15 = 75 秒，在其 1～60 秒的夾值範圍內）
        assert_eq!(cfg.heartbeat.interval, 15);
        assert_eq!(cfg.report.interval, 5);
        assert_eq!(cfg.agent.id_file, "agent.id");
        // A5：本機緩衝預設啟用
        assert!(cfg.buffer.enabled);
        assert_eq!(cfg.buffer.dir, "buffer");
        // METRICS.md D7：每筆紀錄變大後改為 128 MiB（約 2～3 天）
        assert_eq!(cfg.buffer.max_mb, 128);
        assert_eq!(cfg.buffer.max_records, 50_000);
        assert_eq!(cfg.buffer.max_age_hours, 168);
        // 1 萬台同時恢復連線時約每秒 8,300 筆（STORAGE.md 第 11.1 節）
        assert_eq!(cfg.buffer.replay_interval_ms, 1200);
        assert_eq!(cfg.effective_replay_interval_ms(), 1200);
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
    fn buffer_settings_are_validated() {
        for (key, value) in [
            ("PULSE__BUFFER__MAX_MB", "0"),
            ("PULSE__BUFFER__MAX_MB", "102401"),
            ("PULSE__BUFFER__MAX_RECORDS", "99"),
            ("PULSE__BUFFER__MAX_AGE_HOURS", "0"),
            ("PULSE__BUFFER__MAX_AGE_HOURS", "8761"),
            // 低於 500 ms 會超過 Kairos 的 report 速率限制（每秒 2 則）
            ("PULSE__BUFFER__REPLAY_INTERVAL_MS", "499"),
            ("PULSE__BUFFER__REPLAY_INTERVAL_MS", "60001"),
        ] {
            let err = build_err(&[(key, value)]);
            let name = key
                .trim_start_matches("PULSE__BUFFER__")
                .to_ascii_lowercase();
            assert!(
                err.contains(&format!("buffer.{name}")),
                "{key}={value}: {err}"
            );
        }
        assert!(build_err(&[("PULSE__BUFFER__DIR", " ")]).contains("buffer.dir"));
        // 停用時目錄可以留空
        let cfg = AppConfig::build(
            None,
            Some(env(&[
                ("PULSE__BUFFER__ENABLED", "false"),
                ("PULSE__BUFFER__DIR", ""),
                ("PULSE__BUFFER__REPLAY_INTERVAL_MS", "500"),
            ])),
        )
        .unwrap();
        assert!(!cfg.buffer.enabled);
        assert_eq!(cfg.buffer.replay_interval_ms, 500);
    }

    #[test]
    fn replay_interval_is_capped_below_the_report_interval() {
        let with = |report: &str, replay: &str| {
            AppConfig::build(
                None,
                Some(env(&[
                    ("PULSE__REPORT__INTERVAL", report),
                    ("PULSE__BUFFER__REPLAY_INTERVAL_MS", replay),
                ])),
            )
            .unwrap()
            .effective_replay_interval_ms()
        };
        // 取樣每秒一筆時，預設的 1200 ms 會比取樣慢：改用 600 ms（與 0.2.x 相同）
        assert_eq!(with("1", "1200"), 600);
        assert_eq!(with("1", "500"), 500);
        assert_eq!(with("2", "1200"), 1200);
        assert_eq!(with("2", "5000"), 1200);
        assert_eq!(with("5", "2000"), 2000);
        assert_eq!(with("5", "60000"), 3000);
        assert_eq!(with("3600", "60000"), 60000);
        // 送出速率一定高於取樣速率
        for report in [1u64, 2, 3, 5, 60] {
            for replay in [500u64, 600, 1200, 5000, 60000] {
                let ms = with(&report.to_string(), &replay.to_string());
                assert!(
                    ms < report * 1000,
                    "report {report} s、replay {replay} ms → {ms}"
                );
            }
        }
    }

    #[test]
    fn invalid_values_rejected() {
        assert!(build_err(&[("PULSE__HEARTBEAT__INTERVAL", "0")]).contains("heartbeat.interval"));
        assert!(build_err(&[("PULSE__RETRY__MAX_DELAY", "1")]).contains("max_delay"));
    }

    #[test]
    fn fallback_addresses_from_file_defaults_and_env() {
        let cfg = AppConfig::build(None, Some(Map::new())).unwrap();
        assert!(cfg.server.fallback_addresses.is_empty());
        assert_eq!(cfg.server.candidates().len(), 1);
        // 環境變數：逗號分隔，去除空白，空字串表示沒有
        let cfg = AppConfig::build(
            None,
            Some(env(&[
                ("PULSE__SERVER__PORT", "8443"),
                (
                    "PULSE__SERVER__FALLBACK_ADDRESSES",
                    " kairos-b.example.com , 10.0.20.11:9443 ,",
                ),
            ])),
        )
        .unwrap();
        assert_eq!(
            cfg.server.fallback_addresses,
            ["kairos-b.example.com", "10.0.20.11:9443"]
        );
        let shown: Vec<String> = cfg
            .server
            .candidates()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            shown,
            [
                "127.0.0.1:8443",
                "kairos-b.example.com:8443",
                "10.0.20.11:9443"
            ]
        );
        let cfg = AppConfig::build(
            None,
            Some(env(&[("PULSE__SERVER__FALLBACK_ADDRESSES", "")])),
        )
        .unwrap();
        assert!(cfg.server.fallback_addresses.is_empty());
        // 設定檔的陣列
        let dir = std::env::temp_dir().join(format!("pulse-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pulse.toml");
        std::fs::write(
            &file,
            "[server]
fallback_addresses = [\"[2001:db8::10]\", \"kairos-c:9000\"]
",
        )
        .unwrap();
        let cfg = AppConfig::build(Some(&file), Some(Map::new())).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            cfg.server.fallback_addresses,
            ["[2001:db8::10]", "kairos-c:9000"]
        );
        assert_eq!(
            cfg.server.candidates()[1].to_string(),
            "[2001:db8::10]:8080"
        );
    }

    #[test]
    fn invalid_fallback_addresses_are_config_errors() {
        // PLS-E0201：IPv6 沒有方括號、連接埠不合法、超過 16 個
        for bad in [
            "2001:db8::10",
            "host:0",
            "bad_host",
            "a.example.com,b:70000",
        ] {
            let err = build_err(&[("PULSE__SERVER__FALLBACK_ADDRESSES", bad)]);
            assert!(err.contains("server.fallback_addresses"), "{bad}: {err}");
        }
        let many: Vec<String> = (0..17).map(|i| format!("h{i}.example.com")).collect();
        let err = build_err(&[("PULSE__SERVER__FALLBACK_ADDRESSES", many.join(",").as_str())]);
        assert!(err.contains("最多 16 個"), "{err}");
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
    fn invalid_domain_is_a_config_error() {
        // 不是有效的 DNS 名稱或 IP 位址：設定錯誤（結束代碼 3），而不是等到建立 TLS 設定時才失敗
        for bad in ["bad domain", "kairos..example.com", "-"] {
            let err = build_err(&[("PULSE__SERVER__DOMAIN", bad)]);
            assert!(
                err.contains("server.domain") && err.contains(bad),
                "{bad:?}: {err}"
            );
        }
        for good in ["kairos.example.com", "localhost", "127.0.0.1", "::1"] {
            let cfg =
                AppConfig::build(None, Some(env(&[("PULSE__SERVER__DOMAIN", good)]))).unwrap();
            assert_eq!(cfg.server.domain, good);
        }
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
            AppConfig::build(Some(file.as_path()), Some(Map::new())).unwrap()
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

    #[test]
    fn display_name_and_tags_from_pulse_toml() {
        // 協定 v2 的 display_name／tags：預設不送；不符合 Kairos 的 hello 限制時啟動即報錯
        let cfg = AppConfig::build(None, Some(Map::new())).unwrap();
        assert_eq!(cfg.agent.display_name, "");
        assert!(cfg.agent.tags.is_empty());
        let cfg =
            AppConfig::build(None, Some(env(&[("PULSE__AGENT__DISPLAY_NAME", "Web 01")]))).unwrap();
        assert_eq!(cfg.agent.display_name, "Web 01");
        assert!(
            build_err(&[("PULSE__AGENT__DISPLAY_NAME", "a\u{202e}b")])
                .contains("agent.display_name")
        );
        assert!(
            build_err(&[("PULSE__AGENT__DISPLAY_NAME", &"n".repeat(256))])
                .contains("agent.display_name")
        );

        let dir = std::env::temp_dir()
            .join("pulse-config-tests")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pulse.toml");
        let load = |content: &str| {
            std::fs::write(&file, content).unwrap();
            AppConfig::build(Some(file.as_path()), Some(Map::new()))
        };
        let cfg = load("[agent]\ndisplay_name = \"主機 A\"\ntags = [\"env:prod\", \"team:web\"]\n")
            .unwrap();
        assert_eq!(cfg.agent.display_name, "主機 A");
        assert_eq!(cfg.agent.tags, ["env:prod", "team:web"]);
        for bad in [
            "[agent]\ntags = [\"a\", \"a\"]\n",
            "[agent]\ntags = [\"\"]\n",
            "[agent]\ntags = [\"a\\nb\"]\n",
        ] {
            let err = load(bad).unwrap_err().to_string();
            assert!(err.contains("agent.tags"), "{bad}: {err}");
        }
        let many: Vec<String> = (0..=kairos_proto::MAX_TAGS)
            .map(|i| format!("\"t{i}\""))
            .collect();
        let err = load(&format!("[agent]\ntags = [{}]\n", many.join(",")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("agent.tags"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn metrics_collection_defaults_follow_the_spec() {
        // METRICS.md 第 4.6 節的預設值
        let cfg = AppConfig::default_for_tests();
        assert!(cfg.collect.filesystems && cfg.collect.disk_io);
        assert!(cfg.collect.network && cfg.collect.temperature);
        assert_eq!(
            (
                cfg.collect.max_filesystems,
                cfg.collect.max_disks,
                cfg.collect.max_interfaces
            ),
            (16, 8, 16)
        );
        assert!(cfg.collect.interface_exclude.is_empty());
        assert!(cfg.processes.enabled);
        assert_eq!(cfg.processes.interval, 60);
        assert_eq!(cfg.effective_processes_interval(), 60);
        assert_eq!(cfg.processes.top, 5);
        assert_eq!(cfg.processes.command, "none");
        assert!(!cfg.processes.include_user);
        assert!(cfg.inventory.enabled);
        assert_eq!(cfg.inventory.interval, 3600);
        assert!(cfg.inventory.hardware_serials && cfg.inventory.network_addresses);
    }

    #[test]
    fn metrics_collection_settings_are_validated() {
        for (key, value, name) in [
            (
                "PULSE__COLLECT__MAX_FILESYSTEMS",
                "17",
                "collect.max_filesystems",
            ),
            ("PULSE__COLLECT__MAX_DISKS", "9", "collect.max_disks"),
            (
                "PULSE__COLLECT__MAX_INTERFACES",
                "17",
                "collect.max_interfaces",
            ),
            ("PULSE__PROCESSES__INTERVAL", "0", "processes.interval"),
            ("PULSE__PROCESSES__INTERVAL", "3601", "processes.interval"),
            ("PULSE__PROCESSES__TOP", "0", "processes.top"),
            ("PULSE__PROCESSES__TOP", "11", "processes.top"),
            ("PULSE__PROCESSES__COMMAND", "args", "processes.command"),
            ("PULSE__INVENTORY__INTERVAL", "299", "inventory.interval"),
            ("PULSE__INVENTORY__INTERVAL", "86401", "inventory.interval"),
        ] {
            let err = build_err(&[(key, value)]);
            assert!(err.contains(name), "{key}={value}: {err}");
        }
        // 上限 0 = 該類不送；邊界值可接受
        let cfg = AppConfig::build(
            None,
            Some(env(&[
                ("PULSE__COLLECT__MAX_FILESYSTEMS", "0"),
                ("PULSE__COLLECT__MAX_DISKS", "8"),
                ("PULSE__PROCESSES__COMMAND", "full"),
                ("PULSE__PROCESSES__TOP", "10"),
                ("PULSE__INVENTORY__INTERVAL", "300"),
            ])),
        )
        .unwrap();
        assert_eq!(cfg.collect.max_filesystems, 0);
        assert_eq!(cfg.processes.command, "full");
        // 行程排行附在 report 上：間隔不小於 report.interval（不因此拒絕舊設定）
        let cfg = AppConfig::build(
            None,
            Some(env(&[
                ("PULSE__REPORT__INTERVAL", "300"),
                ("PULSE__PROCESSES__INTERVAL", "60"),
            ])),
        )
        .unwrap();
        assert_eq!(cfg.effective_processes_interval(), 300);
    }

    #[test]
    fn interface_exclude_from_pulse_toml_is_validated() {
        let dir = std::env::temp_dir().join(format!("pulse-cfg-exclude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pulse.toml");
        let load = |text: &str| {
            std::fs::write(&path, text).unwrap();
            AppConfig::build(Some(&path), Some(Map::new()))
        };
        let cfg = load("[collect]\ninterface_exclude = [\"tap*\", \"wg?\"]\n").unwrap();
        assert_eq!(cfg.collect.interface_exclude, ["tap*", "wg?"]);
        let err = load("[collect]\ninterface_exclude = [\" \"]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("collect.interface_exclude"), "{err}");
        let many: Vec<String> = (0..33).map(|i| format!("\"x{i}\"")).collect();
        let err = load(&format!(
            "[collect]\ninterface_exclude = [{}]\n",
            many.join(", ")
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("最多 32 個"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
