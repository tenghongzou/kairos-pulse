//! 註冊通道（8444，`[enroll]`）與 PKI（`[pki]`、`[pki.policy]`）的設定。
//!
//! 鍵名與預設值依 `crates/kairos-proto/ENROLL.md` 第 19.1 節、設計包 `decisions/INTEGRATION.md` §2.1～§2.3
//! 與 `kairos/PKI.md` 第 8 節。
//!
//! - **雙層預設**：沒有 `[enroll]`（或沒有 `mode`）時為 `closed`，啟動時記錄一次 `KRS-I2010`；
//!   沒有 `[pki]`（或沒有 `mode`）時為離線簽發（升級不會自動開始簽發）。
//! - 主控台可修改的項目（`enroll.mode`、`enroll.auto_pause_*`、`pki.policy.*`）另有資料庫的覆寫層，
//!   優先順序為環境變數 ＞ `settings` 表 ＞ `kairos.toml` ＞ 內建預設（[`crate::settings`]）。

use kairos_proto::enroll::{DEFAULT_ENROLL_PORT, EnrollMode, MAX_BODY_BYTES};
use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;

use super::string_or_list;

/// 簽發模式（`pki.mode`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PkiMode {
    /// Kairos 持有代理簽發 CA 的私鑰，核准後立即簽發（`kairos/PKI.md` 第 3 節）
    Online,
    /// Kairos 沒有任何 CA 私鑰；核准後匯出 CSR 佇列、在 CA 主機簽發、再匯入（第 6 節）
    Offline,
}

impl PkiMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PkiMode::Online => "online",
            PkiMode::Offline => "offline",
        }
    }
}

/// `[enroll]`：註冊通道（8444）
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct EnrollSettings {
    /// `open`／`renew_only`／`closed`；未設定時為 `closed`（雙層預設，`KRS-I2010`）
    pub mode: Option<EnrollMode>,
    /// 開放模式下，連續 `auto_pause_after_hours` 小時沒有有效 token 時自動暫停新註冊
    pub auto_pause_enabled: bool,
    /// 1～168
    pub auto_pause_after_hours: u32,
    /// 監聽位址；空字串＝與 `server.listen` 同一個 IP、連接埠 8444
    pub listen: String,
    /// 對外公告的註冊端點（`host:port`；安裝字串的 `e` 與簽章內容的名稱之一）；空字串時不另外公告
    pub advertise: String,
    /// 8444 的伺服器憑證與私鑰；空字串時沿用 `server.cert_path`／`key_path`
    pub cert_path: String,
    pub key_path: String,
    /// L1：同時連線上限（握手中＝一半）
    pub max_connections: usize,
    /// L1：全域新連線速率
    pub max_new_connections_per_sec: u32,
    pub max_new_connections_burst: u32,
    /// L2：每個位址的同時連線
    pub max_connections_per_ip: usize,
    /// L3：每個來源的新連線速率（軟上限；硬上限＝4 倍）
    pub max_new_connections_per_ip_per_sec: u32,
    pub max_new_connections_per_ip_burst: u32,
    /// L4：每個來源的新註冊類請求（`server-info`、`token-check`、新註冊）
    pub max_requests_per_ip_per_minute: u32,
    pub max_requests_per_ip_burst: u32,
    /// L4r：每個來源的過期續簽
    pub max_renewals_per_ip_per_minute: u32,
    pub max_renewals_per_ip_burst: u32,
    /// L5：每個 token 最快多少秒用完（速率＝次數上限 ÷ 此值，至少每分鐘 1 次）；0＝不限
    pub token_min_drain_secs: u32,
    pub max_requests_per_token_burst: u32,
    /// L5b：需核准的 token 在限定網段外的申請，每小時上限
    pub max_outside_subnet_requests_per_token_per_hour: u32,
    /// L6：每個 `agent_id` 的新註冊
    pub max_requests_per_agent_per_minute: u32,
    pub max_requests_per_agent_burst: u32,
    /// L6r：每個 `agent_id` 的過期續簽（每小時）
    pub max_renewals_per_agent_per_hour: u32,
    /// L7：時間窗內相異無效值的上限（新註冊類、續簽類分開）
    pub max_invalid_tokens_per_ip: u32,
    pub max_invalid_renewals_per_ip: u32,
    pub invalid_window_secs: u32,
    /// L7：封鎖時間（再犯加倍，上限 `block_max_secs`）
    pub block_initial_secs: u32,
    pub block_max_secs: u32,
    /// L8：全域申請速率（新註冊與過期續簽合計）
    pub max_requests_per_sec: u32,
    pub max_requests_burst: u32,
    /// L9：待核准佇列上限
    pub max_pending: u32,
    /// L10：同一筆申請兩次輪詢的最短間隔（可連續 3 次）
    pub poll_min_interval_secs: u32,
    /// L10：動態 `poll_after_ms` ＝ max(基準, 同一來源待核准筆數 ÷ 目標速率)，上限
    pub poll_after_base_secs: u32,
    pub poll_after_max_secs: u32,
    pub poll_target_per_ip_per_sec: u32,
    /// L10：長輪詢的上限秒數（0＝停用）與名額
    pub long_poll_secs: u32,
    pub max_long_polls_per_ip: u32,
    pub max_long_polls: u32,
    /// L11：下載的同時數（每個來源；批次部署來源乘以倍率）與全部
    pub max_downloads_per_ip: u32,
    pub max_downloads: u32,
    /// 下載端點 `GET /agent/<檔名>` 提供的目錄（Pulse 的發佈檔案：`pulse-install.sh`、套件、`SHA256SUMS`、
    /// `SHA256SUMS.sig`）；空字串＝不提供（`/agent/` 回 404）。相對路徑以 Kairos 的工作目錄為準
    pub download_dir: String,
    /// 批次部署來源（CIDR）：L2、L3、L4、L11 乘以 `batch_source_multiplier`
    #[serde(deserialize_with = "string_or_list")]
    pub batch_sources: Vec<String>,
    pub batch_source_multiplier: u32,
    /// 429／503 的 `Retry-After` 上限（L7 封鎖與 L9 除外）
    pub retry_after_max_secs: u32,
    /// 讀完請求標頭的上限
    pub header_timeout_secs: u32,
    /// keep-alive 閒置的上限
    pub idle_timeout_secs: u32,
    /// 請求本文上限（bytes）
    pub max_body_bytes: usize,
    /// 待核准申請的有效期限（天）
    pub request_expiry_days: u32,
}

