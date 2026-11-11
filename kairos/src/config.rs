//! 設定載入：程式內預設 → `kairos.toml`（或 `--config <path>`）→ `KAIROS__<SECTION>__<KEY>` 環境變數。

use config::{Config, Environment, File, FileFormat};
use log::LevelFilter;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{Error, ErrorKind};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

mod accounts;
mod alerts;
pub mod cluster;
mod enroll;
pub mod export;
mod postgres;
mod remote;

pub use alerts::{AlertSettings, parse_offset as alerts_parse_offset};

pub use accounts::{AccountSettings, AuditSettings, LdapSettings};
pub use cluster::ClusterSettings;
pub use enroll::{
    CertPolicySettings, EnrollSettings, MAX_CERT_LIFETIME_DAYS, MAX_GRACE_DAYS,
    MIN_CERT_LIFETIME_DAYS, PkiMode, PkiSettings,
};
pub use export::{ExportSettings, OtlpSettings, PrometheusExportSettings};
pub use postgres::{PgSslMode, PostgresSettings};
pub use remote::RemoteSettings;

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

/// `storage.raw_retention_days` 的預設值：原始 report 保留 7 天（今天與前 7 天）
pub const DEFAULT_RAW_RETENTION_DAYS: u32 = 7;
/// `storage.rollup_5m_retention_days` 的預設值
pub const DEFAULT_ROLLUP_5M_RETENTION_DAYS: u32 = 31;
/// `storage.rollup_1h_retention_days` 的預設值（13 個月，可做跨年同期比較）
pub const DEFAULT_ROLLUP_1H_RETENTION_DAYS: u32 = 400;
/// `storage.max_total_bytes` 的預設值：64 GiB（SQLite 資料庫與 WAL 合計；0 = 不限制）
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// `storage.max_total_bytes` 不為 0 時的下限（64 MiB）
pub const MIN_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// 主機清冊筆數上限的預設值（實際為 `max(此值, limits.max_agents)`）
pub const DEFAULT_MAX_HOSTS: usize = 100_000;
/// `storage.max_hosts` 的最大值
pub const MAX_MAX_HOSTS: usize = 1_000_000;

/// 容量估算（`kairos/STORAGE.md`「容量估算」）：每台主機每 5 秒一筆 report
pub const ESTIMATE_REPORT_INTERVAL_SECS: u64 = 5;
/// 容量估算：每筆原始 report 在 SQLite 中的大小（含索引與主機、序列等）。0.8.0 的 Pulse 在一般伺服器
/// （2 個掛載點、2 個磁碟 I/O 裝置、2 個介面）約 41 個指標、約 530 bytes（METRICS.md 第 9.6 節；
/// 0.2.0 的 17 個指標實測約 235 bytes，見 STORAGE.md「量測」）
pub const ESTIMATED_RAW_ROW_BYTES: u64 = 530;
/// 容量估算：每台主機彙總的序列（指標名稱＋label 組合）數；計數器只彙總速率（METRICS.md 第 9.6 節）
pub const ESTIMATED_SERIES_PER_HOST: u64 = 47;
/// 容量估算：行程快照每台每天的大小（每 60 秒一筆、約 1 KB；保留期限同原始資料）
pub const ESTIMATED_PROCESS_BYTES_PER_DAY: u64 = 1440 * 1000;
/// `storage.max_series_per_host` 的預設值（METRICS.md 第 9.1 節）
pub const DEFAULT_MAX_SERIES_PER_HOST: usize = 2048;
/// `storage.max_series_per_host` 的範圍
pub const MIN_MAX_SERIES_PER_HOST: usize = 64;
pub const MAX_MAX_SERIES_PER_HOST: usize = 65_536;
/// 容量估算：每列彙總的大小
pub const ESTIMATED_ROLLUP_ROW_BYTES: u64 = 50;

/// 同時連線數上限的最大值
pub const MAX_MAX_CONNECTIONS: usize = 65535;

/// 狀態表筆數上限的最大值。狀態表已滿時，每次註冊新 agent 都要在全域鎖下以 O(n) 找出
/// 離線最久者，因此上限不宜過大
pub const MAX_MAX_AGENTS: usize = 100_000;

/// 未設定 `server.max_connections_per_ip` 時的預設值（不超過 `server.max_connections`）
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 32;

/// 未設定 `server.max_connections_per_allowlisted_ip` 時的預設值（設計包 D14）。
/// 實際生效值不超過 `server.max_connections`（D15）
pub const DEFAULT_MAX_CONNECTIONS_PER_ALLOWLISTED_IP: usize = 4096;

/// `server.max_connections_per_allowlisted_ip` 的最大值（D15）
pub const MAX_MAX_CONNECTIONS_PER_ALLOWLISTED_IP: usize = 65535;

/// 未設定 `limits.max_agents` 時的預設值（不小於 `server.max_connections`）
pub const DEFAULT_MAX_AGENTS: usize = 10000;

/// reports 與日誌保留天數的上限（0 表示永久保留）
pub const MAX_RETENTION_DAYS: u32 = 3650;

/// TLS 熱重載檢查間隔的上限（秒；0 表示停用）
pub const MAX_RELOAD_INTERVAL_SECS: u64 = 3600;

/// `server.cert_expiry_warning_days` 的預設值：伺服器憑證到期前 30 天開始預警
pub const DEFAULT_CERT_EXPIRY_WARNING_DAYS: u32 = 30;

/// `auth.crl_expiry_warning_days` 的預設值：CRL 的 nextUpdate 前 7 天開始預警
pub const DEFAULT_CRL_EXPIRY_WARNING_DAYS: u32 = 7;

/// 到期預警天數的上限（0 表示不預警，已過期時仍記錄 ERROR）
pub const MAX_EXPIRY_WARNING_DAYS: u32 = 365;

/// `console.listen` 的預設值：只綁 loopback（設計包 `ia.md` §3.2：初始化期間只綁 127.0.0.1:9443）
pub const DEFAULT_CONSOLE_LISTEN: &str = "127.0.0.1:9443";

/// `console.max_connections` 的預設值與上限
pub const DEFAULT_CONSOLE_MAX_CONNECTIONS: usize = 64;
pub const MAX_CONSOLE_MAX_CONNECTIONS: usize = 4096;

/// `console.max_streams`（SSE 的同時連線數）的預設值
pub const DEFAULT_CONSOLE_MAX_STREAMS: usize = 16;

/// 除了連線之外，行程需要的檔案描述符（日誌、資料庫與 WAL、TLS 設定檔、listener 等）的估計量
pub const FD_RESERVE: u64 = 64;

/// `KRS-E0202`：兩個監聽位址相同（agent、主控台、註冊端點之間）
pub fn listen_conflict_between(a_name: &str, a: &str, b_name: &str, b: &str) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        format!(
            "錯誤 KRS-E0202：設定互相矛盾：{a_name}（{a}）與 {b_name}（{b}）使用同一個位址與連接埠。agent 連線埠（預設 8443）、註冊端點（預設 8444）與主控台（預設 9443）必須分開；請修改其中一個"
        ),
    )
}

/// 建議的 `LimitNOFILE` 下限
pub const MIN_SUGGESTED_NOFILE: u64 = 4096;

/// `auth.crl` 以環境變數設定時的清單分隔字元
pub const LIST_SEPARATOR: &str = ",";