impl Default for EnrollSettings {
    fn default() -> Self {
        Self {
            mode: None,
            auto_pause_enabled: true,
            auto_pause_after_hours: 24,
            listen: String::new(),
            advertise: String::new(),
            cert_path: String::new(),
            key_path: String::new(),
            max_connections: 1024,
            max_new_connections_per_sec: 200,
            max_new_connections_burst: 1000,
            max_connections_per_ip: 64,
            max_new_connections_per_ip_per_sec: 20,
            max_new_connections_per_ip_burst: 200,
            max_requests_per_ip_per_minute: 600,
            max_requests_per_ip_burst: 600,
            max_renewals_per_ip_per_minute: 600,
            max_renewals_per_ip_burst: 600,
            token_min_drain_secs: 300,
            max_requests_per_token_burst: 100,
            max_outside_subnet_requests_per_token_per_hour: 10,
            max_requests_per_agent_per_minute: 1,
            max_requests_per_agent_burst: 5,
            max_renewals_per_agent_per_hour: 10,
            max_invalid_tokens_per_ip: 20,
            max_invalid_renewals_per_ip: 20,
            invalid_window_secs: 3600,
            block_initial_secs: 900,
            block_max_secs: 86_400,
            max_requests_per_sec: 50,
            max_requests_burst: 500,
            max_pending: 20_000,
            poll_min_interval_secs: 10,
            poll_after_base_secs: 30,
            poll_after_max_secs: 900,
            poll_target_per_ip_per_sec: 5,
            long_poll_secs: 60,
            max_long_polls_per_ip: 32,
            max_long_polls: 1000,
            max_downloads_per_ip: 8,
            max_downloads: 64,
            download_dir: String::new(),
            batch_sources: Vec::new(),
            batch_source_multiplier: 10,
            retry_after_max_secs: 600,
            header_timeout_secs: 10,
            idle_timeout_secs: 15,
            max_body_bytes: MAX_BODY_BYTES,
            request_expiry_days: 7,
        }
    }
}

/// `[pki.policy]`：憑證政策（設計包 S14）。主控台修改的值存在 `settings` 表、優先於這裡
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct CertPolicySettings {
    /// 用戶端憑證的有效天數（D2；90～825）
    pub cert_lifetime_days: u32,
    /// 到期前幾天開始輪替（1～180，且小於有效天數）
    pub renew_before_days: u32,
    /// 輪替與過期續簽是否需要人工核准
    pub renew_requires_approval: bool,
    /// 過期寬限期（天；0～365，0＝不接受過期續簽）
    pub expired_renew_grace_days: u32,
    /// 主機自行除役時撤銷憑證（`cessationOfOperation`）
    pub revoke_on_self_decommission: bool,
    /// 輪替成功（新憑證第一次 hello）後以 `superseded` 撤銷舊憑證
    pub revoke_superseded: bool,
}

impl Default for CertPolicySettings {
    fn default() -> Self {
        Self {
            cert_lifetime_days: 365,
            renew_before_days: 30,
            renew_requires_approval: false,
            expired_renew_grace_days: 90,
            revoke_on_self_decommission: true,
            revoke_superseded: true,
        }
    }
}

/// `[pki]`
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct PkiSettings {
    /// `online`／`offline`；未設定時為 `offline`（雙層預設）
    pub mode: Option<PkiMode>,
    /// Pulse 信任的 CA（安裝字串的 CA 指紋、8444 的鏈尾、`issued` 回應的 `ca_pem`）。未設定時 8444 不監聽
    pub server_ca_path: String,
    /// Kairos 產生的 CRL 的有效天數（`nextUpdate`）
    pub crl_validity_days: u32,
    /// 沒有新撤銷時重新產生 CRL 的間隔（小時）
    pub crl_refresh_hours: u32,
    /// 簽發 CA 的組織名稱（subject `CN=<組織> Kairos Agent Issuing CA <ca_id>`）
    pub organization: String,
    /// 簽發 CA 剩餘天數少於此值時記 `KRS-W1304`（需要輪替）。未設定時為「憑證政策的有效天數（含有效
    /// token 覆寫中最長者）＋過期寬限期」（預設 455 天；設計包 INTEGRATION §27.4 #5）
    pub issuing_ca_warning_days: Option<u32>,
    pub policy: CertPolicySettings,
}

impl Default for PkiSettings {
    fn default() -> Self {
        Self {
            mode: None,
            server_ca_path: String::new(),
            crl_validity_days: 30,
            crl_refresh_hours: 24,
            organization: "Kairos".to_string(),
            issuing_ca_warning_days: None,
            policy: CertPolicySettings::default(),
        }
    }
}

impl PkiSettings {
    /// 實際的簽發模式（未設定為離線）
    pub fn effective_mode(&self) -> PkiMode {
        self.mode.unwrap_or(PkiMode::Offline)
    }

    /// 設定的 Pulse 信任 CA 路徑
    pub fn server_ca(&self) -> Option<&str> {
        let p = self.server_ca_path.trim();
        (!p.is_empty()).then_some(p)
    }
}

impl EnrollSettings {
    /// 實際的模式（未設定為關閉）
    pub fn effective_mode(&self) -> EnrollMode {
        self.mode.unwrap_or(EnrollMode::Closed)
    }

    /// 8444 實際使用的憑證與私鑰：未設定時沿用 `server.cert_path`／`key_path`
    pub fn tls_files(&self, server: &super::ServerSettings) -> (String, String) {
        if self.cert_path.trim().is_empty() {
            (server.cert_path.clone(), server.key_path.clone())
        } else {
            (
                self.cert_path.trim().to_string(),
                self.key_path.trim().to_string(),
            )
        }
    }

    /// 實際的監聽位址（字串）：未設定時為 `server.listen` 的主機加上 8444
    pub fn listen_addr(&self, server_listen: &str) -> String {
        let l = self.listen.trim();
        if !l.is_empty() {
            return l.to_string();
        }
        let host = match server_listen.trim().rsplit_once(':') {
            Some((host, _)) => host,
            None => server_listen.trim(),
        };
        format!("{host}:{DEFAULT_ENROLL_PORT}")
    }

    /// 解析後的批次部署來源
    pub fn batch_allowlist(&self) -> Result<crate::per_ip::Allowlist, Error> {
        crate::per_ip::Allowlist::parse(&self.batch_sources).map_err(|e| {
            Error::new(
                ErrorKind::InvalidInput,
                e.replace("server.per_ip_allowlist", "enroll.batch_sources"),
            )
        })
    }