pub const USAGE: &str = "用法: kairos [--config <path>]\n       \
kairos hosts list [選項]（主機清冊；kairos hosts --help 顯示說明）\n       \
kairos hosts purge|export|pseudonym <…>（刪除主機資料、匯出個資、查詢假名；kairos/PRIVACY.md）\n       \
kairos reports export [選項]（匯出 report；kairos reports --help 顯示說明）\n       \
kairos reports import [選項] <檔案或目錄>...（匯入 0.2.x 的 JSONL；kairos reports import --help 顯示說明）\n       \
kairos db backup <備份檔>（線上備份；kairos db --help 顯示說明）\n       \
kairos api-token create|list|revoke [選項]（主控台 API 的 token；kairos api-token --help 顯示說明）\n       \
kairos enroll-token create|list|show|disable [選項]（註冊 token）\n       \
kairos enroll list|show|approve|reject|mode [選項]（註冊申請與註冊端點模式）\n       \
kairos alerts list|show|ack|resolve|comment [選項]（告警事件；kairos alerts --help 顯示說明）\n       \
kairos silences list|create|end [選項]（靜音與維護時段）\n       \
kairos alert-rules list|enable|disable [選項]（告警規則）\n       \
kairos alert-channels list（通知管道）\n       \
kairos pki issuing-ca|certificates|revoke|queue|sign-bundle [選項]（簽發 CA、憑證與 CRL）\n       \
kairos config show --effective（實際生效的設定與來源）\n       \
kairos users create|list|show|grant|revoke-role|disable|enable|delete|reset-password|reset-mfa|unlock|sessions|end-sessions|map-group [選項]（使用者與角色）\n       \
kairos audit list|verify|export [選項]（稽核日誌）\n       \
kairos cluster status|init|keys|drain|step-down|forget [選項]（叢集；kairos cluster --help 顯示說明）\n\n\
kairos --check-config [--config <path>] [--strict] [--json]（檢查設定後結束）\n       \
kairos diag [選項]（產生支援診斷包；kairos diag --help 顯示說明）\n\n\
選項:\n  \
--config <path>  指定設定檔（預設讀取目前工作目錄的 kairos.toml，不存在則使用預設值）\n  \
-V, --version    顯示版本與建置的 commit\n  \
-h, --help       顯示此說明\n\n\
環境變數 KAIROS__<SECTION>__<KEY> 可覆寫設定，例如 KAIROS__SERVER__LISTEN=0.0.0.0:8080";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerSettings,
    pub timeouts: TimeoutSettings,
    pub storage: StorageSettings,
    pub auth: AuthSettings,
    pub limits: LimitSettings,
    pub log: LogSettings,
    pub console: ConsoleSettings,
    /// `/healthz`、`/readyz`、`/metrics`（審計 O3）
    pub observability: ObservabilitySettings,
    /// 註冊通道（8444，`crates/kairos-proto/ENROLL.md`）
    pub enroll: EnrollSettings,
    /// PKI 與憑證政策（`kairos/PKI.md` 第 8 節）
    pub pki: PkiSettings,
    /// 告警引擎（`kairos/ALERTS.md`）
    pub alerts: AlertSettings,
    /// 本機帳號、密碼政策、登入鎖定與工作階段（`kairos/API.md` 第 12 節）
    pub accounts: AccountSettings,
    /// 目錄服務（LDAP／AD）
    pub ldap: LdapSettings,
    /// 稽核日誌
    pub audit: AuditSettings,
    /// 指標轉出：Prometheus exporter、OTLP（`kairos/EXPORT.md`）
    pub export: ExportSettings,
    /// 個資保護（商業審計 S6；`kairos/PRIVACY.md`）
    pub privacy: crate::privacy::PrivacySettings,
    /// 叢集的節點設定（0.9.0 只有 `node_id`，`kairos/HA.md` 第 15.1 節）
    pub cluster: ClusterSettings,
    /// 集中設定與 Pulse 遠端升級（`crates/kairos-proto/REMOTE.md` 第 7 節）
    pub remote: RemoteSettings,
    /// 每個設定鍵的來源（設定檔或環境變數；`kairos config show --effective`、主控台的鎖定判斷）
    #[serde(skip)]
    pub provenance: Provenance,
}

/// 設定鍵（以 `.` 連接的小寫路徑，例如 `enroll.mode`）的來源
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Provenance {
    /// 設定檔中出現的鍵
    pub file_keys: BTreeSet<String>,
    /// 以 `KAIROS__<SECTION>__<KEY>` 環境變數設定的鍵
    pub env_keys: BTreeSet<String>,
    /// 設定檔中的值（`kairos config show --effective` 的「被覆寫的值」）
    pub file_values: std::collections::BTreeMap<String, serde_json::Value>,
}

/// 設定值的來源（`kairos config show --effective`、`GET /api/v1/enroll/mode`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingSource {
    /// `KAIROS__…` 環境變數（主控台無法修改）
    Environment,
    /// 主控台或 CLI 寫入資料庫 `settings` 表的值
    Console,
    /// `kairos.toml`
    File,
    /// 程式內建預設
    Default,
}

impl SettingSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SettingSource::Environment => "environment",
            SettingSource::Console => "console",
            SettingSource::File => "file",
            SettingSource::Default => "default",
        }
    }
}

impl Provenance {
    /// 鍵對應的環境變數名稱（`enroll.mode` → `KAIROS__ENROLL__MODE`）
    pub fn env_var(key: &str) -> String {
        format!("KAIROS__{}", key.replace('.', "__").to_ascii_uppercase())
    }

    /// 不考慮資料庫覆寫時的來源
    pub fn source(&self, key: &str) -> SettingSource {
        if self.env_keys.contains(key) {
            SettingSource::Environment
        } else if self.file_keys.contains(key) {
            SettingSource::File
        } else {
            SettingSource::Default
        }
    }

    /// 目前行程的 `KAIROS__…` 環境變數設定了哪些鍵
    pub fn env_keys_from_process() -> BTreeSet<String> {
        std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .filter_map(|k| {
                k.strip_prefix("KAIROS__").map(|rest| {
                    rest.split("__")
                        .collect::<Vec<_>>()
                        .join(".")
                        .to_ascii_lowercase()
                })
            })
            .filter(|k| !k.is_empty())
            .collect()
    }
}

/// 把設定檔的內容攤平成 鍵 → 值
fn flatten_file_values(
    prefix: &str,
    value: &serde_json::Value,
    out: &mut std::collections::BTreeMap<String, serde_json::Value>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.to_ascii_lowercase()
                } else {
                    format!("{prefix}.{}", k.to_ascii_lowercase())
                };
                flatten_file_values(&key, v, out);
            }
        }
        v => {
            if !prefix.is_empty() {
                out.insert(prefix.to_string(), v.clone());
            }
        }
    }
}

/// 把 JSON 物件攤平成 `a.b.c` 形式的鍵（陣列與純量為葉節點）
pub fn flatten_keys(prefix: &str, value: &serde_json::Value, out: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.to_ascii_lowercase()
                } else {
                    format!("{prefix}.{}", k.to_ascii_lowercase())
                };
                flatten_keys(&key, v, out);
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.insert(prefix.to_string());
            }
        }
    }
}

/// 主控台 API（`[console]`，見 `kairos/API.md` 第 2 節）
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ConsoleSettings {
    /// `false`：不監聽主控台連接埠
    pub enabled: bool,
    /// 監聽位址；預設只綁 loopback
    pub listen: String,
    /// 主控台的伺服器憑證；空字串時沿用 `server.cert_path`
    pub cert_path: String,
    /// 主控台的私鑰；空字串時沿用 `server.key_path`（與 `cert_path` 同時設定或同時留空）
    pub key_path: String,
    /// 同時連線上限（含 SSE）
    pub max_connections: usize,
    /// SSE（`/api/v1/events`）的同時連線上限
    pub max_streams: usize,
}

impl Default for ConsoleSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: DEFAULT_CONSOLE_LISTEN.to_string(),
            cert_path: String::new(),
            key_path: String::new(),
            max_connections: DEFAULT_CONSOLE_MAX_CONNECTIONS,
            max_streams: DEFAULT_CONSOLE_MAX_STREAMS,
        }
    }
}

impl ConsoleSettings {
    /// 主控台實際使用的憑證與私鑰：未設定時沿用 agent 連線埠的（`server.cert_path`／`key_path`）
    pub fn tls_files(&self, server: &ServerSettings) -> (String, String) {
        if self.cert_path.trim().is_empty() {
            (server.cert_path.clone(), server.key_path.clone())
        } else {
            (
                self.cert_path.trim().to_string(),
                self.key_path.trim().to_string(),
            )
        }
    }
}

/// 兩個監聽位址是否會衝突：連接埠相同（且不是 0），且 IP 相同或其中一個是所有介面（`0.0.0.0`、`::`）
pub fn listen_addrs_conflict(a: &SocketAddr, b: &SocketAddr) -> bool {
    if a.port() == 0 || a.port() != b.port() {
        return false;
    }
    let (x, y) = (a.ip().to_canonical(), b.ip().to_canonical());
    x == y || x.is_unspecified() || y.is_unspecified()
}

/// `KRS-E0202`：agent 連線埠與主控台的監聽位址相同
pub fn listen_conflict_error(agent: &str, console: &str) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        format!(
            "錯誤 KRS-E0202：設定互相矛盾：server.listen（{agent}）與 console.listen（{console}）使用同一個位址與連接埠。agent 連線埠與主控台（預設 9443）必須分開；請修改其中一個，或以 console.enabled = false 停用主控台"
        ),
    )
}