    /// 檢查範圍（設計包 `enroll-rate-limit.md` §7）
    pub fn validate(&self) -> Result<(), Error> {
        let invalid = |msg: String| Err(Error::new(ErrorKind::InvalidInput, msg));
        let ranges: &[(&str, u64, u64, u64)] = &[
            (
                "enroll.auto_pause_after_hours",
                self.auto_pause_after_hours.into(),
                1,
                168,
            ),
            (
                "enroll.max_connections",
                self.max_connections as u64,
                16,
                65_535,
            ),
            (
                "enroll.max_new_connections_per_sec",
                self.max_new_connections_per_sec.into(),
                10,
                10_000,
            ),
            (
                "enroll.max_new_connections_burst",
                self.max_new_connections_burst.into(),
                10,
                10_000,
            ),
            (
                "enroll.max_connections_per_ip",
                self.max_connections_per_ip as u64,
                1,
                self.max_connections as u64,
            ),
            (
                "enroll.max_new_connections_per_ip_per_sec",
                self.max_new_connections_per_ip_per_sec.into(),
                1,
                1000,
            ),
            (
                "enroll.max_new_connections_per_ip_burst",
                self.max_new_connections_per_ip_burst.into(),
                1,
                10_000,
            ),
            (
                "enroll.max_requests_per_ip_per_minute",
                self.max_requests_per_ip_per_minute.into(),
                1,
                60_000,
            ),
            (
                "enroll.max_requests_per_ip_burst",
                self.max_requests_per_ip_burst.into(),
                1,
                100_000,
            ),
            (
                "enroll.max_renewals_per_ip_per_minute",
                self.max_renewals_per_ip_per_minute.into(),
                1,
                60_000,
            ),
            (
                "enroll.max_renewals_per_ip_burst",
                self.max_renewals_per_ip_burst.into(),
                1,
                100_000,
            ),
            (
                "enroll.token_min_drain_secs",
                self.token_min_drain_secs.into(),
                0,
                86_400,
            ),
            (
                "enroll.max_requests_per_token_burst",
                self.max_requests_per_token_burst.into(),
                1,
                10_000,
            ),
            (
                "enroll.max_outside_subnet_requests_per_token_per_hour",
                self.max_outside_subnet_requests_per_token_per_hour.into(),
                0,
                1000,
            ),
            (
                "enroll.max_requests_per_agent_per_minute",
                self.max_requests_per_agent_per_minute.into(),
                1,
                60,
            ),
            (
                "enroll.max_requests_per_agent_burst",
                self.max_requests_per_agent_burst.into(),
                1,
                60,
            ),
            (
                "enroll.max_renewals_per_agent_per_hour",
                self.max_renewals_per_agent_per_hour.into(),
                1,
                3600,
            ),
            (
                "enroll.max_invalid_tokens_per_ip",
                self.max_invalid_tokens_per_ip.into(),
                5,
                1000,
            ),
            (
                "enroll.max_invalid_renewals_per_ip",
                self.max_invalid_renewals_per_ip.into(),
                5,
                1000,
            ),
            (
                "enroll.invalid_window_secs",
                self.invalid_window_secs.into(),
                60,
                86_400,
            ),
            (
                "enroll.block_initial_secs",
                self.block_initial_secs.into(),
                60,
                86_400,
            ),
            (
                "enroll.block_max_secs",
                self.block_max_secs.into(),
                self.block_initial_secs.into(),
                86_400,
            ),
            (
                "enroll.max_requests_per_sec",
                self.max_requests_per_sec.into(),
                1,
                10_000,
            ),
            (
                "enroll.max_requests_burst",
                self.max_requests_burst.into(),
                1,
                100_000,
            ),
            ("enroll.max_pending", self.max_pending.into(), 100, 200_000),
            (
                "enroll.poll_min_interval_secs",
                self.poll_min_interval_secs.into(),
                1,
                300,
            ),
            (
                "enroll.poll_after_base_secs",
                self.poll_after_base_secs.into(),
                5,
                3600,
            ),
            (
                "enroll.poll_after_max_secs",
                self.poll_after_max_secs.into(),
                self.poll_after_base_secs.into(),
                3600,
            ),
            (
                "enroll.poll_target_per_ip_per_sec",
                self.poll_target_per_ip_per_sec.into(),
                1,
                1000,
            ),
            ("enroll.long_poll_secs", self.long_poll_secs.into(), 0, 60),
            (
                "enroll.max_long_polls_per_ip",
                self.max_long_polls_per_ip.into(),
                0,
                1024,
            ),
            (
                "enroll.max_long_polls",
                self.max_long_polls.into(),
                0,
                65_535,
            ),
            (
                "enroll.batch_source_multiplier",
                self.batch_source_multiplier.into(),
                1,
                100,
            ),
            (
                "enroll.retry_after_max_secs",
                self.retry_after_max_secs.into(),
                60,
                3600,
            ),
            (
                "enroll.header_timeout_secs",
                self.header_timeout_secs.into(),
                1,
                60,
            ),
            (
                "enroll.idle_timeout_secs",
                self.idle_timeout_secs.into(),
                1,
                300,
            ),
            (
                "enroll.request_expiry_days",
                self.request_expiry_days.into(),
                1,
                90,
            ),
        ];
        for &(name, value, min, max) in ranges {
            if !(min..=max).contains(&value) {
                return invalid(format!("{name} 必須介於 {min}..={max}（目前為 {value}）"));
            }
        }
        if !(4096..=1_048_576).contains(&self.max_body_bytes) {
            return invalid(format!(
                "enroll.max_body_bytes 必須介於 4096..=1048576（目前為 {}）",
                self.max_body_bytes
            ));
        }
        for (name, value) in [
            ("enroll.max_downloads_per_ip", self.max_downloads_per_ip),
            ("enroll.max_downloads", self.max_downloads),
        ] {
            if !(1..=1024).contains(&value) {
                return invalid(format!("{name} 必須介於 1..=1024（目前為 {value}）"));
            }
        }
        if self.cert_path.trim().is_empty() != self.key_path.trim().is_empty() {
            return invalid(
                "enroll.cert_path 與 enroll.key_path 必須同時設定或同時留空（留空時沿用 server.cert_path／key_path）"
                    .to_string(),
            );
        }
        if !self.advertise.trim().is_empty() {
            kairos_proto::enroll::normalize_endpoint(self.advertise.trim()).map_err(|e| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("enroll.advertise \"{}\" 無效：{e}", self.advertise),
                )
            })?;
        }
        if !self.listen.trim().is_empty() && self.listen.trim().parse::<SocketAddr>().is_err() {
            // 主機名稱形式在啟動時解析；這裡只確認有連接埠
            if self.listen.trim().rsplit_once(':').is_none() {
                return invalid(format!(
                    "enroll.listen \"{}\" 必須是 IP:port 或 主機名稱:port",
                    self.listen
                ));
            }
        }
        self.batch_allowlist()?;
        Ok(())
    }
}

impl CertPolicySettings {
    pub fn validate(&self) -> Result<(), Error> {
        let invalid = |msg: String| Err(Error::new(ErrorKind::InvalidInput, msg));
        if !(MIN_CERT_LIFETIME_DAYS..=MAX_CERT_LIFETIME_DAYS).contains(&self.cert_lifetime_days) {
            return invalid(format!(
                "pki.policy.cert_lifetime_days 必須介於 {MIN_CERT_LIFETIME_DAYS}..={MAX_CERT_LIFETIME_DAYS}（目前為 {}）",
                self.cert_lifetime_days
            ));
        }
        if !(1..=180).contains(&self.renew_before_days)
            || self.renew_before_days >= self.cert_lifetime_days
        {
            return invalid(format!(
                "pki.policy.renew_before_days 必須介於 1..=180 且小於 cert_lifetime_days（目前為 {}）",
                self.renew_before_days
            ));
        }
        if self.expired_renew_grace_days > MAX_GRACE_DAYS {
            return invalid(format!(
                "pki.policy.expired_renew_grace_days 必須介於 0..={MAX_GRACE_DAYS}（目前為 {}）",
                self.expired_renew_grace_days
            ));
        }
        Ok(())
    }
}

/// 用戶端憑證有效天數的範圍（設計包 D2、S12）
pub const MIN_CERT_LIFETIME_DAYS: u32 = 90;
pub const MAX_CERT_LIFETIME_DAYS: u32 = 825;
/// 過期寬限期的上限（天）
pub const MAX_GRACE_DAYS: u32 = 365;

impl PkiSettings {
    pub fn validate(&self) -> Result<(), Error> {
        let invalid = |msg: String| Err(Error::new(ErrorKind::InvalidInput, msg));
        if !(1..=365).contains(&self.crl_validity_days) {
            return invalid(format!(
                "pki.crl_validity_days 必須介於 1..=365（目前為 {}）",
                self.crl_validity_days
            ));
        }
        if !(1..=24 * 30).contains(&self.crl_refresh_hours)
            || u64::from(self.crl_refresh_hours) >= u64::from(self.crl_validity_days) * 24
        {
            return invalid(format!(
                "pki.crl_refresh_hours 必須介於 1..=720 且小於 crl_validity_days × 24（目前為 {}）",
                self.crl_refresh_hours
            ));
        }
        let org = self.organization.trim();
        if org.is_empty() || org.chars().count() > 64 || org.chars().any(char::is_control) {
            return invalid("pki.organization 必須是 1～64 個字元、不含控制字元".to_string());
        }
        self.policy.validate()
    }
}