/// 建議的 `LimitNOFILE`：不小於需要量的 2 的冪次，且至少 [`MIN_SUGGESTED_NOFILE`]
pub fn suggested_nofile(needed: u64) -> u64 {
    needed
        .checked_next_power_of_two()
        .unwrap_or(needed)
        .max(MIN_SUGGESTED_NOFILE)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerSettings {
    pub listen: String,
    pub cert_path: String,
    pub key_path: String,
    pub max_connections: usize,
    /// 同一個來源 IP 位址的連線數上限。未啟用 mTLS 時計算全部連線（含握手中、尚未送出 hello 的連線）；
    /// 啟用 mTLS 時只計算尚未通過 hello 的連線，通過後改依憑證身分計數（審計 A7，見 `per_ip` 模組）。
    /// 未設定時為 `min(32, max_connections)`，見 [`AppConfig::max_connections_per_ip`]
    pub max_connections_per_ip: Option<usize>,
    /// 每 IP 上限的網段允許清單（CIDR，IPv4／IPv6；只寫位址表示單一位址）。清單內的位址改用
    /// `max_connections_per_allowlisted_ip`（審計 A7 過渡方案，設計包 D14、D15）。
    /// 也接受以 [`LIST_SEPARATOR`] 分隔的單一字串（環境變數 `KAIROS__SERVER__PER_IP_ALLOWLIST`）
    #[serde(deserialize_with = "string_or_list")]
    pub per_ip_allowlist: Vec<String>,
    /// `per_ip_allowlist` 內每個位址的上限，1..=65535；未設定時為 4096。實際生效值為
    /// `min(此值, max_connections)`，見 [`AppConfig::max_connections_per_allowlisted_ip`]
    pub max_connections_per_allowlisted_ip: Option<usize>,
    /// 每隔幾秒檢查憑證、私鑰、客戶端 CA 與 CRL 檔是否變更（並重新驗證現役連線的客戶端憑證）；0 = 停用
    pub reload_interval_secs: u64,
    /// 伺服器憑證到期前幾天開始記 WARN（O1）；0 = 不預警（已過期時仍記錄 ERROR）
    pub cert_expiry_warning_days: u32,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".to_string(),
            cert_path: "cert.pem".to_string(),
            key_path: "key.pem".to_string(),
            max_connections: 1024,
            max_connections_per_ip: None,
            per_ip_allowlist: Vec::new(),
            max_connections_per_allowlisted_ip: None,
            reload_interval_secs: 30,
            cert_expiry_warning_days: DEFAULT_CERT_EXPIRY_WARNING_DAYS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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

/// report 的儲存後端（`storage.backend`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    /// SQLite（`<data_dir>/kairos.db`）：原始資料、彙總與主機清冊（A1、A6、A9）
    #[default]
    Sqlite,
    /// 舊版的按日 JSONL（`reports-YYYY-MM-DD.jsonl`）；沒有主機清冊與彙總，只供相容與回復
    Jsonl,
    /// PostgreSQL（`[storage.postgres]`；`kairos/HA.md` 第 4、5 節）：功能與 SQLite 相同，資料放在外部資料庫
    Postgres,
}

impl StorageBackend {
    /// 是資料庫後端（SQLite 或 PostgreSQL）：有主機清冊、彙總、主控台、註冊、告警等需要資料庫的功能
    pub fn is_database(self) -> bool {
        matches!(self, StorageBackend::Sqlite | StorageBackend::Postgres)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StorageBackend::Sqlite => "sqlite",
            StorageBackend::Jsonl => "jsonl",
            StorageBackend::Postgres => "postgres",
        }
    }
}

/// SQLite 的 `PRAGMA synchronous`（`storage.sqlite_synchronous`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SqliteSynchronous {
    /// WAL 模式下 commit 只寫入作業系統（不 fsync）：行程當掉不會遺失已 `ack` 的資料，
    /// 斷電可能遺失最後幾個交易（與 JSONL 的 flush 相同，符合 PROTOCOL.md 的 ack 語意）
    #[default]
    Normal,
    /// 每次 commit 都 fsync WAL：斷電也不遺失已 `ack` 的資料，寫入延遲較高
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct StorageSettings {
    pub data_dir: String,
    /// report 的儲存後端，預設 `sqlite`
    pub backend: StorageBackend,
    /// `backend = "sqlite"` 時是否另外輸出 JSONL 鏡像（`reports-YYYY-MM-DD.jsonl`，盡力而為，
    /// 不影響 `ack`）
    pub jsonl_mirror: bool,
    /// reports-YYYY-MM-DD.jsonl 保留天數（今天與前 N 天保留），0 = 永久保留。
    /// 只用於 JSONL（`backend = "jsonl"` 或 `jsonl_mirror = true`）
    pub retention_days: u32,
    /// 每個 reports-YYYY-MM-DD.jsonl 的大小上限（bytes），0 = 不限制。達到上限後當天的 report
    /// 不再寫入（`backend = "jsonl"` 時回 `storage_unavailable`；鏡像時只是不寫入鏡像）
    pub max_daily_bytes: u64,
    /// SQLite 原始 report 的保留天數（今天與前 N 天，UTC），0 = 不依時間刪除
    pub raw_retention_days: u32,
    /// 5 分鐘彙總的保留天數，0 = 不依時間刪除
    pub rollup_5m_retention_days: u32,
    /// 1 小時彙總的保留天數，0 = 不依時間刪除
    pub rollup_1h_retention_days: u32,
    /// SQLite 資料庫（含 WAL）的總容量上限（bytes），0 = 不限制。超過時依序刪除最舊的原始 report、
    /// 5 分鐘彙總、1 小時彙總，直到低於上限的 95%
    pub max_total_bytes: u64,
    /// SQLite 的 `PRAGMA synchronous`：`normal`（預設）或 `full`
    pub sqlite_synchronous: SqliteSynchronous,
    /// 主機清冊最多幾台主機（新主機超過時其 report 回 `storage_unavailable`）。未設定時為
    /// `max(100000, limits.max_agents)`，見 [`AppConfig::max_hosts`]
    pub max_hosts: Option<usize>,
    /// 每台主機最多幾個指標序列（含 Kairos 算出的速率與推算序列）。超過時新序列的指標不保存
    /// （`KRS-W4104`），其餘照存並回 `ack`（METRICS.md 第 9.1 節）
    pub max_series_per_host: usize,
    /// `backend = "postgres"` 時的連線設定
    pub postgres: PostgresSettings,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            data_dir: "data".to_string(),
            backend: StorageBackend::Sqlite,
            jsonl_mirror: false,
            retention_days: 30,
            max_daily_bytes: DEFAULT_MAX_DAILY_BYTES,
            raw_retention_days: DEFAULT_RAW_RETENTION_DAYS,
            rollup_5m_retention_days: DEFAULT_ROLLUP_5M_RETENTION_DAYS,
            rollup_1h_retention_days: DEFAULT_ROLLUP_1H_RETENTION_DAYS,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            sqlite_synchronous: SqliteSynchronous::Normal,
            max_hosts: None,
            max_series_per_host: DEFAULT_MAX_SERIES_PER_HOST,
            postgres: PostgresSettings::default(),
        }
    }
}

impl StorageSettings {
    /// 是否寫入 JSONL（主要輸出或鏡像）
    pub fn writes_jsonl(&self) -> bool {
        self.backend == StorageBackend::Jsonl || self.jsonl_mirror
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
    /// CRL 的下次更新時間（nextUpdate）前幾天開始記 WARN（O1）；0 = 不預警（已過期時仍記錄 ERROR）
    pub crl_expiry_warning_days: u32,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            client_ca: String::new(),
            crl: Vec::new(),
            allow_multiple_dns_sans: false,
            allow_unauthenticated: false,
            crl_expiry_warning_days: DEFAULT_CRL_EXPIRY_WARNING_DAYS,
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct LimitSettings {
    /// 狀態表（記憶體與 agents.json）最多保留幾個 agent。達上限時先淘汰離線最久的 agent；
    /// 沒有可淘汰的離線 agent 時拒絕新的 agent_id。必須不小於 `server.max_connections`，
    /// 確保線上的 agent 一定放得下。未設定時為 `max(10000, max_connections)`，
    /// 見 [`AppConfig::max_agents`]
    pub max_agents: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct LogSettings {
    pub level: String,
    /// logs/kairos-YYYY-MM-DD.log 保留天數（今天與前 N 天），0 = 永久保留
    pub retention_days: u32,
    /// `text`（預設，RFC 3339 帶時區）、`json`（一行一個 JSON，`logs/kairos-YYYY-MM-DD.jsonl`）、
    /// `legacy`（0.5 以前的格式：本地時間不帶時區，過渡用）
    pub format: String,
    /// `local`（預設）或 `utc`：`text` 的時間；`json` 一律 UTC、`legacy` 一律本地時間
    pub timezone: String,
    /// 同時寫到 stdout（以 systemd 執行時進 journal）；預設 `true`（與 0.5 相同）
    pub stdout: bool,
    /// syslog 外送（RFC 5424）
    pub syslog: SyslogSettings,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            retention_days: 14,
            format: "text".to_string(),
            timezone: "local".to_string(),
            stdout: true,
            syslog: SyslogSettings::default(),
        }
    }
}

/// `[log.syslog]`：外送到 syslog（審計 S7、O3）
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct SyslogSettings {
    /// `udp://主機:埠`（預設埠 514）、`tcp://主機:埠`（預設埠 601，RFC 6587 octet counting）、
    /// `unix:///dev/log`；空字串＝不外送
    pub target: String,
    /// `daemon`（預設）、`user`、`auth`、`authpriv`、`local0`～`local7`
    pub facility: String,
    /// RFC 5424 的 APP-NAME
    pub app_name: String,
    /// `text`（預設）或 `json`（MSG 為與 `log.format = "json"` 相同的一行 JSON）
    pub payload: String,
}

impl Default for SyslogSettings {
    fn default() -> Self {
        Self {
            target: String::new(),
            facility: "daemon".to_string(),
            app_name: "kairos".to_string(),
            payload: "text".to_string(),
        }
    }
}

/// `[observability]`：健康檢查與 Prometheus 端點（審計 O3；設計包 §24 #8）
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ObservabilitySettings {
    /// 另外以純 HTTP 監聽 `/healthz`、`/readyz`、`/metrics` 的位址（例如 `127.0.0.1:9464`）；
    /// 空字串＝不另外監聽（端點仍在主控台的監聽上）
    pub listen: String,
    /// 主控台的監聽（`console.listen`，HTTPS）也提供這三個端點
    pub on_console: bool,
    /// `/metrics` 的 bearer token 所在的檔案（內容為 token，一行）。設定時任何來源都要帶
    /// `Authorization: Bearer <token>`；未設定時 `/metrics` 只接受 loopback 來源。
    /// `/healthz`、`/readyz` 不需要認證（只回傳狀態，不含版本、路徑與數量）
    pub metrics_token_file: String,
}

impl Default for ObservabilitySettings {
    fn default() -> Self {
        Self {
            listen: String::new(),
            on_console: true,
            metrics_token_file: String::new(),
        }
    }
}

/// `observability.metrics_token_file` 的 token 最短長度
pub const MIN_METRICS_TOKEN_LEN: usize = 16;

impl ObservabilitySettings {
    /// 讀取 `/metrics` 的 token（未設定時為 `None`）
    pub fn metrics_token(&self) -> Result<Option<String>, Error> {
        let path = self.metrics_token_file.trim();
        if path.is_empty() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("無法讀取 observability.metrics_token_file {path}：{e}"),
            )
        })?;
        let token = text.trim().to_string();
        if token.len() < MIN_METRICS_TOKEN_LEN || !token.chars().all(|c| c.is_ascii_graphic()) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "observability.metrics_token_file {path} 的內容必須是至少 {MIN_METRICS_TOKEN_LEN} 個可列印 ASCII 字元的一行 token（例如 openssl rand -hex 32 的輸出）"
                ),
            ));
        }
        Ok(Some(token))
    }
}

impl LogSettings {
    /// `log.format`
    pub fn log_format(&self) -> Result<kp_obs::logfmt::LogFormat, Error> {
        kp_obs::logfmt::LogFormat::parse(&self.format).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "log.format 的值 \"{}\" 無效，可用值: text、json、legacy",
                    self.format
                ),
            )
        })
    }

    /// `log.timezone`
    pub fn log_timezone(&self) -> Result<kp_obs::logfmt::LogTimezone, Error> {
        kp_obs::logfmt::LogTimezone::parse(&self.timezone).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "log.timezone 的值 \"{}\" 無效，可用值: local、utc",
                    self.timezone
                ),
            )
        })
    }

    /// `[log.syslog]` 轉成外送設定；`target` 為空時為 `None`
    pub fn syslog_config(&self, hostname: &str) -> Result<Option<kp_obs::syslog::Config>, Error> {
        let s = &self.syslog;
        if s.target.trim().is_empty() {
            return Ok(None);
        }
        let invalid = |m: String| Error::new(ErrorKind::InvalidInput, m);
        let transport = kp_obs::syslog::parse_target(&s.target)
            .map_err(|e| invalid(format!("log.syslog.target 無效：{e}")))?;
        let facility = kp_obs::syslog::parse_facility(&s.facility).ok_or_else(|| {
            invalid(format!(
                "log.syslog.facility 的值 \"{}\" 無效，可用值: daemon、user、auth、authpriv、local0～local7",
                s.facility
            ))
        })?;
        let payload = kp_obs::syslog::Payload::parse(&s.payload).ok_or_else(|| {
            invalid(format!(
                "log.syslog.payload 的值 \"{}\" 無效，可用值: text、json",
                s.payload
            ))
        })?;
        if s.app_name.trim().is_empty() {
            return Err(invalid("log.syslog.app_name 不可為空".to_string()));
        }
        Ok(Some(kp_obs::syslog::Config {
            transport,
            facility,
            app_name: s.app_name.trim().to_string(),
            hostname: hostname.to_string(),
            payload,
        }))
    }

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
    Run {
        config_path: Option<PathBuf>,
    },
    Help,
    Version,
    /// `kairos hosts ...`（主機清冊查詢，見 [`crate::hosts_cli`]）
    Hosts(crate::hosts_cli::HostsCommand),
    /// `kairos reports ...`（匯出 SQLite 中的 report，見 [`crate::export_cli`]）
    Reports(crate::export_cli::ReportsCommand),
    /// `kairos db ...`（資料庫的線上備份，見 [`crate::export_cli`]）
    Db(crate::export_cli::DbCommand),
    /// `kairos api-token ...`（主控台 API 的 token，見 [`crate::token_cli`]）
    ApiToken(crate::token_cli::TokenCommand),
    /// `kairos enroll-token|enroll|pki|config ...`（註冊與 PKI，見 [`crate::enroll_cli`]）
    Enroll(crate::enroll_cli::Cmd),
    /// `kairos alerts|silences|alert-rules|alert-channels ...`（告警，見 [`crate::alerts_cli`]）
    Alerts(crate::alerts_cli::Cmd),
    /// `kairos users ...`（使用者、角色指派、工作階段，見 [`crate::users_cli`]）
    Users(crate::users_cli::UsersCommand),
    /// `kairos audit ...`（稽核日誌的查詢、驗證與匯出，見 [`crate::audit_cli`]）
    Audit(crate::audit_cli::AuditCommand),
    /// `kairos cluster ...`（叢集的維運命令，見 [`crate::cluster_cli`]）
    Cluster(crate::cluster_cli::ClusterCommand),
    /// `kairos diag ...`（支援診斷包，見 [`crate::diag`]）
    Diag(crate::diag::DiagArgs),
    /// `kairos --check-config`／`kairos check-config`（檢查設定後結束，見 [`crate::diag`]）
    CheckConfig(crate::diag::CheckArgs),
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
    let args: Vec<OsString> = args.into_iter().collect();
    if args.first().is_some_and(|a| a == "diag") {
        return crate::diag::parse_args(args.into_iter().skip(1)).map(CliAction::Diag);
    }
    if args.first().is_some_and(|a| a == "check-config") {
        return crate::diag::parse_check_args(args.into_iter().skip(1)).map(CliAction::CheckConfig);
    }
    if args.iter().any(|a| a == "--check-config") {
        return crate::diag::parse_check_args(args.into_iter().filter(|a| a != "--check-config"))
            .map(CliAction::CheckConfig);
    }
    let mut iter = args.into_iter().peekable();
    if iter.peek().is_some_and(|a| a == "hosts") {
        iter.next();
        return crate::hosts_cli::parse_args(iter).map(CliAction::Hosts);
    }
    if iter.peek().is_some_and(|a| a == "reports") {
        iter.next();
        return crate::export_cli::parse_args(iter).map(CliAction::Reports);
    }
    if iter.peek().is_some_and(|a| a == "db") {
        iter.next();
        return crate::export_cli::parse_db_args(iter).map(CliAction::Db);
    }
    if iter.peek().is_some_and(|a| a == "users") {
        iter.next();
        return crate::users_cli::parse_args(iter).map(CliAction::Users);
    }
    if iter.peek().is_some_and(|a| a == "audit") {
        iter.next();
        return crate::audit_cli::parse_args(iter).map(CliAction::Audit);
    }
    if iter.peek().is_some_and(|a| a == "cluster") {
        iter.next();
        return crate::cluster_cli::parse_args(iter).map(CliAction::Cluster);
    }
    if iter.peek().is_some_and(|a| a == "api-token") {
        iter.next();
        return crate::token_cli::parse_args(iter).map(CliAction::ApiToken);
    }
    for (name, group) in [
        ("enroll-token", crate::enroll_cli::Group::EnrollToken),
        ("enroll", crate::enroll_cli::Group::Enroll),
        ("pki", crate::enroll_cli::Group::Pki),
        ("config", crate::enroll_cli::Group::Config),
    ] {
        if iter.peek().is_some_and(|a| a == name) {
            iter.next();
            return crate::enroll_cli::parse_args(group, iter).map(CliAction::Enroll);
        }
    }
    for (name, group) in [
        ("alerts", crate::alerts_cli::Group::Alerts),
        ("silences", crate::alerts_cli::Group::Silences),
        ("alert-rules", crate::alerts_cli::Group::Rules),
        ("alert-channels", crate::alerts_cli::Group::Channels),
    ] {
        if iter.peek().is_some_and(|a| a == name) {
            iter.next();
            return crate::alerts_cli::parse_args(group, iter).map(CliAction::Alerts);
        }
    }
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

/// 解析 `--config <path>`／`--config=<path>`（子命令共用）：`arg` 是這個選項時回傳路徑，
/// 不是時回傳 `Ok(None)`
pub fn take_config_arg(
    arg: &OsStr,
    rest: &mut impl Iterator<Item = OsString>,
) -> Result<Option<PathBuf>, String> {
    let path = if arg == "--config" {
        match rest.next() {
            Some(p) if !p.is_empty() => p,
            _ => return Err("--config 需要指定設定檔路徑".to_string()),
        }
    } else if let Some(p) = strip_config_eq(arg) {
        let p = p?;
        if p.is_empty() {
            return Err("--config 需要指定設定檔路徑".to_string());
        }
        p
    } else {
        return Ok(None);
    };
    Ok(Some(PathBuf::from(path)))
}

/// 字串清單：接受陣列，或以 [`LIST_SEPARATOR`] 分隔的單一字串
pub(crate) fn string_or_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
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

    /// 解析後的 `server.per_ip_allowlist`
    pub fn per_ip_allowlist(&self) -> Result<crate::per_ip::Allowlist, Error> {
        crate::per_ip::Allowlist::parse(&self.server.per_ip_allowlist)
            .map_err(|e| Error::new(ErrorKind::InvalidInput, e))
    }

    /// `server.max_connections_per_allowlisted_ip` 的設定值（未設定時為內建預設 4096）
    pub fn configured_max_connections_per_allowlisted_ip(&self) -> usize {
        self.server
            .max_connections_per_allowlisted_ip
            .unwrap_or(DEFAULT_MAX_CONNECTIONS_PER_ALLOWLISTED_IP)
    }

    /// 允許清單內每個位址的實際上限：`min(server.max_connections_per_allowlisted_ip, server.max_connections)`
    /// （同一個位址的連線也算在全部連線之內；D15）
    pub fn max_connections_per_allowlisted_ip(&self) -> usize {
        self.configured_max_connections_per_allowlisted_ip()
            .min(self.server.max_connections)
    }

    /// 設計包 `KRS-W0203` 的情境：`server.per_ip_allowlist` 非空，且
    /// `server.max_connections_per_allowlisted_ip`（含內建預設）大於 `server.max_connections` 時，
    /// 回傳啟動時要記錄一次的警告訊息。清單為空時這個鍵不生效，不檢查也不警告（D22）
    pub fn allowlisted_limit_warning(&self) -> Option<String> {
        let configured = self.configured_max_connections_per_allowlisted_ip();
        let max = self.server.max_connections;
        let has_entries = self
            .server
            .per_ip_allowlist
            .iter()
            .any(|e| !e.trim().is_empty());
        if !has_entries || configured <= max {
            return None;
        }
        let source = if self.server.max_connections_per_allowlisted_ip.is_some() {
            ""
        } else {
            "（內建預設）"
        };
        Some(format!(
            "允許清單內每位址上限（{configured}）大於同時連線上限（{max}），實際只會生效 {max}：server.max_connections_per_allowlisted_ip = {configured}{source} 大於 server.max_connections = {max}，同一個位址的連線也算在全部連線之內，server.per_ip_allowlist 內每個位址最多 {max} 條連線。單一 NAT 出口後的主機不超過 {max} 台時不需處理（也可把 server.max_connections_per_allowlisted_ip 改為 {max} 以下）；需要更多連線時調高 server.max_connections（需重新啟動），並把檔案描述元上限調到大於 server.max_connections + 64（Linux：systemd unit 的 LimitNOFILE，手動執行時 ulimit -n）"
        ))
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

    /// 主機清冊的筆數上限：明確設定的值，或 `max(100000, limits.max_agents)`
    pub fn max_hosts(&self) -> usize {
        self.storage
            .max_hosts
            .unwrap_or(DEFAULT_MAX_HOSTS.max(self.max_agents()))
    }

    /// 以 `hosts` 台主機、每 5 秒一筆 report 估算 SQLite 在保留期限內需要的容量（bytes；
    /// 公式見 `kairos/STORAGE.md`「容量估算」）。保留天數為 0（不依時間刪除）時以 365 天估算
    pub fn estimated_sqlite_bytes(&self, hosts: u64) -> u64 {
        hosts.saturating_mul(
            self.estimated_sqlite_bytes_per_host(ESTIMATE_REPORT_INTERVAL_SECS * 1000),
        )
    }

    /// 一台每 `report_interval_ms` 一筆 report 的主機在保留期限內需要的容量（bytes）：
    /// 系統健康以目前回報中的主機與各自宣告的間隔加總（設計包 §24 #19、§25.5 #4）
    pub fn estimated_sqlite_bytes_per_host(&self, report_interval_ms: u64) -> u64 {
        let days = |d: u32| u64::from(if d == 0 { 365 } else { d + 1 });
        let reports_per_day = 86_400_000 / report_interval_ms.max(1000);
        let raw = reports_per_day * ESTIMATED_RAW_ROW_BYTES * days(self.storage.raw_retention_days);
        let rollup = |bucket_secs: u64, d: u32| {
            (86_400 / bucket_secs)
                * ESTIMATED_SERIES_PER_HOST
                * ESTIMATED_ROLLUP_ROW_BYTES
                * days(d)
        };
        raw + ESTIMATED_PROCESS_BYTES_PER_DAY * days(self.storage.raw_retention_days)
            + rollup(300, self.storage.rollup_5m_retention_days)
            + rollup(3600, self.storage.rollup_1h_retention_days)
    }

    /// SQLite 時 `storage.max_total_bytes` 小於以 `max_connections` 台主機估算的容量時回傳
    /// (上限, 估算量)，供啟動時警告（達到上限後會提早刪除最舊的資料）
    pub fn total_limit_below_estimate(&self) -> Option<(u64, u64)> {
        let max = self.storage.max_total_bytes;
        let estimate = self.estimated_sqlite_bytes(self.server.max_connections as u64);
        (self.storage.backend.is_database() && max > 0 && estimate > max).then_some((max, estimate))
    }

    /// 到期預警的門檻（O1）
    pub fn expiry_policy(&self) -> crate::expiry::ExpiryPolicy {
        crate::expiry::ExpiryPolicy {
            cert_warning_days: self.server.cert_expiry_warning_days,
            crl_warning_days: self.auth.crl_expiry_warning_days,
        }
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

        let mut settings: AppConfig = Config::builder()
            .add_source(file_source.clone())
            .add_source(environment())
            .build()
            .and_then(|c| c.try_deserialize())
            .map_err(|e| {
                Error::new(
                    ErrorKind::InvalidData,
                    format!("載入設定失敗（{} / KAIROS__ 環境變數）: {}", source_name, e),
                )
            })?;
        // 每個鍵的來源：只讀設定檔一次（上面已確認能解析），攤平成鍵的集合
        let mut file_keys = BTreeSet::new();
        let mut file_values = std::collections::BTreeMap::new();
        if let Ok(v) = Config::builder()
            .add_source(file_source)
            .build()
            .and_then(|c| c.try_deserialize::<serde_json::Value>())
        {
            flatten_keys("", &v, &mut file_keys);
            flatten_file_values("", &v, &mut file_values);
        }
        settings.provenance = Provenance {
            file_keys,
            env_keys: Provenance::env_keys_from_process(),
            file_values,
        };

        settings.validate()?;
        // 這個行程使用的資料庫後端（`storage.backend = "postgres"` 時建立連線池，不連線）。單元測試中
        // 不設定：測試各自載入不同的設定，行程層級的後端會互相干擾
        #[cfg(not(test))]
        crate::store::postgres::pool::configure(&settings);
        // 叢集模式：這個行程（服務與所有子命令）不自動建立金鑰檔（kairos/HA.md 第 11.1 節）
        #[cfg(not(test))]
        if settings.cluster.enabled {
            crate::cluster::keys::disable_autocreate();
        }
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
        if let Some(per_allowlisted) = self.server.max_connections_per_allowlisted_ip
            && !(1..=MAX_MAX_CONNECTIONS_PER_ALLOWLISTED_IP).contains(&per_allowlisted)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "server.max_connections_per_allowlisted_ip 必須介於 1..={}（目前為 {}）",
                    MAX_MAX_CONNECTIONS_PER_ALLOWLISTED_IP, per_allowlisted
                ),
            ));
        }
        self.per_ip_allowlist()?;
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
        for (name, value) in [
            (
                "server.cert_expiry_warning_days",
                self.server.cert_expiry_warning_days,
            ),
            (
                "auth.crl_expiry_warning_days",
                self.auth.crl_expiry_warning_days,
            ),
        ] {
            if value > MAX_EXPIRY_WARNING_DAYS {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "{} 必須介於 0..={}（目前為 {}）",
                        name, MAX_EXPIRY_WARNING_DAYS, value
                    ),
                ));
            }
        }
        if !self.auth.crl_paths().is_empty() && self.auth.client_ca_path().is_none() {
            return invalid("auth.crl 只在設定 auth.client_ca（啟用 mTLS）時有效");
        }
        for (name, value) in [
            ("storage.retention_days", self.storage.retention_days),
            (
                "storage.raw_retention_days",
                self.storage.raw_retention_days,
            ),
            (
                "storage.rollup_5m_retention_days",
                self.storage.rollup_5m_retention_days,
            ),
            (
                "storage.rollup_1h_retention_days",
                self.storage.rollup_1h_retention_days,
            ),
        ] {
            if value > MAX_RETENTION_DAYS {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "{} 必須介於 0..={}（目前為 {}）",
                        name, MAX_RETENTION_DAYS, value
                    ),
                ));
            }
        }
        let total = self.storage.max_total_bytes;
        if total != 0 && total < MIN_MAX_TOTAL_BYTES {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "storage.max_total_bytes 必須為 0（不限制）或至少 {}（64 MiB）（目前為 {}）",
                    MIN_MAX_TOTAL_BYTES, total
                ),
            ));
        }
        if let Some(max_hosts) = self.storage.max_hosts
            && !(self.max_agents()..=MAX_MAX_HOSTS).contains(&max_hosts)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "storage.max_hosts 必須介於 limits.max_agents（{}）..={}（目前為 {}）",
                    self.max_agents(),
                    MAX_MAX_HOSTS,
                    max_hosts
                ),
            ));
        }
        if !(MIN_MAX_SERIES_PER_HOST..=MAX_MAX_SERIES_PER_HOST)
            .contains(&self.storage.max_series_per_host)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "storage.max_series_per_host 必須介於 {}..={}（目前為 {}）",
                    MIN_MAX_SERIES_PER_HOST,
                    MAX_MAX_SERIES_PER_HOST,
                    self.storage.max_series_per_host
                ),
            ));
        }
        if self.storage.jsonl_mirror && self.storage.backend == StorageBackend::Jsonl {
            return invalid(
                "storage.jsonl_mirror 只在 storage.backend = \"sqlite\" 或 \"postgres\" 時有效（backend = \"jsonl\" 時 JSONL 已是主要輸出）",
            );
        }
        if self.storage.backend == StorageBackend::Postgres {
            self.storage.postgres.validate()?;
        }
        self.validate_console()?;
        self.enroll.validate()?;
        self.pki.validate()?;
        self.alerts.validate()?;
        self.accounts.validate()?;
        self.ldap.validate()?;
        self.audit.validate()?;
        self.export.validate()?;
        self.cluster.validate()?;
        // 叢集模式只支援 PostgreSQL，且不支援 JSONL 鏡像（kairos/HA.md 第 15.1 節、第 21 節 #17）
        if self.cluster.enabled && self.storage.backend != StorageBackend::Postgres {
            return invalid(
                "錯誤 KRS-E0202：設定互相矛盾：cluster.enabled = true 需要 storage.backend = \"postgres\"（叢集的節點共用一個 PostgreSQL；SQLite 與 JSONL 只支援單機）",
            );
        }
        if self.cluster.enabled && self.storage.jsonl_mirror {
            return invalid(
                "錯誤 KRS-E0202：設定互相矛盾：叢集模式（cluster.enabled = true）不支援 storage.jsonl_mirror = true（JSONL 鏡像是節點本機的檔案）",
            );
        }
        self.remote.validate()?;
        self.validate_listen_conflicts()?;
        self.log.level_filter()?;
        self.log.log_format()?;
        self.log.log_timezone()?;
        self.log.syslog_config("-")?;
        self.validate_observability()?;
        Ok(())
    }

    /// `[observability]` 的檢查：位址的格式、token 檔可讀且格式正確（主機名稱在啟動時解析）
    fn validate_observability(&self) -> Result<(), Error> {
        let o = &self.observability;
        let listen = o.listen.trim();
        if !listen.is_empty() && !listen.contains(':') {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "observability.listen 應為 位址:埠（例如 127.0.0.1:9464），目前為 {listen}"
                ),
            ));
        }
        o.metrics_token()?;
        Ok(())
    }

    /// 三個監聽位址（agent、主控台、註冊端點）不可相同（`KRS-E0202`，ENROLL.md 第 19.1 節第 1 項）。
    /// 主機名稱形式的位址在啟動時解析後再比較一次
    fn validate_listen_conflicts(&self) -> Result<(), Error> {
        let mut named: Vec<(&str, String)> = vec![("server.listen", self.server.listen.clone())];
        if self.console.enabled {
            named.push(("console.listen", self.console.listen.clone()));
        }
        named.push((
            "enroll.listen",
            self.enroll.listen_addr(&self.server.listen),
        ));
        if !self.observability.listen.trim().is_empty() {
            named.push(("observability.listen", self.observability.listen.clone()));
        }
        for i in 0..named.len() {
            for j in i + 1..named.len() {
                if let (Ok(a), Ok(b)) = (
                    named[i].1.trim().parse::<SocketAddr>(),
                    named[j].1.trim().parse::<SocketAddr>(),
                ) && listen_addrs_conflict(&a, &b)
                {
                    return Err(listen_conflict_between(
                        named[i].0,
                        &named[i].1,
                        named[j].0,
                        &named[j].1,
                    ));
                }
            }
        }
        Ok(())
    }

    /// 8444 是否可能監聽（模式可在執行中切換，與目前的模式無關）：需要 SQLite、mTLS 與 `pki.server_ca_path`
    pub fn enroll_possible(&self) -> bool {
        self.storage.backend.is_database()
            && self.auth.client_ca_path().is_some()
            && self.pki.server_ca().is_some()
    }

    /// `[console]` 的檢查，以及監聽位址不可相同（設計包 §24 #16 (a)、`KRS-E0202`）。
    /// 主機名稱形式的位址在啟動時解析後再以 [`listen_addrs_conflict`] 比較一次
    fn validate_console(&self) -> Result<(), Error> {
        let c = &self.console;
        let invalid = |msg: String| Err(Error::new(ErrorKind::InvalidInput, msg));
        if !c.enabled {
            return Ok(());
        }
        if c.listen.trim().is_empty() {
            return invalid("console.listen 不可為空".to_string());
        }
        if !(1..=MAX_CONSOLE_MAX_CONNECTIONS).contains(&c.max_connections) {
            return invalid(format!(
                "console.max_connections 必須介於 1..={}（目前為 {}）",
                MAX_CONSOLE_MAX_CONNECTIONS, c.max_connections
            ));
        }
        if !(1..=c.max_connections).contains(&c.max_streams) {
            return invalid(format!(
                "console.max_streams 必須介於 1..=console.max_connections（{}）（目前為 {}）",
                c.max_connections, c.max_streams
            ));
        }
        if c.cert_path.trim().is_empty() != c.key_path.trim().is_empty() {
            return invalid(
                "console.cert_path 與 console.key_path 必須同時設定或同時留空（留空時沿用 server.cert_path／key_path）"
                    .to_string(),
            );
        }
        if let (Ok(agent), Ok(console)) = (
            self.server.listen.trim().parse::<SocketAddr>(),
            c.listen.trim().parse::<SocketAddr>(),
        ) && listen_addrs_conflict(&agent, &console)
        {
            return Err(listen_conflict_error(&self.server.listen, &c.listen));
        }
        Ok(())
    }

    /// 檔案描述元的需要量（設計包 §24 #18 (b)(c)、D21）：agent 連線上限＋主控台連線上限＋預留。
    /// 啟動檢查與系統健康共用這個算式
    pub fn fd_needed(&self) -> u64 {
        self.server.max_connections as u64 + self.extra_connections() + FD_RESERVE
    }

    /// agent 連線埠以外的連線上限：主控台（啟用時）＋註冊端點（可能監聽時，D21）
    pub fn extra_connections(&self) -> u64 {
        let console = if self.console.enabled {
            self.console.max_connections as u64
        } else {
            0
        };
        let enroll = if self.enroll_possible() {
            self.enroll.max_connections as u64
        } else {
            0
        };
        console + enroll
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
        assert_eq!(c.server.cert_expiry_warning_days, 30);
        assert_eq!(c.auth.crl_expiry_warning_days, 7);
        assert_eq!(c.timeouts.handshake_secs, 5);
        assert_eq!(c.timeouts.hello_secs, 5);
        assert_eq!(c.timeouts.write_secs, 5);
        assert_eq!(c.storage.data_dir, "data");
        assert_eq!(c.storage.retention_days, 30);
        assert_eq!(c.storage.max_daily_bytes, 0);
        assert_eq!(c.daily_limit_below_estimate(), None);
        assert_eq!(c.storage.backend, StorageBackend::Sqlite);
        assert!(!c.storage.jsonl_mirror);
        assert!(!c.storage.writes_jsonl());
        assert_eq!(c.storage.raw_retention_days, 7);
        assert_eq!(c.storage.rollup_5m_retention_days, 31);
        assert_eq!(c.storage.rollup_1h_retention_days, 400);
        assert_eq!(c.storage.max_total_bytes, 64 * 1024 * 1024 * 1024);
        assert_eq!(c.storage.sqlite_synchronous, SqliteSynchronous::Normal);
        assert_eq!(c.max_hosts(), 100_000);
        assert_eq!(c.server.reload_interval_secs, 30);
        assert_eq!(c.log.retention_days, 14);
        assert!(c.auth.crl.is_empty());
        assert_eq!(c.auth.client_ca, "");
        assert_eq!(c.auth.client_ca_path(), None);
        assert_eq!(c.log.level, "info");
        assert!(c.console.enabled);
        assert_eq!(c.console.listen, "127.0.0.1:9443");
        assert_eq!(c.console.max_connections, 64);
        assert_eq!(c.console.max_streams, 16);
        assert_eq!(
            c.console.tls_files(&c.server),
            ("cert.pem".to_string(), "key.pem".to_string())
        );
        // 1024 + 64 + 64
        assert_eq!(c.fd_needed(), 1152);
        assert_eq!(suggested_nofile(c.fd_needed()), 4096);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn console_settings_are_validated() {
        // 預設值合法；停用時不檢查其他欄位
        let mut c = AppConfig::default();
        c.console.enabled = false;
        c.console.listen = String::new();
        assert!(c.validate().is_ok());
        assert_eq!(c.fd_needed(), 1024 + 64);

        let mutations: [fn(&mut AppConfig); 6] = [
            |c| c.console.listen = " ".into(),
            |c| c.console.max_connections = 0,
            |c| c.console.max_connections = MAX_CONSOLE_MAX_CONNECTIONS + 1,
            |c| c.console.max_streams = 0,
            |c| c.console.max_streams = c.console.max_connections + 1,
            |c| c.console.cert_path = "console.pem".into(),
        ];
        for mutate in mutations {
            let mut c = AppConfig::default();
            mutate(&mut c);
            assert!(c.validate().is_err(), "{:?}", c.console);
        }
        // 另設憑證時兩者都要有
        let mut c = AppConfig::default();
        c.console.cert_path = "console.pem".into();
        c.console.key_path = "console.key".into();
        assert!(c.validate().is_ok());
        assert_eq!(
            c.console.tls_files(&c.server),
            ("console.pem".to_string(), "console.key".to_string())
        );

        // 環境變數
        let c = from_env(&[
            ("KAIROS__CONSOLE__LISTEN", "0.0.0.0:9443"),
            ("KAIROS__CONSOLE__MAX_CONNECTIONS", "128"),
            ("KAIROS__CONSOLE__ENABLED", "false"),
        ])
        .unwrap();
        assert_eq!(c.console.listen, "0.0.0.0:9443");
        assert_eq!(c.console.max_connections, 128);
        assert!(!c.console.enabled);
    }

    #[test]
    fn listen_addresses_must_not_collide() {
        // KRS-E0202：IP:port 相同，或其中一個是所有介面而連接埠相同
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert!(listen_addrs_conflict(
            &a("127.0.0.1:9443"),
            &a("127.0.0.1:9443")
        ));
        assert!(listen_addrs_conflict(
            &a("0.0.0.0:9443"),
            &a("127.0.0.1:9443")
        ));
        assert!(listen_addrs_conflict(&a("[::]:9443"), &a("10.0.0.5:9443")));
        assert!(listen_addrs_conflict(
            &a("[::ffff:127.0.0.1]:9443"),
            &a("127.0.0.1:9443")
        ));
        assert!(!listen_addrs_conflict(
            &a("127.0.0.1:8443"),
            &a("127.0.0.1:9443")
        ));
        assert!(!listen_addrs_conflict(
            &a("127.0.0.1:9443"),
            &a("127.0.0.2:9443")
        ));
        // 連接埠 0（由作業系統分配）不會衝突
        assert!(!listen_addrs_conflict(&a("127.0.0.1:0"), &a("127.0.0.1:0")));

        let mut c = AppConfig::default();
        c.server.listen = "0.0.0.0:9443".into();
        c.auth.client_ca = "ca.pem".into();
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("KRS-E0202"), "{err}");
        assert!(err.to_string().contains("console.listen"), "{err}");
        // 停用主控台時不檢查
        c.console.enabled = false;
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

        let mut ok = AppConfig::default();
        ok.server.cert_expiry_warning_days = MAX_EXPIRY_WARNING_DAYS;
        ok.auth.crl_expiry_warning_days = 0;
        assert!(ok.validate().is_ok());

        let mutations: [fn(&mut AppConfig); 11] = [
            |c| c.server.cert_expiry_warning_days = MAX_EXPIRY_WARNING_DAYS + 1,
            |c| c.auth.crl_expiry_warning_days = MAX_EXPIRY_WARNING_DAYS + 1,
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
            ("KAIROS__AUTH__CRL_EXPIRY_WARNING_DAYS", "14"),
            ("KAIROS__SERVER__CERT_EXPIRY_WARNING_DAYS", "60"),
        ])
        .unwrap();
        assert!(c.auth.allow_unauthenticated);
        assert_eq!(c.auth.crl_expiry_warning_days, 14);
        assert_eq!(c.server.cert_expiry_warning_days, 60);
        assert_eq!(
            c.expiry_policy(),
            crate::expiry::ExpiryPolicy {
                cert_warning_days: 60,
                crl_warning_days: 14
            }
        );
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
    fn storage_backend_settings() {
        let c = from_env(&[
            ("KAIROS__STORAGE__BACKEND", "jsonl"),
            ("KAIROS__STORAGE__RAW_RETENTION_DAYS", "3"),
            ("KAIROS__STORAGE__MAX_TOTAL_BYTES", "0"),
            ("KAIROS__STORAGE__SQLITE_SYNCHRONOUS", "full"),
            ("KAIROS__STORAGE__MAX_HOSTS", "20000"),
        ])
        .unwrap();
        assert_eq!(c.storage.backend, StorageBackend::Jsonl);
        assert!(c.storage.writes_jsonl());
        assert_eq!(c.storage.raw_retention_days, 3);
        assert_eq!(c.storage.max_total_bytes, 0);
        assert_eq!(c.storage.sqlite_synchronous, SqliteSynchronous::Full);
        assert_eq!(c.max_hosts(), 20000);
        assert!(c.validate().is_ok());
        // 不認得的值是錯誤
        assert!(from_env(&[("KAIROS__STORAGE__BACKEND", "mysql")]).is_err());
        // PostgreSQL 後端（0.10.0）：連線設定在 [storage.postgres]，只在選用時檢查
        let c = from_env(&[
            ("KAIROS__STORAGE__BACKEND", "postgres"),
            ("KAIROS__STORAGE__POSTGRES__HOST", "db.example.com"),
            ("KAIROS__STORAGE__POSTGRES__SCHEMA", "kairos_a"),
        ])
        .unwrap();
        assert_eq!(c.storage.backend, StorageBackend::Postgres);
        assert!(c.storage.backend.is_database() && !c.storage.writes_jsonl());
        assert_eq!(c.storage.postgres.host, "db.example.com");
        assert_eq!(
            c.storage.postgres.effective_sslmode(),
            PgSslMode::VerifyFull
        );
        assert!(
            c.validate().is_ok() && c.enroll_possible() == AppConfig::default().enroll_possible()
        );
        let bad = from_env(&[
            ("KAIROS__STORAGE__BACKEND", "postgres"),
            ("KAIROS__STORAGE__POSTGRES__SCHEMA", "Bad-Name"),
        ])
        .unwrap();
        let e = bad.validate().unwrap_err().to_string();
        assert!(e.contains("storage.postgres.schema"), "{e}");
        // 沒有選用 PostgreSQL 時不檢查 [storage.postgres]
        let unused = from_env(&[("KAIROS__STORAGE__POSTGRES__SCHEMA", "Bad-Name")]).unwrap();
        assert!(unused.validate().is_ok());
        assert!(from_env(&[("KAIROS__STORAGE__SQLITE_SYNCHRONOUS", "off")]).is_err());

        let c: AppConfig = Config::builder()
            .add_source(File::from_str(
                "[storage]\njsonl_mirror = true\nrollup_1h_retention_days = 0\n",
                FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();
        assert!(c.storage.jsonl_mirror && c.storage.writes_jsonl());
        assert!(c.validate().is_ok());

        let mutations: [fn(&mut AppConfig); 6] = [
            |c| c.storage.raw_retention_days = MAX_RETENTION_DAYS + 1,
            |c| c.storage.rollup_5m_retention_days = MAX_RETENTION_DAYS + 1,
            |c| c.storage.rollup_1h_retention_days = MAX_RETENTION_DAYS + 1,
            |c| c.storage.max_total_bytes = MIN_MAX_TOTAL_BYTES - 1,
            |c| c.storage.max_hosts = Some(c.max_agents() - 1),
            |c| {
                c.storage.backend = StorageBackend::Jsonl;
                c.storage.jsonl_mirror = true;
            },
        ];
        for mutate in mutations {
            let mut c = AppConfig::default();
            mutate(&mut c);
            assert!(c.validate().is_err(), "{:?}", c.storage);
        }
        // 上限與下限本身合法
        let mut c = AppConfig::default();
        c.storage.max_total_bytes = MIN_MAX_TOTAL_BYTES;
        c.storage.max_hosts = Some(MAX_MAX_HOSTS);
        assert!(c.validate().is_ok());
        // 未設定 max_hosts 時至少為 limits.max_agents
        c.storage.max_hosts = None;
        c.server.max_connections = 65535;
        c.limits.max_agents = Some(MAX_MAX_AGENTS);
        assert_eq!(c.max_hosts(), 100_000);
    }

    #[test]
    fn sqlite_estimate_follows_the_documented_formula() {
        // STORAGE.md「容量估算」：每台每天 原始 17280 × 530 × (7+1) 天＋行程快照 1440 × 1000 × (7+1) 天
        // + 5 分鐘 288 × 47 × 50 × (31+1) 天 + 1 小時 24 × 47 × 50 × (400+1) 天
        let c = AppConfig::default();
        let processes = 1440 * 1000 * 8;
        let per_host = 17280 * 530 * 8 + processes + 288 * 47 * 50 * 32 + 24 * 47 * 50 * 401;
        assert_eq!(c.estimated_sqlite_bytes(1), per_host);
        assert_eq!(c.estimated_sqlite_bytes(1024), 1024 * per_host);
        // 依宣告的 report 間隔：每 60 秒一筆時原始資料少 12 倍，彙總不變
        assert_eq!(c.estimated_sqlite_bytes_per_host(5000), per_host);
        assert_eq!(
            c.estimated_sqlite_bytes_per_host(60_000),
            1440 * 530 * 8 + processes + 288 * 47 * 50 * 32 + 24 * 47 * 50 * 401
        );
        // 1 秒以下以 1 秒計
        assert_eq!(
            c.estimated_sqlite_bytes_per_host(10),
            c.estimated_sqlite_bytes_per_host(1000)
        );
        // 0.8.0 起每台約 129 MB（METRICS.md 第 9.6 節）：預設 1024 台約 132 GB，超過預設上限 64 GiB，
        // 啟動時警告（D1：不改預設上限，由管理者依規模調整）；500 台以下不警告
        assert_eq!(
            c.total_limit_below_estimate(),
            Some((DEFAULT_MAX_TOTAL_BYTES, 1024 * per_host))
        );
        let mut small = AppConfig::default();
        small.server.max_connections = 500;
        assert_eq!(small.total_limit_below_estimate(), None);
        let mut big = AppConfig::default();
        big.server.max_connections = 10000;
        let (max, estimate) = big.total_limit_below_estimate().unwrap();
        assert_eq!(max, DEFAULT_MAX_TOTAL_BYTES);
        assert_eq!(estimate, 10000 * per_host);
        big.storage.max_total_bytes = 0;
        assert_eq!(big.total_limit_below_estimate(), None);
        big.storage.max_total_bytes = DEFAULT_MAX_TOTAL_BYTES;
        big.storage.backend = StorageBackend::Jsonl;
        assert_eq!(big.total_limit_below_estimate(), None);
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

    #[test]
    fn per_ip_allowlist_settings() {
        // 預設：清單空、允許清單上限 4096（D14），實際上限 min(4096, max_connections)（D15）
        let c = AppConfig::default();
        assert!(c.server.per_ip_allowlist.is_empty());
        assert_eq!(c.server.max_connections_per_allowlisted_ip, None);
        assert_eq!(c.configured_max_connections_per_allowlisted_ip(), 4096);
        assert_eq!(c.max_connections_per_allowlisted_ip(), 1024);
        assert!(c.per_ip_allowlist().unwrap().is_empty());

        // 設定檔：陣列；環境變數：逗號分隔
        let c: AppConfig = Config::builder()
            .add_source(File::from_str(
                "[server]\nmax_connections = 8000\nper_ip_allowlist = [\"203.0.113.10\", \"10.20.0.0/16\", \"2001:db8:100::/48\"]\nmax_connections_per_allowlisted_ip = 5000\n",
                FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();
        assert!(c.validate().is_ok());
        assert_eq!(c.max_connections_per_allowlisted_ip(), 5000);
        let list = c.per_ip_allowlist().unwrap();
        assert_eq!(
            list.to_string(),
            "203.0.113.10/32、10.20.0.0/16、2001:db8:100::/48"
        );
        assert!(list.contains("::ffff:10.20.3.4".parse().unwrap()));

        let c = from_env(&[
            (
                "KAIROS__SERVER__PER_IP_ALLOWLIST",
                "198.51.100.0/24,2001:db8::/32",
            ),
            ("KAIROS__SERVER__MAX_CONNECTIONS_PER_ALLOWLISTED_IP", "600"),
        ])
        .unwrap();
        assert_eq!(
            c.server.per_ip_allowlist,
            ["198.51.100.0/24", "2001:db8::/32"]
        );
        assert_eq!(c.max_connections_per_allowlisted_ip(), 600);
        assert!(c.validate().is_ok());
        // 空字串表示空清單
        let c = from_env(&[("KAIROS__SERVER__PER_IP_ALLOWLIST", "")]).unwrap();
        assert!(c.validate().is_ok());
        assert!(c.per_ip_allowlist().unwrap().is_empty());
    }

    #[test]
    fn per_ip_allowlist_validation() {
        let mut c = AppConfig::default();
        // 範圍 1..=65535，與 server.max_connections 無關（較大時只警告）
        for (value, ok) in [(0, false), (1, true), (65535, true), (65536, false)] {
            c.server.max_connections_per_allowlisted_ip = Some(value);
            assert_eq!(c.validate().is_ok(), ok, "{value}");
        }
        c.server.max_connections_per_allowlisted_ip = None;
        // 錯誤的網段拒絕啟動，訊息指出第幾項與原因
        c.server.per_ip_allowlist = vec!["10.0.0.0/8".into(), "10.0.0.1/8".into()];
        let err = c.validate().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(err.to_string().contains("第 2 項 \"10.0.0.1/8\""), "{err}");
        assert!(err.to_string().contains("10.0.0.0/8"), "{err}");
        for bad in [
            "10.0.0.0/33",
            "nat-gw",
            "fe80::1%eth0",
            "::/129",
            "10.0.0.0/+8",
        ] {
            c.server.per_ip_allowlist = vec![bad.into()];
            assert!(c.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn allowlisted_limit_warning_only_when_the_list_has_entries() {
        // D22：清單為空時不檢查、不警告（即使內建預設 4096 > 1024）
        let mut c = AppConfig::default();
        assert_eq!(c.allowlisted_limit_warning(), None);
        c.server.per_ip_allowlist = vec!["  ".into()];
        assert_eq!(c.allowlisted_limit_warning(), None);
        c.server.max_connections_per_allowlisted_ip = Some(60000);
        assert_eq!(c.allowlisted_limit_warning(), None);

        // D15：清單非空且設定值（含內建預設）大於 server.max_connections 時警告（KRS-W0203）
        c.server.max_connections_per_allowlisted_ip = None;
        c.server.per_ip_allowlist = vec!["203.0.113.10".into()];
        let w = c.allowlisted_limit_warning().unwrap();
        assert!(
            w.starts_with(
                "允許清單內每位址上限（4096）大於同時連線上限（1024），實際只會生效 1024"
            ),
            "{w}"
        );
        assert!(w.contains("4096（內建預設）"), "{w}");
        assert!(w.contains("LimitNOFILE"), "{w}");
        c.server.max_connections_per_allowlisted_ip = Some(2000);
        let w = c.allowlisted_limit_warning().unwrap();
        assert!(w.contains("= 2000 大於"), "{w}");
        // 等於或小於時不警告（比較為 >，D18）
        c.server.max_connections_per_allowlisted_ip = Some(1024);
        assert_eq!(c.allowlisted_limit_warning(), None);
        c.server.max_connections_per_allowlisted_ip = None;
        c.server.max_connections = 4096;
        assert_eq!(c.allowlisted_limit_warning(), None);
        assert_eq!(c.max_connections_per_allowlisted_ip(), 4096);
        c.server.max_connections = 2400;
        assert!(c.allowlisted_limit_warning().is_some());
        assert_eq!(c.max_connections_per_allowlisted_ip(), 2400);
    }
}
